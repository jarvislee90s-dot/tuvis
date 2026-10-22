//! DesktopIpcAdapter：Desktop IPC 控制面门面（connection/owners/mirror/pending 的
//! 单一编排点）。与 `inject/headless` 平行，不共用其 transport/envelope/并发额度。
//!
//! 纪律（计划 Task 6）：
//! * 所有写入在派发前**最后复核**（lease 现行 + pending 现存 + token 有效 + 能力
//!   已验证），响应前读到的过期 lease 不算授权（writer 侧还有 validity 复核兜底）；
//! * start-turn 只带 `conversationId/turnStart{request{threadId,input,text_elements,
//!   clientUserMessageId},context{}}`——不发 model/权限/collaboration 覆盖；
//! * 两类审批各走各的 wire：command → `decision:"accept"|"decline"`；permissions →
//!   回显服务端保存的原 permissions（批准）或 `{}`（拒绝）+ `scope:"turn"`，
//!   绝不混用、绝不接受远端上送的自定义 permissions；
//! * 阻塞问答按原 qid 映射，不能漏答/混入未知 qid；`isOther=false` 禁自由文本；
//!   `isSecret` 的回答不进日志/错误文本（不回显保证）；
//! * 锁纪律：所有互斥锁只在同步临界区持有，不跨 await；
//! * 忙 Thread 留队由 router（Task 7）调度；审批/问答不等普通消息队列。

use super::connection::{ConnectionEvent, ConnectionHandle, RpcOutcome, RpcRequest};
use super::mirror::ThreadMirror;
use super::owners::{
    bind_lease, declare_following, discover_owner, wait_first_snapshot, FollowingSet,
};
use super::pending::{parse_request_key, DECISION_ACCEPT, DECISION_CANCEL, DECISION_DECLINE};
use super::probe::runner::{ProbeOutcome, ProbeRunner, ProbeStage};
use super::probe::schema_probe::{SchemaEvidence, SchemaProbeOutcome};
use super::probe::{ProbeLedger, CAP_SCHEMA, CAP_STATE};
use super::profile::ProtocolProfile;
use super::types::{
    CapabilitySet, ContentState, ControlAvailability, Delivery, MessagesPage, MessagesPageStatus,
    OwnerLease, PendingAction, PendingKind, PendingView, ProbeStatus, ProtocolError, ThreadKey,
    ThreadProjection,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, Mutex};

/// 连接工厂（生产：endpoint 解析 + transport；测试：注入 ScriptedPeer 的 io）。
///
/// R3（2026-10-17 审计）：连接必须绑定 endpoint 身份（build 门）——[`Connector::connect_endpoint`]
/// 返回 `(io, endpoint)`，adapter 每次连接按 endpoint 重算 profile；旧式只产 io 的
/// 实现走 [`Connector::connect`]（无 endpoint → closed profile，A 写全关）。
/// 两个方法互为缺省委托：实现者必须至少覆盖其一，否则运行时无限递归。
pub trait Connector: Send + Sync {
    /// 连接并携带 endpoint 身份（platform/build 来源；R3 每次连接重算能力的依据）。
    #[allow(clippy::type_complexity)]
    fn connect_endpoint(
        &self,
    ) -> Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        (super::transport::BoxIo, super::transport::DesktopEndpoint),
                        ProtocolError,
                    >,
                > + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let io = self.connect().await?;
            // 旧式实现不暴露 endpoint：按无 endpoint 处理（closed profile，只读仍可）
            let endpoint = super::transport::DesktopEndpoint {
                platform: std::env::consts::OS.to_string(),
                path: String::new(),
                build: None,
            };
            Ok((io, endpoint))
        })
    }

    /// 兼容旧形态：仅连接（缺省委托 `connect_endpoint`，丢弃 endpoint）。
    fn connect(
        &self,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = Result<super::transport::BoxIo, ProtocolError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move { Ok(self.connect_endpoint().await?.0) })
    }
}

/// 生产连接器：resolve endpoint → transport::connect（endpoint 身份不丢弃，R3）。
pub struct ProductionConnector {
    pub resolver: Arc<dyn super::transport::EndpointResolver>,
}

impl Connector for ProductionConnector {
    fn connect_endpoint(
        &self,
    ) -> Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        (super::transport::BoxIo, super::transport::DesktopEndpoint),
                        ProtocolError,
                    >,
                > + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let endpoint: super::transport::DesktopEndpoint = self.resolver.resolve()?;
            let io = super::transport::connect(&endpoint).await?;
            Ok((io, endpoint))
        })
    }
}

/// 交互提交互斥条目（R4 + 2026-10-17 复审 C3）：`Dispatching` = 已派发待回执；
/// `BlockedUnknown` = 未知投递——同 (thread, request_key) 不得借新 actionId 再发。
/// **屏障按完整上下文分代**：`token` 记录 Unknown 提交所用的 contextToken——该
/// pending 消失或重现换 token（服务端新世界）即由状态应用驱动清理，旧 Unknown
/// 不再挡新卡；旧 token 本身仍被 pending 投影器拒绝。
#[derive(Debug, Clone)]
struct InflightEntry {
    state: InflightState,
    /// BlockedUnknown 时的提交上下文 token（Dispatching 为 None）
    token: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InflightState {
    Dispatching,
    BlockedUnknown,
}

/// #172：当前「尚无证据」档 build 的探测目标（键含 `cli_id`，算一次即缓存）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProbeTarget {
    /// 目标所属连接代际；同 build 重连也必须隔离异步指纹回填。
    epoch: u64,
    platform: String,
    build: String,
    /// 账本主键（`platform|build|cli_id|epoch`）；`None` = 尚未计算（算 `cli_id` 可能
    /// 要跑一次 `codex.exe --version`，只在真正探测时按需算，不拖慢连接路径）。
    key: Option<String>,
}

/// #172：debug 专用强制开关（`TUVIS_CODEX_IPC_FORCE_UNVERIFIED=1`）——把**已验证**
/// build 也压到「尚无证据」档，好在真机上跑探测分支（本机 4894 已 shipped，否则永远
/// 走不到未知 build 的路径）。**release 构建恒 false**：`cfg!(debug_assertions)` 是
/// 编译期常量，发行包里这条分支被消掉，用户无法借此关掉能力门。
fn force_unverified() -> bool {
    cfg!(debug_assertions)
        && std::env::var("TUVIS_CODEX_IPC_FORCE_UNVERIFIED")
            .map(|v| v.trim() == "1")
            .unwrap_or(false)
}

struct AdapterInner {
    connector: Arc<dyn Connector>,
    /// 当前连接的 profile（R3：动态）。初始为构造入参（首个连接前的基线）；
    /// 每次连接按 endpoint build 重算（未知 build/无 endpoint → `closed()`）。
    /// 同步短临界区读写，绝不跨 await 持有、不与其他锁嵌套。
    profile: std::sync::Mutex<ProtocolProfile>,
    /// #172：**最近一次成功 resolve 的 Desktop build 是否带写能力证据**。
    /// `None` = 本进程内还没成功解析过 build；`Some(false)` = 解析到了但不在 shipped
    /// 证据表（未知 build）；`Some(true)` = 已验证 build。
    ///
    /// 为什么要与 [`profile`] 分开记：连接失败会把 profile 回落 `closed()`（R3 的
    /// 「Desktop 缺席」诚实缺省），此时能力位不再回答「这台 build 支持什么」。兜底
    /// 交接必须区分「build 没取证」（不得静默改走 headless）与「验证过的 build 断了」
    /// （既有兜底契约不变），所以证据要单独留存、不被链路状态冲掉。
    build_send_verified: std::sync::Mutex<Option<bool>>,
    /// #172：只读探测单飞器（同一 `probe_key` 同刻只有一条探测在飞）。
    probe: Arc<ProbeRunner>,
    /// #172：探测账本（remote 层注入 `DeviceStore` 实现；`None` = 不查不写账本，
    /// 仍可探测——测试与「账本不可用」降级路径）。
    probe_ledger: std::sync::Mutex<Option<Arc<dyn ProbeLedger>>>,
    /// #172：当前「**尚无证据**」档 build 的探测目标（shipped build → `None`；
    /// 连接失败回落 closed 时保留原目标，便于重连后复用同一键）。
    probe_target: std::sync::Mutex<Option<ProbeTarget>>,
    /// #172/T8 测试缝：固定账本键（生产恒 `None` → 真实计算；测试避免 spawn
    /// 本机 `codex.exe --version` 取 `cli_id`）。
    probe_key_override: std::sync::Mutex<Option<String>>,
    /// #172：探测预算（生产 = P1/P2/P3 2/3/3s；测试可收紧，免得真等 8s）。
    probe_budget: std::sync::Mutex<super::probe::runner::ProbeBudget>,
    conn: Mutex<Option<ConnectionHandle>>,
    /// follow 互斥（C2）：settle 对账与派发路径可能并发 ensure 同一 thread——
    /// 冷启动竞跑会产生重复 discovery/绑定（lease 翻代）。持锁串行化 follow 段；
    /// 后到者在锁内重查新鲜镜像即短路。
    ensure_lock: tokio::sync::Mutex<()>,
    owners: Mutex<OwnerRegistry>,
    mirrors: Mutex<HashMap<ThreadKey, ThreadMirror>>,
    /// C4：全体镜像字节账本（生产 64MiB；所有镜像共用一份）
    mirror_ledger: Arc<std::sync::Mutex<super::mirror::MirrorLedger>>,
    followings: Mutex<FollowingSet>,
    /// (thread_id, request_key) → 提交互斥（R4；独立短锁，不与 mirrors/owners 嵌套）。
    inflight: Mutex<HashMap<(String, String), InflightEntry>>,
    /// 只读 hydrate（load-complete-history）状态机（2026-10-18 形态适配，替代旧
    /// 「曾请求」HashSet）：per-thread 记 owner 世代 / 已试次数 / 在飞——在飞去重、
    /// 失败有界重试（不是「曾请求即永久锁死」）、换代即重置（旧世代回执不污染）。
    history_hydrations: Mutex<HashMap<ThreadKey, Hydration>>,
    /// 事件转发任务单飞标志（每 adapter 生命周期至多一个 forward_events）。
    forward_alive: AtomicBool,
    /// 重连恢复循环单飞标志（R2.5）。
    recovering: AtomicBool,
    /// 同 socket 恢复的工作队列与消费者资格（第四轮复审 B2 结构化）：pending 与
    /// running 同锁——「取空待办」与「释放消费者资格」在**同一临界区**完成，
    /// 退出判定与并发入队之间不存在窗口。
    recovery_queue: std::sync::Mutex<RecoveryQueue>,
    /// shutdown 信号（恢复循环/重拉循环的退出条件）。
    stopping: AtomicBool,
    /// C3 测试缝：respond 两阶段间的可注入等待（生产恒 None → 零开销直通）；
    /// 回归测试用它固定「阶段 1 读镜像之后、阶段 3 取 lease 之前」的 owner 换代。
    interphase_gate: std::sync::Mutex<Option<Arc<tokio::sync::Notify>>>,
    /// 第四轮复审 B3 测试缝：follow 的 bind 与 declare 之间的可注入等待
    /// （生产恒 None → 零开销直通）；回归固定「bind/declare 间卸载」交错。
    follow_gate: std::sync::Mutex<Option<Arc<tokio::sync::Notify>>>,
    /// B3 测试缝探针：任务已到达 follow gate。
    follow_gate_reached: AtomicBool,
    /// #172/T7：官方 schema 证据（未知 build 的审批决策词来源；`None` = 尚未取证）。
    /// 有它就能在「无真机审批证据」的 build 上安全开放 `accept` + `cancel`。
    schema_evidence: std::sync::Mutex<Option<(u64, Option<ProbeTarget>, SchemaEvidence)>>,
    /// #172/T7-R4：schema 探针对完整指纹与连接代际**各跑一次**的单飞标志（结论为 `Unknown` 时也不重跑：
    /// 探针失败即「未知」，重跑只会再拿同一个 `Unknown`；账本行已记 `inconclusive`
    /// + 退避窗口，跨重启的复用由账本负责）。
    schema_probe_ran: tokio::sync::Mutex<Option<(u64, String)>>,
    /// #172/T7-R4 测试缝：注入 schema 探针结论（生产恒 `None` → 真跑 CLI；
    /// 测试注入后**绝不 spawn 真实 codex.exe**）。
    schema_probe_override: std::sync::Mutex<Option<SchemaProbeOutcome>>,
    /// #172/T9a：探测只读事实的**缓存**（账本 `state` 行的结论）。
    ///
    /// 为什么要缓存：投影在每次状态变化时都会重建（多处发布点），每次都查账本
    /// 会把 SQLite 读放大成热路径负担。账本 `state` 行的**唯一写者**是本 adapter
    /// 的 [`DesktopIpcAdapter::ensure_read_evidence`]（remote 层只写 `send` 行），
    /// 故在写点与 build 目标变更点失效即可保持准确。
    probe_status_cache: std::sync::Mutex<Option<ProbeStatus>>,
    epoch: AtomicU64,
    events: broadcast::Sender<ThreadProjection>,
}

use super::owners::OwnerRegistry;

/// Desktop IPC 适配器（克隆共享）。
#[derive(Clone)]
pub struct DesktopIpcAdapter {
    inner: Arc<AdapterInner>,
}

impl DesktopIpcAdapter {
    pub fn new(connector: Arc<dyn Connector>, profile: ProtocolProfile) -> Self {
        Self {
            inner: Arc::new(AdapterInner {
                connector,
                profile: std::sync::Mutex::new(profile.clone()),
                // 构造期基线 profile 来自装配时 resolve 到的 build（生产）/ 注入基线
                //（测试）：**有写能力证据才记 true**，无证据保持 None（按「未取证」处理）
                build_send_verified: std::sync::Mutex::new(
                    profile.capabilities.send.then_some(true),
                ),
                probe: Arc::new(ProbeRunner::new()),
                probe_ledger: std::sync::Mutex::new(None),
                // 构造期基线 profile 若已是「尚无证据」档（生产装配时 resolve 到未知
                // build / 测试注入），先把探测目标记下来：连接暂不可达也能查账本
                probe_target: std::sync::Mutex::new(profile.is_probe_eligible().then(|| {
                    ProbeTarget {
                        epoch: 0,
                        platform: profile.platform.clone(),
                        build: profile.build.clone(),
                        key: None,
                    }
                })),
                probe_key_override: std::sync::Mutex::new(None),
                probe_budget: std::sync::Mutex::new(super::probe::runner::ProbeBudget::default()),
                conn: Mutex::new(None),
                ensure_lock: tokio::sync::Mutex::new(()),
                owners: Mutex::new(OwnerRegistry::default()),
                mirrors: Mutex::new(HashMap::new()),
                mirror_ledger: Arc::new(std::sync::Mutex::new(
                    super::mirror::MirrorLedger::global(),
                )),
                followings: Mutex::new(FollowingSet::default()),
                inflight: Mutex::new(HashMap::new()),
                history_hydrations: Mutex::new(HashMap::new()),
                forward_alive: AtomicBool::new(false),
                recovering: AtomicBool::new(false),
                recovery_queue: std::sync::Mutex::new(RecoveryQueue::default()),
                stopping: AtomicBool::new(false),
                interphase_gate: std::sync::Mutex::new(None),
                follow_gate: std::sync::Mutex::new(None),
                follow_gate_reached: AtomicBool::new(false),
                schema_evidence: std::sync::Mutex::new(None),
                schema_probe_ran: tokio::sync::Mutex::new(None),
                schema_probe_override: std::sync::Mutex::new(None),
                probe_status_cache: std::sync::Mutex::new(None),
                epoch: AtomicU64::new(0),
                events: broadcast::channel(256).0,
            }),
        }
    }

    /// #172/T7：注入官方 schema 证据（生产由装配期一次性探测填入；测试直接注入）。
    pub fn set_schema_evidence(&self, evidence: Option<SchemaEvidence>) {
        *self
            .inner
            .schema_evidence
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = evidence.map(|ev| {
            (
                self.inner.epoch.load(Ordering::SeqCst),
                self.probe_target_snapshot(),
                ev,
            )
        });
    }

    /// #172/T7-R4 测试缝：注入 schema 探针的**结论**（`Known` / `Unknown`）。
    ///
    /// 注入后 [`ensure_schema_evidence`](Self::ensure_schema_evidence) 直接采用注入结果，
    /// **不跑真实 CLI**——测试绝不 spawn 本机的 `codex.exe`（那会真的执行
    /// `app-server generate-json-schema`，既慢又不受控）。
    ///
    /// 传 `None` = 回到「真跑 CLI」的生产行为。注入为空**且**已有证据时仍按幂等
    /// 直接返回（证据检查在注入检查之前）。
    #[doc(hidden)]
    pub fn set_schema_probe_override(&self, outcome: Option<SchemaProbeOutcome>) {
        *self
            .inner
            .schema_probe_override
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = outcome;
    }

    /// #172/T7-R4：**官方 schema 探针的结论落进探测账本**（`capability = "schema"`）。
    ///
    /// 为什么要有这一层：`state` 行只说「这台 build 的**读链**有没有证据」，说不清
    /// 「这台 build 的**审批决策词**从哪来」。把 schema 探针的结论也记一行，
    /// 「build × 能力」的数据集才完整（后续人工提升 shipped 表时可回看依据）。
    ///
    /// 流程（幂等 + 只跑一次）：
    /// 1. **同一完整指纹与连接代际已完成探测** → 直接返回；
    /// 2. 取探测目标（`probe_target_snapshot()` + `resolve_probe_key()`）：取不到
    ///    target → **直接返回，不记账**（无法归因的行宁可没有，也不能错记到别的
    ///    build 上）；
    /// 3. 跑探针（`probe_schema()` 内部已 3s 有界 + 临时目录用完即删；测试走注入
    ///    结论，绝不 spawn 真实 CLI）；
    /// 4. `Known(evidence)` → 装证据 + 记 `Probed`；`Unknown{error_code}` → **不装
    ///    证据**（审批保持「到桌面处理」）+ 记 `Inconclusive`（带错误码）。
    ///
    /// schema 内容指纹随结论写入账本，供审批词表归因。
    pub async fn ensure_schema_evidence(&self) {
        // 归因：拿不到 build 目标就没有 probe_key——**不记账**（宁可缺行，不可错记）。
        let Some(target) = self.probe_target_snapshot() else {
            log::info!("codex-ipc schema: 未取到 build 目标（无法归因），不探不记账");
            return;
        };
        let epoch = target.epoch;
        let probe_key = self.resolve_probe_key(&target).await;
        let bound_target = self.probe_target_snapshot();
        if self.inner.epoch.load(Ordering::SeqCst) != epoch
            || !bound_target.as_ref().is_some_and(|current| {
                current.epoch == target.epoch
                    && current.platform == target.platform
                    && current.build == target.build
            })
        {
            return;
        }
        // 锁覆盖探测；取消时自动释放，且仅完成后标记已探测。
        let mut ran = self.inner.schema_probe_ran.lock().await;
        if ran.as_ref() == Some(&(epoch, probe_key.clone())) {
            return;
        }
        let started = std::time::Instant::now();
        // 测试缝：注入结论时**不跑真实 CLI**（生产恒 None → 真跑）。
        let outcome = match self.schema_probe_override() {
            Some(injected) => injected,
            None => super::probe::schema_probe::probe_schema().await,
        };
        let duration_ms = started.elapsed().as_millis() as u64;
        if self.inner.epoch.load(Ordering::SeqCst) != epoch
            || self.probe_target_snapshot() != bound_target
        {
            return; // 身份已变化，拒收旧异步结果（也不记账）。
        }
        *ran = Some((epoch, probe_key.clone()));
        match outcome {
            SchemaProbeOutcome::Known(evidence) => {
                // §4.11 观测：结构化日志（打指纹与两个决策词列表；不记正文）。
                log::info!(
                    "codex-ipc schema: key={probe_key} fingerprint={} command={:?} file={:?} durationMs={duration_ms}",
                    evidence.fingerprint,
                    evidence.command_decisions,
                    evidence.file_decisions
                );
                let fingerprint = evidence.fingerprint.clone();
                *self
                    .inner
                    .schema_evidence
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some((epoch, bound_target, evidence));
                if let Some(ledger) = self.probe_ledger() {
                    // stage 记 P3：账本的 `probe_stage` 只有 P0–P3 词表，schema 探针
                    // 属「读链末端的能力取证」，归入 P3（不改 DAO 的词表）。
                    ledger.record(
                        &probe_key,
                        CAP_SCHEMA,
                        &ProbeOutcome::Probed {
                            stage: ProbeStage::P3,
                            revision: None,
                            duration_ms,
                        },
                        // 官方 schema 的内容指纹（文件名+大小的 sha256）：让「build × 能力」
                        // 数据集能追到具体 schema 版本
                        Some(&fingerprint),
                    );
                }
            }
            SchemaProbeOutcome::Unknown { error_code } => {
                // 拿不到就保持关闭（审批卡走「到桌面处理」），不重试、不猜。
                log::info!(
                    "codex-ipc schema: key={probe_key} 未取证（{error_code}），审批保持关闭"
                );
                if let Some(ledger) = self.probe_ledger() {
                    ledger.record(
                        &probe_key,
                        CAP_SCHEMA,
                        &ProbeOutcome::Inconclusive {
                            stage: ProbeStage::P3,
                            error_code,
                            duration_ms,
                        },
                        // 未取证 → 没有指纹可给
                        None,
                    );
                }
            }
        }
    }

    /// 取出测试缝注入的 schema 探针结论（生产恒 `None`）。
    fn schema_probe_override(&self) -> Option<SchemaProbeOutcome> {
        self.inner
            .schema_probe_override
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 当前生效的**审批决策策略**（服务端单一来源）：
    ///
    /// * 有真机证据（shipped profile 的 `command` 能力）→ `accept` + `decline`；
    /// * 未知 build 但 schema 已取证、且两个词都在 → `accept` + `cancel`；
    /// * 其余 → 空（卡片只显示「到桌面处理」，且 `respond` 拒发）。
    pub fn approval_policy(&self) -> super::pending::DecisionPolicy {
        if self.current_caps().command {
            return super::pending::DecisionPolicy::evidence();
        }
        let evidence = self
            .inner
            .schema_evidence
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        match evidence {
            Some((epoch, target, ev))
                if epoch == self.inner.epoch.load(Ordering::SeqCst)
                    && target == self.probe_target_snapshot() =>
            {
                super::pending::DecisionPolicy::schema(
                    ev.command_allows(super::pending::DECISION_ACCEPT)
                        && ev.command_allows(super::pending::DECISION_CANCEL),
                    ev.file_allows(super::pending::DECISION_ACCEPT)
                        && ev.file_allows(super::pending::DECISION_CANCEL),
                )
            }
            _ => super::pending::DecisionPolicy::default(),
        }
    }

    /// 当前连接的 profile（R3 动态值；clone 出短临界区）。
    fn current_profile(&self) -> ProtocolProfile {
        self.inner.profile.lock().expect("profile lock").clone()
    }

    /// 更新当前 profile（同步短临界区；guard 不进入 async 栈帧）。
    fn set_profile(&self, profile: ProtocolProfile) {
        *self.inner.profile.lock().expect("profile lock") = profile;
    }

    fn current_caps(&self) -> CapabilitySet {
        self.current_profile().capabilities
    }

    /// 本 build 的能力基线（能力四档的来源；R3：读当前连接的动态 profile，
    /// 无连接/未知 build → closed）。
    pub fn capabilities(&self) -> CapabilitySet {
        self.current_caps()
    }

    /// #172：本机 Desktop build 是否**带写能力证据**（与当前连接是否活着解耦）。
    /// `false` = 未知 build / 本进程还没解析过 build / 已验证但无写能力——这类桌面
    /// 归属会话**不得静默改走 headless**（兜底交接门用它，见 `remote::codex_ipc`）。
    pub fn build_send_verified(&self) -> bool {
        self.inner
            .build_send_verified
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .unwrap_or(false)
    }

    // ========================================================
    // #172：能力探测（惰性只读；账本优先；单飞）
    // ========================================================

    /// 注入探测账本（生产 `DeviceStore` 实现；测试内存实现）。不注入 = 不查不写，
    /// 仍可探测（降级路径：结论不跨重启复用）。
    pub fn set_probe_ledger(&self, ledger: Arc<dyn ProbeLedger>) {
        *self
            .inner
            .probe_ledger
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(ledger);
        // #172/T9a：账本换手 → 探测只读事实的缓存作废（下次现查）。
        self.invalidate_probe_status_cache();
    }

    // ========================================================
    // #172/T9a：探测结论的只读事实（probe_status）
    // ========================================================

    /// #172/T9a：**这台 build 的读链探测到哪一步了**（只读事实）。
    ///
    /// 数据来源全是既有只读能力，**不新增任何副作用**（不写账本、不建连、不发帧）：
    /// * **在飞** → `probing`（单飞器 `is_flight_active`；同 build 的不同线程
    ///   各有一条单飞键，故按 build 前缀匹配）；
    /// * **账本 `state` 行** → `probed` / `inconclusive` / `failed`
    ///   （映射见 [`super::reason_code::probe_status_from`]）；
    /// * **该 build 尚无任何探测记录** → `idle` + `desktop_build_unverified`；
    /// * **取不到**（无 build 目标 / 账本不可用）→ `idle` + **无码**（不猜）。
    pub async fn probe_status(&self) -> ProbeStatus {
        let Some(target) = self.probe_target_snapshot() else {
            // 没有 build 目标（shipped build / 还没 resolve 到 build）→ 取不到，不猜
            return ProbeStatus::idle();
        };
        let probe_key = self.resolve_probe_key(&target).await;
        // 在飞 → probing（单飞器既有能力；只读查询）
        if self.inner.probe.is_flight_active(&probe_key) {
            return ProbeStatus::probing();
        }
        // 账本结论（缓存；写点失效后重查）
        if let Some(cached) = self
            .inner
            .probe_status_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return cached;
        }
        let status = self.load_probe_status(&probe_key);
        *self
            .inner
            .probe_status_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(status.clone());
        status
    }

    /// 读账本 `state` 行 → 探测只读事实（**只读**：不写账本、不触发探测）。
    fn load_probe_status(&self, probe_key: &str) -> ProbeStatus {
        let Some(ledger) = self.probe_ledger() else {
            // 账本不可用 → 取不到，不猜（不报「未验证」——那也是猜）
            return ProbeStatus::idle();
        };
        match ledger.get(probe_key, CAP_STATE) {
            Some(entry) => super::reason_code::probe_status_from(
                entry.state,
                entry.stage.as_deref(),
                entry.error_code.as_deref(),
            ),
            // 该 build 尚无任何探测记录 → idle + desktop_build_unverified
            None => ProbeStatus::unverified(),
        }
    }

    /// 探测只读事实的缓存作废（账本写点 / build 目标变更点调用）。
    fn invalidate_probe_status_cache(&self) {
        *self
            .inner
            .probe_status_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// 给投影盖上探测只读事实（#172/T9a；**单一盖章点**）。
    ///
    /// 镜像层不知道账本与单飞器，故由 adapter 在**发布前**统一盖；既有字段的
    /// 语义一字不动——本方法只写 `probe_status` 这一个**只读**字段。
    async fn stamp_probe_status(&self, mut p: ThreadProjection) -> ThreadProjection {
        p.probe_status = self.probe_status().await;
        p
    }

    /// 是否正在跑只读探测（#172 的「探测中」信号；T9 用它渲染骨架态）。
    pub fn probe_in_progress(&self) -> bool {
        self.inner.probe.inflight() > 0
    }

    /// 测试缝：收紧探测预算（生产恒 2/3/3s；用例不必真等 8s）。
    #[doc(hidden)]
    pub fn set_probe_budget(&self, budget: super::probe::runner::ProbeBudget) {
        *self
            .inner
            .probe_budget
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = budget;
    }

    fn probe_budget(&self) -> super::probe::runner::ProbeBudget {
        *self
            .inner
            .probe_budget
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// 当前是否处于「尚无证据、可探测」档（唯一允许探测的证据级别）。
    pub fn probe_eligible(&self) -> bool {
        self.current_profile().is_probe_eligible()
    }

    /// **首次 `state` 访问触发的只读探测**（#172）。
    ///
    /// 顺序（都不得阻断读路径）：
    /// 1. 已有写/读证据（shipped 档、或本进程先前探测通过）→ 直接返回；
    /// 2. 非「尚无证据」档（closed / 不可达）→ 不探；
    /// 3. 算/取指纹键（同 build 只算一次）→ 查账本：
    ///    * `probed`/`verified` → 提升本进程 profile 的 `state`，**零帧**复用；
    ///    * `failed` → 本 build 已判死（明确协议错），**本轮不探**（换 build /
    ///      `PROBE_EPOCH` 变更 / 用户显式重探才会再来）；
    ///    * `inconclusive` 未到重试点 → 本轮不探（到点后才重试）；
    /// 4. 探测（P0 `ensure_connection` + P1–P3）→ 落账本；通过才提升 `state`。
    ///
    /// 探测失败**不阻断**后续读路径（follow 照常尝试，读不通自有别的原因）。
    pub async fn ensure_read_evidence(&self, key: &ThreadKey) {
        if self.current_caps().state {
            return; // 已有证据（shipped 或先前探测通过）
        }
        if !self.probe_eligible() {
            return; // closed / 不可达：不探（判死档不因触发而复活）
        }
        // 已跟随该线程 = 生产读链在跑：探测会用临时 follow/unfollow 打扰它（wire 上
        // 后到的 following:false 会撤掉生产跟随）——此时不探，交给后续触发点。
        if self.inner.followings.lock().await.is_following(key) {
            return;
        }
        let Some(mut target) = self.probe_target_snapshot() else {
            return; // 还没 resolve 到 build：无法归因，不探（等下次连接）
        };
        let mut probe_key = self.resolve_probe_key(&target).await;
        if self.reuse_ledger_evidence(&target, &probe_key) {
            return;
        }
        let conn = match self.ensure_connection().await {
            Ok(conn) => conn,
            Err(e) => {
                // P0 不可达 = **没有证据**（Desktop 可能只是没起/冷启动），不是协议被
                // 证伪——记 inconclusive（带错误码），到退避点后再试。
                log::info!(
                    "codex-ipc probe: P0 不可达（{}），本轮不探（key={probe_key}）",
                    e.code()
                );
                self.apply_read_probe_result(
                    &target,
                    &probe_key,
                    &ProbeOutcome::Inconclusive {
                        stage: ProbeStage::P0,
                        error_code: e.code().to_string(),
                        duration_ms: 0,
                    },
                );
                return;
            }
        };
        // 建连可能换代：重新绑定该连接的目标与指纹，不能带着旧 key 探测新连接。
        let Some(connected_target) = self.probe_target_snapshot() else {
            return;
        };
        if connected_target.epoch != conn.epoch() {
            return;
        }
        if target.epoch != connected_target.epoch {
            target = connected_target;
            probe_key = self.resolve_probe_key(&target).await;
        }
        if self.with_current_probe_target(&target, || ()).is_none() {
            return;
        }
        // 单飞键带上线程：同 build 的两个线程各自触发时，A 线程的「无 owner」不得
        // 直接当成 B 线程的结论（账本键仍不含线程——那才是跨线程复用的粒度）。
        let flight_key = format!("{probe_key}|{}|{}", target.epoch, key.thread_id);
        let outcome = self
            .inner
            .probe
            .run(
                &flight_key,
                super::probe::runner::probe_read_with_budget(&conn, key, self.probe_budget()),
            )
            .await;
        // §4.11 观测：结构化日志（不记正文；供排障与人工提升 shipped 表时检索）
        log::info!(
            "codex-ipc probe: key={probe_key} stage={} ok={} error={:?} durationMs={}",
            outcome.stage().as_str(),
            matches!(outcome, ProbeOutcome::Probed { .. }),
            outcome.error_code(),
            outcome.duration_ms()
        );
        if !self.apply_read_probe_result(&target, &probe_key, &outcome) {
            return;
        }
        // #172/T9a：把「用户看到什么」也记进日志（只读事实的中文原文；供排障）
        let status = super::reason_code::probe_status_from(
            outcome.as_state(),
            Some(outcome.stage().as_str()),
            outcome.error_code(),
        );
        let reason = status.reason_code.as_deref().unwrap_or("-");
        log::info!(
            "codex-ipc probe status: state={} reason={reason} userMessage={}",
            status.state.as_str(),
            status
                .reason_code
                .as_deref()
                .and_then(super::reason_code::message_of)
                .unwrap_or("-")
        );
    }

    /// 账本复用（**账本命中必须零帧**）：命中 `probed`/`verified` → 提升 `state` 并
    /// 返回 `true`；`failed` → 返回 `true`（本 build 已判死，本轮不探）；
    /// `inconclusive` 未到重试点 → 返回 `true`（本轮不探，但能力仍关）。
    fn reuse_ledger_evidence(&self, target: &ProbeTarget, probe_key: &str) -> bool {
        if self.with_current_probe_target(target, || ()).is_none() {
            return true;
        }
        let Some(ledger) = self.probe_ledger() else {
            return false;
        };
        let Some(entry) = ledger.get(probe_key, CAP_STATE) else {
            return false;
        };
        // 账本读取期间也可能重连；效果必须在身份守卫内提交。
        self.with_current_probe_target(target, || {
            match entry.state {
                crate::database::dao::codex_ipc_probe::ProbeState::Probed
                | crate::database::dao::codex_ipc_probe::ProbeState::Verified => {
                    log::info!("codex-ipc probe: 账本命中，零帧复用（key={probe_key}）");
                    self.set_state_capability(true);
                    true
                }
                // 明确失败 = 这个 build 已经判死：本轮不探（换 build / epoch 变更 /
                // 用户显式重探才会再来）——否则每次详情页访问都会白跑一遍 15s 探测。
                crate::database::dao::codex_ipc_probe::ProbeState::Failed => true,
                // 不确定：到退避点才重试；窗口内本轮不探
                crate::database::dao::codex_ipc_probe::ProbeState::Inconclusive => {
                    let now = chrono::Utc::now().timestamp_millis();
                    entry.next_retry_at.is_some_and(|at| at > now)
                }
                _ => false,
            }
        })
        .unwrap_or(true)
    }

    /// 与重连身份更新共用 target 锁；同步效果全部完成后才允许切换目标。
    fn with_current_probe_target<T>(
        &self,
        target: &ProbeTarget,
        action: impl FnOnce() -> T,
    ) -> Option<T> {
        let slot = self
            .inner
            .probe_target
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let current = slot.as_ref()?;
        if current.epoch != target.epoch
            || self.inner.epoch.load(Ordering::SeqCst) != target.epoch
            || current.platform != target.platform
            || current.build != target.build
        {
            return None;
        }
        Some(action())
    }

    fn apply_read_probe_result(
        &self,
        target: &ProbeTarget,
        key: &str,
        outcome: &ProbeOutcome,
    ) -> bool {
        self.with_current_probe_target(target, || {
            if let Some(ledger) = self.probe_ledger() {
                ledger.record(key, CAP_STATE, outcome, None);
                self.invalidate_probe_status_cache();
            }
            if matches!(outcome, ProbeOutcome::Probed { .. }) {
                self.set_state_capability(true);
            }
        })
        .is_some()
    }

    fn probe_ledger(&self) -> Option<Arc<dyn ProbeLedger>> {
        self.inner
            .probe_ledger
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn probe_target_snapshot(&self) -> Option<ProbeTarget> {
        self.inner
            .probe_target
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// #172/T8：**当前 build 的账本键**（探测/兼容发送的归因用）。
    ///
    /// 拿不到 build 目标（还没 resolve 到 build）→ `None` = **不记账**（宁可缺行，
    /// 不可错记——错记会把别的 build 的证据算到本 build 头上）。
    pub async fn current_probe_key(&self) -> Option<String> {
        let target = self.probe_target_snapshot()?;
        let key = self.resolve_probe_key(&target).await;
        self.with_current_probe_target(&target, || key)
    }

    /// #172/T8 测试缝：固定账本键（生产恒 `None`）。
    ///
    /// 为什么需要：真实键含 `cli_id` 段——它要跑本机 `codex.exe --version`（spawn）。
    /// 测试里绝不 spawn 本机 CLI，故用固定键把归因钉死（断言也据此写死）。
    #[doc(hidden)]
    pub fn set_probe_key_override(&self, key: Option<String>) {
        *self
            .inner
            .probe_key_override
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = key;
    }

    /// 取（并缓存）该 build 的账本键：`cli_id` 可能要跑一次 `codex.exe --version`
    /// （2s 上限），放阻塞线程池，避免卡住 async worker；同 build 只算一次。
    async fn resolve_probe_key(&self, target: &ProbeTarget) -> String {
        // 测试缝优先：固定键（避免测试里跑本机 CLI 指纹探测——它会 spawn
        // `codex.exe --version`；生产恒 None → 走真实计算）。
        if let Some(fixed) = self
            .inner
            .probe_key_override
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return fixed;
        }
        if let Some(key) = &target.key {
            return key.clone();
        }
        self.resolve_probe_key_with(target, async {
            let platform = target.platform.clone();
            let build = target.build.clone();
            tokio::task::spawn_blocking(move || {
                super::probe::fingerprint::probe_key(&platform, Some(build.as_str())).to_key()
            })
            .await
            .unwrap_or_else(|_| {
                super::probe::fingerprint::ProbeKey::new(
                    &target.platform,
                    Some(target.build.as_str()),
                    None,
                )
                .to_key()
            })
        })
        .await
    }

    async fn resolve_probe_key_with<F>(&self, target: &ProbeTarget, compute: F) -> String
    where
        F: std::future::Future<Output = String>,
    {
        let computed = compute.await;
        // 在 target 锁内核对连接代际与目标身份，拒绝同 build 重连的旧计算。
        let mut slot = self
            .inner
            .probe_target
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(cur) = slot.as_mut() {
            if cur.epoch == target.epoch
                && self.inner.epoch.load(Ordering::SeqCst) == target.epoch
                && cur.platform == target.platform
                && cur.build == target.build
            {
                cur.key = Some(computed.clone());
            }
        }
        computed
    }

    /// 提升本进程 profile 的 `state` 能力（探测通过 / 账本命中）。**只动 state**：
    /// 写能力仍由 shipped 证据或后续 L2 开关决定。
    fn set_state_capability(&self, on: bool) {
        let mut profile = self.inner.profile.lock().expect("profile lock");
        if profile.is_probe_eligible() && profile.capabilities.state != on {
            profile.capabilities.state = on;
        }
    }

    /// 订阅投影事件（REST/SSE/队列调度共用）。
    /// 广播一份投影：代际已由 `ThreadMirror::project` 从**绑定的 lease** 填好
    ///（广播事件与 GET/SSE 首快照同源），此处不再二次盖章——否则会把「新 owner 的
    /// 代际」盖到旧 owner 尚未清理的镜像上，制造虚假的新状态。
    async fn publish_event(&self, p: ThreadProjection) {
        let _ = self.inner.events.send(p);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ThreadProjection> {
        self.inner.events.subscribe()
    }

    /// 确保连接存活（每次重连 epoch 递增，不重置为 0）。
    /// R3：每次连接按 endpoint 重算 profile（未知 build/无 endpoint → closed）；
    /// 连接失败同样落到 closed——Desktop 晚启动不需要重启 Tuvis，投影照常给出
    /// stale/CompatOnly。身份安装锁序 conn → target → profile，同步段不跨 await。
    async fn ensure_connection(&self) -> Result<ConnectionHandle, ProtocolError> {
        if self.inner.stopping.load(Ordering::SeqCst) {
            // C4：shutdown 后不再新建连接（恢复循环/热重启统一由 stop/resume 管理）
            return Err(ProtocolError::Disconnected("adapter shutting down".into()));
        }
        {
            let conn = self.inner.conn.lock().await;
            if let Some(c) = conn.as_ref() {
                if !c.is_closed() {
                    return Ok(c.clone());
                }
            }
        }
        // 连接 + endpoint 身份（R3：resolver 产 endpoint，build 不丢弃）
        let (io, endpoint) = match self.inner.connector.connect_endpoint().await {
            Ok(v) => v,
            Err(e) => {
                // 无 endpoint（Desktop 缺席等）：诚实按 closed 投影
                self.set_profile(ProtocolProfile::closed());
                return Err(e);
            }
        };
        // endpoint → profile（R3：精确 build 匹配；未知 build → **尚无证据**档，可探测）
        let resolved = super::profile::profile_for(
            &endpoint.platform,
            endpoint.build.as_deref().unwrap_or(""),
        );
        // #172：把「build 的写能力证据」单独留存（不看连接存活；连接失败路径不覆盖）
        *self
            .inner
            .build_send_verified
            .lock()
            .unwrap_or_else(|e| e.into_inner()) =
            Some(resolved.as_ref().is_some_and(|p| p.capabilities.send));
        // #172：未命中 shipped 表 → **「尚无证据」档**（不是「明确不可用」）：能力位全关，
        // 但允许首次 state 访问时惰性探测 / 复用账本。debug 专用强制开关把已验证 build
        // 也压到这一档（本机 4894 已 shipped，否则跑不到未知分支）。
        let forced_unverified = force_unverified();
        let build = endpoint.build.clone().unwrap_or_default();
        let profile = match &resolved {
            Some(p) if !forced_unverified => p.clone(),
            _ => ProtocolProfile::unverified(&endpoint.platform, &build),
        };
        // 双检：另一任务可能已重连（持 conn 锁期间不做其他锁操作）
        let (conn, epoch) = {
            let mut guard = self.inner.conn.lock().await;
            if let Some(c) = guard.as_ref() {
                if !c.is_closed() {
                    return Ok(c.clone());
                }
            }
            // 固定锁序 conn → target → profile；同步段内安装整组身份。
            let mut target = self
                .inner
                .probe_target
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let epoch = self.inner.epoch.fetch_add(1, Ordering::SeqCst) + 1;
            *target = profile.is_probe_eligible().then(|| ProbeTarget {
                epoch,
                platform: endpoint.platform.clone(),
                build,
                key: None,
            });
            self.set_profile(profile.clone());
            let conn = super::connection::spawn(io, profile.clone(), epoch);
            *guard = Some(conn.clone());
            (conn, epoch)
        };
        // #172/T9a：build 目标变了 → 探测只读事实的缓存作废（账本键换了）
        self.invalidate_probe_status_cache();
        // 已释放 conn 锁：换代清理（短锁逐个）
        self.inner.owners.lock().await.reset(epoch);
        // 每次连接均检查当前完整指纹；覆盖已知版本升级到未知版本。
        let adapter_for_schema = self.clone();
        tokio::spawn(async move {
            adapter_for_schema.ensure_schema_evidence().await;
        });
        // 旧镜像全部标 stale（重新 following 的 snapshot 复活）
        {
            let mut mirrors = self.inner.mirrors.lock().await;
            for mirror in mirrors.values_mut() {
                mirror.mark_stale();
            }
        }
        // 事件转发任务单飞（恢复期间由恢复任务在 follow 前自行就位，不在此 spawn）
        if !self.inner.recovering.load(Ordering::SeqCst)
            && !self.inner.forward_alive.swap(true, Ordering::SeqCst)
        {
            spawn_forward_task(self.clone(), conn.clone());
        }
        Ok(conn)
    }

    /// 确保线程已发现 owner、声明 following 并收到 snapshot；返回投影。
    /// C2：follow 段经 ensure_lock 串行化——settle 对账与派发路径并发 ensure 时，
    /// 后到者持锁后重查新鲜镜像即短路（不产生第二次 discovery/绑定）。
    pub async fn ensure_thread(&self, key: &ThreadKey) -> Result<ThreadProjection, ProtocolError> {
        let conn = self.ensure_connection().await?;
        // #172：首次 `state` 访问 → 惰性只读探测（账本优先；失败不阻断读路径——
        // 读不通自有别的原因，探测只是给出「这台 build 的读能力有没有证据」）。
        self.ensure_read_evidence(key).await;
        // 快路径：已跟随且新鲜（短锁）
        if let Some(p) = self.fresh_projection(key).await {
            return Ok(p);
        }
        let _ensure = self.inner.ensure_lock.lock().await;
        // 持锁重查：并发 ensure 可能已完成 follow
        if let Some(p) = self.fresh_projection(key).await {
            return Ok(p);
        }
        // 名额（短锁）
        {
            let mut f = self.inner.followings.lock().await;
            if !f.follow(key.clone()) {
                return Err(ProtocolError::Capacity("following set full (64)".into()));
            }
        }
        self.follow_thread(&conn, key).await?;
        Ok(self.projection(key).await)
    }

    /// 已跟随且新鲜的投影（短锁；None = 需要 follow）。
    ///
    /// #172/T9a：镜像锁**之外**再盖探测只读事实（盖章要读账本/单飞器，不得持镜像锁）。
    async fn fresh_projection(&self, key: &ThreadKey) -> Option<ThreadProjection> {
        let p = {
            let mirrors = self.inner.mirrors.lock().await;
            mirrors
                .get(key)
                .filter(|m| !m.is_stale() && m.revision().is_some())
                .map(|m| m.project(self.current_caps()))
        };
        match p {
            Some(p) => Some(self.stamp_probe_status(p).await),
            None => None,
        }
    }

    /// 恢复任务的 follow 统一入口（第三轮复审 B3）：**前置核验**仍有关注（已卸载
    /// 的 key——含 B2 待办合并进来的——直接跳过，不 discovery、不重建镜像）；
    /// follow 完成后**事后复查**，途中被退出关注的（discovery/following 已在途）
    /// 立即回收复活物（following:false + lease + 镜像）——撤销使旧恢复工作失效，
    /// 不只在任务开始时查一次。
    async fn follow_if_following(
        &self,
        conn: &ConnectionHandle,
        key: &ThreadKey,
    ) -> Result<(), ProtocolError> {
        {
            let f = self.inner.followings.lock().await;
            if !f.is_following(key) {
                return Ok(()); // 已卸载：跳过（不重新订阅、不复活镜像）
            }
        }
        // 第四轮复审 B3：成功/错误/取消**所有出口都过统一收尾**——本任务可能已
        // declare(true) 而本地关注已被撤销；错误出口不得 `?` 早退跳过撤销核验
        let result = self.follow_thread(conn, key).await;
        if !self.inner.followings.lock().await.is_following(key) {
            // unfollow 幂等重定义版：有复活 lease/mirror 即回收（含补偿 false）
            self.unfollow(key).await;
        }
        result
    }

    /// 可复用的 follow 流程（R2.5：ensure_thread 与重连恢复共用）：
    /// 建镜像（接 snapshot）→ discovery → 绑定 → following → 等 snapshot。
    /// 锁序纪律（R5）：mirrors → owners，各自短临界区，绝不嵌套、不跨 await 持锁。
    async fn follow_thread(
        &self,
        conn: &ConnectionHandle,
        key: &ThreadKey,
    ) -> Result<(), ProtocolError> {
        // mirror 必须在 following 声明前就位（snapshot 由事件转发落进来；
        // C4：镜像挂全体字节账本）
        {
            let mut mirrors = self.inner.mirrors.lock().await;
            mirrors.entry(key.clone()).or_insert_with(|| {
                ThreadMirror::with_ledger(key.clone(), self.inner.mirror_ledger.clone())
            });
        }
        // discovery（await 不持锁）→ 绑定（短锁）。
        // 第三轮复审 B3：discovery 在途期间关注可能已被撤销——回包后**bind/declare
        // 之前**核验，已卸载则清除为接 snapshot 而建的空镜像并 bail（不绑定 lease、
        // 不声明 following:true）。调用方按失败退避，下一轮前置核验跳过。
        let owner_id = discover_owner(conn, key).await?;
        {
            let f = self.inner.followings.lock().await;
            if !f.is_following(key) {
                let removed = self.inner.mirrors.lock().await.remove(key);
                drop(removed);
                return Err(ProtocolError::StaleContext(
                    "unfollowed during discovery".into(),
                ));
            }
        }
        let lease = {
            let mut owners = self.inner.owners.lock().await;
            bind_lease(&mut owners, key, owner_id, conn.epoch())
        };
        // 新 follow（owner 换代）：load-complete-history 每 follow 至多一次（R1）
        self.inner.history_hydrations.lock().await.remove(key);
        // B3 测试缝：bind 与 declare 之间的可注入等待（生产恒 None 直通）
        let gate = self.inner.follow_gate.lock().expect("gate lock").clone();
        if let Some(gate) = gate {
            self.inner.follow_gate_reached.store(true, Ordering::SeqCst);
            gate.notified().await;
            self.inner
                .follow_gate_reached
                .store(false, Ordering::SeqCst);
        }
        // 第四轮复审 B3：declare **之前**最后核验——bind/declare 窗口内已卸载则
        // 不声明（清刚 bind 的复活 lease 与镜像，bail 交调用方退避/跳过）
        if !self.inner.followings.lock().await.is_following(key) {
            self.inner.owners.lock().await.release(key);
            let removed = self.inner.mirrors.lock().await.remove(key);
            drop(removed);
            return Err(ProtocolError::StaleContext(
                "unfollowed before declare".into(),
            ));
        }
        let mut validity = {
            let owners = self.inner.owners.lock().await;
            owners.validity(&lease)
        };
        declare_following(conn, key).await?;
        // 第四轮复审 B3：declare **之后**复查——declare 前已发生的 unfollow 把
        // following:false 发上了 wire，本任务刚发出的 true 就是「false 之后的
        // 迟到 true」，必须由本任务**直接向 wire** 补偿撤销（不依赖本地 lease/
        // mirror 是否还在——它们可能已被 unfollow 清空），随后清本地并按失败退出
        if !self.inner.followings.lock().await.is_following(key) {
            if let Ok(version) = conn.version_of("thread-stream-following-changed") {
                let _ = conn
                    .broadcast(
                        "thread-stream-following-changed",
                        version,
                        serde_json::json!({
                            "conversationId": key.thread_id,
                            "hostId": key.host_id,
                            "following": false,
                        }),
                    )
                    .await;
            }
            self.inner.owners.lock().await.release(key);
            let removed = self.inner.mirrors.lock().await.remove(key);
            drop(removed);
            return Err(ProtocolError::StaleContext(
                "unfollowed around declare".into(),
            ));
        }
        // 换代兜底：following 声明后 lease 失效（如 Desktop 断开）则失败重试
        if !*validity.borrow_and_update() {
            return Err(ProtocolError::StaleContext(
                "lease lost during follow".into(),
            ));
        }
        wait_first_snapshot(conn, key).await?;
        let _ = &mut validity;
        self.wait_fresh_projection(key).await?;
        Ok(())
    }

    async fn wait_fresh_projection(&self, key: &ThreadKey) -> Result<(), ProtocolError> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            {
                let mirrors = self.inner.mirrors.lock().await;
                if let Some(m) = mirrors.get(key) {
                    if !m.is_stale() && m.revision().is_some() {
                        return Ok(());
                    }
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(ProtocolError::Timeout(
                    "snapshot not applied in time".into(),
                ));
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// 当前投影（不触发连接；未跟随/无状态 → stale 空投影）。
    ///
    /// #172/T7：投影前把当前**审批策略**下发给镜像——`allowedDecisions` 只在这里算，
    /// 前端不重复策略。
    ///
    /// #172/T9a：镜像锁**之外**再盖探测只读事实（盖章要读账本/单飞器，不得持镜像锁）。
    pub async fn projection(&self, key: &ThreadKey) -> ThreadProjection {
        let caps = self.current_caps();
        let policy = self.approval_policy();
        let p = {
            let mut mirrors = self.inner.mirrors.lock().await;
            match mirrors.get_mut(key) {
                Some(m) => {
                    m.set_approval_policy(Some(policy));
                    Some(m.project(caps))
                }
                None => None,
            }
        };
        let p = p.unwrap_or(ThreadProjection {
            thread_id: key.thread_id.clone(),
            revision: None,
            stale: true,
            availability: ControlAvailability::CompatOnly,
            capabilities: caps,
            runtime_status: "unknown".into(),
            content_state: ContentState::Loading,
            messages: Vec::new(),
            truncated: false,
            history_token: String::new(),
            pending: Vec::new(),
            probe_status: ProbeStatus::idle(),
            owner_epoch: 0,
            owner_generation: 0,
        });
        self.stamp_probe_status(p).await
    }

    /// 该 Thread 是否已有镜像（分页端点前置：无镜像才走有界 ensure/hydrate）。
    pub async fn has_mirror(&self, key: &ThreadKey) -> bool {
        self.inner.mirrors.lock().await.contains_key(key)
    }

    /// 同代只读历史页：取镜像锁 → **同步**分页 → 释放；不跨 await 持锁、不重扫
    /// rollout/磁盘。无镜像 → `unavailable/mirror_unavailable`（不是 `end`）。
    pub async fn messages_page(
        &self,
        key: &ThreadKey,
        cursor: Option<&str>,
        history_token: &str,
        limit: usize,
    ) -> MessagesPage {
        let mirrors = self.inner.mirrors.lock().await;
        match mirrors.get(key) {
            Some(mirror) => mirror.messages_page(cursor, history_token, limit),
            None => MessagesPage {
                messages: Vec::new(),
                history_token: String::new(),
                revision: None,
                next_before: None,
                status: MessagesPageStatus::Unavailable,
                reason_code: Some("mirror_unavailable".into()),
            },
        }
    }

    /// 同步忙快照（Codex Desktop IPC Task 8 加性接口：`DesktopDispatch::thread_busy`
    /// 的 R2.3 忙判定消费；不跨 await）。active/unknown（含快照缺字段）按「忙」保守
    /// ——消息留队等 idle 边沿接力；idle/failed 不忙；锁被占（镜像正在写）也按忙保守。
    /// 无镜像（未跟随）= 不忙（派发路径 ensure_thread 先行，正常不可达此态）。
    pub fn busy_snapshot(&self, key: &ThreadKey) -> bool {
        let caps = self.current_caps();
        match self.inner.mirrors.try_lock() {
            Ok(mirrors) => mirrors
                .get(key)
                .map(|m| {
                    matches!(
                        m.project(caps).runtime_status.as_str(),
                        "active" | "unknown"
                    )
                })
                .unwrap_or(false),
            Err(_) => true,
        }
    }

    /// 正式消息身份核验（2026-10-20 复审 P1）：该 Turn 中是否已有本次请求的
    /// `userMessage.clientId`。仅用于 A 路 start-turn 回执关联，不参与展示。
    pub async fn turn_has_client_message(
        &self,
        key: &ThreadKey,
        turn_id: &str,
        client_message_id: &str,
    ) -> bool {
        let mirrors = self.inner.mirrors.lock().await;
        mirrors
            .get(key)
            .is_some_and(|m| m.turn_has_client_message(turn_id, client_message_id))
    }

    /// **派发边界快照**（2026-10-20 复审 P1）：ensure 之后、start-turn 之前的忙态与
    /// 已存在 Turn 集合，**同一把镜像锁**取出（保证两判据来自同一时刻状态）。
    /// 忙 = active/unknown；无镜像 = 不忙（ensure 失败路径另由 start_turn 如实 NotSent）；
    /// 锁被占（镜像正在写）= 忙（保守留队）。
    pub fn pre_write_snapshot(&self, key: &ThreadKey) -> (bool, std::collections::HashSet<String>) {
        let caps = self.current_caps();
        match self.inner.mirrors.try_lock() {
            Ok(mirrors) => match mirrors.get(key) {
                Some(m) => {
                    let p = m.project(caps);
                    let busy = matches!(p.runtime_status.as_str(), "active" | "unknown");
                    (busy, m.turn_ids())
                }
                None => (false, std::collections::HashSet::new()),
            },
            Err(_) => (true, std::collections::HashSet::new()),
        }
    }

    /// 2026-10-17 复审 C1：绑定 Turn 的终态证据——该 turnId 的实体在当前镜像中以
    /// 终态（completed/failed）出现才 true。无镜像/镜像 stale/锁被占（镜像正在写）
    /// → false（保守不释放：屏障宁可多留一拍，绝不因证据缺失放行第二条）。
    pub fn turn_evidence_terminal(&self, key: &ThreadKey, turn_id: &str) -> bool {
        match self.inner.mirrors.try_lock() {
            Ok(mirrors) => mirrors
                .get(key)
                .map(|m| m.turn_is_terminal(turn_id))
                .unwrap_or(false),
            Err(_) => false,
        }
    }

    /// 退出跟随（2026-10-17 复审 C4 收口：最后一引用退出 = 真正退场）——
    /// 向 Desktop 声明 `following:false`、回收 owner lease（后续该 owner 的帧无
    /// 授权可套）、删除镜像（字节账本随 Drop 释放；卸载后的帧**不复活镜像**）。
    /// 未在跟随（幂等）→ 零副作用。
    pub async fn unfollow(&self, key: &ThreadKey) {
        let was_following = self.inner.followings.lock().await.unfollow(key);
        // C4：只对**真正声明过 following**（有现行 lease）的 thread 发 following:false
        //——ensure 失败回滚（discovery 未成功、从未声明）只回收名额，不发噪声帧
        let had_lease = self.inner.owners.lock().await.current(key).is_some();
        let had_mirror = self.inner.mirrors.lock().await.contains_key(key);
        // 第三轮复审 B3：幂等 = 「真的无物可回收」——following 集合、lease、镜像
        // 三者皆无才早退。在途恢复任务复活出的 lease/镜像（following 已被撤销）
        // 必须同样被回收，不能因 was_following=false 提前返回而滞留。
        if !was_following && !had_lease && !had_mirror {
            return;
        }
        if had_lease && !self.inner.stopping.load(Ordering::SeqCst) {
            if let Some(conn) = current_conn(self).await {
                if let Ok(version) = conn.version_of("thread-stream-following-changed") {
                    let _ = conn
                        .broadcast(
                            "thread-stream-following-changed",
                            version,
                            serde_json::json!({
                                "conversationId": key.thread_id,
                                "hostId": key.host_id,
                                "following": false,
                            }),
                        )
                        .await;
                }
            }
        }
        self.inner.owners.lock().await.release(key);
        let removed = self.inner.mirrors.lock().await.remove(key);
        drop(removed); // Drop 释放字节账本占用
    }

    /// C4：进入停机——恢复循环/重拉/转发不再重启，ensure 不再新建连接；
    /// 已派发动作的独立落账任务不受影响（只经 store/conn 完成落账）。
    pub fn begin_shutdown(&self) {
        self.inner.stopping.store(true, Ordering::SeqCst);
    }

    /// C4：从停机恢复（hub.start 热重启路径）——重新允许连接。
    pub fn resume(&self) {
        self.inner.stopping.store(false, Ordering::SeqCst);
    }

    /// C4 探针：是否处于停机态。
    #[doc(hidden)]
    pub fn is_stopping(&self) -> bool {
        self.inner.stopping.load(Ordering::SeqCst)
    }

    /// 原生 start turn（text 为已组装文本——来源标记由 router 恰好组装一次）。
    pub async fn start_turn(
        &self,
        key: &ThreadKey,
        action_id: &str,
        text: &str,
        client_message_id: &str,
    ) -> RpcOutcome {
        self.start_turn_with(key, action_id, text, client_message_id, false)
            .await
    }

    /// **兼容发送变体（#172/T8）**：本 build 的写能力**未取证**时，用户在设置页显式
    /// 开启「兼容发送（实验）」后，允许走**同一条 start_turn 路径**尝试写出
    /// （wire 形态逐字不变——只放开能力门，不改协议）。
    ///
    /// 为什么放在 adapter 而不是调用方：`start_turn` 的能力门是「这个 build 能不能写」
    /// 的**最后一道**防线，绕过它就等于在未验证 build 上盲发。放行许可只能来自
    /// **同一个**设置键 + 同一个 build 证据判据（remote 层与派发层共用）。
    pub async fn start_turn_compat(
        &self,
        key: &ThreadKey,
        action_id: &str,
        text: &str,
        client_message_id: &str,
    ) -> RpcOutcome {
        self.start_turn_with(key, action_id, text, client_message_id, true)
            .await
    }

    /// start turn 主体（`compat` = 兼容发送许可；false = 既有语义逐字不变）。
    async fn start_turn_with(
        &self,
        key: &ThreadKey,
        _action_id: &str,
        text: &str,
        client_message_id: &str,
        compat: bool,
    ) -> RpcOutcome {
        let envelope_id = super::types::new_uuid();
        // R3：能力读当前连接的动态 profile（无连接/未知 build → closed → 拒）
        // #172/T8：兼容发送许可只放开「写能力未取证」这一档；**已验证 build 的行为
        // 完全不变**（它的 caps.send 本来就是 true，根本进不到这个分支）。
        if !self.current_caps().send && !compat {
            return not_sent(&envelope_id, "capability_unavailable");
        }
        let (lease, validity) = {
            let owners = self.inner.owners.lock().await;
            match current_lease(&owners, key) {
                Some(l) => {
                    let v = owners.validity(&l);
                    (l, v)
                }
                None => return not_sent(&envelope_id, "stale_context"),
            }
        };
        let params = json!({
            "conversationId": key.thread_id,
            "turnStart": {
                "request": {
                    "threadId": key.thread_id,
                    "input": [{ "type": "text", "text": text, "text_elements": [] }],
                    "clientUserMessageId": client_message_id,
                },
                "context": {},
            }
        });
        // 版本不在此解析：dispatch 统一按方法表查（无连接 → disconnected，
        // 方法缺失 → unsupported_method，均为零投递）。
        self.dispatch("thread-follower-start-turn", params, lease, validity)
            .await
    }

    /// 审批/作答。requestKey/contextToken 回显校验；服务端保存的 pending 上下文是
    /// 唯一事实源（远端不上送 owner/permissions/raw method）。
    /// 2026-10-17 复审 C3：拆锁后的最终复核携带**完整上下文代际**——阶段 1 记录镜像
    /// 绑定的 owner lease；阶段 3 的现行 lease 必须与其同代（client/epoch/generation
    /// 全同），并在 owners 复核后以短锁重验镜像非 stale 且 token 仍有效（卡内容/存在
    /// 未变）。两阶段间 owner 换代/参数变化 → 旧载荷零写入（stale_context）。
    pub async fn respond(
        &self,
        key: &ThreadKey,
        request_key: &str,
        context_token: &str,
        action: PendingAction,
    ) -> RpcOutcome {
        let envelope_id = super::types::new_uuid();
        let caps = self.current_caps();
        let inflight_key = (key.thread_id.clone(), request_key.to_string());
        // 阶段 1（R5 短锁：只持 mirrors）：stale 门（R4.1）+ 全部 mirror 读 + 镜像
        // 绑定的 owner lease（C3 同代复核锚）
        let policy = self.approval_policy();
        let snap = {
            let mirrors = self.inner.mirrors.lock().await;
            let Some(mirror) = mirrors.get(key) else {
                return not_sent(&envelope_id, "stale_context");
            };
            if mirror.is_stale() {
                return not_sent(&envelope_id, "stale_context");
            }
            RespondSnapshot::collect(mirror, caps, &policy, request_key, context_token)
        };
        // 阶段 2（R5 短锁：只持 inflight）：清理已消失/换 token 的 Blocked + 抢占提交权
        {
            let mut inflight = self.inner.inflight.lock().await;
            let live: std::collections::HashMap<&str, &str> = snap
                .live_pendings
                .iter()
                .map(|(rk, tk)| (rk.as_str(), tk.as_str()))
                .collect();
            inflight.retain(|(tid, rk), entry| {
                !(tid == &key.thread_id
                    && entry.state == InflightState::BlockedUnknown
                    && match live.get(rk.as_str()) {
                        // pending 消失 → 屏障解除
                        None => true,
                        // 重现但换 token（新世界）→ 旧 Unknown 不挡新卡
                        Some(cur) => Some(*cur) != entry.token.as_deref(),
                    })
            });
            if inflight.contains_key(&inflight_key) {
                return not_sent(&envelope_id, "pending_submit_in_progress");
            }
            inflight.insert(
                inflight_key.clone(),
                InflightEntry {
                    state: InflightState::Dispatching,
                    token: None,
                },
            );
        }
        // 校验 + wire 组装（不持任何锁；失败必须退掉提交权）。
        // C3 同代复核锚在 snap 被消耗前取出
        let mirror_lease = snap.mirror_lease.clone();
        let prepared = match snap.into_prepared(key, request_key, context_token, &action) {
            Ok(p) => p,
            Err(reason) => {
                self.inner.inflight.lock().await.remove(&inflight_key);
                return not_sent(&envelope_id, &reason);
            }
        };
        // C3 测试缝：阶段 1/2 与阶段 3 之间可注入等待（生产恒直通）
        //（先绑定 clone 再判——if let 暂存的 MutexGuard 会跨 await，破坏 Send）
        let gate = self
            .inner
            .interphase_gate
            .lock()
            .expect("gate lock")
            .clone();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        // 阶段 3（R5 短锁：只持 owners）：lease 最后复核（响应前读到的过期 lease 不算
        // 授权）+ C3 同代复核——现行 lease 必须就是阶段 1 镜像所绑定的那一代
        let lease_valid = {
            let owners = self.inner.owners.lock().await;
            match current_lease(&owners, key) {
                Some(l) if Some(&l) == mirror_lease.as_ref() => {
                    let v = owners.validity(&l);
                    Some((l, v))
                }
                // 无 lease / 已换代（阶段 1 之后 owner 失效或被接管）：旧载荷零写入
                _ => None,
            }
        };
        let (lease, validity) = match lease_valid {
            Some(v) => v,
            None => {
                self.inner.inflight.lock().await.remove(&inflight_key);
                return not_sent(&envelope_id, "stale_context");
            }
        };
        // 阶段 3.5（C3 短锁：只持 mirrors）：镜像复核——非 stale 且 token 仍绑定当前
        // 上下文（卡还在场、内容/方法未变；流式 revision 前进不影响同 token）
        let reverified = {
            let mirrors = self.inner.mirrors.lock().await;
            mirrors
                .get(key)
                .is_some_and(|m| !m.is_stale() && m.token_valid(request_key, context_token))
        };
        if !reverified {
            self.inner.inflight.lock().await.remove(&inflight_key);
            return not_sent(&envelope_id, "stale_context");
        }
        let outcome = self
            .dispatch(prepared.method, prepared.params, lease, validity)
            .await;
        // R4.3 + C3：Accepted/Rejected/NotSent（保守零投递，可重试）释放提交权；
        // UnknownDelivery → 保留为 Blocked（记提交所用 token 分代，状态应用驱动清理）。
        {
            let mut inflight = self.inner.inflight.lock().await;
            if outcome.delivery == Delivery::UnknownDelivery {
                inflight.insert(
                    inflight_key,
                    InflightEntry {
                        state: InflightState::BlockedUnknown,
                        token: Some(context_token.to_string()),
                    },
                );
            } else {
                inflight.remove(&inflight_key);
            }
        }
        outcome
    }

    /// 统一派发：mutating RPC（未知结果绝不自动重试——connection 语义保证）。
    ///
    /// **版本号唯一来源 = profile 方法表**：这里按方法名查表（`version_of`），任何调用
    /// 点都不得自带版本——组装侧硬编码会绕过单一真相源。方法不在表里 = **明确方法
    /// 缺失** → `NotSent`（零投递），绝不用猜测版本瞎发；**新增方法必须先有真机证据
    /// 才能进表**（`profile::method_table` 注释）。
    async fn dispatch(
        &self,
        method: &str,
        params: Value,
        target: OwnerLease,
        validity: tokio::sync::watch::Receiver<bool>,
    ) -> RpcOutcome {
        let envelope_id = super::types::new_uuid();
        let conn = self.inner.conn.lock().await.clone();
        let Some(conn) = conn else {
            return not_sent(&envelope_id, "disconnected");
        };
        // 方法缺失 = 明确协议错（unsupported_method）→ 零投递，保持既有 not_sent 语义
        let version = match conn.version_of(method) {
            Ok(v) => v,
            Err(e) => return not_sent(&envelope_id, e.code()),
        };
        conn.request(RpcRequest {
            method: method.to_string(),
            params,
            version: Some(version),
            target: Some(target),
            mutating: true,
            validity: Some(validity),
            source_client_id: None,
            timeout_ms: None,
        })
        .await
    }

    pub fn shutdown(&self) -> Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        let inner = self.inner.clone();
        Box::pin(async move {
            let conn = inner.conn.lock().await.clone();
            if let Some(conn) = conn {
                conn.shutdown().await;
            }
        })
    }

    /// C3 测试缝：注入 respond 两阶段间的等待门（生产不调用——恒直通）。
    #[doc(hidden)]
    pub fn set_respond_interphase_gate(&self, gate: Arc<tokio::sync::Notify>) {
        *self.inner.interphase_gate.lock().expect("gate lock") = Some(gate);
    }

    /// B3 测试缝：注入 follow 的 bind/declare 间等待门（生产不调用——恒直通）。
    #[doc(hidden)]
    pub fn set_follow_gate(&self, gate: Arc<tokio::sync::Notify>) {
        *self.inner.follow_gate.lock().expect("gate lock") = Some(gate);
    }

    /// B3 测试缝：任务是否已到达 follow gate。
    #[doc(hidden)]
    pub fn follow_gate_reached(&self) -> bool {
        self.inner.follow_gate_reached.load(Ordering::SeqCst)
    }

    /// C3 测试缝：该 (thread, request_key) 是否已抢占提交权（回归测试固定
    /// 「respond 已过阶段 2、停在测试缝」的时序用）。
    #[doc(hidden)]
    pub fn inflight_claimed(&self, thread_id: &str, request_key: &str) -> bool {
        self.inner
            .inflight
            .try_lock()
            .map(|g| g.contains_key(&(thread_id.to_string(), request_key.to_string())))
            .unwrap_or(false)
    }
}

fn not_sent(envelope_id: &str, reason: &str) -> RpcOutcome {
    RpcOutcome {
        delivery: Delivery::NotSent,
        envelope_id: envelope_id.to_string(),
        result: None,
        handled_by: None,
        reason_code: Some(reason.to_string()),
    }
}

fn current_lease(owners: &OwnerRegistry, key: &ThreadKey) -> Option<OwnerLease> {
    let lease = owners.current(key)?;
    owners.is_current(&lease).then_some(lease)
}

/// 审批/作答类方法名（集中登记，避免散落在组装与派发两侧）。
///
/// **版本号唯一来源 = [`ProtocolProfile`] 方法表**（`profile::method_table`）：这些常量
/// 只钉方法名，版本一律由 [`DesktopIpcAdapter::dispatch`] 用 `conn.version_of` 解析；
/// 方法不在表里 = 明确方法缺失 → `NotSent`（零投递），绝不带猜测版本瞎发。
/// **新增方法必须先有真机证据才能进表**（见 observed/README.md）。
const METHOD_COMMAND_DECISION: &str = "thread-follower-command-approval-decision";
const METHOD_FILE_DECISION: &str = "thread-follower-file-approval-decision";
const METHOD_PERMISSIONS_RESPONSE: &str = "thread-follower-permissions-request-approval-response";
const METHOD_SUBMIT_USER_INPUT: &str = "thread-follower-submit-user-input";

/// 审批类 pending → 对应决策方法名（command / file 各用各的方法，绝不混用）。
/// 非审批类（permissions / 问答 / 不支持）没有「决策词」方法，返回 `None`。
fn decision_method(kind: &PendingKind) -> Option<&'static str> {
    match kind {
        PendingKind::CommandApproval => Some(METHOD_COMMAND_DECISION),
        // 官方方法表 `26.1016.6573.0` 已核对：file 决策方法与 command 并列登记为 v1
        PendingKind::FileChangeApproval => Some(METHOD_FILE_DECISION),
        _ => None,
    }
}

/// respond 的镜像侧快照（阶段 1 的产物；收集于 mirrors 短锁内，之后不持锁使用）。
struct RespondSnapshot {
    /// 能力基线（阶段 1 取自当前动态 profile）
    caps: CapabilitySet,
    /// 当前投影中的 (request_key, context_token) 全集（R4/C3 Blocked 清理依据——
    /// 屏障按完整上下文分代，重现换 token 即解除）
    live_pendings: Vec<(String, String)>,
    /// 当前投影中的本请求卡（存在性 + kind + questions）
    pending: Option<PendingView>,
    /// token 是否绑定当前上下文
    token_valid: bool,
    /// 阶段 1 镜像绑定的 owner lease（C3 同代复核锚：阶段 3 现行 lease 必须与之全同）
    mirror_lease: Option<OwnerLease>,
    /// 服务端保存的原 permissions 回显源（requests[].params.permissions）
    raw_requests: Option<Value>,
    request_id: Option<Value>,
}

impl RespondSnapshot {
    /// 阶段 1：mirrors 短锁内完成全部 mirror 读（R4.1 stale 门已由调用方先行）。
    fn collect(
        mirror: &ThreadMirror,
        caps: CapabilitySet,
        policy: &super::pending::DecisionPolicy,
        request_key: &str,
        _token: &str,
    ) -> Self {
        let projection = mirror.project_with_policy(caps, policy);
        let live_pendings = projection
            .pending
            .iter()
            .map(|p| (p.request_key.clone(), p.context_token.clone()))
            .collect();
        let pending = projection
            .pending
            .iter()
            .find(|p| p.request_key == request_key)
            .cloned();
        // requestKey 无损可逆；解析失败按 invalid_id 拒（阶段 2.5 报告）
        let request_id = parse_request_key(request_key)
            .ok()
            .and_then(|id| serde_json::to_value(&id).ok());
        // R1：raw_state 即 conversationState 本体，requests 按根直读
        let raw_requests = mirror
            .raw_state()
            .and_then(|raw| raw.get("requests").cloned());
        Self {
            caps,
            live_pendings,
            pending,
            token_valid: mirror.token_valid(request_key, _token),
            mirror_lease: mirror.lease().cloned(),
            raw_requests,
            request_id,
        }
    }

    /// 阶段 2.5：校验 + wire 组装（不持任何锁）。Err = 拒绝原因（零投递）。
    fn into_prepared(
        self,
        key: &ThreadKey,
        request_key: &str,
        context_token: &str,
        action: &PendingAction,
    ) -> Result<PreparedRespond, String> {
        // pending 现存
        let pending = self.pending.ok_or_else(|| "stale_context".to_string())?;
        // token 绑定当前上下文（过期卡拒绝）
        if !self.token_valid {
            return Err("stale_context".into());
        }
        let request_id_value = self.request_id.ok_or_else(|| "invalid_id".to_string())?;
        let caps = self.caps;
        // (方法名, 决策词)；决策词只对 command/file 有意义，其余为空串。
        // **这里不写版本号**：版本唯一来源是方法表，由派发侧 `dispatch` 用
        // `conn.version_of(method)` 解析（方法缺失 → NotSent，见 dispatch 注释）。
        let (method, decision_word): (&'static str, &str) = match (&pending.kind, action) {
            // 命令 / 文件变更审批：**决策词由服务端策略给定的 allowedDecisions 决定**
            // （单一来源；未知 build 只有 accept/cancel，已知 build 有 accept/decline）
            (
                PendingKind::CommandApproval | PendingKind::FileChangeApproval,
                PendingAction::Approve | PendingAction::Reject,
            ) => {
                let method =
                    decision_method(&pending.kind).ok_or_else(|| "unsupported".to_string())?;
                let word = match action {
                    PendingAction::Approve => {
                        pick_decision(&pending.allowed_decisions, &[DECISION_ACCEPT])
                    }
                    // 拒绝：优先 `decline`（回合继续），策略只给了 `cancel` 时用它
                    // （终止本轮）——两者语义不同，绝不混用
                    _ => pick_decision(
                        &pending.allowed_decisions,
                        &[DECISION_DECLINE, DECISION_CANCEL],
                    ),
                }
                .ok_or_else(|| "capability_unavailable".to_string())?;
                (method, word)
            }
            (PendingKind::PermissionsApproval, PendingAction::Approve | PendingAction::Reject) => {
                if !caps.permissions {
                    return Err("capability_unavailable".into());
                }
                (METHOD_PERMISSIONS_RESPONSE, "")
            }
            (PendingKind::UserInput, PendingAction::Answer(_)) => {
                if !caps.blocking_input {
                    return Err("capability_unavailable".into());
                }
                (METHOD_SUBMIT_USER_INPUT, "")
            }
            (PendingKind::Unsupported, _) => return Err("unsupported".into()),
            _ => return Err("action_mismatch".into()),
        };
        let approve = matches!(action, PendingAction::Approve);
        let params = match (&pending.kind, action) {
            // command / file 共用同一决策体：只发 decision 词，绝不携带 permissions 对象
            (PendingKind::CommandApproval | PendingKind::FileChangeApproval, _) => {
                json!({
                    "conversationId": key.thread_id,
                    "requestId": request_id_value,
                    "decision": decision_word,
                })
            }
            (PendingKind::PermissionsApproval, _) => {
                // permissions 变体：批准=回显服务端保存的原 permissions；拒绝={}；scope=turn
                let original = self
                    .raw_requests
                    .and_then(|r| r.as_array().cloned())
                    .and_then(|arr| {
                        arr.iter()
                            .find(|r| {
                                r.get("id")
                                    .map(|id| id == &request_id_value)
                                    .unwrap_or(false)
                            })
                            .cloned()
                    })
                    .and_then(|req| {
                        req.get("params")
                            .and_then(|p| p.get("permissions"))
                            .cloned()
                    });
                let permissions = if approve {
                    original.unwrap_or(json!({}))
                } else {
                    json!({})
                };
                json!({
                    "conversationId": key.thread_id,
                    "requestId": request_id_value,
                    "response": { "permissions": permissions, "scope": "turn" },
                })
            }
            (PendingKind::UserInput, PendingAction::Answer(answers)) => {
                // 按 qid 精确映射：不能漏答/多答/混入未知 qid；isOther=false 禁自由文本
                let questions = &pending.questions;
                if answers.len() != questions.len() {
                    return Err("answers_mismatch".into());
                }
                let mut mapped = serde_json::Map::new();
                for q in questions {
                    let Some(entries) = answers.get(&q.id) else {
                        return Err("answers_mismatch".into());
                    };
                    if entries.is_empty() {
                        return Err("answers_mismatch".into());
                    }
                    if !q.is_other
                        && entries
                            .iter()
                            .any(|e| !q.options.iter().any(|o| &o.label == e))
                    {
                        return Err("answer_not_option".into());
                    }
                    mapped.insert(q.id.clone(), json!({ "answers": entries }));
                }
                json!({
                    "conversationId": key.thread_id,
                    "requestId": request_id_value,
                    "response": { "answers": Value::Object(mapped) },
                })
            }
            _ => return Err("action_mismatch".into()),
        };
        let _ = request_key;
        let _ = context_token;
        Ok(PreparedRespond { method, params })
    }
}

/// wire 组装完成后的派发载荷。
///
/// **不携带版本号**：版本唯一来源是 profile 方法表，由 [`DesktopIpcAdapter::dispatch`]
/// 在真正派发前按方法名解析（方法不在表里 → NotSent）。组装侧若自带版本，等于绕过
/// 单一真相源——这正是本结构此前硬编码 `version = 1` 的问题。
struct PreparedRespond {
    method: &'static str,
    params: Value,
}

/// 从策略给的 `allowedDecisions` 里按**优先级**挑一个词（候选取自语义正确的集合）。
///
/// 例：拒绝的候选是 `[decline, cancel]`——策略同时给了两个词时先用 `decline`
///（回合继续），只给 `cancel` 时才用它（终止本轮）。
fn pick_decision<'a>(allowed: &[String], candidates: &[&'a str]) -> Option<&'a str> {
    candidates
        .iter()
        .copied()
        .find(|word| allowed.iter().any(|w| w == word))
}

// ============================================================
// R1/R2：broadcast 事件转发（need_snapshot 闭环 / owner 失效 / 迟到帧 /
// following 查询 / 断线重连恢复）。锁纪律（R5）：全程顺序短锁，绝不嵌套。
// ============================================================

/// forward_events 的单飞 spawn。独立非 async 函数：把 forward_events 的 future
/// 类型移出 ensure_connection 的协程内部类型，断开 ensure→forward→recovery→ensure
/// 的递归类型环（否则该环上 auto trait Send 不可计算，编译报 cycle）。
fn spawn_forward_task(adapter: DesktopIpcAdapter, conn: ConnectionHandle) {
    tokio::spawn(async move { forward_events(adapter, conn).await });
}

/// broadcast 事件转发主循环：state/信号处理；断线 → 全体 stale + 发布 → 单飞
/// 交接给重连恢复任务后退出（新连接的事件处理由恢复任务在 follow 前先行就位）。
async fn forward_events(adapter: DesktopIpcAdapter, conn: ConnectionHandle) {
    let mut rx = conn.subscribe();
    loop {
        match rx.recv().await {
            Ok(ConnectionEvent::Broadcast { frame, .. }) => {
                handle_broadcast(&adapter, &conn, frame).await;
            }
            Ok(ConnectionEvent::Initialized { .. }) => continue,
            Ok(ConnectionEvent::Disconnected { .. }) | Err(_) => break,
        }
    }
    // 连接断开：全体 mirror stale + 发布（保留视图只读）
    mark_all_mirrors_stale(&adapter, false).await;
    adapter.inner.forward_alive.store(false, Ordering::SeqCst);
    if adapter.inner.stopping.load(Ordering::SeqCst) {
        return;
    }
    // 单飞交接（R2.5：AtomicBool 防并发恢复；已有恢复在跑则本任务直接退出）
    if adapter.inner.recovering.swap(true, Ordering::SeqCst) {
        return;
    }
    spawn_recovery_task(adapter);
}

/// 恢复任务的单飞 spawn（独立非 async 函数，理由同 [`spawn_forward_task`]）。
fn spawn_recovery_task(adapter: DesktopIpcAdapter) {
    tokio::spawn(async move {
        let recovered = recovery_loop(&adapter).await;
        adapter.inner.recovering.store(false, Ordering::SeqCst);
        if recovered.is_none() {
            // shutdown：转发任务已随断线退出，恢复任务也退出
            adapter.inner.forward_alive.store(false, Ordering::SeqCst);
        }
    });
}

/// 同 socket 恢复的工作队列与消费者资格（第四轮复审 B2）：pending 与 running
/// 同锁。「取空待办」与「释放消费者资格」在 [`Self::take_or_release`] 的**同一
/// 临界区**内完成——任务退出判定与并发入队之间不存在可插入窗口：take 空 ⇒
/// 资格已原子释放，其后到达的 [`Self::enqueue`] 必然拿到资格并启动新任务
///（`pending=[B], running=false` 无消费者的状态不可能出现）。
#[derive(Default)]
struct RecoveryQueue {
    pending: Vec<ThreadKey>,
    running: bool,
}

impl RecoveryQueue {
    /// spawn 侧：并入 keys 并竞争消费者资格；`true` = 本调用者须启动任务。
    fn enqueue(&mut self, keys: impl IntoIterator<Item = ThreadKey>) -> bool {
        self.pending.extend(keys);
        if self.running {
            // 在飞任务会在下一轮 take_or_release 消费（同锁，绝不丢）
            return false;
        }
        self.running = true;
        true
    }

    /// 任务侧：原子取走全部待办；**为空时同时释放消费者资格**（B2 不变量）。
    fn take_or_release(&mut self) -> Option<Vec<ThreadKey>> {
        let keys = std::mem::take(&mut self.pending);
        if keys.is_empty() {
            self.running = false;
            None
        } else {
            Some(keys)
        }
    }

    /// 任务侧中途退出（连接断/停机）：keys 放回待办并释放资格（同一临界区；
    /// 其后的 enqueue 拿到资格即成为新消费者，keys 不丢）。
    fn requeue_and_release(&mut self, keys: impl IntoIterator<Item = ThreadKey>) {
        self.pending.extend(keys);
        self.running = false;
    }
}

/// 2026-10-17 复审 C2 / 第三轮复审 B2：**同 socket 恢复**——owner 失效
///（client-status-changed disconnected）或 `ipc-connection-reset` 后，连接本身
/// 仍活着，连接级恢复循环（由断线驱动）不会触发；若无人重新 discovery/follow，
/// 重新接管的 owner 帧因无 lease 被丢、视图永久 stale。单飞只避免重复工作：
/// 后到失效事件的 keys 在待办锁内并入（绝不丢弃），由在飞任务继续消费。
fn spawn_same_socket_recovery(adapter: DesktopIpcAdapter, keys: Vec<ThreadKey>) {
    if keys.is_empty() || adapter.inner.stopping.load(Ordering::SeqCst) {
        return;
    }
    // B2：入队并竞争消费者资格（同锁原子）——在飞任务存在时直接返回，后到
    // 事件由它继续消费；退出边界瞬间入队的由继任任务接管（RecoveryQueue 契约）
    let start = {
        let mut queue = adapter.inner.recovery_queue.lock().expect("queue lock");
        queue.enqueue(keys)
    };
    if start {
        tokio::spawn(run_same_socket_recovery(adapter));
    }
}

/// 同 socket 恢复主循环（第四轮复审 B2 重构）：每轮 `take_or_release` 取走全部
/// 待办为**一批**；批内失败的 keys 就地退避重试（预算 3 轮，只约束本批——预算
/// 耗尽仅放弃本批，**绝不 clear 共享待办**，等待期间后到的 keys 由下一轮
/// take 消费）；已卸载的 key 由 [`DesktopIpcAdapter::follow_if_following`] 跳过/
/// 回收（B3）。连接断开/停机时未完成 keys 放回队列并释放资格（同临界区）。
async fn run_same_socket_recovery(adapter: DesktopIpcAdapter) {
    loop {
        if adapter.inner.stopping.load(Ordering::SeqCst) {
            // 停机：待办放回（进程退出即随进程消失），资格原子释放
            let mut queue = adapter.inner.recovery_queue.lock().expect("queue lock");
            let leftover = queue.take_or_release();
            if let Some(keys) = leftover {
                queue.requeue_and_release(keys);
            }
            return;
        }
        // 唯一取待办点：空 ⇒ 资格已在同一临界区释放 ⇒ 退出（其后 enqueue 必有
        // 新消费者——B2 原子交接不变量）
        let Some(keys) = ({
            let mut queue = adapter.inner.recovery_queue.lock().expect("queue lock");
            queue.take_or_release()
        }) else {
            break;
        };
        let Some(conn) = current_conn(&adapter).await else {
            // 连接已断：keys 放回待办 + 资格释放（同临界区），让位连接级恢复；
            // 后续事件 enqueue 拿到资格即继续消费
            let mut queue = adapter.inner.recovery_queue.lock().expect("queue lock");
            queue.requeue_and_release(keys);
            return;
        };
        // —— 本批：批内退避重试，预算只约束本批 keys ——
        let mut batch: Vec<ThreadKey> = keys;
        let mut rounds = 0u32;
        loop {
            let mut still_failed: Vec<ThreadKey> = Vec::new();
            for key in &batch {
                if adapter.follow_if_following(&conn, key).await.is_err() {
                    still_failed.push(key.clone());
                }
            }
            if still_failed.is_empty() {
                break;
            }
            rounds += 1;
            if rounds >= 3 {
                // 本批预算耗尽：仅放弃本批失败 keys（与旧约定一致的有界资源）。
                // **共享待办不 clear**——本批等待响应期间后到的 keys 一次都未被
                // 尝试，由循环头 take_or_release 继续消费（B2 回归 2）
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            if adapter.inner.stopping.load(Ordering::SeqCst) {
                let mut queue = adapter.inner.recovery_queue.lock().expect("queue lock");
                queue.requeue_and_release(still_failed);
                return;
            }
            let Some(conn2) = current_conn(&adapter).await else {
                // 连接断：失败 keys 放回待办 + 资格释放，让位连接级恢复
                let mut queue = adapter.inner.recovery_queue.lock().expect("queue lock");
                queue.requeue_and_release(still_failed);
                return;
            };
            let _ = conn2;
            batch = still_failed;
        }
    }
}

/// R2.5 重连恢复循环：退避 1/2/4/8/16/30s（成功归零——本函数成功即返回）；
/// 每轮 ensure_connection → 新连接的事件转发先行就位（swap 幂等防重复）→
/// 对关注集中每个 thread 重新 discover→bind→declare→等 snapshot（B3：经
/// follow_if_following 统一入口——在途退出关注的 key 跳过/回收）。仅退出于
/// shutdown（None）或恢复成功（Some）。
async fn recovery_loop(adapter: &DesktopIpcAdapter) -> Option<ConnectionHandle> {
    let backoffs = [1u64, 2, 4, 8, 16, 30];
    let mut round = 0usize;
    loop {
        if adapter.inner.stopping.load(Ordering::SeqCst) {
            return None;
        }
        if let Ok(conn) = adapter.ensure_connection().await {
            // 新连接的事件处理先行：follow 的 snapshot 由它落进镜像
            if !adapter.inner.forward_alive.swap(true, Ordering::SeqCst) {
                spawn_forward_task(adapter.clone(), conn.clone());
            }
            let keys = followed_keys(adapter).await;
            let mut all_ok = true;
            for key in &keys {
                // B3：恢复统一入口——在途退出关注的 key 跳过/回收，不复活订阅
                if adapter.follow_if_following(&conn, key).await.is_err() {
                    all_ok = false;
                    break;
                }
            }
            if all_ok {
                return Some(conn);
            }
            // 某 thread follow 失败：整轮退避重试（连接可能又断了）
        }
        let delay = backoffs[round.min(backoffs.len() - 1)];
        round += 1;
        tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
    }
}

async fn handle_broadcast(adapter: &DesktopIpcAdapter, conn: &ConnectionHandle, frame: Value) {
    match frame.get("method").and_then(|m| m.as_str()) {
        Some("thread-stream-state-changed") => handle_state_changed(adapter, frame).await,
        Some("client-status-changed") => handle_client_status_changed(adapter, &frame).await,
        // 连接级信号：全体 stale + 发布，并**同 socket 重跟随**（C2：Desktop 侧要求
        // 重置状态时连接未断——只 stale 不重跟随会让视图永久停在旧世界）
        Some("ipc-connection-reset") => {
            mark_all_mirrors_stale(adapter, false).await;
            let keys = followed_keys(adapter).await;
            spawn_same_socket_recovery(adapter.clone(), keys);
        }
        Some("thread-stream-following-status-requested") => {
            handle_following_status(adapter, conn, &frame).await;
        }
        _ => {}
    }
}

/// state-changed：来源核对（R2.4）→ 原子应用 → changed 发布 / need_snapshot 或
/// 非法增量 → stale 发布 + 有界重拉（R1）→ 不完整历史 → 只读 load-complete-history。
async fn handle_state_changed(adapter: &DesktopIpcAdapter, frame: Value) {
    let conversation_id = frame["params"]["conversationId"].as_str().unwrap_or("");
    if conversation_id.is_empty() {
        return;
    }
    let key = ThreadKey {
        host_id: "local".into(),
        thread_id: conversation_id.to_string(),
    };
    let source = frame["sourceClientId"].as_str();
    // 短锁 1：现行 lease（缺失 = 旧 owner 迟到帧，丢弃）
    let lease = {
        let owners = adapter.inner.owners.lock().await;
        owners.current(&key)
    };
    let Some(lease) = lease else { return };
    // R2.4：帧来源必须与现行 lease 一致——旧 owner 迟到帧不得污染新镜像
    if source != Some(lease.client_id.as_str()) {
        return;
    }
    let change = frame["params"]["change"].clone();
    let caps = adapter.current_caps();
    // 短锁 2：原子应用
    let mut fresh: Option<ThreadProjection> = None;
    let mut need_recover = false;
    let mut history_needed = false;
    {
        let mut mirrors = adapter.inner.mirrors.lock().await;
        let Some(mirror) = mirrors.get_mut(&key) else {
            return; // 未跟随的 thread 不建镜像、不投影
        };
        match mirror.apply(&lease, &change) {
            Ok(eff) => {
                if eff.changed {
                    fresh = Some(mirror.project(caps));
                    // 2026-10-19 复审 P2-2：触发判据 = 列表不完整 **或** items 未加载/
                    // 淘汰（itemsView）——列表完整不代表正文完整，不能等 Desktop
                    // 自己打开才补全
                    if mirror.needs_hydration() {
                        history_needed = true;
                    }
                } else if eff.need_snapshot {
                    // revision 缺口/未知 change 形态/缺 revision：stale + 重拉
                    need_recover = true;
                }
            }
            Err(_) => {
                // 非法/预算超限：apply 内已置 stale；同样触发有界重拉
                need_recover = true;
            }
        }
    }
    if need_recover {
        // 先发布 stale 投影（SSE 不能停在旧视图），再有界重拉
        publish_projection(adapter, &key).await;
        spawn_stale_refollow(adapter.clone(), key.clone(), current_conn(adapter).await);
    }
    if let Some(p) = fresh {
        // C3：状态应用驱动的提交屏障清理——pending 消失或重现换 token 即解除旧
        // Unknown 屏障（不等下一次 respond 入口），新卡立即可答
        cleanup_inflight_for(adapter, &key.thread_id, &p.pending).await;
        // #172/T9a：盖上探测只读事实（镜像锁已释放）
        adapter
            .publish_event(adapter.stamp_probe_status(p).await)
            .await;
        if history_needed {
            maybe_request_complete_history(adapter, &key).await;
        }
    }
}

/// C3：把该 thread 的 BlockedUnknown 提交屏障与当前投影对齐——(request_key, token)
/// 仍与在场卡完全一致才保留；消失/换 token 的旧屏障解除（Dispatching 不动）。
async fn cleanup_inflight_for(
    adapter: &DesktopIpcAdapter,
    thread_id: &str,
    pending: &[PendingView],
) {
    let live: std::collections::HashMap<&str, &str> = pending
        .iter()
        .map(|p| (p.request_key.as_str(), p.context_token.as_str()))
        .collect();
    let mut inflight = adapter.inner.inflight.lock().await;
    inflight.retain(|(tid, rk), entry| {
        !(tid == thread_id
            && entry.state == InflightState::BlockedUnknown
            && match live.get(rk.as_str()) {
                None => true,
                Some(cur) => Some(*cur) != entry.token.as_deref(),
            })
    });
}

/// R1：消费 need_snapshot/apply error 的有界重拉——复用 probe 定案
///（再声明 following 即得新 snapshot），最多 3 次、间隔 500ms；成功即停，
/// 3 次失败保持 stale 并停止（等下次事件/ensure 救活）。
fn spawn_stale_refollow(
    adapter: DesktopIpcAdapter,
    key: ThreadKey,
    conn: Option<ConnectionHandle>,
) {
    let Some(conn) = conn else { return };
    tokio::spawn(async move {
        for attempt in 0..3u32 {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            if adapter.inner.stopping.load(Ordering::SeqCst) || conn.is_closed() {
                return;
            }
            if is_fresh(&adapter, &key).await {
                return; // 已被并发恢复救活
            }
            if declare_following(&conn, &key).await.is_err() {
                continue;
            }
            // 等新 snapshot 落进镜像（正常路径恢复）
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
            loop {
                if is_fresh(&adapter, &key).await {
                    return;
                }
                if std::time::Instant::now() >= deadline {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }
        // 3 次失败：保持 stale 并停止（等下次事件/ensure）
    });
}

async fn is_fresh(adapter: &DesktopIpcAdapter, key: &ThreadKey) -> bool {
    let mirrors = adapter.inner.mirrors.lock().await;
    mirrors
        .get(key)
        .is_some_and(|m| !m.is_stale() && m.revision().is_some())
}

/// 发布该 thread 的当前投影（短锁读取后发广播）。
async fn publish_projection(adapter: &DesktopIpcAdapter, key: &ThreadKey) {
    let caps = adapter.current_caps();
    let projection = {
        let mirrors = adapter.inner.mirrors.lock().await;
        mirrors.get(key).map(|m| m.project(caps))
    };
    if let Some(p) = projection {
        // #172/T9a：盖上探测只读事实（镜像锁已释放）
        adapter
            .publish_event(adapter.stamp_probe_status(p).await)
            .await;
    }
}

async fn current_conn(adapter: &DesktopIpcAdapter) -> Option<ConnectionHandle> {
    let conn = adapter.inner.conn.lock().await;
    conn.as_ref().filter(|c| !c.is_closed()).cloned()
}

/// 只读 hydrate 状态（per thread）：owner 世代隔离 + 有界重试。
#[derive(Clone, Copy)]
struct Hydration {
    /// 发起时的 owner generation（换代即重置，旧世代回执一律不收敛新世代）
    generation: u64,
    attempts: u32,
    in_flight: bool,
}

/// 每 (thread, owner 世代) 的只读加载尝试上限（超过即本 follow 放弃自动加载，
/// 等待重跟随/换代重置；只读加载绝不自动重试写操作）。
const HYDRATE_MAX_ATTEMPTS: u32 = 3;
/// 重试间隔（有界退避，非风暴）。
const HYDRATE_RETRY_MS: u64 = 1_000;
/// 成功后等镜像吃到对应 snapshot 的预算（revision 核对；**超时按未收敛处理**
/// ——走有界重试，不虚称收敛；无 revision 载荷的回执才按事件流推进直接收敛）。
const HYDRATE_SETTLE_MS: u64 = 5_000;

/// R1.4 + 2026-10-18 形态适配：Turn 列表不完整（history.isComplete ≠ true）→ v1
/// **只读** load-complete-history（mutating=false，params 带 conversationId 与
/// hostId）。状态机语义：
/// * **在飞去重**：同 thread 已有在飞加载 → 本次触发直接返回；
/// * **换代隔离**：记录发起时的 owner generation——换代（follow 重绑即清）后旧
///   状态作废，旧连接的迟到回执不收敛新世代；
/// * **有界重试**：失败/超时不再「曾请求即永久锁死」——attempts < 上限时退避重试；
/// * **结果核对**：RPC Accepted 且返回目标 revision → 等镜像 revision 追平才算
///   收敛（无 revision 载荷按事件流推进处理，直接收敛）。
async fn maybe_request_complete_history(adapter: &DesktopIpcAdapter, key: &ThreadKey) {
    // 当前 lease 世代（无 lease = 不该有镜像触发，防御返回）
    let generation = match adapter.inner.owners.lock().await.current(key) {
        Some(lease) => lease.generation,
        None => return,
    };
    // 短锁：在飞去重 + 占位（占位在任务收尾统一清）
    {
        let mut hydrations = adapter.inner.history_hydrations.lock().await;
        match hydrations.get_mut(key) {
            Some(h) if h.generation == generation => {
                if h.in_flight {
                    return; // 在飞：去重
                }
                if h.attempts >= HYDRATE_MAX_ATTEMPTS {
                    return; // 本 follow 放弃（重跟随/换代后重置）
                }
                h.in_flight = true;
            }
            Some(_) => {
                // 换代残留：重置计数
                *hydrations.get_mut(key).unwrap() = Hydration {
                    generation,
                    attempts: 0,
                    in_flight: true,
                };
            }
            None => {
                hydrations.insert(
                    key.clone(),
                    Hydration {
                        generation,
                        attempts: 0,
                        in_flight: true,
                    },
                );
            }
        }
    }
    let Some(conn) = current_conn(adapter).await else {
        // 连接缺位：清在飞（下一次事件可再触发）
        // P3-1（复审）：清位须同代——旧触发不得误清新世代的在飞占位
        if let Some(h) = adapter.inner.history_hydrations.lock().await.get_mut(key) {
            if h.generation == generation {
                h.in_flight = false;
            }
        }
        return;
    };
    let Ok(version) = conn.version_of("thread-follower-load-complete-history") else {
        if let Some(h) = adapter.inner.history_hydrations.lock().await.get_mut(key) {
            if h.generation == generation {
                h.in_flight = false;
            }
        }
        return;
    };
    let c = conn.clone();
    let k = key.clone();
    let adapter = adapter.clone();
    tokio::spawn(async move {
        let mut attempt = {
            let mut hydrations = adapter.inner.history_hydrations.lock().await;
            // P2-1（复审）：占位可能在 spawn 前的 await 点被 follow 重绑清态——
            // 已被清除 = 本次加载本就作废（新世代自己会发起），直接退出不 panic
            match hydrations.get_mut(&k) {
                Some(h) if h.generation == generation => {
                    h.attempts += 1;
                    h.attempts
                }
                _ => return,
            }
        };
        loop {
            // 换代守卫：owner 已换代 → 本次加载作废（状态已随 follow 重绑清除）
            let current_generation = adapter
                .inner
                .owners
                .lock()
                .await
                .current(&k)
                .map(|l| l.generation);
            if current_generation != Some(generation) {
                break;
            }
            // 只读：mutating=false、不携带 target/validity（结果只影响后续 snapshot；
            // 调用形态与复审探针实测成功形态一致——broadcast 定位 owner）
            let outcome = c
                .request(RpcRequest {
                    method: "thread-follower-load-complete-history".into(),
                    params: json!({ "conversationId": k.thread_id, "hostId": k.host_id }),
                    version: Some(version),
                    target: None,
                    mutating: false,
                    validity: None,
                    source_client_id: None,
                    timeout_ms: None,
                })
                .await;
            let accepted = matches!(outcome.delivery, super::types::Delivery::Accepted);
            let target_rev = outcome
                .result
                .as_ref()
                .and_then(|r| r.get("revision"))
                .and_then(Value::as_u64);
            if accepted {
                // 结果核对：有目标 revision → 等镜像追平（有界）；无 → snapshot 由
                // 事件流推进，直接收敛
                let settled = match target_rev {
                    Some(target) => {
                        let deadline = tokio::time::Instant::now()
                            + std::time::Duration::from_millis(HYDRATE_SETTLE_MS);
                        loop {
                            let current = {
                                let mirrors = adapter.inner.mirrors.lock().await;
                                mirrors.get(&k).and_then(|m| m.revision())
                            };
                            if current.is_some_and(|rev| rev >= target) {
                                break true;
                            }
                            if tokio::time::Instant::now() >= deadline {
                                break false; // 目标 revision 未达成 = 未收敛（走有界重试）
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                        }
                    }
                    None => true,
                };
                if settled {
                    break;
                }
                // revision 未追平：按失败走有界重试
            }
            if attempt >= HYDRATE_MAX_ATTEMPTS {
                break; // 有界：本 follow 放弃（重跟随/换代后可再发起）
            }
            tokio::time::sleep(std::time::Duration::from_millis(HYDRATE_RETRY_MS)).await;
            // 退避后再验世代（睡眠期间可能已换代/断开）
            let still_same = adapter
                .inner
                .owners
                .lock()
                .await
                .current(&k)
                .map(|l| l.generation)
                == Some(generation);
            if !still_same {
                break;
            }
            let mut hydrations = adapter.inner.history_hydrations.lock().await;
            match hydrations.get_mut(&k) {
                Some(h) if h.generation == generation => {
                    h.attempts += 1;
                    attempt = h.attempts;
                }
                _ => break, // 已被重跟随清态：让新世代自己发起
            }
        }
        // 收口：本世代在飞标志清位（成功收敛 / 放弃都一样——放弃态由 attempts 上限
        // 挡住后续触发）
        let mut hydrations = adapter.inner.history_hydrations.lock().await;
        if let Some(h) = hydrations.get_mut(&k) {
            if h.generation == generation {
                h.in_flight = false;
            }
        }
    });
}

/// R2.1：owner 广播 disconnected → 清该 client 全部 lease；受影响 thread 的
/// mirror invalidate（视图+token 全清）并逐个发布 stale 投影。
async fn handle_client_status_changed(adapter: &DesktopIpcAdapter, frame: &Value) {
    let Some(client_id) = frame["params"]["clientId"].as_str() else {
        return;
    };
    if frame["params"]["status"].as_str() != Some("disconnected") {
        return;
    }
    // 短锁 A：候选 keys（mirror 存在的 thread）
    let candidates: Vec<ThreadKey> = {
        let mirrors = adapter.inner.mirrors.lock().await;
        mirrors.keys().cloned().collect()
    };
    // 短锁 B：owners——收集受影响 thread + invalidate_client
    let affected: Vec<ThreadKey> = {
        let mut owners = adapter.inner.owners.lock().await;
        let affected: Vec<ThreadKey> = candidates
            .into_iter()
            .filter(|k| {
                owners
                    .current(k)
                    .map(|l| l.client_id == client_id)
                    .unwrap_or(false)
            })
            .collect();
        owners.invalidate_client(client_id);
        affected
    };
    if affected.is_empty() {
        return;
    }
    // 短锁 C：mirror invalidate + 投影；顺带清该 thread 的提交互斥（owner 换代即新世界）
    let caps = adapter.current_caps();
    let mut projections = Vec::new();
    {
        let mut mirrors = adapter.inner.mirrors.lock().await;
        for key in &affected {
            if let Some(m) = mirrors.get_mut(key) {
                m.invalidate();
                projections.push(m.project(caps));
            }
        }
    }
    {
        let mut inflight = adapter.inner.inflight.lock().await;
        inflight.retain(|(tid, _), _| !affected.iter().any(|k| &k.thread_id == tid));
    }
    for p in projections {
        // #172/T9a：盖上探测只读事实（镜像锁已释放）
        adapter
            .publish_event(adapter.stamp_probe_status(p).await)
            .await;
    }
    // 2026-10-17 复审 C2：owner 失效但 socket 仍活着——连接级恢复循环不会触发；
    // 主动重新 discovery/follow 让新 owner 接管（重新接管的帧才能落进镜像），
    // 不依赖手机下次 GET
    spawn_same_socket_recovery(adapter.clone(), affected);
}
/// R2.3：owner 问「谁在跟随」——当前关注集包含该 conversation 才回
/// broadcast thread-stream-following-changed {following:true}（version 按表）。
async fn handle_following_status(
    adapter: &DesktopIpcAdapter,
    conn: &ConnectionHandle,
    frame: &Value,
) {
    let Some(conversation_id) = frame["params"]["conversationId"].as_str() else {
        return;
    };
    let key = ThreadKey {
        host_id: "local".into(),
        thread_id: conversation_id.to_string(),
    };
    let following = {
        let f = adapter.inner.followings.lock().await;
        f.is_following(&key)
    };
    if !following {
        return;
    }
    let Ok(version) = conn.version_of("thread-stream-following-changed") else {
        return;
    };
    let _ = conn
        .broadcast(
            "thread-stream-following-changed",
            version,
            json!({
                "conversationId": conversation_id,
                "hostId": "local",
                "following": true,
            }),
        )
        .await;
}

/// 全部 mirror stale + 发布。`invalidate=true`（owner 失效）清视图与 token；
/// `false`（断线/reset）保留最后视图只读。
async fn mark_all_mirrors_stale(adapter: &DesktopIpcAdapter, invalidate: bool) {
    let caps = adapter.current_caps();
    let mut projections = Vec::new();
    {
        let mut mirrors = adapter.inner.mirrors.lock().await;
        for mirror in mirrors.values_mut() {
            let publish = if invalidate {
                mirror.invalidate();
                true
            } else if !mirror.is_stale() {
                mirror.mark_stale();
                true
            } else {
                false
            };
            if publish {
                projections.push(mirror.project(caps));
            }
        }
    }
    for p in projections {
        // #172/T9a：盖上探测只读事实（镜像锁已释放）
        adapter
            .publish_event(adapter.stamp_probe_status(p).await)
            .await;
    }
}

/// 关注集中的 thread（mirror 键 ∩ 关注集；两锁顺序读取，不嵌套）。
async fn followed_keys(adapter: &DesktopIpcAdapter) -> Vec<ThreadKey> {
    let mirror_keys: Vec<ThreadKey> = {
        let mirrors = adapter.inner.mirrors.lock().await;
        mirrors.keys().cloned().collect()
    };
    let followings = adapter.inner.followings.lock().await;
    mirror_keys
        .into_iter()
        .filter(|k| followings.is_following(k))
        .collect()
}

#[cfg(test)]
mod recovery_queue_tests {
    //! 第四轮复审 B2：RecoveryQueue 交接契约——「取空待办」与「释放消费者资格」
    //! 原子性（同一临界区）与多生产者零丢失。

    use super::*;

    fn key(id: &str) -> ThreadKey {
        ThreadKey {
            host_id: "local".into(),
            thread_id: id.into(),
        }
    }

    /// B2 契约：take 空 = 资格原子释放；随后 enqueue 必然获得新消费者资格，
    /// `pending=[B], running=false`（无消费者）的状态不可能出现。
    #[test]
    fn take_empty_releases_running_atomically_and_later_enqueue_gets_consumer() {
        let mut q = RecoveryQueue::default();
        assert!(q.enqueue([key("A")]), "首个入队者获得消费者资格");
        assert_eq!(q.take_or_release(), Some(vec![key("A")]));
        // 任务处理完（批为空）再 take：空 ⇒ 原子释放资格
        assert_eq!(q.take_or_release(), None);
        assert!(!q.running, "资格已随 take 空原子释放");
        // 退出边界瞬间入队 B：必然拿到资格（新消费者接管），不出现无消费者状态
        assert!(q.enqueue([key("B")]), "资格释放后的入队必须获得新消费者");
        assert_eq!(q.take_or_release(), Some(vec![key("B")]));
        assert_eq!(q.take_or_release(), None);
        assert!(!q.running);
    }

    /// B2 契约：在飞期间入队不获资格（单飞），但 keys 必在待办中等待消费
    ///（任务已把首批 take 走，后到的 B 单独等下一轮）。
    #[test]
    fn enqueue_while_running_waits_in_pending() {
        let mut q = RecoveryQueue::default();
        assert!(q.enqueue([key("A")]));
        assert_eq!(q.take_or_release(), Some(vec![key("A")]), "首批被任务取走");
        assert!(!q.enqueue([key("B")]), "在飞期间入队不获资格");
        assert_eq!(q.pending.len(), 1, "B 必在待办等待消费");
        assert_eq!(q.take_or_release(), Some(vec![key("B")]));
    }

    /// B2 契约：中途退出（连接断/停机）= keys 放回 + 资格释放（同临界区）。
    #[test]
    fn requeue_and_release_keeps_keys_and_frees_consumer() {
        let mut q = RecoveryQueue::default();
        assert!(q.enqueue([key("A")]));
        q.requeue_and_release([key("A"), key("B")]);
        assert!(!q.running);
        assert_eq!(q.pending.len(), 3, "原 A（未消费）+ 放回的 A/B 都在待办");
        assert!(q.enqueue([key("C")]), "资格已释放：后来者接管");
        let taken = q.take_or_release().expect("keys");
        assert_eq!(
            taken.len(),
            4,
            "放回的 keys（原A+放回A/B）与新入队 C 都被一并消费"
        );
    }

    /// B2 压力：多生产者并发入队、单消费者循环 take_or_release——零丢失、零重复、
    /// 终态 running=false 且 pending 空（真实线程交错打在交接窗口上）。
    #[test]
    fn concurrent_producers_single_consumer_lose_nothing() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::Arc;
        const PRODUCERS: usize = 4;
        const PER_PRODUCER: usize = 500;
        let queue = Arc::new(std::sync::Mutex::new(RecoveryQueue::default()));
        let consumed = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let done = Arc::new(AtomicBool::new(false));
        let produced_total = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for p in 0..PRODUCERS {
            let queue = queue.clone();
            let consumed = consumed.clone();
            let produced_total = produced_total.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..PER_PRODUCER {
                    let k = key(&format!("p{p}-i{i}"));
                    let start = {
                        let mut q = queue.lock().unwrap();
                        q.enqueue([k])
                    };
                    produced_total.fetch_add(1, Ordering::SeqCst);
                    if start {
                        // 本线程获得消费者资格：驱动消费循环至 take 空（原子释放）
                        loop {
                            let consumed_now = {
                                let mut q = queue.lock().unwrap();
                                q.take_or_release()
                            };
                            match consumed_now {
                                Some(keys) => {
                                    let mut c = consumed.lock().unwrap();
                                    c.extend(keys.into_iter().map(|k| k.thread_id));
                                }
                                None => break, // 资格已原子释放；由后续 enqueue 接管
                            }
                        }
                    }
                }
            }));
        }
        // 独立常驻消费者（任意时刻拿到资格就消费）
        let consumer_queue = queue.clone();
        let consumer_consumed = consumed.clone();
        let consumer_done = done.clone();
        let consumer = std::thread::spawn(move || loop {
            if consumer_done.load(Ordering::SeqCst) {
                break;
            }
            let consumed_now = {
                let mut q = consumer_queue.lock().unwrap();
                q.take_or_release()
            };
            match consumed_now {
                Some(keys) => {
                    let mut c = consumer_consumed.lock().unwrap();
                    c.extend(keys.into_iter().map(|k| k.thread_id));
                }
                None => std::thread::sleep(std::time::Duration::from_micros(50)),
            }
        });
        for h in handles {
            h.join().unwrap();
        }
        // 全部生产完成后等消费者排空，再停
        loop {
            let consumed_now = {
                let mut q = queue.lock().unwrap();
                q.take_or_release()
            };
            match consumed_now {
                Some(keys) => {
                    let mut c = consumed.lock().unwrap();
                    c.extend(keys.into_iter().map(|k| k.thread_id));
                }
                None => break,
            }
        }
        done.store(true, Ordering::SeqCst);
        consumer.join().unwrap();

        let produced = produced_total.load(Ordering::SeqCst);
        let mut c = consumed.lock().unwrap();
        assert_eq!(c.len(), produced, "零丢失（含资格交接窗口）");
        c.sort();
        c.dedup();
        assert_eq!(c.len(), produced, "零重复消费");
        let q = queue.lock().unwrap();
        assert!(q.pending.is_empty() && !q.running, "终态无残留无消费者");
    }
}

#[cfg(test)]
mod dispatch_tests {
    //! T7-R3：派发版本号**只从方法表解析**——方法不在表里 = 明确方法缺失 →
    //! `NotSent`（证明不再像旧实现那样在组装侧硬编码 `version = 1` 放行）。
    //! 方法表新增项（file 决策方法）的取证见 profile.rs 单测与 observed/README.md。

    use super::*;
    use crate::services::codex_desktop_ipc::connection;

    type ConnectFuture<'a> = Pin<
        Box<
            dyn std::future::Future<Output = Result<super::super::transport::BoxIo, ProtocolError>>
                + Send
                + 'a,
        >,
    >;

    /// 本模块不建真实连接：dispatch 在版本解析阶段即返回，连接只作为 `Some` 存在。
    struct NoopConnector;

    impl Connector for NoopConnector {
        fn connect(&self) -> ConnectFuture<'_> {
            Box::pin(async { Err(ProtocolError::Disconnected("test connector".into())) })
        }
    }

    #[tokio::test]
    async fn schema_evidence_is_bound_to_identity_and_reprobed() {
        let adapter = DesktopIpcAdapter::new(
            Arc::new(NoopConnector),
            ProtocolProfile::unverified("macos", "unknown-a"),
        );
        adapter.set_probe_key_override(Some("macos|unknown-a|cli-a|1".into()));
        adapter.set_schema_probe_override(Some(SchemaProbeOutcome::Known(SchemaEvidence {
            fingerprint: "schema-a".into(),
            command_decisions: vec!["accept".into(), "cancel".into()],
            file_decisions: vec![],
        })));
        adapter.ensure_schema_evidence().await;
        assert!(!adapter.approval_policy().is_empty());
        adapter.inner.epoch.fetch_add(1, Ordering::SeqCst);
        *adapter.inner.probe_target.lock().unwrap() = Some(ProbeTarget {
            epoch: 1,
            platform: "macos".into(),
            build: "unknown-b".into(),
            key: None,
        });
        adapter.set_probe_key_override(Some("macos|unknown-b|cli-b|1".into()));
        assert!(adapter.approval_policy().is_empty(), "旧证据不得跨身份复用");
        adapter.ensure_schema_evidence().await;
        assert!(!adapter.approval_policy().is_empty(), "新身份应重新取证");
    }

    #[tokio::test]
    async fn old_fingerprint_cannot_fill_reconnected_target() {
        let adapter = DesktopIpcAdapter::new(
            Arc::new(NoopConnector),
            ProtocolProfile::unverified("macos", "same-build"),
        );
        let old_target = adapter.probe_target_snapshot().unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let old_adapter = adapter.clone();
        let old = tokio::spawn(async move {
            old_adapter
                .resolve_probe_key_with(&old_target, async {
                    started_tx.send(()).unwrap();
                    finish_rx.await.unwrap();
                    "cli-a".to_string()
                })
                .await
        });
        started_rx.await.unwrap();
        // build 不变，连接与 CLI 已换代；先清空新 target，再放行旧计算。
        adapter.inner.epoch.fetch_add(1, Ordering::SeqCst);
        *adapter.inner.probe_target.lock().unwrap() = Some(ProbeTarget {
            epoch: 1,
            platform: "macos".into(),
            build: "same-build".into(),
            key: None,
        });
        finish_tx.send(()).unwrap();
        assert_eq!(old.await.unwrap(), "cli-a");
        assert_eq!(
            adapter.probe_target_snapshot().unwrap().key,
            None,
            "旧 CLI 指纹不得污染新连接的缓存"
        );
        let new_target = adapter.probe_target_snapshot().unwrap();
        adapter
            .resolve_probe_key_with(&new_target, async { "cli-b".to_string() })
            .await;
        assert_eq!(
            adapter.probe_target_snapshot().unwrap().key.as_deref(),
            Some("cli-b")
        );
    }

    #[derive(Default)]
    struct ProbedLedger {
        records: AtomicU64,
    }
    impl ProbeLedger for ProbedLedger {
        fn get(&self, key: &str, _: &str) -> Option<super::super::probe::ProbeLedgerEntry> {
            (key == "cli-a").then(|| super::super::probe::ProbeLedgerEntry {
                state: crate::database::dao::codex_ipc_probe::ProbeState::Probed,
                next_retry_at: None,
                stage: None,
                error_code: None,
                fingerprint: None,
            })
        }
        fn record(&self, key: &str, _: &str, _: &ProbeOutcome, _: Option<&str>) {
            assert_eq!(key, "cli-b", "旧结果不得落账");
            self.records.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn old_read_key_cannot_unlock_reconnected_target() {
        let adapter = DesktopIpcAdapter::new(
            Arc::new(NoopConnector),
            ProtocolProfile::unverified("macos", "same-build"),
        );
        adapter.set_probe_ledger(Arc::new(ProbedLedger::default()));
        let old_target = adapter.probe_target_snapshot().unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let a = adapter.clone();
        let old = tokio::spawn(async move {
            let key = a
                .resolve_probe_key_with(&old_target, async {
                    started_tx.send(()).unwrap();
                    finish_rx.await.unwrap();
                    "cli-a".to_string()
                })
                .await;
            a.reuse_ledger_evidence(&old_target, &key)
        });
        started_rx.await.unwrap();
        {
            let mut slot = adapter.inner.probe_target.lock().unwrap();
            adapter.inner.epoch.fetch_add(1, Ordering::SeqCst);
            *slot = Some(ProbeTarget {
                epoch: 1,
                platform: "macos".into(),
                build: "same-build".into(),
                key: None,
            });
            adapter.set_profile(ProtocolProfile::unverified("macos", "same-build"));
        }
        finish_tx.send(()).unwrap();
        old.await.unwrap();
        assert!(!adapter.capabilities().state, "A 的 probed 账本不得解锁 B");
    }

    #[tokio::test]
    async fn old_read_result_cannot_record_or_unlock_reconnected_target() {
        let adapter = DesktopIpcAdapter::new(
            Arc::new(NoopConnector),
            ProtocolProfile::unverified("macos", "same-build"),
        );
        let ledger = Arc::new(ProbedLedger::default());
        adapter.set_probe_ledger(ledger.clone());
        let old_target = adapter.probe_target_snapshot().unwrap();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let a = adapter.clone();
        let old = tokio::spawn(async move {
            let outcome = finish_rx.await.unwrap();
            a.apply_read_probe_result(&old_target, "cli-a", &outcome)
        });
        {
            let mut slot = adapter.inner.probe_target.lock().unwrap();
            adapter.inner.epoch.fetch_add(1, Ordering::SeqCst);
            *slot = Some(ProbeTarget {
                epoch: 1,
                platform: "macos".into(),
                build: "same-build".into(),
                key: None,
            });
            adapter.set_profile(ProtocolProfile::unverified("macos", "same-build"));
        }
        let outcome = ProbeOutcome::Probed {
            stage: ProbeStage::P3,
            revision: Some(1),
            duration_ms: 0,
        };
        finish_tx.send(outcome.clone()).unwrap();
        assert!(!old.await.unwrap());
        assert_eq!(ledger.records.load(Ordering::SeqCst), 0);
        assert!(!adapter.capabilities().state, "A 的探测成功不得解锁 B");
        let current = adapter.probe_target_snapshot().unwrap();
        assert!(adapter.apply_read_probe_result(&current, "cli-b", &outcome));
        assert_eq!(ledger.records.load(Ordering::SeqCst), 1);
        assert!(adapter.capabilities().state, "B 自己的证据应能解锁 B");
    }

    fn lease() -> OwnerLease {
        OwnerLease {
            key: ThreadKey {
                host_id: "local".into(),
                thread_id: "t-1".into(),
            },
            client_id: "owner-1".into(),
            epoch: 1,
            generation: 1,
        }
    }

    /// 回归：未进表方法即便拿到一条活连接也只能 `NotSent`，reason 为方法缺失码
    ///（旧硬编码路径会以 v1 发出 → 这里会变成非 NotSent，用例即变红）。
    #[tokio::test]
    async fn dispatch_rejects_method_missing_from_table() {
        let (io, _peer) = tokio::io::duplex(64 * 1024);
        let conn = connection::spawn(Box::new(io), ProtocolProfile::closed(), 1);
        let adapter = DesktopIpcAdapter::new(Arc::new(NoopConnector), ProtocolProfile::closed());
        *adapter.inner.conn.lock().await = Some(conn.clone());
        let (_tx, validity) = tokio::sync::watch::channel(true);
        //「形似官方方法但未进表」：正是硬编码时代会被放行的名字
        let outcome = adapter
            .dispatch(
                "thread-follower-interrupt-turn",
                json!({}),
                lease(),
                validity,
            )
            .await;
        assert_eq!(outcome.delivery, Delivery::NotSent, "未进表方法必须零投递");
        assert_eq!(
            outcome.reason_code.as_deref(),
            Some("unsupported_method"),
            "reason = 方法缺失码（不是 disconnected / 放行）"
        );
        assert!(outcome.result.is_none());
        conn.shutdown().await;
    }

    /// 方法名集中登记：command / file 各用各的决策方法，且两条都必须在方法表里
    /// （版本唯一来源）；非决策类不伪造决策方法。
    #[test]
    fn decision_methods_resolve_from_method_table() {
        let profile = ProtocolProfile::closed();
        for kind in [
            PendingKind::CommandApproval,
            PendingKind::FileChangeApproval,
        ] {
            let method = decision_method(&kind).expect("审批类必须有决策方法");
            assert_eq!(
                profile.version(method, None),
                Ok(1),
                "{method} 必须登记在方法表且为 v1"
            );
        }
        assert_eq!(
            decision_method(&PendingKind::CommandApproval),
            Some(METHOD_COMMAND_DECISION)
        );
        assert_eq!(
            decision_method(&PendingKind::FileChangeApproval),
            Some(METHOD_FILE_DECISION)
        );
        assert_ne!(
            METHOD_COMMAND_DECISION, METHOD_FILE_DECISION,
            "command / file 决策方法互不替代"
        );
        // 非决策类没有「决策词」方法
        assert_eq!(decision_method(&PendingKind::PermissionsApproval), None);
        assert_eq!(decision_method(&PendingKind::UserInput), None);
        assert_eq!(decision_method(&PendingKind::Unsupported), None);
        // 未登记方法 → UnsupportedMethod（单一真相源明确拒绝，不猜版本）
        assert_eq!(
            profile.version("thread-follower-interrupt-turn", None),
            Err(ProtocolError::UnsupportedMethod(
                "thread-follower-interrupt-turn".into()
            ))
        );
    }
}
