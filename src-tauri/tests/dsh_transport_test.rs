//! T07：官方 HTTP/Mux 协议与连接生命周期（brief 指定四用例 + 补充覆盖）。
//!
//! 全部走真实生产入口 [`transport::connect`]，对**本地 127.0.0.1 临时端口的
//! axum 假宿主**（同一 authority 三通道：`/connect`、`POST /api/*`、
//! `GET /api/remote.mux` WS——与真实 DSH 宿主同形）；「系统代理」用另一端口的
//! 裸 TCP 监听代表，客户端若碰它即计数。不连真实 DSH、不出环回。
//!
//! 用例（brief 断言规范 + 逐项行为核验）：
//! 1. `cookie_attaches_to_owned_authority_only` —— cookie 只发钉定 authority；
//!    重定向试图跳出 → 请求失败；`external_redirect_hits == 0`（代理与外部宿主零接触）；
//! 2. `unary_rejects_stream_signature` —— 流式方法走一元通道的 typed failure
//!    原样上抛不误读；mux 帧形态落在一元通道 → 客户端侧拒绝；
//! 3. `old_generation_frame_is_ignored` —— 换代后旧代际晚帧被丢弃，
//!    `mirror_after_late.epoch == current_epoch`；
//! 4. `mux_disconnect_invalidates_peer` —— mux 断开即失效：
//!    `!connection.write_available()`，后续写立即拒绝而非排队；
//! 5. 401/409 终态不重连；6. 写超时 = UnknownDelivery 语义且不重投；
//! 7. >2MiB 帧有界拒绝；8. ready/clientId 上浮；9. close/drop 无遗留任务；
//! 10. 二进制响应按 content-type 分流不硬解；11. 退避上限 10 秒 + 默认限值。
//!
//! 观测纪律：断言变量全部来自真实生产入口返回或测试边界（假宿主计数、
//! 代理假体连接计数、镜像快照），不以实现内部状态复制期望。

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::ws::{WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

use multi_agents_manager_lib::services::dsh_control::actions::{
    NativeDispatch, NativeSendOutcome, PeerChannel,
};
use multi_agents_manager_lib::services::dsh_control::error::codes;
use multi_agents_manager_lib::services::dsh_control::model::{AuthorizedPeer, NativeResult};
use multi_agents_manager_lib::services::dsh_control::transport::{
    self, TransportConfig, UnaryReply,
};

// ============================================================
// 可编程假宿主（axum；一 authority 三通道，与真实 DSH 宿主同形）
// ============================================================

/// 一元通道行为（逐测试注入）。
#[derive(Clone)]
enum UnaryBehavior {
    /// 回 `server-response` ok:true（value 带 method 回显）。
    Envelope,
    /// 成功信封缺少受理字段。
    AcceptedMissing,
    /// 成功信封明确拒绝受理。
    AcceptedFalse,
    /// 成功信封的受理字段类型错误。
    AcceptedMistyped,
    /// 回 typed 失败（`ok:false` + 原生 code/message）。
    TypedFailure {
        code: &'static str,
        message: &'static str,
    },
    /// 回 mux 帧形态（流签名落在错通道的客户端侧考验）。
    MuxShapedBody,
    /// 回 302 → Location（重定向拦截考验）。
    Redirect(String),
    /// 收下请求但长时不应答（写超时考验）。
    Hang,
    /// 回 application/octet-stream 二进制（content-type 分流考验）。
    Binary,
    /// 无 Content-Length 的流式大响应（R11：读取时累计限额，不先聚合整包）。
    ///
    /// 确定性闸：推到 `park_after` 字节后**停在原地**（等 `release`），客户端必须
    /// 在这之前就报超限——所以「是否收满整包」不靠内核缓冲的运气判断。
    NoLengthStream {
        total_bytes: usize,
        chunk_bytes: usize,
        park_after: usize,
        written: Arc<AtomicU32>,
        release: Arc<tokio::sync::Notify>,
    },
    /// 无 Content-Length 的小响应（合法 JSON 信封；正常路径不得被限流误伤）。
    NoLengthEnvelope,
    /// 回 200 + 非法 JSON 体（P1-2：发送后响应不可解读）。
    GarbledJson,
    /// 回 200 + 合法信封但 rpcId 不匹配（P1-2：发送后身份闸失败）。
    RpcMismatch,
    /// 回 200 + 声明超限的固定体（P1-2：发送后响应超限）。
    DeclaredOversize,
    /// 回 401（P1-2：宿主闸门拒绝——方法未执行）。
    Unauthorized,
    /// 回 500（M6-f：处理器可能已运行——保守未知）。
    InternalError,
    /// 请求已到达后中断响应体，模拟写出后的连接断开。
    DisconnectResponse,
}

/// WS 会话在收到 open 后依序执行的步骤。
#[derive(Clone)]
enum WsStep {
    /// 回 item 帧（value 原样）。
    Item(Value),
    /// 回 end 帧（终结该流）。
    End,
    /// 回 error 帧。
    Error { code: String, message: String },
    /// 回 >2MiB 的 item 帧（超限考验）。
    OversizeItem,
    /// 在此步等待测试放行（rail 信号；晚帧时序的确定性闸）。
    HoldRail(Arc<WsRail>),
    /// 服务端主动关闭 socket（断链考验）。
    CloseSocket,
}

struct WsRail {
    entered: Semaphore,
    release: Semaphore,
}

impl WsRail {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
        })
    }

    async fn hold(&self) {
        self.entered.add_permits(1);
        // 许可随此步骤结束归还，一次放行也适用于后续订阅。
        let _permit = self.release.acquire().await.expect("放行闸仍开启");
    }

    fn release(&self) {
        self.release.add_permits(1);
    }

    async fn wait_for_entry(&self) {
        tokio::time::timeout(Duration::from_secs(10), self.entered.acquire())
            .await
            .expect("follow 订阅必须到达宿主")
            .expect("观察信号量仍开启")
            .forget();
    }
}

struct FakeHostState {
    /// 已签发的 cookie（`name=value` 全串；测试用假值，非真实凭据）。
    cookie: Mutex<Option<String>>,
    /// 请求观测到的 cookie 头（原样；只在钉定 authority 上应有值）。
    cookies_seen: Mutex<Vec<String>>,
    /// 到达本宿主的任意 HTTP 命中（含 WS 升级尝试；外部命中数观测面对偶）。
    http_hits: AtomicU32,
    /// /connect 命中数。
    connect_hits: AtomicU32,
    /// WS 升级尝试数（每次 connect 恰 +1 → 无内部重试的观测面）。
    ws_attempts: AtomicU32,
    /// 存活 WS 会话数（409 判定 + socket 关闭观测）。
    ws_live: AtomicU32,
    /// 一元请求命中数（session/prompt 形状回归的观测面）。
    unary_hits: AtomicU32,
    /// 已捕获的一元请求信封（parse 失败的请求体不入表）。
    unary_bodies: Mutex<Vec<Value>>,
    /// socket 关闭即移除 Peer → 该 cookie 的后续 HTTP/升级一律 401（协议事实）。
    peer_invalidated: AtomicBool,
    unary: Mutex<UnaryBehavior>,
    ws_steps: Mutex<Vec<WsStep>>,
    /// 可选的首个订阅脚本；后续订阅仍按 ws_steps 回帧。
    first_ws_steps: Mutex<Option<Vec<WsStep>>>,
    /// M6-e：服务端主动断开归属 socket（notify_one 存许可，先杀后建连不丢）。
    kill_ws: Arc<tokio::sync::Notify>,
}

type Shared = Arc<FakeHostState>;

impl FakeHostState {
    /// 签发（或取回）假 cookie；测试先签发再据此构造 peer。
    fn issue_cookie(&self) -> String {
        let mut guard = self.cookie.lock().unwrap();
        guard
            .get_or_insert_with(|| format!("dsh-auth-testfake={:016x}", rand::random::<u64>()))
            .clone()
    }

    fn cookies_seen(&self) -> Vec<String> {
        self.cookies_seen.lock().unwrap().clone()
    }

    fn set_unary(&self, behavior: UnaryBehavior) {
        *self.unary.lock().unwrap() = behavior;
    }

    fn set_ws_steps(&self, steps: Vec<WsStep>) {
        *self.ws_steps.lock().unwrap() = steps;
    }

    fn set_first_ws_steps(&self, steps: Vec<WsStep>) {
        *self.first_ws_steps.lock().unwrap() = Some(steps);
    }

    fn http_hits(&self) -> u32 {
        self.http_hits.load(Ordering::SeqCst)
    }

    fn ws_attempts(&self) -> u32 {
        self.ws_attempts.load(Ordering::SeqCst)
    }

    fn unary_hits(&self) -> u32 {
        self.unary_hits.load(Ordering::SeqCst)
    }

    /// 取回已捕获的一元请求信封（parse 失败的请求体不入表）。
    fn unary_bodies(&self) -> Vec<Value> {
        self.unary_bodies.lock().unwrap().clone()
    }

    fn live_ws(&self) -> u32 {
        self.ws_live.load(Ordering::SeqCst)
    }

    /// 服务端主动断开当前归属 socket（会话结束 → Peer 失效 → cookie 终态）。
    fn kill_ws(&self) {
        self.kill_ws.notify_one();
    }

    /// 轮询等服务端观测到 socket 关闭（ws_live 归零）。
    async fn wait_until_no_live_ws(&self, budget: Duration) -> bool {
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            if self.live_ws() == 0 {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        self.live_ws() == 0
    }
}

/// 起一台假宿主（127.0.0.1 临时端口）。
async fn spawn_fake_host() -> (String, Shared) {
    let state: Shared = Arc::new(FakeHostState {
        cookie: Mutex::new(None),
        cookies_seen: Mutex::new(Vec::new()),
        http_hits: AtomicU32::new(0),
        connect_hits: AtomicU32::new(0),
        ws_attempts: AtomicU32::new(0),
        ws_live: AtomicU32::new(0),
        unary_hits: AtomicU32::new(0),
        unary_bodies: Mutex::new(Vec::new()),
        peer_invalidated: AtomicBool::new(false),
        unary: Mutex::new(UnaryBehavior::Envelope),
        ws_steps: Mutex::new(Vec::new()),
        first_ws_steps: Mutex::new(None),
        kill_ws: Arc::new(tokio::sync::Notify::new()),
    });
    let app = Router::new()
        .route("/connect", get(connect_route))
        .route("/api/{*path}", post(unary_route).get(mux_route))
        .with_state(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let port = listener.local_addr().expect("local addr").port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve fake host");
    });
    (format!("127.0.0.1:{port}"), state)
}

/// `/connect`：token 缺失/不符 → 401；interactions 非法 → 400；
/// 合法 → 302 + `location: ./api/remote.mux` + 签名 API cookie（E2 实测形态）。
async fn connect_route(Query(params): Query<HashOfString>, State(st): State<Shared>) -> Response {
    st.connect_hits.fetch_add(1, Ordering::SeqCst);
    st.http_hits.fetch_add(1, Ordering::SeqCst);
    let token_ok = params.get("token").map(|t| t.len() >= 8).unwrap_or(false);
    if !token_ok {
        return (StatusCode::UNAUTHORIZED, "fake host: token refused").into_response();
    }
    match params.get("interactions").map(String::as_str) {
        Some("answer") | Some("none") | None => {}
        Some(_) => {
            return (
                StatusCode::BAD_REQUEST,
                "interactions must be answer or none",
            )
                .into_response()
        }
    }
    let cookie = st.issue_cookie();
    Response::builder()
        .status(StatusCode::FOUND)
        .header(header::LOCATION, "./api/remote.mux")
        .header(
            header::SET_COOKIE,
            format!("{cookie}; Path=/; HttpOnly; SameSite=Strict"),
        )
        .body(axum::body::Body::empty())
        .expect("static /connect response")
}

type HashOfString = std::collections::HashMap<String, String>;

/// `POST /api/{ns}/{method}`：记录 cookie 头 + 按注入行为应答。
async fn unary_route(
    Path(_path): Path<String>,
    headers: HeaderMap,
    State(st): State<Shared>,
    body: axum::body::Bytes,
) -> Response {
    st.http_hits.fetch_add(1, Ordering::SeqCst);
    st.unary_hits.fetch_add(1, Ordering::SeqCst);
    if let Ok(text) = std::str::from_utf8(&body) {
        let _ = text; // 请求体不落日志（假宿主纪律与生产一致）
    }
    if let Some(cookie) = headers.get(header::COOKIE).and_then(|v| v.to_str().ok()) {
        st.cookies_seen.lock().unwrap().push(cookie.to_string());
    }
    if st.peer_invalidated.load(Ordering::SeqCst) {
        // 协议事实：socket 关闭移除 Peer → 该 cookie 的 HTTP 被拒
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let behavior = st.unary.lock().unwrap().clone();
    let envelope: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    // 载荷形状回归观测面：可解析信封入表（仅测试内存，不落日志）。
    if !envelope.is_null() {
        st.unary_bodies.lock().unwrap().push(envelope.clone());
    }
    let rpc_id = envelope
        .get("rpcId")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    match behavior {
        UnaryBehavior::Envelope
        | UnaryBehavior::AcceptedMissing
        | UnaryBehavior::AcceptedFalse
        | UnaryBehavior::AcceptedMistyped => {
            let value = match behavior {
                UnaryBehavior::Envelope => json!({"method": "fake", "accepted": true}),
                UnaryBehavior::AcceptedMissing => json!({"method": "fake"}),
                UnaryBehavior::AcceptedFalse => json!({"accepted": false}),
                UnaryBehavior::AcceptedMistyped => json!({"accepted": "yes"}),
                _ => unreachable!(),
            };
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                json!({"type": "server-response", "rpcId": rpc_id, "result": {"ok": true, "value": value}}).to_string(),
            ).into_response()
        }
        UnaryBehavior::TypedFailure { code, message } => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            json!({
                "type": "server-response",
                "rpcId": rpc_id,
                "result": {"ok": false, "error": {"code": code, "message": message}}
            })
            .to_string(),
        )
            .into_response(),
        UnaryBehavior::MuxShapedBody => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            json!({"type": "item", "streamId": "s-x", "value": {"sneaky": true}}).to_string(),
        )
            .into_response(),
        UnaryBehavior::Redirect(location) => (
            StatusCode::FOUND,
            [(header::LOCATION, location.as_str())],
            String::new(),
        )
            .into_response(),
        UnaryBehavior::DisconnectResponse => {
            let stream = futures::stream::once(async {
                Err::<axum::body::Bytes, std::io::Error>(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "fixture disconnect",
                ))
            });
            Response::new(axum::body::Body::from_stream(stream))
        }
        UnaryBehavior::Hang => {
            tokio::time::sleep(Duration::from_secs(30)).await;
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                json!({"type": "server-response", "rpcId": rpc_id, "result": {"ok": true, "value": null}})
                    .to_string(),
            )
                .into_response()
        }
        UnaryBehavior::Binary => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/octet-stream")],
            b"\x00\x01binary-payload".to_vec(),
        )
            .into_response(),
        // R11：无 Content-Length（axum 对 stream body 走 chunked）——上限必须在
        // **读取过程中**累计生效，不得先聚合整包。
        UnaryBehavior::NoLengthStream {
            total_bytes,
            chunk_bytes,
            park_after,
            written,
            release,
        } => {
            let stream = futures::stream::unfold(0usize, move |sent| {
                let written = written.clone();
                let release = release.clone();
                async move {
                    if sent >= total_bytes {
                        return None;
                    }
                    if sent >= park_after {
                        // 停在原地：客户端必须在此前就拒绝（不依赖内核缓冲大小）。
                        release.notified().await;
                        return None;
                    }
                    let n = chunk_bytes.min(total_bytes - sent);
                    written.fetch_add(n as u32, Ordering::SeqCst);
                    Some((
                        Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(vec![b'x'; n])),
                        sent + n,
                    ))
                }
            });
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from_stream(stream))
                .expect("stream body")
        }
        // 同样无 Content-Length，但体量正常：限额逻辑不得误伤小响应。
        UnaryBehavior::NoLengthEnvelope => {
            let payload = json!({
                "type": "server-response",
                "rpcId": rpc_id,
                "result": {"ok": true, "value": {"method": "fake", "accepted": true}}
            })
            .to_string();
            let stream = futures::stream::once(async move {
                Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(payload))
            });
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from_stream(stream))
                .expect("stream body")
        }
        // P1-2：发送后响应不可解读（请求已到达宿主——结局未知）。
        UnaryBehavior::GarbledJson => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            "not-json{garbled".to_string(),
        )
            .into_response(),
        UnaryBehavior::RpcMismatch => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            json!({
                "type": "server-response",
                "rpcId": "someone-else",
                "result": {"ok": true, "value": null}
            })
            .to_string(),
        )
            .into_response(),
        UnaryBehavior::DeclaredOversize => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            vec![b'x'; transport::MAX_JSON_FRAME_BYTES + 4096],
        )
            .into_response(),
        // P1-2：宿主闸门拒绝（方法未执行——明确失败）。
        UnaryBehavior::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized").into_response(),
        UnaryBehavior::InternalError => (StatusCode::INTERNAL_SERVER_ERROR, "boom").into_response(),
    }
}

/// `GET /api/remote.mux`：WS 升级（cookie 校验 → 401；已有会话 → 409；
/// Peer 失效 → 401——与真实宿主的栅栏顺序一致：先信任面后认证面）。
async fn mux_route(ws: WebSocketUpgrade, headers: HeaderMap, State(st): State<Shared>) -> Response {
    st.http_hits.fetch_add(1, Ordering::SeqCst);
    st.ws_attempts.fetch_add(1, Ordering::SeqCst);
    let got = headers.get(header::COOKIE).and_then(|v| v.to_str().ok());
    let expected = st.cookie.lock().unwrap().clone();
    if st.peer_invalidated.load(Ordering::SeqCst) {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::CONNECTION, "close")],
            "unauthorized",
        )
            .into_response();
    }
    if got != expected.as_deref() {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    if st.ws_live.load(Ordering::SeqCst) > 0 {
        return (StatusCode::CONFLICT, "conflict").into_response();
    }
    st.ws_live.fetch_add(1, Ordering::SeqCst);
    ws.on_upgrade(move |sock| handle_socket(sock, st))
}

/// WS 会话：收到 open 后按注入步骤应答；会话结束即宣告 Peer 失效。
async fn handle_socket(mut sock: WebSocket, st: Shared) {
    // M6-e：kill_ws 一响即服务端断开（走既有会话结束路径：Peer 失效 + cookie 终态）。
    loop {
        let next = tokio::select! {
            _ = st.kill_ws.notified() => None,
            m = sock.recv() => m,
        };
        let Some(Ok(msg)) = next else { break };
        let text = match msg.into_text() {
            Ok(text) => text,
            Err(_) => continue,
        };
        let frame: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if frame.get("type").and_then(Value::as_str) != Some("open") {
            continue;
        }
        let stream_id = frame
            .get("streamId")
            .and_then(Value::as_str)
            .unwrap_or("s-unknown")
            .to_string();
        let steps = st
            .first_ws_steps
            .lock()
            .unwrap()
            .take()
            .unwrap_or_else(|| st.ws_steps.lock().unwrap().clone());
        let mut session_ended = false;
        for step in &steps {
            let outbound = match step {
                WsStep::Item(value) => {
                    Some(json!({"type": "item", "streamId": stream_id, "value": value}))
                }
                WsStep::End => Some(json!({"type": "end", "streamId": stream_id})),
                WsStep::Error { code, message } => Some(json!({
                    "type": "error", "streamId": stream_id,
                    "error": {"code": code, "message": message}
                })),
                WsStep::OversizeItem => Some(json!({
                    "type": "item", "streamId": stream_id,
                    "value": {"blob": "x".repeat(transport::MAX_JSON_FRAME_BYTES + 4096)}
                })),
                WsStep::HoldRail(rail) => {
                    rail.hold().await;
                    None
                }
                WsStep::CloseSocket => {
                    session_ended = true;
                    let _ = sock.send(axum::extract::ws::Message::Close(None)).await;
                    None
                }
            };
            if let Some(out) = outbound {
                if sock
                    .send(axum::extract::ws::Message::Text(out.to_string().into()))
                    .await
                    .is_err()
                {
                    session_ended = true;
                    break;
                }
            }
            if session_ended {
                break;
            }
        }
        if session_ended {
            break;
        }
    }
    st.ws_live.fetch_sub(1, Ordering::SeqCst);
    // 协议事实：socket 关闭即移除 Peer，此后该 cookie 的 HTTP/升级被拒。
    st.peer_invalidated.store(true, Ordering::SeqCst);
}

/// 「系统代理」假体：裸 TCP 监听；客户端若经代理即计一命中。
async fn spawn_proxy_fake() -> (u16, Arc<AtomicU32>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy fake");
    let port = listener.local_addr().expect("local addr").port();
    let hits = Arc::new(AtomicU32::new(0));
    let counter = hits.clone();
    tokio::spawn(async move {
        while let Ok((_sock, _)) = listener.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
            // 连接即丢弃：只计命中，不服务
        }
    });
    (port, hits)
}

/// 环境代理注入（Drop 恢复）：指向假代理——若客户端没有禁用系统代理，
/// 假代理必然收到连接（external_redirect_hits > 0，用例失败）。
struct EnvProxyGuard;

impl EnvProxyGuard {
    fn set(port: u16) -> Self {
        let addr = format!("http://127.0.0.1:{port}");
        for key in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            std::env::set_var(key, &addr);
        }
        EnvProxyGuard
    }
}

impl Drop for EnvProxyGuard {
    fn drop(&mut self) {
        for key in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            std::env::remove_var(key);
        }
    }
}

/// 测试用授权句柄：假 cookie + 钉定 authority + 代际（真实生产入口只吃句柄，
/// 不接原始 cookie 参数）。
fn peer_to(authority: &str, epoch: u64, cookie: &str) -> AuthorizedPeer {
    AuthorizedPeer::with_authority(
        "host-under-test",
        cookie.as_bytes().to_vec(),
        authority,
        epoch,
    )
}

fn ready_value(client_id: &str) -> Value {
    json!({"type": "ready", "clientId": client_id, "host": {"home": "/tmp/dsh-shadow"}})
}

// ============================================================
// ① 命名用例：cookie 只归属钉定 authority（外部/代理零命中）
// ============================================================

#[tokio::test]
async fn cookie_attaches_to_owned_authority_only() {
    let (authority, host) = spawn_fake_host().await;
    let (foreign_authority, foreign_host) = spawn_fake_host().await;
    let (proxy_port, proxy_hits) = spawn_proxy_fake().await;
    let _guard = EnvProxyGuard::set(proxy_port);

    let cookie = host.issue_cookie();
    let conn = transport::connect(peer_to(&authority, 7, &cookie))
        .await
        .expect("连接归属宿主");

    // 一元请求：cookie 只随请求发给钉定 authority
    let resp = conn
        .request("r-1", "session/modelCatalog", &json!({}))
        .await
        .expect("一元请求成功");
    assert!(
        matches!(resp.result, NativeResult::Success { .. }),
        "信封 typed 成功"
    );
    assert_eq!(
        host.cookies_seen(),
        vec![cookie.clone()],
        "cookie 原样只发往归属 authority"
    );
    assert!(
        foreign_host.cookies_seen().is_empty(),
        "第二台本机服务器（不同端口）拿不到任何凭据"
    );

    // 触发试图跳出 authority 的重定向：请求必须失败，且不盲随
    host.set_unary(UnaryBehavior::Redirect(format!(
        "http://{foreign_authority}/steal"
    )));
    let err = conn
        .request("r-2", "session/evilRedirect", &json!({}))
        .await
        .expect_err("重定向必须失败");
    assert_eq!(
        err.code,
        codes::REDIRECT_BLOCKED,
        "3xx 不盲随：{}",
        err.detail
    );

    let external_redirect_hits = proxy_hits.load(Ordering::SeqCst) + foreign_host.http_hits();
    assert_eq!(external_redirect_hits, 0, "系统代理与外部宿主零接触");
    conn.close().await;
}

// ============================================================
// ② 命名用例：一元通道拒绝流签名
// ============================================================

#[tokio::test]
async fn unary_rejects_stream_signature() {
    let (authority, host) = spawn_fake_host().await;
    let cookie = host.issue_cookie();
    let conn = transport::connect(peer_to(&authority, 5, &cookie))
        .await
        .expect("连接");

    // a) 流式方法走一元通道：宿主 typed failure（E2 实测 gateway/signature-invalid）
    //    原样上抛，不得误读成成功
    host.set_unary(UnaryBehavior::TypedFailure {
        code: "gateway/signature-invalid",
        message: "stream Remote methods must be opened through the stream carrier",
    });
    let resp = conn
        .request(
            "r-s",
            "workspace/follow",
            &json!({"request": {"address": {"kind": "session", "sessionId": "session-x"}}}),
        )
        .await
        .expect("typed 失败也是合法响应");
    match resp.result {
        NativeResult::Failure { error } => assert_eq!(error.code, "gateway/signature-invalid"),
        NativeResult::Success { .. } => panic!("流签名失败被误读为成功"),
    }

    // b) mux 帧形态直接落在一元通道：客户端侧拒绝（不硬解成信封）
    host.set_unary(UnaryBehavior::MuxShapedBody);
    let err = conn
        .request("r-s2", "session/list", &json!({}))
        .await
        .expect_err("mux 帧不是 server-response 信封");
    assert_eq!(err.code, codes::FRAME_INVALID);
    assert!(
        err.detail.contains("stream"),
        "detail 指明流签名被拒：{}",
        err.detail
    );
    conn.close().await;
}

// ============================================================
// ③ 命名用例：旧代际晚帧被丢弃（mirror_after_late.epoch == current_epoch）
// ============================================================

#[tokio::test]
async fn old_generation_frame_is_ignored() {
    let (authority, host) = spawn_fake_host().await;
    let rail = WsRail::new();
    host.set_ws_steps(vec![
        WsStep::Item(json!({"type": "turn/start", "turnId": "t1"})),
        WsStep::HoldRail(rail.clone()),
        WsStep::Item(json!({"type": "turn/start", "turnId": "t2-late"})),
        WsStep::End,
    ]);
    let cookie = host.issue_cookie();
    let conn = transport::connect(peer_to(&authority, 1, &cookie))
        .await
        .expect("连接（代际 1）");
    let mirror = conn.mirror();
    assert_eq!(mirror.current_epoch(), 1, "镜像初始代 = 绑定代际");

    let mut stream = conn
        .subscribe("$events", &json!({}))
        .await
        .expect("订阅 $events");
    let admitted = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("首事件在窗内")
        .expect("流未终结")
        .expect("事件合法");
    assert_eq!(admitted.epoch, 1, "当前代内事件照常入镜");
    assert_eq!(admitted.value["turnId"], "t1");

    // 宿主换代 → hub 推进镜像代际；随后放行旧连接的迟到帧
    mirror.advance_generation(2);
    let current_epoch = mirror.current_epoch();
    rail.release();

    match tokio::time::timeout(Duration::from_secs(2), stream.next()).await {
        Err(_elapsed) => {} // 晚帧被丢弃：流仍开着，等不到事件
        Ok(None) => {}      // end 帧先到：流终结，晚帧同样未出现
        Ok(Some(Ok(ev))) => panic!("旧代际晚帧污染了镜像: {ev:?}"),
        Ok(Some(Err(e))) => panic!("旧代际晚帧被当成流错误: {e}"),
    }
    let mirror_after_late = mirror.snapshot();
    assert_eq!(mirror_after_late.epoch, current_epoch, "晚帧不改写镜像代际");
    assert_eq!(
        mirror_after_late.latest_epoch,
        Some(1),
        "镜像态停在当前代之前的最后入镜"
    );
    assert!(
        mirror.apply(1, json!({"type": "late-direct"})).is_none(),
        "镜像面直接丢弃旧代帧"
    );
    conn.close().await;
}

// ============================================================
// ④ 命名用例：mux 断开即失效（写不可用、写立即拒绝、cookie 复连被拒）
// ============================================================

#[tokio::test]
async fn mux_disconnect_invalidates_peer() {
    let (authority, host) = spawn_fake_host().await;
    host.set_ws_steps(vec![
        WsStep::Item(ready_value("client-disconnect")),
        WsStep::CloseSocket,
    ]);
    let cookie = host.issue_cookie();
    let conn = transport::connect(peer_to(&authority, 3, &cookie))
        .await
        .expect("连接");
    let mut stream = conn.subscribe("$events", &json!({})).await.expect("订阅");
    let ready = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("ready 在窗内")
        .expect("流未终结")
        .expect("事件合法");
    assert_eq!(ready.value["type"], "ready");

    // 服务端随后关闭 socket → Peer 失效
    assert!(
        tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("终结在窗内")
            .is_none(),
        "断链终结订阅流"
    );
    assert!(!conn.write_available(), "断链后写不可用");

    let started = Instant::now();
    let err = conn
        .request("r-9", "session/list", &json!({}))
        .await
        .expect_err("断链后写拒绝");
    assert_eq!(err.code, codes::CONNECTION_CLOSED);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "立即失败而非排队：{:?}",
        started.elapsed()
    );
    let err2 = conn
        .subscribe("$events", &json!({}))
        .await
        .expect_err("断链后订阅拒绝");
    assert_eq!(err2.code, codes::CONNECTION_CLOSED);

    // 协议面：socket 关闭移除 Peer → 同一 cookie 的升级被拒（终态）
    let err3 = transport::connect(peer_to(&authority, 3, &cookie))
        .await
        .expect_err("旧 cookie 复连被拒");
    assert_eq!(err3.code, codes::PEER_UNAUTHORIZED);
    conn.close().await;
}

// ============================================================
// 补充：401/409 终态；写超时 UnknownDelivery；超限帧；ready/clientId；
// close/drop 无遗留任务；二进制分流；退避上限
// ============================================================

#[tokio::test]
async fn terminal_auth_and_conflict_do_not_retry() {
    let (authority, host) = spawn_fake_host().await;
    // 401：cookie 不对 → 升级拒绝；恰一次尝试（无内部重试循环）
    let err = transport::connect(peer_to(&authority, 2, "dsh-auth-test=WRONG"))
        .await
        .expect_err("错 cookie 拒连");
    assert_eq!(err.code, codes::PEER_UNAUTHORIZED);
    assert_eq!(host.ws_attempts(), 1, "401 终态不重试");

    // 409：一 Peer 一条归属 socket；第二条物理 socket 冲突
    host.set_ws_steps(vec![WsStep::Item(ready_value("client-keep"))]);
    let cookie = host.issue_cookie();
    let conn = transport::connect(peer_to(&authority, 2, &cookie))
        .await
        .expect("首条 socket");
    let err2 = transport::connect(peer_to(&authority, 2, &cookie))
        .await
        .expect_err("重复关联冲突");
    assert_eq!(err2.code, codes::PEER_CONFLICT);
    assert_eq!(
        host.ws_attempts(),
        3,
        "每次 connect 恰一次升级尝试（1+1+1）"
    );
    conn.close().await;
}

#[tokio::test]
async fn write_timeout_surfaces_unknown_delivery_without_retry() {
    let (authority, host) = spawn_fake_host().await;
    host.set_unary(UnaryBehavior::Hang);
    let cookie = host.issue_cookie();
    let conn = transport::connect_with(
        peer_to(&authority, 4, &cookie),
        TransportConfig {
            unary_timeout: Duration::from_millis(300),
        },
    )
    .await
    .expect("连接（短超时注入）");
    let started = Instant::now();
    let err = conn
        .request("r-t", "session/prompt", &json!({"text": "hi"}))
        .await
        .expect_err("写超时");
    assert_eq!(
        err.code,
        codes::WRITE_TIMEOUT,
        "送达未知（上游映射 UnknownDelivery）"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "按注入超时即断：{:?}",
        started.elapsed()
    );
    assert_eq!(host.unary_hits(), 1, "写超时不自动重投");
    conn.close().await;
}

#[tokio::test]
async fn oversize_frame_is_rejected() {
    let (authority, host) = spawn_fake_host().await;
    host.set_ws_steps(vec![WsStep::OversizeItem]);
    let cookie = host.issue_cookie();
    let conn = transport::connect(peer_to(&authority, 6, &cookie))
        .await
        .expect("连接");
    let mut stream = conn.subscribe("$events", &json!({})).await.expect("订阅");
    let err = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("窗内")
        .expect("流未静默终结")
        .expect_err("超限帧被拒");
    assert_eq!(err.code, codes::FRAME_OVERSIZE, "JSON 帧 2MiB 上限");
    assert!(
        !conn.write_available(),
        "超限帧按协议违规关闭连接（有界拒绝）"
    );
    conn.close().await;
}

/// R11：**无 Content-Length** 的响应体必须在读取过程中累计限额——不得先
/// `bytes()` 聚合整包再按 2MiB 拒绝（chunked/未声明长度的大响应因此可以突破
/// 声称的内存硬上限）。
///
/// 确定性设计：假宿主推满 `park_after`（= 上限 + 一个 chunk）后**停在原地**，
/// 剩余 body 只有测试放行才继续。客户端必须在「宿主还停着」时就报超限——若它
/// 等整包再判，请求会一直挂到放出剩余字节（本用例不等放行，直接观察失败）。
#[tokio::test]
async fn undeclared_length_body_is_capped_while_reading() {
    let (authority, host) = spawn_fake_host().await;
    let cap = transport::MAX_JSON_FRAME_BYTES;
    let chunk = 64 * 1024;
    let park_after = cap + chunk; // 上限 + 一个 chunk：越界必然已可判定
    let total = park_after + 1024 * 1024; // 至少再多 1MiB 没推送
    let written = Arc::new(AtomicU32::new(0));
    let release = Arc::new(tokio::sync::Notify::new());
    host.set_unary(UnaryBehavior::NoLengthStream {
        total_bytes: total,
        chunk_bytes: chunk,
        park_after,
        written: written.clone(),
        release: release.clone(),
    });
    let cookie = host.issue_cookie();
    let conn = transport::connect(peer_to(&authority, 12, &cookie))
        .await
        .expect("连接");
    let err = tokio::time::timeout(
        Duration::from_secs(5),
        conn.request("r-big", "session/list", &json!({})),
    )
    .await
    .expect("宿主停在 cap 附近时客户端就必须拒绝（不得等整包）")
    .expect_err("无长度大响应必须被拒");
    assert_eq!(
        err.code,
        codes::FRAME_OVERSIZE,
        "沿用既有超限码：{}",
        err.detail
    );

    // 关键断言：宿主从未把整包推完（停在 park_after），说明客户端读入量有界。
    let pushed = written.load(Ordering::SeqCst) as usize;
    assert_eq!(pushed, park_after, "宿主应恰好推到闸点：{pushed}");
    assert!(
        pushed < total,
        "整包（{total}）没有被推送：上限在读取时生效"
    );
    release.notify_waiters(); // 放行宿主 stream，避免残留任务
    conn.close().await;
}

/// R11 对照：同为无 Content-Length，但体量正常的响应仍照常解析（限额逻辑不误伤）。
#[tokio::test]
async fn undeclared_length_small_body_still_parses() {
    let (authority, host) = spawn_fake_host().await;
    host.set_unary(UnaryBehavior::NoLengthEnvelope);
    let cookie = host.issue_cookie();
    let conn = transport::connect(peer_to(&authority, 13, &cookie))
        .await
        .expect("连接");
    let resp = conn
        .request("r-small", "session/modelCatalog", &json!({}))
        .await
        .expect("无长度小响应正常解析");
    assert!(
        matches!(resp.result, NativeResult::Success { .. }),
        "信封 typed 成功"
    );
    conn.close().await;
}

#[tokio::test]
async fn native_stream_error_frame_surfaces_as_stream_error() {
    let (authority, host) = spawn_fake_host().await;
    // windows 报告 §3 实测键形：{"type":"error","streamId",error:{code,message}}
    host.set_ws_steps(vec![WsStep::Error {
        code: "gateway/invocation-unavailable".into(),
        message: "no active Remote method exports this endpoint".into(),
    }]);
    let cookie = host.issue_cookie();
    let conn = transport::connect(peer_to(&authority, 11, &cookie))
        .await
        .expect("连接");
    let mut stream = conn.subscribe("$events", &json!({})).await.expect("订阅");
    let err = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("窗内")
        .expect("流未静默终结")
        .expect_err("错误帧上抛为流级错误");
    assert_eq!(err.code, codes::STREAM_ERROR);
    assert!(
        err.detail.contains("gateway/invocation-unavailable"),
        "原生错误码进 detail：{}",
        err.detail
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("终结在窗内")
            .is_none(),
        "错误帧后流终结"
    );
    assert!(conn.write_available(), "流级错误不断开归属连接");
    conn.close().await;
}

#[tokio::test]
async fn ready_and_client_id_surface_after_open() {
    let (authority, host) = spawn_fake_host().await;
    host.set_ws_steps(vec![
        WsStep::Item(ready_value("11111111-2222-3333-4444-555555555555")),
        WsStep::End,
    ]);
    let cookie = host.issue_cookie();
    let conn = transport::connect(peer_to(&authority, 8, &cookie))
        .await
        .expect("连接");
    assert_eq!(conn.client_id(), None, "升级后尚未观测 ready");
    let mut stream = conn.subscribe("$events", &json!({})).await.expect("订阅");
    let ready = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("窗内")
        .expect("流未终结")
        .expect("事件合法");
    assert_eq!(ready.value["type"], "ready");
    assert_eq!(
        ready.value["clientId"],
        "11111111-2222-3333-4444-555555555555"
    );
    assert_eq!(
        conn.client_id().as_deref(),
        Some("11111111-2222-3333-4444-555555555555"),
        "ready 的 clientId 上浮到连接观测面"
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("end 在窗内")
            .is_none(),
        "end 帧终结订阅流"
    );
    conn.close().await;
}

#[tokio::test]
async fn close_and_drop_leave_no_lingering_tasks() {
    // close()：join reader/writer 完成（完成即证明任务退出，无遗留）
    let (authority, host) = spawn_fake_host().await;
    let cookie = host.issue_cookie();
    let conn = transport::connect(peer_to(&authority, 9, &cookie))
        .await
        .expect("连接");
    let mut stream = conn.subscribe("$events", &json!({})).await.expect("订阅");
    conn.close().await;
    assert!(!conn.write_available());
    assert!(
        tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("终结在窗内")
            .is_none(),
        "关闭取消订阅流"
    );
    assert!(
        host.wait_until_no_live_ws(Duration::from_secs(2)).await,
        "服务端观测 socket 已关"
    );

    // drop：连接与流句柄全部遗弃 → socket 关闭（无任务拉住连接）
    let (authority2, host2) = spawn_fake_host().await;
    let cookie2 = host2.issue_cookie();
    let conn2 = transport::connect(peer_to(&authority2, 9, &cookie2))
        .await
        .expect("第二条连接");
    let stream2 = conn2.subscribe("$events", &json!({})).await.expect("订阅");
    drop(stream2);
    drop(conn2);
    assert!(
        host2.wait_until_no_live_ws(Duration::from_secs(2)).await,
        "drop 后 socket 关闭"
    );
}

#[tokio::test]
async fn binary_response_is_not_misread_as_envelope() {
    let (authority, host) = spawn_fake_host().await;
    host.set_unary(UnaryBehavior::Binary);
    let cookie = host.issue_cookie();
    let conn = transport::connect(peer_to(&authority, 10, &cookie))
        .await
        .expect("连接");
    let reply = conn
        .request_raw("r-b", "session/uploadFileBinary", &json!({}))
        .await
        .expect("按 content-type 分流");
    match reply {
        UnaryReply::Binary {
            content_type,
            bytes,
        } => {
            assert!(
                content_type.starts_with("application/octet-stream"),
                "{content_type}"
            );
            assert_eq!(bytes, b"\x00\x01binary-payload".to_vec(), "二进制原样交付");
        }
        UnaryReply::Envelope(_) => panic!("二进制响应被硬解成信封"),
    }
    let err = conn
        .request("r-b2", "session/uploadFileBinary", &json!({}))
        .await
        .expect_err("信封路径拒绝二进制");
    assert_eq!(err.code, codes::FRAME_INVALID);
    conn.close().await;
}

#[test]
fn recovery_backoff_caps_at_ten_seconds() {
    let mut previous = Duration::ZERO;
    for attempt in 0u32..=24 {
        let delay = transport::recovery_backoff(attempt);
        assert!(
            delay <= transport::RECOVERY_BACKOFF_CAP,
            "退避不超过 10 秒：{attempt} → {delay:?}"
        );
        assert!(delay >= previous, "退避单调不回退");
        previous = delay;
    }
    assert_eq!(transport::recovery_backoff(0), Duration::from_millis(500));
    assert_eq!(transport::recovery_backoff(4), Duration::from_secs(8));
    assert_eq!(transport::recovery_backoff(5), Duration::from_secs(10));
    assert_eq!(
        transport::recovery_backoff(23),
        Duration::from_secs(10),
        "封顶后不无限增长"
    );
    assert_eq!(
        TransportConfig::default().unary_timeout,
        Duration::from_secs(10),
        "单次请求默认 10 秒"
    );
    assert_eq!(
        transport::MAX_JSON_FRAME_BYTES,
        2 * 1024 * 1024,
        "JSON 帧上限 2MiB"
    );
}

#[test]
fn peer_authority_is_visible_but_secret_redacted() {
    let peer = AuthorizedPeer::with_authority("h", b"cookie-VALUE".to_vec(), "127.0.0.1:1", 3);
    assert_eq!(peer.authority(), "127.0.0.1:1");
    assert_eq!(peer.epoch(), 3);
    let rendered = format!("{peer:?}");
    assert!(
        rendered.contains("127.0.0.1:1"),
        "authority 非秘密可显示：{rendered}"
    );
    assert!(
        !rendered.contains("cookie-VALUE"),
        "Debug 泄露了秘密载荷：{rendered}"
    );
}

/// 未钉定 authority 的句柄 → 拒绝连接（不猜目标）。
#[tokio::test]
async fn connect_refuses_peer_without_pinned_authority() {
    let err = transport::connect(AuthorizedPeer::new("h", b"cookie".to_vec()))
        .await
        .expect_err("无 authority 拒连");
    assert_eq!(err.code, codes::CONNECT_FAILED);
}

// ============================================================
// R08 回归：撤权必须贯穿传输层（关闭在途连接 + 拒绝重连），不只是清内存
// ============================================================

/// R08（审查 R08 条目）：旧 `revoke` 只清 `current_peer` 与一条 IPC 连接；**已交付
/// 的句柄**、官方 `PeerConnection`、Mux socket 都没有失效——连上原生通道后旧连接
/// 仍可继续发请求。契约（与 group 3 钉定）：
///  ① 进程内闸门翻转：此后任何带该 binding 句柄的 `transport::connect` 一律拒绝
///     （专用码 `DSH_PEER_REVOKED`）；
///  ② 该 binding 名下**每一条活连接**都在 socket 层被关掉（含 mux socket）。
///
/// 本用例走**真实**本地假宿主 + 真实 `transport::connect`（不用内存假连接）。
#[tokio::test]
async fn revoke_gate_closes_live_connection_and_refuses_reconnect() {
    let (authority, host) = spawn_fake_host().await;
    let cookie = host.issue_cookie();

    // 两个独立 binding 各持一份闸门（第二份用于验证「未被撤的正确连接不受影响」）。
    let gate_a = transport::PeerGate::new();
    let gate_b = transport::PeerGate::new();

    let peer_a = peer_to(&authority, 12, &cookie).with_gate(Arc::clone(&gate_a));
    let conn_a = transport::connect(peer_a.clone())
        .await
        .expect("binding A 连上");
    assert!(conn_a.write_available(), "撤权前可写");

    // 闸门已登记这条活连接（撤权前的连接计数观测面）。
    assert_eq!(gate_a.live_connections(), 1, "在途连接必须登记到闸门");

    // 撤权（auth 侧 `revoke` 的落点就是这一步）。
    gate_a.revoke();
    assert!(gate_a.is_revoked(), "闸门翻转");

    // ② 活连接在 socket 层被关：观测面翻转 + 后续请求立即拒绝 + 宿主看到 socket 关闭。
    assert!(
        !conn_a.write_available(),
        "撤权后活连接写不可用（不再冒充可写）"
    );
    let err = conn_a
        .request("r-revoked", "session/list", &json!({}))
        .await
        .expect_err("撤权后请求必须失败");
    assert_eq!(err.code, codes::CONNECTION_CLOSED, "{}", err.detail);
    assert!(
        host.wait_until_no_live_ws(Duration::from_secs(3)).await,
        "撤权必须真的关掉 mux socket（宿主侧观测到 0 条活连接）"
    );

    // ① 重连被拒（专用码，不是笼统的 CONNECT_FAILED）。
    let err2 = transport::connect(peer_a)
        .await
        .expect_err("撤权后重连必须被拒");
    assert_eq!(err2.code, codes::PEER_REVOKED, "{}", err2.detail);
    assert_eq!(host.ws_attempts(), 1, "撤权后不得再向宿主发起升级尝试");

    // 未被撤的第二份 binding 不受影响（撤权不得殃及其他绑定）。
    let (authority_b, host_b) = spawn_fake_host().await;
    let cookie_b = host_b.issue_cookie();
    let conn_b =
        transport::connect(peer_to(&authority_b, 13, &cookie_b).with_gate(Arc::clone(&gate_b)))
            .await
            .expect("binding B 连上");
    assert!(conn_b.write_available(), "B 在 A 撤权后仍可写");
    conn_b
        .request("r-b", "session/list", &json!({}))
        .await
        .expect("B 的请求不受 A 撤权影响");
    assert!(!gate_b.is_revoked(), "B 的闸门未被翻转");
    conn_b.close().await;
    conn_a.close().await;
}

/// 撤权闸门的进程内语义细节：幂等、撤销后注册被拒、未撤权时连接登记后
/// 正常解除（不留悬挂回调）。
#[tokio::test]
async fn revoke_gate_is_idempotent_and_refuses_late_registration() {
    let gate = transport::PeerGate::new();
    assert!(!gate.is_revoked());
    gate.revoke();
    gate.revoke(); // 幂等
    assert!(gate.is_revoked());
    assert_eq!(gate.live_connections(), 0);
}

/// 未挂闸门的句柄行为不变（旧调用点/测试构造不受影响——公共面保持向后兼容）。
#[tokio::test]
async fn peer_without_gate_connects_as_before() {
    let (authority, host) = spawn_fake_host().await;
    let cookie = host.issue_cookie();
    let peer = peer_to(&authority, 14, &cookie);
    assert!(peer.gate().is_none(), "未挂闸门的句柄 gate() 为 None");
    let conn = transport::connect(peer).await.expect("照常连上");
    assert!(conn.write_available());
    conn.close().await;
}

// ============================================================
// P1-2 / P1-1a（T10 复审）：发送相位分类 + 长生命周期归属连接
// ============================================================

/// 测试信道：恒定句柄直连假宿主。
fn channel_to(authority: &str, epoch: u64, cookie: &str) -> Arc<PeerChannel> {
    PeerChannel::from_static_peer(peer_to(authority, epoch, cookie))
}

fn send_target() -> Target {
    Target {
        host_id: "host-under-test".into(),
        epoch: 5,
        session_id: "s-1".into(),
    }
}

fn send_payload(text: &str) -> SendPayload {
    SendPayload {
        text: text.into(),
        receipt_ids: Vec::new(),
        queue_only: false,
        bypass_paused_queue: false,
    }
}

/// 预提交门拒绝时请求不得写出，撤销门后可正常重试。
#[tokio::test]
async fn pre_submit_gate_refusal_prevents_wire_write() {
    use multi_agents_manager_lib::services::dsh_control::error::DshError;
    let (authority, host) = spawn_fake_host().await;
    let channel = channel_to(&authority, 5, &host.issue_cookie());
    let target = send_target();
    let args = json!({"request": {"content": "first"}});
    channel
        .request_classified("first", "session/prompt", &args, Some(&target))
        .await
        .expect("建连")
        .expect("首次发送");
    channel.install_pre_submit(Some(Arc::new(|_target| {
        Box::pin(async { Err(DshError::new(codes::STATE_IO, "gate refused")) })
    })));
    let err = channel
        .request_classified("blocked", "session/prompt", &args, Some(&target))
        .await
        .expect_err("门拒绝应明确上抛");
    assert_eq!(err.code, codes::STATE_IO);
    assert_eq!(host.unary_hits(), 1);
    assert!(!host
        .unary_bodies()
        .iter()
        .any(|body| body["rpcId"] == "blocked"));
    channel.install_pre_submit(None);
    channel
        .request_classified("retry", "session/prompt", &args, Some(&target))
        .await
        .expect("撤门")
        .expect("重试发送");
    assert!(host
        .unary_bodies()
        .iter()
        .any(|body| body["rpcId"] == "retry"));
}

/// 持有通道锁期间安装门，等待中的请求必须在拿锁后重新读取门。
#[tokio::test]
async fn pre_submit_gate_checked_after_lock_and_identity() {
    use multi_agents_manager_lib::services::dsh_control::error::DshError;
    let (authority, host) = spawn_fake_host().await;
    let channel = channel_to(&authority, 5, &host.issue_cookie());
    let target = send_target();
    let args = json!({"request": {"content": "first"}});
    channel
        .request_classified("first", "session/prompt", &args, Some(&target))
        .await
        .expect("建连")
        .expect("首次发送");
    host.set_unary(UnaryBehavior::Hang);
    let holder_channel = channel.clone();
    let holder = tokio::spawn(async move {
        holder_channel
            .request_classified("holder", "session/prompt", &json!({}), Some(&send_target()))
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while host.unary_hits() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("持锁请求到达宿主");
    let blocked = channel.request_classified("blocked", "session/prompt", &args, Some(&target));
    tokio::pin!(blocked);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut blocked)
            .await
            .is_err(),
        "请求应等待通道锁"
    );
    channel.install_pre_submit(Some(Arc::new(|_target| {
        Box::pin(async { Err(DshError::new(codes::STATE_IO, "gate refused")) })
    })));
    holder.abort();
    let _ = holder.await;
    let err = tokio::time::timeout(Duration::from_secs(5), &mut blocked)
        .await
        .expect("等待门结果")
        .expect_err("拿锁后应拒绝");
    assert_eq!(err.code, codes::STATE_IO);
    assert!(!host
        .unary_bodies()
        .iter()
        .any(|body| body["rpcId"] == "blocked"));
}

/// 建连等待期间选择切换：门在连接就绪后、写出前二次复核（复审追补）。
/// 状态门首次放行（覆盖建连路径）、连接就绪后转为拒绝——确定性模拟「门放行
/// 后等待 WebSocket 升级期间用户切走」；断言请求零写出（确定未执行）。
#[tokio::test]
async fn pre_submit_gate_rechecked_after_connection_setup() {
    use multi_agents_manager_lib::services::dsh_control::error::DshError;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (authority, host) = spawn_fake_host().await;
    let cookie = host.issue_cookie();
    let channel = PeerChannel::from_static_peer(peer_to(&authority, 5, &cookie));
    let calls = AtomicUsize::new(0);
    channel.install_pre_submit(Some(Arc::new(move |_target| {
        let n = calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if n == 0 {
                Ok(())
            } else {
                Err(DshError::new(
                    codes::STATE_IO,
                    "selection switched during connect",
                ))
            }
        })
    })));
    let target = send_target();
    let args = json!({"request": {"content": "second-gate"}});
    let err = channel
        .request_classified("second-gate", "session/prompt", &args, Some(&target))
        .await
        .expect_err("建连后二次复核应拒绝");
    assert_eq!(err.code, codes::STATE_IO);
    assert!(
        !host
            .unary_bodies()
            .iter()
            .any(|body| body["rpcId"] == "second-gate"),
        "建连等待期间切换 → 请求零写出"
    );
}

/// 宿主已经收到请求但不应答时，提交边界的排空不得等待响应。
#[tokio::test(flavor = "multi_thread")]
async fn response_barrier_must_not_hold_commit_permit() {
    use multi_agents_manager_lib::services::dsh_control::production;

    let (authority, host) = spawn_fake_host().await;
    host.set_unary(UnaryBehavior::Hang);
    let channel = channel_to(&authority, 5, &host.issue_cookie());
    let sender = tokio::spawn(async move {
        channel
            .send_message(&send_target(), &send_payload("挂起响应"), "held-response")
            .await
    });
    let arrived = tokio::time::timeout(Duration::from_secs(5), async {
        while !host
            .unary_bodies()
            .iter()
            .any(|body| body["rpcId"] == "held-response")
        {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let mut flip = tokio::task::spawn_blocking(production::commit_flip_begin);
    let drained = tokio::time::timeout(Duration::from_secs(1), &mut flip).await;
    // 即使断言失败也先撤销挂起请求并释放全局边界。
    sender.abort();
    let _ = sender.await;
    if let Ok(guard) = drained.as_ref() {
        assert!(guard.is_ok(), "排空任务不能异常退出");
    }
    let drained_in_time = drained.is_ok();
    if let Ok(guard) = drained {
        drop(guard);
    } else {
        drop(
            tokio::time::timeout(Duration::from_secs(5), flip)
                .await
                .expect("排空清理"),
        );
    }
    assert!(arrived.is_ok(), "请求必须真实到达假宿主");
    assert!(drained_in_time, "响应仍挂起时提交边界必须在 1 秒内排空");
}

/// 关闭须取消停在第二道预检的旧请求，第三道门与宿主 POST 都不得发生。
#[tokio::test(flavor = "multi_thread")]
async fn closing_cancels_send_while_second_pre_submit_gate_is_blocked() {
    use multi_agents_manager_lib::services::dsh_control::production;
    use std::sync::atomic::AtomicUsize;
    let (authority, host) = spawn_fake_host().await;
    let instance_id = "flip-close-under-test";
    let cookie = host.issue_cookie();
    let channel = PeerChannel::from_static_peer(AuthorizedPeer::with_authority(
        instance_id,
        cookie.as_bytes().to_vec(),
        &authority,
        5,
    ));
    let target = Target {
        host_id: instance_id.into(),
        epoch: 5,
        session_id: "s-1".into(),
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let checked = Arc::new(tokio::sync::Notify::new());
    channel.install_pre_submit(Some(Arc::new({
        let calls = calls.clone();
        let checked = checked.clone();
        move |_| {
            let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
            let checked = checked.clone();
            Box::pin(async move {
                if n == 2 {
                    checked.notify_one();
                    std::future::pending::<()>().await;
                }
                Ok(())
            })
        }
    })));
    let task = tokio::spawn(async move {
        channel
            .request_classified("commit-race", "session/prompt", &json!({}), Some(&target))
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), checked.notified())
        .await
        .expect("第二次门到达屏障");
    let closing_epoch = production::mark_closing(instance_id);
    let mut task = task;
    let result = tokio::time::timeout(Duration::from_secs(5), &mut task).await;
    if result.is_err() {
        task.abort();
    }
    assert!(production::clear_closing_if(instance_id, closing_epoch));
    let result = result
        .expect("第二道门仍阻塞时，关闭须取消请求")
        .expect("任务完成");
    let hits = host
        .unary_bodies()
        .iter()
        .filter(|body| body["rpcId"] == "commit-race")
        .count();
    let count = calls.load(Ordering::SeqCst);
    assert!(
        result.is_err() && hits == 0 && count == 2,
        "结果={result:?}, 请求命中={hits}, 门调用={count}"
    );
    let err = result.unwrap_err();
    assert_eq!(
        err.code,
        codes::SWITCH_OFF,
        "关闭取消须报告开关关闭，实际={err}"
    );
}

/// 响应挂住期间关闭必须立即生效（销项复审 Important-6 关闭首步回归）：真通道
/// Hang 发送（POST 已达、响应悬住）→ `mark_closing`（disable 首步语义）→
/// 拒绝面即刻在场：`is_closing=true`，并行的**第二笔**发送 1s 内 SWITCH_OFF
/// 拒绝且第二假宿主零命中；随后挂住的第一笔按 Hang 超时 → UnknownDelivery
/// 收口。第二笔走**第二假宿主**（同一假宿主一 Peer 一 socket——409 会先于
/// 关闭判据拒绝；关闭旗标按实例 id 全局生效，两宿主同一 id 即同一拒绝面）。
/// 不使用 submit_pause 停驻闸（到达观测走假宿主 unary_bodies 轮询）——无
/// rpcId 归因歧义面。
#[tokio::test(flavor = "multi_thread")]
async fn close_effective_while_response_barrier_held() {
    use multi_agents_manager_lib::services::dsh_control::production;
    let (authority, host) = spawn_fake_host().await;
    let (refused_authority, refused_host) = spawn_fake_host().await;
    host.set_unary(UnaryBehavior::Hang);
    let instance_id = "close-while-hang";
    let target_for = |session: &str| Target {
        host_id: instance_id.into(),
        epoch: 5,
        session_id: session.into(),
    };
    let channel = PeerChannel::from_static_peer(AuthorizedPeer::with_authority(
        instance_id,
        host.issue_cookie().as_bytes().to_vec(),
        &authority,
        5,
    ));
    let sender_channel = Arc::clone(&channel);
    let sender_target = target_for("s-1");
    let sender = tokio::spawn(async move {
        sender_channel
            .send_message(&sender_target, &send_payload("挂住"), "held-close")
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !host
            .unary_bodies()
            .iter()
            .any(|body| body["rpcId"] == "held-close")
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("挂住的请求必须真实到达假宿主（响应悬住）");
    // 关闭首步（disable 入口同款）落地：拒绝面即刻生效。
    let closing_epoch = production::mark_closing(instance_id);
    assert!(
        production::is_closing(instance_id),
        "关闭首步后旗标必须立即在场"
    );
    // 响应挂住期间：第二笔发送立即拒绝（1s 预算内；不等 unary 超时；同一
    // 实例 id 在第二宿主上同样被内存判据拒绝）。
    let refused_channel = PeerChannel::from_static_peer(AuthorizedPeer::with_authority(
        instance_id,
        refused_host.issue_cookie().as_bytes().to_vec(),
        &refused_authority,
        5,
    ));
    let refused_target = target_for("s-1");
    let refused = tokio::time::timeout(
        Duration::from_secs(1),
        refused_channel.send_message(&refused_target, &send_payload("拒绝面"), "refused-close"),
    )
    .await;
    let refused = refused
        .expect("关闭期间新发送必须在 1 秒内拒绝（不等 unary 超时）")
        .expect_err("关闭旗标在场时必须拒绝");
    // 收尾清旗（按纪元；断言失败也不毒化同进程其他用例）。
    assert!(
        production::clear_closing_if(instance_id, closing_epoch),
        "清理必须按纪元清旗"
    );
    assert_eq!(refused.code, codes::SWITCH_OFF, "实际拒绝码={refused}");
    assert!(
        !refused_host
            .unary_bodies()
            .iter()
            .any(|body| body["rpcId"] == "refused-close"),
        "关闭期间的新发送零 POST"
    );
    // 挂住的第一笔：Hang 超时（10s unary 上界）→ UnknownDelivery 路径收口。
    let held = tokio::time::timeout(Duration::from_secs(13), sender)
        .await
        .expect("挂住发送最终收口")
        .expect("发送任务未 panic");
    assert_eq!(held, Ok(NativeSendOutcome::UnknownDelivery));
}

/// 发送停在「最终盘读门已过、许可已领、尚未交付」窗口时关闭落地（销项复审
/// Important-6 提交点回归锚）：`TEST_SUBMIT_PAUSE` 停驻（dsh-test-probes 面）
/// → `mark_closing` → 放行 → 许可区**内存判据**拒绝（SWITCH_OFF、假宿主该
/// rpcId 零命中）。独立 host_id：旗标为进程级全局量，不与并行用例串扰。
/// （dsh-test-probes 面：submit_pause 接缝仅在该 feature 下导出——默认构建
/// 编译本测试时本用例整体缺席。）
#[cfg(feature = "dsh-test-probes")]
#[tokio::test(flavor = "multi_thread")]
async fn close_refuses_stalled_send_zero_post() {
    assert_stalled_send_refused(false).await;
}

#[cfg(feature = "dsh-test-probes")]
#[tokio::test(flavor = "multi_thread")]
async fn stalled_send_close_via_disable_refuses_zero_commit() {
    assert_stalled_send_refused(true).await;
}

#[cfg(feature = "dsh-test-probes")]
static STALLED_SEND_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(feature = "dsh-test-probes")]
async fn assert_stalled_send_refused(via_disable: bool) {
    use multi_agents_manager_lib::services::dsh_control::{actions, production};
    let _serial = STALLED_SEND_SERIAL.lock().await;
    let (authority, host) = spawn_fake_host().await;
    let profile = tempfile::tempdir().unwrap();
    let registered = via_disable.then(|| {
        use multi_agents_manager_lib::services::dsh_control::instances::HostKind;
        std::fs::write(profile.path().join("package.json"), b"{}").unwrap();
        production::production_registry()
            .unwrap()
            .register_candidate(HostKind::Desktop, &profile.path().canonicalize().unwrap())
            .unwrap()
    });
    let instance_id = registered
        .as_ref()
        .map_or("stall-close-under-test", |entry| entry.instance_id.as_str());
    let cookie = host.issue_cookie();
    let channel = PeerChannel::from_static_peer(AuthorizedPeer::with_authority(
        instance_id,
        cookie.as_bytes().to_vec(),
        &authority,
        5,
    ));
    let target = Target {
        host_id: instance_id.into(),
        epoch: 5,
        session_id: "s-1".into(),
    };
    // 布防停驻闸（RAII 守卫：断言失败 unwind 也解除布防，不留全局残留）
    // → 发送停在「最终门后、交付前」→ 关闭首步落地 → 放行。
    let _pause = actions::submit_pause::arm();
    let sender_channel = Arc::clone(&channel);
    let sender_target = target.clone();
    let sender = tokio::spawn(async move {
        sender_channel
            .send_message(&sender_target, &send_payload("停驻"), "stall-close")
            .await
    });
    let entered = tokio::time::timeout(Duration::from_secs(5), async {
        while !actions::submit_pause::entered_for("stall-close") {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(entered.is_ok(), "发送必须到达提交前停驻点");
    let closing_epoch = if via_disable {
        // 服务层真实关闭入口，登记但未开启的实例也必须在入口标记关闭。
        let service = production::switch_for_instance(instance_id).unwrap();
        tokio::task::spawn_blocking(move || service.disable())
            .await
            .unwrap()
            .unwrap();
        production::closing_epoch(instance_id).expect("真实关闭入口必须先标记")
    } else {
        production::mark_closing(instance_id)
    };
    actions::submit_pause::resume();
    // 先等发送收口再清旗——旗标必须在停驻窗内存活到内存判据读取（提前清旗
    // 会与被测判据竞速，测的是清理不是关闭面）。
    let outcome = tokio::time::timeout(Duration::from_secs(5), sender)
        .await
        .expect("停驻发送必须收口")
        .expect("发送任务未 panic");
    assert!(
        matches!(&outcome, Err(e) if e.code == codes::SWITCH_OFF),
        "停驻期间的关闭必须以 SWITCH_OFF 拒绝，实际={outcome:?}"
    );
    assert!(
        !host
            .unary_bodies()
            .iter()
            .any(|body| body["rpcId"] == "stall-close"),
        "假宿主该 rpcId 必须零命中"
    );
    // 收尾清旗（按纪元；此时旗标仍应在场——拒绝面不清它，清理责任在测试）。
    assert!(
        production::clear_closing_if(instance_id, closing_epoch),
        "旗标必须在发送收口时仍在场（纪元条件清除核对）"
    );
}

#[cfg(feature = "dsh-test-probes")]
#[tokio::test(flavor = "multi_thread")]
async fn stalled_send_close_reopen_cycle_refuses_then_fresh_send_delivers() {
    use multi_agents_manager_lib::services::dsh_control::{actions, production};
    let _serial = STALLED_SEND_SERIAL.lock().await;
    let (authority, host) = spawn_fake_host().await;
    let instance_id = "stall-close-reopen";
    let channel = PeerChannel::from_static_peer(AuthorizedPeer::with_authority(
        instance_id,
        host.issue_cookie().into_bytes(),
        &authority,
        5,
    ));
    let target = Target {
        host_id: instance_id.into(),
        epoch: 5,
        session_id: "s-1".into(),
    };
    let _pause = actions::submit_pause::arm();
    let old = tokio::spawn({
        let channel = Arc::clone(&channel);
        let target = target.clone();
        async move {
            channel
                .send_message(&target, &send_payload("旧"), "old-reopen")
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !actions::submit_pause::entered_for("old-reopen") {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("旧发送必须停在许可区内");
    let generation = production::instance_generation(instance_id);
    let epoch = production::mark_closing(instance_id);
    assert!(production::clear_closing_and_advance(instance_id, epoch));
    assert!(production::instance_generation(instance_id) > generation);
    actions::submit_pause::resume();
    let old = tokio::time::timeout(Duration::from_secs(5), old)
        .await
        .unwrap()
        .unwrap();
    let fresh = channel
        .send_message(&target, &send_payload("新"), "fresh-reopen")
        .await;
    let bodies = host.unary_bodies();
    assert!(
        matches!(fresh, Ok(NativeSendOutcome::Accepted { .. })),
        "新发送必须被受理: {fresh:?}"
    );
    assert_eq!(
        bodies
            .iter()
            .filter(|body| body["rpcId"] == "fresh-reopen")
            .count(),
        1
    );
    let old_posts = bodies
        .iter()
        .filter(|body| body["rpcId"] == "old-reopen")
        .count();
    assert!(
        matches!(&old, Err(error) if error.code == codes::SWITCH_OFF) && old_posts == 0,
        "旧代次必须拒绝且零交付: {old:?}, POST={old_posts}"
    );
}

async fn send_with_unary_behavior(behavior: UnaryBehavior, rpc_id: &str) -> NativeSendOutcome {
    let (authority, host) = spawn_fake_host().await;
    host.set_unary(behavior);
    let channel = channel_to(&authority, 5, &host.issue_cookie());
    let outcome = channel
        .send_message(&send_target(), &send_payload("测试"), rpc_id)
        .await
        .expect("请求完成");
    assert!(host
        .unary_bodies()
        .iter()
        .any(|body| body["rpcId"] == rpc_id));
    outcome
}

#[tokio::test]
async fn send_missing_accepted_is_unknown_delivery() {
    assert_eq!(
        send_with_unary_behavior(UnaryBehavior::AcceptedMissing, "missing").await,
        NativeSendOutcome::UnknownDelivery
    );
}

#[tokio::test]
async fn send_false_accepted_is_rejected() {
    assert_eq!(
        send_with_unary_behavior(UnaryBehavior::AcceptedFalse, "false").await,
        NativeSendOutcome::Rejected {
            code: "gateway/not-accepted".into()
        }
    );
}

#[tokio::test]
async fn send_mistyped_accepted_is_unknown_delivery() {
    assert_eq!(
        send_with_unary_behavior(UnaryBehavior::AcceptedMistyped, "mistyped").await,
        NativeSendOutcome::UnknownDelivery
    );
}

/// 写出后响应连接中断仍是投递未知，不能降为确定未执行。
#[tokio::test]
async fn post_write_disconnect_still_unknown_delivery() {
    let (authority, host) = spawn_fake_host().await;
    host.set_unary(UnaryBehavior::DisconnectResponse);
    let channel = channel_to(&authority, 5, &host.issue_cookie());
    let outcome = channel
        .send_message(&send_target(), &send_payload("断线"), "disconnect")
        .await;
    assert_eq!(outcome, Ok(NativeSendOutcome::UnknownDelivery));
    assert!(
        host.unary_bodies()
            .iter()
            .any(|body| body["rpcId"] == "disconnect"),
        "请求已到达宿主"
    );
}

/// 发送后响应不可解读（非法 JSON）→ **UnknownDelivery**：POST 已到达宿主
/// （unary_hits ≥ 1），可能已被消费——不得记明确失败（复审 P1-2 原文场景）。
#[tokio::test]
async fn post_send_garbled_response_is_unknown_delivery_not_failed() {
    let (authority, host) = spawn_fake_host().await;
    host.set_unary(UnaryBehavior::GarbledJson);
    let cookie = host.issue_cookie();
    let channel = channel_to(&authority, 5, &cookie);
    let out = channel
        .send_message(&send_target(), &send_payload("你好\n世界"), "act-1")
        .await;
    assert_eq!(
        out,
        Ok(NativeSendOutcome::UnknownDelivery),
        "发送后响应不可解读 = 结局未知，不是明确失败"
    );
    assert!(host.unary_hits() >= 1, "请求确实已交出宿主");
}

/// 发送后 rpcId 身份闸失败 / 响应超限 / 二进制信封缺失 → 同为 UnknownDelivery。
#[tokio::test]
async fn post_send_rpc_mismatch_oversize_and_binary_are_unknown_delivery() {
    let (authority, host) = spawn_fake_host().await;
    let cookie = host.issue_cookie();
    let channel = channel_to(&authority, 5, &cookie);

    host.set_unary(UnaryBehavior::RpcMismatch);
    let out = channel
        .send_message(&send_target(), &send_payload("a"), "act-1")
        .await;
    assert_eq!(
        out,
        Ok(NativeSendOutcome::UnknownDelivery),
        "rpcId 不匹配 = 未知"
    );

    host.set_unary(UnaryBehavior::DeclaredOversize);
    let out = channel
        .send_message(&send_target(), &send_payload("b"), "act-2")
        .await;
    assert_eq!(
        out,
        Ok(NativeSendOutcome::UnknownDelivery),
        "响应超限 = 未知"
    );

    host.set_unary(UnaryBehavior::Binary);
    let out = channel
        .send_message(&send_target(), &send_payload("c"), "act-3")
        .await;
    assert_eq!(
        out,
        Ok(NativeSendOutcome::UnknownDelivery),
        "二进制响应不硬解 = 未知"
    );
}

/// 发送前失败与宿主闸门拒绝 → **明确 Err**（确定未执行——记 Failed 是安全的）：
/// a) 请求体编码超限（>2MiB）：零字节出门，宿主零命中；
/// b) 宿主 401：方法未运行。
#[tokio::test]
async fn pre_send_failures_and_gate_refusals_are_explicit_errors() {
    let (authority, host) = spawn_fake_host().await;
    let cookie = host.issue_cookie();
    let channel = channel_to(&authority, 5, &cookie);

    // a) 编码超限：本地拒绝，宿主零命中。
    let big = "x".repeat(transport::MAX_JSON_FRAME_BYTES + 64);
    let err = channel
        .send_message(&send_target(), &send_payload(&big), "act-big")
        .await
        .expect_err("发送前失败必须上抛（不是未知投递）");
    assert_eq!(err.code, codes::FRAME_OVERSIZE, "实际：{err:?}");
    assert_eq!(host.unary_hits(), 0, "超限请求零字节出门——宿主不得被触碰");

    // b) 宿主闸门 401：方法未执行——明确失败。
    host.set_unary(UnaryBehavior::Unauthorized);
    let err = channel
        .send_message(&send_target(), &send_payload("hi"), "act-401")
        .await
        .expect_err("闸门拒绝必须上抛（不是未知投递）");
    assert_eq!(err.code, codes::PEER_UNAUTHORIZED, "实际：{err:?}");
    assert!(
        host.unary_hits() >= 1,
        "请求到达了宿主闸门（被拒于方法运行前）"
    );
}

/// P1-1a：长生命周期归属连接——多次发送**共用一条 socket**（ws_attempts == 1；
/// T07 协议事实：cookie 与一条物理 socket 生死绑定，per-message 建连必然 409/401）。
#[tokio::test]
async fn long_lived_channel_reuses_one_socket_across_sends() {
    let (authority, host) = spawn_fake_host().await;
    let cookie = host.issue_cookie();
    let channel = channel_to(&authority, 5, &cookie);

    for i in 0..3 {
        let out = channel
            .send_message(
                &send_target(),
                &send_payload(&format!("第 {i} 条")),
                &format!("act-{i}"),
            )
            .await
            .expect("发送成功");
        assert!(
            matches!(out, NativeSendOutcome::Accepted { .. }),
            "第 {i} 条应受理：{out:?}"
        );
    }
    assert_eq!(host.unary_hits(), 3, "三次一元请求");
    assert_eq!(
        host.ws_attempts(),
        1,
        "全部写共用一条归属 socket（第二条 socket 会被宿主 409）"
    );
    assert_eq!(host.live_ws(), 1, "连接长持有，不中途关闭");
}

// ============================================================
// M6-e / M6-f（复审收口）：死连接重建语义 + 相位判别测试
// ============================================================

/// M6-e：宿主断开归属 socket → 下一次发送**重建尝试**（ws_attempts==2），同
/// cookie 升级被宿主 401 拒绝（协议事实：socket 关即 cookie 终态）——如实上抛
/// 明确未执行，不是未知投递。
#[tokio::test]
async fn dead_connection_is_rebuilt_and_same_cookie_401_is_honest() {
    let (authority, host) = spawn_fake_host().await;
    let cookie = host.issue_cookie();
    let channel = channel_to(&authority, 5, &cookie);

    let out = channel
        .send_message(&send_target(), &send_payload("一"), "act-1")
        .await
        .expect("首发成功");
    assert!(matches!(out, NativeSendOutcome::Accepted { .. }));
    assert_eq!(host.ws_attempts(), 1, "首发恰一条归属 socket");

    host.kill_ws();
    assert!(
        host.wait_until_no_live_ws(Duration::from_secs(5)).await,
        "宿主侧 socket 应已断开"
    );

    let err = channel
        .send_message(&send_target(), &send_payload("二"), "act-2")
        .await
        .expect_err("同 cookie 重建必须被宿主 401 拒绝（终态）");
    assert_eq!(err.code, codes::PEER_UNAUTHORIZED, "实际：{err:?}");
    assert_eq!(host.ws_attempts(), 2, "确实做了重建尝试（不是无脑失败）");
}

/// M6-f：timeout/5xx/3xx 在 `request_classified` 层的相位判别（变异 f 的收口
/// ——timeout 误标 NotExecuted 时本用例必须红）。
#[tokio::test]
async fn classified_timeout_5xx_and_redirect_phases() {
    use multi_agents_manager_lib::services::dsh_control::transport::UnaryFailure;

    let (authority, host) = spawn_fake_host().await;
    let cookie = host.issue_cookie();
    // 短超时配置直连（信道用默认 10s——等不起）。
    let conn = transport::connect_with(
        peer_to(&authority, 5, &cookie),
        TransportConfig {
            unary_timeout: Duration::from_millis(300),
        },
    )
    .await
    .expect("连接");

    // a) 等响应超时：请求已交出 → 未知（不自动重发）。
    host.set_unary(UnaryBehavior::Hang);
    let err = conn
        .request_classified("r-1", "session/prompt", &serde_json::json!({}))
        .await
        .expect_err("超时必须失败");
    match &err {
        UnaryFailure::DeliveryUnknown(e) => assert_eq!(e.code, codes::WRITE_TIMEOUT),
        other => panic!("超时必须是 DeliveryUnknown：{other:?}"),
    }

    // b) 5xx：处理器可能已运行 → 保守未知。
    host.set_unary(UnaryBehavior::InternalError);
    let err = conn
        .request_classified("r-2", "session/prompt", &serde_json::json!({}))
        .await
        .expect_err("5xx 必须失败");
    match &err {
        UnaryFailure::DeliveryUnknown(e) => assert_eq!(e.code, codes::CONNECT_FAILED),
        other => panic!("5xx 必须是 DeliveryUnknown：{other:?}"),
    }

    // c) 3xx：路由层（工程假定：方法未运行）→ 未执行。
    host.set_unary(UnaryBehavior::Redirect("http://elsewhere/x".into()));
    let err = conn
        .request_classified("r-3", "session/prompt", &serde_json::json!({}))
        .await
        .expect_err("3xx 必须失败");
    match &err {
        UnaryFailure::NotExecuted(e) => assert_eq!(e.code, codes::REDIRECT_BLOCKED),
        other => panic!("3xx 必须是 NotExecuted：{other:?}"),
    }
    conn.close().await;
}

// ============================================================
// P1-B（复审）：真实效果闭环——效果源（session/follow user/message ↔ requestId）
// 确认受理动作 → record_effect 自动推进下一条排队消息（零手工注入）。
// ============================================================

use multi_agents_manager_lib::database as dsh_db;
use multi_agents_manager_lib::services::dsh_control::actions::{
    ContractReadProbe, EffectSource, PeerEffectSource,
};
use multi_agents_manager_lib::services::dsh_control::contract::{
    ContractReadReport, CONTRACT_RULE_VERSION,
};
use multi_agents_manager_lib::services::dsh_control::model::{
    ContractEvidence, DeviceContext, LocalHost, Operation, OperationContractMatch, SendPayload,
    Target,
};
use multi_agents_manager_lib::services::dsh_control::{DshControlHub, DshDb};

fn seeded_report(match_send: bool) -> ContractReadReport {
    let mut matches = std::collections::BTreeMap::new();
    if match_send {
        matches.insert(
            Operation::Send,
            OperationContractMatch {
                method: "session/prompt".into(),
                structural_rule: "structural".into(),
                semantic_rule: "semantic".into(),
            },
        );
    }
    ContractReadReport {
        evidence: ContractEvidence {
            package_sha256: "fp-loop-test".into(),
            rule_version: CONTRACT_RULE_VERSION,
            matches,
        },
        inconclusive: Default::default(),
        unresolved: Default::default(),
        budget_limited: false,
    }
}

thread_local! {
    /// 闭环测试的包束根（lazy 建——loop_host 需要真实 executable 让 stat 有效）。
    static LOOP_BUNDLE: std::path::PathBuf = {
        let root = tempfile::tempdir().expect("loop bundle");
        let contents = root.path().join("Dsh.app/Contents");
        std::fs::create_dir_all(contents.join("MacOS")).expect("MacOS");
        std::fs::create_dir_all(contents.join("Resources")).expect("Resources");
        std::fs::write(contents.join("MacOS/exec"), b"binary").expect("exec");
        std::fs::write(contents.join("Resources/app.asar"), b"asar").expect("asar");
        let exec = contents.join("MacOS/exec");
        std::mem::forget(root); // 生命周期交测试进程
        exec
    };
}

fn loop_host(authority: &str) -> LocalHost {
    LocalHost {
        host_id: "host-under-test".into(),
        platform: "macos".into(),
        profile_dir: Default::default(),
        // P1（第六轮复审）：真实 executable → stat 快照有效（门可评）。
        executable: LOOP_BUNDLE.with(|p| p.clone()),
        pid: 1,
        started_at_ms: 1,
        epoch: 5,
        authority: authority.to_string(),
        package_sha256: String::new(),
    }
}

fn loop_payload(text: &str) -> SendPayload {
    SendPayload {
        text: text.into(),
        receipt_ids: Vec::new(),
        queue_only: false,
        bypass_paused_queue: false,
    }
}

/// 全闭环：m1 发送受理 → follow 流回带 requestId 的 user/message → watcher
/// record_effect → 泵自动真发 m2（不调用 record_effect/advance 任何手工入口）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn effect_stream_confirms_accept_and_pumps_the_queue() {
    let (authority, host) = spawn_fake_host().await;
    let first_effect = WsRail::new();
    let second_effect = WsRail::new();
    // 每个订阅都先回官方 snapshot；后续订阅再独立扣住 act-2 的增量事件。
    // 首个订阅只回 snapshot，避免第二个闸挡住单 socket 接收 m2 的 open。
    let snapshot = WsStep::Item(json!({
        "type": "snapshot",
        "header": {"version": 4, "id": "s-loop", "createdAt": 1, "isSeeded": false},
        "cursor": 6,
        "records": [
            {"type": "event", "event": {"type": "user/message", "seq": 6, "time": 1,
                "data": {"content": "第一条", "source": {"kind": "user", "rpcId": "act-1"}}}},
        ],
        "hasMore": false,
    }));
    host.set_first_ws_steps(vec![
        WsStep::HoldRail(first_effect.clone()),
        snapshot.clone(),
    ]);
    host.set_ws_steps(vec![
        snapshot,
        WsStep::HoldRail(second_effect.clone()),
        WsStep::Item(json!({
            "type": "event",
            "event": {"type": "user/message", "seq": 8, "time": 2,
                "data": {"content": "第二条", "source": {"kind": "user", "rpcId": "act-2"}}},
        })),
    ]);
    let cookie = host.issue_cookie();
    let peer = peer_to(&authority, 5, &cookie);
    let channel = PeerChannel::from_static_peer(peer);

    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("tuvis.db");
    {
        let conn = rusqlite::Connection::open(&db_path).expect("建库");
        dsh_db::schema::init(&conn).unwrap();
    }
    let hub_db = rusqlite::Connection::open(&db_path).expect("hub conn");

    // 效果源与原生发送**共享同一** PeerChannel（一 Peer 一 socket——第二条
    // socket 会被宿主 409；本测试最初就因此失败，实证架构约束）。
    let effects: std::sync::Arc<dyn EffectSource> =
        std::sync::Arc::new(PeerEffectSource(std::sync::Arc::clone(&channel)));
    let read: ContractReadProbe = std::sync::Arc::new(move |_h| Ok(seeded_report(true)));
    let auth_for_probe = authority.clone();
    let probe = std::sync::Arc::new(move || Ok(loop_host(&auth_for_probe)));

    // I4③：新写入口的会话归属门（绑定传入实例+会话；epoch 与 loop_host 的 5 对齐）。
    let resolver: multi_agents_manager_lib::services::dsh_control::SessionResolver =
        std::sync::Arc::new(|instance_id: &str, session_id: &str| {
            Ok(
                multi_agents_manager_lib::services::dsh_control::model::Target {
                    host_id: instance_id.to_string(),
                    epoch: 5,
                    session_id: session_id.to_string(),
                },
            )
        });
    let hub = DshControlHub::with_pipeline(
        DshDb::Owned(std::sync::Arc::new(std::sync::Mutex::new(hub_db))),
        std::sync::Arc::clone(&channel) as std::sync::Arc<dyn NativeDispatch>,
        probe,
        read,
        Some(effects),
        Some(resolver),
    );
    hub.open_write_gate();
    let device = DeviceContext {
        device_id: "dev-1".into(),
        device_name: "Dev".into(),
    };

    // m1：受理（一元 #1）。watcher 随后订阅 follow（WS open → 假宿主回实帧）。
    let r1 = hub
        .dispatch_session_send_for_instance(
            device.clone(),
            "s-loop",
            "act-1",
            loop_payload("第一条"),
            true,
            Some("host-under-test"),
        )
        .await
        .expect("m1");
    assert_eq!(
        r1.0.state,
        multi_agents_manager_lib::services::dsh_control::model::ActionState::Accepted
    );
    first_effect.wait_for_entry().await;

    // 假宿主已收到 follow，但先扣住效果：m2 必须因 m1 尚未确认而排队。
    let r2 = hub
        .dispatch_session_send_for_instance(
            device.clone(),
            "s-loop",
            "act-2",
            loop_payload("第二条"),
            true,
            Some("host-under-test"),
        )
        .await
        .expect("m2");
    assert_eq!(
        r2.0.state,
        multi_agents_manager_lib::services::dsh_control::model::ActionState::Queued
    );

    assert_eq!(host.unary_hits(), 1, "放行前宿主只应收到 m1");
    assert_eq!(host.unary_bodies()[0]["rpcId"], "act-1");

    // m3 同时排队：只有泵路径受理的 m2 也挂 watcher，act-2 效果才会推进 m3。
    let r3 = hub
        .dispatch_session_send_for_instance(
            device.clone(),
            "s-loop",
            "act-3",
            loop_payload("第三条"),
            true,
            Some("host-under-test"),
        )
        .await
        .expect("m3");
    assert_eq!(
        r3.0.state,
        multi_agents_manager_lib::services::dsh_control::model::ActionState::Queued
    );
    assert_eq!(host.unary_hits(), 1, "m3 入队也不得绕过 m1");

    first_effect.release();
    // watcher 仅在 m2 受理落账后订阅。此时 act-2 的效果仍被宿主扣住。
    second_effect.wait_for_entry().await;
    let assert_rpc_ids = |expected: &[Option<&str>]| {
        let bodies = host.unary_bodies();
        let actual: Vec<_> = bodies.iter().map(|body| body["rpcId"].as_str()).collect();
        assert_eq!(actual, expected);
    };
    assert_eq!(host.unary_hits(), 2, "放行 act-2 效果前只应发送前两条");
    assert_rpc_ids(&[Some("act-1"), Some("act-2")]);
    assert_eq!(
        hub.action(&device, "act-2")
            .await
            .expect("读 m2")
            .receipt
            .state,
        multi_agents_manager_lib::services::dsh_control::model::ActionState::Accepted
    );
    assert_eq!(
        hub.action(&device, "act-3")
            .await
            .expect("读 m3")
            .receipt
            .state,
        multi_agents_manager_lib::services::dsh_control::model::ActionState::Queued,
        "act-3 不得越过未确认的 act-2"
    );

    second_effect.release();
    second_effect.wait_for_entry().await; // m3 已受理，来自第三个订阅的信号
    assert_rpc_ids(&[Some("act-1"), Some("act-2"), Some("act-3")]);
    assert_eq!(host.unary_hits(), 3, "三条请求必须按顺序各发送一次");

    let m1 = hub.action(&device, "act-1").await.expect("读 m1");
    assert_eq!(
        m1.receipt.state,
        multi_agents_manager_lib::services::dsh_control::model::ActionState::Succeeded,
        "m1 由事件流效果提升为成功"
    );
    assert_eq!(
        m1.evidence.as_ref().map(|e| e.effect_kind),
        Some(multi_agents_manager_lib::services::dsh_control::model::EffectKind::Message)
    );
    let m2 = hub.action(&device, "act-2").await.expect("读 m2");
    assert_eq!(
        m2.receipt.state,
        multi_agents_manager_lib::services::dsh_control::model::ActionState::Succeeded,
        "m2 被 act-2 事件确认"
    );
    let m3 = hub.action(&device, "act-3").await.expect("读 m3");
    assert_eq!(
        m3.receipt.state,
        multi_agents_manager_lib::services::dsh_control::model::ActionState::Accepted,
        "m3 受理（等待 act-3 的确认事件——本测试未下发，收刈器兜底）"
    );

    drop(channel);
}

/// 缺少受理字段时账本记录未知结局，同一载荷和附件可用原 ID 幂等重试。
#[tokio::test]
async fn missing_accepted_preserves_unknown_ledger_and_idempotent_retry() {
    assert_unknown_ledger_retry(UnaryBehavior::AcceptedMissing).await;
}

/// 响应断线后同 ID 重试复用未知结局，不得再次命中宿主。
#[tokio::test]
async fn post_write_disconnect_retry_does_not_repost() {
    assert_unknown_ledger_retry(UnaryBehavior::DisconnectResponse).await;
}

async fn assert_unknown_ledger_retry(behavior: UnaryBehavior) {
    use multi_agents_manager_lib::services::dsh_control::model::ActionState;
    let (authority, host) = spawn_fake_host().await;
    host.set_unary(behavior);
    let channel = channel_to(&authority, 5, &host.issue_cookie());
    let tmp = tempfile::tempdir().expect("临时库");
    let db_path = tmp.path().join("tuvis.db");
    let conn = rusqlite::Connection::open(&db_path).expect("建库");
    dsh_db::schema::init(&conn).unwrap();
    let probe_authority = authority.clone();
    let probe = Arc::new(move || Ok(loop_host(&probe_authority)));
    let read: ContractReadProbe = Arc::new(|_| Ok(seeded_report(true)));
    let resolver: multi_agents_manager_lib::services::dsh_control::SessionResolver =
        Arc::new(|instance_id, session_id| {
            Ok(Target {
                host_id: instance_id.into(),
                epoch: 5,
                session_id: session_id.into(),
            })
        });
    let hub = DshControlHub::with_pipeline(
        DshDb::Owned(Arc::new(Mutex::new(conn))),
        channel as Arc<dyn NativeDispatch>,
        probe,
        read,
        None,
        Some(resolver),
    );
    hub.open_write_gate();
    let device = DeviceContext {
        device_id: "dev-1".into(),
        device_name: "Dev".into(),
    };
    let payload = SendPayload {
        text: "保留草稿".into(),
        receipt_ids: vec!["receipt-1".into()],
        queue_only: false,
        bypass_paused_queue: false,
    };
    let (first, replayed) = hub
        .dispatch_session_send_for_instance(
            device.clone(),
            "s-loop",
            "unknown-missing",
            payload.clone(),
            true,
            Some("host-under-test"),
        )
        .await
        .expect("首次派发");
    let hits_before_retry = host.unary_hits();
    let (retry, replayed_retry) = hub
        .dispatch_session_send_for_instance(
            device.clone(),
            "s-loop",
            "unknown-missing",
            payload.clone(),
            true,
            Some("host-under-test"),
        )
        .await
        .expect("同 ID 重试");
    let view = hub
        .action(&device, "unknown-missing")
        .await
        .expect("读取账本");
    let conn = rusqlite::Connection::open(&db_path).expect("读取身份摘要");
    let identity: Option<String> = conn
        .query_row(
            "SELECT payload_identity FROM dsh_actions WHERE action_id = ?1",
            ["unknown-missing"],
            |row| row.get(0),
        )
        .expect("账本行");
    assert_eq!(
        (
            first.state,
            retry.state,
            view.receipt.state,
            replayed,
            replayed_retry,
            host.unary_hits()
        ),
        (
            ActionState::UnknownDelivery,
            ActionState::UnknownDelivery,
            ActionState::UnknownDelivery,
            false,
            true,
            1
        )
    );
    assert_eq!(hits_before_retry, 1, "首次派发必须命中宿主一次");
    assert_eq!(
        host.unary_hits(),
        hits_before_retry,
        "同 ID 重试不得增加宿主命中"
    );
    assert!(identity.is_some(), "账本保留载荷身份以核验幂等重试");
    assert_eq!(payload.text, "保留草稿");
    assert_eq!(payload.receipt_ids, vec!["receipt-1"]);
    let args = prompt_args_of(&host.unary_bodies(), "unknown-missing");
    assert_eq!(
        args["request"]["content"],
        json!([{"type":"file","receiptId":"receipt-1"},{"type":"text","text":"保留草稿"}])
    );
}

/// 收刈兜底：accepted 超过确认预算 → unknown_delivery（dsh_effect_unconfirmed），
/// 被释放会话的排队消息随后可推进（这里只验收口本身）。
#[tokio::test]
async fn stale_accepted_is_reaped_to_unknown_delivery() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let conn = rusqlite::Connection::open(tmp.path().join("t.db")).expect("库");
    dsh_db::schema::init(&conn).unwrap();
    conn.execute(
        "INSERT INTO dsh_actions (seq, action_id, device_id, device_name, host_id, epoch, \
         session_id, operation, payload_hash, state, created_at_ms, updated_at_ms) \
         VALUES (1,'stale-1','dev-1','Dev','local','1','s','send','h','accepted',1,1)",
        [],
    )
    .expect("插入旧 accepted 行");
    let store = multi_agents_manager_lib::services::dsh_control::store::DshActionStore::new(&conn);
    let reaped = store.reap_unconfirmed_accepted(1000).expect("收刈");
    assert_eq!(reaped, 1);
    let state: String = conn
        .query_row(
            "SELECT state FROM dsh_actions WHERE action_id='stale-1'",
            [],
            |r| r.get(0),
        )
        .expect("读回");
    assert_eq!(state, "unknown_delivery");
    let reason: String = conn
        .query_row(
            "SELECT reason_code FROM dsh_actions WHERE action_id='stale-1'",
            [],
            |r| r.get(0),
        )
        .expect("读回原因");
    assert_eq!(reason, "dsh_effect_unconfirmed");
}

// ============================================================
// P1-c（第四轮复审）：**连接身份与动作 target 核对**——旧 target（换代前的
// host/epoch）在新代际连接上发送必须被拒（明确未执行），不跨代误投。
// ============================================================

#[tokio::test]
async fn cross_generation_target_is_refused_before_write() {
    let (authority, host) = spawn_fake_host().await;
    let cookie = host.issue_cookie();
    // 信道建连在**新代际**（epoch=9）。
    let channel = channel_to(&authority, 9, &cookie);

    // 旧代际 target（epoch=5）——动作等待信道锁期间宿主换代的形态。
    let stale_target = Target {
        host_id: "host-under-test".into(),
        epoch: 5,
        session_id: "s-1".into(),
    };
    let err = channel
        .send_message(&stale_target, &send_payload("hi"), "act-stale")
        .await
        .expect_err("跨代 target 必须被拒（明确未执行）");
    assert_eq!(err.code, codes::HOST_NOT_RUNNING, "实际：{err:?}");
    assert_eq!(host.unary_hits(), 0, "核对在写出之前——宿主零命中（零误投）");

    // 当代 target 正常发送（与信道同代际 epoch=9）。
    let fresh_target = Target {
        host_id: "host-under-test".into(),
        epoch: 9,
        session_id: "s-1".into(),
    };
    let out = channel
        .send_message(&fresh_target, &send_payload("hi"), "act-fresh")
        .await
        .expect("当代 target 正常");
    assert!(matches!(out, NativeSendOutcome::Accepted { .. }));
    assert_eq!(host.unary_hits(), 1);
}

// ============================================================
// 2026-11-16 宿主 session/prompt schema 漂移回归（真机发现③）：
// 唯一实证 wire 契约（2026-11-17 P1-1 收窄）：session/prompt 参数 = 单一
// `request` 对象（typert.host.js SessionController 实读；形状分发整链已删）。
// ============================================================

/// 提取一条 session/prompt 请求的 args（信封形态：{type:"client-request",
/// rpcId, method, payload:{args}}）。
fn prompt_args_of(bodies: &[Value], rpc_id: &str) -> Value {
    bodies
        .iter()
        .find(|b| {
            b.get("method").and_then(Value::as_str) == Some("session/prompt")
                && b.get("rpcId").and_then(Value::as_str) == Some(rpc_id)
        })
        .map(|b| b["payload"]["args"].clone())
        .unwrap_or_else(|| panic!("no session/prompt request with rpcId {rpc_id} captured"))
}

/// 发出的 session/prompt args 必须是 `request` 包装：附件 receipt 块迁入
/// request.content（receipts 在前、text 收尾）；rpcId 归因仍由传输信封携带
/// （= actionId）。
#[tokio::test]
async fn channel_sends_request_wrapped_prompt_with_receipt_blocks() {
    let (authority, host) = spawn_fake_host().await;
    let cookie = host.issue_cookie();
    let channel = PeerChannel::from_static_peer(peer_to(&authority, 5, &cookie));
    let target = Target {
        host_id: "host-under-test".into(),
        epoch: 5,
        session_id: "session-aed8892b".into(),
    };
    let payload = SendPayload {
        text: "第一条".into(),
        receipt_ids: vec!["rc-9".into()],
        queue_only: false,
        bypass_paused_queue: false,
    };
    let outcome = channel
        .send_message(&target, &payload, "act-acp")
        .await
        .expect("发送受理");
    assert!(matches!(outcome, NativeSendOutcome::Accepted { .. }));

    let args = prompt_args_of(&host.unary_bodies(), "act-acp");
    let keys: Vec<&str> = args
        .as_object()
        .expect("args object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, vec!["request"], "顶层恰为 request 包装键：{args}");
    let request = &args["request"];
    assert_eq!(request["requestId"], "act-acp");
    assert_eq!(request["sessionId"], "session-aed8892b");
    assert_eq!(request["mode"], "queue");
    assert_eq!(
        request["content"],
        json!([
            {"type": "file", "receiptId": "rc-9"},
            {"type": "text", "text": "第一条"},
        ]),
        "附件 receipt 块与文本块迁入 request.content；实际 args：{args}"
    );
}

/// 默认信道（静态句柄构造）：纯文本载荷同样走 request 包装不回退。
#[tokio::test]
async fn default_channel_sends_request_wrapped_text_payload() {
    let (authority, host) = spawn_fake_host().await;
    let cookie = host.issue_cookie();
    let channel = PeerChannel::from_static_peer(peer_to(&authority, 5, &cookie));
    let target = Target {
        host_id: "host-under-test".into(),
        epoch: 5,
        session_id: "session-aed8892b".into(),
    };
    let outcome = channel
        .send_message(&target, &send_payload("hi"), "act-legacy")
        .await
        .expect("发送受理");
    assert!(matches!(outcome, NativeSendOutcome::Accepted { .. }));

    let args = prompt_args_of(&host.unary_bodies(), "act-legacy");
    let request = &args["request"];
    assert_eq!(request["requestId"], "act-legacy");
    assert_eq!(request["sessionId"], "session-aed8892b");
    assert_eq!(request["mode"], "queue");
    assert_eq!(
        request["content"],
        json!([{"type": "text", "text": "hi"}]),
        "request.content 四字段恒在 request 对象内：{args}"
    );
}
