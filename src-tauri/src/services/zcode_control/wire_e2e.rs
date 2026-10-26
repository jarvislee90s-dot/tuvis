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

    use axum::http::StatusCode;
    use axum::routing::post;
    use axum::{Json, Router};
    use base64::Engine as _;
    use futures::{SinkExt, StreamExt};
    use http_body_util::BodyExt as _;
    use rusqlite::Connection;
    use tokio::sync::mpsc;
    use tokio_tungstenite::tungstenite::Message;
    use tower::ServiceExt as _;

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
        decode_channel, encode_frame, encode_response_for_test, RpcIdentity,
        REQUEST_TYPE_EVENT_LISTEN, REQUEST_TYPE_PROMISE, RESPONSE_TYPE_EVENT_FIRE,
    };
    use super::super::command::{AGENT_SEND_COMMAND_METHOD, SEND_TEXT_TYPE};
    use super::super::config::set_config;
    use super::super::hub::{HubBackend, PartsBackend, ZcodeHub};
    use super::super::projection::CONVERSATION_TOPIC_PREFIX;
    use super::super::relay::{DeviceRoute, RelayClient, RelayEvent};
    use super::super::types::{
        codes, CapabilityFlag, CapabilityView, ConnectionState, TargetKey, ZResult,
    };
    use crate::database::{migration, schema};
    use crate::inject::capability::new_table;
    use crate::inject::engine::Injector;
    use crate::remote::codex_ipc::CodexControlHub;
    use crate::remote::pairing::{persist_device, DeviceStore, NewDevice};
    use crate::remote::pin::PinRateLimiter;
    use crate::remote::server::{router, CreateTaskHub, RemoteState, SseRegistry};
    use crate::session::{AgentType, ProcessForm, Session, SessionStatus, SessionsResponse};

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

    /// 假体对 `sendConversationCommandV4` 的 ACK 形态（失败用例逐个覆盖；默认 accepted）。
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum AckMode {
        /// 正常受理：`{"status":"accepted",...}`。
        Accepted,
        /// 宿主显式拒绝：`{"status":"rejected"}`。
        Rejected,
        /// 无法解释的应答：`{}`（生产按保守 `unknown` 收口）。
        Malformed,
    }

    impl AckMode {
        fn body(self) -> serde_json::Value {
            match self {
                Self::Accepted => {
                    serde_json::json!({"status": "accepted", "commandId": "cmd-synthetic"})
                }
                Self::Rejected => serde_json::json!({"status": "rejected"}),
                Self::Malformed => serde_json::json!({}),
            }
        }
    }

    /// 回环 WS 握手假体句柄：`origin` 为客户端应注入的 ws 源（`ws://127.0.0.1:<port>`），
    /// `accepted()` 为 TCP 层已接受的连接数（断言「恰好一条 socket」与「断开后零重连」），
    /// `echoes()` 为已回显的数据面帧数（断言「一帧一次、不重复」），
    /// `log()` 为收到的 app 业务请求的顺序记录：`zcode_type` 业务请求记其类型，
    /// `rpc-frame` 记解出的 channel 方法名，两者**混在同一序列**；
    /// `commands()` 为解出的写命令信封（本文件只关心 `sendConversationCommandV4`）。
    struct FakeWsHandshake {
        origin: String,
        accepted: Arc<AtomicUsize>,
        echoes: Arc<AtomicUsize>,
        log: Arc<Mutex<Vec<String>>>,
        commands: Arc<Mutex<Vec<serde_json::Value>>>,
        ack: Arc<Mutex<AckMode>>,
        /// `drop_link()` 的断链信号：每条连接 select 它，收到即发 Close 并收尾。
        drop_tx: tokio::sync::broadcast::Sender<()>,
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

        /// 已解出的写命令官方 params（`sendConversationCommandV4` 的 body[0]：
        /// `{workspacePath, envelope}`，生产 CommandPort 适配层产物）。
        fn commands(&self) -> Vec<serde_json::Value> {
            self.commands.lock().expect("写命令日志锁").clone()
        }

        /// 设定 `sendConversationCommandV4` 的 ACK 形态（只影响该方法的应答）。
        fn set_ack(&self, mode: AckMode) {
            *self.ack.lock().expect("ACK 模式锁") = mode;
        }

        /// 从假体侧主动断链：已建立的连接收到 Close（生产 relay 观察到掉线，但
        /// Hub 代际不变——符合「链路断开」而非「用户显式断开」）。
        fn drop_link(&self) {
            let _ = self.drop_tx.send(());
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
    /// （`helloConversationV4` / `initializeConversationV4` / 两个订阅；
    /// `sendConversationCommandV4` 按 [`AckMode`] 回 ACK，默认 accepted）。
    /// 会话订阅 ACK 之后推一条**真实形态**的 conversation snapshot（revision=1、
    /// 空 pending/control），让生产的 bind/投影走完整 read 面（不手推 SSE）。
    /// 其余 JSON 文本帧（数据面 `data` 探针）**原样回显一次**（不改写、不重复）；
    /// socket 保持到对端关闭（或 `drop_link()`）。
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
        let commands = Arc::new(Mutex::new(Vec::new()));
        let command_log = commands.clone();
        let ack = Arc::new(Mutex::new(AckMode::Accepted));
        let ack_mode = ack.clone();
        let (drop_tx, _keep_drop_rx) = tokio::sync::broadcast::channel(8);
        let drop_signal = drop_tx.clone();
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
                    command_log.clone(),
                    ack_mode.clone(),
                    drop_signal.subscribe(),
                ));
            }
        });
        FakeWsHandshake {
            origin,
            accepted,
            echoes,
            log,
            commands,
            ack,
            drop_tx,
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

    /// 合成会话快照（与 bridge 单元测试里官方假体的快照同形）：`revision=1`、
    /// 空 pending / 空 control、带当前选择 config。生产投影据此 `ready=true`。
    fn synthetic_conversation_snapshot(session_id: &str) -> serde_json::Value {
        serde_json::json!({
            "protocolVersion": 1,
            "sessionId": session_id,
            "logEpoch": "synthetic-epoch",
            "seq": 1,
            "revision": 1,
            "control": {"activeWorks": []},
            "pendingInteractions": [],
            "config": {
                "provider": "synthetic-provider",
                "model": "synthetic-model-a",
                "thought": "high",
            },
        })
    }

    /// 推一条该会话 topic 的 conversation snapshot（204 事件帧）：
    /// complete 包装 + `[fromSeq,toSeq]=[0,1]`，与官方 wire 形态一致。
    async fn push_conversation_snapshot(
        socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        peer: &mut FakePeer,
        bridge: &str,
        session_id: &str,
    ) {
        // 生产在订阅 RPC 之前先发 EventListen；缺 listen id 就无法路由事件帧。
        let Some(listen) = peer.listen.get(bridge).copied() else {
            return;
        };
        let topic = format!("{CONVERSATION_TOPIC_PREFIX}{session_id}");
        let logical = serde_json::json!({
            "topic": topic,
            "subscriptionId": CONVERSATION_SUBSCRIPTION,
            "fromSeq": 0,
            "toSeq": 1,
            "sentAt": 0,
            "payload": {
                "kind": "snapshot",
                "snapshot": synthetic_conversation_snapshot(session_id),
            },
        });
        let wire = serde_json::json!({
            "wireVersion": V4_WIRE_PROTOCOL_VERSION,
            "kind": "complete",
            "deliveryKind": "initial",
            "logicalFrameId": "synthetic-logical-1",
            "logicalFrameOrdinal": 1,
            "topic": topic,
            "subscriptionId": CONVERSATION_SUBSCRIPTION,
            "frame": logical,
        });
        send_data(socket, peer.frame(bridge, encode_event_fire(listen, &wire))).await;
    }

    /// 处理一条入站 `rpc-frame`：解 header 记账 channel 方法，再按方法回 201。
    ///
    /// - `helloConversationV4` → `{"kind":"hello","protocolVersion":V4_WIRE_PROTOCOL_VERSION,"connectionId":...}`；
    /// - `initializeConversationV4` → `{}`；
    /// - `subscribeControllerV4` → 订阅 ack，并补一条 controller 任务索引快照（204）；
    /// - `subscribeConversationV4` → 订阅 ack，并补一条该会话的 conversation snapshot（204）；
    /// - `sendConversationCommandV4` → 解出信封记账，按 [`AckMode`] 回 ACK；
    /// - 其余 → 空 body（不猜业务语义）。
    async fn serve_rpc_frame(
        socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        peer: &mut FakePeer,
        log: &Arc<Mutex<Vec<String>>>,
        commands: &Arc<Mutex<Vec<serde_json::Value>>>,
        ack: &Arc<Mutex<AckMode>>,
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
                if method == AGENT_SEND_COMMAND_METHOD {
                    // 只记录解出的命令信封（正文/身份都在里面），供断言「恰一帧、
                    // 正文原样离机、clientId 来自配对设备」。
                    if let Ok(message) = decode_channel(&bytes) {
                        let envelope = message.body.get(0).cloned().unwrap_or_default();
                        commands.lock().expect("写命令日志锁").push(envelope);
                    }
                }
                let conversation_session = if method == AGENT_SUBSCRIBE_CONVERSATION_METHOD {
                    decode_channel(&bytes).ok().and_then(|message| {
                        message
                            .body
                            .get(0)
                            .and_then(|arg| arg.get("sessionId"))
                            .and_then(|value| value.as_str())
                            .map(str::to_owned)
                    })
                } else {
                    None
                };
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
                    AGENT_SEND_COMMAND_METHOD => ack.lock().expect("ACK 模式锁").body(),
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
                if let Some(session_id) = conversation_session {
                    push_conversation_snapshot(socket, peer, &bridge, &session_id).await;
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
        commands: Arc<Mutex<Vec<serde_json::Value>>>,
        ack: Arc<Mutex<AckMode>>,
        mut close_rx: tokio::sync::broadcast::Receiver<()>,
    ) {
        let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
            return;
        };
        let mut device_sid: Option<String> = None;
        let mut peer = FakePeer::default();
        loop {
            // `drop_link()` 优先于继续读帧：从假体侧发 Close，生产 relay 观察到掉线。
            let frame = tokio::select! {
                biased;
                _ = close_rx.recv() => {
                    let _ = socket.close(None).await;
                    break;
                }
                frame = socket.next() => frame,
            };
            let Some(frame) = frame else {
                break;
            };
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
                        serve_rpc_frame(&mut socket, &mut peer, &log, &commands, &ack, payload)
                            .await;
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

    // =======================================================================
    // T8.1 全链：真实生产组件 × 回环假官方（任务 12）
    // =======================================================================

    /// 已配对设备 cookie（设备身份只来自配对门，绝不来自请求体）。
    const COOKIE: &str = "mam_device=dev-e2e";
    /// 与 cookie 对应的设备 id。
    const DEVICE_ID: &str = "dev-e2e";
    /// 合成官方链接（只用于 Hub 解析 `sid`；relay 端点由假体注入，零真实网络）。
    const SYNTH_URL: &str = "https://zcode.z.ai/remote/v4?sid=synthetic-full-chain";
    /// 成功用例的 actionId（同 id 重放必须命中同一账本行）。
    const ACTION_ID: &str = "act-full-1";
    /// 合成正文（审计只记字数，绝不落正文）。
    const SEND_TEXT: &str = "合成正文：全链发送";

    type SharedConn = Arc<Mutex<Connection>>;

    fn shared_conn() -> SharedConn {
        let conn = Connection::open_in_memory().expect("打开内存库");
        schema::init(&conn);
        migration::migrate(&conn).expect("迁移");
        Arc::new(Mutex::new(conn))
    }

    /// 合成已配对设备 + 已保存的官方链接（同一内存库，与生产三方共享）。
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
            set_config(c, SYNTH_URL).expect("保存合成链接");
        });
    }

    /// 移动端快照里的唯一 ZCode 会话（id 与假体任务索引精确相等）。
    fn zcode_session() -> Session {
        Session {
            id: SYNTHETIC_TASK_ID.into(),
            agent_type: AgentType::ZCode,
            project_name: "zcode-full-chain".into(),
            project_path: "/tmp/travel".into(),
            title: None,
            git_branch: None,
            github_url: None,
            status: SessionStatus::Idle,
            last_message: None,
            last_message_role: None,
            last_message_subagent_report: false,
            flap_from_subagent_activity: false,
            last_activity_at: "2026-10-27T00:00:00Z".into(),
            pid: 4242,
            cpu_usage: 0.0,
            active_subagent_count: 0,
            form: ProcessForm::Cli,
            jump_supported: false,
            unread: false,
        }
    }

    /// 记录型注入器：本文件的所有动作都必须走官方 relay；任何终端注入都是回归。
    #[derive(Default)]
    struct RecordingInjector {
        calls: Mutex<Vec<String>>,
    }

    impl RecordingInjector {
        fn calls(&self) -> usize {
            self.calls.lock().expect("注入器日志锁").len()
        }
    }

    impl Injector for RecordingInjector {
        fn locate_and_inject(&self, pid: u32, text: &str) -> Result<(), String> {
            self.calls
                .lock()
                .expect("注入器日志锁")
                .push(format!("inject:{pid}:{text}"));
            Ok(())
        }

        fn locate_and_send_key_inner(&self, pid: u32, key: &str) -> Result<(), String> {
            self.calls
                .lock()
                .expect("注入器日志锁")
                .push(format!("key:{pid}:{key}"));
            Ok(())
        }
    }

    /// 移动端 `RemoteState` 夹具（字段照抄 `tests/zcode_control_remote.rs` 的桩形态）：
    /// 内存库 + 单 ZCode 会话快照 + 空隐藏集合；`set_zcode_hub` 后 router 才走真实 Hub。
    fn remote_state(
        conn: &SharedConn,
        hub: Arc<ZcodeHub>,
        injector: Arc<RecordingInjector>,
    ) -> Arc<RemoteState> {
        let sessions = vec![zcode_session()];
        let store = DeviceStore::Owned(conn.clone());
        let state = Arc::new(RemoteState {
            ui_config_source: Box::new(|| None),
            subagent_source: std::collections::HashMap::new(),
            subagent_message_source: std::collections::HashMap::new(),
            capability_table: new_table(),
            session_source: Box::new(move || SessionsResponse {
                total_count: sessions.len(),
                sessions: sessions.clone(),
                waiting_count: 0,
            }),
            pairing_counter: Box::new(Vec::new),
            board_hidden_ids: Box::new(Vec::new),
            board_hidden_hide: Arc::new(|_| 0usize),
            board_hidden_unhide: Arc::new(|_| 0usize),
            unread_mark_read: Arc::new(|_, _| ()),
            session_close: Arc::new(|_| Ok(())),
            store: store.clone(),
            host_source: Box::new(|| serde_json::json!({ "host": { "name": "test-host" } })),
            message_source: Box::new(|_, _, _| Err("测试桩：无内容".to_string())),
            path_source: Box::new(|_, _, _| (Vec::new(), false)),
            watcher_tx: tokio::sync::broadcast::channel(64).0,
            sse_registry: Arc::new(SseRegistry::default()),
            max_devices_source: Box::new(|| 3),
            pin_limiter: Mutex::new(PinRateLimiter::new()),
            global_pin_limiter: Mutex::new(PinRateLimiter::global()),
            pin_source: Box::new(|| Some("1234".to_string())),
            now_source: Box::new(|| chrono::Utc::now().timestamp_millis()),
            via_hosts_source: Box::new(|| None),
            rate_bucket_channels_source: Box::new(Vec::new),
            home_source: Box::new(|| None),
            injector: injector.clone(),
            resume_spawner: Arc::new(|_: &crate::inject::resume::SpawnSpec| Ok(())),
            create_hub: Arc::new(CreateTaskHub::stub()),
            archive_source: Box::new(Vec::new),
            archive_delete: Arc::new(|_: Option<&str>| 0usize),
            confirm_probe: Arc::new(|_, _, _| true),
            dialog_probe: Arc::new(|_, _| None),
            screen_probe: Arc::new(|_, _| None),
            codex_control: Arc::new(CodexControlHub::disabled_with_store(store)),
            desktop_language: Default::default(),
        });
        state.set_zcode_hub(hub);
        state
    }

    fn get_req(uri: &str) -> axum::http::Request<axum::body::Body> {
        axum::http::Request::builder()
            .method("GET")
            .uri(uri)
            .header("cookie", COOKIE)
            .body(axum::body::Body::empty())
            .expect("GET 请求")
    }

    fn action_req(body: &serde_json::Value) -> axum::http::Request<axum::body::Body> {
        axum::http::Request::builder()
            .method("POST")
            .uri("/m/api/v1/zcode/actions")
            .header("cookie", COOKIE)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body.to_string()))
            .expect("POST 请求")
    }

    async fn body_json(resp: axum::http::Response<axum::body::Body>) -> serde_json::Value {
        let bytes = resp
            .into_body()
            .collect()
            .await
            .expect("响应体可读")
            .to_bytes();
        serde_json::from_slice(&bytes).expect("响应体必须是 JSON")
    }

    async fn get_json(app: &axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
        let resp = app.clone().oneshot(get_req(uri)).await.expect("请求应完成");
        let status = resp.status();
        (status, body_json(resp).await)
    }

    async fn post_action(
        app: &axum::Router,
        body: &serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let resp = app
            .clone()
            .oneshot(action_req(body))
            .await
            .expect("请求应完成");
        let status = resp.status();
        (status, body_json(resp).await)
    }

    /// 读一帧 SSE（2s 超时；流结束返回 None）。与既有 `/zcode/events` 测试同一读法。
    async fn next_sse_frame(stream: &mut axum::body::BodyDataStream) -> Option<String> {
        let next = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("SSE 帧读取超时");
        next.map(|chunk| String::from_utf8(chunk.expect("SSE 帧读取失败").to_vec()).expect("UTF-8"))
    }

    /// 从一帧 SSE 文本里取 `data:` 行并解析成 JSON。
    fn sse_data(frame: &str) -> serde_json::Value {
        let data = frame
            .lines()
            .find_map(|line| line.strip_prefix("data: "))
            .unwrap_or_else(|| panic!("SSE 帧必须带 data 行：{frame:?}"));
        serde_json::from_str(data)
            .unwrap_or_else(|error| panic!("SSE data 必须是 JSON（{error}）：{frame:?}"))
    }

    /// 等一条指定事件名的 SSE 帧（跳过 keep-alive 与其它事件），返回事件体。
    async fn await_sse_event(
        stream: &mut axum::body::BodyDataStream,
        name: &str,
        accept: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let Some(frame) = next_sse_frame(stream).await else {
                panic!("SSE 流在等待 {name} 事件时结束");
            };
            let Some(event) = frame.lines().find_map(|line| line.strip_prefix("event: ")) else {
                continue;
            };
            if event != name {
                continue;
            }
            let data = sse_data(&frame);
            if accept(&data) {
                return data;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "等待 SSE {name} 事件超时"
            );
        }
    }

    /// 轮询等待（20ms 间隔、5s 上限；只等真实事件落定，不代替断言）。
    async fn wait_until(mut done: impl FnMut() -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(tokio::time::Instant::now() < deadline, "等待状态落定超时");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn ledger_rows(store: &DeviceStore) -> i64 {
        store.with(|c| {
            c.query_row("SELECT COUNT(*) FROM zcode_actions", [], |r| r.get(0))
                .expect("zcode_actions 可读")
        })
    }

    fn ledger_state(store: &DeviceStore, action_id: &str) -> String {
        store.with(|c| {
            c.query_row(
                "SELECT state FROM zcode_actions WHERE action_id = ?1",
                [action_id],
                |r| r.get(0),
            )
            .expect("zcode_actions 行必须存在")
        })
    }

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

    fn audit_summaries(store: &DeviceStore) -> Vec<String> {
        store.with(|c| {
            let mut stmt = c
                .prepare("SELECT summary FROM write_audit WHERE action = 'zcode_send' ORDER BY id")
                .expect("write_audit 可读");
            let rows = stmt.query_map([], |r| r.get(0)).expect("write_audit 可读");
            rows.map(|row| row.expect("write_audit 行可读")).collect()
        })
    }

    /// 测试策略：只开 `send`（其余七项保持生产默认全关）；生产默认矩阵不受影响。
    fn send_policy() -> CapabilityView {
        CapabilityView {
            send: CapabilityFlag {
                enabled: true,
                reason_code: None,
            },
            ..CapabilityView::default()
        }
    }

    /// 全链装配：回环假官方（OAuth token / terminal token / relay WS / bridge RPC）
    /// + 生产 AuthService（真实 OAuth 回调）→ 生产 RelayClient → 生产 BridgeManager →
    ///   生产 CommandSender → 生产 ZcodeHub → 生产事件泵。端点之外零替身。
    struct Chain {
        /// 回环假官方 HTTP 服务：Drop 即停止（保活到用例结束）。
        _fake: FakeOfficialHttp,
        ws: FakeWsHandshake,
        /// 已授权的真实 AuthService（两个 Hub 共用；避免二次 OAuth）。
        auth: Arc<AuthService>,
        conn: SharedConn,
        store: DeviceStore,
        hub: Arc<ZcodeHub>,
        injector: Arc<RecordingInjector>,
    }

    async fn full_chain(policy: CapabilityView) -> Chain {
        let fake = FakeOfficialHttp::start().await;
        // 显式禁用系统代理（macOS 系统代理会把 127.0.0.1 也转发出去）。
        let http = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("测试 HTTP 客户端");
        // 真实 OAuth：等回环回调 → 交换账号 JWT（只入内存）→ terminal token 端点。
        let auth = authorized_service(&fake, &http).await;
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("回环 WS 端口应可绑定");
        let ws = spawn_ws_handshake(listener);

        let conn = shared_conn();
        seed(&conn);
        let store = DeviceStore::Owned(conn.clone());
        let (parts, events, lag) =
            PartsBackend::production_test(store.clone(), auth.clone(), ws.origin.clone());
        let bridge = parts.pump_bridge();
        let backend: Arc<dyn HubBackend> = Arc::new(parts);
        let hub = Arc::new(ZcodeHub::with_pump_and_lag(
            store.clone(),
            backend,
            policy,
            events,
            bridge,
            lag,
        ));
        // 真实 AuthService 已持有回环 OAuth 拿到的 JWT：只镜像 `authorize` 成功后的
        // Hub 状态（不第二次 OAuth、不等第二次回调）。
        hub.mark_authorized_for_test();
        let connected = tokio::time::timeout(Duration::from_secs(10), hub.connect())
            .await
            .expect("connect 未在时限内完成")
            .expect("真实回环 relay 应配对成功");
        assert_eq!(
            connected.state,
            ConnectionState::Paired,
            "connect 后必须已配对"
        );
        Chain {
            _fake: fake,
            ws,
            auth,
            conn,
            store,
            hub,
            injector: Arc::new(RecordingInjector::default()),
        }
    }

    /// 在同一回环假官方 + 同一内存库上再装一台生产 Hub（能力闸对照用）。
    async fn second_hub(chain: &Chain, policy: CapabilityView) -> Arc<ZcodeHub> {
        let (parts, events, lag) = PartsBackend::production_test(
            chain.store.clone(),
            chain.auth.clone(),
            chain.ws.origin.clone(),
        );
        let bridge = parts.pump_bridge();
        let backend: Arc<dyn HubBackend> = Arc::new(parts);
        let hub = Arc::new(ZcodeHub::with_pump_and_lag(
            chain.store.clone(),
            backend,
            policy,
            events,
            bridge,
            lag,
        ));
        hub.mark_authorized_for_test();
        let connected = tokio::time::timeout(Duration::from_secs(10), hub.connect())
            .await
            .expect("connect 未在时限内完成")
            .expect("真实回环 relay 应配对成功");
        assert_eq!(
            connected.state,
            ConnectionState::Paired,
            "第二台 Hub 也应配对"
        );
        hub
    }

    /// 经移动端真实 `GET /zcode/session` 触发 bind/订阅，并等到投影就绪。
    ///
    /// 不手工 bind、不手推快照：就绪只来自假体在订阅 ACK 后推送的真实会话快照。
    async fn bind_and_wait_ready(app: &axum::Router, session_id: &str) -> serde_json::Value {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let (status, body) = get_json(
                app,
                &format!("/m/api/v1/zcode/session?sessionId={session_id}"),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "会话投影 GET 必须可读：{body}");
            if body["ready"].as_bool() == Some(true) {
                return body;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "会话投影未在时限内就绪：{body}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn send_body(action_id: &str, generation: u64, text: &str) -> serde_json::Value {
        serde_json::json!({
            "actionId": action_id,
            "action": {
                "kind": "send",
                "sessionId": SYNTHETIC_TASK_ID,
                "generation": generation,
                "text": text,
            },
        })
    }

    /// T8.1 全链成功用例：send / receipt / audit / SSE 全部经生产路径。
    ///
    /// 流程 = 假官方 HTTP(Token) → 真实 AuthService → 假官方 WS → 真实 RelayClient →
    /// codec → 真实 BridgeManager → 真实 CommandSender → 真实 ZcodeHub → 真实事件泵 →
    /// `RemoteState`/router（真实 axum 路由 + 配对 gate）→ SSE/HTTP。
    ///
    /// 断言面：
    /// 1. SSE 首帧 `snapshot` = paired（真实连接视图）；随后 `GET session` 触发真实
    ///    bind/订阅，假体推真实会话快照后投影 `ready=true`，并观察到真实 Hub 广播的
    ///    `session` SSE 帧（非手工推送）；
    /// 2. POST send 经真实 HTTP → Hub → CommandSender，假官方 WS **恰一帧**
    ///    `sendConversationCommandV4`（正文原样、clientId 来自配对门）；
    /// 3. 回执 `accepted`；`zcode_actions` 恰一行 accepted；`write_audit` started +
    ///    accepted 各一行且摘要不含正文；SSE 收到真实 `action` 帧；
    /// 4. 假体侧断链后同 actionId 重放：逐字段相同回执、零新帧/零新审计/零新行。
    #[tokio::test]
    async fn full_chain_send_receipt_audit_and_sse() {
        let chain = full_chain(send_policy()).await;
        let app = router(remote_state(
            &chain.conn,
            chain.hub.clone(),
            chain.injector.clone(),
        ));
        let generation = chain.hub.connection().generation;

        // ---- 1) SSE 首帧：连接快照（真实 Hub 当前视图）----
        let resp = app
            .clone()
            .oneshot(get_req(&format!(
                "/m/api/v1/zcode/events?sessionId={SYNTHETIC_TASK_ID}"
            )))
            .await
            .expect("SSE 请求应完成");
        assert_eq!(resp.status(), StatusCode::OK);
        let mut stream = resp.into_body().into_data_stream();
        let first = next_sse_frame(&mut stream).await.expect("首帧不得缺失");
        assert!(
            first.starts_with("event: snapshot\n"),
            "首帧必须是 snapshot，实际 {first:?}"
        );
        let initial = sse_data(&first);
        assert_eq!(initial["connection"]["state"], "paired", "{initial}");
        assert_eq!(
            initial["connection"]["generation"].as_u64(),
            Some(generation),
            "SSE 首帧连接代际必须等于 Hub 代际"
        );

        // ---- 2) GET session 触发真实 bind/订阅；等真实快照 → 投影就绪 ----
        let session = bind_and_wait_ready(&app, SYNTHETIC_TASK_ID).await;
        assert_eq!(session["sessionId"], SYNTHETIC_TASK_ID);
        assert_eq!(
            session["revision"].as_u64(),
            Some(1),
            "宿主 revision 必须经真实投影读到"
        );
        assert_eq!(session["generation"].as_u64(), Some(generation));
        assert_eq!(
            session["capabilities"]["send"]["enabled"], true,
            "测试策略只开 send"
        );
        // 真实 Hub 广播的 session 帧（不是测试手推）：等待 ready=true 的那一条。
        let session_event = await_sse_event(&mut stream, "session", |data| {
            data["data"]["ready"].as_bool() == Some(true)
        })
        .await;
        assert_eq!(session_event["kind"], "session");
        assert_eq!(session_event["sessionId"], SYNTHETIC_TASK_ID);

        // ---- 3) POST send：HTTP → router → 真实 Hub → CommandSender → 假官方 WS ----
        let body = send_body(ACTION_ID, generation, SEND_TEXT);
        let (status, receipt) = post_action(&app, &body).await;
        assert_eq!(status, StatusCode::OK, "send 必须受理：{receipt}");
        assert_eq!(receipt["actionId"], ACTION_ID);
        assert_eq!(receipt["sessionId"], SYNTHETIC_TASK_ID);
        assert_eq!(receipt["generation"].as_u64(), Some(generation));
        assert_eq!(receipt["state"], "accepted", "假官方 accepted ACK");

        let commands = chain.ws.commands();
        assert_eq!(
            commands.len(),
            1,
            "整条链只允许一帧 sendConversationCommandV4：{commands:?}"
        );
        let command = &commands[0];
        // 官方 params = `{workspacePath, envelope}`（由 BridgeManager 的 CommandPort
        // 适配层按精确绑定目标包出，手机自报的 workspace 一律不读）。
        assert_eq!(command["workspacePath"], SYNTHETIC_WORKSPACE_PATH);
        let envelope = &command["envelope"];
        assert_eq!(envelope["type"], SEND_TEXT_TYPE);
        assert_eq!(
            envelope["payload"]["text"], SEND_TEXT,
            "正文必须原样离机（签名缺省关闭）"
        );
        assert_eq!(
            envelope["clientId"], DEVICE_ID,
            "clientId 只来自配对门，不来自请求体"
        );
        assert_eq!(envelope["sessionId"], SYNTHETIC_TASK_ID);
        assert_eq!(
            chain
                .ws
                .log()
                .iter()
                .filter(|method| method.as_str() == AGENT_SEND_COMMAND_METHOD)
                .count(),
            1,
            "sendConversationCommandV4 只允许一帧"
        );
        assert_eq!(chain.injector.calls(), 0, "官方路径不得回落终端注入");

        // ---- 4) 账本/审计：恰一行终态 + started/终态各一条审计（无正文）----
        assert_eq!(ledger_rows(&chain.store), 1, "一次派发恰一行");
        assert_eq!(ledger_state(&chain.store, ACTION_ID), "accepted");
        assert_eq!(audit_rows(&chain.store, "zcode_send", "started"), 1);
        assert_eq!(audit_rows(&chain.store, "zcode_send", "accepted"), 1);
        let summaries = audit_summaries(&chain.store);
        assert_eq!(summaries.len(), 2, "started + accepted：{summaries:?}");
        assert!(
            summaries
                .iter()
                .all(|summary| { summary == &format!("send chars={}", SEND_TEXT.chars().count()) }),
            "审计摘要只含类别 + 字数，绝不摘正文：{summaries:?}"
        );
        // 真实 Hub 广播的 action 帧。
        let action_event = await_sse_event(&mut stream, "action", |data| {
            data["data"]["actionId"] == ACTION_ID
        })
        .await;
        assert_eq!(action_event["data"]["state"], "accepted");
        assert_eq!(action_event["sessionId"], SYNTHETIC_TASK_ID);

        // ---- 5) 断链后同 actionId 重放：同回执、零新帧/审计/账本行 ----
        chain.ws.drop_link();
        wait_until(|| chain.hub.connection().state == ConnectionState::Offline).await;
        // 重放前后整份 app 请求序列必须逐条相同：不只数 send 帧，也不允许
        // 任何新的 bind/枚举/订阅请求——否则「查询早于 bind」的语义并未被证明。
        let ws_log_before_replay = chain.ws.log();
        let (status, replay) = post_action(&app, &body).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "断链后重放必须取回既有回执：{replay}"
        );
        assert_eq!(replay, receipt, "同 actionId 必须拿回逐字段相同的既有回执");
        assert_eq!(chain.ws.commands().len(), 1, "重放零新帧（绝不重发）");
        assert_eq!(
            chain.ws.log(),
            ws_log_before_replay,
            "重放不得新增任何 app 请求（含 bind/枚举/订阅）"
        );
        assert_eq!(ledger_rows(&chain.store), 1, "重放零新账本行");
        assert_eq!(audit_rows(&chain.store, "zcode_send", "started"), 1);
        assert_eq!(audit_rows(&chain.store, "zcode_send", "accepted"), 1);
    }

    /// 失败场景：宿主显式 `rejected` ACK → 回执 `rejected`、终态审计、绝不重发。
    ///
    /// 假官方 WS 收回真实 ACK 形状 `{"status":"rejected"}`；生产 `classify_ack` 映射为
    /// `Rejected`，`record_result` 落终态行 + 终态审计。静置观察窗证明无自动重发。
    #[tokio::test]
    async fn rejected_ack_is_terminal_without_resend() {
        let chain = full_chain(send_policy()).await;
        chain.ws.set_ack(AckMode::Rejected);
        let app = router(remote_state(
            &chain.conn,
            chain.hub.clone(),
            chain.injector.clone(),
        ));
        let generation = chain.hub.connection().generation;
        bind_and_wait_ready(&app, SYNTHETIC_TASK_ID).await;

        let (status, receipt) = post_action(
            &app,
            &send_body("act-rejected-1", generation, "宿主显式拒绝的合成正文"),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "rejected 也是已受理的动作结果：{receipt}"
        );
        assert_eq!(receipt["state"], "rejected");
        assert!(
            receipt["reasonCode"].is_null(),
            "显式拒绝不带原因码：{receipt}"
        );
        assert_eq!(chain.ws.commands().len(), 1, "rejected 后不得自动重发");
        assert_eq!(ledger_state(&chain.store, "act-rejected-1"), "rejected");
        assert_eq!(audit_rows(&chain.store, "zcode_send", "started"), 1);
        assert_eq!(audit_rows(&chain.store, "zcode_send", "rejected"), 1);

        // 静置观察窗：生产没有重发计时器，第一百毫秒也不得出现第二帧。
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(chain.ws.commands().len(), 1, "静置期后仍只允许一帧");
    }

    /// 失败场景：无法解释的 ACK（`{}`）→ 保守 `unknown`，恰一帧、绝不自动重发。
    #[tokio::test]
    async fn malformed_ack_is_unknown_without_resend() {
        let chain = full_chain(send_policy()).await;
        chain.ws.set_ack(AckMode::Malformed);
        let app = router(remote_state(
            &chain.conn,
            chain.hub.clone(),
            chain.injector.clone(),
        ));
        let generation = chain.hub.connection().generation;
        bind_and_wait_ready(&app, SYNTHETIC_TASK_ID).await;

        let (status, receipt) = post_action(
            &app,
            &send_body("act-malformed-1", generation, "无法解释应答的合成正文"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{receipt}");
        assert_eq!(receipt["state"], "unknown", "无法解释的 ACK 必须保守未知");
        assert_eq!(receipt["reasonCode"], codes::UNKNOWN);
        assert_eq!(chain.ws.commands().len(), 1, "unknown 不得自动重发");
        assert_eq!(ledger_state(&chain.store, "act-malformed-1"), "unknown");
        assert_eq!(audit_rows(&chain.store, "zcode_send", "started"), 1);
        assert_eq!(audit_rows(&chain.store, "zcode_send", "unknown"), 1);

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(chain.ws.commands().len(), 1, "静置期后仍只允许一帧");
    }

    /// 失败场景：生产默认全关策略下 send → `needs_evidence`（HTTP 409）。
    ///
    /// 能力闸在 bind 之前拒绝：零上游帧、零业务帧（不 bind、不订阅）、零账本行、
    /// 零审计。第二台 Hub 用默认矩阵，生产默认（`ZcodeHub::new`）不受测试策略影响。
    #[tokio::test]
    async fn capability_closed_send_is_needs_evidence_with_zero_side_effects() {
        assert!(
            !CapabilityView::default().send.enabled,
            "生产默认矩阵必须全关"
        );
        let chain = full_chain(send_policy()).await;
        let closed = second_hub(&chain, CapabilityView::default()).await;
        let app = router(remote_state(
            &chain.conn,
            closed.clone(),
            chain.injector.clone(),
        ));
        let generation = closed.connection().generation;
        let before = chain.ws.log();

        let (status, body) = post_action(
            &app,
            &send_body("act-closed-1", generation, "能力关闭时的合成正文"),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "能力闸拒绝必须 409：{body}");
        assert_eq!(body["error"], codes::NEEDS_EVIDENCE);
        assert_eq!(chain.ws.commands().len(), 0, "能力全关零上游帧");
        assert_eq!(
            chain.ws.log(),
            before,
            "能力闸必须在 bind 前拒绝：零订阅/零业务帧"
        );
        assert_eq!(ledger_rows(&chain.store), 0, "未派发不得起账");
        assert_eq!(audit_rows(&chain.store, "zcode_send", "started"), 0);
        assert_eq!(audit_rows(&chain.store, "zcode_send", "accepted"), 0);
    }
}
