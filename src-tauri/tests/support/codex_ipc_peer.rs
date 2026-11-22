//! 两个集成测试共享的内存 Desktop IPC 对端。

use multi_agents_manager_lib::services::codex_desktop_ipc::adapter::Connector;
use multi_agents_manager_lib::services::codex_desktop_ipc::codec::{encode_frame, FrameDecoder};
use multi_agents_manager_lib::services::codex_desktop_ipc::transport::{BoxIo, DesktopEndpoint};
use multi_agents_manager_lib::services::codex_desktop_ipc::types::ProtocolError;
use std::sync::Mutex as StdMutex;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

pub(super) struct ScriptedPeer {
    io: DuplexStream,
    backlog: std::collections::VecDeque<serde_json::Value>,
}

impl ScriptedPeer {
    pub(super) fn new() -> (BoxIo, Self) {
        let (client, server) = tokio::io::duplex(64 * 1024);
        (Box::new(client), Self::from_stream(server))
    }

    pub(super) fn from_stream(io: DuplexStream) -> Self {
        Self {
            io,
            backlog: std::collections::VecDeque::new(),
        }
    }

    pub(super) async fn recv(&mut self) -> serde_json::Value {
        if let Some(f) = self.backlog.pop_front() {
            return f;
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
            for f in frames {
                self.backlog.push_back(f);
            }
            return first;
        }
    }

    pub(super) async fn send(&mut self, value: &serde_json::Value) {
        let frame = encode_frame(value).expect("encode");
        self.io.write_all(&frame).await.expect("peer write");
    }

    #[allow(dead_code, reason = "仅 IPC 集成套件使用")]
    pub(super) fn close(self) {
        drop(self.io);
    }

    /// 对端断链（EOF）——**保留 peer 本体**：断链后仍可断言「没有第二帧」
    /// （`close` 消费 self，断链后就没法再读帧了）。
    #[allow(dead_code, reason = "仅 IPC 集成套件使用")]
    pub(super) fn eof(&mut self) {
        let (client, server) = tokio::io::duplex(64 * 1024);
        drop(server); // 对端消失 → 本端读到 EOF
        self.io = client; // 旧 io 随赋值丢弃 → 适配器那侧同步见 EOF
    }

    /// 有界收帧（容忍对端 EOF：连接已关时读到 EOF = 此后不会再有任何帧；用于
    /// `hub.stop()` 之后的「零 Invoke」断言，不让 EOF 伪装成测试骨架崩溃）。
    #[allow(dead_code, reason = "仅问答跳过集成套件使用")]
    pub(super) async fn recv_or_eof(&mut self) -> Option<serde_json::Value> {
        if let Some(f) = self.backlog.pop_front() {
            return Some(f);
        }
        let mut decoder = FrameDecoder::new();
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            match tokio::time::timeout(Duration::from_millis(150), self.io.read(&mut buf)).await {
                Err(_) | Ok(Err(_)) | Ok(Ok(0)) => return None,
                Ok(Ok(n)) => {
                    let mut frames = decoder.push(&buf[..n]).ok()?;
                    if frames.is_empty() {
                        continue;
                    }
                    let first = frames.remove(0);
                    for f in frames {
                        self.backlog.push_back(f);
                    }
                    return Some(first);
                }
            }
        }
    }

    #[allow(dead_code, reason = "仅 IPC 集成套件检查原始读结果")]
    pub(super) async fn read_bytes(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.io.read(buf).await
    }

    #[allow(dead_code, reason = "仅 IPC 集成套件重新放回已读帧")]
    pub(super) fn unread_frame(&mut self, frame: serde_json::Value) {
        self.backlog.push_front(frame);
    }

    #[allow(dead_code, reason = "仅 IPC 集成套件断言合并读取的额外帧")]
    pub(super) fn backlog_is_empty(&self) -> bool {
        self.backlog.is_empty()
    }
}

pub(super) struct StaticConnector {
    endpoint: DesktopEndpoint,
    io: StdMutex<Option<BoxIo>>,
}

/// R3 起 adapter 每次连接按 endpoint 重算 profile——假 Desktop 必须自报**已验证
/// build** 的 endpoint 身份，能力由生产 `profile_for(platform, build)` 派生（绝不注入
/// 固定 profile 替代 resolver 链）。选 windows 已验证 build：send+state+command+
/// blocking_input 全开（本套件的审批/作答分派链全覆盖；permissions 未实测档与本
/// 套件无分派交集）。
pub(super) const TEST_BUILD: &str = "26.1013.3656.0";

impl StaticConnector {
    pub(super) fn new(io: BoxIo) -> Self {
        Self::with_build(io, TEST_BUILD)
    }

    /// 自报 build 身份（未知 build 用例用）：能力由生产 `profile_for` 派生——
    /// 未命中 shipped 表 → 「尚无证据」档（写能力未取证）。
    pub(super) fn with_build(io: BoxIo, build: &str) -> Self {
        Self {
            endpoint: DesktopEndpoint {
                platform: "windows".into(),
                path: "\\\\.\\pipe\\codex-ipc-test-fixture".into(),
                build: Some(build.into()),
            },
            io: StdMutex::new(Some(io)),
        }
    }

    /// 打断能力 fixture 用：自报**已开启 `interrupt` 的**已验证 build
    /// （macOS `26.1013.40934`——2026-10-21 真机协议取证：直连真实 Desktop 的
    /// `thread-follower-interrupt-turn` v4，回包 `{interruptedTurnId:null, ok:true}`
    /// 且安全阀生效未打断任何回合 → 方法名/版本/参数形态/owner 路由全对）。
    /// 能力由生产 `profile_for(platform, build)` 派生，**不注入固定 profile**。
    #[allow(dead_code, reason = "仅 IPC 集成套件使用")]
    pub(super) fn new_interrupt(io: BoxIo) -> Self {
        Self {
            endpoint: DesktopEndpoint {
                platform: "macos".into(),
                path: "/tmp/codex-ipc-test-fixture.sock".into(),
                build: Some("26.1013.40934".into()),
            },
            io: StdMutex::new(Some(io)),
        }
    }

    #[allow(dead_code, reason = "仅 IPC 集成套件的重连用例使用")]
    pub(super) fn replace_io(&self, io: BoxIo) {
        *self.io.lock().unwrap() = Some(io);
    }
}

impl Connector for StaticConnector {
    fn connect_endpoint(
        &self,
    ) -> std::pin::Pin<
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
                .expect("lock")
                .take()
                .ok_or_else(|| ProtocolError::Disconnected("no io".into()))?;
            Ok((io, self.endpoint.clone()))
        })
    }
}

pub(super) fn discovery_response(request_id: &str, owner: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "response", "requestId": request_id,
        "resultType": "success", "method": "thread-owner-discovery",
        "handledByClientId": owner,
        "result": { "supportsUntrustedAppInput": true }
    })
}

/// 有界探测 peer 是否收到任何字节（true = 有流量；「零写入」断言要求 false）。
pub(super) async fn peer_has_traffic(peer: &mut ScriptedPeer) -> bool {
    let mut buf = vec![0u8; 1024];
    matches!(
        tokio::time::timeout(Duration::from_millis(300), peer.io.read(&mut buf)).await,
        Ok(Ok(n)) if n > 0
    )
}

#[allow(dead_code, reason = "仅 IPC 集成套件断言不重发 start-turn")]
/// peer 后续还会收到的 `thread-follower-start-turn` 帧数（backlog + 新帧，读到
/// EOF/超时为止）——「不得重发」的帧数断言。
pub(super) async fn count_start_turn_frames(peer: &mut ScriptedPeer) -> usize {
    let mut count = 0;
    for f in peer.backlog.drain(..) {
        if f["method"] == "thread-follower-start-turn" {
            count += 1;
        }
    }
    let mut decoder = FrameDecoder::new();
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = match tokio::time::timeout(Duration::from_millis(300), peer.io.read(&mut buf)).await
        {
            Ok(Ok(n)) => n,
            _ => break,
        };
        if n == 0 {
            break; // EOF
        }
        for f in decoder.push(&buf[..n]).expect("decode") {
            if f["method"] == "thread-follower-start-turn" {
                count += 1;
            }
        }
    }
    count
}
