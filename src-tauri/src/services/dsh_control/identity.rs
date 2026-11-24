//! Desktop 安装定位与唯一实例发现（计划 §2.3：`identity` 单源解析）。
//!
//! 职责（T02）：
//! * [`resolve_installation`]：从常规安装根（数据驱动探测，不硬编码盘符/端口）
//!   找官方 Desktop 安装，给出**安装包自带 CLI**（npm 全局 `dsh` 不是它）；
//!   Desktop 未运行也可用（安装管理的目标单源）。
//! * [`discover`]：唯一运行实例定位——cmdline 双特征（`dsh-desktop-host` 包令牌
//!   **且** profile 令牌，Win 报告 §1.2「唯二双特征判定」的强口径）筛候选，
//!   多于一个 → `DSH_TARGET_AMBIGUOUS`（禁止 monitor 的「取第一个」，见
//!   `monitor::dsh::find_dsh_desktop_host_pid` 的展示口径）；唯一候选再做
//!   「PID+启动时刻+exe+profile+监听属主」组合核验。
//!
//! 证据锚（docs/engineering/evidence/dsh-native-rpc/received/）：
//!
//! * Win 报告 §1.1/§1.2：安装路径 `D:\Program Files\Deepseek-Harness\`（非系统盘
//!   加空格）、自带 CLI `resources\runtime\cli\bin\dsh.cmd`、宿主 argv 含
//!   `dsh-desktop-host\lib\index.js` 与 `C:\Users\<u>\.dsh\profiles\desktop`、
//!   监听 `127.0.0.1:<动态端口>` 且属主 = 宿主 PID（`--port 0` 内核挑选）。
//! * Mac 报告 §1/§3.2：bundle `/Applications/DeepSeek Harness.app`、自带 CLI
//!   `Contents/Resources/runtime/cli/bin/dsh`、profile argv 同上。
//!
//! 只读纪律（验收④）：本模块只触碰安装面与 profile 标记文件，绝不打开
//! `sessions/`、`storages/` 等会话内容（dsh_identity_test 有路径触碰断言）。
//! 全部文件/进程访问经 [`DshEnvironment`] 注入适配器，生产实现由后续任务接线。

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::error::{codes, DshError};
use super::instances::{instance_id_for, normalize_profile, HostKind, InstanceCandidate};
use super::model::{DshEnvironment, DshListener, DshProcess, LocalHost};

/// 安装包描述文件读取预算（计划全局约束「候选单文件 48 MiB」参考值）。真实
/// 包级 SHA-256 指纹属未测项（夹具 README「未测项」），下轮授权采样补齐时由
/// 文件源实现升级为预算内采样哈希——本入口语义（哈希源 = 安装包描述文件）不变。
const PACKAGE_DESCRIPTOR_BUDGET_BYTES: u64 = 48 * 1024 * 1024;

/// 官方产品可执行文件名（安装锚点数据——Win 报告 §1.1 / Mac 报告 §1 实测；
/// 安装**目录**名不设限，中文/空格均可，见验收③）。
const WIN_EXE_NAME: &str = "DeepSeek Harness.exe";
const MAC_EXE_NAME: &str = "DeepSeek Harness";

/// desktop profile 相对 DSH home 的路径（`~/.dsh/profiles/desktop`）。
const PROFILE_REL: &[&str] = &["profiles", "desktop"];
/// profile 官方标记文件（Win 报告 §1.3：各 profile 独占 `package.json` 等）。
const PROFILE_MARKER: &str = "package.json";

/// 自带 CLI 相对资源目录路径（Win 报告 §1.1 `runtime\cli\bin\dsh.cmd`；
/// Mac 报告 §3.2 `Resources/runtime/cli/bin/dsh`）。
const WIN_CLI_REL: &[&str] = &["runtime", "cli", "bin", "dsh.cmd"];
const MAC_CLI_REL: &[&str] = &["runtime", "cli", "bin", "dsh"];

// ============================================================
// 公共入口（§2.3 签名固定）
// ============================================================

/// Desktop 安装解析产物（§2.3 `resolve_installation` 返回；本机事实，不进远程 DTO）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopInstallation {
    /// 安装根（mac = `.app` bundle 目录；win = 安装目录）。
    pub root: PathBuf,
    /// Desktop 可执行文件。
    pub executable: PathBuf,
    /// 资源目录（mac = `Contents/Resources`；win = `<安装目录>\resources`）。
    pub resources_dir: PathBuf,
    /// 安装包自带 CLI（相对安装根拼装；**npm 全局 dsh 永远不是它**）。
    pub cli_path: PathBuf,
    /// 安装包**描述文件**（`LocalHost.package_sha256` 的哈希源）。
    ///
    /// 产出的是「描述符级快速身份」，**不是**整包 `app.asar` 内容指纹；内容指纹
    /// 的权威是 `contract::read_report`（`evidence.package_sha256`）。两者不可混用。
    pub package_descriptor: PathBuf,
    /// desktop profile 目录（`<DSH home>/profiles/desktop`）。
    pub profile_dir: PathBuf,
    /// 平台词（macos / windows）。
    pub platform: String,
    /// 桌面版本（T02 不读；管理任务接 CFBundleShortVersionString / FileVersion）。
    pub version: Option<String>,
}

/// 唯一运行实例定位（§2.3 签名固定；H04 起为 [`discover_detailed`] 之上的兼容
/// 委托）。审查 I5：**唯一性决策只在本入口与 resolve 面**——本入口口径：
/// * 恰有一个**已核验**候选且无其他宿主观测 → 返回它（单实例行为不变）；
/// * 多宿主观测（多已核验候选 / 已核验 + 未核验或不可归属宿主并存）→
///   `DSH_TARGET_AMBIGUOUS`（多宿主必须显式选择，不静默取第一项，也不静默
///   吞掉不可归属的宿主）；
/// * 只有未核验宿主 → `DSH_INSTANCE_UNVERIFIED`；
/// * 无宿主 → 保留旧「安装/profile 门 → 未运行」错误分档；单进程核验失败
///   （如监听缺失）如实上抛原错误。
pub fn discover(env: &DshEnvironment) -> Result<LocalHost, DshError> {
    let d = discover_detailed(env)?;
    let verified: Vec<&InstanceCandidate> = d.candidates.iter().filter(|c| c.verified).collect();
    let unverified_count = d.candidates.len() - verified.len();
    // 多宿主口径（审查 I5）：已核验候选 + 核验失败的 Desktop 宿主 + 未核验/
    // 不可归属宿主，合计 >1 即歧义（旧「双 Desktop 即歧义」语义由此逐字保留）。
    let observed_hosts =
        verified.len() + d.unresolved_desktop_hosts + d.unattributable_web_hosts + unverified_count;
    match verified.len() {
        1 if observed_hosts == 1 => Ok(verified[0].host.clone()),
        n if n > 0 || observed_hosts > 1 => Err(DshError::new(
            codes::TARGET_AMBIGUOUS,
            format!(
                "{n} verified dsh host instance(s) (+{unverified_count} unverified, +{} unattributable web, +{} unresolved desktop) require explicit selection",
                d.unattributable_web_hosts, d.unresolved_desktop_hosts
            ),
        )),
        // 无已核验候选且无多宿主：
        _ => {
            // 单进程核验失败如实上抛（保留旧错误分档：监听缺失/未初始化）。
            if let Some(e) = d.desktop_error.or(d.web_error) {
                return Err(e);
            }
            if !d.candidates.is_empty() || d.unattributable_web_hosts > 0 {
                return Err(DshError::new(
                    codes::INSTANCE_UNVERIFIED,
                    "dsh host(s) observed but none with verified home attribution; awaiting trusted plugin verification",
                ));
            }
            // 未运行必须先过「安装+profile」门：未安装/未初始化要如实分开报
            // （brief GREEN：分开未安装/未初始化/未运行/实例多义）。
            resolve_installation(env)?;
            Err(DshError::new(
                codes::HOST_NOT_RUNNING,
                "no live dsh host instance found",
            ))
        }
    }
}

/// 仅用于首次受限证明的原始 Web 进程事实，不含 profile 或稳定实例 ID。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawWebHost {
    pub process: DshProcess,
    pub listeners: Vec<DshListener>,
}

/// 一次双宿主发现的完整事实（审查 I5：候选发现与唯一性决策分离——本结构只
/// 报事实，不选边；唯一性/多宿主歧义由 [`discover`] 与
/// `InstanceRegistry::resolve_selected` 决策）。
#[derive(Debug, Clone)]
pub struct Discovery {
    /// 具有已识别 profile 的实例候选；未证明的 Web 只出现在 raw_web_hosts。
    pub candidates: Vec<InstanceCandidate>,
    /// 未证明 profile 的 Web 原始进程，不可用于业务写入。
    pub raw_web_hosts: Vec<RawWebHost>,
    /// 观测到但未证明 profile 的 Web 进程数，多宿主歧义决策消费。
    pub unattributable_web_hosts: usize,
    /// 观测到但**核验失败**的 Desktop 宿主数（Desktop 形态进程在但 profile/监听
    /// 核验不过——仍是观测到的宿主，多宿主歧义决策必须计入，不得静默吞掉；
    /// 旧 discover 的「双 Desktop 即歧义」语义由此保留）。
    pub unresolved_desktop_hosts: usize,
    /// 单一 Desktop 宿主进程的核验失败（无可证候选时由消费方如实上抛）。
    pub desktop_error: Option<DshError>,
    /// 单一 Web 宿主进程的核验失败（同上）。
    pub web_error: Option<DshError>,
}

/// 双宿主活体发现·完整事实（H04 §3.1；审查 I1/I5 重构）。
///
/// * **Desktop**：T02 双特征判定（argv 含 `dsh-desktop-host` 且带
///   `profiles/desktop` 令牌），profile 权威在 argv（Win 报告 §1.2）——每个合格
///   宿主逐一核验并产 `verified = true` 候选；**不再在本层做唯一性截断**（双
///   Desktop 同时返回，歧义决策归 resolve 面）。
/// * **Web**：先逐一保留 PID、启动身份和监听的原始事实；**再由 OS 启动环境观测
///   尝试归属**（[`web_candidates_from_raw`]，与 [`restricted_web_host`] 同一映射
///   单源）。观测成功且该实例 ID 唯一 → 产 `verified = true` 候选；不可观测
///   （平台无实现 / 读不到环境）或同实例多进程 → 只留原始事实，**绝不猜 home**。
///
/// 为什么要这一步（P1-1b）：Web 候选若永远只出现在 `raw_web_hosts`，生产链上
/// `discover_all` / `resolve_selected` 就永远看不到 Web 实例——用户在设置里能
/// 选中并批准，实际消息/附件/子任务全被归属门拒（批准只是 auth 域内部结论）。
/// * 伪造进程/监听拒绝：两类候选都走监听属主核验（假监听/非回环都拒）。
pub fn discover_detailed(env: &DshEnvironment) -> Result<Discovery, DshError> {
    let processes = env.processes.list_processes();
    let desktops: Vec<&DshProcess> = processes
        .iter()
        .filter(|p| is_desktop_host_cmdline(&p.cmdline))
        .collect();
    let webs: Vec<&DshProcess> = processes
        .iter()
        .filter(|p| is_web_host_process(p))
        .collect();
    let mut desktop_error: Option<DshError> = None;
    let mut web_error: Option<DshError> = None;
    let mut candidates: Vec<InstanceCandidate> = Vec::new();
    let mut unresolved_desktop_hosts: usize = 0;
    for host in &desktops {
        match desktop_candidate(env, host) {
            Ok(candidate) => candidates.push(candidate),
            Err(e) => {
                // 核验失败的 Desktop 宿主：计数进多宿主歧义决策（不得静默吞掉）；
                // 单宿主失败另存错误供 discover / resolve 面如实上抛（旧分档）。
                unresolved_desktop_hosts += 1;
                if desktops.len() == 1 {
                    desktop_error = Some(e);
                }
            }
        }
    }
    // 原始发现不以当前进程的 env.home 猜测 Web profile；每个 PID 保留独立监听事实。
    let listeners = if webs.is_empty() {
        Vec::new()
    } else {
        env.processes.listening_sockets()
    };
    let raw_web_hosts: Vec<RawWebHost> = webs
        .iter()
        .map(|host| RawWebHost {
            process: (*host).clone(),
            listeners: listeners
                .iter()
                .filter(|l| l.owner_pid == host.pid && is_loopback_authority(&l.authority))
                .cloned()
                .collect(),
        })
        .collect();
    // Web 归属晋升：能经 OS 启动环境证明的唯一映射 → 真候选（verified = true）。
    // 证不出来的一律留在 raw 里（`unattributable_web_hosts` 继续为多宿主歧义记账）。
    let web = web_candidates_from_raw(env, &raw_web_hosts);
    candidates.extend(web.candidates);
    // `web_error` 的语义：**该实例存在但归属/监听不可证**，供单进程场景如实上抛
    // （与 Desktop 单宿主同分档）。只在「有宿主前景」时挂它——没有回环监听的 web
    // 进程不是可连接的宿主（没有 authority 可代理），不能被当成「观测到的宿主」
    // 把 `HOST_NOT_RUNNING` 抬成歧义/未核验。
    let live_web_hosts = raw_web_hosts
        .iter()
        .filter(|raw| !raw.listeners.is_empty())
        .count();
    if web_error.is_none() && live_web_hosts == 1 && web.error.is_some() {
        web_error = web.error;
    }
    // 歧义记账同样只算「可连接的宿主」：无监听的进程既不能被代理也无从证明，
    // 把它计进多宿主会把「没有合格宿主」错报成「多宿主歧义」。
    let unattributable_web_hosts = web.raw.iter().filter(|r| !r.listeners.is_empty()).count();
    Ok(Discovery {
        candidates,
        raw_web_hosts: web.raw,
        unattributable_web_hosts,
        unresolved_desktop_hosts,
        desktop_error,
        web_error,
    })
}

/// 官方封装 Web CLI 的退出门识别（**只用于 `observe_desktop_exit` 的排除**）：
/// 宿主包内的 `dsh-desktop-host` 入口 + 独立 `web` 命令词，且**不是** Desktop
/// 宿主形态（不带 `profiles/desktop` 令牌）。Windows 实测 argv 把 `web` 放在
/// profile 旗标之后，mac 打包形态的紧邻判定（[`is_web_host_cmdline`]）漏这种，
/// 于是它会被 Desktop 宽网抓成「不可归属宿主」永久阻塞卸载。连接侧归属规则不受
/// 本函数影响。
fn is_packaged_web_cli(cmdline: &[String]) -> bool {
    let has_host_entry = cmdline.iter().any(|token| {
        let token = token.replace('\\', "/");
        token.contains("dsh-desktop-host") && token.ends_with("cli.js")
    });
    let has_web_word = cmdline.iter().any(|token| token == "web");
    // Desktop 宿主进程自身也含 `dsh-desktop-host`，但它带 profile 令牌——先排除。
    has_host_entry && has_web_word && argv_profile_dir(cmdline).is_none()
}

/// Desktop 退出观察（安装管理专用，`host_gate_for` 消费）：把「进程形态事实」
/// 与「可连接活体」分开，避免把「监听已消失」误判成「目标已退出」——宿主退出
/// 过程中先关监听、后结束进程，两者不同拍（用户现场：等退出期间被报
/// `DSH_HOST_LISTENER_MISSING` 直接失败）。
///
/// 判据只读原始进程表（零副作用），**不要求监听在场**（监听是远程连接证据，
/// 不是「进程是否还活着」的证据）：
/// * 桌面宿主形态进程的 profile 令牌**可核验**（目录在、官方标记在场）→ 能派生
///   稳定实例 ID：等于目标 → `TargetAlive`（继续等）；是别的实例 → 与本作业无关，
///   不阻塞；
/// * profile 令牌**不可核验**（无令牌 / 目录不在 / 缺官方标记）→ `Unproven`：
///   不能排除它就是目标，也**不得**按词法路径哈希成「别的实例」放行——那会让
///   一个 profile 缺失、归属未证的目标被当成无关宿主，卸载照做；
/// * 没有任何桌面宿主形态进程 → `Gone`：已证退出。
///
/// `Unproven` 的收口由调用方按发现错误细分：真正的发现失败（profile 未初始化等）
/// 仍走原安全分支；只有「多台不可归属」这类歧义才保守等待。
///
/// **只用于安装管理的退出门**：远程连接的活体校验仍走 [`discover`] /
/// [`discover_detailed`]（监听必须是当代理性证据，不因本函数放宽）。
pub fn observe_desktop_exit(
    env: &DshEnvironment,
    target_instance_id: &str,
) -> DesktopExitObservation {
    // 退出门的最终判据是「目标已退出」，不能读 1 秒缓存快照下结论：宿主可能在
    // 缓存窗口内刚启动（快照仍是启动前的空表），据此放行会让本作业在活体上移包。
    // 声明 Gone 前强制取一次新鲜采样。
    let processes = env.processes.list_processes_fresh();
    let mut unproven = false;
    for process in &processes {
        // Web 形态不是 Desktop 宿主，不能阻塞 Desktop 卸载。`is_web_host_cmdline`
        // 只认 mac 打包形态的紧邻 `cli.js web`；Windows 官方封装把 `web` 放在
        // profile 旗标**之后**（实测 `… cli.js --profile X --from-default-profile
        // web --no-open`，见 `docs/engineering/evidence/dsh-native-rpc/received/
        // windows-report.md` §3.2），需要额外按「desktop-host cli.js + web 词」识别。
        // 注意：这里只做**退出门的排除**，不修改连接侧归属规则。
        let os_cmdline: Vec<OsString> = process.cmdline.iter().map(OsString::from).collect();
        if is_web_host_cmdline(&process.cmdline) || is_packaged_web_cli(&process.cmdline) {
            continue;
        }
        if !crate::monitor::dsh::cmdline_is_dsh_desktop_host(&os_cmdline) {
            continue;
        }
        // 只有**可核验**的 profile 令牌才能证明归属：目录在 + 官方标记在场，
        // 此时 `normalize_profile` 走 canonicalize，实例 ID 才是可证明的派生值。
        let Some(profile_dir) = argv_profile_dir(&process.cmdline).map(PathBuf::from) else {
            unproven = true;
            continue;
        };
        if !profile_initialized(env, &profile_dir) {
            // 目录缺失/未初始化：**不可证**，不得按词法路径当「别的实例」。
            unproven = true;
            continue;
        }
        let instance = instance_id_for(HostKind::Desktop, &profile_dir);
        if instance == target_instance_id {
            return DesktopExitObservation::TargetAlive;
        }
        // 已证是别的实例：与本次卸载无关，不阻塞。
    }
    if unproven {
        DesktopExitObservation::Unproven
    } else {
        DesktopExitObservation::Gone
    }
}

/// Desktop 退出观察结果（见 [`observe_desktop_exit`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopExitObservation {
    /// 目标实例的宿主进程仍在本机（监听可能已消失）——仍在退出中。
    TargetAlive,
    /// 存在归属不可证的 Desktop 宿主形态进程——目标退出不可证，交调用方按
    /// 发现错误细分（安全分支 / 保守等待）。
    Unproven,
    /// 不存在目标实例、也不存在不可归属的 Desktop 宿主——已证退出。
    Gone,
}

/// Web 原始事实 → 候选的归属晋升结果。
struct WebAttribution {
    /// 归属已被 OS 启动环境证明的候选（每个实例 ID 至多一个）。
    candidates: Vec<InstanceCandidate>,
    attributed: Vec<(String, DshProcess, super::model::ProcessEnvironmentEvidence)>,
    /// 归属证不出的原始事实（含同实例多进程的歧义）。
    raw: Vec<RawWebHost>,
    /// 仅当「有 Web 进程但一个都没晋升」时有值（单进程场景的可诊断错误）。
    error: Option<DshError>,
}

/// 把原始 Web 进程映射成实例候选——**唯一映射口径**，与 [`restricted_web_host`]
/// 同源（后者是单实例版消费方）。
///
/// 证明材料：进程的**启动环境**（`DSH_HOME` > `HOME/.dsh`，官方优先级）+ 该 home
/// 下的 `profiles/web` 派生实例 ID。这不是「按当前进程的 env.home 猜」——读的是
/// 宿主进程自己的启动环境，且观测前后复核进程身份（[`observed_environment`] 内部
/// 已做，pid 换代/读取失败均返回 None）。
///
/// 拒绝情形（一律留在 raw，不产候选）：
/// * 环境不可观测（非 macOS 无实现 / 权限不足 / 进程已换代）；
/// * 环境里没有可用 home（既无 `DSH_HOME` 也无 `HOME`）；
/// * 监听不是唯一回环监听（假监听/多监听无法钉死 authority）；
/// * 同一实例 ID 命中多个进程（歧义——writer/peer 不可证）。
fn web_candidates_from_raw(env: &DshEnvironment, raw_hosts: &[RawWebHost]) -> WebAttribution {
    let mut by_instance: std::collections::BTreeMap<
        String,
        Vec<(
            &RawWebHost,
            PathBuf,
            super::model::ProcessEnvironmentEvidence,
        )>,
    > = std::collections::BTreeMap::new();
    let mut unproven: Vec<RawWebHost> = Vec::new();
    let mut first_error: Option<DshError> = None;
    for raw in raw_hosts {
        // 唯一回环监听才能钉死 authority（多监听/无监听不可代理）。
        if raw.listeners.len() != 1 {
            unproven.push(raw.clone());
            first_error.get_or_insert_with(|| {
                DshError::new(
                    codes::HOST_LISTENER_MISSING,
                    "web host must own exactly one loopback listener for attribution",
                )
            });
            continue;
        }
        let Some(observed) = env.environments.observed_environment(raw.process.pid) else {
            // 不可观测 ≠ 未设置：平台无实现（Windows）或读不到 → 一律不猜。
            unproven.push(raw.clone());
            first_error.get_or_insert_with(|| {
                DshError::new(
                    codes::INSTANCE_UNVERIFIED,
                    "web host startup environment is not observable; home attribution unavailable",
                )
            });
            continue;
        };
        // 观测前后必须是同一内核代际，并与发现时的进程事实一致。
        // 读取完成后再次核对唯一监听仍属于该进程。
        let processes = env.processes.list_processes_fresh();
        let listeners = env.processes.listening_sockets_fresh();
        let process_matches = observed.matches_process(&raw.process);
        let after = env.environments.observed_environment(raw.process.pid);
        let owned: Vec<_> = listeners
            .iter()
            .filter(|listener| {
                listener.owner_pid == raw.process.pid && is_loopback_authority(&listener.authority)
            })
            .collect();
        if !process_matches
            || after.as_ref() != Some(&observed)
            || !processes.iter().any(|process| process == &raw.process)
            || owned.len() != 1
            || owned[0].authority != raw.listeners[0].authority
        {
            unproven.push(raw.clone());
            first_error.get_or_insert_with(|| {
                DshError::new(
                    codes::INSTANCE_UNVERIFIED,
                    "web process identity or listener changed during observation",
                )
            });
            continue;
        }
        let Some(home) = observed.derive_home() else {
            unproven.push(raw.clone());
            first_error.get_or_insert_with(|| {
                DshError::new(
                    codes::INSTANCE_UNVERIFIED,
                    "web host startup home is not proven",
                )
            });
            continue;
        };
        let profile = home.join("profiles").join("web");
        let instance_id = instance_id_for(HostKind::Web, &profile);
        by_instance
            .entry(instance_id)
            .or_default()
            .push((raw, profile, observed));
    }
    let mut candidates = Vec::new();
    let mut attributed = Vec::new();
    for (instance_id, mut matched) in by_instance {
        if matched.len() != 1 {
            // 同实例多进程：归属不可证，全部退回原始事实并记歧义。
            unproven.extend(matched.into_iter().map(|(raw, _, _)| raw.clone()));
            continue;
        }
        let (raw, profile, observed) = matched.pop().expect("len == 1");
        let generation = observed
            .identity_before
            .native_generation
            .as_ref()
            .expect("verified generation");
        attributed.push((instance_id.clone(), raw.process.clone(), observed.clone()));
        let normalized = normalize_profile(&profile);
        let mut host = build_local_host(
            env,
            &raw.process,
            &raw.listeners[0],
            &profile,
            &normalized,
            &instance_id,
        );
        let mut hasher = Sha256::new();
        hasher.update(b"dsh-web-native-epoch-v1");
        hasher.update(host.epoch.to_le_bytes());
        hasher.update(generation.unique_id.to_le_bytes());
        hasher.update(generation.pid_version.to_le_bytes());
        let digest = hasher.finalize();
        host.epoch = u64::from_le_bytes(digest[..8].try_into().expect("sha256 prefix"));
        candidates.push(InstanceCandidate {
            instance_id: instance_id.clone(),
            kind: HostKind::Web,
            display_name: display_name(&profile),
            host,
            // OS 启动环境 + 派生 ID 唯一命中 = home 归属被正面证明。
            verified: true,
            profile_dir: normalized,
        });
    }
    WebAttribution {
        candidates,
        attributed,
        raw: unproven,
        error: first_error,
    }
}

pub(crate) enum WebTargetObservation {
    // Box 压小变体尺寸（clippy::large_enum_variant）：Live 携带完整进程环境证据，
    // 与零大小的 Offline 悬殊；证据本体只在观察核验时按引用使用。
    Live(DshProcess, Box<super::model::ProcessEnvironmentEvidence>),
    Offline,
}

pub(crate) fn observe_web_target(
    env: &DshEnvironment,
    id: &str,
) -> Result<WebTargetObservation, DshError> {
    let processes = env.processes.list_processes_fresh();
    let listeners = env.processes.listening_sockets_fresh();
    let raw: Vec<_> = processes
        .into_iter()
        .filter(is_web_host_process)
        .map(|process| {
            let owned = listeners
                .iter()
                .filter(|l| l.owner_pid == process.pid && is_loopback_authority(&l.authority))
                .cloned()
                .collect();
            RawWebHost {
                process,
                listeners: owned,
            }
        })
        .collect();
    let result = web_candidates_from_raw(env, &raw);
    if let Some((_, process, evidence)) = result
        .attributed
        .into_iter()
        .find(|(instance, _, _)| instance == id)
    {
        return Ok(WebTargetObservation::Live(process, Box::new(evidence)));
    }
    if !result.raw.is_empty() {
        return Err(DshError::new(
            codes::INSTANCE_UNVERIFIED,
            "web host attribution is inconclusive",
        ));
    }
    Ok(WebTargetObservation::Offline)
}

pub(crate) fn target_observation_still_valid(
    env: &DshEnvironment,
    process: &DshProcess,
    evidence: &super::model::ProcessEnvironmentEvidence,
) -> bool {
    let processes = env.processes.list_processes_fresh();
    let matches = processes.iter().any(|observed| observed == process);
    let evidence_matches =
        env.environments.observed_environment(process.pid).as_ref() == Some(evidence);
    matches && evidence_matches
}

/// §3.1 契约签名（`discover_all -> Vec<InstanceCandidate>`）：返回全部候选；
/// 无候选且存在单进程核验错误时如实上抛该错误（保持「无候选不静默」语义）。
pub fn discover_all(env: &DshEnvironment) -> Result<Vec<InstanceCandidate>, DshError> {
    let d = discover_detailed(env)?;
    if d.candidates.is_empty() {
        if let Some(e) = d.desktop_error {
            return Err(e);
        }
        if let Some(e) = d.web_error {
            return Err(e);
        }
    }
    Ok(d.candidates)
}

/// 实例作用域解析·完整候选（H05 审查 C1）：在双宿主发现事实里找**该实例**的
/// 当代活体候选，**连同归属核验标志一并交还**——授权面（握手/凭据交付）必须
/// 自行消费 `verified`：`verified = false`（Web 候选，profile 锚在 env.home 而非
/// 运行进程的实际 home）不得作为任何绑定/交付的依据。
///
/// * 恰一个候选 → 该候选（`verified` 可能是 false——消费方按用途裁决）；
/// * 同实例多个活体进程 → `DSH_TARGET_AMBIGUOUS`（writer/peer 在竞争者间不可证，
///   绝不取首项——H04 N1 同款）；
/// * 无候选但有该形态的单进程核验错误（如监听缺失）→ 如实上抛该错误（比笼统的
///   「未运行」更可诊断）；
/// * 实例不在场 → `DSH_HOST_NOT_RUNNING`。
pub fn discover_instance_candidate(
    env: &DshEnvironment,
    instance_id: &str,
) -> Result<InstanceCandidate, DshError> {
    let d = discover_detailed(env)?;
    let matches: Vec<&InstanceCandidate> = d
        .candidates
        .iter()
        .filter(|c| c.instance_id == instance_id)
        .collect();
    match matches.len() {
        1 => Ok(matches[0].clone()),
        0 => {
            if let Some(e) = d.desktop_error.or(d.web_error) {
                return Err(e);
            }
            Err(DshError::new(
                codes::HOST_NOT_RUNNING,
                format!("instance {instance_id} is not live"),
            ))
        }
        _ => Err(DshError::new(
            codes::TARGET_AMBIGUOUS,
            format!(
                "instance {instance_id} has {} distinct live processes (pids {:?}); instance ambiguity",
                matches.len(),
                matches.iter().map(|c| c.host.pid).collect::<Vec<_>>()
            ),
        )),
    }
}

/// 实例作用域解析（活体宿主；兼容包装——授权面请用
/// [`discover_instance_candidate`] 连同 `verified` 一并消费）。
pub fn discover_instance(env: &DshEnvironment, instance_id: &str) -> Result<LocalHost, DshError> {
    Ok(discover_instance_candidate(env, instance_id)?.host)
}

/// Web 形态进程是否在场（H05 审查 I5：web 目标的活体门按**原始进程表**判定——
/// Web 候选锚在 env.home，不能定位任意 home 的宿主；形态在场 = 有 web 宿主在跑，
/// 是当前证据面下「等待退出」能依据的边界，如实申报）。
pub fn web_host_alive(env: &DshEnvironment) -> bool {
    env.processes
        .list_processes()
        .iter()
        .any(is_web_host_process)
}

// ============================================================
// 经核验的 web CLI 源（H05 审查 I5-r2）
// ============================================================

/// 官方 dsh CLI 入口文件名形态（npm bin shim / Desktop 自带 wrapper）。
const CLI_ENTRY_NAMES: [&str; 3] = ["dsh", "dsh.cmd", "dsh.exe"];

/// 词法绝对性（Windows 形态路径在 POSIX 主机上 std Path 不识别 `'\'`）：
/// `/…`、`\\…`（UNC）或 `X:\…`（盘符）视为绝对。
fn lexically_absolute(token: &str) -> bool {
    token.starts_with('/')
        || token.starts_with("\\")
        || (token.as_bytes().get(1) == Some(&b':') && token.as_bytes().get(2).is_some())
}

/// 词法文件名（`'/'` 与 `'\'` 都算分隔），小写比较。
fn lexical_file_name(token: &str) -> Option<String> {
    let trimmed = token.trim_end_matches(['/', '\\']);
    let name = trimmed.rsplit(['/', '\\']).next()?;
    if name.is_empty() {
        return None;
    }
    Some(name.to_lowercase())
}

/// 官方 CLI 入口形态：文件名 = `dsh` / `dsh.cmd` / `dsh.exe`（同名非 DSH 命令
/// 靠**同安装树锚**拒绝，见 [`dsh_package_anchor_exists`]）。
fn cli_entry_shape(token: &str) -> bool {
    lexical_file_name(token).is_some_and(|name| CLI_ENTRY_NAMES.iter().any(|entry| name == *entry))
}

/// argv 里的 dsh 入口判定（与 monitor 单源 `has_dsh` 同形：精确 `dsh` 或
/// `/dsh`、`\dsh` 结尾——它就是让该进程呈 web 形态的那一个令牌）。
fn is_dsh_entry_token(token: &str) -> bool {
    token == "dsh" || token.ends_with("/dsh") || token.ends_with("\\dsh")
}

/// 包锚读预算（package.json / shim 内容都是小文本）。
const CLI_METADATA_READ_BUDGET: u64 = 64 * 1024;
/// 官方 dsh 包名（package.json 的 name 字段 + shim 内容引用形态）。
const DSH_PACKAGE_NAME: &str = "@deepseek-ai/dsh";

/// 包锚核验（H05 审查 I5-r4①）：`node_modules/@deepseek-ai/dsh/package.json` 的
/// **实际目标**（resolve_actual_path：相对/多级链接/`..` 统一解析）必须是可读常规
/// 文件，且其 `name` 字段 = `@deepseek-ai/dsh`、并**声明 CLI 入口**（`bin`：字符串
/// 形态或以 `dsh` 为键的对象——exports-only 无法证明入口 → 拒绝）。
/// 核验通过 → `Some((包目录实际形态, bin 声明相对路径))`——包目录与入口最终目标
/// 统一经实际路径解析（词法父目录不再单独充当锚）。
fn verified_dsh_package_anchor(
    env: &DshEnvironment,
    package_json: &Path,
) -> Option<(String, String)> {
    let actual = env.files.resolve_actual_path(package_json)?;
    let bytes = env
        .files
        .read_small(&actual, CLI_METADATA_READ_BUDGET)
        .ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    if value["name"].as_str() != Some(DSH_PACKAGE_NAME) {
        return None;
    }
    let bin_decl = match &value["bin"] {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Object(map) => map
            .get("dsh")
            .and_then(|v| v.as_str())
            .map(str::to_string)?,
        _ => return None,
    };
    if bin_decl.trim().is_empty() {
        return None;
    }
    let package_dir = super::model::lexical_parent_of(&actual.to_string_lossy())?;
    Some((package_dir, bin_decl))
}

/// 入口身份核验（H05 审查 I5-r5）：「同名 ≠ 入口」——常规文件（shim）入口走
/// **完整调用结构核验**（[`verified_shim_structure`]）；符号链接入口走解析链
/// 实际目标与声明入口的组件级相等。声明入口自身须组件级落在包目录内。
fn verified_cli_entry_identity(
    env: &DshEnvironment,
    entry: &Path,
    package_dir: &str,
    bin_decl: &str,
) -> bool {
    use super::model::{lexical_component_within, lexical_join, lexical_normalize_components};
    let package_dir_norm = lexical_normalize_components(package_dir);
    // 声明入口：实际解析 + 组件级落在包目录内。
    let declared_path = lexical_join(&package_dir_norm, bin_decl);
    let Some(declared_actual) = env.files.resolve_actual_path(Path::new(&declared_path)) else {
        return false;
    };
    let declared_norm = lexical_normalize_components(&declared_actual.to_string_lossy());
    if !lexical_component_within(&declared_norm, &package_dir_norm) {
        return false; // bin 声明逃逸包目录（`..` 等）→ 拒
    }
    if env.files.read_link_target(entry).is_some() {
        // (i) 符号链接分支：解析链（相对/多级/目录链接/`..`）后的实际目标
        //     == 声明入口实际目标（组件级）。
        let Some(entry_actual) = env.files.resolve_actual_path(entry) else {
            return false;
        };
        let ok = lexical_normalize_components(&entry_actual.to_string_lossy()) == declared_norm;
        return ok;
    }
    // (ii) 常规 shim：完整调用结构核验（I5-r5②）。
    verified_shim_structure(env, entry, &package_dir_norm, &declared_norm)
}

/// shim 里允许出现的赋值变量（官方实录形态：`$basedir` / `$resources`）。
const SHIM_ALLOWED_ASSIGNMENTS: [&str; 2] = ["basedir", "resources"];

/// 官方 basedir 赋值的**整表达式**形态（I5-r6②：命令替换/旧 dirname 形态一律拒）。
const OFFICIAL_BASEDIR_VALUE: &str = "$(CDPATH= cd -- \"$(dirname -- \"$0\")/../../..\" && pwd -P)";
/// 官方 resources 赋值的**整表达式**形态（`$launcher` = wrapper 自身 = 被核验入口）。
const OFFICIAL_RESOURCES_VALUE: &str =
    "$(CDPATH= cd -- \"$(dirname -- \"$launcher\")/../../..\" && pwd -P)";
/// 官方赋值表达式的上溯深度（`../../..` = 3 层；从入口所在目录走到安装根）。
const OFFICIAL_WALK_DEPTH: usize = 3;
/// 环境赋值前缀白名单（官方实录：ELECTRON_RUN_AS_NODE=1 exec …）。
const SHIM_ALLOWED_ENV_PREFIXES: [&str; 1] = ["ELECTRON_RUN_AS_NODE"];

/// 词法上溯（官方赋值模板的 `../../..` 语义）：从 dir 起上溯 depth 层；根级
/// `..` 丢弃（绝不越过根）。
fn lexical_walk_up(dir: &str, depth: usize) -> String {
    use super::model::lexical_parent_of;
    let mut current = dir.to_string();
    for _ in 0..depth {
        match lexical_parent_of(&current) {
            Some(parent) => current = parent,
            None => break,
        }
    }
    current
}

/// 官方宿主二进制文件名（同 identity 安装锚点；大小写不敏感比较）。
fn is_official_host_binary_name(actual: &Path) -> bool {
    let name = actual
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    name == "deepseek harness" || name == "deepseek harness.exe"
}

/// 赋值行识别：`VAR=value`（VAR = 下划线小写字母数字）。
fn parse_assignment(line: &str) -> Option<(&str, &str)> {
    let eq = line.find('=')?;
    let var = &line[..eq];
    if var.is_empty() || !var.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') {
        return None;
    }
    Some((var, &line[eq + 1..]))
}

/// exec 行的 shell 形 token 化（I5-r5②）：双引号内空格保留（剥外层双引号），
/// 空白切分——`"$resources/MacOS/DeepSeek Harness"` 是**一个** token。
fn shell_tokens(rest: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut raw = String::new();
    let mut in_quotes = false;
    let finish = |tokens: &mut Vec<String>, current: &mut String, raw: &mut String| {
        if !current.is_empty() {
            // 只接受逐字的 "$@"；保留其他拼写供 Form N 拒绝。
            if current == "$@" && raw != "\"$@\"" {
                tokens.push("$INVALID_PLACEHOLDER".to_string());
                raw.clear();
                current.clear();
            } else {
                tokens.push(std::mem::take(current));
                raw.clear();
            }
        }
    };
    for ch in rest.chars() {
        match ch {
            '"' => {
                raw.push(ch);
                in_quotes = !in_quotes;
            }
            c if c.is_whitespace() && !in_quotes => finish(&mut tokens, &mut current, &mut raw),
            c => {
                raw.push(c);
                current.push(c);
            }
        }
    }
    if in_quotes {
        tokens.push("$INVALID_QUOTE".to_string());
    } else {
        finish(&mut tokens, &mut current, &mut raw);
    }
    tokens
}

/// shim 调用结构核验（H05 审查 I5-r5②）——「内容包含包名」从来不是证据，
/// 核验的是**调用结构**：
///  * 注释行（`#` 起）跳过——注释里的 exec/官方路径零证据（绕过 a 封死）；
///  * 赋值行只认 [`SHIM_ALLOWED_ASSIGNMENTS`] 且各至多一次；`basedir` 被使用时
///    必须恰有一次赋值，赋值形态须为官方 dirname/CDPATH-cd 形态；其它变量
///    （`cmd=…; $cmd` 一类改写）→ 拒；
///  * 控制流（if/case/for/while/fi/esac/done/then/else/elif/in/do）→ 拒；
///  * 其它无法解释的命令行 → 拒；
///  * **恰一条 exec 行**：剥环境赋值前缀（`VAR=value`）后取**第一个非选项 token**
///    为执行目标——
///    - Form D：目标实际解析 == 声明入口（组件级）；
///    - Form N：目标为 node 解释器（`node`/`node.exe`）且首个非选项**参数**实际
///      解析 == 声明入口（解释器剥除——rule ②）；
///    - Form B（官方实录形态，macos-followup-report.md:35）：目标为**同安装树的
///      官方宿主二进制**（`DeepSeek Harness`/`.exe`）+ 某内层参数解析落在该安装
///      的 node_modules 树内（包内声明 CLI 路径）——**两个证据都核对**；
///    * 其余参数不作为通过证据；含未代入 `$` 的目标 → 无法核验 → 拒。
fn verified_shim_structure(
    env: &DshEnvironment,
    entry: &Path,
    package_dir_norm: &str,
    declared_norm: &str,
) -> bool {
    use super::model::{lexical_component_within, lexical_normalize_components};
    let Ok(bytes) = env.files.read_small(entry, CLI_METADATA_READ_BUDGET) else {
        return false;
    };
    let content = String::from_utf8_lossy(&bytes);
    let mut assignments: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut exec_seen = false;
    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue; // 注释/空行（含 shebang）——零证据（绕过 a：注释 exec 封死）
        }
        if let Some((var, value)) = parse_assignment(line) {
            if !SHIM_ALLOWED_ASSIGNMENTS.contains(&var) && var != "launcher" {
                return false; // 变量改写面（cmd=… 等）→ 拒
            }
            if assignments
                .insert(var.to_string(), value.to_string())
                .is_some()
            {
                return false; // 重复赋值 → 拒
            }
            // I5-r6②：整表达式匹配官方形态——命令替换（$/usr/bin/printf 等）与
            // 旧 dirname 形态一律拒。
            let official = match var {
                "basedir" => value == OFFICIAL_BASEDIR_VALUE,
                "resources" => {
                    value == OFFICIAL_RESOURCES_VALUE
                        && assignments.get("launcher").is_some_and(|v| v == "$0")
                }
                "launcher" => value == "$0",
                _ => false,
            };
            if !official {
                return false;
            }
            continue;
        }
        let first_word = line.split_whitespace().next().unwrap_or("");
        if matches!(
            first_word,
            "if" | "case"
                | "for"
                | "while"
                | "fi"
                | "esac"
                | "done"
                | "then"
                | "else"
                | "elif"
                | "in"
                | "do"
                | "{"
                | "}"
        ) {
            return false; // 条件执行分支 → 拒（I5-r5②⑤）
        }
        // 环境赋值前缀（官方实录：ELECTRON_RUN_AS_NODE=1 exec …）：白名单变量逐个
        // 剥除；其它前缀/无 exec 残留 → 无法解释 → 拒。
        let mut work = line;
        loop {
            let first = work.split_whitespace().next().unwrap_or("");
            if first == "exec" || first.is_empty() {
                break;
            }
            let Some(eq) = first.find('=') else {
                return false; // 无法解释的命令行 → 拒
            };
            let name = &first[..eq];
            if !SHIM_ALLOWED_ENV_PREFIXES.contains(&name) {
                return false; // 非白名单环境赋值 → 拒
            }
            work = work[first.len()..].trim_start();
        }
        let Some(exec_rest) = work.strip_prefix("exec ") else {
            return false; // 无法解释的命令行 → 拒
        };
        if exec_seen {
            return false; // 多条 exec → 拒（结构歧义）
        }
        exec_seen = true;
        // token 化：shell 形——双引号内空格保留（剥外层引号）、空白切分。
        let mut tokens: Vec<String> = shell_tokens(exec_rest);
        // 剥环境赋值前缀（官方实录形态：ELECTRON_RUN_AS_NODE=1 exec …）。
        while let Some(token) = tokens.first() {
            if parse_assignment(token).is_some() {
                tokens.remove(0);
            } else {
                break;
            }
        }
        // 官方赋值代入：$basedir/$resources = 入口所在目录上溯 OFFICIAL_WALK_DEPTH
        // 层（模板语义：dirname($0|$launcher) + `../../..`）；$launcher = 入口自身。
        let entry_dir =
            super::model::lexical_parent_of(&entry.to_string_lossy()).unwrap_or_default();
        let official_walk = lexical_walk_up(&entry_dir, OFFICIAL_WALK_DEPTH);
        let substitute = |token: &str| -> String {
            let mut text = token.to_string();
            for (var, resolved) in [
                ("basedir", official_walk.as_str()),
                ("resources", official_walk.as_str()),
                ("launcher", &entry.to_string_lossy()),
            ] {
                text = text.replace(&format!("${var}"), resolved);
            }
            text
        };
        // 执行目标 = 第一个非选项 token；首个非选项**参数**为其实际调用对象。
        // **目标**含未代入 `$` → 无法核验 → 拒；**参数**含未代入 `$`（`"$@"`、
        // `"$1"` 等调用方占位——rule ④「其余参数不作为通过证据」）→ 跳过，不作
        // 证据也绝不构成拒绝。
        let mut target: Option<String> = None;
        let mut args: Vec<String> = Vec::new();
        let mut target_taken = false;
        for token in &tokens {
            let substituted = substitute(token);
            if !target_taken {
                if token.starts_with('-') || substituted.contains('$') {
                    return false; // 执行目标缺位/无法核验 → 拒
                }
                target = Some(substituted);
                target_taken = true;
                continue;
            }
            // 选项 token 保留入列：Form N 对任何 `-` 开头 token 显式拒（--eval
            // 绕过封死）；Form B 对不可解析参数自然跳过。
            args.push(substituted);
        }
        let Some(target) = target else {
            return false;
        };
        // 解释器剥除（rule ②）按**形态**识别：目标 basename = node/node.exe →
        // 真正命令 = 其后的脚本参数（身份证据在脚本，不在解释器二进制）。
        let interpreter_name = lexical_file_name(&target).unwrap_or_default();
        let is_node_interpreter = interpreter_name == "node" || interpreter_name == "node.exe";
        if is_node_interpreter {
            // Form N（收紧，I5-r6②）：**仅接受 `node <声明入口> [占位…]` 形态**——
            // 任何 `-` 开头 token（--eval/--print/--require…）与任何额外非占位参数
            // 一律拒；`"$@"` 等调用方占位跳过。
            let mut script_seen = false;
            for token in &args {
                if token.starts_with('-') {
                    return false; // 即使含变量，任何选项都必须拒绝
                }
                if token == "$@" {
                    continue; // 唯一允许的参数占位
                }
                if token.contains('$') {
                    return false;
                }
                if script_seen {
                    return false; // 额外非占位参数 → 拒
                }
                let Some(script_actual) = env.files.resolve_actual_path(Path::new(token)) else {
                    return false;
                };
                if lexical_normalize_components(&script_actual.to_string_lossy()) != declared_norm {
                    return false; // 脚本 ≠ 声明入口
                }
                script_seen = true;
            }
            if script_seen {
                continue;
            }
            return false; // 无声明入口脚本 → 拒
        }
        // Form D：直接 exec 声明入口（其余参数不作证据）。
        let Some(invoked_actual) = env.files.resolve_actual_path(Path::new(&target)) else {
            return false;
        };
        let invoked_norm = lexical_normalize_components(&invoked_actual.to_string_lossy());
        if invoked_norm == declared_norm {
            continue;
        }
        // Form B（官方实录形态，收紧 I5-r6②）：exec 目标为**官方宿主二进制**且
        // 解析后落在**已核验安装根**（安装根 = 包锚实际路径向上走到 node_modules
        // 的那一层——含 node_modules 的目录；整径前缀比较），且**某内层参数实际
        // 解析后组件级等于声明入口**（「任一参数落包父目录之下」不再算证据——
        // 两个证据都核对，不一刀切拒绝官方形态）。
        if !is_node_interpreter {
            let Some(target_actual) = env.files.resolve_actual_path(Path::new(&target)) else {
                return false;
            };
            let target_norm = lexical_normalize_components(&target_actual.to_string_lossy());
            // 安装根 = 从包目录逐层上溯、走到 node_modules 的那一层的**上一层**
            // （容纳 node_modules 的目录——npm 布局的 <prefix>）；有界 8 层。
            let mut install_root_norm: Option<String> = None;
            let mut cursor: String = package_dir_norm.to_string();
            for _ in 0..8 {
                match super::model::lexical_parent_of(&cursor) {
                    Some(parent) => {
                        let base = parent
                            .rsplit(['/', '\\'])
                            .next()
                            .map(|n| n.to_string())
                            .unwrap_or_default();
                        if base == "node_modules" {
                            // 走到 node_modules：安装根 = 容纳它的上一层。
                            install_root_norm = super::model::lexical_parent_of(&parent);
                            break;
                        }
                        cursor = parent;
                    }
                    None => break,
                }
            }
            let Some(install_root_norm) = install_root_norm else {
                return false;
            };
            // 证据一：宿主二进制（名称 + 位置）落在已核验安装根内。
            let host_in_root = is_official_host_binary_name(&target_actual)
                && lexical_component_within(&target_norm, &install_root_norm);
            // 证据二：某内层参数实际解析后**组件级等于声明入口**。
            let inner_arg_declared = args.iter().any(|arg| {
                env.files
                    .resolve_actual_path(Path::new(arg))
                    .is_some_and(|actual| {
                        lexical_normalize_components(&actual.to_string_lossy()) == declared_norm
                    })
            });
            if host_in_root && inner_arg_declared {
                continue;
            }
        }
        return false; // 对应关系无法确认 → 拒（I5-r5②）
    }
    // $basedir 被使用 → 必须恰有一次赋值（rule ③；赋值形态已在逐行核对）。
    if content.contains("$basedir") && !assignments.contains_key("basedir") {
        return false;
    }
    exec_seen
}

/// 单个 CLI 入口候选的完整核验：形态/绝对性/文件在场 → 从入口所在树找**经核验的
/// 包锚** → 入口身份与该包配对。任一环节不成立 → None（同名冒充在此被拒）。
pub(crate) fn verified_cli_for_entry(env: &DshEnvironment, entry_token: &str) -> Option<PathBuf> {
    if !is_dsh_entry_token(entry_token)
        || !lexically_absolute(entry_token)
        || !cli_entry_shape(entry_token)
    {
        return None;
    }
    let entry_path = PathBuf::from(entry_token);
    if !env.files.exists(&entry_path) {
        return None;
    }
    // 锚搜索 = 入口所在目录起**有界上溯**（官方 basedir 形态上溯 3 层 + node_modules
    // 层；有界 8 层防失控）：每个祖先各探 `A/node_modules/…` 与 `A/lib/node_modules/…`
    // 两个落点。任一锚 + 入口身份配对成功 → 采信。
    let bin_dir = lexical_parent(entry_token)?;
    let rel = ["node_modules", "@deepseek-ai", "dsh", "package.json"];
    let lib_rel = ["lib", "node_modules", "@deepseek-ai", "dsh", "package.json"];
    let mut ancestor = Some(bin_dir.clone());
    for _ in 0..8 {
        let Some(base) = ancestor else { break };
        for probe in [join_lexical(&base, &rel), join_lexical(&base, &lib_rel)] {
            if let Some((package_dir, bin_decl)) = verified_dsh_package_anchor(env, &probe) {
                if verified_cli_entry_identity(env, &entry_path, &package_dir, &bin_decl) {
                    return Some(entry_path);
                }
            }
        }
        ancestor = lexical_parent(&base);
    }
    None
}

pub(crate) fn npm_cli_fingerprint(env: &DshEnvironment, entry: &Path) -> Option<String> {
    let token = entry.to_str()?;
    verified_cli_for_entry(env, token)?;
    let mut ancestor = Some(lexical_parent(token)?);
    for _ in 0..8 {
        let Some(base) = ancestor else { break };
        for probe in [
            join_lexical(
                &base,
                &["node_modules", "@deepseek-ai", "dsh", "package.json"],
            ),
            join_lexical(
                &base,
                &["lib", "node_modules", "@deepseek-ai", "dsh", "package.json"],
            ),
        ] {
            let Some((package_dir, declared)) = verified_dsh_package_anchor(env, &probe) else {
                continue;
            };
            if !verified_cli_entry_identity(env, entry, &package_dir, &declared) {
                continue;
            }
            let actual_entry = env.files.resolve_actual_path(entry)?;
            let bin = super::model::lexical_join(&package_dir, &declared);
            let actual_bin = env.files.resolve_actual_path(Path::new(&bin))?;
            let mut hash = Sha256::new();
            if env.files.read_link_target(entry).is_none() {
                let shim = env.files.read_small(entry, CLI_METADATA_READ_BUDGET).ok()?;
                let text = std::str::from_utf8(&shim).ok()?;
                let exec_line = text.lines().find_map(|line| {
                    line.trim()
                        .strip_prefix("exec ")
                        .or_else(|| line.trim().strip_prefix("ELECTRON_RUN_AS_NODE=1 exec "))
                })?;
                let target = shell_tokens(exec_line).into_iter().next()?;
                let entry_dir = super::model::lexical_parent_of(&entry.to_string_lossy())?;
                let root = lexical_walk_up(&entry_dir, OFFICIAL_WALK_DEPTH);
                let target = target
                    .replace("$basedir", &root)
                    .replace("$resources", &root)
                    .replace("$launcher", &entry.to_string_lossy());
                let interpreter = lexical_file_name(&target).unwrap_or_default();
                if interpreter != "node" && interpreter != "node.exe" {
                    let executable = env.files.resolve_actual_path(Path::new(&target))?;
                    if is_official_host_binary_name(&executable) {
                        hash.update(executable.to_string_lossy().as_bytes());
                        hash.update(env.files.read_small(&executable, 128 * 1024 * 1024).ok()?);
                    }
                }
            }
            for path in [&actual_entry, &actual_bin, &probe] {
                hash.update(path.to_string_lossy().as_bytes());
                hash.update(
                    env.files
                        .read_small(path, PACKAGE_DESCRIPTOR_BUDGET_BYTES)
                        .ok()?,
                );
            }
            let mut pending = vec![(PathBuf::from(&package_dir), 0usize)];
            let mut files = 0usize;
            let mut bytes_total = 0usize;
            while let Some((directory, depth)) = pending.pop() {
                if depth > 32 {
                    return None;
                }
                let mut children = env.files.list_dir_checked(&directory).ok()?;
                children.sort();
                for child in children {
                    files += 1;
                    if files > 10_000 {
                        return None;
                    }
                    hash.update(child.to_string_lossy().as_bytes());
                    if env.files.read_link_target(&child).is_some() {
                        let actual = env.files.resolve_actual_path(&child)?;
                        if !super::model::lexical_component_within(
                            &super::model::lexical_normalize_components(&actual.to_string_lossy()),
                            &super::model::lexical_normalize_components(&package_dir),
                        ) {
                            return None;
                        }
                        hash.update(actual.to_string_lossy().as_bytes());
                        let bytes = env
                            .files
                            .read_small(&actual, PACKAGE_DESCRIPTOR_BUDGET_BYTES)
                            .ok()?;
                        bytes_total = bytes_total.checked_add(bytes.len())?;
                        if bytes_total > 256 * 1024 * 1024 {
                            return None;
                        }
                        hash.update(&bytes);
                    } else if env.files.is_dir(&child) {
                        pending.push((child, depth + 1));
                    } else {
                        let bytes = env
                            .files
                            .read_small(&child, PACKAGE_DESCRIPTOR_BUDGET_BYTES)
                            .ok()?;
                        bytes_total = bytes_total.checked_add(bytes.len())?;
                        if bytes_total > 256 * 1024 * 1024 {
                            return None;
                        }
                        hash.update(&bytes);
                    }
                }
            }
            return Some(hash.finalize().iter().map(|b| format!("{b:02x}")).collect());
        }
        ancestor = lexical_parent(&base);
    }
    None
}

const MAC_BUNDLED_WRAPPER: &str = r##"#!/bin/sh
# Run the installed CLI without changing the invoking shell or working directory.
set -e
launcher=$0
while [ -L "$launcher" ]; do
  directory=$(CDPATH= cd -- "$(dirname -- "$launcher")" && pwd -P)
  target=$(readlink "$launcher")
  case "$target" in
    /*) launcher=$target ;;
    *) launcher=$directory/$target ;;
  esac
done
resources=$(CDPATH= cd -- "$(dirname -- "$launcher")/../../.." && pwd -P)
ELECTRON_RUN_AS_NODE=1 exec "$resources/../MacOS/DeepSeek Harness" --expose-internals "$resources/app.asar/dsh/node_modules/@deepseek-ai/dsh-desktop-host/lib/cli.js" "$@"
"##;

pub(crate) fn bundled_wrapper_for_process(process: &DshProcess) -> Option<PathBuf> {
    if !is_web_host_process(process) {
        return None;
    }
    let argv0 = process.cmdline.first()?.replace('\\', "/");
    let root = argv0
        .strip_suffix("/Contents/Resources/../MacOS/DeepSeek Harness")
        .or_else(|| argv0.strip_suffix("/Contents/MacOS/DeepSeek Harness"))?;
    let root = Path::new(root).canonicalize().ok()?;
    let executable = root.join("Contents/MacOS/DeepSeek Harness");
    if !super::model::same_executable_path(&process.exe, &executable) {
        return None;
    }
    Some(root.join("Contents/Resources/runtime/cli/bin/dsh"))
}

pub(crate) fn bundled_cli_for_process(
    env: &DshEnvironment,
    process: &DshProcess,
) -> Option<PathBuf> {
    let wrapper = bundled_wrapper_for_process(process)?;
    validate_bundled_cli(env, &wrapper).then_some(wrapper)
}

pub(crate) fn bundled_cli_fingerprint(env: &DshEnvironment, wrapper: &Path) -> Option<String> {
    let root = wrapper.ancestors().nth(6)?.canonicalize().ok()?;
    let executable = root.join("Contents/MacOS/DeepSeek Harness");
    let expected = root.join("Contents/Resources/runtime/cli/bin/dsh");
    let archive = root.join("Contents/Resources/app.asar");
    if wrapper != expected {
        return None;
    }
    for path in [&executable, &expected, &archive] {
        if !path.canonicalize().ok()?.starts_with(&root) {
            return None;
        }
    }
    let wrapper_bytes = env.files.read_small(&expected, 16 * 1024).ok()?;
    if wrapper_bytes != MAC_BUNDLED_WRAPPER.as_bytes() {
        return None;
    }
    let executable_bytes = env.files.read_small(&executable, 128 * 1024 * 1024).ok()?;
    let archive_hash = super::contract::bundled_cli_archive_fingerprint(&archive)?;
    let mut digest = Sha256::new();
    digest.update(&wrapper_bytes);
    digest.update(&executable_bytes);
    digest.update(archive_hash.as_bytes());
    Some(
        digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    )
}

pub(crate) fn validate_bundled_cli(env: &DshEnvironment, wrapper: &Path) -> bool {
    bundled_cli_fingerprint(env, wrapper).is_some()
}

/// 活宿主 argv 的经核验 dsh 入口。无合格入口则不采信。
fn verified_web_argv_cli(env: &DshEnvironment) -> Option<PathBuf> {
    env.processes
        .list_processes()
        .into_iter()
        .filter(is_web_host_process)
        .find_map(|p| {
            p.cmdline
                .iter()
                .map(String::as_str)
                .find_map(|token| verified_cli_for_entry(env, token))
        })
}

/// PATH 解析（I5-r3）：逐目录逐入口名走完整核验（含包锚元数据与入口身份）；
/// 相对目录项跳过。全部不合格 → None。
pub(crate) fn resolve_path_cli_entry(
    env: &DshEnvironment,
    path_var: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    for dir in std::env::split_paths(path_var?) {
        if !dir.is_absolute() {
            continue;
        }
        for name in CLI_ENTRY_NAMES {
            let candidate = dir.join(name);
            let token = candidate.to_string_lossy().to_string();
            if let Some(verified) = verified_cli_for_entry(env, &token) {
                return Some(verified);
            }
        }
    }
    None
}

/// web 目标的**经核验** CLI 源（固定已核验绝对路径；无法证明 → 明确错误码拒绝，
/// 绝不执行未核验来源）：
/// ① 活宿主 argv 的 dsh 入口（I5-r3 完整身份核验——见上）；
/// ② 无活宿主 → PATH 解析（`path_var` 由调用方注入：生产 = 进程 PATH，测试注入
///    夹具布局）；
/// ③ 两者皆不可证明 → `DSH_CLI_SOURCE_UNVERIFIED`。
pub fn web_cli_source(
    env: &DshEnvironment,
    path_var: Option<&std::ffi::OsStr>,
) -> Result<PathBuf, DshError> {
    let bundled: Vec<_> = env
        .processes
        .list_processes()
        .into_iter()
        .filter(|process| {
            is_web_host_process(process)
                && process
                    .cmdline
                    .iter()
                    .any(|arg| arg.contains("dsh-desktop-host"))
        })
        .collect();
    if !bundled.is_empty() {
        if bundled.len() == 1 {
            if let Some(cli) = bundled_cli_for_process(env, &bundled[0]) {
                return Ok(cli);
            }
        }
        return resolve_path_cli_entry(env, path_var).ok_or_else(|| {
            DshError::new(
                codes::CLI_SOURCE_UNVERIFIED,
                "bundled web cli source is unverified",
            )
        });
    }
    if let Some(cli) = verified_web_argv_cli(env) {
        return Ok(cli);
    }
    resolve_path_cli_entry(env, path_var).ok_or_else(|| {
        DshError::new(
            codes::CLI_SOURCE_UNVERIFIED,
            "no verified dsh cli entry for web profile management (host argv and PATH both unverified); start the dsh host once or install the official cli",
        )
    })
}

/// Desktop 候选核验（profile 门 → 监听属主核验 → 组合身份）。argv 令牌是官方
/// 携带的 profile 权威证据（Win/Mac 报告 §1.2）→ `verified = true`。
fn desktop_candidate(
    env: &DshEnvironment,
    host: &DshProcess,
) -> Result<InstanceCandidate, DshError> {
    // profile 门：宿主 argv 里的 profile 令牌是权威（Win 报告 §1.2 argv[3]
    // projectDir；DSH_HOME 影子目录也如实跟随），缺失时退回 env.home 推导值。
    let profile_dir = argv_profile_dir(&host.cmdline)
        .map(PathBuf::from)
        .unwrap_or_else(|| desktop_profile_dir(env));
    if !profile_initialized(env, &profile_dir) {
        return Err(DshError::new(
            codes::PROFILE_NOT_INITIALIZED,
            format!(
                "desktop profile missing official marker: {}",
                profile_dir.display()
            ),
        ));
    }
    // 监听属主核验：只认宿主 pid 名下的回环监听（假监听/非回环都拒——验收①b）。
    let listener = owned_loopback_listener(env, host.pid)?;
    let normalized = normalize_profile(&profile_dir);
    let instance_id = instance_id_for(HostKind::Desktop, &normalized);
    let local_host = build_local_host(
        env,
        host,
        &listener,
        &profile_dir,
        &normalized,
        &instance_id,
    );
    Ok(InstanceCandidate {
        instance_id,
        kind: HostKind::Desktop,
        display_name: display_name(&profile_dir),
        profile_dir: normalized,
        host: local_host,
        verified: true,
    })
}

fn is_web_host_process(process: &DshProcess) -> bool {
    if !is_web_host_cmdline(&process.cmdline) {
        return false;
    }
    let os: Vec<OsString> = process.cmdline.iter().map(Into::into).collect();
    if crate::monitor::dsh::cmdline_is_dsh_desktop_host(&os) {
        return process.cmdline.first().is_some_and(|argv0| {
            super::model::same_executable_path(&process.exe, Path::new(argv0))
        });
    }
    true
}

/// Web 形态包括官方桌面包内 CLI 的 `cli.js web`，以及普通 `dsh web`。
/// 桌面 profile 在场时仍优先归为 Desktop；普通形态继续排除 Desktop 宽网。
pub(crate) fn is_web_host_cmdline(cmdline: &[String]) -> bool {
    let bundled_web = if let [executable, flag, entry, command, ..] = cmdline {
        let executable = executable.replace('\\', "/");
        let entry = entry.replace('\\', "/");
        let app = executable
            .strip_suffix("/Contents/Resources/../MacOS/DeepSeek Harness")
            .or_else(|| executable.strip_suffix("/Contents/MacOS/DeepSeek Harness"));
        let entry_app = entry.strip_suffix(
            "/Contents/Resources/app.asar/dsh/node_modules/@deepseek-ai/dsh-desktop-host/lib/cli.js",
        );
        app.is_some() && app == entry_app && flag == "--expose-internals" && command == "web"
    } else {
        false
    };
    if bundled_web && argv_profile_dir(cmdline).is_none() {
        return true;
    }
    let os: Vec<OsString> = cmdline.iter().map(Into::into).collect();
    if crate::monitor::dsh::cmdline_is_dsh_desktop_host(&os) {
        return false;
    }
    // monitor 宿主门 = (has_dsh && has_web) || Desktop 宽网；剔出宽网后剩下的
    // 恰为 web 形态。
    crate::monitor::dsh::cmdline_is_dsh_host(&os)
}

/// 候选显示名 = profile 目录尾名（同名靠 profile_dir 全路径区分）。词法切分
/// （'/' 与 '\' 都算分隔）——Windows 形态路径在 POSIX 主机上 std Path 不识别 '\'。
/// 公开包装：已登记但未在场的条目也要有可读名字（列表统一口径）。
pub fn display_name_for(profile_dir: &Path) -> String {
    display_name(profile_dir)
}

fn display_name(profile_dir: &Path) -> String {
    let text = profile_dir.to_string_lossy();
    let trimmed = text.trim_end_matches(['/', '\\']);
    match trimmed.rfind(['/', '\\']) {
        Some(idx) => trimmed[idx + 1..].to_string(),
        None => trimmed.to_string(),
    }
}

/// 宿主进程判定（auth 层对单进程的核验入口；T02 双特征判定的公开包装——
/// 本机 IPC 连接方的 PID 必须映射到这样一条进程，握手才继续）。
pub fn is_desktop_host_process(process: &DshProcess) -> bool {
    is_desktop_host_cmdline(&process.cmdline)
}

/// 安装解析（§2.3 签名固定）：Desktop 未运行也能定位安装与自带 CLI。
/// `DSH_DESKTOP_NOT_INSTALLED`（含「只有 npm 全局 dsh」形态）/
/// `DSH_PROFILE_NOT_INITIALIZED`（有安装但 profile 缺官方标记）分开上抛。
pub fn resolve_installation(env: &DshEnvironment) -> Result<DesktopInstallation, DshError> {
    match find_installed_executable(env) {
        Some(executable) => {
            let installation = installation_from_executable(env, &executable);
            if !profile_initialized(env, &installation.profile_dir) {
                return Err(DshError::new(
                    codes::PROFILE_NOT_INITIALIZED,
                    format!(
                        "desktop profile missing official marker: {}",
                        installation.profile_dir.display()
                    ),
                ));
            }
            Ok(installation)
        }
        None => Err(DshError::new(
            codes::DESKTOP_NOT_INSTALLED,
            "no official desktop installation found under conventional roots",
        )),
    }
}

/// 安装自带 CLI 的定位（**无 desktop profile 门**；H05 实例目标模式的 CLI 源）。
///
/// 目标是 web profile（或任何非 env.home desktop profile）时，CLI 只是**工具源**
/// ——profile 初始化与否由管理目标自行核验（`DshPluginManager::start` 的目标门），
/// 不得让「env.home 的 desktop profile 未初始化」挡住别的 profile 的正规管理。
pub fn resolve_cli(env: &DshEnvironment) -> Result<DesktopInstallation, DshError> {
    find_installed_executable(env)
        .map(|exe| installation_from_executable(env, &exe))
        .ok_or_else(|| {
            DshError::new(
                codes::DESKTOP_NOT_INSTALLED,
                "no official desktop installation found under conventional roots",
            )
        })
}

// ============================================================
// 安装定位（数据驱动探测）
// ============================================================

/// 常规安装根候选清单。**候选只是数据，不假设任何一处必装**（计划红线：不硬
/// 编码 C 盘——各盘符 Program Files 都在探测清单内，缺席盘探测落空无副作用）。
/// 用户级根从 DSH home 反推用户 home；生产 T05 可再以注册表/LaunchServices 扩充。
fn install_search_roots(env: &DshEnvironment) -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from("/Applications")];
    if let Some(user_home) = lexical_parent(&env.home.to_string_lossy()) {
        roots.push(join_lexical(&user_home, &["Applications"]));
        roots.push(join_lexical(&user_home, &["AppData", "Local", "Programs"]));
    }
    for letter in b'C'..=b'Z' {
        roots.push(PathBuf::from(format!("{}:\\Program Files", letter as char)));
        roots.push(PathBuf::from(format!(
            "{}:\\Program Files (x86)",
            letter as char
        )));
    }
    roots
}

/// 逐根枚举条目，认两类官方布局（条目名任意——中文/空格不设限）：
/// Win 形态 `<条目>\DeepSeek Harness.exe`（条目 = 安装目录，E2 报告 §1.1）；
/// Mac 形态 `<条目>/Contents/MacOS/DeepSeek Harness`（条目 = `.app` bundle）。
/// 多安装命中取首个（根/条目均确定性排序）；Conflict 分档归 T04 管理任务。
fn find_installed_executable(env: &DshEnvironment) -> Option<PathBuf> {
    for root in install_search_roots(env) {
        let mut entries = env.files.list_dir(&root);
        entries.sort();
        for entry in entries {
            let entry = entry.to_string_lossy().to_string();
            let win_exe = join_lexical(&entry, &[WIN_EXE_NAME]);
            if env.files.exists(&win_exe) {
                return Some(win_exe);
            }
            let mac_exe = join_lexical(&entry, &["Contents", "MacOS", MAC_EXE_NAME]);
            if env.files.exists(&mac_exe) {
                return Some(mac_exe);
            }
        }
    }
    None
}

/// 从可执行文件形态反推安装布局（discover 运行路径用：运行中进程 = 最强安装
/// 证据，不依赖安装根探测的覆盖面）。
fn installation_from_executable(env: &DshEnvironment, executable: &Path) -> DesktopInstallation {
    let exe = executable.to_string_lossy().to_string();
    let profile_dir = desktop_profile_dir(env);
    if exe.to_lowercase().ends_with(".exe") {
        // Win：exe = <安装目录>\DeepSeek Harness.exe
        let root = lexical_parent(&exe).unwrap_or(exe);
        let resources = join_lexical(&root, &["resources"]);
        DesktopInstallation {
            cli_path: join_lexical(&resources.to_string_lossy(), WIN_CLI_REL),
            package_descriptor: join_lexical(&root, &["resources", "app-update.yml"]),
            root: PathBuf::from(&root),
            resources_dir: resources,
            executable: executable.to_path_buf(),
            profile_dir,
            platform: "windows".into(),
            version: None,
        }
    } else {
        // Mac：exe = <app>/Contents/MacOS/<name>（词法两级上溯到 bundle 根）。
        // 双特征宿主的可执行文件只会是 bundle 内 Helper/主程序形态，兜底按 Mac。
        let macos_dir = lexical_parent(&exe).unwrap_or(exe);
        let contents = lexical_parent(&macos_dir).unwrap_or(macos_dir);
        let root = lexical_parent(&contents.clone()).unwrap_or(contents.clone());
        let resources = join_lexical(&contents, &["Resources"]);
        DesktopInstallation {
            cli_path: join_lexical(&resources.to_string_lossy(), MAC_CLI_REL),
            package_descriptor: join_lexical(&contents, &["Info.plist"]),
            root: PathBuf::from(&root),
            resources_dir: resources,
            executable: executable.to_path_buf(),
            profile_dir,
            platform: "macos".into(),
            version: None,
        }
    }
}

/// desktop profile 目录（env.home = `~/.dsh`，见 model.rs DshEnvironment 注释）。
fn desktop_profile_dir(env: &DshEnvironment) -> PathBuf {
    join_lexical(env.home.to_string_lossy().as_ref(), PROFILE_REL)
}

/// profile 初始化 = 目录在且有官方标记文件（Win 报告 §1.3：各 profile 独占
/// `package.json`；只查标记，不读会话内容——验收④）。
pub(crate) fn profile_initialized(env: &DshEnvironment, profile_dir: &Path) -> bool {
    env.files.exists(profile_dir)
        && env.files.exists(&join_lexical(
            profile_dir.to_string_lossy().as_ref(),
            &[PROFILE_MARKER],
        ))
}

// ============================================================
// 宿主判定与身份组合
// ============================================================

/// 控制面宿主判定 = 双特征**齐备**（Win 报告 §1.2「唯二双特征判定」强口径）：
/// ① 有 argv 含 `dsh-desktop-host`（官方宿主包路径子串，最强特征）；
/// ② 有 argv 以 `<sep>profiles<sep>desktop` 结尾。
/// `monitor::dsh::cmdline_is_dsh_desktop_host` 是展示用宽网（两特征任一命中）；
/// 控制面要求组合齐备，单一特征的进程不得入选（验收①a）。
fn is_desktop_host_cmdline(cmdline: &[String]) -> bool {
    argv_profile_dir(cmdline).is_some() && cmdline.iter().any(|t| t.contains("dsh-desktop-host"))
}

/// desktop profile 后缀（`profiles/desktop` 的两种分隔符形态）。
///
/// **刻意不含 `.dsh` 字面量**（Mac 补探测报告 §6 第 6 条 / 审查 N3）：
/// profile 目录是 `$DSH_HOME/profiles/desktop`，而 `DSH_HOME` 可以是任意影子
/// 目录（实测形态 `<…>/home-shadow/profiles/desktop`）。要求 `/.dsh/` 会让这类
/// 宿主在控制面隐身（`DSH_HOST_NOT_RUNNING`）。锚点只落在 `profiles/desktop`
/// 两段，故 `profiles/web` 之类的其它 profile 名字仍然不入围。
const PROFILE_SUFFIXES: [&str; 2] = ["/profiles/desktop", "\\profiles\\desktop"];

/// argv 里的 desktop profile 令牌（宿主实际使用的 profile 目录；与 monitor
/// 判定同源的后缀口径，DSH_HOME 影子目录如实跟随）。
fn argv_profile_dir(cmdline: &[String]) -> Option<&str> {
    cmdline
        .iter()
        .map(String::as_str)
        .find(|t| PROFILE_SUFFIXES.iter().any(|suffix| t.ends_with(suffix)))
}

/// 官方宿主固定 `--host 127.0.0.1 --port 0`（Mac 报告 §1 源码确认）——authority
/// 形如 `127.0.0.1:<动态端口>`；非回环监听不认（不跳外部地址，全局约束）。
fn is_loopback_authority(authority: &str) -> bool {
    authority.starts_with("127.0.0.1:")
}

/// 监听属主核验：只认宿主 pid 名下的回环监听（Win 报告 §1.2：6 进程仅 1 条
/// LISTEN 且属主 = 宿主 PID）。假源若给多条，取字典序最小保证确定性。
fn owned_loopback_listener(env: &DshEnvironment, host_pid: u32) -> Result<DshListener, DshError> {
    let mut owned: Vec<DshListener> = env
        .processes
        .listening_sockets()
        .into_iter()
        .filter(|l| l.owner_pid == host_pid && is_loopback_authority(&l.authority))
        .collect();
    owned.sort_by(|a, b| a.authority.cmp(&b.authority));
    owned.into_iter().next().ok_or_else(|| {
        DshError::new(
            codes::HOST_LISTENER_MISSING,
            format!("no loopback listener owned by host pid {host_pid}"),
        )
    })
}

/// R01 受限首握手宿主：唯一原始 Web 进程 + 专属回环监听。
///
/// 返回的 `LocalHost` 仅用于发布**受限待批入口**（让插件把实际
/// `ctx.profileContext.home` 报上来参与核验）；`profile_dir` 仍是登记时的
/// 候选 profile，`verified` 语义不由本函数授予——调用方必须把结果当作
/// 「未核验」，不得据此签发凭据或放行写操作。
/// R01 受限首握手入口：用**观测到的启动环境**把实例映射到唯一 Web 进程。
///
/// 映射逻辑**不再在本函数里**：`discover_detailed` 的 Web 归属晋升
/// （[`web_candidates_from_raw`]）用同一份材料（OS 启动环境 + home 派生实例 ID）
/// 产候选，本函数只是它的「按实例 ID 取唯一项」消费方——单源避免两处映射漂移。
///
/// 只把**观测成功且唯一匹配**当作支持证据；读不到环境、零/多匹配均拒绝。
/// 返回的 `LocalHost` 仍供「受限待批入口」使用：**当选**与凭据签发不因本函数
/// 而放弃后续的当代核验（`resolve_*` 每次重新发现并核指纹）。
pub fn restricted_web_host(env: &DshEnvironment, instance_id: &str) -> Result<LocalHost, DshError> {
    let discovery = discover_detailed(env)?;
    // 归属已证明的候选（`discover_detailed` 晋升出来的）+ 未晋升的原始事实里，
    // 可能仍有本实例的进程（例如同实例多进程 → 全部退回 raw）——后者必须计入
    // 「多匹配」，否则歧义会被当成「未运行」。
    let matches: Vec<&InstanceCandidate> = discovery
        .candidates
        .iter()
        .filter(|c| c.instance_id == instance_id && c.verified)
        .collect();
    match matches.len() {
        1 => Ok(matches[0].host.clone()),
        0 => {
            // 有原始 Web 事实但一个都没晋升 → 归属证不出（可诊断），不是「没运行」。
            if !discovery.raw_web_hosts.is_empty() {
                return Err(discovery.web_error.unwrap_or_else(|| {
                    DshError::new(
                        codes::INSTANCE_UNVERIFIED,
                        format!("instance {instance_id} has web hosts but none attributable"),
                    )
                }));
            }
            if let Some(e) = discovery.desktop_error {
                return Err(e);
            }
            Err(DshError::new(
                codes::HOST_NOT_RUNNING,
                format!("instance {instance_id} is not live"),
            ))
        }
        n => Err(DshError::new(
            codes::TARGET_AMBIGUOUS,
            format!(
                "restricted first handshake requires exactly one observed web host for this instance (observed {n})"
            ),
        )),
    }
}

/// LocalHost 装配：PID+启动时刻+exe+profile+监听属主组合核验后的本机身份。
/// `host_id` = 稳定实例 ID（H04：不再固定 `local`；wire 字段名不变，旧动作的
/// `local` 身份只在账本里保守原样保留，不自动归给新实例）。
fn build_local_host(
    env: &DshEnvironment,
    host: &DshProcess,
    listener: &DshListener,
    profile_dir: &Path,
    normalized_profile: &Path,
    instance_id: &str,
) -> LocalHost {
    let installation = installation_from_executable(env, &host.exe);
    // 包**描述符级**身份哈希（Info.plist / app-update.yml）：读不到/超预算 → 空串
    // = 未知（采样缺失不得编造）。它是「安装是否换代」的快速比对值，**不是**整包
    // 内容指纹——后者由 `contract::read_report` 现算（唯一权威）。
    let package_sha256 = env
        .files
        .read_small(
            &installation.package_descriptor,
            PACKAGE_DESCRIPTOR_BUDGET_BYTES,
        )
        .map(|bytes| sha256_hex(&bytes))
        .unwrap_or_default();
    LocalHost {
        host_id: instance_id.to_owned(),
        platform: installation.platform,
        profile_dir: profile_dir.to_path_buf(),
        executable: host.exe.clone(),
        pid: host.pid,
        started_at_ms: host.started_at_ms,
        epoch: epoch_of(host, normalized_profile),
        authority: listener.authority.clone(),
        package_sha256,
    }
}

/// 连接代际 = 进程身份指纹（PID+启动时刻+exe+profile 的 SHA-256 截断）。
/// 端口**不参与**（动态且可复用——验收②：同端口换代必须换代际）；无状态派生
/// 使 discover 可重复调用且同一进程代际稳定。生产如需严格单调代数，由后续
/// store 任务以持久计数器替换；本入口只保证「进程身份变 ⇒ 代际必变」。
fn epoch_of(host: &DshProcess, profile_dir: &Path) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(b"dsh-host-epoch-v1");
    hasher.update(host.pid.to_le_bytes());
    hasher.update(host.started_at_ms.to_le_bytes());
    hasher.update(b"\0");
    hasher.update(host.exe.to_string_lossy().as_bytes());
    hasher.update(b"\0");
    hasher.update(profile_dir.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    u64::from_be_bytes(digest[..8].try_into().expect("sha256 digest >= 8 bytes"))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

// ============================================================
// 词法路径工具（Windows 形态路径在 POSIX 主机上 std Path 不识别 '\'，
// file_identity.rs 同款词法处理；不查文件系统）
// ============================================================

/// 词法层父目录（'/' 与 '\' 都算分隔）。
pub(super) fn lexical_parent(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches(['/', '\\']);
    let idx = trimmed.rfind(['/', '\\'])?;
    let parent = &trimmed[..idx];
    Some(if parent.is_empty() {
        "/".to_string()
    } else {
        parent.to_string()
    })
}

/// 以 base 的分隔符语义拼接相对路径（保持 Windows 路径的原生反斜杠形态）。
pub(super) fn join_lexical(base: &str, rel: &[&str]) -> PathBuf {
    let sep = if base.contains('\\') { "\\" } else { "/" };
    let trimmed = base.trim_end_matches(['/', '\\']);
    PathBuf::from(format!("{trimmed}{sep}{}", rel.join(sep)))
}
