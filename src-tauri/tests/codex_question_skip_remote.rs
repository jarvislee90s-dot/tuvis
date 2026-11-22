//! R4（B4）题卡跳过四路真实 HTTP 纵向集成测试（计划 E 表 E1–E9）。
//!
//! 纪律：
//! * 真 Router（`remote::server::router`）→ 真实 gated nest/PIN 门禁/路由层
//!   no-store → 真实 `RemoteState`/`CodexControlHub`（`with_adapter`）→ 真实
//!   内存 SQLite（schema+migration）→ 真实 coordinator/DAO；**不直调 handler**。
//! * 零真实副作用：`DeviceStore::Owned(共享内存连接)`；Desktop IPC 走 Tokio
//!   duplex `ScriptedPeer`（本地假 Desktop，绝不连接真实官方 Desktop）；home_source
//!   恒 None（cfg(test) 的 rollout 解析不读真实 ~/.codex）。
//! * Native 发现工厂：B3 只在 crate 内注释提到「测试 `with_question_skip_factories`」，
//!   但 `CodexControlHub` 没有该公共构造缝、`DiscoveryFactories` 也是 `pub(crate)`——
//!   外部集成测试无法注入 Fake 工厂。因此 E2「有绑定健康路径」在本目标内不可达：
//!   本文件只钉住「缺省工厂保守 503」的反向事实（见 e2_*），并在报告中申报阻塞。
//!
//! E 行映射（测试名 → 计划行）：
//! * e1_*  → E1 展示/同步（skip 持久 + GET 投影 history/display groups）；
//! * e2_*  → E2 健康 Native（**不完整**：缺公共 Fake 工厂注入缝，仅缺省保守面）；
//! * e3_*  → E3 拒绝/降级（多题 409 零写入；无绑定 Tuvis-only 零副作用在 e7）；
//! * e4_*  → E4 restore/历史（sourceGone 仍可 restore，只回滚展示）；
//! * e5_*  → E5 幂等/恢复（同包原回执、异包 409、Prepared 重试 503）；
//! * e6_*  → E6 所有权/结算（HTTP 断连后 owned job 仍 durable 落账）；
//! * e7_*  → E7 零副作用（无 answer/send/steer/stop/queue-dispatch 帧）；
//! * e8_*  → E8/E9 协议与门禁（auth/method/strict DTO/no-store/字面量/SafeInt）。

mod support;

#[path = "support/codex_ipc_peer.rs"]
mod codex_ipc_peer;
use codex_ipc_peer::{
    discovery_response, peer_has_traffic, ScriptedPeer, StaticConnector, TEST_BUILD,
};

#[path = "support/codex_fixture.rs"]
mod codex_fixture;

use multi_agents_manager_lib::remote::codex_ipc::CodexControlHub;
use multi_agents_manager_lib::remote::pairing::{
    persist_device, revoke_device, DeviceStore, NewDevice,
};
use multi_agents_manager_lib::remote::server::{router, CreateTaskHub, RemoteState, SseRegistry};
use multi_agents_manager_lib::services::codex_desktop_ipc::adapter::DesktopIpcAdapter;
use multi_agents_manager_lib::services::codex_desktop_ipc::profile::{
    profile_for, ProtocolProfile,
};
use multi_agents_manager_lib::session::{
    AgentType, ProcessForm, Session, SessionStatus, SessionsResponse,
};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tower::ServiceExt as _;

// ===========================================================================
// 常量与请求样板
// ===========================================================================

const DEVICE_ID: &str = "dev-1";
const COOKIE: &str = "mam_device=dev-1";
const THREAD: &str = "0f1e2d3c-1111-4222-8333-444455556666";
const JS_SAFE_MAX: i64 = 9_007_199_254_740_991;
const QS: &str = "/m/api/v1/codex/question-skip";
const TITLE: &str = "部署到哪个环境？";
const OPTION_A: &str = "Staging";
const OPTION_B: &str = "Prod";

fn req(
    method: &str,
    uri: &str,
    cookie: Option<&str>,
    body: Option<String>,
) -> axum::http::Request<axum::body::Body> {
    let mut b = axum::http::Request::builder().method(method).uri(uri);
    if body.is_some() {
        b = b.header("content-type", "application/json");
    }
    if let Some(c) = cookie {
        b = b.header("cookie", c);
    }
    b.body(axum::body::Body::from(body.unwrap_or_default()))
        .unwrap()
}

async fn send(
    app: &axum::Router,
    request: axum::http::Request<axum::body::Body>,
) -> axum::http::Response<axum::body::Body> {
    app.clone().oneshot(request).await.expect("router oneshot")
}

async fn body_json(resp: axum::http::Response<axum::body::Body>) -> serde_json::Value {
    use http_body_util::BodyExt;
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).expect("JSON 响应体")
}

fn cache_control(resp: &axum::http::Response<axum::body::Body>) -> Option<&str> {
    resp.headers()
        .get("cache-control")
        .and_then(|v| v.to_str().ok())
}

fn thread_uri() -> String {
    format!("/m/api/v1/codex/thread?session_id={THREAD}")
}

fn skip_uri() -> String {
    format!("{QS}/skip")
}

fn restore_uri() -> String {
    format!("{QS}/restore")
}

fn preview_uri() -> String {
    format!("{QS}/pair-preview")
}

fn confirm_uri() -> String {
    format!("{QS}/pair-confirm")
}

/// 任意 group4（threadId 必须是可解析 UUID 会话号；其余字段由调用方给）。
fn group_value(turn: &str, item: &str, generation: &str) -> serde_json::Value {
    serde_json::json!({
        "threadId": THREAD,
        "sourceTurnId": turn,
        "sourceItemId": item,
        "contentGeneration": generation,
    })
}

fn skip_body(group: &serde_json::Value, action_id: &str, binding: Option<&str>) -> String {
    let mut v = serde_json::json!({
        "sessionId": THREAD,
        "group": group,
        "actionId": action_id,
    });
    if let Some(binding) = binding {
        v["bindingId"] = serde_json::json!(binding);
    }
    v.to_string()
}

fn restore_body(group: &serde_json::Value, action_id: &str, expected_revision: i64) -> String {
    serde_json::json!({
        "sessionId": THREAD,
        "group": group,
        "actionId": action_id,
        "expectedRevision": expected_revision,
    })
    .to_string()
}

fn preview_body(group: &serde_json::Value) -> String {
    serde_json::json!({ "sessionId": THREAD, "group": group }).to_string()
}

fn confirm_body(group: &serde_json::Value, challenge: &str) -> serde_json::Value {
    serde_json::json!({
        "sessionId": THREAD,
        "group": group,
        "challengeId": challenge,
        "candidateToken": "candidate-token",
        "sameThreadAndQuestion": true,
        "nativePanelQuestionCount": 1,
        "nativeDraftWillBeCleared": true,
    })
}

fn caps_open_profile() -> ProtocolProfile {
    profile_for("windows", TEST_BUILD).expect("shipped build profile")
}

/// 单题 agentMessage 的真实 canonical 历史（isComplete=true）。
fn single_question_history() -> (serde_json::Value, serde_json::Value) {
    (
        serde_json::json!({
            "t1": { "id": "t1", "type": "turn", "status": "completed", "items": ["q1"] },
            "q1": {
                "id": "q1",
                "type": "agent_message",
                "ts": 1,
                "questions": [{
                    "title": TITLE,
                    "options": [{ "label": OPTION_A }, { "label": OPTION_B }],
                }],
            },
        }),
        serde_json::json!(["t1"]),
    )
}

/// 同组两题（E3：多题必须整体拒绝，不拆单）。
fn two_question_history() -> (serde_json::Value, serde_json::Value) {
    (
        serde_json::json!({
            "t1": { "id": "t1", "type": "turn", "status": "completed", "items": ["q1"] },
            "q1": {
                "id": "q1",
                "type": "agent_message",
                "ts": 1,
                "questions": [
                    { "title": "第一题", "options": ["A"] },
                    { "title": "第二题", "options": ["B"] },
                ],
            },
        }),
        serde_json::json!(["t1"]),
    )
}

fn snapshot_frame(entities: serde_json::Value, turn_ids: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "type": "broadcast",
        "method": "thread-stream-state-changed",
        "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": THREAD,
            "hostId": "local",
            "change": {
                "type": "snapshot",
                "revision": 7,
                "conversationState": {
                    "turns": [],
                    "requests": [],
                    "threadRuntimeStatus": { "type": "idle" },
                    "turnHistory": codex_fixture::canonical_turn_history(entities, turn_ids),
                }
            }
        }
    })
}

// ===========================================================================
// Fixture：真实 Router + 内存库 + 假 Desktop（可选 peer）
// ===========================================================================

type SharedConn = Arc<StdMutex<rusqlite::Connection>>;

fn shared_conn() -> SharedConn {
    let conn = rusqlite::Connection::open_in_memory().expect("open memory db");
    multi_agents_manager_lib::database::schema::init(&conn);
    multi_agents_manager_lib::database::migration::migrate(&conn).expect("migrate");
    Arc::new(StdMutex::new(conn))
}

fn seed_device(conn: &SharedConn) {
    // last_seen_at 故意回拨 60s：E6 以「last_seen_at 推进」作为请求已过设备门的
    // 确定性信号（同毫秒内建卡+请求会让该信号恒不变化 → 假红）。
    let paired_at = chrono::Utc::now().timestamp_millis() - 60_000;
    let dev = NewDevice {
        id: DEVICE_ID.to_string(),
        name: "iPhone".to_string(),
        ua: "ua-test".to_string(),
        origin_ip: "127.0.0.1#dev-1".to_string(),
        via: "lan".into(),
        paired_at,
    };
    DeviceStore::Owned(conn.clone()).with(|c| persist_device(c, &dev).expect("seed device"));
}

fn codex_app_session() -> Session {
    Session {
        id: THREAD.into(),
        agent_type: AgentType::Codex,
        project_name: "proj".into(),
        project_path: "/tmp/proj".into(),
        title: None,
        git_branch: None,
        github_url: None,
        status: SessionStatus::Idle,
        last_message: None,
        last_message_role: None,
        last_message_subagent_report: false,
        flap_from_subagent_activity: false,
        last_activity_at: "2026-11-17T00:00:00Z".into(),
        pid: 4242,
        cpu_usage: 0.0,
        active_subagent_count: 0,
        form: ProcessForm::App,
        jump_supported: false,
        unread: false,
        unread_turned_green_at_ms: None,
    }
}

struct Fixture {
    conn: SharedConn,
    state: Arc<RemoteState>,
    app: axum::Router,
    peer: Option<ScriptedPeer>,
    /// 会话集共享单元：M1 用例需要让会话「离场」再验证账本回放不依赖它。
    sessions: Arc<StdMutex<Vec<Session>>>,
}

impl Fixture {
    fn set_sessions(&self, sessions: Vec<Session>) {
        *self.sessions.lock().unwrap_or_else(|e| e.into_inner()) = sessions;
    }
}

fn fixture() -> Fixture {
    support::setup();
    let conn = shared_conn();
    seed_device(&conn);
    let sessions: Arc<StdMutex<Vec<Session>>> = Arc::new(StdMutex::new(vec![codex_app_session()]));
    let (io, peer) = ScriptedPeer::new();
    let adapter = Arc::new(DesktopIpcAdapter::new(
        Arc::new(StaticConnector::new(io)),
        caps_open_profile(),
    ));
    let hub = Arc::new(CodexControlHub::with_adapter(
        DeviceStore::Owned(conn.clone()),
        adapter,
        true,
    ));
    let state = Arc::new(RemoteState {
        ui_config_source: Box::new(|| None),
        subagent_source: std::collections::HashMap::new(),
        subagent_message_source: std::collections::HashMap::new(),
        capability_table: multi_agents_manager_lib::inject::capability::new_table(),
        // 生产同源：显示路径与动作路径读同一份扫描快照；测试用共享单元以便
        // 「会话离场」用例在运行中改会话集。
        session_source: {
            let sessions = sessions.clone();
            Box::new(move || {
                let list = sessions.lock().unwrap_or_else(|e| e.into_inner()).clone();
                SessionsResponse {
                    total_count: list.len(),
                    sessions: list,
                    waiting_count: 0,
                }
            })
        },
        fresh_session_source: {
            let sessions = sessions.clone();
            Box::new(move || {
                let list = sessions.lock().unwrap_or_else(|e| e.into_inner()).clone();
                SessionsResponse {
                    total_count: list.len(),
                    sessions: list,
                    waiting_count: 0,
                }
            })
        },
        pairing_counter: Box::new(Vec::new),
        board_hidden_ids: Box::new(Vec::new),
        board_hidden_hide: Arc::new(|_| 0usize),
        board_hidden_unhide: Arc::new(|_| 0usize),
        unread_mark_read: Arc::new(|_, _| ()),
        session_close: Arc::new(|_| Ok(())),
        store: DeviceStore::Owned(conn.clone()),
        host_source: Box::new(|| serde_json::json!({ "host": { "name": "t" } })),
        message_source: Box::new(|_, _, _| Err("测试桩：无内容".to_string())),
        path_source: Box::new(|_, _, _| (Vec::new(), false)),
        watcher_tx: tokio::sync::broadcast::channel(64).0,
        sse_registry: Arc::new(SseRegistry::default()),
        max_devices_source: Box::new(|| 3),
        pin_limiter: StdMutex::new(multi_agents_manager_lib::remote::pin::PinRateLimiter::new()),
        global_pin_limiter: StdMutex::new(
            multi_agents_manager_lib::remote::pin::PinRateLimiter::global(),
        ),
        pin_source: Box::new(|| Some("1234".to_string())),
        now_source: Box::new(|| chrono::Utc::now().timestamp_millis()),
        via_hosts_source: Box::new(|| None),
        rate_bucket_channels_source: Box::new(Vec::new),
        home_source: Box::new(|| None),
        injector: Arc::new(multi_agents_manager_lib::inject::engine::RealInjector),
        resume_spawner: Arc::new(|_: &multi_agents_manager_lib::inject::resume::SpawnSpec| Ok(())),
        create_hub: Arc::new(CreateTaskHub::stub()),
        archive_source: Box::new(Vec::new),
        archive_delete: Arc::new(|_: Option<&str>| 0usize),
        confirm_probe: Arc::new(|_, _, _| true),
        dialog_probe: Arc::new(|_, _| None),
        screen_probe: Arc::new(|_, _| None),
        codex_control: hub,
        desktop_language: Default::default(),
    });
    let app = router(state.clone());
    Fixture {
        conn,
        state,
        app,
        peer: Some(peer),
        sessions,
    }
}

impl Fixture {
    /// 首次 GET /codex/thread：驱动假 Desktop（initialize → discovery → following →
    /// snapshot），返回首个投影（其 `questionDisplayGroups` 给出 source 权威 group4）。
    async fn open_thread(
        &mut self,
        entities: serde_json::Value,
        turn_ids: serde_json::Value,
    ) -> serde_json::Value {
        let mut peer = self.peer.take().expect("peer available");
        let app = self.app.clone();
        let task = tokio::spawn(async move {
            app.oneshot(req("GET", &thread_uri(), Some(COOKIE), None))
                .await
                .expect("oneshot")
        });

        let init = peer.recv().await;
        assert_eq!(init["method"], "initialize");
        peer.send(&serde_json::json!({
            "type": "response", "requestId": init["requestId"],
            "resultType": "success", "method": "initialize",
            "handledByClientId": "owner-1", "result": { "clientId": "owner-1" }
        }))
        .await;
        let disc = peer.recv().await;
        assert_eq!(disc["method"], "thread-owner-discovery");
        peer.send(&discovery_response(
            disc["requestId"].as_str().unwrap(),
            "owner-1",
        ))
        .await;
        let follow = peer.recv().await;
        assert_eq!(follow["method"], "thread-stream-following-changed");
        peer.send(&snapshot_frame(entities, turn_ids)).await;

        let resp = tokio::time::timeout(Duration::from_secs(15), task)
            .await
            .expect("GET /codex/thread 未在时限内返回")
            .expect("join");
        assert_eq!(resp.status(), 200, "首次投影必须 200");
        let projection = body_json(resp).await;
        self.peer = Some(peer);
        projection
    }

    fn display_row(&self, group: &serde_json::Value) -> Option<(String, i64)> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.query_row(
            "SELECT decision, revision FROM codex_question_display
             WHERE thread_id = ?1 AND source_turn_id = ?2
               AND source_item_id = ?3 AND content_generation = ?4",
            rusqlite::params![
                group["threadId"].as_str().unwrap(),
                group["sourceTurnId"].as_str().unwrap(),
                group["sourceItemId"].as_str().unwrap(),
                group["contentGeneration"].as_str().unwrap(),
            ],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )
        .ok()
    }

    fn count_rows(&self, table: &str) -> i64 {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap_or(-1)
    }

    fn action_state(&self, action_id: &str) -> Option<String> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.query_row(
            "SELECT state FROM codex_question_skip_actions WHERE action_id = ?1",
            [action_id],
            |r| r.get(0),
        )
        .ok()
    }

    fn action_reason(&self, action_id: &str) -> Option<String> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.query_row(
            "SELECT native_reason FROM codex_question_skip_actions WHERE action_id = ?1",
            [action_id],
            |r| r.get(0),
        )
        .ok()
        .flatten()
    }

    fn last_seen_at(&self) -> i64 {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.query_row(
            "SELECT last_seen_at FROM remote_devices WHERE id = ?1",
            [DEVICE_ID],
            |r| r.get(0),
        )
        .unwrap_or(-1)
    }
}

/// 从首个投影取唯一 source group（camelCase，可直接作为请求 group）。
fn single_group(projection: &serde_json::Value) -> (serde_json::Value, i64) {
    let groups = projection["questionDisplayGroups"]
        .as_array()
        .expect("首个投影必须携带 questionDisplayGroups（健康 source 同观察）");
    assert_eq!(groups.len(), 1, "本夹具恰一组");
    (
        groups[0]["group"].clone(),
        groups[0]["sourceQuestionCount"].as_i64().unwrap(),
    )
}

/// 有界排空假 Desktop 已到达的帧（用于「零写入帧」断言）。
async fn drain_frames(peer: &mut ScriptedPeer) -> Vec<serde_json::Value> {
    let mut frames = Vec::new();
    while let Some(frame) = peer.recv_or_eof().await {
        frames.push(frame);
    }
    frames
}

/// 出现即视为「额外原生/消息副作用」的 IPC 方法（E7）。
fn forbidden_ipc_method(method: &str) -> bool {
    matches!(
        method,
        "thread-follower-start-turn"
            | "thread-follower-steer-turn"
            | "thread-follower-interrupt-turn"
            | "thread-follower-submit-user-input"
            | "thread-follower-command-approval-decision"
            | "thread-follower-file-approval-decision"
            | "thread-follower-permissions-approval-decision"
    )
}

fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

// ===========================================================================
// E8/E9：协议与门禁
// ===========================================================================

/// E8：四路必须过设备 cookie 门禁（无 cookie / 已吊销都 403）。
#[tokio::test]
async fn e8_four_routes_require_device_cookie() {
    let f = fixture();
    let routes = [preview_uri(), confirm_uri(), skip_uri(), restore_uri()];
    for uri in &routes {
        let r = send(&f.app, req("POST", uri, None, Some("{}".into()))).await;
        assert_eq!(r.status(), 403, "无 cookie 必须 403：{uri}");
    }
    revoke_device(&f.conn.lock().unwrap_or_else(|e| e.into_inner()), DEVICE_ID).expect("revoke");
    for uri in &routes {
        let r = send(&f.app, req("POST", uri, Some(COOKIE), Some("{}".into()))).await;
        assert_eq!(r.status(), 403, "已吊销设备必须 403：{uri}");
    }
}

/// E8：**auth 拒绝面同样禁止被中间层缓存**（计划 row：no-store 含所有错误）。
///
/// 现状暴露的生产缺口：gate 在 route 层 `force_no_store` 之外短路（`unauthorized()`
/// 直接 `403.into_response()`），未带 `Cache-Control: no-store`。本用例按需求保留
/// 严格断言，不以放宽换绿（控制者修生产，勿改本测试）。
#[tokio::test]
async fn e8_auth_failures_carry_no_store() {
    let f = fixture();
    let routes = [preview_uri(), confirm_uri(), skip_uri(), restore_uri()];
    for uri in &routes {
        let r = send(&f.app, req("POST", uri, None, Some("{}".into()))).await;
        assert_eq!(r.status(), 403, "{uri}");
        assert_eq!(
            cache_control(&r),
            Some("no-store"),
            "E8：auth 拒绝面必须 no-store（否则中间层可缓存鉴权失败）：{uri}"
        );
    }
}

/// E8：方法错误（GET 到只注册 POST 的路径）→ 405 且路由层 no-store 覆盖该面。
#[tokio::test]
async fn e8_method_not_allowed_is_405_with_no_store() {
    let f = fixture();
    for uri in [preview_uri(), confirm_uri(), skip_uri(), restore_uri()] {
        let r = send(&f.app, req("GET", &uri, Some(COOKIE), None)).await;
        assert_eq!(r.status(), 405, "GET 到 POST-only 路由必须 405：{uri}");
        assert_eq!(
            cache_control(&r),
            Some("no-store"),
            "405 必须经路由层 no-store（非 handler 面）：{uri}"
        );
    }
}

/// E8：outer/nested DTO `deny_unknown_fields` 双层都拒未知字段、且 no-store。
#[tokio::test]
async fn e8_unknown_outer_and_nested_fields_rejected_with_no_store() {
    let f = fixture();
    let group = group_value("t-x", "i-x", "g-x");

    // outer 层：四路各自多塞一个未知键。
    let mut skip_outer =
        serde_json::from_str::<serde_json::Value>(&skip_body(&group, "a-unknown-outer", None))
            .unwrap();
    skip_outer["pid"] = serde_json::json!(123);
    let mut preview_outer =
        serde_json::from_str::<serde_json::Value>(&preview_body(&group)).unwrap();
    preview_outer["deviceId"] = serde_json::json!("dev-evil");
    let mut confirm_outer = confirm_body(&group, "challenge-x");
    confirm_outer["nativePanelCount"] = serde_json::json!(1);
    let mut restore_outer =
        serde_json::from_str::<serde_json::Value>(&restore_body(&group, "a-restore-outer", 1))
            .unwrap();
    restore_outer["device"] = serde_json::json!("dev-evil");

    // nested 层：group4 内部多塞 scope（四路共用同一 GroupIdentityWire）。
    let mut nested_group = group.clone();
    nested_group["scope"] = serde_json::json!({ "hwnd": 1 });
    let mut skip_nested = serde_json::json!({
        "sessionId": THREAD,
        "group": nested_group.clone(),
        "actionId": "a-unknown-nested",
    });
    skip_nested["scope"] = serde_json::json!({ "hwnd": 2 }); // outer 同时多塞
    let mut confirm_nested = confirm_body(&nested_group, "challenge-x");
    confirm_nested["group"] = nested_group.clone();
    let preview_nested = preview_body(&nested_group);
    let restore_nested = restore_body(&nested_group, "a-restore-nested", 1);

    let cases: [(&str, String); 8] = [
        (&skip_uri(), skip_outer.to_string()),
        (&skip_uri(), skip_nested.to_string()),
        (&preview_uri(), preview_outer.to_string()),
        (&preview_uri(), preview_nested),
        (&confirm_uri(), confirm_outer.to_string()),
        (&confirm_uri(), confirm_nested.to_string()),
        (&restore_uri(), restore_outer.to_string()),
        (&restore_uri(), restore_nested),
    ];
    for (uri, body) in cases {
        let r = send(&f.app, req("POST", uri, Some(COOKIE), Some(body.clone()))).await;
        assert!(
            (400..500).contains(&r.status().as_u16()),
            "未知 DTO 字段必须 4xx（{uri}），得到 {} body={body}",
            r.status()
        );
        assert_eq!(
            cache_control(&r),
            Some("no-store"),
            "提取拒绝面必须 no-store：{uri}"
        );
    }
}

/// E8：cookie 决定 device——body 夹带 device/PID/HWND/RID/scope 一律拒绝。
#[tokio::test]
async fn e8_body_device_fields_rejected_by_deny_unknown_fields() {
    let f = fixture();
    let group = group_value("t-x", "i-x", "g-x");
    for (field, value) in [
        ("deviceId", serde_json::json!("dev-evil")),
        ("pid", serde_json::json!(1)),
        ("hwnd", serde_json::json!(2)),
        ("rid", serde_json::json!("rid")),
        ("scope", serde_json::json!({ "pid": 1 })),
    ] {
        let mut body =
            serde_json::from_str::<serde_json::Value>(&skip_body(&group, "a-dev-field", None))
                .unwrap();
        body[field] = value;
        let r = send(
            &f.app,
            req("POST", &skip_uri(), Some(COOKIE), Some(body.to_string())),
        )
        .await;
        assert!(
            (400..500).contains(&r.status().as_u16()),
            "body 夹带 {field} 必须被拒，得到 {}",
            r.status()
        );
        assert_eq!(cache_control(&r), Some("no-store"), "拒绝面必须 no-store");
    }
}

/// E8：pair-confirm 两个确认只接受字面量 `true`、题数只接受字面量 `1`。
#[tokio::test]
async fn e8_confirm_requires_literal_true_and_literal_one() {
    let f = fixture();
    let group = group_value("t-x", "i-x", "g-x");
    let base = confirm_body(&group, "challenge-x");
    let mut cases: Vec<(&str, serde_json::Value)> = Vec::new();
    for bad in [
        serde_json::json!(false),
        serde_json::json!("true"),
        serde_json::json!(1),
    ] {
        let mut v = base.clone();
        v["sameThreadAndQuestion"] = bad.clone();
        cases.push(("sameThreadAndQuestion", v));
    }
    let mut v = base.clone();
    v.as_object_mut().unwrap().remove("sameThreadAndQuestion");
    cases.push(("missing sameThreadAndQuestion", v));
    for bad in [
        serde_json::json!(0),
        serde_json::json!(2),
        serde_json::json!("1"),
        serde_json::json!(1.0),
    ] {
        let mut v = base.clone();
        v["nativePanelQuestionCount"] = bad.clone();
        cases.push(("nativePanelQuestionCount", v));
    }
    let mut v = base.clone();
    v.as_object_mut()
        .unwrap()
        .remove("nativePanelQuestionCount");
    cases.push(("missing nativePanelQuestionCount", v));
    let mut v = base.clone();
    v["nativeDraftWillBeCleared"] = serde_json::json!(false);
    cases.push(("nativeDraftWillBeCleared", v));

    for (label, body) in cases {
        let r = send(
            &f.app,
            req("POST", &confirm_uri(), Some(COOKIE), Some(body.to_string())),
        )
        .await;
        assert!(
            (400..500).contains(&r.status().as_u16()),
            "非字面量确认必须 4xx（{label}），得到 {}",
            r.status()
        );
        assert_eq!(
            cache_control(&r),
            Some("no-store"),
            "拒绝面必须 no-store（{label}）"
        );
    }
}

/// E8：expectedRevision 只接受整数 1..=JS_SAFE_MAX（正安全整数边界）。
#[tokio::test]
async fn e8_restore_revision_bounds_rejected() {
    let f = fixture();
    let group = group_value("t-x", "i-x", "g-x");
    for bad in [0i64, -1, JS_SAFE_MAX + 1] {
        let r = send(
            &f.app,
            req(
                "POST",
                &restore_uri(),
                Some(COOKIE),
                Some(restore_body(&group, "a-rev", bad)),
            ),
        )
        .await;
        assert_eq!(r.status(), 400, "expectedRevision={bad} 必须 400");
        assert_eq!(cache_control(&r), Some("no-store"), "400 必须 no-store");
    }
    // 浮点：serde i64 拒绝（提取层 4xx），绝不静默取整。
    let float_body = serde_json::json!({
        "sessionId": THREAD,
        "group": group,
        "actionId": "a-rev-float",
        "expectedRevision": 1.5,
    })
    .to_string();
    let r = send(
        &f.app,
        req("POST", &restore_uri(), Some(COOKIE), Some(float_body)),
    )
    .await;
    assert!(
        (400..500).contains(&r.status().as_u16()),
        "浮点 expectedRevision 必须 4xx，得到 {}",
        r.status()
    );
    assert_eq!(cache_control(&r), Some("no-store"), "拒绝面必须 no-store");
}

/// E8：bindingId 空串拒绝（None 与 Some 是不同请求，不得经空串退化为无绑定）。
#[tokio::test]
async fn e8_binding_id_empty_or_action_id_blank_is_400() {
    let f = fixture();
    let group = group_value("t-x", "i-x", "g-x");
    let r = send(
        &f.app,
        req(
            "POST",
            &skip_uri(),
            Some(COOKIE),
            Some(skip_body(&group, "a-empty-binding", Some(""))),
        ),
    )
    .await;
    assert_eq!(r.status(), 400, "bindingId 空串必须 400");
    assert_eq!(cache_control(&r), Some("no-store"));

    let r = send(
        &f.app,
        req(
            "POST",
            &skip_uri(),
            Some(COOKIE),
            Some(skip_body(&group, "   ", None)),
        ),
    )
    .await;
    assert_eq!(r.status(), 400, "空白 actionId 必须 400");
    assert_eq!(cache_control(&r), Some("no-store"));
}

/// E8/E9：会话不可解析 / group.threadId 与解析 ThreadKey 不一致 → 404（不拿
/// sessionId 冒充 ThreadKey，也放过跨线程身份），且零写入。
#[tokio::test]
async fn e8_unknown_session_or_thread_mismatch_is_404() {
    let f = fixture();
    let group = group_value("t-x", "i-x", "g-x");
    let mut unknown_session =
        serde_json::from_str::<serde_json::Value>(&skip_body(&group, "a-unknown-session", None))
            .unwrap();
    unknown_session["sessionId"] = serde_json::json!("2f2e2d2c-1111-4222-8333-444455556666");
    let r = send(
        &f.app,
        req(
            "POST",
            &skip_uri(),
            Some(COOKIE),
            Some(unknown_session.to_string()),
        ),
    )
    .await;
    assert_eq!(r.status(), 404, "未知会话必须 404");
    assert_eq!(cache_control(&r), Some("no-store"));

    let mut mismatched = group_value("t-x", "i-x", "g-x");
    mismatched["threadId"] = serde_json::json!("999e2d3c-1111-4222-8333-444455556666");
    let r = send(
        &f.app,
        req(
            "POST",
            &skip_uri(),
            Some(COOKIE),
            Some(skip_body(&mismatched, "a-thread-mismatch", None)),
        ),
    )
    .await;
    assert_eq!(r.status(), 404, "group.threadId 与解析线程不符必须 404");
    assert_eq!(cache_control(&r), Some("no-store"));
    assert_eq!(
        f.count_rows("codex_question_display"),
        0,
        "身份拒绝不得写展示行"
    );
    assert_eq!(
        f.count_rows("codex_question_skip_actions"),
        0,
        "身份拒绝不得写动作行"
    );
}

/// E8：expectedRevision 正 SafeInt 上边界必须被接受（新 revision 恰为
/// `JS_SAFE_MAX`）；超过上界在上一用例已被 400 拒绝。
#[tokio::test]
async fn e8_restore_safe_int_upper_boundary_is_accepted() {
    let f = fixture();
    let group = group_value("turn-safe", "item-safe", "gen-safe");
    {
        let conn = f.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "INSERT INTO codex_question_display
                (thread_id, source_turn_id, source_item_id, content_generation,
                 decision, revision, device_id, updated_at, source_snapshot_json)
             VALUES (?1, ?2, ?3, ?4, 'skipped', ?5, ?6, ?7, NULL)",
            rusqlite::params![
                THREAD,
                "turn-safe",
                "item-safe",
                "gen-safe",
                JS_SAFE_MAX - 1,
                DEVICE_ID,
                1_i64,
            ],
        )
        .expect("seed skipped row at boundary");
    }
    let r = send(
        &f.app,
        req(
            "POST",
            &restore_uri(),
            Some(COOKIE),
            Some(restore_body(&group, "restore-safe-max", JS_SAFE_MAX - 1)),
        ),
    )
    .await;
    assert_eq!(r.status(), 200, "正 SafeInt 上边界必须被接受");
    let receipt = body_json(r).await;
    assert_eq!(receipt["displayDecision"], "active");
    assert_eq!(receipt["revision"], serde_json::json!(JS_SAFE_MAX));
    assert_eq!(
        f.display_row(&group),
        Some(("active".to_string(), JS_SAFE_MAX))
    );
}

// ===========================================================================
// E3：多题整体拒绝（零展示行/零动作行/零 Invoke）
// ===========================================================================

#[tokio::test]
async fn e3_multi_question_group_is_refused_with_zero_writes() {
    let mut f = fixture();
    let (entities, turn_ids) = two_question_history();
    let projection = f.open_thread(entities, turn_ids).await;
    let (group, count) = single_group(&projection);
    assert_eq!(count, 2, "夹具必须是同组两题");

    let mut peer = f.peer.take().expect("peer");
    let _ = drain_frames(&mut peer).await;

    let r = send(
        &f.app,
        req(
            "POST",
            &skip_uri(),
            Some(COOKIE),
            Some(skip_body(&group, "skip-multi", None)),
        ),
    )
    .await;
    assert_eq!(r.status(), 409, "多题组必须整体拒绝");
    let body = body_json(r).await;
    assert_eq!(body["error"], "unsupported_group");

    assert_eq!(
        f.count_rows("codex_question_display"),
        0,
        "拒绝不得新增展示行"
    );
    assert_eq!(
        f.count_rows("codex_question_skip_actions"),
        0,
        "拒绝不得新增动作行"
    );
    let frames = drain_frames(&mut peer).await;
    assert!(
        frames
            .iter()
            .all(|frame| !forbidden_ipc_method(frame["method"].as_str().unwrap_or(""))),
        "多题拒绝必须零 Invoke：{frames:?}"
    );
    f.peer = Some(peer);
}

// ===========================================================================
// E2：缺省（无 Fake 工厂注入缝）保守 503 —— E2 健康路径在本目标不可达
// ===========================================================================

/// E2（**反向保守面**）：B3 未提供公共 Fake discovery 工厂注入缝
/// （`with_question_skip_factories` 只存在于 `codex_ipc.rs` 注释；`DiscoveryFactories`
/// 是 `pub(crate)` 且 `with_parts_inner` 私有），外部集成测试**无法**构造「有绑定
/// 健康路径」——pair-preview/pair-confirm 的 0 Invoke 正向断言与「健康绑定 skip 恰
/// 1 次 Invoke」在本目标内不可达（已向控制者报阻塞）。
///
/// 本用例钉住缺省工厂的保守语义：pair-preview 503 operation_pending、零展示/动作行、
/// 零写帧。为排除「其实是 live source 未装载」这一替代解释，末尾再发一条 skip 并
/// 断言 200（同一夹具、同一线程，无任何中间帧）——source 在场而 preview 仍 503，
/// 503 只能归因于缺省工厂不可用。
#[tokio::test]
async fn e2_pair_preview_without_injected_fake_factory_is_pending() {
    let mut f = fixture();
    let (entities, turn_ids) = single_question_history();
    let projection = f.open_thread(entities, turn_ids).await;
    let (group, _) = single_group(&projection);

    let mut peer = f.peer.take().expect("peer");
    let _ = drain_frames(&mut peer).await;

    let r = send(
        &f.app,
        req(
            "POST",
            &preview_uri(),
            Some(COOKIE),
            Some(preview_body(&group)),
        ),
    )
    .await;
    assert_eq!(r.status(), 503, "缺省 Native 工厂必须保守 Pending");
    assert_eq!(cache_control(&r), Some("no-store"));
    let body = body_json(r).await;
    assert_eq!(body["error"], "operation_pending");

    assert_eq!(
        f.count_rows("codex_question_display"),
        0,
        "preview 必须零展示行"
    );
    assert_eq!(
        f.count_rows("codex_question_skip_actions"),
        0,
        "preview 必须零动作行"
    );
    let frames = drain_frames(&mut peer).await;
    assert!(
        frames
            .iter()
            .all(|frame| !forbidden_ipc_method(frame["method"].as_str().unwrap_or(""))),
        "preview 必须零 Invoke：{frames:?}"
    );

    // 归因排除：source 在场 → skip 必须能走完 prepare/settle（200）。
    let r = send(
        &f.app,
        req(
            "POST",
            &skip_uri(),
            Some(COOKIE),
            Some(skip_body(&group, "skip-e2-source-live", None)),
        ),
    )
    .await;
    assert_eq!(
        r.status(),
        200,
        "同夹具 skip 可承接 ⇒ 上面的 preview 503 不是「source 未装载」"
    );
    assert_eq!(
        f.action_state("skip-e2-source-live").as_deref(),
        Some("finished")
    );
    f.peer = Some(peer);
}

// ===========================================================================
// E1 + E7：无绑定 Tuvis-only skip（展示持久 + 投影可见 + 零副作用）
// ===========================================================================

#[tokio::test]
async fn e1_skip_persists_and_thread_projection_reports_skipped() {
    let mut f = fixture();
    let (entities, turn_ids) = single_question_history();
    let projection = f.open_thread(entities, turn_ids).await;
    let (group, count) = single_group(&projection);
    assert_eq!(count, 1);
    assert_eq!(projection["questionDisplayGroups"][0]["decision"], "active");
    assert_eq!(projection["questionDisplayGroups"][0]["revision"], 0);

    let r = send(
        &f.app,
        req(
            "POST",
            &skip_uri(),
            Some(COOKIE),
            Some(skip_body(&group, "skip-e1", None)),
        ),
    )
    .await;
    assert_eq!(r.status(), 200, "无绑定 skip 必须被承接");
    assert_eq!(cache_control(&r), Some("no-store"));
    let receipt = body_json(r).await;
    assert_eq!(receipt["actionId"], "skip-e1");
    assert_eq!(receipt["group"], group);
    assert_eq!(receipt["displayDecision"], "skipped");
    assert_eq!(receipt["nativeEffect"], "unresolved");
    assert_eq!(receipt["reasonCode"], "binding_required");
    assert_eq!(receipt["revision"], 1);

    // DAO：展示行持久 skipped/revision 1；动作行终态。
    assert_eq!(
        f.display_row(&group),
        Some(("skipped".to_string(), 1)),
        "skip 必须持久展示行"
    );
    assert_eq!(f.action_state("skip-e1").as_deref(), Some("finished"));
    assert_eq!(
        f.action_reason("skip-e1").as_deref(),
        Some("binding_required"),
        "零 Invoke 结算原因必须如实为 binding_required"
    );

    // GET /codex/thread（同观察投影）：history 与 display groups 都反映 skipped。
    let r = send(&f.app, req("GET", &thread_uri(), Some(COOKIE), None)).await;
    assert_eq!(r.status(), 200);
    let v = body_json(r).await;
    let history = v["questionSkipHistory"].as_array().expect("history 账");
    assert_eq!(history.len(), 1);
    assert_eq!(history[0]["group"], group);
    assert_eq!(history[0]["decision"], "skipped");
    assert_eq!(history[0]["revision"], 1);
    assert_eq!(history[0]["latestNativeEffect"], "unresolved");
    assert_eq!(history[0]["reasonCode"], "binding_required");
    assert_eq!(history[0]["question"]["title"], TITLE);
    let groups = v["questionDisplayGroups"]
        .as_array()
        .expect("display groups 账");
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0]["decision"], "skipped");
    assert_eq!(groups[0]["revision"], 1);
    assert_eq!(groups[0]["group"], group);
}

/// E1：skip 展示变更经**同一次观察**进入 GET 投影；Desktop revision/queuePaused
/// 未变也必须携带**更大的 localSeq**（展示通知不得只靠 Desktop 帧推进；sourceSeq
/// 不进投影，属 live 内部记账）。
#[tokio::test]
async fn e1_tuvis_only_skip_bumps_local_seq_not_desktop_revision() {
    let mut f = fixture();
    let (entities, turn_ids) = single_question_history();
    let projection = f.open_thread(entities, turn_ids).await;
    let (group, _) = single_group(&projection);
    let desktop_revision = projection["revision"].clone();
    let queue_paused = projection["queuePaused"].clone();
    let before_seq = projection["projectionVersion"]["localSeq"]
        .as_u64()
        .expect("T02 起投影必须带 projectionVersion.localSeq");

    let r = send(
        &f.app,
        req(
            "POST",
            &skip_uri(),
            Some(COOKIE),
            Some(skip_body(&group, "skip-e1-seq", None)),
        ),
    )
    .await;
    assert_eq!(r.status(), 200);

    let r = send(&f.app, req("GET", &thread_uri(), Some(COOKIE), None)).await;
    assert_eq!(r.status(), 200);
    let v = body_json(r).await;
    let after_seq = v["projectionVersion"]["localSeq"]
        .as_u64()
        .expect("skip 后投影仍须带 localSeq");
    assert!(
        after_seq > before_seq,
        "展示变更必须发布更大 localSeq（before={before_seq} after={after_seq}）"
    );
    assert_eq!(v["revision"], desktop_revision, "不得伪造 Desktop revision");
    assert_eq!(
        v["queuePaused"], queue_paused,
        "不得借 queuePaused 去重/推进"
    );
    assert_eq!(
        v["questionDisplayGroups"][0]["decision"], "skipped",
        "同一次观察必须已包含展示决策"
    );
    assert_eq!(v["questionDisplayGroups"][0]["revision"], 1);
}

#[tokio::test]
async fn e7_unbound_skip_emits_zero_native_or_message_side_effects() {
    let mut f = fixture();
    let (entities, turn_ids) = single_question_history();
    let projection = f.open_thread(entities, turn_ids).await;
    let (group, _) = single_group(&projection);

    let mut peer = f.peer.take().expect("peer");
    let _ = drain_frames(&mut peer).await;

    let r = send(
        &f.app,
        req(
            "POST",
            &skip_uri(),
            Some(COOKIE),
            Some(skip_body(&group, "skip-e7", None)),
        ),
    )
    .await;
    assert_eq!(r.status(), 200);

    let frames = drain_frames(&mut peer).await;
    assert!(
        frames
            .iter()
            .all(|frame| !forbidden_ipc_method(frame["method"].as_str().unwrap_or(""))),
        "skip 不得派发 answer/send/steer/stop/审批帧：{frames:?}"
    );
    assert!(
        !peer_has_traffic(&mut peer).await,
        "skip 完成后假 Desktop 必须再无任何 IPC 流量"
    );
    assert_eq!(
        f.count_rows("codex_actions"),
        0,
        "skip 不得新增普通消息/审批动作账行（answer/send/steer/stop 全 0）"
    );
    assert_eq!(
        f.count_rows("codex_question_skip_actions"),
        1,
        "恰一条 skip 动作行"
    );
    f.peer = Some(peer);
}

// ===========================================================================
// E4：restore（source 不在场仍可恢复展示；不重开 Native、不造待答卡）
// ===========================================================================

#[tokio::test]
async fn e4_restore_after_source_gone_returns_active_with_higher_revision() {
    let mut f = fixture();
    let (entities, turn_ids) = single_question_history();
    let projection = f.open_thread(entities, turn_ids).await;
    let (group, _) = single_group(&projection);

    let r = send(
        &f.app,
        req(
            "POST",
            &skip_uri(),
            Some(COOKIE),
            Some(skip_body(&group, "skip-e4", None)),
        ),
    )
    .await;
    assert_eq!(r.status(), 200);
    assert_eq!(f.display_row(&group), Some(("skipped".to_string(), 1)));

    // 让 live source 整体失效（关停即清 source/事实；restore 不得要求当前源在场）。
    f.state.codex_control.stop();
    let mut peer = f.peer.take().expect("peer");
    let _ = drain_frames(&mut peer).await;

    let r = send(
        &f.app,
        req(
            "POST",
            &restore_uri(),
            Some(COOKIE),
            Some(restore_body(&group, "restore-e4", 1)),
        ),
    )
    .await;
    assert_eq!(r.status(), 200, "sourceGone 仍必须可 restore");
    assert_eq!(cache_control(&r), Some("no-store"));
    let receipt = body_json(r).await;
    assert_eq!(receipt["actionId"], "restore-e4");
    assert_eq!(receipt["displayDecision"], "active");
    assert_eq!(
        receipt["revision"], 2,
        "restore 后 revision 必须 > expected"
    );
    assert_eq!(receipt["nativeEffect"], "unresolved");
    assert!(
        receipt["reasonCode"].is_null(),
        "restore 无原生面（不伪造原因）"
    );

    assert_eq!(
        f.display_row(&group),
        Some(("active".to_string(), 2)),
        "restore 只回滚 Tuvis 展示（CAS）"
    );
    assert_eq!(f.action_state("restore-e4").as_deref(), Some("finished"));
    // 不重开 Native/不派发消息：动作账只有 skip + restore 两行 skip 动作。
    assert_eq!(f.count_rows("codex_actions"), 0);
    assert_eq!(f.count_rows("codex_question_skip_actions"), 2);
    let frames = drain_frames(&mut peer).await;
    assert!(
        frames
            .iter()
            .all(|frame| !forbidden_ipc_method(frame["method"].as_str().unwrap_or(""))),
        "restore 不得重开 Native/派发任何动作帧：{frames:?}"
    );

    // 投影：历史账仍保留该组，外层展示态回到 active/revision 2（sourceGone →
    // display groups 省略，不伪造待答卡；条目只因失败降级而缺席）。
    let r = send(&f.app, req("GET", &thread_uri(), Some(COOKIE), None)).await;
    assert_eq!(r.status(), 200);
    let v = body_json(r).await;
    let history = v["questionSkipHistory"]
        .as_array()
        .expect("history 账（展示历史）");
    assert_eq!(history.len(), 1);
    assert_eq!(history[0]["decision"], "active");
    assert_eq!(history[0]["revision"], 2);
    assert_eq!(history[0]["group"], group);
    assert!(
        v["questionDisplayGroups"].is_null(),
        "source 不在场不得伪造 current display groups（不造待答卡）"
    );
    f.peer = Some(peer);
}

// ===========================================================================
// E5：幂等/恢复
// ===========================================================================

#[tokio::test]
async fn e5_same_action_id_same_package_replays_original_receipt() {
    let mut f = fixture();
    let (entities, turn_ids) = single_question_history();
    let projection = f.open_thread(entities, turn_ids).await;
    let (group, _) = single_group(&projection);
    let body = skip_body(&group, "skip-e5-replay", None);

    let mut peer = f.peer.take().expect("peer");
    let _ = drain_frames(&mut peer).await;

    let r1 = send(
        &f.app,
        req("POST", &skip_uri(), Some(COOKIE), Some(body.clone())),
    )
    .await;
    assert_eq!(r1.status(), 200);
    let first = body_json(r1).await;

    let r2 = send(&f.app, req("POST", &skip_uri(), Some(COOKIE), Some(body))).await;
    assert_eq!(r2.status(), 200, "同包重发必须回放原回执");
    let second = body_json(r2).await;
    assert_eq!(first, second, "同 actionId 同包必须逐字段同一原回执");
    assert_eq!(
        f.count_rows("codex_question_skip_actions"),
        1,
        "回放不得新增动作行（不重复 Invoke）"
    );
    assert_eq!(
        f.display_row(&group),
        Some(("skipped".to_string(), 1)),
        "回放不得推进展示 revision"
    );
    let frames = drain_frames(&mut peer).await;
    assert!(
        frames
            .iter()
            .all(|frame| !forbidden_ipc_method(frame["method"].as_str().unwrap_or(""))),
        "回放不得产生任何 Invoke 帧：{frames:?}"
    );
    f.peer = Some(peer);
}

#[tokio::test]
async fn e5_same_action_id_different_group_is_409_action_mismatch() {
    let mut f = fixture();
    let (entities, turn_ids) = single_question_history();
    let projection = f.open_thread(entities, turn_ids).await;
    let (group, _) = single_group(&projection);

    let r = send(
        &f.app,
        req(
            "POST",
            &skip_uri(),
            Some(COOKIE),
            Some(skip_body(&group, "skip-e5-mismatch", None)),
        ),
    )
    .await;
    assert_eq!(r.status(), 200);

    let other_group = group_value("different-turn", "different-item", "different-generation");
    let r = send(
        &f.app,
        req(
            "POST",
            &skip_uri(),
            Some(COOKIE),
            Some(skip_body(&other_group, "skip-e5-mismatch", None)),
        ),
    )
    .await;
    assert_eq!(r.status(), 409, "同 ID 异包必须 409");
    assert_eq!(cache_control(&r), Some("no-store"));
    let body = body_json(r).await;
    assert_eq!(body["error"], "action_mismatch");
    assert_eq!(
        f.count_rows("codex_question_skip_actions"),
        1,
        "异包拒绝不得新增动作行"
    );
}

/// E5：Prepared 存量行（跨进程遗留/已承接但未结算）重试 → 503 pending 且零 Invoke。
#[tokio::test]
async fn e5_persisted_prepared_retry_is_pending_with_zero_writes() {
    let mut f = fixture();
    // 与生产 `skip_canonical_payload` 同构的规范正文（preserve_order 下逐字相同）。
    let payload = serde_json::json!({
        "bindingId": serde_json::Value::Null,
        "group": group_value("turn-prepared", "item-prepared", "gen-prepared"),
        "kind": "skip",
    })
    .to_string();
    let digest = sha256_hex(&payload);
    {
        let conn = f.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "INSERT INTO codex_question_skip_actions
                (action_id, seq, kind, device_id, session_id, thread_host_id, thread_id,
                 group_turn_id, group_item_id, group_generation, canonical_payload,
                 payload_digest, state, revision_at_start, created_at)
             VALUES (?1, 1, 'skip', ?2, ?3, 'local', ?4, ?5, ?6, ?7, ?8, ?9, 'prepared', 0, ?10)",
            rusqlite::params![
                "skip-e5-prepared",
                DEVICE_ID,
                THREAD,
                THREAD,
                "turn-prepared",
                "item-prepared",
                "gen-prepared",
                payload,
                digest,
                1_i64,
            ],
        )
        .expect("insert prepared row");
    }

    let mut peer = f.peer.take().expect("peer");
    let body = skip_body(
        &group_value("turn-prepared", "item-prepared", "gen-prepared"),
        "skip-e5-prepared",
        None,
    );
    let r = send(&f.app, req("POST", &skip_uri(), Some(COOKIE), Some(body))).await;
    assert_eq!(r.status(), 503, "Prepared 存量重试必须保守 Pending");
    assert_eq!(cache_control(&r), Some("no-store"));
    let v = body_json(r).await;
    assert_eq!(v["error"], "operation_pending");
    assert_eq!(
        f.action_state("skip-e5-prepared").as_deref(),
        Some("prepared"),
        "重试不得推进/重写存量 Prepared 行"
    );
    assert_eq!(f.count_rows("codex_question_skip_actions"), 1);
    assert_eq!(
        f.count_rows("codex_question_display"),
        0,
        "重试不得写展示行"
    );
    let frames = drain_frames(&mut peer).await;
    assert!(
        frames
            .iter()
            .all(|frame| !forbidden_ipc_method(frame["method"].as_str().unwrap_or(""))),
        "Prepared 重试必须零 Invoke：{frames:?}"
    );
    f.peer = Some(peer);
}

// ===========================================================================
// E6：HTTP 断连不丢 owned 操作（durable 落账）
// ===========================================================================

#[tokio::test]
async fn e6_http_disconnect_after_accept_still_settles_durable() {
    let mut f = fixture();
    let (entities, turn_ids) = single_question_history();
    let projection = f.open_thread(entities, turn_ids).await;
    let (group, _) = single_group(&projection);

    // 用真实 router 的线程派发锁制造「已承接但 waiter 在途」的确定窗口：
    // 先持锁 → 请求注册 owned task 并阻塞在 lock_owned → 丢弃 HTTP future →
    // 释放锁 → owned task 必须继续走完 prepare/settle（不随连接断而丢）。
    let key = multi_agents_manager_lib::services::codex_desktop_ipc::types::ThreadKey {
        host_id: "local".into(),
        thread_id: THREAD.into(),
    };
    let guard = f
        .state
        .codex_control
        .router
        .thread_dispatch_lock(&key)
        .lock_owned()
        .await;

    tokio::time::sleep(Duration::from_millis(3)).await;
    let before = f.last_seen_at();
    let app = f.app.clone();
    let body = skip_body(&group, "skip-e6", None);
    let handle = tokio::spawn(async move {
        app.oneshot(req("POST", &skip_uri(), Some(COOKIE), Some(body)))
            .await
            .expect("oneshot")
    });

    // 有界等待 gate 放行（last_seen_at 推进 = 已过设备门；此后到注册无阻塞点）。
    let mut touched = false;
    for _ in 0..200 {
        if f.last_seen_at() != before {
            touched = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(touched, "请求未在时限内通过设备门");
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        !handle.is_finished(),
        "请求应仍在等待承接回执（锁在测试手里）"
    );
    handle.abort();
    drop(guard);

    let mut settled = None;
    for _ in 0..100 {
        if f.action_state("skip-e6").as_deref() == Some("finished") {
            settled = Some("finished");
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        settled,
        Some("finished"),
        "HTTP 断连后 owned job 仍必须 durable 落账（不得随 waiter 丢弃）"
    );
    assert_eq!(
        f.action_reason("skip-e6").as_deref(),
        Some("binding_required"),
        "断连后结算仍走零 Invoke 保守原因"
    );
    assert_eq!(
        f.display_row(&group),
        Some(("skipped".to_string(), 1)),
        "断连不丢展示冻结"
    );
    assert_eq!(
        f.count_rows("codex_question_skip_actions"),
        1,
        "断连不得重复承接"
    );
}

// ===========================================================================
// E8（补充）：binding 缺省/null 同义（明确 Tuvis-only）
// ===========================================================================

#[tokio::test]
async fn e8_binding_absent_and_null_are_equivalent_tuvis_only() {
    let mut f = fixture();
    let (entities, turn_ids) = single_question_history();
    let projection = f.open_thread(entities, turn_ids).await;
    let (group, _) = single_group(&projection);

    let absent = serde_json::json!({
        "sessionId": THREAD,
        "group": group,
        "actionId": "skip-absent-binding",
    })
    .to_string();
    // 防误拼：确认缺省键不存在
    assert!(!absent.contains("bindingId"));
    let r = send(&f.app, req("POST", &skip_uri(), Some(COOKIE), Some(absent))).await;
    assert_eq!(r.status(), 200, "binding 缺省 = 明确 Tuvis-only");
    let a = body_json(r).await;
    assert_eq!(a["displayDecision"], "skipped");
    assert_eq!(a["reasonCode"], "binding_required");

    let null_body = serde_json::json!({
        "sessionId": THREAD,
        "group": group,
        "actionId": "skip-null-binding",
        "bindingId": serde_json::Value::Null,
    })
    .to_string();
    let r = send(
        &f.app,
        req("POST", &skip_uri(), Some(COOKIE), Some(null_body)),
    )
    .await;
    assert_eq!(r.status(), 200, "binding null = 明确 Tuvis-only");
    let b = body_json(r).await;
    assert_eq!(b["displayDecision"], "skipped");
    assert_eq!(b["reasonCode"], "binding_required");
    assert_eq!(a["nativeEffect"], b["nativeEffect"]);
    assert_eq!(
        f.count_rows("codex_question_skip_actions"),
        2,
        "两个不同 actionId 各一行"
    );
}

// ===========================================================================
// M1（ledger-first）：Finished 回放不依赖旧 session/source/lease 在场
// ===========================================================================

/// M1 行为锁：skip 成功（Finished）后会话离场，同 actionId 同包重发必须仍回放
/// 原回执（逐字段一致）——修复前该场景在 session 定位处 404。
#[tokio::test]
async fn e5_ledger_first_replay_after_session_gone() {
    let mut f = fixture();
    let (entities, turn_ids) = single_question_history();
    let projection = f.open_thread(entities, turn_ids).await;
    let (group, _) = single_group(&projection);
    let body = skip_body(&group, "skip-e5-session-gone", None);

    let r1 = send(
        &f.app,
        req("POST", &skip_uri(), Some(COOKIE), Some(body.clone())),
    )
    .await;
    assert_eq!(r1.status(), 200, "首次 skip 必须被承接");
    let first = body_json(r1).await;
    assert_eq!(first["displayDecision"], "skipped");

    // 会话离场（扫描快照里不再有该会话）——New 分支的 fresh resolve 因此必然 404。
    f.set_sessions(Vec::new());
    let r = send(&f.app, req("GET", &thread_uri(), Some(COOKIE), None)).await;
    assert_eq!(r.status(), 404, "夹具前提：会话离场后 GET 必须 404");

    let r2 = send(&f.app, req("POST", &skip_uri(), Some(COOKIE), Some(body))).await;
    assert_eq!(
        r2.status(),
        200,
        "M1：Finished 同包回放必须零旧 session 依赖（修复前此处 404）"
    );
    assert_eq!(cache_control(&r2), Some("no-store"));
    let second = body_json(r2).await;
    assert_eq!(first, second, "回放必须逐字段同一原回执");
    assert_eq!(
        f.count_rows("codex_question_skip_actions"),
        1,
        "回放不得新增动作行"
    );
    assert_eq!(
        f.display_row(&group),
        Some(("skipped".to_string(), 1)),
        "回放不得推进展示 revision"
    );
}

/// M1 反向边界：会话离场后**新** actionId 仍必须 404（ledger-first 不放行
/// 「账内无行、无法 fresh resolve」的新请求，也不伪造回执）。
#[tokio::test]
async fn e5_ledger_first_new_action_after_session_gone_is_404() {
    let mut f = fixture();
    let (entities, turn_ids) = single_question_history();
    let projection = f.open_thread(entities, turn_ids).await;
    let (group, _) = single_group(&projection);

    f.set_sessions(Vec::new());
    let r = send(
        &f.app,
        req(
            "POST",
            &skip_uri(),
            Some(COOKIE),
            Some(skip_body(&group, "skip-e5-new-after-gone", None)),
        ),
    )
    .await;
    assert_eq!(r.status(), 404, "新请求仍受当前会话存在性约束");
    assert_eq!(cache_control(&r), Some("no-store"));
    assert_eq!(
        f.count_rows("codex_question_skip_actions"),
        0,
        "404 不得写动作行"
    );
    assert_eq!(
        f.count_rows("codex_question_display"),
        0,
        "404 不得写展示行"
    );
}

// ===========================================================================
// M4（pair_pin 即时 Device 事实）：新配对设备立刻可通过 Device 门
// ===========================================================================

/// E9（鉴权/动作归属面）：`/pair/pin` 新配对的设备用其 cookie 可以立即通过 HTTP
/// 设备门并完成一次 skip，动作行的 `device_id` 归属该新设备。
///
/// **覆盖边界（Major 2 修正）**：本用例走的是无绑定 Tuvis-only skip，它不经
/// Device **live 事实**门（无绑定路径不建 attempt guard）。因此这里**只**声称
/// 「cookie 鉴权通过 + 动作归属正确」，不声称 Device live 门。
/// Device live 门（配对写事实 → `guard_for_attempt`+`recheck` 由 Unknown 变 clear、
/// 以及同指纹 upsert 命中旧行 id 槽位）的验证归属：
/// `src-tauri/src/remote/codex_question_skip_vertical_tests.rs` 的
/// `m4b_pair_writes_device_fact_on_real_upsert_target` / `m4b_without_device_fact_commit_paired_row_stays_unknown`
/// （那里有完整的 Fake Native 栈与真实 guard 观察口）。
#[tokio::test]
async fn e9_new_paired_device_cookie_is_authorized_for_skip() {
    let mut f = fixture();
    let (entities, turn_ids) = single_question_history();
    let projection = f.open_thread(entities, turn_ids).await;
    let (group, _) = single_group(&projection);

    // 夹具里的 dev-1 是直接 seed 的（未经 pair_pin）；这里用 pair_pin 真配对一台新设备。
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/m/api/v1/pair/pin")
        .header("content-type", "application/json")
        .header("user-agent", "ua-pair-e9")
        .extension(axum::extract::ConnectInfo(
            "127.0.0.1:40123".parse::<std::net::SocketAddr>().unwrap(),
        ))
        .body(axum::body::Body::from(r#"{"pin":"1234","name":"新设备"}"#))
        .unwrap();
    let r = send(&f.app, request).await;
    assert_eq!(r.status(), 200, "PIN 正确必须配对成功");
    let cookie_header = r
        .headers()
        .get("set-cookie")
        .expect("pair_pin 必须下发 cookie")
        .to_str()
        .unwrap()
        .to_string();
    let new_device = cookie_header.split(';').next().unwrap().trim().to_string();
    assert!(
        new_device.starts_with("mam_device="),
        "cookie 形态：{new_device}"
    );
    let new_id = new_device.trim_start_matches("mam_device=").to_string();
    assert_ne!(new_id, DEVICE_ID, "必须是新设备行");
    assert_eq!(
        f.count_rows("remote_devices"),
        2,
        "dev-1 + 新配对设备各一行"
    );

    // 新设备 cookie 走一次 skip：只断言「设备门放行 + 动作归属」，不断言 Native 面。
    let r = send(
        &f.app,
        req(
            "POST",
            &skip_uri(),
            Some(&new_device),
            Some(skip_body(&group, "skip-e9-new-device", None)),
        ),
    )
    .await;
    assert_eq!(
        r.status(),
        200,
        "E9：pair_pin 后新设备 cookie 必须能通过 HTTP 设备门"
    );
    let receipt = body_json(r).await;
    assert_eq!(receipt["displayDecision"], "skipped");
    // 设备归属必须来自 cookie 的新设备（不是 dev-1）。
    let owner: String = {
        let conn = f.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.query_row(
            "SELECT device_id FROM codex_question_skip_actions WHERE action_id = ?1",
            ["skip-e9-new-device"],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(owner, new_id, "动作行的设备必须是新配对设备");
}

// 说明：`database::dao::codex_question_display` 是 `pub(crate)`——集成 crate 无法
// 直调 DAO。本文件全部展示账断言走**同一内存库**的只读 SQL（主键/词表与 schema
// 逐字对齐），不引入第二份事实源。
