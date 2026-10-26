//! T8.1 生产链 e2e（第一条切片）：官方宿主假体 × 真实命令层 / 移动读链。
//!
//! # 覆盖
//!
//! 1. `two_devices_same_action_single_dispatch_across_disconnect`：
//!    真实 `CommandSender` + 真实 `ActionStore`（内存 `DeviceStore`）+ 进程内
//!    `FakeOfficial`。同一 actionId 的两次提交之间发生官方链路断开——上游
//!    `sendText` 恰一帧，两次回执逐字段相同。
//! 2. `full_chain_reads_are_independent_of_control_failure`：
//!    Hub 处于 `needs_auth` / `offline` 时，既有移动读链（`/session-messages`、
//!    `/file`、`/session-files`）仍 200；控制面经 HTTP 如实 503 且上游零帧。
//!
//! # 夹具纪律
//!
//! 全部合成值与 tempdir/内存库：不读真实 `~/.zcode`、`~/.tuvis`，不发起任何官方
//! 网络请求，不写入任何真实账号凭据。进程内官方宿主的偏差申报见
//! `tests/common/zcode.rs` 模块文档（loopback HTTP+WS 已由 T1/T2 的 fake 覆盖，
//! 本切片在 HubBackend/端口边界扮演宿主）。
//!
//! # `actionId` 身份的已知边界（本用例的表述口径）
//!
//! 账本身份 = `(device, session, generation, payload)`：同一 actionId 的重放只有
//! 在**同一配对设备**上才返回既有回执；另一台真正不同的配对设备复用同一 id 会被
//! 判为 `action_payload_conflict`（T3 裁决：跨设备绝不泄漏回执）。因此下面「两台
//! 设备共享同一 actionId」的可重放路径按同一配对设备（设备身份来自配对门/本机
//! DB，不来自请求体）跨断链重试来验证，并另断言跨设备复用被拒、零第二帧。

mod common;

use common::zcode::{send_policy, stop_policy, FakeOfficial};
use multi_agents_manager_lib::adapter::AgentProcess;
use multi_agents_manager_lib::monitor::zcode_parser::{build_sessions, ZcodeRoots};
use multi_agents_manager_lib::remote::codex_ipc::CodexControlHub;
use multi_agents_manager_lib::remote::content::read_session_messages_with;
use multi_agents_manager_lib::remote::files::extract_file_paths_with;
use multi_agents_manager_lib::remote::pairing::{
    persist_device, revoke_device, DeviceStore, NewDevice,
};
use multi_agents_manager_lib::remote::server::{router, RemoteState};
use multi_agents_manager_lib::services::zcode_control::{
    codes, set_config, ActionContext, ActionState, CapabilityView, CommandSender, ConnectionState,
    ControlAction, HubBackend, OAuthProvider, PendingKind, PendingView, SessionView, TargetKey,
    ZcodeError, ZcodeHub,
};
use multi_agents_manager_lib::session::{ProcessForm, Session, SessionStatus, SessionsResponse};
use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tower::ServiceExt as _;

type SharedConn = Arc<Mutex<Connection>>;

const COOKIE: &str = "mam_device=dev-e2e";
const DEVICE_ID: &str = "dev-e2e";
/// 合成 ZCode 主会话 id（`session_` + 32 hex，A6 合法域；不来自真实用户）。
const SESSION: &str = "session_0f1e2d3c4b5a494882769a0b8c7d6e5f";
const SYNTH_URL: &str = "https://zcode.z.ai/remote/v4?sid=synthetic-e2e-0001&name=E2E";
const ACTION_ID: &str = "e2e-action-0001";
const SEND_TEXT: &str = "合成正文第一行\n第二行";

/// 动作账本行数（一次真正派发恰一行；重放/冲突零新行）。
fn ledger_rows(store: &DeviceStore) -> i64 {
    store.with(|c| {
        c.query_row("SELECT COUNT(*) FROM zcode_actions", [], |r| r.get(0))
            .expect("zcode_actions 可读")
    })
}

/// 已起账的发送动作数（`started` 审计只在预留成功时写；重放不得重复起账）。
fn started_audits(store: &DeviceStore) -> i64 {
    store.with(|c| {
        c.query_row(
            "SELECT COUNT(*) FROM write_audit WHERE action = 'zcode_send' AND result = 'started'",
            [],
            |r| r.get(0),
        )
        .expect("write_audit 可读")
    })
}

// ============================================================
// 用例 1：同 actionId 跨断链恰一帧
// ============================================================

#[tokio::test]
async fn two_devices_same_action_single_dispatch_across_disconnect() {
    let store = DeviceStore::memory();
    let official = FakeOfficial::new(store.clone(), send_policy());
    // 真实命令层 + 真实账本：设备身份由配对门/本机 DB 填充（不来自请求体）。
    let sender: CommandSender = official.sender();

    let target = official
        .bind_session(&TargetKey {
            session_id: SESSION.to_owned(),
        })
        .await
        .expect("假宿主必须能精确绑定合成会话");
    let ctx = ActionContext {
        action_id: ACTION_ID.to_owned(),
        device_id: DEVICE_ID.to_owned(),
        device_name: "iPhone".to_owned(),
    };
    let action = ControlAction::Send {
        session_id: SESSION.to_owned(),
        generation: target.generation,
        text: SEND_TEXT.to_owned(),
    };

    // 第一次点击：唯一一次真正离机的 sendText（正文原样，含真实换行）。
    let first = sender
        .execute(&ctx, &target, &action)
        .await
        .expect("首次派发必须留下回执");
    assert_eq!(first.state, ActionState::Accepted);
    assert_eq!(official.send_text_frames(), 1, "首次只允许一帧");
    let frame = official.frames().remove(0);
    assert_eq!(frame.envelope_type(), Some("sendText"));
    assert_eq!(frame.method, "sendConversationCommandV4");
    assert_eq!(frame.text(), Some(SEND_TEXT), "正文必须原样离机");
    assert_eq!(frame.envelope()["payload"]["text"], SEND_TEXT);

    // 两次提交之间官方链路断开：此后任何新动作都不可能离机（假体确证这一点）。
    official.drop_link();

    // 第二台设备（同 actionId，同配对身份）在断链后重放：返回既有回执，零新帧。
    let second = sender
        .execute(&ctx, &target, &action)
        .await
        .expect("同 id 同载荷重放必须命中既有回执");
    assert_eq!(
        second, first,
        "两次提交看到的回执必须逐字段相同（含 actionId/session/generation/state）"
    );
    assert_eq!(official.send_text_frames(), 1, "全生命周期只允许一帧");
    assert_eq!(ledger_rows(&store), 1, "账本只允许一行（一次派发）");
    assert_eq!(
        started_audits(&store),
        1,
        "起始审计只允许一条（重放不得再次起账）"
    );

    // 断链是真实生效的：同身份的新动作零帧且只能保守 unknown。
    let dropped = sender
        .execute(
            &ActionContext {
                action_id: "e2e-action-probe".to_owned(),
                ..ctx.clone()
            },
            &target,
            &ControlAction::Send {
                session_id: SESSION.to_owned(),
                generation: target.generation,
                text: "断链后探针".to_owned(),
            },
        )
        .await
        .expect("写后结果未知仍必须给回执");
    assert_eq!(
        dropped.state,
        ActionState::Unknown,
        "断链后的新写只能保守 unknown"
    );
    assert_eq!(official.send_text_frames(), 1, "断链后零帧");

    // 跨设备复用同一 actionId：冲突且零帧（跨设备绝不泄漏回执）。
    let other_device = sender
        .execute(
            &ActionContext {
                device_id: "dev-e2e-other".to_owned(),
                device_name: "iPad".to_owned(),
                ..ctx.clone()
            },
            &target,
            &action,
        )
        .await
        .unwrap_err();
    assert_eq!(other_device.code, codes::ACTION_PAYLOAD_CONFLICT);
    assert_eq!(official.send_text_frames(), 1, "冲突不得产生第二帧");
    assert_eq!(
        ledger_rows(&store),
        2,
        "冲突不得新增账本行（1 次发送 + 1 次探针）"
    );
}

// ============================================================
// 用例 2：控制面失败不影响移动读链
// ============================================================

const USER_BODY: &str = "synthetic-user-body 合成用户正文";
const ASSIST_BODY: &str = "synthetic-assistant-body 合成助手正文";
const PREVIEW_BODY: &str = "synthetic preview body";

fn shared_conn() -> SharedConn {
    let conn = Connection::open_in_memory().expect("open memory db");
    multi_agents_manager_lib::database::schema::init(&conn);
    multi_agents_manager_lib::database::migration::migrate(&conn).expect("migrate");
    Arc::new(Mutex::new(conn))
}

/// 合成设备 + 已保存的官方链接（同一内存库：与生产一致的三方共享）。
fn seed(conn: &SharedConn) {
    DeviceStore::Owned(conn.clone()).with(|c| {
        persist_device(
            c,
            &NewDevice {
                id: DEVICE_ID.to_owned(),
                name: "iPhone".into(),
                ua: format!("ua-{DEVICE_ID}"),
                origin_ip: format!("127.0.0.1#{DEVICE_ID}"),
                via: "lan".into(),
                paired_at: chrono::Utc::now().timestamp_millis(),
            },
        )
        .expect("seed device");
        set_config(c, SYNTH_URL).expect("save synthetic url");
    });
}

/// ZCode cli 库夹具（与 `zcode_read_remote.rs` 同构，只用合成 id/正文）。
fn build_cli_db(path: &std::path::Path) -> Connection {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE session (
            id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT,
            task_type TEXT, title TEXT, time_updated INTEGER
         );
         CREATE TABLE message (
            id TEXT PRIMARY KEY, session_id TEXT, sequence INTEGER,
            time_created INTEGER, data TEXT
         );
         CREATE TABLE part (
            id TEXT PRIMARY KEY, message_id TEXT, sequence INTEGER, data TEXT
         );",
    )
    .unwrap();
    conn
}

fn insert_session(conn: &Connection, id: &str, directory: &str, time_updated: i64) {
    conn.execute(
        "INSERT INTO session (id, parent_id, directory, task_type, title, time_updated)
         VALUES (?1, NULL, ?2, 'interactive', NULL, ?3)",
        rusqlite::params![id, directory, time_updated],
    )
    .unwrap();
}

fn insert_message(conn: &Connection, id: &str, session_id: &str, seq: i64, data: &str) {
    let now = chrono::Utc::now().timestamp_millis();
    conn.execute(
        "INSERT INTO message (id, session_id, sequence, time_created, data)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![id, session_id, seq, now + seq, data],
    )
    .unwrap();
}

fn insert_part(conn: &Connection, id: &str, message_id: &str, seq: i64, data: &str) {
    conn.execute(
        "INSERT INTO part (id, message_id, sequence, data) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![id, message_id, seq, data],
    )
    .unwrap();
}

fn fake_host() -> AgentProcess {
    AgentProcess {
        pid: 4242,
        cpu_usage: 1.5,
        cwd: None,
        exe: Some(PathBuf::from(
            "/Applications/ZCode.app/Contents/MacOS/ZCode",
        )),
        form: ProcessForm::App,
    }
}

/// 读链夹具：tempdir 下的 ZCode cli 库 + 真实项目目录（含可预览文件）。
fn read_fixture() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let project = tmp.path().join("synth-proj");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("preview.txt"), PREVIEW_BODY).unwrap();

    let roots = ZcodeRoots::from_home(&home);
    let cli = build_cli_db(&roots.cli_db);
    let now = chrono::Utc::now().timestamp_millis();
    insert_session(&cli, SESSION, project.to_str().unwrap(), now - 1000);
    insert_message(
        &cli,
        "m_e2e_1",
        SESSION,
        1,
        r#"{"role":"user","semantics":{"kind":"user_prompt"}}"#,
    );
    insert_part(
        &cli,
        "p_e2e_1",
        "m_e2e_1",
        1,
        &format!(r#"{{"type":"text","text":"{USER_BODY}"}}"#),
    );
    // 用户附件 part：附件表读链的非退化样本（与 /file 预览指向同一合成文件）。
    insert_part(
        &cli,
        "p_e2e_file",
        "m_e2e_1",
        2,
        &format!(
            r#"{{"type":"file","source":{{"path":"{}"}}}}"#,
            project.join("preview.txt").display()
        ),
    );
    insert_message(
        &cli,
        "m_e2e_2",
        SESSION,
        2,
        r#"{"role":"assistant","semantics":{"kind":"assistant"}}"#,
    );
    insert_part(
        &cli,
        "p_e2e_2",
        "m_e2e_2",
        1,
        &format!(r#"{{"type":"text","text":"{ASSIST_BODY}"}}"#),
    );
    drop(cli);
    (tmp, home)
}

/// 生产读链装配：会话快照（真实 parser）+ 正文/文件源（真实读取器）+ 内存库。
fn read_state(conn: &SharedConn, home: &std::path::Path) -> Arc<RemoteState> {
    let roots = ZcodeRoots::from_home(home);
    let host = fake_host();
    let session_source = Box::new(move || {
        let sessions: Vec<Session> =
            build_sessions(&roots, &host, chrono::Utc::now().timestamp_millis());
        let waiting_count = sessions
            .iter()
            .filter(|s| matches!(s.status, SessionStatus::Waiting))
            .count();
        SessionsResponse {
            total_count: sessions.len(),
            waiting_count,
            sessions,
        }
    });
    let msg_home = home.to_path_buf();
    let message_source = Box::new(move |agent: &str, sid: &str, limit: usize| {
        read_session_messages_with(&msg_home, agent, sid, limit)
    });
    let path_home = home.to_path_buf();
    let path_source = Box::new(move |agent: &str, sid: &str, limit: usize| {
        extract_file_paths_with(&path_home, agent, sid, limit)
    });
    let blacklist_home = home.to_string_lossy().to_string();

    Arc::new(RemoteState {
        ui_config_source: Box::new(|| None),
        subagent_source: std::collections::HashMap::new(),
        subagent_message_source: std::collections::HashMap::new(),
        capability_table: multi_agents_manager_lib::inject::capability::new_table(),
        session_source,
        pairing_counter: Box::new(Vec::new),
        board_hidden_ids: Box::new(Vec::new),
        board_hidden_hide: Arc::new(|_| 0usize),
        board_hidden_unhide: Arc::new(|_| 0usize),
        unread_mark_read: Arc::new(|_, _| ()),
        session_close: Arc::new(|_| Ok(())),
        store: DeviceStore::Owned(conn.clone()),
        host_source: Box::new(|| serde_json::json!({ "host": { "name": "test-host" } })),
        message_source,
        path_source,
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
        home_source: Box::new(move || Some(blacklist_home.clone())),
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

fn get_req(uri: &str) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::builder()
        .method("GET")
        .uri(uri)
        .header("cookie", COOKIE)
        .body(axum::body::Body::empty())
        .unwrap()
}

fn action_req(body: &serde_json::Value) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::builder()
        .method("POST")
        .uri("/m/api/v1/zcode/actions")
        .header("cookie", COOKIE)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap()
}

async fn body_json(resp: axum::http::Response<axum::body::Body>) -> serde_json::Value {
    use http_body_util::BodyExt as _;
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).expect("JSON body")
}

async fn get_json(app: axum::Router, uri: &str) -> (axum::http::StatusCode, serde_json::Value) {
    let resp = app.oneshot(get_req(uri)).await.unwrap();
    let status = resp.status();
    (status, body_json(resp).await)
}

/// 读链一次完整断言（消息 + 文件预览 + 附件表），供两个控制失败态各跑一遍。
async fn assert_read_chain_intact(app: axum::Router) {
    let (status, body) = get_json(
        app.clone(),
        &format!("/m/api/v1/session-messages?agent_type=zcode&session_id={SESSION}"),
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::OK,
        "正文读链不得受控制失败影响"
    );
    let texts = body.to_string();
    assert!(
        texts.contains(USER_BODY) && texts.contains(ASSIST_BODY),
        "正文须含真实 user/assistant 消息，实际 {body}"
    );

    let (status, body) = get_json(
        app.clone(),
        &format!("/m/api/v1/file?session_id={SESSION}&path=preview.txt"),
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::OK,
        "文件预览不得受控制失败影响"
    );
    assert_eq!(body["content"], PREVIEW_BODY);
    assert_eq!(body["mime"], "text/plain");

    let (status, body) = get_json(
        app.clone(),
        &format!("/m/api/v1/session-files?agent_type=zcode&session_id={SESSION}"),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "附件表读链保持可用");
    assert!(body["files"].is_array(), "附件表形状不变，实际 {body}");
    assert!(
        body.to_string().contains("preview.txt"),
        "附件表须含合成附件路径，实际 {body}"
    );
}

/// 控制请求：断言 HTTP 状态 + 冻结错误码 + 上游零帧。
async fn assert_control_failure(
    app: axum::Router,
    official: &FakeOfficial,
    generation: u64,
    expected_status: axum::http::StatusCode,
    expected_code: &str,
    action_id: &str,
) {
    let body = serde_json::json!({
        "actionId": action_id,
        "action": {
            "kind": "send",
            "sessionId": SESSION,
            "generation": generation,
            "text": "控制面失败时的合成正文",
        },
    });
    let resp = app.oneshot(action_req(&body)).await.unwrap();
    assert_eq!(resp.status(), expected_status, "控制面必须如实失败");
    let json = body_json(resp).await;
    assert_eq!(json["error"], expected_code);
    assert_eq!(official.send_text_frames(), 0, "控制失败必须上游零帧");
}

#[tokio::test]
async fn full_chain_reads_are_independent_of_control_failure() {
    let conn = shared_conn();
    seed(&conn);
    let (_tmp, home) = read_fixture();
    let state = read_state(&conn, &home);
    let official = FakeOfficial::new(DeviceStore::Owned(conn.clone()), send_policy());
    let hub = Arc::new(ZcodeHub::with_parts(
        DeviceStore::Owned(conn.clone()),
        official.clone(),
        send_policy(),
    ));
    state.set_zcode_hub(hub.clone());
    let app = router(state);

    // ---- 态一：needs_auth（已保存链接、未授权）----
    assert_eq!(hub.connection().state, ConnectionState::NeedsAuth);
    let generation = hub.connection().generation;
    let (status, body) = get_json(app.clone(), "/m/api/v1/zcode/connection").await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(body["state"], "needs_auth", "连接面如实申报未授权");
    assert_control_failure(
        app.clone(),
        &official,
        generation,
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        codes::NEEDS_AUTH,
        "e2e-needs-auth",
    )
    .await;
    assert_read_chain_intact(app.clone()).await;

    // ---- 态二：offline（授权后宿主离线）----
    official.set_state(ConnectionState::Offline);
    hub.authorize(OAuthProvider::Bigmodel, Arc::new(|_url: String| Ok(())))
        .await
        .expect("假授权成功");
    let view = hub.connect().await.expect("假连接返回宿主真实（离线）状态");
    assert_eq!(view.state, ConnectionState::Offline);
    assert_eq!(hub.connection().state, ConnectionState::Offline);
    let generation = hub.connection().generation;
    assert_control_failure(
        app.clone(),
        &official,
        generation,
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        codes::OFFLINE,
        "e2e-offline",
    )
    .await;
    assert_read_chain_intact(app).await;
}

// ============================================================
// 用例 3：未知 control 的有界等待到期——零 stop、零 sendText
// ============================================================

/// 宿主从未宣告 `activeWorks`（control = None，不是空闲），真实 `CommandSender`
/// 以注入的短窗等待；窗尽必须零 stop 帧、零 sendText 帧、零账本行。
///
/// 生产窗是 `INITIAL_CONTROL_TIMEOUT`（10s）；本用例经 `doc(hidden)` 的
/// `with_initial_control_wait` 注入 60ms/5ms，不真实等待、不发起任何网络。
#[tokio::test]
async fn no_initial_control_timeout_zero_stop_and_no_trigger() {
    let store = DeviceStore::memory();
    let official = FakeOfficial::new(store.clone(), stop_policy());
    let target = official
        .bind_session(&TargetKey {
            session_id: SESSION.to_owned(),
        })
        .await
        .expect("假宿主必须能精确绑定合成会话");
    // 缺省投影：`ready=true` 但 `control=None` —— 未知 control，不得等同空闲。
    let sender = official
        .sender()
        .with_initial_control_wait(Duration::from_millis(60), Duration::from_millis(5));
    let err = sender
        .execute(
            &ActionContext {
                action_id: "e2e-stop-timeout".into(),
                device_id: DEVICE_ID.into(),
                device_name: "iPhone".into(),
            },
            &target,
            &ControlAction::Stop {
                session_id: SESSION.into(),
                generation: target.generation,
                expected_foreground_execution_id: "exec-synthetic-timeout".into(),
            },
        )
        .await
        .expect_err("未知 control 窗尽必须如实拒绝，绝不猜测目标");
    assert_eq!(err.code, codes::NOT_READY);
    // 零上游帧：既没有 stop 帧，也绝不合成 sendText 作为降级路径。
    assert_eq!(official.frame_count("stop"), 0, "窗尽必须零 stop 帧");
    assert_eq!(
        official.frame_count("sendText"),
        0,
        "绝不合成 sendText 作为打断降级路径"
    );
    assert!(official.frames().is_empty(), "窗尽不得留下任何上游帧");
    assert_eq!(ledger_rows(&store), 0, "未派发不得起账");
}

// ============================================================
// 用例 4：SSE lag → resync（重 GET）→ 投影未就绪零写
// ============================================================

/// 读一帧 SSE（2s 超时；流结束返回 None）。与既有 /zcode/events 测试同一读法。
async fn next_sse_frame(stream: &mut axum::body::BodyDataStream) -> Option<String> {
    use futures::StreamExt as _;
    let next = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("SSE 帧读取超时");
    next.map(|chunk| String::from_utf8(chunk.expect("SSE 帧读取失败").to_vec()).expect("UTF-8"))
}

fn sse_req() -> axum::http::Request<axum::body::Body> {
    get_req(&format!("/m/api/v1/zcode/events?sessionId={SESSION}"))
}

/// 生产链装配：已授权/已配对的 Hub（真实 `ZcodeHub` + 真实命令层 + `FakeOfficial`）。
async fn paired_hub(
    conn: &SharedConn,
    policy: CapabilityView,
) -> (Arc<ZcodeHub>, Arc<FakeOfficial>) {
    let official = FakeOfficial::new(DeviceStore::Owned(conn.clone()), policy.clone());
    let hub = Arc::new(ZcodeHub::with_parts(
        DeviceStore::Owned(conn.clone()),
        official.clone(),
        policy,
    ));
    hub.authorize(OAuthProvider::Bigmodel, Arc::new(|_url: String| Ok(())))
        .await
        .expect("假授权成功");
    hub.connect().await.expect("假连接返回宿主真实状态");
    assert_eq!(hub.connection().state, ConnectionState::Paired);
    (hub, official)
}

#[tokio::test]
async fn sse_lag_resyncs_before_next_write() {
    let conn = shared_conn();
    seed(&conn);
    let (_tmp, home) = read_fixture();
    let state = read_state(&conn, &home);
    let (hub, official) = paired_hub(&conn, send_policy()).await;
    state.set_zcode_hub(hub.clone());
    let app = router(state);

    // 首帧：连接 + 会话投影（ready=true）。
    let resp = app.clone().oneshot(sse_req()).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let mut stream = resp.into_body().into_data_stream();
    let first = next_sse_frame(&mut stream).await.expect("首帧不得缺失");
    assert!(
        first.starts_with("event: snapshot\n"),
        "首帧必须是 snapshot"
    );

    // 停止读帧制造 >64 条积压（每轮 disconnect+connect 至少 3 条广播事件）。
    for _ in 0..25 {
        hub.disconnect().await;
        hub.connect().await.expect("假重连成功");
    }
    let resync = next_sse_frame(&mut stream)
        .await
        .expect("lag 后必须有 resync 帧");
    assert!(
        resync.starts_with("event: resync\n"),
        "lag 必须发 resync（客户端据它重 GET），实际 {resync:?}"
    );
    assert!(
        !resync.contains("actionId"),
        "resync 帧绝不得重放动作，实际 {resync:?}"
    );

    // 客户端重 GET 自愈：连接/会话都以服务端当前代际为准。
    let (status, connection) = get_json(app.clone(), "/m/api/v1/zcode/connection").await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let generation = connection["generation"]
        .as_u64()
        .expect("连接视图必须带 generation");
    assert_eq!(connection["state"], "paired");
    let (status, session) = get_json(
        app.clone(),
        &format!("/m/api/v1/zcode/session?sessionId={SESSION}"),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "resync GET 必须可读");
    assert_eq!(session["generation"], generation, "投影只暴露 Hub 代际");
    assert_eq!(session["ready"], true);

    // 桥重订阅窗口：投影未就绪时绑定拒绝 → 动作零帧（客户端此时不得写）。
    official.set_bind_error(Some(ZcodeError {
        code: codes::NOT_READY.to_owned(),
    }));
    let (status, body) = post_action(
        app.clone(),
        serde_json::json!({
            "actionId": "e2e-lag-write-1",
            "action": {
                "kind": "send",
                "sessionId": SESSION,
                "generation": generation,
                "text": "投影未就绪时的合成正文",
            },
        }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CONFLICT);
    assert_eq!(body["error"], "not_ready");
    let writes_while_projection_not_ready = official.frames().len();
    assert_eq!(
        writes_while_projection_not_ready, 0,
        "投影未就绪期间不得有任何上游写帧"
    );

    // resync 完成、投影恢复后，同一代际的新动作才允许恰一帧。
    official.set_bind_error(None);
    let (status, body) = post_action(
        app,
        serde_json::json!({
            "actionId": "e2e-lag-write-2",
            "action": {
                "kind": "send",
                "sessionId": SESSION,
                "generation": generation,
                "text": "resync 完成后的合成正文",
            },
        }),
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::OK,
        "恢复后动作应受理：{body}"
    );
    assert_eq!(body["state"], "accepted");
    assert_eq!(official.send_text_frames(), 1, "恢复后恰一帧");
}

// ============================================================
// 用例 5：换链接（代际推进）——旧 pending / 旧动作零帧，新代际可用
// ============================================================

const SYNTH_URL_B: &str = "https://zcode.z.ai/remote/v4?sid=synthetic-e2e-0002&name=E2E2";
const OLD_INTERACTION: &str = "interaction-e2e-old-pending";

fn old_pending_card() -> PendingView {
    PendingView {
        interaction_id: OLD_INTERACTION.to_owned(),
        kind: PendingKind::UserInput,
        anchor_row_id: Some(7),
        tool_call_id: None,
        created_at: Some(1),
        summary: Some("合成旧卡".to_owned()),
        tool_name: None,
        schema_interaction: None,
        sensitive: false,
        auto_resolution: false,
        auto_resolution_state: None,
        questions: Vec::new(),
        options: Vec::new(),
    }
}

async fn post_action(
    app: axum::Router,
    body: serde_json::Value,
) -> (axum::http::StatusCode, serde_json::Value) {
    let resp = app.oneshot(action_req(&body)).await.unwrap();
    let status = resp.status();
    (status, body_json(resp).await)
}

#[tokio::test]
async fn link_change_invalidates_old_pending_and_actions() {
    let conn = shared_conn();
    seed(&conn);
    let (_tmp, home) = read_fixture();
    let state = read_state(&conn, &home);
    let (hub, official) = paired_hub(&conn, send_policy()).await;
    state.set_zcode_hub(hub.clone());
    let app = router(state);

    let old_generation = hub.connection().generation;
    // 旧链接下宿主投影里有一张旧卡：GET 能读到，作为「旧 pending」的在场证据。
    official.set_session(SessionView {
        session_id: SESSION.to_owned(),
        generation: 1,
        revision: Some(1),
        ready: true,
        pending: vec![old_pending_card()],
        control: None,
        selection: None,
        capabilities: Default::default(),
    });
    let (status, session) = get_json(
        app.clone(),
        &format!("/m/api/v1/zcode/session?sessionId={SESSION}"),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(session["pending"][0]["interactionId"], OLD_INTERACTION);

    // 旧链接下成功发送一次（证明旧代际当时可用）。
    let (status, body) = post_action(
        app.clone(),
        serde_json::json!({
            "actionId": "e2e-link-old-action",
            "action": {
                "kind": "send",
                "sessionId": SESSION,
                "generation": old_generation,
                "text": "旧链接下的合成正文",
            },
        }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    assert_eq!(official.send_text_frames(), 1);

    // 保存新链接 → Hub 推进代际（旧 relay/投影一律作废）。
    DeviceStore::Owned(conn.clone())
        .with(|c| set_config(c, SYNTH_URL_B))
        .expect("保存合成新链接");
    let changed = hub.config_changed().await;
    let new_generation = changed.generation;
    assert!(new_generation > old_generation, "换链接必须推进代际");
    assert_eq!(changed.state, ConnectionState::NeedsAuth);

    // 旧代际动作（同 actionId 重放）：stale_generation，零新帧、零新账本行。
    let (status, body) = post_action(
        app.clone(),
        serde_json::json!({
            "actionId": "e2e-link-old-action",
            "action": {
                "kind": "send",
                "sessionId": SESSION,
                "generation": old_generation,
                "text": "旧链接下的合成正文",
            },
        }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CONFLICT);
    assert_eq!(body["error"], codes::STALE_GENERATION);
    assert_eq!(official.send_text_frames(), 1, "旧代际重放零帧");
    assert_eq!(
        ledger_rows(&DeviceStore::Owned(conn.clone())),
        1,
        "旧代际重放零新行"
    );

    // 旧 pending 的卡动作同样在旧代际被拒：零帧、零 snooze 端口调用。
    let (status, body) = post_action(
        app.clone(),
        serde_json::json!({
            "actionId": "e2e-link-old-pending",
            "action": {
                "kind": "snooze",
                "sessionId": SESSION,
                "generation": old_generation,
                "interactionId": OLD_INTERACTION,
            },
        }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CONFLICT);
    assert_eq!(body["error"], codes::STALE_GENERATION);
    assert_eq!(official.frames().len(), 1, "旧代际卡动作零新帧");
    assert!(official.snoozes().is_empty(), "旧代际卡动作零上游调用");

    // 新链接下重新授权/连接：旧 actionId 走到账本层被判载荷冲突（代际入身份），
    // 零第二帧——绝不把旧回执当新代际的受理。
    hub.authorize(OAuthProvider::Bigmodel, Arc::new(|_url: String| Ok(())))
        .await
        .expect("新链接假授权成功");
    hub.connect().await.expect("新链接假连接成功");
    let (status, session) = get_json(
        app.clone(),
        &format!("/m/api/v1/zcode/session?sessionId={SESSION}"),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(session["generation"], new_generation, "投影即刻贴新代际");
    let (status, body) = post_action(
        app.clone(),
        serde_json::json!({
            "actionId": "e2e-link-old-action",
            "action": {
                "kind": "send",
                "sessionId": SESSION,
                "generation": new_generation,
                "text": "旧链接下的合成正文",
            },
        }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::CONFLICT);
    assert_eq!(body["error"], codes::ACTION_PAYLOAD_CONFLICT);
    assert_eq!(official.send_text_frames(), 1, "旧 actionId 不得跨代际复用");

    // 新宿主投影已没有旧卡：对旧 interaction 的新动作零帧（不猜、不重放）。
    official.set_session(SessionView {
        session_id: SESSION.to_owned(),
        generation: 1,
        revision: Some(2),
        ready: true,
        pending: Vec::new(),
        control: None,
        selection: None,
        capabilities: Default::default(),
    });
    let (status, body) = post_action(
        app.clone(),
        serde_json::json!({
            "actionId": "e2e-link-new-pending",
            "action": {
                "kind": "snooze",
                "sessionId": SESSION,
                "generation": new_generation,
                "interactionId": OLD_INTERACTION,
            },
        }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"], codes::UNKNOWN);
    assert!(official.snoozes().is_empty(), "旧卡不得被新代际再次操作");

    // 新代际 + 新 actionId：正常派发恰一帧。
    let (status, body) = post_action(
        app,
        serde_json::json!({
            "actionId": "e2e-link-new-action",
            "action": {
                "kind": "send",
                "sessionId": SESSION,
                "generation": new_generation,
                "text": "新链接下的合成正文",
            },
        }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    assert_eq!(body["generation"], new_generation);
    assert_eq!(official.send_text_frames(), 2, "新代际恰一帧");
}

// ============================================================
// 用例 6：吊销设备——动作零帧、零事件
// ============================================================

#[tokio::test]
async fn revoked_device_cannot_write_or_receive() {
    let conn = shared_conn();
    seed(&conn);
    let (_tmp, home) = read_fixture();
    let state = read_state(&conn, &home);
    let (hub, official) = paired_hub(&conn, send_policy()).await;
    state.set_zcode_hub(hub.clone());
    let registry = state.sse_registry.clone();
    let app = router(state);
    let generation = hub.connection().generation;
    let mut events = hub.subscribe();

    // 吊销前的活事件流：首帧 + snapshot 触发的本会话 Session 事件。
    let resp = app.clone().oneshot(sse_req()).await.unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let mut stream = resp.into_body().into_data_stream();
    let first = next_sse_frame(&mut stream).await.expect("首帧不得缺失");
    assert!(first.starts_with("event: snapshot\n"));
    let session_frame = next_sse_frame(&mut stream)
        .await
        .expect("snapshot 的会话读取必须广播一条本会话事件");
    assert!(session_frame.starts_with("event: session\n"));
    while events.try_recv().is_ok() {}
    // 首帧快照的会话读取是吊销前唯一一次绑定；此后动作不得再新增绑定。
    let binds_before = official.bind_calls().len();

    // 生产吊销序列：设备表失效 + 立即断开该设备全部 SSE。
    DeviceStore::Owned(conn.clone()).with(|c| revoke_device(c, DEVICE_ID).expect("吊销设备"));
    assert_eq!(
        registry.disconnect_device(DEVICE_ID),
        1,
        "吊销应关闭 1 条事件流"
    );
    assert!(
        next_sse_frame(&mut stream).await.is_none(),
        "吊销后事件流必须立即结束"
    );

    // 吊销后的动作：gate 403，绝不触达 Hub/官方端口（零帧、零绑定）。
    // gate 的 403 是空体（不泄露未配对/设备失效的区分），故只断言状态码。
    let resp = app
        .oneshot(action_req(&serde_json::json!({
            "actionId": "e2e-revoked-write",
            "action": {
                "kind": "send",
                "sessionId": SESSION,
                "generation": generation,
                "text": "吊销设备不得离机的合成正文",
            },
        })))
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::FORBIDDEN);
    assert_eq!(official.send_text_frames(), 0, "吊销后零上游帧");
    assert!(official.frames().is_empty());
    assert_eq!(
        official.bind_calls().len(),
        binds_before,
        "吊销后动作不得新增任何绑定"
    );
    // 零事件：吊销后既无动作回执，也无任何新广播。
    assert!(events.try_recv().is_err(), "吊销设备不得再收到任何控制事件");
}
