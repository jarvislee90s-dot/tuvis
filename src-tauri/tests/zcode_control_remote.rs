//! T7.1 路由边界集成测试（B1 切片）：ZCode 旧 session-send 拒绝 + send-info 广告
//! 官方控制路径。
//!
//! 零真实副作用纪律：DeviceStore::Owned(共享内存库) + 桩 RemoteState；会话快照与
//! 注入器全为合成/记录型假体，绝不接触真实 ~/.tuvis，绝不发起任何官方 HTTP。

use multi_agents_manager_lib::remote::codex_ipc::CodexControlHub;
use multi_agents_manager_lib::remote::pairing::{persist_device, DeviceStore, NewDevice};
use multi_agents_manager_lib::remote::server::{router, RemoteState};
use multi_agents_manager_lib::services::zcode_control::{
    codes, set_config, ActionContext, ActionReceipt, ActionState, BoundTarget, CapabilityFlag,
    CapabilityView, ConnectionState, ConnectionView, ControlAction, DeviceRoute, HubBackend,
    OAuthProvider, SessionView, TargetKey, ZResult, ZcodeError, ZcodeHub,
};
use multi_agents_manager_lib::session::{
    AgentType, ProcessForm, Session, SessionStatus, SessionsResponse,
};
use std::sync::atomic::{AtomicUsize, Ordering};
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
        board_hidden_ids: Box::new(Vec::new),
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

    fn bind_calls(&self) -> Vec<TargetKey> {
        self.bind_calls.lock().unwrap().clone()
    }

    fn execute_calls(&self) -> usize {
        self.execute_calls.load(Ordering::SeqCst)
    }

    fn contexts(&self) -> Vec<ActionContext> {
        self.contexts.lock().unwrap().clone()
    }

    fn actions(&self) -> Vec<ControlAction> {
        self.actions.lock().unwrap().clone()
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
        Box::pin(async { Ok(()) })
    }

    fn connect<'a>(
        &'a self,
        _route: DeviceRoute,
    ) -> futures::future::BoxFuture<'a, ZResult<ConnectionView>> {
        self.connect_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Ok(ConnectionView {
                state: ConnectionState::Paired,
                generation: 1,
                reason_code: None,
                device_label: Some("fake-device".into()),
            })
        })
    }

    fn disconnect<'a>(&'a self) -> futures::future::BoxFuture<'a, ()> {
        Box::pin(async {})
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
    assert!(backend.bind_calls().is_empty(), "403 必须先于 hub 绑定");
    assert_eq!(backend.connect_calls(), 1, "未配对请求不得新增连接");
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

/// workspacePath（顶层或动作内）只能被拒绝或被忽略；无论哪条路都**绝不**传给 Hub
/// （绑定只按 sessionId，动作里没有路径字段）。
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

    let mut body = send_body("action-B2b-ws", generation);
    body.as_object_mut()
        .unwrap()
        .insert("workspacePath".into(), serde_json::json!("/evil/workspace"));
    body["action"]
        .as_object_mut()
        .unwrap()
        .insert("workspacePath".into(), serde_json::json!("/evil/workspace"));

    let resp = router(st).oneshot(action_req(&body, true)).await.unwrap();
    let status = resp.status();
    let wire = body_json(resp).await;
    assert!(
        status == axum::http::StatusCode::BAD_REQUEST || status == axum::http::StatusCode::OK,
        "只允许拒绝（400）或忽略（200），实际 {status}"
    );
    if status == axum::http::StatusCode::BAD_REQUEST {
        assert_eq!(wire, serde_json::json!({ "error": "invalid_action" }));
        assert_eq!(backend.execute_calls(), 0);
        assert!(backend.bind_calls().is_empty());
    }
    assert!(
        backend
            .bind_calls()
            .iter()
            .all(|key| key.session_id == ZCODE_SESSION),
        "绑定只按 sessionId"
    );
    assert!(
        backend
            .actions()
            .iter()
            .all(|action| !format!("{action:?}").contains("/evil")),
        "hub 收到的动作不得含客户端自报 workspace"
    );
    assert!(
        !wire.to_string().contains("/evil"),
        "响应不得回显客户端自报 workspace"
    );
}
