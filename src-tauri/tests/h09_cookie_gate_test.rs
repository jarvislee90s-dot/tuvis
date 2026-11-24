//! H09 §3.3：实际 gate + 内存 DeviceStore 的独立测试服务。
//!
//! 自动目标测试与人工浏览器服务分开：自动测试通过不代表浏览器隔离通过。
//! 目标测试入口（默认不启动人工浏览器服务）：
//! cargo test --manifest-path /Users/jarvis/Documents/tuvis-worktree/src-tauri/Cargo.toml --test h09_cookie_gate_test
//! 浏览器服务入口（15 分钟自动退出，也可 Ctrl+C；使用全新浏览器配置）：
//! cargo test --manifest-path /Users/jarvis/Documents/tuvis-worktree/src-tauri/Cargo.toml --test h09_cookie_gate_test browser_gate_service -- --ignored --exact --nocapture --test-threads=1
//!
//! 直接调用生产 gate、persist_device、device_cookie；不挂载生产业务 router，
//! 不调用 remote::serve / Tauri / watcher / scan_bus::start / 全局 DB / support::setup。
//! 仅测试配对端点下发 Cookie，不调用产品 PIN/限速/审计流程。
//! 所有设备保存在 DeviceStore::memory；管理处理器仅返回计数标记；WS 仅发测试文本并关闭。
//! gate 的 note_api_request 仍执行（保持实际代码）：新进程不启动扫描总线，唤醒无接收者。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use app::remote::gate;
use app::remote::pairing::{self, DeviceStore, NewDevice};
use app::remote::server::{CreateTaskHub, RemoteState, SseRegistry};
use axum::body::Body;
use axum::extract::ws::{Message, WebSocketUpgrade};
use axum::extract::{OriginalUri, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use http_body_util::BodyExt;
use multi_agents_manager_lib as app;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tower::ServiceExt;

const PAGE: &str = include_str!("../../tests/h09/mam-device-rust-gate.html");

// 第二候选与第一候选共用实际 gate、内存设备和管理计数器，均仅用于测试。
#[path = "h09_sandbox_candidate/mod.rs"]
mod sandbox_candidate;

// 任何意外注入均立即失败，绝不委派 RealInjector。
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

fn memory_state() -> Arc<RemoteState> {
    let store = DeviceStore::memory();
    assert_memory_store(&store);
    Arc::new(RemoteState {
        opencode_native: std::sync::Arc::new(
            multi_agents_manager_lib::services::opencode_native::NativeHub::new(
                multi_agents_manager_lib::remote::pairing::DeviceStore::memory(),
            ),
        ),
        session_source: Box::new(empty_sessions),
        fresh_session_source: Box::new(empty_sessions),
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
        host_source: Box::new(|| json!({"host": {"name": "h09-memory-fixture"}})),
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
        dsh_console_mount: None,
        dsh_console_upstream_source: None,
        dsh_auto_adapt_source: Arc::new(|| false),
        desktop_language: Default::default(),
    })
}

fn assert_memory_store(store: &DeviceStore) {
    assert!(
        matches!(store, DeviceStore::Owned(_)),
        "禁止全局 DeviceStore"
    );
    store.with(|conn| {
        let mut statement = conn.prepare("PRAGMA database_list").unwrap();
        let paths: Vec<String> = statement
            .query_map([], |row| row.get(2))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(!paths.is_empty());
        assert!(paths.iter().all(String::is_empty), "禁止磁盘数据库");
    });
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Origins {
    main: String,
    same_host_console: String,
    cross_site_console: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Attempt {
    run: String,
    case: String,
    method: String,
    path: String,
    host: String,
    origin: Option<String>,
    fetch_site: Option<String>,
    cookie_present: bool,
    transport: String,
    // 只在 gate 之后的假处理器上设置标记，不复刻 device_valid 判据。
    authorized: bool,
    status: u16,
}

struct Fixture {
    remote: Arc<RemoteState>,
    origins: Origins,
    attempts: Mutex<Vec<Attempt>>,
}

impl Fixture {
    fn new(origins: Origins) -> Arc<Self> {
        Arc::new(Self {
            remote: memory_state(),
            origins,
            attempts: Mutex::new(vec![]),
        })
    }

    fn evidence(&self) -> Value {
        let rows = self.attempts.lock().unwrap().clone();
        let authorized = rows.iter().filter(|row| row.authorized).count();
        let baseline_ready = rows.iter().any(|candidate| {
            ["positive-get", "positive-post", "positive-ws"]
                .iter()
                .all(|case| {
                    rows.iter()
                        .any(|row| row.run == candidate.run && row.case == *case && row.authorized)
                })
        });
        json!({
            "fixture": "actual Rust gate + in-memory DeviceStore; fake management handlers only",
            "origins": self.origins,
            "cookieTemplate": pairing::device_cookie("REDACTED"),
            "baselineReady": baseline_ready,
            "counts": {"attempted": rows.len(), "authorized": authorized, "rejected": rows.len() - authorized},
            "attempts": rows,
        })
    }

    fn pair(&self) -> String {
        // 设备令牌只进入本次内存库和 Set-Cookie，不写日志/证据 JSON。
        let id = format!("{:032x}", rand::random::<u128>());
        let device = NewDevice {
            id,
            name: "H09 temporary browser".into(),
            ua: "h09-memory-fixture".into(),
            origin_ip: "127.0.0.1".into(),
            via: "lan".into(),
            paired_at: chrono::Utc::now().timestamp_millis(),
        };
        let id = self
            .remote
            .store
            .with(|conn| pairing::persist_device(conn, &device).expect("仅写本次内存设备表"));
        pairing::device_cookie(&id)
    }
}

#[derive(Clone)]
struct Authorized;

fn mark_authorized(mut response: Response) -> Response {
    response.extensions_mut().insert(Authorized);
    response
}

async fn fake_management_counter() -> Response {
    mark_authorized((StatusCode::OK, "fake management handler only").into_response())
}

async fn fake_management_ws(ws: WebSocketUpgrade) -> Response {
    mark_authorized(ws.on_upgrade(|mut socket| async move {
        let _ = socket.send(Message::Text("h09-counter-only".into())).await;
        let _ = socket.send(Message::Close(None)).await;
    }))
}

fn header_text(headers: &HeaderMap, key: &str) -> Option<String> {
    headers
        .get(key)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

async fn record_attempt(State(fixture): State<Arc<Fixture>>, req: Request, next: Next) -> Response {
    let path = req
        .extensions()
        .get::<OriginalUri>()
        .map(|uri| uri.0.path())
        .unwrap_or_else(|| req.uri().path())
        .to_owned();
    let pieces: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let mut row = Attempt {
        run: pieces.get(4).unwrap_or(&"").to_string(),
        case: pieces.get(5).unwrap_or(&"").to_string(),
        method: req.method().to_string(),
        host: header_text(req.headers(), "host").unwrap_or_default(),
        origin: header_text(req.headers(), "origin"),
        fetch_site: header_text(req.headers(), "sec-fetch-site"),
        cookie_present: gate::extract_device(req.headers()).is_some(),
        transport: if header_text(req.headers(), "upgrade")
            .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
        {
            "ws".into()
        } else {
            "http".into()
        },
        path,
        authorized: false,
        status: 0,
    };
    let response = next.run(req).await;
    row.status = response.status().as_u16();
    row.authorized = response.extensions().get::<Authorized>().is_some();
    fixture.attempts.lock().unwrap().push(row);
    response
}

fn routes(fixture: Arc<Fixture>, main: bool) -> Router {
    let mut root = Router::new().route("/m/fixture/evidence", get(evidence_handler));
    if main {
        // 层顺序由内到外：假处理器 → 实际 gate → 计数记录；nest 与产品路径剥前缀同形。
        let api = Router::new()
            .route("/probe/{run}/{case}", any(fake_management_counter))
            .route("/ws/{run}/{case}", get(fake_management_ws))
            .fallback(|| async { StatusCode::FORBIDDEN })
            .layer(middleware::from_fn_with_state(
                fixture.remote.clone(),
                gate::gate,
            ))
            .layer(middleware::from_fn_with_state(
                fixture.clone(),
                record_attempt,
            ));
        root = root
            .nest("/m/api/v1", api)
            .route("/m/fixture/pair", post(pair_handler))
            .route("/m/fixture", get(main_page));
    } else {
        root = root.route("/m/fixture", get(console_page));
    }
    root.with_state(fixture)
}

async fn pair_handler(State(fixture): State<Arc<Fixture>>) -> Response {
    (
        [
            (header::SET_COOKIE, fixture.pair()),
            (header::CACHE_CONTROL, "no-store".to_owned()),
        ],
        "temporary in-memory device",
    )
        .into_response()
}

async fn evidence_handler(State(fixture): State<Arc<Fixture>>) -> Response {
    // 无凭据/敏感管理数据，三个测试 listener 读取同一个计数器；不添加 CORS 放行。
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(fixture.evidence()),
    )
        .into_response()
}

fn render(fixture: &Fixture, mode: &str) -> Response {
    let config = json!({
        "origins": fixture.origins,
        "mode": mode,
        "run": format!("{:032x}", rand::random::<u128>()),
        "cookieTemplate": pairing::device_cookie("REDACTED"),
    });
    (
        [(header::CACHE_CONTROL, "no-store")],
        Html(PAGE.replace("__FIXTURE_CONFIG__", &config.to_string())),
    )
        .into_response()
}

async fn main_page(State(fixture): State<Arc<Fixture>>) -> Response {
    render(&fixture, "main")
}

async fn console_page(State(fixture): State<Arc<Fixture>>) -> Response {
    render(&fixture, "console")
}

struct Running {
    fixture: Arc<Fixture>,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for Running {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Running {
    async fn start() -> Self {
        // 端口全部由 OS 分配；不做 bind→释放→重绑，避免抢占竞态。
        let main = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("主站回环监听");
        let same = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("同主机控制台监听");
        let cross = TcpListener::bind("[::1]:0")
            .await
            .expect("原生 IPv6 回环监听；不可用时明确失败，不降级成同主机");
        let fixture = Fixture::new(Origins {
            main: format!("http://{}", main.local_addr().unwrap()),
            same_host_console: format!("http://{}", same.local_addr().unwrap()),
            cross_site_console: format!("http://{}", cross.local_addr().unwrap()),
        });
        let mut tasks = vec![];
        for (listener, is_main) in [(main, true), (same, false), (cross, false)] {
            let router = routes(fixture.clone(), is_main);
            tasks.push(tokio::spawn(async move {
                axum::serve(listener, router)
                    .await
                    .expect("测试 listener 意外退出");
            }));
        }
        Self { fixture, tasks }
    }
}

fn unit_fixture() -> Arc<Fixture> {
    Fixture::new(Origins {
        main: "http://127.0.0.1:19001".into(),
        same_host_console: "http://127.0.0.1:19002".into(),
        cross_site_console: "http://[::1]:19003".into(),
    })
}

async fn call(
    f: &Arc<Fixture>,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    origin: Option<&str>,
) -> Response {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "127.0.0.1:19001");
    if let Some(cookie) = cookie {
        builder = builder.header(header::COOKIE, cookie);
    }
    if let Some(origin) = origin {
        builder = builder.header(header::ORIGIN, origin);
    }
    routes(f.clone(), true)
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

fn request_cookie(set_cookie: &str) -> &str {
    set_cookie.split(';').next().unwrap()
}

#[test]
fn temporary_device_store_has_no_disk_database() {
    let fixture = unit_fixture();
    assert_memory_store(&fixture.remote.store);
}

#[tokio::test]
async fn temporary_pairing_issues_the_production_cookie_attributes() {
    let f = unit_fixture();
    let response = call(&f, "POST", "/m/fixture/pair", None, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let cookie = response.headers()[header::SET_COOKIE].to_str().unwrap();
    assert!(cookie.starts_with("mam_device="));
    assert!(cookie.ends_with("; Path=/m; HttpOnly; SameSite=Lax; Max-Age=15552000"));
    assert!(!cookie.contains("Domain="));
    assert!(!cookie.contains("Secure"));
    assert_eq!(f.evidence()["counts"]["authorized"], 0);
}

#[tokio::test]
async fn actual_gate_rejects_a_simple_post_without_device_cookie() {
    let f = unit_fixture();
    let response = call(
        &f,
        "POST",
        "/m/api/v1/probe/absent/simple-post",
        None,
        Some(&f.origins.cross_site_console),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        f.evidence()["counts"],
        json!({"attempted": 1, "authorized": 0, "rejected": 1})
    );
}

#[tokio::test]
async fn actual_gate_allows_a_valid_temporary_device_to_reach_the_counter() {
    let f = unit_fixture();
    let cookie = f.pair();
    let response = call(
        &f,
        "GET",
        "/m/api/v1/probe/positive/positive-get",
        Some(request_cookie(&cookie)),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .as_ref(),
        b"fake management handler only"
    );
    assert_eq!(f.evidence()["counts"]["authorized"], 1);
    assert!(!f.evidence().to_string().contains(request_cookie(&cookie)));
}

#[tokio::test]
async fn actual_gate_accepts_valid_cookie_on_cross_origin_simple_post_counterexample() {
    let f = unit_fixture();
    let cookie = f.pair();
    let request = Request::builder()
        .method("POST")
        .uri("/m/api/v1/probe/cross/simple-post")
        .header(header::COOKIE, request_cookie(&cookie))
        .header(header::ORIGIN, &f.origins.same_host_console)
        .header(header::CONTENT_TYPE, "text/plain")
        .body(Body::from("counter-only"))
        .unwrap();
    let response = routes(f.clone(), true).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!response
        .headers()
        .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN));
    assert_eq!(f.evidence()["counts"]["authorized"], 1);
}

#[tokio::test]
async fn actual_gate_rejects_preflight_without_authorizing_the_counter() {
    let f = unit_fixture();
    let request = Request::builder()
        .method("OPTIONS")
        .uri("/m/api/v1/probe/preflight/json-post")
        .header(header::ORIGIN, &f.origins.same_host_console)
        .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
        .header(header::ACCESS_CONTROL_REQUEST_HEADERS, "content-type")
        .body(Body::empty())
        .unwrap();
    let response = routes(f.clone(), true).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(!response
        .headers()
        .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN));
    assert_eq!(f.evidence()["counts"]["authorized"], 0);
}

#[tokio::test]
async fn actual_gate_rejects_a_revoked_temporary_device() {
    let f = unit_fixture();
    let cookie = f.pair();
    let id = request_cookie(&cookie).split_once('=').unwrap().1;
    f.remote
        .store
        .with(|conn| pairing::revoke_device(conn, id).unwrap());
    let response = call(
        &f,
        "POST",
        "/m/api/v1/probe/revoked/simple-post",
        Some(request_cookie(&cookie)),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(f.evidence()["counts"]["authorized"], 0);
}

#[tokio::test]
async fn actual_gate_rejects_an_expired_temporary_device() {
    let f = unit_fixture();
    let cookie = f.pair();
    let id = request_cookie(&cookie).split_once('=').unwrap().1;
    let seen = chrono::Utc::now().timestamp_millis() - pairing::DEVICE_TTL_MS - 60_000;
    f.remote.store.with(|conn| {
        conn.execute(
            "UPDATE remote_devices SET last_seen_at = ?1 WHERE id = ?2",
            rusqlite::params![seen, id],
        )
        .unwrap();
    });
    let response = call(
        &f,
        "GET",
        "/m/api/v1/probe/expired/cors-get",
        Some(request_cookie(&cookie)),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(f.evidence()["counts"]["authorized"], 0);
}

#[tokio::test]
async fn another_memory_store_does_not_accept_the_first_stores_cookie() {
    let first = unit_fixture();
    let second = unit_fixture();
    let cookie = first.pair();
    let response = call(
        &second,
        "POST",
        "/m/api/v1/probe/foreign/simple-post",
        Some(request_cookie(&cookie)),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(second.evidence()["counts"]["authorized"], 0);
}

#[tokio::test]
async fn actual_gate_allows_cross_origin_ws_with_valid_temporary_cookie_counterexample() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let running = Running::start().await;
    let f = &running.fixture;
    let cookie = f.pair();
    let url = format!(
        "{}/m/api/v1/ws/live/cross-ws",
        f.origins.main.replace("http:", "ws:")
    );
    let mut request = url.into_client_request().unwrap();
    request
        .headers_mut()
        .insert(header::COOKIE, request_cookie(&cookie).parse().unwrap());
    request
        .headers_mut()
        .insert(header::ORIGIN, f.origins.same_host_console.parse().unwrap());
    let (mut socket, response) = tokio::time::timeout(
        Duration::from_secs(3),
        tokio_tungstenite::connect_async(request),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
    let _ = socket.close(None).await;
    assert_eq!(f.evidence()["counts"]["authorized"], 1);
}

#[tokio::test]
async fn actual_gate_rejects_cross_origin_ws_without_a_device_cookie() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let running = Running::start().await;
    let f = &running.fixture;
    let url = format!(
        "{}/m/api/v1/ws/absent/cross-ws",
        f.origins.main.replace("http:", "ws:")
    );
    let mut request = url.into_client_request().unwrap();
    request.headers_mut().insert(
        header::ORIGIN,
        f.origins.cross_site_console.parse().unwrap(),
    );
    let error = tokio::time::timeout(
        Duration::from_secs(3),
        tokio_tungstenite::connect_async(request),
    )
    .await
    .unwrap()
    .unwrap_err();
    match error {
        tokio_tungstenite::tungstenite::Error::Http(response) => {
            assert_eq!(response.status(), StatusCode::FORBIDDEN)
        }
        other => panic!("必须得到服务端 403，不能把连接错误当成拒绝：{other}"),
    }
    assert_eq!(
        f.evidence()["counts"],
        json!({"attempted": 1, "authorized": 0, "rejected": 1})
    );
}

#[tokio::test]
#[ignore = "人工浏览器服务；显式 --ignored --exact browser_gate_service 启动"]
async fn browser_gate_service() {
    let running = Running::start().await;
    println!(
        "H09 实际 gate 测试服务；浏览器尚未执行。使用全新配置，在同一浏览器先正对照再两个控制台。"
    );
    println!("main = {}/m/fixture", running.fixture.origins.main);
    println!(
        "same-host counterexample = {}/m/fixture",
        running.fixture.origins.same_host_console
    );
    println!(
        "cross-site candidate = {}/m/fixture",
        running.fixture.origins.cross_site_console
    );
    println!("Cookie = {}", pairing::device_cookie("REDACTED"));
    println!("15 分钟后自动退出；Ctrl+C 可提前退出。仅人工观察、退出不表示浏览器隔离通过。");
    tokio::select! {
        result = tokio::signal::ctrl_c() => result.expect("监听 Ctrl+C"),
        _ = tokio::time::sleep(Duration::from_secs(15 * 60)) => {},
    }
    println!(
        "最终服务端计数（非浏览器验收结论）：{}",
        running.fixture.evidence()
    );
}
