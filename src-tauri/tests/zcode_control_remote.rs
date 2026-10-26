//! T7.1 路由边界集成测试（B1 切片）：ZCode 旧 session-send 拒绝 + send-info 广告
//! 官方控制路径。
//!
//! 零真实副作用纪律：DeviceStore::Owned(共享内存库) + 桩 RemoteState；会话快照与
//! 注入器全为合成/记录型假体，绝不接触真实 ~/.tuvis，绝不发起任何官方 HTTP。

use multi_agents_manager_lib::remote::codex_ipc::CodexControlHub;
use multi_agents_manager_lib::remote::pairing::{persist_device, DeviceStore, NewDevice};
use multi_agents_manager_lib::remote::server::{router, RemoteState};
use multi_agents_manager_lib::services::zcode_control::set_config;
use multi_agents_manager_lib::session::{
    AgentType, ProcessForm, Session, SessionStatus, SessionsResponse,
};
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
    Arc::new(RemoteState {
        ui_config_source: Box::new(|| None),
        subagent_source: std::collections::HashMap::new(),
        subagent_message_source: std::collections::HashMap::new(),
        capability_table: multi_agents_manager_lib::inject::capability::new_table(),
        session_source: Box::new(|| SessionsResponse {
            sessions: vec![zcode_session()],
            total_count: 1,
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
