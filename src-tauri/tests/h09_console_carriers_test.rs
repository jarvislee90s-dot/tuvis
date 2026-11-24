//! 官方控制台方法载体回归：真实挂载、真实 WS 及记录型本地上游。
//!
//! 依据本机 DeepSeek Harness.app 的 dsh-api-session-controller/lib/typert.host.js：
//! session/control 无业务参数、mode stream；session/follow 是 stream，
//! session/projections 是一元方法。实际 follow 参数亦见 macos-report.md §4。

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{Message as AxumMessage, WebSocketUpgrade};
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::{body::Body, Router};
use futures::{SinkExt, StreamExt};
use multi_agents_manager_lib as app;
use serde_json::{json, Value};
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::Message;
use tower::ServiceExt;

use app::remote::console_capability::ConsoleCapability;
use app::remote::console_mount::{self, ConsoleMount};
use app::services::dsh_control::transport::PeerGate;

const LIMIT: Duration = Duration::from_secs(5);
const TOKEN: &str = "carrier-test-token";

fn mount(authority: &str) -> Arc<ConsoleMount> {
    let mount = Arc::new(ConsoleMount::new().with_device_check(|device| device == "dev-a"));
    mount
        .install_browser_credential(
            "inst-a",
            1,
            authority,
            "browser=synthetic".into(),
            PeerGate::new(),
        )
        .unwrap();
    mount
        .register(ConsoleCapability {
            token: TOKEN.into(),
            device_id: "dev-a".into(),
            instance_id: "inst-a".into(),
            authority: authority.into(),
            epoch: 1,
            bootstrap: false,
            expires_at_ms: mount.now() + 60_000,
        })
        .unwrap();
    mount
}

async fn listen(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (address, task)
}

/// 上游连接建立时发就绪帧，关闭后才交出完整业务帧清单。
async fn ws_upstream() -> (
    String,
    oneshot::Receiver<Vec<Value>>,
    tokio::task::JoinHandle<()>,
) {
    let (done_tx, done_rx) = oneshot::channel();
    let done = Arc::new(Mutex::new(Some(done_tx)));
    let router = Router::new().route(
        "/api/remote.mux",
        get(move |upgrade: WebSocketUpgrade| {
            let done = done.lock().unwrap().take().unwrap();
            async move {
                upgrade.on_upgrade(move |mut socket| async move {
                    if socket
                        .send(AxumMessage::Text(
                            json!({"type":"fixture-ready"}).to_string().into(),
                        ))
                        .await
                        .is_err()
                    {
                        let _ = done.send(Vec::new());
                        return;
                    }
                    let mut frames = Vec::new();
                    while let Some(Ok(message)) = socket.recv().await {
                        if let AxumMessage::Text(text) = message {
                            let frame: Value = serde_json::from_str(&text).unwrap();
                            let id = frame["streamId"].as_str().unwrap().to_owned();
                            let endpoint = frame["endpoint"].as_str().unwrap().to_owned();
                            frames.push(frame);
                            // 控制流的 baseline 类型取自官方 controlState.control 实现；
                            // 其 value 为合成数据，仅验证 Mux 回程和连接复用。
                            let value = if endpoint == "session/control" {
                                json!({"type":"baseline", "value":{"projections":{}}})
                            } else {
                                json!({"type":"fixture-event", "endpoint":endpoint})
                            };
                            let reply = json!({"type":"item", "streamId":id, "value":value});
                            if socket
                                .send(AxumMessage::Text(reply.to_string().into()))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                    let _ = done.send(frames);
                })
            }
        }),
    );
    let (address, task) = listen(router).await;
    (address, done_rx, task)
}

async fn ready(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) {
    let message = tokio::time::timeout(LIMIT, socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let value: Value = serde_json::from_str(message.to_text().unwrap()).unwrap();
    assert_eq!(value["type"], "fixture-ready", "上游握手必须完成");
}

#[tokio::test]
async fn control_events_and_follow_share_one_mux_and_receive_upstream_items() {
    let (authority, finished, upstream_task) = ws_upstream().await;
    let mount = mount(&authority);
    let (gateway, gateway_task) = listen(console_mount::router(mount)).await;
    let (mut socket, _) =
        tokio_tungstenite::connect_async(format!("ws://{gateway}/console/{TOKEN}/api/remote.mux"))
            .await
            .expect("真实挂载必须接受 WS 升级");
    ready(&mut socket).await;
    let opens = [
        json!({"type":"open", "streamId":"control", "endpoint":"session/control", "payload":{"args":{}}}),
        json!({"type":"open", "streamId":"events", "endpoint":"$events", "payload":{"args":{}}}),
        json!({"type":"open", "streamId":"follow", "endpoint":"session/follow", "payload":{"args":{"request":{"address":{"kind":"session", "sessionId":"session-fixture"}}}}}),
    ];
    for open in &opens {
        socket.send(Message::text(open.to_string())).await.unwrap();
        let reply = tokio::time::timeout(LIMIT, socket.next())
            .await
            .expect("合法流应收到上游帧")
            .expect("桥接不得关闭")
            .expect("帧应可读");
        let reply: Value = serde_json::from_str(reply.to_text().unwrap()).unwrap();
        assert_eq!(reply["type"], "item");
        assert_eq!(reply["streamId"], open["streamId"]);
        if open["endpoint"] == "session/control" {
            assert_eq!(reply["value"]["type"], "baseline");
            assert_eq!(reply["value"]["value"]["projections"], json!({}));
        }
    }
    socket.close(None).await.unwrap();
    let seen = tokio::time::timeout(LIMIT, finished)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(seen, opens, "三个官方 open 必须按原始语义到达同一上游连接");
    gateway_task.abort();
    upstream_task.abort();
}

#[tokio::test]
async fn both_http_calls_are_dispatched_by_upstream() {
    let requests = Arc::new(Mutex::new(Vec::<String>::new()));
    let record = Arc::clone(&requests);
    let upstream = Router::new().fallback(move |request: Request<Body>| {
        let record = Arc::clone(&record);
        async move {
            record.lock().unwrap().push(request.uri().path().to_owned());
            StatusCode::OK
        }
    });
    let (authority, upstream_task) = listen(upstream).await;
    let mount = mount(&authority);
    let app = console_mount::router(mount);
    let request = |path: &str| {
        Request::builder()
            .method("POST")
            .uri(format!("/console/{TOKEN}{path}"))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"request":{"sessionId":"session-fixture"}}"#))
            .unwrap()
    };
    let response = app
        .clone()
        .oneshot(request("/api/session/projections"))
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "一元 projections 正对照应到达上游"
    );
    assert_eq!(
        requests.lock().unwrap().as_slice(),
        ["/api/session/projections"]
    );
    requests.lock().unwrap().clear();
    let response = app.oneshot(request("/api/session/follow")).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "该方法是否支持 HTTP 由上游决定"
    );
    assert_eq!(requests.lock().unwrap().as_slice(), ["/api/session/follow"]);
    upstream_task.abort();
}
