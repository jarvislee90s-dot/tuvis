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
    use futures::{SinkExt, StreamExt};
    use tokio::sync::mpsc;
    use tokio_tungstenite::tungstenite::Message;

    use super::super::auth::{
        AuthService, OAuthProvider, OpenAuthorize, OAUTH_TOKEN_PATH, RELAY_TOKEN_PATH,
    };
    use super::super::bridge::{
        BridgeManager, BridgeTiming, BOOTSTRAP_REQUEST, WORKSPACE_BRIDGE_OPEN,
        WORKSPACE_LIST_REQUEST,
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
    /// `log()` 为收到的 app 业务请求的 `zcode_type` 序列（断言生产端请求顺序）。
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

        /// 已收到的 app 业务请求 `zcode_type`（按到达顺序）。
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
    /// `workspace-bridge-open` 只记账、**不回** `workspace-bridge-ready`（bind 停在开桥）。
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

    /// 合成 listing 回执：回显 `requestId`，单 workspace（`/tmp/travel`，无 identity）。
    fn app_listing_reply(payload: &serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "requestId": payload.get("requestId").cloned().unwrap_or_default(),
            "result": {
                "activeWorkspaceKey": "ws-synthetic",
                "workspaces": [{
                    "workspacePath": "/tmp/travel",
                    "workspaceIdentity": null,
                }],
            },
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
            // 握手完成后的 app 业务请求：先记账 `zcode_type`，再按类型应答。
            // bootstrap/workspace-list 回合成 listing；workspace-bridge-open 只记账
            // （无 ready → 生产 bind 停在开桥，不会虚构后续身份）。
            if device_sid.is_some() {
                if let Some(payload) = app_payload(&value) {
                    if let Some(zcode_type) =
                        payload.get("zcode_type").and_then(|kind| kind.as_str())
                    {
                        log.lock()
                            .expect("app 请求日志锁")
                            .push(zcode_type.to_owned());
                        let reply = match zcode_type {
                            BOOTSTRAP_REQUEST | WORKSPACE_LIST_REQUEST => {
                                Some(app_listing_reply(payload))
                            }
                            _ => None,
                        };
                        if let Some(reply) = reply {
                            let frame = serde_json::json!({"type": "data", "payload": reply});
                            let _ = socket.send(Message::text(frame.to_string())).await;
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

    /// 真实回环 relay 上的桥 bootstrap 全链：真实 [`AuthService`]（回环 HTTP 取 token）+
    /// 真实 [`RelayClient`]（回环 WS 配对）+ 真实 [`BridgeManager`]（默认生产时序）。
    ///
    /// 回环假体按 `zcode_type` 应答：`bootstrap-request` / `workspace-list-request` 回
    /// 合成 listing（`/tmp/travel`），`workspace-bridge-open` 只记账、不回 ready。
    /// 因此 `bind` 必然停在开桥（2s 超时收口）；而出站日志里
    /// `bootstrap-request → workspace-list-request → workspace-bridge-open` 的**顺序**
    /// 证明：生产桥真的解析了假体回的合成 workspace，并接着在**同一条真实 relay 连接**上
    /// 发下一条请求（不是假体自导自演）。
    #[tokio::test]
    async fn bridge_bootstrap_runs_over_real_loopback_relay() {
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

        // 事件通道兼作生产事件泵的输入：状态事件占少量槽位，Data 逐条转给桥。
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

        let bridge = Arc::new(BridgeManager::with_timing(
            relay.clone(),
            BridgeTiming::default(),
        ));
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

        // 假体不回 `workspace-bridge-ready`：bind 停在开桥，由 2s 超时收口。
        let bound = tokio::time::timeout(
            Duration::from_secs(2),
            bridge.bind(&TargetKey {
                session_id: "session_synthetic".into(),
            }),
        )
        .await;
        assert!(
            bound.is_err(),
            "无 ready 应答时 bind 必须停在 bridge-open（不得自造回执继续）"
        );
        pump.abort();

        assert_eq!(
            ws.log(),
            vec![
                BOOTSTRAP_REQUEST,
                WORKSPACE_LIST_REQUEST,
                WORKSPACE_BRIDGE_OPEN
            ],
            "生产桥必须按 bootstrap → listing → 开桥 的顺序发出请求"
        );
        assert_eq!(ws.accepted(), 1, "整条 bootstrap 只需一条 relay 连接");
    }
}
