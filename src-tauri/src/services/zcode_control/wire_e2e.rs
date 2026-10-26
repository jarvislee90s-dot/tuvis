//! T8.1 生产链假服务（HTTP 切片）：真实回环 axum 端点，替代进程内端口假体。
//!
//! 本文件只承载 `#[cfg(test)]` 测试：在 `127.0.0.1:0` 起一个**真实 HTTP 服务**，
//! 让生产 [`AuthService`] 走完「等授权回调 → 交换账号 JWT → 申请终端 token」的
//! 完整 HTTP 路径（reqwest 序列化、状态码与响应体解析都按生产代码跑），
//! 而不是在 trait 端口边界做替身。
//!
//! 合成值纪律：JWT 为标准三段式 `header.payload.sig`，payload 只有远期 `exp`；
//! 全部取值均为合成字面量，不读真实账号/DB/外网。

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use axum::routing::post;
    use axum::{Json, Router};
    use base64::Engine as _;
    use futures::{SinkExt, StreamExt};
    use tokio::sync::mpsc;
    use tokio_tungstenite::tungstenite::Message;

    use super::super::auth::{
        AuthService, OAuthProvider, OpenAuthorize, OAUTH_TOKEN_PATH, RELAY_TOKEN_PATH,
    };
    use super::super::bridge::{
        BridgeManager, BridgeTiming, AGENT_CLIENT_HELLO_METHOD, AGENT_HELLO_METHOD,
        AGENT_SUBSCRIBE_CONVERSATION_METHOD, BOOTSTRAP_REQUEST, CONTROLLER_SUBSCRIBE_METHOD,
        CONTROLLER_TASKS_INDEX_TOPIC, INITIALIZE_BYTES, V4_WIRE_PROTOCOL_VERSION,
        WORKSPACE_BRIDGE_OPEN, WORKSPACE_BRIDGE_READY, WORKSPACE_LIST_REQUEST,
    };
    use super::super::codec::{
        encode_frame, encode_response_for_test, RpcIdentity, REQUEST_TYPE_EVENT_LISTEN,
        REQUEST_TYPE_PROMISE, RESPONSE_TYPE_EVENT_FIRE,
    };
    use super::super::relay::{DeviceRoute, RelayClient, RelayEvent};
    use super::super::types::{ConnectionState, TargetKey, ZResult};

    /// 合成账号 JWT：`header.payload.sig`，payload = `{"exp":9999999999}`（base64url）。
    const SYNTHETIC_JWT: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJleHAiOjk5OTk5OTk5OTl9.sig";
    /// 合成终端 relay token。
    const SYNTHETIC_TERMINAL_TOKEN: &str = "synthetic-terminal";
    /// 终端 token 有效期（秒）。
    const SYNTHETIC_EXPIRES_IN: u64 = 60;

    /// 回环假官方端：生命周期跟随测试（Drop 即中止 serve 任务）。
    struct FakeOfficialHttp {
        origin: String,
        task: tokio::task::JoinHandle<()>,
    }

    impl FakeOfficialHttp {
        async fn start() -> Self {
            let app = Router::new()
                .route(OAUTH_TOKEN_PATH, post(oauth_token))
                .route(RELAY_TOKEN_PATH, post(terminal_token));
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .expect("回环端口应可绑定");
            let port = listener.local_addr().expect("回环地址应可读").port();
            let task = tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            Self {
                origin: format!("http://127.0.0.1:{port}"),
                task,
            }
        }
    }

    impl Drop for FakeOfficialHttp {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    /// `POST /api/v1/oauth/token`：只回 `data.token`（账号 JWT）。
    async fn oauth_token() -> Json<serde_json::Value> {
        Json(serde_json::json!({ "data": { "token": SYNTHETIC_JWT } }))
    }

    /// `POST /api/v1/remote/token`：只回 `data.token` 与正整数 `data.expires_in`。
    async fn terminal_token() -> Json<serde_json::Value> {
        Json(serde_json::json!({
            "data": { "token": SYNTHETIC_TERMINAL_TOKEN, "expires_in": SYNTHETIC_EXPIRES_IN },
        }))
    }

    /// 回环 WS 握手假体句柄：`origin` 为客户端应注入的 ws 源（`ws://127.0.0.1:<port>`），
    /// `accepted()` 为 TCP 层已接受的连接数（断言「恰好一条 socket」与「断开后零重连」），
    /// `echoes()` 为已回显的数据面帧数（断言「一帧一次、不重复」），
    /// `log()` 为收到的 app 业务请求的顺序记录：`zcode_type` 业务请求记其类型，
    /// `rpc-frame` 记解出的 channel 方法名，两者**混在同一序列**。
    struct FakeWsHandshake {
        origin: String,
        accepted: Arc<AtomicUsize>,
        echoes: Arc<AtomicUsize>,
        log: Arc<Mutex<Vec<String>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl FakeWsHandshake {
        fn accepted(&self) -> usize {
            self.accepted.load(Ordering::SeqCst)
        }

        fn echoes(&self) -> usize {
            self.echoes.load(Ordering::SeqCst)
        }

        /// 已收到的 app 业务请求记录（按到达顺序）：`data` 信封的业务请求记
        /// `zcode_type`，`rpc-frame` 记解出的 channel 方法名；两者混在同一序列。
        fn log(&self) -> Vec<String> {
            self.log.lock().expect("app 请求日志锁").clone()
        }
    }

    impl Drop for FakeWsHandshake {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    /// 最小回环 WS 握手假体：循环接受连接并计数；每条连接读首个 JSON `auth_init` 取
    /// `device_sid` → 回同 `device_sid` + `pair_status:"matched"` 的 `auth_ack`
    /// （配对即刻完成，无等待窗）；之后对每条 `pair_status_query` 回
    /// `pair_status_ack("matched")`。
    ///
    /// `data` 信封内的 app 业务请求按 `zcode_type` 分派并**逐条记账**：
    /// `bootstrap-request` / `workspace-list-request` 回合成 listing（回显 `requestId`）；
    /// `workspace-bridge-open` 先回 `workspace-bridge-ready`（只回显请求声明过的身份），
    /// 紧接着推该桥 ChannelServer 的 `Initialize(200)`（每桥首帧 seq=1 / messageSeq=1）；
    /// 入站 `rpc-frame` 解出 channel 方法后记账方法名，并逐条回 201
    /// （`helloConversationV4` / `initializeConversationV4` / 两个订阅）。
    /// 其余 JSON 文本帧（数据面 `data` 探针）**原样回显一次**（不改写、不重复）；
    /// socket 保持到对端关闭。
    ///
    /// 返回 [`FakeWsHandshake`]：`origin` 直接作为 `RelayClient` 端点源注入
    /// （路径仍由生产代码拼 `/api/v1/remote/ws2`）。合成值纪律同本文件：不读真实
    /// 账号/DB/外网；解析不了的帧一律忽略。
    fn spawn_ws_handshake(listener: tokio::net::TcpListener) -> FakeWsHandshake {
        let origin = format!(
            "ws://{}",
            listener.local_addr().expect("回环 WS 地址应可读")
        );
        let accepted = Arc::new(AtomicUsize::new(0));
        let counter = accepted.clone();
        let echoes = Arc::new(AtomicUsize::new(0));
        let echo_counter = echoes.clone();
        let log = Arc::new(Mutex::new(Vec::new()));
        let app_log = log.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(serve_ws_handshake(
                    stream,
                    echo_counter.clone(),
                    app_log.clone(),
                ));
            }
        });
        FakeWsHandshake {
            origin,
            accepted,
            echoes,
            log,
            task,
        }
    }

    /// `data` 信封内的 app 业务载荷（握手/心跳帧返回 `None`）。
    fn app_payload(value: &serde_json::Value) -> Option<&serde_json::Value> {
        if value.get("type").and_then(|kind| kind.as_str()) != Some("data") {
            return None;
        }
        value.get("payload")
    }

    /// 假体 controller 订阅 ack 的 `subscriptionId`（合成值）。
    const CONTROLLER_SUBSCRIPTION: &str = "sub-controller-synthetic";
    /// 假体会话订阅 ack 的 `subscriptionId`（合成值）。
    const CONVERSATION_SUBSCRIPTION: &str = "sub-conversation-synthetic";
    /// 假体任务索引里的单行任务（与 `TargetKey.session_id` 精确相等）。
    const SYNTHETIC_TASK_ID: &str = "session_synthetic";
    /// 假体 listing / 任务索引共用的 workspace 路径。
    const SYNTHETIC_WORKSPACE_PATH: &str = "/tmp/travel";

    /// 写一个 VQL（7 bit 小端变长）；仅供假体的 204 事件帧成形。
    fn push_vql(out: &mut Vec<u8>, value: u32) {
        let mut value = value;
        loop {
            let mut byte = (value & 0x7f) as u8;
            value >>= 7;
            if value > 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if value == 0 {
                return;
            }
        }
    }

    /// 合成一条事件推送（204）的 wire 字节：header `[204, id]` + JSON body（Object 标签）。
    ///
    /// 生产侧只发请求，204 只出现在假体方向；这里与 [`encode_response_for_test`] 同样
    /// 按官方类型标签成形（不含任何业务改写）。
    fn encode_event_fire(id: u32, body: &serde_json::Value) -> Vec<u8> {
        let text = serde_json::to_vec(body).expect("事件 body 应可序列化");
        let mut out = vec![4, 2, 6];
        push_vql(&mut out, RESPONSE_TYPE_EVENT_FIRE);
        out.push(6);
        push_vql(&mut out, id);
        out.push(5);
        push_vql(&mut out, u32::try_from(text.len()).expect("事件 body 长度"));
        out.extend_from_slice(&text);
        out
    }

    /// 读一个 VQL，返回 `(值, 下一位置)`。
    fn read_vql(bytes: &[u8], mut pos: usize) -> Option<(u32, usize)> {
        let mut value: u64 = 0;
        for shift in (0..35).step_by(7) {
            let byte = *bytes.get(pos)?;
            pos += 1;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return u32::try_from(value).ok().map(|value| (value, pos));
            }
        }
        None
    }

    /// 读一个 Int 标签元素（6 + VQL），返回 `(值, 下一位置)`。
    fn read_tagged_int(bytes: &[u8], pos: usize) -> Option<(u32, usize)> {
        if *bytes.get(pos)? != 6 {
            return None;
        }
        read_vql(bytes, pos + 1)
    }

    /// 读一个 String 标签元素（1 + VQL 长度 + UTF-8），返回 `(文本, 下一位置)`。
    fn read_tagged_text(bytes: &[u8], pos: usize) -> Option<(String, usize)> {
        if *bytes.get(pos)? != 1 {
            return None;
        }
        let (len, next) = read_vql(bytes, pos + 1)?;
        let end = next.checked_add(len as usize)?;
        let text = std::str::from_utf8(bytes.get(next..end)?).ok()?;
        Some((text.to_owned(), end))
    }

    /// 解出 Channel 请求头 `[类型, id, channel, method]`（假体只需 Promise 100）。
    fn request_head(bytes: &[u8]) -> Option<(u32, u32, String, String)> {
        // header 是数组：标签 4 + 元素数。
        if bytes.first() != Some(&4) {
            return None;
        }
        let (_, pos) = read_vql(bytes, 1)?;
        let (message_type, pos) = read_tagged_int(bytes, pos)?;
        let (id, pos) = read_tagged_int(bytes, pos)?;
        let (channel, pos) = read_tagged_text(bytes, pos)?;
        let (method, _) = read_tagged_text(bytes, pos)?;
        Some((message_type, id, channel, method))
    }

    /// 处理一条入站 `rpc-frame`：解 header 记账 channel 方法，再按方法回 201。
    ///
    /// - `helloConversationV4` → `{"kind":"hello","protocolVersion":V4_WIRE_PROTOCOL_VERSION,"connectionId":...}`；
    /// - `initializeConversationV4` → `{}`；
    /// - `subscribeControllerV4` → 订阅 ack，并补一条 controller 任务索引快照（204）；
    /// - `subscribeConversationV4` → 订阅 ack；
    /// - 其余 → 空 body（不猜业务语义）。
    async fn serve_rpc_frame(
        socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        peer: &mut FakePeer,
        log: &Arc<Mutex<Vec<String>>>,
        payload: &serde_json::Value,
    ) {
        let bridge = payload
            .get("bridgeSessionId")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_owned();
        let Some(bytes) = payload
            .get("dataBase64")
            .and_then(|value| value.as_str())
            .and_then(|data| base64::engine::general_purpose::STANDARD.decode(data).ok())
        else {
            return;
        };
        let Some((message_type, id, _channel, method)) = request_head(&bytes) else {
            return;
        };
        match message_type {
            REQUEST_TYPE_PROMISE => {
                log.lock().expect("app 请求日志锁").push(method.clone());
                let body = match method.as_str() {
                    AGENT_HELLO_METHOD => serde_json::json!({
                        "kind": "hello",
                        "protocolVersion": V4_WIRE_PROTOCOL_VERSION,
                        "connectionId": "conn-synthetic",
                    }),
                    AGENT_CLIENT_HELLO_METHOD => serde_json::json!({}),
                    CONTROLLER_SUBSCRIBE_METHOD => {
                        serde_json::json!({"ack": {"subscriptionId": CONTROLLER_SUBSCRIPTION}})
                    }
                    AGENT_SUBSCRIBE_CONVERSATION_METHOD => {
                        serde_json::json!({"ack": {"subscriptionId": CONVERSATION_SUBSCRIPTION}})
                    }
                    _ => serde_json::json!({}),
                };
                let Ok(reply) = encode_response_for_test(id, body) else {
                    return;
                };
                send_data(socket, peer.frame(&bridge, reply)).await;
                if method == CONTROLLER_SUBSCRIBE_METHOD {
                    if let Some(listen) = peer.listen.get(&bridge).copied() {
                        let snapshot = serde_json::json!({
                            "wireVersion": V4_WIRE_PROTOCOL_VERSION,
                            "topic": CONTROLLER_TASKS_INDEX_TOPIC,
                            "subscriptionId": CONTROLLER_SUBSCRIPTION,
                            "payload": {
                                "kind": "snapshot",
                                "snapshot": {"tasks": [{
                                    "address": {
                                        "workspacePath": SYNTHETIC_WORKSPACE_PATH,
                                        "taskId": SYNTHETIC_TASK_ID,
                                    },
                                }]},
                            },
                        });
                        send_data(
                            socket,
                            peer.frame(&bridge, encode_event_fire(listen, &snapshot)),
                        )
                        .await;
                    }
                }
            }
            REQUEST_TYPE_EVENT_LISTEN => {
                // 事件推送按最近的 listen id 路由（controller / conversation 各一）。
                peer.listen.insert(bridge, id);
            }
            _ => {}
        }
    }

    /// 单条回环连接的假体出站账本：每桥物理 `seq`/`messageSeq` 与最近的 EventListen id。
    ///
    /// 官方组建器要求 `seq`/`messageSeq` **逐桥**严格递增，且每桥首帧必须是
    /// Initialize（seq=1 / messageSeq=1），因此计数按 `bridgeSessionId` 分开记。
    #[derive(Default)]
    struct FakePeer {
        seq: HashMap<String, u64>,
        message_seq: HashMap<String, u64>,
        /// bridgeSessionId → 最近一次 `EventListen` 的请求 id（事件推送按它路由）。
        listen: HashMap<String, u32>,
    }

    impl FakePeer {
        /// 编码一条本桥的 rpc-frame（计数逐桥递增，首帧即 seq=1 / messageSeq=1）。
        fn frame(&mut self, bridge: &str, bytes: Vec<u8>) -> serde_json::Value {
            let seq = {
                let counter = self.seq.entry(bridge.to_owned()).or_insert(0);
                *counter += 1;
                *counter
            };
            let message_seq = {
                let counter = self.message_seq.entry(bridge.to_owned()).or_insert(0);
                *counter += 1;
                *counter
            };
            encode_frame(&RpcIdentity::new(bridge), seq, message_seq, &bytes)
                .expect("假体 rpc-frame 应可编码")
        }
    }

    /// 以 `data` 信封写一条业务回执/帧（`payload` 为业务载荷，非整帧）。
    async fn send_data(
        socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        payload: serde_json::Value,
    ) {
        let frame = serde_json::json!({"type": "data", "payload": payload, "client_ts": 0});
        let _ = socket.send(Message::text(frame.to_string())).await;
    }

    /// 合成 listing 回执：回显 `requestId`，单 workspace（`/tmp/travel`，无 identity）。
    fn app_listing_reply(payload: &serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "requestId": payload.get("requestId").cloned().unwrap_or_default(),
            "result": {
                "activeWorkspaceKey": "ws-synthetic",
                "workspaces": [{
                    "workspacePath": SYNTHETIC_WORKSPACE_PATH,
                    "workspaceIdentity": null,
                }],
            },
        })
    }

    /// 合成 `workspace-bridge-ready`：回显 requestId / bridgeSessionId / workspaceKey。
    ///
    /// 请求未声明 `bridgeGeneration`/`recoveryId`，ready 也**不得**带这两个字段
    /// （生产对多余身份字段 fail-closed）。
    fn app_bridge_ready(payload: &serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "zcode_type": WORKSPACE_BRIDGE_READY,
            "requestId": payload.get("requestId").cloned().unwrap_or_default(),
            "bridgeSessionId": payload.get("bridgeSessionId").cloned().unwrap_or_default(),
            "workspaceKey": payload.get("workspaceKey").cloned().unwrap_or_default(),
        })
    }

    /// 单条回环 WS 连接的握手/心跳应答（配对即刻完成）、app 请求分派与数据面回显。
    async fn serve_ws_handshake(
        stream: tokio::net::TcpStream,
        echoes: Arc<AtomicUsize>,
        log: Arc<Mutex<Vec<String>>>,
    ) {
        let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
            return;
        };
        let mut device_sid: Option<String> = None;
        let mut peer = FakePeer::default();
        while let Some(frame) = socket.next().await {
            let Ok(frame) = frame else {
                break;
            };
            // 文本帧保留原文用于回显；解析不了的帧一律忽略（与官方客户端一致）。
            let (value, raw_text) = match frame {
                Message::Close(_) => break,
                Message::Text(text) => (
                    serde_json::from_str::<serde_json::Value>(text.as_str()).ok(),
                    Some(text),
                ),
                Message::Binary(bytes) => (
                    serde_json::from_slice::<serde_json::Value>(bytes.as_ref()).ok(),
                    None,
                ),
                _ => (None, None),
            };
            let Some(value) = value else {
                continue;
            };
            // 握手完成后的 app 业务请求：`rpc-frame` 解方法名并逐条回 201；
            // bootstrap/workspace-list 回合成 listing；workspace-bridge-open 回 ready
            // 并紧接着推该桥的 Initialize(200)（官方在 200 之前不发任何请求）。
            if device_sid.is_some() {
                if let Some(payload) = app_payload(&value) {
                    let zcode_type = payload.get("zcode_type").and_then(|kind| kind.as_str());
                    if zcode_type == Some("rpc-frame") {
                        serve_rpc_frame(&mut socket, &mut peer, &log, payload).await;
                        continue;
                    }
                    if let Some(zcode_type) = zcode_type {
                        log.lock()
                            .expect("app 请求日志锁")
                            .push(zcode_type.to_owned());
                        let reply = match zcode_type {
                            BOOTSTRAP_REQUEST | WORKSPACE_LIST_REQUEST => {
                                Some(app_listing_reply(payload))
                            }
                            WORKSPACE_BRIDGE_OPEN => Some(app_bridge_ready(payload)),
                            _ => None,
                        };
                        if let Some(reply) = reply {
                            send_data(&mut socket, reply).await;
                            if zcode_type == WORKSPACE_BRIDGE_OPEN {
                                let bridge = payload
                                    .get("bridgeSessionId")
                                    .and_then(|value| value.as_str())
                                    .unwrap_or_default();
                                let initialize = peer.frame(bridge, INITIALIZE_BYTES.to_vec());
                                send_data(&mut socket, initialize).await;
                            }
                        }
                        continue;
                    }
                }
            }
            match value.get("type").and_then(|kind| kind.as_str()) {
                // 只认首个 auth_init：重复帧不再回 ack（官方只在 authenticating 态消费）。
                Some("auth_init") if device_sid.is_none() => {
                    let sid = value
                        .get("device_sid")
                        .and_then(|sid| sid.as_str())
                        .unwrap_or_default()
                        .to_string();
                    device_sid = Some(sid.clone());
                    let ack = serde_json::json!({
                        "type": "auth_ack",
                        "device_sid": sid,
                        "pair_status": "matched",
                    });
                    let _ = socket.send(Message::text(ack.to_string())).await;
                }
                Some("pair_status_query") => {
                    let ack = serde_json::json!({
                        "type": "pair_status_ack",
                        "device_sid": device_sid.clone().unwrap_or_default(),
                        "pair_status": "matched",
                    });
                    let _ = socket.send(Message::text(ack.to_string())).await;
                }
                // 握手完成后其余 JSON 文本帧（数据面 `data`）原样回显一次：
                // 不改写 payload、不重复回显，供生产入站路径解析后上报 `RelayEvent::Data`。
                Some(_) if device_sid.is_some() => {
                    if let Some(text) = raw_text {
                        echoes.fetch_add(1, Ordering::SeqCst);
                        let _ = socket.send(Message::text(text)).await;
                    }
                }
                _ => {}
            }
        }
    }

    /// 走一次完整回环授权（BigModel 通道），返回已持有内存 JWT 的 `AuthService`。
    ///
    /// `open` 只做「交给系统浏览器」的替身：从授权 URL 解析回环 redirect 与 state，
    /// 再异步 GET 投递合成 code（真实浏览器在终端外的等价动作）。
    async fn authorized_service(
        fake: &FakeOfficialHttp,
        http: &reqwest::Client,
    ) -> Arc<AuthService> {
        let auth = Arc::new(AuthService::with_endpoints(
            http.clone(),
            fake.origin.clone(),
            fake.origin.clone(),
        ));
        let open: OpenAuthorize = Arc::new({
            let http = http.clone();
            move |url: String| -> ZResult<()> {
                let parsed = reqwest::Url::parse(&url).expect("授权 URL 应可解析");
                let params: HashMap<String, String> = parsed.query_pairs().into_owned().collect();
                let redirect = params
                    .get("redirect")
                    .or_else(|| params.get("redirect_uri"))
                    .expect("授权 URL 必带回环 redirect")
                    .clone();
                let state = params.get("state").expect("授权 URL 必带 state").clone();
                let client = http.clone();
                tokio::spawn(async move {
                    let callback = format!("{redirect}?state={state}&code=synthetic");
                    let _ = client.get(callback).send().await;
                });
                Ok(())
            }
        });

        let authorized = tokio::time::timeout(
            Duration::from_secs(10),
            auth.authorize(OAuthProvider::Bigmodel, open),
        )
        .await;
        match authorized {
            Ok(Ok(())) => {}
            Ok(Err(err)) => panic!("授权应成功，实际 {}", err.code),
            Err(_) => panic!("授权未在时限内完成"),
        }
        auth
    }

    /// 真实回环 HTTP 全链：授权回调 → 交换 JWT → 终端 token。
    #[tokio::test]
    async fn loopback_http_auth_and_terminal_token() {
        let fake = FakeOfficialHttp::start().await;
        // 显式禁用系统代理：macOS 系统代理会把 127.0.0.1 也转发出去（与本域行为无关）
        let http = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("测试 HTTP 客户端");
        let auth = authorized_service(&fake, &http).await;

        // 生产 `terminal_token` 先校验 JWT 的 payload `exp`，再带上它请求回环端点。
        let token = match auth.terminal_token().await {
            Ok(token) => token,
            Err(err) => panic!("终端 token 应可取，实际 {}", err.code),
        };
        assert_eq!(token.value, SYNTHETIC_TERMINAL_TOKEN);
        assert_eq!(token.expires_in, SYNTHETIC_EXPIRES_IN);
    }

    /// 真实回环 WS 配对全链：真实 [`AuthService`]（回环 HTTP 取 terminal token）+
    /// 真实 [`RelayClient`]（回环 WS 升级与 `auth_init`→`auth_ack` 握手）。
    ///
    /// 端点源按生产客户端的期望注入 `ws://127.0.0.1:<port>`，`/api/v1/remote/ws2` 路径、
    /// `token`/`mid` 查询参数与帧编码全部由生产代码拼装。配对后为 `Paired`；
    /// 假体只接受了一条 socket；显式 `disconnect` 后生产侧零自动重连。
    #[tokio::test]
    async fn loopback_ws_pairs_real_relay() {
        let fake = FakeOfficialHttp::start().await;
        let http = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("测试 HTTP 客户端");
        let auth = authorized_service(&fake, &http).await;

        // 授权已落内存：先确认生产 `terminal_token` 可用（connect 内部还会现取新 token）。
        let token = match auth.terminal_token().await {
            Ok(token) => token,
            Err(err) => panic!("终端 token 应可取，实际 {}", err.code),
        };
        assert_eq!(token.value, SYNTHETIC_TERMINAL_TOKEN);

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("回环 WS 端口应可绑定");
        let ws = spawn_ws_handshake(listener);

        let (events_tx, _events_rx) = mpsc::channel(8);
        let client = RelayClient::with_endpoint(auth, events_tx, ws.origin.clone());

        let paired = tokio::time::timeout(
            Duration::from_secs(10),
            client.connect(DeviceRoute {
                sid: "synthetic-sid".into(),
                mid: Some("synthetic-mid".into()),
                label: None,
            }),
        )
        .await
        .expect("connect 未在时限内完成")
        .expect("真实回环 relay 应配对成功");
        assert_eq!(
            paired.state,
            ConnectionState::Paired,
            "握手完成后应为 Paired"
        );
        assert_eq!(ws.accepted(), 1, "一次 connect 只允许一条 socket");

        // 显式断开：生产无自动重连，断开后不得再开第二条 socket。
        client.disconnect().await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(ws.accepted(), 1, "显式断开后不得自动重连");
    }

    /// 真实回环 WS 数据往返：真实 [`AuthService`] + 真实 [`RelayClient`]（配对成功）后，
    /// [`RelayClient::send_data`] 写出的 `data` 帧被假体**原样回显一次**，生产入站路径
    /// 解析后以 [`RelayEvent::Data`] 上报同一 JSON 载荷（无改写、无重复）。
    ///
    /// 覆盖「写帧 → 回环 → 入站解析 → 事件上报」整条数据面，不只验证写成功。
    #[tokio::test]
    async fn loopback_ws_round_trips_relay_data() {
        let fake = FakeOfficialHttp::start().await;
        let http = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("测试 HTTP 客户端");
        let auth = authorized_service(&fake, &http).await;

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("回环 WS 端口应可绑定");
        let ws = spawn_ws_handshake(listener);

        let (events_tx, mut events_rx) = mpsc::channel(8);
        let client = RelayClient::with_endpoint(auth, events_tx, ws.origin.clone());

        let paired = tokio::time::timeout(
            Duration::from_secs(10),
            client.connect(DeviceRoute {
                sid: "synthetic-sid".into(),
                mid: Some("synthetic-mid".into()),
                label: None,
            }),
        )
        .await
        .expect("connect 未在时限内完成")
        .expect("真实回环 relay 应配对成功");
        assert_eq!(paired.state, ConnectionState::Paired, "数据面要求已配对");

        let probe = serde_json::json!({ "probe": "synthetic" });
        tokio::time::timeout(Duration::from_secs(5), client.send_data(probe.clone()))
            .await
            .expect("send_data 未在时限内完成")
            .expect("已配对连接应接受 data 帧");

        // 事件流前面还有 connecting/paired 状态事件：只取第一条 Data 作为往返结果。
        let round_tripped = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match events_rx.recv().await {
                    Some(RelayEvent::Data(payload)) => return payload,
                    Some(RelayEvent::State(_)) => continue,
                    None => panic!("事件通道在收到 Data 前关闭"),
                }
            }
        })
        .await
        .expect("Data 事件未在时限内到达");

        assert_eq!(
            round_tripped, probe,
            "回环载荷必须原样回到 RelayEvent::Data"
        );
        assert_eq!(ws.echoes(), 1, "一条 data 帧只允许一次回显");
    }

    /// 回环配对 + 桥装配：真实 [`AuthService`]（回环 HTTP 取 token）+ 真实
    /// [`RelayClient`]（回环 WS 配对）+ 真实 [`BridgeManager`]（默认生产时序）。
    ///
    /// 事件通道兼作生产事件泵的输入：`Data` 逐条转给桥（状态事件忽略）。
    /// 返回的假体句柄必须由调用方持有（Drop 即中止回环 WS 服务）。
    async fn paired_bridge_over_loopback() -> (
        FakeWsHandshake,
        Arc<BridgeManager>,
        tokio::task::JoinHandle<()>,
    ) {
        let fake = FakeOfficialHttp::start().await;
        let http = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("测试 HTTP 客户端");
        let auth = authorized_service(&fake, &http).await;

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("回环 WS 端口应可绑定");
        let ws = spawn_ws_handshake(listener);

        let (events_tx, mut events_rx) = mpsc::channel(32);
        let relay = Arc::new(RelayClient::with_endpoint(
            auth,
            events_tx,
            ws.origin.clone(),
        ));
        let paired = tokio::time::timeout(
            Duration::from_secs(10),
            relay.connect(DeviceRoute {
                sid: "synthetic-sid".into(),
                mid: Some("synthetic-mid".into()),
                label: None,
            }),
        )
        .await
        .expect("connect 未在时限内完成")
        .expect("真实回环 relay 应配对成功");
        assert_eq!(paired.state, ConnectionState::Paired, "桥要求已配对");

        let bridge = Arc::new(BridgeManager::with_timing(relay, BridgeTiming::default()));
        let pump = {
            let bridge = bridge.clone();
            tokio::spawn(async move {
                while let Some(event) = events_rx.recv().await {
                    if let RelayEvent::Data(payload) = event {
                        let _ = bridge.on_data(payload).await;
                    }
                }
            })
        };
        (ws, bridge, pump)
    }

    /// 真实回环 relay 上的桥 bootstrap 全链。
    ///
    /// 假体按 `zcode_type` 应答：`bootstrap-request` / `workspace-list-request` 回
    /// 合成 listing（`/tmp/travel`），`workspace-bridge-open` 回 ready 并推 Initialize。
    /// 出站日志前三条 `bootstrap-request → workspace-list-request → workspace-bridge-open`
    /// 的**顺序**证明：生产桥真的解析了假体回的合成 workspace，并接着在**同一条真实
    /// relay 连接**上发下一条请求（不是假体自导自演）。
    #[tokio::test]
    async fn bridge_bootstrap_runs_over_real_loopback_relay() {
        let (ws, bridge, pump) = paired_bridge_over_loopback().await;

        let bound =
            tokio::time::timeout(Duration::from_secs(5), bridge.bind(&synthetic_target())).await;
        assert!(
            matches!(bound, Ok(Ok(_))),
            "ready/Initialize 齐备后 bind 应在窗口内完成"
        );
        pump.abort();

        let log = ws.log();
        assert_eq!(
            log.iter()
                .take(3)
                .map(String::as_str)
                .collect::<Vec<&str>>(),
            vec![
                BOOTSTRAP_REQUEST,
                WORKSPACE_LIST_REQUEST,
                WORKSPACE_BRIDGE_OPEN
            ],
            "生产桥必须按 bootstrap → listing → 开桥 的顺序发出请求"
        );
        assert_eq!(ws.accepted(), 1, "整条 bootstrap 只需一条 relay 连接");
    }

    /// 真实回环 relay 上的 ready → Initialize → hello happy path（T8.1）。
    ///
    /// 假体在 `workspace-bridge-open` 后回 `workspace-bridge-ready`（只回显请求声明过的
    /// 身份），并紧接着推该桥的 `Initialize(200)`（每桥首帧 seq=1 / messageSeq=1）；
    /// 之后对生产发出的每条 rpc-frame 解码 channel 方法并回 201。本测试断言的是**正向
    /// 路径**：ready 回显被生产接受；收到 `Initialize(200)` 后，生产在**同一条真实
    /// relay 连接**上发出 `helloConversationV4` / `initializeConversationV4`，并订阅
    /// controller 与 conversation 两条 topic。
    ///
    /// **本测试不证明**「等 200」这道门真的起作用：去掉该门后生产是否会提前发帧、
    /// 或行为是否失败，属于负向门测试，留 Task 12；此处只覆盖门存在时的 happy path。
    #[tokio::test]
    async fn bridge_ready_initialize_hello_over_loopback_relay() {
        let (ws, bridge, pump) = paired_bridge_over_loopback().await;

        let bound =
            tokio::time::timeout(Duration::from_secs(5), bridge.bind(&synthetic_target())).await;
        match bound {
            Ok(Ok(target)) => assert_eq!(target.session_id, SYNTHETIC_TASK_ID),
            Ok(Err(error)) => panic!("bind 应完成，实际 {}", error.code),
            Err(_) => panic!("bind 未在 5s 内完成"),
        }
        pump.abort();

        let log = ws.log();
        assert!(
            log.iter()
                .any(|method| method.as_str() == AGENT_HELLO_METHOD),
            "helloConversationV4 必须经真实 relay 走通：{log:?}"
        );
        assert!(
            log.iter()
                .any(|method| method.as_str() == AGENT_CLIENT_HELLO_METHOD),
            "initializeConversationV4 必须经真实 relay 走通：{log:?}"
        );
        assert!(
            log.iter()
                .any(|method| method.as_str() == CONTROLLER_SUBSCRIBE_METHOD),
            "subscribeControllerV4 必须经真实 relay 发出：{log:?}"
        );
        assert!(
            log.iter()
                .any(|method| method.as_str() == AGENT_SUBSCRIBE_CONVERSATION_METHOD),
            "subscribeConversationV4 必须经真实 relay 发出：{log:?}"
        );
        // 枚举桥 + 会话桥各开一次（两次 ready/Initialize 都被生产接受）。
        assert_eq!(
            log.iter()
                .filter(|zcode_type| zcode_type.as_str() == WORKSPACE_BRIDGE_OPEN)
                .count(),
            2,
            "枚举桥与会话桥各需一次 workspace-bridge-open：{log:?}"
        );
        assert_eq!(
            ws.accepted(),
            1,
            "整条 ready/Initialize/hello 只需一条 relay 连接"
        );
    }

    /// 假体任务索引里的唯一目标（与 `SYNTHETIC_TASK_ID` 精确相等）。
    fn synthetic_target() -> TargetKey {
        TargetKey {
            session_id: SYNTHETIC_TASK_ID.into(),
        }
    }
}
