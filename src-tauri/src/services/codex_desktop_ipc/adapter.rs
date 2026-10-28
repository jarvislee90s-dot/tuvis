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

use super::capability::{
    CodexOperation, ContractEvidenceStore, ContractTarget, DesktopContractEvidence,
    DesktopContractReader, EffectiveOperations, ObservedOperationsSource, OperationContext,
    OperationPolicy, ProductionContractReader, CONTRACT_RULE_VERSION,
};
use super::connection::{ConnectionEvent, ConnectionHandle, RpcOutcome, RpcRequest};
use super::mirror::ThreadMirror;
use super::owners::{
    bind_lease, declare_following, discover_owner, wait_first_snapshot, FollowingSet,
};
use super::pending::{parse_request_key, DECISION_ACCEPT, DECISION_CANCEL, DECISION_DECLINE};
use super::probe::runner::{ProbeOutcome, ProbeRunner, ProbeStage};
use super::probe::schema_probe::{SchemaEvidence, SchemaProbeOutcome};
use super::probe::{ProbeLedger, CAP_SCHEMA, CAP_SEND, CAP_STATE};
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

/// 终审 F4：写边界守卫闭包形状（action_id + 是否依赖兼容授权 → 判定）。
pub type WriteGuardFn =
    Arc<dyn Fn(&str, bool) -> crate::services::codex_control::WriteGuardVerdict + Send + Sync>;

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

/// S1：官方 schema 证据槽（连接代际, 绑定目标, 决策词证据, 取证时间）。
type SchemaEvidenceSlot = (u64, Option<ProbeTarget>, SchemaEvidence, i64);

/// T02：逐操作契约证据槽（连接代际, 绑定目标, 证据）。
type ContractEvidenceSlot = (u64, Option<ProbeTarget>, DesktopContractEvidence);

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

/// H02：设备有效性只读源（生产 = 配对表；interrupt 写边界许可使用）。
/// N01：当前包内容身份——与**目标代际**绑定；换代后旧 SHA 不得再代表当前包。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageIdentity {
    pub epoch: u64,
    pub probe_key: String,
    pub sha256: String,
}

pub type DeviceValidSource = std::sync::Arc<dyn Fn(&str) -> bool + Send + Sync>;

struct AdapterInner {
    connector: Arc<dyn Connector>,
    /// 当前连接的 profile（R3：动态）。初始为构造入参（首个连接前的基线）；
    /// 每次连接按 endpoint build 重算（登记 build → 证据档；未知 build → 兼容
    /// 发送档；空 build/无 endpoint → `closed()`）。
    /// 同步短临界区读写，绝不跨 await 持有、不与其他锁嵌套。
    profile: std::sync::Mutex<ProtocolProfile>,
    /// S2 复核后正名（原 `build_send_verified`）：**当前连接档位的 send 能力位快照**。
    /// `None` = 本进程内还没成功建连过；`Some(bit)` = 最近一次成功 resolve 的档位的
    /// `capabilities.send`（shipped 证据档 = 真证据；未知 build 的兼容档 = 「可尝试」
    /// 档位，**不等于已取证**）。
    ///
    /// 为什么要与 [`profile`] 分开记：连接失败会把 profile 回落 `closed()`（R3 的
    /// 「Desktop 缺席」诚实缺省），此时能力位不再回答「这台 build 的档位带不带写」。
    /// 旧注释「兼容档 send 开 = 按『已验证』记 true（headless 兜底口径不变）」的
    /// 依托——Desktop 自动 headless 兜底臂——已随 S2 删除（设计 §1.4/§4.1），本标志
    /// 不再表达「已验证」；现消费者只有两个：①「未知 build 可尝试发送」判据
    /// （`send_gate_static` / 派发侧 compat 门，`!snapshot` = 未取证可尝试）；
    /// ②判死档下区分「已取证 build 瞬时断链」与「从未取证」（前者给
    /// `capability_unavailable`，后者 `capability_unverified`）。
    profile_send_capable: std::sync::Mutex<Option<bool>>,
    /// Gate A #2：**最近一次成功建连（或装配期 resolve）的证据档**。与
    /// [`profile_send_capable`] 同点写入、同样不看连接存活——断链把 profile 回落
    /// `closed()` 是「Desktop 缺席」的诚实投影，但**「已取证」是持久事实**，DTO 的
    /// shipped 判定读这里而不是当前 profile（否则 shipped build 的
    /// `evidenced.{state,send}` 随链路抖动翻转，违反 S1 登记契约）。
    last_resolved_evidence: std::sync::Mutex<Option<super::profile::EvidenceLevel>>,
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
    /// 终审 R2：写右判定（生产 = remote 层注入；每次新连接 spawn 后转发给连接）。
    write_right_check:
        std::sync::Mutex<Option<crate::services::codex_control::WriteGuardVerdictChecker>>,
    /// 复审 R-T1：取得首次写出权的 claim（生产 = remote 层注入的两态 CAS；
    /// 每次新连接 spawn 后转发给连接——最终判定之后、write_all 之前执行）。
    write_right_claim: std::sync::Mutex<Option<crate::services::codex_control::WriteRightClaim>>,
    /// 复审 R-T4：队列暂停事实源（生产 = remote 层注入：读 `codex_queue_pause`
    /// 是否有任意行；读失败按 true 保守——与发送门同口径）。`None` = false
    ///（未装配，如纯 adapter 单测）。镜像层不知道账本，投影由 adapter 盖章。
    /// 锁纪律：std::Mutex 短临界区取 Arc 后即释锁再调用（闭包内自取 DB 锁，
    /// 与 write_right_check 同款；盖章点均在镜像锁之外）。
    queue_pause_source: std::sync::Mutex<Option<QueuePauseSource>>,
    /// 复审 round 4（V1）：`republish_thread_projection` 已发布过的 queuePaused 值
    ///（按 ThreadKey）。**按实际状态变化发布**——暂停事实不变时（调度循环消费自身
    /// 通知 / 周期对账反复结算），重发布被抑制：旧实现每次 settle 只要读到暂停还在
    /// 就无条件重发布，`publish_event` 每次分配新 local_seq，scheduler 收到自己的
    /// 投影再次结算再发布——无外部输入的调度自激循环（CPU/DB/SSE 持续空转，去重
    /// 挡不住递增序号）。std::Mutex 短临界区（不跨 await），与 queue_pause_source
    /// 同款锁纪律。复审 round 6（Q1-W1 收口）：该锁同时是「去重判定 + CAS 预留 +
    /// 登记」原子段（`pause_publish_grant`）的临界区——tracker 与发布资格的取得
    /// 同步可见，并发去重不再漏看待提交值。复审 round 7（Q1）：判定基准改为临界区
    /// 内重读的暂停事实（发布头/先前观察都不证明事实未变），登记与广播同用重读值。
    pause_published: std::sync::Mutex<HashMap<ThreadKey, bool>>,
    /// 第四轮复审 B3 测试缝：follow 的 bind 与 declare 之间的可注入等待
    /// （生产恒 None → 零开销直通）；回归固定「bind/declare 间卸载」交错。
    follow_gate: std::sync::Mutex<Option<Arc<tokio::sync::Notify>>>,
    /// B3 测试缝探针：任务已到达 follow gate。
    follow_gate_reached: AtomicBool,
    /// 终审 F4 测试缝：写边界守卫调用**前**的可注入等待（生产恒 None 直通）；
    /// 回归固定「守卫判定前插入人工继续」的交错。
    write_guard_gate: std::sync::Mutex<Option<Arc<tokio::sync::Notify>>>,
    /// 终审 F4 测试缝探针：任务已到达 write guard（供测试轮询替代固定睡眠）。
    write_guard_reached: AtomicBool,
    /// #172/T7：官方 schema 证据（未知 build 的审批决策词来源；`None` = 尚未取证）。
    /// 有它就能在「无真机审批证据」的 build 上安全开放 `accept` + `cancel`。
    /// S1：四元组含 `evidenced_at`（取证时间进 DTO；持久副本见探测账本的
    /// `codex_ipc_schema_evidence` 表——重启由 [`Self::ensure_schema_evidence`] 先装回）。
    schema_evidence: std::sync::Mutex<Option<SchemaEvidenceSlot>>,
    /// #172/T7-R4：schema 探针对完整指纹与连接代际**各跑一次**的单飞标志（结论为 `Unknown` 时也不重跑：
    /// 探针失败即「未知」，重跑只会再拿同一个 `Unknown`；账本行已记 `inconclusive`
    /// + 退避窗口，跨重启的复用由账本负责）。
    schema_probe_ran: tokio::sync::Mutex<Option<(u64, String)>>,
    /// #172/T7-R4 测试缝：注入 schema 探针结论（生产恒 `None` → 真跑 CLI；
    /// 测试注入后**绝不 spawn 真实 codex.exe**）。
    schema_probe_override: std::sync::Mutex<Option<SchemaProbeOutcome>>,
    /// 终审 F2 测试缝：身份最终检查**通过后、安装前**被调用的同步闭包（生产恒
    /// `None` 直通）。用于定序回归——在此窗口强制换代/换指纹，验证安装是否仍受
    /// 取证时捕获的身份约束（旧代际结论不得落新身份）。
    schema_install_gate: std::sync::Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    /// 复审 round 3（T3）测试缝 ×3：取快照定序窗（生产恒 `None` 直通）。
    ///
    /// * `projection_presnapshot_gate`：`projection()` 镜像锁临界区**之前**——
    ///   旧代码在此前已锁外读 caps/policy；窗口内「设置/证据翻转 + 真实重发布」
    ///   可整体越过本任务（重发布会推进投影序号），随后本任务以旧内容进临界区
    ///   盖不晚于该事件的序号——内容与序号不一致，真新策略可被吞。
    /// * `projection_sealed_gate`：临界区**之后、返回前**——旧代码在此后还有
    ///   「锁外用最新序号无条件重盖章」（复审 T3 主缺陷）。
    /// * `fresh_snapshot_gate`：`fresh_projection` 旧代码的 caps 锁外先读窗口
    ///   （读后、镜像锁前）。
    ///
    /// 三缝均为 `Arc<dyn Fn>`：runner **先取出、释放缝锁、后调用**——缝回调里若
    /// 再入任何取快照路径（如触发重发布），不得被缝自身的锁挡住（Box+持锁调用
    /// 会自锁死）。
    projection_presnapshot_gate: std::sync::Mutex<Option<std::sync::Arc<dyn Fn() + Send + Sync>>>,
    projection_sealed_gate: std::sync::Mutex<Option<std::sync::Arc<dyn Fn() + Send + Sync>>>,
    fresh_snapshot_gate: std::sync::Mutex<Option<std::sync::Arc<dyn Fn() + Send + Sync>>>,
    /// 复审 round 4（V3）测试缝，round 5（W1）后置：重发布路径「检查（refresh）
    /// 已通过、最终发布序号尚未取得」的定序窗（生产恒 `None` 直通）。T3 三缝只停
    /// **取快照**；本缝停**发布**——窗内完成「设置关闭 + 新禁用策略发布」后放行，
    /// 验证迟到旧 payload 不得靠发布点重新分配的更大序号覆盖新状态。`Arc<dyn Fn>`
    /// 同款纪律：runner 先取出、释放缝锁、后调用（缝回调内再入重发布不得被缝自身
    /// 锁挡死）。
    republish_publish_gate: std::sync::Mutex<Option<std::sync::Arc<dyn Fn() + Send + Sync>>>,
    /// 复审 round 5（W1/V3 收口）测试缝：普通 state-changed 发布「镜像锁已释放、
    /// `cleanup_inflight_for().await` 等待完成、发布序号尚未取得」的定序窗（生产
    /// 恒 `None` 直通）。复审原文指认的生产异步路径：fresh payload 已生成 → 释放
    /// 镜像锁 → 等待窗内「关闭设置 + 新禁用策略发布」整体完成 → 旧 payload 照旧
    /// 发布取得更大序号。`Arc<dyn Fn>` 同款纪律（runner 先取锁、后调用）。
    state_publish_gate: std::sync::Mutex<Option<std::sync::Arc<dyn Fn() + Send + Sync>>>,
    /// 复审 round 6（Q1）测试缝：四条直接发布路径（state-changed / publish_
    /// projection / client-status-changed / mark-all-stale）「caps 已锁外读取、
    /// 尚未取得镜像锁」的观察边界窗（生产恒 `None` 直通）。参数 = 路径标签，
    /// 测试按标签只停被测路径。窗内「关闭设置 + 新禁用策略发布」整体完成后放行：
    /// 旧代码以锁外旧 caps 生成的 payload 照旧盖章/预留发布，availability 回翻；
    /// 修复后 caps 与盖章同一临界区，放行后读取即新禁用状态。
    caps_observe_gate: std::sync::Mutex<Option<CapsObserveGate>>,
    /// 复审 round 6（Q1-W1 收口）测试缝：暂停 tracker「发布资格已预留（CAS 成功）、
    /// tracker 尚未登记」的并发窗（生产恒 `None` 直通）。窗内插一条真实 resume：
    /// 旧代码去重读到旧 tracker 值误抑制恢复通知，旧暂停值随后照旧提交广播，真实
    /// 暂停反被去重吞掉；修复后预留与登记同一临界区，窗内 resume 读到待发布值照常
    /// 发布恢复事实，后续第三次真实变化仍可通知。
    pause_reserve_gate: std::sync::Mutex<Option<std::sync::Arc<dyn Fn() + Send + Sync>>>,
    /// 复审 round 7（Q1）测试缝：暂停重发布「投影观察与 refresh 全部完成、去重
    /// 判定 grant 尚未进入」的定序窗（生产恒 None 直通）。窗内可整体完成一次同值
    /// 恢复（真实 resume 清行 + 其 republish 收敛），验证放行后的 grant 不以缝前
    /// 旧观察提交暂停事实。`Arc<dyn Fn>` 同款纪律：runner 先取出、释放缝锁、后调用。
    pause_observe_gate: std::sync::Mutex<Option<std::sync::Arc<dyn Fn() + Send + Sync>>>,
    /// #172/T9a：探测只读事实的**缓存**（账本 `state` 行的结论）。
    ///
    /// 为什么要缓存：投影在每次状态变化时都会重建（多处发布点），每次都查账本
    /// 会把 SQLite 读放大成热路径负担。账本 `state` 行的**唯一写者**是本 adapter
    /// 的 [`DesktopIpcAdapter::ensure_read_evidence`]（remote 层只写 `send` 行），
    /// 故在写点与 build 目标变更点失效即可保持准确。
    probe_status_cache: std::sync::Mutex<Option<ProbeStatus>>,
    /// S1：**自动适配门**（新设置「自动适配」的生效值来源；生产 = remote 层注入的
    /// settings 读取闭包，缺省开）。`None` = 未注入 → 恒开（适配器单测不依赖 DB）。
    /// 契约：只读查询、必须快（投影热路径会调）、**活值**（开关翻转即刻生效）。
    auto_adapt_gate: std::sync::Mutex<Option<Arc<dyn Fn() -> bool + Send + Sync>>>,
    /// 终审 F4：**写边界守卫**（mutation 专用）——生产 = remote 层注入（撤销标记
    /// 读取 + 兼容许可活值重读）；`None` = 恒放行（适配器单测不依赖 DB）。
    /// 契约：只读、必须快、在 dispatch 进入连接 writer 队列**之前**调用（该点与
    /// 写帧之间只剩 conn 锁与 mpsc 入队——已是可达的最晚判定点）。
    /// 第二参 = 本次写出**依赖兼容授权**（compat 尝试 ∨ 档位 send 能力来自兼容档
    /// 而非已验证证据）——活门收回时这两类写出都必须拒绝。
    write_guard: std::sync::Mutex<Option<WriteGuardFn>>,
    /// T02：**契约证据槽**（连接代际, 绑定目标, 证据）——只由
    /// [`Self::install_contract_evidence`] 在身份守卫下写入。
    contract_evidence: std::sync::Mutex<Option<ContractEvidenceSlot>>,
    /// T02：契约证据的**读取器**（生产 = 真实包只读解析；测试注入受控 reader，
    /// 绝不扫描/连接本机 App）。
    contract_reader: std::sync::Mutex<Option<Arc<dyn DesktopContractReader>>>,
    /// T02：契约证据的持久存储（生产 = DB DAO；测试注入内存实现）。
    contract_store: std::sync::Mutex<Option<Arc<dyn ContractEvidenceStore>>>,
    /// T02：**契约识别单飞**（`tokio::sync::Mutex`：任务取消即释放，等待者随后
    /// 重试；不在取消后留下"已识别"假记录）。
    contract_inflight: tokio::sync::Mutex<()>,
    /// T02：本代际 + 指纹已尝试识别的标记（失败/成功都记；成功由证据槽表达，
    /// 失败由 [`Self::contract_failure`] 表达）——避免每次投影热路径重读安装包。
    contract_attempted: std::sync::Mutex<Option<(u64, String)>>,
    /// T02：最近一次契约识别失败（阶段 + 错误码；与成功证据分离，暂时失败不抹成功）。
    contract_failure: std::sync::Mutex<Option<(u64, String, String)>>,
    /// T02：契约安装定序缝（测试用：读取返回后、身份守卫安装前；生产恒 None）。
    contract_install_gate: std::sync::Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    /// T02：目标解析测试缝（生产恒 None → 走官方运行包定位）。
    contract_target_override: std::sync::Mutex<Option<ContractTarget>>,
    /// T02：本机真实成功证据来源（生产由 remote 层注入；缺省 = 无成功证据）。
    observed_operations: std::sync::Mutex<Option<Arc<dyn ObservedOperationsSource>>>,
    /// T03：异步题持久提示的存储（生产 = DB DAO；缺省 = 无提示）。
    interaction_hint_store:
        std::sync::Mutex<Option<Arc<dyn super::async_question::InteractionHintStore>>>,
    /// R01：**交互资格事实快照**（投影临界区内同步写入；claim 在同一把资格闸下
    /// 读取）。只含判定所需的标量/键，不含正文。
    interaction_facts: std::sync::Mutex<HashMap<ThreadKey, InteractionFacts>>,
    /// J01：账本动作的完整组绑定（action_id → 来源/成员）。
    group_bindings: std::sync::Mutex<HashMap<String, GroupBinding>>,
    /// H02：设备有效性只读源（生产 = 配对表；interrupt 写边界许可使用）。
    device_valid_source: std::sync::Mutex<Option<DeviceValidSource>>,
    /// H01：当前已识别包的**内容 SHA**（成功证据的完整身份之一）。
    package_sha: std::sync::Mutex<Option<PackageIdentity>>,
    /// H01 测试缝：覆盖包 SHA（生产恒 None → 取已识别证据的 SHA）。
    package_sha_override: std::sync::Mutex<Option<String>>,
    /// H02 测试缝：写边界资格复核的会合点（生产恒 None——闭包不阻塞）。
    recheck_barrier: std::sync::Mutex<Option<Arc<std::sync::Barrier>>>,
    recheck_reached: std::sync::atomic::AtomicBool,
    /// R01：资格闸——投影写事实与首次写出 claim 共用（std 短临界区，闭包内不出让
    /// 执行权、无 await）：先失效先胜出，claim 之后才允许写出。
    interaction_gate: std::sync::Mutex<()>,
    /// T03：已装载提示缓存（thread_id → 提示；镜像锁内只读它，不读 DB）。
    interaction_hints_cache:
        std::sync::Mutex<HashMap<String, Arc<Vec<super::async_question::InteractionHint>>>>,
    /// S1：本进程内已确立的**读链成功证据**（与能力位分离——S0 §3.1 裁决：兼容档
    /// state=true 表达「可尝试」，不等于已取证；探测触发与否看证据，不看能力位）。
    /// Some(target) = 该连接代际 + build 目标已有成功证据（账本命中或探测通过）。
    state_evidence: std::sync::Mutex<Option<ProbeTarget>>,
    /// S3：迟到结果槽位（跨连接共享——换代不丢已捕获证据；连接退出只释放未成熟槽位）。
    late: Arc<super::late::LateSlotRegistry>,
    /// S3：连接级超时/槽位参数注参（受控时间测试；生产 = 缺省常量）。
    conn_tuning: std::sync::Mutex<super::connection::ConnTuning>,
    epoch: AtomicU64,
    /// 终审 F5：本地投影序号（单调递增；发布点经 [`DesktopIpcAdapter::
    /// reserve_publish_seq`]**CAS 预留**，复审 round 5 W1 起不再有无条件
    /// fetch_add 的发布点；快照路径 [`DesktopIpcAdapter::seal_snapshot_seq`]
    /// load 不递增）——本地策略更新（schema 证据安装/恢复、探测通过）与
    /// Desktop 状态帧竞争 SSE 下发时的决胜字段，见 `types::ThreadProjection::
    /// local_seq`。
    projection_seq: AtomicU64,
    events: broadcast::Sender<ThreadProjection>,
}

use super::owners::OwnerRegistry;

/// Desktop IPC 适配器（克隆共享）。
/// 复审 R-T4：队列暂停事实源（thread_id → 是否存在任意暂停行；读失败按 true
/// 保守）。生产装配在 remote 层 `with_parts`（读 `codex_queue_pause`）。
pub(crate) type QueuePauseSource = std::sync::Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// 复审 round 6（Q1）：caps 观察边界缝的回调形状（按路径标签停窗；生产恒
/// `None` 直通）。
pub type CapsObserveGate = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

/// 复审 round 6（Q1-W1 收口）：[`DesktopIpcAdapter::pause_publish_grant`] 的结果。
enum PausePublishGrant {
    /// 重读事实与 tracker 记录值相同（含他人在途已登记的待发布值）→ 抑制
    ///（V1「按实际状态变化发布」的静默语义；在途值也是真实将要落地的值）。
    /// 复审 round 7（Q1）起「记录值」的判定基准 = grant 临界区内**重读**的事实。
    Deduped,
    /// 观察被并发发布越过 → 调用方重观察重试。tracker **未动**——放弃/重观察
    /// 的尝试不留未发布值的假记录（W1 语义保持）。
    Conflict,
    /// 取得发布资格（预留序号）且 tracker 已在同一临界区登记。携带 grant 临界区
    /// 内**重读**的暂停事实——调用方必须在广播前以其覆盖投影（round 7 Q1：广播
    /// 内容必须是提交边界的重核值，不是先前的观察值）。
    Granted(u64, bool),
}

/// R01：一个 Thread 的**交互资格事实快照**（投影临界区内同步写入）。
///
/// 只存判定用标量/键：不含正文、不含信封。`operations` 与卡片/详情同源
/// （`project_mirror` 同一份求值结果），因此 claim 时的「当前是否允许」不需要
/// 二次读 DB 证据或重算策略。
#[derive(Debug, Clone)]
struct InteractionFacts {
    stale: bool,
    /// 逐操作策略（与投影下发同一份事实）。
    operations: super::capability::EffectiveOperations,
    /// 同上但**视为未暂停**（仅一次性越队授权使用；其余门不变）。
    operations_unpaused: super::capability::EffectiveOperations,
    /// 队列暂停（暂停时仅显式越队授权可答）。
    queue_paused: bool,
    /// 原生 pending：`(request_key, context_token, actionable, kind_str)`。
    pending: Vec<(String, String, bool, String)>,
    /// 异步题：`(question_key, context_token, state_str, actionable, disabled_reason,
    /// source_turn_id, source_item_id)`（J01：最终写权按完整组核对）。
    questions: Vec<QuestionFact>,
    /// 当前活动回合（定向打断的安全阀核对；镜像事实）。
    active_turn_id: Option<String>,
}

/// J01：组答题写入时绑定的**完整动作身份**（来源 + 成员 key/token）。
#[derive(Debug, Clone)]
pub(crate) struct GroupBinding {
    pub source_turn_id: String,
    pub source_item_id: String,
    pub members: Vec<(String, String)>,
}

type QuestionFact = (String, String, String, bool, Option<String>, String, String);

impl InteractionFacts {
    fn from_projection(
        p: &super::types::ThreadProjection,
        operations: super::capability::EffectiveOperations,
        operations_unpaused: super::capability::EffectiveOperations,
        queue_paused: bool,
    ) -> Self {
        Self {
            stale: p.stale,
            operations,
            operations_unpaused,
            queue_paused,
            pending: p
                .pending
                .iter()
                .map(|pending| {
                    (
                        pending.request_key.clone(),
                        pending.context_token.clone(),
                        pending.actionable,
                        pending_kind_key(pending.kind).to_string(),
                    )
                })
                .collect(),
            questions: p
                .async_questions
                .iter()
                .map(|question| {
                    (
                        question.question_key.clone(),
                        question.context_token.clone(),
                        question.state.as_str().to_string(),
                        question.actionable,
                        question.disabled_reason_code.clone(),
                        question.source_turn_id.clone(),
                        question.source_item_id.clone(),
                    )
                })
                .collect(),
            active_turn_id: None,
        }
    }
}

/// R01：pending 类别 → 稳定字符串（与 `ActionKind` 账本口径一致，用于 claim 比对）。
fn pending_kind_key(kind: super::types::PendingKind) -> &'static str {
    match kind {
        super::types::PendingKind::CommandApproval => "command_approval",
        super::types::PendingKind::FileChangeApproval => "file_change_approval",
        super::types::PendingKind::PermissionsApproval => "permissions_approval",
        super::types::PendingKind::UserInput => "user_input",
        super::types::PendingKind::Unsupported => "unsupported",
    }
}

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
                profile_send_capable: std::sync::Mutex::new(
                    profile.capabilities.send.then_some(true),
                ),
                last_resolved_evidence: std::sync::Mutex::new(Some(profile.evidence)),
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
                write_right_check: std::sync::Mutex::new(None),
                write_right_claim: std::sync::Mutex::new(None),
                queue_pause_source: std::sync::Mutex::new(None),
                pause_published: std::sync::Mutex::new(HashMap::new()),
                follow_gate: std::sync::Mutex::new(None),
                follow_gate_reached: AtomicBool::new(false),
                write_guard_gate: std::sync::Mutex::new(None),
                write_guard_reached: AtomicBool::new(false),
                schema_evidence: std::sync::Mutex::new(None),
                schema_probe_ran: tokio::sync::Mutex::new(None),
                schema_probe_override: std::sync::Mutex::new(None),
                schema_install_gate: std::sync::Mutex::new(None),
                projection_presnapshot_gate: std::sync::Mutex::new(None),
                projection_sealed_gate: std::sync::Mutex::new(None),
                fresh_snapshot_gate: std::sync::Mutex::new(None),
                republish_publish_gate: std::sync::Mutex::new(None),
                state_publish_gate: std::sync::Mutex::new(None),
                caps_observe_gate: std::sync::Mutex::new(None),
                pause_reserve_gate: std::sync::Mutex::new(None),
                pause_observe_gate: std::sync::Mutex::new(None),
                contract_evidence: std::sync::Mutex::new(None),
                contract_reader: std::sync::Mutex::new(None),
                contract_store: std::sync::Mutex::new(None),
                contract_inflight: tokio::sync::Mutex::new(()),
                contract_attempted: std::sync::Mutex::new(None),
                contract_failure: std::sync::Mutex::new(None),
                contract_install_gate: std::sync::Mutex::new(None),
                contract_target_override: std::sync::Mutex::new(None),
                observed_operations: std::sync::Mutex::new(None),
                interaction_hint_store: std::sync::Mutex::new(None),
                interaction_hints_cache: std::sync::Mutex::new(HashMap::new()),
                interaction_facts: std::sync::Mutex::new(HashMap::new()),
                group_bindings: std::sync::Mutex::new(HashMap::new()),
                interaction_gate: std::sync::Mutex::new(()),
                device_valid_source: std::sync::Mutex::new(None),
                package_sha: std::sync::Mutex::new(None),
                package_sha_override: std::sync::Mutex::new(None),
                recheck_barrier: std::sync::Mutex::new(None),
                recheck_reached: std::sync::atomic::AtomicBool::new(false),
                probe_status_cache: std::sync::Mutex::new(None),
                auto_adapt_gate: std::sync::Mutex::new(None),
                write_guard: std::sync::Mutex::new(None),
                state_evidence: std::sync::Mutex::new(None),
                late: Arc::default(),
                conn_tuning: std::sync::Mutex::new(super::connection::ConnTuning::default()),
                epoch: AtomicU64::new(0),
                projection_seq: AtomicU64::new(0),
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
                chrono::Utc::now().timestamp_millis(),
            )
        });
    }

    /// S1：把 schema 决策词证据装进**取证时捕获的身份**（终审 F2）。
    ///
    /// `captured` = 取证/装回发起时的 (epoch, target) 快照。安装前在
    /// [`Self::with_current_probe_target`] 共用身份守卫下比对当前身份——「最终
    /// 检查通过后、安装前」另一 worker 完成换代时，本安装**放弃**（旧代际结论
    /// 不得标注为新身份，§3.4「旧代际异步结果不得安装到新连接」）。守卫内只取
    /// `schema_evidence` 锁，不复用其他锁（锁序纪律：不嵌套、不跨 await）。
    ///
    /// 返回是否已安装（false = 身份已换代，调用方随之放弃记账/重发布）。
    fn install_schema_evidence(
        &self,
        captured: &ProbeTarget,
        evidence: SchemaEvidence,
        evidenced_at: i64,
    ) -> bool {
        self.with_current_probe_target(captured, || {
            let slot = Some((
                captured.epoch,
                Some(captured.clone()),
                evidence,
                evidenced_at,
            ));
            *self
                .inner
                .schema_evidence
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = slot;
        })
        .is_some()
    }

    /// 终审 R4/R6：**自动适配状态变更通知**（toggle 命令链调用；enabled = 重开，
    /// false = 关闭）。
    ///
    /// 终审 R4：调度用 `tauri::async_runtime::spawn`（内部自带全局 runtime，任意
    /// 线程可用——无当前 Tokio runtime 语境的同步命令线程也能安排任务），**不依赖
    /// 调用线程**的 `Handle::try_current()`（那在真实同步命令路径上会失败并静默
    /// 丢弃通知）。停机态不触发（stop 后不再有任何新探测/安装/重发布）。
    ///
    /// 行为：重开（true）→ 补触发 schema 按需取证（证据门照旧：已有成功证据零
    /// 重探、迟到结果身份守卫不变）；**开/关都重发布**已跟随线程的投影——
    /// `approval_policy` 与 `attemptable` 是活值，重发布让 SSE 下游收到生产真实
    /// 会发的 availability/可尝试翻转帧（终审 R6 后端半边；F2 安装身份守卫不变）。
    pub fn notify_auto_adapt_changed(&self, enabled: bool) {
        // 停机态不触发（stop 后不再有任何新探测/安装/重发布）。
        if self.inner.stopping.load(Ordering::SeqCst) {
            return;
        }
        let adapter = self.clone();
        tauri::async_runtime::spawn(async move {
            adapter.run_auto_adapt_notify(enabled).await;
        });
    }

    /// T08：**已在异步上下文内**调用（移动端点）——用当前 tokio 运行时派生。
    ///
    /// 为什么分开：`tauri::async_runtime::spawn` 会懒初始化其全局运行时；在已有
    /// 运行时里做该初始化会阻塞（T08 集成测试实证的死锁）。同步命令线程仍走
    /// [`Self::notify_auto_adapt_changed`]（那里没有当前运行时可用）。
    pub fn notify_auto_adapt_changed_async(&self, enabled: bool) {
        if self.inner.stopping.load(Ordering::SeqCst) {
            return;
        }
        let adapter = self.clone();
        tokio::spawn(async move {
            adapter.run_auto_adapt_notify(enabled).await;
        });
    }

    /// 通知主体：重开 = 只读按需取证（schema 决策词 + 当前包契约识别，spec §3.2：
    /// 不自动批准、不发送、不恢复队列）；**开/关都重发布**已跟随线程的投影。
    async fn run_auto_adapt_notify(self, enabled: bool) {
        if enabled {
            self.ensure_schema_evidence().await;
            self.ensure_contract_evidence().await;
        }
        self.republish_all_projections().await;
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
    /// 流程（幂等 + 只跑一次；S1 起先装持久证据，再跑探针）：
    /// 1. 自动适配关闭 → 直接返回（**活值门**：关闭后迟到的探针结论不得安装、
    ///    不得解锁兼容审批——设计 §3.3）；
    /// 2. 取探测目标（`probe_target_snapshot()` + `resolve_probe_key()`）：取不到
    ///    target → **直接返回，不记账**（无法归因的行宁可没有，也不能错记到别的
    ///    build 上）；
    /// 3. **同一完整指纹与连接代际已完成探测** → 直接返回；
    /// 4. **持久决策词证据命中 → 原地装回**（零 CLI 重跑、零重记账——重启恢复即此
    ///    路径；装回绑定当前身份，旧代际/旧指纹的行不安装）；
    /// 5. 跑探针（`probe_schema()` 内部已 3s 有界 + 临时目录用完即删；测试走注入
    ///    结论，绝不 spawn 真实 CLI）；
    /// 6. `Known(evidence)` → 装证据 + 决策词落持久表 + 记 `Probed` + **原地重发布
    ///    已跟随线程**（比首快照晚到也原地解锁已打开的审批卡）；`Unknown{error_code}`
    ///    → **不装证据**（审批保持「到桌面处理」）+ 记 `Inconclusive`（带错误码）。
    ///
    /// schema 内容指纹随结论写入账本，供审批词表归因。
    pub async fn ensure_schema_evidence(&self) {
        if !self.auto_adapt_enabled() {
            // 关闭 = 阻止兼容审批决策的取证与安装（已安装的在证值由 approval_policy
            // 的活值门即时压回空档，见下）。
            return;
        }
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
        // S1：持久决策词证据命中 → **先于单飞检查**原地装回（零 CLI 重跑）。装回在
        // ran 之前：持久证据 = 成功证据，本轮 epoch 是否跑过 CLI 探针只约束 CLI
        // 重跑，不约束证据装回（「成功后一次不确定」不得遮住既有成功证据，设计
        // §3.2）。装回前复核身份：换代的持久行不属于本连接。
        if let Some(persisted) = self
            .probe_ledger()
            .as_ref()
            .and_then(|l| l.load_schema_evidence(&probe_key))
        {
            if self.inner.epoch.load(Ordering::SeqCst) != epoch
                || self.probe_target_snapshot() != bound_target
            {
                return; // 身份已变化：旧代际的持久证据不安装到新连接。
            }
            log::info!(
                "codex-ipc schema: key={probe_key} 持久决策词证据命中，零重跑装回（fingerprint={}）",
                persisted.fingerprint
            );
            *ran = Some((epoch, probe_key.clone()));
            // 终审 F2 测试缝：恢复分支同样暴露「检查后、装回前」窗口（生产直通）。
            self.run_schema_install_gate();
            // 装回绑定**取证时捕获的身份**（与最终检查同一份快照，含完整指纹）：
            // 窗口内换代 → 放弃装回（不发布旧档）。
            let captured = bound_target.clone().unwrap_or_else(|| target.clone());
            if !self.install_schema_evidence(
                &captured,
                SchemaEvidence {
                    fingerprint: persisted.fingerprint,
                    command_decisions: persisted.command_decisions,
                    file_decisions: persisted.file_decisions,
                },
                persisted.evidenced_at,
            ) {
                log::info!(
                    "codex-ipc schema: key={probe_key} 恢复装回时身份已换代，放弃（不装旧代际证据）"
                );
                return;
            }
            self.republish_all_projections().await;
            return;
        }
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
        // 终审 F2 测试缝：在「最终身份检查通过后、安装前」暴露定序窗口（生产直通）。
        self.run_schema_install_gate();
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
                let evidenced_at = chrono::Utc::now().timestamp_millis();
                // 终审 F2：安装绑定取证时捕获的身份（`bound_target` = 最终身份检查
                // 所比对的那份快照——含 resolve 后的完整指纹）；窗口内换代 → 不
                // 安装、不记账、不重发布（A 的结论不得错落到 B 的账上/解锁 B 的卡）。
                let captured = bound_target.clone().unwrap_or_else(|| target.clone());
                if !self.install_schema_evidence(&captured, evidence.clone(), evidenced_at) {
                    log::info!(
                        "codex-ipc schema: key={probe_key} 结论到达时身份已换代，拒收（不记账）"
                    );
                    return;
                }
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
                    // S1：决策词**持久**保存（只存指纹重建不了词，设计 §3.2）——
                    // 重启后零 CLI 重跑恢复审批档。
                    ledger.store_schema_evidence(&probe_key, &evidence, evidenced_at);
                }
                // S1：比首快照晚到也要原地解锁——已跟随线程立即重发布新审批档。
                self.republish_all_projections().await;
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

    /// S1：把当前策略下已跟随线程的投影**原地重发布**（成功证据加载/更新通知）。
    ///
    /// 消费方 = `subscribe()` 的广播（SSE thread/events 与队列调度同源）：schema
    /// 证据安装/恢复、只读探测通过都会走这里，让已打开的审批卡与「已适配」状态
    /// **不需要重新进入会话**即更新（设计 §3.4「不能出现第一次进入禁用、重新进入
    /// 才解锁」）。无镜像的 key 跳过（不发布 stale 空投影）。
    async fn republish_all_projections(&self) {
        for key in followed_keys(self).await {
            if !self.has_mirror(&key).await {
                continue;
            }
            let p = self.projection(&key).await;
            // 复审 round 4（V3）：观察被后续发布越过 → 重观察（见 refresh 的
            // 内容版本/传输顺序分离论证）。
            let p = self.refresh_if_projection_stale(&key, p).await;
            // 复审 round 5（W1/V3 收口）测试缝：**检查（refresh）已通过、最终发布
            // 序号尚未取得**的定序窗（生产恒 None 直通）。
            self.run_republish_publish_gate();
            // 复审 round 5（W1）：发布走全入口统一的内容版本协议——CAS 预留；
            // 窗内被并发发布越过 → 重观察后再预留（round 4 的 refresh 只是前置
            // 快速路径，强制边界在预留这一步）。
            self.publish_fresh_projection(&key, p).await;
        }
    }

    /// 复审 round 4（V3）：**内容版本轴与传输顺序分离**的重发布守卫。
    ///
    /// `p.local_seq` = 投影内容生成（镜像临界区盖章）时观察到的版本（
    /// `seal_snapshot_seq`）；`projection_seq` 当前头 = 本 adapter 已发布到的
    /// 位置。两者相等 = 盖章以来无任何发布，观察仍是最新的——发布点分配新序号
    /// 是合法的内容版本推进（终审 F5 的穿透语义保持）。**小于**头 = 本观察已被
    /// 后续发布越过（典型：定序窗内设置翻转 + 新策略发布完成）——旧 payload 若
    /// 照旧发布，`publish_event` 会按发送时间重赋更大序号，「发送得晚」被当成
    /// 「内容更新」：SSE supersedes 判它覆盖新状态（迟到旧启用按钮压回禁用卡，
    /// Desktop 静默后长期停留旧策略）。此时**丢弃旧观察、重新观察**：新内容的
    /// 盖章即当前头，序号分配表示的是实际观察到的状态而非发送时刻；旧 payload
    /// 不取得覆盖新状态的资格，也不伪造 Desktop revision。无新增锁（两次
    /// atomic load）。
    ///
    /// 复审 round 5（W1/V3 收口）：round 4 的检查与发布仍是两步——检查通过后
    /// 到取号之间的窗口依然裸奔。**强制边界收敛到发布点的 CAS 预留**
    /// （`reserve_publish_seq`，见 `publish_fresh_projection`）；本检查保留为
    /// 前置快速路径（少一次注定失败的预留 + 缝窗口语义不变）。
    async fn refresh_if_projection_stale(
        &self,
        key: &ThreadKey,
        p: ThreadProjection,
    ) -> ThreadProjection {
        let head = self.inner.projection_seq.load(Ordering::SeqCst);
        if p.local_seq >= head {
            return p;
        }
        self.projection(key).await
    }

    /// 终审 F2 测试缝：注入「最终检查后、安装前」的回调（生产恒 `None` 直通）。
    #[doc(hidden)]
    pub fn set_schema_install_gate(&self, gate: Option<Box<dyn Fn() + Send + Sync>>) {
        *self
            .inner
            .schema_install_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = gate;
    }

    /// 跑一次安装前测试缝（生产恒 None → 零开销直通）。
    fn run_schema_install_gate(&self) {
        if let Some(g) = self
            .inner
            .schema_install_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            g();
        }
    }

    /// 复审 round 3（T3）测试缝注入 ×3（生产恒 `None` 直通；语义见字段注释）。
    #[doc(hidden)]
    pub fn set_projection_presnapshot_gate(
        &self,
        gate: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
    ) {
        *self
            .inner
            .projection_presnapshot_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = gate;
    }

    #[doc(hidden)]
    pub fn set_projection_sealed_gate(&self, gate: Option<std::sync::Arc<dyn Fn() + Send + Sync>>) {
        *self
            .inner
            .projection_sealed_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = gate;
    }

    #[doc(hidden)]
    pub fn set_fresh_snapshot_gate(&self, gate: Option<std::sync::Arc<dyn Fn() + Send + Sync>>) {
        *self
            .inner
            .fresh_snapshot_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = gate;
    }

    /// 复审 round 4（V3）测试缝注入（生产恒 `None` 直通；语义见字段注释）。
    #[doc(hidden)]
    pub fn set_republish_publish_gate(&self, gate: Option<std::sync::Arc<dyn Fn() + Send + Sync>>) {
        *self
            .inner
            .republish_publish_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = gate;
    }

    /// 复审 round 5（W1/V3 收口）测试缝注入（生产恒 `None` 直通；语义见字段注释）。
    #[doc(hidden)]
    pub fn set_state_publish_gate(&self, gate: Option<std::sync::Arc<dyn Fn() + Send + Sync>>) {
        *self
            .inner
            .state_publish_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = gate;
    }

    /// 复审 round 6（Q1）测试缝注入（生产恒 `None` 直通；语义见字段注释）。
    /// 参数 = 路径标签（`state_changed` / `publish_projection` /
    /// `client_status_changed` / `mark_all_mirrors_stale`）。
    #[doc(hidden)]
    pub fn set_caps_observe_gate(&self, gate: Option<CapsObserveGate>) {
        *self
            .inner
            .caps_observe_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = gate;
    }

    /// 复审 round 6（Q1-W1 收口）测试缝注入（生产恒 `None` 直通；语义见字段注释）。
    #[doc(hidden)]
    pub fn set_pause_reserve_gate(&self, gate: Option<std::sync::Arc<dyn Fn() + Send + Sync>>) {
        *self
            .inner
            .pause_reserve_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = gate;
    }

    /// 复审 round 7（Q1）测试缝注入（生产恒 `None` 直通；语义见字段注释）。
    #[doc(hidden)]
    pub fn set_pause_observe_gate(&self, gate: Option<std::sync::Arc<dyn Fn() + Send + Sync>>) {
        *self
            .inner
            .pause_observe_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = gate;
    }

    fn run_projection_presnapshot_gate(&self) {
        // 先取出、释放缝锁、后调用——缝回调可再入取快照路径（触发重发布等），
        // 持锁调用会自锁死。
        let g = self
            .inner
            .projection_presnapshot_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(g) = g.as_ref() {
            g();
        }
    }

    fn run_projection_sealed_gate(&self) {
        let g = self
            .inner
            .projection_sealed_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(g) = g.as_ref() {
            g();
        }
    }

    fn run_fresh_snapshot_gate(&self) {
        let g = self
            .inner
            .fresh_snapshot_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(g) = g.as_ref() {
            g();
        }
    }

    /// 复审 round 4（V3）：跑一次重发布发布缝（生产恒 None → 零开销直通）。
    fn run_republish_publish_gate(&self) {
        let g = self
            .inner
            .republish_publish_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(g) = g.as_ref() {
            g();
        }
    }

    /// 复审 round 5（W1）：跑一次 state-changed 发布缝（生产恒 None → 零开销直通）。
    fn run_state_publish_gate(&self) {
        let g = self
            .inner
            .state_publish_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(g) = g.as_ref() {
            g();
        }
    }

    /// 复审 round 6（Q1）：跑一次 caps 观察边界缝（生产恒 None → 零开销直通）。
    /// 先取出、释放缝锁、后调用——缝回调内可再入发布/取快照路径（伙伴重发布），
    /// 持锁调用会自锁死（T3 缝同款纪律）。
    fn run_caps_observe_gate(&self, path: &'static str) {
        let g = self
            .inner
            .caps_observe_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(g) = g.as_ref() {
            g(path);
        }
    }

    /// 复审 round 6（Q1-W1 收口）：跑一次暂停 tracker 预留缝（生产恒 None 直通）。
    fn run_pause_reserve_gate(&self) {
        let g = self
            .inner
            .pause_reserve_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(g) = g.as_ref() {
            g();
        }
    }

    /// 复审 round 7（Q1）：跑一次暂停重发布观察缝（生产恒 None → 零开销直通）。
    /// 先取出、释放缝锁、后调用（缝回调可阻塞等待伙伴动作，持锁调用会自锁死）。
    fn run_pause_observe_gate(&self) {
        let g = self
            .inner
            .pause_observe_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(g) = g.as_ref() {
            g();
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
        // S1（设计 §3.3）：自动适配关闭 → 兼容审批决策一并关闭（已验证 build 的
        // 证据档已在上面返回，不受影响）；**活值**门——关闭后迟到的 schema 结果
        // 不得恢复未知版本操作。
        if !self.auto_adapt_enabled() {
            return super::pending::DecisionPolicy::default();
        }
        let evidence = self
            .inner
            .schema_evidence
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        match evidence {
            Some((epoch, target, ev, _))
                if epoch == self.inner.epoch.load(Ordering::SeqCst)
                    && target == self.probe_target_snapshot() =>
            {
                {
                    // R09：按 schema **实际词表**逐类构造（命令策略不再覆盖文件策略）。
                    let words = |allows: &dyn Fn(&str) -> bool| {
                        [
                            super::pending::DECISION_ACCEPT,
                            super::pending::DECISION_DECLINE,
                            super::pending::DECISION_CANCEL,
                        ]
                        .into_iter()
                        .filter(|word| allows(word))
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                    };
                    // 命令侧沿用既有已核验口径（未知 build 不开 decline：语义差异会
                    // 让用户误以为"拒绝后回合继续"）；文件侧按 R09 采用 schema 实际词
                    // （含 decline 才能表达"拒绝但继续"）。
                    let command = words(&|word| {
                        ev.command_allows(word)
                            && (word == super::pending::DECISION_ACCEPT
                                || word == super::pending::DECISION_CANCEL)
                    });
                    let file = words(&|word| ev.file_allows(word));
                    // 至少要有 accept 才开放该类的任何决策（否则空策略）。
                    let keep = |mut list: Vec<String>| {
                        if list
                            .iter()
                            .any(|word| word == super::pending::DECISION_ACCEPT)
                        {
                            list
                        } else {
                            list.clear();
                            list
                        }
                    };
                    super::pending::DecisionPolicy::schema_with_words(keep(command), keep(file))
                }
            }
            _ => super::pending::DecisionPolicy::default(),
        }
    }

    /// 当前连接的 profile（R3 动态值；clone 出短临界区）。
    /// H02：注入设备有效性只读源（生产 = 配对表同库查询）。
    #[doc(hidden)]
    pub fn set_device_valid_source(&self, source: Option<DeviceValidSource>) {
        *self
            .inner
            .device_valid_source
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = source;
    }

    fn device_is_valid(&self, device_id: &str) -> bool {
        self.inner
            .device_valid_source
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|check| check(device_id))
    }

    /// H01 测试缝：覆盖当前包内容 SHA（生产不调用）。
    #[doc(hidden)]
    pub fn set_package_sha_override(&self, sha: Option<String>) {
        *self
            .inner
            .package_sha_override
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = sha;
    }

    /// H01：当前**完整目标**（probe key + 包内容 SHA）；任一缺失 → 无目标（不授权）。
    pub(crate) fn observed_target(
        &self,
    ) -> Option<crate::services::codex_desktop_ipc::capability::ObservedTarget> {
        let probe_key = self.observed_probe_key()?;
        let current_epoch = self.inner.epoch.load(std::sync::atomic::Ordering::SeqCst);
        let sha = self
            .inner
            .package_sha_override
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .or_else(|| {
                self.inner
                    .package_sha
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .filter(|identity| {
                        identity.epoch == current_epoch && identity.probe_key == probe_key
                    })
                    .map(|identity| identity.sha256.clone())
            })?;
        Some(
            crate::services::codex_desktop_ipc::capability::ObservedTarget {
                probe_key,
                package_sha256: sha,
            },
        )
    }

    /// H02 测试缝：写边界复核会合（生产不调用）。
    #[doc(hidden)]
    pub fn set_interaction_recheck_barrier(&self, barrier: Option<Arc<std::sync::Barrier>>) {
        *self
            .inner
            .recheck_barrier
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = barrier;
    }

    /// H02 测试缝：复核是否已到达（有界轮询用）。
    #[doc(hidden)]
    pub fn interaction_recheck_reached(&self) -> bool {
        self.inner
            .recheck_reached
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// H02：写边界复核的会合（生产恒 None → 直通）。
    fn run_interaction_recheck_gate(&self) {
        let barrier = self
            .inner
            .recheck_barrier
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(barrier) = barrier {
            self.inner
                .recheck_reached
                .store(true, std::sync::atomic::Ordering::SeqCst);
            let _ = barrier.wait();
            self.inner
                .recheck_reached
                .store(false, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// H01：当前观察目标的 probe key（无则 None——成功证据不跨目标复用）。
    pub(crate) fn observed_probe_key(&self) -> Option<String> {
        if let Some(key) = self
            .inner
            .probe_key_override
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return Some(key);
        }
        self.inner
            .probe_target
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|target| target.key.clone())
    }

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

    /// 终审 Gate F Major 2：**投影用有效 caps**——设置活门并入投影：关闭自动适配
    /// 且未知 build（compat 档，证据级别 Unverified）→ send 压 false →
    /// availability 翻 read_only（兼容档在场时 caps.send=true 是**连接时装入**的
    /// 档位，活门翻转必须即刻生效——与 [`Self::send_gate_static`] Unverified 活值
    /// 回撤同源同语义；重开由同一活值自动翻回，无特判）。已验证 build（Shipped）
    /// 不受开关连带影响（F3/F9/T6 语义保持：按已验证能力运行）。仅用于**投影**
    /// 位点（availability/可尝试呈现）；协议能力检查（start_turn 等）仍用
    /// [`Self::current_caps`]。
    fn effective_projection_caps(&self) -> CapabilitySet {
        let mut caps = self.current_caps();
        let profile = self.current_profile();
        if !self.auto_adapt_enabled()
            && profile.evidence == super::profile::EvidenceLevel::Unverified
            && caps.send
        {
            caps.send = false;
        }
        caps
    }

    /// S2 正名（原 `build_send_verified`）：最近一次成功建连档位的 send 能力位
    /// 快照（`None` 视为 false）。**不表达「已验证」**——未知 build 兼容档同样记
    /// true；语义与消费者见 [`AdapterInner.profile_send_capable`] 注释。
    pub fn profile_send_capable(&self) -> bool {
        self.profile_send_capable_opt().unwrap_or(false)
    }

    /// 同上的 `Option` 形态（`None` = 本进程还没成功建连过；send_gate_static 用它
    /// 区分「从未取证」与「取证过但档位无写」）。
    pub fn profile_send_capable_opt(&self) -> Option<bool> {
        *self
            .inner
            .profile_send_capable
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Gate A #2：最近一次成功建连的证据档（DTO「已取证」的 shipped 判定来源；
    /// 不随断链回落——断链只回落当前 profile，不失忆档位历史）。
    fn last_resolved_evidence_opt(&self) -> Option<super::profile::EvidenceLevel> {
        *self
            .inner
            .last_resolved_evidence
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// **S2 可发送判据（静态部分，单点）**——send-info 与 session-send 的服务端门
    /// 同源消费（实时守卫见 `send_gate_snapshot` / router 两道门）。
    ///
    /// * shipped 证据档：按证据位（已验证 build 恒按证据，自动适配不放开也不收紧）；
    /// * 判死档（closed）：不可尝试；若快照显示该 build 档位带写（已取证 build 的
    ///   瞬时断链）返回 `capability_unavailable`，否则 `capability_unverified`；
    /// * 未知 build（兼容档/只读档/未验证档）：自动适配开 = 可按协议守卫尝试
    ///   （发送本身提供写证据，设计 §3.3）；关 = 不可尝试。
    ///
    /// `Err(code)` = 不可尝试及其理由码；`Ok(())` = 可尝试（仍需实时守卫放行）。
    pub fn send_gate_static(&self) -> Result<(), &'static str> {
        let profile = self.current_profile();
        match profile.evidence {
            super::profile::EvidenceLevel::Closed => {
                if self.profile_send_capable_opt() == Some(true) {
                    Err("capability_unavailable")
                } else {
                    Err("capability_unverified")
                }
            }
            super::profile::EvidenceLevel::Shipped => {
                if profile.capabilities.send {
                    Ok(())
                } else {
                    Err("capability_unverified")
                }
            }
            super::profile::EvidenceLevel::Unverified => {
                // Gate A #1：自动适配关 = **活值**阻止未知 build 普通发送（设计
                // §3.3）——兼容档连接在场（caps.send=true）不构成豁免：档位是连接
                // 时装入的，开关翻转必须即刻生效。与 DTO `attemptable.send` 的活值
                // 回撤同源同答案（send-info 与 DTO 不得给出相反结论）。
                if self.auto_adapt_enabled() {
                    Ok(())
                } else {
                    Err("capability_unverified")
                }
            }
        }
    }

    // ========================================================
    // S1：自动适配门 + 读链证据旗标 + 适配状态 DTO
    // ========================================================

    /// 终审 F4 测试/装配缝：注入**写边界守卫**（生产 = remote 层的撤销标记读取 +
    /// 兼容许可活值；`None` = 恒放行，适配器单测不依赖 DB）。守卫在 dispatch 进
    /// writer 队列前调用——「已过上层检查但帧未写」的在飞动作在此与人工继续
    /// 原子竞争（同一持久撤销标记）。锁纪律：std::Mutex 短临界区取 Arc 后即释锁
    /// 再调用（闭包内自取 DB 锁，不与 adapter 锁嵌套）。
    #[doc(hidden)]
    pub fn set_write_guard(&self, guard: Option<WriteGuardFn>) {
        *self
            .inner
            .write_guard
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = guard;
    }

    /// 终审 F4 测试缝探针：是否有任务正停在 write guard（测试轮询替代固定睡眠）。
    #[doc(hidden)]
    pub fn write_guard_gate_reached(&self) -> bool {
        self.inner.write_guard_reached.load(Ordering::SeqCst)
    }

    /// 终审 F4 测试缝：注入写边界守卫**调用前**的等待（生产恒 None）。
    #[doc(hidden)]
    pub fn set_write_guard_gate(&self, gate: Option<Arc<tokio::sync::Notify>>) {
        *self
            .inner
            .write_guard_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = gate;
    }

    /// 终审 R2：现行连接的**写右判定**注入（生产在 ensure_connection 后由 remote
    /// 层设置一次；测试经本缝在现行连接上注入/清除）。`None` = 清除。
    /// 终审 R2（同步源 setter）：只写判定源字段（后续新连接 spawn 时继承转发）。
    /// 用于同步装配（`with_parts`）——不可在运行中连接上直接生效；需要即时转发
    /// 请用 async 版 [`Self::set_write_right_check`]。
    #[doc(hidden)]
    pub(crate) fn set_write_right_check_source(
        &self,
        check: Option<crate::services::codex_control::WriteGuardVerdictChecker>,
    ) {
        *self
            .inner
            .write_right_check
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = check;
    }

    /// 复审 R-T1（同步源 setter）：只写 claim 源字段（后续新连接 spawn 时继承
    /// 转发）——与 [`Self::set_write_right_check_source`] 同款装配纪律。
    #[doc(hidden)]
    pub(crate) fn set_write_right_claim_source(
        &self,
        claim: Option<crate::services::codex_control::WriteRightClaim>,
    ) {
        *self
            .inner
            .write_right_claim
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = claim;
    }

    /// 复审 R-T4（同步源 setter）：队列暂停事实源——remote 层在 `with_parts`
    /// 装配（读持久暂停行是否存在）；投影盖章与 republish 消费。
    #[doc(hidden)]
    pub(crate) fn set_queue_pause_source(&self, source: Option<QueuePauseSource>) {
        *self
            .inner
            .queue_pause_source
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = source;
    }

    #[doc(hidden)]
    pub async fn set_write_right_check(
        &self,
        check: Option<crate::services::codex_control::WriteGuardVerdictChecker>,
    ) {
        let conn = self.inner.conn.lock().await.clone();
        if let Some(conn) = conn {
            conn.set_write_right_check(check);
        }
    }

    /// J01 测试缝：异步组复核之后、最终 claim 之前的 gate。
    #[doc(hidden)]
    pub async fn set_final_claim_gate_for_tests(&self, gate: Option<Arc<tokio::sync::Notify>>) {
        let conn = self.inner.conn.lock().await.clone();
        if let Some(conn) = conn {
            conn.set_final_claim_gate(gate);
        }
    }

    #[doc(hidden)]
    pub async fn final_claim_gate_reached(&self) -> bool {
        let conn = self.inner.conn.lock().await.clone();
        conn.map(|conn| conn.final_claim_gate_reached())
            .unwrap_or(false)
    }

    /// 终审 R2 测试缝：现行连接的 writer 定序门注入/清除。
    #[doc(hidden)]
    pub async fn set_write_right_gate_for_tests(&self, gate: Option<Arc<tokio::sync::Notify>>) {
        let conn = self.inner.conn.lock().await.clone();
        if let Some(conn) = conn {
            conn.set_write_right_gate(gate);
        }
    }

    /// 复审 R-T1 测试缝：现行连接的 claim 注入/清除（DB 读失败反例等）。
    #[doc(hidden)]
    pub async fn set_write_right_claim_for_tests(
        &self,
        claim: Option<crate::services::codex_control::WriteRightClaim>,
    ) {
        let conn = self.inner.conn.lock().await.clone();
        if let Some(conn) = conn {
            conn.set_write_right_claim(claim);
        }
    }

    /// 终审 R2 测试缝探针：writer 是否停在现行连接的写右判定前。
    #[doc(hidden)]
    pub async fn write_right_gate_reached_for_tests(&self) -> bool {
        let conn = self.inner.conn.lock().await.clone();
        conn.is_some_and(|c| c.write_right_gate_reached())
    }
    /// 终审 F4：写边界守卫的读取（None = 放行；见 [`AdapterInner.write_guard`]）。
    /// 测试缝：守卫闭包调用前若注入了等待门则先等待（固定「判定前交错」）。
    fn write_guard_verdict(
        &self,
        action_id: &str,
        compat: bool,
    ) -> crate::services::codex_control::WriteGuardVerdict {
        let guard = self
            .inner
            .write_guard
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        match guard {
            Some(g) => g(action_id, compat),
            None => crate::services::codex_control::WriteGuardVerdict::Allow,
        }
    }

    /// S1 测试/装配缝：注入「自动适配」设置的**活值**读取闭包（生产 = remote 层的
    /// settings 读取；`None` = 恒开，适配器单测不依赖 DB）。契约见
    /// [`AdapterInner.auto_adapt_gate`]。
    #[doc(hidden)]
    pub fn set_auto_adapt_gate(&self, gate: Option<Arc<dyn Fn() -> bool + Send + Sync>>) {
        *self
            .inner
            .auto_adapt_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = gate;
    }

    /// S3：注入连接级参数（受控时间测试收窄超时/槽位容量；生产恒缺省常量）。
    #[doc(hidden)]
    pub fn set_conn_tuning(&self, tuning: super::connection::ConnTuning) {
        *self
            .inner
            .conn_tuning
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = tuning;
    }

    fn conn_tuning(&self) -> super::connection::ConnTuning {
        *self
            .inner
            .conn_tuning
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// S3：迟到槽位注册表（生产消费口 = [`Self::take_ready_late_results`]；
    /// 测试可直接注入/检查）。
    pub fn late_registry(&self) -> Arc<super::late::LateSlotRegistry> {
        self.inner.late.clone()
    }

    /// S3：排空全部已捕获的迟到结果（后台对账兜底消费口；等待者丢失/收尾已结束的
    /// 迟到证据由此归还原动作）。
    pub async fn take_ready_late_results(&self) -> Vec<super::late::LateResult> {
        self.inner.late.take_ready()
    }

    /// 【Gate B 复审】定向消费：只取指定 action 的成熟迟到结论（只读核对步骤②
    /// 专用——异动作证据留给调度循环对账，核对预算不被软化）。
    pub async fn take_ready_late_results_for(
        &self,
        action_id: &str,
    ) -> Vec<super::late::LateResult> {
        self.inner.late.take_ready_for(action_id)
    }

    /// 自动适配生效值（缺省开；活值——开关翻转即刻约束新的探测/安装/审批档）。
    fn auto_adapt_enabled(&self) -> bool {
        self.inner
            .auto_adapt_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|g| g())
            .unwrap_or(true)
    }

    /// 读链成功证据是否对**当前**连接代际 + build 目标成立（S1；不查账本、纯内存）。
    fn state_evidence_current(&self) -> bool {
        let slot = self
            .inner
            .state_evidence
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(ev) = slot else {
            return false;
        };
        match self.probe_target_snapshot() {
            Some(t) => {
                t.epoch == ev.epoch
                    && t.platform == ev.platform
                    && t.build == ev.build
                    && (t.key.is_none() || ev.key.is_none() || t.key == ev.key)
            }
            None => false,
        }
    }

    /// 在当前连接代际 + build 目标下确立读链成功证据（探测通过 / 测试直调时用；
    /// 目标已换代时静默丢弃——旧代际证据不跨代安装）。
    ///
    /// **锁纪律**：本方法会取 `probe_target` 锁做身份复核——**不得**在
    /// [`Self::with_current_probe_target`] 的闭包里调用（那把锁已被闭包外层持有，
    /// `std::sync::Mutex` 不可重入，同线程再取即自锁死）；身份守卫内的调用点
    /// （账本命中）用 [`Self::store_state_evidence`]。
    fn mark_state_evidenced(&self, target: &ProbeTarget) {
        let current = self.probe_target_snapshot();
        let still_current = self.inner.epoch.load(Ordering::SeqCst) == target.epoch
            && current.as_ref().is_some_and(|t| {
                t.epoch == target.epoch && t.platform == target.platform && t.build == target.build
            });
        if still_current {
            self.store_state_evidence(target);
        }
    }

    /// S1：无复核写旗标——**只**取 `state_evidence` 锁。调用方必须已在身份守卫内
    /// （`with_current_probe_target` 闭包已复核过代际与 build 目标）。
    fn store_state_evidence(&self, target: &ProbeTarget) {
        *self
            .inner
            .state_evidence
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(target.clone());
    }

    /// S1：**适配状态 DTO**（权威形状见 `adapt_status::CodexAdaptStatus`；S2/S5/R1
    /// 只消费它）。`auto_adapt_explicit` = 新设置的用户显式选择（`None` = 从未写过
    /// → 缺省开；生效值在 DTO 内单点计算）。
    ///
    /// 只读：不查账本写路径、不建连、不发帧；账本读两次（state/send 行）+ 一次
    /// 指纹解析（同 build 已缓存则零开销）。
    pub async fn adapt_status(
        &self,
        auto_adapt_explicit: Option<bool>,
    ) -> super::adapt_status::CodexAdaptStatus {
        use super::adapt_status::{
            AttemptableSet, CodexAdaptStatus, EvidencedSet, FailureObservation, SchemaEvidenceDto,
        };
        let profile = self.current_profile();
        let caps = profile.capabilities;
        let gate_on = self.auto_adapt_enabled();
        // Gate A #2：「已取证」的 shipped 判定读**最近一次成功建连的证据档**——
        // 断链把当前 profile 回落 closed 后，shipped build 的已取证维度不得失忆。
        let shipped =
            self.last_resolved_evidence_opt() == Some(super::profile::EvidenceLevel::Shipped);
        let target = self.probe_target_snapshot();
        let probe_key = match &target {
            Some(t) => Some(self.resolve_probe_key(t).await),
            None => None,
        };
        let ledger = self.probe_ledger();
        let state_row = probe_key
            .as_ref()
            .zip(ledger.as_ref())
            .and_then(|(k, l)| l.get(k, CAP_STATE));
        let send_row = probe_key
            .as_ref()
            .zip(ledger.as_ref())
            .and_then(|(k, l)| l.get(k, CAP_SEND));
        // schema 证据只认**绑定当前身份**（epoch + 完整指纹）的在证值——旧代际/旧
        // 指纹的异步回填不得计入（§3.4）。
        let schema_ev = self
            .inner
            .schema_evidence
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let schema_current = schema_ev
            .filter(|(epoch, bound, _, _)| {
                *epoch == self.inner.epoch.load(Ordering::SeqCst)
                    && bound.as_ref() == target.as_ref()
            })
            .map(|(_, _, ev, at)| (ev, at));
        // 「已取证」：shipped 表真机证据 ∨ 持久账本证据 ∨ 本进程已确立的证据旗标。
        let evidenced_state = shipped
            || state_row.as_ref().is_some_and(|e| e.evidenced)
            || self.state_evidence_current();
        // Gate A #2：写档证据与**当前能力位**解耦（caps.send 随断链回落，证据不回
        // 落）——shipped 判据看「最近一次建连的档位带写」快照（profile_send_capable
        // 与证据档同点写、同样持久）。
        let evidenced_send = (shipped && self.profile_send_capable_opt() == Some(true))
            || send_row.as_ref().is_some_and(|e| e.evidenced);
        // 最近失败观测（与成功证据分离的一条线）：state/send 两行里取最近的一条。
        let last_failure = [
            state_row.as_ref().map(|e| (CAP_STATE, e)),
            send_row.as_ref().map(|e| (CAP_SEND, e)),
        ]
        .into_iter()
        .flatten()
        .filter(|(_, e)| {
            matches!(
                e.state,
                crate::database::dao::codex_ipc_probe::ProbeState::Failed
                    | crate::database::dao::codex_ipc_probe::ProbeState::Inconclusive
            )
        })
        .max_by_key(|(_, e)| e.last_seen_at.unwrap_or(i64::MIN))
        .map(|(capability, e)| FailureObservation {
            capability: capability.to_string(),
            state: e.state.as_str().to_string(),
            stage: e.stage.clone(),
            error_code: e.error_code.clone(),
            observed_at: e.last_seen_at,
            next_retry_at: e.next_retry_at,
        });
        // 「可尝试」：档位能力位 ∧（自动适配开 ∨ shipped 证据档）。开关关闭时未知
        // build 的兼容档发送位**活值回撤**（Gate A #1：send_gate_static 同款活值，
        // 两处同源同答案）；断链回落 closed 档后能力位归零（可尝试随链路回落，
        // 与已取证分列）。
        CodexAdaptStatus {
            auto_adapt_enabled: auto_adapt_explicit.unwrap_or(true),
            auto_adapt_explicit,
            platform: profile.platform.clone(),
            desktop_build: if profile.build.is_empty() {
                None
            } else {
                Some(profile.build.clone())
            },
            probe_key,
            connection_epoch: self.inner.epoch.load(Ordering::SeqCst),
            attemptable: AttemptableSet {
                send: caps.send && (gate_on || shipped),
                state: caps.state,
            },
            evidenced: EvidencedSet {
                state: evidenced_state,
                send: evidenced_send,
                schema: schema_current.is_some(),
            },
            schema_evidence: schema_current.map(|(ev, at)| SchemaEvidenceDto {
                fingerprint: ev.fingerprint,
                command_decisions: ev.command_decisions,
                file_decisions: ev.file_decisions,
                evidenced_at: at,
            }),
            last_failure,
            probe_status: self.probe_status().await,
        }
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

    /// 复审 R-T4：给投影盖**队列暂停事实**（单一盖章点；probe_status 同款纪律
    /// ——镜像层不知道账本，由 adapter 在发布/快照前统一盖）。同步、只读账本。
    /// T02/T03：**影像临界区内的唯一投影入口**——先求值逐操作策略，再投影。
    ///
    /// 为什么在临界区内先求值：`effectiveOperations` 与 `asyncQuestions` 必须来自
    /// 同一份事实（否则会出现"策略说可答、题说运行中"的自相矛盾快照）。
    /// 求值只读内存事实（caps／证据槽／成功证据／镜像预览／暂停事实），
    /// **不读 SQLite、不扫安装包、不跨 await**。
    fn project_mirror(
        &self,
        key: &ThreadKey,
        mirror: &mut super::mirror::ThreadMirror,
        caps: CapabilitySet,
    ) -> ThreadProjection {
        // H02：暂停事实与投影/资格事实在**同一临界区**读取（调用方持镜像锁）——
        // 旧代码在锁外先读、锁内后写事实，旧观察会把新暂停事实覆盖回 false，
        // 让 writer 无越队就放行。锁序仍为 镜像 → 设置/账本单条读（既有纪律）。
        let queue_paused = self.queue_paused_of(key);
        let (runtime_status, has_pending) = mirror.operation_preview();
        let operations = self.compute_operations(runtime_status.clone(), has_pending, queue_paused);
        // R01：越队授权消费后仍要求其余门（运行中/审批/证据）不通过则不予放行。
        let operations_unpaused = self.compute_operations(runtime_status, has_pending, false);
        mirror.set_effective_operations(operations.clone());
        // T03：持久提示的**已装载副本**（锁内不读 DB）；未装载 → 无提示（保守同前）。
        let hints = self
            .inner
            .interaction_hints_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key.thread_id)
            .cloned();
        mirror.set_interaction_hints(hints);
        let mut projection = mirror.project(caps);
        // B01：可用性必须由**同一份逐操作结论**推导，而不是 profile caps 快照。
        // 契约匹配（或兼容档）让 Send 通过后，未知 build 也能进入可发送/可交互
        // 态；自动适配关闭时 send_gate_static 会否掉 Send，结论同步回退只读。
        let send_static_ok = self.send_gate_static().is_ok();
        let mut operations = projection.effective_operations.clone();
        if let Some(status) = operations
            .operations
            .get_mut(&super::capability::CodexOperation::Send)
        {
            if !send_static_ok {
                status.allowed = false;
                status.reason_code = Some("capability_unverified".into());
            }
        }
        projection.effective_operations = operations.clone();
        mirror.set_effective_operations(operations.clone());
        let send_allowed = operations
            .operations
            .get(&super::capability::CodexOperation::Send)
            .is_some_and(|status| status.allowed);
        // 读链可用：镜像已建立（revision 存在）或 profile 明确给出 state。
        let read_capable = caps.state || projection.revision.is_some();
        projection.availability = match (send_allowed, read_capable) {
            (true, true) => super::types::ControlAvailability::Controllable,
            (true, false) => super::types::ControlAvailability::SendOnly,
            (false, true) => super::types::ControlAvailability::ReadOnly,
            (false, false) => super::types::ControlAvailability::CompatOnly,
        };
        let mut facts = InteractionFacts::from_projection(
            &projection,
            operations,
            operations_unpaused,
            queue_paused,
        );
        facts.active_turn_id = mirror.active_turn_id();
        self.store_interaction_facts(key, facts);
        projection
    }

    /// R01：写入资格事实快照（与 claim 共用资格闸——先失效先胜出）。
    fn store_interaction_facts(&self, key: &ThreadKey, facts: InteractionFacts) {
        let _gate = self
            .inner
            .interaction_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.inner
            .interaction_facts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.clone(), facts);
    }

    /// **持资格闸执行**：判定通过后立即在闸内执行 `allowed`（生产 = claim CAS）。
    /// 判定与 claim 之间不允许任何事实更新插入——这就是「资格检查与首次写出权
    /// 竞争绑定」的线性化点。
    /// J01：把本动作的完整组身份绑定到最终写权判定（在取得任何写权之前写入；
    /// 与 `interaction_gate` 的最终裁决配合，不依赖外层预检快照）。
    pub(crate) fn bind_group_action(
        &self,
        action_id: &str,
        source_turn_id: &str,
        source_item_id: &str,
        members: Vec<(String, String)>,
    ) {
        self.inner
            .group_bindings
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                action_id.to_string(),
                GroupBinding {
                    source_turn_id: source_turn_id.to_string(),
                    source_item_id: source_item_id.to_string(),
                    members,
                },
            );
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn with_interaction_verdict<T>(
        &self,
        action_id: &str,
        kind: crate::services::codex_desktop_ipc::types::ActionKind,
        thread_id: &str,
        request_key: Option<&str>,
        context_token: Option<&str>,
        bypass_paused: bool,
        allowed: impl FnOnce() -> T,
    ) -> Result<T, String> {
        let _gate = self
            .inner
            .interaction_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(reason) = self.interaction_verdict_locked(
            action_id,
            kind,
            thread_id,
            request_key,
            context_token,
            bypass_paused,
        ) {
            return Err(reason);
        }
        Ok(allowed())
    }

    /// 判定主体（**必须持资格闸调用**；见 [`Self::with_interaction_verdict`]）。
    fn interaction_verdict_locked(
        &self,
        action_id: &str,
        kind: crate::services::codex_desktop_ipc::types::ActionKind,
        thread_id: &str,
        request_key: Option<&str>,
        context_token: Option<&str>,
        bypass_paused: bool,
    ) -> Option<String> {
        use crate::services::codex_desktop_ipc::types::ActionKind;
        // 只对**交互写**做逐题/逐请求复核；普通消息保持既有发送门语义。
        let operation = match kind {
            ActionKind::UserInput => super::capability::CodexOperation::BlockingInput,
            ActionKind::CommandApproval => super::capability::CodexOperation::CommandApproval,
            ActionKind::FileChangeApproval => super::capability::CodexOperation::FileChangeApproval,
            ActionKind::AsyncQuestion => super::capability::CodexOperation::AsyncAnswerIdle,
            _ => return None,
        };
        let facts = {
            let facts = self
                .inner
                .interaction_facts
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            facts
                .get(&ThreadKey {
                    host_id: "local".into(),
                    thread_id: thread_id.to_string(),
                })
                .cloned()
        };
        let Some(facts) = facts else {
            return Some("stale_context".into());
        };
        if facts.stale {
            return Some("stale_context".into());
        }
        // 暂停：只有显式越队授权才使用"视为未暂停"的策略（其余门同源）。
        let policy = if bypass_paused && facts.queue_paused {
            &facts.operations_unpaused
        } else {
            &facts.operations
        };
        let Some(status) = policy.get(operation) else {
            return Some("operation_not_implemented".into());
        };
        if !status.allowed {
            return Some(
                status
                    .reason_code
                    .clone()
                    .unwrap_or_else(|| "not_actionable".into()),
            );
        }
        match kind {
            ActionKind::AsyncQuestion => {
                let (Some(question_key), Some(token)) = (request_key, context_token) else {
                    return Some("stale_context".into());
                };
                let Some(question) = facts
                    .questions
                    .iter()
                    .find(|question| question.0 == question_key)
                else {
                    // 已不在投影里（已答/已消失/换上下文）→ 零写出。
                    return Some("stale_context".into());
                };
                if question.1 != token {
                    return Some("stale_context".into());
                }
                // 已答/已结束：即使投影还带着旧条目也不得再提交。
                if question.2 == "answered" || question.2 == "ended" || question.2 == "cancelled" {
                    return Some("not_actionable".into());
                }
                // H03：越队授权**只**能放宽"该题仅因 operation_queue_paused 暂不可
                // 提交"。历史缺页 Unknown、stale、内容换代等其他不可提交原因一律
                // 保持拒绝——不能让越队把"不知道为什么不能答"也一起放过。
                if !question.3 {
                    let pause_only = bypass_paused
                        && question.2 == "temporarily_blocked"
                        && question.4.as_deref() == Some("operation_queue_paused");
                    if !pause_only {
                        return Some(
                            question
                                .4
                                .clone()
                                .unwrap_or_else(|| "not_actionable".into()),
                        );
                    }
                }
                // J01：组绑定存在时，**最终写权判定**逐题核对完整成员集合和状态；
                // 这不依赖 hook 返回时的旧快照，而是在 interaction_gate 内读取
                // 最新发布的本地事实，再执行 CAS。
                let binding = self
                    .inner
                    .group_bindings
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(action_id)
                    .cloned();
                if let Some(binding) = binding {
                    let expected: std::collections::BTreeSet<String> =
                        binding.members.iter().map(|(key, _)| key.clone()).collect();
                    for (key, token) in &binding.members {
                        let Some(question) =
                            facts.questions.iter().find(|question| question.0 == *key)
                        else {
                            return Some("stale_context".into());
                        };
                        if question.1 != *token
                            || matches!(
                                question.2.as_str(),
                                "answered" | "ended" | "cancelled" | "rejected"
                            )
                        {
                            return Some("question_already_answered".into());
                        }
                        if !question.3 {
                            let pause_only = bypass_paused
                                && question.2 == "temporarily_blocked"
                                && question.4.as_deref() == Some("operation_queue_paused");
                            if !pause_only {
                                return Some(
                                    question
                                        .4
                                        .clone()
                                        .unwrap_or_else(|| "not_actionable".into()),
                                );
                            }
                        }
                    }
                    let live: std::collections::BTreeSet<String> = facts
                        .questions
                        .iter()
                        .filter(|question| {
                            question.5 == binding.source_turn_id
                                && question.6 == binding.source_item_id
                                && !matches!(
                                    question.2.as_str(),
                                    "answered" | "ended" | "cancelled" | "rejected"
                                )
                        })
                        .map(|question| question.0.clone())
                        .collect();
                    if live != expected {
                        return Some("answers_mismatch".into());
                    }
                }
                None
            }
            _ => {
                let (Some(request_key), Some(token)) = (request_key, context_token) else {
                    return Some("stale_context".into());
                };
                let expected_kind = match kind {
                    ActionKind::CommandApproval => "command_approval",
                    ActionKind::FileChangeApproval => "file_change_approval",
                    ActionKind::UserInput => "user_input",
                    _ => "unsupported",
                };
                let Some(pending) = facts
                    .pending
                    .iter()
                    .find(|pending| pending.0 == request_key)
                else {
                    return Some("stale_context".into());
                };
                if pending.1 != token {
                    return Some("stale_context".into());
                }
                if pending.3 != expected_kind {
                    return Some("action_mismatch".into());
                }
                if !pending.2 {
                    return Some("not_actionable".into());
                }
                None
            }
        }
    }

    /// 由运行态/pending/暂停 + 缓存事实求值策略（纯内存）。
    fn compute_operations(
        &self,
        runtime_status: String,
        blocking_pending: bool,
        queue_paused: bool,
    ) -> EffectiveOperations {
        let context = self.operation_context_from(runtime_status, blocking_pending, queue_paused);
        OperationPolicy.evaluate(&context)
    }

    /// T04：按当前镜像事实求值策略（短锁读预览；不在锁内求值）。
    async fn compute_operations_for(
        &self,
        key: &ThreadKey,
        queue_paused: bool,
    ) -> EffectiveOperations {
        let preview = {
            let mirrors = self.inner.mirrors.lock().await;
            mirrors.get(key).map(|mirror| mirror.operation_preview())
        };
        match preview {
            Some((runtime_status, has_pending)) => {
                self.compute_operations(runtime_status, has_pending, queue_paused)
            }
            None => self.compute_operations("unknown".to_string(), false, queue_paused),
        }
    }

    /// 由事实片段 + 缓存证据组装策略上下文。
    fn operation_context_from(
        &self,
        runtime_status: String,
        blocking_pending: bool,
        queue_paused: bool,
    ) -> OperationContext {
        let mut shipped = std::collections::BTreeMap::new();
        let caps = self.current_caps();
        shipped.insert(CodexOperation::Send, caps.send);
        shipped.insert(CodexOperation::CommandApproval, caps.command);
        // 精确逐能力：file 审批没有独立的 shipped 位——**不**用 command 批量制造
        //（spec §3.2：其他 build 的成功不能平移到这里）。
        shipped.insert(CodexOperation::BlockingInput, caps.blocking_input);
        shipped.insert(CodexOperation::Interrupt, caps.interrupt);

        // H01：成功证据按**当前完整目标**限定（跨 build/包 SHA/规则不通用）。
        let observed_target = self.observed_target();
        let observed = self
            .inner
            .observed_operations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|source| source.observed(observed_target.as_ref()))
            .unwrap_or_default();

        // schema 取证（未知 build 的审批词）也是证据来源之一：与卡片同一策略。
        let approval = self.approval_policy();
        let schema_command = approval
            .for_kind(super::types::PendingKind::CommandApproval)
            .iter()
            .any(|word| word == super::pending::DECISION_ACCEPT);
        let schema_file = approval
            .for_kind(super::types::PendingKind::FileChangeApproval)
            .iter()
            .any(|word| word == super::pending::DECISION_ACCEPT);
        let mut schema = std::collections::BTreeMap::new();
        schema.insert(CodexOperation::CommandApproval, schema_command);
        schema.insert(CodexOperation::FileChangeApproval, schema_file);

        let (contract, contract_error) = self.current_contract_facts();

        // 目标新鲜度 = **连接在场**（内存事实；不 try_lock 造成抖动）。
        // 为什么不再要求 probe_target：shipped 证据档的会话本来就没有探测目标，把
        // "可写"绑到探测目标上会让已验证 build 的审批/问答被误判为 stale。镜像
        // stale（断链/换代）由投影路径的 `stale` 字段另行撤回动作。
        let target_fresh = self.current_conn_alive();
        OperationContext {
            target_fresh,
            auto_adapt_enabled: self.auto_adapt_enabled(),
            runtime_status,
            blocking_pending,
            queue_paused,
            read_evidence: self.state_evidence_current(),
            shipped,
            observed,
            schema,
            contract,
            contract_error,
        }
    }

    /// 连接是否在场（内存判据，不 try_lock：并发发布时不得抖成"不可用"）。
    fn current_conn_alive(&self) -> bool {
        match self.inner.conn.try_lock() {
            Ok(guard) => guard.as_ref().is_some_and(|conn| !conn.is_closed()),
            // 锁被别的任务暂持：连接状态未知 → 保守按"在场"（stale 判据另有兜底），
            // 避免瞬时策略抖动引发无意义重发布。
            Err(_) => true,
        }
    }

    /// 当前契约事实（证据槽命中当前代际+目标才采纳；否则只可能给出失败码）。
    fn current_contract_facts(
        &self,
    ) -> (
        std::collections::BTreeMap<CodexOperation, super::capability::ContractMatch>,
        Option<String>,
    ) {
        let epoch = self.inner.epoch.load(Ordering::SeqCst);
        let target = self.probe_target_snapshot();
        let slot = self
            .inner
            .contract_evidence
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some((slot_epoch, slot_target, evidence)) = slot {
            if slot_epoch == epoch && slot_target == target {
                return (evidence.matches, None);
            }
        }
        let failure = self
            .inner
            .contract_failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        match failure {
            Some((failure_epoch, _key, code)) if failure_epoch == epoch => {
                (std::collections::BTreeMap::new(), Some(code))
            }
            _ => (std::collections::BTreeMap::new(), None),
        }
    }

    /// T02 测试缝：注入契约读取器（生产缺省 = 真实包只读解析）。
    #[doc(hidden)]
    pub fn set_contract_reader(&self, reader: Option<Arc<dyn DesktopContractReader>>) {
        *self
            .inner
            .contract_reader
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = reader;
    }

    /// T02：注入契约证据持久存储（生产 = DB DAO）。
    pub fn set_contract_evidence_store(&self, store: Option<Arc<dyn ContractEvidenceStore>>) {
        *self
            .inner
            .contract_store
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = store;
    }

    /// T02：注入本机真实成功证据来源（生产由 remote 层注入）。
    pub fn set_observed_operations_source(
        &self,
        source: Option<Arc<dyn ObservedOperationsSource>>,
    ) {
        *self
            .inner
            .observed_operations
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = source;
    }

    /// T03：注入异步题持久提示存储（生产 = DB DAO）。
    pub fn set_interaction_hint_store(
        &self,
        store: Option<Arc<dyn super::async_question::InteractionHintStore>>,
    ) {
        *self
            .inner
            .interaction_hint_store
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = store;
    }

    /// T03：装载某线程的持久提示（**每线程一次**；DB 读取在镜像锁外）。
    ///
    /// 读失败 / 无存储 → 不缓存（保持 Unknown 的保守语义；不合成"已答"）。
    pub async fn ensure_interaction_hints(&self, key: &ThreadKey) {
        {
            let cache = self
                .inner
                .interaction_hints_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if cache.contains_key(&key.thread_id) {
                return;
            }
        }
        let store = self
            .inner
            .interaction_hint_store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(store) = store else {
            return;
        };
        let thread_id = key.thread_id.clone();
        let loaded = tokio::task::spawn_blocking(move || store.load(&thread_id))
            .await
            .ok()
            .flatten();
        let Some(rows) = loaded else {
            return;
        };
        let hints: Vec<super::async_question::InteractionHint> = rows
            .into_iter()
            .map(|row| super::async_question::InteractionHint {
                thread_id: row.thread_id,
                source_turn_id: row.source_turn_id,
                source_item_id: row.source_item_id,
                question_index: row.question_index,
                last_fact: row.last_fact,
            })
            .collect();
        self.inner
            .interaction_hints_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.thread_id.clone(), Arc::new(hints));
    }

    /// R18：把投影里的题目事实写穿到持久提示（**仅变化时落库**；无存储/无题即跳过）。
    ///
    /// 只写"事实"（answered/pending/unknown），不写题干/答案/信封。用途：重启后相关
    /// 历史未加载时仍能给「已知但未核对」的题一个占位，而不是把"没看到"当"没答过"。
    async fn persist_interaction_hints(
        &self,
        key: &ThreadKey,
        questions: &[super::async_question::AsyncQuestionView],
    ) {
        if questions.is_empty() {
            return;
        }
        let store = self
            .inner
            .interaction_hint_store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let Some(store) = store else {
            return;
        };
        let mut changed: Vec<super::async_question::InteractionHint> = Vec::new();
        {
            let cache = self
                .inner
                .interaction_hints_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let existing = cache.get(&key.thread_id);
            for question in questions {
                let fact = match question.state {
                    super::async_question::InteractionState::Answered => "answered",
                    super::async_question::InteractionState::Unknown => "unknown",
                    _ => "pending",
                };
                let unchanged = existing.is_some_and(|hints| {
                    hints.iter().any(|hint| {
                        hint.thread_id == key.thread_id
                            && hint.source_turn_id == question.source_turn_id
                            && hint.source_item_id == question.source_item_id
                            && hint.question_index == question.question_index
                            && hint.last_fact == fact
                    })
                });
                if !unchanged {
                    changed.push(super::async_question::InteractionHint {
                        thread_id: key.thread_id.clone(),
                        source_turn_id: question.source_turn_id.clone(),
                        source_item_id: question.source_item_id.clone(),
                        question_index: question.question_index,
                        last_fact: fact.to_string(),
                    });
                }
            }
        }
        if changed.is_empty() {
            return;
        }
        let rows = changed.clone();
        let at = chrono::Utc::now().timestamp_millis();
        let written = rows.clone();
        // 落库在阻塞池（不持任何锁、不阻塞投影路径）；失败由存储自身记日志。
        let _ = tokio::task::spawn_blocking(move || {
            for hint in &written {
                store.upsert(hint, at);
            }
        })
        .await;
        // 写穿缓存（下一轮比较据此去重）。
        let mut cache = self
            .inner
            .interaction_hints_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let entry = cache
            .entry(key.thread_id.clone())
            .or_insert_with(|| Arc::new(Vec::new()));
        let mut merged = (**entry).clone();
        for hint in rows {
            if let Some(slot) = merged.iter_mut().find(|existing| {
                existing.source_turn_id == hint.source_turn_id
                    && existing.source_item_id == hint.source_item_id
                    && existing.question_index == hint.question_index
            }) {
                *slot = hint;
            } else {
                merged.push(hint);
            }
        }
        *entry = Arc::new(merged);
    }

    /// T02 测试缝：**同步**安装一份契约证据（绑定当前代际与探测目标）。
    ///
    /// 集成测试的 fixture 是同步构造的（不能 await `ensure_contract_evidence`）；
    /// 生产代码只经 [`Self::ensure_contract_evidence`] 的读取+守卫路径安装。
    #[doc(hidden)]
    pub fn set_contract_evidence_snapshot(
        &self,
        evidence: super::capability::DesktopContractEvidence,
    ) {
        let epoch = self.inner.epoch.load(Ordering::SeqCst);
        let target = self.probe_target_snapshot();
        *self
            .inner
            .contract_evidence
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some((epoch, target, evidence));
    }

    /// T02 测试缝：契约安装定序窗（生产恒 None）。
    #[doc(hidden)]
    pub fn set_contract_install_gate(&self, gate: Option<Box<dyn Fn() + Send + Sync>>) {
        *self
            .inner
            .contract_install_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = gate;
    }

    /// T02 测试缝：固定契约目标（生产恒 None → 官方运行包定位）。
    #[doc(hidden)]
    pub fn set_contract_target_override(&self, target: Option<ContractTarget>) {
        *self
            .inner
            .contract_target_override
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = target;
    }

    fn run_contract_install_gate(&self) {
        let gate = self
            .inner
            .contract_install_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(gate) = gate {
            gate();
        }
    }

    /// T02：把证据装进**读取发起时捕获的身份**（旧代际/旧目标的迟到结果不安装、
    /// 不落账）。返回是否安装。
    fn install_contract_evidence(
        &self,
        captured: &ProbeTarget,
        probe_key: &str,
        evidence: DesktopContractEvidence,
    ) -> bool {
        // 注意：`with_current_probe_target` 内部持 `probe_target` 锁执行闭包——
        // 闭包内**不得**再调 `probe_target_snapshot()`（同锁重入 = 自锁死）。
        // 槽位里存发起时捕获的同一份目标（守卫已保证它仍是当前目标）。
        self.with_current_probe_target(captured, || {
            *self
                .inner
                .package_sha
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = Some(PackageIdentity {
                epoch: captured.epoch,
                probe_key: probe_key.to_string(),
                sha256: evidence.package_sha256.clone(),
            });
            *self
                .inner
                .contract_evidence
                .lock()
                .unwrap_or_else(|e| e.into_inner()) =
                Some((captured.epoch, Some(captured.clone()), evidence));
            *self
                .inner
                .contract_failure
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = None;
        })
        .is_some()
    }

    /// T02：逐操作有效集合（异步入口：先用已装载/持久证据，缺则只读识别一次）。
    ///
    /// 与投影路径同一算法：这里返回的集合就是投影里的 `effectiveOperations`
    /// （调用方不自行拼按钮）。
    /// **不触发包扫描**：契约识别由连接建立/会话打开路径按需触发（单飞、幂等），
    /// 端点门/队列视图只消费**当前已装载**的事实（避免每次轮询重读安装包）。
    pub async fn effective_operations(&self, key: &ThreadKey) -> EffectiveOperations {
        self.projection(key).await.effective_operations
    }

    /// T02：按需识别当前运行包的逐操作契约（幂等；单飞；取消安全）。
    ///
    /// 顺序（每一步都重新核对身份才有资格继续）：
    /// 1. 自动适配关闭 → 不取新证据（已装证据保留，活门在策略里压回）；
    /// 2. 取当前探测目标与完整指纹（拿不到 → 不识别、不落账）；
    /// 3. 证据槽已命中当前代际+目标 → 直接返回（零重读）；
    /// 4. **持久证据命中 → 零重读装回**（重启恢复路径；同键多包指纹 → 不猜）；
    /// 5. 单飞识别（取消即释放；失败记阶段/错误码，不抹历史成功）；
    /// 6. 身份守卫下安装 + 落账 + 重发布（无 Desktop 新帧也能推达 UI）。
    pub async fn ensure_contract_evidence(&self) {
        if !self.auto_adapt_enabled() {
            return;
        }
        let Some(target) = self.probe_target_snapshot() else {
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
        // 槽命中：同一代际 + 同一目标 + 同一指纹 → 零重读。
        if self.contract_slot_matches(epoch, &probe_key) {
            return;
        }
        // 持久证据：重启/重连零重读装回（失败观测不抹成功）。
        // R17：复用前**核对当前包内容**（大小 + mtime）——同 build/CLI 但包被替换时
        // 旧结论不得安装；老行未记录指纹 → 同样不复用（重新只读识别）。
        let current_package = self
            .contract_target_for(&probe_key, epoch)
            .ok()
            .and_then(|target| std::fs::metadata(&target.package_path).ok())
            .and_then(|meta| {
                let mtime = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as i64);
                mtime.map(|mtime| (meta.len(), mtime))
            });
        // H14：stat 只是快速预筛；**内容哈希**一致才允许复用缓存结论。
        let current_sha = if current_package.is_some() {
            let package_path = self
                .contract_target_for(&probe_key, epoch)
                .ok()
                .map(|target| target.package_path);
            match package_path {
                Some(path) => tokio::task::spawn_blocking(move || {
                    super::probe::desktop_contract::hash_package(&path)
                })
                .await
                .ok()
                .and_then(Result::ok),
                None => None,
            }
        } else {
            None
        };
        if let Some(rows) = self
            .contract_store_ref()
            .and_then(|store| store.load(&probe_key, CONTRACT_RULE_VERSION))
        {
            if let Some(evidence) = DesktopContractEvidence::from_rows(&rows, CONTRACT_RULE_VERSION)
                .and_then(|evidence| evidence.pinned_to_current_package(current_package))
                .filter(|evidence| current_sha.as_deref() == Some(evidence.package_sha256.as_str()))
            {
                self.run_contract_install_gate();
                let captured = bound_target.clone().unwrap_or_else(|| target.clone());
                if !self.install_contract_evidence(&captured, &probe_key, evidence) {
                    return;
                }
                *self
                    .inner
                    .contract_attempted
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some((epoch, probe_key));
                self.republish_all_projections().await;
                return;
            }
        }
        // 单飞：等待者与后来的调用在同一把锁上排队；任务取消即释放。
        let _guard = self.inner.contract_inflight.lock().await;
        // 双检：等待期间可能已有别的任务装好。
        if self.contract_slot_matches(epoch, &probe_key) || self.auto_adapt_disabled_now() {
            return;
        }
        let attempted = self
            .inner
            .contract_attempted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if attempted.as_ref() == Some(&(epoch, probe_key.clone())) {
            return;
        }
        let contract_target = match self.contract_target_for(&probe_key, epoch) {
            Ok(target) => target,
            Err(error) => {
                self.record_contract_failure(epoch, &probe_key, &error);
                return;
            }
        };
        let reader = self
            .inner
            .contract_reader
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_else(|| Arc::new(ProductionContractReader));
        let evidence = reader.read(contract_target).await;
        // 身份守卫：读取返回后、安装前（窗内换代/换目标 → 拒绝）。
        self.run_contract_install_gate();
        let captured = bound_target.clone().unwrap_or_else(|| target.clone());
        match evidence {
            Ok(evidence) => {
                if !self.install_contract_evidence(&captured, &probe_key, evidence.clone()) {
                    log::info!(
                        "codex-ipc contract: key={probe_key} 证据到达时身份已换代，拒收（不落账）"
                    );
                    return;
                }
                *self
                    .inner
                    .contract_attempted
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some((epoch, probe_key.clone()));
                if let Some(store) = self.contract_store_ref() {
                    store.store(&evidence, chrono::Utc::now().timestamp_millis());
                }
                log::info!(
                    "codex-ipc contract: key={probe_key} sha={} matches={:?}",
                    evidence.package_sha256,
                    evidence
                        .matches
                        .iter()
                        .map(|(op, m)| (*op, m.base_version))
                        .collect::<Vec<_>>()
                );
                self.republish_all_projections().await;
            }
            Err(error) => {
                self.record_contract_failure(epoch, &probe_key, &error);
            }
        }
    }

    fn auto_adapt_disabled_now(&self) -> bool {
        !self.auto_adapt_enabled()
    }

    fn contract_store_ref(&self) -> Option<Arc<dyn ContractEvidenceStore>> {
        self.inner
            .contract_store
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn contract_slot_matches(&self, epoch: u64, probe_key: &str) -> bool {
        if self.inner.epoch.load(Ordering::SeqCst) != epoch {
            return false;
        }
        let Some(current) = self.probe_target_snapshot() else {
            return false;
        };
        let Some(key) = current.key.as_deref() else {
            return false;
        };
        if key != probe_key {
            return false;
        }
        let slot = self
            .inner
            .contract_evidence
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        matches!(slot, Some((slot_epoch, slot_target, _)) if slot_epoch == epoch && slot_target == Some(current))
    }

    fn record_contract_failure(&self, epoch: u64, probe_key: &str, error: &ProtocolError) {
        let code = super::probe::desktop_contract::contract_error_code(error);
        log::info!("codex-ipc contract: key={probe_key} 识别失败（{code}），对应动作保持关闭");
        *self
            .inner
            .contract_attempted
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some((epoch, probe_key.to_string()));
        *self
            .inner
            .contract_failure
            .lock()
            .unwrap_or_else(|e| e.into_inner()) =
            Some((epoch, probe_key.to_string(), code.to_string()));
    }

    fn contract_target_for(
        &self,
        probe_key: &str,
        epoch: u64,
    ) -> Result<ContractTarget, ProtocolError> {
        if let Some(target) = self
            .inner
            .contract_target_override
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return Ok(target);
        }
        let target = self.probe_target_snapshot().ok_or_else(|| {
            ProtocolError::InvalidState("no probe target for contract discovery".into())
        })?;
        // 用**已解析的完整指纹**构造 ProbeKey（含 cli_id 段）：契约证据的复用键
        // 必须与账本同键，否则同一 build 会因缺 cli_id 被当成两个目标。
        let mut parts = probe_key.split('|');
        let platform = parts.next().unwrap_or(&target.platform).to_string();
        let build = parts.next().filter(|part| *part != "-").map(str::to_string);
        let cli = parts.next().filter(|part| *part != "-").map(str::to_string);
        let key = super::probe::fingerprint::ProbeKey::new(
            &platform,
            build.as_deref().or(Some(target.build.as_str())),
            cli.as_deref(),
        );
        super::probe::desktop_contract::resolve_contract_target_for(&platform, &key, epoch)
    }

    fn stamp_queue_pause(&self, key: &ThreadKey, mut p: ThreadProjection) -> ThreadProjection {
        p.queue_paused = self.queue_paused_of(key);
        p
    }

    /// 该 Thread 是否处于队列暂停（EXISTS 任意行；读失败按 true 保守——与
    /// router 的 `queue_paused_or_fallback` 同口径；未装配事实源 = false）。
    fn queue_paused_of(&self, key: &ThreadKey) -> bool {
        let src = self
            .inner
            .queue_pause_source
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        match src {
            Some(f) => f(&key.thread_id),
            None => false,
        }
    }

    /// 复审 R-T4：**单线程**投影重发布（暂停变化点触发：前置落行/撤回/resume/
    /// settle）——发布序号经 CAS 预留（round 5 W1），SSE 下游按 F5 决胜穿透收到
    /// `queuePaused` 翻转帧。无镜像的 key 跳过（不发布 stale 空投影，与
    /// `republish_all_projections` 同纪律；恢复出口 = 连接复活后的状态帧流量）。
    ///
    /// 复审 round 4（V1）：**按实际状态变化发布**——本路径发布的语义事实只有
    /// `queuePaused`（revision 等其余字段由 Desktop 状态帧路径自然推进）。与
    /// [`Self::pause_published`] 记录的上次发布值相同 → 跳过发布：调度循环消费
    /// 自身通知 / 周期对账反复结算同一线程时，不得因暂停事实不变而再次发布
    ///（旧实现的无界自激循环：settle→republish→新 local_seq→settle→…）。真实
    /// 翻转（落行 true / 撤回·resume false）与「镜像缺席错过窗口后的首次收敛」
    /// （tracker 无记录）照常发布——resume 的反向通知不受影响。
    ///
    /// 复审 round 7（Q1）：`projection()` 盖章的 `queue_paused` 只是**先前观察**，
    /// 不再参与去重判定——事实以 grant 临界区内的重读值为准（见
    /// [`Self::pause_publish_grant`]），广播内容同样是重核值。同值恢复（观察后、
    /// grant 前真实 resume 清行）由此正确收敛为静默，旧观察无法复活成 stale true。
    pub async fn republish_thread_projection(&self, key: &ThreadKey) {
        if !self.has_mirror(key).await {
            return;
        }
        let p = self.projection(key).await;
        // 复审 round 4（V3）：同 `republish_all_projections`——观察被后续发布越过
        // 时重观察，暂停事实以重观察值为准做去重判定。
        let p = self.refresh_if_projection_stale(key, p).await;
        // 复审 round 7（Q1）测试缝：观察与 refresh 全部完成、grant 循环之前的定序
        // 窗（生产恒 None 直通）。窗内完成的同值恢复等事实变更，不得被缝前旧观察
        // 的提交越过。
        self.run_pause_observe_gate();
        // 复审 round 5（W1）：同全入口协议——CAS 预留发布序号，观察被越过 →
        // 重观察后再预留（重试有界且只由别人的真实发布驱动，不构成自激环）；
        // tracker 在实际取得发布资格后才提交（round 4 的「检查后即记录」会让
        // 放弃的尝试留下未发布值的假记录，后续同值真实发布会被误抑制）。
        // 复审 round 6（Q1-W1 收口）：去重判定、CAS 预留与 tracker 登记**同一
        // std::Mutex 临界区**（无 await，可短临界区持有）——旧实现三步分离，
        // 「资格已预留、tracker 未登记」的跨线程窗里，并发 resume 的去重判定读
        // 不到待提交值而误抑制恢复事实；旧暂停值随后照旧广播，DB 已恢复而投影
        // 说暂停，第三次真实变化反被去重吞掉（Q1② 红测窗口）。
        // 复审 round 7（Q1）：去重判定的事实输入改为 grant 临界区内重读
        // （`p.queue_paused` 只是先前观察，不传入）——观察后、提交前完成的同值
        // 恢复不再能被旧观察越过（红测
        // `sse_pause_stale_observation_cannot_survive_same_value_resume`）。
        let mut p = p;
        let mut retries = 0u32;
        loop {
            match self.pause_publish_grant(key, p.local_seq) {
                PausePublishGrant::Deduped => return,
                PausePublishGrant::Granted(seq, paused) => {
                    // 复审 round 7（Q1）：广播内容 = grant 临界区内的重核值（不是
                    // 本函数先前的观察值）——发布的事实与去重判定/tracker 登记同源。
                    p.queue_paused = paused;
                    // 复审 round 6（Q1-W1 收口）测试缝：**资格已取得、tracker 已
                    // 登记**（同一临界区完成后的合法定序点；生产恒 None 直通）。
                    self.run_pause_reserve_gate();
                    self.publish_at_seq(p, seq).await;
                    return;
                }
                PausePublishGrant::Conflict if retries >= Self::PUBLISH_REOBSERVE_MAX => return,
                PausePublishGrant::Conflict => {
                    retries += 1;
                    p = self.projection(key).await;
                }
            }
        }
    }

    /// 复审 round 6（Q1-W1 收口）：**去重判定 + 发布资格预留 + tracker 登记**的
    /// 原子段。三步在同一 `pause_published` 短临界区内完成（CAS 是纯原子操作、
    /// 无 await，同步短临界区合法；该锁只被本路径使用、无反向持有者，无新锁序）：
    /// 并发同路径发布者要么整体看到登记前状态、要么整体看到登记后状态，不再能
    /// 观察到「资格已预留、tracker 未登记」的中间态（Q1② 窗口闭合）。CAS 失败
    /// 不登记（W1：不留假记录）。
    ///
    /// 复审 round 7（Q1）：**暂停事实在本临界区内重读**（[`Self::queue_paused_of`]
    /// ，同步 SQLite 读），去重、登记与返回值一律以重读值为准——调用方先前的
    /// 观察（`projection()` 内 `stamp_queue_pause` 盖章）到本 grant 之间隔着 await
    /// 点与多线程切换，同值恢复（真实 resume 清行）可在该窗内完成，旧观察不再
    /// 代表当前事实；发布头只证明「已发布位置」，不证明暂停事实未变。事实读取
    /// 与去重判定/预留/登记由此同一原子边界，正确性论证：
    ///
    /// * 每个真实 DB 变更点（打断前置落行 / 撤回 / resume 清行）之后，同一 actor
    ///   **顺序**调用 `republish_thread_projection`（写→通知串行）；最后一次执行
    ///   到 grant 的重发布在锁内重读 DB ⇒ 要么广播当前事实、要么因「重读值 ==
    ///   上次广播值」正确去重——旧观察不再参与暂停事实判断，stale true 无法经
    ///   CAS 合法提交（settle 因 paused=false 不再主动重发布，也不能靠它纠偏）。
    /// * `Granted` 携带重读值，调用方在 `publish_at_seq` 前把它盖进投影——广播
    ///   内容必须是重核值。
    ///
    /// 锁序（round 7 审计结论，无反向边、无死锁环）：`pause_published` →
    /// `queue_pause_source`（短暂 clone 后即释锁）→ DB 连接（`DeviceStore::with`
    /// 闭包内，出闭包即释放）。全库无任何路径**持有** `queue_pause_source` 或
    /// DB 连接反向调用本方法：resume 端点（`clear_queue_pause` 返回后才
    /// `republish_thread_projection`）、interrupt 撤回臂（`clear_queue_pause_
    /// owned` 同理）、settle（`queue_pause` 读毕语句即结束）各写点均在调用
    /// republish 之前完成并释放连接锁；`pause_published` 全库仅本方法使用。
    fn pause_publish_grant(&self, key: &ThreadKey, observed: u64) -> PausePublishGrant {
        let mut st = self
            .inner
            .pause_published
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // 复审 round 7（Q1）：以受保护提交边界的重读值为准（未装配事实源 = false，
        // 与 `queue_paused_of` 缺省口径一致）。
        let paused = self.queue_paused_of(key);
        if st.get(key).is_some_and(|prev| *prev == paused) {
            return PausePublishGrant::Deduped;
        }
        match self.reserve_publish_seq(observed) {
            Some(seq) => {
                st.insert(key.clone(), paused);
                PausePublishGrant::Granted(seq, paused)
            }
            None => PausePublishGrant::Conflict,
        }
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

    /// **首次 `state` 访问触发的只读探测**（#172；S1 改判据：看**证据**不看能力位）。
    ///
    /// 顺序（都不得阻断读路径）：
    /// 1. 本进程已确立读链证据（账本命中 / 探测通过，按连接代际 + build 目标隔离）
    ///    → 直接返回。兼容档 `state=true` 只是「可尝试」，**不能**再当「已取证」
    ///    短路（S0 §3.1 裁决：两维度分列）；
    /// 2. 非「尚无证据」档（closed / 不可达）→ 不探；
    /// 3. 自动适配关闭 → 不**自动**探（设计 §3.3；手动重探是显式入口，后续任务接线）；
    /// 4. 算/取指纹键（同 build 只算一次）→ 查账本：
    ///    * 有成功证据（含「成功后失败」的行）→ 确立证据，**零帧**复用；
    ///    * `failed` → 本 build 已判死（明确协议错），**本轮不探**（换 build /
    ///      `PROBE_EPOCH` 变更 / 用户显式重探才会再来）；
    ///    * `inconclusive` 未到重试点 → 本轮不探（到点后才重试）；
    /// 5. 探测（P0 `ensure_connection` + P1–P3）→ 落账本；通过才确立证据并**原地
    ///    重发布**已跟随线程（「已适配」投影随即生效）。
    ///
    /// 探测失败**不阻断**后续读路径（follow 照常尝试，读不通自有别的原因）。
    pub async fn ensure_read_evidence(&self, key: &ThreadKey) {
        if self.state_evidence_current() {
            return; // 本进程已确立证据（同代零帧复用）
        }
        if !self.probe_eligible() {
            return; // closed / 不可达：不探（判死档不因触发而复活）
        }
        if !self.auto_adapt_enabled() {
            return; // 关闭自动适配：阻止新的未知 build 自动探测（设计 §3.3）
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
        if matches!(outcome, ProbeOutcome::Probed { .. }) {
            // S1：探测通过 = 成功证据确立（账本命中同一把钥匙）＋ 已跟随线程原地
            // 重发布（探测只读事实「已适配」不靠下一次状态变化才生效）。
            self.mark_state_evidenced(&target);
            self.republish_all_projections().await;
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
            // S1：命中判据 = **有成功证据**（`evidenced`：证据列或旧数据兼容的
            // state∈probed|verified）——「成功后失败」的行照样复用，不再重探
            //（设计 §3.2：成功证据不因后续超时/断链失效）。
            if entry.evidenced {
                log::info!("codex-ipc probe: 账本命中，零帧复用（key={probe_key}）");
                self.set_state_capability(true);
                // 身份守卫内写旗标：用无复核版（probe_target 锁已由外层持有，
                // mark_state_evidenced 在这里再取同一把锁会自锁死）。
                self.store_state_evidence(target);
                return true;
            }
            match entry.state {
                crate::database::dao::codex_ipc_probe::ProbeState::Probed
                | crate::database::dao::codex_ipc_probe::ProbeState::Verified => {
                    log::info!("codex-ipc probe: 账本命中，零帧复用（key={probe_key}）");
                    self.set_state_capability(true);
                    self.store_state_evidence(target);
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
    ///
    /// 复审 round 5（W1/V3 收口）：本方法退位为「按已预留序号广播」的底层——
    /// 序号只能来自 [`Self::reserve_publish_seq`] 的成功预留；全部发布入口经
    /// [`Self::publish_fresh_projection`]（内容版本协议），不再有无条件
    /// fetch_add 的发布点（旧 `publish_event` 已删除）。
    async fn publish_at_seq(&self, mut p: ThreadProjection, seq: u64) {
        // T02：`effectiveOperations` 由镜像临界区内的 `project_mirror` 求值并写入
        // 投影本体（与 `asyncQuestions` 同一份事实），发布点不再二次覆盖。
        p.local_seq = seq;
        let _ = self.inner.events.send(p);
    }

    /// 复审 round 5（W1/V3 收口）：**发布资格预留（CAS）**——内容版本协议的核心。
    ///
    /// 以 `observed`（投影内容生成时镜像临界区内的盖章 = `seal_snapshot_seq()`）
    /// 为比较值对发布头做 compare_exchange：成功 = 自观察以来无任何发布越过本
    /// 观察 → 原子取得该内容的发布资格并**预留下一版**（返回 `Some(observed+1)`，
    /// 即发布序号）；失败 = 观察已被后续发布越过 → 返回 `None`，调用方**重观察
    /// 或放弃**。「检查通过」与「取得发布资格」由此成为同一个原子步骤——复审
    /// W1 指认的「refresh 通过后仍无条件 fetch_add」两步窗（republish 检查后 /
    /// state-changed 等待窗后）不再能把旧观察发布成更大序号；传输到达顺序不给
    /// 旧内容升级资格，内容版本轴与传输顺序分离的原则保持（序号表示重观察
    /// 所见而非发送时刻）。无新增锁（单条 CAS）。
    fn reserve_publish_seq(&self, observed: u64) -> Option<u64> {
        self.inner
            .projection_seq
            .compare_exchange(observed, observed + 1, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| observed + 1)
    }

    /// 重观察重试上限。每次重试都对应「别的发布者真实推进了发布头」（CAS 失败
    /// 的唯一原因），不由自身发布驱动、无反馈回路——有界即不构成自激环；耗尽
    /// 放弃是保守出口（绝不以更晚序号发布更旧内容）。8 连续并发发布越过重观察
    /// 窗在单机桌面量级不可达。
    const PUBLISH_REOBSERVE_MAX: u32 = 8;

    /// 复审 round 5（W1/V3 收口）：**全发布入口统一的内容版本协议**——CAS 预留
    /// 加有界重观察。观察被越过 → `projection()` 重观察（新内容的盖章即当前头，
    /// 内容/序号/账本事实同源）后再预留；重试只在别的发布者真实推进了发布头时
    /// 发生，耗尽即放弃。适用于无内容级去重门的入口（去重门在
    /// `republish_thread_projection` 内联循环处理）。
    async fn publish_fresh_projection(&self, key: &ThreadKey, p: ThreadProjection) -> bool {
        let mut p = p;
        let mut retries = 0u32;
        loop {
            if let Some(seq) = self.reserve_publish_seq(p.local_seq) {
                self.publish_at_seq(p, seq).await;
                return true;
            }
            if retries >= Self::PUBLISH_REOBSERVE_MAX {
                return false;
            }
            retries += 1;
            p = self.projection(key).await;
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ThreadProjection> {
        self.inner.events.subscribe()
    }

    /// 终审 R5/Gate F：快照盖章单点——返回当前投影序号基线（**load，不递增**）。
    ///
    /// 必须在**镜像锁临界区内**调用：本 load 之后完成的镜像 mutation + 发布，其
    /// fetch_add 必然晚于本 load（mutation 需要同一把锁）→ 其 seq 严格大于盖章值，
    /// 不会被合并门/supersedes 误丢；反之任何 seq ≤ 盖章值的帧，其 mutation 可证
    /// 先于快照读取（内容已包含或已被更晚策略覆盖）——TOCTOU 小窗由此闭合。
    /// `projection()` 与 `fresh_projection` 两条快路径共用本 helper，不再分叉。
    fn seal_snapshot_seq(&self) -> u64 {
        self.inner.projection_seq.load(Ordering::SeqCst)
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
        // endpoint → profile（R3：登记 build → 证据档；未知 build → 兼容发送档
        // （send/state 开，2026-10-22 用户裁决）；空 build/无 endpoint → closed）。
        // S1（设计 §3.3）：自动适配**关闭**时未知 build 装「只读可尝试」档
        // （发送关；已验证 build 不受影响，按已验证能力运行）。
        let profile = match super::profile::profile_for(
            &endpoint.platform,
            endpoint.build.as_deref().unwrap_or(""),
        ) {
            Some(p) => p,
            None if !endpoint.build.as_deref().unwrap_or("").is_empty()
                && !self.auto_adapt_enabled() =>
            {
                super::profile::compat_readonly_profile(
                    &endpoint.platform,
                    endpoint.build.as_deref().unwrap_or(""),
                )
            }
            None => super::profile::profile_for_or_compat(
                &endpoint.platform,
                endpoint.build.as_deref().unwrap_or(""),
            ),
        };
        // S2 复核（原「按已验证记 true」语义随兜底臂废除）：留存的是**当前档位的
        // send 能力位快照**（不看连接存活；连接失败路径不覆盖）。它不再声称「已
        // 验证」——未知 build 的兼容档 send 开只表达「可尝试」；消费者见字段注释。
        *self
            .inner
            .profile_send_capable
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(profile.capabilities.send);
        // Gate A #2：同点留存证据档（已取证不随断链失忆，见字段注释）。
        *self
            .inner
            .last_resolved_evidence
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(profile.evidence);
        // #172 debug 专用强制开关：把任何档压回「尚无证据」档（能力位全关，可
        // 探测），真机才能跑到未知 build 的探测分支；release 构建恒不触发。
        let forced_unverified = force_unverified();
        let build = endpoint.build.clone().unwrap_or_default();
        let profile = if forced_unverified {
            ProtocolProfile::unverified(&endpoint.platform, &build)
        } else {
            profile
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
            let conn = super::connection::spawn_with_tuning(
                io,
                profile.clone(),
                epoch,
                self.conn_tuning(),
                self.inner.late.clone(),
            );
            // 终审 R2：写右判定随连接注入（每次新连接重新转发——连接级缝逐连接）
            {
                let checker = self
                    .inner
                    .write_right_check
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                conn.set_write_right_check(checker);
                // 复审 R-T1：claim（取得首次写出权）随连接注入——同一装配纪律
                let claim = self
                    .inner
                    .write_right_claim
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                conn.set_write_right_claim(claim);
            }
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
            // T02：进入会话后按需识别当前包契约（只读、单飞、失败不判死）；
            // 旧代际迟到结果由安装守卫拒绝。
            adapter_for_schema.ensure_contract_evidence().await;
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
    /// #172/T9a：探测只读事实在镜像锁**之外**再盖（读账本/单飞器，不得持镜像锁）。
    async fn fresh_projection(&self, key: &ThreadKey) -> Option<ThreadProjection> {
        // T3 红测缝：旧代码 caps 锁外先读后、镜像锁前的窗口（生产恒 None 直通）。
        // 修复后 caps 在临界区内读取，本缝退化为「先跑窗口动作再取快照」的无害定序。
        self.run_fresh_snapshot_gate();
        let mut p = {
            let mut mirrors = self.inner.mirrors.lock().await;
            // T3（复审 round 3）：caps 与盖章**同一临界区**——快路径首快照的内容
            // 同样必须是序号实际观察到的版本。Gate F Nit ②「设置 DB 读留在锁外」
            // 在快照路径上作废（内容与序号一致优先，见 [`Self::projection`] 同款
            // 论证；发布路径不受影响）。
            let caps = self.effective_projection_caps();
            let p = mirrors
                .get_mut(key)
                .filter(|m| !m.is_stale() && m.revision().is_some())
                .map(|m| self.project_mirror(key, m, caps));
            // Gate F Major 1：快路径首快照同样盖章（临界区内，见 seal_snapshot_seq）
            //——第二订阅者/重连与事件共享同一条本地排序轴。
            p.map(|mut p| {
                p.local_seq = self.seal_snapshot_seq();
                p
            })
        };
        match p.as_mut() {
            Some(_) => {
                let p = p.take().expect("just checked");
                // 复审 R-T4：快路径同样盖队列暂停事实（与 projection() 同源）
                let p = self.stamp_queue_pause(key, p);
                Some(self.stamp_probe_status(p).await)
            }
            None => None,
        }
    }

    /// 当前镜像里处于 `inProgress` 的活动回合 id（2026-10-22 原生回合打断扩展）：
    /// 端点打断阀值的回退来源——在册屏障不存在（回合由桌面手打产生）时，用它
    /// 定位目标。无镜像 / stale / 无活动回合 → None——端点据此如实拒绝，不盲发
    /// （P1-1 纪律不变：有阀值才发，只是阀值来源多一级）。
    pub async fn mirror_active_turn_id(&self, key: &ThreadKey) -> Option<String> {
        let mirrors = self.inner.mirrors.lock().await;
        mirrors.get(key).and_then(|m| m.active_turn_id())
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
        // T3 红测缝：进入临界区前插窗（生产恒 None 直通）。修复后内容读取在临界
        // 区内，本缝只验证「窗口内完成翻转+重发布」的定序不再产生旧内容新序号。
        self.run_projection_presnapshot_gate();
        // T02：暂停事实在**镜像锁外**读取（锁内不读 DB）；策略求值仍与投影同一临界区。
        // 无镜像回退分支的策略求值需要暂停事实（有镜像时由 project_mirror 在临界区
        // 内自读，保证与资格事实同一观察点）。
        let queue_paused = self.queue_paused_of(key);
        // T03：持久提示每线程装载一次（锁外；读失败保持 Unknown 保守语义）。
        self.ensure_interaction_hints(key).await;
        // T3（复审 round 3）：caps/policy 与序号基线在**同一临界区**内读取——快照
        // 携带的序号必须是内容实际观察到的版本。任何 seq ≤ 盖章值的事件：其镜像
        // 变更与 caps/policy 观测都可证先于本临界区（发布点在镜像交互与内容生成
        // 之后；重发布的内容生成走同一条临界区纪律）→ 内容已包含或已被更晚策略
        // 覆盖，丢弃不丢真。旧代码在临界区前锁外读 caps/policy（Gate F Nit ②）+
        // 临界区后锁外重盖章（R5），两个窗口都能给旧内容盖上更晚的序号、把同
        // revision 的真新策略当「已合并」去重丢弃（设置/证据变化不依赖镜像锁）。
        // 设置读取是单条索引 SQL，量级远低于正确性代价——宁可持锁多读。复审
        // round 6（Q1）：**发布路径同规**——W1 起 payload 以临界区内盖章声明观察
        // 版本、CAS 以它为比较值，「读 caps → 取得镜像锁」的锁外窗同样能把旧
        // caps 生成的 payload 发成更大序号（availability 回翻覆盖新禁用），故四条
        // 直接发布路径（handle_state_changed / publish_projection /
        // handle_client_status_changed / mark_all_mirrors_stale）的 caps 观察一并
        // 挪进临界区，与快照路径同一边界。
        let p = {
            let mut mirrors = self.inner.mirrors.lock().await;
            let caps = self.effective_projection_caps();
            let policy = self.approval_policy();
            let sealed = self.seal_snapshot_seq();
            match mirrors.get_mut(key) {
                Some(m) => {
                    m.set_approval_policy(Some(policy));
                    let mut p = self.project_mirror(key, m, caps);
                    p.local_seq = sealed;
                    p
                }
                // 无镜像回退分支同样以临界区内的基线盖章：revision=None 的空投影
                // 仅与 revision=None 事件按 local_seq 竞争，而该类事件（本地重发
                // 布）必须镜像在场才会发布——此处无被回退风险，R5「空投影也盖章」
                // 语义保持。
                None => ThreadProjection {
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
                    // 无镜像：无题目可投影（不是"没有未答题"的事实）。
                    async_questions: Vec::new(),
                    // 无镜像也要给出**当前证据**（断链只撤回动作，不抹掉"已识别"）；
                    // 运行态按 unknown 保守处理。
                    effective_operations: self.compute_operations(
                        "unknown".to_string(),
                        false,
                        queue_paused,
                    ),
                    probe_status: ProbeStatus::idle(),
                    queue_paused: false,
                    owner_epoch: 0,
                    owner_generation: 0,
                    local_seq: sealed,
                },
            }
        };
        // T3 红测缝：临界区后插窗（生产恒 None 直通）。
        self.run_projection_sealed_gate();
        // 复审 R-T4：队列暂停事实盖章（镜像层不知道账本；空投影也盖——暂停是
        // 账本事实，不随镜像在场与否漂移）。盖章（内容+序号）已定稿，此处只叠
        // 更晚读取的账本事实——内容只会比序号声称的更新，不会更旧。
        let p = self.stamp_queue_pause(key, p);
        let p = self.stamp_probe_status(p).await;
        // R18：题目事实写穿持久提示（仅变化时落库；重启后未加载的题有占位）。
        self.persist_interaction_hints(key, &p.async_questions)
            .await;
        p
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

    /// S2：同步**发送守卫三态快照** `(状态可确认, 忙, 有未处理原生请求)`（flush
    /// 接力的廉价判定；不触发连接）。无镜像 / 锁被占（镜像正在写）= 不可确认
    /// （调用方按 StateUnknown 冻结——不能借空缓存证明没有审批）；有镜像 →
    /// 任一 pending 在场即 pending=true（含不支持的审批/问答——设计 §4.2「任何
    /// 未处理的原生请求不得仅因 idle 放行普通消息」）。
    pub fn send_gate_snapshot(&self, key: &ThreadKey) -> (bool, bool, bool) {
        let caps = self.current_caps();
        match self.inner.mirrors.try_lock() {
            Ok(mirrors) => match mirrors.get(key) {
                Some(m) => {
                    let p = m.project(caps);
                    (
                        true,
                        matches!(p.runtime_status.as_str(), "active" | "unknown"),
                        !p.pending.is_empty(),
                    )
                }
                None => (false, false, false),
            },
            Err(_) => (false, true, true),
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

    /// S4 只读核对补绑：在当前镜像全部 Turn 中查找承载本动作正式消息身份
    ///（`userMessage.clientId`）的 Turn。只读（不加锁写入、不发帧）；无镜像/stale/
    /// 查不到 → None。多义（多个 Turn 都有该 clientId）→ None（无法确定归属，
    /// 放弃同步维持现状——不猜纪律）。
    pub async fn turn_of_client_message(
        &self,
        key: &ThreadKey,
        client_message_id: &str,
    ) -> Option<String> {
        let mirrors = self.inner.mirrors.lock().await;
        let m = mirrors.get(key)?;
        if m.is_stale() {
            return None;
        }
        let ids = m.turn_ids_of_client_message(client_message_id);
        if ids.len() != 1 {
            return None;
        }
        ids.into_iter().next()
    }

    /// **派发边界快照**（2026-10-20 复审 P1）：ensure 之后、start-turn 之前的忙态与
    /// 已存在 Turn 集合，**同一把镜像锁**取出（保证两判据来自同一时刻状态）。
    /// 忙 = active/unknown **或任一 pending 在场**（S2：入队与写出之间新出现的原生
    /// 请求同样冻结派发——「已在队列中的普通消息不被 idle 假象穿透」）；无镜像 =
    /// 不忙（ensure 失败路径另由 start_turn 如实 NotSent）；锁被占（镜像正在写）=
    /// 忙（保守留队）。
    pub fn pre_write_snapshot(&self, key: &ThreadKey) -> (bool, std::collections::HashSet<String>) {
        let caps = self.current_caps();
        match self.inner.mirrors.try_lock() {
            Ok(mirrors) => match mirrors.get(key) {
                Some(m) => {
                    let p = m.project(caps);
                    let busy = matches!(p.runtime_status.as_str(), "active" | "unknown")
                        || !p.pending.is_empty();
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
        self.start_turn_with(
            key,
            action_id,
            text,
            client_message_id,
            false,
            None,
            Default::default(),
            None,
        )
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
        self.start_turn_with(
            key,
            action_id,
            text,
            client_message_id,
            true,
            None,
            Default::default(),
            None,
        )
        .await
    }

    /// S3：带预算上界的 start_turn 变体（A 路派发缝消费）——RPC 等待取
    /// `min(缺省 RPC 超时, 截止)`，子步骤只用剩余预算（设计 §5.1）。
    pub async fn start_turn_bounded(
        &self,
        key: &ThreadKey,
        action_id: &str,
        text: &str,
        client_message_id: &str,
        deadline: Option<tokio::time::Instant>,
        pre_dispatch_turns: std::collections::HashSet<String>,
    ) -> RpcOutcome {
        self.start_turn_with(
            key,
            action_id,
            text,
            client_message_id,
            false,
            deadline,
            pre_dispatch_turns,
            None,
        )
        .await
    }

    /// G04：异步组答题专用——带**完整组写边界复核**的 start-turn。
    #[allow(clippy::too_many_arguments)]
    pub async fn start_turn_group_bounded(
        &self,
        key: &ThreadKey,
        action_id: &str,
        text: &str,
        client_message_id: &str,
        deadline: Option<tokio::time::Instant>,
        pre_dispatch_turns: std::collections::HashSet<String>,
        recheck: super::types::AsyncWriteRecheck,
    ) -> RpcOutcome {
        self.start_turn_with(
            key,
            action_id,
            text,
            client_message_id,
            false,
            deadline,
            pre_dispatch_turns,
            Some(recheck),
        )
        .await
    }

    /// S3：[`start_turn_bounded`](Self::start_turn_bounded) 的兼容发送变体。
    pub async fn start_turn_compat_bounded(
        &self,
        key: &ThreadKey,
        action_id: &str,
        text: &str,
        client_message_id: &str,
        deadline: Option<tokio::time::Instant>,
        pre_dispatch_turns: std::collections::HashSet<String>,
    ) -> RpcOutcome {
        self.start_turn_with(
            key,
            action_id,
            text,
            client_message_id,
            true,
            deadline,
            pre_dispatch_turns,
            None,
        )
        .await
    }

    /// start turn 主体（`compat` = 兼容发送许可；false = 既有语义逐字不变）。
    /// S3：`deadline` = 本动作的后台收尾预算（RPC 等待只用剩余预算，可能已写出时
    /// 上层按「待确认」收尾——绝不因预算记 NotSent）。超参上限是**刻意**豁免
    ///（与 upsert_ok_conn 同理：形参按语义成列，不为省参数打乱列序引入错位风险）。
    #[allow(clippy::too_many_arguments)]
    async fn start_turn_with(
        &self,
        key: &ThreadKey,
        action_id: &str,
        text: &str,
        client_message_id: &str,
        compat: bool,
        deadline: Option<tokio::time::Instant>,
        pre_dispatch_turns: std::collections::HashSet<String>,
        async_write_recheck: Option<super::types::AsyncWriteRecheck>,
    ) -> RpcOutcome {
        let envelope_id = super::types::new_uuid();
        // R3：能力读当前连接的动态 profile（无连接/未知 build → closed → 拒）
        // #172/T8：兼容发送许可只放开「写能力未取证」这一档；**已验证 build 的行为
        // 完全不变**（它的 caps.send 本来就是 true，根本进不到这个分支）。
        // G01：自动适配关闭时建连可能装入 compat_readonly（send=false），随后
        // 开启适配并匹配契约——实际派发必须与投影/发送门同源放行，不能被旧连接
        // 的 send 位永久卡死；关闭/断链/换目标时 `send_gate_static` 仍会拒绝。
        let send_gate_ok = self.send_gate_static().is_ok();
        if !self.current_caps().send && !compat && !send_gate_ok {
            return not_sent(&envelope_id, "capability_unavailable");
        }
        let compat = compat || (!self.current_caps().send && send_gate_ok);
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
        self.dispatch(
            "thread-follower-start-turn",
            None,
            params,
            lease,
            validity,
            Some(action_id),
            deadline,
            compat,
            pre_dispatch_turns,
            None,
            async_write_recheck,
        )
        .await
    }

    /// 打断桌面当前回合（`thread-follower-interrupt-turn` v4，2026-10-21 新增）。
    ///
    /// 协议契约（由 Desktop 打包产物静态取证：`app.asar` 方法表 + handler + 打断
    /// 状态机）：
    /// - method `thread-follower-interrupt-turn`，基值版本 **4**；
    /// - params `{conversationId, mode, expectedTurnId}`；`mode` 取 `user-stop`
    ///   （等价用户手动按 Stop）或 `system`；`expectedTurnId` 是**安全阀**——与
    ///   当前活动回合不一致时 Desktop 静默不动作（不误伤别的回合）；
    /// - 返回 `{interruptedTurnId, ok}`（`goalPauseError` 只在 mode=user-stop 且
    ///   goal 暂停失败时出现，且仍带 interruptedTurnId）。
    ///
    /// 版本纪律：Desktop 侧版本协商对「params 缺 `expectedTurnId` 的 interrupt」
    /// 期望版本 **3**、带上则期望基值 **4**。本实现**始终带上 expectedTurnId**
    /// （缺省由调用方给出在册屏障绑定的 turnId），故固定走基值 4。
    ///
    /// **安全阀硬门（2026-10-22 复审 P1-1）**：`expected_turn_id == None` →
    /// `not_sent("expected_turn_missing")`，**零 IPC 写入**。缺该参数的 interrupt
    /// Desktop 照收不误（期望版本还会回落 3），等于无阀打断当前活动回合——含用户
    /// 在桌面手开的、与队列无关的回合。阀值必须由调用方（端点）取到：请求显式
    /// 携带，或从在册原生屏障绑定 turnId 读出；两路都取不到就拒发，不做回退。
    ///
    /// 打断后 Desktop 把该回合 status 置 `interrupted`——镜像终态门已认该状态
    /// （`mirror.rs turn_is_terminal`），屏障随之释放、队列接力下一条。
    /// T06：打断当前是否允许（逐操作策略；含"有活动回合"门）。
    async fn interrupt_allowed(&self) -> bool {
        let queue_paused = false;
        // 无法从 key 侧取暂停事实时按 false（打断要求暂停**前置**建立，不是入参）。
        self.compute_operations_for_key_unknown_pause(queue_paused)
            .allows(CodexOperation::Interrupt)
    }

    /// 无 key 视角的策略求值（打断门）：运行态取任一在场镜像的预览（同源事实）。
    fn compute_operations_for_key_unknown_pause(&self, queue_paused: bool) -> EffectiveOperations {
        let preview = self.inner.mirrors.try_lock().ok().and_then(|mirrors| {
            mirrors
                .values()
                .next()
                .map(|mirror| mirror.operation_preview())
        });
        match preview {
            Some((runtime_status, has_pending)) => {
                self.compute_operations(runtime_status, has_pending, queue_paused)
            }
            None => self.compute_operations("unknown".to_string(), false, queue_paused),
        }
    }

    /// R01：打断的写前资格复核（同一资格闸/同一事实；返回 Some = 零指令）。
    fn interrupt_verdict(&self, key: &ThreadKey, expected_turn_id: &str) -> Option<String> {
        let _gate = self
            .inner
            .interaction_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let facts = self
            .inner
            .interaction_facts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .cloned();
        // 无镜像事实（尚未投影/单测直连）→ 交回既有门（证据 + expectedTurnId）。
        let facts = facts?;
        if facts.stale {
            return Some("stale_context".into());
        }
        let Some(status) = facts
            .operations
            .get(super::capability::CodexOperation::Interrupt)
        else {
            return Some("operation_not_implemented".into());
        };
        if !status.allowed {
            return Some(
                status
                    .reason_code
                    .clone()
                    .unwrap_or_else(|| "capability_unavailable".into()),
            );
        }
        if let Some(active) = facts.active_turn_id.as_deref() {
            if active != expected_turn_id {
                return Some("stale_context".into());
            }
        }
        None
    }

    pub async fn interrupt_turn(
        &self,
        key: &ThreadKey,
        expected_turn_id: Option<&str>,
    ) -> RpcOutcome {
        self.interrupt_turn_bounded(key, expected_turn_id, super::budget::Deadline::none(), None)
            .await
    }

    /// R13：带前台预算的打断（零预算 = **零帧**；临近到期同样不写）。
    pub async fn interrupt_turn_bounded(
        &self,
        key: &ThreadKey,
        expected_turn_id: Option<&str>,
        deadline: super::budget::Deadline,
        device_id: Option<&str>,
    ) -> RpcOutcome {
        let envelope_id = super::types::new_uuid();
        if deadline.is_expired() {
            return not_sent(&envelope_id, "front_budget_exhausted");
        }
        // T06：打断能力改由**逐操作策略**判定（契约匹配/精确 shipped/真实成功），
        // 不再只看静态 `caps.interrupt`——未知 build 的已匹配 v4 契约可以定向打断。
        // 求值只读内存事实；不重置任何预算。
        if !self.interrupt_allowed().await {
            return not_sent(&envelope_id, "capability_unavailable");
        }
        // 安全阀硬门（复审 P1-1）：无阀值一律拒发、零 IPC 写入（理由见函数注释）。
        let Some(expected_turn_id) = expected_turn_id else {
            return not_sent(&envelope_id, "expected_turn_missing");
        };
        // R01：最终资格复核（目标仍在 + 逐操作允许）——不满足零指令。
        if let Some(reason) = self.interrupt_verdict(key, expected_turn_id) {
            return not_sent(&envelope_id, &reason);
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
        let version = {
            let conn = self.inner.conn.lock().await;
            match conn
                .as_ref()
                .map(|c| c.version_of("thread-follower-interrupt-turn"))
            {
                Some(Ok(v)) => v,
                _ => return not_sent(&envelope_id, "disconnected"),
            }
        };
        let params = json!({
            "conversationId": key.thread_id,
            "mode": "user-stop",
            "expectedTurnId": expected_turn_id,
        });
        // H02：interrupt 无账本 claim，改用**写边界短命许可**（writer 在 write_all
        // 前调用本闭包；资格先失效 → 零首次写出）。
        let recheck_adapter = self.clone();
        let recheck_key = key.clone();
        let recheck_expected = expected_turn_id.to_string();
        let recheck_device = device_id.map(str::to_string);
        let recheck: super::types::InteractionRecheck = std::sync::Arc::new(move || {
            // 测试缝（生产直通）：停在 writer 写边界前，便于窗口内变更事实。
            recheck_adapter.run_interaction_recheck_gate();
            // H02：短命许可**携设备**——撤设备后即使镜像不变也必须零帧。
            if let Some(device_id) = recheck_device.as_deref() {
                let ok = recheck_adapter.device_is_valid(device_id);
                eprintln!("[h02dbg] interrupt device={device_id} ok={ok}");
                if !ok {
                    return Some("device_revoked".into());
                }
            }
            recheck_adapter.interrupt_verdict(&recheck_key, &recheck_expected)
        });
        self.dispatch(
            "thread-follower-interrupt-turn",
            Some(version),
            params,
            lease,
            validity,
            // 打断不进动作账本：不预留迟到槽位（打断回执超时按既有 Unknown 语义）
            None,
            // N03：打断同样携首次写出截止（writer 到点零帧）。
            deadline.instant(),
            false,
            Default::default(),
            Some(recheck),
            None,
        )
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
        action_id: &str,
    ) -> RpcOutcome {
        self.respond_bounded(
            key,
            request_key,
            context_token,
            action,
            action_id,
            super::budget::Deadline::none(),
        )
        .await
    }

    /// T07：**异步 Question 的空闲正式答题**——构造官方信封并经 start-turn 一次
    /// 提交（独立动作，绝不借普通消息 FIFO／steer）。
    ///
    /// 准入（全部在写出前再次复核）：
    /// * 该题在**当前投影**里存在、`contextToken` 一致、状态 = Pending/可提交；
    /// * 答案 ≤ 10_000 字符、最终信封 ≤ 64 KiB（超限明确零投递）；
    /// * 写前状态复核：仍 idle、无原生阻塞请求、投影不 stale（变 busy → NotSent，
    ///   保稿且不退回 FIFO、不自动重发）；
    /// * `bypass_paused_queue` 只影响路由层的一次性 grant（这里不自动清暂停）。
    pub async fn answer_async(
        &self,
        key: &ThreadKey,
        action_id: &str,
        command: &super::async_question::AsyncAnswerCommand,
        client_message_id: &str,
        bypass_paused_queue: bool,
        deadline: super::budget::Deadline,
    ) -> RpcOutcome {
        let envelope_id = super::types::new_uuid();
        if deadline.is_expired() {
            return not_sent(&envelope_id, "front_budget_exhausted");
        }
        if command.answer.chars().count() > super::async_question::MAX_ANSWER_CHARS {
            return not_sent(&envelope_id, "answer_too_long");
        }
        let Some((item_id, index)) =
            super::async_question::parse_question_key(&command.question_key)
        else {
            return not_sent(&envelope_id, "invalid_id");
        };
        // 投影是唯一事实源：题目存在性、token 绑定、生命周期状态都在这里复核。
        let projection = self.projection(key).await;
        if projection.stale {
            return not_sent(&envelope_id, "stale_context");
        }
        let terminal = |state: super::async_question::InteractionState| {
            matches!(
                state,
                super::async_question::InteractionState::Answered
                    | super::async_question::InteractionState::Ended
                    | super::async_question::InteractionState::Cancelled
                    | super::async_question::InteractionState::Rejected
            )
        };
        let is_group = !command.answers.is_empty();
        // B05：组答题必须**整组同源**、整组成员齐全、一次信封；单题路径保持原状。
        let (group, paused_only) = if is_group {
            let mut entries = Vec::with_capacity(command.answers.len());
            let mut keys = std::collections::BTreeSet::new();
            for answer in &command.answers {
                if !keys.insert(answer.question_key.clone()) {
                    return not_sent(&envelope_id, "answers_mismatch");
                }
                let Some(question) = projection
                    .async_questions
                    .iter()
                    .find(|question| question.question_key == answer.question_key)
                else {
                    return not_sent(&envelope_id, "stale_context");
                };
                let expected_token = if answer.context_token.is_empty() {
                    command.context_token.as_str()
                } else {
                    answer.context_token.as_str()
                };
                if question.context_token != expected_token {
                    return not_sent(&envelope_id, "stale_context");
                }
                if terminal(question.state) {
                    return not_sent(&envelope_id, "question_already_answered");
                }
                entries.push((question, answer.answer.clone()));
            }
            let Some((first, _)) = entries.first() else {
                return not_sent(&envelope_id, "answers_mismatch");
            };
            let source = (first.source_turn_id.clone(), first.source_item_id.clone());
            let group_all: Vec<_> = projection
                .async_questions
                .iter()
                .filter(|question| {
                    question.source_turn_id == source.0 && question.source_item_id == source.1
                })
                .collect();
            // G08：别端已答必须在用户显式复核集合里；剩余集合必须精确等于
            // “整组 − 已确认已答”，不能少答、不能跨组替换、不能重答已答。
            let ack: std::collections::BTreeSet<String> = command
                .acknowledged_answered
                .iter()
                .map(|key| key.trim().to_string())
                .filter(|key| !key.is_empty())
                .collect();
            let answered_keys: std::collections::BTreeSet<String> = group_all
                .iter()
                .filter(|question| {
                    question.state == super::async_question::InteractionState::Answered
                })
                .map(|question| question.question_key.clone())
                .collect();
            if !answered_keys.is_subset(&ack) {
                return not_sent(&envelope_id, "question_already_answered");
            }
            if group_all.iter().any(|question| {
                terminal(question.state)
                    && question.state != super::async_question::InteractionState::Answered
            }) {
                return not_sent(&envelope_id, "not_actionable");
            }
            let live_keys: std::collections::BTreeSet<String> = group_all
                .iter()
                .filter(|question| {
                    question.state != super::async_question::InteractionState::Answered
                })
                .map(|question| question.question_key.clone())
                .collect();
            if live_keys != keys {
                return not_sent(&envelope_id, "answers_mismatch");
            }
            let paused_only = bypass_paused_queue
                && entries.iter().all(|(question, _)| {
                    question.state == super::async_question::InteractionState::TemporarilyBlocked
                        && question.disabled_reason_code.as_deref()
                            == Some("operation_queue_paused")
                });
            if entries
                .iter()
                .any(|(question, _)| !question.actionable && !paused_only)
            {
                let reason = entries
                    .iter()
                    .find(|(question, _)| !question.actionable && !paused_only)
                    .and_then(|(question, _)| question.disabled_reason_code.as_deref())
                    .unwrap_or("not_actionable");
                return not_sent(&envelope_id, reason);
            }
            (entries, paused_only)
        } else {
            let Some(question) = projection
                .async_questions
                .iter()
                .find(|question| question.question_key == command.question_key)
            else {
                return not_sent(&envelope_id, "stale_context");
            };
            if question.context_token != command.context_token {
                return not_sent(&envelope_id, "stale_context");
            }
            if terminal(question.state) {
                return not_sent(&envelope_id, "not_actionable");
            }
            let paused_only = bypass_paused_queue
                && question.state == super::async_question::InteractionState::TemporarilyBlocked
                && question.disabled_reason_code.as_deref() == Some("operation_queue_paused");
            if !question.actionable && !paused_only {
                return not_sent(
                    &envelope_id,
                    question
                        .disabled_reason_code
                        .as_deref()
                        .unwrap_or("not_actionable"),
                );
            }
            (vec![(question, command.answer.clone())], paused_only)
        };
        let _ = paused_only;
        let envelope = if is_group {
            let parts: Vec<(String, u32, String, String)> = group
                .iter()
                .map(|(question, answer)| {
                    let (item, index) =
                        super::async_question::parse_question_key(&question.question_key)
                            .unwrap_or((question.source_item_id.clone(), question.question_index));
                    (item, index, question.title.clone(), answer.clone())
                })
                .collect();
            super::async_question::compose_async_answers(&parts)
        } else {
            super::async_question::compose_async_answer(
                &item_id,
                index,
                &group[0].0.title,
                &group[0].1,
            )
        };
        if envelope.len() > super::async_question::MAX_ENVELOPE_BYTES {
            return not_sent(&envelope_id, "answer_too_large");
        }
        // 写前状态复核（最小窗口）：题仍在、仍空闲、无阻塞请求。
        let pre_write = self.projection(key).await;
        if pre_write.stale
            || !pre_write.pending.is_empty()
            || !pre_write.runtime_status.eq_ignore_ascii_case("idle")
        {
            return not_sent(&envelope_id, "busy_before_write");
        }
        // G04：组答题把完整组成员/token/来源绑定到 **writer 首写权** 的复核；
        // 单题保持既有 `start_turn_bounded` 语义（其入口/写前检查已覆盖）。
        let outcome = if is_group {
            // G04：把完整组的成员/token/来源绑定到**writer 首写权临界区**的复核。
            // 停在 gate 后任一成员被别端回答、移除或换 token → 零首次写出。
            let expected: Vec<(String, String)> = group
                .iter()
                .map(|(question, _)| {
                    (
                        question.question_key.clone(),
                        question.context_token.clone(),
                    )
                })
                .collect();
            let expected_keys: std::collections::BTreeSet<String> =
                expected.iter().map(|(key, _)| key.clone()).collect();
            let source = (
                group[0].0.source_turn_id.clone(),
                group[0].0.source_item_id.clone(),
            );
            // C01：只有真正登记成功、进入派发的拥有者才能建立组绑定。绑定放在
            // 路由 register/reserve 之后的 adapter 内，冲突请求到不了这里，无法
            // 用同 actionId 的另一组覆盖；writer 最终 gate 读到的必是本动作来源。
            self.bind_group_action(action_id, &source.0, &source.1, expected.clone());
            let recheck_adapter = self.clone();
            let recheck_key = key.clone();
            let recheck: super::types::AsyncWriteRecheck = std::sync::Arc::new(move || {
                let adapter = recheck_adapter.clone();
                let key = recheck_key.clone();
                let expected = expected.clone();
                let expected_keys = expected_keys.clone();
                let source = source.clone();
                Box::pin(async move {
                    let projection = adapter.projection(&key).await;
                    if projection.stale
                        || !projection.pending.is_empty()
                        || !projection.runtime_status.eq_ignore_ascii_case("idle")
                    {
                        return Some("busy_before_write".to_string());
                    }
                    for (question_key, token) in &expected {
                        let Some(question) = projection
                            .async_questions
                            .iter()
                            .find(|question| question.question_key == *question_key)
                        else {
                            return Some("stale_context".to_string());
                        };
                        if question.context_token != *token
                            || matches!(
                                question.state,
                                super::async_question::InteractionState::Answered
                                    | super::async_question::InteractionState::Ended
                                    | super::async_question::InteractionState::Cancelled
                                    | super::async_question::InteractionState::Rejected
                            )
                        {
                            return Some("question_already_answered".to_string());
                        }
                    }
                    let live_keys: std::collections::BTreeSet<String> = projection
                        .async_questions
                        .iter()
                        .filter(|question| {
                            question.source_turn_id == source.0
                                && question.source_item_id == source.1
                                && question.state
                                    != super::async_question::InteractionState::Answered
                                && question.state != super::async_question::InteractionState::Ended
                                && question.state
                                    != super::async_question::InteractionState::Cancelled
                                && question.state
                                    != super::async_question::InteractionState::Rejected
                        })
                        .map(|question| question.question_key.clone())
                        .collect();
                    (live_keys != expected_keys).then(|| "answers_mismatch".to_string())
                })
            });
            // 一次 canonical start-turn（信封作为正文；不是 steer、不进 FIFO）。
            self.start_turn_group_bounded(
                key,
                action_id,
                &envelope,
                client_message_id,
                deadline.instant(),
                Default::default(),
                recheck,
            )
            .await
        } else {
            self.start_turn_bounded(
                key,
                action_id,
                &envelope,
                client_message_id,
                deadline.instant(),
                Default::default(),
            )
            .await
        };
        // 受理后核对：原题是否已由原会话正式答复派生为 Answered（更强证据）。
        outcome
    }

    /// T04：某原生请求的解决证据（响应条目优先于单纯消失；只读镜像，不写）。
    pub async fn native_request_resolution(
        &self,
        key: &ThreadKey,
        request_key: &str,
    ) -> super::pending::NativeResolution {
        self.native_request_resolution_since(key, request_key, None)
            .await
            .0
    }

    /// N02：带**效果基线**的原生解决证据——只有镜像在基线之后推进（或 owner
    /// 换代）时，完成条目才算本动作的效果；stale 镜像一律不提升成功。
    ///
    /// 返回 `(resolution, revision)`，调用方可以记录新的基线。
    pub async fn native_request_resolution_since(
        &self,
        key: &ThreadKey,
        request_key: &str,
        baseline_revision: Option<u64>,
    ) -> (super::pending::NativeResolution, u64) {
        let Ok(request_id) = super::pending::parse_request_key(request_key) else {
            return (super::pending::NativeResolution::Ended, 0);
        };
        let id_value = serde_json::to_value(&request_id).unwrap_or(serde_json::Value::Null);
        let mirrors = self.inner.mirrors.lock().await;
        let Some(mirror) = mirrors.get(key) else {
            return (super::pending::NativeResolution::Ended, 0);
        };
        let revision = mirror.revision().unwrap_or(0);
        if mirror.is_stale() {
            return (super::pending::NativeResolution::Ended, revision);
        }
        if baseline_revision.is_some_and(|baseline| revision <= baseline) {
            // 没有比派发基线更新的事实 → 不能拿旧完成条目当本动作效果。
            return (super::pending::NativeResolution::Ended, revision);
        }
        let Some(raw) = mirror.raw_state() else {
            return (super::pending::NativeResolution::Ended, revision);
        };
        (
            super::pending::resolve_native_request(raw, &id_value, true),
            revision,
        )
    }

    /// T04：带**前台总预算**的原生响应（子步骤只用剩余预算；到点按"可能已写出"
    /// 收尾，绝不记 NotSent）。
    ///
    /// `deadline` 是调用方已经起算的绝对截止（点击起 30s）；本方法**不重置**它。
    pub async fn respond_bounded(
        &self,
        key: &ThreadKey,
        request_key: &str,
        context_token: &str,
        action: PendingAction,
        action_id: &str,
        deadline: super::budget::Deadline,
    ) -> RpcOutcome {
        let envelope_id = super::types::new_uuid();
        let caps = self.current_caps();
        if deadline.is_expired() {
            // 前台预算已耗尽：零派发（不是"已送达"）。
            return not_sent(&envelope_id, "front_budget_exhausted");
        }
        let queue_paused = self.queue_paused_of(key);
        // T04：放行门用的逐操作策略在**镜像锁外**求值（只读同一份事实；避免在
        // 持锁路径里再取 profile/settings 锁）。
        let operations = self.compute_operations_for(key, queue_paused).await;
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
            RespondSnapshot::collect(
                mirror,
                caps,
                &policy,
                &operations,
                request_key,
                context_token,
            )
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
            .dispatch(
                prepared.method,
                None,
                prepared.params,
                lease,
                validity,
                // S3：审批 mutation 绑定动作 id（迟到回执经槽位 + 对账兜底归原动作）
                Some(action_id),
                // T04：前台绝对截止向下传递（连接层取 min(缺省 RPC 超时, 截止)）。
                deadline.instant(),
                false,
                // 终审 F10：审批路径无写前镜像快照点——空集 = 「快照缺失」语义
                //（validate 对空集退化为纯身份等待，不误判 stale_turn_echo）
                Default::default(),
                // 审批/问答有账本 claim，不再叠加无账本复核。
                None,
                None,
            )
            .await;
        // T04：Accepted 之后按**更强证据**复核解决状态——响应条目
        // `userInputResponse.completed` 优先；只有请求消失而原因不明时才写
        // "请求已结束"（绝不因"不在 requests 里"就猜已答）。
        let mut outcome = outcome;
        if outcome.delivery == Delivery::Accepted && matches!(action, PendingAction::Answer(_)) {
            outcome.reason_code = match self.native_request_resolution(key, request_key).await {
                super::pending::NativeResolution::Answered => None,
                super::pending::NativeResolution::Ended => Some("request_ended".to_string()),
                super::pending::NativeResolution::Pending => None,
            };
        }
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
    /// S3：`action_id`（mutation 迟到槽位归属）/ `deadline`（统一预算的等待上界——
    /// 连接层取 min(缺省 RPC 超时, 截止)）。
    #[allow(clippy::too_many_arguments)]
    async fn dispatch(
        &self,
        method: &str,
        version: Option<u32>,
        params: Value,
        target: OwnerLease,
        validity: tokio::sync::watch::Receiver<bool>,
        action_id: Option<&str>,
        deadline: Option<tokio::time::Instant>,
        compat_attempt: bool,
        pre_dispatch_turns: std::collections::HashSet<String>,
        interaction_recheck: Option<super::types::InteractionRecheck>,
        async_write_recheck: Option<super::types::AsyncWriteRecheck>,
    ) -> RpcOutcome {
        let envelope_id = super::types::new_uuid();
        let conn = self.inner.conn.lock().await.clone();
        let Some(conn) = conn else {
            return not_sent(&envelope_id, "disconnected");
        };
        // 方法缺失 = 明确协议错（unsupported_method）→ 零投递，保持既有 not_sent 语义；
        // 显式版本（interrupt 基值 4 纪律）跳过查表。
        let version = match version {
            Some(v) => v,
            None => match conn.version_of(method) {
                Ok(v) => v,
                Err(e) => return not_sent(&envelope_id, e.code()),
            },
        };
        // 终审 F4 **写边界守卫**：mutation 在进入连接 writer 队列之前核「派发权
        // 撤销 + 兼容许可活值」。这是可达的最晚判定点（此点之后只剩 mpsc 入队与
        // writer 的 validity 复核）——上层检查通过之后才发生的撤销在此被拦截，
        // 旧动作零首次写出（撤销臂语义：定格 manual_resume_revoked 的 Unknown）。
        // 守卫失败（DB 读错）由闭包自身映射为 Reject（保守拒绝）——本层不猜。
        // 「依赖兼容授权」= 显式 compat 尝试 ∨ 当前档位 send 能力来自兼容档
        //（证据级别 Unverified 且 send 开——关闭自动适配后这类档位本不该再写）。
        // 终审 R2：该票据随帧携带，writer 写前原子复核（活门收回 → 零投递）。
        let compat_licensed = compat_attempt || {
            let p = self.current_profile();
            p.evidence == super::profile::EvidenceLevel::Unverified && p.capabilities.send
        };
        if let Some(action_id) = action_id {
            // 终审 F4 测试缝：守卫判定前可注入等待（生产恒 None 直通）——固定
            // 「上层检查已过、守卫未判」窗口内的人工继续交错。
            let gate = self
                .inner
                .write_guard_gate
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if let Some(gate) = gate {
                self.inner.write_guard_reached.store(true, Ordering::SeqCst);
                gate.notified().await;
                self.inner
                    .write_guard_reached
                    .store(false, Ordering::SeqCst);
            }
            match self.write_guard_verdict(action_id, compat_licensed) {
                crate::services::codex_control::WriteGuardVerdict::Allow => {}
                crate::services::codex_control::WriteGuardVerdict::RejectRevoked => {
                    return RpcOutcome::unknown(
                        envelope_id,
                        crate::services::codex_control::MANUAL_RESUME_REVOKED_REASON,
                    );
                }
                crate::services::codex_control::WriteGuardVerdict::RejectGuardReadFailed => {
                    // Gate C #4：读失败独立定格码（非人工继续语义）
                    return RpcOutcome::unknown(
                        envelope_id,
                        crate::services::codex_control::WRITE_GUARD_READ_FAILED_REASON,
                    );
                }
                crate::services::codex_control::WriteGuardVerdict::RejectCompatRetracted => {
                    return not_sent(&envelope_id, "capability_unverified");
                }
            }
        }
        conn.request(RpcRequest {
            method: method.to_string(),
            params,
            version: Some(version),
            target: Some(target),
            mutating: true,
            validity: Some(validity),
            source_client_id: None,
            timeout_ms: None,
            action_id: action_id.map(str::to_string),
            compat_licensed,
            deadline,
            pre_dispatch_turns,
            interaction_recheck,
            async_write_recheck,
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

/// T04：阻塞问答的服务端上限（spec §5 功能点 3/4）。
const MAX_BLOCKING_QUESTIONS: usize = 32;
const MAX_BLOCKING_ANSWERS_PER_QUESTION: usize = 32;
const MAX_BLOCKING_ANSWER_CHARS: usize = 10_000;
const MAX_BLOCKING_TOTAL_BYTES: usize = 64 * 1024;

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
#[derive(Clone)]
struct RespondSnapshot {
    /// 能力基线（阶段 1 取自当前动态 profile）
    caps: CapabilitySet,
    /// T04：逐操作有效集合（F02 阻塞问答的放行门；不再只看静态 `caps.blocking_input`）。
    operations: EffectiveOperations,
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
        operations: &EffectiveOperations,
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
            operations: operations.clone(),
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
                PendingAction::Approve | PendingAction::Reject | PendingAction::Cancel,
            ) => {
                let method =
                    decision_method(&pending.kind).ok_or_else(|| "unsupported".to_string())?;
                // T05：三个动作**各自只对应一个词**（不再由一个 reject 隐式猜
                // cancel）：Approve→accept、Reject→decline、Cancel→cancel。
                // 词不在本卡合法列表 → 明确拒绝（零写出），不静默换词。
                let candidates: &[&str] = match action {
                    PendingAction::Approve => &[DECISION_ACCEPT],
                    PendingAction::Reject => &[DECISION_DECLINE],
                    _ => &[DECISION_CANCEL],
                };
                let word = pick_decision(&pending.allowed_decisions, candidates)
                    .ok_or_else(|| "action_mismatch".to_string())?;
                (method, word)
            }
            (PendingKind::PermissionsApproval, PendingAction::Approve | PendingAction::Reject) => {
                if !caps.permissions {
                    return Err("capability_unavailable".into());
                }
                (METHOD_PERMISSIONS_RESPONSE, "")
            }
            (PendingKind::UserInput, PendingAction::Answer(_)) => {
                // T04：放行门 = 当前逐操作策略（契约匹配/精确 shipped/真实成功），
                // 不再只看静态 `caps.blocking_input`——当前 Mac 契约无需新 build
                // 白名单即可正式回答。未识别/关闭适配 → 保留摘要走回原端。
                if !self.operations.allows(CodexOperation::BlockingInput) {
                    return Err(self
                        .operations
                        .get(CodexOperation::BlockingInput)
                        .and_then(|status| status.reason_code.clone())
                        .unwrap_or_else(|| "capability_unavailable".into()));
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
                // 按 qid 精确映射：不能漏答/多答/混入未知 qid；isOther=false 禁自由文本。
                // T04 追加：题数/题 id 唯一/单题答案数/单答案长度/总字节全部在服务端
                // 校验（前端隐藏控件不代替写出边界校验）。
                let questions = &pending.questions;
                if questions.len() > MAX_BLOCKING_QUESTIONS {
                    return Err("too_many_questions".into());
                }
                let mut seen = std::collections::HashSet::new();
                for q in questions {
                    if !seen.insert(q.id.as_str()) {
                        return Err("duplicate_question".into());
                    }
                }
                if answers.len() != questions.len() {
                    return Err("answers_mismatch".into());
                }
                let mut mapped = serde_json::Map::new();
                let mut total_bytes = 0usize;
                for q in questions {
                    let Some(entries) = answers.get(&q.id) else {
                        return Err("answers_mismatch".into());
                    };
                    if entries.is_empty() {
                        return Err("answers_mismatch".into());
                    }
                    let policy = q.answer_policy;
                    if !policy.allow_multiple && entries.len() > 1 {
                        return Err("answer_not_multiple".into());
                    }
                    if entries.len() > MAX_BLOCKING_ANSWERS_PER_QUESTION
                        || entries.len() as u32 > policy.max_answers.max(1)
                    {
                        return Err("too_many_answers".into());
                    }
                    for entry in entries {
                        if entry.chars().count() > MAX_BLOCKING_ANSWER_CHARS {
                            return Err("answer_too_long".into());
                        }
                        total_bytes += entry.len();
                        if !policy.allow_free_text
                            && !q.options.iter().any(|option| &option.label == entry)
                        {
                            return Err("answer_not_option".into());
                        }
                    }
                    mapped.insert(q.id.clone(), json!({ "answers": entries }));
                }
                if total_bytes > MAX_BLOCKING_TOTAL_BYTES {
                    return Err("answers_too_large".into());
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
#[derive(Debug)]
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
    // 复审 round 6（Q1）测试缝：「caps 观察窗」（修复后 caps 在临界区内读取，本缝
    // 退化为「先跑窗口动作再取快照」的无害定序——窗内完成的翻转/重发布随后被临界
    // 区内的最新观察吸收）。
    adapter.run_caps_observe_gate("state_changed");
    // 短锁 2：原子应用
    let mut fresh: Option<ThreadProjection> = None;
    let mut need_recover = false;
    let mut history_needed = false;
    // T02：暂停事实在镜像锁外读取（锁内不读 DB）。
    {
        let mut mirrors = adapter.inner.mirrors.lock().await;
        let Some(mirror) = mirrors.get_mut(&key) else {
            return; // 未跟随的 thread 不建镜像、不投影
        };
        match mirror.apply(&lease, &change) {
            Ok(eff) => {
                if eff.changed {
                    // 复审 round 6（Q1）：caps 与盖章**同一临界区**——发布 payload
                    // 声称的观察版本（seal）必须与生成它的 caps 观察同一边界。旧
                    // 代码在锁外读 caps：窗内完成的「关闭设置 + 禁用发布（seq=N）」
                    // 会被旧 caps 生成的 payload 以 seal=N、CAS(N→N+1) 合法覆盖，
                    // availability 从 read_only 回翻（W1 的 CAS 只封「检查后」窗，
                    // 封不了「读 caps → 取得镜像锁」窗）。锁纪律（T3 同款论证）：持
                    // 镜像锁读取 caps = profile（std 叶子锁）+ auto_adapt_gate（std
                    // 短锁 → 设置 DB 单条读，生产闭包在 gate 锁内调用——该锁序自
                    // round 3 起即存在于 projection()/fresh_projection() 快照路径，
                    // 经 Gate G/H/I 复核；反向序不存在——设置 DB 锁持有者是同步代
                    // 码，不可能再 await 镜像锁）。
                    let caps = adapter.effective_projection_caps();
                    let mut proj = adapter.project_mirror(&key, mirror, caps);
                    // 复审 round 5（W1）：发布路径的 payload 也以**镜像临界区内**
                    // 的盖章声明其内容版本（T3 快照同款边界）——发布点的 CAS 以
                    // 它为比较值，「内容观察」与「发布资格取得」才有共同的版本轴。
                    proj.local_seq = adapter.seal_snapshot_seq();
                    fresh = Some(proj);
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
        // 复审 round 5（W1/V3 收口）测试缝：镜像锁已释放、cleanup 等待已完成、
        // 发布序号尚未取得的定序窗（生产恒 None 直通）——复审原文指认的生产
        // 异步竞争窗。
        adapter.run_state_publish_gate();
        // 复审 round 4（V1）一致性：队列暂停事实盖章（镜像锁已释放）——状态帧
        // 路径的发布若漏盖，暂停中的线程会带着 mirror 缺省 false 广播「未暂停」
        // 假状态。#172/T9a：探测只读事实。
        // 复审 round 5（W1）：发布走全入口统一的内容版本协议——CAS 预留（比较值
        // = 临界区内的盖章）；等待窗内被并发发布越过 → 重观察后再预留，旧观察
        // 不再以发送时刻取得更大序号（W1② 红测窗口）。
        let p = adapter.stamp_queue_pause(&key, p);
        let p = adapter.stamp_probe_status(p).await;
        adapter.publish_fresh_projection(&key, p).await;
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
    // 复审 round 6（Q1）测试缝：「caps 观察窗」（修复后 caps 在临界区内读取，无害
    // 定序——见 handle_state_changed 同款注释）。
    adapter.run_caps_observe_gate("publish_projection");
    // T02：暂停事实在镜像锁外读取。
    let projection = {
        let mut mirrors = adapter.inner.mirrors.lock().await;
        // 复审 round 6（Q1）：caps 与盖章同一临界区（Q1① 同款论证与锁纪律）——
        // 窗内完成的禁用发布不得被锁外旧 caps 生成的 payload 覆盖。
        let caps = adapter.effective_projection_caps();
        mirrors.get_mut(key).map(|m| {
            let mut p = adapter.project_mirror(key, m, caps);
            // 复审 round 5（W1）：发布 payload 以镜像临界区内的盖章声明内容版本
            //（handle_state_changed 同款）——发布点的 CAS 以它为比较值。
            p.local_seq = adapter.seal_snapshot_seq();
            p
        })
    };
    if let Some(p) = projection {
        // 复审 R-T4：队列暂停事实（镜像锁已释放）+ #172/T9a：探测只读事实。
        // 复审 round 5（W1）：CAS 预留发布（全入口统一协议）；被并发发布越过
        // → 重观察后再预留。
        let p = adapter.stamp_queue_pause(key, p);
        let p = adapter.stamp_probe_status(p).await;
        adapter.publish_fresh_projection(key, p).await;
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
                    action_id: None,
                    compat_licensed: false,
                    deadline: None,
                    pre_dispatch_turns: Default::default(),
                    interaction_recheck: None,
                    async_write_recheck: None,
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
    // 复审 round 6（Q1）测试缝：「caps 观察窗」（修复后 caps 在临界区内读取，无害
    // 定序——见 handle_state_changed 同款注释）。
    adapter.run_caps_observe_gate("client_status_changed");
    let mut projections = Vec::new();
    {
        let mut mirrors = adapter.inner.mirrors.lock().await;
        // 复审 round 6（Q1）：caps 与盖章同一临界区（Q1① 同款论证与锁纪律）。
        let caps = adapter.effective_projection_caps();
        for key in &affected {
            if let Some(m) = mirrors.get_mut(key) {
                m.invalidate();
                let mut p = adapter.project_mirror(key, m, caps);
                // 复审 round 5（W1）：发布 payload 以镜像临界区内的盖章声明内容
                // 版本——发布点的 CAS 以它为比较值。
                p.local_seq = adapter.seal_snapshot_seq();
                projections.push((key.clone(), p));
            }
        }
    }
    {
        let mut inflight = adapter.inner.inflight.lock().await;
        inflight.retain(|(tid, _), _| !affected.iter().any(|k| &k.thread_id == tid));
    }
    for (key, p) in projections {
        // 复审 round 4（V1）一致性：队列暂停事实盖章（镜像锁已释放）——失效投影
        // 若漏盖，暂停中的线程会在断连广播里带 mirror 缺省 false（假「未暂停」）。
        // #172/T9a：探测只读事实。
        // 复审 round 5（W1）：CAS 预留发布（全入口统一协议）；被并发发布越过
        // → 重观察后再预留（重观察经 `projection()` 出口，失效镜像给出同样
        // 如实的 stale 内容）。
        let p = adapter.stamp_queue_pause(&key, p);
        let p = adapter.stamp_probe_status(p).await;
        adapter.publish_fresh_projection(&key, p).await;
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
    // 复审 round 6（Q1）测试缝：「caps 观察窗」（修复后 caps 在临界区内读取，无害
    // 定序——见 handle_state_changed 同款注释）。
    adapter.run_caps_observe_gate("mark_all_mirrors_stale");
    let mut projections = Vec::new();
    {
        let mut mirrors = adapter.inner.mirrors.lock().await;
        // 复审 round 6（Q1）：caps 与盖章同一临界区（Q1① 同款论证与锁纪律）。
        let caps = adapter.effective_projection_caps();
        for (key, mirror) in mirrors.iter_mut() {
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
                let mut p = adapter.project_mirror(key, mirror, caps);
                // 复审 round 5（W1）：发布 payload 以镜像临界区内的盖章声明内容
                // 版本——发布点的 CAS 以它为比较值。
                p.local_seq = adapter.seal_snapshot_seq();
                projections.push((key.clone(), p));
            }
        }
    }
    for (key, p) in projections {
        // 复审 round 4（V1）一致性：队列暂停事实盖章（镜像锁已释放）——断线/reset
        // 的 stale 投影若漏盖，暂停中的线程会带 mirror 缺省 false 广播假「未暂停」。
        // #172/T9a：探测只读事实。
        // 复审 round 5（W1）：CAS 预留发布（全入口统一协议）；被并发发布越过
        // → 重观察后再预留。
        let p = adapter.stamp_queue_pause(&key, p);
        let p = adapter.stamp_probe_status(p).await;
        adapter.publish_fresh_projection(&key, p).await;
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
    pub(super) struct NoopConnector;

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
        gets: AtomicU64,
    }
    impl ProbeLedger for ProbedLedger {
        fn get(&self, key: &str, _: &str) -> Option<super::super::probe::ProbeLedgerEntry> {
            self.gets.fetch_add(1, Ordering::SeqCst);
            (key == "cli-a").then_some(super::super::probe::ProbeLedgerEntry {
                state: crate::database::dao::codex_ipc_probe::ProbeState::Probed,
                next_retry_at: None,
                stage: None,
                error_code: None,
                fingerprint: None,
                evidenced: true,
                evidenced_at: None,
                last_seen_at: None,
            })
        }
        fn record(&self, key: &str, _: &str, _: &ProbeOutcome, _: Option<&str>) {
            assert_eq!(key, "cli-b", "旧结果不得落账");
            self.records.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// 终审 F2（探针臂）：**旧 schema 结论不得落新身份**——定序窗口 = 最终身份
    /// 检查通过后、安装前。A 的 CLI 探针结论到达安装点时另一 worker 已换代到 B：
    /// A 的证据不得安装到 B（审批策略不得因旧证据解锁）、不得替 B 记账；B 自身
    /// 的证据路径不受影响（换回新身份的探针照常安装）。
    #[tokio::test]
    async fn late_schema_result_cannot_install_onto_new_generation() {
        let adapter = DesktopIpcAdapter::new(
            Arc::new(NoopConnector),
            ProtocolProfile::unverified("macos", "same-build"),
        );
        adapter.set_probe_key_override(Some("macos|same-build|cli-a|1".into()));
        adapter.set_schema_probe_override(Some(SchemaProbeOutcome::Known(SchemaEvidence {
            fingerprint: "schema-of-A".into(),
            command_decisions: vec!["accept".into(), "cancel".into()],
            file_decisions: vec![],
        })));
        // 定序：A 的结论过了最终身份检查、到达安装点时，切换到 B（换代 + 换指纹）。
        let a = adapter.clone();
        adapter.set_schema_install_gate(Some(Box::new(move || {
            a.inner.epoch.fetch_add(1, Ordering::SeqCst);
            let mut slot = a.inner.probe_target.lock().unwrap();
            *slot = Some(ProbeTarget {
                epoch: 1,
                platform: "macos".into(),
                build: "same-build".into(),
                key: None,
            });
        })));
        adapter.ensure_schema_evidence().await;
        assert!(
            adapter.approval_policy().is_empty(),
            "A 的证据不得安装到 B：B 的审批策略不得被旧代际结论解锁"
        );
        // B 自身的证据路径不受影响：清掉定序缝，B 身份重新取证照常安装。
        adapter.set_probe_key_override(Some("macos|same-build|cli-b|1".into()));
        adapter.set_schema_install_gate(None);
        adapter.ensure_schema_evidence().await;
        assert!(
            !adapter.approval_policy().is_empty(),
            "B 自身的取证应正常安装（不被 A 的迟到结果挤占）"
        );
    }

    /// 终审 F2（持久恢复臂）：同类窗口——持久决策词证据命中后的最终身份检查通过、
    /// 装回前换代到 B：A 的持久证据不得装进 B 身份（B 的策略保持空，直到 B 自己
    /// 命中/取证自己的证据）。
    #[tokio::test]
    async fn persisted_schema_restore_cannot_install_onto_new_generation() {
        #[derive(Default)]
        struct PersistedLedger {
            words: std::sync::Mutex<Option<super::super::probe::PersistedSchemaEvidence>>,
        }
        impl ProbeLedger for PersistedLedger {
            fn get(&self, _: &str, _: &str) -> Option<super::super::probe::ProbeLedgerEntry> {
                None
            }
            fn record(&self, _: &str, _: &str, _: &ProbeOutcome, _: Option<&str>) {}
            fn load_schema_evidence(
                &self,
                _probe_key: &str,
            ) -> Option<super::super::probe::PersistedSchemaEvidence> {
                self.words.lock().unwrap().clone()
            }
            fn store_schema_evidence(
                &self,
                _: &str,
                _: &super::super::probe::schema_probe::SchemaEvidence,
                _: i64,
            ) {
            }
        }
        let adapter = DesktopIpcAdapter::new(
            Arc::new(NoopConnector),
            ProtocolProfile::unverified("macos", "same-build"),
        );
        adapter.set_probe_key_override(Some("macos|same-build|cli-a|1".into()));
        adapter.set_probe_ledger(Arc::new(PersistedLedger {
            words: std::sync::Mutex::new(Some(super::super::probe::PersistedSchemaEvidence {
                fingerprint: "persisted-of-A".into(),
                command_decisions: vec!["accept".into(), "cancel".into()],
                file_decisions: vec![],
                evidenced_at: 1_000,
            })),
        }));
        // 定序：持久证据命中后的身份检查通过、装回前切换到 B。
        let a = adapter.clone();
        adapter.set_schema_install_gate(Some(Box::new(move || {
            a.inner.epoch.fetch_add(1, Ordering::SeqCst);
            let mut slot = a.inner.probe_target.lock().unwrap();
            *slot = Some(ProbeTarget {
                epoch: 1,
                platform: "macos".into(),
                build: "same-build".into(),
                key: None,
            });
        })));
        adapter.ensure_schema_evidence().await;
        assert!(
            adapter.approval_policy().is_empty(),
            "A 的持久证据不得装进 B：恢复窗口换代的结论同样不跨身份"
        );
    }

    /// Gate A #2：**已取证不随断链回落**——shipped build 的 profile 随断链回落
    /// `closed()`（R3 诚实缺省），但「已取证」维度必须继续如实：证据档看**最近一次
    /// 成功建连的档位**（持久留存），不看当前连接。「可尝试」维度则随断链回落
    /// （两维度分列，S0 §3.1 裁决）。
    #[tokio::test]
    async fn shipped_build_evidenced_survives_link_down() {
        let adapter = DesktopIpcAdapter::new(
            Arc::new(NoopConnector),
            crate::services::codex_desktop_ipc::profile::profile_for("macos", "26.1013.31256")
                .expect("shipped build"),
        );
        let status = adapter.adapt_status(None).await;
        assert!(
            status.evidenced.state && status.evidenced.send,
            "链路健康时 shipped 即已取证"
        );
        // 断链：profile 回落 closed（ensure_connection 失败臂同款手工复现）
        adapter.set_profile(ProtocolProfile::closed());
        let status = adapter.adapt_status(None).await;
        assert!(
            status.evidenced.state,
            "已取证（读链）不随断链回落——S1 登记契约"
        );
        assert!(
            status.evidenced.send,
            "已取证（写档）不随断链回落——S1 登记契约"
        );
        assert!(
            !status.attemptable.send,
            "「可尝试」随断链回落（closed 档能力位全关；与已取证分列）"
        );
    }

    /// S1/T4：**旧 epoch 成功不解锁新连接**——读链证据旗标按「连接代际 + build 目标」
    /// 隔离：旧代际确立的证据在换代（同 build 重连）后立即失效，新连接的首次触发必须
    /// 重新走「查账本/重探」路径（不得因旧代际的在途成功短路）。兼容档 state=true
    /// 只是「可尝试」，不是代际间的免检通行证（S0 §3.1 两维度裁决）。
    #[tokio::test]
    async fn old_epoch_success_does_not_unlock_reconnected_target() {
        let adapter = DesktopIpcAdapter::new(
            Arc::new(NoopConnector),
            crate::services::codex_desktop_ipc::profile::compat_send_profile("macos", "same-build"),
        );
        let ledger = Arc::new(ProbedLedger::default());
        adapter.set_probe_ledger(ledger.clone());
        adapter.set_probe_key_override(Some("cli-a".into()));
        let old_target = adapter.probe_target_snapshot().unwrap();
        // 旧代际内：探测成功确立证据（旗标对当前 target 成立）
        adapter.mark_state_evidenced(&old_target);
        assert!(
            adapter.state_evidence_current(),
            "旧代际内证据旗标成立（同代复用不重探）"
        );
        // 换代（同 build 重连，epoch 递增、target 换新）
        {
            let mut slot = adapter.inner.probe_target.lock().unwrap();
            adapter.inner.epoch.fetch_add(1, Ordering::SeqCst);
            *slot = Some(ProbeTarget {
                epoch: 1,
                platform: "macos".into(),
                build: "same-build".into(),
                key: None,
            });
        }
        assert!(
            !adapter.state_evidence_current(),
            "换代后旧代际证据旗标必须失效"
        );
        // 新连接首次触发：必须实际查账本（cli-b 无证据 → 继续走探/连路径），
        // 不得因旧代际成功而短路——「旧 epoch 成功不解锁新连接」的可观察面。
        adapter.set_probe_key_override(Some("cli-b".into()));
        let key = ThreadKey {
            host_id: "local".into(),
            thread_id: "t-new-conn".into(),
        };
        adapter.ensure_read_evidence(&key).await;
        assert_eq!(
            ledger.gets.load(Ordering::SeqCst),
            1,
            "换代后必须重新查账本（不得凭旧 epoch 成功短路）"
        );
        // 新代际 DTO 口径：尚无成功证据（旧 epoch 的成功不计入新连接）
        let status = adapter.adapt_status(None).await;
        assert!(!status.evidenced.state, "新连接尚无成功证据");
        assert_eq!(
            status.connection_epoch, 1,
            "DTO 携带当前连接代际（在途结果隔离键）"
        );
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
        //（interrupt-turn 已随原生打断功能入表，另取形似名）
        let outcome = adapter
            .dispatch(
                "thread-follower-interrupt-turn-legacy",
                None,
                json!({}),
                lease(),
                validity,
                None,
                None,
                false,
                Default::default(),
                None,
                None,
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
        // 原生打断方法已登记（基值 4，2026-10-21 静态取证；main 合并）
        assert_eq!(
            profile.version("thread-follower-interrupt-turn", None),
            Ok(4),
            "interrupt-turn 必须从方法表取版本（基值 4）"
        );
        // 未登记方法 → UnsupportedMethod（单一真相源明确拒绝，不猜版本）
        assert_eq!(
            profile.version("thread-follower-interrupt-turn-legacy", None),
            Err(ProtocolError::UnsupportedMethod(
                "thread-follower-interrupt-turn-legacy".into()
            ))
        );
    }
}

#[cfg(test)]
mod blocking_answer_validation_tests {
    //! T04：阻塞问答的服务端校验（未知 qid／缺题／非法选项／多值边界）——
    //! **零写出**语义由 `into_prepared` 的 Err 保证（派发前的最后一道组装门）。

    use super::*;
    use crate::services::codex_desktop_ipc::capability::{
        ContractMatch, EffectiveOperations, OperationContext, OperationPolicy,
    };
    use crate::services::codex_desktop_ipc::types::{
        AnswerPolicy, PendingKind, QuestionOption, QuestionView,
    };

    fn ops_allowing_blocking() -> EffectiveOperations {
        let mut context = OperationContext {
            target_fresh: true,
            auto_adapt_enabled: true,
            runtime_status: "idle".into(),
            blocking_pending: true,
            queue_paused: false,
            read_evidence: true,
            shipped: Default::default(),
            observed: Default::default(),
            schema: Default::default(),
            contract: Default::default(),
            contract_error: None,
        };
        context.contract.insert(
            CodexOperation::BlockingInput,
            ContractMatch {
                method: "thread-follower-submit-user-input".into(),
                base_version: 1,
                request_rule: "conversationId+requestId+response".into(),
                response_rule: "ok".into(),
                semantic_rule: "blocking user-input v1".into(),
            },
        );
        OperationPolicy.evaluate(&context)
    }

    fn question(id: &str, options: &[&str], is_other: bool) -> QuestionView {
        QuestionView {
            id: id.into(),
            header: String::new(),
            question: format!("题 {id}"),
            is_other,
            is_secret: false,
            options: options
                .iter()
                .map(|label| QuestionOption {
                    label: (*label).into(),
                    description: String::new(),
                })
                .collect(),
            answer_policy: AnswerPolicy::verified(is_other),
        }
    }

    fn snapshot(questions: Vec<QuestionView>, token_valid: bool) -> RespondSnapshot {
        let pending = crate::services::codex_desktop_ipc::types::PendingView {
            request_key: "n:187".into(),
            context_token: "tok".into(),
            kind: PendingKind::UserInput,
            actionable: true,
            disabled_reason_code: None,
            allowed_decisions: Vec::new(),
            command: None,
            cwd: None,
            reason: None,
            file_summary: None,
            permission_summary: None,
            questions,
        };
        RespondSnapshot {
            caps: CapabilitySet {
                send: true,
                state: true,
                command: false,
                permissions: false,
                blocking_input: false,
                interrupt: false,
            },
            operations: ops_allowing_blocking(),
            live_pendings: vec![("n:187".into(), "tok".into())],
            pending: Some(pending),
            token_valid,
            mirror_lease: None,
            raw_requests: None,
            request_id: Some(serde_json::json!(187)),
        }
    }

    fn key() -> ThreadKey {
        ThreadKey {
            host_id: "local".into(),
            thread_id: "thread-1".into(),
        }
    }

    fn answers(pairs: &[(&str, &[&str])]) -> PendingAction {
        PendingAction::Answer(
            pairs
                .iter()
                .map(|(qid, values)| {
                    (
                        (*qid).to_string(),
                        values.iter().map(|v| (*v).to_string()).collect::<Vec<_>>(),
                    )
                })
                .collect(),
        )
    }

    /// 未知 build（静态 `caps.blocking_input=false`）经契约匹配放行：多题多值一帧
    /// 正式响应（不是普通消息、不是 steer）。
    #[test]
    fn f08p_t04_unknown_build_blocking_multi_is_formal_response() {
        let snap = snapshot(
            vec![
                question("m1", &["甲", "乙", "丙"], false),
                question("m2", &["是", "否"], false),
                question("m3", &["北京", "上海"], true),
            ],
            true,
        );
        let prepared = snap
            .into_prepared(
                &key(),
                "n:187",
                "tok",
                &answers(&[("m1", &["甲", "丙"]), ("m2", &["是"]), ("m3", &["自填"])]),
            )
            .expect("contract-matched blocking answer must be allowed");
        assert_eq!(prepared.method, "thread-follower-submit-user-input");
        assert_eq!(
            prepared.params["response"]["answers"]["m1"],
            serde_json::json!({"answers": ["甲", "丙"]})
        );
        assert_eq!(
            prepared.params["response"]["answers"]["m3"],
            serde_json::json!({"answers": ["自填"]})
        );
        assert_eq!(prepared.params["requestId"], serde_json::json!(187));
    }

    /// 未知 qid / 缺题 / 非法选项 / 超数量 → 组装门拒绝（零写出）。
    #[test]
    fn f08p_t04_unknown_qid_missing_required_or_illegal_option_zero_write() {
        let snap = snapshot(
            vec![
                question("q1", &["甲", "乙"], false),
                question("q2", &["是"], false),
            ],
            true,
        );
        // 未知 qid（多答一题）
        let err = snap
            .clone()
            .into_prepared(
                &key(),
                "n:187",
                "tok",
                &answers(&[("q1", &["甲"]), ("q2", &["是"]), ("q9", &["x"])]),
            )
            .expect_err("unknown qid must be rejected");
        assert_eq!(err, "answers_mismatch");
        // 缺题
        let err = snap
            .clone()
            .into_prepared(&key(), "n:187", "tok", &answers(&[("q1", &["甲"])]))
            .expect_err("missing question must be rejected");
        assert_eq!(err, "answers_mismatch");
        // 非法选项（isOther=false 时只能选给定标签）
        let err = snap
            .clone()
            .into_prepared(
                &key(),
                "n:187",
                "tok",
                &answers(&[("q1", &["丙"]), ("q2", &["是"])]),
            )
            .expect_err("illegal option must be rejected");
        assert_eq!(err, "answer_not_option");
        // 超单题数量上限（32）
        let many: Vec<&str> = (0..33).map(|_| "甲").collect();
        let err = snap
            .clone()
            .into_prepared(
                &key(),
                "n:187",
                "tok",
                &answers(&[("q1", &many), ("q2", &["是"])]),
            )
            .expect_err("too many answers must be rejected");
        assert_eq!(err, "too_many_answers");
        // 超长答案（10_000 字符）
        let long = "x".repeat(10_001);
        let err = snap
            .clone()
            .into_prepared(
                &key(),
                "n:187",
                "tok",
                &answers(&[("q1", &[long.as_str()]), ("q2", &["是"])]),
            )
            .expect_err("too long answer must be rejected");
        assert_eq!(err, "answer_too_long");
    }

    /// T05：Reject 只发 decline、Cancel 只发 cancel——**拒绝不得隐式转成终止**。
    #[test]
    fn f08p_t05_reject_never_silently_cancels_turn() {
        let mut pending = crate::services::codex_desktop_ipc::types::PendingView {
            request_key: "n:18".into(),
            context_token: "tok".into(),
            kind: PendingKind::CommandApproval,
            actionable: true,
            disabled_reason_code: None,
            // 显式列表只给了 accept + cancel（没有 decline）
            allowed_decisions: vec!["accept".into(), "cancel".into()],
            command: Some("echo hi".into()),
            cwd: None,
            reason: None,
            file_summary: None,
            permission_summary: None,
            questions: Vec::new(),
        };
        let snap = RespondSnapshot {
            caps: CapabilitySet {
                send: true,
                state: true,
                command: true,
                permissions: false,
                blocking_input: false,
                interrupt: false,
            },
            operations: ops_allowing_blocking(),
            live_pendings: vec![("n:18".into(), "tok".into())],
            pending: Some(pending.clone()),
            token_valid: true,
            mirror_lease: None,
            raw_requests: None,
            request_id: Some(serde_json::json!(18)),
        };
        let err = snap
            .clone()
            .into_prepared(&key(), "n:18", "tok", &PendingAction::Reject)
            .expect_err("未列 decline 时拒绝不得悄悄发 cancel");
        assert_eq!(err, "action_mismatch");
        // Cancel 明确发 cancel。
        let prepared = snap
            .clone()
            .into_prepared(&key(), "n:18", "tok", &PendingAction::Cancel)
            .expect("cancel 合法");
        assert_eq!(prepared.params["decision"], serde_json::json!("cancel"));
        // Approve 发 accept。
        let prepared = snap
            .into_prepared(&key(), "n:18", "tok", &PendingAction::Approve)
            .expect("approve 合法");
        assert_eq!(prepared.params["decision"], serde_json::json!("accept"));
        // 允许 decline 时 Reject 只发 decline（不转 cancel）。
        pending.allowed_decisions = vec!["accept".into(), "decline".into(), "cancel".into()];
        let snap = RespondSnapshot {
            caps: CapabilitySet {
                send: true,
                state: true,
                command: true,
                permissions: false,
                blocking_input: false,
                interrupt: false,
            },
            operations: ops_allowing_blocking(),
            live_pendings: vec![("n:18".into(), "tok".into())],
            pending: Some(pending),
            token_valid: true,
            mirror_lease: None,
            raw_requests: None,
            request_id: Some(serde_json::json!(18)),
        };
        let prepared = snap
            .into_prepared(&key(), "n:18", "tok", &PendingAction::Reject)
            .expect("reject 合法");
        assert_eq!(prepared.params["decision"], serde_json::json!("decline"));
    }

    /// 旧 token（卡换代）→ stale_context（零写出）；单值策略不允许多值。
    #[test]
    fn f08p_t04_request_id_reused_after_epoch_rejects_old_token() {
        let snap = snapshot(vec![question("q1", &["甲"], false)], false);
        let err = snap
            .into_prepared(&key(), "n:187", "old-token", &answers(&[("q1", &["甲"])]))
            .expect_err("stale token must be rejected");
        assert_eq!(err, "stale_context");

        // 保守单值策略（老载荷缺 answerPolicy）拒绝多值。
        let mut single = question("q1", &["甲", "乙"], false);
        single.answer_policy = AnswerPolicy::conservative();
        let snap = snapshot(vec![single], true);
        let err = snap
            .into_prepared(&key(), "n:187", "tok", &answers(&[("q1", &["甲", "乙"])]))
            .expect_err("single-value policy must reject multiple");
        assert_eq!(err, "answer_not_multiple");
    }
}

#[cfg(test)]
mod publish_protocol_tests {
    //! 复审 round 5（W1/V3 收口）：发布点内容版本协议的单元钉——CAS 预留语义与
    //! 去重 tracker 的「只读判定 / 取得资格后才提交」拆分。定序级端到端验收在
    //! `tests/codex_ipc_remote.rs` 的 sse_publish_window_* / sse_state_changed_
    //! publish_window_* 两条真实 SSE 红测。

    use super::*;

    pub(super) struct NoopConnector;
    impl Connector for NoopConnector {
        fn connect(
            &self,
        ) -> Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<super::super::transport::BoxIo, ProtocolError>,
                    > + Send
                    + '_,
            >,
        > {
            Box::pin(async { Err(ProtocolError::Disconnected("test connector".into())) })
        }
    }

    fn adapter() -> DesktopIpcAdapter {
        DesktopIpcAdapter::new(Arc::new(NoopConnector), ProtocolProfile::closed())
    }

    fn key(id: &str) -> ThreadKey {
        ThreadKey {
            host_id: "local".into(),
            thread_id: id.into(),
        }
    }

    /// CAS 预留语义：预留成功 = 观察仍是当前头（原子推进恰一版）；观察被越过 =
    /// None 且**头不得被失败尝试推进**——「检查」与「取得发布资格」由此同一原子
    /// 步骤（W1：不再分离 load 与无条件 fetch_add）。
    #[test]
    fn reserve_publish_seq_grants_only_when_observation_is_current() {
        let a = adapter();
        assert_eq!(a.inner.projection_seq.load(Ordering::SeqCst), 0);
        assert_eq!(
            a.reserve_publish_seq(0),
            Some(1),
            "当前观察取得资格并预留下一版"
        );
        assert_eq!(a.inner.projection_seq.load(Ordering::SeqCst), 1);
        assert_eq!(
            a.reserve_publish_seq(0),
            None,
            "过期观察（头已 1）不得取得资格"
        );
        assert_eq!(
            a.inner.projection_seq.load(Ordering::SeqCst),
            1,
            "失败的预留不得推进发布头"
        );
        assert_eq!(
            a.reserve_publish_seq(1),
            Some(2),
            "与头一致的观察照常取得资格"
        );
    }

    /// 去重 tracker 与发布资格的**原子 grant**：放弃的尝试（CAS 失败耗尽重试后
    /// 返回 Conflict）不得在 tracker 留下未发布值的假记录，否则后续同值的真实
    /// 发布会被误抑制（暂停事实从未到达 SSE 却被当成「已发布过」）。旧实现
    /// `pause_publish_changed` 检查即记录 → 第二次判定即 false，本用例红。W1 语义
    /// 在 round 6（Q1）的融合 API 下保持并加固：去重判定、CAS 预留、登记同一
    /// 临界区（并发读者看不到中间态）。复审 round 7（Q1）：事实在 grant 临界区内
    /// 重读并以重读值为准——「观察后、grant 前事实经真实落行 + 同值恢复回到原值」
    /// 时，旧观察不得作为提交依据复活为 stale 广播（端到端定序见红测
    /// `sse_pause_stale_observation_cannot_survive_same_value_resume`）。
    #[test]
    fn pause_publish_tracker_records_only_on_grant() {
        let a = adapter();
        let k = key("t-1");
        // 注入事实源（原子布尔扮演真实 DB 暂停行的存在/清行；true = 暂停中）。
        let fact = Arc::new(AtomicBool::new(true));
        let fact2 = fact.clone();
        a.set_queue_pause_source(Some(Arc::new(move |_tid: &str| {
            fact2.load(Ordering::SeqCst)
        })));
        assert!(
            matches!(a.pause_publish_grant(&k, 999), PausePublishGrant::Conflict),
            "过期观察（头为 0）不得取得资格"
        );
        assert!(
            matches!(
                a.pause_publish_grant(&k, 0),
                PausePublishGrant::Granted(1, true)
            ),
            "失败的尝试（Conflict）不得记账：同值真实尝试仍应取得资格并发布（携带重读值）"
        );
        assert!(
            matches!(a.pause_publish_grant(&k, 1), PausePublishGrant::Deduped),
            "已登记（=已发布）的值抑制同值发布（V1 静默语义）"
        );
        // 真实恢复：事实翻转 false，重读 ≠ tracker → 放行并携带重读值。
        fact.store(false, Ordering::SeqCst);
        assert!(
            matches!(
                a.pause_publish_grant(&k, 1),
                PausePublishGrant::Granted(2, false)
            ),
            "反向翻转照常放行"
        );
        // round 7 Q1 缺陷窗：A 真实落行（观察到 true）→ B 同值恢复清行，两步都在
        // A 的 grant 之前完成 → grant 重读 false == tracker false → 正确去重；
        // 修复前旧观察 true 经参数传入会广播 stale true 并登记（红测定序的单元面）。
        fact.store(true, Ordering::SeqCst);
        fact.store(false, Ordering::SeqCst);
        assert!(
            matches!(a.pause_publish_grant(&k, 2), PausePublishGrant::Deduped),
            "重读事实==上次广播值 → 去重（同值恢复不作废、也不复活旧观察）"
        );
        // 第三次真实变化照常通知（不被 stale true 吞掉）。
        fact.store(true, Ordering::SeqCst);
        assert!(matches!(
            a.pause_publish_grant(&k, 2),
            PausePublishGrant::Granted(3, true)
        ));
        assert!(matches!(
            a.pause_publish_grant(&k, 3),
            PausePublishGrant::Deduped
        ));
    }
}

#[cfg(test)]
mod contract_evidence_tests {
    //! T02：逐操作契约证据的安装/持久/取消/静默（adapter 级定序）。
    //! 受控 reader + 内存 store：**不扫描、不连接本机 App，不读真实安装包**。

    use super::*;

    /// 本模块不建真实连接：契约识别只取决于注入的 reader。
    pub(super) struct NoopConnector;

    impl Connector for NoopConnector {
        fn connect(
            &self,
        ) -> Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<super::super::transport::BoxIo, ProtocolError>,
                    > + Send
                    + '_,
            >,
        > {
            Box::pin(async { Err(ProtocolError::Disconnected("test connector".into())) })
        }
    }

    // ============================================================
    // T02：逐操作契约证据的安装/持久/取消/静默（adapter 级定序）
    // ============================================================

    use crate::database::dao::codex_ipc_probe::StoredContractEvidence;
    use crate::services::codex_desktop_ipc::capability::{
        ContractEvidenceStore, ContractMatch, DesktopContractEvidence, DesktopContractReader,
        CONTRACT_RULE_VERSION,
    };

    fn t02_key(id: &str) -> ThreadKey {
        ThreadKey {
            host_id: "local".into(),
            thread_id: id.into(),
        }
    }

    fn t02_evidence() -> DesktopContractEvidence {
        let mut matches = std::collections::BTreeMap::new();
        for (operation, method, version) in [
            (CodexOperation::Send, "thread-follower-start-turn", 2),
            (
                CodexOperation::BlockingInput,
                "thread-follower-submit-user-input",
                1,
            ),
            (
                CodexOperation::AsyncAnswerIdle,
                "thread-follower-start-turn",
                2,
            ),
            (
                CodexOperation::Interrupt,
                "thread-follower-interrupt-turn",
                4,
            ),
        ] {
            matches.insert(
                operation,
                ContractMatch {
                    method: method.into(),
                    base_version: version,
                    request_rule: "conversationId".into(),
                    response_rule: "ok".into(),
                    semantic_rule: "rule".into(),
                },
            );
        }
        DesktopContractEvidence {
            probe_key: "macos|build-a|cli-a|1".into(),
            package_sha256: "a".repeat(64),
            rule_version: CONTRACT_RULE_VERSION,
            matches,
            package_size: None,
            package_mtime_ms: None,
        }
    }

    /// 受控读取器：可注入证据或错误，可注入"门"制造在途/取消窗；记录调用次数。
    struct ScriptedReader {
        calls: AtomicU64,
        evidence: Option<DesktopContractEvidence>,
        gate: Option<Arc<tokio::sync::Notify>>,
    }

    impl ScriptedReader {
        fn ok() -> Self {
            Self {
                calls: AtomicU64::new(0),
                evidence: Some(t02_evidence()),
                gate: None,
            }
        }
        /// R17：证据带**真实临时包**的大小/mtime（重启复用前的核对能通过）。
        fn ok_pinned_to(path: &std::path::Path) -> Self {
            let meta = std::fs::metadata(path).expect("temp package");
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64);
            let mut evidence = t02_evidence();
            evidence.package_size = Some(meta.len());
            evidence.package_mtime_ms = mtime;
            // H14：缓存复用要核对**内容哈希**——脚本证据必须与真实临时包一致。
            evidence.package_sha256 =
                crate::services::codex_desktop_ipc::probe::desktop_contract::hash_package(path)
                    .expect("hash temp package");
            Self {
                calls: AtomicU64::new(0),
                evidence: Some(evidence),
                gate: None,
            }
        }
        fn failing() -> Self {
            Self {
                calls: AtomicU64::new(0),
                evidence: None,
                gate: None,
            }
        }
    }

    impl DesktopContractReader for ScriptedReader {
        fn read(
            &self,
            _target: ContractTarget,
        ) -> Pin<
            Box<
                dyn std::future::Future<Output = Result<DesktopContractEvidence, ProtocolError>>
                    + Send,
            >,
        > {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let evidence = self.evidence.clone();
            let gate = self.gate.clone();
            Box::pin(async move {
                if let Some(gate) = gate {
                    gate.notified().await;
                }
                evidence.ok_or_else(|| ProtocolError::InvalidState("scripted failure".into()))
            })
        }
    }

    /// 内存持久存储：记录写入次数；`load` 按 (probe_key, rule_version) 过滤。
    #[derive(Default)]
    pub(super) struct MemoryContractStore {
        rows: std::sync::Mutex<Vec<StoredContractEvidence>>,
        stores: AtomicU64,
    }

    impl ContractEvidenceStore for MemoryContractStore {
        fn load(&self, probe_key: &str, rule_version: u32) -> Option<Vec<StoredContractEvidence>> {
            let rows: Vec<_> = self
                .rows
                .lock()
                .unwrap()
                .iter()
                .filter(|row| row.probe_key == probe_key && row.rule_version == rule_version)
                .cloned()
                .collect();
            (!rows.is_empty()).then_some(rows)
        }

        fn store(&self, evidence: &DesktopContractEvidence, at_ms: i64) {
            self.stores.fetch_add(1, Ordering::SeqCst);
            let mut rows = self.rows.lock().unwrap();
            for (operation, matched) in &evidence.matches {
                rows.push(StoredContractEvidence {
                    probe_key: evidence.probe_key.clone(),
                    operation: operation.as_str().into(),
                    rule_version: evidence.rule_version,
                    package_sha256: evidence.package_sha256.clone(),
                    method: matched.method.clone(),
                    base_version: matched.base_version,
                    request_rule: matched.request_rule.clone(),
                    response_rule: matched.response_rule.clone(),
                    semantic_rule: matched.semantic_rule.clone(),
                    evidenced_at: at_ms,
                    observed_at: at_ms,
                    package_size: evidence.package_size.map(|v| v as i64),
                    package_mtime_ms: evidence.package_mtime_ms,
                });
            }
        }
    }

    fn t02_adapter_at(build: &str, package: &std::path::Path) -> DesktopIpcAdapter {
        let adapter = t02_adapter(build);
        adapter.set_contract_target_override(Some(ContractTarget {
            probe_key: crate::services::codex_desktop_ipc::probe::fingerprint::ProbeKey::new(
                "macos",
                Some(build),
                Some("cli-a"),
            ),
            connection_epoch: 0,
            package_path: package.to_path_buf(),
            cli_path: std::path::PathBuf::from("/nonexistent/codex"),
        }));
        adapter
    }

    fn t02_adapter(build: &str) -> DesktopIpcAdapter {
        let adapter = DesktopIpcAdapter::new(
            Arc::new(NoopConnector),
            ProtocolProfile::unverified("macos", build),
        );
        adapter.set_probe_key_override(Some(format!("macos|{build}|cli-a|1")));
        adapter.set_contract_target_override(Some(ContractTarget {
            probe_key: crate::services::codex_desktop_ipc::probe::fingerprint::ProbeKey::new(
                "macos",
                Some(build),
                Some("cli-a"),
            ),
            connection_epoch: 0,
            package_path: std::path::PathBuf::from("/nonexistent/app.asar"),
            cli_path: std::path::PathBuf::from("/nonexistent/codex"),
        }));
        adapter
    }

    fn t02_evidence_installed(adapter: &DesktopIpcAdapter) -> bool {
        adapter
            .inner
            .contract_evidence
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|(epoch, _, _)| *epoch == adapter.inner.epoch.load(Ordering::SeqCst))
    }

    /// 复审 T02 核心定序：读取返回后身份已换代 → 证据**不安装、不落账**，
    /// 新目标的相关操作保持关闭。
    #[tokio::test]
    async fn f08p_t02_late_contract_not_installed_or_persisted() {
        let adapter = t02_adapter("build-a");
        let store = Arc::new(MemoryContractStore::default());
        adapter.set_contract_evidence_store(Some(store.clone()));
        let reader = Arc::new(ScriptedReader::ok());
        adapter.set_contract_reader(Some(reader.clone()));
        let gate_adapter = adapter.clone();
        adapter.set_contract_install_gate(Some(Box::new(move || {
            // 窗内重连到 build-b（新代际 + 新目标 + 新指纹）
            gate_adapter.inner.epoch.fetch_add(1, Ordering::SeqCst);
            *gate_adapter.inner.probe_target.lock().unwrap() = Some(ProbeTarget {
                epoch: 1,
                platform: "macos".into(),
                build: "build-b".into(),
                key: Some("macos|build-b|cli-b|1".into()),
            });
        })));
        adapter.ensure_contract_evidence().await;
        assert_eq!(reader.calls.load(Ordering::SeqCst), 1, "读取确实发生");
        assert!(!t02_evidence_installed(&adapter), "旧目标证据不得安装");
        assert_eq!(store.stores.load(Ordering::SeqCst), 0, "旧目标证据不得落账");
        let ops = adapter
            .projection(&t02_key("t-late"))
            .await
            .effective_operations;
        assert!(!ops.allows(CodexOperation::BlockingInput));
        assert!(!ops.allows(CodexOperation::AsyncAnswerIdle));
    }

    /// 成功证据跨"暂时失败 + 重启"保留：第二个 adapter 直接走持久证据，零重读。
    #[tokio::test]
    async fn f08p_t02_success_survives_failure_restart() {
        // R17：复用持久证据前要核对**当前包内容**——本用例给真实临时包，
        // 指纹一致才允许零重读；包变化/指纹缺失的用例见 codex_evidence 单测。
        let dir = tempfile::tempdir().unwrap();
        let package = dir.path().join("app.asar");
        std::fs::write(&package, b"official-package-bytes").unwrap();
        let store = Arc::new(MemoryContractStore::default());
        let first = t02_adapter_at("build-a", &package);
        first.set_contract_evidence_store(Some(store.clone()));
        let reader = Arc::new(ScriptedReader::ok_pinned_to(&package));
        first.set_contract_reader(Some(reader.clone()));
        first.ensure_contract_evidence().await;
        assert_eq!(reader.calls.load(Ordering::SeqCst), 1);
        assert!(t02_evidence_installed(&first));
        assert_eq!(store.stores.load(Ordering::SeqCst), 1);

        // 重启：新 adapter + 同一持久存储 + **会失败**的 reader。
        let restarted = t02_adapter_at("build-a", &package);
        restarted.set_contract_evidence_store(Some(store.clone()));
        let failing = Arc::new(ScriptedReader::failing());
        restarted.set_contract_reader(Some(failing.clone()));
        restarted.ensure_contract_evidence().await;
        assert_eq!(
            failing.calls.load(Ordering::SeqCst),
            0,
            "持久成功证据命中时不得重跑完整识别"
        );
        assert!(t02_evidence_installed(&restarted));
        let projection = restarted.projection(&t02_key("t-restart")).await;
        assert_eq!(
            projection
                .effective_operations
                .get(CodexOperation::BlockingInput)
                .unwrap()
                .evidence,
            crate::services::codex_desktop_ipc::capability::EvidenceSource::ContractMatched,
            "恢复后仍有当前契约证据（断链只是临时状态，不是永久不支持）"
        );

        // 暂时失败不抹去历史成功：清掉内存槽后重跑，持久证据装回。
        restarted.inner.contract_evidence.lock().unwrap().take();
        restarted.ensure_contract_evidence().await;
        assert!(t02_evidence_installed(&restarted), "持久成功证据重新装回");
    }

    /// 读取任务被取消：单飞锁释放，后来者可重试（不留假"已识别"）。
    #[tokio::test]
    async fn f08p_t02_cancelled_reader_wakes_waiters_and_retries() {
        let adapter = t02_adapter("build-a");
        let gate = Arc::new(tokio::sync::Notify::new());
        let reader = Arc::new(ScriptedReader {
            calls: AtomicU64::new(0),
            evidence: Some(t02_evidence()),
            gate: Some(gate.clone()),
        });
        adapter.set_contract_reader(Some(reader.clone()));

        let task_adapter = adapter.clone();
        let task = tokio::spawn(async move { task_adapter.ensure_contract_evidence().await });
        while reader.calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        task.abort();
        let _ = task.await;
        assert!(!t02_evidence_installed(&adapter), "取消不得留下证据");

        // 后来者重试：reader 换成就绪的，必须能装好证据。
        gate.notify_waiters();
        let retry_reader = Arc::new(ScriptedReader::ok());
        adapter.set_contract_reader(Some(retry_reader.clone()));
        adapter.ensure_contract_evidence().await;
        assert_eq!(
            retry_reader.calls.load(Ordering::SeqCst),
            1,
            "取消后必须重试"
        );
        assert!(t02_evidence_installed(&adapter));
    }

    /// 事实不变 → 同一策略 revision（静默期零新增投影）；无镜像时重发布静默；
    /// 证据命中不重读安装包。
    #[tokio::test]
    async fn f08p_t02_policy_republish_order_and_quiet() {
        let adapter = t02_adapter("build-a");
        let reader = Arc::new(ScriptedReader::ok());
        adapter.set_contract_reader(Some(reader.clone()));
        let mut events = adapter.subscribe();
        adapter.ensure_contract_evidence().await;
        assert!(t02_evidence_installed(&adapter));
        assert!(events.try_recv().is_err(), "没有镜像时重发布必须静默");
        let first = adapter.projection(&t02_key("t-quiet")).await;
        let second = adapter.projection(&t02_key("t-quiet")).await;
        assert_eq!(
            first.effective_operations.policy_revision, second.effective_operations.policy_revision,
            "同一组事实 → 同一策略版本（否则下游会被无谓刷新）"
        );
        adapter.ensure_contract_evidence().await;
        assert_eq!(
            reader.calls.load(Ordering::SeqCst),
            1,
            "证据命中后不得重读安装包"
        );
    }
}

#[cfg(test)]
mod h14_package_content_tests {
    use super::*;
    use crate::services::codex_desktop_ipc::capability::{
        ContractMatch, DesktopContractEvidence, CONTRACT_RULE_VERSION,
    };
    use crate::services::codex_desktop_ipc::probe::desktop_contract::hash_package;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::services::codex_desktop_ipc::adapter::contract_evidence_tests::{
        MemoryContractStore, NoopConnector,
    };

    struct FailingReader {
        calls: AtomicU64,
    }

    impl crate::services::codex_desktop_ipc::capability::DesktopContractReader for FailingReader {
        fn read(
            &self,
            _target: crate::services::codex_desktop_ipc::capability::ContractTarget,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<DesktopContractEvidence, super::ProtocolError>,
                    > + Send,
            >,
        > {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Err(super::ProtocolError::InvalidState(
                    "scripted failure".into(),
                ))
            })
        }
    }

    /// N01：当前包身份与**目标代际**绑定——重连换代后旧 SHA 不得代表当前包。
    #[test]
    fn n01_package_identity_is_generation_bound() {
        let adapter = DesktopIpcAdapter::new(
            Arc::new(NoopConnector),
            ProtocolProfile::unverified("macos", "build-a"),
        );
        adapter.set_probe_key_override(Some("macos|build-a|cli|1".into()));
        let epoch = adapter
            .inner
            .epoch
            .load(std::sync::atomic::Ordering::SeqCst);
        *adapter
            .inner
            .package_sha
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(super::PackageIdentity {
            epoch,
            probe_key: "macos|build-a|cli|1".into(),
            sha256: "sha-a".into(),
        });
        assert_eq!(
            adapter.observed_target().map(|t| t.package_sha256),
            Some("sha-a".into())
        );
        // 模拟重连换代：连接 epoch 前进后，旧身份槽必须失效。
        adapter
            .inner
            .epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        assert!(
            adapter.observed_target().is_none(),
            "换代后旧 SHA 不得代表当前包"
        );
        // 新代际写入新身份（真实哈希成功安装）→ 只对新包生效。
        let epoch = adapter
            .inner
            .epoch
            .load(std::sync::atomic::Ordering::SeqCst);
        *adapter
            .inner
            .package_sha
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(super::PackageIdentity {
            epoch,
            probe_key: "macos|build-a|cli|1".into(),
            sha256: "sha-b".into(),
        });
        assert_eq!(
            adapter.observed_target().map(|t| t.package_sha256),
            Some("sha-b".into())
        );
    }

    /// H14：**同 size/mtime 但内容不同** → 旧契约结论不得安装（内容哈希核对）。
    #[tokio::test]
    async fn h14_same_stat_different_content_is_not_reused() {
        let dir = tempfile::tempdir().unwrap();
        let package = dir.path().join("app.asar");
        std::fs::write(&package, b"AAAA-official").unwrap();
        let meta = std::fs::metadata(&package).unwrap();
        let mtime = meta.modified().unwrap();
        let sha = hash_package(&package).unwrap();

        let store = Arc::new(MemoryContractStore::default());
        let mut matches = std::collections::BTreeMap::new();
        matches.insert(
            crate::services::codex_desktop_ipc::capability::CodexOperation::AsyncAnswerIdle,
            ContractMatch {
                method: "thread-follower-start-turn".into(),
                base_version: 2,
                request_rule: "conversationId+turnStart".into(),
                response_rule: "result.result".into(),
                semantic_rule: "envelope".into(),
            },
        );
        let evidence = DesktopContractEvidence {
            probe_key: "macos|build-a|cli-a|1".into(),
            package_sha256: sha,
            rule_version: CONTRACT_RULE_VERSION,
            matches,
            package_size: Some(meta.len()),
            package_mtime_ms: mtime
                .duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_millis() as i64),
        };
        {
            use crate::services::codex_desktop_ipc::capability::ContractEvidenceStore;
            store.store(&evidence, 1);
        }

        // 覆盖为**同长度**内容并恢复 mtime（stat 完全相同、内容不同）
        std::fs::write(&package, b"BBBB-official").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&package)
            .unwrap()
            .set_modified(mtime)
            .unwrap();

        let adapter = DesktopIpcAdapter::new(
            Arc::new(NoopConnector),
            ProtocolProfile::unverified("macos", "build-a"),
        );
        adapter.set_probe_key_override(Some("macos|build-a|cli-a|1".into()));
        adapter.set_contract_target_override(Some(
            crate::services::codex_desktop_ipc::capability::ContractTarget {
                probe_key: crate::services::codex_desktop_ipc::probe::fingerprint::ProbeKey::new(
                    "macos",
                    Some("build-a"),
                    Some("cli-a"),
                ),
                connection_epoch: 0,
                package_path: package.clone(),
                cli_path: std::path::PathBuf::from("/nonexistent/codex"),
            },
        ));
        adapter.set_contract_evidence_store(Some(store));
        let reader = Arc::new(FailingReader {
            calls: AtomicU64::new(0),
        });
        adapter.set_contract_reader(Some(reader.clone()));
        adapter.ensure_contract_evidence().await;
        assert_eq!(
            reader.calls.load(Ordering::SeqCst),
            1,
            "内容变化必须重新识别（不得复用旧结论）"
        );
        assert!(
            !adapter
                .projection(&crate::services::codex_desktop_ipc::types::ThreadKey {
                    host_id: "local".into(),
                    thread_id: "t".into(),
                })
                .await
                .effective_operations
                .allows(
                    crate::services::codex_desktop_ipc::capability::CodexOperation::AsyncAnswerIdle
                ),
            "旧结论不得生效"
        );
    }
}
