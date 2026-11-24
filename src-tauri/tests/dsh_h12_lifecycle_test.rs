//! H12 生命周期回归（不依赖真实握手/真实宿主）。
//!
//! 覆盖：账本损坏不静默、撤权墓碑跨重启、未绑定跨重启、实例域隔离；
//! 以及 Task 2 的宿主世代重启回归（A 退出 → B 同 profile 同钥启动 → 兔维斯
//! 自动重发布当代握手记录并恢复授权，旧代连接/凭据失效）。
//! 全部使用隔离 home 与内存/临时状态目录，不触真实 DSH 与本机配对。
#![cfg(unix)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use multi_agents_manager_lib::services::dsh_control::auth;
use multi_agents_manager_lib::services::dsh_control::error::{codes, DshError};
use multi_agents_manager_lib::services::dsh_control::identity;
use multi_agents_manager_lib::services::dsh_control::instances::{instance_id_for, HostKind};
use multi_agents_manager_lib::services::dsh_control::local_ipc::mem::{
    mem_listener, MemClient, MemClientFactory,
};
use multi_agents_manager_lib::services::dsh_control::local_ipc::PeerOsIdentity;
use multi_agents_manager_lib::services::dsh_control::model::{
    AuthorizationState, CommandRequest, DshCommandRunner, DshEnvironment, DshFileSource,
    DshListener, DshProcess, DshProcessSource, LocalHost, RunOutcome,
};
use multi_agents_manager_lib::services::dsh_control::production::{self, FsFileSource};

struct NoProcesses;
impl DshProcessSource for NoProcesses {
    fn list_processes(&self) -> Vec<DshProcess> {
        Vec::new()
    }
    fn listening_sockets(&self) -> Vec<DshListener> {
        Vec::new()
    }
}

struct NoRunner;
impl DshCommandRunner for NoRunner {
    fn run(
        &self,
        _: &CommandRequest,
    ) -> Result<RunOutcome, multi_agents_manager_lib::services::dsh_control::error::DshError> {
        panic!("生命周期测试不得执行 CLI")
    }
}

struct FixedClock(AtomicI64);
impl multi_agents_manager_lib::services::dsh_control::model::DshClock for FixedClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

fn fixture_env(root: &std::path::Path) -> DshEnvironment {
    let home = root.join("dsh-home");
    std::fs::create_dir_all(&home).expect("home");
    DshEnvironment {
        home,
        tuvis_home: root.join("tuvis"),
        dsh_state_dir: Some(root.join("state")),
        processes: Arc::new(NoProcesses),
        files: Arc::new(FsFileSource) as Arc<dyn DshFileSource>,
        runner: Arc::new(NoRunner),
        clock: Arc::new(FixedClock(AtomicI64::new(1_760_000_000_000))),
        // 测试默认「不可观测」：显式拒绝 Web home 归属（fail-closed），
        // 绝不让假进程源落到真实系统 PID 上。需要 Web 归属的用例自行注入假观测器。
        environments: std::sync::Arc::new(
            multi_agents_manager_lib::services::dsh_control::model::TestUnobservableEnvironments,
        ),
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
        // 测试默认「不可观测」：显式拒绝 Web home 归属（fail-closed），
        // 绝不让假进程源落到真实系统 PID 上。需要 Web 归属的用例自行注入假观测器。
        environments: std::sync::Arc::new(
            multi_agents_manager_lib::services::dsh_control::model::TestUnobservableEnvironments,
        ),
    }
}

const INSTANCE: &str = "0123456789abcdef0123456789abcdef";

/// 未绑定实例重启后仍是 Unbound（空账本不得被说成已授权）。
#[tokio::test]
async fn fresh_instance_stays_unbound_across_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let env = fixture_env(dir.path());
    {
        let svc = auth::DshAuthService::new_for_instance(clone_env(&env), INSTANCE)
            .expect("实例域可装配");
        assert_eq!(svc.authorization_state(), AuthorizationState::Unbound);
    }
    let restarted =
        auth::DshAuthService::new_for_instance(clone_env(&env), INSTANCE).expect("重启后装配");
    assert_eq!(
        restarted.authorization_state(),
        AuthorizationState::Unbound,
        "空账本重启后必须仍是未绑定"
    );
}

/// 绑定账本被写坏后必须**拒绝启动该实例授权域**，不得静默降级成空账本
/// （否则墓碑消失、旧绑定会复活）。
#[tokio::test]
async fn corrupted_ledger_fails_closed_instead_of_resetting_to_unbound() {
    let dir = tempfile::tempdir().expect("tempdir");
    let env = fixture_env(dir.path());
    let state = auth::instance_state_dir(&env, INSTANCE).expect("实例子目录");
    std::fs::create_dir_all(&state).expect("state dir");
    std::fs::write(state.join("bindings.json"), b"not valid ledger json").expect("corrupt");

    let err = match auth::DshAuthService::new_for_instance(clone_env(&env), INSTANCE) {
        Ok(_) => panic!("损坏账本必须拒绝装配"),
        Err(err) => err,
    };
    assert_eq!(err.code, codes::IPC_STATE, "损坏账本必须如实报错: {err}");
}

/// 每个实例的账本彼此隔离：一个实例损坏不影响另一个。
#[tokio::test]
async fn corrupted_ledger_of_one_instance_does_not_affect_another() {
    let dir = tempfile::tempdir().expect("tempdir");
    let env = fixture_env(dir.path());
    let other = "fedcba9876543210fedcba9876543210";
    let bad = auth::instance_state_dir(&env, INSTANCE).expect("实例子目录");
    std::fs::create_dir_all(&bad).expect("state dir");
    std::fs::write(bad.join("bindings.json"), b"{broken").expect("corrupt");

    assert!(
        auth::DshAuthService::new_for_instance(clone_env(&env), INSTANCE).is_err(),
        "损坏实例必须拒绝"
    );
    let healthy =
        auth::DshAuthService::new_for_instance(clone_env(&env), other).expect("另一个实例不受影响");
    assert_eq!(healthy.authorization_state(), AuthorizationState::Unbound);
}

/// 实例切换后 Rendezvous 落到各自实例子目录（不共享一个文件）。
#[tokio::test]
async fn rendezvous_files_are_per_instance() {
    let dir = tempfile::tempdir().expect("tempdir");
    let env = fixture_env(dir.path());
    let a = auth::instance_state_dir(&env, INSTANCE).expect("a");
    let b = auth::instance_state_dir(&env, "fedcba9876543210fedcba9876543210").expect("b");
    assert_ne!(a, b, "两个实例的状态目录必须不同");
    // 初始化两个域（会写各自密钥/账本），确认互不覆盖。
    let _a = auth::DshAuthService::new_for_instance(clone_env(&env), INSTANCE).expect("a 装配");
    let _b =
        auth::DshAuthService::new_for_instance(clone_env(&env), "fedcba9876543210fedcba9876543210")
            .expect("b 装配");
    assert!(a.join("bindings.json").exists() || a.exists());
    assert!(b.exists());
}

/// 撤权墓碑跨重启保持（通过真实账本文件写入，再重启读取）。
#[tokio::test]
async fn tombstone_remains_revoked_after_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let env = fixture_env(dir.path());
    let state = auth::instance_state_dir(&env, INSTANCE).expect("state dir");
    std::fs::create_dir_all(&state).expect("create");
    // 生产账本形态：一条已撤权墓碑行。
    let ledger = serde_json::json!({
        "bindings": [{
            "bindingId": "b-h12",
            "publicKeySpki": "",
            "revokedKeySpki": "revoked-key",
            "executable": "/synthetic/dsh",
            "pid": 1,
            "startedAtMs": 1,
            "profileDir": "/synthetic/home/profiles/desktop",
            "createdAtMs": 1,
            "state": "revoked"
        }]
    });
    std::fs::write(
        state.join("bindings.json"),
        serde_json::to_vec_pretty(&ledger).unwrap(),
    )
    .expect("write ledger");

    let svc =
        auth::DshAuthService::new_for_instance(clone_env(&env), INSTANCE).expect("墓碑账本可装配");
    assert_eq!(
        svc.authorization_state(),
        AuthorizationState::Revoked,
        "重启后必须仍报告已撤权"
    );
}

/// H12-b：**换代不复活已撤权绑定**。
///
/// 场景：某实例在 E1 被撤权（墓碑行）；宿主重启到 E2（`startedAtMs` 变化）。
/// 期望：重启后仍是 Revoked——换代只改变「哪一代活体在场」，**不**改变
/// 「这把钥匙已被永久吊销」这一事实。
#[tokio::test]
async fn host_upgrade_does_not_revive_a_revoked_binding() {
    let dir = tempfile::tempdir().expect("tempdir");
    let env = fixture_env(dir.path());
    let state = auth::instance_state_dir(&env, INSTANCE).expect("state dir");
    std::fs::create_dir_all(&state).expect("create");
    let epoch_one_started_at = 1_760_000_000_000i64;
    let ledger = serde_json::json!({
        "bindings": [
            {
                "bindingId": "b-old-epoch",
                "publicKeySpki": "",
                "revokedKeySpki": "revoked-key-e1",
                "executable": "/synthetic/dsh",
                "pid": 1,
                "startedAtMs": epoch_one_started_at,
                "profileDir": "/synthetic/home/profiles/desktop",
                "createdAtMs": 1,
                "state": "revoked"
            },
            {
                // 另一把仍有效的钥匙也必须保持有效——换代不是全量清空。
                "bindingId": "b-still-bound",
                "publicKeySpki": "spki-live",
                "revokedKeySpki": "",
                "executable": "/synthetic/dsh",
                "pid": 1,
                "startedAtMs": epoch_one_started_at,
                "profileDir": "/synthetic/home/profiles/desktop",
                "createdAtMs": 2,
                "state": "bound"
            }
        ]
    });
    std::fs::write(
        state.join("bindings.json"),
        serde_json::to_vec_pretty(&ledger).unwrap(),
    )
    .expect("write ledger");

    // 宿主换代：时钟推进（equivalent to started_at changing on restart）。
    let mut restarted = clone_env(&env);
    restarted.clock = Arc::new(FixedClock(AtomicI64::new(epoch_one_started_at + 60_000)));
    let svc = auth::DshAuthService::new_for_instance(restarted, INSTANCE).expect("换代后装配");
    assert_eq!(
        svc.authorization_state(),
        AuthorizationState::Bound,
        "仍有 bound 行时状态为 Bound（换代不改变已 Bound 的结论）"
    );

    // 只留墓碑行 → 换代后仍是 Revoked，不会被当成 Unbound（= 可重新批准）。
    let only_tombstone = serde_json::json!({
        "bindings": [{
            "bindingId": "b-old-epoch",
            "publicKeySpki": "",
            "revokedKeySpki": "revoked-key-e1",
            "executable": "/synthetic/dsh",
            "pid": 1,
            "startedAtMs": epoch_one_started_at,
            "profileDir": "/synthetic/home/profiles/desktop",
            "createdAtMs": 1,
            "state": "revoked"
        }]
    });
    std::fs::write(
        state.join("bindings.json"),
        serde_json::to_vec_pretty(&only_tombstone).unwrap(),
    )
    .expect("rewrite ledger");
    let mut restarted = clone_env(&env);
    restarted.clock = Arc::new(FixedClock(AtomicI64::new(epoch_one_started_at + 120_000)));
    let svc = auth::DshAuthService::new_for_instance(restarted, INSTANCE).expect("仅墓碑也能装配");
    assert_eq!(
        svc.authorization_state(),
        AuthorizationState::Revoked,
        "换代后墓碑必须仍是 Revoked"
    );
}

// ============================================================
// Task 2：宿主世代重启回归（A 退出 → B 同 profile 同钥启动 → 自动重连）
// ============================================================

const RESTART_NOW: i64 = 1_760_000_000_000;
const RESTART_EXE: &str = "/synthetic-restart/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness";
const RESTART_WAIT: Duration = Duration::from_secs(8);
/// 活体跟踪拍间隔（与服务端 LIVENESS_POLL 对齐的轮询步长）。
const TRACK_STEP: Duration = Duration::from_millis(100);

/// 可变进程表（同一 env 里先后投放不同宿主代际）。
struct MutableProcesses {
    processes: Arc<Mutex<Vec<DshProcess>>>,
}

impl DshProcessSource for MutableProcesses {
    fn list_processes(&self) -> Vec<DshProcess> {
        self.processes.lock().unwrap().clone()
    }

    fn listening_sockets(&self) -> Vec<DshListener> {
        self.processes
            .lock()
            .unwrap()
            .iter()
            .enumerate()
            .map(|(i, p)| DshListener {
                owner_pid: p.pid,
                authority: format!("127.0.0.1:{}", 25_101 + i),
            })
            .collect()
    }
}

/// 夹具文件源：只暴露 profile 目录与 package.json 标记（desktop_candidate 核验面）。
struct RestartFiles(BTreeSet<PathBuf>);

impl DshFileSource for RestartFiles {
    fn exists(&self, path: &Path) -> bool {
        self.0.contains(path)
    }

    fn list_dir(&self, _path: &Path) -> Vec<PathBuf> {
        Vec::new()
    }

    fn read_small(&self, path: &Path, max_bytes: u64) -> Result<Vec<u8>, DshError> {
        if self.0.contains(path)
            && path.file_name().is_some_and(|s| s == "package.json")
            && max_bytes >= 2
        {
            Ok(b"{}".to_vec())
        } else {
            Err(DshError::new(
                codes::UNSUPPORTED,
                "fixture file unavailable",
            ))
        }
    }

    fn read_link_target(&self, _path: &Path) -> Option<PathBuf> {
        None
    }

    fn is_readable_regular_file(&self, path: &Path) -> bool {
        self.0.contains(path) && path.file_name().is_some_and(|s| s == "package.json")
    }
}

/// 世代重启夹具：同一 profile、可变进程表（A 退出 → B 上场）、隔离状态目录。
struct RestartBench {
    env: DshEnvironment,
    processes: Arc<Mutex<Vec<DshProcess>>>,
    profile: PathBuf,
    /// 另一实例的 profile（跨实例反例投放用；同 home 双实例歧义纪律照旧）。
    foreign_profile: PathBuf,
    instance_id: String,
    _tmp: tempfile::TempDir,
}

fn desktop_process(profile: &Path, pid: u32, started_at_ms: i64) -> DshProcess {
    DshProcess {
        pid,
        exe: RESTART_EXE.into(),
        cmdline: vec![
            RESTART_EXE.into(),
            "/synthetic-restart/node_modules/@deepseek-ai/dsh-desktop-host/lib/index.js".into(),
            profile.to_string_lossy().into_owned(),
        ],
        started_at_ms,
    }
}

impl RestartBench {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dsh_home = tmp.path().join("dsh-home");
        let profile = dsh_home.join("profiles/desktop");
        let foreign_profile = tmp.path().join("other-home/profiles/desktop");
        let mut files = BTreeSet::new();
        files.insert(profile.clone());
        files.insert(profile.join("package.json"));
        files.insert(foreign_profile.clone());
        files.insert(foreign_profile.join("package.json"));
        let processes: Arc<Mutex<Vec<DshProcess>>> = Arc::new(Mutex::new(Vec::new()));
        let env = DshEnvironment {
            home: dsh_home,
            tuvis_home: tmp.path().join("tuvis"),
            dsh_state_dir: Some(tmp.path().join("state")),
            processes: Arc::new(MutableProcesses {
                processes: Arc::clone(&processes),
            }),
            files: Arc::new(RestartFiles(files)) as Arc<dyn DshFileSource>,
            runner: Arc::new(NoRunner),
            clock: Arc::new(FixedClock(AtomicI64::new(RESTART_NOW))),
            // 测试默认「不可观测」：显式拒绝 Web home 归属（fail-closed）。
            environments: std::sync::Arc::new(
                multi_agents_manager_lib::services::dsh_control::model::TestUnobservableEnvironments,
            ),
        };
        let instance_id = instance_id_for(HostKind::Desktop, &profile);
        Self {
            env,
            processes,
            profile,
            foreign_profile,
            instance_id,
            _tmp: tmp,
        }
    }

    /// 投放一代宿主（替换整个进程表——上一代退出、新一代上场一步到位）。
    fn set_host(&self, pid: u32, started_at_ms: i64) {
        *self.processes.lock().unwrap() = vec![desktop_process(&self.profile, pid, started_at_ms)];
    }

    /// 投放**另一实例**的宿主（本域宿主已退出时的跨实例反例）。
    fn set_foreign_host(&self, pid: u32, started_at_ms: i64) {
        *self.processes.lock().unwrap() =
            vec![desktop_process(&self.foreign_profile, pid, started_at_ms)];
    }

    /// 全部宿主退出。
    fn clear_hosts(&self) {
        self.processes.lock().unwrap().clear();
    }

    /// 当代活体（发现单源）。
    fn live_host(&self) -> LocalHost {
        identity::discover_instance(&self.env, &self.instance_id).expect("夹具宿主必须可发现")
    }

    /// homeBinding 的 homeRoot（DSH home = profile 两级上溯）。
    fn home_root(&self) -> PathBuf {
        self.profile
            .parent()
            .and_then(|p| p.parent())
            .expect("profile 形态 <home>/profiles/desktop")
            .to_path_buf()
    }
}

fn restart_service(
    bench: &RestartBench,
    peer_pid: u32,
) -> (Arc<auth::DshAuthService>, Arc<MemClientFactory>) {
    let (listener, factory) = mem_listener(PeerOsIdentity {
        pid: peer_pid,
        uid: None,
    });
    let svc = Arc::new(
        production::init_instance_auth(&bench.env, &bench.instance_id)
            .expect("实例域可装配")
            .with_listener(Box::new(listener)),
    );
    (svc, factory)
}

fn read_record(bench: &RestartBench) -> Option<serde_json::Value> {
    let path = auth::instance_state_dir(&bench.env, &bench.instance_id)
        .expect("实例状态目录")
        .join("rendezvous.json");
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

async fn exchange(conn: &MemClient, wire: &serde_json::Value) -> serde_json::Value {
    tokio::time::timeout(RESTART_WAIT, async {
        conn.send(&wire.to_string()).await.expect("发送帧");
        serde_json::from_str(&conn.recv().await.expect("应答帧")).expect("应答帧可解析")
    })
    .await
    .expect("握手往返不得悬挂")
}

/// homeBinding wire（typed struct 保证字段顺序与生产编码器一致，签名稳定）。
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct RestartHome {
    home_root: String,
    host_pid: u32,
    host_started_at_ms: i64,
}

/// 被签字节（固定 6+1 字段；顺序与生产 BindingChallengeMessage 一致）。
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct RestartChallenge {
    binding_protocol: u32,
    key_digests: [String; 2],
    nonce: String,
    host_id: String,
    epoch: String,
    expiry: i64,
    home_binding: RestartHome,
}

/// 构造一次合法的 v2 hello（客户端签名真实；宿主事实取当代发现；
/// keyDigests[0] = 服务端（Rust）公钥摘要，keyDigests[1] = 客户端公钥摘要）。
fn hello_wire(
    rust_key_digest: &str,
    keys: &auth::RustSigningKey,
    host: &LocalHost,
    nonce: u8,
    home_root: &Path,
) -> serde_json::Value {
    let message = RestartChallenge {
        binding_protocol: 2,
        key_digests: [rust_key_digest.to_string(), auth::key_digest(&keys.spki)],
        nonce: auth::b64url_encode(&[nonce; 32]),
        host_id: host.host_id.clone(),
        epoch: host.epoch.to_string(),
        expiry: RESTART_NOW + 60_000,
        home_binding: RestartHome {
            home_root: home_root.to_string_lossy().into_owned(),
            host_pid: host.pid,
            host_started_at_ms: host.started_at_ms,
        },
    };
    serde_json::json!({
        "type": "hello", "bindingProtocol": 2,
        "publicKeySpki": auth::b64url_encode(&keys.spki),
        "executable": host.executable.to_string_lossy(),
        "pid": host.pid, "startedAtMs": host.started_at_ms,
        "hostId": message.host_id, "epoch": message.epoch,
        "nonce": message.nonce, "expiry": message.expiry,
        "signature": auth::b64url_encode(&keys.sign(&serde_json::to_vec(&message).unwrap())),
        "homeBinding": serde_json::to_value(&message.home_binding).unwrap(),
    })
}

/// 交付凭据帧（API cookie + 浏览器 cookie 分别承载；wire 真实）。
async fn deliver_credential(
    conn: &MemClient,
    binding_id: &str,
    cookie: &str,
    browser_cookie: &str,
) -> serde_json::Value {
    let frame = auth::ClientFrame::Credential(auth::CredentialFrame {
        binding_id: binding_id.to_string(),
        cookie: cookie.to_string(),
        browser_cookie: Some(browser_cookie.to_string()),
    });
    tokio::time::timeout(RESTART_WAIT, async {
        conn.send(&auth::client_frame_line(&frame))
            .await
            .expect("凭据帧发送");
        serde_json::from_str(&conn.recv().await.expect("凭据应答")).expect("凭据应答可解析")
    })
    .await
    .expect("凭据交付不得悬挂")
}

/// 调用方身份（authorize_grant 的 OS 进程核对面；与 hello 自报同源）。
fn client_identity(
    keys: &auth::RustSigningKey,
    host: &LocalHost,
) -> multi_agents_manager_lib::services::dsh_control::model::ClientIdentity {
    multi_agents_manager_lib::services::dsh_control::model::ClientIdentity {
        public_key_spki: keys.spki.clone(),
        executable: host.executable.clone(),
        pid: host.pid,
        started_at_ms: host.started_at_ms,
    }
}

/// 完整首绑：hello → 待批 → 批准 → Granted → 凭据交付 → ACK。返回连接与绑定号。
async fn bind_and_deliver(
    svc: &Arc<auth::DshAuthService>,
    factory: &Arc<MemClientFactory>,
    keys: &auth::RustSigningKey,
    host: &LocalHost,
    profile: &Path,
) -> (MemClient, String) {
    let conn = factory.connect().await;
    let reply = exchange(&conn, &hello_wire(svc.key_digest(), keys, host, 1, profile)).await;
    assert_eq!(reply["type"], "server_hello", "首绑必须可达：{reply}");
    assert_eq!(
        reply["state"], "pending_approval",
        "首绑必须经人工批准：{reply}"
    );
    let binding_id = reply["bindingId"].as_str().expect("待批绑定号").to_string();
    svc.approve(&binding_id).await.expect("批准");
    let granted = serde_json::from_str::<serde_json::Value>(
        &tokio::time::timeout(RESTART_WAIT, conn.recv())
            .await
            .expect("批准回执有界")
            .expect("批准回执"),
    )
    .expect("批准回执可解析");
    assert_eq!(granted["type"], "granted", "批准推送：{granted}");
    let ack = deliver_credential(
        &conn,
        &binding_id,
        "restart-op-cookie",
        "restart-browser-cookie",
    )
    .await;
    assert_eq!(
        ack["event"], "credential_ack",
        "凭据交付必须有本代 ACK：{ack}"
    );
    (conn, binding_id)
}

/// Task 2 核心回归（brief 验收①）：A 已绑定 → A 退出 → B 用同一 profile 和同一把
/// 钥匙启动。B 在**没有预先连接 IPC、没有点击绑定**的情况下取得新记录并恢复授权。
///
/// RED（旧行为）：A 退出后无人重发布记录——记录仍指向已退出的 A 代 PID，B 永远
/// 找不到匹配记录而保持 idle。GREEN（修复后）：兔维斯活体跟踪发现换代，自动把
/// 当代记录写进本实例 rendezvous；B 连接后同钥握手直接 `authorized`（无需再批准），
/// 凭据分别交付成功，旧代连接已被失效。
#[tokio::test]
async fn host_restart_republishes_record_and_reauthorizes_same_key_without_approval() {
    let bench = RestartBench::new();
    bench.set_host(4201, RESTART_NOW - 10_000);
    let host_a = bench.live_host();
    let (svc, factory) = restart_service(&bench, 4201);
    svc.serve()
        .await
        .expect("服务启动（A 代在场，初始发布成功）");
    let record_a = read_record(&bench).expect("A 代记录必须已发布");
    assert_eq!(
        record_a["processIdentity"]["pid"], 4201,
        "初始记录指向 A 代：{record_a}"
    );
    assert_ne!(record_a["retired"], serde_json::Value::Bool(true));

    let keys = auth::RustSigningKey::generate();
    let (conn_a, binding_id) =
        bind_and_deliver(&svc, &factory, &keys, &host_a, &bench.home_root()).await;
    assert!(svc.has_delivered_peer(&binding_id), "A 代凭据已交付");
    assert_eq!(svc.authorization_state(), AuthorizationState::Bound);

    // A 退出；B 用同一 profile（同一实例、同一把插件钥匙）上场。
    bench.clear_hosts();
    bench.set_host(4202, RESTART_NOW - 5_000);
    let host_b = bench.live_host();
    assert_ne!(host_a.epoch, host_b.epoch, "重启即换代");

    // B 取得新记录：不允许 B 先连接、不允许任何点击——只等兔维斯自动重发布。
    let deadline = std::time::Instant::now() + RESTART_WAIT;
    loop {
        if let Some(record) = read_record(&bench) {
            if record["processIdentity"]["pid"] == 4202
                && record["epoch"].as_str().and_then(|e| e.parse::<u64>().ok())
                    == Some(host_b.epoch)
                && record["retired"] != serde_json::Value::Bool(true)
            {
                break;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "宿主换代后必须自动重发布当代握手记录（不允许预连接/点击绑定）"
        );
        tokio::time::sleep(TRACK_STEP).await;
    }

    // 旧代连接必须已被失效（A 的受信连接在换代时被服务端关闭）。
    assert!(!conn_a.write_available(), "旧代受信连接必须在换代时失效");
    // 旧代临时凭据不可继续使用：换代重交付前，当代授权入口必须以 EPOCH_STALE
    // 拒绝旧代句柄（T09-G 同一合同：诊断码指向「等待当代重交付」）。
    let caller_b = client_identity(&keys, &host_b);
    let stale = svc
        .authorize_grant(&host_b, &caller_b)
        .await
        .expect_err("换代重交付前旧代凭据必须被拒");
    assert_eq!(stale.code, codes::AUTH_EPOCH_STALE, "实际：{stale:?}");

    // B 连接（全新会话、同一把钥匙）：同钥重连必须直接 authorized，无需再批准。
    let conn_b = factory
        .connect_as(PeerOsIdentity {
            pid: 4202,
            uid: None,
        })
        .await;
    let reply = exchange(
        &conn_b,
        &hello_wire(svc.key_digest(), &keys, &host_b, 2, &bench.home_root()),
    )
    .await;
    assert_eq!(reply["type"], "server_hello", "B 握手必须可达：{reply}");
    assert_eq!(
        reply["state"], "authorized",
        "同一受信公钥换代必须自动重授权（无需用户再确认）：{reply}"
    );
    let binding_b = reply["bindingId"].as_str().expect("授权绑定号");
    assert_eq!(binding_b, binding_id, "同钥重连沿用同一绑定号");
    let ack = deliver_credential(
        &conn_b,
        binding_b,
        "restart-op-cookie-b",
        "restart-browser-cookie-b",
    )
    .await;
    assert_eq!(
        ack["event"], "credential_ack",
        "B 代凭据交付必须有 ACK：{ack}"
    );
    assert_eq!(svc.authorization_state(), AuthorizationState::Bound);
    assert_eq!(
        svc.current_generation_key_digest().unwrap(),
        Some(auth::key_digest(&keys.spki)),
        "B 代当代指纹必须恢复"
    );
}

/// Task 2 验收②（兔维斯先启动）：应用启动时 DSH 尚未在场——serve 必须成功（监听
/// 就绪、记录推迟），宿主上场后自动补发布；随后同钥重连自动恢复授权。
#[tokio::test]
async fn tuvis_before_dsh_publishes_record_when_host_appears() {
    let bench = RestartBench::new();
    // 兔维斯先启动：进程表为空（DSH 尚未启动）。
    let (svc, factory) = restart_service(&bench, 4202);
    svc.serve()
        .await
        .expect("宿主缺席不得让授权服务退役（初始发布推迟）");
    assert!(
        read_record(&bench).is_none(),
        "宿主缺席时不得发布指向空代际的记录"
    );

    // DSH 启动（B 代）：记录必须自动补发布（本拍显式驱动；周期拍同样覆盖）。
    bench.set_host(4202, RESTART_NOW - 5_000);
    let host_b = bench.live_host();
    let outcome = svc
        .refresh_host_tracking()
        .expect("宿主上场后的跟踪拍不得失败");
    assert!(
        matches!(
            outcome,
            auth::HostTracking::Published { .. } | auth::HostTracking::Current
        ),
        "宿主上场后必须发布当代记录：{outcome:?}"
    );
    let record = read_record(&bench).expect("宿主上场后必须有当代记录");
    assert_eq!(record["processIdentity"]["pid"], 4202, "{record}");
    assert_eq!(record["epoch"], host_b.epoch.to_string(), "{record}");

    // 同钥重连在补发布后照常自动授权（同一受信公钥换代无需再确认）。
    let keys = auth::RustSigningKey::generate();
    let conn = factory
        .connect_as(PeerOsIdentity {
            pid: 4202,
            uid: None,
        })
        .await;
    let reply = exchange(
        &conn,
        &hello_wire(svc.key_digest(), &keys, &host_b, 1, &bench.home_root()),
    )
    .await;
    assert_eq!(reply["type"], "server_hello", "补发布后握手可达：{reply}");
    assert_eq!(
        reply["state"], "pending_approval",
        "本域从未绑定过这把钥：首次仍是 TOFU 待批（不属于重连授权面）：{reply}"
    );
}

/// Task 2 验收②（DSH 先启动 + 连续两次重启）：A 绑定交付 → A 退出 → B → 退出 →
/// C，每一代都自动重发布；旧代连接与临时凭据在换代时失效；同钥重连免批准。
#[tokio::test]
async fn two_consecutive_restarts_track_generations_and_invalidate_old() {
    let bench = RestartBench::new();
    bench.set_host(4201, RESTART_NOW - 10_000);
    let host_a = bench.live_host();
    let (svc, factory) = restart_service(&bench, 4201);
    svc.serve().await.expect("A 代服务启动");
    let keys = auth::RustSigningKey::generate();
    let (conn_a, binding_id) =
        bind_and_deliver(&svc, &factory, &keys, &host_a, &bench.home_root()).await;

    // 第一次重启：A → B。
    bench.set_host(4202, RESTART_NOW - 5_000);
    let host_b = bench.live_host();
    let _ = svc.refresh_host_tracking().expect("第一代换代跟踪");
    let record = read_record(&bench).expect("B 代记录");
    assert_eq!(record["processIdentity"]["pid"], 4202, "{record}");
    assert_ne!(record["retired"], serde_json::Value::Bool(true));
    assert!(!conn_a.write_available(), "A 代连接必须在换代时失效");
    // A 代凭据不可继续使用：B 代重交付前，当代授权入口按 T09-G 合同拒绝旧代句柄。
    let stale = svc
        .authorize_grant(&host_b, &client_identity(&keys, &host_b))
        .await
        .expect_err("换代重交付前旧代凭据必须被拒");
    assert_eq!(stale.code, codes::AUTH_EPOCH_STALE, "实际：{stale:?}");
    let conn_b = factory
        .connect_as(PeerOsIdentity {
            pid: 4202,
            uid: None,
        })
        .await;
    let reply = exchange(
        &conn_b,
        &hello_wire(svc.key_digest(), &keys, &host_b, 2, &bench.home_root()),
    )
    .await;
    assert_eq!(reply["state"], "authorized", "同钥换代免批准：{reply}");
    let ack = deliver_credential(&conn_b, binding_id.as_str(), "cookie-b", "browser-b").await;
    assert_eq!(ack["event"], "credential_ack", "B 代凭据重新交付：{ack}");

    // 第二次重启：B → C。
    bench.set_host(4203, RESTART_NOW - 1_000);
    let host_c = bench.live_host();
    let _ = svc.refresh_host_tracking().expect("第二代换代跟踪");
    let record = read_record(&bench).expect("C 代记录");
    assert_eq!(record["processIdentity"]["pid"], 4203, "{record}");
    assert!(!conn_b.write_available(), "B 代连接必须在第二次换代时失效");
    let conn_c = factory
        .connect_as(PeerOsIdentity {
            pid: 4203,
            uid: None,
        })
        .await;
    let reply = exchange(
        &conn_c,
        &hello_wire(svc.key_digest(), &keys, &host_c, 3, &bench.home_root()),
    )
    .await;
    assert_eq!(
        reply["state"], "authorized",
        "连续换代同钥仍免批准：{reply}"
    );
    assert_eq!(
        reply["bindingId"].as_str(),
        Some(binding_id.as_str()),
        "三代共用同一绑定号"
    );
    let ack = deliver_credential(&conn_c, binding_id.as_str(), "cookie-c", "browser-c").await;
    assert_eq!(ack["event"], "credential_ack", "C 代凭据重新交付：{ack}");
    assert_eq!(
        svc.current_generation_key_digest().unwrap(),
        Some(auth::key_digest(&keys.spki)),
        "C 代当代指纹恢复"
    );
}

/// 宿主退出：记录必须停用（retired = true，防 PID 复用者把它当自己的入口），
/// 旧代连接/凭据失效，且重复跟踪拍幂等（不重复失效、不复活记录）。
#[tokio::test]
async fn host_exit_marks_record_retired_and_invalidation_is_idempotent() {
    let bench = RestartBench::new();
    bench.set_host(4201, RESTART_NOW - 10_000);
    let host_a = bench.live_host();
    let (svc, factory) = restart_service(&bench, 4201);
    svc.serve().await.expect("A 代服务启动");
    let keys = auth::RustSigningKey::generate();
    let (conn_a, _binding_id) =
        bind_and_deliver(&svc, &factory, &keys, &host_a, &bench.home_root()).await;

    bench.clear_hosts();
    let _ = svc.refresh_host_tracking().expect("退出跟踪拍");
    let record = read_record(&bench).expect("记录仍在盘上");
    assert_eq!(
        record["retired"],
        serde_json::Value::Bool(true),
        "宿主退出后记录必须停用：{record}"
    );
    assert!(
        !conn_a.write_available(),
        "退出必须失效旧代连接（旧代凭据句柄按 T09-G 合同保留槽位，但宿主不在场时授权入口在代际/消费点拒绝，此处观测连接与记录）"
    );

    // 幂等：再拍不复活记录、不再重复失效（invalidated = false）。
    let outcome = svc.refresh_host_tracking().expect("第二次跟踪拍");
    assert_eq!(
        outcome,
        auth::HostTracking::HostAbsent { invalidated: false },
        "无旧代可失效时必须如实报告：{outcome:?}"
    );
    let record = read_record(&bench).expect("记录仍在");
    assert_eq!(record["retired"], serde_json::Value::Bool(true));
}

/// Task 2 验收③（换钥反例）：换代后**新钥**不得借正常重连自动授权——仍走
/// TOFU 待批，等待用户批准。
#[tokio::test]
async fn changed_key_after_restart_still_needs_approval() {
    let bench = RestartBench::new();
    bench.set_host(4201, RESTART_NOW - 10_000);
    let host_a = bench.live_host();
    let (svc, factory) = restart_service(&bench, 4201);
    svc.serve().await.expect("A 代服务启动");
    let keys_a = auth::RustSigningKey::generate();
    let (_conn_a, _binding) =
        bind_and_deliver(&svc, &factory, &keys_a, &host_a, &bench.home_root()).await;

    bench.set_host(4202, RESTART_NOW - 5_000);
    let host_b = bench.live_host();
    let _ = svc.refresh_host_tracking().expect("换代跟踪拍");
    let keys_b = auth::RustSigningKey::generate();
    let conn = factory
        .connect_as(PeerOsIdentity {
            pid: 4202,
            uid: None,
        })
        .await;
    let reply = exchange(
        &conn,
        &hello_wire(svc.key_digest(), &keys_b, &host_b, 2, &bench.home_root()),
    )
    .await;
    assert_eq!(reply["type"], "server_hello", "{reply}");
    assert_eq!(
        reply["state"], "pending_approval",
        "换钥后的连接不是同钥重连：必须回到待批（不得自动授权）：{reply}"
    );
    assert!(reply["displayCode"].as_str().is_some(), "待批核对码在场");
}

/// Task 2 验收③（已撤权反例）：撤权后的同钥连接跨换代仍不得自动授权——服务端
/// 以签名决定回 `revoked`（插件据此落墓碑、停止重连）。
#[tokio::test]
async fn revoked_key_is_not_reauthorized_after_restart() {
    let bench = RestartBench::new();
    bench.set_host(4201, RESTART_NOW - 10_000);
    let host_a = bench.live_host();
    let (svc, factory) = restart_service(&bench, 4201);
    svc.serve().await.expect("A 代服务启动");
    let keys = auth::RustSigningKey::generate();
    let (_conn_a, binding_id) =
        bind_and_deliver(&svc, &factory, &keys, &host_a, &bench.home_root()).await;
    svc.revoke(&binding_id).await.expect("撤权");
    assert_eq!(svc.authorization_state(), AuthorizationState::Revoked);

    bench.set_host(4202, RESTART_NOW - 5_000);
    let host_b = bench.live_host();
    let _ = svc.refresh_host_tracking().expect("换代跟踪拍");
    let conn = factory
        .connect_as(PeerOsIdentity {
            pid: 4202,
            uid: None,
        })
        .await;
    let reply = exchange(
        &conn,
        &hello_wire(svc.key_digest(), &keys, &host_b, 2, &bench.home_root()),
    )
    .await;
    assert_eq!(reply["type"], "server_hello", "{reply}");
    assert_eq!(
        reply["state"], "revoked",
        "已撤权的钥匙换代后不得自动授权：{reply}"
    );
}

/// Task 2 验收③（错误归属反例）：换代重连的 hello 声明**别人的 home** → 拒绝，
/// 不得自动授权、不得改写归属账本。
#[tokio::test]
async fn foreign_home_is_not_auto_authorized_after_restart() {
    let bench = RestartBench::new();
    bench.set_host(4201, RESTART_NOW - 10_000);
    let host_a = bench.live_host();
    let (svc, factory) = restart_service(&bench, 4201);
    svc.serve().await.expect("A 代服务启动");
    let keys = auth::RustSigningKey::generate();
    let (_conn_a, _binding_id) =
        bind_and_deliver(&svc, &factory, &keys, &host_a, &bench.home_root()).await;
    let ledger = auth::instance_state_dir(&bench.env, &bench.instance_id)
        .unwrap()
        .join("bindings.json");
    let before = std::fs::read(&ledger).unwrap();

    bench.set_host(4202, RESTART_NOW - 5_000);
    let host_b = bench.live_host();
    let _ = svc.refresh_host_tracking().expect("换代跟踪拍");
    let conn = factory
        .connect_as(PeerOsIdentity {
            pid: 4202,
            uid: None,
        })
        .await;
    let foreign_home = bench.foreign_profile.parent().unwrap().to_path_buf();
    let reply = exchange(
        &conn,
        &hello_wire(svc.key_digest(), &keys, &host_b, 2, &foreign_home),
    )
    .await;
    assert_eq!(
        reply["type"], "error",
        "声明他人 home 的重连必须被拒：{reply}"
    );
    assert_eq!(
        std::fs::read(&ledger).unwrap(),
        before,
        "拒绝不得改写归属账本"
    );
}

/// Task 2 验收③（不同实例反例）：本域宿主退出后上场的是**另一实例**的宿主——
/// 本域记录只停用，绝不把别家代际发布成本域入口。
#[tokio::test]
async fn foreign_instance_host_never_publishes_into_our_domain() {
    let bench = RestartBench::new();
    bench.set_host(4201, RESTART_NOW - 10_000);
    let (svc, _factory) = restart_service(&bench, 4201);
    svc.serve().await.expect("A 代服务启动");
    assert!(read_record(&bench).is_some(), "A 代记录已发布");

    // A 退出；上场的是另一实例的宿主（不同 home → 不同实例 ID）。
    bench.set_foreign_host(4300, RESTART_NOW - 2_000);
    let outcome = svc.refresh_host_tracking().expect("跨实例跟踪拍");
    assert!(
        matches!(outcome, auth::HostTracking::HostAbsent { .. }),
        "别家宿主不是本域当代：{outcome:?}"
    );
    let record = read_record(&bench).expect("本域记录仍在盘上");
    assert_eq!(
        record["retired"],
        serde_json::Value::Bool(true),
        "本域旧代记录必须停用：{record}"
    );
    assert_ne!(
        record["processIdentity"]["pid"], 4300,
        "绝不能把别家实例发布成本域入口：{record}"
    );
}
