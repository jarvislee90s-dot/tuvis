//! B1 一键恢复 pending 状态机（F4 项 4A · 待确认态；HD 出手前原子占位 + 四要素
//! 迟到观察；G5 不二窗保持）。
//!
//! 回执 = **真实窗口观察**（brief 4A.3）：起窗请求出手后，经「进程扫描 + 登记核对」
//! 确认「新 TUI 进程 argv 含目标 session + 其 serve 登记」才回执成功；观察未就绪 →
//! **待确认态保留**（本注册表），**绝不自动再开第二个窗口**；占位未核销时重复
//! 恢复请求**零出手**（幂等返回 pending，明确码 `OC_RESTORE_PENDING`）；观察确认
//! 或用户显式放弃才流转。超时保持待确认 + 提示手动核对（fail-closed，不冒充成功）。
//!
//! ## HD（H4）出手前原子占位（桌面 + 远程共用按会话恢复权）
//! - **占位前移**：G5 的 pending 在交付后插入；HD 起把占位前移到**出手之前**——
//!   三核对（目录 + 端点回环 + 登记进程身份）通过后、spawner 出手前，单锁
//!   `entry` 按会话**原子取得**恢复权（同一会话同时至多一个占位）。桌面命令
//!   （commands/session.rs `session_open`）与远程 API（remote/api.rs
//!   `session_open`）消费同一 [`open_restore_with_pending`]——跨通道并发同一会话
//!   **恰一窗**（败者转复查观察分诊，零第二次 osascript；占位失败复用
//!   PendingRefused 同族码）。
//! - **释放规则（用户原文口径）**：只有**明确未交付**（起窗请求确认失败——确定性
//!   错误 / 三核对失败）才释放占位（或允许备用终端）；**osascript 超时、交付结果
//!   未知、窗口观察迟到 ⇒ 转待确认只核对**（占位保持、不二窗）。交付未知分类
//!   （[`super::resume::is_delivery_unknown_error`]）与 HC 交付处置同源常量；
//!   resume.rs `open_macos_with` 同判据阻断「超时后转次选通道」。
//! - **确认四要素**：窗口 + 会话 + 目录 + **服务代际**（GB 四要素加代际一致性——
//!   占位取得时刻锚定 discovery `RuntimeKey`（service_id + pid + started_at_ms，
//!   与绑定行 `opencode_native_pinned_windows` 同源同形），观察核对时现行登记
//!   代际必须与锚全等；serve 换代 ⇒ 拒绝核实，旧代际残迹不归入新状态）。
//! - 占位与 restore_pending 既有状态机**同一套状态**（同表同流转，无第二套）。
//!
//! ## I3 恢复核销按本次请求唯一标识 + 起窗前窗口基线
//! - **请求唯一标识**：占位取得时生成 `request_token`（resume.rs
//!   `unique_request_token` 同核——HC EnvSecret 文件名 token 同族：纳秒+pid+计数器，
//!   不建第二套标识）。核销/释放/状态推进**全部按 token 比对**（[`remove_if_token`]）
//!   ——迟到结果只核销自己那条请求的占位，绝不按会话 ID 误删后续请求的占位。
//! - **状态三分**（[`RestorePendingState`]）：**准备中**（占位已取得、spawner 未
//!   出手——观察器不得核销，并发核对不得在首请求出手前清占位）/ **已交付**（交付
//!   回调触发即推进）/ **待观察**（出手后交付未知、观察未决——迟到观察可核销）。
//!   核销只接受非准备中形态（`redeemable`）。
//! - **起窗前窗口基线**：出手前一次观察快照（[`production_baseline_snapshot`]，
//!   一次进程扫描按 effect_sig 过滤——成本申报：仅 opencode 首请求出手前一次）
//!   存入占位；核销要求确认窗口 **∉ 基线**（[`window_in_baseline`]，pid+cmd 全等
//!   = 同一窗口身份）——旧窗口（起窗前已在）不得提前核销新恢复。
//! - 观察缝（[`RestoreObserver`]）改为返回**全部候选窗口**（空集 = 未就绪）；
//!   基线外排除/token 比对/状态闸统一收口在状态机（[`pick_new_window`] +
//!   [`remove_if_token`] + `redeemable`）——跨环境可测，观察器只负责发现。
//! - 显式放弃按会话 ID 出清（用户全局操作语义：放弃 = 停掉该会话当前在途恢复，
//!   不论占位属于哪条请求——区别于流内核销的身份闸）。**IC fix round 1（P1-B
//!   用户裁决）收紧**：准备或交付仍在执行时（`in_flight`）放弃不得直接释放起窗权
//!   （交付链无取消语义，执行方无法确认取消 ⇒ 占位保持 + 诚实回执
//!   [`RestoreFlow::AbandonDeferred`]）——否则放弃删占位后原请求仍可继续交付、
//!   新请求也能取得占位 ⇒ 两窗；执行收口后放弃走既有出清语义。
//! - **交付回调只更新仍匹配本次 request_token 的占位，空位零重插**（P1-B：HD 起
//!   占位先于出手在场，回调只承担状态推进——占位先行消亡 = 起窗权已移出本请求，
//!   旧请求不得借回调复活）。
//!
//! ## G5（慢起窗不二窗）保持
//! - 占位已先于出手在场；交付回调保留为 `open_macos_with`「交付后效果回查未命中
//!   不转次选通道」的开关（同 token 状态推进），迟到观察只做核对（五要素），
//!   不自动尝试第二个窗口。
//! - **五要素**观察面（[`production_restore_observer`]）：窗口+会话 = 命令行快照
//!   含出手特征；目录 = 命中进程 cwd 归一后等于目标目录（cwd 刷不出 ⇒ 不命中，
//!   fail-closed）；服务+代际 = opencode_native discovery **登记发现分支全链** +
//!   代际锚全等；**serve 连接就绪**（IC fix round 1 · P2-C：候选 pid 持有到目标
//!   serve 端点的 ESTABLISHED 连接——新窗口未连接成功时前四要素也可能同时成立，
//!   连接证据把「确认」与「实际服务连接」关联；探针不可得 ⇒ fail-closed 不确认）。
//! - 交付失败（确定性错误，请求未达终端）⇒ 占位释放（HD），复请求走既有
//!   failed-retry 语义（F4 报告 §4.4 口径保持）。
//!
//! ## G2（起窗前三核对）
//! 「目标会话存在且属规范目录」在本状态机 ④ 前置（project_path realpath 化 +
//! is_dir，失败零起窗零 pending，哨兵 `session_directory_unresolved`）；端点回环 /
//! 登记进程身份两核对在预备链（resume.rs `prepare_opencode_resume_with_sources`）。
//!
//! ## 结构（构造与执行分离，resume.rs 同款纪律）
//! - [`restore_with_pending_in`]：核心状态机（注册表/观察/预算/时钟/登记面/
//!   交付未知分类全注入，测试零全局态零真实资产触碰）；[`open_restore_with_pending`]
//!   = 生产装配（全局注册表 + 生产观察 + 生产登记面来源），endpoint（remote/api.rs
//!   `session_open`）与桌面命令（commands/session.rs `session_open`）共同消费。
//! - 持久化位置申报：**内存 + 重启重查**（进程级 `HashMap`；重启清零后由「复请求
//!   先复查观察」防双开——窗口若在，复查命中即核销，不再开新窗）。
//!
//! ## 作用域
//! pending 门只作用于 **opencode** 会话（B1 恢复面 = 登记发现分支所在面）；
//! 其余工具保持既有行为（出手成功即回执 opening，零行为变更）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use once_cell::sync::Lazy;
use sha2::{Digest, Sha256};

use crate::services::opencode_native::types::{RuntimeKey, ServiceDescriptor};
use crate::session::Session;

use super::create_path::CreateDirectory;
use super::resume::{
    prepare_session_terminal, refresh_cmd_cwd_snapshot, resume_effect_in_snapshot, DeliveryHook,
    SpawnFn,
};

/// pending 注册表类型（G5：交付回调需要与流转主体共享注册表——锁型参数注入，
/// 测试自建锁实例，核心仍零全局态）。
pub(crate) type PendingRegistry = Mutex<HashMap<String, RestorePending>>;

/// 待确认态条目（按 session_id 一条）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RestorePending {
    pub session_id: String,
    /// 效果回查特征（出手载荷的 argv 直拼形态，含目标 session id——唯一）。
    pub effect_sig: String,
    /// 登记发现分支的状态文件（discovery `state_file_for` 同一来源）；None =
    /// home 不可解析（登记面按不可核验处理，fail-closed）。
    pub state_file: Option<PathBuf>,
    /// G5 观察四要素之「目录」面（canonicalize 后的目标目录）；None = 目录不可
    /// 解析（观察面按不可核验处理，fail-closed 永不确认——目录已不在 ⇒ `cd` 落
    /// 点不可证，保持待确认是诚实语义）。
    pub expected_directory: Option<PathBuf>,
    pub created_at_ms: i64,
    /// HC（H3）交付未知生命周期：本轮交付的受控入口 env 文件路径（resume.rs
    /// `EnvSecret.path` 同源）。交付未知（osascript 超时/等待失败）时文件保留至
    /// 观察窗结算（确认核销/显式放弃即删，[`settle_env_secret_file`]）；24h 清扫
    /// 兜底（resume.rs `sweep_stale_resume_env_files`）。None = 无受控入口（自托管
    /// 形态/非 opencode），结算零动作。
    pub env_secret_path: Option<PathBuf>,
    /// HD（H4）服务代际锚（确认四要素之「服务代际」面）：占位取得时刻的登记代际
    /// （discovery `RuntimeKey` = service_id + pid + started_at_ms，与绑定行
    /// `opencode_native_pinned_windows` 的 service_id/runtime_pid/runtime_started_at_ms
    /// 同源同形）。观察核对时**现行登记代际必须与锚全等**——占位与观察之间 serve
    /// 换代（重启/pid 复用/换 id）⇒ 拒绝核实（旧代际残迹不归入新状态，保持待确认）。
    /// None = 占位时刻登记面不可核验（登记缺失/不可解析）——fail-closed 永不确认。
    pub generation: Option<RuntimeKey>,
    created_target: Option<CreatedRestoreTarget>,
    /// I3 占位状态：准备中（占位已取得、spawner 未出手——观察器不得核销）/
    /// 已交付（交付信号成立）/ 待观察（出手后交付未知、观察未决）。核销只接受
    /// 「已交付或待观察」形态（准备中态下任何观察都不得核销）。
    pub state: RestorePendingState,
    /// I3 本次恢复请求唯一标识（占位取得时生成；resume.rs `unique_request_token`
    /// 同核——HC EnvSecret 文件名 token 同族，纳秒+pid+计数器，不建第二套标识）。
    /// 核销/释放/状态推进**全部按 token 比对**：迟到结果只核销自己那条请求的
    /// 占位，绝不按会话 ID 误删后续请求的占位。
    pub request_token: String,
    /// I3 起窗前窗口基线：出手前一次观察快照（当时已存在的匹配窗口集合，生产 =
    /// 一次进程扫描按 effect_sig 过滤——快照成本申报：仅 opencode 首请求出手前
    /// 一次）。核销要求确认窗口 **∉ 基线**——旧窗口（起窗前已在）不得提前核销
    /// 新恢复。
    pub baseline: Vec<ObservedWindow>,
    /// IC fix round 1（P1-B）起窗权在途标记：本占位的**流转线程仍在执行**
    /// （准备中：占位已取得、spawner 未出手；或已交付但观察窗/交付链仍在途）。
    /// 交付链无法确认取消（osascript 无取消语义）⇒ 执行在途时**放弃不得直接
    /// 释放起窗权**（用户裁决：否则放弃删占位后原请求仍可交付、新请求也能取得
    /// 占位 ⇒ 两窗）——在途时放弃得诚实回执（[`RestoreFlow::AbandonDeferred`]，
    /// 占位保持）。流转线程收口时清零（按 token 比对——占位已易主时零触碰）。
    /// false = 执行已结束（待确认滞留/待观察）——既有「放弃 = 按会话结算出清」
    /// 语义保持。
    pub in_flight: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CreatedRestoreTarget {
    directory: CreateDirectory,
    endpoint: reqwest::Url,
    version: String,
    credential_hash: [u8; 32],
}

impl CreatedRestoreTarget {
    fn matches(&self, service: &ServiceDescriptor) -> bool {
        let credential_hash: [u8; 32] = Sha256::digest(service.password.as_bytes()).into();
        self.endpoint == service.endpoint
            && self.version == service.version
            && self.credential_hash == credential_hash
            && self.directory.still_current()
    }
}

/// I3 观察到的候选窗口标识（进程快照条目：pid + 命令行拼接串；两者全等 = 同一
/// 窗口身份——基线比对按全等，防 pid 复用误判）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ObservedWindow {
    pub pid: u32,
    pub cmd: String,
}

/// I3 占位状态（「准备中/已交付/待观察」三分；核销只接受非准备中形态）：
/// * [`Self::Preparing`]：占位已取得、spawner 未出手——**观察器不得核销**
///   （并发核对不得在首请求出手前清占位）；
/// * [`Self::Delivered`]：交付信号成立（交付回调已触发——「交付即插 pending」
///   的已交付态，等待观察窗核销）；
/// * [`Self::AwaitingObservation`]：出手已发生但交付未知（osascript 超时/等待
///   失败——Apple 事件可能已送达），观察未决——迟到观察可核销（核销条件同
///   已交付态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RestorePendingState {
    Preparing,
    Delivered,
    AwaitingObservation,
}

impl RestorePendingState {
    /// 核销资格：准备中态永不合格（fail-closed）；已交付/待观察形态合格
    ///（「已交付且待观察」= 窗口可能真实在场的两种形态）。
    fn redeemable(self) -> bool {
        !matches!(self, Self::Preparing)
    }
}

/// 观察缝：回执 = 真实窗口观察（进程面 + 登记面）。生产 = [`production_restore_observer`]；
/// 测试 = 记录型假体（零真实进程表/文件系统触碰）。I3 起返回**全部候选窗口**
/// （四要素进程面命中的每一扇），空集 = 未就绪；「不在起窗前基线内」的排除由
/// 状态机统一施加（[`window_in_baseline`]）——观察器只负责发现，核销资格判定
/// （基线外 + token + 状态）收口在状态机（跨环境可测）。
pub(crate) type RestoreObserver = dyn Fn(&RestorePending) -> Vec<ObservedWindow> + Send + Sync;

/// 恢复流转结果。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RestoreFlow {
    /// 本轮出手了 spawn。`confirmed = true`：出手后观察窗内核实（回执成功）；
    /// `false`：观察窗耗尽未就绪 → **待确认态保留**（不冒充成功）。
    Dispatched { confirmed: bool },
    /// pending 未核销：本轮**零出手**（重复恢复请求被拒/幂等返回 pending）。
    PendingRefused,
    /// 复请求复查观察命中：核销 pending（零出手、零重复开窗）。
    PendingConfirmed,
    /// 显式放弃：清 pending（零出手）。无 pending 时幂等同形。
    Abandoned,
    /// IC fix round 1（P1-B）：放弃被暂缓——起窗权在途（准备/交付仍在执行，交付
    /// 链无法确认取消），**占位保持**（诚实回执：交付进行中，无法放弃，请等待
    /// 核对）。零出手、零状态变化；待核对/结算后再放弃即可（执行收口后放弃走
    /// [`RestoreFlow::Abandoned`] 既有语义）。
    AbandonDeferred,
}

/// 出手后观察窗预算与采样间隔（长等待分短 sleep 纪律）。
const RESTORE_OBSERVE_WINDOW: Duration = Duration::from_secs(5);
const RESTORE_OBSERVE_POLL: Duration = Duration::from_millis(500);

/// 全局 pending 注册表（进程级；持久化位置 = 内存 + 重启重查，见模块 doc）。
static PENDINGS: Lazy<PendingRegistry> = Lazy::new(|| PendingRegistry::new(HashMap::new()));

/// J1（竞窗注入缝，仅测试）：abandon「读取旧占位之后、删除之前」的确定性停点。
/// 修复前两锁形态在此间隙可插入「旧占位核销 → 新请求占位」；修复后单临界区，
/// 钩子在临界区之后触发（序列变为「原子删除旧占位 → 钩子：旧核销空转+新占位
/// 插入」→ 新占位存活）。
#[cfg(test)]
type AbandonPauseHook = std::sync::Arc<dyn Fn() + Send + Sync>;
/// J1（用户复审测试修整）：停点按**会话 ID 限定**——全局单槽会误伤并行其他
/// abandon 用例（调度依赖）。fire 只查本会话键；测试用 install/remove 装卸。
#[cfg(test)]
static TEST_BETWEEN_ABANDON_LOCKS: Lazy<Mutex<HashMap<String, AbandonPauseHook>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

#[cfg(test)]
fn test_between_abandon_locks_fire(sid: &str) {
    let hook = TEST_BETWEEN_ABANDON_LOCKS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(sid)
        .map(std::sync::Arc::clone);
    if let Some(hook) = hook {
        hook();
    }
}

#[cfg(test)]
fn install_between_abandon_locks_pause(sid: &str, hook: AbandonPauseHook) {
    TEST_BETWEEN_ABANDON_LOCKS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(sid.to_string(), hook);
}

#[cfg(test)]
fn remove_between_abandon_locks_pause(sid: &str) {
    TEST_BETWEEN_ABANDON_LOCKS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(sid);
}

/// 测试观察覆盖（per-session id；并行测试按 sid 隔离，零交叉）。
#[cfg(test)]
static OBSERVER_OVERRIDES: Lazy<Mutex<HashMap<String, std::sync::Arc<RestoreObserver>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// 测试观察窗预算覆盖（endpoint 测试免等 5s 全窗）。
#[cfg(test)]
static OBSERVE_BUDGET_OVERRIDE: Lazy<Mutex<Option<Duration>>> = Lazy::new(|| Mutex::new(None));

#[cfg(test)]
pub(crate) fn reset_for_tests() {
    PENDINGS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
    OBSERVER_OVERRIDES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clear();
    *OBSERVE_BUDGET_OVERRIDE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
}

#[cfg(test)]
pub(crate) fn set_observer_for_tests(session_id: &str, observer: std::sync::Arc<RestoreObserver>) {
    OBSERVER_OVERRIDES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(session_id.trim().to_string(), observer);
}

#[cfg(test)]
pub(crate) fn set_observe_budget_for_tests(budget: Duration) {
    *OBSERVE_BUDGET_OVERRIDE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(budget);
}

/// 生产观察（真实窗口观察，五要素核对）——注入核 [`restore_observer_with`] 的
/// 生产装配（真实进程快照 + 真实登记读取 + 真实进程事实 + 真实连接探针）。
fn production_restore_observer(pending: &RestorePending) -> Vec<ObservedWindow> {
    let mut sys = sysinfo::System::new();
    let snapshot = refresh_cmd_cwd_snapshot(&mut sys);
    let windows = restore_observer_with(
        pending,
        &snapshot,
        &read_state_file_io_err,
        &crate::monitor::opencode_observation::production_process_fact,
        &production_serve_conn_established,
    );
    #[cfg(target_os = "macos")]
    let windows = if let Some(created) = &pending.created_target {
        let mut windows = windows;
        let Some(runtime) = &pending.generation else {
            return Vec::new();
        };
        let Some(host_port) = serve_host_port(&created.endpoint) else {
            return Vec::new();
        };
        windows.retain(|window| production_tcp_pair(window.pid, runtime.pid, &host_port));
        if !windows.is_empty()
            && !current_restore_service(
                pending,
                &read_state_file_io_err,
                &crate::monitor::opencode_observation::production_process_fact,
            )
            .is_some_and(|service| created.matches(&service))
        {
            return Vec::new();
        }
        windows
    } else {
        windows
    };
    windows
}

/// 观察核（五要素核对；快照/登记读/进程事实/连接探针全注入——跨环境可测，测试
/// 零真实进程表/登记/网络触碰）：
/// ①+③ 进程面 = 命令行快照含效果特征（新 TUI 进程 argv 含目标 session id =
/// 「窗口 + 会话」两要素）**且** 该命中进程的 cwd 归一后等于目标目录（「目录」
/// 要素——`cd '<cwd>'` 落点核对；cwd 刷不出 ⇒ 不命中，fail-closed）——返回
/// 进程+目录面命中的全部候选；
/// ② 登记面 = discovery **登记发现分支全链**（状态文件可读 + 形状合法 + 回环端点 +
/// 登记 pid 存活——pid 不在场即旧代际残迹，拒绝核实）=「服务」要素 **+ 代际锚
/// 全等**（现行登记代际与占位锚 `RuntimeKey` 全等——serve 换代即拒绝核实）；
/// ⑤ **serve 连接就绪**（IC fix round 1 · P2-C）：候选窗口 pid 持有到目标 serve
/// 端点（**代际锚全等的登记描述符**给出的 host:port——连接证据与预期服务代际
/// 关联，防连到别代 serve 也算数）的 ESTABLISHED 连接——新窗口「进程已起」与
/// 「实际连接」是两件事：连接未就绪时前四要素也可能同时成立（TUI 起了但连不上/
/// 连错），不确认（保持待确认，宁核对不冒充）。探针不可得/调用失败 ⇒ 候选剔除
/// （fail-closed 不确认）。
/// 目录要素锚缺席（expected_directory None）⇒ 要素不全 ⇒ 空集（保持待确认）。
/// 任一要素缺席 = 未就绪（保持待确认）；**迟到观察只核对，绝不自动开第二窗**。
fn restore_observer_with(
    pending: &RestorePending,
    snapshot: &[super::resume::CmdCwdEntry],
    read_file: &dyn Fn(
        &std::path::Path,
    )
        -> Result<Vec<u8>, crate::services::opencode_native::types::NativeError>,
    process_fact: &dyn Fn(u32) -> Option<i64>,
    conn_probe: &dyn Fn(u32, &str) -> bool,
) -> Vec<ObservedWindow> {
    // ①③ 进程面 + 目录面
    let Some(expected_dir) = &pending.expected_directory else {
        return Vec::new();
    };
    let mut hits: Vec<ObservedWindow> = snapshot
        .iter()
        .filter(|entry| {
            if !resume_effect_in_snapshot(&pending.effect_sig, std::slice::from_ref(&entry.cmd)) {
                return false;
            }
            let Some(cwd) = &entry.cwd else {
                return false;
            };
            crate::monitor::cwd::cwd_equivalent(
                &cwd.to_string_lossy(),
                &expected_dir.to_string_lossy(),
            )
        })
        .map(|entry| ObservedWindow {
            pid: entry.pid,
            cmd: entry.cmd.clone(),
        })
        .collect();
    if hits.is_empty() {
        return Vec::new();
    }
    // ② 登记面（登记发现分支全链 + HD 服务代际一致性）
    let Some(descriptor) = current_restore_service(pending, read_file, process_fact) else {
        return Vec::new();
    };
    if pending
        .created_target
        .as_ref()
        .is_some_and(|created| !created.matches(&descriptor))
    {
        return Vec::new();
    }
    // ⑤ serve 连接就绪面（连接目标 = 代际锚全等描述符的端点）
    let Some(host_port) = serve_host_port(&descriptor.endpoint) else {
        return Vec::new();
    };
    hits.retain(|w| conn_probe(w.pid, &host_port));
    hits
}

fn current_restore_service(
    pending: &RestorePending,
    read_file: &dyn Fn(
        &Path,
    )
        -> Result<Vec<u8>, crate::services::opencode_native::types::NativeError>,
    process_fact: &dyn Fn(u32) -> Option<i64>,
) -> Option<ServiceDescriptor> {
    let mut input = crate::services::opencode_native::discovery::production_input();
    input.state_file = pending.state_file.clone()?;
    registration_descriptor_with(&input, pending.generation.as_ref(), read_file, process_fact)
}

#[cfg(target_os = "macos")]
fn established_tcp_names(pid: u32) -> Vec<String> {
    let output = std::process::Command::new("/usr/sbin/lsof")
        .args([
            "-nP",
            "-a",
            "-p",
            &pid.to_string(),
            "-iTCP",
            "-sTCP:ESTABLISHED",
            "-Fn",
        ])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix('n').map(str::to_owned))
        .collect()
}

#[cfg(target_os = "macos")]
fn production_tcp_pair(client_pid: u32, service_pid: u32, host_port: &str) -> bool {
    let service = established_tcp_names(service_pid);
    established_tcp_names(client_pid).iter().any(|name| {
        name.split_once("->").is_some_and(|(local, remote)| {
            tcp_peer_matches(remote, host_port)
                && service
                    .iter()
                    .any(|peer| peer.split_once("->") == Some((remote, local)))
        })
    })
}

#[cfg(target_os = "macos")]
fn tcp_peer_matches(remote: &str, host_port: &str) -> bool {
    remote == host_port
        || host_port
            .strip_prefix("localhost:")
            .and_then(|port| port.parse::<u16>().ok())
            .is_some_and(|port| {
                remote
                    .parse::<std::net::SocketAddr>()
                    .is_ok_and(|peer| peer.ip().is_loopback() && peer.port() == port)
            })
}

/// serve 端点的 `host:port` 直拼形态（连接探针比对键）；host 或 port 缺失 ⇒
/// None（fail-closed——端点不完整即无法给出连接证据键，不确认）。
fn serve_host_port(endpoint: &reqwest::Url) -> Option<String> {
    let host = endpoint.host_str()?;
    let port = endpoint.port()?;
    Some(format!("{host}:{port}"))
}

/// 生产 serve 连接证据缝（P2-C）：候选窗口进程是否持有到目标 serve（host:port）
/// 的 **ESTABLISHED** TCP 连接。平台实现：
/// * macOS：`lsof -a -i -n -P -p <pid>` 解析 `-><host:port>` + `ESTABLISHED` 行
/// * Windows：`netstat -ano -p TCP` 解析 foreign = host:port + state +
///   pid 列（`-n` 数字形态，无 DNS/服务名解析）
/// * 其余平台：false（fail-closed；注入平台门本就只放行 Windows/macOS 起窗链）
///
/// 成本申报：仅当候选窗口通过进程+目录+登记三面后**每候选一次**子进程调用
///（通常 0–1 次/轮询拍），不在空拍路径出手。调用失败/解析不出 = 连接证据不可得
/// ⇒ false（fail-closed：不确认，保持待核对）。
fn production_serve_conn_established(pid: u32, host_port: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        established_tcp_names(pid).iter().any(|name| {
            name.split_once("->")
                .is_some_and(|(_, remote)| tcp_peer_matches(remote, host_port))
        })
    }
    #[cfg(windows)]
    {
        let Ok(out) = std::process::Command::new("netstat")
            .args(["-ano", "-p", "TCP"])
            .output()
        else {
            return false;
        };
        String::from_utf8_lossy(&out.stdout).lines().any(|line| {
            let cols: Vec<&str> = line.split_whitespace().collect();
            // `TCP  <local>  <foreign>  <state>  <pid>`（-n 数字形态）
            cols.len() == 5
                && cols[0] == "TCP"
                && cols[2] == host_port
                && cols[3] == "ESTABLISHED"
                && cols[4] == pid.to_string()
        })
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = (pid, host_port);
        false
    }
}

/// I3 生产基线快照（出手前一次观察）：进程快照按 effect_sig 过滤出「当前已存在
/// 的匹配窗口集合」。快照成本 = 一次进程扫描（与观察窗轮询同源同形——本轮申报：
/// 仅 opencode 首请求出手前一次，可接受）。
fn production_baseline_snapshot(effect_sig: &str) -> Vec<ObservedWindow> {
    let mut sys = sysinfo::System::new();
    refresh_cmd_cwd_snapshot(&mut sys)
        .into_iter()
        .filter(|entry| resume_effect_in_snapshot(effect_sig, std::slice::from_ref(&entry.cmd)))
        .map(|entry| ObservedWindow {
            pid: entry.pid,
            cmd: entry.cmd,
        })
        .collect()
}

/// 登记状态文件真实读取（生产装配用；读取失败包装为 OC_IO 结构化错误）。
fn read_state_file_io_err(
    path: &std::path::Path,
) -> Result<Vec<u8>, crate::services::opencode_native::types::NativeError> {
    std::fs::read(path).map_err(|e| {
        crate::services::opencode_native::types::NativeError::new(
            "OC_IO",
            &format!("state file unreadable: {e}"),
        )
    })
}

/// 登记面核对纯函数（HD 确认五要素之「服务 + 服务代际」两要素；跨环境可测——
/// 读/进程事实全注入，零真实登记触碰）：登记发现分支全链可核 + **现行登记代际
/// 与占位锚全等**（`RuntimeKey` = service_id + pid + started_at_ms 逐字一致——
/// 占位与观察之间 serve 换代即不等）。锚缺席（占位时刻登记面不可核验）⇒ None
/// （fail-closed：登记代际从未可证，永不确认——旧代际残迹不冒充新状态）。
/// 返回核验通过的登记描述符（观察核据此取 serve 端点做连接证据，P2-C）。
fn registration_descriptor_with(
    input: &crate::services::opencode_native::types::DiscoveryInput,
    anchor: Option<&RuntimeKey>,
    read_file: &dyn Fn(
        &std::path::Path,
    )
        -> Result<Vec<u8>, crate::services::opencode_native::types::NativeError>,
    process_fact: &dyn Fn(u32) -> Option<i64>,
) -> Option<crate::services::opencode_native::types::ServiceDescriptor> {
    let anchor = anchor?;
    let descriptor =
        crate::services::opencode_native::discovery::discover(input, read_file, process_fact)
            .ok()?;
    (descriptor.runtime == *anchor).then_some(descriptor)
}

/// 生产登记面来源（HD 注入缝的生产装配）：状态文件路径 + 占位时刻服务代际锚
/// （discovery 全链读取）。登记缺失/不可解析 → (None, None)——登记面 fail-closed
/// （与既有「home 不可解析」同形）。
fn production_registration_source() -> (Option<PathBuf>, Option<RuntimeKey>) {
    let Some(state_file) = production_state_file() else {
        return (None, None);
    };
    let mut input = crate::services::opencode_native::discovery::production_input();
    input.state_file = state_file.clone();
    let generation = crate::services::opencode_native::discovery::discover(
        &input,
        &read_state_file_io_err,
        &crate::monitor::opencode_observation::production_process_fact,
    )
    .ok()
    .map(|d| d.runtime);
    (Some(state_file), generation)
}

/// 解析登记状态文件路径（生产来源 = 进程环境 HOME/XDG_STATE_HOME，与 discovery
/// 登记发现分支同一来源）；home 不可解析 → None（登记面 fail-closed）。
fn production_state_file() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let xdg = std::env::var_os("XDG_STATE_HOME").map(std::path::PathBuf::from);
    Some(crate::services::opencode_native::discovery::state_file_for(
        &home,
        xdg.as_deref(),
    ))
}

/// 起窗执行缝类型（G5 四参：载荷/会话/spawner/交付回调；clippy type_complexity
/// 收敛别名）。生产 = [`super::resume::execute_prepared_spawn_with_delivery`]。
/// I3：`Send + Sync`（I3 并发测试跨线程共享 deps——三并发占位竞争）。
pub(crate) type RestoreExecuteFn<'a> = dyn Fn(
        &super::resume::PreparedSpawn,
        &Session,
        &SpawnFn,
        Option<&DeliveryHook<'_>>,
    ) -> Result<(), String>
    + Send
    + Sync
    + 'a;

/// 恢复链依赖束（构造/执行/观察三缝 + spawner + HD 登记面/交付未知两缝 +
/// I3 基线快照缝；clippy too_many_arguments 收敛）。I3：全部缝 `Send + Sync`
///（I3 并发测试跨线程共享 deps——三并发占位竞争；`RestoreDeps` 随字段自动
/// Send + Sync，跨线程借用/移动合法）。
pub(crate) struct RestoreDeps<'a> {
    /// 起窗预备（纯构造）：命令表/哨兵/opencode 预备门（生产 =
    /// [`super::resume::prepare_session_terminal`]）。
    pub prepare:
        &'a (dyn Fn(&Session) -> Result<super::resume::PreparedSpawn, String> + Send + Sync),
    /// 起窗执行（平台分派 + spawner 出手；G5：第四参 = 交付回调——交付即调回调）。
    pub execute: &'a RestoreExecuteFn<'a>,
    pub spawner: &'a SpawnFn,
    /// 观察缝（真实窗口观察，返回全部候选窗口；生产 = [`production_restore_observer`]）。
    pub observer: &'a RestoreObserver,
    /// I3 起窗前窗口基线缝：返回「当前已存在的匹配窗口集合」（生产 =
    /// [`production_baseline_snapshot`]，一次进程扫描；测试 = 假体零真实扫描）。
    /// 出手前采集一次存入占位，核销要求确认窗口 ∉ 基线。
    pub baseline_snapshot: &'a (dyn Fn(&str) -> Vec<ObservedWindow> + Send + Sync),
    /// HD（H4）登记面来源缝：返回 (状态文件路径, 占位时刻服务代际锚)。生产 =
    /// [`production_registration_source`]（真实登记读取）；测试 = 假体（零真实
    /// 登记触碰）。锚 None = 登记面不可核验（fail-closed）。
    created_target: Option<CreatedRestoreTarget>,
    on_confirmed: Option<&'a (dyn Fn(u32) -> bool + Sync)>,
    pub registration_source: &'a (dyn Fn() -> (Option<PathBuf>, Option<RuntimeKey>) + Send + Sync),
    /// HD（H4）交付未知分类缝：execute Err 文案是否「交付未知」（Apple 事件可能
    /// 已送达）。生产 = [`super::resume::is_delivery_unknown_error`]（与 HC 交付
    /// 处置同源常量）；测试可注入。true = 占位保持转待确认；false = 明确未交付
    /// （占位释放，重试合法）。
    pub delivery_unknown: &'a (dyn Fn(&str) -> bool + Send + Sync),
}

/// 核心状态机（全缝注入，测试零全局态；注册表 = 锁型注入供 G5 交付回调共享）：
/// ① 非 opencode：既有链（预备 → 出手成功即 confirmed，无 pending 面，零行为变更）；
/// ② opencode + abandon：清 pending（幂等，零出手）；
/// ③ opencode + 占位在场（复请求/并发败者）：**先复查观察**（零出手）——命中即
///    核销，未命中即拒绝（占位失败复用 PendingRefused 同族码）；
/// ②' opencode + abandon：**P1-B 在途守卫**——起窗权在途（`in_flight`）⇒ 放弃
///    暂缓（占位保持，诚实回执 AbandonDeferred）；执行已收口 ⇒ 清 pending（幂
///    等，零出手，结算 env 文件）；
/// ④ opencode 首请求：G2 三核对（目录 → 预备内端点回环 + 登记进程身份）→
///    **出手前原子占位**（HD：三核对通过后、spawner 出手前，单锁 entry 按会话
///    取得恢复权——桌面命令与远程端点同函数消费，跨通道并发恰一窗）→ 出手
///    （明确未交付 ⇒ 占位释放可重试；交付未知 ⇒ 占位保持转待确认）→ 观察窗
///    轮询（确认核销 / 耗尽保持待确认，零重复开窗）。迟到观察只核对（五要素
///    窗口/会话/目录/服务代际/serve 连接就绪），不自动尝试第二个窗口。
pub(crate) fn restore_with_pending_in(
    session: &Session,
    deps: &RestoreDeps<'_>,
    abandon: bool,
    registry: &PendingRegistry,
    observe_window: Duration,
    observe_poll: Duration,
    now_ms: i64,
) -> Result<RestoreFlow, String> {
    let sid = session.id.trim().to_string();
    if session.agent_type.tool_id() != "opencode" {
        let prepared = (deps.prepare)(session)?;
        (deps.execute)(&prepared, session, deps.spawner, None)?;
        return Ok(RestoreFlow::Dispatched { confirmed: true });
    }
    // ② 显式放弃（IC fix round 1 · P1-B 用户裁决）：**准备或交付仍在执行时不得
    //    直接释放起窗权**——交付链（osascript）无取消语义，执行方无法确认取消 ⇒
    //    占位保持 + 诚实回执 [`RestoreFlow::AbandonDeferred`]（修复前：放弃直接
    //    删占位，原请求仍可继续交付、新请求也能取得占位 ⇒ 两窗；交付回调还会把
    //    旧请求占位重插回空位）。执行已收口（`in_flight = false`：待确认滞留 /
    //    待观察）⇒ 既有「放弃 = 按会话结算出清」语义保持（幂等，零出手；HC：结算
    //    即删交付未知留存的受控入口 env 文件）。
    if abandon {
        // J1：占位身份判定 + in_flight 检查 + 删除合并**同一临界区**——修复前
        // 「get 克隆（锁一）→ 释放 → remove(&sid)（锁二）」间隙可被「旧占位核销
        // → 新请求占位」插入，旧放弃盲删新占位。entry 单锁后该序列在删除前不可
        // 表示；settle 只结算本临界区捕获的占位凭据文件（token 唯一文件名，不触
        // 碰新请求文件）。
        let settled = {
            let mut guard = registry
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            match guard.entry(sid.clone()) {
                std::collections::hash_map::Entry::Occupied(entry) => {
                    if entry.get().in_flight {
                        return Ok(RestoreFlow::AbandonDeferred);
                    }
                    Some(entry.remove())
                }
                std::collections::hash_map::Entry::Vacant(_) => None,
            }
        };
        // J1 竞窗注入缝（仅测试）：单临界区之后触发——「旧占位核销 → 新请求占位」
        // 只能发生在原子删除之后，新占位不再暴露给旧放弃的删除面。按会话限定。
        #[cfg(test)]
        test_between_abandon_locks_fire(&sid);
        if let Some(pending) = settled {
            settle_env_secret_file(&pending.env_secret_path);
        }
        return Ok(RestoreFlow::Abandoned);
    }
    // ③ 复请求/已占快路径（零预备零出手）：占位在场 → 先复查观察——命中即核销，
    //    未命中即拒绝（重复恢复请求不再开第二个窗口；HD：占位失败复用 PendingRefused
    //    同族码，既有「复请求先复查观察」行为保持）
    let existing = registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&sid)
        .cloned();
    if let Some(pending) = existing {
        return Ok(recheck_matching_pending(&pending, deps, registry));
    }
    // ④ 首请求：G2 目录核对（目标会话存在且属规范目录）→ 预备 → 出手（出手前
    //    原子占位）→ 观察窗轮询
    //
    // G2 三核对之「目标会话属规范目录」：project_path realpath 化 + is_dir，失败
    // ⇒ 结构化 Err **零起窗零 pending**（目录已不在 ⇒ `cd` 落点不可证，宁拒不猜）。
    let expected_directory = match std::fs::canonicalize(session.project_path.trim()) {
        Ok(dir) if dir.is_dir() => dir,
        _ => {
            return Err(format!(
                "{}: 目标会话目录不可解析（{}）——目录不存在或不是规范目录，拒绝起窗；\
                 请在主机核对项目目录后重试",
                super::resume::OC_RESUME_DIRECTORY_UNRESOLVED,
                session.project_path.trim()
            ));
        }
    };
    let prepared = (deps.prepare)(session)?;
    // HD（H4）登记面来源经注入缝：状态文件 + 占位时刻服务代际锚（与绑定行
    // service_id/runtime_pid/runtime_started_at_ms 同源同形）。
    let (state_file, generation) = (deps.registration_source)();
    // HD（H4）出手前原子占位：三核对（目录 + 端点回环 + 登记进程身份）通过后、
    // spawner 出手前，按会话**原子取得恢复权**（单锁 entry——同一会话同时至多一个
    // 占位；桌面命令与远程端点同函数消费，跨通道并发恰一窗）。已被占（并发败者）
    // ⇒ 转复查观察分诊（零出手、零第二窗）。
    //
    // GR1b HD-1：entry 判定与败者持有者读取**并入同一临界区**——修复前是两个独立
    // 锁段（entry 判 Occupied → 释放锁 → 重 get），胜者可在其间确定性失败快速返回
    // 并释放占位 → 败者 `get` 得 None → `expect("…必然在场")` panic（Tauri 命令
    // 线程 = 用户可感崩溃面）。合并后「占位已消失」态不可表示（同锁内判定+克隆），
    // 败者恒得在场快照转复查分诊——快照后占位才被胜者释放的场景，观察面按当前
    // 状态判定自然不命中 → PendingRefused（请求时点确有在途恢复的诚实回执，
    // 复请求经 ③ 自愈），全程无 panic、无误重建、零出手。
    // I3 起窗前窗口基线（出手前一次观察快照，存入占位）：核销要求确认窗口
    // ∉ 基线——旧窗口（起窗前已在）不得提前核销新恢复。快照在占位插入前采集
    //（仍先于出手；快照成本 = 一次进程扫描，见 production_baseline_snapshot 申报）。
    let baseline = (deps.baseline_snapshot)(&prepared.effect_sig);
    let pending = RestorePending {
        effect_sig: prepared.effect_sig.clone(),
        state_file: state_file.clone(),
        expected_directory: Some(expected_directory),
        session_id: sid.clone(),
        created_at_ms: now_ms,
        env_secret_path: prepared.env_secret.as_ref().map(|s| s.path.clone()),
        generation,
        created_target: deps.created_target.clone(),
        // I3：占位取得即准备中态（spawner 未出手——观察器不得核销）；请求唯一
        // 标识同核生成（resume.rs unique_request_token——HC EnvSecret 文件名
        // token 同族，不建第二套标识）。
        state: RestorePendingState::Preparing,
        request_token: super::resume::unique_request_token(),
        baseline,
        // P1-B：占位取得即起窗权在途（流转线程从出手到观察窗收口全程持权）——
        // 在途期间放弃不得直接释放（见 ② 分支）。
        in_flight: true,
    };
    let (_acquired, loser_holder) = {
        let mut reg = registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match reg.entry(sid.clone()) {
            std::collections::hash_map::Entry::Occupied(entry) => {
                (false, Some(entry.get().clone()))
            }
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(pending.clone());
                (true, None)
            }
        }
    };
    if let Some(holder) = loser_holder {
        return Ok(recheck_matching_pending(&holder, deps, registry));
    }
    // G5 交付回调保留：Some(hook) 是 open_macos_with「交付后回查未命中不转次选」
    // 的开关（不二窗）；占位已先于出手在场。I3：交付回调 = **状态推进触发点**
    //（交付信号成立 ⇒ 准备中 → 已交付，核销资格自此开启）；同 token 原子推进，
    // 占位已属其他请求（token 不匹配）时零触碰（不覆盖、不误插）。
    // IC fix round 1（P1-B）：**空位分支重插路径消除**——HD 起占位先于出手在场
    //（hook 只承担 Preparing → Delivered 推进，不再承担 G5 时代的插入职责）；
    // hook 触发时占位不在场 = 占位已先于本次交付消亡（起窗权已移出本请求）⇒
    // **零触碰不重插**（修复前 Vacant 分支会把旧请求占位插回空位，让已被放弃/
    // 释放的请求重新占用起窗权）。
    let hook_sid = sid.clone();
    let mut hook_pending = pending.clone();
    hook_pending.state = RestorePendingState::Delivered;
    let hook = move || {
        let mut reg = registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let std::collections::hash_map::Entry::Occupied(mut entry) = reg.entry(hook_sid.clone())
        {
            if entry.get().request_token == hook_pending.request_token {
                entry.insert(hook_pending.clone());
            }
            // token 不匹配：占位已属后续请求——交付回调零触碰（不覆盖、不误插）
        }
        // 空位：占位已先于交付消亡——旧请求不重插（零触碰）
    };
    // HD（H4）释放规则：**明确未交付**（确定性失败——请求未达终端）⇒ 占位释放
    //（I3：按本次请求 token 比对释放——占位已属后续请求时零触碰），重试合法
    //（可再取得占位再开窗）；**交付未知**（osascript 超时/等待失败——Apple 事件
    // 可能已送达）⇒ 占位保持 + 状态推进「待观察」（出手已发生、观察未决——迟到
    // 观察可按同一核销条件收口），不释放、不二窗、不冒充成功。HC env 文件处置由
    // 交付层同一分类收口（RetainForSettle 留存至结算）。
    if let Err(e) = (deps.execute)(&prepared, session, deps.spawner, Some(&hook)) {
        if (deps.delivery_unknown)(&e) {
            let mut reg = registry
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(p) = reg.get_mut(&sid) {
                if p.request_token == pending.request_token {
                    p.state = RestorePendingState::AwaitingObservation;
                    // P1-B：流转线程就此收口（出手已发生、结果未知）——起窗权
                    // 在途标记清零，此后放弃恢复既有结算语义
                    p.in_flight = false;
                }
            }
            return Ok(RestoreFlow::Dispatched { confirmed: false });
        }
        remove_if_token(registry, &sid, &pending.request_token);
        return Err(e);
    }
    // 观察窗：确认才收口；耗尽 → 待确认态保留（占位保持，零重复开窗，不冒充成功）。
    // I3 核销条件（全满足才核销）：① 本请求占位仍在场（token 比对——迟到结果
    // 只核销自己的占位）+ 状态非准备中（交付信号成立过）；② 候选窗口**∉ 起窗前
    // 基线**（旧窗口不得提前核销新恢复）。
    let deadline = std::time::Instant::now() + observe_window;
    loop {
        let candidates = (deps.observer)(&pending);
        if let Some(new_window) = pick_new_window(candidates, &pending.baseline) {
            if deps
                .on_confirmed
                .is_some_and(|confirmed| !confirmed(new_window.pid))
            {
                if std::time::Instant::now() < deadline {
                    std::thread::sleep(observe_poll);
                    continue;
                }
            } else {
                let redeemed = remove_if_token(registry, &sid, &pending.request_token).is_some();
                if !redeemed {
                    // 占位已属后续请求（token 不匹配）：本次请求的核销权已随占位移交
                    // ——不删除、不冒充确认（后续请求的复查按其 token/基线自行收口）
                    return Ok(RestoreFlow::Dispatched { confirmed: false });
                }
                // HC：观察确认 = 结算，交付未知留存的 env 文件随之删除（窗口已
                // source+rm 自删的路径此处幂等 no-op）
                settle_env_secret_file(&pending.env_secret_path);
                return Ok(RestoreFlow::Dispatched { confirmed: true });
            }
        }
        if std::time::Instant::now() >= deadline {
            // P1-B：观察窗耗尽，流转线程就此收口（待确认态滞留）——起窗权在途
            // 标记清零（按 token 比对：占位已属后续请求时零触碰），此后放弃恢复
            // 既有「按会话结算出清」语义
            let mut reg = registry
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(p) = reg.get_mut(&sid) {
                if p.request_token == pending.request_token {
                    p.in_flight = false;
                }
            }
            return Ok(RestoreFlow::Dispatched { confirmed: false });
        }
        std::thread::sleep(observe_poll);
    }
}

/// I3 基线比对（窗口身份 = pid+cmd 全等；全等才算「基线内旧窗」，防 pid 复用
/// 误判——cmd 变了就是新进程新窗口）。
fn window_in_baseline(w: &ObservedWindow, baseline: &[ObservedWindow]) -> bool {
    baseline.iter().any(|b| b.pid == w.pid && b.cmd == w.cmd)
}

/// I3 候选遴选：候选集中第一扇**不在基线内**的窗口（基线内旧窗全部忽略——旧
/// 窗口不得提前核销新恢复）；无合格候选 = None（未就绪）。
fn pick_new_window(
    candidates: Vec<ObservedWindow>,
    baseline: &[ObservedWindow],
) -> Option<ObservedWindow> {
    candidates
        .into_iter()
        .find(|w| !window_in_baseline(w, baseline))
}

/// I3 token 守卫删除：仅当注册表现行条目仍是 `token` 所属请求的占位时才删除
///（返回被删条目）；token 不匹配 = 占位已属后续请求——零触碰。核销/释放/状态
/// 推进的统一身份闸（迟到结果只核销自己那条请求的占位）。
fn remove_if_token(registry: &PendingRegistry, sid: &str, token: &str) -> Option<RestorePending> {
    let mut reg = registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match reg.get(sid) {
        Some(p) if p.request_token == token => reg.remove(sid),
        _ => None,
    }
}

/// 复查观察分诊（复请求/并发败者共用；零出手）。I3 核销资格三闸：
/// ① **状态闸**——占位准备中（spawner 未出手）⇒ 直接拒绝（不消费观察器：并发
///    核对不得在首请求出手前清占位）；
/// ② **基线闸**——候选窗口必须 ∉ 起窗前基线（旧窗口不得提前核销新恢复）；
/// ③ **身份闸**——remove_if_token（复查与删除之间占位被后续请求取代时零触碰）。
/// 三闸全过 → 核销（结算 env 文件）→ PendingConfirmed；任一不过 → PendingRefused。
fn recheck_matching_pending(
    pending: &RestorePending,
    deps: &RestoreDeps<'_>,
    registry: &PendingRegistry,
) -> RestoreFlow {
    if let Some(expected) = &deps.created_target {
        if pending.created_target.as_ref() != Some(expected)
            || pending.generation != (deps.registration_source)().1
        {
            return RestoreFlow::PendingRefused;
        }
    }
    recheck_settle(pending, deps, registry)
}

fn recheck_settle(
    pending: &RestorePending,
    deps: &RestoreDeps<'_>,
    registry: &PendingRegistry,
) -> RestoreFlow {
    if !pending.state.redeemable() {
        return RestoreFlow::PendingRefused;
    }
    let candidates = (deps.observer)(pending);
    if let Some(window) = pick_new_window(candidates, &pending.baseline) {
        if deps
            .on_confirmed
            .is_some_and(|confirmed| !confirmed(window.pid))
        {
            return RestoreFlow::PendingRefused;
        }
        if remove_if_token(registry, &pending.session_id, &pending.request_token).is_some() {
            settle_env_secret_file(&pending.env_secret_path);
            return RestoreFlow::PendingConfirmed;
        }
    }
    RestoreFlow::PendingRefused
}

/// 观察窗结算（HC 交付未知生命周期）：pending 流转收口（确认核销 / 显式放弃）
/// 时 best-effort 删除本轮交付的受控入口 env 文件——交付未知留存的文件自此收口；
/// 窗口已 source+rm 自删的路径此处幂等 no-op。删除失败不阻塞流转（文件仍有 0600
/// 保护 + 24h 清扫兜底）。
fn settle_env_secret_file(env_secret_path: &Option<PathBuf>) {
    if let Some(p) = env_secret_path {
        let _ = std::fs::remove_file(p);
    }
}

/// 生产装配（endpoint 与桌面命令共用）：全局注册表 + 生产观察 + 生产登记面来源。
/// 并发纪律（HD）：注册表锁为短临界区；按会话互斥由 ④ 的**单锁 entry 原子占位**
/// 保证（同一会话同时至多一个占位——并发复请求在「已占」分诊面排队复查，交付后
/// 语义与 G5 时序一致）。
pub(crate) fn open_restore_with_pending(
    session: &Session,
    spawner: &SpawnFn,
    abandon: bool,
) -> Result<RestoreFlow, String> {
    open_restore_with_service(session, spawner, abandon, None, None)
}

/// 历史会话只允许附着已核验的原生目标，沿用恢复占位和本地观察。
pub(crate) fn open_verified_opencode(
    session: &Session,
    spawner: &SpawnFn,
    abandon: bool,
    hub: &crate::services::opencode_native::NativeHub,
    service: &ServiceDescriptor,
    directory: &CreateDirectory,
) -> Result<RestoreFlow, String> {
    let confirmed = |pid| {
        crate::monitor::opencode_observation::fresh_opened_window_for_pid(&session.id, pid)
            .is_some()
    };
    open_restore_with_service(
        session,
        spawner,
        abandon,
        Some((hub, service, directory)),
        Some(&confirmed),
    )
}

/// 起窗前已核验的创建目标失效，不能切换到候选终端或服务继续交付。
pub(crate) fn created_target_error_code(error: &str) -> Option<&str> {
    use crate::services::opencode_native::types::codes;
    let (code, _) = error.split_once(": ")?;
    match code {
        codes::OC_DIRECTORY_MISMATCH
        | codes::OC_GENERATION_MISMATCH
        | codes::OC_INSTANCE_AMBIGUOUS
        | codes::OC_SERVICE_NOT_REGISTERED
        | codes::OC_SERVICE_REGISTRATION_INVALID
        | codes::OC_ENDPOINT_INVALID
        | codes::OC_ENDPOINT_NOT_LOOPBACK
        | codes::OC_RUNTIME_UNVERIFIED => Some(code),
        _ => None,
    }
}

/// 为已创建会话起窗。使用创建时的代际和端点，出手前再次确认登记。
pub(crate) fn open_created_opencode(
    session: &Session,
    spawner: &SpawnFn,
    hub: &crate::services::opencode_native::NativeHub,
    service: &ServiceDescriptor,
    directory: &CreateDirectory,
) -> Result<
    (
        RestoreFlow,
        Option<crate::services::opencode_native::binding_gate::LockedWindow>,
    ),
    String,
> {
    let captured = Mutex::new(None);
    let confirmed = |pid| {
        let window =
            crate::monitor::opencode_observation::fresh_opened_window_for_pid(&session.id, pid);
        let confirmed = window.is_some();
        *captured.lock().unwrap_or_else(|err| err.into_inner()) = window;
        confirmed
    };
    let result = open_restore_with_service(
        session,
        spawner,
        false,
        Some((hub, service, directory)),
        Some(&confirmed),
    )?;
    Ok((
        result,
        captured.into_inner().unwrap_or_else(|err| err.into_inner()),
    ))
}

/// 结果可能已交付时只观察。进程重启丢失内存占位后也不再次起窗。
pub(crate) fn recheck_created_opencode(
    session: &Session,
    hub: &crate::services::opencode_native::NativeHub,
    service: &ServiceDescriptor,
    directory: &CreateDirectory,
) -> (
    RestoreFlow,
    Option<crate::services::opencode_native::binding_gate::LockedWindow>,
) {
    let observer = observer_for(&session.id);
    let pending = PENDINGS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&session.id)
        .cloned();
    if let Some(pending) = pending {
        let matches = pending.generation.as_ref() == Some(&service.runtime)
            && pending
                .created_target
                .as_ref()
                .is_some_and(|target| target.directory == *directory && target.matches(service));
        if matches {
            if let Some(window) = pending
                .state
                .redeemable()
                .then(|| {
                    observer(&pending)
                        .into_iter()
                        .filter(|candidate| !window_in_baseline(candidate, &pending.baseline))
                        .find_map(|candidate| {
                            created_window_identity(
                                session,
                                hub,
                                service,
                                directory,
                                Some(candidate.pid),
                            )
                        })
                })
                .flatten()
                .filter(|_| {
                    remove_if_token(&PENDINGS, &pending.session_id, &pending.request_token)
                        .is_some()
                })
            {
                settle_env_secret_file(&pending.env_secret_path);
                return (RestoreFlow::PendingConfirmed, Some(window));
            }
            return (RestoreFlow::PendingRefused, None);
        }
        if pending.generation.as_ref() == Some(&service.runtime) || pending.in_flight {
            return (RestoreFlow::PendingRefused, None);
        }
    }
    // 内存占位丢失时，没有证据能证明已有窗口来自本次交付。
    (RestoreFlow::PendingRefused, None)
}

pub(crate) fn created_window_identity(
    session: &Session,
    hub: &crate::services::opencode_native::NativeHub,
    service: &ServiceDescriptor,
    directory: &CreateDirectory,
    expected_pid: Option<u32>,
) -> Option<crate::services::opencode_native::binding_gate::LockedWindow> {
    #[cfg(target_os = "macos")]
    {
        use crate::monitor::opencode_observation::{
            fresh_opened_window, fresh_opened_window_for_pid,
        };
        let window = match expected_pid {
            Some(pid) => fresh_opened_window_for_pid(&session.id, pid),
            None => fresh_opened_window(&session.id),
        }?;
        let peer = serve_host_port(&service.endpoint)?;
        if production_tcp_pair(window.pid, service.runtime.pid, &peer)
            && directory.still_current()
            && hub
                .verify_create_generation(Path::new(&directory.path), service)
                .is_ok()
            && fresh_opened_window_for_pid(&session.id, window.pid).as_ref() == Some(&window)
        {
            return Some(window);
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (session, hub, service, directory, expected_pid);
    None
}

fn open_restore_with_service(
    session: &Session,
    spawner: &SpawnFn,
    abandon: bool,
    created: Option<(
        &crate::services::opencode_native::NativeHub,
        &ServiceDescriptor,
        &CreateDirectory,
    )>,
    on_confirmed: Option<&(dyn Fn(u32) -> bool + Sync)>,
) -> Result<RestoreFlow, String> {
    let observer = observer_for(session.id.trim());
    let budget = observe_budget();
    let verify_created = |session: &Session,
                          hub: &crate::services::opencode_native::NativeHub,
                          original: &ServiceDescriptor,
                          directory: &CreateDirectory|
     -> Result<String, String> {
        let digest = hub
            .verify_create_generation(Path::new(&session.project_path), original)
            .map_err(|err| format!("{}: {}", err.code, err.message))?;
        if directory.path != session.project_path || !directory.still_current() {
            return Err("OC_DIRECTORY_MISMATCH: 已创建会话的目标目录发生变化".into());
        }
        Ok(digest)
    };
    let deps = RestoreDeps {
        prepare: &|session| match created {
            Some((hub, service, directory)) => {
                let digest = verify_created(session, hub, service, directory)?;
                super::resume::prepare_created_opencode_terminal(
                    session,
                    service,
                    directory,
                    hub.registration_file(),
                    &digest,
                )
            }
            None => prepare_session_terminal(session),
        },
        execute: &|prepared, session, spawner, on_delivered| {
            let guarded = |spec: &super::resume::SpawnSpec| {
                if let Some((hub, service, directory)) = created {
                    verify_created(session, hub, service, directory)?;
                }
                spawner(spec)
            };
            super::resume::execute_prepared_spawn_with_delivery(
                prepared,
                session.project_path.trim(),
                &guarded,
                on_delivered,
            )
        },
        spawner,
        observer: observer.as_ref(),
        baseline_snapshot: &production_baseline_snapshot,
        created_target: created.map(|(_, service, directory)| CreatedRestoreTarget {
            directory: directory.clone(),
            endpoint: service.endpoint.clone(),
            version: service.version.clone(),
            credential_hash: Sha256::digest(service.password.as_bytes()).into(),
        }),
        on_confirmed,
        registration_source: &|| match created {
            Some((hub, service, _)) => (
                Some(hub.registration_file().to_owned()),
                Some(service.runtime.clone()),
            ),
            None => production_registration_source(),
        },
        delivery_unknown: &super::resume::is_delivery_unknown_error,
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0);
    restore_with_pending_in(
        session,
        &deps,
        abandon,
        &PENDINGS,
        budget,
        RESTORE_OBSERVE_POLL,
        now_ms,
    )
}

fn observer_for(sid: &str) -> std::sync::Arc<RestoreObserver> {
    #[cfg(test)]
    {
        let overrides = OBSERVER_OVERRIDES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(o) = overrides.get(sid).cloned() {
            return o;
        }
    }
    #[cfg(not(test))]
    {
        let _ = sid;
    }
    std::sync::Arc::new(production_restore_observer)
}

#[cfg(test)]
fn observe_budget() -> Duration {
    OBSERVE_BUDGET_OVERRIDE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .unwrap_or(RESTORE_OBSERVE_WINDOW)
}

#[cfg(not(test))]
fn observe_budget() -> Duration {
    RESTORE_OBSERVE_WINDOW
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{AgentType, ProcessForm, SessionStatus};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc as StdArc;

    /// 最小 opencode 会话夹具（resume.rs sess 同形）。G2 起 cwd 必须**真实存在**
    /// （起窗前目录核对 realpath 化）——夹具用 tempdir，项目目录零伪造。
    fn oc_sess(id: &str, cwd: &std::path::Path) -> Session {
        Session {
            dsh_instance_id: None,
            dsh_subagent: None,
            id: id.into(),
            agent_type: AgentType::OpenCode,
            project_name: "proj".into(),
            project_path: cwd.to_string_lossy().to_string(),
            title: None,
            git_branch: None,
            github_url: None,
            status: SessionStatus::Waiting,
            last_message: None,
            last_message_role: None,
            last_message_subagent_report: false,
            flap_from_subagent_activity: false,
            last_activity_at: "2026-11-17T00:00:00Z".into(),
            pid: 7,
            cpu_usage: 0.0,
            active_subagent_count: 0,
            form: ProcessForm::Cli,
            jump_supported: false,
            unread: false,
            unread_turned_green_at_ms: None,
        }
    }

    /// 记录型夹具：prepare 恒 Ok（特征含 sid 唯一）；execute 计数（= 出手/开窗
    /// 次数）并**模拟交付语义**——交付回调在场即调（生产 execute 的 G5 行为：
    /// osascript/spawn Ok 即回调）。
    struct Rig {
        dispatches: StdArc<AtomicUsize>,
    }

    impl Rig {
        fn new() -> Self {
            Self {
                dispatches: StdArc::new(AtomicUsize::new(0)),
            }
        }

        /// 观察恒 false（窗口观察未就绪）。
        fn deps_never_ready(&self) -> RestoreDeps<'static> {
            // 借用安全：闭包不捕获环境引用（除 dispatches 计数，用 Arc 值捕获）。
            let dispatches = self.dispatches.clone();
            let prepare = move |s: &Session| {
                Ok(super::super::resume::PreparedSpawn {
                    macos_payload: format!("'bin' --session {}", s.id),
                    windows_payload: format!("bin --session {}", s.id),
                    effect_sig: format!("bin --session {}", s.id),
                    env_secret: None,
                })
            };
            let execute = move |p: &super::super::resume::PreparedSpawn,
                                _s: &Session,
                                _sp: &SpawnFn,
                                on_delivered: Option<&DeliveryHook<'_>>|
                  -> Result<(), String> {
                dispatches.fetch_add(1, Ordering::SeqCst);
                // G5 生产语义：交付成功即回调（模拟 osascript 返回）
                if let Some(hook) = on_delivered {
                    hook();
                }
                let _ = p;
                Ok(())
            };
            let observer = |_p: &RestorePending| Vec::new();
            RestoreDeps {
                prepare: leak_box(prepare),
                execute: leak_box(execute),
                spawner: leak_box(|_spec: &super::super::resume::SpawnSpec| Ok(())),
                observer: leak_box(observer),
                baseline_snapshot: leak_box(|_sig: &str| Vec::new()),
                created_target: None,
                on_confirmed: None,
                registration_source: leak_box(|| (None, None)),
                delivery_unknown: leak_box(|e: &str| {
                    crate::inject::resume::is_delivery_unknown_error(e)
                }),
            }
        }

        /// 观察恒 true（真实窗口观察就绪）。
        fn deps_always_ready(&self) -> RestoreDeps<'static> {
            let dispatches = self.dispatches.clone();
            let prepare = move |s: &Session| {
                Ok(super::super::resume::PreparedSpawn {
                    macos_payload: format!("'bin' --session {}", s.id),
                    windows_payload: format!("bin --session {}", s.id),
                    effect_sig: format!("bin --session {}", s.id),
                    env_secret: None,
                })
            };
            let execute = move |_p: &super::super::resume::PreparedSpawn,
                                _s: &Session,
                                _sp: &SpawnFn,
                                on_delivered: Option<&DeliveryHook<'_>>|
                  -> Result<(), String> {
                dispatches.fetch_add(1, Ordering::SeqCst);
                if let Some(hook) = on_delivered {
                    hook();
                }
                Ok(())
            };
            let observer = |p: &RestorePending| {
                vec![ObservedWindow {
                    pid: 424_242,
                    cmd: p.effect_sig.clone(),
                }]
            };
            RestoreDeps {
                prepare: leak_box(prepare),
                execute: leak_box(execute),
                spawner: leak_box(|_spec: &super::super::resume::SpawnSpec| Ok(())),
                observer: leak_box(observer),
                baseline_snapshot: leak_box(|_sig: &str| Vec::new()),
                created_target: None,
                on_confirmed: None,
                registration_source: leak_box(|| (None, None)),
                delivery_unknown: leak_box(|e: &str| {
                    crate::inject::resume::is_delivery_unknown_error(e)
                }),
            }
        }

        /// 交付失败形态（osascript Err / spawn Err，请求未达终端）：execute 计数
        /// 但**不调交付回调**（零交付零 pending——既有 failed-retry 语义）。
        fn deps_deliver_fails(&self) -> RestoreDeps<'static> {
            let dispatches = self.dispatches.clone();
            let prepare = move |s: &Session| {
                Ok(super::super::resume::PreparedSpawn {
                    macos_payload: format!("'bin' --session {}", s.id),
                    windows_payload: format!("bin --session {}", s.id),
                    effect_sig: format!("bin --session {}", s.id),
                    env_secret: None,
                })
            };
            let execute = move |_p: &super::super::resume::PreparedSpawn,
                                _s: &Session,
                                _sp: &SpawnFn,
                                _on_delivered: Option<&DeliveryHook<'_>>|
                  -> Result<(), String> {
                dispatches.fetch_add(1, Ordering::SeqCst);
                Err("osascript 执行失败（退出码 1）".to_string())
            };
            let observer = |_p: &RestorePending| Vec::new();
            RestoreDeps {
                prepare: leak_box(prepare),
                execute: leak_box(execute),
                spawner: leak_box(|_spec: &super::super::resume::SpawnSpec| Ok(())),
                observer: leak_box(observer),
                baseline_snapshot: leak_box(|_sig: &str| Vec::new()),
                created_target: None,
                on_confirmed: None,
                registration_source: leak_box(|| (None, None)),
                delivery_unknown: leak_box(|e: &str| {
                    crate::inject::resume::is_delivery_unknown_error(e)
                }),
            }
        }
    }

    /// 闭包升级为 'static 引用（测试夹具专用：进程生命周期泄漏，量级可忽略）。
    fn leak_box<T: 'static>(value: T) -> &'static T {
        Box::leak(Box::new(value))
    }

    fn empty_registry() -> PendingRegistry {
        PendingRegistry::new(HashMap::new())
    }

    /// J1（用户复审测试修整）：作用域结束自动移除本会话停点（defer 守卫）。
    fn defer_remove_between_abandon_locks_pause(sid: &'static str) -> impl Drop {
        struct Guard(&'static str);
        impl Drop for Guard {
            fn drop(&mut self) {
                remove_between_abandon_locks_pause(self.0);
            }
        }
        Guard(sid)
    }

    fn registry_has(registry: &PendingRegistry, sid: &str) -> bool {
        registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains_key(sid)
    }

    const WINDOW: Duration = Duration::from_millis(30);
    const POLL: Duration = Duration::from_millis(5);

    /// RED a（brief 4A.4①）：pending 未核销时第二次恢复请求被拒——**不再开窗**
    ///（出手/开窗次数恰 1，第二次返回 PendingRefused）。
    #[test]
    fn second_request_while_pending_unsettled_is_refused_without_second_window() {
        let rig = Rig::new();
        let registry = empty_registry();
        let home = tempfile::tempdir().unwrap();
        let s = oc_sess("ses_f4_r1", home.path());
        let first = restore_with_pending_in(
            &s,
            &rig.deps_never_ready(),
            false,
            &registry,
            WINDOW,
            POLL,
            1_000,
        )
        .expect("首请求出手成功");
        assert_eq!(
            first,
            RestoreFlow::Dispatched { confirmed: false },
            "观察未就绪：出手但不能冒充成功（待确认态保留）"
        );
        let second = restore_with_pending_in(
            &s,
            &rig.deps_never_ready(),
            false,
            &registry,
            WINDOW,
            POLL,
            2_000,
        )
        .expect("复请求是受控拒绝，不是错误");
        assert_eq!(
            second,
            RestoreFlow::PendingRefused,
            "pending 未核销：第二次恢复请求必须被拒（不再开窗）"
        );
        assert_eq!(
            rig.dispatches.load(Ordering::SeqCst),
            1,
            "出手恰一次——绝不自动再开第二个窗口"
        );
    }

    /// RED b（brief 4A.4②）：观察匹配 → 确认流转。首请求观察窗内确认；
    /// 复请求复查命中 → PendingConfirmed 核销（零出手）；核销后 pending 清空。
    #[test]
    fn observation_match_confirms_and_settles() {
        let rig = Rig::new();
        let registry = empty_registry();
        let home = tempfile::tempdir().unwrap();
        let s = oc_sess("ses_f4_r2", home.path());
        let first = restore_with_pending_in(
            &s,
            &rig.deps_always_ready(),
            false,
            &registry,
            WINDOW,
            POLL,
            1_000,
        )
        .expect("首请求出手成功");
        assert_eq!(
            first,
            RestoreFlow::Dispatched { confirmed: true },
            "观察匹配 → 回执成功（确认流转）"
        );
        assert!(
            !registry_has(&registry, "ses_f4_r2"),
            "确认后 pending 必须核销清空"
        );
        // 复请求：pending 已核销 → 走既有链（允许再次出手，如用户主动再开）
        let second = restore_with_pending_in(
            &s,
            &rig.deps_always_ready(),
            false,
            &registry,
            WINDOW,
            POLL,
            2_000,
        )
        .expect("核销后复请求走既有链");
        assert_eq!(second, RestoreFlow::Dispatched { confirmed: true });
        assert_eq!(rig.dispatches.load(Ordering::SeqCst), 2);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn created_restore_cannot_settle_confirmable_generic_pending() {
        let rig = Rig::new();
        let registry = empty_registry();
        let project = tempfile::Builder::new()
            .prefix("restore-created-request-")
            .tempdir_in(env!("CARGO_MANIFEST_DIR"))
            .unwrap();
        let directory =
            super::super::create_path::validate_existing(project.path().to_str().unwrap()).unwrap();
        let session = oc_sess("ses_created_with_generic_pending", project.path());
        let owner_token = "generic-restore-owner";
        registry.lock().unwrap().insert(
            session.id.clone(),
            RestorePending {
                session_id: session.id.clone(),
                effect_sig: format!("bin --session {}", session.id),
                state_file: None,
                expected_directory: Some(PathBuf::from(&directory.path)),
                created_at_ms: 1_000,
                env_secret_path: None,
                generation: None,
                created_target: None,
                state: RestorePendingState::Delivered,
                request_token: owner_token.into(),
                baseline: Vec::new(),
                in_flight: false,
            },
        );
        let mut deps = rig.deps_always_ready();
        deps.created_target = Some(CreatedRestoreTarget {
            directory,
            endpoint: reqwest::Url::parse("http://127.0.0.1:40123").unwrap(),
            version: "2.2.5".into(),
            credential_hash: Sha256::digest(b"created-restore-fixture").into(),
        });
        deps.spawner = &|_| panic!("新建恢复请求被拒时不得调用 spawner");

        let flow = restore_with_pending_in(&session, &deps, false, &registry, WINDOW, POLL, 2_000)
            .expect("已有一般恢复占位时，新建恢复请求应受控拒绝");

        assert_eq!(flow, RestoreFlow::PendingRefused);
        let guard = registry.lock().unwrap();
        let pending = guard.get(&session.id).expect("一般恢复占位必须保留");
        assert_eq!(pending.request_token, owner_token);
        assert!(pending.created_target.is_none());
        assert_eq!(rig.dispatches.load(Ordering::SeqCst), 0);
    }

    /// RED c（brief 4A.4②复请求面）：pending 未核销时复请求先复查观察——此刻
    /// 观察就绪（窗口真的开了）→ PendingConfirmed 核销，**零出手**（不开新窗）。
    /// G5 迟到观察语义：观察翻转只发生核对流转，绝不补开第二窗。
    #[test]
    fn recheck_observation_confirms_pending_without_new_window() {
        let rig = Rig::new();
        let registry = empty_registry();
        let home = tempfile::tempdir().unwrap();
        let s = oc_sess("ses_f4_r3", home.path());
        // 首请求观察未就绪（待确认态保留）
        let first = restore_with_pending_in(
            &s,
            &rig.deps_never_ready(),
            false,
            &registry,
            WINDOW,
            POLL,
            1_000,
        )
        .expect("首请求出手成功");
        assert_eq!(first, RestoreFlow::Dispatched { confirmed: false });
        assert!(registry_has(&registry, "ses_f4_r3"), "待确认态必须保留");
        // 复请求：观察此刻就绪（真实窗口已开）→ 核销，零出手
        let second = restore_with_pending_in(
            &s,
            &rig.deps_always_ready(),
            false,
            &registry,
            WINDOW,
            POLL,
            2_000,
        )
        .expect("复查命中 → 核销");
        assert_eq!(
            second,
            RestoreFlow::PendingConfirmed,
            "复查观察命中 → 确认流转（零出手）"
        );
        assert_eq!(
            rig.dispatches.load(Ordering::SeqCst),
            1,
            "复查核销零出手——窗口观察确认不重复开窗"
        );
        assert!(!registry_has(&registry, "ses_f4_r3"), "核销后 pending 清空");
    }

    /// RED d（brief 4A.4③）：观察失败 → 保持待确认不假成功——pending 保留、
    /// 出手面不冒充成功；用户显式放弃 → Abandoned 清 pending，零出手。
    #[test]
    fn observation_failure_keeps_pending_and_abandon_clears_it() {
        let rig = Rig::new();
        let registry = empty_registry();
        let home = tempfile::tempdir().unwrap();
        let s = oc_sess("ses_f4_r4", home.path());
        let first = restore_with_pending_in(
            &s,
            &rig.deps_never_ready(),
            false,
            &registry,
            WINDOW,
            POLL,
            1_000,
        )
        .expect("首请求出手成功");
        assert_eq!(
            first,
            RestoreFlow::Dispatched { confirmed: false },
            "观察失败不得冒充成功"
        );
        assert!(registry_has(&registry, "ses_f4_r4"), "待确认态保留");
        // 显式放弃：清 pending、零出手
        let abandoned = restore_with_pending_in(
            &s,
            &rig.deps_never_ready(),
            true,
            &registry,
            WINDOW,
            POLL,
            2_000,
        )
        .expect("放弃是受控流转");
        assert_eq!(abandoned, RestoreFlow::Abandoned, "显式放弃 → Abandoned");
        assert!(!registry_has(&registry, "ses_f4_r4"), "放弃后 pending 清空");
        assert_eq!(rig.dispatches.load(Ordering::SeqCst), 1, "放弃路径零出手");
        // 放弃幂等（无 pending 再放弃 → 同形，零出手）
        let again = restore_with_pending_in(
            &s,
            &rig.deps_never_ready(),
            true,
            &registry,
            WINDOW,
            POLL,
            3_000,
        )
        .expect("幂等放弃");
        assert_eq!(again, RestoreFlow::Abandoned);
        assert_eq!(rig.dispatches.load(Ordering::SeqCst), 1);
    }

    /// 非 opencode 工具零行为变更：无 pending 面，出手成功即 confirmed（既有语义）。
    #[test]
    fn non_opencode_keeps_existing_behavior_without_pending() {
        let rig = Rig::new();
        let registry = empty_registry();
        let home = tempfile::tempdir().unwrap();
        let mut s = oc_sess("ses_f4_r5", home.path());
        s.agent_type = AgentType::Claude;
        let flow = restore_with_pending_in(
            &s,
            &rig.deps_never_ready(),
            false,
            &registry,
            WINDOW,
            POLL,
            1_000,
        )
        .expect("非 opencode 既有链");
        assert_eq!(
            flow,
            RestoreFlow::Dispatched { confirmed: true },
            "非 opencode：出手成功即回执成功（零行为变更）"
        );
        assert!(
            !registry_has(&registry, "ses_f4_r5"),
            "非 opencode 无 pending 面"
        );
        // 复请求同样直接出手（无 pending 门）
        restore_with_pending_in(
            &s,
            &rig.deps_never_ready(),
            false,
            &registry,
            WINDOW,
            POLL,
            2_000,
        )
        .expect("复请求既有链");
        assert_eq!(rig.dispatches.load(Ordering::SeqCst), 2);
    }

    // ==== G5：交付即插 pending（慢起窗不二窗）====

    /// RED d（brief GB d）：慢观察注入——交付成功后观察窗内未命中 ⇒ pending 已在
    /// 场（第二次请求被拒），无第二窗（出手恰一次）。夹具的 execute 模拟生产交付
    /// 语义（交付回调在 execute 内部触发——osascript 返回即插），观察恒未命中。
    #[test]
    fn delivered_but_unobserved_keeps_pending_and_refuses_second_dispatch() {
        let rig = Rig::new();
        let registry = empty_registry();
        let home = tempfile::tempdir().unwrap();
        let s = oc_sess("ses_gb_d1", home.path());
        let first = restore_with_pending_in(
            &s,
            &rig.deps_never_ready(),
            false,
            &registry,
            WINDOW,
            POLL,
            1_000,
        )
        .expect("交付成功");
        assert_eq!(
            first,
            RestoreFlow::Dispatched { confirmed: false },
            "交付成功但观察未命中：待确认态（不假成功）"
        );
        assert!(
            registry_has(&registry, "ses_gb_d1"),
            "交付即插 pending：观察窗开始前 pending 必须在场"
        );
        let pending = registry
            .lock()
            .unwrap()
            .get("ses_gb_d1")
            .cloned()
            .expect("pending 在场");
        assert_eq!(pending.session_id, "ses_gb_d1", "pending 绑定目标会话");
        assert!(
            pending.expected_directory.is_some(),
            "pending 必须携带目录面锚（G5 四要素）"
        );
        // 第二次请求：pending 在场 → 拒绝（零出手）
        let second = restore_with_pending_in(
            &s,
            &rig.deps_never_ready(),
            false,
            &registry,
            WINDOW,
            POLL,
            2_000,
        )
        .expect("复请求是受控拒绝");
        assert_eq!(
            second,
            RestoreFlow::PendingRefused,
            "交付后未核销 pending：第二次请求必须被拒（无第二窗）"
        );
        assert_eq!(
            rig.dispatches.load(Ordering::SeqCst),
            1,
            "出手恰一次——交付后观察未中绝不补开第二窗"
        );
    }

    /// G5 失败面（零交付）：execute Err（osascript/spawn 失败，请求未达终端）⇒
    /// 零 pending（交付回调未触发）——既有 failed-retry 语义保持（复请求允许
    /// 重新出手；F4 报告 §4.4 口径：FAILED 回执即 fail-closed）。
    #[test]
    fn delivery_failure_keeps_no_pending_and_allows_retry() {
        let rig = Rig::new();
        let registry = empty_registry();
        let home = tempfile::tempdir().unwrap();
        let s = oc_sess("ses_gb_d2", home.path());
        let first = restore_with_pending_in(
            &s,
            &rig.deps_deliver_fails(),
            false,
            &registry,
            WINDOW,
            POLL,
            1_000,
        );
        assert!(first.is_err(), "交付失败 = Err 回执（不假成功）");
        assert!(
            !registry_has(&registry, "ses_gb_d2"),
            "零交付零 pending：交付失败不得插 pending"
        );
        // 复请求：走既有链（failed-retry，允许重新出手）
        let second = restore_with_pending_in(
            &s,
            &rig.deps_deliver_fails(),
            false,
            &registry,
            WINDOW,
            POLL,
            2_000,
        );
        assert!(second.is_err(), "复请求仍失败（夹具恒败）——既有语义");
        assert_eq!(rig.dispatches.load(Ordering::SeqCst), 2, "复请求重新出手");
    }

    // ==== G2：起窗前三核对（目标会话属规范目录）====

    /// RED b（brief GB b）目录面：会话目录不存在（canonicalize 失败）⇒ 零起窗
    /// 零 pending——结构化哨兵 Err，execute/spawner 不被调用。
    #[test]
    fn unresolved_directory_refuses_dispatch_without_pending() {
        let rig = Rig::new();
        let registry = empty_registry();
        let home = tempfile::tempdir().unwrap();
        let gone = home.path().join("no-such-project-dir");
        let s = oc_sess("ses_gb_b1", &gone);
        let r = restore_with_pending_in(
            &s,
            &rig.deps_never_ready(),
            false,
            &registry,
            WINDOW,
            POLL,
            1_000,
        );
        let err = r.expect_err("目录不可解析必须拒绝");
        assert!(
            err.starts_with(crate::inject::resume::OC_RESUME_DIRECTORY_UNRESOLVED),
            "结构化哨兵（端点映射 404）：{err}"
        );
        assert_eq!(
            rig.dispatches.load(Ordering::SeqCst),
            0,
            "零起窗：目录核对失败不出手"
        );
        assert!(
            !registry_has(&registry, "ses_gb_b1"),
            "零 pending：目录核对失败不插待确认态"
        );
    }

    /// RED e（brief GB e）：迟到观察命中 ⇒ 核对流转成功（核销零出手）；迟到不
    /// 命中 ⇒ 保持待确认。两分支在同registry上顺序验证。
    #[test]
    fn late_observation_hit_settles_miss_keeps_pending() {
        let rig = Rig::new();
        let registry = empty_registry();
        let home = tempfile::tempdir().unwrap();
        let s = oc_sess("ses_gb_e1", home.path());
        // 交付 + 观察未命中 → 待确认
        let first = restore_with_pending_in(
            &s,
            &rig.deps_never_ready(),
            false,
            &registry,
            WINDOW,
            POLL,
            1_000,
        )
        .expect("交付成功");
        assert_eq!(first, RestoreFlow::Dispatched { confirmed: false });
        // 迟到观察仍未命中 → 保持待确认（拒绝复请求，零出手）
        let miss = restore_with_pending_in(
            &s,
            &rig.deps_never_ready(),
            false,
            &registry,
            WINDOW,
            POLL,
            2_000,
        )
        .expect("复请求受控拒绝");
        assert_eq!(miss, RestoreFlow::PendingRefused);
        // 迟到观察命中（观察翻转）→ PendingConfirmed 核销，零出手
        let hit = restore_with_pending_in(
            &s,
            &rig.deps_always_ready(),
            false,
            &registry,
            WINDOW,
            POLL,
            3_000,
        )
        .expect("迟到命中 → 核销");
        assert_eq!(
            hit,
            RestoreFlow::PendingConfirmed,
            "迟到观察命中：核对流转成功（不重开窗）"
        );
        assert_eq!(rig.dispatches.load(Ordering::SeqCst), 1, "迟到核对零出手");
        assert!(!registry_has(&registry, "ses_gb_e1"), "核销后 pending 清空");
    }

    // ==== HC（H3）：交付未知生命周期——观察窗结算删凭据文件 ====

    /// HC 结算夹具：prepare 携带受控入口秘密（tempdir 内路径，假凭据）；execute
    /// 模拟生产交付语义（交付回调在场即调）；观察就绪与否由 ready 参数定。
    #[cfg(unix)]
    fn hc_deps_with_env_secret(
        secret_path: std::path::PathBuf,
        ready: bool,
        dispatches: &StdArc<AtomicUsize>,
    ) -> RestoreDeps<'static> {
        let dispatches = dispatches.clone();
        let prepare = move |s: &Session| {
            Ok(super::super::resume::PreparedSpawn {
                macos_payload: format!("'bin' --session {}", s.id),
                windows_payload: format!("bin --session {}", s.id),
                effect_sig: format!("bin --session {}", s.id),
                env_secret: Some(super::super::resume::EnvSecret {
                    path: secret_path.clone(),
                    password: "HCpw".to_string(),
                }),
            })
        };
        let execute = move |_p: &super::super::resume::PreparedSpawn,
                            _s: &Session,
                            _sp: &SpawnFn,
                            on_delivered: Option<&DeliveryHook<'_>>|
              -> Result<(), String> {
            dispatches.fetch_add(1, Ordering::SeqCst);
            if let Some(hook) = on_delivered {
                hook();
            }
            Ok(())
        };
        let observer = move |_p: &RestorePending| {
            if ready {
                vec![win(424_243, "hc --session settled")]
            } else {
                Vec::new()
            }
        };
        RestoreDeps {
            prepare: leak_box(prepare),
            execute: leak_box(execute),
            spawner: leak_box(|_spec: &super::super::resume::SpawnSpec| Ok(())),
            observer: leak_box(observer),
            baseline_snapshot: leak_box(|_sig: &str| Vec::new()),
            created_target: None,
            on_confirmed: None,
            registration_source: leak_box(|| (None, None)),
            delivery_unknown: leak_box(|e: &str| {
                crate::inject::resume::is_delivery_unknown_error(e)
            }),
        }
    }

    /// RED d-2（brief HC d）：交付未知 ⇒ env 文件按申报生命周期收口——观察窗
    /// 结算（复查命中核销 / 观察窗内确认 / 用户显式放弃）即删；PendingRefused
    /// （未核销）留存（窗口可能迟 source，不冒进删）；清扫器 24h 兜底（最坏留存
    /// 上界）。留存期文件保持 0600（不留权限过宽文件）。
    #[cfg(unix)]
    #[test]
    fn hc_delivery_unknown_env_file_settles_on_confirm_and_abandon() {
        use std::os::unix::fs::PermissionsExt;
        let mk_file = |path: &std::path::Path| {
            std::fs::write(path, "export OPENCODE_PASSWORD='HCpw'\n").unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        };
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let dispatches: StdArc<AtomicUsize> = StdArc::new(AtomicUsize::new(0));
        let registry = empty_registry();
        // 轮 1：交付未知（观察未命中）→ 留存 + 0600 → 复核未命中（PendingRefused）
        //   仍留存 → 迟到命中（PendingConfirmed）→ 结算即删
        let path1 = dir.path().join("resume-env-s1.sh");
        mk_file(&path1);
        let s1 = oc_sess("ses_hc_d1", home.path());
        let first = restore_with_pending_in(
            &s1,
            &hc_deps_with_env_secret(path1.clone(), false, &dispatches),
            false,
            &registry,
            WINDOW,
            POLL,
            1_000,
        )
        .expect("首请求交付成功");
        assert_eq!(
            first,
            RestoreFlow::Dispatched { confirmed: false },
            "观察未命中：交付未知（不假成功）"
        );
        {
            let reg = registry.lock().unwrap();
            let pending = reg.get("ses_hc_d1").expect("pending 在场");
            assert_eq!(
                pending.env_secret_path,
                Some(path1.clone()),
                "pending 必须携带受控入口路径（结算钩子锚）"
            );
        }
        assert!(path1.exists(), "交付未知：文件留存至观察窗结算");
        assert_eq!(
            std::fs::metadata(&path1).unwrap().permissions().mode() & 0o777,
            0o600,
            "留存期文件必须保持 0600（不留权限过宽文件）"
        );
        let second = restore_with_pending_in(
            &s1,
            &hc_deps_with_env_secret(path1.clone(), false, &dispatches),
            false,
            &registry,
            WINDOW,
            POLL,
            2_000,
        )
        .expect("复请求受控拒绝");
        assert_eq!(second, RestoreFlow::PendingRefused, "未核销：复请求被拒");
        assert!(
            path1.exists(),
            "未核销：留存（窗口可能迟 source，不冒进删；24h 清扫兜底）"
        );
        let third = restore_with_pending_in(
            &s1,
            &hc_deps_with_env_secret(path1.clone(), true, &dispatches),
            false,
            &registry,
            WINDOW,
            POLL,
            3_000,
        )
        .expect("迟到命中 → 核销");
        assert_eq!(third, RestoreFlow::PendingConfirmed, "复查命中：核对流转");
        assert!(!path1.exists(), "结算核销（复查命中）→ env 文件即删");
        // 轮 2：交付未知 → 用户显式放弃 → 结算即删
        let path2 = dir.path().join("resume-env-s2.sh");
        mk_file(&path2);
        let s2 = oc_sess("ses_hc_d2", home.path());
        let first2 = restore_with_pending_in(
            &s2,
            &hc_deps_with_env_secret(path2.clone(), false, &dispatches),
            false,
            &registry,
            WINDOW,
            POLL,
            4_000,
        )
        .expect("轮 2 交付成功");
        assert_eq!(first2, RestoreFlow::Dispatched { confirmed: false });
        assert!(path2.exists(), "轮 2 交付未知：留存");
        let ab = restore_with_pending_in(
            &s2,
            &hc_deps_with_env_secret(path2.clone(), false, &dispatches),
            true,
            &registry,
            WINDOW,
            POLL,
            5_000,
        )
        .expect("显式放弃是受控流转");
        assert_eq!(ab, RestoreFlow::Abandoned, "显式放弃：清 pending");
        assert!(!path2.exists(), "显式放弃 = 结算 → env 文件即删");
        // 轮 3：观察窗内确认（首请求即就绪）→ 结算即删（rm 自删未遂时的幂等兜底
        // ——窗口被用户提前关闭时载荷 rm 不会跑，此处收口）
        let path3 = dir.path().join("resume-env-s3.sh");
        mk_file(&path3);
        let s3 = oc_sess("ses_hc_d3", home.path());
        let first3 = restore_with_pending_in(
            &s3,
            &hc_deps_with_env_secret(path3.clone(), true, &dispatches),
            false,
            &registry,
            WINDOW,
            POLL,
            6_000,
        )
        .expect("轮 3 交付 + 观察确认");
        assert_eq!(
            first3,
            RestoreFlow::Dispatched { confirmed: true },
            "观察窗内确认"
        );
        assert!(!path3.exists(), "确认结算 → env 文件即删（幂等兜底）");
        // 轮 4：清扫器 24h 兜底（崩溃等未结算残留的最坏留存上界）
        let path4 = dir.path().join("resume-env-s4.sh");
        mk_file(&path4);
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(25 * 3600);
        {
            let f = std::fs::File::options().write(true).open(&path4).unwrap();
            f.set_times(std::fs::FileTimes::new().set_modified(old))
                .unwrap();
        }
        super::super::resume::sweep_stale_resume_env_files(dir.path());
        assert!(!path4.exists(), "24h 清扫兜底：超窗残留即删");
        assert_eq!(
            std::fs::read_dir(dir.path())
                .map(|rd| rd.filter_map(|e| e.ok()).count())
                .unwrap_or(usize::MAX),
            0,
            "目录零残留"
        );
        assert_eq!(
            dispatches.load(Ordering::SeqCst),
            3,
            "结算流转零额外出手（三轮各出手一次，复请求/放弃/确认零出手）"
        );
    }

    // ==== HD（H4）：一键恢复出手前原子占位（桌面+远程共用按会话恢复权）====

    /// HD 超时回执文案（生产 `run_osascript_wait` 同前缀形态；下方漂移守卫断言
    /// 其必被生产分类器判为交付未知）。
    const HD_TIMEOUT_ERR: &str = "osascript 执行超时（10s 上限，已终止）——请重试";

    /// HD 夹具的交付行为。
    #[derive(Clone, Copy)]
    enum HdDelivery {
        /// 交付成功（执行计数 + 记录载荷 + 交付回调在场即调——生产 G5 语义）。
        Delivered,
        /// 确定性失败（请求未达终端——退出码/双通道皆败形态）。
        DefiniteFail,
        /// 交付未知（osascript 超时形态——Apple 事件可能已送达）。
        UnknownTimeout,
    }

    /// HD 可拼装夹具：共享计数/载荷记录/观察开关（并发/迟到场景可运行时翻转），
    /// registration_source 可注入代际锚（默认自托管形态 (None, None)）。
    fn hd_deps(
        dispatches: &StdArc<AtomicUsize>,
        payloads: &StdArc<std::sync::Mutex<Vec<String>>>,
        ready: &StdArc<std::sync::atomic::AtomicBool>,
        delivery: HdDelivery,
        generation: Option<RuntimeKey>,
    ) -> RestoreDeps<'static> {
        let dispatches = dispatches.clone();
        let payloads = payloads.clone();
        let ready = ready.clone();
        let prepare = move |s: &Session| {
            Ok(super::super::resume::PreparedSpawn {
                macos_payload: format!("'bin' --session {}", s.id),
                windows_payload: format!("bin --session {}", s.id),
                effect_sig: format!("bin --session {}", s.id),
                env_secret: None,
            })
        };
        let execute = move |p: &super::super::resume::PreparedSpawn,
                            _s: &Session,
                            _sp: &SpawnFn,
                            on_delivered: Option<&DeliveryHook<'_>>|
              -> Result<(), String> {
            dispatches.fetch_add(1, Ordering::SeqCst);
            payloads.lock().unwrap().push(p.effect_sig.clone());
            match delivery {
                HdDelivery::Delivered => {
                    if let Some(hook) = on_delivered {
                        hook();
                    }
                    Ok(())
                }
                HdDelivery::DefiniteFail => Err("osascript 执行失败（退出码 1）".to_string()),
                HdDelivery::UnknownTimeout => Err(HD_TIMEOUT_ERR.to_string()),
            }
        };
        let observer = move |p: &RestorePending| {
            if ready.load(Ordering::SeqCst) {
                vec![ObservedWindow {
                    pid: 424_244,
                    cmd: p.effect_sig.clone(),
                }]
            } else {
                Vec::new()
            }
        };
        RestoreDeps {
            prepare: leak_box(prepare),
            execute: leak_box(execute),
            spawner: leak_box(|_spec: &super::super::resume::SpawnSpec| Ok(())),
            observer: leak_box(observer),
            // I3 基线缝：HD 既有测试不涉基线语义——恒空集（零真实扫描）。
            baseline_snapshot: leak_box(|_sig: &str| Vec::new()),
            created_target: None,
            on_confirmed: None,
            registration_source: leak_box(move || {
                (
                    Some(std::path::PathBuf::from("/hd-test-state-file")),
                    generation.clone(),
                )
            }),
            delivery_unknown: leak_box(|e: &str| {
                crate::inject::resume::is_delivery_unknown_error(e)
            }),
        }
    }

    /// 漂移守卫：HD 超时文案必被生产分类器判为「交付未知」（与 HC 交付处置同源
    /// 判据——文案漂移即此处红）。
    #[test]
    fn hd_timeout_error_literal_classified_as_delivery_unknown() {
        assert!(
            crate::inject::resume::is_delivery_unknown_error(HD_TIMEOUT_ERR),
            "超时文案漂移：{HD_TIMEOUT_ERR}"
        );
        assert!(
            crate::inject::resume::is_delivery_unknown_error(
                "osascript 执行超时（Apple Event -1712）"
            ),
            "Apple Event -1712 应归为交付未知"
        );
        assert!(
            !crate::inject::resume::is_delivery_unknown_error("osascript 执行失败（退出码 1）"),
            "确定性失败不得误判为交付未知"
        );
    }

    /// 并发双通道夹具产出（流转结果 + 出手计数 + 载荷记录；type_complexity 收敛别名）。
    type HdConcurrentPair = (
        Vec<Result<RestoreFlow, String>>,
        StdArc<AtomicUsize>,
        StdArc<std::sync::Mutex<Vec<String>>>,
    );

    /// 并发双通道夹具（RED a / RED e 共用）：两个「通道」（桌面命令 / 远程端点的
    /// 状态机同形调用）在 prepare 内 Barrier 同步到同一竞争点后并发争取得占位。
    fn run_hd_concurrent_pair(sid: &str, home: &std::path::Path) -> HdConcurrentPair {
        let registry = empty_registry();
        let dispatches = StdArc::new(AtomicUsize::new(0));
        let payloads: StdArc<std::sync::Mutex<Vec<String>>> =
            StdArc::new(std::sync::Mutex::new(Vec::new()));
        let barrier = StdArc::new(std::sync::Barrier::new(2));
        let sessions: Vec<Session> = [(0u64, 1_000u64), (1, 2_000)]
            .iter()
            .map(|_| oc_sess(sid, home))
            .collect();
        let registry_ref = &registry;
        let results: Vec<Result<RestoreFlow, String>> = std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for s in sessions {
                let barrier = barrier.clone();
                let dispatches = dispatches.clone();
                let payloads = payloads.clone();
                handles.push(scope.spawn(move || {
                    // 每通道自建 deps（闭包在各自线程内构造；共享 Arc 计数/barrier）
                    let prepare = move |sess: &Session| {
                        barrier.wait();
                        Ok(super::super::resume::PreparedSpawn {
                            macos_payload: format!("'bin' --session {}", sess.id),
                            windows_payload: format!("bin --session {}", sess.id),
                            effect_sig: format!("bin --session {}", sess.id),
                            env_secret: None,
                        })
                    };
                    let dispatches2 = dispatches.clone();
                    let payloads2 = payloads.clone();
                    let execute = move |p: &super::super::resume::PreparedSpawn,
                                        _s: &Session,
                                        _sp: &SpawnFn,
                                        on_delivered: Option<&DeliveryHook<'_>>|
                          -> Result<(), String> {
                        dispatches2.fetch_add(1, Ordering::SeqCst);
                        payloads2.lock().unwrap().push(p.effect_sig.clone());
                        if let Some(hook) = on_delivered {
                            hook();
                        }
                        Ok(())
                    };
                    let ready = StdArc::new(std::sync::atomic::AtomicBool::new(false));
                    let ready2 = ready.clone();
                    let observer = move |p: &RestorePending| {
                        if ready2.load(Ordering::SeqCst) {
                            vec![ObservedWindow {
                                pid: 424_245,
                                cmd: p.effect_sig.clone(),
                            }]
                        } else {
                            Vec::new()
                        }
                    };
                    let deps = RestoreDeps {
                        prepare: leak_box(prepare),
                        execute: leak_box(execute),
                        spawner: leak_box(|_spec: &super::super::resume::SpawnSpec| Ok(())),
                        observer: leak_box(observer),
                        baseline_snapshot: leak_box(|_sig: &str| Vec::new()),
                        created_target: None,
                        on_confirmed: None,
                        registration_source: leak_box(|| (None, None)),
                        delivery_unknown: leak_box(|e: &str| {
                            crate::inject::resume::is_delivery_unknown_error(e)
                        }),
                    };
                    restore_with_pending_in(&s, &deps, false, registry_ref, WINDOW, POLL, 1_000)
                }));
            }
            handles
                .into_iter()
                .map(|h| h.join().expect("并发通道线程不 panic"))
                .collect()
        });
        (results, dispatches.clone(), payloads.clone())
    }

    /// RED a（brief HD a）：桌面与浏览器**并发**恢复同一会话——占位原子取得保证
    /// 恰一窗：一个通道出手（待确认），另一个被占位拒绝（PendingRefused 同族），
    /// 零第二次 osascript（出手/载荷恰一次）。
    #[test]
    fn hd_concurrent_desktop_and_browser_restore_exactly_one_window() {
        let home = tempfile::tempdir().unwrap();
        let (results, dispatches, payloads) = run_hd_concurrent_pair("ses_hd_a1", home.path());
        let flows: Vec<RestoreFlow> = results
            .into_iter()
            .map(|r| r.expect("并发恢复各自是受控流转，不是错误"))
            .collect();
        assert_eq!(
            flows
                .iter()
                .filter(|f| **f == RestoreFlow::PendingRefused)
                .count(),
            1,
            "并发败者必须被占位拒绝（PendingRefused 同族）：{flows:?}"
        );
        assert!(
            flows.contains(&RestoreFlow::Dispatched { confirmed: false }),
            "并发胜者出手（观察未就绪 → 待确认）：{flows:?}"
        );
        assert_eq!(
            dispatches.load(Ordering::SeqCst),
            1,
            "出手恰一次——并发双通道绝不产生第二个 osascript"
        );
        assert_eq!(payloads.lock().unwrap().len(), 1, "起窗载荷恰一次");
    }

    /// RED b（brief HD b）：AppleEvent 已交付但回执超时 ⇒ 占位保持 → 转待确认
    /// 只核对（复请求复查未命中即拒）；零第二窗。
    #[test]
    fn hd_delivery_unknown_timeout_keeps_placeholder_pending_only() {
        let dispatches = StdArc::new(AtomicUsize::new(0));
        let payloads: StdArc<std::sync::Mutex<Vec<String>>> =
            StdArc::new(std::sync::Mutex::new(Vec::new()));
        let ready = StdArc::new(std::sync::atomic::AtomicBool::new(false));
        let registry = empty_registry();
        let home = tempfile::tempdir().unwrap();
        let s = oc_sess("ses_hd_b1", home.path());
        let deps = hd_deps(
            &dispatches,
            &payloads,
            &ready,
            HdDelivery::UnknownTimeout,
            None,
        );
        let first = restore_with_pending_in(&s, &deps, false, &registry, WINDOW, POLL, 1_000)
            .expect("交付未知是受控流转（转待确认），不是错误");
        assert_eq!(
            first,
            RestoreFlow::Dispatched { confirmed: false },
            "回执超时：转待确认（不冒充成功、不当作确定性失败释放占位）"
        );
        assert!(registry_has(&registry, "ses_hd_b1"), "交付未知：占位保持");
        let second = restore_with_pending_in(&s, &deps, false, &registry, WINDOW, POLL, 2_000)
            .expect("复请求是受控拒绝");
        assert_eq!(
            second,
            RestoreFlow::PendingRefused,
            "交付未知后复请求只允许核对：复查未命中即拒"
        );
        assert_eq!(
            dispatches.load(Ordering::SeqCst),
            1,
            "零第二窗：超时后复请求绝不出手"
        );
        assert!(registry_has(&registry, "ses_hd_b1"), "占位未释放");
        // I3 状态三分收口：交付未知占位 = 待观察态（出手已发生、观察未决）——
        // 迟到观察命中可按同一核销条件收口（核销资格 = 非「准备中」形态全表）。
        // 先验态标记，再翻转观察命中 → 复查核销（零额外出手）。
        {
            let reg = registry.lock().unwrap();
            let p = reg.get("ses_hd_b1").expect("待观察占位在场");
            assert_eq!(
                p.state,
                RestorePendingState::AwaitingObservation,
                "交付未知 ⇒ 待观察态（出手已发生、观察未决）"
            );
        }
        ready.store(true, Ordering::SeqCst);
        let third = restore_with_pending_in(&s, &deps, false, &registry, WINDOW, POLL, 3_000)
            .expect("待观察占位迟到命中是受控流转");
        assert_eq!(
            third,
            RestoreFlow::PendingConfirmed,
            "待观察形态可核销：迟到观察命中按同一核销条件收口"
        );
        assert!(!registry_has(&registry, "ses_hd_b1"), "核销后注册表清空");
        assert_eq!(
            dispatches.load(Ordering::SeqCst),
            1,
            "复查核销零额外出手（零第二窗保持）"
        );
    }

    /// RED c（brief HD c）：迟到窗口先后到达（两个候选窗口先后出现）——只核对
    /// **第一个匹配四要素的**（复查命中即核销），零出手；其后第二个候选出现系统
    /// 零动作（不自动开新窗）。占位必须携带四要素（含 HD 服务代际锚）。
    #[test]
    fn hd_late_windows_first_four_factor_match_settles_without_new_window() {
        let dispatches = StdArc::new(AtomicUsize::new(0));
        let payloads: StdArc<std::sync::Mutex<Vec<String>>> =
            StdArc::new(std::sync::Mutex::new(Vec::new()));
        let ready = StdArc::new(std::sync::atomic::AtomicBool::new(false));
        let registry = empty_registry();
        let home = tempfile::tempdir().unwrap();
        let s = oc_sess("ses_hd_c1", home.path());
        let anchor = RuntimeKey {
            service_id: "svc_hd_c1".to_string(),
            pid: 4321,
            started_at_ms: 777_000,
        };
        let deps = hd_deps(
            &dispatches,
            &payloads,
            &ready,
            HdDelivery::Delivered,
            Some(anchor),
        );
        let first = restore_with_pending_in(&s, &deps, false, &registry, WINDOW, POLL, 1_000)
            .expect("首请求交付成功");
        assert_eq!(
            first,
            RestoreFlow::Dispatched { confirmed: false },
            "候选窗口未出现：待确认"
        );
        {
            let reg = registry.lock().unwrap();
            let pending = reg.get("ses_hd_c1").expect("占位在场");
            assert_eq!(
                pending.generation.as_ref().map(|g| g.service_id.as_str()),
                Some("svc_hd_c1"),
                "占位必须携带服务代际锚（确认四要素）"
            );
        }
        // 迟到候选窗口 1 出现（观察翻转 = 第一个匹配四要素的候选）→ 复查命中核销
        ready.store(true, Ordering::SeqCst);
        let second = restore_with_pending_in(&s, &deps, false, &registry, WINDOW, POLL, 2_000)
            .expect("迟到命中 → 核销");
        assert_eq!(
            second,
            RestoreFlow::PendingConfirmed,
            "只核对第一个匹配四要素的候选窗口（零出手核销）"
        );
        assert!(!registry_has(&registry, "ses_hd_c1"), "核销后占位清空");
        // 候选窗口 2 随后出现（观察仍 true）——系统零动作：无请求不出手
        assert_eq!(
            dispatches.load(Ordering::SeqCst),
            1,
            "迟到窗口只核对不重开：出手恒为首次那一窗"
        );
    }

    /// RED d（brief HD d）：明确未交付（起窗确认失败——确定性错误）⇒ 占位释放
    /// → 重试合法（可再取得占位并再开窗）。
    #[test]
    fn hd_definite_failure_releases_placeholder_retry_allowed() {
        let dispatches = StdArc::new(AtomicUsize::new(0));
        let payloads: StdArc<std::sync::Mutex<Vec<String>>> =
            StdArc::new(std::sync::Mutex::new(Vec::new()));
        let ready = StdArc::new(std::sync::atomic::AtomicBool::new(false));
        let registry = empty_registry();
        let home = tempfile::tempdir().unwrap();
        let s = oc_sess("ses_hd_d1", home.path());
        let deps = hd_deps(
            &dispatches,
            &payloads,
            &ready,
            HdDelivery::DefiniteFail,
            None,
        );
        let first = restore_with_pending_in(&s, &deps, false, &registry, WINDOW, POLL, 1_000);
        assert!(first.is_err(), "明确未交付 = Err 回执（不假成功）");
        assert!(
            !registry_has(&registry, "ses_hd_d1"),
            "明确未交付：占位必须释放（不得滞留拒绝合法重试）"
        );
        // 重试合法：占位已释放 → 再取得 → 再出手（再开窗允许）
        let second = restore_with_pending_in(&s, &deps, false, &registry, WINDOW, POLL, 2_000);
        assert!(second.is_err(), "夹具恒败：复请求仍失败（既有语义）");
        assert_eq!(
            dispatches.load(Ordering::SeqCst),
            2,
            "占位释放后重试合法——第二次出手未被滞留占位拒绝"
        );
        assert!(!registry_has(&registry, "ses_hd_d1"), "失败后零占位残留");
    }

    /// RED e（brief HD e）：恢复侧对「恢复后首条原生消息恰好入账一次」的贡献面
    /// = 出手恰一次 + 载荷零消息注入（resume 零注入，载荷 = 纯 resume 命令）；
    /// 首句入账由交付入口原生 send 恰一次承担（resume.rs live M7 断言 + actions
    /// 既有 outbox-once 测试在案）。并发双通道下载荷仍恰一次且不含消息文本。
    #[test]
    fn hd_restore_zero_message_injection_single_dispatch_accounting() {
        let home = tempfile::tempdir().unwrap();
        let (_results, dispatches, payloads) = run_hd_concurrent_pair("ses_hd_e1", home.path());
        assert_eq!(
            dispatches.load(Ordering::SeqCst),
            1,
            "出手恰一次（恢复链对消息账本的唯一合法贡献面）"
        );
        let recorded = payloads.lock().unwrap().clone();
        assert_eq!(
            recorded,
            vec!["bin --session ses_hd_e1".to_string()],
            "载荷 = 纯 resume 命令（零消息注入）：{recorded:?}"
        );
    }

    /// HD 确认四要素之「服务代际」面（登记面核对纯函数）：现行登记代际与占位锚
    /// 全等才可核实——service_id / pid / started_at_ms 任一漂移（serve 换代）即拒
    /// 绝核实（旧代际残迹不冒充新状态）；登记不可读 / 锚缺席 fail-closed 拒绝。
    #[test]
    fn hd_generation_anchor_four_factor_registration_face() {
        use crate::services::opencode_native::types::DiscoveryInput;
        let dir = tempfile::tempdir().unwrap();
        let state_file = dir.path().join("state.json");
        let input = DiscoveryInput {
            state_file: state_file.clone(),
            home: dir.path().to_path_buf(),
        };
        let anchor = RuntimeKey {
            service_id: "svc-1".to_string(),
            pid: 100,
            started_at_ms: 5_000,
        };
        let write_reg = |id: &str, pid: u32| {
            std::fs::write(
                &state_file,
                format!(
                    r#"{{"id":"{id}","version":"2.2.5","url":"http://127.0.0.1:40123","pid":{pid},"password":"pw"}}"#
                ),
            )
            .unwrap();
        };
        let read_ok = |path: &std::path::Path| {
            std::fs::read(path).map_err(|e| {
                crate::services::opencode_native::types::NativeError::new(
                    "OC_IO",
                    &format!("unreadable: {e}"),
                )
            })
        };
        let fact_at = |want: i64| move |pid: u32| if pid == 100 { Some(want) } else { None };
        // ① 全等：可核实
        write_reg("svc-1", 100);
        assert!(
            registration_descriptor_with(&input, Some(&anchor), &read_ok, &fact_at(5_000))
                .is_some(),
            "代际全等（id/pid/lstart 一致）必须可核实"
        );
        // ② 换代：pid 漂移 → 拒绝
        write_reg("svc-1", 200);
        assert!(
            !registration_descriptor_with(&input, Some(&anchor), &read_ok, &fact_at(5_000))
                .is_some(),
            "登记 pid 漂移 = 换代（pid 复用/重启），拒绝核实"
        );
        // ③ 换代：启动时间漂移 → 拒绝
        write_reg("svc-1", 100);
        assert!(
            !registration_descriptor_with(&input, Some(&anchor), &read_ok, &fact_at(6_000))
                .is_some(),
            "进程启动事实漂移 = 新代际，拒绝核实"
        );
        // ④ 换代：service_id 漂移 → 拒绝
        write_reg("svc-2", 100);
        assert!(
            !registration_descriptor_with(&input, Some(&anchor), &read_ok, &fact_at(5_000))
                .is_some(),
            "登记 service_id 漂移 = 换代，拒绝核实"
        );
        // ⑤ 登记不可读 → fail-closed 拒绝
        std::fs::remove_file(&state_file).unwrap();
        assert!(
            !registration_descriptor_with(&input, Some(&anchor), &read_ok, &fact_at(5_000))
                .is_some(),
            "登记缺失不可核验 → fail-closed"
        );
        // ⑥ 锚缺席（占位时刻登记面不可核验）→ 永不确认（不冒充）
        write_reg("svc-1", 100);
        assert!(
            !registration_descriptor_with(&input, None, &read_ok, &fact_at(5_000)).is_some(),
            "锚缺席 = 登记代际从未可证 → fail-closed 永不确认"
        );
    }

    // ==== GR1b HD-1：并发败者竞窗（胜者确定性失败释放占位）====

    /// 构造「并发败者」时序的依赖束（注入缝控制时序，零真实线程竞速）。
    ///
    /// * prepare 缝 = 时序注入 1——并发胜者在败者 ③ 判空之后、④ entry 之前
    ///   **原子取得占位**（生产单锁 entry 的跨通道并发形态）；
    /// * observer 缝 = 时序注入 2（首次调用）——胜者 execute 确定性失败快速返回
    ///   并**释放占位**，落点在败者已克隆持有者快照之后（修复前正是此时序使败者
    ///   重 get 得 None → `expect` panic）。
    ///
    /// 占位注册表 leak 为 'static 供闭包共享（与流转主体同一实例）。
    fn loser_race_deps<'a>(
        registry: &'static PendingRegistry,
        dispatches: StdArc<AtomicUsize>,
        observer_calls: StdArc<AtomicUsize>,
        release_on_first_observe: bool,
    ) -> RestoreDeps<'a> {
        let prepare = move |s: &Session| {
            let placeholder = RestorePending {
                session_id: s.id.clone(),
                effect_sig: format!("winner --session {}", s.id),
                state_file: None,
                expected_directory: None,
                created_at_ms: 0,
                env_secret_path: None,
                generation: None,
                created_target: None,
                state: RestorePendingState::Delivered,
                request_token: "gr1b-holder-token".to_string(),
                baseline: Vec::new(),
                // 时序注入假体：无真实流转线程持权（胜者由 prepare 模拟）——非在途
                in_flight: false,
            };
            registry
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .entry(s.id.clone())
                .or_insert(placeholder);
            Ok(super::super::resume::PreparedSpawn {
                macos_payload: format!("'bin' --session {}", s.id),
                windows_payload: format!("bin --session {}", s.id),
                effect_sig: format!("bin --session {}", s.id),
                env_secret: None,
            })
        };
        let execute = move |_p: &super::super::resume::PreparedSpawn,
                            _s: &Session,
                            _sp: &SpawnFn,
                            _on_delivered: Option<&DeliveryHook<'_>>|
              -> Result<(), String> {
            dispatches.fetch_add(1, Ordering::SeqCst);
            Ok(())
        };
        let observer = move |p: &RestorePending| {
            let n = observer_calls.fetch_add(1, Ordering::SeqCst);
            if release_on_first_observe && n == 0 {
                // 胜者确定性失败释放占位（零交付 ⇒ 当前无窗口，诚实核对面）
                registry
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&p.session_id);
            }
            Vec::<ObservedWindow>::new()
        };
        RestoreDeps {
            prepare: leak_box(prepare),
            execute: leak_box(execute),
            spawner: leak_box(|_spec: &super::super::resume::SpawnSpec| Ok(())),
            observer: leak_box(observer),
            baseline_snapshot: leak_box(|_sig: &str| Vec::new()),
            created_target: None,
            on_confirmed: None,
            registration_source: leak_box(|| (None, None)),
            delivery_unknown: leak_box(|_: &str| false),
        }
    }

    /// HD-1 竞窗主测试（确定性时序注入）：胜者占位在场 → 败者 entry 判 Occupied
    /// →（修复前此处重 get，胜者恰好已释放 ⇒ None ⇒ expect panic）→ 修复后同锁
    /// 克隆持有者快照、转复查分诊——断言**无 panic**、PendingRefused、败者零出手、
    /// 胜者已释放的占位**不被败者复活/重建**（注册表终态空）。
    #[test]
    fn concurrent_loser_survives_winner_fast_fail_release_without_panic() {
        let registry: &'static PendingRegistry =
            Box::leak(Box::new(PendingRegistry::new(HashMap::new())));
        let dispatches = StdArc::new(AtomicUsize::new(0));
        let observer_calls = StdArc::new(AtomicUsize::new(0));
        let deps = loser_race_deps(
            registry,
            dispatches.clone(),
            observer_calls.clone(),
            true, // 胜者快速失败：首次观察时释放占位（竞窗时序注入 2）
        );
        let home = tempfile::tempdir().unwrap();
        let s = oc_sess("ses_hd1_race", home.path());
        let flow = restore_with_pending_in(&s, &deps, false, registry, WINDOW, POLL, 1_000)
            .expect("HD-1 修复面：败者分诊必须无 panic 且受控流转");
        assert_eq!(
            flow,
            RestoreFlow::PendingRefused,
            "请求时点确有在途恢复（占位在场）→ 复查不命中 → 诚实拒绝"
        );
        assert_eq!(
            dispatches.load(Ordering::SeqCst),
            0,
            "败者零出手（复查分诊零 dispatch，不二窗）"
        );
        assert!(
            !registry_has(registry, "ses_hd1_race"),
            "胜者已释放的占位不得被败者复活/重建（终态 = 胜者 fast-fail 释放后归零）"
        );
        assert!(
            observer_calls.load(Ordering::SeqCst) >= 1,
            "复查分诊必须真实执行观察判定"
        );
    }

    /// 败者分诊对照面：占位仍**在场**（胜者未失败，正常并发复请求形态）→
    /// 同样 PendingRefused 零出手，且**占位保持**（败者分诊不得销毁在途恢复的
    /// 待确认态——胜者稍后返回时仍走既有占位语义）。
    #[test]
    fn concurrent_loser_triage_keeps_live_placeholder_and_refuses() {
        let registry: &'static PendingRegistry =
            Box::leak(Box::new(PendingRegistry::new(HashMap::new())));
        let dispatches = StdArc::new(AtomicUsize::new(0));
        let observer_calls = StdArc::new(AtomicUsize::new(0));
        let deps = loser_race_deps(
            registry,
            dispatches.clone(),
            observer_calls.clone(),
            false, // 胜者在途（未释放）：占位保持
        );
        let home = tempfile::tempdir().unwrap();
        let s = oc_sess("ses_hd1_live", home.path());
        let flow = restore_with_pending_in(&s, &deps, false, registry, WINDOW, POLL, 1_000)
            .expect("败者分诊受控流转");
        assert_eq!(flow, RestoreFlow::PendingRefused);
        assert_eq!(dispatches.load(Ordering::SeqCst), 0, "败者零出手");
        assert!(
            registry_has(registry, "ses_hd1_live"),
            "占位保持：败者分诊不得销毁在途恢复的待确认态"
        );
    }

    /// 真实线程并发压力（belt-and-braces，非确定性时序但断言确定性）：8 线程同
    /// 会话并发恢复、execute 全部确定性失败（占位必释放）——断言零 panic、每次
    /// 结局受控（Err = 出手者 / PendingRefused = 败者）、全部结束后占位归零。
    /// 修复前此形态可概率性命中 HD-1 panic 窗（entry 与重 get 之间的释放）。
    #[test]
    fn concurrent_acquisition_stress_is_panic_free_and_settles_empty() {
        let registry: &'static PendingRegistry =
            Box::leak(Box::new(PendingRegistry::new(HashMap::new())));
        let dispatches = StdArc::new(AtomicUsize::new(0));
        let home = tempfile::tempdir().unwrap();
        let s = oc_sess("ses_hd1_stress", home.path());
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let dispatches = dispatches.clone();
                let s = s.clone();
                std::thread::spawn(move || {
                    let dispatches = dispatches.clone();
                    let prepare = move |s2: &Session| {
                        Ok(super::super::resume::PreparedSpawn {
                            macos_payload: format!("'bin' --session {}", s2.id),
                            windows_payload: format!("bin --session {}", s2.id),
                            effect_sig: format!("bin --session {}", s2.id),
                            env_secret: None,
                        })
                    };
                    let execute = move |_p: &super::super::resume::PreparedSpawn,
                                        _s2: &Session,
                                        _sp: &SpawnFn,
                                        _on_delivered: Option<&DeliveryHook<'_>>|
                          -> Result<(), String> {
                        dispatches.fetch_add(1, Ordering::SeqCst);
                        Err("osascript 执行失败（退出码 1）".to_string())
                    };
                    let deps = RestoreDeps {
                        prepare: leak_box(prepare),
                        execute: leak_box(execute),
                        spawner: leak_box(|_spec: &super::super::resume::SpawnSpec| Ok(())),
                        observer: leak_box(|_p: &RestorePending| Vec::new()),
                        baseline_snapshot: leak_box(|_sig: &str| Vec::new()),
                        created_target: None,
                        on_confirmed: None,
                        registration_source: leak_box(|| (None, None)),
                        delivery_unknown: leak_box(|_: &str| false),
                    };
                    restore_with_pending_in(&s, &deps, false, registry, WINDOW, POLL, 1_000)
                })
            })
            .collect();
        for h in handles {
            let outcome = h
                .join()
                .expect("HD-1：并发线程零 panic（修复前可概率性 expect panic）");
            match outcome {
                // 出手者：确定性失败 Err（占位已随之释放）——受控失败回执
                Err(e) => assert!(e.contains("osascript"), "受控失败回执：{e}"),
                // 败者：复查分诊受控拒绝
                Ok(RestoreFlow::PendingRefused) => {}
                other => panic!("并发结局必须受控（Err / PendingRefused）：{other:?}"),
            }
        }
        assert!(
            dispatches.load(Ordering::SeqCst) >= 1,
            "至少一线程取得占位并出手"
        );
        assert!(
            !registry_has(registry, "ses_hd1_stress"),
            "全部确定性失败释放后占位归零（注册表终态一致）"
        );
    }

    // ==== I3：恢复核销按请求唯一标识 + 起窗前窗口基线 ====

    /// 候选窗口标记捷径（测试观察器用；pid+cmd 全等 = 同一窗口身份）。
    fn win(pid: u32, cmd: &str) -> ObservedWindow {
        ObservedWindow {
            pid,
            cmd: cmd.to_string(),
        }
    }

    /// I3 可拼装夹具：候选窗口集 / 基线快照 / 出手计数全部共享 Arc 运行时可调
    ///（execute 模拟生产交付语义：交付回调在场即调——交付即推进 Delivered）。
    struct I3Rig {
        dispatches: StdArc<AtomicUsize>,
        candidates: StdArc<std::sync::Mutex<Vec<ObservedWindow>>>,
        baseline: StdArc<std::sync::Mutex<Vec<ObservedWindow>>>,
    }

    impl I3Rig {
        fn new() -> Self {
            Self {
                dispatches: StdArc::new(AtomicUsize::new(0)),
                candidates: StdArc::new(std::sync::Mutex::new(Vec::new())),
                baseline: StdArc::new(std::sync::Mutex::new(Vec::new())),
            }
        }

        /// 依赖束（'static 泄漏；观察器返回当前候选集、基线缝返回当前基线集）。
        fn deps(&self) -> RestoreDeps<'static> {
            let dispatches = self.dispatches.clone();
            let candidates = self.candidates.clone();
            let baseline = self.baseline.clone();
            let prepare = move |s: &Session| {
                Ok(super::super::resume::PreparedSpawn {
                    macos_payload: format!("'bin' --session {}", s.id),
                    windows_payload: format!("bin --session {}", s.id),
                    effect_sig: format!("bin --session {}", s.id),
                    env_secret: None,
                })
            };
            let execute = move |_p: &super::super::resume::PreparedSpawn,
                                _s: &Session,
                                _sp: &SpawnFn,
                                on_delivered: Option<&DeliveryHook<'_>>|
                  -> Result<(), String> {
                dispatches.fetch_add(1, Ordering::SeqCst);
                if let Some(hook) = on_delivered {
                    hook();
                }
                Ok(())
            };
            let observer = move |_p: &RestorePending| candidates.lock().unwrap().clone();
            RestoreDeps {
                prepare: leak_box(prepare),
                execute: leak_box(execute),
                spawner: leak_box(|_spec: &super::super::resume::SpawnSpec| Ok(())),
                observer: leak_box(observer),
                baseline_snapshot: leak_box(move |_sig: &str| baseline.lock().unwrap().clone()),
                created_target: None,
                on_confirmed: None,
                registration_source: leak_box(|| (None, None)),
                delivery_unknown: leak_box(|e: &str| {
                    crate::inject::resume::is_delivery_unknown_error(e)
                }),
            }
        }

        /// 并发变体：prepare 内 Barrier 同步到同一竞争点（多通道同抢单锁占位）。
        fn deps_with_barrier(&self, barrier: StdArc<std::sync::Barrier>) -> RestoreDeps<'static> {
            let dispatches = self.dispatches.clone();
            let candidates = self.candidates.clone();
            let baseline = self.baseline.clone();
            let prepare = move |s: &Session| {
                barrier.wait();
                Ok(super::super::resume::PreparedSpawn {
                    macos_payload: format!("'bin' --session {}", s.id),
                    windows_payload: format!("bin --session {}", s.id),
                    effect_sig: format!("bin --session {}", s.id),
                    env_secret: None,
                })
            };
            let execute = move |_p: &super::super::resume::PreparedSpawn,
                                _s: &Session,
                                _sp: &SpawnFn,
                                on_delivered: Option<&DeliveryHook<'_>>|
                  -> Result<(), String> {
                dispatches.fetch_add(1, Ordering::SeqCst);
                if let Some(hook) = on_delivered {
                    hook();
                }
                Ok(())
            };
            let observer = move |_p: &RestorePending| candidates.lock().unwrap().clone();
            RestoreDeps {
                prepare: leak_box(prepare),
                execute: leak_box(execute),
                spawner: leak_box(|_spec: &super::super::resume::SpawnSpec| Ok(())),
                observer: leak_box(observer),
                baseline_snapshot: leak_box(move |_sig: &str| baseline.lock().unwrap().clone()),
                created_target: None,
                on_confirmed: None,
                registration_source: leak_box(|| (None, None)),
                delivery_unknown: leak_box(|e: &str| {
                    crate::inject::resume::is_delivery_unknown_error(e)
                }),
            }
        }

        fn sig_of(sid: &str) -> String {
            format!("bin --session {sid}")
        }
    }

    /// RED a（brief I3 a；主会话裁定口径）：三请求**并发**恢复同会话——H 轮语义
    /// 保持「同一会话同时只持一个占位」（1 胜者 2 败者）；验证核心面 = 占位按
    /// 请求 token 区分（互不误删）、败者拒绝后各自后续请求独立（核销身份化）、
    /// 终态各归其位（胜者占位核销后，新一轮并发仍 1 胜 2 败、新占位 token 独立）。
    #[test]
    fn i3_three_concurrent_requests_identity_scoped_exactly_one_window() {
        let home = tempfile::tempdir().unwrap();
        let sid = "ses_i3_a1";
        let rig = I3Rig::new();
        let registry: &'static PendingRegistry =
            Box::leak(Box::new(PendingRegistry::new(HashMap::new())));
        let s = oc_sess(sid, home.path());
        // 轮 1：三通道并发（Barrier 同点竞争单锁占位）→ 1 胜者（待确认）2 败者
        let run_trio = |rig: &I3Rig, registry: &'static PendingRegistry, s: &Session| {
            let barrier = StdArc::new(std::sync::Barrier::new(3));
            let results: Vec<Result<RestoreFlow, String>> = std::thread::scope(|scope| {
                let mut handles = Vec::new();
                for _ in 0..3 {
                    let barrier = barrier.clone();
                    let s = s.clone();
                    let deps = rig.deps_with_barrier(barrier);
                    handles.push(scope.spawn(move || {
                        restore_with_pending_in(&s, &deps, false, registry, WINDOW, POLL, 1_000)
                    }));
                }
                handles
                    .into_iter()
                    .map(|h| h.join().expect("并发通道线程不 panic"))
                    .collect()
            });
            results
        };
        let round1 = run_trio(&rig, registry, &s);
        let flows1: Vec<RestoreFlow> = round1
            .into_iter()
            .map(|r| r.expect("并发恢复各自是受控流转，不是错误"))
            .collect();
        assert_eq!(
            flows1
                .iter()
                .filter(|f| **f == RestoreFlow::PendingRefused)
                .count(),
            2,
            "两败者被占位拒绝：{flows1:?}"
        );
        assert_eq!(
            flows1
                .iter()
                .filter(|f| **f == RestoreFlow::Dispatched { confirmed: false })
                .count(),
            1,
            "恰一胜者出手（观察未就绪 → 待确认）：{flows1:?}"
        );
        assert_eq!(
            rig.dispatches.load(Ordering::SeqCst),
            1,
            "三并发恰一窗（H 轮语义保持）"
        );
        // 占位按请求标识在场：token 非空、状态已交付（交付即推进）、基线快照随占位
        let token_round1 = {
            let reg = registry.lock().unwrap();
            let p = reg.get(sid).expect("胜者占位在场");
            assert!(!p.request_token.is_empty(), "占位必须携带请求唯一标识");
            assert_eq!(
                p.state,
                RestorePendingState::Delivered,
                "交付回调已触发 ⇒ 已交付态（核销资格形态）"
            );
            p.request_token.clone()
        };
        // 胜者占位核销（观察翻转 → 复查命中，按 token 核销自己的占位）
        *rig.candidates.lock().unwrap() = vec![win(300, &I3Rig::sig_of(sid))];
        let settle = restore_with_pending_in(&s, &rig.deps(), false, registry, WINDOW, POLL, 2_000)
            .expect("复查命中 → 核销");
        assert_eq!(
            settle,
            RestoreFlow::PendingConfirmed,
            "终态各归其位：胜者占位核销"
        );
        assert!(!registry_has(registry, sid), "核销后注册表清空");
        // 轮 2：败者通道的后续请求独立——新一轮并发仍 1 胜 2 败，新占位 token 独立
        //（互不误删：核销身份化的核心面——前一轮 token 已随占位消亡，新请求不受影响）
        rig.dispatches.store(0, Ordering::SeqCst);
        *rig.candidates.lock().unwrap() = Vec::new();
        let round2 = run_trio(&rig, registry, &s);
        let flows2: Vec<RestoreFlow> = round2
            .into_iter()
            .map(|r| r.expect("轮 2 并发恢复受控流转"))
            .collect();
        assert_eq!(
            flows2
                .iter()
                .filter(|f| **f == RestoreFlow::PendingRefused)
                .count(),
            2,
            "轮 2 同形：两败者：{flows2:?}"
        );
        assert_eq!(
            rig.dispatches.load(Ordering::SeqCst),
            1,
            "轮 2 出手恰一次（后续请求独立、零第二窗）"
        );
        let token_round2 = {
            let reg = registry.lock().unwrap();
            let p = reg.get(sid).expect("轮 2 胜者占位在场");
            assert_ne!(
                p.request_token, token_round1,
                "占位按请求标识区分：每请求独立 token（互不误删的身份基础）"
            );
            p.request_token.clone()
        };
        assert!(!token_round2.is_empty());
    }

    /// RED b-1（brief I3 改法 4）：并发核对不得在首请求出手前清占位——复查落在
    /// 胜者「占位已取得、spawner 未出手」（准备中态）窗口内时，即使观察器报告
    /// 匹配窗口在场，也不得核销（零消费观察器、占位保持、随后胜者正常交付流转）。
    /// 时序注入（mpsc 门，零真实竞速）：A 的 execute 进入即阻塞，B 在主线程复查。
    #[test]
    fn i3_recheck_during_preparing_never_consumes_observer_or_settles() {
        let home = tempfile::tempdir().unwrap();
        let sid = "ses_i3_b1";
        let rig = I3Rig::new();
        let registry: &'static PendingRegistry =
            Box::leak(Box::new(PendingRegistry::new(HashMap::new())));
        let s = oc_sess(sid, home.path());
        let (tx_enter, rx_enter) = std::sync::mpsc::channel();
        let (tx_release, rx_release) = std::sync::mpsc::channel();
        // 胜者 A：占位插入后 execute 阻塞（准备中态在场窗口），放行后才出手交付
        let dispatches_a = rig.dispatches.clone();
        let prepare_a = move |s: &Session| {
            Ok(super::super::resume::PreparedSpawn {
                macos_payload: format!("'bin' --session {}", s.id),
                windows_payload: format!("bin --session {}", s.id),
                effect_sig: format!("bin --session {}", s.id),
                env_secret: None,
            })
        };
        // Receiver 非 Sync（deps 需 Send+Sync 跨线程共享）——Mutex 包一层
        let rx_release = StdArc::new(std::sync::Mutex::new(rx_release));
        let execute_a = move |_p: &super::super::resume::PreparedSpawn,
                              _s: &Session,
                              _sp: &SpawnFn,
                              on_delivered: Option<&DeliveryHook<'_>>|
              -> Result<(), String> {
            tx_enter.send(()).expect("A execute 进入信号");
            rx_release
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .recv()
                .expect("A execute 放行");
            dispatches_a.fetch_add(1, Ordering::SeqCst);
            if let Some(hook) = on_delivered {
                hook();
            }
            Ok(())
        };
        let deps_a = RestoreDeps {
            prepare: leak_box(prepare_a),
            execute: leak_box(execute_a),
            spawner: leak_box(|_spec: &super::super::resume::SpawnSpec| Ok(())),
            observer: leak_box(|_p: &RestorePending| Vec::new()),
            baseline_snapshot: leak_box(|_sig: &str| Vec::new()),
            created_target: None,
            on_confirmed: None,
            registration_source: leak_box(|| (None, None)),
            delivery_unknown: leak_box(|e: &str| {
                crate::inject::resume::is_delivery_unknown_error(e)
            }),
        };
        let a_handle = std::thread::spawn(move || {
            restore_with_pending_in(&s, &deps_a, false, registry, WINDOW, POLL, 1_000)
        });
        rx_enter
            .recv()
            .expect("A 已插入占位并阻塞在 execute（准备中态在场）");
        // 败者 B：主线程同步复查——观察器恒报「有匹配窗」，但准备中守卫必须零消费
        let b_observer_calls = StdArc::new(AtomicUsize::new(0));
        let b_calls2 = b_observer_calls.clone();
        let prepare_b = move |s: &Session| {
            Ok(super::super::resume::PreparedSpawn {
                macos_payload: format!("'bin' --session {}", s.id),
                windows_payload: format!("bin --session {}", s.id),
                effect_sig: format!("bin --session {}", s.id),
                env_secret: None,
            })
        };
        let execute_b = move |_p: &super::super::resume::PreparedSpawn,
                              _s: &Session,
                              _sp: &SpawnFn,
                              _on_delivered: Option<&DeliveryHook<'_>>|
              -> Result<(), String> {
            panic!("败者零出手：复查分诊绝不 dispatch")
        };
        let observer_b = move |_p: &RestorePending| {
            b_calls2.fetch_add(1, Ordering::SeqCst);
            vec![win(990, "stale --session ses_i3_b1")]
        };
        let deps_b = RestoreDeps {
            prepare: leak_box(prepare_b),
            execute: leak_box(execute_b),
            spawner: leak_box(|_spec: &super::super::resume::SpawnSpec| Ok(())),
            observer: leak_box(observer_b),
            baseline_snapshot: leak_box(|_sig: &str| Vec::new()),
            created_target: None,
            on_confirmed: None,
            registration_source: leak_box(|| (None, None)),
            delivery_unknown: leak_box(|e: &str| {
                crate::inject::resume::is_delivery_unknown_error(e)
            }),
        };
        let s_b = oc_sess(sid, home.path());
        let b = restore_with_pending_in(&s_b, &deps_b, false, registry, WINDOW, POLL, 2_000)
            .expect("复查是受控流转");
        assert_eq!(
            b,
            RestoreFlow::PendingRefused,
            "准备中态：复查只拒不清（并发核对不得在首请求出手前清占位）"
        );
        assert_eq!(
            b_observer_calls.load(Ordering::SeqCst),
            0,
            "准备中态观察器不得被消费（更不得据此核销）"
        );
        assert!(registry_has(registry, sid), "占位保持（未被并发核对清掉）");
        // 放行 A：出手 + 交付（Delivered）→ 观察空 → 待确认（终态各归其位）
        tx_release.send(()).expect("放行 A");
        let a_flow = a_handle
            .join()
            .expect("胜者线程不 panic")
            .expect("胜者受控流转");
        assert_eq!(a_flow, RestoreFlow::Dispatched { confirmed: false });
        assert_eq!(rig.dispatches.load(Ordering::SeqCst), 1, "出手恰一次");
        let reg = registry.lock().unwrap();
        let p = reg.get(sid).expect("占位保持（A 交付后的待确认态）");
        assert_eq!(
            p.state,
            RestorePendingState::Delivered,
            "A 交付回调已推进已交付态（等待观察核销）"
        );
        assert!(!p.request_token.is_empty(), "占位携带请求唯一标识");
    }

    /// RED b-2（brief I3 b）起窗前窗口基线：出手前已存在的匹配窗口（基线内）
    /// 触发观察 ⇒ 不核销新请求（非本次新起窗口），保持待确认不冒充成功；
    /// 基线外新窗口出现（候选混入旧+新）→ 只核销基线外那扇（正常链回归，RED d）。
    #[test]
    fn i3_baseline_window_late_observation_refuses_and_new_window_settles() {
        let home = tempfile::tempdir().unwrap();
        let sid = "ses_i3_b2";
        let rig = I3Rig::new();
        let registry = empty_registry();
        let s = oc_sess(sid, home.path());
        let sig = I3Rig::sig_of(sid);
        // 基线：出手前已有一扇匹配窗（pid 100）；观察器晚到报告同一旧窗
        *rig.baseline.lock().unwrap() = vec![win(100, &sig)];
        *rig.candidates.lock().unwrap() = vec![win(100, &sig)];
        let first = restore_with_pending_in(&s, &rig.deps(), false, &registry, WINDOW, POLL, 1_000)
            .expect("首请求出手成功");
        assert_eq!(
            first,
            RestoreFlow::Dispatched { confirmed: false },
            "基线内旧窗口不得核销新恢复（不冒充成功）"
        );
        assert!(registry_has(&registry, sid), "保持待确认（占位在场）");
        assert_eq!(
            rig.dispatches.load(Ordering::SeqCst),
            1,
            "旧窗拒绝面零额外出手"
        );
        // 候选混入基线外新窗（pid 200）→ 挑新窗核销（正常链：新窗+服务连接 → 确认）
        *rig.candidates.lock().unwrap() = vec![win(100, &sig), win(200, &sig)];
        let second =
            restore_with_pending_in(&s, &rig.deps(), false, &registry, WINDOW, POLL, 2_000)
                .expect("复查命中新窗 → 核销");
        assert_eq!(
            second,
            RestoreFlow::PendingConfirmed,
            "基线外新窗口才可核销（正常链回归）"
        );
        assert!(!registry_has(&registry, sid), "核销后注册表清空");
        assert_eq!(rig.dispatches.load(Ordering::SeqCst), 1, "复查核销零出手");
    }

    /// J1：abandon 原子化——「读取旧占位 → 旧占位核销 → 新请求占位 → 旧放弃继续」
    /// 竞窗下，旧放弃不得删除新占位、不得清理新请求的凭据文件；只结算自己捕获的
    /// 占位与凭据文件。修复前两锁形态：注入缝触发后第二锁 `remove(&sid)` 盲删
    /// 【新占位】（本测断言 RED）；修复后单临界区，注入缝在原子删除之后触发
    /// （新占位存活，settle 仅及自己捕获的旧凭据文件）。
    #[test]
    fn j1_abandon_settles_only_own_placeholder_and_env_file() {
        let home = tempfile::tempdir().unwrap();
        let sid = "ses_j1_a1";
        let registry: &'static PendingRegistry =
            Box::leak(Box::new(PendingRegistry::new(HashMap::new())));
        let f_old = home.path().join("resume-env-old.token");
        let f_new = home.path().join("resume-env-new.token");
        std::fs::write(&f_old, b"old").unwrap();
        std::fs::write(&f_new, b"new").unwrap();
        let mk = |token: &str, env: Option<PathBuf>| RestorePending {
            session_id: sid.to_string(),
            effect_sig: format!("'bin' --session {sid}"),
            state_file: None,
            expected_directory: Some(home.path().to_path_buf()),
            created_at_ms: 1_000,
            env_secret_path: env,
            generation: None,
            created_target: None,
            state: RestorePendingState::AwaitingObservation,
            request_token: token.to_string(),
            baseline: Vec::new(),
            in_flight: false,
        };
        registry
            .lock()
            .unwrap()
            .insert(sid.to_string(), mk("tok-old", Some(f_old.clone())));
        // 竞窗注入：abandon 读取旧占位之后——旧占位核销 + 新请求占位（新 token/
        // 新凭据文件/in_flight=false）。钩子持有自有克隆（'static 闭包）。
        let sid_h = sid.to_string();
        let sid_r = sid_h.clone();
        let sid_m = sid_h.clone();
        let home_h = home.path().to_path_buf();
        let f_new_h = f_new.clone();
        let mk_hook = move |token: String, env: PathBuf| RestorePending {
            session_id: sid_m.clone(),
            effect_sig: format!("'bin' --session {sid_m}"),
            state_file: None,
            expected_directory: Some(home_h.clone()),
            created_at_ms: 1_000,
            env_secret_path: Some(env),
            generation: None,
            created_target: None,
            state: RestorePendingState::AwaitingObservation,
            request_token: token,
            baseline: Vec::new(),
            in_flight: false,
        };
        // 停点按会话键安装（用户复审测试修整：全局单槽会误伤并行其他 abandon
        // 用例），用毕由 defer 守卫移除。
        install_between_abandon_locks_pause(
            sid,
            StdArc::new(move || {
                let mut g = registry.lock().unwrap();
                g.remove(&sid_r);
                g.insert(
                    sid_h.clone(),
                    mk_hook("tok-new".to_string(), f_new_h.clone()),
                );
            }),
        );
        // 具名守卫：临时值会在语句末丢弃（停点被立刻移除的根因）——必须绑定存活
        let _pause_guard = defer_remove_between_abandon_locks_pause(sid);
        let s = oc_sess(sid, home.path());
        let deps = RestoreDeps {
            prepare: leak_box(|_s: &Session| Err("never".to_string())),
            execute: leak_box(
                |_p: &super::super::resume::PreparedSpawn,
                 _s: &Session,
                 _sp: &super::super::resume::SpawnFn,
                 _h: Option<&DeliveryHook<'_>>|
                 -> Result<(), String> { Err("never".to_string()) },
            ),
            spawner: leak_box(|_spec: &super::super::resume::SpawnSpec| Ok(())),
            observer: leak_box(|_p: &RestorePending| Vec::new()),
            baseline_snapshot: leak_box(|_sig: &str| Vec::new()),
            created_target: None,
            on_confirmed: None,
            registration_source: leak_box(|| (None, None)),
            delivery_unknown: leak_box(|e: &str| {
                crate::inject::resume::is_delivery_unknown_error(e)
            }),
        };
        let flow = restore_with_pending_in(&s, &deps, true, registry, WINDOW, POLL, 2_000)
            .expect("放弃是受控流转");
        assert_eq!(flow, RestoreFlow::Abandoned);
        // 新占位存活且未被旧放弃盲删（修复前：被第二锁 remove(&sid) 删除 → RED）
        let survivor = registry.lock().unwrap().get(sid).cloned();
        assert_eq!(
            survivor.map(|p| p.request_token),
            Some("tok-new".to_string()),
            "旧放弃不得删除新请求的占位"
        );
        // 凭据文件：旧（自己捕获的）已结算；新请求的原样（token 唯一路径，旧
        // settle 只及自己捕获的路径）
        assert!(!f_old.exists(), "旧请求凭据文件按 token 结算删除");
        assert!(f_new.exists(), "新请求凭据文件不得被旧放弃清理");
    }

    /// IC fix round 1 · P1-B 主回归（用户裁决口径「A 正在交付 → B 放弃 → C 重试」）：
    /// 准备/交付仍在执行（起窗权在途）时放弃**不得直接释放起窗权**——交付链无法
    /// 确认取消 ⇒ 占位保持 + 诚实回执 [`RestoreFlow::AbandonDeferred`]；C 重试在
    /// 占位被 A 持有期间被拒（零出手）；A 的交付不受 B 放弃影响，完成后按状态机
    /// 正常核销。全程**最多一次起窗**（修复前：B 放弃删占位 → C 取得占位出手 +
    /// A 继续交付 = 两窗，且交付回调会把 A 的占位重插回空位）。
    #[test]
    fn i3_abandon_deferred_while_delivering_at_most_one_window() {
        let home = tempfile::tempdir().unwrap();
        let sid = "ses_p1b_a1";
        let rig = I3Rig::new();
        let registry: &'static PendingRegistry =
            Box::leak(Box::new(PendingRegistry::new(HashMap::new())));
        let s = oc_sess(sid, home.path());
        let (tx_enter, rx_enter) = std::sync::mpsc::channel();
        let (tx_release, rx_release) = std::sync::mpsc::channel();
        // 胜者 A：占位插入后 execute 阻塞（准备中态/起窗权在途窗口），放行后才交付
        let dispatches_a = rig.dispatches.clone();
        let prepare_a = move |s: &Session| {
            Ok(super::super::resume::PreparedSpawn {
                macos_payload: format!("'bin' --session {}", s.id),
                windows_payload: format!("bin --session {}", s.id),
                effect_sig: format!("bin --session {}", s.id),
                env_secret: None,
            })
        };
        // Receiver 非 Sync（deps 需 Send+Sync 跨线程共享）——Mutex 包一层
        let rx_release = StdArc::new(std::sync::Mutex::new(rx_release));
        let execute_a = move |_p: &super::super::resume::PreparedSpawn,
                              _s: &Session,
                              _sp: &SpawnFn,
                              on_delivered: Option<&DeliveryHook<'_>>|
              -> Result<(), String> {
            tx_enter.send(()).expect("A execute 进入信号");
            rx_release
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .recv()
                .expect("A execute 放行");
            dispatches_a.fetch_add(1, Ordering::SeqCst);
            if let Some(hook) = on_delivered {
                hook();
            }
            Ok(())
        };
        let deps_a = RestoreDeps {
            prepare: leak_box(prepare_a),
            execute: leak_box(execute_a),
            spawner: leak_box(|_spec: &super::super::resume::SpawnSpec| Ok(())),
            observer: leak_box(|_p: &RestorePending| Vec::new()),
            baseline_snapshot: leak_box(|_sig: &str| Vec::new()),
            created_target: None,
            on_confirmed: None,
            registration_source: leak_box(|| (None, None)),
            delivery_unknown: leak_box(|e: &str| {
                crate::inject::resume::is_delivery_unknown_error(e)
            }),
        };
        let s_a = oc_sess(sid, home.path());
        let a_handle = std::thread::spawn(move || {
            restore_with_pending_in(&s_a, &deps_a, false, registry, WINDOW, POLL, 1_000)
        });
        rx_enter
            .recv()
            .expect("A 已插入占位并阻塞在 execute（起窗权在途）");
        // B 放弃：起窗权在途 ⇒ 暂缓（占位保持，诚实回执；零出手）
        let s_b = oc_sess(sid, home.path());
        let b = restore_with_pending_in(&s_b, &rig.deps(), true, registry, WINDOW, POLL, 1_100)
            .expect("放弃暂缓是受控流转");
        assert_eq!(
            b,
            RestoreFlow::AbandonDeferred,
            "起窗权在途：放弃不得直接释放（交付进行中，无法放弃，请等待核对）"
        );
        assert_eq!(rig.dispatches.load(Ordering::SeqCst), 0, "B 放弃零出手");
        {
            let reg = registry.lock().unwrap();
            let p = reg.get(sid).expect("占位必须保持（未被 B 放弃清掉）");
            assert_eq!(p.state, RestorePendingState::Preparing, "A 仍在准备中");
            assert!(p.in_flight, "起窗权在途标记在场");
        }
        // C 重试：占位被 A 持有期间被拒（零出手、不消费观察器——准备中态闸）
        let s_c = oc_sess(sid, home.path());
        let c = restore_with_pending_in(&s_c, &rig.deps(), false, registry, WINDOW, POLL, 1_200)
            .expect("C 重试是受控流转");
        assert_eq!(c, RestoreFlow::PendingRefused, "C 在占位被 A 持有期间被拒");
        assert_eq!(rig.dispatches.load(Ordering::SeqCst), 0, "C 零出手");
        // 放行 A：交付不受 B 放弃影响（交付回调推进已交付态）→ 观察空 → 待确认
        tx_release.send(()).expect("放行 A");
        let a_flow = a_handle
            .join()
            .expect("胜者线程不 panic")
            .expect("A 受控流转");
        assert_eq!(a_flow, RestoreFlow::Dispatched { confirmed: false });
        assert_eq!(
            rig.dispatches.load(Ordering::SeqCst),
            1,
            "最多一次起窗：A 的交付 + B 的放弃 + C 的重试合计恰一窗（修复前两窗）"
        );
        {
            let reg = registry.lock().unwrap();
            let p = reg.get(sid).expect("A 交付后占位保持（待确认态）");
            assert_eq!(p.state, RestorePendingState::Delivered, "交付回调已推进");
            assert!(!p.in_flight, "流转线程已收口（起窗权在途标记清零）");
            assert!(!p.request_token.is_empty(), "占位携带请求唯一标识");
        }
        // A 完成后按状态机正常核销（观察翻转 → 复查命中）
        *rig.candidates.lock().unwrap() = vec![win(400, &I3Rig::sig_of(sid))];
        let settle = restore_with_pending_in(&s, &rig.deps(), false, registry, WINDOW, POLL, 2_000)
            .expect("复查命中 → 核销");
        assert_eq!(settle, RestoreFlow::PendingConfirmed, "终态各归其位");
        assert!(!registry_has(registry, sid), "核销后注册表清空");
        // 收口后再放弃：幂等放弃（无占位）——既有语义恢复
        let s_b2 = oc_sess(sid, home.path());
        let b2 = restore_with_pending_in(&s_b2, &rig.deps(), true, registry, WINDOW, POLL, 2_100)
            .expect("收口后放弃幂等");
        assert_eq!(b2, RestoreFlow::Abandoned);
        assert_eq!(
            rig.dispatches.load(Ordering::SeqCst),
            1,
            "全程出手恰一次（核销/放弃零额外出手）"
        );
    }

    /// IC fix round 1 · P1-B 消除面一：交付回调遇**空位**零重插——占位先于交付
    /// 消亡（起窗权已移出本请求）时，回调不得把旧请求占位插回空位（修复前 Vacant
    /// 分支 or_insert 会让已放弃/已释放的请求复活占用起窗权）。时序注入（mpsc 门）：
    /// 占位删除落在 A 出手前，回调在删除后触发。
    #[test]
    fn i3_delivery_hook_vacant_slot_never_reinserts() {
        let home = tempfile::tempdir().unwrap();
        let sid = "ses_p1b_h1";
        let registry: &'static PendingRegistry =
            Box::leak(Box::new(PendingRegistry::new(HashMap::new())));
        let dispatches = StdArc::new(AtomicUsize::new(0));
        let (tx_enter, rx_enter) = std::sync::mpsc::channel();
        let (tx_release, rx_release) = std::sync::mpsc::channel();
        let dispatches2 = dispatches.clone();
        let prepare = move |s: &Session| {
            Ok(super::super::resume::PreparedSpawn {
                macos_payload: format!("'bin' --session {}", s.id),
                windows_payload: format!("bin --session {}", s.id),
                effect_sig: format!("bin --session {}", s.id),
                env_secret: None,
            })
        };
        let rx_release = StdArc::new(std::sync::Mutex::new(rx_release));
        let execute = move |_p: &super::super::resume::PreparedSpawn,
                            _s: &Session,
                            _sp: &SpawnFn,
                            on_delivered: Option<&DeliveryHook<'_>>|
              -> Result<(), String> {
            tx_enter.send(()).expect("execute 进入信号");
            rx_release
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .recv()
                .expect("execute 放行");
            dispatches2.fetch_add(1, Ordering::SeqCst);
            if let Some(hook) = on_delivered {
                hook();
            }
            Ok(())
        };
        let deps = RestoreDeps {
            prepare: leak_box(prepare),
            execute: leak_box(execute),
            spawner: leak_box(|_spec: &super::super::resume::SpawnSpec| Ok(())),
            observer: leak_box(|_p: &RestorePending| Vec::new()),
            baseline_snapshot: leak_box(|_sig: &str| Vec::new()),
            created_target: None,
            on_confirmed: None,
            registration_source: leak_box(|| (None, None)),
            delivery_unknown: leak_box(|e: &str| {
                crate::inject::resume::is_delivery_unknown_error(e)
            }),
        };
        let s = oc_sess(sid, home.path());
        let handle = std::thread::spawn(move || {
            restore_with_pending_in(&s, &deps, false, registry, WINDOW, POLL, 1_000)
        });
        rx_enter.recv().expect("占位已在场并阻塞在 execute");
        // 占位先行消亡（删除落在出手前——模拟起窗权已移出本请求的注册表终态）
        registry.lock().unwrap().remove(sid);
        tx_release.send(()).expect("放行");
        let flow = handle.join().expect("流转线程不 panic").expect("受控流转");
        assert_eq!(flow, RestoreFlow::Dispatched { confirmed: false });
        assert!(
            !registry_has(registry, sid),
            "交付回调遇空位必须零重插（旧请求不得借回调复活占用起窗权）"
        );
        assert_eq!(dispatches.load(Ordering::SeqCst), 1, "出手恰一次");
    }

    /// IC fix round 1 · P1-B 消除面二：交付回调遇**异 token** 占位零触碰——占位已
    /// 属后续请求时回调不得覆盖/误插（H 轮语义保持的回归锁）；流转收口的在途标记
    /// 清零同样按 token 比对零触碰（不误清后续请求的标记）。
    #[test]
    fn i3_delivery_hook_foreign_token_zero_touch() {
        let home = tempfile::tempdir().unwrap();
        let sid = "ses_p1b_h2";
        let registry: &'static PendingRegistry =
            Box::leak(Box::new(PendingRegistry::new(HashMap::new())));
        let dispatches = StdArc::new(AtomicUsize::new(0));
        let (tx_enter, rx_enter) = std::sync::mpsc::channel();
        let (tx_release, rx_release) = std::sync::mpsc::channel();
        let dispatches2 = dispatches.clone();
        let prepare = move |s: &Session| {
            Ok(super::super::resume::PreparedSpawn {
                macos_payload: format!("'bin' --session {}", s.id),
                windows_payload: format!("bin --session {}", s.id),
                effect_sig: format!("bin --session {}", s.id),
                env_secret: None,
            })
        };
        let rx_release = StdArc::new(std::sync::Mutex::new(rx_release));
        let execute = move |_p: &super::super::resume::PreparedSpawn,
                            _s: &Session,
                            _sp: &SpawnFn,
                            on_delivered: Option<&DeliveryHook<'_>>|
              -> Result<(), String> {
            tx_enter.send(()).expect("execute 进入信号");
            rx_release
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .recv()
                .expect("execute 放行");
            dispatches2.fetch_add(1, Ordering::SeqCst);
            if let Some(hook) = on_delivered {
                hook();
            }
            Ok(())
        };
        let deps = RestoreDeps {
            prepare: leak_box(prepare),
            execute: leak_box(execute),
            spawner: leak_box(|_spec: &super::super::resume::SpawnSpec| Ok(())),
            observer: leak_box(|_p: &RestorePending| Vec::new()),
            baseline_snapshot: leak_box(|_sig: &str| Vec::new()),
            created_target: None,
            on_confirmed: None,
            registration_source: leak_box(|| (None, None)),
            delivery_unknown: leak_box(|e: &str| {
                crate::inject::resume::is_delivery_unknown_error(e)
            }),
        };
        let s = oc_sess(sid, home.path());
        let handle = std::thread::spawn(move || {
            restore_with_pending_in(&s, &deps, false, registry, WINDOW, POLL, 1_000)
        });
        rx_enter.recv().expect("占位已在场并阻塞在 execute");
        // 占位易主为后续请求（异 token、非在途）——落在出手前
        let successor = RestorePending {
            session_id: sid.to_string(),
            effect_sig: format!("successor --session {sid}"),
            state_file: None,
            expected_directory: None,
            created_at_ms: 1_050,
            env_secret_path: None,
            generation: None,
            created_target: None,
            state: RestorePendingState::Delivered,
            request_token: "successor-token".to_string(),
            baseline: Vec::new(),
            in_flight: false,
        };
        registry.lock().unwrap().insert(sid.to_string(), successor);
        tx_release.send(()).expect("放行");
        let flow = handle.join().expect("流转线程不 panic").expect("受控流转");
        assert_eq!(
            flow,
            RestoreFlow::Dispatched { confirmed: false },
            "异 token：旧请求不得核销（更不得冒充确认）"
        );
        {
            let reg = registry.lock().unwrap();
            let p = reg.get(sid).expect("后续请求占位必须保持");
            assert_eq!(
                p.request_token, "successor-token",
                "交付回调异 token 零触碰：占位仍属后续请求"
            );
            assert_eq!(
                p.state,
                RestorePendingState::Delivered,
                "后续请求状态未被覆盖"
            );
            assert!(
                !p.in_flight,
                "收口清零按 token 比对：不误清后续请求的在途标记"
            );
        }
        assert_eq!(dispatches.load(Ordering::SeqCst), 1, "出手恰一次");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn tcp_peer_matches_rejects_port_prefix() {
        assert!(!tcp_peer_matches("127.0.0.1:40123", "127.0.0.1:4012"));
        assert!(!tcp_peer_matches("127.0.0.1:40123", "localhost:4012"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn tcp_peer_matches_accepts_exact_loopback_port() {
        assert!(tcp_peer_matches("127.0.0.1:40123", "127.0.0.1:40123"));
        assert!(tcp_peer_matches("127.0.0.1:40123", "localhost:40123"));
        assert!(tcp_peer_matches("[::1]:40123", "localhost:40123"));
        assert!(!tcp_peer_matches("192.0.2.1:40123", "localhost:40123"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn restore_observer_rejects_changed_endpoint_with_same_runtime_key() {
        let project = tempfile::Builder::new()
            .prefix("restore-observer-project-")
            .tempdir_in(env!("CARGO_MANIFEST_DIR"))
            .unwrap();
        let directory =
            super::super::create_path::validate_existing(project.path().to_str().unwrap())
                .expect("工作区临时项目应满足 D21 目录校验");
        let state_file = project.path().join("state.json");
        let pending = RestorePending {
            session_id: "ses_created_endpoint".into(),
            effect_sig: "bin --session ses_created_endpoint".into(),
            state_file: Some(state_file.clone()),
            expected_directory: Some(PathBuf::from(&directory.path)),
            created_at_ms: 0,
            env_secret_path: None,
            generation: Some(RuntimeKey {
                service_id: "svc-created".into(),
                pid: 424,
                started_at_ms: 5_000,
            }),
            created_target: Some(CreatedRestoreTarget {
                directory,
                endpoint: reqwest::Url::parse("http://127.0.0.1:40123").unwrap(),
                version: "2.2.5".into(),
                credential_hash: Sha256::digest(b"pw").into(),
            }),
            state: RestorePendingState::Delivered,
            request_token: "created-endpoint-token".into(),
            baseline: Vec::new(),
            in_flight: false,
        };
        let snapshot = [super::super::resume::CmdCwdEntry {
            pid: 777,
            cmd: pending.effect_sig.clone(),
            cwd: pending.expected_directory.clone(),
        }];
        let fact_at = |pid| (pid == 424).then_some(5_000);
        let observe = |port| {
            restore_observer_with(
                &pending,
                &snapshot,
                &|path| {
                    assert_eq!(path, state_file.as_path());
                    Ok(format!(
                        r#"{{"id":"svc-created","version":"2.2.5","url":"http://127.0.0.1:{port}","pid":424,"password":"pw"}}"#
                    )
                    .into_bytes())
                },
                &fact_at,
                &|_, host_port| {
                    assert_eq!(host_port, "127.0.0.1:40123", "端点变更时不得探测新端点");
                    true
                },
            )
        };
        assert_eq!(observe(40123), vec![win(777, &pending.effect_sig)]);
        assert!(observe(40124).is_empty(), "端点变化后保持未确认");
    }

    /// IC fix round 1 · P2-C 回归一（观察核级）：serve 连接证据闸——进程+目录+登记
    /// +代际四面全过但候选窗口**连接未就绪** ⇒ 空集（不确认）；连接到正确服务
    /// （代际锚全等登记描述符给出的端点）⇒ 候选放行；登记面未过 ⇒ 探针零出手
    ///（fail-closed 不确认）。
    #[test]
    fn restore_observer_connection_evidence_gate() {
        use crate::services::opencode_native::types::NativeError;
        let dir = tempfile::tempdir().unwrap();
        let proj = tempfile::tempdir().unwrap();
        let proj_canon = std::fs::canonicalize(proj.path()).unwrap();
        let state_file = dir.path().join("state.json");
        std::fs::write(
            &state_file,
            r#"{"id":"svc-p2c","version":"2.2.5","url":"http://127.0.0.1:40123","pid":424,"password":"pw"}"#,
        )
        .unwrap();
        let pending = RestorePending {
            session_id: "ses_p2c".to_string(),
            effect_sig: "bin --session ses_p2c".to_string(),
            state_file: Some(state_file.clone()),
            expected_directory: Some(proj_canon.clone()),
            created_at_ms: 0,
            env_secret_path: None,
            generation: Some(RuntimeKey {
                service_id: "svc-p2c".to_string(),
                pid: 424,
                started_at_ms: 5_000,
            }),
            created_target: None,
            state: RestorePendingState::Delivered,
            request_token: "t-p2c".to_string(),
            baseline: Vec::new(),
            in_flight: false,
        };
        let snapshot = vec![super::super::resume::CmdCwdEntry {
            pid: 777,
            cmd: "bin --session ses_p2c".to_string(),
            cwd: Some(proj_canon),
        }];
        let read_ok = |p: &std::path::Path| {
            std::fs::read(p).map_err(|e| NativeError::new("OC_IO", &format!("unreadable: {e}")))
        };
        let fact_at = |pid: u32| if pid == 424 { Some(5_000i64) } else { None };
        // ① 连接未就绪：前四面全过 + 探针 false ⇒ 空集（保持待确认，不冒充成功）
        let probes: StdArc<std::sync::Mutex<Vec<(u32, String)>>> =
            StdArc::new(std::sync::Mutex::new(Vec::new()));
        let probes2 = probes.clone();
        let probe_not_ready = move |pid: u32, host_port: &str| {
            probes2.lock().unwrap().push((pid, host_port.to_string()));
            false
        };
        let obs1 = restore_observer_with(&pending, &snapshot, &read_ok, &fact_at, &probe_not_ready);
        assert!(
            obs1.is_empty(),
            "进程已起、连接未就绪 ⇒ 不确认（保持待确认）：{obs1:?}"
        );
        assert_eq!(
            *probes.lock().unwrap(),
            vec![(777, "127.0.0.1:40123".to_string())],
            "连接证据与预期服务代际关联（探针目标 = 代际锚全等登记的端点）"
        );
        // ② 连接到正确服务 ⇒ 候选放行（确认）
        let obs2 =
            restore_observer_with(&pending, &snapshot, &read_ok, &fact_at, &|_pid, _hp| true);
        assert_eq!(
            obs2,
            vec![win(777, "bin --session ses_p2c")],
            "连接到正确服务（代际锚全等端点 ESTABLISHED）后才确认"
        );
        // ③ 登记面未过（serve 缺失 = 旧代际残迹）⇒ 空集且连接探针零出手
        std::fs::remove_file(&state_file).unwrap();
        let probe_calls = StdArc::new(AtomicUsize::new(0));
        let probe_calls2 = probe_calls.clone();
        let probe_counting = move |_pid: u32, _hp: &str| {
            probe_calls2.fetch_add(1, Ordering::SeqCst);
            true
        };
        let obs3 = restore_observer_with(&pending, &snapshot, &read_ok, &fact_at, &probe_counting);
        assert!(obs3.is_empty(), "登记缺失 fail-closed 不确认");
        assert_eq!(
            probe_calls.load(Ordering::SeqCst),
            0,
            "登记面未过 ⇒ 连接探针零出手（不probe旧代际）"
        );
    }

    /// IC fix round 1 · P2-C 回归二（流转级，观察核装配生产核）：「进程已起、连接
    /// 未就绪 ⇒ 保持 pending」→「连接到正确服务后才确认」。首请求出手 + 交付 +
    /// 连接未就绪（探针 false）⇒ 待确认态保留；连接翻转后复查命中核销（零额外出手
    /// ——最多一窗）。
    #[test]
    fn conn_not_ready_keeps_pending_ready_conn_confirms() {
        use crate::services::opencode_native::types::NativeError;
        fn read_ok(p: &std::path::Path) -> Result<Vec<u8>, NativeError> {
            std::fs::read(p).map_err(|e| NativeError::new("OC_IO", &format!("unreadable: {e}")))
        }
        fn fact_at(pid: u32) -> Option<i64> {
            if pid == 425 {
                Some(6_000)
            } else {
                None
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let proj = tempfile::tempdir().unwrap();
        let proj_canon = std::fs::canonicalize(proj.path()).unwrap();
        let state_file = dir.path().join("state.json");
        std::fs::write(
            &state_file,
            r#"{"id":"svc-p2c-f","version":"2.2.5","url":"http://127.0.0.1:40124","pid":425,"password":"pw"}"#,
        )
        .unwrap();
        let anchor = RuntimeKey {
            service_id: "svc-p2c-f".to_string(),
            pid: 425,
            started_at_ms: 6_000,
        };
        let snapshot: &'static Vec<super::super::resume::CmdCwdEntry> =
            Box::leak(Box::new(vec![super::super::resume::CmdCwdEntry {
                pid: 888,
                cmd: "bin --session ses_p2c_f".to_string(),
                cwd: Some(proj_canon),
            }]));
        let conn_ready = StdArc::new(std::sync::atomic::AtomicBool::new(false));
        let conn_ready2 = conn_ready.clone();
        let observer = move |p: &RestorePending| -> Vec<ObservedWindow> {
            // 内层探针闭包借用外层捕获的 conn_ready2（非 move——外层保持 Fn）
            restore_observer_with(
                p,
                &snapshot[..],
                &read_ok,
                &fact_at,
                &|_pid: u32, _hp: &str| conn_ready2.load(std::sync::atomic::Ordering::SeqCst),
            )
        };
        let dispatches = StdArc::new(AtomicUsize::new(0));
        let dispatches2 = dispatches.clone();
        let prepare = move |s: &Session| {
            Ok(super::super::resume::PreparedSpawn {
                macos_payload: format!("'bin' --session {}", s.id),
                windows_payload: format!("bin --session {}", s.id),
                effect_sig: format!("bin --session {}", s.id),
                env_secret: None,
            })
        };
        let execute = move |_p: &super::super::resume::PreparedSpawn,
                            _s: &Session,
                            _sp: &SpawnFn,
                            on_delivered: Option<&DeliveryHook<'_>>|
              -> Result<(), String> {
            dispatches2.fetch_add(1, Ordering::SeqCst);
            if let Some(hook) = on_delivered {
                hook();
            }
            Ok(())
        };
        let state_file_for_source = state_file.clone();
        let anchor_for_source = anchor.clone();
        let deps = RestoreDeps {
            prepare: leak_box(prepare),
            execute: leak_box(execute),
            spawner: leak_box(|_spec: &super::super::resume::SpawnSpec| Ok(())),
            observer: leak_box(observer),
            baseline_snapshot: leak_box(|_sig: &str| Vec::new()),
            created_target: None,
            on_confirmed: None,
            registration_source: leak_box(move || {
                (
                    Some(state_file_for_source.clone()),
                    Some(anchor_for_source.clone()),
                )
            }),
            delivery_unknown: leak_box(|e: &str| {
                crate::inject::resume::is_delivery_unknown_error(e)
            }),
        };
        let registry = empty_registry();
        let sid = "ses_p2c_f";
        let s = oc_sess(sid, proj.path());
        // 进程已起（快照含候选）+ 连接未就绪 ⇒ 出手但保持 pending（不冒充成功）
        let first = restore_with_pending_in(&s, &deps, false, &registry, WINDOW, POLL, 1_000)
            .expect("首请求受控流转");
        assert_eq!(
            first,
            RestoreFlow::Dispatched { confirmed: false },
            "连接未就绪：出手但不得确认（保持 pending）"
        );
        assert!(registry_has(&registry, sid), "待确认态保留");
        assert_eq!(dispatches.load(Ordering::SeqCst), 1, "出手恰一次");
        // 连接到正确服务（探针翻转）⇒ 复查命中核销（零额外出手，最多一窗）
        conn_ready.store(true, Ordering::SeqCst);
        let second = restore_with_pending_in(&s, &deps, false, &registry, WINDOW, POLL, 2_000)
            .expect("复查受控流转");
        assert_eq!(
            second,
            RestoreFlow::PendingConfirmed,
            "连接到正确服务后才确认"
        );
        assert!(!registry_has(&registry, sid), "核销后注册表清空");
        assert_eq!(
            dispatches.load(Ordering::SeqCst),
            1,
            "复查核销零额外出手（最多一窗）"
        );
    }
}
