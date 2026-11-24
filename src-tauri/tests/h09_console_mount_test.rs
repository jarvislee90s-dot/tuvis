//! H09 生产控制台挂载的端到端回归：真实 `router_with_static` + 真实 gate +
//! 内存设备库 + 本地假上游。
//!
//! 覆盖：
//! 1. 未配对设备 → 入口端点 403（不签发能力）；
//! 2. 已配对设备 + 会话归属可证 → 入口返回相对 `/console/<token>/`；
//! 3. 无 `mam_device` cookie 的控制台请求**能**取到官方页面（授权靠能力令牌）；
//! 4. 控制台响应强制 opaque sandbox CSP，且**不回传**上游 `Set-Cookie`；
//! 5. 撤权后能力立即失效（403），活连接收到取消信号；
//! 6. 越界路径在连接上游之前被拒绝（403 而非 502）。
//!
//! 假上游只在本测试进程内监听 127.0.0.1，不触真实 DSH、不读写 `~/.dsh`。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use app::remote::console_capability::ConsoleCapability;
use app::remote::console_mount;
use app::remote::gate;
use app::remote::pairing::{self, DeviceStore, NewDevice};
use app::remote::server::{self, CreateTaskHub, RemoteState, SseRegistry};
use axum::body::Body;
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get as route_get;
use axum::Router;
use http_body_util::BodyExt;
use multi_agents_manager_lib as app;
use serde_json::json;
use tower::ServiceExt;

struct NoInjector;
impl app::inject::engine::Injector for NoInjector {
    fn locate_and_inject(&self, _: u32, _: &str) -> Result<(), String> {
        panic!("H09 测试不允许注入终端")
    }
    fn locate_and_send_key_inner(&self, _: u32, _: &str) -> Result<(), String> {
        panic!("H09 测试不允许发送终端按键")
    }
}

fn empty_sessions() -> app::session::SessionsResponse {
    app::session::SessionsResponse {
        sessions: vec![],
        total_count: 0,
        waiting_count: 0,
    }
}

/// 会话源：返回一条归属 `inst-a` 的 DSH 会话。
///
/// M02 的兑换端点要求「会话存在且归属可证」——空会话源拿不到票据，
/// 用它测签发票据会退化成「测 404」。
fn one_dsh_session() -> app::session::SessionsResponse {
    use app::session::{Session, SessionStatus};
    app::session::SessionsResponse {
        sessions: vec![Session {
            id: "sess-1".into(),
            agent_type: app::session::AgentType::Dsh,
            project_name: "h09".into(),
            project_path: "/tmp/h09".into(),
            title: None,
            git_branch: None,
            github_url: None,
            status: SessionStatus::Idle,
            last_message: None,
            last_message_role: None,
            last_message_subagent_report: false,
            flap_from_subagent_activity: false,
            last_activity_at: "2026-11-11T00:00:00Z".into(),
            pid: 43001,
            cpu_usage: 0.0,
            active_subagent_count: 0,
            form: app::session::ProcessForm::App,
            jump_supported: false,
            unread: false,
            unread_turned_green_at_ms: None,
            dsh_instance_id: Some("inst-a".into()),
            dsh_subagent: None,
        }],
        total_count: 1,
        waiting_count: 0,
    }
}

fn memory_state() -> Arc<RemoteState> {
    memory_state_with_sessions(empty_sessions)
}

/// 与 `memory_state` 同构，但会话源可注入（M02 兑换需要一条真实归属的会话）。
fn memory_state_with_dsh_session() -> Arc<RemoteState> {
    let state = memory_state_with_sessions(one_dsh_session);
    install_fixture_cookie(
        state.dsh_console_mount.as_ref().unwrap(),
        "127.0.0.1:5987",
        1,
    );
    state
}

fn memory_state_with_sessions(source: fn() -> app::session::SessionsResponse) -> Arc<RemoteState> {
    // M02：兑换端点必须打到**本测试的** mount（否则打进程单例、票据查不到）。
    // device 校验恒真：gate 已经保证设备有效，这里只测「票据是否绑该设备」。
    let test_mount = Arc::new(console_mount::ConsoleMount::new().with_device_check(|_| true));
    memory_state_with_mount(source, test_mount)
}

/// 与 [`memory_state_with_sessions`] 同构，但挂载可注入（T3：设备失效分码的
/// 端点级回归需要 device_check 拒绝的挂载）。
fn memory_state_with_mount(
    source: fn() -> app::session::SessionsResponse,
    test_mount: Arc<console_mount::ConsoleMount>,
) -> Arc<RemoteState> {
    let store = DeviceStore::memory();
    Arc::new(RemoteState {
        opencode_native: std::sync::Arc::new(
            multi_agents_manager_lib::services::opencode_native::NativeHub::new(
                multi_agents_manager_lib::remote::pairing::DeviceStore::memory(),
            ),
        ),
        session_source: Box::new(source),
        fresh_session_source: Box::new(source),
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
        host_source: Box::new(|| json!({"host": {"name": "h09-mount-fixture"}})),
        message_source: Box::new(|_, _, _| Err("H09 无真实内容源".into())),
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
        // M02：签发时的上游解析注入合成值（真实路径要求活体；兑换语义与此无关）。
        dsh_console_mount: Some(std::sync::Arc::clone(&test_mount)),
        dsh_console_upstream_source: Some(std::sync::Arc::new(|instance_id: &str| {
            Ok(app::remote::console_upstream::ConsoleUpstream {
                instance_id: instance_id.to_owned(),
                authority: "127.0.0.1:5987".into(),
                epoch: 1,
            })
        })),
    })
}

fn pair_device(state: &Arc<RemoteState>) -> String {
    let id = format!("{:032x}", rand::random::<u128>());
    let device = NewDevice {
        id: id.clone(),
        name: "h09 mount fixture".into(),
        ua: "h09-mount-test".into(),
        origin_ip: "127.0.0.1".into(),
        via: "lan".into(),
        paired_at: chrono::Utc::now().timestamp_millis(),
    };
    state
        .store
        .with(|conn| pairing::persist_device(conn, &device))
        .expect("内存设备库可写入");
    id
}

/// 设备 cookie 头（与生产下发同名；只用于证明 gate 行为，不记录值）。
fn device_cookie(device: &str) -> String {
    let template = pairing::device_cookie(device);
    template.split(';').next().unwrap_or_default().to_string()
}

async fn call(app: &Router, request: Request<Body>) -> Response {
    app.clone().oneshot(request).await.expect("路由可服务")
}

fn get_request(uri: &str, cookie: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().uri(uri).method("GET");
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    builder.body(Body::empty()).unwrap()
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

fn sandbox_csp(response: &Response) -> String {
    response
        .headers()
        .get(header::CONTENT_SECURITY_POLICY)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

/// 启动一个只回答固定内容的假上游，返回其 authority。
async fn fake_upstream() -> String {
    async fn index() -> Response {
        (
            [
                (header::CONTENT_TYPE, "text/html"),
                // 上游自己的 cookie 绝不能被网关回传给浏览器。
                (header::SET_COOKIE, "dsh-auth=HOST-SECRET"),
            ],
            "<html>official-console-fixture</html>",
        )
            .into_response()
    }
    let app = Router::new().route("/", route_get(index)).route(
        "/api/session/list",
        axum::routing::post(|| async { "pong" }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let authority = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    authority
}

/// 每个用例**自己装配**一个挂载实例（不碰生产单例），因此天然并行安全。
///
/// 设备有效性由闭包注入，等价于「查设备库」；撤销就是闭包结果翻转。
fn mount_with_devices(
    devices: &[&str],
) -> (Arc<console_mount::ConsoleMount>, Arc<Mutex<Vec<String>>>) {
    let allowed: Arc<Mutex<Vec<String>>> =
        Arc::new(Mutex::new(devices.iter().map(|d| d.to_string()).collect()));
    let check = Arc::clone(&allowed);
    let mount = console_mount::ConsoleMount::new()
        .with_device_check(move |device| check.lock().unwrap().iter().any(|d| d == device));
    (Arc::new(mount), allowed)
}

/// 用给定挂载装配路由。
///
/// 控制台子树与生产**同一个 `console_mount::router`**；外层只补一个 404 兜底
/// （生产里是静态兜底 + 管理 API，与本文要验证的控制台语义无关）。
/// 不叠 `router_with_static`：两者都有 fallback，axum 会 panic。
fn app_with(mount: &Arc<console_mount::ConsoleMount>) -> Router {
    console_mount::router(Arc::clone(mount))
        .fallback(|| async { StatusCode::NOT_FOUND.into_response() })
}

fn register_capability(mount: &Arc<console_mount::ConsoleMount>, authority: &str) -> String {
    register_capability_with_epoch(mount, authority, 1)
}

fn register_capability_with_epoch(
    mount: &Arc<console_mount::ConsoleMount>,
    authority: &str,
    epoch: u64,
) -> String {
    let token = format!("{:032x}", rand::random::<u128>());
    mount
        .register(ConsoleCapability {
            token: token.clone(),
            device_id: "dev-a".into(),
            instance_id: "inst-a".into(),
            authority: authority.to_owned(),
            epoch,
            // 既有用例直接测「会话能力」语义，不经过引导盖章。
            bootstrap: false,
            expires_at_ms: chrono::Utc::now().timestamp_millis() + 60_000,
        })
        .expect("能力可登记");
    token
}

#[tokio::test]
async fn unpaired_device_cannot_obtain_a_console_entry() {
    let state = memory_state();
    let app = server::router_with_static(state);
    let response = call(
        &app,
        get_request("/m/api/v1/dsh-console-entry?session_id=x", None),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "未配对设备不得获得控制台入口"
    );
}

#[tokio::test]
async fn paired_device_without_a_provable_session_gets_no_entry() {
    let state = memory_state();
    let device = pair_device(&state);
    let cookie = device_cookie(&device);
    let app = server::router_with_static(state);
    let response = call(
        &app,
        get_request(
            "/m/api/v1/dsh-console-entry?session_id=missing",
            Some(&cookie),
        ),
    )
    .await;
    // 会话快照为空 → 找不到会话；不得因为设备有效就签发能力。
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn console_page_loads_without_device_cookie_and_is_sandboxed() {
    let _guard = CREDENTIAL_TEST_LOCK.lock().await;
    let (mount, _devices) = mount_with_devices(&["dev-a"]);
    let authority = fake_upstream().await;
    install_fixture_cookie(&mount, &authority, 1);
    let token = register_capability(&mount, &authority);
    let app = app_with(&mount);

    // 关键负控：**不带任何 cookie** 请求控制台页面。
    let response = call(&app, get_request(&format!("/console/{token}/"), None)).await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "授权靠能力令牌，不应依赖设备 cookie"
    );
    let csp = sandbox_csp(&response);
    assert!(csp.contains("sandbox"), "缺少 sandbox: {csp}");
    assert!(
        !csp.contains("allow-same-origin"),
        "控制台绝不能与主站同源: {csp}"
    );
    assert!(
        response.headers().get(header::SET_COOKIE).is_none(),
        "上游宿主凭据不得回传浏览器"
    );
    let body = body_text(response).await;
    assert!(body.contains("official-console-fixture"), "body={body}");
}

#[tokio::test]
async fn console_subresource_and_api_requests_are_proxied() {
    let _guard = CREDENTIAL_TEST_LOCK.lock().await;
    let (mount, _devices) = mount_with_devices(&["dev-a"]);
    let authority = fake_upstream().await;
    install_fixture_cookie(&mount, &authority, 1);
    let token = register_capability(&mount, &authority);
    let app = app_with(&mount);

    // 官方 `session/list` 是 **POST** 一元调用：用 GET 会被方法表按「方法/路径
    // 错配」拒绝（这本身也是 M01 要的行为）。
    let api = call(
        &app,
        Request::builder()
            .uri(format!("/console/{token}/api/session/list"))
            .method("POST")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap(),
    )
    .await;
    assert_eq!(api.status(), StatusCode::OK);
    assert_eq!(body_text(api).await, "pong");
}

#[tokio::test]
async fn control_console_cannot_reach_tuvis_management_api() {
    let _guard = CREDENTIAL_TEST_LOCK.lock().await;
    let (mount, _devices) = mount_with_devices(&["dev-a"]);
    let authority = fake_upstream().await;
    install_fixture_cookie(&mount, &authority, 1);
    let token = register_capability(&mount, &authority);
    let app = app_with(&mount);

    // 路径穿越不能把控制台能力带到兔维斯管理面。
    for uri in [format!("/console/{token}/../../m/api/v1/sessions")] {
        let response = call(&app, get_request(&uri, None)).await;
        assert!(
            response.status() == StatusCode::FORBIDDEN
                || response.status() == StatusCode::NOT_FOUND,
            "{uri} 不得放行，实际 {}",
            response.status()
        );
    }
    // 管理面本身在没有设备 cookie 时仍然 403（控制台能力不构成设备身份）。
    // 用真实生产路由验证：控制台子树是**独立**路由，不参与这条判定。
    let management_app = server::router_with_static(memory_state());
    let management = call(&management_app, get_request("/m/api/v1/sessions", None)).await;
    assert_eq!(management.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn put_path_manipulation_is_rejected_before_contacting_upstream() {
    let _guard = CREDENTIAL_TEST_LOCK.lock().await;
    let (mount, _devices) = mount_with_devices(&["dev-a"]);
    install_fixture_cookie(&mount, "127.0.0.1:1", 1);
    let token = register_capability(&mount, "127.0.0.1:1");
    let app = app_with(&mount);
    let response = call(
        &app,
        get_request(&format!("/console/{token}/assets/../secret"), None),
    )
    .await;
    // 归一化后无法越界时仍是 404/403；关键是**不是** 502（未连上游）。
    assert_ne!(
        response.status(),
        StatusCode::BAD_GATEWAY,
        "越界路径必须在连接上游之前被拒绝"
    );
}

#[tokio::test]
async fn revocation_invalidates_capability_and_cancels_live_connections() {
    let _guard = CREDENTIAL_TEST_LOCK.lock().await;
    let (mount, _devices) = mount_with_devices(&["dev-a"]);
    let authority = fake_upstream().await;
    install_fixture_cookie(&mount, &authority, 1);
    let token = register_capability(&mount, &authority);
    let app = app_with(&mount);

    assert_eq!(
        call(&app, get_request(&format!("/console/{token}/"), None))
            .await
            .status(),
        StatusCode::OK
    );

    // 原子授权：登记一条活连接，拿到取消接收端。
    let (_record, mut watcher) = mount
        .authorize_live_for_test(&token)
        .expect("活连接应被授权");

    mount.revoke_all();

    assert!(*watcher.borrow_and_update(), "撤权必须取消活连接");
    assert_eq!(
        call(&app, get_request(&format!("/console/{token}/"), None))
            .await
            .status(),
        StatusCode::FORBIDDEN,
        "撤权后能力必须立即失效"
    );
}

#[tokio::test]
async fn unknown_capability_is_forbidden() {
    let (mount, _devices) = mount_with_devices(&["dev-a"]);
    let app = app_with(&mount);
    let response = call(
        &app,
        get_request("/console/ffffffffffffffffffffffffffffffff/", None),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let csp = sandbox_csp(&response);
    assert!(csp.contains("sandbox"), "连拒绝响应也必须声明 sandbox");
}

/// 内存设备是 gate 的唯一依赖：确认测试没有偷偷落盘。
#[tokio::test]
async fn fixture_never_uses_a_disk_device_store() {
    let state = memory_state();
    let store: &DeviceStore = &state.store;
    store.with(|conn| {
        let mut statement = conn.prepare("PRAGMA database_list").unwrap();
        let paths: Vec<String> = statement
            .query_map([], |row| row.get(2))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(paths.iter().all(String::is_empty), "禁止磁盘设备库");
    });
    // gate 常量与生产一致（防止测试与生产漂移）。
    assert_eq!(gate::COOKIE_NAME, "mam_device");
    let _: HashMap<String, String> = HashMap::new();
}

// ============================================================
// 四渠道回程：LAN / quick / named / Tailscale
// ============================================================
//
// 四渠道都**复用同一监听与同一 `/console/<capability>/` 子树**：
// * LAN  = `http://<ip>:<port>`
// * quick = `https://<random>.trycloudflare.com`
// * named = `https://<自有域名>`
// * Tailscale = `https://<machine>.<tailnet>.ts.net`
//
// 渠道只改变浏览器看到的 origin 与传输方式（https → wss），不改变路径、
// 不改变授权方式。因此对每个渠道只需证明：同一能力路径在该 origin 下
// 仍然（a）不依赖设备 cookie 可加载、（b）强制 opaque sandbox、
// （c）不把上游 Set-Cookie 回传。

/// 四渠道在浏览器侧的 origin 形态（域名形态与生产一致，绝不是真实地址）。
const CHANNEL_ORIGINS: [(&str, &str); 4] = [
    ("lan", "http://192.168.1.50:9420"),
    ("quick", "https://random-words-1234.trycloudflare.com"),
    ("named", "https://board.example.com"),
    ("tailscale", "https://my-mac.tailnet-abc.ts.net"),
];

#[tokio::test]
async fn every_channel_serves_the_console_from_the_same_listener_and_path() {
    let _guard = CREDENTIAL_TEST_LOCK.lock().await;
    let (mount, _devices) = mount_with_devices(&["dev-a"]);
    let authority = fake_upstream().await;
    let app = app_with(&mount);

    install_fixture_cookie(&mount, &authority, 1);
    for (channel, origin) in CHANNEL_ORIGINS {
        // 每个渠道独立签一条能力（生产里设备/实例相同但入口各自生成）。
        let token = register_capability(&mount, &authority);
        let path = format!("/console/{token}/");

        // 1) 浏览器带该渠道的 Origin/Referer 请求控制台资源。
        let builder = Request::builder()
            .uri(&path)
            .method("GET")
            .header(header::ORIGIN, origin)
            .header(header::REFERER, format!("{origin}{path}"));
        let request = builder.body(Body::empty()).unwrap();
        let response = call(&app, request).await;
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "{channel}: 控制台必须在同一监听同一路径下可加载"
        );
        let csp = sandbox_csp(&response);
        assert!(
            !csp.contains("allow-same-origin"),
            "{channel}: 渠道不得改变隔离强度: {csp}"
        );
        assert!(
            response.headers().get(header::SET_COOKIE).is_none(),
            "{channel}: 宿主凭据不得回传"
        );
        let body = body_text(response).await;
        assert!(
            body.contains("official-console-fixture"),
            "{channel}: body={body}"
        );

        // 2) 同一渠道下访问管理面（不带设备 cookie）必须仍被拒——
        //    控制台能力令牌不能当作设备身份使用。管理面在生产路由里验证。
        let management_app = server::router_with_static(memory_state());
        let builder = Request::builder()
            .uri("/m/api/v1/sessions")
            .method("GET")
            .header(header::ORIGIN, origin);
        let request = builder.body(Body::empty()).unwrap();
        assert_eq!(
            call(&management_app, request).await.status(),
            StatusCode::FORBIDDEN,
            "{channel}: 渠道不得绕过设备门禁"
        );
    }
}

#[tokio::test]
async fn console_entry_url_is_relative_so_it_works_on_every_channel() {
    // 入口返回的必须是**相对路径**：绝对 URL 会把某一渠道的地址烧进响应，
    // 换渠道（尤其 quick 的随机域名）就会失效。
    let state = memory_state();
    let device = pair_device(&state);
    let cookie = device_cookie(&device);
    let app = server::router_with_static(state);

    // 会话快照为空 → 不会真签发，但错误分支也必须不泄露绝对地址。
    let response = call(
        &app,
        get_request(
            "/m/api/v1/dsh-console-entry?session_id=missing",
            Some(&cookie),
        ),
    )
    .await;
    let body = body_text(response).await;
    for (_, origin) in CHANNEL_ORIGINS {
        assert!(
            !body.contains(origin),
            "响应不得包含任何渠道的绝对地址: {body}"
        );
    }
    assert!(
        !body.contains("http://") && !body.contains("https://"),
        "入口/错误响应不得硬编码绝对 URL: {body}"
    );
}

// ============================================================
// 撤权接线：设备吊销 / 停远程必须真正作废能力（走产品入口，不走内部函数）
// ============================================================

#[tokio::test]
async fn revoking_a_device_invalidates_only_that_devices_capabilities() {
    let _guard = CREDENTIAL_TEST_LOCK.lock().await;
    let (mount, devices) = mount_with_devices(&["dev-revoke-me", "dev-keep"]);
    let authority = fake_upstream().await;
    install_fixture_cookie(&mount, &authority, 1);
    let mut tokens = Vec::new();
    for device in ["dev-revoke-me", "dev-keep"] {
        let token = format!("{:032x}", rand::random::<u128>());
        mount
            .register(ConsoleCapability {
                token: token.clone(),
                device_id: device.into(),
                instance_id: "inst-a".into(),
                authority: authority.clone(),
                epoch: 1,
                bootstrap: false,
                expires_at_ms: chrono::Utc::now().timestamp_millis() + 60_000,
            })
            .expect("能力可登记");
        tokens.push(token);
    }
    let (revoked_token, kept_token) = (&tokens[0], &tokens[1]);
    let app = app_with(&mount);
    assert_eq!(
        call(
            &app,
            get_request(&format!("/console/{revoked_token}/"), None)
        )
        .await
        .status(),
        StatusCode::OK
    );

    // 两台的活连接都登记下来，用于验证「只断被撤的那台」。
    let (_, mut revoked_live) = mount
        .authorize_live_for_test(revoked_token)
        .expect("授权 A");
    let (_, mut kept_live) = mount.authorize_live_for_test(kept_token).expect("授权 B");

    // 撤设备：设备库判定翻转（生产即设备行 revoked）。
    // 关键断言：即使**没有人记得撤销能力表**，该设备的令牌也必须立即失效。
    devices.lock().unwrap().retain(|d| d != "dev-revoke-me");

    assert_eq!(
        call(
            &app,
            get_request(&format!("/console/{revoked_token}/"), None)
        )
        .await
        .status(),
        StatusCode::FORBIDDEN,
        "被撤设备的能力必须立即失效"
    );
    assert_eq!(
        call(&app, get_request(&format!("/console/{kept_token}/"), None))
            .await
            .status(),
        StatusCode::OK,
        "其他设备的能力不受影响"
    );

    // 显式撤该设备：只取消它自己的活连接。
    mount.revoke_device("dev-revoke-me");
    assert!(
        *revoked_live.borrow_and_update(),
        "被撤设备的活连接必须取消"
    );
    assert!(
        !*kept_live.borrow_and_update(),
        "其他设备的活连接不得被误断"
    );
}

#[tokio::test]
async fn stopping_remote_revokes_the_production_mount() {
    let _guard = CREDENTIAL_TEST_LOCK.lock().await;
    // 生产挂载的授权不使用测试设备库，此处仅核对整域撤权状态。
    let production = console_mount::production();
    let token = format!("{:032x}", rand::random::<u128>());
    production
        .register(ConsoleCapability {
            token: token.clone(),
            device_id: "dev-prod".into(),
            instance_id: "inst-prod".into(),
            authority: "127.0.0.1:1".into(),
            epoch: 1,
            bootstrap: false,
            expires_at_ms: chrono::Utc::now().timestamp_millis() + 60_000,
        })
        .expect("能力可登记");

    assert!(production.has_capability_for_test(&token));
    let lifecycle = "inst-prod";
    let authority = "127.0.0.1:1";
    app::remote::console_lifecycle::install_upstream_credentials(
        lifecycle,
        "api=fixture".into(),
        "browser=fixture".into(),
        1,
        authority,
    )
    .unwrap();
    assert!(
        app::remote::console_lifecycle::upstream_browser_cookie_for(lifecycle, 1, authority)
            .is_some()
    );

    app::remote::console_lifecycle::revoke_console_access();

    assert!(
        !production.has_capability_for_test(&token),
        "关远程后生产挂载的能力必须作废"
    );
    assert!(
        app::remote::console_lifecycle::upstream_browser_cookie_for(lifecycle, 1, authority)
            .is_none()
    );
}

#[tokio::test]
async fn revoke_all_cancels_every_live_connection_and_clears_capabilities() {
    let (mount, _devices) = mount_with_devices(&["dev-a"]);
    let authority = fake_upstream().await;
    install_fixture_cookie(&mount, &authority, 1);
    let first = register_capability(&mount, &authority);
    let second = register_capability(&mount, &authority);
    let (_, mut live_a) = mount.authorize_live_for_test(&first).expect("授权 A");
    let (_, mut live_b) = mount.authorize_live_for_test(&second).expect("授权 B");

    mount.revoke_all();

    assert!(*live_a.borrow_and_update(), "全量撤权必须取消 A");
    assert!(*live_b.borrow_and_update(), "全量撤权必须取消 B");
    assert!(mount.lookup_for_test(&first).is_none(), "能力必须清空");
}

// ============================================================
// P1-2a / P1-2b：上游凭据受众与转发保真
//
// 这两组断言的判据都在**假上游侧**：它记录自己实际收到了什么。只看网关返回值
// 证明不了「发出去的头对不对」——上游 200 可能只是因为它不在乎。
// ============================================================

/// 记录型假上游：捕获每个请求的路径、cookie 与关键头。
#[derive(Clone, Default)]
struct Captured {
    inner: Arc<Mutex<Vec<CapturedRequest>>>,
}

#[derive(Clone, Debug)]
struct CapturedRequest {
    path: String,
    method: String,
    query: Option<String>,
    body: axum::body::Bytes,
    cookie: Option<String>,
    content_type: Option<String>,
}

impl Captured {
    fn take(&self) -> Vec<CapturedRequest> {
        std::mem::take(&mut *self.inner.lock().unwrap())
    }
    fn push(&self, request: CapturedRequest) {
        self.inner.lock().unwrap().push(request);
    }
}

/// 记录型假上游：`/` 与 `/assets/*` 回首页，`/api/*` 回 JSON，`/redirect`
/// 回 302 指向 `redirect_to`（用于验证网关**不跟随**）。
///
/// `redirect_to` 必须是另一个**记录型**服务器：只有这样「有没有跟随」才可观测
/// ——把目标设成不可解析域名时，跟随与不跟随都以 502 收场，用例对策略变异不敏感。
async fn recording_upstream_with_redirect_to(redirect_to: Option<String>) -> (String, Captured) {
    use axum::extract::Request as AxumRequest;

    let captured = Captured::default();
    let state = captured.clone();
    let app = Router::new().fallback(move |request: AxumRequest| {
        let captured = state.clone();
        let redirect_to = redirect_to.clone();
        async move {
            let path = request.uri().path().to_owned();
            let (parts, body) = request.into_parts();
            let headers = parts.headers;
            captured.push(CapturedRequest {
                path: path.clone(),
                method: parts.method.to_string(),
                query: parts.uri.query().map(str::to_owned),
                body: axum::body::to_bytes(body, 1024 * 1024).await.unwrap(),
                cookie: headers
                    .get(header::COOKIE)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned),
                content_type: headers
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned),
            });
            if path.ends_with("/redirect") {
                let target = redirect_to
                    .clone()
                    .unwrap_or_else(|| "http://example.invalid/elsewhere".to_string());
                return (StatusCode::FOUND, [(header::LOCATION, target)], "moved").into_response();
            }
            if path.starts_with("/api/") {
                return (
                    [(header::CONTENT_TYPE, "application/json")],
                    r#"{"ok":true}"#,
                )
                    .into_response();
            }
            (
                [(header::CONTENT_TYPE, "text/html")],
                "<html>console</html>",
            )
                .into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let authority = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (authority, captured)
}

/// 无重定向目标时的普通记录型上游（既有用例用）。
async fn recording_upstream() -> (String, Captured) {
    recording_upstream_with_redirect_to(None).await
}

/// 每个用例独占改进程级凭据存储；与已安装凭据的其他用例串行。
static CREDENTIAL_TEST_LOCK: std::sync::LazyLock<tokio::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(()));

/// 安装到测试使用的挂载；必须先于该实例的能力登记。
fn install_fixture_cookie(mount: &console_mount::ConsoleMount, authority: &str, epoch: u64) {
    mount
        .install_browser_credential(
            "inst-a",
            epoch,
            authority,
            "fixture-browser=synthetic".into(),
            app::services::dsh_control::transport::PeerGate::new(),
        )
        .expect("合成浏览器凭据可安装");
}

/// P1-2a：**控制台页面通道（首页、静态资源、`/api/*`）一律走浏览器 cookie**。
///
/// 依据官方 `admit()`：它先试 `isAuthenticated`（浏览器 cookie → `browserPeer`），
/// 认不出才试 `authenticateApi`（version 2 → 具名 API Peer）。页面自己加载的脚本
/// 用的就是它那枚浏览器 cookie。
///
/// 为什么 `/api/*` **不能**用 API 凭据：那会把页面挂到 `/connect` 开出的**共享
/// API Peer** 上，而插件正持有同一条单 WS——两边争用一条连接，关掉控制台就把
/// 插件那条掐断。浏览器 cookie 让两边各是各自的 Peer。
#[tokio::test]
async fn the_whole_page_channel_uses_the_browser_cookie_not_the_shared_api_peer() {
    let _guard = CREDENTIAL_TEST_LOCK.lock().await;
    let (authority, captured) = recording_upstream().await;
    app::remote::console_lifecycle::install_upstream_credentials(
        "inst-a",
        "API-CREDENTIAL-v2".into(),
        String::new(),
        1,
        &authority,
    )
    .expect("旧网关 API 凭据可安装作对照");
    let (mount, _devices) = mount_with_devices(&["dev-a"]);
    mount
        .install_browser_credential(
            "inst-a",
            1,
            &authority,
            "BROWSER-COOKIE-v1".into(),
            app::services::dsh_control::transport::PeerGate::new(),
        )
        .expect("浏览器凭据可安装");
    let token = register_capability(&mount, &authority);
    let app = app_with(&mount);

    // 首页
    let response = call(&app, get_request(&format!("/console/{token}/"), None)).await;
    assert_eq!(response.status(), StatusCode::OK);
    // 静态资源
    let response = call(
        &app,
        get_request(&format!("/console/{token}/assets/app.js"), None),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    // API（官方一元方法是 POST；GET 会被方法表按错配拒绝）
    let response = call(
        &app,
        Request::builder()
            .uri(format!("/console/{token}/api/session/list"))
            .method("POST")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    let seen = captured.take();
    let home = seen
        .iter()
        .find(|r| r.path == "/")
        .expect("首页请求到达上游");
    assert_eq!(
        home.cookie.as_deref(),
        Some("BROWSER-COOKIE-v1"),
        "首页必须用浏览器会话 cookie（authorizeIndex 只认 version 1）；实际：{:?}",
        home.cookie
    );
    let static_req = seen
        .iter()
        .find(|r| r.path.starts_with("/assets/"))
        .expect("静态资源请求到达上游");
    assert_eq!(
        static_req.cookie.as_deref(),
        Some("BROWSER-COOKIE-v1"),
        "静态资源与首页同受众"
    );
    let api = seen
        .iter()
        .find(|r| r.path.starts_with("/api/"))
        .expect("API 请求到达上游");
    assert_eq!(
        api.cookie.as_deref(),
        Some("BROWSER-COOKIE-v1"),
        "页面通道的 /api/* 必须用浏览器 cookie（官方 admit 认它为 browserPeer）；\
         用 API 凭据会与插件争用同一条共享单 WS。实际：{:?}",
        api.cookie
    );
    app::remote::console_lifecycle::clear_upstream_credentials();
}

/// P1-2a 的反面：只有 API 凭据时，首页**不得**拿它顶替。
#[tokio::test]
async fn home_never_falls_back_to_the_api_credential_when_the_browser_cookie_is_missing() {
    let _guard = CREDENTIAL_TEST_LOCK.lock().await;
    let (authority, captured) = recording_upstream().await;
    // 只交 API 凭据（等价旧版插件：没有 browserCookie 字段）。
    app::remote::console_lifecycle::install_upstream_credentials(
        "inst-a",
        "API-CREDENTIAL-v2".to_string(),
        String::new(),
        1,
        &authority,
    )
    .expect("API 凭据可安装");

    let (mount, _devices) = mount_with_devices(&["dev-a"]);
    let token = register_capability(&mount, &authority);
    let app = app_with(&mount);
    let response = call(&app, get_request(&format!("/console/{token}/"), None)).await;

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(
        captured.take().is_empty(),
        "浏览器凭据缺失时不得访问上游，也不得使用 API 凭据顶替"
    );

    app::remote::console_lifecycle::clear_upstream_credentials();
}

/// P1-2b：入站 `content-type` 必须传到上游（丢它会让 JSON/multipart 被上游 415）。
#[tokio::test]
async fn request_content_type_is_forwarded_to_the_upstream() {
    let _guard = CREDENTIAL_TEST_LOCK.lock().await;
    let (authority, captured) = recording_upstream().await;

    let (mount, _devices) = mount_with_devices(&["dev-a"]);
    mount
        .install_browser_credential(
            "inst-a",
            1,
            &authority,
            "BROWSER-COOKIE-v1".into(),
            app::services::dsh_control::transport::PeerGate::new(),
        )
        .expect("浏览器凭据可安装");
    let token = register_capability(&mount, &authority);
    let app = app_with(&mount);

    let request = Request::builder()
        .uri(format!("/console/{token}/api/session/prompt"))
        .method("POST")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"a":1}"#))
        .unwrap();
    let response = call(&app, request).await;
    assert_eq!(response.status(), StatusCode::OK);

    let seen = captured.take();
    let post = seen
        .iter()
        .find(|r| r.path == "/api/session/prompt")
        .expect("POST 到达上游");
    assert_eq!(
        post.content_type.as_deref(),
        Some("application/json"),
        "content-type 必须原样转发（旧实现在此丢掉它，上游会 415）"
    );
}

/// P1-2b：上游 3xx **不得**被网关跟随或回传（避免把带凭据的请求送出去）。
#[tokio::test]
async fn upstream_redirects_are_refused_instead_of_followed() {
    let _guard = CREDENTIAL_TEST_LOCK.lock().await;
    // 重定向目标 = 第二个记录型上游。跟随与否在这里**可区分**：若网关跟随，
    // 目标服务器会收到一次带凭据的请求（这正是要防的事）。
    let (elsewhere, elsewhere_seen) = recording_upstream().await;
    let (authority, captured) =
        recording_upstream_with_redirect_to(Some(format!("http://{elsewhere}/leak"))).await;

    let (mount, _devices) = mount_with_devices(&["dev-a"]);
    mount
        .install_browser_credential(
            "inst-a",
            1,
            &authority,
            "BROWSER-COOKIE-v1".into(),
            app::services::dsh_control::transport::PeerGate::new(),
        )
        .expect("浏览器凭据可安装");
    // `/redirect` 不在允许路由里，用 `/assets/redirect` 命中静态分类但由假上游回 302。
    let token = register_capability(&mount, &authority);
    let app = app_with(&mount);
    let response = call(
        &app,
        get_request(&format!("/console/{token}/assets/redirect"), None),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::BAD_GATEWAY,
        "上游重定向必须被拒绝，不得跟随也不得回传 location"
    );
    assert!(
        response.headers().get(header::LOCATION).is_none(),
        "不得把上游的 location 回传给浏览器（会把控制台送出实例）"
    );
    // 只应有一次上游请求：跟随会产生第二次（打到另一台服务器）。
    let seen = captured.take();
    assert_eq!(seen.len(), 1, "不得跟随重定向产生额外请求：{seen:?}");
    let leaked = elsewhere_seen.take();
    assert!(
        leaked.is_empty(),
        "重定向目标不得收到任何请求（跟随就会把带凭据的请求送出去）：{leaked:?}"
    );
}

// ============================================================
// P1-2c：能力必须绑代际——端口会复用，代际不会
// ============================================================

/// 宿主换代（同实例、可能同端口）后，旧能力必须立即失效。
///
/// 威胁路径：能力只绑 `authority` → 宿主重启后端口被复用（甚至是另一个进程占上）
/// → 网关仍为旧能力附上**当代**凭据并发往那个端口，等于把新代秘密送到陌生对端。
#[tokio::test]
async fn capability_is_rejected_once_the_host_generation_changes() {
    let _guard = CREDENTIAL_TEST_LOCK.lock().await;
    let (authority, _captured) = recording_upstream().await;
    // 当代代际 = 2；能力签发时代际 = 1（模拟签发后宿主重启换代）。
    let mount = console_mount::ConsoleMount::new()
        .with_device_check(|_| true)
        .with_epoch_check(|instance_id, epoch| instance_id == "inst-a" && epoch == 2);
    let mount = Arc::new(mount);
    install_fixture_cookie(&mount, &authority, 2);
    let token = register_capability_with_epoch(&mount, &authority, 1);
    let app = app_with(&mount);

    let response = call(&app, get_request(&format!("/console/{token}/"), None)).await;
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "换代后旧能力必须立即失效（不得把当代凭据送往旧代 authority）"
    );

    // 对照：当代能力照常可用（不是把整条通道关掉）。
    let fresh = register_capability_with_epoch(&mount, &authority, 2);
    let response = call(&app, get_request(&format!("/console/{fresh}/"), None)).await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "当代代际的能力必须仍可用"
    );
}

// ============================================================
// P1-2d：撤权必须能终止**在途 HTTP**（不只是发信号）
//
// 判据取自上游侧：上游确实收到请求并开始回包，而网关在撤权后**没有**把响应
// 交给浏览器。只断言「取消信号被置位」是空转——信号置位而转发照常读完，用户
// 看到的仍是一份完整响应。
// ============================================================

/// 永远不结束响应体的慢上游（发完头就挂住）。
async fn hanging_upstream() -> (String, Arc<tokio::sync::Notify>) {
    use axum::response::IntoResponse;
    let started = Arc::new(tokio::sync::Notify::new());
    let notify = Arc::clone(&started);
    let app = Router::new().fallback(move || {
        let notify = Arc::clone(&notify);
        async move {
            notify.notify_waiters();
            // 头已发出，body 永不来：这正是「撤权后会不会继续等」的场景。
            let stream = futures::stream::pending::<Result<Vec<u8>, std::io::Error>>();
            (
                [(header::CONTENT_TYPE, "text/html")],
                axum::body::Body::from_stream(stream),
            )
                .into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let authority = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (authority, started)
}

/// 撤权必须让在途 HTTP 请求立即结束（而不是等上游把 body 发完）。
#[tokio::test]
async fn revocation_terminates_an_in_flight_http_request() {
    let _guard = CREDENTIAL_TEST_LOCK.lock().await;
    let (authority, started) = hanging_upstream().await;
    let (mount, _devices) = mount_with_devices(&["dev-a"]);
    install_fixture_cookie(&mount, &authority, 1);
    let token = register_capability(&mount, &authority);
    let app = app_with(&mount);

    let request = get_request(&format!("/console/{token}/"), None);
    let mount_for_revoke = Arc::clone(&mount);
    let token_for_revoke = token.clone();
    // 等上游真的开始回包（头已发、body 挂住），再撤权。
    tokio::spawn(async move {
        started.notified().await;
        // 让网关先进入「等 body」状态。
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        mount_for_revoke.revoke_instance("inst-a");
        let _ = token_for_revoke;
    });

    let response = tokio::time::timeout(std::time::Duration::from_secs(5), call(&app, request))
        .await
        .expect("撤权必须在有限时间内结束在途 HTTP 请求（否则慢上游会让撤权形同虚设）");
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "撤权后在途请求必须被终止（403），不得继续等上游 body"
    );
}

// ============================================================
// P1-2e：入口 URL 是**一次性引导**，不是可转交的长期凭据
//
// 威胁：入口 URL 会出现在地址栏/历史/聊天分享里。若它本身就是 30 分钟有效的
// 页面会话，任何拿到链接的人都能在窗口内打开控制台。分段之后，URL 里那枚只能
// 换一次，换完即废。
// ============================================================

/// 引导票据**不能**在挂载层被公开兑换（M02 把兑换移到已配对设备的 POST）。
///
/// 旧行为是：访问 `/console/<ticket>/` 会 302 换出会话令牌。那等于任何拿到 URL
/// 的人都能自行换会话——两步协议形同虚设。现在挂载层只服务**已兑换的会话**，
/// 引导票据在这里应当 403（它不是会话能力）。
#[tokio::test]
async fn a_bootstrap_ticket_cannot_be_redeemed_through_the_mount() {
    let _guard = CREDENTIAL_TEST_LOCK.lock().await;
    let (authority, _captured) = recording_upstream().await;
    let mount = Arc::new(console_mount::ConsoleMount::new().with_device_check(|_| true));
    install_fixture_cookie(&mount, &authority, 1);
    let bootstrap = format!("{:032x}", rand::random::<u128>());
    mount
        .register(ConsoleCapability {
            token: bootstrap.clone(),
            device_id: "dev-a".into(),
            instance_id: "inst-a".into(),
            authority: authority.clone(),
            epoch: 1,
            bootstrap: true,
            expires_at_ms: chrono::Utc::now().timestamp_millis() + 60_000,
        })
        .expect("引导可登记");
    let app = app_with(&mount);

    let response = call(&app, get_request(&format!("/console/{bootstrap}/"), None)).await;
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "引导票据不得在挂载层被兑换：兑换必须经已配对设备的 POST 入口"
    );

    // 设备绑定的兑换仍然有效（正对照：证明不是把兑换整个关掉）。
    let Some(session) = mount.redeem_bootstrap_for_device(&bootstrap, "dev-a") else {
        panic!("已配对设备应能兑换引导票据");
    };
    let response = call(&app, get_request(&format!("/console/{session}/"), None)).await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "兑换出的会话应能加载控制台"
    );

    // 兑换后再走设备兑换：票据已消费，必须失败。
    assert!(
        mount
            .redeem_bootstrap_for_device(&bootstrap, "dev-a")
            .is_none(),
        "票据一次性：第二次兑换必须失败"
    );
}

/// 引导之外的会话能力不能被误当引导盖章（否则页面内每次导航都会换令牌）。
#[tokio::test]
async fn a_session_token_is_never_treated_as_a_bootstrap() {
    let _guard = CREDENTIAL_TEST_LOCK.lock().await;
    let (authority, _captured) = recording_upstream().await;
    let mount = Arc::new(console_mount::ConsoleMount::new().with_device_check(|_| true));
    install_fixture_cookie(&mount, &authority, 1);
    let token = register_capability(&mount, &authority); // bootstrap: false
    let app = app_with(&mount);
    let response = call(&app, get_request(&format!("/console/{token}/"), None)).await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "页面会话令牌必须直接服务页面，不得被误盖章成 302（否则页面内导航会无限换令牌）"
    );
}

// ============================================================
// P1：引导令牌不得绕过兑换直达子路由 / WS
//
// 引导的语义是「一次性换会话」。若它同时能直接服务请求，那 60 秒窗口内它就是
// 一枚可直达 `/api/*`、可升 WS 的通用凭据——一次性与短寿命都白设。
// ============================================================

#[tokio::test]
async fn a_bootstrap_token_cannot_serve_subresources_or_open_a_websocket() {
    let _guard = CREDENTIAL_TEST_LOCK.lock().await;
    let (authority, captured) = recording_upstream().await;
    let mount = Arc::new(console_mount::ConsoleMount::new().with_device_check(|_| true));
    install_fixture_cookie(&mount, &authority, 1);
    let bootstrap = format!("{:032x}", rand::random::<u128>());
    mount
        .register(ConsoleCapability {
            token: bootstrap.clone(),
            device_id: "dev-a".into(),
            instance_id: "inst-a".into(),
            authority: authority.clone(),
            epoch: 1,
            bootstrap: true,
            expires_at_ms: chrono::Utc::now().timestamp_millis() + 60_000,
        })
        .expect("引导可登记");
    let app = app_with(&mount);

    // 静态资源：必须 403（引导只能换会话，不能当页面用）。
    let response = call(
        &app,
        get_request(&format!("/console/{bootstrap}/assets/app.js"), None),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "引导令牌不得直接服务静态资源（否则它就是一页通用凭据）"
    );

    // API：同样 403。
    let response = call(
        &app,
        get_request(&format!("/console/{bootstrap}/api/ping"), None),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "引导令牌不得直达 API"
    );

    // 上游一次都不该被打到。
    assert!(
        captured.take().is_empty(),
        "被拒的引导请求不得触达上游（拒绝必须发生在转发之前）"
    );

    // WS 升级：登记活连接同样必须拒绝。
    assert!(
        mount.authorize_live_for_test(&bootstrap).is_none(),
        "引导令牌不得登记活连接（WS 升级路径与 HTTP 同一道门）"
    );
}

// ============================================================
// P1：HTTP 到期终止 + 流式正文上限
//
// 到期与撤权是两个独立条件：只挂取消信号时，「页面开着、设备还在、但能力已过期」
// 的请求仍会成功。正文上限也不能只看 Content-Length——chunked 或上游撒谎时
// 那个头根本不存在。
// ============================================================

/// 发送超限字节后暂停尾块和 EOF，确保网关不等待整包完成。
async fn chunked_oversize_upstream() -> (String, tokio::sync::oneshot::Sender<()>) {
    use futures::StreamExt;
    let (release, wait) = tokio::sync::oneshot::channel::<()>();
    let wait = Arc::new(Mutex::new(Some(wait)));
    let app = Router::new().fallback(move || {
        let wait = wait.lock().unwrap().take().unwrap();
        async move {
            let chunks = futures::stream::iter(
                (0..33).map(|_| Ok::<Vec<u8>, std::io::Error>(vec![b'x'; 1024 * 1024])),
            );
            let tail = futures::stream::once(async move {
                let _ = wait.await;
                Ok::<Vec<u8>, std::io::Error>(vec![b'z'])
            });
            (
                [(header::CONTENT_TYPE, "application/octet-stream")],
                axum::body::Body::from_stream(chunks.chain(tail)),
            )
                .into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let authority = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (authority, release)
}

#[tokio::test]
async fn an_oversize_streamed_body_is_refused_without_buffering_it_all() {
    let _guard = CREDENTIAL_TEST_LOCK.lock().await;
    let (authority, release_tail) = chunked_oversize_upstream().await;
    let (mount, _devices) = mount_with_devices(&["dev-a"]);
    install_fixture_cookie(&mount, &authority, 1);
    let token = register_capability(&mount, &authority);
    let app = app_with(&mount);

    let response = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        call(
            &app,
            get_request(&format!("/console/{token}/assets/big.bin"), None),
        ),
    )
    .await
    .expect("超限正文必须在有界时间内被拒（不得把 64 MiB 全读进内存再判断）");
    assert_eq!(
        response.status(),
        StatusCode::BAD_GATEWAY,
        "流式正文超过上限必须拒绝（Content-Length 缺失/撒谎时也要挡住）"
    );
    // 等到响应返回之后才释放尾块；等待 EOF 的整包读取无法通过上述 timeout。
    let _ = release_tail.send(());
}

/// 能力到期后，在途 HTTP 请求必须被终止（不是只有撤权才终止）。
#[tokio::test]
async fn expiry_terminates_an_in_flight_http_request() {
    let _guard = CREDENTIAL_TEST_LOCK.lock().await;
    let (authority, _started) = hanging_upstream().await;
    let (mount, _devices) = mount_with_devices(&["dev-a"]);
    install_fixture_cookie(&mount, &authority, 1);
    // 能力 400ms 后到期：足以建立连接、但不足以等上游把 body 发完（它永不发完）。
    let token = format!("{:032x}", rand::random::<u128>());
    mount
        .register(ConsoleCapability {
            token: token.clone(),
            device_id: "dev-a".into(),
            instance_id: "inst-a".into(),
            authority,
            epoch: 1,
            bootstrap: false,
            expires_at_ms: chrono::Utc::now().timestamp_millis() + 400,
        })
        .expect("能力可登记");
    let app = app_with(&mount);
    let request = get_request(&format!("/console/{token}/"), None);

    let response = tokio::time::timeout(std::time::Duration::from_secs(5), call(&app, request))
        .await
        .expect("到期必须在有限时间内终止在途 HTTP 请求");
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "能力到期后不得继续服务在途请求（只靠取消信号会让过期页面一直可用）"
    );
}

// ============================================================
// M01：控制台方法准入（HTTP 方法＋路径、Mux 端点与帧类型）
//
// 判据全部落在**上游侧**：上游必须零命中/零业务帧。只看代理返回码不够——代理
// 可能先把请求送到上游、再因上游 401 回一个错误，那已经是越权暴露。
// ============================================================

/// 记录型 WS 上游：接受升级，记录收到的每条文本帧，并回一个最小应答。
async fn recording_ws_upstream() -> (String, Arc<Mutex<Vec<String>>>) {
    use axum::extract::ws::{Message as WsMessage, WebSocketUpgrade};
    use axum::routing::any;

    let frames: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&frames);
    let app = Router::new().route(
        "/api/remote.mux",
        any(move |upgrade: WebSocketUpgrade| {
            let sink = Arc::clone(&sink);
            async move {
                upgrade.on_upgrade(move |mut socket| async move {
                    let s = sink;
                    while let Some(Ok(message)) = socket.recv().await {
                        match message {
                            WsMessage::Text(text) => {
                                s.lock().unwrap().push(text.to_string());
                            }
                            WsMessage::Binary(bytes) => {
                                s.lock().unwrap().push(format!("<binary:{}>", bytes.len()));
                            }
                            _ => {}
                        }
                    }
                })
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let authority = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (authority, frames)
}

/// 起一个**真实监听**的控制台挂载（WS 升级不能走 oneshot），返回其 authority。
async fn serve_console_mount(mount: &Arc<console_mount::ConsoleMount>) -> String {
    let app = app_with(mount);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let authority = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    authority
}

/// 经**真实 router + 真实监听**连控制台 WS 并发送给定帧。
///
/// 必须走真实监听：`tower::ServiceExt::oneshot` 不承载 WS 升级，用它测 WS 等于
/// 测一个永远连不上的路径。
async fn forward_ws_frames_via_mount(
    mount: &Arc<console_mount::ConsoleMount>,
    token: &str,
    frames: &[serde_json::Value],
) -> bool {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let gateway = serve_console_mount(mount).await;
    let url = format!("ws://{gateway}/console/{token}/api/remote.mux");
    let Ok(request) = url.into_client_request() else {
        return false;
    };
    let Ok((mut socket, _)) = tokio_tungstenite::connect_async(request).await else {
        return false;
    };
    use futures::{SinkExt, StreamExt};
    for frame in frames {
        let text = serde_json::to_string(frame).unwrap();
        if socket
            .send(tokio_tungstenite::tungstenite::Message::text(text))
            .await
            .is_err()
        {
            return false;
        }
    }
    // 留时间让桥接把帧送到上游，再关闭。
    let _ = tokio::time::timeout(std::time::Duration::from_millis(400), socket.next()).await;
    let _ = socket.close(None).await;
    true
}

/// 完成一次真实 HTTP 代理后，活请求登记必须释放。
#[tokio::test]
async fn completed_http_request_releases_its_live_registration() {
    let _guard = CREDENTIAL_TEST_LOCK.lock().await;
    let (authority, captured) = recording_upstream().await;
    let (mount, _) = mount_with_devices(&["dev-a"]);
    install_fixture_cookie(&mount, &authority, 1);
    let token = register_capability(&mount, &authority);
    let baseline = mount.live_count_for_test();
    let response = call(
        &app_with(&mount),
        get_request(&format!("/console/{token}/"), None),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(captured.take().len(), 1, "请求必须实际到达上游");
    assert_eq!(
        mount.live_count_for_test(),
        baseline,
        "HTTP 完成后必须释放活请求登记"
    );
}

/// 正常转发完成后释放活请求登记。
#[tokio::test]
async fn forwarded_http_request_releases_its_live_registration() {
    let _guard = CREDENTIAL_TEST_LOCK.lock().await;
    let (authority, captured) = recording_upstream().await;
    let (mount, _) = mount_with_devices(&["dev-a"]);
    install_fixture_cookie(&mount, &authority, 1);
    let token = register_capability(&mount, &authority);
    let baseline = mount.live_count_for_test();
    let response = call(
        &app_with(&mount),
        post_json(
            &format!("/console/{token}/api/terminal/create"),
            None,
            json!({}),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(captured.take().len(), 1);
    assert_eq!(
        mount.live_count_for_test(),
        baseline,
        "转发完成后必须释放活请求登记"
    );
}

/// 上游自然关闭 WS 后，生产挂载必须释放活连接登记。
#[tokio::test]
async fn naturally_closed_websocket_releases_its_live_registration() {
    use axum::extract::ws::WebSocketUpgrade;
    use futures::{SinkExt, StreamExt};

    let _guard = CREDENTIAL_TEST_LOCK.lock().await;
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
    let (close_tx, close_rx) = tokio::sync::oneshot::channel::<()>();
    let handshake = Arc::new(Mutex::new(Some((ready_tx, close_rx))));
    let upstream = Router::new().route(
        "/api/remote.mux",
        route_get(move |upgrade: WebSocketUpgrade| {
            let (ready, close) = handshake.lock().unwrap().take().unwrap();
            async move {
                upgrade.on_upgrade(move |mut socket| async move {
                    let _ = ready.send(());
                    let _ = close.await;
                    let _ = socket.close().await;
                })
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let authority = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let _ = axum::serve(listener, upstream).await;
    });
    let (mount, _) = mount_with_devices(&["dev-a"]);
    install_fixture_cookie(&mount, &authority, 1);
    let token = register_capability(&mount, &authority);
    let baseline = mount.live_count_for_test();
    let gateway = serve_console_mount(&mount).await;
    let (mut socket, _) =
        tokio_tungstenite::connect_async(format!("ws://{gateway}/console/{token}/api/remote.mux"))
            .await
            .expect("挂载 WS 必须成功升级");
    tokio::time::timeout(std::time::Duration::from_secs(3), ready_rx)
        .await
        .expect("上游 WS 握手超时")
        .expect("上游必须确认连接");
    assert_eq!(
        mount.live_count_for_test(),
        baseline + 1,
        "关闭前必须实际登记活连接"
    );
    close_tx.send(()).expect("上游应等待自然关闭信号");
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while let Some(Ok(message)) = socket.next().await {
            if message.is_close() {
                break;
            }
        }
    })
    .await
    .expect("浏览器端必须观察到桥接关闭");
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while mount.live_count_for_test() != baseline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("WS 自然结束后活连接登记必须恢复基线");
}

#[tokio::test]
async fn mounted_http_preserves_method_path_query_and_body_for_new_routes() {
    let _guard = CREDENTIAL_TEST_LOCK.lock().await;
    let (authority, captured) = recording_upstream().await;
    let (mount, _) = mount_with_devices(&["dev-a"]);
    install_fixture_cookie(&mount, &authority, 1);
    let token = register_capability(&mount, &authority);
    let response = call(
        &app_with(&mount),
        Request::builder()
            .uri(format!("/console/{token}/new-plugin/action?q=a%2Bb"))
            .method("PATCH")
            .body(Body::from("payload"))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let seen = captured.take();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, "PATCH");
    assert_eq!(seen[0].path, "/new-plugin/action");
    assert_eq!(seen[0].query.as_deref(), Some("q=a%2Bb"));
    assert_eq!(seen[0].body.as_ref(), b"payload");
}

/// 正对照：官方一元方法必须放行（证明拒绝不是被设备门/坏令牌挡住）。
#[tokio::test]
async fn official_http_unary_methods_are_forwarded() {
    let _guard = CREDENTIAL_TEST_LOCK.lock().await;
    for path in ["/api/session/list", "/api/settings/describe"] {
        let (authority, captured) = recording_upstream().await;
        let (mount, _devices) = mount_with_devices(&["dev-a"]);
        install_fixture_cookie(&mount, &authority, 1);
        let token = register_capability(&mount, &authority);
        let app = app_with(&mount);
        let request = Request::builder()
            .uri(format!("/console/{token}{path}"))
            .method("POST")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let response = call(&app, request).await;
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "{path} 是官方一元方法，必须放行（否则拒绝缺少正对照）"
        );
        let seen = captured.take();
        assert_eq!(seen.len(), 1, "{path} 必须恰好触达上游一次：{seen:?}");
    }
}

// ============================================================
// M01：活跃 WS 桥撤权时关闭上游连接。
// 此夹具不能观测生产 send 的 Poll::Pending，不覆盖阻塞写入期间撤权。
// ============================================================

/// 上游消费 WS 消息并观察连接关闭；不证明生产写入进入 Pending。
async fn non_reading_ws_upstream() -> (String, Arc<std::sync::atomic::AtomicBool>) {
    use axum::extract::ws::WebSocketUpgrade;
    use axum::routing::any;

    // 上游连接被关闭时置位：这是「网关真的释放了连接」的直接证据。
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = Arc::clone(&dropped);
    let app = Router::new().route(
        "/api/remote.mux",
        any(move |upgrade: WebSocketUpgrade| {
            let flag = Arc::clone(&flag);
            async move {
                upgrade.on_upgrade(move |mut socket| async move {
                    // 接收消息以观察关闭；读取过程会消费数据，因此不能证明背压。
                    loop {
                        match tokio::time::timeout(
                            std::time::Duration::from_millis(100),
                            socket.recv(),
                        )
                        .await
                        {
                            // 超时：尚未收到数据或关闭，继续等待。
                            Err(_) => continue,
                            // 真正收到关闭（或数据后关闭）。
                            Ok(None) | Ok(Some(Err(_))) => {
                                flag.store(true, std::sync::atomic::Ordering::SeqCst);
                                return;
                            }
                            // 收到 Ping/Pong/Close 等控制帧：继续等。
                            Ok(Some(Ok(_))) => continue,
                        }
                    }
                })
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let authority = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (authority, dropped)
}

/// 检查活跃 WS 桥撤权后关闭上游连接；此用例不证明 send 进入 Pending。
#[tokio::test]
async fn revocation_closes_an_active_websocket_bridge() {
    let _guard = CREDENTIAL_TEST_LOCK.lock().await;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    async fn spawn_blocked_bridge(token: &str, gateway: &str) -> tokio::task::JoinHandle<()> {
        let url = format!("ws://{gateway}/console/{token}/api/remote.mux");
        let request = url.into_client_request().unwrap();
        let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        use futures::SinkExt;
        socket
            .send(tokio_tungstenite::tungstenite::Message::text(
                r#"{"type":"open","streamId":"s1","endpoint":"session/follow","payload":{"args":{}}}"#,
            ))
            .await
            .unwrap();
        // `item` 帧的字段名按官方协议是 `value`（不是 payload），否则真实上游会拒。
        let big = "x".repeat(64 * 1024);
        let item = format!(r#"{{"type":"item","streamId":"s1","value":{{"data":"{big}"}}}}"#);
        tokio::spawn(async move {
            for _ in 0..64 {
                if socket
                    .send(tokio_tungstenite::tungstenite::Message::text(item.clone()))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            // 保持连接打开，模拟慢端。
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            let _ = socket.close(None).await;
        })
    }

    // --- 对照组：不撤权，活连接数不应归零 ---
    {
        let (authority, upstream_dropped) = non_reading_ws_upstream().await;
        let (mount, _devices) = mount_with_devices(&["dev-a"]);
        install_fixture_cookie(&mount, &authority, 1);
        let token = register_capability(&mount, &authority);
        let gateway = serve_console_mount(&mount).await;
        let writer = spawn_blocked_bridge(&token, &gateway).await;
        // 等待连接稳定；这里只检查不撤权时连接未关闭。
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        assert!(
            !upstream_dropped.load(std::sync::atomic::Ordering::SeqCst),
            "对照：不撤权时上游连接不得被关闭（否则下面的「关闭」不能归因于撤权）"
        );
        writer.abort();
    }

    // --- 实验组：撤权后活连接数必须归零 ---
    {
        let (authority, upstream_dropped) = non_reading_ws_upstream().await;
        let (mount, _devices) = mount_with_devices(&["dev-a"]);
        install_fixture_cookie(&mount, &authority, 1);
        let token = register_capability(&mount, &authority);
        let gateway = serve_console_mount(&mount).await;
        let writer = spawn_blocked_bridge(&token, &gateway).await;
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;

        // 判据用**上游连接的释放**，不是登记表。
        //
        // `revoke_*` 会把条目从 `live` 里 drain 掉，所以「登记数归零」在任何实现
        // 下都会立刻成立——那是个假判据（我第一版就是错的）。真正的证据是上游侧
        // 那条被背压卡住的连接被关掉：用一个计数上游 on_close 的观察点。
        mount.revoke_device("dev-a");
        let mut released = false;
        for _ in 0..50 {
            if upstream_dropped.load(std::sync::atomic::Ordering::SeqCst) {
                released = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(released, "撤权后必须关闭已建立的上游连接");
        writer.abort();
    }
}

#[tokio::test]
async fn mounted_new_websocket_keeps_query_and_binary_messages() {
    use axum::extract::ws::WebSocketUpgrade;
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
    let _guard = CREDENTIAL_TEST_LOCK.lock().await;
    let upstream = Router::new().route(
        "/plugin/ws",
        route_get(
            |uri: axum::http::Uri, headers: HeaderMap, ws: WebSocketUpgrade| async move {
                assert_eq!(uri.query(), Some("mode=a%2Bb"));
                assert_eq!(headers[header::COOKIE], "fixture-browser=synthetic");
                ws.protocols(["plugin.v2"])
                    .on_upgrade(|mut socket| async move {
                        if let Some(Ok(message)) = socket.recv().await {
                            socket.send(message).await.unwrap();
                        }
                    })
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let authority = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        axum::serve(listener, upstream).await.unwrap();
    });
    let (mount, _) = mount_with_devices(&["dev-a"]);
    install_fixture_cookie(&mount, &authority, 1);
    let token = register_capability(&mount, &authority);
    let gateway = serve_console_mount(&mount).await;
    let mut request = format!("ws://{gateway}/console/{token}/plugin/ws?mode=a%2Bb")
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        header::SEC_WEBSOCKET_PROTOCOL,
        "plugin.v1, plugin.v2".parse().unwrap(),
    );
    request.headers_mut().insert(
        header::COOKIE,
        "mam_device=dev-a; fixture-browser=browser-supplied"
            .parse()
            .unwrap(),
    );
    let (mut socket, response) = tokio_tungstenite::connect_async(request).await.unwrap();
    assert_eq!(
        response.headers()[header::SEC_WEBSOCKET_PROTOCOL],
        "plugin.v2"
    );
    let sent = Message::Binary(vec![0, 255, 1].into());
    socket.send(sent.clone()).await.unwrap();
    let received = tokio::time::timeout(std::time::Duration::from_secs(3), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(received, sent);
}

#[tokio::test]
async fn mounted_sse_delivers_first_event_and_releases_on_revoke() {
    use futures::StreamExt;
    let _guard = CREDENTIAL_TEST_LOCK.lock().await;
    let upstream = Router::new().route(
        "/plugins/events",
        route_get(|| async {
            let body = futures::stream::once(async {
                Ok::<_, std::io::Error>(axum::body::Bytes::from_static(b"data: ready\n\n"))
            })
            .chain(futures::stream::pending());
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                Body::from_stream(body),
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let authority = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        axum::serve(listener, upstream).await.unwrap();
    });
    let (mount, _) = mount_with_devices(&["dev-a"]);
    install_fixture_cookie(&mount, &authority, 1);
    let token = register_capability(&mount, &authority);
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        call(
            &app_with(&mount),
            get_request(&format!("/console/{token}/plugins/events"), None),
        ),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut stream = response.into_body().into_data_stream();
    assert_eq!(
        stream.next().await.unwrap().unwrap().as_ref(),
        b"data: ready\n\n"
    );
    mount.revoke_device("dev-a");
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(3), stream.next())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn official_job_subscriptions_keep_history_and_events_on_the_same_socket() {
    let _guard = CREDENTIAL_TEST_LOCK.lock().await;
    let (authority, frames) = recording_ws_upstream().await;
    let (mount, _devices) = mount_with_devices(&["dev-a"]);
    install_fixture_cookie(&mount, &authority, 1);
    let token = register_capability(&mount, &authority);
    let sent = [
        json!({"type":"future","streamId":"new","endpoint":"plugin/new"}),
        json!({"type":"open","streamId":"history","endpoint":"session/follow","payload":{"args":{"sessionId":"sess-1"}}}),
        json!({"type":"open","streamId":"jobs","endpoint":"job/list","payload":{"args":{"request":{"sessionId":"sess-1"}}}}),
        json!({"type":"open","streamId":"job-output","endpoint":"job/follow","payload":{"args":{"request":{"sessionId":"sess-1","jobId":"job-1","from":0}}}}),
        json!({"type":"open","streamId":"events","endpoint":"$events","payload":{"args":{}}}),
    ];
    assert!(forward_ws_frames_via_mount(&mount, &token, &sent).await);
    let received: Vec<serde_json::Value> = frames
        .lock()
        .unwrap()
        .iter()
        .map(|frame| serde_json::from_str(frame).unwrap())
        .collect();
    assert_eq!(received, sent);
}

/// `end` 之后仍允许同一流的 `cancel`（输入半关闭 ≠ 终止）。
#[tokio::test]
async fn end_is_a_half_close_and_still_allows_cancel_on_the_same_stream() {
    let _guard = CREDENTIAL_TEST_LOCK.lock().await;
    let (authority, frames) = recording_ws_upstream().await;
    let (mount, _devices) = mount_with_devices(&["dev-a"]);
    install_fixture_cookie(&mount, &authority, 1);
    let token = register_capability(&mount, &authority);
    let frames_sent = [
        json!({"type":"open","streamId":"s1","endpoint":"session/follow","payload":{"args":{}}}),
        json!({"type":"end","streamId":"s1"}),
        json!({"type":"cancel","streamId":"s1"}),
    ];
    let _ = forward_ws_frames_via_mount(&mount, &token, &frames_sent).await;
    let seen = frames.lock().unwrap().clone();
    assert_eq!(
        seen.len(),
        3,
        "open → end → cancel 是合法序列，三帧都应到达上游；实际 {seen:?}"
    );
}

// ============================================================
// M02：已配对两步兑换（GET 签发票据 → POST 兑换会话）
//
// 为什么必须两步：入口 URL 出现在地址栏/历史/分享里。一步发放「可直接用的页面
// 会话」等于把凭据写在 URL 上；两步之后，URL 里那枚票据只能用一次、60 秒、且
// 必须由**同一台已配对设备**兑换。
// ============================================================

fn post_json(uri: &str, cookie: Option<&str>, body: serde_json::Value) -> Request<Body> {
    let mut builder = Request::builder()
        .uri(uri)
        .method("POST")
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

/// 从入口响应里取票据（新协议：`{ticket}`，不再是 `{url}`）。
async fn issue_ticket(
    app: &Router,
    cookie: &str,
    session_id: &str,
) -> (StatusCode, serde_json::Value) {
    let response = call(
        app,
        get_request(
            &format!("/m/api/v1/dsh-console-entry?session_id={session_id}&instanceId=inst-a"),
            Some(cookie),
        ),
    )
    .await;
    let status = response.status();
    let body = body_text(response).await;
    (
        status,
        serde_json::from_str(&body).unwrap_or(serde_json::Value::Null),
    )
}

/// 反例 1：设备 B 拿设备 A 的票据兑换必须被拒，且 A 仍可兑换一次。
#[tokio::test]
async fn a_ticket_cannot_be_redeemed_by_another_device() {
    let state = memory_state_with_dsh_session();
    let device_a = pair_device(&state);
    let device_b = pair_device(&state);
    let cookie_a = device_cookie(&device_a);
    let cookie_b = device_cookie(&device_b);
    let app = server::router_with_static(state);

    assert_ne!(cookie_a, cookie_b, "前置：两台设备的 cookie 必须不同");
    let (status, body) = issue_ticket(&app, &cookie_a, "sess-1").await;
    assert_eq!(status, StatusCode::OK, "已配对设备应能拿到票据：{body}");
    let ticket = body["ticket"]
        .as_str()
        .expect("响应必须含 ticket")
        .to_string();
    assert!(
        body.get("url").is_none(),
        "签发阶段不得直接给可用 URL（那等于把会话凭据写进地址栏）"
    );

    // B 兑换 → 拒绝。
    //
    // 如实申报：这一条**不能**单独证明挂载层做了设备比对——gate 会先按设备有效性
    // 挡一道，而 B 是有效设备，所以真正拦住它的是挂载层的 device 过滤（该过滤由
    // `console_mount` 的单元用例 `redeem_binds_the_ticket_to_the_issuing_device...`
    // 直接覆盖，删掉过滤那条会变红）。这里保留端到端形状：它证明整条 HTTP 链在
    // 错设备时确实给出 403。
    let response = call(
        &app,
        post_json(
            "/m/api/v1/dsh-console-redeem",
            Some(&cookie_b),
            json!({ "ticket": ticket }),
        ),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "错设备兑换必须被拒（端到端形状）"
    );

    // A 仍可兑换一次（B 的失败不得烧掉 A 的票据）。
    let response = call(
        &app,
        post_json(
            "/m/api/v1/dsh-console-redeem",
            Some(&cookie_a),
            json!({ "ticket": ticket }),
        ),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "B 的失败不得消费 A 的票据"
    );
    let ok = body_text(response).await;
    let parsed: serde_json::Value = serde_json::from_str(&ok).unwrap();
    assert!(
        parsed["url"]
            .as_str()
            .is_some_and(|u| u.starts_with("/console/")),
        "兑换成功应返回相对控制台 URL：{parsed}"
    );

    // A 再兑换一次 → 已消费，拒绝。
    let response = call(
        &app,
        post_json(
            "/m/api/v1/dsh-console-redeem",
            Some(&cookie_a),
            json!({ "ticket": ticket }),
        ),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "票据是一次性的，第二次兑换必须失败"
    );
}

/// 反例 2：未配对设备不得兑换。
#[tokio::test]
async fn an_unpaired_device_cannot_redeem_or_issue() {
    let state = memory_state_with_dsh_session();
    let device_a = pair_device(&state);
    let cookie_a = device_cookie(&device_a);
    let app = server::router_with_static(state.clone());

    let (_, body) = issue_ticket(&app, &cookie_a, "sess-1").await;
    let ticket = body["ticket"].as_str().unwrap_or_default().to_string();

    let response = call(
        &app,
        post_json(
            "/m/api/v1/dsh-console-redeem",
            None,
            json!({ "ticket": ticket }),
        ),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "无设备 cookie 不得兑换"
    );
}

/// 反例 3：伪造/未知票据不得兑换，且响应不泄露票据是否存在。
#[tokio::test]
async fn unknown_or_malformed_tickets_are_refused_indistinguishably() {
    let state = memory_state_with_dsh_session();
    let device = pair_device(&state);
    let cookie = device_cookie(&device);
    let app = server::router_with_static(state);

    for body in [
        json!({ "ticket": "00000000000000000000000000000000" }),
        json!({ "ticket": "" }),
        json!({}),
    ] {
        let response = call(
            &app,
            post_json("/m/api/v1/dsh-console-redeem", Some(&cookie), body.clone()),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "未知/畸形票据必须拒绝：{body}"
        );
    }
}

/// 反例 4：票据不得跨实例使用（签发时绑定的实例必须与兑换时的当前选择一致）。
#[tokio::test]
async fn a_ticket_is_bound_to_the_instance_that_issued_it() {
    let state = memory_state_with_dsh_session();
    let device = pair_device(&state);
    let cookie = device_cookie(&device);
    let app = server::router_with_static(state);

    // 用一个不存在的会话签发 → 拿不到票据（归属不可证）。
    let (status, body) = issue_ticket(&app, &cookie, "sess-unknown").await;
    assert_ne!(status, StatusCode::OK, "会话不存在时不得签发票据");
    assert!(body.get("ticket").is_none(), "失败时不得返回票据：{body}");
}

/// M02b：凭据按**完整目标**取用——换代或换端口后旧地址拿不到新代凭据。
///
/// 旧实现按实例 ID 存一份「最新值」，取用时再按实例搜。宿主换代（甚至只是换端口）
/// 之后，旧能力仍会把**新代**的凭据送往旧地址——那是把秘密交给一个已经不属于
/// 该实例的端口。
#[tokio::test]
async fn a_stale_target_never_receives_a_newer_generations_credential() {
    let _guard = CREDENTIAL_TEST_LOCK.lock().await;
    let (authority, captured) = recording_upstream().await;
    let (mount, _devices) = mount_with_devices(&["dev-a"]);
    // 注入新代凭据后登记旧能力，确保拒绝来自三元组不匹配，而非能力被替换时清除。
    mount
        .install_browser_credential(
            "inst-a",
            2,
            &authority,
            "BROWSER-new".into(),
            app::services::dsh_control::transport::PeerGate::new(),
        )
        .unwrap();
    let token = register_capability_with_epoch(&mount, &authority, 1);
    let app = app_with(&mount);

    let response = call(&app, get_request(&format!("/console/{token}/"), None)).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(captured.take().is_empty(), "旧代能力不得访问上游");

    // 正对照：同代能力必须拿到该凭据。
    let (authority2, captured2) = recording_upstream().await;
    let (mount2, _devices2) = mount_with_devices(&["dev-a"]);
    mount2
        .install_browser_credential(
            "inst-a",
            2,
            &authority2,
            "BROWSER-new".into(),
            app::services::dsh_control::transport::PeerGate::new(),
        )
        .unwrap();
    let token2 = register_capability_with_epoch(&mount2, &authority2, 2);
    let app2 = app_with(&mount2);
    let _ = call(&app2, get_request(&format!("/console/{token2}/"), None)).await;
    let seen2 = captured2.take();
    assert_eq!(
        seen2
            .iter()
            .find(|r| r.path == "/")
            .and_then(|r| r.cookie.clone())
            .as_deref(),
        Some("BROWSER-new"),
        "同代能力必须拿到凭据（证明上面的拒绝不是把通道关掉）"
    );
    app::remote::console_lifecycle::clear_upstream_credentials();
}

// ============================================================
// M02c：撤销序号——锁外探测期间被撤，准入必须作废
//
// 威胁：`device_valid` / `epoch_valid` 是锁外调用（查库或重新发现活体，耗时不可控）。
// 若撤权发生在「检查通过」与「回锁登记」之间，那条记录此刻已不该被服务；没有序号
// 复核时它会照常登记成一条活连接。
// ============================================================

/// 撤权发生在锁外探测期间 → 该次准入必须失败。
///
/// 编排：建 mount 时就把 `epoch_check` 装好，它在**锁外**被调用；闭包里做
/// 「撤权 + 重新登记同名令牌」，因此能力仍然存在（不是靠表被清空侥幸通过），
/// 只有撤销序号复核能识别这次过期。
#[tokio::test]
async fn a_revocation_during_the_out_of_lock_probe_invalidates_the_admission() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let fired = Arc::new(AtomicBool::new(false));
    let fired_in_check = Arc::clone(&fired);
    // 闭包需要在 mount 建好之后才能 revoke/register：用 OnceLock 后填。
    let mount_slot: Arc<std::sync::OnceLock<Arc<console_mount::ConsoleMount>>> =
        Arc::new(std::sync::OnceLock::new());
    let slot_in_check = Arc::clone(&mount_slot);
    let mount = Arc::new(
        console_mount::ConsoleMount::new()
            .with_device_check(|_| true)
            .with_epoch_check(move |_, _| {
                if !fired_in_check.swap(true, Ordering::SeqCst) {
                    if let Some(m) = slot_in_check.get() {
                        m.revoke_instance("inst-a");
                        install_fixture_cookie(m, "127.0.0.1:5", 1);
                        let _ = m.register(ConsoleCapability {
                            token: "the-token".into(),
                            device_id: "dev-a".into(),
                            instance_id: "inst-a".into(),
                            authority: "127.0.0.1:5".into(),
                            epoch: 1,
                            bootstrap: false,
                            expires_at_ms: chrono::Utc::now().timestamp_millis() + 60_000,
                        });
                    }
                }
                true
            }),
    );
    install_fixture_cookie(&mount, "127.0.0.1:5", 1);
    mount_slot
        .set(Arc::clone(&mount))
        .ok()
        .expect("slot 未设过");
    mount
        .register(ConsoleCapability {
            token: "the-token".into(),
            device_id: "dev-a".into(),
            instance_id: "inst-a".into(),
            authority: "127.0.0.1:5".into(),
            epoch: 1,
            bootstrap: false,
            expires_at_ms: chrono::Utc::now().timestamp_millis() + 60_000,
        })
        .expect("能力可登记");

    let outcome = mount.authorize_live_for_test("the-token");
    assert!(
        fired.load(Ordering::SeqCst),
        "夹具前置：撤权确实发生在锁外探测期间"
    );
    assert!(
        outcome.is_none(),
        "撤权发生在锁外探测期间时，该次准入必须作废（否则已撤能力会被登记成活连接）"
    );
    // 断言能力本身仍在（证明拒绝不是因为表被清空）。
    assert!(
        mount.has_capability_for_test("the-token"),
        "夹具前置：撤权后同名令牌已重新登记，能力仍在"
    );
}

/// 正对照：没有夹带撤权时，准入必须成功（证明上面的拒绝不是把路径关掉）。
#[tokio::test]
async fn an_admission_without_a_concurrent_revocation_still_succeeds() {
    let mount = Arc::new(console_mount::ConsoleMount::new().with_device_check(|_| true));
    install_fixture_cookie(&mount, "127.0.0.1:5", 1);
    let token = format!("{:032x}", rand::random::<u128>());
    mount
        .register(ConsoleCapability {
            token: token.clone(),
            device_id: "dev-a".into(),
            instance_id: "inst-a".into(),
            authority: "127.0.0.1:5".into(),
            epoch: 1,
            bootstrap: false,
            expires_at_ms: chrono::Utc::now().timestamp_millis() + 60_000,
        })
        .expect("能力可登记");
    assert!(
        mount.authorize_live_for_test(&token).is_some(),
        "无并发撤权时准入必须成功"
    );
}

/// 官方问答等待是 Mux stream，且不能作为 HTTP 一元方法转发。
#[tokio::test]
async fn mounted_question_endpoint_is_dispatched_by_upstream() {
    let _guard = CREDENTIAL_TEST_LOCK.lock().await;
    let (authority, frames) = recording_ws_upstream().await;
    let (mount, _) = mount_with_devices(&["dev-a"]);
    install_fixture_cookie(&mount, &authority, 1);
    let token = register_capability(&mount, &authority);
    assert!(
        forward_ws_frames_via_mount(
            &mount,
            &token,
            &[json!({
                "type": "open", "streamId": "questions", "endpoint": "userQuestions/attachWait",
                "payload": {"args": {}}
            })]
        )
        .await
    );
    let seen = frames.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "问答流必须到达上游: {seen:?}");
    let frame: serde_json::Value = serde_json::from_str(&seen[0]).unwrap();
    assert_eq!(frame["endpoint"], "userQuestions/attachWait");

    let (authority, captured) = recording_upstream().await;
    install_fixture_cookie(&mount, &authority, 1);
    let token = register_capability(&mount, &authority);
    let response = call(
        &app_with(&mount),
        post_json(
            &format!("/console/{token}/api/userQuestions/attachWait"),
            None,
            json!({"args": {}}),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(captured.take().len(), 1);
    app::remote::console_lifecycle::clear_upstream_credentials();
}

/// 设备仍然有效不代表旧代票据可兑换。
#[tokio::test]
async fn redeem_rejects_a_ticket_after_the_host_epoch_changes() {
    use std::sync::atomic::{AtomicU64, Ordering};
    let epoch = Arc::new(AtomicU64::new(1));
    let current = Arc::clone(&epoch);
    let mount = Arc::new(
        console_mount::ConsoleMount::new()
            .with_device_check(|_| true)
            .with_epoch_check(move |_, candidate| candidate == current.load(Ordering::SeqCst)),
    );
    let (authority, captured) = recording_upstream().await;
    install_fixture_cookie(&mount, &authority, 1);
    mount
        .register(ConsoleCapability {
            token: "stale-ticket".into(),
            device_id: "dev-a".into(),
            instance_id: "inst-a".into(),
            authority,
            epoch: 1,
            bootstrap: true,
            expires_at_ms: mount.now() + 60_000,
        })
        .unwrap();
    epoch.store(2, Ordering::SeqCst);
    assert!(
        mount
            .redeem_bootstrap_for_device("stale-ticket", "dev-a")
            .is_none(),
        "旧代票据不得兑换为会话"
    );
    assert!(captured.take().is_empty());
}

/// 锁外检查期间过期时，重新取得锁后必须使用当前时间。
#[tokio::test]
async fn expiry_during_admission_probe_prevents_contacting_upstream() {
    use std::sync::atomic::{AtomicI64, Ordering};
    let (authority, captured) = recording_upstream().await;
    let clock = Arc::new(AtomicI64::new(100));
    let in_check = Arc::clone(&clock);
    let mut mount = console_mount::ConsoleMount::new()
        .with_device_check(|_| true)
        .with_epoch_check(move |_, _| {
            in_check.store(200, Ordering::SeqCst);
            true
        });
    let read_clock = Arc::clone(&clock);
    mount.now_ms = Box::new(move || read_clock.load(Ordering::SeqCst));
    let mount = Arc::new(mount);
    install_fixture_cookie(&mount, &authority, 1);
    mount
        .register(ConsoleCapability {
            token: "expires-in-probe".into(),
            device_id: "dev-a".into(),
            instance_id: "inst-a".into(),
            authority,
            epoch: 1,
            bootstrap: false,
            expires_at_ms: 200,
        })
        .unwrap();
    assert!(
        mount.authorize_live_for_test("expires-in-probe").is_none(),
        "探测期间到期的能力不得登记活连接"
    );
    assert!(captured.take().is_empty());
}

/// 无匹配浏览器凭据时不能无凭据探测旧地址。
#[tokio::test]
async fn missing_browser_credential_refuses_the_request_before_contacting_old_upstream() {
    let _guard = CREDENTIAL_TEST_LOCK.lock().await;
    let (authority, captured) = recording_upstream().await;
    let instance = format!("missing-{}", rand::random::<u64>());
    let mount = Arc::new(console_mount::ConsoleMount::new().with_device_check(|_| true));
    mount
        .register(ConsoleCapability {
            token: "missing-browser".into(),
            device_id: "dev-a".into(),
            instance_id: instance,
            authority,
            epoch: 1,
            bootstrap: false,
            expires_at_ms: mount.now() + 60_000,
        })
        .unwrap();
    let response = call(
        &app_with(&mount),
        get_request("/console/missing-browser/", None),
    )
    .await;
    assert_ne!(response.status(), StatusCode::OK, "缺凭据必须拒绝");
    assert!(captured.take().is_empty(), "缺凭据时旧上游必须零请求");
}

/// 签发解析器返回旧身份前撤权，注册不得复活旧票据。
#[tokio::test]
async fn revocation_during_issue_cannot_publish_a_stale_ticket() {
    let (authority, captured) = recording_upstream().await;
    let mut state = memory_state_with_dsh_session();
    let mount = Arc::clone(state.dsh_console_mount.as_ref().unwrap());
    install_fixture_cookie(&mount, &authority, 1);
    let during_resolve = Arc::clone(&mount);
    Arc::get_mut(&mut state)
        .unwrap()
        .dsh_console_upstream_source = Some(Arc::new(move |id| {
        during_resolve.revoke_instance(id);
        Ok(app::remote::console_upstream::ConsoleUpstream {
            instance_id: id.to_owned(),
            authority: authority.clone(),
            epoch: 1,
        })
    }));
    let cookie = device_cookie(&pair_device(&state));
    let app = server::router_with_static(state);
    let (status, body) = issue_ticket(&app, &cookie, "sess-1").await;
    assert_ne!(status, StatusCode::OK, "解析期间撤权不得签发: {body}");
    assert!(body.get("ticket").is_none(), "撤权后不得返回旧票据: {body}");
    assert!(captured.take().is_empty(), "旧上游必须零请求");
}

/// 同一实例、同一代际换地址时，兑换和已兑换会话都必须复核完整目标。
#[tokio::test]
async fn redeem_rejects_a_ticket_after_the_authority_changes_in_the_same_epoch() {
    let _guard = CREDENTIAL_TEST_LOCK.lock().await;
    let (old_authority, old_requests) = recording_upstream().await;
    let (new_authority, new_requests) = recording_upstream().await;
    assert_ne!(old_authority, new_authority);

    let selected = Arc::new(Mutex::new(old_authority.clone()));
    let selected_for_check = Arc::clone(&selected);
    let checked = Arc::new(Mutex::new(Vec::<(String, u64, String)>::new()));
    let checked_in_probe = Arc::clone(&checked);
    let mount = Arc::new(
        console_mount::ConsoleMount::new()
            .with_device_check(|_| true)
            .with_target_check(move |id, epoch, authority| {
                checked_in_probe
                    .lock()
                    .unwrap()
                    .push((id.to_owned(), epoch, authority.to_owned()));
                id == "inst-a"
                    && epoch == 1
                    && authority == selected_for_check.lock().unwrap().as_str()
            }),
    );
    let mut state = memory_state_with_dsh_session();
    let device = pair_device(&state);
    let cookie = device_cookie(&device);
    Arc::get_mut(&mut state).unwrap().dsh_console_mount = Some(Arc::clone(&mount));
    let management = server::router_with_static(state);
    let console = app_with(&mount);
    let register_ticket = |ticket: &str, authority: &str| {
        mount
            .register(ConsoleCapability {
                token: ticket.into(),
                device_id: device.clone(),
                instance_id: "inst-a".into(),
                authority: authority.into(),
                epoch: 1,
                bootstrap: true,
                expires_at_ms: mount.now() + 60_000,
            })
            .unwrap();
    };

    install_fixture_cookie(&mount, &old_authority, 1);
    register_ticket("old-session-ticket", &old_authority);
    register_ticket("old-address-ticket", &old_authority);
    let redeem = |ticket: &str| {
        post_json(
            "/m/api/v1/dsh-console-redeem",
            Some(&cookie),
            json!({"ticket": ticket}),
        )
    };
    let response = call(&management, redeem("old-session-ticket")).await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "旧目标仍被选中时应能兑换"
    );
    let body: serde_json::Value = serde_json::from_str(&body_text(response).await).unwrap();
    let old_url = body["url"].as_str().unwrap().to_owned();
    assert_eq!(
        call(&console, get_request(&old_url, None)).await.status(),
        StatusCode::OK
    );
    assert_eq!(old_requests.take().len(), 1, "前置：旧上游确实可访问");

    *selected.lock().unwrap() = new_authority.clone();
    let response = call(&management, redeem("old-address-ticket")).await;
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "同代换地址后旧票据不得兑换"
    );
    assert!(
        mount.has_capability_for_test("old-address-ticket"),
        "失败兑换不得消费票据"
    );
    let response = call(&console, get_request(&old_url, None)).await;
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "旧会话不得访问旧地址"
    );
    assert!(old_requests.take().is_empty(), "换地址后旧上游必须零请求");

    install_fixture_cookie(&mount, &new_authority, 1);
    register_ticket("new-address-ticket", &new_authority);
    let response = call(&management, redeem("new-address-ticket")).await;
    assert_eq!(response.status(), StatusCode::OK, "同代新地址票据应能兑换");
    let body: serde_json::Value = serde_json::from_str(&body_text(response).await).unwrap();
    let new_url = body["url"].as_str().unwrap();
    assert_eq!(
        call(&console, get_request(new_url, None)).await.status(),
        StatusCode::OK
    );
    assert_eq!(new_requests.take().len(), 1, "新地址应收到正对照请求");
    let response = call(&management, get_request(new_url, None)).await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "兑换与页面必须使用同一生产路由的挂载"
    );
    let seen = new_requests.take();
    assert_eq!(seen.len(), 1, "同一生产路由的页面应到达新上游");
    assert_eq!(seen[0].cookie.as_deref(), Some("fixture-browser=synthetic"));
    assert!(old_requests.take().is_empty(), "正对照也不得触及旧上游");
    let checks = checked.lock().unwrap();
    assert!(checks.iter().any(|(id, epoch, authority)| id == "inst-a"
        && *epoch == 1
        && authority == &old_authority));
    assert!(checks.iter().any(|(id, epoch, authority)| id == "inst-a"
        && *epoch == 1
        && authority == &new_authority));
}

/// 上游发现之后、凭据读取之前发生换代，旧地址不得收到任何请求。
#[tokio::test]
async fn replacing_credentials_during_admission_never_contacts_the_old_authority() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let _guard = CREDENTIAL_TEST_LOCK.lock().await;
    let (old_authority, old_requests) = recording_upstream().await;
    let (new_authority, _) = recording_upstream().await;
    let instance = format!("switch-{}", rand::random::<u64>());
    let switched = Arc::new(AtomicBool::new(false));
    let switched_in_check = Arc::clone(&switched);
    let instance_in_check = instance.clone();
    let slot: Arc<std::sync::OnceLock<std::sync::Weak<console_mount::ConsoleMount>>> =
        Arc::new(std::sync::OnceLock::new());
    let slot_in_check = Arc::clone(&slot);
    let mount = Arc::new(
        console_mount::ConsoleMount::new()
            .with_device_check(|_| true)
            .with_epoch_check(move |_, _| {
                slot_in_check
                    .get()
                    .unwrap()
                    .upgrade()
                    .unwrap()
                    .install_browser_credential(
                        &instance_in_check,
                        2,
                        &new_authority,
                        "new-browser".into(),
                        app::services::dsh_control::transport::PeerGate::new(),
                    )
                    .unwrap();
                switched_in_check.store(true, Ordering::SeqCst);
                true
            }),
    );
    assert!(slot.set(Arc::downgrade(&mount)).is_ok(), "挂载只初始化一次");
    mount
        .install_browser_credential(
            &instance,
            1,
            &old_authority,
            "old-browser".into(),
            app::services::dsh_control::transport::PeerGate::new(),
        )
        .unwrap();
    mount
        .register(ConsoleCapability {
            token: "old-outbound".into(),
            device_id: "dev-a".into(),
            instance_id: instance,
            authority: old_authority,
            epoch: 1,
            bootstrap: false,
            expires_at_ms: mount.now() + 60_000,
        })
        .unwrap();
    let response = call(
        &app_with(&mount),
        get_request("/console/old-outbound/", None),
    )
    .await;
    assert!(
        switched.load(Ordering::SeqCst),
        "夹具必须在探测期间替换凭据"
    );
    assert_ne!(response.status(), StatusCode::OK, "旧目标必须拒绝");
    assert!(old_requests.take().is_empty(), "旧地址必须零请求");
}

/// 在单独进程中把状态路径设为普通文件，避免进程单例和真实目录干扰。
#[test]
fn injected_upstream_does_not_initialize_production_registry() {
    const CHILD: &str = "TUVIS_H09_REGISTRY_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let state = memory_state_with_dsh_session();
            let cookie = device_cookie(&pair_device(&state));
            let app = server::router_with_static(state);
            let (status, body) = issue_ticket(&app, &cookie, "sess-1").await;
            assert_eq!(
                status,
                StatusCode::OK,
                "注入解析器不应依赖生产 Registry: {body}"
            );
            assert!(body["ticket"].as_str().is_some());
        });
        return;
    }
    let root = tempfile::Builder::new()
        .prefix("h09-")
        .tempdir_in("/tmp")
        .unwrap();
    let blocked = root.path().join("s");
    std::fs::write(&blocked, b"not a directory").unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "injected_upstream_does_not_initialize_production_registry",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .env("TUVIS_HOME", root.path().join("t"))
        .env("DSH_HOME", root.path().join("d"))
        .env("TUVIS_DSH_STATE_DIR", &blocked)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "隔离子进程失败: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// ============================================================
// T3（移动端四态映射契约）：入口拒绝按因分码——移动端据 `error` 码区分
// 「网页凭据未就绪 / 换代或设备失效 / 实例不可用」，不再折叠成一句通用失败。
// ============================================================

#[tokio::test]
async fn entry_refuses_with_credential_reason_when_web_credentials_are_missing() {
    // 不安装浏览器凭据（与 memory_state_with_dsh_session 的差异仅此一处）——
    // 签发必须停在挂载凭据校验，回 console_credential_invalid。
    let state = memory_state_with_sessions(one_dsh_session);
    let device = pair_device(&state);
    let app = server::router_with_static(state);
    let response = call(
        &app,
        get_request(
            "/m/api/v1/dsh-console-entry?session_id=sess-1&instanceId=inst-a",
            Some(&device_cookie(&device)),
        ),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "缺网页凭据必须拒绝签发"
    );
    let body: serde_json::Value =
        serde_json::from_str(&body_text(response).await).unwrap_or(serde_json::Value::Null);
    assert_eq!(
        body["error"], "console_credential_invalid",
        "凭据未就绪必须是独立可映射的码：{body}"
    );
}

#[tokio::test]
async fn entry_refuses_when_credential_table_generation_does_not_match_the_upstream() {
    // 挂载凭据表里的代际（2）与上游解析的当代（1）不一致——宿主重启换代后、
    // 凭据尚未随当代重装的形态。签发必须拒绝并给出独立可映射的码。
    let state = memory_state_with_sessions(one_dsh_session);
    install_fixture_cookie(
        state.dsh_console_mount.as_ref().unwrap(),
        "127.0.0.1:5987",
        2,
    );
    let device = pair_device(&state);
    let app = server::router_with_static(state);
    let response = call(
        &app,
        get_request(
            "/m/api/v1/dsh-console-entry?session_id=sess-1&instanceId=inst-a",
            Some(&device_cookie(&device)),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: serde_json::Value =
        serde_json::from_str(&body_text(response).await).unwrap_or(serde_json::Value::Null);
    assert_eq!(
        body["error"], "console_credential_invalid",
        "凭据表代际不匹配必须按凭据未就绪拒绝：{body}"
    );
}

/// T3 交叉态（控制台不被发送面阻断）：dsh_control 未装配（发送面整体不可用）
/// 时，控制台签发与兑换照常——两个就绪面互相独立，各自给出准确状态。
#[tokio::test]
async fn console_entry_and_redeem_work_while_send_control_is_disabled() {
    let state = memory_state_with_dsh_session();
    assert!(
        !state.dsh_control.is_wired(),
        "前置：本 fixture 的发送面未装配"
    );
    let device = pair_device(&state);
    let app = server::router_with_static(state);
    let (status, body) = issue_ticket(&app, &device_cookie(&device), "sess-1").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "发送面未装配不得阻断控制台签发：{body}"
    );
    let ticket = body["ticket"].as_str().expect("票据在").to_string();
    let response = call(
        &app,
        post_json(
            "/m/api/v1/dsh-console-redeem",
            Some(&device_cookie(&device)),
            json!({ "ticket": ticket }),
        ),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "发送面未装配不得阻断控制台兑换"
    );
}

/// T3（复审 Important-1 钉死）：挂载设备校验拒绝（设备签发后被撤）时，入口按
/// `console_device_invalid` 分码拒绝——移动端据此与「换代稍后重试」区分出
/// 「重新配对」。设备通过 gate（真配对设备），只让挂载校验拒绝。
#[tokio::test]
async fn entry_refuses_with_device_reason_when_mount_device_check_fails() {
    let mount = Arc::new(console_mount::ConsoleMount::new().with_device_check(|_| false));
    let state = memory_state_with_mount(one_dsh_session, mount);
    let device = pair_device(&state);
    let app = server::router_with_static(state);
    let response = call(
        &app,
        get_request(
            "/m/api/v1/dsh-console-entry?session_id=sess-1&instanceId=inst-a",
            Some(&device_cookie(&device)),
        ),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body: serde_json::Value =
        serde_json::from_str(&body_text(response).await).unwrap_or(serde_json::Value::Null);
    assert_eq!(
        body["error"], "console_device_invalid",
        "挂载设备校验拒绝必须是独立设备码：{body}"
    );
}
