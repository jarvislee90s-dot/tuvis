//! H04：双宿主实例登记及可靠选择（计划 §5 H04 / §3.1 接口契约；审查 I1—I5 修订）。
//!
//! 全部输入经注入式 [`DshEnvironment`]（真实临时目录 + 假进程/监听源）：
//! 登记面用真实文件系统（fs2 文件锁与 canonicalize 语义需要真实目录），进程/
//! 监听全注入，不触真实 `~/.dsh`、真实 DSH 进程、真实网络（计划全局约束）。
//!
//! RED 证据（报告如实申报）：`reused_port_does_not_reuse_identity`（在途）在
//! dsh_identity_test 以行为失败固定（实现前两侧 host_id 均为 `local`）；
//! 审查 I1—I5 的新行为（verified 门 / 指纹门 / 形态门 / 发送归属门）在实现前
//! 无 API 可表达——行为由本文件 GREEN 用例固定，RED 形态在 h04-report.md 申报。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use multi_agents_manager_lib::services::dsh_control::error::{codes, DshError};
use multi_agents_manager_lib::services::dsh_control::identity::{
    discover, discover_all, discover_detailed,
};
use multi_agents_manager_lib::services::dsh_control::instances::{
    instance_id_for, HostKind, InstanceIdentityProbe, InstanceRegistry, SelectedInstance,
};
use multi_agents_manager_lib::services::dsh_control::model::{
    CommandRequest, DshClock, DshCommandRunner, DshEnvironment, DshFileSource, DshListener,
    DshProcess, DshProcessSource, RunOutcome,
};

// ============================================================
// 注入假体（真实文件系统 + 可变进程/监听源）
// ============================================================

/// 真实文件源的只读投影（存在性 / 类型 / 目录枚举 / 预算内小文件）。
#[derive(Default)]
struct RealFiles;

impl DshFileSource for RealFiles {
    fn exists(&self, path: &Path) -> bool {
        std::fs::symlink_metadata(path).is_ok()
    }

    /// 类型核验（审查 I3）：真实 stat 判目录。
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

    fn read_small(&self, path: &Path, max_bytes: u64) -> Result<Vec<u8>, DshError> {
        // 真实 fs 只读投影；测试夹具不种包描述文件 → Err（discover 按未知降级为
        // 空指纹，不编造）。
        let meta = std::fs::metadata(path)
            .map_err(|e| DshError::new(codes::STATE_IO, format!("stat {}: {e}", path.display())))?;
        if meta.len() > max_bytes {
            return Err(DshError::new(
                codes::STATE_IO,
                format!("file over budget: {}", path.display()),
            ));
        }
        std::fs::read(path).map_err(|e| DshError::new(codes::STATE_IO, format!("read: {e}")))
    }
}

/// 可变进程/监听源（`lost_selection_never_falls_back_to_first_host` 需要在
/// 不重建 registry 的情况下让实例「消失」）。
#[derive(Default)]
struct SharedProcs {
    processes: Mutex<Vec<DshProcess>>,
    listeners: Mutex<Vec<DshListener>>,
}

impl SharedProcs {
    fn kill(&self, pid: u32) {
        self.processes.lock().unwrap().retain(|p| p.pid != pid);
        self.listeners
            .lock()
            .unwrap()
            .retain(|l| l.owner_pid != pid);
    }
}

impl DshProcessSource for SharedProcs {
    fn list_processes(&self) -> Vec<DshProcess> {
        self.processes.lock().unwrap().clone()
    }

    fn listening_sockets(&self) -> Vec<DshListener> {
        self.listeners.lock().unwrap().clone()
    }
}

struct NullRunner;
impl DshCommandRunner for NullRunner {
    fn run(&self, _request: &CommandRequest) -> Result<RunOutcome, DshError> {
        unreachable!("实例登记测试不执行命令")
    }
}

struct FixedClock(i64);
impl DshClock for FixedClock {
    fn now_ms(&self) -> i64 {
        self.0
    }
}

/// 自洽指纹探针（审查 I2 测试面）：回显登记值（当代来源核验通过）。
struct SelfConsistentProbe;
impl InstanceIdentityProbe for SelfConsistentProbe {
    fn current_fingerprint(&self, instance: &SelectedInstance) -> Result<Option<String>, DshError> {
        Ok(Some(instance.identity_fingerprint.clone()))
    }
}

/// 恒定指纹探针（当代观测值由测试给定；None = 来源不可用）。
struct FixedProbe(Option<String>);
impl InstanceIdentityProbe for FixedProbe {
    fn current_fingerprint(
        &self,
        _instance: &SelectedInstance,
    ) -> Result<Option<String>, DshError> {
        Ok(self.0.clone())
    }
}

// ============================================================
// 夹具构造（真实临时目录；profile 形态按 H02 取证：各 profile 独占 package.json）
// ============================================================

/// Desktop 宿主 argv（双特征：`dsh-desktop-host` 包令牌 + `profiles/desktop`
/// 尾段；profile 目录真实存在）。
fn desktop_host_process(pid: u32, home: &Path, started_at_ms: i64) -> DshProcess {
    let profile = home.join("profiles").join("desktop");
    DshProcess {
        pid,
        exe: PathBuf::from("/Applications/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness"),
        cmdline: vec![
            "/Applications/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness".into(),
            "--expose-internals".into(),
            "/Applications/DeepSeek Harness.app/Contents/Resources/app.asar/dsh/node_modules/@deepseek-ai/dsh-desktop-host/lib/index.js".into(),
            profile.to_string_lossy().to_string(),
        ],
        started_at_ms,
    }
}

/// Web 宿主 argv（`dsh`+`web` 令牌；H02：Web argv 不含 home——home 归属无正面
/// 证据，候选一律未核验）。
fn web_host_process(pid: u32, started_at_ms: i64) -> DshProcess {
    DshProcess {
        pid,
        exe: PathBuf::from("/usr/local/bin/node"),
        cmdline: vec![
            "/usr/local/bin/node".into(),
            "/opt/dsh/bin/dsh".into(),
            "web".into(),
        ],
        started_at_ms,
    }
}

/// 残缺 Desktop 形态（只有包令牌、缺 profile 令牌——审查 I1：不得算进 Web）。
fn partial_desktop_process(pid: u32, started_at_ms: i64) -> DshProcess {
    DshProcess {
        pid,
        exe: PathBuf::from("/usr/local/bin/node"),
        cmdline: vec![
            "/usr/local/bin/node".into(),
            "/opt/dsh/lib/dsh-desktop-host/index.js".into(),
            "web".into(),
        ],
        started_at_ms,
    }
}

fn listener(pid: u32, authority: &str) -> DshListener {
    DshListener {
        owner_pid: pid,
        authority: authority.into(),
    }
}

/// 种一个已初始化 profile（目录 + 官方标记文件 `package.json`）。
fn seed_profile(home: &Path, kind: &str) -> PathBuf {
    let profile = home.join("profiles").join(kind);
    std::fs::create_dir_all(&profile).unwrap();
    std::fs::write(profile.join("package.json"), b"{}").unwrap();
    profile
}

/// 种一个主会话目录（`<home>/sessions/<project>/<session_id>/`；归属核验只做
/// 只读存在性/类型判断，不触碰会话内容——§3.3 数据目录线索口径）。
fn seed_session(home: &Path, project: &str, session_id: &str) {
    let dir = home.join("sessions").join(project).join(session_id);
    std::fs::create_dir_all(&dir).unwrap();
}

/// 组装注入环境（env.home = web 侧 home；desktop 侧 home 由 argv 携带）。
fn env_with(
    web_home: &Path,
    processes: Vec<DshProcess>,
    listeners: Vec<DshListener>,
) -> (DshEnvironment, Arc<SharedProcs>) {
    let procs = Arc::new(SharedProcs {
        processes: Mutex::new(processes),
        listeners: Mutex::new(listeners),
    });
    let env = DshEnvironment {
        home: web_home.to_path_buf(),
        tuvis_home: web_home.join("tuvis-shadow"),
        dsh_state_dir: None,
        processes: Arc::clone(&procs) as _,
        files: Arc::new(RealFiles) as _,
        runner: Arc::new(NullRunner) as _,
        clock: Arc::new(FixedClock(0)) as _,
        // 测试默认「不可观测」：显式拒绝 Web home 归属（fail-closed），
        // 绝不让假进程源落到真实系统 PID 上。需要 Web 归属的用例自行注入假观测器。
        environments: std::sync::Arc::new(
            multi_agents_manager_lib::services::dsh_control::model::TestUnobservableEnvironments,
        ),
    };
    (env, procs)
}

/// 注入环境的浅克隆（适配器 Arc 共享——`DshEnvironment` 无 Clone，手工复制）。
fn clone_env(env: &DshEnvironment) -> DshEnvironment {
    DshEnvironment {
        home: env.home.clone(),
        tuvis_home: env.tuvis_home.clone(),
        dsh_state_dir: env.dsh_state_dir.clone(),
        processes: Arc::clone(&env.processes),
        files: Arc::clone(&env.files),
        runner: Arc::clone(&env.runner),
        clock: Arc::clone(&env.clock),
        // 克隆必须**传递**观测器：它和 processes/files 一样是注入面的一部分。
        // 在这里重置成默认值会让「注入了假观测器的 env」在登记侧静默失去观测
        // 能力，把「能发现」变成「不能解析」——正是本文件要守的那条链。
        environments: Arc::clone(&env.environments),
    }
}

fn registry(env: &DshEnvironment, dir: &Path) -> InstanceRegistry {
    InstanceRegistry::open_with_env(dir.join("instances.json"), Some(Arc::new(clone_env(env))))
        .unwrap()
}

/// 带自洽指纹探针的登记（已登记实例的 resolve 需要当代指纹核验——审查 I2）。
fn registry_with_probe(env: &DshEnvironment, dir: &Path) -> InstanceRegistry {
    registry(env, dir).with_identity_probe(Arc::new(SelfConsistentProbe))
}

/// 双实例夹具：Desktop（home-a）+ Web（env.home = home-b）同时在场，均已登记。
fn two_live_instances() -> (
    tempfile::TempDir,
    DshEnvironment,
    Arc<SharedProcs>,
    SelectedInstance,
    SelectedInstance,
) {
    let root = tempfile::tempdir().unwrap();
    let home_a = root.path().join("home-a");
    let home_b = root.path().join("home-b");
    std::fs::create_dir_all(&home_a).unwrap();
    std::fs::create_dir_all(&home_b).unwrap();
    let desktop_profile = seed_profile(&home_a, "desktop");
    seed_profile(&home_b, "web");
    let (env, procs) = env_with(
        &home_b,
        vec![
            desktop_host_process(100, &home_a, 1_000),
            web_host_process(200, 2_000),
        ],
        vec![
            listener(100, "127.0.0.1:5001"),
            listener(200, "127.0.0.1:5002"),
        ],
    );
    let registry = registry(&env, root.path());
    let a = registry
        .register(HostKind::Desktop, &desktop_profile, "key-a")
        .unwrap();
    let web_profile = home_b.join("profiles").join("web");
    let b = registry
        .register(HostKind::Web, &web_profile, "key-b")
        .unwrap();
    drop(registry);
    (root, env, procs, a, b)
}

#[test]
fn readonly_unknown_oversized_header_cannot_prove_uniqueness() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let profile = seed_profile(&home, "desktop");
    let (env, _) = env_with(&home, vec![], vec![]);
    let reg = registry_with_probe(&env, root.path());
    let selected = reg.register(HostKind::Desktop, &profile, "key-a").unwrap();
    reg.select(&selected.instance_id).unwrap();
    let target = home.join("sessions/project/session-target");
    let unknown = home.join("sessions/project/session-unknown");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::create_dir_all(&unknown).unwrap();
    std::fs::write(
        target.join("session.jsonl"),
        b"{\"id\":\"session-target\"}\n",
    )
    .unwrap();
    let header = format!(
        "{{\"id\":\"other\",\"padding\":\"{}\"}}\n",
        "x".repeat(70_000)
    );
    std::fs::write(unknown.join("session.jsonl"), header).unwrap();
    assert!(reg
        .resolve_readonly_root(&selected.instance_id, "session-target")
        .is_err());
}

// ============================================================
// 计划指定 RED 用例
// ============================================================

/// 计划指定 RED-1：无选择报歧义。多宿主同时在场且未做选择时，控制面必须报
/// `DSH_TARGET_AMBIGUOUS`——discover 兼容委托不得取第一项，resolve_selected
/// 不得自动替用户选；已核验 + 未核验宿主并存同样算多宿主（审查 I5：不静默）。
#[test]
fn multiple_hosts_require_explicit_selection() {
    let (_root, env, _procs, a, b) = two_live_instances();
    // 兼容委托：已核验 Desktop + 未核验 Web 并存 = 多宿主，必须报歧义。
    let err = discover(&env).unwrap_err();
    assert_eq!(err.code, codes::TARGET_AMBIGUOUS, "多宿主必须歧义：{err}");
    // R01 分层：未取得实际 home 证明的 Web **不产候选**（只保留原始进程事实），
    // 因此候选只剩已核验 Desktop；但宿主计数仍必须把 Web 算进多宿主。
    let candidates = discover_all(&env).unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].instance_id, a.instance_id);
    assert!(candidates[0].verified);
    assert!(
        candidates.iter().all(|c| c.instance_id != b.instance_id),
        "未归属 Web 不得以候选身份出现"
    );
    let detailed = discover_detailed(&env).unwrap();
    assert_eq!(detailed.raw_web_hosts.len(), 1, "原始 Web 事实必须保留");
    // 登记面：无选择时 resolve_selected 报歧义，绝不悄悄替用户选。
    let registry = registry_with_probe(&env, _root.path());
    assert_eq!(registry.selected().unwrap(), None);
    let err = registry.resolve_selected().unwrap_err();
    assert_eq!(
        err.code,
        codes::TARGET_AMBIGUOUS,
        "无选择 + 多宿主必须报歧义：{err}"
    );
    // 显式选择 A（Desktop，已核验）后：resolve_selected 返回 A 的当代活体。
    registry.select(&a.instance_id).unwrap();
    let host = registry.resolve_selected().unwrap();
    assert_eq!(host.host_id, a.instance_id);
    assert_eq!(host.authority, "127.0.0.1:5001");
}

/// 审查 I1：Web 宿主 home 无正面证据（argv 无 home，H02）——env.home 的
/// `profiles/web` 标记 + 监听属主只是间接线索（A profile 残留 + 唯一活跃 Web
/// 实际用 B shadow home 的反例不能被排除）。候选可产（供登记可见）但
/// `verified = false`；resolve_selected / resolve_session 一律不消费。
#[test]
fn selected_web_instance_is_not_resolvable_until_verified() {
    let (_root, env, _procs, _a, b) = two_live_instances();
    let registry = registry_with_probe(&env, _root.path());
    registry.select(&b.instance_id).unwrap();
    let err = registry.resolve_selected().unwrap_err();
    // R01 分层：Web 未取得实际 home 证明前**不产候选**（原始事实另存），
    // 因此这里报「不在场」而非「未核验候选」；拒绝语义与不回落不变。
    assert!(
        matches!(
            err.code,
            codes::INSTANCE_UNVERIFIED | codes::HOST_NOT_RUNNING
        ),
        "未核验 Web 实例不得被 resolve 消费：{err}"
    );
    // 会话归属同样拒绝（归属未证 → 不能给可写 Target）。
    let err = registry
        .resolve_session(&b.instance_id, "session-01234567")
        .unwrap_err();
    assert!(
        matches!(
            err.code,
            codes::INSTANCE_UNVERIFIED | codes::HOST_NOT_RUNNING
        ),
        "未核验实例的会话归属不得成立：{err}"
    );
}

/// 计划指定 RED-2：同名会话不串（H02 补测：两实例同名会话、不同 sessionId）。
/// resolve_session 以「已核验活体 + 目录类型/范围 + 形态 + 无跨实例认领」核验；
/// 证不动一律拒绝（审查 I3 收紧）。
#[test]
fn same_named_sessions_do_not_alias() {
    let (_root, env, _procs, a, b) = two_live_instances();
    let home_a = _root.path().join("home-a");
    let home_b = _root.path().join("home-b");
    // 同名不同 ID（H02 补测形态）：A 的 session-a 与 B 的 session-b 同名
    // TUVIS_H02_SAME_NAME（名字不进归属判据——判据是已核验活体 + 数据目录）。
    seed_session(&home_a, "proj-a", "session-aaa1aaaa");
    seed_session(&home_b, "proj-b", "session-bbb1bbbb");
    // 克隆形态：同一 sessionId 同时存在于两个活体 home 下 → writer 归属不可证。
    seed_session(&home_a, "proj-x", "session-cccccccc");
    seed_session(&home_b, "proj-x", "session-cccccccc");

    let registry = registry_with_probe(&env, _root.path());
    // R01 分层：未归属 Web 在场时，无法排除它持有同名会话 → 保守拒绝归属。
    let blocked = registry
        .resolve_session(&a.instance_id, "session-aaa1aaaa")
        .unwrap_err();
    assert_eq!(
        blocked.code,
        codes::SESSION_OWNERSHIP_UNPROVEN,
        "未归属 Web 在场时不得宣称唯一归属：{blocked}"
    );
    // 跨实例错配拒绝：B 未核验 → 其任何归属请求都拒绝（不猜）。
    let err = registry
        .resolve_session(&b.instance_id, "session-aaa1aaaa")
        .unwrap_err();
    // R01 分层：Web 未归属时不产候选 → 报「不在场」；拒绝语义不变。
    assert!(
        matches!(
            err.code,
            codes::INSTANCE_UNVERIFIED | codes::HOST_NOT_RUNNING
        ),
        "未核验 Web 的任何归属请求必须拒绝：{err}"
    );
    registry
        .resolve_session(&a.instance_id, "session-bbb1bbbb")
        .unwrap_err();
    // 克隆/共享形态：两个活体候选都持有同一 sessionId 目录 → 拒绝（不猜 writer）。
    let err = registry
        .resolve_session(&a.instance_id, "session-cccccccc")
        .unwrap_err();
    assert_eq!(
        err.code,
        codes::SESSION_OWNERSHIP_UNPROVEN,
        "多活体认领同一会话必须拒绝：{err}"
    );
    // 不存在的会话拒绝（历史只读保留，不给可写 Target）。
    let err = registry
        .resolve_session(&a.instance_id, "session-dddddddd")
        .unwrap_err();
    assert_eq!(err.code, codes::SESSION_OWNERSHIP_UNPROVEN);
}

/// 审查 I3：session ID 形态 + 目录类型核验——非原生形态（含路径逃逸向量）与
/// 「同名文件」都不得换出可写 Target。
#[test]
fn session_id_shape_and_type_are_enforced() {
    let (_root, env, _procs, a, _b) = two_live_instances();
    let home_a = _root.path().join("home-a");
    seed_session(&home_a, "proj-a", "session-aaa1aaaa");
    // 同名「文件」（非目录）在场：类型不符 → 拒绝。
    std::fs::write(
        home_a
            .join("sessions")
            .join("proj-a")
            .join("session-eeeeeeee"),
        b"x",
    )
    .unwrap();
    let registry = registry_with_probe(&env, _root.path());
    for bad in [
        "not-a-session",            // 缺前缀
        "session-",                 // 空体
        "session-../escape",        // 点段（形态不符）
        "session-aaa1aaaa/../../x", // 路径分隔（形态不符）
        "session-SHORT",            // 大写/过短
        "session-aaa1aaaa-",        // 尾连字符
    ] {
        let err = registry.resolve_session(&a.instance_id, bad).unwrap_err();
        assert_eq!(
            err.code,
            codes::SESSION_OWNERSHIP_UNPROVEN,
            "非原生形态必须拒绝：{bad}: {err}"
        );
    }
    let err = registry
        .resolve_session(&a.instance_id, "session-eeeeeeee")
        .unwrap_err();
    assert_eq!(
        err.code,
        codes::SESSION_OWNERSHIP_UNPROVEN,
        "同名文件（非目录）不得认领：{err}"
    );
}

/// 计划指定 RED-3：端口复用不换身份（在途用例，位于 dsh_identity_test）。
/// 这里补登记面口径：同一 profile 重启（换 PID/端口）后登记与派生 ID 稳定。
#[test]
fn reused_port_does_not_reuse_identity_across_registry() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let profile = seed_profile(&home, "desktop");
    // 第一代：PID 100、端口 5001。
    let (env1, _p1) = env_with(
        &home,
        vec![desktop_host_process(100, &home, 1_000)],
        vec![listener(100, "127.0.0.1:5001")],
    );
    let first_registry = registry(&env1, root.path());
    let first = first_registry
        .register(HostKind::Desktop, &profile, "key")
        .unwrap();
    // 第二代：同 profile 重启，PID/端口/启动时刻全变（端口甚至复用 5001）。
    let (env2, _p2) = env_with(
        &home,
        vec![desktop_host_process(999, &home, 9_000)],
        vec![listener(999, "127.0.0.1:5001")],
    );
    let registry2 = registry(&env2, root.path());
    let second = registry2
        .register(HostKind::Desktop, &profile, "key")
        .unwrap();
    assert_eq!(
        first, second,
        "同 profile 重启（PID/端口全换代）必须保持同一稳定实例身份"
    );
    assert_ne!(first.instance_id, "local");
    // 审查 I2 残留：该实例**已登记**（虽未 select）——无选择的兼容路径不得
    // 豁免指纹门：探针未接线 → 如实拒绝（不可核验 ≠ 通过）。
    let err = registry2.resolve_selected().unwrap_err();
    assert_eq!(
        err.code,
        codes::INSTANCE_UNVERIFIED,
        "已登记实例不得借无选择路径豁免指纹门：{err}"
    );
    // 接线自洽指纹探针后：同 profile 重启（PID/端口全换代）解析成功且身份稳定。
    let registry2_probed = InstanceRegistry::open_with_env(
        root.path().join("instances.json"),
        Some(Arc::new(clone_env(&env2))),
    )
    .unwrap()
    .with_identity_probe(Arc::new(SelfConsistentProbe));
    let host = registry2_probed.resolve_selected().unwrap();
    assert_eq!(host.host_id, first.instance_id);
    // 兼容豁免只属于**真正未登记**的实例：全新登记文件 + 同一活体 → 直接返回。
    let unregistered = InstanceRegistry::open_with_env(
        root.path().join("unregistered.json"),
        Some(Arc::new(clone_env(&env2))),
    )
    .unwrap();
    let host = unregistered.resolve_selected().unwrap();
    assert_eq!(host.host_id, first.instance_id);
}

/// 计划指定 RED-4：选 A 后 A 消失，绝不改连 B；恢复的唯一出口 = 显式重新选择。
#[test]
fn lost_selection_never_falls_back_to_first_host() {
    let (_root, env, procs, a, b) = two_live_instances();
    // 再种一台已核验 Desktop C（恢复路径用——Web 未核验本就不可解析）。
    let home_c = _root.path().join("home-c");
    std::fs::create_dir_all(&home_c).unwrap();
    let profile_c = seed_profile(&home_c, "desktop");
    procs
        .processes
        .lock()
        .unwrap()
        .push(desktop_host_process(300, &home_c, 3_000));
    procs
        .listeners
        .lock()
        .unwrap()
        .push(listener(300, "127.0.0.1:5003"));
    let registry = registry_with_probe(&env, _root.path());
    let c = registry
        .register(HostKind::Desktop, &profile_c, "key-c")
        .unwrap();
    registry.select(&a.instance_id).unwrap();
    let host = registry.resolve_selected().unwrap();
    assert_eq!(host.host_id, a.instance_id);
    assert_eq!(host.authority, "127.0.0.1:5001");
    // A 消失（进程退出 + 监听关闭）；C 与 B 仍活体。
    procs.kill(100);
    let err = registry.resolve_selected().unwrap_err();
    assert_eq!(
        err.code,
        codes::HOST_NOT_RUNNING,
        "已选实例消失必须明确报错：{err}"
    );
    assert!(
        err.detail.contains("re-select"),
        "错误必须指向「重新选择」出口而非回退：{err}"
    );
    // 选择记录保留（不静默清除）；B/C 仍活体但绝不自动接上。
    assert_eq!(
        registry.selected().unwrap().unwrap().instance_id,
        a.instance_id
    );
    // B 的监听 authority 不得出现在任何解析结果里（未核验 Web 更不可能）。
    let host_b = registry
        .resolve_session(&b.instance_id, "session-aaaaaaaa")
        .unwrap_err();
    assert!(
        !host_b.to_string().contains("127.0.0.1:5002"),
        "A 消失后任何路径都不得把目标换成 B 的监听"
    );
    // 恢复的唯一出口 = 显式重新选择另一台已核验 Desktop。
    registry.select(&c.instance_id).unwrap();
    let host = registry.resolve_selected().unwrap();
    assert_eq!(host.host_id, c.instance_id);
    assert_eq!(host.authority, "127.0.0.1:5003");
}

// ============================================================
// 伪造进程/监听拒绝（计划 RED 要点④）
// ============================================================

/// Web 候选同样要走监听属主核验：web 形态 argv + 名下无回环监听（假监听归属
/// 别人）→ 候选不成形，已选该实例时 resolve_selected 如实报监听缺失。
#[test]
fn forged_web_listener_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let profile = seed_profile(&home, "web");
    // web 形态进程，但监听属主是别的 PID（伪造监听）。
    let (env, _procs) = env_with(
        &home,
        vec![web_host_process(300, 3_000)],
        vec![listener(999, "127.0.0.1:5003")],
    );
    let registry = registry_with_probe(&env, root.path());
    let instance = registry.register(HostKind::Web, &profile, "key").unwrap();
    registry.select(&instance.instance_id).unwrap();
    let err = registry.resolve_selected().unwrap_err();
    // R01 分层：Web 未归属时不产候选，故报「不在场」；伪造监听同样不得通过。
    assert!(
        matches!(
            err.code,
            codes::HOST_LISTENER_MISSING | codes::HOST_NOT_RUNNING
        ),
        "伪造监听不得通过 web 候选核验：{err}"
    );
    assert!(
        !err.detail.contains("127.0.0.1:5003"),
        "拒绝信息不得泄露/采用伪造监听：{err}"
    );
}

/// 伪造的 web 形态进程（第二个 `dsh web` 进程）会让真实 web 宿主变得不可归属
/// ——候选缺席、fail-closed（攻击只能阻断、不能重定向），已选实例随之报错。
#[test]
fn forged_extra_web_process_blocks_attribution_instead_of_redirecting() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let profile = seed_profile(&home, "web");
    // 真实 web 宿主 + 一个伪造的 web 形态进程（各自都有回环监听）。
    let (env, _procs) = env_with(
        &home,
        vec![web_host_process(400, 4_000), web_host_process(401, 4_100)],
        vec![
            listener(400, "127.0.0.1:5004"),
            listener(401, "127.0.0.1:5005"),
        ],
    );
    let registry = registry_with_probe(&env, root.path());
    let instance = registry.register(HostKind::Web, &profile, "key").unwrap();
    registry.select(&instance.instance_id).unwrap();
    let err = registry.resolve_selected().unwrap_err();
    assert_eq!(
        err.code,
        codes::HOST_NOT_RUNNING,
        "归属不可证时必须 fail-closed（不能重定向到任一监听）：{err}"
    );
}

/// 审查 I1（严格形态）：残缺 Desktop 形态（有 `dsh-desktop-host` 包令牌 +
/// `web` 令牌但缺 profile 令牌）不得算进 Web 候选。
#[test]
fn partial_desktop_shape_is_not_a_web_candidate() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    seed_profile(&home, "web");
    let (env, _procs) = env_with(
        &home,
        vec![partial_desktop_process(500, 5_000)],
        vec![listener(500, "127.0.0.1:5006")],
    );
    let candidates = discover_all(&env).unwrap();
    assert!(
        candidates.is_empty(),
        "残缺 Desktop 形态不得进 Web 候选：{candidates:?}"
    );
}

// ============================================================
// 审查 I2：已登记实例的当代指纹核验
// ============================================================

/// 已选择（= 已登记）实例的每次 resolve 都要核对当代受信插件指纹：探针未接线 /
/// 当代来源不可用 → `DSH_INSTANCE_UNVERIFIED`（绝不当作通过）；指纹变化 →
/// `DSH_AUTH_SIGNATURE`；一致 → 放行。
#[test]
fn resolve_requires_continuous_plugin_fingerprint() {
    let (_root, env, _procs, a, _b) = two_live_instances();
    // 探针未接线：已登记选择的 resolve 一律拒绝（fail-closed，不当作通过）。
    let registry = registry(&env, _root.path());
    registry.select(&a.instance_id).unwrap();
    let err = registry.resolve_selected().unwrap_err();
    assert_eq!(
        err.code,
        codes::INSTANCE_UNVERIFIED,
        "指纹源未接线不得当作通过：{err}"
    );
    // 当代来源不可用（None）：同样拒绝。
    let registry_none = InstanceRegistry::open_with_env(
        _root.path().join("instances.json"),
        Some(Arc::new(clone_env(&env))),
    )
    .unwrap()
    .with_identity_probe(Arc::new(FixedProbe(None)));
    registry_none.select(&a.instance_id).unwrap();
    let err = registry_none.resolve_selected().unwrap_err();
    assert_eq!(err.code, codes::INSTANCE_UNVERIFIED);
    // 指纹变化（同 profile 换钥）：身份不连续，要求重新选择/授权。
    let registry_rotated = InstanceRegistry::open_with_env(
        _root.path().join("instances.json"),
        Some(Arc::new(clone_env(&env))),
    )
    .unwrap()
    .with_identity_probe(Arc::new(FixedProbe(Some("rotated".into()))));
    registry_rotated.select(&a.instance_id).unwrap();
    let err = registry_rotated.resolve_selected().unwrap_err();
    assert_eq!(
        err.code,
        codes::AUTH_SIGNATURE,
        "同 profile 换钥必须判身份不连续：{err}"
    );
    // 指纹连续：放行。
    let registry = registry_with_probe(&env, _root.path());
    registry.select(&a.instance_id).unwrap();
    assert_eq!(registry.resolve_selected().unwrap().host_id, a.instance_id);
    // resolve_session 同门：指纹连续才给 Target。
    let home_a = _root.path().join("home-a");
    seed_session(&home_a, "proj-a", "session-aaa1aaaa");
    // 存在未归属 Web 时，会话归属保守拒绝（无法排除它持有同一会话）；
    // 这是 R01 分层的刻意降级，不是放行。
    let err = registry
        .resolve_session(&a.instance_id, "session-aaa1aaaa")
        .unwrap_err();
    assert_eq!(
        err.code,
        codes::SESSION_OWNERSHIP_UNPROVEN,
        "未归属 Web 在场时不得宣称会话唯一归属：{err}"
    );
}

// ============================================================
// 审查 I5：发现与唯一性决策分离
// ============================================================

/// 双 Desktop 同时在场：发现层**逐一返回**（不再提前歧义截断）；歧义决策在
/// discover 兼容委托与 resolve_selected 的无选择分支；已选择其一可正常解析。
#[test]
fn discovery_returns_all_desktop_candidates_and_selection_resolves() {
    let root = tempfile::tempdir().unwrap();
    let home_a = root.path().join("home-a");
    let home_c = root.path().join("home-c");
    std::fs::create_dir_all(&home_a).unwrap();
    std::fs::create_dir_all(&home_c).unwrap();
    let profile_a = seed_profile(&home_a, "desktop");
    let profile_c = seed_profile(&home_c, "desktop");
    let (env, _procs) = env_with(
        &home_c, // env.home 任意（Desktop profile 走 argv）
        vec![
            desktop_host_process(100, &home_a, 1_000),
            desktop_host_process(300, &home_c, 3_000),
        ],
        vec![
            listener(100, "127.0.0.1:5001"),
            listener(300, "127.0.0.1:5003"),
        ],
    );
    // 发现层：两个已核验候选都返回。
    let candidates = discover_all(&env).unwrap();
    assert_eq!(candidates.len(), 2, "双 Desktop 必须逐一返回");
    assert!(candidates.iter().all(|c| c.verified));
    // 兼容委托（无选择语义）：多宿主 → 歧义。
    assert_eq!(discover(&env).unwrap_err().code, codes::TARGET_AMBIGUOUS);
    // 已选择其一：正常解析，另一个在场不影响。
    let registry = registry_with_probe(&env, root.path());
    let a = registry
        .register(HostKind::Desktop, &profile_a, "key-a")
        .unwrap();
    let _c = registry
        .register(HostKind::Desktop, &profile_c, "key-c")
        .unwrap();
    registry.select(&a.instance_id).unwrap();
    let host = registry.resolve_selected().unwrap();
    assert_eq!(host.host_id, a.instance_id);
    assert_eq!(host.authority, "127.0.0.1:5001");
    // 选择另一台：解析另一台（绝不静默选边）。
    registry.select(&_c.instance_id).unwrap();
    let host = registry.resolve_selected().unwrap();
    assert_eq!(host.host_id, _c.instance_id);
    assert_eq!(host.authority, "127.0.0.1:5003");
}

// ============================================================
// 稳定实例 ID 派生面
// ============================================================

/// 稳定实例 ID 派生面：ID 不含 PID/端口/启动时刻（输入只有 kind + 规范化
/// profile），跨进程代稳定。
#[test]
fn instance_id_is_stable_across_process_generations() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let profile = seed_profile(&home, "desktop");
    let id = instance_id_for(HostKind::Desktop, &profile);
    // 换 PID/端口/启动时刻重算：ID 只看 kind + profile。
    let (env, _procs) = env_with(
        &home,
        vec![desktop_host_process(777, &home, 7_777)],
        vec![listener(777, "127.0.0.1:5077")],
    );
    let registry = registry(&env, root.path());
    let registered = registry
        .register(HostKind::Desktop, &profile, "key")
        .unwrap();
    assert_eq!(registered.instance_id, id, "登记与派生必须同一来源");
    // 尾部斜杠拼写差不得产生第二个身份。
    let trailing = PathBuf::from(format!("{}/", profile.to_string_lossy()));
    assert_eq!(
        instance_id_for(HostKind::Desktop, &trailing),
        id,
        "路径拼写差（尾分隔符）不得分裂身份"
    );
}

// ============================================================
// 审查 I4①：卡片归属钩子只回已核验实例
// ============================================================

/// 归属器对「同 home 唯一已核验候选」给 ID；Web 未核验 / 多义 / 发现失败 → None。
/// 生产实现的发现源不可注入（production_env），此处按其实现逻辑用注入环境
/// 复刻同一契约（candidates→verified→唯一 home 匹配）。
#[test]
fn instance_attributor_only_reports_verified_unique_home() {
    fn attributor_for(env: &DshEnvironment) -> impl Fn(&Path) -> Option<String> + '_ {
        move |home: &Path| {
            let discovery =
                multi_agents_manager_lib::services::dsh_control::identity::discover_detailed(env)
                    .ok()?;
            let target_home =
                multi_agents_manager_lib::services::dsh_control::instances::normalize_profile(home);
            let mut hit: Option<String> = None;
            for c in discovery.candidates.iter().filter(|c| c.verified) {
                // 候选 home = 规范化 profile 两级上溯（与生产归属器同一口径）。
                let text = c.profile_dir.to_string_lossy();
                let trimmed = text.trim_end_matches(['/', '\\']);
                let candidate_home = trimmed
                    .rsplit_once(['/', '\\'])
                    .map(|(profiles, _)| profiles)
                    .unwrap_or(trimmed)
                    .rsplit_once(['/', '\\'])
                    .map(|(home, _)| home)
                    .unwrap_or(trimmed);
                if Path::new(candidate_home) == target_home {
                    if hit.is_some() {
                        return None; // 同 home 多实例 → 多义
                    }
                    hit = Some(c.instance_id.clone());
                }
            }
            hit
        }
    }
    let root = tempfile::tempdir().unwrap();
    let home_a = root.path().join("home-a");
    let home_b = root.path().join("home-b");
    std::fs::create_dir_all(&home_a).unwrap();
    std::fs::create_dir_all(&home_b).unwrap();
    seed_profile(&home_a, "desktop");
    seed_profile(&home_b, "web");
    let (env, _procs) = env_with(
        &home_b,
        vec![
            desktop_host_process(100, &home_a, 1_000),
            web_host_process(200, 2_000),
        ],
        vec![
            listener(100, "127.0.0.1:5001"),
            listener(200, "127.0.0.1:5002"),
        ],
    );
    let attr = attributor_for(&env);
    // 唯一已核验 Desktop（home-a）→ ID。
    assert!(attr(&home_a).is_some(), "已核验唯一 Desktop 必须给 ID");
    // Web（未核验）home → None（归属钩子不消费未核验候选）。
    assert!(attr(&home_b).is_none(), "未核验 Web 不得给 ID");
    // 无宿主 home → None。
    assert!(attr(&root.path().join("home-none")).is_none());
}

// ============================================================
// 审查 M1（Minor）：并发选择与持久化验收覆盖
// ============================================================

/// 两个独立打开的登记并发 select 不同实例：各自成功、最终选择收敛到其一、
/// 已登记实例零丢失（锁内 load→改→save 的原子性验收面）。
#[test]
fn concurrent_selections_keep_every_registered_instance() {
    let dir = tempfile::tempdir().unwrap();
    let a_path = dir.path().join("a");
    let b_path = dir.path().join("b");
    std::fs::create_dir(&a_path).unwrap();
    std::fs::create_dir(&b_path).unwrap();
    let path = dir.path().join("registry.json");
    let setup = InstanceRegistry::open(&path).unwrap();
    let a = setup.register(HostKind::Desktop, &a_path, "key-a").unwrap();
    let b = setup.register(HostKind::Web, &b_path, "key-b").unwrap();
    drop(setup);
    let r1 = InstanceRegistry::open(&path).unwrap();
    let r2 = InstanceRegistry::open(&path).unwrap();
    let (pa, pb) = (a_path.clone(), b_path.clone());
    let (ia, ib) = (a.instance_id.clone(), b.instance_id.clone());
    let t1 = std::thread::spawn(move || {
        let r = InstanceRegistry::open(pa.parent().unwrap().join("registry.json")).unwrap();
        r.select(&ia)
            .map(|_| (r.selected().unwrap().unwrap().instance_id,))
    });
    let t2 = std::thread::spawn(move || {
        let r = InstanceRegistry::open(pb.parent().unwrap().join("registry.json")).unwrap();
        r.select(&ib)
            .map(|_| (r.selected().unwrap().unwrap().instance_id,))
    });
    // 并发选择各自成功（锁内 load→改→save，无交错丢写）；后写覆盖先写属正常
    // 收敛——最终态断言在下方。
    t1.join().unwrap().expect("select A 成功");
    t2.join().unwrap().expect("select B 成功");
    // 最终持久态：选择收敛到其一，登记面零丢失。
    let (env, _procs) = env_with(&b_path, vec![], vec![]);
    let _ = env;
    let final_registry = InstanceRegistry::open(&path).unwrap();
    let selected = final_registry.selected().unwrap().unwrap();
    assert!(
        selected.instance_id == a.instance_id || selected.instance_id == b.instance_id,
        "最终选择必须是并发写入者之一：{selected:?}"
    );
    let reopened = InstanceRegistry::open(&path).unwrap();
    assert!(
        reopened.select(&a.instance_id).is_ok(),
        "A 不得因并发选择丢失"
    );
    assert!(
        reopened.select(&b.instance_id).is_ok(),
        "B 不得因并发选择丢失"
    );
    let _ = (r1, r2);
}

// ============================================================
// 审查 N1：同实例多活体（同 profile 双进程）漏拒
// ============================================================

/// 两个不同 PID 携同一 profile → 派生同一实例 ID；即便指纹连续，writer 在两个
/// 竞争活体间不可证——resolve_selected 报实例歧义，resolve_session 报归属未证
/// （不取首项）。无选择时同属多宿主歧义（发现层逐一返回、决策层拒绝）。
#[test]
fn same_profile_dual_live_hosts_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let profile = seed_profile(&home, "desktop");
    let session_home_dir = home.clone();
    seed_session(&session_home_dir, "proj", "session-aaa1aaaa");
    // 同 profile 两代进程同时在跑（不同 PID/端口/启动时刻）。
    let (env, _procs) = env_with(
        &home,
        vec![
            desktop_host_process(100, &home, 1_000),
            desktop_host_process(200, &home, 2_000),
        ],
        vec![
            listener(100, "127.0.0.1:5001"),
            listener(200, "127.0.0.1:5002"),
        ],
    );
    let registry = registry_with_probe(&env, root.path());
    let instance = registry
        .register(HostKind::Desktop, &profile, "key")
        .unwrap();
    // 发现层：两个候选都返回（同实例 ID）。
    let candidates = discover_all(&env).unwrap();
    assert_eq!(candidates.len(), 2);
    assert!(candidates
        .iter()
        .all(|c| c.instance_id == instance.instance_id));
    // 有选择 + 指纹连续：仍必须拒绝（多活体 = writer 不可证）。
    registry.select(&instance.instance_id).unwrap();
    let err = registry.resolve_selected().unwrap_err();
    assert_eq!(
        err.code,
        codes::TARGET_AMBIGUOUS,
        "同实例双活体必须报实例歧义：{err}"
    );
    assert!(err.detail.contains("pids"), "错误必须列出竞争 PID：{err}");
    // resolve_session：同 ID 多活体 → 归属未证（不取首项）。
    let err = registry
        .resolve_session(&instance.instance_id, "session-aaa1aaaa")
        .unwrap_err();
    assert_eq!(
        err.code,
        codes::SESSION_OWNERSHIP_UNPROVEN,
        "同实例双活体的会话 writer 归属不可证：{err}"
    );
    // 无选择路径同拒（多宿主歧义口径）。
    let fresh = InstanceRegistry::open_with_env(
        root.path().join("unregistered.json"),
        Some(Arc::new(clone_env(&env))),
    )
    .unwrap();
    let err = fresh.resolve_selected().unwrap_err();
    assert_eq!(err.code, codes::TARGET_AMBIGUOUS);
}

// ============================================================
// P1-1a：生产指纹锚点的三态语义（直接打真实 InstanceRegistry）
//
// 为什么必须打真实登记：锚点是「授权域 → 登记面」的唯一写入点。它的三态
// （空值写入 / 同值幂等 / 异值拒绝）如果只用替身断言，就等于用复制品证明
// 复制品——真实登记的文件锁、canonicalize、原子写都不在断言里。
// ============================================================

use multi_agents_manager_lib::services::dsh_control::auth::RegistryFingerprintAnchor as _;
use multi_agents_manager_lib::services::dsh_control::production::RegistryFingerprintAnchor;

/// 候选登记（空指纹）→ 锚定写入 → 同值幂等 → 异值拒绝。
#[test]
fn registry_fingerprint_anchor_writes_once_then_refuses_a_different_identity() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home-a");
    std::fs::create_dir_all(&home).unwrap();
    let profile = seed_profile(&home, "desktop");
    let (env, _procs) = env_with(
        &home,
        vec![desktop_host_process(100, &home, 1_000)],
        vec![listener(100, "127.0.0.1:5301")],
    );
    let candidate = registry(&env, root.path())
        .register_candidate(HostKind::Desktop, &profile)
        .expect("候选登记（空指纹）");
    assert_eq!(
        candidate.identity_fingerprint, "",
        "前置条件：候选登记必须是未锚定的空指纹"
    );

    let anchor = RegistryFingerprintAnchor::with_registry(Arc::new(registry(&env, root.path())));

    // ① 空登记值 → 写入。
    anchor
        .anchor_fingerprint(&candidate.instance_id, "digest-first")
        .expect("空登记值必须可锚定");
    let anchored = registry(&env, root.path())
        .registered(&candidate.instance_id)
        .unwrap();
    assert_eq!(
        anchored.identity_fingerprint, "digest-first",
        "锚定必须真的落进登记文件（下次 resolve 才能比对）"
    );

    // ② 同值 → 幂等 no-op（不报错、不改值）。
    anchor
        .anchor_fingerprint(&candidate.instance_id, "digest-first")
        .expect("同值重复锚定必须幂等");
    assert_eq!(
        registry(&env, root.path())
            .registered(&candidate.instance_id)
            .unwrap()
            .identity_fingerprint,
        "digest-first"
    );

    // ③ 异值 → 拒绝（绝不静默换钉）。
    let err = anchor
        .anchor_fingerprint(&candidate.instance_id, "digest-second")
        .expect_err("已锚定不同值必须拒绝，不得静默覆盖");
    assert_eq!(err.code, codes::AUTH_SIGNATURE, "{err}");
    assert_eq!(
        registry(&env, root.path())
            .registered(&candidate.instance_id)
            .unwrap()
            .identity_fingerprint,
        "digest-first",
        "拒绝时登记值必须保持原样（不得半写）"
    );

    // ④ 空指纹 → 拒绝（空串不是身份）。
    let err = anchor
        .anchor_fingerprint(&candidate.instance_id, "")
        .expect_err("空指纹不得锚定");
    assert_eq!(err.code, codes::AUTH_SIGNATURE, "{err}");
}

// ============================================================
// P1-1b：Web 归属必须进入**生产发现/选择/解析链**
//
// 为什么必须在这里（而不是在 identity 单测里）：Web home 无法从 argv 读出，
// 只能靠「该进程以什么 HOME/DSH_HOME 启动」证明。此前 `discover_detailed` 把
// Web 恒留在 `raw_web_hosts`，于是：
//   * `instance_list_core` 看不到 Web 候选 → 用户无从选中；
//   * `resolve_selected` / `resolve_session` 永远 `HOST_NOT_RUNNING`。
// 结果是「设置里能批准、实际什么都读不了」。下面的用例把这条链一路打到底。
// ============================================================

/// 注入了启动环境观测的假源（**不读真实系统 PID**）。
struct FakeEnvironments {
    homes: std::collections::HashMap<u32, (DshProcess, Option<String>, Option<String>)>,
}

impl multi_agents_manager_lib::services::dsh_control::model::ProcessEnvironmentReader
    for FakeEnvironments
{
    fn observed_environment(
        &self,
        pid: u32,
    ) -> Option<multi_agents_manager_lib::services::dsh_control::model::ProcessEnvironmentEvidence>
    {
        use multi_agents_manager_lib::services::dsh_control::model::ProcessEnvironmentEvidence;
        use multi_agents_manager_lib::services::dsh_control::process_environment::{
            AllowlistedEnvironment, EnvironmentField, NativeGeneration, ObservedProcessIdentity,
            ReadCompleteness,
        };
        let (process, dsh_home, home) = self.homes.get(&pid)?;
        let identity = ObservedProcessIdentity {
            pid: process.pid,
            started_at_ms: process.started_at_ms,
            executable: process.exe.clone(),
            native_generation: Some(NativeGeneration {
                unique_id: u64::from(pid),
                pid_version: 1,
            }),
        };
        // 忠实复刻生产的三种形态：空串是 `Empty`（变量存在但为空），
        // 不是 `Value("")`——否则「空 DSH_HOME 必须拒绝」这类判据在夹具里永远
        // 走不到，用例会假绿。
        let field = |v: &Option<String>| match v {
            Some(s) if s.is_empty() => EnvironmentField::Empty,
            Some(s) => EnvironmentField::Value(s.clone()),
            None => EnvironmentField::Unset,
        };
        Some(ProcessEnvironmentEvidence {
            identity_before: identity.clone(),
            identity_after: identity,
            completeness: ReadCompleteness::Complete,
            environment: AllowlistedEnvironment {
                dsh_home: field(dsh_home),
                home: field(home),
            },
        })
    }
}

/// 在原有启动环境证据上控制内核代际；不读取真实系统进程。
struct WebGenerations {
    evidence: FakeEnvironments,
    versions: Mutex<std::collections::HashMap<u32, u32>>,
}

impl multi_agents_manager_lib::services::dsh_control::model::ProcessEnvironmentReader
    for WebGenerations
{
    fn observed_environment(
        &self,
        pid: u32,
    ) -> Option<multi_agents_manager_lib::services::dsh_control::model::ProcessEnvironmentEvidence>
    {
        let mut evidence = self.evidence.observed_environment(pid)?;
        let version = *self.versions.lock().unwrap().get(&pid)?;
        evidence
            .identity_before
            .native_generation
            .as_mut()?
            .pid_version = version;
        evidence
            .identity_after
            .native_generation
            .as_mut()?
            .pid_version = version;
        Some(evidence)
    }
}

/// 构造一个可归属的 Web 实例夹具（真实临时 profile 目录 + 假进程/监听 + 假启动环境）。
fn web_instance_fixture(root: &Path) -> (DshEnvironment, Arc<SharedProcs>, String, PathBuf) {
    let home = root.join("web-home");
    let profile = seed_profile(&home, "web");
    // macOS 的 /tmp 与 /var 是符号链接：观测器给出的是**规范路径**，夹具必须
    // 用同一形态，否则「观测到的 home」与夹具期望值只差 /private 前缀就假红。
    let home = std::fs::canonicalize(&home).expect("夹具 home 可规范化");
    let pid = 4242u32;
    let procs = Arc::new(SharedProcs {
        processes: Mutex::new(Vec::new()),
        listeners: Mutex::new(Vec::new()),
    });
    *procs.processes.lock().unwrap() = vec![web_host_process(pid, 5_000)];
    *procs.listeners.lock().unwrap() = vec![DshListener {
        owner_pid: pid,
        authority: "127.0.0.1:5399".into(),
    }];
    let mut homes = std::collections::HashMap::new();
    homes.insert(
        pid,
        (
            web_host_process(pid, 5_000),
            Some(home.to_string_lossy().to_string()),
            None,
        ),
    );
    let env = DshEnvironment {
        home: home.clone(),
        tuvis_home: root.join("tuvis-home"),
        dsh_state_dir: None,
        processes: procs.clone(),
        files: Arc::new(RealFiles),
        runner: Arc::new(NullRunner),
        clock: Arc::new(FixedClock(1_760_000_000_000)),
        environments: Arc::new(FakeEnvironments { homes }),
    };
    let instance_id = instance_id_for(HostKind::Web, &profile);
    // 候选里的 profile_dir 是**规范化**形态（与登记侧同一 normalize_profile）。
    let profile = std::fs::canonicalize(&profile).expect("夹具 profile 可规范化");
    (env, procs, instance_id, profile)
}

#[test]
fn instance_list_associates_web_native_generation_with_its_verified_instance() {
    use multi_agents_manager_lib::commands::dsh::instance_list_core;
    let root = tempfile::tempdir().unwrap();
    let (mut env, procs, a_id, a_profile) = web_instance_fixture(root.path());
    let b_home = root.path().join("web-home-b");
    let b_profile = seed_profile(&b_home, "web");
    let b_home = std::fs::canonicalize(b_home).unwrap();
    let b_id = instance_id_for(HostKind::Web, &b_profile);
    let a_process = web_host_process(4242, 5_000);
    let b_process = web_host_process(4243, 6_000);
    procs.processes.lock().unwrap().push(b_process.clone());
    procs
        .listeners
        .lock()
        .unwrap()
        .push(listener(4243, "127.0.0.1:5400"));
    let evidence = FakeEnvironments {
        homes: [
            (
                4242,
                (
                    a_process,
                    Some(env.home.to_string_lossy().into_owned()),
                    None,
                ),
            ),
            (
                4243,
                (b_process, Some(b_home.to_string_lossy().into_owned()), None),
            ),
        ]
        .into_iter()
        .collect(),
    };
    let generations = Arc::new(WebGenerations {
        evidence,
        versions: Mutex::new([(4242, 1), (4243, 1)].into_iter().collect()),
    });
    env.environments = generations.clone();
    let registry = registry(&env, root.path());
    registry
        .register_candidate(HostKind::Web, &a_profile)
        .unwrap();
    registry
        .register_candidate(HostKind::Web, &b_profile)
        .unwrap();
    let snapshot = || serde_json::to_value(instance_list_core(&env, &registry).unwrap()).unwrap();
    let epoch = |rows: &serde_json::Value, id: &str| -> String {
        let row = rows
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["instanceId"] == id)
            .unwrap();
        assert_eq!(row["kind"], "web");
        assert_eq!(row["live"], true);
        row["hostEpoch"]
            .as_str()
            .expect("已核验 Web 必须返回代际字符串")
            .to_owned()
    };
    let before = snapshot();
    let a_before = epoch(&before, &a_id);
    let b_before = epoch(&before, &b_id);
    let candidates = discover_all(&env).unwrap();
    for id in [&a_id, &b_id] {
        let candidate = candidates
            .iter()
            .find(|candidate| candidate.instance_id == *id)
            .unwrap();
        assert!(candidate.verified);
        assert_eq!(epoch(&before, id), candidate.host.epoch.to_string());
    }
    assert_eq!(epoch(&snapshot(), &a_id), a_before, "同一原生代际应稳定");
    // PID、启动时间、监听和归属 home 都不变，仅 A 的内核代际换代。
    generations.versions.lock().unwrap().insert(4242, 2);
    let after = snapshot();
    assert_ne!(epoch(&after, &a_id), a_before);
    assert_eq!(epoch(&after, &b_id), b_before, "A 换代不得影响 B");
    assert_eq!(after.as_array().unwrap().len(), 2);
}

#[test]
fn instance_list_hides_web_epoch_when_home_attribution_is_unverified() {
    use multi_agents_manager_lib::commands::dsh::instance_list_core;
    let root = tempfile::tempdir().unwrap();
    let (mut env, _procs, id, profile) = web_instance_fixture(root.path());
    let registry = registry(&env, root.path());
    let before = instance_list_core(&env, &registry).unwrap();
    let verified = before.iter().find(|row| row.instance_id == id).unwrap();
    assert!(verified.live && verified.host_epoch.is_some());
    env.environments = Arc::new(
        multi_agents_manager_lib::services::dsh_control::model::TestUnobservableEnvironments,
    );
    assert!(
        instance_list_core(&env, &registry)
            .unwrap()
            .iter()
            .all(|row| row.instance_id != id),
        "未登记且未核验的宿主不应出现在列表"
    );
    registry
        .register_candidate(HostKind::Web, &profile)
        .unwrap();
    registry.select(&id).unwrap();
    let rows = instance_list_core(&env, &registry).unwrap();
    let row = rows.iter().find(|row| row.instance_id == id).unwrap();
    assert!(!row.live);
    assert!(row.selected);
    let wire = serde_json::to_value(row).unwrap();
    assert!(
        wire.get("hostEpoch").unwrap().is_null(),
        "未核验时不能沿用先前代际"
    );
}

/// Web 归属进入发现：`discover_all` 必须给出该实例的**已核验**候选。
#[test]
fn web_instance_is_discovered_as_a_verified_candidate_from_its_startup_environment() {
    let root = tempfile::tempdir().unwrap();
    let (env, _procs, instance_id, profile) = web_instance_fixture(root.path());

    let candidates = discover_all(&env).expect("发现不得失败");
    let hit = candidates
        .iter()
        .find(|c| c.instance_id == instance_id)
        .unwrap_or_else(|| {
            panic!(
                "启动环境已证明的 Web 实例必须出现在候选里，实际：{:?}",
                candidates
                    .iter()
                    .map(|c| (&c.instance_id, c.kind, c.verified))
                    .collect::<Vec<_>>()
            )
        });
    assert_eq!(hit.kind, HostKind::Web);
    assert!(
        hit.verified,
        "OS 启动环境 + 派生 ID 唯一命中的 Web 候选必须是已核验归属"
    );
    assert_eq!(
        hit.profile_dir, profile,
        "候选 profile 必须来自观测到的 home"
    );
    assert_eq!(hit.host.authority, "127.0.0.1:5399");
}

/// 观测不到启动环境时必须**保持未归属**（不猜）——这是 fail-closed 的那一半。
#[test]
fn web_instance_stays_unattributed_when_the_environment_is_not_observable() {
    let root = tempfile::tempdir().unwrap();
    let (mut env, _procs, instance_id, _profile) = web_instance_fixture(root.path());
    // 换成「恒不可观测」的观测器：归属链必须拒绝，而不是退回 env.home 猜测。
    env.environments = Arc::new(
        multi_agents_manager_lib::services::dsh_control::model::TestUnobservableEnvironments,
    );

    let discovery = discover_detailed(&env).expect("发现本身不得报错");
    assert!(
        discovery
            .candidates
            .iter()
            .all(|c| c.instance_id != instance_id),
        "观测不到启动环境时不得产出该实例候选"
    );
    assert_eq!(
        discovery.raw_web_hosts.len(),
        1,
        "真实进程事实必须保留在 raw 里（不猜，但也不丢）"
    );
    assert_eq!(discovery.unattributable_web_hosts, 1);
}

/// **完整链**：发现 → 登记 → 选中 → `resolve_selected` 必须拿到当代活体。
///
/// 这是 P1-1b 的验收点：Web 实例不能再是「能批准、不能解析」。
#[test]
fn web_instance_can_be_registered_selected_and_resolved_end_to_end() {
    let root = tempfile::tempdir().unwrap();
    let (env, _procs, instance_id, profile) = web_instance_fixture(root.path());

    let registry = registry_with_probe(&env, root.path());
    registry
        .register(HostKind::Web, &profile, "web-plugin-digest")
        .expect("Web 实例可以按已核验归属登记");

    // 选中（生产 select 入口语义）。
    registry.select(&instance_id).expect("选中该 Web 实例");

    // resolve_selected：当代活体必须解析出来，且身份与登记一致。
    let host = registry
        .resolve_selected()
        .expect("已核验的 Web 实例必须可解析（否则界面能批准但读写全拒）");
    assert_eq!(host.host_id, instance_id);
    assert_eq!(host.authority, "127.0.0.1:5399");
    assert_eq!(host.pid, 4242);
}

/// P1（修正版）：home 解析必须区分 `Value` / `Unset` / `Empty`。
///
/// 我第一版把「`Unset`」也当成不可观测，结果连单实例默认用户（不设 `DSH_HOME`、
/// 只有 `HOME`）都无法归属——那是把正常形态误判成异常。正确语义（官方优先级
/// `DSH_HOME` > `HOME/.dsh`）：
///
/// * `DSH_HOME = Value` → 用它；
/// * `DSH_HOME = Unset` 且有 `HOME = Value` → 用 `HOME/.dsh`（官方默认）；
/// * `DSH_HOME = Empty` → **拒绝**（空路径不是 home，拼 `.dsh` 只会得到相对路径）；
/// * 两者都没有可用值 → 拒绝（拼不出任何 home）。
#[test]
fn web_attribution_distinguishes_unset_from_empty_and_unusable_home() {
    // (label, dsh_home, user_home, 期望是否产出候选)
    let cases: [(&str, Option<String>, Option<String>, bool); 4] = [
        (
            "dsh_home unset + HOME value falls back to the official default",
            None,
            Some("/tmp/some-user-home".to_string()),
            true,
        ),
        (
            "empty DSH_HOME is refused",
            Some(String::new()),
            Some("/tmp/some-user-home".to_string()),
            false,
        ),
        ("both unset is refused", None, None, false),
        (
            "empty HOME cannot produce a default",
            None,
            Some(String::new()),
            false,
        ),
    ];
    for (label, dsh_home, user_home, expect_candidate) in cases {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("web-home");
        // 默认分支要落在这个 home 上：把 HOME 指向 root，让 `HOME/.dsh` 正好可控。
        let effective_user_home = match &user_home {
            Some(v) if !v.is_empty() => Some(root.path().to_string_lossy().to_string()),
            other => other.clone(),
        };
        let _ = &home;
        let pid = 4_251u32;
        let procs = Arc::new(SharedProcs {
            processes: Mutex::new(vec![web_host_process(pid, 5_100)]),
            listeners: Mutex::new(vec![DshListener {
                owner_pid: pid,
                authority: "127.0.0.1:5398".into(),
            }]),
        });
        let mut homes = std::collections::HashMap::new();
        homes.insert(
            pid,
            (
                web_host_process(pid, 5_100),
                dsh_home.clone(),
                effective_user_home.clone(),
            ),
        );
        let env = DshEnvironment {
            home: home.clone(),
            tuvis_home: root.path().join("tuvis-home"),
            dsh_state_dir: None,
            processes: procs,
            files: Arc::new(RealFiles),
            runner: Arc::new(NullRunner),
            clock: Arc::new(FixedClock(1_760_000_000_000)),
            environments: Arc::new(FakeEnvironments { homes }),
        };

        let discovery = discover_detailed(&env).expect("发现本身不得报错");
        let web_candidates: Vec<_> = discovery
            .candidates
            .iter()
            .filter(|c| c.kind == HostKind::Web)
            .collect();
        if expect_candidate {
            assert_eq!(
                web_candidates.len(),
                1,
                "[{label}] 应产出 Web 候选（Unset 走官方默认 HOME/.dsh）"
            );
        } else {
            assert!(
                web_candidates.is_empty(),
                "[{label}] 不得产出 Web 候选（拼不出可信 home）"
            );
            assert_eq!(
                discovery.raw_web_hosts.len(),
                1,
                "[{label}] 真实进程事实必须保留"
            );
        }
    }
}

#[cfg(unix)]
fn linked_desktop_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, DshEnvironment) {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("proven-home");
    let foreign = root.path().join("foreign-home");
    let physical = seed_profile(&foreign, "desktop");
    std::fs::create_dir_all(home.join("profiles")).unwrap();
    symlink(&physical, home.join("profiles/desktop")).unwrap();
    let (env, _) = env_with(
        &home,
        vec![desktop_host_process(81001, &home, 1234)],
        vec![listener(81001, "127.0.0.1:58101")],
    );
    (root, home, foreign, env)
}

#[cfg(unix)]
#[test]
fn linked_profile_session_resolution_uses_proven_home() {
    let (root, home, foreign, env) = linked_desktop_fixture();
    let profile = home.join("profiles/desktop");
    seed_session(&home, "project", "session-aaaaaaaa");
    seed_session(&foreign, "project", "session-bbbbbbbb");
    let registry = registry_with_probe(&env, root.path());
    let candidate = registry
        .register_candidate_with_home(HostKind::Desktop, &profile, Some(&home))
        .unwrap();
    registry
        .register(HostKind::Desktop, &profile, "key-a")
        .unwrap();
    let target = registry
        .resolve_session(&candidate.instance_id, "session-aaaaaaaa")
        .expect("已证明 home 的会话应可归属");
    assert_eq!(target.host_id, candidate.instance_id);
    assert_eq!(
        registry
            .resolve_session(&candidate.instance_id, "session-bbbbbbbb")
            .unwrap_err()
            .code,
        codes::SESSION_OWNERSHIP_UNPROVEN
    );
}

#[cfg(unix)]
#[test]
fn legacy_linked_profile_does_not_claim_foreign_home_session() {
    let (root, home, foreign, env) = linked_desktop_fixture();
    seed_session(&foreign, "project", "session-bbbbbbbb");
    let registry = registry_with_probe(&env, root.path());
    let old = registry
        .register(HostKind::Desktop, &home.join("profiles/desktop"), "key-a")
        .unwrap();
    assert!(old.dsh_home.is_none());
    assert_eq!(
        registry
            .resolve_session(&old.instance_id, "session-bbbbbbbb")
            .unwrap_err()
            .code,
        codes::SESSION_OWNERSHIP_UNPROVEN
    );
}

#[cfg(unix)]
#[test]
fn legacy_registry_can_add_proven_home_without_changing_identity_or_selection() {
    let (root, home, _, _) = linked_desktop_fixture();
    let path = root.path().join("registry.json");
    let registry = InstanceRegistry::open(&path).unwrap();
    let profile = home.join("profiles/desktop");
    let old = registry
        .register(HostKind::Desktop, &profile, "key-a")
        .unwrap();
    registry.select(&old.instance_id).unwrap();
    let old_json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert!(old_json["instances"][0].get("dshHome").is_none());
    drop(registry);
    let registry = InstanceRegistry::open(&path).unwrap();
    assert_eq!(registry.selected().unwrap().unwrap(), old);
    let updated = registry
        .register_candidate_with_home(HostKind::Desktop, &profile, Some(&home))
        .unwrap();
    assert_eq!(updated.instance_id, old.instance_id);
    assert_eq!(updated.identity_fingerprint, old.identity_fingerprint);
    assert_eq!(
        updated.dsh_home,
        Some(std::fs::canonicalize(&home).unwrap())
    );
    drop(registry);
    let persisted: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        persisted["instances"][0]["dshHome"].as_str(),
        Some(std::fs::canonicalize(&home).unwrap().to_str().unwrap())
    );
    let reopened = InstanceRegistry::open(&path).unwrap();
    assert_eq!(reopened.selected().unwrap(), Some(updated));
}

#[cfg(unix)]
#[test]
fn web_restart_instructions_use_proven_home_for_linked_profile() {
    use multi_agents_manager_lib::commands::dsh::{dsh_restart_host_core, DshInstanceRequest};
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("proven-home");
    let physical = seed_profile(&root.path().join("foreign-home"), "web");
    std::fs::create_dir_all(home.join("profiles")).unwrap();
    symlink(&physical, home.join("profiles/web")).unwrap();
    let (env, _) = env_with(&home, vec![], vec![]);
    let registry = InstanceRegistry::open(root.path().join("registry.json")).unwrap();
    let instance = registry
        .register_candidate_with_home(HostKind::Web, &home.join("profiles/web"), Some(&home))
        .unwrap();
    registry.select(&instance.instance_id).unwrap();
    let error = dsh_restart_host_core(
        &env,
        &registry,
        DshInstanceRequest {
            instance_id: instance.instance_id,
        },
    )
    .expect_err("Web 不由应用启动");
    assert!(error.contains(codes::UNSUPPORTED));
    assert!(
        error.contains(std::fs::canonicalize(home).unwrap().to_str().unwrap()),
        "应提示已证明 home：{error}"
    );
    assert!(
        !error.contains(root.path().join("foreign-home").to_str().unwrap()),
        "不得提示外链目标所属 home：{error}"
    );
}

#[cfg(unix)]
struct LegacyReadonlyLedgerFixture {
    _root: tempfile::TempDir,
    home_a: PathBuf,
    home_b: PathBuf,
    env: DshEnvironment,
    registry: InstanceRegistry,
    instance: SelectedInstance,
    ledger_path: PathBuf,
    registry_path: PathBuf,
}

#[cfg(unix)]
impl LegacyReadonlyLedgerFixture {
    fn new() -> Self {
        use multi_agents_manager_lib::services::dsh_control::auth::{
            instance_state_dir, key_digest,
        };
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let home_a = root.path().join("home-a");
        let home_b = root.path().join("home-b");
        let physical = seed_profile(&home_b, "desktop");
        std::fs::create_dir_all(home_a.join("profiles")).unwrap();
        symlink(&physical, home_a.join("profiles/desktop")).unwrap();
        for (home, id) in [(&home_a, "session-aaaaaaaa"), (&home_b, "session-bbbbbbbb")] {
            seed_session(home, "project", id);
            std::fs::write(
                home.join("sessions/project").join(id).join("session.jsonl"),
                format!("{{\"id\":\"{id}\"}}\n"),
            )
            .unwrap();
        }
        let (env, _) = env_with(&home_a, vec![], vec![]);
        let registry_path = root.path().join("registry.json");
        let registry =
            InstanceRegistry::open_with_env(&registry_path, Some(Arc::new(clone_env(&env))))
                .unwrap();
        let instance = registry
            .register(
                HostKind::Desktop,
                &home_a.join("profiles/desktop"),
                &key_digest(b"fixture-spki"),
            )
            .unwrap();
        registry.select(&instance.instance_id).unwrap();
        assert!(instance.dsh_home.is_none());
        let ledger_path = instance_state_dir(&env, &instance.instance_id)
            .unwrap()
            .join("bindings.json");
        std::fs::create_dir_all(ledger_path.parent().unwrap()).unwrap();
        Self {
            _root: root,
            home_a,
            home_b,
            env,
            registry,
            instance,
            ledger_path,
            registry_path,
        }
    }

    fn row(&self, id: &str, profile: &Path, state: &str, key: &[u8]) -> serde_json::Value {
        use multi_agents_manager_lib::services::dsh_control::auth::b64url_encode;
        serde_json::json!({
            "bindingId": id, "publicKeySpki": if state == "bound" { b64url_encode(key) } else { String::new() },
            "revokedKeySpki": if state == "revoked" { b64url_encode(key) } else { String::new() },
            "executable": "/fixture/dsh", "pid": 1, "startedAtMs": 100,
            "profileDir": profile.to_string_lossy(), "createdAtMs": 101, "state": state
        })
    }

    fn write_rows(&self, rows: Vec<serde_json::Value>) {
        std::fs::write(
            &self.ledger_path,
            serde_json::to_vec(&serde_json::json!({"bindings": rows})).unwrap(),
        )
        .unwrap();
    }
}

#[cfg(unix)]
#[test]
fn legacy_readonly_uses_unique_bound_profile_home_for_linked_profile() {
    let f = LegacyReadonlyLedgerFixture::new();
    f.write_rows(vec![f.row(
        "binding-a",
        &f.home_a.join("profiles/desktop"),
        "bound",
        b"fixture-spki",
    )]);
    let before = std::fs::read(&f.registry_path).unwrap();
    assert_eq!(
        f.registry
            .resolve_readonly_root(&f.instance.instance_id, "session-aaaaaaaa")
            .unwrap(),
        std::fs::canonicalize(&f.home_a).unwrap()
    );
    assert_eq!(
        f.registry
            .resolve_readonly_root(&f.instance.instance_id, "session-bbbbbbbb")
            .unwrap_err()
            .code,
        codes::SESSION_OWNERSHIP_UNPROVEN
    );
    assert_eq!(
        std::fs::read(&f.registry_path).unwrap(),
        before,
        "只读不得升级登记或授权"
    );
}

#[cfg(unix)]
#[test]
fn legacy_readonly_direct_profile_with_bound_ledger_remains_available_offline() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("direct-home");
    let profile = seed_profile(&home, "desktop");
    seed_session(&home, "project", "session-bbbbbbbb");
    std::fs::write(
        home.join("sessions/project/session-bbbbbbbb/session.jsonl"),
        b"{\"id\":\"session-bbbbbbbb\"}\n",
    )
    .unwrap();
    let (env, _) = env_with(&home, vec![], vec![]);
    let registry = registry(&env, root.path());
    let key = b"fixture-spki";
    let instance = registry
        .register(
            HostKind::Desktop,
            &profile,
            &multi_agents_manager_lib::services::dsh_control::auth::key_digest(key),
        )
        .unwrap();
    registry.select(&instance.instance_id).unwrap();
    let ledger = multi_agents_manager_lib::services::dsh_control::auth::instance_state_dir(
        &env,
        &instance.instance_id,
    )
    .unwrap()
    .join("bindings.json");
    std::fs::create_dir_all(ledger.parent().unwrap()).unwrap();
    let row = serde_json::json!({"bindingId":"direct", "publicKeySpki":multi_agents_manager_lib::services::dsh_control::auth::b64url_encode(key), "revokedKeySpki":"", "executable":"/fixture/dsh", "pid":1, "startedAtMs":100, "profileDir":profile.to_string_lossy(), "createdAtMs":101, "state":"bound"});
    std::fs::write(
        ledger,
        serde_json::to_vec(&serde_json::json!({"bindings":[row]})).unwrap(),
    )
    .unwrap();
    assert_eq!(
        registry
            .resolve_readonly_root(&instance.instance_id, "session-bbbbbbbb")
            .unwrap(),
        std::fs::canonicalize(home).unwrap()
    );
}

#[cfg(unix)]
#[test]
fn legacy_readonly_rejects_bound_rows_proving_different_homes() {
    let f = LegacyReadonlyLedgerFixture::new();
    f.write_rows(vec![
        f.row(
            "binding-a",
            &f.home_a.join("profiles/desktop"),
            "bound",
            b"fixture-spki",
        ),
        f.row(
            "binding-b",
            &f.home_b.join("profiles/desktop"),
            "bound",
            b"fixture-spki",
        ),
    ]);
    for id in ["session-aaaaaaaa", "session-bbbbbbbb"] {
        assert_eq!(
            f.registry
                .resolve_readonly_root(&f.instance.instance_id, id)
                .unwrap_err()
                .code,
            codes::SESSION_OWNERSHIP_UNPROVEN
        );
    }
}

#[cfg(unix)]
#[test]
fn legacy_readonly_rejects_discontinuous_bound_fingerprint() {
    let f = LegacyReadonlyLedgerFixture::new();
    f.write_rows(vec![f.row(
        "binding-a",
        &f.home_a.join("profiles/desktop"),
        "bound",
        b"other-spki",
    )]);
    assert_eq!(
        f.registry
            .resolve_readonly_root(&f.instance.instance_id, "session-aaaaaaaa")
            .unwrap_err()
            .code,
        codes::SESSION_OWNERSHIP_UNPROVEN
    );
}

#[cfg(unix)]
#[test]
fn legacy_readonly_rejects_revoked_binding_as_home_evidence() {
    let f = LegacyReadonlyLedgerFixture::new();
    f.write_rows(vec![f.row(
        "binding-a",
        &f.home_a.join("profiles/desktop"),
        "revoked",
        b"fixture-spki",
    )]);
    assert_eq!(
        f.registry
            .resolve_readonly_root(&f.instance.instance_id, "session-aaaaaaaa")
            .unwrap_err()
            .code,
        codes::SESSION_OWNERSHIP_UNPROVEN
    );
}

#[cfg(unix)]
#[test]
fn legacy_readonly_rejects_corrupted_binding_ledger() {
    let f = LegacyReadonlyLedgerFixture::new();
    std::fs::write(&f.ledger_path, b"{corrupt").unwrap();
    assert_eq!(
        f.registry
            .resolve_readonly_root(&f.instance.instance_id, "session-aaaaaaaa")
            .unwrap_err()
            .code,
        codes::SESSION_OWNERSHIP_UNPROVEN
    );
    assert!(
        multi_agents_manager_lib::services::dsh_control::auth::pinned_fingerprint_for(
            &f.env,
            &f.instance.instance_id
        )
        .is_none()
    );
}

// ============================================================
// Round-8 P1-3：退役登记的只读历史归属（独立实例证据；硬下界不放松）
// ============================================================

/// 退役登记夹具：登记锚定 `anchor_key`（授权落账形态）→ 卸载退役锚
/// （`retire_identity_fingerprint`——只读证据保留）；绑定账本由用例直写。
#[cfg(unix)]
struct RetiredReadonlyFixture {
    _root: tempfile::TempDir,
    home: PathBuf,
    /// 仅作持有面（state 目录与登记句柄共享同一事实源；用例本身经
    /// `registry` / `ledger_path` 消费，不直读 env）。
    _env: DshEnvironment,
    registry: InstanceRegistry,
    instance_id: String,
    ledger_path: PathBuf,
}

#[cfg(unix)]
impl RetiredReadonlyFixture {
    fn new_anchored(anchor_key: &[u8]) -> Self {
        use multi_agents_manager_lib::services::dsh_control::auth::{
            instance_state_dir, key_digest,
        };
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let profile = seed_profile(&home, "desktop");
        seed_session(&home, "project", "session-aaaaaaaa");
        std::fs::write(
            home.join("sessions/project/session-aaaaaaaa/session.jsonl"),
            b"{\"id\":\"session-aaaaaaaa\"}\n",
        )
        .unwrap();
        let (env, _) = env_with(&home, vec![], vec![]);
        let registry = registry(&env, root.path());
        let instance = registry
            .register(HostKind::Desktop, &profile, &key_digest(anchor_key))
            .unwrap();
        registry.select(&instance.instance_id).unwrap();
        let ledger_path = instance_state_dir(&env, &instance.instance_id)
            .unwrap()
            .join("bindings.json");
        std::fs::create_dir_all(ledger_path.parent().unwrap()).unwrap();
        Self {
            _root: root,
            home,
            _env: env,
            registry,
            instance_id: instance.instance_id,
            ledger_path,
        }
    }

    fn row(&self, id: &str, profile: &Path, state: &str, key: &[u8]) -> serde_json::Value {
        use multi_agents_manager_lib::services::dsh_control::auth::b64url_encode;
        serde_json::json!({
            "bindingId": id, "publicKeySpki": if state == "bound" { b64url_encode(key) } else { String::new() },
            "revokedKeySpki": if state == "revoked" { b64url_encode(key) } else { String::new() },
            "executable": "/fixture/dsh", "pid": 1, "startedAtMs": 100,
            "profileDir": profile.to_string_lossy(), "createdAtMs": 101, "state": state
        })
    }

    fn write_rows(&self, rows: Vec<serde_json::Value>) {
        std::fs::write(
            &self.ledger_path,
            serde_json::to_vec(&serde_json::json!({"bindings": rows})).unwrap(),
        )
        .unwrap();
    }

    /// 模拟关闭→卸载的收尾段：登记锚清空（登记行/绑定账本保留）。
    fn retire(&self) {
        self.registry
            .retire_identity_fingerprint(&self.instance_id)
            .unwrap();
        assert!(
            self.registry
                .registered(&self.instance_id)
                .unwrap()
                .identity_fingerprint
                .is_empty(),
            "前置：登记锚已退役"
        );
    }
}

/// 退役登记 + 撤权墓碑（绑定起源行）+ 登记行保留的只读证据 → 原会话仍可读：
/// 卸载销毁锚与身份目录，不销毁只读归属证据（关闭+卸载后历史可读的修复面）。
#[cfg(unix)]
#[test]
fn retired_readonly_passes_with_preserved_evidence_and_tombstone_home() {
    let f = RetiredReadonlyFixture::new_anchored(b"fixture-spki");
    f.write_rows(vec![f.row(
        "binding-a",
        &f.home.join("profiles/desktop"),
        "revoked",
        b"fixture-spki",
    )]);
    f.retire();
    assert_eq!(
        f.registry
            .resolve_readonly_root(&f.instance_id, "session-aaaaaaaa")
            .unwrap(),
        std::fs::canonicalize(&f.home).unwrap()
    );
    // 硬下界：不属于本实例的会话依旧拒绝。
    assert_eq!(
        f.registry
            .resolve_readonly_root(&f.instance_id, "session-zzzzzzzz")
            .unwrap_err()
            .code,
        codes::SESSION_OWNERSHIP_UNPROVEN
    );
}

/// 硬下界：从未锚定的候选登记（无只读证据）退役后依旧拒绝——证据保留不等于
/// 凭空授权（候选登记不得凭墓碑账本获得读权）。
#[cfg(unix)]
#[test]
fn retired_readonly_denies_never_anchored_record() {
    use multi_agents_manager_lib::services::dsh_control::auth::instance_state_dir;
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let profile = seed_profile(&home, "desktop");
    seed_session(&home, "project", "session-aaaaaaaa");
    let (env, _) = env_with(&home, vec![], vec![]);
    let registry = registry(&env, root.path());
    let instance = registry
        .register_candidate(HostKind::Desktop, &profile)
        .unwrap();
    registry.select(&instance.instance_id).unwrap();
    let ledger_path = instance_state_dir(&env, &instance.instance_id)
        .unwrap()
        .join("bindings.json");
    std::fs::create_dir_all(ledger_path.parent().unwrap()).unwrap();
    let row = serde_json::json!({
        "bindingId": "binding-a", "publicKeySpki": "",
        "revokedKeySpki": multi_agents_manager_lib::services::dsh_control::auth::b64url_encode(b"fixture-spki"),
        "executable": "/fixture/dsh", "pid": 1, "startedAtMs": 100,
        "profileDir": profile.to_string_lossy(), "createdAtMs": 101, "state": "revoked"
    });
    std::fs::write(
        &ledger_path,
        serde_json::to_vec(&serde_json::json!({"bindings": [row]})).unwrap(),
    )
    .unwrap();
    registry
        .retire_identity_fingerprint(&instance.instance_id)
        .unwrap();
    assert_eq!(
        registry
            .resolve_readonly_root(&instance.instance_id, "session-aaaaaaaa")
            .unwrap_err()
            .code,
        codes::SESSION_OWNERSHIP_UNPROVEN
    );
}

/// 硬下界：只读证据与墓碑账本不连续（登记锚钉的是另一把钥）→ 退役后仍拒绝。
#[cfg(unix)]
#[test]
fn retired_readonly_rejects_evidence_discontinuous_with_tombstone() {
    let f = RetiredReadonlyFixture::new_anchored(b"other-spki");
    f.write_rows(vec![f.row(
        "binding-a",
        &f.home.join("profiles/desktop"),
        "revoked",
        b"fixture-spki",
    )]);
    f.retire();
    assert_eq!(
        f.registry
            .resolve_readonly_root(&f.instance_id, "session-aaaaaaaa")
            .unwrap_err()
            .code,
        codes::SESSION_OWNERSHIP_UNPROVEN
    );
}

/// 硬下界：绑定起源行的入口证据多义（同一钥两条墓碑推导出两个自洽 home）→
/// 退役后仍拒绝（不从物理 profile 反推，不猜）。
#[cfg(unix)]
#[test]
fn retired_readonly_rejects_ambiguous_tombstone_homes() {
    use multi_agents_manager_lib::services::dsh_control::auth::key_digest;
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let home_a = root.path().join("home-a");
    let home_b = root.path().join("home-b");
    let physical = seed_profile(&home_b, "desktop");
    std::fs::create_dir_all(home_a.join("profiles")).unwrap();
    symlink(&physical, home_a.join("profiles/desktop")).unwrap();
    seed_session(&home_a, "project", "session-aaaaaaaa");
    let (env, _) = env_with(&home_a, vec![], vec![]);
    let registry = registry(&env, root.path());
    let instance = registry
        .register(
            HostKind::Desktop,
            &home_a.join("profiles/desktop"),
            &key_digest(b"fixture-spki"),
        )
        .unwrap();
    registry.select(&instance.instance_id).unwrap();
    let ledger_path = multi_agents_manager_lib::services::dsh_control::auth::instance_state_dir(
        &env,
        &instance.instance_id,
    )
    .unwrap()
    .join("bindings.json");
    std::fs::create_dir_all(ledger_path.parent().unwrap()).unwrap();
    let row = |id: &str, profile: &Path| {
        serde_json::json!({
            "bindingId": id, "publicKeySpki": "",
            "revokedKeySpki": multi_agents_manager_lib::services::dsh_control::auth::b64url_encode(b"fixture-spki"),
            "executable": "/fixture/dsh", "pid": 1, "startedAtMs": 100,
            "profileDir": profile.to_string_lossy(), "createdAtMs": 101, "state": "revoked"
        })
    };
    std::fs::write(
        &ledger_path,
        serde_json::to_vec(&serde_json::json!({"bindings": [
            row("binding-a", &home_a.join("profiles/desktop")),
            row("binding-b", &home_b.join("profiles/desktop")),
        ]}))
        .unwrap(),
    )
    .unwrap();
    registry
        .retire_identity_fingerprint(&instance.instance_id)
        .unwrap();
    assert_eq!(
        registry
            .resolve_readonly_root(&instance.instance_id, "session-aaaaaaaa")
            .unwrap_err()
            .code,
        codes::SESSION_OWNERSHIP_UNPROVEN
    );
}

/// 硬下界：退役登记 + 无任何绑定账本（不可核验）→ 拒绝（绝不把缺失当通过）。
#[cfg(unix)]
#[test]
fn retired_readonly_denies_without_binding_ledger() {
    let f = RetiredReadonlyFixture::new_anchored(b"fixture-spki");
    f.retire();
    assert_eq!(
        f.registry
            .resolve_readonly_root(&f.instance_id, "session-aaaaaaaa")
            .unwrap_err()
            .code,
        codes::SESSION_OWNERSHIP_UNPROVEN
    );
}

/// 复审 Important 回归：升级携带的锚定行（锚在、只读证据缺——旧构建形态）在本
/// 构建的三个授权触达点回填证据（再登记同钥 / 幂等 CAS / 退役销锚前），关闭+
/// 卸载后只读历史不断链（墓碑起源行对位回填证据）。回填只是复制授权侧已写的
/// 锚，不虚构授权；硬下界负例（无锚无证据 / 证据不连续）见上方退役用例组。
#[cfg(unix)]
#[test]
fn upgrade_shaped_row_backfills_read_evidence_at_anchor_touchpoints() {
    use multi_agents_manager_lib::services::dsh_control::auth::{instance_state_dir, key_digest};
    fn rewrite_upgrade_shaped(path: &Path, digest: &str) {
        let mut data: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        for instance in data["instances"].as_array_mut().unwrap() {
            let obj = instance.as_object_mut().unwrap();
            obj.insert(
                "identityFingerprint".into(),
                serde_json::Value::String(digest.to_string()),
            );
            obj.remove("readEvidenceFingerprint");
        }
        std::fs::write(path, serde_json::to_vec(&data).unwrap()).unwrap();
    }
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let profile = seed_profile(&home, "desktop");
    seed_session(&home, "project", "session-aaaaaaaa");
    std::fs::write(
        home.join("sessions/project/session-aaaaaaaa/session.jsonl"),
        b"{\"id\":\"session-aaaaaaaa\"}\n",
    )
    .unwrap();
    let (env, _) = env_with(&home, vec![], vec![]);
    let registry_path = root.path().join("instances.json");
    let registry = registry(&env, root.path());
    let instance = registry
        .register_candidate(HostKind::Desktop, &profile)
        .unwrap();
    registry.select(&instance.instance_id).unwrap();
    let digest = key_digest(b"fixture-spki");

    // ① 再登记同钥（register 锚等值分支）回填。
    rewrite_upgrade_shaped(&registry_path, &digest);
    let re = registry
        .register(HostKind::Desktop, &profile, &digest)
        .unwrap();
    assert_eq!(re.identity_fingerprint, digest);
    assert_eq!(
        re.read_evidence_fingerprint, digest,
        "再登记触达必须为升级携带行回填证据"
    );

    // ② 幂等 CAS（current == new）分支回填。
    rewrite_upgrade_shaped(&registry_path, &digest);
    registry
        .compare_and_set_fingerprint(&instance.instance_id, &digest, &digest)
        .unwrap();
    assert_eq!(
        registry
            .registered(&instance.instance_id)
            .unwrap()
            .read_evidence_fingerprint,
        digest,
        "幂等 CAS 触达必须为升级携带行回填证据"
    );

    // ③ 退役销锚前回填（关键点）：升级用户首次关闭+卸载的唯一保证触达点。
    rewrite_upgrade_shaped(&registry_path, &digest);
    registry
        .retire_identity_fingerprint(&instance.instance_id)
        .unwrap();
    let retired = registry.registered(&instance.instance_id).unwrap();
    assert!(retired.identity_fingerprint.is_empty(), "前置：锚已退役");
    assert_eq!(
        retired.read_evidence_fingerprint, digest,
        "退役必须为升级携带行回填证据（销锚前保全）"
    );
    // 墓碑起源行 + 回填证据 → 关闭+卸载后原会话仍可读。
    let ledger_path = instance_state_dir(&env, &instance.instance_id)
        .unwrap()
        .join("bindings.json");
    std::fs::create_dir_all(ledger_path.parent().unwrap()).unwrap();
    let row = serde_json::json!({
        "bindingId": "binding-a", "publicKeySpki": "",
        "revokedKeySpki": multi_agents_manager_lib::services::dsh_control::auth::b64url_encode(b"fixture-spki"),
        "executable": "/fixture/dsh", "pid": 1, "startedAtMs": 100,
        "profileDir": profile.to_string_lossy(), "createdAtMs": 101, "state": "revoked"
    });
    std::fs::write(
        &ledger_path,
        serde_json::to_vec(&serde_json::json!({"bindings": [row]})).unwrap(),
    )
    .unwrap();
    assert_eq!(
        registry
            .resolve_readonly_root(&instance.instance_id, "session-aaaaaaaa")
            .unwrap(),
        std::fs::canonicalize(&home).unwrap()
    );
}
