//! OpenCode TUI 窗口观察（R1：看板会话匹配的事实来源）。
//!
//! 证据：`docs/engineering/evidence/opencode-native/product-completion/
//! 2026-11-10-window-session-binding-probe.md` §2/§5/§9（R0/R0b）——免配置来源全部
//! 不可用（`/api/tui/*` 不存在、`session/active` 空闲恒空、DB time 族不变、argv/env
//! 无会话、tabs.json 集合级且跨窗覆盖）；**一次配置可用** = TUI 侧只读插件按需同步读
//! `ctx.ui.router.current()`（`GET /current`），插件与 TUI 共亡（端口拒绝 = 窗口消失）。
//!
//! 职责分层（R1 附件约束 5，3s 扫描预算契约见 [`crate::monitor::session_scan`]）：
//! - **异步采集层**（本模块 [`collect_once`]/[`spawn_runtime`]）：读插件自述的端点
//!   登记文件（禁止端口扫描，约束 2）→ 身份核验（pid + boot id + 进程启动时间，约束 1）
//!   → 回环 HTTP 读 `/health` + `/current` → 写进程内快照缓存。**本层任何失败静默降级
//!   为无观察**（保持既有快照值直到新鲜度降级，绝不抛错打断扫描主链路）。
//! - **同步解析消费**（[`crate::monitor::opencode_parser`]）：`find_sessions` 保持同步
//!   `Vec<Session>`，只消费快照解析后的 [`ResolvedObservation`]——活跃卡**只能**来自
//!   「已核验观察的 sessionID」；cwd / 最新历史行 / active API 一律不得再用于产活跃卡
//!   （约束 6，历史/未读语义保留）。
//!
//! 消费语义（附件约束 1-4 + 宪法「读不到保持未知」+ fix round 1 复审 M1/M2）：
//! - `home` 路由（新鲜）→ [`ResolvedObservation::Home`]（权威「无会话」）；
//! - `session` 路由（新鲜）→ [`ResolvedObservation::Opened`]（权威「已打开」，按 ID 读
//!   DB 出卡，空闲也出卡）；
//! - 读失败/过期但**同代际且最后确认是打开** → [`ResolvedObservation::Retained`]（复审
//!   保留最后确认的记录，但不作为活跃卡来源；不重盖新鲜戳、不跨重启持久）；
//! - 其余一切（无登记 / 从未有观察 / 进程退出 / **同 PID 换代** / 身份不符 / 过期且最后
//!   确认非打开 / 未知路由形状）→ [`ResolvedObservation::Unknown`]（**未知 ≠ 首页**：
//!   不产卡也不冒充旧会话）。
//!
//! 代际守卫（复审 M1）：观察身份三元组（pid + boot + 进程启动时间）不止在采集时校验，
//! **消费时逐拍复验**——缓存锚定的 `started_at_ms` 必须与当前进程实例一致；采集器清理
//! 残迹登记时同步失效缓存；采集中途进程退出/换代的迟到读数不得写回
//!（[`generation_matches`] 闸）。
//!
//! 新鲜度（约束 4）：[`OBSERVATION_FRESH_MS`] = 15s ≈ 5 个 3s 扫描周期（采集节奏 2s，
//! 阈值给出 7 拍容错）。阈值内 = 新鲜确认态；阈值外 = 保留态（见上），永不冒充新鲜
//! ——`observed_at` 只有成功读数才推进。

use once_cell::sync::Lazy;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::adapter::AgentProcess;
use crate::services::opencode_native::binding_gate::LockedWindow;

/// 观察新鲜阈值（毫秒）：超过即从「新鲜确认态」转入「保留态」语义（附件约束 4 +
/// 复审 M2；N 按 3s 扫描节奏定为 5 个扫描周期，记录于模块文档）。
pub const OBSERVATION_FRESH_MS: i64 = 15_000;
/// 采集节奏：略快于 3s 扫描，保证解析侧每次消费都有近 2s 内的读数可取。
const COLLECT_CADENCE_MS: u64 = 2_000;
/// 单次 HTTP 读数超时（回环 + 插件进程内同步读，正常亚毫秒；1.5s 已是极宽上限）。
const FETCH_TIMEOUT: Duration = Duration::from_millis(1500);
/// 响应体上限（/health、/current 都是小 JSON；64KB 与 opencode_native 同量级）。
const MAX_BODY_BYTES: usize = 64 * 1024;
/// 端点登记目录（bridge.json 形态的 Tuvis 私有目录；插件与 Rust 读取侧共用此约定，
/// 一致性由 `plugin_asset_registration_contract_matches_rust_reader` 源码锁看守）。
const REGISTRATION_SUBDIR: &str = ".tuvis/opencode-tui";

/// 解析器消费口径的窗口观察（已把身份核验、新鲜度、读失败全部折叠进来）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedObservation {
    /// 首页（`router.current() == {"type":"home"}`）：权威「无会话」。
    Home,
    /// 会话页（`{"type":"session","sessionID":"ses_…"}`）：该窗口当前打开着这个会话。
    /// **新鲜确认态**：读数在新鲜阈值内。
    Opened {
        /// 已核验的会话 ID（活跃卡唯一来源）。
        session_id: String,
        /// 插件侧报告的目录（若有）；卡片目录仍以 DB 行为权威，本字段只作诊断。
        directory: Option<String>,
    },
    /// **保留态**（fix round 1 / 复审 M2）：读失败超阈值期间的「最后确认打开态」——
    /// 不宣告会话已关闭，也不作为活跃卡来源；不重盖新鲜戳、不跨重启持久。
    /// 恢复新鲜观察后可重新上板。
    /// 进程存活与代际一致仍由消费守卫逐拍校验（窗口真退了代际守卫立即失效保留态）。
    Retained {
        session_id: String,
        directory: Option<String>,
    },
    /// 无观察 / 进程退出 / 代际不符 / 身份不符 / 未知路由形状 / 过期且最后确认非打开。
    /// **未知 ≠ 首页**：不产卡、不冒充旧会话、不清历史（未读语义在 adapter 层保留）。
    Unknown,
}

/// 插件自述的端点登记（`~/.tuvis/opencode-tui/<pid>.json`，0600）。
/// 字段约束 2：端点**只**来自插件自述登记，兔维斯侧禁止任何端口扫描。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registration {
    pub pid: u32,
    /// 本次 TUI 进程 boot 的 UUID（插件 setup 时生成；/health 必须逐字一致）。
    pub boot: String,
    /// 插件回环 HTTP 端口（listen(0) 由内核分配）。
    pub port: u16,
}

/// 登记文件 JSON 形态（多余字段如 `at` 忽略；缺必需字段即非法）。
#[derive(Deserialize)]
struct RegistrationFile {
    pid: u32,
    boot: String,
    port: u16,
}

/// 一扇已核验窗口的观察（身份三元组 = pid + boot id + 进程启动时间，附件约束 1）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowObservation {
    pub pid: u32,
    pub boot_id: String,
    /// Tuvis 侧 sysinfo 读到的进程启动时刻（epoch 毫秒）；身份防 PID 复用。
    pub started_at_ms: i64,
    /// 读数时刻（epoch 毫秒）；新鲜度阈值以此为锚。
    pub observed_at_ms: i64,
    pub route: ResolvedObservation,
}

/// 进程内观察状态（fix round 3 合并单锁）：观察缓存 + 磁盘登记身份主张共用一把锁——
/// 消费侧一次取锁即得**同一瞬间的成对视图**，杜绝「A 主张 × B 观察」的跨代际拼视图
///（采集层写、解析层读；读写都是亚微秒级内存操作，无 IO 持锁）。
#[derive(Default)]
struct ObservationState {
    /// pid → 最近一次成功核验的观察（权威身份来源）。
    observations: HashMap<u32, WindowObservation>,
    /// pid → 最近一拍磁盘登记身份主张（boot，**不经 HTTP 核验**，只作消费侧 boot
    /// 比对与换代失效的输入）。
    claims: HashMap<u32, String>,
}

static STATE: Lazy<Mutex<ObservationState>> = Lazy::new(|| Mutex::new(ObservationState::default()));

fn state_lock() -> std::sync::MutexGuard<'static, ObservationState> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// 登记目录（生产）：`~/.tuvis/opencode-tui`。
pub fn registration_dir_for(home: &Path) -> PathBuf {
    home.join(REGISTRATION_SUBDIR)
}

/// 解析登记文件字节（pid 必须为正、boot 非空；port 由 u16 类型天然限域）。
pub(crate) fn parse_registration(bytes: &[u8]) -> Option<Registration> {
    let file: RegistrationFile = serde_json::from_slice(bytes).ok()?;
    if file.pid == 0 || file.boot.trim().is_empty() {
        return None;
    }
    Some(Registration {
        pid: file.pid,
        boot: file.boot,
        port: file.port,
    })
}

/// 身份链第 2 环（纯函数）：登记文件必须由**当前进程代际**写下——文件 mtime 不早于
/// 进程启动时刻，否则是 PID 复用前的旧代际残迹（附件约束 1，discovery.rs process_fact
/// 同款守卫思路）。sysinfo 启动时间按整秒向下取整，同代际恒有 `mtime >= started`。
pub(crate) fn verify_incarnation(file_mtime_ms: i64, process_started_at_ms: i64) -> bool {
    file_mtime_ms >= process_started_at_ms
}

/// 解析 `/current` 载荷（`{"route": …, "at": …}`；R0b §9.3 实测形态）：
/// `home` → [`ResolvedObservation::Home`]；`session` + 非空 sessionID →
/// [`ResolvedObservation::Opened`]；其余（plugin 型 / route=null / 形状漂移 / sessionID
/// 空缺）→ [`ResolvedObservation::Unknown`]（不猜）。
pub(crate) fn parse_current(current: &serde_json::Value) -> ResolvedObservation {
    let Some(route) = current.get("route") else {
        return ResolvedObservation::Unknown;
    };
    match route.get("type").and_then(|t| t.as_str()) {
        Some("home") => ResolvedObservation::Home,
        Some("session") => match route.get("sessionID").and_then(|s| s.as_str()) {
            Some(session_id) if !session_id.trim().is_empty() => ResolvedObservation::Opened {
                session_id: session_id.to_string(),
                directory: route
                    .get("directory")
                    .and_then(|d| d.as_str())
                    .map(str::to_string),
            },
            _ => ResolvedObservation::Unknown,
        },
        _ => ResolvedObservation::Unknown,
    }
}

/// 吸收一次成功读数（纯函数，锁内调用）：核验 `/health` 身份（boot + pid 逐字一致，
/// 防同用户进程抢占端口后冒名），核验通过且路由形状已知才写快照。
/// 返回是否写入（false = 拒绝；既有快照保持不变，由新鲜度兜底降级）。
pub(crate) fn absorb_observation(
    snapshot: &mut HashMap<u32, WindowObservation>,
    reg: &Registration,
    started_at_ms: i64,
    health: &serde_json::Value,
    current: &serde_json::Value,
    now_ms: i64,
) -> bool {
    let health_pid = health.get("pid").and_then(|p| p.as_u64());
    let health_boot = health.get("boot").and_then(|b| b.as_str());
    if health_pid != Some(u64::from(reg.pid)) || health_boot != Some(reg.boot.as_str()) {
        return false;
    }
    let route = parse_current(current);
    if route == ResolvedObservation::Unknown {
        // 未知路由形状（plugin 型 / 形状漂移 / 读失败透传 null）：不写快照——
        // 保留既有观察直到新鲜度降级（约束 3「不清卡」）
        return false;
    }
    snapshot.insert(
        reg.pid,
        WindowObservation {
            pid: reg.pid,
            boot_id: reg.boot.clone(),
            started_at_ms,
            observed_at_ms: now_ms,
            route,
        },
    );
    true
}

/// 快照 → 解析消费口径（同步，进程事实 + 登记身份主张注入）。
/// 身份三元组（pid + boot + started_at）在**消费侧**全等才可消费：
/// - pid：键位对位（恒成立才走到本函数）；
/// - started_at：`current_started_at_ms` 与缓存逐字一致（进程退出/换代 → Unknown）；
/// - boot：`current_claim_boot`（采集层从磁盘登记刷新的身份主张）与缓存 `boot_id`
///   逐字一致——主张换代 → Unknown，即使缓存仍在新鲜期内、也不得进入保留态
///   （fix round 2 / 复审 M1 残余：伪造或异常复用的同 pid 同启动时间新登记）；
///   主张缺失（None，登记文件被删/尚未采集到）→ 不启用 boot 轴，退回 started_at 守卫。
pub(crate) fn resolve_with(
    cached: Option<&WindowObservation>,
    now_ms: i64,
    current_started_at_ms: Option<i64>,
    current_claim_boot: Option<&str>,
) -> ResolvedObservation {
    let Some(o) = cached else {
        return ResolvedObservation::Unknown;
    };
    // 代际守卫（复审 M1）：进程退出 / 同 PID 换代 → 旧观察立即作废。
    // 启动时刻由同一 process_fact 语义（sysinfo 整秒 ×1000）产出，同代际逐字稳定。
    if !generation_matches(o.started_at_ms, current_started_at_ms) {
        return ResolvedObservation::Unknown;
    }
    // boot 轴守卫（fix round 2，复审 M1 残余）：磁盘登记主张换代（同 pid(+同启动时间)
    // 出现新 boot = 伪造/异常复用信号）→ 旧观察立即不可消费，也不得进入保留态
    if let Some(claim) = current_claim_boot {
        if o.boot_id != claim {
            return ResolvedObservation::Unknown;
        }
    }
    if now_ms.saturating_sub(o.observed_at_ms) <= OBSERVATION_FRESH_MS {
        return o.route.clone();
    }
    // 过期保留态（复审 M2）：读失败不宣告关闭，但也不再是新鲜确认——
    // 显式降为 Retained（不重盖新鲜戳：observed_at 只有成功读数才推进；不跨重启持久）
    match &o.route {
        ResolvedObservation::Opened {
            session_id,
            directory,
        } => ResolvedObservation::Retained {
            session_id: session_id.clone(),
            directory: directory.clone(),
        },
        _ => ResolvedObservation::Unknown,
    }
}

/// 迟到写回 / 消费共用的代际闸（纯函数）：采集/缓存锚定的进程启动时刻必须与**当前**
/// 进程实例逐字一致——`None`（进程已退）或不等（同 PID 新进程）都判换代。
pub(crate) fn generation_matches(collected_started_at_ms: i64, current: Option<i64>) -> bool {
    current == Some(collected_started_at_ms)
}

/// 失效某 pid 的缓存观察（窗口死亡 / 换代 / 迟到写回被拒 / 登记 boot 换代时调用）。
/// 注意：与身份主张联动的复合操作（采集层的换代失效、登记清理）应经 [`state_lock`]
/// 在同一把锁内同时改两个字段，不留可拼不一致视图的窗口。
pub(crate) fn invalidate(pid: u32) {
    state_lock().observations.remove(&pid);
}

/// 缓存观察与磁盘登记主张是否 boot 冲突（纯函数）：有缓存且 boot 不等 → 冲突
///（同 pid（+同启动时间）出现新 boot 登记 = 伪造/异常复用信号，旧观察不可再消费）。
pub(crate) fn boot_claim_conflicts_with_cached(
    cached: Option<&WindowObservation>,
    claim_boot: &str,
) -> bool {
    matches!(cached, Some(o) if o.boot_id != claim_boot)
}

/// 消费视图：claims 与 observations 的**一次捕获**产物（同一锁内复制，逐字段一致）。
#[derive(Default)]
pub(crate) struct ObservationView {
    pub observations: HashMap<u32, WindowObservation>,
    pub claims: HashMap<u32, String>,
}

/// 一致视图捕获（fix round 3）：消费侧必须一次取锁同时拿到 claims 与 observations——
/// 分两次取锁会拼出跨代际不一致视图（用 A 主张否掉换代后 B 的有效观察 → 无谓丢卡）。
pub(crate) fn consistent_view() -> ObservationView {
    let state = state_lock();
    ObservationView {
        observations: state.observations.clone(),
        claims: state.claims.clone(),
    }
}

/// 按扫描到的 TUI 进程取解析后的观察快照（同步纯函数：一致视图 + 进程事实注入）。
pub(crate) fn resolved_for_processes_with(
    processes: &[AgentProcess],
    process_fact: &dyn Fn(u32) -> Option<i64>,
    view: &ObservationView,
    now_ms: i64,
) -> Vec<(u32, ResolvedObservation)> {
    processes
        .iter()
        .map(|p| {
            let resolved = resolve_with(
                view.observations.get(&p.pid),
                now_ms,
                process_fact(p.pid),
                view.claims.get(&p.pid).map(String::as_str),
            );
            (p.pid, resolved)
        })
        .collect()
}

/// 按扫描到的 TUI 进程取解析后的观察快照（生产入口：真实进程事实 + 一致视图 + 当前时刻）。
pub fn resolved_for_processes(processes: &[AgentProcess]) -> Vec<(u32, ResolvedObservation)> {
    let now = chrono::Utc::now().timestamp_millis();
    let view = consistent_view();
    resolved_for_processes_with(processes, &process_fact, &view, now)
}

/// 生产锁定窗口查询（echo-challenge 归属写门用，H03）：返回当前**新鲜 `Opened`**
/// 持有指定会话的 TUI 窗口身份；没有则 `None`（归属不可证，fail-closed）。
///
/// 与活跃卡同一条判定链（单次取锁的一致视图 + 进程事实代际守卫 + boot 主张守卫 +
/// 新鲜阈值）：`Retained`（过期保留态）不作为锁定来源——写门只认新鲜确认。
/// 多窗口同时打开同一会话时取最小 pid（确定性；其余窗口拿不到交付控制文件、
/// 不会应答 challenge）。
pub fn fresh_opened_window(session_id: &str) -> Option<LockedWindow> {
    find_opened_window(session_id, None)
}

pub(crate) fn fresh_opened_window_for_pid(session_id: &str, pid: u32) -> Option<LockedWindow> {
    find_opened_window(session_id, Some(pid))
}

fn find_opened_window(session_id: &str, expected_pid: Option<u32>) -> Option<LockedWindow> {
    let now = chrono::Utc::now().timestamp_millis();
    let view = consistent_view();
    let mut locked: Option<LockedWindow> = None;
    for (pid, observation) in view.observations.iter() {
        if expected_pid.is_some_and(|expected| expected != *pid) {
            continue;
        }
        let resolved = resolve_with(
            Some(observation),
            now,
            process_fact(*pid),
            view.claims.get(pid).map(String::as_str),
        );
        let ResolvedObservation::Opened {
            session_id: opened, ..
        } = resolved
        else {
            continue;
        };
        if opened != session_id {
            continue;
        }
        let candidate = LockedWindow {
            pid: *pid,
            started_at_ms: observation.started_at_ms,
            boot: Some(observation.boot_id.clone()),
        };
        locked = Some(match locked {
            Some(current) if current.pid <= candidate.pid => current,
            _ => candidate,
        });
    }
    locked
}

/// 登记已经观察到同进程代际的不同 boot 时，可提前否定原插件授权。
pub(crate) fn plugin_boot_changed(window: &LockedWindow) -> bool {
    let Some(boot) = window.boot.as_deref() else {
        return false;
    };
    if process_fact(window.pid) != Some(window.started_at_ms) {
        return false;
    }
    let Some(home) = dirs::home_dir() else {
        return false;
    };
    let path = registration_dir_for(&home).join(format!("{}.json", window.pid));
    let Ok(mut file) = std::fs::File::open(&path) else {
        return false;
    };
    let Some(mtime) = file_mtime_ms(&path) else {
        return false;
    };
    if !verify_incarnation(mtime, window.started_at_ms) {
        return false;
    }
    use std::io::Read as _;
    let mut bytes = Vec::new();
    (&mut file).take(4096).read_to_end(&mut bytes).is_ok()
        && parse_registration(&bytes).is_some_and(|reg| reg.pid == window.pid && reg.boot != boot)
}

/// 生产进程事实（echo-challenge 写门的换代核对用）：pid → 启动毫秒；拿不到 =
/// `None`（进程已死，fail-closed）。与模块内私有 [`process_fact`] 同一实现。
pub fn production_process_fact(pid: u32) -> Option<i64> {
    process_fact(pid)
}

/// 登记文件是否该被清理（纯函数）：pid 无存活进程（`process_fact` = None，插件与
/// TUI 共亡后的残迹）或文件早于当前进程代际（PID 复用前的旧残迹）→ 清理。
/// 活进程的新鲜登记不动（采集还要用它）。代际判定复用 [`verify_incarnation`]。
pub(crate) fn should_prune_registration(
    file_mtime_ms: i64,
    pid: u32,
    process_fact: &dyn Fn(u32) -> Option<i64>,
) -> bool {
    match process_fact(pid) {
        None => true,
        Some(started) => !verify_incarnation(file_mtime_ms, started),
    }
}

/// 生产采集入口（异步，约定 2s 一拍）：枚举登记目录 → 身份核验 → 回环读 /health +
/// /current → 吸收快照。**任何一步失败静默降级**（不写快照 / 清理残迹文件），绝不
/// 影响扫描主链路。登记目录不存在 = 全机未装插件 = 全部未知（约束 8）。
pub async fn collect_once() {
    let Some(home) = dirs::home_dir() else {
        return;
    };
    let dir = registration_dir_for(&home);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return; // 未装插件 / 目录未建：静默降级为无观察
    };
    let client = http_client();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(mtime) = file_mtime_ms(&path) else {
            continue;
        };
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        let Some(reg) = parse_registration(&bytes) else {
            continue; // 非法登记不清理（可能是外来文件），仅跳过
        };
        if should_prune_registration(mtime, reg.pid, &process_fact) {
            // 插件与 TUI 共亡（R0b §9.4 场景 5）后的残迹 / PID 复用前的旧代际残迹：
            // 登记文件一并清理（登记是 Tuvis 私有目录，清理只碰自己的文件）；
            // 缓存与身份主张同一把锁内同步失效（复审 M1：窗口死亡/换代不得留旧观察
            // 给新窗口）
            let _ = std::fs::remove_file(&path);
            // 探针控制文件残迹一并清理（I5）：同 pid 的旧授权不得留给复用 pid 的
            // 新窗口。
            crate::services::opencode_native::binding_gate::remove_probe_control_residue(
                &home, reg.pid,
            );
            {
                let mut state = state_lock();
                state.observations.remove(&reg.pid);
                state.claims.remove(&reg.pid);
            }
            continue;
        }
        // 身份主张登记（fix round 2，复审 M1 残余）：磁盘登记的 boot 每拍刷新；
        // 与缓存 boot 冲突 = 同 pid(+同启动时间) 换代登记（伪造/异常复用）→
        // **立即**失效旧缓存——即使本轮 HTTP 读失败，旧观察也不再被消费/保留。
        // 主张登记与冲突失效在同一把锁内完成（fix round 3：不留可拼视图的窗口）
        {
            let mut state = state_lock();
            let conflicted =
                boot_claim_conflicts_with_cached(state.observations.get(&reg.pid), &reg.boot);
            state.claims.insert(reg.pid, reg.boot.clone());
            if conflicted {
                state.observations.remove(&reg.pid);
            }
        }
        let Some(started) = process_fact(reg.pid) else {
            continue;
        };
        // 读失败（端口拒绝=窗口刚退 / 超时 / 非 200）：保留既有快照（约束 3），
        // 下一拍重试；过期后由消费侧保留态语义接管（复审 M2）
        let Some(health) = fetch_json(client, reg.port, "/health").await else {
            continue;
        };
        let Some(current) = fetch_json(client, reg.port, "/current").await else {
            continue;
        };
        // 迟到写回闸（复审 M1）：采集期间进程退出/换代 → 本次读数不得写回，
        // 缓存一并失效（防同 PID 复用的新进程继承旧窗口的会话卡）
        if !generation_matches(started, process_fact(reg.pid)) {
            invalidate(reg.pid);
            continue;
        }
        let now = chrono::Utc::now().timestamp_millis();
        absorb_observation(
            &mut state_lock().observations,
            &reg,
            started,
            &health,
            &current,
            now,
        );
    }
}

/// 后台采集循环（lib.rs setup 调用）：每 [`COLLECT_CADENCE_MS`] 一拍。
/// 失败静默（collect_once 内部已降级），循环本身永不退出。
pub fn spawn_runtime() {
    tauri::async_runtime::spawn(async move {
        loop {
            collect_once().await;
            tokio::time::sleep(Duration::from_millis(COLLECT_CADENCE_MS)).await;
        }
    });
}

/// 共享 HTTP 客户端（禁重定向 + 关代理 + 超时；与 opencode_native/client.rs 同一加固口径：
/// 回环端点遇 302 一律失败而不是跟出去）。
fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_millis(500))
            .timeout(FETCH_TIMEOUT)
            .no_proxy()
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

/// 单端点 JSON 读数（生产 fetch 缝；测试用 mock TCP 服务覆盖）。
async fn fetch_json(client: &reqwest::Client, port: u16, path: &str) -> Option<serde_json::Value> {
    let url = format!("http://127.0.0.1:{port}{path}");
    let response = client.get(url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    read_json_body_capped(response).await
}

/// 响应体读取：先看声明长度，再按累计字节停下（分块/EOF 定界响应也能在上限处
/// 中止，M1）——预算检查必须发生在**读取过程中**，不得把整个响应体读完再检查，
/// 否则「无 Content-Length 的超大响应」会把观察循环拖满 FETCH_TIMEOUT。
/// （与 opencode_native/client.rs::read_body_capped 同一加固口径。）
async fn read_json_body_capped(mut response: reqwest::Response) -> Option<serde_json::Value> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_BODY_BYTES as u64)
    {
        return None;
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if body.len() + chunk.len() > MAX_BODY_BYTES {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).ok()
}

/// 生产进程事实：pid 的启动时刻（epoch 毫秒；拿不到 = 进程已死，fail-closed）。
/// 与 services::opencode_native::projection::production_process_fact 同款实现
///（sysinfo 启动秒 × 1000；0/负值拒绝）。
fn process_fact(pid: u32) -> Option<i64> {
    let pid = sysinfo::Pid::from_u32(pid);
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    let process = system.process(pid)?;
    let started_secs = i64::try_from(process.start_time()).ok()?;
    if started_secs <= 0 {
        return None;
    }
    started_secs.checked_mul(1000)
}

fn file_mtime_ms(path: &Path) -> Option<i64> {
    let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
    let secs = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    i64::try_from(secs.as_millis()).ok()
}

/// TUI 侧只读插件资产（装载形态 `<config>/opencode/plugins/tuvis-route-observer/tui.mjs`，
/// v2.2.5 目录自动发现、不改 config；R0b §9.2 实测）。只读路由 + 自述端点登记，
/// 与本模块的登记读取/身份核验共用同一文件契约。
///
/// 当前仅测试消费（契约锁测试看守两端漂移）：生产安装入口（写插件目录 / 注册 / 卸载）
/// 属 R4 安装 UI 范围（附件约束 7「剩余限制」），接线时移除 cfg(test) 改为安装写入源。
#[cfg(test)]
pub(crate) const TUI_PLUGIN_ASSET: &str = include_str!("opencode_tui_route.mjs");

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PID: u32 = 56019;
    const BOOT: &str = "0f6e2c1e-3a44-4c8b-9a10-5e2f1d3b7a90";
    const PORT: u16 = 51401;
    const STARTED_AT_MS: i64 = 1_700_000_000_000;

    fn registration_bytes() -> Vec<u8> {
        json!({ "pid": PID, "boot": BOOT, "port": PORT, "at": STARTED_AT_MS + 5_000 })
            .to_string()
            .into_bytes()
    }

    fn registration() -> Registration {
        parse_registration(&registration_bytes()).expect("合法登记必须可解析")
    }

    fn health_ok() -> serde_json::Value {
        json!({ "boot": BOOT, "pid": PID })
    }

    fn current_home() -> serde_json::Value {
        json!({ "route": { "type": "home" }, "at": 1 })
    }

    fn current_session(sid: &str) -> serde_json::Value {
        json!({ "route": { "type": "session", "sessionID": sid }, "at": 2 })
    }

    fn stored(pid: u32, observed_at_ms: i64, route: ResolvedObservation) -> WindowObservation {
        WindowObservation {
            pid,
            boot_id: BOOT.to_string(),
            started_at_ms: STARTED_AT_MS,
            observed_at_ms,
            route,
        }
    }

    // ==== RED-2 断言（GREEN 实现后转绿）====

    /// /current 路由解析三态：home → Home；session → Opened；plugin 型/缺 route/空
    /// sessionID → Unknown（未知形状不猜，R0b §9.6 边界 3）
    #[test]
    fn parse_current_routes_home_session_and_unknown_shapes() {
        assert_eq!(parse_current(&current_home()), ResolvedObservation::Home);
        assert_eq!(
            parse_current(&current_session("ses_abc")),
            ResolvedObservation::Opened {
                session_id: "ses_abc".to_string(),
                directory: None
            }
        );
        // session 带 directory（若上游未来携带）→ 原样携带，仍是 Opened
        assert_eq!(
            parse_current(&json!({
                "route": {"type": "session", "sessionID": "ses_d", "directory": "/w"},
                "at": 3
            })),
            ResolvedObservation::Opened {
                session_id: "ses_d".to_string(),
                directory: Some("/w".to_string())
            }
        );
        for shape in [
            json!({ "route": { "type": "plugin", "id": "x" }, "at": 4 }),
            json!({ "route": null, "at": 5 }),
            json!({ "at": 6 }),
            json!({ "route": { "type": "session" }, "at": 7 }),
            json!({ "route": { "type": "session", "sessionID": "  " }, "at": 8 }),
        ] {
            assert_eq!(
                parse_current(&shape),
                ResolvedObservation::Unknown,
                "未知形状必须保守降级：{shape}"
            );
        }
    }

    /// 登记解析：合法形态通过；缺字段 / pid=0 / 空 boot / 非法 JSON / 越界端口全拒
    #[test]
    fn parse_registration_validates_fields_and_rejects_garbage() {
        let reg = registration();
        assert_eq!(reg.pid, PID);
        assert_eq!(reg.boot, BOOT);
        assert_eq!(reg.port, PORT);
        for bad in [
            json!({ "pid": PID, "boot": BOOT }).to_string(), // 缺 port
            json!({ "pid": 0, "boot": BOOT, "port": PORT }).to_string(), // pid=0
            json!({ "pid": PID, "boot": "  ", "port": PORT }).to_string(), // 空 boot
            json!({ "pid": PID, "port": PORT }).to_string(), // 缺 boot
            json!({ "boot": BOOT, "port": PORT }).to_string(), // 缺 pid
            json!({ "pid": PID, "boot": BOOT, "port": 70000 }).to_string(), // 越界端口
            "not json".to_string(),
        ] {
            assert!(
                parse_registration(bad.as_bytes()).is_none(),
                "非法登记必须拒绝：{bad}"
            );
        }
    }

    /// 身份链（约束 1）：登记文件 mtime 早于进程启动 = 上一代际残迹（PID 复用）→ 拒绝；
    /// 同代际（mtime ≥ 启动时刻）→ 通过
    #[test]
    fn verify_incarnation_rejects_stale_registration_pid_reuse() {
        assert!(
            !verify_incarnation(STARTED_AT_MS - 1, STARTED_AT_MS),
            "文件早于进程启动 = 旧代际残迹，不得复活"
        );
        assert!(verify_incarnation(STARTED_AT_MS, STARTED_AT_MS));
        assert!(verify_incarnation(STARTED_AT_MS + 5_000, STARTED_AT_MS));
    }

    /// 吸收核验（约束 1/2）：/health 的 boot 与 pid 必须与登记逐字一致——同用户进程
    /// 抢占端口（冒名）时拿不到 boot UUID，必须拒绝且不写快照
    #[test]
    fn absorb_rejects_boot_or_pid_mismatch() {
        let mut snapshot = HashMap::new();
        let reg = registration();
        let squatter_health = json!({ "boot": "other-boot", "pid": PID });
        assert!(!absorb_observation(
            &mut snapshot,
            &reg,
            STARTED_AT_MS,
            &squatter_health,
            &current_session("ses_a"),
            1_000
        ));
        let wrong_pid_health = json!({ "boot": BOOT, "pid": 999 });
        assert!(!absorb_observation(
            &mut snapshot,
            &reg,
            STARTED_AT_MS,
            &wrong_pid_health,
            &current_session("ses_a"),
            1_000
        ));
        assert!(snapshot.is_empty(), "身份不符不得写快照");
    }

    /// 吸收正例：session / home 都写快照（home 也是权威读数——「无会话」必须能上板）
    #[test]
    fn absorb_writes_fresh_observation_for_opened_and_home() {
        let mut snapshot = HashMap::new();
        let reg = registration();
        assert!(absorb_observation(
            &mut snapshot,
            &reg,
            STARTED_AT_MS,
            &health_ok(),
            &current_session("ses_a"),
            1_000
        ));
        assert_eq!(
            snapshot.get(&PID).map(|o| o.route.clone()),
            Some(ResolvedObservation::Opened {
                session_id: "ses_a".to_string(),
                directory: None
            })
        );
        let written = snapshot.get(&PID).unwrap();
        assert_eq!(written.boot_id, BOOT);
        assert_eq!(written.started_at_ms, STARTED_AT_MS);
        assert_eq!(written.observed_at_ms, 1_000);
        assert_eq!(written.pid, PID);

        // 随后读到 home：覆盖为 Home（权威「无」）
        assert!(absorb_observation(
            &mut snapshot,
            &reg,
            STARTED_AT_MS,
            &health_ok(),
            &current_home(),
            2_000
        ));
        assert_eq!(
            snapshot.get(&PID).map(|o| o.route.clone()),
            Some(ResolvedObservation::Home)
        );
    }

    /// 路由形状漂移（Unknown）：不改写快照——保留旧观察直到新鲜度降级（约束 3 不清卡）
    #[test]
    fn absorb_unknown_route_keeps_previous_observation() {
        let mut snapshot = HashMap::new();
        snapshot.insert(PID, stored(PID, 1_000, ResolvedObservation::Home));
        let reg = registration();
        assert!(!absorb_observation(
            &mut snapshot,
            &reg,
            STARTED_AT_MS,
            &health_ok(),
            &json!({ "route": { "type": "plugin", "id": "x" }, "at": 9 }),
            2_000
        ));
        assert_eq!(
            snapshot.get(&PID).map(|o| o.observed_at_ms),
            Some(1_000),
            "未知形状不得覆盖既有观察"
        );
    }

    /// 新鲜确认态（约束 4 的「确认」面）：阈值内原样透传（边界 = 仍新鲜）；
    /// 无缓存 = Unknown
    #[test]
    fn resolve_passes_fresh_route_within_threshold() {
        let fresh_opened = stored(PID, 10_000, opened_a());
        assert_eq!(
            resolve_with(
                Some(&fresh_opened),
                10_000 + OBSERVATION_FRESH_MS,
                Some(STARTED_AT_MS),
                None
            ),
            opened_a(),
            "阈值边界（=）仍算新鲜确认"
        );
        let fresh_home = stored(PID, 10_000, ResolvedObservation::Home);
        assert_eq!(
            resolve_with(Some(&fresh_home), 11_000, Some(STARTED_AT_MS), None),
            ResolvedObservation::Home,
            "新鲜 Home 权威透传"
        );
        assert_eq!(
            resolve_with(None, 0, Some(STARTED_AT_MS), None),
            ResolvedObservation::Unknown,
            "无缓存 = 未知"
        );
    }

    /// 复审 M1 主判据：同 PID、新启动时间（进程换代）、旧缓存仍在有效期内
    /// → 不得沿用旧卡（Unknown）；进程退出同理；只有同代际才透传确认态
    #[test]
    fn same_pid_new_generation_does_not_inherit_cached_observation() {
        let fresh = stored(PID, 10_000, opened_a()); // observed 10s，缓存新鲜
        let now = 10_000 + 1_000;
        assert_eq!(
            resolve_with(Some(&fresh), now, Some(STARTED_AT_MS + 60_000), None),
            ResolvedObservation::Unknown,
            "同 PID 新启动时间（换代）→ 旧观察作废，即使仍在新鲜期内"
        );
        assert_eq!(
            resolve_with(Some(&fresh), now, None, None),
            ResolvedObservation::Unknown,
            "进程已退 → 观察作废（确认的退出移除卡）"
        );
        assert_eq!(
            resolve_with(Some(&fresh), now, Some(STARTED_AT_MS), None),
            opened_a(),
            "同代际 → 新鲜确认态照常透传"
        );
    }

    /// 复审 M2 主判据：过期（持续读失败超阈值）时保留**最后确认状态**——
    /// 确认过的 Opened 以 Retained 保留卡（读失败不宣告关闭）；
    /// 确认过的 Home / 从未有观察 → Unknown（保守维持无卡，不冒充权威 home-now）
    #[test]
    fn stale_confirmed_opened_is_retained_but_home_and_missing_are_not() {
        let confirmed_opened = stored(PID, 10_000, opened_a());
        let stale_now = 10_000 + OBSERVATION_FRESH_MS + 1;
        assert_eq!(
            resolve_with(
                Some(&confirmed_opened),
                stale_now,
                Some(STARTED_AT_MS),
                None
            ),
            ResolvedObservation::Retained {
                session_id: "ses_a".to_string(),
                directory: None
            },
            "持续读失败超阈值：确认过的 Opened 保留卡，不得当作已关闭"
        );
        let confirmed_home = stored(PID, 10_000, ResolvedObservation::Home);
        assert_eq!(
            resolve_with(Some(&confirmed_home), stale_now, Some(STARTED_AT_MS), None),
            ResolvedObservation::Unknown,
            "过期 + 最后确认 Home：保守维持无卡（不是权威 home-now）"
        );
        assert_eq!(
            resolve_with(
                Some(&confirmed_home),
                stale_now,
                Some(STARTED_AT_MS + 60_000),
                None
            ),
            ResolvedObservation::Unknown,
            "过期 + 换代：双重作废"
        );
    }

    /// 复审 M2 发布行为：恢复读数回到确认状态——保留态之后一次成功读数立即接管
    ///（成功 Home → 无卡；成功 Opened(B) → 确认 B）
    #[test]
    fn recovery_returns_to_confirmed_state_after_retention() {
        let mut snapshot = HashMap::new();
        snapshot.insert(PID, stored(PID, 10_000, opened_a()));
        let stale_now = 10_000 + OBSERVATION_FRESH_MS + 1;
        assert_eq!(
            resolve_with(snapshot.get(&PID), stale_now, Some(STARTED_AT_MS), None),
            ResolvedObservation::Retained {
                session_id: "ses_a".to_string(),
                directory: None
            }
        );
        // 恢复：确认 Home → 卡移除
        assert!(absorb_observation(
            &mut snapshot,
            &registration(),
            STARTED_AT_MS,
            &health_ok(),
            &current_home(),
            stale_now + 2_000
        ));
        assert_eq!(
            resolve_with(
                snapshot.get(&PID),
                stale_now + 2_000,
                Some(STARTED_AT_MS),
                None
            ),
            ResolvedObservation::Home,
            "恢复读数 → 回到确认态（Home 权威移除卡）"
        );
        // 再恢复：确认 Opened(B) → 确认 B
        assert!(absorb_observation(
            &mut snapshot,
            &registration(),
            STARTED_AT_MS,
            &health_ok(),
            &current_session("ses_b"),
            stale_now + 4_000
        ));
        assert_eq!(
            resolve_with(
                snapshot.get(&PID),
                stale_now + 4_000,
                Some(STARTED_AT_MS),
                None
            ),
            ResolvedObservation::Opened {
                session_id: "ses_b".to_string(),
                directory: None
            },
            "恢复读数 → 回到确认态（确认 B）"
        );
    }

    /// 迟到写回闸（复审 M1）：采集锚定的启动时刻必须与当前进程实例逐字一致；
    /// 进程退出（None）或换代（新启动时间）→ 迟到结果不得写回
    #[test]
    fn generation_matches_guards_late_write_back() {
        assert!(generation_matches(STARTED_AT_MS, Some(STARTED_AT_MS)));
        assert!(
            !generation_matches(STARTED_AT_MS, Some(STARTED_AT_MS + 60_000)),
            "同 PID 新进程 = 换代，迟到结果不得写回"
        );
        assert!(!generation_matches(STARTED_AT_MS, None), "进程已退不得写回");
    }

    /// 缓存失效（复审 M1）：窗口死亡 / 换代 / 迟到写回被拒时移除缓存条目
    #[test]
    fn invalidate_removes_cached_observation() {
        let _guard = global_state_guard();
        set_snapshot_for_test(vec![stored(PID, 1_000, opened_a())]);
        assert!(
            state_lock().observations.contains_key(&PID),
            "前置：缓存在场"
        );
        invalidate(PID);
        assert!(
            !state_lock().observations.contains_key(&PID),
            "失效后缓存条目必须移除"
        );
        set_snapshot_for_test(Vec::new());
    }

    fn opened_a() -> ResolvedObservation {
        ResolvedObservation::Opened {
            session_id: "ses_a".to_string(),
            directory: None,
        }
    }

    /// 解析消费缝（进程事实注入）：按进程 pid 对位出观察；缓存缺失的进程 = Unknown
    #[test]
    fn resolved_for_processes_maps_pid_and_defaults_unknown() {
        let now = 10_000 + 1_000;
        let procs = [
            AgentProcess {
                pid: 11,
                cpu_usage: 0.0,
                cwd: None,
                exe: None,
                form: crate::session::ProcessForm::Cli,
            },
            AgentProcess {
                pid: 22,
                cpu_usage: 0.0,
                cwd: None,
                exe: None,
                form: crate::session::ProcessForm::Cli,
            },
        ];
        let fact = |pid: u32| (pid == 11).then_some(STARTED_AT_MS);
        let view = ObservationView {
            observations: HashMap::from([(11u32, stored(11, 10_000, opened_a()))]),
            claims: HashMap::from([(11u32, BOOT.to_string())]),
        };
        let resolved = resolved_for_processes_with(&procs, &fact, &view, now);
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].0, 11);
        assert_eq!(resolved[0].1, opened_a(), "同代际且新鲜 → 确认态");
        assert_eq!(resolved[1].0, 22);
        assert_eq!(resolved[1].1, ResolvedObservation::Unknown, "无缓存 = 未知");
    }

    /// 消费链端到端（复审 M1）：同 PID 换代 / 进程死亡在**发布口径**直接 Unknown，
    /// 即使缓存仍在新鲜期内——登记 mtime 纯函数之外，缓存消费链也要有守卫
    #[test]
    fn resolved_for_processes_rejects_generation_mismatch_and_dead_process() {
        let now = 10_000 + 1_000;
        let procs = [AgentProcess {
            pid: 11,
            cpu_usage: 0.0,
            cwd: None,
            exe: None,
            form: crate::session::ProcessForm::Cli,
        }];
        let view = ObservationView {
            observations: HashMap::from([(11u32, stored(11, 10_000, opened_a()))]),
            claims: HashMap::from([(11u32, BOOT.to_string())]),
        };
        // 同 PID、新启动时间（旧缓存仍在有效期内）
        let recycled = |pid: u32| (pid == 11).then_some(STARTED_AT_MS + 60_000);
        let resolved = resolved_for_processes_with(&procs, &recycled, &view, now);
        assert_eq!(
            resolved[0].1,
            ResolvedObservation::Unknown,
            "PID 复用 + 新鲜期内旧缓存 → 不得沿用旧卡"
        );
        // 进程死亡
        let dead = |_pid: u32| None;
        let resolved = resolved_for_processes_with(&procs, &dead, &view, now);
        assert_eq!(
            resolved[0].1,
            ResolvedObservation::Unknown,
            "进程死亡 → 观察作废（确认的退出移除卡）"
        );
    }

    /// 登记残迹清理判据：pid 死（process_fact=None）→ 清；文件早于进程启动（PID 复用）
    /// → 清；活进程的新鲜登记 → 保留
    #[test]
    fn should_prune_registration_rules() {
        let fact = |pid: u32| (pid == PID).then_some(STARTED_AT_MS);
        assert!(
            should_prune_registration(STARTED_AT_MS, 42, &fact),
            "pid 已死 → 清"
        );
        assert!(
            should_prune_registration(STARTED_AT_MS - 1, PID, &fact),
            "早于当前代际的残迹 → 清"
        );
        assert!(
            !should_prune_registration(STARTED_AT_MS + 1_000, PID, &fact),
            "活进程的新鲜登记 → 保留"
        );
    }

    // ==== Fix round 2（复审 M1 残余：boot 轴进入消费与写回）====

    /// 复审 M1 残余主判据（复审指定场景）：同 PID、**同启动时间**、登记 boot 换代
    /// （伪造/异常复用）→ 旧观察不得被消费，也不得进入保留态（不无限 Retained）；
    /// 主张一致或主张缺失时语义不变
    #[test]
    fn same_started_time_different_boot_claim_stops_consumption_and_retention() {
        let confirmed = stored(PID, 10_000, opened_a()); // boot = BOOT，缓存新鲜
        let now_fresh = 11_000;
        let stale_now = 10_000 + OBSERVATION_FRESH_MS + 1;
        // 新鲜期内：boot 主张换代 → 拒绝消费
        assert_eq!(
            resolve_with(
                Some(&confirmed),
                now_fresh,
                Some(STARTED_AT_MS),
                Some("forged-boot")
            ),
            ResolvedObservation::Unknown,
            "同 pid 同启动时间、boot 换代 → 不得沿用旧观察（三元组不全等）"
        );
        // 过期 + boot 换代 → 不得 Retained（不无限保留）
        assert_eq!(
            resolve_with(
                Some(&confirmed),
                stale_now,
                Some(STARTED_AT_MS),
                Some("forged-boot")
            ),
            ResolvedObservation::Unknown,
            "boot 换代后过期不得进入保留态"
        );
        // 主张一致 → 原有语义不变（新鲜确认 / 过期保留）
        assert_eq!(
            resolve_with(Some(&confirmed), now_fresh, Some(STARTED_AT_MS), Some(BOOT)),
            opened_a(),
            "主张一致 → 确认态照常"
        );
        assert_eq!(
            resolve_with(Some(&confirmed), stale_now, Some(STARTED_AT_MS), Some(BOOT)),
            ResolvedObservation::Retained {
                session_id: "ses_a".to_string(),
                directory: None
            },
            "主张一致 → 过期保留态照常"
        );
        // 主张缺失（登记被删/未采集到）→ 不启用 boot 轴，退回 started_at 守卫语义
        assert_eq!(
            resolve_with(Some(&confirmed), now_fresh, Some(STARTED_AT_MS), None),
            opened_a(),
            "主张缺失 → boot 轴不启用"
        );
    }

    /// boot 冲突判据（纯函数）：有缓存且 boot 不等 → 冲突；主张一致 / 无缓存 → 不冲突
    #[test]
    fn boot_claim_conflicts_with_cached_rules() {
        let cached = stored(PID, 1_000, opened_a());
        assert!(
            boot_claim_conflicts_with_cached(Some(&cached), "forged-boot"),
            "boot 不同 → 冲突"
        );
        assert!(
            !boot_claim_conflicts_with_cached(Some(&cached), BOOT),
            "boot 相同 → 不冲突"
        );
        assert!(
            !boot_claim_conflicts_with_cached(None, "whatever"),
            "无缓存 → 无从冲突（消费侧本来就 Unknown）"
        );
    }

    /// 复审指定流程：同 pid(+启动时间) 新 boot 登记出现 → 采集层立即失效旧缓存；
    /// 随后新读数失败（无 absorb）→ 无缓存可消费：不消费旧观察、不无限 Retained
    #[test]
    fn boot_claim_change_invalidates_cache_and_no_infinite_retention() {
        let _guard = global_state_guard();
        set_snapshot_for_test(vec![stored(PID, 1_000, opened_a())]); // 旧 boot = BOOT
                                                                     // 采集层换代处置（与 collect_once 同款单锁复合）：主张登记 + 冲突判定 + 失效
        {
            let mut state = state_lock();
            let conflicted =
                boot_claim_conflicts_with_cached(state.observations.get(&PID), "forged-boot");
            assert!(conflicted, "前置：缓存 boot 与磁盘主张冲突");
            state.claims.insert(PID, "forged-boot".to_string());
            state.observations.remove(&PID);
        }
        assert!(
            !state_lock().observations.contains_key(&PID),
            "换代后旧缓存必须移除"
        );
        // 新读数失败（本测试不做 absorb）→ 消费侧无缓存：Unknown（不是 Retained）
        let stale_now = 1_000 + OBSERVATION_FRESH_MS + 1;
        let resolved = resolve_with(
            state_lock().observations.get(&PID),
            stale_now,
            Some(STARTED_AT_MS),
            Some("forged-boot"),
        );
        assert_eq!(
            resolved,
            ResolvedObservation::Unknown,
            "换代失效 + 新读数失败 → 不消费旧观察、不无限 Retained"
        );
        // 登记清理路径同步删除身份主张
        state_lock().claims.remove(&PID);
        assert!(
            !state_lock().claims.contains_key(&PID),
            "登记清理路径必须同步删除身份主张"
        );
        set_snapshot_for_test(Vec::new());
    }

    /// 发布口径全链（复审 M1 残余）：消费与写回两侧核对完整身份三元组——
    /// pid（键位）+ started_at（活进程）+ boot（磁盘登记主张）任一不符 → Unknown
    #[test]
    fn resolved_for_processes_checks_full_identity_triple() {
        let now = 10_000 + 1_000;
        let procs = [AgentProcess {
            pid: 11,
            cpu_usage: 0.0,
            cwd: None,
            exe: None,
            form: crate::session::ProcessForm::Cli,
        }];
        let same_started = |pid: u32| (pid == 11).then_some(STARTED_AT_MS);
        let obs_a = HashMap::from([(11u32, stored(11, 10_000, opened_a()))]); // boot = BOOT
                                                                              // boot 轴冲突（同 pid 同启动时间、主张换代）
        let view = ObservationView {
            observations: obs_a.clone(),
            claims: HashMap::from([(11u32, "forged-boot".to_string())]),
        };
        let resolved = resolved_for_processes_with(&procs, &same_started, &view, now);
        assert_eq!(
            resolved[0].1,
            ResolvedObservation::Unknown,
            "三元组 boot 轴不符 → 发布 Unknown"
        );
        // 主张一致 → 确认态
        let view = ObservationView {
            observations: obs_a.clone(),
            claims: HashMap::from([(11u32, BOOT.to_string())]),
        };
        let resolved = resolved_for_processes_with(&procs, &same_started, &view, now);
        assert_eq!(resolved[0].1, opened_a(), "三元组全等 → 确认态");
        // 主张缺失 → boot 轴不启用
        let view = ObservationView {
            observations: obs_a,
            claims: HashMap::new(),
        };
        let resolved = resolved_for_processes_with(&procs, &same_started, &view, now);
        assert_eq!(resolved[0].1, opened_a(), "主张缺失 → started_at 守卫语义");
    }

    // ==== Fix round 3（消费视图一致性）====

    /// 复审 fix round 3 确定性交错序列：主张复制后、快照复制前发生换代——
    /// 一致视图语义下任何一拍都只能看到「同一瞬间的 (claims, observations) 对」：
    /// 拍 1 (A 主张, A 观察) → 出卡；换代拍 (B 主张, 无观察) → 诚实 Unknown（≤1 采集拍，
    /// M1 要求旧 boot 立即失效的固有代价）；拍 3 (B 主张, B 观察) → **最终产卡**。
    /// 「A 主张 × B 观察」的拼视图在单次捕获下**构造上不存在**——卡片永不因内部
    /// 快照不一致而消失
    #[test]
    fn consistent_view_sequence_final_card_survives_generation_transition() {
        let now = 10_000 + 1_000;
        let procs = [AgentProcess {
            pid: 11,
            cpu_usage: 0.0,
            cwd: None,
            exe: None,
            form: crate::session::ProcessForm::Cli,
        }];
        let same_started = |pid: u32| (pid == 11).then_some(STARTED_AT_MS);
        let obs_a = HashMap::from([(11u32, stored(11, 10_000, opened_a()))]); // boot = BOOT
        let obs_b = HashMap::from([(
            11u32,
            WindowObservation {
                boot_id: "new-boot".to_string(),
                ..stored(11, 20_000, opened_a())
            },
        )]);

        // 拍 1：A 主张 + A 观察（同代际新鲜）→ 出卡
        let view = ObservationView {
            observations: obs_a.clone(),
            claims: HashMap::from([(11u32, BOOT.to_string())]),
        };
        assert_eq!(
            resolved_for_processes_with(&procs, &same_started, &view, now)[0].1,
            opened_a(),
            "换代前：确认态出卡"
        );

        // 换代拍：B 主张已登记、B 观察尚未核验（读失败/在途）→ 诚实 Unknown
        let view = ObservationView {
            observations: HashMap::new(),
            claims: HashMap::from([(11u32, "new-boot".to_string())]),
        };
        assert_eq!(
            resolved_for_processes_with(&procs, &same_started, &view, now)[0].1,
            ResolvedObservation::Unknown,
            "换代过渡拍：无同代际观察可消费（旧 boot 已按 M1 失效）"
        );

        // 拍 3：B 主张 + B 观察（新代际核验成功）→ **最终产卡**，不因内部不一致继续丢卡
        let view = ObservationView {
            observations: obs_b.clone(),
            claims: HashMap::from([(11u32, "new-boot".to_string())]),
        };
        assert_eq!(
            resolved_for_processes_with(&procs, &same_started, &view, now)[0].1,
            opened_a(),
            "换代完成：新观察必须产卡——不得被陈旧 A 主张否掉（fix round 3 主判据）"
        );
    }

    /// 结构锁（fix round 3）：生产入口的一致视图必须**单次取锁**同时捕获 claims 与
    /// observations——`consistent_view()` 内部不得出现第二次取锁，入口本体不得各自取锁
    ///（两锁拼接会产出任一瞬间都不存在过的「A 主张 × B 观察」组合 → 无谓丢卡一拍）
    #[test]
    fn resolved_for_processes_captures_view_under_single_lock() {
        let src = include_str!("opencode_observation.rs");
        let body = function_body(src, "pub(crate) fn consistent_view()");
        assert!(
            body.contains("state_lock()"),
            "一致视图必须经合并状态单次取锁：{body}"
        );
        assert!(
            !body.contains("claims_lock()") && !body.contains("snapshot_lock()"),
            "consistent_view 内不得分两次取锁（跨代际拼视图根源）：{body}"
        );
        let entry = function_body(src, "pub fn resolved_for_processes(");
        assert!(
            entry.contains("consistent_view()"),
            "生产入口必须取一致视图：{entry}"
        );
        assert!(
            !entry.contains("claims_lock()") && !entry.contains("snapshot_lock()"),
            "生产入口本体不得各自取锁（与 consistent_view 内部捕获叠加仍是两段式）：{entry}"
        );
    }

    /// 从源码提取函数体（按花括号配对；签名必须唯一命中）。家族先例：
    /// commands/session.rs r1_mutation_scan_tests 同款 helper
    fn function_body<'a>(src: &'a str, signature: &str) -> &'a str {
        let start = src.find(signature).unwrap_or_else(|| {
            panic!("函数签名缺失：{signature}");
        });
        let open = src[start..].find('{').expect("函数体缺失") + start;
        let mut depth = 0usize;
        for (i, c) in src[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return &src[open..=open + i];
                    }
                }
                _ => {}
            }
        }
        panic!("函数体未闭合：{signature}");
    }

    /// 端点登记契约（约束 2，源码锁）：插件资产与 Rust 读取侧共用同一目录与 JSON 键，
    /// 任何一端漂移都会让观察链整体失效——本锁逼两端同步改
    #[test]
    fn plugin_asset_registration_contract_matches_rust_reader() {
        // 装载目录约定（R0b §9.2：入口文件名必须是 tui.mjs）
        assert!(TUI_PLUGIN_ASSET.contains("opencode-tui"), "登记目录约定");
        assert!(
            TUI_PLUGIN_ASSET.contains("0o600"),
            "登记文件必须 0600（bridge.json 形态）"
        );
        assert!(TUI_PLUGIN_ASSET.contains("0o700"), "登记目录必须 0700");
        assert!(TUI_PLUGIN_ASSET.contains("127.0.0.1"), "HTTP 只绑回环");
        assert!(
            TUI_PLUGIN_ASSET.contains("router.current"),
            "只读路由观察（R0b 读数通道）"
        );
        assert!(
            TUI_PLUGIN_ASSET.contains("sessionID"),
            "会话路由键名（上游 router 形态）"
        );
        // 登记 JSON 键锁：插件写出的对象与 Registration 字段（pid/boot/port）逐键一致
        assert!(
            TUI_PLUGIN_ASSET
                .contains("JSON.stringify({ pid: process.pid, boot: BOOT, port, at: Date.now() })"),
            "登记 JSON 键（pid/boot/port）必须与 Registration 字段一致"
        );
        // H03 echo-challenge 探针通道锁：控制文件名 / 交付键 / ack 协议形态两端一致
        assert!(
            TUI_PLUGIN_ASSET.contains("\".probe\""),
            "探针控制文件后缀必须与 write_probe_control 的 PROBE_CONTROL_SUFFIX 一致"
        );
        assert!(
            TUI_PLUGIN_ASSET.contains("expectPid"),
            "控制文件必须携带 expectPid（插件自检身份，多 TUI 只有被锁定窗口应答）"
        );
        assert!(
            TUI_PLUGIN_ASSET.contains("chal:") && TUI_PLUGIN_ASSET.contains("ack:"),
            "challenge/ack 标题协议前缀必须与 binding_gate 同形"
        );
        assert!(
            TUI_PLUGIN_ASSET.contains("session.update"),
            "ack 写通道 = fresh client.session.update（仅探针标题，唯一写动作）"
        );
        // I5/I6 授权与 ACK 生命周期锁：绑定轮次 nonce、进程代际 expectBoot、有效期
        // deadlineMs、ACK 串行化、未知结果保守去重（answered 只进不出）。
        assert!(
            TUI_PLUGIN_ASSET.contains("deadlineMs")
                && TUI_PLUGIN_ASSET.contains("expectBoot")
                && TUI_PLUGIN_ASSET.contains("current.nonce === nonce"),
            "插件必须复核授权轮次/代际/有效期（控制文件授权生命周期）"
        );
        assert!(
            TUI_PLUGIN_ASSET.contains("ackBusy"),
            "ACK 任务必须串行化（旧请求迟到不得覆盖新轮槽位）"
        );
        // I6 残余修正：授权复核必须出现在**最后一次异步等待之后**（读侧 + 写侧各
        // 一次），槽位读取与真正写之间不得再有任何 await。
        assert!(
            TUI_PLUGIN_ASSET
                .matches("authorizedFor(control, nonce)")
                .count()
                >= 2,
            "授权复核必须双点存在（读侧 + 写前最后一次异步之后）"
        );
        // 轮次隔离断言：守卫按 nonce 判旧轮 ack（Rust 侧 evaluate_observation
        // NonceMismatch 既有用例 + 插件只答当前授权轮）。
        assert!(
            TUI_PLUGIN_ASSET.contains("current.nonce === nonce"),
            "插件只答当前授权轮 nonce（轮次隔离）"
        );
        assert!(
            !TUI_PLUGIN_ASSET.contains("answered.delete") && !TUI_PLUGIN_ASSET.contains("clear()"),
            "写结果未知必须保守去重：answered 集合只进不出，绝不无界重写/清空遗忘"
        );
        // 只读边界（附件约束 7 + H03 收窄）：不得出现导航调用形态（router.navigate(...)）、
        // 不得创建会话（探针会话只由兔维斯写门创建）
        assert!(
            !TUI_PLUGIN_ASSET.contains("navigate("),
            "插件必须只读：不得出现导航调用形态"
        );
        assert!(
            !TUI_PLUGIN_ASSET.contains("createSession"),
            "插件不得写会话数据"
        );
        assert!(
            !TUI_PLUGIN_ASSET.contains("event.subscribe")
                && !TUI_PLUGIN_ASSET.contains("events.on"),
            "插件不得订阅事件（既裁决：tick 轮询是唯一检测通道）"
        );
        // Rust 读取侧目录与插件写入目录同源
        let home = Path::new("/Users/example");
        assert_eq!(
            registration_dir_for(home),
            home.join(".tuvis/opencode-tui"),
            "登记目录两侧必须一致"
        );
    }

    /// 生产 fetch 缝（mock 回环 TCP）：/health、/current 可读可解析；404/断连 → None；
    /// 超限响应体 → None
    #[tokio::test]
    async fn production_fetch_reads_health_and_current_with_bounds() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let peeked = String::from_utf8_lossy(&buf);
                if peeked.contains("GET /health") {
                    let body = format!("{{\"boot\":\"{BOOT}\",\"pid\":{PID}}}");
                    let _ = sock
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            )
                            .as_bytes(),
                        )
                        .await;
                } else if peeked.contains("GET /current") {
                    let body = json!({ "route": { "type": "session", "sessionID": "ses_live" } })
                        .to_string();
                    let _ = sock
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            )
                            .as_bytes(),
                        )
                        .await;
                } else {
                    let _ = sock
                        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                }
                let _ = sock.shutdown().await;
            }
        });

        let client = http_client();
        let health = fetch_json(client, port, "/health")
            .await
            .expect("mock /health 必须可读");
        assert_eq!(health["boot"], BOOT);
        let current = fetch_json(client, port, "/current")
            .await
            .expect("mock /current 必须可读");
        assert_eq!(parse_current(&current).session_id(), Some("ses_live"));
        assert!(
            fetch_json(client, port, "/nope").await.is_none(),
            "404 必须 None（不产观察）"
        );
        assert!(
            fetch_json(client, 1, "/health").await.is_none(),
            "端口拒绝（TUI 已退）必须 None"
        );
        server.abort();
        let _ = server.await;
    }

    /// M1：无 Content-Length 的超限响应（靠 EOF 定界、服务端不主动关流）——
    /// 读取必须在**累计字节越限处中止**，而不是把整个响应体读完（等 EOF/超时）再检查。
    /// 旧实现（先 `bytes()` 全读、后比长度）会把观察循环拖满 FETCH_TIMEOUT。
    #[tokio::test]
    async fn oversize_body_without_content_length_aborts_read() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf).await;
            // 无 Content-Length、无 chunked：HTTP/1.1 的 EOF 定界响应
            let _ = sock
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n",
                )
                .await;
            // 持续写超过 64KB 且不关流：新实现在累计越限处中止（对端断开使写入失败退出）；
            // 旧实现会一路读到 FETCH_TIMEOUT 才以 None 收场。
            let chunk = [b'a'; 4096];
            loop {
                if sock.write_all(&chunk).await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let _ = sock.shutdown().await;
        });

        let client = http_client();
        let started = std::time::Instant::now();
        let result = tokio::time::timeout(
            Duration::from_millis(700),
            fetch_json(client, port, "/health"),
        )
        .await;
        assert!(
            result.is_ok(),
            "读取必须在 700ms 内返回（不得等满 FETCH_TIMEOUT 才发现超限）"
        );
        assert!(result.unwrap().is_none(), "超限响应必须 None（不产观察）");
        assert!(
            started.elapsed() < Duration::from_millis(700),
            "越限中止必须发生在完整读取之前"
        );
        server.abort();
        let _ = server.await;
    }

    // ==== 测试注入缝 ====

    /// 全局快照/主张表是进程级共享态，触及它们的用例必须共用同一把锁串行化
    ///（家族先例：monitor::test_env::HOME_ENV_LOCK；并行 teardown 清空缓存会互踩他测）
    fn global_state_guard() -> std::sync::MutexGuard<'static, ()> {
        static SNAPSHOT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        SNAPSHOT_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set_snapshot_for_test(entries: Vec<WindowObservation>) {
        state_lock().observations = entries.into_iter().map(|o| (o.pid, o)).collect();
    }

    impl ResolvedObservation {
        /// 测试便利：Opened / Retained 的 session_id 投影
        fn session_id(&self) -> Option<&str> {
            match self {
                ResolvedObservation::Opened { session_id, .. }
                | ResolvedObservation::Retained { session_id, .. } => Some(session_id),
                _ => None,
            }
        }
    }
}
