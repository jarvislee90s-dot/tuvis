//! T8.1 生产链 e2e：真实 Hub/HTTP/命令层 × 进程内官方宿主假体。
//!
//! # 覆盖（逐条对应断言；未列出的面**不在**本文件覆盖内）
//!
//! 1. `two_devices_same_action_single_dispatch_across_disconnect`：**两台已配对设备**
//!    （各自 cookie 经配对 gate）共享同一个 Hub/账本。设备 A 先发一次；断链后
//!    设备 B 用同一 actionId 提交会命中**同一账本行**并被判 `action_payload_conflict`
//!    （T3 裁决：跨设备绝不泄漏回执），整个动作只产生一帧；A 的重放拿回逐字段相同
//!    的既有回执，零新帧。B 换自己的 actionId 才派发第二帧——设备隔离。
//! 2. `full_chain_reads_are_independent_of_control_failure`：Hub 处于 `needs_auth` /
//!    `offline` 时，移动读链（`/session-messages`、`/file`、`/session-files`）仍 200；
//!    控制面经 HTTP 如实 503 且上游零帧。
//! 3. `no_initial_control_timeout_zero_stop_and_no_trigger`：未知 control 的有界等待
//!    走**生产窗**（10s/50ms）与 tokio 虚拟时钟；用 `StopPort::session` 的轮询次数、
//!    逐次剩余预算与虚拟耗时证明等待真的到期，零 stop / 零 sendText / 零账本行。
//! 4. `sse_lag_resyncs_before_next_write`：SSE lag → resync 帧；resync GET 完成之前
//!    零上游写帧，且首个动作 POST 在请求日志中晚于两个 resync GET 的完成；投影
//!    未就绪期间写入被拒且零帧，恢复后才恰一帧。
//! 5. 正向卡动作/档位与终态：`answer` / `permission` / `selection` 经 HTTP 各恰一帧，
//!    宿主撤卡/生效后回执升级为 `confirmed`，审计同时有 `started` 与终态行；stop 的
//!    撤卡确认单独覆盖。审计行断言通道/工具/设备身份且摘要不含正文。
//!
//! # 已知边界（如实申报，勿把本文件读成 wire 覆盖）
//!
//! - 假体停在 `HubBackend` / `CommandPort` 边界；`RelayClient ↔ BridgeManager ↔
//!   CommandSender` 的生产组合与 relay/codec/WS 的 wire 形态由 T1/T2 的 fake 单独
//!   覆盖，本文件不覆盖；
//! - `Frame` 是命令层信封记录（未离机）；
//! - 换链接/吊销用例复刻生产编排序列（`set_config` + `hub.config_changed()` /
//!   `revoke_device` + `disconnect_device`），不是 `remote_revoke_device` 的端到端锁。
//!
//! # 夹具纪律
//!
//! 全部合成值与 tempdir/内存库：不读真实 `~/.zcode`、`~/.tuvis`，不发起任何官方
//! 网络请求，不写入任何真实账号凭据。进程内官方宿主的偏差申报见
//! `tests/common/zcode.rs` 模块文档。

mod common;

use common::zcode::{all_policy, send_policy, stop_policy, FakeOfficial};
use multi_agents_manager_lib::adapter::AgentProcess;
use multi_agents_manager_lib::monitor::zcode_parser::{build_sessions, ZcodeRoots};
use multi_agents_manager_lib::remote::codex_ipc::CodexControlHub;
use multi_agents_manager_lib::remote::content::read_session_messages_with;
use multi_agents_manager_lib::remote::files::extract_file_paths_with;
use multi_agents_manager_lib::remote::pairing::{
    persist_device, revoke_device, DeviceStore, NewDevice,
};
use multi_agents_manager_lib::remote::server::{router, RemoteState};
use multi_agents_manager_lib::services::zcode_control::command::INITIAL_CONTROL_TIMEOUT;
use multi_agents_manager_lib::services::zcode_control::{
    codes, set_config, ActionContext, ActiveWork, CapabilityView, ConnectionState, ControlAction,
    ControlView, HubBackend, ModelChoice, OAuthProvider, PendingKind, PendingView,
    PermissionOptionView, QuestionOption, QuestionView, SelectionView, SessionView, TargetKey,
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
const COOKIE_B: &str = "mam_device=dev-e2e-ipad";
const DEVICE_ID_B: &str = "dev-e2e-ipad";
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

/// 指定 actionId 的账本归属设备（证明跨设备同 id 没有改写既有行）。
fn action_device(store: &DeviceStore, action_id: &str) -> String {
    store.with(|c| {
        c.query_row(
            "SELECT device_id FROM zcode_actions WHERE action_id = ?1",
            [action_id],
            |r| r.get(0),
        )
        .expect("zcode_actions 行必须存在")
    })
}

/// 指定动作类别 + 结果的审计行数（`zcode_send` / `zcode_answer` / …）。
fn audit_rows(store: &DeviceStore, action: &str, result: &str) -> i64 {
    store.with(|c| {
        c.query_row(
            "SELECT COUNT(*) FROM write_audit WHERE action = ?1 AND result = ?2",
            rusqlite::params![action, result],
            |r| r.get(0),
        )
        .expect("write_audit 可读")
    })
}

/// 指定动作类别的全部审计行 `(device_id, agent_type, channel, summary, result)`。
fn audit_detail(
    store: &DeviceStore,
    action: &str,
) -> Vec<(String, String, String, String, String)> {
    store.with(|c| {
        let mut stmt = c
            .prepare(
                "SELECT device_id, agent_type, channel, summary, result
                 FROM write_audit WHERE action = ?1 ORDER BY id",
            )
            .expect("write_audit 可读");
        let rows = stmt
            .query_map([action], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .expect("write_audit 可读");
        rows.map(|row| row.expect("write_audit 行可读")).collect()
    })
}

/// 已起账的发送动作数（`started` 审计只在预留成功时写；重放不得重复起账）。
fn started_audits(store: &DeviceStore) -> i64 {
    audit_rows(store, "zcode_send", "started")
}

// ============================================================
// 用例 1：同 actionId 跨断链恰一帧
// ============================================================

#[tokio::test]
async fn two_devices_same_action_single_dispatch_across_disconnect() {
    // 两台**真正不同**的已配对设备共享同一个 Hub / 账本（设备身份只来自 cookie
    // 与配对 DB，不来自请求体）。
    let conn = shared_conn();
    seed(&conn);
    seed_device(&conn, DEVICE_ID_B, "iPad");
    let (_tmp, home) = read_fixture();
    let state = read_state(&conn, &home);
    let (hub, official) = paired_hub(&conn, send_policy()).await;
    state.set_zcode_hub(hub.clone());
    let app = router(state);
    let generation = hub.connection().generation;
    let store = DeviceStore::Owned(conn.clone());

    let send_body = serde_json::json!({
        "actionId": ACTION_ID,
        "action": {
            "kind": "send",
            "sessionId": SESSION,
            "generation": generation,
            "text": SEND_TEXT,
        },
    });

    // 设备 A（cookie=dev-e2e）第一次点击：唯一一次真正派发。
    let (status, first) = post_action_as(app.clone(), COOKIE, send_body.clone()).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{first}");
    assert_eq!(first["state"], "accepted");
    assert_eq!(first["actionId"], ACTION_ID);
    assert_eq!(official.send_text_frames(), 1, "设备 A 只允许一帧");
    let frame = official.frames().remove(0);
    assert_eq!(frame.envelope_type(), Some("sendText"));
    assert_eq!(frame.method, "sendConversationCommandV4");
    assert_eq!(frame.text(), Some(SEND_TEXT), "正文必须原样离机");
    assert_eq!(frame.envelope()["payload"]["text"], SEND_TEXT);
    assert_eq!(ledger_rows(&store), 1, "一次派发恰一行");
    assert_eq!(
        action_device(&store, ACTION_ID),
        DEVICE_ID,
        "账本归属设备 A"
    );
    assert_eq!(started_audits(&store), 1);

    // 两次提交之间官方链路断开：任何新动作都不可能派发（假体确证这一点）。
    official.drop_link();

    // 设备 B（cookie=dev-e2e-ipad）复用**同一 actionId**：命中同一账本行后按
    // T3 身份口径拒绝（跨设备绝不泄漏回执），零第二帧、零新行。
    let (status, conflict) = post_action_as(app.clone(), COOKIE_B, send_body.clone()).await;
    assert_eq!(status, axum::http::StatusCode::CONFLICT);
    assert_eq!(conflict["error"], codes::ACTION_PAYLOAD_CONFLICT);
    assert!(
        conflict.get("actionId").is_none() && conflict.get("state").is_none(),
        "冲突响应不得夹带设备 A 的回执字段，实际 {conflict}"
    );
    assert_eq!(official.send_text_frames(), 1, "同 id 跨设备不得产生第二帧");
    assert_eq!(ledger_rows(&store), 1, "冲突不得新增账本行");
    assert_eq!(
        action_device(&store, ACTION_ID),
        DEVICE_ID,
        "跨设备冲突不得改写既有行归属"
    );
    assert_eq!(started_audits(&store), 1, "冲突不得再次起账");

    // 设备 A 断链后重放同一 actionId：命中**同一存储动作**，回执逐字段相同、零新帧。
    let (status, replay) = post_action_as(app.clone(), COOKIE, send_body.clone()).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{replay}");
    assert_eq!(
        replay, first,
        "同一设备重放必须拿回逐字段相同的既有回执（含 actionId/session/generation/state）"
    );
    assert_eq!(official.send_text_frames(), 1, "全生命周期只允许一帧");
    assert_eq!(ledger_rows(&store), 1);

    // 设备 B 换**自己的 actionId**（断链期间）零帧：链路断开对新动作同样生效。
    let offline_probe = serde_json::json!({
        "actionId": "e2e-action-probe-b",
        "action": {
            "kind": "send",
            "sessionId": SESSION,
            "generation": generation,
            "text": "断链后设备 B 的合成正文",
        },
    });
    let (status, probe) = post_action_as(app.clone(), COOKIE_B, offline_probe.clone()).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{probe}");
    assert_eq!(probe["state"], "unknown", "断链后的新写只能保守 unknown");
    assert_eq!(official.send_text_frames(), 1, "断链后零帧");

    // 链路恢复后设备 B 用自己的 actionId：独立派发第二帧，账本行归属设备 B。
    official.restore_link();
    let b_body = serde_json::json!({
        "actionId": "e2e-action-0002",
        "action": {
            "kind": "send",
            "sessionId": SESSION,
            "generation": generation,
            "text": "设备 B 的合成正文",
        },
    });
    let (status, b_receipt) = post_action_as(app.clone(), COOKIE_B, b_body).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{b_receipt}");
    assert_eq!(b_receipt["state"], "accepted");
    assert_eq!(official.send_text_frames(), 2, "设备 B 恰一帧");
    assert_eq!(ledger_rows(&store), 3, "1 次发送 + 1 次探针 + 1 次设备 B");
    assert_eq!(
        action_device(&store, "e2e-action-0002"),
        DEVICE_ID_B,
        "设备 B 的动作必须归属设备 B"
    );
    assert_eq!(started_audits(&store), 3);

    // 反向隔离：设备 A 复用设备 B 的 actionId → 冲突且零第三帧（B 的回执不泄漏）。
    let reverse_body = serde_json::json!({
        "actionId": "e2e-action-0002",
        "action": {
            "kind": "send",
            "sessionId": SESSION,
            "generation": generation,
            "text": "设备 B 的合成正文",
        },
    });
    let (status, reverse) = post_action_as(app, COOKIE, reverse_body).await;
    assert_eq!(status, axum::http::StatusCode::CONFLICT);
    assert_eq!(reverse["error"], codes::ACTION_PAYLOAD_CONFLICT);
    assert!(
        reverse.get("actionId").is_none() && reverse.get("state").is_none(),
        "冲突响应不得夹带设备 B 的回执字段，实际 {reverse}"
    );
    assert_eq!(official.send_text_frames(), 2, "跨设备冲突零第三帧");
    assert_eq!(ledger_rows(&store), 3);
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

/// 追加一台已配对设备（不同 ua/指纹，绝不覆盖第一台）。
fn seed_device(conn: &SharedConn, id: &str, name: &str) {
    DeviceStore::Owned(conn.clone())
        .with(|c| {
            persist_device(
                c,
                &NewDevice {
                    id: id.to_owned(),
                    name: name.into(),
                    ua: format!("ua-{id}"),
                    origin_ip: format!("127.0.0.1#{id}"),
                    via: "lan".into(),
                    paired_at: chrono::Utc::now().timestamp_millis(),
                },
            )
        })
        .expect("seed second device");
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
    action_req_as(COOKIE, body)
}

/// 指定配对设备 cookie 的动作请求（两种设备身份共享同一 Router/账本）。
fn action_req_as(cookie: &str, body: &serde_json::Value) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::builder()
        .method("POST")
        .uri("/m/api/v1/zcode/actions")
        .header("cookie", cookie)
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
/// 以**生产窗**（`INITIAL_CONTROL_TIMEOUT` 10s / `INITIAL_CONTROL_POLL` 50ms）等待；
/// 窗尽必须零 stop 帧、零 sendText 帧、零账本行。
///
/// 虚拟时钟（`start_paused`）让 10s 窗瞬时到期：既不真实 sleep，又保留生产常量
/// 本身；断言 `StopPort::session` 的轮询次数、逐次剩余预算与虚拟耗时，证明等待
/// 真的发生过（旧用例只看零帧，等待被改成「立刻 not_ready」也能绿）。
#[tokio::test(start_paused = true)]
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
    let sender = official.sender();

    let started = tokio::time::Instant::now();
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
    let elapsed = started.elapsed();

    assert_eq!(err.code, codes::NOT_READY);
    // 有界等待真的发生：生产窗 10s / 50ms ⇒ 至少 190 次轮询（留 5% 余量）。
    let reads = official.stop_reads();
    assert!(
        reads.len() >= 190,
        "未知 control 必须轮询到生产窗尽，实际读取 {} 次",
        reads.len()
    );
    // 单次读只拿剩余预算：首读 = 总窗，随后严格收紧（绝不每次都给满窗）。
    assert!(
        reads[0] >= INITIAL_CONTROL_TIMEOUT - Duration::from_millis(1)
            && reads[0] <= INITIAL_CONTROL_TIMEOUT,
        "首读预算必须是生产总窗，实际 {:?}",
        reads[0]
    );
    let last = *reads.last().expect("至少一次读取");
    assert!(
        last < reads[0] && !last.is_zero(),
        "末次预算必须是剩余期限且非零，实际 {last:?}"
    );
    // 端到端 deadline：虚拟耗时不得早于总窗，也不得拖过总窗 + 一个轮询间隔。
    assert!(
        elapsed >= INITIAL_CONTROL_TIMEOUT,
        "窗尽前必须等满总窗，实际 {elapsed:?}"
    );
    assert!(
        elapsed < INITIAL_CONTROL_TIMEOUT + Duration::from_millis(100),
        "等待不得越过总窗，实际 {elapsed:?}"
    );

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

/// 请求完成顺序日志（`方法 路径 start|end 状态码`）。
///
/// 用于证明「resync GET **完成**之后才出现动作 POST」，而不是只靠测试代码的
/// 书写顺序自证。`end` 在响应头返回后记录（动作路由是缓冲响应，等价于处理完成）。
#[derive(Clone, Default)]
struct RequestLog(Arc<Mutex<Vec<String>>>);

impl RequestLog {
    fn entries(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }

    /// 首条满足前缀的日志下标。
    fn first(&self, needle: &str) -> Option<usize> {
        self.entries().iter().position(|entry| entry == needle)
    }
}

async fn record_requests(
    axum::extract::State(log): axum::extract::State<RequestLog>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();
    log.0.lock().unwrap().push(format!("{method} {path} start"));
    let resp = next.run(req).await;
    log.0
        .lock()
        .unwrap()
        .push(format!("{method} {path} end {}", resp.status().as_u16()));
    resp
}

#[tokio::test]
async fn sse_lag_resyncs_before_next_write() {
    let conn = shared_conn();
    seed(&conn);
    let (_tmp, home) = read_fixture();
    let state = read_state(&conn, &home);
    let (hub, official) = paired_hub(&conn, send_policy()).await;
    state.set_zcode_hub(hub.clone());
    let log = RequestLog::default();
    let app = router(state).layer(axum::middleware::from_fn_with_state(
        log.clone(),
        record_requests,
    ));

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
    // lag 之后、resync GET 完成之前：不得有任何写帧，也不得出现动作 POST
    // （「先纠偏再放写」；旧断言只数最终帧数，无法发现顺序颠倒）。
    assert!(official.frames().is_empty(), "resync 前不得有任何上游写帧");
    assert!(
        !log.entries()
            .iter()
            .any(|entry| entry.starts_with("POST /m/api/v1/zcode/actions")),
        "resync GET 完成前不得出现动作 POST，实际 {:?}",
        log.entries()
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

    // 请求日志证明两个 resync GET **已完成**（end 条目），且排在任何动作 POST 之前。
    let connection_end = log
        .first("GET /m/api/v1/zcode/connection end 200")
        .unwrap_or_else(|| panic!("resync 连接 GET 未完成：{:?}", log.entries()));
    let session_end = log
        .first("GET /m/api/v1/zcode/session end 200")
        .unwrap_or_else(|| panic!("resync 会话 GET 未完成：{:?}", log.entries()));
    // 两个 GET 都完成后才允许写：此刻仍无任何上游帧。
    assert!(
        official.frames().is_empty(),
        "resync GET 完成时不得已有上游写帧"
    );

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
    let not_ready_start = log
        .first("POST /m/api/v1/zcode/actions start")
        .unwrap_or_else(|| panic!("动作 POST 尚未发生：{:?}", log.entries()));
    assert!(
        not_ready_start > connection_end && not_ready_start > session_end,
        "resync GET 必须在首个动作 POST 之前完成（connection_end={connection_end}, \
         session_end={session_end}, post_start={not_ready_start}）：{:?}",
        log.entries()
    );
    let not_ready_end = log
        .first("POST /m/api/v1/zcode/actions end 409")
        .unwrap_or_else(|| panic!("未就绪写入必须以 409 结束：{:?}", log.entries()));
    assert!(
        not_ready_end > session_end,
        "未就绪写入的完成也必须晚于 resync GET：{:?}",
        log.entries()
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
    let accepted_end = log
        .first("POST /m/api/v1/zcode/actions end 200")
        .unwrap_or_else(|| panic!("受理写入必须以 200 结束：{:?}", log.entries()));
    assert!(
        accepted_end > not_ready_end,
        "受理写入必须发生在未就绪拒绝之后：{:?}",
        log.entries()
    );
}

// ============================================================
// 用例 5：正向卡动作 / 档位：恰一帧、撤卡或生效后 confirmed、终态审计
// ============================================================

const ANSWER_INTERACTION: &str = "interaction-e2e-answer";
const PERMISSION_INTERACTION: &str = "interaction-e2e-permission";
const STOP_EXECUTION: &str = "exec-e2e-stop";
const ANSWER_QUESTION: &str = "合成问题：选哪个？";
const PERMISSION_OPTION: &str = "host-allow-once-1";
const SELECTION_MODEL: &str = "glm-5";
const SELECTION_THOUGHT: &str = "high";

fn question_card() -> PendingView {
    PendingView {
        interaction_id: ANSWER_INTERACTION.to_owned(),
        kind: PendingKind::UserInput,
        anchor_row_id: Some(11),
        tool_call_id: Some("tool-e2e-answer".to_owned()),
        created_at: Some(1),
        summary: Some("合成问答卡".to_owned()),
        tool_name: Some("AskUserQuestion".to_owned()),
        schema_interaction: Some("AskUserQuestion".to_owned()),
        sensitive: false,
        auto_resolution: false,
        auto_resolution_state: None,
        questions: vec![QuestionView {
            header: "合成题头".to_owned(),
            question_text: ANSWER_QUESTION.to_owned(),
            options: vec![QuestionOption {
                value: "opt-a".to_owned(),
                label: "选项 A".to_owned(),
                description: None,
                preview: None,
            }],
            multi_select: false,
            free_text_allowed: false,
        }],
        options: Vec::new(),
    }
}

fn permission_card() -> PendingView {
    PendingView {
        interaction_id: PERMISSION_INTERACTION.to_owned(),
        kind: PendingKind::Permission,
        anchor_row_id: Some(12),
        tool_call_id: Some("tool-e2e-permission".to_owned()),
        created_at: Some(2),
        summary: Some("合成审批卡".to_owned()),
        tool_name: Some("Bash".to_owned()),
        schema_interaction: None,
        sensitive: false,
        auto_resolution: false,
        auto_resolution_state: None,
        questions: Vec::new(),
        options: vec![PermissionOptionView {
            option_id: PERMISSION_OPTION.to_owned(),
            label: "允许一次".to_owned(),
            kind: "allowOnce".to_owned(),
        }],
    }
}

fn selection_view() -> SelectionView {
    SelectionView {
        provider: "bigmodel".to_owned(),
        model: SELECTION_MODEL.to_owned(),
        thought: Some(SELECTION_THOUGHT.to_owned()),
        models: vec![ModelChoice {
            model_id: SELECTION_MODEL.to_owned(),
            thought_levels: vec!["low".to_owned(), SELECTION_THOUGHT.to_owned()],
        }],
    }
}

fn session_with_cards() -> SessionView {
    SessionView {
        session_id: SESSION.to_owned(),
        generation: 1,
        revision: Some(4),
        ready: true,
        pending: vec![question_card(), permission_card()],
        control: Some(ControlView {
            active_works: vec![ActiveWork {
                kind: "primaryTurn".to_owned(),
                foreground_execution_id: Some(STOP_EXECUTION.to_owned()),
            }],
        }),
        selection: None,
        capabilities: all_policy(),
    }
}

#[tokio::test]
async fn answer_permission_selection_confirm_and_audit_terminal_rows() {
    let conn = shared_conn();
    seed(&conn);
    let (_tmp, home) = read_fixture();
    let state = read_state(&conn, &home);
    let (hub, official) = paired_hub(&conn, all_policy()).await;
    state.set_zcode_hub(hub.clone());
    let app = router(state);
    let generation = hub.connection().generation;
    let store = DeviceStore::Owned(conn.clone());
    official.set_session(session_with_cards());
    // 宿主撤卡会发生：等待端口返回 true → accepted 因果升级 confirmed。
    official.set_host_cleared(true);

    // ---- 问答应答：恰一帧 resolveInteraction，宿主撤卡后 confirmed ----
    let (status, body) = post_action(
        app.clone(),
        serde_json::json!({
            "actionId": "e2e-answer-1",
            "action": {
                "kind": "answer",
                "sessionId": SESSION,
                "generation": generation,
                "interactionId": ANSWER_INTERACTION,
                "answers": [{
                    "questionText": ANSWER_QUESTION,
                    "selectedValues": ["opt-a"],
                    "freeText": null,
                }],
            },
        }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    assert_eq!(body["state"], "confirmed", "宿主撤卡后必须升级为 confirmed");
    assert_eq!(official.frame_count("resolveInteraction"), 1);
    assert!(official.snoozes().is_empty(), "非自动结束卡不得 snooze");
    assert_eq!(official.wait_host_cleared_calls(), 1);
    assert_eq!(audit_rows(&store, "zcode_answer", "started"), 1);
    assert_eq!(audit_rows(&store, "zcode_answer", "accepted"), 1);
    assert_eq!(audit_rows(&store, "zcode_answer", "confirmed"), 1);

    // ---- 审批既有卡：optionId 原值恰好一帧，宿主撤卡后 confirmed ----
    let (status, body) = post_action(
        app.clone(),
        serde_json::json!({
            "actionId": "e2e-permission-1",
            "action": {
                "kind": "permission",
                "sessionId": SESSION,
                "generation": generation,
                "interactionId": PERMISSION_INTERACTION,
                "optionId": PERMISSION_OPTION,
            },
        }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    assert_eq!(body["state"], "confirmed");
    assert_eq!(official.frame_count("resolveInteraction"), 2);
    let permission_frame = official
        .frames()
        .into_iter()
        .find(|frame| frame.envelope()["payload"]["interactionId"] == PERMISSION_INTERACTION)
        .expect("审批必须留下 resolveInteraction 帧");
    assert_eq!(
        permission_frame.envelope()["payload"]["answer"]["optionId"],
        PERMISSION_OPTION,
        "必须回传宿主原值"
    );
    assert_eq!(official.wait_host_cleared_calls(), 2);
    assert_eq!(audit_rows(&store, "zcode_permission", "started"), 1);
    assert_eq!(audit_rows(&store, "zcode_permission", "confirmed"), 1);

    // ---- 档位切换：恰一帧 switchModelConfig，宿主生效后 confirmed ----
    official.set_selection(selection_view());
    let (status, body) = post_action(
        app.clone(),
        serde_json::json!({
            "actionId": "e2e-selection-1",
            "action": {
                "kind": "selection",
                "sessionId": SESSION,
                "generation": generation,
                "model": SELECTION_MODEL,
                "thought": SELECTION_THOUGHT,
            },
        }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    assert_eq!(body["state"], "confirmed");
    assert_eq!(official.frame_count("switchModelConfig"), 1);
    assert_eq!(official.wait_selection_calls(), 1);
    assert_eq!(audit_rows(&store, "zcode_selection", "started"), 1);
    assert_eq!(audit_rows(&store, "zcode_selection", "confirmed"), 1);

    // 三个动作合计恰三帧，绝无 sendText 合成路径。
    assert_eq!(official.frames().len(), 3);
    assert_eq!(official.send_text_frames(), 0);
    assert_eq!(ledger_rows(&store), 3);

    // 终态审计：每类都有 started + accepted + confirmed 三行，身份/通道精确。
    for action in ["zcode_answer", "zcode_permission", "zcode_selection"] {
        let rows = audit_detail(&store, action);
        assert_eq!(rows.len(), 3, "{action} 审计行数：{rows:?}");
        let results: Vec<&str> = rows.iter().map(|row| row.4.as_str()).collect();
        assert_eq!(results, vec!["started", "accepted", "confirmed"]);
        for (device, agent, channel, summary, _) in &rows {
            assert_eq!(device, DEVICE_ID, "审计设备身份必须来自配对门");
            assert_eq!(agent, "zcode");
            assert_eq!(channel, "zcode_official");
            for forbidden in [ANSWER_QUESTION, "opt-a", PERMISSION_OPTION, SELECTION_MODEL] {
                assert!(
                    !summary.contains(forbidden),
                    "{action} 摘要不得夹带正文/答案/选项原值：{summary}"
                );
            }
        }
    }
}

#[tokio::test]
async fn stop_confirms_after_host_clears_execution() {
    let conn = shared_conn();
    seed(&conn);
    let (_tmp, home) = read_fixture();
    let state = read_state(&conn, &home);
    let (hub, official) = paired_hub(&conn, stop_policy()).await;
    state.set_zcode_hub(hub.clone());
    let app = router(state);
    let generation = hub.connection().generation;
    let store = DeviceStore::Owned(conn.clone());
    official.set_session(SessionView {
        selection: None,
        pending: Vec::new(),
        capabilities: stop_policy(),
        ..session_with_cards()
    });
    // 宿主已撤掉目标执行（stop 终态证据）：等待端口返回 true → confirmed。
    official.set_stop_cleared(true);

    let (status, body) = post_action(
        app,
        serde_json::json!({
            "actionId": "e2e-stop-confirm",
            "action": {
                "kind": "stop",
                "sessionId": SESSION,
                "generation": generation,
                "expectedForegroundExecutionId": STOP_EXECUTION,
            },
        }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    assert_eq!(body["state"], "confirmed", "宿主撤掉执行后必须 confirmed");
    assert_eq!(official.frame_count("stop"), 1, "恰一帧 stop");
    assert_eq!(official.send_text_frames(), 0, "绝不合成 sendText");
    let stop_frame = official.frames().remove(0);
    assert_eq!(
        stop_frame.envelope()["payload"]["expectedForegroundExecutionId"],
        STOP_EXECUTION
    );
    assert_eq!(official.wait_stop_cleared_calls(), 1);

    let rows = audit_detail(&store, "zcode_stop");
    assert_eq!(rows.len(), 3, "started + accepted + confirmed：{rows:?}");
    assert_eq!(
        rows.iter().map(|row| row.4.as_str()).collect::<Vec<_>>(),
        vec!["started", "accepted", "confirmed"]
    );
    for (device, agent, channel, summary, _) in &rows {
        assert_eq!(device, DEVICE_ID);
        assert_eq!(agent, "zcode");
        assert_eq!(channel, "zcode_official");
        assert!(!summary.contains(STOP_EXECUTION), "守卫 id 不得入摘要");
    }
}

// ============================================================
// 用例 6：换链接（代际推进）——旧 pending / 旧动作零帧，新代际可用
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
    post_action_as(app, COOKIE, body).await
}

/// 以指定设备 cookie 提交动作（同一 Hub/账本；设备身份来自配对门）。
async fn post_action_as(
    app: axum::Router,
    cookie: &str,
    body: serde_json::Value,
) -> (axum::http::StatusCode, serde_json::Value) {
    let resp = app.oneshot(action_req_as(cookie, &body)).await.unwrap();
    let status = resp.status();
    (status, body_json(resp).await)
}

#[tokio::test]
async fn link_change_invalidates_old_pending_and_actions() {
    let conn = shared_conn();
    seed(&conn);
    let (_tmp, home) = read_fixture();
    let state = read_state(&conn, &home);
    // 用全开策略让卡动作走到宿主题型/旧卡判定；本用例证明换链接作废旧 pending，
    // 不是证明能力闸（能力闸由 #5/#4 的专门用例覆盖）。
    let (hub, official) = paired_hub(&conn, all_policy()).await;
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

    // 保存新链接 → Hub 推进代际（旧 relay/投影作废），但账号 JWT 保留。
    DeviceStore::Owned(conn.clone())
        .with(|c| set_config(c, SYNTH_URL_B))
        .expect("保存合成新链接");
    let changed = hub.config_changed().await;
    let new_generation = changed.generation;
    assert!(new_generation > old_generation, "换链接必须推进代际");
    assert_eq!(
        changed.state,
        ConnectionState::Offline,
        "换链接保留账号（只需要重连，不需重新授权）"
    );
    assert_eq!(official.authorize_calls(), 1, "换链接不得重新 OAuth");

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

    // 新链接下直接连接（复用保留的账号 JWT，不再 OAuth）：旧 actionId 走到账本层
    // 被判载荷冲突（代际入身份），零第二帧——绝不把旧回执当新代际的受理。
    hub.connect().await.expect("新链接假连接成功");
    assert_eq!(official.authorize_calls(), 1, "新链接不得重新授权");
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
// 用例 7：吊销设备——动作零帧、零事件
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
