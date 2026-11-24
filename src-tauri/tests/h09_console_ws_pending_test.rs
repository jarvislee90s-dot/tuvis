#![cfg(feature = "dsh-test-probes")]

//! 真实控制台挂载与上游 WS：确认生产转发的发送处于 Pending 后撤权。

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::extract::ws::{Message as AxumMessage, WebSocketUpgrade};
use axum::routing::get;
use axum::Router;
use futures::{Sink, SinkExt, StreamExt};
use multi_agents_manager_lib as app;
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::{Error, Message};

use app::remote::console_capability::ConsoleCapability;
use app::remote::console_mount::{self, ConsoleMount, TestSinkWrapper, TestUpstreamSink};
use app::services::dsh_control::transport::PeerGate;

const LIMIT: Duration = Duration::from_secs(5);

/// 首个合法 open 正常透传，第二次发送在 poll_ready 中持续 Pending。
struct PendingSecondSend {
    inner: TestUpstreamSink,
    sent_open: bool,
    pending: Option<oneshot::Sender<()>>,
    dropped: Option<oneshot::Sender<()>>,
}

impl Sink<Message> for PendingSecondSend {
    type Error = Error;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        if self.sent_open {
            if let Some(pending) = self.pending.take() {
                let _ = pending.send(());
            }
            return Poll::Pending;
        }
        self.inner.as_mut().poll_ready(cx)
    }

    fn start_send(mut self: Pin<&mut Self>, message: Message) -> Result<(), Error> {
        self.inner.as_mut().start_send(message)?;
        self.sent_open = true;
        Ok(())
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        self.inner.as_mut().poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        self.inner.as_mut().poll_close(cx)
    }
}

impl Drop for PendingSecondSend {
    fn drop(&mut self) {
        if let Some(dropped) = self.dropped.take() {
            let _ = dropped.send(());
        }
    }
}

#[tokio::test]
async fn revocation_drops_the_production_bridge_while_upstream_send_is_pending() {
    let (open_tx, open_rx) = oneshot::channel::<String>();
    let (closed_tx, closed_rx) = oneshot::channel::<()>();
    let events = Arc::new(Mutex::new(Some((open_tx, closed_tx))));
    let upstream = Router::new().route(
        "/api/remote.mux",
        get(move |upgrade: WebSocketUpgrade| {
            let events = Arc::clone(&events);
            async move {
                upgrade.on_upgrade(move |mut socket| async move {
                    let (open, closed) = events.lock().unwrap().take().unwrap();
                    let first = socket.recv().await;
                    if let Some(Ok(AxumMessage::Text(text))) = first {
                        let _ = open.send(text.to_string());
                    }
                    while let Some(Ok(message)) = socket.recv().await {
                        if matches!(message, AxumMessage::Close(_)) {
                            break;
                        }
                    }
                    let _ = closed.send(());
                })
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let authority = listener.local_addr().unwrap().to_string();
    let upstream_task = tokio::spawn(async move { axum::serve(listener, upstream).await });

    let (pending_tx, pending_rx) = oneshot::channel();
    let (dropped_tx, dropped_rx) = oneshot::channel();
    let signals = Arc::new(Mutex::new(Some((pending_tx, dropped_tx))));
    let wrapper: TestSinkWrapper = Arc::new(move |inner| {
        let (pending, dropped) = signals.lock().unwrap().take().unwrap();
        Box::pin(PendingSecondSend {
            inner,
            sent_open: false,
            pending: Some(pending),
            dropped: Some(dropped),
        })
    });
    let mount = Arc::new(
        ConsoleMount::new()
            .with_device_check(|device| device == "dev-a")
            .with_test_sink_wrapper(wrapper),
    );
    mount
        .install_browser_credential(
            "inst-a",
            1,
            &authority,
            "browser=synthetic".into(),
            PeerGate::new(),
        )
        .unwrap();
    let token = "pending-send-test";
    mount
        .register(ConsoleCapability {
            token: token.into(),
            device_id: "dev-a".into(),
            instance_id: "inst-a".into(),
            authority,
            epoch: 1,
            bootstrap: false,
            expires_at_ms: mount.now() + 60_000,
        })
        .unwrap();
    let gateway_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway_address = gateway_listener.local_addr().unwrap();
    let gateway = console_mount::router(Arc::clone(&mount));
    let gateway_task = tokio::spawn(async move { axum::serve(gateway_listener, gateway).await });
    let (mut browser, _) = tokio_tungstenite::connect_async(format!(
        "ws://{gateway_address}/console/{token}/api/remote.mux"
    ))
    .await
    .expect("浏览器必须通过真实挂载完成 WS 握手");

    browser
        .send(Message::text(
            r#"{"type":"open","streamId":"s1","endpoint":"session/follow","payload":{"args":{}}}"#,
        ))
        .await
        .unwrap();
    let open = tokio::time::timeout(LIMIT, open_rx)
        .await
        .expect("上游应收到合法 open")
        .expect("上游必须确认 open");
    let parsed: serde_json::Value = serde_json::from_str(&open).unwrap();
    assert_eq!(parsed["endpoint"], "session/follow");
    assert_eq!(parsed["streamId"], "s1");
    assert_eq!(mount.live_count_for_test(), 1, "桥接必须已登记");

    browser
        .send(Message::text(
            r#"{"type":"item","streamId":"s1","value":{"data":"pending"}}"#,
        ))
        .await
        .unwrap();
    tokio::time::timeout(LIMIT, pending_rx)
        .await
        .expect("生产转发必须实际轮询到 Pending")
        .expect("包装器必须报告 Pending");
    mount.revoke_device("dev-a");
    tokio::time::timeout(LIMIT, dropped_rx)
        .await
        .expect("撤权必须丢弃阻塞发送的 sink")
        .expect("包装器必须报告 Drop");
    tokio::time::timeout(LIMIT, closed_rx)
        .await
        .expect("撤权必须关闭上游 WS")
        .expect("上游必须观察到关闭");
    tokio::time::timeout(LIMIT, async {
        loop {
            match browser.next().await {
                Some(Ok(message)) if message.is_close() => break,
                None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            }
        }
    })
    .await
    .expect("撤权必须关闭浏览器 WS");
    assert_eq!(mount.live_count_for_test(), 0, "桥接结束后不得保留活连接");
    gateway_task.abort();
    upstream_task.abort();
}
