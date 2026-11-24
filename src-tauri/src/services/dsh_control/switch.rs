//! 「远程控制 DSH」单开关（Task 4；DSH Spec §6/§6.1 主按钮全生命周期）。
//!
//! ## 职责与边界
//!
//! 一个开关拥有该实例远控的**完整生命周期**：开启 = 线性推进（检查实例 → 按需
//! 等待退出 + 安装 → 提示重开 → 核验插件加载 → 自动完成授权 → 就绪），关闭 =
//! 立即失效（取消意图、撤权、断控制台）+ 自动卸载（等待宿主正常退出）。状态机
//! 由**后端按选定实例持久化**（`<dsh_state_dir>/instances/<id>/remote-switch.json`），
//! 前端只消费后端实际状态——不落 localStorage、不自行编排。
//!
//! ## 意图绑定与迟到回执（brief 纪律）
//!
//! * 每次开启/关闭/修复意图都推进 [`SwitchRecord::generation`]（操作代次）；
//!   开启意图绑定（选定实例、操作代次、已同意范围 [`SWITCH_SCOPE`]）。
//! * 驱动器每拍先读盘核对 generation，**CAS 提交**（读 → 核对 → 改 → 原子写）：
//!   迟到的旧驱动器/旧意图在代次更替后一律退场，不得把已关闭的实例标成启用，
//!   也不得把旧意图用于另一实例（非选定实例时开启方向**暂停**，不推进不失败）。
//! * 短时握手意图复用 Task 2 机制（[`DshAuthService::begin_selected_authorization_intent`]），
//!   在目标就绪（插件运行中）才签发；TTL 过期由驱动器自动续签，不需要用户在
//!   超时后再按一次授权。
//! * 已有合法绑定的用户升级后（无开关记录 + 插件在场 + 绑定 Bound）按现有事实
//!   派生并持久化「已启用」，不要求重新授权、不误卸载。
//!
//! ## 关闭语义（§2.5 序保持；P1-1 执行面）
//!
//! 关闭命令先落 `enabled=false`（**执行面先行**：授权提交/发送最终复核/附件写盘/
//! 控制台准入四类生产执行门从这一刻起立即拒绝，撤权成败不再决定「开关说关、
//! 远控仍活」），随后**同步段**：取消待授权意图 → 撤销活跃绑定（翻闸门、断受信
//! Peer、吊销控制台会话与凭据副本）→ 落「远控已关闭 · 等待卸载」。卸载走既有
//! 管理器（先撤全部匹配绑定再移包、等待宿主正常退出、绝不强杀——M06 保证原样
//! 约束）；进行中的安装/修复作业由卸载推进段先取消（`tick_uninstall` 让位）。
//! 撤权失败保持远控关闭（enabled 不回滚）并落 Failed(revoke) + 重试入口，
//! 重试重跑撤权直到成功才进等待卸载。
//!
//! 再开启 = 一次新的明确授权（操作代次推进，旧代布防意图的迟到握手在授权提交点
//! 被拒）：旧钥随卸载**退役**（P1-2 同钥循环复装——卸载完成段删除插件身份目录并
//! 清空登记锚），再开启装出全新钥，经本次开启的布防意图自动绑定（首绑 TOFU，
//! 全程无手工步骤/无核对码）；旧钥被撤权墓碑与闸门双重拦截，不能在关闭期间自行
//! 恢复。未经退役的身份漂移（登记锚 ≠ 新钥）仍进既有可信重绑面板确认。
//!
//! ## 公开 IPC 门（裁决 R2）
//!
//! [`gate_authorization`] / [`gate_lifecycle`]：授权激活类命令（绑定/批准/布防/
//! 重绑）在开关未开启时拒绝（`DSH_SWITCH_OFF`）；生命周期类命令（安装/修复/
//! 卸载/撤权）在开关持有生命周期期间拒绝直调（`DSH_SWITCH_ACTIVE`）——底层
//! 原语保留，开关与「修复插件」经服务内部复用，不经公开门。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::auth::DshAuthService;
use super::error::{codes, DshError};
use super::identity;
use super::install::DshPluginManager;
use super::instances::InstanceRegistry;
use super::model::{
    ActivationState, AuthorizationState, DshEnvironment, InstallationState, ManagementJob,
    ManagementKind, ManagementPhase,
};

/// 已同意范围（开启意图的绑定面之一：本次**实例级**授权，不扩大为批准所有
/// 未来待批客户端）。固定值；旧记录/异范围记录不得跨用。
pub const SWITCH_SCOPE: &str = "instance-authorization";

/// 开关账本文件名（实例状态目录内；`RemoteSwitchService::record_path` 与启动
/// 扫描共用同一名字，杜绝双源漂移）。
const SWITCH_RECORD_FILE: &str = "remote-switch.json";

/// 开关推进相位（wire 词 snake_case；前端线性进度按此渲染）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwitchPhase {
    /// 检查实例（登记/归属/插件事实盘点）。
    Checking,
    /// 自动安装（作业的等待退出/Applying/Verifying 由 job 面透出）。
    Installing,
    /// 提示用户重开 DSH。
    AwaitingReopen,
    /// 核验插件已加载（宿主运行中 + 插件装载）。
    VerifyingPlugin,
    /// 自动完成授权（短时握手意图已布防/续签中）。
    Authorizing,
    /// 等待用户经既有可信重绑面板确认插件身份变化。
    RebindRequired,
    /// 已启用（发送/控制台各自就绪结果在 view 面实时给出）。
    Ready,
    /// 修复中（修复作业推进；完成后接续 [`SwitchRecord::resume_phase`]）。
    Repairing,
    /// 远控已关闭 · 等待卸载（卸载作业等待宿主正常退出后移包）。
    WaitingUninstall,
    /// 失败（意图保持；失败步骤 + 原因 + 重试入口）。
    Failed,
    /// 已关闭（终态）。
    Off,
}

/// 失败步骤（重试入口指向的线性步）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwitchStep {
    InstanceCheck,
    Install,
    Reopen,
    Verify,
    Authorize,
    Repair,
    Revoke,
    Uninstall,
    /// 撤权前登记 home 证据保全（第九轮 P1-2）：缺 `dsh_home` 的旧登记在撤权前
    /// 回填卸载目标失败（证据不足/多义/账本不可读）——宁可不撤（保持关闭侧
    /// 可重试）也不写错目标。
    HomeEvidence,
}

/// 按选定实例持久化的开关与推进账本（全部非敏感：状态机词 + 作业 id）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SwitchRecord {
    /// 用户意图：true = 启用中/已启用；false = 关闭中/已关闭。
    pub enabled: bool,
    /// 推进相位。
    pub phase: SwitchPhase,
    /// 失败步骤（phase = failed 时有值）。
    pub failed_step: Option<SwitchStep>,
    /// 失败原因码（`DSH_*`）。
    pub reason_code: Option<String>,
    /// 操作代次：每次意图（开启/关闭/修复/重试）递增；迟到驱动器凭它退场。
    pub generation: u64,
    /// 已同意范围（开启意图绑定面；跨范围记录不复用）。
    pub scope: String,
    /// 活跃管理作业 id（安装/卸载/修复；跨应用重启恢复用）。
    pub job_id: Option<String>,
    /// 活跃管理作业类别。
    pub job_kind: Option<ManagementKind>,
    /// 修复完成后的接续相位。
    pub resume_phase: Option<SwitchPhase>,
    pub updated_at_ms: i64,
}

impl Default for SwitchRecord {
    fn default() -> Self {
        Self {
            enabled: false,
            phase: SwitchPhase::Off,
            failed_step: None,
            reason_code: None,
            generation: 0,
            scope: String::new(),
            job_id: None,
            job_kind: None,
            resume_phase: None,
            updated_at_ms: 0,
        }
    }
}

impl SwitchRecord {
    fn default_with(phase: SwitchPhase) -> Self {
        Self {
            phase,
            ..Default::default()
        }
    }
}

impl SwitchRecord {
    /// 开关是否在授权激活可用窗口（enabled 且未在失败/关闭/等待卸载）。
    pub fn authorization_active(&self) -> bool {
        self.enabled
            && !matches!(
                self.phase,
                SwitchPhase::Failed | SwitchPhase::WaitingUninstall | SwitchPhase::Off
            )
    }

    /// 开关是否正持有生命周期（生命周期命令直调的拒绝窗口）。
    pub fn lifecycle_active(&self) -> bool {
        self.enabled && !matches!(self.phase, SwitchPhase::Failed | SwitchPhase::Off)
    }

    /// 关闭流程未收敛（第九轮 P1-1 **共享判据**，enable/repair/gate_lifecycle 三处
    /// 消费同一谓词，杜绝各自复制的判定漂移）：等待卸载，或关闭侧失败（enabled =
    /// false 且失败步骤为撤权/卸载/撤权前 home 证据回填）。这些状态下旧绑定/旧钥
    /// 尚未安全退役——任何「推进开关」（enable/repair）或「直调生命周期命令」都
    /// 必须先完成关闭（重试关闭收敛），不得绕行。撤权前回填失败
    /// （Failed(HomeEvidence)，终审 Important-1）尤其如此：disable 在调撤权段
    /// **之前**就返回了，旧 Bound 行、受信连接、已交付凭据全部活体在场——旁路
    /// 等于让旧绑定直接复活。开启侧失败（enabled = true + Failed）不属关闭流程，
    /// 不在本判据内。
    pub fn close_pending(&self) -> bool {
        self.phase == SwitchPhase::WaitingUninstall
            || (self.phase == SwitchPhase::Failed
                && !self.enabled
                && matches!(
                    self.failed_step,
                    Some(SwitchStep::Revoke)
                        | Some(SwitchStep::Uninstall)
                        | Some(SwitchStep::HomeEvidence)
                ))
    }
}

// ============================================================
// 公开 IPC 门（裁决 R2：不允许绕过开关单独激活远控）
// ============================================================

/// 授权激活命令的开关门：开关未开启（**无记录**/Off/Failed/WaitingUninstall，
/// 含记录读取失败）→ `DSH_SWITCH_OFF`。「选定但从未物化 Off 记录」的实例同样
/// 拒绝——记录缺席就是未开启，不得成为绕过窗（复审 Important-1）。调用方必须
/// 把 Option 原样传入，不得在壳层自行放行 None。
pub fn gate_authorization(record: Option<&SwitchRecord>) -> Result<(), DshError> {
    match record {
        Some(record) if record.authorization_active() => Ok(()),
        _ => Err(DshError::new(
            codes::SWITCH_OFF,
            "remote-control switch is off; enable it before authorizing dsh remote access",
        )),
    }
}

/// 生命周期命令的开关门：开关持有生命周期期间拒绝直调 → `DSH_SWITCH_ACTIVE`
/// （用开关关闭/重试；修复走 `dsh_switch_repair`）。记录缺席（含读取失败）与
/// 真 Off = 开关未持有生命周期 → 放行（关闭方向的清理口保留）。
/// 第九轮 P1-1：关闭未收敛（[`SwitchRecord::close_pending`]——等待卸载 / 关闭侧
/// Failed(Revoke|Uninstall|HomeEvidence)）同样拒绝直调——直调修复/安装曾把
/// Failed(Revoke) 洗成可推进态（resume_phase=Off + failed_step 清空），让未撤权
/// 旧绑定重新获得写权限；唯一出路 = 再次关闭（重试收敛）。判定与 enable/repair
/// 共用同一谓词（单一判据来源），`lifecycle_active` 本体语义不动。
pub fn gate_lifecycle(record: Option<&SwitchRecord>) -> Result<(), DshError> {
    match record {
        Some(record) if record.lifecycle_active() => Err(DshError::new(
            codes::SWITCH_ACTIVE,
            "remote-control switch owns this instance lifecycle; use the switch",
        )),
        Some(record) if record.close_pending() => Err(DshError::new(
            codes::SWITCH_ACTIVE,
            "switch is closing this instance; retry closing (disable) until it converges",
        )),
        _ => Ok(()),
    }
}

// ============================================================
// P1-1 开关执行门（生产执行点的持久化事实源）
//
// 关闭语义的**执行面**：`enabled=false` 落盘那一刻起，四类生产执行点——授权
// 提交（握手写绑定前）、发送最终派发复核、附件写盘、控制台准入——一律拒绝，
// 撤权成败不再决定「开关说关、远控仍活」。生产码统一 `DSH_SWITCH_OFF`
// （已登记 + zh/en 文案齐备，不新增码）。
// ============================================================

/// 测试专用：按实例观测关闭入口，避免其他并行夹具触发。
#[cfg(test)]
static TEST_DISABLE_ENTER: std::sync::Mutex<Option<(String, std::sync::mpsc::Sender<()>)>> =
    std::sync::Mutex::new(None);

/// 执行门统一拒绝错误（fail-closed：关闭/缺席/不可读/服务不可用同一码，定位
/// 信息进 detail）。
pub(crate) fn switch_off_error() -> DshError {
    DshError::new(
        codes::SWITCH_OFF,
        "remote-control switch is off for this instance; remote control is closed",
    )
}

/// 读取实例开关持久化账本（生产缓存服务；缺席/读取失败/服务不可用 → `None`）。
/// **缺席语义由调用方的门定义**：授权激活门按未开启拒绝（fail-closed），生命
/// 周期门按未持有放行——命令壳（`commands::dsh`）与本模块共用同一读取口径。
pub(crate) fn persisted_record(instance_id: &str) -> Option<SwitchRecord> {
    crate::services::dsh_control::production::switch_for_instance(instance_id)
        .ok()?
        .read_raw()
        .ok()
        .flatten()
}

/// P1-1 生产执行门（发送最终派发复核 / 附件写盘 / 控制台准入共用）：目标实例
/// 持久化开关记录必须在「开启」窗口。关闭/缺席（含读取失败、服务不可用）→
/// `DSH_SWITCH_OFF`。记录缺席按现有事实**只读派生**（升级恢复面：已有合法绑定
/// + 插件在场 = 已启用，不得把升级用户误关；派生不落盘、不冒充状态）。
pub fn enforcement_gate(instance_id: &str) -> Result<(), DshError> {
    match crate::services::dsh_control::production::switch_for_instance(instance_id) {
        Ok(service) => service.execution_gate(),
        Err(_) => Err(switch_off_error()),
    }
}

/// P1-1 授权提交点执行门的装配闭包（job-admission gate 同款注入模式）：auth 域
/// 不反向依赖 switch——生产装配点（`production::auth_for_instance`）与开关夹具
/// 用本构造把 [`RemoteSwitchService`] 的判定接线到
/// `DshAuthService::set_switch_gate`。闭包捕获服务 Arc；判定在
/// [`RemoteSwitchService::authorization_gate_check`]（无锁读 + 只读派生）。
pub fn switch_authorization_gate(
    service: Arc<RemoteSwitchService>,
) -> super::auth::SwitchAuthorizationGate {
    Arc::new(move |armed_generation: Option<u64>| {
        service.authorization_gate_check(armed_generation)
    })
}

// ============================================================
// 对外表（前端消费面）
// ============================================================

/// 通道就绪三态（发送/控制台各自独立呈现；与 mobile 侧同一判据词）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelReadiness {
    /// 当代凭据在位（发送可写 / 控制台可开）。
    Ready,
    /// 绑定在场但凭据旧代际/未交付——恢复中（自动重发布/重交付）。
    Recovering,
    /// 无 Bound 绑定/闸门已撤/当代宿主不可证——未授权。
    NotAuthorized,
}

impl From<super::auth::ControlPeerState> for ChannelReadiness {
    fn from(state: super::auth::ControlPeerState) -> Self {
        match state {
            super::auth::ControlPeerState::Ready => Self::Ready,
            super::auth::ControlPeerState::Recovering => Self::Recovering,
            super::auth::ControlPeerState::NotAuthorized => Self::NotAuthorized,
        }
    }
}

/// 驱动器不再需要的终态（Ready / Off / Failed）。`view()` 的补拉判定与启动/
/// 选择后恢复扫描共用同一判定——两处恢复面必须同进同退，不得各写一份。
fn phase_is_terminal(phase: &SwitchPhase) -> bool {
    matches!(
        phase,
        SwitchPhase::Ready | SwitchPhase::Off | SwitchPhase::Failed
    )
}

/// P1-3 启动恢复扫描（实例状态目录枚举）：扫 `<state_dir>/instances/*/remote-switch.json`，
/// 返回持有**非终态**开关记录的实例 id（启动装配段据此逐实例拉起本代驱动器，
/// 不依赖任何 UI 查询）。
///
/// fail-soft：目录枚举失败/记录不可读/畸形 JSON/实例 id 形态非法一律跳过（不
/// panic、不部分猜测）——坏记录不得阻断应用启动；终态记录不返回（无需驱动器）。
/// 与 [`RemoteSwitchService::resume_incomplete`] 配对使用（扫描给名单，恢复给动作）。
pub fn incomplete_startup_records(env: &DshEnvironment) -> Vec<String> {
    // 状态目录与 auth 域同源解析（R10：变量即状态目录）。
    let base = super::production::resolve_state_dir(
        &super::production::DshEnvSnapshot {
            dsh_state_dir: env.dsh_state_dir.clone(),
        },
        &env.tuvis_home,
    );
    let instances = match std::fs::read_dir(base.join("instances")) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(), // 无实例目录 = 无记录可恢复（常态，不是错误）
    };
    let mut pending = Vec::new();
    for entry in instances.flatten() {
        let dir_name = entry.file_name().to_string_lossy().into_owned();
        // 目录名必须通过实例 id 形态校验（与实例域装配同一判据；防任意目录注入）。
        let Some(dir) = super::auth::instance_state_dir(env, &dir_name) else {
            continue;
        };
        let Ok(bytes) = std::fs::read(dir.join(SWITCH_RECORD_FILE)) else {
            continue; // 无记录 / 不可读：无可恢复（fail-soft，坏记录交 view() 兜底）
        };
        let Ok(record) = serde_json::from_slice::<SwitchRecord>(&bytes) else {
            log::warn!("dsh switch startup scan skipped malformed record for instance {dir_name}");
            continue;
        };
        if !phase_is_terminal(&record.phase) {
            pending.push(dir_name);
        }
    }
    pending.sort();
    pending
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SwitchPluginStatus {
    pub installation: InstallationState,
    pub activation: ActivationState,
    pub authorization: AuthorizationState,
    pub version: Option<String>,
    pub reason_code: Option<String>,
}

/// 开关状态外表（`dsh_switch_get/enable/disable/repair` 返回体）。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SwitchView {
    pub instance_id: String,
    /// 开关勾选态（用户意图；后端持久值）。
    pub enabled: bool,
    pub phase: SwitchPhase,
    pub failed_step: Option<SwitchStep>,
    pub reason_code: Option<String>,
    /// 活跃管理作业（安装/卸载/修复；作业阶段——等待退出等由 job 面透出）。
    pub job: Option<ManagementJob>,
    pub plugin_status: Option<SwitchPluginStatus>,
    /// 发送通道就绪结果。
    pub send: Option<ChannelReadiness>,
    /// 控制台通道就绪结果（需要已交付的上游凭据副本）。
    pub console: Option<ChannelReadiness>,
    /// 插件身份变化：等待用户经可信重绑面板确认。
    pub rebind_required: bool,
}

// ============================================================
// 服务
// ============================================================

/// 实例服务来源（生产 = 缓存单例 closure；测试注入预装配 Arc）。
pub type SwitchServices =
    dyn Fn() -> Result<(Arc<DshPluginManager>, Arc<DshAuthService>), DshError> + Send + Sync;

/// 全实例驱动器登记（instance_id → 正在驱动的 generation；防重复驱动器）。
fn drivers() -> &'static Mutex<HashMap<String, u64>> {
    static DRIVERS: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
    DRIVERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 进程内临时名序号（persist 的 tmp 文件名；非语义）。
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// 升级证据只读事实面（第九轮 P2 复审 Important 修复）：三证的**纯盘读**投影。
/// 证据读取绝不装配授权域——不创建密钥（`load_or_create_key`）、不消费待落盘
/// 撤权标记（`consume_revoke_pending`）、不缓存服务；生产装配见
/// [`super::production::upgrade_evidence_source`]，夹具注入同判据的只读投影。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeEvidenceFacts {
    /// 该实例账本最新 Bound 行公钥摘要（`None` = 无 Bound 行/不可证明 = 证一
    /// 不成立）。
    pub bound_digest: Option<String>,
    /// 该 Bound 行是否被「待落盘撤销标记」指名（撤权意图未收敛 → 证据不足，
    /// 不得为读证据消费标记）。
    pub bound_revocation_pending: bool,
    /// 安装档在场（`Installed | Conflict`）。
    pub installed: bool,
}

/// 升级证据只读源（构造注入；生产钩子与夹具共用形态）。与
/// [`SwitchServices`]（驱动器/派生面的 manager+auth 装配闭包）严格分开：
/// 取证据走本源，真实装配走 `assemble_auth`（内核判定通过之后）。
pub type SwitchEvidenceSource = dyn Fn() -> UpgradeEvidenceFacts + Send + Sync;

/// 「远程控制 DSH」开关服务（按实例一份；命令壳与驱动器共享同一 Arc）。
pub struct RemoteSwitchService {
    env: Arc<DshEnvironment>,
    registry: Arc<InstanceRegistry>,
    instance_id: String,
    record_path: PathBuf,
    services: Arc<SwitchServices>,
    /// 升级证据只读源（与 services 装配闭包严格分开——证据读取零装配副作用）。
    evidence: Arc<SwitchEvidenceSource>,
    /// 命令段与驱动器提交段的写序锁（同实例串行；跨实例无竞争）。
    action_lock: Mutex<()>,
    /// 推进拍间隔（生产 2s；测试 20ms）。
    poll: Duration,
}

impl RemoteSwitchService {
    /// 注入构造（生产经 [`super::production::switch_for_instance`]；测试直接构造）。
    /// `evidence` = 升级证据只读源（证据读取不得装配授权域，见
    /// [`UpgradeEvidenceFacts`]）；与 `services`（驱动器装配闭包）严格分开。
    pub fn new(
        env: Arc<DshEnvironment>,
        registry: Arc<InstanceRegistry>,
        instance_id: impl Into<String>,
        services: Arc<SwitchServices>,
        evidence: Arc<SwitchEvidenceSource>,
    ) -> Result<Self, DshError> {
        let instance_id = instance_id.into();
        let dir = super::auth::instance_state_dir(&env, &instance_id).ok_or_else(|| {
            DshError::new(
                codes::IPC_STATE,
                format!("invalid dsh instance id: {instance_id}"),
            )
        })?;
        let service = Self {
            env,
            registry,
            instance_id,
            record_path: dir.join(SWITCH_RECORD_FILE),
            services,
            evidence,
            action_lock: Mutex::new(()),
            poll: Duration::from_millis(2_000),
        };
        if let Some(parent) = service.record_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| DshError::new(codes::STATE_IO, format!("create switch dir: {e}")))?;
        }
        Ok(service)
    }

    /// 测试接缝：推进拍间隔。
    pub fn with_poll(mut self, poll: Duration) -> Self {
        self.poll = poll;
        self
    }

    // --------------------------------------------------------
    // 账本读写
    // --------------------------------------------------------

    /// 原子读（畸形账本 = `DSH_STATE_IO`；不把损坏当「从未开启」）。
    pub fn read_raw(&self) -> Result<Option<SwitchRecord>, DshError> {
        match std::fs::read(&self.record_path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| DshError::new(codes::STATE_IO, format!("parse switch record: {e}"))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(DshError::new(
                codes::STATE_IO,
                format!("read switch record: {error}"),
            )),
        }
    }

    /// 原子写（temp + rename；非敏感状态，management.json 同口径）。
    fn persist(&self, record: &SwitchRecord) -> Result<(), DshError> {
        if let Some(parent) = self.record_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| DshError::new(codes::STATE_IO, format!("create switch dir: {e}")))?;
        }
        let bytes = serde_json::to_vec_pretty(record)
            .map_err(|e| DshError::new(codes::STATE_IO, format!("serialize switch record: {e}")))?;
        let tmp = self.record_path.with_extension(format!(
            "tmp-{}-{}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let result =
            std::fs::write(&tmp, &bytes).and_then(|()| std::fs::rename(&tmp, &self.record_path));
        if let Err(error) = result {
            let _ = std::fs::remove_file(&tmp);
            return Err(DshError::new(
                codes::STATE_IO,
                format!("write switch record: {error}"),
            ));
        }
        Ok(())
    }

    /// CAS 提交：读 → 核对 generation → 改 → 原子写。代次已更替 → `None`
    /// （调用方退场——迟到的驱动器/意图不得改写新一代意图的账本）。
    fn commit(
        &self,
        expected_generation: u64,
        mutate: impl FnOnce(&mut SwitchRecord),
    ) -> Result<Option<SwitchRecord>, DshError> {
        let _guard = self.action_lock.lock().unwrap();
        let Some(mut record) = self.read_raw()? else {
            return Ok(None);
        };
        if record.generation != expected_generation {
            return Ok(None);
        }
        mutate(&mut record);
        record.updated_at_ms = self.env.clock.now_ms();
        self.persist(&record)?;
        Ok(Some(record))
    }

    /// 读账本（无记录时按现有事实派生并落盘——升级恢复：已有合法绑定 + 插件
    /// 在场 → 直接「已启用」；其余 → Off）。
    pub fn read(&self) -> Result<SwitchRecord, DshError> {
        let _guard = self.action_lock.lock().unwrap();
        self.read_locked()
    }

    /// read 的无锁内核（调用方必须已持 action_lock）。
    fn read_locked(&self) -> Result<SwitchRecord, DshError> {
        if let Some(record) = self.read_raw()? {
            return Ok(record);
        }
        let derived = self.derive_record();
        self.persist(&derived)?;
        Ok(derived)
    }

    /// 按现有事实派生（升级恢复路径；服务装配失败按 Off 处理——不冒充状态）。
    fn derive_record(&self) -> SwitchRecord {
        let mut record = SwitchRecord::default_with(SwitchPhase::Off);
        if let Ok((manager, auth)) = (self.services)() {
            let bound = auth.authorization_state() == AuthorizationState::Bound;
            if let (true, Ok(status)) = (bound, manager.status()) {
                if matches!(
                    status.installation,
                    InstallationState::Installed | InstallationState::Conflict
                ) {
                    record.enabled = true;
                    record.phase = SwitchPhase::Ready;
                    record.scope = SWITCH_SCOPE.to_owned();
                }
            }
        }
        record
    }

    /// 升级恢复**三证核验**（第九轮 P2；记录缺席时判定「该实例是否为开关功能
    /// 上线前已绑定+安装的合法升级用户」）。与 [`Self::derive_record`] 同源判据
    ///（Bound 行 + 安装在场），外加**登记锚一致性**一证：
    ///
    /// 1. **Bound 行存在**——账本最新 Bound 行可证明（从未授权的实例绝不补起）；
    /// 2. **安装在场**——`Installed | Conflict`（插件不在场的实例无监听意义）；
    /// 3. **登记锚在场且与账本一致**——登记指纹非空且等于账本最新 Bound 行公钥
    ///    摘要（防锚漂移误恢复：登记指向别的钥时，免批重连也会被指纹门拒收，
    ///    补起监听只是白装配）。
    ///
    /// **只读取证（第九轮 P2 复审 Important 修复）**：三证全部经注入的只读源
    /// （[`UpgradeEvidenceFacts`]，纯盘读）与登记面（内存登记读）取得——**绝不
    /// 经 `services` 装配授权域**（冷缓存构造会 `load_or_create_key` 落钥并
    /// `consume_revoke_pending` 消费待落盘撤权标记）；待落盘撤权在册按证据不足
    /// 处理（撤权意图在先）。任何一证不齐 → `false`，真实装配只发生在证据通过
    /// 之后（内核的 `assemble_auth` → 生产 `auth_for_instance`）。不落盘、不
    /// 派生记录、不冒充状态。
    pub(crate) fn upgrade_recovery_evidence(&self) -> bool {
        // 证三先行（登记面读，最廉价）：锚缺失直接不足，不触任何盘读/装配面。
        let Ok(registered) = self.registry.registered(&self.instance_id) else {
            return false;
        };
        if registered.identity_fingerprint.is_empty() {
            return false;
        }
        // 只读事实面（纯盘读；绝不装配授权域）。
        let facts = (self.evidence)();
        // 证一 + 证三：Bound 行存在且摘要与登记锚一致。
        if facts.bound_digest.as_deref() != Some(registered.identity_fingerprint.as_str()) {
            return false;
        }
        // 待落盘撤权未收敛 = 撤权意图在先 → 证据不足（不得为读证据消费标记）。
        if facts.bound_revocation_pending {
            return false;
        }
        // 证二：安装在场。
        facts.installed
    }

    // --------------------------------------------------------
    // 对外表
    // --------------------------------------------------------

    /// 当前状态外表（含实时就绪结果与活跃作业）。
    ///
    /// 兼作**跨重启恢复点**：账本处于非终态而本进程没有本代驱动器（兔维斯
    /// 重启 / 设置页重开后的首次查询）→ 重新拉起本代驱动器（同代幂等）。
    /// 线性推进因此跨设置页与兔维斯重启可恢复，不需要用户重新操作开关。
    pub fn view(self: &Arc<Self>) -> Result<SwitchView, DshError> {
        let record = self.read()?;
        if !phase_is_terminal(&record.phase) {
            self.spawn_driver(record.generation);
        }
        Ok(self.view_of(record))
    }

    /// P1-3 启动/选择后恢复面（单实例）：读盘上**原始**记录（不派生、不落盘），
    /// 非终态 → 拉起本代驱动器（同代幂等）。与 [`Self::view`] 的差别：不派生
    /// 缺席记录、不返回外表、**fail-soft**——账本缺席 = 无需恢复；不可读 = 记
    /// 日志返回 false（坏记录不得阻断应用启动，[`Self::view`] 仍是次级恢复点）。
    /// 返回 true = 本代驱动器已被拉起（或在跑）。
    pub fn resume_incomplete(self: &Arc<Self>) -> bool {
        match self.read_raw() {
            Ok(Some(record)) => {
                if phase_is_terminal(&record.phase) {
                    return false;
                }
                self.spawn_driver(record.generation);
                true
            }
            Ok(None) => false,
            Err(e) => {
                log::warn!(
                    "dsh switch resume skipped for instance {} (record unreadable): {} ({})",
                    self.instance_id,
                    e.code,
                    e.detail
                );
                false
            }
        }
    }

    fn view_of(&self, record: SwitchRecord) -> SwitchView {
        // 作业外表只报**真实作业**（F09-A）：按记录里的 jobId 查管理器作业表
        // （内存活体 + 账本终态回执），阶段原样透出（含 Succeeded/Failed/
        // Cancelled）。查不到（作业 ID 未知/账本已轮换）→ 不返回作业，绝不凭
        // 旧 jobId 伪造 `Queued`——伪造会让前端判定「仍在忙」，把本可用的
        // 「重试关闭」「修复插件」永久锁死（用户现场）。
        let recorded_job = |manager: &DshPluginManager| -> Option<ManagementJob> {
            record
                .job_id
                .as_deref()
                .and_then(|job_id| match manager.job(job_id) {
                    Ok(job) => Some(job),
                    Err(e) if e.code == codes::JOB_UNKNOWN => None,
                    Err(e) => {
                        log::warn!(
                            "dsh switch job lookup failed for instance {} ({}): {}",
                            self.instance_id,
                            job_id,
                            e.detail
                        );
                        None
                    }
                })
        };
        let (job, plugin_status, send, console) = match (self.services)() {
            Ok((manager, auth)) => {
                let status = manager.status().ok();
                // 活跃作业优先（status 只报未达终态的作业）；没有活跃作业时
                // 回落查询记录里登记过的那个作业，把终态如实透出。
                let job = status
                    .as_ref()
                    .and_then(|s| s.operation.clone())
                    .or_else(|| recorded_job(&manager));
                let send = Some(ChannelReadiness::from(auth.control_peer_state()));
                let console = match send {
                    Some(ChannelReadiness::Ready) => {
                        let delivered = console_credentials_delivered(&self.instance_id);
                        Some(if delivered {
                            ChannelReadiness::Ready
                        } else {
                            ChannelReadiness::Recovering
                        })
                    }
                    other => other,
                };
                let plugin_status = status.map(|status| SwitchPluginStatus {
                    installation: status.installation,
                    activation: status.activation,
                    authorization: auth.authorization_state(),
                    version: status.plugin_version,
                    reason_code: status.reason_code,
                });
                (job, plugin_status, send, console)
            }
            // 服务装配失败（授权域/管理器 None）：没有任何可证明的作业事实
            // → 不返回作业，不伪造进度。
            Err(_) => (None, None, None, None),
        };
        SwitchView {
            instance_id: self.instance_id.clone(),
            enabled: record.enabled,
            phase: record.phase,
            failed_step: record.failed_step,
            reason_code: record.reason_code,
            job,
            plugin_status,
            send,
            console,
            rebind_required: record.phase == SwitchPhase::RebindRequired,
        }
    }

    // --------------------------------------------------------
    // 意图入口（命令壳调用）
    // --------------------------------------------------------

    /// 开启（线性推进起点）。幂等：已启用/推进中原样返回；失败后重开 = 从
    /// 检查步重新推进（事实就绪的步骤自动跳过）。
    ///
    /// 销项复审 P2-3：**不再在入口清除「正在关闭」旗标**——并发 enable 不得
    /// 在 disable 进行中提前拆掉拒绝面。旗标只在开关记录成功提交 enabled
    /// （下方 persist）之后清除：关闭生命周期内的任何 enable 竞争者要么被
    /// close_pending 拒（旗标保留），要么等待 action_lock 到关闭收敛后走
    /// 正常推进、在提交点清旗标。
    pub fn enable(self: &Arc<Self>) -> Result<SwitchView, DshError> {
        // 销项复审 P1-2：入口先观察当前关闭纪元——成功提交 enabled 后只按
        // 观察值**条件**清除（`clear_closing_if`）。「观察之后、提交之前」到达
        // 的新关闭（更高纪元）不被本次 enable 拆除，关闭拒绝面保持。
        let observed_closing =
            crate::services::dsh_control::production::closing_epoch(&self.instance_id);
        // 回归接缝（test-only，unix 测试面）：enable 入口确定性插队窗（P1-2
        // 回归据此在「已观察纪元 → 未提交」之间塞进第二个关闭意图）。
        #[cfg(all(test, unix))]
        arm_test_hooks::before_enable(&self.instance_id);
        let _guard = self.action_lock.lock().unwrap();
        let mut record = self.read_locked()?;
        match record.phase {
            SwitchPhase::Ready if record.enabled => return Ok(self.view_of(record)),
            // 推进中的再次开启 = 原样返回（不重置推进、不换代次——驱动器继续）。
            _ if record.enabled
                && !matches!(record.phase, SwitchPhase::Failed | SwitchPhase::Off) =>
            {
                return Ok(self.view_of(record));
            }
            SwitchPhase::Repairing => {
                return Err(DshError::new(
                    codes::SWITCH_ACTIVE,
                    "switch is closing or repairing; wait for it to finish",
                ));
            }
            // 第九轮 P1-1（共享判据 close_pending）：关闭未收敛（等待卸载 / 关闭侧
            // Failed）→ 开启拒绝。Round-8 只拦 Failed(Revoke)，Failed(Uninstall) 曾
            // 直接放行推进（连撤权兜底面都没有）。放行等于让未退役的旧钥/旧绑定
            // 重新获得写权限；唯一出路 = 再次关闭（重试收敛），收敛进等待卸载、
            // 卸载完成后按既有流程重新开启。等待卸载保持既有 SWITCH_ACTIVE 文案；
            // 关闭侧 Failed 统一走既有码 DSH_SWITCH_REVOKE_PENDING（detail 区分
            // Revoke/Uninstall/HomeEvidence——终审 Important-1 纳入判据的第三态，
            // disable 在撤权段之前返回，旧绑定活体在场）。
            _ if record.close_pending() => {
                return Err(if record.phase == SwitchPhase::WaitingUninstall {
                    DshError::new(
                        codes::SWITCH_ACTIVE,
                        "switch is closing or repairing; wait for it to finish",
                    )
                } else {
                    DshError::new(
                        codes::SWITCH_REVOKE_PENDING,
                        match record.failed_step {
                            Some(SwitchStep::Uninstall) => {
                                "closing did not finish uninstalling; disable again to retry closing"
                            }
                            Some(SwitchStep::HomeEvidence) => {
                                "closing could not preserve the uninstall home evidence; disable again to retry closing"
                            }
                            _ => {
                                "revocation did not complete; disable again to retry closing"
                            }
                        },
                    )
                });
            }
            _ => {}
        }
        record.enabled = true;
        record.phase = SwitchPhase::Checking;
        record.failed_step = None;
        record.reason_code = None;
        record.job_id = None;
        record.job_kind = None;
        record.resume_phase = None;
        record.scope = SWITCH_SCOPE.to_owned();
        record.generation += 1;
        record.updated_at_ms = self.env.clock.now_ms();
        if observed_closing.is_some() {
            self.persist(&record)?;
        } else {
            // 冷启动可能只有持久关闭记录；提交 enabled 与推进代次同一临界区。
            crate::services::dsh_control::production::commit_open_generation(
                &self.instance_id,
                || self.persist(&record),
            )?;
        }
        // 销项复审 P1：开关记录成功提交 enabled（上方 persist）后才清除
        // 「正在关闭」旗标——提交点清除，不在入口（P2-3：并发 enable 不得在
        // disable 进行中提前拆掉拒绝面）。销项复审 P1-2：清除按入口观察到的
        // 纪元**条件**生效——期间到达的新关闭（更高纪元）保留拒绝面，等它的
        // disable 拿到锁完成关闭。
        if let Some(observed) = observed_closing {
            // 销项复审 P1-3（附件上传原子代次边界）：清旗成功 = 本次 enable 完成
            // 了「关闭+重开」完整周期的后半程——实例代次再推进 +1（关闭侧
            // mark_closing 已推进过一次）。关闭前受理的旧附件上传在发布点的
            // 代次比对必然失配，不按新开关状态写盘。
            // 销项审查发现 2（P2）：清旗与推进**同锁复合**（`clear_closing_and_
            // advance`）——旧实现两步分开锁，窗口里上传可经入口准入并捕获
            // 「已清旗、未推进」的代次，enable 推进后在附件发布点被误拒
            // stale_upload_generation；复合化后该窗口不复存在。
            crate::services::dsh_control::production::clear_closing_and_advance(
                &self.instance_id,
                observed,
            );
        }
        let generation = record.generation;
        self.spawn_driver(generation);
        Ok(self.view_of(record))
    }

    /// 关闭。**enabled=false 先行落盘**（P1-1 执行面：四类生产执行门——授权提交/
    /// 发送最终复核/附件写盘/控制台准入——从这一刻起立即拒绝，不等撤权结果），
    /// 随后同步失效段（取消待授权意图 + 撤销活跃绑定）。撤权写盘失败 →
    /// Failed(Revoke) + 重试入口（enabled 不回滚——执行门保持关闭）；重试（再次
    /// disable）重跑撤权，成功后进「远控已关闭 · 等待卸载」并由驱动器推进卸载作业。
    pub fn disable(self: &Arc<Self>) -> Result<SwitchView, DshError> {
        // 销项复审 P1/P2-2：**关闭首步 = 立即拒绝面**，移到 action_lock 之前
        // （先于任何盘读/等待）——①标记「正在关闭」（许可区内存判据/授权门/
        // 控制台入口/附件发布点即刻拒绝新请求）；②吊销该实例旧控制台票据与
        // 活连接。action_lock 被长段占用（驱动器推进/并发开关）时，拒绝面与
        // 撤销面照常立即生效——关闭入口不再等锁。
        crate::services::dsh_control::production::mark_closing(&self.instance_id);
        // P1-1：关闭首步即**同步封存**实时问答——清空待答表并停掉常驻订阅。
        // 紧跟 mark_closing：此后任何在途回执都过不了代次校验，不会写进关闭中的实例。
        crate::services::dsh_control::seal_live_questions(&self.instance_id);
        crate::remote::console_lifecycle::revoke_console_access_for_instance(&self.instance_id);
        #[cfg(test)]
        {
            let hook = TEST_DISABLE_ENTER.lock().unwrap();
            if let Some((instance, entered)) = hook.as_ref() {
                if instance == &self.instance_id {
                    let _ = entered.send(());
                }
            }
        }
        let _guard = self.action_lock.lock().unwrap();
        let mut record = self.read_locked()?;
        if !record.enabled && record.phase == SwitchPhase::Off {
            // 关闭已完成的历史重入：旗标保留（关闭即效面持续拒绝；入口已
            // 标记，此处幂等重申），直接回视图。
            crate::services::dsh_control::production::mark_closing(&self.instance_id);
            return Ok(self.view_of(record));
        }
        // ① fail-closed face：enabled=false **先行落盘**（相位暂记等待卸载；撤权
        // 结果随后收口）。此写之后任何执行门查询都看到关闭——撤权成败不再决定
        // 「开关说关、远控仍活」。本函数全程持 action_lock（与驱动器 CAS 提交互斥），
        // 期间旧代驱动器因代次更替自行退场，不会被中间态误推进。
        //
        // 已知窗口（复审 Minor-2 裁决：本批接受，不改序）：persist#1 到本函数
        // 末尾 spawn_driver 之间，并发的 `view()` 会读到「WaitingUninstall + 本代
        // 代次 + 无作业」并拉起同代驱动器——它与本函数的撤权段并发。良性：卸载
        // 作业的 revoke-all 与本函数撤权在 binding_mutation 上串行、同向收敛
        // （后到的一方按「绑定已不活跃」失败；本函数侧表现为一次可重试的
        // Failed(Revoke)，卸载侧作业照常推进）；换序（撤权后再落盘）会丢掉
        // fail-closed face，崩溃恢复面（中间态留 WaitingUninstall → 重启后
        // view() 续推卸载）优先于该窗口。
        // 重试去重（F09-A/D）：若记录里的作业正是**本次关闭的卸载作业**，保留这个
        // 锚——重复点「重试关闭」只能让同一个作业继续/收敛，不得丢掉锚后在下一拍
        // 另起一个卸载作业（实测：旧实现清锚 + 换代 → 首作业完成后驱动器按「无
        // 记录」再 start 一次，官方 remove 被调用两次）。
        //
        // 覆盖两个窗口：
        // * 作业仍活跃（等待退出/写入/核验）→ 保留锚，驱动器继续跟同一个作业；
        // * 作业**已 Succeeded 但驱动器还没来得及提交 Off** → 同样保留锚：驱动器
        //   下一拍会读到终态并直接进 Off，若此刻清锚就会另起第二个作业。
        //
        // 其它情形（安装/修复作业、Failed/Cancelled 终态、无作业、作业已消失）照
        // 旧清锚：关闭方向会经 tick_uninstall 先取消并让位，再由新卸载作业承接。
        //
        // 只在「已经处于等待卸载且登记的是卸载作业」时才查作业表：这条路径紧接着
        // 本函数就要装配同一对 services 去撤权，故不新增装配类别；首次关闭
        // （Ready/Installing 等）不会因为本判据提前触碰服务。
        let retrying_uninstall = record.phase == SwitchPhase::WaitingUninstall
            && record.job_kind == Some(ManagementKind::Uninstall)
            && record.job_id.is_some();
        let live_uninstall_job = record
            .job_id
            .clone()
            .filter(|job_id| {
                !retrying_uninstall
                    || match (self.services)() {
                        Ok((manager, _)) => manager.job(job_id).is_ok_and(|job| {
                            job.kind == ManagementKind::Uninstall
                                && !matches!(
                                    job.phase,
                                    ManagementPhase::Failed | ManagementPhase::Cancelled
                                )
                        }),
                        Err(_) => false,
                    }
            })
            .filter(|_| retrying_uninstall);
        record.enabled = false;
        record.phase = SwitchPhase::WaitingUninstall;
        record.failed_step = None;
        record.reason_code = None;
        record.resume_phase = None;
        if live_uninstall_job.is_none() {
            record.job_id = None;
            record.job_kind = None;
        }
        record.generation += 1;
        record.updated_at_ms = self.env.clock.now_ms();
        // 销项复审 P1：状态翻转（persist#1 enabled=false 落盘）持提交边界独占
        // ——先阻断新许可、排空存量在途写，Guard 持到记录落盘结束；翻转先落地
        // 时，发送的提交点门必然读到关闭并拒绝（0 次写出）。只罩翻转段，不罩
        // 撤权/等宿主退出的长尾——persist#1 之后执行门已经拒绝一切新发送。
        let _commit_flip = crate::services::dsh_control::production::commit_flip_begin();
        self.persist(&record)?;
        drop(_commit_flip);
        let generation = record.generation;

        // ①.5 撤权前保全卸载目标（第九轮 P1-2）：缺 `dsh_home` 的旧登记（升级
        // 携带形态）必须在撤权把 Bound 行翻成墓碑**之前**回填登记 home——否则
        // 卸载受理门的 home 证据（登记值或 Bound 行）在撤权后永远 Unproven，
        // 关闭+卸载死路（复审根因 2）。对位失败宁可不撤（保持关闭侧、重试可
        // 再入）也不写错目标：失败落 Failed(HomeEvidence) + 具体原因码，重试
        // （再次关闭）重跑回填；成功/已回填的实例照常进入撤权段。
        if let Err(error) = super::instances::backfill_missing_registered_home(
            &self.env,
            &self.registry,
            &self.instance_id,
        ) {
            record.phase = SwitchPhase::Failed;
            record.failed_step = Some(SwitchStep::HomeEvidence);
            record.reason_code = Some(error.code.to_string());
            record.updated_at_ms = self.env.clock.now_ms();
            self.persist(&record)?;
            return Ok(self.view_of(record));
        }

        // ② 同步失效段（不等待宿主退出；§2.5 撤权先于一切移除）。**全程持
        // `binding_mutation`（Round-8 Ruling 10）**：disable 持 action_lock 进入
        // `disable_invalidation`，其内部再取绑定互斥（锁序 action_lock →
        // binding_mutation，无反序路径——pin_new_binding 不取 action_lock，开关门
        // 是无锁读）——与绑定提交严格串行：要么提交先落地（本段看到并撤走新鲜
        // 绑定），要么本段先完成（提交的互斥内门看到 enabled=false 而拒绝）。
        //
        // P1-4（2026-11-14 用户裁决，推翻第 6 轮 Ruling 4 的「全局吊销、接受为
        // 已知限制」）：撤权内部的控制台吊销已**按实例收敛**（
        // `console_lifecycle::revoke_console_access_for_instance`）——关闭实例 A
        // 只吊销 A 的网页凭据与活控制台，其他实例不受影响；A 自身的恢复由重绑定
        // 交付链（重装/重授权）与活体跟踪拍对账（受信 peer 在场 + 凭据缺失 →
        // 重交付）承担。整域关闭（revoke_console_access）保留全局语义。回归见
        // `switch_tests::disable_invalidates_immediately_then_uninstalls` 与
        // `tests/dsh_console_per_instance_revoke_test.rs`。
        let revoke_failure = match (self.services)() {
            Ok((_, auth)) => match auth.disable_invalidation() {
                Ok(conn) => {
                    if let Some(conn) = conn {
                        conn.close();
                    }
                    None
                }
                Err(e) => Some(e),
            },
            Err(e) => Some(e),
        };
        // ③ 撤权失败收口：Failed(Revoke) + 重试入口（enabled 保持 false——执行门
        // 不因撤权失败回滚）；重试路径见 ①（再次 disable 重跑撤权）。
        if let Some(e) = revoke_failure {
            record.phase = SwitchPhase::Failed;
            record.failed_step = Some(SwitchStep::Revoke);
            record.reason_code = Some(e.code.to_string());
            record.updated_at_ms = self.env.clock.now_ms();
            self.persist(&record)?;
        }
        self.spawn_driver(generation);
        Ok(self.view_of(record))
    }

    /// 修复插件（只作用于当前实例；完成后自动接续原流程：启用中 → 重新推进，
    /// 关闭 → 保持关闭）。幂等：修复进行中原样返回。
    pub fn repair(self: &Arc<Self>) -> Result<SwitchView, DshError> {
        let _guard = self.action_lock.lock().unwrap();
        let mut record = self.read_locked()?;
        if record.phase == SwitchPhase::Repairing && record.job_id.is_some() {
            return Ok(self.view_of(record));
        }
        // 第九轮 P1-1（共享判据 close_pending，必须在启动修复作业**之前**判定）：
        // 关闭未收敛（等待卸载 / 关闭侧 Failed(Revoke|Uninstall)）→ 修复拒绝。
        // 修复完成曾按 resume_phase=Off 清掉 failed_step，把 Failed(Revoke) 洗成
        // 可推进态（enable 的撤权拒绝不再命中，旧绑定免撤权复活）；Failed(Uninstall)
        // 也一样会被洗掉。唯一出路 = 再次关闭收敛。
        if record.close_pending() {
            return Err(DshError::new(
                codes::SWITCH_ACTIVE,
                "switch is closing this instance; retry closing (disable) until it converges",
            ));
        }
        let (manager, _) = (self.services)()?;
        // 活跃作业期间拒绝修复（避免与安装/卸载作业交叠；等作业终态后再修）。
        if let Some(operation) = manager.status().ok().and_then(|s| s.operation) {
            return Err(DshError::new(
                codes::SWITCH_ACTIVE,
                format!("management job {} is active", operation.job_id),
            ));
        }
        let job_id = manager.start(ManagementKind::Repair)?;
        record.resume_phase = Some(if record.enabled {
            SwitchPhase::Checking
        } else {
            SwitchPhase::Off
        });
        record.phase = SwitchPhase::Repairing;
        record.job_id = Some(job_id);
        record.job_kind = Some(ManagementKind::Repair);
        record.failed_step = None;
        record.reason_code = None;
        record.generation += 1;
        record.updated_at_ms = self.env.clock.now_ms();
        self.persist(&record)?;
        let generation = record.generation;
        self.spawn_driver(generation);
        Ok(self.view_of(record))
    }

    // --------------------------------------------------------
    // 驱动器
    // --------------------------------------------------------

    /// 启动（或确认已在跑）本代驱动器。
    fn spawn_driver(self: &Arc<Self>, generation: u64) {
        {
            let mut drivers = drivers().lock().unwrap();
            if drivers.get(&self.instance_id) == Some(&generation) {
                return;
            }
            drivers.insert(self.instance_id.clone(), generation);
        }
        let service = Arc::clone(self);
        tokio::spawn(async move {
            service.drive(generation).await;
        });
    }

    /// 驱动器主循环：拍间隔推进，直到终态/代次更替/账本不可读。
    async fn drive(self: Arc<Self>, generation: u64) {
        // last_arm 是驱动器局部状态（意图续签节拍），不进账本。
        let mut last_arm: Option<std::time::Instant> = None;
        loop {
            tokio::time::sleep(self.poll).await;
            match self.tick(generation, &mut last_arm).await {
                Ok(true) => continue,
                Ok(false) | Err(()) => break,
            }
        }
        let mut drivers = drivers().lock().unwrap();
        if drivers.get(&self.instance_id) == Some(&generation) {
            drivers.remove(&self.instance_id);
        }
        drop(drivers);
        #[cfg(all(test, unix))]
        arm_test_hooks::note_driver_exit(&self.instance_id);
    }

    /// 一拍推进：读账本 → 核对代次 → 按相位评估 → CAS 提交。
    /// 返回 `true` = 继续推进；`false` = 退场（终态/代次更替/账本不可读）。
    async fn tick(
        &self,
        generation: u64,
        last_arm: &mut Option<std::time::Instant>,
    ) -> Result<bool, ()> {
        let record = match self.read_raw() {
            Ok(Some(record)) => record,
            Ok(None) | Err(_) => return Ok(false),
        };
        if record.generation != generation {
            return Ok(false);
        }
        match record.phase {
            SwitchPhase::Ready | SwitchPhase::Off | SwitchPhase::Failed => Ok(false),
            SwitchPhase::Repairing => self.tick_repair(generation, &record).await,
            SwitchPhase::WaitingUninstall => self.tick_uninstall(generation, &record).await,
            _ if !record.enabled => Ok(false),
            SwitchPhase::Checking => self.tick_checking(generation, &record).await,
            SwitchPhase::Installing => self.tick_installing(generation, &record).await,
            SwitchPhase::AwaitingReopen | SwitchPhase::VerifyingPlugin => {
                self.tick_activation(generation, &record).await
            }
            SwitchPhase::Authorizing => self.tick_authorizing(generation, &record, last_arm).await,
            SwitchPhase::RebindRequired => self.tick_rebind(generation, &record).await,
        }
    }

    /// CAS 提交封装（代次不符 → `Err(())` 驱动器退场）。
    fn advance(&self, generation: u64, mutate: impl FnOnce(&mut SwitchRecord)) -> Result<bool, ()> {
        match self.commit(generation, mutate) {
            Ok(Some(_)) => Ok(true),
            Ok(None) => Err(()),
            Err(e) => {
                log::warn!("dsh switch record commit failed: {}", e.detail);
                Err(())
            }
        }
    }

    /// 开启方向暂停门：本实例不再是选定实例 → 暂停（不推进不失败——意图保留，
    /// 切回后继续；绝不把旧意图用于另一实例）。
    fn paused(&self) -> bool {
        matches!(
            self.registry.selected(),
            Ok(Some(selected)) if selected.instance_id != self.instance_id
        )
    }

    fn fail_step(&self, generation: u64, step: SwitchStep, error: &DshError) -> Result<bool, ()> {
        self.fail_step_with_reason(generation, step, error.code, &error.detail)
    }

    /// 卸载失败保留已登记的作业原因，旧账本中的未知值使用通用错误码。
    fn fail_step_preserving_reason(
        &self,
        generation: u64,
        step: SwitchStep,
        reason: Option<&str>,
        fallback: &'static str,
        detail: &str,
    ) -> Result<bool, ()> {
        let code = reason
            .map(str::trim)
            .filter(|reason| {
                !reason.is_empty() && crate::services::dsh_control::error::is_known_code(reason)
            })
            .unwrap_or(fallback);
        self.fail_step_with_reason(generation, step, code, detail)
    }

    fn fail_step_with_reason(
        &self,
        generation: u64,
        step: SwitchStep,
        code: &str,
        detail: &str,
    ) -> Result<bool, ()> {
        if !detail.is_empty() {
            log::warn!("dsh switch failed step {step:?}: {code}: {detail}");
        }
        self.advance(generation, |record| {
            record.phase = SwitchPhase::Failed;
            record.failed_step = Some(step);
            record.reason_code = Some(code.to_string());
            // jobId 刻意保留：它是审计锚，也是 `view_of` 查回**真实终态作业**的
            // 唯一线索（F09-A）。遗留 jobId 本身不再被当成活跃作业——是否活跃
            // 由查询到的作业相位决定，重试由 `disable()` 清锚并另起作业承接。
        })
    }

    /// 检查实例：登记在场 + 插件事实盘点（已就绪的步骤在这里自动跳过）。
    /// P1-2 升级兼容：插件在场且宿主运行中但带**死钥**（撤权在证 + 旧身份目录
    /// 在场，[`dead_identity_in_force`]）→ 路由进安装段——由安装路径先退役旧身份
    /// 再装（等待宿主正常退出，绝不强杀），新钥经本次开启的授权意图自动绑定。
    /// 宿主未运行的死钥态由 [`Self::tick_activation`] 在重开过渡点识别（同一谓词）。
    async fn tick_checking(&self, generation: u64, _record: &SwitchRecord) -> Result<bool, ()> {
        if self.paused() {
            return Ok(true);
        }
        let probe = tokio::task::spawn_blocking({
            let snapshot = self.snapshot();
            move || -> Result<SwitchPhase, DshError> {
                snapshot.registry.registered(&snapshot.instance_id)?;
                let (manager, _) = (snapshot.services)()?;
                let status = manager.status()?;
                Ok(match status.installation {
                    InstallationState::Installed => {
                        if status.activation == ActivationState::Running {
                            // P1-2 死钥识别（与 tick_activation 共用同一谓词；
                            // 探测 home 消费装配面）。
                            if dead_identity_in_force(
                                &snapshot.env,
                                &snapshot.registry,
                                &snapshot.instance_id,
                                Some(manager.assembled_home()),
                            )? {
                                SwitchPhase::Installing
                            } else {
                                SwitchPhase::VerifyingPlugin
                            }
                        } else {
                            SwitchPhase::AwaitingReopen
                        }
                    }
                    InstallationState::Missing => SwitchPhase::Installing,
                    InstallationState::Conflict => {
                        return Err(DshError::new(
                            codes::PACKAGE_CONFLICT,
                            "owned plugin files drifted; repair required",
                        ));
                    }
                    InstallationState::Broken => {
                        return Err(DshError::new(
                            codes::VERIFY_FAILED,
                            "installed plugin is unhealthy; repair required",
                        ));
                    }
                })
            }
        })
        .await;
        match probe {
            Ok(Ok(next)) => self.advance(generation, |record| {
                record.phase = next;
            }),
            Ok(Err(e)) => self.fail_step(generation, SwitchStep::InstanceCheck, &e),
            Err(_) => Err(()),
        }
    }

    /// 自动安装：启动/接续安装作业（作业自带「等待宿主正常退出」门）。
    async fn tick_installing(&self, generation: u64, record: &SwitchRecord) -> Result<bool, ()> {
        if self.paused() {
            return Ok(true);
        }
        let probe = tokio::task::spawn_blocking({
            let snapshot = self.snapshot();
            let record = record.clone();
            move || -> Result<InstallTick, DshError> {
                let (manager, _) = (snapshot.services)()?;
                let job = match &record.job_id {
                    Some(job_id) => match manager.job(job_id) {
                        Ok(job) => Some(job),
                        Err(e) if e.code == codes::JOB_UNKNOWN => None,
                        Err(e) => return Err(e),
                    },
                    None => None,
                };
                let Some(job) = job else {
                    let job_id = manager.start(ManagementKind::Install)?;
                    return Ok(InstallTick::Started(job_id));
                };
                Ok(match job.phase {
                    ManagementPhase::Succeeded
                    | ManagementPhase::Failed
                    | ManagementPhase::Cancelled => InstallTick::Terminal(job),
                    _ => InstallTick::Running,
                })
            }
        })
        .await;
        match probe {
            Ok(Ok(InstallTick::Started(job_id))) => self.advance(generation, |record| {
                record.job_id = Some(job_id);
                record.job_kind = Some(ManagementKind::Install);
            }),
            Ok(Ok(InstallTick::Running)) => Ok(true),
            Ok(Ok(InstallTick::Terminal(job))) => {
                if job.phase == ManagementPhase::Succeeded {
                    self.advance(generation, |record| {
                        record.job_id = None;
                        record.job_kind = None;
                        record.phase = SwitchPhase::AwaitingReopen;
                    })
                } else {
                    let reason = job
                        .reason_code
                        .unwrap_or_else(|| codes::CLI_FAILED.to_string());
                    let error = DshError::new(codes::CLI_FAILED, reason);
                    self.fail_step(generation, SwitchStep::Install, &error)
                }
            }
            Ok(Err(e)) => self.fail_step(generation, SwitchStep::Install, &e),
            Err(_) => Err(()),
        }
    }

    /// 重开/核验两相共用：宿主运行 + 插件装载 → 授权；插件缺席 → 回安装。
    /// P1-2 复审补口（Important）：死钥识别不只属于时点检查——「等待重开」与
    /// 「宿主回归」不同拍（时点检查时宿主未运行只会落等待重开），带死钥的插件
    /// 重启后永远连不上，授权步只会死等。重开过渡点以 [`dead_identity_in_force`]
    /// （与 tick_checking 同一谓词）再识别 → 回安装段（先退役旧身份再装）。
    async fn tick_activation(&self, generation: u64, _record: &SwitchRecord) -> Result<bool, ()> {
        if self.paused() {
            return Ok(true);
        }
        let probe = tokio::task::spawn_blocking({
            let snapshot = self.snapshot();
            move || -> Result<ActivationTick, DshError> {
                let (manager, _) = (snapshot.services)()?;
                let status = manager.status()?;
                Ok(match (status.installation, status.activation) {
                    (
                        InstallationState::Installed | InstallationState::Conflict,
                        ActivationState::Running,
                    ) => {
                        // Conflict 不在此路由（时点检查已按「自有文件漂移需修复」
                        // 报失败；修复路径的清理段覆盖其退役）。
                        if status.installation == InstallationState::Installed
                            && dead_identity_in_force(
                                &snapshot.env,
                                &snapshot.registry,
                                &snapshot.instance_id,
                                Some(manager.assembled_home()),
                            )?
                        {
                            ActivationTick::DeadIdentity
                        } else {
                            ActivationTick::Loaded
                        }
                    }
                    (InstallationState::Missing, _) => ActivationTick::Missing,
                    _ => ActivationTick::Waiting,
                })
            }
        })
        .await;
        match probe {
            Ok(Ok(ActivationTick::Loaded)) => self.advance(generation, |record| {
                if record.phase == SwitchPhase::AwaitingReopen {
                    record.phase = SwitchPhase::VerifyingPlugin;
                } else {
                    record.phase = SwitchPhase::Authorizing;
                }
            }),
            Ok(Ok(ActivationTick::DeadIdentity)) => self.advance(generation, |record| {
                record.phase = SwitchPhase::Installing;
            }),
            Ok(Ok(ActivationTick::Missing)) => self.advance(generation, |record| {
                record.phase = SwitchPhase::Installing;
            }),
            Ok(Ok(ActivationTick::Waiting)) => Ok(true),
            Ok(Err(e)) => self.fail_step(generation, SwitchStep::Verify, &e),
            Err(_) => Err(()),
        }
    }

    /// 自动完成授权：Bound → 就绪；身份变化待确认 → 重绑面板；否则布防/续签
    /// 短时握手意图（目标就绪才签发；过期自动续，不需要用户再按一次）。
    async fn tick_authorizing(
        &self,
        generation: u64,
        _record: &SwitchRecord,
        last_arm: &mut Option<std::time::Instant>,
    ) -> Result<bool, ()> {
        if self.paused() {
            return Ok(true);
        }
        let probe = tokio::task::spawn_blocking({
            let snapshot = self.snapshot();
            move || -> Result<AuthorizeProbe, DshError> {
                let (_, auth) = (snapshot.services)()?;
                if auth.authorization_state() == AuthorizationState::Bound {
                    return Ok(AuthorizeProbe::Bound);
                }
                if let Some(pending) = auth.pending_binding() {
                    let new_fingerprint = super::auth::key_digest(&pending.client.public_key_spki);
                    let registered = snapshot.registry.registered(&snapshot.instance_id)?;
                    if !registered.identity_fingerprint.is_empty()
                        && registered.identity_fingerprint != new_fingerprint
                    {
                        return Ok(AuthorizeProbe::RebindRequired);
                    }
                }
                Ok(AuthorizeProbe::NeedArm)
            }
        })
        .await;
        match probe {
            Ok(Ok(AuthorizeProbe::Bound)) => self.advance(generation, |record| {
                record.phase = SwitchPhase::Ready;
            }),
            Ok(Ok(AuthorizeProbe::RebindRequired)) => self.advance(generation, |record| {
                record.phase = SwitchPhase::RebindRequired;
            }),
            Ok(Ok(AuthorizeProbe::NeedArm)) => {
                // 短时意图续签：目标就绪才签发（插件运行中已由上一相位核过）；
                // 45s 续签一次，覆盖「插件重连晚于意图过期」——用户不需要在
                // 超时后再按一次授权。
                let due = last_arm
                    .map(|at| at.elapsed() >= Duration::from_secs(45))
                    .unwrap_or(true);
                if !due {
                    return Ok(true);
                }
                *last_arm = Some(std::time::Instant::now());
                match self.arm_authorization(generation).await {
                    Ok(()) => {
                        // 布防是账本 CAS 之外的**副作用**（跨任务复审 Important）：
                        // 布防进行中「关闭」可能已落账（generation 推进 / enabled
                        // 翻转）。布防返回后必须持提交锁复核账本——已换代或已关闭
                        // → 立即撤销刚布防的意图，否则它会在关闭之后存活整整一个
                        // 挑战窗口（60s），把关闭窗口内的新钥握手自动批准成绑定
                        // （违反「点击关闭后立即失效」与「迟到的握手回执不得把
                        // 已关闭的实例标成启用」）。复核持 `action_lock`：与
                        // disable 的同步段互斥——「布防+复核」相对「关闭落账」的
                        // 先序是确定的（关闭持锁期间复核必然等待，读到的是关闭后
                        // 的账本），窗口从 60s 收敛到锁内一次读改。
                        let _guard = self.action_lock.lock().unwrap();
                        let stale = match self.read_raw() {
                            Ok(Some(record)) => record.generation != generation || !record.enabled,
                            // 账本缺席/不可读：无法证明开启意图仍在 → 保守撤销。
                            _ => true,
                        };
                        if stale {
                            if let Ok((_, auth)) = (self.services)() {
                                auth.cancel_authorization_intent();
                            }
                            #[cfg(all(test, unix))]
                            arm_test_hooks::note_stale_arm_cancel(&self.instance_id);
                        }
                        Ok(true)
                    }
                    Err(e) => self.fail_step(generation, SwitchStep::Authorize, &e),
                }
            }
            Ok(Err(e)) => self.fail_step(generation, SwitchStep::Authorize, &e),
            Err(_) => Err(()),
        }
    }

    /// 布防一次性授权意图（Task 2 机制复用；不另建第二条握手通道）：核验服务
    /// 监听启动 + 选定实例当代宿主在场，随后在授权域布防实例级意图。P1-1：布防
    /// 携带**本代操作代次**——授权提交点据此核对（迟到的旧代握手在提交点退场）。
    async fn arm_authorization(&self, generation: u64) -> Result<(), DshError> {
        // 回归接缝（test-only，unix 测试面）：确定性停在「已读账本 → 未布防」窗口。
        #[cfg(all(test, unix))]
        arm_test_hooks::before_arm(&self.instance_id);
        let (_, auth) = (self.services)()?;
        auth.serve().await?;
        let env = Arc::clone(&self.env);
        let instance_id = self.instance_id.clone();
        let (selected, revision) = self.registry.selection_snapshot()?;
        if selected.as_ref().map(|s| s.instance_id.as_str()) != Some(self.instance_id.as_str()) {
            return Err(DshError::new(
                codes::INSTANCE_SWITCHED,
                "selected instance changed while arming authorization",
            ));
        }
        let candidate = tokio::task::spawn_blocking(move || {
            identity::discover_instance_candidate(&env, &instance_id)
        })
        .await
        .map_err(|_| DshError::new(codes::IPC_STATE, "instance probe task failed"))??;
        if !candidate.verified {
            return Err(DshError::new(
                codes::INSTANCE_UNVERIFIED,
                "selected instance is not verified; authorization intent not armed",
            ));
        }
        auth.begin_selected_authorization_intent_generation(
            Arc::clone(&self.registry),
            &self.instance_id,
            revision,
            candidate.host.epoch,
            Some(generation),
        )
    }

    /// 重绑等待：用户经可信重绑面板确认后绑定落地 → 就绪；待批消失 → 回授权。
    async fn tick_rebind(&self, generation: u64, _record: &SwitchRecord) -> Result<bool, ()> {
        if self.paused() {
            return Ok(true);
        }
        let probe = tokio::task::spawn_blocking({
            let snapshot = self.snapshot();
            move || -> Result<RebindProbe, DshError> {
                let (_, auth) = (snapshot.services)()?;
                if auth.authorization_state() == AuthorizationState::Bound {
                    return Ok(RebindProbe::Bound);
                }
                Ok(if auth.pending_binding().is_some() {
                    RebindProbe::Waiting
                } else {
                    RebindProbe::PendingGone
                })
            }
        })
        .await;
        match probe {
            Ok(Ok(RebindProbe::Bound)) => self.advance(generation, |record| {
                record.phase = SwitchPhase::Ready;
            }),
            Ok(Ok(RebindProbe::Waiting)) => Ok(true),
            Ok(Ok(RebindProbe::PendingGone)) => self.advance(generation, |record| {
                record.phase = SwitchPhase::Authorizing;
            }),
            Ok(Err(e)) => self.fail_step(generation, SwitchStep::Authorize, &e),
            Err(_) => Err(()),
        }
    }

    /// 修复作业推进；终态后接续原流程（启用 → 重新推进；关闭 → 保持关闭）。
    async fn tick_repair(&self, generation: u64, record: &SwitchRecord) -> Result<bool, ()> {
        let probe = tokio::task::spawn_blocking({
            let snapshot = self.snapshot();
            let record = record.clone();
            move || -> Result<JobTick, DshError> {
                let (manager, _) = (snapshot.services)()?;
                job_tick(&manager, record.job_id.as_deref())
            }
        })
        .await;
        match probe {
            Ok(Ok(JobTick::Running)) => Ok(true),
            Ok(Ok(JobTick::Terminal(job))) => {
                if job.phase == ManagementPhase::Succeeded {
                    self.advance(generation, |record| {
                        record.phase = record.resume_phase.take().unwrap_or(SwitchPhase::Off);
                        record.job_id = None;
                        record.job_kind = None;
                    })
                } else {
                    let reason = job
                        .reason_code
                        .unwrap_or_else(|| codes::VERIFY_FAILED.to_string());
                    let error = DshError::new(codes::VERIFY_FAILED, reason);
                    self.fail_step(generation, SwitchStep::Repair, &error)
                }
            }
            Ok(Ok(JobTick::Missing)) => {
                let error = DshError::new(codes::JOB_UNKNOWN, "repair job is gone");
                self.fail_step(generation, SwitchStep::Repair, &error)
            }
            Ok(Err(e)) => self.fail_step(generation, SwitchStep::Repair, &e),
            Err(_) => Err(()),
        }
    }

    /// 卸载推进（关闭方向）：先让位/取消非卸载作业，再启动/接续卸载作业
    /// （先撤全部匹配绑定再移包；等待宿主正常退出，绝不强杀）。
    async fn tick_uninstall(&self, generation: u64, record: &SwitchRecord) -> Result<bool, ()> {
        let probe = tokio::task::spawn_blocking({
            let snapshot = self.snapshot();
            let record = record.clone();
            move || -> Result<UninstallTick, DshError> {
                let (manager, _) = (snapshot.services)()?;
                // 活跃作业让位：非卸载作业（如关闭打断的安装）先取消，等它落地。
                if let Some(operation) = manager.status().ok().and_then(|s| s.operation) {
                    if operation.kind != ManagementKind::Uninstall {
                        manager.cancel(&operation.job_id)?;
                        return Ok(UninstallTick::Yielding);
                    }
                    return Ok(uninstall_phase_tick(operation));
                }
                match job_tick(&manager, record.job_id.as_deref())? {
                    JobTick::Running => Ok(UninstallTick::Running),
                    JobTick::Terminal(job) => Ok(uninstall_phase_tick(job)),
                    JobTick::Missing => {
                        let job_id = manager.start(ManagementKind::Uninstall)?;
                        Ok(UninstallTick::Started(job_id))
                    }
                }
            }
        })
        .await;
        match probe {
            Ok(Ok(UninstallTick::Yielding)) | Ok(Ok(UninstallTick::Running)) => Ok(true),
            Ok(Ok(UninstallTick::Started(job_id))) => self.advance(generation, |record| {
                record.job_id = Some(job_id);
                record.job_kind = Some(ManagementKind::Uninstall);
            }),
            Ok(Ok(UninstallTick::Terminal(job))) => {
                if job.phase == ManagementPhase::Succeeded {
                    self.advance(generation, |record| {
                        record.phase = SwitchPhase::Off;
                        record.job_id = None;
                        record.job_kind = None;
                        record.failed_step = None;
                        record.reason_code = None;
                    })
                } else {
                    self.fail_step_preserving_reason(
                        generation,
                        SwitchStep::Uninstall,
                        job.reason_code.as_deref(),
                        codes::STATE_IO,
                        "uninstall job terminated without success",
                    )
                }
            }
            Ok(Err(e)) => self.fail_step(generation, SwitchStep::Uninstall, &e),
            Err(_) => Err(()),
        }
    }

    /// 跨线程快照（spawn_blocking 闭包的数据面；提交段统一经 `commit` 的锁）。
    fn snapshot(&self) -> RemoteSwitchRef {
        RemoteSwitchRef {
            env: Arc::clone(&self.env),
            registry: Arc::clone(&self.registry),
            instance_id: self.instance_id.clone(),
            services: Arc::clone(&self.services),
        }
    }

    // --------------------------------------------------------
    // P1-1 开关执行门（生产执行点判定内核）
    // --------------------------------------------------------

    /// 执行门的记录读取内核（**无锁**）：门可能在绑定互斥内被调（钉钥重连分支），
    /// 绝不取 `action_lock`；读取失败如实上抛（fail-closed），记录缺席按现有事实
    /// **只读派生**（升级恢复面；派生不落盘）。
    fn enforcement_record_unlocked(&self) -> Result<SwitchRecord, DshError> {
        match self.read_raw()? {
            Some(record) => Ok(record),
            None => Ok(self.derive_record()),
        }
    }

    /// P1-1 授权提交点判定（握手写绑定前；经 [`switch_authorization_gate`] 接线
    /// 到 auth 域）：enabled 且——有布防代次时——代次与当前记录一致。关闭/缺席/
    /// 不可读/代次不匹配 → `DSH_SWITCH_OFF`。
    pub fn authorization_gate_check(&self, armed_generation: Option<u64>) -> Result<(), DshError> {
        // 销项复审 P1：关闭即效面——「正在关闭」的实例不接受新的授权提交
        // （重开/修复成功时旗标清除）。
        if crate::services::dsh_control::production::is_closing(&self.instance_id) {
            return Err(DshError::new(
                codes::SWITCH_OFF,
                "instance is closing; authorization submissions refused",
            ));
        }
        let record = self.enforcement_record_unlocked()?;
        if !record.enabled {
            return Err(switch_off_error());
        }
        if let Some(armed) = armed_generation {
            if record.generation != armed {
                return Err(switch_off_error());
            }
        }
        Ok(())
    }

    /// P1-1 执行门判定（发送最终派发复核 / 附件写盘 / 控制台准入）：持久化记录
    /// 必须在「开启」窗口。生产取用入口 = [`enforcement_gate`]。
    pub fn execution_gate(&self) -> Result<(), DshError> {
        // 销项复审 P1：关闭首步旗标先于记录落盘生效——发送/上传/控制台准入
        // 的执行门在「正在关闭」期间立即拒绝（不等 persist#1/撤权段）。
        if crate::services::dsh_control::production::is_closing(&self.instance_id) {
            return Err(switch_off_error());
        }
        let record = self.enforcement_record_unlocked()?;
        if record.enabled {
            Ok(())
        } else {
            Err(switch_off_error())
        }
    }
}

/// 作业轮询内核（按记录里的 job_id；缺失/丢失 → Missing）。
fn job_tick(manager: &DshPluginManager, job_id: Option<&str>) -> Result<JobTick, DshError> {
    let Some(job_id) = job_id else {
        return Ok(JobTick::Missing);
    };
    match manager.job(job_id) {
        Ok(job) => Ok(match job.phase {
            ManagementPhase::Succeeded | ManagementPhase::Failed | ManagementPhase::Cancelled => {
                JobTick::Terminal(job)
            }
            _ => JobTick::Running,
        }),
        Err(e) if e.code == codes::JOB_UNKNOWN => Ok(JobTick::Missing),
        Err(e) => Err(e),
    }
}

fn uninstall_phase_tick(job: ManagementJob) -> UninstallTick {
    match job.phase {
        ManagementPhase::Succeeded | ManagementPhase::Failed | ManagementPhase::Cancelled => {
            UninstallTick::Terminal(job)
        }
        _ => UninstallTick::Running,
    }
}

/// P1-2 死钥识别（tick_checking 与 tick_activation **共用同一入口**）：把登记
/// 实例解析成 [`super::install::InstanceTarget`] 后委托 **钥域单源谓词**
/// [`super::install::dead_identity_in_force`]（与安装路径 `retire_stale_identity_if_revoked`
/// 同判据同实现，杜绝双谓词语义漂移）。判据 = 域内无 Bound 行 + 墓碑留存的被撤钥
/// == 身份文件公钥（或身份文件在场但读不出公钥的保守面）→ 插件带死钥：本地
/// revoked 标志与撤权墓碑双重拒绝下永远连不上，必须路由安装段先退役再装；全新钥
/// 一律放行（防重装死循环）。登记缺失 → false（保守不路由，绝不误装）。
///
/// home 契约（Round-8 P1-3 + 第九轮 P1-2 消费装配面）：探测 env.home = **已装配
/// 管理器的固定基线** `assembled_home`（调用点已过归属核验——装配单源，与安装
/// 路径 `retire_stale_identity_if_revoked` 的 env 同源）；装配面缺席（None，理论
/// 外的兜底形态）才退回登记推导 [`super::instances::verified_instance_home`]。
/// 不再从登记记录第二推导：管理器缓存命中（装配时证据可证、其后绑定失效）下
/// 登记推导与装配基线可能不同——自定义/外链 home 的宿主推导值是错目录，探测
/// 查错目录 → 死钥漏检 → tick_checking/tick_activation 不路由安装段 →
/// Authorizing 死等。
fn dead_identity_in_force(
    env: &DshEnvironment,
    registry: &InstanceRegistry,
    instance_id: &str,
    assembled_home: Option<&std::path::Path>,
) -> Result<bool, DshError> {
    let registered = registry.registered(instance_id)?;
    let probe_home = match assembled_home {
        Some(home) => home.to_path_buf(),
        None => super::instances::verified_instance_home(&registered),
    };
    let target = super::install::InstanceTarget {
        instance_id: registered.instance_id,
        kind: registered.kind,
        profile_dir: registered.profile_dir,
    };
    let mut probe_env = env.clone();
    probe_env.home = probe_home;
    Ok(super::install::dead_identity_in_force(&probe_env, &target))
}

/// 服务字段快照（驱动器 spawn_blocking 闭包的数据面）。
#[derive(Clone)]
struct RemoteSwitchRef {
    env: Arc<DshEnvironment>,
    registry: Arc<InstanceRegistry>,
    instance_id: String,
    services: Arc<SwitchServices>,
}

/// 控制台通道凭据是否已交付（API + 浏览器会话副本都在场）。
fn console_credentials_delivered(instance_id: &str) -> bool {
    crate::remote::console_lifecycle::upstream_credential(instance_id).is_some()
        && crate::remote::console_lifecycle::upstream_browser_cookie(instance_id).is_some()
}

enum InstallTick {
    Started(String),
    Running,
    Terminal(ManagementJob),
}

enum ActivationTick {
    /// 宿主运行 + 插件装载（ Installed/Conflict × Running 且无死钥）。
    Loaded,
    /// P1-2 复审补口：重开过渡点识别出死钥（撤权在证 + 旧身份在场）→ 回安装段。
    DeadIdentity,
    Missing,
    Waiting,
}

enum AuthorizeProbe {
    Bound,
    RebindRequired,
    NeedArm,
}

enum RebindProbe {
    Bound,
    Waiting,
    PendingGone,
}

enum JobTick {
    Running,
    Missing,
    Terminal(ManagementJob),
}

enum UninstallTick {
    Yielding,
    Started(String),
    Running,
    Terminal(ManagementJob),
}

// ============================================================
// 回归接缝（test-only）：布防前后确定性观测
// ============================================================

/// 布防副作用（账本 CAS 之外）的回归接缝（**按实例键**；与消费方
/// `switch_tests` 同为 unix-only 测试面——windows-gnu 交叉构建下整体缺席，
/// 不留 dead-code）。并发纪律：lib 单测默认并行，接缝必须对其他测试的驱动器
/// 透明——hook 按 instance_id 查表，未注册的实例走 None 路径照常布防；互斥锁
/// 只护查表/登记，**绝不横跨 hook 调用**（hook 可能长时间停驻等待插队）。
/// * `set_before_arm` / `before_arm`——让回归在「已读账本 → 未布防」窗口内
///   确定性插队（模拟关闭与布防竞态）；
/// * `note_stale_arm_cancel` / `stale_arm_cancelled`——按实例记录「布防后复核
///   撤销了迟到意图」，证明复核真的执行（非空转断言）；
/// * `before_arm_keys_empty`——回归收尾断言接缝已清理，泄漏键不得毒化后续运行。
#[cfg(all(test, unix))]
pub(crate) mod arm_test_hooks {
    use std::collections::{HashMap, HashSet};
    use std::sync::{Arc, Mutex, OnceLock};

    type Hook = Arc<dyn Fn() + Send + Sync>;

    fn before_arms() -> &'static Mutex<HashMap<String, Hook>> {
        static HOOKS: OnceLock<Mutex<HashMap<String, Hook>>> = OnceLock::new();
        HOOKS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    fn stale_arm_cancels() -> &'static Mutex<HashSet<String>> {
        static CANCELS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
        CANCELS.get_or_init(|| Mutex::new(HashSet::new()))
    }

    pub fn set_before_arm(instance_id: &str, hook: Option<Hook>) {
        let mut guard = before_arms().lock().unwrap();
        match hook {
            Some(hook) => {
                guard.insert(instance_id.to_owned(), hook);
            }
            None => {
                guard.remove(instance_id);
            }
        }
    }

    pub fn before_arm(instance_id: &str) {
        // 查表与调用分离：锁在 clone 后即释放，hook 停驻不占互斥锁。
        let hook = before_arms().lock().unwrap().get(instance_id).cloned();
        if let Some(hook) = hook {
            hook();
        }
    }

    pub fn note_stale_arm_cancel(instance_id: &str) {
        stale_arm_cancels()
            .lock()
            .unwrap()
            .insert(instance_id.to_owned());
    }

    pub fn stale_arm_cancelled(instance_id: &str) -> bool {
        stale_arm_cancels().lock().unwrap().contains(instance_id)
    }

    pub fn before_arm_keys_empty() -> bool {
        before_arms().lock().unwrap().is_empty()
    }

    // ----------------------------------------------------
    // enable 入口接缝（销项复审 P1-2 回归）：在「enable 已入口 → 未提交
    // enabled」窗口内确定性插队（模拟观察纪元之后、条件清除之前到达的新关闭）。
    // 查表纪律与 before_arm 同（锁不横跨 hook 调用；未注册实例走 None 路径）。
    // ----------------------------------------------------
    fn before_enables() -> &'static Mutex<HashMap<String, Hook>> {
        static HOOKS: OnceLock<Mutex<HashMap<String, Hook>>> = OnceLock::new();
        HOOKS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    pub fn set_before_enable(instance_id: &str, hook: Option<Hook>) {
        let mut guard = before_enables().lock().unwrap();
        match hook {
            Some(hook) => {
                guard.insert(instance_id.to_owned(), hook);
            }
            None => {
                guard.remove(instance_id);
            }
        }
    }

    pub fn before_enable(instance_id: &str) {
        let hook = before_enables().lock().unwrap().get(instance_id).cloned();
        if let Some(hook) = hook {
            hook();
        }
    }

    pub fn before_enable_keys_empty() -> bool {
        before_enables().lock().unwrap().is_empty()
    }

    /// 驱动器退场观测（按实例累计计数）：重启模拟用例等待「旧代驱动器已退场」
    /// 的确定性面——进程内没有杀任务的接缝，用例以代次更替（驱动器既有退场
    /// 路径）逼退旧驱动器，再等本计数递增确认退场完成后才恢复盘面。
    pub fn note_driver_exit(instance_id: &str) {
        let mut exits = driver_exits().lock().unwrap();
        let next = exits.entry(instance_id.to_owned()).or_insert(0);
        *next = next.checked_add(1).expect("driver exit counter overflow");
    }

    pub fn driver_exit_count(instance_id: &str) -> u64 {
        driver_exits()
            .lock()
            .unwrap()
            .get(instance_id)
            .copied()
            .unwrap_or(0)
    }

    /// 驱动器退场计数（按实例；仅测试面消费）。
    fn driver_exits() -> &'static Mutex<HashMap<String, u64>> {
        static EXITS: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
        EXITS.get_or_init(|| Mutex::new(HashMap::new()))
    }
}

// ============================================================
// 回归（brief 指定验收行为；隔离 TUVIS_HOME/DSH 状态目录，绝不触真实家目录）
// ============================================================
#[cfg(all(test, unix))]
#[path = "switch_tests.rs"]
mod switch_tests;
