//! raw Web 不得从真实 resolve_session 的 writer 归属判断中消失。
//!
//! 真实临时目录、生产 FsFileSource 与 InstanceRegistry；只注入 OS 进程/监听快照。
//! Desktop A 经 argv/profile 标记/监听属主发现为 verified；Web B 始终只有 raw
//! 事实，不登记、不预置 verified、指纹或授权账本。直接请求 A，不先请求 B。
//! 此文件刻意保留行为 RED，生产修复不属于本轮范围。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use multi_agents_manager_lib::services::dsh_control::error::{codes, DshError};
use multi_agents_manager_lib::services::dsh_control::identity::{discover_detailed, Discovery};
use multi_agents_manager_lib::services::dsh_control::instances::{
    HostKind, InstanceIdentityProbe, InstanceRegistry, SelectedInstance,
};
use multi_agents_manager_lib::services::dsh_control::model::{
    CommandRequest, DshClock, DshCommandRunner, DshEnvironment, DshListener, DshProcess,
    DshProcessSource, RunOutcome,
};
use multi_agents_manager_lib::services::dsh_control::production::FsFileSource;

const SESSION: &str = "session-cccccccc";
const DESKTOP_PID: u32 = 101;
const WEB_PID: u32 = 202;
const DESKTOP_KEY: &str = "desktop-a-current-key";

/// 快照边界计数用于验证零宿主时不触发昂贵的系统监听扫描。
struct ProcessSnapshot {
    processes: Vec<DshProcess>,
    listeners: Vec<DshListener>,
    listener_calls: AtomicUsize,
}

impl DshProcessSource for ProcessSnapshot {
    fn list_processes(&self) -> Vec<DshProcess> {
        self.processes.clone()
    }

    fn listening_sockets(&self) -> Vec<DshListener> {
        self.listener_calls.fetch_add(1, Ordering::SeqCst);
        self.listeners.clone()
    }
}

struct NoCommands;
impl DshCommandRunner for NoCommands {
    fn run(&self, _: &CommandRequest) -> Result<RunOutcome, DshError> {
        panic!("归属解析不得执行宿主命令")
    }
}

struct FixedClock;
impl DshClock for FixedClock {
    fn now_ms(&self) -> i64 {
        10_000
    }
}

/// 仅为 A 接线当代身份；若实现转而查询 B 指纹，立即暴露夹具越界。
struct DesktopIdentity {
    instance_id: String,
}
impl InstanceIdentityProbe for DesktopIdentity {
    fn current_fingerprint(&self, instance: &SelectedInstance) -> Result<Option<String>, DshError> {
        assert_eq!(instance.instance_id, self.instance_id);
        assert_eq!(instance.kind, HostKind::Desktop);
        Ok(Some(DESKTOP_KEY.into()))
    }
}

fn web_process() -> DshProcess {
    DshProcess {
        pid: WEB_PID,
        exe: PathBuf::from("/isolated-runtime/node"),
        // 不携带 home：env.home 与目录标记不能变成 B 的归属证明。
        cmdline: vec![
            "/isolated-runtime/node".into(),
            "/isolated-runtime/bin/dsh".into(),
            "web".into(),
        ],
        started_at_ms: 2_000,
    }
}

fn listener(pid: u32, authority: &str) -> DshListener {
    DshListener {
        owner_pid: pid,
        authority: authority.into(),
    }
}

fn seed_profile(home: &Path, kind: &str) -> PathBuf {
    let profile = home.join("profiles").join(kind);
    std::fs::create_dir_all(&profile).unwrap();
    std::fs::write(profile.join("package.json"), b"{}").unwrap();
    std::fs::canonicalize(profile).unwrap()
}

fn seed_session(home: &Path, project: &str, session: &str) -> PathBuf {
    let dir = home.join("sessions").join(project).join(session);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(dir).unwrap()
}

struct Bench {
    registry: InstanceRegistry,
    env: Arc<DshEnvironment>,
    processes: Arc<ProcessSnapshot>,
    home_a: PathBuf,
    home_b: PathBuf,
    desktop: SelectedInstance,
    _root: tempfile::TempDir,
}

impl Bench {
    fn new(with_desktop: bool, with_web: bool, web_listeners: Vec<DshListener>) -> Self {
        let root = tempfile::tempdir().unwrap();
        let profile_a = seed_profile(&root.path().join("Desktop A"), "desktop");
        let profile_b = seed_profile(&root.path().join("Web B"), "web");
        let home_a = profile_a.parent().unwrap().parent().unwrap().to_path_buf();
        let home_b = profile_b.parent().unwrap().parent().unwrap().to_path_buf();
        assert_ne!(home_a, home_b, "A/B 必须为两个实际隔离 home");
        let mut hosts = Vec::new();
        let mut listeners = web_listeners;
        if with_desktop {
            let exe = root
                .path()
                .join("DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness");
            let entry = root.path().join(
                "DeepSeek Harness.app/Contents/Resources/app.asar/dsh/node_modules/@deepseek-ai/dsh-desktop-host/lib/index.js",
            );
            hosts.push(DshProcess {
                pid: DESKTOP_PID,
                exe: exe.clone(),
                cmdline: vec![
                    exe.to_string_lossy().into_owned(),
                    "--expose-internals".into(),
                    entry.to_string_lossy().into_owned(),
                    profile_a.to_string_lossy().into_owned(),
                ],
                started_at_ms: 1_000,
            });
            listeners.push(listener(DESKTOP_PID, "127.0.0.1:5101"));
        }
        if with_web {
            hosts.push(web_process());
        }
        let processes = Arc::new(ProcessSnapshot {
            processes: hosts,
            listeners,
            listener_calls: AtomicUsize::new(0),
        });
        let env = Arc::new(DshEnvironment {
            home: home_b.clone(),
            tuvis_home: root.path().join("tuvis-shadow"),
            dsh_state_dir: Some(root.path().join("auth-shadow")),
            processes: processes.clone(),
            files: Arc::new(FsFileSource),
            runner: Arc::new(NoCommands),
            clock: Arc::new(FixedClock),
            // 测试默认「不可观测」：显式拒绝 Web home 归属（fail-closed），
            // 绝不让假进程源落到真实系统 PID 上。需要 Web 归属的用例自行注入假观测器。
            environments: std::sync::Arc::new(multi_agents_manager_lib::services::dsh_control::model::TestUnobservableEnvironments),
        });
        let registry =
            InstanceRegistry::open_with_env(root.path().join("instances.json"), Some(env.clone()))
                .unwrap();
        // 仅 A 登记身份；B 连登记记录都没有，更没有受信指纹或 Bound 行。
        let desktop = registry
            .register(HostKind::Desktop, &profile_a, DESKTOP_KEY)
            .unwrap();
        let registry = registry.with_identity_probe(Arc::new(DesktopIdentity {
            instance_id: desktop.instance_id.clone(),
        }));
        Self {
            registry,
            env,
            processes,
            home_a,
            home_b,
            desktop,
            _root: root,
        }
    }

    fn assert_mixed_discovery(&self) -> Discovery {
        let discovery = discover_detailed(&self.env).unwrap();
        assert_eq!(discovery.candidates.len(), 1, "B 不能被猜成 profile 候选");
        let a = &discovery.candidates[0];
        assert_eq!(a.instance_id, self.desktop.instance_id);
        assert_eq!(a.kind, HostKind::Desktop);
        assert!(a.verified, "必须由真实发现核验 A，不能预置 verified");
        assert_eq!(a.host.pid, DESKTOP_PID);
        assert_eq!(discovery.raw_web_hosts.len(), 1);
        assert_eq!(discovery.raw_web_hosts[0].process, web_process());
        assert_eq!(discovery.unattributable_web_hosts, 1);
        assert_eq!(
            self.registry.registered_ids().unwrap(),
            vec![self.desktop.instance_id.clone()],
            "不得给 B 预置登记身份"
        );
        discovery
    }
}

#[test]
fn desktop_write_target_is_refused_when_raw_web_home_contains_the_same_session_id() {
    let b = Bench::new(true, true, vec![listener(WEB_PID, "127.0.0.1:5202")]);
    let session_a = seed_session(&b.home_a, "project-a", SESSION);
    let session_b = seed_session(&b.home_b, "project-b", SESSION);
    assert_ne!(session_a, session_b, "同 ID 必须是两个隔离目录");
    let discovery = b.assert_mixed_discovery();
    assert_eq!(
        discovery.raw_web_hosts[0].listeners,
        vec![listener(WEB_PID, "127.0.0.1:5202")]
    );

    // 第一次解析就请求已核验 A，不能让 B 自己的未核验拒绝提前遮蔽本断言。
    let result = b.registry.resolve_session(&b.desktop.instance_id, SESSION);
    assert!(
        matches!(&result, Err(e) if e.code == codes::SESSION_OWNERSHIP_UNPROVEN),
        "A/B 同 ID：raw Web 不得被跨实例归属检查漏掉，A 写目标必须拒绝；实际 {result:?}"
    );
}

#[test]
fn desktop_alone_resolves_its_write_target_even_with_a_duplicate_in_an_offline_web_home() {
    let b = Bench::new(true, false, vec![]);
    seed_session(&b.home_a, "project-a", SESSION);
    seed_session(&b.home_b, "project-b", SESSION);
    let discovery = discover_detailed(&b.env).unwrap();
    assert_eq!(discovery.candidates.len(), 1);
    assert!(discovery.candidates[0].verified);
    assert!(discovery.raw_web_hosts.is_empty());

    let target = b
        .registry
        .resolve_session(&b.desktop.instance_id, SESSION)
        .expect("正控制：仅 A 在场时，不能仅因离线 B 目录存在就拒绝 A");
    assert_eq!(target.host_id, b.desktop.instance_id);
    assert_eq!(target.session_id, SESSION);
    assert_eq!(target.epoch, discovery.candidates[0].host.epoch);
    assert!(target.epoch > 0);
}

#[test]
fn unknown_raw_web_without_a_visible_session_collision_degrades_to_unproven_ownership() {
    let b = Bench::new(true, true, vec![listener(WEB_PID, "127.0.0.1:5202")]);
    seed_session(&b.home_a, "project-a", SESSION);
    seed_session(&b.home_b, "project-b", "session-bbbbbbbb");
    assert!(!b.home_b.join("sessions/project-b").join(SESSION).exists());
    b.assert_mixed_discovery();

    let result = b.registry.resolve_session(&b.desktop.instance_id, SESSION);
    assert!(
        matches!(&result, Err(e) if e.code == codes::SESSION_OWNERSHIP_UNPROVEN),
        "未知 Web 无实际 home 证明：没有可见重名不等于排除 writer 冲突，应保守降级；实际 {result:?}"
    );
}

#[test]
fn raw_web_process_is_retained_when_no_listener_is_observed() {
    let b = Bench::new(false, true, vec![]);
    let discovery = discover_detailed(&b.env).unwrap();
    assert_eq!(discovery.raw_web_hosts.len(), 1);
    assert_eq!(discovery.raw_web_hosts[0].process, web_process());
    assert!(discovery.raw_web_hosts[0].listeners.is_empty());
    assert!(discovery.candidates.is_empty());
    assert_eq!(discovery.unattributable_web_hosts, 0);
}

#[test]
fn raw_web_process_is_retained_without_borrowing_another_process_listener() {
    let b = Bench::new(false, true, vec![listener(999, "127.0.0.1:5202")]);
    let discovery = discover_detailed(&b.env).unwrap();
    assert_eq!(discovery.raw_web_hosts.len(), 1);
    assert_eq!(discovery.raw_web_hosts[0].process, web_process());
    assert!(
        discovery.raw_web_hosts[0].listeners.is_empty(),
        "错属主监听不能借给 B；B 的 PID/启动代际/exe 事实仍须保留"
    );
    assert!(discovery.candidates.is_empty());
    assert_eq!(discovery.unattributable_web_hosts, 0);
}

#[test]
fn raw_web_with_its_own_listener_and_unknown_home_counts_as_unattributable() {
    let b = Bench::new(false, true, vec![listener(WEB_PID, "127.0.0.1:5202")]);
    let discovery = discover_detailed(&b.env).unwrap();
    assert_eq!(discovery.raw_web_hosts.len(), 1);
    assert_eq!(discovery.raw_web_hosts[0].process, web_process());
    assert_eq!(
        discovery.raw_web_hosts[0].listeners,
        vec![listener(WEB_PID, "127.0.0.1:5202")]
    );
    assert!(discovery.candidates.is_empty());
    assert_eq!(discovery.unattributable_web_hosts, 1);
}

#[test]
fn discovery_with_zero_hosts_never_scans_listeners() {
    let b = Bench::new(false, false, vec![listener(999, "127.0.0.1:5999")]);
    let discovery = discover_detailed(&b.env).unwrap();
    assert!(discovery.candidates.is_empty());
    assert!(discovery.raw_web_hosts.is_empty());
    assert_eq!(discovery.unattributable_web_hosts, 0);
    assert_eq!(discovery.unresolved_desktop_hosts, 0);
    assert_eq!(
        b.processes.listener_calls.load(Ordering::SeqCst),
        0,
        "没有 DSH 宿主时监听扫描调用预算为零"
    );
}
