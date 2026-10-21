//! Codex Desktop IPC 集成测试（本地假 IPC，Tokio duplex ScriptedPeer；不连接真实 Desktop）。
//!
//! 覆盖：RPC 关联（id+method）、未知投递边界（部分写/EOF=Unknown；排队前失败=NotSent）、
//! discovery 必答（旁路容量）、在飞上限、超时=Unknown。
//!
//! 并发模式：`conn.request()` 返回惰性 future；需要「请求在飞 + 同时做对端 IO」的
//! 用例必须把请求放 tokio::task（用克隆句柄拿到 'static），否则 recv 会永远等不到帧。

use multi_agents_manager_lib::services::codex_desktop_ipc::codec::encode_frame;

#[path = "support/codex_fixture.rs"]
mod codex_fixture;
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

// ---------------- 连接生命周期回归（PR #165 审查三缺陷） ----------------

/// 缺陷1回归：请求先于握手失败进入握手等待 → 对端回 error 后必须**有限时间**
/// 返回 NotSent。原实现挂在 `init.changed()` 上永不唤醒（握手失败路径从不向
/// init 发值、closed 置位不唤醒 watch 等待者），future 自持 handle 泄漏并拖死
/// 下游 router 派发任务；外层 timeout 断言防回归成挂死。
/// 2026-10-20 复审 P1：写前边界快照 = ensure 后的忙态 + 已存在 Turn 集合，
/// 供 adapter 在真正 start-turn 前拒绝「router 判 idle 后转 active」的竞态。
#[tokio::test]
async fn pre_write_snapshot_reports_busy_and_known_turns() {
    let (io, mut peer) = ScriptedPeer::new();
    let adapter = DesktopIpcAdapter::new(
        std::sync::Arc::new(TestConnector::new(test_endpoint("macos", MACOS_BUILD), io)),
        profile_for("macos", MACOS_BUILD).unwrap(),
    );
    let key = tkey("prewrite-thread");

    // idle 快照：不忙，但 Turn 集合已可用（用于旧 Turn 判定）
    let task = {
        let a = adapter.clone();
        let k = key.clone();
        tokio::spawn(async move { a.ensure_thread(&k).await })
    };
    drive_follow(
        &mut peer,
        "prewrite-thread",
        "owner-1",
        snapshot_change(10, vec![], true),
        true,
    )
    .await;
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("ensure in time")
        .expect("join")
        .expect("fresh");
    let (busy, ids) = adapter.pre_write_snapshot(&key);
    assert!(!busy, "idle 快照不忙");
    assert!(!ids.is_empty(), "已存在 Turn 集合可用：{ids:?}");

    // active 增量（同一 owner）：写前快照必须报忙
    let mut sse = adapter.subscribe();
    let active = serde_json::json!({
        "type": "patches", "baseRevision": 10, "revision": 11,
        "patches": [{ "op": "replace", "path": ["threadRuntimeStatus"], "value": { "type": "active" } }]
    });
    peer.send(&state_frame("prewrite-thread", "owner-1", active))
        .await;
    let _ = wait_projection(&mut sse, "prewrite-thread", false, 5).await;
    assert!(adapter.pre_write_snapshot(&key).0, "active 时必须报忙");
}

#[tokio::test(start_paused = true)]
async fn request_parked_on_handshake_wakes_when_handshake_fails() {
    let (io, mut peer) = ScriptedPeer::new();
    let profile = profile_for("macos", "26.1013.31256").unwrap();
    let conn = spawn(io, profile, 1);

    // t=0：请求进入握手等待（此刻 init=None、closed=false、未入队）
    let task = request_in_task(&conn, simple_request("thread-owner-discovery"));
    let init = peer.recv().await;
    assert_eq!(init["method"], "initialize");
    // 对端明确拒绝握手 → initialize_handshake 走失败收尾
    peer.send(&serde_json::json!({
        "type": "response",
        "requestId": init["requestId"],
        "resultType": "error",
        "method": "initialize",
        "error": { "code": "refused", "message": "no" }
    }))
    .await;

    let outcome = tokio::time::timeout(Duration::from_secs(8), task)
        .await
        .expect("握手失败后请求必须被唤醒，不得挂死")
        .expect("task ok");
    assert_eq!(outcome.delivery, Delivery::NotSent);
    assert!(conn.is_closed());
}

/// 缺陷1回归（握手超时路径）：对端收下 initialize 后装聋 → 3s INIT_TIMEOUT
/// 失败收尾 → 等待中的请求必须有限时间返回 NotSent（原实现永久挂起）。
/// 虚拟时钟：start_paused 下运行时空闲时自动推进，无需真实等待 3s。
#[tokio::test(start_paused = true)]
async fn request_parked_on_handshake_wakes_on_init_timeout() {
    let (io, mut peer) = ScriptedPeer::new();
    let profile = profile_for("macos", "26.1013.31256").unwrap();
    let conn = spawn(io, profile, 1);

    let task = request_in_task(&conn, simple_request("thread-owner-discovery"));
    let init = peer.recv().await;
    assert_eq!(init["method"], "initialize");
    // 对端不回执：虚拟时间自动推进触发 INIT_TIMEOUT（3s）失败收尾

    let outcome = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("握手超时后请求必须被唤醒，不得挂死")
        .expect("task ok");
    assert_eq!(outcome.delivery, Delivery::NotSent);
    assert!(conn.is_closed());
}

/// 缺陷2回归：在飞请求遭遇断线清场时，回执 envelope_id 必须是**原 envelope
/// UUID**（= 写出的 requestId），不得被 "released" 占位串顶替——占位串违反
/// RpcOutcome 契约（envelope_id 恒为本次生成的 UUID）并随持久账本入库。
#[tokio::test]
async fn disconnect_unknown_outcome_keeps_original_envelope_id() {
    let (io, mut peer) = ScriptedPeer::new();
    let profile = profile_for("macos", "26.1013.31256").unwrap();
    let conn = spawn(io, profile, 1);
    handshake(&mut peer, "owner-1").await;
    let _ = wait_initialized(&conn).await;

    let task = request_in_task(&conn, simple_request("thread-owner-discovery"));
    let frame = peer.recv().await;
    let rid = frame["requestId"]
        .as_str()
        .expect("uuid requestId")
        .to_string();
    // 在飞（帧已写出、回执未达）期间对端断线 → writer 收尾清场
    peer.close();
    let outcome = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("outcome in time")
        .expect("task ok");
    assert_eq!(outcome.delivery, Delivery::UnknownDelivery);
    assert_eq!(
        outcome.envelope_id, rid,
        "断线清场回执必须携带原 envelope UUID"
    );
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

/// 死 lease 复查（PR #165 复审 Minor）：registry 已换代到新 epoch 后，旧 epoch 连接
/// 的迟到 discovery 回执会 bind 出 generation=0 的死 lease——`discover_and_follow`
/// 不得把它当成功 Ok 返回，应在 declare 后以 StaleContext 失败（调用方按失败重试）。
#[tokio::test]
async fn discover_and_follow_rejects_dead_lease_from_old_epoch() {
    let (io, mut peer) = ScriptedPeer::new();
    let profile = profile_for("macos", "26.1013.31256").unwrap();
    let conn = spawn(io, profile, 1);
    handshake(&mut peer, "owner-1").await;
    let _ = wait_initialized(&conn).await;

    let task = {
        let c = conn.clone();
        tokio::spawn(async move {
            let mut owners = OwnerRegistry::default();
            owners.reset(2); // registry 已随新连接（epoch=2）换代：本连接 epoch=1 迟到
            let key = ThreadKey {
                host_id: "local".into(),
                thread_id: "thread-9".into(),
            };
            discover_and_follow(&c, &mut owners, &key).await
        })
    };
    // 旧连接的 discovery 照常得到回执（旧 epoch 迟到回执场景）
    let disc = peer.recv().await;
    peer.send(&discovery_response(
        disc["requestId"].as_str().unwrap(),
        "owner-1",
    ))
    .await;
    let _follow = peer.recv().await; // following 声明照发（wire 已见 true）
    let result = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("in time")
        .expect("task ok");
    assert!(
        matches!(
            result,
            Err(multi_agents_manager_lib::services::codex_desktop_ipc::types::ProtocolError::StaleContext(_))
        ),
        "死 lease 必须报错而非 Ok 返回：{result:?}"
    );
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
                    "turnHistory": codex_fixture::canonical_turn_history(serde_json::json!({
                            "t1": { "id": "t1", "type": "turn", "status": "inProgress", "items": ["u1"] },
                            "u1": { "id": "u1", "type": "user_message", "text": "跑测试", "ts": 1 }
                        }), serde_json::json!([{ "entries": ["t1"] }]))
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
    // 拆码契约：方法已知（user_input）但能力未验证 → capability_unverified
    //（下一行回执的 capability_unavailable 是投递面 reason_code，与投影拆码无关）
    assert_eq!(
        pending.disabled_reason_code.as_deref(),
        Some("capability_unverified")
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
    // 2026-10-18 形态适配：isComplete 在 **history 层**（真机金样），实体走
    // canonical 真实形态（合成键 + turnId 字段 + 内联 camelCase items）
    let mut change = serde_json::json!({
        "type": "snapshot", "revision": rev,
        "conversationState": {
            "turns": [], "requests": requests,
            "threadRuntimeStatus": { "type": "idle" },
            "turnHistory": codex_fixture::canonical_turn_history(
                serde_json::json!({
                    "t1": { "id": "t1", "type": "turn", "status": "completed", "items": ["u1"] },
                    "u1": { "id": "u1", "type": "user_message", "text": "hello", "ts": 1 }
                }),
                serde_json::json!(["t1"])
            )
        }
    });
    if !history_complete {
        change["conversationState"]["turnHistory"]["history"]["isComplete"] =
            serde_json::json!(false);
    }
    change
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

    // 快照不完整 → v1 只读 load-complete-history（params 带 conversationId + hostId:"local"；
    // result 带 revision——成功回执核对的面）
    let load = recv_frame(&mut peer, 5).await;
    assert_eq!(load["type"], "request");
    assert_eq!(load["method"], "thread-follower-load-complete-history");
    assert_eq!(load["version"], serde_json::json!(1));
    assert_eq!(load["params"]["conversationId"], "hist-thread");
    assert_eq!(load["params"]["hostId"], "local");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": load["requestId"],
        "resultType": "success", "method": "thread-follower-load-complete-history",
        "result": { "revision": 101 }
    }))
    .await;
    // 结果核对：回执带目标 revision → 等镜像吃到对应 snapshot 才收敛；本测试不发
    // 后续 snapshot → 加载状态停留（允许下一轮重试，见下）

    // 有界重试（2026-10-18 形态适配：不再是「每 follow 至多一次」——失败/未收敛
    // 允许退避重试，上限 3 次）。第一次回执 revision=101 未达成 → 重试发第二发
    // （第一轮回执的 revision 核对窗 5s + 退避 1s → 第二发最晚 ~6s 后到）
    let load2 = recv_frame(&mut peer, 12).await;
    assert_eq!(load2["method"], "thread-follower-load-complete-history");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": load2["requestId"],
        "resultType": "success", "method": "thread-follower-load-complete-history",
        "result": {}
    }))
    .await;
    // 无 revision 载荷 → 直接收敛（snapshot 由事件流推进）；后续仍不完整的
    // snapshot 是**新触发**（attempts=2 < 上限）→ 第三发
    peer.send(&state_frame(
        "hist-thread",
        "owner-1",
        snapshot_change(101, vec![], false),
    ))
    .await;
    let _ = wait_revision(&adapter, &key, 101).await;
    let load3 = recv_frame(&mut peer, 5).await;
    assert_eq!(load3["method"], "thread-follower-load-complete-history");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": load3["requestId"],
        "resultType": "success", "method": "thread-follower-load-complete-history",
        "result": {}
    }))
    .await;

    // 第三发之后（attempt=3 = 上限）不再自动发起：仍不完整的 snapshot 不再触发
    peer.send(&state_frame(
        "hist-thread",
        "owner-1",
        snapshot_change(102, vec![], false),
    ))
    .await;
    let _ = wait_revision(&adapter, &key, 102).await;
    assert_no_frame(&mut peer, 900).await;

    // 完整 snapshot 后同样没有加载帧
    peer.send(&state_frame(
        "hist-thread",
        "owner-1",
        snapshot_change(103, vec![], true),
    ))
    .await;
    let _ = wait_revision(&adapter, &key, 103).await;
    assert_no_frame(&mut peer, 900).await;
    adapter.shutdown().await;
}

/// 2026-10-19 复审 P2-2：Turn 列表完整（isComplete=true）但某 Turn 的 items 分页
/// 未载完 → **生产事件路径**必须发起只读 load-complete-history（旧判据只看列表
/// 完整性 → 恒不发起，产品永远等 Desktop 自己打开才补全）；成功后后续 snapshot
/// 收敛 Ready 且不再发起。target 终态证据不受其他 Turn 正文缺口影响（单测已钉）。
#[tokio::test]
async fn items_incomplete_triggers_read_only_hydrate() {
    fn items_snapshot(rev: u64, loaded_oldest: bool) -> serde_json::Value {
        serde_json::json!({
            "type": "snapshot", "revision": rev,
            "conversationState": {
                "turns": [], "requests": [],
                "threadRuntimeStatus": { "type": "idle" },
                "turnHistory": { "kind": "canonical", "history": {
                    "generation": 0, "isComplete": true,
                    "entitiesByKey": {
                        "turn:ti1": {
                            "turnId": "ti1", "status": "completed",
                            "items": [
                                { "type": "userMessage", "id": "ti1-u",
                                  "content": [ { "type": "text", "text": "正文" } ] }
                            ],
                            "itemsPagination": { "hasLoadedOldest": loaded_oldest,
                                                 "isLoadingOlder": false }
                        }
                    },
                    "islands": [ { "entries": [
                        { "key": "turn:ti1", "value": "turn:ti1" }
                    ] } ]
                } }
            }
        })
    }
    let (io, mut peer) = ScriptedPeer::new();
    let adapter = DesktopIpcAdapter::new(
        std::sync::Arc::new(TestConnector::new(test_endpoint("macos", MACOS_BUILD), io)),
        profile_for("macos", MACOS_BUILD).unwrap(),
    );
    let key = tkey("items-thread");
    let task = {
        let a = adapter.clone();
        let k = key.clone();
        tokio::spawn(async move { a.ensure_thread(&k).await })
    };
    drive_follow(
        &mut peer,
        "items-thread",
        "owner-1",
        items_snapshot(100, false),
        true,
    )
    .await;
    let proj = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("ensure in time")
        .expect("join")
        .expect("fresh");
    assert_eq!(
        proj.content_state,
        multi_agents_manager_lib::services::codex_desktop_ipc::types::ContentState::Partial,
        "列表完整 + items 未载入 = Partial（诚实标记）"
    );
    // 生产事件路径：实际发出只读加载请求
    let load = recv_frame(&mut peer, 5).await;
    assert_eq!(load["type"], "request");
    assert_eq!(load["method"], "thread-follower-load-complete-history");
    assert_eq!(load["params"]["conversationId"], "items-thread");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": load["requestId"],
        "resultType": "success", "method": "thread-follower-load-complete-history",
        "result": {}
    }))
    .await;
    // 后续 snapshot：items 已载完 → Ready 收敛，且不再发起加载
    peer.send(&state_frame(
        "items-thread",
        "owner-1",
        items_snapshot(101, true),
    ))
    .await;
    let p = wait_revision(&adapter, &key, 101).await;
    assert_eq!(
        p.content_state,
        multi_agents_manager_lib::services::codex_desktop_ipc::types::ContentState::Ready
    );
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

// 以下附件组合均为合成回归输入，字段证据见 artifacts/cases.synthetic.json。
// 测试只证明投影契约，不证明 Desktop 的运行时状态枚举。
fn artifact_projection(turn: serde_json::Value) -> serde_json::Value {
    artifact_projection_turns(vec![("turn", turn)])
}

/// 多回合投影：`entitiesByKey` 按给定顺序建引用链，用于验证回合间编号隔离。
fn artifact_projection_turns(turns: Vec<(&str, serde_json::Value)>) -> serde_json::Value {
    use multi_agents_manager_lib::services::codex_desktop_ipc::{
        mirror::ThreadMirror,
        types::{CapabilitySet, OwnerLease},
    };
    let key = ThreadKey {
        host_id: "local".into(),
        thread_id: "artifacts".into(),
    };
    let lease = OwnerLease {
        key: key.clone(),
        client_id: "owner".into(),
        epoch: 1,
        generation: 1,
    };
    let mut entities = serde_json::Map::new();
    let mut entries = Vec::new();
    for (entity_key, turn) in turns {
        entities.insert(entity_key.to_owned(), turn);
        entries.push(serde_json::json!({"key": entity_key, "value": entity_key}));
    }
    let mut mirror = ThreadMirror::new(key);
    mirror
        .apply(
            &lease,
            &serde_json::json!({
                "type": "snapshot", "revision": 1,
                "conversationState": { "cwd": "/project", "turns": [], "requests": [],
                    "turnHistory": { "kind": "canonical", "history": { "isComplete": true,
                        "entitiesByKey": serde_json::Value::Object(entities),
                        "islands": [{ "entries": entries }]
                    }}
                }
            }),
        )
        .unwrap();
    serde_json::to_value(mirror.project(CapabilitySet {
        send: true,
        state: true,
        command: false,
        permissions: false,
        blocking_input: false,
    }))
    .unwrap()
}

/// 附件对象字段集合（任务 1 契约）：序列化后应恰好是这 12 个键。
const ARTIFACT_KEYS: [&str; 12] = [
    "artifactId",
    "kind",
    "source",
    "path",
    "url",
    "fileKey",
    "ordinal",
    "imageIndexInTurn",
    "summary",
    "sourceStatus",
    "changeKind",
    "unavailableReason",
];

/// 从夹具记录的用户输入 / item 独立反推应产出的来源顺序。夹具是已评审证据，
/// 期望由它推导，不维护一份与记录位置对齐的硬编码清单（避免错位后静默通过）。
fn fixture_expected_sources(case: &serde_json::Value) -> Vec<&'static str> {
    fn push_blocks(blocks: &serde_json::Value, out: &mut Vec<&'static str>) {
        for block in blocks.as_array().into_iter().flatten() {
            match block["type"].as_str() {
                Some("localImage") => out.push("local_image"),
                Some("mention") => out.push("mention"),
                Some("image") => out.push("image_url"),
                _ => {}
            }
        }
    }
    let turn = &case["turn"];
    let items = turn["items"].as_array().cloned().unwrap_or_default();
    let has_formal_user = items.iter().any(|item| {
        item["type"] == "userMessage"
            && item["content"].as_array().is_some_and(|blocks| {
                blocks.iter().any(|block| {
                    matches!(
                        block["type"].as_str(),
                        Some("text" | "localImage" | "mention" | "image")
                    )
                })
            })
    });
    let mut out = Vec::new();
    for item in &items {
        match item["type"].as_str() {
            Some("userMessage") => push_blocks(&item["content"], &mut out),
            Some("imageGeneration") => out.push("image_generation"),
            Some("imageView") => out.push("image_view"),
            Some("fileChange") => out.extend(std::iter::repeat_n(
                "file_change",
                item["changes"].as_array().map_or(0, Vec::len),
            )),
            _ => {}
        }
    }
    if !has_formal_user {
        push_blocks(&turn["params"]["input"], &mut out);
    }
    out
}

#[test]
fn artifacts_extract_all_documented_sources_without_inventing_success() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/codex-desktop-ipc/artifacts/cases.synthetic.json"
    ))
    .unwrap();
    let cases = fixture["cases"].as_array().expect("fixture cases");
    assert_eq!(
        cases.len(),
        6,
        "Task 1 夹具固定六个回归组合；增删须先评审证据本身"
    );
    let expected: Vec<(&str, Vec<&str>)> = cases
        .iter()
        .map(|case| {
            (
                case["id"].as_str().expect("case id"),
                fixture_expected_sources(case),
            )
        })
        .collect();
    // 六类来源都必须被夹具覆盖（image_url 是用户 image.url 的独立分支）。
    let covered: std::collections::HashSet<&str> = expected
        .iter()
        .flat_map(|(_, sources)| sources.iter().copied())
        .collect();
    for source in [
        "image_generation",
        "local_image",
        "mention",
        "file_change",
        "image_view",
        "image_url",
    ] {
        assert!(covered.contains(source), "夹具未覆盖来源 {source}");
    }
    for (id, wanted) in &expected {
        let case = cases
            .iter()
            .find(|case| case["id"] == *id)
            .expect("fixture case");
        let p = artifact_projection(case["turn"].clone());
        let artifacts: Vec<&serde_json::Value> = p["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .flat_map(|message| message["artifacts"].as_array().into_iter().flatten())
            .collect();
        let actual: Vec<&str> = artifacts
            .iter()
            .map(|artifact| artifact["source"].as_str().expect("source"))
            .collect();
        assert_eq!(actual, *wanted, "case {id} 的来源顺序与夹具记录不一致");
        for artifact in &artifacts {
            let object = artifact.as_object().expect("artifact object");
            assert_eq!(
                object.len(),
                ARTIFACT_KEYS.len(),
                "case {id} 附件字段集合应与任务 1 契约一致"
            );
            for key in ARTIFACT_KEYS {
                assert!(object.contains_key(key), "case {id} 缺字段 {key}");
            }
            assert!(
                matches!(
                    artifact["kind"].as_str(),
                    Some("image" | "document" | "external_image")
                ),
                "case {id} kind 越界: {artifact}"
            );
        }
    }
    // 夹具未覆盖的组合：同一回合内五类引用 + fileChange 并存；`skill` 必须排除。
    let p = artifact_projection(serde_json::json!({"turnId":"t", "items":[
        {"id":"u", "type":"userMessage", "content":[
            {"type":"localImage", "path":"upload.png"},
            {"type":"mention", "name":"notes.md", "path":"notes.md"},
            {"type":"image", "url":"https://example.invalid/figure.png"},
            {"type":"image", "fileId":"secret"},
            {"type":"skill", "path":"ignored/SKILL.md"}]},
        {"id":"gen", "type":"imageGeneration", "savedPath":"generated.png", "revisedPrompt":"prompt"},
        {"id":"change", "type":"fileChange", "status":"invented-status", "changes":[
            {"path":"src/a.ts", "kind":"invented-add"}, {"path":null, "kind":"invented-modify"}]},
        {"id":"view", "type":"imageView", "path":"viewed.png"}]}));
    let artifacts: Vec<&serde_json::Value> = p["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .flat_map(|message| message["artifacts"].as_array().into_iter().flatten())
        .collect();
    let sources: Vec<&str> = artifacts
        .iter()
        .map(|artifact| artifact["source"].as_str().expect("source"))
        .collect();
    assert_eq!(
        sources,
        [
            "local_image",
            "mention",
            "image_url",
            "image_url",
            "image_generation",
            "file_change",
            "file_change",
            "image_view"
        ],
        "同一回合混合来源的投影顺序"
    );
    assert_eq!(
        artifacts[2]["url"], "https://example.invalid/figure.png",
        "HTTP(S) 外链原样保留，不代理下载"
    );
    assert_eq!(artifacts[3]["unavailableReason"], "unresolved_file_id");
    assert!(
        artifacts[3]["path"].is_null() && artifacts[3]["url"].is_null(),
        "纯 fileId 不推测本地路径"
    );
    // 无完成语义证据的状态/变更类型只保留原标量，fileChange 仍是文件 chip；
    // 可预览引用不因“完成度未验证”被标成不可用。
    let p = artifact_projection(serde_json::json!({"turnId":"t", "items":[
        {"id":"change", "type":"fileChange", "status":"invented-status", "changes":[
            {"path":"a.png", "kind":"invented-kind"}]},
        {"id":"unknown", "type":"newTool", "output":"/secret/guess.png"}
    ]}));
    let change = &p["messages"][0]["artifacts"][0];
    assert_eq!(change["kind"], "document", "fileChange 图片仍是文件 chip");
    assert_eq!(change["sourceStatus"], "invented-status");
    assert_eq!(change["changeKind"], "invented-kind");
    assert_eq!(change["path"], "a.png");
    assert_eq!(change["fileKey"], "/project/a.png");
    assert_eq!(change["imageIndexInTurn"], serde_json::Value::Null);
    assert_eq!(
        change["unavailableReason"],
        serde_json::Value::Null,
        "可预览引用不因完成度未验证而不可用"
    );
    assert_eq!(p["messages"][1]["text"], "[unsupported item: newTool]");
    assert_eq!(p["messages"][1]["artifacts"], serde_json::json!([]));
    // 原始状态/变更类型同样有界：超限丢弃原文，不截断伪造标量，也不写不可用原因。
    let p = artifact_projection(serde_json::json!({"turnId":"t", "items":[
        {"id":"change", "type":"fileChange", "status":"s".repeat(1025),
         "changes":[{"path":"a.ts", "kind":"k".repeat(1025)}]}]}));
    let change = &p["messages"][0]["artifacts"][0];
    assert_eq!(change["kind"], "document");
    assert!(
        change["sourceStatus"].is_null() && change["changeKind"].is_null(),
        "超限原始标量不截断、不猜测语义"
    );
    assert_eq!(change["path"], "a.ts");
    assert_eq!(change["fileKey"], "/project/a.ts");
    assert_eq!(
        change["unavailableReason"],
        serde_json::Value::Null,
        "元数据超限不是预览不可用"
    );
}

#[test]
fn artifacts_mention_kind_follows_allowed_image_extensions() {
    let p = artifact_projection(serde_json::json!({"turnId":"t", "items":[
        {"id":"u", "type":"userMessage", "content":[
            {"type":"mention", "name":"photo.PNG", "path":"photo.PNG"},
            {"type":"mention", "name":"report.pdf", "path":"report.pdf"},
            {"type":"mention", "name":"no-path.png"}]}]}));
    let artifacts = p["messages"][0]["artifacts"].as_array().expect("artifacts");
    assert_eq!(
        artifacts[0]["kind"], "image",
        "图片扩展名（大小写不敏感）→ 图片卡"
    );
    assert_eq!(
        artifacts[0]["summary"], "photo.PNG",
        "mention 摘要取 name 原文"
    );
    assert_eq!(artifacts[0]["imageIndexInTurn"], 1);
    assert_eq!(artifacts[1]["kind"], "document");
    assert!(artifacts[1]["imageIndexInTurn"].is_null());
    assert_eq!(artifacts[1]["fileKey"], "/project/report.pdf");
    // 缺路径的图片名不产生本地路径 / 图片编号，仍保留不可用引用。
    assert_eq!(artifacts[2]["kind"], "image");
    assert_eq!(artifacts[2]["unavailableReason"], "missing_path");
    assert!(artifacts[2]["path"].is_null() && artifacts[2]["imageIndexInTurn"].is_null());
    assert_eq!(p["messages"][0]["turnImageCount"], 1);
}

#[test]
fn artifacts_attachment_only_formal_replaces_opening_with_same_identity() {
    let mut turn = serde_json::json!({"turnId":"t", "params":{
        "clientUserMessageId":"client", "input":[{"type":"localImage", "path":"./a.png"}]}, "items":[]});
    let before = artifact_projection(turn.clone());
    let before_messages = before["messages"].as_array().expect("messages");
    assert_eq!(before_messages.len(), 1, "纯附件开场后备必须可见");
    assert_eq!(before_messages[0]["role"], "user");
    assert_eq!(before_messages[0]["text"], "");
    assert_eq!(before_messages[0]["turnId"], "t", "后备也有稳定回合身份");
    assert_eq!(before_messages[0]["artifacts"][0]["source"], "local_image");
    assert_eq!(before_messages[0]["turnImageCount"], 1);
    turn["items"] = serde_json::json!([{"id":"formal", "clientId":"client", "type":"userMessage",
        "content":[{"type":"localImage", "path":"./a.png"}]}]);
    let after = artifact_projection(turn);
    let after_messages = after["messages"].as_array().expect("messages");
    assert_eq!(
        after_messages.len(),
        1,
        "正式项替换后备，不得叠成两条用户消息"
    );
    assert_eq!(
        after_messages[0]["id"], before_messages[0]["id"],
        "同一逻辑消息身份不变"
    );
    assert_eq!(
        after_messages[0]["artifacts"], before_messages[0]["artifacts"],
        "替换后附件身份与内容不变"
    );
    assert_eq!(
        after_messages[0]["artifacts"][0]["fileKey"],
        "/project/a.png"
    );
    assert_eq!(after_messages[0]["text"], "");
}

#[test]
fn artifacts_number_before_tail_window_and_separate_source_ordinals() {
    let mut items = vec![
        serde_json::json!({"id":"u", "type":"userMessage", "content":[
        {"type":"localImage", "path":"upload.png"},
        {"type":"image", "url":"https://example.invalid/a.png"}]}),
        serde_json::json!({"id":"f", "type":"fileChange", "changes":[{"path":"f.png"}]}),
    ];
    for i in 0..501 {
        items.push(serde_json::json!({"id":format!("g{i}"), "type":"imageGeneration", "savedPath":format!("g{i}.png")}));
    }
    let p = artifact_projection(serde_json::json!({"turnId":"t", "items":items}));
    let messages = p["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 500, "尾窗最多 500 条");
    assert_eq!(messages[0]["id"], "g1", "丢弃更早三条，保留最新 500 条");
    // 编号在完整 canonical 顺序上统计：g0 被裁掉仍占第 1 个生成图序号 / 第 2 张图。
    assert_eq!(messages[0]["artifacts"][0]["source"], "image_generation");
    assert_eq!(
        messages[0]["artifacts"][0]["artifactId"], "g1/artifact/0",
        "尾窗裁剪不改变附件身份"
    );
    assert_eq!(
        messages[0]["artifacts"][0]["ordinal"], 2,
        "来源序号按生成图自身计数"
    );
    assert_eq!(
        messages[0]["artifacts"][0]["imageIndexInTurn"], 3,
        "回合图片索引与来源序号互不混用"
    );
    assert_eq!(messages[0]["turnImageCount"], 502);
    assert_eq!(messages[499]["artifacts"][0]["imageIndexInTurn"], 502);
    assert_eq!(p["truncated"], true);
}

#[test]
fn artifacts_missing_and_unsupported_references_have_bounded_reasons() {
    let p = artifact_projection(serde_json::json!({"turnId":"t", "items":[
        {"id":"u", "type":"userMessage", "content":[
            {"type":"localImage", "path":"  "}, {"type":"mention", "name":"missing.pdf"},
            {"type":"image", "fileId":"secret-id"},
            {"type":"image"},
            {"type":"image", "url":format!("data:image/png;base64,{}", "A".repeat(2_000_000))},
            {"type":"image", "url":"file:///secret.png"},
            {"type":"image", "url":"https://example.invalid/a.png"},
            {"type":"image", "url":format!("https://example.invalid/{}", "a".repeat(16 * 1024))},
            {"type":"skill", "path":"not-a-mention.md"}]}]}));
    let message = &p["messages"][0];
    let a = message["artifacts"].as_array().expect("artifacts");
    assert_eq!(a.len(), 8, "skill 不因与 mention 共用字段而扩张为附件");
    for (i, reason) in [
        (0, "missing_path"),
        (1, "missing_path"),
        (2, "unresolved_file_id"),
        (3, "missing_url"),
        (4, "data_url"),
        (5, "unsupported_url_scheme"),
        (7, "url_too_long"),
    ] {
        assert_eq!(a[i]["unavailableReason"], reason, "artifact {i}");
        assert!(a[i]["path"].is_null() && a[i]["url"].is_null() && a[i]["fileKey"].is_null());
        assert!(
            a[i]["imageIndexInTurn"].is_null(),
            "不可用图片不占回合图片索引"
        );
    }
    assert_eq!(a[6]["kind"], "external_image");
    assert_eq!(a[6]["url"], "https://example.invalid/a.png");
    assert_eq!(a[6]["unavailableReason"], serde_json::Value::Null);
    assert!(a[6]["imageIndexInTurn"].is_null(), "外链不占图片卡限额");
    assert_eq!(message["turnImageCount"], 0, "不可用引用不算图片卡");
    // data URL 原文（2 MB 的 A）与超长 URL 不进入 DTO；纯 fileId 也不复制。
    let serialized = serde_json::to_string(&p).unwrap();
    assert!(serialized.len() < 10_000, "投影体积与输入 base64/URL 无关");
    assert!(!serialized.contains("data:image/png;base64"));
    assert!(!serialized.contains("secret-id"));
}

#[test]
fn artifacts_bound_fields_counts_and_preserve_full_turn_image_count() {
    let mut content = vec![
        serde_json::json!({"type":"mention", "name":"图".repeat(1100), "path":"ok.md"}),
        serde_json::json!({"type":"localImage", "path":"x".repeat(16 * 1024 + 1)}),
        serde_json::json!({"type":"localImage", "path":"p".repeat(16 * 1024)}),
    ];
    for i in 0..130 {
        content.push(serde_json::json!({"type":"localImage", "path":format!("{i}.png")}));
    }
    let p = artifact_projection(
        serde_json::json!({"turnId":"t", "items":[{"id":"u", "type":"userMessage", "content":content}]}),
    );
    let m = &p["messages"][0];
    assert_eq!(
        m["artifacts"].as_array().unwrap().len(),
        128,
        "每消息最多 128 个附件"
    );
    assert_eq!(m["omittedArtifactCount"], 5, "溢出只计数，不复制字段");
    assert_eq!(
        m["turnImageCount"], 131,
        "总数按完整回合的可展示图片统计（不含超长路径那条）"
    );
    assert_eq!(
        m["artifacts"][0]["summary"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        1024,
        "摘要按 Unicode 字符截到 1024"
    );
    assert_eq!(
        m["artifacts"][0]["unavailableReason"],
        serde_json::Value::Null,
        "摘要截断不是预览不可用，路径/身份键可用的引用保持可渲染"
    );
    assert_eq!(m["artifacts"][0]["path"], "ok.md");
    assert_eq!(m["artifacts"][0]["fileKey"], "/project/ok.md");
    assert_eq!(m["artifacts"][1]["unavailableReason"], "path_too_long");
    assert!(m["artifacts"][1]["path"].is_null());
    assert!(
        m["artifacts"][1]["imageIndexInTurn"].is_null(),
        "路径不可用的图片不占回合图片索引"
    );
    assert_eq!(m["artifacts"][2]["unavailableReason"], "file_key_too_long");
    assert!(m["artifacts"][2]["fileKey"].is_null());
    assert_eq!(
        m["artifacts"][2]["imageIndexInTurn"], 1,
        "路径可用但身份键超限仍可作为图片卡显示"
    );
    assert_eq!(m["artifacts"][127]["imageIndexInTurn"], 126);
    assert_eq!(
        p["truncated"], false,
        "128 条/消息是服务端附件预算，不能冒充尾窗截断"
    );
}

#[test]
fn artifacts_serialized_budget_counts_escaping_and_latest_metadata() {
    // 引号在 JSON 里转义为 2 字节：按 text.len() 累计会低估一半，尾窗按实际
    // 序列化字节计。
    let items: Vec<_> = (0..4).map(|i| serde_json::json!({"type":"agentMessage", "id":format!("a{i}"), "text":"\"".repeat(200_000)})).collect();
    let p = artifact_projection(serde_json::json!({"turnId":"t", "items":items}));
    let messages = p["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 2, "4×400 KB（转义后）只容得下最新 2 条");
    assert_eq!(messages[1]["id"], "a3");
    assert!(serde_json::to_vec(messages).unwrap().len() <= 1024 * 1024);
    assert_eq!(p["truncated"], true, "丢弃更早消息必须显式标记");
    // 最新条 128 个超大路径附件自身 > 1 MiB：保底保留最新条，超限附件按服务端
    // 预算省略并计数；较早消息因预算被丢弃。不能把省略说成 UI 的 12 图限额。
    let content: Vec<_> = (0..128).map(|_| serde_json::json!({"type":"localImage", "path":format!("/{}.png", "x".repeat(12_000))})).collect();
    let p = artifact_projection(serde_json::json!({"turnId":"t", "items":[
        {"id":"old", "type":"agentMessage", "text":"o".repeat(100_000)},
        {"id":"latest", "type":"userMessage", "content":content}]}));
    let messages = p["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 1, "较早条被字节预算丢弃，最新条保底保留");
    assert_eq!(messages[0]["id"], "latest");
    assert!(
        messages[0]["omittedArtifactCount"].as_u64().unwrap() > 0,
        "被预算裁掉的附件必须计数"
    );
    assert_eq!(p["truncated"], true);
    assert!(
        serde_json::to_vec(messages).unwrap().len() <= 1024 * 1024,
        "按实际序列化字节守 1 MiB 预算"
    );
}

#[test]
fn artifacts_long_summary_keeps_image_cards_renderable() {
    let p = artifact_projection(serde_json::json!({"turnId":"t", "items":[
        {"id":"u", "type":"userMessage", "content":[
            {"type":"text", "text":"长".repeat(1100)},
            {"type":"localImage", "path":"upload.png"}]},
        {"id":"gen", "type":"imageGeneration", "savedPath":"generated.png",
         "revisedPrompt":"p".repeat(1100), "status":"synthetic-complete"}]}));
    let messages = p["messages"].as_array().expect("messages");
    let artifacts: Vec<&serde_json::Value> = messages
        .iter()
        .flat_map(|message| message["artifacts"].as_array().into_iter().flatten())
        .collect();
    assert_eq!(artifacts.len(), 2);
    for artifact in &artifacts {
        assert_eq!(
            artifact["summary"].as_str().unwrap().chars().count(),
            1024,
            "摘要按 Unicode 字符有界"
        );
        assert_eq!(
            artifact["unavailableReason"],
            serde_json::Value::Null,
            "摘要/状态有界截断不得把可预览引用标成不可用"
        );
        assert!(
            artifact["path"].as_str().is_some() && artifact["fileKey"].as_str().is_some(),
            "可预览引用必须保留路径与身份键"
        );
    }
    assert_eq!(artifacts[0]["kind"], "image");
    assert_eq!(artifacts[0]["path"], "upload.png");
    assert_eq!(artifacts[0]["fileKey"], "/project/upload.png");
    assert_eq!(artifacts[0]["imageIndexInTurn"], 1);
    assert_eq!(artifacts[1]["path"], "generated.png");
    assert_eq!(artifacts[1]["fileKey"], "/project/generated.png");
    assert_eq!(artifacts[1]["sourceStatus"], "synthetic-complete");
    assert_eq!(artifacts[1]["imageIndexInTurn"], 2);
}

#[test]
fn artifacts_opening_fallback_survives_empty_formal_user_content() {
    let p = artifact_projection(serde_json::json!({"turnId":"t", "params":{
        "clientUserMessageId":"client",
        "input":[{"type":"text", "text":"开场正文"}]},
        "items":[{"id":"formal", "type":"userMessage", "content":[]}]}));
    let messages = p["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 1, "空 content 的正式项不得压掉开场后备");
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["text"], "开场正文");
    assert_eq!(messages[0]["id"], "t/opening/client");
}

#[test]
fn artifacts_opening_fallback_survives_skill_only_formal_user_content() {
    let p = artifact_projection(serde_json::json!({"turnId":"t", "params":{
        "clientUserMessageId":"client",
        "input":[{"type":"text", "text":"开场正文"}]},
        "items":[{"id":"formal", "clientId":"client", "type":"userMessage",
                  "content":[{"type":"skill", "name":"x", "path":"SKILL.md"}]}]}));
    let messages = p["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 1, "skill-only 正式项不得压掉开场后备");
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["text"], "开场正文");
    assert_eq!(messages[0]["id"], "t/opening/client");
}

#[test]
fn artifacts_popping_newest_artifacts_does_not_mark_window_truncated() {
    let content: Vec<_> = (0..128)
        .map(|_| {
            serde_json::json!({"type":"localImage", "path":format!("/{}.png", "x".repeat(12_000))})
        })
        .collect();
    let p = artifact_projection(serde_json::json!({"turnId":"t", "items":[
        {"id":"latest", "type":"userMessage", "content":content}]}));
    let messages = p["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 1);
    let artifacts = messages[0]["artifacts"].as_array().expect("artifacts");
    assert!(artifacts.len() < 128, "单条超预算时必须裁掉附件元数据");
    assert!(
        messages[0]["omittedArtifactCount"].as_u64().unwrap() > 0,
        "被裁附件必须计数"
    );
    assert_eq!(
        p["truncated"], false,
        "只省略消息内附件不是消息窗口截断，横幅不得误报"
    );
}

#[test]
fn artifacts_turn_numbering_and_omitted_counts_are_isolated_per_turn() {
    let p = artifact_projection_turns(vec![
        (
            "turnA",
            serde_json::json!({"turnId":"turnA", "items":[
                {"id":"ua", "type":"userMessage", "content":[
                    {"type":"localImage", "path":"a1.png"},
                    {"type":"localImage", "path":"a2.png"}]},
                {"id":"fa", "type":"fileChange", "changes":[
                    {"path":"a.ts"}, {"path":"b.ts"}]}]}),
        ),
        (
            "turnB",
            serde_json::json!({"turnId":"turnB", "items":[
                {"id":"ub", "type":"userMessage", "content":[
                    {"type":"localImage", "path":"b1.png"}]},
                {"id":"fb", "type":"fileChange", "changes":[{"path":"c.ts"}]}]}),
        ),
    ]);
    let messages = p["messages"].as_array().expect("messages");
    let turn_artifacts = |turn_id: &str| -> Vec<&serde_json::Value> {
        messages
            .iter()
            .filter(|message| message["turnId"] == turn_id)
            .flat_map(|message| message["artifacts"].as_array().into_iter().flatten())
            .collect()
    };
    let a = turn_artifacts("turnA");
    let b = turn_artifacts("turnB");
    assert_eq!(a.len(), 4);
    assert_eq!(b.len(), 2);
    // 每个回合的来源序号从 1 独立开始，不跨回合串号。
    assert_eq!(a[0]["ordinal"], 1);
    assert_eq!(a[1]["ordinal"], 2);
    assert_eq!(a[2]["ordinal"], 1);
    assert_eq!(a[3]["ordinal"], 2);
    assert_eq!(b[0]["ordinal"], 1);
    assert_eq!(b[1]["ordinal"], 1);
    // 图片索引与总数按回合各自统计。
    assert_eq!(a[0]["imageIndexInTurn"], 1);
    assert_eq!(a[1]["imageIndexInTurn"], 2);
    assert_eq!(b[0]["imageIndexInTurn"], 1);
    for (turn_id, expected_images) in [("turnA", 2), ("turnB", 1)] {
        for message in messages.iter().filter(|m| m["turnId"] == turn_id) {
            assert_eq!(message["turnImageCount"], expected_images, "{turn_id}");
            assert_eq!(message["omittedArtifactCount"], 0, "{turn_id}");
        }
    }
}

#[test]
fn artifacts_serialized_window_counts_commas_for_retained_non_newest_message() {
    use multi_agents_manager_lib::services::codex_desktop_ipc::types::ProjectedMessage;
    // 先量出 agentMessage 的固定序列化开销（id "old" 与 "new" 等长，文本为 ASCII
    // 单字节、无转义），再把非最新条长度精确放到“含逗号刚好等于 1 MiB”的边界。
    let base = serde_json::to_vec(&ProjectedMessage {
        id: "old".into(),
        role: "assistant".into(),
        kind: "agentMessage".into(),
        text: String::new(),
        ts: None,
        turn_id: Some("t".into()),
        artifacts: Vec::new(),
        turn_image_count: 0,
        omitted_artifact_count: 0,
    })
    .unwrap()
    .len();
    let newest_text = 1_000;
    let newest_bytes = base + newest_text;
    // 2（数组括号）+ 最新条 + 1（逗号）+ 非最新条 == 1 MiB。
    let older_text = 1024 * 1024 - 2 - newest_bytes - 1 - base;

    let p = artifact_projection(serde_json::json!({"turnId":"t", "items":[
        {"id":"old", "type":"agentMessage", "text":"y".repeat(older_text)},
        {"id":"new", "type":"agentMessage", "text":"z".repeat(newest_text)}]}));
    let messages = p["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 2, "含逗号刚好落在预算内的非最新条必须保留");
    assert_eq!(messages[0]["id"], "old");
    assert_eq!(messages[1]["id"], "new");
    let byte_len = serde_json::to_vec(messages).unwrap().len();
    assert_eq!(
        byte_len,
        1024 * 1024,
        "尾窗按含数组逗号的真实序列化字节精确计数"
    );
    assert_eq!(p["truncated"], false);

    // 非最新条再多 1 字节：含逗号后越过预算，必须丢弃它并标 truncated。
    let p = artifact_projection(serde_json::json!({"turnId":"t", "items":[
        {"id":"old", "type":"agentMessage", "text":"y".repeat(older_text + 1)},
        {"id":"new", "type":"agentMessage", "text":"z".repeat(newest_text)}]}));
    let messages = p["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 1, "逗号参与预算后，超 1 字节即丢旧条");
    assert_eq!(messages[0]["id"], "new");
    assert_eq!(p["truncated"], true);
}

#[test]
fn artifacts_old_message_payload_defaults_new_fields() {
    use multi_agents_manager_lib::services::codex_desktop_ipc::types::ProjectedMessage;
    let m: ProjectedMessage = serde_json::from_value(serde_json::json!({"id":"old", "role":"user", "kind":"userMessage", "text":"old text", "ts":null})).unwrap();
    let m = serde_json::to_value(m).unwrap();
    assert_eq!(m["artifacts"], serde_json::json!([]));
    assert_eq!(m["turnId"], serde_json::Value::Null);
    assert_eq!(m["turnImageCount"], 0);
    assert_eq!(m["omittedArtifactCount"], 0);
}

// ============================================================
// 任务 4：同代只读消息分页（ThreadMirror::messages_page）
//
// 夹具全部合成，只证明分页契约（游标/代际/预算/状态），不冒充 Desktop 实机字段证据。
// ============================================================

mod messages_page {
    use multi_agents_manager_lib::services::codex_desktop_ipc::{
        mirror::{
            MirrorLedger, ThreadMirror, MESSAGES_PAGE_BYTES, MESSAGES_PAGE_LIMIT,
            PROJECTION_TAIL_BYTES,
        },
        types::{
            CapabilitySet, MessagesPage, MessagesPageStatus, OwnerLease, ProjectedMessage,
            ThreadKey, ThreadProjection,
        },
    };
    use std::sync::{Arc, Mutex as StdMutex};

    fn key() -> ThreadKey {
        ThreadKey {
            host_id: "local".into(),
            thread_id: "page-thread".into(),
        }
    }

    fn lease(epoch: u64, generation: u64) -> OwnerLease {
        OwnerLease {
            key: key(),
            client_id: "owner-1".into(),
            epoch,
            generation,
        }
    }

    fn caps() -> CapabilitySet {
        CapabilitySet {
            send: true,
            state: true,
            command: false,
            permissions: false,
            blocking_input: false,
        }
    }

    fn snapshot(rev: u64, state: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "type": "snapshot", "revision": rev, "conversationState": state
        })
    }

    fn state_of_items(items: serde_json::Value) -> serde_json::Value {
        state_of_items_with_status(items, "completed")
    }

    /// 单 Turn 夹具，可指定该 Turn 的状态（活动 Turn 用 `inProgress`）。
    fn state_of_items_with_status(items: serde_json::Value, status: &str) -> serde_json::Value {
        serde_json::json!({
            "turns": [], "requests": [],
            "threadRuntimeStatus": { "type": "idle" },
            "turnHistory": { "kind": "canonical", "history": {
                "isComplete": true,
                "entitiesByKey": {
                    "turn:t1": { "turnId": "t1", "status": status, "items": items }
                },
                "islands": [{ "entries": [{ "key": "turn:t1", "value": "turn:t1" }] }]
            }}
        })
    }

    fn state_with_ids(ids: &[&str]) -> serde_json::Value {
        state_with_ids_status(ids, "completed")
    }

    /// 单 Turn 夹具（可指定状态）：活动 Turn 用 `inProgress`。
    fn state_with_ids_status(ids: &[&str], status: &str) -> serde_json::Value {
        let items: Vec<serde_json::Value> = ids
            .iter()
            .map(|id| serde_json::json!({ "type": "agentMessage", "id": id, "text": format!("text-{id}") }))
            .collect();
        state_of_items_with_status(serde_json::json!(items), status)
    }

    fn mirror_with_ids_status(ids: &[&str], status: &str) -> ThreadMirror {
        let mut m = ThreadMirror::new(key());
        m.apply(
            &lease(1, 1),
            &snapshot(1, state_with_ids_status(ids, status)),
        )
        .expect("snapshot");
        m
    }

    fn mirror_with_ids(ids: &[&str]) -> ThreadMirror {
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(1, 1), &snapshot(1, state_with_ids(ids)))
            .expect("snapshot");
        m
    }

    fn message_ids(messages: &[ProjectedMessage]) -> Vec<String> {
        messages.iter().map(|m| m.id.clone()).collect()
    }

    fn page(mirror: &ThreadMirror, cursor: Option<&str>, token: &str) -> MessagesPage {
        mirror.messages_page(cursor, token, MESSAGES_PAGE_LIMIT)
    }

    fn projection(mirror: &ThreadMirror) -> ThreadProjection {
        mirror.project(caps())
    }

    /// 多 Turn 夹具：一个 island 里按 t1..tN 顺序引用实体（canonical 旧 → 新）。
    fn state_of_turns(turns: &[(String, Vec<serde_json::Value>)]) -> serde_json::Value {
        let mut entities = serde_json::Map::new();
        let mut entries = Vec::new();
        for (turn_id, items) in turns {
            let key = format!("turn:{turn_id}");
            entities.insert(
                key.clone(),
                serde_json::json!({ "turnId": turn_id, "status": "completed", "items": items }),
            );
            entries.push(serde_json::json!({ "key": key, "value": key }));
        }
        serde_json::json!({
            "turns": [], "requests": [],
            "threadRuntimeStatus": { "type": "idle" },
            "turnHistory": { "kind": "canonical", "history": {
                "isComplete": true,
                "entitiesByKey": entities,
                "islands": [{ "entries": entries }]
            }}
        })
    }

    fn turn(id: &str, items: Vec<serde_json::Value>) -> (String, Vec<serde_json::Value>) {
        (id.to_string(), items)
    }

    /// 多 Turn 夹具：把**最后一个** Turn 标成 `inProgress`——活动 Turn 的真实形态
    /// （流式补全只发生在活动 Turn 的末条消息上）。
    fn state_of_turns_last_active(turns: &[(String, Vec<serde_json::Value>)]) -> serde_json::Value {
        let mut state = state_of_turns(turns);
        let key = format!("turn:{}", turns.last().expect("至少一个 Turn").0);
        state["turnHistory"]["history"]["entitiesByKey"][key.as_str()]["status"] =
            serde_json::json!("inProgress");
        state
    }

    /// 覆盖若干 Turn 的状态（`None` = 删掉 status 字段，模拟未知/旧载荷）。
    fn override_turn_status(state: &mut serde_json::Value, overrides: &[(&str, Option<&str>)]) {
        for (turn_id, status) in overrides {
            let key = format!("turn:{turn_id}");
            let entity = state["turnHistory"]["history"]["entitiesByKey"]
                .get_mut(key.as_str())
                .and_then(serde_json::Value::as_object_mut)
                .unwrap_or_else(|| panic!("夹具缺少 Turn {turn_id}"));
            match status {
                Some(value) => {
                    entity.insert("status".to_string(), serde_json::json!(value));
                }
                None => {
                    entity.remove("status");
                }
            }
        }
    }

    fn agent(id: &str, text: &str) -> serde_json::Value {
        serde_json::json!({ "type": "agentMessage", "id": id, "text": text })
    }

    fn user(id: &str, text: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "userMessage", "id": id,
            "content": [{ "type": "text", "text": text }]
        })
    }

    fn mirror_of(state: serde_json::Value) -> ThreadMirror {
        let mut mirror = ThreadMirror::new(key());
        mirror
            .apply(&lease(1, 1), &snapshot(1, state))
            .expect("snapshot");
        mirror
    }

    fn patch(
        mirror: &mut ThreadMirror,
        base_revision: u64,
        revision: u64,
        patches: serde_json::Value,
    ) {
        mirror
            .apply(
                &lease(1, 1),
                &serde_json::json!({
                    "type": "patches",
                    "baseRevision": base_revision,
                    "revision": revision,
                    "patches": patches
                }),
            )
            .expect("patch");
    }

    /// 600 条 / 60 个 Turn（每 Turn 10 条）：尾窗 500 条饱和，dropped = 100。
    fn turns_000_to_599() -> Vec<(String, Vec<serde_json::Value>)> {
        (0..60)
            .map(|turn_index| {
                let items: Vec<serde_json::Value> = (0..10)
                    .map(|offset| {
                        let index = turn_index * 10 + offset;
                        agent(&format!("m{index:03}"), &format!("text-m{index:03}"))
                    })
                    .collect();
                (format!("t{}", turn_index + 1), items)
            })
            .collect()
    }

    /// 页边界：不含游标自身、canonical 顺序、连续翻页不重复不跳过；空历史与真正
    /// 到头返回 end。
    #[test]
    fn page_boundaries_are_contiguous_and_exclude_cursor() {
        let ids: Vec<String> = (0..600).map(|i| format!("m{i:03}")).collect();
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        let mirror = mirror_with_ids(&refs);
        let p1 = projection(&mirror);
        assert_eq!(p1.messages.len(), 500, "尾窗最多 500 条");
        assert_eq!(p1.messages[0].id, "m100");
        assert!(p1.truncated);
        assert!(!p1.history_token.is_empty(), "投影必须带历史代际 token");

        let cursor = p1.messages[0].id.clone();
        let first = page(&mirror, Some(cursor.as_str()), &p1.history_token);
        assert_eq!(first.status, MessagesPageStatus::Ready);
        assert_eq!(first.revision, Some(1));
        assert_eq!(first.reason_code, None);
        assert_eq!(
            message_ids(&first.messages),
            (50..100).map(|i| format!("m{i:03}")).collect::<Vec<_>>()
        );
        assert!(
            first.messages.iter().all(|m| m.id != cursor),
            "页不含游标自身"
        );
        assert_eq!(
            first.next_before.as_deref(),
            Some("m050"),
            "nextBefore 是页内最早条"
        );
        assert!(
            first.messages.windows(2).all(|w| w[0].id < w[1].id),
            "canonical 顺序（旧 → 新）"
        );

        // 连续翻页：直到 end；各页倒序拼接后必须与完整 canonical 历史逐条一致。
        let mut pages = vec![message_ids(&p1.messages), message_ids(&first.messages)];
        let (mut cursor, mut token) = (
            first.next_before.clone().expect("nextBefore"),
            first.history_token.clone(),
        );
        let mut ended = false;
        for _ in 0..16 {
            let next = page(&mirror, Some(cursor.as_str()), &token);
            if next.status == MessagesPageStatus::End {
                assert!(next.messages.is_empty(), "end 不带消息");
                assert_eq!(next.next_before, None);
                ended = true;
                break;
            }
            assert_eq!(next.status, MessagesPageStatus::Ready);
            assert!(!next.messages.is_empty());
            assert!(next.messages.iter().all(|m| m.id != cursor));
            assert_eq!(
                next.next_before.as_deref(),
                Some(next.messages[0].id.as_str())
            );
            pages.push(message_ids(&next.messages));
            cursor = next.next_before.clone().expect("nextBefore");
            token = next.history_token.clone();
        }
        assert!(ended, "600 条 / 每页 50 必然走到 end");
        let mut flattened = Vec::new();
        for page_ids in pages.iter().rev() {
            flattened.extend(page_ids.iter().cloned());
        }
        assert_eq!(
            flattened, ids,
            "连续翻页不重复、不跳过，覆盖完整 canonical 历史"
        );
    }

    /// 超 1 MiB 的单条：不得丢弃、不得伪装成完整页；截头 + 显式标记，且后续仍可翻页。
    #[test]
    fn page_single_oversized_message_is_trimmed_and_still_pages() {
        let original_chars = PROJECTION_TAIL_BYTES / 3 + 1000; // 3 B/char → > 1 MiB
        let big = "汉".repeat(original_chars);
        let state = state_of_items(serde_json::json!([
            { "type": "agentMessage", "id": "m0", "text": "small" },
            { "type": "agentMessage", "id": "big1", "text": big },
            { "type": "agentMessage", "id": "m2", "text": "after" },
            { "type": "agentMessage", "id": "m3", "text": "newest" }
        ]));
        let mut mirror = ThreadMirror::new(key());
        mirror.apply(&lease(1, 1), &snapshot(1, state)).unwrap();
        let p = projection(&mirror);
        // 尾窗只留得下最新两条（big1 自身超 1 MiB 被尾窗丢弃）——分页必须能取回它。
        assert_eq!(message_ids(&p.messages), ["m2", "m3"]);

        let first = page(&mirror, Some("m2"), &p.history_token);
        assert_eq!(first.status, MessagesPageStatus::Ready);
        assert_eq!(
            message_ids(&first.messages),
            ["big1"],
            "超大单条不得静默丢弃，也不得把旧条混进来冒充"
        );
        assert_eq!(
            first.reason_code.as_deref(),
            Some("page_truncated"),
            "内容被裁剪必须显式上报（不能假装完整）"
        );
        let text = &first.messages[0].text;
        assert!(
            text.starts_with("DISPLAY_TRUNCATED: "),
            "沿用展示截断标记: {text}"
        );
        assert!(text.ends_with(&format!("…已截断，原文共 {original_chars} 字符")));
        assert!(text.len() < big.len());
        assert!(
            serde_json::to_vec(&first.messages).unwrap().len() <= MESSAGES_PAGE_BYTES,
            "页 messages 序列化体积必须 ≤ 1 MiB"
        );
        assert_eq!(first.next_before.as_deref(), Some("big1"));

        // 仍能继续往前翻（省略只影响该条正文，不切断游标链）。
        let second = page(&mirror, Some("big1"), &first.history_token);
        assert_eq!(second.status, MessagesPageStatus::Ready);
        assert_eq!(message_ids(&second.messages), ["m0"]);
        assert_eq!(second.reason_code, None);
        let third = page(&mirror, Some("m0"), &second.history_token);
        assert_eq!(third.status, MessagesPageStatus::End);
    }

    /// 缺游标 / 游标不存在 / 部分历史 / 坏 token / 无镜像 / 镜像预算拒绝：都不是 end。
    #[test]
    fn page_statuses_cover_missing_cursor_partial_history_and_bad_token() {
        // 空历史（真实 0 条）+ 合法 token → end
        let empty = mirror_with_ids(&[]);
        let empty_token = projection(&empty).history_token;
        let end = page(&empty, None, &empty_token);
        assert_eq!(end.status, MessagesPageStatus::End);
        assert!(end.messages.is_empty());
        assert_eq!(end.next_before, None);

        // 非空历史 + 缺游标 → cursor_invalid/cursor_required（不猜最新页）
        let m = mirror_with_ids(&["m0", "m1"]);
        let token = projection(&m).history_token;
        let missing = page(&m, None, &token);
        assert_eq!(missing.status, MessagesPageStatus::CursorInvalid);
        assert_eq!(missing.reason_code.as_deref(), Some("cursor_required"));
        assert!(missing.messages.is_empty(), "非正常状态不能当完整空页");

        // 游标不存在 → cursor_not_found（不能解释为 end）
        let unknown = page(&m, Some("does-not-exist"), &token);
        assert_eq!(unknown.status, MessagesPageStatus::CursorInvalid);
        assert_eq!(unknown.reason_code.as_deref(), Some("cursor_not_found"));

        // 畸形 token → invalid_history_token
        let malformed = page(&m, Some("m1"), "not-a-token");
        assert_eq!(malformed.status, MessagesPageStatus::CursorInvalid);
        assert_eq!(
            malformed.reason_code.as_deref(),
            Some("invalid_history_token")
        );

        // 部分历史（turn 列表不完整）→ loading/history_incomplete，先不返回半页
        let mut partial_state = state_with_ids(&["m0", "m1"]);
        partial_state["turnHistory"]["history"]["isComplete"] = serde_json::json!(false);
        let mut partial = ThreadMirror::new(key());
        partial
            .apply(&lease(1, 1), &snapshot(1, partial_state))
            .unwrap();
        let loading = page(&partial, Some("m1"), &projection(&partial).history_token);
        assert_eq!(loading.status, MessagesPageStatus::Loading);
        assert_eq!(loading.reason_code.as_deref(), Some("history_incomplete"));
        assert!(loading.messages.is_empty());
        assert_eq!(loading.next_before, None);

        // 列表完整但 items 分页未载完 → 同样 loading（不拿残缺 items 当到头）
        let mut items_state = state_with_ids(&["m0", "m1"]);
        items_state["turnHistory"]["history"]["entitiesByKey"]["turn:t1"]["itemsPagination"] =
            serde_json::json!({ "hasLoadedOldest": false });
        let mut items = ThreadMirror::new(key());
        items
            .apply(&lease(1, 1), &snapshot(1, items_state))
            .unwrap();
        let loading = page(&items, Some("m1"), &projection(&items).history_token);
        assert_eq!(loading.status, MessagesPageStatus::Loading);
        assert_eq!(loading.reason_code.as_deref(), Some("history_incomplete"));

        // 无镜像（未跟随/已清态）→ unavailable/mirror_unavailable
        let bare = ThreadMirror::new(key());
        let unavailable = page(&bare, Some("m1"), "v1-x");
        assert_eq!(unavailable.status, MessagesPageStatus::Unavailable);
        assert_eq!(
            unavailable.reason_code.as_deref(),
            Some("mirror_unavailable")
        );
        assert_eq!(unavailable.revision, None);

        // 镜像字节预算拒绝 → unavailable/mirror_capacity，不得伪装成 end
        let mut budgeted =
            ThreadMirror::with_ledger(key(), Arc::new(StdMutex::new(MirrorLedger::with_cap(0))));
        assert!(budgeted
            .apply(&lease(1, 1), &snapshot(1, state_with_ids(&["m0"])))
            .is_err());
        let refused = page(&budgeted, None, "v1-x");
        assert_eq!(refused.status, MessagesPageStatus::Unavailable);
        assert_eq!(refused.reason_code.as_deref(), Some("mirror_capacity"));
    }

    /// token 隔离 owner/镜像重建与历史改写；普通尾部追加与流式补全不得使历史页失效。
    #[test]
    fn page_token_isolates_generations_and_rewrites_but_not_appends() {
        let ids: Vec<String> = (0..600).map(|i| format!("m{i:03}")).collect();
        let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        let mut mirror = mirror_with_ids_status(&refs, "inProgress");
        let p = projection(&mirror);
        let cursor = p.messages[0].id.clone();
        let token = p.history_token.clone();
        let baseline = page(&mirror, Some(cursor.as_str()), &token);
        assert_eq!(baseline.status, MessagesPageStatus::Ready);

        // 1) 普通尾部追加（patch add 一条 agentMessage）→ 前缀 id 序不变 → 页仍有效
        let append = serde_json::json!({
            "type": "patches", "baseRevision": 1, "revision": 2, "patches": [
                { "op": "add", "path": ["turnHistory", "history", "entitiesByKey", "turn:t1", "items", 600],
                  "value": { "type": "agentMessage", "id": "m600", "text": "tail" } }
            ]
        });
        mirror.apply(&lease(1, 1), &append).unwrap();
        let after_append = page(&mirror, Some(cursor.as_str()), &token);
        assert_eq!(
            after_append.status,
            MessagesPageStatus::Ready,
            "尾部追加不得使历史页无故失效"
        );
        assert_eq!(
            message_ids(&after_append.messages),
            message_ids(&baseline.messages)
        );

        // 2) 活动消息流式补全（同 id 文本原位变长，且必须是**当前末条** m600——
        //    活动豁免只覆盖末条；改写倒数第二条属于历史改写，必须失效）→ 页仍有效
        let stream = serde_json::json!({
            "type": "patches", "baseRevision": 2, "revision": 3, "patches": [
                { "op": "replace", "path": ["turnHistory", "history", "entitiesByKey", "turn:t1", "items", 600, "text"],
                  "value": "streaming completion".repeat(1000) }
            ]
        });
        mirror.apply(&lease(1, 1), &stream).unwrap();
        let after_stream = page(&mirror, Some(cursor.as_str()), &token);
        assert_eq!(
            after_stream.status,
            MessagesPageStatus::Ready,
            "活动消息流式补全不得使历史页无故失效"
        );
        assert_eq!(
            message_ids(&after_stream.messages),
            message_ids(&baseline.messages)
        );

        // 3) 已分页区域被改写（m050 换成新身份）→ 绑定该前缀的页 token 不能继续用。
        //    投影 token 是「首屏锚点」（v2 root），不绑 dropped 前缀；先取一页更近的
        //    历史，拿到绑定「m100 之前前缀」的页 token，再用它续翻。
        let seed = page(&mirror, Some("m150"), &token);
        assert_eq!(seed.status, MessagesPageStatus::Ready);
        assert_eq!(seed.next_before.as_deref(), Some("m100"));
        let prefix_token = seed.history_token.clone();
        let rewrite = serde_json::json!({
            "type": "patches", "baseRevision": 3, "revision": 4, "patches": [
                { "op": "replace", "path": ["turnHistory", "history", "entitiesByKey", "turn:t1", "items", 50, "id"],
                  "value": "m050-rewritten" }
            ]
        });
        mirror.apply(&lease(1, 1), &rewrite).unwrap();
        let stale = page(&mirror, Some(cursor.as_str()), &prefix_token);
        assert_eq!(stale.status, MessagesPageStatus::CursorInvalid);
        assert_eq!(stale.reason_code.as_deref(), Some("history_rewritten"));
        assert!(stale.messages.is_empty());

        // 4) owner 换代（epoch 2）→ 旧 token 的 generation 不再可用；新投影 token 可用
        let fresh_state = state_with_ids(&refs);
        mirror
            .apply(&lease(2, 1), &snapshot(5, fresh_state))
            .unwrap();
        let old = page(&mirror, Some(cursor.as_str()), &token);
        assert_eq!(old.status, MessagesPageStatus::Unavailable);
        assert_eq!(old.reason_code.as_deref(), Some("generation_mismatch"));
        let fresh = projection(&mirror);
        let fresh_page = page(
            &mirror,
            Some(fresh.messages[0].id.as_str()),
            &fresh.history_token,
        );
        assert_eq!(fresh_page.status, MessagesPageStatus::Ready);

        // 5) 镜像重建（同 lease 再次快照）→ 旧 token 同样失效
        let rebuilt = state_with_ids(&refs);
        mirror.apply(&lease(2, 1), &snapshot(6, rebuilt)).unwrap();
        let stale = page(
            &mirror,
            Some(fresh.messages[0].id.as_str()),
            &fresh.history_token,
        );
        assert_eq!(stale.status, MessagesPageStatus::Unavailable);
        assert_eq!(stale.reason_code.as_deref(), Some("generation_mismatch"));
    }

    // ---- P1/P2（token v2）：已结算内容的观察与尾窗前移解耦 ----

    /// 已结算 Turn 内**同 id** 文本原位改写（落在已取回的页里）：投影 token 必须换，
    /// 绑定该页前缀的页 token 不能继续。
    /// BASE 红：v1 token 只绑 id 前缀——同 id 文本改写既不换投影 token，也放行旧页。
    #[test]
    fn same_id_text_rewrite_in_settled_turn_invalidates_tokens() {
        let turns = turns_000_to_599();
        let mut mirror = mirror_of(state_of_turns(&turns));
        let before = projection(&mirror);
        assert_eq!(before.messages[0].id, "m100");
        let first = page(&mirror, Some("m100"), &before.history_token);
        assert_eq!(first.status, MessagesPageStatus::Ready);
        assert_eq!(first.next_before.as_deref(), Some("m050"));
        let page_token = first.history_token.clone();

        // t7（m060..m069）是已结算 Turn，且落在刚取回的页里；只改文本、id 不变。
        patch(
            &mut mirror,
            1,
            2,
            serde_json::json!([
                { "op": "replace",
                  "path": ["turnHistory", "history", "entitiesByKey", "turn:t7", "items", 0, "text"],
                  "value": "m060-rewritten" }
            ]),
        );

        // m060 不在 page_token 绑定的前缀 [m000..m049) 里——只有内容观察代能拦住它。
        // 先只走分页路径：分页本身必须观察到已结算内容变化（不依赖先调 project）。
        let stale = page(&mirror, first.next_before.as_deref(), &page_token);
        assert_eq!(stale.status, MessagesPageStatus::CursorInvalid);
        assert_eq!(stale.reason_code.as_deref(), Some("history_rewritten"));
        assert!(stale.messages.is_empty());

        let after = projection(&mirror);
        assert_ne!(
            after.history_token, before.history_token,
            "已结算 Turn 的同 id 文本改写必须换投影 token"
        );
    }

    /// 已结算 Turn 内同 id 附件变化（imageGeneration.savedPath）：投影 token 必须换，
    /// 已取回的页 token 不能继续。
    /// BASE 红：id 序不变 → 投影 token 不变、旧页照常续翻。
    #[test]
    fn same_id_artifact_change_in_settled_turn_invalidates_tokens() {
        let mut turns = turns_000_to_599();
        // t7 的第 6 条（全局 index 65，落在已取回的页里）改为图片生成卡。
        turns[6].1[5] = serde_json::json!({
            "type": "imageGeneration", "id": "img65",
            "savedPath": "/tmp/one.png", "status": "completed"
        });
        let mut mirror = mirror_of(state_of_turns(&turns));
        let before = projection(&mirror);
        let first = page(&mirror, Some("m100"), &before.history_token);
        assert_eq!(first.status, MessagesPageStatus::Ready);
        assert_eq!(first.next_before.as_deref(), Some("m050"));
        let page_token = first.history_token.clone();
        let paged = first
            .messages
            .iter()
            .find(|m| m.id == "img65")
            .expect("图片生成卡必须出现在已取回的页里");
        assert_eq!(
            paged.artifacts[0].path.as_deref(),
            Some("/tmp/one.png"),
            "夹具必须真的把 savedPath 投到 path"
        );

        patch(
            &mut mirror,
            1,
            2,
            serde_json::json!([
                { "op": "replace",
                  "path": ["turnHistory", "history", "entitiesByKey", "turn:t7", "items", 5, "savedPath"],
                  "value": "/tmp/two.png" }
            ]),
        );

        // img65 不在 page_token 绑定的前缀 [m000..m049) 里——只有内容观察代能拦住它。
        // 先只走分页路径：分页本身必须观察到已结算内容变化（不依赖先调 project）。
        let stale = page(&mirror, first.next_before.as_deref(), &page_token);
        assert_eq!(stale.status, MessagesPageStatus::CursorInvalid);
        assert_eq!(stale.reason_code.as_deref(), Some("history_rewritten"));
        assert!(stale.messages.is_empty());

        let after = projection(&mirror);
        assert_ne!(
            after.history_token, before.history_token,
            "已结算 Turn 的附件变化必须换投影 token"
        );
    }

    /// 删除已结算 Turn：投影 token 必须换。
    /// BASE 红：小夹具 dropped = 0，v1 token 只由 id 前缀决定，删除不换 token。
    #[test]
    fn deletion_of_settled_turn_changes_projection_token() {
        let mut mirror = mirror_of(state_of_turns(&[
            turn("t1", vec![user("m0", "q0"), agent("m1", "a0")]),
            turn("t2", vec![user("m2", "q1")]),
            turn("t3", vec![agent("m3", "a2")]),
        ]));
        let before = projection(&mirror);
        assert_eq!(message_ids(&before.messages), ["m0", "m1", "m2", "m3"]);

        // 删除 t1 的 canonical 引用（实体表孤儿不影响可见历史，引用链决定顺序）。
        patch(
            &mut mirror,
            1,
            2,
            serde_json::json!([
                { "op": "remove",
                  "path": ["turnHistory", "history", "islands", 0, "entries", 0] }
            ]),
        );

        let after = projection(&mirror);
        assert_eq!(
            message_ids(&after.messages),
            ["m2", "m3"],
            "删除的 Turn 不得继续出现在投影里"
        );
        assert_ne!(
            after.history_token, before.history_token,
            "删除已结算 Turn 必须换投影 token"
        );
    }

    /// 尾窗饱和后普通追加一个**新 Turn**：投影 token 不变，追加前的页 token 仍可
    /// 继续翻到更早一页。BASE 红：尾窗前移使 dropped 增大 → 投影 token 换。
    #[test]
    fn append_of_new_turn_after_tail_saturation_keeps_tokens() {
        let turns = turns_000_to_599();
        let mut mirror = mirror_of(state_of_turns(&turns));
        let before = projection(&mirror);
        assert_eq!(before.messages.len(), 500);
        assert_eq!(before.messages[0].id, "m100");
        let first = page(&mirror, Some("m100"), &before.history_token);
        assert_eq!(first.status, MessagesPageStatus::Ready);
        assert_eq!(first.next_before.as_deref(), Some("m050"));
        let page_token = first.history_token.clone();

        patch(
            &mut mirror,
            1,
            2,
            serde_json::json!([
                { "op": "add",
                  "path": ["turnHistory", "history", "entitiesByKey", "turn:t61"],
                  "value": { "turnId": "t61", "status": "inProgress",
                             "items": [{ "type": "agentMessage", "id": "m600", "text": "tail" }] } },
                { "op": "add",
                  "path": ["turnHistory", "history", "islands", 0, "entries", 60],
                  "value": { "key": "turn:t61", "value": "turn:t61" } }
            ]),
        );

        let after = projection(&mirror);
        assert_eq!(after.messages.len(), 500);
        assert_eq!(after.messages[0].id, "m101", "尾窗前移一条");
        assert_eq!(
            after.history_token, before.history_token,
            "普通新 Turn 追加（尾窗前移）不得换投影 token"
        );
        let next = page(&mirror, first.next_before.as_deref(), &page_token);
        assert_eq!(
            next.status,
            MessagesPageStatus::Ready,
            "追加前的页 token 必须仍可继续"
        );
        assert_eq!(
            message_ids(&next.messages),
            (0..50).map(|i| format!("m{i:03}")).collect::<Vec<_>>()
        );
    }

    /// 最新（**活动**，status = inProgress）Turn 的 agent 消息流式补全（同 id 文本
    /// 从空占位到有正文）不得换投影 token。活动身份是这条用例的前提：已结算 Turn
    /// 的末条改写必须换 token（见 `same_id_text_rewrite_in_completed_latest_turn...`）。
    /// BASE 红：消息变为可投影后 dropped +1 → 投影 token 换。
    #[test]
    fn streaming_growth_of_newest_agent_message_keeps_projection_token() {
        let turns = turns_000_to_599();
        let mut mirror = mirror_of(state_of_turns_last_active(&turns));
        let before = projection(&mirror);
        assert_eq!(before.messages[0].id, "m100");

        // 空文本占位不投影（既有语义），随后同 id 原位补全才出现。
        patch(
            &mut mirror,
            1,
            2,
            serde_json::json!([
                { "op": "add",
                  "path": ["turnHistory", "history", "entitiesByKey", "turn:t60", "items", 10],
                  "value": { "type": "agentMessage", "id": "stream", "text": "" } }
            ]),
        );
        let placeholder = projection(&mirror);
        assert!(!placeholder.messages.iter().any(|m| m.id == "stream"));
        assert_eq!(placeholder.history_token, before.history_token);

        patch(
            &mut mirror,
            2,
            3,
            serde_json::json!([
                { "op": "replace",
                  "path": ["turnHistory", "history", "entitiesByKey", "turn:t60", "items", 10, "text"],
                  "value": "流式补全的正文" }
            ]),
        );
        let after = projection(&mirror);
        assert!(after.messages.iter().any(|m| m.id == "stream"));
        assert_eq!(
            after.history_token, before.history_token,
            "活动 Turn 流式补全不得换投影 token"
        );
    }

    /// 仅尾窗前移（已结算内容不变，窗口从 500/100 变 500/150）：投影 token 逐字
    /// 相同。BASE 红：token 直接绑 dropped 前缀。
    #[test]
    fn tail_window_rollover_alone_keeps_projection_token() {
        let turns = turns_000_to_599();
        let mut mirror = mirror_of(state_of_turns(&turns));
        let before = projection(&mirror);
        assert_eq!(before.messages[0].id, "m100");
        assert!(before.truncated);

        let patches: Vec<serde_json::Value> = (0..50)
            .map(|offset| {
                serde_json::json!({
                    "op": "add",
                    "path": ["turnHistory", "history", "entitiesByKey", "turn:t60", "items", 10 + offset],
                    "value": { "type": "agentMessage", "id": format!("tail{offset:02}"), "text": "tail" }
                })
            })
            .collect();
        patch(&mut mirror, 1, 2, serde_json::Value::Array(patches));

        let after = projection(&mirror);
        assert_eq!(after.messages.len(), 500);
        assert_eq!(after.messages[0].id, "m150", "尾窗前移 50 条（内容未变）");
        assert_eq!(
            after.history_token, before.history_token,
            "仅尾窗前移不得换投影 token"
        );
    }

    // ---- 复核 F1/F4：最新 Turn 不再整体豁免；跨 Turn 顺序进入观察 ----

    /// 复核 F1：单 Turn 600 条、仍在产出时，**已分页**的 m050—m099 不是无人看守的
    /// 可变区——m099 同 id 正文改写必须换投影 token，并让绑定 m050 游标的页失效。
    /// BASE 红：整体排除最新 Turn → 内容代不动，旧页照常续翻并展示旧正文。
    #[test]
    fn same_id_text_rewrite_in_long_active_turn_invalidates_paged_history() {
        let ids: Vec<String> = (0..600).map(|i| format!("m{i:03}")).collect();
        let items: Vec<serde_json::Value> = ids
            .iter()
            .map(|id| agent(id, &format!("text-{id}")))
            .collect();
        let mut mirror = mirror_of(state_of_items_with_status(
            serde_json::json!(items),
            "inProgress",
        ));
        let before = projection(&mirror);
        assert_eq!(before.messages[0].id, "m100", "尾窗只保留最新 500 条");
        let first = page(&mirror, Some("m100"), &before.history_token);
        assert_eq!(first.status, MessagesPageStatus::Ready);
        assert_eq!(first.next_before.as_deref(), Some("m050"));
        assert_eq!(first.messages.last().map(|m| m.id.as_str()), Some("m099"));
        let page_token = first.history_token.clone();

        // m099 是该 Turn 的倒数第二条（末条是 m599），不在「活动末条」豁免内。
        patch(
            &mut mirror,
            1,
            2,
            serde_json::json!([
                { "op": "replace",
                  "path": ["turnHistory", "history", "entitiesByKey", "turn:t1", "items", 99, "text"],
                  "value": "m099-rewritten" }
            ]),
        );

        let after = projection(&mirror);
        assert_ne!(
            after.history_token, before.history_token,
            "单 Turn 内已分页内容被改写必须换投影 token"
        );
        let stale = page(&mirror, first.next_before.as_deref(), &page_token);
        assert_eq!(stale.status, MessagesPageStatus::CursorInvalid);
        assert_eq!(stale.reason_code.as_deref(), Some("history_rewritten"));
    }

    /// 复核 F1（附件面）：单 Turn 600 条、仍在产出时，已分页的图片生成卡
    /// `savedPath` 变化同样必须换 token（附件序列化进指纹）。
    /// BASE 红：整体排除最新 Turn → 附件改写观察不到。
    #[test]
    fn same_id_artifact_change_in_long_active_turn_invalidates_paged_history() {
        let ids: Vec<String> = (0..600).map(|i| format!("m{i:03}")).collect();
        let mut items: Vec<serde_json::Value> = ids
            .iter()
            .map(|id| agent(id, &format!("text-{id}")))
            .collect();
        items[99] = serde_json::json!({
            "type": "imageGeneration", "id": "m099",
            "savedPath": "/tmp/one.png", "status": "failed"
        });
        let mut mirror = mirror_of(state_of_items_with_status(
            serde_json::json!(items),
            "inProgress",
        ));
        let before = projection(&mirror);
        let first = page(&mirror, Some("m100"), &before.history_token);
        assert_eq!(first.status, MessagesPageStatus::Ready);
        let page_token = first.history_token.clone();
        let paged = first
            .messages
            .iter()
            .find(|m| m.id == "m099")
            .expect("图片生成卡必须在已取回的页里");
        assert_eq!(paged.artifacts[0].path.as_deref(), Some("/tmp/one.png"));

        patch(
            &mut mirror,
            1,
            2,
            serde_json::json!([
                { "op": "replace",
                  "path": ["turnHistory", "history", "entitiesByKey", "turn:t1", "items", 99, "savedPath"],
                  "value": "/tmp/two.png" }
            ]),
        );

        let after = projection(&mirror);
        assert_ne!(
            after.history_token, before.history_token,
            "单 Turn 内已分页附件的 savedPath 变化必须换投影 token"
        );
        let stale = page(&mirror, first.next_before.as_deref(), &page_token);
        assert_eq!(stale.status, MessagesPageStatus::CursorInvalid);
        assert_eq!(stale.reason_code.as_deref(), Some("history_rewritten"));
    }

    /// 复核 F1：**最新 Turn 已结算**时，其末条消息的同 id 改写也必须换 token——
    /// 流式豁免只该给仍在产出的 Turn。
    /// BASE 红：最新 Turn 整体不登记 → 末条改写观察不到。
    #[test]
    fn same_id_text_rewrite_in_completed_latest_turn_invalidates() {
        let turns = turns_000_to_599();
        let mut mirror = mirror_of(state_of_turns(&turns));
        let before = projection(&mirror);

        patch(
            &mut mirror,
            1,
            2,
            serde_json::json!([
                { "op": "replace",
                  "path": ["turnHistory", "history", "entitiesByKey", "turn:t60", "items", 9, "text"],
                  "value": "m599-rewritten" }
            ]),
        );

        let after = projection(&mirror);
        assert_ne!(
            after.history_token, before.history_token,
            "已结算的最新 Turn 末条改写必须换投影 token"
        );
        // 基线已跟上：再投影不得继续抖动（一次改写只推进一次内容代）。
        let again = projection(&mirror);
        assert_eq!(again.history_token, after.history_token);
    }

    /// 复核 F4：两个已结算 Turn 整体互换（各自内容与内部顺序都不变）必须换 token。
    /// BASE 红：`HashMap<turn, fingerprint>` 保不住顺序 → 观察集合相等，token 不变。
    #[test]
    fn reorder_of_settled_turns_changes_projection_token() {
        let mut mirror = mirror_of(state_of_turns(&[
            turn("t1", vec![user("m0", "q0")]),
            turn("t2", vec![agent("m1", "a0")]),
            turn("t3", vec![agent("m2", "a1")]),
        ]));
        let before = projection(&mirror);
        assert_eq!(message_ids(&before.messages), ["m0", "m1", "m2"]);

        patch(
            &mut mirror,
            1,
            2,
            serde_json::json!([
                { "op": "replace",
                  "path": ["turnHistory", "history", "islands", 0, "entries", 0],
                  "value": { "key": "turn:t2", "value": "turn:t2" } },
                { "op": "replace",
                  "path": ["turnHistory", "history", "islands", 0, "entries", 1],
                  "value": { "key": "turn:t1", "value": "turn:t1" } }
            ]),
        );

        let after = projection(&mirror);
        assert_eq!(message_ids(&after.messages), ["m1", "m0", "m2"]);
        assert_ne!(
            after.history_token, before.history_token,
            "已结算 Turn 整体换序必须换投影 token"
        );
    }

    /// 复核 G2：最新的 Turn **已失败**时，它的末条消息不是流式可变区——同 id 改写
    /// 必须换 token。「不是 completed」不等于「仍在输出」。
    /// BASE 红：非 completed 一律 active → 末条被豁免，改写观察不到。
    #[test]
    fn rewrite_of_failed_latest_turn_tail_invalidates_tokens() {
        let mut state = state_of_turns(&[
            turn("t1", vec![user("m0", "q0")]),
            turn("t2", vec![agent("m1", "a0")]),
        ]);
        override_turn_status(&mut state, &[("t2", Some("failed"))]);
        let mut mirror = mirror_of(state);
        let before = projection(&mirror);

        patch(
            &mut mirror,
            1,
            2,
            serde_json::json!([
                { "op": "replace",
                  "path": ["turnHistory", "history", "entitiesByKey", "turn:t2", "items", 0, "text"],
                  "value": "a0-rewritten" }
            ]),
        );

        let after = projection(&mirror);
        assert_ne!(
            after.history_token, before.history_token,
            "已失败的最新 Turn 末条改写必须换投影 token"
        );
    }

    /// 复核 G2：状态缺失的末条同样不得豁免（未知状态 ≠ 正在输出）。
    #[test]
    fn rewrite_of_statusless_latest_turn_tail_invalidates_tokens() {
        let mut state = state_of_turns(&[
            turn("t1", vec![user("m0", "q0")]),
            turn("t2", vec![agent("m1", "a0")]),
        ]);
        override_turn_status(&mut state, &[("t2", None)]);
        let mut mirror = mirror_of(state);
        let before = projection(&mirror);

        patch(
            &mut mirror,
            1,
            2,
            serde_json::json!([
                { "op": "replace",
                  "path": ["turnHistory", "history", "entitiesByKey", "turn:t2", "items", 0, "text"],
                  "value": "a0-rewritten" }
            ]),
        );

        let after = projection(&mirror);
        assert_ne!(
            after.history_token, before.history_token,
            "缺状态的最新 Turn 末条改写必须换投影 token"
        );
    }

    /// 复核 G3：末次增量与「追加下一条」同批到达时不得换 token（旧末条只豁免一次）。
    /// BASE 红：新末条是 m2 → m1 被纳入比对 → 正常结算增量触发换代、清空旧页。
    #[test]
    fn final_delta_with_appended_item_keeps_projection_token() {
        let mut mirror = mirror_of(state_of_items_with_status(
            serde_json::json!([
                { "type": "userMessage", "id": "m0",
                  "content": [{ "type": "text", "text": "q" }] },
                { "type": "agentMessage", "id": "m1", "text": "partial" }
            ]),
            "inProgress",
        ));
        let before = projection(&mirror);

        patch(
            &mut mirror,
            1,
            2,
            serde_json::json!([
                { "op": "replace",
                  "path": ["turnHistory", "history", "entitiesByKey", "turn:t1", "items", 1, "text"],
                  "value": "final text" },
                { "op": "add",
                  "path": ["turnHistory", "history", "entitiesByKey", "turn:t1", "items", 2],
                  "value": { "type": "agentMessage", "id": "m2", "text": "next" } }
            ]),
        );

        let after = projection(&mirror);
        assert_eq!(message_ids(&after.messages), ["m0", "m1", "m2"]);
        assert_eq!(
            after.history_token, before.history_token,
            "末次增量 + 追加下一条不得换投影 token"
        );
        assert_eq!(
            projection(&mirror).history_token,
            after.history_token,
            "基线必须跟上"
        );
    }

    /// 复核 G3：末次增量与 turn 结算同批到达时同样不得换 token。
    #[test]
    fn final_delta_with_turn_completion_keeps_projection_token() {
        let mut mirror = mirror_of(state_of_items_with_status(
            serde_json::json!([
                { "type": "userMessage", "id": "m0",
                  "content": [{ "type": "text", "text": "q" }] },
                { "type": "agentMessage", "id": "m1", "text": "partial" }
            ]),
            "inProgress",
        ));
        let before = projection(&mirror);

        patch(
            &mut mirror,
            1,
            2,
            serde_json::json!([
                { "op": "replace",
                  "path": ["turnHistory", "history", "entitiesByKey", "turn:t1", "status"],
                  "value": "completed" },
                { "op": "replace",
                  "path": ["turnHistory", "history", "entitiesByKey", "turn:t1", "items", 1, "text"],
                  "value": "final text" }
            ]),
        );

        let after = projection(&mirror);
        assert_eq!(
            after.history_token, before.history_token,
            "末次增量 + turn 结算不得换投影 token"
        );
        assert_eq!(
            projection(&mirror).history_token,
            after.history_token,
            "基线必须跟上"
        );
    }

    /// 复核 F1 补例：活动 Turn 内**删除**一条非末条消息必须换 token（id 序变短）。
    #[test]
    fn deletion_of_message_in_active_turn_changes_projection_token() {
        let mut mirror = mirror_of(state_of_items_with_status(
            serde_json::json!([
                { "type": "userMessage", "id": "m0",
                  "content": [{ "type": "text", "text": "q" }] },
                { "type": "agentMessage", "id": "m1", "text": "a1" },
                { "type": "agentMessage", "id": "m2", "text": "a2" }
            ]),
            "inProgress",
        ));
        let before = projection(&mirror);
        assert_eq!(message_ids(&before.messages), ["m0", "m1", "m2"]);

        patch(
            &mut mirror,
            1,
            2,
            serde_json::json!([
                { "op": "remove",
                  "path": ["turnHistory", "history", "entitiesByKey", "turn:t1", "items", 1] }
            ]),
        );

        let after = projection(&mirror);
        assert_eq!(message_ids(&after.messages), ["m0", "m2"]);
        assert_ne!(
            after.history_token, before.history_token,
            "活动 Turn 内删除消息必须换投影 token"
        );
    }

    /// 复核 F1 的反向守卫：活动 Turn 内**追加**一条新消息不得换 token（否则每个
    /// 新 item 都会清空用户刚翻出来的历史页）。
    #[test]
    fn append_within_active_turn_keeps_projection_token() {
        let mut mirror = mirror_of(state_of_items_with_status(
            serde_json::json!([
                { "type": "userMessage", "id": "m0",
                  "content": [{ "type": "text", "text": "q" }] },
                { "type": "agentMessage", "id": "m1", "text": "a1" }
            ]),
            "inProgress",
        ));
        let before = projection(&mirror);

        patch(
            &mut mirror,
            1,
            2,
            serde_json::json!([
                { "op": "add",
                  "path": ["turnHistory", "history", "entitiesByKey", "turn:t1", "items", 2],
                  "value": { "type": "agentMessage", "id": "m2", "text": "a2" } }
            ]),
        );

        let after = projection(&mirror);
        assert_eq!(message_ids(&after.messages), ["m0", "m1", "m2"]);
        assert_eq!(
            after.history_token, before.history_token,
            "活动 Turn 内追加新消息不得换投影 token"
        );
    }
}
