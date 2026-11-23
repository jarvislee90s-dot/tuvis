#![cfg(target_os = "macos")]
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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use multi_agents_manager_lib::inject::engine::Injector;
use multi_agents_manager_lib::inject::queue::{flush_one, FlushOutcome};
use multi_agents_manager_lib::remote::pairing::{
    persist_device, revoke_device, DeviceStore, NewDevice,
};
use multi_agents_manager_lib::remote::server::{router, RemoteState, SseRegistry};
use multi_agents_manager_lib::services::opencode_native::{
    DiscoveryInput, NativeActionStore, NativeHub, NativeTarget, RuntimeKey,
};
use multi_agents_manager_lib::session::{
    AgentType, ProcessForm, Session, SessionStatus, SessionsResponse,
};
use tower::ServiceExt as _;

#[path = "support/opencode_route_preview_red.rs"]
mod opencode_route_preview_red;
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
    #[cfg(target_os = "macos")]
    create: StdMutex<Option<Arc<native_create_tests::CreatePeer>>>,
    writes: AtomicUsize,
    b_writes: AtomicUsize,
    offline: AtomicBool,
    unauthorized_next: AtomicBool,
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
    /// N1：按会话记账的 prompt 写（「A 原生正式记录恰一条、B 零条」的线上事实；
    /// 与回显用的 `prompts` 分开记，不扰动既有用例的读形状）。
    prompt_by_session: StdMutex<Vec<(String, String)>>,
    /// R2 冒名旋钮：非 0 时 `/api/info` 自报该 pid（登记滞留不变 = 同端口冒名形态）。
    info_pid: AtomicUsize,
    // ---- H03 echo-challenge 探针通道（写门用例专用；既有用例零触碰）----
    /// 探针会话 id（POST /api/session 恒返回它；GET/PATCH /api/session/{它} 走探针分支）。
    probe_id: String,
    /// 探针槽位当前标题（初始 `probe:tuvis`——非协议形态，对守卫是「无在飞」）。
    probe_title: StdMutex<String>,
    /// ack 旋钮：非 0 时收到 challenge 形态标题立即翻转为该 pid 的 ack（模拟锁定 TUI
    /// 即时应答）；0 = 不应答（零 ack 用例）。
    gate_ack_pid: AtomicUsize,
    /// ack 预算：>0 时每应答一次递减，归零后不再应答（entry 过 / dispatch 拒的用例）。
    gate_ack_budget: AtomicUsize,
    /// 置位后探针 PATCH 一律 500（探针不可写用例）。
    probe_unwritable: AtomicBool,
    /// 探针通道写计数（POST /api/session + PATCH 探针标题；与用户会话写分离取证）。
    probe_writes: AtomicUsize,
    /// 置位后探针会话读/写一律 404（明确删除 → 受控重建路径用例）。
    probe_deleted: AtomicBool,
    /// 置位后 prompt POST 一律 409（写被服务端拒绝 → Unconfirmed 动作用例）。
    prompt_refuse: AtomicBool,
    /// I1 确定性时序：置位后 PATCH 挑战先通知测试并挂起，直到放行。
    gate_hold_ack: AtomicBool,
    /// hold-ack 的「已挂起」信号与「放行」信号。
    gate_ack_held: tokio::sync::Notify,
    gate_ack_release: tokio::sync::Notify,
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
            #[cfg(target_os = "macos")]
            create: StdMutex::new(None),
            writes: AtomicUsize::new(0),
            b_writes: AtomicUsize::new(0),
            offline: AtomicBool::new(false),
            unauthorized_next: AtomicBool::new(false),
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
            prompt_by_session: StdMutex::new(Vec::new()),
            info_pid: AtomicUsize::new(0),
            probe_id: "ses-probe-fixture".to_string(),
            probe_title: StdMutex::new("probe:tuvis".to_string()),
            gate_ack_pid: AtomicUsize::new(0),
            gate_ack_budget: AtomicUsize::new(0),
            probe_unwritable: AtomicBool::new(false),
            probe_writes: AtomicUsize::new(0),
            probe_deleted: AtomicBool::new(false),
            prompt_refuse: AtomicBool::new(false),
            gate_hold_ack: AtomicBool::new(false),
            gate_ack_held: tokio::sync::Notify::new(),
            gate_ack_release: tokio::sync::Notify::new(),
        });
        let accepted = Arc::clone(&shared);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let shared = Arc::clone(&accepted);
                tokio::spawn(async move { serve_connection(stream, shared).await });
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

    /// N1：指定会话收到的 prompt 写条数（「A 恰一条、B 零条」按会话记账的线上事实）。
    fn session_prompt_writes(&self, sid: &str) -> usize {
        self.shared
            .prompt_by_session
            .lock()
            .unwrap()
            .iter()
            .filter(|(session, _)| session == sid)
            .count()
    }

    // ---- H03 echo-challenge 探针旋钮（写门用例专用）----

    /// 武装模拟锁定 TUI：收到 challenge 标题立即翻转为该 pid 的 ack。
    /// `budget` = Some(n) 时只应答 n 次（entry 过 / dispatch 拒的用例）。
    fn arm_gate_ack(&self, pid: u32, budget: Option<usize>) {
        self.shared
            .gate_ack_pid
            .store(pid as usize, Ordering::SeqCst);
        self.shared
            .gate_ack_budget
            .store(budget.unwrap_or(usize::MAX), Ordering::SeqCst);
    }

    /// 撤防：不再应答任何 challenge（零 ack 用例）。
    fn disarm_gate_ack(&self) {
        self.shared.gate_ack_pid.store(0, Ordering::SeqCst);
    }

    /// 探针 PATCH 一律 500（探针不可写用例）。
    fn break_probe_writes(&self) {
        self.shared.probe_unwritable.store(true, Ordering::SeqCst);
    }

    /// 探针通道写计数（POST /api/session + PATCH 探针标题）。
    fn probe_writes(&self) -> usize {
        self.shared.probe_writes.load(Ordering::SeqCst)
    }

    /// 探针槽位当前标题（取证用）。
    fn probe_title(&self) -> String {
        self.shared.probe_title.lock().unwrap().clone()
    }

    /// 探针会话读/写一律 404（明确删除 → 守卫受控重建路径）。
    fn delete_probe_session(&self) {
        self.shared.probe_deleted.store(true, Ordering::SeqCst);
    }

    /// prompt POST 一律 409（写被拒 → Unconfirmed 动作，M1 只读核对用例）。
    fn refuse_prompts(&self) {
        self.shared.prompt_refuse.store(true, Ordering::SeqCst);
    }

    /// I1 确定性时序：PATCH 挑战挂起直到 [`Peer::release_gate_ack`]。
    fn hold_gate_ack(&self) {
        self.shared.gate_hold_ack.store(true, Ordering::SeqCst);
    }

    async fn wait_gate_ack_held(&self) {
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            self.shared.gate_ack_held.notified(),
        )
        .await
        .expect("挑战必须在时限内挂起到位");
    }

    fn release_gate_ack(&self) {
        self.shared.gate_hold_ack.store(false, Ordering::SeqCst);
        self.shared.gate_ack_release.notify_one();
    }
}

async fn serve_connection(mut stream: tokio::net::TcpStream, shared: Arc<Shared>) {
    let mut buf = Vec::new();
    let mut first_request = true;
    while serve_one(&mut stream, &shared, &mut buf, first_request).await {
        first_request = false;
    }
}

async fn serve_one(
    stream: &mut tokio::net::TcpStream,
    shared: &Shared,
    buf: &mut Vec<u8>,
    first_request: bool,
) -> bool {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    // 可控断链：连接关闭而非伪造成功响应，覆盖已缓存 source 的健康复核。
    if shared.offline.load(Ordering::SeqCst) {
        return false;
    }
    let mut chunk = [0u8; 2048];
    let head_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return false,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
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
    let body_start = head_end + 4;
    let body_end = body_start + body_len;
    while buf.len() < body_end {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return false,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let body_text = String::from_utf8_lossy(&buf[body_start..body_end]).into_owned();
    buf.drain(..body_end);
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
            return false;
        }
        let _ = stream.flush().await;
        let mut sink = [0u8; 256];
        while matches!(stream.read(&mut sink).await, Ok(n) if n > 0) {}
        return false;
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

    // H03 I1：hold-ack 武装时，探针挑战写先通知测试并挂起（确定性时序旋钮）。
    if shared.gate_hold_ack.load(Ordering::SeqCst)
        && method == "PATCH"
        && path == format!("/api/session/{}", shared.probe_id)
        && body_text.contains("\"chal:")
    {
        shared.gate_ack_held.notify_one();
        shared.gate_ack_release.notified().await;
    }
    let response = || {
        if shared.unauthorized_next.swap(false, Ordering::SeqCst) {
            (401, "{}".to_string())
        } else {
            reply(shared, &method, &path, &body_text)
        }
    };
    #[cfg(target_os = "macos")]
    let create = shared.create.lock().unwrap().clone();
    #[cfg(not(target_os = "macos"))]
    let _ = first_request;
    let (status, body, keep_alive) = {
        #[cfg(target_os = "macos")]
        if let Some(create) = create {
            match create
                .reply(&method, &target, &body_text, first_request)
                .await
            {
                Some(reply) => reply,
                None => return false,
            }
        } else {
            let (status, body) = response();
            (status, body, false)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let (status, body) = response();
            (status, body, false)
        }
    };
    let connection = if keep_alive { "keep-alive" } else { "close" };
    let reply = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: {connection}\r\n\r\n{body}",
        body.len()
    );
    if stream.write_all(reply.as_bytes()).await.is_err() {
        return false;
    }
    stream.flush().await.is_ok() && keep_alive
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
    // ---- H03 echo-challenge 探针分支（必须在通用会话分支之前）----
    // POST /api/session：写门 ensure_probe 的创建调用（固定探针 id，创建一次复用）。
    if method == "POST" && path == "/api/session" {
        shared.probe_writes.fetch_add(1, Ordering::SeqCst);
        // 服务端重新确立探针会话：此前的「已删除」状态随之解除，标题回到受控初始值
        // （新建会话只带已知初始标题——与 binding_gate 的重建判据一致）。
        shared.probe_deleted.store(false, Ordering::SeqCst);
        *shared.probe_title.lock().unwrap() = "probe:tuvis".to_string();
        return (
            200,
            serde_json::json!({"data": {"id": shared.probe_id}}).to_string(),
        );
    }
    if path == format!("/api/session/{}", shared.probe_id) {
        // 探针会话明确删除（404）→ 守卫受控重建路径。
        if shared.probe_deleted.load(Ordering::SeqCst) {
            return (404, "{}".to_string());
        }
        // PATCH：探针标题覆盖写（b1/H02 实测 204 无 body）。模拟锁定 TUI：
        // challenge 形态 + ack 武装 → 立即翻转为该 pid 的 ack（acktime = 当前毫秒）。
        if method == "PATCH" {
            shared.probe_writes.fetch_add(1, Ordering::SeqCst);
            if shared.probe_unwritable.load(Ordering::SeqCst) {
                return (500, "{}".to_string());
            }
            let body: serde_json::Value = serde_json::from_str(body_text).unwrap_or_default();
            let title = body
                .get("title")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let mut slot = shared.probe_title.lock().unwrap();
            *slot = title.to_string();
            let ack_pid = shared.gate_ack_pid.load(Ordering::SeqCst) as u32;
            let budget = shared.gate_ack_budget.load(Ordering::SeqCst);
            let is_challenge = title.starts_with("chal:")
                && title.split(':').count() == 3
                && title.split(':').nth(1).is_some_and(|nonce| {
                    !nonce.is_empty()
                        && nonce.len() == 32
                        && nonce
                            .bytes()
                            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
                });
            if is_challenge && ack_pid != 0 && budget > 0 {
                shared.gate_ack_budget.store(budget - 1, Ordering::SeqCst);
                let acktime = chrono::Utc::now().timestamp_millis();
                let nonce = title.split(':').nth(1).unwrap_or("");
                *slot = format!("ack:{nonce}:{ack_pid}:{acktime}");
            }
            return (204, String::new());
        }
        // GET：{"data":{id,title,location}}（GET 包 data 一层 + 三重核对所需字段；
        // 与普通会话读同形，另含探针标题）。
        if method == "GET" {
            let title = shared.probe_title.lock().unwrap().clone();
            return (
                200,
                serde_json::json!({
                    "data": {
                        "id": shared.probe_id,
                        "title": title,
                        "agent": "agent-a",
                        "location": { "directory": shared.directory },
                    },
                })
                .to_string(),
            );
        }
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
            if shared.prompt_refuse.load(Ordering::SeqCst) {
                return (409, "{}".to_string());
            }
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
            shared
                .prompt_by_session
                .lock()
                .unwrap()
                .push((session_id.to_string(), id.to_string()));
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

fn shared_conn() -> SharedConn {
    let conn = rusqlite::Connection::open_in_memory().expect("open memory db");
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
    #[cfg(target_os = "macos")]
    resume_spawn_calls: Arc<AtomicUsize>,
    hide_first_scan: Arc<AtomicBool>,
    session_scans: Arc<AtomicUsize>,
    hide_cards: Arc<AtomicBool>,
    scan_override: Arc<StdMutex<Option<Vec<Session>>>>,
    scan_gate: Arc<ScanGate>,
    dir: PathBuf,
    /// 写门锁定 TUI 的存活开关（I1 确定性时序；未装门 = None）。
    gate_tui_alive: Option<Arc<AtomicBool>>,
    /// 写门控制文件根（`arm_binding_gate_for_tests` 的 home；未装门 = None）。
    gate_home: Option<PathBuf>,
    /// N1：可变窗口观察旋钮（装门夹具恒有；默认 = 锁定窗口新鲜 `Opened("ses-1")`，
    /// 与既有静态夹具行为逐字一致）。
    gate_opened: Option<GateObservation>,
    /// N1：锁定 pid 的**当前代际**（同 pid 进程替换模拟；fact 注入读此值）。
    gate_tui_generation: Option<Arc<AtomicI64>>,
    /// N2：可变的「锚定 TUI 构建 SHA」解析旋钮（装门夹具恒有）。缺省 = 任意 pid
    /// 返回登记构建（H02 锚）——既有用例的「已登记构建」世界逐字保留；用例可按
    /// pid 覆写（同版本不同构建）或注入解析失败（None = 取不到，fail-closed）。
    gate_binary_sha: Option<Arc<StdMutex<HashMap<u32, Option<String>>>>>,
    /// M2：可变的「服务构建 SHA」解析旋钮（装门夹具恒有）。缺省 = 任意 pid 返回
    /// 登记构建（H02 拓扑中服务与 TUI 同一二进制）；「只换服务构建」用例按
    /// descriptor.runtime.pid（夹具登记文件的 PEER_PID）覆写或注入解析失败。
    gate_service_sha: Option<Arc<StdMutex<HashMap<u32, Option<String>>>>>,
    /// M2：服务进程代际旋钮（启动毫秒可换代；SHA 缓存换代重算与恢复形态用）。
    service_generation: Arc<AtomicI64>,
    _temp: tempfile::TempDir,
}

/// H03 echo-challenge 写门用例的装配配置：锁定窗口（pid+启动时间）与窗口参数。
/// 控制文件根取夹具临时目录（真实 `~/.tuvis` 零读写）。
#[derive(Clone, Copy)]
struct GateArm {
    locked_pid: u32,
    started_at_ms: i64,
    window_ms: i64,
    poll_interval_ms: u64,
}

/// N1：可变的「窗口当前新鲜 Opened」观察模拟（与生产观察注册表同形——锁定窗口
/// 查询只认**当前** `Opened` 的会话与窗口身份；测试用旋钮切换，模拟同一 TUI 在
/// 同服务内浏览别的会话 / 另一窗口打开同一会话 / 观察消失）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct GateOpenedWindow {
    sid: String,
    pid: u32,
    started_at_ms: i64,
}

type GateObservation = Arc<StdMutex<Option<GateOpenedWindow>>>;

/// 写门用例的锁定身份常量：pid 与假 peer 服务 pid 无关（TUI 是另一个进程）。
const GATE_LOCKED_PID: u32 = 5150;
const GATE_STARTED_AT_MS: i64 = 1_000;
const GATE_BOOT: &str = "boot-gate-fixture";
/// N1 第二窗口常量（「两个窗口都打开 A 时的选择」用例；pid 更大 = 生产 min-pid
/// 确定性里**后来者**，不得因它自动换窗）。
const GATE_ALT_PID: u32 = 5200;
const GATE_ALT_STARTED_AT_MS: i64 = 2_000;
/// N2 语义证据准入：样本 B（同版本不同构建）的锁定窗口身份——独立进程/独立二进制，
/// 与假 peer 服务 pid 无关；HTTP 契约与版本与样本 A 完全一致（同一假 peer 同一
/// `PEER_VERSION`），只有构建 SHA 不在登记表内。
const GATE_UNPROVEN_PID: u32 = 5250;
const GATE_UNPROVEN_STARTED_AT_MS: i64 = 3_000;
/// N2 未登记构建的 SHA（小写 hex 形态合法，但不在 H02 登记表内）。
const GATE_UNREGISTERED_SHA: &str =
    "b7e5c1d09a44f2a68c3175e0b8d2f4a6193c7e58a0d4b6f2c8e1a3d5f7b9042c6";

impl Fixture {
    /// `build` 拿到**已 realpath** 的项目目录（会话卡片与假 peer 必须同值，绑定才成立）。
    async fn new(switch: Switch, build: impl FnOnce(&Path) -> Vec<Session>) -> Self {
        Self::new_with_ownership_gate(switch, build, false, None).await
    }

    /// H03 写门用例入口：**生产判定分支**（`production_gate=true` 的服务身份链 +
    /// 装配 echo-challenge 归属写门；只注入环境事实，不靠协调器跳闸——复审 C1）。
    async fn new_with_gate(switch: Switch, arm: GateArm) -> Self {
        Self::new_with_gate_sessions(switch, arm, |dir| vec![opencode_session("ses-1", dir)]).await
    }

    /// 同 [`Fixture::new_with_gate`]，会话集由调用方给定（探针目标用例要第二张卡）。
    async fn new_with_gate_sessions(
        switch: Switch,
        arm: GateArm,
        build: impl FnOnce(&Path) -> Vec<Session>,
    ) -> Self {
        Self::new_with_ownership_gate(switch, build, true, Some(arm)).await
    }

    async fn new_with_ownership_gate(
        switch: Switch,
        build: impl FnOnce(&Path) -> Vec<Session>,
        production_gate: bool,
        arm_gate: Option<GateArm>,
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

        let conn = shared_conn();
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
        // M2：服务进程代际旋钮（登记文件 pid 不变、启动毫秒可换代——服务 SHA 每代际
        // 缓存的换代重算与「复验登记 = 新代际跑登记构建」恢复形态用；缺省 = 既有
        // 静态值，既有用例世界不变）。
        let service_generation = Arc::new(AtomicI64::new(PEER_STARTED_AT_MS));
        let service_generation_for_fact = Arc::clone(&service_generation);
        // 旧成功用例测试底层协调器；R2 用例走生产服务身份门（with_sources）。
        // 冲突探针确定性关闭：本文件只考证据链判定，不考进程表扫描。
        let mut built = if production_gate {
            NativeHub::with_sources(
                DeviceStore::Owned(conn.clone()),
                DiscoveryInput {
                    state_file,
                    home: temp.path().to_path_buf(),
                },
                Arc::new(move |_pid: u32| {
                    let started_at_ms = service_generation_for_fact.load(Ordering::SeqCst);
                    (started_at_ms > 0).then_some(started_at_ms)
                }),
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
        };
        // H03：装配 echo-challenge 归属写门（注入锁定窗口与窗口参数；控制文件根 =
        // 夹具临时目录下的独立 home，真实 ~/.tuvis 零读写）。
        let gate_tui_alive = arm_gate.map(|_| Arc::new(AtomicBool::new(true)));
        let mut gate_home_dir: Option<PathBuf> = None;
        // N1：可变窗口观察（默认 = 锁定窗口新鲜 Opened("ses-1")——既有用例的静态
        // 行为逐字保留；需要模拟窗口导航/换窗的用例经 `set_gate_opened*` 旋钮切换）。
        let gate_opened: Option<GateObservation> = arm_gate.map(|gate| {
            Arc::new(StdMutex::new(Some(GateOpenedWindow {
                sid: "ses-1".to_string(),
                pid: gate.locked_pid,
                started_at_ms: gate.started_at_ms,
            })))
        });
        let gate_tui_generation: Option<Arc<AtomicI64>> =
            arm_gate.map(|gate| Arc::new(AtomicI64::new(gate.started_at_ms)));
        // N2：SHA 解析旋钮（缺省 = 任意 pid 返回登记构建 H02 锚；用例按 pid 覆写或
        // 注入 None 模拟解析失败）。M2：服务构建 SHA 旋钮同形（缺省登记构建）。
        let gate_binary_sha: Option<Arc<StdMutex<HashMap<u32, Option<String>>>>> =
            arm_gate.map(|_| Arc::new(StdMutex::new(HashMap::new())));
        let gate_service_sha: Option<Arc<StdMutex<HashMap<u32, Option<String>>>>> =
            arm_gate.map(|_| Arc::new(StdMutex::new(HashMap::new())));
        if let Some(gate) = arm_gate {
            let gate_home = temp.path().join("gate-home");
            std::fs::create_dir_all(&gate_home).expect("写门控制文件根");
            gate_home_dir = Some(gate_home.clone());
            let observation = gate_opened.clone().expect("装门夹具必有观察旋钮");
            let observation_for_lookup = Arc::clone(&observation);
            let alive_for_fact = gate_tui_alive.clone().unwrap();
            let fact_observation = Arc::clone(&observation);
            let locked_generation = gate_tui_generation.clone().expect("装门夹具必有代际旋钮");
            let sha_knob = gate_binary_sha.clone().expect("装门夹具必有 SHA 旋钮");
            let service_sha_knob = gate_service_sha.clone().expect("装门夹具必有服务 SHA 旋钮");
            built.arm_binding_gate_for_tests(
                // 与生产 `fresh_opened_window` 同形：只认**当前**新鲜 Opened 的会话，
                // 身份三元组（pid + 启动时刻 + boot）取自该窗口。
                Arc::new(move |sid: &str| {
                    let opened = observation_for_lookup.lock().unwrap().clone();
                    opened.filter(|w| w.sid == sid).map(|w| {
                        multi_agents_manager_lib::services::opencode_native::LockedWindow {
                            pid: w.pid,
                            started_at_ms: w.started_at_ms,
                            boot: Some(GATE_BOOT.to_string()),
                        }
                    })
                }),
                // 进程事实按 pid 取启动时刻（同生产 sysinfo 语义：任何存活 TUI 都有）。
                Arc::new(move |pid: u32| {
                    if !alive_for_fact.load(Ordering::SeqCst) {
                        return None;
                    }
                    if pid == gate.locked_pid {
                        return Some(locked_generation.load(Ordering::SeqCst));
                    }
                    fact_observation
                        .lock()
                        .unwrap()
                        .as_ref()
                        .filter(|w| w.pid == pid)
                        .map(|w| w.started_at_ms)
                }),
                // N2 锚定构建解析：按 pid 查覆写表（None = 解析失败）；未覆写的 pid
                // = 登记构建（既有用例世界不变）。
                Arc::new(move |pid: u32| {
                    sha_knob
                        .lock()
                        .unwrap()
                        .get(&pid)
                        .cloned()
                        .unwrap_or_else(|| {
                            Some(
                                multi_agents_manager_lib::services::opencode_native::attribution_evidence::H02_ANCHOR_SHA256.to_string(),
                            )
                        })
                }),
                // M2 服务构建解析：同形旋钮（缺省登记构建——H02 拓扑中服务与 TUI
                // 同一二进制；「只换服务构建」用例按 pid 覆写）。
                Arc::new(move |pid: u32| {
                    service_sha_knob
                        .lock()
                        .unwrap()
                        .get(&pid)
                        .cloned()
                        .unwrap_or_else(|| {
                            Some(
                                multi_agents_manager_lib::services::opencode_native::attribution_evidence::H02_ANCHOR_SHA256.to_string(),
                            )
                        })
                }),
                gate.window_ms,
                gate.poll_interval_ms,
                gate_home,
            );
        }
        let hub = Arc::new(built);
        let injector = SpyInjector::arc();
        let sessions_for_state = sessions.clone();
        let screen_reads = Arc::new(AtomicUsize::new(0));
        let screen_spy = screen_reads.clone();
        let resume_spawn_calls = Arc::new(AtomicUsize::new(0));
        let resume_spawn_spy = resume_spawn_calls.clone();
        let hide_first_scan = Arc::new(AtomicBool::new(false));
        let session_scans = Arc::new(AtomicUsize::new(0));
        let ordinary_scans = session_scans.clone();
        let fresh_scans = session_scans.clone();
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
            ui_config_source: Box::new(|| None),
            subagent_source: std::collections::HashMap::new(),
            subagent_message_source: std::collections::HashMap::new(),
            capability_table: multi_agents_manager_lib::inject::capability::new_table(),
            session_source: Box::new(move || {
                ordinary_scans.fetch_add(1, Ordering::SeqCst);
                SessionsResponse {
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
                }
            }),
            // 原生路由的会话解析走 fresh_session_source 缝：与 session_source 同源，
            // 且共享同一「首次可信快照缺失」控制（hide_scan 先到先消费）——缺快照
            // 语义必须对两条解析缝同时成立。
            fresh_session_source: Box::new(move || {
                fresh_scans.fetch_add(1, Ordering::SeqCst);
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
            home_source: Box::new(|| None),
            injector: injector.clone(),
            resume_spawner: Arc::new(
                move |_: &multi_agents_manager_lib::inject::resume::SpawnSpec| {
                    resume_spawn_spy.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                },
            ),
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
            #[cfg(target_os = "macos")]
            resume_spawn_calls,
            hide_first_scan,
            session_scans,
            hide_cards,
            scan_override,
            scan_gate,
            dir,
            gate_tui_alive,
            gate_home: gate_home_dir,
            gate_opened,
            gate_tui_generation,
            gate_binary_sha,
            gate_service_sha,
            service_generation,
            _temp: temp,
        }
    }

    // ---- N1 可变窗口观察旋钮（装门夹具；语义见 GateOpenedWindow）----

    /// 窗口存活开关（I1 终验判据；退出 = fact None，fail-closed）。
    fn set_gate_tui_alive(&self, alive: bool) {
        self.gate_tui_alive
            .as_ref()
            .expect("装门夹具必有存活开关")
            .store(alive, Ordering::SeqCst);
    }

    /// N1「PID/boot 替换」：同一 pid 换成新进程代际（新启动时刻；观察与进程事实
    /// 同步换代——生产语义 = 旧窗口消失、新窗口接管同一 pid 位）。
    fn replace_gate_window_generation(&self, new_started_at_ms: i64) {
        self.gate_tui_generation
            .as_ref()
            .expect("装门夹具必有代际旋钮")
            .store(new_started_at_ms, Ordering::SeqCst);
        let observation = self.gate_opened.as_ref().expect("装门夹具必有观察旋钮");
        let mut opened = observation.lock().unwrap();
        if let Some(window) = opened.as_mut() {
            window.started_at_ms = new_started_at_ms;
        }
    }

    fn set_gate_opened(&self, sid: &str) {
        let observation = self.gate_opened.as_ref().expect("装门夹具必有观察旋钮");
        let mut opened = observation.lock().unwrap();
        let previous = opened.clone().expect("切换前必须有在观察窗口");
        *opened = Some(GateOpenedWindow {
            sid: sid.to_string(),
            pid: previous.pid,
            started_at_ms: previous.started_at_ms,
        });
    }

    /// N1「两个窗口都打开 A 时的选择」：把当前观察切到另一扇窗口（更大 pid）。
    fn set_gate_opened_window(&self, sid: &str, pid: u32, started_at_ms: i64) {
        let observation = self.gate_opened.as_ref().expect("装门夹具必有观察旋钮");
        *observation.lock().unwrap() = Some(GateOpenedWindow {
            sid: sid.to_string(),
            pid,
            started_at_ms,
        });
    }

    /// N1「观察过期」：观察消失（无任何新鲜 Opened；窗口可能已退出前台读数）。
    fn clear_gate_opened(&self) {
        let observation = self.gate_opened.as_ref().expect("装门夹具必有观察旋钮");
        *observation.lock().unwrap() = None;
    }

    // ---- N2 语义证据准入旋钮（装门夹具；语义见 gate_binary_sha）----

    /// N2「同版本不同构建」：按 pid 覆写锚定 TUI 的构建 SHA（不在登记表内的构建）。
    fn set_gate_binary_sha(&self, pid: u32, sha: &str) {
        self.gate_binary_sha
            .as_ref()
            .expect("装门夹具必有 SHA 旋钮")
            .lock()
            .unwrap()
            .insert(pid, Some(sha.to_string()));
    }

    /// N2「错误形态」：按 pid 注入 SHA 解析失败（取不到 → 准入按不适用处理）。
    fn fail_gate_binary_sha(&self, pid: u32) {
        self.gate_binary_sha
            .as_ref()
            .expect("装门夹具必有 SHA 旋钮")
            .lock()
            .unwrap()
            .insert(pid, None);
    }

    /// N2：清除按 pid 覆写（回到登记构建缺省——复验重新登记后的恢复形态）。
    fn reset_gate_binary_sha(&self, pid: u32) {
        self.gate_binary_sha
            .as_ref()
            .expect("装门夹具必有 SHA 旋钮")
            .lock()
            .unwrap()
            .remove(&pid);
    }

    // ---- M2 服务构建 SHA 旋钮（装门夹具；语义见 gate_service_sha）----

    /// M2「只换服务构建」：按服务进程 pid 覆写服务构建 SHA（不在登记表内的构建；
    /// TUI 构建保持登记缺省）。
    fn set_gate_service_sha(&self, pid: u32, sha: &str) {
        self.gate_service_sha
            .as_ref()
            .expect("装门夹具必有服务 SHA 旋钮")
            .lock()
            .unwrap()
            .insert(pid, Some(sha.to_string()));
    }

    /// M2「错误形态」：按服务进程 pid 注入服务 SHA 解析失败（取不到 → 准入按
    /// 不适用处理）。
    fn fail_gate_service_sha(&self, pid: u32) {
        self.gate_service_sha
            .as_ref()
            .expect("装门夹具必有服务 SHA 旋钮")
            .lock()
            .unwrap()
            .insert(pid, None);
    }

    /// M2：清除按 pid 覆写（回到登记构建缺省——复验重新登记后的恢复形态）。
    fn reset_gate_service_sha(&self, pid: u32) {
        self.gate_service_sha
            .as_ref()
            .expect("装门夹具必有服务 SHA 旋钮")
            .lock()
            .unwrap()
            .remove(&pid);
    }

    /// 当前新鲜观察（取证「窗口世界当前停在那一扇」）。
    fn gate_opened_window(&self) -> Option<GateOpenedWindow> {
        self.gate_opened
            .as_ref()
            .expect("装门夹具必有观察旋钮")
            .lock()
            .unwrap()
            .clone()
    }

    /// M2：服务进程换代（同 pid 新启动毫秒；登记 pid 不变）。换代后既有绑定目标
    /// 与新描述符失配（前置身份链 OC_GENERATION_MISMATCH），恢复走 N1 既有合法
    /// 途径 = 交回 legacy + 显式重选（与 N2 换代用例同形）。
    fn set_service_generation(&self, new_started_at_ms: i64) {
        self.service_generation
            .store(new_started_at_ms, Ordering::SeqCst);
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

    fn route_request(&self, sid: &str, token: &str, force: bool) -> String {
        let version: i64 = self.state.store.with(|c| {
            c.query_row(
                "SELECT route_version FROM opencode_native_routes WHERE session_id = ?1",
                [sid],
                |r| r.get(0),
            )
            .expect("测试路由必须存在")
        });
        let mut request =
            serde_json::json!({"sessionId": sid, "bindingToken": token, "routeVersion": version});
        if force {
            request["force"] = serde_json::json!(true);
        }
        request.to_string()
    }

    async fn preview_token(&self, sid: &str, cookie: &str) -> serde_json::Value {
        let response = self
            .app
            .clone()
            .oneshot(req(
                "GET",
                &format!("/m/api/v1/opencode/route/preview?sessionId={sid}"),
                Some(cookie),
                None,
            ))
            .await
            .unwrap();
        let status = response.status();
        let body: serde_json::Value = serde_json::from_str(&body_of(response).await).unwrap();
        assert_eq!(status, 200, "预览候选：{body}");
        assert!(body["previewToken"].as_str().is_some());
        body["previewToken"].clone()
    }

    async fn native_route_request(&self, sid: &str, token: &str, force: bool) -> String {
        let (status, preview) = self
            .get(&format!("/m/api/v1/opencode/route/preview?sessionId={sid}"))
            .await;
        assert_eq!(status, 200, "预览候选：{preview}");
        let mut request: serde_json::Value =
            serde_json::from_str(&self.route_request(sid, token, force)).unwrap();
        request["candidateToken"] = preview["previewToken"].clone();
        assert!(request["candidateToken"].as_str().is_some());
        request.to_string()
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

fn bound_get(uri: &str, token: &str) -> axum::http::Request<axum::body::Body> {
    let (mut parts, body) = req("GET", uri, Some(COOKIE), None).into_parts();
    parts
        .headers
        .insert("x-opencode-expected-binding", token.parse().unwrap());
    axum::http::Request::from_parts(parts, body)
}

async fn get_bound(
    app: &axum::Router,
    uri: &str,
    token: &str,
) -> (axum::http::StatusCode, serde_json::Value) {
    let response = app.clone().oneshot(bound_get(uri, token)).await.unwrap();
    let status = response.status();
    let body = serde_json::from_str(&body_of(response).await).unwrap();
    (status, body)
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

#[cfg(target_os = "macos")]
#[tokio::test]
async fn native_create_rejects_canonical_non_ascii_directory_before_discovery() {
    const ISOLATED_STATE: &str = "TUVIS_NATIVE_CREATE_RED_STATE";
    if std::env::var_os(ISOLATED_STATE).is_none() {
        let state = tempfile::tempdir_in("/tmp").expect("隔离的 OpenCode 状态目录");
        // handler 直接读取 XDG；只给子测试进程设置环境，避免干扰并行测试。
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "native_create_rejects_canonical_non_ascii_directory_before_discovery",
                "--nocapture",
            ])
            .env(ISOLATED_STATE, state.path())
            .env("XDG_STATE_HOME", state.path())
            .status()
            .expect("运行隔离子测试");
        assert!(status.success(), "隔离子测试退出：{status}");
        return;
    }

    let state = std::path::PathBuf::from(std::env::var_os(ISOLATED_STATE).unwrap());
    assert_eq!(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        Some(state.as_os_str())
    );
    assert!(!state.join("opencode/service.json").exists());
    let target = state.join("项目");
    std::fs::create_dir(&target).expect("创建真实的非 ASCII 目录");
    let alias = state.join("project-link");
    std::os::unix::fs::symlink(&target, &alias).expect("创建 ASCII 目录别名");
    assert!(alias.to_str().unwrap().is_ascii());
    assert!(alias.is_dir());
    assert!(!alias.canonicalize().unwrap().to_str().unwrap().is_ascii());

    let f = Fixture::single(Switch::On).await;
    let (status, body) = post(
        &f.app,
        "/m/api/v1/opencode/create-native",
        COOKIE,
        serde_json::json!({
            "requestId": native_create_tests::REQUEST_ID,
            "directory": alias,
        })
        .to_string(),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    let response: serde_json::Value = serde_json::from_str(&body).expect("JSON 错误响应");
    assert_eq!(response["reasonCode"], "non_ascii_path", "{body}");
}

#[cfg(target_os = "macos")]
mod native_create_tests {
    use super::*;
    use axum::http::StatusCode;
    use serde_json::{json, Value};
    use std::time::Duration;
    use tokio::sync::Notify;

    pub(super) const REQUEST_ID: &str = "0123456789abcdef0123456789abcdef";
    const FALLBACK_CREATED_ID: &str = "ses-native-created";

    struct CreatedSession {
        id: String,
        metadata: Value,
    }

    pub(super) struct CreatePeer {
        info: StdMutex<Value>,
        readback_directory: PathBuf,
        mismatch_create_reply_id: AtomicBool,
        mismatch_readback_metadata: AtomicBool,
        created_session: StdMutex<Option<CreatedSession>>,
        requests: StdMutex<Vec<String>>,
        hold_next_info: AtomicBool,
        close_next_info: AtomicBool,
        create_on_fresh_connection: AtomicBool,
        info_reached: Notify,
        release_info: Notify,
        hold_next_create: AtomicBool,
        hold_next_create_reply: AtomicBool,
        drop_next_create_reply: AtomicBool,
        create_reached: Notify,
        release_create: Notify,
        hold_readback: bool,
        readback_reached: Notify,
        release_readback: Notify,
    }

    impl CreatePeer {
        pub(super) async fn reply(
            &self,
            method: &str,
            target: &str,
            request_body: &str,
            first_request: bool,
        ) -> Option<(u16, String, bool)> {
            self.requests
                .lock()
                .unwrap()
                .push(format!("{method} {target}"));
            let mut keep_alive = true;
            let body = match (method, target) {
                ("GET", "/api/info") => {
                    keep_alive = !self.close_next_info.swap(false, Ordering::SeqCst);
                    if self.hold_next_info.swap(false, Ordering::SeqCst) {
                        self.info_reached.notify_one();
                        self.release_info.notified().await;
                    }
                    self.info.lock().unwrap().clone()
                }
                ("POST", "/api/session") => {
                    if first_request {
                        self.create_on_fresh_connection
                            .store(true, Ordering::SeqCst);
                    }
                    if self.hold_next_create.swap(false, Ordering::SeqCst) {
                        self.create_reached.notify_one();
                        self.release_create.notified().await;
                    }
                    let submitted: Value =
                        serde_json::from_str(request_body).expect("原生创建 POST 请求体须为 JSON");
                    let id = submitted["id"].as_str().unwrap_or(FALLBACK_CREATED_ID);
                    *self.created_session.lock().unwrap() = Some(CreatedSession {
                        id: id.to_owned(),
                        metadata: submitted["metadata"].clone(),
                    });
                    if self.hold_next_create_reply.swap(false, Ordering::SeqCst) {
                        self.create_reached.notify_one();
                        self.release_create.notified().await;
                    }
                    if self.drop_next_create_reply.swap(false, Ordering::SeqCst) {
                        return None;
                    }
                    let reply_id = if self.mismatch_create_reply_id.load(Ordering::SeqCst) {
                        "ses_tuvis_unrelated"
                    } else {
                        id
                    };
                    json!({ "data": { "id": reply_id } })
                }
                ("GET", _) if target.starts_with("/api/session/") => {
                    let (id, metadata) = {
                        let created = self.created_session.lock().unwrap();
                        let Some(session) = created
                            .as_ref()
                            .filter(|session| target == format!("/api/session/{}", session.id))
                        else {
                            return Some((404, "{}".to_string(), keep_alive));
                        };
                        (session.id.clone(), session.metadata.clone())
                    };
                    if self.hold_readback {
                        self.readback_reached.notify_one();
                        self.release_readback.notified().await;
                    }
                    let metadata = if self.mismatch_readback_metadata.load(Ordering::SeqCst) {
                        json!({ "tuvisCreateRequest": "different-request" })
                    } else {
                        metadata
                    };
                    json!({ "data": {
                        "id": id,
                        "location": { "directory": self.readback_directory },
                        "metadata": metadata,
                    } })
                }
                _ => return Some((404, "{}".to_string(), keep_alive)),
            };
            Some((200, body.to_string(), keep_alive))
        }
    }

    struct CreateFixture {
        fixture: Fixture,
        peer: Arc<CreatePeer>,
        directory: PathBuf,
        _projects: tempfile::TempDir,
    }

    impl CreateFixture {
        async fn new(directory_mismatch: bool, hold_readback: bool) -> Self {
            // macOS 临时目录的 realpath 在 /private 下，D21 会拒绝。使用工作区内的本地目录。
            let projects = tempfile::Builder::new()
                .prefix(".native-create-")
                .tempdir_in(env!("CARGO_MANIFEST_DIR"))
                .expect("隔离的本地项目目录");
            let directory = projects.path().join("project");
            std::fs::create_dir(&directory).unwrap();
            let directory = directory.canonicalize().unwrap();
            assert!(directory.to_str().unwrap().is_ascii());
            let readback_directory = if directory_mismatch {
                let other = projects.path().join("other-project");
                std::fs::create_dir(&other).unwrap();
                other.canonicalize().unwrap()
            } else {
                directory.clone()
            };
            let fixture =
                Fixture::new_with_ownership_gate(Switch::On, |_| vec![], true, None).await;
            let peer = Arc::new(CreatePeer {
                info: StdMutex::new(json!({
                    "pid": PEER_PID,
                    "version": PEER_VERSION,
                    "urls": [fixture.peer.endpoint],
                })),
                readback_directory,
                mismatch_create_reply_id: AtomicBool::new(false),
                mismatch_readback_metadata: AtomicBool::new(false),
                created_session: StdMutex::new(None),
                requests: StdMutex::new(Vec::new()),
                hold_next_info: AtomicBool::new(false),
                close_next_info: AtomicBool::new(false),
                create_on_fresh_connection: AtomicBool::new(false),
                info_reached: Notify::new(),
                release_info: Notify::new(),
                hold_next_create: AtomicBool::new(false),
                hold_next_create_reply: AtomicBool::new(false),
                drop_next_create_reply: AtomicBool::new(false),
                create_reached: Notify::new(),
                release_create: Notify::new(),
                hold_readback,
                readback_reached: Notify::new(),
                release_readback: Notify::new(),
            });
            *fixture.peer.shared.create.lock().unwrap() = Some(peer.clone());
            Self {
                fixture,
                peer,
                directory,
                _projects: projects,
            }
        }

        async fn create(&self) -> (StatusCode, Value) {
            let (status, body) = post(
                &self.fixture.app,
                "/m/api/v1/opencode/create-native",
                COOKIE,
                json!({
                    "requestId": REQUEST_ID,
                    "directory": self.directory,
                    "firstSentence": "测试首句",
                })
                .to_string(),
            )
            .await;
            (
                status,
                serde_json::from_str(&body).expect("新建路由 JSON 回执"),
            )
        }

        async fn create_status(&self, cookie: &str, request_id: &str) -> (StatusCode, String) {
            let uri = format!("/m/api/v1/opencode/create-native?requestId={request_id}");
            let response = self
                .fixture
                .app
                .clone()
                .oneshot(req("GET", &uri, Some(cookie), None))
                .await
                .unwrap();
            (response.status(), body_of(response).await)
        }

        async fn owner_create_status(&self) -> Value {
            let (status, body) = self.create_status(COOKIE, REQUEST_ID).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            let view: Value = serde_json::from_str(&body).expect("创建状态 JSON");
            assert_eq!(view["requestId"], REQUEST_ID, "{view}");
            assert_eq!(view["directory"], json!(self.directory), "{view}");
            assert!(
                matches!(
                    view["window"].as_str(),
                    Some("creating" | "created" | "pending" | "failed" | "opened" | "blocked")
                ),
                "创建状态须返回有效 window 值：{view}"
            );
            view
        }

        async fn recover(&self) -> (StatusCode, String) {
            post(
                &self.fixture.app,
                "/m/api/v1/opencode/create-native/recover",
                COOKIE,
                json!({ "requestId": REQUEST_ID }).to_string(),
            )
            .await
        }

        fn created_id(&self) -> String {
            self.peer
                .created_session
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .id
                .clone()
        }

        fn assert_create_and_readback(&self) {
            let readback = format!("GET /api/session/{}", self.created_id());
            self.assert_observed(
                &[
                    "GET /api/info",
                    "POST /api/session",
                    &readback,
                    "GET /api/info",
                ],
                1,
            );
        }

        fn assert_read_only_recovery(&self) {
            let created = self.peer.created_session.lock().unwrap();
            let session = created.as_ref().expect("上游须已接受创建");
            let opaque = session
                .id
                .strip_prefix("ses_tuvis_")
                .expect("创建请求须提供预分配 ID");
            assert!(!opaque.is_empty());
            assert_eq!(session.metadata["tuvisCreateRequest"], opaque);
            let readback = format!("GET /api/session/{}", session.id);
            let requests = self.peer.requests.lock().unwrap();
            assert_eq!(&requests[..2], ["GET /api/info", "POST /api/session"]);
            let recovery_reads = &requests[2..];
            assert_eq!(
                recovery_reads
                    .iter()
                    .filter(|request| **request == readback)
                    .count(),
                1,
                "{recovery_reads:?}"
            );
            assert!(
                recovery_reads
                    .iter()
                    .all(|request| request == &readback || request == "GET /api/info"),
                "{recovery_reads:?}"
            );
            assert_eq!(self.fixture.peer.writes(), 1, "恢复只能读取上游");
        }

        async fn create_with_lost_reply(&self) {
            self.peer
                .drop_next_create_reply
                .store(true, Ordering::SeqCst);
            self.create().await;
            self.assert_observed(&["GET /api/info", "POST /api/session"], 1);
            self.assert_creation_unconfirmed().await;
        }

        async fn assert_creation_unconfirmed(&self) {
            let view = self.owner_create_status().await;
            assert_eq!(view["creation"], "unconfirmed", "{view}");
            assert_eq!(view.get("sessionId"), Some(&Value::Null), "{view}");
        }

        fn create_posts(&self) -> usize {
            self.peer
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|request| *request == "POST /api/session")
                .count()
        }

        fn assert_single_create_post(
            &self,
            first: &(StatusCode, Value),
            second: &(StatusCode, Value),
        ) {
            let posts = self.create_posts();
            let requests = self.peer.requests.lock().unwrap();
            assert_eq!(
                posts,
                1,
                "同一 requestId {REQUEST_ID} 只能创建一次。上游请求总数 {}，记录 {requests:?}，两次 handler 回执 {first:?} / {second:?}",
                requests.len(),
            );
        }

        fn assert_observed(&self, requests: &[&str], writes: usize) {
            assert_eq!(self.peer.requests.lock().unwrap().as_slice(), requests);
            assert_eq!(self.fixture.peer.writes(), writes, "含首句在内的原生写总数");
            assert_eq!(
                self.fixture.resume_spawn_calls.load(Ordering::SeqCst),
                0,
                "起窗 spawner 调用次数"
            );
        }
    }

    #[tokio::test]
    async fn oc_closure_create_prepares_missing_directory_on_submit() {
        let f = CreateFixture::new(false, false).await;
        // 保留合法临时父目录和 mock 回读路径，只移除目标叶目录。
        std::fs::remove_dir(&f.directory).unwrap();
        assert!(!f.directory.exists());

        let (status, body) = post(
            &f.fixture.app,
            "/m/api/v1/opencode/create-native",
            COOKIE,
            json!({ "requestId": REQUEST_ID, "directory": f.directory }).to_string(),
        )
        .await;

        assert!(
            f.directory.is_dir(),
            "确认提交须准备目标目录：{status} {body}"
        );
        assert_eq!(f.directory.canonicalize().unwrap(), f.directory);
        assert_eq!(status, StatusCode::OK, "{body}");
        let receipt: Value = serde_json::from_str(&body).expect("新建路由 JSON 回执");
        assert_eq!(receipt["creation"], "created", "{receipt}");
        assert_eq!(receipt["sessionId"], f.created_id(), "{receipt}");
        assert_eq!(receipt["directory"], json!(f.directory), "{receipt}");
        assert_eq!(f.create_posts(), 1, "须在夹具指定的服务上创建一次");
        assert_eq!(f.fixture.peer.writes(), 1, "无首句时仅允许创建写入");
    }

    #[tokio::test]
    async fn oc_closure_create_preview_does_not_prepare_missing_directory() {
        let f = CreateFixture::new(false, false).await;
        std::fs::remove_dir(&f.directory).unwrap();

        let (status, body) = f
            .fixture
            .get(&format!(
                "/m/api/v1/opencode/create-native/preview?requestId={REQUEST_ID}"
            ))
            .await;

        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert!(!f.directory.exists(), "预览不得创建目录");
        f.assert_observed(&[], 0);
    }

    #[tokio::test]
    async fn oc_closure_create_invalid_directory_does_not_prepare_missing_ancestor() {
        let f = CreateFixture::new(false, false).await;
        let missing_parent = f.directory.join("missing-parent");
        let invalid_directory = missing_parent.join("非ASCII");

        let (status, body) = post(
            &f.fixture.app,
            "/m/api/v1/opencode/create-native",
            COOKIE,
            json!({ "requestId": REQUEST_ID, "directory": invalid_directory }).to_string(),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(!missing_parent.exists(), "非法路径不得部分落盘");
        f.assert_observed(&[], 0);
    }

    #[tokio::test]
    async fn oc_closure_create_concurrent_conflicting_intents_prepare_only_winner_directory() {
        let f = CreateFixture::new(false, false).await;
        let loser_directory = f.directory.with_file_name("other-missing-project");
        std::fs::remove_dir(&f.directory).unwrap();
        assert!(!f.directory.exists());
        assert!(!loser_directory.exists());
        f.peer.hold_next_info.store(true, Ordering::SeqCst);

        let submit = |directory: &Path| {
            post(
                &f.fixture.app,
                "/m/api/v1/opencode/create-native",
                COOKIE,
                json!({ "requestId": REQUEST_ID, "directory": directory }).to_string(),
            )
        };
        let conflicting_submit = async {
            tokio::time::timeout(Duration::from_secs(5), f.peer.info_reached.notified())
                .await
                .expect("赢家请求须抵达模拟服务的 GET /api/info");
            let reply =
                tokio::time::timeout(Duration::from_secs(5), submit(&loser_directory)).await;
            f.peer.release_info.notify_one();
            reply.expect("冲突请求须在首个请求暂停时返回")
        };
        let ((winner_status, winner_body), (loser_status, loser_body)) =
            tokio::join!(submit(&f.directory), conflicting_submit);

        assert_eq!(loser_status, StatusCode::CONFLICT, "{loser_body}");
        assert!(!loser_directory.exists(), "冲突请求不得创建自己的目录");
        assert_eq!(winner_status, StatusCode::OK, "{winner_body}");
        assert!(f.directory.is_dir(), "赢家目录须建成");
        let receipt: Value = serde_json::from_str(&winner_body).expect("赢家创建回执 JSON");
        assert_eq!(receipt["creation"], "created", "{receipt}");
        assert_eq!(receipt["directory"], json!(f.directory), "{receipt}");
        assert_eq!(f.create_posts(), 1, "两个请求合计只能创建一个会话");
        assert_eq!(receipt["sessionId"], f.created_id(), "{receipt}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn oc_closure_create_rejects_symlink_parent_into_d21_blacklist() {
        let f = CreateFixture::new(false, false).await;
        // 黑名单落点也放在夹具临时目录内，测试不接触用户的 ~/.ssh。
        let forbidden_parent = f.directory.join(".ssh");
        std::fs::create_dir(&forbidden_parent).unwrap();
        let alias = f.directory.join("allowed-looking-parent");
        std::os::unix::fs::symlink(&forbidden_parent, &alias).unwrap();
        let requested = alias.join("new-project");
        let forbidden_target = forbidden_parent.join("new-project");
        assert!(!forbidden_target.exists());

        let (status, body) = post(
            &f.fixture.app,
            "/m/api/v1/opencode/create-native",
            COOKIE,
            json!({ "requestId": REQUEST_ID, "directory": requested }).to_string(),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(!forbidden_target.exists(), "不得经别名向黑名单目录落盘");
        f.assert_observed(&[], 0);
    }

    #[tokio::test]
    async fn oc_closure_create_preview_does_not_recreate_directory_from_existing_receipt() {
        let f = CreateFixture::new(false, false).await;
        let (status, body) = post(
            &f.fixture.app,
            "/m/api/v1/opencode/create-native",
            COOKIE,
            json!({ "requestId": REQUEST_ID, "directory": f.directory }).to_string(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let receipt = f.owner_create_status().await;
        assert_eq!(receipt["creation"], "created", "{receipt}");
        assert_eq!(receipt["sessionId"], f.created_id(), "{receipt}");
        std::fs::remove_dir(&f.directory).unwrap();
        let prior_requests = f.peer.requests.lock().unwrap().clone();
        let prior_writes = f.fixture.peer.writes();

        let (status, body) = f
            .fixture
            .get(&format!(
                "/m/api/v1/opencode/create-native/preview?requestId={REQUEST_ID}"
            ))
            .await;

        assert!(!status.is_success(), "已删除目录不得返回可用候选：{body}");
        assert!(!f.directory.exists(), "已有收据的预览也不得补建目录");
        assert_eq!(*f.peer.requests.lock().unwrap(), prior_requests);
        assert_eq!(f.fixture.peer.writes(), prior_writes);
    }

    #[tokio::test]
    async fn same_request_id_concurrent_posts_create_only_once() {
        // 让精确回读返回异目录，隔离起窗路径。
        let f = CreateFixture::new(true, false).await;
        f.peer.hold_next_create.store(true, Ordering::SeqCst);
        let duplicate = async {
            tokio::time::timeout(Duration::from_secs(5), f.peer.create_reached.notified())
                .await
                .expect("首个请求须抵达上游 POST /api/session");
            let reply = tokio::time::timeout(Duration::from_secs(5), f.create()).await;
            f.peer.release_create.notify_one();
            reply.expect("首个创建仍在进行时，重复请求应能读到原记录")
        };
        let (first, second) = tokio::join!(f.create(), duplicate);

        f.assert_single_create_post(&first, &second);
    }

    #[tokio::test]
    async fn same_request_id_retry_reads_original_after_upstream_drops_create_reply() {
        let f = CreateFixture::new(true, false).await;
        f.peer.drop_next_create_reply.store(true, Ordering::SeqCst);
        let first = f.create().await;
        f.assert_observed(&["GET /api/info", "POST /api/session"], 1);
        let second = f.create().await;

        f.assert_single_create_post(&first, &second);
    }

    #[tokio::test]
    async fn owner_get_reports_unconfirmed_creation_after_lost_reply_without_upstream_writes() {
        let f = CreateFixture::new(true, false).await;
        f.peer.drop_next_create_reply.store(true, Ordering::SeqCst);
        f.create().await;
        f.assert_observed(&["GET /api/info", "POST /api/session"], 1);

        let view = f.owner_create_status().await;

        assert_eq!(view["creation"], "unconfirmed", "{view}");
        assert_eq!(view.get("sessionId"), Some(&Value::Null), "{view}");
        assert_eq!(f.fixture.peer.writes(), 1, "GET 不应写入上游");
    }

    async fn seed_stale_first_sentence_status(
        f: &CreateFixture,
        action_device: &str,
        action_session: &str,
    ) {
        let (status, receipt) = f.recover().await;
        assert_eq!(status, StatusCode::OK, "{receipt}");
        let session_id = f.created_id();
        f.fixture.state.store.with(|conn| {
            let (first_action_id, service_id, pid, started_at_ms): (String, String, i64, i64) =
                conn.query_row(
                    "SELECT json_extract(record_json, '$.first_action_id'),
                            json_extract(record_json, '$.runtime.serviceId'),
                            json_extract(record_json, '$.runtime.pid'),
                            json_extract(record_json, '$.runtime.startedAtMs')
                       FROM opencode_native_creates
                      WHERE device_id = 'dev-1' AND request_id = ?1 AND session_id = ?2",
                    rusqlite::params![REQUEST_ID, session_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .expect("读取该创建记录的固定首句动作 ID 和原生代际");
            // 模拟首句动作已确认、创建记录的候选更新 CAS 未落库。
            conn.execute(
                "UPDATE opencode_native_creates
                    SET record_json = json_set(record_json,
                        '$.window', 'opened', '$.window_attempted', json('true'),
                        '$.first_sentence_status', 'pending',
                        '$.reason_code', 'OC_BINDING_STALE',
                        '$.reason', 'the original created window changed')
                  WHERE device_id = 'dev-1' AND request_id = ?1",
                [REQUEST_ID],
            )
            .expect("保留过期的首句状态和错误");
            conn.execute(
                "INSERT INTO opencode_native_actions (
                     action_id, device_id, session_id, service_id, runtime_pid,
                     runtime_started_at_ms, directory, operation, payload_sha256,
                     state, native_id, created_at, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'send', 'sha',
                           'confirmed', ?8, 1, 2)",
                rusqlite::params![
                    first_action_id,
                    action_device,
                    action_session,
                    service_id,
                    pid,
                    started_at_ms,
                    f.directory.to_str().unwrap(),
                    format!("msg_{first_action_id}"),
                ],
            )
            .expect("植入已确认的首句动作");
        });
        f.assert_read_only_recovery();
    }

    #[tokio::test]
    async fn owner_get_reconciles_confirmed_first_sentence_without_upstream_writes() {
        let f = CreateFixture::new(false, false).await;
        f.create_with_lost_reply().await;
        let session_id = f.created_id();
        seed_stale_first_sentence_status(&f, "dev-1", &session_id).await;

        let view = f.owner_create_status().await;

        assert_eq!(f.fixture.peer.writes(), 1, "GET 不应写入上游");
        assert_eq!(view["creation"], "created", "{view}");
        assert_eq!(view["sessionId"], session_id, "{view}");
        assert_eq!(view["firstSentenceStatus"], "confirmed", "{view}");
        assert!(view["reasonCode"].is_null(), "{view}");
        assert!(view["reason"].is_null(), "{view}");
    }

    async fn assert_owner_get_ignores_unrelated_first_sentence(
        f: &CreateFixture,
        action_device: &str,
        action_session: &str,
    ) {
        seed_stale_first_sentence_status(f, action_device, action_session).await;

        let view = f.owner_create_status().await;

        assert_eq!(f.fixture.peer.writes(), 1, "GET 不应写入上游");
        assert_eq!(view["firstSentenceStatus"], "pending", "{view}");
        assert_eq!(view["reasonCode"], "OC_BINDING_STALE", "{view}");
    }

    #[tokio::test]
    async fn owner_get_ignores_confirmed_first_sentence_from_another_device() {
        let f = CreateFixture::new(false, false).await;
        f.create_with_lost_reply().await;
        assert_owner_get_ignores_unrelated_first_sentence(&f, "dev-2", &f.created_id()).await;
    }

    #[tokio::test]
    async fn owner_get_ignores_confirmed_first_sentence_from_another_session() {
        let f = CreateFixture::new(false, false).await;
        f.create_with_lost_reply().await;
        assert_owner_get_ignores_unrelated_first_sentence(&f, "dev-1", "ses-unrelated").await;
    }

    #[tokio::test]
    async fn recover_confirms_creation_by_preallocated_id_after_lost_reply() {
        let f = CreateFixture::new(false, false).await;
        f.create_with_lost_reply().await;

        let (status, receipt) = f.recover().await;
        let view = f.owner_create_status().await;

        assert_eq!(
            view["creation"], "created",
            "恢复返回 {status} {receipt}，状态 {view}"
        );
        assert_eq!(view["sessionId"], f.created_id(), "{view}");
        assert_eq!(view["window"], "created", "{view}");
        assert_eq!(f.fixture.resume_spawn_calls.load(Ordering::SeqCst), 0);
        f.assert_read_only_recovery();
    }

    #[tokio::test]
    async fn repeated_verify_only_recovery_never_opens_window_or_sends_first_sentence() {
        let f = CreateFixture::new(false, false).await;
        f.create_with_lost_reply().await;

        for attempt in 1..=2 {
            let (status, body) = post(
                &f.fixture.app,
                "/m/api/v1/opencode/create-native/recover",
                COOKIE,
                json!({ "requestId": REQUEST_ID, "verifyOnly": true }).to_string(),
            )
            .await;

            assert_eq!(
                f.fixture.resume_spawn_calls.load(Ordering::SeqCst),
                0,
                "第 {attempt} 次核对不能起窗：{body}"
            );
            assert_eq!(f.create_posts(), 1, "第 {attempt} 次核对不能重复创建");
            assert_eq!(f.fixture.peer.writes(), 1, "唯一的上游写入须是创建");
            assert_eq!(status, StatusCode::OK, "{body}");
            let receipt: Value = serde_json::from_str(&body).expect("核对回执 JSON");
            assert_eq!(receipt["creation"], "created", "{receipt}");
            assert_eq!(receipt["sessionId"], f.created_id(), "{receipt}");
            assert_eq!(receipt["window"], "created", "{receipt}");

            let view = f.owner_create_status().await;
            assert_eq!(view["creation"], "created", "{view}");
            assert_eq!(view["window"], "created", "{view}");
        }
    }

    #[tokio::test]
    async fn create_post_returns_current_receipt_when_verify_only_confirms_first() {
        let f = CreateFixture::new(false, false).await;
        f.peer.hold_next_create_reply.store(true, Ordering::SeqCst);
        let verify = async {
            tokio::time::timeout(Duration::from_secs(5), f.peer.create_reached.notified())
                .await
                .expect("上游须已创建会话并暂停 POST 回包");
            let recovered = tokio::time::timeout(Duration::from_secs(5), async {
                f.assert_creation_unconfirmed().await;
                post(
                    &f.fixture.app,
                    "/m/api/v1/opencode/create-native/recover",
                    COOKIE,
                    json!({ "requestId": REQUEST_ID, "verifyOnly": true }).to_string(),
                )
                .await
            })
            .await;
            let (status, body) = recovered.expect("只读核对须在 POST 回包前完成");
            assert_eq!(status, StatusCode::OK, "{body}");
            let confirmed: Value = serde_json::from_str(&body).expect("核对回执 JSON");
            assert_eq!(confirmed["creation"], "created", "{confirmed}");
            assert_eq!(confirmed["sessionId"], f.created_id(), "{confirmed}");
            assert_eq!(confirmed, f.owner_create_status().await);
            assert_eq!(f.fixture.resume_spawn_calls.load(Ordering::SeqCst), 0);
            assert_eq!(f.fixture.peer.writes(), 1, "核对期间唯一的上游写入须是创建");
            f.peer.release_create.notify_one();
            confirmed
        };
        let ((post_status, post_receipt), confirmed) = tokio::join!(f.create(), verify);

        assert_eq!(f.create_posts(), 1, "同一请求不得重复创建");
        assert!(f.fixture.resume_spawn_calls.load(Ordering::SeqCst) <= 1);
        assert_eq!(post_status, StatusCode::OK, "{post_receipt}");
        assert_eq!(post_receipt["creation"], "created", "{post_receipt}");
        assert_eq!(
            post_receipt["sessionId"], confirmed["sessionId"],
            "{post_receipt}"
        );
        assert_eq!(post_receipt, f.owner_create_status().await);
    }

    #[tokio::test]
    async fn recover_with_mismatched_metadata_leaves_creation_unconfirmed() {
        let f = CreateFixture::new(false, false).await;
        f.create_with_lost_reply().await;
        f.peer
            .mismatch_readback_metadata
            .store(true, Ordering::SeqCst);

        let (status, receipt) = f.recover().await;

        assert_ne!(
            status,
            StatusCode::NOT_FOUND,
            "恢复路由应处理已存在的请求：{receipt}"
        );
        f.assert_creation_unconfirmed().await;
        f.assert_read_only_recovery();
        assert_eq!(f.fixture.resume_spawn_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn create_rejects_reply_id_different_from_preallocated_id() {
        let f = CreateFixture::new(false, false).await;
        f.peer
            .mismatch_create_reply_id
            .store(true, Ordering::SeqCst);

        let (status, receipt) = f.create().await;

        let view = f.owner_create_status().await;
        assert_eq!(
            view["creation"], "unconfirmed",
            "创建返回 {status} {receipt}，状态 {view}"
        );
        assert_eq!(view.get("sessionId"), Some(&Value::Null), "{view}");
        f.assert_observed(&["GET /api/info", "POST /api/session"], 1);
    }

    #[tokio::test]
    async fn create_blocks_opening_when_exact_readback_metadata_differs() {
        let f = CreateFixture::new(false, false).await;
        f.peer
            .mismatch_readback_metadata
            .store(true, Ordering::SeqCst);

        let (status, receipt) = f.create().await;

        assert_eq!(status, StatusCode::CONFLICT, "{receipt}");
        f.assert_create_and_readback();
    }

    #[tokio::test]
    async fn create_returns_known_session_id_without_confirming_mismatched_directory() {
        let f = CreateFixture::new(true, false).await;

        let (status, receipt) = f.create().await;

        assert_eq!(status, StatusCode::CONFLICT, "{receipt}");
        assert_eq!(receipt["sessionId"], f.created_id(), "{receipt}");
        assert_eq!(receipt["window"], "blocked", "{receipt}");
        assert_eq!(receipt["creation"], "unconfirmed", "{receipt}");
        f.assert_create_and_readback();
    }

    #[tokio::test]
    async fn owner_get_preserves_known_session_id_after_directory_readback_mismatch() {
        let f = CreateFixture::new(true, false).await;
        let (_, receipt) = f.create().await;
        assert_eq!(receipt["sessionId"], f.created_id(), "{receipt}");
        f.assert_create_and_readback();

        let view = f.owner_create_status().await;

        assert_eq!(view["creation"], "unconfirmed", "{view}");
        assert_eq!(view["sessionId"], receipt["sessionId"], "{view}");
        assert_eq!(f.fixture.peer.writes(), 1, "GET 不应写入上游");
    }

    #[tokio::test]
    async fn get_returns_same_not_found_for_other_device_and_unknown_request_id() {
        const UNKNOWN_REQUEST_ID: &str = "fedcba9876543210fedcba9876543210";
        let f = CreateFixture::new(true, false).await;
        f.create().await;
        f.owner_create_status().await;

        let other_device = f.create_status(COOKIE_OTHER, REQUEST_ID).await;
        let unknown_id = f.create_status(COOKIE, UNKNOWN_REQUEST_ID).await;

        assert_eq!(unknown_id.0, StatusCode::NOT_FOUND, "{}", unknown_id.1);
        assert_eq!(other_device, unknown_id);
        assert_eq!(f.fixture.peer.writes(), 1, "GET 不应写入上游");
    }

    #[tokio::test]
    async fn invalid_registration_blocks_create_before_http() {
        let f = CreateFixture::new(false, false).await;
        let registration = f.fixture._temp.path().join("service.json");
        let mut invalid: Value =
            serde_json::from_slice(&std::fs::read(&registration).unwrap()).unwrap();
        invalid["password"] = json!("");
        std::fs::write(registration, invalid.to_string()).unwrap();

        let (status, body) = f.create().await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(
            body["reasonCode"], "OC_SERVICE_REGISTRATION_INVALID",
            "{body}"
        );
        f.assert_observed(&[], 0);
    }

    #[tokio::test]
    async fn unverified_process_generation_blocks_create_before_http() {
        let f = CreateFixture::new(false, false).await;
        f.fixture.set_service_generation(0);

        let (status, body) = f.create().await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["reasonCode"], "OC_RUNTIME_UNVERIFIED", "{body}");
        f.assert_observed(&[], 0);
    }

    async fn assert_info_mismatch(field: &str, value: Value, code: &str) {
        let f = CreateFixture::new(false, false).await;
        f.peer.info.lock().unwrap()[field] = value;

        let (status, body) = f.create().await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["reasonCode"], code, "{body}");
        f.assert_observed(&["GET /api/info"], 0);
    }

    #[tokio::test]
    async fn info_pid_mismatch_blocks_create_post() {
        assert_info_mismatch("pid", json!(PEER_PID + 1), "OC_GENERATION_MISMATCH").await;
    }

    #[tokio::test]
    async fn info_version_mismatch_blocks_create_post() {
        assert_info_mismatch("version", json!("99.0.0"), "OC_VERSION_MISMATCH").await;
    }

    #[tokio::test]
    async fn info_endpoint_mismatch_blocks_create_post() {
        assert_info_mismatch(
            "urls",
            json!(["http://127.0.0.1:1"]),
            "OC_GENERATION_MISMATCH",
        )
        .await;
    }

    #[tokio::test]
    async fn directory_replaced_with_symlink_during_info_blocks_create_post() {
        let f = CreateFixture::new(true, false).await;
        f.peer.hold_next_info.store(true, Ordering::SeqCst);
        let replace_directory = async {
            tokio::time::timeout(Duration::from_secs(5), f.peer.info_reached.notified())
                .await
                .expect("目录校验通过后须发起首次 GET /api/info");
            std::fs::remove_dir(&f.directory).unwrap();
            std::os::unix::fs::symlink(&f.peer.readback_directory, &f.directory).unwrap();
            f.peer.release_info.notify_one();
        };
        let ((status, body), ()) = tokio::join!(f.create(), replace_directory);

        f.assert_observed(&["GET /api/info"], 0);
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["reasonCode"], "OC_DIRECTORY_MISMATCH", "{body}");
    }

    #[tokio::test]
    async fn info_connection_close_blocks_create_post_on_new_connection() {
        let f = CreateFixture::new(false, false).await;
        f.peer.close_next_info.store(true, Ordering::SeqCst);

        let (status, body) = f.create().await;

        assert!(
            !f.peer.create_on_fresh_connection.load(Ordering::SeqCst),
            "首轮 GET /api/info 关闭连接后，POST /api/session 成为了新连接上的首个请求。请求记录 {:?}，路由返回 {status} {body}",
            f.peer.requests.lock().unwrap().as_slice()
        );
        f.assert_observed(&["GET /api/info"], 0);
    }

    #[tokio::test]
    async fn created_id_readback_with_different_directory_blocks_resume() {
        let f = CreateFixture::new(true, false).await;

        let (status, body) = f.create().await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["reasonCode"], "OC_DIRECTORY_MISMATCH", "{body}");
        assert_eq!(body["sessionId"], f.created_id());
        f.assert_create_and_readback();
    }

    #[tokio::test]
    async fn generation_change_after_create_reply_preserves_created_id_without_resume() {
        let f = CreateFixture::new(false, true).await;
        let replace_generation = async {
            // 精确回读已抵达，证明 handler 已消费 POST 回包；让同一 PID 此时换代。
            tokio::time::timeout(Duration::from_secs(5), f.peer.readback_reached.notified())
                .await
                .expect("POST 成功后须发起精确回读");
            f.fixture.set_service_generation(PEER_STARTED_AT_MS + 1);
            f.peer.release_readback.notify_one();
        };
        let ((status, body), ()) = tokio::join!(f.create(), replace_generation);

        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["reasonCode"], "OC_GENERATION_MISMATCH", "{body}");
        assert_eq!(body["sessionId"], f.created_id());
        assert_eq!(body["window"], "pending");
        f.assert_create_and_readback();
    }
}

// ============================================================
// 1. 未配对 / 绑定过期：403，且零请求到原生服务（同文件另有可达性对照）
// ============================================================

#[tokio::test]
async fn unpaired_requests_are_forbidden() {
    let f = Fixture::single(Switch::On).await;
    for (method, uri) in NATIVE_ROUTES {
        let r = f
            .app
            .clone()
            .oneshot(req(method, uri, None, None))
            .await
            .unwrap();
        assert_eq!(r.status(), 403, "未配对必须 403：{method} {uri}");
    }
    assert_eq!(f.peer.requests(), 0, "被 gate 拒绝的请求零原生网络请求");
    assert_eq!(f.peer.writes(), 0, "被 gate 拒绝的请求零原生写");
}

#[tokio::test]
async fn expired_binding_never_posts_to_the_native_service() {
    let f = Fixture::single(Switch::On).await;

    // 过期绑定：gate 直接 403，且一个原生请求都不发（peer 计数 = 线上事实）
    let (status, _) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE_EXPIRED,
        command_body("ses-1", "act-expired", "bt-x"),
    )
    .await;
    assert_eq!(status, 403, "绑定过期必须 403");
    assert_eq!(
        f.peer.requests(),
        0,
        "过期绑定零原生请求（不是「handler 没写」）"
    );
    assert_eq!(f.peer.writes(), 0, "过期绑定零原生写");

    // 可达性对照：同夹具、同路径、有效设备 + 已认领路由 → 原生写**真的发出**
    // （否则上面的 0 只能证明「handler 没写」，证明不了「gate 挡住了」）
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "认领前置：视图必须成功：{view}");
    let token = view["bindingToken"].as_str().expect("绑定令牌").to_string();
    let before = f.peer.writes();
    let (status, text) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        command_body("ses-1", "act_live", &token),
    )
    .await;
    assert_ne!(status, 403, "有效设备不得被 gate 拒绝：{text}");
    assert!(
        f.peer.writes() > before,
        "已认领 + 有效设备的原生命令必须真的发出（peer 计数器非空转）"
    );
}

// ============================================================
// 2. 非 OpenCode / 客户端自带 raw URL、method、permissions → 400
// ============================================================

#[tokio::test]
async fn non_opencode_and_client_supplied_shapes_are_rejected() {
    let f = Fixture::new(Switch::On, |dir| {
        vec![
            opencode_session("ses-1", dir),
            codex_session("ses-codex", dir),
        ]
    })
    .await;

    // 非 OpenCode 会话：原生接口只服务 OpenCode 默认后台 service
    let r = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            "/m/api/v1/opencode/view?session_id=ses-codex",
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 400, "非 OpenCode 会话必须 400");
    assert!(body_of(r).await.contains("not_opencode"));

    // 缺 session_id
    let r = f
        .app
        .clone()
        .oneshot(req("GET", "/m/api/v1/opencode/view", Some(COOKIE), None))
        .await
        .unwrap();
    assert_eq!(r.status(), 400, "缺 session_id 必须 400");

    // 客户端自带 raw URL / method / permissions：DTO 拒未知字段 → 400（零派发）
    for extra in [
        serde_json::json!({ "url": "http://127.0.0.1:1/api/session" }),
        serde_json::json!({ "method": "DELETE" }),
        serde_json::json!({ "permissions": { "edit": "allow" } }),
    ] {
        let mut body: serde_json::Value =
            serde_json::from_str(&command_body("ses-1", "act-1", "bt-x")).unwrap();
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        let (status, _) = post(
            &f.app,
            "/m/api/v1/opencode/actions",
            COOKIE,
            body.to_string(),
        )
        .await;
        assert_eq!(status, 400, "客户端自带字段必须 400：{extra}");
    }
    // 嵌套在 operation 里的同类字段同样被拒
    let nested = serde_json::json!({
        "actionId": "act-2",
        "sessionId": "ses-1",
        "bindingToken": "bt",
        "operation": {
            "kind": "send",
            "text": "x",
            "attachmentPaths": [],
            "delivery": "queue",
            "clientMessageId": "msg_act-2",
            "url": "http://127.0.0.1:1"
        }
    });
    let (status, _) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        nested.to_string(),
    )
    .await;
    assert_eq!(status, 400, "operation 内未知字段必须 400");
    assert_eq!(f.peer.writes(), 0, "形态拒绝路径零原生写");

    // 交回旧路径只认 sessionId / force（其余未知字段 → 400；`force` 是合法字段）
    let (status, _) = post(
        &f.app,
        "/m/api/v1/opencode/route/legacy",
        COOKIE,
        serde_json::json!({ "sessionId": "ses-1", "admin": true }).to_string(),
    )
    .await;
    assert_eq!(status, 400, "route/legacy 拒未知字段");
}

// ============================================================
// 3. 原生表单结果回读（R5b）：200 官方答案 / 404 不泄露存在性
// ============================================================

#[tokio::test]
async fn form_result_reads_official_answer_over_http() {
    let f = Fixture::single(Switch::On).await;

    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "视图必须可用：{view}");
    let token = view["bindingToken"].as_str().expect("绑定令牌");
    let (status, body) = get_bound(
        &f.app,
        "/m/api/v1/opencode/forms/frm-1?session_id=ses-1",
        token,
    )
    .await;
    assert_eq!(status, 200, "已答表单必须 200：{body}");
    assert_eq!(body["formId"], "frm-1");
    assert_eq!(body["sessionId"], "ses-1");
    assert_eq!(body["status"], "answered");
    // 单值答案的 wire 形状：内部标签 + content
    assert_eq!(body["answers"]["q0"]["kind"], "text");
    assert_eq!(body["answers"]["q0"]["value"], "blue");
}

#[tokio::test]
async fn form_result_waits_across_pause_returns_404_not_stale_answer() {
    let f = Fixture::single(Switch::On).await;
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "先建立原生归属");
    let token = view["bindingToken"].as_str().expect("绑定令牌").to_owned();

    f.peer.hold_next_form_detail();
    let app = f.app.clone();
    let handle = tokio::spawn(async move {
        app.oneshot(bound_get(
            "/m/api/v1/opencode/forms/frm-1?session_id=ses-1",
            &token,
        ))
        .await
        .expect("旧监听的 form 请求")
    });
    f.peer.wait_until_form_detail_blocked().await;

    // 等待 detail 期间远程 pause + 新监听重新 view；随后放行旧 detail。
    f.state.opencode_native.pause();
    let (after, _) = f.view("ses-1").await;
    assert_eq!(after, 200, "新监听必须能重新投影");
    f.peer.release_form_detail();

    let resp = handle.await.expect("旧请求任务");
    assert_eq!(
        resp.status(),
        axum::http::StatusCode::NOT_FOUND,
        "旧监听的 detail 不能作为新监听的正式答案返回"
    );
}

#[tokio::test]
async fn form_result_404_for_unknown_other_session_and_read_failure() {
    let f = Fixture::new(Switch::On, |dir| {
        vec![
            opencode_session("ses-1", dir),
            opencode_session("ses-2", dir),
        ]
    })
    .await;

    let (status, view1) = f.view("ses-1").await;
    assert_eq!(status, 200, "ses-1 视图：{view1}");
    let (status, view2) = f.view("ses-2").await;
    assert_eq!(status, 200, "ses-2 视图：{view2}");
    let token1 = view1["bindingToken"].as_str().expect("ses-1 绑定令牌");
    let token2 = view2["bindingToken"].as_str().expect("ses-2 绑定令牌");

    // 不存在 / 读失败 / 本表单属于其他会话：一律同一个 404，不泄露存在性。
    for (uri, token) in [
        (
            "/m/api/v1/opencode/forms/frm-unknown?session_id=ses-1",
            token1,
        ),
        (
            "/m/api/v1/opencode/forms/frm-read-fail?session_id=ses-1",
            token1,
        ),
        ("/m/api/v1/opencode/forms/frm-1?session_id=ses-2", token2),
    ] {
        let (status, body) = get_bound(&f.app, uri, token).await;
        assert_eq!(status, 404, "表单结果不可读一律 404：{uri} {body}");
        assert_eq!(
            body["error"], "form_not_found",
            "不区分不存在/他会话/读失败"
        );
        assert!(
            body.get("answers").is_none(),
            "404 不得携带任何答案：{body}"
        );
    }
}

// ============================================================
// 3c. 附件回显（R5c）：原生回读的文件按消息 seq 关联，受控 file:// 给 previewPath
// ============================================================

#[tokio::test]
async fn concurrent_same_name_uploads_send_distinct_files_over_http() {
    let f = Fixture::single(Switch::On).await;
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    let token = view["bindingToken"].as_str().unwrap();
    let url = "/m/api/v1/session-attachment?session_id=ses-1&name=same.txt&interaction_route=opencode_native";
    let upload = |text: &str| {
        let mut request = req("POST", url, Some(COOKIE), Some(text.to_owned()));
        request
            .headers_mut()
            .insert("x-opencode-expected-binding", token.parse().unwrap());
        let app = f.app.clone();
        async move {
            let response = app.oneshot(request).await.unwrap();
            (response.status(), body_of(response).await)
        }
    };
    let (first, second) = tokio::join!(upload("first"), upload("second"));
    let mut paths = Vec::new();
    for ((status, reply), expected) in [(first, "first"), (second, "second")] {
        assert_eq!(status, 200, "上传响应：{reply}");
        let path = serde_json::from_str::<serde_json::Value>(&reply).unwrap()["path"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);
        paths.push(path);
    }
    assert_ne!(paths[0], paths[1]);
    let (_, view) = f.view("ses-1").await;
    let command = serde_json::json!({
        "actionId": "act_parallel_attach", "sessionId": "ses-1", "bindingToken": view["bindingToken"],
        "operation": { "kind": "send", "text": "两个同名附件", "attachmentPaths": paths, "delivery": "queue", "clientMessageId": "msg_act_parallel_attach" }
    }).to_string();
    let (status, reply) = post(&f.app, "/m/api/v1/opencode/actions", COOKIE, command).await;
    assert_eq!(status, 200, "发送响应：{reply}");
    let (_, view) = f.view("ses-1").await;
    let files = view["files"]["0"].as_array().unwrap();
    assert_eq!(files.len(), 2);
    for (index, expected) in ["first", "second"].into_iter().enumerate() {
        let preview = files[index]["previewPath"].as_str().unwrap();
        assert_ne!(preview, paths[index], "确认后使用独立冻结副本");
        assert_eq!(std::fs::read_to_string(preview).unwrap(), expected);
        assert_eq!(files[index]["size"], expected.len());
        assert_eq!(std::fs::read_to_string(&paths[index]).unwrap(), expected);
    }
}

#[tokio::test]
async fn attachment_files_are_associated_with_message_over_http() {
    let f = Fixture::single(Switch::On).await;
    let controlled = f.dir.join(".tuvis-attachments").join("ses-1");
    std::fs::create_dir_all(&controlled).expect("受控附件目录");
    let note = controlled.join("中文 space 报告.txt");
    std::fs::write(&note, "hello attachment").expect("受控附件");

    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "先真的认领：{view}");
    let token = view["bindingToken"].as_str().expect("绑定令牌").to_string();
    let body = serde_json::json!({
        "actionId": "act_attach_http",
        "sessionId": "ses-1",
        "bindingToken": token,
        "operation": {
            "kind": "send",
            "text": "带附件发送",
            "attachmentPaths": [note.to_string_lossy()],
            "delivery": "queue",
            "clientMessageId": "msg_act_attach_http",
        }
    })
    .to_string();
    let (status, text) = post(&f.app, "/m/api/v1/opencode/actions", COOKIE, body).await;
    assert_eq!(status, 200, "附件发送应确认：{text}");

    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "读回投影：{view}");
    let entries = view["files"]["0"]
        .as_array()
        .unwrap_or_else(|| panic!("附件应关联到 seq 0：{view}"));
    assert_eq!(entries.len(), 1, "一条消息一个受控附件");
    assert_eq!(entries[0]["name"], "中文 space 报告.txt");
    assert_eq!(entries[0]["mime"], "text/plain");
    assert_eq!(entries[0]["size"], 16, "真实附件 data 换成大小");
    let frozen = Path::new(entries[0]["previewPath"].as_str().unwrap());
    assert!(frozen.starts_with(controlled.canonicalize().unwrap()));
    assert_eq!(
        std::fs::read(frozen).unwrap(),
        std::fs::read(&note).unwrap(),
        "预览对应冻结确认内容"
    );
    assert!(
        entries[0].get("data").is_none(),
        "base64 绝不下发手机：{entries:?}"
    );

    // T5：从生产投影给出的路径真的读取文本，而非只断言 DTO。
    let preview_path = entries[0]["previewPath"].as_str().unwrap();
    let query = reqwest::Url::parse_with_params(
        "http://localhost/m/api/v1/file",
        &[("session_id", "ses-1"), ("path", preview_path)],
    )
    .unwrap();
    let (status, content) = f
        .get(&format!("{}?{}", query.path(), query.query().unwrap()))
        .await;
    assert_eq!(status, 200, "受控附件预览必须读到内容：{content}");
    assert_eq!(content["content"], "hello attachment");

    // PNG 也经历生产发送编码 → peer 读文件 → 生产投影 → 真读取。
    let picture = controlled.join("image.png");
    use base64::Engine;
    let png = base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a1ZkAAAAASUVORK5CYII=").unwrap();
    std::fs::write(&picture, &png).unwrap();
    let body = serde_json::json!({
        "actionId": "act_image_http", "sessionId": "ses-1", "bindingToken": token,
        "operation": { "kind": "send", "text": "图片", "attachmentPaths": [picture.to_string_lossy()], "delivery": "queue", "clientMessageId": "msg_act_image_http" }
    }).to_string();
    let (status, reply) = post(&f.app, "/m/api/v1/opencode/actions", COOKIE, body).await;
    assert_eq!(status, 200, "图片发送：{reply}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&reply).unwrap()["nativeId"],
        "msg_act_image_http"
    );
    let (_, image_view) = f.view("ses-1").await;
    let image_file = &image_view["files"]["1"][0];
    assert_eq!(image_file["name"], "image.png");
    assert_eq!(image_file["mime"], "image/png");
    assert_eq!(image_file["size"], png.len());
    let url = reqwest::Url::parse_with_params(
        "http://localhost/m/api/v1/file",
        &[
            ("session_id", "ses-1"),
            (
                "path",
                image_file["previewPath"]
                    .as_str()
                    .expect("生产图片预览路径"),
            ),
        ],
    )
    .unwrap();
    let response = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            &format!("{}?{}", url.path(), url.query().unwrap()),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "image/png");
    use http_body_util::BodyExt;
    assert_eq!(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .as_ref(),
        png.as_slice()
    );

    // 刷新（同一会话再次投影）后附件关联与元数据不丢。
    let (_, refreshed) = f.view("ses-1").await;
    assert_eq!(
        refreshed["files"], image_view["files"],
        "刷新后附件仍在且一致"
    );
}

// ============================================================
// 4. 另一设备的动作 → 404（属主读同一行 → 200）
// ============================================================

#[tokio::test]
async fn another_devices_action_is_not_readable() {
    let f = Fixture::single(Switch::On).await;
    f.seed_action("act-own", "ses-1", "not_sent");

    let r = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            "/m/api/v1/opencode/actions/act-own",
            Some(COOKIE_OTHER),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        404,
        "别人的动作与不存在同码（不给存在性预言机）"
    );

    let r = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            "/m/api/v1/opencode/actions/act-own",
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "属主可读自己的动作视图");
    let body: serde_json::Value = serde_json::from_str(&body_of(r).await).unwrap();
    assert_eq!(body["actionId"], "act-own");
    assert_eq!(body["state"], "not_sent");
}

// ============================================================
// 3b. 首次派发在 reserve 之前：404 只表示当前没查到，不得创建/释放账本，也没有第二次派发
// ============================================================

#[tokio::test]
async fn verify_before_reserve_never_enables_second_dispatch() {
    let f = Fixture::single(Switch::On).await;
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "先真的认领：{view}");
    let token = view["bindingToken"].as_str().expect("绑定令牌").to_string();
    let writes_before = f.peer.writes();

    // 武装一次性阻塞：首次派发的投影读（reserve 之前）在 peer 上停住。
    f.peer.hold_next_message_read();
    let app = f.app.clone();
    let body = command_body("ses-1", "act_pre", &token);
    let dispatch =
        tokio::spawn(async move { post(&app, "/m/api/v1/opencode/actions", COOKIE, body).await });
    f.peer.wait_until_read_blocked().await;

    // 此刻首发仍在 reserve 之前：读与核对都只能 404，账本里也没有这一行。
    let (status, body) = f.get("/m/api/v1/opencode/actions/act_pre").await;
    assert_eq!(status, 404, "reserve 之前读不到动作：{body}");
    let (status, text) = post(
        &f.app,
        "/m/api/v1/opencode/actions/act_pre/verify",
        COOKIE,
        "{}".to_string(),
    )
    .await;
    assert_eq!(status, 404, "reserve 之前核对也只能 404：{text}");
    assert_eq!(f.action_rows("act_pre"), 0, "404 路径不得创建账本行");
    assert_eq!(f.peer.prompt_writes(), 0, "404 窗口没有发生任何写");

    // 放行：首发继续，最多一次写，并真的 Confirmed（写后回读命中关联 id）。
    f.peer.release_message_read();
    let (status, text) = dispatch.await.expect("派发任务不得 panic");
    assert_eq!(status, 200, "首发必须成功返回：{text}");
    let view: serde_json::Value = serde_json::from_str(&text).expect("动作视图");
    assert_eq!(view["state"], "confirmed", "写后回读命中关联 id：{text}");
    assert_eq!(f.peer.prompt_writes(), 1, "首发只写一次");
    assert_eq!(f.peer.writes(), writes_before + 1, "整轮只有一次原生写");
    assert_eq!(f.action_rows("act_pre"), 1, "账本只有首发这一行");
    assert_eq!(f.action_row("act_pre").0, "confirmed");

    // 再核对两次：仍是 Confirmed，且没有新的写——404 没有释放/创建任何东西。
    for _ in 0..2 {
        let (status, text) = post(
            &f.app,
            "/m/api/v1/opencode/actions/act_pre/verify",
            COOKIE,
            "{}".to_string(),
        )
        .await;
        assert_eq!(status, 200, "账本已确认后核对必须 200：{text}");
        let view: serde_json::Value = serde_json::from_str(&text).expect("动作视图");
        assert_eq!(view["state"], "confirmed");
    }
    assert_eq!(f.peer.prompt_writes(), 1, "核对绝不重发");
    assert_eq!(f.peer.writes(), writes_before + 1, "核对零写");
    assert_eq!(f.action_rows("act_pre"), 1, "没有留下一行墓碑/第二次派发");
}

// ============================================================
// 4. 注销/停止：原生 watch 关闭（订阅端观察到终止）
// ============================================================

#[tokio::test]
async fn stop_closes_the_native_watch_and_revoke_blocks_routes() {
    let f = Fixture::single(Switch::On).await;

    // 先订阅（生产侧 = GET /opencode/events 的同一个 `NativeHub::watch`）
    let mut rx = f.state.opencode_native.watch("ses-1");
    assert!(
        !rx.has_changed().unwrap_or(true),
        "刚订阅时没有新修订（频道活着）"
    );

    // 停远程（生产 = `stop_server_with` 里对同一 hub 调 `shutdown`）
    f.state.opencode_native.shutdown();
    let closed = tokio::time::timeout(std::time::Duration::from_secs(5), rx.changed())
        .await
        .expect("shutdown 必须在时限内关闭 watch");
    assert!(
        closed.is_err(),
        "shutdown 后订阅端必须观察到频道关闭（发送端被丢弃）"
    );

    // 注销设备：同一批路由立刻 403（gate 层的设备失效，不进入 handler）
    f.state
        .store
        .with(|c| revoke_device(c, "dev-1").expect("revoke"));
    let r = f
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
    assert_eq!(r.status(), 403, "注销设备后原生订阅必须 403");
}

// ============================================================
// 5. 打开视图即认领（I-2/I-4）：真投影成功 → route=opencode_native
// ============================================================

#[tokio::test]
async fn view_claims_the_route_when_the_switch_is_on() {
    let f = Fixture::single(Switch::On).await;
    assert_eq!(f.route_state("ses-1"), None, "认领前没有路由行");

    let (status, body) = f.view("ses-1").await;
    assert_eq!(status, 200, "契约可读时必须真投影成功：{body}");
    assert_eq!(body["route"], "opencode_native", "开关开 + 投影成功即认领");
    assert!(body["routeReasonCode"].is_null(), "认领成功不给原因词");
    assert!(body["bindingToken"].is_string(), "投影必须带绑定令牌");
    assert_eq!(
        body["complete"], true,
        "NativeRouteView 必须原样透出 complete（空页 = 权威终止）"
    );
    assert_eq!(f.route_state("ses-1").as_deref(), Some("active"));
    assert!(
        f.peer.requests() > 0,
        "投影必然访问原生服务（测试夹具的 peer 可达性证据）"
    );
    assert_eq!(f.peer.writes(), 0, "认领本身是只读：零原生写");
}

// O-M9-2 ①（patch6）：view 认领拒绝体携带路由行版本——「尚未接管」面重选自愈。
// 候选重启后路由行 active v1 指向死亡代际，页面刷新的 view 认领 409
// 拒绝体不携带 routeVersion → 面板重选 POST 缺 routeVersion 必再 409（结构性循环；
// N7 场景 12 能走通的前提是「页面不刷新」）。修复：路由行在场时，`view_and_claim_bound`
// 的拒绝体追加只读的 `routeVersion`（数据库事实，零凭据零副作用）；无路由行（缺省态）
// 不携带该键——既有错误体三键契约形状不变（仅追加可选字段）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn view_claim_refusal_body_carries_route_version_when_route_row_exists() {
    // 生产门世界（echo 写门装配 + 服务代际旋钮）：view 认领真实可建立路由行。
    let f = Fixture::new_with_gate(Switch::On, gate_arm()).await;
    // 缺省态（无路由行）：投影失败的错误体保持既有三键形状，不携带 routeVersion。
    f.peer.set_info_pid(PEER_PID + 999);
    let (status, body) = f.view("ses-1").await;
    assert_ne!(status, 200, "身份未证不得发布投影：{body}");
    assert!(
        body["error"].is_string() && body["reasonCode"].is_string() && body["reason"].is_string(),
        "错误体契约形状（error/reasonCode/reason）不变：{body}"
    );
    assert!(
        body.get("routeVersion").is_none(),
        "无路由行的缺省态不得携带 routeVersion：{body}"
    );
    // 恢复身份 → 正常认领建立路由行 v1。
    f.peer.set_info_pid(PEER_PID);
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "认领必须真的发生：{view}");
    assert_eq!(view["route"], "opencode_native", "认领真实发生：{view}");
    assert_eq!(f.route_state("ses-1").as_deref(), Some("active"));
    // O-M9-2 拓扑：服务换代 → 路由行 active v1 指向死亡代际 → 刷新页面的 view 认领 409。
    f.set_service_generation(PEER_STARTED_AT_MS + 60_000);
    let (status, body) = f.view("ses-1").await;
    assert_eq!(status, 409, "{body}");
    assert!(
        body["error"].is_string() && body["reasonCode"].is_string() && body["reason"].is_string(),
        "错误体契约形状（error/reasonCode/reason）不变：{body}"
    );
    assert_eq!(
        body["routeVersion"],
        serde_json::json!(1),
        "拒绝体必须携带路由行版本（重选请求据此一次成形）：{body}"
    );
    // 闭环（O-M9-2 ③）：据 409 体的 routeVersion，显式重选成功换代 v1→v2。
    // 重选走写权链：echo 门照常武装（与 M2 世代用例同款武装，门契约零改动）。
    // 服务换代后的**首次**重选按 M2 既有语义保守拒绝（写权链读取中途装新代际
    // source → installed_ok 守卫拒绝；路由行零变更），第二次自愈放行——与
    // `gate_semantics_service_build_swap_rejects_before_probe` 的既定断言同形；
    // 本用例只关心「请求已一次成形（不再缺 routeVersion 409）且重试后闭环」。
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    let candidate = f.preview_token("ses-1", COOKIE).await;
    let reselect_body = || {
        serde_json::json!({"sessionId":"ses-1","routeVersion":1,"candidateToken":candidate})
            .to_string()
    };
    let (mut status, mut reply) = post(
        &f.app,
        "/m/api/v1/opencode/route/native",
        COOKIE,
        reselect_body(),
    )
    .await;
    if status == 409 {
        let body: serde_json::Value = serde_json::from_str(&reply).unwrap();
        assert_eq!(
            body["reasonCode"], "OC_BINDING_STALE",
            "换代后首次重选只允许 installed_ok 守卫的保守拒绝：{reply}"
        );
        let (retry_status, retry_text) = post(
            &f.app,
            "/m/api/v1/opencode/route/native",
            COOKIE,
            reselect_body(),
        )
        .await;
        status = retry_status;
        reply = retry_text;
    }
    assert_eq!(
        status, 200,
        "携带拒绝体 routeVersion 的显式重选必须闭环成功：{reply}"
    );
    let reply: serde_json::Value = serde_json::from_str(&reply).expect("重选回执是 JSON");
    assert_eq!(reply["route"], "opencode_native");
    assert_eq!(reply["routeVersion"], serde_json::json!(2));
    // 零重投（O-M9-2 ③）：用户会话零写入门（echo 门探针写是门自身证据链，与 M2 同口径）。
    assert_eq!(f.peer.prompt_writes(), 0, "重选链零用户会话写入");
}

// ============================================================
// 6. 已归 Native：五条旧写入口 409 + retract 仍可删本机行 + flush 让位
// ============================================================

#[tokio::test]
async fn native_route_blocks_every_legacy_write_entry() {
    let f = Fixture::single(Switch::On).await;

    let (status, text) = post(
        &f.app,
        "/m/api/v1/session-send",
        COOKIE,
        send_body("ses-1", "归原生之前的消息", true),
    )
    .await;
    assert_legacy_disabled(status, &text);
    assert_eq!(f.pending_rows("ses-1"), 0);
    assert_eq!(f.route_state("ses-1"), None);
    assert_eq!(f.peer.requests(), 0);

    let (status, body) = f.view("ses-1").await;
    assert_eq!(status, 200, "认领必须真的发生：{body}");
    assert_eq!(f.route_state("ses-1").as_deref(), Some("active"));

    let requests = f.peer.requests();
    // 五条旧写入口全部由原生专用策略拒绝。
    let blocked: [(&str, String); 5] = [
        (
            "/m/api/v1/session-send",
            send_body("ses-1", "旧入口消息", false),
        ),
        (
            "/m/api/v1/session-approve",
            serde_json::json!({ "sessionId": "ses-1", "optionId": "approve" }).to_string(),
        ),
        (
            "/m/api/v1/session-question/answer",
            serde_json::json!({ "sessionId": "ses-1", "action": "select", "index": 0 }).to_string(),
        ),
        (
            "/m/api/v1/session-mode/switch",
            serde_json::json!({ "sessionId": "ses-1", "target": "plan" }).to_string(),
        ),
        (
            "/m/api/v1/session-queue/jump",
            serde_json::json!({ "sessionId": "ses-1", "itemId": 1 }).to_string(),
        ),
    ];
    for (uri, body) in blocked {
        let (status, text) = post(&f.app, uri, COOKIE, body).await;
        assert_legacy_disabled(status, &text);
    }
    assert_eq!(f.pending_rows("ses-1"), 0, "旧客户端不得再入旧队列");

    // retract 和后台 flush 都不得更改同一条历史记录。
    let item = f.seed_pending_row("ses-1", "切换前遗留的旧项");
    let before = legacy_queue_row(&f, item);
    let (status, text) = post(
        &f.app,
        "/m/api/v1/session-queue/retract",
        COOKIE,
        serde_json::json!({ "sessionId": "ses-1", "itemId": item }).to_string(),
    )
    .await;
    assert_legacy_disabled(status, &text);
    assert_eq!(legacy_queue_row(&f, item), before);

    assert_eq!(
        flush_one(&f.state, "ses-1", false),
        FlushOutcome::Deferred,
        "已归 Native 的会话 flush 必须让位"
    );

    // 终端侧：全程零正文、零键序（peer 可达已由 view 证明）
    assert!(
        f.injector.texts().is_empty(),
        "已归 Native 的会话零正文注入（实际：{:?}）",
        f.injector.texts()
    );
    assert!(
        f.injector.keys().is_empty(),
        "已归 Native 的会话零终端键序（实际：{:?}）",
        f.injector.keys()
    );
    assert_eq!(f.pending_rows("ses-1"), 1, "屏障轮不消费队首");
    assert_eq!(legacy_queue_row(&f, item), before);
    assert_eq!(f.peer.requests(), requests);
    assert_no_legacy_write_side_effects(&f);
}

fn assert_legacy_disabled(status: axum::http::StatusCode, text: &str) {
    assert_eq!(status, 409, "{text}");
    let body: serde_json::Value = serde_json::from_str(text).expect("JSON 拒绝回执");
    assert_eq!(body["reasonCode"], "OC_LEGACY_DISABLED", "{text}");
}

fn legacy_queue_row(f: &Fixture, item: i64) -> String {
    f.state.store.with(|c| {
        c.query_row(
            "SELECT json_array(id, session_id, agent_type, device_id, device_name, content, \
             enqueued_at, sent_at, failed_reason) FROM inject_queue WHERE id = ?1",
            [item],
            |r| r.get(0),
        )
        .expect("历史队列行必须保留")
    })
}

fn assert_no_legacy_write_side_effects(f: &Fixture) {
    assert!(f.injector.texts().is_empty() && f.injector.keys().is_empty());
    assert_eq!(f.screen_reads.load(Ordering::SeqCst), 0);
    assert_eq!(f.peer.writes(), 0);
}

/// 已归 Native 的会话也不得通过旧 retract 删除历史行。
#[tokio::test]
async fn retract_preserves_native_owned_rows() {
    let f = Fixture::single(Switch::On).await;
    let (status, _) = f.view("ses-1").await;
    assert_eq!(status, 200);
    assert_eq!(f.route_state("ses-1").as_deref(), Some("active"));

    let item = f.seed_pending_row("ses-1", "切换时遗留");
    let before = legacy_queue_row(&f, item);
    let route = route_version_snapshot(&f);
    let requests = f.peer.requests();
    let (status, text) = post(
        &f.app,
        "/m/api/v1/session-send",
        COOKIE,
        send_body("ses-1", "不许再入队", true),
    )
    .await;
    assert_legacy_disabled(status, &text);
    let (status, text) = post(
        &f.app,
        "/m/api/v1/session-queue/jump",
        COOKIE,
        serde_json::json!({ "sessionId": "ses-1", "itemId": item }).to_string(),
    )
    .await;
    assert_legacy_disabled(status, &text);

    let (status, text) = post(
        &f.app,
        "/m/api/v1/session-queue/retract",
        COOKIE,
        serde_json::json!({ "sessionId": "ses-1", "itemId": item }).to_string(),
    )
    .await;
    assert_legacy_disabled(status, &text);
    assert_eq!(legacy_queue_row(&f, item), before);
    assert_eq!(f.pending_rows("ses-1"), 1);
    assert_eq!(route_version_snapshot(&f), route);
    assert_eq!(f.peer.requests(), requests);
    assert_no_legacy_write_side_effects(&f);
}

// ============================================================
// 7. 退役开关不影响旧写禁用或原生可用性
// ============================================================

#[tokio::test]
async fn retired_switch_keeps_legacy_disabled_and_native_available() {
    let f = Fixture::new(Switch::On, |dir| {
        vec![
            opencode_session("ses-1", dir),
            opencode_session("ses-legacy", dir),
        ]
    })
    .await;
    let item = f.seed_pending_row("ses-legacy", "退役开关不得释放历史待发项");
    let row = legacy_queue_row(&f, item);

    for enabled in [true, false] {
        f.set_switch(enabled);
        let (status, body) = post(
            &f.app,
            "/m/api/v1/session-send",
            COOKIE,
            send_body("ses-1", "退役开关不恢复旧发送", true),
        )
        .await;
        assert_legacy_disabled(status, &body);
        assert_eq!(f.pending_rows("ses-1"), 0);
        assert_eq!(f.route_state("ses-1"), None);
        assert_eq!(
            flush_one(&f.state, "ses-legacy", false),
            FlushOutcome::Deferred
        );
        assert_eq!(legacy_queue_row(&f, item), row);
        assert_eq!(f.pending_rows("ses-legacy"), 1);
        assert_eq!(f.route_state("ses-legacy"), None);
    }
    assert_eq!(f.peer.requests(), 0);
    assert_no_legacy_write_side_effects(&f);

    let (_, view) = f.view("ses-1").await;
    let (status, text) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        command_body("ses-1", "act_off", view["bindingToken"].as_str().unwrap()),
    )
    .await;
    assert_eq!(status, 200, "退役的旧键不能关闭原生写：{text}");
    assert_eq!(f.peer.writes(), 1);

    // 既有动作仍可只读核对（撤回的是写权限，不是读）
    f.seed_action("act-readonly", "ses-1", "unconfirmed");
    let r = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            "/m/api/v1/opencode/actions/act-readonly",
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "开关关闭后既有动作仍可只读核对");
}

// ============================================================
// 8. 退役验证键缺失：原生正常认领与写入，归属继续阻挡旧写
// ============================================================

#[tokio::test]
async fn absent_retired_switch_still_claims_native_and_blocks_legacy() {
    let f = Fixture::single(Switch::Absent).await;
    let (status, body) = f.view("ses-1").await;
    assert_eq!(status, 200);
    assert_eq!(body["route"], "opencode_native");
    assert_eq!(f.route_state("ses-1").as_deref(), Some("active"));
    let (status, text) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        command_body(
            "ses-1",
            "act_default",
            body["bindingToken"].as_str().unwrap(),
        ),
    )
    .await;
    assert_eq!(status, 200, "{text}");
    let (status, text) = post(
        &f.app,
        "/m/api/v1/session-send",
        COOKIE,
        send_body("ses-1", "归属保持原生", true),
    )
    .await;
    assert_eq!(status, 409, "{text}");
    assert!(f.injector.texts().is_empty());
}

// ============================================================
// 9. 显式交回已停用；历史 draining 仍可显式再进入原生
// ============================================================

#[tokio::test]
async fn historical_draining_requires_explicit_native_reentry() {
    let f = Fixture::single(Switch::On).await;
    let (status, view) = f.view("ses-1").await;
    let token = view["bindingToken"].as_str().expect("绑定令牌").to_owned();
    assert_eq!(status, 200, "先真的认领");
    assert_eq!(f.route_state("ses-1").as_deref(), Some("active"));

    seed_historical_draining(&f);
    let before = route_version_snapshot(&f);

    // 历史 draining 是用户的显式决定：视图只读，不静默再认领。
    let (status, body) = f.view("ses-1").await;
    assert_eq!(status, 200, "交回后打开视图仍可只读：{body}");
    assert_eq!(
        body.get("route").and_then(|v| v.as_str()),
        Some("legacy"),
        "已显式退回的会话不得被视图重新认领：{body}"
    );
    assert_eq!(
        body.get("routeReasonCode").and_then(|v| v.as_str()),
        Some("returned_to_legacy"),
        "理由必须是「用户显式退回」，不是旧队列未清：{body}"
    );
    assert_eq!(
        f.route_state("ses-1").as_deref(),
        Some("draining"),
        "视图不得改路由行（仍停在 draining）"
    );
    assert_eq!(route_version_snapshot(&f), before);
    let requests = f.peer.requests();
    let (status, text) = post(
        &f.app,
        "/m/api/v1/session-send",
        COOKIE,
        send_body("ses-1", "退回后的旧写", true),
    )
    .await;
    assert_legacy_disabled(status, &text);
    assert_eq!(f.pending_rows("ses-1"), 0);
    assert_eq!(route_version_snapshot(&f), before);
    assert_eq!(f.peer.requests(), requests);
    assert_no_legacy_write_side_effects(&f);

    // 重新进入必须显式：`POST /opencode/route/native`
    let (status, text) = post(
        &f.app,
        "/m/api/v1/opencode/route/native",
        COOKIE,
        f.native_route_request("ses-1", &token, false).await,
    )
    .await;
    assert_eq!(status, 200, "显式再进入必须成功：{text}");
    assert_eq!(
        f.route_state("ses-1").as_deref(),
        Some("active"),
        "显式再进入把 draining 拉回 active"
    );
    let (status, text) = post(
        &f.app,
        "/m/api/v1/session-send",
        COOKIE,
        send_body("ses-1", "再进入后的旧写", true),
    )
    .await;
    assert_legacy_disabled(status, &text);

    // 幂等：已在 active 时再调一次仍成功
    let (status, text) = post(
        &f.app,
        "/m/api/v1/opencode/route/native",
        COOKIE,
        f.native_route_request("ses-1", &token, false).await,
    )
    .await;
    assert_eq!(status, 200, "已归原生时再进入必须幂等成功：{text}");

    // 旧队列未清时再进入被拒（不迁移、不重投）
    seed_historical_draining(&f);
    let before = route_version_snapshot(&f);
    let item = f.seed_pending_row("ses-1", "历史旧队列未发消息");
    let row = legacy_queue_row(&f, item);
    let (status, text) = post(
        &f.app,
        "/m/api/v1/opencode/route/native",
        COOKIE,
        f.native_route_request("ses-1", &token, false).await,
    )
    .await;
    assert_eq!(status, 409, "旧队列仍有待发行时再进入必须被拒：{text}");
    assert_eq!(
        f.route_state("ses-1").as_deref(),
        Some("draining"),
        "被拒时零变更"
    );
    assert_eq!(route_version_snapshot(&f), before);
    assert_eq!(legacy_queue_row(&f, item), row);
    assert_eq!(f.pending_rows("ses-1"), 1);
    assert_no_legacy_write_side_effects(&f);
}

fn seed_historical_draining(f: &Fixture) {
    f.state.store.with(|c| {
        assert_eq!(
            c.execute(
                "UPDATE opencode_native_routes SET route_state = 'draining', \
                 route_version = route_version + 1 WHERE session_id = 'ses-1' AND route_state = 'active'",
                [],
            )
            .expect("模拟旧版本已经完成的交回"),
            1
        );
    });
}

#[tokio::test]
async fn explicit_legacy_return_preserves_active_and_historical_draining_routes() {
    let f = Fixture::single(Switch::On).await;
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    let token = view["bindingToken"].as_str().expect("绑定令牌");
    let initial_request = f.route_request("ses-1", token, false);
    let item = f.seed_pending_row("ses-1", "历史待发项");
    let row = legacy_queue_row(&f, item);
    let requests = f.peer.requests();
    for route in ["active", "draining"] {
        if route == "draining" {
            seed_historical_draining(&f);
        }
        let before = route_version_snapshot(&f);
        assert_eq!(before.4, route);
        // 重放原请求和携带当前版本的请求都不能写入或推进版本。
        for request in [
            initial_request.clone(),
            f.route_request("ses-1", token, false),
        ] {
            let (status, text) =
                post(&f.app, "/m/api/v1/opencode/route/legacy", COOKIE, request).await;
            assert_legacy_disabled(status, &text);
            assert_eq!(route_version_snapshot(&f), before);
            assert_eq!(legacy_queue_row(&f, item), row);
            assert_eq!(f.pending_rows("ses-1"), 1);
        }
    }
    assert_eq!(f.peer.requests(), requests);
    assert_no_legacy_write_side_effects(&f);
}

#[tokio::test]
async fn return_to_legacy_is_refused_while_an_action_is_unresolved() {
    let f = Fixture::single(Switch::On).await;
    let (status, view) = f.view("ses-1").await;
    let token = view["bindingToken"].as_str().expect("绑定令牌").to_owned();
    assert_eq!(status, 200);
    // 结果不明（unconfirmed）的 native 动作：不允许靠「切回旧路径」绕过只读核对
    f.seed_action("act-pending", "ses-1", "unconfirmed");

    let before = route_version_snapshot(&f);
    let action = f.action_row("act-pending");
    let requests = f.peer.requests();
    let (status, text) = post(
        &f.app,
        "/m/api/v1/opencode/route/legacy",
        COOKIE,
        f.route_request("ses-1", &token, false),
    )
    .await;
    assert_legacy_disabled(status, &text);
    assert_eq!(
        f.route_state("ses-1").as_deref(),
        Some("active"),
        "拒绝时零变更（归属仍是 active）"
    );
    // 旧写入口仍被挡（归还未发生）
    let (status, text) = post(
        &f.app,
        "/m/api/v1/session-send",
        COOKIE,
        send_body("ses-1", "仍应被挡", true),
    )
    .await;
    assert_legacy_disabled(status, &text);
    assert_eq!(route_version_snapshot(&f), before);
    assert_eq!(f.action_row("act-pending"), action);
    assert_eq!(f.pending_rows("ses-1"), 0);
    assert_eq!(f.peer.requests(), requests);
    assert_no_legacy_write_side_effects(&f);
}

// ============================================================
// 10. 旧请求与路由切换并发：至多一条路径取得写资格
// ============================================================

#[tokio::test]
async fn old_send_racing_route_switch_yields_at_most_one_writer() {
    let ids: Vec<String> = (0..6).map(|i| format!("ses-race-{i}")).collect();
    let f = Fixture::new(Switch::On, |dir| {
        let mut sessions: Vec<Session> = ids.iter().map(|id| opencode_session(id, dir)).collect();
        sessions.push(opencode_session("ses-hold", dir));
        sessions
    })
    .await;

    for sid in &ids {
        let app = f.app.clone();
        let hub = f.state.opencode_native.clone();
        let target = NativeTarget {
            runtime: RuntimeKey {
                service_id: "svc-test".into(),
                pid: PEER_PID,
                started_at_ms: PEER_STARTED_AT_MS,
            },
            session_id: sid.clone(),
            directory: f.dir.clone(),
        };
        let body = send_body(sid, "竞态消息", true);
        let (resp, claimed) = tokio::join!(
            async move {
                app.oneshot(req(
                    "POST",
                    "/m/api/v1/session-send",
                    Some(COOKIE),
                    Some(body),
                ))
                .await
                .unwrap()
            },
            async move { hub.select_native_route(&target).await.expect("claim") }
        );
        let status = resp.status();
        let text = body_of(resp).await;
        let queued = f.pending_rows(sid);
        let owned = f.route_state(sid).as_deref() == Some("active");
        assert_legacy_disabled(status, &text);
        assert!(owned && claimed, "旧请求不得抢占原生认领（{sid}）");
        assert_eq!(queued, 0, "旧请求不得入队（{sid}）");
    }
    assert_no_legacy_write_side_effects(&f);

    // 确定性片段（不靠调度运气）：测试自身充当「在飞旧写」持 read permit 时，
    // 切路由（write permit）必须等待，而同步许可（flush 用）必须让位。
    let held = f.state.opencode_native.try_legacy_permit("ses-hold").await;
    let hub = f.state.opencode_native.clone();
    let target = NativeTarget {
        runtime: RuntimeKey {
            service_id: "svc-test".into(),
            pid: PEER_PID,
            started_at_ms: PEER_STARTED_AT_MS,
        },
        session_id: "ses-hold".to_string(),
        directory: f.dir.clone(),
    };
    let claim = tokio::spawn(async move { hub.select_native_route(&target).await });
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    assert!(
        !claim.is_finished(),
        "在飞旧写未结束时切路由不得完成（write permit 必须等 read permit）"
    );
    assert!(
        f.state
            .opencode_native
            .try_legacy_permit_now("ses-hold")
            .is_none(),
        "有写者在排队时同步许可必须让位（flush 不得越过屏障）"
    );
    drop(held);
    assert!(
        claim.await.expect("claim task").expect("claim ok"),
        "旧写结束后切路由取得写资格"
    );
    assert_eq!(f.route_state("ses-hold").as_deref(), Some("active"));
}

// ============================================================
// 11. flush 不得越过屏障：已归 Native 的会话旧 FIFO 零终端写
// ============================================================

#[tokio::test]
async fn flush_cannot_bypass_the_native_barrier() {
    let f = Fixture::new(Switch::On, |dir| {
        vec![
            opencode_session("ses-1", dir),
            opencode_session("ses-legacy", dir),
        ]
    })
    .await;
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "认领必须真的发生：{view}");
    assert_eq!(f.route_state("ses-1").as_deref(), Some("active"));
    assert_eq!(f.route_state("ses-legacy"), None);
    let requests = f.peer.requests();

    for sid in ["ses-legacy", "ses-1"] {
        let item = f.seed_pending_row(sid, "历史旧队列消息");
        let row = legacy_queue_row(&f, item);
        assert_eq!(
            flush_one(&f.state, sid, false),
            FlushOutcome::Deferred,
            "后台 flush 不得投递（{sid}）"
        );
        assert_eq!(legacy_queue_row(&f, item), row);
        assert_eq!(f.pending_rows(sid), 1);
    }
    assert_eq!(f.route_state("ses-legacy"), None);
    assert_eq!(f.route_state("ses-1").as_deref(), Some("active"));
    assert_eq!(f.peer.requests(), requests);
    assert_no_legacy_write_side_effects(&f);
}

// ============================================================
// 12. 有旧 pending 的会话不被自动切到 Native
// ============================================================

#[tokio::test]
async fn legacy_pending_prevents_route_claim() {
    let f = Fixture::single(Switch::On).await;

    let item = f.seed_pending_row("ses-1", "遗留旧项");
    let row = legacy_queue_row(&f, item);
    let (status, body) = f.view("ses-1").await;
    assert_eq!(status, 200, "视图仍可只读：{body}");
    assert_eq!(body["route"], "legacy", "有旧 pending 时不得切 Native");
    assert_eq!(
        body["routeReasonCode"], "legacy_pending",
        "必须给出受控原因词"
    );
    assert_eq!(f.route_state("ses-1"), None, "被拒不得留下路由行");

    let requests = f.peer.requests();
    let (status, text) = post(
        &f.app,
        "/m/api/v1/session-send",
        COOKIE,
        send_body("ses-1", "第二条旧项", true),
    )
    .await;
    assert_legacy_disabled(status, &text);
    assert_eq!(f.pending_rows("ses-1"), 1, "不入队、不搬迁");
    assert_eq!(legacy_queue_row(&f, item), row);
    assert_eq!(f.route_state("ses-1"), None);
    assert_eq!(f.peer.requests(), requests);
    assert_no_legacy_write_side_effects(&f);
}

// ============================================================
// 11. 平台门：非 macOS 分支必须 409 且零原生请求（可测缝）
// ============================================================

/// 平台判据是**编译期**事实（`cfg!(target_os = "macos")`），在 macOS 主机上
/// `OC_PLATFORM_UNVERIFIED` 分支永远走不到。为了让它可被真实验证，handler 提供
/// 注入平台判据的入口 `native_view_with`：本用例走**同一个 handler 体**（不是另写一份
/// 门判断），只把判据换成 false，断言受控码与非零请求之前拦截。
#[tokio::test]
async fn non_macos_platform_gate_refuses_before_any_native_request() {
    use axum::extract::{Query, State};
    use axum::http::{header, HeaderMap, HeaderValue};

    let f = Fixture::single(Switch::On).await;
    let mut headers = HeaderMap::new();
    headers.insert(header::COOKIE, HeaderValue::from_static(COOKIE));
    let mut params = std::collections::HashMap::new();
    params.insert("session_id".to_string(), "ses-1".to_string());

    let resp = multi_agents_manager_lib::remote::opencode_native::native_view_with(
        State(f.state.clone()),
        headers,
        Query(params),
        false,
    )
    .await;
    assert_eq!(resp.status(), 409, "非 macOS 必须受控拒绝");
    let text = body_of(resp).await;
    assert!(
        text.contains("OC_PLATFORM_UNVERIFIED"),
        "必须是受控码而不是 404/500：{text}"
    );
    assert_eq!(f.peer.requests(), 0, "平台门必须在任何原生请求之前");
    assert_eq!(f.peer.writes(), 0, "平台门后不得有任何写请求");
    assert_eq!(f.route_state("ses-1"), None, "被拒不得留下路由行");

    // 对照（同一夹具、同一 handler 体）：生产判据为 true 时真的投影并认领，
    // 证明上面的 0 不是「peer 不可达」造成的空断言。
    let (status, body) = f.view("ses-1").await;
    assert_eq!(status, 200, "生产判据下必须正常：{body}");
    assert!(f.peer.requests() > 0, "对照：peer 真的被访问过");
    assert_eq!(f.route_state("ses-1").as_deref(), Some("active"));
}

// ============================================================
// 13. send-info 分别报告原生就绪状态和已有归属，不认领
// ============================================================

#[tokio::test]
async fn send_info_reports_native_readiness_without_claiming() {
    let f = Fixture::single(Switch::On).await;

    let (status, info) = f.get("/m/api/v1/session-send-info?session_id=ses-1").await;
    assert_eq!(status, 200, "send-info 必须可用：{info}");
    assert_eq!(
        info["interactionRoute"], "opencode_native",
        "可唯一绑定 ⇒ 必须报告原生交互路由：{info}"
    );
    assert_native_only_send_info(&info, false, false);
    assert_eq!(
        f.route_state("ses-1"),
        None,
        "send-info 绝不认领（零路由行）"
    );
    assert!(
        f.peer.requests() > 0,
        "候选判定必须真的探测过原生服务（同一份发现+绑定路径）"
    );
    assert_eq!(f.peer.writes(), 0, "候选判定是只读：零原生写");

    // 打开视图 = 认领（唯一写点）；此后旧写入口必须 409
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "视图必须真投影成功：{view}");
    assert_eq!(f.route_state("ses-1").as_deref(), Some("active"));
    let (status, text) = post(
        &f.app,
        "/m/api/v1/session-send",
        COOKIE,
        send_body("ses-1", "认领后的旧入口", true),
    )
    .await;
    assert_legacy_disabled(status, &text);
    assert_eq!(f.pending_rows("ses-1"), 0);

    // 原生已就绪与原生已认领是不同的信号。
    let route = route_version_snapshot(&f);
    let (status, info) = f.get("/m/api/v1/session-send-info?session_id=ses-1").await;
    assert_eq!(status, 200);
    assert_native_only_send_info(&info, true, false);
    assert_eq!(route_version_snapshot(&f), route);
    assert_no_legacy_write_side_effects(&f);
}

fn assert_native_only_send_info(info: &serde_json::Value, owned: bool, offline: bool) {
    assert_eq!(info["interactionRoute"], "opencode_native", "{info}");
    assert_eq!(info["injectable"], false, "{info}");
    assert_eq!(info["reasonCode"], "OC_LEGACY_DISABLED", "{info}");
    assert!(info.get("interactionRouteCandidate").is_none(), "{info}");
    assert!(info.get("interactionRouteReason").is_none(), "{info}");
    let routing = &info["openCodeRouting"];
    assert_eq!(routing["policy"], "native_only", "{info}");
    assert_eq!(routing["nativeOwned"], owned, "{info}");
    assert_eq!(routing["legacyAllowed"], false, "{info}");
    if offline {
        assert_eq!(routing["connection"], "unavailable", "{info}");
        assert_eq!(routing["reasonCode"], "OC_NATIVE_UNAVAILABLE", "{info}");
    } else {
        assert_eq!(routing["connection"], "ready", "{info}");
        assert!(routing["reasonCode"].is_null(), "{info}");
    }
}

#[tokio::test]
async fn send_info_ignores_retired_switch_for_native_readiness() {
    let f = Fixture::single(Switch::Absent).await;
    for old_false in [false, true] {
        if old_false {
            f.set_switch(false);
        }
        let (status, info) = f.get("/m/api/v1/session-send-info?session_id=ses-1").await;
        assert_eq!(status, 200);
        assert_native_only_send_info(&info, false, false);
        assert_eq!(f.route_state("ses-1"), None);
        assert_no_legacy_write_side_effects(&f);
    }
}

// ============================================================
// 14. 评审 Blocker 2：换代后无法核对的动作必须有一条**显式**的作废出口
// ============================================================

/// 死代际（上一次服务）的 acknowledged 行 + 待答占用。
fn seed_dead_generation_action(f: &Fixture) {
    f.seed_action_generation("act_dead", "ses-1", "acknowledged", "svc-old", 1111, 999);
    f.seed_claim("act_dead", "ses-1", "form:ses-1:frm_dead");
}

#[tokio::test]
async fn return_without_force_keeps_dead_generation_actions_untouched() {
    let f = Fixture::single(Switch::On).await;
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    let token = view["bindingToken"].as_str().expect("绑定令牌").to_owned();
    seed_dead_generation_action(&f);

    let action = f.action_row("act_dead");
    let route = route_version_snapshot(&f);
    let requests = f.peer.requests();
    let (status, text) = post(
        &f.app,
        "/m/api/v1/opencode/route/legacy",
        COOKIE,
        f.route_request("ses-1", &token, false),
    )
    .await;
    assert_legacy_disabled(status, &text);
    let body: serde_json::Value = serde_json::from_str(&text).unwrap();
    // 不提供旧 force 候选，且不更改动作、占用或路由。
    assert!(body.get("unverifiableActions").is_none());
    assert!(body.get("unverifiableActionIds").is_none());
    assert!(body.get("forceAvailable").is_none());
    assert_eq!(f.action_row("act_dead"), action);
    assert_eq!(f.action_row("act_dead").0, "acknowledged");
    assert_eq!(f.claim_rows("act_dead"), 1, "占用未释放");
    assert_eq!(route_version_snapshot(&f), route);
    assert_eq!(f.route_state("ses-1").as_deref(), Some("active"));
    assert_eq!(f.peer.requests(), requests);
    assert_no_legacy_write_side_effects(&f);
}

#[tokio::test]
async fn force_return_preserves_actions_from_a_dead_generation() {
    let f = Fixture::single(Switch::On).await;
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    let token = view["bindingToken"].as_str().expect("绑定令牌").to_owned();
    seed_dead_generation_action(&f);
    let route_before = route_version_snapshot(&f);

    let (status, text) = post(
        &f.app,
        "/m/api/v1/opencode/route/legacy",
        COOKIE,
        f.route_request("ses-1", &token, true),
    )
    .await;
    assert_eq!(status, 409, "force 必须保守拒绝：{text}");
    assert!(text.contains("OC_FORCE_CONFIRMATION_REQUIRED"), "{text}");
    let (state, reason, service) = f.action_row("act_dead");
    assert_eq!(state, "acknowledged");
    assert_eq!(reason, None);
    assert_eq!(service, "svc-old");
    assert_eq!(f.claim_rows("act_dead"), 1);
    assert_eq!(route_version_snapshot(&f), route_before);
    assert_eq!(f.route_state("ses-1").as_deref(), Some("active"));
    assert_eq!(f.pending_rows("ses-1"), 0);
    assert_eq!(f.peer.writes(), 0);
    assert!(f.injector.texts().is_empty() && f.injector.keys().is_empty());
}

#[tokio::test]
async fn force_never_touches_actions_from_the_current_generation() {
    let f = Fixture::single(Switch::On).await;
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    let token = view["bindingToken"].as_str().expect("绑定令牌").to_owned();
    // 同代际的未确认行：仍可由权威回读收敛，force 一条都不许碰
    f.seed_action("act_live", "ses-1", "acknowledged");
    f.seed_claim("act_live", "ses-1", "form:ses-1:frm_live");

    let (status, text) = post(
        &f.app,
        "/m/api/v1/opencode/route/legacy",
        COOKIE,
        f.route_request("ses-1", &token, true),
    )
    .await;
    assert_eq!(status, 409, "同代际未决动作仍然挡住交回：{text}");
    assert!(
        text.contains("OC_FORCE_CONFIRMATION_REQUIRED"),
        "force 必须保守拒绝：{text}"
    );

    let (state, reason, service) = f.action_row("act_live");
    assert_eq!(state, "acknowledged", "同代际行必须原样保留");
    assert_eq!(reason, None, "不得写任何碑文");
    assert_eq!(service, PEER_SERVICE_ID);
    assert_eq!(
        f.claim_rows("act_live"),
        1,
        "占用保留（防止第二个动作挤进同一表单）"
    );
    assert_eq!(
        f.route_state("ses-1").as_deref(),
        Some("active"),
        "路由仍归原生"
    );

    // 旧写入口仍被挡（交回没有发生）
    let (status, _) = post(
        &f.app,
        "/m/api/v1/session-send",
        COOKIE,
        send_body("ses-1", "仍应被挡", true),
    )
    .await;
    assert_eq!(status, 409);
}

/// 落一条「本进程在飞」的 `dispatching` 行：先按指定（可能已死的）代际 seed，再用
/// **本进程代次**的账本 `claim`。`process_epoch()` 是进程级静态，集成测试与 hub 的账本
/// 共用同一次取值，因此这一行正是「存储代际已过期、但写仍可能在本进程里飞」的形态。
fn seed_in_flight_row(f: &Fixture, action_id: &str, sid: &str, generation: (&str, u32, i64)) {
    f.seed_action_generation(
        action_id,
        sid,
        "not_sent",
        generation.0,
        generation.1,
        generation.2,
    );
    let store = NativeActionStore::new(f.state.store.clone());
    assert!(
        store.claim(action_id).expect("claim"),
        "在飞行行必须进入 dispatching"
    );
}

/// G7：`force` 的候选集必须排除**本进程在飞**的写。这里存储代际是旧服务（看起来
/// 「已不可核对」），但该行由本进程 claim（`dispatch_epoch` = 本代次）——写可能已经发出，
/// 强制交回不得把它作废成 `not_sent`，只能 409 并原样保留。
#[tokio::test]
async fn force_return_keeps_in_flight_write_and_refuses() {
    let f = Fixture::single(Switch::On).await;
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    let token = view["bindingToken"].as_str().expect("绑定令牌").to_owned();
    seed_in_flight_row(&f, "act-inflight", "ses-1", ("svc-old", 1111, 999));

    let (status, text) = post(
        &f.app,
        "/m/api/v1/opencode/route/legacy",
        COOKIE,
        f.route_request("ses-1", &token, true),
    )
    .await;
    assert_eq!(
        status, 409,
        "本进程在飞的写必须挡住强制交回（不得作废可能已发出的写）：{text}"
    );

    let (state, reason, service) = f.action_row("act-inflight");
    assert_eq!(state, "dispatching", "在飞行不得被作废成墓碑");
    assert_eq!(reason, None, "不得写碑文");
    assert_eq!(service, "svc-old", "行原样保留（不篡改历史）");
    assert_eq!(
        f.route_state("ses-1").as_deref(),
        Some("active"),
        "交回未成立时路由不得改变"
    );
}

/// 即使清掉同代际阻塞行，force 仍须保守拒绝，所有旧代际行及占用保持不变。
#[tokio::test]
async fn force_return_preserves_dead_rows_after_blocker_removed() {
    let f = Fixture::single(Switch::On).await;
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    let token = view["bindingToken"].as_str().expect("绑定令牌").to_owned();
    // 两条死代际候选（acknowledged / unconfirmed），各带待答占用。
    f.seed_action_generation("act-dead-1", "ses-1", "acknowledged", "svc-old", 1111, 999);
    f.seed_claim("act-dead-1", "ses-1", "form:ses-1:frm_dead_1");
    f.seed_action_generation("act-dead-2", "ses-1", "unconfirmed", "svc-old", 1111, 999);
    f.seed_claim("act-dead-2", "ses-1", "form:ses-1:frm_dead_2");
    // 当前代际的未决行：它让交回不成立，且**不是**可作废候选。
    f.seed_action("act-live", "ses-1", "acknowledged");

    let (status, text) = post(
        &f.app,
        "/m/api/v1/opencode/route/legacy",
        COOKIE,
        f.route_request("ses-1", &token, true),
    )
    .await;
    assert_eq!(status, 409, "仍有当前代际未决动作，交回必须被拒：{text}");
    assert!(text.contains("OC_FORCE_CONFIRMATION_REQUIRED"), "{text}");
    // 原子性：交回不成立 ⇒ 零墓碑、占用保留、路由不变。
    for (id, expected, expected_reason) in [
        ("act-dead-1", "acknowledged", None),
        // seed 的 unconfirmed 行自带原有理由词，不得被改写。
        ("act-dead-2", "unconfirmed", Some("timeout")),
    ] {
        let (state, reason, _) = f.action_row(id);
        assert_eq!(state, expected, "{id} 不得留下墓碑：{text}");
        assert_eq!(reason.as_deref(), expected_reason, "{id} 不得写碑文");
        assert_eq!(f.claim_rows(id), 1, "{id} 的占用不得释放");
    }
    assert_eq!(
        f.route_state("ses-1").as_deref(),
        Some("active"),
        "交回不成立时路由不变"
    );

    // 清掉阻塞行后依然没有来源失效证据，force 必须继续拒绝。
    f.state
        .store
        .with(|c| {
            c.execute(
                "DELETE FROM opencode_native_actions WHERE action_id = 'act-live'",
                [],
            )
        })
        .expect("移除阻塞行");

    let (status, text) = post(
        &f.app,
        "/m/api/v1/opencode/route/legacy",
        COOKIE,
        f.route_request("ses-1", &token, true),
    )
    .await;
    assert_eq!(status, 409, "移除阻塞行后 force 仍必须拒绝：{text}");
    assert!(text.contains("OC_FORCE_CONFIRMATION_REQUIRED"), "{text}");
    for (id, expected, expected_reason) in [
        ("act-dead-1", "acknowledged", None),
        ("act-dead-2", "unconfirmed", Some("timeout")),
    ] {
        let (state, reason, service) = f.action_row(id);
        assert_eq!(state, expected, "{id} 不得作废");
        assert_eq!(reason.as_deref(), expected_reason);
        assert_eq!(service, "svc-old");
        assert_eq!(f.claim_rows(id), 1, "{id} 的待答占用必须保留");
    }
    assert_eq!(f.route_state("ses-1").as_deref(), Some("active"));
    assert_eq!(f.peer.writes(), 0);
    assert!(f.injector.texts().is_empty() && f.injector.keys().is_empty());
}

#[tokio::test]
async fn native_only_blocks_legacy_before_screen_or_keys() {
    let f = Fixture::single(Switch::Absent).await;
    f.state
        .store
        .with(|c| {
            c.execute(
                "DELETE FROM settings WHERE key='remote.opencode_interaction_policy'",
                [],
            )
        })
        .unwrap();
    assert_legacy_blocked_before_screen_or_keys(f, true).await;
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn macos_native_only_blocks_persisted_legacy_compatible() {
    // Fixture 写入真实旧版 SQLite 设置 remote.opencode_interaction_policy=legacy_compatible。
    let f = Fixture::single(Switch::Absent).await;
    assert_legacy_blocked_before_screen_or_keys(f, false).await;
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn macos_legacy_retract_preserves_historical_pending_row() {
    let f = Fixture::single(Switch::Absent).await;
    assert_eq!(f.route_state("ses-1"), None);
    let item_id = f.seed_pending_row("ses-1", "历史待发正文");
    let snapshot = || {
        f.state.store.with(|c| {
            c.query_row(
                "SELECT json_object('id', id, 'session_id', session_id, 'agent_type', agent_type, \
                 'device_id', device_id, 'device_name', device_name, 'content', content, \
                 'enqueued_at', enqueued_at, 'sent_at', sent_at, 'failed_reason', failed_reason) \
                 FROM inject_queue WHERE id=?1",
                [item_id],
                |row| row.get::<_, String>(0),
            )
        })
    };
    let before = snapshot().expect("预置的旧待发行");
    let (status, body) = post(
        &f.app,
        "/m/api/v1/session-queue/retract",
        COOKIE,
        serde_json::json!({"sessionId":"ses-1","itemId":item_id}).to_string(),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("OC_LEGACY_DISABLED"), "{body}");
    assert_eq!(snapshot().expect("旧待发行不得删除"), before);
    assert_eq!(f.pending_rows("ses-1"), 1);
    assert_eq!(f.route_state("ses-1"), None);
    assert_eq!(f.screen_reads.load(Ordering::SeqCst), 0);
    assert!(f.injector.keys().is_empty());
    assert!(f.injector.texts().is_empty());
    assert_eq!(f.peer.writes(), 0);
}

async fn assert_legacy_blocked_before_screen_or_keys(f: Fixture, check_route_switch: bool) {
    f.set_switch(false);
    assert_eq!(f.route_state("ses-1"), None, "会话不得被原生认领");
    let route_token = if check_route_switch {
        let (status, view) = f.view("ses-1").await;
        assert_eq!(status, 200, "{view}");
        Some(view["bindingToken"].as_str().expect("绑定令牌").to_owned())
    } else {
        None
    };
    let item_id = f.seed_pending_row("ses-1", "preserve");
    for (method, uri, body) in [
        (
            "POST",
            "/m/api/v1/session-send",
            send_body("ses-1", "must stay local", true),
        ),
        (
            "POST",
            "/m/api/v1/session-approve",
            serde_json::json!({"sessionId":"ses-1","optionId":"approve"}).to_string(),
        ),
        (
            "POST",
            "/m/api/v1/session-question/answer",
            serde_json::json!({"sessionId":"ses-1","action":"select","index":0}).to_string(),
        ),
        (
            "POST",
            "/m/api/v1/session-mode/switch",
            serde_json::json!({"sessionId":"ses-1","target":"plan"}).to_string(),
        ),
        (
            "POST",
            "/m/api/v1/session-queue/jump",
            serde_json::json!({"sessionId":"ses-1","itemId":item_id}).to_string(),
        ),
        (
            "POST",
            "/m/api/v1/session-plan-feedback",
            serde_json::json!({"sessionId":"ses-1","action":"clear"}).to_string(),
        ),
        (
            "GET",
            "/m/api/v1/session-approve-options?session_id=ses-1",
            String::new(),
        ),
        (
            "GET",
            "/m/api/v1/session-question?session_id=ses-1",
            String::new(),
        ),
        (
            "GET",
            "/m/api/v1/session-mode?session_id=ses-1",
            String::new(),
        ),
        (
            "GET",
            "/m/api/v1/session-mode/menu?session_id=ses-1",
            String::new(),
        ),
        (
            "POST",
            "/m/api/v1/session-mode/menu",
            serde_json::json!({"sessionId":"ses-1","action":"open"}).to_string(),
        ),
        (
            "POST",
            "/m/api/v1/session-create",
            serde_json::json!({"tool":"opencode","projectPath":f.dir,"firstMessage":"hello"})
                .to_string(),
        ),
    ] {
        let response = f
            .app
            .clone()
            .oneshot(req(
                method,
                uri,
                Some(COOKIE),
                if body.is_empty() { None } else { Some(body) },
            ))
            .await
            .unwrap();
        let status = response.status();
        let text = body_of(response).await;
        assert_eq!(status, 409, "{uri}: {text}");
        assert!(text.contains("OC_LEGACY_DISABLED"), "{uri}: {text}");
    }
    assert_eq!(flush_one(&f.state, "ses-1", false), FlushOutcome::Deferred);
    assert_eq!(f.pending_rows("ses-1"), 1);
    let (status, info) = f.get("/m/api/v1/session-send-info?session_id=ses-1").await;
    assert_eq!(status, 200);
    assert_eq!(info["openCodeRouting"]["legacyAllowed"], false);
    if let Some(token) = route_token {
        let route_before = route_version_snapshot(&f);
        for force in [false, true] {
            let (status, body) = post(
                &f.app,
                "/m/api/v1/opencode/route/legacy",
                COOKIE,
                f.route_request("ses-1", &token, force),
            )
            .await;
            assert_eq!(status, 409, "{body}");
            let expected = if force {
                "OC_FORCE_CONFIRMATION_REQUIRED"
            } else {
                "OC_LEGACY_DISABLED"
            };
            assert!(body.contains(expected), "{body}");
            assert_eq!(route_version_snapshot(&f), route_before, "拒绝不得改路由");
        }
    }
    assert_eq!(f.screen_reads.load(Ordering::SeqCst), 0);
    assert!(f.injector.keys().is_empty());
    assert!(f.injector.texts().is_empty());
    assert_eq!(f.peer.writes(), 0);
}

#[tokio::test]
async fn rejected_legacy_policy_preserves_pending_and_unconfirmed() {
    use multi_agents_manager_lib::services::opencode_native::policy::{
        write_policy, OpenCodeInteractionPolicy::*,
    };
    let f = Fixture::single(Switch::On).await;
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    f.seed_pending_row("ses-1", "old pending");
    f.seed_action("unconfirmed-policy", "ses-1", "unconfirmed");
    seed_in_flight_row(&f, "inflight-policy", "ses-1", ("svc-old", 1111, 999));
    let route_before = route_version_snapshot(&f);
    assert_eq!(
        write_policy(&f.state.store, LegacyCompatible)
            .unwrap_err()
            .code,
        "OC_LEGACY_DISABLED"
    );
    write_policy(&f.state.store, NativeOnly).unwrap();
    assert_eq!(
        route_version_snapshot(&f),
        route_before,
        "策略写入不得改路由"
    );
    assert_eq!(f.pending_rows("ses-1"), 1);
    assert_eq!(f.action_row("unconfirmed-policy").0, "unconfirmed");
    assert_eq!(f.action_row("inflight-policy").0, "dispatching");
    assert_eq!(f.route_state("ses-1").as_deref(), Some("active"));
    assert_eq!(flush_one(&f.state, "ses-1", false), FlushOutcome::Deferred);
    assert!(f.injector.texts().is_empty());
    assert!(f.injector.keys().is_empty());
    assert_eq!(f.peer.writes(), 0);
}

#[tokio::test]
async fn missing_snapshot_cannot_turn_into_unguarded_opencode_send() {
    use multi_agents_manager_lib::services::opencode_native::policy::{
        write_policy, OpenCodeInteractionPolicy,
    };
    let f = Fixture::single(Switch::On).await;
    write_policy(&f.state.store, OpenCodeInteractionPolicy::NativeOnly).unwrap();
    f.hide_first_scan.store(true, Ordering::SeqCst);
    let (status, body) = post(
        &f.app,
        "/m/api/v1/session-send",
        COOKIE,
        send_body("ses-1", "must not queue", true),
    )
    .await;
    assert_eq!(status, 404, "请求首次可信快照没有此会话：{body}");
    assert_eq!(f.pending_rows("ses-1"), 0);
    assert_eq!(f.screen_reads.load(Ordering::SeqCst), 0);
    assert!(f.injector.texts().is_empty());
    assert!(f.injector.keys().is_empty());
}

#[tokio::test]
async fn missing_snapshot_cannot_turn_into_unguarded_opencode_mode_switch() {
    assert_missing_snapshot_rejects_opencode_write(
        "/m/api/v1/session-mode/switch",
        serde_json::json!({"sessionId":"ses-1","target":"plan"}),
    )
    .await;
}

#[tokio::test]
async fn missing_snapshot_cannot_turn_into_unguarded_opencode_approve() {
    assert_missing_snapshot_rejects_opencode_write(
        "/m/api/v1/session-approve",
        serde_json::json!({"sessionId":"ses-1","optionId":"approve"}),
    )
    .await;
}

async fn assert_missing_snapshot_rejects_opencode_write(uri: &str, payload: serde_json::Value) {
    use multi_agents_manager_lib::services::opencode_native::policy::{
        write_policy, OpenCodeInteractionPolicy,
    };
    let f = Fixture::single(Switch::On).await;
    write_policy(&f.state.store, OpenCodeInteractionPolicy::NativeOnly).unwrap();
    f.hide_first_scan.store(true, Ordering::SeqCst);
    let scans_before = f.session_scans.load(Ordering::SeqCst);
    let (status, body) = post(&f.app, uri, COOKIE, payload.to_string()).await;
    assert_eq!(status, 404, "首次可信快照未发现会话：{uri}: {body}");
    assert_eq!(f.session_scans.load(Ordering::SeqCst) - scans_before, 1);
    assert!(!f.hide_first_scan.load(Ordering::SeqCst));
    assert_eq!(f.screen_reads.load(Ordering::SeqCst), 0);
    assert!(f.injector.texts().is_empty());
    assert!(f.injector.keys().is_empty());
    assert_eq!(f.peer.writes(), 0);
}

#[tokio::test]
async fn routing_connection_is_independent_from_route_ownership() {
    for route in ["none", "active", "draining"] {
        let f = Fixture::single(Switch::On).await;
        if route != "none" {
            let (status, view) = f.view("ses-1").await;
            assert_eq!(status, 200, "{view}");
            if route == "draining" {
                seed_historical_draining(&f);
            }
        }
        let before = f.route_state("ses-1").map(|_| route_version_snapshot(&f));
        for offline in [false, true] {
            f.peer.shared.offline.store(offline, Ordering::SeqCst);
            let (status, body) = f.get("/m/api/v1/session-send-info?session_id=ses-1").await;
            assert_eq!(status, 200, "{body}");
            assert_native_only_send_info(&body, route == "active", offline);
            assert_eq!(
                f.route_state("ses-1").map(|_| route_version_snapshot(&f)),
                before,
                "GET 不认领或改写路由"
            );
            assert_no_legacy_write_side_effects(&f);
        }
    }
}

#[tokio::test]
async fn policy_read_requires_pairing_and_ignores_persisted_values() {
    let f = Fixture::single(Switch::On).await;
    let response = f
        .app
        .clone()
        .oneshot(req("GET", "/m/api/v1/opencode/policy", None, None))
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    for persisted in ["native_only", "legacy_compatible", "unknown"] {
        seed_persisted_policy(&f, persisted);
        let (status, body) = f.get("/m/api/v1/opencode/policy").await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["policy"], "native_only", "persisted={persisted:?}");
    }
}

fn seed_persisted_policy(f: &Fixture, value: &str) {
    use multi_agents_manager_lib::services::opencode_native::policy::POLICY_KEY;
    f.state.store.with(|c| {
        c.execute(
            "INSERT OR REPLACE INTO settings (key,value) VALUES (?1,?2)",
            [POLICY_KEY, value],
        )
        .unwrap();
    });
}

#[tokio::test]
async fn persisted_legacy_policy_keeps_native_write_with_absent_or_false_legacy_switch() {
    for old_false in [false, true] {
        let f = Fixture::single(Switch::Absent).await;
        if old_false {
            f.set_switch(false);
        }
        seed_persisted_policy(&f, "legacy_compatible");
        let (status, view) = f.view("ses-1").await;
        assert_eq!(status, 200, "{view}");
        assert_eq!(view["route"], "opencode_native");
        let (status, body) = post(
            &f.app,
            "/m/api/v1/opencode/actions",
            COOKIE,
            command_body(
                "ses-1",
                "act_compat",
                view["bindingToken"].as_str().unwrap(),
            ),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(f.peer.writes(), 1);
        assert_eq!(f.route_state("ses-1").as_deref(), Some("active"));
    }
}

#[tokio::test]
async fn policy_stream_stays_quiet_on_idempotent_write_and_closes_on_revoke() {
    use http_body_util::BodyExt;
    use multi_agents_manager_lib::services::opencode_native::policy::{
        write_policy, OpenCodeInteractionPolicy::*,
    };
    let f = Fixture::single(Switch::On).await;
    let unauth = f
        .app
        .clone()
        .oneshot(req("GET", "/m/api/v1/opencode/policy/events", None, None))
        .await
        .unwrap();
    assert_eq!(unauth.status(), 403);
    let mut streams = Vec::new();
    for cookie in [COOKIE, COOKIE_OTHER] {
        let response = f
            .app
            .clone()
            .oneshot(req(
                "GET",
                "/m/api/v1/opencode/policy/events",
                Some(cookie),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let mut body = response.into_body();
        let initial = body.frame().await.unwrap().unwrap();
        assert!(std::str::from_utf8(initial.data_ref().unwrap())
            .unwrap()
            .contains("event: policy"));
        streams.push(body);
    }
    write_policy(&f.state.store, NativeOnly).unwrap();
    assert_eq!(
        write_policy(&f.state.store, LegacyCompatible)
            .unwrap_err()
            .code,
        "OC_LEGACY_DISABLED"
    );
    for body in &mut streams {
        assert!(
            std::future::poll_fn(|cx| {
                let mut frame = std::pin::pin!(body.frame());
                std::task::Poll::Ready(std::future::Future::poll(frame.as_mut(), cx).is_pending())
            })
            .await,
            "无状态变化不得广播策略事件"
        );
    }
    f.state.sse_registry.disconnect_all();
    for body in &mut streams {
        assert!(body.frame().await.is_none());
    }
}

// 以下用 with_coordinator_sources 注入测试专用归属证据，不能作为真实连接证据。
// A、B 是同一回环假服务上的不同会话，另有独立假服务 B；账本与登记文件均为测试私有。
async fn hidden_target_fixture() -> Fixture {
    Fixture::new(Switch::On, |dir| {
        vec![
            opencode_session("ses-1", dir),
            opencode_session("ses-b", dir),
        ]
    })
    .await
}
#[tokio::test]
async fn hidden_authorized_target_remote_send_still_reaches_original_service() {
    let f = hidden_target_fixture().await;
    let b = Peer::start(&f.dir).await;
    assert!(reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("{}/api/info", b.endpoint))
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "授权 A：{view}");
    let token = view["bindingToken"].as_str().unwrap();
    f.hide_cards.store(true, Ordering::SeqCst);
    let (status, body) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        command_body("ses-1", "hidden_direct", token),
    )
    .await;
    assert_eq!(b.writes(), 0, "独立服务 B 绝不能收到写");
    assert_eq!(
        f.peer.shared.b_writes.load(Ordering::SeqCst),
        0,
        "可见会话 B 绝不能收到写"
    );
    assert_eq!(status, 200, "下卡不应使原已授权目标不可解析：{body}");
    assert_eq!(f.peer.prompt_writes(), 1, "消息只投递原服务 A");
}

#[tokio::test]
async fn same_id_other_directory_card_does_not_shadow_queued_original_target() {
    let f = Fixture::single(Switch::On).await;
    let other_dir = f._temp.path().join("other-project");
    std::fs::create_dir_all(&other_dir).unwrap();
    let other_dir = other_dir.canonicalize().unwrap();
    assert_ne!(other_dir, f.dir);
    let other_peer = Peer::start(&other_dir).await;
    let reachable = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("{}/api/session/ses-1", other_peer.endpoint))
        .send()
        .await
        .unwrap();
    assert!(reachable.status().is_success());
    let other_session: serde_json::Value = reachable.json().await.unwrap();
    assert_eq!(
        other_session["location"]["directory"],
        other_dir.to_str().unwrap()
    );

    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "先授权原目录 A：{view}");
    let token = view["bindingToken"].as_str().unwrap();
    let pause = serde_json::json!({
        "actionId": "same_id_pause", "sessionId": "ses-1", "bindingToken": token,
        "operation": {"kind": "set_outbox_paused", "paused": true, "expectedRevision": 0}
    })
    .to_string();
    let (status, body) = post(&f.app, "/m/api/v1/opencode/actions", COOKIE, pause).await;
    assert_eq!(status, 200, "暂停：{body}");
    let (status, body) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        command_body("ses-1", "same_id_queued", token),
    )
    .await;
    assert_eq!(status, 200, "入队：{body}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["state"],
        "queued"
    );
    assert_eq!(f.peer.writes(), 0, "暂停时未投递");
    f.state.store.with(|c| {
        assert_eq!(c.execute(
            "UPDATE opencode_outbox_targets SET paused=0 WHERE session_id='ses-1' AND directory=?1",
            [f.dir.canonicalize().unwrap().to_string_lossy().as_ref()],
        ).unwrap(), 1, "必须解暂停原目录目标")
    });

    let scans = Arc::new(AtomicUsize::new(0));
    let scans_for_source = scans.clone();
    let other_card = opencode_session("ses-1", &other_dir);
    f.state.opencode_native.start_outbox(Arc::new(move || {
        scans_for_source.fetch_add(1, Ordering::SeqCst);
        vec![other_card.clone()]
    }));
    let delivered = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while f.peer.prompt_writes() == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    f.state.opencode_native.pause();
    assert!(
        scans.load(Ordering::SeqCst) > 0,
        "必须实际扫描同 ID 异目录卡"
    );
    assert_eq!(other_peer.writes(), 0, "异目录服务不得收到写");
    assert!(
        delivered.is_ok(),
        "应派发到原目录 A，账本：{:?}",
        f.action_row("same_id_queued")
    );
    assert_eq!(f.peer.prompt_writes(), 1, "原目录 A 恰好收到一次派发");
}

#[tokio::test]
async fn hidden_authorized_target_background_queue_delivers_to_original_service() {
    let f = hidden_target_fixture().await;
    let b = Peer::start(&f.dir).await;
    assert!(reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("{}/api/info", b.endpoint))
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "授权 A：{view}");
    let token = view["bindingToken"].as_str().unwrap();
    let pause = serde_json::json!({"actionId":"hidden_pause","sessionId":"ses-1","bindingToken":token,"operation":{"kind":"set_outbox_paused","paused":true,"expectedRevision":0}}).to_string();
    let (status, body) = post(&f.app, "/m/api/v1/opencode/actions", COOKIE, pause).await;
    assert_eq!(status, 200, "暂停：{body}");
    let (status, body) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        command_body("ses-1", "hidden_queued", token),
    )
    .await;
    assert_eq!(status, 200, "入队：{body}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["state"],
        "queued"
    );
    f.hide_cards.store(true, Ordering::SeqCst);
    f.state.store.with(|c| {
        assert_eq!(c.execute(
            "UPDATE opencode_outbox_targets SET paused=0 WHERE session_id='ses-1' AND directory=?1",
            [f.dir.canonicalize().unwrap().to_string_lossy().as_ref()],
        ).unwrap(), 1, "必须解暂停原目录目标")
    });
    let scans = Arc::new(AtomicUsize::new(0));
    let scans_for_source = scans.clone();
    let card_b = opencode_session("ses-b", &f.dir);
    f.state.opencode_native.start_outbox(Arc::new(move || {
        scans_for_source.fetch_add(1, Ordering::SeqCst);
        vec![card_b.clone()]
    }));
    let delivered = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while f.peer.prompt_writes() == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    f.state.opencode_native.pause();
    assert!(
        scans.load(Ordering::SeqCst) > 0,
        "后台消费者必须实际扫描过仅含 B 的卡片列表"
    );
    assert_eq!(b.writes(), 0, "独立服务 B 绝不能收到写");
    assert_eq!(
        f.peer.shared.b_writes.load(Ordering::SeqCst),
        0,
        "可见会话 B 绝不能收到写"
    );
    assert!(
        delivered.is_ok(),
        "已授权 A 下卡后队列仍应派发至 A；实际队列状态：{:?}",
        f.action_row("hidden_queued")
    );
    assert_eq!(f.peer.prompt_writes(), 1);
}

#[tokio::test]
async fn hidden_target_expired_generation_rejects_remote_write_and_retains_queue() {
    let f = hidden_target_fixture().await;
    let b = Peer::start(&f.dir).await;
    assert!(reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .get(format!("{}/api/info", b.endpoint))
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "授权 A：{view}");
    let token = view["bindingToken"].as_str().unwrap();
    let pause = serde_json::json!({"actionId":"expired_pause","sessionId":"ses-1","bindingToken":token,"operation":{"kind":"set_outbox_paused","paused":true,"expectedRevision":0}}).to_string();
    assert_eq!(
        post(&f.app, "/m/api/v1/opencode/actions", COOKIE, pause)
            .await
            .0,
        200
    );
    let (status, body) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        command_body("ses-1", "expired_queued", token),
    )
    .await;
    assert_eq!(status, 200, "入队：{body}");
    f.hide_cards.store(true, Ordering::SeqCst);
    f.peer.set_info_pid(9999);
    f.state.store.with(|c| {
        assert_eq!(c.execute(
            "UPDATE opencode_outbox_targets SET paused=0 WHERE session_id='ses-1' AND directory=?1",
            [f.dir.canonicalize().unwrap().to_string_lossy().as_ref()],
        ).unwrap(), 1, "必须解暂停原目录目标")
    });
    let err = f
        .state
        .opencode_native
        .flush_outbox(&opencode_session("ses-1", &f.dir))
        .await;
    assert!(err.is_err(), "代际不符必须拒绝队列派发");
    assert_eq!(f.action_row("expired_queued").0, "queued", "待发必须保留");
    let (status, body) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        command_body("ses-1", "expired_direct", token),
    )
    .await;
    assert_eq!(f.peer.writes(), 0, "A 代际失效后零写");
    assert_eq!(b.writes(), 0, "独立服务 B 零写");
    assert_eq!(
        f.peer.shared.b_writes.load(Ordering::SeqCst),
        0,
        "可见会话 B 零写"
    );
    assert_eq!(
        status, 409,
        "应解析原目标并以代际失效拒绝，而非把下卡当成不存在：{body}"
    );
}

#[tokio::test]
async fn outbox_http_pause_queue_read_and_revoke() {
    let f = Fixture::single(Switch::On).await;
    let (_, view) = f.view("ses-1").await;
    let command=serde_json::json!({"actionId":"outbox_pause","sessionId":"ses-1","bindingToken":view["bindingToken"],"operation":{"kind":"set_outbox_paused","paused":true,"expectedRevision":0}}).to_string();
    let (status, body) = post(&f.app, "/m/api/v1/opencode/actions", COOKIE, command).await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        command_body(
            "ses-1",
            "outbox_send",
            view["bindingToken"].as_str().unwrap(),
        ),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["state"],
        "queued"
    );
    let (status, snapshot) = get_bound(
        &f.app,
        "/m/api/v1/opencode/outbox?session_id=ses-1",
        view["bindingToken"].as_str().expect("绑定令牌"),
    )
    .await;
    assert_eq!(status, 200, "{snapshot}");
    assert_eq!(snapshot["items"][0]["owner"], "tuvis");
    assert_eq!(snapshot["paused"], true);
    assert_eq!(f.peer.writes(), 0);
    let denied = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            "/m/api/v1/opencode/outbox?session_id=ses-1",
            None,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(denied.status(), 403);
}

#[tokio::test]
async fn outbox_item_verification_preserves_pairing_owner_and_original_target() {
    let f = Fixture::single(Switch::On).await;
    f.seed_action("background_verify", "ses-1", "unconfirmed");
    f.state.store.with(|c| c.execute("INSERT INTO opencode_outbox(action_id,session_id,command_json,text_summary,attachment_count) VALUES ('background_verify','ses-1',NULL,'pending verification',0)",[]).unwrap());
    let url = "/m/api/v1/opencode/actions/background_verify/verify";
    for (cookie, expected) in [(None, 403), (Some(COOKIE_OTHER), 404)] {
        let response = f
            .app
            .clone()
            .oneshot(req("POST", url, cookie, None))
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }
    assert_eq!(f.peer.requests(), 0, "被拒请求不能读取原生私有动作");
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    let (status, body) = post(&f.app, url, COOKIE, String::new()).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["state"],
        "unconfirmed",
        "没有证据仍保持未知"
    );
    assert_eq!(f.peer.writes(), 0, "verify控制请求只产生原生读取");
    let (status, snapshot) = get_bound(
        &f.app,
        "/m/api/v1/opencode/outbox?session_id=ses-1",
        view["bindingToken"].as_str().unwrap(),
    )
    .await;
    assert_eq!(status, 200, "{snapshot}");
    assert_eq!(snapshot["items"][0]["state"], "unconfirmed");
    assert_eq!(snapshot["items"][0]["owner"], "native");
}

#[tokio::test]
async fn t10_conditional_answers_validate_at_http_boundary_before_dispatch() {
    let f = Fixture::single(Switch::On).await;
    let mut raw: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/opencode-native/product-form-conditional.json"
    ))
    .unwrap();
    raw["data"]["id"] = serde_json::json!("frm-conditional");
    raw["data"]["sessionID"] = serde_json::json!("ses-1");
    // 题干正文：官方 `Form.StringField.description` 必须穿透到手机视图（只读展示）。
    raw["data"]["fields"][0]["title"] = serde_json::json!("CHOICE");
    raw["data"]["fields"][0]["description"] = serde_json::json!("确认要发布吗？");
    *f.peer.shared.conditional_form.lock().unwrap() = Some(raw["data"].clone());
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200);
    assert_eq!(
        view["pending"][0]["fields"][0]["description"], "确认要发布吗？",
        "字段题干必须出现在公开视图的字段上"
    );
    assert!(
        view["pending"][0]["fields"][1].get("description").is_none(),
        "未声明 description 的字段不得凭空长出该键（老载荷形状不变）"
    );
    assert_eq!(view["pending"][0]["fields"][1]["when"]["value"], "yes");
    let route_before = route_version_snapshot(&f);
    for (id, answers, expected) in [
        (
            "missing",
            serde_json::json!({"q0":{"kind":"text","value":"yes"}}),
            "OC_ANSWER_INCOMPLETE",
        ),
        (
            "inactive",
            serde_json::json!({"q0":{"kind":"text","value":"no"},"q1":{"kind":"text","value":"extra"}}),
            "OC_ANSWER_INVALID",
        ),
    ] {
        let (status, body) = post(&f.app, "/m/api/v1/opencode/actions", COOKIE, serde_json::json!({
            "actionId":id,"sessionId":"ses-1","bindingToken":view["bindingToken"],
            "operation":{"kind":"answer","formId":"frm-conditional","contextToken":view["pending"][0]["context_token"],"answers":answers}
        }).to_string()).await;
        assert_eq!(status, 400, "{body}");
        assert!(body.contains(expected), "{body}");
        assert_eq!(route_version_snapshot(&f), route_before, "拒绝不得改路由");
    }
    assert_eq!(f.peer.writes(), 0);
    let (status, body) = post(&f.app, "/m/api/v1/opencode/actions", COOKIE, serde_json::json!({
        "actionId":"t10_valid","sessionId":"ses-1","bindingToken":view["bindingToken"],
        "operation":{"kind":"answer","formId":"frm-conditional","contextToken":view["pending"][0]["context_token"],"answers":{"q0":{"kind":"text","value":"no"}}}
    }).to_string()).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(f.peer.writes(), 1);
    let (status, result) = get_bound(
        &f.app,
        "/m/api/v1/opencode/forms/frm-conditional?session_id=ses-1",
        view["bindingToken"].as_str().expect("绑定令牌"),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(result["status"], "answered");
    assert_eq!(result["answers"]["q0"]["value"], "no");
    assert!(result["answers"].get("q1").is_none());
}

// R2 生产服务身份门：证据链不成立（同端口冒名 = info 与登记 id/pid 不一致）时，
// 任何写路径都在 reserve/claim 之前按证据链的受控码拒绝；pending 原样保留、verify
// 只读不改判。服务身份一致仍需可信 TUI 连接证据才能确认实例归属。
#[tokio::test]
async fn unproven_service_identity_blocks_every_write_before_claim() {
    let f = Fixture::new_with_ownership_gate(
        Switch::On,
        |dir| vec![opencode_session("ses-1", dir)],
        true,
        None,
    )
    .await;
    let candidate = f.preview_token("ses-1", COOKIE).await;
    // 同端口冒名：登记滞留不变，`/api/info` 自报他者 pid（R0 反例 2 形态）。
    f.peer.set_info_pid(PEER_PID + 999);
    let (view_status, view_body) = f.view("ses-1").await;
    // 身份不可证 ⇒ 连只读投影都无法建立（服务不可用，不得读旧登记硬连）。
    assert_ne!(view_status, 200, "身份未证不得发布投影: {view_body}");
    assert!(
        view_body.to_string().contains("OC_GENERATION_MISMATCH"),
        "info 与登记不一致必须按换代拒读: {view_body}"
    );
    let (status, body) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        command_body("ses-1", "b1_send", "whatever"),
    )
    .await;
    assert_eq!(status, 409, "身份未证不得写: {body}");
    assert!(
        body.contains("OC_GENERATION_MISMATCH"),
        "拒绝必须来自证据链（换代不符），不是一刀切归属门: {body}"
    );
    let (status, body) = post(
        &f.app,
        "/m/api/v1/opencode/route/native",
        COOKIE,
        serde_json::json!({"sessionId":"ses-1", "candidateToken":candidate}).to_string(),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert!(
        body.contains("OC_GENERATION_MISMATCH") || body.contains("OC_BINDING_STALE"),
        "{body}"
    );
    assert_eq!(f.route_state("ses-1"), None);
    assert_eq!(f.peer.writes(), 0);
}

#[tokio::test]
async fn unproven_identity_preserves_pending_and_verifies_readonly() {
    let f = Fixture::new_with_ownership_gate(
        Switch::On,
        |dir| vec![opencode_session("ses-1", dir)],
        true,
        None,
    )
    .await;
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "身份失效前读取视图：{view}");
    let command: multi_agents_manager_lib::services::opencode_native::NativeCommand =
        serde_json::from_str(&command_body(
            "ses-1",
            "b1_queued",
            view["bindingToken"].as_str().unwrap(),
        ))
        .unwrap();
    let multi_agents_manager_lib::services::opencode_native::NativeOperation::Send(send) =
        &command.operation
    else {
        panic!("必须冻结发送意图");
    };
    let frozen =
        serde_json::json!({"command": command, "attachment_paths": [], "send": send}).to_string();
    let fingerprint =
        multi_agents_manager_lib::services::opencode_native::actions::payload_sha256(&command);
    f.seed_action("b1_pending", "ses-1", "unconfirmed");
    f.seed_action("b1_queued", "ses-1", "queued");
    f.state.store.with(|c| {
        c.execute("INSERT INTO opencode_outbox_targets(session_id,directory,paused) VALUES ('ses-1',?1,0)",
            [f.dir.canonicalize().unwrap().to_string_lossy().as_ref()]).unwrap();
        c.execute("UPDATE opencode_native_actions SET payload_sha256=?1 WHERE action_id='b1_queued'", [&fingerprint]).unwrap();
        c.execute("INSERT INTO opencode_outbox(action_id,session_id,command_json,text_summary,attachment_count) VALUES ('b1_pending','ses-1',NULL,'pending',0),('b1_queued','ses-1',?1,'queued',0)", [&frozen]).unwrap();
    });
    f.peer.set_info_pid(PEER_PID + 999);
    for (i, operation) in [
        serde_json::json!({"kind":"answer","formId":"frm-1","contextToken":"ctx","answers":{}}),
        serde_json::json!({"kind":"decide","requestId":"per-1","contextToken":"ctx","decision":"once"}),
        serde_json::json!({"kind":"decide","requestId":"per-1","contextToken":"ctx","decision":"reject"}),
        serde_json::json!({"kind":"retract","itemId":"inbox-1"}),
        serde_json::json!({"kind":"rebind_outbox","targetActionId":"b1_queued"}),
    ].into_iter().enumerate() {
        let (status, body) = post(&f.app, "/m/api/v1/opencode/actions", COOKIE, serde_json::json!({
            "actionId":format!("b1_attempt_{i}"),"sessionId":"ses-1","bindingToken":"whatever","operation":operation
        }).to_string()).await;
        assert_eq!(status, 409, "{body}");
        assert!(body.contains("OC_GENERATION_MISMATCH"), "{body}");
    }
    // 合法冻结意图和指纹必须通过解析，才能验证派发确实被身份门阻止。
    let error = f
        .state
        .opencode_native
        .flush_outbox(&opencode_session("ses-1", &f.dir))
        .await
        .expect_err("身份未证时 flush 不得成功");
    assert_eq!(error.code, "OC_GENERATION_MISMATCH");
    assert_eq!(
        f.action_row("b1_queued").1.as_deref(),
        Some("OC_GENERATION_MISMATCH")
    );
    let retained: String = f.state.store.with(|c| {
        c.query_row(
            "SELECT command_json FROM opencode_outbox WHERE action_id='b1_queued'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    });
    assert_eq!(retained, frozen, "拒绝派发必须保留冻结意图");
    // 已可能写出的动作仍可 verify：身份未证 ⇒ 只读核对原样返回，绝不改判「确定未写」。
    let (status, body) = post(
        &f.app,
        "/m/api/v1/opencode/actions/b1_pending/verify",
        COOKIE,
        String::new(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["state"],
        "unconfirmed"
    );
    // 已排队项原样保留（状态不动、不退回、不重投）。
    let (status, outbox) = get_bound(
        &f.app,
        "/m/api/v1/opencode/outbox?session_id=ses-1",
        view["bindingToken"].as_str().unwrap(),
    )
    .await;
    assert_eq!(status, 200, "{outbox}");
    assert_eq!(outbox["items"].as_array().unwrap().len(), 2);
    let rows: Vec<(String, String)> = f.state.store.with(|c| {
        let mut stmt = c
            .prepare("SELECT action_id,state FROM opencode_native_actions ORDER BY action_id")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    });
    assert_eq!(
        rows,
        vec![
            ("b1_pending".into(), "unconfirmed".into()),
            ("b1_queued".into(), "queued".into())
        ]
    );
    assert_eq!(f.peer.writes(), 0);
}

#[tokio::test]
async fn service_identity_without_tui_ownership_allows_read_and_local_pause_but_blocks_native_send()
{
    let f = Fixture::new_with_ownership_gate(
        Switch::On,
        |dir| vec![opencode_session("ses-1", dir)],
        true,
        None,
    )
    .await;
    // 升级前的 active 路由和服务身份不能代替可信 TUI 连接证据。
    f.state.store.with(|c| {
        c.execute(
            "INSERT INTO opencode_native_routes (session_id, service_id, runtime_pid, \
                 runtime_started_at_ms, directory, route_state, claimed_at, updated_at) \
                 VALUES ('ses-1', ?1, ?2, ?3, ?4, 'active', 1, 1)",
            rusqlite::params![
                PEER_SERVICE_ID,
                PEER_PID,
                PEER_STARTED_AT_MS,
                f.dir.to_string_lossy()
            ],
        )
        .unwrap();
    });
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "证据链成立时只读投影可用: {view}");
    assert_eq!(view["route"], "opencode_native");
    assert_eq!(view["routeReasonCode"], "OC_INSTANCE_OWNERSHIP_UNVERIFIED");
    assert_eq!(view["capabilities"]["read_messages"]["allowed"], true);
    // 未暂停时验证发送受到归属门禁阻止。
    let (status, body) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        command_body(
            "ses-1",
            "b1_verified_send",
            view["bindingToken"].as_str().unwrap(),
        ),
    )
    .await;
    assert_eq!(status, 409, "缺少 TUI 归属证据不得写: {body}");
    assert!(body.contains("OC_INSTANCE_OWNERSHIP_UNVERIFIED"), "{body}");
    assert_eq!(f.peer.writes(), 0, "归属未证时零原生写");

    // 本地暂停不需要原生写权限。
    let (status, body) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        serde_json::json!({
            "actionId":"b1_local_pause","sessionId":"ses-1","bindingToken":view["bindingToken"],
            "operation":{"kind":"set_outbox_paused","paused":true,"expectedRevision":0}
        })
        .to_string(),
    )
    .await;
    assert_eq!(status, 200, "local pause must not need write gate: {body}");
    let (status, outbox) = get_bound(
        &f.app,
        "/m/api/v1/opencode/outbox?session_id=ses-1",
        view["bindingToken"].as_str().unwrap(),
    )
    .await;
    assert_eq!(status, 200, "{outbox}");
    assert_eq!(outbox["paused"], true);
    assert_eq!(f.peer.writes(), 0, "本地暂停不产生原生写");
}

// 仅验证注入的 coordinator 归属下的寻址；不证明真实服务的身份授权链。
async fn hidden_read_fixture() -> (Fixture, String) {
    let f = hidden_target_fixture().await;
    let (status, initial) = f.view("ses-1").await;
    assert_eq!(status, 200, "先认领 A：{initial}");
    assert_eq!(initial["route"], "opencode_native");
    assert_eq!(f.route_state("ses-1").as_deref(), Some("active"));
    let token = initial["bindingToken"]
        .as_str()
        .expect("A 绑定令牌")
        .to_string();
    f.hide_cards.store(true, Ordering::SeqCst);
    (f, token)
}

fn hidden_pinned_request(uri: &str, token: &str) -> axum::http::Request<axum::body::Body> {
    if uri.contains("/events?") {
        req(
            "GET",
            &format!("{uri}&expected_binding_token={token}"),
            Some(COOKIE),
            None,
        )
    } else {
        let mut request = req("GET", uri, Some(COOKIE), None);
        request
            .headers_mut()
            .insert("x-opencode-expected-binding", token.parse().unwrap());
        request
    }
}

async fn hidden_pinned_get(
    f: &Fixture,
    uri: &str,
    token: &str,
) -> (axum::http::StatusCode, serde_json::Value) {
    let response = f
        .app
        .clone()
        .oneshot(hidden_pinned_request(uri, token))
        .await
        .unwrap();
    let status = response.status();
    let text = body_of(response).await;
    (
        status,
        serde_json::from_str(&text).unwrap_or(serde_json::json!({"raw": text})),
    )
}

#[tokio::test]
async fn hidden_active_form_result_reads_original_answer() {
    let (f, token) = hidden_read_fixture().await;
    f.hide_cards.store(false, Ordering::SeqCst);
    let (status, baseline) = hidden_pinned_get(
        &f,
        "/m/api/v1/opencode/forms/frm-1?session_id=ses-1",
        &token,
    )
    .await;
    assert_eq!(status, 200, "下卡前表单必须可读：{baseline}");
    assert_eq!(baseline["answers"]["q0"]["value"], "blue");
    f.hide_cards.store(true, Ordering::SeqCst);
    let (status, body) = hidden_pinned_get(
        &f,
        "/m/api/v1/opencode/forms/frm-1?session_id=ses-1",
        &token,
    )
    .await;
    assert_eq!(status, 200, "下卡后仍应读取 A 的表单：{body}");
    assert_eq!(body["sessionId"], "ses-1");
    assert_eq!(body["answers"]["q0"]["value"], "blue");
    assert_eq!(f.peer.writes(), 0);
}

#[tokio::test]
async fn hidden_active_agents_read_original_agent() {
    let (f, token) = hidden_read_fixture().await;
    f.hide_cards.store(false, Ordering::SeqCst);
    let (status, baseline) =
        hidden_pinned_get(&f, "/m/api/v1/opencode/agents?session_id=ses-1", &token).await;
    assert_eq!(status, 200, "下卡前 agent 必须可读：{baseline}");
    assert_eq!(baseline["current"], "agent-a");
    assert_eq!(baseline["options"][0]["id"], "agent-a");
    f.hide_cards.store(true, Ordering::SeqCst);
    let (status, body) =
        hidden_pinned_get(&f, "/m/api/v1/opencode/agents?session_id=ses-1", &token).await;
    assert_eq!(status, 200, "下卡后仍应读取 A 的 agent：{body}");
    assert_eq!(body["current"], "agent-a");
    assert_eq!(body["options"][0]["id"], "agent-a");
    assert_eq!(f.peer.writes(), 0);
}

// SSE 必须固定已认领的目标，且目标失效时不能依赖下一条业务修订才断流。
async fn pinned_event_stream(f: &Fixture) -> axum::body::Body {
    use http_body_util::BodyExt as _;
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "先认领 A：{view}");
    assert_eq!(f.route_state("ses-1").as_deref(), Some("active"));
    let token = view["bindingToken"].as_str().expect("A 绑定令牌");
    let response = f
        .app
        .clone()
        .oneshot(hidden_pinned_request(
            "/m/api/v1/opencode/events?session_id=ses-1",
            token,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "active 目标必须可订阅");
    let mut body = response.into_body();
    let frame = tokio::time::timeout(std::time::Duration::from_secs(5), body.frame())
        .await
        .expect("初始修订超时")
        .expect("流提前结束")
        .expect("读取失败");
    assert!(std::str::from_utf8(frame.data_ref().unwrap())
        .unwrap()
        .contains("event: revision"));
    body
}

async fn assert_event_stream_ends_without_revision(mut body: axum::body::Body) {
    use http_body_util::BodyExt as _;
    let next = tokio::time::timeout(std::time::Duration::from_secs(3), body.frame())
        .await
        .expect("目标失效后无业务修订时 SSE 仍未主动结束");
    assert!(
        next.is_none(),
        "目标失效后不得继续发送修订或 keepalive：{next:?}"
    );
}

#[tokio::test]
async fn events_before_active_route_do_not_open_unpinned_stream() {
    let f = Fixture::single(Switch::On).await;
    assert_eq!(f.route_state("ses-1"), None);
    let response = f
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
    if response.status() == 200 {
        assert_event_stream_ends_without_revision(response.into_body()).await;
    }
    assert_eq!(f.route_state("ses-1"), None);
}

#[tokio::test]
async fn pinned_events_end_when_route_drains_without_revision() {
    let f = Fixture::single(Switch::On).await;
    let body = pinned_event_stream(&f).await;
    let revision = f.state.opencode_native.watch("ses-1");
    f.state.store.with(|c| {
        assert_eq!(
            c.execute(
                "UPDATE opencode_native_routes SET route_state='draining' WHERE session_id='ses-1'",
                []
            )
            .unwrap(),
            1
        );
    });
    assert_eq!(f.route_state("ses-1").as_deref(), Some("draining"));
    assert!(
        !revision.has_changed().unwrap(),
        "路由变化不得靠业务修订触发测试"
    );
    assert_event_stream_ends_without_revision(body).await;
}

#[tokio::test]
async fn pinned_events_end_when_source_generation_changes_without_revision() {
    let f = Fixture::single(Switch::On).await;
    let body = pinned_event_stream(&f).await;
    let revision = f.state.opencode_native.watch("ses-1");
    let b = Peer::start(&f.dir).await;
    b.set_info_pid(9999);
    std::fs::write(f._temp.path().join("service.json"), format!(
        r#"{{"id":"svc-b","version":"{PEER_VERSION}","url":"{}","pid":9999,"password":"pw-fixture"}}"#, b.endpoint
    )).unwrap();
    let mut other = opencode_session("ses-2", &f.dir);
    other.pid = 9999;
    *f.scan_override.lock().unwrap() = Some(vec![opencode_session("ses-1", &f.dir), other]);
    let (status, view) = f.view("ses-2").await;
    assert_eq!(status, 200, "另一会话必须安装 B：{view}");
    assert_eq!(view["serviceId"], "svc-b", "必须确认来源确已换代：{view}");
    assert_eq!(f.route_state("ses-1").as_deref(), Some("active"));
    assert!(
        !revision.has_changed().unwrap(),
        "B 安装不得以 A 的业务修订唤醒旧流"
    );
    assert_event_stream_ends_without_revision(body).await;
}

#[tokio::test]
async fn hidden_active_events_allow_original_revision_subscription() {
    use http_body_util::BodyExt as _;
    let (f, token) = hidden_read_fixture().await;
    f.hide_cards.store(false, Ordering::SeqCst);
    let baseline = f
        .app
        .clone()
        .oneshot(hidden_pinned_request(
            "/m/api/v1/opencode/events?session_id=ses-1",
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(baseline.status(), 200, "下卡前必须能订阅");
    assert!(baseline.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/event-stream"));
    drop(baseline);
    f.hide_cards.store(true, Ordering::SeqCst);
    let response = f
        .app
        .clone()
        .oneshot(hidden_pinned_request(
            "/m/api/v1/opencode/events?session_id=ses-1",
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "下卡后仍应能订阅 A");
    assert!(response.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/event-stream"));
    let mut stream = response.into_body();
    let frame = tokio::time::timeout(std::time::Duration::from_secs(5), stream.frame())
        .await
        .expect("应收到初始修订")
        .unwrap()
        .unwrap();
    assert!(std::str::from_utf8(frame.data_ref().unwrap())
        .unwrap()
        .contains("event: revision"));
    assert_eq!(f.peer.writes(), 0);
}

// B 占用同一 session ID 和目录，但拥有不同的服务代际及可识别内容。
// 只改私有临时登记文件，令 A 离线，避免读取 A 成功掩盖误绑定 B。
async fn replace_hidden_generation(f: &Fixture) -> Peer {
    let b = Peer::start(&f.dir).await;
    b.set_info_pid(9999);
    b.shared.prompts.lock().unwrap().push(serde_json::json!({"type":"user","id":"b-message","text":"view-b-secret","time":{"created":1}}));
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    for (path, marker) in [
        ("/api/session/ses-1/message", "view-b-secret"),
        ("/api/session/ses-1/form/frm-1", "form-b-secret"),
        ("/api/agent", "agent-b-secret"),
    ] {
        let body = client
            .get(format!("{}{path}", b.endpoint))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(body.contains(marker), "B 必须实际提供可识别的内容：{body}");
    }
    f.peer.shared.offline.store(true, Ordering::SeqCst);
    std::fs::write(f._temp.path().join("service.json"), format!(r#"{{"id":"svc-b","version":"{PEER_VERSION}","url":"{}","pid":9999,"password":"pw-fixture"}}"#, b.endpoint)).unwrap();
    b
}

// 已授权 A 后，扫描返回同 ID 的异目录 B；登记文件及两个 peer 均为临时夹具。
async fn visible_other_directory_fixture() -> (Fixture, Peer, String) {
    let f = Fixture::single(Switch::On).await;
    let (status, initial) = f.view("ses-1").await;
    assert_eq!(status, 200, "先授权 A：{initial}");
    assert_eq!(f.route_state("ses-1").as_deref(), Some("active"));
    let token = initial["bindingToken"].as_str().unwrap().to_string();
    let other_dir = f._temp.path().join("other-project-visible");
    std::fs::create_dir_all(&other_dir).unwrap();
    let other_dir = other_dir.canonicalize().unwrap();
    let b = Peer::start(&other_dir).await;
    b.set_info_pid(9999);
    b.shared.prompts.lock().unwrap().push(serde_json::json!({
        "type":"user", "id":"b-message", "text":"visible-b-secret", "time":{"created":1}
    }));
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let b_session: serde_json::Value = client
        .get(format!("{}/api/session/ses-1", b.endpoint))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        b_session["location"]["directory"],
        other_dir.to_str().unwrap()
    );
    let mut card = opencode_session("ses-1", &other_dir);
    card.pid = 9999;
    *f.scan_override.lock().unwrap() = Some(vec![card]);
    std::fs::write(f._temp.path().join("service.json"), format!(
        r#"{{"id":"svc-b","version":"{PEER_VERSION}","url":"{}","pid":9999,"password":"pw-fixture"}}"#, b.endpoint
    )).unwrap();
    (f, b, token)
}

fn active_target(f: &Fixture) -> (String, i64, i64, String, String) {
    f.state.store.with(|c| c.query_row(
        "SELECT service_id,runtime_pid,runtime_started_at_ms,directory,route_state FROM opencode_native_routes WHERE session_id='ses-1'",
        [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
    ).unwrap())
}

#[tokio::test]
async fn visible_same_id_other_directory_view_keeps_authorized_target() {
    let (f, b, token) = visible_other_directory_fixture().await;
    let original = active_target(&f);
    assert_eq!(
        original,
        (
            PEER_SERVICE_ID.to_string(),
            i64::from(PEER_PID),
            PEER_STARTED_AT_MS,
            f.dir.to_string_lossy().into_owned(),
            "active".to_string()
        )
    );
    let (status, body) = f.view("ses-1").await;
    assert_eq!(
        active_target(&f),
        original,
        "视图不得换绑到 B：{status} {body}"
    );
    assert!(
        !body.to_string().contains("visible-b-secret"),
        "不得返回 B 内容：{body}"
    );
    if let Some(returned) = body["bindingToken"].as_str() {
        assert_eq!(returned, token, "不得发布 B 的绑定令牌：{body}");
    }
    assert_eq!(b.writes(), 0);
}

#[tokio::test]
async fn visible_same_id_other_directory_action_keeps_authorized_target() {
    let (f, b, token) = visible_other_directory_fixture().await;
    let original = active_target(&f);
    let b_reads_before = b.shared.session_reads.load(Ordering::SeqCst);
    let (status, body) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        command_body("ses-1", "visible_other_action", &token),
    )
    .await;
    assert_eq!(
        active_target(&f),
        original,
        "动作不得换绑到 B：{status} {body}"
    );
    assert_eq!(
        b.shared.session_reads.load(Ordering::SeqCst),
        b_reads_before,
        "动作必须先遵守 A 固定目标，不能读取 B 会话：{status} {body}"
    );
    assert_eq!(b.writes(), 0, "动作不得写入异目录 B：{status} {body}");
    assert!(
        !body.contains("visible-b-secret"),
        "动作不得返回 B 内容：{body}"
    );
    // 能读到 A 时应继续服务已授权目标；无法核对 A 时应保守拒绝。
    assert!(
        status != 200 || f.peer.prompt_writes() == 1,
        "不能跳过 A 并把 B 当作操作目标：{body}"
    );
}

async fn assert_hidden_replacement_refused(uri: &str) {
    let (f, token) = hidden_read_fixture().await;
    let b = replace_hidden_generation(&f).await;
    let response = f
        .app
        .clone()
        .oneshot(hidden_pinned_request(uri, &token))
        .await
        .unwrap();
    let status = response.status();
    // 对 SSE 不等待无限流；成功订阅本身就会把旧 A 关联到新代际。
    assert_ne!(status, 200, "A 离线且代际更换时不得读或订阅 B：{uri}");
    let body = body_of(response).await;
    for marker in ["view-b-secret", "form-b-secret", "agent-b-secret"] {
        assert!(!body.contains(marker), "不得泄漏 B 内容：{body}");
    }
    assert_eq!(f.route_state("ses-1").as_deref(), Some("active"));
    assert_eq!(b.writes(), 0);
}

#[tokio::test]
async fn hidden_active_view_rejects_replacement_generation() {
    assert_hidden_replacement_refused("/m/api/v1/opencode/view?session_id=ses-1").await;
}

#[tokio::test]
async fn hidden_active_form_result_rejects_replacement_generation() {
    assert_hidden_replacement_refused("/m/api/v1/opencode/forms/frm-1?session_id=ses-1").await;
}

#[tokio::test]
async fn hidden_active_agents_reject_replacement_generation() {
    assert_hidden_replacement_refused("/m/api/v1/opencode/agents?session_id=ses-1").await;
}

#[tokio::test]
async fn hidden_active_events_reject_replacement_generation() {
    assert_hidden_replacement_refused("/m/api/v1/opencode/events?session_id=ses-1").await;
}

#[tokio::test]
async fn hidden_authorized_target_view_recovers_original_binding() {
    let f = Fixture::single(Switch::On).await;
    let (status, initial) = f.view("ses-1").await;
    assert_eq!(status, 200, "{initial}");
    assert_eq!(initial["route"], "opencode_native");
    f.hide_cards.store(true, Ordering::SeqCst);
    let (status, recovered) = f.view("ses-1").await;
    assert_eq!(status, 200, "已授权目标下卡后应恢复视图: {recovered}");
    assert_eq!(recovered["serviceId"], initial["serviceId"]);
    assert_eq!(recovered["servicePid"], initial["servicePid"]);
    assert_eq!(recovered["bindingToken"], initial["bindingToken"]);
}

async fn assert_view_does_not_publish_after_route_change(route: &str) {
    let f = Fixture::single(Switch::On).await;
    let (status, initial) = f.view("ses-1").await;
    assert_eq!(status, 200, "{initial}");
    assert_eq!(initial["route"], "opencode_native");
    assert!(initial["bindingToken"].as_str().is_some());

    // read permit 阻住认领的 write permit；观察锁的公平让位确认完整投影已完成。
    let held = f.state.opencode_native.try_legacy_permit("ses-1").await;
    let app = f.app.clone();
    let viewing = tokio::spawn(async move {
        let response = app
            .oneshot(req(
                "GET",
                "/m/api/v1/opencode/view?session_id=ses-1",
                Some(COOKIE),
                None,
            ))
            .await
            .unwrap();
        let status = response.status();
        let text = body_of(response).await;
        (
            status,
            serde_json::from_str::<serde_json::Value>(&text).unwrap_or_default(),
        )
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if f.state
                .opencode_native
                .try_legacy_permit_now("ses-1")
                .is_none()
            {
                break;
            }
            assert!(!viewing.is_finished(), "视图必须在认领前等待");
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("视图必须完成投影并等待认领许可");
    f.state.store.with(|c| {
        if route == "g2" {
            c.execute("UPDATE opencode_native_routes SET service_id='svc-g2',runtime_pid=7412,runtime_started_at_ms=2000 WHERE session_id='ses-1'", []).unwrap();
        } else {
            c.execute("UPDATE opencode_native_routes SET route_state='draining' WHERE session_id='ses-1'", []).unwrap();
        }
    });
    drop(held);
    let (status, body) = viewing.await.expect("view task");
    let row: (String, String, i64) = f.state.store.with(|c| c.query_row(
        "SELECT route_state,service_id,runtime_pid FROM opencode_native_routes WHERE session_id='ses-1'", [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))
    ).unwrap());
    let expected = if route == "g2" {
        ("active", "svc-g2", 7412)
    } else {
        ("draining", PEER_SERVICE_ID, i64::from(PEER_PID))
    };
    assert_eq!(
        (row.0.as_str(), row.1.as_str(), row.2),
        expected,
        "过期视图不得认领路由: {status} {body}"
    );
    assert!(
        body.get("bindingToken").is_none_or(|v| v.is_null()),
        "过期视图不得发布 token: {status} {body}"
    );
    assert!(
        body.get("messages").is_none_or(|v| v.is_null()),
        "过期视图不得发布投影: {status} {body}"
    );
}

#[tokio::test]
async fn view_does_not_reclaim_or_publish_after_route_changes_to_g2() {
    assert_view_does_not_publish_after_route_change("g2").await;
}

#[tokio::test]
async fn view_does_not_publish_after_route_changes_to_draining() {
    assert_view_does_not_publish_after_route_change("draining").await;
}

#[tokio::test]
async fn route_change_after_recheck_preserves_queued_message_without_writing() {
    let f = Fixture::single(Switch::On).await;
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["route"], "opencode_native");
    let token = view["bindingToken"].as_str().unwrap();
    let pause = serde_json::json!({
        "actionId":"race_pause", "sessionId":"ses-1", "bindingToken":token,
        "operation":{"kind":"set_outbox_paused","paused":true,"expectedRevision":0}
    })
    .to_string();
    assert_eq!(
        post(&f.app, "/m/api/v1/opencode/actions", COOKIE, pause)
            .await
            .0,
        200
    );
    let (status, body) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        command_body("ses-1", "race_queued", token),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["state"],
        "queued"
    );
    let frozen: String = f.state.store.with(|c| {
        c.query_row(
            "SELECT command_json FROM opencode_outbox WHERE action_id='race_queued'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    });
    f.state.store.with(|c| {
        assert_eq!(c.execute(
        "UPDATE opencode_outbox_targets SET paused=0 WHERE session_id='ses-1' AND directory=?1",
        [f.dir.canonicalize().unwrap().to_string_lossy().as_ref()]
    ).unwrap(), 1, "必须解暂停原目录目标")
    });

    // 先停在 flush 投影的 message 读；此时武装下一次 info，保证拦的是
    // recheck 之后的写通道复核，而不是投影开始时的 info。
    f.peer.hold_next_message_read();
    let hub = f.state.opencode_native.clone();
    let card = opencode_session("ses-1", &f.dir);
    let flush = tokio::spawn(async move { hub.flush_outbox(&card).await });
    f.peer.wait_until_read_blocked().await;
    f.peer.shared.hold_info_read.store(true, Ordering::SeqCst);
    f.peer.release_message_read();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        f.peer.shared.info_blocked.notified(),
    )
    .await
    .expect("recheck 后的写通道复核必须停住");
    f.state.store.with(|c| c.execute(
        "UPDATE opencode_native_routes SET service_id='svc-g2', runtime_pid=7412, runtime_started_at_ms=2000 WHERE session_id='ses-1'", []
    ).unwrap());
    f.peer.shared.info_release.notify_one();
    let _ = flush.await.expect("flush task");
    assert_eq!(f.peer.writes(), 0, "G1 失去路由后不得写原生服务");
    assert_eq!(f.action_row("race_queued").0, "queued");
    let after: String = f.state.store.with(|c| {
        c.query_row(
            "SELECT command_json FROM opencode_outbox WHERE action_id='race_queued'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    });
    assert_eq!(after, frozen, "排队内容必须保留");
}

#[tokio::test]
async fn local_outbox_control_survives_ambiguous_service_gate() {
    // 同目录/同机多个 service ⇒ 证据不明（保守闸）：原生写拒绝，纯本地操作不受影响。
    // （进程表冲突在单测层注入，见 actions::tests::ambiguous_service_count_refuses_new_writes；
    // 本用例锁 HTTP 面：冲突=false 时本地操作零原生写地完成。）
    let f = Fixture::new_with_ownership_gate(
        Switch::On,
        |dir| vec![opencode_session("ses-1", dir)],
        true,
        None,
    )
    .await;
    f.seed_action("b1_local_queued", "ses-1", "queued");
    f.state.store.with(|c| c.execute("INSERT INTO opencode_outbox(action_id,session_id,command_json,text_summary,attachment_count) VALUES ('b1_local_queued','ses-1','{}','queued',0)",[]).unwrap());
    let (_, view) = f.view("ses-1").await;
    let binding = view["bindingToken"].as_str().unwrap().to_string();
    let (status, body) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        serde_json::json!({
            "actionId":"b1_local_retract","sessionId":"ses-1","bindingToken":binding,
            "operation":{"kind":"retract_outbox","targetActionId":"b1_local_queued"}
        })
        .to_string(),
    )
    .await;
    assert_eq!(
        status, 200,
        "unclaimed local retract must not need the write gate: {body}"
    );
    let (status, outbox) = get_bound(
        &f.app,
        "/m/api/v1/opencode/outbox?session_id=ses-1",
        &binding,
    )
    .await;
    assert_eq!(status, 200, "{outbox}");
    let items = outbox["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["state"], "not_sent");
    assert_eq!(items[0]["reasonCode"], "outbox_retracted");
    assert_eq!(f.peer.writes(), 0);
}

// 先通过真实 HTTP 入队，保留生产计算的意图摘要，再模拟持久账本中的不同在途状态。
async fn duplicate_hidden_action_fixture(
    state: &str,
    replace_directory: bool,
) -> (Fixture, String) {
    let f = Fixture::single(Switch::On).await;
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "先认领原项目：{view}");
    let token = view["bindingToken"].as_str().unwrap();
    let pause = serde_json::json!({
        "actionId": "duplicate_pause", "sessionId": "ses-1", "bindingToken": token,
        "operation": {"kind": "set_outbox_paused", "paused": true, "expectedRevision": 0}
    })
    .to_string();
    let (status, text) = post(&f.app, "/m/api/v1/opencode/actions", COOKIE, pause).await;
    assert_eq!(status, 200, "暂停队列：{text}");
    let body = command_body("ses-1", "duplicate_pending", token);
    let (status, text) = post(&f.app, "/m/api/v1/opencode/actions", COOKIE, body.clone()).await;
    assert_eq!(status, 200, "首次入队：{text}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&text).unwrap()["state"],
        "queued"
    );
    f.state.store.with(|c| {
        c.execute(
            "UPDATE opencode_native_actions SET state=?1 WHERE action_id='duplicate_pending'",
            [state],
        )
        .unwrap()
    });
    assert_eq!(f.action_row("duplicate_pending").0, state);
    assert_eq!(f.peer.writes(), 0, "暂停队列时无原生写");
    f.hide_cards.store(true, Ordering::SeqCst);
    // 重命名保留原目录但使原路径不可访问；另一形态在旧路径放普通文件。
    std::fs::rename(&f.dir, f._temp.path().join("moved-project")).unwrap();
    if replace_directory {
        std::fs::write(&f.dir, "replaced").unwrap();
    }
    (f, body)
}

async fn assert_duplicate_hidden_action_returns_ledger(state: &str, replace_directory: bool) {
    let (f, body) = duplicate_hidden_action_fixture(state, replace_directory).await;
    let before = f.peer.writes();
    let (status, text) = post(&f.app, "/m/api/v1/opencode/actions", COOKIE, body).await;
    assert_eq!(f.peer.writes(), before, "重复 POST 不得新增原生写");
    assert!(
        f.injector.texts().is_empty() && f.injector.keys().is_empty(),
        "不得新增终端写"
    );
    assert_eq!(f.action_rows("duplicate_pending"), 1);
    assert_eq!(f.action_row("duplicate_pending").0, state);
    assert!(
        !text.contains("OC_ACTION_INVALID"),
        "在途动作不能被归类为确定零写：{status} {text}"
    );
    assert_eq!(status, 200, "重复请求应返回原账本状态：{text}");
    let result: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(result["actionId"], "duplicate_pending");
    assert_eq!(result["state"], state);
}

// A 在 reserve 前的原生投影读停住；B 到达 fresh scan 表明早期账本查询已返回无行。
async fn duplicate_race_start() -> (
    Fixture,
    tokio::task::JoinHandle<(axum::http::StatusCode, String)>,
    tokio::task::JoinHandle<(axum::http::StatusCode, String)>,
    String,
) {
    let f = Fixture::single(Switch::On).await;
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "先认领：{view}");
    let body = command_body(
        "ses-1",
        "race_duplicate_context",
        view["bindingToken"].as_str().unwrap(),
    );
    f.peer.hold_next_message_read();
    let app = f.app.clone();
    let first_body = body.clone();
    let a =
        tokio::spawn(
            async move { post(&app, "/m/api/v1/opencode/actions", COOKIE, first_body).await },
        );
    f.peer.wait_until_read_blocked().await;
    assert_eq!(
        f.action_rows("race_duplicate_context"),
        0,
        "A 还在 reserve 前"
    );
    assert_eq!(f.peer.writes(), 0);
    f.scan_gate.armed.store(true, Ordering::SeqCst);
    let app = f.app.clone();
    let second_body = body.clone();
    let b =
        tokio::spawn(
            async move { post(&app, "/m/api/v1/opencode/actions", COOKIE, second_body).await },
        );
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        f.scan_gate.entered.notified(),
    )
    .await
    .expect("B 必须越过早期查账并停在扫描");
    assert_eq!(f.action_rows("race_duplicate_context"), 0, "B 查账后仍无行");
    (f, a, b, body)
}

// 同一上下文以全新 actionId 走 HTTP；无账本命中时必须暴露解析失败。
async fn assert_context_unavailable(f: &Fixture, body: &str) {
    let mut control: serde_json::Value = serde_json::from_str(body).unwrap();
    control["actionId"] = serde_json::json!("race_context_control");
    control["operation"]["clientMessageId"] = serde_json::json!("msg_race_context_control");
    let (status, text) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        control.to_string(),
    )
    .await;
    assert_eq!(
        status, 409,
        "缓存绑定恢复后目录不可解析必须受控拒绝：{text}"
    );
    let response: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        response["reasonCode"], "OC_DIRECTORY_UNRESOLVED",
        "固定目标目录已移走：{text}"
    );
    assert_eq!(f.action_rows("race_context_control"), 0);
}

#[tokio::test]
async fn duplicate_context_failure_returns_first_receipt_after_first_write() {
    let (f, a, b, body) = duplicate_race_start().await;
    // B 已越过早期查账并停在扫描；先让持有同 actionId 锁的 A 完成。
    f.peer.release_message_read();
    let (a_status, a_body) = tokio::time::timeout(std::time::Duration::from_secs(10), a)
        .await
        .expect("A 响应超时")
        .unwrap();
    assert_eq!(a_status, 200, "A 必须实际完成：{a_body}");
    assert_eq!(f.peer.prompt_writes(), 1, "A 真实写出一次");
    f.hide_cards.store(true, Ordering::SeqCst);
    std::fs::rename(&f.dir, f._temp.path().join("race-moved-project")).unwrap();
    f.state.store.with(|c| assert_eq!(c.execute(
        "UPDATE opencode_native_routes SET route_state='draining' WHERE session_id='ses-1' AND route_state='active'", [],
    ).unwrap(), 1, "禁用 active 路由恢复"));
    assert_eq!(
        f.action_rows("race_duplicate_context"),
        1,
        "保留 A 的动作账本"
    );
    assert_context_unavailable(&f, &body).await;
    f.scan_gate.release();
    let (b_status, b_body) = tokio::time::timeout(std::time::Duration::from_secs(10), b)
        .await
        .expect("B 响应超时")
        .unwrap();
    assert_eq!(b_status, 200, "B 应回读同一动作：{b_body}");
    assert_eq!(f.peer.prompt_writes(), 1, "B 不得重复写出");
    assert_eq!(f.peer.writes(), 1, "原生服务写入总计一次");
    assert!(
        f.injector.texts().is_empty() && f.injector.keys().is_empty(),
        "不得产生终端注入"
    );
    assert_eq!(f.action_rows("race_duplicate_context"), 1);
    let first_reply: serde_json::Value = serde_json::from_str(&a_body).unwrap();
    let reply: serde_json::Value = serde_json::from_str(&b_body).unwrap();
    assert_eq!(first_reply["actionId"], "race_duplicate_context");
    assert_eq!(reply["actionId"], first_reply["actionId"]);
    assert_eq!(reply["state"], first_reply["state"]);
    let code = reply["reasonCode"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_uppercase();
    // 与手机端 DEFINITE_REFUSAL_CODES 对齐，避免换一个确定零写码后误过。
    let definite = [
        "OC_ACTION_INVALID",
        "OC_COMMAND_UNSUPPORTED",
        "OC_SEND_EMPTY",
        "OC_SEND_TOO_LONG",
        "OC_ATTACHMENT_REFUSED",
        "OC_ANSWER_INCOMPLETE",
        "OC_ANSWER_INVALID",
        "OC_ANSWER_UNSUPPORTED",
        "OC_FORM_INVALID",
        "OC_ACTION_CONFLICT",
        "OC_BINDING_STALE",
        "OC_ROUTE_NOT_OWNED",
        "OC_SESSION_MISMATCH",
        "OC_DIRECTORY_UNRESOLVED",
        "OC_DIRECTORY_MISMATCH",
        "OC_ROUTE_CHANGED",
        "OC_PLATFORM_UNVERIFIED",
        "OC_INSTANCE_OWNERSHIP_UNVERIFIED",
        "OC_INSTANCE_AMBIGUOUS",
        "OC_DESKTOP_BINDING_REQUIRED",
        "OC_RUNTIME_UNVERIFIED",
    ];
    assert!(
        !definite.contains(&code.as_str()),
        "B 不得在 A 稍后写出时报告确定拒绝：{b_status} {b_body}"
    );
    assert!(
        !matches!(reply["state"].as_str(), Some("not_sent" | "not_accepted")),
        "B 不得返回确定零写状态：{b_body}"
    );
}

#[tokio::test]
async fn duplicate_context_failure_returns_unknown_claimed_after_early_lookup() {
    let (f, a, b, body) = duplicate_race_start().await;
    f.peer.release_message_read();
    let (a_status, a_body) = tokio::time::timeout(std::time::Duration::from_secs(10), a)
        .await
        .expect("A 响应超时")
        .unwrap();
    assert_eq!(a_status, 200, "A 必须真实 claim 并写出：{a_body}");
    assert_eq!(f.peer.prompt_writes(), 1);
    assert_eq!(f.action_rows("race_duplicate_context"), 1);
    // 模拟写已发生但回执丢失的持久 Unknown；摘要仍是 A 真实命令生成的值。
    f.state.store.with(|c| c.execute("UPDATE opencode_native_actions SET state='unconfirmed', reason_code='timeout' WHERE action_id='race_duplicate_context'", []).unwrap());
    f.hide_cards.store(true, Ordering::SeqCst);
    std::fs::rename(&f.dir, f._temp.path().join("race-moved-project")).unwrap();
    f.state.store.with(|c| assert_eq!(c.execute(
        "UPDATE opencode_native_routes SET route_state='draining' WHERE session_id='ses-1' AND route_state='active'", [],
    ).unwrap(), 1, "禁用 active 路由恢复"));
    assert_eq!(
        f.action_rows("race_duplicate_context"),
        1,
        "保留 A 的动作账本"
    );
    assert_context_unavailable(&f, &body).await;
    f.scan_gate.release();
    let (status, body) = tokio::time::timeout(std::time::Duration::from_secs(10), b)
        .await
        .expect("B 响应超时")
        .unwrap();
    assert_eq!(f.peer.prompt_writes(), 1, "B 不得重写");
    assert_eq!(f.peer.writes(), 1, "原生服务写入总计一次");
    assert!(
        f.injector.texts().is_empty() && f.injector.keys().is_empty(),
        "不得产生终端注入"
    );
    assert_eq!(f.action_rows("race_duplicate_context"), 1);
    assert_eq!(f.action_row("race_duplicate_context").0, "unconfirmed");
    assert_eq!(status, 200, "B 应回读并返回已有 Unknown：{body}");
    let result: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(result["actionId"], "race_duplicate_context");
    assert_eq!(result["state"], "unconfirmed");
}

macro_rules! hidden_duplicate_case {
    ($name:ident, $state:literal, $replace:expr) => {
        #[tokio::test]
        async fn $name() {
            assert_duplicate_hidden_action_returns_ledger($state, $replace).await;
        }
    };
}

hidden_duplicate_case!(
    duplicate_hidden_missing_directory_unconfirmed,
    "unconfirmed",
    false
);
hidden_duplicate_case!(
    duplicate_hidden_missing_directory_dispatching,
    "dispatching",
    false
);
hidden_duplicate_case!(duplicate_hidden_missing_directory_queued, "queued", false);
hidden_duplicate_case!(
    duplicate_hidden_replaced_directory_unconfirmed,
    "unconfirmed",
    true
);
hidden_duplicate_case!(
    duplicate_hidden_replaced_directory_dispatching,
    "dispatching",
    true
);
hidden_duplicate_case!(duplicate_hidden_replaced_directory_queued, "queued", true);

#[tokio::test]
async fn duplicate_hidden_action_does_not_disclose_another_devices_ledger() {
    let (f, body) = duplicate_hidden_action_fixture("unconfirmed", false).await;
    let before = f.peer.writes();
    let (status, text) = post(&f.app, "/m/api/v1/opencode/actions", COOKIE_OTHER, body).await;
    assert_ne!(status, 200, "其他设备不能读取属主动作：{text}");
    let response: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(response.get("state").is_none(), "不得泄露动作状态：{text}");
    assert_eq!(f.peer.writes(), before);
    assert!(f.injector.texts().is_empty() && f.injector.keys().is_empty());
    assert_eq!(f.action_rows("duplicate_pending"), 1);
    assert_eq!(f.action_row("duplicate_pending").0, "unconfirmed");
}

// 以下使用现有协调器测试缝隔离路由选择；该缝绕过桌面归属验证，不能证明生产归属。
#[tokio::test]
async fn explicit_reselection_switches_active_target_without_pending_actions() {
    let (f, b, token_a) = visible_other_directory_fixture().await;
    let original = active_target(&f);
    let (status, refreshed) = f.view("ses-1").await;
    assert_eq!(
        active_target(&f),
        original,
        "普通刷新不得选择 B：{status} {refreshed}"
    );
    assert!(!refreshed.to_string().contains("visible-b-secret"));
    if let Some(token) = refreshed["bindingToken"].as_str() {
        assert_eq!(token, token_a);
    }

    let (status, body) = post(
        &f.app,
        "/m/api/v1/opencode/route/native",
        COOKIE,
        f.native_route_request("ses-1", &token_a, false).await,
    )
    .await;
    assert_eq!(status, 200, "显式选择 B 应成功：{body}");
    let target = active_target(&f);
    assert_eq!(target.0, "svc-b");
    assert_eq!(target.1, 9999);
    assert_eq!(target.3, b.shared.directory);
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "选择后可读取 B：{view}");
    assert_eq!(view["serviceId"], "svc-b");
    assert!(view.to_string().contains("visible-b-secret"));
    assert_ne!(view["bindingToken"].as_str().expect("B token"), token_a);
    assert_eq!(f.peer.writes(), 0);
    assert_eq!(b.writes(), 0);
}

async fn assert_explicit_reselection_preserves_pending_action(state: &str) {
    let (f, b, token_a) = visible_other_directory_fixture().await;
    let original = active_target(&f);
    f.seed_action("reselect-pending", "ses-1", state);
    let action_target = || {
        f.state.store.with(|c| c.query_row(
        "SELECT service_id, runtime_pid, runtime_started_at_ms, directory, state FROM opencode_native_actions WHERE action_id = 'reselect-pending'",
        [], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?))
    ).unwrap())
    };
    let action_before = action_target();
    let (status, refreshed) = f.view("ses-1").await;
    assert_eq!(
        active_target(&f),
        original,
        "普通刷新不得换绑：{status} {refreshed}"
    );
    assert!(!refreshed.to_string().contains("visible-b-secret"));
    if let Some(token) = refreshed["bindingToken"].as_str() {
        assert_eq!(token, token_a);
    }
    let (status, body) = post(
        &f.app,
        "/m/api/v1/opencode/route/native",
        COOKIE,
        f.native_route_request("ses-1", &token_a, false).await,
    )
    .await;
    assert_eq!(
        active_target(&f),
        original,
        "未决动作不得换绑：{status} {body}"
    );
    assert_eq!(
        action_target(),
        action_before,
        "不得迁移动作代际、目录或改变状态"
    );
    assert_eq!(f.action_rows("reselect-pending"), 1);
    assert_eq!(f.peer.writes(), 0, "不得向 A 重发");
    assert_eq!(b.writes(), 0, "不得向 B 重发");
    assert!(f.injector.texts().is_empty() && f.injector.keys().is_empty());
    assert_ne!(status, 200, "未决动作必须阻止选择 B：{body}");
}

#[tokio::test]
async fn explicit_reselection_preserves_queued_action_on_original_target() {
    assert_explicit_reselection_preserves_pending_action("queued").await;
}

#[tokio::test]
async fn explicit_reselection_preserves_unconfirmed_action_on_original_target() {
    assert_explicit_reselection_preserves_pending_action("unconfirmed").await;
}

// ============================================================
// patch3 D2：显式确认终态未知（死代际 acknowledged 行的用户可达出口）
// ============================================================

/// 活代际（当前投影可证同代际）的 acknowledged 行仍可只读核对 → 确认被拒、零改写。
#[tokio::test]
async fn confirm_unknown_refuses_a_verifiable_current_generation_row() {
    let f = Fixture::single(Switch::On).await;
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    f.seed_action("act_live", "ses-1", "acknowledged");
    f.seed_claim("act_live", "ses-1", "form:ses-1:frm_live");

    let (status, text) = post(
        &f.app,
        "/m/api/v1/opencode/actions/act_live/confirm-unknown",
        COOKIE,
        String::new(),
    )
    .await;
    assert_eq!(status, 409, "活代际必须拒绝确认：{text}");
    assert!(text.contains("OC_ACTION_STATE"), "{text}");
    assert_eq!(f.action_row("act_live").0, "acknowledged", "拒绝必须零改写");
    assert_eq!(f.claim_rows("act_live"), 1, "拒绝不得释放占用");
    assert_eq!(f.peer.writes(), 0, "确认绝不触发对端写");
}

/// 死代际 acknowledged 行：未决屏障挡住显式重选 → 确认终态未知 → 归档态
/// （unknown_confirmed + 审计词 + 占用释放）→ 显式重选屏障解除。全程零对端写。
#[tokio::test]
async fn confirm_unknown_archives_dead_generation_action_and_unlocks_reselection() {
    let (f, b, token_a) = visible_other_directory_fixture().await;
    f.seed_action_generation("act_dead", "ses-1", "acknowledged", "svc-old", 1111, 999);
    f.seed_claim("act_dead", "ses-1", "form:ses-1:frm_dead");

    // 未决屏障在场：显式重选 B 被拒（H13 S3/S6 同形态）
    let (status, body) = post(
        &f.app,
        "/m/api/v1/opencode/route/native",
        COOKIE,
        f.native_route_request("ses-1", &token_a, false).await,
    )
    .await;
    assert_ne!(status, 200, "未决动作必须挡住重选：{body}");
    assert_eq!(f.action_row("act_dead").0, "acknowledged");

    // 确认终态未知：归档态 + 审计词 + 占用释放
    let (status, text) = post(
        &f.app,
        "/m/api/v1/opencode/actions/act_dead/confirm-unknown",
        COOKIE,
        String::new(),
    )
    .await;
    assert_eq!(status, 200, "{text}");
    let view: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(view["state"], "unknown_confirmed");
    assert_eq!(view["reasonCode"], "user_confirmed_unknown");
    assert_eq!(f.claim_rows("act_dead"), 0, "归档态释放待答占用");

    // 屏障解除：显式重选 B 成功；归档行零触碰（不重发、不迁移）
    let (status, body) = post(
        &f.app,
        "/m/api/v1/opencode/route/native",
        COOKIE,
        f.native_route_request("ses-1", &token_a, false).await,
    )
    .await;
    assert_eq!(status, 200, "确认后重选屏障解除：{body}");
    assert_eq!(active_target(&f).0, "svc-b");
    let (state, reason, service) = f.action_row("act_dead");
    assert_eq!(state, "unknown_confirmed", "归档态不可再变");
    assert_eq!(reason.as_deref(), Some("user_confirmed_unknown"));
    assert_eq!(service, "svc-old", "归档行不迁移代际");
    assert_eq!(f.peer.writes(), 0, "全程零对端写（不重发）");
    assert_eq!(b.writes(), 0);
}

/// 确认死代际动作不会重新启用已退役的旧路由。
#[tokio::test]
async fn confirm_unknown_does_not_reenable_retired_legacy_route() {
    let f = Fixture::single(Switch::On).await;
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    let token = view["bindingToken"].as_str().expect("绑定令牌").to_owned();
    f.seed_action_generation("act_dead", "ses-1", "unconfirmed", "svc-old", 1111, 999);
    let before = route_version_snapshot(&f);
    let action_before = f.action_row("act_dead");
    let claims_before = f.claim_rows("act_dead");

    let (status, text) = post(
        &f.app,
        "/m/api/v1/opencode/route/legacy",
        COOKIE,
        f.route_request("ses-1", &token, false),
    )
    .await;
    assert_eq!(status, 409, "旧路由已退役：{text}");
    assert!(text.contains("OC_LEGACY_DISABLED"), "{text}");
    assert_eq!(route_version_snapshot(&f), before);
    assert_eq!(f.action_row("act_dead"), action_before);
    assert_eq!(f.claim_rows("act_dead"), claims_before);

    let (status, text) = post(
        &f.app,
        "/m/api/v1/opencode/actions/act_dead/confirm-unknown",
        COOKIE,
        String::new(),
    )
    .await;
    assert_eq!(status, 200, "{text}");

    assert_eq!(f.action_row("act_dead").0, "unknown_confirmed");
    let before = route_version_snapshot(&f);
    let action_before = f.action_row("act_dead");
    let claims_before = f.claim_rows("act_dead");
    let (status, text) = post(
        &f.app,
        "/m/api/v1/opencode/route/legacy",
        COOKIE,
        f.route_request("ses-1", &token, false),
    )
    .await;
    assert_eq!(status, 409, "确认后旧路由仍然退役：{text}");
    assert!(text.contains("OC_LEGACY_DISABLED"), "{text}");
    assert_eq!(route_version_snapshot(&f), before);
    assert_eq!(f.action_row("act_dead"), action_before);
    assert_eq!(f.claim_rows("act_dead"), claims_before);
    assert!(f.injector.texts().is_empty() && f.injector.keys().is_empty());
    assert_eq!(f.peer.writes(), 0);
}

/// patch3 O1（H13 证据 §10-O1）：重入拒绝文案按**触发族**分流。族一 = 换代期未决
/// 原生动作（H13 实测真因）→「结果未确认的原生动作」文案，不得再错报「旧队列消息」。
#[tokio::test]
async fn reenter_rejection_copy_matches_the_unresolved_native_action_family() {
    let (f, _b, token_a) = visible_other_directory_fixture().await;
    f.seed_action_generation("act_dead", "ses-1", "acknowledged", "svc-old", 1111, 999);
    assert_eq!(f.pending_rows("ses-1"), 0, "前置：旧 inject_queue 为空");

    let (status, body) = post(
        &f.app,
        "/m/api/v1/opencode/route/native",
        COOKIE,
        f.native_route_request("ses-1", &token_a, false).await,
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert!(
        body.contains("结果未确认的原生动作"),
        "拒绝文案必须指认未决原生动作族：{body}"
    );
    assert!(
        !body.contains("旧队列消息"),
        "不得把未决动作族错报成旧队列消息：{body}"
    );
}

/// patch3 O1 族二：旧 `inject_queue` 未决行 → 「旧队列消息」文案（保留）。
#[tokio::test]
async fn reenter_rejection_copy_keeps_the_legacy_queue_family() {
    let f = Fixture::single(Switch::On).await;
    f.seed_pending_row("ses-1", "旧待发消息");
    let candidate = f.preview_token("ses-1", COOKIE).await;
    // 无路由行：首次显式进入只带候选凭据
    let (status, body) = post(
        &f.app,
        "/m/api/v1/opencode/route/native",
        COOKIE,
        serde_json::json!({"sessionId":"ses-1", "candidateToken":candidate}).to_string(),
    )
    .await;
    assert_eq!(status, 409, "{body}");
    assert!(
        body.contains("旧队列消息"),
        "旧队列未决族必须保留原文案：{body}"
    );
    assert!(
        !body.contains("结果未确认的原生动作"),
        "不得把旧队列族错报成未决动作：{body}"
    );
}

// 跨请求契约（待读端实现）：首次显式选择使用不带头的 view；之后四个读入口
// 以 X-OpenCode-Expected-Binding 携带该页面拿到的不透明 bindingToken；浏览器
// EventSource 无法设置自定义头，events 使用 expected_binding_token 查询参数。
// 绑定失效返回 409 / OC_BINDING_STALE，不返回新目标的内容或 token。
async fn assert_old_page_binding_rejected_after_explicit_switch(path: &str) {
    let (f, b, token_a) = visible_other_directory_fixture().await;
    seed_historical_draining(&f);
    let (status, body) = post(
        &f.app,
        "/m/api/v1/opencode/route/native",
        COOKIE,
        f.native_route_request("ses-1", &token_a, false).await,
    )
    .await;
    assert_eq!(status, 200, "另一请求显式选择 B：{body}");
    let (status, selected_b) = f.view("ses-1").await;
    assert_eq!(status, 200, "另一请求显式选择 B：{selected_b}");
    assert_eq!(selected_b["route"], "opencode_native");
    assert_eq!(selected_b["serviceId"], "svc-b");
    let token_b = selected_b["bindingToken"].as_str().expect("B 绑定令牌");
    assert_ne!(token_a, token_b, "两个目标的绑定必须不同");
    assert_eq!(active_target(&f).0, "svc-b");

    // 旧页面继续携带它最初的 A，不把另一页面的选择当作自己的选择。
    let uri = if path.contains("/events?") {
        format!("{path}&expected_binding_token={token_a}")
    } else {
        path.to_string()
    };
    let request = req("GET", &uri, Some(COOKIE), None);
    let (mut parts, body) = request.into_parts();
    if !path.contains("/events?") {
        parts
            .headers
            .insert("x-opencode-expected-binding", token_a.parse().unwrap());
    }
    let response = f
        .app
        .clone()
        .oneshot(axum::http::Request::from_parts(parts, body))
        .await
        .unwrap();
    let status = response.status();
    // events 若错误地返回 200 是无限流，因此只读取非成功响应。
    assert_eq!(
        status, 409,
        "旧页面读取 {path} 必须报告绑定失效，不能读取 B"
    );
    let text = body_of(response).await;
    assert!(text.contains("OC_BINDING_STALE"), "应报告绑定失效：{text}");
    for secret in [
        "visible-b-secret",
        "form-b-secret",
        "agent-b-secret",
        token_b,
    ] {
        assert!(
            !text.contains(secret),
            "旧页面不得收到 B 内容或令牌：{text}"
        );
    }
    assert_eq!(active_target(&f).0, "svc-b", "旧请求不得改变新路由");
    assert_eq!(b.writes(), 0);
}

#[tokio::test]
async fn cross_request_pin_rejects_stale_view() {
    assert_old_page_binding_rejected_after_explicit_switch(
        "/m/api/v1/opencode/view?session_id=ses-1",
    )
    .await;
}

#[tokio::test]
async fn cross_request_pin_rejects_stale_form() {
    assert_old_page_binding_rejected_after_explicit_switch(
        "/m/api/v1/opencode/forms/frm-1?session_id=ses-1",
    )
    .await;
}

#[tokio::test]
async fn cross_request_pin_rejects_stale_agents() {
    assert_old_page_binding_rejected_after_explicit_switch(
        "/m/api/v1/opencode/agents?session_id=ses-1",
    )
    .await;
}

#[tokio::test]
async fn cross_request_pin_rejects_stale_events() {
    assert_old_page_binding_rejected_after_explicit_switch(
        "/m/api/v1/opencode/events?session_id=ses-1",
    )
    .await;
}

async fn switch_to_visible_b(f: &Fixture, token_a: &str) {
    seed_historical_draining(f);
    let (status, body) = post(
        &f.app,
        "/m/api/v1/opencode/route/native",
        COOKIE,
        f.native_route_request("ses-1", &token_a, false).await,
    )
    .await;
    assert_eq!(status, 200, "选择 B：{body}");
    assert_eq!(active_target(f).0, "svc-b");
}

fn pause_command(action_id: &str, token: &str, revision: i64) -> String {
    serde_json::json!({
        "actionId": action_id, "sessionId": "ses-1", "bindingToken": token,
        "operation": {"kind":"set_outbox_paused", "paused":true, "expectedRevision":revision}
    })
    .to_string()
}

#[tokio::test]
async fn stale_binding_new_pause_does_not_change_same_revision_active_target() {
    let (f, b, token_a) = visible_other_directory_fixture().await;
    let (status, a_snapshot) = get_bound(
        &f.app,
        "/m/api/v1/opencode/outbox?session_id=ses-1",
        &token_a,
    )
    .await;
    assert_eq!(status, 200, "读取 A 队列：{a_snapshot}");
    assert_eq!(a_snapshot["revision"], 0);
    assert_eq!(a_snapshot["paused"], false);
    switch_to_visible_b(&f, &token_a).await;
    let (status, view_b) = f.view("ses-1").await;
    assert_eq!(status, 200, "读取 B 视图：{view_b}");
    assert_eq!(view_b["serviceId"], "svc-b");
    let token_b = view_b["bindingToken"].as_str().expect("B 绑定令牌");
    assert_ne!(token_a, token_b);
    let before = active_target(&f);
    let (status, snapshot) = get_bound(
        &f.app,
        "/m/api/v1/opencode/outbox?session_id=ses-1",
        token_b,
    )
    .await;
    assert_eq!(status, 200, "读取 B 队列：{snapshot}");
    assert_eq!(snapshot["revision"], 0);
    assert_eq!(snapshot["paused"], false);
    let (status, body) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        pause_command("stale_new_pause", &token_a, 0),
    )
    .await;
    let (read_status, after) = get_bound(
        &f.app,
        "/m/api/v1/opencode/outbox?session_id=ses-1",
        token_b,
    )
    .await;
    assert_eq!(read_status, 200, "再次读取 B 队列：{after}");
    assert_eq!(
        after["revision"], snapshot["revision"],
        "旧 A 不得改变 B revision：{body}"
    );
    assert_eq!(after["paused"], false, "旧 A 不得暂停 B：{body}");
    assert_eq!(active_target(&f), before);
    assert_eq!(f.action_rows("stale_new_pause"), 0);
    assert_eq!(b.writes(), 0);
    assert_eq!(status, 409, "旧绑定必须被拒绝：{body}");
    assert!(body.contains("OC_BINDING_STALE"), "应返回绑定失效：{body}");
}

#[tokio::test]
async fn duplicate_pause_returns_original_result_after_active_binding_changes() {
    let (f, _b, token_a) = visible_other_directory_fixture().await;
    let command = pause_command("prior_pause_result", &token_a, 0);
    let (status, first) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        command.clone(),
    )
    .await;
    assert_eq!(status, 200, "先记录 A 动作：{first}");
    switch_to_visible_b(&f, &token_a).await;
    let before = active_target(&f);
    let (status, again) = post(&f.app, "/m/api/v1/opencode/actions", COOKIE, command).await;
    assert_eq!(status, 200, "重复 actionId 应优先回读：{again}");
    let first: serde_json::Value = serde_json::from_str(&first).unwrap();
    let again: serde_json::Value = serde_json::from_str(&again).unwrap();
    assert_eq!(again, first);
    assert_eq!(f.action_rows("prior_pause_result"), 1);
    assert_eq!(active_target(&f), before);
}

async fn historical_a_outbox_with_b_card() -> (Fixture, Peer) {
    let (f, b, _token_a) = visible_other_directory_fixture().await;
    seed_historical_draining(&f);
    f.seed_action("historical_a", "ses-1", "acknowledged");
    f.state.store.with(|c| {
        c.execute("INSERT INTO opencode_outbox_targets(session_id,directory,revision,paused) VALUES ('ses-1',?1,1,0)", [f.dir.to_string_lossy().as_ref()]).unwrap();
        c.execute("INSERT INTO opencode_outbox(action_id,session_id,command_json,text_summary,attachment_count) VALUES ('historical_a','ses-1',NULL,'a-private-summary',0)", []).unwrap();
    });
    (f, b)
}

#[tokio::test]
async fn unpinned_outbox_does_not_guess_historical_a_for_visible_b_card() {
    let (f, _b) = historical_a_outbox_with_b_card().await;
    let (status, body) = f.get("/m/api/v1/opencode/outbox?session_id=ses-1").await;
    assert_eq!(status, 409, "缺少绑定令牌必须拒绝：{body}");
    assert!(
        !body.to_string().contains("a-private-summary"),
        "B 卡不得展示 A 历史摘要：{status} {body}"
    );
}

#[tokio::test]
async fn unpinned_outbox_does_not_guess_historical_a_when_b_view_fails() {
    let (f, b) = historical_a_outbox_with_b_card().await;
    b.shared.offline.store(true, Ordering::SeqCst);
    let (view_status, view) = f.view("ses-1").await;
    assert_ne!(view_status, 200, "先确认 B 视图失败：{view}");
    let (status, body) = f.get("/m/api/v1/opencode/outbox?session_id=ses-1").await;
    assert!(
        !body.to_string().contains("a-private-summary"),
        "B 视图失败不能回退到 A 历史摘要：{status} {body}"
    );
}

// 这些用例仅使用本文件的共享内存数据库及回环 peer。
fn route_version_snapshot(f: &Fixture) -> (String, i64, i64, String, String, i64) {
    f.state.store.with(|c| c.query_row(
        "SELECT service_id,runtime_pid,runtime_started_at_ms,directory,route_state,route_version FROM opencode_native_routes WHERE session_id='ses-1'",
        [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
    ).unwrap())
}

#[tokio::test]
async fn route_version_active_view_refresh_preserves_version() {
    let f = Fixture::single(Switch::On).await;
    let (status, first) = f.view("ses-1").await;
    assert_eq!(status, 200, "首次视图：{first}");
    let version = route_version_snapshot(&f).5;
    let (status, second) = f.view("ses-1").await;
    assert_eq!(status, 200, "普通刷新：{second}");
    assert_eq!(
        route_version_snapshot(&f).5,
        version,
        "同一 active 视图刷新不得推进版本"
    );
    assert_eq!(
        first["routeVersion"].as_i64(),
        Some(version),
        "视图必须提供已认领路由的版本：{first}"
    );
    assert_eq!(
        second["routeVersion"].as_i64(),
        Some(version),
        "刷新必须保留版本：{second}"
    );
}

async fn assert_old_route_version_rejected_after_aba(path: &str, force: bool) {
    let f = Fixture::single(Switch::On).await;
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "先授权 A：{view}");
    let token_a = view["bindingToken"].as_str().unwrap().to_string();
    let old_version = view["routeVersion"].as_i64().expect("视图路由版本");
    let other_dir = f._temp.path().join("other-project-visible");
    std::fs::create_dir_all(&other_dir).unwrap();
    let other_dir = other_dir.canonicalize().unwrap();
    let b = Peer::start(&other_dir).await;
    b.set_info_pid(9999);
    let mut card = opencode_session("ses-1", &other_dir);
    card.pid = 9999;
    *f.scan_override.lock().unwrap() = Some(vec![card]);
    std::fs::write(f._temp.path().join("service.json"), format!(
        r#"{{"id":"svc-b","version":"{PEER_VERSION}","url":"{}","pid":9999,"password":"pw-fixture"}}"#, b.endpoint
    )).unwrap();
    seed_historical_draining(&f);
    let returned_a_version = route_version_snapshot(&f).5;
    let candidate = f.preview_token("ses-1", COOKIE_OTHER).await;
    let (status, body) = post(&f.app, "/m/api/v1/opencode/route/native", COOKIE_OTHER,
        serde_json::json!({"sessionId":"ses-1", "bindingToken":token_a, "routeVersion":returned_a_version, "candidateToken":candidate}).to_string()).await;
    assert_eq!(status, 200, "另一设备选择 B：{body}");
    assert_eq!(active_target(&f).0, "svc-b");
    let selected_b: serde_json::Value = serde_json::from_str(&body).unwrap();
    let token_b = selected_b["bindingToken"].as_str().unwrap().to_string();
    seed_historical_draining(&f);
    let returned_b_version = route_version_snapshot(&f).5;
    *f.scan_override.lock().unwrap() = Some(vec![opencode_session("ses-1", &f.dir)]);
    std::fs::write(f._temp.path().join("service.json"), format!(
        r#"{{"id":"{PEER_SERVICE_ID}","version":"{PEER_VERSION}","url":"{}","pid":{PEER_PID},"password":"pw-fixture"}}"#, f.peer.endpoint
    )).unwrap();
    let candidate = f.preview_token("ses-1", COOKIE_OTHER).await;
    let (status, body) = post(&f.app, "/m/api/v1/opencode/route/native", COOKIE_OTHER,
        serde_json::json!({"sessionId":"ses-1", "bindingToken":token_b, "routeVersion":returned_b_version, "candidateToken":candidate}).to_string()).await;
    assert_eq!(status, 200, "另一设备切回 A：{body}");
    assert_eq!(active_target(&f).0, PEER_SERVICE_ID);
    assert!(
        route_version_snapshot(&f).5 > old_version,
        "ABA 必须推进路由版本"
    );
    if force {
        std::fs::remove_file(f._temp.path().join("service.json")).unwrap();
        f.seed_action("aba-preserved", "ses-1", "acknowledged");
    } else {
        f.seed_action_generation(
            "aba-preserved",
            "ses-1",
            "acknowledged",
            "svc-old",
            1111,
            999,
        );
    }
    f.seed_claim("aba-preserved", "ses-1", "form:aba-preserved");
    let before = route_version_snapshot(&f);
    let action_before = f.action_row("aba-preserved");
    let claims_before = f.claim_rows("aba-preserved");
    let writes_before = (f.peer.writes(), b.writes());
    let mut request = serde_json::json!({"sessionId":"ses-1", "bindingToken":token_a, "routeVersion":old_version});
    if path.ends_with("/native") {
        request["candidateToken"] = f.preview_token("ses-1", COOKIE).await;
    }
    if path.ends_with("/legacy") {
        request["force"] = serde_json::json!(force);
        if force {
            // 模拟旧客户端持有的历史确认，不从已退役入口申请候选。
            request["confirmedActionIds"] = serde_json::json!(["aba-preserved"]);
        }
    }
    let (status, body) = post(&f.app, path, COOKIE, request.to_string()).await;
    assert_eq!(
        route_version_snapshot(&f),
        before,
        "旧版本请求不得改变路由：{body}"
    );
    assert_eq!(
        f.action_row("aba-preserved"),
        action_before,
        "旧版本请求不得改变动作：{body}"
    );
    assert_eq!(
        f.claim_rows("aba-preserved"),
        claims_before,
        "旧版本请求不得释放占用：{body}"
    );
    assert_eq!((f.peer.writes(), b.writes()), writes_before);
    assert!(f.injector.texts().is_empty() && f.injector.keys().is_empty());
    assert_eq!(status, 409, "旧路由版本应作为冲突拒绝：{body}");
    if path.ends_with("/native") {
        assert!(
            body.contains("OC_BINDING_STALE"),
            "原生 ABA 必须由版本门拒绝：{body}"
        );
    }
}

#[tokio::test]
async fn route_version_aba_rejects_old_native_request() {
    assert_old_route_version_rejected_after_aba("/m/api/v1/opencode/route/native", false).await;
}

#[tokio::test]
async fn route_version_aba_rejects_old_legacy_request() {
    assert_old_route_version_rejected_after_aba("/m/api/v1/opencode/route/legacy", false).await;
}

#[tokio::test]
async fn route_version_aba_rejects_old_force_request() {
    assert_old_route_version_rejected_after_aba("/m/api/v1/opencode/route/legacy", true).await;
}

async fn assert_retired_force_preserves_historical_candidates(replace: bool) {
    let f = Fixture::single(Switch::On).await;
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "建立 active 视图：{view}");
    let token = view["bindingToken"].as_str().unwrap();
    // 旧客户端持有的确认针对登记失效时已无法核对的动作。
    std::fs::remove_file(f._temp.path().join("service.json")).unwrap();
    f.seed_action("act_dead", "ses-1", "acknowledged");
    f.seed_claim("act_dead", "ses-1", "form:ses-1:frm_dead");
    // 模拟已获得确认的旧客户端；请求送达时，历史候选集已经变化。
    let request = serde_json::json!({
        "sessionId":"ses-1", "bindingToken":token, "routeVersion":view["routeVersion"],
        "force":true, "confirmedActionIds":["act_dead"]
    })
    .to_string();
    if replace {
        f.state.store.with(|c| {
            c.execute(
                "UPDATE opencode_native_actions SET state='confirmed' WHERE action_id='act_dead'",
                [],
            )
            .unwrap();
            c.execute(
                "DELETE FROM opencode_native_claims WHERE action_id='act_dead'",
                [],
            )
            .unwrap();
        });
    }
    f.seed_action("new-after-confirmation", "ses-1", "acknowledged");
    f.seed_claim(
        "new-after-confirmation",
        "ses-1",
        "form:new-after-confirmation",
    );
    let before = route_version_snapshot(&f);
    let original_action = f.action_row("act_dead");
    let original_claims = f.claim_rows("act_dead");
    let new_action = f.action_row("new-after-confirmation");
    let claims = f.claim_rows("new-after-confirmation");
    let writes = f.peer.writes();
    let (status, body) = post(&f.app, "/m/api/v1/opencode/route/legacy", COOKIE, request).await;
    assert_eq!(status, 409, "退役 force 请求必须被拒绝：{body}");
    assert!(body.contains("OC_FORCE_CONFIRMATION_REQUIRED"), "{body}");
    assert_eq!(route_version_snapshot(&f), before);
    assert_eq!(f.action_row("act_dead"), original_action);
    assert_eq!(f.claim_rows("act_dead"), original_claims);
    assert_eq!(f.action_row("new-after-confirmation"), new_action);
    assert_eq!(f.claim_rows("new-after-confirmation"), claims);
    assert_eq!(f.peer.writes(), writes);
    assert!(f.injector.texts().is_empty() && f.injector.keys().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retired_force_preserves_added_candidate() {
    assert_retired_force_preserves_historical_candidates(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retired_force_preserves_same_count_replacement() {
    assert_retired_force_preserves_historical_candidates(true).await;
}

// ============================================================

// ============================================================
// H03 echo-challenge 归属写门（binding_gate · 复审 fix round）：
// 生产判定分支（production_gate 链 + 装门，只注入环境事实）下的成对证据核验。
// 断言口径：用户会话写（prompt_writes）与探针写（probe_writes）分离取证；
// 拒绝路径 = 零用户会话写 + 零新动作行 + 零新队列项；认领/写同链消费。
// ============================================================

/// 短窗参数：拒绝路径在数百毫秒内收敛（真实默认窗见 binding_gate::DEFAULT_WINDOW_MS）。
fn gate_arm() -> GateArm {
    GateArm {
        locked_pid: GATE_LOCKED_PID,
        started_at_ms: GATE_STARTED_AT_MS,
        window_ms: 600,
        poll_interval_ms: 10,
    }
}

/// N2 多样本用：按会话参数化的发送体（gate_send_body 的 ses-1 专写版不适用于
/// 「同服务双会话」的样本对照）。
fn gate_send_body_for(action: &str, sid: &str, token: &str) -> String {
    serde_json::json!({
        "actionId": action,
        "sessionId": sid,
        "bindingToken": token,
        "operation": {
            "kind": "send",
            "text": "归属写门放行消息",
            "attachmentPaths": [],
            "delivery": "queue",
            "clientMessageId": format!("msg_{action}"),
        }
    })
    .to_string()
}

fn gate_send_body(action: &str, token: &str, attachments: &[String]) -> String {
    serde_json::json!({
        "actionId": action,
        "sessionId": "ses-1",
        "bindingToken": token,
        "operation": {
            "kind": "send",
            "text": "归属写门放行消息",
            "attachmentPaths": attachments,
            "delivery": "queue",
            "clientMessageId": format!("msg_{action}"),
        }
    })
    .to_string()
}

fn gate_outbox_rows(f: &Fixture) -> usize {
    f.state
        .store
        .with(|conn| {
            conn.query_row("SELECT COUNT(*) FROM opencode_outbox", [], |r| {
                r.get::<_, i64>(0)
            })
        })
        .unwrap_or(-1) as usize
}

fn action_row_count(f: &Fixture, action_id: &str) -> usize {
    f.action_rows(action_id)
}

/// 认领（打开视图）走生产写权链：门通过 = 路由认领成立。
/// 正例全链：认领（1 创建 + 1 挑战）→ 发送（预留前 + 派发权前各 1 挑战）→ Confirmed。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_pass_round_lets_paired_send_through() {
    let f = Fixture::new_with_gate(Switch::On, gate_arm()).await;
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "认领（生产链 echo 证据）必须成功：{view}");
    assert_eq!(
        view["route"], "opencode_native",
        "门通过即归属证明成立：{view}"
    );
    let token = view["bindingToken"].as_str().expect("绑定令牌").to_string();
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_gate_pass", &token, &[]),
    )
    .await;
    assert_eq!(
        status, 200,
        "三件套齐（窗口内 + 同 nonce + 锁定 pid）必须放行：{reply}"
    );
    let view = serde_json::from_str::<serde_json::Value>(&reply).unwrap();
    assert_eq!(view["state"], "confirmed", "写后回读定性：{view}");
    assert_eq!(f.peer.prompt_writes(), 1, "用户会话写恰一次");
    // 探针通道：发送预留前/派发权前各 1 挑战 + 1 创建（认领零远端写副作用——N2）。
    assert_eq!(f.peer.probe_writes(), 3, "探针写 = 1 创建 + 2 挑战");
    // N1：认领后的第二次 GET 不得把健康路由重新宣告不可写——route 保持
    // opencode_native、无原因词、bindingRequired 不发布，且**零探针写**（读路径
    // 零远端副作用）；随后发送仍逐次过门（写权证明不变）。
    let probe_before_second_view = f.peer.probe_writes();
    let (status, view2) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view2}");
    assert_eq!(
        view2["route"], "opencode_native",
        "active 路由展示态如实：{view2}"
    );
    assert!(
        view2["routeReasonCode"].is_null(),
        "健康 active 路由不得再发布归属未证原因词：{view2}"
    );
    assert!(
        view2["bindingRequired"] != serde_json::json!(true),
        "不得发布 bindingRequired=true：{view2}"
    );
    assert_eq!(
        f.peer.probe_writes(),
        probe_before_second_view,
        "第二次 GET 零远端写副作用（读路径不过门，N2）"
    );
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_gate_pass_2", &token, &[]),
    )
    .await;
    assert_eq!(status, 200, "第二次 GET 后发送仍逐次过门放行：{reply}");
    assert_eq!(f.peer.prompt_writes(), 2);
    // I5：轮次结束撤销控制文件授权（成功路径——每轮结束无论成败都撤销）。
    let gate_home = f.gate_home.clone().expect("装门夹具必有控制文件根");
    let control = gate_home
        .join(".tuvis/opencode-tui")
        .join(format!("{GATE_LOCKED_PID}.probe"));
    assert!(!control.exists(), "轮次结束必须撤销控制文件授权");
}

/// I5：轮次进行中控制文件绑定轮次（nonce）+ 进程代际（expectPid/expectBoot）+
/// 有效期（deadlineMs）、0600；轮次结束撤销。hold-ack 确定性时序下取证。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_control_file_lifecycle_binds_round_and_revokes() {
    let f = Fixture::new_with_gate(Switch::On, gate_arm()).await;
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    // 认领零远端写（N2）：先完成视图认领，hold 武装后由**发送的预留前轮**挂起。
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    let token = view["bindingToken"].as_str().unwrap().to_string();
    f.peer.hold_gate_ack();
    let app = f.app.clone();
    let body = gate_send_body("act_gate_hold_probe", &token, &[]);
    let task =
        tokio::spawn(async move { post(&app, "/m/api/v1/opencode/actions", COOKIE, body).await });
    // 预留前轮的挑战挂起：取证控制文件。
    f.peer.wait_gate_ack_held().await;
    let gate_home = f.gate_home.clone().expect("装门夹具必有控制文件根");
    let control = gate_home
        .join(".tuvis/opencode-tui")
        .join(format!("{GATE_LOCKED_PID}.probe"));
    let raw = std::fs::read(&control).expect("轮内控制文件必须在场");
    let parsed: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(parsed["sessionID"], "ses-probe-fixture");
    assert_eq!(parsed["expectPid"], GATE_LOCKED_PID, "授权绑定锁定窗口");
    assert_eq!(parsed["expectBoot"], GATE_BOOT, "授权绑定进程代际（boot）");
    let nonce = parsed["nonce"].as_str().expect("授权必须绑定轮次 nonce");
    assert_eq!(nonce.len(), 32, "nonce 形态");
    let deadline = parsed["deadlineMs"].as_i64().expect("授权必须带有效期");
    assert!(
        deadline > chrono::Utc::now().timestamp_millis(),
        "有效期在未来"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&control).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "控制文件必须 0600");
    }
    // 放行 → 挑战被应答（预留前轮通过；派发权前轮无 hold 会即时完成）→ 发送确认。
    f.peer.release_gate_ack();
    let (status, reply) = tokio::time::timeout(std::time::Duration::from_secs(10), task)
        .await
        .expect("发送必须结束")
        .unwrap();
    assert_eq!(status, 200, "{reply}");
    assert!(
        !control.exists(),
        "轮次结束撤销授权：换锁定窗口后旧窗口不再应答"
    );
}

/// 反例：ack 带错误 pid（E4：写错实例会得到错误 TUI 的合法 ack）→ 写时拒绝零写。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_pid_mismatch_rejects_with_zero_user_writes() {
    let f = Fixture::new_with_gate(Switch::On, gate_arm()).await;
    // 认领先以正确 pid 通过（路由激活），再换错误 pid 的 ack → 写时门拒。
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    let token = view["bindingToken"].as_str().unwrap().to_string();
    f.peer.arm_gate_ack(GATE_LOCKED_PID + 7, None);
    let before = gate_outbox_rows(&f);
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_gate_pid", &token, &[]),
    )
    .await;
    assert_eq!(status, 409, "pid 不匹配必须拒绝：{reply}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&reply).unwrap()["reasonCode"],
        "OC_GATE_PID_MISMATCH"
    );
    assert_eq!(f.peer.prompt_writes(), 0, "零用户会话写");
    assert_eq!(gate_outbox_rows(&f), before, "零新队列项");
    assert_eq!(
        action_row_count(&f, "act_gate_pid"),
        0,
        "预留前拒绝零动作行"
    );
}

/// 反例：窗口内零 ack（门未装配 ack 源）→ 写时 OC_GATE_NO_ACK；同 actionId 重放
/// 返回持久回执零新写；新 intent 在 ack 武装后放行（M1 口径：正文发送）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_no_ack_rejection_is_retryable_with_new_intent_only() {
    let f = Fixture::new_with_gate(Switch::On, gate_arm()).await;
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    let token = view["bindingToken"].as_str().unwrap().to_string();
    // 认领已成立（ack 在场）；撤防后写时门零 ack → 拒绝。
    f.peer.disarm_gate_ack();
    let prompt_before = f.peer.prompt_writes();
    let probe_before = f.peer.probe_writes();
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_gate_retry", &token, &[]),
    )
    .await;
    assert_eq!(status, 409, "窗口内零 ack 必须 fail-closed：{reply}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&reply).unwrap()["reasonCode"],
        "OC_GATE_NO_ACK"
    );
    assert_eq!(f.peer.prompt_writes(), prompt_before, "零用户会话写");
    // 拒绝轮本身写过 1 次探针挑战（预读+PATCH；与用户会话写分开取证）。
    let probe_after_refusal = f.peer.probe_writes();
    // 同 actionId 重放：拒绝已持久（rejections 墓碑 → 幂等回执），零新写。
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_gate_retry", &token, &[]),
    )
    .await;
    assert_eq!(status, 200, "同 id 重放返回持久回执：{reply}");
    let receipt = serde_json::from_str::<serde_json::Value>(&reply).unwrap();
    assert_eq!(receipt["state"], "not_sent");
    assert_eq!(receipt["reasonCode"], "OC_GATE_NO_ACK");
    assert_eq!(f.peer.prompt_writes(), prompt_before, "重放零新用户写");
    assert_eq!(f.peer.probe_writes(), probe_after_refusal, "重放零新探针写");
    // 新 intent（新 actionId）+ ack 武装 → 放行。
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_gate_retry_2", &token, &[]),
    )
    .await;
    assert_eq!(status, 200, "新 intent 在归属证明后必须可发出：{reply}");
    assert_eq!(f.peer.prompt_writes(), prompt_before + 1);
    assert!(
        f.peer.probe_writes() > probe_after_refusal,
        "新 intent 又经过预留前/派发权前两轮挑战"
    );
}

/// 反例：探针不可写（PATCH 500）→ OC_GATE_PROBE_UNWRITABLE，零用户写零动作行；
/// 只读路径（视图）不受门影响。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_probe_unwritable_rejects_with_zero_user_writes() {
    let f = Fixture::new_with_gate(Switch::On, gate_arm()).await;
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    let token = view["bindingToken"].as_str().unwrap().to_string();
    f.peer.break_probe_writes();
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_gate_probe", &token, &[]),
    )
    .await;
    assert_eq!(status, 409, "探针不可写必须拒绝：{reply}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&reply).unwrap()["reasonCode"],
        "OC_GATE_PROBE_UNWRITABLE"
    );
    assert_eq!(f.peer.prompt_writes(), 0, "零用户会话写");
    assert_eq!(
        action_row_count(&f, "act_gate_probe"),
        0,
        "预留前拒绝零动作行"
    );
    // 旧动作仍可只读核对（视图可读）。
    let (status, _) = f.view("ses-1").await;
    assert_eq!(status, 200, "只读核对不受门影响");
}

/// I1 确定性时序：ACK 挂起期间锁定 TUI 退出/换代 → ACK 后终验拒绝，
/// 零动作行零用户写（「已 ACK、返回前退出」场景）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_tui_exit_after_ack_rejects_before_claim() {
    let f = Fixture::new_with_gate(Switch::On, gate_arm()).await;
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    // 先认领（ack 正常），写轮再挂起。
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    let token = view["bindingToken"].as_str().unwrap().to_string();
    f.peer.hold_gate_ack();
    let app = f.app.clone();
    let body = gate_send_body("act_gate_exit", &token, &[]);
    let task =
        tokio::spawn(async move { post(&app, "/m/api/v1/opencode/actions", COOKIE, body).await });
    f.peer.wait_gate_ack_held().await;
    // ACK 未发生、挑战在飞：此刻锁定 TUI「退出」（进程事实 → None）。
    let alive = f.gate_tui_alive.clone().expect("装门夹具必有存活开关");
    alive.store(false, Ordering::SeqCst);
    f.peer.release_gate_ack();
    let (status, reply) = tokio::time::timeout(std::time::Duration::from_secs(10), task)
        .await
        .expect("派发必须在时限内结束")
        .unwrap();
    assert_eq!(status, 409, "ACK 后 TUI 退出必须终验拒绝：{reply}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&reply).unwrap()["reasonCode"],
        "OC_GATE_PID_UNVERIFIED"
    );
    assert_eq!(f.peer.prompt_writes(), 0, "零用户会话写");
    assert_eq!(
        action_row_count(&f, "act_gate_exit"),
        0,
        "预留前拒绝零动作行"
    );
}

/// 派发权前复核：认领 + 预留前轮 ack（预算 2）→ reserve 成功 → 派发权前轮零 ack →
/// 墓碑放弃（not_sent + binding_unverified），零 prompt 写、零队列项。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_dispatch_stage_rejection_leaves_tombstone_and_no_queue_item() {
    let f = Fixture::new_with_gate(Switch::On, gate_arm()).await;
    f.peer.arm_gate_ack(GATE_LOCKED_PID, Some(1)); // 预留前轮；派发权前轮无 ack
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    let token = view["bindingToken"].as_str().unwrap().to_string();
    let outbox_before = gate_outbox_rows(&f);
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_gate_dispatch", &token, &[]),
    )
    .await;
    assert_eq!(status, 409, "派发权前轮零 ack 必须拒绝：{reply}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&reply).unwrap()["reasonCode"],
        "OC_GATE_NO_ACK"
    );
    let (state, reason, _) = f.action_row("act_gate_dispatch");
    assert_eq!(state, "not_sent", "派发权前失败 = 墓碑放弃：{state}");
    assert_eq!(reason.as_deref(), Some("binding_unverified"));
    assert_eq!(f.peer.prompt_writes(), 0, "零用户会话写");
    assert_eq!(gate_outbox_rows(&f), outbox_before, "零新队列项");
}

/// M1：探针在缓存已建立后被删除 → 下一写轮受控重建（创建恰一次），发送仍 Confirmed。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_recreates_deleted_probe_exactly_once() {
    let f = Fixture::new_with_gate(Switch::On, gate_arm()).await;
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    let token = view["bindingToken"].as_str().unwrap().to_string();
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_gate_recreate_a", &token, &[]),
    )
    .await;
    assert_eq!(status, 200, "首轮放行：{reply}");
    let probe_writes_first = f.peer.probe_writes();
    // 探针被删（404）→ 次轮受控重建一次。
    f.peer.delete_probe_session();
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_gate_recreate_b", &token, &[]),
    )
    .await;
    assert_eq!(status, 200, "重建后放行：{reply}");
    // 次轮 = 重建（1 创建）+ 预留前/派发权前 2 挑战 = 3 探针写。
    assert_eq!(
        f.peer.probe_writes() - probe_writes_first,
        3,
        "重建恰一次（不是每步都重建）"
    );
}

/// M1：带附件发送过门——正例 Confirmed 且回执携带附件；零 ack 反例零写零队列项。
/// 口径（fix3 申报修正）：附件**上传**入口（`POST /session-attachment`，native
/// 分支）走**完整 echo 写权链**（固定目标核对处消费）——过门 = 提前失败（归属未证
/// 时上传即拒、零本地落盘）；**提交**（prompt 携带附件 URI）时再次过门 = 绑定证明
/// 点。本用例覆盖提交段的正反例，上传段正反例见 gate_upload_path_*。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_attachment_send_passes_and_rejects_fail_closed() {
    let f = Fixture::new_with_gate(Switch::On, gate_arm()).await;
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    let controlled = f.dir.join(".tuvis-attachments").join("ses-1");
    std::fs::create_dir_all(&controlled).expect("受控附件目录");
    let note = controlled.join("gate-note.txt");
    std::fs::write(&note, "hello gate").expect("写附件");
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    let token = view["bindingToken"].as_str().unwrap().to_string();
    // 反例先行：撤防 ack → 带附件发送零写零队列项。
    f.peer.disarm_gate_ack();
    let before = gate_outbox_rows(&f);
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body(
            "act_gate_attach_rej",
            &token,
            &[note.to_string_lossy().into_owned()],
        ),
    )
    .await;
    assert_eq!(status, 409, "零 ack 带附件必须拒绝：{reply}");
    assert_eq!(f.peer.prompt_writes(), 0, "零用户会话写");
    assert_eq!(gate_outbox_rows(&f), before, "零新队列项");
    // 正例：ack 武装 → 带附件发送 Confirmed，回执携带附件。
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body(
            "act_gate_attach_ok",
            &token,
            &[note.to_string_lossy().into_owned()],
        ),
    )
    .await;
    assert_eq!(status, 200, "带附件过门放行：{reply}");
    let view = serde_json::from_str::<serde_json::Value>(&reply).unwrap();
    assert_eq!(view["state"], "confirmed");
    assert_eq!(view["nativeId"], "msg_act_gate_attach_ok");
}

/// M1：旧动作只读核对不受门影响——已确认动作在门撤防后仍可 verify 收敛/读回。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_keeps_readonly_verify_available() {
    let f = Fixture::new_with_gate(Switch::On, gate_arm()).await;
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    let token = view["bindingToken"].as_str().unwrap().to_string();
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_gate_verify_ok", &token, &[]),
    )
    .await;
    assert_eq!(status, 200, "先有一笔已确认动作：{reply}");
    // 制造一笔**未确认**动作（prompt 被服务端拒绝：写已发生但结果未知），
    // 再验证装门状态下 verify 仍走真实原生回读路径。
    f.peer.refuse_prompts();
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_gate_verify_unc", &token, &[]),
    )
    .await;
    assert_eq!(
        status, 200,
        "prompt 被拒 = Unconfirmed 回执（不是错误）：{reply}"
    );
    let view = serde_json::from_str::<serde_json::Value>(&reply).unwrap();
    assert_eq!(view["state"], "unconfirmed", "写已发生、结果未知：{view}");
    // 门撤防（新写仍会被拒），旧未确认动作的只读核对与动作读不受影响。
    f.peer.disarm_gate_ack();
    let (status, text) = post(
        &f.app,
        "/m/api/v1/opencode/actions/act_gate_verify_unc/verify",
        COOKIE,
        "{}".to_string(),
    )
    .await;
    assert_eq!(status, 200, "未确认动作的只读核对不受门影响：{text}");
    let view = serde_json::from_str::<serde_json::Value>(&text).unwrap();
    assert_eq!(view["actionId"], "act_gate_verify_unc");
    let (status, _) = f
        .get("/m/api/v1/opencode/actions/act_gate_verify_unc")
        .await;
    assert_eq!(status, 200, "动作读不受门影响");
}

/// I4：探针目标隔离——把探针会话当动作目标（发送 / 带附件 / 切模式）一律
/// 结构化拒绝（OC_GATE_PROBE_TARGET），零动作行、零队列项、零非标题写。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_probe_target_writes_are_refused() {
    let f = Fixture::new_with_gate_sessions(Switch::On, gate_arm(), |dir| {
        vec![
            opencode_session("ses-1", dir),
            opencode_session("ses-probe-fixture", dir),
        ]
    })
    .await;
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    // 先在 ses-1 上完整过一轮门（探针注册表落条目）。
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    let token = view["bindingToken"].as_str().unwrap().to_string();
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_gate_probe_seed", &token, &[]),
    )
    .await;
    assert_eq!(status, 200, "种子动作放行：{reply}");
    let writes_after_seed = (f.peer.prompt_writes(), f.peer.probe_writes());
    // 打开探针会话视图并尝试认领 → 探针目标在写权链前置被拒（软失败，只读）。
    let (status, probe_view) = f.view("ses-probe-fixture").await;
    assert_eq!(status, 200, "探针视图必须软失败 200：{probe_view}");
    assert_eq!(
        probe_view["route"], "legacy",
        "探针目标不得认领：{probe_view}"
    );
    let probe_token = probe_view["bindingToken"].as_str().unwrap().to_string();
    let before_outbox = gate_outbox_rows(&f);
    // 1) 普通发送
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        serde_json::json!({
            "actionId": "act_probe_send", "sessionId": "ses-probe-fixture", "bindingToken": probe_token,
            "operation": { "kind": "send", "text": "x", "attachmentPaths": [], "delivery": "queue", "clientMessageId": "msg_act_probe_send" }
        }).to_string(),
    )
    .await;
    assert_eq!(status, 409, "探针目标发送必须拒绝：{reply}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&reply).unwrap()["reasonCode"],
        "OC_GATE_PROBE_TARGET"
    );
    // 2) 带附件发送（I4 + M1 附件口径）
    let controlled = f.dir.join(".tuvis-attachments").join("ses-probe-fixture");
    std::fs::create_dir_all(&controlled).expect("受控附件目录");
    let note = controlled.join("probe-note.txt");
    std::fs::write(&note, "no").unwrap();
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        serde_json::json!({
            "actionId": "act_probe_attach", "sessionId": "ses-probe-fixture", "bindingToken": probe_token,
            "operation": { "kind": "send", "text": "x", "attachmentPaths": [note.to_string_lossy().into_owned()], "delivery": "queue", "clientMessageId": "msg_act_probe_attach" }
        }).to_string(),
    )
    .await;
    assert_eq!(status, 409, "探针目标带附件必须拒绝：{reply}");
    // 3) 切模式
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        serde_json::json!({
            "actionId": "act_probe_agent", "sessionId": "ses-probe-fixture", "bindingToken": probe_token,
            "operation": { "kind": "set_agent", "agent": "agent-a" }
        }).to_string(),
    )
    .await;
    assert_eq!(status, 409, "探针目标切模式必须拒绝：{reply}");
    assert_eq!(f.peer.prompt_writes(), writes_after_seed.0, "零用户会话写");
    assert_eq!(
        f.peer.probe_writes(),
        writes_after_seed.1,
        "零非标题写（零探针写）"
    );
    assert_eq!(gate_outbox_rows(&f), before_outbox, "零新队列项");
    assert_eq!(action_row_count(&f, "act_probe_send"), 0, "零动作行");
    assert_eq!(action_row_count(&f, "act_probe_attach"), 0);
    assert_eq!(action_row_count(&f, "act_probe_agent"), 0);
}

/// I1 残余：终验等待期间（/api/info 挂起）锁定 TUI 退出 → 最后一次网络等待之后的
/// 代际终验拒绝，零动作行零用户写。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_tui_exit_during_final_info_wait_rejects() {
    let f = Fixture::new_with_gate(Switch::On, gate_arm()).await;
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    let token = view["bindingToken"].as_str().unwrap().to_string();
    // 武装一次性 /api/info 挂起（拦的正是 ACK 后的终验读）。
    f.peer.shared.hold_info_read.store(true, Ordering::SeqCst);
    let app = f.app.clone();
    let body = gate_send_body("act_gate_info_exit", &token, &[]);
    let task =
        tokio::spawn(async move { post(&app, "/m/api/v1/opencode/actions", COOKIE, body).await });
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        f.peer.shared.info_blocked.notified(),
    )
    .await
    .expect("ACK 后终验读必须停住");
    // 终验读挂起期间：锁定 TUI「退出」。
    let alive = f.gate_tui_alive.clone().expect("装门夹具必有存活开关");
    alive.store(false, Ordering::SeqCst);
    f.peer.shared.info_release.notify_one();
    let (status, reply) = tokio::time::timeout(std::time::Duration::from_secs(10), task)
        .await
        .expect("派发必须结束")
        .unwrap();
    assert_eq!(status, 409, "终验等待期间退出必须拒绝：{reply}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&reply).unwrap()["reasonCode"],
        "OC_GATE_PID_UNVERIFIED"
    );
    assert_eq!(f.peer.prompt_writes(), 0, "零用户会话写");
    assert_eq!(action_row_count(&f, "act_gate_info_exit"), 0, "零动作行");
}

/// 探针会话管理：同一代际多轮门共用一个探针会话（创建一次复用）。/// fix3-3：**上传路径**（POST /session-attachment，native 分支）走完整 echo 门——
/// 生产装门夹具正反例。口径（申报修正后）：上传口过门 = 提前失败（归属未证时
/// 上传即拒、零本地落盘）；提交时再次过门 = 绑定证明点。探针写口径 = 1 创建 +
/// 每写轮 1 挑战（上传 = 一次写轮）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_upload_path_passes_and_rejects_fail_closed() {
    let f = Fixture::new_with_gate(Switch::On, gate_arm()).await;
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    // 认领（零远端写，N2）→ 上传经 restore_active_target 拿到固定目标。
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    let token = view["bindingToken"].as_str().unwrap().to_string();
    let url = "/m/api/v1/session-attachment?session_id=ses-1&name=gate-upload.txt&interaction_route=opencode_native";
    let token_ref = &token;
    let upload = |app: axum::Router, body: String| {
        let token_ref = token_ref;
        async move {
            use tower::ServiceExt as _;
            let request = req("POST", url, Some(COOKIE), Some(body));
            let (parts, body) = request.into_parts();
            let mut request = axum::http::Request::from_parts(parts, body);
            request
                .headers_mut()
                .insert("x-opencode-expected-binding", token_ref.parse().unwrap());
            let resp = app.oneshot(request).await.expect("上传请求");
            let status = resp.status();
            let text = body_of(resp).await;
            (
                status,
                serde_json::from_str::<serde_json::Value>(&text).unwrap_or(serde_json::json!({})),
            )
        }
    };
    let probe_before = f.peer.probe_writes();
    // 反例先行：零 ack → 上传被门阻断，错误码可辨、零本地落盘。
    f.peer.disarm_gate_ack();
    let (status, reply) = upload(f.app.clone(), "先拒后放".to_string()).await;
    assert_eq!(status, 409, "零 ack 上传必须被门阻断：{reply}");
    assert_eq!(
        reply["error"], "OC_GATE_NO_ACK",
        "上传口错误码可辨（与动作口同族）：{reply}"
    );
    let reject_path = f
        .dir
        .join(".tuvis-attachments")
        .join("ses-1")
        .join("gate-upload.txt");
    assert!(!reject_path.exists(), "拒绝路径零本地落盘");
    // 正例：ack 武装 → 上传轮过门（1 创建 + 1 挑战）→ 本地落盘成功。
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    let (status, reply) = upload(f.app.clone(), "过门上传内容".to_string()).await;
    assert_eq!(status, 200, "上传过门放行：{reply}");
    let written = reply["path"].as_str().expect("落盘路径").to_string();
    assert_eq!(std::fs::read_to_string(&written).unwrap(), "过门上传内容");
    assert_eq!(written, reject_path.to_string_lossy(), "落盘受控目录正确");
    // 探针写口径：拒绝轮 1 挑战 + 放行轮 1 创建 + 1 挑战 = 3。
    assert_eq!(
        f.peer.probe_writes() - probe_before,
        3,
        "上传 = 一次写轮（1 创建 + 每轮 1 挑战，两次尝试）"
    );
}

/// 探针会话管理：同一代际多轮门共用一个探针会话（创建一次复用）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_probe_session_is_created_once_and_reused() {
    let f = Fixture::new_with_gate(Switch::On, gate_arm()).await;
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    let token = view["bindingToken"].as_str().unwrap().to_string();
    for action in ["act_gate_probe_a", "act_gate_probe_b"] {
        let (status, reply) = post(
            &f.app,
            "/m/api/v1/opencode/actions",
            COOKIE,
            gate_send_body(action, &token, &[]),
        )
        .await;
        assert_eq!(status, 200, "{action}: {reply}");
    }
    // 认领零远端写（N2）；两次发送各 2 轮挑战 + 1 创建 = 5 探针写。
    assert_eq!(
        f.peer.probe_writes(),
        1 + 4,
        "创建一次、challenge 每写两轮一次"
    );
}

// ============================================================
// N1 固定手机目标与窗口导航解耦：选择时保存窗口身份，写授权用已绑定窗口，
// 不被同一窗口后续导航（Opened(B)）劫持。观察夹具走生产窗口选择及写门判据
//（查询只认「当前新鲜 Opened」——与 fresh_opened_window 同形，只是可切换）。
// ============================================================

/// 主判据（计划 N1-RED）：TUI P 在服务 S 上打开 A → 手机完成针对 A 的固定选择 →
/// 同一 P 更新 `Opened(B)`（PID、启动时刻、插件 boot、S、A 规范目录全部不变）→
/// 确认动作仍携带 A 绑定：A 原生正式记录恰一条、B 零条。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_pinned_target_survives_same_window_navigation_to_b() {
    let f = Fixture::new_with_gate_sessions(Switch::On, gate_arm(), |dir| {
        vec![
            opencode_session("ses-1", dir),
            opencode_session("ses-2", dir),
        ]
    })
    .await;
    // 前置：TUI P（pid/启动时刻/boot 固定）当前新鲜 Opened(A)。
    f.set_gate_opened("ses-1");
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    // 手机完成针对 A 的固定选择（GET view 认领 = 生产选择入口）。
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "针对 A 的固定选择必须成功：{view}");
    assert_eq!(view["route"], "opencode_native", "{view}");
    let token = view["bindingToken"].as_str().expect("绑定令牌").to_string();
    // 同一 P 浏览到 B：PID、启动时刻、插件 boot、服务 S、A 规范目录全部不变。
    f.set_gate_opened("ses-2");
    // 确认动作仍携带 A 绑定：已固定 A 的写授权用已绑定窗口，不被导航劫持。
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_pinned_nav_b", &token, &[]),
    )
    .await;
    assert_eq!(
        status, 200,
        "已固定 A 的写授权必须用选择时绑定的窗口：{reply}"
    );
    let receipt = serde_json::from_str::<serde_json::Value>(&reply).unwrap();
    assert_eq!(receipt["state"], "confirmed", "{reply}");
    // A 原生正式记录恰一条、B 零条（按会话记账的线上事实）。
    assert_eq!(
        f.peer.session_prompt_writes("ses-1"),
        1,
        "A 原生正式记录恰一条"
    );
    assert_eq!(
        f.peer.session_prompt_writes("ses-2"),
        0,
        "B 零条（不得改投 B）"
    );
    assert_eq!(f.peer.prompt_writes(), 1, "用户会话写恰一次");
}

/// 回归「A 无活跃卡但既有绑定仍有效」+「观察过期」：观察整体消失（A 无卡、无任何
/// 新鲜 `Opened`）后，已固定 A 的写授权仍用选择时绑定的窗口身份（存活 + 同代际 +
/// 同 boot），逐次门判定不变。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_pinned_binding_holds_without_active_card_and_stale_observation() {
    let f = Fixture::new_with_gate(Switch::On, gate_arm()).await;
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    let token = view["bindingToken"].as_str().unwrap().to_string();
    // 观察过期/消失：没有任何新鲜 Opened（A 无活跃卡）。
    f.clear_gate_opened();
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_pinned_stale_obs", &token, &[]),
    )
    .await;
    assert_eq!(status, 200, "既有绑定期内写授权不依赖当前新鲜观察：{reply}");
    assert_eq!(f.peer.session_prompt_writes("ses-1"), 1, "写仍落 A");
}

/// 回归「窗口退出」+「待发或未确认动作仍留在原目标」：已绑定窗口退出（进程事实
/// None）→ fail-closed 拒绝；账本里 queued / acknowledged 行原样保留（不迁移、
/// 不重投、不换目标）；旧动作仍可只读核对。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_pinned_window_exit_rejects_and_keeps_actions_on_original_target() {
    let f = Fixture::new_with_gate(Switch::On, gate_arm()).await;
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    let token = view["bindingToken"].as_str().unwrap().to_string();
    // 待发与未决动作：各一行，绑定当前代际与原目录。
    f.seed_action("act_pinned_queued", "ses-1", "queued");
    f.seed_action("act_pinned_ack", "ses-1", "acknowledged");
    // 已绑定窗口退出。
    f.set_gate_tui_alive(false);
    let before = f.peer.writes();
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_pinned_exit", &token, &[]),
    )
    .await;
    assert_eq!(status, 409, "已绑定窗口退出必须 fail-closed：{reply}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&reply).unwrap()["reasonCode"],
        "OC_GATE_PID_UNVERIFIED"
    );
    assert_eq!(f.peer.prompt_writes(), 0, "零用户会话写");
    assert_eq!(f.peer.writes(), before, "拒绝路径零新写（探针也不写）");
    // 待发或未确认动作仍留在原目标：行数、代际、目录逐字不变。
    assert_eq!(f.action_rows("act_pinned_queued"), 1);
    assert_eq!(f.action_rows("act_pinned_ack"), 1);
    for action in ["act_pinned_queued", "act_pinned_ack"] {
        let (state, _native, directory) = f.action_row(action);
        assert!(!directory.is_empty(), "{action}: 目录保留原值");
        let _ = state;
    }
    // 旧动作仍可只读核对（只读核对不受门影响）。
    let (status, _) = f.view("ses-1").await;
    assert_eq!(status, 200, "只读核对仍可用");
}

/// 回归「PID/boot 替换」：导航换代前，轮内控制文件的进程代际（expectPid）与插件
/// 代际（expectBoot）始终是**选择时绑定**的窗口身份（生产侧插件据此自检；Rust 侧
/// 取证绑定身份未被替换）；同一 pid 换成新进程代际（新启动时刻）后 → 旧绑定
/// 拒绝新写（门在探针写之前就拒绝，零探针零用户写）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_pinned_pid_or_boot_replacement_rejects_new_writes() {
    let f = Fixture::new_with_gate(Switch::On, gate_arm()).await;
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    let token = view["bindingToken"].as_str().unwrap().to_string();
    f.set_gate_opened("ses-2");
    // boot 取证（换代前）：hold-ack 挂起轮内挑战，控制文件绑定选择时的窗口身份。
    f.peer.hold_gate_ack();
    let app = f.app.clone();
    let body = gate_send_body("act_pinned_regen_boot", &token, &[]);
    let task =
        tokio::spawn(async move { post(&app, "/m/api/v1/opencode/actions", COOKIE, body).await });
    f.peer.wait_gate_ack_held().await;
    let gate_home = f.gate_home.clone().expect("装门夹具必有控制文件根");
    let control = gate_home
        .join(".tuvis/opencode-tui")
        .join(format!("{GATE_LOCKED_PID}.probe"));
    let raw = std::fs::read(&control).expect("轮内控制文件必须在场");
    let parsed: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(
        parsed["expectPid"], GATE_LOCKED_PID,
        "控制文件绑定已绑定的窗口 pid（导航后不漂移）"
    );
    assert_eq!(
        parsed["expectBoot"], GATE_BOOT,
        "控制文件的插件代际 = 选择时绑定的 boot"
    );
    f.peer.release_gate_ack();
    let (status, reply) = tokio::time::timeout(std::time::Duration::from_secs(10), task)
        .await
        .expect("发送必须结束")
        .unwrap();
    assert_eq!(status, 200, "同代际绑定窗口照常放行：{reply}");
    assert_eq!(f.peer.session_prompt_writes("ses-1"), 1);
    // PID 替换：同 pid 新代际（观察与进程事实同步换代）→ 旧绑定（旧启动时刻）不得再写；
    // 门在探针写之前即拒绝（零探针写增量、零用户写）。
    let writes_before = f.peer.writes();
    f.replace_gate_window_generation(GATE_STARTED_AT_MS + 60_000);
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_pinned_regen", &token, &[]),
    )
    .await;
    assert_eq!(status, 409, "PID/boot 替换后旧绑定必须拒绝：{reply}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&reply).unwrap()["reasonCode"],
        "OC_GATE_PID_UNVERIFIED"
    );
    assert_eq!(f.peer.prompt_writes(), 1, "零新用户会话写");
    assert_eq!(f.peer.writes(), writes_before, "拒绝发生在探针写之前");
}

/// 回归「同端口换代」：同端口被新一代际服务接管（/api/info 自报 pid 变化）→
/// ACK 后服务通道终验拒绝；恢复原服务后绑定窗口照常适用（绑定未被换代破坏）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_pinned_same_port_generation_change_rejects_then_binding_recovers() {
    let f = Fixture::new_with_gate(Switch::On, gate_arm()).await;
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    let token = view["bindingToken"].as_str().unwrap().to_string();
    f.set_gate_opened("ses-2");
    // 同端口换代：登记滞留（同端点），/api/info 自报他者 pid。
    f.peer.set_info_pid(9999);
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_pinned_regenerated", &token, &[]),
    )
    .await;
    assert_eq!(status, 409, "同端口换代必须拒绝：{reply}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&reply).unwrap()["reasonCode"],
        "OC_GENERATION_MISMATCH"
    );
    assert_eq!(f.peer.prompt_writes(), 0, "零用户会话写");
    // 原服务恢复：已绑定窗口照常适用（绑定没有因换代拒绝而被清除/换窗）。
    f.peer.set_info_pid(0);
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_pinned_regenerated_2", &token, &[]),
    )
    .await;
    assert_eq!(status, 200, "原服务恢复后绑定窗口照常适用：{reply}");
    assert_eq!(f.peer.session_prompt_writes("ses-1"), 1);
    assert_eq!(f.peer.session_prompt_writes("ses-2"), 0);
}

/// 回归「实际切服」：登记改指另一个服务（另一端点）→ 已固定目标的写必须拒绝
///（目标代际与发现不符，绑定窗口不得跟着登记漂移到别家服务）；登记还原后绑定
/// 照常适用，且全程零写落到原服务。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_pinned_actual_service_switch_rejects_bound_write() {
    let f = Fixture::new_with_gate(Switch::On, gate_arm()).await;
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    let token = view["bindingToken"].as_str().unwrap().to_string();
    f.set_gate_opened("ses-2");
    // 实际切服：登记文件改指一个不可达端点（pid 也换 → 目标代际核对先拒）。
    let state_file = f.dir.parent().unwrap().join("service.json");
    std::fs::write(
        &state_file,
        format!(
            r#"{{"id":"{PEER_SERVICE_ID}","version":"{PEER_VERSION}","url":"http://127.0.0.1:1","pid":5555,"password":"pw-fixture"}}"#
        ),
    )
    .expect("改写登记文件");
    let before = f.peer.writes();
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_pinned_switched", &token, &[]),
    )
    .await;
    assert_eq!(status, 409, "实际切服后旧绑定必须拒绝：{reply}");
    let reason = serde_json::from_str::<serde_json::Value>(&reply).unwrap()["reasonCode"]
        .as_str()
        .expect("拒绝必须带原因词")
        .to_string();
    assert!(
        reason == "OC_GENERATION_MISMATCH" || reason == "OC_TRANSPORT_FAILED",
        "切服拒绝原因可辨（代际不符或新服务不可达）：{reason}"
    );
    assert_eq!(f.peer.writes(), before, "零写落到原服务（不跟随登记漂移）");
    assert_eq!(f.peer.prompt_writes(), 0, "零用户会话写");
    // 登记还原：绑定窗口照常适用。
    std::fs::write(
        &state_file,
        format!(
            r#"{{"id":"{PEER_SERVICE_ID}","version":"{PEER_VERSION}","url":"{}","pid":{PEER_PID},"password":"pw-fixture"}}"#,
            f.peer.endpoint
        ),
    )
    .expect("还原登记文件");
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_pinned_switched_2", &token, &[]),
    )
    .await;
    assert_eq!(status, 200, "登记还原后绑定窗口照常适用：{reply}");
    assert_eq!(f.peer.session_prompt_writes("ses-1"), 1);
    assert_eq!(f.peer.session_prompt_writes("ses-2"), 0);
}

/// 回归「两个窗口都打开 A 时的选择」+「对已绑定目标禁止自动换窗口」：选择时刻
/// 把当时持有 A 的窗口（P1）绑定下来；之后另一扇窗口（P2，更大 pid）也打开 A 时，
/// 新鲜查询会指向 P2，但写授权仍锁定 P1——P2 的 ack 判 PidMismatch，P1 的 ack 放行。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_pinned_two_windows_on_a_stay_on_selected_window() {
    let f = Fixture::new_with_gate(Switch::On, gate_arm()).await;
    // 选择时刻：P1（GATE_LOCKED_PID）打开 A → 手机固定 A。
    f.set_gate_opened("ses-1");
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    let token = view["bindingToken"].as_str().unwrap().to_string();
    // 之后另一扇窗口 P2 也打开 A（新鲜查询的确定性指向会变成 P2）。
    f.set_gate_opened_window("ses-1", GATE_ALT_PID, GATE_ALT_STARTED_AT_MS);
    // P2 的 ack 不能替 P1 的绑定放行（禁止自动换窗）。
    f.peer.arm_gate_ack(GATE_ALT_PID, None);
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_pinned_two_windows", &token, &[]),
    )
    .await;
    assert_eq!(status, 409, "另一窗口的 ack 不得替绑定窗口放行：{reply}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&reply).unwrap()["reasonCode"],
        "OC_GATE_PID_MISMATCH"
    );
    assert_eq!(f.peer.prompt_writes(), 0, "零用户会话写");
    // P1（已绑定窗口）的 ack 照常放行。
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_pinned_two_windows_2", &token, &[]),
    )
    .await;
    assert_eq!(status, 200, "已绑定窗口（选择时的 P1）照常放行：{reply}");
    assert_eq!(f.peer.session_prompt_writes("ses-1"), 1);
}

/// 回归「已绑定窗口退出后不得自动换到另一扇打开 A 的窗口」：P1（已绑定）退出、
/// P2 打开 A 时，写授权仍核对 P1 的存活 → fail-closed 拒绝（绝不静默换窗续命）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_pinned_does_not_auto_switch_to_another_window_after_exit() {
    let f = Fixture::new_with_gate(Switch::On, gate_arm()).await;
    f.set_gate_opened("ses-1");
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    let token = view["bindingToken"].as_str().unwrap().to_string();
    // P1 退出；P2 打开 A（新鲜查询可找到 A 的持有者）。
    f.set_gate_tui_alive(false);
    f.set_gate_opened_window("ses-1", GATE_ALT_PID, GATE_ALT_STARTED_AT_MS);
    f.peer.arm_gate_ack(GATE_ALT_PID, None);
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_pinned_no_auto_switch", &token, &[]),
    )
    .await;
    assert_eq!(status, 409, "绑定窗口退出后不得自动换窗：{reply}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&reply).unwrap()["reasonCode"],
        "OC_GATE_PID_UNVERIFIED"
    );
    assert_eq!(f.peer.prompt_writes(), 0, "零用户会话写");
}

/// 回归「清理和重新选择」：从旧版本遗留的 draining 路由加载后，旧绑定不再适用；显式重进（`POST /opencode/route/native`，candidateToken
/// 确认的候选）是恢复绑定的唯一合法途径——重选后再次导航也不劫持。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_pinned_cleanup_then_explicit_reselect_rebinds_window() {
    let f = Fixture::new_with_gate(Switch::On, gate_arm()).await;
    f.set_gate_opened("ses-1");
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    let token = view["bindingToken"].as_str().unwrap().to_string();
    f.set_gate_opened("ses-2");
    // 模拟旧版本已经完成路由清理。
    seed_historical_draining(&f);
    assert_eq!(f.route_state("ses-1").as_deref(), Some("draining"));
    // 清理后旧绑定不再适用：同一窗口仍浏览着 B，写被拒（回退语义 fail-closed）。
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_pinned_after_cleanup", &token, &[]),
    )
    .await;
    assert_eq!(status, 409, "清理后旧绑定必须失效：{reply}");
    assert_eq!(f.peer.prompt_writes(), 0, "零用户会话写");
    // 显式重选：用户把窗口导航回 A，重新进入（candidateToken 确认）。
    f.set_gate_opened("ses-1");
    let (status, text) = post(
        &f.app,
        "/m/api/v1/opencode/route/native",
        COOKIE,
        f.native_route_request("ses-1", &token, false).await,
    )
    .await;
    assert_eq!(status, 200, "显式重选恢复路由：{text}");
    assert_eq!(f.route_state("ses-1").as_deref(), Some("active"));
    // 重选捕获了当前窗口身份：再次导航到 B 不劫持。
    f.set_gate_opened("ses-2");
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_pinned_after_reselect", &token, &[]),
    )
    .await;
    assert_eq!(status, 200, "重选后的绑定照常解耦窗口导航：{reply}");
    assert_eq!(f.peer.session_prompt_writes("ses-1"), 1);
    assert_eq!(f.peer.session_prompt_writes("ses-2"), 0);
}

/// 回归「后台 flush 加相同拒绝断言」（正向 + 反向）：已固定 A、窗口浏览 B 时，
/// 后台队列 flush 经已绑定窗口照常派发到 A；绑定窗口退出后 flush 同样拒绝，
/// 待发项原样保留（不迁移、不重投）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_pinned_flush_uses_bound_window_and_rejects_after_window_loss() {
    let f = Fixture::new_with_gate(Switch::On, gate_arm()).await;
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    let token = view["bindingToken"].as_str().unwrap().to_string();
    let directory = f.dir.canonicalize().unwrap().to_string_lossy().into_owned();
    // 暂停本地派发（expectedRevision 取当前值；pause/enqueue 都会 bump revision）。
    let pause = |f: &Fixture, action: &str, token: &str| {
        let revision: i64 = f.state.store.with(|c| {
            c.query_row(
                "SELECT revision FROM opencode_outbox_targets WHERE session_id='ses-1' AND directory=?1",
                [&directory],
                |r| r.get(0),
            )
            .unwrap_or(0)
        });
        let body = serde_json::json!({
            "actionId": format!("{action}_pause"),
            "sessionId": "ses-1",
            "bindingToken": token,
            "operation": {"kind":"set_outbox_paused","paused":true,"expectedRevision":revision},
        })
        .to_string();
        body
    };
    // 正向：窗口浏览 B（A 无活跃卡），后台 flush 仍经已绑定窗口派发到 A。
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        pause(&f, "act_pinned_flush_ok", &token),
    )
    .await;
    assert_eq!(status, 200, "暂停本地派发：{reply}");
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_pinned_flush_ok", &token, &[]),
    )
    .await;
    assert_eq!(status, 200, "入队：{reply}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&reply).unwrap()["state"],
        "queued"
    );
    f.set_gate_opened("ses-2");
    f.state.store.with(|c| {
        assert_eq!(
            c.execute(
                "UPDATE opencode_outbox_targets SET paused=0 WHERE session_id='ses-1' AND directory=?1",
                [&directory],
            )
            .unwrap(),
            1,
            "必须解暂停原目录目标"
        )
    });
    f.state
        .opencode_native
        .flush_outbox(&opencode_session("ses-1", &f.dir))
        .await
        .expect("绑定窗口期内 flush 必须派发到 A");
    assert_eq!(f.peer.session_prompt_writes("ses-1"), 1, "flush 写落 A");
    assert_eq!(f.peer.session_prompt_writes("ses-2"), 0, "B 零条");
    // 反向：绑定窗口退出 → flush 同样 fail-closed，待发项留在原目标。
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        pause(&f, "act_pinned_flush_dead", &token),
    )
    .await;
    assert_eq!(status, 200, "再暂停：{reply}");
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_pinned_flush_dead", &token, &[]),
    )
    .await;
    assert_eq!(status, 200, "入队：{reply}");
    f.state.store.with(|c| {
        assert_eq!(
            c.execute(
                "UPDATE opencode_outbox_targets SET paused=0 WHERE session_id='ses-1' AND directory=?1",
                [&directory],
            )
            .unwrap(),
            1,
            "必须解暂停原目录目标"
        )
    });
    f.set_gate_tui_alive(false);
    let err = f
        .state
        .opencode_native
        .flush_outbox(&opencode_session("ses-1", &f.dir))
        .await;
    assert!(err.is_err(), "绑定窗口退出后 flush 必须拒绝");
    assert_eq!(f.peer.session_prompt_writes("ses-1"), 1, "零新用户写");
    let (state, reason, _dir) = f.action_row("act_pinned_flush_dead");
    assert_eq!(state, "queued", "待发项留在原目标（不迁移不重投）");
    assert!(
        reason.is_some(),
        "拒绝原因落账（队列项可见原因）：{:?}",
        reason
    );
}

// ============================================================
// M1 重启后的固定绑定（复审 P1 · N1）：N1 的窗口绑定注册表是进程内存态，
// Tuvis 重启清空后，持久待发项的后台派发经 `locked_window_for_write` 的
// 「新鲜 Opened」回退查询，可能用**另一扇正打开 A 的窗口**取得授权。
// 判据：重启 + 原窗口退出（pid 位被新进程接管）+ 另一窗开 A 后，重选前零写
//（零探针写 / 零用户会话写 / 动作行保持 queued）；同一窗口仍在时绑定身份恢复，
// 照常放行；显式重选是换窗的唯一合法途径。
// ============================================================

/// 复审指定 RED：原服务存活、原窗口 P1 退出（代际替换）、另一窗口 P2 打开 A、
/// Tuvis 重启（绑定注册表清空）→ 持久待发项的后台派发必须**零写保留**，
/// 不得经新鲜回退查询用 P2 取得授权（回声门/ACK 终验/语义准入判据不放宽，
/// 只收紧锁定窗口来源）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_pinned_restart_window_replaced_background_flush_stays_zero_write() {
    let f = Fixture::new_with_gate(Switch::On, gate_arm()).await;
    f.set_gate_opened("ses-1");
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    // 手机固定选择 A（选择事件：绑定窗口 P1）。
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "针对 A 的固定选择必须成功：{view}");
    let token = view["bindingToken"].as_str().expect("绑定令牌").to_string();
    // 持久待发项：暂停本地派发 → 入队 → 解暂停（与既有 flush 用例同一夹具纪律）。
    let directory = f.dir.canonicalize().unwrap().to_string_lossy().into_owned();
    let revision: i64 = f.state.store.with(|c| {
        c.query_row(
            "SELECT revision FROM opencode_outbox_targets WHERE session_id='ses-1' AND directory=?1",
            [&directory],
            |r| r.get(0),
        )
        .unwrap_or(0)
    });
    let pause = serde_json::json!({
        "actionId": "act_pinned_restart_flush_pause",
        "sessionId": "ses-1",
        "bindingToken": token,
        "operation": {"kind":"set_outbox_paused","paused":true,"expectedRevision":revision},
    })
    .to_string();
    let (status, reply) = post(&f.app, "/m/api/v1/opencode/actions", COOKIE, pause).await;
    assert_eq!(status, 200, "暂停本地派发：{reply}");
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_pinned_restart_flush", &token, &[]),
    )
    .await;
    assert_eq!(status, 200, "入队：{reply}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&reply).unwrap()["state"],
        "queued"
    );
    f.state.store.with(|c| {
        assert_eq!(
            c.execute(
                "UPDATE opencode_outbox_targets SET paused=0 WHERE session_id='ses-1' AND directory=?1",
                [&directory],
            )
            .unwrap(),
            1,
            "必须解暂停原目录目标"
        )
    });
    // Tuvis 重启：内存绑定注册表清空（持久化绑定身份不受影响——修复面）。
    f.state
        .opencode_native
        .clear_pinned_windows_for_restart_tests();
    // 原窗口 P1 退出（pid 位被新进程接管 = 代际替换）；另一扇窗口 P2 打开 A。
    f.replace_gate_window_generation(GATE_STARTED_AT_MS + 60_000);
    f.set_gate_opened_window("ses-1", GATE_ALT_PID, GATE_ALT_STARTED_AT_MS);
    // 只有 P2 的回声在服务端可达（P1 已死；新鲜回退查询此刻指向 P2）。
    f.peer.arm_gate_ack(GATE_ALT_PID, None);
    let probes_before = f.peer.probe_writes();
    // 后台派发：重选前必须零写。
    let err = f
        .state
        .opencode_native
        .flush_outbox(&opencode_session("ses-1", &f.dir))
        .await;
    assert!(
        err.is_err(),
        "重启换窗后重选前不得派发（回退查询不得授权另一窗）:{err:?}"
    );
    assert_eq!(f.peer.prompt_writes(), 0, "零用户会话写");
    assert_eq!(f.peer.session_prompt_writes("ses-1"), 0, "A 原生记录零条");
    assert_eq!(f.peer.probe_writes(), probes_before, "零探针写");
    let (state, reason, _dir) = f.action_row("act_pinned_restart_flush");
    assert_eq!(state, "queued", "动作行保持 queued（零状态漂移）");
    // 拒绝机制锁定：恢复的绑定身份指向 P1，而 P1 的 pid 位已被新进程接管（代际
    // 替换）→ 门按窗口存活/代际判据拒绝。若是别的拒绝码，说明恢复语义漂移。
    assert_eq!(
        reason.as_deref(),
        Some("OC_GATE_PID_UNVERIFIED"),
        "拒绝原因必须是「锁定窗口（恢复的 P1）进程代际不可证」：{reason:?}"
    );
}

/// 回归矩阵 1（重启后同一窗口仍开 A）：重启后即使另一扇窗口 P2 也打开 A
///（新鲜查询指向 P2），写授权仍锁定**恢复出来的 P1 绑定身份**——先只武装 P2
/// 的 ack（PID_MISMATCH 证明锁的是 P1 不是回退的 P2），再武装 P1 的 ack 放行；
/// 随后 P1 换代（pid 位被接管）→ 旧绑定仍拒（窗口换代仍拒）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_pinned_restart_restores_binding_and_still_rejects_regeneration() {
    let f = Fixture::new_with_gate(Switch::On, gate_arm()).await;
    f.set_gate_opened("ses-1");
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    let token = view["bindingToken"].as_str().unwrap().to_string();
    // Tuvis 重启：注册表清空；P1 仍存活（代际不变），另一扇窗口 P2 也打开 A。
    f.state
        .opencode_native
        .clear_pinned_windows_for_restart_tests();
    f.set_gate_opened_window("ses-1", GATE_ALT_PID, GATE_ALT_STARTED_AT_MS);
    // 只武装 P2 的 ack：若经新鲜回退锁了 P2，这一轮会放行（= N1 前语义）；
    // 恢复的绑定锁 P1 → P2 的 ack 判 PidMismatch。
    f.peer.arm_gate_ack(GATE_ALT_PID, None);
    let writes_before = f.peer.writes();
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_pinned_restart_same_window", &token, &[]),
    )
    .await;
    assert_eq!(
        status, 409,
        "恢复绑定必须锁定 P1（P2 的 ack 不得放行）：{reply}"
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&reply).unwrap()["reasonCode"],
        "OC_GATE_PID_MISMATCH",
        "拒绝机制 = 锁定窗口（恢复的 P1）与回声 ack（P2）pid 比对不符：{reply}"
    );
    assert!(
        f.peer.probe_writes() > 0 || f.peer.writes() > writes_before,
        "拒绝发生在挑战轮内（恢复绑定锁定 P1 的探针真实跑过）"
    );
    assert_eq!(f.peer.prompt_writes(), 0, "零用户会话写");
    // 武装 P1 的 ack：同一窗口仍开 A → 重启后照常放行（绑定身份正确恢复）。
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_pinned_restart_same_window_ok", &token, &[]),
    )
    .await;
    assert_eq!(status, 200, "重启后同一窗口仍开 A 照常放行：{reply}");
    assert_eq!(f.peer.session_prompt_writes("ses-1"), 1, "写仍落 A");
    assert_eq!(f.peer.session_prompt_writes("ses-2"), 0, "B 零条");
    // 窗口换代仍拒：P1 的 pid 位被新进程接管（代际替换）→ 恢复绑定（旧代际）拒绝。
    f.replace_gate_window_generation(GATE_STARTED_AT_MS + 120_000);
    let prompt_before = f.peer.prompt_writes();
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_pinned_restart_regen", &token, &[]),
    )
    .await;
    assert_eq!(status, 409, "窗口换代后恢复绑定必须拒绝：{reply}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&reply).unwrap()["reasonCode"],
        "OC_GATE_PID_UNVERIFIED"
    );
    assert_eq!(f.peer.prompt_writes(), prompt_before, "零新用户会话写");
}

/// 回归矩阵 3+4+5（前台用户动作路径 + 窗口退出 + 显式重选恢复写）：重启 + 原窗口
/// P1 退出（pid 位被接管）+ 另一窗 P2 打开 A → 用户动作必须拒绝（零写）；用户把
/// 新窗口留在 A 显式重选（candidateToken 确认）→ 绑定换到 P2 → 恢复写。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_pinned_restart_user_action_rejected_until_explicit_reselect() {
    let f = Fixture::new_with_gate(Switch::On, gate_arm()).await;
    f.set_gate_opened("ses-1");
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    let token = view["bindingToken"].as_str().unwrap().to_string();
    // 重启 + P1 退出（代际替换）+ P2 打开 A（P2 的回声是服务端唯一应答者）。
    f.state
        .opencode_native
        .clear_pinned_windows_for_restart_tests();
    f.replace_gate_window_generation(GATE_STARTED_AT_MS + 60_000);
    f.set_gate_opened_window("ses-1", GATE_ALT_PID, GATE_ALT_STARTED_AT_MS);
    f.peer.arm_gate_ack(GATE_ALT_PID, None);
    let writes_before = f.peer.writes();
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_pinned_restart_user", &token, &[]),
    )
    .await;
    assert_eq!(status, 409, "重启换窗后用户动作重选前必须拒绝：{reply}");
    assert_eq!(f.peer.prompt_writes(), 0, "零用户会话写");
    assert!(
        f.peer.writes() >= writes_before,
        "探针写只增不减（拒绝发生在挑战之后，用户写为零）"
    );
    // 显式重选：candidateToken 确认（用户把当前窗口 P2 留在 A 上重进）。
    let (status, text) = post(
        &f.app,
        "/m/api/v1/opencode/route/native",
        COOKIE,
        f.native_route_request("ses-1", &token, false).await,
    )
    .await;
    assert_eq!(status, 200, "显式重选恢复路由并换绑当前窗口：{text}");
    assert_eq!(f.route_state("ses-1").as_deref(), Some("active"));
    // 重选后写授权锁定 P2：P2 的 ack 放行（换窗的唯一合法途径 = 显式重选）。
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_pinned_restart_after_reselect", &token, &[]),
    )
    .await;
    assert_eq!(status, 200, "显式重选后恢复写（新绑定窗口 P2）：{reply}");
    assert_eq!(f.peer.session_prompt_writes("ses-1"), 1, "写仍落 A");
}

const NATIVE_RESELECT_PATH: &str = "/m/api/v1/opencode/route/native";

async fn reselect_body(f: &Fixture, token: &str, request_id: &str) -> String {
    let mut body: serde_json::Value =
        serde_json::from_str(&f.native_route_request("ses-1", token, false).await).unwrap();
    body["requestId"] = serde_json::json!(request_id);
    body.to_string()
}

fn reselect_status_uri(request_id: &str) -> String {
    format!("{NATIVE_RESELECT_PATH}/status?sessionId=ses-1&requestId={request_id}")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reselect_receipt_commits_on_first_confirmation_after_restart() {
    const REQUEST: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
    const OLD_SERVICE_PID: u32 = PEER_PID + 1;
    const OLD_SERVICE_STARTED_AT_MS: i64 = PEER_STARTED_AT_MS - 1;
    let f = Fixture::new_with_gate(Switch::On, gate_arm()).await;
    // 直接恢复旧应用留下的路由与窗口，保持新 hub 从未安装过 source。
    f.state.store.with(|conn| {
        let directory = f.dir.to_string_lossy();
        // 窗口表由生产代码惰性创建，冷启动夹具需还原旧库的表结构。
        conn.execute_batch(
            "CREATE TABLE opencode_native_pinned_windows (
                session_id TEXT PRIMARY KEY,
                service_id TEXT NOT NULL,
                runtime_pid INTEGER NOT NULL,
                runtime_started_at_ms INTEGER NOT NULL,
                directory TEXT NOT NULL,
                window_pid INTEGER NOT NULL,
                window_started_at_ms INTEGER NOT NULL,
                window_boot TEXT,
                updated_at INTEGER NOT NULL
            )",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO opencode_native_routes (session_id, service_id, runtime_pid, \
                 runtime_started_at_ms, directory, route_state, claimed_at, updated_at) \
                 VALUES ('ses-1', ?1, ?2, ?3, ?4, 'active', 1, 1)",
            rusqlite::params![PEER_SERVICE_ID, OLD_SERVICE_PID, OLD_SERVICE_STARTED_AT_MS, directory],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO opencode_native_pinned_windows (session_id, service_id, runtime_pid, \
                 runtime_started_at_ms, directory, window_pid, window_started_at_ms, window_boot, updated_at) \
                 VALUES ('ses-1', ?1, ?2, ?3, ?4, ?5, ?6, ?7, 1)",
            rusqlite::params![
                PEER_SERVICE_ID,
                OLD_SERVICE_PID,
                OLD_SERVICE_STARTED_AT_MS,
                directory,
                GATE_LOCKED_PID,
                GATE_STARTED_AT_MS,
                GATE_BOOT,
            ],
        )
        .unwrap();
    });
    f.set_gate_opened_window("ses-1", GATE_ALT_PID, GATE_ALT_STARTED_AT_MS);
    f.peer.arm_gate_ack(GATE_ALT_PID, None);

    let (status, preview) = f
        .get("/m/api/v1/opencode/route/preview?sessionId=ses-1")
        .await;
    assert_eq!(status, 200, "预览候选：{preview}");
    assert_eq!(preview["candidate"]["pid"], PEER_PID);
    let previous = &preview["previousRoute"];
    let (status, reply) = post(
        &f.app,
        NATIVE_RESELECT_PATH,
        COOKIE,
        serde_json::json!({
            "sessionId": "ses-1",
            "bindingToken": previous["bindingToken"],
            "routeVersion": previous["routeVersion"],
            "candidateToken": preview["previewToken"],
            "requestId": REQUEST,
        })
        .to_string(),
    )
    .await;
    assert_eq!(status, 200, "重启后的首次重选应提交：{reply}");
    let committed: serde_json::Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(committed["state"], "committed");
    assert_eq!(committed["requestId"], REQUEST);
    assert_eq!(committed["current"], true);
    assert_eq!(
        committed["bindingToken"],
        preview["candidate"]["bindingToken"]
    );
    assert_eq!(
        committed["routeVersion"],
        previous["routeVersion"].as_i64().unwrap() + 1
    );
    assert_eq!(committed["window"]["pid"], GATE_ALT_PID);
    assert_eq!(committed["window"]["startedAtMs"], GATE_ALT_STARTED_AT_MS);
    assert_eq!(
        f.get(&reselect_status_uri(REQUEST)).await,
        (axum::http::StatusCode::OK, committed)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reselect_receipt_records_only_the_winner_of_concurrent_confirmations() {
    const FIRST: &str = "11111111111111111111111111111111";
    const SECOND: &str = "22222222222222222222222222222222";
    const UNKNOWN: &str = "33333333333333333333333333333333";
    let f = Fixture::new_with_gate(Switch::On, gate_arm()).await;
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    let token = view["bindingToken"].as_str().unwrap();
    seed_historical_draining(&f);
    assert_eq!(f.route_state("ses-1").as_deref(), Some("draining"));
    let first = reselect_body(&f, token, FIRST).await;
    let second = reselect_body(&f, token, SECOND).await;
    let initial_version = serde_json::from_str::<serde_json::Value>(&first).unwrap()
        ["routeVersion"]
        .as_i64()
        .unwrap();

    // 首次确认读了旧路由后停在上游 HTTP，第二次确认用同一版本先提交。
    f.peer.shared.hold_info_read.store(true, Ordering::SeqCst);
    let app = f.app.clone();
    let pending =
        tokio::spawn(async move { post(&app, NATIVE_RESELECT_PATH, COOKIE, first).await });
    let timeout = std::time::Duration::from_secs(10);
    tokio::time::timeout(timeout, f.peer.shared.info_blocked.notified())
        .await
        .expect("首次确认必须停在候选读取");

    let first_uri = reselect_status_uri(FIRST);
    let (status, receipt) = f.get(&first_uri).await;
    assert_eq!(status, 200, "{receipt}");
    assert_eq!(receipt["state"], "in_progress", "{receipt}");
    assert_eq!(receipt["requestId"], FIRST);
    assert_eq!(receipt["sessionId"], "ses-1");
    let other = f
        .app
        .clone()
        .oneshot(req("GET", &first_uri, Some(COOKIE_OTHER), None))
        .await
        .unwrap();
    let (missing_status, missing) = f.get(&reselect_status_uri(UNKNOWN)).await;
    assert_eq!(missing_status, 404, "{missing}");
    assert_eq!(other.status(), missing_status);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body_of(other).await).unwrap(),
        missing
    );

    let winner =
        tokio::time::timeout(timeout, post(&f.app, NATIVE_RESELECT_PATH, COOKIE, second)).await;
    f.peer.shared.info_release.notify_one();
    let (status, reply) = winner.expect("第二次确认必须在第一次放行前结束");
    assert_eq!(status, 200, "{reply}");
    let committed: serde_json::Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(committed["state"], "committed");
    assert_eq!(committed["requestId"], SECOND);
    assert_eq!(committed["current"], true);
    assert_eq!(committed["bindingToken"], token);
    assert_eq!(committed["routeVersion"], initial_version + 1);
    assert_eq!(committed["window"]["pid"], GATE_LOCKED_PID);
    assert_eq!(committed["window"]["startedAtMs"], GATE_STARTED_AT_MS);
    assert_eq!(committed["window"]["boot"], GATE_BOOT);

    let (status, reply) = tokio::time::timeout(timeout, pending)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status, 409, "{reply}");
    let error: serde_json::Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(error["reasonCode"], "OC_BINDING_STALE", "{error}");
    let (status, rejected) = f.get(&first_uri).await;
    assert_eq!(status, 200, "{rejected}");
    assert_eq!(rejected["state"], "not_applied");
    assert_eq!(
        f.get(&reselect_status_uri(SECOND)).await,
        (axum::http::StatusCode::OK, committed)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reselect_receipt_becomes_historical_after_same_service_window_change() {
    const FIRST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const SECOND: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let f = Fixture::new_with_gate(Switch::On, gate_arm()).await;
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    let token = view["bindingToken"].as_str().unwrap();
    let (status, reply) = post(
        &f.app,
        NATIVE_RESELECT_PATH,
        COOKIE,
        reselect_body(&f, token, FIRST).await,
    )
    .await;
    assert_eq!(status, 200, "{reply}");
    let mut original: serde_json::Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(original["state"], "committed");
    assert_eq!(original["current"], true);
    assert_eq!(original["window"]["pid"], GATE_LOCKED_PID);

    f.set_gate_opened_window("ses-1", GATE_ALT_PID, GATE_ALT_STARTED_AT_MS);
    f.peer.arm_gate_ack(GATE_ALT_PID, None);
    let (status, reply) = post(
        &f.app,
        NATIVE_RESELECT_PATH,
        COOKIE,
        reselect_body(&f, token, SECOND).await,
    )
    .await;
    assert_eq!(status, 200, "{reply}");
    let replacement: serde_json::Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(replacement["state"], "committed");
    assert_eq!(replacement["current"], true);
    assert_eq!(replacement["bindingToken"], original["bindingToken"]);
    assert_eq!(replacement["window"]["pid"], GATE_ALT_PID);
    assert_eq!(replacement["window"]["startedAtMs"], GATE_ALT_STARTED_AT_MS);
    assert_eq!(
        replacement["routeVersion"],
        original["routeVersion"].as_i64().unwrap() + 1
    );

    original["current"] = serde_json::json!(false);
    assert_eq!(
        f.get(&reselect_status_uri(FIRST)).await,
        (axum::http::StatusCode::OK, original)
    );
    assert_eq!(
        f.get(&reselect_status_uri(SECOND)).await,
        (axum::http::StatusCode::OK, replacement)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reselect_receipt_requires_a_locked_window() {
    const REQUEST: &str = "dddddddddddddddddddddddddddddddd";
    // 无归属门的夹具允许旧式确认，可单独检验带 ID 的提交点窗口要求。
    let f = Fixture::single(Switch::On).await;
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    let token = view["bindingToken"].as_str().unwrap();
    seed_historical_draining(&f);
    let before = f.route_request("ses-1", token, false);
    let body = reselect_body(&f, token, REQUEST).await;
    let (status, reply) = post(&f.app, NATIVE_RESELECT_PATH, COOKIE, body).await;
    assert_eq!(status, 409, "{reply}");
    let (status, receipt) = f.get(&reselect_status_uri(REQUEST)).await;
    assert_eq!(status, 200, "{receipt}");
    assert_eq!(receipt["state"], "not_applied");
    assert_eq!(f.route_state("ses-1").as_deref(), Some("draining"));
    assert_eq!(f.route_request("ses-1", token, false), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reselect_receipt_failure_rolls_back_route_and_window() {
    const REQUEST: &str = "cccccccccccccccccccccccccccccccc";
    let f = Fixture::new_with_gate(Switch::On, gate_arm()).await;
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    let token = view["bindingToken"].as_str().unwrap();
    let read_binding = || {
        f.state.store.with(|conn| {
            conn.query_row(
                "SELECT r.route_version, r.route_state, w.window_pid, w.window_started_at_ms, w.window_boot \
                 FROM opencode_native_routes r JOIN opencode_native_pinned_windows w USING (session_id) \
                 WHERE r.session_id = 'ses-1'",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, u32>(2)?,
                    row.get::<_, i64>(3)?, row.get::<_, String>(4)?)),
            ).unwrap()
        })
    };
    let before = read_binding();
    assert_eq!(before.2, GATE_LOCKED_PID);
    f.set_gate_opened_window("ses-1", GATE_ALT_PID, GATE_ALT_STARTED_AT_MS);
    f.peer.arm_gate_ack(GATE_ALT_PID, None);
    let request = reselect_body(&f, token, REQUEST).await;
    // 仅在夹具数据库阻断 committed 更新，允许失败路径记账为 not_applied。
    f.state.store.with(|conn| {
        conn.execute_batch(
            "CREATE TEMP TRIGGER reject_committed_receipt \
             BEFORE UPDATE ON opencode_native_reselections WHEN NEW.state = 'committed' \
             BEGIN SELECT RAISE(ABORT, 'fixture rejected committed receipt'); END;",
        )
        .unwrap();
    });
    let (status, reply) = post(&f.app, NATIVE_RESELECT_PATH, COOKIE, request).await;
    assert!((400..600).contains(&status.as_u16()), "{status}: {reply}");
    let (status, receipt) = f.get(&reselect_status_uri(REQUEST)).await;
    assert_eq!(status, 200, "{receipt}");
    assert_eq!(receipt["state"], "not_applied");
    assert_eq!(read_binding(), before);
}

/// 回归矩阵 6（非持久会话重启后行为不变）：无待发项的已归原会话重启后，
/// 只读投影、路由归属、队列视图全部不变——不派发、不落动作行、不改路由。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_pinned_restart_without_pending_keeps_readonly_semantics() {
    let f = Fixture::new_with_gate(Switch::On, gate_arm()).await;
    f.set_gate_opened("ses-1");
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["route"], "opencode_native", "{view}");
    // 无任何待发/入队动作。
    let writes_before = f.peer.writes();
    f.state
        .opencode_native
        .clear_pinned_windows_for_restart_tests();
    // 后台派发循环对无队列会话零动作：flush 直呼无队列即 Ok 且零写。
    f.state
        .opencode_native
        .flush_outbox(&opencode_session("ses-1", &f.dir))
        .await
        .expect("无待发项的 flush 是空操作");
    assert_eq!(f.peer.writes(), writes_before, "零写（探针与用户写都没有）");
    assert_eq!(f.peer.prompt_writes(), 0);
    // 只读投影照常可用（读路径不过写门，重启不影响）。
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "重启后只读投影照常：{view}");
    assert_eq!(
        f.route_state("ses-1").as_deref(),
        Some("active"),
        "路由归属不变"
    );
}

// ============================================================
// H6（HF）：服务换代手机自助恢复三段链（同目录新代际接管）
//
// 拓扑 = 服务换代复现的线上事实：旧代际 serve 退出（进程事实把旧 pid 读成新
// lstart = PID 复用 ⇒ 死亡可证明），同目录新代际接管登记。手机三段链：
// ① 错误提示：旧绑定发送被确定拒绝（OC_RUNTIME_UNVERIFIED，快照前置零写出）；
// ② 旧动作核对：只读（跨代 read-back 收敛 Confirmed，或未命中保持 + runtimeDead
//    宣告 → 显式确认终态未知归档）；未决动作在场时显式重选被拒（零迁移零重投）；
// ③ 显式重选：preview（新代际候选）+ candidateToken CAS 换代登记；随后新发送
//    **恰好一次**落新代际，旧代际零追加写（零重投）。
// ============================================================

const H6_GEN2_SERVICE_ID: &str = "svc-gen2";
const H6_GEN2_PID: u32 = 6400;
const H6_GEN2_STARTED_AT_MS: i64 = 5_000;

/// 换代夹具：旧代际认领 + 真实 send（受理行带真实历史写）→ 登记与进程事实同切
/// 新代际（同目录新 peer、新 pid、新 lstart）。返回（夹具、新代际 peer、旧绑定
/// 令牌、受理动作 id）。`readback_hit` = 新代际上会话记录已含 `msg_act_h6`
/// （换代不换存储的线上事实：跨代 read-back 可收敛）。
async fn h6_generation_swap_fixture(readback_hit: bool) -> (Fixture, Peer, String, &'static str) {
    let f = Fixture::new_with_gate(Switch::On, gate_arm()).await;
    f.set_gate_opened("ses-1");
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    // 旧代际：认领 + 真实发送（写恰一次落旧代际）。
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "旧代际认领：{view}");
    let token = view["bindingToken"].as_str().unwrap().to_string();
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_h6", &token, &[]),
    )
    .await;
    assert_eq!(status, 200, "旧代际发送受理：{reply}");
    assert_eq!(f.peer.session_prompt_writes("ses-1"), 1, "旧代际恰一次写");
    // 夹具口径：假 peer 回显让真实 send 立即收敛 confirmed；换代场景的账本中间态是
    // 「已受理未确认」（写已发生、回读回执未及）——把行还原为 acknowledged 再换代，
    // 与 H6 拓扑（kill 落在 ack 与 read-back 之间）对齐。
    f.state
        .store
        .with(|c| {
            c.execute(
                "UPDATE opencode_native_actions SET state='acknowledged', native_id=NULL \
                 WHERE action_id='act_h6'",
                [],
            )
        })
        .expect("还原受理态");
    // 服务换代：同目录新 peer 接管（登记 + 进程事实同切；旧 pid 被读成新 lstart
    // = PID 复用 ⇒ 旧代际死亡可证明）。换代不换存储：会话记录按参数预置。
    let peer2 = Peer::start(&f.dir).await;
    peer2.set_info_pid(H6_GEN2_PID);
    // 换代后 echo-challenge 打向新代际通道：锁定 TUI（同一扇窗，TUI 存活）照常应答。
    peer2.arm_gate_ack(GATE_LOCKED_PID, None);
    if readback_hit {
        peer2
            .shared
            .prompts
            .lock()
            .unwrap()
            .push(serde_json::json!({
                "type":"user", "id":"msg_act_h6", "text":"归属写门放行消息", "time":{"created":1}
            }));
    }
    std::fs::write(
        f._temp.path().join("service.json"),
        format!(
            r#"{{"id":"{H6_GEN2_SERVICE_ID}","version":"{PEER_VERSION}","url":"{}","pid":{H6_GEN2_PID},"password":"pw-fixture"}}"#,
            peer2.endpoint
        ),
    )
    .expect("换代登记文件");
    f.service_generation
        .store(H6_GEN2_STARTED_AT_MS, Ordering::SeqCst);
    (f, peer2, token, "act_h6")
}

/// 三段链 · 收敛臂：跨代 read-back 命中 → 核对 Confirmed → 重选直接放行 →
/// 新代际恰一次写，旧代际零追加写。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn h6_generation_swap_readback_confirms_then_reselect_delivers_exactly_once() {
    let (f, peer2, token, action_id) = h6_generation_swap_fixture(true).await;

    // ① 错误提示段：旧绑定发送 → 确定拒绝（快照前置：旧代际进程事实失配）
    let old_writes_before_refusal = f.peer.writes();
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_h6_refused", &token, &[]),
    )
    .await;
    assert_eq!(status, 409, "换代后旧绑定发送必须确定拒绝：{reply}");
    let reply: serde_json::Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(reply["reasonCode"], "OC_RUNTIME_UNVERIFIED", "{reply}");
    assert_eq!(
        f.peer.writes(),
        old_writes_before_refusal,
        "旧代际零追加写（拒绝在快照处短路，探针也没有）"
    );
    assert_eq!(peer2.writes(), 0, "新代际零写");

    // ①' 旧绑定只读与无绑定重读都 409（路由行钉住旧代际：restore 恢复路径同样按
    //    账本代际核对，进程事实失配即拒）——换代后的只读恢复唯一路 = 显式重选
    //    （F5 实录语义）；拒绝体带 routeVersion 供重选请求一次成形（patch6）。
    let (status, reply) =
        get_bound(&f.app, "/m/api/v1/opencode/view?session_id=ses-1", &token).await;
    assert_eq!(status, 409, "旧绑定只读必须拒绝：{reply}");
    assert_eq!(reply["reasonCode"], "OC_RUNTIME_UNVERIFIED");
    let (status, reply) = f.view("ses-1").await;
    assert_eq!(status, 409, "无绑定重读同样钉在旧代际：{reply}");
    assert_eq!(reply["reasonCode"], "OC_RUNTIME_UNVERIFIED");
    assert_eq!(reply["routeVersion"], 1, "拒绝体携带只读 routeVersion");
    assert_eq!(
        f.route_state("ses-1").as_deref(),
        Some("active"),
        "路由行保持（不自动换代）"
    );
    let (service_id, pid, started, _, _) = f.state.store.with(|c| {
        c.query_row(
            "SELECT service_id,runtime_pid,runtime_started_at_ms,directory,route_state \
             FROM opencode_native_routes WHERE session_id='ses-1'",
            [],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                ))
            },
        )
        .unwrap()
    });
    assert_eq!(
        (service_id.as_str(), pid, started),
        (PEER_SERVICE_ID, i64::from(PEER_PID), PEER_STARTED_AT_MS),
        "路由行仍锁定旧代际（零自动迁移）"
    );

    // ② 核对段：跨代 read-back 命中（原会话 + 规范目录 + msg_<actionId> 三元组）
    //    ⇒ Confirmed（零重投，只读 GET）
    let (status, reply) = post(
        &f.app,
        &format!("/m/api/v1/opencode/actions/{action_id}/verify"),
        COOKIE,
        String::new(),
    )
    .await;
    assert_eq!(status, 200, "跨代核对：{reply}");
    let view: serde_json::Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(view["state"], "confirmed", "{view}");
    assert_eq!(view["nativeId"], "msg_act_h6");
    assert_eq!(peer2.writes(), 0, "核对零写（只读 read-back）");
    assert_eq!(
        f.peer.writes(),
        old_writes_before_refusal,
        "旧代际零追加写（核对也不触旧代际）"
    );

    // ③ 显式重选段：preview（新代际候选）+ candidateToken CAS → 路由行原子换代
    let (status, preview) = f
        .get("/m/api/v1/opencode/route/preview?sessionId=ses-1")
        .await;
    assert_eq!(status, 200, "换代候选预览：{preview}");
    assert_eq!(preview["candidate"]["serviceId"], H6_GEN2_SERVICE_ID);
    assert_eq!(preview["candidate"]["pid"], H6_GEN2_PID);
    let request = serde_json::json!({
        "sessionId": "ses-1",
        "bindingToken": token,
        "routeVersion": 1,
        "candidateToken": preview["previewToken"],
    });
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/route/native",
        COOKIE,
        request.to_string(),
    )
    .await;
    assert_eq!(
        status, 200,
        "换代重选（旧代际死亡可证明豁免旧绑定比对）：{reply}"
    );
    let reply: serde_json::Value = serde_json::from_str(&reply).unwrap();
    let reselected_token = reply["bindingToken"].as_str().unwrap().to_string();
    assert_ne!(reselected_token, token, "重选回执 = 新代际绑定");
    let (service_id, pid, started, _, route_state) = f.state.store.with(|c| {
        c.query_row(
            "SELECT service_id,runtime_pid,runtime_started_at_ms,directory,route_state \
             FROM opencode_native_routes WHERE session_id='ses-1'",
            [],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                ))
            },
        )
        .unwrap()
    });
    assert_eq!(route_state, "active");
    assert_eq!(
        (service_id.as_str(), pid, started),
        (
            H6_GEN2_SERVICE_ID,
            i64::from(H6_GEN2_PID),
            H6_GEN2_STARTED_AT_MS
        ),
        "路由行原子换代到新代际"
    );

    // ④ 发送恰一次：新代际绑定发送 → 恰一次落新代际；旧代际零追加写
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_h6_after_reselect", &reselected_token, &[]),
    )
    .await;
    assert_eq!(status, 200, "新代际发送受理：{reply}");
    let reply: serde_json::Value = serde_json::from_str(&reply).unwrap();
    // 回执态 = confirmed/acknowledged 均可（假 peer 回显让回读即时收敛；线上受理窗
    // 为 acknowledged）——恰一次断言落在用户写计数上。
    assert!(
        matches!(
            reply["state"].as_str(),
            Some("confirmed") | Some("acknowledged")
        ),
        "{reply}"
    );
    assert_eq!(
        peer2.session_prompt_writes("ses-1"),
        1,
        "新代际恰一次写（msg_act 恰 1）"
    );
    assert_eq!(
        f.peer.writes(),
        old_writes_before_refusal,
        "旧代际零重投（写计数冻结在换代前）"
    );
}

/// 三段链 · 保持臂：跨代 read-back 未命中 → 核对保持受理（runtimeDead 宣告）→
/// 未决屏障拒重选 → 显式确认终态未知归档 → 重选放行 → 恰一次写。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn h6_generation_swap_readback_keeps_row_until_confirmed_then_delivers_exactly_once() {
    let (f, peer2, token, action_id) = h6_generation_swap_fixture(false).await;
    let old_writes_baseline = f.peer.writes();

    // ② 核对段（未命中）：保持受理 + 死代际宣告（不假收敛、不重发）
    let (status, reply) = post(
        &f.app,
        &format!("/m/api/v1/opencode/actions/{action_id}/verify"),
        COOKIE,
        String::new(),
    )
    .await;
    assert_eq!(status, 200, "核对未命中保持受理：{reply}");
    let view: serde_json::Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(view["state"], "acknowledged", "{view}");
    assert_eq!(view["runtimeDead"], true, "死亡宣告驱动确认出口");
    assert_eq!(peer2.session_prompt_writes("ses-1"), 0, "核对零用户写");

    // ③ 未决屏障：显式重选被拒（不迁移未知旧动作），文案指向核对/确认出口
    let (status, preview) = f
        .get("/m/api/v1/opencode/route/preview?sessionId=ses-1")
        .await;
    assert_eq!(status, 200, "换代候选预览（只读）：{preview}");
    let request = serde_json::json!({
        "sessionId": "ses-1",
        "bindingToken": token,
        "routeVersion": 1,
        "candidateToken": preview["previewToken"],
    });
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/route/native",
        COOKIE,
        request.to_string(),
    )
    .await;
    assert_eq!(status, 409, "未决动作必须挡住换代重选：{reply}");
    let reply: serde_json::Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(reply["reasonCode"], "OC_ROUTE_CHANGED", "{reply}");
    assert!(
        reply["reason"]
            .as_str()
            .unwrap()
            .contains("结果未确认的原生动作"),
        "拒绝文案指向可操作出口：{reply}"
    );
    assert_eq!(
        peer2.session_prompt_writes("ses-1"),
        0,
        "拒绝零用户写（归属探针写不算投递）"
    );

    // ②' 显式确认终态未知：read-back 未命中 ⇒ UnknownConfirmed（账本保持未知）
    let (status, reply) = post(
        &f.app,
        &format!("/m/api/v1/opencode/actions/{action_id}/confirm-unknown"),
        COOKIE,
        String::new(),
    )
    .await;
    assert_eq!(status, 200, "死代际确认归档：{reply}");
    let view: serde_json::Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(view["state"], "unknown_confirmed", "{view}");
    assert_eq!(view["reasonCode"], "user_confirmed_unknown");
    assert_eq!(
        f.peer.writes(),
        old_writes_baseline,
        "确认零写（旧代际计数冻结）"
    );
    assert_eq!(peer2.session_prompt_writes("ses-1"), 0);

    // ③' 屏障解除：重选换代放行
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/route/native",
        COOKIE,
        f.native_route_request("ses-1", &token, false).await,
    )
    .await;
    assert_eq!(status, 200, "确认后重选屏障解除：{reply}");
    let reply: serde_json::Value = serde_json::from_str(&reply).unwrap();
    let reselected_token = reply["bindingToken"].as_str().unwrap().to_string();

    // ④ 发送恰一次
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_h6_after_confirm", &reselected_token, &[]),
    )
    .await;
    assert_eq!(status, 200, "新代际发送受理：{reply}");
    assert_eq!(peer2.session_prompt_writes("ses-1"), 1, "新代际恰一次写");
    assert_eq!(f.peer.writes(), old_writes_baseline, "旧代际零重投");
}

/// 回归「上传加相同拒绝断言」：已固定 A、窗口浏览 B 时上传经绑定窗口放行；
/// 绑定窗口退出后上传同样 fail-closed（受控码可辨 + 零本地落盘）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_pinned_upload_follows_bound_window_and_rejects_after_window_loss() {
    let f = Fixture::new_with_gate(Switch::On, gate_arm()).await;
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "{view}");
    let token = view["bindingToken"].as_str().unwrap().to_string();
    let token_ref = &token;
    let upload = |app: axum::Router, name: &str, body: String| {
        let url = format!(
            "/m/api/v1/session-attachment?session_id=ses-1&name={name}&interaction_route=opencode_native"
        );
        async move {
            use tower::ServiceExt as _;
            let request = req("POST", &url, Some(COOKIE), Some(body));
            let (parts, body) = request.into_parts();
            let mut request = axum::http::Request::from_parts(parts, body);
            request
                .headers_mut()
                .insert("x-opencode-expected-binding", token_ref.parse().unwrap());
            let resp = app.oneshot(request).await.expect("上传请求");
            let status = resp.status();
            let text = body_of(resp).await;
            (
                status,
                serde_json::from_str::<serde_json::Value>(&text).unwrap_or(serde_json::json!({})),
            )
        }
    };
    // 正向：窗口浏览 B，上传经绑定窗口放行（受控落盘）。
    f.set_gate_opened("ses-2");
    let (status, reply) = upload(
        f.app.clone(),
        "pinned-upload.txt",
        "绑定窗口上传".to_string(),
    )
    .await;
    assert_eq!(status, 200, "绑定窗口期内上传放行：{reply}");
    let written = reply["path"].as_str().expect("落盘路径").to_string();
    assert_eq!(std::fs::read_to_string(&written).unwrap(), "绑定窗口上传");
    // 反向：绑定窗口退出 → 上传 fail-closed（与其他写入口同一拒绝语义）。
    f.set_gate_tui_alive(false);
    let (status, reply) = upload(
        f.app.clone(),
        "pinned-upload-rejected.txt",
        "窗口退出后的上传".to_string(),
    )
    .await;
    assert_eq!(status, 409, "绑定窗口退出后上传必须拒绝：{reply}");
    assert_eq!(
        reply["error"], "OC_GATE_PID_UNVERIFIED",
        "上传口错误码可辨（与动作口同族）：{reply}"
    );
    let rejected = f
        .dir
        .join(".tuvis-attachments")
        .join("ses-1")
        .join("pinned-upload-rejected.txt");
    assert!(!rejected.exists(), "拒绝路径零本地落盘");
}

// ============================================================
// N2 语义证据准入（attribution_evidence）：echo-challenge 机制的版本绑定
// （H02 结论速览 9）。判据本体 = 「HTTP schema 合规 ≠ 归属语义适用」：
// 两个运行样本契约层面不可区分，只有登记表内的构建组合才放行依赖该证明的新写。
// 断言口径沿用 H03 门用例：拒绝 = 零探针写 + 零用户动作行 + 零队列项（三零），
// 且原生只读、旧动作只读核对与本地安全操作照常。
// ============================================================

/// RED 判据本体：样本 A（H02 锚定构建）与样本 B（同版本不同构建）都符合 HTTP
/// 契约、测试回声均可返回合法字段（两个样本共用同一假 peer/契约/回声形态）。
/// 只给样本 A 配置适用的归属行为证据 → 样本 B 的新写必须在探针开始前拒绝（三零）；
/// 样本 A 照常放行且不受 B 拒绝影响。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_semantics_admission_rejects_unregistered_build_before_probe() {
    let f = Fixture::new_with_gate_sessions(Switch::On, gate_arm(), |dir| {
        vec![
            opencode_session("ses-1", dir),
            opencode_session("ses-2", dir),
        ]
    })
    .await;
    // 回声合法性：样本 B 与样本 A 共用同一假 peer（同端点、同契约、同 ack 形态），
    // 「契约合规且回声可合法返回」由样本 A 的放行轮在线证明；样本 B 不武装专属
    // ack 也不影响判定——它的探针轮必须被准入挡在开始之前（零探针写断言）。
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    // 样本 B：同版本不同构建（登记表只含 H02 锚定构建）。
    f.set_gate_binary_sha(GATE_UNPROVEN_PID, GATE_UNREGISTERED_SHA);

    // 样本 A（登记构建）：认领 + 发送 → 正向放行，写权链真实跑过探针。
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "样本 A 认领：{view}");
    let token_a = view["bindingToken"].as_str().expect("绑定令牌").to_string();
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_semantics_a", &token_a, &[]),
    )
    .await;
    assert_eq!(status, 200, "登记组合照常放行（准入只是第一道）：{reply}");
    assert_eq!(f.peer.session_prompt_writes("ses-1"), 1, "写恰落样本 A");
    let probes_after_a = f.peer.probe_writes();
    assert!(probes_after_a > 0, "样本 A 的写权链含真实探针轮");

    // 样本 B：观察切到另一扇窗口（另一构建）打开 ses-2；认领（只读面）照常。
    f.set_gate_opened_window("ses-2", GATE_UNPROVEN_PID, GATE_UNPROVEN_STARTED_AT_MS);
    let (status, view_b) = f.view("ses-2").await;
    assert_eq!(status, 200, "样本 B 原生只读照常（读路径不过门）：{view_b}");
    let token_b = view_b["bindingToken"]
        .as_str()
        .expect("绑定令牌")
        .to_string();
    // 旧动作核对照常：只读核对不依赖归属语义证据。
    f.seed_action("act_semantics_old", "ses-2", "acknowledged");
    let (status, _) = f.view("ses-2").await;
    assert_eq!(status, 200, "旧动作只读核对不受准入影响");

    // 样本 B 的新写：探针开始前拒绝（三零）。
    let writes_before = f.peer.writes();
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body_for("act_semantics_b", "ses-2", &token_b),
    )
    .await;
    assert_eq!(status, 409, "未登记构建的新写必须在探针开始前拒绝：{reply}");
    let body: serde_json::Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(
        body["reasonCode"], "OC_GATE_SEMANTICS_UNPROVEN",
        "稳定原因码可辨（schema 合规 ≠ 语义适用）：{reply}"
    );
    assert_eq!(
        f.peer.probe_writes(),
        probes_after_a,
        "零探针写（准入先于探针创建，武装了的回声也不许开跑）"
    );
    assert_eq!(f.action_rows("act_semantics_b"), 0, "零用户动作行");
    assert_eq!(f.peer.session_prompt_writes("ses-2"), 0, "零用户会话写");
    assert_eq!(f.peer.writes(), writes_before, "拒绝路径零新写");
    let outbox_before = gate_outbox_rows(&f);
    // 本地安全操作照常：暂停本地队列是本地操作，不过写权链。
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        serde_json::json!({
            "actionId": "act_semantics_pause",
            "sessionId": "ses-2",
            "bindingToken": token_b,
            "operation": {"kind": "set_outbox_paused", "paused": true, "expectedRevision": 0}
        })
        .to_string(),
    )
    .await;
    assert_eq!(status, 200, "本地安全操作不受准入拒绝影响：{reply}");
    assert_eq!(gate_outbox_rows(&f), outbox_before, "零新队列项");

    // 样本 A 不受样本 B 拒绝影响：准入按锚定窗口逐写判定，不搞全局封禁。
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_semantics_a2", &token_a, &[]),
    )
    .await;
    assert_eq!(status, 200, "登记组合照常放行：{reply}");
    assert_eq!(f.peer.session_prompt_writes("ses-1"), 2);
}

/// N2「缺 TUI 身份」：无新鲜锁定窗口（也无选择时绑定）→ 锚定进程不可证 →
/// 依赖语义证明的新写按不适用拒绝（fail-closed，先于探针）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_semantics_missing_tui_identity_rejects_before_probe() {
    let f = Fixture::new_with_gate(Switch::On, gate_arm()).await;
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    // 先清观察再认领：选择时刻无新鲜 Opened → 绑定捕获空操作（不虚设身份）。
    f.clear_gate_opened();
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "原生只读照常：{view}");
    let token = view["bindingToken"].as_str().expect("绑定令牌").to_string();
    let writes_before = f.peer.writes();
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_semantics_no_tui", &token, &[]),
    )
    .await;
    assert_eq!(status, 409, "缺 TUI 身份必须按不适用拒绝：{reply}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&reply).unwrap()["reasonCode"],
        "OC_GATE_SEMANTICS_UNPROVEN",
        "准入先于门内锁定窗口判据（可区分的稳定原因码）：{reply}"
    );
    assert_eq!(f.peer.probe_writes(), 0, "零探针写");
    assert_eq!(f.action_rows("act_semantics_no_tui"), 0, "零用户动作行");
    assert_eq!(f.peer.prompt_writes(), 0, "零用户会话写");
    assert_eq!(f.peer.writes(), writes_before, "拒绝路径零新写");
    // 原生只读照常。
    let (status, _) = f.view("ses-1").await;
    assert_eq!(status, 200, "只读核对仍可用");
}

/// N2「换代」两断言：①锚定 TUI 同 pid 新代际换未登记构建（升级模拟）→ 缓存键
/// 换代必须重算并拒绝；复验登记恢复同一组合 → 放行。②服务代际失配仍由**前置
/// 身份链**先行拒绝（服务身份证据先于语义准入，次序不倒转），两段都零探针写。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_semantics_generation_swap_rechecks_binary_and_service_identity_precedes() {
    let f = Fixture::new_with_gate(Switch::On, gate_arm()).await;
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    // 升级模拟先行：观察与进程事实同步换代（新窗口代际），二进制换成未登记构建。
    f.replace_gate_window_generation(GATE_STARTED_AT_MS + 60_000);
    f.set_gate_binary_sha(GATE_LOCKED_PID, GATE_UNREGISTERED_SHA);
    // 认领（捕获新代际绑定）→ 新写：准入锚定新代际窗口 → 未登记 → 探针前拒绝。
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "原生只读照常：{view}");
    let token = view["bindingToken"].as_str().expect("绑定令牌").to_string();
    let writes_before = f.peer.writes();
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_gen_unregistered", &token, &[]),
    )
    .await;
    assert_eq!(status, 409, "换代后未登记构建必须拒绝：{reply}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&reply).unwrap()["reasonCode"],
        "OC_GATE_SEMANTICS_UNPROVEN"
    );
    assert_eq!(f.peer.probe_writes(), 0, "零探针写");
    assert_eq!(f.action_rows("act_gen_unregistered"), 0, "零用户动作行");
    assert_eq!(f.peer.prompt_writes(), 0, "零用户会话写");
    assert_eq!(f.peer.writes(), writes_before, "拒绝路径零新写");
    // 复验通过并重新登记 → 恢复。恢复形态必须尊重「进程存活期二进制不变」不变量：
    // 存活进程的二进制不会中途变化（SHA 每代际缓存正是在该不变量下成立），恢复 =
    // 新进程代际跑锚定构建；换代后旧代际绑定失效，从历史 draining 状态显式重选以
    // 恢复绑定，重选轮本身过准入与门。
    f.replace_gate_window_generation(GATE_STARTED_AT_MS + 120_000);
    f.reset_gate_binary_sha(GATE_LOCKED_PID);
    seed_historical_draining(&f);
    let (status, text) = post(
        &f.app,
        "/m/api/v1/opencode/route/native",
        COOKIE,
        f.native_route_request("ses-1", &token, false).await,
    )
    .await;
    assert_eq!(status, 200, "显式重选恢复路由（重选轮过准入与门）：{text}");
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_gen_registered", &token, &[]),
    )
    .await;
    assert_eq!(status, 200, "登记恢复后放行：{reply}");
    assert_eq!(f.peer.prompt_writes(), 1);
    // 服务代际失配：前置身份链先拒绝（OC_GENERATION_MISMATCH，不是准入码）——
    // 服务身份证据仍先于语义准入，次序不倒转。
    let probes_before = f.peer.probe_writes();
    f.peer.set_info_pid(9999);
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_gen_service_swap", &token, &[]),
    )
    .await;
    assert_eq!(status, 409, "服务代际失配必须拒绝：{reply}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&reply).unwrap()["reasonCode"],
        "OC_GENERATION_MISMATCH",
        "前置身份链先于语义准入：{reply}"
    );
    assert_eq!(f.peer.probe_writes(), probes_before, "零探针写");
    assert_eq!(f.action_rows("act_gen_service_swap"), 0, "零用户动作行");
}

/// N2「错误形态」：构建 SHA 获取失败（解析 None）→ 按不适用 fail-closed 拒绝
///（先于探针、三零）；失败不入缓存——解析恢复后同代际立即重算并放行。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_semantics_sha_resolution_failure_is_fail_closed() {
    let f = Fixture::new_with_gate(Switch::On, gate_arm()).await;
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "原生只读照常：{view}");
    let token = view["bindingToken"].as_str().expect("绑定令牌").to_string();
    // SHA 解析失败（取不到构建身份 → 无法证明语义适用）。
    f.fail_gate_binary_sha(GATE_LOCKED_PID);
    let writes_before = f.peer.writes();
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_semantics_sha_fail", &token, &[]),
    )
    .await;
    assert_eq!(status, 409, "SHA 获取失败必须按不适用拒绝：{reply}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&reply).unwrap()["reasonCode"],
        "OC_GATE_SEMANTICS_UNPROVEN"
    );
    assert_eq!(f.peer.probe_writes(), 0, "零探针写");
    assert_eq!(f.action_rows("act_semantics_sha_fail"), 0, "零用户动作行");
    assert_eq!(f.peer.prompt_writes(), 0, "零用户会话写");
    assert_eq!(f.peer.writes(), writes_before, "拒绝路径零新写");
    // 失败不入缓存：解析恢复（可执行路径可读）后同代际重算 → 放行。
    f.reset_gate_binary_sha(GATE_LOCKED_PID);
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_semantics_sha_ok", &token, &[]),
    )
    .await;
    assert_eq!(status, 200, "解析恢复后同代际重算并放行：{reply}");
    assert_eq!(f.peer.prompt_writes(), 1);
}

// ============================================================
// M2 语义证据准入补全（复审 P1「N2 归属准入只验了一半」）：
// ①登记组合 =（TUI 构建 SHA, 服务构建 SHA, 平台, 启动形态）四元组——归属语义
//   （数据层 SSE 失效刷新）由 TUI 与服务共同表现，只验 TUI 构建放过了「只换
//   服务构建」的组合；②同轮单一窗口身份——准入与回声门共用一次解析的锁定
//   窗口，消除无固定绑定时两次解析之间换窗的「准入验 A、回声锁 B」竞态。
// ============================================================

/// M2 RED ①「只替换服务构建」：TUI 构建是登记锚、服务构建换成未登记构建（同一
/// 假 peer 同一契约，只有 descriptor.runtime 锚定的服务进程构建不同）→ 该组合的
/// 写必须在探针开始前拒绝（三零）。当前实现只查 TUI SHA 会放行（RED）。
///
/// 阶段遵守 SHA 每代际缓存语义（N2 先例：同代际只算一次、只缓存成功值——「存活
/// 进程盘中换二进制」不是可测形态）：
/// ①只换服务构建（RED 本体）→ ②恢复 = 复验重新登记 + **服务换代**跑登记构建
///（换代后旧绑定失效，走 N1 合法途径交回 + 显式重选，N2 换代用例同形）→
/// ③新代际上服务 SHA 解析失败（fail-closed；重选候选核验同被拒）→ ④双换拒。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_semantics_service_build_swap_rejects_before_probe() {
    let f = Fixture::new_with_gate(Switch::On, gate_arm()).await;
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    // 只换服务构建：TUI 保持登记锚（缺省），服务进程（登记文件 pid = PEER_PID）
    // 的构建换成未登记构建。
    f.set_gate_service_sha(PEER_PID, GATE_UNREGISTERED_SHA);
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "原生只读照常（读路径不过准入）：{view}");
    let token = view["bindingToken"].as_str().expect("绑定令牌").to_string();
    let writes_before = f.peer.writes();
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_m2_svc_swap", &token, &[]),
    )
    .await;
    assert_eq!(status, 409, "只换服务构建的组合必须在探针前拒绝：{reply}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&reply).unwrap()["reasonCode"],
        "OC_GATE_SEMANTICS_UNPROVEN",
        "登记组合 =（TUI SHA, 服务 SHA, 平台, 形态）：服务维度未登记即不适用：{reply}"
    );
    assert_eq!(f.peer.probe_writes(), 0, "零探针写（准入先于探针创建）");
    assert_eq!(f.action_rows("act_m2_svc_swap"), 0, "零用户动作行");
    assert_eq!(f.peer.prompt_writes(), 0, "零用户会话写");
    assert_eq!(f.peer.writes(), writes_before, "拒绝路径零新写");

    // ②恢复 = 复验重新登记（新服务代际跑登记构建；SHA 缓存键换代 → 重算）。
    // 初始未登记构建只能读；服务换代后显式选择登记构建。
    let generation_2 = PEER_STARTED_AT_MS + 60_000;
    f.set_service_generation(generation_2);
    f.reset_gate_service_sha(PEER_PID);
    // 服务换代后的显式重选（机制语义，既有行为非 M2 引入）：候选核验的写权链经
    // `source_for_session` **中途装新代际 source**（重选读取在册 source 时刻与
    // claim 时刻之间），claim 的 installed_ok 守卫对首次尝试保守拒绝
    //（`OC_BINDING_STALE`）；第二次尝试读取时在册者已是新代际 source（指针同源）
    // → 自愈放行。按此语义断言：首次保守拒绝不是缺陷，只允许这一个拒绝面。
    let (mut status, mut text) = post(
        &f.app,
        "/m/api/v1/opencode/route/native",
        COOKIE,
        f.native_route_request("ses-1", &token, false).await,
    )
    .await;
    if status == 409 {
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&text).unwrap()["reasonCode"],
            "OC_BINDING_STALE",
            "换代后首次重选只允许 installed_ok 守卫的保守拒绝：{text}"
        );
        let (retry_status, retry_text) = post(
            &f.app,
            "/m/api/v1/opencode/route/native",
            COOKIE,
            f.native_route_request("ses-1", &token, false).await,
        )
        .await;
        status = retry_status;
        text = retry_text;
    }
    assert_eq!(
        status, 200,
        "显式重选恢复路由（重选轮过双构建准入与门）：{text}"
    );
    // 服务换代改变路由目标（RuntimeKey 含服务代际）→ 重选后绑定令牌随新投影刷新；
    // 旧令牌对旧目标签发，发送会被投影令牌核对拒绝（与写权链无关，取新令牌即可）。
    let (status, view) = f.view("ses-1").await;
    assert_eq!(
        status, 200,
        "重选后重取投影（新代际目标的新绑定令牌）：{view}"
    );
    let token = view["bindingToken"]
        .as_str()
        .expect("新代际绑定令牌")
        .to_string();
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_m2_svc_ok", &token, &[]),
    )
    .await;
    assert_eq!(status, 200, "（TUI+服务）双合规组合照常放行：{reply}");
    assert_eq!(f.peer.prompt_writes(), 1);
    let probes_after_ok = f.peer.probe_writes();
    assert!(probes_after_ok > 0, "放行轮含真实探针挑战");

    // ③新代际上服务 SHA 解析失败（取不到服务构建身份 → 按不适用 fail-closed）：
    // 拒绝面覆盖到显式重选（重选候选核验同跑双构建准入）——未证组合连重选也被拒。
    let generation_3 = generation_2 + 60_000;
    f.set_service_generation(generation_3);
    f.fail_gate_service_sha(PEER_PID);
    let writes_before = f.peer.writes();
    seed_historical_draining(&f);
    let (status, text) = post(
        &f.app,
        "/m/api/v1/opencode/route/native",
        COOKIE,
        f.native_route_request("ses-1", &token, false).await,
    )
    .await;
    assert_eq!(status, 409, "服务 SHA 取不到时重选候选核验必须拒绝：{text}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&text).unwrap()["reasonCode"],
        "OC_GATE_SEMANTICS_UNPROVEN"
    );
    assert_eq!(
        f.peer.probe_writes(),
        probes_after_ok,
        "零探针写（准入先于探针创建）"
    );
    assert_eq!(f.peer.prompt_writes(), 1, "零用户会话写");
    assert_eq!(f.peer.writes(), writes_before, "拒绝路径零新写");

    // ④双换拒：TUI 与服务构建都未登记（双双换代，缓存键双双换代重算）→ 同一准入
    // 拒绝，重选同样被挡。
    let generation_4 = generation_3 + 60_000;
    f.set_service_generation(generation_4);
    f.set_gate_service_sha(PEER_PID, GATE_UNREGISTERED_SHA);
    f.set_gate_binary_sha(GATE_LOCKED_PID, GATE_UNREGISTERED_SHA);
    f.replace_gate_window_generation(GATE_STARTED_AT_MS + 60_000);
    let writes_before = f.peer.writes();
    // 上一轮显式确认已拒绝，历史路由仍处于 draining。
    let (status, text) = post(
        &f.app,
        "/m/api/v1/opencode/route/native",
        COOKIE,
        f.native_route_request("ses-1", &token, false).await,
    )
    .await;
    assert_eq!(status, 409, "双换组合必须拒绝（含重选候选核验）：{text}");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&text).unwrap()["reasonCode"],
        "OC_GATE_SEMANTICS_UNPROVEN"
    );
    assert_eq!(f.peer.probe_writes(), probes_after_ok, "零新探针写");
    assert_eq!(f.peer.prompt_writes(), 1, "零新用户会话写");
    assert_eq!(f.peer.writes(), writes_before, "拒绝路径零新写");
}

/// M2「等待期间换窗」单一身份（F12 架构形）：认领（view）时把新鲜观察的窗口身份
/// A（登记构建）随路由 CAS 同事务写进绑定行；写权链的锁定窗口**只来自绑定行**
/// （单次解析，准入与回声门共用），认领之后、写之前的窗口世界变化（另一扇窗口 B
/// 接管同一会话）既不影响准入也不影响回声锁定的窗口。本用例只武装 B 的 ack——
/// 绑定行锁定的 A 收到 B 的 ack = pid 比对拒绝（探针轮内 fail-closed）。
///
/// 历史（F12 前）：无固定绑定时准入与回声门分两次解析锁定窗口（回退新鲜查询），
/// 两次解析之间换窗会让「准入所验身份」与「实际回声/授权身份」不一致（RED②：
/// 准入验 A、回声锁到 B 并由 B 应答 → 放行）。F12 起 view 认领与绑定行同事务落库
/// + 写权链绑定行单次解析，双选窗面结构性消失；「路由 active 而绑定行缺失/不匹配」
/// 的形态由项 1.5 写前 fail-closed 承接（见
/// `background_dispatch_without_window_record_fails_closed_before_any_echo`）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gate_semantics_window_swap_between_admission_and_echo_is_single_identity() {
    let f = Fixture::new_with_gate(Switch::On, gate_arm()).await;
    // 只武装窗口 B 的 ack：绑定行锁定的 A 收到 B 的 ack → OC_GATE_PID_MISMATCH
    //（E4：pid 比对是唯一 TUI 身份防线）。
    f.peer.arm_gate_ack(GATE_UNPROVEN_PID, None);
    // 认领时刻窗口世界 = A（夹具默认新鲜观察，登记构建）：F12 起 view 认领与绑定行
    // 同事务落库，A 的窗口身份进绑定行。
    let (status, view) = f.view("ses-1").await;
    assert_eq!(status, 200, "原生只读照常：{view}");
    let token = view["bindingToken"].as_str().expect("绑定令牌").to_string();
    // 认领之后、写之前：窗口世界切到 B（另一扇窗口接管同一会话——M2 威胁形态）。
    f.set_gate_opened_window("ses-1", GATE_UNPROVEN_PID, GATE_UNPROVEN_STARTED_AT_MS);
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_m2_swap", &token, &[]),
    )
    .await;
    assert_eq!(
        status, 409,
        "准入与回声必须共用同一窗口身份；换窗后的世界由门内判据拒绝：{reply}"
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&reply).unwrap()["reasonCode"],
        "OC_GATE_PID_MISMATCH",
        "同轮单一身份：绑定行锁定的 A 收到 B 的 ack → pid 比对拒绝（探针轮内 fail-closed）：{reply}"
    );
    // 单一身份等价取证：世界已切到 B，而拒绝来自「绑定行 A vs B 的 ack」的 pid 比对
    //——世界变化没有让写权链改锁 B（锁定窗口只来自绑定行，准入与回声共用同一身份）。
    assert_eq!(
        f.gate_opened_window(),
        Some(GateOpenedWindow {
            sid: "ses-1".to_string(),
            pid: GATE_UNPROVEN_PID,
            started_at_ms: GATE_UNPROVEN_STARTED_AT_MS,
        }),
        "窗口世界已切到 B（换窗发生在认领与写之间）"
    );
    assert_eq!(f.peer.prompt_writes(), 0, "零用户会话写");
    assert_eq!(f.peer.session_prompt_writes("ses-1"), 0, "零该会话用户写");
    assert_eq!(f.action_rows("act_m2_swap"), 0, "预留前拒绝零动作行");
    // 探针轮内 fail-closed（brief 指定的可接受形态）：唯一写增量 = 探针挑战本身
    //（建探针 POST + challenge 覆写 PATCH = 2），零用户会话写、零动作行；
    // `writes()` 计入探针通道，此处按「只有探针写」断言而非恒零。
    assert_eq!(
        f.peer.probe_writes(),
        2,
        "拒绝发生在探针轮内：写增量只有探针挑战（2），无其他写"
    );

    // 正控：武装 A 的 ack → 新 intent 照常放行（世界仍停在 B：放行来自「绑定行 A
    // + A 的 ack」的身份匹配——证明拒绝来自身份保护而非世界切换本身，夹具未坏）。
    f.peer.arm_gate_ack(GATE_LOCKED_PID, None);
    let (status, reply) = post(
        &f.app,
        "/m/api/v1/opencode/actions",
        COOKIE,
        gate_send_body("act_m2_swap_ok", &token, &[]),
    )
    .await;
    assert_eq!(status, 200, "正控：A 的 ack 在场时照常放行：{reply}");
    assert_eq!(f.peer.prompt_writes(), 1);
}

// Task 6：旧队列只读元数据；无法核验原代际的行不提供处置动作。
const LEGACY_OUTBOX_URI: &str = "/m/api/v1/opencode/legacy-outbox?session_id=ses-1";

#[tokio::test]
async fn legacy_outbox_lists_only_own_frozen_pending_metadata_without_side_effects() {
    let f = Fixture::single(Switch::On).await;
    let item = f.seed_pending_row("ses-1", "private-legacy-message");
    let other_session = f.seed_pending_row("ses-2", "private-other-session");
    let other_rows = f.state.store.with(|c| {
        [
            ("opencode", "dev-2", None),
            ("claude", "dev-1", None),
            ("opencode", "dev-1", Some(2000_i64)),
        ]
        .map(|(agent, device, sent_at)| {
            c.execute(
                "INSERT INTO inject_queue (session_id, agent_type, device_id, device_name, \
                     content, enqueued_at, sent_at) VALUES ('ses-1', ?1, ?2, 'private-name', \
                     'private-other-message', 1000, ?3)",
                rusqlite::params![agent, device, sent_at],
            )
            .unwrap();
            c.last_insert_rowid()
        })
    });
    let ids = [
        item,
        other_session,
        other_rows[0],
        other_rows[1],
        other_rows[2],
    ];
    let before = ids.map(|id| legacy_queue_row(&f, id));

    let (status, body) = f.get(LEGACY_OUTBOX_URI).await;

    assert_eq!(ids.map(|id| legacy_queue_row(&f, id)), before);
    assert_no_legacy_write_side_effects(&f);
    assert_eq!(status, 200, "已配对设备可读取自身冻结项：{body}");
    assert_eq!(
        body,
        serde_json::json!({
            "count": 1,
            "items": [{
                "id": item,
                "enqueuedAt": 1000,
                "disposition": "blocked_unverified_generation"
            }]
        }),
        "仅返回同设备、同会话的 OpenCode 未决项元数据"
    );
}

#[tokio::test]
async fn legacy_outbox_reports_storage_read_failure_instead_of_empty_success() {
    let f = Fixture::single(Switch::On).await;
    f.state.store.with(|c| {
        c.execute_batch("ALTER TABLE inject_queue RENAME TO inject_queue_unavailable")
            .unwrap();
    });

    let (status, body) = f.get(LEGACY_OUTBOX_URI).await;

    assert_no_legacy_write_side_effects(&f);
    assert!(
        status.is_server_error(),
        "存储读失败必须显式报错：{status} {body}"
    );
}
