//! Codex Desktop IPC 集成测试（本地假 IPC，Tokio duplex ScriptedPeer；不连接真实 Desktop）。
//!
//! 覆盖：RPC 关联（id+method）、未知投递边界（部分写/EOF=Unknown；排队前失败=NotSent）、
//! discovery 必答（旁路容量）、在飞上限、超时=Unknown。
//!
//! 并发模式：`conn.request()` 返回惰性 future；需要「请求在飞 + 同时做对端 IO」的
//! 用例必须把请求放 tokio::task（用克隆句柄拿到 'static），否则 recv 会永远等不到帧。

use multi_agents_manager_lib::services::codex_desktop_ipc::codec::encode_frame;
use multi_agents_manager_lib::services::codex_desktop_ipc::connection::{
    spawn, ConnectionEvent, ConnectionHandle, RpcOutcome, RpcRequest, RPC_TIMEOUT,
};
use multi_agents_manager_lib::services::codex_desktop_ipc::owners::{
    discover_and_follow, OwnerRegistry,
};
use multi_agents_manager_lib::services::codex_desktop_ipc::profile::profile_for;
use multi_agents_manager_lib::services::codex_desktop_ipc::types::{Delivery, ThreadKey};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

/// 本地假 Desktop：duplex 一端，帧级收发。
/// 同批读到的多余帧进 backlog（对端可能把多帧合并成一次写出）。
pub struct ScriptedPeer {
    io: DuplexStream,
    backlog: std::collections::VecDeque<serde_json::Value>,
}

impl ScriptedPeer {
    pub fn new() -> (
        Box<dyn multi_agents_manager_lib::services::codex_desktop_ipc::transport::IpcIo>,
        Self,
    ) {
        let (client, server) = tokio::io::duplex(64 * 1024);
        (
            Box::new(client),
            Self {
                io: server,
                backlog: std::collections::VecDeque::new(),
            },
        )
    }

    /// 读取一帧（返回 JSON）。
    pub async fn recv(&mut self) -> serde_json::Value {
        if let Some(f) = self.backlog.pop_front() {
            return f;
        }
        let mut decoder =
            multi_agents_manager_lib::services::codex_desktop_ipc::codec::FrameDecoder::new();
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            let n = self.io.read(&mut buf).await.expect("peer read");
            assert!(n > 0, "peer EOF");
            let mut frames = decoder.push(&buf[..n]).expect("decode");
            if frames.is_empty() {
                continue;
            }
            let first = frames.remove(0);
            for f in frames {
                self.backlog.push_back(f);
            }
            return first;
        }
    }

    /// 发送一帧。
    pub async fn send(&mut self, value: &serde_json::Value) {
        let frame = encode_frame(value).expect("encode");
        self.io.write_all(&frame).await.expect("peer write");
    }

    /// 关闭连接（EOF）。
    pub fn close(self) {
        drop(self.io);
    }

    /// 直接写原始字节（半帧/垃圾流用）。
    pub async fn send_raw(&mut self, bytes: &[u8]) {
        self.io.write_all(bytes).await.expect("peer raw write");
    }
}

/// 标准 initialize 握手：读 initialize → 回 clientId。
async fn handshake(peer: &mut ScriptedPeer, client_id: &str) -> serde_json::Value {
    let init = peer.recv().await;
    assert_eq!(init["method"], "initialize");
    let rid = init["requestId"].clone();
    peer.send(&serde_json::json!({
        "type": "response",
        "requestId": rid,
        "resultType": "success",
        "method": "initialize",
        "handledByClientId": client_id,
        "result": { "clientId": client_id }
    }))
    .await;
    init
}

/// 等待 Initialized：先查状态口（广播可能早于订阅发出），兜底听事件。
async fn wait_initialized(conn: &ConnectionHandle) -> String {
    if let Some(id) = conn.initialized_client() {
        return id;
    }
    let mut rx = conn.subscribe();
    loop {
        if let Some(id) = conn.initialized_client() {
            return id;
        }
        match tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
            Ok(Ok(ConnectionEvent::Initialized { client_id, .. })) => return client_id,
            Ok(Ok(ConnectionEvent::Broadcast { .. })) => continue,
            Ok(Ok(ConnectionEvent::Disconnected { .. })) | Ok(Err(_)) => {
                panic!("连接在握手前断开")
            }
            Err(_) => continue, // 状态口轮询
        }
    }
}

fn simple_request(method: &str) -> RpcRequest {
    RpcRequest {
        method: method.into(),
        params: serde_json::json!({ "probe": true }),
        version: None,
        target: None,
        mutating: false,
        validity: None,
    }
}

/// 在独立任务中发起请求（惰性 future 需要被轮询才会真正出帧）。
fn request_in_task(
    conn: &ConnectionHandle,
    req: RpcRequest,
) -> tokio::task::JoinHandle<RpcOutcome> {
    let c = conn.clone();
    tokio::spawn(async move { c.request(req).await })
}

#[tokio::test]
async fn rpc_matches_id_and_method() {
    let (io, mut peer) = ScriptedPeer::new();
    let profile = profile_for("macos", "26.1013.31256").unwrap();
    let conn = spawn(io, profile, 1);
    handshake(&mut peer, "owner-1").await;
    assert_eq!(wait_initialized(&conn).await, "owner-1");

    let t1 = request_in_task(&conn, simple_request("thread-owner-discovery"));
    let t2 = request_in_task(&conn, simple_request("thread-follower-start-turn"));
    // 两帧到达顺序由任务调度决定：按 method 找帧
    let mut r1 = None;
    let mut r2 = None;
    for _ in 0..2 {
        let f = tokio::time::timeout(Duration::from_secs(5), peer.recv())
            .await
            .expect("帧在 5s 内到达");
        if f["method"] == "thread-owner-discovery" && r1.is_none() {
            r1 = Some(f);
        } else {
            r2 = Some(f);
        }
    }
    let r1 = r1.expect("discovery frame");
    let r2 = r2.expect("start-turn frame");
    // f2 的回执带错 method（r1 的）：不得唤醒 f2
    peer.send(&serde_json::json!({
        "type": "response",
        "requestId": r2["requestId"],
        "resultType": "success",
        "method": "thread-owner-discovery",
        "result": {}
    }))
    .await;
    // 正确回 r1（带 handledByClientId）
    peer.send(&serde_json::json!({
        "type": "response",
        "requestId": r1["requestId"],
        "resultType": "success",
        "method": "thread-owner-discovery",
        "handledByClientId": "owner-1",
        "result": { "supportsUntrustedAppInput": true }
    }))
    .await;
    let o1 = tokio::time::timeout(Duration::from_secs(5), t1)
        .await
        .expect("t1 in time")
        .expect("task ok");
    assert_eq!(o1.delivery, Delivery::Accepted);
    assert_eq!(
        o1.result.unwrap()["supportsUntrustedAppInput"],
        serde_json::json!(true)
    );
    // f2 的迟到正确回执（补发）：此刻才完成
    peer.send(&serde_json::json!({
        "type": "response",
        "requestId": r2["requestId"],
        "resultType": "success",
        "method": "thread-follower-start-turn",
        "result": { "turn": { "id": "t1" } }
    }))
    .await;
    let o2 = tokio::time::timeout(Duration::from_secs(5), t2)
        .await
        .expect("t2 in time")
        .expect("task ok");
    assert_eq!(o2.delivery, Delivery::Accepted);
    conn.shutdown().await;
}

#[tokio::test]
async fn partial_write_is_unknown() {
    let (io, mut peer) = ScriptedPeer::new();
    let profile = profile_for("macos", "26.1013.31256").unwrap();
    let conn = spawn(io, profile, 1);
    handshake(&mut peer, "owner-1").await;
    let _ = wait_initialized(&conn).await;

    let req = RpcRequest {
        mutating: true,
        ..simple_request("thread-follower-start-turn")
    };
    let task = request_in_task(&conn, req);
    let frame = peer.recv().await;
    // 收到写请求后立即断线（不回执）→ 未知投递
    assert_eq!(frame["method"], "thread-follower-start-turn");
    peer.close();
    let outcome = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("outcome within 5s")
        .expect("task ok");
    assert_eq!(outcome.delivery, Delivery::UnknownDelivery);
    assert!(conn.is_closed());
}

#[tokio::test]
async fn half_frame_then_eof_is_unknown() {
    let (io, mut peer) = ScriptedPeer::new();
    let profile = profile_for("macos", "26.1013.31256").unwrap();
    let conn = spawn(io, profile, 1);
    handshake(&mut peer, "owner-1").await;
    let _ = wait_initialized(&conn).await;
    let task = request_in_task(&conn, simple_request("thread-owner-discovery"));
    let _frame = peer.recv().await;
    // 模拟对端回了半帧后断线：请求方结果必须未知（不是 NotSent）
    let echo = encode_frame(&serde_json::json!({"half": true})).unwrap();
    peer.send_raw(&echo[..3]).await;
    peer.close();
    let outcome = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("outcome")
        .expect("task ok");
    assert_eq!(outcome.delivery, Delivery::UnknownDelivery);
}

#[tokio::test]
async fn discovery_is_answered_while_write_waits() {
    let (io, mut peer) = ScriptedPeer::new();
    let profile = profile_for("macos", "26.1013.31256").unwrap();
    let conn = spawn(io, profile, 1);
    handshake(&mut peer, "owner-1").await;
    let _ = wait_initialized(&conn).await;

    // 发起一个慢 RPC（对端不回执；任务保持在飞）
    let slow = request_in_task(&conn, simple_request("thread-owner-discovery"));
    let frame = peer.recv().await;
    // Desktop 发来 discovery 询问：必须立即回 canHandle:false，不被慢 RPC 堵住
    peer.send(&serde_json::json!({
        "type": "client-discovery-request",
        "requestId": 7,
        "request": { "method": "thread-owner-discovery", "params": {} }
    }))
    .await;
    let reply = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let f = peer.recv().await;
            if f["type"] == "client-discovery-response" {
                return f;
            }
        }
    })
    .await
    .expect("discovery reply within 2s");
    assert_eq!(reply["requestId"], 7);
    assert_eq!(reply["response"]["canHandle"], serde_json::json!(false));
    // 慢 RPC 仍在等待（随后回执完成）
    peer.send(&serde_json::json!({
        "type": "response",
        "requestId": frame["requestId"],
        "resultType": "success",
        "method": "thread-owner-discovery",
        "result": {}
    }))
    .await;
    let outcome = tokio::time::timeout(Duration::from_secs(5), slow)
        .await
        .expect("slow in time")
        .expect("task ok");
    assert_eq!(outcome.delivery, Delivery::Accepted);
    conn.shutdown().await;
}

#[tokio::test]
async fn stale_epoch_or_invalid_owner_is_not_sent_without_write() {
    let (io, mut peer) = ScriptedPeer::new();
    let profile = profile_for("macos", "26.1013.31256").unwrap();
    // 连接 epoch=1；lease 记录在 epoch=2 → 直接零投递
    let conn = spawn(io, profile, 1);
    handshake(&mut peer, "owner-1").await;
    let _ = wait_initialized(&conn).await;

    let mut lease = multi_agents_manager_lib::services::codex_desktop_ipc::types::OwnerLease {
        key: multi_agents_manager_lib::services::codex_desktop_ipc::types::ThreadKey {
            host_id: "local".into(),
            thread_id: "t1".into(),
        },
        client_id: "owner-1".into(),
        epoch: 2, // ≠ 连接 epoch
        generation: 1,
    };
    let req = RpcRequest {
        target: Some(lease.clone()),
        ..simple_request("thread-follower-start-turn")
    };
    let outcome = conn.request(req).await;
    assert_eq!(outcome.delivery, Delivery::NotSent);
    assert_eq!(outcome.reason_code.as_deref(), Some("stale_epoch"));

    // validity 已失活 → 零投递
    let (vt, vr) = tokio::sync::watch::channel(false);
    lease.epoch = 1;
    let req = RpcRequest {
        target: Some(lease),
        validity: Some(vr),
        ..simple_request("thread-follower-start-turn")
    };
    let outcome = conn.request(req).await;
    assert_eq!(outcome.delivery, Delivery::NotSent);
    assert_eq!(outcome.reason_code.as_deref(), Some("owner_invalid"));
    let _ = vt; // 保持 sender 存活至断言结束

    // 对端没有收到任何 mutating 帧
    let drain = tokio::time::timeout(Duration::from_millis(200), peer.recv()).await;
    assert!(drain.is_err(), "不应有帧写出");
    conn.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn timeout_is_unknown() {
    let (io, mut peer) = ScriptedPeer::new();
    let profile = profile_for("macos", "26.1013.31256").unwrap();
    let conn = spawn(io, profile, 1);
    handshake(&mut peer, "owner-1").await;
    let _ = wait_initialized(&conn).await;
    let task = request_in_task(&conn, simple_request("thread-owner-discovery"));
    let _frame = peer.recv().await; // 对端永不回执
                                    // 虚拟时间推进到超时
    let outcome = tokio::time::timeout(RPC_TIMEOUT + Duration::from_secs(1), task)
        .await
        .expect("virtual time fires")
        .expect("task ok");
    assert_eq!(outcome.delivery, Delivery::UnknownDelivery);
    assert_eq!(outcome.reason_code.as_deref(), Some("timeout"));
    conn.shutdown().await;
}

#[tokio::test]
async fn pending_capacity_rejects_before_enqueue() {
    let (io, mut peer) = ScriptedPeer::new();
    let profile = profile_for("macos", "26.1013.31256").unwrap();
    let conn = spawn(io, profile, 1);
    handshake(&mut peer, "owner-1").await;
    let _ = wait_initialized(&conn).await;

    // 串行播种 64 个在飞 RPC（每个必达对端后再播下一个）→ 在飞表确定性满 64
    let mut flights = Vec::new();
    let mut seen = Vec::new();
    for i in 0..64 {
        flights.push(request_in_task(
            &conn,
            simple_request(&format!("probe-method-{i}")),
        ));
        let frame = tokio::time::timeout(Duration::from_secs(5), peer.recv())
            .await
            .expect("帧应可达");
        seen.push(frame);
    }
    // 第 65 个：入队前拒绝，且不产生第 65 次写入
    let extra = conn.request(simple_request("probe-method-65")).await;
    assert_eq!(extra.delivery, Delivery::NotSent);
    assert_eq!(extra.reason_code.as_deref(), Some("pending_capacity"));
    let no_extra = tokio::time::timeout(Duration::from_millis(300), peer.recv()).await;
    assert!(no_extra.is_err(), "第 65 个请求不应写出");
    // 全部回执后恢复正常
    for frame in &seen {
        peer.send(&serde_json::json!({
            "type": "response",
            "requestId": frame["requestId"],
            "resultType": "success",
            "method": frame["method"],
            "result": {}
        }))
        .await;
    }
    for f in flights {
        let o = tokio::time::timeout(Duration::from_secs(5), f)
            .await
            .expect("flight in time")
            .expect("task ok");
        assert_eq!(o.delivery, Delivery::Accepted);
    }
    conn.shutdown().await;
}

// ---------------- Task 4：Owner Registry / following / 恢复代际（集成） ----------------

/// discovery 响应帧。
fn discovery_response(request_id: &str, owner: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "response",
        "requestId": request_id,
        "resultType": "success",
        "method": "thread-owner-discovery",
        "handledByClientId": owner,
        "result": { "supportsUntrustedAppInput": true }
    })
}

#[tokio::test]
async fn discover_and_follow_declares_and_waits_snapshot() {
    let (io, mut peer) = ScriptedPeer::new();
    let profile = profile_for("macos", "26.1013.31256").unwrap();
    let conn = spawn(io, profile, 1);
    handshake(&mut peer, "owner-1").await;
    let _ = wait_initialized(&conn).await;

    let owners = OwnerRegistry::default();
    let key = ThreadKey {
        host_id: "local".into(),
        thread_id: "thread-9".into(),
    };
    let task = {
        let c = conn.clone();
        let mut owners = owners;
        let key = key.clone();
        tokio::spawn(async move { discover_and_follow(&c, &mut owners, &key).await })
    };

    // 逐字段断言 discovery params（hostId 放 params；envelope 不发 hostId，version=1 基值）
    let disc = peer.recv().await;
    assert_eq!(disc["method"], "thread-owner-discovery");
    assert_eq!(disc["version"], serde_json::json!(1));
    assert!(disc.get("hostId").is_none(), "envelope 不发 hostId");
    assert_eq!(disc["params"]["hostId"], "local");
    assert_eq!(disc["params"]["conversationId"], "thread-9");
    peer.send(&discovery_response(
        disc["requestId"].as_str().unwrap(),
        "owner-1",
    ))
    .await;

    // following 声明 = broadcast v1，params 带 following:true + hostId
    let follow = peer.recv().await;
    assert_eq!(follow["type"], "broadcast");
    assert_eq!(follow["method"], "thread-stream-following-changed");
    assert_eq!(follow["version"], serde_json::json!(1));
    assert_eq!(follow["params"]["following"], serde_json::json!(true));
    assert_eq!(follow["params"]["conversationId"], "thread-9");
    assert_eq!(follow["params"]["hostId"], "local");

    // 无 following 不允许假装同步完成：snapshot 到达前不返回
    // （此处先回 snapshot，任务随即完成）
    peer.send(&serde_json::json!({
        "type": "broadcast",
        "method": "thread-stream-state-changed",
        "version": 11,
        "sourceClientId": "owner-1",
        "targetClientIds": ["tuvis"],
        "params": {
            "conversationId": "thread-9",
            "hostId": "local",
            "change": { "type": "snapshot", "revision": 1, "conversationState": {} }
        }
    }))
    .await;
    let lease = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("in time")
        .expect("task ok")
        .expect("lease");
    assert_eq!(lease.client_id, "owner-1");
    assert_eq!(lease.key.thread_id, "thread-9");
    assert_eq!(lease.epoch, 1);
    conn.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn no_snapshot_after_following_is_timeout_not_fake_success() {
    let (io, mut peer) = ScriptedPeer::new();
    let profile = profile_for("macos", "26.1013.31256").unwrap();
    let conn = spawn(io, profile, 1);
    handshake(&mut peer, "owner-1").await;
    let _ = wait_initialized(&conn).await;

    let task = {
        let c = conn.clone();
        tokio::spawn(async move {
            let mut owners = OwnerRegistry::default();
            let key = ThreadKey {
                host_id: "local".into(),
                thread_id: "thread-9".into(),
            };
            discover_and_follow(&c, &mut owners, &key).await
        })
    };
    let disc = peer.recv().await;
    peer.send(&discovery_response(
        disc["requestId"].as_str().unwrap(),
        "owner-1",
    ))
    .await;
    let _follow = peer.recv().await; // 声明后永不回 snapshot
    let result = tokio::time::timeout(RPC_TIMEOUT + Duration::from_secs(1), task)
        .await
        .expect("virtual time fires")
        .expect("task ok");
    assert!(matches!(
        result,
        Err(
            multi_agents_manager_lib::services::codex_desktop_ipc::types::ProtocolError::Timeout(_)
        )
    ));
    conn.shutdown().await;
}

#[tokio::test]
async fn discovery_no_client_found_is_error_not_owner() {
    let (io, mut peer) = ScriptedPeer::new();
    let profile = profile_for("macos", "26.1013.31256").unwrap();
    let conn = spawn(io, profile, 1);
    handshake(&mut peer, "owner-1").await;
    let _ = wait_initialized(&conn).await;

    let task = {
        let c = conn.clone();
        tokio::spawn(async move {
            let mut owners = OwnerRegistry::default();
            let key = ThreadKey {
                host_id: "local".into(),
                thread_id: "ghost".into(),
            };
            discover_and_follow(&c, &mut owners, &key).await
        })
    };
    let disc = peer.recv().await;
    peer.send(&serde_json::json!({
        "type": "response",
        "requestId": disc["requestId"],
        "resultType": "error",
        "method": "thread-owner-discovery",
        "error": { "code": "no-client-found" }
    }))
    .await;
    let result = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("in time")
        .expect("task ok");
    assert!(result.is_err(), "no-client-found 不能伪装成发现成功");
    conn.shutdown().await;
}

#[tokio::test]
async fn owner_rebind_invalidates_old_lease_end_to_end() {
    use std::sync::Arc;
    use tokio::sync::Mutex as AsyncMutex;
    let (io, mut peer) = ScriptedPeer::new();
    let profile = profile_for("macos", "26.1013.31256").unwrap();
    let conn = spawn(io, profile, 1);
    handshake(&mut peer, "owner-1").await;
    let _ = wait_initialized(&conn).await;

    let owners = Arc::new(AsyncMutex::new(OwnerRegistry::default()));
    let key = ThreadKey {
        host_id: "local".into(),
        thread_id: "thread-9".into(),
    };
    let follow_once = {
        let c = conn.clone();
        let owners = owners.clone();
        let key = key.clone();
        tokio::spawn(async move {
            let mut g = owners.lock().await;
            discover_and_follow(&c, &mut g, &key).await
        })
    };
    let disc = peer.recv().await;
    peer.send(&discovery_response(
        disc["requestId"].as_str().unwrap(),
        "owner-1",
    ))
    .await;
    let _ = peer.recv().await; // following
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "params": { "conversationId": "thread-9", "hostId": "local",
                    "change": { "type": "snapshot", "revision": 1, "conversationState": {} } }
    }))
    .await;
    let lease_a = tokio::time::timeout(Duration::from_secs(5), follow_once)
        .await
        .expect("in time")
        .expect("task ok")
        .expect("lease");

    // owner A → B（Desktop 重启换代语义）：重新 discovery 命中新 owner
    let follow_again = {
        let c = conn.clone();
        let owners = owners.clone();
        let key = key.clone();
        tokio::spawn(async move {
            let mut g = owners.lock().await;
            discover_and_follow(&c, &mut g, &key).await
        })
    };
    let disc2 = peer.recv().await;
    peer.send(&discovery_response(
        disc2["requestId"].as_str().unwrap(),
        "owner-2",
    ))
    .await;
    let _ = peer.recv().await;
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "params": { "conversationId": "thread-9", "hostId": "local",
                    "change": { "type": "snapshot", "revision": 2, "conversationState": {} } }
    }))
    .await;
    let lease_b = tokio::time::timeout(Duration::from_secs(5), follow_again)
        .await
        .expect("in time")
        .expect("task ok")
        .expect("lease");

    assert_eq!(lease_b.client_id, "owner-2");
    assert_eq!(
        lease_b.generation,
        lease_a.generation + 1,
        "换代 generation 单调"
    );
    {
        // 旧 lease 在 registry 里已非当前绑定；其真实活性信号为 false
        let g = owners.lock().await;
        assert!(!g.is_current(&lease_a));
        let mut v = g.validity(&lease_a);
        assert!(!*v.borrow_and_update(), "旧 lease validity = false");
    }
    // 旧 lease 的写请求（带真实失活信号）被拒为 NotSent，且不写帧
    let validity = {
        let g = owners.lock().await;
        g.validity(&lease_a)
    };
    let outcome = conn
        .request(RpcRequest {
            method: "thread-follower-start-turn".into(),
            params: serde_json::json!({}),
            version: Some(2),
            target: Some(lease_a.clone()),
            mutating: true,
            validity: Some(validity),
        })
        .await;
    assert_eq!(outcome.delivery, Delivery::NotSent);
    conn.shutdown().await;
}

// ---------------- Task 6：原生 Start Turn / 两类审批 / 阻塞问答（native_*） ----------------

use multi_agents_manager_lib::services::codex_desktop_ipc::adapter::{
    Connector, DesktopIpcAdapter,
};
use multi_agents_manager_lib::services::codex_desktop_ipc::transport::BoxIo;
use multi_agents_manager_lib::services::codex_desktop_ipc::types::PendingAction;
use std::sync::Mutex as StdMutex;

use std::pin::Pin;

/// 已验证 build（生产 profile 精确匹配键；R3 后能力一律由 resolver 链的 endpoint 派生）。
const MACOS_BUILD: &str = "26.1013.31256";
const WINDOWS_BUILD: &str = "26.1013.3656.0";

fn test_endpoint(
    platform: &str,
    build: &str,
) -> multi_agents_manager_lib::services::codex_desktop_ipc::transport::DesktopEndpoint {
    multi_agents_manager_lib::services::codex_desktop_ipc::transport::DesktopEndpoint {
        platform: platform.into(),
        path: "test-endpoint".into(),
        build: Some(build.into()),
    }
}

/// 测试连接器：endpoint 身份 + 单个 io（R3：连接必须携带 endpoint，能力由
/// 生产 `profile_for(platform, build)` 派生——不注入固定 profile）。
struct TestConnector {
    endpoint: multi_agents_manager_lib::services::codex_desktop_ipc::transport::DesktopEndpoint,
    io: StdMutex<
        Option<Box<dyn multi_agents_manager_lib::services::codex_desktop_ipc::transport::IpcIo>>,
    >,
}

impl TestConnector {
    fn new(
        endpoint: multi_agents_manager_lib::services::codex_desktop_ipc::transport::DesktopEndpoint,
        io: Box<dyn multi_agents_manager_lib::services::codex_desktop_ipc::transport::IpcIo>,
    ) -> Self {
        Self {
            endpoint,
            io: StdMutex::new(Some(io)),
        }
    }
}

impl Connector for TestConnector {
    fn connect_endpoint(
        &self,
    ) -> Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        (
                            BoxIo,
                            multi_agents_manager_lib::services::codex_desktop_ipc::transport::DesktopEndpoint,
                        ),
                        multi_agents_manager_lib::services::codex_desktop_ipc::types::ProtocolError,
                    >,
                > + Send
                + '_,
        >,
    >{
        Box::pin(async move {
            let io = self
                .io
                .lock()
                .expect("lock")
                .take()
                .ok_or_else(|| multi_agents_manager_lib::services::codex_desktop_ipc::types::ProtocolError::Disconnected("no io".into()))?;
            Ok((io, self.endpoint.clone()))
        })
    }
}

/// 建 harness：驱动 ensure_thread 的握手/discovery/following/snapshot，
/// 返回（adapter、peer、快照后的投影）。能力由 endpoint（platform+build）经生产
/// profile 表派生：macos=permissions 开、windows=command/blocking_input 开。
async fn harness_for(
    platform: &str,
    build: &str,
    requests: Vec<serde_json::Value>,
) -> (
    DesktopIpcAdapter,
    ScriptedPeer,
    multi_agents_manager_lib::services::codex_desktop_ipc::types::ThreadProjection,
    ThreadKey,
) {
    let (io, peer) = ScriptedPeer::new();
    let adapter = DesktopIpcAdapter::new(
        std::sync::Arc::new(TestConnector::new(test_endpoint(platform, build), io)),
        // R3 前：构造 profile 即连接期 profile；R3 后：仅为首个连接前的基线，
        // 连接后按 endpoint 重算（同值）。
        profile_for(platform, build).unwrap_or_else(
            multi_agents_manager_lib::services::codex_desktop_ipc::profile::ProtocolProfile::closed,
        ),
    );
    let key = ThreadKey {
        host_id: "local".into(),
        thread_id: "00000000-0000-4000-8000-0000000000m1".into(),
    };
    let task = {
        let a = adapter.clone();
        let key = key.clone();
        tokio::spawn(async move { a.ensure_thread(&key).await })
    };
    let mut peer = peer;
    // 握手
    let init = peer.recv().await;
    assert_eq!(init["method"], "initialize");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": init["requestId"],
        "resultType": "success", "method": "initialize",
        "handledByClientId": "owner-1", "result": { "clientId": "owner-1" }
    }))
    .await;
    // discovery
    let disc = peer.recv().await;
    assert_eq!(disc["method"], "thread-owner-discovery");
    peer.send(&discovery_response(
        disc["requestId"].as_str().unwrap(),
        "owner-1",
    ))
    .await;
    // following
    let follow = peer.recv().await;
    assert_eq!(follow["method"], "thread-stream-following-changed");
    // snapshot（带 pending requests）
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": key.thread_id, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": 209,
                "conversationState": {
                    "turns": [], "requests": requests,
                    "threadRuntimeStatus": { "type": "active" },
                    "turnHistory": { "isComplete": true, "history": {
                        "entitiesByKey": {
                            "t1": { "id": "t1", "type": "turn", "status": "inProgress", "items": ["u1"] },
                            "u1": { "id": "u1", "type": "user_message", "text": "跑测试", "ts": 1 }
                        },
                        "islands": [{ "entries": ["t1"] }]
                    }}
                }
            }
        }
    })).await;
    let projection = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("ensure_thread in time")
        .expect("join")
        .expect("projection");
    (adapter, peer, projection, key)
}

/// macOS 已验证 build harness（permissions 开；command/blocking_input 关）。
async fn harness(
    requests: Vec<serde_json::Value>,
) -> (
    DesktopIpcAdapter,
    ScriptedPeer,
    multi_agents_manager_lib::services::codex_desktop_ipc::types::ThreadProjection,
    ThreadKey,
) {
    harness_for("macos", MACOS_BUILD, requests).await
}

#[tokio::test]
async fn native_start_turn_targets_original_thread() {
    let (adapter, mut peer, _projection, key) = harness(vec![]).await;
    let client_message_id =
        multi_agents_manager_lib::services::codex_desktop_ipc::types::new_uuid();
    let task = {
        let a = adapter.clone();
        let key = key.clone();
        let cmid = client_message_id.clone();
        tokio::spawn(async move { a.start_turn(&key, "act-1", "帮我跑测试", &cmid).await })
    };
    let frame = peer.recv().await;
    // 逐字段断言：start-turn v2、target=当前 owner、会话自洽、无任何覆盖字段
    assert_eq!(frame["method"], "thread-follower-start-turn");
    assert_eq!(frame["version"], serde_json::json!(2));
    assert_eq!(frame["targetClientId"], "owner-1");
    assert_eq!(frame["params"]["conversationId"], key.thread_id);
    let req = &frame["params"]["turnStart"]["request"];
    assert_eq!(
        req["threadId"], key.thread_id,
        "turnStart.request.threadId == conversationId"
    );
    assert_eq!(req["input"][0]["type"], "text");
    assert_eq!(req["input"][0]["text"], "帮我跑测试");
    assert_eq!(req["input"][0]["text_elements"], serde_json::json!([]));
    assert_eq!(req["clientUserMessageId"], client_message_id);
    assert_eq!(
        frame["params"]["turnStart"]["context"],
        serde_json::json!({}),
        "context 空对象"
    );
    // 严格键集：无 model / 权限 / collaboration 覆盖
    let req_keys: Vec<String> = req.as_object().unwrap().keys().cloned().collect();
    assert_eq!(req_keys, vec!["threadId", "input", "clientUserMessageId"]);
    let ts_keys: Vec<String> = frame["params"]["turnStart"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    assert_eq!(ts_keys, vec!["request", "context"]);
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "turn": { "id": "turn-9", "status": "inProgress" } }
    }))
    .await;
    let outcome = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("in time")
        .expect("join");
    assert_eq!(outcome.delivery, Delivery::Accepted);
    adapter.shutdown().await;
}

#[tokio::test]
async fn native_command_approval_uses_decision_wire() {
    // R3 后能力由生产 profile 派生：command 已验证 = Windows build（permissions 未验证）
    let (adapter, mut peer, projection, key) = harness_for(
        "windows",
        WINDOWS_BUILD,
        vec![serde_json::json!({
            "id": 18, "method": "item/commandExecution/requestApproval",
            "params": { "command": "pnpm test", "cwd": "/w", "reason": "r" }
        })],
    )
    .await;
    let cmd_key = projection
        .pending
        .iter()
        .find(|p| p.kind == multi_agents_manager_lib::services::codex_desktop_ipc::types::PendingKind::CommandApproval)
        .expect("command pending")
        .clone();

    // command approve：thread-follower-command-approval-decision + decision:"accept"
    let t = {
        let a = adapter.clone();
        let key = key.clone();
        let rk = cmd_key.request_key.clone();
        let tk = cmd_key.context_token.clone();
        tokio::spawn(async move { a.respond(&key, &rk, &tk, PendingAction::Approve).await })
    };
    let frame = peer.recv().await;
    assert_eq!(frame["method"], "thread-follower-command-approval-decision");
    assert_eq!(frame["version"], serde_json::json!(1));
    assert_eq!(
        frame["params"]["requestId"],
        serde_json::json!(18),
        "整数 id 无损"
    );
    assert_eq!(frame["params"]["decision"], "accept");
    assert!(
        frame["params"].get("permissions").is_none(),
        "command 变体绝不带 permissions"
    );
    assert!(
        frame["params"].get("response").is_none(),
        "command 变体绝不用 permissions 回执形态"
    );
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-command-approval-decision",
        "result": { "ok": true }
    }))
    .await;
    let o = tokio::time::timeout(Duration::from_secs(5), t)
        .await
        .expect("in time")
        .expect("join");
    assert_eq!(o.delivery, Delivery::Accepted);
    adapter.shutdown().await;
}

#[tokio::test]
async fn native_permissions_approval_echoes_saved_permissions() {
    // R3 后能力由生产 profile 派生：permissions 已验证 = macOS build（command 未验证）
    let (adapter, mut peer, projection, key) = harness_for(
        "macos",
        MACOS_BUILD,
        vec![serde_json::json!({
            "id": 12, "method": "item/permissions/requestApproval",
            "params": { "cwd": "/tmp/x", "permissions": {
                "network": serde_json::Value::Null,
                "fileSystem": { "read": serde_json::Value::Null, "write": ["/tmp/x"], "entries": [] }
            } }
        })],
    )
    .await;
    let perm_key = projection
        .pending
        .iter()
        .find(|p| p.kind == multi_agents_manager_lib::services::codex_desktop_ipc::types::PendingKind::PermissionsApproval)
        .expect("permissions pending")
        .clone();

    // permissions approve：回显服务端保存的原 permissions + scope=turn
    let t = {
        let a = adapter.clone();
        let key = key.clone();
        let rk = perm_key.request_key.clone();
        let tk = perm_key.context_token.clone();
        tokio::spawn(async move { a.respond(&key, &rk, &tk, PendingAction::Approve).await })
    };
    let frame = peer.recv().await;
    assert_eq!(
        frame["method"],
        "thread-follower-permissions-request-approval-response"
    );
    assert_eq!(frame["params"]["requestId"], serde_json::json!(12));
    assert!(
        frame["params"].get("decision").is_none(),
        "permissions 变体不用 decision 词"
    );
    let resp = &frame["params"]["response"];
    assert_eq!(resp["scope"], "turn");
    assert_eq!(
        resp["permissions"],
        serde_json::json!({
            "network": serde_json::Value::Null,
            "fileSystem": { "read": serde_json::Value::Null, "write": ["/tmp/x"], "entries": [] }
        }),
        "原 permissions 对象回显"
    );
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-permissions-request-approval-response",
        "result": { "ok": true }
    }))
    .await;
    let _ = tokio::time::timeout(Duration::from_secs(5), t)
        .await
        .expect("in time")
        .expect("join");

    // permissions reject：permissions = {}
    let t = {
        let a = adapter.clone();
        let key = key.clone();
        let rk = perm_key.request_key.clone();
        let tk = perm_key.context_token.clone();
        tokio::spawn(async move { a.respond(&key, &rk, &tk, PendingAction::Reject).await })
    };
    let frame = peer.recv().await;
    assert_eq!(
        frame["params"]["response"]["permissions"],
        serde_json::json!({})
    );
    assert_eq!(frame["params"]["response"]["scope"], "turn");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-permissions-request-approval-response",
        "result": { "ok": true }
    }))
    .await;
    let _ = tokio::time::timeout(Duration::from_secs(5), t)
        .await
        .expect("in time")
        .expect("join");
    adapter.shutdown().await;
}

#[tokio::test]
async fn native_answer_binds_current_pending() {
    // user-input 夹具：q-1（isOther）+ q-2（isSecret）
    let ui: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(format!(
            "{}/tests/fixtures/codex-desktop-ipc/user-input.json",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap(),
    )
    .expect("fixture json");
    // R3 后能力由生产 profile 派生：blocking_input 已验证 = Windows build
    let (adapter, mut peer, projection, key) =
        harness_for("windows", WINDOWS_BUILD, vec![ui]).await;
    let pending = projection
        .pending
        .iter()
        .find(|p| p.kind == multi_agents_manager_lib::services::codex_desktop_ipc::types::PendingKind::UserInput)
        .expect("user input pending")
        .clone();

    // 未知 qid → 拒绝且零投递
    let bad = PendingAction::Answer(
        [
            ("q-1", vec!["alpha".to_string()]),
            ("q-ghost", vec!["x".to_string()]),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect(),
    );
    let o = adapter
        .respond(&key, &pending.request_key, &pending.context_token, bad)
        .await;
    assert_eq!(o.delivery, Delivery::NotSent);
    assert_eq!(o.reason_code.as_deref(), Some("answers_mismatch"));

    // 漏答 → 拒绝
    let missing = PendingAction::Answer(
        [("q-1", vec!["alpha".to_string()])]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    );
    let o = adapter
        .respond(&key, &pending.request_key, &pending.context_token, missing)
        .await;
    assert_eq!(o.delivery, Delivery::NotSent);

    // token 错 → 拒绝（过期卡）
    let ok_answer = PendingAction::Answer(
        [
            ("q-1", vec!["alpha".to_string()]),
            ("q-2", vec!["口令".to_string()]),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect(),
    );
    let o = adapter
        .respond(&key, &pending.request_key, "wrong-token", ok_answer.clone())
        .await;
    assert_eq!(o.delivery, Delivery::NotSent);
    assert_eq!(o.reason_code.as_deref(), Some("stale_context"));
    let no_frame = tokio::time::timeout(Duration::from_millis(200), peer.recv()).await;
    assert!(no_frame.is_err(), "被拒动作不得写出帧");

    // 正确作答：原 qid 映射、isSecret 在 wire 上出现但不回显于错误/日志
    let t = {
        let a = adapter.clone();
        let key = key.clone();
        let rk = pending.request_key.clone();
        let tk = pending.context_token.clone();
        let ans = ok_answer.clone();
        tokio::spawn(async move { a.respond(&key, &rk, &tk, ans).await })
    };
    let frame = peer.recv().await;
    assert_eq!(frame["method"], "thread-follower-submit-user-input");
    assert_eq!(frame["version"], serde_json::json!(1));
    assert_eq!(
        frame["params"]["requestId"],
        serde_json::json!("req-str-9007199254740993"),
        "字符串 id 无损"
    );
    assert_eq!(
        frame["params"]["response"]["answers"],
        serde_json::json!({
            "q-1": { "answers": ["alpha"] },
            "q-2": { "answers": ["口令"] }
        })
    );
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-submit-user-input",
        "result": { "ok": true }
    }))
    .await;
    let o = tokio::time::timeout(Duration::from_secs(5), t)
        .await
        .expect("in time")
        .expect("join");
    assert_eq!(o.delivery, Delivery::Accepted);
    adapter.shutdown().await;
}

#[tokio::test]
async fn native_is_other_false_rejects_free_text() {
    let req = serde_json::json!({
        "id": "req-fixed", "method": "item/tool/requestUserInput",
        "params": { "isBlocking": true, "questions": [
            { "id": "q1", "header": "h", "question": "?", "isOther": false, "isSecret": false,
              "options": [ { "label": "alpha", "description": "" } ] }
        ]}
    });
    // R3 后能力由生产 profile 派生：blocking_input 已验证 = Windows build
    let (adapter, mut peer, projection, key) =
        harness_for("windows", WINDOWS_BUILD, vec![req]).await;
    let pending = projection.pending[0].clone();
    // isOther=false：自由文本拒绝
    let free_text = PendingAction::Answer(
        [("q1", vec!["自拟答案".to_string()])]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    );
    let o = adapter
        .respond(
            &key,
            &pending.request_key,
            &pending.context_token,
            free_text,
        )
        .await;
    assert_eq!(o.delivery, Delivery::NotSent);
    assert_eq!(o.reason_code.as_deref(), Some("answer_not_option"));
    // 选项 label 接受
    let t = {
        let a = adapter.clone();
        let key = key.clone();
        let rk = pending.request_key.clone();
        let tk = pending.context_token.clone();
        tokio::spawn(async move {
            a.respond(
                &key,
                &rk,
                &tk,
                PendingAction::Answer(
                    [("q1", vec!["alpha".to_string()])]
                        .into_iter()
                        .map(|(k, v)| (k.to_string(), v))
                        .collect(),
                ),
            )
            .await
        })
    };
    let frame = peer.recv().await;
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-submit-user-input",
        "result": { "ok": true }
    }))
    .await;
    let o = tokio::time::timeout(Duration::from_secs(5), t)
        .await
        .expect("in time")
        .expect("join");
    assert_eq!(o.delivery, Delivery::Accepted);
    adapter.shutdown().await;
}

#[tokio::test]
async fn native_capability_gated_and_unsupported_requests_never_send() {
    // macOS 生产 profile（endpoint 派生）：blocking_input=false → user-input 可见但不可答
    let req = serde_json::json!({
        "id": 7, "method": "item/tool/requestUserInput",
        "params": { "isBlocking": true, "questions": [
            { "id": "q1", "header": "h", "question": "?", "isOther": true, "isSecret": false, "options": [] }
        ]}
    });
    let (adapter, mut peer, projection, key) = harness(vec![req]).await;
    let pending = &projection.pending[0];
    assert_eq!(
        pending.kind,
        multi_agents_manager_lib::services::codex_desktop_ipc::types::PendingKind::UserInput
    );
    assert!(!pending.actionable, "能力未验证 → 不可操作");
    assert_eq!(
        pending.disabled_reason_code.as_deref(),
        Some("capability_unavailable")
    );
    let o = adapter
        .respond(
            &key,
            &pending.request_key,
            &pending.context_token,
            PendingAction::Answer(
                [("q1", vec!["x".to_string()])]
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v))
                    .collect(),
            ),
        )
        .await;
    assert_eq!(o.delivery, Delivery::NotSent);
    assert_eq!(o.reason_code.as_deref(), Some("capability_unavailable"));
    let no_frame = tokio::time::timeout(Duration::from_millis(200), peer.recv()).await;
    assert!(no_frame.is_err(), "能力门拒绝不得写出帧");

    // 未知 method：Unsupported 卡
    let o = adapter
        .respond(&key, "n:1", "t", PendingAction::Approve)
        .await;
    assert_eq!(o.delivery, Delivery::NotSent);
    assert_eq!(
        o.reason_code.as_deref(),
        Some("stale_context"),
        "未知 request 无卡可答"
    );
    adapter.shutdown().await;
}

// ================= R1–R5 返工回归（2026-10-17 审计；TDD 先红后绿） =================

use multi_agents_manager_lib::services::codex_desktop_ipc::transport::EndpointResolver;
use multi_agents_manager_lib::services::codex_desktop_ipc::types::ProtocolError;
use tokio::sync::broadcast;

fn tkey(id: &str) -> ThreadKey {
    ThreadKey {
        host_id: "local".into(),
        thread_id: id.into(),
    }
}

/// permissions 审批 pending（整数 id 12；macos verified build 下 actionable）。
fn permissions_request() -> serde_json::Value {
    serde_json::json!({
        "id": 12, "method": "item/permissions/requestApproval",
        "params": { "cwd": "/tmp/x", "permissions": {
            "network": serde_json::Value::Null,
            "fileSystem": { "read": serde_json::Value::Null, "write": ["/tmp/x"], "entries": [] }
        } }
    })
}

/// 标准 snapshot change（单条用户消息 + idle runtime；可带 pending / 历史完整性）。
fn snapshot_change(
    rev: u64,
    requests: Vec<serde_json::Value>,
    history_complete: bool,
) -> serde_json::Value {
    serde_json::json!({
        "type": "snapshot", "revision": rev,
        "conversationState": {
            "turns": [], "requests": requests,
            "threadRuntimeStatus": { "type": "idle" },
            "turnHistory": { "isComplete": history_complete, "history": {
                "entitiesByKey": {
                    "t1": { "id": "t1", "type": "turn", "status": "completed", "items": ["u1"] },
                    "u1": { "id": "u1", "type": "user_message", "text": "hello", "ts": 1 }
                },
                "islands": [ { "entries": ["t1"] } ]
            }}
        }
    })
}

fn state_frame(thread_id: &str, source: &str, change: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": source, "targetClientIds": ["tuvis"],
        "params": { "conversationId": thread_id, "hostId": "local", "change": change }
    })
}

/// 限时收帧（防测试悬挂）。
async fn recv_frame(peer: &mut ScriptedPeer, secs: u64) -> serde_json::Value {
    tokio::time::timeout(Duration::from_secs(secs), peer.recv())
        .await
        .expect("帧超时")
}

/// 驱动一次完整 follow（可含 init），并在 following 声明后下发 snapshot。
/// 严格按 discovery→following 顺序（测试自身无并发帧时使用）。
async fn drive_follow(
    peer: &mut ScriptedPeer,
    thread_id: &str,
    owner: &str,
    snapshot: serde_json::Value,
    with_init: bool,
) {
    drive_follow_collecting(peer, thread_id, owner, snapshot, with_init).await;
}

/// 宽容版 follow 驱动：按 method 分拣帧，混入的无关请求帧（如并发 respond）原样
/// 返回给调用方处置；following 声明后下发 snapshot。
async fn drive_follow_collecting(
    peer: &mut ScriptedPeer,
    thread_id: &str,
    owner: &str,
    snapshot: serde_json::Value,
    with_init: bool,
) -> Vec<serde_json::Value> {
    let mut others = Vec::new();
    if with_init {
        let init = recv_frame(peer, 10).await;
        assert_eq!(init["method"], "initialize");
        peer.send(&serde_json::json!({
            "type": "response", "requestId": init["requestId"],
            "resultType": "success", "method": "initialize",
            "handledByClientId": owner, "result": { "clientId": owner }
        }))
        .await;
    }
    loop {
        let frame = recv_frame(peer, 10).await;
        match frame["method"].as_str().unwrap_or("") {
            "thread-owner-discovery" => {
                assert_eq!(frame["params"]["conversationId"], thread_id);
                peer.send(&discovery_response(
                    frame["requestId"].as_str().unwrap(),
                    owner,
                ))
                .await;
            }
            "thread-stream-following-changed" => {
                assert_eq!(frame["params"]["conversationId"], thread_id);
                break;
            }
            _ => others.push(frame),
        }
    }
    peer.send(&state_frame(thread_id, owner, snapshot)).await;
    others
}

/// 等**多个** thread 各自的下一个投影（收集式：非目标帧缓存复用，不丢弃——
/// 双线程失效测试的帧到达顺序随机，逐个过滤会吞掉另一线程的帧，满载下
/// ~50% 假红；R9 复审 P3-1）
async fn wait_projections_multi(
    rx: &mut tokio::sync::broadcast::Receiver<
        multi_agents_manager_lib::services::codex_desktop_ipc::types::ThreadProjection,
    >,
    thread_ids: &[&str],
    stale_only: bool,
    secs: u64,
) -> std::collections::HashMap<
    String,
    multi_agents_manager_lib::services::codex_desktop_ipc::types::ThreadProjection,
> {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    let mut got = std::collections::HashMap::new();
    let mut skipped: Vec<
        multi_agents_manager_lib::services::codex_desktop_ipc::types::ThreadProjection,
    > = Vec::new();
    while got.len() < thread_ids.len() {
        assert!(
            std::time::Instant::now() < deadline,
            "等待 threads {thread_ids:?} 投影超时（已得 {:?}）",
            got.keys().collect::<Vec<_>>()
        );
        let p = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await;
        let Ok(Ok(p)) = p else { continue };
        if stale_only && !p.stale {
            continue;
        }
        if thread_ids.contains(&p.thread_id.as_str()) {
            got.insert(p.thread_id.clone(), p);
        } else {
            skipped.push(p); // 非目标帧保留回灌，不吞
        }
    }
    got
}

/// 等 SSE 里某 thread 的下一个投影（可选 stale 过滤）。
async fn wait_projection(
    rx: &mut broadcast::Receiver<
        multi_agents_manager_lib::services::codex_desktop_ipc::types::ThreadProjection,
    >,
    thread_id: &str,
    stale_only: bool,
    secs: u64,
) -> multi_agents_manager_lib::services::codex_desktop_ipc::types::ThreadProjection {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    loop {
        assert!(
            std::time::Instant::now() < deadline,
            "等待 thread {thread_id} 投影超时"
        );
        let p = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await;
        let Ok(Ok(p)) = p else { continue };
        if p.thread_id == thread_id && (!stale_only || p.stale) {
            return p;
        }
    }
}

/// 轮询直到镜像推进到目标 revision（projection 只读，不触发连接）。
async fn wait_revision(
    adapter: &DesktopIpcAdapter,
    key: &ThreadKey,
    rev: u64,
) -> multi_agents_manager_lib::services::codex_desktop_ipc::types::ThreadProjection {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let p = adapter.projection(key).await;
        if p.revision == Some(rev) {
            return p;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "等待 revision {rev} 超时（当前 {:?}）",
            p.revision
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// 帧断言：对端（超时）无新帧。
async fn assert_no_frame(peer: &mut ScriptedPeer, ms: u64) {
    let got = tokio::time::timeout(Duration::from_millis(ms), peer.recv()).await;
    assert!(
        got.is_err(),
        "不应有新帧：{:?}",
        got.ok().map(|f| f["method"].clone())
    );
}

/// R3 resolver 链：endpoint 由 resolver 产（生产 profile_for 消费 build），io 由脚本队列供。
struct FakeResolver(std::sync::Mutex<FakeEndpointState>);

enum FakeEndpointState {
    Absent,
    Endpoint(multi_agents_manager_lib::services::codex_desktop_ipc::transport::DesktopEndpoint),
}

impl FakeResolver {
    fn absent() -> Self {
        Self(std::sync::Mutex::new(FakeEndpointState::Absent))
    }
    fn set_endpoint(
        &self,
        ep: multi_agents_manager_lib::services::codex_desktop_ipc::transport::DesktopEndpoint,
    ) {
        *self.0.lock().expect("lock") = FakeEndpointState::Endpoint(ep);
    }
    /// F4（2026-10-17 复审）：恢复轮失败注入（Desktop 缺席语义）。
    fn set_absent(&self) {
        *self.0.lock().expect("lock") = FakeEndpointState::Absent;
    }
}

impl EndpointResolver for FakeResolver {
    fn resolve(
        &self,
    ) -> Result<
        multi_agents_manager_lib::services::codex_desktop_ipc::transport::DesktopEndpoint,
        ProtocolError,
    > {
        match &*self.0.lock().expect("lock") {
            FakeEndpointState::Absent => Err(ProtocolError::Disconnected(
                "desktop endpoint absent".into(),
            )),
            FakeEndpointState::Endpoint(ep) => Ok(ep.clone()),
        }
    }
}

/// 脚本连接器：每次连接 = resolver.resolve() + 弹出一个预排队的 io（R3 测试 6 用）。
struct ScriptedConnector {
    resolver: std::sync::Arc<FakeResolver>,
    ios: StdMutex<std::collections::VecDeque<BoxIo>>,
}

impl Connector for ScriptedConnector {
    fn connect_endpoint(
        &self,
    ) -> Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<(BoxIo, multi_agents_manager_lib::services::codex_desktop_ipc::transport::DesktopEndpoint), ProtocolError>,
                > + Send
                + '_,
        >,
    >{
        Box::pin(async move {
            let endpoint = self.resolver.resolve()?;
            let io = self
                .ios
                .lock()
                .expect("lock")
                .pop_front()
                .ok_or_else(|| ProtocolError::Disconnected("no scripted io".into()))?;
            Ok((io, endpoint))
        })
    }
}

#[tokio::test]
async fn revision_gap_republishes_stale_and_refollows() {
    let (io, mut peer) = ScriptedPeer::new();
    let adapter = DesktopIpcAdapter::new(
        std::sync::Arc::new(TestConnector::new(test_endpoint("macos", MACOS_BUILD), io)),
        profile_for("macos", MACOS_BUILD).unwrap(),
    );
    let key = tkey("gap-thread");
    let mut sse = adapter.subscribe();
    let task = {
        let a = adapter.clone();
        let k = key.clone();
        tokio::spawn(async move { a.ensure_thread(&k).await })
    };
    drive_follow(
        &mut peer,
        "gap-thread",
        "owner-1",
        snapshot_change(209, vec![permissions_request()], true),
        true,
    )
    .await;
    let proj = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("ensure in time")
        .expect("join")
        .expect("fresh");
    assert!(!proj.stale);
    assert_eq!(proj.pending.len(), 1);

    // 故意 revision 缺口（当前 209；帧声称 base=210/rev=211——中间缺 210，乱序缺口）
    let gap = serde_json::json!({ "type": "patches", "baseRevision": 210, "revision": 211, "patches": [] });
    peer.send(&state_frame("gap-thread", "owner-1", gap)).await;
    // SSE 收到 stale 投影（不能停在旧视图）
    let stale = wait_projection(&mut sse, "gap-thread", true, 5).await;
    assert!(stale.stale);

    // 有界重拉：re-declare following（probe 定案：再声明即得新 snapshot）
    let follow = recv_frame(&mut peer, 5).await;
    assert_eq!(follow["method"], "thread-stream-following-changed");
    assert_eq!(follow["params"]["following"], serde_json::json!(true));
    assert_eq!(follow["params"]["conversationId"], "gap-thread");

    // 新 snapshot 恢复，无重复气泡
    peer.send(&state_frame(
        "gap-thread",
        "owner-1",
        snapshot_change(210, vec![permissions_request()], true),
    ))
    .await;
    let fresh = wait_projection(&mut sse, "gap-thread", false, 5).await;
    assert!(!fresh.stale);
    assert_eq!(fresh.revision, Some(210));
    assert_eq!(
        fresh.messages.len(),
        proj.messages.len(),
        "新 snapshot 恢复后不得产生重复气泡"
    );

    // 恢复成功即停止重试：不再有 following 帧
    assert_no_frame(&mut peer, 900).await;
    adapter.shutdown().await;
}

#[tokio::test]
async fn history_incomplete_triggers_read_only_load() {
    let (io, mut peer) = ScriptedPeer::new();
    let adapter = DesktopIpcAdapter::new(
        std::sync::Arc::new(TestConnector::new(test_endpoint("macos", MACOS_BUILD), io)),
        profile_for("macos", MACOS_BUILD).unwrap(),
    );
    let key = tkey("hist-thread");
    let task = {
        let a = adapter.clone();
        let k = key.clone();
        tokio::spawn(async move { a.ensure_thread(&k).await })
    };
    drive_follow(
        &mut peer,
        "hist-thread",
        "owner-1",
        snapshot_change(100, vec![], false),
        true,
    )
    .await;
    let proj = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("ensure in time")
        .expect("join")
        .expect("fresh");
    assert!(!proj.stale);

    // 快照不完整 → v1 只读 load-complete-history（params 带 conversationId + hostId:"local"）
    let load = recv_frame(&mut peer, 5).await;
    assert_eq!(load["type"], "request");
    assert_eq!(load["method"], "thread-follower-load-complete-history");
    assert_eq!(load["version"], serde_json::json!(1));
    assert_eq!(load["params"]["conversationId"], "hist-thread");
    assert_eq!(load["params"]["hostId"], "local");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": load["requestId"],
        "resultType": "success", "method": "thread-follower-load-complete-history",
        "result": {}
    }))
    .await;

    // 每 follow 至多一次：后续仍不完整的 snapshot 不再发起
    peer.send(&state_frame(
        "hist-thread",
        "owner-1",
        snapshot_change(101, vec![], false),
    ))
    .await;
    let _ = wait_revision(&adapter, &key, 101).await;
    assert_no_frame(&mut peer, 900).await;

    // 完整 snapshot 后同样不再发起
    peer.send(&state_frame(
        "hist-thread",
        "owner-1",
        snapshot_change(102, vec![], true),
    ))
    .await;
    let _ = wait_revision(&adapter, &key, 102).await;
    assert_no_frame(&mut peer, 900).await;
    adapter.shutdown().await;
}

#[tokio::test]
async fn client_status_changed_invalidates_all_threads_of_owner() {
    let (io, mut peer) = ScriptedPeer::new();
    let adapter = DesktopIpcAdapter::new(
        std::sync::Arc::new(TestConnector::new(test_endpoint("macos", MACOS_BUILD), io)),
        profile_for("macos", MACOS_BUILD).unwrap(),
    );
    let mut sse = adapter.subscribe();
    let k1 = tkey("own-t1");
    let k2 = tkey("own-t2");
    // 两 thread 共 owner-1
    let t1 = {
        let a = adapter.clone();
        let k = k1.clone();
        tokio::spawn(async move { a.ensure_thread(&k).await })
    };
    drive_follow(
        &mut peer,
        "own-t1",
        "owner-1",
        snapshot_change(1, vec![permissions_request()], true),
        true,
    )
    .await;
    let p1 = tokio::time::timeout(Duration::from_secs(10), t1)
        .await
        .expect("in time")
        .expect("join")
        .expect("fresh");
    let t2 = {
        let a = adapter.clone();
        let k = k2.clone();
        tokio::spawn(async move { a.ensure_thread(&k).await })
    };
    drive_follow(
        &mut peer,
        "own-t2",
        "owner-1",
        snapshot_change(2, vec![], true),
        false,
    )
    .await;
    let _p2 = tokio::time::timeout(Duration::from_secs(10), t2)
        .await
        .expect("in time")
        .expect("join")
        .expect("fresh");

    // owner 广播 disconnected
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "client-status-changed", "version": 0,
        "sourceClientId": "owner-1",
        "params": { "clientId": "owner-1", "status": "disconnected" }
    }))
    .await;

    // SSE 收到两 thread 的 stale 投影（收集式：帧序随机，不逐个过滤吞帧）
    let got = wait_projections_multi(&mut sse, &["own-t1", "own-t2"], true, 5).await;
    assert!(got["own-t1"].stale);
    assert!(got["own-t2"].stale);

    // 旧卡 respond 零写（旧 lease/token 全失效）
    let rk = p1.pending[0].request_key.clone();
    let tk = p1.pending[0].context_token.clone();
    let o = adapter.respond(&k1, &rk, &tk, PendingAction::Approve).await;
    assert_eq!(o.delivery, Delivery::NotSent);
    assert_eq!(o.reason_code.as_deref(), Some("stale_context"));
    // C2：owner 失效但 socket 仍活着 → 自动重跟随两 thread（发现顺序不固定，按帧分拣）
    for _ in 0..2 {
        let disc = recv_frame(&mut peer, 5).await;
        assert_eq!(disc["method"], "thread-owner-discovery", "C2 自动重发现");
        let tid = disc["params"]["conversationId"]
            .as_str()
            .unwrap()
            .to_string();
        peer.send(&serde_json::json!({
            "type": "response", "requestId": disc["requestId"],
            "resultType": "success", "method": "thread-owner-discovery",
            "handledByClientId": "owner-1",
            "result": { "supportsUntrustedAppInput": true }
        }))
        .await;
        let follow = recv_frame(&mut peer, 5).await;
        assert_eq!(follow["method"], "thread-stream-following-changed");
        assert_eq!(follow["params"]["conversationId"], serde_json::json!(tid));
        // 新世界 snapshot（换代重理 token；失效卡不复活）
        peer.send(&state_frame(
            &tid,
            "owner-1",
            snapshot_change(50, vec![], true),
        ))
        .await;
    }
    assert_no_frame(&mut peer, 300).await;
    adapter.shutdown().await;
}

#[tokio::test]
async fn late_frame_from_old_owner_is_dropped() {
    let (io, mut peer) = ScriptedPeer::new();
    let adapter = DesktopIpcAdapter::new(
        std::sync::Arc::new(TestConnector::new(test_endpoint("macos", MACOS_BUILD), io)),
        profile_for("macos", MACOS_BUILD).unwrap(),
    );
    let mut sse = adapter.subscribe();
    let key = tkey("late-thread");
    let task = {
        let a = adapter.clone();
        let k = key.clone();
        tokio::spawn(async move { a.ensure_thread(&k).await })
    };
    drive_follow(
        &mut peer,
        "late-thread",
        "owner-1",
        snapshot_change(5, vec![], true),
        true,
    )
    .await;
    let _ = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("in time")
        .expect("join")
        .expect("fresh");

    // owner A → B：disconnected + 重新 ensure（新 owner 接管）
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "client-status-changed", "version": 0,
        "sourceClientId": "owner-1",
        "params": { "clientId": "owner-1", "status": "disconnected" }
    }))
    .await;
    let _stale = wait_projection(&mut sse, "late-thread", true, 5).await;
    // C2：owner A→B 接管走**自动重跟随**（生产路径；不再手工 ensure）
    let disc = recv_frame(&mut peer, 5).await;
    assert_eq!(disc["method"], "thread-owner-discovery");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": disc["requestId"],
        "resultType": "success", "method": "thread-owner-discovery",
        "handledByClientId": "owner-2",
        "result": { "supportsUntrustedAppInput": true }
    }))
    .await;
    let follow = recv_frame(&mut peer, 5).await;
    assert_eq!(follow["method"], "thread-stream-following-changed");
    peer.send(&state_frame(
        "late-thread",
        "owner-2",
        snapshot_change(6, vec![], true),
    ))
    .await;
    let now = wait_revision(&adapter, &key, 6).await;
    assert_eq!(now.revision, Some(6));

    // 旧 owner 的迟到帧（rev 7）：必须丢弃——镜像 revision/内容不变
    peer.send(&state_frame(
        "late-thread",
        "owner-1",
        snapshot_change(7, vec![], true),
    ))
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let now = adapter.projection(&key).await;
    assert_eq!(now.revision, Some(6), "迟到帧不得推进 revision/污染镜像");
    assert_eq!(now.messages[0].text, "hello", "内容不被迟到帧覆盖");

    // 现行 owner 的同一帧：正常应用
    peer.send(&state_frame(
        "late-thread",
        "owner-2",
        snapshot_change(7, vec![], true),
    ))
    .await;
    let p7 = wait_revision(&adapter, &key, 7).await;
    assert_eq!(p7.revision, Some(7));
    assert_eq!(
        p7.messages[0].text, "hello",
        "同一帧换现行 owner 来源后生效"
    );
    adapter.shutdown().await;
}

#[tokio::test]
async fn unknown_build_closes_and_verified_reopens() {
    // resolver 链：无 Desktop → verified build → unknown build → verified；
    // 每步断言能力动态变化与零/有 IPC 写（不注入固定 profile）。
    let resolver = std::sync::Arc::new(FakeResolver::absent());
    let (io_b, mut peer_b) = ScriptedPeer::new();
    let (io_c, mut peer_c) = ScriptedPeer::new();
    let (io_d, mut peer_d) = ScriptedPeer::new();
    let adapter = DesktopIpcAdapter::new(
        std::sync::Arc::new(ScriptedConnector {
            resolver: resolver.clone(),
            ios: StdMutex::new([io_b, io_c, io_d].into_iter().collect()),
        }),
        multi_agents_manager_lib::services::codex_desktop_ipc::profile::ProtocolProfile::closed(),
    );
    let key = tkey("dyn-thread");

    // Step A：无 Desktop（resolver 失败）→ closed 投影 + start_turn 零写
    {
        let a = adapter.clone();
        let k = key.clone();
        let joined = tokio::time::timeout(Duration::from_secs(10), a.ensure_thread(&k))
            .await
            .expect("ensure returns in time");
        assert!(joined.is_err(), "无 endpoint 时 ensure 失败");
    }
    assert!(
        !adapter.capabilities().send,
        "无连接/无 endpoint → closed 能力"
    );
    let o = adapter.start_turn(&key, "act-a", "x", "cmid-a").await;
    assert_eq!(o.delivery, Delivery::NotSent);
    assert_eq!(o.reason_code.as_deref(), Some("capability_unavailable"));

    // Step B：verified build → 能力开、可写
    resolver.set_endpoint(test_endpoint("macos", MACOS_BUILD));
    let t = {
        let a = adapter.clone();
        let k = key.clone();
        tokio::spawn(async move { a.ensure_thread(&k).await })
    };
    drive_follow(
        &mut peer_b,
        "dyn-thread",
        "owner-1",
        snapshot_change(1, vec![], true),
        true,
    )
    .await;
    let _ = tokio::time::timeout(Duration::from_secs(10), t)
        .await
        .expect("in time")
        .expect("join")
        .expect("fresh");
    assert!(adapter.capabilities().send, "verified build → 能力开");
    let wt = {
        let a = adapter.clone();
        let k = key.clone();
        tokio::spawn(async move { a.start_turn(&k, "act-b", "帮我", "cmid-b").await })
    };
    let frame = recv_frame(&mut peer_b, 5).await;
    assert_eq!(frame["method"], "thread-follower-start-turn");
    peer_b
        .send(&serde_json::json!({
            "type": "response", "requestId": frame["requestId"],
            "resultType": "success", "method": "thread-follower-start-turn",
            "result": { "turn": { "id": "turn-1", "status": "inProgress" } }
        }))
        .await;
    let o = tokio::time::timeout(Duration::from_secs(5), wt)
        .await
        .expect("in time")
        .expect("join");
    assert_eq!(o.delivery, Delivery::Accepted);

    // Step C：unknown build（先切 resolver 再断开）→ 重连后立即关 A 写、start_turn 零写
    resolver.set_endpoint(test_endpoint("macos", "9.9.9-unknown"));
    peer_b.close();
    drive_follow(
        &mut peer_c,
        "dyn-thread",
        "owner-2",
        snapshot_change(2, vec![], true),
        true,
    )
    .await;
    let _ = wait_revision(&adapter, &key, 2).await;
    assert!(!adapter.capabilities().send, "unknown build → 能力立即关闭");
    let o = adapter.start_turn(&key, "act-c", "x", "cmid-c").await;
    assert_eq!(o.delivery, Delivery::NotSent);
    assert_eq!(o.reason_code.as_deref(), Some("capability_unavailable"));
    assert_no_frame(&mut peer_c, 500).await;

    // Step D：verified 重开（无需重启 Tuvis）
    resolver.set_endpoint(test_endpoint("macos", MACOS_BUILD));
    peer_c.close();
    drive_follow(
        &mut peer_d,
        "dyn-thread",
        "owner-3",
        snapshot_change(3, vec![], true),
        true,
    )
    .await;
    let _ = wait_revision(&adapter, &key, 3).await;
    assert!(
        adapter.capabilities().send,
        "verified build 恢复 → 能力重开"
    );
    let wt = {
        let a = adapter.clone();
        let k = key.clone();
        tokio::spawn(async move { a.start_turn(&k, "act-d", "帮我", "cmid-d").await })
    };
    let frame = recv_frame(&mut peer_d, 5).await;
    assert_eq!(frame["method"], "thread-follower-start-turn");
    peer_d
        .send(&serde_json::json!({
            "type": "response", "requestId": frame["requestId"],
            "resultType": "success", "method": "thread-follower-start-turn",
            "result": { "turn": { "id": "turn-2", "status": "inProgress" } }
        }))
        .await;
    let o = tokio::time::timeout(Duration::from_secs(5), wt)
        .await
        .expect("in time")
        .expect("join");
    assert_eq!(o.delivery, Delivery::Accepted);
    adapter.shutdown().await;
}

#[tokio::test]
async fn follow_and_respond_do_not_deadlock() {
    let (adapter, mut peer, projection, key1) = harness(vec![permissions_request()]).await;
    let mut sse = adapter.subscribe();
    let key2 = tkey("deadlock-t2");
    let rk = projection.pending[0].request_key.clone();
    let tk = projection.pending[0].context_token.clone();
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));

    // 两条路径同时出发：ensure_thread(t2) 与 respond(t1)（R5 反向锁序死锁回归）
    let t_follow = {
        let a = adapter.clone();
        let k = key2.clone();
        let b = barrier.clone();
        tokio::spawn(async move {
            b.wait().await;
            a.ensure_thread(&k).await
        })
    };
    let t_respond = {
        let a = adapter.clone();
        let k = key1.clone();
        let rk = rk.clone();
        let tk = tk.clone();
        let b = barrier.clone();
        tokio::spawn(async move {
            b.wait().await;
            a.respond(&k, &rk, &tk, PendingAction::Approve).await
        })
    };
    // 对端按 method 分拣驱动（两任务的帧任意交错）
    let others = drive_follow_collecting(
        &mut peer,
        "deadlock-t2",
        "owner-1",
        snapshot_change(300, vec![], true),
        false,
    )
    .await;
    // respond 帧（可能与其他帧交错到达）逐一回执
    for frame in others {
        peer.send(&serde_json::json!({
            "type": "response", "requestId": frame["requestId"],
            "resultType": "success", "method": frame["method"],
            "result": { "ok": true }
        }))
        .await;
    }
    let p = tokio::time::timeout(Duration::from_secs(5), t_follow)
        .await
        .expect("follow 必须限时完成（不得互等死锁）")
        .expect("join")
        .expect("fresh");
    assert!(!p.stale);
    let o = tokio::time::timeout(Duration::from_secs(5), t_respond)
        .await
        .expect("respond 必须限时完成（不得互等死锁）")
        .expect("join");
    assert_eq!(o.delivery, Delivery::Accepted);

    // lease/token 最终复核仍在：owner 失效 → C2 自动重跟随两 thread（顺序不按序分拣）
    // → 换代后旧 token 作废，旧卡 respond 零写
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "client-status-changed", "version": 0,
        "sourceClientId": "owner-1",
        "params": { "clientId": "owner-1", "status": "disconnected" }
    }))
    .await;
    let stale = wait_projection(&mut sse, &key1.thread_id, true, 5).await;
    assert!(stale.stale);
    for _ in 0..2 {
        let disc = recv_frame(&mut peer, 5).await;
        assert_eq!(disc["method"], "thread-owner-discovery");
        let tid = disc["params"]["conversationId"]
            .as_str()
            .unwrap()
            .to_string();
        peer.send(&serde_json::json!({
            "type": "response", "requestId": disc["requestId"],
            "resultType": "success", "method": "thread-owner-discovery",
            "handledByClientId": "owner-1",
            "result": { "supportsUntrustedAppInput": true }
        }))
        .await;
        let follow = recv_frame(&mut peer, 5).await;
        assert_eq!(follow["method"], "thread-stream-following-changed");
        // key1 的世界带原 pending（新 token）；key2 空世界——按 tid 区分
        let change = if tid == key1.thread_id {
            snapshot_change(301, vec![permissions_request()], true)
        } else {
            snapshot_change(301, vec![], true)
        };
        peer.send(&state_frame(&tid, "owner-1", change)).await;
    }
    let o2 = adapter
        .respond(&key1, &rk, &tk, PendingAction::Approve)
        .await;
    assert_eq!(o2.delivery, Delivery::NotSent);
    assert_eq!(
        o2.reason_code.as_deref(),
        Some("stale_context"),
        "换代后旧 token 作废"
    );
    assert_no_frame(&mut peer, 300).await;
    adapter.shutdown().await;
}

#[tokio::test]
async fn vanished_pending_revokes_token() {
    let (adapter, mut peer, projection, key) = harness(vec![permissions_request()]).await;
    let rk = projection.pending[0].request_key.clone();
    let old_token = projection.pending[0].context_token.clone();
    // [r] → []
    peer.send(&state_frame(
        "00000000-0000-4000-8000-0000000000m1",
        "owner-1",
        serde_json::json!({ "type": "patches", "baseRevision": 209, "revision": 210,
            "patches": [ { "op": "replace", "path": ["requests"], "value": [] } ] }),
    ))
    .await;
    // [] → [r]（重现）
    peer.send(&state_frame(
        "00000000-0000-4000-8000-0000000000m1",
        "owner-1",
        serde_json::json!({ "type": "patches", "baseRevision": 210, "revision": 211,
            "patches": [ { "op": "replace", "path": ["requests"], "value": [permissions_request()] } ] }),
    ))
    .await;
    let now = wait_revision(&adapter, &key, 211).await;
    assert_eq!(now.pending.len(), 1);
    let new_token = now.pending[0].context_token.clone();
    assert_ne!(new_token, old_token, "重现后必须换新 token");

    // 旧 token respond → 409/stale_context 零写
    let o = adapter
        .respond(&key, &rk, &old_token, PendingAction::Approve)
        .await;
    assert_eq!(o.delivery, Delivery::NotSent);
    assert_eq!(o.reason_code.as_deref(), Some("stale_context"));
    assert_no_frame(&mut peer, 300).await;

    // 新 token 可用
    let t = {
        let a = adapter.clone();
        let k = key.clone();
        let rk = rk.clone();
        let tk = new_token.clone();
        tokio::spawn(async move { a.respond(&k, &rk, &tk, PendingAction::Approve).await })
    };
    let frame = recv_frame(&mut peer, 5).await;
    assert_eq!(
        frame["method"],
        "thread-follower-permissions-request-approval-response"
    );
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-permissions-request-approval-response",
        "result": { "ok": true }
    }))
    .await;
    let o = tokio::time::timeout(Duration::from_secs(5), t)
        .await
        .expect("in time")
        .expect("join");
    assert_eq!(o.delivery, Delivery::Accepted);
    adapter.shutdown().await;
}

#[tokio::test]
async fn stale_mirror_rejects_respond() {
    let (adapter, mut peer, projection, key) = harness(vec![permissions_request()]).await;
    let rk = projection.pending[0].request_key.clone();
    let tk = projection.pending[0].context_token.clone();
    // revision 缺口（当前 209；帧声称 base=210/rev=211——缺口不应用，mirror stale）
    peer.send(&state_frame(
        "00000000-0000-4000-8000-0000000000m1",
        "owner-1",
        serde_json::json!({ "type": "patches", "baseRevision": 210, "revision": 211, "patches": [] }),
    ))
    .await;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let p = adapter.projection(&key).await;
        if p.stale {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "缺口后应 stale");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // 缺口后旧卡批准 → stale_context、零写（following 声明属 R1 有界重拉，允许）
    let o = adapter
        .respond(&key, &rk, &tk, PendingAction::Approve)
        .await;
    assert_eq!(o.delivery, Delivery::NotSent);
    assert_eq!(o.reason_code.as_deref(), Some("stale_context"));
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    loop {
        assert!(
            std::time::Instant::now() < deadline,
            "不得有 mutating 帧写出"
        );
        match tokio::time::timeout(Duration::from_millis(200), peer.recv()).await {
            Err(_) => break,
            Ok(frame) => assert!(
                frame["method"]
                    .as_str()
                    .unwrap_or("")
                    .starts_with("thread-stream"),
                "缺口后不得有交互写帧：{frame}"
            ),
        }
    }
    adapter.shutdown().await;
}

#[tokio::test]
async fn concurrent_submit_same_pending_writes_once() {
    let (adapter, mut peer, projection, key) = harness(vec![permissions_request()]).await;
    let rk = projection.pending[0].request_key.clone();
    let tk = projection.pending[0].context_token.clone();
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    // 两个不同 actionId（两次 respond）并发提交同一 pending
    let ta = {
        let a = adapter.clone();
        let k = key.clone();
        let rk = rk.clone();
        let tk = tk.clone();
        let b = barrier.clone();
        tokio::spawn(async move {
            b.wait().await;
            a.respond(&k, &rk, &tk, PendingAction::Approve).await
        })
    };
    let tb = {
        let a = adapter.clone();
        let k = key.clone();
        let rk = rk.clone();
        let tk = tk.clone();
        let b = barrier.clone();
        tokio::spawn(async move {
            b.wait().await;
            a.respond(&k, &rk, &tk, PendingAction::Approve).await
        })
    };
    // 恰一个 wire 帧
    let frame = recv_frame(&mut peer, 5).await;
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": frame["method"],
        "result": { "ok": true }
    }))
    .await;
    let oa = tokio::time::timeout(Duration::from_secs(5), ta)
        .await
        .expect("in time")
        .expect("join");
    let ob = tokio::time::timeout(Duration::from_secs(5), tb)
        .await
        .expect("in time")
        .expect("join");
    let outcomes = [oa, ob];
    let accepted = outcomes
        .iter()
        .filter(|o| o.delivery == Delivery::Accepted)
        .count();
    let blocked = outcomes
        .iter()
        .filter(|o| {
            o.delivery == Delivery::NotSent
                && o.reason_code.as_deref() == Some("pending_submit_in_progress")
        })
        .count();
    assert_eq!(accepted, 1, "并发提交恰一次写");
    assert_eq!(blocked, 1, "另一提交被 pending_submit_in_progress 拒绝");
    assert_no_frame(&mut peer, 300).await;
    adapter.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn unknown_delivery_blocks_retry_same_pending() {
    let (adapter, mut peer, projection, key) = harness(vec![permissions_request()]).await;
    let rk = projection.pending[0].request_key.clone();
    let tk = projection.pending[0].context_token.clone();
    // 第一次提交：帧到达后对端不回执 → 虚拟时间推进到 RPC 超时 = UnknownDelivery
    let t1 = {
        let a = adapter.clone();
        let k = key.clone();
        let rk = rk.clone();
        let tk = tk.clone();
        tokio::spawn(async move { a.respond(&k, &rk, &tk, PendingAction::Approve).await })
    };
    let frame = recv_frame(&mut peer, 5).await;
    let _ = frame;
    let o1 = tokio::time::timeout(RPC_TIMEOUT + Duration::from_secs(5), t1)
        .await
        .expect("timeout fires")
        .expect("join");
    assert_eq!(o1.delivery, Delivery::UnknownDelivery);

    // 新 actionId 再 respond 同一 pending → 拒（未知结果不能借新 actionId 重发）
    let o2 = adapter
        .respond(&key, &rk, &tk, PendingAction::Approve)
        .await;
    assert_eq!(o2.delivery, Delivery::NotSent);
    assert_eq!(
        o2.reason_code.as_deref(),
        Some("pending_submit_in_progress")
    );
    assert_no_frame(&mut peer, 300).await;

    // pending 消失：**状态应用驱动清理屏障**（2026-10-17 复审 C3——期间没有任何
    // respond 调用，旧实现靠这里的额外 respond 替生产清屏障，遮住常态）
    peer.send(&state_frame(
        "00000000-0000-4000-8000-0000000000m1",
        "owner-1",
        serde_json::json!({ "type": "patches", "baseRevision": 209, "revision": 210,
            "patches": [ { "op": "replace", "path": ["requests"], "value": [] } ] }),
    ))
    .await;
    let _ = wait_revision(&adapter, &key, 210).await;

    // [r] 重现（新 token）：旧 Unknown 不挡新卡——新 token 可提交、旧 token 仍拒
    //（证明屏障按完整上下文分代解除，而非永久卡死）
    peer.send(&state_frame(
        "00000000-0000-4000-8000-0000000000m1",
        "owner-1",
        serde_json::json!({ "type": "patches", "baseRevision": 210, "revision": 211,
            "patches": [ { "op": "replace", "path": ["requests"], "value": [permissions_request()] } ] }),
    ))
    .await;
    let now = wait_revision(&adapter, &key, 211).await;
    let fresh_token = now.pending[0].context_token.clone();
    assert_ne!(fresh_token, tk, "重现必须换 token（新世界）");
    let o_old = adapter
        .respond(&key, &rk, &tk, PendingAction::Approve)
        .await;
    assert_eq!(o_old.delivery, Delivery::NotSent);
    assert_eq!(
        o_old.reason_code.as_deref(),
        Some("stale_context"),
        "旧 token 仍被拒（换代上下文不作废提交语义）"
    );
    assert_no_frame(&mut peer, 300).await;
    let t4 = {
        let a = adapter.clone();
        let k = key.clone();
        let rk = rk.clone();
        tokio::spawn(async move {
            a.respond(&k, &rk, &fresh_token, PendingAction::Approve)
                .await
        })
    };
    let frame = recv_frame(&mut peer, 5).await;
    assert_eq!(
        frame["method"],
        "thread-follower-permissions-request-approval-response"
    );
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-permissions-request-approval-response",
        "result": { "ok": true }
    }))
    .await;
    let o4 = tokio::time::timeout(Duration::from_secs(5), t4)
        .await
        .expect("in time")
        .expect("join");
    assert_eq!(o4.delivery, Delivery::Accepted);
    adapter.shutdown().await;
}

/// 2026-10-17 复审 C2/F4：断线恢复闭环——peer 掉线 → stale 发布 → 恢复循环第一
/// 轮失败（Desktop 缺席）→ 退避后第二轮自动重连 + 重跟随（无 GET/无手工 ensure），
/// 新 snapshot 落进镜像、视图自动复活。
#[tokio::test]
async fn recovery_loop_reconnects_with_backoff_after_peer_drop() {
    let resolver = std::sync::Arc::new(FakeResolver::absent());
    let (io_a, mut peer_a) = ScriptedPeer::new();
    let (io_b, mut peer_b) = ScriptedPeer::new();
    let adapter = DesktopIpcAdapter::new(
        std::sync::Arc::new(ScriptedConnector {
            resolver: resolver.clone(),
            ios: StdMutex::new([io_a, io_b].into_iter().collect()),
        }),
        multi_agents_manager_lib::services::codex_desktop_ipc::profile::ProtocolProfile::closed(),
    );
    let key = tkey("rec-thread");
    let mut sse = adapter.subscribe();
    // 首连成功（io_a）
    resolver.set_endpoint(test_endpoint("macos", MACOS_BUILD));
    let t = {
        let a = adapter.clone();
        let k = key.clone();
        tokio::spawn(async move { a.ensure_thread(&k).await })
    };
    drive_follow(
        &mut peer_a,
        "rec-thread",
        "owner-1",
        snapshot_change(1, vec![], true),
        true,
    )
    .await;
    let _ = tokio::time::timeout(Duration::from_secs(10), t)
        .await
        .expect("in time")
        .expect("join")
        .expect("fresh");

    // 掐断对端 → stale 发布 → 恢复循环启动；第一轮注入失败（Desktop 缺席）
    resolver.set_absent();
    peer_a.close();
    let _ = wait_projection(&mut sse, "rec-thread", true, 5).await;

    // 退避窗口内恢复 endpoint → 第二轮自动重连（io_b）+ 重跟随（无 GET）
    resolver.set_endpoint(test_endpoint("macos", MACOS_BUILD));
    // 新连接先握手（initialize），随后才是恢复性的 discovery/following
    {
        let init = recv_frame(&mut peer_b, 15).await;
        assert_eq!(init["method"], "initialize");
        peer_b
            .send(&serde_json::json!({
                "type": "response", "requestId": init["requestId"],
                "resultType": "success", "method": "initialize",
                "handledByClientId": "owner-1", "result": { "clientId": "owner-1" }
            }))
            .await;
    }
    let disc = recv_frame(&mut peer_b, 15).await;
    assert_eq!(
        disc["method"], "thread-owner-discovery",
        "恢复循环必须自动重发现（无 GET/无手工 ensure）"
    );
    reply_discovery(&mut peer_b, &disc, "owner-1").await;
    let follow = recv_frame(&mut peer_b, 5).await;
    assert_eq!(follow["method"], "thread-stream-following-changed");
    peer_b
        .send(&state_frame(
            "rec-thread",
            "owner-1",
            snapshot_change(2, vec![], true),
        ))
        .await;
    // 新 snapshot 落进镜像：视图自动复活（revision 前进到 2）
    let now = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let p = adapter.projection(&key).await;
            if !p.stale && p.revision == Some(2) {
                return p;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("恢复后视图必须自动复活");
    assert_eq!(now.runtime_status, "idle");
    adapter.shutdown().await;
}

/// discovery 帧的应答（C4 测试用）。
async fn reply_discovery(peer: &mut ScriptedPeer, disc: &serde_json::Value, owner: &str) {
    peer.send(&serde_json::json!({
        "type": "response", "requestId": disc["requestId"],
        "resultType": "success", "method": "thread-owner-discovery",
        "handledByClientId": owner,
        "result": { "supportsUntrustedAppInput": true }
    }))
    .await;
}

/// 2026-10-17 复审 C4：unfollow = 真正退场——following:false 必达、owner lease
/// 回收（卸载后的 owner 帧不复活镜像）、幂等重入零副作用。
#[tokio::test]
async fn unfollow_sends_false_releases_lease_and_frames_do_not_revive() {
    let (adapter, mut peer, _projection, key) = harness(vec![permissions_request()]).await;
    adapter.unfollow(&key).await;
    let off = recv_frame(&mut peer, 5).await;
    assert_eq!(off["method"], "thread-stream-following-changed");
    assert_eq!(off["params"]["following"], serde_json::json!(false));
    assert_eq!(
        off["params"]["conversationId"],
        serde_json::json!(key.thread_id)
    );

    // 卸载后的 owner 帧：无 lease 可套、无镜像可落 → 不复活
    peer.send(&state_frame(
        &key.thread_id,
        "owner-1",
        snapshot_change(999, vec![permissions_request()], true),
    ))
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let p = adapter.projection(&key).await;
    assert!(p.stale && p.revision.is_none(), "卸载后帧不复活镜像");

    // 幂等：再次 unfollow 零副作用（不再发 following:false）
    adapter.unfollow(&key).await;
    assert_no_frame(&mut peer, 300).await;
    adapter.shutdown().await;
}

/// C4：连续 65 次挂载/卸载——逐次 following:false、名额逐次释放，第 65 次照常
/// 完整 follow（名额不因历史挂载耗尽）。
#[tokio::test]
async fn mount_unmount_cycles_never_exhaust_following() {
    let (adapter, mut peer, _p, _k) = harness(vec![]).await;
    for i in 0..65u32 {
        let key = tkey(&format!("cycle-{i}"));
        let a = adapter.clone();
        let k = key.clone();
        let task = tokio::spawn(async move { a.ensure_thread(&k).await });
        let disc = recv_frame(&mut peer, 5).await;
        assert_eq!(disc["method"], "thread-owner-discovery", "第 {i} 次挂载");
        reply_discovery(&mut peer, &disc, "owner-1").await;
        let follow = recv_frame(&mut peer, 5).await;
        assert_eq!(follow["method"], "thread-stream-following-changed");
        peer.send(&state_frame(
            &key.thread_id,
            "owner-1",
            snapshot_change(1, vec![], true),
        ))
        .await;
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("in time")
            .expect("join")
            .expect("fresh");
        // 卸载：following:false 必达
        adapter.unfollow(&key).await;
        let off = recv_frame(&mut peer, 5).await;
        assert_eq!(off["method"], "thread-stream-following-changed");
        assert_eq!(off["params"]["following"], serde_json::json!(false));
    }
    adapter.shutdown().await;
}

/// 2026-10-17 复审 C3：respond 两阶段之间的 owner 换代——旧 A 的镜像载荷不得套
/// 新 B 的有效 lease 写出。测试缝固定「阶段 1 读镜像 → owner 失效/接管 → 阶段 3
/// 取 lease」的交错；阶段 3 同代复核（现行 lease ≠ 阶段 1 镜像 lease）→ 零写入。
#[tokio::test]
async fn respond_rejects_payload_when_owner_changes_between_phases() {
    let (adapter, mut peer, projection, key) = harness(vec![permissions_request()]).await;
    let rk = projection.pending[0].request_key.clone();
    let tk = projection.pending[0].context_token.clone();
    // 注入两阶段等待门；等 respond 抢到提交权（已过阶段 2、停在缝上）
    let gate = std::sync::Arc::new(tokio::sync::Notify::new());
    adapter.set_respond_interphase_gate(gate.clone());
    let t = {
        let a = adapter.clone();
        let k = key.clone();
        let rk = rk.clone();
        let tk = tk.clone();
        tokio::spawn(async move { a.respond(&k, &rk, &tk, PendingAction::Approve).await })
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if adapter.inflight_claimed(&key.thread_id, &rk) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("respond 必须在测试缝处就位");

    // 两阶段间 owner 换代：owner-1 失效 → C2 同 socket 恢复自动重跟随 owner-2
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "client-status-changed", "version": 0,
        "sourceClientId": "owner-1",
        "params": { "clientId": "owner-1", "status": "disconnected" }
    }))
    .await;
    let disc = recv_frame(&mut peer, 5).await;
    assert_eq!(disc["method"], "thread-owner-discovery");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": disc["requestId"],
        "resultType": "success", "method": "thread-owner-discovery",
        "handledByClientId": "owner-2",
        "result": { "supportsUntrustedAppInput": true }
    }))
    .await;
    let follow = recv_frame(&mut peer, 5).await;
    assert_eq!(follow["method"], "thread-stream-following-changed");
    peer.send(&state_frame(
        &key.thread_id,
        "owner-2",
        snapshot_change(300, vec![permissions_request()], true),
    ))
    .await;

    // 放行 respond：阶段 3 现行 lease（owner-2）≠ 阶段 1 镜像 lease（owner-1）
    // → 旧 A 载荷零写入
    gate.notify_one();
    let o = tokio::time::timeout(Duration::from_secs(5), t)
        .await
        .expect("in time")
        .expect("join");
    assert_eq!(o.delivery, Delivery::NotSent);
    assert_eq!(
        o.reason_code.as_deref(),
        Some("stale_context"),
        "跨代载荷必须拒绝（阶段 1 镜像属 owner-1，现行 lease 已是 owner-2）"
    );
    assert_no_frame(&mut peer, 300).await;
    adapter.shutdown().await;
}

#[tokio::test]
async fn following_status_request_gets_reply_from_following_set() {
    let (adapter, mut peer, _projection, key) = harness(vec![]).await;
    // owner 问「谁在跟随」：当前关注集内的 thread 立即回 following:true
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-following-status-requested", "version": 1,
        "sourceClientId": "owner-1",
        "params": { "conversationId": key.thread_id }
    }))
    .await;
    let reply = recv_frame(&mut peer, 5).await;
    assert_eq!(reply["method"], "thread-stream-following-changed");
    assert_eq!(reply["version"], serde_json::json!(1));
    assert_eq!(reply["params"]["following"], serde_json::json!(true));
    assert_eq!(reply["params"]["conversationId"], key.thread_id);
    assert_eq!(reply["params"]["hostId"], "local");

    // 非关注 thread 不回
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-following-status-requested", "version": 1,
        "sourceClientId": "owner-1",
        "params": { "conversationId": "ghost-thread" }
    }))
    .await;
    assert_no_frame(&mut peer, 500).await;
    adapter.shutdown().await;
}

#[tokio::test]
async fn connection_reset_marks_all_stale() {
    let (adapter, mut peer, _projection, key) = harness(vec![]).await;
    let mut sse = adapter.subscribe();
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "ipc-connection-reset", "version": 1,
        "sourceClientId": "owner-1", "params": {}
    }))
    .await;
    let s = wait_projection(&mut sse, &key.thread_id, true, 5).await;
    assert!(s.stale, "连接级 reset → 全部 mirror stale 并发布");
    adapter.shutdown().await;
}

/// 第三轮复审 B3 · 窗口 1（复审原文交错）：discovery **在途**时退出最后引用——
/// 回包后不得 bind lease、不得再次 following:true、无镜像复活；推进重试时间仍
/// 不复活。
#[tokio::test]
async fn unfollow_during_discovery_prevents_resubscribe() {
    let (adapter, mut peer, _projection, key) = harness(vec![]).await;
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "client-status-changed", "version": 0,
        "sourceClientId": "owner-1",
        "params": { "clientId": "owner-1", "status": "disconnected" }
    }))
    .await;
    let disc = recv_frame(&mut peer, 5).await;
    assert_eq!(disc["method"], "thread-owner-discovery");
    // discovery 在途（回包未给）→ 退出最后引用。
    // 此刻 lease 未绑定、尚未向现行世界 declare following——按 C4 噪声条款不发
    // following:false（无「释放回包」可期待）；唯一硬要求：回包后不得 bind/declare
    adapter.unfollow(&key).await;
    // 旧 discovery 回包到达：恢复任务必须取消资格（不 bind、不 declare）
    reply_discovery(&mut peer, &disc, "owner-1").await;
    // 推进重试窗口：期间不得出现 following:true（发现 true 即失败）
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), peer.recv()).await {
            Err(_) => continue,
            Ok(frame) => assert_ne!(
                frame.get("method").and_then(|m| m.as_str()),
                Some("thread-stream-following-changed"),
                "卸载后不得重新订阅：{frame}"
            ),
        }
    }
    let p = adapter.projection(&key).await;
    assert!(p.stale && p.revision.is_none(), "镜像不得复活");
    adapter.shutdown().await;
}

/// 第三轮复审 B3 · 窗口 2：following:true 已声明（恢复任务停在 snapshot 等待）
/// 时退出——释放回包后对端不得再收到 true；snapshot 回包/owner 帧/重试推进都
/// 不复活镜像。
#[tokio::test]
async fn unfollow_after_following_declared_stays_dead() {
    let (adapter, mut peer, _projection, key) = harness(vec![]).await;
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "client-status-changed", "version": 0,
        "sourceClientId": "owner-1",
        "params": { "clientId": "owner-1", "status": "disconnected" }
    }))
    .await;
    let disc = recv_frame(&mut peer, 5).await;
    assert_eq!(disc["method"], "thread-owner-discovery");
    reply_discovery(&mut peer, &disc, "owner-1").await;
    let follow = recv_frame(&mut peer, 5).await;
    assert_eq!(follow["method"], "thread-stream-following-changed");
    assert_eq!(follow["params"]["following"], serde_json::json!(true));

    // 任务停在 snapshot 等待时退出最后引用
    adapter.unfollow(&key).await;
    let off = recv_frame(&mut peer, 5).await;
    assert_eq!(off["method"], "thread-stream-following-changed");
    assert_eq!(off["params"]["following"], serde_json::json!(false));

    // snapshot 回包到达：任务不得再次订阅；镜像保持不存在
    peer.send(&state_frame(
        &key.thread_id,
        "owner-1",
        snapshot_change(2, vec![], true),
    ))
    .await;
    let deadline = std::time::Instant::now() + Duration::from_secs(7);
    while std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(300), peer.recv()).await {
            Err(_) => continue,
            Ok(frame) => assert_ne!(
                frame.get("method").and_then(|m| m.as_str()),
                Some("thread-stream-following-changed"),
                "释放回包后不得再次 following:true（B3）：{frame}"
            ),
        }
    }
    let p = adapter.projection(&key).await;
    assert!(p.stale && p.revision.is_none(), "镜像不得复活");
    // 再推进 + owner 帧也不复活
    tokio::time::sleep(Duration::from_millis(1200)).await;
    peer.send(&state_frame(
        &key.thread_id,
        "owner-1",
        snapshot_change(3, vec![], true),
    ))
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let p = adapter.projection(&key).await;
    assert!(p.stale && p.revision.is_none(), "owner 帧不复活镜像");
    adapter.shutdown().await;
}

/// 第四轮复审 B3 回归 1：bind 与 declare 之间的卸载（gate 缝固定交错）——
/// unfollow 的 following:false 之后的重试窗口内**不得出现迟到 true**；镜像/
/// lease 无残留；旧实现在此直接 declare（true-after-false，红）。
#[tokio::test]
async fn unfollow_between_bind_and_declare_never_sends_late_true() {
    let (adapter, mut peer, _projection, key) = harness(vec![]).await;
    // 注入 bind/declare 间等待门（恢复任务会停在 bind 之后）
    let gate = std::sync::Arc::new(tokio::sync::Notify::new());
    adapter.set_follow_gate(gate.clone());
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "client-status-changed", "version": 0,
        "sourceClientId": "owner-1",
        "params": { "clientId": "owner-1", "status": "disconnected" }
    }))
    .await;
    let disc = recv_frame(&mut peer, 5).await;
    assert_eq!(disc["method"], "thread-owner-discovery");
    reply_discovery(&mut peer, &disc, "owner-1").await;
    // 等任务到达 gate（lease 已 bind、尚未 declare）
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if adapter.follow_gate_reached() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("任务必须到达 bind/declare 间的 gate");
    // bind/declare 间卸载（lease 已 bind → 释放回包 following:false 必发）
    adapter.unfollow(&key).await;
    let off = recv_frame(&mut peer, 5).await;
    assert_eq!(off["method"], "thread-stream-following-changed");
    assert_eq!(off["params"]["following"], serde_json::json!(false));
    // 放行任务：declare 前核验必须拦下——重试窗口内 following 声明不得再有 true
    gate.notify_one();
    let deadline = std::time::Instant::now() + Duration::from_secs(7);
    while std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(300), peer.recv()).await {
            Err(_) => continue,
            Ok(frame) => assert_ne!(
                frame.get("method").and_then(|m| m.as_str()),
                Some("thread-stream-following-changed"),
                "false 之后不得出现迟到 true（B3）：{frame}"
            ),
        }
    }
    let p = adapter.projection(&key).await;
    assert!(p.stale && p.revision.is_none(), "镜像不得复活");
    adapter.shutdown().await;
}

/// 第四轮复审 B3 回归 2：bind/declare 间卸载走错误收尾后——推进重试仍不复活，
/// 卸载后的 owner 帧（无 lease 可套）也不复活。
#[tokio::test]
async fn unfollow_between_bind_and_declare_recovery_retry_stays_dead() {
    let (adapter, mut peer, _projection, key) = harness(vec![]).await;
    let gate = std::sync::Arc::new(tokio::sync::Notify::new());
    adapter.set_follow_gate(gate.clone());
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "client-status-changed", "version": 0,
        "sourceClientId": "owner-1",
        "params": { "clientId": "owner-1", "status": "disconnected" }
    }))
    .await;
    let disc = recv_frame(&mut peer, 5).await;
    assert_eq!(disc["method"], "thread-owner-discovery");
    reply_discovery(&mut peer, &disc, "owner-1").await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if adapter.follow_gate_reached() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("到 gate");
    adapter.unfollow(&key).await;
    let off = recv_frame(&mut peer, 5).await;
    assert_eq!(off["params"]["following"], serde_json::json!(false));
    gate.notify_one();
    // 错误收尾后：重试轮前置核验跳过——推进超过退避/预算窗口仍无任何订阅声明
    tokio::time::sleep(Duration::from_millis(2200)).await;
    tokio::time::timeout(Duration::from_millis(400), peer.recv())
        .await
        .expect_err("错误收尾后推进重试不得有任何新帧（B3 回归 2）");
    let p = adapter.projection(&key).await;
    assert!(p.stale && p.revision.is_none(), "重试后镜像仍不复活");
    // 卸载后的 owner 帧也不复活（lease 已被收尾回收）
    peer.send(&state_frame(
        &key.thread_id,
        "owner-1",
        snapshot_change(2, vec![], true),
    ))
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let p = adapter.projection(&key).await;
    assert!(p.stale && p.revision.is_none(), "owner 帧不复活镜像");
    adapter.shutdown().await;
}
