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

use common::zcode::{send_policy, FakeOfficial};
use multi_agents_manager_lib::adapter::AgentProcess;
use multi_agents_manager_lib::monitor::zcode_parser::{build_sessions, ZcodeRoots};
use multi_agents_manager_lib::remote::codex_ipc::CodexControlHub;
use multi_agents_manager_lib::remote::content::read_session_messages_with;
use multi_agents_manager_lib::remote::files::extract_file_paths_with;
use multi_agents_manager_lib::remote::pairing::{persist_device, DeviceStore, NewDevice};
use multi_agents_manager_lib::remote::server::{router, RemoteState};
use multi_agents_manager_lib::services::zcode_control::{
    codes, set_config, ActionContext, ActionState, CommandSender, ConnectionState, ControlAction,
    HubBackend, OAuthProvider, TargetKey, ZcodeHub,
};
use multi_agents_manager_lib::session::{ProcessForm, Session, SessionStatus, SessionsResponse};
use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
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
