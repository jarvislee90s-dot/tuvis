//! M04：PeerChannel 真实入口在 resolver 和探测阻塞时仍保持单线程心跳。
//! 守护线程仅用超时防止失败版本死锁；探测进入和释放均通过通道同步。

#![cfg(feature = "dsh-test-probes")]

#[path = "support/dsh_blocking.rs"]
mod blocking;
use auth::{ClientFrame, CredentialFrame, ServerFrame};
use auth::{DshAuthService, RustSigningKey};
use blocking::WATCHDOG;
use blocking::{arm, checked, hello_with_keys, Bench, Source};
use multi_agents_manager_lib::services::dsh_control::actions::PeerChannel;
use multi_agents_manager_lib::services::dsh_control::local_ipc::mem::MemClient;
use multi_agents_manager_lib::services::dsh_control::{auth, identity};
use std::sync::Arc;

// 先完成真实握手、批准和凭据交付，避免无 Bound 槽时提前返回而漏测采样。
async fn delivered_service(b: &Bench) -> (Arc<DshAuthService>, MemClient) {
    let host = identity::discover(&b.env).unwrap();
    let (svc, factory) = b.service();
    svc.serve().await.unwrap();
    let client = factory.connect().await;
    let keys = RustSigningKey::generate();
    let id = hello_with_keys(&svc, &client, host.epoch, &keys).await;
    svc.approve(&id).await.unwrap();
    assert!(matches!(
        auth::parse_server_frame(&client.recv().await.unwrap()).unwrap(),
        ServerFrame::Granted { .. }
    ));
    client
        .send(&auth::client_frame_line(&ClientFrame::Credential(
            CredentialFrame {
                binding_id: id,
                cookie: "fixture-cookie".into(),
                browser_cookie: None,
            },
        )))
        .await
        .unwrap();
    assert!(matches!(
        auth::parse_server_frame(&client.recv().await.unwrap()).unwrap(),
        ServerFrame::Granted { .. }
    ));
    (svc, client)
}

#[derive(Clone, Copy)]
enum Entry {
    Ready,
    Request,
}

async fn channel_keeps_heartbeat_running(entry: Entry, kind: Source) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let b = Bench::new(listener.local_addr().unwrap().to_string());
    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
    let (svc, _client) = delivered_service(&b).await;
    let processes = Arc::clone(&b.processes);
    // 待主 agent 提供的接缝：resolver 返回真实授权服务，而非静态 peer。
    let channel = PeerChannel::from_auth_resolver(Arc::new(move || {
        processes.block_if_armed(Source::Resolver);
        Ok(Arc::clone(&svc))
    }));
    let heartbeat = arm(&b.processes, kind);
    match entry {
        Entry::Ready => {
            let ready = channel.ready().await;
            checked(heartbeat).await;
            assert!(ready, "已交付的当代凭据应使通道就绪");
        }
        Entry::Request => {
            let request = tokio::spawn(async move {
                channel
                    .request_classified(
                        "fixture-rpc",
                        "session/prompt",
                        &serde_json::json!({}),
                        None,
                    )
                    .await
            });
            checked(heartbeat).await;
            // 专属监听端只接受连接，不完成 WebSocket 握手；到达此处证明真实
            // current_peer 已返回句柄并进入 transport，随后取消请求以避免外部依赖。
            let (socket, _) = tokio::time::timeout(WATCHDOG, listener.accept())
                .await
                .expect("取到句柄后必须进入建连")
                .expect("接受测试连接");
            request.abort();
            let _ = request.await;
            drop(socket);
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn ready_keeps_heartbeat_running_during_resolver() {
    channel_keeps_heartbeat_running(Entry::Ready, Source::Resolver).await;
}
#[tokio::test(flavor = "current_thread")]
async fn ready_keeps_heartbeat_running_during_process_probe() {
    channel_keeps_heartbeat_running(Entry::Ready, Source::Process).await;
}
#[tokio::test(flavor = "current_thread")]
async fn ready_keeps_heartbeat_running_during_listener_probe() {
    channel_keeps_heartbeat_running(Entry::Ready, Source::Listener).await;
}
#[tokio::test(flavor = "current_thread")]
async fn request_keeps_heartbeat_running_during_resolver() {
    channel_keeps_heartbeat_running(Entry::Request, Source::Resolver).await;
}
#[tokio::test(flavor = "current_thread")]
async fn request_keeps_heartbeat_running_during_process_probe() {
    channel_keeps_heartbeat_running(Entry::Request, Source::Process).await;
}
#[tokio::test(flavor = "current_thread")]
async fn request_keeps_heartbeat_running_during_listener_probe() {
    channel_keeps_heartbeat_running(Entry::Request, Source::Listener).await;
}
