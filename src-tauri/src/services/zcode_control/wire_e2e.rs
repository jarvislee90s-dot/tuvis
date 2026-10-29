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

    use axum::extract::State;
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
        AuthService, AuthStatus, OAuthProvider, OpenAuthorize, OAUTH_TOKEN_PATH, RELAY_TOKEN_PATH,
    };
    use super::super::bridge::{
        BridgeManager, BridgeTiming, AGENT_CLIENT_HELLO_METHOD, AGENT_HELLO_METHOD,
        AGENT_SUBSCRIBE_CONVERSATION_METHOD, BOOTSTRAP_REQUEST, CONTROLLER_SUBSCRIBE_METHOD,
        CONTROLLER_TASKS_INDEX_TOPIC, INITIALIZE_BYTES, V4_WIRE_PROTOCOL_VERSION,
        WORKSPACE_BRIDGE_OPEN, WORKSPACE_BRIDGE_READY, WORKSPACE_LIST_REQUEST,
    };
    use super::super::codec::{
        decode_channel, encode_frame, encode_promise_error_for_test, encode_response_for_test,
        RpcIdentity, REQUEST_TYPE_EVENT_LISTEN, REQUEST_TYPE_PROMISE, RESPONSE_TYPE_EVENT_FIRE,
    };
    use super::super::command::{
        AGENT_SEND_COMMAND_METHOD, RESOLVE_INTERACTION_TYPE, SEND_TEXT_TYPE,
    };
    use super::super::config::set_config;
    use super::super::credential_store::MemoryCredentialStore;
    use super::super::hub::{HubBackend, PartsBackend, ZcodeHub};
    use super::super::projection::CONVERSATION_TOPIC_PREFIX;
    use super::super::relay::{DeviceRoute, RelayClient, RelayEvent};
    use super::super::types::{
        codes, CapabilityFlag, CapabilityView, ConnectionState, ControlEventKind, HostBuildProbe,
        TargetKey, ZResult, PROTOCOL_CLIENT_ID,
    };
    use crate::database::dao::settings::try_set_setting_conn;
    use crate::database::{migration, schema};
    use crate::inject::capability::new_table;
    use crate::inject::engine::Injector;
    use crate::remote::codex_ipc::CodexControlHub;
    use crate::remote::pairing::{persist_device, DeviceStore, NewDevice};
    use crate::remote::pin::PinRateLimiter;
    use crate::remote::server::{router, CreateTaskHub, RemoteState, SseRegistry};
    use crate::remote::KEY_ENABLED;
    use crate::session::{AgentType, ProcessForm, Session, SessionStatus, SessionsResponse};

    /// 合成账号 JWT：`header.payload.sig`，payload = `{"exp":9999999999}`（base64url）。
    const SYNTHETIC_JWT: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJleHAiOjk5OTk5OTk5OTl9.sig";
    /// 合成终端 relay token。
    const SYNTHETIC_TERMINAL_TOKEN: &str = "synthetic-terminal";
    /// 终端 token 有效期（秒）。
    const SYNTHETIC_EXPIRES_IN: u64 = 60;

    /// 回环假官方端计数：OAuth 交换与 terminal token 各请求过几次。
    ///
    /// 凭据复用验收只看**真实生产路径实际发出的请求**，不靠测试侧自记；
    /// 计数器由假端点自身在进入处理函数时递增。
    #[derive(Clone, Default)]
    struct FakeCounters {
        oauth_exchanges: Arc<AtomicUsize>,
        terminal_token_requests: Arc<AtomicUsize>,
    }

    /// 回环假官方端：生命周期跟随测试（Drop 即中止 serve 任务）。
    struct FakeOfficialHttp {
        origin: String,
        counters: FakeCounters,
        task: tokio::task::JoinHandle<()>,
    }

    impl FakeOfficialHttp {
        async fn start() -> Self {
            let counters = FakeCounters::default();
            let app = Router::new()
                .route(OAUTH_TOKEN_PATH, post(oauth_token))
                .route(RELAY_TOKEN_PATH, post(terminal_token))
                .with_state(counters.clone());
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .expect("回环端口应可绑定");
            let port = listener.local_addr().expect("回环地址应可读").port();
            let task = tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            Self {
                origin: format!("http://127.0.0.1:{port}"),
                counters,
                task,
            }
        }

        /// 已到达 `POST /api/v1/oauth/token` 的交换次数（重启后仍应保持 1）。
        fn oauth_exchanges(&self) -> usize {
            self.counters.oauth_exchanges.load(Ordering::SeqCst)
        }

        /// 已到达 `POST /api/v1/remote/token` 的请求次数（每次连接现取一次）。
        fn terminal_token_requests(&self) -> usize {
            self.counters.terminal_token_requests.load(Ordering::SeqCst)
        }
    }

    impl Drop for FakeOfficialHttp {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    /// `POST /api/v1/oauth/token`：只回 `data.token`（账号 JWT），并记录一次真实交换。
    async fn oauth_token(State(counters): State<FakeCounters>) -> Json<serde_json::Value> {
        counters.oauth_exchanges.fetch_add(1, Ordering::SeqCst);
        Json(serde_json::json!({ "data": { "token": SYNTHETIC_JWT } }))
    }

    /// `POST /api/v1/remote/token`：只回 `data.token` 与正整数 `data.expires_in`，
    /// 并记录一次真实 terminal token 请求。
    async fn terminal_token(State(counters): State<FakeCounters>) -> Json<serde_json::Value> {
        counters
            .terminal_token_requests
            .fetch_add(1, Ordering::SeqCst);
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
        /// 注入合成 pending 卡（T9 首答腿）：非 None 时 conversation 快照带这张卡。
        pending_card: Arc<Mutex<Option<serde_json::Value>>>,
        /// 外部评审 C1：`model-selection.getView` 应答体。None = 既有空应答（目录
        /// 采不到）；Some = 真实目录形态（已知 build e2e 的目录形状来源）。
        model_catalog: Arc<Mutex<Option<serde_json::Value>>>,
        /// P2 回归注入：后续会话快照的 `config.model` 覆盖（见 FakeHostState）。
        snapshot_model: Arc<Mutex<Option<String>>>,
        /// 三轮 A：握手身份 / 严格校验 / 注入 fault（见 FakeHostState）。
        handshake_client_id: Arc<Mutex<Option<String>>>,
        strict_client_identity: Arc<Mutex<bool>>,
        command_fault: Arc<Mutex<Option<String>>>,
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

        /// 注入合成 pending 卡（须在 connect/bind 前设定：初始快照就带它）。
        fn set_pending_card(&self, card: serde_json::Value) {
            *self.pending_card.lock().expect("pending 卡锁") = Some(card);
        }

        /// 设定 `getView` 的合成目录（外部评审 C1；须在触发目录读前设定）。
        fn set_model_catalog(&self, catalog: serde_json::Value) {
            *self.model_catalog.lock().expect("目录锁") = Some(catalog);
        }

        /// 设定后续会话快照（ACK 跟进帧）的 `config.model`（P2 回归注入；
        /// 须在触发该帧的动作前设定）。
        fn set_snapshot_model(&self, model: &str) {
            *self.snapshot_model.lock().expect("快照模型锁") = Some(model.to_owned());
        }

        /// 三轮 A：开启严格身份校验（须在 connect 前设定；握手身份在 clientHello
        /// 到达时记录）。
        fn set_strict_client_identity(&self, strict: bool) {
            *self.strict_client_identity.lock().expect("严格身份锁") = strict;
        }

        /// 三轮 A：对写命令无条件回 202 指定 fault code（本地分类用例）。
        fn set_command_fault(&self, code: &str) {
            *self.command_fault.lock().expect("fault 锁") = Some(code.to_owned());
        }

        /// 三轮 A：clientHello 宣告的 clientId（断言命令身份与握手一致）。
        fn handshake_client_id(&self) -> Option<String> {
            self.handshake_client_id.lock().expect("握手身份锁").clone()
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
    /// pending = 测试注入的合成卡或空册、空 control），让生产的 bind/投影走完整
    /// read 面（不手推 SSE）。
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
        let pending_card = Arc::new(Mutex::new(None));
        let pending_card_per_conn = pending_card.clone();
        let model_catalog = Arc::new(Mutex::new(None));
        let model_catalog_per_conn = model_catalog.clone();
        let snapshot_model = Arc::new(Mutex::new(None));
        let snapshot_model_per_conn = snapshot_model.clone();
        let handshake_client_id = Arc::new(Mutex::new(None));
        let handshake_client_id_per_conn = handshake_client_id.clone();
        let strict_client_identity = Arc::new(Mutex::new(false));
        let strict_client_identity_per_conn = strict_client_identity.clone();
        let command_fault = Arc::new(Mutex::new(None));
        let command_fault_per_conn = command_fault.clone();
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
                    FakeHostState {
                        log: app_log.clone(),
                        commands: command_log.clone(),
                        ack: ack_mode.clone(),
                        pending_card: pending_card_per_conn.clone(),
                        model_catalog: model_catalog_per_conn.clone(),
                        snapshot_model: snapshot_model_per_conn.clone(),
                        handshake_client_id: handshake_client_id_per_conn.clone(),
                        strict_client_identity: strict_client_identity_per_conn.clone(),
                        command_fault: command_fault_per_conn.clone(),
                    },
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
            pending_card,
            model_catalog,
            snapshot_model,
            handshake_client_id,
            strict_client_identity,
            command_fault,
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

    /// 合成会话快照（与 bridge 单元测试里官方假体的快照同形）：`revision` 可参数化、
    /// 空 pending / 空 control、带当前选择 config。生产投影据此 `ready=true`。
    ///
    /// `seq` 与 `revision` 同值前进：`seq` 推进日志头指纹（send 确认的「新用户消息
    /// 可见」观察面），`revision` 推进会话水位（send 确认的「revision 前进」判据）。
    /// 同步合成会话快照；`model` 覆盖 `config.model`（P2 回归注入：宿主换模型的
    /// 实时快照）。
    fn synthetic_conversation_snapshot_with_model(
        session_id: &str,
        revision: u64,
        pending_cards: &[serde_json::Value],
        model: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "protocolVersion": 1,
            "sessionId": session_id,
            "logEpoch": "synthetic-epoch",
            "seq": revision,
            "revision": revision,
            "control": {"activeWorks": []},
            "pendingInteractions": pending_cards,
            "config": {
                "provider": "synthetic-provider",
                "model": model,
                "thought": "high",
            },
        })
    }

    /// 推一条该会话 topic 的 conversation snapshot（204 事件帧）：
    /// complete 包装 + `[fromSeq,toSeq]=[0,revision]`，与官方 wire 形态一致。
    async fn push_conversation_snapshot(
        socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        peer: &mut FakePeer,
        bridge: &str,
        session_id: &str,
        revision: u64,
        pending_cards: &[serde_json::Value],
        model: &str,
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
            "toSeq": revision,
            "sentAt": 0,
            "payload": {
                "kind": "snapshot",
                "snapshot": synthetic_conversation_snapshot_with_model(
                    session_id,
                    revision,
                    pending_cards,
                    model,
                ),
            },
        });
        let wire = serde_json::json!({
            "wireVersion": V4_WIRE_PROTOCOL_VERSION,
            "kind": "complete",
            "deliveryKind": "initial",
            "logicalFrameId": format!("synthetic-logical-{revision}"),
            "logicalFrameOrdinal": revision,
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
        host: &FakeHostState,
        payload: &serde_json::Value,
    ) {
        let FakeHostState {
            log,
            commands,
            ack,
            pending_card,
            model_catalog,
            snapshot_model,
            handshake_client_id,
            strict_client_identity,
            command_fault,
        } = host;
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
                // 三轮 A：记录 clientHello 宣告的协议身份（官方以它为命令身份基准）。
                if method == AGENT_CLIENT_HELLO_METHOD {
                    if let Ok(message) = decode_channel(&bytes) {
                        if let Some(id) = message
                            .body
                            .get(0)
                            .and_then(|hello| hello.get("clientId"))
                            .and_then(|value| value.as_str())
                        {
                            *handshake_client_id.lock().expect("握手身份锁") = Some(id.to_owned());
                        }
                    }
                }
                let mut command_client_id: Option<String> = None;
                if method == AGENT_SEND_COMMAND_METHOD {
                    // 只记录解出的命令信封（正文/身份都在里面），供断言「恰一帧、
                    // 正文原样离机、clientId 与握手身份一致」。
                    if let Ok(message) = decode_channel(&bytes) {
                        // body[0] = `{workspacePath, envelope}`（生产 CommandPort 适配层
                        // 产物）；协议身份在内层 envelope.clientId。
                        let wrapper = message.body.get(0).cloned().unwrap_or_default();
                        command_client_id = wrapper
                            .get("envelope")
                            .and_then(|envelope| envelope.get("clientId"))
                            .and_then(|value| value.as_str())
                            .map(str::to_owned);
                        commands.lock().expect("写命令日志锁").push(wrapper);
                    }
                }
                // 三轮 A：写命令的 202 fault 路径——注入 fault 或严格身份校验失败
                // （信封 clientId ≠ clientHello 身份）→ 官方语义 `fault.command.clientMismatch`。
                if method == AGENT_SEND_COMMAND_METHOD {
                    let injected = command_fault.lock().expect("fault 锁").clone();
                    let mismatch = *strict_client_identity.lock().expect("严格身份锁")
                        && handshake_client_id
                            .lock()
                            .expect("握手身份锁")
                            .as_deref()
                            .is_some_and(|handshake| {
                                command_client_id.as_deref() != Some(handshake)
                            });
                    let fault_code = injected
                        .or_else(|| mismatch.then(|| "fault.command.clientMismatch".to_owned()));
                    if let Some(code) = fault_code {
                        let body = serde_json::json!({
                            "code": code,
                            "message": "synthetic upstream text that must never leak",
                        });
                        let Ok(reply) = encode_promise_error_for_test(id, body) else {
                            return;
                        };
                        send_data(socket, peer.frame(&bridge, reply)).await;
                        return;
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
                    // 外部评审 C1：目录读应答（None = 既有空应答，目录采不到）。
                    "getView" => model_catalog
                        .lock()
                        .expect("目录锁")
                        .clone()
                        .unwrap_or_else(|| serde_json::json!({})),
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
                    peer.conversation = Some(session_id.clone());
                    // 初始快照的 pending 集合 = 测试注入的合成卡（未注入 = 空册）。
                    let cards = pending_card
                        .lock()
                        .expect("pending 卡锁")
                        .clone()
                        .map(|card| vec![card])
                        .unwrap_or_default();
                    push_conversation_snapshot(
                        socket,
                        peer,
                        &bridge,
                        &session_id,
                        1,
                        &cards,
                        "synthetic-model-a",
                    )
                    .await;
                }
                // T9：Accepted ACK 后推一条 revision/seq 前进的会话快照（revision=2），
                // 模拟宿主吸收用户消息/答案后的会话状态推进。这**不是**探针消息——
                // 它既不由生产发出、也不携带任何额外文本，只是让假体作为「宿主状态」
                // 的真实形态来源给出 spec R4 确认判据要求的观察证据：
                // - send：revision 前进 + 日志头指纹前进 = 新用户消息可见；
                // - answer：宿主整体替换 pending 册（作答的卡撤下）+ revision 前进
                //   = 「目标卡撤下」判据（wait_host_cleared 与泵观察同源）。
                // rejected/malformed 的 ACK 不推（宿主状态未前进，确认窗只能到点
                // inconclusive）。未订阅会话 topic 的连接不推（无从谈起会话状态）。
                if method == AGENT_SEND_COMMAND_METHOD
                    && *ack.lock().expect("ACK 模式锁") == AckMode::Accepted
                {
                    if let Some(session_id) = peer.conversation.clone() {
                        // 会话快照走该会话桥的 listen id；经 rpc-frame 的 bridge 就是
                        // 会话桥（send/resolve 命令只在会话桥上发出）。
                        let model = snapshot_model
                            .lock()
                            .expect("快照模型锁")
                            .clone()
                            .unwrap_or_else(|| "synthetic-model-a".to_owned());
                        push_conversation_snapshot(
                            socket,
                            peer,
                            &bridge,
                            &session_id,
                            2,
                            &[],
                            &model,
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
        /// 本连接上最近一次 `subscribeConversationV4` 的会话 id（send ACK 后的
        /// 跟进快照要推给它的 topic；未订阅过 = None，不推）。
        conversation: Option<String>,
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

    /// 假宿主跨连接共享的注入态/账本（每条连接克隆一份 Arc 句柄）。
    #[derive(Clone)]
    struct FakeHostState {
        log: Arc<Mutex<Vec<String>>>,
        commands: Arc<Mutex<Vec<serde_json::Value>>>,
        ack: Arc<Mutex<AckMode>>,
        pending_card: Arc<Mutex<Option<serde_json::Value>>>,
        model_catalog: Arc<Mutex<Option<serde_json::Value>>>,
        /// P2 回归注入：后续会话快照（ACK 跟进帧）的 `config.model` 覆盖。
        /// None = 合成默认 `synthetic-model-a`。
        snapshot_model: Arc<Mutex<Option<String>>>,
        /// 三轮 A（协议身份）：clientHello 宣告的 clientId（官方宿主以它为后续
        /// 命令的身份基准）。None = 尚未握手。
        handshake_client_id: Arc<Mutex<Option<String>>>,
        /// 三轮 A：严格身份校验开关——开启后写命令信封 `clientId` ≠ 握手身份即回
        /// 202 `fault.command.clientMismatch`（模拟官方实际行为），而非无条件 ACK。
        strict_client_identity: Arc<Mutex<bool>>,
        /// 三轮 A：对写命令无条件回指定 fault code 的 202（本地错误分类用例）。
        command_fault: Arc<Mutex<Option<String>>>,
    }

    /// 单条回环 WS 连接的握手/心跳应答（配对即刻完成）、app 请求分派与数据面回显。
    async fn serve_ws_handshake(
        stream: tokio::net::TcpStream,
        echoes: Arc<AtomicUsize>,
        host: FakeHostState,
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
                        serve_rpc_frame(&mut socket, &mut peer, &host, payload).await;
                        continue;
                    }
                    if let Some(zcode_type) = zcode_type {
                        host.log
                            .lock()
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

    /// 回环测试 HTTP 客户端：显式禁用系统代理（macOS 系统代理会把 127.0.0.1 也转发出去）。
    fn loopback_http() -> reqwest::Client {
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("测试 HTTP 客户端")
    }

    /// 走一次完整回环授权（BigModel 通道），返回已持有内存 JWT 的 `AuthService`。
    ///
    /// `open` 只做「交给系统浏览器」的替身：从授权 URL 解析回环 redirect 与 state，
    /// 再异步 GET 投递合成 code（真实浏览器在终端外的等价动作）。
    async fn authorized_service(
        fake: &FakeOfficialHttp,
        http: &reqwest::Client,
    ) -> Arc<AuthService> {
        authorized_service_with_store(fake, http, None).await
    }

    /// 同上，但授权成功的 JWT 会经 `store` 持久化（凭据复用用例的唯一差异点）：
    /// 生产 `AuthService` 在 `authorize` 成功后写系统存储，恢复时不再走 OAuth。
    async fn authorized_service_with_store(
        fake: &FakeOfficialHttp,
        http: &reqwest::Client,
        store: Option<Arc<MemoryCredentialStore>>,
    ) -> Arc<AuthService> {
        let mut service =
            AuthService::with_endpoints(http.clone(), fake.origin.clone(), fake.origin.clone());
        if let Some(store) = store {
            service = service.with_credential_store(store);
        }
        let auth = Arc::new(service);
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
    /// 首答腿的合成 pending 卡 id（与宿主 pendingInteractions 的 interactionId 精确相等）。
    const ANSWER_INTERACTION_ID: &str = "int_synthetic_answer_0001";
    /// 首答腿的合成题干（宿主原值；build_user_answer 以它为 answer 键）。
    const ANSWER_QUESTION_TEXT: &str = "合成问答：这个任务接下来要做什么？";
    /// 首答腿的合成选项 value（宿主原值；绝不回传 label）。
    const ANSWER_OPTION_VALUE: &str = "opt-continue";

    /// 合成 AskUserQuestion 单选卡（形状钉 `pending.rs` 解析的官方 wire 形态；
    /// 全合成值，不读真实宿主数据）。
    fn synthetic_user_input_card() -> serde_json::Value {
        serde_json::json!({
            "interactionId": ANSWER_INTERACTION_ID,
            "kind": "userInput",
            "anchorRowId": 7,
            "createdAt": 1_700_000_000_500u64,
            "payload": {
                "kind": "userInput",
                "prompt": "synthetic: pick one",
                "toolCallId": "call_synthetic_answer_0001",
                "toolName": "AskUserQuestion",
                "questions": [{
                    "header": "下一步",
                    "question": ANSWER_QUESTION_TEXT,
                    "multiSelect": false,
                    "options": [
                        {"value": ANSWER_OPTION_VALUE, "label": "继续实现"},
                        {"value": "opt-test-first", "label": "先写测试"},
                    ],
                }],
            },
        })
    }

    /// 首答腿的动作体（官方 tagged answer；单选回传宿主原值）。
    fn answer_body(action_id: &str, generation: u64) -> serde_json::Value {
        serde_json::json!({
            "actionId": action_id,
            "action": {
                "kind": "answer",
                "sessionId": SYNTHETIC_TASK_ID,
                "generation": generation,
                "interactionId": ANSWER_INTERACTION_ID,
                "answers": [{
                    "questionText": ANSWER_QUESTION_TEXT,
                    "selectedValues": [ANSWER_OPTION_VALUE],
                    "freeText": null,
                }],
            },
        })
    }

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

    /// 能力账本某行状态（T9 确认流转断言用；行不存在 = 测试失败）。
    /// 外部评审 P1-3：schema 形状指纹进 key 后，测试按 `platform|build|protocol|`
    /// 前缀匹配账本行（不再硬编码指纹哈希；前缀仍精确钉住 build+protocol）。
    fn ledger_key_prefix(build: &str) -> String {
        format!("{}|{}|3|", std::env::consts::OS, build)
    }

    fn capability_ledger_state(store: &DeviceStore, key_prefix: &str, capability: &str) -> String {
        store.with(|c| {
            c.query_row(
                "SELECT state FROM zcode_capability_evidence \
                 WHERE key LIKE ?1 || '%' AND capability = ?2",
                rusqlite::params![key_prefix, capability],
                |r| r.get::<_, String>(0),
            )
            .expect("zcode_capability_evidence 行必须存在")
        })
    }

    /// 能力账本某行判据词（confirmed 行必带结构化判据标识）。
    fn capability_ledger_evidence(
        store: &DeviceStore,
        key_prefix: &str,
        capability: &str,
    ) -> String {
        store.with(|c| {
            c.query_row(
                "SELECT evidence FROM zcode_capability_evidence \
                 WHERE key LIKE ?1 || '%' AND capability = ?2",
                rusqlite::params![key_prefix, capability],
                |r| r.get::<_, String>(0),
            )
            .expect("zcode_capability_evidence 行必须存在")
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

    /// T9 全链装配（resolver 驱动形态）：与 [`full_chain`] 同一回环假官方 + 生产组件，
    /// 但 Hub 用**生产同源**的 capability resolver 策略源与确认账本（不注入静态矩阵，
    /// 与生产 `ZcodeHub::new` 同一拼法），宿主 build 探针由调用方注入——未知 build
    /// 用例的确定性来源（真实探针读本机 plist，跨机器/跨时间不可复现）。
    async fn resolver_chain(host_build: HostBuildProbe) -> Chain {
        let fake = FakeOfficialHttp::start().await;
        // 显式禁用系统代理（macOS 系统代理会把 127.0.0.1 也转发出去）。
        let http = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("测试 HTTP 客户端");
        let auth = authorized_service(&fake, &http).await;
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("回环 WS 端口应可绑定");
        let ws = spawn_ws_handshake(listener);

        let conn = shared_conn();
        seed(&conn);
        let store = DeviceStore::Owned(conn.clone());
        let (parts, events, lag) = PartsBackend::production_test_with_host_build(
            store.clone(),
            auth.clone(),
            ws.origin.clone(),
            host_build,
        );
        let bridge = parts.pump_bridge();
        let policy_source = parts.policy_source();
        let ledger = parts.capability_ledger();
        let backend: Arc<dyn HubBackend> = Arc::new(parts);
        let hub = Arc::new(ZcodeHub::with_pump_policy_source_and_confirmation(
            store.clone(),
            backend,
            policy_source,
            events,
            bridge,
            lag,
            ledger,
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

    /// 「整进程重启」共享世界：同一个回环假官方、同一个 WS 假体、同一个合成库、
    /// 同一个 fake 系统凭据存储。每次模拟新进程只**重新构造** `AuthService`/`ZcodeHub`，
    /// 绝不复用已授权的 AuthService，也不用 `mark_authorized_for_test` 冒充恢复。
    struct PersistentWorld {
        fake: FakeOfficialHttp,
        ws: FakeWsHandshake,
        conn: SharedConn,
        vault: Arc<MemoryCredentialStore>,
    }

    impl PersistentWorld {
        async fn start() -> Self {
            let fake = FakeOfficialHttp::start().await;
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .expect("回环 WS 端口应可绑定");
            let ws = spawn_ws_handshake(listener);
            let conn = shared_conn();
            seed(&conn);
            // 自动连接门：远程总开关开启 + 链接已保存（auto 意图缺键 = true）。
            DeviceStore::Owned(conn.clone()).with(|c| {
                try_set_setting_conn(c, KEY_ENABLED, "true").expect("开启远程总开关");
            });
            Self {
                fake,
                ws,
                conn,
                vault: Arc::new(MemoryCredentialStore::new()),
            }
        }

        fn store(&self) -> DeviceStore {
            DeviceStore::Owned(self.conn.clone())
        }

        /// 新进程的 AuthService：只共享 fake 系统存储，不共享内存 JWT/epoch/取消态。
        fn new_auth(&self) -> Arc<AuthService> {
            let auth = AuthService::with_endpoints(
                loopback_http(),
                self.fake.origin.clone(),
                self.fake.origin.clone(),
            )
            .with_credential_store(self.vault.clone());
            Arc::new(auth)
        }

        /// 新进程的 Hub：真实生产组件（Relay/Bridge/CommandSender + 事件泵），
        /// 只把回环端点注入 relay；不使用 `mark_authorized_for_test`。
        fn new_hub(&self, auth: Arc<AuthService>, policy: CapabilityView) -> Arc<ZcodeHub> {
            let (parts, events, lag) =
                PartsBackend::production_test(self.store(), auth, self.ws.origin.clone());
            let bridge = parts.pump_bridge();
            let backend: Arc<dyn HubBackend> = Arc::new(parts);
            Arc::new(ZcodeHub::with_pump_and_lag(
                self.store(),
                backend,
                policy,
                events,
                bridge,
                lag,
            ))
        }

        /// 首次进程：真实回环 OAuth 恰一次（JWT 落 fake 持久存储）→ 既有自动连接 worker。
        async fn oauth_and_auto_connect(
            &self,
            policy: CapabilityView,
        ) -> (Arc<AuthService>, Arc<ZcodeHub>) {
            let auth = authorized_service_with_store(
                &self.fake,
                &loopback_http(),
                Some(self.vault.clone()),
            )
            .await;
            let hub = self.new_hub(auth.clone(), policy);
            hub.start_auto_connect();
            wait_paired(&hub).await;
            (auth, hub)
        }

        /// 重启后的进程：先做启动恢复（读 fake 持久存储，绝不 OAuth），再启动自动连接。
        async fn restart(
            &self,
            policy: CapabilityView,
        ) -> (AuthStatus, Arc<AuthService>, Arc<ZcodeHub>) {
            let auth = self.new_auth();
            let hub = self.new_hub(auth.clone(), policy);
            let status = hub.restore_authorization().await;
            hub.start_auto_connect();
            (status, auth, hub)
        }
    }

    /// 等到自动连接 worker 把真实回环 relay 配对完成。
    async fn wait_paired(hub: &ZcodeHub) {
        wait_until(|| hub.connection().state == ConnectionState::Paired).await;
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
            envelope["clientId"], PROTOCOL_CLIENT_ID,
            "协议 clientId 与握手同源（三轮 A）；手机 device_id 只留在 Tuvis 侧归属"
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

    /// T9 e2e（spec §7.3/§7.6/§8）：未知 build（shipped 表不命中）走 resolver 的
    /// 接口候选（A 策略）；send 首条真实消息完成「accepted → revision 前进 →
    /// 账本 confirmed」全链；全程**恰一条**命令帧（零探针消息、零自动重发）。
    ///
    /// 与既有全链用例的关键差别：Hub 不注入静态矩阵——策略矩阵与确认落账都走
    /// 生产装配的 capability resolver + DB 账本（同 `ZcodeHub::new` 拼法），宿主
    /// build 确定性注入为未知值。绝不访问真实官方端点/凭据（全链回环假体）。
    #[tokio::test]
    async fn unknown_build_send_confirms_without_probe_message() {
        // 未知 build：`9.9.9.9999` 不在 shipped 表（唯一已知 `3.16.2.8769`），
        // 平台/协议无关——shipped 表精确命中失败 → 只剩接口候选可判。
        let unknown_build: HostBuildProbe = Arc::new(|| Some("9.9.9.9999".to_owned()));
        let chain = resolver_chain(unknown_build).await;
        let app = router(remote_state(
            &chain.conn,
            chain.hub.clone(),
            chain.injector.clone(),
        ));
        let generation = chain.hub.connection().generation;
        // 能力 key 与 resolver 同一拼法（platform|build|protocol|schema|epoch；
        // schema 占位 "v4"、epoch 0 是 T4 裁决的生产常量）。
        let key = ledger_key_prefix("9.9.9.9999");

        // ---- 1) resolver 候选开放：真实 bind/订阅/形状采集后 send enabled（无原因码）----
        let session = bind_and_wait_ready(&app, SYNTHETIC_TASK_ID).await;
        assert_eq!(
            session["capabilities"]["send"]["enabled"], true,
            "未知 build 接口候选必须开放 send：{session}"
        );
        assert!(
            session["capabilities"]["send"]["reasonCode"].is_null(),
            "候选开放不带原因码（UI 只见可用）：{session}"
        );
        // 候选只开接口形状齐备的项：合成快照无 pending/control/目录 → 其余七项
        // 仍是 needs_evidence（形状缺席不开闸，spec R3）。
        for capability in [
            "singleSelect",
            "multiSelect",
            "freeText",
            "model",
            "interrupt",
        ] {
            assert_eq!(
                session["capabilities"][capability]["enabled"], false,
                "{capability} 形状缺席必须保持关闭：{session}"
            );
            assert_eq!(
                session["capabilities"][capability]["reasonCode"],
                codes::NEEDS_EVIDENCE,
                "{capability} 关闭必须给 needs_evidence 原因：{session}"
            );
        }
        assert_eq!(session["revision"].as_u64(), Some(1));

        // ---- 2) 发一条真实 send：accepted + 恰一条命令帧（正文原样、clientId 来自配对门）----
        let (status, receipt) =
            post_action(&app, &send_body(ACTION_ID, generation, SEND_TEXT)).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "候选开放即可真实发送（A 策略）：{receipt}"
        );
        assert_eq!(receipt["state"], "accepted");
        assert_eq!(
            chain.ws.commands().len(),
            1,
            "整条链只允许一帧命令信封：{:#?}",
            chain.ws.commands()
        );
        let envelope = &chain.ws.commands()[0]["envelope"];
        assert_eq!(envelope["type"], SEND_TEXT_TYPE);
        assert_eq!(
            envelope["payload"]["text"], SEND_TEXT,
            "正文必须原样离机，绝不携带探针文本"
        );
        assert_eq!(envelope["clientId"], PROTOCOL_CLIENT_ID);
        assert_eq!(chain.injector.calls(), 0, "官方路径不得回落终端注入");

        // ---- 3) revision 前进：send 后会话水位推进（宿主状态前进的可观察证据）----
        // （RED 阶段：假体不推跟进快照 → revision 停在 1 → 本断言失败。）
        let advanced = async {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            loop {
                let (_, body) = get_json(
                    &app,
                    &format!("/m/api/v1/zcode/session?sessionId={SYNTHETIC_TASK_ID}"),
                )
                .await;
                if body["revision"].as_u64() == Some(2) {
                    return true;
                }
                if tokio::time::Instant::now() >= deadline {
                    return false;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        assert!(
            advanced.await,
            "send 后会话 revision 必须前进到 2（宿主吸收消息的推进证据）"
        );

        // ---- 4) 确认流转：泵观察「revision 前进 + 新用户消息指纹」→ 账本 confirmed ----
        // （RED 阶段：无新投影证据 → 确认窗到点 inconclusive → 本断言失败。）
        wait_until(|| {
            chain.store.with(|c| {
                c.query_row(
                    "SELECT state FROM zcode_capability_evidence WHERE key LIKE ?1 || '%' AND capability = 'send'",
                    rusqlite::params![key],
                    |r| r.get::<_, String>(0),
                )
                .ok()
                .as_deref()
                == Some("confirmed")
            })
        })
        .await;
        assert_eq!(
            capability_ledger_state(&chain.store, &key, "send"),
            "confirmed",
            "未知 build 的 send 首次真实使用必须确认落账"
        );
        assert_eq!(
            capability_ledger_evidence(&chain.store, &key, "send"),
            "revision_advanced+new_user_message",
            "confirmed 行必须携带结构化判据词（无正文）"
        );

        // ---- 5) 确认后矩阵仍开放：confirmed 优先级不降级，同一 resolver 现读 ----
        let (_, after) = get_json(
            &app,
            &format!("/m/api/v1/zcode/session?sessionId={SYNTHETIC_TASK_ID}"),
        )
        .await;
        assert_eq!(
            after["capabilities"]["send"]["enabled"], true,
            "confirmed 后 send 仍可用（账本判决被 resolver 消费）：{after}"
        );

        // ---- 6) 无探针消息：业务请求序列逐条固定 + 静置观察窗仍恰一帧 ----
        // 整条链的业务请求恰为：bootstrap → listing → 枚举桥 → 控制器订阅（枚举桥
        // 上）→ 会话桥 → hello → clientHello → 会话订阅 → **用户的真实 send**。
        // 多一帧、少一帧、顺序漂移（如 send 后重新 bind/订阅）都说明链路里有额外动作。
        let business = upstream_business_requests(&chain.ws.log());
        assert_eq!(
            business,
            vec![
                BOOTSTRAP_REQUEST.to_owned(),
                WORKSPACE_LIST_REQUEST.to_owned(),
                WORKSPACE_BRIDGE_OPEN.to_owned(),
                CONTROLLER_SUBSCRIBE_METHOD.to_owned(),
                WORKSPACE_BRIDGE_OPEN.to_owned(),
                AGENT_HELLO_METHOD.to_owned(),
                AGENT_CLIENT_HELLO_METHOD.to_owned(),
                AGENT_SUBSCRIBE_CONVERSATION_METHOD.to_owned(),
                AGENT_SEND_COMMAND_METHOD.to_owned(),
            ],
            "业务请求序列必须恰为 bind/订阅 + 用户的一条 send（零探针）：{business:?}"
        );
        // 静置观察窗：确认落账后也不得冒出第二帧（零探针、零自动重发）。
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(chain.ws.commands().len(), 1, "静置期后仍只允许一帧命令信封");
    }

    /// 外部评审 C1 回归 e2e：**已知 build**（`3.16.2.8769`，shipped 表 model/thought
    /// = Confirmed）走生产 resolver 全链，model/thought 必须 enabled。
    ///
    /// 修复前的自锁：`has_model_catalog` 只在写闸之后的 `SelectionPort::load` 里采集，
    /// 闸又要求 enabled → 目录永远采不到 → 永远 needs_evidence（已知 build 也打不开）。
    /// 修复后读路径（GET /zcode/session）惰性单飞读一次 getView（不经能力闸），
    /// 目录形状进 evidence → resolver 放行 shipped confirmed。
    ///
    /// 本用例同时钉住：目录读恰一帧 getView、无探针文本、零命令帧（只读），以及
    /// shipped confirmed 的 send 不随观察落账（I1：出厂判定不进首次确认）。
    #[tokio::test]
    async fn known_build_opens_model_and_thought_via_catalog_read() {
        let known_build: HostBuildProbe = Arc::new(|| Some("3.16.2.8769".to_owned()));
        let chain = resolver_chain(known_build).await;
        // 已知 build 的宿主必须能回真实目录（只认当前 provider）。
        chain.ws.set_model_catalog(serde_json::json!({
            "providers": [
                {"providerId": "synthetic-provider", "models": [
                    {"modelId": "synthetic-model-a", "config": {"optionSpecs": {"reasoningLevel": {"values": ["low", "high"]}}}},
                    {"modelId": "synthetic-model-b", "config": {"optionSpecs": {"reasoningLevel": {"values": ["low"]}}}}
                ]}
            ]
        }));
        let app = router(remote_state(
            &chain.conn,
            chain.hub.clone(),
            chain.injector.clone(),
        ));

        // 第一次 GET：bind + 订阅 + 快照就绪；读路径顺带惰性读目录。目录读与快照就绪
        // 可能同帧或下一帧生效，所以按能力位轮询（有界 10s）。
        let mut session = bind_and_wait_ready(&app, SYNTHETIC_TASK_ID).await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while session["capabilities"]["model"]["enabled"] != true {
            assert!(
                tokio::time::Instant::now() < deadline,
                "已知 build 的 model 未在时限内开放（目录读自锁未解）：{session}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
            let (status, body) = get_json(
                &app,
                &format!("/m/api/v1/zcode/session?sessionId={SYNTHETIC_TASK_ID}"),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            session = body;
        }
        assert_eq!(
            session["capabilities"]["model"]["enabled"], true,
            "已知 build shipped confirmed + 目录形状 → model 可用：{session}"
        );
        assert!(session["capabilities"]["model"]["reasonCode"].is_null());
        assert_eq!(
            session["capabilities"]["thoughtLevel"]["enabled"], true,
            "已知 build shipped confirmed + 档位形状 → thought_level 可用：{session}"
        );
        assert_eq!(
            session["capabilities"]["send"]["enabled"], true,
            "已知 build shipped confirmed send 可用：{session}"
        );
        // 目录写回视图：模型下拉有真实候选（非扁平快照的单项）。
        let models = session["selection"]["models"]
            .as_array()
            .expect("selection.models 数组");
        let ids: Vec<&str> = models
            .iter()
            .filter_map(|m| m["modelId"].as_str())
            .collect();
        assert!(
            ids.contains(&"synthetic-model-a") && ids.contains(&"synthetic-model-b"),
            "目录模型应写回视图：{ids:?}"
        );

        // 只读链：零命令帧、零终端注入；目录读恰一帧 getView（单飞、每 revision 一次）。
        assert!(chain.ws.commands().is_empty(), "只读链不得发任何命令帧");
        assert_eq!(chain.injector.calls(), 0);
        let get_views = chain
            .ws
            .log()
            .into_iter()
            .filter(|method| method == "getView")
            .count();
        assert_eq!(
            get_views,
            1,
            "目录读恰一次（同 revision 不重试）：{:?}",
            chain.ws.log()
        );

        // 再 GET 若干次：目录读不再重复（revision 未前进）。
        for _ in 0..3 {
            let (status, _) = get_json(
                &app,
                &format!("/m/api/v1/zcode/session?sessionId={SYNTHETIC_TASK_ID}"),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
        }
        let get_views_after = chain
            .ws
            .log()
            .into_iter()
            .filter(|method| method == "getView")
            .count();
        assert_eq!(get_views_after, 1, "同 revision 重复 GET 零新目录读");

        // I1：shipped confirmed 的能力不进首次确认——账本对该已知 build key 保持空。
        // P1-3：按 build+protocol 前缀断言「零账本行」——指纹参与 key 后前缀仍
        // 精确钉住该 build，任何指纹下的行都不会漏检。
        let key = ledger_key_prefix("3.16.2.8769");
        let rows: i64 = chain
            .conn
            .lock()
            .expect("conn")
            .query_row(
                "SELECT COUNT(*) FROM zcode_capability_evidence WHERE key LIKE ?1 || '%'",
                [&key],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(rows, 0, "已知 build shipped confirmed 零账本行");
    }

    /// 三轮 A 回归 e2e：**严格假宿主执行官方身份校验**——写命令信封 `clientId`
    /// 必须等于 clientHello 宣告的握手身份（官方对不一致回
    /// `fault.command.clientMismatch`）。旧实现命令信封用手机 device_id（≠ 握手
    /// 身份 `tuvis-remote`），严格宿主下全部写动作被拒；修复后命令与握手同源，
    /// 手机 device_id 只留在 Tuvis 侧（配对/审计/幂等归属）。
    #[tokio::test]
    async fn strict_host_enforces_handshake_client_identity() {
        let chain = resolver_chain(Arc::new(|| Some("9.9.9.9999".to_owned()))).await;
        // 严格身份校验在连接建立前开启：握手身份被记录，命令信封逐一核对。
        chain.ws.set_strict_client_identity(true);
        let app = router(remote_state(
            &chain.conn,
            chain.hub.clone(),
            chain.injector.clone(),
        ));

        let session = bind_and_wait_ready(&app, SYNTHETIC_TASK_ID).await;
        assert_eq!(
            session["capabilities"]["send"]["enabled"], true,
            "{session}"
        );
        let generation = chain.hub.connection().generation;

        // 真实 send：严格宿主只放行与握手身份一致的命令。
        let (status, receipt) =
            post_action(&app, &send_body(ACTION_ID, generation, SEND_TEXT)).await;
        assert_eq!(status, StatusCode::OK, "{receipt}");
        assert_eq!(
            receipt["state"], "accepted",
            "命令身份与握手一致时必须被严格宿主受理：{receipt}"
        );
        assert_eq!(chain.injector.calls(), 0, "不得回落终端注入");

        // 命令信封身份 == clientHello 宣告身份（同一来源，逐字段断言）。
        let handshake = chain
            .ws
            .handshake_client_id()
            .expect("clientHello 必须宣告协议身份");
        assert!(!handshake.is_empty());
        let envelope = &chain.ws.commands()[0]["envelope"];
        assert_eq!(
            envelope["clientId"], handshake,
            "命令协议身份必须与握手同源（不得混入手机 device_id）：{envelope}"
        );
        assert_ne!(
            envelope["clientId"], DEVICE_ID,
            "手机 device_id 不得进入协议信封"
        );
        assert_eq!(envelope["type"], SEND_TEXT_TYPE);
        assert_eq!(envelope["payload"]["text"], SEND_TEXT);

        // 手机 device_id 仍用于 Tuvis 侧归属：zcode_actions 行的 device = 配对设备。
        let device_rows: i64 = chain.store.with(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM zcode_actions WHERE device_id = ?1 AND state = 'accepted'",
                rusqlite::params![DEVICE_ID],
                |r| r.get(0),
            )
            .expect("zcode_actions 查询")
        });
        assert_eq!(device_rows, 1, "动作归属必须是配对手机 device_id");
    }

    /// 三轮 A 回归 e2e：官方 `fault.command.clientMismatch`（202）必须被**本地分类**为
    /// 冻结码 `client_mismatch`——不透传上游原文、不自动重发、不写能力账本（身份
    /// 错误不是能力失败，不得把 build 误判为不支持该能力）。
    #[tokio::test]
    async fn client_mismatch_fault_classifies_locally_without_resend_or_degrade() {
        let chain = resolver_chain(Arc::new(|| Some("9.9.9.9999".to_owned()))).await;
        // 无条件回 202 clientMismatch（模拟官方对身份不一致的拒绝）。
        chain.ws.set_command_fault("fault.command.clientMismatch");
        let app = router(remote_state(
            &chain.conn,
            chain.hub.clone(),
            chain.injector.clone(),
        ));
        let _session = bind_and_wait_ready(&app, SYNTHETIC_TASK_ID).await;
        let generation = chain.hub.connection().generation;

        let (status, receipt) =
            post_action(&app, &send_body(ACTION_ID, generation, SEND_TEXT)).await;
        assert_eq!(status, StatusCode::OK, "{receipt}");
        assert_eq!(
            receipt["state"], "unknown",
            "身份错误不伪装成 accepted：{receipt}"
        );
        assert_eq!(
            receipt["reasonCode"],
            codes::CLIENT_MISMATCH,
            "必须是本地冻结分类，而非笼统 unknown：{receipt}"
        );
        let rendered = receipt.to_string();
        assert!(
            !rendered.contains("synthetic upstream text"),
            "上游原文绝不透传到回执：{rendered}"
        );

        // 不自动重发：静置后仍恰一帧命令；不回落终端注入。
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(chain.ws.commands().len(), 1, "身份错误不得触发重发");
        assert_eq!(chain.injector.calls(), 0);

        // 不写能力账本：身份/网络类错误不是能力失败，send 保持候选。
        let key = ledger_key_prefix("9.9.9.9999");
        let rows: i64 = chain.store.with(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM zcode_capability_evidence WHERE key LIKE ?1 || '%'",
                [&key],
                |r| r.get(0),
            )
            .expect("count")
        });
        assert_eq!(rows, 0, "身份错误不得写 degraded/任何账本行");
        let (status, session) = get_json(
            &app,
            &format!("/m/api/v1/zcode/session?sessionId={SYNTHETIC_TASK_ID}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            session["capabilities"]["send"]["enabled"], true,
            "send 能力不因身份错误被关闭：{session}"
        );
    }

    /// 外部评审 P2 回归 e2e：**实时广播不得覆盖合并目录**。
    ///
    /// GET 经读路径合并 `getView` 目录；随后宿主推一条正常快照（revision 2、换
    /// 模型），Hub 广播的 Session 事件——即 `/zcode/events` 的 `event: session`
    /// 载荷源（`control_frame` 只过滤转发 `ControlEvent.data`，手机
    /// `useZcodeControl` 按它整体覆盖状态）——必须**仍带完整目录**且当前选择随
    /// 宿主更新。修复前泵广播面用原始投影，模型下拉掉回单项。
    #[tokio::test]
    async fn session_broadcast_keeps_full_catalog_across_live_delta() {
        let chain = resolver_chain(Arc::new(|| Some("9.9.9.9999".to_owned()))).await;
        chain.ws.set_model_catalog(serde_json::json!({
            "providers": [
                {"providerId": "synthetic-provider", "models": [
                    {"modelId": "synthetic-model-a", "config": {"optionSpecs": {"reasoningLevel": {"values": ["low", "high"]}}}},
                    {"modelId": "synthetic-model-b", "config": {"optionSpecs": {"reasoningLevel": {"values": ["low"]}}}}
                ]}
            ]
        }));
        let app = router(remote_state(
            &chain.conn,
            chain.hub.clone(),
            chain.injector.clone(),
        ));

        // ---- 1) GET：读路径合并目录，两个模型都在（当前 a）----
        let session = bind_and_wait_ready(&app, SYNTHETIC_TASK_ID).await;
        let ids: Vec<&str> = session["selection"]["models"]
            .as_array()
            .expect("selection.models 数组")
            .iter()
            .filter_map(|m| m["modelId"].as_str())
            .collect();
        assert_eq!(
            ids,
            ["synthetic-model-a", "synthetic-model-b"],
            "GET 必须给出合并后的完整目录：{session}"
        );
        assert_eq!(session["selection"]["model"], "synthetic-model-a");

        // ---- 2) 订阅 Hub 广播（SSE 会话事件的同源载荷）----
        let mut events = chain.hub.subscribe();

        // ---- 3) 宿主正常推进：注入「换模型」覆盖后发一条 send，假体回 ACK 跟进
        // 快照（revision 2、config.model = b）——这是一条普通实时帧 ----
        chain.ws.set_snapshot_model("synthetic-model-b");
        let generation = chain.hub.connection().generation;
        let (status, receipt) =
            post_action(&app, &send_body(ACTION_ID, generation, SEND_TEXT)).await;
        assert_eq!(status, StatusCode::OK, "{receipt}");
        assert_eq!(receipt["state"], "accepted");

        // ---- 4) 广播 Session 事件（revision 2）：完整目录保留 + 选择随宿主更新 ----
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let payload = loop {
            let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
                .await
                .expect("广播事件读取超时")
                .expect("广播通道不关闭");
            if event.kind != ControlEventKind::Session {
                continue;
            }
            if event.data["revision"].as_u64() != Some(2) {
                continue;
            }
            break event.data;
        };
        assert!(
            tokio::time::Instant::now() < deadline,
            "时限内未取得 revision 2 的 Session 广播"
        );
        assert_eq!(
            payload["selection"]["model"], "synthetic-model-b",
            "当前选择必须随宿主快照更新：{payload}"
        );
        let ids: Vec<&str> = payload["selection"]["models"]
            .as_array()
            .expect("实时广播必须仍带 selection.models")
            .iter()
            .filter_map(|m| m["modelId"].as_str())
            .collect();
        assert_eq!(
            ids,
            ["synthetic-model-a", "synthetic-model-b"],
            "实时帧不得覆盖合并目录（P2）：{payload}"
        );

        // ---- 5) GET 再次核对：实时帧之后目录仍在 ----
        let (status, session_after) = get_json(
            &app,
            &format!("/m/api/v1/zcode/session?sessionId={SYNTHETIC_TASK_ID}"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let ids: Vec<&str> = session_after["selection"]["models"]
            .as_array()
            .expect("selection.models 数组")
            .iter()
            .filter_map(|m| m["modelId"].as_str())
            .collect();
        assert_eq!(
            ids,
            ["synthetic-model-a", "synthetic-model-b"],
            "实时帧后的 GET 仍须保留完整目录：{session_after}"
        );
    }

    /// T9 补腿 e2e（brief 验收「pending 卡首答确认」）：未知 build 的会话快照带一张
    /// 合成 AskUserQuestion 单选卡 → resolver 候选开放 single_select → 提交一条真实
    /// Answer（经生产 check_answer_precheck/authorize/build_user_answer 全链）→
    /// 宿主撤卡 + revision 前进 → 账本 confirmed；全程**恰一条**答案命令帧
    /// （resolveInteraction，无探针文本、无自动重发）。
    ///
    /// 确认走两条生产路径之一（都断言接受）：派发内 `wait_host_cleared` 命中 →
    /// 回执 `confirmed`、回执终态落账（判据词 `pending_cleared`）；或回执 accepted
    /// 后事件泵观察「目标卡撤下 + revision 前进」先落账（判据词
    /// `pending_cleared+revision_advanced`）。两条路径都是同一 unknown-build key 上
    /// 恰一条 confirmed 行。
    #[tokio::test]
    async fn unknown_build_answer_confirms_pending_without_probe_message() {
        let unknown_build: HostBuildProbe = Arc::new(|| Some("9.9.9.9999".to_owned()));
        let chain = resolver_chain(unknown_build).await;
        // 初始快照带合成单选卡（须在 connect 前注入）。
        chain.ws.set_pending_card(synthetic_user_input_card());
        let app = router(remote_state(
            &chain.conn,
            chain.hub.clone(),
            chain.injector.clone(),
        ));
        let generation = chain.hub.connection().generation;
        let key = ledger_key_prefix("9.9.9.9999");

        // ---- 1) resolver 候选开放：卡形状在场 → single_select 候选可用（无原因码）----
        let session = bind_and_wait_ready(&app, SYNTHETIC_TASK_ID).await;
        assert_eq!(
            session["pending"][0]["interactionId"], ANSWER_INTERACTION_ID,
            "合成卡必须随真实投影上会话视图：{session}"
        );
        assert_eq!(
            session["capabilities"]["singleSelect"]["enabled"], true,
            "未知 build 上 user_input 卡形状必须开放 single_select 候选：{session}"
        );
        assert!(
            session["capabilities"]["singleSelect"]["reasonCode"].is_null(),
            "候选开放不带原因码（UI 只见可用）：{session}"
        );
        assert_eq!(
            session["capabilities"]["approvalReply"]["enabled"], false,
            "无 permission 形状不得开放审批：{session}"
        );

        // ---- 2) 提交一条真实 Answer：回执 confirmed（宿主撤卡因果终态）----
        // （RED 证据：假体跟进快照不撤卡时，wait_host_cleared 到点未命中 → 回执停在
        // accepted，本断言失败（10s 窗）——「宿主未撤卡不得转正」的可证伪面。）
        let (status, receipt) = post_action(&app, &answer_body(ACTION_ID, generation)).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "候选开放即可真实作答（A 策略）：{receipt}"
        );
        assert_eq!(
            receipt["state"], "confirmed",
            "宿主撤下目标卡后 Answer 必须拿到因果终态 confirmed：{receipt}"
        );
        assert_eq!(
            chain.ws.commands().len(),
            1,
            "恰一条答案命令帧：{:#?}",
            chain.ws.commands()
        );
        let envelope = &chain.ws.commands()[0]["envelope"];
        assert_eq!(envelope["type"], RESOLVE_INTERACTION_TYPE);
        assert_eq!(
            envelope["payload"]["interactionId"], ANSWER_INTERACTION_ID,
            "答案命令必须精确指向宿主卡：{envelope}"
        );
        assert_eq!(
            envelope["payload"]["answer"]["content"]["answers"][ANSWER_QUESTION_TEXT],
            ANSWER_OPTION_VALUE,
            "单选答案以宿主题干为键、回传宿主原值：{envelope}"
        );
        assert_eq!(envelope["clientId"], PROTOCOL_CLIENT_ID);
        assert_eq!(chain.injector.calls(), 0, "官方路径不得回落终端注入");

        // ---- 3) 宿主撤卡 + revision 前进：GET 会话 pending 清空、水位 2 ----
        let cleared = async {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            loop {
                let (_, body) = get_json(
                    &app,
                    &format!("/m/api/v1/zcode/session?sessionId={SYNTHETIC_TASK_ID}"),
                )
                .await;
                if body["revision"].as_u64() == Some(2)
                    && body["pending"].as_array().is_some_and(|cards| {
                        cards
                            .iter()
                            .all(|card| card["interactionId"] != ANSWER_INTERACTION_ID)
                    })
                {
                    return true;
                }
                if tokio::time::Instant::now() >= deadline {
                    return false;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        assert!(
            cleared.await,
            "作答后宿主必须撤下目标卡且 revision 前进（确认判据的可观察证据）"
        );

        // ---- 4) 确认流转：账本 confirmed（两条生产路径任一，判据词按实际走法）----
        wait_until(|| {
            chain.store.with(|c| {
                c.query_row(
                    "SELECT state FROM zcode_capability_evidence WHERE key LIKE ?1 || '%' AND capability = 'single_select'",
                    rusqlite::params![key],
                    |r| r.get::<_, String>(0),
                )
                .ok()
                .as_deref()
                == Some("confirmed")
            })
        })
        .await;
        assert_eq!(
            capability_ledger_state(&chain.store, &key, "single_select"),
            "confirmed"
        );
        assert!(
            matches!(
                capability_ledger_evidence(&chain.store, &key, "single_select").as_str(),
                "pending_cleared" | "pending_cleared+revision_advanced"
            ),
            "confirmed 判据词必须是两条生产路径之一：{}",
            capability_ledger_evidence(&chain.store, &key, "single_select")
        );

        // ---- 5) 无探针消息：业务请求序列逐条固定 + 静置观察窗仍恰一帧 ----
        let business = upstream_business_requests(&chain.ws.log());
        assert_eq!(
            business,
            vec![
                BOOTSTRAP_REQUEST.to_owned(),
                WORKSPACE_LIST_REQUEST.to_owned(),
                WORKSPACE_BRIDGE_OPEN.to_owned(),
                CONTROLLER_SUBSCRIBE_METHOD.to_owned(),
                WORKSPACE_BRIDGE_OPEN.to_owned(),
                AGENT_HELLO_METHOD.to_owned(),
                AGENT_CLIENT_HELLO_METHOD.to_owned(),
                AGENT_SUBSCRIBE_CONVERSATION_METHOD.to_owned(),
                AGENT_SEND_COMMAND_METHOD.to_owned(),
            ],
            "业务请求序列必须恰为 bind/订阅 + 用户的一条 answer（零探针）：{business:?}"
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            chain.ws.commands().len(),
            1,
            "静置期后仍只允许一帧命令信封（零探针、零重发）"
        );
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

    /// 整进程重启：第一次 OAuth + 自动连接后**丢弃**旧 AuthService/Hub，新进程只
    /// 重新构造 AuthService/Hub 并从共享 fake 系统存储恢复 → 第二次 terminal token /
    /// WS 配对；OAuth 交换仍只有第一次那一次（不二次授权）。
    #[tokio::test]
    async fn whole_new_process_state_reuses_vault_without_oauth() {
        let world = PersistentWorld::start().await;
        let (auth, hub) = world.oauth_and_auto_connect(send_policy()).await;
        assert_eq!(world.fake.oauth_exchanges(), 1, "首次 OAuth 交换必须恰一次");
        assert_eq!(
            world.fake.terminal_token_requests(),
            1,
            "首次自动连接现取一次 terminal token"
        );
        assert_eq!(hub.connection().state, ConnectionState::Paired);
        assert_eq!(
            world.vault.stored().expect("授权 JWT 已落 fake 存储").jwt,
            SYNTHETIC_JWT,
            "持久记录必须来自真实授权交换"
        );
        let first_hub_generation = hub.connection().generation;

        // 进程退出：显式断开旧连接（保留账号），再丢弃旧 AuthService/Hub。
        let closed = hub.disconnect().await;
        assert_eq!(closed.state, ConnectionState::Offline);
        drop(hub);
        drop(auth);

        // 新进程：全新的 AuthService（无内存 JWT）+ 全新 Hub，只共享 fake 持久存储。
        let fresh_auth = world.new_auth();
        assert!(
            !fresh_auth.status().authorized,
            "新建 AuthService 不得携带旧进程的内存授权"
        );
        let fresh_hub = world.new_hub(fresh_auth.clone(), send_policy());
        let restored = fresh_hub.restore_authorization().await;
        assert!(restored.authorized, "启动恢复必须从持久存储取回授权");
        fresh_hub.start_auto_connect();
        wait_paired(&fresh_hub).await;

        let oauth_exchanges_after_second_hub = world.fake.oauth_exchanges();
        let terminal_token_requests_after_second_hub = world.fake.terminal_token_requests();
        assert_eq!(
            oauth_exchanges_after_second_hub, 1,
            "恢复路径绝不二次 OAuth"
        );
        assert_eq!(
            terminal_token_requests_after_second_hub, 2,
            "第二次连接必须重新现取 terminal token"
        );
        assert_eq!(
            fresh_hub.connection().state,
            ConnectionState::Paired,
            "新进程必须用恢复的身份重新配对"
        );
        assert!(
            fresh_hub.connection().generation >= first_hub_generation,
            "新 Hub 是独立生命周期"
        );
        assert_eq!(world.vault.save_count(), 1, "全程只允许一次授权落库");
        assert_eq!(world.vault.load_count(), 1, "恢复必须恰好读一次持久记录");
        assert_eq!(
            world.ws.accepted(),
            2,
            "每个进程各建立一条 relay 连接（旧连接已断开）"
        );
    }

    /// 恢复授权后能力矩阵仍按生产默认全关（宿主证据门），send 依旧 409 且零上游帧。
    #[tokio::test]
    async fn default_closed_capabilities_remain_closed_after_restore() {
        let world = PersistentWorld::start().await;
        let (auth, hub) = world
            .oauth_and_auto_connect(CapabilityView::default())
            .await;
        hub.disconnect().await;
        drop(hub);
        drop(auth);

        let (status, _fresh_auth, second_hub) = world.restart(CapabilityView::default()).await;
        assert!(status.authorized, "重启后应已从持久存储恢复授权");
        wait_paired(&second_hub).await;
        let app = router(remote_state(
            &world.conn,
            second_hub.clone(),
            Arc::new(RecordingInjector::default()),
        ));
        let second_session = bind_and_wait_ready(&app, SYNTHETIC_TASK_ID).await;

        for capability in [
            "send",
            "singleSelect",
            "multiSelect",
            "freeText",
            "interrupt",
            "approvalReply",
            "model",
            "thoughtLevel",
        ] {
            assert_eq!(
                second_session["capabilities"][capability]["enabled"], false,
                "恢复后 {capability} 仍必须默认关闭：{second_session}"
            );
            assert_eq!(
                second_session["capabilities"][capability]["reasonCode"],
                codes::NEEDS_EVIDENCE,
                "默认关闭必须给出 needs_evidence 原因：{second_session}"
            );
        }

        // 证据门：即使已恢复并配对，默认关闭的 send 仍不得下发上游业务命令。
        let generation = second_hub.connection().generation;
        // 只比较**业务请求**白名单（`upstream_business_requests`，见其模块级文档）。
        let upstream_before_send = upstream_business_requests(&world.ws.log());
        let (http_status, body) = post_action(
            &app,
            &send_body(
                "act-restored-closed-1",
                generation,
                "恢复后默认关闭的合成正文",
            ),
        )
        .await;
        assert_eq!(
            http_status,
            StatusCode::CONFLICT,
            "恢复不得放宽能力闸：{body}"
        );
        assert_eq!(body["error"], codes::NEEDS_EVIDENCE);
        assert_eq!(world.ws.commands().len(), 0, "默认关闭时零业务命令帧");
        assert_eq!(
            upstream_business_requests(&world.ws.log()),
            upstream_before_send,
            "能力闸必须在 bind 前拒绝：零新增业务请求（bind/订阅/hello/initialize/命令）"
        );
    }

    /// 业务请求白名单（bind/订阅/hello/initialize/发送命令）：假体日志同时会收到
    /// 生产对每个 RPC 帧的传输层 `rpc-frame-ack`（入站，不是业务请求，也可能在
    /// ready 之后才到），它不在白名单内；白名单本身就是要断言的项。
    fn upstream_business_requests(log: &[String]) -> Vec<String> {
        log.iter()
            .filter(|method| {
                matches!(
                    method.as_str(),
                    BOOTSTRAP_REQUEST
                        | WORKSPACE_LIST_REQUEST
                        | WORKSPACE_BRIDGE_OPEN
                        | AGENT_HELLO_METHOD
                        | AGENT_CLIENT_HELLO_METHOD
                        | CONTROLLER_SUBSCRIBE_METHOD
                        | AGENT_SUBSCRIBE_CONVERSATION_METHOD
                        | AGENT_SEND_COMMAND_METHOD
                )
            })
            .cloned()
            .collect::<Vec<_>>()
    }

    /// 用户断开（持久 auto=false）或忘记授权后重启：新进程零 WS、零 terminal token。
    #[tokio::test]
    async fn paused_or_forgotten_restart_has_zero_upstream() {
        // ---- 1) 用户主动断开：保留凭据，但重启后不得自动连接 ----
        let paused_world = PersistentWorld::start().await;
        let (paused_auth, paused_hub) = paused_world.oauth_and_auto_connect(send_policy()).await;
        let paused_view = paused_hub
            .disconnect_requested()
            .await
            .expect("用户断开必须持久化 auto=false");
        assert_eq!(paused_view.state, ConnectionState::Offline);
        assert_eq!(
            paused_world.vault.stored().map(|record| record.jwt),
            Some(SYNTHETIC_JWT.to_owned()),
            "断开不清账号 JWT"
        );
        drop(paused_hub);
        drop(paused_auth);
        let paused_ws_before_restart = paused_world.ws.accepted();
        let paused_tokens_before_restart = paused_world.fake.terminal_token_requests();

        let (paused_status, _paused_auth, paused_restarted) =
            paused_world.restart(send_policy()).await;
        assert!(
            paused_status.authorized,
            "重启恢复仍应看到已保存的授权（只是不自动连接）"
        );
        wait_until(|| paused_restarted.auto_attempt_count() >= 1).await;
        // 观察窗：给 worker 足够时间证明它**不会**在 gate 失败后仍发起连接。
        tokio::time::sleep(Duration::from_millis(100)).await;
        let paused_restart_ws = paused_world.ws.accepted() - paused_ws_before_restart;
        let paused_restart_tokens =
            paused_world.fake.terminal_token_requests() - paused_tokens_before_restart;
        assert_eq!(paused_restart_ws, 0, "断开关机后重启不得新开 WS");
        assert_eq!(
            paused_restart_tokens, 0,
            "断开关机后重启不得请求 terminal token"
        );

        // ---- 2) 忘记授权：删除持久记录，重启后既不连接也不恢复 ----
        let forgotten_world = PersistentWorld::start().await;
        let (forgotten_auth, forgotten_hub) =
            forgotten_world.oauth_and_auto_connect(send_policy()).await;
        let forgotten_view = forgotten_hub
            .forget_authorization()
            .await
            .expect("忘记授权必须成功");
        assert_eq!(forgotten_view.state, ConnectionState::NeedsAuth);
        assert!(
            forgotten_world.vault.stored().is_none(),
            "忘记必须删除持久记录"
        );
        drop(forgotten_hub);
        drop(forgotten_auth);
        let forgotten_ws_before_restart = forgotten_world.ws.accepted();
        let forgotten_tokens_before_restart = forgotten_world.fake.terminal_token_requests();

        let (forgotten_status, _forgotten_auth, forgotten_restarted) =
            forgotten_world.restart(send_policy()).await;
        assert!(!forgotten_status.authorized, "忘记后重启不得恢复授权");
        wait_until(|| forgotten_restarted.auto_attempt_count() >= 1).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let forgotten_restart_ws = forgotten_world.ws.accepted() - forgotten_ws_before_restart;
        let forgotten_restart_tokens =
            forgotten_world.fake.terminal_token_requests() - forgotten_tokens_before_restart;
        assert_eq!(forgotten_restart_ws, 0, "忘记后重启不得新开 WS");
        assert_eq!(
            forgotten_restart_tokens, 0,
            "忘记后重启不得请求 terminal token"
        );
        assert_eq!(
            forgotten_world.vault.load_count(),
            1,
            "重启只读一次已删除的持久记录，绝不重新保存"
        );
        assert_eq!(
            forgotten_world.vault.save_count(),
            1,
            "忘记后不得有新的授权落库"
        );
    }

    /// 自动连接只负责配对：整条链零业务命令帧、零账本行、零终端注入。
    #[tokio::test]
    async fn auto_connection_only_never_dispatches_messages() {
        let world = PersistentWorld::start().await;
        let (_auth, hub) = world.oauth_and_auto_connect(send_policy()).await;
        assert_eq!(hub.connection().state, ConnectionState::Paired);
        // 观察窗：配对完成后再等一会儿，证明没有任何“自动补发/自动动作”。
        tokio::time::sleep(Duration::from_millis(150)).await;

        let all_business_command_frames = world.ws.commands().len();
        assert_eq!(
            all_business_command_frames, 0,
            "自动连接不得派发任何业务命令帧"
        );
        assert!(
            !world
                .ws
                .log()
                .iter()
                .any(|method| method == AGENT_SEND_COMMAND_METHOD),
            "自动连接日志不得出现 sendConversationCommandV4：{:?}",
            world.ws.log()
        );
        assert_eq!(ledger_rows(&world.store()), 0, "自动连接不得起动作账");
        assert_eq!(audit_rows(&world.store(), "zcode_send", "started"), 0);
        assert_eq!(audit_rows(&world.store(), "zcode_send", "accepted"), 0);
    }
}
