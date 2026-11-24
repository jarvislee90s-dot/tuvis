//! T05：DSH 本机管理 IPC 命令级行为测试（§2.4 九命令的可测核心 + 注册契约）。
//!
//! 与 dsh_install_test.rs 同一套边界注入（假进程源 / 假文件源 / 假 CLI runner /
//! 假绑定撤销 / 真实 tempdir 作 `TUVIS_HOME`），但观测面是**命令层**：核心函数
//! 接收注入的 [`DshPluginManager`]（生产壳层才经 OnceLock 单例），断言固定 IPC
//! 面的受理/回执/错误串形态。自动适配开关走 `support::setup()` 重定向的全局
//! 设置库，绝不触真实 `~/.tuvis`（计划全局约束）。

mod support;

#[path = "support/dsh_management.rs"]
mod dsh_management;
use dsh_management::{desktop_host_process, invalid_profile_name, outcome, wait_phase};

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use multi_agents_manager_lib::commands::dsh as dsh_cmds;
use multi_agents_manager_lib::services::dsh_control::auth::DshAuthService;
use multi_agents_manager_lib::services::dsh_control::error::{codes, DshError};
use multi_agents_manager_lib::services::dsh_control::install::{
    embedded_manifest, BindingRevoker, DshPluginManager, PACKAGE_NAME,
};
use multi_agents_manager_lib::services::dsh_control::model::{
    ActivationState, CommandRequest, DshClock, DshCommandRunner, DshEnvironment, DshFileSource,
    DshListener, DshProcess, DshProcessSource, InstallationState, ManagementKind, ManagementPhase,
    RunOutcome,
};

// ============================================================
// 夹具常量与词法工具（与 dsh_install_test 同源的 E2 实测形态）
// ============================================================

const WIN_HOME: &str = r"C:\Users\bunny\.dsh";
const WIN_PROFILE_TOKEN: &str = r"C:\Users\bunny\.dsh\profiles\desktop";
const WIN_INSTALL_DIR: &str = r"D:\Program Files\Deepseek-Harness";

#[path = "support/dsh_paths.rs"]
mod dsh_paths;
use dsh_paths::{join_lexical, lexical_parent};

// ============================================================
// 共享假文件源（内存态 + 可写：假 CLI 经同一存储模拟官方 CLI 的落盘后果）
// ============================================================

#[derive(Default)]
struct FakeFiles {
    files: BTreeMap<PathBuf, Vec<u8>>,
    dirs: BTreeSet<PathBuf>,
}

impl FakeFiles {
    fn set_file(&mut self, path: impl Into<PathBuf>, content: impl Into<Vec<u8>>) {
        self.files.insert(path.into(), content.into());
    }

    fn add_dir(&mut self, path: impl Into<PathBuf>) {
        self.dirs.insert(path.into());
    }

    fn read(&self, path: &Path) -> Option<Vec<u8>> {
        self.files.get(path).cloned()
    }

    fn remove_prefix(&mut self, prefix: &Path) {
        let prefix = prefix.to_string_lossy().to_string();
        self.files
            .retain(|p, _| !p.to_string_lossy().starts_with(&prefix));
        self.dirs
            .retain(|p| !p.to_string_lossy().starts_with(&prefix));
    }
}

struct StoreSource(Arc<Mutex<FakeFiles>>);

impl DshFileSource for StoreSource {
    fn exists(&self, path: &Path) -> bool {
        let store = self.0.lock().unwrap();
        store.files.contains_key(path) || store.dirs.contains(path)
    }

    fn list_dir(&self, path: &Path) -> Vec<PathBuf> {
        let store = self.0.lock().unwrap();
        let dir = path
            .to_string_lossy()
            .trim_end_matches(['/', '\\'])
            .to_string();
        let mut out: Vec<PathBuf> = store
            .files
            .keys()
            .chain(store.dirs.iter())
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
        let store = self.0.lock().unwrap();
        match store.files.get(path) {
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

// ============================================================
// 假进程源（宿主在/不在可切换）
// ============================================================

struct SharedProcs {
    processes: Mutex<Vec<DshProcess>>,
    listeners: Mutex<Vec<DshListener>>,
}

impl DshProcessSource for SharedProcs {
    fn list_processes(&self) -> Vec<DshProcess> {
        self.processes.lock().unwrap().clone()
    }

    fn listening_sockets(&self) -> Vec<DshListener> {
        self.listeners.lock().unwrap().clone()
    }
}

impl SharedProcs {
    fn set_host_running(&self, running: bool) {
        *self.processes.lock().unwrap() = if running {
            vec![desktop_host_process(
                37096,
                WIN_INSTALL_DIR,
                WIN_PROFILE_TOKEN,
                1_760_000_000_000,
            )]
        } else {
            Vec::new()
        };
        *self.listeners.lock().unwrap() = if running {
            vec![DshListener {
                owner_pid: 37096,
                authority: "127.0.0.1:5214".into(),
            }]
        } else {
            Vec::new()
        };
    }
}

// ============================================================
// profile 种法
// ============================================================

#[derive(Clone, Copy)]
enum ProfileSeed {
    Full,
}

fn seed_win_install(files: &mut FakeFiles, install_dir: &str, profile: ProfileSeed) {
    let dir = PathBuf::from(install_dir);
    files.add_dir(dir.clone());
    files.set_file(
        join_lexical(&dir.to_string_lossy(), &["DeepSeek Harness.exe"]),
        b"mz",
    );
    files.set_file(
        join_lexical(&dir.to_string_lossy(), &["resources", "app-update.yml"]),
        b"provider: generic\n",
    );
    files.set_file(
        join_lexical(
            &dir.to_string_lossy(),
            &["resources", "runtime", "cli", "bin", "dsh.cmd"],
        ),
        b"@echo off\r\n",
    );
    files.add_dir(join_lexical(WIN_HOME, &["profiles", "desktop"]));
    let ProfileSeed::Full = profile;
    files.set_file(
        join_lexical(WIN_PROFILE_TOKEN, &["package.json"]),
        br#"{"name":"dsh-profile-desktop","dsh":{"profile":{"bundles":[]}},"dependencies":{}}"#
            .to_vec(),
    );
}

// ============================================================
// 假官方 CLI（与 dsh_install_test 同款：add/list/remove 的可观测后果）
// ============================================================

#[derive(Debug, Clone)]
struct CliCall {
    program: PathBuf,
    args: Vec<String>,
    env: Vec<(String, String)>,
}

impl CliCall {
    fn destructive(&self) -> bool {
        self.args.iter().any(|a| {
            let low = a.to_lowercase();
            low.contains("kill") || low == "stop-process"
        })
    }

    fn env_value(&self, key: &str) -> Option<&str> {
        self.env
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

struct FakeCli {
    store: Arc<Mutex<FakeFiles>>,
    calls: Mutex<Vec<CliCall>>,
    kill_requests: AtomicU32,
    /// 「假成功」注入：add exit 0 但什么都不写——命令层必须看到核验失败。
    silent_add: AtomicBool,
}

/// 官方 CLI 的 profile 名拒绝语义（`resolveProfileDir`：拒绝路径分隔符）。
impl DshCommandRunner for FakeCli {
    fn run(&self, request: &CommandRequest) -> Result<RunOutcome, DshError> {
        let argv: Vec<String> = request
            .args
            .iter()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        let env: Vec<(String, String)> = request
            .env
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().to_string(),
                    v.to_string_lossy().to_string(),
                )
            })
            .collect();
        self.calls.lock().unwrap().push(CliCall {
            program: request.program.clone(),
            args: argv.clone(),
            env: env.clone(),
        });
        if argv.iter().any(|a| {
            let low = a.to_lowercase();
            low.contains("kill") || low == "stop-process"
        }) {
            self.kill_requests.fetch_add(1, Ordering::SeqCst);
        }
        if argv.len() < 4 || argv[0] != "plugin" || argv[1] != "--profile" {
            return Ok(outcome(2, "", format!("unexpected argv: {argv:?}")));
        }
        let profile_name = argv[2].clone();
        if let Some(reason) = invalid_profile_name(&profile_name) {
            return Ok(outcome(1, "", reason));
        }
        let Some(home) = env
            .iter()
            .find(|(k, _)| k == "DSH_HOME")
            .map(|(_, v)| v.clone())
        else {
            return Ok(outcome(
                1,
                "",
                "dsh: Error: profile resolution requires DSH_HOME from the invoking process",
            ));
        };
        // 官方口径：profile 目录 = `$DSH_HOME/profiles/<name>`（词法拼接以匹配
        // 夹具的原生分隔符形态）。
        let profile = join_lexical(&home, &["profiles", &profile_name]);
        match argv[3].as_str() {
            "add" => {
                if self.silent_add.load(Ordering::SeqCst) {
                    return Ok(outcome(0, "added (silent)", ""));
                }
                let Some(tgz) = argv.get(4) else {
                    return Ok(outcome(2, "", "add: missing package path argument"));
                };
                match std::fs::metadata(tgz) {
                    Ok(m) if m.is_file() && m.len() > 0 => {}
                    other => {
                        return Ok(outcome(
                            1,
                            "",
                            format!("add: package unusable: {tgz} ({other:?})"),
                        ))
                    }
                }
                if !self
                    .store
                    .lock()
                    .unwrap()
                    .files
                    .contains_key(&join_lexical(&profile.to_string_lossy(), &["package.json"]))
                {
                    return Ok(outcome(
                        1,
                        "",
                        "dsh: Error: Open DeepSeek Harness Desktop once to initialize its profile, then fully quit it before running dsh plugin --profile desktop.",
                    ));
                }
                let version = embedded_manifest()
                    .map(|m| m.plugin_version)
                    .unwrap_or_default();
                let mut store = self.store.lock().unwrap();
                simulate_add(&mut store, &profile, tgz, &version);
                Ok(outcome(0, format!("+ {PACKAGE_NAME} {version}\n"), ""))
            }
            "list" => {
                let store = self.store.lock().unwrap();
                Ok(outcome(0, simulate_list(&store, &profile), ""))
            }
            "remove" => {
                let Some(target) = argv.get(4) else {
                    return Ok(outcome(2, "", "remove: missing package name argument"));
                };
                if target != PACKAGE_NAME {
                    return Ok(outcome(
                        2,
                        "",
                        format!("remove: unexpected package {target}"),
                    ));
                }
                let mut store = self.store.lock().unwrap();
                simulate_remove(&mut store, &profile);
                Ok(outcome(0, "", ""))
            }
            other => Ok(outcome(
                2,
                "",
                format!("unexpected plugin subcommand: {other}"),
            )),
        }
    }
}

/// profile `package.json`（缺失/畸形 → 空对象）。
fn read_profile_manifest(store: &FakeFiles, profile: &Path) -> serde_json::Value {
    store
        .read(&join_lexical(&profile.to_string_lossy(), &["package.json"]))
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}))
}

fn write_profile_manifest(store: &mut FakeFiles, profile: &Path, value: &serde_json::Value) {
    store.set_file(
        join_lexical(&profile.to_string_lossy(), &["package.json"]),
        serde_json::to_vec_pretty(value).expect("profile manifest 可序列化"),
    );
}

/// `add` 的真实落盘后果（Mac 报告 §2.3）：两处 package.json 标记 + 包内容。
fn simulate_add(store: &mut FakeFiles, profile: &Path, tgz: &str, version: &str) {
    let mut manifest = read_profile_manifest(store, profile);
    manifest["dependencies"][PACKAGE_NAME] = serde_json::Value::String(format!("file:{tgz}"));
    if manifest["dsh"]["profile"]["bundles"]
        .as_array_mut()
        .is_none()
    {
        manifest["dsh"]["profile"]["bundles"] = serde_json::json!([]);
    }
    if let Some(bundles) = manifest["dsh"]["profile"]["bundles"].as_array_mut() {
        if !bundles.iter().any(|v| v.as_str() == Some(PACKAGE_NAME)) {
            bundles.push(serde_json::Value::String(PACKAGE_NAME.to_string()));
        }
    }
    write_profile_manifest(store, profile, &manifest);
    let ours = join_lexical(
        &profile.to_string_lossy(),
        &["node_modules", "@tuvis", "dsh-remote-access"],
    );
    store.add_dir(ours.clone());
    store.set_file(
        join_lexical(&ours.to_string_lossy(), &["package.json"]),
        format!(r#"{{"name":"{PACKAGE_NAME}","version":"{version}"}}"#).into_bytes(),
    );
}

/// `list` 的真实输出：无依赖 → 空 stdout + rc=0；有依赖 → 树形图例。
fn simulate_list(store: &FakeFiles, profile: &Path) -> String {
    let manifest = read_profile_manifest(store, profile);
    let deps = manifest["dependencies"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    if deps.is_empty() {
        return String::new();
    }
    let mut out = format!("{} (PRIVATE)\n", profile.to_string_lossy());
    let names: Vec<&String> = deps.keys().collect();
    for (idx, name) in names.iter().enumerate() {
        let branch = if idx + 1 == names.len() {
            "└──"
        } else {
            "├──"
        };
        let pkg_dir = join_lexical(
            &join_lexical(&profile.to_string_lossy(), &["node_modules"]).to_string_lossy(),
            &name.split('/').collect::<Vec<_>>(),
        );
        let version = store
            .read(&join_lexical(&pkg_dir.to_string_lossy(), &["package.json"]))
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| v["version"].as_str().map(str::to_string))
            .unwrap_or_else(|| "0.0.0".into());
        out.push_str(&format!("{branch} {name}@{version}\n"));
    }
    out.push_str(&format!("{} package\n", names.len()));
    out
}

/// `remove` 的真实落盘后果：撤 bundles 项、删 dependencies 项、删包目录，
/// 但留下 `node_modules/@tuvis/` 空目录。
fn simulate_remove(store: &mut FakeFiles, profile: &Path) {
    let mut manifest = read_profile_manifest(store, profile);
    if let Some(deps) = manifest["dependencies"].as_object_mut() {
        deps.remove(PACKAGE_NAME);
    }
    if let Some(bundles) = manifest["dsh"]["profile"]["bundles"].as_array_mut() {
        bundles.retain(|v| v.as_str() != Some(PACKAGE_NAME));
    }
    write_profile_manifest(store, profile, &manifest);
    let ours = join_lexical(
        &profile.to_string_lossy(),
        &["node_modules", "@tuvis", "dsh-remote-access"],
    );
    store.remove_prefix(&ours);
    store.add_dir(join_lexical(
        &profile.to_string_lossy(),
        &["node_modules", "@tuvis"],
    ));
}

// ============================================================
// 假绑定撤销与假时钟
// ============================================================

struct RecordingRevoker;

impl BindingRevoker for RecordingRevoker {
    fn revoke_profile(&self, _profile_id: &str) -> Result<(), DshError> {
        Ok(())
    }
}

struct FixedClock(i64);

impl DshClock for FixedClock {
    fn now_ms(&self) -> i64 {
        self.0
    }
}

// ============================================================
// 测试台
// ============================================================

struct Bench {
    store: Arc<Mutex<FakeFiles>>,
    procs: Arc<SharedProcs>,
    cli: Arc<FakeCli>,
    _tmp: tempfile::TempDir,
}

fn bench() -> Bench {
    let mut files = FakeFiles::default();
    seed_win_install(&mut files, WIN_INSTALL_DIR, ProfileSeed::Full);
    let store = Arc::new(Mutex::new(files));
    let procs = Arc::new(SharedProcs {
        processes: Mutex::new(Vec::new()),
        listeners: Mutex::new(Vec::new()),
    });
    let cli = Arc::new(FakeCli {
        store: store.clone(),
        calls: Mutex::new(Vec::new()),
        kill_requests: AtomicU32::new(0),
        silent_add: AtomicBool::new(false),
    });
    let tmp = tempfile::tempdir().expect("tempdir 可建");
    Bench {
        store,
        procs,
        cli,
        _tmp: tmp,
    }
}

impl Bench {
    fn env(&self) -> DshEnvironment {
        DshEnvironment {
            home: PathBuf::from(WIN_HOME),
            tuvis_home: self._tmp.path().join("tuvis-home"),
            dsh_state_dir: None,
            processes: self.procs.clone(),
            files: Arc::new(StoreSource(self.store.clone())),
            runner: self.cli.clone(),
            clock: Arc::new(FixedClock(1_760_000_000_000)),
            // 测试默认「不可观测」：显式拒绝 Web home 归属（fail-closed），
            // 绝不让假进程源落到真实系统 PID 上。需要 Web 归属的用例自行注入假观测器。
            environments: std::sync::Arc::new(multi_agents_manager_lib::services::dsh_control::model::TestUnobservableEnvironments),
        }
    }

    fn manager(&self) -> DshPluginManager {
        DshPluginManager::new(self.env(), Arc::new(RecordingRevoker))
    }

    fn set_host_running(&self, running: bool) {
        self.procs.set_host_running(running);
    }

    fn set_silent_add(&self, silent: bool) {
        self.cli.silent_add.store(silent, Ordering::SeqCst);
    }

    fn kill_requests(&self) -> u32 {
        self.cli.kill_requests.load(Ordering::SeqCst)
    }
}

/// 轮询到目标阶段（命令层观测面与 dsh_install_test 同款）。
// ============================================================
// 命令级行为（brief 指定四用例）
// ============================================================

/// 固定命令受理安装 → 作业走完 → `dsh_plugin_job` 面可查；安装后状态面
/// Installed + 无破坏性调用（管理命令不接受包名/任意路径，目标由 identity 解析）。
#[tokio::test]
async fn install_command_starts_job_via_fixed_command() {
    let b = bench();
    let mgr = b.manager();
    // 命令核心受理（RED 阶段此处返回 DSH_UNSUPPORTED —— 首个有效失败点）。
    let job_id = dsh_cmds::plugin_start_core(&mgr, ManagementKind::Install).expect("命令受理");
    assert!(job_id.starts_with("mgmt-"), "job_id 形态：{job_id}");
    let job = wait_phase(&mgr, &job_id, &[ManagementPhase::Succeeded]).await;
    assert_eq!(job.phase, ManagementPhase::Succeeded);
    assert_eq!(job.reason_code, None);
    // 命令面查询回执：同一作业、同一阶段。
    let via_cmd = dsh_cmds::plugin_job_core(&mgr, &job_id).expect("job 命令可查");
    assert_eq!(via_cmd.job_id, job_id);
    assert_eq!(via_cmd.phase, ManagementPhase::Succeeded);
    // 状态面：已安装 + autoAdaptEnabled 按调用方传入值叠加。
    let dto = dsh_cmds::remote_status_core(&mgr, true, None).expect("状态命令可查");
    assert_eq!(dto.status.installation, InstallationState::Installed);
    assert!(dto.auto_adapt_enabled, "命令层叠加的开关生效值照实返回");
    // 全程零 kill、零破坏性 argv（管理 IPC 与 T04 作业同一纪律）。
    assert_eq!(b.kill_requests(), 0);
    let calls: Vec<CliCall> = b.cli.calls.lock().unwrap().clone();
    assert!(
        calls.iter().all(|c| !c.destructive()),
        "破坏性调用：{calls:?}"
    );
    // 目标单源：CLI = 安装包自带 cli（identity 解析；命令层不接任何包名/路径入参）。
    assert!(
        calls[0].program.to_string_lossy().ends_with(
            join_lexical(
                WIN_INSTALL_DIR,
                &["resources", "runtime", "cli", "bin", "dsh.cmd"]
            )
            .to_string_lossy()
            .as_ref()
        ),
        "必须用自带 CLI：{:?}",
        calls[0].program
    );
    // R01：命令层同样把 CLI 绑定到已发现的 DSH home 与 desktop 名称。
    assert_eq!(
        calls[0].env_value("DSH_HOME"),
        Some(WIN_HOME),
        "命令路径也必须显式下发 DSH_HOME：{:?}",
        calls[0].env
    );
    assert_eq!(calls[0].args[2], "desktop", "`--profile` 必须是名称");
}

/// 「CLI exit 0」在命令面也不等于「运行中」：假成功（exit 0 未核验）→ 作业
/// Failed + 状态仍 Missing/Disabled；真实成功但宿主未运行 → Enabled 而非
/// Running（安装态与激活态分档，不把「配置已启用」写作「运行中」）。
#[tokio::test]
async fn cli_exit_zero_is_not_running() {
    // 假成功：exit 0 但 profile 无痕迹 → 核验抓住，绝不 Running/Installed。
    let b = bench();
    b.set_silent_add(true);
    let mgr = b.manager();
    let job_id = dsh_cmds::plugin_start_core(&mgr, ManagementKind::Install).expect("命令受理");
    let job = wait_phase(&mgr, &job_id, &[ManagementPhase::Failed]).await;
    assert_eq!(job.phase, ManagementPhase::Failed);
    let dto = dsh_cmds::remote_status_core(&mgr, true, None).expect("状态命令可查");
    assert_eq!(dto.status.installation, InstallationState::Missing);
    assert_ne!(
        dto.status.activation,
        ActivationState::Running,
        "exit 0 ≠ 运行中"
    );
    // 真实成功但宿主未运行：Enabled（保存选择），不得冒报 Running。
    let b2 = bench();
    let mgr2 = b2.manager();
    let job_id2 = dsh_cmds::plugin_start_core(&mgr2, ManagementKind::Install).expect("命令受理");
    wait_phase(&mgr2, &job_id2, &[ManagementPhase::Succeeded]).await;
    let dto2 = dsh_cmds::remote_status_core(&mgr2, true, None).expect("状态命令可查");
    assert_eq!(dto2.status.installation, InstallationState::Installed);
    assert_eq!(
        dto2.status.activation,
        ActivationState::Enabled,
        "宿主未在运行：Enabled ≠ Running（运行要另经宿主进程证据）"
    );
    // 同一份「exit 0 + 核验过」的安装，宿主随后运行 → 才升 Running（状态分档是
    // 活证据，不是安装动作的副产物）。
    b2.set_host_running(true);
    let dto2b = dsh_cmds::remote_status_core(&mgr2, true, None).expect("状态命令可查");
    assert_eq!(dto2b.status.activation, ActivationState::Running);
}

/// 绑定命令 T06 真实语义（注入 auth 服务；错误串 CODE: detail 形态）：
/// 无待批连接 → DSH_AUTH_NO_PENDING（绝不假成功）；未知绑定撤权 →
/// DSH_AUTH_UNKNOWN_BINDING。
#[tokio::test]
async fn binding_commands_surface_honest_errors() {
    let b = bench();
    let auth = Arc::new(DshAuthService::new(b.env()).expect("auth 服务可建（tempdir env）"));
    let err = dsh_cmds::binding_start_core(&auth).unwrap_err();
    assert!(
        err.starts_with(concat!("DSH", "_AUTH_NO_PENDING: ")),
        "无待批连接必须如实上报：{err}"
    );
    let err = dsh_cmds::binding_revoke_core(&auth, "binding-nope")
        .await
        .unwrap_err();
    assert!(
        err.starts_with(concat!("DSH", "_AUTH_UNKNOWN_BINDING: ")),
        "未知绑定撤权必须如实上报：{err}"
    );
}

/// 错误串走既有「CODE: detail」形态（前端 formatInvokeError 的命中前提）：
/// 查无此作业以 DSH_JOB_UNKNOWN 开头且带 detail。
#[tokio::test]
async fn command_errors_follow_code_detail_form() {
    let b = bench();
    let mgr = b.manager();
    let err = dsh_cmds::plugin_job_core(&mgr, "mgmt-nope").unwrap_err();
    assert!(
        err.starts_with(concat!("DSH", "_JOB_UNKNOWN: ")),
        "错误串必须 CODE: detail 形态：{err}"
    );
    let err = dsh_cmds::plugin_cancel_core(&mgr, "mgmt-nope").unwrap_err();
    assert!(err.starts_with(concat!("DSH", "_JOB_UNKNOWN: ")), "{err}");
}

/// 自动适配开关：缺省开（缺键/乱串），显式写值被尊重；命令写后状态命令的
/// 叠加字段如实反映（settings KV 持久，经 support::setup() 重定向）。
#[test]
fn auto_adapt_defaults_on_and_persists() {
    support::setup();
    assert!(dsh_cmds::current_auto_adapt(), "缺键 = 缺省开");
    // 写显式关 → 生效值关；再写开 → 开（settings KV 持久往返）。
    assert!(!dsh_cmds::set_auto_adapt_core(false).expect("写开关受理"));
    assert!(!dsh_cmds::current_auto_adapt(), "显式关被尊重");
    assert!(dsh_cmds::set_auto_adapt_core(true).expect("写开关受理"));
    assert!(dsh_cmds::current_auto_adapt(), "显式开被尊重");
}

// ============================================================
// R09 回归：授权域初始化失败 → 状态面「不可用 + 修复出口」，不拖垮应用
// ============================================================

/// 授权域损坏时：`production::init_auth_service` 返回错误（不 panic），且该原因
/// 经命令层叠加后，状态 DTO 呈现「Broken + 具体 reasonCode」——卡面据此显示
/// 不可用与修复按钮，而不是整块消失或整个应用起不来。
///
/// 观测面说明：`apply_domain_failure` 是命令壳对失败标记的**唯一**投影点，
/// 这里直接喂它同一个标记（壳层本身只是 `production_auth_state()` 的搬运，
/// 其失败分支由 `production::tests::corrupt_key_is_isolated_as_error_not_panic`
/// 覆盖）。
#[test]
fn domain_init_failure_surfaces_as_broken_with_reason() {
    let tmp = tempfile::tempdir().expect("tempdir 可建");
    let state = tmp.path().join("state");
    std::fs::create_dir_all(&state).expect("建状态目录");
    std::fs::write(state.join("authorization.key"), b"garbage").expect("写坏密钥");
    let b = bench();
    let mut env = b.env();
    env.dsh_state_dir = Some(state);
    let err = match multi_agents_manager_lib::services::dsh_control::production::init_auth_service(
        &env,
    ) {
        Ok(_) => panic!("损坏密钥必须返回错误"),
        Err(e) => e,
    };
    // 与生产壳层同款：把失败原因喂给状态叠加。
    let mgr = b.manager();
    let mut dto = dsh_cmds::remote_status_core(&mgr, true, None).expect("状态命令仍可查");
    dsh_cmds::apply_domain_failure(&mut dto, err.code);
    assert_eq!(
        dto.status.installation,
        InstallationState::Broken,
        "域不可用必须落在既有的 Broken 档（卡面共用一个修复出口）"
    );
    assert_eq!(
        dto.status.reason_code.as_deref(),
        Some(err.code),
        "具体原因码必须原样透出（前端 reasonText 按码取文案）"
    );
    assert!(dto.binding_id.is_none(), "读不到真值时不得留着绑定 id");
    assert!(dto.auto_adapt_enabled, "开关叠加不受域失败影响（独立设置）");
}

// ============================================================
// K8 回归（2026-10-29 修复批 M-3；复审 M-10 改走可达形态）：域降级原因不得覆盖
// 作业自己的原因码
// ============================================================
//
// 缺陷：`apply_domain_failure` 无条件写 `reason_code = <域码>`，把作业自己的原因
// 覆盖掉——卡面显示的是「域不可用」而不是「这次操作为什么停在这里」，用户看不到
// 自己那次操作的真实结论。
//
// 契约：域降级原因是**背景信息**，只在状态里还没有原因时补位；作业自己的原因码
// 优先（`installation = Broken` 与 `binding_id = None` 的降级语义不变）。
//
// **M-10 复审**：旧回归用手搭的 `Failed + DSH_CLI_FAILED` DTO 举证，但产线上
// `apply_domain_failure` 只在 `dsh_remote_status` 壳层调用，那里的 `operation`
// 来自 `manager.status()` 的**活跃作业**（`record.active()`——`Failed` 是终态、不再
// 活跃，因此 Failed 作业根本不会出现在这个位置），手搭形态产线发不出，守卫等于
// 未被证明可达。现在改用真实 manager 造出的**可达**形态：一个
// `WaitingForDesktopExit`（活跃，宿主在场即停在这里）作业 + 域降级叠加。
#[tokio::test]
async fn domain_failure_does_not_override_a_waiting_job_reason() {
    let b = bench();
    let mgr = b.manager();
    // 宿主在场 → 作业停在等待退出（活跃作业，reason = DSH_DESKTOP_EXIT_PENDING）。
    b.set_host_running(true);
    let job_id = dsh_cmds::plugin_start_core(&mgr, ManagementKind::Uninstall).expect("作业受理");
    let job = wait_phase(&mgr, &job_id, &[ManagementPhase::WaitingForDesktopExit]).await;
    assert_eq!(
        job.reason_code.as_deref(),
        Some(codes::DESKTOP_EXIT_PENDING),
        "前置：作业自己的原因码是等待退出"
    );

    // 与 R09 壳层同款顺序：先取真实状态，再叠加域降级。
    let mut dto = dsh_cmds::remote_status_core(&mgr, true, Some("binding-stale".into()))
        .expect("状态命令仍可查");
    assert!(
        dto.status.operation.is_some(),
        "前置：活跃等待作业必须出现在状态里（否则本回归证明不了可达性）"
    );
    dsh_cmds::apply_domain_failure(&mut dto, codes::IPC_STATE);

    assert_eq!(
        dto.status.reason_code.as_deref(),
        Some(codes::DESKTOP_EXIT_PENDING),
        "作业自己的原因码必须优先于域降级原因（M-3/M-10）：{:?}",
        dto.status.reason_code
    );
    // 降级的其余语义不变：仍是 Broken 档（修复出口）+ 不留陈旧绑定 id。
    assert_eq!(dto.status.installation, InstallationState::Broken);
    assert_eq!(dto.status.activation, ActivationState::Disabled);
    assert!(dto.binding_id.is_none());
}

/// 正面控制：作业没有自己的原因时，域降级原因照旧补位（不丢信息）。
#[test]
fn domain_failure_still_fills_in_when_no_job_reason_exists() {
    let mut dto = dsh_cmds::DshRemoteStatusDto {
        status: multi_agents_manager_lib::services::dsh_control::model::ManagementStatus {
            installation: InstallationState::Installed,
            activation:
                multi_agents_manager_lib::services::dsh_control::model::ActivationState::Enabled,
            authorization:
                multi_agents_manager_lib::services::dsh_control::model::AuthorizationState::Unbound,
            operation: None,
            desktop_version: None,
            plugin_version: None,
            reason_code: None,
        },
        auto_adapt_enabled: true,
        binding_id: None,
    };

    dsh_cmds::apply_domain_failure(&mut dto, codes::IPC_STATE);

    assert_eq!(
        dto.status.reason_code.as_deref(),
        Some(codes::IPC_STATE),
        "无作业原因时域降级原因必须补位"
    );
    assert_eq!(dto.status.installation, InstallationState::Broken);
}
