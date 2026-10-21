//! #172 强制未验证开关（`TUVIS_CODEX_IPC_FORCE_UNVERIFIED=1`）的**真机可跑性**回归。
//!
//! 为什么要单开一个测试二进制：开关是**进程级环境变量**，而本机 build 已 shipped
//! （4894）——只有强制压到「尚无证据」档才跑得到探测分支。把这条用例隔离成独立
//! 二进制（一个用例），环境变量不会污染其他测试文件里的连接/能力断言。
//!
//! 纪律边界：开关本身 `cfg!(debug_assertions)` 编译期门控，**release 构建恒 false**；
//! 本文件只在 debug 测试构建里跑。

use multi_agents_manager_lib::services::codex_desktop_ipc::adapter::{
    Connector, DesktopIpcAdapter,
};
use multi_agents_manager_lib::services::codex_desktop_ipc::codec::{encode_frame, FrameDecoder};
use multi_agents_manager_lib::services::codex_desktop_ipc::profile::profile_for;
use multi_agents_manager_lib::services::codex_desktop_ipc::transport::{
    BoxIo, DesktopEndpoint, IpcIo,
};
use multi_agents_manager_lib::services::codex_desktop_ipc::types::{ProtocolError, ThreadKey};
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

/// 已验证 build（shipped：默认**不会**探测的那一档）。
const MACOS_BUILD: &str = "26.1013.31256";
const THREAD: &str = "0f1e2d3c-aaaa-4222-8333-444455556666";

/// 假 Desktop 对端（与 `codex_desktop_ipc` 测试同构的最小版）。
struct Peer {
    io: DuplexStream,
    backlog: std::collections::VecDeque<serde_json::Value>,
}

impl Peer {
    fn new() -> (Box<dyn IpcIo>, Self) {
        let (client, server) = tokio::io::duplex(64 * 1024);
        (
            Box::new(client),
            Self {
                io: server,
                backlog: std::collections::VecDeque::new(),
            },
        )
    }

    async fn recv(&mut self) -> serde_json::Value {
        if let Some(frame) = self.backlog.pop_front() {
            return frame;
        }
        let mut decoder = FrameDecoder::new();
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            let n = self.io.read(&mut buf).await.expect("peer read");
            assert!(n > 0, "peer EOF");
            let mut frames = decoder.push(&buf[..n]).expect("decode");
            if frames.is_empty() {
                continue;
            }
            let first = frames.remove(0);
            for frame in frames {
                self.backlog.push_back(frame);
            }
            return first;
        }
    }

    async fn send(&mut self, value: &serde_json::Value) {
        let frame = encode_frame(value).expect("encode");
        self.io.write_all(&frame).await.expect("peer write");
    }
}

async fn expect_frame(peer: &mut Peer, what: &str) -> serde_json::Value {
    tokio::time::timeout(Duration::from_secs(5), peer.recv())
        .await
        .unwrap_or_else(|_| panic!("{what} 帧未在 5s 内到达"))
}

/// 连接器：endpoint = 已验证 build（默认 shipped 档）。
struct ShippedBuildConnector {
    io: StdMutex<Option<Box<dyn IpcIo>>>,
}

impl Connector for ShippedBuildConnector {
    fn connect_endpoint(
        &self,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = Result<(BoxIo, DesktopEndpoint), ProtocolError>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            let io = self
                .io
                .lock()
                .unwrap()
                .take()
                .ok_or_else(|| ProtocolError::Disconnected("fixture: 已连接过一次".into()))?;
            Ok((
                io,
                DesktopEndpoint {
                    platform: "macos".into(),
                    path: "probe-force-test".into(),
                    build: Some(MACOS_BUILD.into()),
                },
            ))
        })
    }
}

/// 环境变量守卫：用例结束（含 panic 展开）时清掉开关。
struct ForceGuard;
impl Drop for ForceGuard {
    fn drop(&mut self) {
        std::env::remove_var("TUVIS_CODEX_IPC_FORCE_UNVERIFIED");
    }
}

#[tokio::test]
async fn force_unverified_env_makes_shipped_build_probe() {
    std::env::set_var("TUVIS_CODEX_IPC_FORCE_UNVERIFIED", "1");
    let _guard = ForceGuard;

    let (io, mut peer) = Peer::new();
    let adapter = Arc::new(DesktopIpcAdapter::new(
        Arc::new(ShippedBuildConnector {
            io: StdMutex::new(Some(io)),
        }),
        // 构造期基线 = shipped 档；开关在**连接重算 profile** 时生效
        profile_for("macos", MACOS_BUILD).unwrap(),
    ));
    adapter.set_probe_budget(
        multi_agents_manager_lib::services::codex_desktop_ipc::probe::runner::ProbeBudget {
            p1: Duration::from_millis(500),
            p2: Duration::from_millis(200),
            p3: Duration::from_millis(150),
        },
    );

    let key = ThreadKey {
        host_id: "local".into(),
        thread_id: THREAD.into(),
    };
    let a = adapter.clone();
    let key_in_task = key.clone();
    // ensure_thread 的顺序：ensure_connection（此处按开关压到「尚无证据」档）→
    // ensure_read_evidence（探测）→ 生产 follow
    let task = tokio::spawn(async move { a.ensure_thread(&key_in_task).await });

    // initialize 握手
    let init = expect_frame(&mut peer, "initialize").await;
    assert_eq!(init["method"], "initialize");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": init["requestId"],
        "resultType": "success", "method": "initialize",
        "handledByClientId": "owner-1",
        "result": { "clientId": "owner-1" }
    }))
    .await;

    // 探针帧：**shipped build 也因为开关被探测**（这正是开关的用途）
    let disc = expect_frame(&mut peer, "probe discovery").await;
    assert_eq!(
        disc["method"], "thread-owner-discovery",
        "强制档必须走探测 P1"
    );
    assert_eq!(disc["params"]["sourceClientId"], "owner-1");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": disc["requestId"],
        "resultType": "success", "method": "thread-owner-discovery",
        "handledByClientId": "owner-1", "result": {}
    }))
    .await;
    let follow = expect_frame(&mut peer, "probe following").await;
    assert_eq!(follow["method"], "thread-stream-following-changed");
    assert_eq!(follow["params"]["following"], serde_json::json!(true));
    peer.send(&serde_json::json!({
        "type": "broadcast",
        "method": "thread-stream-state-changed",
        "params": {
            "conversationId": THREAD,
            "change": {
                "type": "snapshot",
                "revision": 11,
                "conversationState": {
                    "id": THREAD,
                    "turns": [],
                    "requests": [],
                    "turnHistory": { "kind": "canonical", "history": {} }
                }
            }
        }
    }))
    .await;
    let off = expect_frame(&mut peer, "probe unfollow").await;
    assert_eq!(off["params"]["following"], serde_json::json!(false));

    // 探测通过 → 内存 profile 的 state 已提升（接下来是生产 follow 的 discovery）
    let next = expect_frame(&mut peer, "post-probe follow").await;
    assert_eq!(
        next["method"], "thread-owner-discovery",
        "探测结束后继续生产 follow（探测不阻断读路径）"
    );
    assert!(
        adapter.capabilities().state,
        "强制档探测通过 → 读能力提升（本机 shipped build 也走了同一条证据链）"
    );
    task.abort();
    adapter.shutdown().await;
}
