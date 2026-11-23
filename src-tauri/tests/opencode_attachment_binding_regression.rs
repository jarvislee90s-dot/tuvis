#![cfg(all(target_os = "macos", feature = "opencode-coordinator-tests"))]
#![allow(dead_code)]
//! OpenCode 原生交互移动路由（计划任务 8）HTTP 集成测试：**真配对门** + 可读契约的回环假 peer。
//!
//! 零真实副作用纪律（与 `codex_ipc_remote.rs` / `m9r_e2e.rs` 同款）：
//! * `DeviceStore::Owned(共享内存连接)`——`state.store` 与原生 hub 的账本**同库同锁**
//!   （旧路径屏障与路由归属必须是同一道锁，否则测试证明的不是生产形态）；
//! * 原生服务一律走本文件手写的**回环假 peer**（`127.0.0.1:0`）：它真的提供
//!   `/openapi.json`（仓库内契约夹具）+ `/api/info` + 会话读 + 消息/pending/inbox，
//!   因此 `GET /opencode/view` 能**真投影成功并认领路由**——「旧入口零终端写」的断言
//!   才有意义（peer 在同一夹具里可证明可达，计数器证明写没发生）；
//! * 注入器是**记录型假体**：断言「终端零键序/零正文」看它，不看 HTTP 状态码；
//! * 会话扫描经 `session_source` 缝注入固定快照（零真实目录扫描）。
//!
//! 覆盖（brief + 评审 I-1..I-4）：
//! 1. 未配对 / 绑定过期 → 403 且**零请求到原生服务**（同夹具另有可达性对照）；
//! 2. 非 OpenCode 会话、客户端自带 raw URL/method/permissions → 400；
//! 3. 另一设备的动作 → 404（属主自己读 → 200，证明不是「一律 404」的空断言）；
//! 4. 停远程/注销设备 → 原生 watch 关闭（订阅端观察到终止）；
//! 5. `GET /opencode/view`（开关开）→ **真的认领**（I-2/I-4）；
//! 6. 已归 Native 时旧 send / approve / question-answer / mode-switch / jump 一律 409
//!    `OC_ROUTE_CHANGED`，retract **仍可删本机行**（I-3），flush 让位且零终端写（I-4）；
//! 7. `POST /opencode/route/legacy`（I-1）：成功交回 → 旧写入口复活；未决动作 → 409 零变更；
//!    幂等（未接过 Native 也成功）；
//! 8. 退役验证键缺失/false 不关闭原生；兼容策略下原生认领仍隔离旧写；
//! 9. 旧请求与路由切换并发 → 至多一条路径取得写资格；
//! 10. 有旧 pending 的会话不被自动切到 Native（`route=legacy` + `legacy_pending`）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use multi_agents_manager_lib::inject::engine::Injector;
use multi_agents_manager_lib::remote::pairing::{persist_device, DeviceStore, NewDevice};
use multi_agents_manager_lib::remote::server::{router, RemoteState, SseRegistry};
use multi_agents_manager_lib::services::opencode_native::{DiscoveryInput, NativeHub};
use multi_agents_manager_lib::session::{
    AgentType, ProcessForm, Session, SessionStatus, SessionsResponse,
};
use tower::ServiceExt as _;

// ============================================================
// 回环假 peer：可读契约 + 真投影所需的最小应答集（带计数器）
// ============================================================

/// 服务代际（登记文件 / `/api/info` / 会话卡片 pid 三处必须一致）。
const PEER_SERVICE_ID: &str = "svc-test";
const PEER_PID: u32 = 7411;
const PEER_VERSION: &str = "2.0.22";
const PEER_STARTED_AT_MS: i64 = 1_000;

/// 假 peer 的共享态：目录 + 契约文本 + 计数器 + 仅测试用的可控阻塞。
struct Shared {
    /// 已 realpath 的项目目录（会话卡片与 `/api/session/{id}` 必须同值）。
    directory: String,
    conditional_form: StdMutex<Option<serde_json::Value>>,
    endpoint: String,
    contract: String,
    requests: AtomicUsize,
    session_reads: AtomicUsize,
    writes: AtomicUsize,
    b_writes: AtomicUsize,
    offline: AtomicBool,
    offline_connections: AtomicUsize,
    offline_connected: tokio::sync::Notify,
    event_connections: AtomicUsize,
    event_connected: tokio::sync::Notify,
    event_disconnections: AtomicUsize,
    event_disconnected: tokio::sync::Notify,
    disconnect_events: tokio::sync::Notify,
    /// 下一次 `/message` 读是否在应答前停住（只在本文件的 reserve 前窗口用例里武装）。
    hold_message_read: AtomicBool,
    /// 「已经停在读上」的信号（测试确认首次派发确实还没走到 reserve）。
    read_blocked: tokio::sync::Notify,
    /// 下一次 form detail 读是否停住（C2：等待期间 pause/重开）。
    hold_form_detail: AtomicBool,
    form_detail_blocked: tokio::sync::Notify,
    form_detail_release: tokio::sync::Notify,
    /// 放行信号。
    read_release: tokio::sync::Notify,
    /// 投影读结束后，暂停写通道最后的身份复核。
    hold_info_read: AtomicBool,
    info_blocked: tokio::sync::Notify,
    info_release: tokio::sync::Notify,
    /// 假 peer 收到的 prompt 写入：回读的 message 列表原样回显（好让 send 能真的 Confirmed）。
    prompts: StdMutex<Vec<serde_json::Value>>,
    /// R2 冒名旋钮：非 0 时 `/api/info` 自报该 pid（登记滞留不变 = 同端口冒名形态）。
    info_pid: AtomicUsize,
}

/// 手写回环 HTTP/1.1 假 peer。
///
/// 断言口径（brief「expired binding → 零 POST」）：`requests()` = 收到的请求数，
/// `writes()` = 其中非 GET 的请求数。两条计数器都是**线上事实**，不看返回值自证；
/// 同夹具里 `GET /opencode/view` 成功即证明 peer 可达，故「0」不是空断言。
struct Peer {
    endpoint: String,
    shared: Arc<Shared>,
}

impl Peer {
    async fn start(directory: &Path) -> Self {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind 假 peer");
        let port = listener.local_addr().expect("peer addr").port();
        let shared = Arc::new(Shared {
            directory: directory.to_string_lossy().into_owned(),
            conditional_form: StdMutex::new(None),
            endpoint: format!("http://127.0.0.1:{port}"),
            contract: std::fs::read_to_string(format!(
                "{}/tests/fixtures/opencode-native/openapi.contract.json",
                env!("CARGO_MANIFEST_DIR")
            ))
            .expect("读取 openapi 契约夹具"),
            requests: AtomicUsize::new(0),
            session_reads: AtomicUsize::new(0),
            writes: AtomicUsize::new(0),
            b_writes: AtomicUsize::new(0),
            offline: AtomicBool::new(false),
            offline_connections: AtomicUsize::new(0),
            offline_connected: tokio::sync::Notify::new(),
            event_connections: AtomicUsize::new(0),
            event_connected: tokio::sync::Notify::new(),
            event_disconnections: AtomicUsize::new(0),
            event_disconnected: tokio::sync::Notify::new(),
            disconnect_events: tokio::sync::Notify::new(),
            hold_message_read: AtomicBool::new(false),
            read_blocked: tokio::sync::Notify::new(),
            read_release: tokio::sync::Notify::new(),
            hold_info_read: AtomicBool::new(false),
            info_blocked: tokio::sync::Notify::new(),
            info_release: tokio::sync::Notify::new(),
            hold_form_detail: AtomicBool::new(false),
            form_detail_blocked: tokio::sync::Notify::new(),
            form_detail_release: tokio::sync::Notify::new(),
            prompts: StdMutex::new(Vec::new()),
            info_pid: AtomicUsize::new(0),
        });
        let accepted = Arc::clone(&shared);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let shared = Arc::clone(&accepted);
                tokio::spawn(async move { serve_one(stream, shared).await });
            }
        });
        Self {
            endpoint: shared.endpoint.clone(),
            shared,
        }
    }

    fn requests(&self) -> usize {
        self.shared.requests.load(Ordering::SeqCst)
    }

    fn writes(&self) -> usize {
        self.shared.writes.load(Ordering::SeqCst)
    }

    /// R2 冒名形态：`/api/info` 自报他者 pid（登记文件滞留不变）。
    fn set_info_pid(&self, pid: u32) {
        self.shared.info_pid.store(pid as usize, Ordering::SeqCst);
    }

    /// 武装一次性阻塞：下一次 `/message` 读在给出应答前停住（reserve 尚未发生）。
    fn hold_next_message_read(&self) {
        self.shared.hold_message_read.store(true, Ordering::SeqCst);
    }

    /// 等首次派发的投影读真的停住（不用 sleep 猜时序）。
    async fn wait_until_read_blocked(&self) {
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            self.shared.read_blocked.notified(),
        )
        .await
        .expect("首次派发必须在时限内停到投影读上");
    }

    /// C2：一次性阻塞下一次 form detail 读（等待期间 pause/重开）。
    fn hold_next_form_detail(&self) {
        self.shared.hold_form_detail.store(true, Ordering::SeqCst);
    }

    async fn wait_until_form_detail_blocked(&self) {
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            self.shared.form_detail_blocked.notified(),
        )
        .await
        .expect("form detail 读必须在时限内停住");
    }

    fn release_form_detail(&self) {
        self.shared.form_detail_release.notify_one();
    }

    fn release_message_read(&self) {
        self.shared.read_release.notify_one();
    }

    /// 假 peer 收到的 prompt 写入条数（「最多一次写」的线上事实）。
    fn prompt_writes(&self) -> usize {
        self.shared.prompts.lock().unwrap().len()
    }
}

async fn serve_one(mut stream: tokio::net::TcpStream, shared: Arc<Shared>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    // 可控断链：连接关闭而非伪造成功响应，覆盖已缓存 source 的健康复核。
    if shared.offline.load(Ordering::SeqCst) {
        shared.offline_connections.fetch_add(1, Ordering::SeqCst);
        shared.offline_connected.notify_one();
        return;
    }
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 2048];
    let head_end = loop {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut request_line = head.split("\r\n").next().unwrap_or("").split_whitespace();
    let method = request_line.next().unwrap_or("").to_string();
    let target = request_line.next().unwrap_or("").to_string();
    let path = target.split('?').next().unwrap_or("").to_string();
    // 把请求体读尽（否则客户端看到的是 RST 而不是应答）
    let mut body_len: usize = 0;
    for line in head.split("\r\n").skip(1) {
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                body_len = value.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut have = buf.len().saturating_sub(head_end + 4);
    while have < body_len {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                have += n;
            }
        }
    }
    let body_text = String::from_utf8_lossy(&buf[head_end + 4..]).to_string();
    shared.requests.fetch_add(1, Ordering::SeqCst);
    if method == "GET" && path.starts_with("/api/session/") {
        shared.session_reads.fetch_add(1, Ordering::SeqCst);
    }
    if method != "GET" {
        shared.writes.fetch_add(1, Ordering::SeqCst);
        if path.starts_with("/api/session/ses-b/") {
            shared.b_writes.fetch_add(1, Ordering::SeqCst);
        }
    }

    // 事件流：真投影不订阅时不建连；`watch` 之后 pump 会连上来，保持打开直到对端收线。
    if path == "/api/event" {
        let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                    Cache-Control: no-cache\r\n\r\n";
        if stream.write_all(head.as_bytes()).await.is_err() {
            return;
        }
        let _ = stream.flush().await;
        shared.event_connections.fetch_add(1, Ordering::SeqCst);
        shared.event_connected.notify_one();
        let mut sink = [0u8; 256];
        tokio::select! {
            _ = shared.disconnect_events.notified() => {},
            _ = async { while matches!(stream.read(&mut sink).await, Ok(n) if n > 0) {} } => {},
        }
        shared.event_disconnections.fetch_add(1, Ordering::SeqCst);
        shared.event_disconnected.notify_one();
        return;
    }

    // 仅测试用：可控阻塞**首次派发的投影读**（在 reserve 之前），让核对命中 404 窗口。
    // 一次性（swap 先消费）：写之后的回读绝不再被拦住。
    if method == "GET"
        && path.starts_with("/api/session/")
        && path.ends_with("/message")
        && shared.hold_message_read.swap(false, Ordering::SeqCst)
    {
        shared.read_blocked.notify_one();
        shared.read_release.notified().await;
    }
    if method == "GET" && path == "/api/info" && shared.hold_info_read.swap(false, Ordering::SeqCst)
    {
        shared.info_blocked.notify_one();
        shared.info_release.notified().await;
    }
    // C2：可控阻塞 form detail（等待期间 pause/重开，旧答案不得返回新监听）。
    if method == "GET"
        && path.ends_with("/form/frm-1")
        && shared.hold_form_detail.swap(false, Ordering::SeqCst)
    {
        shared.form_detail_blocked.notify_one();
        shared.form_detail_release.notified().await;
    }

    let (status, body) = reply(&shared, &method, &path, &body_text);
    let reply = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(reply.as_bytes()).await;
    let _ = stream.flush().await;
}

/// 最小可投影应答集（形状与 `services/opencode_native` 的读取器逐字段对齐）。
///
/// `method` / `body_text` 只用于本文件的阻塞窗口用例：prompt 写会回显到 message 列表，
/// 因此 send 的写后回读可以真的定性为 Confirmed（不是靠测试自证）。
fn reply(shared: &Shared, method: &str, path: &str, body_text: &str) -> (u16, String) {
    if path == "/openapi.json" {
        return (200, shared.contract.clone());
    }
    if path == "/api/info" {
        let reported = match shared.info_pid.load(Ordering::SeqCst) {
            0 => PEER_PID,
            other => other as u32,
        };
        return (
            200,
            serde_json::json!({
                "version": PEER_VERSION,
                "pid": reported,
                "urls": [shared.endpoint],
            })
            .to_string(),
        );
    }
    let is_b = shared.info_pid.load(Ordering::SeqCst) == 9999;
    if path == "/api/agent" {
        let agent = if is_b { "agent-b-secret" } else { "agent-a" };
        return (
            200,
            serde_json::json!({"data":[{"id":agent,"name":agent,"mode":"primary"}]}).to_string(),
        );
    }
    let Some(rest) = path.strip_prefix("/api/session/") else {
        return (404, "{}".to_string());
    };
    let mut parts = rest.split('/');
    let session_id = parts.next().unwrap_or("");
    let session = serde_json::json!({
        "id": session_id,
        "agent": if is_b { "agent-b-secret" } else { "agent-a" },
        "location": { "directory": shared.directory },
    })
    .to_string();
    match parts.next() {
        // 会话读：`bind_session` 的三重核对要用 id + location.directory
        None => (200, session),
        Some("interrupt") if method == "POST" => {
            (200, serde_json::json!({"interrupted": true}).to_string())
        }
        Some("message") => (
            200,
            serde_json::json!({
                "data": shared.prompts.lock().unwrap().clone(),
                "cursor": { "previous": null, "next": null },
            })
            .to_string(),
        ),
        Some("prompt") if method == "POST" => {
            let body: serde_json::Value = serde_json::from_str(body_text).unwrap_or_default();
            let Some(id) = body.get("id").and_then(serde_json::Value::as_str) else {
                return (400, "{}".to_string());
            };
            // R5c：回读形态（顶层 files，条目带 data/mime/source.uri）——写请求里是
            // `{uri,name}`，这里模拟服务端读入后内联 base64 的真实回读形状。
            let files = body
                .get("files")
                .and_then(serde_json::Value::as_array)
                .map(|files| {
                    serde_json::Value::Array(
                        files
                            .iter()
                            .map(|file| {
                                use base64::Engine;
                                let uri = file["uri"].as_str().expect("附件 URI");
                                let path =
                                    reqwest::Url::parse(uri).unwrap().to_file_path().unwrap();
                                let bytes = std::fs::read(&path).expect("peer 真读受控附件");
                                let mime =
                                    if path.extension().and_then(|e| e.to_str()) == Some("png") {
                                        "image/png"
                                    } else {
                                        "text/plain"
                                    };
                                serde_json::json!({
                                    "name": file.get("name").cloned().unwrap_or_default(),
                                    "mime": mime,
                                    "data": base64::engine::general_purpose::STANDARD.encode(bytes),
                                    "source": {
                                        "type": "uri",
                                        "uri": file.get("uri").cloned().unwrap_or_default(),
                                    },
                                })
                            })
                            .collect(),
                    )
                });
            let mut record = serde_json::json!({
                "type": "user",
                "id": id,
                "text": body.get("text").cloned().unwrap_or_default(),
                "time": { "created": 1 },
            });
            if let Some(files) = files {
                record["files"] = files;
            }
            shared.prompts.lock().unwrap().push(record);
            (200, serde_json::json!({ "id": id }).to_string())
        }
        Some("form") => match parts.next() {
            Some("frm-conditional") => {
                let mut form = shared.conditional_form.lock().unwrap();
                if let Some(form) = form.as_mut() {
                    if method == "POST" {
                        let body: serde_json::Value = serde_json::from_str(body_text).unwrap();
                        form["state"] = serde_json::json!({"status":"answered", "answer":body["answer"]});
                        (204, String::new())
                    } else { (200, serde_json::json!({"data":form}).to_string()) }
                } else { (404, "{}".to_string()) }
            }
            None if shared.conditional_form.lock().unwrap().is_some() => {
                (200, serde_json::json!({"data":[shared.conditional_form.lock().unwrap().as_ref().unwrap()]}).to_string())
            }
            // 表单列表：本夹具不需要 pending 表单（结果回读走 detail，不依赖列表）。
            None => (200, serde_json::json!({ "data": [] }).to_string()),
            // R5b：已答表单的 detail（权威状态 + 官方答案）。表单属于 ses-1：
            // 用别的会话读同一 id 时 peer 自己就 404（模拟服务端按会话隔离）。
            Some("frm-1") if session_id == "ses-1" => (
                200,
                serde_json::json!({
                    "data": {
                        "id": "frm-1",
                        "sessionID": session_id,
                        "title": "Questions",
                        "fields": [{
                            "key": "q0",
                            "title": "COLOR",
                            "type": "string",
                            "required": true,
                            "options": [
                                { "value": "red", "label": "red" },
                                { "value": "blue", "label": "blue" }
                            ],
                            "custom": true,
                        }],
                        "state": { "status": "answered", "answer": { "q0": if is_b { "form-b-secret" } else { "blue" } } },
                    }
                })
                .to_string(),
            ),
            // 读失败：路由必须与「不存在」同码（404），不泄露存在性。
            Some("frm-read-fail") if session_id == "ses-1" => (500, "{}".to_string()),
            Some(_) => (404, "{}".to_string()),
        },
        Some("permission") | Some("inbox") => (200, serde_json::json!({ "data": [] }).to_string()),
        _ => (404, "{}".to_string()),
    }
}

// ============================================================
// 记录型假注入器：断言「终端收到什么」的唯一事实源
// ============================================================

#[derive(Default)]
struct SpyInjector {
    texts: StdMutex<Vec<String>>,
    keys: StdMutex<Vec<String>>,
}

impl SpyInjector {
    fn arc() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn texts(&self) -> Vec<String> {
        self.texts.lock().unwrap().clone()
    }

    fn keys(&self) -> Vec<String> {
        self.keys.lock().unwrap().clone()
    }
}

impl Injector for SpyInjector {
    fn name(&self) -> &'static str {
        "spy"
    }

    fn locate_and_inject(&self, _pid: u32, text: &str) -> Result<(), String> {
        self.texts.lock().unwrap().push(text.to_string());
        Ok(())
    }

    fn locate_and_send_key_inner(&self, _pid: u32, key: &str) -> Result<(), String> {
        self.keys.lock().unwrap().push(key.to_string());
        Ok(())
    }
}

// ============================================================
// fixture
// ============================================================

type SharedConn = Arc<StdMutex<rusqlite::Connection>>;

const COOKIE: &str = "mam_device=dev-1";
const COOKIE_OTHER: &str = "mam_device=dev-2";
const COOKIE_EXPIRED: &str = "mam_device=dev-old";
/// 设备 TTL 是 180 天：用远古时间戳造「绑定过期」。
const STALE_SEEN_AT: i64 = 1_000;

/// 验证开关的三种真实形态（I-4.4：缺省必须被真正构造过）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Switch {
    /// settings 表里**没有**这个键（生产默认形态）
    Absent,
    On,
}

fn shared_conn(_path: &Path) -> SharedConn {
    let conn = rusqlite::Connection::open_in_memory().expect("open in-memory db");
    multi_agents_manager_lib::database::schema::init(&conn).unwrap();
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
        paired_at: chrono::Utc::now().timestamp_millis(),
    };
    let store = DeviceStore::Owned(conn.clone());
    store.with(|c| persist_device(c, &dev).expect("seed device"));
}

fn opencode_session(id: &str, dir: &Path) -> Session {
    Session {
        id: id.to_string(),
        agent_type: AgentType::OpenCode,
        project_name: "proj".into(),
        project_path: dir.to_string_lossy().into_owned(),
        title: None,
        git_branch: None,
        github_url: None,
        status: SessionStatus::Waiting,
        last_message: None,
        last_message_role: None,
        last_message_subagent_report: false,
        flap_from_subagent_activity: false,
        last_activity_at: "2026-10-30T00:00:00Z".into(),
        pid: PEER_PID,
        cpu_usage: 0.0,
        unread_turned_green_at_ms: None,
        active_subagent_count: 0,
        form: ProcessForm::Cli,
        jump_supported: false,
        unread: false,
    }
}

fn codex_session(id: &str, dir: &Path) -> Session {
    let mut s = opencode_session(id, dir);
    s.agent_type = AgentType::Codex;
    s
}

/// 问答在场（通道 B 形态判据）：最后一条是 tool-call，其 args 可被 `parse_questions`
/// 解析，其后无 tool-result（与 `codex_ipc_remote.rs` 同款夹具）。
fn question_page() -> multi_agents_manager_lib::remote::content::MessagesPage {
    use multi_agents_manager_lib::remote::content::{MessagesPage, SessionMessage};
    MessagesPage {
        messages: vec![SessionMessage {
            seq: 0,
            role: "assistant".into(),
            kind: "tool-call".into(),
            content: "question".into(),
            ts: Some(1),
            tool_name: Some("question".into()),
            tool_args: Some(
                r#"{"questions":[{"question":"挑一个","options":[{"label":"甲"},{"label":"乙"}]}]}"#
                    .into(),
            ),
            collapsed: false,
        }],
        truncated: false,
        cwd: None,
    }
}

#[derive(Default)]
struct ScanGate {
    armed: AtomicBool,
    entered: tokio::sync::Notify,
    released: StdMutex<bool>,
    wake: std::sync::Condvar,
}

impl ScanGate {
    fn wait_if_armed(&self) {
        if self.armed.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            let released = self.released.lock().unwrap();
            let (released, _) = self
                .wake
                .wait_timeout_while(released, std::time::Duration::from_secs(10), |released| {
                    !*released
                })
                .unwrap();
            assert!(*released, "测试必须放行受控扫描");
        }
    }

    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.wake.notify_one();
    }
}

struct Fixture {
    state: Arc<RemoteState>,
    app: axum::Router,
    peer: Peer,
    injector: Arc<SpyInjector>,
    screen_reads: Arc<AtomicUsize>,
    hide_first_scan: Arc<AtomicBool>,
    hide_cards: Arc<AtomicBool>,
    scan_override: Arc<StdMutex<Option<Vec<Session>>>>,
    scan_gate: Arc<ScanGate>,
    dir: PathBuf,
    _temp: tempfile::TempDir,
}

impl Fixture {
    /// `build` 拿到**已 realpath** 的项目目录（会话卡片与假 peer 必须同值，绑定才成立）。
    async fn new(switch: Switch, build: impl FnOnce(&Path) -> Vec<Session>) -> Self {
        Self::new_with_ownership_gate(switch, build, false).await
    }

    async fn new_with_ownership_gate(
        switch: Switch,
        build: impl FnOnce(&Path) -> Vec<Session>,
        production_gate: bool,
    ) -> Self {
        let temp = tempfile::tempdir().expect("临时目录");
        let dir = temp.path().join("project ");
        std::fs::create_dir_all(&dir).expect("项目目录");
        let dir = dir.canonicalize().expect("项目目录 realpath");
        let peer = Peer::start(&dir).await;

        // 登记文件：只指向回环假 peer（不触碰真实 ~/.local/state/opencode）
        let state_file = temp.path().join("service.json");
        std::fs::write(
            &state_file,
            format!(
                r#"{{"id":"{PEER_SERVICE_ID}","version":"{PEER_VERSION}","url":"{}","pid":{PEER_PID},"password":"pw-fixture"}}"#,
                peer.endpoint
            ),
        )
        .expect("写登记文件");

        let conn = shared_conn(&temp.path().join("test.sqlite"));
        seed_device(&conn, "dev-1", "iPhone");
        seed_device(&conn, "dev-2", "iPad");
        seed_device(&conn, "dev-old", "旧手机");
        // 绑定过期（last_seen_at 远早于 TTL）
        conn.lock()
            .unwrap()
            .execute(
                "UPDATE remote_devices SET last_seen_at = ?2 WHERE id = ?1",
                rusqlite::params!["dev-old", STALE_SEEN_AT],
            )
            .expect("过期设备");
        if switch != Switch::Absent {
            let value = "true";
            conn.lock()
                .unwrap()
                .execute(
                    "INSERT OR REPLACE INTO settings (key, value) \
                     VALUES ('remote.opencode_native_enabled', ?1)",
                    [value],
                )
                .expect("写验证开关");
        }

        conn.lock().unwrap().execute("INSERT INTO settings (key,value) VALUES ('remote.opencode_interaction_policy','legacy_compatible')", []).unwrap();
        let sessions = build(&dir);
        let store = DeviceStore::Owned(conn.clone());
        // 旧成功用例测试底层协调器；R2 用例走生产服务身份门（with_sources）。
        // 冲突探针确定性关闭：本文件只考证据链判定，不考进程表扫描。
        let hub = Arc::new(if production_gate {
            NativeHub::with_sources(
                DeviceStore::Owned(conn.clone()),
                DiscoveryInput {
                    state_file,
                    home: temp.path().to_path_buf(),
                },
                Arc::new(|_pid: u32| Some(PEER_STARTED_AT_MS)),
                Some(Arc::new(|_dir: &Path| false)),
                None,
            )
        } else {
            NativeHub::with_coordinator_sources(
                DeviceStore::Owned(conn.clone()),
                DiscoveryInput {
                    state_file,
                    home: temp.path().to_path_buf(),
                },
                Arc::new(|_pid: u32| Some(PEER_STARTED_AT_MS)),
            )
        });
        let injector = SpyInjector::arc();
        let sessions_for_state = sessions.clone();
        let screen_reads = Arc::new(AtomicUsize::new(0));
        let screen_spy = screen_reads.clone();
        let hide_first_scan = Arc::new(AtomicBool::new(false));
        let hide_cards = Arc::new(AtomicBool::new(false));
        let scan_override = Arc::new(StdMutex::new(None::<Vec<Session>>));
        let scan_for_state = scan_override.clone();
        let scan_for_fresh = scan_override.clone();
        let scan_gate = Arc::new(ScanGate::default());
        let gate_for_fresh = scan_gate.clone();
        let hidden = hide_cards.clone();
        let hidden_fresh = hide_cards.clone();
        let hide_scan = hide_first_scan.clone();
        let sessions_for_fresh = sessions.clone();
        let hide_for_fresh = hide_first_scan.clone();
        let index_home = temp.path().join("index-home");
        std::fs::create_dir_all(&index_home).unwrap();
        let state = Arc::new(RemoteState {
            ui_config_source: Box::new(|| None),
            subagent_source: std::collections::HashMap::new(),
            subagent_message_source: std::collections::HashMap::new(),
            capability_table: multi_agents_manager_lib::inject::capability::new_table(),
            session_source: Box::new(move || SessionsResponse {
                sessions: if hidden.load(Ordering::SeqCst) {
                    sessions_for_state
                        .iter()
                        .filter(|s| s.id != "ses-1")
                        .cloned()
                        .collect()
                } else if hide_scan.swap(false, Ordering::SeqCst) {
                    vec![]
                } else {
                    scan_for_state
                        .lock()
                        .unwrap()
                        .clone()
                        .unwrap_or_else(|| sessions_for_state.clone())
                },
                total_count: sessions_for_state.len(),
                waiting_count: 0,
            }),
            // 原生路由的会话解析走 fresh_session_source 缝：与 session_source 同源，
            // 且共享同一「首次可信快照缺失」控制（hide_scan 先到先消费）——缺快照
            // 语义必须对两条解析缝同时成立。
            fresh_session_source: Box::new(move || {
                gate_for_fresh.wait_if_armed();
                SessionsResponse {
                    sessions: if hidden_fresh.load(Ordering::SeqCst) {
                        sessions_for_fresh
                            .iter()
                            .filter(|s| s.id != "ses-1")
                            .cloned()
                            .collect()
                    } else if hide_for_fresh.swap(false, Ordering::SeqCst) {
                        vec![]
                    } else {
                        scan_for_fresh
                            .lock()
                            .unwrap()
                            .clone()
                            .unwrap_or_else(|| sessions_for_fresh.clone())
                    },
                    total_count: sessions_for_fresh.len(),
                    waiting_count: 0,
                }
            }),
            pairing_counter: Box::new(Vec::new),
            board_hidden_ids: Box::new(Vec::new),
            board_hidden_hide: Arc::new(|_| 0usize),
            board_hidden_unhide: Arc::new(|_| 0usize),
            unread_mark_read: Arc::new(|_, _| ()),
            session_close: Arc::new(|_| Ok(())),
            store,
            host_source: Box::new(|| serde_json::json!({ "host": { "name": "t" } })),
            message_source: Box::new(|_, _, _| Ok(question_page())),
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
            home_source: Box::new(move || Some(index_home.to_string_lossy().into_owned())),
            injector: injector.clone(),
            resume_spawner: Arc::new(|_: &multi_agents_manager_lib::inject::resume::SpawnSpec| {
                Ok(())
            }),
            create_hub: Arc::new(multi_agents_manager_lib::remote::server::CreateTaskHub::stub()),
            archive_source: Box::new(Vec::new),
            archive_delete: Arc::new(|_: Option<&str>| 0usize),
            confirm_probe: Arc::new(|_, _, _| true),
            dialog_probe: Arc::new(|_, _| None),
            screen_probe: Arc::new(move |_, _| {
                screen_spy.fetch_add(1, Ordering::SeqCst);
                None
            }),
            codex_control:
                multi_agents_manager_lib::remote::codex_ipc::CodexControlHub::disabled_arc(),
            desktop_language: Default::default(),
            opencode_native: hub,
        });
        let app = router(state.clone());
        Self {
            state,
            app,
            peer,
            injector,
            screen_reads,
            hide_first_scan,
            hide_cards,
            scan_override,
            scan_gate,
            dir,
            _temp: temp,
        }
    }

    /// 单会话 OpenCode fixture（绝大多数用例的形态）。
    async fn single(switch: Switch) -> Self {
        Self::new(switch, |dir| vec![opencode_session("ses-1", dir)]).await
    }

    fn set_switch(&self, on: bool) {
        let value = if on { "true" } else { "false" };
        self.state
            .store
            .with(|c| {
                c.execute(
                    "INSERT OR REPLACE INTO settings (key, value) \
                     VALUES ('remote.opencode_native_enabled', ?1)",
                    [value],
                )
            })
            .expect("写验证开关");
    }

    /// GET 帮手（send-info 等只读端点）：返回状态码 + 已解析 JSON。
    async fn get(&self, uri: &str) -> (axum::http::StatusCode, serde_json::Value) {
        let r = self
            .app
            .clone()
            .oneshot(req("GET", uri, Some(COOKIE), None))
            .await
            .unwrap();
        let status = r.status();
        let text = body_of(r).await;
        let body = serde_json::from_str(&text).unwrap_or(serde_json::json!({ "raw": text }));
        (status, body)
    }

    /// 生产认领入口：`GET /opencode/view`（开关开 ⇒ 投影成功即认领）。
    async fn view(&self, sid: &str) -> (axum::http::StatusCode, serde_json::Value) {
        let r = self
            .app
            .clone()
            .oneshot(req(
                "GET",
                &format!("/m/api/v1/opencode/view?session_id={sid}"),
                Some(COOKIE),
                None,
            ))
            .await
            .unwrap();
        let status = r.status();
        let text = body_of(r).await;
        let body = serde_json::from_str(&text).unwrap_or(serde_json::json!({ "raw": text }));
        (status, body)
    }

    fn route_state(&self, sid: &str) -> Option<String> {
        self.state.store.with(|c| {
            c.query_row(
                "SELECT route_state FROM opencode_native_routes WHERE session_id = ?1",
                [sid],
                |r| r.get::<_, String>(0),
            )
            .ok()
        })
    }

    fn pending_rows(&self, sid: &str) -> usize {
        self.state.store.with(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM inject_queue WHERE session_id = ?1 \
                 AND sent_at IS NULL AND failed_reason IS NULL",
                [sid],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0) as usize
        })
    }

    /// 直接落一行旧待发（模拟「切路由之前就已排队的旧项」）。
    fn seed_pending_row(&self, sid: &str, content: &str) -> i64 {
        self.state.store.with(|c| {
            c.execute(
                "INSERT INTO inject_queue (session_id, agent_type, device_id, device_name, \
                 content, enqueued_at) VALUES (?1, 'opencode', 'dev-1', 'iPhone', ?2, 1000)",
                rusqlite::params![sid, content],
            )
            .expect("落一行旧待发");
            c.last_insert_rowid()
        })
    }

    /// 落一行动作账本（`state` 决定它是否算「未决」：unconfirmed / acknowledged 算）。
    /// 默认用**当前代际**（svc-test / PEER_PID / PEER_STARTED_AT_MS）。
    fn seed_action(&self, action_id: &str, sid: &str, state: &str) {
        self.seed_action_generation(
            action_id,
            sid,
            state,
            PEER_SERVICE_ID,
            PEER_PID,
            PEER_STARTED_AT_MS,
        );
    }

    /// 指定代际的账本行：`service_id` / `pid` / `started_at` 属于**上一次服务**时，
    /// 该行永远无法再被权威回读收敛（评审 Blocker 2 的「代际已不可核对」）。
    fn seed_action_generation(
        &self,
        action_id: &str,
        sid: &str,
        state: &str,
        service_id: &str,
        pid: u32,
        started_at_ms: i64,
    ) {
        let reason = if state == "unconfirmed" {
            "timeout"
        } else {
            ""
        };
        self.state
            .store
            .with(|c| {
                c.execute(
                    "INSERT INTO opencode_native_actions (action_id, device_id, session_id, \
                 service_id, runtime_pid, runtime_started_at_ms, directory, operation, \
                 request_key, payload_sha256, occupancy_key, state, native_id, reason_code, \
                 dispatch_epoch, created_at, updated_at) \
                 VALUES (?1, 'dev-1', ?2, ?3, ?4, ?5, ?6, 'send', NULL, 'sha', NULL, \
                 ?7, NULL, NULLIF(?8, ''), 'epoch-test', 1, 1)",
                    rusqlite::params![
                        action_id,
                        sid,
                        service_id,
                        i64::from(pid),
                        started_at_ms,
                        self.dir.to_string_lossy(),
                        state,
                        reason
                    ],
                )
            })
            .expect("落一行动作账本");
    }

    /// 落一行「待答占用」（证明作废时同事务释放）。
    fn seed_claim(&self, action_id: &str, sid: &str, occupancy_key: &str) {
        self.state
            .store
            .with(|c| {
                c.execute(
                    "INSERT INTO opencode_native_claims (occupancy_key, action_id, session_id, \
                 created_at, updated_at) VALUES (?1, ?2, ?3, 1, 1)",
                    rusqlite::params![occupancy_key, action_id, sid],
                )
            })
            .expect("落一行占用");
    }

    /// `(state, reason_code, service_id)`：作废断言用。
    fn action_row(&self, action_id: &str) -> (String, Option<String>, String) {
        self.state.store.with(|c| {
            c.query_row(
                "SELECT state, reason_code, service_id FROM opencode_native_actions \
                 WHERE action_id = ?1",
                [action_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                },
            )
            .expect("动作行必须存在")
        })
    }

    /// 该 actionId 的账本行数：404 窗口里必须仍是 0（核对不得创建/释放账本）。
    fn action_rows(&self, action_id: &str) -> usize {
        self.state.store.with(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM opencode_native_actions WHERE action_id = ?1",
                [action_id],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0) as usize
        })
    }

    fn claim_rows(&self, action_id: &str) -> usize {
        self.state.store.with(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM opencode_native_claims WHERE action_id = ?1",
                [action_id],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(0) as usize
        })
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

async fn body_of(resp: axum::http::Response<axum::body::Body>) -> String {
    use http_body_util::BodyExt;
    String::from_utf8(
        resp.into_body()
            .collect()
            .await
            .expect("读响应体")
            .to_bytes()
            .to_vec(),
    )
    .expect("utf8")
}

async fn upload(f: &Fixture, token: Option<&str>) -> (u16, String) {
    let route = if token.is_some() {
        "&interaction_route=opencode_native"
    } else {
        ""
    };
    let mut request = axum::http::Request::builder()
        .method("POST")
        .uri(format!(
            "/m/api/v1/session-attachment?session_id=ses-1&name=sample.txt{route}"
        ))
        .header("cookie", COOKIE);
    if let Some(token) = token {
        request = request.header("x-opencode-expected-binding", token);
    }
    let response = f
        .app
        .clone()
        .oneshot(
            request
                .body(axum::body::Body::from("attachment bytes"))
                .unwrap(),
        )
        .await
        .unwrap();
    (response.status().as_u16(), body_of(response).await)
}

fn prepare_git(dir: &Path) {
    std::fs::create_dir_all(dir.join(".git/info")).unwrap();
    std::fs::write(dir.join(".git/info/exclude"), b"existing-rule\n").unwrap();
}

fn assert_uploaded_to_a(f: &Fixture, other: &Path, status: u16, body: &str, tool: &str) {
    let trimmed = PathBuf::from(f.dir.to_str().unwrap().trim_end());
    let path = f.dir.join(".tuvis-attachments/ses-1/sample.txt");
    let a_bytes = std::fs::read(&path).ok();
    let b_exists = other.join(".tuvis-attachments").exists();
    let trimmed_exists = trimmed.join(".tuvis-attachments").exists();
    let index = multi_agents_manager_lib::remote::attachments::read_index(
        &f._temp.path().join("index-home"),
    );
    let index_matches = index.len() == 1
        && index[0].project == f.dir.to_str().unwrap()
        && index[0].path == path.to_str().unwrap()
        && index[0].tool == tool
        && index[0].session == "ses-1";
    assert!(status == 200 && a_bytes.as_deref() == Some(b"attachment bytes".as_slice())
        && !b_exists && !trimmed_exists && index_matches,
        "上传必须只写 A 并索引 A；状态={status} 响应={body} A内容={a_bytes:?} B有附件={b_exists} 去空格目录有附件={trimmed_exists} 索引={index:?}");
    assert_eq!(
        std::fs::read(f.dir.join(".git/info/exclude")).unwrap(),
        b"existing-rule\n.tuvis-attachments/\n"
    );
}

async fn native_upload_after_scan_change(hidden: bool) {
    let f = Fixture::single(Switch::On).await;
    assert!(f.dir.to_str().unwrap().ends_with(' '));
    prepare_git(&f.dir);
    let other = f._temp.path().join("project-b");
    std::fs::create_dir_all(&other).unwrap();
    let other = other.canonicalize().unwrap();
    prepare_git(&other);
    let trimmed = PathBuf::from(f.dir.to_str().unwrap().trim_end());
    std::fs::create_dir_all(&trimmed).unwrap();
    prepare_git(&trimmed);
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "先选中可信注入的 A: {view}");
    assert_eq!(view["route"], "opencode_native");
    let token = view["bindingToken"].as_str().unwrap().to_owned();
    *f.scan_override.lock().unwrap() = Some(if hidden {
        vec![]
    } else {
        vec![opencode_session("ses-1", &other)]
    });
    let (status, body) = upload(&f, Some(&token)).await;
    assert_uploaded_to_a(&f, &other, status, &body, "opencode");
    assert_eq!(
        std::fs::read(other.join(".git/info/exclude")).unwrap(),
        b"existing-rule\n"
    );
    assert_eq!(
        std::fs::read(trimmed.join(".git/info/exclude")).unwrap(),
        b"existing-rule\n"
    );
    assert_eq!(f.peer.writes(), 0);
}

#[tokio::test]
async fn native_upload_preserves_trailing_space_in_original_directory() {
    let f = Fixture::single(Switch::On).await;
    prepare_git(&f.dir);
    let trimmed = PathBuf::from(f.dir.to_str().unwrap().trim_end());
    std::fs::create_dir_all(&trimmed).unwrap();
    prepare_git(&trimmed);
    let other = f._temp.path().join("unused-project");
    std::fs::create_dir_all(&other).unwrap();
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "先选中可信注入的 A: {view}");
    assert_eq!(view["route"], "opencode_native");
    let token = view["bindingToken"].as_str().unwrap();
    let (status, body) = upload(&f, Some(token)).await;
    assert_uploaded_to_a(&f, &other, status, &body, "opencode");
}

#[tokio::test]
async fn native_upload_keeps_original_target_when_scan_replaces_a_with_b() {
    native_upload_after_scan_change(false).await;
}

#[tokio::test]
async fn native_upload_keeps_original_target_when_a_disappears_from_scan() {
    native_upload_after_scan_change(true).await;
}

async fn shared_upload_adds_missing_git_exclude(agent: AgentType, tool: &str) {
    let f = Fixture::new(Switch::On, |dir| {
        let mut s = opencode_session("ses-1", dir);
        s.agent_type = agent;
        vec![s]
    })
    .await;
    // 共享路径单独使用无尾空格的真实目录，以聚焦已有 exclude 追加行为。
    let project = f._temp.path().join("shared-project");
    std::fs::create_dir_all(&project).unwrap();
    let project = project.canonicalize().unwrap();
    prepare_git(&project);
    let mut session = opencode_session("ses-1", &project);
    session.agent_type = match tool {
        "claude" => AgentType::Claude,
        _ => AgentType::Codex,
    };
    *f.scan_override.lock().unwrap() = Some(vec![session]);
    let (status, body) = upload(&f, None).await;
    assert_eq!(status, 200, "{tool} 共享上传应成功: {body}");
    assert_eq!(
        std::fs::read(project.join(".tuvis-attachments/ses-1/sample.txt")).unwrap(),
        b"attachment bytes"
    );
    let index = multi_agents_manager_lib::remote::attachments::read_index(
        &f._temp.path().join("index-home"),
    );
    assert_eq!(index.len(), 1);
    assert_eq!(index[0].project, project.to_str().unwrap());
    assert_eq!(index[0].tool, tool);
    assert_eq!(
        std::fs::read(project.join(".git/info/exclude")).unwrap(),
        b"existing-rule\n.tuvis-attachments/\n"
    );
}

#[tokio::test]
async fn claude_shared_upload_adds_missing_rule_to_existing_exclude() {
    shared_upload_adds_missing_git_exclude(AgentType::Claude, "claude").await;
}

#[tokio::test]
async fn codex_shared_upload_adds_missing_rule_to_existing_exclude() {
    shared_upload_adds_missing_git_exclude(AgentType::Codex, "codex").await;
}
