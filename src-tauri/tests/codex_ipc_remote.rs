//! Codex Desktop IPC 远程控制（计划 Task 8）HTTP 集成测试：认证后的新端点、
//! session-send/send-info 的 hub 接线、兼容兜底 H3 门、审计与生命周期。
//!
//! 零真实副作用纪律：`DeviceStore::Owned(共享内存连接)`（state.store 与 hub 的
//! ActionStore 同库同连接）、Desktop IPC 走 Tokio duplex `ScriptedPeer`（本地假
//! Desktop，**绝不连接真实官方 Desktop**）、home_source 恒 None（rollout 解析只走
//! 会话号 UUID 回退，**绝不读真实 ~/.codex**）；兜底路径的 CLI 发现经临时空 PATH
//! 隔离（**绝不 spawn 真 codex**）。

use multi_agents_manager_lib::remote::codex_ipc::CodexControlHub;
use multi_agents_manager_lib::remote::pairing::{
    persist_device, revoke_device, DeviceStore, NewDevice,
};
use multi_agents_manager_lib::remote::server::{router, RemoteState};
use multi_agents_manager_lib::services::codex_control::{
    payload_hash, ActionContext, ActionKind, ActionPayload, ActionState, ActionStore, ControlRoute,
    Delivery, ThreadKey,
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
    io: StdMutex<Option<Box<dyn IpcIo>>>,
}

impl StaticConnector {
    fn new(io: Box<dyn IpcIo>) -> Self {
        Self {
            io: StdMutex::new(Some(io)),
        }
    }
}

impl Connector for StaticConnector {
    fn connect(
        &self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        BoxIo,
                        multi_agents_manager_lib::services::codex_desktop_ipc::types::ProtocolError,
                    >,
                > + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            self.io
                .lock()
                .expect("lock")
                .take()
                .ok_or_else(|| multi_agents_manager_lib::services::codex_desktop_ipc::types::ProtocolError::Disconnected("no io".into()))
        })
    }
}

/// 全能力测试 profile（macOS 已验证 build + 全 flag：只测构造器，不改生产 profile）。
fn caps_open_profile(
) -> multi_agents_manager_lib::services::codex_desktop_ipc::profile::ProtocolProfile {
    let mut p = profile_for("macos", "26.1013.31256").unwrap();
    p.capabilities = multi_agents_manager_lib::services::codex_desktop_ipc::types::CapabilitySet {
        send: true,
        state: true,
        command: true,
        permissions: true,
        blocking_input: true,
    };
    p
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
                    "turnHistory": { "isComplete": true, "history": {
                        "entitiesByKey": {
                            "t1": { "id": "t1", "type": "turn", "status": "completed", "items": ["u1"] },
                            "u1": { "id": "u1", "type": "user_message", "text": "seed", "ts": 1 }
                        },
                        "islands": [{ "entries": ["t1"] }]
                    }}
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

/// hub 不可用 fixture（disabled：一切能力关闭）。
fn fixture_disabled(sessions: Vec<Session>) -> Fixture {
    fixture_with(sessions, |_| CodexControlHub::disabled_arc(), None)
}

fn fixture_with(
    sessions: Vec<Session>,
    hub_builder: impl FnOnce(DeviceStore) -> Arc<CodexControlHub>,
    peer: Option<ScriptedPeer>,
) -> Fixture {
    let conn = shared_conn();
    seed_device(&conn, "dev-1", "iPhone");
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
                    "turnHistory": { "isComplete": true, "history": {
                        "entitiesByKey": {
                            "t2": { "id": "t2", "type": "turn", "status": "inProgress", "items": ["u2"] },
                            "u2": { "id": "u2", "type": "user_message", "text": "running", "ts": 2 }
                        },
                        "islands": [{ "entries": ["t2"] }]
                    }}
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

/// 手机断开 POST 不取消已派发写动作：handler future 被中止（模拟断连）后，
/// start-turn 已写出的动作保持 reserved（留至落账或 unknown），零二次写入。
#[tokio::test]
async fn client_disconnect_does_not_cancel_dispatched_write() {
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
    // 此时手机断连：handler future 被丢弃
    task.abort();
    // Desktop 侧仍正常应答（写动作不可撤回）
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "turn": { "id": "turn-d", "status": "inProgress" } }
    }))
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let view = f
        .hub_store()
        .get_for_device("act-disc-1", "dev-1")
        .unwrap()
        .unwrap();
    assert_eq!(
        view.state,
        ActionState::Reserved,
        "已派发动作留至落账或 unknown，绝不回 queued"
    );
    assert!(!peer_has_traffic(&mut peer).await, "断连后无二次写入");
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

/// 无投影（disabled hub）保持旧路径：补投不经本 Task 阻断（compat 场景唯一交互面；
/// 空 PATH 下 CLI 缺失 → 200 headless 封套如实失败，而非 503 阻断）。
#[tokio::test]
async fn app_repost_kept_when_no_projection() {
    let _path = IsolatedPath::lock_empty();
    let f = fixture_disabled(vec![codex_app_session(THREAD)]);
    // H3 开：补投是受 H3 门控的既有路径（H3 关 → 403 headless_disabled 属既有语义，
    // 非本 Task 改动面）；空 PATH 下 CLI 缺失 → 200 headless 封套如实失败
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
            Some(
                serde_json::json!({
                    "sessionId": THREAD, "action": "freeText", "text": "compat 补答",
                })
                .to_string(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["status"], "headless");
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
