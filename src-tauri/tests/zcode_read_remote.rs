//! A6 集成测试：ZCode 双形态主会话 id 与读取恢复合同（设计 §6）。
//!
//! 覆盖：合法主会话 id 域同时接受 `sess_` + 严格 8-4-4-4-12 hex UUID 与
//! `session_` + 32 ASCII hex（hex 大小写兼容，保留原 ID）；新旧主会话在 tempdir
//! 两库 fixture 中共存出卡、ID 不重命名不归一不合并；parent 非空／子代理前缀／
//! 归档／deleted／窗口外逐项排除、空串 parent 沿用旧口径、无宿主不出卡；
//! 合成新 ID 经快照 → /session-messages／/session-files 正文与附件可读，
//! 合成内容过滤与状态输入不变，隐藏／未读仍按各自门。
//! A7 追加：正文与附件引用的同源锚点——附件序号 = 所属消息投影后的最终显示 seq
//! （非原 DB 窗口 ordinal），file-only／被过滤消息保留文件、lastSeq 取中性 0 占位。
//!
//! 零真实副作用纪律：全部 SQLite 一律 tempdir／内存库 fixture，只用合成 id 与
//! 合成正文，绝不接触真实 ~/.zcode、~/.tuvis；本测试不实现任何发送／审批能力。

use multi_agents_manager_lib::adapter::AgentProcess;
use multi_agents_manager_lib::monitor::zcode_parser::{
    build_sessions, get_zcode_sessions, is_valid_session_id, ZcodeRoots,
};
use multi_agents_manager_lib::remote::codex_ipc::CodexControlHub;
use multi_agents_manager_lib::remote::content::read_session_messages_with;
use multi_agents_manager_lib::remote::files::{
    extract_file_paths_with, read_file_safe, FileOrigin,
};
use multi_agents_manager_lib::remote::pairing::{persist_device, DeviceStore, NewDevice};
use multi_agents_manager_lib::remote::server::{router, RemoteState};
use multi_agents_manager_lib::session::{ProcessForm, Session, SessionStatus, SessionsResponse};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tower::ServiceExt as _;

type SharedConn = Arc<Mutex<Connection>>;

const COOKIE: &str = "mam_device=dev-1";

// ===== 合成夹具 id（严禁真实用户会话）=====

/// 旧形态：sess_ + 严格 UUID（8-4-4-4-12 hex）
const OLD_MAIN: &str = "sess_0f1e2d3c-4b5a-4948-8276-9a0b8c7d6e5f";
/// 新形态：session_ + 32 hex —— 与 OLD_MAIN 同 32 位 hex 芯（去横杠形态）。
/// 两者必须各自独立出卡：任何「去横杠／归一化／合并为同一键」的实现都会让它们
/// 撞成一个键，被本 fixture 的 distinct 断言抓住
const NEW_MAIN: &str = "session_0f1e2d3c4b5a494882769a0b8c7d6e5f";
/// 新形态 + 大写 hex（大小写兼容面，且出卡 id 必须原样保留大小写）
const NEW_MAIN_UPPER: &str = "session_7F1E2D3C4B5A494882769A0B8C7D6E5F";

/// 固定「当前时刻」（毫秒）：parser 时钟全部注入，不依赖墙钟
const NOW: i64 = 1_771_000_000_000;
const ONE_HOUR_MS: i64 = 3_600_000;

// ===== fixture 构建（与 monitor::zcode_parser 测试模块同构；一律 tempdir）=====

fn build_cli_db(path: &Path) -> Connection {
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

fn build_tasks_db(path: &Path) -> Connection {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE tasks (
            task_id TEXT PRIMARY KEY, title TEXT, task_status TEXT,
            workspace_path TEXT, archived INTEGER, deleted INTEGER,
            pinned INTEGER, updated_at INTEGER,
            unread_at INTEGER, last_unread_at INTEGER
         );",
    )
    .unwrap();
    conn
}

#[allow(clippy::too_many_arguments)]
fn insert_session(
    conn: &Connection,
    id: &str,
    parent_id: Option<&str>,
    dir: &str,
    task_type: &str,
    title: Option<&str>,
    time_updated: i64,
) {
    conn.execute(
        "INSERT INTO session (id, parent_id, directory, task_type, title, time_updated)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![id, parent_id, dir, task_type, title, time_updated],
    )
    .unwrap();
}

fn insert_message(
    conn: &Connection,
    id: &str,
    session_id: &str,
    seq: i64,
    time_created: i64,
    data: &str,
) {
    conn.execute(
        "INSERT INTO message (id, session_id, sequence, time_created, data)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![id, session_id, seq, time_created, data],
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

fn insert_task(conn: &Connection, task_id: &str, title: Option<&str>, archived: i64, deleted: i64) {
    conn.execute(
        "INSERT INTO tasks (task_id, title, task_status, workspace_path, archived, deleted, pinned, updated_at)
         VALUES (?1, ?2, NULL, '/ws', ?3, ?4, 0, ?5)",
        rusqlite::params![task_id, title, archived, deleted, NOW],
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

/// 已配对 GET 薄壳：cookie + oneshot，返回 (状态码, 响应体字符串)
async fn get_json(app: axum::Router, uri: &str) -> (axum::http::StatusCode, String) {
    use http_body_util::BodyExt as _;
    let resp = app
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri(uri)
                .header("cookie", COOKIE)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

// ===== 红测 1：双形态合法域与边界 =====

#[test]
fn accepts_both_zcode_main_id_shapes() {
    // 旧形态照旧：sess_ + 严格 8-4-4-4-12 hex UUID（hex 大小写兼容）
    assert!(is_valid_session_id(OLD_MAIN));
    assert!(is_valid_session_id(
        "sess_0F1E2D3C-4B5A-4948-8276-9A0B8C7D6E5F"
    ));
    // 新形态：session_ + 32 ASCII hex（hex 大小写兼容）
    assert!(is_valid_session_id(NEW_MAIN));
    assert!(is_valid_session_id(NEW_MAIN_UPPER));
    assert!(is_valid_session_id(
        "session_aAbBcCdD00112233445566778899ff00"
    ));

    // 拒绝面逐条锁死（合法域只扩这两型，不得过宽）：
    // 缺前缀（裸 UUID / 裸 32hex）
    assert!(!is_valid_session_id("0f1e2d3c-4b5a-4948-8276-9a0b8c7d6e5f"));
    assert!(!is_valid_session_id("0f1e2d3c4b5a494882769a0b8c7d6e5f"));
    // 前缀与形态错配（契约只定义两型；混配不放宽）
    assert!(!is_valid_session_id(
        "sess_0f1e2d3c4b5a494882769a0b8c7d6e5f"
    ));
    assert!(!is_valid_session_id(
        "session_0f1e2d3c-4b5a-4948-8276-9a0b8c7d6e5f"
    ));
    // 空／缺主体
    assert!(!is_valid_session_id(""));
    assert!(!is_valid_session_id("sess_"));
    assert!(!is_valid_session_id("session_"));
    // 短长串（32 hex 的 ±1、严格 UUID 的 ±1）
    assert!(!is_valid_session_id(
        "session_0f1e2d3c4b5a494882769a0b8c7d6e5"
    )); // 31
    assert!(!is_valid_session_id(
        "session_0f1e2d3c4b5a494882769a0b8c7d6e5f0"
    )); // 33
    assert!(!is_valid_session_id(
        "sess_0f1e2d3c-4b5a-4948-8276-9a0b8c7d6e5"
    )); // 35
    assert!(!is_valid_session_id(
        "sess_0f1e2d3c-4b5a-4948-8276-9a0b8c7d6e5f0"
    )); // 37
        // 非 hex 字符集（36 字符四连字符骗术与 32 位含非 hex）
    assert!(!is_valid_session_id(
        "sess_zzzzzzzz-zzzz-zzzz-zzzz-zzzzzzzzzzzz"
    ));
    assert!(!is_valid_session_id(
        "session_zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"
    ));
    assert!(!is_valid_session_id(
        "session_0f1e2d3c4b5a494882769a0b8c7d6e5&"
    ));
    // 非 ASCII
    assert!(!is_valid_session_id(
        "session_0f1e2d3c4b5a494882769a0兔维斯d6e5"
    ));
    assert!(!is_valid_session_id(
        "sèssion_0f1e2d3c4b5a494882769a0b8c7d6e5f"
    ));
    // 路径形态
    assert!(!is_valid_session_id("sess_../etc/passwd"));
    assert!(!is_valid_session_id(
        "session_0f1e2d3c/4b5a494882769a0b8c7d6e5f"
    ));
    assert!(!is_valid_session_id(
        "session_0f1e2d3c\\4b5a494882769a0b8c7d6e5"
    ));
    // 子代理前缀（旧形态子代理 id 双保险仍在）
    assert!(!is_valid_session_id(
        "sess_subagent_agent_1f1e2d3c-4b5a-4948-8276-9a0b8c7d6e5f"
    ));
}

// ===== 红测 2：新旧主会话共存出卡、ID 各自独立 =====

#[test]
fn mixed_main_sessions_keep_distinct_ids() {
    let tmp = tempfile::tempdir().unwrap();
    let roots = ZcodeRoots::from_home(tmp.path());
    let cli = build_cli_db(&roots.cli_db);
    let tasks = build_tasks_db(&roots.tasks_db);

    // 应出卡：旧形态主 + 新形态主（同 hex 芯，考验不合并）+ 新形态大写 + 空串 parent
    insert_session(
        &cli,
        OLD_MAIN,
        None,
        "/proj/a",
        "interactive",
        Some("旧主"),
        NOW - 1000,
    );
    insert_session(
        &cli,
        NEW_MAIN,
        None,
        "/proj/a",
        "interactive",
        Some("新主"),
        NOW - 2000,
    );
    insert_session(
        &cli,
        NEW_MAIN_UPPER,
        None,
        "/proj/a",
        "interactive",
        None,
        NOW - 3000,
    );
    // 空串 parent 沿用现有口径 = 视为无父，照常出卡（非空才过滤）
    insert_session(
        &cli,
        "sess_6f1e2d3c-4b5a-4948-8276-9a0b8c7d6e5f",
        Some(""),
        "/proj/a",
        "interactive",
        None,
        NOW - 4000,
    );
    // 应排除：非空 parent（新形态子行双保险走 parent 门）
    insert_session(
        &cli,
        "session_2f1e2d3c4b5a494882769a0b8c7d6e5f",
        Some(NEW_MAIN),
        "/proj/a",
        "interactive",
        None,
        NOW - 1000,
    );
    // 应排除：旧形态子代理前缀 id（parent 为 NULL，只靠 id 门挡下）
    insert_session(
        &cli,
        "sess_subagent_agent_8f1e2d3c-4b5a-4948-8276-9a0b8c7d6e5f",
        None,
        "/proj/a",
        "interactive",
        None,
        NOW - 1000,
    );
    // 应排除：归档（新形态 id，任务索引 archived=1）
    insert_session(
        &cli,
        "session_3f1e2d3c4b5a494882769a0b8c7d6e5f",
        None,
        "/proj/a",
        "interactive",
        None,
        NOW - 1000,
    );
    insert_task(
        &tasks,
        "session_3f1e2d3c4b5a494882769a0b8c7d6e5f",
        None,
        1,
        0,
    );
    // 应排除：deleted（旧形态 id，任务索引 deleted=1）
    insert_session(
        &cli,
        "sess_4f1e2d3c-4b5a-4948-8276-9a0b8c7d6e5f",
        None,
        "/proj/a",
        "interactive",
        None,
        NOW - 1000,
    );
    insert_task(
        &tasks,
        "sess_4f1e2d3c-4b5a-4948-8276-9a0b8c7d6e5f",
        None,
        0,
        1,
    );
    // 应排除：窗口外（新形态 id，time_updated 早于 24h 窗）
    insert_session(
        &cli,
        "session_5f1e2d3c4b5a494882769a0b8c7d6e5f",
        None,
        "/proj/a",
        "interactive",
        None,
        NOW - 25 * ONE_HOUR_MS,
    );
    drop(cli);
    drop(tasks);

    let sessions = build_sessions(&roots, &fake_host(), NOW);
    let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();

    // 出卡集合逐项断言：四个主会话都在，六个排除项都不在
    assert!(ids.contains(&OLD_MAIN), "旧形态主会话应出卡，实际 {ids:?}");
    assert!(ids.contains(&NEW_MAIN), "新形态主会话应出卡，实际 {ids:?}");
    assert!(
        ids.contains(&NEW_MAIN_UPPER),
        "新形态大写 hex 应出卡，实际 {ids:?}"
    );
    assert!(
        ids.contains(&"sess_6f1e2d3c-4b5a-4948-8276-9a0b8c7d6e5f"),
        "空串 parent 应沿用旧口径（视为无父）出卡，实际 {ids:?}"
    );
    for excluded in [
        "session_2f1e2d3c4b5a494882769a0b8c7d6e5f", // parent 非空（子代理）
        "sess_subagent_agent_8f1e2d3c-4b5a-4948-8276-9a0b8c7d6e5f", // 子代理前缀
        "session_3f1e2d3c4b5a494882769a0b8c7d6e5f", // 归档
        "sess_4f1e2d3c-4b5a-4948-8276-9a0b8c7d6e5f", // deleted
        "session_5f1e2d3c4b5a494882769a0b8c7d6e5f", // 窗口外
    ] {
        assert!(
            !ids.contains(&excluded),
            "{excluded} 应被逐项排除，实际 {ids:?}"
        );
    }

    // ID 原样保留：不重命名、不转写、不删横杠、不合并为同一键
    // （OLD_MAIN 与 NEW_MAIN 同 32 位 hex 芯，必须仍是两张独立卡）
    let old_card = sessions.iter().find(|s| s.id == OLD_MAIN).unwrap();
    let new_card = sessions.iter().find(|s| s.id == NEW_MAIN).unwrap();
    assert_ne!(
        old_card.id, new_card.id,
        "同 hex 芯的新旧 id 不得合并为一键"
    );
    assert!(old_card.id.contains('-'), "旧 id 的横杠必须原样保留");
    assert!(
        new_card.id.starts_with("session_"),
        "新 id 前缀必须原样保留"
    );
    let upper_card = sessions.iter().find(|s| s.id == NEW_MAIN_UPPER).unwrap();
    assert!(
        upper_card.id.contains('F') && upper_card.id.contains('E'),
        "新 id 的原始大小写必须原样保留，实际 {:?}",
        upper_card.id
    );

    // 宿主映射与其他规则不因形态扩充变化：卡片挂宿主进程、APP 形态、ZCode 归属
    assert!(!sessions.is_empty());
    for card in &sessions {
        assert_eq!(
            card.pid, 4242,
            "卡片应挂宿主进程（进程池化），与 id 形态无关"
        );
        assert!(matches!(card.form, ProcessForm::App));
        assert_eq!(
            card.agent_type,
            multi_agents_manager_lib::session::AgentType::ZCode
        );
    }

    // 无宿主不出卡（宿主是 ZCode 全部卡片的总开关；本断言不触真实数据目录——
    // get_zcode_sessions 对空进程表在取 home 之前即早退）
    assert!(get_zcode_sessions(&[]).is_empty(), "无宿主进程时不得出卡");
    let cli_form = AgentProcess {
        form: ProcessForm::Cli,
        ..fake_host()
    };
    assert!(
        get_zcode_sessions(&[cli_form]).is_empty(),
        "非 App 形态进程不得当作宿主出卡"
    );
}

// ===== 红测 3：合成新 ID 走通快照 → session-messages / session-files =====

#[tokio::test]
async fn new_id_reaches_remote_reading() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().to_path_buf();
    let roots = ZcodeRoots::from_home(&home);
    let cli = build_cli_db(&roots.cli_db);
    let tasks = build_tasks_db(&roots.tasks_db);

    let now = chrono::Utc::now().timestamp_millis();
    // 新旧两条主会话消息流形态完全一致（正文合成过滤与状态输入的输入相同）：
    // 只有 id 形态不同 → 卡片除 id 外必须逐项相同
    insert_session(
        &cli,
        OLD_MAIN,
        None,
        "/ws/synth-proj",
        "interactive",
        None,
        now - 1000,
    );
    insert_session(
        &cli,
        NEW_MAIN,
        None,
        "/ws/synth-proj",
        "interactive",
        None,
        now - 1000,
    );
    // 任务索引同标题；新会话行带 ZCode 侧栏未读标记（兔维斯未读池不使用——
    // unread 必须仍为 false，未读治理归各自的池管线门）
    insert_task(&tasks, OLD_MAIN, Some("synthetic-task-title"), 0, 0);
    insert_task(&tasks, NEW_MAIN, Some("synthetic-task-title"), 0, 0);
    tasks
        .execute(
            "UPDATE tasks SET unread_at = ?1, last_unread_at = ?1 WHERE task_id = ?2",
            rusqlite::params![now - 500, NEW_MAIN],
        )
        .unwrap();

    const USER_BODY: &str = "synthetic-user-body 合成用户正文";
    const ASSIST_BODY: &str = "synthetic-assistant-body 合成助手正文";
    const SYNTH_MARKER: &str = "SYNTHETIC-REMINDER-应被过滤";
    const UPLOAD_PATH: &str = "/synthetic/uploads/new-upload.txt";
    // 两型会话各自的消息三件套（真实 user → assistant → 合成 user 提醒）；
    // 附件 part 只挂新形态会话（考验按原 id 隔离读取、不跨会话合并）
    for (tag, sid, with_file) in [("new", NEW_MAIN, true), ("old", OLD_MAIN, false)] {
        insert_message(
            &cli,
            &format!("m_{tag}_1"),
            sid,
            1,
            now - 900,
            r#"{"role":"user","semantics":{"kind":"user_prompt"}}"#,
        );
        insert_part(
            &cli,
            &format!("p_{tag}_1"),
            &format!("m_{tag}_1"),
            1,
            &format!(r#"{{"type":"text","text":"{USER_BODY}"}}"#),
        );
        if with_file {
            insert_part(
                &cli,
                &format!("p_{tag}_file"),
                &format!("m_{tag}_1"),
                2,
                &format!(r#"{{"type":"file","source":{{"path":"{UPLOAD_PATH}"}}}}"#),
            );
        }
        insert_message(
            &cli,
            &format!("m_{tag}_2"),
            sid,
            2,
            now - 800,
            r#"{"role":"assistant","semantics":{"kind":"assistant"}}"#,
        );
        insert_part(
            &cli,
            &format!("p_{tag}_2"),
            &format!("m_{tag}_2"),
            1,
            &format!(r#"{{"type":"text","text":"{ASSIST_BODY}"}}"#),
        );
        // 回合中途合成注入（user 角色 + synthetic 标记 + 记账 kind）
        insert_message(
            &cli,
            &format!("m_{tag}_3"),
            sid,
            3,
            now - 700,
            r#"{"role":"user","synthetic":true,"semantics":{"kind":"todo_reminder"}}"#,
        );
        insert_part(
            &cli,
            &format!("p_{tag}_3"),
            &format!("m_{tag}_3"),
            1,
            &format!(r#"{{"type":"text","text":"{SYNTH_MARKER}"}}"#),
        );
    }
    drop(cli);
    drop(tasks);

    // ===== RemoteState 假源装配（zcode_config_remote.rs 同款风格）=====
    let conn: SharedConn = {
        let c = Connection::open_in_memory().expect("open memory db");
        multi_agents_manager_lib::database::schema::init(&c);
        multi_agents_manager_lib::database::migration::migrate(&c).expect("migrate");
        Arc::new(Mutex::new(c))
    };
    DeviceStore::Owned(conn.clone())
        .with(|c| {
            persist_device(
                c,
                &NewDevice {
                    id: "dev-1".to_string(),
                    name: "iPhone".into(),
                    ua: "ua-dev-1".into(),
                    origin_ip: "127.0.0.1#dev-1".into(),
                    via: "lan".into(),
                    paired_at: chrono::Utc::now().timestamp_millis(),
                },
            )
        })
        .expect("seed device");

    // 快照（session_source）与正文／文件读取全部指向同一 tempdir home 夹具，
    // 零真实 ~/.zcode 接触
    let snap_roots = roots.clone();
    let host = fake_host();
    let session_source = Box::new(move || {
        let sessions: Vec<Session> =
            build_sessions(&snap_roots, &host, chrono::Utc::now().timestamp_millis());
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
    let msg_home = home.clone();
    let message_source = Box::new(move |agent: &str, sid: &str, limit: usize| {
        read_session_messages_with(&msg_home, agent, sid, limit)
    });
    let path_home = home.clone();
    let path_source = Box::new(move |agent: &str, sid: &str, limit: usize| {
        extract_file_paths_with(&path_home, agent, sid, limit)
    });
    let hidden: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let hidden_src = hidden.clone();

    let state = Arc::new(RemoteState {
        ui_config_source: Box::new(|| None),
        subagent_source: std::collections::HashMap::new(),
        subagent_message_source: std::collections::HashMap::new(),
        capability_table: multi_agents_manager_lib::inject::capability::new_table(),
        session_source,
        pairing_counter: Box::new(Vec::new),
        board_hidden_ids: Box::new(move || hidden_src.lock().unwrap().clone()),
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
    });
    let app = router(state);

    // (1) 快照：新形态卡必须出现在 /sessions（bug 本体 = 卡不再出现）
    let (status, body) = get_json(app.clone(), "/m/api/v1/sessions").await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).expect("sessions JSON");
    let find = |body: &serde_json::Value, sid: &str| -> serde_json::Value {
        body["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == serde_json::Value::String(sid.to_string()))
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    };
    let new_card = find(&json, NEW_MAIN);
    assert_ne!(
        new_card,
        serde_json::Value::Null,
        "新形态主会话卡必须进快照，实际 {body}"
    );
    let old_card = find(&json, OLD_MAIN);
    assert_ne!(
        old_card,
        serde_json::Value::Null,
        "旧形态主会话卡应照常进快照"
    );
    // ID 原样：wire 上的 id 就是数据库里的原始串
    assert_eq!(new_card["id"], NEW_MAIN);
    assert_eq!(old_card["id"], OLD_MAIN);

    // 状态输入不变：两卡消息流形态相同、仅 id 形态不同 → 除 id 外卡片逐项相同
    // （id 形态不是状态推导输入；合成提醒不参与预览）
    let mut norm_new = new_card.clone();
    let mut norm_old = old_card.clone();
    norm_new.as_object_mut().unwrap().remove("id");
    norm_old.as_object_mut().unwrap().remove("id");
    assert_eq!(
        norm_new, norm_old,
        "同输入必须同状态（含 lastMessage 不被合成消息污染）"
    );
    assert_eq!(new_card["lastMessage"].as_str(), Some(ASSIST_BODY));
    // 未读门不受本修复影响：parser 恒 unread=false，ZCode 侧栏 unread_at 不外泄；
    // 未读治理归兔维斯自己的未读池管线（adapter 层），不在此门
    assert_eq!(
        new_card["unread"],
        serde_json::Value::Bool(false),
        "unread_at 不得泄漏为未读态"
    );

    // (2) 正文：新 ID 走 /session-messages 读到合成正文；合成注入被过滤（口径不变）
    let (status, body) = get_json(
        app.clone(),
        &format!("/m/api/v1/session-messages?agent_type=zcode&session_id={NEW_MAIN}"),
    )
    .await;
    assert_eq!(
        status,
        axum::http::StatusCode::OK,
        "新形态 id 正文必须可读，实际 {body}"
    );
    assert!(
        body.contains(USER_BODY) && body.contains(ASSIST_BODY),
        "正文须含真实 user/assistant 消息，实际 {body}"
    );
    assert!(
        !body.contains(SYNTH_MARKER),
        "合成注入必须继续被过滤，实际 {body}"
    );
    let (status, body) = get_json(
        app.clone(),
        &format!("/m/api/v1/session-messages?agent_type=zcode&session_id={OLD_MAIN}"),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(body.contains(USER_BODY) && !body.contains(SYNTH_MARKER));

    // (3) 附件：新 ID 走 /session-files 读到 type=file part 的路径；
    // 旧会话（同消息流、无附件 part）不得串到别人的附件（原 id 各自为键）
    let (status, body) = get_json(
        app.clone(),
        &format!("/m/api/v1/session-files?agent_type=zcode&session_id={NEW_MAIN}"),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(
        body.contains(UPLOAD_PATH),
        "新形态 id 的附件必须可读，实际 {body}"
    );
    let (status, body) = get_json(
        app.clone(),
        &format!("/m/api/v1/session-files?agent_type=zcode&session_id={OLD_MAIN}"),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(
        !body.contains(UPLOAD_PATH),
        "附件按原 id 隔离，不得跨会话合并，实际 {body}"
    );

    // (4) 隐藏门照旧作用于新形态卡（board 隐藏集合作为自己的门，不因 id 形态扩充而绕过）
    hidden.lock().unwrap().push(NEW_MAIN.to_string());
    let (status, body) = get_json(app.clone(), "/m/api/v1/sessions").await;
    assert_eq!(status, axum::http::StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).expect("sessions JSON");
    assert_eq!(
        find(&json, NEW_MAIN),
        serde_json::Value::Null,
        "隐藏后新形态卡须从看板剔除"
    );
    assert_ne!(
        find(&json, OLD_MAIN),
        serde_json::Value::Null,
        "隐藏只作用于被隐藏项"
    );
    hidden.lock().unwrap().clear();
    let (_, body) = get_json(app.clone(), "/m/api/v1/sessions").await;
    let json: serde_json::Value = serde_json::from_str(&body).expect("sessions JSON");
    assert_ne!(
        find(&json, NEW_MAIN),
        serde_json::Value::Null,
        "取消隐藏后须回归"
    );
}

// ===== A7 红测：正文与附件引用的同源锚点（设计 §6 末段）=====
//
// 缺陷本体：`read_zcode_attachment_refs_with` 用消息窗口的**原始 DB ordinal**当
// 附件序号，而正文侧的 seq 是「多 parts 展开 + 合成过滤 + tail 裁剪（finalize）」
// 之后的显示序号——两个刻度对不齐，卡片上的附件锚点会指错消息。修法：附件锚点
// 必须从与正文同一个有界窗口／finalize 投影结果推导（私有 ZcodeProjection 的
// source_anchors：数据库 message ID → 最终显示 seq）；无显示条目的消息（file-only
// /被过滤）保留文件池、lastSeq 用既有中性 0 口径仅作排序占位。

const ANCHOR_SID: &str = "sess_1a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d";

/// 红测 1：附件锚点 = 所属消息在投影后的**最终显示 seq**，不是原 DB ordinal；
/// fixture 序：合成消息 → user 文本 → assistant（thinking/tool/text 三条目）→
/// 下一 user 文本+file。改变 limit 后仍对齐。
#[test]
fn zcode_attachment_anchor_matches_projected_message() {
    let tmp = tempfile::tempdir().unwrap();
    let roots = ZcodeRoots::from_home(tmp.path());
    let cli = build_cli_db(&roots.cli_db);

    const SYNTH_BODY: &str = "anchor1-synthetic-reminder-应被过滤";
    const U2_BODY: &str = "anchor1-user-first-body";
    const THINK: &str = "anchor1-assistant-thinking";
    const A3_BODY: &str = "anchor1-assistant-final-text";
    const U4_BODY: &str = "anchor1-user-second-body-with-file";
    const UPLOAD: &str = "/synthetic/anchor1/upload.txt";

    // DB 窗口 ordinal（旧缺陷口径）：m_synth=0 / m_u2=1 / m_a3=2 / m_u4=3；
    // 显示 seq（正确口径，合成被滤、assistant 展开 3 条）：u2=0 thinking=1
    // tool=2 text=3 u4=4 —— 附件必须锚到 4，而不是原 ordinal 3。
    insert_message(
        &cli,
        "m_synth",
        ANCHOR_SID,
        1,
        NOW - 4000,
        r#"{"role":"user","synthetic":true,"semantics":{"kind":"todo_reminder"}}"#,
    );
    insert_part(
        &cli,
        "p_synth",
        "m_synth",
        1,
        &format!(r#"{{"type":"text","text":"{SYNTH_BODY}"}}"#),
    );
    insert_message(
        &cli,
        "m_u2",
        ANCHOR_SID,
        2,
        NOW - 3000,
        r#"{"role":"user","semantics":{"kind":"user_prompt"}}"#,
    );
    insert_part(
        &cli,
        "p_u2",
        "m_u2",
        1,
        &format!(r#"{{"type":"text","text":"{U2_BODY}"}}"#),
    );
    insert_message(
        &cli,
        "m_a3",
        ANCHOR_SID,
        3,
        NOW - 2000,
        r#"{"role":"assistant","semantics":{"kind":"assistant"}}"#,
    );
    insert_part(
        &cli,
        "p_a3_th",
        "m_a3",
        1,
        &format!(r#"{{"type":"reasoning","text":"{THINK}"}}"#),
    );
    insert_part(
        &cli,
        "p_a3_tool",
        "m_a3",
        2,
        r#"{"type":"tool","tool":"Bash","state":{"input":{"command":"ls"}}}"#,
    );
    insert_part(
        &cli,
        "p_a3_tx",
        "m_a3",
        3,
        &format!(r#"{{"type":"text","text":"{A3_BODY}"}}"#),
    );
    insert_message(
        &cli,
        "m_u4",
        ANCHOR_SID,
        4,
        NOW - 1000,
        r#"{"role":"user","semantics":{"kind":"user_prompt"}}"#,
    );
    insert_part(
        &cli,
        "p_u4",
        "m_u4",
        1,
        &format!(r#"{{"type":"text","text":"{U4_BODY}"}}"#),
    );
    insert_part(
        &cli,
        "p_u4_file",
        "m_u4",
        2,
        &format!(r#"{{"type":"file","source":{{"path":"{UPLOAD}"}}}}"#),
    );
    drop(cli);

    // —— limit=500：完整窗（合成过滤 + 多 parts 展开后共 5 条显示项）
    let pg = read_session_messages_with(tmp.path(), "zcode", ANCHOR_SID, 500).unwrap();
    assert_eq!(
        pg.messages.len(),
        5,
        "u2 1 条 + assistant 展开 3 条 + u4 1 条"
    );
    let body_seq = pg
        .messages
        .iter()
        .find(|m| m.content == U4_BODY)
        .expect("带附件的 user 正文必须上板")
        .seq;
    assert_eq!(body_seq, 4, "显示序：合成被滤、assistant 占 1..3");
    let (entries, _) = extract_file_paths_with(tmp.path(), "zcode", ANCHOR_SID, 500);
    let file = entries
        .iter()
        .find(|e| e.path == UPLOAD)
        .expect("附件必须入池（既有行为保留）");
    assert_eq!(file.origin, FileOrigin::User);
    assert_eq!(
        file.last_seq, body_seq,
        "附件锚点必须等于所属消息最终显示 seq（同源投影），实得 {}",
        file.last_seq
    );
    assert_eq!(file.last_seq, 4, "锚点是投影后 seq，不是原 DB ordinal 3");

    // —— limit=3：有界窗（尾 3 条消息）再 tail 裁剪（尾 3 个显示项）后仍对齐
    let pg3 = read_session_messages_with(tmp.path(), "zcode", ANCHOR_SID, 3).unwrap();
    let body_seq3 = pg3
        .messages
        .iter()
        .find(|m| m.content == U4_BODY)
        .expect("limit=3 尾窗仍须覆盖带附件消息")
        .seq;
    let (entries3, _) = extract_file_paths_with(tmp.path(), "zcode", ANCHOR_SID, 3);
    let file3 = entries3
        .iter()
        .find(|e| e.path == UPLOAD)
        .expect("limit=3 附件仍在池");
    assert_eq!(
        file3.last_seq, body_seq3,
        "改变 limit 后锚点仍须与投影正文同刻度（正文侧实得 {body_seq3}）"
    );
}

/// 红测 2：被过滤条目不指向其他消息；file-only 消息无可显示条目 → 文件保留、
/// lastSeq=0 作中性排序占位（不新增正文消息、不引入负数哨兵）；预览仍可用。
#[test]
fn synthetic_or_unrendered_has_no_fake_anchor() {
    let tmp = tempfile::tempdir().unwrap();
    let roots = ZcodeRoots::from_home(tmp.path());
    let cli = build_cli_db(&roots.cli_db);

    const SID: &str = ANCHOR_SID;
    const M1_BODY: &str = "anchor2-user-body-A";
    const SYNTH_BODY: &str = "anchor2-synthetic-应被过滤";
    const M3_BODY: &str = "anchor2-user-body-B";
    // 合成消息携带的附件：旧缺陷 = ordinal 1（恰指到 m3 的显示槽位 = 假锚点）
    let fake_synth_path = tmp.path().join("synth-src/fake-synth.txt");
    // file-only 消息（无 text part → 无显示条目）：旧缺陷 = ordinal 3
    let only_file_path = tmp.path().join("synth-src/only-file.txt");
    std::fs::create_dir_all(only_file_path.parent().unwrap()).unwrap();
    std::fs::write(&fake_synth_path, b"synthetic fake").unwrap();
    std::fs::write(&only_file_path, b"synthetic only-file body").unwrap();
    let fake_synth = fake_synth_path.to_string_lossy().into_owned();
    let only_file = only_file_path.to_string_lossy().into_owned();

    insert_message(
        &cli,
        "m1",
        SID,
        1,
        NOW - 4000,
        r#"{"role":"user","semantics":{"kind":"user_prompt"}}"#,
    );
    insert_part(
        &cli,
        "p1",
        "m1",
        1,
        &format!(r#"{{"type":"text","text":"{M1_BODY}"}}"#),
    );
    insert_message(
        &cli,
        "m2",
        SID,
        2,
        NOW - 3000,
        r#"{"role":"user","synthetic":true,"semantics":{"kind":"todo_reminder"}}"#,
    );
    insert_part(
        &cli,
        "p2",
        "m2",
        1,
        &format!(r#"{{"type":"text","text":"{SYNTH_BODY}"}}"#),
    );
    insert_part(
        &cli,
        "p2_file",
        "m2",
        2,
        &format!(r#"{{"type":"file","source":{{"path":"{fake_synth}"}}}}"#),
    );
    insert_message(
        &cli,
        "m3",
        SID,
        3,
        NOW - 2000,
        r#"{"role":"user","semantics":{"kind":"user_prompt"}}"#,
    );
    insert_part(
        &cli,
        "p3",
        "m3",
        1,
        &format!(r#"{{"type":"text","text":"{M3_BODY}"}}"#),
    );
    // file-only user 消息：整条只有一个 file part（无正文可显示）
    insert_message(
        &cli,
        "m4",
        SID,
        4,
        NOW - 1000,
        r#"{"role":"user","semantics":{"kind":"user_prompt"}}"#,
    );
    insert_part(
        &cli,
        "p4_file",
        "m4",
        1,
        &format!(r#"{{"type":"file","source":{{"path":"{only_file}"}}}}"#),
    );
    drop(cli);

    // 正文面：合成被滤、file-only 不伪造正文 —— 恰两条显示消息
    let pg = read_session_messages_with(tmp.path(), "zcode", SID, 500).unwrap();
    assert_eq!(
        pg.messages.len(),
        2,
        "被过滤与 file-only 消息都不得新增正文条目（不伪造）"
    );
    assert!(pg.messages.iter().all(|m| m.content != SYNTH_BODY));
    let m3_seq = pg
        .messages
        .iter()
        .find(|m| m.content == M3_BODY)
        .unwrap()
        .seq;

    // 文件面：两条附件都保留在池里
    let (entries, _) = extract_file_paths_with(tmp.path(), "zcode", SID, 500);
    let synth = entries
        .iter()
        .find(|e| e.path == fake_synth)
        .expect("合成消息的附件仍须保留文件池");
    let only = entries
        .iter()
        .find(|e| e.path == only_file)
        .expect("file-only 消息的附件仍须保留文件池");
    assert!(
        entries.iter().all(|e| e.last_seq >= 0),
        "不得引入负数哨兵：{:?}",
        entries.iter().map(|e| e.last_seq).collect::<Vec<_>>()
    );
    // 中性 0 占位；旧缺陷口径下合成条目拿 ordinal 1 = 恰指向 m3 显示槽（假锚点）
    assert_eq!(
        synth.last_seq, 0,
        "被过滤条目不得指向其他消息（m3 seq={m3_seq}）"
    );
    assert_ne!(
        synth.last_seq, m3_seq,
        "合成附件的锚点不得假指到真实显示的 m3"
    );
    assert_eq!(only.last_seq, 0, "file-only 无显示条目 → 中性 0 排序占位");
    assert_eq!(synth.origin, FileOrigin::User);
    assert_eq!(only.origin, FileOrigin::User);

    // 预览仍可用：入池路径即磁盘实体，read_file_safe 正常读出
    let home_s = tmp.path().to_str().unwrap();
    let (bytes, _) = read_file_safe(home_s, &only_file, Some(home_s)).expect("预览可用");
    assert_eq!(bytes, b"synthetic only-file body");
    let (bytes2, _) = read_file_safe(home_s, &fake_synth, Some(home_s)).expect("预览可用");
    assert_eq!(bytes2, b"synthetic fake");
}
