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
//! 7. `POST /opencode/route/legacy`（I-1）：非 macOS 成功交回 → 旧写入口复活；macOS 拒绝旧写；未决动作 → 409 零变更；
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

#[path = "support/opencode_readonly_verify_red.rs"]
mod readonly_verify_red;
mod support;

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
    emit_event: tokio::sync::Notify,
    event_sent: tokio::sync::Notify,
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
            emit_event: tokio::sync::Notify::new(),
            event_sent: tokio::sync::Notify::new(),
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
        loop {
            tokio::select! {
                _ = shared.disconnect_events.notified() => break,
                result = stream.read(&mut sink) => {
                    if !matches!(result, Ok(n) if n > 0) { break; }
                },
                _ = shared.emit_event.notified() => {
                    if stream.write_all(b"data: {\"type\":\"session.updated\"}\n\n").await.is_err() { break; }
                    if stream.flush().await.is_err() { break; }
                    shared.event_sent.notify_one();
                },
            }
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

fn shared_conn(path: &Path) -> SharedConn {
    let conn = rusqlite::Connection::open(path).expect("open temporary db");
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
        dsh_instance_id: None,
        dsh_subagent: None,
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
        older_before: None,
        dsh_model_summary: None,
        messages: vec![SessionMessage {
            blocks: Vec::new(),
            media: Vec::new(),
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
        support::setup();
        let temp = tempfile::tempdir().expect("临时目录");
        let dir = temp.path().join("project");
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
        let state = Arc::new(RemoteState {
            dsh_control: multi_agents_manager_lib::services::dsh_control::disabled_arc(),
            dsh_auto_adapt_source: Arc::new(|| true),
            dsh_console_mount: None,
            dsh_console_upstream_source: None,
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
            unread_mark_read: Arc::new(|_, _, _| ()),
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
            home_source: Box::new(|| None),
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

#[path = "support/opencode_http.rs"]
mod opencode_http;
use opencode_http::req;

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

fn send_body(sid: &str, text: &str, queue_only: bool) -> String {
    serde_json::json!({ "sessionId": sid, "text": text, "queueOnly": queue_only }).to_string()
}

fn command_body(sid: &str, action_id: &str, binding_token: &str) -> String {
    serde_json::json!({
        "actionId": action_id,
        "sessionId": sid,
        "bindingToken": binding_token,
        "operation": {
            "kind": "send",
            "text": "原生消息",
            "attachmentPaths": [],
            "delivery": "queue",
            "clientMessageId": format!("msg_{action_id}"),
        }
    })
    .to_string()
}

const NATIVE_ROUTES: [(&str, &str); 7] = [
    ("GET", "/m/api/v1/opencode/view?session_id=ses-1"),
    ("GET", "/m/api/v1/opencode/events?session_id=ses-1"),
    ("POST", "/m/api/v1/opencode/actions"),
    ("GET", "/m/api/v1/opencode/actions/act-1"),
    ("POST", "/m/api/v1/opencode/actions/act-1/verify"),
    ("GET", "/m/api/v1/opencode/forms/frm-1?session_id=ses-1"),
    ("POST", "/m/api/v1/opencode/route/legacy"),
];

async fn post(
    app: &axum::Router,
    uri: &str,
    cookie: &str,
    body: String,
) -> (axum::http::StatusCode, String) {
    let r = app
        .clone()
        .oneshot(req("POST", uri, Some(cookie), Some(body)))
        .await
        .unwrap();
    let status = r.status();
    (status, body_of(r).await)
}

async fn pinned_events(f: &Fixture, token: &str) -> axum::http::Response<axum::body::Body> {
    let uri = format!("/m/api/v1/opencode/events?session_id=ses-1&expected_binding_token={token}");
    f.app
        .clone()
        .oneshot(req("GET", &uri, Some(COOKIE), None))
        .await
        .unwrap()
}

async fn wait_for_event_connections(peer: &Peer, count: usize) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let notified = peer.shared.event_connected.notified();
            if peer.shared.event_connections.load(Ordering::SeqCst) >= count {
                break;
            }
            notified.await;
        }
    })
    .await
    .expect("上游事件流必须真正建立");
}

#[tokio::test]
async fn pinned_stream_ends_after_upstream_disconnect_and_failed_reconnect() {
    use http_body_util::BodyExt;
    let (f, token) = readonly_unproven_fixture().await;
    let response = pinned_events(&f, &token).await;
    assert_eq!(response.status(), 200, "A 的 pinned 下游必须建立");
    wait_for_event_connections(&f.peer, 1).await;

    // 真断链并拒绝后续连接；没有发送任何业务事件或 revision。
    f.peer.shared.offline.store(true, Ordering::SeqCst);
    f.peer.shared.disconnect_events.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let notified = f.peer.shared.offline_connected.notified();
            if f.peer.shared.offline_connections.load(Ordering::SeqCst) > 0 {
                break;
            }
            notified.await;
        }
    })
    .await
    .expect("断流后必须观察到上游重连失败");
    let ended = tokio::time::timeout(
        std::time::Duration::from_secs(6),
        response.into_body().collect(),
    )
    .await;
    f.peer.shared.offline.store(false, Ordering::SeqCst);

    // 即使旧流未结束，也检查同代际能否重新发现和订阅。
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "恢复后应重新发现 A：{view}");
    assert_eq!(view["serviceId"], PEER_SERVICE_ID);
    assert_eq!(view["bindingToken"], token, "恢复的是同一代际");
    let fresh = pinned_events(&f, &token).await;
    assert_eq!(fresh.status(), 200, "同代际应可重新订阅");
    wait_for_event_connections(&f.peer, 2).await;
    assert_eq!(f.peer.writes(), 0, "整个过程不能向上游发送写请求");
    let body = ended
        .expect("上游断开且重连失败后，无业务 revision 的旧下游必须主动结束")
        .expect("读取旧流");
    let text = String::from_utf8(body.to_bytes().to_vec()).unwrap();
    assert!(
        text.matches("event: revision").count() <= 1,
        "断流过程中不得凭空产生业务 revision：{text}"
    );
}

// 夹具来自 opencode_native_remote：单独快照避免并发修改共享测试。
// 生产归属门无桌面连接证据；没有多服务冲突，服务代际与登记一致。
async fn readonly_unproven_fixture() -> (Fixture, String) {
    let f = Fixture::new_with_ownership_gate(
        Switch::On,
        |dir| vec![opencode_session("ses-1", dir)],
        true,
    )
    .await;
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "归属未证但可验证代际时首次投影仍只读：{view}");
    assert_eq!(view["serviceId"], PEER_SERVICE_ID);
    assert_ne!(view["route"], "opencode_native", "不得认领未证的归属");
    assert_eq!(view["routeReasonCode"], "OC_INSTANCE_OWNERSHIP_UNVERIFIED");
    assert_eq!(f.route_state("ses-1"), None);
    let token = view["bindingToken"]
        .as_str()
        .expect("只读投影必须携带绑定")
        .to_string();
    (f, token)
}

#[tokio::test]
async fn readonly_unproven_attachment_upload_rejects_without_changing_project() {
    let (f, token) = readonly_unproven_fixture().await;
    let info = f.dir.join(".git/info");
    std::fs::create_dir_all(&info).unwrap();
    let exclude = info.join("exclude");
    std::fs::write(&exclude, b"existing-rule\n").unwrap();
    std::fs::write(f.dir.join("existing.txt"), b"untouched").unwrap();

    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/m/api/v1/session-attachment?session_id=ses-1&name=sample.txt&interaction_route=opencode_native")
        .header("cookie", COOKIE)
        .header("x-opencode-expected-binding", &token)
        .body(axum::body::Body::from("attachment bytes"))
        .unwrap();
    let response = f.app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body = body_of(response).await;
    let entries: std::collections::BTreeSet<_> = std::fs::read_dir(&f.dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    let expected = [".git", "existing.txt"]
        .into_iter()
        .map(std::ffi::OsString::from)
        .collect();
    let exclude_after = std::fs::read(&exclude).unwrap();
    let existing_after = std::fs::read(f.dir.join("existing.txt")).unwrap();
    assert_eq!(
        (status.as_u16(), &entries, &exclude_after, &existing_after),
        (
            409,
            &expected,
            &b"existing-rule\n".to_vec(),
            &b"untouched".to_vec()
        ),
        "归属未证的上传必须拒绝且不能改项目或 exclude；响应 {body}"
    );
    assert!(body.contains("OC_INSTANCE_OWNERSHIP_UNVERIFIED"), "{body}");
    assert_eq!(f.peer.writes(), 0);
}

async fn attachment_request(f: &Fixture, query: &str, binding: Option<&str>) -> (u16, String) {
    let mut request = axum::http::Request::builder()
        .method("POST")
        .uri(format!("/m/api/v1/session-attachment?{query}"))
        .header("cookie", COOKIE);
    if let Some(token) = binding {
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

fn attachment_assert_no_project_writes(f: &Fixture) {
    assert!(
        !f.dir.join(".tuvis-attachments").exists(),
        "拒绝请求不能创建附件目录"
    );
    assert_eq!(
        std::fs::read(f.dir.join(".git/info/exclude")).unwrap(),
        b"existing-rule\n",
        "拒绝请求不能修改 exclude"
    );
}

fn attachment_prepare_project(f: &Fixture) {
    std::fs::create_dir_all(f.dir.join(".git/info")).unwrap();
    std::fs::write(f.dir.join(".git/info/exclude"), b"existing-rule\n").unwrap();
}

#[tokio::test]
async fn attachment_native_only_rejects_legacy_without_disk_writes() {
    let f = Fixture::single(Switch::On).await;
    f.state.store.with(|conn| conn.execute("UPDATE settings SET value='native_only' WHERE key='remote.opencode_interaction_policy'", []).unwrap());
    attachment_prepare_project(&f);
    let (status, body) = attachment_request(
        &f,
        "session_id=ses-1&name=sample.txt&interaction_route=legacy",
        None,
    )
    .await;
    attachment_assert_no_project_writes(&f);
    assert_eq!(status, 409, "native_only 不能绕到旧路径：{body}");
}

#[tokio::test]
async fn attachment_native_only_rejects_implicit_legacy_without_disk_writes() {
    let f = Fixture::single(Switch::On).await;
    f.state.store.with(|conn| conn.execute("UPDATE settings SET value='native_only' WHERE key='remote.opencode_interaction_policy'", []).unwrap());
    attachment_prepare_project(&f);
    let (status, body) = attachment_request(&f, "session_id=ses-1&name=sample.txt", None).await;
    attachment_assert_no_project_writes(&f);
    assert_eq!(
        status, 409,
        "native_only 不允许省略路由绕开原生门禁：{body}"
    );
}

#[tokio::test]
async fn attachment_native_route_with_matching_binding_uploads() {
    let f = Fixture::single(Switch::On).await;
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["route"], "opencode_native");
    let token = view["bindingToken"].as_str().unwrap();
    let (status, body) = attachment_request(
        &f,
        "session_id=ses-1&name=sample.txt&interaction_route=opencode_native",
        Some(token),
    )
    .await;
    assert_eq!(status, 200, "原绑定的原生上传：{body}");
    assert_eq!(
        std::fs::read(f.dir.join(".tuvis-attachments/ses-1/sample.txt")).unwrap(),
        b"attachment bytes"
    );
}

#[tokio::test]
async fn attachment_native_route_rejects_wrong_binding_without_disk_writes() {
    let f = Fixture::single(Switch::On).await;
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["route"], "opencode_native");
    attachment_prepare_project(&f);
    let (status, body) = attachment_request(
        &f,
        "session_id=ses-1&name=sample.txt&interaction_route=opencode_native",
        Some("wrong-binding"),
    )
    .await;
    attachment_assert_no_project_writes(&f);
    assert_ne!(status, 200, "绑定不匹配不能上传：{body}");
}

#[cfg(not(target_os = "macos"))]
#[tokio::test]
async fn attachment_legacy_compatible_without_active_route_uploads() {
    let f = Fixture::single(Switch::On).await;
    assert_eq!(f.route_state("ses-1"), None);
    let (status, body) = attachment_request(&f, "session_id=ses-1&name=sample.txt", None).await;
    assert_eq!(status, 200, "合法旧上传：{body}");
    assert_eq!(
        std::fs::read(f.dir.join(".tuvis-attachments/ses-1/sample.txt")).unwrap(),
        b"attachment bytes"
    );
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn attachment_legacy_compatible_without_active_route_rejects_without_writes() {
    let f = Fixture::single(Switch::On).await;
    attachment_prepare_project(&f);
    assert_eq!(f.route_state("ses-1"), None);
    let (status, body) = attachment_request(&f, "session_id=ses-1&name=sample.txt", None).await;
    assert_eq!(status, 409, "macOS 不允许旧上传：{body}");
    assert!(body.contains("OC_LEGACY_DISABLED"), "{body}");
    attachment_assert_no_project_writes(&f);
    assert_eq!(f.peer.writes(), 0);
}

#[tokio::test]
async fn attachment_other_tool_upload_remains_available_under_native_only() {
    let f = Fixture::new(Switch::On, |dir| vec![codex_session("ses-1", dir)]).await;
    f.state.store.with(|conn| conn.execute("UPDATE settings SET value='native_only' WHERE key='remote.opencode_interaction_policy'", []).unwrap());
    let (status, body) = attachment_request(&f, "session_id=ses-1&name=sample.txt", None).await;
    assert_eq!(status, 200, "其他工具不受 OpenCode 策略影响：{body}");
    assert_eq!(
        std::fs::read(f.dir.join(".tuvis-attachments/ses-1/sample.txt")).unwrap(),
        b"attachment bytes"
    );
}

#[tokio::test]
async fn attachment_active_native_route_rejects_legacy_without_disk_writes() {
    let f = Fixture::single(Switch::On).await;
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["route"], "opencode_native", "测试必须先认领原生路由");
    attachment_prepare_project(&f);
    let (status, body) = attachment_request(
        &f,
        "session_id=ses-1&name=sample.txt&interaction_route=legacy",
        None,
    )
    .await;
    attachment_assert_no_project_writes(&f);
    assert_eq!(status, 409, "活动原生路由不能绕到旧路径：{body}");
}

#[tokio::test]
async fn attachment_conflicting_route_and_binding_are_rejected_without_disk_writes() {
    let (f, token) = readonly_unproven_fixture().await;
    attachment_prepare_project(&f);
    let (status, body) = attachment_request(
        &f,
        "session_id=ses-1&name=sample.txt&interaction_route=legacy",
        Some(&token),
    )
    .await;
    attachment_assert_no_project_writes(&f);
    assert_eq!(status, 400, "旧路由与原生绑定冲突：{body}");
}

#[tokio::test]
async fn attachment_duplicate_conflicting_routes_are_rejected_without_disk_writes() {
    let f = Fixture::single(Switch::On).await;
    attachment_prepare_project(&f);
    let (status, body) = attachment_request(&f, "session_id=ses-1&name=sample.txt&interaction_route=opencode_native&interaction_route=legacy", None).await;
    attachment_assert_no_project_writes(&f);
    assert_eq!(status, 400, "重复且冲突的路由参数：{body}");
}

#[tokio::test]
async fn readonly_unproven_capabilities_block_every_native_write_with_ownership_reason() {
    let (f, _) = readonly_unproven_fixture().await;
    let (_, view) = f.view("ses-1").await;
    let capabilities = &view["capabilities"];
    let mut failures = Vec::new();
    for operation in [
        "send",
        "retract",
        "permission_once",
        "permission_reject",
        "answer_group",
        "stop_session",
        "set_agent",
        "permission_always",
    ] {
        let item = &capabilities[operation];
        if item["allowed"] != false || item["reasonCode"] != "OC_INSTANCE_OWNERSHIP_UNVERIFIED" {
            failures.push(format!("{operation}: {item}"));
        }
    }
    assert!(
        failures.is_empty(),
        "归属未证的写能力必须禁用并给出原因：{}",
        failures.join(", ")
    );
    assert_eq!(f.peer.writes(), 0);
}

#[tokio::test]
async fn readonly_unproven_capabilities_preserve_contract_reads() {
    let (f, _) = readonly_unproven_fixture().await;
    let (_, view) = f.view("ses-1").await;
    for operation in ["read_messages", "read_inbox", "read_pending"] {
        assert_eq!(
            view["capabilities"][operation]["allowed"], true,
            "读取能力 {operation}: {view}"
        );
    }
    assert_eq!(f.peer.writes(), 0);
}

#[tokio::test]
async fn readonly_unproven_local_queue_can_be_paused() {
    let (f, token) = readonly_unproven_fixture().await;
    let (status, body) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        serde_json::json!({
            "actionId":"readonly_pause", "sessionId":"ses-1", "bindingToken":token,
            "operation":{"kind":"set_outbox_paused","paused":true,"expectedRevision":0}
        })
        .to_string(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let (status, outbox) =
        get_with_binding(&f, "/m/api/v1/opencode/outbox?session_id=ses-1", &token).await;
    assert_eq!(status, 200);
    assert_eq!(outbox["paused"], true);
    assert_eq!(f.peer.writes(), 0);
}

#[tokio::test]
async fn readonly_pause_does_not_authorize_resume_without_ownership() {
    let (f, token) = readonly_unproven_fixture().await;
    let pause = serde_json::json!({"actionId":"pause_before_resume","sessionId":"ses-1","bindingToken":token,"operation":{"kind":"set_outbox_paused","paused":true,"expectedRevision":0}}).to_string();
    let (status, body) = post(&f.app, "/m/api/v1/opencode/actions", COOKIE, pause).await;
    assert_eq!(status, 200, "只读预暂停必须成功：{body}");
    let resume = serde_json::json!({"actionId":"unowned_resume","sessionId":"ses-1","bindingToken":token,"operation":{"kind":"set_outbox_paused","paused":false,"expectedRevision":1}}).to_string();
    let (status, body) = post(&f.app, "/m/api/v1/opencode/actions", COOKIE, resume).await;
    assert_eq!(status, 409, "恢复派发仍须证明归属：{body}");
    assert!(body.contains("OC_INSTANCE_OWNERSHIP_UNVERIFIED"), "{body}");
    let (status, snapshot) =
        get_with_binding(&f, "/m/api/v1/opencode/outbox?session_id=ses-1", &token).await;
    assert_eq!(status, 200, "同一 binding 必须能读取暂停状态：{snapshot}");
    assert_eq!(snapshot["paused"], true);
    assert_eq!(f.peer.writes(), 0);
}

fn reopened_hub(f: &Fixture) -> NativeHub {
    let conn = shared_conn(&f._temp.path().join("test.sqlite"));
    NativeHub::with_coordinator_sources(
        DeviceStore::Owned(conn),
        DiscoveryInput {
            state_file: f._temp.path().join("service.json"),
            home: f._temp.path().to_path_buf(),
        },
        Arc::new(|_: u32| Some(PEER_STARTED_AT_MS)),
    )
}

#[tokio::test]
async fn paused_queue_does_not_transfer_to_same_id_in_other_directory_after_restart() {
    let f = Fixture::single(Switch::On).await;
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "A 的视图必须可读：{view}");
    let token = view["bindingToken"].as_str().unwrap();
    let pause = serde_json::json!({"actionId":"pause_a","sessionId":"ses-1","bindingToken":token,"operation":{"kind":"set_outbox_paused","paused":true,"expectedRevision":0}}).to_string();
    let (status, body) = post(&f.app, "/m/api/v1/opencode/actions", COOKIE, pause).await;
    assert_eq!(status, 200, "A 的预暂停必须成功：{body}");
    f.seed_action("queued_a", "ses-1", "queued");
    f.state.store.with(|c| c.execute("INSERT INTO opencode_outbox(action_id,session_id,command_json,text_summary,attachment_count) VALUES ('queued_a','ses-1','{}','A only',0)", []).unwrap());
    let b_dir = f._temp.path().join("project-b");
    std::fs::create_dir_all(&b_dir).unwrap();
    let a = opencode_session("ses-1", &f.dir);
    let b = opencode_session("ses-1", &b_dir.canonicalize().unwrap());
    let a_before = f.state.opencode_native.outbox_view(&a).unwrap();
    let b_before = f.state.opencode_native.outbox_view(&b).unwrap();
    let hub = reopened_hub(&f);
    let a_after = hub.outbox_view(&a).unwrap();
    let b_after = hub.outbox_view(&b).unwrap();
    assert!(a_before.paused && a_after.paused, "A 的暂停必须跨重启保留");
    assert_eq!(a_before.items.len(), 1);
    assert_eq!(a_after.items.len(), 1, "A 的原队列必须保留");
    assert!(
        !b_before.paused && !b_after.paused,
        "B 不得继承 A 的暂停，包括重启后"
    );
    assert!(
        b_before.items.is_empty() && b_after.items.is_empty(),
        "B 不得看到 A 的原队列"
    );
    assert_eq!(f.peer.writes(), 0);
}

#[tokio::test]
async fn original_queue_is_not_visible_in_another_directory_after_restart() {
    let f = Fixture::single(Switch::On).await;
    f.seed_action("queued_a_only", "ses-1", "queued");
    f.state.store.with(|c| c.execute("INSERT INTO opencode_outbox(action_id,session_id,command_json,text_summary,attachment_count) VALUES ('queued_a_only','ses-1','{}','A only',0)", []).unwrap());
    let b_dir = f._temp.path().join("project-b");
    std::fs::create_dir_all(&b_dir).unwrap();
    let a = opencode_session("ses-1", &f.dir);
    let b = opencode_session("ses-1", &b_dir.canonicalize().unwrap());
    let hub = reopened_hub(&f);
    assert_eq!(
        hub.outbox_view(&a).unwrap().items.len(),
        1,
        "A 的原队列必须保留"
    );
    assert!(
        hub.outbox_view(&b).unwrap().items.is_empty(),
        "B 不得看到 A 的原队列"
    );
    assert_eq!(f.peer.writes(), 0);
}

#[tokio::test]
async fn legacy_session_only_pause_is_not_assigned_to_an_arbitrary_directory() {
    let f = Fixture::single(Switch::On).await;
    f.state.store.with(|c| {
        c.execute(
            "INSERT INTO opencode_outbox_sessions(session_id,revision,paused) VALUES ('ses-1',7,1)",
            [],
        )
        .unwrap();
        multi_agents_manager_lib::database::migration::migrate(c).unwrap();
        multi_agents_manager_lib::database::migration::migrate(c).unwrap();
    });
    let b_dir = f._temp.path().join("project-b");
    std::fs::create_dir_all(&b_dir).unwrap();
    let a = opencode_session("ses-1", &f.dir);
    let b = opencode_session("ses-1", &b_dir.canonicalize().unwrap());
    let hub = reopened_hub(&f);
    assert!(
        !hub.outbox_view(&a).unwrap().paused,
        "旧状态缺少目标证据，不能猜成 A"
    );
    assert!(
        !hub.outbox_view(&b).unwrap().paused,
        "旧状态缺少目标证据，不能猜成 B"
    );
    assert_eq!(f.peer.writes(), 0);
}

#[tokio::test]
async fn readonly_unproven_local_queue_item_can_be_retracted() {
    let (f, token) = readonly_unproven_fixture().await;
    f.seed_action("readonly_queued", "ses-1", "queued");
    f.state.store.with(|c| c.execute("INSERT INTO opencode_outbox(action_id,session_id,command_json,text_summary,attachment_count) VALUES ('readonly_queued','ses-1','{}','queued',0)", []).unwrap());
    let (status, body) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        serde_json::json!({
            "actionId":"readonly_retract", "sessionId":"ses-1", "bindingToken":token,
            "operation":{"kind":"retract_outbox","targetActionId":"readonly_queued"}
        })
        .to_string(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let (status, outbox) =
        get_with_binding(&f, "/m/api/v1/opencode/outbox?session_id=ses-1", &token).await;
    assert_eq!(status, 200);
    assert_eq!(outbox["items"][0]["state"], "not_sent");
    assert_eq!(outbox["items"][0]["reasonCode"], "outbox_retracted");
    assert_eq!(f.peer.writes(), 0);
}

async fn get_with_binding(
    f: &Fixture,
    path: &str,
    token: &str,
) -> (axum::http::StatusCode, serde_json::Value) {
    let (mut parts, body) = req("GET", path, Some(COOKIE), None).into_parts();
    parts
        .headers
        .insert("x-opencode-expected-binding", token.parse().unwrap());
    let response = f
        .app
        .clone()
        .oneshot(axum::http::Request::from_parts(parts, body))
        .await
        .unwrap();
    let status = response.status();
    let body = body_of(response).await;
    (status, serde_json::from_str(&body).expect("JSON 响应"))
}

/// 建立并关闭唯一的下游订阅，观察上游连接实际关闭，再等待 pump 的排空期限。
async fn readonly_after_idle_reap() -> (Fixture, String) {
    let (f, token) = readonly_unproven_fixture().await;
    let stream = pinned_events(&f, &token).await;
    assert_eq!(stream.status(), 200, "首次 SSE 必须建立");
    wait_for_event_connections(&f.peer, 1).await;
    drop(stream);
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let notified = f.peer.shared.event_disconnected.notified();
            if f.peer.shared.event_disconnections.load(Ordering::SeqCst) > 0 {
                break;
            }
            notified.await;
        }
    })
    .await
    .expect("最后订阅关闭后上游必须正常回收");
    // 连接关闭发生在 pump 完成排空之前；留足生产排空上限以观察回收后的状态。
    tokio::time::sleep(std::time::Duration::from_millis(700)).await;
    assert_eq!(f.route_state("ses-1"), None);
    (f, token)
}

#[tokio::test]
async fn readonly_idle_reap_original_token_restores_view_on_same_target() {
    let (f, token) = readonly_after_idle_reap().await;
    let (status, view) =
        get_with_binding(&f, "/m/api/v1/opencode/view?session_id=ses-1", &token).await;
    assert_eq!(
        status, 200,
        "同一 A 空闲回收后原 token 必须恢复视图：{view}"
    );
    assert_eq!(view["serviceId"], PEER_SERVICE_ID);
    assert_eq!(view["bindingToken"], token);
    assert_eq!(f.route_state("ses-1"), None);
    assert_eq!(f.peer.writes(), 0);
}

#[tokio::test]
async fn readonly_idle_reap_original_token_restores_events_on_same_target() {
    let (f, token) = readonly_after_idle_reap().await;
    let stream = pinned_events(&f, &token).await;
    assert_eq!(
        stream.status(),
        200,
        "同一 A 空闲回收后原 token 必须恢复 SSE"
    );
    wait_for_event_connections(&f.peer, 2).await;
    assert_eq!(f.route_state("ses-1"), None);
    assert_eq!(f.peer.writes(), 0);
}

async fn readonly_idle_reap_with_replacement() -> (Fixture, String, Peer) {
    let (f, token) = readonly_after_idle_reap().await;
    let b_dir = f._temp.path().join("project-b");
    std::fs::create_dir_all(&b_dir).unwrap();
    let b_dir = b_dir.canonicalize().unwrap();
    let b = Peer::start(&b_dir).await;
    b.set_info_pid(9999);
    let mut card = opencode_session("ses-1", &b_dir);
    card.pid = 9999;
    *f.scan_override.lock().unwrap() = Some(vec![card]);
    std::fs::write(f._temp.path().join("service.json"), format!(
        r#"{{"id":"svc-b","version":"{PEER_VERSION}","url":"{}","pid":9999,"password":"pw-fixture"}}"#, b.endpoint
    )).unwrap();
    (f, token, b)
}

#[tokio::test]
async fn readonly_idle_reap_original_token_keeps_a_when_scan_finds_replacement() {
    let (f, token, b) = readonly_idle_reap_with_replacement().await;
    let (status, view) =
        get_with_binding(&f, "/m/api/v1/opencode/view?session_id=ses-1", &token).await;
    assert_eq!(status, 200, "A 仍可核验时原 token 必须读取 A：{view}");
    assert_eq!(view["bindingToken"], token);
    assert_eq!(view["directory"], f.dir.to_string_lossy().as_ref());
    assert_eq!(view["serviceId"], PEER_SERVICE_ID);
    assert_eq!(view["servicePid"], PEER_PID);
    assert_eq!(
        view["serviceUrl"].as_str().unwrap().trim_end_matches('/'),
        f.peer.endpoint
    );
    assert_ne!(view["route"], "opencode_native");
    assert_eq!(view["routeReasonCode"], "OC_INSTANCE_OWNERSHIP_UNVERIFIED");
    assert_eq!(f.route_state("ses-1"), None);
    let events = pinned_events(&f, &token).await;
    assert_eq!(events.status(), 200, "原 token 必须仍能订阅 A");
    wait_for_event_connections(&f.peer, 2).await;
    assert_eq!(b.requests(), 0, "B 不得收到任何业务读取或其他请求");
    assert_eq!(b.shared.event_connections.load(Ordering::SeqCst), 0);
    assert_eq!(b.writes(), 0);
    assert_eq!(f.peer.writes(), 0);
    assert_eq!(f.route_state("ses-1"), None);
}

#[tokio::test]
async fn readonly_idle_reap_rejects_original_token_when_a_is_offline_and_b_replaces_it() {
    let (f, token, b) = readonly_idle_reap_with_replacement().await;
    f.peer.shared.offline.store(true, Ordering::SeqCst);
    let (status, view) =
        get_with_binding(&f, "/m/api/v1/opencode/view?session_id=ses-1", &token).await;
    assert_eq!(status, 409, "A 已离线必须受控拒绝，不能改读 B：{view}");
    assert!(
        view["reasonCode"]
            .as_str()
            .is_some_and(|code| code.starts_with("OC_")),
        "必须返回受控原因：{view}"
    );
    assert!(
        f.peer.shared.offline_connections.load(Ordering::SeqCst) > 0,
        "必须实际尝试并发现 A 离线"
    );
    let events = pinned_events(&f, &token).await;
    let status = events.status();
    let body = body_of(events).await;
    assert_eq!(status, 409, "A 已离线不能订阅 B：{body}");
    let error: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(
        error["reasonCode"]
            .as_str()
            .is_some_and(|code| code.starts_with("OC_")),
        "必须返回受控原因：{body}"
    );
    assert_eq!(b.requests(), 0, "A 失效时也不得读取或订阅 B");
    assert_eq!(b.shared.event_connections.load(Ordering::SeqCst), 0);
    assert_eq!(b.writes(), 0);
    assert_eq!(f.peer.writes(), 0);
    assert_eq!(f.route_state("ses-1"), None);
}

#[tokio::test]
async fn readonly_unproven_view_keeps_initial_binding() {
    let (f, token) = readonly_unproven_fixture().await;
    let (status, view) =
        get_with_binding(&f, "/m/api/v1/opencode/view?session_id=ses-1", &token).await;
    assert_eq!(status, 200, "只读刷新必须保留 A：{view}");
    assert_eq!(view["serviceId"], PEER_SERVICE_ID);
    assert_eq!(view["bindingToken"], token);
    assert_eq!(f.route_state("ses-1"), None);
    assert_eq!(f.peer.writes(), 0);
}

#[tokio::test]
async fn readonly_unproven_form_keeps_initial_binding() {
    let (f, token) = readonly_unproven_fixture().await;
    let (status, form) = get_with_binding(
        &f,
        "/m/api/v1/opencode/forms/frm-1?session_id=ses-1",
        &token,
    )
    .await;
    assert_eq!(status, 200, "只读表单必须属于 A：{form}");
    assert_eq!(form["answers"]["q0"]["value"], "blue");
    assert_eq!(f.route_state("ses-1"), None);
    assert_eq!(f.peer.writes(), 0);
}

#[tokio::test]
async fn readonly_unproven_agents_keep_initial_binding() {
    let (f, token) = readonly_unproven_fixture().await;
    let (status, agents) =
        get_with_binding(&f, "/m/api/v1/opencode/agents?session_id=ses-1", &token).await;
    assert_eq!(status, 200, "只读 agent 必须属于 A：{agents}");
    assert_eq!(agents["current"], "agent-a");
    assert_eq!(f.route_state("ses-1"), None);
    assert_eq!(f.peer.writes(), 0);
}

async fn switched_to_b() -> (Fixture, Peer) {
    let f = Fixture::single(Switch::On).await;
    let (status, a) = f.view("ses-1").await;
    assert_eq!(status, 200, "先选择 A：{a}");
    assert_eq!(a["route"], "opencode_native");
    let b_dir = f._temp.path().join("project-b");
    std::fs::create_dir_all(&b_dir).unwrap();
    let b_dir = b_dir.canonicalize().unwrap();
    let b = Peer::start(&b_dir).await;
    b.set_info_pid(9999);
    let mut card = opencode_session("ses-1", &b_dir);
    card.pid = 9999;
    *f.scan_override.lock().unwrap() = Some(vec![card]);
    std::fs::write(f._temp.path().join("service.json"), format!(
        r#"{{"id":"svc-b","version":"{PEER_VERSION}","url":"{}","pid":9999,"password":"pw-fixture"}}"#, b.endpoint
    )).unwrap();
    let token_a = a["bindingToken"].as_str().expect("A 的事务前绑定");
    let version_a = a["routeVersion"].as_i64().expect("A 视图的路由版本");
    #[cfg(target_os = "macos")]
    let returned_version = {
        // 恢复旧版本留下的 draining 历史，不借已关闭的旧写接口造夹具。
        let updated = f.state.store.with(|conn| {
            conn.execute(
                "UPDATE opencode_native_routes SET route_state = 'draining', route_version = route_version + 1 \
                 WHERE session_id = 'ses-1' AND route_state = 'active' AND route_version = ?1",
                [version_a],
            )
            .unwrap()
        });
        assert_eq!(updated, 1);
        version_a + 1
    };
    #[cfg(not(target_os = "macos"))]
    let returned_version = {
        let (status, body) = post(
            &f.app,
            "/m/api/v1/opencode/route/legacy",
            COOKIE,
            serde_json::json!({"sessionId":"ses-1", "bindingToken":token_a, "routeVersion":version_a})
                .to_string(),
        )
        .await;
        assert_eq!(status, 200, "交回 A：{body}");
        let returned: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(returned["bindingToken"], token_a, "交回的绑定必须是 A");
        returned["routeVersion"]
            .as_i64()
            .expect("交回事务的路由版本")
    };
    let preview = f
        .get("/m/api/v1/opencode/route/preview?sessionId=ses-1")
        .await;
    assert_eq!(preview.0, 200, "预览 B：{}", preview.1);
    let (status, body) = post(&f.app, "/m/api/v1/opencode/route/native", COOKIE, serde_json::json!({"sessionId":"ses-1", "bindingToken":token_a, "routeVersion":returned_version, "candidateToken":preview.1["previewToken"]}).to_string()).await;
    assert_eq!(status, 200, "另一请求显式选择 B：{body}");
    let selected: serde_json::Value = serde_json::from_str(&body).unwrap();
    let token_b = selected["bindingToken"]
        .as_str()
        .expect("选择事务返回 B 的绑定");
    assert_ne!(token_a, token_b, "B 的绑定必须不同于 A");
    let (status, b_view) =
        get_with_binding(&f, "/m/api/v1/opencode/view?session_id=ses-1", token_b).await;
    assert_eq!(status, 200, "B 可读：{b_view}");
    assert_eq!(b_view["serviceId"], "svc-b");
    assert_eq!(b_view["bindingToken"], token_b);
    (f, b)
}

async fn assert_missing_binding_refused(path: &str) {
    let (f, b) = switched_to_b().await;
    let response = f
        .app
        .clone()
        .oneshot(req("GET", path, Some(COOKIE), None))
        .await
        .unwrap();
    assert_eq!(response.status(), 409, "缺绑定不得默认读取或订阅 B：{path}");
    let text = body_of(response).await;
    assert!(text.contains("OC_BINDING_STALE"), "必须受控拒绝：{text}");
    assert!(!text.contains("form-b-secret") && !text.contains("agent-b-secret"));
    assert_eq!(b.writes(), 0);
}

#[tokio::test]
async fn missing_binding_cannot_read_replacement_form() {
    assert_missing_binding_refused("/m/api/v1/opencode/forms/frm-1?session_id=ses-1").await;
}

#[tokio::test]
async fn missing_binding_cannot_read_replacement_agents() {
    assert_missing_binding_refused("/m/api/v1/opencode/agents?session_id=ses-1").await;
}

#[tokio::test]
async fn missing_binding_cannot_subscribe_to_replacement_events() {
    assert_missing_binding_refused("/m/api/v1/opencode/events?session_id=ses-1").await;
}

#[tokio::test]
async fn native_selection_rejects_replacement_after_confirmed_readonly_preview() {
    let f = Fixture::single(Switch::On).await;
    let (status, a) = f.view("ses-1").await;
    assert_eq!(status, 200, "先建立旧路由：{a}");
    let token_a = a["bindingToken"].as_str().unwrap();
    let version = a["routeVersion"].as_i64().unwrap();
    let b = Peer::start(&f.dir).await;
    let c = Peer::start(&f.dir).await;
    let register = |service: &str, peer: &Peer| {
        std::fs::write(f._temp.path().join("service.json"), format!(
            r#"{{"id":"{service}","version":"{PEER_VERSION}","url":"{}","pid":{PEER_PID},"password":"pw-fixture"}}"#, peer.endpoint
        )).unwrap();
    };
    register("candidate-b", &b);
    let response = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            "/m/api/v1/opencode/route/preview?sessionId=ses-1",
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "预览 B 应成功");
    let preview_b: serde_json::Value = serde_json::from_str(&body_of(response).await).unwrap();
    assert_eq!(preview_b["candidate"]["serviceId"], "candidate-b");
    let token_b = preview_b["candidate"]["bindingToken"].as_str().unwrap();
    assert_ne!(token_b, token_a);
    let preview_token_b = preview_b["previewToken"].as_str().unwrap();
    let (status, body) = post(
        &f.app,
        "/m/api/v1/opencode/route/native",
        COOKIE,
        serde_json::json!({"sessionId":"ses-1", "bindingToken":token_a, "routeVersion":version, "candidateToken":preview_token_b}).to_string(),
    ).await;
    assert_eq!(status, 200, "确认 B 应成功：{body}");
    let selected: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        selected["bindingToken"], token_b,
        "确认结果必须绑定预览的 B"
    );
    let route = || {
        f.state.store.with(|conn| {
        conn.query_row("SELECT service_id,runtime_pid,runtime_started_at_ms,directory,route_state,route_version,claimed_at,updated_at FROM opencode_native_routes WHERE session_id='ses-1'", [], |r| Ok((r.get::<_, String>(0)?,r.get::<_, i64>(1)?,r.get::<_, i64>(2)?,r.get::<_, String>(3)?,r.get::<_, String>(4)?,r.get::<_, i64>(5)?,r.get::<_, i64>(6)?,r.get::<_, i64>(7)?))).unwrap()
    })
    };
    let before = route();
    assert_eq!(before.0, "candidate-b", "确认后路由必须指向 B");
    register("candidate-c", &c);
    let response = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            "/m/api/v1/opencode/route/preview?sessionId=ses-1",
            Some(COOKIE_OTHER),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "预览 C 应成功");
    let preview_c: serde_json::Value = serde_json::from_str(&body_of(response).await).unwrap();
    assert_eq!(preview_c["candidate"]["serviceId"], "candidate-c");
    let preview_token_c = preview_c["previewToken"].as_str().unwrap();
    let c_requests_before = c.requests();
    let (status, body) = post(
        &f.app,
        "/m/api/v1/opencode/route/native",
        COOKIE_OTHER,
        serde_json::json!({"sessionId":"ses-1", "bindingToken":token_a, "routeVersion":version, "candidateToken":preview_token_c})
            .to_string(),
    )
    .await;
    let after = route();
    assert_eq!(c.requests(), c_requests_before, "改投不得向 C 发送请求");
    assert_eq!((f.peer.writes(), b.writes(), c.writes()), (0, 0, 0));
    assert_eq!(after, before, "确认 B 后不得改投 C：{body}");
    assert_eq!(status, 409, "旧绑定及版本不能用于改投 C：{body}");
}

#[tokio::test]
async fn explicit_native_selection_returns_the_binding_selected_by_the_transaction() {
    let f = Fixture::single(Switch::On).await;
    let (preview_status, preview) = f
        .get("/m/api/v1/opencode/route/preview?sessionId=ses-1")
        .await;
    assert_eq!(preview_status, 200, "预览候选：{preview}");
    let (status, body) = post(
        &f.app,
        "/m/api/v1/opencode/route/native",
        COOKIE,
        serde_json::json!({"sessionId":"ses-1", "candidateToken":preview["previewToken"]})
            .to_string(),
    )
    .await;
    assert_eq!(status, 200, "选择必须成功：{body}");
    let selected: serde_json::Value = serde_json::from_str(&body).unwrap();
    let token = selected["bindingToken"]
        .as_str()
        .expect("事务必须返回实际选择的绑定");
    let (status, pinned) =
        get_with_binding(&f, "/m/api/v1/opencode/view?session_id=ses-1", token).await;
    assert_eq!(status, 200, "返回的绑定必须能读取刚选中的目标：{pinned}");
    assert_eq!(pinned["bindingToken"], token);
    assert_eq!(f.peer.writes(), 0);
}

#[tokio::test]
async fn legacy_return_rejects_a_stale_devices_binding() {
    let (f, b) = switched_to_b().await;
    let (_, b_view) = f.view("ses-1").await;
    let token_b = b_view["bindingToken"].as_str().unwrap();
    let version_b = b_view["routeVersion"].as_i64().expect("B 视图的路由版本");
    let (status, body) = post(&f.app, "/m/api/v1/opencode/route/legacy", COOKIE_OTHER, serde_json::json!({"sessionId":"ses-1", "bindingToken":"stale-device-a", "routeVersion":version_b}).to_string()).await;
    assert_eq!(f.route_state("ses-1").as_deref(), Some("active"));
    let (view_status, pinned) =
        get_with_binding(&f, "/m/api/v1/opencode/view?session_id=ses-1", token_b).await;
    assert_eq!(view_status, 200, "B 的绑定必须保留：{pinned}");
    assert_eq!(b.writes(), 0);
    assert_eq!(status, 409, "旧设备不能交回 B 的路由：{body}");
}

#[tokio::test]
async fn legacy_return_for_current_binding_obeys_platform_policy() {
    let f = Fixture::single(Switch::On).await;
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200);
    let token = view["bindingToken"].as_str().unwrap();
    let route_version = view["routeVersion"].as_i64().expect("视图的路由版本");
    let (status, body) = post(&f.app, "/m/api/v1/opencode/route/legacy", COOKIE, serde_json::json!({"sessionId":"ses-1", "bindingToken":token, "routeVersion":route_version}).to_string()).await;
    #[cfg(not(target_os = "macos"))]
    {
        assert_eq!(status, 200, "当前绑定应能显式交回：{body}");
        assert_eq!(f.route_state("ses-1").as_deref(), Some("draining"));
    }
    #[cfg(target_os = "macos")]
    {
        assert_eq!(status, 409, "macOS 不允许交回旧路径：{body}");
        assert!(body.contains("OC_LEGACY_DISABLED"), "{body}");
        assert_eq!(f.route_state("ses-1").as_deref(), Some("active"));
    }
    assert_eq!(f.peer.writes(), 0);
}

#[tokio::test]
async fn events_racing_first_view_cannot_open_unpinned_stream() {
    let f = Fixture::single(Switch::On).await;
    f.peer.hold_next_message_read();
    let app = f.app.clone();
    let first_view = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            "/m/api/v1/opencode/view?session_id=ses-1",
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    f.peer.wait_until_read_blocked().await;
    assert_eq!(f.route_state("ses-1"), None, "首个视图尚未返回或认领");
    let events = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            "/m/api/v1/opencode/events?session_id=ses-1",
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    let status = events.status();
    f.peer.release_message_read();
    let view = first_view.await.unwrap();
    assert_eq!(view.status(), 200, "首个 view 的正向对照");
    assert_eq!(status, 409, "首个 view 尚在途时不得建立无 pin 事件流");
    assert!(body_of(events).await.contains("OC_BINDING_STALE"));
    assert_eq!(f.peer.writes(), 0);
}
