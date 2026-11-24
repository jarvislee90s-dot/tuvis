//! H10：本机候选/稳定 ID 选择/查询的服务→命令核心集成测试（不依赖 H09）。
//!
//! 当前是接口阻塞的测试草案，不是已运行的行为 RED：本轮用户禁止运行 Cargo。
//! `commands::dsh` 尚缺下列接缝；故不放测试内替身、不用 cfg/ignore 隐藏缺失导入：
//!
//! - `instance_list_core(&DshEnvironment, &InstanceRegistry) -> Result<Vec<本机候选DTO>, String>`；
//! - `instance_select_core(&DshEnvironment, &InstanceRegistry, DshInstanceRequest) -> Result<(), String>`；
//! - `instance_status_core(&DshEnvironment, &InstanceRegistry, Option<DshInstanceRequest>)`
//!   `-> Result<带 instanceId 的本机管理状态DTO, String>`；
//! - `DshInstanceRequest { instance_id: String }`：camelCase/deny_unknown_fields；
//!   None 查询持久选择，Some 查询明确 ID，均不能变更选择或在失效后回落其他实例。
//!
//! 以上名称是本测试提出的最小接缝，尚未冻结为已有生产接口。候选 DTO 是数组，包含
//! instanceId/kind/displayName/profileDir；状态 DTO 复用既有管理字段并增加 instanceId。
//! 生产壳应复用 production_env()/production_registry() 和这些核心，不另建注册表。
//! 本文件不声称已验证 generate_handler!/实际 invoke 可达性；命令壳与注册仍需生产接线。
//!
//! 真实服务：identity::discover_all、InstanceRegistry 的文件锁/原子持久化，以及
//! 实例 manager/auth 的生产装配核心。边界输入：临时 profile 文件、可变进程/监听源。
//! 不注入授权读取结果/受信指纹/Bound 账本，不 register(...,"key")、不伪造 Peer。
//! Desktop 候选输入只证明配置路由；R01 的真实 OS/Web 归属及首次签名落锚不在此闭环。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

// 预期编译阻塞在这些缺失的生产符号；不得在测试目录补同名实现来制造通过。
use multi_agents_manager_lib::commands::dsh::{
    dsh_restart_host_core, instance_list_core, instance_select_core, instance_status_core,
    DshInstanceRequest,
};
use multi_agents_manager_lib::services::dsh_control::error::{codes, DshError};
use multi_agents_manager_lib::services::dsh_control::instances::InstanceRegistry;
use multi_agents_manager_lib::services::dsh_control::model::{
    CommandRequest, DshClock, DshCommandRunner, DshEnvironment, DshListener, DshProcess,
    DshProcessSource, RunOutcome,
};
use multi_agents_manager_lib::services::dsh_control::production::FsFileSource;
use serde_json::{json, Value};

#[derive(Default)]
struct ProcessFacts {
    processes: Mutex<Vec<DshProcess>>,
    listeners: Mutex<Vec<DshListener>>,
}

impl DshProcessSource for ProcessFacts {
    fn list_processes(&self) -> Vec<DshProcess> {
        self.processes.lock().unwrap().clone()
    }

    fn listening_sockets(&self) -> Vec<DshListener> {
        self.listeners.lock().unwrap().clone()
    }
}

impl ProcessFacts {
    // 仅删假源中的原始快照，不对真实进程发送信号。
    fn remove_host(&self, pid: u32) {
        self.processes.lock().unwrap().retain(|p| p.pid != pid);
        self.listeners
            .lock()
            .unwrap()
            .retain(|l| l.owner_pid != pid);
    }
}

struct NoCommands;

impl DshCommandRunner for NoCommands {
    fn run(&self, request: &CommandRequest) -> Result<RunOutcome, DshError> {
        panic!("候选/选择/查询不得执行 CLI 或启停宿主：{request:?}");
    }
}

struct FixedClock;

impl DshClock for FixedClock {
    fn now_ms(&self) -> i64 {
        1_760_000_000_000
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    env: Arc<DshEnvironment>,
    facts: Arc<ProcessFacts>,
    registry: InstanceRegistry,
    registry_path: PathBuf,
    profile_a: PathBuf,
    profile_b: PathBuf,
}

fn desktop_process(pid: u32, profile: &Path, started_at_ms: i64) -> DshProcess {
    let root = profile
        .ancestors()
        .nth(3)
        .expect("profile 位于临时 root/home/profiles/desktop");
    let contents = root.join("Applications/DeepSeek Harness.app/Contents");
    let executable = contents.join("MacOS/DeepSeek Harness");
    DshProcess {
        pid,
        exe: executable.clone(),
        cmdline: vec![
            executable.to_string_lossy().into_owned(),
            "--expose-internals".into(),
            contents
                .join("Resources/app.asar/dsh/node_modules/@deepseek-ai/dsh-desktop-host/lib/index.js")
                .to_string_lossy()
                .into_owned(),
            profile.to_string_lossy().into_owned(),
        ],
        started_at_ms,
    }
}

fn seed_profile(root: &Path, home_name: &str) -> PathBuf {
    let profile = root.join(home_name).join("profiles/desktop");
    std::fs::create_dir_all(&profile).unwrap();
    // 只写已初始化的空 profile；不种插件包或凭据。
    std::fs::write(
        profile.join("package.json"),
        br#"{"name":"dsh-profile-desktop","dependencies":{},"dsh":{"profile":{"bundles":[]}}}"#,
    )
    .unwrap();
    std::fs::canonicalize(profile).unwrap()
}

impl Fixture {
    fn new() -> Self {
        #[cfg(unix)]
        let root = tempfile::Builder::new()
            .prefix("h10-")
            .tempdir_in("/tmp")
            .unwrap();
        #[cfg(not(unix))]
        let root = tempfile::Builder::new().prefix("h10-").tempdir().unwrap();
        let profile_a = seed_profile(root.path(), "home-a");
        let profile_b = seed_profile(root.path(), "home-b");
        let contents = root
            .path()
            .join("Applications/DeepSeek Harness.app/Contents");
        std::fs::create_dir_all(contents.join("MacOS")).unwrap();
        std::fs::write(
            contents.join("MacOS/DeepSeek Harness"),
            b"not an executable; fixture only",
        )
        .unwrap();
        std::fs::write(contents.join("Info.plist"), b"<plist><dict/></plist>").unwrap();
        let facts = Arc::new(ProcessFacts {
            processes: Mutex::new(vec![
                desktop_process(41001, &profile_a, 1_000),
                desktop_process(41002, &profile_b, 2_000),
            ]),
            listeners: Mutex::new(vec![
                DshListener {
                    owner_pid: 41001,
                    authority: "127.0.0.1:5101".into(),
                },
                DshListener {
                    owner_pid: 41002,
                    authority: "127.0.0.1:5102".into(),
                },
            ]),
        });
        let env = Arc::new(DshEnvironment {
            // 故意指向 A：选择 B 后不得错误消费这个全局默认 home。
            home: profile_a.parent().unwrap().parent().unwrap().to_path_buf(),
            tuvis_home: root.path().join("tuvis"),
            dsh_state_dir: Some(root.path().join("s")),
            processes: facts.clone(),
            files: Arc::new(FsFileSource),
            runner: Arc::new(NoCommands),
            clock: Arc::new(FixedClock),
            // 测试默认「不可观测」：显式拒绝 Web home 归属（fail-closed），
            // 绝不让假进程源落到真实系统 PID 上。需要 Web 归属的用例自行注入假观测器。
            environments: std::sync::Arc::new(multi_agents_manager_lib::services::dsh_control::model::TestUnobservableEnvironments),
        });
        let registry_path = root.path().join("s/instances.json");
        let registry = InstanceRegistry::open_with_env(&registry_path, Some(env.clone())).unwrap();
        Self {
            _root: root,
            env,
            facts,
            registry,
            registry_path,
            profile_a,
            profile_b,
        }
    }

    fn candidates(&self) -> Vec<Value> {
        serde_json::to_value(
            instance_list_core(&self.env, &self.registry).expect("候选命令可列出全部候选"),
        )
        .unwrap()
        .as_array()
        .expect("候选和选择/管理状态不能混成同一个 DTO")
        .clone()
    }

    fn id_for(&self, profile: &Path) -> String {
        let profile = profile.to_string_lossy();
        let matches: Vec<Value> = self
            .candidates()
            .into_iter()
            .filter(|candidate| candidate["profileDir"].as_str() == Some(profile.as_ref()))
            .collect();
        assert_eq!(matches.len(), 1, "每个 profile 必须保留独立候选");
        matches[0]["instanceId"]
            .as_str()
            .expect("候选必须携稳定 ID")
            .to_owned()
    }

    fn select(&self, id: &str) {
        instance_select_core(&self.env, &self.registry, request(id), &|_| {})
            .expect("明确选择受理");
    }

    fn status(&self, id: Option<&str>) -> Result<Value, String> {
        instance_status_core(&self.env, &self.registry, id.map(request))
            .map(|status| serde_json::to_value(status).unwrap())
    }
}

fn request(id: &str) -> DshInstanceRequest {
    serde_json::from_value(json!({ "instanceId": id })).expect("稳定 ID 请求可解析")
}

fn expect_code(error: &str, code: &str) {
    assert!(
        error.starts_with(&format!("{code}:")),
        "预期 {code}，实际 {error}"
    );
}

#[test]
fn candidate_list_preserves_both_same_named_profiles_without_selecting_either() {
    let f = Fixture::new();
    let candidates = f.candidates();
    assert_eq!(candidates.len(), 2);
    let paths: BTreeSet<&str> = candidates
        .iter()
        .map(|c| c["profileDir"].as_str().unwrap())
        .collect();
    assert_eq!(
        paths,
        BTreeSet::from([f.profile_a.to_str().unwrap(), f.profile_b.to_str().unwrap()])
    );
    assert!(
        f.registry.selected().unwrap().is_none(),
        "列候选不得自动选第一项"
    );
    assert!(
        f.registry.registered_ids().unwrap().is_empty(),
        "列候选不得偷偷登记可信身份"
    );
}

#[test]
fn instance_list_keeps_desktop_epoch_stable_and_changes_it_after_restart() {
    let f = Fixture::new();
    let a_id = f.id_for(&f.profile_a);
    let b_id = f.id_for(&f.profile_b);
    let epoch = |id: &str| -> String {
        let candidates = f.candidates();
        let row = candidates
            .iter()
            .find(|row| row["instanceId"] == id)
            .unwrap();
        assert_eq!(row["live"], true);
        row["hostEpoch"]
            .as_str()
            .expect("已核验活体须提供字符串代际")
            .to_owned()
    };
    let a_before = epoch(&a_id);
    let b_before = epoch(&b_id);
    assert_eq!(epoch(&a_id), a_before, "同一宿主代际必须稳定");
    let discovered =
        multi_agents_manager_lib::services::dsh_control::identity::discover_all(&f.env).unwrap();
    let a_host = discovered
        .iter()
        .find(|candidate| candidate.instance_id == a_id)
        .unwrap();
    assert!(a_host.verified);
    assert_eq!(a_before, a_host.host.epoch.to_string());
    // 保留 PID、profile 和端口，只改变进程启动时刻，模拟 PID 复用后的重启。
    f.facts
        .processes
        .lock()
        .unwrap()
        .iter_mut()
        .find(|p| p.pid == 41001)
        .unwrap()
        .started_at_ms += 1;
    assert_ne!(epoch(&a_id), a_before, "同实例重启必须换代");
    assert_eq!(epoch(&b_id), b_before, "另一实例的代际不得受影响");
}

#[test]
fn instance_list_discards_desktop_epoch_when_selected_instance_goes_offline() {
    use multi_agents_manager_lib::services::dsh_control::instances::HostKind;
    let f = Fixture::new();
    let a = f
        .registry
        .register_candidate(HostKind::Desktop, &f.profile_a)
        .unwrap();
    f.registry.select(&a.instance_id).unwrap();
    let before = f
        .candidates()
        .into_iter()
        .find(|row| row["instanceId"] == a.instance_id)
        .unwrap();
    assert!(before["hostEpoch"].as_str().is_some());
    f.facts.remove_host(41001);
    let offline = f
        .candidates()
        .into_iter()
        .find(|row| row["instanceId"] == a.instance_id)
        .unwrap();
    assert_eq!(offline["live"], false);
    assert_eq!(offline["selected"], true);
    assert!(
        offline.get("hostEpoch").unwrap().is_null(),
        "离线不得返回旧代际"
    );
}

#[test]
fn instance_list_marks_only_persisted_b_selected_when_b_is_offline() {
    use multi_agents_manager_lib::services::dsh_control::instances::HostKind;

    let f = Fixture::new();
    let a = f
        .registry
        .register_candidate(HostKind::Desktop, &f.profile_a)
        .unwrap();
    let b = f
        .registry
        .register_candidate(HostKind::Desktop, &f.profile_b)
        .unwrap();
    f.registry.select(&b.instance_id).unwrap();
    f.facts.remove_host(41002);

    let reopened = InstanceRegistry::open_with_env(&f.registry_path, Some(f.env.clone())).unwrap();
    let candidates = serde_json::to_value(instance_list_core(&f.env, &reopened).unwrap()).unwrap();
    let candidates = candidates.as_array().unwrap();
    assert_eq!(candidates.len(), 2);
    let a_dto = candidates
        .iter()
        .find(|c| c["instanceId"] == a.instance_id)
        .unwrap();
    let b_dto = candidates
        .iter()
        .find(|c| c["instanceId"] == b.instance_id)
        .unwrap();
    assert_eq!(a_dto["live"], true);
    assert_eq!(a_dto["selected"], false);
    assert_eq!(b_dto["live"], false);
    assert_eq!(b_dto["selected"], true);
}

#[test]
fn instance_list_marks_all_unselected_when_no_instance_is_selected() {
    use multi_agents_manager_lib::services::dsh_control::instances::HostKind;

    let f = Fixture::new();
    let a = f
        .registry
        .register_candidate(HostKind::Desktop, &f.profile_a)
        .unwrap();
    let b = f
        .registry
        .register_candidate(HostKind::Desktop, &f.profile_b)
        .unwrap();
    f.facts.remove_host(41002);
    assert!(f.registry.selected().unwrap().is_none());

    let candidates = f.candidates();
    assert_eq!(candidates.len(), 2);
    let a_dto = candidates
        .iter()
        .find(|c| c["instanceId"] == a.instance_id)
        .unwrap();
    let b_dto = candidates
        .iter()
        .find(|c| c["instanceId"] == b.instance_id)
        .unwrap();
    assert_eq!(a_dto["live"], true);
    assert_eq!(b_dto["live"], false);
    assert_eq!(a_dto["selected"], false);
    assert_eq!(b_dto["selected"], false);
}

#[test]
fn multiple_instances_require_explicit_selection_before_implicit_status_query() {
    let f = Fixture::new();
    assert_eq!(f.candidates().len(), 2, "前置：两实例都可见");
    let error = f
        .status(None)
        .expect_err("无明确选择时状态查询必须拒绝歧义");
    expect_code(&error, codes::TARGET_AMBIGUOUS);
    assert!(f.registry.selected().unwrap().is_none());
}

#[test]
fn selecting_b_persists_b_profile_instead_of_default_home_a() {
    let f = Fixture::new();
    let id_b = f.id_for(&f.profile_b);
    f.select(&id_b);
    let saved = f
        .registry
        .selected()
        .unwrap()
        .expect("命令必须真的持久化选择");
    assert_eq!(saved.instance_id, id_b);
    assert_eq!(saved.profile_dir, f.profile_b);
    assert!(
        saved.identity_fingerprint.is_empty(),
        "候选配置不能冒充受信锚定"
    );
}

#[test]
fn querying_selected_b_returns_b_management_state_without_granting_authorization() {
    let f = Fixture::new();
    let id_b = f.id_for(&f.profile_b);
    f.select(&id_b);
    let status = f
        .status(Some(&id_b))
        .expect("选定且在场的 B 可查本机安装状态");
    assert_eq!(status["instanceId"], id_b);
    assert_eq!(status["installation"], "missing");
    assert_eq!(status["activation"], "disabled");
    assert_eq!(status["authorization"], "unbound");
    assert!(status
        .get("bindingId")
        .expect("必须显式携带绑定字段")
        .is_null());
    assert!(f
        .registry
        .selected()
        .unwrap()
        .unwrap()
        .identity_fingerprint
        .is_empty());
}

#[test]
fn selected_candidate_is_still_rejected_by_trusted_resolution_without_a_real_handshake() {
    let f = Fixture::new();
    let id_b = f.id_for(&f.profile_b);
    f.select(&id_b);
    // 不接 SelfConsistentProbe，不回显已存指纹；这里必须是负对照。
    let error = f.registry.resolve_selected().expect_err("选择不等于信任");
    assert_eq!(error.code, codes::INSTANCE_UNVERIFIED);
}

#[test]
fn reentering_with_a_new_registry_uses_persisted_b_selection() {
    let f = Fixture::new();
    let id_b = f.id_for(&f.profile_b);
    f.select(&id_b);
    let reopened = InstanceRegistry::open_with_env(&f.registry_path, Some(f.env.clone())).unwrap();
    let status = instance_status_core(&f.env, &reopened, None).expect("重开后按保存的 B 查询");
    let status = serde_json::to_value(status).unwrap();
    assert_eq!(status["instanceId"], id_b);
    assert_eq!(status["authorization"], "unbound");
    assert_eq!(
        reopened.selected().unwrap().unwrap().profile_dir,
        f.profile_b
    );
}

#[test]
fn selected_b_disappearing_never_falls_back_to_the_remaining_a() {
    let f = Fixture::new();
    let id_b = f.id_for(&f.profile_b);
    f.select(&id_b);
    f.facts.remove_host(41002);
    // 列表现在同时报「已登记但未运行」的条目（B 会以 live=false 出现），
    // 因此这里按 **live** 过滤来构造「只剩 A 在跑」的前置——断言意图不变：
    // B 失效时必须显式失败，不得回落到唯一活着的 A。
    let all = f.candidates();
    let remaining: Vec<_> = all.iter().filter(|c| c["live"] == true).collect();
    assert_eq!(
        remaining.len(),
        1,
        "前置：只剩 A 在跑，存在错误回落诱因；实际列表：{all:?}"
    );
    assert_eq!(remaining[0]["profileDir"], f.profile_a.to_str().unwrap());
    // B 仍作为离线条目在列表里（这正是「退出后还能重开」的数据前提）。
    assert!(
        all.iter()
            .any(|c| c["instanceId"] == id_b && c["live"] == false),
        "已登记但未运行的 B 必须以 live=false 出现在列表里：{all:?}"
    );
    for target in [None, Some(id_b.as_str())] {
        let error = f
            .status(target)
            .expect_err("B 失效必须显式失败，不得给出 A 状态");
        expect_code(&error, codes::HOST_NOT_RUNNING);
    }
    assert_eq!(f.registry.selected().unwrap().unwrap().instance_id, id_b);
}

#[test]
fn explicit_query_for_a_cannot_silently_replace_selected_b() {
    let f = Fixture::new();
    let id_a = f.id_for(&f.profile_a);
    let id_b = f.id_for(&f.profile_b);
    f.select(&id_b);
    let error = f
        .status(Some(&id_a))
        .expect_err("过时页面的 A 请求不能悄悄切回 A");
    expect_code(&error, codes::INSTANCE_SWITCHED);
    assert_eq!(f.registry.selected().unwrap().unwrap().instance_id, id_b);
}

#[test]
fn b_restart_with_new_pid_and_port_preserves_the_selected_stable_id() {
    let f = Fixture::new();
    let id_b = f.id_for(&f.profile_b);
    f.select(&id_b);
    f.facts.remove_host(41002);
    f.facts
        .processes
        .lock()
        .unwrap()
        .insert(0, desktop_process(42002, &f.profile_b, 9_000));
    f.facts.listeners.lock().unwrap().push(DshListener {
        owner_pid: 42002,
        authority: "127.0.0.1:6199".into(),
    });
    assert_eq!(
        f.id_for(&f.profile_b),
        id_b,
        "重启不能把 PID/端口变成实例 ID"
    );
    let status = f.status(Some(&id_b)).expect("新代际仅可查未授权配置状态");
    assert_eq!(status["instanceId"], id_b);
    assert_eq!(status["authorization"], "unbound");
}

#[test]
fn a_disappeared_candidate_cannot_be_selected_from_a_stale_list() {
    let f = Fixture::new();
    let id_a = f.id_for(&f.profile_a);
    let id_b = f.id_for(&f.profile_b);
    f.select(&id_a);
    f.facts.remove_host(41002);
    let error = instance_select_core(&f.env, &f.registry, request(&id_b), &|_| {})
        .expect_err("选择必须重新核对候选，不能只信 UI 旧 ID");
    expect_code(&error, codes::HOST_NOT_RUNNING);
    assert_eq!(f.registry.selected().unwrap().unwrap().instance_id, id_a);
}

#[test]
fn profile_paths_are_not_accepted_as_instance_ids_by_select_or_query() {
    let f = Fixture::new();
    let id_b = f.id_for(&f.profile_b);
    f.select(&id_b);
    for path in [
        f.profile_a.to_string_lossy().into_owned(),
        "../home-a/profiles/desktop".into(),
        r"C:\Users\bunny\.dsh\profiles\desktop".into(),
        "file:///tmp/dsh/profiles/desktop".into(),
    ] {
        assert!(
            instance_select_core(&f.env, &f.registry, request(&path), &|_| {}).is_err(),
            "选择收了路径 {path}"
        );
        assert!(f.status(Some(&path)).is_err(), "查询收了路径 {path}");
    }
    assert_eq!(f.registry.selected().unwrap().unwrap().instance_id, id_b);
    assert_eq!(f.registry.registered_ids().unwrap(), vec![id_b]);
}

#[test]
fn request_dto_rejects_path_or_command_fields_even_alongside_a_valid_id() {
    let f = Fixture::new();
    let id_b = f.id_for(&f.profile_b);
    for field in ["profileDir", "path", "command", "packageName"] {
        let mut payload = json!({ "instanceId": id_b });
        payload[field] = json!("arbitrary-input");
        assert!(
            serde_json::from_value::<DshInstanceRequest>(payload).is_err(),
            "本机 ID 请求不能接收 {field}"
        );
    }
    assert!(f.registry.registered_ids().unwrap().is_empty());
}

#[test]
fn unknown_well_formed_id_is_not_registered_or_replaced_with_the_first_candidate() {
    let f = Fixture::new();
    let unknown_id = "ffffffffffffffffffffffffffffffff";
    assert!(instance_select_core(&f.env, &f.registry, request(unknown_id), &|_| {}).is_err());
    assert!(f.status(Some(unknown_id)).is_err());
    assert!(f.registry.selected().unwrap().is_none());
    assert!(f.registry.registered_ids().unwrap().is_empty());
}

#[test]
fn corrupted_registry_is_propagated_instead_of_recovering_to_default_home_a() {
    let f = Fixture::new();
    let id_b = f.id_for(&f.profile_b);
    f.select(&id_b);
    std::fs::write(&f.registry_path, b"not valid registry JSON").unwrap();
    let error = f
        .status(Some(&id_b))
        .expect_err("登记读取错误必须传播，不能走 legacy A");
    expect_code(&error, codes::STATE_IO);
}

#[tokio::test]
async fn status_reuses_the_same_manager_so_active_jobs_are_not_lost() {
    use multi_agents_manager_lib::services::dsh_control::auth::DshAuthService;
    use multi_agents_manager_lib::services::dsh_control::install::DshPluginManager;
    use std::sync::{Arc, Mutex};

    let f = Fixture::new();
    let id_b = f.id_for(&f.profile_b);
    f.select(&id_b);

    // 模拟生产缓存：同一实例恒返回同一 (manager, auth) Arc。
    let cache: Arc<Mutex<Option<Arc<DshPluginManager>>>> = Arc::new(Mutex::new(None));
    let auth_cache: Arc<Mutex<Option<Arc<DshAuthService>>>> = Arc::new(Mutex::new(None));
    let env = f.env.clone();
    let services = |id: &str| {
        let mut manager_slot = cache.lock().unwrap();
        if let Some(existing) = manager_slot.as_ref() {
            let auth = auth_cache.lock().unwrap().as_ref().map(Arc::clone);
            let auth = auth
                .ok_or_else(|| DshError::new(codes::INSTANCE_UNVERIFIED, "auth cache missing"))?;
            return Ok((Arc::clone(existing), auth));
        }
        let auth = Arc::new(
            multi_agents_manager_lib::services::dsh_control::production::init_instance_auth(
                &env, id,
            )?,
        );
        let registered = f.registry.registered(id)?;
        let base = Arc::new(
            multi_agents_manager_lib::services::dsh_control::model::DshEnvironment {
                home: env.home.clone(),
                tuvis_home: env.tuvis_home.clone(),
                dsh_state_dir: env.dsh_state_dir.clone(),
                processes: env.processes.clone(),
                files: env.files.clone(),
                runner: env.runner.clone(),
                clock: env.clock.clone(),
                // 测试默认「不可观测」：显式拒绝 Web home 归属（fail-closed），
                // 绝不让假进程源落到真实系统 PID 上。需要 Web 归属的用例自行注入假观测器。
                environments: std::sync::Arc::new(multi_agents_manager_lib::services::dsh_control::model::TestUnobservableEnvironments),
            },
        );
        let auth_for_revoker = Arc::clone(&auth);
        let revoker = multi_agents_manager_lib::services::dsh_control::production::InstanceBindingRevoker::with_resolver(
            base,
            id.to_owned(),
            Arc::new(move || Ok(Arc::clone(&auth_for_revoker))),
        );
        let manager = Arc::new(
            multi_agents_manager_lib::services::dsh_control::production::init_instance_manager(
                &env,
                &registered,
                Arc::new(revoker),
            )
            .with_authorization_reader(Arc::new(
                multi_agents_manager_lib::services::dsh_control::auth::AuthServiceReader(
                    Arc::clone(&auth),
                ),
            )),
        );
        *manager_slot = Some(Arc::clone(&manager));
        *auth_cache.lock().unwrap() = Some(Arc::clone(&auth));
        Ok((manager, auth))
    };

    let (manager, _) = services(&id_b).expect("首建实例服务");
    let job_id = multi_agents_manager_lib::commands::dsh::plugin_start_core(
        &manager,
        multi_agents_manager_lib::services::dsh_control::model::ManagementKind::Install,
    )
    .expect("安装作业受理");

    let value = multi_agents_manager_lib::commands::dsh::instance_status_core_with(
        &f.env,
        &f.registry,
        Some(request(&id_b)),
        &services,
    )
    .expect("状态可查");
    assert_eq!(
        value["operation"]["jobId"].as_str(),
        Some(job_id.as_str()),
        "状态必须带回同一 manager 的活跃作业: {value}"
    );
}

#[test]
/// 切换实例**不**撤销上一实例的控制台能力（P1-4，2026-11-14 用户裁决：
/// 切走关页 ≠ 撤权，推翻旧契约「切换即 `revoke_instance(previous)`」）。
///
/// 切走只做 close-pages（drain 该实例活连接），能力与凭据**保留**——切回该实例
/// 时凭保留凭据直接打开控制台，无需重交付；「撤销该实例授权」收敛到撤权链
/// （`revoke_local` → `revoke_console_access_for_instance`）。关页 + 凭据保留 +
/// 活连接取消的完整回归见 `tests/dsh_console_switch_close_pages_test.rs`。
fn selecting_another_instance_keeps_the_previous_console_capability() {
    use multi_agents_manager_lib::remote::console_capability::ConsoleCapability;
    use multi_agents_manager_lib::remote::console_mount;

    let f = Fixture::new();
    let id_a = f.id_for(&f.profile_a);
    let id_b = f.id_for(&f.profile_b);
    f.select(&id_a);

    // 为 A 登记一条能力（生产单例默认 fail-closed，但撤销侧不依赖设备有效）。
    let mount = console_mount::production();
    let token = format!("{:032x}", rand::random::<u128>());
    mount
        .register(ConsoleCapability {
            token: token.clone(),
            device_id: "dev-switch".into(),
            instance_id: id_a.clone().to_string(),
            authority: "127.0.0.1:1".into(),
            epoch: 1,
            bootstrap: false,
            expires_at_ms: chrono::Utc::now().timestamp_millis() + 600_000,
        })
        .expect("能力可登记");
    // 直接从能力表观测（`lookup_for_test` 会因设备 fail-closed 恒为 None，
    // 无法区分「被撤销」与「设备无效」，故用内部只读计数）。
    assert!(
        mount.has_capability_for_test(&token),
        "登记后能力必须存在（否则本用例无观测面）"
    );

    f.select(&id_b);

    assert!(
        mount.has_capability_for_test(&token),
        "切换实例不得撤销旧实例的控制台能力（切走只关页面，凭据与能力保留供切回）"
    );
}

// ============================================================
// P2：重开必须按宿主类型分流
//
// 旧实现不分 kind 一律 `spawn_desktop_app`：选中的是 **Web** 实例时，「重开」
// 会拉起 Desktop 应用——启动了一个用户没在等的程序，且那个应用的 home 可能
// 属于别的实例。Web 宿主只能由用户在自己终端里跑 `dsh web`，我们给命令与 home。
// ============================================================

/// Web 实例的重开请求必须被明确拒绝（不启动任何 Desktop 应用）。
///
/// 判据：错误码为 `DSH_UNSUPPORTED`，且错误文本里带上该实例的真实 home
/// （用户要能照着它在自己终端里重开）。若实现退回「一律 Desktop」，这里会
/// 拿到 `DSH_INSTALL_NOT_FOUND` 或 `DSH_PROFILE_NOT_INITIALIZED` 之类的
/// 不相干错误——那是本条要抓的缺陷。
#[test]
fn restart_refuses_a_web_instance_instead_of_launching_the_desktop_app() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("web-home");
    let profile = home.join("profiles/web");
    std::fs::create_dir_all(&profile).unwrap();
    std::fs::write(
        profile.join("package.json"),
        br#"{"name":"dsh-profile-web","dependencies":{},"dsh":{"profile":{"bundles":[]}}}"#,
    )
    .unwrap();
    let profile = std::fs::canonicalize(profile).unwrap();

    let facts = Arc::new(ProcessFacts::default());
    let env = DshEnvironment {
        home: std::fs::canonicalize(&home).unwrap(),
        tuvis_home: root.path().join("tuvis-home"),
        dsh_state_dir: None,
        processes: facts.clone(),
        files: Arc::new(multi_agents_manager_lib::services::dsh_control::production::FsFileSource),
        runner: Arc::new(NoCommands),
        clock: Arc::new(FixedClock),
        environments: Arc::new(
            multi_agents_manager_lib::services::dsh_control::model::TestUnobservableEnvironments,
        ),
    };
    let registry = InstanceRegistry::open(root.path().join("instances.json")).unwrap();
    let registered = registry
        .register_candidate_with_home(
            multi_agents_manager_lib::services::dsh_control::instances::HostKind::Web,
            &profile,
            Some(&env.home),
        )
        .expect("Web 实例可登记");
    registry.select(&registered.instance_id).expect("可选中");

    let error = dsh_restart_host_core(
        &env,
        &registry,
        DshInstanceRequest {
            instance_id: registered.instance_id.clone(),
        },
    )
    .expect_err("Web 实例不得走 Desktop 启动器");

    assert!(
        error.contains(codes::UNSUPPORTED),
        "Web 重开必须明确报「不支持由本应用启动」，实际：{error}"
    );
    let expected_home = profile.parent().and_then(|p| p.parent()).unwrap();
    assert!(
        error.contains(&expected_home.to_string_lossy().to_string()),
        "错误文本必须带上该实例的真实 home（用户照着它跑 dsh web）：{error}"
    );
}
