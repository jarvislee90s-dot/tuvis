//! 第九轮 P1-2 回归：关闭后、等待退出（已撤权）、卸载后三时点，同一已配对
//! 设备的历史只读不断链；负例（从未授权 / 指纹不匹配 / 多义 home 证据 / 证据
//! 指向别处）一律不放松。生产入口内核 = `InstanceRegistry::resolve_readonly_root`
//! （远程 api 消息/文件列表经 production_registry 消费同一判定）。
//!
//! 形态：真实目录 + 符号链接，默认 home（从物理 profile 词法推导，**刻意是错
//! 目录**）、逻辑 home（`profiles/<kind>` 链接所在）、物理 profile 三者互不相同；
//! 旧登记无 `dsh_home`（升级携带形态）。隔离数据根（tempdir），不触真实
//! `~/.tuvis` / `~/.dsh`。
//!
//! 仅 unix：符号链接夹具。

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use multi_agents_manager_lib::services::dsh_control::auth;
use multi_agents_manager_lib::services::dsh_control::error::{codes, DshError};
use multi_agents_manager_lib::services::dsh_control::instances::{
    HostKind, InstanceRegistry, SelectedInstance,
};
use multi_agents_manager_lib::services::dsh_control::model::{
    CommandRequest, DshClock, DshCommandRunner, DshEnvironment, DshFileSource, DshListener,
    DshProcess, DshProcessSource, RunOutcome, TestUnobservableEnvironments,
};

const KEY_A: &[u8] = b"p12-fixture-binding-key-a";
const SESSION: &str = "session-aaaaaaaa";

/// 真实文件只读投影（与 dsh_instance_test 的 RealFiles 同形）。
struct RealFiles;

impl DshFileSource for RealFiles {
    fn exists(&self, path: &Path) -> bool {
        std::fs::symlink_metadata(path).is_ok()
    }
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
        let meta = std::fs::metadata(path)
            .map_err(|e| DshError::new(codes::STATE_IO, format!("stat {}: {e}", path.display())))?;
        if meta.len() > max_bytes {
            return Err(DshError::new(
                codes::STATE_IO,
                format!("file over budget: {}", path.display()),
            ));
        }
        std::fs::read(path)
            .map_err(|e| DshError::new(codes::STATE_IO, format!("read {}: {e}", path.display())))
    }
}

struct NullRunner;
impl DshCommandRunner for NullRunner {
    fn run(&self, _request: &CommandRequest) -> Result<RunOutcome, DshError> {
        unreachable!("只读归属测试不执行命令")
    }
}

struct FixedClock;
impl DshClock for FixedClock {
    fn now_ms(&self) -> i64 {
        1_760_000_000_000
    }
}

struct NoProcs;
impl DshProcessSource for NoProcs {
    fn list_processes(&self) -> Vec<DshProcess> {
        Vec::new()
    }
    fn listening_sockets(&self) -> Vec<DshListener> {
        Vec::new()
    }
}

fn env_for(home: &Path, root: &Path) -> DshEnvironment {
    DshEnvironment {
        home: home.to_path_buf(),
        tuvis_home: root.join("tuvis"),
        dsh_state_dir: Some(root.join("state")),
        processes: Arc::new(NoProcs),
        files: Arc::new(RealFiles),
        runner: Arc::new(NullRunner),
        clock: Arc::new(FixedClock),
        environments: Arc::new(TestUnobservableEnvironments),
    }
}

fn clone_env(env: &DshEnvironment) -> DshEnvironment {
    DshEnvironment {
        home: env.home.clone(),
        tuvis_home: env.tuvis_home.clone(),
        dsh_state_dir: env.dsh_state_dir.clone(),
        processes: Arc::clone(&env.processes),
        files: Arc::clone(&env.files),
        runner: Arc::clone(&env.runner),
        clock: Arc::clone(&env.clock),
        environments: Arc::clone(&env.environments),
    }
}

fn seed_session(home: &Path, session_id: &str) {
    let dir = home.join("sessions/project").join(session_id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("session.jsonl"),
        format!("{{\"type\":\"session\",\"id\":\"{session_id}\"}}\n"),
    )
    .unwrap();
}

/// 单行绑定账本写入（`state` = "bound" / "revoked"；`spki` = 行上公钥；profile
/// 入口 = 逻辑形态）。
fn write_row(ledger_path: &Path, state: &str, spki: &str, entry: &Path) {
    let row = serde_json::json!({
        "bindingId": "p12-fixture",
        "publicKeySpki": if state == "bound" { spki } else { "" },
        "revokedKeySpki": if state == "revoked" { spki } else { "" },
        "executable": "/fixture/dsh", "pid": 1, "startedAtMs": 100,
        "profileDir": entry.to_string_lossy(),
        "createdAtMs": 101, "state": state
    });
    std::fs::write(
        ledger_path,
        serde_json::to_vec(&serde_json::json!({ "bindings": [row] })).unwrap(),
    )
    .unwrap();
}

/// 三时点回归台：逻辑 home（`profiles/desktop` 链接所在）+ 物理 profile（独立
/// 目录，词法推导 home 是错目录）+ 会话 + 锚定登记（无 dsh_home）+ 绑定账本。
struct Bench {
    _root: tempfile::TempDir,
    home: PathBuf,
    physical: PathBuf,
    env: DshEnvironment,
    registry: InstanceRegistry,
    registry_path: PathBuf,
    instance: SelectedInstance,
    ledger_path: PathBuf,
}

impl Bench {
    fn write_row(&self, state: &str) {
        write_row(
            &self.ledger_path,
            state,
            &auth::b64url_encode(KEY_A),
            &self.home.join("profiles/desktop"),
        );
    }

    /// 重开登记句柄（生产 registry 每次经进程单例重读同形）。
    fn reopen(&self) -> InstanceRegistry {
        InstanceRegistry::open_with_env(&self.registry_path, Some(Arc::new(clone_env(&self.env))))
            .unwrap()
    }

    fn readable_root(&self) -> PathBuf {
        self.reopen()
            .resolve_readonly_root(&self.instance.instance_id, SESSION)
            .expect("三时点判定内核必须放行已配对设备")
    }
}

/// 构造锚定登记（无 dsh_home）+ 单条 Bound 行的可读台。
fn bench() -> Bench {
    let root_temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(root_temp.path()).unwrap();
    // 物理 profile 独立成树：session_home(physical) = root/physical —— 刻意
    // 不同于逻辑 home（root/home-a）。
    let physical = root.join("physical/profile");
    std::fs::create_dir_all(&physical).unwrap();
    std::fs::write(physical.join("package.json"), b"{}").unwrap();
    let home = root.join("home-a");
    std::fs::create_dir_all(home.join("profiles")).unwrap();
    std::os::unix::fs::symlink(&physical, home.join("profiles/desktop")).unwrap();
    seed_session(&home, SESSION);
    let env = env_for(&home, &root);
    let registry_path = root.join("instances.json");
    let registry =
        InstanceRegistry::open_with_env(&registry_path, Some(Arc::new(clone_env(&env)))).unwrap();
    let instance = registry
        .register(HostKind::Desktop, &physical, &auth::key_digest(KEY_A))
        .expect("锚定登记");
    registry.select(&instance.instance_id).unwrap();
    assert_eq!(instance.dsh_home, None, "升级携带形态：登记无 dsh_home");
    let ledger_path = auth::instance_state_dir(&env, &instance.instance_id)
        .expect("实例 ID 恒合法")
        .join("bindings.json");
    std::fs::create_dir_all(ledger_path.parent().unwrap()).unwrap();
    let bench = Bench {
        _root: root_temp,
        home,
        physical,
        env,
        registry,
        registry_path,
        instance,
        ledger_path,
    };
    bench.write_row("bound");
    bench
}

/// 三时点可读（关闭前 / 等待退出已撤权 / 卸载后）。
#[test]
fn history_stays_readable_across_close_wait_and_uninstall() {
    let bench = bench();
    let expected = std::fs::canonicalize(&bench.home).unwrap();
    // 时点一：关闭前（Bound 行在场）。
    assert_eq!(bench.readable_root(), expected, "关闭前历史可读");
    // 关闭链的两个登记面事实（生产入口同形）：
    // ① 撤权前回填：登记写入口补 dsh_home（物理 profile 对位校验在入口内）。
    bench
        .registry
        .register_candidate_with_home(HostKind::Desktop, &bench.physical, Some(&bench.home))
        .expect("撤权前回填必须受理");
    // ② 撤权：Bound 翻墓碑（关闭流程的同步失效段）。
    bench.write_row("revoked");
    // 时点二：等待退出（已撤权、WaitingUninstall）——本轮修复的 403 窗口。
    assert_eq!(
        bench.readable_root(),
        expected,
        "等待退出（已撤权、锚未清）时点历史必须仍可读"
    );
    // 卸载退役：清锚、只读证据保留（生产卸载完成段同形）。
    bench
        .registry
        .retire_identity_fingerprint(&bench.instance.instance_id)
        .unwrap();
    // 时点三：卸载后。
    assert_eq!(bench.readable_root(), expected, "卸载后历史可读");
    let retired = bench
        .reopen()
        .registered(&bench.instance.instance_id)
        .unwrap();
    assert!(retired.identity_fingerprint.is_empty(), "退役清锚");
    assert_eq!(
        retired.read_evidence_fingerprint,
        auth::key_digest(KEY_A),
        "只读证据保留"
    );
}

/// 负例不放松：从未授权 / 指纹不匹配。
#[test]
fn anchored_tombstone_reads_stay_rejected_without_matching_evidence() {
    // ① 从未授权：候选登记（无锚无证据）→ 入口第一道门拒绝（账本在场也不获得读权）。
    {
        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        let physical = root.join("physical/profile");
        std::fs::create_dir_all(&physical).unwrap();
        std::fs::write(physical.join("package.json"), b"{}").unwrap();
        let home = root.join("home-a");
        std::fs::create_dir_all(home.join("profiles")).unwrap();
        std::os::unix::fs::symlink(&physical, home.join("profiles/desktop")).unwrap();
        seed_session(&home, SESSION);
        let env = env_for(&home, &root);
        let registry_path = root.join("instances.json");
        let registry =
            InstanceRegistry::open_with_env(&registry_path, Some(Arc::new(clone_env(&env))))
                .unwrap();
        let candidate = registry
            .register_candidate(HostKind::Desktop, &physical)
            .unwrap();
        registry.select(&candidate.instance_id).unwrap();
        assert_eq!(candidate.identity_fingerprint, "");
        let err = registry
            .resolve_readonly_root(&candidate.instance_id, SESSION)
            .unwrap_err();
        assert_eq!(err.code, codes::SESSION_OWNERSHIP_UNPROVEN);
    }
    // ② 指纹不匹配：墓碑留存的是另一把钥 → 起源行对位失败。
    let bench = bench();
    bench
        .registry
        .register_candidate_with_home(HostKind::Desktop, &bench.physical, Some(&bench.home))
        .unwrap();
    write_row(
        &bench.ledger_path,
        "revoked",
        &auth::b64url_encode(b"p12-fixture-other-key"),
        &bench.home.join("profiles/desktop"),
    );
    let err = bench
        .reopen()
        .resolve_readonly_root(&bench.instance.instance_id, SESSION)
        .unwrap_err();
    assert_eq!(
        err.code,
        codes::SESSION_OWNERSHIP_UNPROVEN,
        "指纹不匹配的墓碑不得放行"
    );
}

/// 负例不放松：两行不同 home 证据（同钥 Bound 行）= 多义，拒绝。
#[test]
fn multi_home_bound_evidence_stays_ambiguous_and_rejected() {
    let root = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(root.path()).unwrap();
    // 物理 profile 在 home-b 下；home-a 经符号链接对位同一物理 profile——
    // 两行 Bound 证据各自唯一推导出**不同** home 且都自洽 → 不可证唯一。
    let physical = root.join("home-b/profiles/desktop");
    std::fs::create_dir_all(&physical).unwrap();
    std::fs::write(physical.join("package.json"), b"{}").unwrap();
    let home_a = root.join("home-a");
    std::fs::create_dir_all(home_a.join("profiles")).unwrap();
    std::os::unix::fs::symlink(&physical, home_a.join("profiles/desktop")).unwrap();
    seed_session(&home_a, SESSION);
    seed_session(&root.join("home-b"), "session-bbbbbbbb");
    let env = env_for(&home_a, &root);
    let registry_path = root.join("instances.json");
    let registry =
        InstanceRegistry::open_with_env(&registry_path, Some(Arc::new(clone_env(&env)))).unwrap();
    let instance = registry
        .register(HostKind::Desktop, &physical, &auth::key_digest(KEY_A))
        .unwrap();
    registry.select(&instance.instance_id).unwrap();
    let ledger_path = auth::instance_state_dir(&env, &instance.instance_id)
        .unwrap()
        .join("bindings.json");
    std::fs::create_dir_all(ledger_path.parent().unwrap()).unwrap();
    let spki = auth::b64url_encode(KEY_A);
    let entry = |home: &Path| home.join("profiles/desktop").to_string_lossy().into_owned();
    let rows = serde_json::json!({ "bindings": [
        {
            "bindingId": "row-a", "publicKeySpki": spki, "revokedKeySpki": "",
            "executable": "/fixture/dsh", "pid": 1, "startedAtMs": 100,
            "profileDir": entry(&home_a), "createdAtMs": 101, "state": "bound"
        },
        {
            "bindingId": "row-b", "publicKeySpki": spki, "revokedKeySpki": "",
            "executable": "/fixture/dsh", "pid": 2, "startedAtMs": 102,
            "profileDir": entry(&root.join("home-b")), "createdAtMs": 103, "state": "bound"
        }
    ]});
    std::fs::write(&ledger_path, serde_json::to_vec(&rows).unwrap()).unwrap();
    let err = registry
        .resolve_readonly_root(&instance.instance_id, SESSION)
        .unwrap_err();
    assert_eq!(
        err.code,
        codes::SESSION_OWNERSHIP_UNPROVEN,
        "两行不同 home 证据 = 多义，拒绝（不猜、不反推）"
    );
}

/// 负例不放松：绑定行入口指向**另一处 home**（与登记物理 profile 不自洽）→
/// 不构成证据。
#[test]
fn evidence_pointing_at_another_home_stays_rejected() {
    let bench = bench();
    let other_home = bench.home.parent().unwrap().join("home-other");
    std::fs::create_dir_all(other_home.join("profiles")).unwrap();
    let other_physical = bench.physical.parent().unwrap().join("profile-other");
    std::fs::create_dir_all(&other_physical).unwrap();
    std::fs::write(other_physical.join("package.json"), b"{}").unwrap();
    std::os::unix::fs::symlink(&other_physical, other_home.join("profiles/desktop")).unwrap();
    write_row(
        &bench.ledger_path,
        "bound",
        &auth::b64url_encode(KEY_A),
        &other_home.join("profiles/desktop"),
    );
    let err = bench
        .reopen()
        .resolve_readonly_root(&bench.instance.instance_id, SESSION)
        .unwrap_err();
    assert_eq!(
        err.code,
        codes::SESSION_OWNERSHIP_UNPROVEN,
        "指向别处 home 的绑定行不是本实例的证据"
    );
}
