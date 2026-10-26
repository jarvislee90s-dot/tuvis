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
    use std::sync::Arc;
    use std::time::Duration;

    use axum::routing::post;
    use axum::{Json, Router};
    use futures::{SinkExt, StreamExt};
    use tokio::sync::mpsc;
    use tokio_tungstenite::tungstenite::Message;

    use super::super::auth::{
        AuthService, OAuthProvider, OpenAuthorize, OAUTH_TOKEN_PATH, RELAY_TOKEN_PATH,
    };
    use super::super::relay::{DeviceRoute, RelayClient, RelayEvent};
    use super::super::types::{ConnectionState, ZResult};

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
    /// `echoes()` 为已回显的数据面帧数（断言「一帧一次、不重复」）。
    struct FakeWsHandshake {
        origin: String,
        accepted: Arc<AtomicUsize>,
        echoes: Arc<AtomicUsize>,
        task: tokio::task::JoinHandle<()>,
    }

    impl FakeWsHandshake {
        fn accepted(&self) -> usize {
            self.accepted.load(Ordering::SeqCst)
        }

        fn echoes(&self) -> usize {
            self.echoes.load(Ordering::SeqCst)
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
    /// `pair_status_ack("matched")`；**握手后的其余 JSON 文本帧原样回显一次**
    /// （不改写、不重复）；socket 保持到对端关闭。
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
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(serve_ws_handshake(stream, echo_counter.clone()));
            }
        });
        FakeWsHandshake {
            origin,
            accepted,
            echoes,
            task,
        }
    }

    /// 单条回环 WS 连接的握手/心跳应答（配对即刻完成）与数据面回显。
    async fn serve_ws_handshake(stream: tokio::net::TcpStream, echoes: Arc<AtomicUsize>) {
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
}
