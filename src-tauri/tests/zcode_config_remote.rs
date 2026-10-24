//! A4 集成测试：ZCode 官方远控链接配置的远程读取合同。
//!
//! 覆盖：配对门（未配对/吊销一律 403）、已配对读到 Saved/Missing/Invalid 三态、
//! 其他 agent_type 不支持（400）、只读（GET 不改写配置）、no-store。
//!
//! 零真实副作用纪律：DeviceStore::Owned(共享内存库) + 测试桩 RemoteState——绝不接触
//! 真实 ~/.tuvis，绝不读取官方账号，绝不向官方服务发起任何 HTTP（端点实现只读 settings）。
//! 夹具只用合成 sid，不写任何真实用户链接。

use multi_agents_manager_lib::remote::codex_ipc::CodexControlHub;
use multi_agents_manager_lib::remote::pairing::{
    persist_device, revoke_device, DeviceStore, NewDevice,
};
use multi_agents_manager_lib::remote::server::{router, RemoteState};
use multi_agents_manager_lib::services::zcode_control::{get_config, set_config, CONFIG_KEY};
use multi_agents_manager_lib::session::SessionsResponse;
use std::sync::{Arc, Mutex};
use tower::ServiceExt as _;

type SharedConn = Arc<Mutex<rusqlite::Connection>>;

const COOKIE: &str = "mam_device=dev-1";
const ROUTE: &str = "/m/api/v1/official-control?agent_type=zcode";
/// 合成夹具：真实主机 + 合成 sid + 编码中文名称（不来自任何真实用户链接）。
const SYNTH_URL: &str =
    "https://zcode.z.ai/remote/v4?sid=synthetic-sid-a4-0001&name=%E5%85%94%E5%AD%90&region=cn";

fn shared_conn() -> SharedConn {
    let conn = rusqlite::Connection::open_in_memory().expect("open memory db");
    multi_agents_manager_lib::database::schema::init(&conn);
    multi_agents_manager_lib::database::migration::migrate(&conn).expect("migrate");
    Arc::new(Mutex::new(conn))
}

fn seed_device(conn: &SharedConn, id: &str) {
    let dev = NewDevice {
        id: id.to_string(),
        name: "iPhone".into(),
        ua: format!("ua-{id}"),
        origin_ip: format!("127.0.0.1#{id}"),
        via: "lan".into(),
        // 恒用当前时刻：device_valid 按 180 天 TTL 判活
        paired_at: chrono::Utc::now().timestamp_millis(),
    };
    DeviceStore::Owned(conn.clone()).with(|c| persist_device(c, &dev).expect("seed device"));
}

/// 测试装配：全部注入源为空/桩；store 指向共享内存库（配置读写与设备表同库）。
fn state(conn: &SharedConn) -> Arc<RemoteState> {
    Arc::new(RemoteState {
        ui_config_source: Box::new(|| None),
        subagent_source: std::collections::HashMap::new(),
        subagent_message_source: std::collections::HashMap::new(),
        capability_table: multi_agents_manager_lib::inject::capability::new_table(),
        session_source: Box::new(|| SessionsResponse {
            sessions: Vec::new(),
            total_count: 0,
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
        rate_bucket_channels_source: Box::new(|| Vec::new()),
        home_source: Box::new(|| None),
        injector: Arc::new(multi_agents_manager_lib::inject::engine::RealInjector),
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

fn req(method: &str, uri: &str, cookie: Option<&str>) -> axum::http::Request<axum::body::Body> {
    let mut b = axum::http::Request::builder().method(method).uri(uri);
    if let Some(c) = cookie {
        b = b.header("cookie", c);
    }
    b.body(axum::body::Body::empty()).unwrap()
}

async fn body_json(resp: axum::http::Response<axum::body::Body>) -> serde_json::Value {
    use http_body_util::BodyExt as _;
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).expect("JSON body")
}

fn cache_control(resp: &axum::http::Response<axum::body::Body>) -> String {
    resp.headers()
        .get(axum::http::header::CACHE_CONTROL)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

/// 红测 4：配对门 + 三态 DTO + 只读 + 无「已联机」字段。
#[tokio::test]
async fn remote_config_requires_pairing() {
    let conn = shared_conn();
    seed_device(&conn, "dev-1");
    let app = router(state(&conn));

    // 未配对：无 cookie → 403（回环同样拒绝；gate 零豁免）
    let resp = app.clone().oneshot(req("GET", ROUTE, None)).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::FORBIDDEN);

    // 已配对 + 未配置 → 200 {missing,null}，且 no-store
    let resp = app
        .clone()
        .oneshot(req("GET", ROUTE, Some(COOKIE)))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    assert_eq!(cache_control(&resp), "no-store");
    assert_eq!(
        body_json(resp).await,
        serde_json::json!({ "configState": "missing", "url": null })
    );

    // 桌面侧保存后：已配对读取 Saved（同一 DTO）；Saved 只表示格式已存——
    // 载荷只有 configState/url 两键，不含 connected/authorized 等联机旗标
    set_config(&conn.lock().unwrap(), SYNTH_URL).expect("save synthetic url");
    let resp = app
        .clone()
        .oneshot(req("GET", ROUTE, Some(COOKIE)))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(
        body,
        serde_json::json!({ "configState": "saved", "url": SYNTH_URL })
    );
    assert_eq!(body.as_object().unwrap().len(), 2, "DTO 不得携带联机状态");

    // 只读：GET 不改写已存值
    assert_eq!(
        DeviceStore::Owned(conn.clone())
            .with(get_config)
            .url
            .as_deref(),
        Some(SYNTH_URL)
    );

    // 吊销后：同一 cookie → 403
    DeviceStore::Owned(conn.clone()).with(|c| revoke_device(c, "dev-1").unwrap());
    let resp = app
        .clone()
        .oneshot(req("GET", ROUTE, Some(COOKIE)))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::FORBIDDEN);
}

/// 红测 4（续）：其他 agent_type / 缺参 → 400（首版只有 zcode 一种官方控制目标）。
#[tokio::test]
async fn other_agent_types_are_unsupported() {
    let conn = shared_conn();
    seed_device(&conn, "dev-1");
    let app = router(state(&conn));

    for uri in [
        "/m/api/v1/official-control?agent_type=claude",
        "/m/api/v1/official-control?agent_type=codex",
        "/m/api/v1/official-control?agent_type=",
        "/m/api/v1/official-control",
    ] {
        let resp = app
            .clone()
            .oneshot(req("GET", uri, Some(COOKIE)))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            axum::http::StatusCode::BAD_REQUEST,
            "应拒绝：{uri}"
        );
    }
}

/// 历史坏值直读：Invalid + url null（不下发坏 URL），且读取不删除用户设置。
#[tokio::test]
async fn stored_invalid_value_is_reported_without_url() {
    let conn = shared_conn();
    seed_device(&conn, "dev-1");
    let bad = "https://zcode.z.ai.evil.example/remote/v4?sid=synthetic-sid-a4-bad";
    conn.lock()
        .unwrap()
        .execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![CONFIG_KEY, bad],
        )
        .expect("inject stored invalid value");
    let app = router(state(&conn));

    let resp = app.oneshot(req("GET", ROUTE, Some(COOKIE))).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    assert_eq!(
        body_json(resp).await,
        serde_json::json!({ "configState": "invalid", "url": null })
    );
    // 不自动删除历史值（用户可手工替换）
    assert_eq!(
        conn.lock()
            .unwrap()
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                [CONFIG_KEY],
                |r| r.get::<_, String>(0)
            )
            .ok()
            .as_deref(),
        Some(bad)
    );
}
