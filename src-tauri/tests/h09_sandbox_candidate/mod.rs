//! H09 第二候选，仅测试：可信页 + opaque sandbox + console 路径短期能力。
//! cargo test --manifest-path /Users/jarvis/Documents/tuvis-worktree/src-tauri/Cargo.toml --test h09_cookie_gate_test sandbox_candidate -- --test-threads=1
//! 人工服务：同上命令将过滤器换为 sandbox_candidate::sandbox_browser_service，追加
//! -- --ignored --exact --nocapture --test-threads=1；退出不表示浏览器验收通过。
use super::*;
use axum::extract::ws::CloseFrame;
use axum::extract::Extension;
use std::collections::HashMap;

const TTL_MS: i64 = 120_000;
const UPLOAD: &[u8] = b"h09-memory-upload-only";
const DOWNLOAD: &str = "H09 temporary download / 中文 / no real data\n";
const SHELL: &str = include_str!("../../../tests/h09/sandbox/shell.html");
const TRUSTED: &str = include_str!("../../../tests/h09/sandbox/trusted.mjs");
const CHILD: &str = include_str!("../../../tests/h09/sandbox/child.mjs");
const PROTOCOL: &str = include_str!("../../../tests/h09/sandbox/protocol.mjs");

#[derive(Clone)]
struct Grant {
    token: String,
    run: String,
    device: String,
    expires_at: i64,
    revoked: bool,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConsoleRow {
    run: String,
    kind: String,
    method: String,
    path: String,
    origin: Option<String>,
    cookie_present: bool,
    authorized: bool,
    status: u16,
}
struct Sandbox {
    main: Arc<Fixture>,
    grants: Mutex<HashMap<String, Grant>>,
    rows: Mutex<Vec<ConsoleRow>>,
}
#[derive(Clone)]
struct ConsoleHit;

impl Sandbox {
    fn new(main: Arc<Fixture>) -> Arc<Self> {
        Arc::new(Self {
            main,
            grants: Mutex::new(HashMap::new()),
            rows: Mutex::new(vec![]),
        })
    }
    fn issue(&self, device: String, run: String) -> Grant {
        let grant = Grant {
            token: format!(
                "{:032x}{:032x}",
                rand::random::<u128>(),
                rand::random::<u128>()
            ),
            run,
            device,
            expires_at: chrono::Utc::now().timestamp_millis() + TTL_MS,
            revoked: false,
        };
        self.grants
            .lock()
            .unwrap()
            .insert(grant.token.clone(), grant.clone());
        grant
    }
    fn lookup(&self, token: &str, now: i64) -> Option<Grant> {
        let grant = self.grants.lock().unwrap().get(token).cloned()?;
        if grant.revoked || now >= grant.expires_at {
            return None;
        }
        self.main
            .remote
            .store
            .with(|conn| pairing::device_valid(conn, &grant.device, now))
            .then_some(grant)
    }
    fn snapshot(&self) -> Value {
        json!({"management": self.main.evidence(), "console": self.rows.lock().unwrap().clone(),
            "ttlMs": TTL_MS, "scope": "/console/<redacted>/ fixed fixture resources only",
            "realData": false, "browserVerified": false})
    }
}

fn hit(mut response: Response) -> Response {
    response.extensions_mut().insert(ConsoleHit);
    response
}
async fn cap_guard(State(s): State<Arc<Sandbox>>, mut req: Request, next: Next) -> Response {
    let uri = req
        .extensions()
        .get::<OriginalUri>()
        .map(|v| v.0.clone())
        .unwrap_or_else(|| req.uri().clone());
    let segments: Vec<_> = uri.path().trim_start_matches('/').split('/').collect();
    let token = segments.get(1).copied().unwrap_or("");
    let resource = segments.get(2).copied().unwrap_or("");
    let grant = s.grants.lock().unwrap().get(token).cloned();
    let kind = match resource {
        "module.mjs" => "module",
        "fetch.json" => "fetch",
        "download" if uri.query() == Some("via=anchor") => "download-link",
        "download" => "download-fetch",
        other => other,
    }
    .to_owned();
    let mut row = ConsoleRow {
        run: grant.as_ref().map(|g| g.run.clone()).unwrap_or_default(),
        kind,
        method: req.method().to_string(),
        path: format!("/console/<redacted>/{resource}"),
        origin: header_text(req.headers(), "origin"),
        cookie_present: gate::extract_device(req.headers()).is_some(),
        authorized: false,
        status: 403,
    };
    let mut response = if let Some(grant) = s.lookup(token, chrono::Utc::now().timestamp_millis()) {
        req.extensions_mut().insert(grant);
        let mut response = next.run(req).await;
        // 只有已授权 console 资源响应 opaque origin；主站管理 API 完全不加此头。
        response
            .headers_mut()
            .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, "null".parse().unwrap());
        response.headers_mut().insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            "GET, POST, OPTIONS".parse().unwrap(),
        );
        response.headers_mut().insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            "content-type".parse().unwrap(),
        );
        response
            .headers_mut()
            .insert(header::VARY, "Origin".parse().unwrap());
        response
    } else {
        StatusCode::FORBIDDEN.into_response()
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
        .headers_mut()
        .insert(header::REFERRER_POLICY, "no-referrer".parse().unwrap());
    row.status = response.status().as_u16();
    row.authorized = response.extensions().get::<ConsoleHit>().is_some();
    s.rows.lock().unwrap().push(row);
    response
}

fn router(s: Arc<Sandbox>) -> Router {
    let guarded_issue = Router::new()
        .route("/m/api/v1/sandbox/{run}/issue", post(issue))
        .layer(middleware::from_fn_with_state(
            s.main.remote.clone(),
            gate::gate,
        ));
    let console = Router::new()
        .route("/index.html", get(index))
        .route("/module.mjs", get(|| async { hit(js(CHILD)) }))
        .route(
            "/fetch.json",
            get(|| async { hit(Json(json!({"fixture": "h09-console-only"})).into_response()) }),
        )
        .route("/upload", post(upload))
        .route("/download", get(download))
        .route("/ws", get(console_ws))
        .fallback(|| async { StatusCode::FORBIDDEN })
        .layer(middleware::from_fn_with_state(s.clone(), cap_guard));
    let extra = Router::new()
        .route("/m/fixture/sandbox", get(|| async { Html(SHELL) }))
        .route(
            "/m/fixture/sandbox/trusted.mjs",
            get(|| async { js(TRUSTED) }),
        )
        .route(
            "/m/fixture/sandbox/protocol.mjs",
            get(|| async { js(PROTOCOL) }),
        )
        .route("/m/fixture/sandbox/evidence", get(snapshot))
        .merge(guarded_issue)
        .nest("/console/{token}", console)
        .with_state(s.clone());
    super::routes(s.main.clone(), true).merge(extra)
}
fn js(source: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        source,
    )
        .into_response()
}
async fn issue(
    State(s): State<Arc<Sandbox>>,
    axum::extract::Path(run): axum::extract::Path<String>,
    headers: HeaderMap,
) -> Response {
    if run.len() > 64 || !run.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Some(device) = gate::extract_device(&headers) else {
        return StatusCode::FORBIDDEN.into_response();
    };
    let grant = s.issue(device, run);
    ([(header::CACHE_CONTROL, "no-store"), (header::REFERRER_POLICY, "no-referrer")], Json(json!({
        "base": format!("/console/{}/", grant.token), "expiresAt": grant.expires_at, "ttlMs": TTL_MS,
    }))).into_response()
}
async fn snapshot(State(s): State<Arc<Sandbox>>) -> Response {
    ([(header::CACHE_CONTROL, "no-store")], Json(s.snapshot())).into_response()
}
async fn index(Extension(grant): Extension<Grant>) -> Response {
    let config = json!({"run": grant.run, "expiresAt": grant.expires_at});
    // 即使将 console URL 单独打开，也由响应 CSP 强制相同 sandbox 约束。
    let html = format!(
        r#"<!doctype html><meta charset="utf-8"><title>H09 opaque probe</title>
<p>H09 内存测试子页；无真实数据。</p><button id="start">运行子页机制探测</button>
<a id="download" download href="./download?via=anchor">下载无真实数据的测试文件</a>
<pre id="child-output">未执行</pre><script id="config" type="application/json">{config}</script>
<script type="module" crossorigin="anonymous" src="./module.mjs"></script>"#
    );
    hit((
        [(
            header::CONTENT_SECURITY_POLICY,
            "sandbox allow-scripts allow-forms allow-downloads; frame-ancestors 'self'",
        )],
        Html(html),
    )
        .into_response())
}
async fn upload(req: Request) -> Response {
    let multipart = header_text(req.headers(), "content-type")
        .is_some_and(|v| v.starts_with("multipart/form-data; boundary="));
    let Ok(bytes) = axum::body::to_bytes(req.into_body(), 64 * 1024).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    let contains = bytes.windows(UPLOAD.len()).any(|chunk| chunk == UPLOAD);
    if !multipart || !contains {
        return StatusCode::BAD_REQUEST.into_response();
    }
    hit(Json(
        json!({"multipart": true, "containsExpectedFile": true, "bytesReceived": bytes.len()}),
    )
    .into_response())
}
async fn download() -> Response {
    hit(([(header::CONTENT_TYPE, "text/plain; charset=utf-8"),
        (header::CONTENT_DISPOSITION, "attachment; filename=\"h09-download.txt\"; filename*=UTF-8''h09-%E4%B8%8B%E8%BD%BD.txt")], DOWNLOAD).into_response())
}
async fn console_ws(
    State(s): State<Arc<Sandbox>>,
    Extension(grant): Extension<Grant>,
    ws: WebSocketUpgrade,
) -> Response {
    hit(ws.on_upgrade(move |mut socket| async move {
        if socket.send(Message::Text("h09-console-ready".into())).await.is_err() { return; }
        let mut check = tokio::time::interval(Duration::from_millis(100));
        loop {
            tokio::select! {
                _ = check.tick() => {
                    if s.lookup(&grant.token, chrono::Utc::now().timestamp_millis()).is_none() {
                        let _ = socket.send(Message::Close(Some(CloseFrame {code: 1008, reason: "console capability expired or revoked".into()}))).await;
                        s.rows.lock().unwrap().push(ConsoleRow {run: grant.run.clone(), kind: "ws-expired".into(), method: "WS-CLOSE".into(), path: "/console/<redacted>/ws".into(), origin: None, cookie_present: false, authorized: false, status: 1008});
                        break;
                    }
                }
                message = socket.recv() => match message {
                    Some(Ok(Message::Text(text))) => { if socket.send(Message::Text(text)).await.is_err() { break; } }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    _ => {},
                }
            }
        }
    }))
}

struct Live {
    sandbox: Arc<Sandbox>,
    task: JoinHandle<()>,
}
impl Drop for Live {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Live {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let main = Fixture::new(Origins {
            main: origin.clone(),
            same_host_console: origin.clone(),
            cross_site_console: origin,
        });
        let sandbox = Sandbox::new(main);
        let routes = router(sandbox.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, routes).await.unwrap();
        });
        Self { sandbox, task }
    }
}
fn seeded() -> (Arc<Sandbox>, Grant, String) {
    let s = Sandbox::new(unit_fixture());
    let cookie = s.main.pair();
    let device = request_cookie(&cookie)
        .split_once('=')
        .unwrap()
        .1
        .to_owned();
    let grant = s.issue(device, "sandbox-test".into());
    (s, grant, cookie)
}
async fn get_path(s: &Arc<Sandbox>, path: &str) -> Response {
    router(s.clone())
        .oneshot(
            Request::builder()
                .uri(path)
                .header(header::ORIGIN, "null")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

#[test]
fn capability_is_fixed_to_a_short_non_sliding_deadline() {
    let (s, grant, _) = seeded();
    assert!(s.lookup(&grant.token, grant.expires_at - 1).is_some());
    assert!(s.lookup(&grant.token, grant.expires_at).is_none());
    assert_eq!(
        s.grants.lock().unwrap()[&grant.token].expires_at,
        grant.expires_at
    );
}
#[tokio::test]
async fn grant_issuance_requires_the_actual_device_gate() {
    let s = Sandbox::new(unit_fixture());
    let response = router(s)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/m/api/v1/sandbox/run/issue")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}
#[tokio::test]
async fn valid_console_capability_works_without_any_browser_cookie() {
    let (s, grant, _) = seeded();
    let response = get_path(&s, &format!("/console/{}/fetch.json", grant.token)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
        "null"
    );
    assert!(!response
        .headers()
        .contains_key(header::ACCESS_CONTROL_ALLOW_CREDENTIALS));
    assert!(!s.rows.lock().unwrap()[0].cookie_present);
}
#[tokio::test]
async fn capability_cannot_be_used_as_a_main_device_cookie() {
    let (s, grant, _) = seeded();
    let response = router(s.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/m/api/v1/probe/sandbox-test/cap-as-device")
                .header(header::COOKIE, format!("mam_device={}", grant.token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(s.main.evidence()["counts"]["authorized"], 0);
}
#[tokio::test]
async fn console_capability_does_not_allow_unknown_or_management_resources() {
    let (s, grant, _) = seeded();
    assert_eq!(
        get_path(&s, &format!("/console/{}/m/api/v1/settings", grant.token))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
}
#[tokio::test]
async fn expired_console_capability_rejects_resources() {
    let (s, grant, _) = seeded();
    s.grants
        .lock()
        .unwrap()
        .get_mut(&grant.token)
        .unwrap()
        .expires_at = 0;
    assert_eq!(
        get_path(&s, &format!("/console/{}/fetch.json", grant.token))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
}
#[tokio::test]
async fn revoked_device_invalidates_its_console_capability() {
    let (s, grant, _) = seeded();
    s.main
        .remote
        .store
        .with(|conn| pairing::revoke_device(conn, &grant.device).unwrap());
    assert_eq!(
        get_path(&s, &format!("/console/{}/fetch.json", grant.token))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
}
#[tokio::test]
async fn main_gate_rejections_do_not_receive_console_cors_headers() {
    let (s, _, _) = seeded();
    let response = get_path(&s, "/m/api/v1/probe/sandbox-test/simple-post").await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(!response
        .headers()
        .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN));
}
#[tokio::test]
async fn synthetic_multipart_upload_is_read_only_in_memory() {
    let (s, grant, _) = seeded();
    let body = "--h09\r\nContent-Disposition: form-data; name=\"file\"; filename=\"h09.txt\"\r\n\r\nh09-memory-upload-only\r\n--h09--\r\n";
    let response = router(s.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/console/{}/upload", grant.token))
                .header(header::CONTENT_TYPE, "multipart/form-data; boundary=h09")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_memory_store(&s.main.remote.store);
}
#[tokio::test]
async fn oversized_upload_is_rejected_before_authorized_handler_effect() {
    let (s, grant, _) = seeded();
    let response = router(s.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/console/{}/upload", grant.token))
                .header(header::CONTENT_TYPE, "multipart/form-data; boundary=h09")
                .body(Body::from(vec![0; 65537]))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(!s.rows.lock().unwrap()[0].authorized);
}
#[tokio::test]
async fn authenticated_trusted_page_can_issue_a_console_capability() {
    let (s, _, cookie) = seeded();
    let response = router(s.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/m/api/v1/sandbox/issue-positive/issue")
                .header(header::COOKIE, request_cookie(&cookie))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let grant: Value = serde_json::from_slice(&bytes).unwrap();
    let base = grant["base"].as_str().unwrap();
    assert!(base.starts_with("/console/"));
    assert_eq!(
        get_path(&s, &format!("{base}fetch.json")).await.status(),
        StatusCode::OK
    );
}
#[tokio::test]
async fn console_document_enforces_sandbox_without_same_origin_even_when_opened_directly() {
    let (s, grant, _) = seeded();
    let response = get_path(&s, &format!("/console/{}/index.html", grant.token)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_SECURITY_POLICY],
        "sandbox allow-scripts allow-forms allow-downloads; frame-ancestors 'self'"
    );
    assert_eq!(response.headers()[header::REFERRER_POLICY], "no-referrer");
}
#[tokio::test]
async fn console_module_is_served_as_javascript_with_opaque_origin_cors() {
    let (s, grant, _) = seeded();
    let response = get_path(&s, &format!("/console/{}/module.mjs", grant.token)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/javascript; charset=utf-8"
    );
    assert_eq!(
        response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
        "null"
    );
}
#[tokio::test]
async fn console_download_contains_only_fixture_bytes_and_attachment_headers() {
    let (s, grant, _) = seeded();
    let response = get_path(&s, &format!("/console/{}/download", grant.token)).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers()[header::CONTENT_DISPOSITION]
        .to_str()
        .unwrap()
        .starts_with("attachment;"));
    assert_eq!(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .as_ref(),
        DOWNLOAD.as_bytes()
    );
}
#[tokio::test]
async fn live_console_ws_closes_when_its_capability_expires() {
    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let live = Live::start().await;
    let s = &live.sandbox;
    let cookie = s.main.pair();
    let device = request_cookie(&cookie)
        .split_once('=')
        .unwrap()
        .1
        .to_owned();
    let grant = s.issue(device, "live-ws".into());
    let mut request = format!(
        "{}/console/{}/ws",
        s.main.origins.main.replace("http:", "ws:"),
        grant.token
    )
    .into_client_request()
    .unwrap();
    request
        .headers_mut()
        .insert(header::ORIGIN, "null".parse().unwrap());
    let (mut ws, response) = tokio::time::timeout(
        Duration::from_secs(3),
        tokio_tungstenite::connect_async(request),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
    s.grants
        .lock()
        .unwrap()
        .get_mut(&grant.token)
        .unwrap()
        .expires_at = 0;
    tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(message) = ws.next().await {
            if let tokio_tungstenite::tungstenite::Message::Close(Some(frame)) = message.unwrap() {
                assert_eq!(u16::from(frame.code), 1008);
                return;
            }
        }
        panic!("必须收到能力失效关闭帧");
    })
    .await
    .unwrap();
}
#[tokio::test]
#[ignore = "人工浏览器机制服务；测试退出不表示浏览器已通过"]
async fn sandbox_browser_service() {
    let live = Live::start().await;
    println!(
        "H09 第二候选测试：{}/m/fixture/sandbox",
        live.sandbox.main.origins.main
    );
    println!("只用全新浏览器配置和测试文件。先登录正对照，再打开可信新页；能力有效期120秒。15分钟自动退出。");
    tokio::select! {
        result = tokio::signal::ctrl_c() => result.unwrap(),
        _ = tokio::time::sleep(Duration::from_secs(900)) => {},
    }
    println!(
        "第二候选服务端记录（非浏览器验收结论）：{}",
        live.sandbox.snapshot()
    );
}
