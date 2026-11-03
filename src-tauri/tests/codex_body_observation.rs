//! 假 Desktop 的原始接收时间契约。
use multi_agents_manager_lib::services::codex_desktop_ipc::{
    codec::encode_frame,
    connection::{spawn, ConnectionEvent},
    profile::ProtocolProfile,
};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

#[tokio::test]
async fn observation_time_is_transport_arrival_before_projection_work() {
    let (io, mut peer) = tokio::io::duplex(65536);
    let conn = spawn(
        Box::new(io),
        ProtocolProfile::unverified("macos", "test"),
        1,
    );
    let mut rx = conn.subscribe();
    let f = serde_json::json!({"type":"broadcast","method":"test","params":{}});
    let mut bytes = encode_frame(&f).unwrap();
    bytes.extend(encode_frame(&f).unwrap());
    peer.write_all(&bytes).await.unwrap();
    let a = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    let b = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    let (a, b) = match (a, b) {
        (
            ConnectionEvent::Broadcast { arrival: a, .. },
            ConnectionEvent::Broadcast { arrival: b, .. },
        ) => (a, b),
        _ => panic!("expected broadcasts"),
    };
    assert!(a.mono_ns > 0 && a.wall_ms > 0);
    assert_eq!(a.mono_ns, b.mono_ns);
    assert_eq!(a.wall_ms, b.wall_ms);
    conn.shutdown().await;
}

#[tokio::test]
async fn split_frame_uses_arrival_of_completing_chunk() {
    let (io, mut peer) = tokio::io::duplex(65536);
    let conn = spawn(
        Box::new(io),
        ProtocolProfile::unverified("macos", "test"),
        1,
    );
    let mut rx = conn.subscribe();
    let frame = encode_frame(&serde_json::json!({"type":"broadcast","method":"test"})).unwrap();
    let split = frame.len() / 2;
    let mut first = frame.clone();
    first.extend_from_slice(&frame[..split]);
    peer.write_all(&first).await.unwrap();
    let a = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    peer.write_all(&frame[split..]).await.unwrap();
    let b = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .unwrap()
        .unwrap();
    let (a, b) = match (a, b) {
        (
            ConnectionEvent::Broadcast { arrival: a, .. },
            ConnectionEvent::Broadcast { arrival: b, .. },
        ) => (a, b),
        _ => panic!("expected broadcasts"),
    };
    assert!(b.mono_ns > a.mono_ns);
    conn.shutdown().await;
}
