//! T7.1 路由边界集成测试（B1 切片）：ZCode 旧 session-send 拒绝 + send-info 广告
//! 官方控制路径。
//!
//! 零真实副作用纪律：DeviceStore::Owned(共享内存库) + 桩 RemoteState；会话快照与
//! 注入器全为合成/记录型假体，绝不接触真实 ~/.tuvis，绝不发起任何官方 HTTP。

use multi_agents_manager_lib::remote::codex_ipc::CodexControlHub;
use multi_agents_manager_lib::remote::pairing::{
    persist_device, revoke_device, DeviceStore, NewDevice,
};
use multi_agents_manager_lib::remote::server::{router, RemoteState};
use multi_agents_manager_lib::remote::zcode::control_frame;
use multi_agents_manager_lib::services::zcode_control::{
    codes, set_config, ActionContext, ActionReceipt, ActionState, AuthStatus, BoundTarget,
    CapabilityFlag, CapabilityView, ConnectionState, ConnectionView, ControlAction, ControlEvent,
    ControlEventKind, DeviceRoute, HubBackend, OAuthProvider, SessionView, TargetKey, ZResult,
    ZcodeError, ZcodeHub,
};
use multi_agents_manager_lib::session::{
    AgentType, ProcessForm, Session, SessionStatus, SessionsResponse,
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tower::ServiceExt as _;

type SharedConn = Arc<Mutex<rusqlite::Connection>>;

const COOKIE: &str = "mam_device=dev-z1";
const ZCODE_SESSION: &str = "zcode-B1-0001";
const SYNTH_URL: &str = "https://zcode.z.ai/remote/v4?sid=synthetic-sid-b1-0001";

fn shared_conn() -> SharedConn {
    let conn = rusqlite::Connection::open_in_memory().expect("open memory db");
    multi_agents_manager_lib::database::schema::init(&conn);
    multi_agents_manager_lib::database::migration::migrate(&conn).expect("migrate");
    Arc::new(Mutex::new(conn))
}

fn seed_device(conn: &SharedConn) {
    let dev = NewDevice {
        id: "dev-z1".into(),
        name: "iPhone".into(),
        ua: "ua-dev-z1".into(),
        origin_ip: "127.0.0.1#z1".into(),
        via: "lan".into(),
        paired_at: chrono::Utc::now().timestamp_millis(),
    };
    DeviceStore::Owned(conn.clone()).with(|c| persist_device(c, &dev).expect("seed device"));
}

/// 记录型注入器：任何派发都会留下痕迹（B1 断言「零 CLI/终端投递」的证据面）。
#[derive(Default)]
struct RecordingInjector {
    calls: Mutex<Vec<String>>,
}

impl RecordingInjector {
    fn calls(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

impl multi_agents_manager_lib::inject::engine::Injector for RecordingInjector {
    fn locate_and_inject(&self, pid: u32, text: &str) -> Result<(), String> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("inject:{pid}:{text}"));
        Ok(())
    }

    fn locate_and_send_key_inner(&self, pid: u32, key: &str) -> Result<(), String> {
        self.calls.lock().unwrap().push(format!("key:{pid}:{key}"));
        Ok(())
    }
}

fn zcode_session() -> Session {
    Session {
        id: ZCODE_SESSION.into(),
        agent_type: AgentType::ZCode,
        project_name: "zcode-proj".into(),
        project_path: "/tmp/zcode-proj".into(),
        title: None,
        git_branch: None,
        github_url: None,
        status: SessionStatus::Idle,
        last_message: None,
        last_message_role: None,
        last_message_subagent_report: false,
        flap_from_subagent_activity: false,
        last_activity_at: "2026-10-25T00:00:00Z".into(),
        // 活 pid + CLI 形态：若路由边界失守，正常路径本会走终端直发（注入器会有记录）
        pid: 4242,
        cpu_usage: 0.0,
        active_subagent_count: 0,
        form: ProcessForm::Cli,
        jump_supported: false,
        unread: false,
    }
}

fn state(conn: &SharedConn, injector: Arc<RecordingInjector>) -> Arc<RemoteState> {
    state_with_sessions(conn, injector, vec![zcode_session()])
}

fn state_with_sessions(
    conn: &SharedConn,
    injector: Arc<RecordingInjector>,
    sessions: Vec<Session>,
) -> Arc<RemoteState> {
    state_with_hidden(conn, injector, sessions, Vec::new())
}

/// 同上，并注入看板「叉掉」（软归档）的会话集合（`board_hidden_ids`）。
fn state_with_hidden(
    conn: &SharedConn,
    injector: Arc<RecordingInjector>,
    sessions: Vec<Session>,
    hidden: Vec<String>,
) -> Arc<RemoteState> {
    Arc::new(RemoteState {
        ui_config_source: Box::new(|| None),
        subagent_source: std::collections::HashMap::new(),
        subagent_message_source: std::collections::HashMap::new(),
        capability_table: multi_agents_manager_lib::inject::capability::new_table(),
        session_source: Box::new(move || SessionsResponse {
            total_count: sessions.len(),
            sessions: sessions.clone(),
            waiting_count: 0,
        }),
        pairing_counter: Box::new(Vec::new),
        board_hidden_ids: Box::new(move || hidden.clone()),
        board_hidden_hide: Arc::new(|_| 0usize),
        board_hidden_unhide: Arc::new(|_| 0usize),
        unread_mark_read: Arc::new(|_, _| ()),
        session_close: Arc::new(|_| Ok(())),
        store: DeviceStore::Owned(conn.clone()),
        host_source: Box::new(|| serde_json::json!({ "host": { "name": "test-host" } })),
        message_source: Box::new(|_, _, _| Err("测试桩：无内容".to_string())),
        path_source: Box::new(|_, _, _| (Vec::new(), false)),
        watcher_tx: tokio::sync::broadcast::channel(64).0,
        sse_registry: Arc::new(multi_agents_manager_lib::remote::server::SseRegistry::default()),
        max_devices_source: Box::new(|| 3),
        pin_limiter: Mutex::new(multi_agents_manager_lib::remote::pin::PinRateLimiter::new()),
        global_pin_limiter: Mutex::new(
            multi_agents_manager_lib::remote::pin::PinRateLimiter::global(),
        ),
        pin_source: Box::new(|| Some("1234".to_string())),
        now_source: Box::new(|| chrono::Utc::now().timestamp_millis()),
        via_hosts_source: Box::new(|| None),
        rate_bucket_channels_source: Box::new(Vec::new),
        home_source: Box::new(|| None),
        injector,
        resume_spawner: Arc::new(|_: &multi_agents_manager_lib::inject::resume::SpawnSpec| Ok(())),
        create_hub: Arc::new(multi_agents_manager_lib::remote::server::CreateTaskHub::stub()),
        archive_source: Box::new(Vec::new),
        archive_delete: Arc::new(|_: Option<&str>| 0usize),
        confirm_probe: Arc::new(|_, _, _| true),
        dialog_probe: Arc::new(|_, _| None),
        screen_probe: Arc::new(|_, _| None),
        codex_control: Arc::new(CodexControlHub::disabled_with_store(DeviceStore::Owned(
            conn.clone(),
        ))),
        desktop_language: Default::default(),
    })
}

fn get_req(uri: &str) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::builder()
        .method("GET")
        .uri(uri)
        .header("cookie", COOKIE)
        .body(axum::body::Body::empty())
        .unwrap()
}

fn send_req(session_id: &str, text: &str) -> axum::http::Request<axum::body::Body> {
    let body = serde_json::json!({ "sessionId": session_id, "text": text }).to_string();
    axum::http::Request::builder()
        .method("POST")
        .uri("/m/api/v1/session-send")
        .header("cookie", COOKIE)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body))
        .unwrap()
}

async fn body_json(resp: axum::http::Response<axum::body::Body>) -> serde_json::Value {
    use http_body_util::BodyExt as _;
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).expect("JSON body")
}

/// B1 红测 1：旧 session-send 对 ZCode 恒 409 `official_control_required`，在路由/注入
/// 之前短路——注入器零派发、队列零入队、审计零新增。
#[tokio::test]
async fn old_session_send_returns_409_official_control_required_zero_cli() {
    let conn = shared_conn();
    seed_device(&conn);
    let injector = Arc::new(RecordingInjector::default());
    let app = router(state(&conn, injector.clone()));

    let resp = app
        .oneshot(send_req(ZCODE_SESSION, "合成正文"))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::CONFLICT);
    assert_eq!(
        body_json(resp).await,
        serde_json::json!({ "error": "official_control_required" })
    );

    // 零 CLI/终端投递：注入器无任何 inject/key 调用
    assert_eq!(injector.calls(), 0, "ZCode 旧入口不得发生任何注入派发");
    // 零入队：边界在入队之前
    let pending = DeviceStore::Owned(conn.clone()).with(|c| {
        multi_agents_manager_lib::database::dao::inject_queue::pending_for_session_conn(
            c,
            ZCODE_SESSION,
        )
    });
    assert!(pending.is_empty(), "ZCode 消息不得进入注入队列");
}

/// B1 红测 2：send-info 对 ZCode 广告官方控制路径（不可注入），携带已保存配置状态；
/// 无论配置有无，都不得宣称「已连接/已授权」。
#[tokio::test]
async fn send_info_advertises_official_control_without_connected() {
    let conn = shared_conn();
    seed_device(&conn);
    let injector = Arc::new(RecordingInjector::default());
    let app = router(state(&conn, injector.clone()));
    let route = format!("/m/api/v1/session-send-info?session_id={ZCODE_SESSION}");

    // 未配置：officialControl=false，仍是 official_control_required
    let resp = app.clone().oneshot(get_req(&route)).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["injectable"], false);
    assert_eq!(body["reasonCode"], "official_control_required");
    assert_eq!(body["officialControl"], false);
    assert_eq!(body["configState"], "missing");
    assert!(body.get("connected").is_none(), "不得宣称已连接");

    // 已保存链接：officialControl=true + 配置状态原样下发，但仍不可注入
    set_config(&conn.lock().unwrap(), SYNTH_URL).expect("save synthetic url");
    let resp = app.clone().oneshot(get_req(&route)).await.unwrap();
    let body = body_json(resp).await;
    assert_eq!(body["injectable"], false, "保存链接≠可经旧入口注入");
    assert_eq!(body["reasonCode"], "official_control_required");
    assert_eq!(body["officialControl"], true);
    assert_eq!(body["configState"], "saved");
    assert_eq!(body["url"], SYNTH_URL);
    assert!(body.get("connected").is_none(), "不得宣称已连接");
    assert_eq!(injector.calls(), 0);
}

// ============================================================
// T7.1 B2a：配对视角的 ZCode GET 投影（connection / session）
// ============================================================

/// 记录型 Hub 后端：合成连接/投影，绝不发起网络；绑定调用逐条留痕
/// （"403 先于 hub 调用"与"workspacePath 不参与绑定"的证据面）。
#[derive(Default)]
struct FakeHubBackend {
    connect_calls: AtomicUsize,
    /// T8 抢回观察：`disconnect` 被调次数（quiesce 两段语义与 hub 单测同口径）。
    disconnect_calls: AtomicUsize,
    /// T8 抢回观察：`authorize` 被调次数（零 OAuth 断言面）。
    authorize_calls: AtomicUsize,
    /// T8 抢回替身：`auth_status` 是否已授权（默认未授权；抢回重连前由用例置位）。
    auth_authorized: AtomicBool,
    /// T8 抢回替身：下一次 `connect` 的合成结果（`None` = 默认 paired 视图）。
    connect_result: Mutex<Option<ZResult<ConnectionView>>>,
    bind_calls: Mutex<Vec<TargetKey>>,
    execute_calls: AtomicUsize,
    contexts: Mutex<Vec<ActionContext>>,
    actions: Mutex<Vec<ControlAction>>,
    execute_result: Mutex<Option<ZResult<ActionReceipt>>>,
}

impl FakeHubBackend {
    fn connect_calls(&self) -> usize {
        self.connect_calls.load(Ordering::SeqCst)
    }

    fn disconnect_calls(&self) -> usize {
        self.disconnect_calls.load(Ordering::SeqCst)
    }

    fn authorize_calls(&self) -> usize {
        self.authorize_calls.load(Ordering::SeqCst)
    }

    fn set_auth_authorized(&self, authorized: bool) {
        self.auth_authorized.store(authorized, Ordering::SeqCst);
    }

    /// 设定下一次 `connect` 的合成结果（抢回用例先 conflict 后 paired）。
    fn set_connect_result(&self, result: ZResult<ConnectionView>) {
        self.connect_result.lock().unwrap().replace(result);
    }

    fn bind_calls(&self) -> Vec<TargetKey> {
        self.bind_calls.lock().unwrap().clone()
    }

    fn execute_calls(&self) -> usize {
        self.execute_calls.load(Ordering::SeqCst)
    }

    fn contexts(&self) -> Vec<ActionContext> {
        self.contexts.lock().unwrap().clone()
    }

    fn set_execute_result(&self, result: ZResult<ActionReceipt>) {
        self.execute_result.lock().unwrap().replace(result);
    }
}

impl HubBackend for FakeHubBackend {
    fn authorize<'a>(
        &'a self,
        _provider: OAuthProvider,
        _open: multi_agents_manager_lib::services::zcode_control::OpenAuthorize,
    ) -> futures::future::BoxFuture<'a, ZResult<()>> {
        self.authorize_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }

    fn connect<'a>(
        &'a self,
        _route: DeviceRoute,
    ) -> futures::future::BoxFuture<'a, ZResult<ConnectionView>> {
        self.connect_calls.fetch_add(1, Ordering::SeqCst);
        let injected = self.connect_result.lock().unwrap().clone();
        Box::pin(async move {
            injected.unwrap_or_else(|| {
                Ok(ConnectionView {
                    state: ConnectionState::Paired,
                    generation: 1,
                    reason_code: None,
                    device_label: Some("fake-device".into()),
                })
            })
        })
    }

    fn disconnect<'a>(&'a self) -> futures::future::BoxFuture<'a, ()> {
        self.disconnect_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {})
    }

    fn auth_status(&self) -> AuthStatus {
        AuthStatus {
            authorized: self.auth_authorized.load(Ordering::SeqCst),
            ..Default::default()
        }
    }

    fn reset<'a>(&'a self, _generation: u64) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(async {})
    }

    fn bind_session<'a>(
        &'a self,
        key: &'a TargetKey,
    ) -> futures::future::BoxFuture<'a, ZResult<BoundTarget>> {
        self.bind_calls.lock().unwrap().push(key.clone());
        let session_id = key.session_id.clone();
        Box::pin(async move {
            Ok(BoundTarget {
                session_id,
                workspace_path: "/synthetic/workspace".into(),
                workspace_identity: None,
                workspace_key: "/synthetic/workspace".into(),
                generation: 1,
                bridge_generation: None,
            })
        })
    }

    fn session<'a>(
        &'a self,
        target: &'a BoundTarget,
    ) -> futures::future::BoxFuture<'a, ZResult<SessionView>> {
        let view = SessionView {
            session_id: target.session_id.clone(),
            generation: target.generation,
            revision: Some(1),
            ready: true,
            pending: Vec::new(),
            control: None,
            selection: None,
            capabilities: CapabilityView::default(),
        };
        Box::pin(async move { Ok(view) })
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a ActionContext,
        target: &'a BoundTarget,
        action: &'a ControlAction,
    ) -> futures::future::BoxFuture<'a, ZResult<ActionReceipt>> {
        self.execute_calls.fetch_add(1, Ordering::SeqCst);
        self.contexts.lock().unwrap().push(ctx.clone());
        self.actions.lock().unwrap().push(action.clone());
        let receipt = ActionReceipt {
            action_id: ctx.action_id.clone(),
            session_id: target.session_id.clone(),
            generation: action.generation(),
            state: ActionState::Accepted,
            reason_code: None,
        };
        let injected = self.execute_result.lock().unwrap().clone();
        Box::pin(async move { injected.unwrap_or(Ok(receipt)) })
    }

    fn policy(&self) -> CapabilityView {
        CapabilityView::default()
    }
}

/// 合成配对 Hub：保存格式合法的合成链接 → fake 授权 → fake 连接（零网络零账号）。
async fn paired_hub(conn: &SharedConn, backend: Arc<FakeHubBackend>) -> Arc<ZcodeHub> {
    paired_hub_with(conn, backend, CapabilityView::default()).await
}

async fn paired_hub_with(
    conn: &SharedConn,
    backend: Arc<FakeHubBackend>,
    policy: CapabilityView,
) -> Arc<ZcodeHub> {
    set_config(&conn.lock().unwrap(), SYNTH_URL).expect("save synthetic url");
    let hub = Arc::new(ZcodeHub::with_parts(
        DeviceStore::Owned(conn.clone()),
        backend,
        policy,
    ));
    hub.authorize(OAuthProvider::Bigmodel, Arc::new(|_url: String| Ok(())))
        .await
        .expect("fake 授权应成功");
    hub.connect().await.expect("fake 连接应成功");
    hub
}

/// 仅开启 send 能力（其余保持默认关闭）——用于动作端点正向/错误映射用例。
fn send_policy() -> CapabilityView {
    CapabilityView {
        send: CapabilityFlag {
            enabled: true,
            reason_code: None,
        },
        ..CapabilityView::default()
    }
}

async fn paired_hub_open(conn: &SharedConn, backend: Arc<FakeHubBackend>) -> Arc<ZcodeHub> {
    paired_hub_with(conn, backend, send_policy()).await
}

/// POST /m/api/v1/zcode/actions（cookie 可选：未配对用例去掉 cookie 走 gate 403）。
fn action_req(body: &serde_json::Value, cookie: bool) -> axum::http::Request<axum::body::Body> {
    let mut builder = axum::http::Request::builder()
        .method("POST")
        .uri("/m/api/v1/zcode/actions")
        .header("content-type", "application/json");
    if cookie {
        builder = builder.header("cookie", COOKIE);
    }
    builder
        .body(axum::body::Body::from(body.to_string()))
        .unwrap()
}

/// POST /m/api/v1/zcode/takeover（T8 抢回；空请求体，cookie 可选）。
fn takeover_req(cookie: bool) -> axum::http::Request<axum::body::Body> {
    let mut builder = axum::http::Request::builder()
        .method("POST")
        .uri("/m/api/v1/zcode/takeover");
    if cookie {
        builder = builder.header("cookie", COOKIE);
    }
    builder.body(axum::body::Body::empty()).unwrap()
}

/// 合法提交形状：`{actionId, action}`，动作只携合同字段（无设备/路径字段）。
fn send_body(action_id: &str, generation: u64) -> serde_json::Value {
    serde_json::json!({
        "actionId": action_id,
        "action": {
            "kind": "send",
            "sessionId": ZCODE_SESSION,
            "generation": generation,
            "text": "合成正文",
        },
    })
}

fn get_req_without_cookie(uri: &str) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::builder()
        .method("GET")
        .uri(uri)
        .body(axum::body::Body::empty())
        .unwrap()
}

fn no_store(resp: &axum::http::Response<axum::body::Body>) -> Option<&str> {
    resp.headers()
        .get("cache-control")
        .and_then(|v| v.to_str().ok())
}

/// 未配对：两条端点都必须在 gate 层 403，hub/后端零调用（连接、绑定均不新增）。
#[tokio::test]
async fn unpaired_zcode_endpoints_return_403() {
    let conn = shared_conn();
    seed_device(&conn);
    let injector = Arc::new(RecordingInjector::default());
    let backend = Arc::new(FakeHubBackend::default());
    let hub = paired_hub(&conn, backend.clone()).await;
    let st = state(&conn, injector.clone());
    st.set_zcode_hub(hub);
    let app = router(st);

    for route in [
        "/m/api/v1/zcode/connection".to_string(),
        format!("/m/api/v1/zcode/session?sessionId={ZCODE_SESSION}"),
        format!("{EVENTS_ROUTE}?sessionId={ZCODE_SESSION}"),
    ] {
        let resp = app
            .clone()
            .oneshot(get_req_without_cookie(&route))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            axum::http::StatusCode::FORBIDDEN,
            "{route} 未配对必须 403"
        );
    }
    // T8 抢回端点同门：未配对 POST takeover 也必须 403（gate 层拦截，零 hub 调用）。
    let resp = app.clone().oneshot(takeover_req(false)).await.unwrap();
    assert_eq!(
        resp.status(),
        axum::http::StatusCode::FORBIDDEN,
        "未配对抢回必须 403"
    );
    assert!(backend.bind_calls().is_empty(), "403 必须先于 hub 绑定");
    assert_eq!(backend.connect_calls(), 1, "未配对请求不得新增连接");
    assert_eq!(backend.disconnect_calls(), 0, "未配对请求不得触发断开");
    assert_eq!(injector.calls(), 0, "零终端注入");
}

/// 已配对连接视图：200 + no-store + camelCase 四字段，正文无任何凭据形态。
#[tokio::test]
async fn paired_connection_is_no_store_and_has_no_credentials() {
    let conn = shared_conn();
    seed_device(&conn);
    let injector = Arc::new(RecordingInjector::default());
    let backend = Arc::new(FakeHubBackend::default());
    let hub = paired_hub(&conn, backend.clone()).await;
    let st = state(&conn, injector);
    st.set_zcode_hub(hub);

    let resp = router(st)
        .oneshot(get_req("/m/api/v1/zcode/connection"))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    assert_eq!(no_store(&resp), Some("no-store"), "连接视图必须 no-store");
    let body = body_json(resp).await;
    assert_eq!(body["state"], "paired");
    assert_eq!(body["deviceLabel"], "fake-device");
    assert!(body["generation"].is_u64(), "generation 为非负整数");
    let wire = body.to_string().to_lowercase();
    for needle in ["jwt", "token", "secret", "credential", "authorization"] {
        assert!(
            !wire.contains(needle),
            "连接视图不得携带凭据字段（命中 {needle}）"
        );
    }
}

/// T8 抢回（conflict）：一次断开 + 一次重连后 200 paired；no-store；响应无凭据。
#[tokio::test]
async fn takeover_conflict_reconnects_once_and_reports_paired() {
    let conn = shared_conn();
    seed_device(&conn);
    let injector = Arc::new(RecordingInjector::default());
    let backend = Arc::new(FakeHubBackend::default());
    // 初连即 conflict（官方页把 Tuvis 踢成冲突）。
    backend.set_connect_result(Ok(ConnectionView {
        state: ConnectionState::Conflict,
        generation: 1,
        reason_code: Some(codes::CONFLICT.into()),
        device_label: None,
    }));
    let hub = paired_hub(&conn, backend.clone()).await;
    assert_eq!(hub.connection().state, ConnectionState::Conflict);
    // 抢回重连成功：合成 paired 视图 + 后端授权事实（JWT 复用，零 OAuth）。
    backend.set_connect_result(Ok(ConnectionView {
        state: ConnectionState::Paired,
        generation: 2,
        reason_code: None,
        device_label: Some("fake-device".into()),
    }));
    backend.set_auth_authorized(true);
    let st = state(&conn, injector);
    st.set_zcode_hub(hub);

    let resp = router(st).oneshot(takeover_req(true)).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    assert_eq!(no_store(&resp), Some("no-store"), "抢回响应必须 no-store");
    let body = body_json(resp).await;
    assert_eq!(body["state"], "paired");
    assert_eq!(body["deviceLabel"], "fake-device");
    // 一次断开（quiesce 两段 backend.disconnect）+ 一次重连（初连 + 抢回重连）。
    assert_eq!(backend.connect_calls(), 2, "抢回只重连一次");
    assert_eq!(backend.disconnect_calls(), 2, "抢回只断开一次");
    // 响应 DTO 不携带任何凭据形态（JWT/链接不出网）。
    let wire = body.to_string().to_lowercase();
    for needle in ["jwt", "token", "secret", "credential", "authorization"] {
        assert!(
            !wire.contains(needle),
            "抢回响应不得携带凭据字段（命中 {needle}）"
        );
    }
}

/// T8 抢回（未授权）：503 `needs_auth`，零 OAuth（authorize 零调用、零连接）。
#[tokio::test]
async fn takeover_requires_auth_returns_needs_auth_zero_oauth() {
    let conn = shared_conn();
    seed_device(&conn);
    set_config(&conn.lock().unwrap(), SYNTH_URL).expect("save synthetic url");
    let backend = Arc::new(FakeHubBackend::default());
    // 已配置但未授权（未走 paired_hub 的 authorize）：Hub 构造即 needs_auth。
    let hub = Arc::new(ZcodeHub::with_parts(
        DeviceStore::Owned(conn.clone()),
        backend.clone(),
        CapabilityView::default(),
    ));
    let st = state(&conn, Arc::new(RecordingInjector::default()));
    st.set_zcode_hub(hub);

    let resp = router(st).oneshot(takeover_req(true)).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(no_store(&resp), Some("no-store"));
    assert_eq!(
        body_json(resp).await,
        serde_json::json!({ "error": "needs_auth" })
    );
    assert_eq!(backend.authorize_calls(), 0, "抢回绝不触发 OAuth");
    assert_eq!(backend.connect_calls(), 0, "未授权不得发起连接");
    assert_eq!(backend.disconnect_calls(), 0);
}

/// 未知 sessionId：快照对位失败 → 404 no_session，hub 零绑定。
#[tokio::test]
async fn zcode_session_unknown_is_404() {
    let conn = shared_conn();
    seed_device(&conn);
    let injector = Arc::new(RecordingInjector::default());
    let backend = Arc::new(FakeHubBackend::default());
    let hub = paired_hub(&conn, backend.clone()).await;
    let st = state(&conn, injector);
    st.set_zcode_hub(hub);

    let resp = router(st)
        .oneshot(get_req(
            "/m/api/v1/zcode/session?sessionId=zcode-unknown-0001",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
    assert_eq!(
        body_json(resp).await,
        serde_json::json!({ "error": "no_session" })
    );
    assert!(backend.bind_calls().is_empty(), "未知会话不得触达 hub");
}

/// 快照里存在但不是 ZCode 会话：404，且带合法 ID 也不能越权绑定。
#[tokio::test]
async fn non_zcode_session_is_404() {
    let conn = shared_conn();
    seed_device(&conn);
    let injector = Arc::new(RecordingInjector::default());
    let backend = Arc::new(FakeHubBackend::default());
    let hub = paired_hub(&conn, backend.clone()).await;
    let mut claude = zcode_session();
    claude.id = "claude-B1-0001".into();
    claude.agent_type = AgentType::Claude;
    let st = state_with_sessions(&conn, injector, vec![claude]);
    st.set_zcode_hub(hub);

    let resp = router(st)
        .oneshot(get_req("/m/api/v1/zcode/session?sessionId=claude-B1-0001"))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
    assert_eq!(
        body_json(resp).await,
        serde_json::json!({ "error": "no_session" })
    );
    assert!(backend.bind_calls().is_empty(), "非 ZCode 会话不得触达 hub");
}

/// 绑定只认服务端快照 + sessionId：workspacePath 既不能替代 sessionId，
/// 也不能改变绑定目标（hub 收到的 TargetKey 只有原 ID）。
#[tokio::test]
async fn zcode_session_does_not_accept_workspace_path() {
    let conn = shared_conn();
    seed_device(&conn);
    let injector = Arc::new(RecordingInjector::default());
    let backend = Arc::new(FakeHubBackend::default());
    let hub = paired_hub(&conn, backend.clone()).await;
    let st = state(&conn, injector);
    st.set_zcode_hub(hub);
    let app = router(st);

    // ① 只给 workspacePath（缺 sessionId）→ 400，且零绑定
    let resp = app
        .clone()
        .oneshot(get_req(
            "/m/api/v1/zcode/session?workspacePath=%2Fevil%2Fworkspace",
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
    assert!(backend.bind_calls().is_empty());

    // ② 同时提交：workspacePath 被忽略，绑定目标仍只有原 sessionId
    let route = format!(
        "/m/api/v1/zcode/session?sessionId={ZCODE_SESSION}&workspacePath=%2Fevil%2Fworkspace"
    );
    let resp = app.oneshot(get_req(&route)).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    assert_eq!(no_store(&resp), Some("no-store"));
    let body = body_json(resp).await;
    assert_eq!(body["sessionId"], ZCODE_SESSION);
    assert_eq!(
        backend.bind_calls(),
        vec![TargetKey {
            session_id: ZCODE_SESSION.into()
        }],
        "hub 只按 sessionId 绑定"
    );
    assert!(
        !body.to_string().contains("/evil"),
        "响应/绑定不得回显客户端自报 workspace"
    );
}

// ============================================================
// T7.1 B2b：配对视角的 ZCode 写动作（POST /zcode/actions）
// ============================================================

/// 动作端点与 GET 同级授权门：只接受当前 Tuvis 快照里 `agent_type=zcode` 的会话；
/// 隐藏/归档（已离开快照）或非 ZCode 的会话凭手机自报 ID → 404 no_session，
/// 零绑定零执行（不得绕过看板可见性边界）。
#[tokio::test]
async fn action_on_hidden_or_archived_zcode_session_is_404() {
    let conn = shared_conn();
    seed_device(&conn);
    let injector = Arc::new(RecordingInjector::default());
    let backend = Arc::new(FakeHubBackend::default());
    let hub = paired_hub_open(&conn, backend.clone()).await;
    let generation = hub.connection().generation;
    let mut claude = zcode_session();
    claude.id = "claude-hidden-0001".into();
    claude.agent_type = AgentType::Claude;
    let st = state_with_sessions(&conn, injector, vec![zcode_session(), claude]);
    st.set_zcode_hub(hub);
    let app = router(st);

    for target in ["zcode-archived-0001", "claude-hidden-0001"] {
        let mut body = send_body("action-hidden-0001", generation);
        body["action"]["sessionId"] = serde_json::json!(target);
        let resp = app.clone().oneshot(action_req(&body, true)).await.unwrap();
        assert_eq!(
            resp.status(),
            axum::http::StatusCode::NOT_FOUND,
            "{target} 必须 404"
        );
        assert_eq!(
            body_json(resp).await,
            serde_json::json!({ "error": "no_session" })
        );
    }
    assert_eq!(backend.execute_calls(), 0, "看不见的会话不得执行");
    assert!(backend.bind_calls().is_empty(), "看不见的会话不得绑定");
}

/// 看板已「叉掉」（软归档）的活 ZCode 会话：GET session/events 与 POST actions
/// 一律 404，零绑定零执行零事件流注册（隐藏 ≠ 可控制）。
#[tokio::test]
async fn hidden_live_zcode_session_is_404_everywhere() {
    let conn = shared_conn();
    seed_device(&conn);
    let injector = Arc::new(RecordingInjector::default());
    let backend = Arc::new(FakeHubBackend::default());
    let hub = paired_hub_open(&conn, backend.clone()).await;
    let generation = hub.connection().generation;
    let st = state_with_hidden(
        &conn,
        injector,
        vec![zcode_session()],
        vec![ZCODE_SESSION.into()],
    );
    st.set_zcode_hub(hub);
    let registry = st.sse_registry.clone();
    let app = router(st);

    // GET session：隐藏的活会话 404。
    let resp = app
        .clone()
        .oneshot(get_req(&format!(
            "/m/api/v1/zcode/session?sessionId={ZCODE_SESSION}"
        )))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
    assert_eq!(
        body_json(resp).await,
        serde_json::json!({ "error": "no_session" })
    );

    // GET events：不得注册事件流。
    let resp = app.clone().oneshot(sse_req(ZCODE_SESSION)).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
    assert!(!registry.has("dev-z1"), "隐藏会话不得注册事件流");

    // POST actions：404 且零绑定零执行。
    let resp = app
        .oneshot(action_req(
            &send_body("action-hidden-live-0001", generation),
            true,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::NOT_FOUND);
    assert_eq!(
        body_json(resp).await,
        serde_json::json!({ "error": "no_session" })
    );
    assert_eq!(backend.execute_calls(), 0, "隐藏会话不得执行");
    assert!(backend.bind_calls().is_empty(), "隐藏会话不得绑定");
}

/// brief Step 1 冻结名：四条控制端点（connection/session/actions/events）未配对
/// 一律 gate 层 403，先于任何 hub 调用；零帧（无绑定/无执行/无新增连接/无 SSE 注册）。
#[tokio::test]
async fn unpaired_all_control_endpoints_reject() {
    let conn = shared_conn();
    seed_device(&conn);
    let injector = Arc::new(RecordingInjector::default());
    let backend = Arc::new(FakeHubBackend::default());
    let hub = paired_hub_open(&conn, backend.clone()).await;
    let generation = hub.connection().generation;
    let st = state(&conn, injector.clone());
    st.set_zcode_hub(hub);
    let registry = st.sse_registry.clone();
    let app = router(st);

    for route in [
        "/m/api/v1/zcode/connection".to_string(),
        format!("/m/api/v1/zcode/session?sessionId={ZCODE_SESSION}"),
        format!("{EVENTS_ROUTE}?sessionId={ZCODE_SESSION}"),
    ] {
        let resp = app
            .clone()
            .oneshot(get_req_without_cookie(&route))
            .await
            .unwrap();
        assert_eq!(resp.status(), 403, "{route} 未配对必须 403");
    }
    let resp = app
        .clone()
        .oneshot(action_req(
            &send_body("action-unpaired-all", generation),
            false,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), 403, "未配对动作必须 403");

    // wire frames = 0：零绑定/零执行/零新增连接/零 SSE 注册（gate 先于一切 hub 面）
    assert_eq!(backend.execute_calls(), 0, "403 必须先于 hub 执行");
    assert!(backend.bind_calls().is_empty(), "403 必须先于 hub 绑定");
    assert_eq!(backend.connect_calls(), 1, "未配对请求不得新增连接");
    assert!(!registry.has("dev-z1"), "未配对不得注册任何事件流");
    assert_eq!(injector.calls(), 0, "零终端注入");
}

/// 未配对：动作端点必须在 gate 层 403，hub/后端零执行零绑定。
#[tokio::test]
async fn unpaired_action_is_403_zero_calls() {
    let conn = shared_conn();
    seed_device(&conn);
    let injector = Arc::new(RecordingInjector::default());
    let backend = Arc::new(FakeHubBackend::default());
    let hub = paired_hub_open(&conn, backend.clone()).await;
    let generation = hub.connection().generation;
    let st = state(&conn, injector.clone());
    st.set_zcode_hub(hub);

    let resp = router(st)
        .oneshot(action_req(
            &send_body("action-B2b-unpaired", generation),
            false,
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        axum::http::StatusCode::FORBIDDEN,
        "未配对动作必须 403"
    );
    assert_eq!(backend.execute_calls(), 0, "403 必须先于 hub 执行");
    assert!(backend.contexts().is_empty());
    assert!(backend.bind_calls().is_empty());
    assert_eq!(injector.calls(), 0, "零终端注入");
}

/// 设备身份只来自配对门 + 本机 DB：请求体里任何 deviceId/deviceName/capability/
/// provider 一律 400；干净提交时 Hub 收到的 ctx 仍是 DB 花名册身份。
#[tokio::test]
async fn action_body_cannot_override_device_identity() {
    let conn = shared_conn();
    seed_device(&conn);
    let injector = Arc::new(RecordingInjector::default());
    let backend = Arc::new(FakeHubBackend::default());
    let hub = paired_hub_open(&conn, backend.clone()).await;
    let generation = hub.connection().generation;
    let st = state(&conn, injector);
    st.set_zcode_hub(hub);
    let app = router(st);

    let mut spoofed = send_body("action-B2b-spoof", generation);
    let object = spoofed.as_object_mut().unwrap();
    object.insert("deviceId".into(), "dev-evil".into());
    object.insert("deviceName".into(), "Evil Phone".into());
    object.insert("device_id".into(), "dev-evil".into());
    object.insert("capability".into(), serde_json::json!({ "send": true }));
    object.insert("provider".into(), "bigmodel".into());
    let resp = app
        .clone()
        .oneshot(action_req(&spoofed, true))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(
        body_json(resp).await,
        serde_json::json!({ "error": "invalid_action" })
    );
    assert_eq!(backend.execute_calls(), 0, "越权字段不得透传到 hub");
    assert!(backend.contexts().is_empty());

    let resp = app
        .oneshot(action_req(&send_body("action-B2b-ok", generation), true))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    assert_eq!(
        backend.contexts(),
        vec![ActionContext {
            action_id: "action-B2b-ok".into(),
            device_id: "dev-z1".into(),
            device_name: "iPhone".into(),
        }],
        "ctx 身份只来自 cookie + 本机 DB"
    );
}

/// 非法动作载荷（未知 kind / 缺字段 / 缺 actionId）→ 400 invalid_action，零执行。
#[tokio::test]
async fn invalid_action_is_400() {
    let conn = shared_conn();
    seed_device(&conn);
    let injector = Arc::new(RecordingInjector::default());
    let backend = Arc::new(FakeHubBackend::default());
    let hub = paired_hub_open(&conn, backend.clone()).await;
    let generation = hub.connection().generation;
    let st = state(&conn, injector);
    st.set_zcode_hub(hub);
    let app = router(st);

    let cases = [
        serde_json::json!({
            "actionId": "action-B2b-bad-kind",
            "action": { "kind": "reboot", "sessionId": ZCODE_SESSION, "generation": generation },
        }),
        serde_json::json!({
            "actionId": "action-B2b-missing-gen",
            "action": { "kind": "send", "sessionId": ZCODE_SESSION, "text": "合成正文" },
        }),
        serde_json::json!({
            "action": {
                "kind": "send",
                "sessionId": ZCODE_SESSION,
                "generation": generation,
                "text": "合成正文",
            },
        }),
    ];
    for body in cases {
        let resp = app.clone().oneshot(action_req(&body, true)).await.unwrap();
        assert_eq!(
            resp.status(),
            axum::http::StatusCode::BAD_REQUEST,
            "{body} 必须 400"
        );
        assert_eq!(
            body_json(resp).await,
            serde_json::json!({ "error": "invalid_action" })
        );
    }
    assert_eq!(backend.execute_calls(), 0);
    assert!(backend.bind_calls().is_empty());
}

/// 旧代际：409 stale_generation，且零绑定零执行（不得在旧目标上重试）。
#[tokio::test]
async fn stale_generation_is_409() {
    let conn = shared_conn();
    seed_device(&conn);
    let injector = Arc::new(RecordingInjector::default());
    let backend = Arc::new(FakeHubBackend::default());
    let hub = paired_hub_open(&conn, backend.clone()).await;
    let stale = hub.connection().generation + 7;
    let st = state(&conn, injector);
    st.set_zcode_hub(hub);

    let resp = router(st)
        .oneshot(action_req(&send_body("action-B2b-stale", stale), true))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::CONFLICT);
    assert_eq!(
        body_json(resp).await,
        serde_json::json!({ "error": "stale_generation" })
    );
    assert_eq!(backend.execute_calls(), 0);
    assert!(backend.bind_calls().is_empty());
}

/// 能力未开闸（默认全关）：409 needs_evidence，零执行（手机不可自报能力）。
#[tokio::test]
async fn capability_closed_is_409() {
    let conn = shared_conn();
    seed_device(&conn);
    let injector = Arc::new(RecordingInjector::default());
    let backend = Arc::new(FakeHubBackend::default());
    let hub = paired_hub(&conn, backend.clone()).await;
    let generation = hub.connection().generation;
    let st = state(&conn, injector);
    st.set_zcode_hub(hub);

    let resp = router(st)
        .oneshot(action_req(
            &send_body("action-B2b-closed", generation),
            true,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::CONFLICT);
    assert_eq!(
        body_json(resp).await,
        serde_json::json!({ "error": "needs_evidence" })
    );
    assert_eq!(backend.execute_calls(), 0);
}

/// 审计/账本持久化失败：500 storage_failed，错误体只含冻结码。
#[tokio::test]
async fn storage_failure_is_500() {
    let conn = shared_conn();
    seed_device(&conn);
    let injector = Arc::new(RecordingInjector::default());
    let backend = Arc::new(FakeHubBackend::default());
    backend.set_execute_result(Err(ZcodeError {
        code: codes::STORAGE_FAILED.into(),
    }));
    let hub = paired_hub_open(&conn, backend.clone()).await;
    let generation = hub.connection().generation;
    let st = state(&conn, injector);
    st.set_zcode_hub(hub);

    let resp = router(st)
        .oneshot(action_req(
            &send_body("action-B2b-storage", generation),
            true,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    let body = body_json(resp).await;
    assert_eq!(body, serde_json::json!({ "error": "storage_failed" }));
    assert_eq!(backend.execute_calls(), 1);
    let wire = body.to_string().to_lowercase();
    for needle in ["jwt", "token", "secret", "credential"] {
        assert!(
            !wire.contains(needle),
            "错误体不得携带凭据（命中 {needle}）"
        );
    }
}

/// 受理：200 + no-store，回执按 state 区分；正文/答案不回显，无凭据字段。
#[tokio::test]
async fn accepted_receipt_is_200() {
    let conn = shared_conn();
    seed_device(&conn);
    let injector = Arc::new(RecordingInjector::default());
    let backend = Arc::new(FakeHubBackend::default());
    let hub = paired_hub_open(&conn, backend.clone()).await;
    let generation = hub.connection().generation;
    let st = state(&conn, injector);
    st.set_zcode_hub(hub);

    let resp = router(st)
        .oneshot(action_req(&send_body("action-B2b-0001", generation), true))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    assert_eq!(no_store(&resp), Some("no-store"), "动作回执必须 no-store");
    let body = body_json(resp).await;
    assert_eq!(
        body,
        serde_json::json!({
            "actionId": "action-B2b-0001",
            "sessionId": ZCODE_SESSION,
            "generation": generation,
            "state": "accepted",
            "reasonCode": null,
        }),
        "回执只含冻结字段"
    );
    assert_eq!(backend.execute_calls(), 1);
    let wire = body.to_string().to_lowercase();
    for needle in ["jwt", "token", "secret", "credential", "合成正文"] {
        assert!(
            !wire.contains(needle),
            "回执不得回显正文/凭据（命中 {needle}）"
        );
    }
}

/// workspacePath（顶层或动作内）一律 400 invalid_action：白名单是安全合同，
/// 不接受任何客户端自报字段；两种位置都零绑定零执行。
#[tokio::test]
async fn workspace_path_in_body_is_rejected_or_ignored() {
    let conn = shared_conn();
    seed_device(&conn);
    let injector = Arc::new(RecordingInjector::default());
    let backend = Arc::new(FakeHubBackend::default());
    let hub = paired_hub_open(&conn, backend.clone()).await;
    let generation = hub.connection().generation;
    let st = state(&conn, injector);
    st.set_zcode_hub(hub);
    let app = router(st);

    // ① 顶层多余字段
    let mut top = send_body("action-B2b-ws-top", generation);
    top.as_object_mut()
        .unwrap()
        .insert("workspacePath".into(), serde_json::json!("/evil/workspace"));
    // ② 动作内多余字段
    let mut inner = send_body("action-B2b-ws-inner", generation);
    inner["action"]
        .as_object_mut()
        .unwrap()
        .insert("workspacePath".into(), serde_json::json!("/evil/workspace"));

    for body in [top, inner] {
        let resp = app.clone().oneshot(action_req(&body, true)).await.unwrap();
        assert_eq!(
            resp.status(),
            axum::http::StatusCode::BAD_REQUEST,
            "契约外字段必须 400，实际 {body}"
        );
        assert_eq!(
            body_json(resp).await,
            serde_json::json!({ "error": "invalid_action" })
        );
    }
    assert_eq!(backend.execute_calls(), 0, "多余字段不得触达 hub");
    assert!(backend.bind_calls().is_empty(), "不得按自报 workspace 绑定");
}

// ============================================================
// T7.1 B3：配对视角的 ZCode 事件流（GET /zcode/events，SSE）
// ============================================================

const EVENTS_ROUTE: &str = "/m/api/v1/zcode/events";
const OTHER_SESSION: &str = "zcode-B3-0002";

/// 读一帧 SSE 数据（2s 超时；流结束返回 None）。与既有 /events 测试同一读法。
async fn next_frame(stream: &mut axum::body::BodyDataStream) -> Option<String> {
    use futures::StreamExt as _;
    let next = tokio::time::timeout(std::time::Duration::from_secs(2), stream.next())
        .await
        .expect("SSE 帧读取超时");
    next.map(|chunk| String::from_utf8(chunk.expect("SSE 帧读取失败").to_vec()).expect("UTF-8"))
}

/// 帧的 data 行 JSON（axum 固定写 `data: <json>`）。
fn frame_json(frame: &str) -> serde_json::Value {
    let data = frame
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .expect("SSE 帧必须含 data 行");
    serde_json::from_str(data).expect("data 行必须是 JSON")
}

/// 建立配对状态 + Hub 并返回 (router, hub, registry) 三元组。
async fn sse_app(
    conn: &SharedConn,
    backend: Arc<FakeHubBackend>,
) -> (
    axum::Router,
    Arc<ZcodeHub>,
    Arc<multi_agents_manager_lib::remote::server::SseRegistry>,
) {
    seed_device(conn);
    let hub = paired_hub(conn, backend).await;
    let st = state(conn, Arc::new(RecordingInjector::default()));
    st.set_zcode_hub(hub.clone());
    let registry = st.sse_registry.clone();
    (router(st), hub, registry)
}

fn sse_req(session_id: &str) -> axum::http::Request<axum::body::Body> {
    get_req(&format!("{EVENTS_ROUTE}?sessionId={session_id}"))
}

/// 首帧 = 当前连接视图 + 会话就绪快照；no-store + text/event-stream。
#[tokio::test]
async fn sse_first_frame_is_connection_state() {
    let conn = shared_conn();
    let (app, hub, _registry) = sse_app(&conn, Arc::new(FakeHubBackend::default())).await;
    let view = hub.connection();

    let resp = app.oneshot(sse_req(ZCODE_SESSION)).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/event-stream")
    );
    assert_eq!(no_store(&resp), Some("no-store"), "控制事件流必须 no-store");

    let mut stream = resp.into_body().into_data_stream();
    let first = next_frame(&mut stream).await.expect("首帧不得缺失");
    assert!(
        first.starts_with("event: snapshot\n"),
        "首帧必须是 snapshot，实际 {first:?}"
    );
    let body = frame_json(&first);
    assert_eq!(body["connection"]["state"], "paired");
    assert_eq!(body["connection"]["generation"], view.generation);
    assert_eq!(body["connection"]["deviceLabel"], "fake-device");
    assert_eq!(body["session"]["sessionId"], ZCODE_SESSION);
    assert_eq!(body["session"]["ready"], true, "会话就绪随首帧下发");
}

/// 只转发本会话事件：别的会话 session/action 事件不产帧；连接事件恒转发。
#[tokio::test]
async fn sse_filters_other_sessions() {
    let conn = shared_conn();
    let (app, hub, _registry) = sse_app(&conn, Arc::new(FakeHubBackend::default())).await;
    let resp = app.oneshot(sse_req(ZCODE_SESSION)).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let mut stream = resp.into_body().into_data_stream();
    next_frame(&mut stream).await.expect("首帧");

    // 过滤器单点（SSE 每帧都经它产出，见 remote::zcode::control_frame）
    let foreign_session = ControlEvent {
        seq: 1,
        generation: 1,
        session_id: Some(OTHER_SESSION.into()),
        kind: ControlEventKind::Session,
        data: serde_json::json!({ "sessionId": OTHER_SESSION }),
        device_id: None,
    };
    let foreign_action = ControlEvent {
        seq: 2,
        generation: 1,
        session_id: Some(OTHER_SESSION.into()),
        kind: ControlEventKind::Action,
        data: serde_json::json!({ "actionId": "action-other-session" }),
        device_id: None,
    };
    let own_session = ControlEvent {
        seq: 3,
        generation: 1,
        session_id: Some(ZCODE_SESSION.into()),
        kind: ControlEventKind::Session,
        data: serde_json::json!({ "sessionId": ZCODE_SESSION }),
        device_id: None,
    };
    assert!(control_frame(&foreign_session, ZCODE_SESSION, "dev-z1").is_none());
    assert!(control_frame(&foreign_action, ZCODE_SESSION, "dev-z1").is_none());
    assert!(matches!(
        control_frame(&own_session, ZCODE_SESSION, "dev-z1"),
        Some(("session", _))
    ));

    // 活流：首帧快照内的 session() 读取已广播一条本会话 Session 事件，端到端经
    // BroadcastStream 送达（不只是直调过滤函数）。
    let own = next_frame(&mut stream)
        .await
        .expect("本会话 session 事件帧");
    assert!(
        own.starts_with("event: session\n"),
        "本会话 session 事件必须成帧，实际 {own:?}"
    );
    assert_eq!(frame_json(&own)["data"]["sessionId"], ZCODE_SESSION);
    assert_eq!(frame_json(&own)["data"]["ready"], true);

    // 活流：Hub 广播一条连接事件 → 连接帧（非本会话事件不混入）
    hub.disconnect().await;
    let frame = next_frame(&mut stream).await.expect("连接事件帧");
    assert!(
        frame.starts_with("event: connection\n"),
        "连接事件必须转发，实际 {frame:?}"
    );
    let body = frame_json(&frame);
    assert_eq!(body["kind"], "connection");
    assert_eq!(body["data"]["state"], "offline");
    assert!(
        !frame.contains(OTHER_SESSION),
        "不得混入其它会话帧，实际 {frame:?}"
    );
}

/// 广播落后（lag）→ 只发 `resync` 帧（客户端重 GET），绝不重放动作。
#[tokio::test]
async fn sse_lag_emits_resync_not_replay() {
    let conn = shared_conn();
    let (app, hub, _registry) = sse_app(&conn, Arc::new(FakeHubBackend::default())).await;
    let resp = app.oneshot(sse_req(ZCODE_SESSION)).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let mut stream = resp.into_body().into_data_stream();
    next_frame(&mut stream).await.expect("首帧");

    // 停止读帧，制造 >64 条积压（每轮 disconnect+connect 共 3 条连接事件）
    for _ in 0..25 {
        hub.disconnect().await;
        hub.connect().await.expect("fake 重连");
    }
    let lagged = next_frame(&mut stream)
        .await
        .expect("lag 后必须有 resync 帧");
    assert!(
        lagged.starts_with("event: resync\n"),
        "lag 必须发 resync，实际 {lagged:?}"
    );
    assert!(
        !lagged.contains("actionId"),
        "resync 帧不得重放动作，实际 {lagged:?}"
    );

    // resync 之后只回放缓冲里的连接事件，绝不出现动作帧
    for _ in 0..3 {
        let frame = next_frame(&mut stream).await.expect("积压连接帧");
        assert!(
            !frame.starts_with("event: action\n"),
            "lag 后不得重放动作，实际 {frame:?}"
        );
    }
}

/// 设备吊销 → SSE 立即关闭；吊销后帧数恒为 0。
#[tokio::test]
async fn revoked_device_sse_closes_and_frames_after_revoke_are_zero() {
    let conn = shared_conn();
    let (app, _hub, registry) = sse_app(&conn, Arc::new(FakeHubBackend::default())).await;
    let resp = app.oneshot(sse_req(ZCODE_SESSION)).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let mut stream = resp.into_body().into_data_stream();
    next_frame(&mut stream).await.expect("首帧");

    let mut frames_after_revoke = 0;
    assert_eq!(registry.disconnect_device("dev-z1"), 1, "应关闭 1 条连接");
    while next_frame(&mut stream).await.is_some() {
        frames_after_revoke += 1;
    }
    assert_eq!(frames_after_revoke, 0, "吊销后不得再发任何帧");
}

/// 流内绝无凭据；别的设备在别的会话上的动作 id 不外泄。
#[tokio::test]
async fn sse_never_exposes_tokens_or_other_device_actions() {
    let conn = shared_conn();
    let (app, hub, _registry) = sse_app(&conn, Arc::new(FakeHubBackend::default())).await;
    let resp = app.oneshot(sse_req(ZCODE_SESSION)).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let mut stream = resp.into_body().into_data_stream();
    let first = next_frame(&mut stream).await.expect("首帧");
    let wire = first.to_lowercase();
    for needle in ["jwt", "token", "secret", "credential", "authorization"] {
        assert!(!wire.contains(needle), "首帧不得携带凭据（命中 {needle}）");
    }

    // 另一台手机在另一会话上的动作回执：过滤后不产帧（actionId 不外泄）
    let foreign_action = ControlEvent {
        seq: 7,
        generation: 1,
        session_id: Some(OTHER_SESSION.into()),
        kind: ControlEventKind::Action,
        data: serde_json::json!({
            "actionId": "action-other-device-0001",
            "sessionId": OTHER_SESSION,
        }),
        device_id: None,
    };
    assert!(control_frame(&foreign_action, ZCODE_SESSION, "dev-z1").is_none());

    // 同一会话、另一台手机的动作回执：按设备过滤，同样不产帧
    let other_device_same_session = ControlEvent {
        seq: 8,
        generation: 1,
        session_id: Some(ZCODE_SESSION.into()),
        kind: ControlEventKind::Action,
        data: serde_json::json!({
            "actionId": "action-other-device-0001",
            "sessionId": ZCODE_SESSION,
        }),
        device_id: Some("dev-other".into()),
    };
    assert!(
        control_frame(&other_device_same_session, ZCODE_SESSION, "dev-z1").is_none(),
        "同会话他机动作 id 不得出网"
    );

    // 活流：连接事件帧同样不得含凭据/他机动作 id
    hub.disconnect().await;
    let frame = next_frame(&mut stream).await.expect("连接帧");
    let wire = frame.to_lowercase();
    assert!(!wire.contains("action-other-device-0001"));
    for needle in ["jwt", "token", "secret", "credential", "authorization"] {
        assert!(
            !wire.contains(needle),
            "事件帧不得携带凭据（命中 {needle}）"
        );
    }
}

/// 同会话他机动作 id 不得出网：真实 Hub 广播 → SSE 端到端过滤（非直调纯函数）。
#[tokio::test]
async fn sse_does_not_leak_other_device_action_ids() {
    let conn = shared_conn();
    seed_device(&conn);
    let backend = Arc::new(FakeHubBackend::default());
    let hub = paired_hub_open(&conn, backend).await;
    let generation = hub.connection().generation;
    let st = state(&conn, Arc::new(RecordingInjector::default()));
    st.set_zcode_hub(hub.clone());
    let app = router(st);

    let resp = app.oneshot(sse_req(ZCODE_SESSION)).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let mut stream = resp.into_body().into_data_stream();
    next_frame(&mut stream).await.expect("首帧");

    // 另一台配对手机在同一会话执行动作：事件带 device_id=dev-other，不得出网
    hub.execute(
        ActionContext {
            action_id: "action-other-device-0001".into(),
            device_id: "dev-other".into(),
            device_name: "Pixel".into(),
        },
        ControlAction::Send {
            session_id: ZCODE_SESSION.into(),
            generation,
            text: "他机正文".into(),
        },
    )
    .await
    .expect("他机动作应由 Hub 受理");

    // 本机动作：应在同一流上成帧（证明广播路径真实，过滤不是整体丢弃）
    hub.execute(
        ActionContext {
            action_id: "action-own-device-0001".into(),
            device_id: "dev-z1".into(),
            device_name: "iPhone".into(),
        },
        ControlAction::Send {
            session_id: ZCODE_SESSION.into(),
            generation,
            text: "本机正文".into(),
        },
    )
    .await
    .expect("本机动作应由 Hub 受理");

    // 首帧快照内的 session() 会先播一条 session 帧；此后只应见到本机 action 帧。
    let mut seen = Vec::new();
    loop {
        let frame = next_frame(&mut stream).await.expect("本机动作帧");
        let is_action = frame.starts_with("event: action\n");
        seen.push(frame);
        if is_action {
            break;
        }
    }
    let all = seen.join("");
    assert!(
        !all.contains("action-other-device-0001"),
        "同会话他机动作 id 不得出网，实际 {all}"
    );
    let own = frame_json(seen.last().unwrap());
    assert_eq!(own["data"]["actionId"], "action-own-device-0001");
    assert_eq!(own["data"]["sessionId"], ZCODE_SESSION);
}

/// brief Step 1 冻结名：停服/吊销后不能再写或收事件——动作 403 零执行（零动作帧），
/// SSE 立即关闭且关闭后帧数恒 0。
#[tokio::test]
async fn stopped_remote_or_revoked_device_cannot_write_or_receive_events() {
    let conn = shared_conn();
    seed_device(&conn);
    let backend = Arc::new(FakeHubBackend::default());
    let hub = paired_hub_open(&conn, backend.clone()).await;
    let generation = hub.connection().generation;
    let st = state(&conn, Arc::new(RecordingInjector::default()));
    st.set_zcode_hub(hub);
    let registry = st.sse_registry.clone();
    let app = router(st);

    // 停服半场：disconnect_all（stop_server_core 的同一注册表动作）立即断流
    let resp = app.clone().oneshot(sse_req(ZCODE_SESSION)).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let mut stream = resp.into_body().into_data_stream();
    next_frame(&mut stream).await.expect("首帧");
    assert_eq!(registry.disconnect_all(), 1, "停服应关闭 1 条事件流");
    let mut frames_after_stop = 0;
    while next_frame(&mut stream).await.is_some() {
        frames_after_stop += 1;
    }
    assert_eq!(frames_after_stop, 0, "停服后不得再发帧");

    // 吊销半场：设备表失效 → SSE 断流 + 动作 gate 403（零执行 = 零动作帧）
    let resp = app.clone().oneshot(sse_req(ZCODE_SESSION)).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let mut stream = resp.into_body().into_data_stream();
    next_frame(&mut stream).await.expect("首帧");
    // 首帧快照的 session() 会绑定一次；动作被 gate 拦下后不得新增绑定。
    let binds_before = backend.bind_calls().len();
    DeviceStore::Owned(conn.clone()).with(|c| revoke_device(c, "dev-z1").expect("revoke"));
    assert_eq!(
        registry.disconnect_device("dev-z1"),
        1,
        "吊销应关闭 1 条连接"
    );
    let mut frames_after_revoke = 0;
    while next_frame(&mut stream).await.is_some() {
        frames_after_revoke += 1;
    }
    assert_eq!(frames_after_revoke, 0, "吊销后不得再发任何帧");

    let resp = app
        .oneshot(action_req(
            &send_body("action-after-revoke", generation),
            true,
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        axum::http::StatusCode::FORBIDDEN,
        "吊销后动作必须 403"
    );
    assert_eq!(backend.execute_calls(), 0, "吊销后零执行（零动作帧）");
    assert_eq!(
        backend.bind_calls().len(),
        binds_before,
        "吊销后动作不得新增绑定"
    );
}
