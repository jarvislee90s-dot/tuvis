//! macOS 桌面应用内置 CLI 启动 Web 的发现与授权回归。
#![cfg(unix)]
use std::path::{Path, PathBuf};
use std::sync::Arc;

use multi_agents_manager_lib::services::dsh_control::auth::DshAuthService;
use multi_agents_manager_lib::services::dsh_control::error::{codes, DshError};
use multi_agents_manager_lib::services::dsh_control::identity;
use multi_agents_manager_lib::services::dsh_control::instances::{instance_id_for, HostKind};
use multi_agents_manager_lib::services::dsh_control::model::{
    CommandRequest, DshClock, DshCommandRunner, DshEnvironment, DshFileSource, DshListener,
    DshProcess, DshProcessSource, ProcessEnvironmentEvidence, ProcessEnvironmentReader, RunOutcome,
};
use multi_agents_manager_lib::services::dsh_control::process_environment::{
    AllowlistedEnvironment, EnvironmentField, NativeGeneration, ObservedProcessIdentity,
    ReadCompleteness,
};

const PID: u32 = 424242;
const START: i64 = 1_760_000_000_000;
const AUTHORITY: &str = "127.0.0.1:5217";
const EXE: &str = "/Applications/DeepSeek Harness.app/Contents/Resources/../MacOS/DeepSeek Harness";
const CLI: &str = "/Applications/DeepSeek Harness.app/Contents/Resources/app.asar/dsh/node_modules/@deepseek-ai/dsh-desktop-host/lib/cli.js";
const DESKTOP: &str = "/Applications/DeepSeek Harness.app/Contents/Resources/app.asar/dsh/node_modules/@deepseek-ai/dsh-desktop-host/lib/index.js";

fn web_argv() -> Vec<String> {
    [
        EXE,
        "--expose-internals",
        CLI,
        "web",
        "--no-open",
        "--host",
        "127.0.0.1",
        "--port",
        "0",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

struct Processes {
    process: DshProcess,
    listeners: Vec<DshListener>,
}
impl DshProcessSource for Processes {
    fn list_processes(&self) -> Vec<DshProcess> {
        vec![self.process.clone()]
    }
    fn listening_sockets(&self) -> Vec<DshListener> {
        self.listeners.clone()
    }
}
struct Evidence {
    executable: PathBuf,
    home: PathBuf,
    stable: bool,
}
impl ProcessEnvironmentReader for Evidence {
    fn observed_environment(&self, pid: u32) -> Option<ProcessEnvironmentEvidence> {
        let before = ObservedProcessIdentity {
            pid,
            started_at_ms: START,
            executable: self.executable.clone(),
            native_generation: Some(NativeGeneration {
                unique_id: 11,
                pid_version: 1,
            }),
        };
        let mut after = before.clone();
        if !self.stable {
            after.native_generation.as_mut().unwrap().pid_version = 2;
        }
        Some(ProcessEnvironmentEvidence {
            identity_before: before,
            identity_after: after,
            completeness: ReadCompleteness::Complete,
            environment: AllowlistedEnvironment {
                dsh_home: EnvironmentField::Value(self.home.to_string_lossy().into_owned()),
                home: EnvironmentField::Unset,
            },
        })
    }
}
struct Files;
impl DshFileSource for Files {
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
    fn is_dir(&self, path: &Path) -> bool {
        std::fs::symlink_metadata(path)
            .map(|metadata| metadata.is_dir())
            .unwrap_or(false)
    }
    fn list_dir(&self, path: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(path)
            .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
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
                entry.map(|entry| entry.path()).map_err(|e| {
                    DshError::new(
                        codes::STATE_IO,
                        format!("read directory entry {}: {e}", path.display()),
                    )
                })
            })
            .collect()
    }
    fn read_small(&self, path: &Path, max_bytes: u64) -> Result<Vec<u8>, DshError> {
        let bytes =
            std::fs::read(path).map_err(|_| DshError::new(codes::UNSUPPORTED, "读取文件失败"))?;
        if bytes.len() as u64 > max_bytes {
            return Err(DshError::new(codes::UNSUPPORTED, "文件超出读取预算"));
        }
        Ok(bytes)
    }
}
struct Clock;
impl DshClock for Clock {
    fn now_ms(&self) -> i64 {
        START
    }
}
struct NoCommands;
impl DshCommandRunner for NoCommands {
    fn run(&self, _: &CommandRequest) -> Result<RunOutcome, DshError> {
        panic!("不得执行外部命令")
    }
}

struct Bench {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    env: DshEnvironment,
    id: String,
    home: PathBuf,
}
impl Bench {
    fn new(argv: Vec<String>, stable: bool, owner: u32) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let home = root.join("dsh-home");
        let desktop = home.join("profiles/desktop");
        std::fs::create_dir_all(&desktop).unwrap();
        std::fs::write(desktop.join("package.json"), "{}").unwrap();
        let contents = root.join("DeepSeek Harness.app/Contents");
        let resources = contents.join("Resources");
        let executable = contents.join("MacOS/DeepSeek Harness");
        let package = resources.join("app.asar/dsh/node_modules/@deepseek-ai/dsh-desktop-host/lib");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::create_dir_all(&resources).unwrap();
        write_asar_fixture(&resources.join("app.asar"), Some("@deepseek-ai/dsh"));
        std::fs::write(&executable, b"fixture").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // 保留实机 argv 的 Resources/../MacOS 形态，与实际 exe 指向同一文件。
        let argv_executable = resources.join("../MacOS/DeepSeek Harness");
        let argv: Vec<String> = argv
            .into_iter()
            .map(|arg| match arg.as_str() {
                EXE => argv_executable.to_string_lossy().into_owned(),
                CLI => package.join("cli.js").to_string_lossy().into_owned(),
                DESKTOP => package.join("index.js").to_string_lossy().into_owned(),
                _ => arg,
            })
            .collect();
        let process = DshProcess {
            pid: PID,
            exe: executable.clone(),
            cmdline: argv,
            started_at_ms: START,
        };
        let env = DshEnvironment {
            home: root.join("decoy"),
            tuvis_home: root.join("tuvis"),
            dsh_state_dir: Some(root.join("state")),
            processes: Arc::new(Processes {
                process,
                listeners: vec![DshListener {
                    owner_pid: owner,
                    authority: AUTHORITY.into(),
                }],
            }),
            files: Arc::new(Files),
            runner: Arc::new(NoCommands),
            clock: Arc::new(Clock),
            environments: Arc::new(Evidence {
                executable,
                home: home.clone(),
                stable,
            }),
        };
        let id = instance_id_for(HostKind::Web, &home.join("profiles/web"));
        Self {
            _tmp: tmp,
            root,
            env,
            id,
            home,
        }
    }

    fn replace_actual_executable(&mut self) {
        let executable = self.home.parent().unwrap().join("other-executable");
        std::fs::write(&executable, b"fixture").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut process = self.env.processes.list_processes().remove(0);
        process.exe = executable.clone();
        self.env.processes = Arc::new(Processes {
            process,
            listeners: vec![DshListener {
                owner_pid: PID,
                authority: AUTHORITY.into(),
            }],
        });
        self.env.environments = Arc::new(Evidence {
            executable,
            home: self.home.clone(),
            stable: true,
        });
    }
}

#[test]
fn actual_macos_web_argv_is_discovered_as_verified_web() {
    let b = Bench::new(web_argv(), true, PID);
    let discovery = identity::discover_detailed(&b.env).unwrap();
    assert_eq!(
        discovery.candidates.len(),
        1,
        "实机 Web argv 应产生候选：{discovery:?}"
    );
    let candidate = &discovery.candidates[0];
    assert_eq!(candidate.kind, HostKind::Web);
    assert!(candidate.verified);
    assert_eq!(candidate.instance_id, b.id);
    assert_eq!(candidate.host.pid, PID);
    assert_eq!(candidate.host.authority, AUTHORITY);
    assert_eq!(discovery.unresolved_desktop_hosts, 0);
}

#[test]
fn actual_macos_web_argv_resolves_by_instance_id() {
    let b = Bench::new(web_argv(), true, PID);
    let candidate =
        identity::discover_instance_candidate(&b.env, &b.id).expect("实机 Web 应能按实例发现");
    assert!(candidate.verified);
    assert_eq!(candidate.instance_id, b.id);
}

#[test]
fn auth_entry_accepts_actual_macos_web_argv_after_identity_verification() {
    let b = Bench::new(web_argv(), true, PID);
    let service = DshAuthService::new_for_instance(b.env.clone(), &b.id).unwrap();
    assert_eq!(
        service
            .current_generation_key_digest()
            .expect("授权入口应发现已核验 Web"),
        None
    );
}

#[test]
fn actual_desktop_remains_desktop_and_cannot_resolve_as_web() {
    let mut b = Bench::new(Vec::new(), true, PID);
    let profile = b.home.join("profiles/desktop");
    let process = DshProcess {
        pid: PID,
        exe: PathBuf::from(EXE),
        cmdline: vec![
            EXE.into(),
            DESKTOP.into(),
            profile.to_string_lossy().into_owned(),
        ],
        started_at_ms: START,
    };
    b.env.processes = Arc::new(Processes {
        process,
        listeners: vec![DshListener {
            owner_pid: PID,
            authority: AUTHORITY.into(),
        }],
    });
    let discovery = identity::discover_detailed(&b.env).unwrap();
    assert_eq!(discovery.candidates.len(), 1);
    assert_eq!(discovery.candidates[0].kind, HostKind::Desktop);
    assert!(discovery.candidates[0].verified);
    assert!(identity::discover_instance_candidate(&b.env, &b.id).is_err());
}

#[test]
fn unrelated_and_deceptive_argv_do_not_become_web_candidates() {
    for argv in [
        vec!["/usr/bin/other".into(), "web".into()],
        vec![
            EXE.into(),
            "--expose-internals".into(),
            CLI.into(),
            "web-preview".into(),
        ],
        vec!["/usr/bin/node".into(), "/tmp/not-dsh".into(), "web".into()],
    ] {
        let b = Bench::new(argv, true, PID);
        let discovery = identity::discover_detailed(&b.env).unwrap();
        assert!(discovery.candidates.is_empty());
        assert!(identity::discover_instance_candidate(&b.env, &b.id).is_err());
    }
}

fn assert_not_verified_web(b: &Bench) {
    let discovery = identity::discover_detailed(&b.env).unwrap();
    assert!(
        discovery
            .candidates
            .iter()
            .all(|candidate| candidate.kind != HostKind::Web || !candidate.verified),
        "不得将混淆的命令行识别为已核验 Web：{discovery:?}"
    );
    assert!(identity::discover_instance_candidate(&b.env, &b.id).is_err());
}

#[test]
fn actual_web_argv_with_different_executable_is_not_verified_web() {
    let mut b = Bench::new(web_argv(), true, PID);
    let original_argv = b.env.processes.list_processes()[0].cmdline.clone();
    b.replace_actual_executable();
    assert_eq!(b.env.processes.list_processes()[0].cmdline, original_argv);
    assert_not_verified_web(&b);
}

#[test]
fn actual_web_argv_with_different_executable_is_not_authorized() {
    let mut b = Bench::new(web_argv(), true, PID);
    let original_argv = b.env.processes.list_processes()[0].cmdline.clone();
    b.replace_actual_executable();
    assert_eq!(b.env.processes.list_processes()[0].cmdline, original_argv);
    let service = DshAuthService::new_for_instance(b.env.clone(), &b.id).unwrap();
    assert!(service.current_generation_key_digest().is_err());
}

#[test]
fn unrelated_executable_with_bundled_cli_web_arguments_is_not_verified_web() {
    let mut b = Bench::new(web_argv(), true, PID);
    let executable = b.root.join("other");
    std::fs::write(&executable, b"fixture").unwrap();
    let mut process = b.env.processes.list_processes().remove(0);
    process.exe = executable.clone();
    process.cmdline[0] = executable.to_string_lossy().into_owned();
    b.env.processes = Arc::new(Processes {
        process,
        listeners: vec![DshListener {
            owner_pid: PID,
            authority: AUTHORITY.into(),
        }],
    });
    b.env.environments = Arc::new(Evidence {
        executable,
        home: b.home.clone(),
        stable: true,
    });
    assert_not_verified_web(&b);
}

#[test]
fn bundled_cli_as_another_scripts_argument_is_not_verified_web() {
    let argv = [EXE, "--expose-internals", "/tmp/other.js", CLI, "web"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    assert_not_verified_web(&Bench::new(argv, true, PID));
}

#[test]
fn desktop_entry_mixed_with_bundled_cli_web_is_not_verified_web() {
    let argv = [EXE, "--expose-internals", DESKTOP, CLI, "web"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    assert_not_verified_web(&Bench::new(argv, true, PID));
}

#[test]
fn desktop_profile_mixed_with_bundled_cli_web_is_not_verified_web() {
    let mut b = Bench::new(web_argv(), true, PID);
    let mut process = b.env.processes.list_processes().remove(0);
    process.cmdline.push(
        b.home
            .join("profiles/desktop")
            .to_string_lossy()
            .into_owned(),
    );
    b.env.processes = Arc::new(Processes {
        process,
        listeners: vec![DshListener {
            owner_pid: PID,
            authority: AUTHORITY.into(),
        }],
    });
    assert_not_verified_web(&b);
}

// 合成 asar，不执行其中的 JS；每个条目在数据区占据独立片段。
struct AsarFixture<'a> {
    host_name: Option<&'a str>,
    cli: Option<&'a [u8]>,
    package_name: Option<&'a str>,
    bin: Option<&'a [u8]>,
    alias_bin_to_cli: bool,
}

impl Default for AsarFixture<'_> {
    fn default() -> Self {
        Self {
            host_name: Some("@deepseek-ai/dsh-desktop-host"),
            cli: Some(b"import '@deepseek-ai/dsh/lib/bin.js';\n"),
            package_name: Some("@deepseek-ai/dsh"),
            bin: Some(b"// synthetic dsh bin\n"),
            alias_bin_to_cli: false,
        }
    }
}

fn write_asar_fixture(path: &Path, package_name: Option<&str>) {
    write_asar_variant(
        path,
        AsarFixture {
            package_name,
            ..Default::default()
        },
    );
}

fn write_asar_variant(path: &Path, fixture: AsarFixture<'_>) {
    let mut data = Vec::new();
    let mut entry = |bytes: &[u8]| {
        let metadata = serde_json::json!({"size": bytes.len(), "offset": data.len().to_string()});
        data.extend_from_slice(bytes);
        metadata
    };
    let mut host = serde_json::json!({"files": {"lib": {"files": {}}}});
    if let Some(name) = fixture.host_name {
        host["files"]["package.json"] =
            entry(serde_json::json!({"name": name}).to_string().as_bytes());
    }
    if let Some(cli) = fixture.cli {
        host["files"]["lib"]["files"]["cli.js"] = entry(cli);
    }
    let mut scope = serde_json::json!({"dsh-desktop-host": host});
    if let Some(name) = fixture.package_name {
        let mut dsh = serde_json::json!({"files": {"lib": {"files": {}}}});
        dsh["files"]["package.json"] = entry(
            serde_json::json!({"name": name, "bin": {"dsh": "lib/bin.js"}})
                .to_string()
                .as_bytes(),
        );
        if let Some(bin) = fixture.bin {
            let mut bin_entry = entry(bin);
            if fixture.alias_bin_to_cli {
                bin_entry["offset"] =
                    scope["dsh-desktop-host"]["files"]["lib"]["files"]["cli.js"]["offset"].clone();
            }
            dsh["files"]["lib"]["files"]["bin.js"] = bin_entry;
        }
        scope["dsh"] = dsh;
    }
    let index = serde_json::json!({"files": {"dsh": {"files": {
        "node_modules": {"files": {"@deepseek-ai": {"files": scope}}}
    }}}});
    let header = serde_json::to_vec(&index).unwrap();
    let padded_len = (header.len() + 3) & !3;
    let mut archive = Vec::new();
    for word in [
        4u32,
        (8 + padded_len) as u32,
        (4 + padded_len) as u32,
        header.len() as u32,
    ] {
        archive.extend_from_slice(&word.to_le_bytes());
    }
    archive.extend_from_slice(&header);
    archive.resize(16 + padded_len, 0);
    archive.extend_from_slice(&data);
    std::fs::write(path, archive).unwrap();
}

// 2026-11-12 只读采样的官方 wrapper 原文，不执行它。
const OFFICIAL_WRAPPER: &str = r##"#!/bin/sh
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

fn seed_bundled_wrapper(b: &Bench, app_name: &str) -> (PathBuf, PathBuf) {
    let contents = b.root.join(app_name).join("Contents");
    let resources = contents.join("Resources");
    let wrapper = resources.join("runtime/cli/bin/dsh");
    let archive = resources.join("app.asar");
    let executable = contents.join("MacOS/DeepSeek Harness");
    std::fs::create_dir_all(wrapper.parent().unwrap()).unwrap();
    std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
    std::fs::write(&executable, b"fixture").unwrap();
    write_asar_fixture(&archive, Some("@deepseek-ai/dsh"));
    std::fs::write(&wrapper, OFFICIAL_WRAPPER).unwrap();
    (wrapper, archive)
}

fn assert_cli_source_unverified(b: &Bench) {
    let err = identity::web_cli_source(&b.env, None).expect_err("不得采信无法核验的 CLI 来源");
    assert_eq!(err.code, codes::CLI_SOURCE_UNVERIFIED, "{err}");
}

#[test]
fn bundled_web_host_finds_verified_wrapper_without_path() {
    let b = Bench::new(web_argv(), true, PID);
    let (wrapper, _) = seed_bundled_wrapper(&b, "DeepSeek Harness.app");
    assert!(
        identity::discover_instance_candidate(&b.env, &b.id)
            .unwrap()
            .verified
    );
    assert_eq!(
        identity::web_cli_source(&b.env, None).expect("同一安装内的官方 wrapper 应可用"),
        wrapper
    );
}

#[test]
fn bundled_web_host_without_wrapper_rejects_cli_source() {
    let b = Bench::new(web_argv(), true, PID);
    assert!(
        identity::discover_instance_candidate(&b.env, &b.id)
            .unwrap()
            .verified
    );
    assert_cli_source_unverified(&b);
}

#[test]
fn bundled_web_host_rejects_wrapper_with_invalid_package_anchor() {
    let b = Bench::new(web_argv(), true, PID);
    let (_, archive) = seed_bundled_wrapper(&b, "DeepSeek Harness.app");
    write_asar_fixture(&archive, Some("impostor"));
    assert_cli_source_unverified(&b);
}

#[test]
fn bundled_web_host_rejects_wrapper_with_missing_package_anchor() {
    let b = Bench::new(web_argv(), true, PID);
    let (_, archive) = seed_bundled_wrapper(&b, "DeepSeek Harness.app");
    write_asar_fixture(&archive, None);
    assert_cli_source_unverified(&b);
}

// 每个负例先证明同一个布局的正常归档可用，再仅替换一个归档内条件。
fn assert_invalid_asar_rejected(fixture: AsarFixture<'_>) {
    let b = Bench::new(web_argv(), true, PID);
    let (wrapper, archive) = seed_bundled_wrapper(&b, "DeepSeek Harness.app");
    assert_eq!(identity::web_cli_source(&b.env, None).unwrap(), wrapper);
    write_asar_variant(&archive, fixture);
    assert_cli_source_unverified(&b);
}

#[test]
fn bundled_web_host_rejects_missing_desktop_host_package_metadata() {
    assert_invalid_asar_rejected(AsarFixture {
        host_name: None,
        ..Default::default()
    });
}

#[test]
fn bundled_web_host_rejects_wrong_desktop_host_package_name() {
    assert_invalid_asar_rejected(AsarFixture {
        host_name: Some("@deepseek-ai/impostor"),
        ..Default::default()
    });
}

#[test]
fn bundled_web_host_rejects_missing_desktop_host_cli() {
    assert_invalid_asar_rejected(AsarFixture {
        cli: None,
        ..Default::default()
    });
}

#[test]
fn bundled_web_host_rejects_cli_importing_a_different_bin() {
    assert_invalid_asar_rejected(AsarFixture {
        cli: Some(b"import '@deepseek-ai/dsh/lib/other.js';\n"),
        ..Default::default()
    });
}

#[test]
fn bundled_web_host_rejects_missing_dsh_bin() {
    assert_invalid_asar_rejected(AsarFixture {
        bin: None,
        ..Default::default()
    });
}

#[test]
fn bundled_web_host_rejects_empty_dsh_bin() {
    assert_invalid_asar_rejected(AsarFixture {
        bin: Some(b""),
        ..Default::default()
    });
}

#[test]
fn bundled_web_host_rejects_bin_overlapping_desktop_host_cli() {
    assert_invalid_asar_rejected(AsarFixture {
        alias_bin_to_cli: true,
        ..Default::default()
    });
}

#[test]
fn bundled_web_host_rejects_wrapper_calling_a_different_entry() {
    let b = Bench::new(web_argv(), true, PID);
    let (wrapper, _) = seed_bundled_wrapper(&b, "DeepSeek Harness.app");
    std::fs::write(
        &wrapper,
        OFFICIAL_WRAPPER.replace(
            "dsh-desktop-host/lib/cli.js",
            "dsh-desktop-host/lib/other.js",
        ),
    )
    .unwrap();
    assert_cli_source_unverified(&b);
}

#[test]
fn bundled_web_host_rejects_wrapper_from_a_different_installation() {
    let mut b = Bench::new(web_argv(), true, PID);
    let (other_wrapper, _) = seed_bundled_wrapper(&b, "Other.app");
    let mut process = b.env.processes.list_processes().remove(0);
    process
        .cmdline
        .push(other_wrapper.to_string_lossy().into_owned());
    b.env.processes = Arc::new(Processes {
        process,
        listeners: vec![DshListener {
            owner_pid: PID,
            authority: AUTHORITY.into(),
        }],
    });
    assert!(
        identity::discover_instance_candidate(&b.env, &b.id)
            .unwrap()
            .verified
    );
    assert_cli_source_unverified(&b);
}

#[test]
fn actual_web_argv_without_stable_identity_or_owned_listener_is_not_authorized() {
    for (stable, owner) in [(false, PID), (true, PID + 1)] {
        let b = Bench::new(web_argv(), stable, owner);
        let discovery = identity::discover_detailed(&b.env).unwrap();
        assert!(discovery.candidates.iter().all(|c| !c.verified));
        assert!(identity::discover_instance_candidate(&b.env, &b.id).is_err());
        let service = DshAuthService::new_for_instance(b.env.clone(), &b.id).unwrap();
        assert!(service.current_generation_key_digest().is_err());
    }
}

// 管理作业经生产实例装配内核；进程/环境/CLI 是注入边界，磁盘是临时目录。
mod management_regressions {
    use super::*;
    use multi_agents_manager_lib::services::dsh_control::install::{
        embedded_manifest, BindingRevoker, DshPluginManager, PACKAGE_NAME,
    };
    use multi_agents_manager_lib::services::dsh_control::instances::SelectedInstance;
    use multi_agents_manager_lib::services::dsh_control::model::{
        ManagementJob, ManagementKind, ManagementPhase,
    };
    use multi_agents_manager_lib::services::dsh_control::production::init_instance_manager;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    struct Hosts(Mutex<Vec<DshProcess>>);
    impl DshProcessSource for Hosts {
        fn list_processes(&self) -> Vec<DshProcess> {
            self.0.lock().unwrap().clone()
        }
        fn listening_sockets(&self) -> Vec<DshListener> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .map(|p| DshListener {
                    owner_pid: p.pid,
                    authority: format!("127.0.0.1:{}", 5200 + p.pid - PID),
                })
                .collect()
        }
    }
    struct Environments(Vec<(u32, PathBuf, PathBuf)>);
    impl ProcessEnvironmentReader for Environments {
        fn observed_environment(&self, pid: u32) -> Option<ProcessEnvironmentEvidence> {
            let (_, executable, home) = self.0.iter().find(|(p, _, _)| *p == pid)?;
            Evidence {
                executable: executable.clone(),
                home: home.clone(),
                stable: true,
            }
            .observed_environment(pid)
        }
    }
    struct Revoker;
    impl BindingRevoker for Revoker {
        fn revoke_profile(&self, _: &str) -> Result<(), DshError> {
            Ok(())
        }
    }
    #[derive(Default)]
    struct Runner(Mutex<Vec<CommandRequest>>);
    impl DshCommandRunner for Runner {
        fn run(&self, request: &CommandRequest) -> Result<RunOutcome, DshError> {
            self.0.lock().unwrap().push(request.clone());
            let home = request
                .env
                .iter()
                .find(|(k, _)| k == "DSH_HOME")
                .unwrap()
                .1
                .clone();
            let profile = PathBuf::from(home).join("profiles").join(&request.args[2]);
            let manifest_path = profile.join("package.json");
            let mut manifest: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
            let package = profile.join("node_modules/@tuvis/dsh-remote-access");
            let op = request.args[3].to_string_lossy();
            if op == "add" {
                manifest["dependencies"][PACKAGE_NAME] = serde_json::json!("file:fixture");
                manifest["dsh"]["profile"]["bundles"] = serde_json::json!([PACKAGE_NAME]);
                std::fs::create_dir_all(&package).unwrap();
                std::fs::write(
                    package.join("package.json"),
                    serde_json::json!({"version": embedded_manifest().unwrap().plugin_version})
                        .to_string(),
                )
                .unwrap();
                std::fs::write(&manifest_path, manifest.to_string()).unwrap();
            } else if op == "remove" {
                manifest["dependencies"]
                    .as_object_mut()
                    .unwrap()
                    .remove(PACKAGE_NAME);
                manifest["dsh"]["profile"]["bundles"] = serde_json::json!([]);
                if package.exists() {
                    std::fs::remove_dir_all(&package).unwrap();
                }
                std::fs::write(&manifest_path, manifest.to_string()).unwrap();
            }
            let stdout = if op == "list" && package.exists() {
                format!(
                    "└── {}@{}\n",
                    PACKAGE_NAME,
                    embedded_manifest().unwrap().plugin_version
                )
            } else {
                String::new()
            };
            Ok(RunOutcome {
                exit_code: 0,
                stdout,
                stderr: String::new(),
            })
        }
    }
    struct Fixture {
        bench: Bench,
        hosts: Arc<Hosts>,
        runner: Arc<Runner>,
        wrapper_a: PathBuf,
        archive_a: PathBuf,
        home_b: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let mut bench = Bench::new(web_argv(), true, PID);
            let (wrapper_a, archive_a) = seed_bundled_wrapper(&bench, "DeepSeek Harness.app");
            let (wrapper_b, _) = seed_bundled_wrapper(&bench, "Other.app");
            let home_b = bench.root.join("home-b");
            for home in [&bench.home, &home_b] {
                let profile = home.join("profiles/web");
                std::fs::create_dir_all(&profile).unwrap();
                std::fs::write(
                    profile.join("package.json"),
                    br#"{"dependencies":{},"dsh":{"profile":{"bundles":[]}}}"#,
                )
                .unwrap();
            }
            let a = bench.env.processes.list_processes().remove(0);
            let executable_b = wrapper_b
                .ancestors()
                .nth(5)
                .unwrap()
                .join("MacOS/DeepSeek Harness");
            let resources_b = executable_b
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("Resources");
            let b = DshProcess {
                pid: PID + 1,
                exe: executable_b.clone(),
                started_at_ms: START,
                cmdline: vec![
                    executable_b.to_string_lossy().into_owned(),
                    "--expose-internals".into(),
                    resources_b
                        .join("app.asar/dsh/node_modules/@deepseek-ai/dsh-desktop-host/lib/cli.js")
                        .to_string_lossy()
                        .into_owned(),
                    "web".into(),
                ],
            };
            let hosts = Arc::new(Hosts(Mutex::new(vec![b, a.clone()])));
            let runner = Arc::new(Runner::default());
            bench.env.processes = hosts.clone();
            bench.env.runner = runner.clone();
            bench.env.environments = Arc::new(Environments(vec![
                (PID, a.exe.clone(), bench.home.clone()),
                (PID + 1, executable_b, home_b.clone()),
            ]));
            Self {
                bench,
                hosts,
                runner,
                wrapper_a,
                archive_a,
                home_b,
            }
        }
        fn manager(&self) -> DshPluginManager {
            let target = SelectedInstance {
                instance_id: self.bench.id.clone(),
                kind: HostKind::Web,
                profile_dir: self.bench.home.join("profiles/web"),
                dsh_home: Some(self.bench.home.clone()),
                identity_fingerprint: String::new(),
                read_evidence_fingerprint: String::new(),
            };
            init_instance_manager(&self.bench.env, &target, Arc::new(Revoker))
        }
        fn stop_hosts(&self) {
            self.hosts.0.lock().unwrap().clear();
        }
    }
    async fn wait(m: &DshPluginManager, id: &str, terminal: bool) -> ManagementJob {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let job = m.job(id).unwrap();
            if if terminal {
                matches!(
                    job.phase,
                    ManagementPhase::Succeeded
                        | ManagementPhase::Failed
                        | ManagementPhase::Cancelled
                )
            } else {
                job.phase == ManagementPhase::WaitingForDesktopExit
            } {
                return job;
            }
            assert!(Instant::now() < deadline, "作业超时: {job:?}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    async fn start_and_stop(
        f: &Fixture,
        m: &DshPluginManager,
        kind: ManagementKind,
    ) -> ManagementJob {
        let id = m.start(kind).unwrap();
        wait(m, &id, false).await;
        assert!(f.runner.0.lock().unwrap().is_empty());
        f.stop_hosts();
        wait(m, &id, true).await
    }
    #[tokio::test]
    async fn web_exit_gate_ignores_other_proven_instance_after_target_exits() {
        let f = Fixture::new();
        let m = f.manager();
        let id = m.start(ManagementKind::Install).unwrap();
        wait(&m, &id, false).await;
        assert!(f.runner.0.lock().unwrap().is_empty());
        f.hosts.0.lock().unwrap().retain(|p| p.pid != PID);
        let job = wait(&m, &id, true).await;
        assert_eq!(job.phase, ManagementPhase::Succeeded, "{job:?}");
        assert_eq!(f.hosts.0.lock().unwrap().len(), 1);
        assert!(!f.runner.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn web_exit_gate_waits_when_remaining_web_host_cannot_be_attributed() {
        let f = Fixture::new();
        let original = f
            .hosts
            .0
            .lock()
            .unwrap()
            .iter()
            .find(|p| p.pid == PID)
            .unwrap()
            .clone();
        let m = f.manager();
        let id = m.start(ManagementKind::Install).unwrap();
        wait(&m, &id, false).await;
        {
            let mut hosts = f.hosts.0.lock().unwrap();
            hosts.retain(|p| p.pid != PID);
            hosts[0].pid = PID + 2;
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let job = m.job(&id).unwrap();
            assert_eq!(job.phase, ManagementPhase::WaitingForDesktopExit, "{job:?}");
            if job.reason_code.as_deref() == Some(codes::INSTANCE_UNVERIFIED) {
                break;
            }
            assert!(Instant::now() < deadline, "等待不可归属原因超时: {job:?}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(f.runner.0.lock().unwrap().is_empty());
        *f.hosts.0.lock().unwrap() = vec![original];
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let job = m.job(&id).unwrap();
            assert_eq!(job.phase, ManagementPhase::WaitingForDesktopExit, "{job:?}");
            if job.reason_code.as_deref() == Some(codes::DESKTOP_EXIT_PENDING) {
                break;
            }
            assert!(Instant::now() < deadline, "等待已归属原因超时: {job:?}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        m.cancel(&id).unwrap();
        assert_eq!(wait(&m, &id, true).await.phase, ManagementPhase::Cancelled);
    }

    #[tokio::test]
    async fn selected_a_uses_only_a_wrapper_and_web_home_for_add_remove_and_list() {
        let f = Fixture::new();
        let m = f.manager();
        let job = start_and_stop(&f, &m, ManagementKind::Install).await;
        assert_eq!(job.phase, ManagementPhase::Succeeded, "{job:?}");
        let install = f.runner.0.lock().unwrap().clone();
        assert_eq!(install.len(), 2);
        assert_eq!(install[0].program, f.wrapper_a);
        assert_eq!(
            &install[0].args[..4],
            &["plugin", "--profile", "web", "add"]
        );
        assert_eq!(install[0].args.len(), 5);
        assert!(Path::new(&install[0].args[4]).is_absolute());
        assert_eq!(install[1].args, ["plugin", "--profile", "web", "list"]);
        assert_eq!(
            std::fs::read_to_string(f.home_b.join("profiles/web/package.json")).unwrap(),
            r#"{"dependencies":{},"dsh":{"profile":{"bundles":[]}}}"#
        );
        // 再启动活体，确保卸载也从同一目标锚定来源。
        let a = f
            .bench
            .env
            .environments
            .observed_environment(PID)
            .unwrap()
            .identity_before
            .executable;
        let resources = a.parent().unwrap().parent().unwrap().join("Resources");
        f.hosts.0.lock().unwrap().push(DshProcess {
            pid: PID,
            exe: a.clone(),
            started_at_ms: START,
            cmdline: vec![
                a.to_string_lossy().into_owned(),
                "--expose-internals".into(),
                resources
                    .join("app.asar/dsh/node_modules/@deepseek-ai/dsh-desktop-host/lib/cli.js")
                    .to_string_lossy()
                    .into_owned(),
                "web".into(),
            ],
        });
        f.runner.0.lock().unwrap().clear();
        let job = start_and_stop(&f, &m, ManagementKind::Uninstall).await;
        assert_eq!(job.phase, ManagementPhase::Succeeded, "{job:?}");
        let remove = f.runner.0.lock().unwrap().clone();
        assert_eq!(remove.len(), 2);
        assert_eq!(
            remove[0].args,
            ["plugin", "--profile", "web", "remove", PACKAGE_NAME]
        );
        assert_eq!(remove[1].args, ["plugin", "--profile", "web", "list"]);
        for call in install.iter().chain(remove.iter()) {
            assert_eq!(call.program, f.wrapper_a);
            assert_eq!(
                call.env,
                vec![("DSH_HOME".into(), f.bench.home.as_os_str().to_owned())]
            );
        }
    }
    #[tokio::test]
    async fn missing_a_wrapper_does_not_borrow_b() {
        let f = Fixture::new();
        std::fs::remove_file(&f.wrapper_a).unwrap();
        let m = f.manager();
        let job = start_and_stop(&f, &m, ManagementKind::Install).await;
        assert_eq!(job.phase, ManagementPhase::Failed, "{job:?}");
        assert_eq!(
            job.reason_code.as_deref(),
            Some(codes::CLI_SOURCE_UNVERIFIED)
        );
        assert!(f.runner.0.lock().unwrap().is_empty());
    }
    // PATH 来源使用标准 npm 全局布局，入口链接到包声明的 CLI。
    fn seed_path_npm_cli(b: &Bench) -> PathBuf {
        use std::os::unix::fs::symlink;

        let prefix = b.root.join("path-npm");
        let package = prefix.join("lib/node_modules/@deepseek-ai/dsh");
        let bin = package.join("bin/cli.js");
        let entry = prefix.join("bin/dsh");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::create_dir_all(entry.parent().unwrap()).unwrap();
        std::fs::write(
            package.join("package.json"),
            r#"{"name":"@deepseek-ai/dsh","bin":{"dsh":"bin/cli.js"}}"#,
        )
        .unwrap();
        std::fs::write(&bin, b"#!/usr/bin/env node\n// synthetic npm cli\n").unwrap();
        symlink(&bin, &entry).unwrap();
        entry
    }

    #[tokio::test]
    async fn missing_a_wrapper_does_not_borrow_verified_path() {
        const CHILD: &str = "TUVIS_TEST_BUNDLED_CLI_PATH_CHILD";
        if std::env::var_os(CHILD).is_none() {
            // 子进程单独设置 PATH，避免与并发测试共享进程环境互踩。
            let b = Bench::new(web_argv(), true, PID);
            let wrapper = seed_path_npm_cli(&b);
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "management_regressions::missing_a_wrapper_does_not_borrow_verified_path",
                    "--nocapture",
                ])
                .env(CHILD, &wrapper)
                .env("PATH", wrapper.parent().unwrap())
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "子进程失败：{}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }
        let f = Fixture::new();
        f.hosts.0.lock().unwrap().retain(|p| p.pid == PID);
        std::fs::remove_file(&f.wrapper_a).unwrap();
        let from_path =
            identity::web_cli_source(&f.bench.env, std::env::var_os("PATH").as_deref()).unwrap();
        assert_eq!(
            from_path,
            PathBuf::from(std::env::var_os(CHILD).unwrap()),
            "PATH 诱饵必须是可核验的独立来源"
        );
        let m = f.manager();
        let job = start_and_stop(&f, &m, ManagementKind::Install).await;
        assert_eq!(job.phase, ManagementPhase::Failed, "{job:?}");
        assert_eq!(
            job.reason_code.as_deref(),
            Some(codes::CLI_SOURCE_UNVERIFIED)
        );
        assert!(f.runner.0.lock().unwrap().is_empty());
    }
    async fn replaced_source_is_not_executed(replace_archive: bool) {
        let f = Fixture::new();
        // 仅保留 A，避免 B 来源掩盖等待窗口的替换行为。
        f.hosts.0.lock().unwrap().retain(|p| p.pid == PID);
        let m = f.manager();
        let id = m.start(ManagementKind::Install).unwrap();
        wait(&m, &id, false).await;
        let path = if replace_archive {
            &f.archive_a
        } else {
            &f.wrapper_a
        };
        std::fs::remove_file(path).unwrap();
        std::fs::write(path, b"replaced after job start").unwrap();
        f.stop_hosts();
        let job = wait(&m, &id, true).await;
        assert_eq!(job.phase, ManagementPhase::Failed, "{job:?}");
        assert!(f.runner.0.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn wrapper_replaced_while_waiting_is_not_executed() {
        replaced_source_is_not_executed(false).await;
    }
    #[tokio::test]
    async fn asar_replaced_while_waiting_is_not_executed() {
        replaced_source_is_not_executed(true).await;
    }

    #[tokio::test]
    async fn valid_asar_with_changed_contents_while_waiting_is_not_executed() {
        let f = Fixture::new();
        f.hosts.0.lock().unwrap().retain(|p| p.pid == PID);
        let m = f.manager();
        let id = m.start(ManagementKind::Install).unwrap();
        wait(&m, &id, false).await;
        assert!(f.runner.0.lock().unwrap().is_empty());
        // 保持索引、包名、入口和导入关系合法，只改变 bin 的内容。
        write_asar_variant(
            &f.archive_a,
            AsarFixture {
                bin: Some(b"// different but valid synthetic dsh bin\n"),
                ..Default::default()
            },
        );
        assert_eq!(
            identity::web_cli_source(&f.bench.env, None).unwrap(),
            f.wrapper_a
        );
        f.stop_hosts();
        let job = wait(&m, &id, true).await;
        assert_eq!(job.phase, ManagementPhase::Failed, "{job:?}");
        assert!(f.runner.0.lock().unwrap().is_empty());
    }

    fn ordinary_a(f: &mut Fixture) {
        let exe = f.bench.root.join("node");
        std::fs::write(&exe, b"fixture").unwrap();
        let a = DshProcess {
            pid: PID,
            exe: exe.clone(),
            cmdline: vec![
                exe.to_string_lossy().into_owned(),
                "/fixture/dsh".into(),
                "web".into(),
            ],
            started_at_ms: START,
        };
        let mut hosts = f.hosts.0.lock().unwrap();
        hosts.retain(|p| p.pid != PID);
        hosts.push(a);
        drop(hosts);
        let b_exe = f
            .hosts
            .0
            .lock()
            .unwrap()
            .iter()
            .find(|p| p.pid == PID + 1)
            .unwrap()
            .exe
            .clone();
        f.bench.env.environments = Arc::new(Environments(vec![
            (PID, exe, f.bench.home.clone()),
            (PID + 1, b_exe, f.home_b.clone()),
        ]));
    }

    #[tokio::test]
    async fn ordinary_selected_web_does_not_borrow_running_bundled_host() {
        const CHILD: &str = "TUVIS_TEST_ORDINARY_WEB_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "management_regressions::ordinary_selected_web_does_not_borrow_running_bundled_host", "--nocapture"])
                .env(CHILD, "1")
                .env("PATH", "")
                .output().unwrap();
            assert!(
                result.status.success(),
                "子进程失败：{}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }
        let mut f = Fixture::new();
        ordinary_a(&mut f);
        assert!(
            identity::discover_instance_candidate(&f.bench.env, &f.bench.id)
                .unwrap()
                .verified
        );
        let m = f.manager();
        let job = start_and_stop(&f, &m, ManagementKind::Install).await;
        assert_eq!(job.phase, ManagementPhase::Failed, "{job:?}");
        assert_eq!(
            job.reason_code.as_deref(),
            Some(codes::CLI_SOURCE_UNVERIFIED)
        );
        assert!(f.runner.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn stopped_ordinary_web_uses_verified_path_source() {
        const CHILD: &str = "TUVIS_TEST_STOPPED_WEB_PATH_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let b = Bench::new(web_argv(), true, PID);
            let wrapper = seed_path_npm_cli(&b);
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "management_regressions::stopped_ordinary_web_uses_verified_path_source",
                    "--nocapture",
                ])
                .env(CHILD, &wrapper)
                .env("PATH", wrapper.parent().unwrap())
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "子进程失败：{}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }
        let mut f = Fixture::new();
        ordinary_a(&mut f);
        assert!(
            identity::discover_instance_candidate(&f.bench.env, &f.bench.id)
                .unwrap()
                .verified
        );
        f.stop_hosts();
        let expected = PathBuf::from(std::env::var_os(CHILD).unwrap());
        assert_eq!(
            identity::web_cli_source(&f.bench.env, std::env::var_os("PATH").as_deref()).unwrap(),
            expected
        );
        let m = f.manager();
        let id = m.start(ManagementKind::Install).unwrap();
        let job = wait(&m, &id, true).await;
        assert_eq!(job.phase, ManagementPhase::Succeeded, "{job:?}");
        let calls = f.runner.0.lock().unwrap();
        assert!(!calls.is_empty());
        assert!(calls.iter().all(|call| call.program == expected
            && call.env == vec![("DSH_HOME".into(), f.bench.home.as_os_str().to_owned())]));
    }

    #[tokio::test]
    async fn offline_npm_source_remains_bound_after_manager_reconstruction_and_path_change() {
        const CHILD_A: &str = "TUVIS_TEST_OFFLINE_SOURCE_A";
        const CHILD_B: &str = "TUVIS_TEST_OFFLINE_SOURCE_B";
        if std::env::var_os(CHILD_A).is_none() {
            let a = Bench::new(web_argv(), true, PID);
            let b = Bench::new(web_argv(), true, PID);
            let npm_a = seed_path_npm_cli(&a);
            let npm_b = seed_path_npm_cli(&b);
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "management_regressions::offline_npm_source_remains_bound_after_manager_reconstruction_and_path_change",
                    "--nocapture",
                ])
                .env(CHILD_A, &npm_a)
                .env(CHILD_B, &npm_b)
                .env("PATH", npm_a.parent().unwrap())
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "子进程失败：{}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }

        let f = Fixture::new();
        f.stop_hosts();
        let npm_a = PathBuf::from(std::env::var_os(CHILD_A).unwrap());
        let npm_b = PathBuf::from(std::env::var_os(CHILD_B).unwrap());
        assert_ne!(npm_a, npm_b);
        assert_eq!(
            identity::web_cli_source(&f.bench.env, std::env::var_os("PATH").as_deref()).unwrap(),
            npm_a
        );
        let manager = f.manager();
        let id = manager.start(ManagementKind::Install).unwrap();
        let job = wait(&manager, &id, true).await;
        assert_eq!(job.phase, ManagementPhase::Succeeded, "{job:?}");
        {
            let calls = f.runner.0.lock().unwrap();
            assert_eq!(calls.len(), 2);
            assert!(calls.iter().all(|call| call.program == npm_a
                && call.env == vec![("DSH_HOME".into(), f.bench.home.as_os_str().to_owned())]));
            assert_eq!(&calls[0].args[..4], &["plugin", "--profile", "web", "add"]);
            assert_eq!(calls[0].args.len(), 5);
            assert!(Path::new(&calls[0].args[4]).is_absolute());
            assert_eq!(calls[1].args, ["plugin", "--profile", "web", "list"]);
        }
        drop(manager);
        f.runner.0.lock().unwrap().clear();
        // 测试进程由父进程独占启动，PATH 修改不会影响其他测试进程。
        std::env::set_var("PATH", npm_b.parent().unwrap());
        assert_eq!(
            identity::web_cli_source(&f.bench.env, std::env::var_os("PATH").as_deref()).unwrap(),
            npm_b,
            "B 必须是独立可核验的 PATH 来源"
        );
        let manager = f.manager();
        let id = manager.start(ManagementKind::Uninstall).unwrap();
        let job = wait(&manager, &id, true).await;
        let calls = f.runner.0.lock().unwrap();
        if job.phase == ManagementPhase::Succeeded {
            assert_eq!(calls.len(), 2);
            assert!(calls.iter().all(|call| call.program == npm_a
                && call.env == vec![("DSH_HOME".into(), f.bench.home.as_os_str().to_owned())]));
            assert_eq!(
                calls[0].args,
                ["plugin", "--profile", "web", "remove", PACKAGE_NAME]
            );
            assert_eq!(calls[1].args, ["plugin", "--profile", "web", "list"]);
        } else {
            assert_eq!(job.phase, ManagementPhase::Failed, "{job:?}");
            assert_eq!(
                job.reason_code.as_deref(),
                Some(codes::CLI_SOURCE_UNVERIFIED)
            );
            assert!(calls.is_empty(), "拒绝换源时不得调用 CLI");
        }
    }

    struct ChangingEvidence {
        change_after: usize,
        executable: PathBuf,
        home: PathBuf,
        changed_home: PathBuf,
        change_generation: bool,
        reads: std::sync::atomic::AtomicUsize,
    }
    impl ProcessEnvironmentReader for ChangingEvidence {
        fn observed_environment(&self, pid: u32) -> Option<ProcessEnvironmentEvidence> {
            if pid != PID {
                return None;
            }
            // 首次候选发现的两次环境观测一致，后续来源发现遇到复用的 PID。
            let changed =
                self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= self.change_after;
            let home = if changed && !self.change_generation {
                &self.changed_home
            } else {
                &self.home
            };
            let mut evidence = Evidence {
                executable: self.executable.clone(),
                home: home.clone(),
                stable: true,
            }
            .observed_environment(pid)?;
            if changed && self.change_generation {
                evidence
                    .identity_before
                    .native_generation
                    .as_mut()
                    .unwrap()
                    .pid_version += 1;
                evidence
                    .identity_after
                    .native_generation
                    .as_mut()
                    .unwrap()
                    .pid_version += 1;
            }
            Some(evidence)
        }
    }

    async fn changed_identity_during_source_discovery_is_not_executed(change_generation: bool) {
        let mut f = Fixture::new();
        f.hosts.0.lock().unwrap().retain(|p| p.pid == PID);
        assert!(
            identity::discover_instance_candidate(&f.bench.env, &f.bench.id)
                .unwrap()
                .verified
        );
        let exe = f.hosts.0.lock().unwrap()[0].exe.clone();
        f.bench.env.environments = Arc::new(ChangingEvidence {
            change_after: 2,
            executable: exe,
            home: f.bench.home.clone(),
            changed_home: f.home_b.clone(),
            change_generation,
            reads: std::sync::atomic::AtomicUsize::new(0),
        });
        let m = f.manager();
        let id = m.start(ManagementKind::Install).unwrap();
        if change_generation {
            wait(&m, &id, false).await;
            f.stop_hosts();
        }
        // home 变化后宿主已归属另一实例，目标可视为退出；来源仍不得采信。
        let job = wait(&m, &id, true).await;
        assert_eq!(job.phase, ManagementPhase::Failed, "{job:?}");
        assert_eq!(
            job.reason_code.as_deref(),
            Some(codes::CLI_SOURCE_UNVERIFIED)
        );
        assert!(f.runner.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn pid_reused_with_new_generation_during_discovery_does_not_acquire_source() {
        changed_identity_during_source_discovery_is_not_executed(true).await;
    }

    #[tokio::test]
    async fn pid_reused_with_new_home_during_discovery_does_not_acquire_source() {
        changed_identity_during_source_discovery_is_not_executed(false).await;
    }

    fn ledger_path(f: &Fixture) -> PathBuf {
        f.bench
            .env
            .tuvis_home
            .join("dsh/instances")
            .join(&f.bench.id)
            .join("management.json")
    }

    fn assert_no_saved_origin(f: &Fixture) {
        let ledger: serde_json::Value =
            serde_json::from_slice(&std::fs::read(ledger_path(f)).unwrap()).unwrap();
        assert!(
            ledger["cliOrigin"].is_null(),
            "未完成验证的来源不得入账: {ledger}"
        );
    }

    fn seed_npm_source(f: &mut Fixture, symlink: bool) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::symlink as make_symlink;
        ordinary_a(f);
        f.hosts.0.lock().unwrap().retain(|p| p.pid == PID);
        let prefix = f.bench.root.join("npm");
        let package = prefix.join("lib/node_modules/@deepseek-ai/dsh");
        let bin = package.join("bin/cli.js");
        let entry = prefix.join("bin/dsh");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::create_dir_all(entry.parent().unwrap()).unwrap();
        std::fs::write(
            package.join("package.json"),
            r#"{"name":"@deepseek-ai/dsh","bin":{"dsh":"bin/cli.js"}}"#,
        )
        .unwrap();
        std::fs::write(&bin, b"#!/usr/bin/env node\n// original\n").unwrap();
        if symlink {
            make_symlink(&bin, &entry).unwrap();
        } else {
            std::fs::write(
                &entry,
                format!("#!/bin/sh\nexec \"{}\" \"$@\"\n", bin.display()),
            )
            .unwrap();
        }
        f.hosts.0.lock().unwrap()[0].cmdline[1] = entry.to_string_lossy().into_owned();
        assert_eq!(identity::web_cli_source(&f.bench.env, None).unwrap(), entry);
        (entry, bin)
    }

    async fn npm_replacement_is_not_executed(symlink: bool, replace_bin: bool) {
        let mut f = Fixture::new();
        let (entry, bin) = seed_npm_source(&mut f, symlink);
        let m = f.manager();
        let id = m.start(ManagementKind::Install).unwrap();
        wait(&m, &id, false).await;
        assert!(f.runner.0.lock().unwrap().is_empty());
        if replace_bin {
            std::fs::remove_file(&bin).unwrap();
            std::fs::write(&bin, b"#!/usr/bin/env node\n// replaced\n").unwrap();
        } else {
            std::fs::remove_file(&entry).unwrap();
            std::fs::write(&entry, b"#!/bin/sh\nexit 0\n").unwrap();
        }
        f.stop_hosts();
        let job = wait(&m, &id, true).await;
        assert_eq!(job.phase, ManagementPhase::Failed, "{job:?}");
        assert_eq!(
            job.reason_code.as_deref(),
            Some(codes::CLI_SOURCE_UNVERIFIED)
        );
        assert!(f.runner.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn npm_shim_replaced_while_waiting_is_not_executed() {
        npm_replacement_is_not_executed(false, false).await;
    }

    #[tokio::test]
    async fn npm_symlink_replaced_while_waiting_is_not_executed() {
        npm_replacement_is_not_executed(true, false).await;
    }

    #[tokio::test]
    async fn npm_bin_replaced_while_waiting_is_not_executed() {
        npm_replacement_is_not_executed(true, true).await;
    }

    fn seed_npm_form_b(f: &mut Fixture) -> (PathBuf, PathBuf) {
        ordinary_a(f);
        f.hosts.0.lock().unwrap().retain(|p| p.pid == PID);
        let root = f.bench.root.join("form-b");
        let package = root.join("node_modules/@deepseek-ai/dsh");
        let bin = package.join("bin/cli.js");
        let host = root.join("MacOS/DeepSeek Harness");
        let entry = root.join("a/b/c/dsh");
        for path in [&bin, &host, &entry] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        }
        std::fs::write(
            package.join("package.json"),
            r#"{"name":"@deepseek-ai/dsh","bin":{"dsh":"bin/cli.js"}}"#,
        )
        .unwrap();
        std::fs::write(&bin, b"#!/usr/bin/env node\n// form B entry\n").unwrap();
        std::fs::write(&host, b"original host binary\n").unwrap();
        std::fs::write(
            &entry,
            format!(
                "#!/bin/sh\nELECTRON_RUN_AS_NODE=1 exec \"{}\" --expose-internals \"{}\" \"$@\"\n",
                host.display(),
                bin.display()
            ),
        )
        .unwrap();
        assert!(!host.starts_with(&package));
        f.hosts.0.lock().unwrap()[0].cmdline[1] = entry.to_string_lossy().into_owned();
        assert_eq!(identity::web_cli_source(&f.bench.env, None).unwrap(), entry);
        (entry, host)
    }

    #[tokio::test]
    async fn npm_form_b_host_replaced_while_waiting_is_not_executed() {
        let mut f = Fixture::new();
        let (entry, host) = seed_npm_form_b(&mut f);
        let shim = std::fs::read(&entry).unwrap();
        let manager = f.manager();
        let id = manager.start(ManagementKind::Install).unwrap();
        wait(&manager, &id, false).await;
        assert!(f.runner.0.lock().unwrap().is_empty());
        std::fs::remove_file(&host).unwrap();
        std::fs::write(&host, b"replacement host binary\n").unwrap();
        assert_eq!(std::fs::read(&entry).unwrap(), shim);
        assert_eq!(identity::web_cli_source(&f.bench.env, None).unwrap(), entry);
        f.stop_hosts();
        let job = wait(&manager, &id, true).await;
        assert_eq!(job.phase, ManagementPhase::Failed, "{job:?}");
        assert_eq!(
            job.reason_code.as_deref(),
            Some(codes::CLI_SOURCE_UNVERIFIED)
        );
        assert!(f.runner.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn unchanged_npm_form_b_host_allows_install() {
        let mut f = Fixture::new();
        let (entry, _) = seed_npm_form_b(&mut f);
        let manager = f.manager();
        let job = start_and_stop(&f, &manager, ManagementKind::Install).await;
        assert_eq!(job.phase, ManagementPhase::Succeeded, "{job:?}");
        let calls = f.runner.0.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(calls.iter().all(|call| call.program == entry
            && call.env == vec![("DSH_HOME".into(), f.bench.home.as_os_str().to_owned())]));
        assert_eq!(&calls[0].args[..4], &["plugin", "--profile", "web", "add"]);
        assert_eq!(calls[0].args.len(), 5);
        assert!(Path::new(&calls[0].args[4]).is_absolute());
        assert_eq!(calls[1].args, ["plugin", "--profile", "web", "list"]);
    }

    fn seed_npm_importing_implementation(f: &mut Fixture) -> (PathBuf, PathBuf, PathBuf) {
        let (entry, bin) = seed_npm_source(f, true);
        let implementation = bin.parent().unwrap().join("implementation.js");
        std::fs::write(
            &bin,
            b"#!/usr/bin/env node\nimport './implementation.js';\n",
        )
        .unwrap();
        std::fs::write(&implementation, b"// original implementation\n").unwrap();
        assert_eq!(identity::web_cli_source(&f.bench.env, None).unwrap(), entry);
        (entry, bin, implementation)
    }

    #[tokio::test]
    async fn npm_imported_implementation_replaced_while_waiting_is_not_executed() {
        let mut f = Fixture::new();
        let (entry, bin, implementation) = seed_npm_importing_implementation(&mut f);
        let manifest = bin.parent().unwrap().parent().unwrap().join("package.json");
        let entry_target = std::fs::read_link(&entry).unwrap();
        let bin_contents = std::fs::read(&bin).unwrap();
        let manifest_contents = std::fs::read(&manifest).unwrap();
        let m = f.manager();
        let id = m.start(ManagementKind::Install).unwrap();
        wait(&m, &id, false).await;
        assert!(f.runner.0.lock().unwrap().is_empty());

        std::fs::remove_file(&implementation).unwrap();
        std::fs::write(&implementation, b"// replaced implementation\n").unwrap();
        assert_eq!(std::fs::read_link(&entry).unwrap(), entry_target);
        assert_eq!(std::fs::read(&bin).unwrap(), bin_contents);
        assert_eq!(std::fs::read(&manifest).unwrap(), manifest_contents);
        assert_eq!(identity::web_cli_source(&f.bench.env, None).unwrap(), entry);
        f.stop_hosts();
        let job = wait(&m, &id, true).await;
        assert_eq!(job.phase, ManagementPhase::Failed, "{job:?}");
        assert_eq!(
            job.reason_code.as_deref(),
            Some(codes::CLI_SOURCE_UNVERIFIED)
        );
        assert!(f.runner.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn npm_unchanged_imported_implementation_allows_install() {
        let mut f = Fixture::new();
        let (entry, _, _) = seed_npm_importing_implementation(&mut f);
        let m = f.manager();
        let job = start_and_stop(&f, &m, ManagementKind::Install).await;
        assert_eq!(job.phase, ManagementPhase::Succeeded, "{job:?}");
        let calls = f.runner.0.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(calls.iter().all(|call| call.program == entry));
        assert_eq!(&calls[0].args[..4], &["plugin", "--profile", "web", "add"]);
        assert_eq!(calls[1].args, ["plugin", "--profile", "web", "list"]);
    }

    #[tokio::test]
    async fn invalid_bundled_source_is_not_saved_as_confirmed() {
        let f = Fixture::new();
        f.hosts.0.lock().unwrap().retain(|p| p.pid == PID);
        std::fs::remove_file(&f.wrapper_a).unwrap();
        let m = f.manager();
        let job = start_and_stop(&f, &m, ManagementKind::Install).await;
        assert_eq!(job.phase, ManagementPhase::Failed, "{job:?}");
        assert_eq!(
            job.reason_code.as_deref(),
            Some(codes::CLI_SOURCE_UNVERIFIED)
        );
        assert_no_saved_origin(&f);
        assert!(f.runner.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn generation_change_at_final_source_check_is_not_saved() {
        let mut f = Fixture::new();
        f.hosts.0.lock().unwrap().retain(|p| p.pid == PID);
        let exe = f.hosts.0.lock().unwrap()[0].exe.clone();
        f.bench.env.environments = Arc::new(ChangingEvidence {
            change_after: 3,
            executable: exe,
            home: f.bench.home.clone(),
            changed_home: f.home_b.clone(),
            change_generation: true,
            reads: std::sync::atomic::AtomicUsize::new(0),
        });
        let m = f.manager();
        let job = start_and_stop(&f, &m, ManagementKind::Install).await;
        assert_eq!(job.phase, ManagementPhase::Failed, "{job:?}");
        assert_eq!(
            job.reason_code.as_deref(),
            Some(codes::CLI_SOURCE_UNVERIFIED)
        );
        assert_no_saved_origin(&f);
        assert!(f.runner.0.lock().unwrap().is_empty());
    }

    #[cfg(feature = "dsh-test-probes")]
    static MANAGEMENT_WRITE_PROBE_LOCK: Mutex<()> = Mutex::new(());

    #[cfg(feature = "dsh-test-probes")]
    struct ClearManagementWriteProbe;
    #[cfg(feature = "dsh-test-probes")]
    impl Drop for ClearManagementWriteProbe {
        fn drop(&mut self) {
            multi_agents_manager_lib::services::dsh_control::install::ledger_commit_probe::set_fail_write(None);
        }
    }

    #[cfg(feature = "dsh-test-probes")]
    #[tokio::test]
    // 进程内写入探针为单例，跨 await 持锁以串行隔离探针生命周期。
    #[allow(clippy::await_holding_lock)]
    async fn failed_origin_persist_does_not_leak_from_memory_into_later_receipts() {
        let f = Fixture::new();
        f.hosts.0.lock().unwrap().retain(|p| p.pid == PID);
        let _serial = MANAGEMENT_WRITE_PROBE_LOCK.lock().unwrap();
        let _clear = ClearManagementWriteProbe;
        use multi_agents_manager_lib::services::dsh_control::install::ledger_commit_probe;
        // 初始回执、cliUnresolved、cliPending 依次成功，第四次 cliOrigin 提交失败。
        ledger_commit_probe::set_fail_write_after(&ledger_path(&f), 3);
        let m = f.manager();
        let hosts = f.hosts.0.lock().unwrap().clone();
        let failed = m.start(ManagementKind::Install).unwrap();
        wait(&m, &failed, false).await;
        assert!(f.runner.0.lock().unwrap().is_empty());
        assert_no_saved_origin(&f);
        let before_recovery: serde_json::Value =
            serde_json::from_slice(&std::fs::read(ledger_path(&f)).unwrap()).unwrap();
        assert!(
            !before_recovery["cliPending"].is_null(),
            "origin 提交前 pending 必须已落盘"
        );
        assert_eq!(before_recovery["cliUnresolved"], true);
        ledger_commit_probe::set_fail_write(None);
        f.stop_hosts();
        let job = wait(&m, &failed, true).await;
        assert_eq!(job.phase, ManagementPhase::Failed, "{job:?}");
        assert_eq!(
            job.reason_code.as_deref(),
            Some(codes::CLI_SOURCE_UNVERIFIED)
        );
        assert_no_saved_origin(&f);
        let after_recovery: serde_json::Value =
            serde_json::from_slice(&std::fs::read(ledger_path(&f)).unwrap()).unwrap();
        assert_eq!(
            after_recovery["cliPending"], before_recovery["cliPending"],
            "origin 写入失败后必须回滚 pending"
        );
        assert_eq!(
            after_recovery["cliUnresolved"], true,
            "origin 写入失败后必须回滚 unresolved"
        );
        assert!(f.runner.0.lock().unwrap().is_empty());
        *f.hosts.0.lock().unwrap() = hosts;
        let id = m.start(ManagementKind::Install).unwrap();
        wait(&m, &id, false).await;
        f.stop_hosts();
        let job = wait(&m, &id, true).await;
        assert_eq!(job.phase, ManagementPhase::Succeeded, "{job:?}");
        let ledger: serde_json::Value =
            serde_json::from_slice(&std::fs::read(ledger_path(&f)).unwrap()).unwrap();
        let jobs = ledger["jobs"].as_array().unwrap();
        assert_eq!(jobs.len(), 2, "账本应仅包含两条终态回执: {ledger}");
        assert!(jobs
            .iter()
            .any(|row| row["jobId"] == failed && row["phase"] == "failed"));
        assert!(jobs
            .iter()
            .any(|row| row["jobId"] == id && row["phase"] == "succeeded"));
    }

    #[cfg(feature = "dsh-test-probes")]
    #[tokio::test]
    // 进程内写入探针为单例，跨 await 持锁以串行隔离探针生命周期。
    #[allow(clippy::await_holding_lock)]
    async fn failed_pending_persist_does_not_allow_later_offline_path_substitution() {
        const CHILD: &str = "TUVIS_TEST_PENDING_PERSIST_PATH_CHILD";
        if std::env::var_os(CHILD).is_none() {
            // PATH 仅传给子进程，避免影响并发测试；父进程持有 B 的临时目录。
            let b = Bench::new(web_argv(), true, PID);
            let npm_b = seed_path_npm_cli(&b);
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "management_regressions::failed_pending_persist_does_not_allow_later_offline_path_substitution",
                    "--nocapture",
                ])
                .env(CHILD, &npm_b)
                .env("PATH", npm_b.parent().unwrap())
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "子进程失败：{}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }

        let f = Fixture::new();
        f.hosts.0.lock().unwrap().retain(|p| p.pid == PID);
        assert_eq!(
            identity::web_cli_source(&f.bench.env, None).unwrap(),
            f.wrapper_a
        );
        let _serial = MANAGEMENT_WRITE_PROBE_LOCK.lock().unwrap();
        let _clear = ClearManagementWriteProbe;
        use multi_agents_manager_lib::services::dsh_control::install::ledger_commit_probe;
        // 初始回执、cliUnresolved 先成功；第三次 cliPending 提交失败。
        ledger_commit_probe::set_fail_write_after(&ledger_path(&f), 2);
        let m = f.manager();
        let hosts = f.hosts.0.lock().unwrap().clone();
        let first = m.start(ManagementKind::Install).unwrap();
        wait(&m, &first, false).await;
        assert_no_saved_origin(&f);
        let before_recovery: serde_json::Value =
            serde_json::from_slice(&std::fs::read(ledger_path(&f)).unwrap()).unwrap();
        assert_eq!(before_recovery["cliUnresolved"], true, "前一阶段必须已落盘");
        assert!(
            before_recovery["cliPending"].is_null(),
            "pending 提交必须仍未落盘"
        );
        assert!(f.runner.0.lock().unwrap().is_empty());
        ledger_commit_probe::set_fail_write(None);
        f.stop_hosts();
        let first_job = wait(&m, &first, true).await;
        assert_eq!(first_job.phase, ManagementPhase::Failed, "{first_job:?}");
        assert_eq!(
            first_job.reason_code.as_deref(),
            Some(codes::CLI_SOURCE_UNVERIFIED)
        );
        assert_no_saved_origin(&f);
        let after_recovery: serde_json::Value =
            serde_json::from_slice(&std::fs::read(ledger_path(&f)).unwrap()).unwrap();
        assert!(
            !after_recovery["cliPending"].is_null(),
            "后续回执必须保留曾提交失败的 pending 阻断状态"
        );

        let npm_b = PathBuf::from(std::env::var_os(CHILD).unwrap());
        assert_eq!(
            identity::web_cli_source(&f.bench.env, std::env::var_os("PATH").as_deref()).unwrap(),
            npm_b,
            "离线时 PATH 的 B 必须可独立核验，才能证明没有借用它"
        );
        let second = m.start(ManagementKind::Install).unwrap();
        let second_job = wait(&m, &second, true).await;
        assert_eq!(second_job.phase, ManagementPhase::Failed, "{second_job:?}");
        assert_eq!(
            second_job.reason_code.as_deref(),
            Some(codes::CLI_SOURCE_UNVERIFIED)
        );
        assert!(
            f.runner.0.lock().unwrap().is_empty(),
            "离线作业不得调用 PATH 中的 npm B"
        );
        *f.hosts.0.lock().unwrap() = hosts;
        let recovered = m.start(ManagementKind::Install).unwrap();
        wait(&m, &recovered, false).await;
        f.stop_hosts();
        let job = wait(&m, &recovered, true).await;
        assert_eq!(job.phase, ManagementPhase::Succeeded, "{job:?}");
        let ledger: serde_json::Value =
            serde_json::from_slice(&std::fs::read(ledger_path(&f)).unwrap()).unwrap();
        let jobs = ledger["jobs"].as_array().unwrap();
        assert_eq!(jobs.len(), 3);
        assert!(jobs
            .iter()
            .any(|row| row["jobId"] == first && row["phase"] == "failed"));
        assert!(jobs
            .iter()
            .any(|row| row["jobId"] == second && row["phase"] == "failed"));
        assert!(jobs
            .iter()
            .any(|row| row["jobId"] == recovered && row["phase"] == "succeeded"));
    }

    #[derive(Default)]
    struct RecordingRevoker(Mutex<Vec<String>>);
    impl BindingRevoker for RecordingRevoker {
        fn revoke_profile(&self, profile: &str) -> Result<(), DshError> {
            self.0.lock().unwrap().push(profile.to_owned());
            Ok(())
        }
    }

    async fn uninstall_revokes_when_source_fails(replace_while_waiting: bool) {
        let f = Fixture::new();
        f.hosts.0.lock().unwrap().retain(|p| p.pid == PID);
        if !replace_while_waiting {
            std::fs::remove_file(&f.wrapper_a).unwrap();
        }
        let revoker = Arc::new(RecordingRevoker::default());
        let target = SelectedInstance {
            instance_id: f.bench.id.clone(),
            kind: HostKind::Web,
            profile_dir: f.bench.home.join("profiles/web"),
            dsh_home: Some(f.bench.home.clone()),
            identity_fingerprint: String::new(),
            read_evidence_fingerprint: String::new(),
        };
        let m = init_instance_manager(&f.bench.env, &target, revoker.clone());
        let id = m.start(ManagementKind::Uninstall).unwrap();
        wait(&m, &id, false).await;
        assert!(revoker.0.lock().unwrap().is_empty());
        if replace_while_waiting {
            std::fs::remove_file(&f.wrapper_a).unwrap();
            std::fs::write(&f.wrapper_a, b"replaced").unwrap();
        }
        f.stop_hosts();
        let job = wait(&m, &id, true).await;
        assert_eq!(job.phase, ManagementPhase::Failed, "{job:?}");
        assert_eq!(
            job.reason_code.as_deref(),
            Some(codes::CLI_SOURCE_UNVERIFIED)
        );
        assert_eq!(
            *revoker.0.lock().unwrap(),
            vec![target.profile_dir.to_string_lossy().into_owned()]
        );
        assert!(f.runner.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn uninstall_revokes_before_rejecting_unverified_bundled_source() {
        uninstall_revokes_when_source_fails(false).await;
    }

    #[tokio::test]
    async fn uninstall_revokes_before_rejecting_replaced_bundled_source() {
        uninstall_revokes_when_source_fails(true).await;
    }

    struct HostsWithoutListeners(Arc<Hosts>);
    impl DshProcessSource for HostsWithoutListeners {
        fn list_processes(&self) -> Vec<DshProcess> {
            self.0.list_processes()
        }
        fn listening_sockets(&self) -> Vec<DshListener> {
            vec![]
        }
    }

    #[tokio::test]
    async fn web_process_without_listener_cannot_use_offline_path_source() {
        const CHILD: &str = "TUVIS_TEST_WEB_NO_LISTENER_PATH_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let b = Bench::new(web_argv(), true, PID);
            let npm = seed_path_npm_cli(&b);
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "management_regressions::web_process_without_listener_cannot_use_offline_path_source",
                    "--nocapture",
                ])
                .env(CHILD, &npm)
                .env("PATH", npm.parent().unwrap())
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "子进程失败：{}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }
        let mut f = Fixture::new();
        ordinary_a(&mut f);
        f.hosts.0.lock().unwrap().retain(|p| p.pid == PID);
        f.bench.env.processes = Arc::new(HostsWithoutListeners(f.hosts.clone()));
        let npm = PathBuf::from(std::env::var_os(CHILD).unwrap());
        // 离线对照证明 PATH 中的 npm 入口确实可用。
        let mut offline = f.bench.env.clone();
        offline.processes = Arc::new(Hosts(Mutex::new(vec![])));
        assert_eq!(
            identity::web_cli_source(&offline, std::env::var_os("PATH").as_deref()).unwrap(),
            npm
        );
        assert!(identity::web_host_alive(&f.bench.env));
        let m = f.manager();
        let id = m.start(ManagementKind::Install).unwrap();
        wait(&m, &id, false).await;
        assert!(f.runner.0.lock().unwrap().is_empty());
        f.stop_hosts();
        let job = wait(&m, &id, true).await;
        assert_eq!(job.phase, ManagementPhase::Failed, "{job:?}");
        assert_eq!(
            job.reason_code.as_deref(),
            Some(codes::CLI_SOURCE_UNVERIFIED)
        );
        assert!(f.runner.0.lock().unwrap().is_empty());
    }

    async fn failed_live_observation_blocks_path_after_manager_reconstruction(no_listener: bool) {
        const CHILD: &str = "TUVIS_TEST_UNPROVEN_RESTART_PATH_CHILD";
        let test_name = if no_listener {
            "management_regressions::missing_listener_blocks_path_after_manager_reconstruction"
        } else {
            "management_regressions::unproven_ownership_blocks_path_after_manager_reconstruction"
        };
        if std::env::var_os(CHILD).is_none() {
            let b = Bench::new(web_argv(), true, PID);
            let npm_b = seed_path_npm_cli(&b);
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", test_name, "--nocapture"])
                .env(CHILD, &npm_b)
                .env("PATH", npm_b.parent().unwrap())
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "子进程失败：{}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }
        let mut f = Fixture::new();
        f.hosts.0.lock().unwrap().retain(|p| p.pid == PID);
        if no_listener {
            f.bench.env.processes = Arc::new(HostsWithoutListeners(f.hosts.clone()));
        } else {
            f.bench.env.environments = Arc::new(
                multi_agents_manager_lib::services::dsh_control::model::TestUnobservableEnvironments,
            );
        }
        let npm_b = PathBuf::from(std::env::var_os(CHILD).unwrap());
        let mut offline = f.bench.env.clone();
        offline.processes = Arc::new(Hosts(Mutex::new(vec![])));
        assert_eq!(
            identity::web_cli_source(&offline, std::env::var_os("PATH").as_deref()).unwrap(),
            npm_b
        );
        assert!(identity::web_host_alive(&f.bench.env));
        let manager = f.manager();
        let first = manager.start(ManagementKind::Install).unwrap();
        wait(&manager, &first, false).await;
        f.stop_hosts();
        let job = wait(&manager, &first, true).await;
        assert_eq!(job.phase, ManagementPhase::Failed, "{job:?}");
        assert_eq!(
            job.reason_code.as_deref(),
            Some(codes::CLI_SOURCE_UNVERIFIED)
        );
        drop(manager);
        let manager = f.manager();
        let second = manager.start(ManagementKind::Install).unwrap();
        let job = wait(&manager, &second, true).await;
        assert_eq!(
            job.phase,
            ManagementPhase::Failed,
            "离线重建后不得借 PATH B: {job:?}"
        );
        assert_eq!(
            job.reason_code.as_deref(),
            Some(codes::CLI_SOURCE_UNVERIFIED)
        );
        assert!(f.runner.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn missing_listener_blocks_path_after_manager_reconstruction() {
        failed_live_observation_blocks_path_after_manager_reconstruction(true).await;
    }

    #[tokio::test]
    async fn unproven_ownership_blocks_path_after_manager_reconstruction() {
        failed_live_observation_blocks_path_after_manager_reconstruction(false).await;
    }

    #[tokio::test]
    async fn same_target_reacquires_moved_cli_from_live_evidence_without_borrowing_other_instance()
    {
        let mut f = Fixture::new();
        let other_host = f
            .hosts
            .0
            .lock()
            .unwrap()
            .iter()
            .find(|p| p.pid == PID + 1)
            .unwrap()
            .clone();
        let (old_entry, _) = seed_npm_source(&mut f, true);
        let mut target_host = f.hosts.0.lock().unwrap()[0].clone();
        let manager = f.manager();
        let job = start_and_stop(&f, &manager, ManagementKind::Install).await;
        assert_eq!(job.phase, ManagementPhase::Succeeded, "{job:?}");
        assert!(f
            .runner
            .0
            .lock()
            .unwrap()
            .iter()
            .all(|call| call.program == old_entry));
        f.runner.0.lock().unwrap().clear();
        std::fs::remove_file(&old_entry).unwrap();
        let new_entry = seed_path_npm_cli(&f.bench);
        target_host.cmdline[1] = new_entry.to_string_lossy().into_owned();
        f.hosts.0.lock().unwrap().extend([target_host, other_host]);
        assert!(
            identity::discover_instance_candidate(&f.bench.env, &f.bench.id)
                .unwrap()
                .verified
        );
        let manager = f.manager();
        let job = start_and_stop(&f, &manager, ManagementKind::Repair).await;
        assert_eq!(job.phase, ManagementPhase::Succeeded, "{job:?}");
        let calls = f.runner.0.lock().unwrap();
        assert!(!calls.is_empty());
        let other_wrapper = f
            .bench
            .root
            .join("Other.app/Contents/Resources/runtime/cli/bin/dsh");
        assert!(calls
            .iter()
            .all(|call| call.program == new_entry && call.program != other_wrapper));
        assert!(
            calls
                .iter()
                .all(|call| call.env
                    == vec![("DSH_HOME".into(), f.bench.home.as_os_str().to_owned())])
        );
    }

    #[tokio::test]
    async fn failed_live_migration_does_not_reuse_old_origin_offline() {
        let mut f = Fixture::new();
        f.hosts.0.lock().unwrap().retain(|p| p.pid == PID);
        let old_host = f.hosts.0.lock().unwrap()[0].clone();
        let manager = f.manager();
        let first = start_and_stop(&f, &manager, ManagementKind::Install).await;
        assert_eq!(first.phase, ManagementPhase::Succeeded, "{first:?}");
        assert!(f
            .runner
            .0
            .lock()
            .unwrap()
            .iter()
            .all(|c| c.program == f.wrapper_a));
        f.runner.0.lock().unwrap().clear();

        // 同一目标迁移到另一安装，B 的 wrapper 存在，但归档核验失败。
        let (wrapper_b, archive_b) = seed_bundled_wrapper(&f.bench, "Other.app");
        let executable_b = wrapper_b
            .ancestors()
            .nth(5)
            .unwrap()
            .join("MacOS/DeepSeek Harness");
        let resources_b = executable_b
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("Resources");
        let new_host = DshProcess {
            pid: PID,
            exe: executable_b.clone(),
            started_at_ms: START,
            cmdline: vec![
                executable_b.to_string_lossy().into_owned(),
                "--expose-internals".into(),
                resources_b
                    .join("app.asar/dsh/node_modules/@deepseek-ai/dsh-desktop-host/lib/cli.js")
                    .to_string_lossy()
                    .into_owned(),
                "web".into(),
            ],
        };
        f.bench.env.environments = Arc::new(Environments(vec![(
            PID,
            executable_b,
            f.bench.home.clone(),
        )]));
        f.hosts.0.lock().unwrap().push(new_host);
        assert!(
            identity::discover_instance_candidate(&f.bench.env, &f.bench.id)
                .unwrap()
                .verified
        );
        assert_eq!(
            identity::web_cli_source(&f.bench.env, None).unwrap(),
            wrapper_b
        );
        write_asar_fixture(&archive_b, Some("impostor"));
        let manager = f.manager();
        let migration = start_and_stop(&f, &manager, ManagementKind::Repair).await;
        assert_eq!(migration.phase, ManagementPhase::Failed, "{migration:?}");
        assert_eq!(
            migration.reason_code.as_deref(),
            Some(codes::CLI_SOURCE_UNVERIFIED)
        );
        assert!(f.runner.0.lock().unwrap().is_empty());
        // A 仍然可用，因此若离线作业错误复用旧来源，runner 会记录调用。
        assert!(f.wrapper_a.exists());
        assert!(old_host.exe.exists());
        drop(manager);
        let manager = f.manager();
        let retry = manager.start(ManagementKind::Repair).unwrap();
        let job = wait(&manager, &retry, true).await;
        assert_eq!(
            job.phase,
            ManagementPhase::Failed,
            "离线不得复用 A: {job:?}"
        );
        assert_eq!(
            job.reason_code.as_deref(),
            Some(codes::CLI_SOURCE_UNVERIFIED)
        );
        assert!(f.runner.0.lock().unwrap().is_empty());
    }

    async fn failed_online_source_blocks_offline_path(generation_change: bool) {
        const CHILD: &str = "TUVIS_TEST_SOURCE_FAILURE_OFFLINE_PATH_CHILD";
        let test_name = if generation_change {
            "management_regressions::initial_generation_recheck_failure_blocks_offline_path"
        } else {
            "management_regressions::cli_extraction_failure_blocks_offline_path"
        };
        if std::env::var_os(CHILD).is_none() {
            let b = Bench::new(web_argv(), true, PID);
            let npm = seed_path_npm_cli(&b);
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", test_name, "--nocapture"])
                .env(CHILD, &npm)
                .env("PATH", npm.parent().unwrap())
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "子进程失败：{}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }
        let mut f = Fixture::new();
        if generation_change {
            f.hosts.0.lock().unwrap().retain(|p| p.pid == PID);
            let exe = f.hosts.0.lock().unwrap()[0].exe.clone();
            f.bench.env.environments = Arc::new(ChangingEvidence {
                change_after: 2,
                executable: exe,
                home: f.bench.home.clone(),
                changed_home: f.home_b.clone(),
                change_generation: true,
                reads: std::sync::atomic::AtomicUsize::new(0),
            });
        } else {
            ordinary_a(&mut f);
            f.hosts.0.lock().unwrap().retain(|p| p.pid == PID);
            assert!(
                identity::discover_instance_candidate(&f.bench.env, &f.bench.id)
                    .unwrap()
                    .verified
            );
        }
        let manager = f.manager();
        let first = manager.start(ManagementKind::Install).unwrap();
        wait(&manager, &first, false).await;
        f.stop_hosts();
        let job = wait(&manager, &first, true).await;
        assert_eq!(job.phase, ManagementPhase::Failed, "{job:?}");
        assert_eq!(
            job.reason_code.as_deref(),
            Some(codes::CLI_SOURCE_UNVERIFIED)
        );
        assert!(f.runner.0.lock().unwrap().is_empty());
        let npm = PathBuf::from(std::env::var_os(CHILD).unwrap());
        assert_eq!(
            identity::web_cli_source(&f.bench.env, std::env::var_os("PATH").as_deref()).unwrap(),
            npm
        );
        drop(manager);
        let manager = f.manager();
        let retry = manager.start(ManagementKind::Install).unwrap();
        let job = wait(&manager, &retry, true).await;
        assert_eq!(
            job.phase,
            ManagementPhase::Failed,
            "离线不得回退 PATH: {job:?}"
        );
        assert_eq!(
            job.reason_code.as_deref(),
            Some(codes::CLI_SOURCE_UNVERIFIED)
        );
        assert!(f.runner.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn cli_extraction_failure_blocks_offline_path() {
        failed_online_source_blocks_offline_path(false).await;
    }

    #[tokio::test]
    async fn initial_generation_recheck_failure_blocks_offline_path() {
        failed_online_source_blocks_offline_path(true).await;
    }

    struct ExternallyModifiedLedgerRunner {
        runner: Arc<Runner>,
        ledger: PathBuf,
        external: Vec<u8>,
    }
    impl DshCommandRunner for ExternallyModifiedLedgerRunner {
        fn run(&self, request: &CommandRequest) -> Result<RunOutcome, DshError> {
            let outcome = self.runner.run(request)?;
            if request.args[3] == "add" {
                std::fs::write(&self.ledger, &self.external).unwrap();
            }
            Ok(outcome)
        }
    }

    #[tokio::test]
    async fn external_ledger_change_during_cli_reports_state_io_and_preserves_contents() {
        let mut f = Fixture::new();
        f.hosts.0.lock().unwrap().retain(|p| p.pid == PID);
        let ledger = ledger_path(&f);
        let external = b"{\"external\":\"modified during CLI\"}\n".to_vec();
        f.bench.env.runner = Arc::new(ExternallyModifiedLedgerRunner {
            runner: f.runner.clone(),
            ledger: ledger.clone(),
            external: external.clone(),
        });
        let manager = f.manager();
        let job = start_and_stop(&f, &manager, ManagementKind::Install).await;
        assert!(
            !f.runner.0.lock().unwrap().is_empty(),
            "必须在 CLI 执行期间修改账本"
        );
        assert_eq!(job.phase, ManagementPhase::Failed, "{job:?}");
        assert_eq!(job.reason_code.as_deref(), Some(codes::STATE_IO));
        assert_eq!(std::fs::read(ledger).unwrap(), external);
    }

    struct TemporaryFiles;
    impl DshFileSource for TemporaryFiles {
        fn exists(&self, path: &Path) -> bool {
            Files.exists(path)
        }
        fn is_dir(&self, path: &Path) -> bool {
            Files.is_dir(path)
        }
        fn list_dir(&self, path: &Path) -> Vec<PathBuf> {
            if path == Path::new("/Applications") {
                vec![]
            } else {
                Files.list_dir(path)
            }
        }
        fn list_dir_checked(&self, path: &Path) -> Result<Vec<PathBuf>, DshError> {
            Files.list_dir_checked(path)
        }
        fn read_small(&self, path: &Path, max_bytes: u64) -> Result<Vec<u8>, DshError> {
            Files.read_small(path, max_bytes)
        }
    }

    fn assert_profile_lock_released(profile: &Path) {
        use fs2::FileExt;
        let path = profile.join(".tuvis-management/active.lock");
        if path.exists() {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .unwrap();
            file.try_lock_exclusive().expect("启动失败必须释放物理锁");
            FileExt::unlock(&file).unwrap();
        }
    }

    async fn desktop_invalid_ledger(kind: ManagementKind, unreadable: bool) {
        let mut f = Fixture::new();
        f.stop_hosts();
        f.bench.env.home = f.bench.home.clone();
        f.bench.env.files = Arc::new(TemporaryFiles);
        let (wrapper, _) = seed_bundled_wrapper(&f.bench, "Applications/DeepSeek Harness.app");
        let profile = f.bench.home.join("profiles/desktop");
        std::fs::write(
            profile.join("package.json"),
            br#"{"dependencies":{},"dsh":{"profile":{"bundles":[]}}}"#,
        )
        .unwrap();
        let target = SelectedInstance {
            instance_id: instance_id_for(HostKind::Desktop, &profile),
            kind: HostKind::Desktop,
            profile_dir: profile.clone(),
            dsh_home: None,
            identity_fingerprint: String::new(),
            read_evidence_fingerprint: String::new(),
        };
        let ledger = f
            .bench
            .env
            .tuvis_home
            .join("dsh/instances")
            .join(&target.instance_id)
            .join("management.json");
        std::fs::create_dir_all(ledger.parent().unwrap()).unwrap();
        let corrupt = b"{ invalid management ledger";
        if unreadable {
            // 目录不可按文件读取，跨权限模型稳定；标记文件用于验证未被覆盖。
            std::fs::create_dir(&ledger).unwrap();
            std::fs::write(ledger.join("sentinel"), b"keep").unwrap();
        } else {
            std::fs::write(&ledger, corrupt).unwrap();
        }
        assert_eq!(
            identity::resolve_cli(&f.bench.env).unwrap().cli_path,
            wrapper
        );
        let revoker = Arc::new(RecordingRevoker::default());
        let m = init_instance_manager(&f.bench.env, &target, revoker.clone());
        let err = m.start(kind).expect_err("无效账本必须拒绝启动");
        assert_eq!(err.code, codes::STATE_IO);
        assert_profile_lock_released(&profile);
        assert!(
            f.runner.0.lock().unwrap().is_empty(),
            "无效账本不得执行 CLI"
        );
        assert!(revoker.0.lock().unwrap().is_empty(), "无效账本不得撤权");
        if unreadable {
            assert_eq!(std::fs::read(ledger.join("sentinel")).unwrap(), b"keep");
        } else {
            assert_eq!(std::fs::read(&ledger).unwrap(), corrupt);
        }
    }

    async fn desktop_ledger_corrupted_after_construction(kind: ManagementKind) {
        let mut f = Fixture::new();
        f.stop_hosts();
        f.bench.env.home = f.bench.home.clone();
        f.bench.env.files = Arc::new(TemporaryFiles);
        let (wrapper, _) = seed_bundled_wrapper(&f.bench, "Applications/DeepSeek Harness.app");
        let profile = f.bench.home.join("profiles/desktop");
        std::fs::write(
            profile.join("package.json"),
            br#"{"dependencies":{},"dsh":{"profile":{"bundles":[]}}}"#,
        )
        .unwrap();
        let target = SelectedInstance {
            instance_id: instance_id_for(HostKind::Desktop, &profile),
            kind: HostKind::Desktop,
            profile_dir: profile.clone(),
            dsh_home: None,
            identity_fingerprint: String::new(),
            read_evidence_fingerprint: String::new(),
        };
        let ledger = f
            .bench
            .env
            .tuvis_home
            .join("dsh/instances")
            .join(&target.instance_id)
            .join("management.json");
        std::fs::create_dir_all(ledger.parent().unwrap()).unwrap();
        assert_eq!(
            identity::resolve_cli(&f.bench.env).unwrap().cli_path,
            wrapper
        );
        let revoker = Arc::new(RecordingRevoker::default());
        let manager = init_instance_manager(&f.bench.env, &target, revoker.clone());
        let corrupt = b"{ corrupted after manager construction";
        std::fs::write(&ledger, corrupt).unwrap();
        let err = manager.start(kind).expect_err("构造后损坏账本必须拒绝启动");
        assert_eq!(err.code, codes::STATE_IO);
        assert_profile_lock_released(&profile);
        assert!(
            f.runner.0.lock().unwrap().is_empty(),
            "损坏账本不得执行 CLI"
        );
        assert!(revoker.0.lock().unwrap().is_empty(), "无效账本不得撤权");
        assert_eq!(std::fs::read(&ledger).unwrap(), corrupt);
    }

    #[tokio::test]
    async fn desktop_install_rejects_ledger_corrupted_after_manager_construction() {
        desktop_ledger_corrupted_after_construction(ManagementKind::Install).await;
    }
    #[tokio::test]
    async fn desktop_repair_rejects_ledger_corrupted_after_manager_construction() {
        desktop_ledger_corrupted_after_construction(ManagementKind::Repair).await;
    }
    #[tokio::test]
    async fn desktop_uninstall_rejects_ledger_corrupted_after_manager_construction_without_revoking(
    ) {
        desktop_ledger_corrupted_after_construction(ManagementKind::Uninstall).await;
    }

    #[tokio::test]
    async fn desktop_install_rejects_corrupt_ledger() {
        desktop_invalid_ledger(ManagementKind::Install, false).await;
    }
    #[tokio::test]
    async fn desktop_repair_rejects_corrupt_ledger() {
        desktop_invalid_ledger(ManagementKind::Repair, false).await;
    }
    #[tokio::test]
    async fn desktop_uninstall_rejects_corrupt_ledger_without_revoking() {
        desktop_invalid_ledger(ManagementKind::Uninstall, false).await;
    }
    #[tokio::test]
    async fn desktop_install_rejects_unreadable_ledger() {
        desktop_invalid_ledger(ManagementKind::Install, true).await;
    }
    #[tokio::test]
    async fn desktop_repair_rejects_unreadable_ledger() {
        desktop_invalid_ledger(ManagementKind::Repair, true).await;
    }
    #[tokio::test]
    async fn desktop_uninstall_rejects_unreadable_ledger_without_revoking() {
        desktop_invalid_ledger(ManagementKind::Uninstall, true).await;
    }
}
