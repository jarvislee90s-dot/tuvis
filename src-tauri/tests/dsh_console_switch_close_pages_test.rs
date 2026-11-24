//! P1-4（Round-7 Task 4）：切走 ≠ 撤权——`instance_select_core` 切换实例时对上一
//! 实例只做「关旧页面」（drain 活连接 + 取消），**保留**其网页凭据与能力，切回时
//! 控制台无需重交付即可直接打开；「撤销该实例授权」是独立的撤权链动作。
//!
//! 全部经真实生产入口驱动：`instance_select_core`（H10 命令核心，真实登记/选择/
//! 发现）+ `console_mount::production()`（测试注入）+ `console_lifecycle` 凭据
//! （经生产交付链同一安装入口写入）。
//!
//! 本文件单独编译为一个集成测试进程（生产挂载为进程级 OnceLock）。
//! 运行：cargo test --features dsh-test-probes --test dsh_console_switch_close_pages_test
//! 夹具进程表/profile/时钟全部隔离；不碰真实 ~/.dsh、~/.tuvis。

#![cfg(all(unix, feature = "dsh-test-probes"))]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use app::commands::dsh::{instance_select_core, DshInstanceRequest};
use app::remote::{console_lifecycle, console_mount};
use app::services::dsh_control::error::{codes, DshError};
use app::services::dsh_control::identity;
use app::services::dsh_control::instances::InstanceRegistry;
use app::services::dsh_control::model::{
    CommandRequest, DshClock, DshCommandRunner, DshEnvironment, DshFileSource, DshListener,
    DshProcess, DshProcessSource, RunOutcome,
};
use app::services::dsh_control::transport::PeerGate;
use axum::http::StatusCode;
use axum::Router;
use multi_agents_manager_lib as app;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(5);
const DEVICE: &str = "switch-close-fixture-device";

#[derive(Default)]
struct ProcessFacts {
    hosts: Vec<DshProcess>,
    listeners: Vec<DshListener>,
}
impl DshProcessSource for ProcessFacts {
    fn list_processes(&self) -> Vec<DshProcess> {
        self.hosts.clone()
    }
    fn listening_sockets(&self) -> Vec<DshListener> {
        self.listeners.clone()
    }
}
struct FixedClock;
impl DshClock for FixedClock {
    fn now_ms(&self) -> i64 {
        1_760_000_000_000
    }
}
struct NoCommands;
impl DshCommandRunner for NoCommands {
    fn run(&self, _: &CommandRequest) -> Result<RunOutcome, DshError> {
        panic!("候选/选择不得执行 CLI 或启停宿主")
    }
}

/// 真实文件源（h10 同款）：profile 目录与 package.json 落在临时目录内。
struct FsFileSource;

impl DshFileSource for FsFileSource {
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
    fn list_dir(&self, path: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(path)
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok())
                    .map(|entry| entry.path())
                    .collect()
            })
            .unwrap_or_default()
    }
    fn read_small(&self, path: &Path, max_bytes: u64) -> Result<Vec<u8>, DshError> {
        use std::io::Read;
        let mut file = std::fs::File::open(path)
            .map_err(|_| DshError::new(codes::IPC_STATE, "fixture file unreadable"))?;
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer)
            .map_err(|_| DshError::new(codes::IPC_STATE, "fixture file unreadable"))?;
        if buffer.len() as u64 > max_bytes {
            return Err(DshError::new(codes::IPC_STATE, "fixture file too large"));
        }
        Ok(buffer)
    }
    fn read_link_target(&self, _: &Path) -> Option<PathBuf> {
        None
    }
    fn is_readable_regular_file(&self, path: &Path) -> bool {
        path.is_file()
    }
}

fn seed_profile(root: &Path, home_name: &str) -> PathBuf {
    let profile = root.join(home_name).join("profiles/desktop");
    std::fs::create_dir_all(&profile).unwrap();
    std::fs::write(
        profile.join("package.json"),
        br#"{"name":"dsh-profile-desktop","dependencies":{},"dsh":{"profile":{"bundles":[]}}}"#,
    )
    .unwrap();
    std::fs::canonicalize(profile).unwrap()
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

#[path = "support/dsh_console.rs"]
mod dsh_console;
use dsh_console::{get_console, session_capability};

fn request(id: &str) -> DshInstanceRequest {
    serde_json::from_value(serde_json::json!({ "instanceId": id })).expect("稳定 ID 请求可解析")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn switching_away_closes_previous_pages_but_keeps_credentials_for_switch_back() {
    // 真实上游（A）：控制台转发确实出站并返回 200。
    let upstream = Router::new().fallback(|| async { (StatusCode::OK, "fixture console a") });
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let authority_a = upstream_listener.local_addr().unwrap().to_string();
    let upstream_task =
        tokio::spawn(async move { axum::serve(upstream_listener, upstream).await.unwrap() });
    let spare = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let authority_b = spare.local_addr().unwrap().to_string();
    drop(spare); // B 只需一个可发现 authority；本用例不向 B 出站

    #[cfg(unix)]
    let root = tempfile::Builder::new()
        .prefix("dsh-switch-")
        .tempdir_in("/tmp")
        .unwrap();
    let profile_a = seed_profile(root.path(), "home-a");
    let profile_b = seed_profile(root.path(), "home-b");
    let instance_a = instance_id_for(&profile_a);
    let instance_b = instance_id_for(&profile_b);
    let facts = Arc::new(ProcessFacts {
        hosts: vec![
            desktop_process(43001, &profile_a, 1_000),
            desktop_process(43002, &profile_b, 2_000),
        ],
        listeners: vec![
            DshListener {
                owner_pid: 43001,
                authority: authority_a.clone(),
            },
            DshListener {
                owner_pid: 43002,
                authority: authority_b.clone(),
            },
        ],
    });
    let env = Arc::new(DshEnvironment {
        home: profile_a.parent().unwrap().parent().unwrap().to_path_buf(),
        tuvis_home: root.path().join("tuvis"),
        dsh_state_dir: Some(root.path().join("s")),
        processes: facts.clone(),
        files: Arc::new(FsFileSource),
        runner: Arc::new(NoCommands),
        clock: Arc::new(FixedClock),
        environments: Arc::new(app::services::dsh_control::model::TestUnobservableEnvironments),
    });
    let registry_path = root.path().join("s/instances.json");
    let registry = InstanceRegistry::open_with_env(&registry_path, Some(Arc::clone(&env))).unwrap();

    let host_a = identity::discover_instance(&env, &instance_a).unwrap();
    let host_b = identity::discover_instance(&env, &instance_b).unwrap();
    let (ia, ea, aa) = (instance_a.clone(), host_a.epoch, host_a.authority.clone());
    let (ib, eb, ab) = (instance_b.clone(), host_b.epoch, host_b.authority.clone());
    let mount = Arc::new(
        console_mount::ConsoleMount::new()
            .with_device_check(|id| id == DEVICE)
            .with_target_check(move |id, e, a| {
                (id == ia && e == ea && a == aa) || (id == ib && e == eb && a == ab)
            }),
    );
    assert!(
        console_mount::init_production_mount_for_test(Arc::clone(&mount)).is_ok(),
        "必须在生产挂载首次使用之前注入"
    );

    // A 的网页凭据经生产交付链同一安装入口写入（api + browser）。
    console_lifecycle::install_upstream_credentials_with_gate_at_sequence(
        &instance_a,
        "dsh-api=fixture-a".into(),
        "dsh-browser=fixture-a".into(),
        host_a.epoch,
        &host_a.authority,
        PeerGate::new(),
        mount.revocation_sequence(),
    )
    .expect("A 凭据安装");

    // 先选 A：登记 + 控制台会话在场。
    instance_select_core(&env, &registry, request(&instance_a), &|_| {}).expect("选择 A 受理");
    let router = console_mount::router(Arc::clone(console_mount::production()));
    mount
        .register_if_current(
            session_capability(
                DEVICE,
                &mount,
                "tok-a-live",
                &instance_a,
                host_a.epoch,
                &authority_a,
            ),
            mount.revocation_sequence(),
        )
        .expect("A 会话能力登记");
    let (_, mut watch_a) = mount
        .authorize_live_for_test("tok-a-live")
        .expect("A 活连接登记");
    assert_eq!(
        get_console(WAIT, &router, "tok-a-live").await,
        StatusCode::OK
    );

    // ===== 生产选择入口：切到 B（上一实例 = A 只关旧页，不撤授权）=====
    instance_select_core(&env, &registry, request(&instance_b), &|_| {}).expect("切到 B 受理");

    // 关旧页面：A 的活连接必须被取消（页面关闭）。
    assert!(
        *watch_a.borrow_and_update(),
        "切走必须关闭上一实例的活控制台页面"
    );

    // 凭据保留（切回零重交付的前提）。
    assert!(
        console_lifecycle::upstream_credential(&instance_a).is_some(),
        "切走不得清掉上一实例的 API 凭据"
    );
    assert!(
        console_lifecycle::upstream_browser_cookie_for(&instance_a, host_a.epoch, &authority_a)
            .is_some(),
        "切走不得清掉上一实例的浏览器会话凭据"
    );
    // RED 断言（当前实现 revoke_instance 连凭据带能力一起删 → 两条必红）：
    assert!(
        mount
            .register_if_current(
                session_capability(
                    DEVICE,
                    &mount,
                    "tok-a-return",
                    &instance_a,
                    host_a.epoch,
                    &authority_a
                ),
                mount.revocation_sequence(),
            )
            .is_ok(),
        "切回 A 时控制台入口必须凭保留凭据直接登记（无需重交付）"
    );
    assert_eq!(
        get_console(WAIT, &router, "tok-a-return").await,
        StatusCode::OK,
        "切回 A 的控制台必须可用"
    );

    upstream_task.abort();
}

/// instance_id_for 的本地薄封装（Desktop 形态；与 dsh_ipc_late_console_credential_test 同一口径）。
fn instance_id_for(profile: &Path) -> String {
    app::services::dsh_control::instances::instance_id_for(
        app::services::dsh_control::instances::HostKind::Desktop,
        profile,
    )
}
