//! 第九轮 P2 升级形态切回的**控制台面**端到端回归：登记锚 + Bound 行 + 插件
//! 安装三证在场、但 `remote-switch.json` 从未物化（开关功能上线前已绑定+安装
//! 的升级用户）→ 模拟重启后切回该实例 → 监听按升级证据补起（生产钩子同款
//! 内核装配形状）→ 同钥钉公钥重连免批放行 → 网页凭据经真实交付链落挂载 →
//! 控制台入口可签发票据并兑换（真实 `router_with_static` + 生产挂载单例 +
//! 受控上游返回真实页面形态）。
//!
//! 与 `h09_console_mount_test` 的分工：h09 验证挂载/入口机器本身；本文件验证
//! **升级形态**下整条链（监听补位 → 凭据重交付 → 入口签发 → 兑换 → 页面）——
//! 监听缺席（缺陷形态）时凭据交付无从发生，页面通道死路。
//!
//! 全部经真实生产入口驱动：`DshAuthService` 实例域（真实 UDS + 原始帧客户端，
//! 项目标准接缝）+ `console_mount::production()`（测试注入）+ `console_lifecycle`
//! 凭据存储 + 真实 `RemoteSwitchService` 执行门（升级形态只读派生）。
//! 运行：cargo test --features dsh-test-probes --test dsh_switch_upgrade_rejoin_console_test
//! 夹具进程表/文件/时钟全部隔离在短临时目录；不碰真实 ~/.dsh、~/.tuvis。

#![cfg(all(unix, feature = "dsh-test-probes"))]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use app::remote::console_upstream::ConsoleUpstream;
use app::remote::pairing::{self, DeviceStore, NewDevice};
use app::remote::server::{self, CreateTaskHub, RemoteState, SseRegistry};
use app::services::dsh_control::auth::{
    self, BindingChallengeMessage, HomeBinding, RustSigningKey, CHALLENGE_TTL_MS,
};
use app::services::dsh_control::error::{codes, DshError};
use app::services::dsh_control::identity;
use app::services::dsh_control::install::{DshPluginManager, InstanceTarget};
use app::services::dsh_control::instances::{instance_id_for, HostKind, InstanceRegistry};
use app::services::dsh_control::model::{
    AuthorizationState, CommandRequest, DshClock, DshCommandRunner, DshEnvironment, DshFileSource,
    DshListener, DshProcess, DshProcessSource, InstallationState, RunOutcome,
    TestUnobservableEnvironments,
};
use app::services::dsh_control::production::{self, FsFileSource, InstanceBindingRevoker};
use app::services::dsh_control::switch::{self, RemoteSwitchService, SwitchServices};
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use http_body_util::BodyExt;
use multi_agents_manager_lib as app;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tower::ServiceExt;

const EXE: &str = "/dsh-upg-rejoin-fixture/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness";
const NOW: i64 = 1_760_000_000_000;
const DEVICE: &str = "upgrade-rejoin-device";
const WAIT: Duration = Duration::from_secs(10);
const PAGE_MARK: &str = "official-console-upgrade-rejoin-fixture";

/// 限根真实文件源（根外一概不可见；真实临时文件 + 真实内容——安装档
/// `plugin_presence` 与身份发现读的都是真实 manifest 形态）。
struct FixtureFiles(PathBuf);

impl DshFileSource for FixtureFiles {
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
    fn is_dir(&self, path: &Path) -> bool {
        path.is_dir()
    }
    fn list_dir(&self, path: &Path) -> Vec<PathBuf> {
        if !path.starts_with(&self.0) {
            return vec![];
        }
        std::fs::read_dir(path)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .collect()
            })
            .unwrap_or_default()
    }
    fn read_small(&self, path: &Path, max: u64) -> Result<Vec<u8>, DshError> {
        FsFileSource.read_small(path, max)
    }
}

struct OneHost {
    process: DshProcess,
    authority: String,
}
impl DshProcessSource for OneHost {
    fn list_processes(&self) -> Vec<DshProcess> {
        vec![self.process.clone()]
    }
    fn listening_sockets(&self) -> Vec<DshListener> {
        vec![DshListener {
            owner_pid: self.process.pid,
            authority: self.authority.clone(),
        }]
    }
}

struct Clock;
impl DshClock for Clock {
    fn now_ms(&self) -> i64 {
        NOW
    }
}

struct NoCommands;
impl DshCommandRunner for NoCommands {
    fn run(&self, _: &CommandRequest) -> Result<RunOutcome, DshError> {
        Err(DshError::new(codes::UNSUPPORTED, "fixture runner"))
    }
}

/// 受控上游：回真实页面形态（HTML），记录收到的 cookie（判据落在上游侧：
/// 页面请求必须携带**经握手交付链**安装的浏览器会话 cookie）。
async fn controlled_upstream() -> (String, Arc<Mutex<Vec<String>>>) {
    let observed: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&observed);
    let app = Router::new().fallback(move |headers: axum::http::HeaderMap| {
        let sink = Arc::clone(&sink);
        async move {
            sink.lock().unwrap().push(
                headers
                    .get(header::COOKIE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_owned(),
            );
            (
                [(header::CONTENT_TYPE, "text/html")],
                format!("<html>{PAGE_MARK}</html>"),
            )
                .into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let authority = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (authority, observed)
}

/// 原始帧读/写（真实 UDS 上的单行 JSON；dsh_ipc_late_console_credential 同款）。
async fn read_frame(reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>) -> Value {
    let mut line = String::new();
    tokio::time::timeout(WAIT, reader.read_line(&mut line))
        .await
        .expect("读帧超时")
        .expect("读帧失败");
    serde_json::from_str(line.trim()).expect("帧必须是合法 JSON")
}

async fn send_frame(writer: &mut tokio::net::unix::OwnedWriteHalf, frame: Value) {
    let mut line = frame.to_string();
    line.push('\n');
    tokio::time::timeout(WAIT, writer.write_all(line.as_bytes()))
        .await
        .expect("写帧超时")
        .expect("写帧失败");
    let _ = writer.flush().await;
}

/// 升级世界装配（三证在盘、开关记录从未物化）：
/// * 证一：盘面账本 Bound 行（旧构建迁移世界的写盘形态）；
/// * 证二：插件文件在场（真实 manifest + 包内容 → 管理器安装档 Installed）；
/// * 证三：登记锚（同钥摘要；登记行带核验 home 一并落账）；
/// * 缺席面：`remote-switch.json` 不存在。
struct UpgradeWorld {
    _temp: tempfile::TempDir,
    home: PathBuf,
    env: DshEnvironment,
    registry: Arc<InstanceRegistry>,
    instance: String,
    epoch: String,
    authority: String,
    manager: Arc<DshPluginManager>,
    /// 固定种子客户端钥（升级用户的插件钥；免批重连面）。
    keys: RustSigningKey,
}

fn build_upgrade_world(authority: String) -> UpgradeWorld {
    let temp = tempfile::Builder::new()
        .prefix("dsh-upg-rejoin-")
        .tempdir_in("/tmp")
        .unwrap();
    // macOS sun_path 限长：状态目录保短（目录本身真实落盘，文件源限根其中）。
    let root = temp.path().to_path_buf();
    let home = root.join("home");
    let profile = home.join("profiles/desktop");
    std::fs::create_dir_all(&profile).unwrap();
    let instance = instance_id_for(HostKind::Desktop, &profile);
    // 证二先行：插件文件在场（真实 manifest + 包内容——profile 门（官方标记 =
    // package.json）与发现面都读这些文件，必须先于 discover 落盘）。
    let name = app::services::dsh_control::install::PACKAGE_NAME;
    std::fs::write(
        profile.join("package.json"),
        format!(
            r#"{{"dependencies":{{"{name}":"file:cached.tgz"}},"dsh":{{"profile":{{"bundles":["{name}"]}}}}}}"#
        ),
    )
    .unwrap();
    let pkg_dir = profile.join("node_modules").join(name);
    std::fs::create_dir_all(&pkg_dir).unwrap();
    std::fs::write(
        pkg_dir.join("package.json"),
        format!(r#"{{"name":"{name}","version":"9.9.9-fixture"}}"#),
    )
    .unwrap();
    let pid = std::process::id();
    let env = DshEnvironment {
        home: home.clone(),
        tuvis_home: root.join("tuvis"),
        dsh_state_dir: Some(root.join("s")),
        processes: Arc::new(OneHost {
            process: DshProcess {
                pid,
                exe: EXE.into(),
                cmdline: vec![
                    EXE.into(),
                    "dsh-desktop-host/lib/index.js".into(),
                    profile.to_string_lossy().into_owned(),
                ],
                started_at_ms: NOW - 10_000,
            },
            authority: authority.clone(),
        }),
        files: Arc::new(FixtureFiles(root.clone())),
        runner: Arc::new(NoCommands),
        clock: Arc::new(Clock),
        environments: Arc::new(TestUnobservableEnvironments),
    };
    let host = identity::discover_instance(&env, &instance).expect("夹具宿主已核验");
    assert_eq!(host.authority, authority, "前置：发现面指向受控上游");

    // 证三：登记锚（升级用户的登记行带受信指纹；核验 home 同批登记）。
    let env_arc = Arc::new(env.clone());
    let registry = Arc::new(
        InstanceRegistry::open_with_env(root.join("instances.json"), Some(env_arc)).unwrap(),
    );
    let keys = RustSigningKey::from_seed(&[71u8; 32]);
    let instance_row = registry
        .register_candidate_with_home(HostKind::Desktop, &profile, Some(&home))
        .expect("候选登记");
    assert_eq!(instance_row.instance_id, instance, "前置：实例 ID 同源");
    registry.select(&instance).expect("选中升级实例");
    registry
        .register(HostKind::Desktop, &profile, &auth::key_digest(&keys.spki))
        .expect("登记锚落账");

    // 证一：盘面账本 Bound 行（旧构建直接写盘的迁移形态；当代握手/开关面从未
    // 触碰）。账本 wire 形态 = StoredBinding camelCase。
    let state_dir = auth::instance_state_dir(&env, &instance).expect("实例状态目录");
    std::fs::create_dir_all(&state_dir).unwrap();
    let ledger = json!({
        "bindings": [{
            "bindingId": "upgrade-world-binding",
            "publicKeySpki": auth::b64url_encode(&keys.spki),
            "revokedKeySpki": "",
            "executable": EXE,
            "pid": pid,
            "startedAtMs": NOW - 10_000,
            "profileDir": profile.to_string_lossy(),
            "createdAtMs": NOW - 20_000,
            "state": "bound",
        }]
    });
    std::fs::write(state_dir.join("bindings.json"), ledger.to_string()).unwrap();

    // 缺席面：开关记录从未物化（本批缺陷现场）。
    let record_path = state_dir.join("remote-switch.json");
    assert!(!record_path.exists(), "前置：升级形态无开关记录");

    // 安装档核对（证二经真实管理器判 Installed；不冒充）。
    let revoker = InstanceBindingRevoker::with_resolver(
        Arc::new(env.clone()),
        instance.clone(),
        Arc::new(|| -> Result<Arc<auth::DshAuthService>, DshError> {
            unreachable!("升级世界不撤权")
        }),
    );
    let manager = Arc::new(DshPluginManager::for_instance(
        env.clone(),
        Arc::new(revoker),
        InstanceTarget {
            instance_id: instance.clone(),
            kind: HostKind::Desktop,
            profile_dir: profile.clone(),
        },
    ));
    assert!(
        matches!(
            manager.status().expect("管理状态可读").installation,
            InstallationState::Installed
        ),
        "前置：插件安装档 Installed（升级用户证二）"
    );

    UpgradeWorld {
        _temp: temp,
        home,
        env,
        registry,
        instance,
        epoch: host.epoch.to_string(),
        authority,
        manager,
        keys,
    }
}

/// 设备配对 + cookie（h09 生产 gate 形态）。
fn pair_device(state: &Arc<RemoteState>) -> String {
    let device = NewDevice {
        id: DEVICE.to_owned(),
        name: "upgrade rejoin fixture".into(),
        ua: "upgrade-rejoin-test".into(),
        origin_ip: "127.0.0.1".into(),
        via: "lan".into(),
        paired_at: chrono::Utc::now().timestamp_millis(),
    };
    state
        .store
        .with(|conn| pairing::persist_device(conn, &device))
        .expect("内存设备库可写入");
    DEVICE.to_owned()
}

fn device_cookie(device: &str) -> String {
    let template = pairing::device_cookie(device);
    template.split(';').next().unwrap_or_default().to_string()
}

/// 一条归属升级实例的 DSH 会话（入口签发的「归属可证」面）。
fn one_dsh_session(instance: &str) -> app::session::SessionsResponse {
    use app::session::{AgentType, ProcessForm, Session, SessionStatus};
    app::session::SessionsResponse {
        sessions: vec![Session {
            id: "sess-upgrade".into(),
            agent_type: AgentType::Dsh,
            project_name: "upgrade-rejoin".into(),
            project_path: "/tmp/upgrade-rejoin".into(),
            title: None,
            git_branch: None,
            github_url: None,
            status: SessionStatus::Idle,
            last_message: None,
            last_message_role: None,
            last_message_subagent_report: false,
            flap_from_subagent_activity: false,
            last_activity_at: "2026-11-15T00:00:00Z".into(),
            pid: 43001,
            cpu_usage: 0.0,
            active_subagent_count: 0,
            form: ProcessForm::App,
            jump_supported: false,
            unread: false,
            unread_turned_green_at_ms: None,
            dsh_instance_id: Some(instance.to_owned()),
            dsh_subagent: None,
        }],
        total_count: 1,
        waiting_count: 0,
    }
}

struct NoInjector;
impl app::inject::engine::Injector for NoInjector {
    fn locate_and_inject(&self, _: u32, _: &str) -> Result<(), String> {
        panic!("本回归不允许注入终端")
    }
    fn locate_and_send_key_inner(&self, _: u32, _: &str) -> Result<(), String> {
        panic!("本回归不允许发送终端按键")
    }
}

/// h09 生产挂载构造形态的 RemoteState（入口/兑换端点同款装配）：会话源指向
/// 升级实例；挂载与上游解析注入本用例的合成值；开关执行门走**真实**
/// `RemoteSwitchService::execution_gate`（升级形态只读派生）。
fn remote_state_for(
    instance: &str,
    authority: &str,
    epoch: &str,
    mount: &Arc<app::remote::console_mount::ConsoleMount>,
    switch_service: &Arc<RemoteSwitchService>,
) -> Arc<RemoteState> {
    let store = DeviceStore::memory();
    let instance_for_source = instance.to_owned();
    let instance_for_fresh = instance.to_owned();
    let authority_for_source = authority.to_owned();
    let epoch_for_source = epoch.to_owned();
    let state = Arc::new(RemoteState {
        opencode_native: std::sync::Arc::new(
            multi_agents_manager_lib::services::opencode_native::NativeHub::new(
                multi_agents_manager_lib::remote::pairing::DeviceStore::memory(),
            ),
        ),
        session_source: Box::new(move || one_dsh_session(&instance_for_source)),
        fresh_session_source: Box::new(move || one_dsh_session(&instance_for_fresh)),
        pairing_counter: Box::new(Vec::new),
        board_hidden_ids: Box::new(Vec::new),
        board_hidden_hide: Arc::new(|_| panic!("不允许隐藏会话")),
        board_hidden_unhide: Arc::new(|_| panic!("不允许恢复会话")),
        unread_mark_read: Arc::new(|_, _, _| panic!("不允许修改未读状态")),
        session_close: Arc::new(|_| panic!("不允许关闭进程")),
        codex_control: Arc::new(
            app::remote::codex_ipc::CodexControlHub::disabled_with_store(store.clone()),
        ),
        store,
        host_source: Box::new(|| json!({"host": {"name": "upgrade-rejoin-fixture"}})),
        message_source: Box::new(|_, _, _| Err("本回归无真实内容源".into())),
        path_source: Box::new(|_, _, _| (vec![], false)),
        watcher_tx: tokio::sync::broadcast::channel(8).0,
        sse_registry: Arc::new(SseRegistry::default()),
        max_devices_source: Box::new(|| 3),
        pin_limiter: Mutex::new(app::remote::pin::PinRateLimiter::new()),
        global_pin_limiter: Mutex::new(app::remote::pin::PinRateLimiter::global()),
        pin_source: Box::new(|| None),
        now_source: Box::new(|| chrono::Utc::now().timestamp_millis()),
        via_hosts_source: Box::new(|| None),
        rate_bucket_channels_source: Box::new(Vec::new),
        injector: Arc::new(NoInjector),
        resume_spawner: Arc::new(|_| panic!("不允许启动真实终端")),
        archive_source: Box::new(Vec::new),
        archive_delete: Arc::new(|_| panic!("不允许删除归档")),
        confirm_probe: Arc::new(|_, _, _| panic!("不允许读取终端")),
        dialog_probe: Arc::new(|_, _| panic!("不允许探测真实对话框")),
        screen_probe: Arc::new(|_, _| panic!("不允许读取真实屏幕")),
        capability_table: app::inject::capability::new_table(),
        create_hub: Arc::new(CreateTaskHub::stub()),
        home_source: Box::new(|| None),
        ui_config_source: Box::new(|| None),
        subagent_source: Default::default(),
        subagent_message_source: Default::default(),
        dsh_control: app::services::dsh_control::disabled_arc(),
        dsh_auto_adapt_source: Arc::new(|| false),
        desktop_language: Default::default(),
        dsh_console_mount: Some(Arc::clone(mount)),
        dsh_console_upstream_source: Some(Arc::new(move |instance_id: &str| {
            Ok(ConsoleUpstream {
                instance_id: instance_id.to_owned(),
                authority: authority_for_source.clone(),
                epoch: epoch_for_source.parse().expect("epoch 必须是 u64"),
            })
        })),
    });
    // 开关执行门（控制台准入）：真实执行门 = 升级形态只读派生（本批既有面）。
    let gate_service = Arc::clone(switch_service);
    let gate_instance = instance.to_owned();
    state
        .dsh_control
        .set_switch_gate(Some(Arc::new(move |instance_id: &str| {
            if instance_id == gate_instance {
                gate_service.execution_gate()
            } else {
                Ok(())
            }
        })));
    state
}

async fn call(app: &Router, request: Request<Body>) -> Response {
    app.clone().oneshot(request).await.expect("路由可服务")
}

async fn body_text(response: Response) -> String {
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("响应体可读")
        .to_bytes();
    String::from_utf8_lossy(&bytes).to_string()
}

fn get_request(uri: &str, cookie: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().uri(uri).method("GET");
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    builder.body(Body::empty()).unwrap()
}

fn post_json(uri: &str, cookie: Option<&str>, body: Value) -> Request<Body> {
    let mut builder = Request::builder()
        .uri(uri)
        .method("POST")
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

/// 生产 select 钩子的装配形状（第九轮 P2 升级感知内核；夹具面——env 由本用例
/// 构造，绝不触 production_env 单例）。升级证据闭包 = 生产**只读源**
/// `upgrade_evidence_source` 的盘读事实 + 登记锚比对（与
/// `RemoteSwitchService::upgrade_recovery_evidence` 同判据；源零装配副作用
/// ——不创建密钥、不消费待落盘撤权标记）。
fn bringup_listener_after_select(
    switch_service: &Arc<RemoteSwitchService>,
    auth_service: &Arc<auth::DshAuthService>,
    facts_source: Arc<switch::SwitchEvidenceSource>,
    anchored: String,
) -> bool {
    let record_read = switch_service.read_raw();
    let evidence = move || {
        let facts = facts_source();
        !anchored.is_empty()
            && facts.bound_digest.as_deref() == Some(anchored.as_str())
            && !facts.bound_revocation_pending
            && facts.installed
    };
    production::ensure_listener_for_selected_switch(
        record_read,
        evidence,
        || Ok(Arc::clone(auth_service)),
        |auth| {
            tokio::spawn(async move {
                if let Err(e) = auth.serve().await {
                    log::warn!("dsh auth listener bring-up failed: {}", e.detail);
                }
            });
        },
    )
    .unwrap_or(false)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upgrade_form_rejoin_restores_listener_and_console_entry_redeems_to_real_page() {
    let (upstream_authority, observed_cookies) = controlled_upstream().await;
    let world = build_upgrade_world(upstream_authority.clone());

    // 生产挂载（进程单例，注入一次）：目标核验钉升级实例的当代三元组。
    let (wi, we, wa) = (
        world.instance.clone(),
        world.epoch.clone(),
        world.authority.clone(),
    );
    let mount = Arc::new(
        app::remote::console_mount::ConsoleMount::new()
            .with_device_check(|id: &str| id == DEVICE)
            .with_target_check(move |id: &str, e: u64, a: &str| {
                id == wi && e.to_string() == we && a == wa
            }),
    );
    assert!(
        app::remote::console_mount::init_production_mount_for_test(Arc::clone(&mount)).is_ok(),
        "必须在生产挂载首次使用之前注入"
    );

    // 重启模拟：实例域从未在本进程 serve（启动段只为「当时选中」的实例起监听）。
    // 开关域 = 真实 RemoteSwitchService（盘面记录缺席；services = 真实管理器 +
    // 本实例域）。
    let svc = Arc::new(
        production::init_instance_auth(&world.env, &world.instance)
            .expect("实例域构造")
            .without_liveness_watcher(),
    );
    let services: Arc<SwitchServices> = {
        let manager = Arc::clone(&world.manager);
        let svc = Arc::clone(&svc);
        Arc::new(move || Ok((Arc::clone(&manager), Arc::clone(&svc))))
    };
    // 升级证据只读源（生产同款；参数化注入夹具盘面——零装配副作用）。
    let evidence_source = production::upgrade_evidence_source(
        Arc::new(world.env.clone()),
        Arc::clone(&world.registry),
        world.instance.clone(),
    );
    let switch_service = Arc::new(
        RemoteSwitchService::new(
            Arc::new(world.env.clone()),
            Arc::clone(&world.registry),
            world.instance.clone(),
            services,
            Arc::clone(&evidence_source),
        )
        .expect("开关域构造"),
    );
    // 真实开关执行门接线（免批重连的授权提交点 + 控制台准入同一判定）。
    svc.set_switch_gate(switch::switch_authorization_gate(Arc::clone(
        &switch_service,
    )));
    assert_eq!(
        svc.authorization_state(),
        AuthorizationState::Bound,
        "前置：重启域按盘面装载 Bound 行（升级用户证一）"
    );
    let record_path = auth::instance_state_dir(&world.env, &world.instance)
        .unwrap()
        .join("remote-switch.json");
    assert!(!record_path.exists(), "前置：升级形态无开关记录");
    let anchored = world
        .registry
        .registered(&world.instance)
        .map(|row| row.identity_fingerprint)
        .unwrap_or_default();
    assert!(!anchored.is_empty(), "前置：升级用户登记锚在册（证三）");

    // —— 切回：生产 select 钩子装配形状补起监听（记录缺席 → 升级证据三证齐
    //    → 补起；本批修复面）。 ——
    assert!(
        bringup_listener_after_select(
            &switch_service,
            &svc,
            Arc::clone(&evidence_source),
            anchored,
        ),
        "升级证据三证齐必须补起监听"
    );

    // 同钥钉公钥重连（免批面：盘上 Bound 行 + 登记锚同钥，无需再绑定/再批准）。
    let rendezvous_path = auth::instance_state_dir(&world.env, &world.instance)
        .unwrap()
        .join("rendezvous.json");
    let deadline = std::time::Instant::now() + WAIT;
    let rendezvous: Value = loop {
        if let Ok(bytes) = std::fs::read(&rendezvous_path) {
            if let Ok(record) = serde_json::from_slice::<Value>(&bytes) {
                if !record["retired"].as_bool().unwrap_or(false) {
                    break record;
                }
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "等待当代 rendezvous 记录发布超时（监听补位未发生）"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let stream = tokio::time::timeout(
        WAIT,
        UnixStream::connect(rendezvous["address"].as_str().unwrap()),
    )
    .await
    .expect("UDS 连接超时（accept 循环缺席——升级形态缺陷现场）")
    .unwrap();
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    let host = identity::discover_instance(&world.env, &world.instance).unwrap();
    let challenge = BindingChallengeMessage {
        binding_protocol: auth::BINDING_PROTOCOL,
        key_digests: [svc.key_digest().into(), auth::key_digest(&world.keys.spki)],
        nonce: auth::b64url_encode(&[1; 32]),
        host_id: world.instance.clone(),
        epoch: world.epoch.clone(),
        expiry: NOW + CHALLENGE_TTL_MS,
        home_binding: HomeBinding {
            home_root: world.home.to_string_lossy().into_owned(),
            host_pid: host.pid,
            host_started_at_ms: host.started_at_ms,
        },
    };
    let hello = json!({"type":"hello", "bindingProtocol": auth::BINDING_PROTOCOL,
        "publicKeySpki":auth::b64url_encode(&world.keys.spki), "executable":EXE,
        "pid":host.pid, "startedAtMs":host.started_at_ms, "hostId":world.instance,
        "epoch":world.epoch, "nonce":challenge.nonce, "expiry":challenge.expiry,
        "homeBinding":challenge.home_binding,
        "signature":auth::b64url_encode(&world.keys.sign(&challenge.to_signing_bytes()))});
    send_frame(&mut writer, hello).await;
    let reply = read_frame(&mut reader).await;
    assert_eq!(reply["type"], "server_hello", "重连必须有应答: {reply}");
    assert_eq!(
        reply["state"], "authorized",
        "升级形态切回后同钥重连必须免批放行: {reply}"
    );
    let binding = reply["bindingId"].as_str().expect("免批必带 binding_id");

    // 网页凭据经真实交付链落 lifecycle + 生产挂载（双凭据：API + browser）。
    send_frame(
        &mut writer,
        json!({"type":"credential", "bindingId":binding,
            "cookie":"dsh-api=upgrade-rejoin", "browserCookie":"dsh-browser=upgrade-rejoin"}),
    )
    .await;
    let ack = read_frame(&mut reader).await;
    assert_eq!(
        ack["type"], "granted",
        "凭据交付必须被接受（credential_ack 为 granted 帧）: {ack}"
    );

    // —— 控制台面：入口签发票据（生产 router + 真实执行门）→ 兑换 → 页面。 ——
    let state = remote_state_for(
        &world.instance,
        &world.authority,
        &world.epoch,
        &mount,
        &switch_service,
    );
    pair_device(&state);
    let cookie = device_cookie(DEVICE);
    let management = server::router_with_static(Arc::clone(&state));

    let entry = call(
        &management,
        get_request(
            &format!(
                "/m/api/v1/dsh-console-entry?session_id=sess-upgrade&instanceId={}",
                world.instance
            ),
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(
        entry.status(),
        StatusCode::OK,
        "升级形态切回后控制台入口必须可签发票据"
    );
    let entry_body: Value =
        serde_json::from_str(&body_text(entry).await).expect("签发响应必须是 JSON");
    let ticket = entry_body["ticket"]
        .as_str()
        .expect("签发响应必须含 ticket（一次性引导票据）")
        .to_owned();
    assert!(
        entry_body.get("url").is_none(),
        "签发阶段不得直接给可用 URL"
    );

    let redeem = call(
        &management,
        post_json(
            "/m/api/v1/dsh-console-redeem",
            Some(&cookie),
            json!({ "ticket": ticket }),
        ),
    )
    .await;
    assert_eq!(redeem.status(), StatusCode::OK, "已配对设备必须可兑换票据");
    let redeem_body: Value =
        serde_json::from_str(&body_text(redeem).await).expect("兑换响应必须是 JSON");
    let session_url = redeem_body["url"]
        .as_str()
        .expect("兑换成功必须返回相对控制台 URL")
        .to_owned();
    assert!(
        session_url.starts_with("/console/"),
        "兑换出的必须是相对控制台路径: {session_url}"
    );

    // 页面通道：挂载凭据在位（握手交付链安装）→ 代理到受控上游的真实页面形态。
    let console_router =
        app::remote::console_mount::router(app::remote::console_mount::production().clone());
    let page = call(&console_router, get_request(&session_url, None)).await;
    assert_eq!(
        page.status(),
        StatusCode::OK,
        "升级形态切回后控制台页面必须可加载（监听补位 → 凭据在位）"
    );
    let body = body_text(page).await;
    assert!(
        body.contains(PAGE_MARK),
        "页面必须来自受控上游的真实页面形态: {body}"
    );
    let seen = observed_cookies.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec!["dsh-browser=upgrade-rejoin"],
        "页面请求必须携带经握手交付链安装的浏览器会话 cookie: {seen:?}"
    );
    // 票据一次性：同票据二次兑换必须失败。
    let replay = call(
        &management,
        post_json(
            "/m/api/v1/dsh-console-redeem",
            Some(&cookie),
            json!({ "ticket": ticket }),
        ),
    )
    .await;
    assert_eq!(replay.status(), StatusCode::FORBIDDEN, "票据一次性不得复用");
    drop(writer);
    drop(reader);
}
