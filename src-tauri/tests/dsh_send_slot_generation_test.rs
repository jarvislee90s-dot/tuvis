//! 共享原生连接槽位等待时，实例切走再切回应使已进入的旧请求失效。

#![cfg(all(unix, feature = "dsh-test-probes"))]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use app::commands::dsh::{instance_select_core, DshInstanceRequest};
use app::services::dsh_control::error::DshError;
use app::services::dsh_control::identity;
use app::services::dsh_control::instances::InstanceRegistry;
use app::services::dsh_control::model::{
    CommandRequest, DshClock, DshCommandRunner, DshEnvironment, DshListener, DshProcess,
    DshProcessSource, RunOutcome,
};
use axum::Router;
use multi_agents_manager_lib as app;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(5);

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
fn request(id: &str) -> DshInstanceRequest {
    serde_json::from_value(serde_json::json!({ "instanceId": id })).expect("稳定 ID 请求可解析")
}
/// instance_id_for 的本地薄封装（Desktop 形态；与 dsh_ipc_late_console_credential_test 同一口径）。
fn instance_id_for(profile: &Path) -> String {
    app::services::dsh_control::instances::instance_id_for(
        app::services::dsh_control::instances::HostKind::Desktop,
        profile,
    )
}

use app::services::dsh_control::actions::PeerChannel;
use app::services::dsh_control::http1::unary_pause::{self, Stage};
use app::services::dsh_control::model::{AuthorizedPeer, Target};
use app::services::dsh_control::production::{self, FsFileSource};
use app::services::dsh_control::transport::UnaryFailure;
use std::sync::Mutex;
use tokio::sync::Notify;

// 实例选择镜像和阶段探针均为测试进程级状态。
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Fixture {
    _root: tempfile::TempDir,
    _other_listener: tokio::net::TcpListener,
    env: Arc<DshEnvironment>,
    registry: Arc<InstanceRegistry>,
    instance_b: String,
    channel: Arc<PeerChannel>,
    target: Target,
    observed: Arc<Mutex<Vec<String>>>,
    first_arrived: Arc<Notify>,
    release_first: Arc<Notify>,
    server: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn new() -> Self {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let first_arrived = Arc::new(Notify::new());
        let release_first = Arc::new(Notify::new());
        let upstream = Router::new()
            .route(
                "/api/remote.mux",
                axum::routing::get(|ws: axum::extract::ws::WebSocketUpgrade| async {
                    ws.on_upgrade(
                        |mut socket| async move { while socket.recv().await.is_some() {} },
                    )
                }),
            )
            .route(
                "/api/{*path}",
                axum::routing::post({
                    let observed = observed.clone();
                    let first_arrived = first_arrived.clone();
                    let release_first = release_first.clone();
                    move |axum::Json(body): axum::Json<serde_json::Value>| {
                        let observed = observed.clone();
                        let first_arrived = first_arrived.clone();
                        let release_first = release_first.clone();
                        async move {
                            let id = body["rpcId"].as_str().expect("RPC ID");
                            observed.lock().unwrap().push(id.to_owned());
                            if id == "first" {
                                first_arrived.notify_one();
                                release_first.notified().await;
                            }
                            axum::Json(serde_json::json!({
                                "type": "server-response", "rpcId": id,
                                "result": {"ok": true, "value": {"accepted": true}}
                            }))
                        }
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let authority = listener.local_addr().unwrap().to_string();
        let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
        let other_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();

        let root = tempfile::Builder::new()
            .prefix("dsh-slot-")
            .tempdir_in("/tmp")
            .unwrap();
        let profile_a = seed_profile(root.path(), "home-a");
        let profile_b = seed_profile(root.path(), "home-b");
        let instance_a = instance_id_for(&profile_a);
        let instance_b = instance_id_for(&profile_b);
        let env = Arc::new(DshEnvironment {
            home: root.path().join("home-a"),
            tuvis_home: root.path().join("tuvis"),
            dsh_state_dir: Some(root.path().join("s")),
            processes: Arc::new(ProcessFacts {
                hosts: vec![
                    desktop_process(43001, &profile_a, 1_000),
                    desktop_process(43002, &profile_b, 2_000),
                ],
                listeners: vec![
                    DshListener {
                        owner_pid: 43001,
                        authority: authority.clone(),
                    },
                    DshListener {
                        owner_pid: 43002,
                        authority: other_listener.local_addr().unwrap().to_string(),
                    },
                ],
            }),
            files: Arc::new(FsFileSource),
            runner: Arc::new(NoCommands),
            clock: Arc::new(FixedClock),
            environments: Arc::new(app::services::dsh_control::model::TestUnobservableEnvironments),
        });
        let registry = InstanceRegistry::open_with_env(
            root.path().join("s/instances.json"),
            Some(env.clone()),
        )
        .unwrap();
        instance_select_core(&env, &registry, request(&instance_a), &|_| {}).unwrap();
        let epoch = identity::discover_instance(&env, &instance_a)
            .unwrap()
            .epoch;
        let channel = PeerChannel::from_static_peer(AuthorizedPeer::with_authority(
            &instance_a,
            b"fixture=synthetic".to_vec(),
            &authority,
            epoch,
        ));
        let target = Target {
            host_id: instance_a.clone(),
            epoch,
            session_id: "slot-session".into(),
        };
        Self {
            _root: root,
            _other_listener: other_listener,
            env,
            registry: Arc::new(registry),
            instance_b,
            channel,
            target,
            observed,
            first_arrived,
            release_first,
            server,
        }
    }

    fn switch_away_and_back(&self) -> tokio::task::JoinHandle<Result<(), String>> {
        let return_to = self.target.host_id.clone();
        let instance_b = self.instance_b.clone();
        let env = self.env.clone();
        let registry = self.registry.clone();
        tokio::task::spawn_blocking(move || {
            instance_select_core(&env, &registry, request(&instance_b), &|_| {})?;
            instance_select_core(&env, &registry, request(&return_to), &|_| {})
        })
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

#[tokio::test]
async fn switching_away_and_back_invalidates_a_send_waiting_for_the_connection_slot() {
    let _serial = SERIAL.lock().await;
    let fixture = Fixture::new().await;
    let channel = &fixture.channel;
    let target = &fixture.target;
    let instance_a = &target.host_id;
    let generation = production::instance_generation(instance_a);
    let args = serde_json::json!({});
    let first = tokio::spawn({
        let channel = channel.clone();
        let target = target.clone();
        let args = args.clone();
        async move {
            channel
                .request_classified("first", "session/prompt", &args, Some(&target))
                .await
        }
    });
    timeout(WAIT, fixture.first_arrived.notified())
        .await
        .expect("首个 POST 必须到达宿主");

    let stale = channel.request_classified("stale", "session/prompt", &args, Some(target));
    tokio::pin!(stale);
    // 首个响应仍扣住，biased 先 poll 旧动作。唯一能挂起它的是同一连接槽位。
    tokio::select! {
        biased;
        _ = &mut stale => panic!("旧动作必须在共享槽位等待"),
        _ = std::future::ready(()) => {}
    }
    assert!(!first.is_finished(), "首个响应须持续扣住槽位");
    let mut switched = fixture.switch_away_and_back();
    let selected = timeout(WAIT, &mut switched).await;
    if selected.is_err() {
        fixture.release_first.notify_one();
        let _ = timeout(WAIT, switched).await;
    }
    selected
        .expect("首个响应扣住时实例切换必须能完成")
        .unwrap()
        .unwrap();
    assert_eq!(
        production::selected_mirror().as_deref(),
        Some(instance_a.as_str())
    );
    assert_ne!(production::instance_generation(instance_a), generation);
    assert_eq!(fixture.observed.lock().unwrap().as_slice(), ["first"]);

    fixture.release_first.notify_one();
    timeout(WAIT, first)
        .await
        .unwrap()
        .unwrap()
        .unwrap()
        .unwrap();
    let stale_result = timeout(WAIT, &mut stale)
        .await
        .expect("旧动作须退出槽位等待");
    timeout(
        WAIT,
        channel.request_classified("fresh", "session/prompt", &args, Some(target)),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap();

    assert_eq!(
        fixture.observed.lock().unwrap().as_slice(),
        ["first", "fresh"],
        "切回 A 后宿主收到的旧 POST 应为零次，新 POST 应恰为一次"
    );
    assert!(
        matches!(stale_result, Err(_) | Ok(Err(UnaryFailure::NotExecuted(_)))),
        "旧动作须返回明确未执行"
    );
}

#[tokio::test]
async fn switching_away_and_back_invalidates_a_send_before_unary_connect() {
    assert_stale_at_unary_stage(Stage::BeforeConnect).await;
}

#[tokio::test]
async fn switching_away_and_back_prevents_completion_of_a_partial_unary_write() {
    assert_stale_at_unary_stage(Stage::AfterPartialWrite).await;
}

async fn assert_stale_at_unary_stage(stage: Stage) {
    let _serial = SERIAL.lock().await;
    let fixture = Fixture::new().await;
    let generation = production::instance_generation(&fixture.target.host_id);
    let allow_unknown = matches!(stage, Stage::AfterPartialWrite);
    unary_pause::arm("stale", stage);
    let stale = tokio::spawn({
        let channel = fixture.channel.clone();
        let target = fixture.target.clone();
        async move {
            channel
                .request_classified(
                    "stale",
                    "session/prompt",
                    &serde_json::json!({}),
                    Some(&target),
                )
                .await
        }
    });

    let reached = timeout(WAIT, async {
        while !unary_pause::reached() && !stale.is_finished() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        unary_pause::reached()
    })
    .await;
    let mut switched = fixture.switch_away_and_back();
    let selected = timeout(WAIT, &mut switched).await;
    unary_pause::release();
    if selected.is_err() {
        let _ = timeout(WAIT, switched).await;
    }
    let stale_result = timeout(WAIT, stale)
        .await
        .expect("旧动作须退出写入阶段")
        .unwrap();
    assert!(
        matches!(reached, Ok(true)),
        "旧动作须到达指定的一元 HTTP 阶段"
    );
    selected.expect("停驻期间 A→B→A 须完成").unwrap().unwrap();
    assert_ne!(
        production::instance_generation(&fixture.target.host_id),
        generation
    );

    timeout(
        WAIT,
        fixture.channel.request_classified(
            "fresh",
            "session/prompt",
            &serde_json::json!({}),
            Some(&fixture.target),
        ),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap();
    assert_eq!(
        fixture.observed.lock().unwrap().as_slice(),
        ["fresh"],
        "宿主须收到零个完整旧请求和恰一个完整新请求"
    );
    match stale_result {
        Err(_) | Ok(Err(UnaryFailure::NotExecuted(_))) => {}
        Ok(Err(UnaryFailure::DeliveryUnknown(_))) if allow_unknown => {}
        _ => panic!("旧动作应被拒绝；只有部分写阶段允许投递结果未知"),
    }
}
