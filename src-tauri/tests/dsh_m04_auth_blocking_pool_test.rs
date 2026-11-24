//! M04：授权服务在同步进程/监听探测受阻时不得阻塞单线程运行时。
//! 守护线程仅用超时防止失败版本死锁；探测进入和释放均通过通道同步。

#[path = "support/dsh_blocking.rs"]
mod blocking;
use auth::{ClientFrame, CredentialFrame, ServerFrame};
use auth::{DshAuthService, RustSigningKey};
use blocking::{arm, checked, hello_with_keys, Bench, Source, INSTALL, PID, START};
use multi_agents_manager_lib::services::dsh_control::local_ipc::mem::MemClient;
use multi_agents_manager_lib::services::dsh_control::model::ClientIdentity;
use multi_agents_manager_lib::services::dsh_control::{auth, identity};
use std::path::PathBuf;

async fn hello(svc: &DshAuthService, client: &MemClient, epoch: u64) -> String {
    hello_with_keys(svc, client, epoch, &RustSigningKey::generate()).await
}

async fn initial_publish(kind: Source) {
    let b = Bench::new("127.0.0.1:5214".into());
    let (svc, _) = b.service();
    let heartbeat = arm(&b.processes, kind);
    svc.serve().await.expect("初次发布成功");
    checked(heartbeat).await;
}
async fn accept_refresh(kind: Source) {
    let b = Bench::new("127.0.0.1:5214".into());
    let (svc, factory) = b.service();
    svc.serve().await.unwrap();
    let heartbeat = arm(&b.processes, kind);
    let client = factory.connect().await;
    checked(heartbeat).await;
    // 服务端完成刷新并开始处理此连接，才会回应非法帧。
    client.send("invalid frame").await.unwrap();
    assert!(matches!(
        auth::parse_server_frame(&client.recv().await.unwrap()).unwrap(),
        ServerFrame::Error { .. }
    ));
}
async fn approval(kind: Source) {
    let b = Bench::new("127.0.0.1:5214".into());
    let epoch = identity::discover(&b.env).unwrap().epoch;
    let (svc, factory) = b.service();
    svc.serve().await.unwrap();
    let client = factory.connect().await;
    let id = hello(&svc, &client, epoch).await;
    let heartbeat = arm(&b.processes, kind);
    svc.approve(&id).await.expect("批准成功");
    checked(heartbeat).await;
    assert!(matches!(
        auth::parse_server_frame(&client.recv().await.unwrap()).unwrap(),
        ServerFrame::Granted { .. }
    ));
}

async fn handshake(kind: Source) {
    let b = Bench::new("127.0.0.1:5214".into());
    let epoch = identity::discover(&b.env).unwrap().epoch;
    let (svc, factory) = b.service();
    svc.serve().await.unwrap();
    let client = factory.connect().await;
    // 先确认 accept 后的刷新已完成，保证本次拦截来自 hello。
    client.send("invalid frame").await.unwrap();
    assert!(matches!(
        auth::parse_server_frame(&client.recv().await.unwrap()).unwrap(),
        ServerFrame::Error { .. }
    ));
    let heartbeat = arm(&b.processes, kind);
    let _id = hello(&svc, &client, epoch).await;
    checked(heartbeat).await;
}

async fn credential(kind: Source) {
    let b = Bench::new("127.0.0.1:5214".into());
    let epoch = identity::discover(&b.env).unwrap().epoch;
    let (svc, factory) = b.service();
    svc.serve().await.unwrap();
    let client = factory.connect().await;
    let id = hello(&svc, &client, epoch).await;
    svc.approve(&id).await.unwrap();
    assert!(matches!(
        auth::parse_server_frame(&client.recv().await.unwrap()).unwrap(),
        ServerFrame::Granted { .. }
    ));
    let heartbeat = arm(&b.processes, kind);
    let frame = ClientFrame::Credential(CredentialFrame {
        binding_id: id,
        cookie: "fixture-cookie".into(),
        browser_cookie: None,
    });
    client.send(&auth::client_frame_line(&frame)).await.unwrap();
    let reply = auth::parse_server_frame(&client.recv().await.unwrap()).unwrap();
    checked(heartbeat).await;
    assert!(matches!(reply, ServerFrame::Granted { .. }));
}

async fn authorize_grant(kind: Source) {
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
    let caller = ClientIdentity {
        public_key_spki: keys.spki.clone(),
        executable: PathBuf::from(format!(r"{INSTALL}\DeepSeek Harness.exe")),
        pid: PID,
        started_at_ms: START,
    };
    let heartbeat = arm(&b.processes, kind);
    let result = svc.authorize_grant(&host, &caller).await;
    checked(heartbeat).await;
    assert!(result.is_ok(), "已交付凭据的调用方应取得授权句柄");
}

#[tokio::test(flavor = "current_thread")]
async fn authorize_grant_keeps_heartbeat_running_during_process_probe() {
    authorize_grant(Source::Process).await;
}

#[tokio::test(flavor = "current_thread")]
async fn authorize_grant_keeps_heartbeat_running_during_listener_probe() {
    authorize_grant(Source::Listener).await;
}

#[tokio::test(flavor = "current_thread")]
async fn hello_keeps_heartbeat_running_during_process_probe() {
    handshake(Source::Process).await;
}
#[tokio::test(flavor = "current_thread")]
async fn hello_keeps_heartbeat_running_during_listener_probe() {
    handshake(Source::Listener).await;
}
#[tokio::test(flavor = "current_thread")]
async fn credential_keeps_heartbeat_running_during_process_probe() {
    credential(Source::Process).await;
}
#[tokio::test(flavor = "current_thread")]
async fn credential_keeps_heartbeat_running_during_listener_probe() {
    credential(Source::Listener).await;
}

#[tokio::test(flavor = "current_thread")]
async fn initial_publish_keeps_heartbeat_running_during_process_probe() {
    initial_publish(Source::Process).await;
}
#[tokio::test(flavor = "current_thread")]
async fn initial_publish_keeps_heartbeat_running_during_listener_probe() {
    initial_publish(Source::Listener).await;
}
#[tokio::test(flavor = "current_thread")]
async fn accept_refresh_keeps_heartbeat_running_during_process_probe() {
    accept_refresh(Source::Process).await;
}
#[tokio::test(flavor = "current_thread")]
async fn accept_refresh_keeps_heartbeat_running_during_listener_probe() {
    accept_refresh(Source::Listener).await;
}
#[tokio::test(flavor = "current_thread")]
async fn approval_keeps_heartbeat_running_during_process_probe() {
    approval(Source::Process).await;
}
#[tokio::test(flavor = "current_thread")]
async fn approval_keeps_heartbeat_running_during_listener_probe() {
    approval(Source::Listener).await;
}
