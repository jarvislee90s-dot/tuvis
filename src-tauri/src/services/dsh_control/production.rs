//! DSH 控制面生产装配（计划 §2.5「manager 和 hub 共用注入 env 及 auth handle，
//! 不各起一套生产全局」的**单一装配点**；T09 的 `production_hub` 也从这里取
//! env 与 auth handle，不另起第二套生产全局）。
//!
//! 生产实现的口径：
//! * **home 单源**：DSH home 复用 `monitor::dsh::dsh_home()`（`DSH_HOME` 影子
//!   目录优先，采集域纪律同源）；
//! * **数据根单源（R10）**：`tuvis_home` = 应用数据根下的 `.tuvis`（与
//!   `database::connection` 同一解析：debug 认 `TUVIS_HOME`、release 一律真实
//!   目录）；DSH **状态目录**由 `TUVIS_DSH_STATE_DIR` 覆盖，未设则
//!   `<tuvis_home>/dsh`——变量即状态目录，两端同义（[`resolve_state_dir`]）。
//! * **进程源**：sysinfo 扫描投影（cmd/exe 全量刷新；`started_at_ms` = 进程
//!   start_time 秒 → 毫秒），监听走 `lsof`（Unix）/`netstat -ano`（Windows）
//!   的 LISTEN 投影——解析失败/工具缺席返回空表（identity 据此如实上报
//!   `DSH_HOST_LISTENER_MISSING`，绝不编造监听）；
//! * **runner**：`std::process::Command` 独立 args（绝不拼 shell 字符串），
//!   Windows 复用 `remote::NoWindow` 抑制控制台闪窗（install.rs 注释的接线处）；
//! * **BindingRevoker / 授权读取面**（T06）：`auth::ProductionBindingRevoker`
//!   （关受信 Peer + 清信任 + 墓碑）与 `auth::AuthServiceReader`（status 的
//!   授权档/活跃绑定 id）——manager 与 auth 共用同一 env Arc 与 auth handle。
//!
//! 测试**永不**使用本模块的进程/监听/runner 适配器（那些走注入面）；但
//! **路径解析**的纯函数（[`DshEnvSnapshot`] / [`resolve_state_dir`] / [`build_env`]）
//! 有单测——它们正是 R10 的回归面。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, UpdateKind};

use crate::remote::NoWindow;

use super::auth::DshAuthService;
use super::error::{codes, DshError};
use super::install::DshPluginManager;
use super::model::{
    CommandRequest, DshClock, DshCommandRunner, DshEnvironment, DshFileSource, DshListener,
    DshProcess, DshProcessSource, LocalHost, RunOutcome,
};

// ============================================================
// 进程源（sysinfo 扫描投影 + 监听投影）
// ============================================================

/// 快照缓存 TTL：T04 等待门以 25ms 节奏轮询 `host_gate`（discover 每拍都要
/// 进程表 + 监听表），生产侧 sysinfo 全量刷新与 `lsof`/`netstat` spawn 都不便宜
/// ——不缓存就是每秒 40 次全量扫描/子进程。1 秒 TTL 把等待门的退出检测延迟
/// 抬高到 ≤1.5s（用户完全退出桌面的动作本身远慢于此），对卡面无感。
const SNAPSHOT_TTL: std::time::Duration = std::time::Duration::from_millis(1000);

/// 生产进程源：sysinfo 全量扫描投影 + `lsof`/`netstat` 监听投影，双表各带
/// TTL 缓存（`System` 实例驻留复用，adapter::SHARED_SYSTEM 同款）。
pub struct SysinfoProcessSource {
    system: Mutex<sysinfo::System>,
    processes_cache: Mutex<Option<(std::time::Instant, Vec<DshProcess>)>>,
    listeners_cache: Mutex<Option<(std::time::Instant, Vec<DshListener>)>>,
}

impl SysinfoProcessSource {
    pub fn new() -> Self {
        Self {
            system: Mutex::new(sysinfo::System::new()),
            processes_cache: Mutex::new(None),
            listeners_cache: Mutex::new(None),
        }
    }

    /// TTL 内直接复用快照（测试注入面不走本类型，缓存不影响测试确定性）。
    fn cached<T: Clone>(slot: &Mutex<Option<(std::time::Instant, Vec<T>)>>) -> Option<Vec<T>> {
        let guard = slot.lock().unwrap();
        guard
            .as_ref()
            .filter(|(at, _)| at.elapsed() < SNAPSHOT_TTL)
            .map(|(_, value)| value.clone())
    }
}

impl Default for SysinfoProcessSource {
    fn default() -> Self {
        Self::new()
    }
}

impl DshProcessSource for SysinfoProcessSource {
    fn list_processes(&self) -> Vec<DshProcess> {
        if let Some(cached) = Self::cached(&self.processes_cache) {
            return cached;
        }
        self.list_processes_fresh()
    }

    fn list_processes_fresh(&self) -> Vec<DshProcess> {
        let mut system = self.system.lock().unwrap();
        if system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .with_cmd(UpdateKind::Always)
                .with_exe(UpdateKind::Always),
        ) == 0
        {
            // 全量刷新零命中：无进程（空表是事实，不是错误）。
            *self.processes_cache.lock().unwrap() = Some((std::time::Instant::now(), Vec::new()));
            return Vec::new();
        }
        let processes: Vec<DshProcess> = system
            .processes()
            .values()
            .filter(|p| !p.cmd().is_empty())
            .map(|p| DshProcess {
                pid: p.pid().as_u32(),
                exe: p.exe().map(PathBuf::from).unwrap_or_default(),
                cmdline: p
                    .cmd()
                    .iter()
                    .map(|s| s.to_string_lossy().to_string())
                    .collect(),
                // sysinfo start_time = UNIX 秒；model 侧口径是毫秒。
                started_at_ms: (p.start_time() as i64) * 1000,
            })
            .collect();
        *self.processes_cache.lock().unwrap() =
            Some((std::time::Instant::now(), processes.clone()));
        processes
    }

    fn listening_sockets(&self) -> Vec<DshListener> {
        if let Some(cached) = Self::cached(&self.listeners_cache) {
            return cached;
        }
        self.listening_sockets_fresh()
    }

    fn listening_sockets_fresh(&self) -> Vec<DshListener> {
        let listeners = listeners_via_cli();
        *self.listeners_cache.lock().unwrap() =
            Some((std::time::Instant::now(), listeners.clone()));
        listeners
    }
}

/// LISTEN 监听投影：Unix 走 `lsof -nP -iTCP -sTCP:LISTEN`（列 2 = PID，NAME 列
/// 含 `127.0.0.1:<port>`）；Windows 走 `netstat -ano -p TCP`（Local Address 列 +
/// LISTENING 行尾 PID）。工具缺席/输出变形 → 空表（identity 如实上报监听缺失）。
fn listeners_via_cli() -> Vec<DshListener> {
    #[cfg(unix)]
    let probe: Option<(&str, [&str; 3], bool)> =
        Some(("lsof", ["-nP", "-iTCP", "-sTCP:LISTEN"], false));
    #[cfg(not(unix))]
    let probe: Option<(&str, [&str; 3], bool)> = Some(("netstat", ["-ano", "-p", "TCP"], true));
    let Some((program, args, windows_shape)) = probe else {
        return Vec::new();
    };
    use std::io::Read;
    use wait_timeout::ChildExt;
    let mut child = match std::process::Command::new(program)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return Vec::new(),
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Vec::new();
    };
    const OUTPUT_LIMIT: usize = 8 * 1024 * 1024;
    #[cfg(unix)]
    let output = {
        use std::os::fd::AsRawFd;
        let mut stdout = stdout;
        let fd = stdout.as_raw_fd();
        // SAFETY: fd belongs to the live child stdout pipe.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            let _ = child.kill();
            let _ = child.wait();
            return Vec::new();
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut bytes = Vec::new();
        let mut oversized = false;
        let mut chunk = [0u8; 8192];
        let complete = loop {
            match stdout.read(&mut chunk) {
                Ok(0) => break true,
                Ok(n) => {
                    if !oversized && bytes.len() + n <= OUTPUT_LIMIT {
                        bytes.extend_from_slice(&chunk[..n]);
                    } else {
                        oversized = true;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        break false;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break false,
            }
            if std::time::Instant::now() >= deadline {
                break false;
            }
        };
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let status = if complete {
            child.wait_timeout(remaining).ok().flatten()
        } else {
            None
        };
        if !status.is_some_and(|status| status.success()) {
            let _ = child.kill();
            let _ = child.wait();
            return Vec::new();
        }
        if oversized {
            return Vec::new();
        }
        bytes
    };
    #[cfg(not(unix))]
    let output = {
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = stdout
                .take((OUTPUT_LIMIT + 1) as u64)
                .read_to_end(&mut bytes);
            let _ = send.send(result.map(|_| bytes));
        });
        match child.wait_timeout(std::time::Duration::from_secs(2)) {
            Ok(Some(status)) if status.success() => {}
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Vec::new();
            }
        }
        match receive.recv_timeout(std::time::Duration::from_millis(500)) {
            Ok(Ok(bytes)) if bytes.len() <= OUTPUT_LIMIT => bytes,
            _ => return Vec::new(),
        }
    };
    let text = String::from_utf8_lossy(&output);
    let mut out = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if windows_shape {
            // `TCP  127.0.0.1:5214  0.0.0.0:0  LISTENING  <pid>`
            if fields.len() >= 4 && fields[0] == "TCP" && fields[3] == "LISTENING" {
                if let Ok(pid) = fields[fields.len() - 1].parse::<u32>() {
                    out.push(DshListener {
                        owner_pid: pid,
                        authority: fields[1].to_string(),
                    });
                }
            }
        } else {
            // `COMMAND  PID  USER  FD  TYPE  DEVICE  SIZE/OFF  NODE  NAME`
            if fields.len() >= 9 && fields[7] == "TCP" {
                if let Ok(pid) = fields[1].parse::<u32>() {
                    out.push(DshListener {
                        owner_pid: pid,
                        authority: fields[8].to_string(),
                    });
                }
            }
        }
    }
    out
}

// ============================================================
// 文件源（std::fs 只读投影）
// ============================================================

/// 生产文件源：`std::fs` 直读投影（存在性 / 目录枚举 / 预算内小文件）。
pub struct FsFileSource;

impl DshFileSource for FsFileSource {
    fn requires_physical_profile_lock(&self) -> bool {
        cfg!(unix)
    }

    fn exists(&self, path: &Path) -> bool {
        std::fs::symlink_metadata(path).is_ok()
    }

    /// 类型核验（审查 I3）：真实 stat 判目录（缺省实现只会退化成 exists）。
    fn is_dir(&self, path: &Path) -> bool {
        std::fs::symlink_metadata(path)
            .map(|m| m.is_dir())
            .unwrap_or(false)
    }

    fn list_dir(&self, path: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(path)
            .map(|entries| entries.flatten().map(|e| e.path()).collect())
            .unwrap_or_default()
    }

    fn list_dir_checked(&self, path: &Path) -> Result<Vec<PathBuf>, DshError> {
        let entries = std::fs::read_dir(path).map_err(|e| {
            DshError::new(
                codes::STATE_IO,
                format!("read directory {}: {e}", path.display()),
            )
        })?;
        entries
            .map(|entry| {
                entry.map(|item| item.path()).map_err(|e| {
                    DshError::new(
                        codes::STATE_IO,
                        format!("read directory entry {}: {e}", path.display()),
                    )
                })
            })
            .collect()
    }

    fn read_small(&self, path: &Path, max_bytes: u64) -> Result<Vec<u8>, DshError> {
        let meta = std::fs::metadata(path)
            .map_err(|e| DshError::new(codes::STATE_IO, format!("stat {}: {e}", path.display())))?;
        if meta.len() > max_bytes {
            return Err(DshError::new(
                codes::STATE_IO,
                format!(
                    "file over budget: {} ({} > {max_bytes} bytes)",
                    path.display(),
                    meta.len()
                ),
            ));
        }
        std::fs::read(path)
            .map_err(|e| DshError::new(codes::STATE_IO, format!("read {}: {e}", path.display())))
    }

    /// 实际路径解析（H05 审查 I5-r5①）：生产用**真实 canonicalize**——内核语义
    /// 一次解析全部中间目录符号链接、`..`、相对链接与多跳（词法折叠先行的
    /// 「目录链接逃逸」缺口在此结构性封死）；悬空/缺失/链接环 → None。
    /// 终点必须是常规文件（canonicalize 后 is_file）。
    fn resolve_actual_path(&self, path: &Path) -> Option<PathBuf> {
        let resolved = std::fs::canonicalize(path).ok()?;
        if resolved.is_file() {
            Some(resolved)
        } else {
            None
        }
    }
}

// ============================================================
// Runner（独立 args + Windows 防闪窗）
// ============================================================

/// 生产 runner：`std::process::Command` 独立 args（绝不拼 shell 字符串），
/// Windows 复用 [`crate::remote::NoWindow`] 抑制控制台闪窗（install.rs 注释的
/// 生产接线处）。exit 语义：无退出码（信号终止）按 -1 上抛观测产物。
/// 受控启动 Desktop 应用（仅由用户明确点击的 `dsh_restart_host` 调用）。
///
/// 只 spawn 已核验安装的可执行文件，不等待、不强杀；Windows 复用防闪窗。
/// 启动 Desktop 应用，并把它绑定到**指定实例的 home**。
///
/// 多实例场景下不传 `DSH_HOME` 的后果不是「随机」而是「确定的错」：应用会读
/// 默认 home（`~/.dsh`），于是「重开实例 B」实际打开的是实例 A 的 profile——
/// 用户看到的是另一个实例的会话与登录态。因此 home 是必填参数，不接受缺省。
pub fn spawn_desktop_app(
    executable: &std::path::Path,
    dsh_home: &std::path::Path,
) -> Result<(), DshError> {
    if !executable.is_file() {
        return Err(DshError::new(
            codes::DESKTOP_NOT_INSTALLED,
            format!("desktop executable missing: {}", executable.display()),
        ));
    }
    if dsh_home.as_os_str().is_empty() {
        return Err(DshError::new(
            codes::PROFILE_NOT_INITIALIZED,
            "refusing to launch desktop without an explicit DSH home",
        ));
    }
    let mut command = std::process::Command::new(executable);
    // 与 CLI 同一口径：显式覆盖 DSH_HOME，避免落到父进程恰好有的那个 home。
    command.env("DSH_HOME", dsh_home);
    command.no_window();
    command.spawn().map_err(|e| {
        DshError::new(
            codes::CLI_FAILED,
            format!("spawn desktop app {}: {e}", executable.display()),
        )
    })?;
    Ok(())
}

pub struct CliRunner;

impl DshCommandRunner for CliRunner {
    fn run(&self, request: &CommandRequest) -> Result<RunOutcome, DshError> {
        let mut cmd = std::process::Command::new(&request.program);
        cmd.args(&request.args);
        // 显式环境覆盖（DSH_HOME 等）：子进程在父环境之上继承这些键值——
        // CLI 因此被绑定到兔维斯**已发现**的 DSH home，而不是父进程恰好有的那个。
        for (key, value) in &request.env {
            cmd.env(key, value);
        }
        cmd.no_window();
        let output = cmd.output().map_err(|e| {
            DshError::new(
                codes::CLI_FAILED,
                format!("spawn {}: {e}", request.program.display()),
            )
        })?;
        Ok(RunOutcome {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

// ============================================================
// 时钟 / BindingRevoker
// ============================================================

/// 系统时钟（毫秒）。
pub struct SystemClock;

impl DshClock for SystemClock {
    fn now_ms(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64
    }
}

// ============================================================
// 单例装配（manager 与 T09 hub 共用同一 env Arc）
// ============================================================

/// 生产注入环境（OnceLock 单例；T09 `production_hub` 复用同一 Arc，不各起一套）。
///
/// **环境只在首次取用时读一次**（`OnceLock`）：`TUVIS_DSH_STATE_DIR` 属启动期
/// 配置，改它需重启应用（与 `TUVIS_HOME` 同）。解析逻辑见 [`DshEnvSnapshot`] /
/// [`resolve_state_dir`]（纯函数，测试不改进程环境）。
pub fn production_env() -> Arc<DshEnvironment> {
    static ENV: OnceLock<Arc<DshEnvironment>> = OnceLock::new();
    ENV.get_or_init(|| {
        Arc::new(build_env(
            &DshEnvSnapshot::from_process(),
            // 应用数据根（debug 认 `TUVIS_HOME`、release 一律真实目录）。
            crate::database::connection::app_data_home(),
        ))
    })
    .clone()
}

/// 生产卡片归属器（审查 I4①）：同一活体发现源，只认**已核验**候选；规范化
/// home 唯一匹配才给实例 ID（同 home 多实例 = 多义 → None；Web 候选未核验 →
/// 不参与归属）。发现失败按不可证处理（None），绝不把扫描路径变成硬故障。
pub struct ProductionInstanceAttributor;

impl super::instances::InstanceAttributor for ProductionInstanceAttributor {
    fn verified_instance_for_home(&self, home: &Path) -> Option<String> {
        let env = production_env();
        let discovery = super::identity::discover_detailed(&env).ok()?;
        let target_home = super::instances::normalize_profile(home);
        let mut hit: Option<String> = None;
        for candidate in discovery.candidates.iter().filter(|c| c.verified) {
            let candidate_home = super::instances::normalize_profile(
                &super::instances::session_home(&candidate.host.profile_dir),
            );
            if candidate_home == target_home {
                if hit.is_some() {
                    return None; // 同 home 多实例 → 多义，不猜
                }
                hit = Some(candidate.instance_id.clone());
            }
        }
        hit
    }

    /// R03（复审 P1）：已核验实例的扫描根枚举——同 home 多实例明确降级不返回。
    fn attributed_scan_roots(&self) -> Vec<(std::path::PathBuf, u32, String)> {
        let env = production_env();
        let Some(discovery) = super::identity::discover_detailed(&env).ok() else {
            return Vec::new();
        };
        // 按 session_home 分组已核验候选；同 home 多实例 → 明确降级（跳过）
        let mut by_home: std::collections::HashMap<
            std::path::PathBuf,
            (std::path::PathBuf, u32, String),
        > = std::collections::HashMap::new();
        let mut ambiguous: std::collections::HashSet<std::path::PathBuf> =
            std::collections::HashSet::new();
        for candidate in discovery.candidates.iter().filter(|c| c.verified) {
            let home = super::instances::normalize_profile(&super::instances::session_home(
                &candidate.host.profile_dir,
            ));
            if by_home.contains_key(&home) || ambiguous.contains(&home) {
                ambiguous.insert(home.clone());
                by_home.remove(&home);
                continue;
            }
            by_home.insert(
                home.clone(),
                (home, candidate.host.pid, candidate.instance_id.clone()),
            );
        }
        by_home.into_values().collect()
    }
}

/// 安装生产卡片归属器（应用 setup 一次性调用；monitor 3s 扫描从此拿到 DSH 卡的
/// 已核验实例 ID。测试不调用——monitor 扫描在测试里保持 None，零注入回归）。
pub fn install_instance_attributor() {
    crate::monitor::dsh::set_dsh_instance_attributor(std::sync::Arc::new(
        ProductionInstanceAttributor,
    ));
}

/// 实例配置唯一生产源；登记本身不授予任何写入能力。带活体环境打开
/// （`resolve_selected` / `resolve_session` 每次经同一 env 重新发现核验），并
/// 接线当代受信插件指纹探针（H05：来源 = 该实例 auth 域绑定账本的钉公钥 digest
/// ——H02 补测二实证机制；未授权/已撤权实例如实报「不可核验」拒绝）。
pub fn production_registry() -> Result<Arc<super::instances::InstanceRegistry>, DshError> {
    static REGISTRY: OnceLock<Result<Arc<super::instances::InstanceRegistry>, DshError>> =
        OnceLock::new();
    REGISTRY
        .get_or_init(|| {
            let env = production_env();
            let state_dir = checked_state_dir(&env)?;
            super::instances::InstanceRegistry::open_with_env(
                state_dir.join("instances.json"),
                Some(Arc::clone(&env)),
            )
            .map(|registry| {
                registry.with_identity_probe(Arc::new(BindingFingerprintProbe::production(
                    Arc::clone(&env),
                )))
            })
            .map(Arc::new)
        })
        .clone()
}

// ============================================================
// H05：按实例的装配内核与生产入口（§3.1：production 模块统一提供）
// ============================================================

/// 当代受信插件指纹探针（生产实现；[`InstanceIdentityProbe`] 的接线体）。
///
/// 来源 = **该实例 auth 域的绑定账本**（`<state_dir>/instances/<id>/bindings.json`
/// 最新 Bound 行钉住公钥的 SHA-256 digest）：无账本/无 Bound 行（未授权、已撤权、
/// 隔离测试环境）→ `Ok(None)` = 不可核验，resolve 据此拒绝——绝不当作通过。
pub struct BindingFingerprintProbe {
    /// 基础生产环境（实例子目录由探针按 instance_id 派生）。
    pub env: Arc<DshEnvironment>,
    /// R02（复审 P1）：测试注入槽——直取被测服务实例（绕开生产缓存单例）。
    /// 生产装配缺省 None → 走 `auth_for_instance`（与发送通道同源的缓存服务）。
    /// Arc+Mutex 形态：registry 先装探针、测试在服务创建后回填。
    pub service_override:
        std::sync::Arc<std::sync::Mutex<Option<Arc<super::auth::DshAuthService>>>>,
}

impl BindingFingerprintProbe {
    /// 生产形状构造（无注入）。
    pub fn production(env: Arc<DshEnvironment>) -> Self {
        Self {
            env,
            service_override: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }

    fn live_service(
        &self,
        instance_id: &str,
    ) -> Result<Option<Arc<super::auth::DshAuthService>>, DshError> {
        if let Some(svc) = self
            .service_override
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            return Ok(Some(Arc::clone(svc)));
        }
        // 生产：与发送通道同源的缓存单例（I1 终审统一域判定）。
        auth_for_instance(instance_id).map(Some)
    }
}

impl super::instances::InstanceIdentityProbe for BindingFingerprintProbe {
    fn current_fingerprint(
        &self,
        instance: &super::instances::SelectedInstance,
    ) -> Result<Option<String>, DshError> {
        // R02（复审 P1）：当代观测 = **本宿主代际内完成核验的活授权状态**；
        // 历史钉住（账本 Bound 行）只服务只读路径——两者严格分离。
        let svc = self.live_service(&instance.instance_id)?;
        match svc {
            Some(svc) => svc.current_generation_key_digest(),
            None => Ok(None), // 实例域不可用 = 无当代观测 → 不可核验
        }
    }
}

/// 实例 auth 域装配内核（生产单例与测试共用；测试注入 mem 监听端后可直接serve）。
/// state 子目录 / 授权私钥 / 绑定账本 / rendezvous / 端点全部按实例隔离。
pub fn init_instance_auth(
    base: &DshEnvironment,
    instance_id: &str,
) -> Result<DshAuthService, DshError> {
    DshAuthService::new_for_instance(clone_env(base), instance_id)
}

/// 生产指纹锚点：把已核验对端指纹写进**进程单例**登记（`production_registry()`）。
///
/// 语义（fail-closed）：空登记值 → 写入；同值 → 幂等；**非空且不同值 → 拒绝**。
/// 静默覆盖已钉身份等于把「身份不连续」降级成一次普通握手，那正是 H04 指纹门
/// 要防的事。
///
/// `registry` 是注入槽：生产走 [`Self::production`]（进程单例）；测试用
/// [`Self::with_registry`] 打真实 `InstanceRegistry`（tempdir），**不碰**真实状态
/// 目录，也不需要为测试再造一份同语义实现。
pub struct RegistryFingerprintAnchor {
    registry: Option<Arc<super::instances::InstanceRegistry>>,
}

impl RegistryFingerprintAnchor {
    /// 生产形态：每次锚定都取进程单例登记。
    pub fn production() -> Self {
        Self { registry: None }
    }

    /// 注入形态（测试）：锚到给定登记。
    pub fn with_registry(registry: Arc<super::instances::InstanceRegistry>) -> Self {
        Self {
            registry: Some(registry),
        }
    }

    fn registry(&self) -> Result<Arc<super::instances::InstanceRegistry>, DshError> {
        match &self.registry {
            Some(registry) => Ok(Arc::clone(registry)),
            None => production_registry(),
        }
    }
}

impl super::auth::RegistryFingerprintAnchor for RegistryFingerprintAnchor {
    fn commit_fingerprint(
        &self,
        instance_id: &str,
        fingerprint: &str,
        mode: &super::auth::FingerprintCommitMode,
    ) -> Result<super::auth::FingerprintCommitReceipt, DshError> {
        if fingerprint.is_empty() {
            return Err(DshError::new(
                codes::AUTH_SIGNATURE,
                "empty plugin fingerprint",
            ));
        }
        let registry = self.registry()?;
        // 提交前快照（锚 + 只读证据）一并进回执：补偿回滚必须能恢复到提交前
        // 的完整登记状态（第九轮 P1-2 Important-1）。
        let registered = registry.registered(instance_id)?;
        let previous = registered.identity_fingerprint;
        let previous_read_evidence = registered.read_evidence_fingerprint;
        match mode {
            super::auth::FingerprintCommitMode::Normal => {
                if !previous.is_empty() && previous != fingerprint {
                    return Err(DshError::new(
                        codes::AUTH_SIGNATURE,
                        "instance fingerprint differs",
                    ));
                }
                registry.compare_and_set_selected_fingerprint(
                    instance_id,
                    &previous,
                    fingerprint,
                )?;
            }
            super::auth::FingerprintCommitMode::Intent { selection_revision } => {
                if !previous.is_empty() && previous != fingerprint {
                    return Err(DshError::new(
                        codes::AUTH_SIGNATURE,
                        "instance fingerprint differs",
                    ));
                }
                registry.compare_and_set_intent_fingerprint(
                    instance_id,
                    *selection_revision,
                    &previous,
                    fingerprint,
                )?;
            }
            super::auth::FingerprintCommitMode::ExplicitRebind { expected_old } => {
                if expected_old.is_empty() || previous != *expected_old || previous == fingerprint {
                    return Err(DshError::new(
                        codes::AUTH_SIGNATURE,
                        "rebind fingerprint does not match",
                    ));
                }
                registry.compare_and_set_selected_fingerprint(
                    instance_id,
                    expected_old,
                    fingerprint,
                )?;
            }
        }
        Ok(super::auth::FingerprintCommitReceipt {
            instance_id: instance_id.to_owned(),
            changed: previous != fingerprint,
            previous,
            committed: fingerprint.to_owned(),
            previous_read_evidence,
        })
    }

    fn rollback_fingerprint(
        &self,
        receipt: &super::auth::FingerprintCommitReceipt,
    ) -> Result<(), DshError> {
        if receipt.changed {
            self.registry()?.restore_fingerprint_for_rollback(
                &receipt.instance_id,
                &receipt.committed,
                &receipt.previous,
                Some(&receipt.previous_read_evidence),
            )?;
        }
        Ok(())
    }

    fn current_fingerprint(&self, instance_id: &str) -> Result<String, DshError> {
        Ok(self
            .registry()?
            .registered(instance_id)?
            .identity_fingerprint)
    }

    fn accepts(&self, instance_id: &str, fingerprint: &str) -> bool {
        // 与 `anchor_fingerprint` 的接受条件逐条一致：空指纹不接；空登记值接；
        // 同值接；异值拒。查询失败一律拒（不可核验 ≠ 通过）。
        if fingerprint.is_empty() {
            return false;
        }
        match self.registry().and_then(|r| r.registered(instance_id)) {
            Ok(registered) => {
                registered.identity_fingerprint.is_empty()
                    || registered.identity_fingerprint == fingerprint
            }
            Err(_) => false,
        }
    }

    fn anchor_fingerprint(&self, instance_id: &str, fingerprint: &str) -> Result<(), DshError> {
        if fingerprint.is_empty() {
            return Err(DshError::new(
                codes::AUTH_SIGNATURE,
                "empty plugin fingerprint cannot be anchored",
            ));
        }
        let registry = self.registry()?;
        let registered = registry.registered(instance_id)?;
        if registered.identity_fingerprint == fingerprint {
            return Ok(());
        }
        if !registered.identity_fingerprint.is_empty() {
            return Err(DshError::new(
                codes::AUTH_SIGNATURE,
                format!(
                    "instance {instance_id} is anchored to a different plugin identity; re-select/re-authorize required"
                ),
            ));
        }
        // M03：走 CAS（一次锁内比较 + 写）。此处 `expected` 就是刚读到的登记值，
        // 因此若在读取与写入之间有另一条路径提交了不同指纹，CAS 会失败而不是覆盖。
        registry.compare_and_set_fingerprint(
            instance_id,
            &registered.identity_fingerprint,
            fingerprint,
        )?;
        Ok(())
    }
}

/// 实例管理器装配内核（生产单例与测试共用）：env.home = 该实例实际使用的
/// DSH home（官方 CLI 经 `DSH_HOME` 显式下发），CLI `--profile` 与核验落点按
/// 实例 kind/profile 决定，账本按实例落盘。
pub fn init_instance_manager(
    base: &DshEnvironment,
    instance: &super::instances::SelectedInstance,
    revoker: Arc<dyn super::install::BindingRevoker>,
) -> DshPluginManager {
    let mut env = clone_env(base);
    // 该实例实际使用的 DSH home = 装配单源 accessor（登记核验 home 优先，缺失时
    // 按 profile 入口权威推导；Round-8 P1-3 起 switch 旧钥探测共用同一推导——
    // env.home 本身是兔维斯推导值，自定义 DSH_HOME 宿主实例不能用它）。
    env.home = super::instances::verified_instance_home(instance);
    let target = super::install::InstanceTarget {
        instance_id: instance.instance_id.clone(),
        kind: instance.kind,
        profile_dir: instance.profile_dir.clone(),
    };
    DshPluginManager::for_instance(env, revoker, target)
}

type InstanceAuthCache = Mutex<std::collections::HashMap<String, Arc<DshAuthService>>>;
static INSTANCE_AUTHS: OnceLock<InstanceAuthCache> = OnceLock::new();
static LEGACY_AUTH: OnceLock<Result<Arc<DshAuthService>, AuthInitFailure>> = OnceLock::new();

/// 生产授权域取用入口（§3.1 `auth_for_instance`）：**必须已登记**（候选配置登记
/// 即可——I2 阶段边界：首次授权不依赖受信身份落账），进程内按实例缓存（同一实例
/// 恒返回同一服务——闸门/受信 Peer/意图是内存态，重建即失联）。
///
/// H05 审查 C2：缓存创建**串行化**（get-or-create 全程持同一把映射锁）——
/// 「查缓存→放锁→构造→再插」的旧窗口让两次并发首调各得一个 Arc（独立
/// pending/peer/gate/内存账本），撤权翻不了另一个域的闸门、旧账本还可能覆盖墓碑。
pub fn auth_for_instance(instance_id: &str) -> Result<Arc<DshAuthService>, DshError> {
    let auths = INSTANCE_AUTHS.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let mut guard = auths.lock().unwrap();
    if let Some(existing) = guard.get(instance_id) {
        return Ok(Arc::clone(existing));
    }
    let registry = production_registry()?;
    registry.registered(instance_id)?;
    // P1-1a：**只在生产装配点**接线登记指纹锚点——`init_instance_auth` 是生产与
    // 测试共用内核，在那里接 `production_registry()` 会让每个用到该内核的测试都
    // 写真实状态目录（破坏 hermetic）。授权成立（批准/自动授权/已钉钥重连）时把
    // 当代核验过的对端指纹写回登记；否则首绑之后 `fingerprint_gate` 拿到的仍是
    // 候选登记时的空串，所有写入恒 `DSH_INSTANCE_UNVERIFIED`。
    let svc = Arc::new(
        init_instance_auth(&production_env(), instance_id)?
            .with_registry_anchor(Arc::new(RegistryFingerprintAnchor::production())
                as Arc<dyn super::auth::RegistryFingerprintAnchor>),
    );
    // P1-1：开关执行门（授权提交点）登记——auth 不反向依赖 switch，闭包在装配
    // 点接线（job-admission gate 同款注入模式）。门本体在 switch 域
    // （`authorization_gate_check`）：读持久化记录核对 enabled + 布防代次；
    // 服务不可用按未开启拒绝（fail-closed）。
    let gated_instance = instance_id.to_owned();
    svc.set_switch_gate(Arc::new(
        move |armed: Option<u64>| match switch_for_instance(&gated_instance) {
            Ok(service) => service.authorization_gate_check(armed),
            Err(_) => Err(super::switch::switch_off_error()),
        },
    ));
    guard.insert(instance_id.to_owned(), Arc::clone(&svc));
    Ok(svc)
}

/// 为目标选择唯一授权域：登记实例使用实例域，未登记实例只在无选择且活体唯一时使用旧域。
pub fn auth_for_target(instance_id: &str) -> Result<Arc<DshAuthService>, DshError> {
    let registry = production_registry()?;
    if let Some(selected) = registry.selected()? {
        if selected.instance_id != instance_id {
            return Err(DshError::new(
                codes::SESSION_OWNERSHIP_UNPROVEN,
                "target is not the selected instance",
            ));
        }
        return auth_for_instance(instance_id);
    }
    let host = registry.resolve_selected()?;
    if host.host_id != instance_id {
        return Err(DshError::new(
            codes::SESSION_OWNERSHIP_UNPROVEN,
            "target is not the unique live instance",
        ));
    }
    match registry.registered(instance_id) {
        Ok(_) => auth_for_instance(instance_id),
        Err(e) if e.code == codes::HOST_NOT_RUNNING => {
            legacy_auth_state().map_err(|e| e.as_error())
        }
        Err(e) => Err(e),
    }
}

/// 同一实例的 manager 与 auth 缓存单例（H10：状态查询与命令必须共用）。
///
/// 两者来自各自的生产缓存入口，保证安装/取消作业记录与授权状态、撤权闸门
/// 都落在同一个运行时对象上。
pub fn instance_services(
    instance_id: &str,
) -> Result<(Arc<DshPluginManager>, Arc<DshAuthService>), DshError> {
    Ok((
        manager_for_instance(instance_id)?,
        auth_for_instance(instance_id)?,
    ))
}

/// 实例管理器的配置键（缓存复用判据）：kind、登记的物理 profile、home 证据。
/// 任一变化（如旧登记经 `register_candidate_with_home` 补记 `dsh_home`）都构成
/// 新配置——旧 Arc 持有的环境不再对位，必须重装配。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ManagerConfigKey {
    kind: super::instances::HostKind,
    profile_dir: std::path::PathBuf,
    dsh_home: Option<std::path::PathBuf>,
}

impl ManagerConfigKey {
    fn for_instance(instance: &super::instances::SelectedInstance) -> Self {
        Self {
            kind: instance.kind,
            profile_dir: instance.profile_dir.clone(),
            dsh_home: instance.dsh_home.clone(),
        }
    }
}

struct CachedManagerEntry {
    config: ManagerConfigKey,
    /// 本条目管理器装配时绑定的（规范化）home——命中判据的对位面（复审
    /// Important-1）：登记键不变而绑定证据变化时，命中探针据此触发重装配。
    assembled_home: std::path::PathBuf,
    manager: Arc<DshPluginManager>,
}

type ManagerDecorator = Arc<dyn Fn(&mut DshPluginManager, &str) + Send + Sync>;

/// 生产与测试共用的实例管理器缓存内核（H05 审查 C2 串行化 + 重装配语义）：
///
/// * 同一实例 ID 在配置（kind/物理 profile/home 证据）不变时复用同一
///   `Arc<DshPluginManager>`（get-or-create 全程持锁，唯一 Arc）；
/// * 登记配置变化（补记 `dsh_home` 等）：**活跃作业期间保守保留旧管理器**
///   （仍可查询/取消），作业收尾后的下次取用按新登记重装配；重装配前在旧
///   管理器内部锁下标记冻结（旧句柄禁止 `start`，与并发 `start` 串行）；
/// * 旧作业的 `status`/`job`/`cancel` 与终态回执跨重装配保留（账本按实例
///   落盘，重装配继续用同一实例账本；`assemble` 从账本装载终态回执并顶高
///   job_id 序号，固定时钟下也不与已落盘 ID 冲突）；
/// * 生产入口（[`manager_for_instance`]）与本缓存走同一 `get_or_create`
///   内核；生产经 [`Self::set_decorator`] 在每次新建时接 authorization
///   reader，测试注入自己的 revoker，两者不各造缓存逻辑。
#[derive(Default)]
pub struct InstanceManagerCache {
    entries: Mutex<std::collections::HashMap<String, CachedManagerEntry>>,
    decorate: Mutex<Option<ManagerDecorator>>,
}

/// 取用边界共用的证据复核（复审 P1）：登记 home 必须可证明（登记值自洽，
/// 或经可信绑定账本唯一恢复），且不得与已核验活体的 home 冲突。管理器缓存
/// 装配与**作业受理门**（`start` 前置复核）共用同一实现——缓存命中返回的
/// 旧句柄同样不得在绑定证据失效或活体冲突后接收新作业。
pub(crate) fn verify_registered_instance_home(
    base: &DshEnvironment,
    registry: &super::instances::InstanceRegistry,
    instance_id: &str,
) -> Result<std::path::PathBuf, DshError> {
    let instance = registry.registered(instance_id)?;
    let physical_profile = std::fs::canonicalize(&instance.profile_dir).map_err(|_| {
        DshError::new(
            codes::INSTANCE_UNVERIFIED,
            format!(
                "instance {instance_id} registered profile is missing; start the original instance and re-select it"
            ),
        )
    })?;
    let proven_home = match super::instances::resolve_registered_home(
        Some(base),
        &instance,
        &physical_profile,
    ) {
        super::instances::RegisteredHomeResolution::Proven(home) => home,
        super::instances::RegisteredHomeResolution::Unproven(reason) => {
            return Err(DshError::new(
                    codes::INSTANCE_UNVERIFIED,
                    format!(
                        "instance {instance_id} dsh home unproven ({reason}); start the original instance and re-select it"
                    ),
                ));
        }
    };
    // 活体冲突门：已核验候选的原始入口推导 home 与登记证据冲突 → 拒绝。
    // 离线（发现失败/无已核验候选）不构成冲突，不阻断离线兼容；冲突判定
    // 只认**已核验**候选的原始入口推导 home，不扫 Path 猜测。
    if let Ok(discovery) = super::identity::discover_detailed(base) {
        let registered_home = super::instances::normalize_profile(&proven_home);
        for candidate in discovery
            .candidates
            .iter()
            .filter(|candidate| candidate.verified && candidate.instance_id == instance_id)
        {
            let live_home = super::instances::normalize_profile(&super::instances::session_home(
                &candidate.host.profile_dir,
            ));
            if live_home != registered_home {
                return Err(DshError::new(
                    codes::INSTANCE_UNVERIFIED,
                    format!(
                        "instance {instance_id} live home conflicts with registered evidence (live {live_home:?} vs registered {registered_home:?}); re-select required"
                    ),
                ));
            }
        }
    }
    Ok(proven_home)
}

/// 生产作业受理门内核（可注入环境；[`manager_for_instance`] 的装饰器闭包与
/// 回归测试共用同一实现——复审 Major-2 收紧后的自消费例外语义在此集中）。
///
/// * `verify_registered_instance_home` 正常证明 → home 对位装配值，不一致报
///   `INSTANCE_SWITCHED`；
/// * `INSTANCE_UNVERIFIED` 且 `scope.allow_consumed_evidence`（仅本作业撤权后
///   的 CLI 门置位）：例外只覆盖「Bound 证据被自己消费」——装配 home 仍须与
///   登记物理 profile 自洽，且无已核验活体的 home 冲突；任一不成立按原错误
///   拒绝（活体冲突不得被吞）；
/// * 其余错误原样上抛。
pub(crate) fn verify_job_admission(
    base: &DshEnvironment,
    registry: &super::instances::InstanceRegistry,
    instance_id: &str,
    assembled_home: &std::path::Path,
    scope: super::install::AdmissionScope,
) -> Result<std::path::PathBuf, DshError> {
    // `assembled_home` 约定为**装配时已规范化的固定基线**（装饰器传入
    // `normalize_profile(manager.assembled_home())` 的保存值）——此处不得
    // 再规范化：装配后 home 路径被重定向（A→B 符号链接）时，重新解析会把
    // 固定基线漂移到 B，`INSTANCE_SWITCHED` 对位失效（复审）。
    let proven = match verify_registered_instance_home(base, registry, instance_id) {
        Ok(home) => super::instances::normalize_profile(&home),
        Err(e) if e.code == codes::INSTANCE_UNVERIFIED && scope.allow_consumed_evidence => {
            let registered = registry.registered(instance_id)?;
            let physical = std::fs::canonicalize(&registered.profile_dir).map_err(|_| e.clone())?;
            // 固定基线当前解析必须仍等于自身（复审）：装配后 home 路径被
            // 重定向（A → 符号链接到 B）时，即使物理 profile 不变，A 的实际
            // 落点已不是装配时的 B 之外的第三处——本作业按 A 证明，执行期
            // A 解析到别处 = 基线漂移，拒绝而不是用解析结果替换基线。
            match std::fs::canonicalize(assembled_home) {
                Ok(resolved) if resolved == assembled_home => {}
                Ok(resolved) => {
                    return Err(DshError::new(
                        codes::INSTANCE_SWITCHED,
                        format!(
                            "management handle is bound to {assembled_home:?} but that path now resolves to {resolved:?}; re-select required"
                        ),
                    ));
                }
                Err(_) => return Err(e),
            }
            if !super::instances::proven_home_self_consistent(
                assembled_home,
                registered.kind,
                &physical,
            ) {
                return Err(e);
            }
            if let Ok(discovery) = super::identity::discover_detailed(base) {
                for candidate in discovery
                    .candidates
                    .iter()
                    .filter(|c| c.verified && c.instance_id == instance_id)
                {
                    let live_home = super::instances::normalize_profile(
                        &super::instances::session_home(&candidate.host.profile_dir),
                    );
                    if live_home != assembled_home {
                        return Err(DshError::new(
                            codes::INSTANCE_UNVERIFIED,
                            format!(
                                "instance {instance_id} live home conflicts with registered evidence (live {live_home:?} vs registered {assembled_home:?}); re-select required"
                            ),
                        ));
                    }
                }
            }
            assembled_home.to_path_buf()
        }
        Err(e) => return Err(e),
    };
    if proven != assembled_home {
        return Err(DshError::new(
            codes::INSTANCE_SWITCHED,
            format!(
                "management handle is bound to {assembled_home:?} but current evidence proves {proven:?}; re-select required"
            ),
        ));
    }
    Ok(proven)
}

impl InstanceManagerCache {
    /// 生产装配钩子：每次**新建**管理器后调用（重装配同样生效）。
    /// `DshPluginManager` 已按实例配置装配，装饰器只补生产接线（授权读取面）。
    pub fn set_decorator(&self, decorate: ManagerDecorator) {
        *self.decorate.lock().unwrap() = Some(decorate);
    }

    /// 取用/装配该实例的管理器（登记为唯一配置源）。
    pub fn get_or_create(
        &self,
        base: &DshEnvironment,
        registry: &super::instances::InstanceRegistry,
        instance_id: &str,
        revoker: Arc<dyn super::install::BindingRevoker>,
    ) -> Result<Arc<DshPluginManager>, DshError> {
        // 全程持锁（H05 审查 C2）：查缓存→比对→重装配→插入不放开，并发首调
        // 与配置切换都只产出一条 Arc。
        let mut entries = self.entries.lock().unwrap();
        let instance = registry.registered(instance_id)?;
        let config = ManagerConfigKey::for_instance(&instance);
        if let Some(entry) = entries.get(instance_id) {
            if entry.config == config {
                // 命中探针（复审 Important-1）：登记键不变时绑定证据仍可能
                // 变化（dsh_home=None 的记录经账本恢复的 home 从 A 切到 B）。
                // 这里跑**廉价**证据复核（登记读 + canonicalize + 账本读，
                // 不做进程发现）——当前可证明 home 与装配 home 不一致即视为
                // 配置变化，走下方重装配；活体冲突仍由受理门在 start 前把关。
                // 证据读失败（profile 消失/账本不可读）按保守命中处理：
                // 受理门会在 start 前拒绝。
                let still_matches = match std::fs::canonicalize(&instance.profile_dir) {
                    Ok(physical) => {
                        match super::instances::resolve_registered_home(
                            Some(base),
                            &instance,
                            &physical,
                        ) {
                            super::instances::RegisteredHomeResolution::Proven(home) => {
                                super::instances::normalize_profile(&home) == entry.assembled_home
                            }
                            super::instances::RegisteredHomeResolution::Unproven(_) => true,
                        }
                    }
                    Err(_) => true,
                };
                if still_matches {
                    // 缓存命中：新作业的证据复核统一落在**作业受理门**
                    // （`start` 前置复核）——命中句柄不得在绑定证据失效或
                    // 活体 home 冲突后接收新作业；旧作业查询/取消保持可用。
                    return Ok(Arc::clone(&entry.manager));
                }
            }
        }
        // M06 统一判定 + 活体冲突（装配/重装配路径；失败先于冻结，不动旧条目）。
        let mut instance = instance;
        let proven_home = verify_registered_instance_home(base, registry, instance_id)?;
        let assembled_home = super::instances::normalize_profile(&proven_home);
        instance.dsh_home = Some(proven_home);
        if let Some(entry) = entries.get(instance_id) {
            // 冻结与活跃检查在管理器内部锁下原子完成（复审 Important）：活跃 →
            // 保留**冻结**的旧管理器（可查询、可取消；禁止新作业），作业收尾后
            // 下次取用按新登记重装配；空闲 → 直接替换。
            if entry.manager.retire_and_report_active() {
                return Ok(Arc::clone(&entry.manager));
            }
        }
        let mut manager = init_instance_manager(base, &instance, revoker);
        if let Some(decorate) = self.decorate.lock().unwrap().as_ref() {
            decorate(&mut manager, instance_id);
        }
        let manager = Arc::new(manager);
        entries.insert(
            instance_id.to_owned(),
            CachedManagerEntry {
                config,
                assembled_home,
                manager: Arc::clone(&manager),
            },
        );
        Ok(manager)
    }
}

/// 生产管理器取用入口（§3.1 `manager_for_instance`）：必须已登记（候选配置登记
/// 即可——I2：安装固定包不得依赖未完成的授权）；经 [`InstanceManagerCache`]
/// 按实例配置缓存/重装配；撤权面绑定**同一**实例 auth 域的缓存单例（卸载撤权
/// 必须翻转活服务的闸门，不能在一次性重建的域里空转）。
///
/// * H05 审查 C2：缓存创建**串行化**（get-or-create 全程持锁，唯一 Arc——同
///   [`auth_for_instance`]）。
/// * H05 审查 I2：授权读取面**可选注入**（auth 域不可用 → 状态面 Unbound，管理
///   作业照常可建）；撤权 resolver 返回 `Result`，域不可用时走账本只写兜底。
pub fn manager_for_instance(instance_id: &str) -> Result<Arc<DshPluginManager>, DshError> {
    static MANAGERS: OnceLock<InstanceManagerCache> = OnceLock::new();
    let cache = MANAGERS.get_or_init(|| {
        let cache = InstanceManagerCache::default();
        cache.set_decorator(Arc::new(
            |manager: &mut DshPluginManager, instance_id: &str| {
                if let Ok(auth) = auth_for_instance(instance_id) {
                    manager.attach_authorization_reader(Arc::new(super::auth::AuthServiceReader(
                        auth,
                    )));
                }
                // 作业受理门（复审 P1 + Important-1）：每次 start 前复核登记
                // home 证据、活体冲突，并**对位本管理器装配时绑定的 home**——
                // 登记键不变而绑定证据从 A 切到 B 时，按 A 装配的旧句柄不得
                // 按 B 的证据过门后继续下发 A；旧作业的查询/取消不受影响。
                // 门逻辑在 [`verify_job_admission`]（可注入环境，测试直驱）。
                let gated_id = instance_id.to_owned();
                let assembled_home = super::instances::normalize_profile(manager.assembled_home());
                manager.attach_job_admission_gate(Arc::new(
                    move |scope: super::install::AdmissionScope| {
                        let registry = production_registry()?;
                        let base = production_env();
                        verify_job_admission(&base, &registry, &gated_id, &assembled_home, scope)
                    },
                ));
                // P1-2 插件身份退役面：旧钥退役（卸载完成段/升级兼容清理）时清空
                // 实例登记的指纹锚——再开启的新钥走首绑 TOFU；旧钥由撤权墓碑拦截，
                // 不因锚清空复活。
                manager.attach_identity_retirer(Arc::new(move |retired_id: &str| {
                    production_registry()?.retire_identity_fingerprint(retired_id)
                }));
            },
        ));
        cache
    });
    let registry = production_registry()?;
    let base = production_env();
    let id_for_revoker = instance_id.to_owned();
    let revoker = InstanceBindingRevoker::with_resolver(
        Arc::clone(&base),
        instance_id.to_owned(),
        Arc::new(move || auth_for_instance(&id_for_revoker)),
    );
    cache.get_or_create(&base, &registry, instance_id, Arc::new(revoker))
}

/// 实例域绑定撤权器（H05）：卸载作业的撤销落点 = **该实例** auth 域内的
/// 全部匹配绑定。auth 域经 resolver 取用（生产 = 缓存单例，翻转活服务的闸门；
/// 测试注入已装配的域）；域不可用时的账本只写兜底写**实例账本**
/// （`instance_scoped_env` 把状态目录指到该实例子目录），不留陈旧授权。
pub struct InstanceBindingRevoker {
    instance_id: String,
    base_env: Arc<DshEnvironment>,
    resolver: Arc<dyn Fn() -> Result<Arc<DshAuthService>, DshError> + Send + Sync>,
}

impl InstanceBindingRevoker {
    /// 生产装配：resolver = [`auth_for_instance`] 缓存单例。
    pub fn new(base_env: Arc<DshEnvironment>, instance_id: String) -> Self {
        let id = instance_id.clone();
        Self::with_resolver(
            base_env,
            instance_id,
            Arc::new(move || auth_for_instance(&id)),
        )
    }

    /// 注入 resolver（测试面：直接指向已装配的实例域，不经生产单例）。
    pub fn with_resolver(
        base_env: Arc<DshEnvironment>,
        instance_id: String,
        resolver: Arc<dyn Fn() -> Result<Arc<DshAuthService>, DshError> + Send + Sync>,
    ) -> Self {
        Self {
            instance_id,
            base_env,
            resolver,
        }
    }
}

impl super::install::BindingRevoker for InstanceBindingRevoker {
    fn revoke_profile(&self, profile_id: &str) -> Result<(), DshError> {
        super::auth::ProductionBindingRevoker::revoke_with(
            (self.resolver)(),
            &super::auth::instance_scoped_env(&self.base_env, &self.instance_id)?,
            profile_id,
        )
    }
}

/// 生产管理器入口（`env` 与 [`production_env`] 共享同一批适配器 Arc）。
///
/// **H05 委托语义（审查 C3 收紧）**：三种情形严格区分——
/// * 真无选择 → 旧桌面单例（唯一允许的兼容回落）；
/// * 选择/登记面**读取失败** → 错误原样上抛（不折叠成「无选择」、不静默换
///   legacy 域——legacy 管理器指向 desktop profile，静默回落会在已选 Web 实例
///   时操作错误的目标）；
/// * 已选实例的**按实例装配失败** → 错误原样上抛（绝不回落 legacy）。
pub fn production_manager() -> Result<Arc<DshPluginManager>, DshError> {
    // I1 终审收尾：管理装配与绑定入口/状态面**同一域判定**——
    // 已选定 → 该实例域；无选定 + 恰一个已登记 → 该实例域（管理目标唯一合理）；
    // 无选定 + 无登记 → legacy 兼容；无选定 + 多登记 → 歧义拒绝（须先选择）。
    match binding_domain()? {
        AuthDomain::Instance(id) => manager_for_instance(&id),
        AuthDomain::Legacy => match resolve_selected_instance()? {
            SelectedInstanceResolution::NoSelection => Ok(legacy_production_manager()),
            SelectedInstanceResolution::Selected(id) => manager_for_instance(&id),
        },
    }
}

/// 绑定域判定（决策本身；`binding_auth` 与 `production_manager` 共用）。
pub(crate) fn binding_domain() -> Result<AuthDomain, DshError> {
    let registry = production_registry()?;
    let selected = registry.selected()?.map(|s| s.instance_id);
    let registered = registry.registered_ids()?;
    auth_domain_for_binding(selected.as_deref(), &registered)
}

/// 「远程控制 DSH」开关服务缓存单例（Task 4；按实例一份，manager/auth 与
/// 开关驱动器共享同一运行时对象——作业受理、意图布防与账本提交同源）。
pub fn switch_for_instance(
    instance_id: &str,
) -> Result<Arc<super::switch::RemoteSwitchService>, DshError> {
    static SWITCHES: OnceLock<
        Mutex<std::collections::HashMap<String, Arc<super::switch::RemoteSwitchService>>>,
    > = OnceLock::new();
    let switches = SWITCHES.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    let mut guard = switches.lock().unwrap();
    if let Some(existing) = guard.get(instance_id) {
        return Ok(Arc::clone(existing));
    }
    let env = production_env();
    let registry = production_registry()?;
    let services: Arc<super::switch::SwitchServices> = {
        let id = instance_id.to_owned();
        Arc::new(move || instance_services(&id))
    };
    // 升级证据只读源（第九轮 P2 复审 Important 修复）：证据读取零装配副作用
    // ——与 services（驱动器装配闭包）严格分开。
    let evidence = upgrade_evidence_source(
        Arc::clone(&env),
        Arc::clone(&registry),
        instance_id.to_owned(),
    );
    let service = Arc::new(super::switch::RemoteSwitchService::new(
        env,
        registry,
        instance_id.to_owned(),
        services,
        evidence,
    )?);
    guard.insert(instance_id.to_owned(), Arc::clone(&service));
    Ok(service)
}

/// 升级证据只读源（生产装配；第九轮 P2 复审 Important 修复）：三证的**纯盘读**
/// 投影——绝不走 [`instance_services`]/[`manager_for_instance`]（两者会装配授权
/// 域：创建私钥 `load_or_create_key`、消费待落盘撤权标记 `consume_revoke_pending`、
/// 进生产缓存），安装档用**一次性直构管理器**的只读 `status`（无装饰器、无授权
/// reader、revoker 惰性不被调用）。`env`/`registry` 参数化——测试夹具与集成回归
/// 注入自己的盘面（生产钩子传生产单例），共用同一判据实现。
pub fn upgrade_evidence_source(
    env: Arc<DshEnvironment>,
    registry: Arc<super::instances::InstanceRegistry>,
    instance_id: impl Into<String>,
) -> Arc<super::switch::SwitchEvidenceSource> {
    let instance_id = instance_id.into();
    Arc::new(move || {
        let bound = super::auth::latest_bound_evidence_for(&env, &instance_id);
        let (bound_digest, bound_revocation_pending) = match bound {
            Some((digest, pending)) => (Some(digest), pending),
            // 无 Bound 行/不可证明 → 证一不成立；不再读安装档（无谓盘读）。
            None => {
                return super::switch::UpgradeEvidenceFacts {
                    bound_digest: None,
                    bound_revocation_pending: false,
                    installed: false,
                };
            }
        };
        // 待落盘撤权未收敛 = 撤权意图在先 → 证据不足（安装档不再参与；
        // 标记的补刀只发生在真实域装配）。
        let installed =
            !bound_revocation_pending && read_only_install_state(&env, &registry, &instance_id);
        super::switch::UpgradeEvidenceFacts {
            bound_digest,
            bound_revocation_pending,
            installed,
        }
    })
}

/// 只读安装档（第九轮 P2 复审 Important 修复）：**不走生产管理器缓存/装饰器**
/// ——装饰器会装配授权域（`auth_for_instance`）。直构一次性实例管理器取
/// `status().installation`（构造 = 管理账本盘读；status = profile 文件态读）。
/// 登记缺失 / 状态不可读按不在场处理（证据不足口径，不冒充状态）。
fn read_only_install_state(
    env: &DshEnvironment,
    registry: &Arc<super::instances::InstanceRegistry>,
    instance_id: &str,
) -> bool {
    let Ok(registered) = registry.registered(instance_id) else {
        return false;
    };
    // revoker 惰性：只读面不做任何撤权，resolver 永不被调用。
    let revoker = InstanceBindingRevoker::with_resolver(
        Arc::new(clone_env(env)),
        instance_id.to_owned(),
        Arc::new(|| -> Result<Arc<DshAuthService>, DshError> {
            Err(DshError::new(
                codes::IPC_STATE,
                "read-only install state never revokes",
            ))
        }),
    );
    let manager = DshPluginManager::for_instance(
        clone_env(env),
        Arc::new(revoker),
        super::install::InstanceTarget {
            instance_id: registered.instance_id,
            kind: registered.kind,
            profile_dir: registered.profile_dir,
        },
    );
    matches!(
        manager.status().map(|status| status.installation),
        Ok(super::model::InstallationState::Installed | super::model::InstallationState::Conflict)
    )
}

/// P1-3（Round-7 Task 3）启动即恢复：兔维斯启动装配段调用（lib.rs setup）——
/// 扫描已登记实例的非终态开关记录（[`super::switch::incomplete_startup_records`]，
/// 实例状态目录枚举），逐实例拉起本代驱动器（[`RemoteSwitchService::resume_incomplete`]，
/// 同代幂等）。**不依赖任何 UI 查询**——等待卸载/授权中的开关作业随应用启动续跑。
///
/// fail-soft 纪律：登记面打不开 / 实例未登记 / 服务装配失败 / 记录不可读一律
/// 记日志继续——坏记录不得阻断应用启动；设置页 `view()` 仍是次级恢复点。
/// 返回拉起驱动器的实例数（观测面：日志消费）。
///
/// 调用时序约束：必须在**有 tokio 运行时上下文**的调用点使用（`spawn_driver`
/// 内部 `tokio::spawn`）——lib.rs setup 主线程无上下文，经 `tauri::async_runtime`
/// 包装调用。
pub fn resume_switch_drivers_at_startup() -> usize {
    let env = production_env();
    let pending = super::switch::incomplete_startup_records(&env);
    if pending.is_empty() {
        return 0;
    }
    // 登记面 fail-soft：打不开 = 本轮不恢复（view() 次级恢复点接管）。
    let registry = match production_registry() {
        Ok(registry) => registry,
        Err(e) => {
            log::warn!(
                "dsh switch startup resume skipped (registry unavailable): {} ({})",
                e.code,
                e.detail
            );
            return 0;
        }
    };
    let mut resumed = 0usize;
    for instance_id in pending {
        // 只恢复已登记实例（目录残留的未登记实例不拉驱动器；登记读失败跳过）。
        if let Err(e) = registry.registered(&instance_id) {
            log::warn!(
                "dsh switch startup resume skipped unregistered instance {instance_id}: {} ({})",
                e.code,
                e.detail
            );
            continue;
        }
        match switch_for_instance(&instance_id) {
            Ok(service) => {
                if service.resume_incomplete() {
                    resumed += 1;
                }
            }
            Err(e) => {
                log::warn!(
                    "dsh switch startup resume skipped instance {instance_id}: {} ({})",
                    e.code,
                    e.detail
                );
            }
        }
    }
    if resumed > 0 {
        log::info!("dsh switch: resumed {resumed} in-progress switch job(s) at startup");
    }
    resumed
}

/// P1-3 选择后补位（`dsh_instance_select` 的恢复闭包）：新选实例若有非终态
/// 开关记录 → 拉起本代驱动器（同代幂等）。fail-soft：服务装配失败只记日志——
/// 选择本身已成功，不得因开关恢复面失败而报错。
///
/// P1-2（Round-8 Task 2）扩展：**已开启（任意相位，含 Ready 终态）**的新选实例
/// 一并补起授权监听（[`ensure_listener_for_enabled_switch`]）。启动段只为「当时
/// 选中」的实例 serve（lib.rs → [`production_auth_state`]）；重启后切回其余已
/// 开启实例若不起监听，其插件既无当代 rendezvous 记录也无人 accept——发送/
/// 控制台直到重新绑定都死路。serve 幂等（已 bind 不重复 bind，接受循环/活体
/// 跟踪 CAS 补位）；每连接身份/归属/开关核验照旧，不因补位放宽。fail-soft：
/// 监听补位失败（装配/serve）只记日志，绝不反噬选择本身。
///
/// 第九轮 P2 扩展：记录**缺席**不再一刀切跳过——升级恢复三证齐（Bound 行 +
/// 安装在场 + 登记锚与账本一致）的「开关功能上线前已绑定+安装」用户切回时按
/// 升级证据补起（[`ensure_listener_for_selected_switch`]）；记录**损坏**（读取
/// 失败）不是缺席，不落升级恢复。
pub fn resume_switch_driver_after_select(instance_id: &str) {
    let service = match switch_for_instance(instance_id) {
        Ok(service) => service,
        Err(e) => {
            log::warn!(
                "dsh switch resume after select skipped instance {instance_id}: {} ({})",
                e.code,
                e.detail
            );
            return;
        }
    };
    service.resume_incomplete();
    // 记录读取结果**整体**进内核（第九轮 P2）：缺席走升级恢复三证；读取失败
    // 不是缺席——不落升级恢复，只记日志（fail-soft，不阻塞已成功的选择）。
    let record_read = service.read_raw();
    if let Err(e) = &record_read {
        log::warn!(
            "dsh switch record unreadable after select for instance {instance_id}: {} ({})",
            e.code,
            e.detail
        );
    }
    let bring_up = ensure_listener_for_selected_switch(
        record_read,
        // 升级恢复三证（Bound 行 + 安装在场 + 登记锚与账本一致）：记录缺席时
        // 判定「开关功能上线前已绑定+安装」的合法升级用户。
        || service.upgrade_recovery_evidence(),
        // 装配面惰性：只在记录已开启（或升级证据齐）时才装配 auth 域——未开启
        // 实例的单纯选择不得产生装配副作用（域装配会 load_or_create_key 落钥）。
        || auth_for_instance(instance_id),
        |auth| {
            // 与 lib.rs 启动段同一 spawn 形状：选择命令壳跑在 spawn_blocking 里
            //（无 await 上下文），serve 移交后台任务；serve 失败只记日志——
            // 监听缺席由活体跟踪拍/下一次选择补位，不阻塞已成功的选择。
            tauri::async_runtime::spawn(async move {
                if let Err(e) = auth.serve().await {
                    log::warn!(
                        "dsh auth listener bring-up after select failed: {}",
                        e.detail
                    );
                }
            });
        },
    );
    if let Err(e) = bring_up {
        log::warn!(
            "dsh auth listener bring-up after select skipped instance {instance_id}: {} ({})",
            e.code,
            e.detail
        );
    }
}

/// P1-2（Round-8 Task 2）选择后授权监听补位内核（生产钩子与测试夹具共用判定；
/// 装配面注入——生产单测进程绝不触 `production_env` 单例）：持久化开关记录
/// **enabled（任意相位，含 Ready 终态）** → 对该实例授权服务发起 serve（bind +
/// accept 循环 + 活体跟踪 + rendezvous 记录发布；serve 幂等，已在场 = 补位后
/// 立即返回）。记录缺席 / 关闭（enabled=false：Off、等待卸载、关闭侧失败）→
/// 不补起——关闭面就是不为已关闭实例起监听、不发布记录（握手/执行门仍逐连接
/// 兜底），且**不装配** auth 域（`assemble_auth` 不被调）。auth 域装配失败原样
/// 上抛：fail-soft（记日志、不阻塞选择）由生产钩子落。返回是否发起了补位。
pub(crate) fn ensure_listener_for_enabled_switch(
    record: Option<&super::switch::SwitchRecord>,
    assemble_auth: impl FnOnce() -> Result<Arc<DshAuthService>, DshError>,
    spawn_serve: impl FnOnce(Arc<DshAuthService>),
) -> Result<bool, DshError> {
    if !record.is_some_and(|record| record.enabled) {
        return Ok(false);
    }
    let auth = assemble_auth()?;
    spawn_serve(auth);
    Ok(true)
}

/// 第九轮 P2：选择后监听补位内核的**升级恢复感知**姊妹内核（生产钩子与测试
/// 夹具共用判定；记录读取结果整体注入——缺席与损坏必须可区分）：
///
/// * **持久记录在场**：沿用 [`ensure_listener_for_enabled_switch`（enabled → 补起；
///   Off / 等待卸载 / 关闭侧失败 → 不补起、不装配）。证据闭包不被消费。
/// * **记录缺席**：不再一刀切跳过——复用现有迁移证据（升级恢复三证：Bound 行 +
///   安装在场 + 登记锚与账本一致，见
///   `RemoteSwitchService::upgrade_recovery_evidence`）判定「开关功能上线前已
///   绑定+安装」的合法升级用户 → 视为已开启补起监听；证据不齐 → 不补起、
///   **不装配** auth 域（装配会 load_or_create_key 落钥，无谓副作用）。
/// * **记录损坏（读取失败 ≠ 缺席）**：不补起、不装配、**不落升级恢复**——坏账本
///   绝不得被升级恢复面洗成「已开启」；fail-soft（记日志、不阻塞选择）由生产
///   钩子落。
///
/// auth 域装配失败原样上抛（fail-soft 由调用方落）。返回是否发起了补位。
pub fn ensure_listener_for_selected_switch(
    record_read: Result<Option<super::switch::SwitchRecord>, DshError>,
    upgrade_evidence: impl FnOnce() -> bool,
    assemble_auth: impl FnOnce() -> Result<Arc<DshAuthService>, DshError>,
    spawn_serve: impl FnOnce(Arc<DshAuthService>),
) -> Result<bool, DshError> {
    match record_read {
        Ok(Some(record)) => {
            ensure_listener_for_enabled_switch(Some(&record), assemble_auth, spawn_serve)
        }
        Ok(None) => {
            if !upgrade_evidence() {
                return Ok(false);
            }
            let auth = assemble_auth()?;
            spawn_serve(auth);
            Ok(true)
        }
        // 记录损坏：缺席语义不适用（读取失败不是「从未开启」）。
        Err(_) => Ok(false),
    }
}

/// 选择解析结果（审查 C3：「真无选择」与「读取失败」不得混同）。
enum SelectedInstanceResolution {
    NoSelection,
    Selected(String),
}

/// 解析当前选择：登记面打不开 / 已选记录读不出 → **原样上抛**；无选择 →
/// `NoSelection`。绝不把失败折叠成「无选择」。
fn resolve_selected_instance() -> Result<SelectedInstanceResolution, DshError> {
    let registry = production_registry()?;
    match registry.selected()? {
        Some(instance) => Ok(SelectedInstanceResolution::Selected(instance.instance_id)),
        None => Ok(SelectedInstanceResolution::NoSelection),
    }
}

// ============================================================
// P1-2 预提交门（传输层提交边界复核）
// ============================================================

/// 选择复核核心（纯判据；`switch_tests` 用夹具 registry 直测）：
/// * 目标实例**仍是当前选择** → 放行；
/// * 当前选择是**另一实例** → `DSH_INSTANCE_SWITCHED`（用户已切走，排队中的
///   旧实例动作不得再写向旧实例）；
/// * 实例登记在场但**无选择** → `DSH_TARGET_AMBIGUOUS`（与发现面同一判据词：
///   需显式选择后才能写）；
/// * **真无实例登记**（旧桌面单例域）→ 放行选择复核（开关门仍生效）；
/// * 登记面读取失败 → 原样上抛（fail-closed）。
pub(crate) fn pre_submit_selection_check(
    registry: &super::instances::InstanceRegistry,
    host_id: &str,
) -> Result<(), DshError> {
    match registry.selected()? {
        Some(instance) if instance.instance_id == host_id => Ok(()),
        Some(_) => Err(DshError::new(
            codes::INSTANCE_SWITCHED,
            "instance selection moved away from action target; refusing pre-submit write",
        )),
        None => {
            if !registry.registered_ids()?.is_empty() {
                Err(DshError::new(
                    codes::TARGET_AMBIGUOUS,
                    "instances registered but none selected; explicit selection required before writes",
                ))
            } else {
                // 真无登记：旧单例兼容域，选择复核空态。
                Ok(())
            }
        }
    }
}

// ============================================================
// P1（销项复审）：提交边界（实例选择 / 关闭 / 发送准入共享）
// ============================================================
// 两态状态机（第二轮复审修正：纯「观察计数归零」无排斥力——flip 见零后新
// permit 仍可入、旧状态写出）。协议：
// * 发送侧 `commit_permit_enter()`（async）：见 FLIPPING → yield 等待；否则
//   IN_FLIGHT++ 后**复查 STATE**（变 FLIPPING → 回退计数继续等）→ 返回
//   permit（持到字节出门，Drop 释放）。
// * flip 侧 `commit_flip_begin()`（同步）：CAS IDLE→FLIPPING（并发 flip 串
//   行化）→ park 排空存量 IN_FLIGHT → Guard（Drop=IDLE，调用方持到状态落
//   盘结束）。
// 不变式：FLIPPING 期间**零新 permit** 进入关键区；FLIPPING 只排空存量——
// 排空上界 = 存量许可各自完成（门文件 IO + unary 网络超时），**无饿死**
// （FLIPPING 先阻断新进入者，等的不是不断补充的流）。旧状态权威性：flip
// 落地前写出的消息在旧状态权威期内已提交（切换随后生效）；flip 落地后的
// 新发送在提交点门必然读到新状态并拒绝（上游 0 次 POST）。
const COMMIT_STATE_IDLE: usize = 0;
const COMMIT_STATE_FLIPPING: usize = 1;

static COMMIT_BOUNDARY_STATE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(COMMIT_STATE_IDLE);
static COMMIT_IN_FLIGHT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

// ============================================================
// P1（销项复审）：关闭即效旗标（关闭首步 = 同一准入边界的立即拒绝面）
// ============================================================
// disable() 的**第一步**即 mark_closing：许可区门#3、授权提交门、控制台入口
// 与附件发布点全部查此旗标——新发送/上传/授权/控制台在关闭落盘与撤权段之前
// 就被拒绝（不等 10s 响应、不等慢包）。落盘失败时旗标保留（重试清理）；
// enable 提交 enabled 后按**观察到的纪元**条件清除（P1-2）。
//
// 销项复审 P1-2（关闭意图带代次）：每次 mark_closing 先推进全局纪元计数器，
// 实例旗标记录**本次纪元**（`CLOSING_INSTANCES: id → epoch`）。enable 在入口
// 观察当前纪元、提交 enabled 后只按观察值条件清除（`clear_closing_if`）——
// 「enable 观察之后、提交之前」到达的新关闭（更高纪元）不被旧 enable 拆除，
// 关闭拒绝面不再被并发开启拆掉。
//
// 销项复审 P1-3（附件上传原子代次边界）：三层上传检查全是无状态布尔 is_closing
// ——最后检查通过后 create_new 前关闭仍可落地（文件照落盘）；关闭+重开（旗标
// 清除）后排队旧上传按**新开关状态**继续写。修法：注册表加**实例作用域代次**
// `generations: id → u64`——mark_closing 与 enable/重开（clear_closing_if 成功
// 后）各推进 +1；上传入口受理时捕获 `instance_generation`，附件发布点比对
// 「受理代次 == 当前代次」，失配 → 拒绝（PublishRefusal::StaleUploadGeneration）。
// **原子边界 = 代次读取/推进与旗标写同锁**（单 Mutex 注册表）：mark_closing 在
// 一次加锁内同时推进代次并落旗标，关闭落地后的发布点读取必见新代次——直接
// 关闭形态下（旗标面）与关闭+重开形态下（代次面）旧上传都进不来。
static CLOSING_EPOCH: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static CLOSING_INSTANCES: OnceLock<std::sync::Mutex<InstanceClosingRegistry>> = OnceLock::new();

/// 实例关闭注册表（单锁域）：关闭纪元旗标 + 实例代次同锁——旗标面与代次面的
/// 读取/推进互为临界区（P1-3 原子边界的实现载体）。
#[derive(Default)]
struct InstanceClosingRegistry {
    /// id → 当前关闭纪元（仅关闭期存在；`clear_closing_if` 按纪元条件移除）。
    closing: std::collections::HashMap<String, usize>,
    /// id → 实例代次（持久单调累计；mark_closing 与 enable/重开各推进 +1）。
    generations: std::collections::HashMap<String, u64>,
    selection_generations: std::collections::HashMap<String, u64>,
    selection_revision: u64,
}

fn closing_registry() -> &'static std::sync::Mutex<InstanceClosingRegistry> {
    CLOSING_INSTANCES.get_or_init(|| std::sync::Mutex::new(InstanceClosingRegistry::default()))
}

/// 首次受理时固定的进程、开关和选择证据。进程退出后无法证明旧的内存关闭意图，
/// 因此新进程不会沿用上一进程的待发动作。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[doc(hidden)]
pub struct SendAcceptance {
    boot: u128,
    generation: u64,
    selection: u64,
    pub(crate) authority: Option<String>,
}

impl SendAcceptance {
    pub(crate) fn belongs_to_current_process(&self) -> bool {
        self.boot == *SEND_BOOT
    }
}

static SEND_BOOT: std::sync::LazyLock<u128> = std::sync::LazyLock::new(rand::random);
static SEND_INVALIDATED: std::sync::LazyLock<tokio::sync::watch::Sender<u64>> =
    std::sync::LazyLock::new(|| tokio::sync::watch::channel(0).0);

pub(crate) fn send_revision() -> u64 {
    *SEND_INVALIDATED.borrow()
}

pub(crate) struct SendHostStamp {
    pub(crate) instance: String,
    pub(crate) epoch: u64,
    pub(crate) authority: String,
}

fn cached_send_host(instance: Option<&str>) -> Option<SendHostStamp> {
    let selected = selected_mirror();
    let requested = instance.or(selected.as_deref());
    let mut host = None;
    if let Some(cache) = INSTANCE_AUTHS.get() {
        let cache = cache.try_lock().ok()?;
        for (id, service) in cache.iter() {
            if requested.is_some_and(|instance| id != instance) {
                continue;
            }
            if let Some(stamp) = service.send_host_stamp() {
                if host.replace(stamp).is_some() {
                    return None;
                }
            }
        }
    }
    if host.is_some() || selected.is_some() {
        return host;
    }
    LEGACY_AUTH
        .get()?
        .as_ref()
        .ok()?
        .send_host_stamp()
        .filter(|stamp| requested.is_none_or(|instance| stamp.instance == instance))
}

pub(crate) struct SendAdmission {
    revision: u64,
    selection_revision: u64,
    explicit: Option<(String, SendAcceptance)>,
    host: Option<Result<SendHostStamp, DshError>>,
}

impl SendAdmission {
    pub(crate) fn capture(instance: Option<&str>) -> Self {
        let registry = closing_registry().lock().unwrap_or_else(|p| p.into_inner());
        Self {
            revision: send_revision(),
            selection_revision: registry.selection_revision,
            explicit: instance.map(|id| (id.to_owned(), registry.send_acceptance(id, None))),
            host: None,
        }
    }

    pub(crate) fn capture_http(instance: Option<&str>, require_peer: bool) -> Self {
        let mut admission = Self::capture(instance);
        if require_peer {
            admission.host = Some(cached_send_host(instance).ok_or_else(|| {
                DshError::new(
                    codes::SEND_RECONNECTING,
                    "no verified host stamp at HTTP admission",
                )
            }));
        }
        admission
    }

    pub(crate) fn bind(self, host: &LocalHost) -> Result<SendAcceptance, DshError> {
        let instance = host.host_id.as_str();
        if let Some(expected) = self.host {
            let expected = expected?;
            if expected.instance != instance
                || expected.epoch != host.epoch
                || expected.authority != host.authority
            {
                return Err(DshError::new(
                    codes::HOST_NOT_RUNNING,
                    "host changed during HTTP admission",
                ));
            }
        }
        let registry = closing_registry().lock().unwrap_or_else(|p| p.into_inner());
        if registry.selection_revision != self.selection_revision
            || selected_mirror().is_some_and(|selected| selected != instance)
        {
            return Err(DshError::new(
                codes::INSTANCE_SWITCHED,
                "send selection changed during admission",
            ));
        }
        let mut acceptance = match self.explicit {
            Some((id, acceptance)) if id == instance => acceptance,
            None if send_revision() == self.revision => registry.send_acceptance(instance, None),
            _ => {
                return Err(DshError::new(
                    codes::INSTANCE_SWITCHED,
                    "send target changed during admission",
                ));
            }
        };
        acceptance.authority = Some(host.authority.clone());
        registry.check_send_acceptance(instance, &acceptance)?;
        Ok(acceptance)
    }
}

pub(crate) fn capture_send_acceptance(instance: &str, authority: Option<String>) -> SendAcceptance {
    closing_registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .send_acceptance(instance, authority)
}

impl InstanceClosingRegistry {
    fn send_acceptance(&self, instance: &str, authority: Option<String>) -> SendAcceptance {
        SendAcceptance {
            boot: *SEND_BOOT,
            generation: self.generations.get(instance).copied().unwrap_or(0),
            selection: self
                .selection_generations
                .get(instance)
                .copied()
                .unwrap_or(0),
            authority,
        }
    }

    fn check_send_acceptance(
        &self,
        instance: &str,
        acceptance: &SendAcceptance,
    ) -> Result<(), DshError> {
        if self
            .selection_generations
            .get(instance)
            .copied()
            .unwrap_or(0)
            != acceptance.selection
            || selected_mirror().is_some_and(|selected| selected != instance)
        {
            return Err(DshError::new(
                codes::INSTANCE_SWITCHED,
                "send selection generation changed",
            ));
        }
        if !acceptance.belongs_to_current_process()
            || self.closing.contains_key(instance)
            || self.generations.get(instance).copied().unwrap_or(0) != acceptance.generation
        {
            return Err(DshError::new(
                codes::SWITCH_OFF,
                "send instance generation is closed or stale",
            ));
        }
        Ok(())
    }
}

pub(crate) fn with_send_acceptance<T>(
    instance: &str,
    acceptance: &SendAcceptance,
    commit: impl FnOnce() -> T,
) -> Result<T, DshError> {
    let registry = closing_registry().lock().unwrap_or_else(|p| p.into_inner());
    registry.check_send_acceptance(instance, acceptance)?;
    Ok(commit())
}

pub(crate) async fn send_invalidated(instance: &str, acceptance: &SendAcceptance) -> DshError {
    let mut changes = SEND_INVALIDATED.subscribe();
    loop {
        if let Err(error) = with_send_acceptance(instance, acceptance, || ()) {
            return error;
        }
        // The global sender lives for the process. Subscribe before the first snapshot.
        let _ = changes.changed().await;
    }
}

pub(crate) fn with_open_instance_generation<T>(
    instance: &str,
    generation: u64,
    commit: impl FnOnce() -> T,
) -> Result<T, DshError> {
    let registry = closing_registry().lock().unwrap_or_else(|p| p.into_inner());
    if registry.closing.contains_key(instance)
        || registry.generations.get(instance).copied().unwrap_or(0) != generation
    {
        return Err(DshError::new(
            codes::SWITCH_OFF,
            "credential instance generation is closed or stale",
        ));
    }
    Ok(commit())
}

pub(crate) fn record_instance_selection(previous: Option<&str>, current: &str) {
    let mut registry = closing_registry().lock().unwrap_or_else(|p| p.into_inner());
    if previous != Some(current) {
        registry.selection_revision = registry.selection_revision.wrapping_add(1);
    }
    if let Some(previous) = previous.filter(|id| *id != current) {
        *registry.generations.entry(previous.to_owned()).or_default() += 1;
        *registry
            .selection_generations
            .entry(previous.to_owned())
            .or_default() += 1;
    }
    set_selected_mirror(Some(current));
    SEND_INVALIDATED.send_modify(|revision| *revision = revision.wrapping_add(1));
}

/// 标记实例进入「正在关闭」（关闭首步）；返回本次关闭的**纪元**
/// （单调递增；`clear_closing_if` 按它做条件清除）。
pub fn mark_closing(instance_id: &str) -> usize {
    let epoch = CLOSING_EPOCH.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
    let mut registry = closing_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // P1-3：代次推进与旗标落地在**同一次加锁**内（原子边界）——发布点读取
    // 与本推进互斥，「关闭落地后的发布点比对必见新代次」。
    *registry
        .generations
        .entry(instance_id.to_string())
        .or_insert(0) += 1;
    registry.closing.insert(instance_id.to_string(), epoch);
    SEND_INVALIDATED.send_modify(|revision| *revision = revision.wrapping_add(1));
    epoch
}

/// 实例当前关闭纪元（`None` = 未标记「正在关闭」；enable 入口观察面）。
pub fn closing_epoch(instance_id: &str) -> Option<usize> {
    closing_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .closing
        .get(instance_id)
        .copied()
}

/// 实例是否处于「正在关闭」（许可区内存判据/授权门/控制台入口/附件发布点消费）。
pub fn is_closing(instance_id: &str) -> bool {
    closing_epoch(instance_id).is_some()
}

/// 条件清除关闭旗标：仅当该实例**当前纪元 == epoch** 时移除并返回 true——
/// 期间到达的新关闭（更高纪元）保留拒绝面（销项复审 P1-2）。
pub fn clear_closing_if(instance_id: &str, epoch: usize) -> bool {
    let mut registry = closing_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if registry.closing.get(instance_id) == Some(&epoch) {
        registry.closing.remove(instance_id);
        SEND_INVALIDATED.send_modify(|revision| *revision = revision.wrapping_add(1));
        true
    } else {
        false
    }
}

/// 实例当前代次（P1-3 附件上传原子边界的读取面）：上传入口受理时捕获，
/// 附件发布点比对「受理代次 == 当前代次」。与推进（mark_closing /
/// `advance_instance_generation`）同锁域——关闭落地后的读取必见新代次。
pub fn instance_generation(instance_id: &str) -> u64 {
    closing_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .generations
        .get(instance_id)
        .copied()
        .unwrap_or(0)
}

/// 受理快照（终版复核 #2）：旗标与代次在**同一次加锁**内读取——受理面一次
/// 观测同时拿到「是否正在关闭 + 当前代次」。拆成 `is_closing` +
/// `instance_generation` 两次加锁存在撕裂窗：其间「关闭+重开」整段落地时，
/// 旗标读旧（false）而代次读新，旧请求被误标成重开后代次，发布点比对放行；
/// 同锁快照后，受理时的门判据与代次标记来自同一观测时刻，之后的任何关闭
/// /重开推进都由发布点代次比对拒收。返回 `(是否正在关闭, 当前代次)`。
pub fn closing_snapshot(instance_id: &str) -> (bool, u64) {
    let registry = closing_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    (
        registry.closing.contains_key(instance_id),
        registry.generations.get(instance_id).copied().unwrap_or(0),
    )
}

/// 发布临界区拒绝面（目标 1，暂存方案）：与附件 `PublishRefusal` 同族语义，
/// 由 production 层独立定义（services 不反向依赖 remote）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishDecisionError {
    /// 实例「正在关闭」（关闭首步旗标在临界区内命中）。
    InstanceClosing,
    /// 受理代次 ≠ 临界区内当前代次（关闭/重开/切走都推进过代次）。
    StaleUploadGeneration,
    /// 进程内选择镜像指向别的实例（翻转后、镜像同步窗口内的旧上传）。
    InstanceSwitched,
}

/// HTTP 入口快照。无已知实例或发布锁正忙时，用全局修订证明未补领新代次。
pub(crate) struct UploadAdmission {
    requested: Option<String>,
    captured: Option<UploadSnapshot>,
    revision: u64,
}

struct UploadSnapshot {
    target: Option<(String, bool, u64)>,
    selection_revision: u64,
    closed_without_target: bool,
}

impl UploadAdmission {
    pub(crate) fn capture(instance: Option<&str>) -> Self {
        // 不在异步 worker 等待持锁的文件系统操作。若未取得细粒度快照，只接受
        // 从入口读到修订起完全未发生任何生命周期变动的请求。
        let revision = send_revision();
        let registry = match closing_registry().try_lock() {
            Ok(registry) => Some(registry),
            Err(std::sync::TryLockError::Poisoned(error)) => Some(error.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => None,
        };
        let captured = registry
            .as_ref()
            .filter(|_| send_revision() == revision)
            .map(|registry| {
                let target = instance.map(str::to_owned).or_else(selected_mirror);
                UploadSnapshot {
                    closed_without_target: target.is_none() && !registry.closing.is_empty(),
                    target: target.map(|id| {
                        let closing = registry.closing.contains_key(&id);
                        let generation = registry.generations.get(&id).copied().unwrap_or(0);
                        (id, closing, generation)
                    }),
                    selection_revision: registry.selection_revision,
                }
            });
        Self {
            requested: instance.map(str::to_owned),
            captured,
            revision,
        }
    }

    pub(crate) fn bind(self, instance: &str) -> Result<(u64, u64), PublishDecisionError> {
        let registry = closing_registry().lock().unwrap_or_else(|p| p.into_inner());
        if self.requested.as_deref().is_some_and(|id| id != instance)
            || self
                .captured
                .as_ref()
                .is_some_and(|snapshot| registry.selection_revision != snapshot.selection_revision)
            || selected_mirror().is_some_and(|selected| selected != instance)
        {
            return Err(PublishDecisionError::InstanceSwitched);
        }
        if registry.closing.contains_key(instance)
            || self
                .captured
                .as_ref()
                .is_some_and(|snapshot| snapshot.closed_without_target)
        {
            return Err(PublishDecisionError::InstanceClosing);
        }
        let current = registry.generations.get(instance).copied().unwrap_or(0);
        let accepted = (current, registry.selection_revision);
        match self.captured.and_then(|snapshot| snapshot.target) {
            Some((id, _, _)) if id != instance => Err(PublishDecisionError::InstanceSwitched),
            Some((_, true, _)) => Err(PublishDecisionError::InstanceClosing),
            Some((_, false, generation)) if generation == current => Ok(accepted),
            None if send_revision() == self.revision => Ok(accepted),
            _ => Err(PublishDecisionError::StaleUploadGeneration),
        }
    }
}

/// 发布临界区（目标 1）：**代次复核 + 选择镜像复核 + 发布动作**在与关闭
/// 注册表同一次加锁内完成——`mark_closing`（关闭首步）/ `clear_closing_and_advance`
/// （重开后半程）/ 切换代次推进与发布互斥，「复核通过后、落盘前」的推进窗
/// 不复存在（复核通过即已发布）。`publish` 闭包只做内存就绪的同卷原子
/// 发布（hard_link 不覆盖），毫秒级；锁内零盘读、零网络——开关执行门的盘
/// 读留在预检（临界区外），本实例的 disable/enable 分别经由关闭旗标/代次
/// 推进被临界区捕获，不依赖盘读。
///
/// 镜像复核：`SELECTED_MIRROR` 为 Some 且 ≠ 本实例 → 拒（翻转后旧上传不得
/// 落进已切走的实例）；None = 本进程尚无翻转 → 放行（与发送许可区同判据）。
pub fn publish_under_closing_lock<R>(
    instance_id: &str,
    accepted_generation: u64,
    publish: impl FnOnce() -> R,
) -> Result<R, PublishDecisionError> {
    publish_under_upload_admission(instance_id, accepted_generation, None, publish)
}

pub(crate) fn publish_under_upload_admission<R>(
    instance_id: &str,
    accepted_generation: u64,
    selection_revision: Option<u64>,
    publish: impl FnOnce() -> R,
) -> Result<R, PublishDecisionError> {
    let registry = closing_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if registry.closing.contains_key(instance_id) {
        return Err(PublishDecisionError::InstanceClosing);
    }
    if selection_revision.is_some_and(|revision| revision != registry.selection_revision)
        || selected_mirror().is_some_and(|selected| selected != instance_id)
    {
        return Err(PublishDecisionError::InstanceSwitched);
    }
    let current = registry.generations.get(instance_id).copied().unwrap_or(0);
    if current != accepted_generation {
        return Err(PublishDecisionError::StaleUploadGeneration);
    }
    Ok(publish())
}

/// 推进实例代次 +1（P1-3：关闭+重开完整周期的后半程推进；生产 enable 路径
/// 已改用**同锁复合**的 [`clear_closing_and_advance`]——本函数保留为单锁域
/// 原语，供非 enable 的代次推进面/测试消费）。与 mark_closing 同锁域，单调
/// 递增。
pub fn advance_instance_generation(instance_id: &str) -> u64 {
    let mut registry = closing_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let gen = registry
        .generations
        .entry(instance_id.to_string())
        .or_insert(0);
    *gen += 1;
    SEND_INVALIDATED.send_modify(|revision| *revision = revision.wrapping_add(1));
    *gen
}

/// 冷启动只有持久关闭记录时，开启提交不能与附件入口/发布代次脱节。
pub(crate) fn commit_open_generation<T>(
    instance_id: &str,
    commit: impl FnOnce() -> Result<T, DshError>,
) -> Result<T, DshError> {
    let mut registry = closing_registry().lock().unwrap_or_else(|p| p.into_inner());
    let value = commit()?;
    *registry
        .generations
        .entry(instance_id.to_owned())
        .or_default() += 1;
    SEND_INVALIDATED.send_modify(|revision| *revision = revision.wrapping_add(1));
    Ok(value)
}

/// 复合清除+推进（销项审查发现 2，P2）：enable 提交 enabled 后的「按观察纪元
/// 清旗」与「推进实例代次」在**同一次加锁**内完成。旧实现两步分开锁——窗口
/// 里上传可经入口准入并捕获「已清旗、未推进」的代次，enable 推进后在附件发布
/// 点被误拒 `StaleUploadGeneration`；原子化后该窗口不复存在。纪元失配（期间
/// 到达的新关闭）返回 false 且不动任何状态（P1-2 条件清除语义不变：旧观察值
/// 不得拆比自己新的关闭）。
pub fn clear_closing_and_advance(instance_id: &str, observed_epoch: usize) -> bool {
    let mut registry = closing_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if registry.closing.get(instance_id) != Some(&observed_epoch) {
        return false;
    }
    registry.closing.remove(instance_id);
    // P1-3：清旗成功 = 本次 enable 完成「关闭+重开」完整周期的后半程——实例
    // 代次恰好再推进 +1（关闭侧 mark_closing 已推进过一次）。与拆旗同锁，无
    // 「已清旗、未推进」中间态外泄。
    let gen = registry
        .generations
        .entry(instance_id.to_string())
        .or_insert(0);
    *gen += 1;
    SEND_INVALIDATED.send_modify(|revision| *revision = revision.wrapping_add(1));
    true
}

// ============================================================
// P1-5（销项复审）：进程内选择镜像（许可区内存判据）
// ============================================================
// 提交许可区内的复核不得做文件 IO（flip 要排空存量许可——许可持有者的
// 盘读会拖长关闭/切换等待）。选择翻转（`instance_select_core`）在提交边界
// 独占（`commit_flip_begin`）内 `select_exclusive` 落盘后**同步更新本镜像**，
// 许可区内只读内存：Some(id) 且 != 动作 target → 拒（INSTANCE_SWITCHED 语义）；
// None = 本进程尚无翻转 → 空态放行（首翻前的旧盘面由门#2 盘读覆盖）。
//
// 竞态闭证：flip（FLIPPING）先阻断新许可 → 排空存量 → 翻转时同一独占段内
// 同步更新镜像与文件 → IDLE——许可区读到的镜像与盘面不会倒挂。
static SELECTED_MIRROR: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);

/// 更新选择镜像（选择翻转守卫内存活期内、文件落盘后同步调用；`None` 仅留
/// 给「选择被显式清空」的未来路径——当前翻转面恒 Some）。
pub fn set_selected_mirror(instance_id: Option<&str>) {
    *SELECTED_MIRROR
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = instance_id.map(str::to_string);
}

/// 读选择镜像（提交许可区内存判据；短临界区，无 await）。
pub fn selected_mirror() -> Option<String> {
    SELECTED_MIRROR
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// cfg(test) 暂停开关/恢复闸：`commit_permit_enter` 在「计数后、复查前」
/// 暂停，供时序测试确定性构造「计数后才进 flip → 复查回退」的分支。
#[cfg(test)]
static TEST_COMMIT_PERMIT_PAUSE_ARMED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
#[cfg(test)]
static TEST_COMMIT_PERMIT_RESUME: std::sync::LazyLock<tokio::sync::Notify> =
    std::sync::LazyLock::new(tokio::sync::Notify::new);

/// 发送关键区许可：`commit_permit_enter().await` 领取，Drop 释放（纯原子
/// 量——Send，跨 await 无碍）。
pub struct CommitPermit;

impl Drop for CommitPermit {
    fn drop(&mut self) {
        COMMIT_IN_FLIGHT.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// 取消安全计数守卫（四验 Important）：计数与 Permit 构造之间若存在 await
/// （cfg(test) 暂停点），任务在该点被取消时 Drop 回退计数——绝不泄漏。
/// 成功路径 `std::mem::forget` 转正为 [`CommitPermit`]（同一释放职责移交，
/// 不双减）。生产构建无该 await，守卫零成本存在。
struct PermitGuard;

impl Drop for PermitGuard {
    fn drop(&mut self) {
        COMMIT_IN_FLIGHT.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// 领取提交许可（发送关键区入口；必须在提交点门**之前**调用）。FLIPPING
/// 期间等待（yield + 短睡，避免热旋）；「先计数后复查状态」关死
/// 「计数后才进 flip」的插窗。
pub async fn commit_permit_enter() -> CommitPermit {
    loop {
        if COMMIT_BOUNDARY_STATE.load(std::sync::atomic::Ordering::SeqCst) == COMMIT_STATE_FLIPPING
        {
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            continue;
        }
        COMMIT_IN_FLIGHT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let guard = PermitGuard;
        // cfg(test) 暂停点：停在「计数后、状态复查前」，让测试确定性构造
        // 「计数后才进 flip → 复查回退」的关键分支（测试外编译消除）。
        // 取消安全：此处被取消 → guard Drop 回退计数，不泄漏。
        #[cfg(test)]
        if TEST_COMMIT_PERMIT_PAUSE_ARMED.load(std::sync::atomic::Ordering::SeqCst) {
            TEST_COMMIT_PERMIT_RESUME.notified().await;
        }
        if COMMIT_BOUNDARY_STATE.load(std::sync::atomic::Ordering::SeqCst) == COMMIT_STATE_FLIPPING
        {
            // 领取后才进 flip：守卫回退计数，回循环等待 flip 完成（完成后的
            // 提交点门会读到新状态并拒绝——本许可无需真正进入关键区）。
            drop(guard);
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            continue;
        }
        // 成功：守卫转正为许可（释放职责移交 CommitPermit::drop）。
        std::mem::forget(guard);
        return CommitPermit;
    }
}

// 需要即时提交时尝试拿许可，正在换代即拒绝。
pub(crate) fn try_commit_permit() -> Option<CommitPermit> {
    if COMMIT_BOUNDARY_STATE.load(std::sync::atomic::Ordering::SeqCst) == COMMIT_STATE_FLIPPING {
        return None;
    }
    COMMIT_IN_FLIGHT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let permit = CommitPermit;
    (COMMIT_BOUNDARY_STATE.load(std::sync::atomic::Ordering::SeqCst) != COMMIT_STATE_FLIPPING)
        .then_some(permit)
}

/// flip 面入口（同步；生产 flip 全在阻塞池/同步线程，park 合法；同步测试
/// 进程内 IN_FLIGHT 恒 0，立即通过）。CAS 串行化并发 flip；Drop 复位 IDLE。
pub struct CommitFlipGuard;

impl Drop for CommitFlipGuard {
    fn drop(&mut self) {
        COMMIT_BOUNDARY_STATE.store(COMMIT_STATE_IDLE, std::sync::atomic::Ordering::SeqCst);
    }
}

/// 开始一次提交边界 flip：阻断新许可 → 排空存量在途发送 → 持 Guard 到状态
/// 落盘结束（调用方把状态变更写在 Guard 存活期内）。
pub fn commit_flip_begin() -> CommitFlipGuard {
    while COMMIT_BOUNDARY_STATE
        .compare_exchange(
            COMMIT_STATE_IDLE,
            COMMIT_STATE_FLIPPING,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .is_err()
    {
        // 并发 flip：等前一个 flip 完成复位后再抢。
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    while COMMIT_IN_FLIGHT.load(std::sync::atomic::Ordering::SeqCst) != 0 {
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    CommitFlipGuard
}

/// 预提交门生产闭包：① 当前选择复核（[`pre_submit_selection_check`]）；
/// ② 开关执行门——与 hub 最终派发复核**同源**（`switch::enforcement_gate`）。
/// 阻塞面（盘读+文件锁）在闭包内落 blocking 池；join 失败 fail-closed。
pub fn pre_submit_gate() -> super::actions::PreSubmitGate {
    Arc::new(move |target: super::model::Target| {
        Box::pin(async move {
            let outcome = tokio::task::spawn_blocking(move || {
                let registry = production_registry()?;
                pre_submit_selection_check(&registry, &target.host_id)?;
                super::switch::enforcement_gate(&target.host_id)?;
                // 关闭即效旗标（销项复审 P1）：关闭首步即拒——不等撤权/落盘段。
                if is_closing(&target.host_id) {
                    return Err(DshError::new(
                        codes::SWITCH_OFF,
                        "instance is closing; new native writes refused at the admission gate",
                    ));
                }
                Ok(())
            })
            .await
            .map_err(|e| {
                DshError::new(codes::STATE_IO, format!("pre-submit gate task failed: {e}"))
            })?;
            outcome
        })
    })
}

/// 旧桌面单例（H05 前的装配逐字保留：**仅真无选择**时的兼容面）。
fn legacy_production_manager() -> Arc<DshPluginManager> {
    static MANAGER: OnceLock<Arc<DshPluginManager>> = OnceLock::new();
    MANAGER
        .get_or_init(|| {
            let env = production_env();
            let mut manager = DshPluginManager::new(
                clone_env(&env),
                Arc::new(super::auth::ProductionBindingRevoker),
            );
            if let Ok(auth) = production_auth_state() {
                manager = manager
                    .with_authorization_reader(Arc::new(super::auth::AuthServiceReader(auth)));
            }
            Arc::new(manager)
        })
        .clone()
}

// ============================================================
// 授权域初始化（R09：失败隔离——可选能力坏了不能拖垮整个应用启动）
// ============================================================

/// 授权域初始化失败的原因（**启动期标记**，不是每次命令都重试的错误串）。
///
/// R09 的设计取向（policy 一律写死，不留给调用点各自发挥）：
///
/// * **一次尝试 + 锁定失败**：域初始化在**首次取用**时做一次，结果（成功/失败）
///   都进 `OnceLock` 型的 `Result`；失败后**不再自动重试**（避免每次命令都去撞
///   同一份坏文件、刷屏日志）。这与"lazy re-attempt"相比少一个会自己变好的
///   语义，但换来确定性：状态面在同一个进程里不会一会儿可用一会儿不可用。
///   用户的修复出口是**修好状态目录后重启应用**（或点设置卡的修复动作），
///   与其它启动期配置（`TUVIS_DSH_STATE_DIR`）同一节奏。
/// * **失败 = 降级标记**：`dsh_remote_status()` 照常返回状态 DTO，只是
///   `installation = Broken` + `reasonCode = <具体 DSH 码>`，界面因此显示「不可用 +
///   修复出口」而不是整块消失；`dsh_binding_*` 等需要 auth 的命令则如实返回该码。
/// * **绝不 panic**：`DshAuthService::new` 的 `Result` 原样上抛到标记层。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthInitFailure {
    /// 具体原因（沿用既有 DSH 码：`DSH_IPC_STATE` / `DSH_STATE_PATH_TOO_LONG` /
    /// `DSH_STATE_IO` 等——不发明平行通道）。
    pub reason_code: &'static str,
    /// 英文机器诊断（不含凭据）。
    pub detail: String,
}

impl AuthInitFailure {
    fn from_error(e: DshError) -> Self {
        Self {
            reason_code: e.code,
            detail: e.detail,
        }
    }

    /// 还原成可上抛的 [`DshError`]（命令层照常用 `CODE: detail` 形态）。
    pub fn as_error(&self) -> DshError {
        DshError::new(self.reason_code, self.detail.clone())
    }
}

impl std::fmt::Display for AuthInitFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&format!("{}: {}", self.reason_code, self.detail))
    }
}

/// 建授权服务（**唯一的构造点**；`Result` 不吞不掉，也绝不 `expect`）。
///
/// 失败面覆盖：授权密钥不可解析 / 状态目录不可写 / 路径过长（`serve` 阶段）等。
/// 生产单例经 [`production_auth_state`] 把它锁进 `Result`，测试直接调本函数。
pub fn init_auth_service(env: &DshEnvironment) -> Result<DshAuthService, DshError> {
    // 路径过长在**解析点**就先拦（比等到 serve 的 bind 更早、更靠近配置）。
    checked_state_dir(env)?;
    DshAuthService::new(clone_env(env))
}

/// 环境克隆（适配器 Arc 共享；auth 服务要独占 env 所有权）。
fn clone_env(env: &DshEnvironment) -> DshEnvironment {
    DshEnvironment {
        home: env.home.clone(),
        tuvis_home: env.tuvis_home.clone(),
        dsh_state_dir: env.dsh_state_dir.clone(),
        processes: env.processes.clone(),
        files: env.files.clone(),
        runner: env.runner.clone(),
        clock: env.clock.clone(),
        environments: env.environments.clone(),
    }
}

/// 生产授权服务单例的**结果**（`Ok` = 可用，`Err` = 启动期已判定的降级标记）。
///
/// **H05 委托语义（审查 C3 收紧）**：存在明确已选实例 → 该实例的按实例 auth 域
/// （实例隔离的 state 子目录 / 端点 / 授权意图）；真无选择 → 旧基础单例（桌面
/// 单实例行为逐字不变）；选择/登记面**读取失败** → 失败原样成为降级标记（绝不
/// 静默换 legacy 域——那是另一套账本与端点）。每次调用返回同一 `Arc` 或同一份
/// 失败记录——不重试、不抖动。
pub fn production_auth_state() -> Result<Arc<DshAuthService>, AuthInitFailure> {
    match resolve_selected_instance().map_err(AuthInitFailure::from_error)? {
        SelectedInstanceResolution::NoSelection => legacy_auth_state(),
        SelectedInstanceResolution::Selected(id) => {
            auth_for_instance(&id).map_err(AuthInitFailure::from_error)
        }
    }
}

/// 旧基础单例（H05 前的装配逐字保留：**仅真无选择**时的兼容面）。
fn legacy_auth_state() -> Result<Arc<DshAuthService>, AuthInitFailure> {
    LEGACY_AUTH
        .get_or_init(|| match init_auth_service(&production_env()) {
            Ok(svc) => Ok(Arc::new(svc)),
            Err(e) => {
                // 启动期只记一次日志（R09：坏的可选域必须可见但不刷屏）。
                log::warn!(
                    "DSH 控制域不可用（应用继续启动，其它工具不受影响）：{}: {}",
                    e.code,
                    e.detail
                );
                Err(AuthInitFailure::from_error(e))
            }
        })
        .clone()
}

/// 授权服务取用入口（**必须**能拿到服务；拿不到就把启动期判定的失败原样上抛）。
///
/// 失败隔离的界线：`lib.rs` 的 setup 只调 [`production_auth_state`]（容忍 Err），
/// 需要真凭据的命令才走本入口（据此如实报错，不假装有绑定）。
pub fn production_auth() -> Result<Arc<DshAuthService>, DshError> {
    production_auth_state().map_err(|failure| failure.as_error())
}

/// 绑定管理入口的授权域判定（**纯函数**，I1 终审：入口与通道不得各取不同域）。
/// 规则：已选定 → 该实例域；无选定但恰一个已登记 → 该实例域（唯一合理目标）；
/// 无选定且无已登记 → legacy 兼容域（未登记唯一活体的兼容路径，与
/// `auth_for_target` 的回退判定同口径）；多个已登记且无选定 → 歧义拒绝
/// （用户必须先选择——绝不猜）。
pub(crate) enum AuthDomain {
    Legacy,
    Instance(String),
}

pub(crate) fn auth_domain_for_binding(
    selected: Option<&str>,
    registered: &[String],
) -> Result<AuthDomain, DshError> {
    if let Some(id) = selected {
        return Ok(AuthDomain::Instance(id.to_string()));
    }
    match registered.len() {
        0 => Ok(AuthDomain::Legacy),
        1 => Ok(AuthDomain::Instance(registered[0].clone())),
        _ => Err(DshError::new(
            codes::HOST_NOT_RUNNING,
            "multiple registered dsh instances; select one before authorizing",
        )),
    }
}

/// 本机绑定管理命令的授权服务解析（入口与发送通道共用同一判定）。
pub fn binding_auth() -> Result<Arc<DshAuthService>, DshError> {
    match binding_domain()? {
        AuthDomain::Legacy => production_auth(),
        AuthDomain::Instance(id) => auth_for_instance(&id),
    }
}

#[cfg(test)]
mod binding_domain_tests {
    use super::*;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn binding_domain_matrix() {
        // 已选定 → 实例域（即使登记多项）
        assert!(matches!(
            auth_domain_for_binding(Some("a"), &ids(&["a", "b"])),
            Ok(AuthDomain::Instance(id)) if id == "a"
        ));
        // 无选定 + 无登记 → legacy 兼容域
        assert!(matches!(
            auth_domain_for_binding(None, &[]),
            Ok(AuthDomain::Legacy)
        ));
        // 无选定 + 恰一个登记 → 该实例域
        assert!(matches!(
            auth_domain_for_binding(None, &ids(&["a"])),
            Ok(AuthDomain::Instance(id)) if id == "a"
        ));
        // 无选定 + 多登记 → 歧义拒绝（不猜）
        assert!(auth_domain_for_binding(None, &ids(&["a", "b"])).is_err());
    }
}

/// DSH 自动适配生效值（B2 复审：能力门消费 **DSH 自己的键**
/// `remote.dsh_auto_adapt`——与 `commands::dsh::current_auto_adapt` 同键同缺省；
/// 独立成生产函数以便远程层在 spawn_blocking 内取用，不经命令层全局连接两次）。
pub fn dsh_auto_adapt_enabled() -> bool {
    crate::commands::dsh::auto_adapt_enabled_from(
        crate::database::dao::settings::get_setting(crate::commands::dsh::KEY_AUTO_ADAPT)
            .as_deref(),
    )
}

// ============================================================
// 数据根解析（R10 单一来源 + 两端同义覆盖变量）
// ============================================================
//
// 判据（Mac 补充探测报告 §6「可落地的两端一致规则」）：**数据根只有一个来源**
// = Rust 侧 `app_data_home()/.tuvis`；插件不猜 `TUVIS_HOME`，只接受 Rust 显式
// 提供的单一路径变量（两端同名同语义）。本模块是 Rust 侧唯一的解析点。
//
// 为什么 `TUVIS_DSH_STATE_DIR` 在 release 也生效（与 `TUVIS_HOME` 不同）：
// `TUVIS_HOME` 是**测试/影子环境隔离**变量，release 一律忽略（防止环境误设把
// DB 与 skills 数据割裂）；而 `TUVIS_DSH_STATE_DIR` 是**用户可操作的修复旋钮**
// ——`DSH_STATE_PATH_TOO_LONG` 的文案就要求用户设它（release 用户同样会遇到深
// 路径/长用户名）。它只重定位 DSH 控制域自己的状态目录（授权密钥/绑定账本/
// rendezvous/本机套接字），不碰账本库与其它数据；不设时行为与旧版逐字一致。

/// 生产环境变量快照（R10）。**纯数据**：`from_process()` 是唯一读进程环境的
/// 入口，其余解析函数只吃本结构——测试因此不需要改动进程环境（并行安全）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DshEnvSnapshot {
    /// `TUVIS_DSH_STATE_DIR`（两端同义：**它本身就是** DSH 状态目录）。
    /// `None`/空串 = 未设置 → 回落到 `<tuvis_home>/dsh`。
    pub dsh_state_dir: Option<PathBuf>,
}

impl DshEnvSnapshot {
    /// 从进程环境取快照（生产唯一入口；所有语义解析都在纯函数里）。
    ///
    /// 空串按未设置处理：Windows 上「设成空」是常见的清空方式，把它当成
    /// 「根目录」会把密钥写到盘符根，必须避免。
    pub fn from_process() -> Self {
        let state_dir = std::env::var_os(super::local_ipc::STATE_DIR_ENV)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        Self {
            dsh_state_dir: state_dir,
        }
    }
}

/// 解析 DSH 状态目录（纯函数；R10 的唯一解析点）。
///
/// * 覆盖变量已设 → **恰好**该目录（不再拼 `.tuvis` 或 `dsh` 一层：变量即状态
///   目录，两端同义，插件照此读 rendezvous）；
/// * 未设 → `<tuvis_home>/dsh`，`tuvis_home` 由调用方按既有
///   `app_data_home().join(".tuvis")` 给出（debug 认 `TUVIS_HOME`、release 不认，
///   `database::connection` 的既有语义原样保留）。
pub fn resolve_state_dir(snapshot: &DshEnvSnapshot, tuvis_home: &Path) -> PathBuf {
    match snapshot.dsh_state_dir.as_ref() {
        Some(dir) => dir.clone(),
        None => tuvis_home.join("dsh"),
    }
}

/// 纯装配内核：由快照 + 应用数据根造生产环境（不读进程环境，测试直接注入）。
///
/// `app_data_home` 由生产入口传 `crate::database::connection::app_data_home()`
/// 的结果；测试传影子根即可覆盖「debug 设 `TUVIS_HOME`」这一支的产物形态。
pub fn build_env(snapshot: &DshEnvSnapshot, app_data_home: PathBuf) -> DshEnvironment {
    let tuvis_home = app_data_home.join(".tuvis");
    DshEnvironment {
        // DSH home 单源复用 monitor 的解析（DSH_HOME 影子目录优先）。
        home: crate::monitor::dsh::dsh_home(),
        dsh_state_dir: snapshot.dsh_state_dir.clone(),
        tuvis_home,
        processes: Arc::new(SysinfoProcessSource::new()),
        files: Arc::new(FsFileSource),
        runner: Arc::new(CliRunner),
        clock: Arc::new(SystemClock),
        // 生产：Web home 归属靠真实 KERN_PROCARGS2 观测（非 macOS 恒 None = 不可观测）。
        environments: Arc::new(super::model::SystemProcessEnvironment),
    }
}

/// 解析并校验生产 DSH 状态目录：路径过长 → `DSH_STATE_PATH_TOO_LONG`。
///
/// 长度校验放在**解析点**（而不是等 `bind` 报 `SUN_LEN`）：深影子目录/长用户名
/// 下用户看到的第一条信息必须是可以动手的旋钮名（group 1 的
/// [`super::local_ipc::check_state_dir_length`] 负责文案与预算）。
pub fn checked_state_dir(env: &DshEnvironment) -> Result<PathBuf, DshError> {
    let dir = resolve_state_dir(
        &DshEnvSnapshot {
            dsh_state_dir: env.dsh_state_dir.clone(),
        },
        &env.tuvis_home,
    );
    super::local_ipc::check_state_dir_length(&dir)?;
    Ok(dir)
}

#[cfg(test)]
pub(crate) mod tests {
    #[test]
    fn http_send_admission_requires_matching_host_epoch_and_authority() {
        // 选择镜像是进程全局状态，用独立进程隔离并行的切换测试。
        const CHILD: &str = "TUVIS_TEST_SEND_HOST_STAMP_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let test_name = format!(
                "{}::http_send_admission_requires_matching_host_epoch_and_authority",
                module_path!().split_once("::").unwrap().1,
            );
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &test_name, "--nocapture"])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
                "子进程未通过: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            return;
        }

        const INSTANCE: &str = "inst-http-send-stamp";
        const EPOCH: u64 = 7;
        const AUTHORITY: &str = "127.0.0.1:17301";
        let admission = || {
            let mut admission = SendAdmission::capture(Some(INSTANCE));
            admission.host = Some(Ok(SendHostStamp {
                instance: INSTANCE.into(),
                epoch: EPOCH,
                authority: AUTHORITY.into(),
            }));
            admission
        };
        let host = |epoch, authority: &str| LocalHost {
            host_id: INSTANCE.into(),
            platform: "macos".into(),
            profile_dir: Default::default(),
            executable: Default::default(),
            pid: 1,
            started_at_ms: 1,
            epoch,
            authority: authority.into(),
            package_sha256: String::new(),
        };

        assert!(admission().bind(&host(EPOCH, AUTHORITY)).is_ok());
        for changed in [host(EPOCH + 1, AUTHORITY), host(EPOCH, "127.0.0.1:17302")] {
            let error = admission().bind(&changed).expect_err("拒绝已变化的宿主");
            assert_eq!(error.code, codes::HOST_NOT_RUNNING);
        }
    }

    #[test]
    fn publish_under_closing_lock_rejects_and_allows() {
        // 选择镜像是进程全局状态，子进程隔离可避免其他上传或选择测试互相干扰。
        const CHILD: &str = "TUVIS_TEST_PUBLISH_LOCK_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "services::dsh_control::production::tests::publish_under_closing_lock_rejects_and_allows", "--nocapture"])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
                "子进程未通过: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        const INSTANCE: &str = "inst-test-publish-lock";
        let calls = std::cell::Cell::new(0);
        let publish = || {
            calls.set(calls.get() + 1);
            "published"
        };
        let epoch = mark_closing(INSTANCE);
        let generation = instance_generation(INSTANCE);
        assert_eq!(
            publish_under_closing_lock(INSTANCE, generation, publish),
            Err(PublishDecisionError::InstanceClosing)
        );
        clear_closing_if(INSTANCE, epoch);
        assert_eq!(
            publish_under_closing_lock(INSTANCE, generation - 1, publish),
            Err(PublishDecisionError::StaleUploadGeneration)
        );
        let previous_selection = selected_mirror();
        set_selected_mirror(Some("other-instance"));
        assert_eq!(
            publish_under_closing_lock(INSTANCE, generation, publish),
            Err(PublishDecisionError::InstanceSwitched)
        );
        assert_eq!(calls.get(), 0);
        set_selected_mirror(previous_selection.as_deref());
        assert_eq!(
            publish_under_closing_lock(INSTANCE, generation, publish),
            Ok("published")
        );
        assert_eq!(calls.get(), 1);
    }

    use super::*;

    /// 提交边界测试的进程级串行门（销项复审 Important-7：操纵全局提交边界
    /// 的测试——含 switch_tests 的新关闭用例——一律在此排队，消除并行串扰）。
    pub(crate) static TEST_BOUNDARY_SERIAL: std::sync::LazyLock<tokio::sync::Mutex<()>> =
        std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

    struct PauseDisarm;

    impl Drop for PauseDisarm {
        fn drop(&mut self) {
            TEST_COMMIT_PERMIT_PAUSE_ARMED.store(false, std::sync::atomic::Ordering::SeqCst);
            TEST_COMMIT_PERMIT_RESUME.notify_one();
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn flip_pending_blocks_new_permits_until_flip_completes() {
        let _serial = TEST_BOUNDARY_SERIAL.lock().await;
        use std::time::Duration;

        let flip = tokio::task::spawn_blocking(commit_flip_begin)
            .await
            .expect("获取切换守卫");
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let mut enter = tokio::spawn(async move {
            let _ = started_tx.send(());
            commit_permit_enter().await
        });
        started_rx.await.expect("许可任务已启动");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut enter)
                .await
                .is_err(),
            "切换期间新许可不得进入"
        );

        drop(flip);
        let permit = tokio::time::timeout(Duration::from_secs(1), &mut enter)
            .await
            .expect("切换完成后许可应及时进入")
            .expect("许可任务正常结束");
        drop(permit);
        let next_flip = tokio::time::timeout(
            Duration::from_secs(1),
            tokio::task::spawn_blocking(commit_flip_begin),
        )
        .await
        .expect("释放守卫后应能再次切换")
        .expect("切换任务正常结束");
        drop(next_flip);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn permit_enter_reverts_when_flip_acquires_mid_entry() {
        let _serial = TEST_BOUNDARY_SERIAL.lock().await;
        use std::sync::atomic::Ordering;
        use std::time::Duration;

        // 待实现的测试钩子在计数递增后、状态复查前暂停许可入口。
        TEST_COMMIT_PERMIT_PAUSE_ARMED.store(true, Ordering::SeqCst);
        let _disarm = PauseDisarm;
        let mut enter = tokio::spawn(commit_permit_enter());
        tokio::time::timeout(Duration::from_secs(1), async {
            while COMMIT_IN_FLIGHT.load(Ordering::SeqCst) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("许可入口应在计数后暂停");

        let mut flip = tokio::task::spawn_blocking(commit_flip_begin);
        tokio::time::timeout(Duration::from_secs(1), async {
            while COMMIT_BOUNDARY_STATE.load(Ordering::SeqCst) != COMMIT_STATE_FLIPPING {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("切换应抢占状态并等待排空");
        assert!(!flip.is_finished(), "计数未回退前不得取得切换守卫");

        TEST_COMMIT_PERMIT_PAUSE_ARMED.store(false, Ordering::SeqCst);
        TEST_COMMIT_PERMIT_RESUME.notify_one();
        let guard = tokio::time::timeout(Duration::from_secs(1), &mut flip)
            .await
            .expect("许可回退后切换应完成排空")
            .expect("切换任务正常结束");
        assert_eq!(
            COMMIT_IN_FLIGHT.load(Ordering::SeqCst),
            0,
            "许可必须回退计数"
        );
        let blocked = tokio::time::timeout(Duration::from_millis(50), &mut enter)
            .await
            .is_err();
        drop(guard);
        assert!(blocked, "切换守卫存活期间许可不得进入");
        let permit = tokio::time::timeout(Duration::from_secs(1), &mut enter)
            .await
            .expect("切换结束后许可应进入")
            .expect("许可任务正常结束");
        drop(permit);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn flip_begin_waits_for_existing_permit() {
        let _serial = TEST_BOUNDARY_SERIAL.lock().await;
        use std::time::Duration;

        let permit = commit_permit_enter().await;
        let mut flip = tokio::task::spawn_blocking(commit_flip_begin);
        let blocked = tokio::time::timeout(Duration::from_millis(50), &mut flip)
            .await
            .is_err();
        drop(permit);
        assert!(blocked, "已有许可未释放时切换不得完成");
        let guard = tokio::time::timeout(Duration::from_secs(1), &mut flip)
            .await
            .expect("许可释放后切换应及时完成")
            .expect("切换任务正常结束");
        drop(guard);
    }

    /// R10(a)：覆盖变量已设 → 状态目录**恰好**是它（不拼 `.tuvis`/`dsh`）。
    #[test]
    fn state_dir_override_is_exactly_the_given_directory() {
        let snapshot = DshEnvSnapshot {
            dsh_state_dir: Some(PathBuf::from("/tmp/short-dsh-state")),
        };
        let env = build_env(&snapshot, PathBuf::from("/tmp/shadow"));
        assert_eq!(
            resolve_state_dir(
                &DshEnvSnapshot {
                    dsh_state_dir: env.dsh_state_dir.clone()
                },
                &env.tuvis_home
            ),
            PathBuf::from("/tmp/short-dsh-state"),
            "变量即状态目录，不得再多一层"
        );
        // 覆盖时 `<tuvis_home>` 仍是常规形态（管理账本/插件缓存继续落那里）。
        assert_eq!(env.tuvis_home, PathBuf::from("/tmp/shadow/.tuvis"));
    }

    /// R10(b)：未设覆盖 + debug 的 `TUVIS_HOME=/X` → `<X>/.tuvis/dsh`（行为不变）。
    #[test]
    fn unset_override_keeps_tuvis_home_dsh_root() {
        let snapshot = DshEnvSnapshot::default();
        // /X 就是 debug 下 app_data_home() 对 `TUVIS_HOME=/X` 的返回值。
        let env = build_env(&snapshot, PathBuf::from("/X"));
        assert_eq!(env.tuvis_home, PathBuf::from("/X/.tuvis"));
        assert_eq!(env.dsh_state_dir, None);
        assert_eq!(
            resolve_state_dir(&DshEnvSnapshot::default(), &env.tuvis_home),
            PathBuf::from("/X/.tuvis/dsh"),
            "旧口径逐字保留"
        );
    }

    /// R10(c)：长度校验作用在**解析后**的路径上（覆盖目录同样受 103 字节约束）。
    #[test]
    fn length_check_fires_on_resolved_path() {
        let long = PathBuf::from(format!("/{}", "a".repeat(200)));
        let env = build_env(
            &DshEnvSnapshot {
                dsh_state_dir: Some(long),
            },
            PathBuf::from("/tmp/shadow"),
        );
        let err = checked_state_dir(&env).expect_err("超长覆盖目录必须被拒");
        assert_eq!(err.code, codes::STATE_PATH_TOO_LONG);
        assert!(
            err.detail.contains("TUVIS_DSH_STATE_DIR"),
            "文案必须指向用户可改的变量：{}",
            err.detail
        );
    }

    /// 空串按未设置处理（Windows 清空变量的常见形态，绝不能变成盘符根）。
    #[test]
    fn empty_override_falls_back_to_default_root() {
        assert_eq!(
            resolve_state_dir(&DshEnvSnapshot::default(), Path::new("/tmp/tuvis-home")),
            PathBuf::from("/tmp/tuvis-home/dsh")
        );
    }

    /// 对照：短覆盖目录通过校验（策略不误伤正常短路径）。
    #[test]
    fn short_override_passes_length_check() {
        let env = build_env(
            &DshEnvSnapshot {
                dsh_state_dir: Some(PathBuf::from("/tmp/s")),
            },
            PathBuf::from("/tmp/shadow"),
        );
        assert_eq!(
            checked_state_dir(&env).expect("短目录必须通过"),
            PathBuf::from("/tmp/s")
        );
    }

    // --------------------------------------------------------
    // R09：授权域初始化的失败隔离（不得 panic 拖垮应用启动）
    // --------------------------------------------------------

    /// 损坏的 `authorization.key`（非 JSON 垃圾）→ 初始化**返回错误**而不是 panic。
    ///
    /// 旧实现（`DshAuthService::new(env).expect("production dsh auth key init")`）
    /// 在这里直接 abort：整个兔维斯启动失败（所有工具都不可用），而 DSH 控制域
    /// 只是可选能力。本用例守的就是这条边界。
    #[test]
    fn corrupt_key_is_isolated_as_error_not_panic() {
        let tmp = tempfile::tempdir().expect("tempdir 可建");
        let state = tmp.path().join("state");
        std::fs::create_dir_all(&state).expect("建状态目录");
        std::fs::write(state.join("authorization.key"), b"not-a-json-key-file").expect("写坏密钥");

        let env = build_env(
            &DshEnvSnapshot {
                dsh_state_dir: Some(state),
            },
            tmp.path().to_path_buf(),
        );
        let err = match init_auth_service(&env) {
            Ok(_) => panic!("损坏密钥必须如实报错（不 panic）"),
            Err(e) => e,
        };
        assert_eq!(
            err.code,
            codes::IPC_STATE,
            "错误码沿用既有 IPC 状态码：{err}"
        );
        assert!(
            err.detail.contains("authorization.key"),
            "detail 必须指向坏文件（可诊断）：{}",
            err.detail
        );
    }

    /// 正面控制：正常状态目录（无密钥 = 首次启动）初始化成功（隔离不得误伤）。
    #[test]
    fn healthy_state_dir_initializes() {
        let tmp = tempfile::tempdir().expect("tempdir 可建");
        let env = build_env(
            &DshEnvSnapshot {
                dsh_state_dir: Some(tmp.path().join("state")),
            },
            tmp.path().to_path_buf(),
        );
        assert!(
            init_auth_service(&env).is_ok(),
            "正常状态目录必须能建授权服务"
        );
        assert!(
            tmp.path().join("state").join("authorization.key").exists(),
            "首次启动必须生成授权私钥"
        );
    }

    // --------------------------------------------------------
    // 销项审查发现 2（P2）：enable 清旗与推进代次的**同锁复合操作**
    // --------------------------------------------------------

    /// `clear_closing_and_advance` 原子语义——enable 提交 enabled 后的「按观察
    /// 纪元清旗」与「推进实例代次」必须在**同一次加锁**内完成。两步分开锁的旧
    /// 实现存在「已清旗、未推进」窗口：其间受理的上传捕获旧代次，enable 推进后
    /// 在附件发布点被**误拒** stale_upload_generation；复合化后窗口不复存在。
    /// 并发竞态无法单线程复现，此处直接锁定复合函数的终态语义：调用成功 =
    /// 旗标退场且代次恰好 +1；纪元失配 = 既不拆旗也不推进（P1-2 条件清除语义
    /// 不变）。
    #[test]
    fn clear_closing_and_advance_is_atomic() {
        const ID: &str = "inst-clear-advance-atomic";
        let gen_before = instance_generation(ID);
        // 前提态：关闭落地（旗标在场，代次已推进一次）。
        let epoch = mark_closing(ID);
        assert!(is_closing(ID), "前提：关闭旗标在场");
        assert_eq!(
            instance_generation(ID),
            gen_before + 1,
            "前提：mark_closing 已推进一次代次"
        );
        // 复合调用：清旗 + 推进同锁完成。
        assert!(
            clear_closing_and_advance(ID, epoch),
            "按观察纪元复合清除必须成功"
        );
        assert!(!is_closing(ID), "调用后旗标必须退场");
        assert_eq!(
            instance_generation(ID),
            gen_before + 2,
            "调用后代次必须恰好再推进 +1（关闭侧 +1、复合 +1）"
        );
        // 纪元失配：既不拆旗也不推进（enable 的旧观察值不得拆新关闭）。
        let epoch2 = mark_closing(ID);
        assert!(
            !clear_closing_and_advance(ID, epoch2.wrapping_add(1_000)),
            "纪元失配必须返回 false"
        );
        assert!(is_closing(ID), "纪元失配不得拆旗");
        assert_eq!(
            instance_generation(ID),
            gen_before + 3,
            "纪元失配不得推进代次"
        );
        // 收尾：按正确纪元复合清除（旗标退场，不毒化同进程其他用例）。
        assert!(clear_closing_and_advance(ID, epoch2));
        assert!(!is_closing(ID), "收尾后旗标必须退场");
    }

    /// 终版复核 #2：受理快照（`closing_snapshot`）同锁语义——(旗标, 代次)
    /// 一次观测拿齐，与各推进原语的状态逐段对齐（拆开 `is_closing` +
    /// `instance_generation` 两次加锁的撕裂窗在单线程不可判定，这里锁定
    /// 快照读数与注册表状态的逐态一致）。
    #[test]
    fn closing_snapshot_matches_registry_states() {
        const ID: &str = "inst-snap-coherent";
        assert_eq!(
            closing_snapshot(ID),
            (false, 0),
            "未登记实例：旗标退场、代次 0"
        );
        let epoch = mark_closing(ID);
        let (closing, gen) = closing_snapshot(ID);
        assert!(closing, "mark_closing 后快照旗标在场");
        assert_eq!(gen, instance_generation(ID), "快照代次 == 单读代次");
        assert!(clear_closing_and_advance(ID, epoch));
        let (closing, gen) = closing_snapshot(ID);
        assert!(!closing, "复合清除后快照旗标退场");
        assert_eq!(gen, instance_generation(ID), "快照代次 == 单读代次");
        assert!(gen >= 2, "重开周期后代次 ≥ 2（关闭 +1、复合 +1）");
    }
}

#[cfg(test)]
mod spawn_contract_tests {
    use super::*;

    /// 缺少显式 home 时必须拒绝启动：多实例下「默认 home」不是中性选择，
    /// 而是确定的错误目标（另一个实例）。
    #[test]
    fn spawn_refuses_an_empty_home() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("fake-desktop");
        std::fs::write(&exe, b"#!/bin/sh\nexit 0\n").unwrap();
        let error =
            spawn_desktop_app(&exe, std::path::Path::new("")).expect_err("空 home 必须拒绝");
        assert_eq!(error.code, codes::PROFILE_NOT_INITIALIZED);
    }

    /// 可执行文件缺失优先报 DESKTOP_NOT_INSTALLED（先判存在再判 home）。
    #[test]
    fn spawn_reports_missing_executable_before_touching_home() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        let error = spawn_desktop_app(&missing, dir.path()).expect_err("缺失可执行文件");
        assert_eq!(error.code, codes::DESKTOP_NOT_INSTALLED);
    }

    // 「两级上溯」的契约测试已移到 `instances::session_home_tests`——那里测的是
    // **生产实际使用的** `session_home`（`production.rs:508` 把它赋给 `env.home`）。
    // 本处原有的一条只对字面量 `Path` 做 `parent().parent()`，从不调用任何生产函数，
    // 删掉生产代码它照样绿，因此不构成覆盖。
}
