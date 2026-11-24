//! T02：Desktop 安装、profile 与唯一实例定位（identity）行为测试。
//!
//! 全部输入经注入式 `DshEnvironment`（假进程源 / 假文件源 / 假时钟），不触真实
//! 进程、真实 `~/.dsh`、真实网络（计划全局约束）。夹具取值来自 E2/E3 取证报告
//! 的实测路径形态（非系统盘 + 空格、Mac app bundle、宿主 argv profile 令牌、
//! 动态端口）：端口与盘符只是**数据**，生产逻辑不得特判（计划红线：不硬编码
//! 63026/5214/3080 或 C 盘）。

#[path = "support/dsh_management.rs"]
mod dsh_management;
use dsh_management::desktop_host_process;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use multi_agents_manager_lib::services::dsh_control::error::{codes, DshError};
use multi_agents_manager_lib::services::dsh_control::identity::{
    discover, discover_all, resolve_installation, DesktopInstallation,
};
use multi_agents_manager_lib::services::dsh_control::instances::{instance_id_for, HostKind};
use multi_agents_manager_lib::services::dsh_control::model::{
    CommandRequest, DshClock, DshCommandRunner, DshEnvironment, DshFileSource, DshListener,
    DshProcess, DshProcessSource, RunOutcome,
};

// ============================================================
// 注入假体
// ============================================================

/// Win 夹具常量（E2 报告 §1.1/§1.2 实测形态；值是数据不是生产假设）。
const WIN_HOME: &str = r"C:\Users\bunny\.dsh";
const WIN_PROFILE_TOKEN: &str = r"C:\Users\bunny\.dsh\profiles\desktop";
const WIN_INSTALL_DIR: &str = r"D:\Program Files\Deepseek-Harness";

/// 词法层父目录（'/' 与 '\' 都算分隔；Windows 形态夹具在 POSIX 测试主机上
/// `Path::parent` 不识别 '\'——file_identity.rs 同款词法处理）。
#[path = "support/dsh_paths.rs"]
mod dsh_paths;
use dsh_paths::{join_lexical, lexical_parent};

/// 以 base 的分隔符语义拼接相对路径（保持 Windows 夹具路径的原生反斜杠形态）。
/// 假文件源：内存文件/目录 + **触碰记录**（验收④只读扫描的取证面）。
#[derive(Default)]
struct FakeFiles {
    /// 文件 → 内容（内容只喂描述文件哈希；identity 不得解读会话正文）。
    files: BTreeMap<PathBuf, Vec<u8>>,
    dirs: BTreeSet<PathBuf>,
    /// identity 触碰过的全部路径（exists/list_dir/read_small 都记）。
    touched: Mutex<Vec<PathBuf>>,
}

impl FakeFiles {
    fn add_file(&mut self, path: PathBuf) {
        self.files.entry(path).or_default();
    }

    fn add_file_with(&mut self, path: PathBuf, contents: &[u8]) {
        self.files.insert(path, contents.to_vec());
    }

    fn add_dir(&mut self, path: PathBuf) {
        self.dirs.insert(path);
    }

    fn touched(&self) -> Vec<PathBuf> {
        self.touched.lock().unwrap().clone()
    }

    fn record(&self, path: &Path) {
        self.touched.lock().unwrap().push(path.to_path_buf());
    }
}

impl DshFileSource for FakeFiles {
    fn exists(&self, path: &Path) -> bool {
        self.record(path);
        self.files.contains_key(path) || self.dirs.contains(path)
    }

    fn list_dir(&self, path: &Path) -> Vec<PathBuf> {
        self.record(path);
        let dir = path
            .to_string_lossy()
            .trim_end_matches(['/', '\\'])
            .to_string();
        let mut out: Vec<PathBuf> = self
            .files
            .keys()
            .chain(self.dirs.iter())
            .filter(|p| {
                lexical_parent(&p.to_string_lossy())
                    .map(|parent| parent.trim_end_matches(['/', '\\']) == dir)
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        out.sort();
        out.dedup();
        out
    }

    fn read_small(&self, path: &Path, max_bytes: u64) -> Result<Vec<u8>, DshError> {
        self.record(path);
        match self.files.get(path) {
            Some(bytes) if bytes.len() as u64 <= max_bytes => Ok(bytes.clone()),
            Some(bytes) => Err(DshError::new(
                codes::UNSUPPORTED,
                format!("fake: file over budget: {path:?} ({} bytes)", bytes.len()),
            )),
            None => Err(DshError::new(
                codes::UNSUPPORTED,
                format!("fake: no such file: {path:?}"),
            )),
        }
    }
}

/// 假进程/监听源（无 sysinfo；快照由测试直接给定）。
struct FakeProcs {
    processes: Vec<DshProcess>,
    listeners: Vec<DshListener>,
}

impl DshProcessSource for FakeProcs {
    fn list_processes(&self) -> Vec<DshProcess> {
        self.processes.clone()
    }

    fn listening_sockets(&self) -> Vec<DshListener> {
        self.listeners.clone()
    }
}

struct NullRunner;
impl DshCommandRunner for NullRunner {
    fn run(&self, _request: &CommandRequest) -> Result<RunOutcome, DshError> {
        Err(DshError::new(codes::UNSUPPORTED, "no runner in this test"))
    }
}

struct FixedClock(i64);
impl DshClock for FixedClock {
    fn now_ms(&self) -> i64 {
        self.0
    }
}

fn test_env(home: &Path, procs: FakeProcs, files: FakeFiles) -> DshEnvironment {
    DshEnvironment {
        home: home.to_path_buf(),
        tuvis_home: join_lexical(&home.to_string_lossy(), &["tuvis-shadow"]),
        dsh_state_dir: None,
        processes: Arc::new(procs),
        files: Arc::new(files),
        runner: Arc::new(NullRunner),
        clock: Arc::new(FixedClock(0)),
        // 测试默认「不可观测」：显式拒绝 Web home 归属（fail-closed），
        // 绝不让假进程源落到真实系统 PID 上。需要 Web 归属的用例自行注入假观测器。
        environments: std::sync::Arc::new(
            multi_agents_manager_lib::services::dsh_control::model::TestUnobservableEnvironments,
        ),
    }
}

// ============================================================
// 夹具构造（E2/E3 实测形态）
// ============================================================

/// Desktop 宿主子进程 argv（Win 报告 §1.2 实测形态：exe --expose-internals
/// dsh-desktop-host\lib\index.js + profile 令牌 + runtime 目录）。
fn listener(owner_pid: u32, authority: &str) -> DshListener {
    DshListener {
        owner_pid,
        authority: authority.into(),
    }
}

/// desktop profile 种法（Win 报告 §1.3：profile 独占 package.json）。
#[derive(Clone, Copy)]
enum ProfileSeed {
    /// profile 目录 + 官方标记文件（初始化完成）。
    Full,
    /// 只有目录、缺官方标记文件（未初始化形态之一）。
    DirWithoutMarker,
    /// 整个 profile 目录缺席（未初始化形态之二）。
    None,
}

/// 官方安装 + desktop profile。中间安装目录要显式登记（真实文件系统按存在性
/// 隐含列出目录，假源需要显式条目）。描述文件给少量内容喂哈希。
fn seed_win_install(files: &mut FakeFiles, install_dir: &str, profile: ProfileSeed) {
    let dir = PathBuf::from(install_dir);
    files.add_dir(dir.clone());
    files.add_file(join_lexical(
        &dir.to_string_lossy(),
        &["DeepSeek Harness.exe"],
    ));
    files.add_file_with(
        join_lexical(&dir.to_string_lossy(), &["resources", "app-update.yml"]),
        b"provider: generic\n",
    );
    files.add_file(join_lexical(
        &dir.to_string_lossy(),
        &["resources", "runtime", "cli", "bin", "dsh.cmd"],
    ));
    if let ProfileSeed::None = profile {
        return;
    }
    let profile_dir = join_lexical(WIN_HOME, &["profiles", "desktop"]);
    files.add_dir(profile_dir.clone());
    if let ProfileSeed::Full = profile {
        files.add_file(join_lexical(
            &profile_dir.to_string_lossy(),
            &["package.json"],
        ));
    }
}

/// 完整可用的 Win 安装夹具（安装 + profile 齐备）。
fn seeded_win_files() -> FakeFiles {
    let mut files = FakeFiles::default();
    seed_win_install(&mut files, WIN_INSTALL_DIR, ProfileSeed::Full);
    files
}

/// 一台在跑的宿主（E2 报告 Win 形态：pid + 动态端口 5214 作数据）。
fn running_host_env(pid: u32, started_at_ms: i64, files: FakeFiles) -> DshEnvironment {
    test_env(
        Path::new(WIN_HOME),
        FakeProcs {
            processes: vec![desktop_host_process(
                pid,
                WIN_INSTALL_DIR,
                WIN_PROFILE_TOKEN,
                started_at_ms,
            )],
            listeners: vec![listener(pid, "127.0.0.1:5214")],
        },
        files,
    )
}

// ============================================================
// RED 用例（brief 指定）
// ============================================================

/// RED-1（brief 指定）：两个合格宿主在场时**禁止** monitor 的「取第一个命中」
/// ——控制面必须报实例多义，不得静默选边。
#[test]
fn ambiguous_hosts_are_not_first_match() {
    let home = Path::new(WIN_HOME);
    let mut files = seeded_win_files();
    // 第二实例：E 盘安装 + 影子 home 的 profile（同机双开形态）
    seed_win_install(&mut files, r"E:\Apps\Deepseek-Harness", ProfileSeed::Full);
    let procs = FakeProcs {
        processes: vec![
            desktop_host_process(37096, WIN_INSTALL_DIR, WIN_PROFILE_TOKEN, 1_000),
            desktop_host_process(
                41000,
                r"E:\Apps\Deepseek-Harness",
                r"E:\shadow\.dsh\profiles\desktop",
                2_000,
            ),
        ],
        listeners: vec![
            listener(37096, "127.0.0.1:5214"),
            listener(41000, "127.0.0.1:63026"),
        ],
    };
    let env = test_env(home, procs, files);
    let two_hosts = &env;
    assert_eq!(
        discover(two_hosts).unwrap_err().code,
        "DSH_TARGET_AMBIGUOUS"
    );
}

/// RED-2（brief 指定）：E3 非系统盘 + 空格安装路径——自带 CLI 必须原样保留
/// （相对安装根拼装，不被 PATH/npm 全局形态改写）；Desktop 未运行仍可解析。
#[test]
fn non_system_drive_cli_is_preserved() {
    let home = Path::new(WIN_HOME);
    let mut files = seeded_win_files();
    // npm 全局 dsh 在场（诱饵：绝不能当成 Desktop 自带 CLI）
    files.add_file(PathBuf::from(r"C:\Users\bunny\AppData\Roaming\npm\dsh.cmd"));
    // 无任何进程 = Desktop 未运行
    let env = test_env(
        home,
        FakeProcs {
            processes: vec![],
            listeners: vec![],
        },
        files,
    );
    let installation: DesktopInstallation = resolve_installation(&env).unwrap();
    let expected_bundled_cli =
        PathBuf::from(r"D:\Program Files\Deepseek-Harness\resources\runtime\cli\bin\dsh.cmd");
    assert_eq!(installation.cli_path, expected_bundled_cli);
    assert_eq!(
        installation.executable,
        PathBuf::from(r"D:\Program Files\Deepseek-Harness\DeepSeek Harness.exe")
    );
    assert_eq!(installation.platform, "windows");
}

/// npm 全局 dsh（/usr/local/bin/dsh、%APPDATA%\npm\dsh.cmd）**不是** Desktop
/// 安装：只有 npm CLI 时 = 未安装；装了 Desktop 后自带 CLI 也不让位给 npm 路径。
#[test]
fn npm_cli_is_not_desktop_cli() {
    let home = Path::new(WIN_HOME);
    // 只有 npm 全局 CLI，无任何 Desktop 安装
    let mut npm_only = FakeFiles::default();
    npm_only.add_file(PathBuf::from("/usr/local/bin/dsh"));
    npm_only.add_file(PathBuf::from(r"C:\Users\bunny\AppData\Roaming\npm\dsh.cmd"));
    npm_only.add_file(PathBuf::from(
        r"C:\Users\bunny\AppData\Roaming\npm\node_modules\.bin\dsh.cmd",
    ));
    let env = test_env(
        home,
        FakeProcs {
            processes: vec![],
            listeners: vec![],
        },
        npm_only,
    );
    assert_eq!(
        resolve_installation(&env).unwrap_err().code,
        codes::DESKTOP_NOT_INSTALLED
    );
    // 同一机器再装上 Desktop：解析出的必须是安装包自带 CLI，不是 npm 全局那个
    let mut with_desktop = FakeFiles::default();
    with_desktop.add_file(PathBuf::from("/usr/local/bin/dsh"));
    with_desktop.add_file(PathBuf::from(r"C:\Users\bunny\AppData\Roaming\npm\dsh.cmd"));
    seed_win_install(&mut with_desktop, WIN_INSTALL_DIR, ProfileSeed::Full);
    let env = test_env(
        home,
        FakeProcs {
            processes: vec![],
            listeners: vec![],
        },
        with_desktop,
    );
    let installation = resolve_installation(&env).unwrap();
    assert_eq!(
        installation.cli_path,
        PathBuf::from(r"D:\Program Files\Deepseek-Harness\resources\runtime\cli\bin\dsh.cmd")
    );
}

// ============================================================
// 验收用例
// ============================================================

/// 验收①a：单一特征假进程不得入选——只有 `dsh-desktop-host` 令牌（缺 profile
/// 令牌）的进程，以及 npm web 宿主（node dsh web），都不合格。
#[test]
fn fake_process_without_dual_features_is_rejected() {
    let home = Path::new(WIN_HOME);
    let single_feature = DshProcess {
        pid: 5000,
        exe: PathBuf::from(r"D:\Program Files\Deepseek-Harness\DeepSeek Harness.exe"),
        cmdline: vec![
            r"D:\Program Files\Deepseek-Harness\DeepSeek Harness.exe".into(),
            "--expose-internals".into(),
            r"D:\Program Files\Deepseek-Harness\resources\app.asar\dsh\node_modules\@deepseek-ai\dsh-desktop-host\lib\index.js".into(),
        ],
        started_at_ms: 1_000,
    };
    let web_host = DshProcess {
        pid: 5001,
        exe: PathBuf::from("/usr/local/bin/node"),
        cmdline: vec!["node".into(), "/usr/local/bin/dsh".into(), "web".into()],
        started_at_ms: 1_001,
    };
    let env = test_env(
        home,
        FakeProcs {
            processes: vec![single_feature, web_host],
            listeners: vec![listener(5000, "127.0.0.1:4001")],
        },
        seeded_win_files(),
    );
    // 安装与 profile 都在 → 唯一缺的是合格宿主进程
    assert_eq!(discover(&env).unwrap_err().code, codes::HOST_NOT_RUNNING);
}

/// 验收①b：监听属主核验——回环端口不归宿主 pid 所有（假监听），或宿主只有
/// 非回环监听，都不得作为宿主 authority。
#[test]
fn listener_owned_by_other_pid_is_rejected() {
    let home = Path::new(WIN_HOME);
    let env = test_env(
        home,
        FakeProcs {
            processes: vec![desktop_host_process(
                37096,
                WIN_INSTALL_DIR,
                WIN_PROFILE_TOKEN,
                1_000,
            )],
            listeners: vec![
                // 5214 被别的进程占着（假监听）+ 宿主自己只有非回环监听——都不认
                listener(999, "127.0.0.1:5214"),
                listener(37096, "192.168.1.5:5214"),
            ],
        },
        seeded_win_files(),
    );
    assert_eq!(
        discover(&env).unwrap_err().code,
        codes::HOST_LISTENER_MISSING
    );
}

/// 验收②：相同端口不同代际不复用身份——进程 PID+启动时刻变了，代际必须变
/// （端口动态可复用，不参与代际派生）；同一进程身份重复发现代际稳定。
#[test]
fn same_port_new_generation_gets_new_epoch() {
    let env_a = running_host_env(37096, 1_000, seeded_win_files());
    let host_a = discover(&env_a).unwrap();
    // 换代：同端口（数据 5214），不同 pid + 启动时刻
    let env_b = running_host_env(41000, 2_000, seeded_win_files());
    let host_b = discover(&env_b).unwrap();
    assert_eq!(host_b.authority, host_a.authority, "端口相同（数据）");
    assert_ne!(
        host_a.epoch, host_b.epoch,
        "同端口换代必须换代际：pid/启动时刻变了代际不得复用"
    );
    // 同一进程身份重复发现：代际稳定（无状态派生，discover 可重复调用）
    let env_a2 = running_host_env(37096, 1_000, seeded_win_files());
    assert_eq!(discover(&env_a2).unwrap().epoch, host_a.epoch);
}

/// 验收③：中文 + 空格安装目录成功（安装目录名任意——产品锚只认官方可执行
/// 文件名，不假设安装目录拼写；探测根内的条目名全自由）。
#[test]
fn chinese_and_space_install_dir_resolves() {
    let home = Path::new(WIN_HOME);
    let mut files = FakeFiles::default();
    seed_win_install(
        &mut files,
        r"D:\Program Files\深思 Harness 目录",
        ProfileSeed::Full,
    );
    let env = test_env(
        home,
        FakeProcs {
            processes: vec![],
            listeners: vec![],
        },
        files,
    );
    let installation = resolve_installation(&env).unwrap();
    assert_eq!(
        installation.cli_path,
        PathBuf::from(r"D:\Program Files\深思 Harness 目录\resources\runtime\cli\bin\dsh.cmd")
    );
    assert_eq!(
        installation.executable,
        PathBuf::from(r"D:\Program Files\深思 Harness 目录\DeepSeek Harness.exe")
    );
}

/// 验收④：只读扫描不打开用户会话内容——假文件源记录全部触碰路径，discover
/// 全程不得触 `sessions/`、`storages/`、`session_projcache*` 等会话数据面。
#[test]
fn readonly_scan_never_opens_session_content() {
    let home = Path::new(WIN_HOME);
    let mut files = seeded_win_files();
    // 会话数据面真实在场（M0 实测形态：~/.dsh/sessions、storages/session_projcache）
    files.add_dir(join_lexical(WIN_HOME, &["sessions", "proj-a", "session-1"]));
    files.add_file(join_lexical(
        WIN_HOME,
        &[
            "storages",
            "session_projcache",
            "sessions",
            "session-1.json",
        ],
    ));
    let files = Arc::new(files);
    let env = DshEnvironment {
        home: home.to_path_buf(),
        tuvis_home: join_lexical(WIN_HOME, &["tuvis-shadow"]),
        dsh_state_dir: None,
        processes: Arc::new(FakeProcs {
            processes: vec![desktop_host_process(
                37096,
                WIN_INSTALL_DIR,
                WIN_PROFILE_TOKEN,
                1_000,
            )],
            listeners: vec![listener(37096, "127.0.0.1:5214")],
        }),
        files: files.clone(),
        runner: Arc::new(NullRunner),
        clock: Arc::new(FixedClock(0)),
        // 测试默认「不可观测」：显式拒绝 Web home 归属（fail-closed），
        // 绝不让假进程源落到真实系统 PID 上。需要 Web 归属的用例自行注入假观测器。
        environments: std::sync::Arc::new(
            multi_agents_manager_lib::services::dsh_control::model::TestUnobservableEnvironments,
        ),
    };
    let host = discover(&env).unwrap();
    assert_eq!(host.pid, 37096);
    let touched = files.touched();
    assert!(!touched.is_empty(), "触碰记录必须非空（检查本身要活着）");
    for path in touched {
        let s = path.to_string_lossy().replace('\\', "/");
        for banned in ["/sessions/", "/storages/", "session_projcache"] {
            assert!(!s.contains(banned), "identity 触碰了会话内容面: {s}");
        }
    }
}

// ============================================================
// 行为补充
// ============================================================

/// 唯一实例定位全字段：身份组合 = PID+启动时刻+exe+profile+监听属主；
/// 噪声进程（无双特征的 svchost）与其回环监听（pid 888 的 443）不得混入身份。
#[test]
fn discover_returns_unique_running_host_identity() {
    let mut files = seeded_win_files();
    files.add_file(PathBuf::from(r"C:\Windows\System32\svchost.exe"));
    let mut noise = desktop_host_process(888, WIN_INSTALL_DIR, WIN_PROFILE_TOKEN, 9);
    noise.cmdline = vec!["svchost".into(), "-k".into(), "netsvcs".into()];
    noise.exe = PathBuf::from(r"C:\Windows\System32\svchost.exe");
    let env = test_env(
        Path::new(WIN_HOME),
        FakeProcs {
            processes: vec![
                desktop_host_process(37096, WIN_INSTALL_DIR, WIN_PROFILE_TOKEN, 1_760_000_000_000),
                noise,
            ],
            listeners: vec![
                listener(37096, "127.0.0.1:5214"),
                listener(888, "127.0.0.1:443"),
            ],
        },
        files,
    );
    let host = discover(&env).unwrap();
    // H04：host_id = 稳定实例 ID（kind + profile 派生），不再固定 `local`；
    // 同一环境重复发现必须同一 ID（稳定派生，不掺运行代际）。
    let derived = instance_id_for(HostKind::Desktop, Path::new(WIN_PROFILE_TOKEN));
    assert_eq!(host.host_id, derived, "host_id 必须等于稳定实例 ID 派生值");
    assert_ne!(host.host_id, "local", "H04 起不再固定 local 运行身份");
    assert_eq!(discover(&env).unwrap().host_id, host.host_id);
    // 兼容委托：单候选时 discover_all 的候选 host 与 discover 同一身份。
    let candidates = discover_all(&env).unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].instance_id, host.host_id);
    assert_eq!(candidates[0].host, host);
    assert_eq!(candidates[0].kind, HostKind::Desktop);
    assert_eq!(candidates[0].display_name, "desktop");
    assert_eq!(host.platform, "windows");
    assert_eq!(host.pid, 37096);
    assert_eq!(host.started_at_ms, 1_760_000_000_000);
    assert_eq!(host.authority, "127.0.0.1:5214");
    assert_eq!(host.profile_dir, PathBuf::from(WIN_PROFILE_TOKEN));
    assert_eq!(
        host.executable,
        PathBuf::from(r"D:\Program Files\Deepseek-Harness\DeepSeek Harness.exe")
    );
    assert_ne!(host.epoch, 0);
    // 描述文件哈希 = sha256 hex 64 位（真实包级指纹属未测项——夹具 README，
    // 下轮授权采样前不冒充真实值）
    assert_eq!(host.package_sha256.len(), 64);
    assert!(host.package_sha256.chars().all(|c| c.is_ascii_hexdigit()));
}

/// Mac 形态：app bundle 内自带 CLI（Mac 报告 §3.2 实测路径）。
#[test]
fn macos_bundle_install_resolves_bundled_cli() {
    let home = Path::new("/Users/jarvis/.dsh");
    let mut files = FakeFiles::default();
    let app = "/Applications/DeepSeek Harness.app";
    files.add_dir(PathBuf::from(app));
    files.add_file(join_lexical(
        app,
        &["Contents", "MacOS", "DeepSeek Harness"],
    ));
    files.add_file(join_lexical(app, &["Contents", "Info.plist"]));
    files.add_file(join_lexical(
        app,
        &["Contents", "Resources", "runtime", "cli", "bin", "dsh"],
    ));
    let profile = join_lexical("/Users/jarvis/.dsh", &["profiles", "desktop"]);
    files.add_dir(profile.clone());
    files.add_file(join_lexical(&profile.to_string_lossy(), &["package.json"]));
    let env = test_env(
        home,
        FakeProcs {
            processes: vec![],
            listeners: vec![],
        },
        files,
    );
    let installation = resolve_installation(&env).unwrap();
    assert_eq!(
        installation.cli_path,
        PathBuf::from("/Applications/DeepSeek Harness.app/Contents/Resources/runtime/cli/bin/dsh")
    );
    assert_eq!(installation.platform, "macos");
}

/// 四态分开（brief GREEN）：未安装 / profile 未初始化（目录缺失或缺官方标记
/// 文件）/ 未运行 必须码可区分。
#[test]
fn not_installed_profile_and_running_states_are_distinct() {
    let home = Path::new(WIN_HOME);
    // 有安装、profile 目录整体缺失 → DSH_PROFILE_NOT_INITIALIZED
    let env = test_env(
        home,
        FakeProcs {
            processes: vec![],
            listeners: vec![],
        },
        {
            let mut bare = FakeFiles::default();
            seed_win_install(&mut bare, WIN_INSTALL_DIR, ProfileSeed::None);
            bare
        },
    );
    assert_eq!(
        resolve_installation(&env).unwrap_err().code,
        codes::PROFILE_NOT_INITIALIZED
    );
    assert_eq!(
        discover(&env).unwrap_err().code,
        codes::PROFILE_NOT_INITIALIZED
    );
    // profile 目录在场但缺官方标记文件（package.json）→ 仍未初始化
    let mut marker_missing = FakeFiles::default();
    seed_win_install(
        &mut marker_missing,
        WIN_INSTALL_DIR,
        ProfileSeed::DirWithoutMarker,
    );
    let env = test_env(
        home,
        FakeProcs {
            processes: vec![],
            listeners: vec![],
        },
        marker_missing,
    );
    assert_eq!(
        resolve_installation(&env).unwrap_err().code,
        codes::PROFILE_NOT_INITIALIZED
    );
    // 有安装有 profile、无宿主进程 → DSH_HOST_NOT_RUNNING
    let env = test_env(
        home,
        FakeProcs {
            processes: vec![],
            listeners: vec![],
        },
        seeded_win_files(),
    );
    assert_eq!(discover(&env).unwrap_err().code, codes::HOST_NOT_RUNNING);
}

// ============================================================
// N3 回归：自定义 DSH_HOME 的 profile 路径不得依赖字面 `.dsh`
// ============================================================

/// Mac 补探测报告 §6 第 6 条：宿主 argv 的 profile 令牌此前按字面
/// `/.dsh/profiles/desktop`（或反斜杠形态）匹配。当 DSH_HOME 指向**不含 `.dsh`**
/// 的目录（影子环境实测形态 `<…>/home-shadow/profiles/desktop`）时判定落空，
/// 宿主在控制面**隐身**（`DSH_HOST_NOT_RUNNING`），只剩 `env.home` 回落路径。
///
/// 契约：只要 argv 里有以 `<sep>profiles<sep>desktop` 结尾的令牌就算 profile
/// 特征（`/` 与 `\` 两种形态都认），与 home 目录叫什么无关；`dsh-desktop-host`
/// 令牌特征不变；监听属主核验不变。
#[test]
fn custom_home_profile_is_discovered_without_dsh_literal() {
    // 影子 home：全路径没有任何 `.dsh` 成分。
    let shadow_home = "/Users/jarvis/tuvis-dsh-shadow/home-shadow";
    let shadow_profile = format!("{shadow_home}/profiles/desktop");
    assert!(
        !shadow_profile.contains(".dsh"),
        "夹具必须不含 .dsh：{shadow_profile}"
    );

    let mut files = seeded_win_files();
    // 影子 profile 也要初始化（官方标记文件在场），否则会先撞 profile 门。
    files.add_dir(join_lexical(shadow_home, &["profiles", "desktop"]));
    files.add_file(join_lexical(&shadow_profile, &["package.json"]));

    let env = DshEnvironment {
        home: PathBuf::from(WIN_HOME),
        tuvis_home: join_lexical(WIN_HOME, &["tuvis-shadow"]),
        dsh_state_dir: None,
        processes: Arc::new(FakeProcs {
            processes: vec![desktop_host_process(
                51_000,
                WIN_INSTALL_DIR,
                &shadow_profile,
                1_760_000_000_000,
            )],
            listeners: vec![listener(51_000, "127.0.0.1:51583")],
        }),
        files: Arc::new(files),
        runner: Arc::new(NullRunner),
        clock: Arc::new(FixedClock(0)),
        // 测试默认「不可观测」：显式拒绝 Web home 归属（fail-closed），
        // 绝不让假进程源落到真实系统 PID 上。需要 Web 归属的用例自行注入假观测器。
        environments: std::sync::Arc::new(
            multi_agents_manager_lib::services::dsh_control::model::TestUnobservableEnvironments,
        ),
    };

    let host = discover(&env).expect("自定义 home 的宿主必须被发现（不得隐身）");
    assert_eq!(host.pid, 51_000);
    assert_eq!(host.authority, "127.0.0.1:51583");
    assert_eq!(
        host.profile_dir,
        PathBuf::from(&shadow_profile),
        "profile 目录必须取 argv 的权威值（不是 env.home 回落值）"
    );
    assert_ne!(host.epoch, 0);
}

/// 反斜杠形态同样成立：`<custom>\profiles\desktop` 不得要求 `\.dsh\` 字面量。
#[test]
fn custom_home_profile_with_backslashes_is_discovered() {
    let shadow_home = r"E:\shadow\home-shadow";
    let shadow_profile = r"E:\shadow\home-shadow\profiles\desktop";
    let mut files = seeded_win_files();
    files.add_dir(join_lexical(shadow_home, &["profiles", "desktop"]));
    files.add_file(join_lexical(shadow_profile, &["package.json"]));

    let env = DshEnvironment {
        home: PathBuf::from(WIN_HOME),
        tuvis_home: join_lexical(WIN_HOME, &["tuvis-shadow"]),
        dsh_state_dir: None,
        processes: Arc::new(FakeProcs {
            processes: vec![desktop_host_process(
                52_000,
                WIN_INSTALL_DIR,
                shadow_profile,
                1_760_000_000_001,
            )],
            listeners: vec![listener(52_000, "127.0.0.1:55670")],
        }),
        files: Arc::new(files),
        runner: Arc::new(NullRunner),
        clock: Arc::new(FixedClock(0)),
        // 测试默认「不可观测」：显式拒绝 Web home 归属（fail-closed），
        // 绝不让假进程源落到真实系统 PID 上。需要 Web 归属的用例自行注入假观测器。
        environments: std::sync::Arc::new(
            multi_agents_manager_lib::services::dsh_control::model::TestUnobservableEnvironments,
        ),
    };

    let host = discover(&env).expect("反斜杠形态的自定义 home 也必须被发现");
    assert_eq!(host.profile_dir, PathBuf::from(shadow_profile));
}

/// 反面：以 `profiles/web` 结尾的 argv **不是** desktop profile 特征——
/// 松绑 `.dsh` 字面量不得把任何 profile 名字都放进来。
#[test]
fn other_profile_names_are_not_desktop_hosts() {
    let mut files = seeded_win_files();
    let web_profile = "/Users/jarvis/.dsh/profiles/web";
    files.add_dir(join_lexical("/Users/jarvis/.dsh", &["profiles", "web"]));
    files.add_file(join_lexical(web_profile, &["package.json"]));
    let env = DshEnvironment {
        home: PathBuf::from(WIN_HOME),
        tuvis_home: join_lexical(WIN_HOME, &["tuvis-shadow"]),
        dsh_state_dir: None,
        processes: Arc::new(FakeProcs {
            processes: vec![desktop_host_process(
                53_000,
                WIN_INSTALL_DIR,
                web_profile,
                1_760_000_000_002,
            )],
            listeners: vec![listener(53_000, "127.0.0.1:39417")],
        }),
        files: Arc::new(files),
        runner: Arc::new(NullRunner),
        clock: Arc::new(FixedClock(0)),
        // 测试默认「不可观测」：显式拒绝 Web home 归属（fail-closed），
        // 绝不让假进程源落到真实系统 PID 上。需要 Web 归属的用例自行注入假观测器。
        environments: std::sync::Arc::new(
            multi_agents_manager_lib::services::dsh_control::model::TestUnobservableEnvironments,
        ),
    };
    // 安装与 profile 都在（env.home 下的 desktop profile）→ 唯一缺的是合格宿主。
    assert_eq!(discover(&env).unwrap_err().code, codes::HOST_NOT_RUNNING);
}

/// 监听属主核验保持原样：自定义 home 的宿主若没有自己名下的回环监听，
/// 仍必须是 `DSH_HOST_LISTENER_MISSING`（松绑 profile 判定不放松这条）。
#[test]
fn custom_home_still_requires_owned_listener() {
    let shadow_home = "/Users/jarvis/tuvis-dsh-shadow/home-shadow";
    let shadow_profile = format!("{shadow_home}/profiles/desktop");
    let mut files = seeded_win_files();
    files.add_dir(join_lexical(shadow_home, &["profiles", "desktop"]));
    files.add_file(join_lexical(&shadow_profile, &["package.json"]));
    let env = DshEnvironment {
        home: PathBuf::from(WIN_HOME),
        tuvis_home: join_lexical(WIN_HOME, &["tuvis-shadow"]),
        dsh_state_dir: None,
        processes: Arc::new(FakeProcs {
            processes: vec![desktop_host_process(
                54_000,
                WIN_INSTALL_DIR,
                &shadow_profile,
                1_760_000_000_003,
            )],
            // 监听属主是别的进程（假监听）。
            listeners: vec![listener(999, "127.0.0.1:51583")],
        }),
        files: Arc::new(files),
        runner: Arc::new(NullRunner),
        clock: Arc::new(FixedClock(0)),
        // 测试默认「不可观测」：显式拒绝 Web home 归属（fail-closed），
        // 绝不让假进程源落到真实系统 PID 上。需要 Web 归属的用例自行注入假观测器。
        environments: std::sync::Arc::new(
            multi_agents_manager_lib::services::dsh_control::model::TestUnobservableEnvironments,
        ),
    };
    assert_eq!(
        discover(&env).unwrap_err().code,
        codes::HOST_LISTENER_MISSING,
        "监听属主核验不得因 profile 判定松绑而放松"
    );
}

#[test]
fn h04_reused_port_does_not_reuse_identity() {
    let first = discover(&running_host_env(100, 1000, seeded_win_files())).unwrap();
    let profile_b = r"D:\其他用户\.dsh\profiles\desktop";
    let mut files = seeded_win_files();
    files.add_dir(PathBuf::from(profile_b));
    files.add_file(join_lexical(profile_b, &["package.json"]));
    let env_b = test_env(
        Path::new(WIN_HOME),
        FakeProcs {
            processes: vec![desktop_host_process(200, WIN_INSTALL_DIR, profile_b, 2000)],
            listeners: vec![listener(200, "127.0.0.1:5214")],
        },
        files,
    );
    let second = discover(&env_b).unwrap();
    assert_ne!(
        first.host_id, second.host_id,
        "不同 profile 不得因端口复用而共享身份"
    );
}
