//! T04：固定包（`@tuvis/dsh-remote-access`）安装/修复/卸载后台作业行为测试。
//!
//! 全部边界注入（假进程源 / 假文件源 / 假 CLI runner / 假绑定撤销 / 真实
//! tempdir 作 `TUVIS_HOME`），不触真实进程、真实 `~/.dsh`/`~/.tuvis`、不跑
//! 真 CLI（计划全局约束）。
//!
//! **假 CLI 按官方真实契约复刻**（Mac 补探测报告 §2.1/§2.3 实测，不是当前实现的
//! 投影）：
//! * `--profile` 只接受**名称**并拒绝路径分隔符（官方 `resolveProfileDir(name,
//!   home)`）；CLI 经 **`DSH_HOME`** 定位 `<home>/profiles/<name>`——假体对路径
//!   形态或缺失 `DSH_HOME` 的调用直接报错（与真实 CLI 同款拒绝）；
//! * `add` 同时改 profile `package.json` 的**两处**（`dependencies[包名]` =
//!   `file:<tgz 绝对路径>`、`dsh.profile.bundles` 追加包名），并新建
//!   `node_modules/<包名>/package.json`；`cordis.yml` **不变**；
//! * `list` 装后输出树形图例 + `└── <包名>@<ver>`；**无依赖时 stdout 全空且
//!   rc=0**（不是失败）；
//! * `remove` 撤回 `bundles` 项、删除 `dependencies` 项、删包目录，但**留下**
//!   `node_modules/@tuvis/` 空目录（故该目录存在性不能当判据）。
//!
//! Win 夹具取值沿用 E2 取证报告的实测路径形态（非系统盘 + 空格只是数据，
//! 生产不得特判）。

#[path = "support/dsh_management.rs"]
mod dsh_management;
use dsh_management::{desktop_host_process, invalid_profile_name, outcome, wait_phase};

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use multi_agents_manager_lib::services::dsh_control::error::{codes, DshError};
use multi_agents_manager_lib::services::dsh_control::install::{
    embedded_manifest, BindingRevoker, DshPluginManager, EMBEDDED_PACKAGE_BYTES, PACKAGE_NAME,
    PROFILE_NAME,
};
use multi_agents_manager_lib::services::dsh_control::model::{
    AuthorizationState, CommandRequest, DshClock, DshCommandRunner, DshEnvironment, DshFileSource,
    DshListener, DshProcess, DshProcessSource, InstallationState, ManagementKind, ManagementPhase,
    RunOutcome,
};

// ============================================================
// 夹具常量与词法工具（与 dsh_identity_test 同源的 E2 实测形态）
// ============================================================

const WIN_HOME: &str = r"C:\Users\bunny\.dsh";
const WIN_PROFILE_TOKEN: &str = r"C:\Users\bunny\.dsh\profiles\desktop";
const WIN_INSTALL_DIR: &str = r"D:\Program Files\Deepseek-Harness";
/// 其他 bundle（第三方案例）：卸载/修复只许动自有包，它是「分毫未动」的证人。
const OTHER_BUNDLE: &str = "@deepseek-ai/example-bundle";

/// 词法层父目录（'/' 与 '\' 都算分隔；Windows 形态夹具在 POSIX 测试主机上
/// std Path 不识别 '\'，file_identity.rs 同款词法处理）。
#[path = "support/dsh_paths.rs"]
mod dsh_paths;
use dsh_paths::{join_lexical, lexical_parent};

/// 以 base 的分隔符语义拼接相对路径（保持 Windows 夹具路径的原生反斜杠形态）。
/// SHA-256 hex（缓存与 manifest 对账的独立实现，不复用被测代码）。
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

// ============================================================
// 共享假文件源（内存态 + 可写：假 CLI 经同一存储模拟官方 CLI 的落盘后果）
// ============================================================

#[derive(Default)]
struct FakeFiles {
    /// 常规文件（read_small 可读、is_readable_regular_file 为真）。
    files: BTreeMap<PathBuf, Vec<u8>>,
    dirs: BTreeSet<PathBuf>,
    /// 符号链接（I5-r3：入口/锚的**实际目标**核验——链接 → 目标路径）。
    links: BTreeMap<PathBuf, PathBuf>,
}

impl FakeFiles {
    fn set_file(&mut self, path: impl Into<PathBuf>, content: impl Into<Vec<u8>>) {
        self.files.insert(path.into(), content.into());
    }

    fn add_dir(&mut self, path: impl Into<PathBuf>) {
        self.dirs.insert(path.into());
    }

    fn set_link(&mut self, link: impl Into<PathBuf>, target: impl Into<PathBuf>) {
        self.links.insert(link.into(), target.into());
    }

    fn read(&self, path: &Path) -> Option<Vec<u8>> {
        self.files.get(path).cloned()
    }

    /// 文件及链接的父目录在假文件系统中隐含存在。
    fn is_dir(&self, path: &Path) -> bool {
        let wanted = normalize_fake_path(&path.to_string_lossy());
        if self
            .files
            .keys()
            .chain(self.links.keys())
            .any(|entry| normalize_fake_path(&entry.to_string_lossy()) == wanted)
        {
            return false;
        }
        self.dirs
            .iter()
            .any(|dir| normalize_fake_path(&dir.to_string_lossy()) == wanted)
            || self
                .files
                .keys()
                .chain(self.dirs.iter())
                .chain(self.links.keys())
                .any(|entry| {
                    let mut parent = lexical_parent(&entry.to_string_lossy());
                    while let Some(current) = parent {
                        if normalize_fake_path(&current) == wanted {
                            return true;
                        }
                        parent = lexical_parent(&current).filter(|next| next != &current);
                    }
                    false
                })
    }

    /// 返回显式和隐含的直接子项，保留夹具原始分隔符。
    fn children(&self, path: &Path) -> Vec<PathBuf> {
        let wanted = normalize_fake_path(&path.to_string_lossy());
        let mut children = BTreeMap::new();
        for entry in self
            .files
            .keys()
            .chain(self.dirs.iter())
            .chain(self.links.keys())
        {
            let mut child = entry.to_string_lossy().into_owned();
            while let Some(parent) = lexical_parent(&child) {
                if normalize_fake_path(&parent) == wanted {
                    children
                        .entry(normalize_fake_path(&child))
                        .or_insert_with(|| PathBuf::from(&child));
                    break;
                }
                if parent == child {
                    break;
                }
                child = parent;
            }
        }
        children.into_values().collect()
    }

    /// 按字符串前缀清除（POSIX 主机上 std Path 对 '\' 夹具路径不做成分切分，
    /// `starts_with` 不可用；夹具路径全部经 join_lexical 保持同形态）。
    fn remove_prefix(&mut self, prefix: &Path) {
        let prefix = prefix.to_string_lossy().to_string();
        self.files
            .retain(|p, _| !p.to_string_lossy().starts_with(&prefix));
        self.dirs
            .retain(|p| !p.to_string_lossy().starts_with(&prefix));
    }
}

/// 与假文件源的跨平台词法路径比较一致。
fn normalize_fake_path(text: &str) -> String {
    let unified = text.replace('\\', "/");
    let absolute = unified.starts_with('/');
    let mut stack: Vec<&str> = Vec::new();
    for component in unified.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            other => stack.push(other),
        }
    }
    let joined = stack.join("/");
    if absolute {
        format!("/{joined}")
    } else {
        joined
    }
}

/// DshFileSource 视图（manager 只经这个只读 trait 观察假世界）。
struct StoreSource(Arc<Mutex<FakeFiles>>);

impl DshFileSource for StoreSource {
    fn exists(&self, path: &Path) -> bool {
        let store = self.0.lock().unwrap();
        store.files.contains_key(path)
            || store.dirs.contains(path)
            || store.links.contains_key(path)
    }

    fn is_dir(&self, path: &Path) -> bool {
        self.0.lock().unwrap().is_dir(path)
    }

    /// 常规文件 = files 键（links 不是常规文件；dirs 是目录）。比较走**词法组件
    /// 归一形态**（I5-r4：声明入口经归一构造，夹具键保持原生分隔符形态——两者
    /// 必须能在组件级对上；真实文件系统天然如此，注入面在此建模同一语义）。
    fn is_readable_regular_file(&self, path: &Path) -> bool {
        let store = self.0.lock().unwrap();
        if store.files.contains_key(path) {
            return true;
        }
        let wanted = normalize_fake_path(&path.to_string_lossy());
        store
            .files
            .keys()
            .any(|k| normalize_fake_path(&k.to_string_lossy()) == wanted)
    }

    /// 符号链接目标（I5-r3 入口身份核验）。
    fn read_link_target(&self, path: &Path) -> Option<PathBuf> {
        let store = self.0.lock().unwrap();
        store.links.get(path).cloned()
    }

    fn list_dir(&self, path: &Path) -> Vec<PathBuf> {
        self.list_dir_checked(path).unwrap_or_default()
    }

    fn list_dir_checked(&self, path: &Path) -> Result<Vec<PathBuf>, DshError> {
        let store = self.0.lock().unwrap();
        if !store.is_dir(path) {
            return Err(DshError::new(
                codes::STATE_IO,
                format!("fake: not a directory: {path:?}"),
            ));
        }
        Ok(store.children(path))
    }

    fn read_small(&self, path: &Path, max_bytes: u64) -> Result<Vec<u8>, DshError> {
        let store = self.0.lock().unwrap();
        let bytes = store.files.get(path).or_else(|| {
            let wanted = normalize_fake_path(&path.to_string_lossy());
            store
                .files
                .iter()
                .find(|(key, _)| normalize_fake_path(&key.to_string_lossy()) == wanted)
                .map(|(_, bytes)| bytes)
        });
        match bytes {
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

#[test]
fn fake_checked_directory_enumerates_implicit_windows_subdirectories() {
    let mut files = FakeFiles::default();
    files.set_file(r"C:\cli\node_modules\dsh\package.json", b"{}".to_vec());
    files.set_file(r"C:\cli\node_modules\dsh\bin\dsh.js", b"entry".to_vec());
    files.set_link(r"C:\cli\node_modules\dsh\alias", r"C:\other");
    let source = StoreSource(Arc::new(Mutex::new(files)));
    let package = Path::new(r"C:\cli\node_modules\dsh");
    assert!(source.is_dir(package));
    assert_eq!(
        source.list_dir_checked(package).unwrap(),
        vec![
            PathBuf::from(r"C:\cli\node_modules\dsh\alias"),
            PathBuf::from(r"C:\cli\node_modules\dsh\bin"),
            PathBuf::from(r"C:\cli\node_modules\dsh\package.json"),
        ]
    );
    assert_eq!(
        source
            .list_dir_checked(Path::new("C:/cli/node_modules/dsh/bin"))
            .unwrap(),
        vec![PathBuf::from(r"C:\cli\node_modules\dsh\bin\dsh.js")]
    );
}

#[test]
fn fake_checked_directory_accepts_an_explicit_empty_directory() {
    let mut files = FakeFiles::default();
    files.add_dir(r"C:\cli\empty");
    let source = StoreSource(Arc::new(Mutex::new(files)));
    assert_eq!(
        source.list_dir_checked(Path::new(r"C:\cli\empty")).unwrap(),
        Vec::<PathBuf>::new()
    );
}

#[test]
fn fake_checked_directory_rejects_missing_files_and_links() {
    let mut files = FakeFiles::default();
    files.set_file(r"C:\cli\file", b"data".to_vec());
    files.set_link(r"C:\cli\link", r"C:\cli");
    let source = StoreSource(Arc::new(Mutex::new(files)));
    for path in [r"C:\cli\missing", r"C:\cli\file", r"C:\cli\link"] {
        assert!(!source.is_dir(Path::new(path)));
        assert!(source.list_dir_checked(Path::new(path)).is_err());
    }
}

// ============================================================
// 假进程源（宿主在/不在可切换；等待-退出时序由测试拨动）
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
            vec![listener(37096, "127.0.0.1:5214")]
        } else {
            Vec::new()
        };
    }
}

/// Desktop 宿主子进程 argv（Win 报告 §1.2 实测形态，与 identity 测试同源）。
fn listener(owner_pid: u32, authority: &str) -> DshListener {
    DshListener {
        owner_pid,
        authority: authority.into(),
    }
}

// ============================================================
// profile 种法（E2 报告 §1.3：profile 独占 package.json）
// ============================================================

#[derive(Clone, Copy)]
enum ProfileSeed {
    /// profile 目录 + 官方标记文件（初始化完成）。
    Full,
    /// 只有目录、缺官方标记文件（未初始化）。
    DirWithoutMarker,
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
    if let ProfileSeed::Full = profile {
        // 真实 desktop profile 的 package.json 形态（Mac 报告 §2.3：name +
        // `dsh.profile.bundles` + `dependencies`）。
        files.set_file(
            join_lexical(WIN_PROFILE_TOKEN, &["package.json"]),
            br#"{"name":"dsh-profile-desktop","dsh":{"profile":{"bundles":[]}},"dependencies":{}}"#
                .to_vec(),
        );
        // 官方每次启动重写 cordis.yml（不是用户可编辑层，也不是安装判据）——
        // 在场且**不含**包名，正是「cordis.yml 不能当判据」的证人。
        files.set_file(
            join_lexical(WIN_PROFILE_TOKEN, &["cordis.yml"]),
            b"# official root config\n[]\n".to_vec(),
        );
    }
}

/// 第三方 bundle 种法（Mac 报告 §2.3 真实形态）：profile `package.json` 的
/// `dependencies` + `dsh.profile.bundles` **两处**登记 + 包目录与 package.json。
/// 它是「安装/卸载只许动自有包」的证人（bundles 顺序与依赖来源都要原样保留）。
fn seed_other_bundle(files: &mut FakeFiles) {
    let profile = PathBuf::from(WIN_PROFILE_TOKEN);
    let mut manifest = read_profile_manifest(files, &profile);
    manifest["dependencies"][OTHER_BUNDLE] = serde_json::Value::String("2.3.4".into());
    manifest["dsh"]["profile"]["bundles"] = serde_json::json!([OTHER_BUNDLE]);
    write_profile_manifest(files, &profile, &manifest);
    let other = join_lexical(WIN_PROFILE_TOKEN, &["node_modules", OTHER_BUNDLE]);
    files.add_dir(other.clone());
    files.set_file(
        join_lexical(&other.to_string_lossy(), &["package.json"]),
        br#"{"name":"@deepseek-ai/example-bundle","version":"2.3.4"}"#.to_vec(),
    );
}

// ============================================================
// 假官方 CLI（runner 注入体）：模拟 add/list/remove 的可观测后果
// ============================================================

#[derive(Debug, Clone)]
struct CliCall {
    program: PathBuf,
    args: Vec<String>,
    /// 显式环境覆盖（R01 契约：DSH_HOME 必须随每次调用下发）。
    env: Vec<(String, String)>,
}

impl CliCall {
    fn destructive(&self) -> bool {
        self.args.iter().any(|a| {
            let low = a.to_lowercase();
            low.contains("kill") || low == "stop-process"
        })
    }

    /// 取环境覆盖值（键名精确匹配；未下发 → None）。
    fn env_value(&self, key: &str) -> Option<&str> {
        self.env
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailStep {
    Add,
    List,
    Remove,
}

struct FakeCli {
    store: Arc<Mutex<FakeFiles>>,
    calls: Mutex<Vec<CliCall>>,
    kill_requests: AtomicU32,
    fail_on: Mutex<Option<(FailStep, String)>>,
    /// 「假成功」注入：add exit 0 但什么都不写——核验必须抓住。
    silent_add: AtomicBool,
}

/// 官方 CLI 的「profile 名合法性」拒绝（`resolveProfileDir`：拒绝路径分隔符与
/// `.`/`..`/`node_modules`）。假体必须**同样拒绝**，否则错误契约会被绿灯掩盖。
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
        let fail = self.fail_on.lock().unwrap().clone();
        let injected = |step: FailStep| -> Option<String> {
            fail.iter()
                .filter(|(s, _)| *s == step)
                .map(|(_, m)| m.clone())
                .next()
        };
        // argv 契约：plugin --profile <名称> <op> [...]（独立 args，无 shell）。
        if argv.len() < 4 || argv[0] != "plugin" || argv[1] != "--profile" {
            return Ok(outcome(2, "", format!("unexpected argv: {argv:?}")));
        }
        // 官方口径：`--profile` 之后必须是名称（路径形态一律拒绝）。
        let profile_name = argv[2].clone();
        if let Some(reason) = invalid_profile_name(&profile_name) {
            return Ok(outcome(1, "", reason));
        }
        // 官方口径：profile 目录 = `$DSH_HOME/profiles/<name>`；DSH_HOME 必须由
        // 调用方显式下发（父进程环境不可依赖）。
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
        // 夹具的原生分隔符形态：POSIX 主机上 std Path 不识别 '\'）。
        let profile = join_lexical(&home, &["profiles", &profile_name]);
        match argv[3].as_str() {
            "add" => {
                if let Some(msg) = injected(FailStep::Add) {
                    return Ok(outcome(1, "", msg));
                }
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
                    // 真实 CLI 的 requireDesktopProfile：profile 未初始化即报错。
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
                if let Some(msg) = injected(FailStep::List) {
                    return Ok(outcome(1, "", msg));
                }
                let store = self.store.lock().unwrap();
                Ok(outcome(0, simulate_list(&store, &profile), ""))
            }
            "remove" => {
                if let Some(msg) = injected(FailStep::Remove) {
                    return Ok(outcome(1, "", msg));
                }
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

/// profile `package.json` 读成 JSON 对象（缺失/畸形 → 空对象，与官方宽容读同款）。
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

/// `add` 的真实落盘后果（Mac 报告 §2.3 逐项复刻）：
/// `dependencies[包名] = "file:<tgz 绝对路径>"` + `dsh.profile.bundles` 追加包名
/// + 新建 `node_modules/<包名>/package.json`；`cordis.yml` 不变。
fn simulate_add(store: &mut FakeFiles, profile: &Path, tgz: &str, version: &str) {
    let mut manifest = read_profile_manifest(store, profile);
    manifest["dependencies"][PACKAGE_NAME] = serde_json::Value::String(format!("file:{tgz}"));
    // `dsh.profile.bundles` 追加（不重复）。
    let bundles = manifest["dsh"]["profile"]["bundles"]
        .as_array_mut()
        .map(|a| {
            if !a.iter().any(|v| v.as_str() == Some(PACKAGE_NAME)) {
                a.push(serde_json::Value::String(PACKAGE_NAME.to_string()));
            }
        });
    if bundles.is_none() {
        manifest["dsh"]["profile"]["bundles"] = serde_json::json!([PACKAGE_NAME]);
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

/// `list` 的真实输出（Mac 报告 §2.3）：**无依赖 → stdout 全空且 rc=0**；
/// 有依赖 → 树形图例 + `└── <包名>@<ver>`。
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
            &split_package(name),
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

/// 包名 → `node_modules` 下的相对段（`@scope/name` → `["@scope", "name"]`）。
fn split_package(name: &str) -> Vec<&str> {
    name.split('/').collect()
}

/// `remove` 的真实落盘后果（Mac 报告 §2.3）：`dependencies` 项删除、`bundles`
/// 追加项撤回、包目录消失，但 `node_modules/@tuvis/` **留空目录**。
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
    // 官方行为：作用域目录本身留下（空目录）——判据不得依赖它。
    store.add_dir(join_lexical(
        &profile.to_string_lossy(),
        &["node_modules", "@tuvis"],
    ));
}

// ============================================================
// 假绑定撤销（显式空存储 + 调用记录）与假时钟
// ============================================================

struct RecordingRevoker {
    calls: Mutex<Vec<String>>,
    fail: AtomicBool,
}

impl BindingRevoker for RecordingRevoker {
    fn revoke_profile(&self, profile_id: &str) -> Result<(), DshError> {
        self.calls.lock().unwrap().push(profile_id.to_string());
        if self.fail.load(Ordering::SeqCst) {
            return Err(DshError::new(
                codes::REVOKE_FAILED,
                "injected revoke failure",
            ));
        }
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
// 测试台（进程探针 = 测试边界观测：宿主拨动 + CLI 记录 + kill 计数）
// ============================================================

struct ProcessProbe {
    procs: Arc<SharedProcs>,
    cli: Arc<FakeCli>,
}

impl ProcessProbe {
    /// 破坏性请求计数（brief 断言面：`process_probe.kill_requests == 0`）。
    fn kill_requests(&self) -> u32 {
        self.cli.kill_requests.load(Ordering::SeqCst)
    }

    fn set_host_running(&self, running: bool) {
        self.procs.set_host_running(running);
    }

    fn calls(&self) -> Vec<CliCall> {
        self.cli.calls.lock().unwrap().clone()
    }

    fn set_fail(&self, step: FailStep, stderr: &str) {
        *self.cli.fail_on.lock().unwrap() = Some((step, stderr.to_string()));
    }

    fn set_silent_add(&self, silent: bool) {
        self.cli.silent_add.store(silent, Ordering::SeqCst);
    }
}

#[derive(Clone, Copy)]
enum Seed {
    /// 完整官方安装 + 初始化 profile。
    FullDesktop,
    /// 有安装、profile 缺官方标记（未初始化）。
    ProfileWithoutMarker,
    /// 无任何 Desktop 安装。
    NoDesktop,
}

struct Bench {
    store: Arc<Mutex<FakeFiles>>,
    probe: ProcessProbe,
    revoker: Arc<RecordingRevoker>,
    tuvis: PathBuf,
    _tmp: tempfile::TempDir,
}

fn bench(seed: Seed, host_running: bool) -> Bench {
    let mut files = FakeFiles::default();
    match seed {
        Seed::FullDesktop => seed_win_install(&mut files, WIN_INSTALL_DIR, ProfileSeed::Full),
        Seed::ProfileWithoutMarker => {
            seed_win_install(&mut files, WIN_INSTALL_DIR, ProfileSeed::DirWithoutMarker)
        }
        Seed::NoDesktop => {}
    }
    let store = Arc::new(Mutex::new(files));
    let procs = Arc::new(SharedProcs {
        processes: Mutex::new(Vec::new()),
        listeners: Mutex::new(Vec::new()),
    });
    procs.set_host_running(host_running);
    let cli = Arc::new(FakeCli {
        store: store.clone(),
        calls: Mutex::new(Vec::new()),
        kill_requests: AtomicU32::new(0),
        fail_on: Mutex::new(None),
        silent_add: AtomicBool::new(false),
    });
    let revoker = Arc::new(RecordingRevoker {
        calls: Mutex::new(Vec::new()),
        fail: AtomicBool::new(false),
    });
    let tmp = tempfile::tempdir().expect("tempdir 可建");
    let tuvis = tmp.path().join("tuvis-home");
    Bench {
        store,
        probe: ProcessProbe { procs, cli },
        revoker,
        tuvis,
        _tmp: tmp,
    }
}

impl Bench {
    /// 组装注入 env（每次调用新建；测试把值交给 manager 构造器）。
    fn env(&self) -> DshEnvironment {
        DshEnvironment {
            home: PathBuf::from(WIN_HOME),
            tuvis_home: self.tuvis.clone(),
            dsh_state_dir: None,
            processes: self.probe.procs.clone(),
            files: Arc::new(StoreSource(self.store.clone())),
            runner: self.probe.cli.clone(),
            clock: Arc::new(FixedClock(1_760_000_000_000)),
            // 测试默认「不可观测」：显式拒绝 Web home 归属（fail-closed），
            // 绝不让假进程源落到真实系统 PID 上。需要 Web 归属的用例自行注入假观测器。
            environments: std::sync::Arc::new(multi_agents_manager_lib::services::dsh_control::model::TestUnobservableEnvironments),
        }
    }

    fn manager(&self) -> DshPluginManager {
        DshPluginManager::new(self.env(), self.revoker.clone())
    }

    fn profile_str(&self) -> String {
        WIN_PROFILE_TOKEN.to_string()
    }

    /// 稳定版本化缓存路径（`<TUVIS_HOME>/dsh/plugin-cache/@tuvis-dsh-remote-access-<ver>.tgz`）
    /// ——profile 的 `file:` 依赖必须指向这里，而不是临时目录/app 内路径。
    fn cache_pkg_path(&self) -> PathBuf {
        self.tuvis.join("dsh").join("plugin-cache").join(format!(
            "@tuvis-dsh-remote-access-{}.tgz",
            embedded_manifest().unwrap().plugin_version
        ))
    }

    /// 发现到的 DSH home（`--profile` 绑定的目标；必须经 `DSH_HOME` 下发）。
    fn dsh_home(&self) -> String {
        WIN_HOME.to_string()
    }

    /// 其他 bundle 快照（profile `package.json` 中**非自有**的依赖/bundles 项 +
    /// 第三方包全部文件）——安装/卸载只许动自有包的直接证人。
    fn other_bundles_snapshot(&self) -> (String, BTreeMap<String, Vec<u8>>) {
        let store = self.store.lock().unwrap();
        let manifest = read_profile_manifest(&store, Path::new(WIN_PROFILE_TOKEN));
        let deps = manifest["dependencies"]
            .as_object()
            .map(|deps| {
                deps.iter()
                    .filter(|(k, _)| k.as_str() != PACKAGE_NAME)
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        let bundles = manifest["dsh"]["profile"]["bundles"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .filter(|name| *name != PACKAGE_NAME)
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        let other_files: BTreeMap<String, Vec<u8>> = store
            .files
            .iter()
            .filter(|(p, _)| p.to_string_lossy().contains(OTHER_BUNDLE))
            .map(|(p, b)| (p.to_string_lossy().to_string(), b.clone()))
            .collect();
        (format!("{deps}\n--bundles--\n{bundles}"), other_files)
    }

    /// 自有包目录是否在场（`node_modules/@tuvis/dsh-remote-access`——**不是**
    /// `node_modules/@tuvis/`：官方 remove 后作用域空目录残留）。
    fn store_has_package(&self) -> bool {
        let store = self.store.lock().unwrap();
        store.files.contains_key(&join_lexical(
            &join_lexical(
                &join_lexical(WIN_PROFILE_TOKEN, &["node_modules"]).to_string_lossy(),
                &["@tuvis"],
            )
            .to_string_lossy(),
            &["dsh-remote-access", "package.json"],
        ))
    }

    /// profile `package.json` 的原文（判据观测面）。
    fn profile_manifest(&self) -> serde_json::Value {
        read_profile_manifest(&self.store.lock().unwrap(), Path::new(WIN_PROFILE_TOKEN))
    }

    fn revoker_calls(&self) -> Vec<String> {
        self.revoker.calls.lock().unwrap().clone()
    }
}

/// 轮询到目标阶段（后台作业驱动的确定性观测面；超时即失败并带上最后状态）。
// ============================================================
// brief 指定四用例
// ============================================================

/// brief 用例①（Plan Review Focus #1）：首装只依赖嵌入 tgz + 自带 CLI——
/// 无插件在场、无 owner cookie、无授权即可完成；安装不撤销任何绑定；
/// 调用序列 = add + list（核验），全程无 kill、无凭据词。
#[tokio::test]
async fn first_install_needs_no_control_cookie() {
    let b = bench(Seed::FullDesktop, false);
    let mgr = b.manager();
    let job_id = mgr.start(ManagementKind::Install).expect("首装可启动");
    let job = wait_phase(&mgr, &job_id, &[ManagementPhase::Succeeded]).await;
    assert_eq!(job.phase, ManagementPhase::Succeeded, "首装成功：{job:?}");
    assert_eq!(job.reason_code, None);
    // 首装不触碰绑定撤销（§2.5 撤销序只约束卸载）。
    assert!(b.revoker_calls().is_empty(), "首装不得撤销绑定");
    // 官方 CLI 独立 args：add → list（核验面），且全程无破坏性/凭据词。
    let calls = b.probe.calls();
    assert_eq!(calls.len(), 2, "调用序列应为 add + list：{calls:?}");
    // R01：`--profile` 收的是**名称**（不是 profile 目录路径）。
    assert_eq!(
        calls[0].args,
        vec![
            "plugin".to_string(),
            "--profile".to_string(),
            PROFILE_NAME.to_string(),
            "add".to_string(),
            b.cache_pkg_path().to_string_lossy().to_string(),
        ],
        "add 的 argv 契约：profile 名称 + 稳定缓存路径（独立 args，不拼 shell）"
    );
    // R01：CLI 必须被绑定到**已发现**的 DSH home（不能靠父进程环境碰运气）。
    assert_eq!(
        calls[0].env_value("DSH_HOME"),
        Some(b.dsh_home().as_str()),
        "每次调用都必须显式下发 DSH_HOME：{:?}",
        calls[0].env
    );
    assert_eq!(calls[1].args[3], "list", "add 之后必须 list 核验");
    assert_eq!(
        calls[1].env_value("DSH_HOME"),
        Some(b.dsh_home().as_str()),
        "list 同样必须绑定同一 DSH home"
    );
    for call in &calls {
        assert!(!call.destructive(), "出现破坏性调用：{call:?}");
        assert!(
            !call.program.to_string_lossy().contains("DeepSeek Harness"),
            "不得触碰宿主可执行文件：{:?}",
            call.program
        );
        for arg in &call.args {
            let low = arg.to_lowercase();
            for banned in ["cookie", "token=", "secret", "authorization:"] {
                assert!(
                    !low.contains(banned),
                    "凭据词 {banned} 不得出现在 argv：{arg}"
                );
            }
        }
    }
    assert_eq!(b.probe.kill_requests(), 0);
    // 稳定版本化缓存：`add` 引用的路径就是它，文件在场且哈希与 manifest 一致。
    let cache = b.cache_pkg_path();
    assert_eq!(
        std::fs::read(&cache).expect("插件缓存文件必须存在"),
        EMBEDDED_PACKAGE_BYTES,
        "缓存字节 = 嵌入包字节"
    );
    assert_eq!(
        sha256_hex(&std::fs::read(&cache).unwrap()),
        embedded_manifest().unwrap().sha256,
        "缓存哈希必须与 manifest.sha256 一致"
    );
    // 账本落盘（management.json 含本作业回执）。
    let ledger = std::fs::read_to_string(b.tuvis.join("dsh").join("management.json"))
        .expect("management.json 存在");
    assert!(ledger.contains("\"jobs\""), "账本含作业表：{ledger}");
    assert!(
        ledger.contains(&job_id),
        "账本含本作业回执 {job_id}：{ledger}"
    );
    // 四态核验的真实落点：profile 两处标记 + 包目录包内容。
    let manifest = b.profile_manifest();
    assert_eq!(
        manifest["dependencies"][PACKAGE_NAME].as_str(),
        Some(format!("file:{}", cache.to_string_lossy()).as_str()),
        "profile 的 file: 依赖必须指向稳定缓存：{manifest}"
    );
    assert!(
        manifest["dsh"]["profile"]["bundles"]
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(PACKAGE_NAME))),
        "dsh.profile.bundles 必须含包名：{manifest}"
    );
    // 状态：已安装 + 无绑定（≠ 未实现，不是 blanket success）。
    let status = mgr.status().expect("状态可查");
    assert_eq!(status.installation, InstallationState::Installed);
    assert_eq!(status.authorization, AuthorizationState::Unbound);
    // 版本以嵌入 manifest 为唯一真源（本体随包字节递增；测试不复制字面量，
    // 否则每次发版都要手改测试，且改漏时测试反而在守护一个过时版本）。
    let expected_version = embedded_manifest()
        .expect("嵌入 manifest 可解析")
        .plugin_version;
    assert_eq!(
        status.plugin_version.as_deref(),
        Some(expected_version.as_str())
    );
    assert_eq!(status.operation, None, "终结作业不再占 operation");
}

/// brief 用例②：宿主在场 → 作业停 WaitingForDesktopExit（退出未确认），
/// 期间零 CLI 调用、零 kill 请求；用户退出（进程源不再报告宿主）后作业自己
/// 继续；全程不杀进程、不重启、不关应用。
#[tokio::test]
async fn running_desktop_emits_waiting_not_kill() {
    let b = bench(Seed::FullDesktop, true);
    let mgr = b.manager();
    let job_id = mgr
        .start(ManagementKind::Install)
        .expect("忙态也受理为等待作业");
    let job = wait_phase(&mgr, &job_id, &[ManagementPhase::WaitingForDesktopExit]).await;
    assert_eq!(job.phase, ManagementPhase::WaitingForDesktopExit);
    assert_eq!(
        job.reason_code.as_deref(),
        Some(codes::DESKTOP_EXIT_PENDING),
        "退出未确认有独立 reason_code"
    );
    assert!(
        b.probe.calls().is_empty(),
        "等待期不得调用 CLI：{:?}",
        b.probe.calls()
    );
    assert_eq!(b.probe.kill_requests(), 0, "等待期不得有任何 kill 请求");
    // 用户明确退出 → 作业自己继续（非 kill 路径）。
    b.probe.set_host_running(false);
    let job = wait_phase(&mgr, &job_id, &[ManagementPhase::Succeeded]).await;
    assert_eq!(job.phase, ManagementPhase::Succeeded);
    assert_eq!(b.probe.kill_requests(), 0, "全程零 kill 请求");
    let calls = b.probe.calls();
    assert!(
        calls.iter().all(|c| !c.destructive()),
        "不得出现破坏性命令：{calls:?}"
    );
    assert_eq!(calls.len(), 2, "继续后仍是 add + list：{calls:?}");
}

/// brief 用例③：卸载只动自有包；用户改过自有文件（插件缓存）→ 冲突如实上报
/// （Failed + DSH_PACKAGE_CONFLICT），用户版本原样保留、其他 bundle 分毫未动；
/// 撤绑定先于失败发生（§2.5 卸载先撤绑定，随后失败仍保持撤权）。
#[tokio::test]
async fn uninstall_keeps_user_edits() {
    let b = bench(Seed::FullDesktop, false);
    // 第三方 bundle 在场（卸载/修复只许动自有包的「分毫未动」证人）。
    seed_other_bundle(&mut b.store.lock().unwrap());
    let mgr = b.manager();
    // 先正常安装一次（产生 owned 指纹账本）。
    let install_id = mgr.start(ManagementKind::Install).expect("可安装");
    wait_phase(&mgr, &install_id, &[ManagementPhase::Succeeded]).await;
    // 用户改自有缓存文件 → user_version。
    let owned = b.cache_pkg_path();
    std::fs::write(&owned, b"user_version").expect("可写用户版本");
    let before = b.other_bundles_snapshot();
    // 卸载 → 冲突如实可见，不覆盖用户版本。
    let job_id = mgr.start(ManagementKind::Uninstall).expect("卸载可启动");
    let job = wait_phase(&mgr, &job_id, &[ManagementPhase::Failed]).await;
    assert_eq!(job.phase, ManagementPhase::Failed, "{job:?}");
    assert_eq!(
        job.reason_code.as_deref(),
        Some(codes::PACKAGE_CONFLICT),
        "用户改动冲突必须可见"
    );
    // 撤绑定先于失败（失败仍保持撤权）。
    assert_eq!(b.revoker_calls(), vec![b.profile_str()], "卸载先撤绑定一次");
    // changed_owned_file == user_version（绝不回灌/覆盖）。
    assert_eq!(
        std::fs::read(&owned).unwrap(),
        b"user_version",
        "自有文件的用户版本必须原样保留"
    );
    // after.other_bundles == before.other_bundles。
    let after = b.other_bundles_snapshot();
    assert_eq!(after, before, "其他 bundle 分毫未动");
    // 状态面同见冲突。
    let status = mgr.status().expect("状态可查");
    assert_eq!(status.installation, InstallationState::Conflict);
    assert_eq!(status.reason_code.as_deref(), Some(codes::PACKAGE_CONFLICT));
}

/// brief 用例④（验收②）：任务锁按 profile、不按窗口——活跃作业期间重复
/// start（同类双击或异类连点）都复用同一作业，绝不并行；取消后报 Cancelled。
#[tokio::test]
async fn manager_lock_serializes_windows() {
    let b = bench(Seed::FullDesktop, true);
    let mgr = b.manager();
    let first = mgr.start(ManagementKind::Install).expect("首个作业可启动");
    wait_phase(&mgr, &first, &[ManagementPhase::WaitingForDesktopExit]).await;
    // 双击（同类）→ 同一作业。
    let second = mgr.start(ManagementKind::Install).expect("复用不报错");
    assert_eq!(first, second, "双击必须复用同一作业");
    // 异类连点 → 仍是同一个 profile 锁，不并行。
    let third = mgr.start(ManagementKind::Uninstall).expect("复用不报错");
    assert_eq!(first, third, "活跃作业期间不创建并行作业");
    // 全局可见的活跃作业只有一个。
    let status = mgr.status().expect("状态可查");
    assert_eq!(
        status.operation.map(|j| j.job_id),
        Some(first.clone()),
        "operation 只承载这一个作业"
    );
    // 收尾：取消 → Cancelled（绝不 Succeeded）。
    let cancelled = mgr.cancel(&first).expect("取消可受理");
    assert_eq!(cancelled.job_id, first);
    let job = wait_phase(&mgr, &first, &[ManagementPhase::Cancelled]).await;
    assert_eq!(job.phase, ManagementPhase::Cancelled);
}

// ============================================================
// 验收补强用例
// ============================================================

/// 验收④⑤：干净卸载——只删自有包；remove 带 `@tuvis/dsh-remote-access` 独立
/// 参数；先撤绑定一次；其他 bundle 原样；卸载后状态如实回落 Missing。
#[tokio::test]
async fn uninstall_removes_only_fixed_package() {
    let b = bench(Seed::FullDesktop, false);
    seed_other_bundle(&mut b.store.lock().unwrap());
    let mgr = b.manager();
    let install_id = mgr.start(ManagementKind::Install).expect("可安装");
    wait_phase(&mgr, &install_id, &[ManagementPhase::Succeeded]).await;
    assert!(b.store_has_package(), "安装后插件在场（模拟面）");
    let before = b.other_bundles_snapshot();
    let job_id = mgr.start(ManagementKind::Uninstall).expect("卸载可启动");
    let job = wait_phase(&mgr, &job_id, &[ManagementPhase::Succeeded]).await;
    assert_eq!(job.phase, ManagementPhase::Succeeded, "{job:?}");
    assert_eq!(job.reason_code, None);
    // 调用序列：add + list（安装）→ remove + list（卸载核验）。
    let calls = b.probe.calls();
    assert_eq!(calls.len(), 4, "remove 后必须 list 核验：{calls:?}");
    assert_eq!(calls[2].args[3], "remove");
    assert_eq!(calls[2].args[4], PACKAGE_NAME, "remove 只认固定包名");
    assert_eq!(calls[2].args[2], PROFILE_NAME, "remove 也传 profile 名称");
    assert_eq!(calls[3].args[3], "list");
    assert_eq!(
        calls[3].env_value("DSH_HOME"),
        Some(b.dsh_home().as_str()),
        "卸载核验的 list 同样绑定同一 DSH home"
    );
    // 先撤绑定恰好一次（首装不撤）。
    assert_eq!(b.revoker_calls(), vec![b.profile_str()]);
    assert_eq!(b.probe.kill_requests(), 0);
    // 自有包消失，其他 bundle 原样。
    assert!(!b.store_has_package(), "自有包必须被移除");
    let after = b.other_bundles_snapshot();
    assert_eq!(after, before, "其他 bundle 分毫未动");
    // 卸载核验 ②③④ 的否定面：两处标记都撤了（bundles + dependencies），
    // 只剩 `node_modules/@tuvis/` 空目录（官方行为，不能当判据）。
    let manifest = b.profile_manifest();
    assert!(
        manifest["dependencies"].get(PACKAGE_NAME).is_none(),
        "dependencies 项必须删除：{manifest}"
    );
    assert!(
        !manifest["dsh"]["profile"]["bundles"]
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(PACKAGE_NAME))),
        "bundles 项必须撤回：{manifest}"
    );
    assert!(
        b.store.lock().unwrap().dirs.contains(&join_lexical(
            WIN_PROFILE_TOKEN,
            &["node_modules", "@tuvis"]
        )),
        "官方 remove 会留下作用域空目录（核验不得依赖它）"
    );
    // 状态如实回落：Missing + 无版本。
    let status = mgr.status().expect("状态可查");
    assert_eq!(status.installation, InstallationState::Missing);
    assert_eq!(status.plugin_version, None);
    assert_eq!(status.authorization, AuthorizationState::Unbound);
}

// ============================================================
// R01/R02 回归（审查缺陷；契约以 Mac 补探测 §2.1/§2.3 实测为准）
// ============================================================

/// R01 回归：`--profile` 只能是名称，且每次调用都必须显式绑定 `DSH_HOME`。
/// 该组合被假 CLI 按官方 `resolveProfileDir` 语义强制——传路径或缺 DSH_HOME
/// 会直接 rc=1，作业不可能 Succeeded（旧实现正是前者）。
#[tokio::test]
async fn cli_is_bound_by_profile_name_and_dsh_home() {
    let b = bench(Seed::FullDesktop, false);
    let mgr = b.manager();
    let job_id = mgr.start(ManagementKind::Install).expect("可安装");
    let job = wait_phase(&mgr, &job_id, &[ManagementPhase::Succeeded]).await;
    assert_eq!(job.phase, ManagementPhase::Succeeded, "{job:?}");
    let calls = b.probe.calls();
    assert!(!calls.is_empty(), "必须真的调用过 CLI");
    for call in &calls {
        assert_eq!(call.args[1], "--profile");
        assert_eq!(
            call.args[2], PROFILE_NAME,
            "`--profile` 必须是名称（官方拒绝路径分隔符）：{call:?}"
        );
        assert!(
            !call.args[2].contains('/') && !call.args[2].contains('\\'),
            "profile 参数不得是路径：{}",
            call.args[2]
        );
        assert_eq!(
            call.env_value("DSH_HOME"),
            Some(b.dsh_home().as_str()),
            "每次调用都必须显式下发 DSH_HOME：{call:?}"
        );
        assert!(
            !call.args.iter().any(|a| a == &b.profile_str()),
            "profile 目录路径不得出现在 argv：{call:?}"
        );
    }
}

/// R02 回归：核验不得依赖 `cordis.yml`。官方 CLI 从不写它（每次启动还会被宿主
/// 重写成 `[]`），所以「cordis.yml 里出现包名」既不能证明装好，也不能证明没装。
#[tokio::test]
async fn verification_ignores_cordis_yml() {
    // ① cordis.yml 写了包名、但 profile 两处标记与包内容都没有 → 必须失败。
    let b = bench(Seed::FullDesktop, true);
    b.store.lock().unwrap().set_file(
        join_lexical(WIN_PROFILE_TOKEN, &["cordis.yml"]),
        format!("plugins:\n  - {PACKAGE_NAME}\n").into_bytes(),
    );
    let mgr = b.manager();
    // 先让宿主退出并走一次失败安装（silent add：exit 0 但什么都没写）。
    b.probe.set_silent_add(true);
    let job_id = mgr.start(ManagementKind::Install).expect("可启动");
    wait_phase(&mgr, &job_id, &[ManagementPhase::WaitingForDesktopExit]).await;
    b.probe.set_host_running(false);
    let job = wait_phase(&mgr, &job_id, &[ManagementPhase::Failed]).await;
    assert_eq!(
        job.reason_code.as_deref(),
        Some(codes::VERIFY_FAILED),
        "cordis.yml 有包名不得被当成已安装：{job:?}"
    );
    assert_eq!(
        mgr.status().expect("状态可查").installation,
        InstallationState::Missing,
        "状态不得因 cordis.yml 显示已安装"
    );
    // ② 真实安装后 cordis.yml 仍是原始 `[]`（CLI 不改它）——核验照样成功。
    let b2 = bench(Seed::FullDesktop, false);
    let mgr2 = b2.manager();
    let id = mgr2.start(ManagementKind::Install).expect("可安装");
    wait_phase(&mgr2, &id, &[ManagementPhase::Succeeded]).await;
    let cordis = String::from_utf8(
        b2.store
            .lock()
            .unwrap()
            .read(&join_lexical(WIN_PROFILE_TOKEN, &["cordis.yml"]))
            .unwrap(),
    )
    .unwrap();
    assert!(
        !cordis.contains(PACKAGE_NAME),
        "官方 CLI 不写 cordis.yml：{cordis}"
    );
    assert_eq!(
        mgr2.status().expect("状态可查").installation,
        InstallationState::Installed,
        "不写 cordis.yml 也必须是已安装（判据在 package.json + 包内容 + list）"
    );
}

/// R02 ④：`list` 空 stdout + rc=0 表示「无依赖」，是**正常结论**不是错误。
/// 故未安装时的核验结论必须是「未安装」（VERIFY_FAILED），而不是 CLI 失败；
/// 卸载后的空 list 则必须正常通过。
#[tokio::test]
async fn empty_list_output_is_no_dependencies_not_an_error() {
    // 安装后被我方从 profile 抹掉 → list 输出空 + rc=0 → 核验判未装。
    let b = bench(Seed::FullDesktop, false);
    let mgr = b.manager();
    // silent add 会让 profile 完全没变化，list 因此输出空。
    b.probe.set_silent_add(true);
    let job_id = mgr.start(ManagementKind::Install).expect("可启动");
    let job = wait_phase(&mgr, &job_id, &[ManagementPhase::Failed]).await;
    assert_eq!(
        job.reason_code.as_deref(),
        Some(codes::VERIFY_FAILED),
        "空 list（rc=0）是「未安装」，不是 CLI 失败：{job:?}"
    );
    // 卸载后的核验同样面对空 list：必须判成功（不是错误）。
    let b2 = bench(Seed::FullDesktop, false);
    let mgr2 = b2.manager();
    let id = mgr2.start(ManagementKind::Install).expect("可安装");
    wait_phase(&mgr2, &id, &[ManagementPhase::Succeeded]).await;
    let uninstall = mgr2.start(ManagementKind::Uninstall).expect("可卸载");
    let job = wait_phase(&mgr2, &uninstall, &[ManagementPhase::Succeeded]).await;
    assert_eq!(
        job.phase,
        ManagementPhase::Succeeded,
        "空 list 不得让卸载失败：{job:?}"
    );
}

/// 稳定版本化缓存：修复作业在缓存缺失或哈希不符时，从嵌入字节重新同步。
#[tokio::test]
async fn repair_resyncs_stable_plugin_cache() {
    let b = bench(Seed::FullDesktop, false);
    let mgr = b.manager();
    let install_id = mgr.start(ManagementKind::Install).expect("可安装");
    wait_phase(&mgr, &install_id, &[ManagementPhase::Succeeded]).await;
    let cache = b.cache_pkg_path();
    assert!(cache.exists(), "安装后缓存文件必须存在");
    // 缓存内容被改坏（哈希与 manifest 不符）→ 修复必须从嵌入字节重新同步。
    std::fs::write(&cache, b"corrupted").expect("可改坏缓存");
    let repair_id = mgr.start(ManagementKind::Repair).expect("可修复");
    let job = wait_phase(&mgr, &repair_id, &[ManagementPhase::Succeeded]).await;
    assert_eq!(
        job.phase,
        ManagementPhase::Succeeded,
        "修复必须重建缓存：{job:?}"
    );
    assert_eq!(
        sha256_hex(&std::fs::read(&cache).expect("缓存被重建")),
        embedded_manifest().unwrap().sha256,
        "哈希不符 → 重新同步为嵌入字节"
    );
    assert_eq!(std::fs::read(&cache).unwrap(), EMBEDDED_PACKAGE_BYTES);
    // 缓存缺失（例如用户删了）→ 修复从嵌入字节重建并联到 profile。
    std::fs::remove_file(&cache).expect("可删缓存");
    let repair_id = mgr.start(ManagementKind::Repair).expect("可修复");
    let job = wait_phase(&mgr, &repair_id, &[ManagementPhase::Succeeded]).await;
    assert_eq!(job.phase, ManagementPhase::Succeeded, "{job:?}");
    assert_eq!(
        sha256_hex(&std::fs::read(&cache).expect("缓存重建")),
        embedded_manifest().unwrap().sha256,
        "重建的缓存必须与 manifest 哈希一致"
    );
    // `add` 每次引用的都是这个稳定路径；profile 的 file: 依赖同样指向它。
    let calls = b.probe.calls();
    let add_calls: Vec<&CliCall> = calls.iter().filter(|c| c.args[3] == "add").collect();
    assert!(add_calls.len() >= 2, "至少两次 add：{calls:?}");
    for call in add_calls {
        assert_eq!(
            call.args[4],
            cache.to_string_lossy(),
            "add 必须从稳定缓存安装：{call:?}"
        );
    }
    assert_eq!(
        b.profile_manifest()["dependencies"][PACKAGE_NAME].as_str(),
        Some(format!("file:{}", cache.to_string_lossy()).as_str()),
        "profile 的 file: 依赖必须指向稳定缓存"
    );
}

/// 缓存落点必须是**兔维斯 state 目录**（不是临时目录、不是 app 内路径）——
/// profile 的 `file:` 依赖因此跨应用更新存活。断言只依赖 CLI 实际收到的参数，
/// 不看测试自己的路径推导。
#[tokio::test]
async fn plugin_cache_lives_under_tuvis_state_dir() {
    let b = bench(Seed::FullDesktop, false);
    let mgr = b.manager();
    let job_id = mgr.start(ManagementKind::Install).expect("可安装");
    wait_phase(&mgr, &job_id, &[ManagementPhase::Succeeded]).await;
    let add = b
        .probe
        .calls()
        .into_iter()
        .find(|c| c.args[3] == "add")
        .expect("有 add");
    let installed_from = PathBuf::from(&add.args[4]);
    assert!(
        installed_from.starts_with(&b.tuvis),
        "add 的安装源必须在 TUVIS_HOME 之下：{installed_from:?}"
    );
    let relative = installed_from
        .strip_prefix(&b.tuvis)
        .expect("必在 TUVIS_HOME 之下");
    assert_eq!(
        relative
            .iter()
            .map(|s| s.to_string_lossy().to_string())
            .collect::<Vec<_>>(),
        vec![
            "dsh".to_string(),
            "plugin-cache".to_string(),
            format!(
                "@tuvis-dsh-remote-access-{}.tgz",
                embedded_manifest().unwrap().plugin_version
            ),
        ],
        "稳定缓存的落点形态：<TUVIS_HOME>/dsh/plugin-cache/<包名>-<版本>.tgz"
    );
    // 目录名与文件名都带版本/包名（可并存多版本、可辨识所有权）。
    assert!(
        !relative.iter().any(|s| s == "packages"),
        "不得沿用旧的 packages/ 临时落点：{relative:?}"
    );
}

/// 原生锁占用是独立失败族：官方 CLI 报锁占用 → reason_code = DSH_NATIVE_LOCK_BUSY，
/// 与普通 CLI 失败（DSH_CLI_FAILED）可区分，绝不假成功。
#[tokio::test]
async fn native_lock_busy_is_distinct_failure() {
    let b = bench(Seed::FullDesktop, false);
    b.probe.set_fail(
        FailStep::Add,
        "Error: EBUSY: resource busy or locked; profile lock held by another process",
    );
    let mgr = b.manager();
    let job_id = mgr.start(ManagementKind::Install).expect("作业可启动");
    let job = wait_phase(&mgr, &job_id, &[ManagementPhase::Failed]).await;
    assert_eq!(job.phase, ManagementPhase::Failed);
    assert_eq!(
        job.reason_code.as_deref(),
        Some(codes::NATIVE_LOCK_BUSY),
        "锁占用必须独立成码：{job:?}"
    );
}

/// 「CLI exit 0」不是成功：add 假成功（exit 0 但什么都没写）→ list/文件态
/// 核验必须抓住，作业 Failed（DSH_VERIFY_FAILED），状态不得显示已安装。
#[tokio::test]
async fn silent_add_still_fails_verification() {
    let b = bench(Seed::FullDesktop, false);
    b.probe.set_silent_add(true);
    let mgr = b.manager();
    let job_id = mgr.start(ManagementKind::Install).expect("作业可启动");
    let job = wait_phase(&mgr, &job_id, &[ManagementPhase::Failed]).await;
    assert_eq!(job.phase, ManagementPhase::Failed, "{job:?}");
    assert_eq!(job.reason_code.as_deref(), Some(codes::VERIFY_FAILED));
    let status = mgr.status().expect("状态可查");
    assert_eq!(status.installation, InstallationState::Missing, "无假成功");
    assert_eq!(status.plugin_version, None);
}

/// 取消语义：等待中取消 → Cancelled；此后即便宿主退出，作业也绝不继续
/// （不调用 CLI、不撤销绑定、绝不 Succeeded）。
#[tokio::test]
async fn cancelled_job_never_succeeds_after_exit() {
    let b = bench(Seed::FullDesktop, true);
    let mgr = b.manager();
    let job_id = mgr.start(ManagementKind::Install).expect("作业可启动");
    wait_phase(&mgr, &job_id, &[ManagementPhase::WaitingForDesktopExit]).await;
    let cancelled = mgr.cancel(&job_id).expect("取消可受理");
    assert_eq!(cancelled.job_id, job_id);
    let job = wait_phase(&mgr, &job_id, &[ManagementPhase::Cancelled]).await;
    assert_eq!(job.phase, ManagementPhase::Cancelled);
    // 宿主之后退出：已取消的作业原地不动。
    b.probe.set_host_running(false);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let job = mgr.job(&job_id).expect("作业可查");
    assert_eq!(
        job.phase,
        ManagementPhase::Cancelled,
        "取消后不得复活：{job:?}"
    );
    assert!(b.probe.calls().is_empty(), "取消的作业不得再调用 CLI");
    assert!(b.revoker_calls().is_empty(), "安装类作业本就不撤绑定");
}

/// 「无绑定」≠「未实现」：status() 返回真实计算的状态（authorization=Unbound +
/// 安装事实分档），绝不返回 blanket success / DSH_UNSUPPORTED；未安装与
/// profile 未初始化在 start() 处明确分开报错；查无此作业独立成码。
#[test]
fn status_distinguishes_unbound_from_unimplemented() {
    let b = bench(Seed::NoDesktop, false);
    let mgr = b.manager();
    let status = mgr
        .status()
        .expect("status 必须是真实状态，不是 UNSUPPORTED");
    assert_eq!(status.authorization, AuthorizationState::Unbound);
    assert_eq!(status.installation, InstallationState::Missing);
    assert_eq!(
        status.reason_code.as_deref(),
        Some(codes::DESKTOP_NOT_INSTALLED),
        "未安装有独立码"
    );
    // 未安装直接报错（不建作业）。
    let err = mgr.start(ManagementKind::Install).unwrap_err();
    assert_eq!(err.code, codes::DESKTOP_NOT_INSTALLED);
    // profile 未初始化独立报错（brief GREEN 三态之一）。
    let b2 = bench(Seed::ProfileWithoutMarker, false);
    let mgr2 = b2.manager();
    let err = mgr2.start(ManagementKind::Repair).unwrap_err();
    assert_eq!(err.code, codes::PROFILE_NOT_INITIALIZED);
    // 查无此作业。
    let err = mgr.job("mgmt-nope").unwrap_err();
    assert_eq!(err.code, codes::JOB_UNKNOWN);
}

// ============================================================
// H05：按实例的插件安装生命周期
//
// 实例目标（`init_instance_manager` 装配内核）：`--profile` 名与 `DSH_HOME`
// 绑定到该实例的 profile/home；管理账本落 `<tuvis_home>/dsh/instances/<id>/`；
// 卸载经实例 auth 域撤权（先撤后删，范围 = 该实例全部匹配绑定）。
// ============================================================

use multi_agents_manager_lib::services::dsh_control::instances::{
    instance_id_for, HostKind, SelectedInstance,
};
use multi_agents_manager_lib::services::dsh_control::production as dsh_install_production;

/// Web 实例夹具：独立 home + profiles/web（H02 字段表形态）。
const WEB_HOME: &str = r"C:\Users\bunny\.dsh-web";
const WEB_PROFILE_TOKEN: &str = r"C:\Users\bunny\.dsh-web\profiles\web";
/// Web 宿主 PID（`dsh web` 进程形态）。
const WEB_PID: u32 = 4242;

/// Web 宿主 argv（`dsh`+`web` 令牌；H02：Web argv 不含 home；dsh 入口 = 官方布局
/// 的三层深 launcher，argv[1]）。
fn web_host_process(started_at_ms: i64) -> DshProcess {
    DshProcess {
        pid: WEB_PID,
        exe: PathBuf::from(r"C:\Node\node.exe"),
        cmdline: vec![
            r"C:\Node\node.exe".into(),
            r"C:\cli\a\b\c\dsh".into(),
            "web".into(),
        ],
        started_at_ms,
    }
}
/// 种一个已初始化的 web profile（目录 + 官方标记）。
fn seed_web_profile(files: &mut FakeFiles) {
    files.add_dir(join_lexical(WEB_HOME, &["profiles", "web"]));
    files.set_file(
        join_lexical(WEB_PROFILE_TOKEN, &["package.json"]),
        br#"{"name":"dsh-profile-web","dsh":{"profile":{"bundles":[]}},"dependencies":{}}"#
            .to_vec(),
    );
}

impl Bench {
    /// Desktop profile 自有面快照（manifest 两处标记 + cordis.yml + 插件包内容）——
    /// 「Web 侧作业不得修改 Desktop profile」的直接证人。
    fn desktop_owned_snapshot(&self) -> String {
        let store = self.store.lock().unwrap();
        let manifest = read_profile_manifest(&store, Path::new(WIN_PROFILE_TOKEN));
        let cordis = store
            .read(&join_lexical(WIN_PROFILE_TOKEN, &["cordis.yml"]))
            .map(|b| String::from_utf8_lossy(&b).to_string())
            .unwrap_or_default();
        let pkg = store.read(&join_lexical(
            &join_lexical(
                &join_lexical(WIN_PROFILE_TOKEN, &["node_modules"]).to_string_lossy(),
                &["@tuvis"],
            )
            .to_string_lossy(),
            &["dsh-remote-access", "package.json"],
        ));
        format!(
            "{}\n--cordis--\n{cordis}\n--pkg--\n{}",
            serde_json::to_string(&manifest).unwrap(),
            pkg.is_some()
        )
    }
}

/// 实例目标夹具（Windows 形态词法路径；实例 ID 与登记侧同一派生来源）。
fn desktop_target() -> SelectedInstance {
    SelectedInstance {
        instance_id: instance_id_for(HostKind::Desktop, Path::new(WIN_PROFILE_TOKEN)),
        kind: HostKind::Desktop,
        profile_dir: PathBuf::from(WIN_PROFILE_TOKEN),
        dsh_home: None,
        identity_fingerprint: "fixture".into(),
        read_evidence_fingerprint: String::new(),
    }
}

fn web_target() -> SelectedInstance {
    SelectedInstance {
        instance_id: instance_id_for(HostKind::Web, Path::new(WEB_PROFILE_TOKEN)),
        kind: HostKind::Web,
        profile_dir: PathBuf::from(WEB_PROFILE_TOKEN),
        dsh_home: None,
        identity_fingerprint: "fixture".into(),
        read_evidence_fingerprint: String::new(),
    }
}

/// 正向管理夹具：原生绝对 home 与该宿主的稳定启动环境证据。
fn proven_web_fixture(b: &Bench, started_at_ms: i64) -> (DshEnvironment, SelectedInstance) {
    use multi_agents_manager_lib::services::dsh_control::model::{
        ProcessEnvironmentEvidence, ProcessEnvironmentReader,
    };
    use multi_agents_manager_lib::services::dsh_control::process_environment::{
        AllowlistedEnvironment, EnvironmentField, NativeGeneration, ObservedProcessIdentity,
        ReadCompleteness,
    };

    struct WebEnvironment(ProcessEnvironmentEvidence);
    impl ProcessEnvironmentReader for WebEnvironment {
        fn observed_environment(&self, pid: u32) -> Option<ProcessEnvironmentEvidence> {
            (pid == WEB_PID).then(|| self.0.clone())
        }
    }

    let home = b._tmp.path().join("web-home");
    assert!(home.is_absolute());
    let profile = home.join("profiles").join("web");
    {
        let mut store = b.store.lock().unwrap();
        store.add_dir(profile.clone());
        store.set_file(
            profile.join("package.json"),
            br#"{"name":"dsh-profile-web","dsh":{"profile":{"bundles":[]}},"dependencies":{}}"#
                .to_vec(),
        );
    }
    let process = web_host_process(started_at_ms);
    let identity = ObservedProcessIdentity {
        pid: process.pid,
        started_at_ms: process.started_at_ms,
        executable: process.exe,
        native_generation: Some(NativeGeneration {
            unique_id: 1,
            pid_version: 1,
        }),
    };
    let evidence = ProcessEnvironmentEvidence {
        identity_before: identity.clone(),
        identity_after: identity,
        completeness: ReadCompleteness::Incomplete,
        environment: AllowlistedEnvironment {
            dsh_home: EnvironmentField::Value(home.to_str().unwrap().to_owned()),
            home: EnvironmentField::Unset,
        },
    };
    let mut env = b.env();
    env.environments = Arc::new(WebEnvironment(evidence));
    let target = SelectedInstance {
        instance_id: instance_id_for(HostKind::Web, &profile),
        kind: HostKind::Web,
        profile_dir: profile,
        dsh_home: Some(home.clone()),
        identity_fingerprint: "fixture".into(),
        read_evidence_fingerprint: String::new(),
    };
    (env, target)
}

/// H05-RED-4：web_install_does_not_modify_desktop_profile ——
/// Web 实例安装作业只写该实例 profile；Desktop profile（Electron 所有）不被
/// Web 侧 CLI 修改；影子安装互不污染（反向：Desktop 作业不碰 Web profile）。
#[tokio::test]
async fn web_install_does_not_modify_desktop_profile() {
    let b = bench(Seed::FullDesktop, false);
    let (web_env, web_target) = proven_web_fixture(&b, 1_760_045_000_000);
    let web_home = web_target.profile_dir.parent().unwrap().parent().unwrap();
    // Web 宿主自身的 CLI（argv 入口）+ 同安装树锚（I5-r2：经核验才采信）。
    // CLI shim：exec 行的被调用目标 = 包声明的入口（I5-r4②：内容包含不算，exec
    // 对应关系才算）；锚带 bin 声明 + 声明入口文件实际在场。
    // 官方布局：launcher 三层深（basedir `../../..` 上溯到安装根 C:\cli）。
    let launcher = r"C:\cli\a\b\c\dsh";
    b.store.lock().unwrap().set_file(
        PathBuf::from(launcher),
        br#"#!/bin/sh
basedir=$(CDPATH= cd -- "$(dirname -- "$0")/../../.." && pwd -P)
exec "$basedir/node" "$basedir/node_modules/@deepseek-ai/dsh/bin/dsh.js" "$@"
"#
        .to_vec(),
    );
    b.store.lock().unwrap().set_file(
        join_lexical(
            r"C:\cli",
            &["node_modules", "@deepseek-ai", "dsh", "package.json"],
        ),
        br#"{"name":"@deepseek-ai/dsh","bin":{"dsh":"bin/dsh.js"}}"#.to_vec(),
    );
    b.store.lock().unwrap().set_file(
        join_lexical(
            r"C:\cli",
            &["node_modules", "@deepseek-ai", "dsh", "bin", "dsh.js"],
        ),
        b"#!/usr/bin/env node\n".to_vec(),
    );
    let desktop_before = b.desktop_owned_snapshot();

    // 活 web 宿主（启动时锚定 CLI 源的 argv 来源）；退出后作业继续（web 形态活体门）。
    b.probe
        .procs
        .processes
        .lock()
        .unwrap()
        .push(web_host_process(1_760_045_000_000));
    b.probe.procs.listeners.lock().unwrap().push(DshListener {
        owner_pid: WEB_PID,
        authority: "127.0.0.1:5216".into(),
    });

    // Web 实例安装作业：宿主在场 → 等待退出（CLI 已锚定）；退出 → 继续。
    let web_mgr =
        dsh_install_production::init_instance_manager(&web_env, &web_target, b.revoker.clone());
    let job_id = web_mgr
        .start(ManagementKind::Install)
        .expect("web 安装可启动");
    let job = wait_phase(&web_mgr, &job_id, &[ManagementPhase::WaitingForDesktopExit]).await;
    assert_eq!(job.phase, ManagementPhase::WaitingForDesktopExit);
    b.probe
        .procs
        .processes
        .lock()
        .unwrap()
        .retain(|p| p.pid != WEB_PID);
    b.probe
        .procs
        .listeners
        .lock()
        .unwrap()
        .retain(|l| l.owner_pid != WEB_PID);
    let job = wait_phase(&web_mgr, &job_id, &[ManagementPhase::Succeeded]).await;
    assert_eq!(job.phase, ManagementPhase::Succeeded, "{job:?}");
    // CLI 契约：`--profile web` + `DSH_HOME` = Web 实例自己的 home（明确 home，绝不落到 Desktop 的）。
    let calls = b.probe.calls();
    assert!(!calls.is_empty(), "必须真的调用过 CLI");
    for call in &calls {
        assert_eq!(
            call.args[2], "web",
            "web 作业必须用 web profile 名：{call:?}"
        );
        assert_eq!(
            call.env_value("DSH_HOME"),
            Some(web_home.to_str().unwrap()),
            "web 作业必须显式绑定 web home：{call:?}"
        );
    }
    // Web profile 就地安装（两处标记 + 包内容）。
    let web_manifest = read_profile_manifest(&b.store.lock().unwrap(), &web_target.profile_dir);
    assert!(
        web_manifest["dependencies"].get(PACKAGE_NAME).is_some(),
        "web profile dependencies 必须含包名：{web_manifest}"
    );
    assert!(
        web_manifest["dsh"]["profile"]["bundles"]
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(PACKAGE_NAME))),
        "web profile bundles 必须含包名：{web_manifest}"
    );
    // Desktop profile（Electron 所有）分毫未动。
    let desktop_after = b.desktop_owned_snapshot();
    assert_eq!(
        desktop_after, desktop_before,
        "Desktop profile 不得被 Web 侧 CLI 修改"
    );
    assert!(!b.store_has_package(), "Desktop profile 不得出现自有包");

    // 管理账本按实例落盘（影子互不污染）。
    let web_ledger = b
        .tuvis
        .join("dsh")
        .join("instances")
        .join(&web_target.instance_id)
        .join("management.json");
    assert!(web_ledger.exists(), "web 实例账本独立落盘：{web_ledger:?}");
    assert!(
        !b.tuvis.join("dsh").join("management.json").exists(),
        "实例目标模式不得写 legacy 账本路径"
    );

    // 反向：Desktop 实例作业只写 Desktop profile；Web profile 的安装结果原样保留。
    let desktop_mgr = dsh_install_production::init_instance_manager(
        &b.env(),
        &desktop_target(),
        b.revoker.clone(),
    );
    let job2 = desktop_mgr
        .start(ManagementKind::Install)
        .expect("desktop 安装可启动");
    wait_phase(&desktop_mgr, &job2, &[ManagementPhase::Succeeded]).await;
    let calls2 = b.probe.calls();
    let desktop_calls: Vec<&CliCall> = calls2.iter().filter(|c| c.args[2] == "desktop").collect();
    assert!(
        !desktop_calls.is_empty(),
        "desktop 作业用 desktop profile 名"
    );
    for call in &desktop_calls {
        assert_eq!(
            call.env_value("DSH_HOME"),
            Some(WIN_HOME),
            "desktop 作业必须显式绑定 desktop home：{call:?}"
        );
    }
    assert!(b.store_has_package(), "Desktop profile 就地安装");
    let web_after = read_profile_manifest(&b.store.lock().unwrap(), &web_target.profile_dir);
    assert!(
        web_after["dependencies"].get(PACKAGE_NAME).is_some(),
        "Desktop 作业不得动 Web profile 的安装结果：{web_after}"
    );
    let desktop_ledger = b
        .tuvis
        .join("dsh")
        .join("instances")
        .join(desktop_target().instance_id)
        .join("management.json");
    assert!(desktop_ledger.exists(), "desktop 实例账本独立落盘");
    assert_ne!(web_ledger, desktop_ledger, "两个实例的账本互不指向");
}

/// H05 附带：按实例的就绪等待门——Desktop 宿主在场 → 该实例作业停等待退出；
/// Web 宿主的存亡不影响 Desktop 实例的门（门只盯本实例的活体）。
#[tokio::test]
async fn per_instance_exit_gate_waits_only_for_its_own_host() {
    let b = bench(Seed::FullDesktop, false);
    seed_web_profile(&mut b.store.lock().unwrap());
    // Web 宿主在场（Desktop 不在场）：Desktop 实例作业的等待门不因 web 宿主存在而等待？
    // 反过来验证：先只放 Desktop 宿主 → 等待；退出 → 继续。
    let mgr = dsh_install_production::init_instance_manager(
        &b.env(),
        &desktop_target(),
        b.revoker.clone(),
    );
    let pid = 51111u32;
    b.probe
        .procs
        .processes
        .lock()
        .unwrap()
        .push(desktop_host_process(
            pid,
            WIN_INSTALL_DIR,
            WIN_PROFILE_TOKEN,
            1_760_040_000_000,
        ));
    b.probe.procs.listeners.lock().unwrap().push(DshListener {
        owner_pid: pid,
        authority: "127.0.0.1:5217".into(),
    });
    let job_id = mgr.start(ManagementKind::Install).expect("作业可启动");
    let job = wait_phase(&mgr, &job_id, &[ManagementPhase::WaitingForDesktopExit]).await;
    assert_eq!(job.phase, ManagementPhase::WaitingForDesktopExit);
    assert!(b.probe.calls().is_empty(), "等待期零 CLI 调用");
    // 注入缺少环境归属证据的 Web 宿主，确保发现层确实报告 web_error。
    b.probe
        .procs
        .processes
        .lock()
        .unwrap()
        .push(web_host_process(1_760_045_000_000));
    b.probe.procs.listeners.lock().unwrap().push(DshListener {
        owner_pid: WEB_PID,
        authority: "127.0.0.1:5216".into(),
    });
    let discovery =
        multi_agents_manager_lib::services::dsh_control::identity::discover_detailed(&b.env())
            .unwrap();
    assert!(
        discovery.web_error.is_some(),
        "测试必须制造不可归属的 Web 事实"
    );
    // 该实例宿主退出 → 作业继续（Web 宿主存亡与此无关）。
    b.probe
        .procs
        .processes
        .lock()
        .unwrap()
        .retain(|p| p.pid != pid);
    b.probe
        .procs
        .listeners
        .lock()
        .unwrap()
        .retain(|l| l.owner_pid != pid);
    let job = wait_phase(&mgr, &job_id, &[ManagementPhase::Succeeded]).await;
    assert_eq!(job.phase, ManagementPhase::Succeeded);
    assert_eq!(b.probe.kill_requests(), 0, "等待退出路径零 kill");
}

/// H05 附带：卸载先撤**该实例全部匹配绑定**再移除包——实例 auth 域内两条
/// Bound 绑定（不同钥）一并成墓碑；legacy 账本路径零写入。
#[tokio::test]
async fn uninstall_revokes_all_instance_bindings_before_removal() {
    let b = bench(Seed::FullDesktop, false);
    let target = desktop_target();
    // 预种该实例 auth 域的账本：两条 Bound（不同钥、同一 profile）。
    let inst_state = b
        .tuvis
        .join("dsh")
        .join("instances")
        .join(&target.instance_id);
    std::fs::create_dir_all(&inst_state).expect("实例状态目录");
    let ledger = serde_json::json!({
        "bindings": [
            {
                "bindingId": "binding-k1",
                "publicKeySpki": "k1-spki-b64u",
                "revokedKeySpki": "",
                "executable": "exe",
                "pid": 1,
                "startedAtMs": 1,
                "profileDir": WIN_PROFILE_TOKEN,
                "createdAtMs": 1,
                "state": "bound",
            },
            {
                "bindingId": "binding-k2",
                "publicKeySpki": "k2-spki-b64u",
                "revokedKeySpki": "",
                "executable": "exe",
                "pid": 1,
                "startedAtMs": 1,
                "profileDir": WIN_PROFILE_TOKEN,
                "createdAtMs": 2,
                "state": "bound",
            },
        ],
    });
    std::fs::write(
        inst_state.join("bindings.json"),
        serde_json::to_vec_pretty(&ledger).expect("账本可序列化"),
    )
    .expect("预种实例账本");
    assert!(!b.store_has_package(), "前置：包未安装");
    // 先装一次（卸载要有包可移）。
    let install_mgr =
        dsh_install_production::init_instance_manager(&b.env(), &target, b.revoker.clone());
    let install_id = install_mgr.start(ManagementKind::Install).expect("可安装");
    wait_phase(&install_mgr, &install_id, &[ManagementPhase::Succeeded]).await;
    assert!(b.store_has_package(), "前置：包已安装");

    // 实例域撤权器：resolver 指向按该实例装配的 auth 域（测试注入面）。
    let svc = Arc::new(
        dsh_install_production::init_instance_auth(&b.env(), &target.instance_id)
            .expect("实例 auth 域可装配"),
    );
    let base_env = Arc::new(b.env());
    let revoker = dsh_install_production::InstanceBindingRevoker::with_resolver(
        Arc::clone(&base_env),
        target.instance_id.clone(),
        Arc::new(move || Ok(Arc::clone(&svc))),
    );
    let mgr = dsh_install_production::init_instance_manager(&b.env(), &target, Arc::new(revoker));
    let job_id = mgr.start(ManagementKind::Uninstall).expect("卸载可启动");
    let job = wait_phase(&mgr, &job_id, &[ManagementPhase::Succeeded]).await;
    assert_eq!(job.phase, ManagementPhase::Succeeded, "{job:?}");

    // 该实例账本：两条绑定全部成墓碑（信任物销毁、墓碑留存）。
    let ledger: serde_json::Value =
        serde_json::from_slice(&std::fs::read(inst_state.join("bindings.json")).expect("账本在场"))
            .expect("账本可解析");
    let rows = ledger["bindings"].as_array().expect("绑定数组");
    assert_eq!(rows.len(), 2, "两条绑定都在账本内");
    for row in rows {
        assert_eq!(row["state"], "revoked", "绑定必须被撤：{row}");
        assert_eq!(row["publicKeySpki"], "", "信任物必须销毁：{row}");
        assert_ne!(row["revokedKeySpki"], "", "墓碑必须留存被撤钥：{row}");
    }
    // 卸载序先撤后删：包已被移除。
    assert!(!b.store_has_package(), "卸载移除自有包");
    // legacy 基础账本零写入（按实例撤权不得写 legacy 路径）。
    assert!(
        !b.tuvis.join("dsh").join("bindings.json").exists(),
        "按实例撤权不得写 legacy 账本"
    );
}

// ============================================================
// H05 独立审查修复回归（I5：Web-only 安装的 CLI 源）
// ============================================================

/// I5-r2：Web-only 机器（无 Desktop 安装）的 web 实例安装——CLI 源 = **作业启动
/// 时**经核验的宿主 argv dsh 入口（词法绝对 + 文件在场 + 官方入口形态 + **同安装
/// 树锚**）；web 宿主在场 → 等待退出；退出后作业照常完成。
#[tokio::test]
async fn web_only_install_uses_host_argv_cli() {
    let b = bench(Seed::NoDesktop, false);
    let (web_env, web_target) = proven_web_fixture(&b, 1_760_050_000_000);
    let web_home = web_target.profile_dir.parent().unwrap().parent().unwrap();
    // 同安装树锚：`C:\cli` 树里有官方 dsh 包本体（没有它 = 同名非 DSH 命令，被拒）；
    // CLI 文件本身也必须在文件面（经核验绝对路径 = 存在性证据）。
    // CLI shim：exec 行的被调用目标 = 包声明的入口（I5-r4②：内容包含不算，exec
    // 对应关系才算）；锚带 bin 声明 + 声明入口文件实际在场。
    // 官方布局：launcher 三层深（basedir `../../..` 上溯到安装根 C:\cli）。
    let launcher = r"C:\cli\a\b\c\dsh";
    b.store.lock().unwrap().set_file(
        PathBuf::from(launcher),
        br#"#!/bin/sh
basedir=$(CDPATH= cd -- "$(dirname -- "$0")/../../.." && pwd -P)
exec "$basedir/node" "$basedir/node_modules/@deepseek-ai/dsh/bin/dsh.js" "$@"
"#
        .to_vec(),
    );
    b.store.lock().unwrap().set_file(
        join_lexical(
            r"C:\cli",
            &["node_modules", "@deepseek-ai", "dsh", "package.json"],
        ),
        br#"{"name":"@deepseek-ai/dsh","bin":{"dsh":"bin/dsh.js"}}"#.to_vec(),
    );
    b.store.lock().unwrap().set_file(
        join_lexical(
            r"C:\cli",
            &["node_modules", "@deepseek-ai", "dsh", "bin", "dsh.js"],
        ),
        b"#!/usr/bin/env node\n".to_vec(),
    );
    // 活 web 宿主（argv 带官方布局三层深 launcher 入口）。
    b.probe
        .procs
        .processes
        .lock()
        .unwrap()
        .push(web_host_process(1_760_050_000_000));
    b.probe.procs.listeners.lock().unwrap().push(DshListener {
        owner_pid: WEB_PID,
        authority: "127.0.0.1:5218".into(),
    });

    let mgr =
        dsh_install_production::init_instance_manager(&web_env, &web_target, b.revoker.clone());
    let job_id = mgr.start(ManagementKind::Install).expect("web 安装可启动");
    // web 形态宿主在场 → 等待退出（原始进程表门；I5 边界如实申报）。
    let job = wait_phase(&mgr, &job_id, &[ManagementPhase::WaitingForDesktopExit]).await;
    assert_eq!(job.phase, ManagementPhase::WaitingForDesktopExit);
    assert!(b.probe.calls().is_empty(), "等待期零 CLI 调用");

    // 宿主退出 → 作业继续（CLI 已在启动时锚定）。
    b.probe
        .procs
        .processes
        .lock()
        .unwrap()
        .retain(|p| p.pid != WEB_PID);
    b.probe
        .procs
        .listeners
        .lock()
        .unwrap()
        .retain(|l| l.owner_pid != WEB_PID);
    let job = wait_phase(&mgr, &job_id, &[ManagementPhase::Succeeded]).await;
    assert_eq!(job.phase, ManagementPhase::Succeeded, "{job:?}");

    let calls = b.probe.calls();
    assert!(!calls.is_empty());
    for call in &calls {
        assert_eq!(
            call.program,
            PathBuf::from(r"C:\cli\a\b\c\dsh"),
            "CLI 源 = 启动时锚定的宿主 argv dsh 入口（官方三层深布局）：{call:?}"
        );
        assert_eq!(call.args[2], "web");
        assert_eq!(call.env_value("DSH_HOME"), Some(web_home.to_str().unwrap()));
    }
    let web_manifest = read_profile_manifest(&b.store.lock().unwrap(), &web_target.profile_dir);
    assert!(web_manifest["dependencies"].get(PACKAGE_NAME).is_some());
    assert!(
        !b.store_has_package(),
        "Web-only 机器上 Desktop profile 不存在也不得被造出"
    );
}

/// I5-r2：启动时无经核验 CLI 源（无活宿主，且本机 PATH 上解析不出官方形态入口）
/// → Applying 以 `DSH_CLI_SOURCE_UNVERIFIED` 显式拒绝（绝不执行未核验来源）。
/// 不变量断言（环境无关）：本机 PATH 若恰好存在经核验入口而作业成功，则全部 CLI
/// 调用的 program 必须是词法绝对路径且官方入口形态——**裸名 `dsh` 绝不出现**
/// （生产已无 bare 回退路径）。
#[tokio::test]
async fn web_install_without_verified_cli_source_is_refused() {
    let b = bench(Seed::NoDesktop, false);
    seed_web_profile(&mut b.store.lock().unwrap());
    // 无任何宿主进程：等待门直接放行（web 活体 = 原始进程表形态，不在场）。
    let mgr =
        dsh_install_production::init_instance_manager(&b.env(), &web_target(), b.revoker.clone());
    let job_id = mgr.start(ManagementKind::Install).expect("web 安装可启动");
    // 等待任一终态（失败 = 显式拒绝；成功 = 本机 PATH 恰有经核验入口）。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let job = loop {
        let job = mgr.job(&job_id).expect("作业可查");
        if matches!(
            job.phase,
            ManagementPhase::Succeeded | ManagementPhase::Failed
        ) {
            break job;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "等待终态超时，最后作业 {job:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    let calls = b.probe.calls();
    match job.phase {
        ManagementPhase::Failed => {
            assert_eq!(
                job.reason_code.as_deref(),
                Some(codes::CLI_SOURCE_UNVERIFIED),
                "无法证明 CLI 来源必须显式拒绝：{job:?}"
            );
            assert!(calls.is_empty(), "拒绝路径零 CLI 执行");
        }
        ManagementPhase::Succeeded => {
            assert!(!calls.is_empty());
            for call in &calls {
                let program = call.program.to_string_lossy();
                let name = program
                    .rsplit(['/', '\\'])
                    .next()
                    .unwrap_or_default()
                    .to_lowercase();
                assert!(
                    matches!(name.as_str(), "dsh" | "dsh.cmd" | "dsh.exe"),
                    "CLI program 必须是官方入口形态（绝不执行同名猜测命令）：{call:?}"
                );
                assert!(
                    program.starts_with('/') || program.as_bytes().get(1) == Some(&b':'),
                    "CLI program 必须是已解析绝对路径（无 PATH bare 语义）：{call:?}"
                );
                assert_eq!(call.args[2], "web");
                assert_eq!(call.env_value("DSH_HOME"), Some(WEB_HOME));
            }
        }
        other => panic!("非终态：{other:?}"),
    }
}

// ============================================================
// H05 修复轮 2 回归（I5-r2：CLI 来源核验）
// ============================================================

use multi_agents_manager_lib::services::dsh_control::identity as dsh_identity;

/// I5-r3 两面对照①：「同名文件 + **真锚** → 仍拒」——入口身份 ≠ 邻近规则：
/// 官方 dsh 包元数据（name 字段）在场，但入口是同名**常规文件**（内容不引用官方
/// 包名）或指向**包目录之外**的链接 → 一律拒绝。
#[test]
fn web_cli_source_rejects_same_name_entry_even_with_a_real_anchor() {
    let b = bench(Seed::NoDesktop, false);
    // 真锚：官方 dsh 包元数据（name = @deepseek-ai/dsh）在 `C:\tools` 树内。
    b.store.lock().unwrap().set_file(
        join_lexical(
            r"C:\tools",
            &["node_modules", "@deepseek-ai", "dsh", "package.json"],
        ),
        br#"{"name":"@deepseek-ai/dsh","bin":{"dsh":"bin/dsh.js"}}"#.to_vec(),
    );
    let env = b.env();
    let path_stub = Some(std::ffi::OsStr::new("/usr/bin:/bin"));

    // ① 同名常规文件（内容是别的程序，不引用官方包名）→ 拒。
    b.store.lock().unwrap().set_file(
        PathBuf::from(r"C:\tools\dsh"),
        b"#!/bin/sh
exec other-program
"
        .to_vec(),
    );
    b.probe.procs.processes.lock().unwrap().push(DshProcess {
        pid: 43000,
        exe: PathBuf::from(r"C:\Node\node.exe"),
        cmdline: vec![
            r"C:\Node\node.exe".into(),
            r"C:\tools\dsh".into(),
            "web".into(),
        ],
        started_at_ms: 1_760_060_000_000,
    });
    b.probe.procs.listeners.lock().unwrap().push(DshListener {
        owner_pid: 43000,
        authority: "127.0.0.1:5220".into(),
    });
    let err = dsh_identity::web_cli_source(&env, path_stub).expect_err("同名常规文件必须被拒");
    assert_eq!(err.code, codes::CLI_SOURCE_UNVERIFIED, "{err}");

    // ② 同名链接但解析目标在包目录**之外** → 拒（链接目标必须落进官方包）。
    b.store.lock().unwrap().set_link(
        PathBuf::from(r"C:\tools\dsh"),
        PathBuf::from(r"C:\elsewhere\evil.js"),
    );
    let err = dsh_identity::web_cli_source(&env, path_stub).expect_err("指向包外的链接必须被拒");
    assert_eq!(err.code, codes::CLI_SOURCE_UNVERIFIED, "{err}");

    // ③ 同名悬空链接（目标不存在）→ 拒。
    b.store.lock().unwrap().set_link(
        PathBuf::from(r"C:\tools\dsh"),
        PathBuf::from(r"C:\tools\missing.js"),
    );
    let err = dsh_identity::web_cli_source(&env, path_stub).expect_err("悬空链接必须被拒");
    assert_eq!(err.code, codes::CLI_SOURCE_UNVERIFIED, "{err}");
}

/// I5-r3 两面对照②：「合法 wrapper → 官方包 → 采信」——入口为符号链接且解析
/// 目标落在官方包目录内（可读常规文件）；PATH 布局里排在前面的同名冒充项被跳过，
/// 官方项采信；全部不合格 / PATH 缺席 → 显式拒绝。
#[test]
fn path_fallback_accepts_wrapper_resolving_into_official_package() {
    let b = bench(Seed::NoDesktop, false);
    // 官方包（真锚 + 包内入口）+ 前置同名冒充（常规文件，内容不引用官方包名）。
    b.store.lock().unwrap().set_file(
        join_lexical(
            "/opt/dsh",
            &["node_modules", "@deepseek-ai", "dsh", "package.json"],
        ),
        br#"{"name":"@deepseek-ai/dsh","bin":{"dsh":"bin/dsh.js"}}"#.to_vec(),
    );
    b.store.lock().unwrap().set_file(
        join_lexical(
            "/opt/dsh",
            &["node_modules", "@deepseek-ai", "dsh", "bin", "dsh.js"],
        ),
        b"#!/usr/bin/env node\n".to_vec(),
    );
    b.store.lock().unwrap().set_link(
        PathBuf::from("/opt/dsh/bin/dsh"),
        PathBuf::from("/opt/dsh/node_modules/@deepseek-ai/dsh/bin/dsh.js"),
    );
    b.store.lock().unwrap().set_file(
        "/usr/local/bin/dsh",
        b"#!/bin/sh\nexec other-program\n".to_vec(),
    );

    let env = b.env();
    let cli = dsh_identity::web_cli_source(
        &env,
        Some(std::ffi::OsStr::new("/usr/local/bin:/opt/dsh/bin")),
    )
    .expect("官方 wrapper（链接 → 包内目标）必须被采信");
    assert_eq!(
        cli,
        PathBuf::from("/opt/dsh/bin/dsh"),
        "跳过同名冒充，选官方入口"
    );

    // 只有冒充项 → 拒。
    let b2 = bench(Seed::NoDesktop, false);
    b2.store.lock().unwrap().set_file(
        "/usr/local/bin/dsh",
        b"#!/bin/sh\nexec other-program\n".to_vec(),
    );
    let err = dsh_identity::web_cli_source(&b2.env(), Some(std::ffi::OsStr::new("/usr/local/bin")))
        .expect_err("无官方包配对的同名入口必须被拒");
    assert_eq!(err.code, codes::CLI_SOURCE_UNVERIFIED, "{err}");

    // PATH 缺席 → 拒。
    let err = dsh_identity::web_cli_source(&b2.env(), None).expect_err("PATH 缺席必须拒绝");
    assert_eq!(err.code, codes::CLI_SOURCE_UNVERIFIED);
}

// ============================================================
// H05 修复轮 3 回归（N3：卸载撤权先于一切 CLI 依赖）
// ============================================================

/// N3：实例域内存在 Bound 绑定 + CLI 来源不可核验（无宿主/无经核验入口）时，
/// 作业在 CLI 环节失败（包移除未发生），但绑定已全部成墓碑、凭据面已清空——
/// 「先撤后删」不因 CLI 失败而跳过。若本机 PATH 恰有经核验入口则作业成功移包；
/// 两种结局下撤权都已先行（不变量断言与结局无关）。
#[tokio::test]
async fn uninstall_revokes_bindings_even_when_cli_source_is_unverifiable() {
    let b = bench(Seed::NoDesktop, false);
    seed_web_profile(&mut b.store.lock().unwrap());
    // web profile 已装有自有包（无 CLI 来源时安装作业不可用 → 直接种安装标记）。
    {
        let mut store = b.store.lock().unwrap();
        simulate_add(
            &mut store,
            Path::new(WEB_PROFILE_TOKEN),
            "seeded-source",
            "0.0.0",
        );
    }
    let target = web_target();
    // 预种实例域账本：一条 Bound 绑定（不同钥，同 profile）。
    let inst_state = b
        .tuvis
        .join("dsh")
        .join("instances")
        .join(&target.instance_id);
    std::fs::create_dir_all(&inst_state).expect("实例状态目录");
    let ledger = serde_json::json!({
        "bindings": [
            {
                "bindingId": "binding-n3",
                "publicKeySpki": "n3-spki-b64u",
                "revokedKeySpki": "",
                "executable": "exe",
                "pid": 1,
                "startedAtMs": 1,
                "profileDir": WEB_PROFILE_TOKEN,
                "createdAtMs": 1,
                "state": "bound",
            },
        ],
    });
    std::fs::write(
        inst_state.join("bindings.json"),
        serde_json::to_vec_pretty(&ledger).expect("可序列化"),
    )
    .expect("预种实例账本");

    // 实例域 auth + 撤权 resolver（测试注入面；保留断言句柄）。
    let svc = Arc::new(
        dsh_install_production::init_instance_auth(&b.env(), &target.instance_id)
            .expect("实例 auth 域可装配"),
    );
    let svc_for_assert = Arc::clone(&svc);
    let base_env = Arc::new(b.env());
    let revoker = dsh_install_production::InstanceBindingRevoker::with_resolver(
        Arc::clone(&base_env),
        target.instance_id.clone(),
        Arc::new(move || Ok(Arc::clone(&svc))),
    );
    let mgr = dsh_install_production::init_instance_manager(&b.env(), &target, Arc::new(revoker));
    let job_id = mgr.start(ManagementKind::Uninstall).expect("卸载可启动");
    // 等终态（失败 = CLI 来源不可核验；成功 = 本机 PATH 恰有经核验入口）。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let job = loop {
        let job = mgr.job(&job_id).expect("作业可查");
        if matches!(
            job.phase,
            ManagementPhase::Succeeded | ManagementPhase::Failed
        ) {
            break job;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "等待终态超时，最后作业 {job:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };

    // 不变量（与结局无关）：撤权先行——账本零 Bound，全部成墓碑。
    let after: serde_json::Value =
        serde_json::from_slice(&std::fs::read(inst_state.join("bindings.json")).expect("账本在场"))
            .expect("账本可解析");
    for row in after["bindings"].as_array().expect("绑定数组") {
        assert_eq!(row["state"], "revoked", "绑定必须已撤：{row}");
        assert_eq!(row["publicKeySpki"], "", "信任物必须销毁：{row}");
        assert_ne!(row["revokedKeySpki"], "", "墓碑必须留存被撤钥：{row}");
    }
    assert_eq!(
        svc_for_assert.authorization_state(),
        AuthorizationState::Revoked,
        "凭据/授权面已清空（先于任何 CLI 动作）"
    );

    // 结局分支：失败 = CLI 来源不可核验且包移除未发生；成功 = 包被移除。
    match job.phase {
        ManagementPhase::Failed => {
            assert_eq!(
                job.reason_code.as_deref(),
                Some(codes::CLI_SOURCE_UNVERIFIED),
                "CLI 来源不可核验必须显式拒绝：{job:?}"
            );
            let web_manifest =
                read_profile_manifest(&b.store.lock().unwrap(), Path::new(WEB_PROFILE_TOKEN));
            assert!(
                web_manifest["dependencies"].get(PACKAGE_NAME).is_some(),
                "包移除未发生（撤权已完成、移包被拒）：{web_manifest}"
            );
        }
        ManagementPhase::Succeeded => {
            let web_manifest =
                read_profile_manifest(&b.store.lock().unwrap(), Path::new(WEB_PROFILE_TOKEN));
            assert!(
                web_manifest["dependencies"].get(PACKAGE_NAME).is_none(),
                "成功结局 = 包被移除：{web_manifest}"
            );
        }
        other => panic!("非终态：{other:?}"),
    }
}

// ============================================================
// H05 修复轮 4 回归（I5-r4：实际目标解析 / exec 对应关系）
// ============================================================

/// I5-r4① 三面对照：入口身份按**实际目标**核验——
/// ① 相对合法链接（npm 布局 `bin/dsh → ../lib/node_modules/…`）→ **采信**
///    （相对链接以链接父目录展开、包锚与入口统一解析）；
/// ② 二级外跳链接（dsh → mid → 包外真实文件）→ **拒**（表面落包内、解析后出包）；
/// ③ `..` 逃逸链接（解析后越出包目录的真实文件）→ **拒**。
#[test]
fn cli_entry_identity_resolves_actual_targets() {
    // ① 相对合法链接：`/opt/dsh/bin/dsh → ../lib/node_modules/@deepseek-ai/dsh/bin/dsh.js`，
    //    锚在 `/opt/dsh/lib/node_modules/@deepseek-ai/dsh/`（npm 全局布局）。
    let b = bench(Seed::NoDesktop, false);
    b.store.lock().unwrap().set_file(
        join_lexical(
            "/opt/dsh",
            &["lib", "node_modules", "@deepseek-ai", "dsh", "package.json"],
        ),
        br#"{"name":"@deepseek-ai/dsh","bin":{"dsh":"bin/dsh.js"}}"#.to_vec(),
    );
    b.store.lock().unwrap().set_file(
        join_lexical(
            "/opt/dsh",
            &[
                "lib",
                "node_modules",
                "@deepseek-ai",
                "dsh",
                "bin",
                "dsh.js",
            ],
        ),
        b"#!/usr/bin/env node\n".to_vec(),
    );
    b.store.lock().unwrap().set_link(
        PathBuf::from("/opt/dsh/bin/dsh"),
        PathBuf::from("../lib/node_modules/@deepseek-ai/dsh/bin/dsh.js"),
    );
    // 活 web 宿主（argv 带相对链接入口）。
    b.probe.procs.processes.lock().unwrap().push(DshProcess {
        pid: 44000,
        exe: PathBuf::from("/usr/local/bin/node"),
        cmdline: vec![
            "/usr/local/bin/node".into(),
            "/opt/dsh/bin/dsh".into(),
            "web".into(),
        ],
        started_at_ms: 1_760_070_000_000,
    });
    b.probe.procs.listeners.lock().unwrap().push(DshListener {
        owner_pid: 44000,
        authority: "127.0.0.1:5221".into(),
    });
    let env = b.env();
    let cli = dsh_identity::web_cli_source(&env, Some(std::ffi::OsStr::new("/usr/bin")))
        .expect("相对合法链接必须被采信（相对展开 + 统一解析）");
    assert_eq!(cli, PathBuf::from("/opt/dsh/bin/dsh"));

    // ② 二级外跳链接：dsh → mid（链接）→ 包外真实文件（存在！）→ 拒。
    let b2 = bench(Seed::NoDesktop, false);
    b2.store.lock().unwrap().set_file(
        join_lexical(
            "/opt/dsh",
            &["node_modules", "@deepseek-ai", "dsh", "package.json"],
        ),
        br#"{"name":"@deepseek-ai/dsh","bin":{"dsh":"bin/dsh.js"}}"#.to_vec(),
    );
    b2.store.lock().unwrap().set_file(
        join_lexical(
            "/opt/dsh",
            &["node_modules", "@deepseek-ai", "dsh", "bin", "dsh.js"],
        ),
        b"#!/usr/bin/env node\n".to_vec(),
    );
    b2.store
        .lock()
        .unwrap()
        .set_file("/elsewhere/evil.js", b"evil".to_vec());
    b2.store.lock().unwrap().set_link(
        PathBuf::from("/opt/dsh/bin/dsh"),
        PathBuf::from("/opt/dsh/bin/mid"),
    );
    b2.store.lock().unwrap().set_link(
        PathBuf::from("/opt/dsh/bin/mid"),
        PathBuf::from("/elsewhere/evil.js"),
    );
    b2.probe.procs.processes.lock().unwrap().push(DshProcess {
        pid: 44001,
        exe: PathBuf::from("/usr/local/bin/node"),
        cmdline: vec![
            "/usr/local/bin/node".into(),
            "/opt/dsh/bin/dsh".into(),
            "web".into(),
        ],
        started_at_ms: 1_760_070_000_000,
    });
    b2.probe.procs.listeners.lock().unwrap().push(DshListener {
        owner_pid: 44001,
        authority: "127.0.0.1:5222".into(),
    });
    let err = dsh_identity::web_cli_source(&b2.env(), Some(std::ffi::OsStr::new("/usr/bin")))
        .expect_err("二级外跳链接必须被拒（解析后出包）");
    assert_eq!(err.code, codes::CLI_SOURCE_UNVERIFIED, "{err}");

    // ③ `..` 逃逸链接：表面落在包内目录，解析后越出包目录的真实文件 → 拒。
    let b3 = bench(Seed::NoDesktop, false);
    b3.store.lock().unwrap().set_file(
        join_lexical(
            "/opt/dsh",
            &["node_modules", "@deepseek-ai", "dsh", "package.json"],
        ),
        br#"{"name":"@deepseek-ai/dsh","bin":{"dsh":"bin/dsh.js"}}"#.to_vec(),
    );
    b3.store.lock().unwrap().set_file(
        join_lexical(
            "/opt/dsh",
            &["node_modules", "@deepseek-ai", "dsh", "bin", "dsh.js"],
        ),
        b"#!/usr/bin/env node\n".to_vec(),
    );
    b3.store.lock().unwrap().set_file(
        join_lexical("/opt/dsh", &["node_modules", "elsewhere", "evil.js"]),
        b"evil".to_vec(),
    );
    b3.store.lock().unwrap().set_link(
        PathBuf::from("/opt/dsh/bin/dsh"),
        PathBuf::from("/opt/dsh/node_modules/@deepseek-ai/dsh/bin/../../../elsewhere/evil.js"),
    );
    b3.probe.procs.processes.lock().unwrap().push(DshProcess {
        pid: 44002,
        exe: PathBuf::from("/usr/local/bin/node"),
        cmdline: vec![
            "/usr/local/bin/node".into(),
            "/opt/dsh/bin/dsh".into(),
            "web".into(),
        ],
        started_at_ms: 1_760_070_000_000,
    });
    b3.probe.procs.listeners.lock().unwrap().push(DshListener {
        owner_pid: 44002,
        authority: "127.0.0.1:5223".into(),
    });
    let err = dsh_identity::web_cli_source(&b3.env(), Some(std::ffi::OsStr::new("/usr/bin")))
        .expect_err("`..` 逃逸链接必须被拒");
    assert_eq!(err.code, codes::CLI_SOURCE_UNVERIFIED, "{err}");
}

/// I5-r4②：shim 的「内容包含包名」不构成调用关系——注释里写上官方包名 +
/// 真包锚在场，但 **exec 行**实际调用的是包内非声明入口（≠ bin 声明）→ 拒；
/// exec 调用声明入口（$basedir 代入后实际解析一致）→ 采信。
#[test]
fn shim_comment_with_package_name_never_verifies_entry_identity() {
    let b = bench(Seed::NoDesktop, false);
    // 官方布局：launcher 三层深（`../../..` 上溯到安装根），包在安装根 node_modules。
    let launcher = r"C:\tools\a\b\c\dsh";
    // 真锚 + 声明入口 + 包内**另一个**可执行脚本（非 bin 声明）。
    b.store.lock().unwrap().set_file(
        join_lexical(
            r"C:\tools",
            &["node_modules", "@deepseek-ai", "dsh", "package.json"],
        ),
        br#"{"name":"@deepseek-ai/dsh","bin":{"dsh":"bin/dsh.js"}}"#.to_vec(),
    );
    b.store.lock().unwrap().set_file(
        join_lexical(
            r"C:\tools",
            &["node_modules", "@deepseek-ai", "dsh", "bin", "dsh.js"],
        ),
        b"#!/usr/bin/env node\n".to_vec(),
    );
    b.store.lock().unwrap().set_file(
        join_lexical(
            r"C:\tools",
            &["node_modules", "@deepseek-ai", "dsh", "scripts", "other.js"],
        ),
        b"#!/usr/bin/env node\n".to_vec(),
    );
    // 同名 shim：注释含官方包名，exec 行实际调用 scripts/other.js（≠ 声明入口）。
    b.store.lock().unwrap().set_file(
        PathBuf::from(launcher),
        br#"#!/bin/sh
# @deepseek-ai/dsh launcher (trusted-looking comment)
basedir=$(CDPATH= cd -- "$(dirname -- "$0")/../../.." && pwd -P)
exec "$basedir/node" "$basedir/node_modules/@deepseek-ai/dsh/scripts/other.js" "$@"
"#
        .to_vec(),
    );
    b.probe.procs.processes.lock().unwrap().push(DshProcess {
        pid: 44003,
        exe: PathBuf::from(r"C:\Node\node.exe"),
        cmdline: vec![r"C:\Node\node.exe".into(), launcher.into(), "web".into()],
        started_at_ms: 1_760_070_000_000,
    });
    b.probe.procs.listeners.lock().unwrap().push(DshListener {
        owner_pid: 44003,
        authority: "127.0.0.1:5224".into(),
    });
    let err = dsh_identity::web_cli_source(&b.env(), Some(std::ffi::OsStr::new("/usr/bin")))
        .expect_err("注释含包名 + exec 非声明入口必须被拒");
    assert_eq!(err.code, codes::CLI_SOURCE_UNVERIFIED, "{err}");

    // exec 行改为调用**声明入口**（官方 basedir 代入后实际解析一致）→ 采信。
    b.store.lock().unwrap().set_file(
        PathBuf::from(launcher),
        br#"#!/bin/sh
basedir=$(CDPATH= cd -- "$(dirname -- "$0")/../../.." && pwd -P)
exec "$basedir/node" "$basedir/node_modules/@deepseek-ai/dsh/bin/dsh.js" "$@"
"#
        .to_vec(),
    );
    let cli = dsh_identity::web_cli_source(&b.env(), Some(std::ffi::OsStr::new("/usr/bin")))
        .expect("exec 调用声明入口必须被采信");
    assert_eq!(cli, PathBuf::from(launcher));
}

// ============================================================
// H05 修复轮 6 回归（I5①补交两测试 + I5②三处结构性关闭）
// ============================================================

/// I5-r6①（补交）：包内 `bin/` **目录本身**是包外符号链接——`package/bin/dsh.js`
/// 的 read_link 不返回父目录链接、词法上仍「包内」，但组件级实际解析落在包外真实
/// 文件 → 声明入口组件级包含失败 → 拒（实际包外文件不得被判为声明入口）。
#[test]
fn in_package_directory_symlink_escape_is_refused() {
    let b = bench(Seed::NoDesktop, false);
    // 包锚（真实常规文件）+ 包内 bin 目录 = 指向包外的符号链接 + 包外真实文件。
    b.store.lock().unwrap().set_file(
        join_lexical(
            "/opt/escape",
            &["node_modules", "@deepseek-ai", "dsh", "package.json"],
        ),
        br#"{"name":"@deepseek-ai/dsh","bin":{"dsh":"bin/dsh.js"}}"#.to_vec(),
    );
    b.store.lock().unwrap().set_link(
        PathBuf::from("/opt/escape/node_modules/@deepseek-ai/dsh/bin"),
        PathBuf::from("/elsewhere"),
    );
    b.store
        .lock()
        .unwrap()
        .set_file("/elsewhere/dsh.js", b"out-of-package\n".to_vec());
    // 入口：安装根 bin 下的同名链接 → 指向（词法上的）声明入口。
    b.store.lock().unwrap().set_link(
        PathBuf::from("/opt/escape/bin/dsh"),
        PathBuf::from("/opt/escape/node_modules/@deepseek-ai/dsh/bin/dsh.js"),
    );
    // 活 web 宿主（argv 带入口）。
    b.probe.procs.processes.lock().unwrap().push(DshProcess {
        pid: 45000,
        exe: PathBuf::from("/usr/local/bin/node"),
        cmdline: vec![
            "/usr/local/bin/node".into(),
            "/opt/escape/bin/dsh".into(),
            "web".into(),
        ],
        started_at_ms: 1_760_080_000_000,
    });
    b.probe.procs.listeners.lock().unwrap().push(DshListener {
        owner_pid: 45000,
        authority: "127.0.0.1:5225".into(),
    });
    let err = dsh_identity::web_cli_source(&b.env(), Some(std::ffi::OsStr::new("/usr/bin")))
        .expect_err("包内目录链接逃逸必须被拒（实际包外文件 ≠ 声明入口）");
    assert_eq!(err.code, codes::CLI_SOURCE_UNVERIFIED, "{err}");
}

/// I5-r6①（补交）：目录链接 + `..` **交织**——内核序解析（先展开目录链接、
/// `..` 作用于展开后的真实目录）落在包外真实文件；词法折叠先行会误算出
/// 「树内」路径——两者分叉由内核序钉死为拒绝。
#[test]
fn directory_link_and_dotdot_interleave_is_refused() {
    let b = bench(Seed::NoDesktop, false);
    // 同款目录链接逃逸：bin → /elsewhere；声明入口实际解析 = /elsewhere/dsh.js。
    b.store.lock().unwrap().set_file(
        join_lexical(
            "/opt/escape",
            &["node_modules", "@deepseek-ai", "dsh", "package.json"],
        ),
        br#"{"name":"@deepseek-ai/dsh","bin":{"dsh":"bin/dsh.js"}}"#.to_vec(),
    );
    b.store.lock().unwrap().set_link(
        PathBuf::from("/opt/escape/node_modules/@deepseek-ai/dsh/bin"),
        PathBuf::from("/elsewhere"),
    );
    b.store
        .lock()
        .unwrap()
        .set_file("/elsewhere/dsh.js", b"out-of-package\n".to_vec());
    // 交织逃逸目标：内核序 = /elsewhere + ../../../outside/evil.js → /outside/evil.js
    // （真实存在）；词法折叠先行会误算到 node_modules 树内。
    b.store
        .lock()
        .unwrap()
        .set_file("/outside/evil.js", b"evil\n".to_vec());
    // 入口链接：指向交织逃逸目标。
    b.store.lock().unwrap().set_link(
        PathBuf::from("/opt/escape/bin/dsh"),
        PathBuf::from("/opt/escape/node_modules/@deepseek-ai/dsh/bin/../../../outside/evil.js"),
    );
    b.probe.procs.processes.lock().unwrap().push(DshProcess {
        pid: 45001,
        exe: PathBuf::from("/usr/local/bin/node"),
        cmdline: vec![
            "/usr/local/bin/node".into(),
            "/opt/escape/bin/dsh".into(),
            "web".into(),
        ],
        started_at_ms: 1_760_080_000_000,
    });
    b.probe.procs.listeners.lock().unwrap().push(DshListener {
        owner_pid: 45001,
        authority: "127.0.0.1:5226".into(),
    });
    let err = dsh_identity::web_cli_source(&b.env(), Some(std::ffi::OsStr::new("/usr/bin")))
        .expect_err("目录链接 + `..` 交织逃逸必须被拒");
    assert_eq!(err.code, codes::CLI_SOURCE_UNVERIFIED, "{err}");
}

/// I5-r6②Form N 收紧：**仅接受 `node <声明入口> [占位…]` 形态**——`--eval` 一类
/// 选项 token 一律拒（`exec node --eval=… <声明入口>` → 拒；声明入口只是不执行的
/// 参数不再是证据）；正规形态 `exec node <声明入口> "$@"` → 采信。
#[test]
fn form_n_rejects_options_and_extra_args_accepts_declared_script() {
    let b = bench(Seed::NoDesktop, false);
    let launcher = r"/opt/formn/a/b/c/dsh";
    b.store.lock().unwrap().set_file(
        join_lexical(
            "/opt/formn",
            &["node_modules", "@deepseek-ai", "dsh", "package.json"],
        ),
        br#"{"name":"@deepseek-ai/dsh","bin":{"dsh":"bin/dsh.js"}}"#.to_vec(),
    );
    b.store.lock().unwrap().set_file(
        join_lexical(
            "/opt/formn",
            &["node_modules", "@deepseek-ai", "dsh", "bin", "dsh.js"],
        ),
        b"#!/usr/bin/env node\n".to_vec(),
    );
    b.store.lock().unwrap().set_file(
        PathBuf::from(launcher),
        br#"#!/bin/sh
basedir=$(CDPATH= cd -- "$(dirname -- "$0")/../../.." && pwd -P)
exec "$basedir/node" "$basedir/node_modules/@deepseek-ai/dsh/bin/dsh.js" "$@"
"#
        .to_vec(),
    );
    b.probe.procs.processes.lock().unwrap().push(DshProcess {
        pid: 45002,
        exe: PathBuf::from("/usr/local/bin/node"),
        cmdline: vec!["/usr/local/bin/node".into(), launcher.into(), "web".into()],
        started_at_ms: 1_760_080_000_000,
    });
    b.probe.procs.listeners.lock().unwrap().push(DshListener {
        owner_pid: 45002,
        authority: "127.0.0.1:5227".into(),
    });
    let env = b.env();

    // 正规形态：node <声明入口> "$@" → 采信。
    let cli = dsh_identity::web_cli_source(&env, Some(std::ffi::OsStr::new("/usr/bin")))
        .expect("正规 node shim 必须被采信");
    assert_eq!(cli, PathBuf::from(launcher));

    // --eval 绕过：node --eval=<code> <声明入口> → 拒（中间参数全拒）。
    b.store.lock().unwrap().set_file(
        PathBuf::from(launcher),
        br#"#!/bin/sh
basedir=$(CDPATH= cd -- "$(dirname -- "$0")/../../.." && pwd -P)
exec node --eval=console.log(123) "$basedir/node_modules/@deepseek-ai/dsh/bin/dsh.js" "$@"
"#
        .to_vec(),
    );
    let err = dsh_identity::web_cli_source(&env, Some(std::ffi::OsStr::new("/usr/bin")))
        .expect_err("--eval 绕过必须被拒");
    assert_eq!(err.code, codes::CLI_SOURCE_UNVERIFIED, "{err}");

    // 选项含未定义变量时，shell 展开后仍是 --eval；不得当占位跳过。
    b.store.lock().unwrap().set_file(
        PathBuf::from(launcher),
        br#"#!/bin/sh
basedir=$(CDPATH= cd -- "$(dirname -- "$0")/../../.." && pwd -P)
exec node "--eval=0//$IGNORED" "$basedir/node_modules/@deepseek-ai/dsh/bin/dsh.js" "$@"
"#
        .to_vec(),
    );
    let err = dsh_identity::web_cli_source(&env, Some(std::ffi::OsStr::new("/usr/bin")))
        .expect_err("含变量的 --eval 选项必须被拒");
    assert_eq!(err.code, codes::CLI_SOURCE_UNVERIFIED);

    // 未加引号的 $@ 或其他变量不能当作唯一允许的 "$@" 占位。
    for placeholder in ["$@", "\"$IGNORED\""] {
        let script = format!(
            "#!/bin/sh\nbasedir=$(CDPATH= cd -- \"$(dirname -- \"$0\")/../../..\" && pwd -P)\nexec node \"$basedir/node_modules/@deepseek-ai/dsh/bin/dsh.js\" {placeholder}\n"
        );
        b.store
            .lock()
            .unwrap()
            .set_file(PathBuf::from(launcher), script.into_bytes());
        let err = dsh_identity::web_cli_source(&env, Some(std::ffi::OsStr::new("/usr/bin")))
            .expect_err("非精确占位必须被拒");
        assert_eq!(err.code, codes::CLI_SOURCE_UNVERIFIED);
    }

    // 额外非占位参数：node <声明入口> extra.js → 拒。
    b.store.lock().unwrap().set_file(
        PathBuf::from(launcher),
        br#"#!/bin/sh
basedir=$(CDPATH= cd -- "$(dirname -- "$0")/../../.." && pwd -P)
exec node "$basedir/node_modules/@deepseek-ai/dsh/bin/dsh.js" "$basedir/node_modules/@deepseek-ai/dsh/scripts/other.js" "$@"
"#.to_vec(),
    );
    b.store.lock().unwrap().set_file(
        join_lexical(
            "/opt/formn",
            &["node_modules", "@deepseek-ai", "dsh", "scripts", "other.js"],
        ),
        b"#!/usr/bin/env node\n".to_vec(),
    );
    let err = dsh_identity::web_cli_source(&env, Some(std::ffi::OsStr::new("/usr/bin")))
        .expect_err("额外非占位参数必须被拒");
    assert_eq!(err.code, codes::CLI_SOURCE_UNVERIFIED, "{err}");
}

/// I5-r6②Form B 收紧：宿主二进制须落在**已核验安装根**（安装根 = 包锚上溯到
/// node_modules 的那一层的上一层），且内层参数**组件级等于声明入口**（包内其他
/// 脚本不再算证据）。正规官方形态（宿主 + 内层 = 声明入口）→ 采信；内层为包内
/// 其他脚本 → 拒。
#[test]
fn form_b_requires_declared_entry_argument_within_install_root() {
    let b = bench(Seed::NoDesktop, false);
    let launcher = r"/opt/formb/a/b/c/dsh";
    b.store.lock().unwrap().set_file(
        join_lexical(
            "/opt/formb",
            &["node_modules", "@deepseek-ai", "dsh", "package.json"],
        ),
        br#"{"name":"@deepseek-ai/dsh","bin":{"dsh":"bin/dsh.js"}}"#.to_vec(),
    );
    b.store.lock().unwrap().set_file(
        join_lexical(
            "/opt/formb",
            &["node_modules", "@deepseek-ai", "dsh", "bin", "dsh.js"],
        ),
        b"#!/usr/bin/env node\n".to_vec(),
    );
    b.store.lock().unwrap().set_file(
        join_lexical(
            "/opt/formb",
            &["node_modules", "@deepseek-ai", "dsh", "scripts", "other.js"],
        ),
        b"#!/usr/bin/env node\n".to_vec(),
    );
    // 官方宿主二进制（安装根内：安装根 = 容纳 node_modules 的 /opt/formb）。
    b.store.lock().unwrap().set_file(
        join_lexical("/opt/formb", &["MacOS", "DeepSeek Harness"]),
        b"MZ-fake-host-binary\n".to_vec(),
    );
    // 官方实录形态 shim：环境赋值前缀 + exec 宿主二进制（安装根内）+ 内层声明入口。
    b.store.lock().unwrap().set_file(
        PathBuf::from(launcher),
        br#"#!/bin/sh
resources=$(CDPATH= cd -- "$(dirname -- "$launcher")/../../.." && pwd -P)
ELECTRON_RUN_AS_NODE=1 exec "$resources/MacOS/DeepSeek Harness" --expose-internals "$resources/node_modules/@deepseek-ai/dsh/bin/dsh.js" "$@"
"#.to_vec(),
    );
    b.probe.procs.processes.lock().unwrap().push(DshProcess {
        pid: 45003,
        exe: PathBuf::from("/usr/local/bin/node"),
        cmdline: vec!["/usr/local/bin/node".into(), launcher.into(), "web".into()],
        started_at_ms: 1_760_080_000_000,
    });
    b.probe.procs.listeners.lock().unwrap().push(DshListener {
        owner_pid: 45003,
        authority: "127.0.0.1:5228".into(),
    });
    let env = b.env();

    // 缺少 launcher=$0 时不能假定 $launcher 等于入口路径。
    let err = dsh_identity::web_cli_source(&env, Some(std::ffi::OsStr::new("/usr/bin")))
        .expect_err("缺少 launcher 绑定必须被拒");
    assert_eq!(err.code, codes::CLI_SOURCE_UNVERIFIED);

    b.store.lock().unwrap().set_file(
        PathBuf::from(launcher),
        br#"#!/bin/sh
launcher=$0
resources=$(CDPATH= cd -- "$(dirname -- "$launcher")/../../.." && pwd -P)
ELECTRON_RUN_AS_NODE=1 exec "$resources/MacOS/DeepSeek Harness" --expose-internals "$resources/node_modules/@deepseek-ai/dsh/bin/dsh.js" "$@"
"#.to_vec(),
    );
    // 官方形态两个证据齐备（宿主在安装根内 + 内层 = 声明入口）→ 采信。
    let cli = dsh_identity::web_cli_source(&env, Some(std::ffi::OsStr::new("/usr/bin")))
        .expect("官方宿主二进制 + 内层声明入口必须被采信");
    assert_eq!(cli, PathBuf::from(launcher));

    // 收紧拒绝：内层参数换成包内**其他**脚本（≠ 声明入口）→ 拒。
    b.store.lock().unwrap().set_file(
        PathBuf::from(launcher),
        br#"#!/bin/sh
resources=$(CDPATH= cd -- "$(dirname -- "$launcher")/../../.." && pwd -P)
ELECTRON_RUN_AS_NODE=1 exec "$resources/MacOS/DeepSeek Harness" --expose-internals "$resources/node_modules/@deepseek-ai/dsh/scripts/other.js" "$@"
"#.to_vec(),
    );
    let err = dsh_identity::web_cli_source(&env, Some(std::ffi::OsStr::new("/usr/bin")))
        .expect_err("内层参数 ≠ 声明入口必须被拒");
    assert_eq!(err.code, codes::CLI_SOURCE_UNVERIFIED, "{err}");
}

/// I5-r6②赋值结构：basedir/resources 赋值须**整表达式**匹配官方形态——命令
/// 替换（$/usr/bin/printf）、旧 dirname 形态一律拒。
#[test]
fn shim_assignments_must_match_official_templates_exactly() {
    let b = bench(Seed::NoDesktop, false);
    let launcher = r"/opt/formasg/a/b/c/dsh";
    b.store.lock().unwrap().set_file(
        join_lexical(
            "/opt/formasg",
            &["node_modules", "@deepseek-ai", "dsh", "package.json"],
        ),
        br#"{"name":"@deepseek-ai/dsh","bin":{"dsh":"bin/dsh.js"}}"#.to_vec(),
    );
    b.store.lock().unwrap().set_file(
        join_lexical(
            "/opt/formasg",
            &["node_modules", "@deepseek-ai", "dsh", "bin", "dsh.js"],
        ),
        b"#!/usr/bin/env node\n".to_vec(),
    );
    b.probe.procs.processes.lock().unwrap().push(DshProcess {
        pid: 45004,
        exe: PathBuf::from("/usr/local/bin/node"),
        cmdline: vec!["/usr/local/bin/node".into(), launcher.into(), "web".into()],
        started_at_ms: 1_760_080_000_000,
    });
    b.probe.procs.listeners.lock().unwrap().push(DshListener {
        owner_pid: 45004,
        authority: "127.0.0.1:5229".into(),
    });
    let env = b.env();

    // 命令替换赋值（$/usr/bin/printf I5）→ 拒。
    b.store.lock().unwrap().set_file(
        PathBuf::from(launcher),
        br#"#!/bin/sh
resources=$(/usr/bin/printf I5)
exec "$resources/node_modules/@deepseek-ai/dsh/bin/dsh.js" "$@"
"#
        .to_vec(),
    );
    let err = dsh_identity::web_cli_source(&env, Some(std::ffi::OsStr::new("/usr/bin")))
        .expect_err("命令替换赋值必须被拒");
    assert_eq!(err.code, codes::CLI_SOURCE_UNVERIFIED, "{err}");

    // 旧 dirname 形态（非官方整表达式）→ 拒。
    b.store.lock().unwrap().set_file(
        PathBuf::from(launcher),
        br#"#!/bin/sh
basedir=$(dirname "$0")
exec "$basedir/node" "$basedir/node_modules/@deepseek-ai/dsh/bin/dsh.js" "$@"
"#
        .to_vec(),
    );
    let err = dsh_identity::web_cli_source(&env, Some(std::ffi::OsStr::new("/usr/bin")))
        .expect_err("旧 dirname 形态必须被拒");
    assert_eq!(err.code, codes::CLI_SOURCE_UNVERIFIED, "{err}");
}

async fn form_b_alias_management_case(replace_host: bool) {
    let b = bench(Seed::NoDesktop, false);
    let (env, target) = proven_web_fixture(&b, 1_760_045_000_000);
    // 默认逐组件链接解析使用 POSIX 分隔符；用原生绝对路径建模此链接，
    // 避免假文件源的 Windows 键与解析器查询路径不一致。
    let launcher = "/opt/formb/a/b/c/dsh";
    let host = PathBuf::from("/opt/formb/MacOS/DeepSeek Harness");
    {
        let mut files = b.store.lock().unwrap();
        files.set_file(PathBuf::from(launcher), br#"#!/bin/sh
launcher=$0
resources=$(CDPATH= cd -- "$(dirname -- "$launcher")/../../.." && pwd -P)
ELECTRON_RUN_AS_NODE=1 exec "$resources/MacOS/host-alias" --expose-internals "$resources/node_modules/@deepseek-ai/dsh/bin/dsh.js" "$@"
"#.to_vec());
        files.set_file(
            "/opt/formb/node_modules/@deepseek-ai/dsh/package.json",
            br#"{"name":"@deepseek-ai/dsh","bin":{"dsh":"bin/dsh.js"}}"#.to_vec(),
        );
        files.set_file(
            "/opt/formb/node_modules/@deepseek-ai/dsh/bin/dsh.js",
            b"#!/usr/bin/env node\n".to_vec(),
        );
        files.set_file(host.clone(), b"original host".to_vec());
        files.set_link("/opt/formb/MacOS/host-alias", host.clone());
    }
    let mut process = web_host_process(1_760_045_000_000);
    process.cmdline[1] = launcher.into();
    b.probe.procs.processes.lock().unwrap().push(process);
    b.probe.procs.listeners.lock().unwrap().push(DshListener {
        owner_pid: WEB_PID,
        authority: "127.0.0.1:5216".into(),
    });
    let source =
        dsh_identity::web_cli_source(&env, None).expect("替换前 Form B 符号链接必须是可核验来源");
    assert_eq!(source, PathBuf::from(launcher));
    let mgr = dsh_install_production::init_instance_manager(&env, &target, b.revoker.clone());
    let id = mgr
        .start(ManagementKind::Install)
        .expect("启动实例管理作业");
    let waiting = wait_phase(
        &mgr,
        &id,
        &[
            ManagementPhase::WaitingForDesktopExit,
            ManagementPhase::Failed,
        ],
    )
    .await;
    assert_eq!(
        waiting.phase,
        ManagementPhase::WaitingForDesktopExit,
        "{waiting:?}"
    );
    assert!(b.probe.calls().is_empty());
    let ledger_path = b
        .tuvis
        .join("dsh")
        .join("instances")
        .join(&target.instance_id)
        .join("management.json");
    let ledger: serde_json::Value =
        serde_json::from_slice(&std::fs::read(ledger_path).expect("等待前已持久化来源证明"))
            .expect("管理账本可解析");
    assert_eq!(
        ledger["cliOrigin"]["kind"], "npm",
        "等待状态必须持有已核验来源：{ledger}"
    );
    assert_eq!(ledger["cliOrigin"]["entry"], launcher);
    assert_eq!(ledger["cliUnresolved"], false);
    assert!(ledger["cliPending"].is_null());
    if replace_host {
        b.store
            .lock()
            .unwrap()
            .set_file(host, b"replacement host".to_vec());
    }
    b.probe
        .procs
        .processes
        .lock()
        .unwrap()
        .retain(|p| p.pid != WEB_PID);
    b.probe
        .procs
        .listeners
        .lock()
        .unwrap()
        .retain(|l| l.owner_pid != WEB_PID);
    let job = wait_phase(
        &mgr,
        &id,
        &[ManagementPhase::Succeeded, ManagementPhase::Failed],
    )
    .await;
    if replace_host {
        assert_eq!(job.phase, ManagementPhase::Failed, "{job:?}");
        assert_eq!(
            job.reason_code.as_deref(),
            Some(codes::CLI_SOURCE_UNVERIFIED)
        );
        assert!(b.probe.calls().is_empty(), "替换后不得执行 CLI");
    } else {
        assert_eq!(job.phase, ManagementPhase::Succeeded, "{job:?}");
        assert!(!b.probe.calls().is_empty(), "正例必须执行 CLI");
    }
}

// 账本探针为进程级单例；同一测试二进制中使用它的夹具必须串行。
#[cfg(feature = "dsh-test-probes")]
static LEDGER_PROBE_TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(all(unix, feature = "dsh-test-probes"))]
#[tokio::test]
// 进程内账本探针为单例，跨 await 持锁是为了串行隔离整个探针生命周期。
#[allow(clippy::await_holding_lock)]
async fn manager_constructed_during_pending_refreshes_after_profile_lock() {
    let _probe_guard = LEDGER_PROBE_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    use multi_agents_manager_lib::services::dsh_control::install::ledger_commit_probe;

    let b = bench(Seed::NoDesktop, false);
    // instance_id_for 会规范化真实路径；先建目录再生成双方实例 ID，
    // 避免 macOS 的 /var -> /private/var 导致同一 profile 得到不同 ID。
    let profile = b._tmp.path().join("web-home/profiles/web");
    std::fs::create_dir_all(&profile).unwrap();
    std::fs::write(profile.join("package.json"), b"{}").unwrap();
    let (env, target) = proven_web_fixture(&b, 1_760_045_000_000);
    let (mut env_b, target_b) = proven_web_fixture(&b, 1_760_045_000_000);
    assert_eq!(target.instance_id, target_b.instance_id);
    env_b.clock = Arc::new(FixedClock(1_760_000_000_001));
    let launcher = "/opt/formb/a/b/c/dsh";
    {
        let mut files = b.store.lock().unwrap();
        files.set_file(launcher, br#"#!/bin/sh
launcher=$0
resources=$(CDPATH= cd -- "$(dirname -- "$launcher")/../../.." && pwd -P)
ELECTRON_RUN_AS_NODE=1 exec "$resources/MacOS/DeepSeek Harness" --expose-internals "$resources/node_modules/@deepseek-ai/dsh/bin/dsh.js" "$@"
"#.to_vec());
        files.set_file(
            "/opt/formb/node_modules/@deepseek-ai/dsh/package.json",
            br#"{"name":"@deepseek-ai/dsh","bin":{"dsh":"bin/dsh.js"}}"#.to_vec(),
        );
        files.set_file(
            "/opt/formb/node_modules/@deepseek-ai/dsh/bin/dsh.js",
            b"#!/usr/bin/env node\n".to_vec(),
        );
        files.set_file("/opt/formb/MacOS/DeepSeek Harness", b"host".to_vec());
    }
    let mut process = web_host_process(1_760_045_000_000);
    process.cmdline[1] = launcher.into();
    b.probe.procs.processes.lock().unwrap().push(process);
    b.probe.procs.listeners.lock().unwrap().push(DshListener {
        owner_pid: WEB_PID,
        authority: "127.0.0.1:5216".into(),
    });
    let ledger = b
        .tuvis
        .join("dsh/instances")
        .join(&target.instance_id)
        .join("management.json");
    let second: Arc<Mutex<Option<DshPluginManager>>> = Arc::new(Mutex::new(None));
    let captured = second.clone();
    let expected = ledger.clone();
    let target_b = target.clone();
    let revoker = b.revoker.clone();
    ledger_commit_probe::set_before_lock(Some(Arc::new(move |path| {
        if path != expected || captured.lock().unwrap().is_some() {
            return;
        }
        let value = std::fs::read(path)
            .ok()
            .and_then(|data| serde_json::from_slice::<serde_json::Value>(&data).ok());
        if value
            .as_ref()
            .is_some_and(|v| !v["cliPending"].is_null() && v["cliUnresolved"] == true)
        {
            *captured.lock().unwrap() = Some(dsh_install_production::init_instance_manager(
                &env_b,
                &target_b,
                revoker.clone(),
            ));
        }
    })));
    struct ResetProbe;
    impl Drop for ResetProbe {
        fn drop(&mut self) {
            ledger_commit_probe::set_before_lock(None);
        }
    }
    let _reset = ResetProbe;
    let first = dsh_install_production::init_instance_manager(&env, &target, b.revoker.clone());
    let a_id = first.start(ManagementKind::Install).unwrap();
    let waiting = wait_phase(
        &first,
        &a_id,
        &[
            ManagementPhase::WaitingForDesktopExit,
            ManagementPhase::Failed,
        ],
    )
    .await;
    assert_eq!(
        waiting.phase,
        ManagementPhase::WaitingForDesktopExit,
        "{waiting:?}"
    );
    let second = second
        .lock()
        .unwrap()
        .take()
        .expect("必须在 A 的 pending 与 unresolved 均落盘时构造 B");
    ledger_commit_probe::set_before_lock(None);
    assert_eq!(
        second
            .start(ManagementKind::Uninstall)
            .expect_err("A 持有物理 profile 锁")
            .code,
        codes::JOB_BUSY
    );
    b.probe
        .procs
        .processes
        .lock()
        .unwrap()
        .retain(|p| p.pid != WEB_PID);
    b.probe
        .procs
        .listeners
        .lock()
        .unwrap()
        .retain(|l| l.owner_pid != WEB_PID);
    let a = wait_phase(
        &first,
        &a_id,
        &[ManagementPhase::Succeeded, ManagementPhase::Failed],
    )
    .await;
    assert_eq!(a.phase, ManagementPhase::Succeeded, "{a:?}");
    let cleared: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&ledger).unwrap()).unwrap();
    assert!(cleared["cliPending"].is_null());
    assert_eq!(cleared["cliUnresolved"], false);
    // 终态回执可能先于任务析构释放活动锁；只对 JOB_BUSY 等待锁释放。
    let deadline = Instant::now() + Duration::from_secs(10);
    let b_id = loop {
        match second.start(ManagementKind::Uninstall) {
            Ok(id) => break id,
            Err(error) if error.code == codes::JOB_BUSY && Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(error) => panic!("B 获取锁后应刷新账本: {error}"),
        }
    };
    let job = wait_phase(
        &second,
        &b_id,
        &[ManagementPhase::Succeeded, ManagementPhase::Failed],
    )
    .await;
    assert_eq!(job.phase, ManagementPhase::Succeeded, "{job:?}");
    let final_ledger: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&ledger).unwrap()).unwrap();
    let jobs = final_ledger["jobs"].as_array().unwrap();
    for id in [&a_id, &b_id] {
        assert!(
            jobs.iter()
                .any(|job| job["jobId"] == *id && job["phase"] == "succeeded"),
            "历史作业丢失: {final_ledger}"
        );
    }
}

#[tokio::test]
async fn form_b_host_alias_unchanged_allows_management_install() {
    form_b_alias_management_case(false).await;
}

#[tokio::test]
async fn form_b_host_alias_target_replaced_while_waiting_rejects_install() {
    form_b_alias_management_case(true).await;
}

#[cfg(feature = "dsh-test-probes")]
#[test]
fn independent_managers_must_not_overwrite_a_commit_after_comparison() {
    use multi_agents_manager_lib::services::dsh_control::install::ledger_commit_probe;
    use std::sync::mpsc;

    let _probe_guard = LEDGER_PROBE_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let b = bench(Seed::FullDesktop, false);
    let target = desktop_target();
    let ledger = b
        .tuvis
        .join("dsh")
        .join("instances")
        .join(&target.instance_id)
        .join("management.json");
    let first = dsh_install_production::init_instance_manager(&b.env(), &target, b.revoker.clone());
    let second =
        dsh_install_production::init_instance_manager(&b.env(), &target, b.revoker.clone());
    let (arrived_tx, arrived_rx) = mpsc::channel();
    let (second_lock_tx, second_lock_rx) = mpsc::channel();
    let lock_path = ledger.clone();
    ledger_commit_probe::set_before_lock(Some(Arc::new(move |path| {
        if path == lock_path && std::thread::current().name() == Some("dsh-ledger-second") {
            let _ = second_lock_tx.send(());
        }
    })));
    let (resume_tx, resume_rx) = mpsc::channel();
    let resume_rx = Mutex::new(resume_rx);
    let paused = AtomicBool::new(false);
    let expected_path = ledger.clone();
    ledger_commit_probe::set(Some(Arc::new(move |path| {
        if path == expected_path && !paused.swap(true, Ordering::SeqCst) {
            arrived_tx.send(()).unwrap();
            resume_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10))
                .expect("释放账本提交屏障");
        }
    })));
    struct ClearProbe;
    impl Drop for ClearProbe {
        fn drop(&mut self) {
            ledger_commit_probe::set(None);
            ledger_commit_probe::set_before_lock(None);
        }
    }
    let _clear = ClearProbe;
    let first_thread = std::thread::spawn(move || {
        first.test_only_commit_receipt("first-receipt", ManagementKind::Install)
    });
    arrived_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("第一写者比较后到达屏障");
    let (done_tx, done_rx) = mpsc::channel();
    let second_thread = std::thread::Builder::new()
        .name("dsh-ledger-second".into())
        .spawn(move || {
            let result = second.test_only_commit_receipt("second-receipt", ManagementKind::Repair);
            done_tx.send(()).unwrap();
            result
        })
        .expect("创建第二写者线程");
    second_lock_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("第二写者已到达锁获取点");
    // 第一写者持有账本锁时，第二次直接提交必须等待。
    let early = done_rx.recv_timeout(Duration::from_millis(500));
    let ledger_before_release = std::fs::read(&ledger).ok();
    resume_tx.send(()).unwrap();
    first_thread
        .join()
        .expect("第一写者线程完成")
        .expect("第一笔提交成功");
    let second_result = second_thread.join().expect("第二写者线程完成");
    assert!(
        matches!(early, Err(mpsc::RecvTimeoutError::Timeout)),
        "第一写者持锁期间，第二写者不得完成提交"
    );
    assert!(
        ledger_before_release.is_none(),
        "第一写者提交前账本不得出现"
    );
    let error = second_result.expect_err("过期快照不得提交");
    assert_eq!(error.code, codes::STATE_IO, "{error}");
    let final_ledger: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&ledger).expect("第一写者账本存在")).unwrap();
    let jobs = final_ledger["jobs"].as_array().expect("账本包含作业");
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0]["jobId"], "first-receipt");
    assert_eq!(jobs[0]["kind"], "install", "第二写者不得覆盖第一写者账本");
    assert!(b.probe.calls().is_empty(), "快照过期的第二作业不得执行 CLI");
}
