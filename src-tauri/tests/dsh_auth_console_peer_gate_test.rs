//! M02 定向回归：迟到凭据使用同一个 PeerGate，撤权后控制台不能取用。
//!
//! 使用带闸门安装接口及注入的统一挂载，并通过生产 `console_mount::router` 验证出站。
//! 本文件不覆盖完整 IPC，也不声称覆盖 `accept_credential` 到安装之间的竞态；
//! 该窗口仍需生产路径同步钩子及独立回归。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use app::remote::console_capability::ConsoleCapability;
use app::remote::console_mount;
use app::services::dsh_control::transport::PeerGate;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use multi_agents_manager_lib as app;
use tower::ServiceExt;

const EPOCH: u64 = 17;
const BROWSER: &str = "dsh-browser=fixture";

async fn recording_upstream() -> (String, Arc<AtomicUsize>) {
    let requests = Arc::new(AtomicUsize::new(0));
    let recorded = Arc::clone(&requests);
    let app = Router::new().fallback(move || {
        let recorded = Arc::clone(&recorded);
        async move {
            recorded.fetch_add(1, Ordering::SeqCst);
            (StatusCode::OK, "console fixture")
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let authority = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (authority, requests)
}

fn mounted_console(
    mount: &Arc<console_mount::ConsoleMount>,
    instance: &str,
    authority: &str,
) -> Router {
    mount
        .register(ConsoleCapability {
            token: "fixture-token".into(),
            device_id: "fixture-device".into(),
            instance_id: instance.into(),
            authority: authority.into(),
            epoch: EPOCH,
            bootstrap: false,
            expires_at_ms: mount.now() + 60_000,
        })
        .unwrap();
    console_mount::router(Arc::clone(mount))
}

async fn request_console(app: &Router) -> StatusCode {
    app.clone()
        .oneshot(
            Request::builder()
                .uri("/console/fixture-token/")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

/// 票据登记和兑换均使用页面路由对应的同一挂载。
fn issue_and_redeem(
    mount: &console_mount::ConsoleMount,
    instance: &str,
    authority: &str,
    ticket: &str,
) -> String {
    let sequence = mount.revocation_sequence();
    mount
        .register_if_current(
            ConsoleCapability {
                token: ticket.into(),
                device_id: "fixture-device".into(),
                instance_id: instance.into(),
                authority: authority.into(),
                epoch: EPOCH,
                bootstrap: true,
                expires_at_ms: mount.now() + 60_000,
            },
            sequence,
        )
        .expect("引导能力应成功签发");
    mount
        .redeem_bootstrap_for_device(ticket, "fixture-device")
        .expect("对应设备应能兑换引导能力")
}

/// 同一实例、代际和地址换绑定时，旧页面与 WS 不能继承新绑定的 Cookie。
/// 此用例直接装配统一准入核心，不覆盖完整 IPC 或生产单例装配。
#[tokio::test]
async fn replacing_binding_on_same_target_invalidates_old_capability_and_closes_websocket() {
    use axum::extract::ws::WebSocketUpgrade;
    use axum::http::{header, HeaderMap};
    use axum::routing::get;
    use futures::StreamExt;

    let http_cookies: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&http_cookies);
    let (opened_tx, opened_rx) = tokio::sync::oneshot::channel::<String>();
    let (closed_tx, closed_rx) = tokio::sync::oneshot::channel::<()>();
    let ws_signals = Arc::new(Mutex::new(Some((opened_tx, closed_tx))));
    let ws_state = Arc::clone(&ws_signals);
    let upstream = Router::new()
        .route(
            "/",
            get(move |headers: HeaderMap| {
                let captured = Arc::clone(&captured);
                async move {
                    let cookie = headers
                        .get(header::COOKIE)
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("");
                    captured.lock().unwrap().push(cookie.to_owned());
                    (StatusCode::OK, "console fixture")
                }
            }),
        )
        .route(
            "/api/remote.mux",
            get(move |headers: HeaderMap, upgrade: WebSocketUpgrade| {
                let cookie = headers
                    .get(header::COOKIE)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("")
                    .to_owned();
                let signals = ws_state.lock().unwrap().take().expect("只建立一条旧 WS");
                async move {
                    upgrade.on_upgrade(move |mut socket| async move {
                        let _ = signals.0.send(cookie);
                        while let Some(Ok(message)) = socket.recv().await {
                            if matches!(message, axum::extract::ws::Message::Close(_)) {
                                break;
                            }
                        }
                        let _ = signals.1.send(());
                    })
                }
            }),
        );
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let authority = upstream_listener.local_addr().unwrap().to_string();
    let upstream_task =
        tokio::spawn(async move { axum::serve(upstream_listener, upstream).await.unwrap() });

    let instance = format!("same-target-{:032x}", rand::random::<u128>());
    let mount = Arc::new(console_mount::ConsoleMount::new().with_device_check(|_| true));
    let gate_a = PeerGate::new();
    let gate_b = PeerGate::new();
    mount
        .install_browser_credential(
            &instance,
            EPOCH,
            &authority,
            "browser=A".into(),
            Arc::clone(&gate_a),
        )
        .expect("绑定 A 的浏览器凭据应安装成功");
    let old_token = issue_and_redeem(&mount, &instance, &authority, "ticket-a");
    let app = console_mount::router(Arc::clone(&mount));
    let gateway_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway = gateway_listener.local_addr().unwrap();
    let gateway_task = tokio::spawn({
        let app = app.clone();
        async move { axum::serve(gateway_listener, app).await.unwrap() }
    });
    let get_page = |token: &str| {
        Request::builder()
            .uri(format!("/console/{token}/"))
            .body(Body::empty())
            .unwrap()
    };
    let before = app.clone().oneshot(get_page(&old_token)).await.unwrap();
    assert_eq!(before.status(), StatusCode::OK, "替换前旧能力必须可用");
    assert_eq!(*http_cookies.lock().unwrap(), vec!["browser=A"]);
    let (mut socket, _) = tokio_tungstenite::connect_async(format!(
        "ws://{gateway}/console/{old_token}/api/remote.mux"
    ))
    .await
    .expect("旧 WS 必须真实升级");
    let ws_cookie = tokio::time::timeout(Duration::from_secs(3), opened_rx)
        .await
        .expect("等待上游 WS 握手超时")
        .expect("上游必须确认 WS 已建立");
    assert_eq!(ws_cookie, "browser=A", "旧 WS 必须携带 A 的凭据");

    mount
        .install_browser_credential(
            &instance,
            EPOCH,
            &authority,
            "browser=B".into(),
            Arc::clone(&gate_b),
        )
        .expect("同一目标的绑定 B 凭据应安装成功");
    let old_response = app.clone().oneshot(get_page(&old_token)).await.unwrap();
    let old_request_count = http_cookies.lock().unwrap().len();
    let new_token = issue_and_redeem(&mount, &instance, &authority, "ticket-b");
    let new_response = app.clone().oneshot(get_page(&new_token)).await.unwrap();
    let client_closed = tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(message) = socket.next().await {
            match message {
                Ok(message) if message.is_close() => return,
                Err(_) => return,
                _ => {}
            }
        }
    })
    .await;
    let upstream_closed = tokio::time::timeout(Duration::from_secs(3), closed_rx).await;
    let cookies = http_cookies.lock().unwrap().clone();
    gateway_task.abort();
    upstream_task.abort();

    assert_ne!(
        old_response.status(),
        StatusCode::OK,
        "旧能力不能获得 B 的凭据"
    );
    assert_eq!(old_request_count, 1, "替换后旧能力不得向上游出站");
    assert_eq!(new_response.status(), StatusCode::OK, "B 的新能力必须可用");
    assert_eq!(
        cookies,
        vec!["browser=A", "browser=B"],
        "只有 A 旧请求与 B 新请求可到达上游"
    );
    assert!(client_closed.is_ok(), "旧 WS 的浏览器端必须关闭");
    assert!(
        matches!(upstream_closed, Ok(Ok(()))),
        "旧 WS 的上游连接必须关闭"
    );
}

#[tokio::test]
async fn already_revoked_gate_rejects_late_install_and_cannot_reach_upstream() {
    let (authority, requests) = recording_upstream().await;
    let instance = format!("revoked-before-install-{:032x}", rand::random::<u128>());
    let mount = Arc::new(console_mount::ConsoleMount::new().with_device_check(|_| true));
    let gate = PeerGate::new();
    gate.revoke();
    assert!(gate.is_revoked(), "必须先完成撤权，再尝试迟到交付");

    let installed = mount.install_browser_credential(
        &instance,
        EPOCH,
        &authority,
        BROWSER.into(),
        Arc::clone(&gate),
    );
    assert!(installed.is_err(), "已撤闸门不能安装迟到凭据");
    let app = mounted_console(&mount, &instance, &authority);
    assert_eq!(request_console(&app).await, StatusCode::FORBIDDEN);
    assert_eq!(requests.load(Ordering::SeqCst), 0, "旧上游必须零请求");
}

#[tokio::test]
async fn revoking_installed_gate_blocks_credential_access_and_later_console_requests() {
    let (authority, requests) = recording_upstream().await;
    let instance = format!("revoked-after-install-{:032x}", rand::random::<u128>());
    let mount = Arc::new(console_mount::ConsoleMount::new().with_device_check(|_| true));
    let gate = PeerGate::new();
    mount
        .install_browser_credential(
            &instance,
            EPOCH,
            &authority,
            BROWSER.into(),
            Arc::clone(&gate),
        )
        .expect("未撤权时应可安装凭据");
    let app = mounted_console(&mount, &instance, &authority);
    assert_eq!(
        request_console(&app).await,
        StatusCode::OK,
        "正向对照须到达上游"
    );
    assert_eq!(requests.load(Ordering::SeqCst), 1);

    gate.revoke();
    assert!(gate.is_revoked());
    assert_eq!(request_console(&app).await, StatusCode::FORBIDDEN);
    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "撤权后不得再次请求旧上游"
    );
}
