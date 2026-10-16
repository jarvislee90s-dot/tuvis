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
use multi_agents_manager_lib::services::codex_desktop_ipc::types::{CapabilitySet, PendingAction};
use std::sync::Mutex as StdMutex;

/// 注入单个 io 的测试连接器（adapter 只连一次）。
struct StaticConnector {
    io: StdMutex<
        Option<Box<dyn multi_agents_manager_lib::services::codex_desktop_ipc::transport::IpcIo>>,
    >,
}

impl StaticConnector {
    fn new(
        io: Box<dyn multi_agents_manager_lib::services::codex_desktop_ipc::transport::IpcIo>,
    ) -> Self {
        Self {
            io: StdMutex::new(Some(io)),
        }
    }
}

impl Connector for StaticConnector {
    fn connect(
        &self,
    ) -> Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        BoxIo,
                        multi_agents_manager_lib::services::codex_desktop_ipc::types::ProtocolError,
                    >,
                > + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            self.io
                .lock()
                .expect("lock")
                .take()
                .ok_or_else(|| multi_agents_manager_lib::services::codex_desktop_ipc::types::ProtocolError::Disconnected("no io".into()))
        })
    }
}

use std::pin::Pin;

/// 全能力测试 profile（macOS 已验证 build + 全 flag：只测构造器，不改生产 profile）。
fn caps_open_profile(
) -> multi_agents_manager_lib::services::codex_desktop_ipc::profile::ProtocolProfile {
    let mut p = profile_for("macos", "26.1013.31256").unwrap();
    p.capabilities = CapabilitySet {
        send: true,
        state: true,
        command: true,
        permissions: true,
        blocking_input: true,
    };
    p
}

/// 建 harness：驱动 ensure_thread 的握手/discovery/following/snapshot，
/// 返回（adapter、peer、快照后的投影）。
async fn harness(
    profile: multi_agents_manager_lib::services::codex_desktop_ipc::profile::ProtocolProfile,
    requests: Vec<serde_json::Value>,
) -> (
    DesktopIpcAdapter,
    ScriptedPeer,
    multi_agents_manager_lib::services::codex_desktop_ipc::types::ThreadProjection,
    ThreadKey,
) {
    let (io, peer) = ScriptedPeer::new();
    let adapter = DesktopIpcAdapter::new(std::sync::Arc::new(StaticConnector::new(io)), profile);
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

#[tokio::test]
async fn native_start_turn_targets_original_thread() {
    let (adapter, mut peer, _projection, key) = harness(caps_open_profile(), vec![]).await;
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
async fn native_approval_variants_never_share_wire_payload() {
    // 快照带 command（整数 id 18）与 permissions（整数 id 12）两条 pending
    let (adapter, mut peer, projection, key) = harness(
        caps_open_profile(),
        vec![
            serde_json::json!({
                "id": 18, "method": "item/commandExecution/requestApproval",
                "params": { "command": "pnpm test", "cwd": "/w", "reason": "r" }
            }),
            serde_json::json!({
                "id": 12, "method": "item/permissions/requestApproval",
                "params": { "cwd": "/tmp/x", "permissions": {
                    "network": serde_json::Value::Null,
                    "fileSystem": { "read": serde_json::Value::Null, "write": ["/tmp/x"], "entries": [] }
                } }
            }),
        ],
    )
    .await;
    let cmd_key = projection
        .pending
        .iter()
        .find(|p| p.kind == multi_agents_manager_lib::services::codex_desktop_ipc::types::PendingKind::CommandApproval)
        .expect("command pending")
        .clone();
    let perm_key = projection
        .pending
        .iter()
        .find(|p| p.kind == multi_agents_manager_lib::services::codex_desktop_ipc::types::PendingKind::PermissionsApproval)
        .expect("permissions pending")
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
    let (adapter, mut peer, projection, key) = harness(caps_open_profile(), vec![ui]).await;
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
    let (adapter, mut peer, projection, key) = harness(caps_open_profile(), vec![req]).await;
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
    // macOS 生产 profile：blocking_input=false → user-input 可见但不可答
    let mac = profile_for("macos", "26.1013.31256").unwrap();
    let req = serde_json::json!({
        "id": 7, "method": "item/tool/requestUserInput",
        "params": { "isBlocking": true, "questions": [
            { "id": "q1", "header": "h", "question": "?", "isOther": true, "isSecret": false, "options": [] }
        ]}
    });
    let (adapter, mut peer, projection, key) = harness(mac, vec![req]).await;
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
