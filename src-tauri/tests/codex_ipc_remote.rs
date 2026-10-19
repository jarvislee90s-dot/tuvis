//! Codex Desktop IPC 远程控制（计划 Task 8）HTTP 集成测试：认证后的新端点、
//! session-send/send-info 的 hub 接线、兼容兜底 H3 门、审计与生命周期。
//!
//! 零真实副作用纪律：`DeviceStore::Owned(共享内存连接)`（state.store 与 hub 的
//! ActionStore 同库同连接）、Desktop IPC 走 Tokio duplex `ScriptedPeer`（本地假
//! Desktop，**绝不连接真实官方 Desktop**）、home_source 恒 None（rollout 解析只走
//! 会话号 UUID 回退，**绝不读真实 ~/.codex**）；兜底路径的 CLI 发现经临时空 PATH
//! 隔离（**绝不 spawn 真 codex**）。

use multi_agents_manager_lib::remote::codex_ipc::CodexControlHub;

#[path = "support/codex_fixture.rs"]
mod codex_fixture;
use multi_agents_manager_lib::remote::pairing::{
    persist_device, revoke_device, DeviceStore, NewDevice,
};
use multi_agents_manager_lib::remote::server::{router, RemoteState};
use multi_agents_manager_lib::services::codex_control::{
    payload_hash, ActionContext, ActionKind, ActionPayload, ActionReceipt, ActionState,
    ActionStore, ControlRoute, Delivery, ThreadKey,
};
use multi_agents_manager_lib::services::codex_desktop_ipc::adapter::{
    Connector, DesktopIpcAdapter,
};
use multi_agents_manager_lib::services::codex_desktop_ipc::codec::{encode_frame, FrameDecoder};
use multi_agents_manager_lib::services::codex_desktop_ipc::profile::profile_for;
use multi_agents_manager_lib::services::codex_desktop_ipc::transport::{BoxIo, IpcIo};
use multi_agents_manager_lib::session::{
    AgentType, ProcessForm, Session, SessionStatus, SessionsResponse,
};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tower::ServiceExt as _;

// ============================================================
// 假 Desktop IPC：duplex 帧级 peer（与 codex_desktop_ipc 测试同构）
// ============================================================

struct ScriptedPeer {
    io: DuplexStream,
    backlog: std::collections::VecDeque<serde_json::Value>,
}

impl ScriptedPeer {
    fn new() -> (Box<dyn IpcIo>, Self) {
        let (client, server) = tokio::io::duplex(64 * 1024);
        (
            Box::new(client),
            Self {
                io: server,
                backlog: std::collections::VecDeque::new(),
            },
        )
    }

    async fn recv(&mut self) -> serde_json::Value {
        if let Some(f) = self.backlog.pop_front() {
            return f;
        }
        let mut decoder = FrameDecoder::new();
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            let n = self.io.read(&mut buf).await.expect("peer read");
            assert!(n > 0, "peer EOF");
            let mut frames = decoder.push(&buf[..n]).expect("decode");
            if frames.is_empty() {
                continue;
            }
            let first = frames.remove(0);
            for f in frames {
                self.backlog.push_back(f);
            }
            return first;
        }
    }

    async fn send(&mut self, value: &serde_json::Value) {
        let frame = encode_frame(value).expect("encode");
        self.io.write_all(&frame).await.expect("peer write");
    }

    fn close(self) {
        drop(self.io);
    }
}

struct StaticConnector {
    endpoint: multi_agents_manager_lib::services::codex_desktop_ipc::transport::DesktopEndpoint,
    io: StdMutex<Option<Box<dyn IpcIo>>>,
}

/// R3 起 adapter 每次连接按 endpoint 重算 profile——假 Desktop 必须自报**已验证
/// build** 的 endpoint 身份，能力由生产 `profile_for(platform, build)` 派生（绝不注入
/// 固定 profile 替代 resolver 链）。选 windows 已验证 build：send+state+command+
/// blocking_input 全开（本套件的审批/作答分派链全覆盖；permissions 未实测档与本
/// 套件无分派交集）。
const TEST_BUILD: &str = "26.1013.3656.0";

impl StaticConnector {
    fn new(io: Box<dyn IpcIo>) -> Self {
        Self {
            endpoint:
                multi_agents_manager_lib::services::codex_desktop_ipc::transport::DesktopEndpoint {
                    platform: "windows".into(),
                    path: "\\\\.\\pipe\\codex-ipc-test-fixture".into(),
                    build: Some(TEST_BUILD.into()),
                },
            io: StdMutex::new(Some(io)),
        }
    }
}

impl Connector for StaticConnector {
    fn connect_endpoint(
        &self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        (
                            BoxIo,
                            multi_agents_manager_lib::services::codex_desktop_ipc::transport::DesktopEndpoint,
                        ),
                        multi_agents_manager_lib::services::codex_desktop_ipc::types::ProtocolError,
                    >,
                > + Send
                + '_,
        >,
    >{
        Box::pin(async move {
            let io = self
                .io
                .lock()
                .expect("lock")
                .take()
                .ok_or_else(|| multi_agents_manager_lib::services::codex_desktop_ipc::types::ProtocolError::Disconnected("no io".into()))?;
            Ok((io, self.endpoint.clone()))
        })
    }
}

/// 构造期基线 profile（R3：仅首个连接前生效，连接后按 endpoint 重算——同值）。
fn caps_open_profile(
) -> multi_agents_manager_lib::services::codex_desktop_ipc::profile::ProtocolProfile {
    profile_for("windows", TEST_BUILD).unwrap()
}

async fn handshake(peer: &mut ScriptedPeer, client_id: &str) {
    let init = peer.recv().await;
    assert_eq!(init["method"], "initialize");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": init["requestId"],
        "resultType": "success", "method": "initialize",
        "handledByClientId": client_id, "result": { "clientId": client_id }
    }))
    .await;
}

fn discovery_response(request_id: &str, owner: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "response", "requestId": request_id,
        "resultType": "success", "method": "thread-owner-discovery",
        "handledByClientId": owner,
        "result": { "supportsUntrustedAppInput": true }
    })
}

/// 驱动 ensure_thread 全程：initialize → discovery → following → snapshot。
/// `runtime` 控制忙态（active = busy 留队 / idle = 可立即派发）。
async fn drive_ensure(
    peer: &mut ScriptedPeer,
    thread: &str,
    requests: Vec<serde_json::Value>,
    runtime: &str,
) {
    handshake(peer, "owner-1").await;
    let disc = peer.recv().await;
    assert_eq!(disc["method"], "thread-owner-discovery");
    peer.send(&discovery_response(
        disc["requestId"].as_str().unwrap(),
        "owner-1",
    ))
    .await;
    let follow = peer.recv().await;
    assert_eq!(follow["method"], "thread-stream-following-changed");
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": thread, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": 7,
                "conversationState": {
                    "turns": [], "requests": requests,
                    "threadRuntimeStatus": { "type": runtime },
                    "turnHistory": codex_fixture::canonical_turn_history(serde_json::json!({
                            "t1": { "id": "t1", "type": "turn", "status": "completed", "items": ["u1"] },
                            "u1": { "id": "u1", "type": "user_message", "text": "seed", "ts": 1 }
                        }), serde_json::json!([{ "entries": ["t1"] }]))
                }
            }
        }
    }))
    .await;
}

/// 有界探测 peer 是否收到任何字节（true = 有流量；「零写入」断言要求 false）。
async fn peer_has_traffic(peer: &mut ScriptedPeer) -> bool {
    let mut buf = vec![0u8; 1024];
    matches!(
        tokio::time::timeout(Duration::from_millis(300), peer.io.read(&mut buf)).await,
        Ok(Ok(n)) if n > 0
    )
}

// ============================================================
// RemoteState fixture
// ============================================================

type SharedConn = Arc<StdMutex<rusqlite::Connection>>;

fn shared_conn() -> SharedConn {
    let conn = rusqlite::Connection::open_in_memory().expect("open memory db");
    multi_agents_manager_lib::database::schema::init(&conn);
    multi_agents_manager_lib::database::migration::migrate(&conn).expect("migrate");
    Arc::new(StdMutex::new(conn))
}

fn seed_device(conn: &SharedConn, id: &str, name: &str) {
    let dev = NewDevice {
        id: id.to_string(),
        name: name.to_string(),
        ua: format!("ua-{id}"),
        origin_ip: format!("127.0.0.1#{id}"),
        via: "lan".into(),
        paired_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0),
    };
    let store = DeviceStore::Owned(conn.clone());
    store.with(|c| persist_device(c, &dev).expect("seed device"));
}

/// codex APP 会话（id 必须 UUID 形态——thread 解析的回退来源）。
fn codex_app_session(id: &str) -> Session {
    Session {
        id: id.into(),
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
        last_activity_at: "2026-10-16T00:00:00Z".into(),
        pid: 4242,
        cpu_usage: 0.0,
        active_subagent_count: 0,
        form: ProcessForm::App,
        jump_supported: false,
        unread: false,
    }
}

fn claude_cli_session(id: &str, status: SessionStatus) -> Session {
    Session {
        agent_type: AgentType::Claude,
        status,
        pid: 7,
        form: ProcessForm::Cli,
        ..codex_app_session(id)
    }
}

/// rollout 问答形态的消息页（补投通道 B 判据：tool-call + questions 形态 args + 无 result）。
fn question_message_page(
    _session_id: &str,
) -> multi_agents_manager_lib::remote::content::MessagesPage {
    multi_agents_manager_lib::remote::content::MessagesPage {
        messages: vec![multi_agents_manager_lib::remote::content::SessionMessage {
            seq: 0,
            role: "assistant".into(),
            kind: "tool-call".into(),
            content: "request_user_input".into(),
            ts: Some(1),
            tool_name: Some("request_user_input".into()),
            tool_args: Some(
                r#"{"questions":[{"question":"补投题面","options":[{"label":"甲"},{"label":"乙"}]}]}"#
                    .into(),
            ),
            collapsed: false,
        }],
        truncated: false,
        cwd: None,
    }
}

struct Fixture {
    conn: SharedConn,
    state: Arc<RemoteState>,
    app: axum::Router,
    peer: Option<ScriptedPeer>,
}

/// hub 可用（A 可写）fixture：注入 ScriptedPeer 假 IPC 的真 adapter。
fn fixture_ipc(sessions: Vec<Session>) -> Fixture {
    let (io, peer) = ScriptedPeer::new();
    let adapter = Arc::new(DesktopIpcAdapter::new(
        Arc::new(StaticConnector::new(io)),
        caps_open_profile(),
    ));
    fixture_with(
        sessions,
        move |store| Arc::new(CodexControlHub::with_adapter(store, adapter, true)),
        Some(peer),
    )
}

/// hub 不可用 fixture（disabled：一切能力关闭）。R8 起普通消息仍经 router（登记→
/// reserve→NotSent→兜底资格），故 hub 的 ActionStore 必须与 state.store 同库——
/// 设备复核才能放行到「兜底拒发」臂。
fn fixture_disabled(sessions: Vec<Session>) -> Fixture {
    fixture_with(
        sessions,
        |store| Arc::new(CodexControlHub::disabled_with_store(store)),
        None,
    )
}

fn fixture_with(
    sessions: Vec<Session>,
    hub_builder: impl FnOnce(DeviceStore) -> Arc<CodexControlHub>,
    peer: Option<ScriptedPeer>,
) -> Fixture {
    let conn = shared_conn();
    seed_device(&conn, "dev-1", "iPhone");
    fixture_on(conn, sessions, hub_builder, peer)
}

/// 指定共享内存库的 fixture（R7.1 重启恢复测试用：两阶段 hub 同库——「丢弃全部
/// 内存重建 hub，旧 queued 行在恢复后按序发出」）。conn 须已完成 seed_device。
fn fixture_on(
    conn: SharedConn,
    sessions: Vec<Session>,
    hub_builder: impl FnOnce(DeviceStore) -> Arc<CodexControlHub>,
    peer: Option<ScriptedPeer>,
) -> Fixture {
    let store = DeviceStore::Owned(conn.clone());
    let has_question_page = sessions.iter().any(|s| s.agent_type.tool_id() == "codex");
    let state = Arc::new(RemoteState {
        ui_config_source: Box::new(|| None),
        subagent_source: std::collections::HashMap::new(),
        subagent_message_source: std::collections::HashMap::new(),
        target_evidence: Box::new(|_, _| {
            multi_agents_manager_lib::window::tty_map::TargetEvidence::default()
        }),
        capability_table: multi_agents_manager_lib::inject::capability::new_table(),
        session_source: {
            let sessions = sessions.clone();
            Box::new(move || SessionsResponse {
                sessions: sessions.clone(),
                total_count: sessions.len(),
                waiting_count: 0,
            })
        },
        pairing_counter: Box::new(Vec::new),
        board_hidden_ids: Box::new(Vec::new),
        board_hidden_hide: Arc::new(|_| 0usize),
        board_hidden_unhide: Arc::new(|_| 0usize),
        unread_mark_read: Arc::new(|_, _| ()),
        session_close: Arc::new(|_| Ok(())),
        store,
        host_source: Box::new(|| serde_json::json!({ "host": { "name": "t" } })),
        message_source: Box::new(move |agent: &str, sid: &str, _limit: usize| {
            if agent == "codex" && has_question_page {
                Ok(question_message_page(sid))
            } else {
                Err("测试桩：无内容".to_string())
            }
        }),
        path_source: Box::new(|_, _, _| (Vec::new(), false)),
        watcher_tx: tokio::sync::broadcast::channel(64).0,
        sse_registry: Arc::new(multi_agents_manager_lib::remote::server::SseRegistry::default()),
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
        create_hub: Arc::new(multi_agents_manager_lib::remote::server::CreateTaskHub::stub()),
        archive_source: Box::new(Vec::new),
        archive_delete: Arc::new(|_: Option<&str>| 0usize),
        confirm_probe: Arc::new(|_, _, _| true),
        dialog_probe: Arc::new(|_, _| None),
        screen_probe: Arc::new(|_, _| None),
        codex_control: hub_builder(DeviceStore::Owned(conn.clone())),
    });
    let app = router(state.clone());
    Fixture {
        conn,
        state,
        app,
        peer,
    }
}

impl Fixture {
    fn hub_store(&self) -> Arc<ActionStore> {
        self.state.codex_control.router.store().clone()
    }

    fn thread_key(&self) -> ThreadKey {
        ThreadKey {
            host_id: "local".into(),
            thread_id: THREAD.into(),
        }
    }

    fn codex_audits(&self) -> Vec<multi_agents_manager_lib::database::dao::write_audit::AuditRow> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT ts, device_name, agent_type, session_id, channel, action, summary, result FROM write_audit WHERE action LIKE 'codex_%' ORDER BY id")
            .unwrap();
        stmt.query_map([], |r| {
            Ok(
                multi_agents_manager_lib::database::dao::write_audit::AuditRow {
                    ts: r.get(0)?,
                    device_name: r.get(1)?,
                    agent_type: r.get(2)?,
                    session_id: r.get(3)?,
                    channel: r.get(4)?,
                    action: r.get(5)?,
                    summary: r.get(6)?,
                    result: r.get(7)?,
                },
            )
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect()
    }

    fn all_audit_summaries(&self) -> Vec<String> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT summary FROM write_audit").unwrap();
        stmt.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
    }
}

// ---- 请求样板 ----

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

const COOKIE: &str = "mam_device=dev-1";
const THREAD: &str = "0f1e2d3c-1111-4222-8333-444455556666";

async fn body_string(b: axum::http::Response<axum::body::Body>) -> String {
    use http_body_util::BodyExt;
    String::from_utf8(b.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap()
}

/// POST /session-send 的便捷构造。
fn send_body(text: &str, action_id: Option<&str>, queue_only: bool) -> String {
    let mut v = serde_json::json!({ "sessionId": THREAD, "text": text });
    if let Some(a) = action_id {
        v["actionId"] = serde_json::json!(a);
    }
    if queue_only {
        v["queueOnly"] = serde_json::json!(true);
    }
    v.to_string()
}

// ============================================================
// 1. 认证矩阵：新路由无 cookie / 吊销 = 403（回环同样拒绝——gate 零豁免）
// ============================================================

#[tokio::test]
async fn new_routes_require_device_cookie() {
    let f = fixture_disabled(vec![codex_app_session(THREAD)]);
    let app = f.app.clone();
    for uri in [
        "/m/api/v1/codex/thread?session_id=x",
        "/m/api/v1/codex/action?action_id=a",
        "/m/api/v1/codex/queue?session_id=x",
    ] {
        let r = app
            .clone()
            .oneshot(req("GET", uri, None, None))
            .await
            .unwrap();
        assert_eq!(r.status(), 403, "无 cookie 必须 403：{uri}");
        // 回环同样拒绝：无 Host 豁免（gate 零豁免结构不变）
        let r = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri(uri)
                    .header("host", "127.0.0.1:9420")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), 403, "回环同样拒绝：{uri}");
    }
    for uri in [
        "/m/api/v1/codex/queue/retract",
        "/m/api/v1/codex/queue/force-continue",
        "/m/api/v1/codex/pending-response",
    ] {
        let r = app
            .clone()
            .oneshot(req("POST", uri, None, Some("{}".into())))
            .await
            .unwrap();
        assert_eq!(r.status(), 403, "POST 无 cookie 必须 403：{uri}");
    }
}

/// 吊销设备后 cookie 即刻失效（TTL 内也拒）。
#[tokio::test]
async fn revoked_device_gets_403_on_new_routes() {
    let f = fixture_disabled(vec![codex_app_session(THREAD)]);
    DeviceStore::Owned(f.conn.clone()).with(|c| revoke_device(c, "dev-1").unwrap());
    let r = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 403, "吊销设备必须 403");
}

// ============================================================
// 2. GET /codex/thread：投影 + 404 语义
// ============================================================

#[tokio::test]
async fn thread_state_returns_live_projection() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("thread_state in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("no-store"),
        "投影响应必须 no-store"
    );
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["threadId"], THREAD);
    assert_eq!(v["stale"], serde_json::json!(false));
    assert_eq!(v["availability"], "controllable");
    assert_eq!(v["runtimeStatus"], "idle");
}

/// thread 不可解析（非 UUID 且无 rollout）→ 404。
#[tokio::test]
async fn thread_state_unresolvable_session_is_404() {
    let mut f = fixture_ipc(vec![codex_app_session("plain-session-name")]);
    let peer = f.peer.take().unwrap();
    let r = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            "/m/api/v1/codex/thread?session_id=plain-session-name",
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 404, "非 codex thread 解析失败必须 404");
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["error"], "no_session");
    peer.close();
}

// ============================================================
// 3. session-send hub 接线
// ============================================================

/// H3=false（默认关）且 A 可用 → IPC 正常：desktop_ipc 回执 + accepted + 账本落账。
#[tokio::test]
async fn send_h3_off_ipc_available_uses_ipc() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let body = send_body("通过 IPC 发送", Some("act-ipc-1"), false);
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let frame = peer.recv().await;
    assert_eq!(frame["method"], "thread-follower-start-turn");
    assert_eq!(
        frame["params"]["turnStart"]["request"]["input"][0]["text"],
        "通过 IPC 发送 [mobile iPhone]"
    );
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "turn": { "id": "turn-9", "status": "inProgress" } }
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(20), task)
        .await
        .expect("send in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["status"], "desktop_ipc");
    assert_eq!(v["receipt"]["delivery"], "accepted");
    assert_eq!(v["receipt"]["route"], "desktop_ipc");
    assert_eq!(v["receipt"]["turnId"], "turn-9");
    let view = f
        .hub_store()
        .get_for_device("act-ipc-1", "dev-1")
        .unwrap()
        .unwrap();
    assert_eq!(view.state, ActionState::Accepted);
    let audits = f.codex_audits();
    assert!(
        audits.iter().any(|a| a.action == "codex_send"
            && a.channel == "codex_desktop_ipc"
            && a.summary.contains("act-ipc-1")
            && a.result.contains("accepted")),
        "send 有可追踪审计行：{:?}",
        audits
    );
}

/// 忙 Thread（快照 active）→ codex_queued + position（宪法 R2.3 留队）。
#[tokio::test]
async fn send_busy_thread_queues_with_position() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    // 先以 idle 快照建立跟随（镜像在先），再推 active 增量——忙判定从镜像读，
    // 未跟随的 Thread 首发无法预知忙态（R2.3 的排队由 Desktop 自身队列与后续
    // active_turns 承担；此处验证的是「已知忙 → 留队」的 FIFO 语义）
    {
        let app = f.app.clone();
        let task = tokio::spawn(async move {
            app.oneshot(req(
                "GET",
                &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
                Some(COOKIE),
                None,
            ))
            .await
            .unwrap()
        });
        drive_ensure(&mut peer, THREAD, vec![], "idle").await;
        let r = tokio::time::timeout(Duration::from_secs(15), task)
            .await
            .expect("follow in time")
            .unwrap();
        assert_eq!(r.status(), 200);
    }
    // 推 active 增量（state-changed）→ 投影/镜像更新
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": THREAD, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": 8,
                "conversationState": {
                    "turns": [], "requests": [],
                    "threadRuntimeStatus": { "type": "active" },
                    "turnHistory": codex_fixture::canonical_turn_history(serde_json::json!({
                            "t2": { "id": "t2", "type": "turn", "status": "inProgress", "items": ["u2"] },
                            "u2": { "id": "u2", "type": "user_message", "text": "running", "ts": 2 }
                        }), serde_json::json!([{ "entries": ["t2"] }]))
                }
            }
        }
    }))
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let body = send_body("忙态消息", Some("act-q-1"), false);
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("send in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["status"], "codex_queued");
    assert_eq!(v["actionId"], "act-q-1");
    assert_eq!(v["position"], serde_json::json!(1));
    let view = f
        .hub_store()
        .get_for_device("act-q-1", "dev-1")
        .unwrap()
        .unwrap();
    assert_eq!(view.state, ActionState::Queued);
}

/// Codex APP 缺 actionId → 400（不代造，防双投）；零 IPC 写入。
#[tokio::test]
async fn send_missing_action_id_is_400_and_zero_writes() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(send_body("没有 actionId", None, false)),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["reasonCode"], "action_id_required");
    assert!(
        !peer_has_traffic(&mut peer).await,
        "缺 actionId 不得产生任何 IPC 写入"
    );
}

/// A 不可用（disabled hub）且 H3=false → 旧兼容分支如实拒绝（403 headless_disabled）。
#[tokio::test]
async fn send_h3_off_ipc_unavailable_compat_refuses() {
    let f = fixture_disabled(vec![codex_app_session(THREAD)]);
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(send_body("hi", Some("a1"), false)),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["error"], "headless_disabled");
}

/// IPC 明确拒绝（peer 回 error）→ Rejected 回执 200，**不触发 fallback**（H3 开着
/// 也不降级——非 NotSent 绝不交接）。
#[tokio::test]
async fn send_rejected_delivery_never_falls_back() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    // H3 打开（内存库 seed 开关）
    f.state.store.with(|c| {
        c.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('remote.headless_enabled', 'true')",
            [],
        )
        .unwrap();
    });
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let body = send_body("会被拒绝", Some("act-rej-1"), false);
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let frame = peer.recv().await;
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "error", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "error": { "message": "not allowed" }
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(20), task)
        .await
        .expect("in time")
        .unwrap();
    assert_eq!(r.status(), 200, "明确拒绝是 200 的明确回执");
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["status"], "desktop_ipc");
    assert_eq!(v["receipt"]["delivery"], "rejected");
    let audits = f.codex_audits();
    assert!(
        !audits.iter().any(|a| a.action == "codex_fallback"),
        "拒绝绝不降级：{:?}",
        audits
    );
    assert!(audits
        .iter()
        .any(|a| a.action == "codex_send" && a.result.contains("rejected")));
}

/// H3=true + A 可用 + IPC NotSent（对端断连）→ 兼容兜底交接：兜底封套附 actionId；
/// 空 PATH 下 CLI 缺失 → 投递前明确拒绝 NotSent 落账（零 spawn，fence 同事务清）。
#[tokio::test]
async fn send_not_sent_hands_off_to_compat_when_h3_on() {
    let _path = IsolatedPath::lock_empty();
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    f.state.store.with(|c| {
        c.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('remote.headless_enabled', 'true')",
            [],
        )
        .unwrap();
    });
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let body = send_body("降级消息", Some("act-cmp-1"), false);
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    // 握手进行到 following 后立刻断连（EOF）→ writer NotSent → handoff → fallback
    handshake(&mut peer, "owner-1").await;
    let disc = peer.recv().await;
    peer.send(&discovery_response(
        disc["requestId"].as_str().unwrap(),
        "owner-1",
    ))
    .await;
    let _follow = peer.recv().await;
    peer.close();
    let r = tokio::time::timeout(Duration::from_secs(30), task)
        .await
        .expect("compat in time")
        .unwrap();
    assert_eq!(r.status(), 200, "兜底回执走 headless 封套（HTTP 200）");
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["status"], "headless");
    assert_eq!(v["receipt"]["status"], "failed");
    assert_eq!(v["actionId"], "act-cmp-1", "兜底封套附 actionId");
    // 账本：NotSent（CLI 缺失 = 投递前明确拒绝），fence 无残留
    let view = f
        .hub_store()
        .get_for_device("act-cmp-1", "dev-1")
        .unwrap()
        .unwrap();
    assert_eq!(view.state, ActionState::Rejected);
    let receipt = view.receipt.unwrap();
    assert_eq!(receipt.delivery, Delivery::NotSent);
    assert_eq!(receipt.route, ControlRoute::HeadlessCompat);
    assert!(
        !f.hub_store().has_compat_fence(&f.thread_key()).unwrap(),
        "零投递 NotSent 同事务清 fence"
    );
    let audits = f.codex_audits();
    assert!(
        audits
            .iter()
            .any(|a| a.action == "codex_fallback" && a.summary.contains("act-cmp-1")),
        "降级有可追踪审计行：{:?}",
        audits
    );
}

/// 进程级 PATH 隔离：兜底测试期间 PATH 指向空目录（无 codex CLI → 拒发不 spawn）。
/// 互斥锁串行化本文件内的兜底用例，Drop 恢复原 PATH。
struct IsolatedPath {
    _g: std::sync::MutexGuard<'static, ()>,
    orig: Option<std::ffi::OsString>,
}

impl IsolatedPath {
    fn lock_empty() -> Self {
        static LOCK: StdMutex<()> = StdMutex::new(());
        static ORIG: std::sync::OnceLock<std::ffi::OsString> = std::sync::OnceLock::new();
        let g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let orig = ORIG
            .get_or_init(|| std::env::var_os("PATH").unwrap_or_default())
            .clone();
        std::env::set_var("PATH", "/nonexistent-codex-ipc-test-path");
        Self {
            _g: g,
            orig: Some(orig),
        }
    }
}

impl Drop for IsolatedPath {
    fn drop(&mut self) {
        if let Some(p) = self.orig.take() {
            std::env::set_var("PATH", p);
        }
    }
}

/// R9.2：A 路派发由独立 tokio 任务持有至结果落账——HTTP 等待者被**测试主动中止**
///（abort handler future，非真实连接关闭）后，Desktop 侧成功回执 → 账本自动
/// Accepted（不能永久 Reserved、绝不回 queued），零二次写入。
#[tokio::test]
async fn aborted_http_waiter_does_not_lose_dispatched_write() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let body = send_body("断连消息", Some("act-disc-1"), false);
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let frame = peer.recv().await;
    assert_eq!(
        frame["method"], "thread-follower-start-turn",
        "写动作已发出"
    );
    // 此时中止 HTTP 等待者（测试主动取消——不模拟真实连接关闭语义）
    task.abort();
    // Desktop 侧仍正常应答：独立派发任务收到回执并落账
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "turn": { "id": "turn-d", "status": "inProgress" } }
    }))
    .await;
    // 轮询等独立任务落账（不依赖 HTTP 等待者）
    let view = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let v = f
                .hub_store()
                .get_for_device("act-disc-1", "dev-1")
                .unwrap()
                .unwrap();
            if v.state != ActionState::Reserved {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("dispatch task settles");
    assert_eq!(
        view.state,
        ActionState::Accepted,
        "等待者中止后独立任务继续落账（R9.2）"
    );
    assert_eq!(view.receipt.unwrap().turn_id.as_deref(), Some("turn-d"));
    assert!(!peer_has_traffic(&mut peer).await, "中止后无二次写入");
}

/// R9.2 另一面：写帧已发出后 **peer 真实断连**（EOF）→ 在飞 mutating RPC 按
/// UnknownDelivery 如实落账（不虚称失败、不回 queued、不重发）。
#[tokio::test]
async fn peer_eof_after_write_marks_unknown() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let body = send_body("对端断连消息", Some("act-eof-1"), false);
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let frame = peer.recv().await;
    assert_eq!(frame["method"], "thread-follower-start-turn");
    // 对端真实断连（EOF）——在飞 RPC 结果未知
    peer.close();
    let r = tokio::time::timeout(Duration::from_secs(20), task)
        .await
        .expect("in time")
        .unwrap();
    assert_eq!(r.status(), 200, "unknown 是 200 的明确回执");
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["receipt"]["delivery"], "unknown_delivery");
    let view = f
        .hub_store()
        .get_for_device("act-eof-1", "dev-1")
        .unwrap()
        .unwrap();
    assert_eq!(view.state, ActionState::Unknown, "断连自动 Unknown");
}

// ============================================================
// 4. session-send-info：A 可用不看 H3
// ============================================================

#[tokio::test]
async fn send_info_h3_off_ipc_available_injectable_true() {
    let f = fixture_ipc(vec![codex_app_session(THREAD)]);
    f.state.store.with(|c| {
        c.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('remote.headless_enabled', 'false')",
            [],
        )
        .unwrap();
    });
    let r = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/m/api/v1/session-send-info?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(
        v["injectable"],
        serde_json::json!(true),
        "A 可用时 H3 关不置灰（真实 composer 路径）"
    );
    // 通道词保持 headless_codex_*（移动端据此携带 actionId）
    assert_eq!(
        v["channels"],
        serde_json::json!(["headless_codex_queue", "headless_codex_exec"])
    );
}

#[tokio::test]
async fn send_info_h3_off_ipc_unavailable_still_disabled() {
    let f = fixture_disabled(vec![codex_app_session(THREAD)]);
    let r = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/m/api/v1/session-send-info?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["injectable"], serde_json::json!(false));
    assert_eq!(v["reasonCode"], "headless_disabled");
}

// ============================================================
// 5. pending-response
// ============================================================

/// 带 ownerClientId / targetClientId / raw method / permissions 对象 → 400。
#[tokio::test]
async fn pending_response_rejects_owner_params() {
    let f = fixture_ipc(vec![codex_app_session(THREAD)]);
    for extra in [
        serde_json::json!({ "ownerClientId": "o" }),
        serde_json::json!({ "targetClientId": "t" }),
        serde_json::json!({ "rawMethod": "thread-follower-command-approval-decision" }),
        serde_json::json!({ "permissions": { "network": true } }),
    ] {
        let mut body = serde_json::json!({
            "sessionId": THREAD, "actionId": "a", "requestKey": "n:1",
            "contextToken": "t", "action": "approve",
        });
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let r = f
            .app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/codex/pending-response",
                Some(COOKIE),
                Some(body.to_string()),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "多余键必须 400：{extra}");
    }
}

/// 先建立投影（GET thread），返回 (pending requestKey, contextToken)。
async fn follow_with_pending(
    f: &Fixture,
    peer: &mut ScriptedPeer,
    requests: Vec<serde_json::Value>,
) -> (String, String) {
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    drive_ensure(peer, THREAD, requests, "idle").await;
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("follow in time")
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    let pending = &v["pending"][0];
    (
        pending["requestKey"].as_str().unwrap().to_string(),
        pending["contextToken"].as_str().unwrap().to_string(),
    )
}

/// 过期 token / 跨 thread requestKey → 409 且 IPC 写 0。
#[tokio::test]
async fn pending_response_stale_context_is_409_with_zero_writes() {
    let mut f = fixture_ipc(vec![
        codex_app_session(THREAD),
        codex_app_session("99994444-1111-4222-8333-444455556666"),
    ]);
    let mut peer = f.peer.take().unwrap();
    let (rk, _tk) = follow_with_pending(
        &f,
        &mut peer,
        vec![serde_json::json!({
            "id": 18, "method": "item/commandExecution/requestApproval",
            "params": { "command": "pnpm test", "cwd": "/w", "reason": "r" }
        })],
    )
    .await;
    assert_eq!(rk, "n:18");

    // 过期 token → 409（写 0）
    let body = serde_json::json!({
        "sessionId": THREAD, "actionId": "act-stale-1",
        "requestKey": "n:18", "contextToken": "expired-token",
        "action": "approve",
    });
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/pending-response",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 409);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["error"], "stale_context");
    assert!(!peer_has_traffic(&mut peer).await, "过期 token 零 IPC 写入");

    // 跨 thread requestKey（另一会话的 thread 无此 pending）→ 409
    let body = serde_json::json!({
        "sessionId": "99994444-1111-4222-8333-444455556666",
        "actionId": "act-stale-2",
        "requestKey": "n:18", "contextToken": "any",
        "action": "approve",
    });
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/pending-response",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 409, "跨 thread requestKey → 409");
    assert!(!peer_has_traffic(&mut peer).await, "跨 thread 零 IPC 写入");
}

/// 完整应答链：GET 投影 → approve（command 变体）→ 200 accepted 回执 + 审计。
#[tokio::test]
async fn pending_response_approve_end_to_end() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let (rk, tk) = follow_with_pending(
        &f,
        &mut peer,
        vec![serde_json::json!({
            "id": 18, "method": "item/commandExecution/requestApproval",
            "params": { "command": "pnpm test", "cwd": "/w", "reason": "r" }
        })],
    )
    .await;

    let body = serde_json::json!({
        "sessionId": THREAD, "actionId": "act-appr-1",
        "requestKey": rk, "contextToken": tk, "action": "approve",
    });
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/pending-response",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
    });
    // 命令审批帧（decision accept；整数 id 无损）
    let frame = peer.recv().await;
    assert_eq!(frame["method"], "thread-follower-command-approval-decision");
    assert_eq!(frame["params"]["decision"], "accept");
    assert_eq!(frame["params"]["requestId"], serde_json::json!(18));
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-command-approval-decision",
        "result": { "ok": true }
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("respond in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["delivery"], "accepted");
    let view = f
        .hub_store()
        .get_for_device("act-appr-1", "dev-1")
        .unwrap()
        .unwrap();
    assert_eq!(view.state, ActionState::Accepted);
    let audits = f.codex_audits();
    let row = audits
        .iter()
        .find(|a| a.action == "codex_approve")
        .expect("approve 审计行");
    assert!(row.summary.contains("act-appr-1"));
    assert!(row.summary.contains("n:18"));
    assert!(!row.summary.contains("permissions"), "审批审计不带权限对象");
}

/// 结构化回答：限额校验 + 按 qid 映射 + isSecret 答案原文不入审计。
#[tokio::test]
async fn pending_response_answer_limits_and_audit_privacy() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let (rk, tk) = follow_with_pending(
        &f,
        &mut peer,
        vec![serde_json::json!({
            "id": "s:uid-1", "method": "item/tool/requestUserInput",
            "params": { "questions": [
                { "id": "q1", "header": "选择", "question": "选一个",
                  "isOther": false, "isSecret": false,
                  "options": [ { "label": "甲", "description": "" }, { "label": "乙", "description": "" } ] },
                { "id": "q2", "header": "密钥", "question": "输入密钥",
                  "isOther": true, "isSecret": true, "options": [] }
            ]}
        })],
    )
    .await;

    // 空答案 → 400
    let body = serde_json::json!({
        "sessionId": THREAD, "actionId": "act-ans-e",
        "requestKey": rk, "contextToken": tk,
        "action": "answer", "answers": {},
    });
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/pending-response",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 400, "空答案拒绝");

    // 每题超长 → 400
    let long = "x".repeat(10_001);
    let body = serde_json::json!({
        "sessionId": THREAD, "actionId": "act-ans-l",
        "requestKey": rk, "contextToken": tk,
        "action": "answer",
        "answers": { "q1": ["甲"], "q2": [long] },
    });
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/pending-response",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 400, "单答案超 10_000 字符拒绝");

    // 合法回答（isSecret 答案走 q2 自由文本）
    let secret = "SUPER-SECRET-ANSWER-9";
    let body = serde_json::json!({
        "sessionId": THREAD, "actionId": "act-ans-1",
        "requestKey": rk, "contextToken": tk,
        "action": "answer",
        "answers": { "q1": ["甲"], "q2": [secret] },
    });
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/pending-response",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
    });
    let frame = peer.recv().await;
    assert_eq!(frame["method"], "thread-follower-submit-user-input");
    let answers = &frame["params"]["response"]["answers"];
    assert_eq!(answers["q1"]["answers"], serde_json::json!(["甲"]));
    assert_eq!(answers["q2"]["answers"], serde_json::json!([secret]));
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-submit-user-input",
        "result": { "ok": true }
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("respond in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    let view = f
        .hub_store()
        .get_for_device("act-ans-1", "dev-1")
        .unwrap()
        .unwrap();
    assert_eq!(view.state, ActionState::Accepted);
    let audits = f.codex_audits();
    let row = audits
        .iter()
        .find(|a| a.action == "codex_answer")
        .expect("answer 审计行");
    assert!(!row.summary.contains(secret), "isSecret 答案绝不入审计");
    assert!(row.summary.contains("act-ans-1"));
}

// ============================================================
// 6. 队列：认证 / 跨设备 404 / 撤回竞态 409 / cancelled 不重发
// ============================================================

#[tokio::test]
async fn queue_view_and_retract_auth_matrix() {
    let f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let app = f.app.clone();
    // queue GET 无 cookie → 403
    let r = app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/queue?session_id={THREAD}"),
            None,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    // retract 无 cookie → 403
    let r = app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/queue/retract",
            None,
            Some(serde_json::json!({ "actionId": "a" }).to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    // 本设备空队列
    let r = app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/queue?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["actions"], serde_json::json!([]));
    assert!(v["blockedReasonCode"].is_null());
}

/// 撤回：本设备 queued 可撤（cancelled 视图 + 墓碑）；他人 404；已派发 409；
/// cancelled actionId 同载荷重放发送 → 明确不重发回执。
#[tokio::test]
async fn queue_retract_ownership_race_and_tombstone() {
    let f = fixture_ipc(vec![codex_app_session(THREAD)]);
    seed_device(&f.conn, "dev-2", "iPad");
    let store = f.hub_store();
    let ctx = ActionContext {
        action_id: "act-rt-1".into(),
        device_id: "dev-1".into(),
        device_name: "iPhone".into(),
        kind: ActionKind::Send,
        key: f.thread_key(),
    };
    let real_hash = payload_hash(
        ActionKind::Send,
        &f.thread_key(),
        None,
        None,
        &ActionPayload::Text("排队中".into()),
    );
    store
        .register(
            &ctx,
            ActionKind::Send,
            None,
            None,
            &real_hash,
            Some("排队中"),
        )
        .unwrap();

    // 他人设备 → 404
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/queue/retract",
            Some("mam_device=dev-2"),
            Some(serde_json::json!({ "actionId": "act-rt-1" }).to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 404, "他人动作 404");

    // 本设备撤回 → 200 cancelled
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/queue/retract",
            Some(COOKIE),
            Some(serde_json::json!({ "actionId": "act-rt-1" }).to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["state"], "cancelled");

    // 再撤 → 409（墓碑已终态）
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/queue/retract",
            Some(COOKIE),
            Some(serde_json::json!({ "actionId": "act-rt-1" }).to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 409, "已终态撤回 409");

    // cancelled actionId 同载荷重放发送 → 200 明确不重发回执（not_sent action_cancelled）
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(send_body("排队中", Some("act-rt-1"), false)),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["status"], "desktop_ipc");
    assert_eq!(v["receipt"]["delivery"], "not_sent");
    assert_eq!(v["receipt"]["reasonCode"], "action_cancelled");

    // 撤回竞态 409：queued 动作先 reserve（并发派发赢者）→ retract 409
    let ctx2 = ActionContext {
        action_id: "act-rt-2".into(),
        device_id: "dev-1".into(),
        device_name: "iPhone".into(),
        kind: ActionKind::Send,
        key: f.thread_key(),
    };
    store
        .register(&ctx2, ActionKind::Send, None, None, "h", Some("竞态"))
        .unwrap();
    assert!(store.reserve("act-rt-2", ControlRoute::DesktopIpc).unwrap());
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/queue/retract",
            Some(COOKIE),
            Some(serde_json::json!({ "actionId": "act-rt-2" }).to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 409, "已派发不可撤");
}

// ============================================================
// 人工解除「无出口保守屏障」（2026-10-19 复审 Important）：POST /codex/queue/force-continue
// ============================================================

/// 造「unknown + 保守屏障」账本行（复审列出的四条成因收敛后的统一终态）：
/// register → reserve → finish_with_turn_barrier（UnknownDelivery，缺 turnId）。
fn seed_unknown_barrier(store: &ActionStore, action_id: &str) {
    let ctx = ActionContext {
        action_id: action_id.to_string(),
        device_id: "dev-1".into(),
        device_name: "iPhone".into(),
        kind: ActionKind::Send,
        key: ThreadKey {
            host_id: "local".into(),
            thread_id: THREAD.into(),
        },
    };
    store
        .register(&ctx, ActionKind::Send, None, None, "h", Some("未知投递"))
        .unwrap();
    assert!(store.reserve(action_id, ControlRoute::DesktopIpc).unwrap());
    store
        .finish_with_turn_barrier(
            &ActionReceipt {
                action_id: action_id.to_string(),
                delivery: Delivery::UnknownDelivery,
                route: ControlRoute::DesktopIpc,
                turn_id: None,
                envelope_id: None,
                reason_code: Some("rpc_timeout".into()),
            },
            None,
        )
        .unwrap();
}

async fn force_continue(
    f: &Fixture,
    cookie: Option<&str>,
    action_id: &str,
) -> axum::http::Response<axum::body::Body> {
    f.app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/queue/force-continue",
            cookie,
            Some(serde_json::json!({ "actionId": action_id }).to_string()),
        ))
        .await
        .unwrap()
}

async fn queue_view_json(f: &Fixture) -> serde_json::Value {
    let r = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/queue?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    serde_json::from_str(&body_string(r).await).unwrap()
}

/// 成功路径：清屏障 + 队列视图解除（blockedReasonCode/barrierActionIds 双清）+
/// 审计在册；解除断言不带存在性预言机。
#[tokio::test]
async fn force_continue_releases_unknown_barrier_and_updates_queue_view() {
    let f = fixture_ipc(vec![codex_app_session(THREAD)]);
    seed_unknown_barrier(&f.hub_store(), "act-unk-1");

    // 解除前：队列视图报 native_delivery_unknown + 屏障动作清单
    let v = queue_view_json(&f).await;
    assert_eq!(v["blockedReasonCode"], "native_delivery_unknown");
    assert_eq!(v["barrierActionIds"], serde_json::json!(["act-unk-1"]));
    assert!(
        f.hub_store().has_compat_fence(&f.thread_key()).unwrap(),
        "屏障在册"
    );

    // 解除：200 + 视图原状态（unknown）返回
    let r = force_continue(&f, Some(COOKIE), "act-unk-1").await;
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["actionId"], "act-unk-1");
    assert_eq!(v["state"], "unknown", "状态不被改写（只清屏障）");

    // 持久屏障清 + 队列视图解除（三态派生不再报阻塞）
    assert!(
        !f.hub_store().has_compat_fence(&f.thread_key()).unwrap(),
        "屏障已清"
    );
    let v = queue_view_json(&f).await;
    assert!(
        v["blockedReasonCode"].is_null(),
        "解除后队列视图不再报阻塞：{v}"
    );
    assert_eq!(v["barrierActionIds"], serde_json::json!([]));

    // 审计在册：恰好一条 force_continue（谁/哪个 action/何时）
    let conn = f.conn.lock().unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT device_id, ts, summary, result FROM write_audit \
             WHERE action = 'force_continue'",
        )
        .unwrap();
    let rows: Vec<(String, i64, String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(rows.len(), 1, "解除写恰好一条审计");
    assert_eq!(rows[0].0, "dev-1", "谁");
    assert!(rows[0].1 > 0, "何时（ts 在册）");
    assert!(
        rows[0].2.contains("act-unk-1"),
        "哪个 action：{}",
        rows[0].2
    );
    assert_eq!(rows[0].3, "barrier_force_released", "结果词");
}

/// 鉴权与隔离矩阵：非属主设备 404；不存在 404；queued 态 409 queued_use_retract
/// （指引走既有撤回）；reserved+A 路无屏障 409 no_barrier；reserved+compat 兜底在飞
///（fence 未武装瞬态窗口）与 reserved+在飞 fence 均 409 barrier_in_flight（终审
/// Minor：有自动落账路径不得谎报 no_barrier）；缺 actionId 400。
#[tokio::test]
async fn force_continue_ownership_and_state_machine_matrix() {
    let f = fixture_ipc(vec![codex_app_session(THREAD)]);
    seed_device(&f.conn, "dev-2", "iPad");
    let store = f.hub_store();
    seed_unknown_barrier(&store, "act-own-1");

    // 非属主设备 → 404（不给存在性预言机）
    let r = force_continue(&f, Some("mam_device=dev-2"), "act-own-1").await;
    assert_eq!(r.status(), 404, "非属主 404");
    assert!(
        f.hub_store().has_compat_fence(&f.thread_key()).unwrap(),
        "非属主请求不触碰屏障"
    );

    // 不存在 → 404
    let r = force_continue(&f, Some(COOKIE), "no-such-action").await;
    assert_eq!(r.status(), 404, "不存在 404");

    // queued 态 → 409 + queued_use_retract（指引走既有撤回）
    let ctx = ActionContext {
        action_id: "act-q-1".into(),
        device_id: "dev-1".into(),
        device_name: "iPhone".into(),
        kind: ActionKind::Send,
        key: f.thread_key(),
    };
    store
        .register(&ctx, ActionKind::Send, None, None, "h", Some("排队中"))
        .unwrap();
    let r = force_continue(&f, Some(COOKIE), "act-q-1").await;
    assert_eq!(r.status(), 409, "queued 走撤回不走解除");
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["error"], "queued_use_retract");
    assert_eq!(
        f.hub_store()
            .get_for_device("act-q-1", "dev-1")
            .unwrap()
            .unwrap()
            .state,
        ActionState::Queued,
        "queued 行不被触碰（仍可撤）"
    );

    // reserved + A 路无屏障 → 409 no_barrier（A 路在飞无自动兜底交接路径语义的
    // 屏障可解性口径不变：真正无屏障可解才报 no_barrier）
    assert!(store.reserve("act-q-1", ControlRoute::DesktopIpc).unwrap());
    let r = force_continue(&f, Some(COOKIE), "act-q-1").await;
    assert_eq!(r.status(), 409);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["error"], "no_barrier");

    // reserved + compat 兜底回合在飞（handoff 已落、fence 未武装的瞬态窗口）→
    // 409 barrier_in_flight（终审 Minor：该行即将走自动落账，谎报 no_barrier 会
    // 让前端误判「无需解除」、排障者误判状态）
    let ctx_fb = ActionContext {
        action_id: "act-fb-1".into(),
        device_id: "dev-1".into(),
        device_name: "iPhone".into(),
        kind: ActionKind::Send,
        key: f.thread_key(),
    };
    store
        .register(&ctx_fb, ActionKind::Send, None, None, "h", Some("排队中"))
        .unwrap();
    assert!(store
        .reserve("act-fb-1", ControlRoute::HeadlessCompat)
        .unwrap());
    let r = force_continue(&f, Some(COOKIE), "act-fb-1").await;
    assert_eq!(r.status(), 409, "兜底在飞瞬态窗口");
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["error"], "barrier_in_flight");

    // reserved + 在飞 fence（屏障在册但不在解除范围）→ 409 barrier_in_flight
    // （复审 Minor：屏障明明在册，回 no_barrier 会误导排障；reason 说明真实状态）
    store.set_compat_fence("act-q-1", None, None).unwrap();
    let r = force_continue(&f, Some(COOKIE), "act-q-1").await;
    assert_eq!(r.status(), 409);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["error"], "barrier_in_flight");
    assert!(
        v["reason"].as_str().is_some_and(|s| !s.is_empty()),
        "barrier_in_flight 携带可读 reason"
    );

    // 缺 actionId → 400
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/queue/force-continue",
            Some(COOKIE),
            Some(serde_json::json!({}).to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 400);

    // 屏障行仍完好（上述 4xx 全部零写入）
    assert!(
        f.hub_store().has_compat_fence(&f.thread_key()).unwrap(),
        "4xx 不触碰屏障"
    );
}

/// 幂等：重复调用返回当前状态（200），审计不重复；解除后其他路径不回置屏障
/// （启动恢复 / 超龄清扫 / bind_turn 补绑的守卫面）。
#[tokio::test]
async fn force_continue_idempotent_and_monotonic() {
    let f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let store = f.hub_store();
    seed_unknown_barrier(&store, "act-idem-1");

    let r = force_continue(&f, Some(COOKIE), "act-idem-1").await;
    assert_eq!(r.status(), 200, "首次解除成功");

    // 重复调用：幂等 200（当前状态），审计仍是一条
    let r = force_continue(&f, Some(COOKIE), "act-idem-1").await;
    assert_eq!(r.status(), 200, "重复调用幂等返回当前状态");
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["state"], "unknown");

    // 解除后其他路径不回置屏障：
    // ① 启动恢复（recover_reserved 只触 reserved）与超龄清扫不复活
    store.recover_reserved().unwrap();
    store.mark_stale_reserved_unknown(0);
    // ② bind_turn 补绑（set_compat_fence 写点）带单调性守卫
    store
        .set_compat_fence("act-idem-1", Some("m"), Some("t"))
        .unwrap();
    // ③ 原子派发屏障写点同样被守卫
    store
        .set_compat_dispatch_barrier(
            "act-idem-1",
            Some("m"),
            Some("t"),
            Some("/p"),
            Some(1),
            Some("w"),
        )
        .unwrap();
    assert!(
        !store.has_compat_fence(&f.thread_key()).unwrap(),
        "解除后任何路径不回置屏障（单调性）"
    );

    // 审计仍恰好一条（幂等调用零新增）
    let conn = f.conn.lock().unwrap();
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM write_audit WHERE action = 'force_continue'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1, "绝不重复写审计");
}

/// accepted+turnId 绑定屏障（native_turn_pending，有自动路径）同样允许显式覆盖：
/// 队列视图报 native_turn_pending 时，人工解除也有效（用户核对后的越权出口统一面）。
#[tokio::test]
async fn force_continue_also_releases_accepted_turn_binding_barrier() {
    let f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let store = f.hub_store();
    let ctx = ActionContext {
        action_id: "act-acc-1".into(),
        device_id: "dev-1".into(),
        device_name: "iPhone".into(),
        kind: ActionKind::Send,
        key: f.thread_key(),
    };
    store
        .register(&ctx, ActionKind::Send, None, None, "h", Some("已接受"))
        .unwrap();
    assert!(store
        .reserve("act-acc-1", ControlRoute::DesktopIpc)
        .unwrap());
    store
        .finish_with_turn_barrier(
            &ActionReceipt {
                action_id: "act-acc-1".into(),
                delivery: Delivery::Accepted,
                route: ControlRoute::DesktopIpc,
                turn_id: Some("turn-stuck".into()),
                envelope_id: None,
                reason_code: None,
            },
            Some("turn-stuck"),
        )
        .unwrap();

    let v = queue_view_json(&f).await;
    assert_eq!(v["blockedReasonCode"], "native_turn_pending");

    let r = force_continue(&f, Some(COOKIE), "act-acc-1").await;
    assert_eq!(r.status(), 200, "绑定屏障允许桌面端核对后的显式覆盖");
    assert!(
        !store.has_compat_fence(&f.thread_key()).unwrap(),
        "绑定屏障已清"
    );
    let v = queue_view_json(&f).await;
    assert!(v["blockedReasonCode"].is_null());
}

/// queueOnly 发送 → codex_queued（只排队不派发）；hub.stop() 后未派发项仍 queued。
#[tokio::test]
async fn lifecycle_queue_only_survives_hub_stop() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let body = send_body("只排队", Some("act-qq-1"), true);
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    // **queueOnly 不派发**（零 IPC——不 drive_ensure：peer 永远收不到帧才是正确断言）
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("in time")
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["status"], "codex_queued");
    // hub stop（remote 关闭语义）：未派发项仍 queued，不派发
    f.state.codex_control.stop();
    let view = f
        .hub_store()
        .get_for_device("act-qq-1", "dev-1")
        .unwrap()
        .unwrap();
    assert_eq!(view.state, ActionState::Queued);
    assert!(!peer_has_traffic(&mut peer).await, "stop 后零派发");
}

// ============================================================
// 7. SSE：首帧 snapshot + 吊销即时关闭
// ============================================================

#[tokio::test]
async fn sse_stream_snapshots_and_revocation_closes() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread/events?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("sse in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    let mut body = r.into_body();
    use http_body_util::BodyExt;
    // 首帧 snapshot（ThreadProjection）
    let first = tokio::time::timeout(Duration::from_secs(10), body.frame())
        .await
        .expect("first frame")
        .unwrap()
        .unwrap();
    let text = String::from_utf8(first.data_ref().unwrap().to_vec()).unwrap();
    assert!(
        text.contains("\"threadId\""),
        "首帧为 ThreadProjection 快照：{text}"
    );
    // 吊销 → 即时断流（EOF）
    f.state.sse_registry.disconnect_device("dev-1");
    loop {
        match tokio::time::timeout(Duration::from_secs(5), body.frame()).await {
            Ok(Some(_)) => continue, // 缓冲帧
            Ok(None) => break,       // EOF：流已关
            Err(_) => panic!("吊销后流未在超时内关闭"),
        }
    }
}

// ============================================================
// 8. 旧 APP 补投入口阻断（有 A 可写投影时）
// ============================================================

#[tokio::test]
async fn app_repost_blocked_when_native_live() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    // 建立 A 投影（带一条 command pending——投影非空即视为可写活投影）
    let (_rk, _tk) = follow_with_pending(
        &f,
        &mut peer,
        vec![serde_json::json!({
            "id": 18, "method": "item/commandExecution/requestApproval",
            "params": { "command": "pnpm test", "cwd": "/w", "reason": "r" }
        })],
    )
    .await;

    // POST session-question/answer（补投）→ 503 capability_unavailable + 引导
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/session-question/answer",
            Some(COOKIE),
            Some(
                serde_json::json!({
                    "sessionId": THREAD, "action": "freeText", "text": "用文字补答",
                })
                .to_string(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 503, "有 A 投影时补投必须阻断");
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["error"], "capability_unavailable");
    assert_eq!(v["reasonCode"], "native_only");

    // GET session-question → 不把陈旧 rollout 问题广告为可处理原生卡
    let r = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/m/api/v1/session-question?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["answerable"], serde_json::json!(false));
    assert_eq!(v["questions"], serde_json::json!([]));
    assert_eq!(v["reasonCode"], "native_only");
}

/// **R8（2026-10-17 审计，spec F7/S5）：Codex APP 旧问答补投无条件阻断**——
/// stale/无 owner/未知 build（disabled hub）三态一律 503 capability_unavailable，
/// **零 IPC 写、零 queue/exec 投递**（不再退回 AppRepost 通道）；GET 广告同口径
/// 只报 native_only。fresh 投影态由 `app_repost_blocked_when_native_live` 覆盖。
#[tokio::test]
async fn app_repost_always_blocked_for_codex_app() {
    let body = serde_json::json!({
        "sessionId": THREAD, "action": "freeText", "text": "用文字补答",
    })
    .to_string();

    // 态一：IPC 在场但**未跟随**（stale/无 owner 投影）——阻断且零 IPC 写
    {
        let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
        let mut peer = f.peer.take().unwrap();
        let r = f
            .app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some(COOKIE),
                Some(body.clone()),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 503, "无 owner 投影也必须阻断");
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["error"], "capability_unavailable");
        assert_eq!(v["reasonCode"], "native_only");
        assert!(!peer_has_traffic(&mut peer).await, "无 owner 态零 IPC 写入");
        // 零 queue/exec 投递：inject_queue 无行、无 headless 审计投递
        let queue_rows: i64 = {
            let conn = f.conn.lock().unwrap();
            conn.query_row("SELECT COUNT(*) FROM inject_queue", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(queue_rows, 0, "零 queue 投递");
        assert!(
            !f.codex_audits()
                .iter()
                .any(|a| a.result.starts_with("ok") || a.result.starts_with("queued")),
            "零 exec/queue 投递审计"
        );
    }

    // 态二：未知 build（disabled hub，H3 开也不放行）——阻断 + GET 广告 native_only
    {
        let _path = IsolatedPath::lock_empty();
        let f = fixture_disabled(vec![codex_app_session(THREAD)]);
        f.state.store.with(|c| {
            c.execute(
                "INSERT OR REPLACE INTO settings (key, value) VALUES ('remote.headless_enabled', 'true')",
                [],
            )
            .unwrap();
        });
        let r = f
            .app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-question/answer",
                Some(COOKIE),
                Some(body.clone()),
            ))
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            503,
            "未知 build（disabled hub）同样阻断，H3 开不例外"
        );
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["error"], "capability_unavailable");

        // GET 广告同口径：不把陈旧 rollout 问题广告为可处理
        let r = f
            .app
            .clone()
            .oneshot(req(
                "GET",
                &format!("/m/api/v1/session-question?session_id={THREAD}"),
                Some(COOKIE),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["answerable"], serde_json::json!(false));
        assert_eq!(v["questions"], serde_json::json!([]));
        assert_eq!(v["reasonCode"], "native_only");

        let queue_rows: i64 = {
            let conn = f.conn.lock().unwrap();
            conn.query_row("SELECT COUNT(*) FROM inject_queue", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(queue_rows, 0, "未知 build 零 queue 投递");
    }
}

// ============================================================
// 9. CLI 工具零漂移 + 审计哨兵
// ============================================================

/// CLI 会话（非 codex APP）携带多余 actionId 键 → 旧合同零漂移（键被忽略，走老路）。
#[tokio::test]
async fn claude_session_send_ignores_action_id_key() {
    let f = fixture_disabled(vec![claude_cli_session(
        "claude-s-1",
        SessionStatus::Processing,
    )]);
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(
                serde_json::json!({
                    "sessionId": "claude-s-1", "text": "普通消息", "actionId": "not-for-cli",
                })
                .to_string(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["status"], "queued", "CLI 黄态照旧入 inject_queue");
    assert!(v.get("actionId").is_none(), "CLI 回执无 actionId 语义");
    assert!(f.codex_audits().is_empty(), "CLI 路径不写 codex_* 审计");
}

/// 审计哨兵：正文哨兵 / cookie 不出现在任何审计行（答案原文同规，见回答用例）。
#[tokio::test]
async fn audit_never_leaks_secrets() {
    let secret = "SK-SENTINEL-abc123";
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    // 发送一条正文含哨兵的消息（走完整 IPC 接受路径）
    let body = serde_json::json!({
        "sessionId": THREAD, "text": format!("正文带 {secret}"),
        "actionId": "act-secret-1",
    });
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let frame = peer.recv().await;
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "turn": { "id": "t", "status": "inProgress" } }
    }))
    .await;
    tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("in time")
        .unwrap();
    for summary in f.all_audit_summaries() {
        assert!(
            !summary.contains(secret),
            "审计 summary 泄漏正文哨兵：{summary}"
        );
        assert!(!summary.contains(COOKIE), "审计泄漏 cookie：{summary}");
    }
}

// ============================================================
// 10. R6.2：忙态首 snapshot——首次直接 POST 且假 Desktop 直接给 active → 只入队
// 零 start-turn 帧（不再「无镜像恒 false → 直接 start-turn」）。
// ============================================================

#[tokio::test]
async fn first_post_with_active_snapshot_queues_without_start_turn() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let body = send_body("首条即忙", Some("act-busy-first"), false);
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    // 无任何先前 GET：busy_fresh 先 ensure（本驱动直接给 active snapshot）
    drive_ensure(&mut peer, THREAD, vec![], "active").await;
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("send in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["status"], "codex_queued", "首 snapshot active → 只入队");
    assert_eq!(v["actionId"], "act-busy-first");
    assert_eq!(v["position"], serde_json::json!(1), "首个待发位置为 1");
    let view = f
        .hub_store()
        .get_for_device("act-busy-first", "dev-1")
        .unwrap()
        .unwrap();
    assert_eq!(view.state, ActionState::Queued);
    assert!(
        !peer_has_traffic(&mut peer).await,
        "忙态首 POST 零 start-turn 帧（R6.2）"
    );
}

// ============================================================
// 11. R6.3a：start-turn 回执按真实封套解包 turnId（Mac Probe 记录嵌套两层
// result.result.turn）；缺 turnId 如实回 None。
// ============================================================

#[tokio::test]
async fn nested_turn_receipt_yields_turn_id() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let body = send_body("嵌套回执", Some("act-nested-1"), false);
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let frame = peer.recv().await;
    assert_eq!(frame["method"], "thread-follower-start-turn");
    // Mac Probe 实录形态：turn 嵌套两层
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "result": { "turn": { "id": "turn-nested", "status": "inProgress" } } }
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(20), task)
        .await
        .expect("in time")
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["status"], "desktop_ipc");
    assert_eq!(v["receipt"]["delivery"], "accepted");
    assert_eq!(
        v["receipt"]["turnId"], "turn-nested",
        "嵌套两层封套的 turnId 必须解出（R6.3a）"
    );
    let view = f
        .hub_store()
        .get_for_device("act-nested-1", "dev-1")
        .unwrap()
        .unwrap();
    assert_eq!(
        view.receipt.unwrap().turn_id.as_deref(),
        Some("turn-nested")
    );
}

// ============================================================
// 12. R8.1/R8.3：未知 build（disabled hub）+ H3 开 → 普通消息也先经统一 router
//（登记→reserve→NotSent→compat），同 action 重放 / 重建 hub 重放均只投一次。
// ============================================================

#[tokio::test]
async fn unknown_build_send_routes_through_ledger_dedup() {
    let _path = IsolatedPath::lock_empty();
    let f = fixture_disabled(vec![codex_app_session(THREAD)]);
    f.state.store.with(|c| {
        c.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('remote.headless_enabled', 'true')",
            [],
        )
        .unwrap();
    });
    // 首投：经 router 登记 → 兜底（空 PATH CLI 缺失 → 投递前明确拒）
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(send_body("未知 build 消息", Some("act-ub-1"), false)),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["status"], "headless", "未知 build 走兜底封套");
    assert!(
        f.codex_audits()
            .iter()
            .any(|a| a.action == "codex_fallback" && a.summary.contains("act-ub-1")),
        "经 router 有可追踪兜底审计"
    );
    let fallbacks_after_first = f
        .codex_audits()
        .iter()
        .filter(|a| a.action == "codex_fallback")
        .count();

    // 同 action 重放：原回执、零新增投递（持久去重不经 helper）
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(send_body("未知 build 消息", Some("act-ub-1"), false)),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["status"], "desktop_ipc", "重放返回账本既有回执");
    assert_eq!(v["receipt"]["delivery"], "not_sent");

    // 「进程重启」：丢弃全部内存，同库重建 hub 再重放 → 仍零新增投递
    let hub2 = CodexControlHub::disabled_with_store(DeviceStore::Owned(f.conn.clone()));
    let ctx = ActionContext {
        action_id: "act-ub-1".into(),
        device_id: "dev-1".into(),
        device_name: "iPhone".into(),
        kind: ActionKind::Send,
        key: ThreadKey {
            host_id: "local".into(),
            thread_id: THREAD.into(),
        },
    };
    let outcome = hub2.router.send(ctx, "未知 build 消息".into(), false).await;
    match outcome {
        multi_agents_manager_lib::services::codex_control::RouterOutcome::Completed(receipt) => {
            assert_eq!(receipt.delivery, Delivery::NotSent);
        }
        other => panic!("重放应返回 Completed，got {other:?}"),
    }
    let fallbacks_final = f
        .codex_audits()
        .iter()
        .filter(|a| a.action == "codex_fallback")
        .count();
    assert_eq!(
        fallbacks_final, fallbacks_after_first,
        "重放/重启均不得新增投递（R8.3）"
    );
    // queue/exec 通道零投递
    let queue_rows: i64 = {
        let conn = f.conn.lock().unwrap();
        conn.query_row("SELECT COUNT(*) FROM inject_queue", [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(queue_rows, 0);
}

// ============================================================
// 13. R9.1：fence/证据原子持久化失败（SQLite trigger 拒绝屏障 UPDATE）→ 零
// helper 调用、动作如实落账 NotSent。
// ============================================================

#[tokio::test]
async fn compat_barrier_persist_failure_skips_helper() {
    let _path = IsolatedPath::lock_empty();
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    // 共享库上装 trigger：拒绝「屏障置位」UPDATE（fence+证据原子写），放行其余
    f.conn
        .lock()
        .unwrap()
        .execute(
            "CREATE TRIGGER barrier_reject BEFORE UPDATE ON codex_actions \
         WHEN NEW.compat_outstanding = 1 AND OLD.compat_outstanding = 0 \
         BEGIN SELECT RAISE(ABORT, 'barrier rejected by test trigger'); END",
            [],
        )
        .unwrap();
    f.state.store.with(|c| {
        c.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('remote.headless_enabled', 'true')",
            [],
        )
        .unwrap();
    });
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let body = send_body("屏障失败消息", Some("act-barrier-1"), false);
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    // A 路走到 following 后断连 → writer NotSent → handoff → 兜底屏障写失败
    handshake(&mut peer, "owner-1").await;
    let disc = peer.recv().await;
    peer.send(&discovery_response(
        disc["requestId"].as_str().unwrap(),
        "owner-1",
    ))
    .await;
    let _follow = peer.recv().await;
    peer.close();
    let r = tokio::time::timeout(Duration::from_secs(30), task)
        .await
        .expect("in time")
        .unwrap();
    assert_eq!(r.status(), 200, "兜底封套如实回执");
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["receipt"]["status"], "failed");
    assert_eq!(v["receipt"]["reason"], "compat_barrier_persist_failed");
    // 动作如实落账：NotSent（零投递），屏障无残留
    let view = f
        .hub_store()
        .get_for_device("act-barrier-1", "dev-1")
        .unwrap()
        .unwrap();
    assert_eq!(view.state, ActionState::Rejected);
    let receipt = view.receipt.unwrap();
    assert_eq!(receipt.delivery, Delivery::NotSent);
    assert_eq!(
        receipt.reason_code.as_deref(),
        Some("compat_barrier_persist_failed")
    );
    assert!(
        !f.hub_store().has_compat_fence(&f.thread_key()).unwrap(),
        "屏障持久化失败无残留"
    );
    // queue/exec 通道零投递（helper 未被调用）
    let queue_rows: i64 = {
        let conn = f.conn.lock().unwrap();
        conn.query_row("SELECT COUNT(*) FROM inject_queue", [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(queue_rows, 0, "两通道写入均 0（R9.1）");
    let audits = f.codex_audits();
    assert!(
        audits
            .iter()
            .any(|a| a.action == "codex_fallback"
                && a.result == "refused:compat_barrier_persist_failed"),
        "如实落账审计：{:?}",
        audits
    );
}

// ============================================================
// 14. R7.1：hub 启动恢复——丢弃全部内存重建 hub 后，旧 queued 行不打开详情、
// 不手工 flush 即按序发出（首个对账 tick 即启动恢复）。
// ============================================================

fn make_adapter_peer() -> (Arc<DesktopIpcAdapter>, ScriptedPeer) {
    let (io, peer) = ScriptedPeer::new();
    (
        Arc::new(DesktopIpcAdapter::new(
            Arc::new(StaticConnector::new(io)),
            caps_open_profile(),
        )),
        peer,
    )
}

#[tokio::test]
async fn hub_restart_recovery_replays_queued_without_details() {
    let conn = shared_conn();
    seed_device(&conn, "dev-1", "iPhone");
    // —— 阶段一：旧 hub 上 queueOnly 排队（零 IPC），随后丢弃全部内存 ——
    {
        let (adapter, _peer1) = make_adapter_peer();
        let f1 = fixture_on(
            conn.clone(),
            vec![codex_app_session(THREAD)],
            move |store| Arc::new(CodexControlHub::with_adapter(store, adapter, true)),
            None,
        );
        let r = f1
            .app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some(COOKIE),
                Some(send_body("重启前排队", Some("act-rec-1"), true)),
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["status"], "codex_queued");
        assert_eq!(
            f1.hub_store()
                .get_for_device("act-rec-1", "dev-1")
                .unwrap()
                .unwrap()
                .state,
            ActionState::Queued
        );
        // f1 在此 drop：hub/adapter/调度器全部丢弃，仅持久账本（共享 conn）留存
    }
    // —— 阶段二：重建 hub（全新 adapter/peer），start 后不打开详情、不手工 flush ——
    let (adapter2, peer2) = make_adapter_peer();
    let mut f2 = fixture_on(
        conn.clone(),
        vec![codex_app_session(THREAD)],
        move |store| Arc::new(CodexControlHub::with_adapter(store, adapter2, true)),
        Some(peer2),
    );
    f2.state
        .codex_control
        .set_reconcile_period_ms(std::time::Duration::from_millis(200).as_millis() as u64);
    f2.state.codex_control.start(f2.state.clone());
    {
        let mut peer = f2.peer.take().unwrap();
        // 启动恢复经 ensure 重新 following（事件驱动调度），队首按序发出
        drive_ensure(&mut peer, THREAD, vec![], "idle").await;
        let frame = peer.recv().await;
        assert_eq!(
            frame["method"], "thread-follower-start-turn",
            "恢复后旧 queued 行自动发出"
        );
        assert_eq!(
            frame["params"]["turnStart"]["request"]["input"][0]["text"],
            "重启前排队 [mobile iPhone]"
        );
        peer.send(&serde_json::json!({
            "type": "response", "requestId": frame["requestId"],
            "resultType": "success", "method": "thread-follower-start-turn",
            "handledByClientId": "owner-1",
            "result": { "turn": { "id": "turn-rec", "status": "inProgress" } }
        }))
        .await;
    }
    let view = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(Some(v)) = f2.hub_store().get_for_device("act-rec-1", "dev-1") {
                if v.state == ActionState::Accepted {
                    return v;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("recovery dispatch settles");
    assert_eq!(view.receipt.unwrap().turn_id.as_deref(), Some("turn-rec"));
    f2.state.codex_control.stop();
}

// ============================================================
// 15. R7.2：周期对账——hub 运行中新入队的行，未经任何事件/手工触发，下一对账
// 周期即被接力发出。
// ============================================================

#[tokio::test]
async fn periodic_reconcile_dispatches_newly_queued() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let peer = f.peer.take().unwrap();
    f.state
        .codex_control
        .set_reconcile_period_ms(std::time::Duration::from_millis(200).as_millis() as u64);
    f.state.codex_control.start(f.state.clone());
    // hub 运行后（首个恢复 tick 已过、集合为空）再入队——只能靠周期对账接力
    let ctx = ActionContext {
        action_id: "act-tick-1".into(),
        device_id: "dev-1".into(),
        device_name: "iPhone".into(),
        kind: ActionKind::Send,
        key: ThreadKey {
            host_id: "local".into(),
            thread_id: THREAD.into(),
        },
    };
    let hash = payload_hash(
        ActionKind::Send,
        &ctx.key,
        None,
        None,
        &ActionPayload::Text("对账接力".into()),
    );
    f.hub_store()
        .register(&ctx, ActionKind::Send, None, None, &hash, Some("对账接力"))
        .unwrap();

    // 对端服务全程协议（ensure → start-turn → 成功回执）
    let driver = tokio::spawn(async move {
        let mut peer = peer;
        drive_ensure(&mut peer, THREAD, vec![], "idle").await;
        let frame = peer.recv().await;
        assert_eq!(frame["method"], "thread-follower-start-turn");
        peer.send(&serde_json::json!({
            "type": "response", "requestId": frame["requestId"],
            "resultType": "success", "method": "thread-follower-start-turn",
            "handledByClientId": "owner-1",
            "result": { "turn": { "id": "turn-tick", "status": "inProgress" } }
        }))
        .await;
        peer
    });
    let view = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(Some(v)) = f.hub_store().get_for_device("act-tick-1", "dev-1") {
                if v.state == ActionState::Accepted {
                    return v;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("periodic reconcile dispatches");
    assert_eq!(view.receipt.unwrap().turn_id.as_deref(), Some("turn-tick"));
    f.state.codex_control.stop();
    let _ = driver.await;
}

// ============================================================
// 16. R7.3：关注生命周期——连续 65 次挂载/卸载（SSE 订阅/关闭）名额不耗尽；
// 64 次失败 discovery 不耗尽（失败回滚）。
// ============================================================

/// 重复 follow 的协议驱动（**不含握手**——连接与 initialize 一次握手后复用，
/// 每轮只有 discovery → following → snapshot）。snapshot revision 可变（镜像复活用）。
async fn drive_ensure_rev(
    peer: &mut ScriptedPeer,
    thread: &str,
    requests: Vec<serde_json::Value>,
    runtime: &str,
    revision: u64,
) {
    let disc = peer.recv().await;
    assert_eq!(disc["method"], "thread-owner-discovery");
    peer.send(&discovery_response(
        disc["requestId"].as_str().unwrap(),
        "owner-1",
    ))
    .await;
    let follow = peer.recv().await;
    assert_eq!(follow["method"], "thread-stream-following-changed");
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": thread, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": revision,
                "conversationState": {
                    "turns": [], "requests": requests,
                    "threadRuntimeStatus": { "type": runtime },
                    "turnHistory": codex_fixture::canonical_turn_history(serde_json::json!({
                            "t1": { "id": "t1", "type": "turn", "status": "completed", "items": ["u1"] },
                            "u1": { "id": "u1", "type": "user_message", "text": "seed", "ts": 1 }
                        }), serde_json::json!([{ "entries": ["t1"] }]))
                }
            }
        }
    }))
    .await;
}

#[tokio::test]
async fn sse_mount_unmount_cycles_do_not_exhaust_following() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    for cycle in 0..65u32 {
        let app = f.app.clone();
        let task = tokio::spawn(async move {
            app.oneshot(req(
                "GET",
                &format!("/m/api/v1/codex/thread/events?session_id={THREAD}"),
                Some(COOKIE),
                None,
            ))
            .await
            .unwrap()
        });
        // 连接只握手一次（initialize 单次；后续挂载复用同一连接）
        if cycle == 0 {
            handshake(&mut peer, "owner-1").await;
        }
        drive_ensure_rev(&mut peer, THREAD, vec![], "idle", 7 + cycle as u64).await;
        let r = tokio::time::timeout(Duration::from_secs(15), task)
            .await
            .expect("sse in time")
            .unwrap();
        assert_eq!(r.status(), 200);
        let mut body = r.into_body();
        use http_body_util::BodyExt;
        let first = tokio::time::timeout(Duration::from_secs(10), body.frame())
            .await
            .expect("first frame")
            .unwrap()
            .unwrap();
        let text = String::from_utf8(first.data_ref().unwrap().to_vec()).unwrap();
        assert!(
            text.contains("\"stale\":false"),
            "第 {cycle} 次挂载必须拿到新鲜投影（名额未耗尽）：{text}"
        );
        drop(body);
        // 引用归零 → unfollow 释放名额（Drop 内 spawn）：C4 起 unfollow 会真正发
        // following:false——消费它再进入下一轮（下一轮 discovery 之前必达）
        tokio::time::sleep(Duration::from_millis(150)).await;
        let off = peer.recv().await;
        assert_eq!(
            off["method"], "thread-stream-following-changed",
            "第 {cycle} 次卸载必须声明 following:false"
        );
        assert_eq!(off["params"]["following"], serde_json::json!(false));
    }
}

#[tokio::test]
async fn failed_discovery_rolls_back_following_slot() {
    // 64 个不同 thread 的 GET 全部 discovery 失败 → 名额回滚；第 65 个完整 follow 仍可用
    let mut sessions = Vec::new();
    for i in 0..65u32 {
        let id = format!("aaaaaaaa-bbbb-4ccc-8ddd-{i:012}");
        sessions.push(codex_app_session(&id));
    }
    let mut f = fixture_ipc(sessions);
    let mut peer = f.peer.take().unwrap();
    // 首次握手在首个 GET 建连后（连接复用；后续 GET 只有 discovery 往返）
    for i in 0..64u32 {
        let id = format!("aaaaaaaa-bbbb-4ccc-8ddd-{i:012}");
        let app = f.app.clone();
        let task = tokio::spawn(async move {
            app.oneshot(req(
                "GET",
                &format!("/m/api/v1/codex/thread?session_id={id}"),
                Some(COOKIE),
                None,
            ))
            .await
            .unwrap()
        });
        if i == 0 {
            handshake(&mut peer, "owner-1").await;
        }
        let disc = peer.recv().await;
        assert_eq!(disc["method"], "thread-owner-discovery");
        // discovery 失败（no-client-found 语义）
        peer.send(&serde_json::json!({
            "type": "response", "requestId": disc["requestId"],
            "resultType": "error", "method": "thread-owner-discovery",
            "handledByClientId": "owner-1",
            "error": { "message": "no-client-found" }
        }))
        .await;
        let r = tokio::time::timeout(Duration::from_secs(15), task)
            .await
            .expect("get in time")
            .unwrap();
        assert_eq!(r.status(), 200, "ensure 失败回落 stale 投影仍 200");
    }
    // 第 65 个：完整成功 follow——若失败未回滚名额，这里会 Capacity
    let id65 = format!("aaaaaaaa-bbbb-4ccc-8ddd-{:012}", 64);
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={id65}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    drive_ensure_rev(
        &mut peer,
        &format!("aaaaaaaa-bbbb-4ccc-8ddd-{:012}", 64),
        vec![],
        "idle",
        7,
    )
    .await;
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("get in time")
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(
        v["stale"],
        serde_json::json!(false),
        "64 次失败 discovery 后名额未耗尽（失败回滚）"
    );
}

// ============================================================
// 17. 全链生产装配（审计交接要求 3）：production 形态派发缝（AdapterDispatch 包
// 真 adapter，不用 FakeDispatch 替身）+ hub.start 调度循环 + 真 router/bridge——
// HTTP send → 假 Desktop → 账本终态 → idle 投影释放顺序限制 → flush 接力。
// ============================================================

#[tokio::test]
async fn production_chain_send_busy_queue_then_idle_relay() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    // 生产生命周期装配（真实调度循环；对账周期收窄以便测试观察）
    f.state
        .codex_control
        .set_reconcile_period_ms(std::time::Duration::from_millis(200).as_millis() as u64);
    f.state.codex_control.start(f.state.clone());

    // ① 直接 POST send#1：ensure（idle snapshot）→ start-turn → accepted
    let body1 = send_body("全链第一条", Some("act-chain-1"), false);
    let task1 = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body1),
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let frame = peer.recv().await;
    assert_eq!(frame["method"], "thread-follower-start-turn");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "turn": { "id": "turn-chain-1", "status": "inProgress" } }
    }))
    .await;
    let r1 = tokio::time::timeout(Duration::from_secs(20), task1)
        .await
        .expect("chain send 1 in time")
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body_string(r1).await).unwrap();
    assert_eq!(v["receipt"]["delivery"], "accepted");
    assert_eq!(v["receipt"]["turnId"], "turn-chain-1");

    // ② 假 Desktop 推 active snapshot → send#2 忙态入队
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": THREAD, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": 8,
                "conversationState": {
                    "turns": [], "requests": [],
                    "threadRuntimeStatus": { "type": "active" },
                    "turnHistory": codex_fixture::canonical_turn_history(serde_json::json!({
                            "t9": { "id": "t9", "type": "turn", "status": "inProgress", "items": ["u9"] },
                            "u9": { "id": "u9", "type": "user_message", "text": "全链第一条 [mobile iPhone]", "ts": 9 }
                        }), serde_json::json!([{ "entries": ["t9"] }]))
                }
            }
        }
    }))
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let r2 = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(send_body("全链第二条", Some("act-chain-2"), false)),
        ))
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body_string(r2).await).unwrap();
    assert_eq!(v["status"], "codex_queued", "忙态入队");
    assert_eq!(
        f.hub_store()
            .get_for_device("act-chain-2", "dev-1")
            .unwrap()
            .unwrap()
            .state,
        ActionState::Queued
    );

    // ③ 假 Desktop 推 idle snapshot（turn-chain-1 已终态）→ 顺序限制释放 → flush 接力
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": THREAD, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": 9,
                "conversationState": {
                    "turns": [], "requests": [],
                    "threadRuntimeStatus": { "type": "idle" },
                    "turnHistory": codex_fixture::canonical_turn_history(serde_json::json!({
                            "turn-chain-1": { "id": "turn-chain-1", "type": "turn", "status": "completed", "items": ["u9"] },
                            "t9": { "id": "t9", "type": "turn", "status": "completed", "items": ["u9"] },
                            "u9": { "id": "u9", "type": "user_message", "text": "全链第一条 [mobile iPhone]", "ts": 9 }
                        }), serde_json::json!([{ "entries": ["turn-chain-1", "t9"] }]))
                }
            }
        }
    }))
    .await;
    // flush 接力发出 send#2
    let frame2 = tokio::time::timeout(Duration::from_secs(15), peer.recv())
        .await
        .expect("relay start-turn");
    assert_eq!(
        frame2["method"], "thread-follower-start-turn",
        "idle 投影驱动释放 + flush 接力（R6.3 + 全链）"
    );
    assert_eq!(
        frame2["params"]["turnStart"]["request"]["input"][0]["text"],
        "全链第二条 [mobile iPhone]"
    );
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame2["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "turn": { "id": "turn-chain-2", "status": "inProgress" } }
    }))
    .await;
    let v2 = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(Some(v)) = f.hub_store().get_for_device("act-chain-2", "dev-1") {
                if v.state == ActionState::Accepted {
                    return v;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("chain send 2 settles");
    assert_eq!(v2.receipt.unwrap().turn_id.as_deref(), Some("turn-chain-2"));
    // 顺序限制已随 turn-chain-2 的绑定入册（屏障在册 → 队列视图如实说明）
    assert!(
        f.hub_store()
            .has_compat_fence(&ThreadKey {
                host_id: "local".into(),
                thread_id: THREAD.into()
            })
            .unwrap(),
        "在飞 Turn 绑定以屏障槽位持久（blockedReason 语义）"
    );
    f.state.codex_control.stop();
}

/// 2026-10-17 复审 C1：等待者被取消后的收敛仍以「**绑定 Turn** 的终态证据」为准。
/// Accepted(turn-cvrg-1) 后：泛 idle（另一 turn t6 完成、绑定 turn 不在场）经多轮
/// 对账也**不得**接力第二条（修正旧预期「另一 turn 完成 + 泛 idle 即成功」的 PASS）；
/// 绑定 turn 以终态在场（snapshot rev 7）才恢复接力。
#[tokio::test]
async fn aborted_waiter_convergence_requires_bound_turn_terminal_evidence() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    f.state
        .codex_control
        .set_reconcile_period_ms(std::time::Duration::from_millis(200).as_millis() as u64);
    f.state.codex_control.start(f.state.clone());

    // ① send#1：ensure → start-turn 发出 → 中止 HTTP 等待者
    let body1 = send_body("被弃消息", Some("act-cvrg-1"), false);
    let task1 = tokio::spawn(async move {
        let _ = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some(COOKIE),
                Some(body1),
            ))
            .await;
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let frame = peer.recv().await;
    assert_eq!(frame["method"], "thread-follower-start-turn");
    task1.abort();

    // ② Desktop 正常应答：独立派发任务原子落账 accepted + Turn 屏障（含 turnId）
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "turn": { "id": "turn-cvrg-1", "status": "inProgress" } }
    }))
    .await;
    let view = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let v = f
                .hub_store()
                .get_for_device("act-cvrg-1", "dev-1")
                .unwrap()
                .unwrap();
            if v.state != ActionState::Reserved {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("dispatch task settles");
    assert_eq!(view.state, ActionState::Accepted);
    assert_eq!(
        view.receipt.as_ref().unwrap().turn_id.as_deref(),
        Some("turn-cvrg-1")
    );
    assert!(
        f.hub_store().has_compat_fence(&f.thread_key()).unwrap(),
        "accepted 屏障已随落账原子持久（C1.3）"
    );

    // ③ 泛 idle snapshot（rev 8 > ensure 基线 7）：只有**另一 turn t6** 完成、绑定
    //    turn-cvrg-1 不在场 → 多轮对账（≥3 tick）也不得接力第二条（C1.1：目标身份
    //    与终态证据不能省略）
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": THREAD, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": 8,
                "conversationState": {
                    "turns": [], "requests": [],
                    "threadRuntimeStatus": { "type": "idle" },
                    "turnHistory": codex_fixture::canonical_turn_history(serde_json::json!({
                            "t6": { "id": "t6", "type": "turn", "status": "completed", "items": ["u6"] },
                            "u6": { "id": "u6", "type": "user_message", "text": "被弃消息 [mobile iPhone]", "ts": 6 }
                        }), serde_json::json!([{ "entries": ["t6"] }]))
                }
            }
        }
    }))
    .await;
    // 负窗口覆盖 ≥5 个对账 tick（200ms 周期）：期间零 start-turn
    let neg = tokio::time::timeout(Duration::from_millis(1200), peer.recv()).await;
    assert!(
        neg.is_err(),
        "泛 idle + 另一 turn 完成：绑定 turn 无终态证据不得接力（旧预期在此错误 PASS）"
    );

    // ④ 绑定 turn 终态证据到位（turn-cvrg-1 completed，rev 9 > 8，runtime idle）
    //    → 收敛补绑 + 释放 → flush 接力第二条
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": THREAD, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": 9,
                "conversationState": {
                    "turns": [], "requests": [],
                    "threadRuntimeStatus": { "type": "idle" },
                    "turnHistory": codex_fixture::canonical_turn_history(serde_json::json!({
                            "turn-cvrg-1": { "id": "turn-cvrg-1", "type": "turn", "status": "completed", "items": ["u6"] },
                            "t6": { "id": "t6", "type": "turn", "status": "completed", "items": ["u6"] },
                            "u6": { "id": "u6", "type": "user_message", "text": "被弃消息 [mobile iPhone]", "ts": 6 }
                        }), serde_json::json!([{ "entries": ["turn-cvrg-1", "t6"] }]))
                }
            }
        }
    }))
    .await;
    let _ = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(send_body("收敛后第二条", Some("act-cvrg-2"), false)),
        ))
        .await
        .unwrap();
    let frame2 = tokio::time::timeout(Duration::from_secs(5), peer.recv())
        .await
        .expect("绑定 turn 终态证据到位后队列必须流动（第二个 start-turn 帧）");
    assert_eq!(frame2["method"], "thread-follower-start-turn");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame2["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "turn": { "id": "turn-cvrg-2", "status": "inProgress" } }
    }))
    .await;
    f.state.codex_control.stop();
}

// ============================================================
// 18. 2026-10-17 复审 C2：自动恢复两条生产断路——同 socket owner 失效/reset 的
// 重跟随（不依赖 GET），以及持久屏障 Thread 完整重启后自动重建投影源。
// ============================================================

/// 构造一帧 state-changed snapshot（C2 测试用：实体表自定）。
fn snapshot_frame(
    thread: &str,
    owner: &str,
    revision: u64,
    runtime: &str,
    entities: serde_json::Value,
    turn_ids: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": owner,
        "params": {
            "conversationId": thread, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": revision,
                "conversationState": {
                    "turns": [], "requests": [],
                    "threadRuntimeStatus": { "type": runtime },
                    "turnHistory": codex_fixture::canonical_turn_history(entities, turn_ids)
                }
            }
        }
    })
}

/// 2026-10-19 复审 P1：多义终态证据（同 turnId 多副本：缺 status 在前 / 状态
/// 冲突）**顺序无关地**不清持久屏障、不派发下一条；多副本同 completed 才放行。
/// 证据门本体（router）未被放宽——本测试走完整 hub 生产装配。
#[tokio::test]
async fn multi_copy_ambiguous_turn_evidence_never_clears_barrier() {
    fn two_copy_snapshot(
        rev: u64,
        a: (&str, Option<&str>),
        b: (&str, Option<&str>),
    ) -> serde_json::Value {
        let ent = |status: Option<&str>| {
            let mut e = serde_json::json!({
                "turnId": "turn-mc-1",
                "items": []
            });
            if let Some(st) = status {
                e["status"] = serde_json::json!(st);
            }
            e
        };
        serde_json::json!({
            "type": "snapshot", "revision": rev,
            "conversationState": {
                "turns": [], "requests": [],
                "threadRuntimeStatus": { "type": "idle" },
                "turnHistory": { "kind": "canonical", "history": {
                    "generation": 0, "isComplete": true,
                    "entitiesByKey": {
                        a.0: ent(a.1),
                        b.0: ent(b.1),
                    },
                    "islands": [ { "entries": [
                        { "key": a.0, "value": a.0 },
                        { "key": b.0, "value": b.0 }
                    ] } ]
                } }
            }
        })
    }
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    f.state
        .codex_control
        .set_reconcile_period_ms(std::time::Duration::from_millis(200).as_millis() as u64);
    f.state.codex_control.start(f.state.clone());

    // ① send#1：ensure → start-turn → 应答 accepted（绑定 turn-mc-1，屏障在册）
    let body1 = send_body("多义甲", Some("act-mc-1"), false);
    let task1 = tokio::spawn(async move {
        let _ = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some(COOKIE),
                Some(body1),
            ))
            .await;
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let frame = peer.recv().await;
    assert_eq!(frame["method"], "thread-follower-start-turn");
    task1.abort();
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "turn": { "id": "turn-mc-1", "status": "inProgress" } }
    }))
    .await;
    let view = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let v = f
                .hub_store()
                .get_for_device("act-mc-1", "dev-1")
                .unwrap()
                .unwrap();
            if v.state != ActionState::Reserved {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("dispatch settles");
    assert_eq!(view.state, ActionState::Accepted);
    assert!(f.hub_store().has_compat_fence(&f.thread_key()).unwrap());

    // ② 第二条入队（被屏障挡住）
    let _ = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(send_body("多义乙", Some("act-mc-2"), false)),
        ))
        .await
        .unwrap();

    // ③ 缺 status 副本在**前**、completed 在后 → 顺序无关多义：不清屏障、不派发
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": { "conversationId": THREAD, "hostId": "local",
                    "change": two_copy_snapshot(8, ("turn:a", None),
                                                ("turn:b", Some("completed"))) }
    }))
    .await;
    let neg = tokio::time::timeout(Duration::from_millis(1200), peer.recv()).await;
    assert!(neg.is_err(), "缺 status 副本在前：多义不得放行/接力");

    // ④ 状态冲突（completed 在前、inProgress 在后）→ 仍不放行
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": { "conversationId": THREAD, "hostId": "local",
                    "change": two_copy_snapshot(9, ("turn:b", Some("completed")),
                                                ("turn:c", Some("inProgress"))) }
    }))
    .await;
    let neg2 = tokio::time::timeout(Duration::from_millis(1200), peer.recv()).await;
    assert!(neg2.is_err(), "状态冲突不得放行/接力");

    // ⑤ 多副本同 completed → 证据成立：清屏障并只接力下一条
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": { "conversationId": THREAD, "hostId": "local",
                    "change": two_copy_snapshot(10, ("turn:b", Some("completed")),
                                                 ("turn:c", Some("completed"))) }
    }))
    .await;
    let frame2 = tokio::time::timeout(Duration::from_secs(5), peer.recv())
        .await
        .expect("多副本同 completed 后队列必须接力");
    assert_eq!(frame2["method"], "thread-follower-start-turn");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame2["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "turn": { "id": "turn-mc-2", "status": "inProgress" } }
    }))
    .await;
    f.state.codex_control.stop();
}

/// 2026-10-19 复审 P2-3：结构错误（非法 entries）经 HTTP 投影必须 Unsupported，
/// 不得 Ready + 0 条冒充真空会话。
#[tokio::test]
async fn invalid_structure_projects_unsupported_over_http() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let key = f.thread_key();
    let adapter = f.state.codex_control.adapter.clone();
    let task = {
        let a = adapter.clone();
        let k = key.clone();
        tokio::spawn(async move { a.ensure_thread(&k).await })
    };
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let _ = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("ensure in time");
    // 坏结构快照（rev 100 > ensure 基线 7）：非法 entries
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": THREAD, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": 100,
                "conversationState": {
                    "turns": [], "requests": [],
                    "threadRuntimeStatus": { "type": "idle" },
                    "turnHistory": { "kind": "canonical", "history": {
                        "generation": 0, "isComplete": true,
                        "entitiesByKey": {},
                        "islands": [ { "entries": ["not-an-object"] } ]
                    } }
                }
            }
        }
    }))
    .await;
    // 等镜像吃到坏帧（projection 只读轮询）
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if adapter.projection(&key).await.revision == Some(100) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "等待坏结构快照应用超时"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let r = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["contentState"], "unsupported", "坏结构不得 Ready：{v}");
    assert_eq!(v["messages"].as_array().map(|a| a.len()), Some(0));
    f.state.codex_control.stop();
}

/// C2.1：owner 失效（socket 仍活着）→ 必须**自动**重新 discovery/follow（无 GET、
/// 无待发工作），新 owner 的 snapshot 落进镜像、视图自动复活——不再永久 stale。
#[tokio::test]
async fn owner_lost_on_live_socket_auto_refollows() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let key = f.thread_key();
    // 先建立连接与跟随（本测试无 GET/POST 触发面）：并发 ensure + 协议驱动
    {
        let adapter = f.state.codex_control.adapter.clone();
        let k2 = key.clone();
        let ensure_task = tokio::spawn(async move { adapter.ensure_thread(&k2).await });
        drive_ensure(&mut peer, THREAD, vec![], "idle").await;
        let initial = tokio::time::timeout(Duration::from_secs(5), ensure_task)
            .await
            .expect("initial follow in time")
            .expect("initial follow ok");
        let _ = initial;
    }

    // owner-1 失效广播（连接不断）
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "client-status-changed", "version": 1,
        "sourceClientId": "owner-1",
        "params": { "clientId": "owner-1", "status": "disconnected" }
    }))
    .await;

    // C2：自动重恢复——discovery 帧**不经任何 GET/ensure** 到来
    let disc = tokio::time::timeout(Duration::from_secs(5), peer.recv())
        .await
        .expect("owner 失效后必须自动重发现（同 socket 恢复）");
    assert_eq!(disc["method"], "thread-owner-discovery");
    peer.send(&discovery_response(
        disc["requestId"].as_str().unwrap(),
        "owner-2",
    ))
    .await;
    let follow = tokio::time::timeout(Duration::from_secs(5), peer.recv())
        .await
        .expect("自动重声明 following");
    assert_eq!(follow["method"], "thread-stream-following-changed");
    assert_eq!(follow["params"]["following"], serde_json::json!(true));

    // 新 owner 的 snapshot：sourceClientId 与新 lease 一致 → 落进镜像
    let entities = serde_json::json!({
        "t8": { "id": "t8", "type": "turn", "status": "completed", "items": ["u8"] },
        "u8": { "id": "u8", "type": "user_message", "text": "新 owner 世界", "ts": 8 }
    });
    peer.send(&snapshot_frame(
        THREAD,
        "owner-2",
        8,
        "idle",
        entities,
        serde_json::json!(["t8"]),
    ))
    .await;

    // 视图自动复活（新鲜、rev 8、新 owner 世界）
    let p = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let p = f.state.codex_control.adapter.projection(&key).await;
            if !p.stale && p.revision == Some(8) {
                return p;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("视图必须在无 GET 的情况下自动恢复新鲜（C2：不再永久 stale）");
    assert_eq!(p.runtime_status, "idle");
}

/// C2.1：`ipc-connection-reset`（Desktop 要求重置状态，连接不断）→ 全体 stale 后
/// 自动重跟随，重新拿到 snapshot——视图不停在旧世界。
#[tokio::test]
async fn ipc_connection_reset_auto_refollows() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let key = f.thread_key();
    // 先建立连接与跟随（本测试无 GET/POST 触发面）：并发 ensure + 协议驱动
    {
        let adapter = f.state.codex_control.adapter.clone();
        let k2 = key.clone();
        let ensure_task = tokio::spawn(async move { adapter.ensure_thread(&k2).await });
        drive_ensure(&mut peer, THREAD, vec![], "idle").await;
        let initial = tokio::time::timeout(Duration::from_secs(5), ensure_task)
            .await
            .expect("initial follow in time")
            .expect("initial follow ok");
        let _ = initial;
    }

    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "ipc-connection-reset", "version": 1,
        "sourceClientId": "owner-1"
    }))
    .await;

    let disc = tokio::time::timeout(Duration::from_secs(5), peer.recv())
        .await
        .expect("reset 后必须自动重发现（同 socket 恢复）");
    assert_eq!(disc["method"], "thread-owner-discovery");
    peer.send(&discovery_response(
        disc["requestId"].as_str().unwrap(),
        "owner-1",
    ))
    .await;
    let follow = tokio::time::timeout(Duration::from_secs(5), peer.recv())
        .await
        .expect("自动重声明 following");
    assert_eq!(follow["method"], "thread-stream-following-changed");

    let entities = serde_json::json!({
        "t9": { "id": "t9", "type": "turn", "status": "completed", "items": ["u9"] },
        "u9": { "id": "u9", "type": "user_message", "text": "重置后世界", "ts": 9 }
    });
    peer.send(&snapshot_frame(
        THREAD,
        "owner-1",
        9,
        "idle",
        entities,
        serde_json::json!(["t9"]),
    ))
    .await;

    let p = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let p = f.state.codex_control.adapter.projection(&key).await;
            if !p.stale && p.revision == Some(9) {
                return p;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("reset 后视图必须自动恢复新鲜");
    assert_eq!(p.runtime_status, "idle");
}

/// C2.2：持久屏障 Thread 完整重启——丢弃全部内存重建 hub，**不打开详情、不手工
/// flush**：settle 主动 ensure 重建投影源，绑定 Turn 终态证据到位后自动接力队首。
#[tokio::test]
async fn hub_restart_with_turn_barrier_recovers_without_details() {
    let conn = shared_conn();
    seed_device(&conn, "dev-1", "iPhone");
    // —— 阶段一：accepted#1（屏障在册）→ active 排队 send#2 → 丢弃全部内存 ——
    {
        let (adapter, p1) = make_adapter_peer();
        let mut f1 = fixture_on(
            conn.clone(),
            vec![codex_app_session(THREAD)],
            move |store| Arc::new(CodexControlHub::with_adapter(store, adapter, true)),
            Some(p1),
        );
        let mut peer1 = f1.peer.take().unwrap();
        let app1 = f1.app.clone();
        let task1 = tokio::spawn(async move {
            app1.oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some(COOKIE),
                Some(send_body("屏障前已发", Some("act-bar-1"), false)),
            ))
            .await
            .unwrap()
        });
        drive_ensure(&mut peer1, THREAD, vec![], "idle").await;
        let frame = peer1.recv().await;
        assert_eq!(frame["method"], "thread-follower-start-turn");
        peer1
            .send(&serde_json::json!({
                "type": "response", "requestId": frame["requestId"],
                "resultType": "success", "method": "thread-follower-start-turn",
                "handledByClientId": "owner-1",
                "result": { "turn": { "id": "turn-b1", "status": "inProgress" } }
            }))
            .await;
        tokio::time::timeout(Duration::from_secs(10), task1)
            .await
            .expect("send#1 in time")
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(Some(v)) = f1.hub_store().get_for_device("act-bar-1", "dev-1") {
                    if v.state == ActionState::Accepted {
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("act-bar-1 accepted");
        // 转入 active（turn-b1 inProgress）→ send#2 忙态/顺序限制入队
        let entities = serde_json::json!({
            "turn-b1": { "id": "turn-b1", "type": "turn", "status": "inProgress", "items": ["u7"] },
            "u7": { "id": "u7", "type": "user_message", "text": "屏障前已发 [mobile iPhone]", "ts": 7 }
        });
        peer1
            .send(&snapshot_frame(
                THREAD,
                "owner-1",
                8,
                "active",
                entities,
                serde_json::json!(["turn-b1"]),
            ))
            .await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        let r2 = f1
            .app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some(COOKIE),
                Some(send_body("屏障后排队", Some("act-bar-2"), false)),
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r2).await).unwrap();
        assert_eq!(v["status"], "codex_queued", "屏障+忙态：send#2 入队");
        // f1 drop：hub/adapter/调度器全部丢弃，仅持久账本留存
    }
    // —— 阶段二：重建 hub，start 后不打开详情、不手工 flush ——
    let (adapter2, p2) = make_adapter_peer();
    let mut f2 = fixture_on(
        conn.clone(),
        vec![codex_app_session(THREAD)],
        move |store| Arc::new(CodexControlHub::with_adapter(store, adapter2, true)),
        Some(p2),
    );
    // 对账周期放宽：冷启动只有首 tick 的 settle 会 ensure（避免并发 ensure 打乱协议序）
    f2.state
        .codex_control
        .set_reconcile_period_ms(std::time::Duration::from_secs(10).as_millis() as u64);
    f2.state.codex_control.start(f2.state.clone());
    {
        let mut peer = f2.peer.take().unwrap();
        // C2：settle 主动 ensure（无 GET）——handshake → discovery → following
        handshake(&mut peer, "owner-1").await;
        let disc = peer.recv().await;
        assert_eq!(disc["method"], "thread-owner-discovery");
        peer.send(&discovery_response(
            disc["requestId"].as_str().unwrap(),
            "owner-1",
        ))
        .await;
        let follow = peer.recv().await;
        assert_eq!(follow["method"], "thread-stream-following-changed");
        // snapshot rev 9：turn-b1 completed + idle → 屏障释放 → flush 自动接力 send#2
        let entities = serde_json::json!({
            "turn-b1": { "id": "turn-b1", "type": "turn", "status": "completed", "items": ["u7"] },
            "u7": { "id": "u7", "type": "user_message", "text": "屏障前已发 [mobile iPhone]", "ts": 7 }
        });
        peer.send(&snapshot_frame(
            THREAD,
            "owner-1",
            9,
            "idle",
            entities,
            serde_json::json!(["turn-b1"]),
        ))
        .await;
        let frame2 = tokio::time::timeout(Duration::from_secs(10), peer.recv())
            .await
            .expect("重启后屏障 Thread 自动恢复接力（第二个 start-turn 帧）");
        assert_eq!(frame2["method"], "thread-follower-start-turn");
        assert_eq!(
            frame2["params"]["turnStart"]["request"]["input"][0]["text"],
            "屏障后排队 [mobile iPhone]"
        );
        peer.send(&serde_json::json!({
            "type": "response", "requestId": frame2["requestId"],
            "resultType": "success", "method": "thread-follower-start-turn",
            "handledByClientId": "owner-1",
            "result": { "turn": { "id": "turn-b2", "status": "inProgress" } }
        }))
        .await;
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(Some(v)) = f2.hub_store().get_for_device("act-bar-2", "dev-1") {
                if v.state == ActionState::Accepted {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("act-bar-2 accepted（重启恢复闭环）");
    f2.state.codex_control.stop();
}

/// 2026-10-17 复审 C4：hub.stop = 完整停机——adapter 进入 stopping、连接关闭
/// （peer 见 EOF）、ensure 不再新建连接（推进退避时间也不再有新 connect/follow）；
/// 重新 start 解除停机（io 耗尽时诚实快败，不悬挂）。
#[tokio::test]
async fn hub_stop_closes_connection_and_blocks_reconnect_until_start() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let key = f.thread_key();
    // 建连 + 跟随
    {
        let adapter = f.state.codex_control.adapter.clone();
        let k2 = key.clone();
        let ensure_task = tokio::spawn(async move { adapter.ensure_thread(&k2).await });
        drive_ensure(&mut peer, THREAD, vec![], "idle").await;
        tokio::time::timeout(Duration::from_secs(5), ensure_task)
            .await
            .expect("in time")
            .expect("join")
            .expect("fresh");
    }
    // stop：停机 + 连接关闭
    f.state.codex_control.stop();
    assert!(
        f.state.codex_control.adapter.is_stopping(),
        "stop 后 adapter 必须进入停机态"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut buf = vec![0u8; 64];
    let n = tokio::time::timeout(Duration::from_secs(5), peer.io.read(&mut buf))
        .await
        .expect("eof in time")
        .expect("read ok");
    assert_eq!(n, 0, "stop 后对端必须看到 EOF（连接已关闭）");
    // 停机态 ensure 拒绝：无新 connect/follow
    let r = f.state.codex_control.adapter.ensure_thread(&key).await;
    assert!(r.is_err(), "停机后 ensure 不再新建连接");
    // 重新 start：解除停机；connector io 已耗尽 → 诚实快败（不悬挂）
    f.state.codex_control.start(f.state.clone());
    assert!(
        !f.state.codex_control.adapter.is_stopping(),
        "start 解除停机"
    );
    let r2 = tokio::time::timeout(
        Duration::from_secs(5),
        f.state.codex_control.adapter.ensure_thread(&key),
    )
    .await
    .expect("no hang after restart");
    assert!(r2.is_err(), "io 耗尽诚实快败");
}

// ============================================================
// 19. 第三轮复审 B1：revision 基线门不得阻止合法 Turn 完成（生产链）——
// 完成快照先于 ACK 在场、换代后低 revision 新世界，都必须自动接力。
// ============================================================

/// B1 场景 1（生产链）：completed snapshot 先于 ACK 落镜像（之后零新增量）——
/// ACK 后后继消息必须自动接力，不得因「rev == 基线」永久堵队。
#[tokio::test]
async fn completed_snapshot_before_ack_still_relays() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    f.state
        .codex_control
        .set_reconcile_period_ms(std::time::Duration::from_millis(200).as_millis() as u64);
    f.state.codex_control.start(f.state.clone());
    let body1 = send_body("基线前已发", Some("act-b1-1"), false);
    let task1 = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body1),
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let frame = peer.recv().await;
    assert_eq!(frame["method"], "thread-follower-start-turn");
    // 完成快照**先于 ACK** 落镜像（rev 8，turn-chain-1 completed，idle）
    let entities = serde_json::json!({
        "turn-chain-1": { "id": "turn-chain-1", "type": "turn", "status": "completed", "items": ["u7"] },
        "u7": { "id": "u7", "type": "user_message", "text": "基线前已发 [mobile iPhone]", "ts": 7 }
    });
    peer.send(&snapshot_frame(
        THREAD,
        "owner-1",
        8,
        "idle",
        entities,
        serde_json::json!(["turn-chain-1"]),
    ))
    .await;
    // ACK（send_text 回执后采集的基线 == 已在场的 rev 8）
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "turn": { "id": "turn-chain-1", "status": "inProgress" } }
    }))
    .await;
    tokio::time::timeout(Duration::from_secs(15), task1)
        .await
        .expect("send#1 in time")
        .unwrap();
    // 后继入队；**不再发任何新 snapshot**——释放只能靠既有镜像中的完成证据
    let r2 = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(send_body("基线后排队", Some("act-b1-2"), false)),
        ))
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body_string(r2).await).unwrap();
    assert!(
        v["status"] == "codex_queued" || v["status"] == "desktop_ipc",
        "后继入队或直接派发：{v}"
    );
    let frame2 = tokio::time::timeout(Duration::from_secs(8), peer.recv())
        .await
        .expect("完成快照先于 ACK：后继必须自动接力（零新增量）");
    assert_eq!(frame2["method"], "thread-follower-start-turn");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame2["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "turn": { "id": "turn-b1-2", "status": "inProgress" } }
    }))
    .await;
    f.state.codex_control.stop();
}

/// B1 场景 2（生产链）：同进程 owner 换代（Desktop 重启语义）——旧基线 rev 7，
/// 新 owner 从 revision 1 重新发布 snapshot 且目标 Turn 已 completed → 顺序限制
/// 必须释放并自动接力（走 C2 自动恢复，无 GET）。
#[tokio::test]
async fn lower_revision_from_new_owner_still_releases() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    f.state
        .codex_control
        .set_reconcile_period_ms(std::time::Duration::from_millis(200).as_millis() as u64);
    f.state.codex_control.start(f.state.clone());
    let body1 = send_body("换代前已发", Some("act-gen-1"), false);
    let task1 = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body1),
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let frame = peer.recv().await;
    assert_eq!(frame["method"], "thread-follower-start-turn");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "turn": { "id": "turn-gen-1", "status": "inProgress" } }
    }))
    .await;
    tokio::time::timeout(Duration::from_secs(15), task1)
        .await
        .expect("send#1 in time")
        .unwrap();
    // owner-1 失效（同 socket 恢复）→ 新 owner-2 从 revision 1 重新发布：
    // 镜像换代重置后 rev 1 < 旧基线 7，且目标 Turn 已 completed
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "client-status-changed", "version": 1,
        "sourceClientId": "owner-1",
        "params": { "clientId": "owner-1", "status": "disconnected" }
    }))
    .await;
    let disc = peer.recv().await;
    assert_eq!(disc["method"], "thread-owner-discovery");
    peer.send(&discovery_response(
        disc["requestId"].as_str().unwrap(),
        "owner-2",
    ))
    .await;
    let follow = peer.recv().await;
    assert_eq!(follow["method"], "thread-stream-following-changed");
    let entities = serde_json::json!({
        "turn-gen-1": { "id": "turn-gen-1", "type": "turn", "status": "completed", "items": ["u6"] },
        "u6": { "id": "u6", "type": "user_message", "text": "换代前已发 [mobile iPhone]", "ts": 6 }
    });
    peer.send(&snapshot_frame(
        THREAD,
        "owner-2",
        1,
        "idle",
        entities,
        serde_json::json!(["turn-gen-1"]),
    ))
    .await;
    // 后继入队（换代窗口内顺序限制仍在 → codex_queued 或直接派发都可）
    let _ = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(send_body("换代后排队", Some("act-gen-2"), false)),
        ))
        .await
        .unwrap();
    let frame2 = tokio::time::timeout(Duration::from_secs(8), peer.recv())
        .await
        .expect("换代后低 revision 完成世界：后继必须自动接力");
    assert_eq!(frame2["method"], "thread-follower-start-turn");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame2["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-2",
        "result": { "turn": { "id": "turn-gen-2", "status": "inProgress" } }
    }))
    .await;
    f.state.codex_control.stop();
}

/// 第三轮复审 B2：同 socket 恢复单飞**不丢后到失效事件**——tA/tB 分属 owner
/// A/B，A 失效恢复在途（卡在 tA snapshot 等待）时 B 失效，B 的 keys 必须并入
/// 在飞任务的待办（任务退出边界的第三事件同样被消费）。全程同 socket、无 GET、
/// 无手工 ensure/flush，两 Thread 均自动发现新 owner 并发布 fresh snapshot。
#[tokio::test]
async fn interleaved_owner_failures_both_recover_without_get() {
    let id_a = "00000000-0000-4000-8000-0000000000a1";
    let id_b = "00000000-0000-4000-8000-0000000000b2";
    let mut f = fixture_ipc(vec![codex_app_session(id_a), codex_app_session(id_b)]);
    let mut peer = f.peer.take().unwrap();
    let key_a = ThreadKey {
        host_id: "local".into(),
        thread_id: id_a.into(),
    };
    let key_b = ThreadKey {
        host_id: "local".into(),
        thread_id: id_b.into(),
    };
    let adapter = f.state.codex_control.adapter.clone();
    // ① 两 Thread 分别 follow 到不同 owner（同一条连接）
    for (key, id, owner) in [(&key_a, id_a, "owner-a"), (&key_b, id_b, "owner-b")] {
        let a = adapter.clone();
        let k = key.clone();
        let task = tokio::spawn(async move { a.ensure_thread(&k).await });
        if key == &key_a {
            handshake(&mut peer, "owner-a").await;
        }
        let disc = peer.recv().await;
        assert_eq!(disc["method"], "thread-owner-discovery");
        peer.send(&discovery_response(
            disc["requestId"].as_str().unwrap(),
            owner,
        ))
        .await;
        let follow = peer.recv().await;
        assert_eq!(follow["method"], "thread-stream-following-changed");
        let entities = serde_json::json!({
            "t1": { "id": "t1", "type": "turn", "status": "completed", "items": ["u1"] },
            "u1": { "id": "u1", "type": "user_message", "text": "seed", "ts": 1 }
        });
        peer.send(&snapshot_frame(
            id,
            owner,
            1,
            "idle",
            entities,
            serde_json::json!(["t1"]),
        ))
        .await;
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("in time")
            .expect("join")
            .expect("fresh");
    }

    // ② owner-a 失效：恢复任务启动，卡在 tA 的 snapshot 等待（受控在途）
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "client-status-changed", "version": 1,
        "sourceClientId": "owner-a",
        "params": { "clientId": "owner-a", "status": "disconnected" }
    }))
    .await;
    let disc_a = peer.recv().await;
    assert_eq!(disc_a["method"], "thread-owner-discovery");
    peer.send(&discovery_response(
        disc_a["requestId"].as_str().unwrap(),
        "owner-a",
    ))
    .await;
    let follow_a = peer.recv().await;
    assert_eq!(follow_a["method"], "thread-stream-following-changed");

    // ③ owner-b 失效（恢复在途）：B 的 keys 必须并入待办而非被单飞丢弃
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "client-status-changed", "version": 1,
        "sourceClientId": "owner-b",
        "params": { "clientId": "owner-b", "status": "disconnected" }
    }))
    .await;

    // ④ tA snapshot 到达：tA 恢复 → 任务继续消费待办 {tB}
    let entities = serde_json::json!({
        "t2": { "id": "t2", "type": "turn", "status": "completed", "items": ["u2"] },
        "u2": { "id": "u2", "type": "user_message", "text": "A 恢复世界", "ts": 2 }
    });
    peer.send(&snapshot_frame(
        id_a,
        "owner-a",
        2,
        "idle",
        entities,
        serde_json::json!(["t2"]),
    ))
    .await;
    let disc_b = tokio::time::timeout(Duration::from_secs(8), peer.recv())
        .await
        .expect("B 的失效事件不得被单飞丢弃（旧实现 tB 永不重发现）");
    assert_eq!(
        disc_b["method"], "thread-owner-discovery",
        "tB 必须自动重发现"
    );
    assert_eq!(
        disc_b["params"]["conversationId"],
        serde_json::json!(id_b),
        "消费的正是后到事件的 tB"
    );
    peer.send(&discovery_response(
        disc_b["requestId"].as_str().unwrap(),
        "owner-b",
    ))
    .await;
    let follow_b = peer.recv().await;
    assert_eq!(follow_b["method"], "thread-stream-following-changed");
    let entities = serde_json::json!({
        "t2": { "id": "t2", "type": "turn", "status": "completed", "items": ["u2"] },
        "u2": { "id": "u2", "type": "user_message", "text": "B 恢复世界", "ts": 2 }
    });
    peer.send(&snapshot_frame(
        id_b,
        "owner-b",
        2,
        "idle",
        entities,
        serde_json::json!(["t2"]),
    ))
    .await;

    // ⑤ 两 Thread 均自动恢复新鲜（无 GET/无手工 ensure）
    for (key, _id, owner, text) in [
        (&key_a, id_a, "owner-a", "A 恢复世界"),
        (&key_b, id_b, "owner-b", "B 恢复世界"),
    ] {
        let p = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let p = adapter.projection(key).await;
                if !p.stale && p.revision == Some(2) {
                    return p;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("视图必须自动恢复新鲜");
        assert!(
            p.messages.iter().any(|m| m.text == text),
            "{owner} 世界生效"
        );
    }

    // ⑥ 退出边界：任务刚收尾时再来一个失效事件（tA）——事件必须被消费
    //（mirror 已被事件 invalidate → 恢复任务重发现可观察）
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "client-status-changed", "version": 1,
        "sourceClientId": "owner-a",
        "params": { "clientId": "owner-a", "status": "disconnected" }
    }))
    .await;
    let disc_a2 = tokio::time::timeout(Duration::from_secs(8), peer.recv())
        .await
        .expect("任务退出边界的事件不得丢失（第三事件被新任务消费）");
    assert_eq!(
        disc_a2["params"]["conversationId"],
        serde_json::json!(id_a),
        "重发现的是再次失效的 tA"
    );
    peer.send(&discovery_response(
        disc_a2["requestId"].as_str().unwrap(),
        "owner-a",
    ))
    .await;
    let follow_a2 = peer.recv().await;
    assert_eq!(follow_a2["method"], "thread-stream-following-changed");
    let entities = serde_json::json!({
        "t3": { "id": "t3", "type": "turn", "status": "completed", "items": ["u3"] },
        "u3": { "id": "u3", "type": "user_message", "text": "边界恢复世界", "ts": 3 }
    });
    peer.send(&snapshot_frame(
        id_a,
        "owner-a",
        3,
        "idle",
        entities,
        serde_json::json!(["t3"]),
    ))
    .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let p = adapter.projection(&key_a).await;
            if !p.stale && p.revision == Some(3) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("边界事件恢复 fresh");
}

/// 第四轮复审 B2 回归 2：预算耗尽**只放弃本批失败工作**，不得 clear 掉本轮
/// 「等待 A 响应期间刚加入」的后到 keys——tB 未尝试就被删除 = B 永不恢复。
/// 确定时序：第 3 轮 disc(A) 帧到达后，先令 tB 失效（入待办），再回 A 的
/// discovery 错误——旧实现在此 clear 整个共享待办（A+tB 一起丢），tB 的
/// discovery 帧永不出现；修复后 tB 必须被实际尝试并恢复。
#[tokio::test]
async fn budget_exhaustion_does_not_drop_later_keys() {
    let id_a = "00000000-0000-4000-8000-0000000000a1";
    let id_b = "00000000-0000-4000-8000-0000000000b2";
    let mut f = fixture_ipc(vec![codex_app_session(id_a), codex_app_session(id_b)]);
    let mut peer = f.peer.take().unwrap();
    let key_b = ThreadKey {
        host_id: "local".into(),
        thread_id: id_b.into(),
    };
    let adapter = f.state.codex_control.adapter.clone();
    // ① 两 Thread 分别 follow
    for (id, owner) in [(id_a, "owner-a"), (id_b, "owner-b")] {
        let key = ThreadKey {
            host_id: "local".into(),
            thread_id: id.into(),
        };
        let a = adapter.clone();
        let k = key.clone();
        let task = tokio::spawn(async move { a.ensure_thread(&k).await });
        if id == id_a {
            handshake(&mut peer, "owner-a").await;
        }
        let disc = peer.recv().await;
        assert_eq!(disc["method"], "thread-owner-discovery");
        peer.send(&discovery_response(
            disc["requestId"].as_str().unwrap(),
            owner,
        ))
        .await;
        let follow = peer.recv().await;
        assert_eq!(follow["method"], "thread-stream-following-changed");
        let entities = serde_json::json!({
            "t1": { "id": "t1", "type": "turn", "status": "completed", "items": ["u1"] },
            "u1": { "id": "u1", "type": "user_message", "text": "seed", "ts": 1 }
        });
        peer.send(&snapshot_frame(
            id,
            owner,
            1,
            "idle",
            entities,
            serde_json::json!(["t1"]),
        ))
        .await;
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("in time")
            .expect("join")
            .expect("fresh");
    }

    // ② owner-a 失效：恢复任务 take{A}，discovery 回**错误**（follow 失败 → 重试）
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "client-status-changed", "version": 1,
        "sourceClientId": "owner-a",
        "params": { "clientId": "owner-a", "status": "disconnected" }
    }))
    .await;
    let mut a_failures = 0usize;
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(5), peer.recv())
            .await
            .expect("A 的重试 discovery 帧必须到来");
        assert_eq!(frame["method"], "thread-owner-discovery");
        assert_eq!(frame["params"]["conversationId"], serde_json::json!(id_a));
        a_failures += 1;
        if a_failures == 3 {
            // 第三轮「等待 A 响应期间」：tB 失效入待办（B 尚未被尝试）
            peer.send(&serde_json::json!({
                "type": "broadcast", "method": "client-status-changed", "version": 1,
                "sourceClientId": "owner-b",
                "params": { "clientId": "owner-b", "status": "disconnected" }
            }))
            .await;
        }
        peer.send(&serde_json::json!({
            "type": "response", "resultType": "error",
            "requestId": frame["requestId"],
            "method": "thread-owner-discovery",
            "handledByClientId": "owner-a",
            "error": { "message": "no-client-found" }
        }))
        .await;
        if a_failures >= 3 {
            break;
        }
    }
    // 任务对本批 A 的重试预算耗尽：**tB 不得被连带放弃**——它的 discovery 帧必须
    // 出现（旧实现 clear 掉共享待办，此帧永不出现）
    let disc_b = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let frame = peer.recv().await;
            if frame["method"] == "thread-owner-discovery"
                && frame["params"]["conversationId"] == serde_json::json!(id_b)
            {
                return frame;
            }
        }
    })
    .await
    .expect("预算耗尽不得丢弃后到的 tB（B2 回归 2）");
    peer.send(&discovery_response(
        disc_b["requestId"].as_str().unwrap(),
        "owner-b",
    ))
    .await;
    let follow_b = peer.recv().await;
    assert_eq!(follow_b["method"], "thread-stream-following-changed");
    let entities = serde_json::json!({
        "t2": { "id": "t2", "type": "turn", "status": "completed", "items": ["u2"] },
        "u2": { "id": "u2", "type": "user_message", "text": "B 恢复世界", "ts": 2 }
    });
    peer.send(&snapshot_frame(
        id_b,
        "owner-b",
        2,
        "idle",
        entities,
        serde_json::json!(["t2"]),
    ))
    .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let p = adapter.projection(&key_b).await;
            if !p.stale && p.revision == Some(2) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("tB 必须被实际尝试并恢复 fresh");
}

// ============================================================
// 调度解耦回归（PR #165 审查 Important）：单 thread 的兜底（compat）回合是分钟级
// headless 执行，调度循环只触发不执行——一个 thread 的兜底在飞，其他 thread 的
// settle/接力/对账照常推进；同 thread 的重复触发在 permit 的 Thread guard
//（flush_thread try-lock）上排队，不并发第二个兜底。
// ============================================================

/// 假派发器（FakeDispatch 模式）：thread_busy 恒 false、A 路写恒 NotSent——
/// 「派发 → NotSent → 兜底资格」确定成立，不依赖 adapter/连接状态。
#[derive(Default)]
struct DeadIpcDispatch;

impl multi_agents_manager_lib::services::codex_control::DesktopDispatch for DeadIpcDispatch {
    fn thread_busy(&self, _key: &ThreadKey) -> bool {
        false
    }
    fn send_text(
        &self,
        _key: &ThreadKey,
        _action_id: &str,
        _composed: &str,
        _client_message_id: &str,
    ) -> multi_agents_manager_lib::services::codex_control::BoxDispatchFut {
        Box::pin(async move {
            multi_agents_manager_lib::services::codex_control::DispatchOutcome {
                delivery: Delivery::NotSent,
                envelope_id: Some("env-dead".into()),
                turn_id: None,
                reason_code: Some("ipc_disconnected".into()),
                revision: None,
            }
        })
    }
    fn respond(
        &self,
        _key: &ThreadKey,
        _request_key: &str,
        _context_token: &str,
        _action: &multi_agents_manager_lib::services::codex_control::PendingAction,
    ) -> multi_agents_manager_lib::services::codex_control::BoxDispatchFut {
        Box::pin(async move {
            multi_agents_manager_lib::services::codex_control::DispatchOutcome {
                delivery: Delivery::NotSent,
                envelope_id: Some("env-dead".into()),
                turn_id: None,
                reason_code: Some("ipc_disconnected".into()),
                revision: None,
            }
        })
    }
}

/// 死 IPC fixture：A 路派发经假派发器恒 NotSent（兜底资格成立）；hub capable=true
/// 装配真实调度循环。兜底的实际执行时长由 `set_compat_min_duration_ms` 测试缝完全
/// 控制（IsolatedPath 令 CLI 发现秒拒，绝不 spawn 真 codex）。
fn fixture_dead_ipc(sessions: Vec<Session>) -> Fixture {
    let (io, peer) = ScriptedPeer::new();
    drop(peer);
    let adapter = Arc::new(DesktopIpcAdapter::new(
        Arc::new(StaticConnector::new(io)),
        caps_open_profile(),
    ));
    fixture_with(
        sessions,
        move |store| {
            Arc::new(CodexControlHub::with_parts(
                store,
                adapter,
                Arc::new(DeadIpcDispatch),
                true,
            ))
        },
        None,
    )
}

/// 死 IPC 测试的启动序：start 后立即 begin_shutdown——ensure 秒败（不建镜像、
/// 不等 3s 握手超时），调度只走假派发器的 NotSent → 兜底交接链。
fn start_dead_ipc(f: &Fixture) {
    f.state.codex_control.start(f.state.clone());
    f.state.codex_control.adapter.begin_shutdown();
}

/// 直读动作行 route 列（交接态观测：reserved + headless_compat = 兜底已交接在飞）。
fn route_of_conn(f: &Fixture, action_id: &str) -> String {
    let conn = f.conn.lock().unwrap();
    conn.query_row(
        "SELECT route FROM codex_actions WHERE action_id = ?1",
        [action_id],
        |r| r.get(0),
    )
    .unwrap()
}

/// 直接向 hub 账本登记一条 queued 普通消息（不经 HTTP；调度循环对账接力的输入）。
fn register_queued(f: &Fixture, action_id: &str, thread_id: &str, text: &str) {
    let ctx = ActionContext {
        action_id: action_id.into(),
        device_id: "dev-1".into(),
        device_name: "iPhone".into(),
        kind: ActionKind::Send,
        key: ThreadKey {
            host_id: "local".into(),
            thread_id: thread_id.into(),
        },
    };
    let hash = payload_hash(
        ActionKind::Send,
        &ctx.key,
        None,
        None,
        &ActionPayload::Text(text.into()),
    );
    f.hub_store()
        .register(&ctx, ActionKind::Send, None, None, &hash, Some(text))
        .unwrap();
}

/// 轮询到动作达到 reserved + headless_compat（兜底交接完成、兜底执行在飞）。
async fn wait_handoff(f: &Fixture, action_id: &str, budget: Duration) {
    tokio::time::timeout(budget, async {
        loop {
            if let Ok(Some(v)) = f.hub_store().get_for_device(action_id, "dev-1") {
                if v.state == ActionState::Reserved
                    && route_of_conn(f, action_id) == "headless_compat"
                {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        let state = f
            .hub_store()
            .get_for_device(action_id, "dev-1")
            .ok()
            .flatten()
            .map(|v| format!("{:?}", v.state))
            .unwrap_or_else(|| "no-row".into());
        panic!(
            "{action_id} 兜底交接超时（state={state} route={}）",
            route_of_conn(f, action_id)
        );
    });
}

/// 回归 1（跨 thread 不阻塞）：T1 的慢兜底在飞期间，T2 的新入队消息仍能在预算内
/// 走完 settle → A 路 NotSent → 兜底交接。缺陷态（调度循环 await 兜底）下 T2 的
/// 交接必须等 T1 兜底结束（缝 5s ≫ 预算 2s），本用例必然超时。
#[tokio::test]
async fn slow_compat_fallback_does_not_block_other_thread_scheduling() {
    let _path = IsolatedPath::lock_empty();
    let t1 = THREAD;
    let t2 = "0f1e2d3c-2222-4222-8333-444455556666";
    let f = fixture_dead_ipc(vec![codex_app_session(t1), codex_app_session(t2)]);
    f.state.store.with(|c| {
        c.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('remote.headless_enabled', 'true')",
            [],
        )
        .unwrap();
    });
    f.state.codex_control.set_reconcile_period_ms(100);
    // 测试缝：兜底回合最短 5s（模拟分钟级兜底；T2 推进预算 2s ≪ 5s）
    f.state
        .codex_control
        .set_compat_min_duration_ms(std::time::Duration::from_secs(5).as_millis() as u64);
    start_dead_ipc(&f);

    // T1：入队 → 派发 NotSent → 交接 → detached 兜底入睡
    register_queued(&f, "act-blk-1", t1, "T1 慢兜底");
    wait_handoff(&f, "act-blk-1", Duration::from_secs(5)).await;

    // T1 兜底入睡期间：T2 入队 → settle/接力/对账必须照常推进到兜底交接
    register_queued(&f, "act-blk-2", t2, "T2 不被阻塞");
    wait_handoff(&f, "act-blk-2", Duration::from_secs(2)).await;
    assert_eq!(
        f.hub_store()
            .get_for_device("act-blk-1", "dev-1")
            .unwrap()
            .unwrap()
            .state,
        ActionState::Reserved,
        "T1 兜底仍在飞（慢兜底未结束）——T2 的推进发生在其执行期间"
    );

    // detached 兜底最终自行落账（空 PATH CLI 缺失 → NotSent → rejected），不依赖调度循环
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(Some(v)) = f.hub_store().get_for_device("act-blk-1", "dev-1") {
                if v.state == ActionState::Rejected {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("T1 detached 兜底落账");
    f.state.codex_control.stop();
}

/// 回归 2（同 thread 串行化）：T1 的兜底在飞期间，同 thread 的重复触发（每周期
/// 对账 settle）在 permit 的 Thread guard 上排队——第二条消息留在 queued，绝不
/// 并发第二个兜底；第一条落账后 notify_settled 接力才放行第二条。
#[tokio::test]
async fn same_thread_retrigger_queues_behind_in_flight_compat() {
    let _path = IsolatedPath::lock_empty();
    let f = fixture_dead_ipc(vec![codex_app_session(THREAD)]);
    f.state.store.with(|c| {
        c.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('remote.headless_enabled', 'true')",
            [],
        )
        .unwrap();
    });
    f.state.codex_control.set_reconcile_period_ms(100);
    // 测试缝：兜底回合最短 3s——在飞窗口 1.5s 内反复触发也只此一个兜底
    f.state
        .codex_control
        .set_compat_min_duration_ms(std::time::Duration::from_secs(3).as_millis() as u64);
    start_dead_ipc(&f);

    // 同 thread 两条 FIFO：队首 act-ser-1 交接兜底入睡，act-ser-2 留队
    register_queued(&f, "act-ser-1", THREAD, "串行第一条");
    register_queued(&f, "act-ser-2", THREAD, "串行第二条");
    wait_handoff(&f, "act-ser-1", Duration::from_secs(5)).await;

    // 在飞窗口：周期对账反复 settle 本 thread（≈10 次），第二条必须留在 queued
    //（flush_thread try-lock 让路——permit guard 在兜底任务手里），绝不二次派发
    for _ in 0..10 {
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            f.hub_store()
                .get_for_device("act-ser-2", "dev-1")
                .unwrap()
                .unwrap()
                .state,
            ActionState::Queued,
            "兜底在飞期间同 thread 不得并发第二个兜底"
        );
        assert_eq!(
            f.hub_store()
                .get_for_device("act-ser-1", "dev-1")
                .unwrap()
                .unwrap()
                .state,
            ActionState::Reserved,
            "窗口内第一条兜底持续在飞"
        );
    }

    // 第一条落账（NotSent → rejected）→ 任务尾部 notify_settled → 第二条才接力交接
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if let Ok(Some(v)) = f.hub_store().get_for_device("act-ser-1", "dev-1") {
                if v.state == ActionState::Rejected {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("第一条兜底落账");
    wait_handoff(&f, "act-ser-2", Duration::from_secs(5)).await;
    f.state.codex_control.stop();
}
