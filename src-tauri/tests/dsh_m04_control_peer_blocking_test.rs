//! M04：控制面取当前授权句柄时，同步探测不得阻塞单线程运行时。
//! 守护线程仅用超时防止失败版本死锁；探测进入和释放均通过通道同步。

#[path = "support/dsh_blocking.rs"]
mod blocking;
use auth::{ClientFrame, CredentialFrame, RustSigningKey, ServerFrame};
use blocking::{arm, checked, hello_with_keys, Bench, Source};
use multi_agents_manager_lib::services::dsh_control::{auth, identity};

async fn peer_for_control_keeps_heartbeat_running(kind: Source) {
    let b = Bench::new("127.0.0.1:5214".into());
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
    let frame = ClientFrame::Credential(CredentialFrame {
        binding_id: id,
        cookie: "fixture-cookie".into(),
        browser_cookie: None,
    });
    client.send(&auth::client_frame_line(&frame)).await.unwrap();
    assert!(matches!(
        auth::parse_server_frame(&client.recv().await.unwrap()).unwrap(),
        ServerFrame::Granted { .. }
    ));

    let heartbeat = arm(&b.processes, kind);
    let peer = svc.peer_for_control().await;
    checked(heartbeat).await;
    assert!(peer.is_some(), "已交付的当代凭据应可供控制面使用");
}

#[tokio::test(flavor = "current_thread")]
async fn control_peer_keeps_heartbeat_running_during_process_probe() {
    peer_for_control_keeps_heartbeat_running(Source::Process).await;
}

#[tokio::test(flavor = "current_thread")]
async fn control_peer_keeps_heartbeat_running_during_listener_probe() {
    peer_for_control_keeps_heartbeat_running(Source::Listener).await;
}
