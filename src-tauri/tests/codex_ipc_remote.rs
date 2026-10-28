//! Codex Desktop IPC 远程控制（计划 Task 8）HTTP 集成测试：认证后的新端点、
//! session-send/send-info 的 hub 接线、兼容兜底 H3 门、审计与生命周期。
//!
//! 零真实副作用纪律：`DeviceStore::Owned(共享内存连接)`（state.store 与 hub 的
//! ActionStore 同库同连接）、Desktop IPC 走 Tokio duplex `ScriptedPeer`（本地假
//! Desktop，**绝不连接真实官方 Desktop**）、home_source 恒 None（rollout 解析只走
//! 会话号 UUID 回退，**绝不读真实 ~/.codex**）；兜底路径的 CLI 发现经临时空 PATH
//! 隔离（**绝不 spawn 真 codex**）。

use multi_agents_manager_lib::remote::codex_ipc::CodexControlHub;

#[path = "support/codex_fixture.rs"]
mod codex_fixture;
mod support;
use multi_agents_manager_lib::database::dao::codex_ipc_probe::ProbeState;
use multi_agents_manager_lib::remote::pairing::{
    persist_device, revoke_device, DeviceStore, NewDevice,
};
use multi_agents_manager_lib::remote::server::{router, RemoteState};
use multi_agents_manager_lib::services::codex_control::{
    payload_hash, ActionContext, ActionKind, ActionPayload, ActionReceipt, ActionState,
    ActionStore, ControlRoute, Delivery, ThreadKey,
};
use multi_agents_manager_lib::services::codex_desktop_ipc::adapter::{
    Connector, DesktopIpcAdapter,
};
use multi_agents_manager_lib::services::codex_desktop_ipc::codec::{encode_frame, FrameDecoder};
use multi_agents_manager_lib::services::codex_desktop_ipc::probe::runner::ProbeOutcome;
use multi_agents_manager_lib::services::codex_desktop_ipc::probe::{ProbeLedger, ProbeLedgerEntry};
use multi_agents_manager_lib::services::codex_desktop_ipc::profile::{
    profile_for, ProtocolProfile,
};
use multi_agents_manager_lib::services::codex_desktop_ipc::transport::{BoxIo, IpcIo};
use multi_agents_manager_lib::session::{
    AgentType, ProcessForm, Session, SessionStatus, SessionsResponse,
};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tower::ServiceExt as _;

// ============================================================
// 假 Desktop IPC：duplex 帧级 peer（与 codex_desktop_ipc 测试同构）
// ============================================================

struct ScriptedPeer {
    io: DuplexStream,
    backlog: std::collections::VecDeque<serde_json::Value>,
}

impl ScriptedPeer {
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

    async fn send(&mut self, value: &serde_json::Value) {
        let frame = encode_frame(value).expect("encode");
        self.io.write_all(&frame).await.expect("peer write");
    }

    fn close(self) {
        drop(self.io);
    }

    /// 对端断链（EOF）——**保留 peer 本体**：断链后仍可断言「没有第二帧」
    /// （`close` 消费 self，断链后就没法再读帧了）。
    fn eof(&mut self) {
        let (client, server) = tokio::io::duplex(64 * 1024);
        drop(server); // 对端消失 → 本端读到 EOF
        self.io = client; // 旧 io 随赋值丢弃 → 适配器那侧同步见 EOF
    }
}

struct StaticConnector {
    endpoint: multi_agents_manager_lib::services::codex_desktop_ipc::transport::DesktopEndpoint,
    io: StdMutex<Option<Box<dyn IpcIo>>>,
}

/// R3 起 adapter 每次连接按 endpoint 重算 profile——假 Desktop 必须自报**已验证
/// build** 的 endpoint 身份，能力由生产 `profile_for(platform, build)` 派生（绝不注入
/// 固定 profile 替代 resolver 链）。选 windows 已验证 build：send+state+command+
/// blocking_input 全开（本套件的审批/作答分派链全覆盖；permissions 未实测档与本
/// 套件无分派交集）。
const TEST_BUILD: &str = "26.1013.3656.0";

impl StaticConnector {
    fn new(io: Box<dyn IpcIo>) -> Self {
        Self::with_build(io, TEST_BUILD)
    }

    /// 自报 build 身份（未知 build 用例用）：能力由生产 `profile_for` 派生——
    /// 未命中 shipped 表 → 「尚无证据」档（写能力未取证）。
    fn with_build(io: Box<dyn IpcIo>, build: &str) -> Self {
        Self {
            endpoint:
                multi_agents_manager_lib::services::codex_desktop_ipc::transport::DesktopEndpoint {
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
    fn new_interrupt(io: Box<dyn IpcIo>) -> Self {
        Self {
            endpoint:
                multi_agents_manager_lib::services::codex_desktop_ipc::transport::DesktopEndpoint {
                    platform: "macos".into(),
                    path: "/tmp/codex-ipc-test-fixture.sock".into(),
                    build: Some("26.1013.40934".into()),
                },
            io: StdMutex::new(Some(io)),
        }
    }
}

impl Connector for StaticConnector {
    fn connect_endpoint(
        &self,
    ) -> std::pin::Pin<
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

/// 构造期基线 profile（R3：仅首个连接前生效，连接后按 endpoint 重算——同值）。
fn caps_open_profile(
) -> multi_agents_manager_lib::services::codex_desktop_ipc::profile::ProtocolProfile {
    profile_for("windows", TEST_BUILD).unwrap()
}

async fn handshake(peer: &mut ScriptedPeer, client_id: &str) {
    let init = peer.recv().await;
    assert_eq!(init["method"], "initialize");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": init["requestId"],
        "resultType": "success", "method": "initialize",
        "handledByClientId": client_id, "result": { "clientId": client_id }
    }))
    .await;
}

fn discovery_response(request_id: &str, owner: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "response", "requestId": request_id,
        "resultType": "success", "method": "thread-owner-discovery",
        "handledByClientId": owner,
        "result": { "supportsUntrustedAppInput": true }
    })
}

/// 驱动 ensure_thread 全程：initialize → discovery → following → snapshot。
/// `runtime` 控制忙态（active = busy 留队 / idle = 可立即派发）。
async fn drive_ensure(
    peer: &mut ScriptedPeer,
    thread: &str,
    requests: Vec<serde_json::Value>,
    runtime: &str,
) {
    handshake(peer, "owner-1").await;
    let disc = peer.recv().await;
    assert_eq!(disc["method"], "thread-owner-discovery");
    peer.send(&discovery_response(
        disc["requestId"].as_str().unwrap(),
        "owner-1",
    ))
    .await;
    let follow = peer.recv().await;
    assert_eq!(follow["method"], "thread-stream-following-changed");
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": thread, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": 7,
                "conversationState": {
                    "turns": [], "requests": requests,
                    "threadRuntimeStatus": { "type": runtime },
                    "turnHistory": codex_fixture::canonical_turn_history(serde_json::json!({
                            "t1": { "id": "t1", "type": "turn", "status": "completed", "items": ["u1"] },
                            "u1": { "id": "u1", "type": "user_message", "text": "seed", "ts": 1 }
                        }), serde_json::json!([{ "entries": ["t1"] }]))
                }
            }
        }
    }))
    .await;
}

/// 有界探测 peer 是否收到任何字节（true = 有流量；「零写入」断言要求 false）。
async fn peer_has_traffic(peer: &mut ScriptedPeer) -> bool {
    let mut buf = vec![0u8; 1024];
    matches!(
        tokio::time::timeout(Duration::from_millis(300), peer.io.read(&mut buf)).await,
        Ok(Ok(n)) if n > 0
    )
}

/// 排空当前已到达的帧（有界）；用于"没有某类写帧"的断言（握手/快照帧不算）。
async fn drain_frames(peer: &mut ScriptedPeer) -> Vec<serde_json::Value> {
    let mut frames = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_millis(200), peer.recv()).await {
            Ok(frame) => frames.push(frame),
            Err(_) => return frames,
        }
    }
}

/// 有界收帧（打断端点用例专用）：端点若因能力门/上下文拒发则**不会出帧**——
/// 裸 `recv()` 会永久挂住整个测试二进制，这里给上限，超时即失败（不掩盖问题）。
async fn recv_within(peer: &mut ScriptedPeer, secs: u64) -> serde_json::Value {
    tokio::time::timeout(Duration::from_secs(secs), peer.recv())
        .await
        .expect("打断端点未在时限内发出 IPC 帧（应发帧或明确拒发）")
}

// ============================================================
// RemoteState fixture
// ============================================================

type SharedConn = Arc<StdMutex<rusqlite::Connection>>;

fn shared_conn() -> SharedConn {
    let conn = rusqlite::Connection::open_in_memory().expect("open memory db");
    multi_agents_manager_lib::database::schema::init(&conn);
    multi_agents_manager_lib::database::migration::migrate(&conn).expect("migrate");
    Arc::new(StdMutex::new(conn))
}

fn seed_device(conn: &SharedConn, id: &str, name: &str) {
    let dev = NewDevice {
        id: id.to_string(),
        name: name.to_string(),
        ua: format!("ua-{id}"),
        origin_ip: format!("127.0.0.1#{id}"),
        via: "lan".into(),
        paired_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0),
    };
    let store = DeviceStore::Owned(conn.clone());
    store.with(|c| persist_device(c, &dev).expect("seed device"));
}

/// codex APP 会话（id 必须 UUID 形态——thread 解析的回退来源）。
fn codex_app_session(id: &str) -> Session {
    Session {
        id: id.into(),
        agent_type: AgentType::Codex,
        project_name: "proj".into(),
        project_path: "/tmp/proj".into(),
        title: None,
        git_branch: None,
        github_url: None,
        status: SessionStatus::Idle,
        last_message: None,
        last_message_role: None,
        last_message_subagent_report: false,
        flap_from_subagent_activity: false,
        last_activity_at: "2026-10-16T00:00:00Z".into(),
        pid: 4242,
        cpu_usage: 0.0,
        active_subagent_count: 0,
        form: ProcessForm::App,
        jump_supported: false,
        unread: false,
    }
}

/// 进程级 PATH 隔离：涉及可能触达本机 CLI 的用例期间 PATH 指向空目录（无 codex CLI
/// → 拒发不 spawn）。互斥锁串行化本文件内相关用例，Drop 恢复原 PATH。
/// （S2：兜底 bridge 已删；仍用于「未知 build 离线排队」等需要 PATH 隔离的用例。）
struct IsolatedPath {
    _g: std::sync::MutexGuard<'static, ()>,
    orig: Option<std::ffi::OsString>,
}

impl IsolatedPath {
    fn lock_empty() -> Self {
        static LOCK: StdMutex<()> = StdMutex::new(());
        static ORIG: std::sync::OnceLock<std::ffi::OsString> = std::sync::OnceLock::new();
        let g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let orig = ORIG
            .get_or_init(|| std::env::var_os("PATH").unwrap_or_default())
            .clone();
        std::env::set_var("PATH", "/nonexistent-codex-ipc-test-path");
        Self {
            _g: g,
            orig: Some(orig),
        }
    }
}

impl Drop for IsolatedPath {
    fn drop(&mut self) {
        if let Some(p) = self.orig.take() {
            std::env::set_var("PATH", p);
        }
    }
}

fn claude_cli_session(id: &str, status: SessionStatus) -> Session {
    Session {
        agent_type: AgentType::Claude,
        status,
        pid: 7,
        form: ProcessForm::Cli,
        ..codex_app_session(id)
    }
}

/// rollout 问答形态的消息页（补投通道 B 判据：tool-call + questions 形态 args + 无 result）。
fn question_message_page(
    _session_id: &str,
) -> multi_agents_manager_lib::remote::content::MessagesPage {
    multi_agents_manager_lib::remote::content::MessagesPage {
        messages: vec![multi_agents_manager_lib::remote::content::SessionMessage {
            seq: 0,
            role: "assistant".into(),
            kind: "tool-call".into(),
            content: "request_user_input".into(),
            ts: Some(1),
            tool_name: Some("request_user_input".into()),
            tool_args: Some(
                r#"{"questions":[{"question":"补投题面","options":[{"label":"甲"},{"label":"乙"}]}]}"#
                    .into(),
            ),
            collapsed: false,
        }],
        truncated: false,
        cwd: None,
    }
}

struct Fixture {
    conn: SharedConn,
    state: Arc<RemoteState>,
    app: axum::Router,
    peer: Option<ScriptedPeer>,
}

/// hub 可用（A 可写）fixture：注入 ScriptedPeer 假 IPC 的真 adapter。
fn fixture_ipc(sessions: Vec<Session>) -> Fixture {
    let (io, peer) = ScriptedPeer::new();
    let adapter = Arc::new(DesktopIpcAdapter::new(
        Arc::new(StaticConnector::new(io)),
        caps_open_profile(),
    ));
    fixture_with(
        sessions,
        move |store| Arc::new(CodexControlHub::with_adapter(store, adapter, true)),
        Some(peer),
    )
}

/// T04：未知 build（静态 `blocking_input=false`）+ **当前包契约匹配**——
/// 阻塞问答应经契约放行（不再是"整个 build 未登记 → 一律只读"）。
fn fixture_ipc_unknown_blocking(sessions: Vec<Session>) -> Fixture {
    let (io, peer) = ScriptedPeer::new();
    let adapter = Arc::new(DesktopIpcAdapter::new(
        Arc::new(StaticConnector::new(io)),
        multi_agents_manager_lib::services::codex_desktop_ipc::profile::ProtocolProfile::unverified(
            "macos",
            "26.1021.31345",
        ),
    ));
    let mut matches = std::collections::BTreeMap::new();
    matches.insert(
        multi_agents_manager_lib::services::codex_desktop_ipc::capability::CodexOperation::BlockingInput,
        multi_agents_manager_lib::services::codex_desktop_ipc::capability::ContractMatch {
            method: "thread-follower-submit-user-input".into(),
            base_version: 1,
            request_rule: "conversationId+requestId+response".into(),
            response_rule: "ok".into(),
            semantic_rule: "blocking user-input v1".into(),
        },
    );
    adapter.set_contract_evidence_snapshot(
        multi_agents_manager_lib::services::codex_desktop_ipc::capability::DesktopContractEvidence {
            probe_key: "macos|26.1021.31345|cli|1".into(),
            package_sha256: "a".repeat(64),
            rule_version: multi_agents_manager_lib::services::codex_desktop_ipc::capability::CONTRACT_RULE_VERSION,
            matches,
            package_size: None,
            package_mtime_ms: None,
        },
    );
    fixture_with(
        sessions,
        move |store| Arc::new(CodexControlHub::with_adapter(store, adapter, true)),
        Some(peer),
    )
}

/// 构造并安装 interrupt v4 契约证据（绑定**当前**连接代际与目标）。
fn install_interrupt_contract(f: &Fixture) {
    use multi_agents_manager_lib::services::codex_desktop_ipc::capability::{
        CodexOperation, ContractMatch, DesktopContractEvidence, CONTRACT_RULE_VERSION,
    };
    let mut matches = std::collections::BTreeMap::new();
    matches.insert(
        CodexOperation::Interrupt,
        ContractMatch {
            method: "thread-follower-interrupt-turn".into(),
            base_version: 4,
            request_rule: "conversationId+mode+expectedTurnId".into(),
            response_rule: "interruptedTurnId+ok".into(),
            semantic_rule: "interrupt v4 requires expectedTurnId".into(),
        },
    );
    f.state
        .codex_control
        .adapter
        .set_contract_evidence_snapshot(DesktopContractEvidence {
            probe_key: "macos|26.1021.31345|cli|1".into(),
            package_sha256: "b".repeat(64),
            rule_version: CONTRACT_RULE_VERSION,
            matches,
            package_size: None,
            package_mtime_ms: None,
        });
}

/// T06：未知 build（静态 `interrupt=false`）+ **当前包契约匹配**（interrupt v4）——
/// 定向打断应经契约放行。`with_contract=false` 时同一 profile 无任何证据（关闭）。
fn fixture_ipc_unknown_interrupt(sessions: Vec<Session>, with_contract: bool) -> Fixture {
    use multi_agents_manager_lib::services::codex_desktop_ipc::capability::{
        CodexOperation, ContractMatch, DesktopContractEvidence, CONTRACT_RULE_VERSION,
    };
    let (io, peer) = ScriptedPeer::new();
    let adapter = Arc::new(DesktopIpcAdapter::new(
        Arc::new(StaticConnector::new(io)),
        multi_agents_manager_lib::services::codex_desktop_ipc::profile::ProtocolProfile::unverified(
            "macos",
            "26.1021.31345",
        ),
    ));
    if with_contract {
        let mut matches = std::collections::BTreeMap::new();
        matches.insert(
            CodexOperation::Interrupt,
            ContractMatch {
                method: "thread-follower-interrupt-turn".into(),
                base_version: 4,
                request_rule: "conversationId+mode+expectedTurnId".into(),
                response_rule: "interruptedTurnId+ok".into(),
                semantic_rule: "interrupt v4 requires expectedTurnId".into(),
            },
        );
        adapter.set_contract_evidence_snapshot(DesktopContractEvidence {
            probe_key: "macos|26.1021.31345|cli|1".into(),
            package_sha256: "b".repeat(64),
            rule_version: CONTRACT_RULE_VERSION,
            matches,
            package_size: None,
            package_mtime_ms: None,
        });
    }
    fixture_with(
        sessions,
        move |store| {
            // 与生产装配同源：自动适配门读**本 hub 的库**（活值）。缺这个注入，
            // 适配器会退化成"恒开"，移动端关闭开关就测不出撤回（T08）。
            let gate_store = store.clone();
            adapter.set_auto_adapt_gate(Some(Arc::new(move || {
                gate_store.with(multi_agents_manager_lib::remote::auto_adapt_enabled_conn)
            })));
            Arc::new(CodexControlHub::with_adapter(store, adapter, true))
        },
        Some(peer),
    )
}

/// G01：未知 build（可通过开关从只读档切到可用档）。
fn fixture_ipc_unknown_build(sessions: Vec<Session>) -> Fixture {
    let (io, peer) = ScriptedPeer::new();
    let adapter = Arc::new(DesktopIpcAdapter::new(
        Arc::new(StaticConnector::with_build(io, "26.9999.1")),
        multi_agents_manager_lib::services::codex_desktop_ipc::profile::ProtocolProfile::unverified(
            "windows",
            "26.9999.1",
        ),
    ));
    fixture_with(
        sessions,
        move |store| {
            let gate_store = store.clone();
            adapter.set_auto_adapt_gate(Some(Arc::new(move || {
                gate_store.with(multi_agents_manager_lib::remote::auto_adapt_enabled_conn)
            })));
            Arc::new(CodexControlHub::with_adapter(store, adapter, true))
        },
        Some(peer),
    )
}

/// 打断能力开启的 fixture（2026-10-21 新增能力位 `interrupt`——能力由 endpoint
/// 自报 build 派生，故此处假 Desktop 自报已开启打断的 macOS build；未开启的 build
/// 上打断端点必须 503 拒发）。
fn fixture_ipc_interrupt(sessions: Vec<Session>) -> Fixture {
    let (io, peer) = ScriptedPeer::new();
    let adapter = Arc::new(DesktopIpcAdapter::new(
        Arc::new(StaticConnector::new_interrupt(io)),
        caps_open_profile(),
    ));
    fixture_with(
        sessions,
        move |store| Arc::new(CodexControlHub::with_adapter(store, adapter, true)),
        Some(peer),
    )
}

/// hub 不可用 fixture（disabled：一切能力关闭）。R8 起普通消息仍经 router（登记→
/// reserve→NotSent→兜底资格），故 hub 的 ActionStore 必须与 state.store 同库——
/// 设备复核才能放行到「兜底拒发」臂。
fn fixture_disabled(sessions: Vec<Session>) -> Fixture {
    fixture_with(
        sessions,
        |store| Arc::new(CodexControlHub::disabled_with_store(store)),
        None,
    )
}

fn fixture_with(
    sessions: Vec<Session>,
    hub_builder: impl FnOnce(DeviceStore) -> Arc<CodexControlHub>,
    peer: Option<ScriptedPeer>,
) -> Fixture {
    let conn = shared_conn();
    seed_device(&conn, "dev-1", "iPhone");
    fixture_on(conn, sessions, hub_builder, peer)
}

/// 指定共享内存库的 fixture（R7.1 重启恢复测试用：两阶段 hub 同库——「丢弃全部
/// 内存重建 hub，旧 queued 行在恢复后按序发出」）。conn 须已完成 seed_device。
fn fixture_on(
    conn: SharedConn,
    sessions: Vec<Session>,
    hub_builder: impl FnOnce(DeviceStore) -> Arc<CodexControlHub>,
    peer: Option<ScriptedPeer>,
) -> Fixture {
    // 把 HOME/全局库重定向到可写 temp（沙箱内也不再触碰真实 ~/.tuvis）。
    support::setup();
    let store = DeviceStore::Owned(conn.clone());
    let has_question_page = sessions.iter().any(|s| s.agent_type.tool_id() == "codex");
    let state = Arc::new(RemoteState {
        ui_config_source: Box::new(|| None),
        subagent_source: std::collections::HashMap::new(),
        subagent_message_source: std::collections::HashMap::new(),
        capability_table: multi_agents_manager_lib::inject::capability::new_table(),
        session_source: {
            let sessions = sessions.clone();
            Box::new(move || SessionsResponse {
                sessions: sessions.clone(),
                total_count: sessions.len(),
                waiting_count: 0,
            })
        },
        pairing_counter: Box::new(Vec::new),
        board_hidden_ids: Box::new(Vec::new),
        board_hidden_hide: Arc::new(|_| 0usize),
        board_hidden_unhide: Arc::new(|_| 0usize),
        unread_mark_read: Arc::new(|_, _| ()),
        session_close: Arc::new(|_| Ok(())),
        store,
        host_source: Box::new(|| serde_json::json!({ "host": { "name": "t" } })),
        message_source: Box::new(move |agent: &str, sid: &str, _limit: usize| {
            if agent == "codex" && has_question_page {
                Ok(question_message_page(sid))
            } else {
                Err("测试桩：无内容".to_string())
            }
        }),
        path_source: Box::new(|_, _, _| (Vec::new(), false)),
        watcher_tx: tokio::sync::broadcast::channel(64).0,
        sse_registry: Arc::new(multi_agents_manager_lib::remote::server::SseRegistry::default()),
        max_devices_source: Box::new(|| 3),
        pin_limiter: StdMutex::new(multi_agents_manager_lib::remote::pin::PinRateLimiter::new()),
        global_pin_limiter: StdMutex::new(
            multi_agents_manager_lib::remote::pin::PinRateLimiter::global(),
        ),
        pin_source: Box::new(|| Some("1234".to_string())),
        now_source: Box::new(|| chrono::Utc::now().timestamp_millis()),
        via_hosts_source: Box::new(|| None),
        rate_bucket_channels_source: Box::new(Vec::new),
        home_source: Box::new(|| None),
        injector: Arc::new(multi_agents_manager_lib::inject::engine::RealInjector),
        resume_spawner: Arc::new(|_: &multi_agents_manager_lib::inject::resume::SpawnSpec| Ok(())),
        create_hub: Arc::new(multi_agents_manager_lib::remote::server::CreateTaskHub::stub()),
        archive_source: Box::new(Vec::new),
        archive_delete: Arc::new(|_: Option<&str>| 0usize),
        confirm_probe: Arc::new(|_, _, _| true),
        dialog_probe: Arc::new(|_, _| None),
        screen_probe: Arc::new(|_, _| None),
        codex_control: hub_builder(DeviceStore::Owned(conn.clone())),
        desktop_language: Default::default(),
    });
    let app = router(state.clone());
    Fixture {
        conn,
        state,
        app,
        peer,
    }
}

impl Fixture {
    fn hub_store(&self) -> Arc<ActionStore> {
        self.state.codex_control.router.store().clone()
    }

    fn thread_key(&self) -> ThreadKey {
        ThreadKey {
            host_id: "local".into(),
            thread_id: THREAD.into(),
        }
    }

    fn codex_audits(&self) -> Vec<multi_agents_manager_lib::database::dao::write_audit::AuditRow> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT ts, device_name, agent_type, session_id, channel, action, summary, result FROM write_audit WHERE action LIKE 'codex_%' ORDER BY id")
            .unwrap();
        stmt.query_map([], |r| {
            Ok(
                multi_agents_manager_lib::database::dao::write_audit::AuditRow {
                    ts: r.get(0)?,
                    device_name: r.get(1)?,
                    agent_type: r.get(2)?,
                    session_id: r.get(3)?,
                    channel: r.get(4)?,
                    action: r.get(5)?,
                    summary: r.get(6)?,
                    result: r.get(7)?,
                },
            )
        })
        .unwrap()
        .filter_map(|r| r.ok())
        .collect()
    }

    fn all_audit_summaries(&self) -> Vec<String> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT summary FROM write_audit").unwrap();
        stmt.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
    }
}

// ---- 请求样板 ----

fn req(
    method: &str,
    uri: &str,
    cookie: Option<&str>,
    body: Option<String>,
) -> axum::http::Request<axum::body::Body> {
    let mut b = axum::http::Request::builder().method(method).uri(uri);
    if body.is_some() {
        b = b.header("content-type", "application/json");
    }
    if let Some(c) = cookie {
        b = b.header("cookie", c);
    }
    b.body(axum::body::Body::from(body.unwrap_or_default()))
        .unwrap()
}

const COOKIE: &str = "mam_device=dev-1";
const THREAD: &str = "0f1e2d3c-1111-4222-8333-444455556666";

async fn body_string(b: axum::http::Response<axum::body::Body>) -> String {
    use http_body_util::BodyExt;
    String::from_utf8(b.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap()
}

/// POST /session-send 的便捷构造。
fn send_body(text: &str, action_id: Option<&str>, queue_only: bool) -> String {
    send_body_for(text, action_id, queue_only, THREAD)
}

/// 同 [`send_body`]，session_id 可指定（跨 thread 用例用）。
fn send_body_for(text: &str, action_id: Option<&str>, queue_only: bool, session: &str) -> String {
    let mut v = serde_json::json!({ "sessionId": session, "text": text });
    if let Some(a) = action_id {
        v["actionId"] = serde_json::json!(a);
    }
    if queue_only {
        v["queueOnly"] = serde_json::json!(true);
    }
    v.to_string()
}

// ============================================================
// 1. 认证矩阵：新路由无 cookie / 吊销 = 403（回环同样拒绝——gate 零豁免）
// ============================================================

#[tokio::test]
async fn new_routes_require_device_cookie() {
    let f = fixture_disabled(vec![codex_app_session(THREAD)]);
    let app = f.app.clone();
    for uri in [
        "/m/api/v1/codex/thread?session_id=x",
        "/m/api/v1/codex/thread/messages?session_id=x",
        "/m/api/v1/codex/action?action_id=a",
        "/m/api/v1/codex/queue?session_id=x",
    ] {
        let r = app
            .clone()
            .oneshot(req("GET", uri, None, None))
            .await
            .unwrap();
        assert_eq!(r.status(), 403, "无 cookie 必须 403：{uri}");
        // 回环同样拒绝：无 Host 豁免（gate 零豁免结构不变）
        let r = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri(uri)
                    .header("host", "127.0.0.1:9420")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), 403, "回环同样拒绝：{uri}");
    }
    for uri in [
        "/m/api/v1/codex/queue/retract",
        "/m/api/v1/codex/queue/force-continue",
        "/m/api/v1/codex/pending-response",
    ] {
        let r = app
            .clone()
            .oneshot(req("POST", uri, None, Some("{}".into())))
            .await
            .unwrap();
        assert_eq!(r.status(), 403, "POST 无 cookie 必须 403：{uri}");
    }
}

/// 吊销设备后 cookie 即刻失效（TTL 内也拒）。
#[tokio::test]
async fn revoked_device_gets_403_on_new_routes() {
    let f = fixture_disabled(vec![codex_app_session(THREAD)]);
    DeviceStore::Owned(f.conn.clone()).with(|c| revoke_device(c, "dev-1").unwrap());
    let r = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 403, "吊销设备必须 403");
    // 任务 4 的分页路由同属门禁内层（吊销后同样 403）
    let r = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread/messages?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 403, "吊销设备访问分页必须 403");
}

// ============================================================
// 2. GET /codex/thread：投影 + 404 语义
// ============================================================

#[tokio::test]
async fn thread_state_returns_live_projection() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("thread_state in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("no-store"),
        "投影响应必须 no-store"
    );
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["threadId"], THREAD);
    assert_eq!(v["stale"], serde_json::json!(false));
    assert_eq!(v["availability"], "controllable");
    assert_eq!(v["runtimeStatus"], "idle");
}

/// thread 不可解析（非 UUID 且无 rollout）→ 404。
#[tokio::test]
async fn thread_state_unresolvable_session_is_404() {
    let mut f = fixture_ipc(vec![codex_app_session("plain-session-name")]);
    let peer = f.peer.take().unwrap();
    let r = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            "/m/api/v1/codex/thread?session_id=plain-session-name",
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 404, "非 codex thread 解析失败必须 404");
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["error"], "no_session");
    peer.close();
}

// ============================================================
// 3. session-send hub 接线
// ============================================================

/// H3=false（默认关）且 A 可用 → IPC 正常：desktop_ipc 回执 + accepted + 账本落账。
#[tokio::test]
async fn send_h3_off_ipc_available_uses_ipc() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let body = send_body("通过 IPC 发送", Some("act-ipc-1"), false);
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let frame = peer.recv().await;
    confirm_client_message_identity(&mut peer, &frame, "turn-9", "通过 IPC 发送", 8, "owner-1")
        .await;
    assert_eq!(frame["method"], "thread-follower-start-turn");
    assert_eq!(
        frame["params"]["turnStart"]["request"]["input"][0]["text"],
        "通过 IPC 发送"
    );
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "turn": { "id": "turn-9", "status": "inProgress" } }
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(20), task)
        .await
        .expect("send in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["status"], "desktop_ipc");
    assert_eq!(v["receipt"]["delivery"], "accepted");
    assert_eq!(v["receipt"]["route"], "desktop_ipc");
    assert_eq!(v["receipt"]["turnId"], "turn-9");
    let view = f
        .hub_store()
        .get_for_device("act-ipc-1", "dev-1")
        .unwrap()
        .unwrap();
    assert_eq!(view.state, ActionState::Accepted);
    let audits = f.codex_audits();
    assert!(
        audits.iter().any(|a| a.action == "codex_send"
            && a.channel == "codex_desktop_ipc"
            && a.summary.contains("act-ipc-1")
            && a.result.contains("accepted")),
        "send 有可追踪审计行：{:?}",
        audits
    );
}

/// 签名开关开启（2026-10-20 复审 P2）：真实 IPC start-turn 恰好一次
/// `[mobile 花名]`，且与账本/审计同一份 wire（不再恒加、也不重复加）。
#[tokio::test]
async fn signature_on_ipc_start_turn_appends_exactly_once() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    f.state.store.with(|c| {
        c.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('remote_message_signature', 'on')",
            [],
        )
        .unwrap();
    });
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let body = send_body("签名路径", Some("act-sign-1"), false);
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let frame = peer.recv().await;
    confirm_client_message_identity(&mut peer, &frame, "turn-sign", "签名路径", 8, "owner-1").await;
    assert_eq!(frame["method"], "thread-follower-start-turn");
    let text = frame["params"]["turnStart"]["request"]["input"][0]["text"]
        .as_str()
        .unwrap();
    assert_eq!(text, "签名路径 [mobile iPhone]", "开关开 = 恰好一次签名");
    assert_eq!(text.matches("[mobile iPhone]").count(), 1);
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "turn": { "id": "turn-sign", "status": "inProgress" } }
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(20), task)
        .await
        .expect("send in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["status"], "desktop_ipc");
    assert_eq!(v["receipt"]["delivery"], "accepted");
}

/// 2026-10-20 复审 P1：快照之后本地新建 T1、忙态回显 T1（新 ID、无本次消息
/// 身份）→ 不得 Accepted；必须 Unknown、保留屏障、不派发下一条。
#[tokio::test]
async fn busy_race_new_turn_without_client_identity_is_unknown() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let body = send_body("竞态窗口消息", Some("act-race"), false);
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let frame = peer.recv().await;
    assert_eq!(frame["method"], "thread-follower-start-turn");
    let req_id = frame["requestId"].as_str().unwrap().to_string();
    // 本地新建 T1（不同 clientMessageId）：发生在写前快照之后
    peer.send(&snapshot_frame(
        THREAD,
        "owner-1",
        8,
        "active",
        serde_json::json!({
            "t-local": { "id": "t-local", "type": "turn", "status": "inProgress", "items": ["u-local"] },
            "u-local": { "id": "u-local", "type": "userMessage", "clientId": "local-other", "content": [{"type":"text","text":"local"}], "ts": 2 }
        }),
        serde_json::json!(["t-local"]),
    ))
    .await;
    peer.send(&serde_json::json!({
        "type": "response", "requestId": req_id,
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "result": { "turn": { "id": "t-local", "status": "inProgress" } } }
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(20), task)
        .await
        .expect("send in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["status"], "desktop_ipc");
    assert_ne!(
        v["receipt"]["delivery"], "accepted",
        "新 Turn ID 不算送达证据"
    );
    assert_eq!(v["receipt"]["delivery"], "unknown_delivery");
    assert_eq!(v["receipt"]["reasonCode"], "client_message_unverified");
    let view = f
        .hub_store()
        .get_for_device("act-race", "dev-1")
        .unwrap()
        .unwrap();
    assert_eq!(view.state, ActionState::Unknown);
    assert!(
        f.hub_store().has_compat_fence(&f.thread_key()).unwrap(),
        "关联无法确认必须留保守屏障"
    );
}

/// 正向：快照之后新建 T1，且 T1 中已有本次 clientMessageId 的正式 userMessage
/// → 正常 Accepted（身份核验通过）。
#[tokio::test]
async fn accepted_when_new_turn_contains_client_message_identity() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let body = send_body("身份可核验", Some("act-id-ok"), false);
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let frame = peer.recv().await;
    let req_id = frame["requestId"].as_str().unwrap().to_string();
    let cmid = frame["params"]["turnStart"]["request"]["clientUserMessageId"]
        .as_str()
        .unwrap()
        .to_string();
    peer.send(&snapshot_frame(
        THREAD,
        "owner-1",
        8,
        "active",
        serde_json::json!({
            "t-ok": { "id": "t-ok", "type": "turn", "status": "inProgress", "items": ["u-ok"] },
            "u-ok": { "id": "u-ok", "type": "userMessage", "clientId": cmid, "content": [{"type":"text","text":"身份可核验"}], "ts": 2 }
        }),
        serde_json::json!(["t-ok"]),
    ))
    .await;
    peer.send(&serde_json::json!({
        "type": "response", "requestId": req_id,
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "result": { "turn": { "id": "t-ok", "status": "inProgress" } } }
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(20), task)
        .await
        .expect("send in time")
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["receipt"]["delivery"], "accepted");
    assert_eq!(v["receipt"]["turnId"], "t-ok");
}

/// P1 正向延迟路径：正式 userMessage 在回执后 600ms 才进镜像（预检实测
/// 515–544ms）→ 2s 窗口内仍 Accepted，不得误判 Unknown。
#[tokio::test]
async fn delayed_client_identity_within_window_is_accepted() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let body = send_body("延迟身份", Some("act-delay"), false);
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let frame = peer.recv().await;
    assert_eq!(frame["method"], "thread-follower-start-turn");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "result": { "turn": { "id": "turn-delayed", "status": "inProgress" } } }
    }))
    .await;
    // 超过旧 500ms 窗口、但仍在新 2s 窗口内
    tokio::time::sleep(Duration::from_millis(600)).await;
    confirm_client_message_identity(&mut peer, &frame, "turn-delayed", "延迟身份", 8, "owner-1")
        .await;
    let r = tokio::time::timeout(Duration::from_secs(20), task)
        .await
        .expect("send in time")
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["receipt"]["delivery"], "accepted");
    assert_eq!(v["receipt"]["turnId"], "turn-delayed");
}

/// 忙 Thread（快照 active）→ codex_queued + position（宪法 R2.3 留队）。
#[tokio::test]
async fn send_busy_thread_queues_with_position() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    // 先以 idle 快照建立跟随（镜像在先），再推 active 增量——忙判定从镜像读，
    // 未跟随的 Thread 首发无法预知忙态（R2.3 的排队由 Desktop 自身队列与后续
    // active_turns 承担；此处验证的是「已知忙 → 留队」的 FIFO 语义）
    {
        let app = f.app.clone();
        let task = tokio::spawn(async move {
            app.oneshot(req(
                "GET",
                &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
                Some(COOKIE),
                None,
            ))
            .await
            .unwrap()
        });
        drive_ensure(&mut peer, THREAD, vec![], "idle").await;
        let r = tokio::time::timeout(Duration::from_secs(15), task)
            .await
            .expect("follow in time")
            .unwrap();
        assert_eq!(r.status(), 200);
    }
    // 推 active 增量（state-changed）→ 投影/镜像更新
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": THREAD, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": 8,
                "conversationState": {
                    "turns": [], "requests": [],
                    "threadRuntimeStatus": { "type": "active" },
                    "turnHistory": codex_fixture::canonical_turn_history(serde_json::json!({
                            "t2": { "id": "t2", "type": "turn", "status": "inProgress", "items": ["u2"] },
                            "u2": { "id": "u2", "type": "user_message", "text": "running", "ts": 2 }
                        }), serde_json::json!([{ "entries": ["t2"] }]))
                }
            }
        }
    }))
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let body = send_body("忙态消息", Some("act-q-1"), false);
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("send in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["status"], "codex_queued");
    assert_eq!(v["actionId"], "act-q-1");
    assert_eq!(v["position"], serde_json::json!(1));
    let view = f
        .hub_store()
        .get_for_device("act-q-1", "dev-1")
        .unwrap()
        .unwrap();
    assert_eq!(view.state, ActionState::Queued);
}

/// Codex APP 缺 actionId → 400（不代造，防双投）；零 IPC 写入。
#[tokio::test]
async fn send_missing_action_id_is_400_and_zero_writes() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(send_body("没有 actionId", None, false)),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["reasonCode"], "action_id_required");
    assert!(
        !peer_has_traffic(&mut peer).await,
        "缺 actionId 不得产生任何 IPC 写入"
    );
}

/// 恒败连接器（#172 能力位取证用）：连接不可达，能力位只由注入的 profile 决定——
/// 不依赖任何真实 Desktop。
struct FailingConnector;

impl Connector for FailingConnector {
    fn connect(
        &self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        multi_agents_manager_lib::services::codex_desktop_ipc::transport::BoxIo,
                        multi_agents_manager_lib::services::codex_desktop_ipc::types::ProtocolError,
                    >,
                > + Send
                + '_,
        >,
    > {
        Box::pin(async {
            Err(
                multi_agents_manager_lib::services::codex_desktop_ipc::types::ProtocolError::Disconnected(
                    "fixture: no desktop".into(),
                ),
            )
        })
    }
}

/// #172 只读取证 fixture：`state` 已取证、`send` 未取证（旧实现在此档位落
/// `compat_only` 并把整页读能力降级）。方法表沿用已验证 build，只改能力位与 build 号。
fn fixture_state_only(sessions: Vec<Session>) -> Fixture {
    let mut profile = caps_open_profile();
    profile.build = "26.1013.9999.0".into();
    profile.capabilities.send = false;
    profile.capabilities.state = true;
    let adapter = Arc::new(DesktopIpcAdapter::new(Arc::new(FailingConnector), profile));
    fixture_with(
        sessions,
        move |store| Arc::new(CodexControlHub::with_adapter(store, adapter, true)),
        None,
    )
}

/// #172 裁决（设计 §10.1）：桌面归属会话的 A 路写能力未取证 → **如实失败**
/// `capability_unverified`，**不得静默改走 headless**；`session-send-info` 必须同源
/// 同码同文案（判据与位置都在 H3 分支之前）。
///
/// 旧行为：H3 关闭 → 403 headless_disabled（把「能力未验证」误报成「无头未开」）；
/// H3 打开 → 静默走无头通道（用户点发送，消息落到另一个会话）。新行为：两档 H3
/// 【S2 重写·保留名义】桌面归属会话 + 控制面不可尝试（disabled hub）→ session-send
/// 在**静态门**处如实拒绝（403 capability_unverified，H3 开关无关），**零账本行**
/// （从未进入尝试；重放同 ID 同样在门处拒绝）；零兜底交接、拒发留审计。
/// 旧断言差异：旧路径经 router 登记 → permit NotSent 落账再 403（账本有行、审计词
/// `codex_fallback`）——S2 起「未进入尝试」不落账本，审计词归 `codex_send`。
#[tokio::test]
async fn send_desktop_owned_without_send_refuses_instead_of_headless() {
    for h3_on in [false, true] {
        let f = fixture_disabled(vec![codex_app_session(THREAD)]);
        if h3_on {
            f.state.store.with(|c| {
                c.execute(
                    "INSERT OR REPLACE INTO settings (key, value) VALUES ('remote.headless_enabled', 'true')",
                    [],
                )
                .unwrap();
            });
        }
        assert!(
            !f.state.codex_control.send_available(),
            "closed hub：写能力必须不可用"
        );
        assert!(
            !f.state.codex_control.profile_send_capable(),
            "closed hub：档位无写能力快照"
        );
        let r = f
            .app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some(COOKIE),
                Some(send_body("hi", Some("a1"), false)),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 403, "H3={h3_on}：不可尝试 → 如实拒绝");
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["error"], "capability_unverified", "H3={h3_on}: {v}");
        assert_eq!(v["reasonCode"], "capability_unverified");
        let reason = v["reason"].as_str().unwrap_or("");
        assert!(
            !reason.is_empty(),
            "403 必须带可理解原因（手机端只读 reason 展示）: {v}"
        );
        // 零账本行（未进入尝试；同 ID 重放同样在门处拒绝，无重放回执）
        assert!(
            f.hub_store()
                .get_for_device("a1", "dev-1")
                .expect("store query")
                .is_none(),
            "门拒不落账本"
        );
        // 零兜底交接 + 拒发留审计（audit 词归 codex_send）
        let audits = f.codex_audits();
        assert!(
            !audits.iter().any(|a| a.action == "codex_fallback"),
            "H3={h3_on}：桌面会话没有任何兜底分支: {audits:?}"
        );
        assert!(
            audits
                .iter()
                .any(|a| a.action == "codex_send"
                    && a.result.contains("refused:capability_unverified")),
            "H3={h3_on}：拒发必须留痕: {audits:?}"
        );

        // send-info 同源：同一码 + 同一句文案 + 输入区置灰
        let r = f
            .app
            .clone()
            .oneshot(req(
                "GET",
                &format!("/m/api/v1/session-send-info?session_id={THREAD}"),
                Some(COOKIE),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "send-info 恒 200 载荷");
        let info: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(
            info["injectable"],
            serde_json::json!(false),
            "H3={h3_on}: {info}"
        );
        assert_eq!(
            info["reasonCode"], "capability_unverified",
            "两端口径必须一致（H3={h3_on}）: {info}"
        );
        assert_eq!(
            info["reason"].as_str().unwrap_or(""),
            reason,
            "同一句原因文案（H3={h3_on}）"
        );
    }
}

#[tokio::test]
async fn send_state_only_build_refuses_as_capability_unverified() {
    let f = fixture_state_only(vec![codex_app_session(THREAD)]);
    assert!(
        f.state.codex_control.state_available(),
        "读链已取证 → 详情投影可用"
    );
    assert!(
        !f.state.codex_control.send_available(),
        "写未取证 → 发送门必须关"
    );
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(send_body("hi", Some("a-so-1"), false)),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["error"], "capability_unverified", "{v}");
    assert_ne!(
        v["reasonCode"], "headless_disabled",
        "只读档的拒发不得复用无头开关文案"
    );
}

/// #172 边界：**codex CLI 形态**（`form=Cli`，终端归属）会话不进入桌面归属门——
/// 路由结论仍是 `no_process`（不是 capability_unverified）。
///
/// **本用例不覆盖 App 形态**：App 形态的门见 `send_desktop_owned_*` 与
/// `verified_build_link_down_*` 两组用例。
#[tokio::test]
async fn send_cli_form_session_not_gated_by_desktop_hub() {
    let mut cli = codex_app_session(THREAD);
    cli.form = ProcessForm::Cli;
    cli.pid = 0; // 进程不在：路由走 no_process 早退（零注入）
    let f = fixture_disabled(vec![cli]);
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(send_body("hi", Some("a-cli-1"), false)),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["reasonCode"], "no_process", "{v}");
}

/// IPC 明确拒绝（peer 回 error）→ Rejected 回执 200，**不触发 fallback**（H3 开着
/// 也不降级——非 NotSent 绝不交接）。
#[tokio::test]
async fn send_rejected_delivery_never_falls_back() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    // H3 打开（内存库 seed 开关）
    f.state.store.with(|c| {
        c.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('remote.headless_enabled', 'true')",
            [],
        )
        .unwrap();
    });
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let body = send_body("会被拒绝", Some("act-rej-1"), false);
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let frame = peer.recv().await;
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "error", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "error": { "message": "not allowed" }
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(20), task)
        .await
        .expect("in time")
        .unwrap();
    assert_eq!(r.status(), 200, "明确拒绝是 200 的明确回执");
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["status"], "desktop_ipc");
    assert_eq!(v["receipt"]["delivery"], "rejected");
    let audits = f.codex_audits();
    assert!(
        !audits.iter().any(|a| a.action == "codex_fallback"),
        "拒绝绝不降级：{:?}",
        audits
    );
    assert!(audits
        .iter()
        .any(|a| a.action == "codex_send" && a.result.contains("rejected")));
}

/// 【S2 重写（原 `send_not_sent_hands_off_to_compat_when_h3_on`）】旧断言「A 路
/// NotSent（对端断连）→ handoff → headless 兜底封套（H3 开）」随兜底臂废除（设计
/// §1.4 旧口径「明确 NotSent 可自动转 headless」被取代）。新契约 = 断链后状态不可
/// 确认 → **零投递 + 留队冻结**（写出前门按未确认处理；Desktop 恢复后接力），
/// H3 开关无关、零兜底审计、零 headless 封套。
#[tokio::test]
async fn link_down_freezes_message_without_headless() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    f.state.store.with(|c| {
        c.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('remote.headless_enabled', 'true')",
            [],
        )
        .unwrap();
    });
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let body = send_body("断连消息", Some("act-cmp-1"), false);
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    // 握手进行到 following 后立刻断连（EOF）→ writer NotSent → 终态回执
    handshake(&mut peer, "owner-1").await;
    let disc = peer.recv().await;
    peer.send(&discovery_response(
        disc["requestId"].as_str().unwrap(),
        "owner-1",
    ))
    .await;
    let _follow = peer.recv().await;
    peer.close();
    let r = tokio::time::timeout(Duration::from_secs(30), task)
        .await
        .expect("receipt in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    // 断链后状态不可确认 → 消息**留队冻结**（零投递、不终态拒绝、Desktop 回来后
    // 可按原 actionId 派发）——比旧的「终态 NotSent + 兜底」更贴近「默认可用且可
    // 恢复」；无论如何都没有 headless 去向。
    assert_eq!(
        v["status"], "codex_queued",
        "断链冻结留队（无 headless 去向）: {v}"
    );
    assert_eq!(v["actionId"], "act-cmp-1");
    let view = f
        .hub_store()
        .get_for_device("act-cmp-1", "dev-1")
        .unwrap()
        .unwrap();
    assert_eq!(view.state, ActionState::Queued, "留队等待恢复");
    assert!(
        !f.hub_store().has_compat_fence(&f.thread_key()).unwrap(),
        "无屏障残留"
    );
    let audits = f.codex_audits();
    assert!(
        !audits.iter().any(|a| a.action == "codex_fallback"),
        "零兜底审计: {audits:?}"
    );
    assert!(
        audits
            .iter()
            .any(|a| a.action == "codex_send" && a.result == "queued"),
        "发送审计如实记排队: {audits:?}"
    );
}

/// 【S2 重写（原 `verified_build_link_down_keeps_headless_route_for_send_and_info`）】
/// 旧断言「已验证 build 断链 → headless 通道兜底 + send-info injectable:true（无头
/// 通道）」随兜底废除。新契约 = 断链时**零投递、消息留队**（状态不可确认冻结——
/// Desktop 回来后按原 actionId 派发），**没有任何 headless 去向**；send-info 与
/// session-send 同源拆码：快照显示档位带写（已取证）但此刻不可达 →
/// `capability_unavailable`（503 的语义码；输入区置灰引导稍后重试，不误报
/// 「未验证」）。
#[tokio::test]
async fn verified_build_link_down_queues_without_headless_and_info_reports_unavailable() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    f.state.store.with(|c| {
        c.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('remote.headless_enabled', 'true')",
            [],
        )
        .unwrap();
    });
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let body = send_body("断链消息", Some("act-ld-1"), false);
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    // 握手到 following 后断链：写尝试在断链后失败/冻结 → 消息留队（不终态拒绝、不兜底）
    handshake(&mut peer, "owner-1").await;
    let disc = peer.recv().await;
    peer.send(&discovery_response(
        disc["requestId"].as_str().unwrap(),
        "owner-1",
    ))
    .await;
    let _follow = peer.recv().await;
    peer.close();
    let r = tokio::time::timeout(Duration::from_secs(30), task)
        .await
        .expect("in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert!(
        v["status"] == "codex_queued" || v["receipt"]["delivery"] == "not_sent",
        "断链：留队或明确未送达，均不转 headless: {v}"
    );
    assert_ne!(v["status"], "headless", "零 headless 去向");
    assert!(
        !f.codex_audits()
            .iter()
            .any(|a| a.action == "codex_fallback"),
        "零兜底审计"
    );

    // 再 ensure 一次（连接已死 → adapter 回落 closed profile）：
    // 「当前写不可用」但「档位写能力快照仍在」——正是本用例要钉住的双条件
    let _ = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    assert!(
        !f.state.codex_control.send_available(),
        "断链：当前写能力不可用"
    );
    assert!(
        f.state.codex_control.profile_send_capable(),
        "但档位写能力快照必须仍在（不能被链路状态冲掉）"
    );

    // send-info 同源拆码：已取证 build 的瞬时断链 = capability_unavailable（不是
    // capability_unverified），输入区置灰（不再指向 headless 通道）
    let r = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/m/api/v1/session-send-info?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let info: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(info["injectable"], serde_json::json!(false), "{info}");
    assert_eq!(info["reasonCode"], "capability_unavailable", "{info}");
}

#[tokio::test]
async fn aborted_http_waiter_does_not_lose_dispatched_write() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let body = send_body("断连消息", Some("act-disc-1"), false);
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let frame = peer.recv().await;
    confirm_client_message_identity(&mut peer, &frame, "turn-d", "断连消息", 8, "owner-1").await;
    assert_eq!(
        frame["method"], "thread-follower-start-turn",
        "写动作已发出"
    );
    // 此时中止 HTTP 等待者（测试主动取消——不模拟真实连接关闭语义）
    task.abort();
    // Desktop 侧仍正常应答：独立派发任务收到回执并落账
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "turn": { "id": "turn-d", "status": "inProgress" } }
    }))
    .await;
    // 轮询等独立任务落账（不依赖 HTTP 等待者）
    let view = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let v = f
                .hub_store()
                .get_for_device("act-disc-1", "dev-1")
                .unwrap()
                .unwrap();
            if v.state != ActionState::Reserved {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("dispatch task settles");
    assert_eq!(
        view.state,
        ActionState::Accepted,
        "等待者中止后独立任务继续落账（R9.2）"
    );
    assert_eq!(view.receipt.unwrap().turn_id.as_deref(), Some("turn-d"));
    assert!(!peer_has_traffic(&mut peer).await, "中止后无二次写入");
}

/// R9.2 另一面：写帧已发出后 **peer 真实断连**（EOF）→ 在飞 mutating RPC 按
/// UnknownDelivery 如实落账（不虚称失败、不回 queued、不重发）。
#[tokio::test]
async fn peer_eof_after_write_marks_unknown() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let body = send_body("对端断连消息", Some("act-eof-1"), false);
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let frame = peer.recv().await;
    assert_eq!(frame["method"], "thread-follower-start-turn");
    // 对端真实断连（EOF）——在飞 RPC 结果未知
    peer.close();
    let r = tokio::time::timeout(Duration::from_secs(20), task)
        .await
        .expect("in time")
        .unwrap();
    assert_eq!(r.status(), 200, "unknown 是 200 的明确回执");
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["receipt"]["delivery"], "unknown_delivery");
    let view = f
        .hub_store()
        .get_for_device("act-eof-1", "dev-1")
        .unwrap()
        .unwrap();
    assert_eq!(view.state, ActionState::Unknown, "断连自动 Unknown");
}

// ============================================================
// 4. session-send-info：A 可用不看 H3
// ============================================================

#[tokio::test]
async fn send_info_h3_off_ipc_available_injectable_true() {
    let f = fixture_ipc(vec![codex_app_session(THREAD)]);
    f.state.store.with(|c| {
        c.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('remote.headless_enabled', 'false')",
            [],
        )
        .unwrap();
    });
    let r = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/m/api/v1/session-send-info?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(
        v["injectable"],
        serde_json::json!(true),
        "A 可用时 H3 关不置灰（真实 composer 路径）"
    );
    // 通道词保持 headless_codex_*（移动端据此携带 actionId）
    assert_eq!(
        v["channels"],
        serde_json::json!(["headless_codex_queue", "headless_codex_exec"])
    );
}

/// #172：桌面归属会话且 build 无写能力证据 → `injectable:false` 且归因
/// `capability_unverified`（与 session-send 的 403 同码；旧实现报 headless_disabled，
/// 把「能力未验证」误报成「无头未开」）。
#[tokio::test]
async fn send_info_h3_off_ipc_unavailable_still_disabled() {
    let f = fixture_disabled(vec![codex_app_session(THREAD)]);
    let r = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/m/api/v1/session-send-info?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["injectable"], serde_json::json!(false));
    assert_eq!(v["reasonCode"], "capability_unverified", "{v}");
}

// ============================================================
// 5. pending-response
// ============================================================

/// 带 ownerClientId / targetClientId / raw method / permissions 对象 → 400。
#[tokio::test]
async fn pending_response_rejects_owner_params() {
    let f = fixture_ipc(vec![codex_app_session(THREAD)]);
    for extra in [
        serde_json::json!({ "ownerClientId": "o" }),
        serde_json::json!({ "targetClientId": "t" }),
        serde_json::json!({ "rawMethod": "thread-follower-command-approval-decision" }),
        serde_json::json!({ "permissions": { "network": true } }),
    ] {
        let mut body = serde_json::json!({
            "sessionId": THREAD, "actionId": "a", "requestKey": "n:1",
            "contextToken": "t", "action": "approve",
        });
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let r = f
            .app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/codex/pending-response",
                Some(COOKIE),
                Some(body.to_string()),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "多余键必须 400：{extra}");
    }
}

/// 先建立投影（GET thread），返回 (pending requestKey, contextToken)。
async fn follow_with_pending(
    f: &Fixture,
    peer: &mut ScriptedPeer,
    requests: Vec<serde_json::Value>,
) -> (String, String) {
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    drive_ensure(peer, THREAD, requests, "idle").await;
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("follow in time")
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    let pending = &v["pending"][0];
    (
        pending["requestKey"].as_str().unwrap().to_string(),
        pending["contextToken"].as_str().unwrap().to_string(),
    )
}

/// 过期 token / 跨 thread requestKey → 409 且 IPC 写 0。
#[tokio::test]
async fn pending_response_stale_context_is_409_with_zero_writes() {
    let mut f = fixture_ipc(vec![
        codex_app_session(THREAD),
        codex_app_session("99994444-1111-4222-8333-444455556666"),
    ]);
    let mut peer = f.peer.take().unwrap();
    let (rk, _tk) = follow_with_pending(
        &f,
        &mut peer,
        vec![serde_json::json!({
            "id": 18, "method": "item/commandExecution/requestApproval",
            "params": { "command": "pnpm test", "cwd": "/w", "reason": "r" }
        })],
    )
    .await;
    assert_eq!(rk, "n:18");

    // 过期 token → 409（写 0）
    let body = serde_json::json!({
        "sessionId": THREAD, "actionId": "act-stale-1",
        "requestKey": "n:18", "contextToken": "expired-token",
        "action": "approve",
    });
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/pending-response",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 409);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["error"], "stale_context");
    assert!(!peer_has_traffic(&mut peer).await, "过期 token 零 IPC 写入");

    // 跨 thread requestKey（另一会话的 thread 无此 pending）→ 409
    let body = serde_json::json!({
        "sessionId": "99994444-1111-4222-8333-444455556666",
        "actionId": "act-stale-2",
        "requestKey": "n:18", "contextToken": "any",
        "action": "approve",
    });
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/pending-response",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 409, "跨 thread requestKey → 409");
    assert!(!peer_has_traffic(&mut peer).await, "跨 thread 零 IPC 写入");
}

/// 完整应答链：GET 投影 → approve（command 变体）→ 200 accepted 回执 + 审计。
#[tokio::test]
async fn pending_response_approve_end_to_end() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let (rk, tk) = follow_with_pending(
        &f,
        &mut peer,
        vec![serde_json::json!({
            "id": 18, "method": "item/commandExecution/requestApproval",
            "params": { "command": "pnpm test", "cwd": "/w", "reason": "r" }
        })],
    )
    .await;

    let body = serde_json::json!({
        "sessionId": THREAD, "actionId": "act-appr-1",
        "requestKey": rk, "contextToken": tk, "action": "approve",
    });
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/pending-response",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
    });
    // 命令审批帧（decision accept；整数 id 无损）
    let frame = peer.recv().await;
    assert_eq!(frame["method"], "thread-follower-command-approval-decision");
    assert_eq!(frame["params"]["decision"], "accept");
    assert_eq!(frame["params"]["requestId"], serde_json::json!(18));
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-command-approval-decision",
        "result": { "ok": true }
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("respond in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["delivery"], "accepted");
    let view = f
        .hub_store()
        .get_for_device("act-appr-1", "dev-1")
        .unwrap()
        .unwrap();
    assert_eq!(view.state, ActionState::Accepted);
    let audits = f.codex_audits();
    let row = audits
        .iter()
        .find(|a| a.action == "codex_approve")
        .expect("approve 审计行");
    assert!(row.summary.contains("act-appr-1"));
    assert!(row.summary.contains("n:18"));
    assert!(!row.summary.contains("permissions"), "审批审计不带权限对象");
}

/// 结构化回答：限额校验 + 按 qid 映射 + isSecret 答案原文不入审计。
#[tokio::test]
async fn pending_response_answer_limits_and_audit_privacy() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let (rk, tk) = follow_with_pending(
        &f,
        &mut peer,
        vec![serde_json::json!({
            "id": "s:uid-1", "method": "item/tool/requestUserInput",
            "params": { "questions": [
                { "id": "q1", "header": "选择", "question": "选一个",
                  "isOther": false, "isSecret": false,
                  "options": [ { "label": "甲", "description": "" }, { "label": "乙", "description": "" } ] },
                { "id": "q2", "header": "密钥", "question": "输入密钥",
                  "isOther": true, "isSecret": true, "options": [] }
            ]}
        })],
    )
    .await;

    // 空答案 → 400
    let body = serde_json::json!({
        "sessionId": THREAD, "actionId": "act-ans-e",
        "requestKey": rk, "contextToken": tk,
        "action": "answer", "answers": {},
    });
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/pending-response",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 400, "空答案拒绝");

    // 每题超长 → 400
    let long = "x".repeat(10_001);
    let body = serde_json::json!({
        "sessionId": THREAD, "actionId": "act-ans-l",
        "requestKey": rk, "contextToken": tk,
        "action": "answer",
        "answers": { "q1": ["甲"], "q2": [long] },
    });
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/pending-response",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 400, "单答案超 10_000 字符拒绝");

    // 合法回答（isSecret 答案走 q2 自由文本）
    let secret = "SUPER-SECRET-ANSWER-9";
    let body = serde_json::json!({
        "sessionId": THREAD, "actionId": "act-ans-1",
        "requestKey": rk, "contextToken": tk,
        "action": "answer",
        "answers": { "q1": ["甲"], "q2": [secret] },
    });
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/pending-response",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
    });
    let frame = peer.recv().await;
    assert_eq!(frame["method"], "thread-follower-submit-user-input");
    let answers = &frame["params"]["response"]["answers"];
    assert_eq!(answers["q1"]["answers"], serde_json::json!(["甲"]));
    assert_eq!(answers["q2"]["answers"], serde_json::json!([secret]));
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-submit-user-input",
        "result": { "ok": true }
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("respond in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    let view = f
        .hub_store()
        .get_for_device("act-ans-1", "dev-1")
        .unwrap()
        .unwrap();
    assert_eq!(view.state, ActionState::Accepted);
    let audits = f.codex_audits();
    let row = audits
        .iter()
        .find(|a| a.action == "codex_answer")
        .expect("answer 审计行");
    assert!(!row.summary.contains(secret), "isSecret 答案绝不入审计");
    assert!(row.summary.contains("act-ans-1"));
}

// ============================================================
// 6. 队列：认证 / 跨设备 404 / 撤回竞态 409 / cancelled 不重发
// ============================================================

#[tokio::test]
async fn queue_view_and_retract_auth_matrix() {
    let f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let app = f.app.clone();
    // queue GET 无 cookie → 403
    let r = app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/queue?session_id={THREAD}"),
            None,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    // retract 无 cookie → 403
    let r = app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/queue/retract",
            None,
            Some(serde_json::json!({ "actionId": "a" }).to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    // 本设备空队列
    let r = app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/queue?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["actions"], serde_json::json!([]));
    assert!(v["blockedReasonCode"].is_null());
    assert_eq!(
        v["canInterrupt"], false,
        "未开启 interrupt 的 build：队列视图 canInterrupt 必须 false（按钮源头隐藏，复审 P2-2）"
    );
}
#[tokio::test]
async fn queue_view_never_deletes_pause_on_read() {
    let f = fixture_ipc(vec![codex_app_session(THREAD)]);
    // 直写持久层 = 打断端落点；没有待发消息、也没有屏障。
    // C02：读取路径不得删除暂停行（无法证明在途 owner/new token 已结束）；
    // 空黄条由前端自隐，持久闩等待显式 resume 或后续终结证据。
    f.state
        .codex_control
        .router
        .store()
        .set_queue_pause(&f.thread_key(), "interrupt_accepted", "stale-token")
        .unwrap();
    let r = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/queue?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["queuePaused"], serde_json::json!(true), "{v}");
    assert!(
        f.state
            .codex_control
            .router
            .store()
            .queue_pause(&f.thread_key())
            .unwrap()
            .is_some(),
        "只读视图不得删除持久暂停"
    );
}

/// 撤回：本设备 queued 可撤（cancelled 视图 + 墓碑）；他人 404；已派发 409；
/// cancelled actionId 同载荷重放发送 → 明确不重发回执。
#[tokio::test]
async fn queue_retract_ownership_race_and_tombstone() {
    let f = fixture_ipc(vec![codex_app_session(THREAD)]);
    seed_device(&f.conn, "dev-2", "iPad");
    let store = f.hub_store();
    let ctx = ActionContext {
        action_id: "act-rt-1".into(),
        device_id: "dev-1".into(),
        device_name: "iPhone".into(),
        kind: ActionKind::Send,
        key: f.thread_key(),
    };
    let real_hash = payload_hash(
        ActionKind::Send,
        &f.thread_key(),
        None,
        None,
        &ActionPayload::Text("排队中".into()),
    );
    store
        .register(
            &ctx,
            ActionKind::Send,
            None,
            None,
            &real_hash,
            Some("排队中"),
        )
        .unwrap();

    // 他人设备 → 404
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/queue/retract",
            Some("mam_device=dev-2"),
            Some(serde_json::json!({ "actionId": "act-rt-1" }).to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 404, "他人动作 404");

    // 本设备撤回 → 200 cancelled
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/queue/retract",
            Some(COOKIE),
            Some(serde_json::json!({ "actionId": "act-rt-1" }).to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["state"], "cancelled");

    // 再撤 → 409（墓碑已终态）
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/queue/retract",
            Some(COOKIE),
            Some(serde_json::json!({ "actionId": "act-rt-1" }).to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 409, "已终态撤回 409");

    // cancelled actionId 同载荷重放发送 → 200 明确不重发回执（not_sent action_cancelled）
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(send_body("排队中", Some("act-rt-1"), false)),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["status"], "desktop_ipc");
    assert_eq!(v["receipt"]["delivery"], "not_sent");
    assert_eq!(v["receipt"]["reasonCode"], "action_cancelled");

    // 撤回竞态 409：queued 动作先 reserve（并发派发赢者）→ retract 409
    let ctx2 = ActionContext {
        action_id: "act-rt-2".into(),
        device_id: "dev-1".into(),
        device_name: "iPhone".into(),
        kind: ActionKind::Send,
        key: f.thread_key(),
    };
    store
        .register(&ctx2, ActionKind::Send, None, None, "h", Some("竞态"))
        .unwrap();
    assert!(store.reserve("act-rt-2", ControlRoute::DesktopIpc).unwrap());
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/queue/retract",
            Some(COOKIE),
            Some(serde_json::json!({ "actionId": "act-rt-2" }).to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 409, "已派发不可撤");
}

// ============================================================
// 人工解除「无出口保守屏障」（2026-10-19 复审 Important）：POST /codex/queue/force-continue
// ============================================================

/// 造「unknown + 保守屏障」账本行（复审列出的四条成因收敛后的统一终态）：
/// register → reserve → finish_with_turn_barrier（UnknownDelivery，缺 turnId）。
fn seed_unknown_barrier(store: &ActionStore, action_id: &str) {
    let ctx = ActionContext {
        action_id: action_id.to_string(),
        device_id: "dev-1".into(),
        device_name: "iPhone".into(),
        kind: ActionKind::Send,
        key: ThreadKey {
            host_id: "local".into(),
            thread_id: THREAD.into(),
        },
    };
    store
        .register(&ctx, ActionKind::Send, None, None, "h", Some("未知投递"))
        .unwrap();
    assert!(store.reserve(action_id, ControlRoute::DesktopIpc).unwrap());
    store
        .finish_with_turn_barrier(
            &ActionReceipt {
                action_id: action_id.to_string(),
                delivery: Delivery::UnknownDelivery,
                route: ControlRoute::DesktopIpc,
                turn_id: None,
                envelope_id: None,
                reason_code: Some("rpc_timeout".into()),
            },
            None,
        )
        .unwrap();
}

async fn force_continue(
    f: &Fixture,
    cookie: Option<&str>,
    action_id: &str,
) -> axum::http::Response<axum::body::Body> {
    f.app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/queue/force-continue",
            cookie,
            Some(serde_json::json!({ "actionId": action_id }).to_string()),
        ))
        .await
        .unwrap()
}

async fn queue_view_json(f: &Fixture) -> serde_json::Value {
    let r = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/queue?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    serde_json::from_str(&body_string(r).await).unwrap()
}

/// 成功路径：清屏障 + 队列视图解除（blockedReasonCode/barrierActionIds 双清）+
/// 审计在册；解除断言不带存在性预言机。
#[tokio::test]
async fn force_continue_releases_unknown_barrier_and_updates_queue_view() {
    let f = fixture_ipc(vec![codex_app_session(THREAD)]);
    seed_unknown_barrier(&f.hub_store(), "act-unk-1");

    // 解除前：队列视图报 native_delivery_unknown + 屏障动作清单
    let v = queue_view_json(&f).await;
    assert_eq!(v["blockedReasonCode"], "native_delivery_unknown");
    assert_eq!(v["barrierActionIds"], serde_json::json!(["act-unk-1"]));
    assert!(
        f.hub_store().has_compat_fence(&f.thread_key()).unwrap(),
        "屏障在册"
    );

    // 解除：200 + 视图原状态（unknown）返回
    let r = force_continue(&f, Some(COOKIE), "act-unk-1").await;
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["actionId"], "act-unk-1");
    assert_eq!(v["state"], "unknown", "状态不被改写（只清屏障）");

    // 持久屏障清 + 队列视图解除（三态派生不再报阻塞）
    assert!(
        !f.hub_store().has_compat_fence(&f.thread_key()).unwrap(),
        "屏障已清"
    );
    let v = queue_view_json(&f).await;
    assert!(
        v["blockedReasonCode"].is_null(),
        "解除后队列视图不再报阻塞：{v}"
    );
    assert_eq!(v["barrierActionIds"], serde_json::json!([]));

    // 审计在册：恰好一条 force_continue（谁/哪个 action/何时）
    let conn = f.conn.lock().unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT device_id, ts, summary, result FROM write_audit \
             WHERE action = 'force_continue'",
        )
        .unwrap();
    let rows: Vec<(String, i64, String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(rows.len(), 1, "解除写恰好一条审计");
    assert_eq!(rows[0].0, "dev-1", "谁");
    assert!(rows[0].1 > 0, "何时（ts 在册）");
    assert!(
        rows[0].2.contains("act-unk-1"),
        "哪个 action：{}",
        rows[0].2
    );
    assert_eq!(rows[0].3, "barrier_force_released", "结果词");
}

/// 鉴权与隔离矩阵：非属主设备 404；不存在 404；queued 态 409 queued_use_retract
/// （指引走既有撤回）；reserved+A 路无屏障 409 no_barrier；reserved+compat 兜底在飞
///（fence 未武装瞬态窗口）与 reserved+在飞 fence 均 409 barrier_in_flight（终审
/// Minor：有自动落账路径不得谎报 no_barrier）；缺 actionId 400。
#[tokio::test]
async fn force_continue_ownership_and_state_machine_matrix() {
    let f = fixture_ipc(vec![codex_app_session(THREAD)]);
    seed_device(&f.conn, "dev-2", "iPad");
    let store = f.hub_store();
    seed_unknown_barrier(&store, "act-own-1");

    // 非属主设备 → 404（不给存在性预言机）
    let r = force_continue(&f, Some("mam_device=dev-2"), "act-own-1").await;
    assert_eq!(r.status(), 404, "非属主 404");
    assert!(
        f.hub_store().has_compat_fence(&f.thread_key()).unwrap(),
        "非属主请求不触碰屏障"
    );

    // 不存在 → 404
    let r = force_continue(&f, Some(COOKIE), "no-such-action").await;
    assert_eq!(r.status(), 404, "不存在 404");

    // queued 态 → 409 + queued_use_retract（指引走既有撤回）
    let ctx = ActionContext {
        action_id: "act-q-1".into(),
        device_id: "dev-1".into(),
        device_name: "iPhone".into(),
        kind: ActionKind::Send,
        key: f.thread_key(),
    };
    store
        .register(&ctx, ActionKind::Send, None, None, "h", Some("排队中"))
        .unwrap();
    let r = force_continue(&f, Some(COOKIE), "act-q-1").await;
    assert_eq!(r.status(), 409, "queued 走撤回不走解除");
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["error"], "queued_use_retract");
    assert_eq!(
        f.hub_store()
            .get_for_device("act-q-1", "dev-1")
            .unwrap()
            .unwrap()
            .state,
        ActionState::Queued,
        "queued 行不被触碰（仍可撤）"
    );

    // reserved + A 路无屏障 → 409 no_barrier（A 路在飞无自动兜底交接路径语义的
    // 屏障可解性口径不变：真正无屏障可解才报 no_barrier）
    assert!(store.reserve("act-q-1", ControlRoute::DesktopIpc).unwrap());
    let r = force_continue(&f, Some(COOKIE), "act-q-1").await;
    assert_eq!(r.status(), 409);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["error"], "no_barrier");

    // reserved + compat 兜底回合在飞（handoff 已落、fence 未武装的瞬态窗口）→
    // 409 barrier_in_flight（终审 Minor：该行即将走自动落账，谎报 no_barrier 会
    // 让前端误判「无需解除」、排障者误判状态）
    let ctx_fb = ActionContext {
        action_id: "act-fb-1".into(),
        device_id: "dev-1".into(),
        device_name: "iPhone".into(),
        kind: ActionKind::Send,
        key: f.thread_key(),
    };
    store
        .register(&ctx_fb, ActionKind::Send, None, None, "h", Some("排队中"))
        .unwrap();
    assert!(store
        .reserve("act-fb-1", ControlRoute::HeadlessCompat)
        .unwrap());
    let r = force_continue(&f, Some(COOKIE), "act-fb-1").await;
    assert_eq!(r.status(), 409, "兜底在飞瞬态窗口");
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["error"], "barrier_in_flight");

    // reserved + 在飞 fence（屏障在册但不在解除范围）→ 409 barrier_in_flight
    // （复审 Minor：屏障明明在册，回 no_barrier 会误导排障；reason 说明真实状态）
    store.set_compat_fence("act-q-1", None, None).unwrap();
    let r = force_continue(&f, Some(COOKIE), "act-q-1").await;
    assert_eq!(r.status(), 409);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["error"], "barrier_in_flight");
    assert!(
        v["reason"].as_str().is_some_and(|s| !s.is_empty()),
        "barrier_in_flight 携带可读 reason"
    );

    // 缺 actionId → 400
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/queue/force-continue",
            Some(COOKIE),
            Some(serde_json::json!({}).to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 400);

    // 屏障行仍完好（上述 4xx 全部零写入）
    assert!(
        f.hub_store().has_compat_fence(&f.thread_key()).unwrap(),
        "4xx 不触碰屏障"
    );
}

/// 幂等：重复调用返回当前状态（200），审计不重复；解除后其他路径不回置屏障
/// （启动恢复 / 超龄清扫 / bind_turn 补绑的守卫面）。
#[tokio::test]
async fn force_continue_idempotent_and_monotonic() {
    let f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let store = f.hub_store();
    seed_unknown_barrier(&store, "act-idem-1");

    let r = force_continue(&f, Some(COOKIE), "act-idem-1").await;
    assert_eq!(r.status(), 200, "首次解除成功");

    // 重复调用：幂等 200（当前状态），审计仍是一条
    let r = force_continue(&f, Some(COOKIE), "act-idem-1").await;
    assert_eq!(r.status(), 200, "重复调用幂等返回当前状态");
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["state"], "unknown");

    // 解除后其他路径不回置屏障：
    // ① 启动恢复（recover_reserved 只触 reserved）与超龄清扫不复活
    store.recover_reserved().unwrap();
    store.mark_stale_reserved_unknown(0);
    // ② bind_turn 补绑（set_compat_fence 写点）带单调性守卫
    store
        .set_compat_fence("act-idem-1", Some("m"), Some("t"))
        .unwrap();
    // ③ 原子派发屏障写点同样被守卫
    store
        .set_compat_dispatch_barrier(
            "act-idem-1",
            Some("m"),
            Some("t"),
            Some("/p"),
            Some(1),
            Some("w"),
        )
        .unwrap();
    assert!(
        !store.has_compat_fence(&f.thread_key()).unwrap(),
        "解除后任何路径不回置屏障（单调性）"
    );

    // 审计仍恰好一条（幂等调用零新增）
    let conn = f.conn.lock().unwrap();
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM write_audit WHERE action = 'force_continue'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1, "绝不重复写审计");
}

/// accepted+turnId 绑定屏障（native_turn_pending，有自动路径）同样允许显式覆盖：
/// 队列视图报 native_turn_pending 时，人工解除也有效（用户核对后的越权出口统一面）。
#[tokio::test]
async fn force_continue_also_releases_accepted_turn_binding_barrier() {
    let f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let store = f.hub_store();
    let ctx = ActionContext {
        action_id: "act-acc-1".into(),
        device_id: "dev-1".into(),
        device_name: "iPhone".into(),
        kind: ActionKind::Send,
        key: f.thread_key(),
    };
    store
        .register(&ctx, ActionKind::Send, None, None, "h", Some("已接受"))
        .unwrap();
    assert!(store
        .reserve("act-acc-1", ControlRoute::DesktopIpc)
        .unwrap());
    store
        .finish_with_turn_barrier(
            &ActionReceipt {
                action_id: "act-acc-1".into(),
                delivery: Delivery::Accepted,
                route: ControlRoute::DesktopIpc,
                turn_id: Some("turn-stuck".into()),
                envelope_id: None,
                reason_code: None,
            },
            Some("turn-stuck"),
        )
        .unwrap();

    let v = queue_view_json(&f).await;
    assert_eq!(v["blockedReasonCode"], "native_turn_pending");

    let r = force_continue(&f, Some(COOKIE), "act-acc-1").await;
    assert_eq!(r.status(), 200, "绑定屏障允许桌面端核对后的显式覆盖");
    assert!(
        !store.has_compat_fence(&f.thread_key()).unwrap(),
        "绑定屏障已清"
    );
    let v = queue_view_json(&f).await;
    assert!(v["blockedReasonCode"].is_null());
}

/// queueOnly 发送 → codex_queued（只排队不派发）；hub.stop() 后未派发项仍 queued。
#[tokio::test]
async fn lifecycle_queue_only_survives_hub_stop() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let body = send_body("只排队", Some("act-qq-1"), true);
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    // **queueOnly 不派发**（零 IPC——不 drive_ensure：peer 永远收不到帧才是正确断言）
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("in time")
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["status"], "codex_queued");
    // hub stop（remote 关闭语义）：未派发项仍 queued，不派发
    f.state.codex_control.stop();
    let view = f
        .hub_store()
        .get_for_device("act-qq-1", "dev-1")
        .unwrap()
        .unwrap();
    assert_eq!(view.state, ActionState::Queued);
    assert!(!peer_has_traffic(&mut peer).await, "stop 后零派发");
}

// ============================================================
// 7. SSE：首帧 snapshot + 吊销即时关闭
// ============================================================

#[tokio::test]
async fn sse_stream_snapshots_and_revocation_closes() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread/events?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("sse in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    let mut body = r.into_body();
    use http_body_util::BodyExt;
    // 首帧 snapshot（ThreadProjection）
    let first = tokio::time::timeout(Duration::from_secs(10), body.frame())
        .await
        .expect("first frame")
        .unwrap()
        .unwrap();
    let text = String::from_utf8(first.data_ref().unwrap().to_vec()).unwrap();
    assert!(
        text.contains("\"threadId\""),
        "首帧为 ThreadProjection 快照：{text}"
    );
    // 吊销 → 即时断流（EOF）
    f.state.sse_registry.disconnect_device("dev-1");
    loop {
        match tokio::time::timeout(Duration::from_secs(5), body.frame()).await {
            Ok(Some(_)) => continue, // 缓冲帧
            Ok(None) => break,       // EOF：流已关
            Err(_) => panic!("吊销后流未在超时内关闭"),
        }
    }
}

// ============================================================
// 8. 旧 APP 补投入口（native_only 503）与 codex app-server 补投臂
//
// **本轮不交付**：该臂属 PR #117 续作（codex app-server 控制面）。本交付对
// Codex APP 问答沿用既有 409 app_form_pending 止血门（主仓既有测试
// codex_app_form 三入口 409 覆盖），不伪造普通消息补投。
// ============================================================

// 9. CLI 工具零漂移 + 审计哨兵
/// CLI 会话（非 codex APP）携带多余 actionId 键 → 旧合同零漂移（键被忽略，走老路）。
/// （S6 集成验收发现：S2 重写时把 `#[tokio::test]` 与本文档一并削去，用例自此
/// 孤儿化不再运行——clippy 死代码告警暴露；本提交恢复属性，覆盖归位。）
#[tokio::test]
async fn claude_session_send_ignores_action_id_key() {
    let f = fixture_disabled(vec![claude_cli_session(
        "claude-s-1",
        SessionStatus::Processing,
    )]);
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(
                serde_json::json!({
                    "sessionId": "claude-s-1", "text": "普通消息", "actionId": "not-for-cli",
                })
                .to_string(),
            ),
        ))
        .await
        .unwrap();
    let status = r.status();
    let body = body_string(r).await;
    // Linux CI 没有终端注入通道（routing 明确返回 platform），该用例只应在
    // Windows/macOS 上验证 CLI 零漂移；Linux 上如实断言平台拒绝，避免把
    // “平台不支持”误判为 200 回归。
    if !matches!(std::env::consts::OS, "windows" | "macos") {
        assert_eq!(status, 403);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["error"], "not_injectable");
        assert_eq!(v["reasonCode"], "platform");
        return;
    }
    assert_eq!(status, 200);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["status"], "queued", "CLI 黄态照旧入 inject_queue");
    assert!(v.get("actionId").is_none(), "CLI 回执无 actionId 语义");
    assert!(f.codex_audits().is_empty(), "CLI 路径不写 codex_* 审计");
}

/// 审计哨兵：正文哨兵 / cookie 不出现在任何审计行（答案原文同规，见回答用例）。
#[tokio::test]
async fn audit_never_leaks_secrets() {
    let secret = "SK-SENTINEL-abc123";
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    // 发送一条正文含哨兵的消息（走完整 IPC 接受路径）
    let body = serde_json::json!({
        "sessionId": THREAD, "text": format!("正文带 {secret}"),
        "actionId": "act-secret-1",
    });
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let frame = peer.recv().await;
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "turn": { "id": "t", "status": "inProgress" } }
    }))
    .await;
    tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("in time")
        .unwrap();
    for summary in f.all_audit_summaries() {
        assert!(
            !summary.contains(secret),
            "审计 summary 泄漏正文哨兵：{summary}"
        );
        assert!(!summary.contains(COOKIE), "审计泄漏 cookie：{summary}");
    }
}

// ============================================================
// 10. R6.2：忙态首 snapshot——首次直接 POST 且假 Desktop 直接给 active → 只入队
// 零 start-turn 帧（不再「无镜像恒 false → 直接 start-turn」）。
// ============================================================

#[tokio::test]
async fn first_post_with_active_snapshot_queues_without_start_turn() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let body = send_body("首条即忙", Some("act-busy-first"), false);
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    // 无任何先前 GET：busy_fresh 先 ensure（本驱动直接给 active snapshot）
    drive_ensure(&mut peer, THREAD, vec![], "active").await;
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("send in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["status"], "codex_queued", "首 snapshot active → 只入队");
    assert_eq!(v["actionId"], "act-busy-first");
    assert_eq!(v["position"], serde_json::json!(1), "首个待发位置为 1");
    let view = f
        .hub_store()
        .get_for_device("act-busy-first", "dev-1")
        .unwrap()
        .unwrap();
    assert_eq!(view.state, ActionState::Queued);
    assert!(
        !peer_has_traffic(&mut peer).await,
        "忙态首 POST 零 start-turn 帧（R6.2）"
    );
}

// ============================================================
// 11. R6.3a：start-turn 回执按真实封套解包 turnId（Mac Probe 记录嵌套两层
// result.result.turn）；缺 turnId 如实回 None。
// ============================================================

#[tokio::test]
async fn nested_turn_receipt_yields_turn_id() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let body = send_body("嵌套回执", Some("act-nested-1"), false);
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let frame = peer.recv().await;
    confirm_client_message_identity(&mut peer, &frame, "turn-nested", "嵌套回执", 8, "owner-1")
        .await;
    assert_eq!(frame["method"], "thread-follower-start-turn");
    // Mac Probe 实录形态：turn 嵌套两层
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "result": { "turn": { "id": "turn-nested", "status": "inProgress" } } }
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(20), task)
        .await
        .expect("in time")
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["status"], "desktop_ipc");
    assert_eq!(v["receipt"]["delivery"], "accepted");
    assert_eq!(
        v["receipt"]["turnId"], "turn-nested",
        "嵌套两层封套的 turnId 必须解出（R6.3a）"
    );
    let view = f
        .hub_store()
        .get_for_device("act-nested-1", "dev-1")
        .unwrap()
        .unwrap();
    assert_eq!(
        view.receipt.unwrap().turn_id.as_deref(),
        Some("turn-nested")
    );
}

// ============================================================
// 12. R8.1/R8.3 + #172：未知 build（disabled hub，写能力未取证）+ H3 开 → 普通消息
// 仍先经统一 router（登记→reserve→NotSent），但**如实拒发 capability_unverified**
//（不再静默改走 compat）；同 action 重放 / 重建 hub 重放均只投一次。
// ============================================================

/// 【S2 重写（原 `unknown_build_send_routes_through_ledger_dedup`）】旧断言「未知
/// build + disabled hub：经 router 登记 → permit NotSent 落账 → 重放返回账本既有
/// 回执」——S2 起可发送判据的**静态门**前移到 router 之前：不可尝试（hub disabled
/// = 无任何尝试通道）→ 403 拒绝且**零账本行**；重放同 ID 同样在门处拒绝（不存在
/// 「重放回执」，因为从未有写尝试；账本终态去重只约束真正进入过派发的动作）。
#[tokio::test]
async fn send_without_attemptable_refuses_at_door_without_ledger_row() {
    let f = fixture_disabled(vec![codex_app_session(THREAD)]);
    // 首投：静态门拒绝（零登记零派发）
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(send_body("未知 build 消息", Some("act-ub-1"), false)),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 403, "未知 build + disabled hub：如实拒绝");
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["error"], "capability_unverified", "{v}");
    assert!(
        f.hub_store()
            .get_for_device("act-ub-1", "dev-1")
            .expect("store query")
            .is_none(),
        "门拒不落账本行"
    );
    let audits = f.codex_audits();
    assert!(
        !audits.iter().any(|a| a.action == "codex_fallback"),
        "零兜底分支: {audits:?}"
    );
    assert!(
        audits
            .iter()
            .any(|a| a.action == "codex_send" && a.result.contains("refused:capability_unverified")),
        "拒发留审计"
    );

    // 同 action 重放：同样门拒（幂等——无写尝试就无账本去重面）
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(send_body("未知 build 消息", Some("act-ub-1"), false)),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 403, "重放同样门拒");

    // queue/exec 通道零投递
    let queue_rows: i64 = {
        let conn = f.conn.lock().unwrap();
        conn.query_row("SELECT COUNT(*) FROM inject_queue", [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(queue_rows, 0);
}

// ============================================================
// 13.【S2 删除】原 `compat_barrier_persist_failure_skips_helper`（R9.1）：兜底
// bridge 的「发送前原子持久屏障，写失败零 helper 调用」场景随 bridge 删除——
// 屏障置位的单调性守卫（barrier_force_released_at / 重启不回置）由
// `force_continue_idempotent_and_monotonic` 继续钉住；Turn 顺序屏障的原子持久化
// 由 `accepted_and_unknown_persist_turn_barrier_atomically` 继续钉住。
// ============================================================

// ============================================================
// 14. R7.1：hub 启动恢复——丢弃全部内存重建 hub 后，旧 queued 行不打开详情、
// 不手工 flush 即按序发出（首个对账 tick 即启动恢复）。
// ============================================================

fn make_adapter_peer() -> (Arc<DesktopIpcAdapter>, ScriptedPeer) {
    let (io, peer) = ScriptedPeer::new();
    (
        Arc::new(DesktopIpcAdapter::new(
            Arc::new(StaticConnector::new(io)),
            caps_open_profile(),
        )),
        peer,
    )
}

#[tokio::test]
async fn hub_restart_recovery_replays_queued_without_details() {
    let conn = shared_conn();
    seed_device(&conn, "dev-1", "iPhone");
    // —— 阶段一：旧 hub 上 queueOnly 排队（零 IPC），随后丢弃全部内存 ——
    {
        let (adapter, _peer1) = make_adapter_peer();
        let f1 = fixture_on(
            conn.clone(),
            vec![codex_app_session(THREAD)],
            move |store| Arc::new(CodexControlHub::with_adapter(store, adapter, true)),
            None,
        );
        let r = f1
            .app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some(COOKIE),
                Some(send_body("重启前排队", Some("act-rec-1"), true)),
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["status"], "codex_queued");
        assert_eq!(
            f1.hub_store()
                .get_for_device("act-rec-1", "dev-1")
                .unwrap()
                .unwrap()
                .state,
            ActionState::Queued
        );
        // f1 在此 drop：hub/adapter/调度器全部丢弃，仅持久账本（共享 conn）留存
    }
    // —— 阶段二：重建 hub（全新 adapter/peer），start 后不打开详情、不手工 flush ——
    let (adapter2, peer2) = make_adapter_peer();
    let mut f2 = fixture_on(
        conn.clone(),
        vec![codex_app_session(THREAD)],
        move |store| Arc::new(CodexControlHub::with_adapter(store, adapter2, true)),
        Some(peer2),
    );
    f2.state
        .codex_control
        .set_reconcile_period_ms(std::time::Duration::from_millis(200).as_millis() as u64);
    f2.state.codex_control.start(f2.state.clone());
    {
        let mut peer = f2.peer.take().unwrap();
        // 启动恢复经 ensure 重新 following（事件驱动调度），队首按序发出
        drive_ensure(&mut peer, THREAD, vec![], "idle").await;
        let frame = peer.recv().await;
        confirm_client_message_identity(&mut peer, &frame, "turn-rec", "重启前排队", 8, "owner-1")
            .await;
        assert_eq!(
            frame["method"], "thread-follower-start-turn",
            "恢复后旧 queued 行自动发出"
        );
        assert_eq!(
            frame["params"]["turnStart"]["request"]["input"][0]["text"],
            "重启前排队"
        );
        peer.send(&serde_json::json!({
            "type": "response", "requestId": frame["requestId"],
            "resultType": "success", "method": "thread-follower-start-turn",
            "handledByClientId": "owner-1",
            "result": { "turn": { "id": "turn-rec", "status": "inProgress" } }
        }))
        .await;
    }
    let view = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(Some(v)) = f2.hub_store().get_for_device("act-rec-1", "dev-1") {
                if v.state == ActionState::Accepted {
                    return v;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("recovery dispatch settles");
    assert_eq!(view.receipt.unwrap().turn_id.as_deref(), Some("turn-rec"));
    f2.state.codex_control.stop();
}

// ============================================================
// 15. R7.2：周期对账——hub 运行中新入队的行，未经任何事件/手工触发，下一对账
// 周期即被接力发出。
// ============================================================

#[tokio::test]
async fn periodic_reconcile_dispatches_newly_queued() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let peer = f.peer.take().unwrap();
    f.state
        .codex_control
        .set_reconcile_period_ms(std::time::Duration::from_millis(200).as_millis() as u64);
    f.state.codex_control.start(f.state.clone());
    // hub 运行后（首个恢复 tick 已过、集合为空）再入队——只能靠周期对账接力
    let ctx = ActionContext {
        action_id: "act-tick-1".into(),
        device_id: "dev-1".into(),
        device_name: "iPhone".into(),
        kind: ActionKind::Send,
        key: ThreadKey {
            host_id: "local".into(),
            thread_id: THREAD.into(),
        },
    };
    let hash = payload_hash(
        ActionKind::Send,
        &ctx.key,
        None,
        None,
        &ActionPayload::Text("对账接力".into()),
    );
    f.hub_store()
        .register(&ctx, ActionKind::Send, None, None, &hash, Some("对账接力"))
        .unwrap();

    // 对端服务全程协议（ensure → start-turn → 成功回执）
    let driver = tokio::spawn(async move {
        let mut peer = peer;
        drive_ensure(&mut peer, THREAD, vec![], "idle").await;
        let frame = peer.recv().await;
        confirm_client_message_identity(&mut peer, &frame, "turn-tick", "对账接力", 8, "owner-1")
            .await;
        assert_eq!(frame["method"], "thread-follower-start-turn");
        peer.send(&serde_json::json!({
            "type": "response", "requestId": frame["requestId"],
            "resultType": "success", "method": "thread-follower-start-turn",
            "handledByClientId": "owner-1",
            "result": { "turn": { "id": "turn-tick", "status": "inProgress" } }
        }))
        .await;
        peer
    });
    let view = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(Some(v)) = f.hub_store().get_for_device("act-tick-1", "dev-1") {
                if v.state == ActionState::Accepted {
                    return v;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("periodic reconcile dispatches");
    assert_eq!(view.receipt.unwrap().turn_id.as_deref(), Some("turn-tick"));
    f.state.codex_control.stop();
    let _ = driver.await;
}

// ============================================================
// 16. R7.3：关注生命周期——连续 65 次挂载/卸载（SSE 订阅/关闭）名额不耗尽；
// 64 次失败 discovery 不耗尽（失败回滚）。
// ============================================================

/// 重复 follow 的协议驱动（**不含握手**——连接与 initialize 一次握手后复用，
/// 每轮只有 discovery → following → snapshot）。snapshot revision 可变（镜像复活用）。
async fn drive_ensure_rev(
    peer: &mut ScriptedPeer,
    thread: &str,
    requests: Vec<serde_json::Value>,
    runtime: &str,
    revision: u64,
) {
    let disc = peer.recv().await;
    assert_eq!(disc["method"], "thread-owner-discovery");
    peer.send(&discovery_response(
        disc["requestId"].as_str().unwrap(),
        "owner-1",
    ))
    .await;
    let follow = peer.recv().await;
    assert_eq!(follow["method"], "thread-stream-following-changed");
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": thread, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": revision,
                "conversationState": {
                    "turns": [], "requests": requests,
                    "threadRuntimeStatus": { "type": runtime },
                    "turnHistory": codex_fixture::canonical_turn_history(serde_json::json!({
                            "t1": { "id": "t1", "type": "turn", "status": "completed", "items": ["u1"] },
                            "u1": { "id": "u1", "type": "user_message", "text": "seed", "ts": 1 }
                        }), serde_json::json!([{ "entries": ["t1"] }]))
                }
            }
        }
    }))
    .await;
}

#[tokio::test]
async fn sse_mount_unmount_cycles_do_not_exhaust_following() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    for cycle in 0..65u32 {
        let app = f.app.clone();
        let task = tokio::spawn(async move {
            app.oneshot(req(
                "GET",
                &format!("/m/api/v1/codex/thread/events?session_id={THREAD}"),
                Some(COOKIE),
                None,
            ))
            .await
            .unwrap()
        });
        // 连接只握手一次（initialize 单次；后续挂载复用同一连接）
        if cycle == 0 {
            handshake(&mut peer, "owner-1").await;
        }
        drive_ensure_rev(&mut peer, THREAD, vec![], "idle", 7 + cycle as u64).await;
        let r = tokio::time::timeout(Duration::from_secs(15), task)
            .await
            .expect("sse in time")
            .unwrap();
        assert_eq!(r.status(), 200);
        let mut body = r.into_body();
        use http_body_util::BodyExt;
        let first = tokio::time::timeout(Duration::from_secs(10), body.frame())
            .await
            .expect("first frame")
            .unwrap()
            .unwrap();
        let text = String::from_utf8(first.data_ref().unwrap().to_vec()).unwrap();
        assert!(
            text.contains("\"stale\":false"),
            "第 {cycle} 次挂载必须拿到新鲜投影（名额未耗尽）：{text}"
        );
        drop(body);
        // 引用归零 → unfollow 释放名额（Drop 内 spawn）：C4 起 unfollow 会真正发
        // following:false——消费它再进入下一轮（下一轮 discovery 之前必达）
        tokio::time::sleep(Duration::from_millis(150)).await;
        let off = peer.recv().await;
        assert_eq!(
            off["method"], "thread-stream-following-changed",
            "第 {cycle} 次卸载必须声明 following:false"
        );
        assert_eq!(off["params"]["following"], serde_json::json!(false));
    }
}

#[tokio::test]
async fn failed_discovery_rolls_back_following_slot() {
    // 64 个不同 thread 的 GET 全部 discovery 失败 → 名额回滚；第 65 个完整 follow 仍可用
    let mut sessions = Vec::new();
    for i in 0..65u32 {
        let id = format!("aaaaaaaa-bbbb-4ccc-8ddd-{i:012}");
        sessions.push(codex_app_session(&id));
    }
    let mut f = fixture_ipc(sessions);
    let mut peer = f.peer.take().unwrap();
    // 首次握手在首个 GET 建连后（连接复用；后续 GET 只有 discovery 往返）
    for i in 0..64u32 {
        let id = format!("aaaaaaaa-bbbb-4ccc-8ddd-{i:012}");
        let app = f.app.clone();
        let task = tokio::spawn(async move {
            app.oneshot(req(
                "GET",
                &format!("/m/api/v1/codex/thread?session_id={id}"),
                Some(COOKIE),
                None,
            ))
            .await
            .unwrap()
        });
        if i == 0 {
            handshake(&mut peer, "owner-1").await;
        }
        let disc = peer.recv().await;
        assert_eq!(disc["method"], "thread-owner-discovery");
        // discovery 失败（no-client-found 语义）
        peer.send(&serde_json::json!({
            "type": "response", "requestId": disc["requestId"],
            "resultType": "error", "method": "thread-owner-discovery",
            "handledByClientId": "owner-1",
            "error": { "message": "no-client-found" }
        }))
        .await;
        let r = tokio::time::timeout(Duration::from_secs(15), task)
            .await
            .expect("get in time")
            .unwrap();
        assert_eq!(r.status(), 200, "ensure 失败回落 stale 投影仍 200");
    }
    // 第 65 个：完整成功 follow——若失败未回滚名额，这里会 Capacity
    let id65 = format!("aaaaaaaa-bbbb-4ccc-8ddd-{:012}", 64);
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={id65}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    drive_ensure_rev(
        &mut peer,
        &format!("aaaaaaaa-bbbb-4ccc-8ddd-{:012}", 64),
        vec![],
        "idle",
        7,
    )
    .await;
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("get in time")
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(
        v["stale"],
        serde_json::json!(false),
        "64 次失败 discovery 后名额未耗尽（失败回滚）"
    );
}

// ============================================================
// 17. 全链生产装配（审计交接要求 3）：production 形态派发缝（AdapterDispatch 包
// 真 adapter，不用 FakeDispatch 替身）+ hub.start 调度循环 + 真 router/bridge——
// HTTP send → 假 Desktop → 账本终态 → idle 投影释放顺序限制 → flush 接力。
// ============================================================

#[tokio::test]
async fn production_chain_send_busy_queue_then_idle_relay() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    // 生产生命周期装配（真实调度循环；对账周期收窄以便测试观察）
    f.state
        .codex_control
        .set_reconcile_period_ms(std::time::Duration::from_millis(200).as_millis() as u64);
    f.state.codex_control.start(f.state.clone());

    // ① 直接 POST send#1：ensure（idle snapshot）→ start-turn → accepted
    let body1 = send_body("全链第一条", Some("act-chain-1"), false);
    let task1 = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body1),
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let frame = peer.recv().await;
    assert_eq!(frame["method"], "thread-follower-start-turn");
    confirm_client_message_identity(
        &mut peer,
        &frame,
        "turn-chain-1",
        "全链第一条",
        8,
        "owner-1",
    )
    .await;
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "turn": { "id": "turn-chain-1", "status": "inProgress" } }
    }))
    .await;
    let r1 = tokio::time::timeout(Duration::from_secs(20), task1)
        .await
        .expect("chain send 1 in time")
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body_string(r1).await).unwrap();
    assert_eq!(v["receipt"]["delivery"], "accepted");
    assert_eq!(v["receipt"]["turnId"], "turn-chain-1");

    // ② 假 Desktop 推 active snapshot → send#2 忙态入队
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": THREAD, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": 9,
                "conversationState": {
                    "turns": [], "requests": [],
                    "threadRuntimeStatus": { "type": "active" },
                    "turnHistory": codex_fixture::canonical_turn_history(serde_json::json!({
                            "t9": { "id": "t9", "type": "turn", "status": "inProgress", "items": ["u9"] },
                            "u9": { "id": "u9", "type": "user_message", "text": "全链第一条 [mobile iPhone]", "ts": 9 }
                        }), serde_json::json!([{ "entries": ["t9"] }]))
                }
            }
        }
    }))
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let r2 = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(send_body("全链第二条", Some("act-chain-2"), false)),
        ))
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body_string(r2).await).unwrap();
    assert_eq!(v["status"], "codex_queued", "忙态入队");
    assert_eq!(
        f.hub_store()
            .get_for_device("act-chain-2", "dev-1")
            .unwrap()
            .unwrap()
            .state,
        ActionState::Queued
    );

    // ③ 假 Desktop 推 idle snapshot（turn-chain-1 已终态）→ 顺序限制释放 → flush 接力
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": THREAD, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": 10,
                "conversationState": {
                    "turns": [], "requests": [],
                    "threadRuntimeStatus": { "type": "idle" },
                    "turnHistory": codex_fixture::canonical_turn_history(serde_json::json!({
                            "turn-chain-1": { "id": "turn-chain-1", "type": "turn", "status": "completed", "items": ["u9"] },
                            "t9": { "id": "t9", "type": "turn", "status": "completed", "items": ["u9"] },
                            "u9": { "id": "u9", "type": "user_message", "text": "全链第一条 [mobile iPhone]", "ts": 9 }
                        }), serde_json::json!([{ "entries": ["turn-chain-1", "t9"] }]))
                }
            }
        }
    }))
    .await;
    // flush 接力发出 send#2
    let frame2 = tokio::time::timeout(Duration::from_secs(15), peer.recv())
        .await
        .expect("relay start-turn");
    assert_eq!(
        frame2["method"], "thread-follower-start-turn",
        "idle 投影驱动释放 + flush 接力（R6.3 + 全链）"
    );
    assert_eq!(
        frame2["params"]["turnStart"]["request"]["input"][0]["text"],
        "全链第二条"
    );
    confirm_client_message_identity(
        &mut peer,
        &frame2,
        "turn-chain-2",
        "全链第二条",
        11,
        "owner-1",
    )
    .await;
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame2["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "turn": { "id": "turn-chain-2", "status": "inProgress" } }
    }))
    .await;
    let v2 = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(Some(v)) = f.hub_store().get_for_device("act-chain-2", "dev-1") {
                if v.state == ActionState::Accepted {
                    return v;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("chain send 2 settles");
    assert_eq!(v2.receipt.unwrap().turn_id.as_deref(), Some("turn-chain-2"));
    // 顺序限制已随 turn-chain-2 的绑定入册（屏障在册 → 队列视图如实说明）
    assert!(
        f.hub_store()
            .has_compat_fence(&ThreadKey {
                host_id: "local".into(),
                thread_id: THREAD.into()
            })
            .unwrap(),
        "在飞 Turn 绑定以屏障槽位持久（blockedReason 语义）"
    );
    f.state.codex_control.stop();
}

/// 2026-10-17 复审 C1：等待者被取消后的收敛仍以「**绑定 Turn** 的终态证据」为准。
/// Accepted(turn-cvrg-1) 后：泛 idle（另一 turn t6 完成、绑定 turn 不在场）经多轮
/// 对账也**不得**接力第二条（修正旧预期「另一 turn 完成 + 泛 idle 即成功」的 PASS）；
/// 绑定 turn 以终态在场（snapshot rev 7）才恢复接力。
#[tokio::test]
async fn aborted_waiter_convergence_requires_bound_turn_terminal_evidence() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    f.state
        .codex_control
        .set_reconcile_period_ms(std::time::Duration::from_millis(200).as_millis() as u64);
    f.state.codex_control.start(f.state.clone());

    // ① send#1：ensure → start-turn 发出 → 中止 HTTP 等待者
    let body1 = send_body("被弃消息", Some("act-cvrg-1"), false);
    let task1 = tokio::spawn(async move {
        let _ = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some(COOKIE),
                Some(body1),
            ))
            .await;
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let frame = peer.recv().await;
    assert_eq!(frame["method"], "thread-follower-start-turn");
    confirm_client_message_identity(&mut peer, &frame, "turn-cvrg-1", "被弃消息", 8, "owner-1")
        .await;
    task1.abort();

    // ② Desktop 正常应答：独立派发任务原子落账 accepted + Turn 屏障（含 turnId）
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "turn": { "id": "turn-cvrg-1", "status": "inProgress" } }
    }))
    .await;
    let view = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let v = f
                .hub_store()
                .get_for_device("act-cvrg-1", "dev-1")
                .unwrap()
                .unwrap();
            if v.state != ActionState::Reserved {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("dispatch task settles");
    assert_eq!(view.state, ActionState::Accepted);
    assert_eq!(
        view.receipt.as_ref().unwrap().turn_id.as_deref(),
        Some("turn-cvrg-1")
    );
    assert!(
        f.hub_store().has_compat_fence(&f.thread_key()).unwrap(),
        "accepted 屏障已随落账原子持久（C1.3）"
    );

    // ③ 泛 idle snapshot（rev 8 > ensure 基线 7）：只有**另一 turn t6** 完成、绑定
    //    turn-cvrg-1 不在场 → 多轮对账（≥3 tick）也不得接力第二条（C1.1：目标身份
    //    与终态证据不能省略）
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": THREAD, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": 9,
                "conversationState": {
                    "turns": [], "requests": [],
                    "threadRuntimeStatus": { "type": "idle" },
                    "turnHistory": codex_fixture::canonical_turn_history(serde_json::json!({
                            "t6": { "id": "t6", "type": "turn", "status": "completed", "items": ["u6"] },
                            "u6": { "id": "u6", "type": "user_message", "text": "被弃消息 [mobile iPhone]", "ts": 6 }
                        }), serde_json::json!([{ "entries": ["t6"] }]))
                }
            }
        }
    }))
    .await;
    // 负窗口覆盖 ≥5 个对账 tick（200ms 周期）：期间零 start-turn
    let neg = tokio::time::timeout(Duration::from_millis(1200), peer.recv()).await;
    assert!(
        neg.is_err(),
        "泛 idle + 另一 turn 完成：绑定 turn 无终态证据不得接力（旧预期在此错误 PASS）"
    );

    // ④ 绑定 turn 终态证据到位（turn-cvrg-1 completed，rev 9 > 8，runtime idle）
    //    → 收敛补绑 + 释放 → flush 接力第二条
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": THREAD, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": 10,
                "conversationState": {
                    "turns": [], "requests": [],
                    "threadRuntimeStatus": { "type": "idle" },
                    "turnHistory": codex_fixture::canonical_turn_history(serde_json::json!({
                            "turn-cvrg-1": { "id": "turn-cvrg-1", "type": "turn", "status": "completed", "items": ["u6"] },
                            "t6": { "id": "t6", "type": "turn", "status": "completed", "items": ["u6"] },
                            "u6": { "id": "u6", "type": "user_message", "text": "被弃消息 [mobile iPhone]", "ts": 6 }
                        }), serde_json::json!([{ "entries": ["turn-cvrg-1", "t6"] }]))
                }
            }
        }
    }))
    .await;
    let _ = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(send_body("收敛后第二条", Some("act-cvrg-2"), false)),
        ))
        .await
        .unwrap();
    let frame2 = tokio::time::timeout(Duration::from_secs(5), peer.recv())
        .await
        .expect("绑定 turn 终态证据到位后队列必须流动（第二个 start-turn 帧）");
    assert_eq!(frame2["method"], "thread-follower-start-turn");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame2["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "turn": { "id": "turn-cvrg-2", "status": "inProgress" } }
    }))
    .await;
    f.state.codex_control.stop();
}

// ============================================================
// 18. 2026-10-17 复审 C2：自动恢复两条生产断路——同 socket owner 失效/reset 的
// 重跟随（不依赖 GET），以及持久屏障 Thread 完整重启后自动重建投影源。
// ============================================================

/// 发送「本次消息身份已落地」状态帧（2026-10-20 复审 P1）：真实 Desktop 会在
/// start-turn 回执前广播含 `userMessage.clientId` 的 Turn；既有 accepted 用例补此帧，
/// 避免把「新 Turn ID」当成唯一边界证据。
async fn confirm_client_message_identity(
    peer: &mut ScriptedPeer,
    frame: &serde_json::Value,
    turn_id: &str,
    text: &str,
    revision: u64,
    owner: &str,
) {
    confirm_client_message_identity_for(peer, THREAD, frame, turn_id, text, revision, owner).await;
}

/// 跨 thread 变体：快照发到指定 thread（多 thread 用例用）。
async fn confirm_client_message_identity_for(
    peer: &mut ScriptedPeer,
    thread: &str,
    frame: &serde_json::Value,
    turn_id: &str,
    text: &str,
    revision: u64,
    owner: &str,
) {
    let cmid = frame["params"]["turnStart"]["request"]["clientUserMessageId"]
        .as_str()
        .expect("start-turn 请求带 clientUserMessageId");
    let uid = format!("u-{turn_id}");
    let mut entities = serde_json::Map::new();
    entities.insert(
        turn_id.to_string(),
        serde_json::json!({
            "id": turn_id, "type": "turn", "status": "inProgress",
            "items": [uid.clone()]
        }),
    );
    entities.insert(
        uid.clone(),
        serde_json::json!({
            "id": uid, "type": "user_message", "clientId": cmid,
            "text": text, "ts": 2
        }),
    );
    peer.send(&snapshot_frame(
        thread,
        owner,
        revision,
        "active",
        serde_json::Value::Object(entities),
        serde_json::json!([turn_id]),
    ))
    .await;
}

/// 构造一帧 state-changed snapshot（C2 测试用：实体表自定）。
fn snapshot_frame(
    thread: &str,
    owner: &str,
    revision: u64,
    runtime: &str,
    entities: serde_json::Value,
    turn_ids: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": owner,
        "params": {
            "conversationId": thread, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": revision,
                "conversationState": {
                    "turns": [], "requests": [],
                    "threadRuntimeStatus": { "type": runtime },
                    "turnHistory": codex_fixture::canonical_turn_history(entities, turn_ids)
                }
            }
        }
    })
}

/// 2026-10-19 复审 P1：多义终态证据（同 turnId 多副本：缺 status 在前 / 状态
/// 冲突）**顺序无关地**不清持久屏障、不派发下一条；多副本同 completed 才放行。
/// 证据门本体（router）未被放宽——本测试走完整 hub 生产装配。
#[tokio::test]
async fn multi_copy_ambiguous_turn_evidence_never_clears_barrier() {
    fn two_copy_snapshot(
        rev: u64,
        a: (&str, Option<&str>),
        b: (&str, Option<&str>),
    ) -> serde_json::Value {
        let ent = |status: Option<&str>| {
            let mut e = serde_json::json!({
                "turnId": "turn-mc-1",
                "items": []
            });
            if let Some(st) = status {
                e["status"] = serde_json::json!(st);
            }
            e
        };
        serde_json::json!({
            "type": "snapshot", "revision": rev,
            "conversationState": {
                "turns": [], "requests": [],
                "threadRuntimeStatus": { "type": "idle" },
                "turnHistory": { "kind": "canonical", "history": {
                    "generation": 0, "isComplete": true,
                    "entitiesByKey": {
                        a.0: ent(a.1),
                        b.0: ent(b.1),
                    },
                    "islands": [ { "entries": [
                        { "key": a.0, "value": a.0 },
                        { "key": b.0, "value": b.0 }
                    ] } ]
                } }
            }
        })
    }
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    f.state
        .codex_control
        .set_reconcile_period_ms(std::time::Duration::from_millis(200).as_millis() as u64);
    f.state.codex_control.start(f.state.clone());

    // ① send#1：ensure → start-turn → 应答 accepted（绑定 turn-mc-1，屏障在册）
    let body1 = send_body("多义甲", Some("act-mc-1"), false);
    let task1 = tokio::spawn(async move {
        let _ = app
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some(COOKIE),
                Some(body1),
            ))
            .await;
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let frame = peer.recv().await;
    assert_eq!(frame["method"], "thread-follower-start-turn");
    confirm_client_message_identity(&mut peer, &frame, "turn-mc-1", "多义甲", 8, "owner-1").await;
    task1.abort();
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "turn": { "id": "turn-mc-1", "status": "inProgress" } }
    }))
    .await;
    let view = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let v = f
                .hub_store()
                .get_for_device("act-mc-1", "dev-1")
                .unwrap()
                .unwrap();
            if v.state != ActionState::Reserved {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("dispatch settles");
    assert_eq!(view.state, ActionState::Accepted);
    assert!(f.hub_store().has_compat_fence(&f.thread_key()).unwrap());

    // ② 第二条入队（被屏障挡住）
    let _ = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(send_body("多义乙", Some("act-mc-2"), false)),
        ))
        .await
        .unwrap();

    // ③ 缺 status 副本在**前**、completed 在后 → 顺序无关多义：不清屏障、不派发
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": { "conversationId": THREAD, "hostId": "local",
                    "change": two_copy_snapshot(9, ("turn:a", None),
                                                ("turn:b", Some("completed"))) }
    }))
    .await;
    let neg = tokio::time::timeout(Duration::from_millis(1200), peer.recv()).await;
    assert!(neg.is_err(), "缺 status 副本在前：多义不得放行/接力");

    // ④ 状态冲突（completed 在前、inProgress 在后）→ 仍不放行
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": { "conversationId": THREAD, "hostId": "local",
                    "change": two_copy_snapshot(10, ("turn:b", Some("completed")),
                                                ("turn:c", Some("inProgress"))) }
    }))
    .await;
    let neg2 = tokio::time::timeout(Duration::from_millis(1200), peer.recv()).await;
    assert!(neg2.is_err(), "状态冲突不得放行/接力");

    // ⑤ 多副本同 completed → 证据成立：清屏障并只接力下一条
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": { "conversationId": THREAD, "hostId": "local",
                    "change": two_copy_snapshot(11, ("turn:b", Some("completed")),
                                                 ("turn:c", Some("completed"))) }
    }))
    .await;
    let frame2 = tokio::time::timeout(Duration::from_secs(5), peer.recv())
        .await
        .expect("多副本同 completed 后队列必须接力");
    assert_eq!(frame2["method"], "thread-follower-start-turn");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame2["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "turn": { "id": "turn-mc-2", "status": "inProgress" } }
    }))
    .await;
    f.state.codex_control.stop();
}

/// 2026-10-19 复审 P2-3：结构错误（非法 entries）经 HTTP 投影必须 Unsupported，
/// 不得 Ready + 0 条冒充真空会话。
#[tokio::test]
async fn invalid_structure_projects_unsupported_over_http() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let key = f.thread_key();
    let adapter = f.state.codex_control.adapter.clone();
    let task = {
        let a = adapter.clone();
        let k = key.clone();
        tokio::spawn(async move { a.ensure_thread(&k).await })
    };
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let _ = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("ensure in time");
    // 坏结构快照（rev 100 > ensure 基线 7）：非法 entries
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": THREAD, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": 100,
                "conversationState": {
                    "turns": [], "requests": [],
                    "threadRuntimeStatus": { "type": "idle" },
                    "turnHistory": { "kind": "canonical", "history": {
                        "generation": 0, "isComplete": true,
                        "entitiesByKey": {},
                        "islands": [ { "entries": ["not-an-object"] } ]
                    } }
                }
            }
        }
    }))
    .await;
    // 等镜像吃到坏帧（projection 只读轮询）
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if adapter.projection(&key).await.revision == Some(100) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "等待坏结构快照应用超时"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let r = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["contentState"], "unsupported", "坏结构不得 Ready：{v}");
    assert_eq!(v["messages"].as_array().map(|a| a.len()), Some(0));
    f.state.codex_control.stop();
}

/// C2.1：owner 失效（socket 仍活着）→ 必须**自动**重新 discovery/follow（无 GET、
/// 无待发工作），新 owner 的 snapshot 落进镜像、视图自动复活——不再永久 stale。
#[tokio::test]
async fn owner_lost_on_live_socket_auto_refollows() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let key = f.thread_key();
    // 先建立连接与跟随（本测试无 GET/POST 触发面）：并发 ensure + 协议驱动
    {
        let adapter = f.state.codex_control.adapter.clone();
        let k2 = key.clone();
        let ensure_task = tokio::spawn(async move { adapter.ensure_thread(&k2).await });
        drive_ensure(&mut peer, THREAD, vec![], "idle").await;
        let initial = tokio::time::timeout(Duration::from_secs(5), ensure_task)
            .await
            .expect("initial follow in time")
            .expect("initial follow ok");
        let _ = initial;
    }

    // owner-1 失效广播（连接不断）
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "client-status-changed", "version": 1,
        "sourceClientId": "owner-1",
        "params": { "clientId": "owner-1", "status": "disconnected" }
    }))
    .await;

    // C2：自动重恢复——discovery 帧**不经任何 GET/ensure** 到来
    let disc = tokio::time::timeout(Duration::from_secs(5), peer.recv())
        .await
        .expect("owner 失效后必须自动重发现（同 socket 恢复）");
    assert_eq!(disc["method"], "thread-owner-discovery");
    peer.send(&discovery_response(
        disc["requestId"].as_str().unwrap(),
        "owner-2",
    ))
    .await;
    let follow = tokio::time::timeout(Duration::from_secs(5), peer.recv())
        .await
        .expect("自动重声明 following");
    assert_eq!(follow["method"], "thread-stream-following-changed");
    assert_eq!(follow["params"]["following"], serde_json::json!(true));

    // 新 owner 的 snapshot：sourceClientId 与新 lease 一致 → 落进镜像
    let entities = serde_json::json!({
        "t8": { "id": "t8", "type": "turn", "status": "completed", "items": ["u8"] },
        "u8": { "id": "u8", "type": "user_message", "text": "新 owner 世界", "ts": 8 }
    });
    peer.send(&snapshot_frame(
        THREAD,
        "owner-2",
        8,
        "idle",
        entities,
        serde_json::json!(["t8"]),
    ))
    .await;

    // 视图自动复活（新鲜、rev 8、新 owner 世界）
    let p = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let p = f.state.codex_control.adapter.projection(&key).await;
            if !p.stale && p.revision == Some(8) {
                return p;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("视图必须在无 GET 的情况下自动恢复新鲜（C2：不再永久 stale）");
    assert_eq!(p.runtime_status, "idle");
}

/// C2.1：`ipc-connection-reset`（Desktop 要求重置状态，连接不断）→ 全体 stale 后
/// 自动重跟随，重新拿到 snapshot——视图不停在旧世界。
#[tokio::test]
async fn ipc_connection_reset_auto_refollows() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let key = f.thread_key();
    // 先建立连接与跟随（本测试无 GET/POST 触发面）：并发 ensure + 协议驱动
    {
        let adapter = f.state.codex_control.adapter.clone();
        let k2 = key.clone();
        let ensure_task = tokio::spawn(async move { adapter.ensure_thread(&k2).await });
        drive_ensure(&mut peer, THREAD, vec![], "idle").await;
        let initial = tokio::time::timeout(Duration::from_secs(5), ensure_task)
            .await
            .expect("initial follow in time")
            .expect("initial follow ok");
        let _ = initial;
    }

    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "ipc-connection-reset", "version": 1,
        "sourceClientId": "owner-1"
    }))
    .await;

    let disc = tokio::time::timeout(Duration::from_secs(5), peer.recv())
        .await
        .expect("reset 后必须自动重发现（同 socket 恢复）");
    assert_eq!(disc["method"], "thread-owner-discovery");
    peer.send(&discovery_response(
        disc["requestId"].as_str().unwrap(),
        "owner-1",
    ))
    .await;
    let follow = tokio::time::timeout(Duration::from_secs(5), peer.recv())
        .await
        .expect("自动重声明 following");
    assert_eq!(follow["method"], "thread-stream-following-changed");

    let entities = serde_json::json!({
        "t9": { "id": "t9", "type": "turn", "status": "completed", "items": ["u9"] },
        "u9": { "id": "u9", "type": "user_message", "text": "重置后世界", "ts": 9 }
    });
    peer.send(&snapshot_frame(
        THREAD,
        "owner-1",
        9,
        "idle",
        entities,
        serde_json::json!(["t9"]),
    ))
    .await;

    let p = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let p = f.state.codex_control.adapter.projection(&key).await;
            if !p.stale && p.revision == Some(9) {
                return p;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("reset 后视图必须自动恢复新鲜");
    assert_eq!(p.runtime_status, "idle");
}

/// C2.2：持久屏障 Thread 完整重启——丢弃全部内存重建 hub，**不打开详情、不手工
/// flush**：settle 主动 ensure 重建投影源，绑定 Turn 终态证据到位后自动接力队首。
#[tokio::test]
async fn hub_restart_with_turn_barrier_recovers_without_details() {
    let conn = shared_conn();
    seed_device(&conn, "dev-1", "iPhone");
    // —— 阶段一：accepted#1（屏障在册）→ active 排队 send#2 → 丢弃全部内存 ——
    {
        let (adapter, p1) = make_adapter_peer();
        let mut f1 = fixture_on(
            conn.clone(),
            vec![codex_app_session(THREAD)],
            move |store| Arc::new(CodexControlHub::with_adapter(store, adapter, true)),
            Some(p1),
        );
        let mut peer1 = f1.peer.take().unwrap();
        let app1 = f1.app.clone();
        let task1 = tokio::spawn(async move {
            app1.oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some(COOKIE),
                Some(send_body("屏障前已发", Some("act-bar-1"), false)),
            ))
            .await
            .unwrap()
        });
        drive_ensure(&mut peer1, THREAD, vec![], "idle").await;
        let frame = peer1.recv().await;
        assert_eq!(frame["method"], "thread-follower-start-turn");
        confirm_client_message_identity(&mut peer1, &frame, "turn-b1", "屏障前已发", 8, "owner-1")
            .await;
        peer1
            .send(&serde_json::json!({
                "type": "response", "requestId": frame["requestId"],
                "resultType": "success", "method": "thread-follower-start-turn",
                "handledByClientId": "owner-1",
                "result": { "turn": { "id": "turn-b1", "status": "inProgress" } }
            }))
            .await;
        tokio::time::timeout(Duration::from_secs(10), task1)
            .await
            .expect("send#1 in time")
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(Some(v)) = f1.hub_store().get_for_device("act-bar-1", "dev-1") {
                    if v.state == ActionState::Accepted {
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("act-bar-1 accepted");
        // 转入 active（turn-b1 inProgress）→ send#2 忙态/顺序限制入队
        let entities = serde_json::json!({
            "turn-b1": { "id": "turn-b1", "type": "turn", "status": "inProgress", "items": ["u7"] },
            "u7": { "id": "u7", "type": "user_message", "text": "屏障前已发 [mobile iPhone]", "ts": 7 }
        });
        peer1
            .send(&snapshot_frame(
                THREAD,
                "owner-1",
                10,
                "active",
                entities,
                serde_json::json!(["turn-b1"]),
            ))
            .await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        let r2 = f1
            .app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some(COOKIE),
                Some(send_body("屏障后排队", Some("act-bar-2"), false)),
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r2).await).unwrap();
        assert_eq!(v["status"], "codex_queued", "屏障+忙态：send#2 入队");
        // f1 drop：hub/adapter/调度器全部丢弃，仅持久账本留存
    }
    // —— 阶段二：重建 hub，start 后不打开详情、不手工 flush ——
    let (adapter2, p2) = make_adapter_peer();
    let mut f2 = fixture_on(
        conn.clone(),
        vec![codex_app_session(THREAD)],
        move |store| Arc::new(CodexControlHub::with_adapter(store, adapter2, true)),
        Some(p2),
    );
    // 对账周期放宽：冷启动只有首 tick 的 settle 会 ensure（避免并发 ensure 打乱协议序）
    f2.state
        .codex_control
        .set_reconcile_period_ms(std::time::Duration::from_secs(10).as_millis() as u64);
    f2.state.codex_control.start(f2.state.clone());
    {
        let mut peer = f2.peer.take().unwrap();
        // C2：settle 主动 ensure（无 GET）——handshake → discovery → following
        handshake(&mut peer, "owner-1").await;
        let disc = peer.recv().await;
        assert_eq!(disc["method"], "thread-owner-discovery");
        peer.send(&discovery_response(
            disc["requestId"].as_str().unwrap(),
            "owner-1",
        ))
        .await;
        let follow = peer.recv().await;
        assert_eq!(follow["method"], "thread-stream-following-changed");
        // snapshot rev 9：turn-b1 completed + idle → 屏障释放 → flush 自动接力 send#2
        let entities = serde_json::json!({
            "turn-b1": { "id": "turn-b1", "type": "turn", "status": "completed", "items": ["u7"] },
            "u7": { "id": "u7", "type": "user_message", "text": "屏障前已发 [mobile iPhone]", "ts": 7 }
        });
        peer.send(&snapshot_frame(
            THREAD,
            "owner-1",
            9,
            "idle",
            entities,
            serde_json::json!(["turn-b1"]),
        ))
        .await;
        let frame2 = tokio::time::timeout(Duration::from_secs(10), peer.recv())
            .await
            .expect("重启后屏障 Thread 自动恢复接力（第二个 start-turn 帧）");
        assert_eq!(frame2["method"], "thread-follower-start-turn");
        confirm_client_message_identity(&mut peer, &frame2, "turn-b2", "屏障后排队", 11, "owner-1")
            .await;
        assert_eq!(
            frame2["params"]["turnStart"]["request"]["input"][0]["text"],
            "屏障后排队"
        );
        peer.send(&serde_json::json!({
            "type": "response", "requestId": frame2["requestId"],
            "resultType": "success", "method": "thread-follower-start-turn",
            "handledByClientId": "owner-1",
            "result": { "turn": { "id": "turn-b2", "status": "inProgress" } }
        }))
        .await;
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(Some(v)) = f2.hub_store().get_for_device("act-bar-2", "dev-1") {
                if v.state == ActionState::Accepted {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("act-bar-2 accepted（重启恢复闭环）");
    f2.state.codex_control.stop();
}

/// 2026-10-17 复审 C4：hub.stop = 完整停机——adapter 进入 stopping、连接关闭
/// （peer 见 EOF）、ensure 不再新建连接（推进退避时间也不再有新 connect/follow）；
/// 重新 start 解除停机（io 耗尽时诚实快败，不悬挂）。
#[tokio::test]
async fn hub_stop_closes_connection_and_blocks_reconnect_until_start() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let key = f.thread_key();
    // 建连 + 跟随
    {
        let adapter = f.state.codex_control.adapter.clone();
        let k2 = key.clone();
        let ensure_task = tokio::spawn(async move { adapter.ensure_thread(&k2).await });
        drive_ensure(&mut peer, THREAD, vec![], "idle").await;
        tokio::time::timeout(Duration::from_secs(5), ensure_task)
            .await
            .expect("in time")
            .expect("join")
            .expect("fresh");
    }
    // stop：停机 + 连接关闭
    f.state.codex_control.stop();
    assert!(
        f.state.codex_control.adapter.is_stopping(),
        "stop 后 adapter 必须进入停机态"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut buf = vec![0u8; 64];
    let n = tokio::time::timeout(Duration::from_secs(5), peer.io.read(&mut buf))
        .await
        .expect("eof in time")
        .expect("read ok");
    assert_eq!(n, 0, "stop 后对端必须看到 EOF（连接已关闭）");
    // 停机态 ensure 拒绝：无新 connect/follow
    let r = f.state.codex_control.adapter.ensure_thread(&key).await;
    assert!(r.is_err(), "停机后 ensure 不再新建连接");
    // 重新 start：解除停机；connector io 已耗尽 → 诚实快败（不悬挂）
    f.state.codex_control.start(f.state.clone());
    assert!(
        !f.state.codex_control.adapter.is_stopping(),
        "start 解除停机"
    );
    let r2 = tokio::time::timeout(
        Duration::from_secs(5),
        f.state.codex_control.adapter.ensure_thread(&key),
    )
    .await
    .expect("no hang after restart");
    assert!(r2.is_err(), "io 耗尽诚实快败");
}

// ============================================================
// 19. 第三轮复审 B1：revision 基线门不得阻止合法 Turn 完成（生产链）——
// 完成快照先于 ACK 在场、换代后低 revision 新世界，都必须自动接力。
// ============================================================

/// B1 场景 1（生产链）：completed snapshot 先于 ACK 落镜像（之后零新增量）——
/// ACK 后后继消息必须自动接力，不得因「rev == 基线」永久堵队。
#[tokio::test]
async fn completed_snapshot_before_ack_still_relays() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    f.state
        .codex_control
        .set_reconcile_period_ms(std::time::Duration::from_millis(200).as_millis() as u64);
    f.state.codex_control.start(f.state.clone());
    let body1 = send_body("基线前已发", Some("act-b1-1"), false);
    let task1 = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body1),
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let frame = peer.recv().await;
    assert_eq!(frame["method"], "thread-follower-start-turn");
    let cmid = frame["params"]["turnStart"]["request"]["clientUserMessageId"]
        .as_str()
        .unwrap()
        .to_string();
    // 完成快照**先于 ACK** 落镜像（rev 8，turn-chain-1 completed，idle）
    let entities = serde_json::json!({
        "turn-chain-1": { "id": "turn-chain-1", "type": "turn", "status": "completed", "items": ["u7"] },
        "u7": { "id": "u7", "type": "user_message", "clientId": cmid, "text": "基线前已发 [mobile iPhone]", "ts": 7 }
    });
    peer.send(&snapshot_frame(
        THREAD,
        "owner-1",
        8,
        "idle",
        entities,
        serde_json::json!(["turn-chain-1"]),
    ))
    .await;
    // ACK（send_text 回执后采集的基线 == 已在场的 rev 8）
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "turn": { "id": "turn-chain-1", "status": "inProgress" } }
    }))
    .await;
    tokio::time::timeout(Duration::from_secs(15), task1)
        .await
        .expect("send#1 in time")
        .unwrap();
    // 后继入队；**不再发任何新 snapshot**——释放只能靠既有镜像中的完成证据
    let r2 = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(send_body("基线后排队", Some("act-b1-2"), false)),
        ))
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body_string(r2).await).unwrap();
    assert!(
        v["status"] == "codex_queued" || v["status"] == "desktop_ipc",
        "后继入队或直接派发：{v}"
    );
    let frame2 = tokio::time::timeout(Duration::from_secs(8), peer.recv())
        .await
        .expect("完成快照先于 ACK：后继必须自动接力（零新增量）");
    assert_eq!(frame2["method"], "thread-follower-start-turn");
    confirm_client_message_identity(&mut peer, &frame2, "turn-b1-2", "基线后排队", 9, "owner-1")
        .await;
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame2["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "turn": { "id": "turn-b1-2", "status": "inProgress" } }
    }))
    .await;
    f.state.codex_control.stop();
}

/// B1 场景 2（生产链）：同进程 owner 换代（Desktop 重启语义）——旧基线 rev 7，
/// 新 owner 从 revision 1 重新发布 snapshot 且目标 Turn 已 completed → 顺序限制
/// 必须释放并自动接力（走 C2 自动恢复，无 GET）。
#[tokio::test]
async fn lower_revision_from_new_owner_still_releases() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    f.state
        .codex_control
        .set_reconcile_period_ms(std::time::Duration::from_millis(200).as_millis() as u64);
    f.state.codex_control.start(f.state.clone());
    let body1 = send_body("换代前已发", Some("act-gen-1"), false);
    let task1 = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body1),
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let frame = peer.recv().await;
    assert_eq!(frame["method"], "thread-follower-start-turn");
    confirm_client_message_identity(&mut peer, &frame, "turn-gen-1", "换代前已发", 8, "owner-1")
        .await;
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "turn": { "id": "turn-gen-1", "status": "inProgress" } }
    }))
    .await;
    tokio::time::timeout(Duration::from_secs(15), task1)
        .await
        .expect("send#1 in time")
        .unwrap();
    // owner-1 失效（同 socket 恢复）→ 新 owner-2 从 revision 1 重新发布：
    // 镜像换代重置后 rev 1 < 旧基线 7，且目标 Turn 已 completed
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "client-status-changed", "version": 1,
        "sourceClientId": "owner-1",
        "params": { "clientId": "owner-1", "status": "disconnected" }
    }))
    .await;
    let disc = peer.recv().await;
    assert_eq!(disc["method"], "thread-owner-discovery");
    peer.send(&discovery_response(
        disc["requestId"].as_str().unwrap(),
        "owner-2",
    ))
    .await;
    let follow = peer.recv().await;
    assert_eq!(follow["method"], "thread-stream-following-changed");
    let entities = serde_json::json!({
        "turn-gen-1": { "id": "turn-gen-1", "type": "turn", "status": "completed", "items": ["u6"] },
        "u6": { "id": "u6", "type": "user_message", "text": "换代前已发 [mobile iPhone]", "ts": 6 }
    });
    peer.send(&snapshot_frame(
        THREAD,
        "owner-2",
        1,
        "idle",
        entities,
        serde_json::json!(["turn-gen-1"]),
    ))
    .await;
    // 后继入队（换代窗口内顺序限制仍在 → codex_queued 或直接派发都可）
    let _ = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(send_body("换代后排队", Some("act-gen-2"), false)),
        ))
        .await
        .unwrap();
    let frame2 = tokio::time::timeout(Duration::from_secs(8), peer.recv())
        .await
        .expect("换代后低 revision 完成世界：后继必须自动接力");
    assert_eq!(frame2["method"], "thread-follower-start-turn");
    confirm_client_message_identity(&mut peer, &frame2, "turn-gen-2", "换代后排队", 2, "owner-2")
        .await;
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame2["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-2",
        "result": { "turn": { "id": "turn-gen-2", "status": "inProgress" } }
    }))
    .await;
    f.state.codex_control.stop();
}

/// 第三轮复审 B2：同 socket 恢复单飞**不丢后到失效事件**——tA/tB 分属 owner
/// A/B，A 失效恢复在途（卡在 tA snapshot 等待）时 B 失效，B 的 keys 必须并入
/// 在飞任务的待办（任务退出边界的第三事件同样被消费）。全程同 socket、无 GET、
/// 无手工 ensure/flush，两 Thread 均自动发现新 owner 并发布 fresh snapshot。
#[tokio::test]
async fn interleaved_owner_failures_both_recover_without_get() {
    let id_a = "00000000-0000-4000-8000-0000000000a1";
    let id_b = "00000000-0000-4000-8000-0000000000b2";
    let mut f = fixture_ipc(vec![codex_app_session(id_a), codex_app_session(id_b)]);
    let mut peer = f.peer.take().unwrap();
    let key_a = ThreadKey {
        host_id: "local".into(),
        thread_id: id_a.into(),
    };
    let key_b = ThreadKey {
        host_id: "local".into(),
        thread_id: id_b.into(),
    };
    let adapter = f.state.codex_control.adapter.clone();
    // ① 两 Thread 分别 follow 到不同 owner（同一条连接）
    for (key, id, owner) in [(&key_a, id_a, "owner-a"), (&key_b, id_b, "owner-b")] {
        let a = adapter.clone();
        let k = key.clone();
        let task = tokio::spawn(async move { a.ensure_thread(&k).await });
        if key == &key_a {
            handshake(&mut peer, "owner-a").await;
        }
        let disc = peer.recv().await;
        assert_eq!(disc["method"], "thread-owner-discovery");
        peer.send(&discovery_response(
            disc["requestId"].as_str().unwrap(),
            owner,
        ))
        .await;
        let follow = peer.recv().await;
        assert_eq!(follow["method"], "thread-stream-following-changed");
        let entities = serde_json::json!({
            "t1": { "id": "t1", "type": "turn", "status": "completed", "items": ["u1"] },
            "u1": { "id": "u1", "type": "user_message", "text": "seed", "ts": 1 }
        });
        peer.send(&snapshot_frame(
            id,
            owner,
            1,
            "idle",
            entities,
            serde_json::json!(["t1"]),
        ))
        .await;
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("in time")
            .expect("join")
            .expect("fresh");
    }

    // ② owner-a 失效：恢复任务启动，卡在 tA 的 snapshot 等待（受控在途）
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "client-status-changed", "version": 1,
        "sourceClientId": "owner-a",
        "params": { "clientId": "owner-a", "status": "disconnected" }
    }))
    .await;
    let disc_a = peer.recv().await;
    assert_eq!(disc_a["method"], "thread-owner-discovery");
    peer.send(&discovery_response(
        disc_a["requestId"].as_str().unwrap(),
        "owner-a",
    ))
    .await;
    let follow_a = peer.recv().await;
    assert_eq!(follow_a["method"], "thread-stream-following-changed");

    // ③ owner-b 失效（恢复在途）：B 的 keys 必须并入待办而非被单飞丢弃
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "client-status-changed", "version": 1,
        "sourceClientId": "owner-b",
        "params": { "clientId": "owner-b", "status": "disconnected" }
    }))
    .await;

    // ④ tA snapshot 到达：tA 恢复 → 任务继续消费待办 {tB}
    let entities = serde_json::json!({
        "t2": { "id": "t2", "type": "turn", "status": "completed", "items": ["u2"] },
        "u2": { "id": "u2", "type": "user_message", "text": "A 恢复世界", "ts": 2 }
    });
    peer.send(&snapshot_frame(
        id_a,
        "owner-a",
        2,
        "idle",
        entities,
        serde_json::json!(["t2"]),
    ))
    .await;
    let disc_b = tokio::time::timeout(Duration::from_secs(8), peer.recv())
        .await
        .expect("B 的失效事件不得被单飞丢弃（旧实现 tB 永不重发现）");
    assert_eq!(
        disc_b["method"], "thread-owner-discovery",
        "tB 必须自动重发现"
    );
    assert_eq!(
        disc_b["params"]["conversationId"],
        serde_json::json!(id_b),
        "消费的正是后到事件的 tB"
    );
    peer.send(&discovery_response(
        disc_b["requestId"].as_str().unwrap(),
        "owner-b",
    ))
    .await;
    let follow_b = peer.recv().await;
    assert_eq!(follow_b["method"], "thread-stream-following-changed");
    let entities = serde_json::json!({
        "t2": { "id": "t2", "type": "turn", "status": "completed", "items": ["u2"] },
        "u2": { "id": "u2", "type": "user_message", "text": "B 恢复世界", "ts": 2 }
    });
    peer.send(&snapshot_frame(
        id_b,
        "owner-b",
        2,
        "idle",
        entities,
        serde_json::json!(["t2"]),
    ))
    .await;

    // ⑤ 两 Thread 均自动恢复新鲜（无 GET/无手工 ensure）
    for (key, _id, owner, text) in [
        (&key_a, id_a, "owner-a", "A 恢复世界"),
        (&key_b, id_b, "owner-b", "B 恢复世界"),
    ] {
        let p = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let p = adapter.projection(key).await;
                if !p.stale && p.revision == Some(2) {
                    return p;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("视图必须自动恢复新鲜");
        assert!(
            p.messages.iter().any(|m| m.text == text),
            "{owner} 世界生效"
        );
    }

    // ⑥ 退出边界：任务刚收尾时再来一个失效事件（tA）——事件必须被消费
    //（mirror 已被事件 invalidate → 恢复任务重发现可观察）
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "client-status-changed", "version": 1,
        "sourceClientId": "owner-a",
        "params": { "clientId": "owner-a", "status": "disconnected" }
    }))
    .await;
    let disc_a2 = tokio::time::timeout(Duration::from_secs(8), peer.recv())
        .await
        .expect("任务退出边界的事件不得丢失（第三事件被新任务消费）");
    assert_eq!(
        disc_a2["params"]["conversationId"],
        serde_json::json!(id_a),
        "重发现的是再次失效的 tA"
    );
    peer.send(&discovery_response(
        disc_a2["requestId"].as_str().unwrap(),
        "owner-a",
    ))
    .await;
    let follow_a2 = peer.recv().await;
    assert_eq!(follow_a2["method"], "thread-stream-following-changed");
    let entities = serde_json::json!({
        "t3": { "id": "t3", "type": "turn", "status": "completed", "items": ["u3"] },
        "u3": { "id": "u3", "type": "user_message", "text": "边界恢复世界", "ts": 3 }
    });
    peer.send(&snapshot_frame(
        id_a,
        "owner-a",
        3,
        "idle",
        entities,
        serde_json::json!(["t3"]),
    ))
    .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let p = adapter.projection(&key_a).await;
            if !p.stale && p.revision == Some(3) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("边界事件恢复 fresh");
}

/// 第四轮复审 B2 回归 2：预算耗尽**只放弃本批失败工作**，不得 clear 掉本轮
/// 「等待 A 响应期间刚加入」的后到 keys——tB 未尝试就被删除 = B 永不恢复。
/// 确定时序：第 3 轮 disc(A) 帧到达后，先令 tB 失效（入待办），再回 A 的
/// discovery 错误——旧实现在此 clear 整个共享待办（A+tB 一起丢），tB 的
/// discovery 帧永不出现；修复后 tB 必须被实际尝试并恢复。
#[tokio::test]
async fn budget_exhaustion_does_not_drop_later_keys() {
    let id_a = "00000000-0000-4000-8000-0000000000a1";
    let id_b = "00000000-0000-4000-8000-0000000000b2";
    let mut f = fixture_ipc(vec![codex_app_session(id_a), codex_app_session(id_b)]);
    let mut peer = f.peer.take().unwrap();
    let key_b = ThreadKey {
        host_id: "local".into(),
        thread_id: id_b.into(),
    };
    let adapter = f.state.codex_control.adapter.clone();
    // ① 两 Thread 分别 follow
    for (id, owner) in [(id_a, "owner-a"), (id_b, "owner-b")] {
        let key = ThreadKey {
            host_id: "local".into(),
            thread_id: id.into(),
        };
        let a = adapter.clone();
        let k = key.clone();
        let task = tokio::spawn(async move { a.ensure_thread(&k).await });
        if id == id_a {
            handshake(&mut peer, "owner-a").await;
        }
        let disc = peer.recv().await;
        assert_eq!(disc["method"], "thread-owner-discovery");
        peer.send(&discovery_response(
            disc["requestId"].as_str().unwrap(),
            owner,
        ))
        .await;
        let follow = peer.recv().await;
        assert_eq!(follow["method"], "thread-stream-following-changed");
        let entities = serde_json::json!({
            "t1": { "id": "t1", "type": "turn", "status": "completed", "items": ["u1"] },
            "u1": { "id": "u1", "type": "user_message", "text": "seed", "ts": 1 }
        });
        peer.send(&snapshot_frame(
            id,
            owner,
            1,
            "idle",
            entities,
            serde_json::json!(["t1"]),
        ))
        .await;
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("in time")
            .expect("join")
            .expect("fresh");
    }

    // ② owner-a 失效：恢复任务 take{A}，discovery 回**错误**（follow 失败 → 重试）
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "client-status-changed", "version": 1,
        "sourceClientId": "owner-a",
        "params": { "clientId": "owner-a", "status": "disconnected" }
    }))
    .await;
    let mut a_failures = 0usize;
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(5), peer.recv())
            .await
            .expect("A 的重试 discovery 帧必须到来");
        assert_eq!(frame["method"], "thread-owner-discovery");
        assert_eq!(frame["params"]["conversationId"], serde_json::json!(id_a));
        a_failures += 1;
        if a_failures == 3 {
            // 第三轮「等待 A 响应期间」：tB 失效入待办（B 尚未被尝试）
            peer.send(&serde_json::json!({
                "type": "broadcast", "method": "client-status-changed", "version": 1,
                "sourceClientId": "owner-b",
                "params": { "clientId": "owner-b", "status": "disconnected" }
            }))
            .await;
        }
        peer.send(&serde_json::json!({
            "type": "response", "resultType": "error",
            "requestId": frame["requestId"],
            "method": "thread-owner-discovery",
            "handledByClientId": "owner-a",
            "error": { "message": "no-client-found" }
        }))
        .await;
        if a_failures >= 3 {
            break;
        }
    }
    // 任务对本批 A 的重试预算耗尽：**tB 不得被连带放弃**——它的 discovery 帧必须
    // 出现（旧实现 clear 掉共享待办，此帧永不出现）
    let disc_b = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let frame = peer.recv().await;
            if frame["method"] == "thread-owner-discovery"
                && frame["params"]["conversationId"] == serde_json::json!(id_b)
            {
                return frame;
            }
        }
    })
    .await
    .expect("预算耗尽不得丢弃后到的 tB（B2 回归 2）");
    peer.send(&discovery_response(
        disc_b["requestId"].as_str().unwrap(),
        "owner-b",
    ))
    .await;
    let follow_b = peer.recv().await;
    assert_eq!(follow_b["method"], "thread-stream-following-changed");
    let entities = serde_json::json!({
        "t2": { "id": "t2", "type": "turn", "status": "completed", "items": ["u2"] },
        "u2": { "id": "u2", "type": "user_message", "text": "B 恢复世界", "ts": 2 }
    });
    peer.send(&snapshot_frame(
        id_b,
        "owner-b",
        2,
        "idle",
        entities,
        serde_json::json!(["t2"]),
    ))
    .await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let p = adapter.projection(&key_b).await;
            if !p.stale && p.revision == Some(2) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("tB 必须被实际尝试并恢复 fresh");
}

/// 【S2 重写（原 `slow_compat_fallback_does_not_block_other_thread_scheduling`）】
/// 旧回归钉的是「T1 的分钟级兜底在飞不阻塞 T2 的调度推进」——兜底删除后，等价的
/// 阻塞面变成 **A 路在飞派发**：T1 的 start-turn 响应被扣住（在飞），T2 仍能完整
/// 走完 ensure→派发→受理。两个 thread 共用一条 IPC socket，T2 的完成证明调度与
/// 单 thread 的在飞派发解耦。
#[tokio::test]
async fn in_flight_dispatch_does_not_block_other_thread_scheduling() {
    use http_body_util::BodyExt;
    let t1 = THREAD;
    let t2 = "0f1e2d3c-2222-4222-8333-444455556666";
    let mut f = fixture_ipc(vec![codex_app_session(t1), codex_app_session(t2)]);
    let mut peer = f.peer.take().unwrap();

    // 用 SSE 订阅持住两 thread 的 follow 引用（引用计数不归零 → 不被 unfollow 挤出，
    // 镜像保持在场）——排除 follow 生命周期对「在飞不阻塞」本体语义的干扰。
    // t1 的 ensure 含连接 initialize；t2 复用同一连接（只有 discovery/following/snapshot）。
    let (mut body1, mut body2) = {
        let app = f.app.clone();
        let task = tokio::spawn(async move {
            app.oneshot(req(
                "GET",
                &format!("/m/api/v1/codex/thread/events?session_id={t1}"),
                Some(COOKIE),
                None,
            ))
            .await
            .unwrap()
        });
        drive_ensure(&mut peer, t1, vec![], "idle").await;
        let r = tokio::time::timeout(Duration::from_secs(15), task)
            .await
            .expect("sse in time")
            .unwrap();
        assert_eq!(r.status(), 200);
        let b1 = r.into_body();

        let app = f.app.clone();
        let task = tokio::spawn(async move {
            app.oneshot(req(
                "GET",
                &format!("/m/api/v1/codex/thread/events?session_id={t2}"),
                Some(COOKIE),
                None,
            ))
            .await
            .unwrap()
        });
        let disc = peer.recv().await;
        assert_eq!(disc["method"], "thread-owner-discovery");
        peer.send(&discovery_response(
            disc["requestId"].as_str().unwrap(),
            "owner-1",
        ))
        .await;
        let follow = peer.recv().await;
        assert_eq!(follow["method"], "thread-stream-following-changed");
        peer.send(&serde_json::json!({
            "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
            "sourceClientId": "owner-1",
            "params": {
                "conversationId": t2, "hostId": "local",
                "change": {
                    "type": "snapshot", "revision": 7,
                    "conversationState": {
                        "turns": [], "requests": [],
                        "threadRuntimeStatus": { "type": "idle" },
                        "turnHistory": codex_fixture::canonical_turn_history(serde_json::json!({
                                "t1": { "id": "t1", "type": "turn", "status": "completed", "items": ["u1"] },
                                "u1": { "id": "u1", "type": "user_message", "text": "seed", "ts": 1 }
                            }), serde_json::json!([{ "entries": ["t1"] }]))
                    }
                }
            }
        }))
        .await;
        let r = tokio::time::timeout(Duration::from_secs(15), task)
            .await
            .expect("sse in time")
            .unwrap();
        assert_eq!(r.status(), 200);
        (b1, r.into_body())
    };
    // 各消费首帧 snapshot（流保持打开 = follow 引用持续在持）
    for b in [&mut body1, &mut body2] {
        let first = tokio::time::timeout(Duration::from_secs(10), b.frame())
            .await
            .expect("first sse frame")
            .unwrap();
        assert!(first.is_ok(), "首帧 snapshot 在场");
    }

    // T1 发送：start-turn 帧到达但**不回答**（在飞）
    let app = f.app.clone();
    let t1_task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(send_body("T1 慢派发", Some("act-blk-1"), false)),
        ))
        .await
        .unwrap()
    });
    let frame1 = recv_within(&mut peer, 15).await;
    assert_eq!(frame1["method"], "thread-follower-start-turn");
    assert_eq!(frame1["params"]["conversationId"], t1);

    // T1 在飞窗口：T2 入队 + 完整派发 + 受理（守卫/镜像就位，无握手帧）
    let app = f.app.clone();
    let t2_task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(send_body_for("T2 不被阻塞", Some("act-blk-2"), false, t2)),
        ))
        .await
        .unwrap()
    });
    let frame2 = recv_within(&mut peer, 15).await;
    assert_eq!(frame2["method"], "thread-follower-start-turn");
    assert_eq!(
        frame2["params"]["conversationId"], t2,
        "T2 在 T1 在飞期间照常派发"
    );
    confirm_client_message_identity_for(
        &mut peer,
        t2,
        &frame2,
        "turn-t2",
        "T2 不被阻塞",
        9,
        "owner-1",
    )
    .await;
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame2["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "turn": { "id": "turn-t2", "status": "inProgress" } }
    }))
    .await;
    let r2 = tokio::time::timeout(Duration::from_secs(20), t2_task)
        .await
        .expect("T2 in time")
        .unwrap();
    let v2: serde_json::Value = serde_json::from_str(&body_string(r2).await).unwrap();
    assert_eq!(v2["status"], "desktop_ipc", "{v2}");
    assert_eq!(v2["receipt"]["delivery"], "accepted", "T2 已受理");

    // T1 仍在飞（未回答）；收尾：回答 T1 → 受理
    assert_eq!(
        f.hub_store()
            .get_for_device("act-blk-1", "dev-1")
            .unwrap()
            .unwrap()
            .state,
        ActionState::Reserved,
        "T1 派发在途"
    );
    confirm_client_message_identity(&mut peer, &frame1, "turn-t1", "T1 慢派发", 10, "owner-1")
        .await;
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame1["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "turn": { "id": "turn-t1", "status": "inProgress" } }
    }))
    .await;
    let r1 = tokio::time::timeout(Duration::from_secs(20), t1_task)
        .await
        .expect("T1 in time")
        .unwrap();
    let v1: serde_json::Value = serde_json::from_str(&body_string(r1).await).unwrap();
    assert_eq!(v1["receipt"]["delivery"], "accepted", "{v1}");
}

#[tokio::test]
async fn device_revoked_refuses_at_dispatch_with_zero_ipc() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    f.state.codex_control.set_reconcile_period_ms(100);
    let mut peer = f.peer.take().unwrap();
    // queue_only 入队（不触发直发）
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(send_body("吊销窗口内排队", Some("act-rev-1"), true)),
        ))
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["status"], "codex_queued", "{v}");
    // 吊销设备
    DeviceStore::Owned(f.conn.clone()).with(|c| revoke_device(c, "dev-1").unwrap());
    // 接力（调度循环 settle_thread 同款流程：ensure 新鲜投影 → flush_thread）。
    // 先 spawn settle 任务（其 ensure 发出握手帧），再驱动帧，最后等 flush 结果。
    let key = f.thread_key();
    let hub = f.state.codex_control.clone();
    let settle = tokio::spawn(async move {
        let _ = hub.adapter.ensure_thread(&key).await;
        hub.router.flush_thread(&key).await
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let out = tokio::time::timeout(Duration::from_secs(10), settle).await;
    assert!(out.is_ok(), "settle in time");
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            if let Ok(Some(v)) = f.hub_store().get_for_device("act-rev-1", "dev-1") {
                if v.state != ActionState::Queued {
                    return v;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("吊销后动作必须落终态");
    let view = f
        .hub_store()
        .get_for_device("act-rev-1", "dev-1")
        .unwrap()
        .unwrap();
    assert_eq!(view.state, ActionState::Rejected, "明确零投递 = rejected");
    assert_eq!(
        view.receipt.as_ref().and_then(|r| r.reason_code.as_deref()),
        Some("device_revoked"),
        "回执原因如实"
    );
    assert!(
        !peer_has_traffic(&mut peer).await,
        "吊销后零 IPC 写入（start-turn 不出帧）"
    );
    assert!(
        !f.codex_audits()
            .iter()
            .any(|a| a.action == "codex_fallback"),
        "零 headless 审计"
    );
}

// ============================================================
// 19. #172/T8：兼容发送（实验）——未知 build 的**显式 opt-in** 发送通道
//    默认关；开关开才允许尝试发送（同一 start_turn 路径，wire 形态不变），
//    并把结果如实映射到探测账本（capability = "send"）。
// ============================================================

/// 未知 build 的 build 号（**不进 shipped 表**——这里只作「未验证 build」的替身）。
const UNKNOWN_BUILD: &str = "26.1013.9999.0";
/// 兼容发送用例固定的账本键：真实键的 `cli_id` 段要 spawn 本机 `codex.exe --version`
/// （测试绝不 spawn 本机 CLI）→ 用固定键把归因钉死。
const COMPAT_PROBE_KEY: &str = "windows|26.1013.9999.0|test-cli|1";

/// 探测账本桩：只读探测**已通过**（`state` 行 = probed）→ 探测**零帧复用**，
/// 不让用例额外跑一遍 P0–P3（那会多出 discovery/following 帧，干扰帧数断言）。
struct ProbedStateLedger;

impl ProbeLedger for ProbedStateLedger {
    fn get(&self, _probe_key: &str, capability: &str) -> Option<ProbeLedgerEntry> {
        (capability == "state").then_some(ProbeLedgerEntry {
            state: ProbeState::Probed,
            next_retry_at: None,
            // #172/T9a：探测只读事实用的两个字段（桩里没有具体级次/错误码）
            stage: Some("P3".into()),
            error_code: None,
            // 桩里没有内容指纹（只对 capability='schema' 有意义）
            fingerprint: None,
            // S1：成功证据（probed 即有过成功取证）
            evidenced: true,
            evidenced_at: None,
            last_seen_at: None,
        })
    }
    fn record(
        &self,
        _probe_key: &str,
        _capability: &str,
        _outcome: &ProbeOutcome,
        _fingerprint: Option<&str>,
    ) {
    }
}

/// 未验证 build 的「尚无证据」档 profile（能力位全关；方法表沿用基线）。
fn unknown_build_profile() -> ProtocolProfile {
    ProtocolProfile::unverified("windows", UNKNOWN_BUILD)
}

/// 未知 build fixture（假 IPC peer 在场）：endpoint 自报**未验证 build** → adapter
/// 每次连接按 endpoint 重算 profile → 写能力未取证（`build_send_verified` = false）。
fn fixture_unknown_build(sessions: Vec<Session>) -> Fixture {
    let (io, peer) = ScriptedPeer::new();
    let adapter = Arc::new(DesktopIpcAdapter::new(
        Arc::new(StaticConnector::with_build(io, UNKNOWN_BUILD)),
        unknown_build_profile(),
    ));
    adapter.set_probe_key_override(Some(COMPAT_PROBE_KEY.into()));
    adapter.set_probe_ledger(Arc::new(ProbedStateLedger));
    fixture_with(
        sessions,
        move |store| Arc::new(CodexControlHub::with_adapter(store, adapter, true)),
        Some(peer),
    )
}

/// 未知 build + **连接不可达** fixture（写前 NotSent 的成因：连接不可达）。
fn fixture_unknown_build_offline(sessions: Vec<Session>) -> Fixture {
    let adapter = Arc::new(DesktopIpcAdapter::new(
        Arc::new(FailingConnector),
        unknown_build_profile(),
    ));
    adapter.set_probe_key_override(Some(COMPAT_PROBE_KEY.into()));
    adapter.set_probe_ledger(Arc::new(ProbedStateLedger));
    fixture_with(
        sessions,
        move |store| Arc::new(CodexControlHub::with_adapter(store, adapter, true)),
        None,
    )
}

/// 打开兼容发送开关（设置项 `remote.compat_send_enabled`，缺键 = 关）。
fn enable_compat_send(f: &Fixture) {
    f.state.store.with(|c| {
        c.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('remote.compat_send_enabled', 'true')",
            [],
        )
        .unwrap();
    });
}

/// 探测账本里 `capability = "send"` 的行（probe_key, state, error_code）。
fn send_probe_rows(f: &Fixture) -> Vec<(String, String, Option<String>)> {
    let conn = f.conn.lock().unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT probe_key, state, error_code FROM codex_ipc_probe
              WHERE capability = 'send' ORDER BY probe_key",
        )
        .unwrap();
    stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    })
    .unwrap()
    .filter_map(|r| r.ok())
    .collect()
}

/// peer 后续还会收到的 `thread-follower-start-turn` 帧数（backlog + 新帧，读到
/// EOF/超时为止）——「不得重发」的帧数断言。
async fn count_start_turn_frames(peer: &mut ScriptedPeer) -> usize {
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

/// 2026-10-22 用户裁决（main 合并）：未知 build → 兼容发送档，send/state 默认开——
/// **开关关不再拦截**派发（send stays up）。本用例重钉新形态：开关保持默认关，
/// 发送照常走 start_turn、send-info 不置灰；账本**不留** send 行（实验未开启，
/// 正常路径与已验证 build 同口径）。开关门只对「无写能力证据」的档位（closed /
/// 强制压档 / 断链兜底路径）继续生效（见同文件 not_sent / inconclusive 两用例）。
#[tokio::test]
async fn unknown_build_send_dispatches_even_with_compat_switch_off() {
    let mut f = fixture_unknown_build(vec![codex_app_session(THREAD)]);
    // 连接建立前没有写能力记录（缺省 false）；ensure 后兼容档把证据立起来
    assert!(
        !f.state.codex_control.profile_send_capable(),
        "连接前：没有写能力记录"
    );
    // 开关保持默认关（不写设置键）
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let body = send_body("开关关也照发的消息", Some("act-cs-off-1"), false);
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    // 裁决新形态：send stays up——start-turn 帧照发（开关不再拦）
    let frame = peer.recv().await;
    assert_eq!(
        frame["method"], "thread-follower-start-turn",
        "兼容档：开关关也必须派发（2026-10-22 裁决）"
    );
    confirm_client_message_identity(
        &mut peer,
        &frame,
        "turn-cs-1",
        "开关关也照发的消息",
        8,
        "owner-1",
    )
    .await;
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "turn": { "id": "turn-cs-1", "status": "inProgress" } }
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(20), task)
        .await
        .expect("in time")
        .unwrap();
    assert_eq!(r.status(), 200, "兼容档：开关关也受理（不再 403）");
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["status"], "desktop_ipc", "{v}");
    assert_eq!(v["receipt"]["delivery"], "accepted", "{v}");
    // 账本（S2 新契约）：受理即写证据（设计 §3.3「发送本身提供真实写证据」）——
    // 自动适配缺省开下的未知 build 发送受理 → capability=send 记 probed。旧断言
    // 「开关关：不留 send 实验行」编码的是已废除的旧键门控（记录面只认旧开关）；
    // 旧「兼容发送（实验）」键不再参与任何门，本用例的开关保持缺省（未写）。
    let rows = send_probe_rows(&f);
    assert_eq!(rows.len(), 1, "受理记账一行: {rows:?}");
    assert_eq!(rows[0].1, "probed", "{rows:?}");
    // send-info：输入区不置灰（写门开）
    let r = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/m/api/v1/session-send-info?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let info: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(info["injectable"], serde_json::json!(true), "{info}");
}

/// #172/T8 开关开：未知 build 允许**尝试**发送（同一 start_turn 路径，wire 形态不变），
/// 假 IPC 受理 → 回执 accepted + 账本 capability=send 记 **probed**。
#[tokio::test]
async fn compat_send_switch_on_allows_attempt_and_records_probed() {
    let mut f = fixture_unknown_build(vec![codex_app_session(THREAD)]);
    enable_compat_send(&f);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let body = send_body("兼容发送消息", Some("act-cs-on-1"), false);
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let frame = peer.recv().await;
    assert_eq!(
        frame["method"], "thread-follower-start-turn",
        "开关开：允许尝试发送（走同一 start_turn 路径）"
    );
    confirm_client_message_identity(&mut peer, &frame, "turn-cs-1", "兼容发送消息", 8, "owner-1")
        .await;
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "turn": { "id": "turn-cs-1", "status": "inProgress" } }
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(20), task)
        .await
        .expect("in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["status"], "desktop_ipc", "{v}");
    assert_eq!(v["receipt"]["delivery"], "accepted", "{v}");
    // 账本：capability = "send" → probed（该 build 的发送确被受理）
    let rows = send_probe_rows(&f);
    assert_eq!(rows.len(), 1, "账本只应有 send 一行: {rows:?}");
    assert_eq!(rows[0].0, COMPAT_PROBE_KEY);
    assert_eq!(rows[0].1, "probed", "{rows:?}");
    assert_eq!(rows[0].2, None, "probed 不带错误码: {rows:?}");
    // send-info 同源：输入区不置灰（发送门放行）
    let r = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/m/api/v1/session-send-info?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    let info: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(info["injectable"], serde_json::json!(true), "{info}");
}

/// 【S2 重写（原 `compat_send_not_sent_records_failed_and_allows_one_fallback`）】
/// 旧断言「写前未投递 → 账本记 failed + 允许一次 headless 兜底」随兜底废除。新
/// 契约 = 离线（Desktop 不可达）→ 状态不可确认 → **零投递留队冻结**：消息在账
/// 排队等恢复，无 headless 封套、无兜底审计、无写尝试（账本无 send 行——failed
/// 只记「确实尝试过写但写前失败」的情形，冻结不是写尝试）。
#[tokio::test]
async fn unknown_build_offline_send_queues_without_headless() {
    let _path = IsolatedPath::lock_empty();
    let f = fixture_unknown_build_offline(vec![codex_app_session(THREAD)]);
    // H3 开（若存在任何 headless 去向也必须暴露为零）
    f.state.store.with(|c| {
        c.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('remote.headless_enabled', 'true')",
            [],
        )
        .unwrap();
    });
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(send_body("离线消息", Some("act-cs-ns-1"), false)),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["status"], "codex_queued", "离线冻结留队: {v}");
    assert_eq!(v["actionId"], "act-cs-ns-1");
    let view = f
        .hub_store()
        .get_for_device("act-cs-ns-1", "dev-1")
        .unwrap()
        .unwrap();
    assert_eq!(view.state, ActionState::Queued, "留队等待 Desktop 恢复");
    // 零兜底审计 + 零写尝试（账本无 send 行）
    assert!(
        !f.codex_audits()
            .iter()
            .any(|a| a.action == "codex_fallback"),
        "零兜底审计: {:?}",
        f.codex_audits()
    );
    assert!(
        send_probe_rows(&f).is_empty(),
        "冻结不是写尝试，不记 send 行: {:?}",
        send_probe_rows(&f)
    );
    // queue/exec 通道零投递
    let queue_rows: i64 = {
        let conn = f.conn.lock().unwrap();
        conn.query_row("SELECT COUNT(*) FROM inject_queue", [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(queue_rows, 0);
}

/// #172/T8 结果未知（超时/断链）→ 账本记 **inconclusive** + reason `delivery_unknown`，
/// **不重发、不 fallback**（wire 上不得出现第二帧）。
#[tokio::test]
async fn compat_send_unknown_records_inconclusive_and_never_retries() {
    let mut f = fixture_unknown_build(vec![codex_app_session(THREAD)]);
    enable_compat_send(&f);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let body = send_body("未知投递消息", Some("act-cs-uk-1"), false);
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let frame = peer.recv().await;
    assert_eq!(frame["method"], "thread-follower-start-turn");
    // 对端断链（EOF）→ 在飞 mutating RPC 结果未知
    peer.eof();
    let r = tokio::time::timeout(Duration::from_secs(20), task)
        .await
        .expect("in time")
        .unwrap();
    assert_eq!(r.status(), 200, "unknown 是 200 的明确回执");
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["receipt"]["delivery"], "unknown_delivery", "{v}");
    // 账本：capability = "send" → inconclusive + reason delivery_unknown
    let rows = send_probe_rows(&f);
    assert_eq!(rows.len(), 1, "账本只应有 send 一行: {rows:?}");
    assert_eq!(rows[0].1, "inconclusive", "{rows:?}");
    assert_eq!(
        rows[0].2.as_deref(),
        Some("delivery_unknown"),
        "未知结果的固定原因码: {rows:?}"
    );
    // **没有第二帧**：未知结果绝不自动重发
    assert_eq!(
        count_start_turn_frames(&mut peer).await,
        0,
        "未知结果不得重发（wire 上不得出现第二帧）"
    );
    // 未知结果不触发兜底（router 只给 NotSent 兜底资格）
    let handed_off = f
        .codex_audits()
        .iter()
        .filter(|a| a.action == "codex_fallback" && a.result.contains("handed_off"))
        .count();
    assert_eq!(handed_off, 0, "未知结果不得兜底");
}

// 17. 任务 4：同代只读消息分页（GET /codex/thread/messages）
// ============================================================

/// 单回合 N 条 agentMessage 的 canonical 状态（任务 4 合成分页夹具）。
fn paged_state(ids: &[&str]) -> serde_json::Value {
    let items: Vec<serde_json::Value> = ids
        .iter()
        .map(|id| {
            serde_json::json!({ "type": "agentMessage", "id": id, "text": format!("text-{id}") })
        })
        .collect();
    serde_json::json!({
        "turns": [], "requests": [],
        "threadRuntimeStatus": { "type": "idle" },
        "turnHistory": { "kind": "canonical", "history": {
            "isComplete": true,
            "entitiesByKey": {
                "turn:t1": { "turnId": "t1", "status": "completed", "items": items }
            },
            "islands": [{ "entries": [{ "key": "turn:t1", "value": "turn:t1" }] }]
        }}
    })
}

/// 分页夹具 + 一个仍在活动的最新 Turn：让 t1 成为「已结算」区域（token v2 的
/// contentEpoch 只在已结算 Turn 的渲染内容变化时推进；最新 Turn 视为仍在流式产出）。
fn paged_state_with_active_tail(ids: &[&str]) -> serde_json::Value {
    let mut state = paged_state(ids);
    let history = state
        .get_mut("turnHistory")
        .and_then(|turn_history| turn_history.get_mut("history"))
        .expect("history");
    history["entitiesByKey"]["turn:tail"] = serde_json::json!({
        "turnId": "tail",
        "status": "inProgress",
        "items": [{ "type": "agentMessage", "id": "tail-stream", "text": "streaming" }]
    });
    history["islands"][0]["entries"]
        .as_array_mut()
        .expect("island entries")
        .push(serde_json::json!({ "key": "turn:tail", "value": "turn:tail" }));
    state
}

fn incomplete_paged_state(ids: &[&str]) -> serde_json::Value {
    let mut state = paged_state(ids);
    state["turnHistory"]["history"]["isComplete"] = serde_json::json!(false);
    state
}

/// `thread-stream-state-changed` 广播帧（任务 4 分页夹具）。
fn paged_state_frame(thread: &str, change: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": { "conversationId": thread, "hostId": "local", "change": change }
    })
}

fn paged_snapshot_frame(
    thread: &str,
    revision: u64,
    state: serde_json::Value,
) -> serde_json::Value {
    paged_state_frame(
        thread,
        serde_json::json!({ "type": "snapshot", "revision": revision, "conversationState": state }),
    )
}

/// 驱动一次 ensure 的协议（with_init=false 用于复用连接后的第二个 thread）。
async fn drive_ensure_state(
    peer: &mut ScriptedPeer,
    thread: &str,
    revision: u64,
    state: serde_json::Value,
    with_init: bool,
) {
    if with_init {
        handshake(peer, "owner-1").await;
    }
    let disc = peer.recv().await;
    assert_eq!(disc["method"], "thread-owner-discovery");
    assert_eq!(disc["params"]["conversationId"], thread);
    peer.send(&discovery_response(
        disc["requestId"].as_str().unwrap(),
        "owner-1",
    ))
    .await;
    let follow = peer.recv().await;
    assert_eq!(follow["method"], "thread-stream-following-changed");
    assert_eq!(follow["params"]["conversationId"], thread);
    peer.send(&paged_snapshot_frame(thread, revision, state))
        .await;
}

/// 限时收帧（防测试悬挂）。
async fn recv_frame(peer: &mut ScriptedPeer, secs: u64) -> serde_json::Value {
    tokio::time::timeout(Duration::from_secs(secs), peer.recv())
        .await
        .expect("帧超时")
}

fn messages_uri(session: &str, query: &str) -> String {
    format!("/m/api/v1/codex/thread/messages?session_id={session}{query}")
}

/// 带设备 cookie 的 GET，返回 (状态码, JSON)。
async fn http_json(app: &axum::Router, uri: &str) -> (axum::http::StatusCode, serde_json::Value) {
    let r = app
        .clone()
        .oneshot(req("GET", uri, Some(COOKIE), None))
        .await
        .unwrap();
    let status = r.status();
    let body = body_string(r).await;
    (
        status,
        serde_json::from_str(&body).unwrap_or_else(|_| panic!("非 JSON 响应: {body}")),
    )
}

/// GET /codex/thread 并驱动假 Desktop 的 ensure（有界只读路径）。
async fn get_thread_state(
    f: &Fixture,
    peer: &mut ScriptedPeer,
    session: &str,
    revision: u64,
    state: serde_json::Value,
    with_init: bool,
) -> serde_json::Value {
    let app = f.app.clone();
    let uri = format!("/m/api/v1/codex/thread?session_id={session}");
    let task = tokio::spawn(async move {
        app.oneshot(req("GET", &uri, Some(COOKIE), None))
            .await
            .unwrap()
    });
    drive_ensure_state(peer, session, revision, state, with_init).await;
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("thread state in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    serde_json::from_str(&body_string(r).await).unwrap()
}

/// 投影尾窗 → 更早两页 → end；不含游标、不重复不跳过、只读、no-store。
#[tokio::test]
async fn thread_messages_pages_older_history_over_http() {
    let ids: Vec<String> = (0..600).map(|i| format!("m{i:03}")).collect();
    let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let projection = get_thread_state(&f, &mut peer, THREAD, 1, paged_state(&refs), true).await;
    let window: Vec<&str> = projection["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(window.len(), 500, "尾窗最多 500 条");
    assert_eq!(window[0], "m100");
    let token = projection["historyToken"]
        .as_str()
        .expect("投影必须带兼容的 historyToken")
        .to_string();
    assert!(!token.is_empty());

    // 第一页：游标 = 尾窗最早条 → m050..m099（响应必须 no-store）
    let uri = messages_uri(
        THREAD,
        &format!("&before_id=m100&history_token={token}&limit=50"),
    );
    let r = f
        .app
        .clone()
        .oneshot(req("GET", &uri, Some(COOKIE), None))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(
        r.headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok()),
        Some("no-store"),
        "分页响应必须 no-store"
    );
    let page: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    let first: Vec<&str> = page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(page["status"], "ready");
    assert_eq!(page["revision"], 1);
    assert!(page["reasonCode"].is_null());
    assert_eq!(first.len(), 50);
    assert_eq!(first[0], "m050");
    assert_eq!(first[49], "m099");
    assert!(!first.contains(&"m100"), "页不含游标自身");
    assert_eq!(page["nextBefore"], "m050");
    let token2 = page["historyToken"].as_str().unwrap().to_string();
    assert_ne!(token2, token, "下一页 token 绑定新的游标前缀");

    // 第二页：m000..m049
    let uri = messages_uri(THREAD, &format!("&before_id=m050&history_token={token2}"));
    let (status, page) = http_json(&f.app, &uri).await;
    assert_eq!(status, 200);
    let second: Vec<&str> = page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(page["status"], "ready");
    assert_eq!(second.len(), 50);
    assert_eq!(second[0], "m000");
    assert_eq!(second[49], "m049");
    assert_eq!(page["nextBefore"], "m000");
    let token3 = page["historyToken"].as_str().unwrap().to_string();

    // 第三页：真正到头 → end（空页 + null nextBefore）
    let uri = messages_uri(THREAD, &format!("&before_id=m000&history_token={token3}"));
    let (status, page) = http_json(&f.app, &uri).await;
    assert_eq!(status, 200);
    assert_eq!(page["status"], "end");
    assert_eq!(page["messages"], serde_json::json!([]));
    assert!(page["nextBefore"].is_null());
    assert!(page["reasonCode"].is_null(), "真正到头是正常状态");

    // 只读：整轮分页没有触发任何 IPC 帧（不发送、不审批、不重拉）。
    assert!(
        !peer_has_traffic(&mut peer).await,
        "分页不得触发发送/审批等写动作"
    );
    f.state.codex_control.adapter.shutdown().await;
}

/// 参数校验：缺 session_id / history_token、limit 越界与非数字 → 400；
/// limit 边界 1/50 合法；无 cookie → 403（门禁结构性覆盖）。
#[tokio::test]
async fn thread_messages_rejects_bad_parameters_and_missing_auth() {
    let f = fixture_disabled(vec![codex_app_session(THREAD)]);
    for uri in [
        "/m/api/v1/codex/thread/messages".to_string(),
        format!("/m/api/v1/codex/thread/messages?session_id={THREAD}"),
        "/m/api/v1/codex/thread/messages?history_token=v1-x".to_string(),
        messages_uri(THREAD, "&before_id=m1&history_token=v1-x&limit=0"),
        messages_uri(THREAD, "&before_id=m1&history_token=v1-x&limit=51"),
        messages_uri(THREAD, "&before_id=m1&history_token=v1-x&limit=abc"),
        messages_uri(THREAD, "&before_id=m1&history_token=v1-x&limit=1.5"),
        messages_uri(THREAD, "&before_id=m1&history_token=v1-x&limit="),
        messages_uri(THREAD, "&before_id=m1&history_token=&limit=5"),
    ] {
        let r = f
            .app
            .clone()
            .oneshot(req("GET", &uri, Some(COOKIE), None))
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "参数非法必须 400：{uri}");
    }
    let uri = messages_uri(THREAD, "&before_id=m1&history_token=v1-x");
    let r = f
        .app
        .clone()
        .oneshot(req("GET", &uri, None, None))
        .await
        .unwrap();
    assert_eq!(r.status(), 403, "无设备 cookie 必须 403");
    // 合法 limit 边界：hub 关闭（没有 Desktop）→ 200 unavailable，而不是 400。
    for limit in [1, 50] {
        let uri = messages_uri(
            THREAD,
            &format!("&before_id=m1&history_token=v1-x&limit={limit}"),
        );
        let (status, page) = http_json(&f.app, &uri).await;
        assert_eq!(status, 200, "limit={limit} 合法");
        assert_eq!(page["status"], "unavailable");
        assert_eq!(page["reasonCode"], "mirror_unavailable");
    }
}

/// 一个会话不能读取别的线程：S1 的游标/ token 打到 S2 必须 cursor_invalid，
/// 且响应不携带 S1 正文。
#[tokio::test]
async fn thread_messages_cannot_read_another_thread() {
    const S1: &str = "aaaa1111-1111-4111-8111-111111111111";
    const S2: &str = "bbbb2222-2222-4222-8222-222222222222";
    let mut f = fixture_ipc(vec![codex_app_session(S1), codex_app_session(S2)]);
    let mut peer = f.peer.take().unwrap();
    let p1 = get_thread_state(&f, &mut peer, S1, 1, paged_state(&["s1-a", "s1-b"]), true).await;
    let p2 = get_thread_state(
        &f,
        &mut peer,
        S2,
        1,
        paged_state(&["s2-a", "s2-b", "s2-c"]),
        false,
    )
    .await;
    assert_eq!(p1["messages"][0]["id"], "s1-a");
    assert_eq!(p2["messages"][0]["id"], "s2-a");
    let token1 = p1["historyToken"].as_str().unwrap().to_string();

    // S2 + S1 的游标：S2 自己的历史里没有该游标 → cursor_not_found
    let uri = messages_uri(S2, &format!("&before_id=s1-a&history_token={token1}"));
    let (status, page) = http_json(&f.app, &uri).await;
    assert_eq!(status, 200);
    assert_eq!(page["status"], "cursor_invalid");
    assert_eq!(page["reasonCode"], "cursor_not_found");
    assert_eq!(page["messages"], serde_json::json!([]));
    assert!(
        !serde_json::to_string(&page).unwrap().contains("s1-"),
        "不得回传另一个线程的消息"
    );

    // S2 自己的游标 + S1 的 token：token 绑定别的线程/前缀 → 拒绝
    let uri = messages_uri(S2, &format!("&before_id=s2-a&history_token={token1}"));
    let (_, page) = http_json(&f.app, &uri).await;
    assert_eq!(page["status"], "cursor_invalid");
    assert_eq!(page["reasonCode"], "history_rewritten");

    // S2 游标 + S2 token → 真正到头
    let token2 = p2["historyToken"].as_str().unwrap().to_string();
    let uri = messages_uri(S2, &format!("&before_id=s2-a&history_token={token2}"));
    let (_, page) = http_json(&f.app, &uri).await;
    assert_eq!(page["status"], "end");
    f.state.codex_control.adapter.shutdown().await;
}

/// 未跟随的 thread：分页端点复用 GET /codex/thread 的有界只读 ensure/hydrate；
/// 历史不完整 → loading（不得当作 end），且只发只读 load-complete-history。
#[tokio::test]
async fn thread_messages_reuses_read_only_hydrate_for_partial_history() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let uri = messages_uri(THREAD, "&before_id=m1&history_token=v1-x");
    let task = tokio::spawn(async move {
        app.oneshot(req("GET", &uri, Some(COOKIE), None))
            .await
            .unwrap()
    });
    drive_ensure_state(
        &mut peer,
        THREAD,
        1,
        incomplete_paged_state(&["m0", "m1"]),
        true,
    )
    .await;
    // 历史不完整 → 既有只读 hydrate（mutating=false 路径），不是写动作。
    let load = recv_frame(&mut peer, 10).await;
    assert_eq!(load["type"], "request");
    assert_eq!(load["method"], "thread-follower-load-complete-history");
    assert_eq!(load["params"]["conversationId"], THREAD);
    assert_eq!(load["params"]["hostId"], "local");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": load["requestId"],
        "resultType": "success", "method": "thread-follower-load-complete-history",
        "result": {}
    }))
    .await;

    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("page in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    let page: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(page["status"], "loading", "历史不完整不是 end");
    assert_eq!(page["reasonCode"], "history_incomplete");
    assert_eq!(page["messages"], serde_json::json!([]));
    assert!(page["nextBefore"].is_null());
    assert!(
        !peer_has_traffic(&mut peer).await,
        "分页 + hydrate 全程不得产生写动作帧"
    );
    f.state.codex_control.adapter.shutdown().await;
}

/// 历史改写（已分页区域换身份）后，旧游标 + 旧 token 不能继续返回旧页；
/// 新投影 token 可恢复，且改后的身份出现在页里。
#[tokio::test]
async fn thread_messages_history_rewrite_invalidates_old_page() {
    let ids: Vec<String> = (0..600).map(|i| format!("m{i:03}")).collect();
    let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let projection = get_thread_state(
        &f,
        &mut peer,
        THREAD,
        1,
        paged_state_with_active_tail(&refs),
        true,
    )
    .await;
    let token = projection["historyToken"].as_str().unwrap().to_string();
    let uri = messages_uri(
        THREAD,
        &format!("&before_id=m100&history_token={token}&limit=50"),
    );
    let (_, page) = http_json(&f.app, &uri).await;
    assert_eq!(page["status"], "ready");

    // 改写已返回页里的 m075（换身份，结构仍合法）→ revision 2
    peer.send(&paged_state_frame(
        THREAD,
        serde_json::json!({
            "type": "patches", "baseRevision": 1, "revision": 2, "patches": [
                { "op": "replace",
                  "path": ["turnHistory", "history", "entitiesByKey", "turn:t1", "items", 75, "id"],
                  "value": "m075-rewritten" }
            ]
        }),
    ))
    .await;
    let key = f.thread_key();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while f
        .state
        .codex_control
        .adapter
        .projection(&key)
        .await
        .revision
        != Some(2)
    {
        assert!(
            std::time::Instant::now() < deadline,
            "历史改写 patch 未应用到镜像"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // 旧 (游标, token) 对已改写的前缀 → cursor_invalid/history_rewritten
    let uri = messages_uri(
        THREAD,
        &format!("&before_id=m100&history_token={token}&limit=50"),
    );
    let (status, page) = http_json(&f.app, &uri).await;
    assert_eq!(status, 200);
    assert_eq!(page["status"], "cursor_invalid");
    assert_eq!(page["reasonCode"], "history_rewritten");
    assert_eq!(page["messages"], serde_json::json!([]));

    // 重新取投影 → 新 token 可用，页里出现改写后的身份
    let refreshed = f.state.codex_control.adapter.projection(&key).await;
    let fresh_token = refreshed.history_token.clone();
    let uri = messages_uri(
        THREAD,
        &format!("&before_id=m100&history_token={fresh_token}&limit=50"),
    );
    let (_, page) = http_json(&f.app, &uri).await;
    assert_eq!(page["status"], "ready");
    let rewritten = page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["id"] == "m075-rewritten");
    assert!(rewritten, "新页必须反映当前 canonical 历史");
    f.state.codex_control.adapter.shutdown().await;
}

/// 空历史 → end（缺 before_id = 历史末端锚点）；非空历史缺 before_id 必须明确
/// cursor_invalid/cursor_required（不猜最新页，也不猜到头），旧代 token 不伪装成 end。
#[tokio::test]
async fn thread_messages_empty_history_ends_and_missing_cursor_is_explicit() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let empty = get_thread_state(&f, &mut peer, THREAD, 1, paged_state(&[]), true).await;
    assert_eq!(empty["messages"], serde_json::json!([]));
    assert_eq!(empty["contentState"], "ready");
    let token = empty["historyToken"].as_str().unwrap().to_string();
    let uri = messages_uri(THREAD, &format!("&history_token={token}"));
    let (status, page) = http_json(&f.app, &uri).await;
    assert_eq!(status, 200);
    assert_eq!(page["status"], "end", "空历史是真正到头");
    assert_eq!(page["messages"], serde_json::json!([]));
    assert!(page["nextBefore"].is_null());

    // 补一条消息（新 snapshot → 新镜像重建代）：先确认镜像推进到 revision 2
    peer.send(&paged_snapshot_frame(THREAD, 2, paged_state(&["only"])))
        .await;
    let key = f.thread_key();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while f
        .state
        .codex_control
        .adapter
        .projection(&key)
        .await
        .revision
        != Some(2)
    {
        assert!(
            std::time::Instant::now() < deadline,
            "补消息 snapshot 未应用到镜像"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // 非空历史 + 缺 before_id：不得猜成最新页或 end
    let projection = f.state.codex_control.adapter.projection(&key).await;
    let uri = messages_uri(
        THREAD,
        &format!("&history_token={}", projection.history_token),
    );
    let (status, page) = http_json(&f.app, &uri).await;
    assert_eq!(status, 200);
    assert_eq!(page["status"], "cursor_invalid");
    assert_eq!(page["reasonCode"], "cursor_required");
    assert_eq!(page["messages"], serde_json::json!([]));

    // 上一代的 token（镜像重建前）也不得回退成 end
    let uri = messages_uri(THREAD, &format!("&before_id=only&history_token={token}"));
    let (_, page) = http_json(&f.app, &uri).await;
    assert_eq!(page["status"], "unavailable");
    assert_eq!(page["reasonCode"], "generation_mismatch");
    f.state.codex_control.adapter.shutdown().await;
}

// ============================================================
// 打断桌面当前回合（2026-10-21 新增端点）
// ============================================================

/// 打断端点：POST /codex/turn/interrupt → `thread-follower-interrupt-turn` **v4**
/// 帧（params：`conversationId` + `mode=user-stop` + `expectedTurnId` 安全阀）→
/// Desktop 回执 `{interruptedTurnId, ok}` → 200 如实回传打断结果。
#[tokio::test]
async fn interrupt_turn_sends_follower_interrupt_v4_with_expected_turn_id() {
    let mut f = fixture_ipc_interrupt(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    // 先 ensure（取得 owner lease + 镜像）；runtime=active = 桌面回合正在跑
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "active").await;
    let _ = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("ensure in time")
        .unwrap();

    let body = serde_json::json!({ "sessionId": THREAD, "turnId": "t-live" });
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/turn/interrupt",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
    });
    let frame = recv_within(&mut peer, 10).await;
    assert_eq!(frame["method"], "thread-follower-interrupt-turn");
    assert_eq!(frame["version"], 4, "打断方法版本 = Desktop 侧方法表基值");
    assert_eq!(frame["params"]["conversationId"], THREAD);
    assert_eq!(frame["params"]["mode"], "user-stop");
    assert_eq!(
        frame["params"]["expectedTurnId"], "t-live",
        "expectedTurnId 安全阀必须带上（缺失时 Desktop 期望版本 3 而非 4）"
    );
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-interrupt-turn",
        "result": { "interruptedTurnId": "t-live", "ok": true }
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("interrupt in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(
        v["uncertain"], false,
        "明确受理（复审 P2-3）不得带不确定标记"
    );
    assert_eq!(v["interruptedTurnId"], "t-live");
    assert_eq!(v["expectedTurnId"], "t-live");
    // 破坏性动作必须留审计（result 词与回执对应：interruptedTurnId 有值 → interrupted）
    let audits = f.codex_audits();
    assert!(
        audits
            .iter()
            .any(|a| a.action == "codex_interrupt" && a.result == "interrupted"),
        "打断动作必须进审计且 result 词 = interrupted，实际：{:?}",
        audits
    );
}

/// 桌面侧无可打断回合（回执 interruptedTurnId=null）→ 200 如实回 null，
/// **不谎报打断成功**、也不当作错误。请求**显式带 turnId**（复审 P1-1：无 turnId
/// 且无在册屏障 → 409 拒发，走不到这一臂——见 refused_without_valve 用例）。
#[tokio::test]
async fn interrupt_turn_reports_no_active_turn_without_faking_success() {
    let mut f = fixture_ipc_interrupt(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let _ = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("ensure in time")
        .unwrap();

    let body = serde_json::json!({ "sessionId": THREAD, "turnId": "t-gone" });
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/turn/interrupt",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
    });
    let frame = recv_within(&mut peer, 10).await;
    assert_eq!(frame["method"], "thread-follower-interrupt-turn");
    assert_eq!(frame["version"], 4, "打断方法版本 = Desktop 侧方法表基值");
    assert_eq!(frame["params"]["expectedTurnId"], "t-gone", "阀值照带");
    assert_eq!(frame["params"]["mode"], "user-stop");
    assert_eq!(frame["params"]["conversationId"], THREAD);
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-interrupt-turn",
        "result": { "interruptedTurnId": null, "ok": true }
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("interrupt in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["ok"], true, "回合已结束不是错误，如实回 null 不谎报成功");
    assert_eq!(
        v["uncertain"], false,
        "明确受理（复审 P2-3）不得带不确定标记"
    );
    assert_eq!(v["interruptedTurnId"], serde_json::Value::Null);
    assert_eq!(v["expectedTurnId"], "t-gone");
}

/// 复审 P1-1 无阀拒发：请求不带 turnId 且在册原生屏障查无绑定 turnId（测试库
/// 为空 → DAO 返回 Ok(None)）→ 409 `no_expected_turn`、**零 IPC 帧**、审计留
/// `refused:no_expected_turn`（复审 P3-1 全臂审计）。即便 owner lease 新鲜可用
/// （ensure 已取）也照样拒——拒发发生在 HTTP 层，先于 adapter，绝不降级为
/// 无阀打断「当前活动回合」（那会误伤用户在桌面手开的回合）。
#[tokio::test]
async fn interrupt_turn_refused_without_valve_zero_ipc_write() {
    let mut f = fixture_ipc_interrupt(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    // ensure 取 lease：证明上下文可用不是豁免理由，无阀两路（请求未带 / 屏障
    // 查无）在 HTTP 层就被拒，走不到 adapter。
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "active").await;
    let _ = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("ensure in time")
        .unwrap();

    // 只带 sessionId（测试库无在册屏障）
    let body = serde_json::json!({ "sessionId": THREAD }).to_string();
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/turn/interrupt",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 409, "无阀拒发 → 409（复审 P1-1）");
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["error"], "no_expected_turn");
    assert!(
        v["reason"].as_str().is_some_and(|s| !s.is_empty()),
        "拒绝必须带中文可读 reason"
    );
    assert!(
        !peer_has_traffic(&mut peer).await,
        "无阀拒发零 IPC 帧（不得裸发无 expectedTurnId 的打断）"
    );
    let audits = f.codex_audits();
    assert!(
        audits
            .iter()
            .any(|a| a.action == "codex_interrupt" && a.result == "refused:no_expected_turn"),
        "拒发臂必须留审计（全臂审计 P3-1），实际：{:?}",
        audits
    );
    f.state.codex_control.adapter.shutdown().await;
}

/// 复审 P2-1/P2-2 真实失败臂：目标 thread 无 owner lease → adapter
/// `current_lease` 为 None → `not_sent("stale_context")` → 409（状态冲突，不是
/// 503）、**零 IPC 帧**、审计留 `refused:stale_context`（拒绝臂同样留痕）。
///
/// 编排：只 ensure「另一个」会话的 thread——adapter 因此已连接（能力位按 endpoint
/// 重算为 interrupt-on），目标 THREAD 从未被 ensure、无 lease。不能「完全不做
/// ensure」：构造期基线 profile（windows build）无 interrupt 能力位，会在 lease
/// 门之前先撞 503 capability_unavailable（能力位在首个连接时才按 endpoint 重算）。
#[tokio::test]
async fn interrupt_turn_stale_context_maps_conflict_without_sending() {
    let other = "99994444-ffff-4222-8333-444455556666";
    let mut f = fixture_ipc_interrupt(vec![codex_app_session(THREAD), codex_app_session(other)]);
    let mut peer = f.peer.take().unwrap();
    // 只 ensure other：连接建立 + interrupt 能力位就位；THREAD 保持无 lease
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={other}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, other, vec![], "active").await;
    let _ = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("ensure in time")
        .unwrap();

    // 打断目标 = 从未 ensure 的 THREAD：session/thread 解析走 rollout 兜底（UUID
    // 会话号），不依赖 ensure；expectedTurnId 已带 → 撞的是 lease 门
    let body = serde_json::json!({ "sessionId": THREAD, "turnId": "t-any" }).to_string();
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/turn/interrupt",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        409,
        "stale_context 是状态冲突 → 409（复审 P2-2，不是 503）"
    );
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["error"], "stale_context");
    assert!(
        v["reason"].as_str().is_some_and(|s| !s.is_empty()),
        "NotSent 拒绝必须带中文可读 reason（不裸示机器码）"
    );
    assert!(
        !peer_has_traffic(&mut peer).await,
        "NotSent = 写尝试开始前即失败：零 IPC 帧"
    );
    let audits = f.codex_audits();
    assert!(
        audits
            .iter()
            .any(|a| a.action == "codex_interrupt" && a.result == "refused:stale_context"),
        "拒绝臂必须留审计（result 词 refused:<reason>），实际：{:?}",
        audits
    );
    f.state.codex_control.adapter.shutdown().await;
}

/// 复审 P3-2：请求体 `deny_unknown_fields`——任何多余键（如 `ownerClientId`）
/// → 400，HTTP 层不得出现 IPC owner 参数；拒绝发生在解析期，零 IPC 帧。
#[tokio::test]
async fn interrupt_turn_rejects_unknown_fields_with_400() {
    let mut f = fixture_ipc_interrupt(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let body = serde_json::json!({
        "sessionId": THREAD, "turnId": "t-any", "ownerClientId": "spoof"
    })
    .to_string();
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/turn/interrupt",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 400, "多余键必须 400（deny_unknown_fields）");
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["error"], "bad_request");
    assert!(!peer_has_traffic(&mut peer).await, "解析期拒绝：零 IPC 帧");
    f.state.codex_control.adapter.shutdown().await;
}

/// P2-3 明确拒绝臂：Desktop 回错误回执（resultType=error，如 goalPauseError）
/// → `Delivery::Rejected` → 409 `desktop_rejected`——服务本身可用，语义是状态
/// 冲突而非「服务不可用」（不是 503）；拒绝必须带中文 reason，审计留
/// `rejected`（全臂审计 P3-1）。
#[tokio::test]
async fn interrupt_turn_rejected_maps_conflict() {
    let mut f = fixture_ipc_interrupt(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "active").await;
    let _ = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("ensure in time")
        .unwrap();

    let body = serde_json::json!({ "sessionId": THREAD, "turnId": "t-live" });
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/turn/interrupt",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
    });
    let frame = recv_within(&mut peer, 10).await;
    assert_eq!(frame["method"], "thread-follower-interrupt-turn");
    // Desktop 明确拒绝：错误回执（无需超时编排，回执直达）
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "error", "method": "thread-follower-interrupt-turn",
        "error": { "code": "goalPauseError", "message": "goal pause failed" }
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("interrupt in time")
        .unwrap();
    assert_eq!(
        r.status(),
        409,
        "Desktop 明确拒绝 → 409 状态冲突（不是 503）"
    );
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["error"], "desktop_rejected");
    assert!(
        v["reason"].as_str().is_some_and(|s| !s.is_empty()),
        "拒绝必须带中文可读 reason"
    );
    let audits = f.codex_audits();
    assert!(
        audits
            .iter()
            .any(|a| a.action == "codex_interrupt" && a.result == "rejected"),
        "明确拒绝臂必须留审计（result 词 rejected），实际：{:?}",
        audits
    );
    f.state.codex_control.adapter.shutdown().await;
}

/// P2-3 不确定臂：Desktop 收帧后**保持沉默** → 端点等满 RPC_TIMEOUT
/// （connection.rs 15s）→ `Delivery::UnknownDelivery` → 200 + `uncertain:true`
/// （帧已交 writer，打断**可能已生效**——绝不报「停止失败」诱导用户立刻重按，
/// 重按 = 双发破坏性指令）+ `reasonCode:"timeout"`；审计留 `uncertain:timeout`。
/// 耗时 ~15s 是测试代价；pending 条目由超时路径清理，无需额外收尾。
#[tokio::test]
async fn interrupt_turn_timeout_reports_uncertain_not_failure() {
    let mut f = fixture_ipc_interrupt(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "active").await;
    let _ = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("ensure in time")
        .unwrap();

    let body = serde_json::json!({ "sessionId": THREAD, "turnId": "t-live" });
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/turn/interrupt",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
    });
    let frame = recv_within(&mut peer, 10).await;
    assert_eq!(frame["method"], "thread-follower-interrupt-turn");
    // 刻意不回包：桌面沉默 → RPC 超时（15s）；外层 30s 留 2 倍余量
    let r = tokio::time::timeout(Duration::from_secs(30), task)
        .await
        .expect("interrupt in time")
        .unwrap();
    assert_eq!(r.status(), 200, "结果未知不是失败：200 如实回 uncertain");
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(
        v["uncertain"], true,
        "超时 = 打断可能已生效，必须 uncertain:true"
    );
    assert_eq!(v["interruptedTurnId"], serde_json::Value::Null);
    assert_eq!(v["reasonCode"], "timeout");
    assert_eq!(v["expectedTurnId"], "t-live");
    let audits = f.codex_audits();
    assert!(
        audits
            .iter()
            .any(|a| a.action == "codex_interrupt" && a.result == "uncertain:timeout"),
        "不确定臂必须留审计（result 词 uncertain:<reason>），实际：{:?}",
        audits
    );
    f.state.codex_control.adapter.shutdown().await;
}

/// 复审 P2-2：`canInterrupt` 能力位随队列视图下发（codex/queue 从
/// `adapter.capabilities().interrupt` 填充）。已开启打断的 build 上必须为
/// true——若填充被删（`#[serde(default)]` → 恒 false），前端打断按钮会全局
/// 静默消失，本用例变红。
#[tokio::test]
async fn queue_view_surfaces_can_interrupt_on_interrupt_capable_build() {
    let mut f = fixture_ipc_interrupt(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    // ensure 建立连接：能力位按 endpoint（已开启打断的 build）重算后可读
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "active").await;
    let _ = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("ensure in time")
        .unwrap();

    let r = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/queue?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(
        v["canInterrupt"], true,
        "interrupt 能力开的 build：队列视图必须携带 canInterrupt=true"
    );
    f.state.codex_control.adapter.shutdown().await;
}

/// 能力门：该 Desktop build 未开启 `interrupt` → 503 拒发，**零 IPC 写入**。
#[tokio::test]
async fn interrupt_turn_refused_when_capability_not_enabled() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "active").await;
    let _ = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("ensure in time")
        .unwrap();

    // 复审 P1-1 后：无 turnId 的请求在 HTTP 层就被 409 no_expected_turn 拒掉、
    // 到不了能力门——要钉「能力未开启」语义，请求必须带 turnId 走到 adapter。
    let body = serde_json::json!({ "sessionId": THREAD, "turnId": "t-any" }).to_string();
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/turn/interrupt",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 503, "能力未开启 → 503 拒发");
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["error"], "capability_unavailable");
    assert!(
        v["reason"].as_str().is_some_and(|s| !s.is_empty()),
        "拒绝必须带中文可读 reason（复审 P2-2）"
    );
    assert!(
        !peer_has_traffic(&mut peer).await,
        "能力未开启时零 IPC 写入（不盲发未验证方法）"
    );
}

/// 原生回合打断扩展（2026-10-22 B3）：回合由桌面手打产生（无在册屏障）但镜像里
/// 有 inProgress 回合 → ① 队列视图给 native_turn_active + canInterrupt；② 请求
/// 不带 turnId → 阀值回退 = 镜像活动回合 id（帧恒带 expectedTurnId，P1-1 纪律
/// 不破）；桌面确认 → 200 interrupted + 审计。诚实边界（镜像无活动回合 → 409
/// 零帧）由 refused_without_valve 用例既有覆盖（其 seed 快照无 inProgress 回合）。
#[tokio::test]
async fn interrupt_turn_falls_back_to_mirror_active_turn_for_native() {
    let mut f = fixture_ipc_interrupt(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();

    // ensure（seed：t1 completed，无屏障动作）→ 原生活动回合快照（rev 8 > 基线 7）
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "active").await;
    let _ = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("ensure in time")
        .unwrap();
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": THREAD, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": 8,
                "conversationState": {
                    "turns": [], "requests": [],
                    "threadRuntimeStatus": { "type": "active" },
                    "turnHistory": codex_fixture::canonical_turn_history(serde_json::json!({
                            "t1": { "id": "t1", "type": "turn", "status": "completed", "items": ["u1"] },
                            "u1": { "id": "u1", "type": "user_message", "text": "seed", "ts": 1 },
                            "t-native": { "id": "t-native", "type": "turn", "status": "inProgress", "items": ["u2"] },
                            "u2": { "id": "u2", "type": "user_message", "text": "桌面手打的回合", "ts": 2 }
                        }), serde_json::json!([{ "entries": ["t1"] }, { "entries": ["t-native"] }]))
                }
            }
        }
    }))
    .await;

    // ① 队列视图：无屏障 + 镜像活动原生回合 → native_turn_active + canInterrupt。
    //    快照广播经连接转发任务异步入镜像，轮询等待对账窗口。
    let mut saw_code = false;
    for _ in 0..40 {
        let r = f
            .app
            .clone()
            .oneshot(req(
                "GET",
                &format!("/m/api/v1/codex/queue?session_id={THREAD}"),
                Some(COOKIE),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        if v["blockedReasonCode"] == "native_turn_active" {
            assert_eq!(v["canInterrupt"], true, "interrupt 能力 build → 可打断");
            saw_code = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        saw_code,
        "快照应在对账窗口内使队列视图给出 native_turn_active"
    );

    // ② 不带 turnId 的打断：阀值回退 = 镜像活动回合（帧恒带 expectedTurnId）
    let body = serde_json::json!({ "sessionId": THREAD }).to_string();
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/turn/interrupt",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    let frame = recv_within(&mut peer, 10).await;
    assert_eq!(frame["method"], "thread-follower-interrupt-turn");
    assert_eq!(frame["version"], 4);
    assert_eq!(
        frame["params"]["expectedTurnId"], "t-native",
        "阀值回退：在册屏障不存在 → 用镜像活动回合 id"
    );
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-interrupt-turn",
        "result": { "interruptedTurnId": "t-native", "ok": true }
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("interrupt in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["interruptedTurnId"], "t-native");
    let audits = f.codex_audits();
    assert!(
        audits
            .iter()
            .any(|a| a.action == "codex_interrupt" && a.result == "interrupted"),
        "原生回合打断同样进审计（result 词 = interrupted）"
    );
}

// ============================================================
// S2：服务端 pending 硬门 + 打断后队列持久暂停 + 一次性越队（HTTP 面）
// ============================================================

/// SSE 订阅持引用的 ensure（镜像稳定在场，不被 GET 完成后的引用回收挤出）。
/// 返回 SSE 响应体（流保持打开 = 引用在持；测试结束丢弃即释放）。
async fn sse_hold_ensure(
    f: &Fixture,
    peer: &mut ScriptedPeer,
    thread: &str,
    requests: Vec<serde_json::Value>,
    runtime: &str,
) -> (axum::body::Body, serde_json::Value) {
    use http_body_util::BodyExt;
    let app = f.app.clone();
    let sid = thread.to_string();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread/events?session_id={sid}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    drive_ensure(peer, thread, requests, runtime).await;
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("sse in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    let mut body = r.into_body();
    let first = tokio::time::timeout(Duration::from_secs(10), body.frame())
        .await
        .expect("first sse frame")
        .unwrap();
    assert!(first.is_ok(), "首帧 snapshot 在场");
    let text = String::from_utf8(first.unwrap().data_ref().unwrap().to_vec()).unwrap();
    let first: serde_json::Value =
        serde_json::from_str(text.split("data:").nth(1).map(str::trim).unwrap_or("null"))
            .expect("首帧必须是 ThreadProjection JSON");
    (body, first)
}

/// 有界等待镜像快照应用（pending 清空且 revision ≥ want）：push_snapshot 是异步
/// 广播，forward 任务应用需要一拍——轮询同步快照避免竞态 sleep。
async fn wait_snapshot_applied(f: &Fixture, want_rev: u64, want_pending_empty: bool) {
    for _ in 0..100 {
        let (determinable, _busy, pending) = f
            .state
            .codex_control
            .adapter
            .send_gate_snapshot(&f.thread_key());
        let rev = f
            .state
            .codex_control
            .adapter
            .projection(&f.thread_key())
            .await
            .revision;
        if determinable && rev.is_some_and(|r| r >= want_rev) && (!want_pending_empty || !pending) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("快照 rev≥{want_rev} 未在时限内应用");
}

/// 桌面推一条快照（idle/active + 可选原生 requests；复用既有帧形态）。
async fn push_snapshot(
    peer: &mut ScriptedPeer,
    thread: &str,
    revision: u64,
    runtime: &str,
    requests: Vec<serde_json::Value>,
) {
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": thread, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": revision,
                "conversationState": {
                    "turns": [], "requests": requests,
                    "threadRuntimeStatus": { "type": runtime },
                    "turnHistory": codex_fixture::canonical_turn_history(serde_json::json!({
                            "t1": { "id": "t1", "type": "turn", "status": "completed", "items": ["u1"] },
                            "u1": { "id": "u1", "type": "user_message", "text": "seed", "ts": 1 }
                        }), serde_json::json!([{ "entries": ["t1"] }]))
                }
            }
        }
    }))
    .await;
}

/// S2 硬门①：pending + idle（有未处理原生审批请求）→ 新普通消息**拒绝入队**
/// （200 desktop_ipc / not_sent / native_pending），零 start-turn；同 actionId 重放
/// 返回账本既有回执（幂等），零新增写入。直接 HTTP 提交（绕过前端禁用）同样被拦
/// ——前端按钮不是唯一保证。
#[tokio::test]
async fn pending_idle_rejects_new_send_via_direct_http() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let body = send_body("审批没处理就想插话", Some("act-pen-1"), false);
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    // 桌面：idle + 一个待处理命令审批（drive_ensure 消费发送侧 ensure 的握手帧）
    drive_ensure(
        &mut peer,
        THREAD,
        vec![serde_json::json!({
            "id": 18, "method": "item/commandExecution/requestApproval",
            "params": { "command": "pnpm test", "cwd": "/w", "reason": "r" }
        })],
        "idle",
    )
    .await;
    let r = tokio::time::timeout(Duration::from_secs(20), task)
        .await
        .expect("in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["status"], "desktop_ipc", "{v}");
    assert_eq!(v["receipt"]["delivery"], "not_sent", "{v}");
    assert_eq!(v["receipt"]["reasonCode"], "native_pending", "{v}");
    // 账本：拒绝即终态；零队列投递
    let view = f
        .hub_store()
        .get_for_device("act-pen-1", "dev-1")
        .unwrap()
        .unwrap();
    assert_eq!(view.state, ActionState::Rejected);
    // 幂等重放：既有回执
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(send_body("审批没处理就想插话", Some("act-pen-1"), false)),
        ))
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(
        v["receipt"]["reasonCode"], "native_pending",
        "重放返回既有回执"
    );
    assert!(
        !peer_has_traffic(&mut peer).await,
        "零 start-turn（审批在场普通消息不投递）"
    );
}

/// S2 硬门①b：**不支持的审批**（权限请求，本 build 决策词未覆盖）同样是未处理
/// 原生请求——不得仅因 idle 放行普通消息。
#[tokio::test]
async fn unsupported_pending_blocks_send_too() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let body = send_body("权限卡在场也想发", Some("act-pen-2"), false);
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    drive_ensure(
        &mut peer,
        THREAD,
        vec![serde_json::json!({
            "id": 19, "method": "item/permissions/requestApproval",
            "params": { "permissions": [], "reason": "r" }
        })],
        "idle",
    )
    .await;
    let r = tokio::time::timeout(Duration::from_secs(20), task)
        .await
        .expect("in time")
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["receipt"]["delivery"], "not_sent", "{v}");
    assert_eq!(v["receipt"]["reasonCode"], "native_pending", "{v}");
    assert!(
        !peer_has_traffic(&mut peer).await,
        "unsupported pending 零投递"
    );
}

/// S2 硬门②：已在队列中的普通消息被 pending **冻结**（后台 flush 不被 idle 假象
/// 穿透）；pending 解除后按原 FIFO 接力。
#[tokio::test]
async fn background_flush_frozen_while_pending_then_relays() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    // 忙态入队（SSE 持引用，镜像稳定）
    let (_sse, _) = sse_hold_ensure(&f, &mut peer, THREAD, vec![], "active").await;
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(send_body("排队消息", Some("act-frz-1"), false)),
        ))
        .await
        .unwrap()
    });
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("in time")
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["status"], "codex_queued", "忙态 FIFO 排队: {v}");

    // 桌面转 idle 但出现原生审批 → 后台接力必须冻结（不被 idle 假象穿透）
    push_snapshot(
        &mut peer,
        THREAD,
        8,
        "idle",
        vec![serde_json::json!({
            "id": 20, "method": "item/commandExecution/requestApproval",
            "params": { "command": "pnpm build", "cwd": "/w", "reason": "r" }
        })],
    )
    .await;
    wait_snapshot_applied(&f, 8, false).await;
    {
        let hub = f.state.codex_control.clone();
        let key = f.thread_key();
        let settle = tokio::spawn(async move {
            let _ = hub.adapter.ensure_thread(&key).await;
            hub.router.flush_thread(&key).await
        });
        let out = tokio::time::timeout(Duration::from_secs(10), settle)
            .await
            .expect("settle in time")
            .expect("settle task ok");
        assert!(out.is_none(), "pending 冻结后台接力");
    }
    assert_eq!(
        f.hub_store()
            .get_for_device("act-frz-1", "dev-1")
            .unwrap()
            .unwrap()
            .state,
        ActionState::Queued,
        "冻结期消息留队"
    );

    // 审批消失（快照 idle 无 pending）→ 原 FIFO 接力派发
    push_snapshot(&mut peer, THREAD, 9, "idle", vec![]).await;
    wait_snapshot_applied(&f, 9, true).await;
    {
        let hub = f.state.codex_control.clone();
        let key = f.thread_key();
        let settle = tokio::spawn(async move {
            let _ = hub.adapter.ensure_thread(&key).await;
            hub.router.flush_thread(&key).await
        });
        // flush 会派发 start-turn（在 settle 任务内）——帧由本测试侧应答
        let frame = recv_within(&mut peer, 10).await;
        assert_eq!(frame["method"], "thread-follower-start-turn");
        confirm_client_message_identity(&mut peer, &frame, "turn-frz", "排队消息", 10, "owner-1")
            .await;
        peer.send(&serde_json::json!({
            "type": "response", "requestId": frame["requestId"],
            "resultType": "success", "method": "thread-follower-start-turn",
            "handledByClientId": "owner-1",
            "result": { "turn": { "id": "turn-frz", "status": "inProgress" } }
        }))
        .await;
        let out = tokio::time::timeout(Duration::from_secs(15), settle)
            .await
            .expect("settle in time")
            .expect("settle task ok");
        assert!(out.is_some(), "接力派发完成");
    }
    assert_eq!(
        f.hub_store()
            .get_for_device("act-frz-1", "dev-1")
            .unwrap()
            .unwrap()
            .state,
        ActionState::Accepted,
        "pending 解除后原 FIFO 接力"
    );
}

/// S2：send-info 与服务端门**同源**——pending 在场 → 置灰 native_pending（同一码
/// 同一句文案）；队列打断暂停 → 不置灰但附 queuePaused（用户仍可选恢复/先发送）。
#[tokio::test]
async fn send_info_reports_native_pending_and_queue_paused() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    // SSE 持引用 + pending 快照（同步快照读取需要镜像稳定在场）
    let (sse, _) = sse_hold_ensure(
        &f,
        &mut peer,
        THREAD,
        vec![serde_json::json!({
            "id": 21, "method": "item/commandExecution/requestApproval",
            "params": { "command": "pnpm lint", "cwd": "/w", "reason": "r" }
        })],
        "idle",
    )
    .await;

    // pending 在场 → injectable:false + native_pending
    let r = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/m/api/v1/session-send-info?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    let info: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(info["injectable"], serde_json::json!(false), "{info}");
    assert_eq!(info["reasonCode"], "native_pending", "{info}");
    assert!(!info["reason"].as_str().unwrap_or("").is_empty());

    // 审批消失 → injectable:true，无 queuePaused 字段
    push_snapshot(&mut peer, THREAD, 8, "idle", vec![]).await;
    wait_snapshot_applied(&f, 8, true).await;
    let r = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/m/api/v1/session-send-info?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    let info: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(info["injectable"], serde_json::json!(true), "{info}");
    assert!(info.get("queuePaused").is_none(), "{info}");

    // 打断后持久暂停（直写持久层 = 打断端点的落点），但**没有旧队列**：暂停
    // 没有保护对象，send-info 不展示二选一（C02：只隐藏显示，不删持久闩）。
    f.state
        .codex_control
        .router
        .store()
        .set_queue_pause(&f.thread_key(), "interrupt_accepted", "test-token")
        .unwrap();
    let r = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/m/api/v1/session-send-info?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    let info: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(info["injectable"], serde_json::json!(true), "{info}");
    assert!(info.get("queuePaused").is_none(), "{info}");
    assert!(
        f.state
            .codex_control
            .router
            .store()
            .queue_pause(&f.thread_key())
            .unwrap()
            .is_some(),
        "显示自隐不得删除持久暂停"
    );

    // 【Gate D Minor-2 钉子（S6 修正轮 2 补）】显式 resume（恢复队列端点的落点）
    // → 回到「未暂停」形态：send-info 不带 queuePaused 字段（serde 缺省 =
    // false 的区分面——暂停中的 true 与恢复后的缺位不得混淆）
    assert!(
        f.state
            .codex_control
            .router
            .store()
            .clear_queue_pause(&f.thread_key())
            .unwrap(),
        "resume 清除在册暂停行"
    );
    let r = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/m/api/v1/session-send-info?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    let info: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(info["injectable"], serde_json::json!(true), "{info}");
    assert!(
        info.get("queuePaused").is_none(),
        "恢复后不再携带 queuePaused（区分于暂停中的 true）：{info}"
    );
    drop(sse);
}

/// S2 端到端：打断受理 → 旧队列持久暂停（回执 queuePaused、队列视图如实）→
/// 新意见无授权拒绝入队（queue_paused）→ 带一次性越队授权的新意见**恰好派发一次**
/// 且旧两条原样不动 → 用户显式 resume 后旧队列按 FIFO 接力。
#[tokio::test]
async fn interrupt_accepted_pauses_queue_resume_and_bypass_flow() {
    let mut f = fixture_ipc_interrupt(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    // SSE 持引用 + active 回合（打断阀值显式携带）
    let (sse, _) = sse_hold_ensure(&f, &mut peer, THREAD, vec![], "active").await;

    // ① 两条旧消息入队（忙态 FIFO）
    for (i, act) in ["act-old-1", "act-old-2"].iter().enumerate() {
        let app = f.app.clone();
        let body = send_body_for(&format!("旧消息{i}"), Some(act), false, THREAD);
        let task = tokio::spawn(async move {
            app.oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some(COOKIE),
                Some(body),
            ))
            .await
            .unwrap()
        });
        let r = tokio::time::timeout(Duration::from_secs(15), task)
            .await
            .expect("queue in time")
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["status"], "codex_queued", "{v}");
    }
    assert_eq!(
        f.hub_store()
            .get_for_device("act-old-1", "dev-1")
            .unwrap()
            .unwrap()
            .state,
        ActionState::Queued
    );

    // ② 打断受理 → 队列暂停
    let body = serde_json::json!({ "sessionId": THREAD, "turnId": "t-live" });
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/turn/interrupt",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
    });
    // 等帧交给有界的 recv_within（Gate A Nit：删裸 sleep(2s)——ScriptedPeer
    // 自带 backlog 缓冲，回执先到也在缓冲区排队，不依赖这段固定延时）
    let frame = recv_within(&mut peer, 10).await;
    assert_eq!(frame["method"], "thread-follower-interrupt-turn");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-interrupt-turn",
        "result": { "interruptedTurnId": "t-live", "ok": true }
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("interrupt in time")
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(
        v["queuePaused"], true,
        "打断受理回执必须声明队列已暂停: {v}"
    );

    // ③ 队列视图如实：queuePaused + 两条旧消息在队
    let r = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/queue?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    let qv: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(qv["queuePaused"], serde_json::json!(true), "{qv}");
    // 终审 F3：暂停在指令发出前以 interrupt_pending 前置档建立（受理后保留）
    assert_eq!(
        qv["queuePauseReason"],
        serde_json::json!("interrupt_pending")
    );
    assert_eq!(qv["actions"].as_array().unwrap().len(), 2, "{qv}");

    // 打断闭环：被停回合置终态（快照 idle + t-live interrupted）——队列暂停不受影响
    push_snapshot(&mut peer, THREAD, 11, "idle", vec![]).await;
    wait_snapshot_applied(&f, 11, true).await;

    // ④ 打断后新意见（无授权）→ 拒绝入队 queue_paused（旧队列不被穿透）
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(send_body("打断后新意见", Some("act-new-1"), false)),
        ))
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["status"], "desktop_ipc", "{v}");
    assert_eq!(v["receipt"]["reasonCode"], "queue_paused", "{v}");

    // ⑤ 新意见带一次性越队授权 → 恰好派发一次；旧两条原样 queued
    {
        let app = f.app.clone();
        let mut body = serde_json::json!({
            "sessionId": THREAD, "text": "本条先发送",
            "actionId": "act-new-2", "bypassPausedQueue": true,
        });
        let _ = &mut body;
        let task = tokio::spawn(async move {
            app.oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some(COOKIE),
                Some(body.to_string()),
            ))
            .await
            .unwrap()
        });
        let frame = recv_within(&mut peer, 15).await;
        assert_eq!(frame["method"], "thread-follower-start-turn");
        confirm_client_message_identity_for(
            &mut peer,
            THREAD,
            &frame,
            "turn-new-2",
            "本条先发送",
            12,
            "owner-1",
        )
        .await;
        peer.send(&serde_json::json!({
            "type": "response", "requestId": frame["requestId"],
            "resultType": "success", "method": "thread-follower-start-turn",
            "handledByClientId": "owner-1",
            "result": { "turn": { "id": "turn-new-2", "status": "inProgress" } }
        }))
        .await;
        let r = tokio::time::timeout(Duration::from_secs(20), task)
            .await
            .expect("bypass send in time")
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["status"], "desktop_ipc", "{v}");
        assert_eq!(v["receipt"]["delivery"], "accepted", "{v}");
    }
    assert_eq!(
        f.hub_store()
            .get_for_device("act-old-1", "dev-1")
            .unwrap()
            .unwrap()
            .state,
        ActionState::Queued,
        "旧队列保持暂停不动"
    );
    assert_eq!(
        f.hub_store()
            .get_for_device("act-old-2", "dev-1")
            .unwrap()
            .unwrap()
            .state,
        ActionState::Queued
    );

    // ⑥ 用户显式「继续发送」→ 暂停解除 → 旧队列按 FIFO 接力（队首 act-old-1 派发）
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/queue/resume",
            Some(COOKIE),
            Some(serde_json::json!({ "sessionId": THREAD }).to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["paused"], serde_json::json!(false), "{v}");
    // 越队动作的回合终态（idle + turn-new-2 以 completed 在场）→ 顺序屏障释放，
    // 旧队列可接力（快照必须携带该 turn 的终态实体——仅 idle 不是本 Turn 的证据）
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": THREAD, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": 14,
                "conversationState": {
                    "turns": [], "requests": [],
                    "threadRuntimeStatus": { "type": "idle" },
                    "turnHistory": codex_fixture::canonical_turn_history(
                        serde_json::json!({
                            "t1": { "id": "t1", "type": "turn", "status": "completed", "items": ["u1"] },
                            "u1": { "id": "u1", "type": "user_message", "text": "seed", "ts": 1 },
                            "turn-new-2": { "id": "turn-new-2", "type": "turn", "status": "completed", "items": ["u-tn2"] },
                            "u-tn2": { "id": "u-tn2", "type": "user_message", "text": "本条先发送", "ts": 3 }
                        }),
                        serde_json::json!([{ "entries": ["t1"] }, { "entries": ["turn-new-2"] }])
                    )
                }
            }
        }
    }))
    .await;
    wait_snapshot_applied(&f, 14, true).await;
    {
        let hub = f.state.codex_control.clone();
        let key = f.thread_key();
        let settle = tokio::spawn(async move {
            let _ = hub.adapter.ensure_thread(&key).await;
            // settle_thread 同款：投影驱动的顺序屏障释放（本 Turn 终态证据）先于接力
            let p = hub.adapter.projection(&key).await;
            hub.router.release_turn_if_complete(&key, &p);
            hub.router.flush_thread(&key).await
        });
        let frame = recv_within(&mut peer, 10).await;
        assert_eq!(frame["method"], "thread-follower-start-turn");
        confirm_client_message_identity_for(
            &mut peer,
            THREAD,
            &frame,
            "turn-old-1",
            "旧消息0",
            15,
            "owner-1",
        )
        .await;
        peer.send(&serde_json::json!({
            "type": "response", "requestId": frame["requestId"],
            "resultType": "success", "method": "thread-follower-start-turn",
            "handledByClientId": "owner-1",
            "result": { "turn": { "id": "turn-old-1", "status": "inProgress" } }
        }))
        .await;
        let out = tokio::time::timeout(Duration::from_secs(15), settle)
            .await
            .expect("settle in time")
            .expect("settle task ok");
        assert!(out.is_some(), "接力完成");
    }
    assert_eq!(
        f.hub_store()
            .get_for_device("act-old-1", "dev-1")
            .unwrap()
            .unwrap()
            .state,
        ActionState::Accepted,
        "resume 后旧队列队首按 FIFO 接力"
    );
    drop(sse);
}

// ============================================================
// Gate A 修复轮（S1 域）：活值回撤回归
// ============================================================

/// Gate A #1：自动适配关闭是对**未知 build 发送门**的活值回撤——兼容档连接在场
/// （caps.send=true 已装入 profile）后翻转 `remote.codex_auto_adapt=false`：
/// session-send 立即 403（`capability_unverified`，零新帧）、send-info 置灰（同码）。
/// 不得等下次重连重算 profile 才拦（设计 §3.3「关闭 = 阻止普通发送」；S1 登记
/// 「关闭时未知 build 发送门恒 false（活值）」）。
#[tokio::test]
async fn auto_adapt_off_retracts_send_gate_live_after_compat_connection() {
    let mut f = fixture_unknown_build(vec![codex_app_session(THREAD)]);
    // fixture 用 with_adapter 装配（不带生产门注入）：补注入**活值**门，令开关
    // 翻转对已连接的适配器即刻生效（与 production() 同款闭包）。
    let gate_store = f.state.store.clone();
    f.state
        .codex_control
        .adapter
        .set_auto_adapt_gate(Some(Arc::new(move || {
            gate_store.with(multi_agents_manager_lib::remote::auto_adapt_enabled_conn)
        })));

    // —— 第一发：缺省开 → 兼容档受理（连接建立、send 能力位快照记 true）——
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let body = send_body("关闭前消息", Some("act-gate-a-1"), false);
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let frame = peer.recv().await;
    assert_eq!(frame["method"], "thread-follower-start-turn");
    confirm_client_message_identity(
        &mut peer,
        &frame,
        "turn-gate-a-1",
        "关闭前消息",
        8,
        "owner-1",
    )
    .await;
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "turn": { "id": "turn-gate-a-1", "status": "inProgress" } }
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(20), task)
        .await
        .expect("in time")
        .unwrap();
    assert_eq!(r.status(), 200, "缺省开：兼容档受理");

    // —— 翻转：显式关闭自动适配（活值，连接不重算）——
    f.state.store.with(|c| {
        c.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('remote.codex_auto_adapt', 'false')",
            [],
        )
        .unwrap();
    });

    // —— 第二发：静态门立即拒绝（不等重连）——
    let app2 = f.app.clone();
    let task2 = tokio::spawn(async move {
        app2.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(send_body("关闭后消息", Some("act-gate-a-2"), false)),
        ))
        .await
        .unwrap()
    });
    let r2 = tokio::time::timeout(Duration::from_secs(20), task2)
        .await
        .expect("in time")
        .unwrap();
    assert_eq!(r2.status(), 403, "关闭后：未知 build 普通发送必须 403");
    let v2: serde_json::Value = serde_json::from_str(&body_string(r2).await).unwrap();
    assert_eq!(v2["reasonCode"], "capability_unverified", "{v2}");
    assert_eq!(
        count_start_turn_frames(&mut peer).await,
        0,
        "关闭后不得有任何新 start-turn 帧（零投递）"
    );

    // —— send-info 同源置灰（同一理由码）——
    let r3 = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/m/api/v1/session-send-info?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r3.status(), 200);
    let info: serde_json::Value = serde_json::from_str(&body_string(r3).await).unwrap();
    assert_eq!(info["injectable"], serde_json::json!(false), "{info}");
    assert_eq!(info["reasonCode"], "capability_unverified", "{info}");
}

// ============================================================
// S3：统一预算与有界迟到收尾（设计 §5；截止原因/迟到归账/动作保留）
// ============================================================

use multi_agents_manager_lib::services::codex_control::DeliveryStage as S3DeliveryStage;
use multi_agents_manager_lib::services::codex_desktop_ipc::connection::ConnTuning as S3ConnTuning;

/// S3 有界轮询：账本动作到达期望状态（不裸 sleep 镜像实现；每轮短间隔、总时长有界）。
async fn wait_action_state(
    f: &Fixture,
    action_id: &str,
    want: ActionState,
    budget: Duration,
) -> multi_agents_manager_lib::services::codex_desktop_ipc::types::CodexActionView {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if let Ok(Some(v)) = f.hub_store().get_for_device(action_id, "dev-1") {
            if v.state == want {
                return v;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "动作 {action_id} 未在时限内到达状态 {want:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// R1a：前台 30 秒预算到点必须结束等待（写后臂）——回执「待确认」
/// （unknown_delivery + `front_budget_elapsed`），账本保持 reserved（动作不丢、
/// 不记 NotSent、不二次写入）；随后对端迟到回执经后台收尾归还原动作（accepted）。
#[tokio::test]
async fn front_budget_ends_wait_preserves_action_and_converges_late() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    f.state.codex_control.set_front_budget_ms(400);
    f.state.codex_control.set_wrap_up_ms(2_500);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let body = send_body("前台预算消息", Some("act-front-1"), false);
    let started = std::time::Instant::now();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let frame = peer.recv().await;
    assert_eq!(frame["method"], "thread-follower-start-turn", "已写出");
    // 对端不回执：前台预算必须让等待有界结束（禁止真实 30s）
    let r = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("前台预算必须结束等待（红=无预算时挂到 RPC 超时）")
        .unwrap();
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(2_000),
        "前台等待应在预算量级结束，实际 {elapsed:?}"
    );
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["status"], "desktop_ipc", "{v}");
    assert_eq!(v["receipt"]["delivery"], "unknown_delivery", "{v}");
    assert_eq!(v["receipt"]["reasonCode"], "front_budget_elapsed", "{v}");
    // 动作保留：reserved（可能已写出 → 待确认，绝不 NotSent）
    let view = f
        .hub_store()
        .get_for_device("act-front-1", "dev-1")
        .unwrap()
        .expect("动作行保留");
    assert_eq!(view.state, ActionState::Reserved, "{view:?}");
    assert_eq!(view.stage, S3DeliveryStage::Dispatching);
    // 不二次写入：前台结束后 wire 上不得出现第二帧
    assert_eq!(
        count_start_turn_frames(&mut peer).await,
        0,
        "预算到期不得触发重发"
    );
    // 迟到回执：后台收尾归还原动作（正式 userMessage 身份确认先于回执——与正常
    // 路径同一验证门，迟到回执不降级验证标准）
    confirm_client_message_identity_for(
        &mut peer,
        THREAD,
        &frame,
        "turn-front-late",
        "前台预算消息",
        8,
        "owner-1",
    )
    .await;
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "turn": { "id": "turn-front-late", "status": "inProgress" } }
    }))
    .await;
    let view = wait_action_state(
        &f,
        "act-front-1",
        ActionState::Accepted,
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(
        view.receipt.as_ref().unwrap().turn_id.as_deref(),
        Some("turn-front-late")
    );
    peer.close();
}

/// R1b：RPC 超时（收窄注参）后，**有效迟到响应仍归原动作**——槽位持有 + 收尾窗口
/// 内收敛为 accepted（`late_result` 回执），不建新卡、不重发。旧路径（被替换）：
/// 超时即删 pending 关联，迟到响应被静默丢弃，动作永远 unknown。
#[tokio::test]
async fn rpc_timeout_keeps_late_slot_and_converges_original_action() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    f.state.codex_control.adapter.set_conn_tuning(S3ConnTuning {
        rpc_timeout: Duration::from_millis(250),
        ..Default::default()
    });
    f.state.codex_control.set_wrap_up_ms(3_000);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let body = send_body("迟到归账消息", Some("act-late-1"), false);
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let frame = peer.recv().await;
    assert_eq!(frame["method"], "thread-follower-start-turn");
    // 先等 RPC 超时过去（250ms；受控注参），再投递迟到回执。Gate B #4：迟到
    // Accepted 过**身份校验门**（与正常路径同标准）——正式 userMessage 身份先于
    // 回执落地（真实 wire 行为：回显通常先于/紧随受理回执）。
    tokio::time::sleep(Duration::from_millis(400)).await;
    confirm_client_message_identity_for(
        &mut peer,
        THREAD,
        &frame,
        "turn-late-9",
        "迟到归账消息",
        8,
        "owner-1",
    )
    .await;
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "turn": { "id": "turn-late-9", "status": "inProgress" } }
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(8), task)
        .await
        .expect("迟到收敛必须有界")
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["status"], "desktop_ipc", "{v}");
    assert_eq!(v["receipt"]["delivery"], "accepted", "{v}");
    assert_eq!(v["receipt"]["reasonCode"], "late_result", "{v}");
    assert_eq!(v["receipt"]["turnId"], "turn-late-9", "{v}");
    // 原动作收敛（无新动作、无重发）
    let view = wait_action_state(
        &f,
        "act-late-1",
        ActionState::Accepted,
        Duration::from_secs(3),
    )
    .await;
    assert_eq!(
        view.receipt.as_ref().unwrap().envelope_id.as_deref(),
        frame["requestId"].as_str(),
        "迟到证据绑定原 envelope"
    );
    assert_eq!(
        count_start_turn_frames(&mut peer).await,
        0,
        "迟到收敛不重发"
    );
    peer.close();
}

/// R1d：前台预算在**写前**到期（校验/连接等待中）→ 明确零投递
/// （NotSent + `front_budget_elapsed`，queued→rejected CAS，零账本派发、零写帧）。
#[tokio::test]
async fn front_budget_expiry_before_write_refuses_without_dispatch() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    f.state.codex_control.set_front_budget_ms(400);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let body = send_body("写前到期", Some("act-front-2"), false);
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    // 握手照常；discovery 回执**延迟到预算之后**——校验等待（ensure）到期
    let init = peer.recv().await;
    assert_eq!(init["method"], "initialize");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": init["requestId"],
        "resultType": "success", "method": "initialize",
        "handledByClientId": "owner-1", "result": { "clientId": "owner-1" }
    }))
    .await;
    let disc = peer.recv().await;
    assert_eq!(disc["method"], "thread-owner-discovery");
    tokio::time::sleep(Duration::from_millis(700)).await;
    let r = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("写前到期必须有界返回")
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["receipt"]["delivery"], "not_sent", "{v}");
    assert_eq!(v["receipt"]["reasonCode"], "front_budget_elapsed", "{v}");
    // queued→rejected CAS：动作终态保留（拒绝即终态，草稿留在客户端）
    let view = f
        .hub_store()
        .get_for_device("act-front-2", "dev-1")
        .unwrap()
        .expect("拒绝动作保留行");
    assert_eq!(view.state, ActionState::Rejected, "{view:?}");
    // 零写入：start-turn 不得出现
    assert_eq!(
        count_start_turn_frames(&mut peer).await,
        0,
        "写前到期 = 零投递"
    );
    // 迟到的 discovery 回执不再影响结果（ensure 已被预算截断）
    peer.close();
}

/// R2d：迟到回执只归**原动作**，不得成为新连接的能力/owner/审批证据——
/// 收敛后探测账本零 send 行、`profile_send_capable`/静态门答案不变。
#[tokio::test]
async fn late_result_never_becomes_capability_evidence() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    f.state.codex_control.adapter.set_conn_tuning(S3ConnTuning {
        rpc_timeout: Duration::from_millis(250),
        ..Default::default()
    });
    f.state.codex_control.set_wrap_up_ms(3_000);
    let capable_before = f.state.codex_control.profile_send_capable();
    let send_rows_before = send_probe_rows(&f).len();
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let body = send_body("迟到不背书", Some("act-late-cap"), false);
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let frame = peer.recv().await;
    // Gate B #4：迟到 Accepted 过身份校验门——正式 userMessage 身份先于回执落地
    tokio::time::sleep(Duration::from_millis(400)).await;
    confirm_client_message_identity_for(
        &mut peer,
        THREAD,
        &frame,
        "turn-late-cap",
        "迟到不背书",
        8,
        "owner-1",
    )
    .await;
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "turn": { "id": "turn-late-cap", "status": "inProgress" } }
    }))
    .await;
    let _ = tokio::time::timeout(Duration::from_secs(8), task)
        .await
        .expect("迟到收敛必须有界")
        .unwrap();
    let _ = wait_action_state(
        &f,
        "act-late-cap",
        ActionState::Accepted,
        Duration::from_secs(3),
    )
    .await;
    // 能力证据不被迟到回执改写
    assert_eq!(
        send_probe_rows(&f).len(),
        send_rows_before,
        "迟到回执不得写探测账本（能力证据）：{:?}",
        send_probe_rows(&f)
    );
    assert_eq!(
        f.state.codex_control.profile_send_capable(),
        capable_before,
        "profile 快照不被迟到回执改写"
    );
    assert!(
        f.state.codex_control.send_gate_static().is_ok(),
        "静态门答案不因迟到回执变化"
    );
    peer.close();
}

// ============================================================
// S4：只读核对、补绑与人工继续（设计 §6；核对零 mutation、弱证据不解除 Unknown、
// 正式关联收敛、无 turnId 出口不重发、人工继续唯一后继）
// ============================================================

use multi_agents_manager_lib::services::codex_control::READ_VERIFY_REASON as S4ReadVerify;

/// 造「unknown + 保守屏障」行，带真实 payload hash 与正文（人工继续/核对的种子）。
/// 返回该行的 client_message_id（正式关联判据的输入）。
fn seed_unknown_send_row(f: &Fixture, action_id: &str, text: &str) -> String {
    let key = f.thread_key();
    let hash = multi_agents_manager_lib::services::codex_control::payload_hash(
        ActionKind::Send,
        &key,
        None,
        None,
        &multi_agents_manager_lib::services::codex_control::ActionPayload::Text(text.into()),
    );
    f.hub_store()
        .register(
            &ActionContext {
                action_id: action_id.into(),
                device_id: "dev-1".into(),
                device_name: "iPhone".into(),
                kind: ActionKind::Send,
                key,
            },
            ActionKind::Send,
            None,
            None,
            &hash,
            Some(text),
        )
        .unwrap();
    assert!(f
        .hub_store()
        .reserve(action_id, ControlRoute::DesktopIpc)
        .unwrap());
    f.hub_store()
        .finish_with_turn_barrier(
            &ActionReceipt {
                action_id: action_id.into(),
                delivery: Delivery::UnknownDelivery,
                route: ControlRoute::DesktopIpc,
                turn_id: None,
                envelope_id: Some("env-seed".into()),
                reason_code: Some("wrapup_elapsed".into()),
            },
            None,
        )
        .unwrap();
    client_message_id_of(f, action_id)
}

fn client_message_id_of(f: &Fixture, action_id: &str) -> String {
    let conn = f.conn.lock().unwrap();
    conn.query_row(
        "SELECT client_message_id FROM codex_actions WHERE action_id = ?1",
        [action_id],
        |r| r.get(0),
    )
    .unwrap()
}

/// 有界等待镜像推进到指定 revision（推送快照后读循环异步应用——不等待会与后续
/// HTTP 请求竞态；全部异步等待有界）。
async fn wait_mirror_revision(f: &Fixture, rev: u64) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let p = f
            .state
            .codex_control
            .adapter
            .projection(&f.thread_key())
            .await;
        if p.revision.is_some_and(|r| r >= rev) && !p.stale {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "镜像未在时限内推进到 rev {rev}（当前 {:?}）",
            p.revision
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn action_verify(f: &Fixture, action_id: &str) -> axum::http::Response<axum::body::Body> {
    f.app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/action/verify?action_id={action_id}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
}

async fn action_manual_resume(
    f: &Fixture,
    body: serde_json::Value,
) -> axum::http::Response<axum::body::Body> {
    f.app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/action/manual-resume",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
}

/// 有界 ensure 编排（S4 用例专用）：路由未注册（红轮 404）时 ensure 不会发生，
/// 裸 recv 会永久挂死——每一帧收取都带上限，超时即响亮失败。
async fn drive_ensure_bounded(peer: &mut ScriptedPeer, thread: &str, runtime: &str) {
    // initialize 握手同样有界（红轮无连接 → 无 initialize 帧）
    let init = recv_within(peer, 15).await;
    assert_eq!(init["method"], "initialize");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": init["requestId"],
        "resultType": "success", "method": "initialize",
        "handledByClientId": "owner-1", "result": { "clientId": "owner-1" }
    }))
    .await;
    let disc = recv_within(peer, 15).await;
    assert_eq!(disc["method"], "thread-owner-discovery");
    peer.send(&discovery_response(
        disc["requestId"].as_str().unwrap(),
        "owner-1",
    ))
    .await;
    let follow = recv_within(peer, 15).await;
    assert_eq!(follow["method"], "thread-stream-following-changed");
    let seed_entities = serde_json::json!({
        "t1": { "id": "t1", "type": "turn", "status": "completed", "items": ["u1"] },
        "u1": { "id": "u1", "type": "user_message", "text": "seed", "ts": 1 }
    });
    peer.send(&snapshot_frame(
        thread,
        "owner-1",
        7,
        runtime,
        seed_entities,
        serde_json::json!(["t1"]),
    ))
    .await;
}

/// 【红测 R-V1】只读核对：零 mutation（帧计数证明）；「相同文字」「revision 前进」
/// 「泛 idle」「临时 input」都不证明送达——「没找到」继续待确认、保留屏障、
/// Unknown 事实不被改写。
#[tokio::test]
async fn verify_is_read_only_and_weak_evidence_never_resolves() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    f.state.codex_control.set_front_budget_ms(400);
    f.state.codex_control.set_wrap_up_ms(2_000);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let body = send_body("原文消息", Some("act-v1"), false);
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    drive_ensure_bounded(&mut peer, THREAD, "idle").await;
    let frame = recv_within(&mut peer, 15).await;
    assert_eq!(frame["method"], "thread-follower-start-turn");
    // 对端不回执：前台到期 → 后台收尾落 unknown
    let r = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("前台预算必须有界结束")
        .unwrap();
    assert_eq!(r.status(), 200);
    wait_action_state(&f, "act-v1", ActionState::Unknown, Duration::from_secs(6)).await;
    assert!(
        f.hub_store().has_compat_fence(&f.thread_key()).unwrap(),
        "unknown 保守屏障在册"
    );
    let cmid = client_message_id_of(&f, "act-v1");

    // —— 核对 1：快照无新证据（只有开场临时投影 seed）→ unresolved，零写入 ——
    let started = std::time::Instant::now();
    let rv = action_verify(&f, "act-v1").await;
    assert_eq!(rv.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(rv).await).unwrap();
    assert_eq!(v["outcome"], "unresolved", "{v}");
    assert_eq!(v["stage"], "settled");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "单次核对必须有界返回（实际 {:?}）",
        started.elapsed()
    );

    // —— 核对 2：相同文字（无正式 clientId）+ revision 前进 + 泛 idle ——弱证据不认
    let weak_entities = serde_json::json!({
        "t1": { "id": "t1", "type": "turn", "status": "completed", "items": ["u1"] },
        "u1": { "id": "u1", "type": "user_message", "text": "seed", "ts": 1 },
        "t2": { "id": "t2", "type": "turn", "status": "completed", "items": ["u2"] },
        "u2": { "id": "u2", "type": "user_message", "text": "原文消息", "ts": 2 }
    });
    peer.send(&snapshot_frame(
        THREAD,
        "owner-1",
        8,
        "idle",
        weak_entities,
        serde_json::json!(["t1", "t2"]),
    ))
    .await;
    wait_mirror_revision(&f, 8).await;
    let rv = action_verify(&f, "act-v1").await;
    let v: serde_json::Value = serde_json::from_str(&body_string(rv).await).unwrap();
    assert_eq!(
        v["outcome"], "unresolved",
        "相同文字 + revision 前进 + idle 不是送达证据：{v}"
    );

    // —— 核对 3：别的消息身份（另一条消息的 clientId）也不认 ——
    let other_id_entities = serde_json::json!({
        "t1": { "id": "t1", "type": "turn", "status": "completed", "items": ["u1"] },
        "u1": { "id": "u1", "type": "user_message", "text": "seed", "ts": 1 },
        "t2": { "id": "t2", "type": "turn", "status": "completed", "items": ["u2"] },
        "u2": { "id": "u2", "type": "user_message", "text": "原文消息", "clientId": "别的消息身份", "ts": 2 }
    });
    peer.send(&snapshot_frame(
        THREAD,
        "owner-1",
        9,
        "idle",
        other_id_entities,
        serde_json::json!(["t1", "t2"]),
    ))
    .await;
    wait_mirror_revision(&f, 9).await;
    let rv = action_verify(&f, "act-v1").await;
    let v: serde_json::Value = serde_json::from_str(&body_string(rv).await).unwrap();
    assert_eq!(
        v["outcome"], "unresolved",
        "别 Turn/别的身份不算本动作证据：{v}"
    );

    // 弱证据绝不改写事实：账本仍 unknown、屏障保留、零 mutation 帧
    assert_eq!(
        f.hub_store()
            .get_for_device("act-v1", "dev-1")
            .unwrap()
            .unwrap()
            .state,
        ActionState::Unknown,
        "「没找到」不改写 Unknown"
    );
    assert!(
        f.hub_store().has_compat_fence(&f.thread_key()).unwrap(),
        "屏障保留"
    );
    assert_eq!(
        count_start_turn_frames(&mut peer).await,
        0,
        "核对零 mutation（帧计数证明）"
    );

    // —— 正式关联（对照组，同测内）：userMessage.clientId == 本次 cmid → 收敛送达 ——
    let echo_entities = serde_json::json!({
        "t1": { "id": "t1", "type": "turn", "status": "completed", "items": ["u1"] },
        "u1": { "id": "u1", "type": "user_message", "text": "seed", "ts": 1 },
        "t2": { "id": "t2", "type": "turn", "status": "completed", "items": ["u2"] },
        "u2": { "id": "u2", "type": "user_message", "text": "原文消息", "clientId": cmid, "ts": 2 }
    });
    peer.send(&snapshot_frame(
        THREAD,
        "owner-1",
        10,
        "idle",
        echo_entities,
        serde_json::json!(["t1", "t2"]),
    ))
    .await;
    wait_mirror_revision(&f, 10).await;
    let rv = action_verify(&f, "act-v1").await;
    assert_eq!(rv.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(rv).await).unwrap();
    assert_eq!(
        v["outcome"], "delivered",
        "正式 userMessage 关联收敛送达：{v}"
    );
    assert_eq!(v["turnId"], "t2");
    assert_eq!(
        v["runLimitReleased"], true,
        "绑定 Turn 已终态 + 新鲜 idle → 解除运行限制"
    );
    let view = f
        .hub_store()
        .get_for_device("act-v1", "dev-1")
        .unwrap()
        .unwrap();
    assert_eq!(view.state, ActionState::Accepted, "收敛为已受理");
    let receipt = view.receipt.expect("receipt");
    assert_eq!(receipt.turn_id.as_deref(), Some("t2"), "收敛回执绑定 Turn");
    assert_eq!(
        receipt.reason_code.as_deref(),
        Some(S4ReadVerify),
        "provenance=核对收敛"
    );
    assert!(
        !f.hub_store().has_compat_fence(&f.thread_key()).unwrap(),
        "Turn 终态证据解除屏障"
    );
    assert_eq!(
        count_start_turn_frames(&mut peer).await,
        0,
        "收敛同样零 mutation 帧"
    );
    peer.close();
}

/// 【红测 R-V3】单次核对不超过预算（受控注参；对端不响应 ensure → 等待被预算
/// 截断），如实 unresolved（快照缺失 ≠ 没发送）。
#[tokio::test]
async fn verify_budget_bounds_single_check() {
    let f = fixture_ipc(vec![codex_app_session(THREAD)]);
    seed_unknown_send_row(&f, "act-vb", "预算原文");
    f.state.codex_control.set_verify_budget_ms(600);
    let started = std::time::Instant::now();
    let rv = tokio::time::timeout(Duration::from_secs(8), action_verify(&f, "act-vb"))
        .await
        .expect("核对必须在预算内有界返回（红=无预算时挂到 ensure 10s 上限）");
    let elapsed = started.elapsed();
    assert_eq!(rv.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(rv).await).unwrap();
    assert_eq!(
        v["outcome"], "unresolved",
        "快照缺失/超时 → 继续待确认：{v}"
    );
    assert!(
        elapsed >= Duration::from_millis(400) && elapsed < Duration::from_secs(5),
        "核对窗口被预算约束（实际 {elapsed:?}）"
    );
    assert_eq!(
        f.hub_store()
            .get_for_device("act-vb", "dev-1")
            .unwrap()
            .unwrap()
            .state,
        ActionState::Unknown,
        "核对不改写 Unknown"
    );
}

/// 【红测 R-V4】明确零投递证明 → not_delivered（保留草稿的入队前拒绝终态）。
#[tokio::test]
async fn verify_reports_not_delivered_for_proven_zero_delivery() {
    let f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let key = f.thread_key();
    let hash = multi_agents_manager_lib::services::codex_control::payload_hash(
        ActionKind::Send,
        &key,
        None,
        None,
        &multi_agents_manager_lib::services::codex_control::ActionPayload::Text("没发出去".into()),
    );
    f.hub_store()
        .register(
            &ActionContext {
                action_id: "act-nd".into(),
                device_id: "dev-1".into(),
                device_name: "iPhone".into(),
                kind: ActionKind::Send,
                key,
            },
            ActionKind::Send,
            None,
            None,
            &hash,
            Some("没发出去"),
        )
        .unwrap();
    let receipt = ActionReceipt {
        action_id: "act-nd".into(),
        delivery: Delivery::NotSent,
        route: ControlRoute::None,
        turn_id: None,
        envelope_id: None,
        reason_code: Some("native_pending".into()),
    };
    assert!(f.hub_store().finish_queued_refusal(&receipt).unwrap());

    let rv = action_verify(&f, "act-nd").await;
    assert_eq!(rv.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(rv).await).unwrap();
    assert_eq!(v["outcome"], "not_delivered", "{v}");
    assert_eq!(v["reasonCode"], "native_pending");
    assert_eq!(v["stage"], "settled");
}

/// 【红测 R-V5 + R-F1】已受理无 turnId：核对如实 accepted_unbound；显式解除运行
/// 屏障的出口要求「新鲜目标状态可输入、无 pending」——pending 在场 409 不解除；
/// 可输入时解除且**不重发原消息**（帧计数）、已受理事实不改写。
#[tokio::test]
async fn accepted_unbound_verify_then_force_continue_requires_fresh_inputtable() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    seed_unknown_send_row(&f, "act-au", "已受理原文");
    // 改造为 accepted-with-barrier-unbound（受理事实 + 缺 Turn 身份）
    {
        let conn = f.conn.lock().unwrap();
        conn.execute(
            "UPDATE codex_actions SET state = 'accepted', \
                 receipt_json = json_object('actionId','act-au','delivery','accepted',\
                   'route','desktop_ipc','turnId',json('null'),'envelopeId','env-seed',\
                   'reasonCode','client_message_unverified'), \
                 updated_at = 2 WHERE action_id = 'act-au'",
            [],
        )
        .unwrap();
    }
    let mut peer = f.peer.take().unwrap();

    // 核对：无回显 → accepted_unbound（屏障保留）；ensure 编排与请求并发
    let rv = {
        let app = f.app.clone();
        let task = tokio::spawn(async move {
            app.oneshot(req(
                "GET",
                "/m/api/v1/codex/action/verify?action_id=act-au",
                Some(COOKIE),
                None,
            ))
            .await
            .unwrap()
        });
        drive_ensure_bounded(&mut peer, THREAD, "idle").await;
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("verify in time")
            .unwrap()
    };
    let v: serde_json::Value = serde_json::from_str(&body_string(rv).await).unwrap();
    assert_eq!(
        v["outcome"], "accepted_unbound",
        "无回显的已受理动作如实呈报：{v}"
    );
    assert!(
        f.hub_store().has_compat_fence(&f.thread_key()).unwrap(),
        "屏障保留待显式解除"
    );

    // 显式解除出口 ①：pending 在场（新鲜目标状态不可输入）→ 409，屏障不动
    let pending_entities = serde_json::json!({
        "t1": { "id": "t1", "type": "turn", "status": "completed", "items": ["u1"] },
        "u1": { "id": "u1", "type": "user_message", "text": "seed", "ts": 1 }
    });
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": THREAD, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": 8,
                "conversationState": {
                    "turns": [], "requests": [{ "id": 18, "method": "item/commandExecution/requestApproval",
                        "params": { "command": "pnpm test", "cwd": "/w", "reason": "r" } }],
                    "threadRuntimeStatus": { "type": "idle" },
                    "turnHistory": codex_fixture::canonical_turn_history(pending_entities,
                        serde_json::json!([{ "entries": ["t1"] }]))
                }
            }
        }
    }))
    .await;
    wait_mirror_revision(&f, 8).await;
    let r = force_continue(&f, Some(COOKIE), "act-au").await;
    assert_eq!(r.status(), 409, "pending 在场不解除运行屏障");
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["error"], "target_not_inputtable", "{v}");
    assert!(
        f.hub_store().has_compat_fence(&f.thread_key()).unwrap(),
        "解除被拒后屏障仍在"
    );

    // 显式解除出口 ②：新鲜 idle + 无 pending → 解除；不重发原消息；事实不改写
    let idle_entities = serde_json::json!({
        "t1": { "id": "t1", "type": "turn", "status": "completed", "items": ["u1"] },
        "u1": { "id": "u1", "type": "user_message", "text": "seed", "ts": 1 }
    });
    peer.send(&snapshot_frame(
        THREAD,
        "owner-1",
        9,
        "idle",
        idle_entities,
        serde_json::json!(["t1"]),
    ))
    .await;
    wait_mirror_revision(&f, 9).await;
    let r = force_continue(&f, Some(COOKIE), "act-au").await;
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["state"], "accepted", "解除不改写已受理事实");
    assert!(
        !f.hub_store().has_compat_fence(&f.thread_key()).unwrap(),
        "运行屏障已解除"
    );
    let view = f
        .hub_store()
        .get_for_device("act-au", "dev-1")
        .unwrap()
        .unwrap();
    assert_eq!(
        view.receipt.unwrap().reason_code.as_deref(),
        Some("client_message_unverified"),
        "原回执原样保留（不重发不重写）"
    );
    assert_eq!(
        count_start_turn_frames(&mut peer).await,
        0,
        "解除出口零重发（帧计数）"
    );
    peer.close();
}

/// 【红测 R-M1】人工继续 HTTP 面：唯一后继建立并走正常发送路径（受理回执回原
/// 动作无涉）；幂等重放指向同一后继；异载荷 409；原动作保持 Unknown。
#[tokio::test]
async fn manual_resume_http_builds_single_successor_and_respects_mismatch() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    seed_unknown_send_row(&f, "act-m1", "重新发送我");
    let mut peer = f.peer.take().unwrap();

    // 人工继续：后继 act-m2 登记 + 正常发送（ensure 编排 + start-turn + 受理）
    let body = serde_json::json!({
        "actionId": "act-m1", "newActionId": "act-m2", "text": "重新发送我", "reason": "核对无结论"
    });
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/action/manual-resume",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
    });
    drive_ensure_bounded(&mut peer, THREAD, "idle").await;
    // 有界收帧：路由未注册（红轮 404）时端点不会出帧——裸 recv 会永久挂死
    let frame = recv_within(&mut peer, 15).await;
    assert_eq!(
        frame["method"], "thread-follower-start-turn",
        "后继走正常发送路径"
    );
    confirm_client_message_identity(&mut peer, &frame, "turn-m2", "重新发送我", 8, "owner-1").await;
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "turn": { "id": "turn-m2", "status": "inProgress" } }
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("manual resume in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["status"], "resumed", "{v}");
    assert_eq!(v["successorActionId"], "act-m2");
    assert_eq!(v["predecessor"]["state"], "unknown", "原动作保持 Unknown");
    assert_eq!(v["send"]["receipt"]["delivery"], "accepted", "{v}");
    assert_eq!(
        f.hub_store()
            .get_for_device("act-m1", "dev-1")
            .unwrap()
            .unwrap()
            .state,
        ActionState::Unknown,
        "原动作事实不改写"
    );

    // 幂等重放（同请求）：指向同一后继，零新增派发
    let r = action_manual_resume(
        &f,
        serde_json::json!({
            "actionId": "act-m1", "newActionId": "act-m2", "text": "重新发送我"
        }),
    )
    .await;
    assert_eq!(r.status(), 200);
    let v2: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v2["status"], "already_resumed", "{v2}");
    assert_eq!(v2["successorActionId"], "act-m2");

    // 异载荷（同 ID 换正文）→ 409
    let r = action_manual_resume(
        &f,
        serde_json::json!({
            "actionId": "act-m1", "newActionId": "act-m3", "text": "被调包的正文"
        }),
    )
    .await;
    assert_eq!(r.status(), 409);
    let v3: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v3["error"], "manual_resume_text_mismatch", "{v3}");
    assert!(
        f.hub_store()
            .get_for_device("act-m3", "dev-1")
            .unwrap()
            .is_none(),
        "异载荷请求不登记任何新动作"
    );
    // 恰好一次后继派发（原动作 + 幂等重放 + 异载荷都没有新增写入）
    assert_eq!(
        count_start_turn_frames(&mut peer).await,
        0,
        "只有后继的一次 start-turn"
    );
    peer.close();
}

// ============================================================
// S4 修复轮（Gate B #3/#4）：迟到回执身份校验 + 补绑正向覆盖
// ============================================================

/// 【红测 GB-4a】忙态回显形态的迟到回执（success + **旧 Turn ID**——2026-10-20
/// P1 实测的 Windows wire 形态）不得绑定旧 Turn、不得提前放行运行限制：
/// 正常路径的 stale-turn 交集门 + cmid 身份确认必须延伸到迟到消费口。
#[tokio::test]
async fn late_busy_echo_never_binds_old_turn_or_releases_early() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    f.state.codex_control.adapter.set_conn_tuning(S3ConnTuning {
        rpc_timeout: Duration::from_millis(300),
        ..Default::default()
    });
    f.state.codex_control.set_wrap_up_ms(8_000);
    f.state.codex_control.set_front_budget_ms(400);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let body = send_body("忙态回显消息", Some("act-echo-1"), false);
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    // 种子镜像：t1 已终态在场（忙态回显的目标——回执 turnId 落在派发前已存在集合）
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let frame = recv_within(&mut peer, 15).await;
    assert_eq!(frame["method"], "thread-follower-start-turn");
    let r = tokio::time::timeout(Duration::from_secs(6), task)
        .await
        .expect("前台预算必须有界结束")
        .unwrap();
    assert_eq!(r.status(), 200);
    // 前台预算先返回（T 转后台收尾）；RPC 超时（300ms 注参）后投递**忙态回显**
    // 迟到回执：success + 旧 Turn t1（派发前已存在、已终态）
    tokio::time::sleep(Duration::from_millis(250)).await;
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "turn": { "id": "t1", "status": "inProgress" } }
    }))
    .await;
    // 旧形态：Accepted + 绑旧 Turn t1（t1 已终态 → 提前放行运行限制）
    // 新契约：身份校验拒绝旧 Turn 回显 → Unknown，屏障保守保留
    let view = wait_action_state(
        &f,
        "act-echo-1",
        ActionState::Unknown,
        Duration::from_secs(8),
    )
    .await;
    let receipt = view.receipt.expect("receipt");
    assert_eq!(
        receipt.reason_code.as_deref(),
        Some("stale_turn_echo"),
        "{receipt:?}"
    );
    assert_eq!(receipt.turn_id.as_deref(), None, "旧 Turn 身份不绑定");
    let bound: Option<String> = {
        // clippy await_holding_lock：guard 作用域收进块内，任何 await 前出锁
        let conn = f.conn.lock().unwrap();
        conn.query_row(
            "SELECT compat_turn_id FROM codex_actions WHERE action_id = 'act-echo-1'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(bound, None, "持久绑定槽位不写旧 Turn");
    assert!(
        f.hub_store().has_compat_fence(&f.thread_key()).unwrap(),
        "保守屏障保留（运行限制不提前放行）"
    );
    assert_eq!(count_start_turn_frames(&mut peer).await, 0, "零重发");
    peer.close();
}

/// 【红测 GB-4b】迟到 success **缺 turnId**：与正常路径同规（turn_unconfirmed
/// → 保守 Unknown），不得记成「已受理无 turnId」的 accepted 事实。
#[tokio::test]
async fn late_success_without_turn_id_stays_unknown() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    f.state.codex_control.adapter.set_conn_tuning(S3ConnTuning {
        rpc_timeout: Duration::from_millis(300),
        ..Default::default()
    });
    f.state.codex_control.set_wrap_up_ms(8_000);
    f.state.codex_control.set_front_budget_ms(400);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let body = send_body("无回合迟到回执", Some("act-nt-1"), false);
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let frame = recv_within(&mut peer, 15).await;
    assert_eq!(frame["method"], "thread-follower-start-turn");
    let r = tokio::time::timeout(Duration::from_secs(6), task)
        .await
        .expect("前台预算必须有界结束")
        .unwrap();
    assert_eq!(r.status(), 200);
    tokio::time::sleep(Duration::from_millis(250)).await;
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": {}
    }))
    .await;
    let view =
        wait_action_state(&f, "act-nt-1", ActionState::Unknown, Duration::from_secs(8)).await;
    let receipt = view.receipt.expect("receipt");
    assert_eq!(
        receipt.delivery,
        multi_agents_manager_lib::services::codex_control::Delivery::UnknownDelivery
    );
    assert_eq!(
        receipt.reason_code.as_deref(),
        Some("turn_unconfirmed"),
        "{receipt:?}"
    );
    peer.close();
}

/// 【修复轮 #3】accepted-unbound（已受理无 turnId）→ 正式回显（userMessage.clientId
/// == 账本 cmid）→ verify 补绑 Turn 身份并按终态证据解除运行限制；**补绑不改写
/// 已受理事实**（state/receipt 原样，只补 compat_turn_id 槽位）。
#[tokio::test]
async fn accepted_unbound_verify_rebinds_via_formal_echo() {
    let conn = shared_conn();
    seed_device(&conn, "dev-1", "iPhone");
    // 预置 accepted-unbound 行（restart 重建路径等价形态：Router::new 会把
    // accepted + compat_outstanding=1 + route=desktop_ipc 行读进内存 Turn 簿记）
    let cmid = "cmid-rebind-1";
    {
        let c = conn.lock().unwrap();
        c.execute(
            "INSERT INTO codex_actions (action_id, device_id, device_name, thread_id, kind, \
                 request_key, context_token, payload_hash, client_message_id, state, route, \
                 queued_text, created_at, updated_at, receipt_json, compat_outstanding) \
             VALUES ('act-rb', 'dev-1', 'iPhone', ?1, 'send', NULL, NULL, 'h-rb', ?2, \
                 'accepted', 'desktop_ipc', '补绑原文', 1, 2, ?3, 1)",
            rusqlite::params![
                THREAD,
                cmid,
                serde_json::json!({
                    "actionId": "act-rb", "delivery": "accepted", "route": "desktop_ipc",
                    "turnId": null, "envelopeId": "env-rb", "reasonCode": "late_result"
                })
                .to_string()
            ],
        )
        .unwrap();
    }
    let (io, peer) = ScriptedPeer::new();
    let adapter = Arc::new(DesktopIpcAdapter::new(
        Arc::new(StaticConnector::new(io)),
        caps_open_profile(),
    ));
    let mut f = fixture_on(
        conn,
        vec![codex_app_session(THREAD)],
        move |store| Arc::new(CodexControlHub::with_adapter(store, adapter, true)),
        Some(peer),
    );
    let mut peer = f.peer.take().unwrap();

    // 核对 ①：无回显 → accepted_unbound（屏障保留）
    {
        let app = f.app.clone();
        let task = tokio::spawn(async move {
            app.oneshot(req(
                "GET",
                "/m/api/v1/codex/action/verify?action_id=act-rb",
                Some(COOKIE),
                None,
            ))
            .await
            .unwrap()
        });
        drive_ensure_bounded(&mut peer, THREAD, "idle").await;
        let rv = tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("verify in time")
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(rv).await).unwrap();
        assert_eq!(v["outcome"], "accepted_unbound", "{v}");
    }
    assert!(f.hub_store().has_compat_fence(&f.thread_key()).unwrap());

    // 正式回显：t2（completed）承载 userMessage.clientId == cmid
    let echo_entities = serde_json::json!({
        "t1": { "id": "t1", "type": "turn", "status": "completed", "items": ["u1"] },
        "u1": { "id": "u1", "type": "user_message", "text": "seed", "ts": 1 },
        "t2": { "id": "t2", "type": "turn", "status": "completed", "items": ["u2"] },
        "u2": { "id": "u2", "type": "user_message", "text": "补绑原文", "clientId": cmid, "ts": 2 }
    });
    peer.send(&snapshot_frame(
        THREAD,
        "owner-1",
        8,
        "idle",
        echo_entities,
        serde_json::json!(["t1", "t2"]),
    ))
    .await;
    wait_mirror_revision(&f, 8).await;

    // 核对 ②：补绑 + 终态证据解除运行限制
    let rv = action_verify(&f, "act-rb").await;
    assert_eq!(rv.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(rv).await).unwrap();
    assert_eq!(v["outcome"], "delivered", "{v}");
    assert_eq!(v["turnId"], "t2");
    assert_eq!(
        v["runLimitReleased"], true,
        "绑定 Turn 终态 + 新鲜 idle → 解除"
    );
    // 补绑不改写已受理事实：state/receipt 原样；绑定槽位随屏障释放被清
    //（clear 既有语义：释放后 Turn 身份不再需要——槽位写入本身由 DAO 单测
    // `rebind_turn_only_fills_slot_without_rewriting_fact` 钉住）
    let (state, compat_out, receipt_json): (String, i64, String) = {
        // clippy await_holding_lock：guard 作用域收进块内，任何 await 前出锁
        let conn = f.conn.lock().unwrap();
        conn.query_row(
            "SELECT state, compat_outstanding, receipt_json FROM codex_actions WHERE action_id = 'act-rb'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
    };
    assert_eq!(state, "accepted");
    assert_eq!(compat_out, 0, "运行限制已解除");
    let rj: serde_json::Value = serde_json::from_str(&receipt_json).unwrap();
    assert_eq!(
        rj["reasonCode"], "late_result",
        "回执原样（补绑不改写事实）"
    );
    assert_eq!(rj["delivery"], "accepted", "已受理事实原样");
    assert!(
        !f.hub_store().has_compat_fence(&f.thread_key()).unwrap(),
        "运行限制已解除"
    );
    assert_eq!(
        count_start_turn_frames(&mut peer).await,
        0,
        "补绑零 mutation"
    );
    peer.close();
}

/// 后续线程的 ensure 编排（复用同一连接：只有 discovery/following/snapshot，
/// 无 initialize——与 in_flight_dispatch 多线程夹具同构）。
async fn drive_ensure_subsequent(peer: &mut ScriptedPeer, thread: &str) {
    let disc = recv_within(peer, 15).await;
    assert_eq!(disc["method"], "thread-owner-discovery");
    peer.send(&discovery_response(
        disc["requestId"].as_str().unwrap(),
        "owner-1",
    ))
    .await;
    let follow = recv_within(peer, 15).await;
    assert_eq!(follow["method"], "thread-stream-following-changed");
    let seed_entities = serde_json::json!({
        "t1": { "id": "t1", "type": "turn", "status": "completed", "items": ["u1"] },
        "u1": { "id": "u1", "type": "user_message", "text": "seed", "ts": 1 }
    });
    peer.send(&snapshot_frame(
        thread,
        "owner-1",
        7,
        "idle",
        seed_entities,
        serde_json::json!(["t1"]),
    ))
    .await;
}

/// 【定点复审红测】「单次核对 ≤10 秒」不被步骤②软化：多条**异动作**（异 Thread，
/// 与迟到槽位容量 32 的真实场景同构）成熟迟到 Accepted 在场（无回显——旧实现的
/// 全量归账下每条各烧 2s 回显等待）时，核对必须在预算内返回（只定向消费本动作
/// 槽位，异动作证据留给调度循环对账），且本动作仍正确收敛（迟到回执 + 正式回显
/// → accepted 绑 Turn + 运行限制按终态证据解除）。
#[tokio::test]
async fn verify_budget_not_softened_by_foreign_late_slots() {
    let threads = [
        "0f1e2d3c-a001-4222-8333-444455556666",
        "0f1e2d3c-a002-4222-8333-444455556666",
        "0f1e2d3c-a003-4222-8333-444455556666",
        "0f1e2d3c-a004-4222-8333-444455556666",
        "0f1e2d3c-a005-4222-8333-444455556666",
        "0f1e2d3c-a006-4222-8333-444455556666",
        "0f1e2d3c-a007-4222-8333-444455556666",
    ];
    let (_foreign_threads, target_thread) = (&threads[..6], threads[6]);
    let foreign_actions = ["act-f1", "act-f2", "act-f3", "act-f4", "act-f5", "act-f6"];
    let target = "act-target";
    let mut f = fixture_ipc(
        threads
            .iter()
            .map(|t| codex_app_session(t))
            .collect::<Vec<_>>(),
    );
    f.state.codex_control.adapter.set_conn_tuning(S3ConnTuning {
        rpc_timeout: Duration::from_millis(300),
        ..Default::default()
    });
    // wrap 500ms：各线程的 T 在回执到达前放弃轮询（槽位成熟后无消费者 → 留给核对）
    f.state.codex_control.set_wrap_up_ms(500);
    f.state.codex_control.set_front_budget_ms(400);
    let mut peer = f.peer.take().unwrap();

    let mut request_ids: Vec<(String, String, String)> = Vec::new(); // (action, thread, requestId)
    for (idx, thread) in threads.iter().enumerate() {
        let action = if idx == 6 {
            target
        } else {
            foreign_actions[idx]
        };
        let app = f.app.clone();
        let body = send_body_for("核对预算消息", Some(action), false, thread);
        let task = tokio::spawn(async move {
            app.oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some(COOKIE),
                Some(body),
            ))
            .await
            .unwrap()
        });
        if idx == 0 {
            drive_ensure(&mut peer, thread, vec![], "idle").await; // 含 initialize
        } else {
            drive_ensure_subsequent(&mut peer, thread).await;
        }
        let frame = recv_within(&mut peer, 15).await;
        assert_eq!(frame["method"], "thread-follower-start-turn");
        request_ids.push((
            action.to_string(),
            thread.to_string(),
            frame["requestId"].as_str().unwrap().to_string(),
        ));
        let r = tokio::time::timeout(Duration::from_secs(6), task)
            .await
            .expect("front in time")
            .unwrap();
        assert_eq!(r.status(), 200);
    }
    // 所有线程的 T 已在 wrap（500ms）内放弃轮询并落 unknown（wrapup_elapsed；
    // 各线程屏障独立，互不阻塞）
    tokio::time::sleep(Duration::from_millis(700)).await;
    for (action, thread, _) in &request_ids {
        wait_action_state(&f, action, ActionState::Unknown, Duration::from_secs(5)).await;
        let _ = thread;
    }

    // 目标动作的正式回显（userMessage.clientId == 账本 cmid）先于迟到回执落地
    let cmid = client_message_id_of(&f, target);
    let echo_entities = serde_json::json!({
        "t1": { "id": "t1", "type": "turn", "status": "completed", "items": ["u1"] },
        "u1": { "id": "u1", "type": "user_message", "text": "seed", "ts": 1 },
        "tv": { "id": "tv", "type": "turn", "status": "completed", "items": ["uv"] },
        "uv": { "id": "uv", "type": "user_message", "text": "核对预算消息", "clientId": cmid, "ts": 2 }
    });
    peer.send(&snapshot_frame(
        target_thread,
        "owner-1",
        8,
        "idle",
        echo_entities,
        serde_json::json!(["t1", "tv"]),
    ))
    .await;
    // 目标线程镜像推进到 rev 8（bounded 轮询）
    {
        let key = ThreadKey {
            host_id: "local".into(),
            thread_id: target_thread.into(),
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let p = f.state.codex_control.adapter.projection(&key).await;
            if p.revision.is_some_and(|r| r >= 8) && !p.stale {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "目标线程镜像未推进");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    // 迟到回执：6 条异动作（各自线程的新 Turn、无回显）+ 目标（tv，回显已落地）
    for (action, _thread, rid) in &request_ids {
        let turn_id = if action == target { "tv" } else { "turn-x" };
        peer.send(&serde_json::json!({
            "type": "response", "requestId": rid,
            "resultType": "success", "method": "thread-follower-start-turn",
            "handledByClientId": "owner-1",
            "result": { "turn": { "id": turn_id, "status": "inProgress" } }
        }))
        .await;
    }
    // 槽位成熟（响应经连接读循环异步 fill）：有界等待 7 条全部成熟
    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            if f.state.codex_control.adapter.late_registry().matured_len() == 7 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "迟到槽位未在时限内全部成熟"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    assert_eq!(
        f.state.codex_control.adapter.late_registry().len(),
        7,
        "成熟迟到槽位在场"
    );

    // 核对：必须 ≤10s 返回（旧实现 = 全量归账 → 6×2s ≈ 12s 软化违反）；目标动作
    // 仍正确收敛（accepted 绑 Turn），异动作证据不被核对消费（留给调度循环）
    let started = std::time::Instant::now();
    let rv = tokio::time::timeout(Duration::from_secs(25), action_verify(&f, target))
        .await
        .expect("verify in time");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(10),
        "单次核对必须 ≤10s（设计 §6.1/全局约束）；实际 {elapsed:?}"
    );
    assert_eq!(rv.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(rv).await).unwrap();
    assert_eq!(v["outcome"], "delivered", "{v}");
    assert_eq!(v["turnId"], "tv");
    let view = f
        .hub_store()
        .get_for_device(target, "dev-1")
        .unwrap()
        .unwrap();
    assert_eq!(
        view.state,
        ActionState::Accepted,
        "本动作经迟到回执正确收敛"
    );
    assert_eq!(
        f.state.codex_control.adapter.late_registry().len(),
        6,
        "异动作成熟槽位不被核对消费（留给调度循环）"
    );
    for action in foreign_actions {
        assert_eq!(
            f.hub_store()
                .get_for_device(action, "dev-1")
                .unwrap()
                .unwrap()
                .state,
            ActionState::Unknown,
            "异动作不被核对改动（证据保留）"
        );
    }
    peer.close();
}

// ============================================================
// 集中终审 F3/F4：打断与派发的竞态窗口（红轮先行）
// ============================================================

/// F3 受控定序（终审给定的合法顺序）：旧消息在队 → 发出 interrupt（回执扣住不发）
/// → Desktop 先广播 interrupted/idle 终态 → 调度 settle 释放屏障并 flush 旧消息
/// （**红轮必冲出**：暂停此刻还不存在——它在 interrupt response 之后才写）→
/// 放行 interrupt response → 断言旧队列零写出（终态帧数恒 0）。修后：打断发出前
/// 已预置暂停，终态/回执先后次序不再决定安全。
#[tokio::test]
async fn f3_interrupt_terminal_before_receipt_must_not_flush_old_queue() {
    let mut f = fixture_ipc_interrupt(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    // SSE 持引用 + active 回合（打断阀值显式携带）
    let (sse, _) = sse_hold_ensure(&f, &mut peer, THREAD, vec![], "active").await;

    // 旧消息入队（忙态 FIFO；active 镜像 → 留队）
    {
        let app = f.app.clone();
        let task = tokio::spawn(async move {
            app.oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some(COOKIE),
                Some(send_body("F3 旧消息", Some("act-f3-old"), false)),
            ))
            .await
            .unwrap()
        });
        let r = tokio::time::timeout(Duration::from_secs(15), task)
            .await
            .expect("queue in time")
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["status"], "codex_queued", "{v}");
    }
    assert_eq!(
        f.hub_store()
            .get_for_device("act-f3-old", "dev-1")
            .unwrap()
            .unwrap()
            .state,
        ActionState::Queued
    );

    // 发出打断；回执扣住不发（受理尚无回执）
    let body = serde_json::json!({ "sessionId": THREAD, "turnId": "t-live" });
    let app = f.app.clone();
    let interrupt_task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/turn/interrupt",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
    });
    let frame = recv_within(&mut peer, 10).await;
    assert_eq!(frame["method"], "thread-follower-interrupt-turn");

    // —— 终态先到：Desktop 广播 interrupted/idle（打断目标 Turn 以终态在场）——
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": THREAD, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": 20,
                "conversationState": {
                    "turns": [], "requests": [],
                    "threadRuntimeStatus": { "type": "idle" },
                    "turnHistory": codex_fixture::canonical_turn_history(
                        serde_json::json!({
                            "t-live": { "id": "t-live", "type": "turn", "status": "interrupted", "items": ["u-live"] },
                            "u-live": { "id": "u-live", "type": "user_message", "text": "seed", "ts": 1 }
                        }),
                        serde_json::json!([{ "entries": ["t-live"] }])
                    )
                }
            }
        }
    }))
    .await;
    // 等终态快照应用
    wait_snapshot_applied(&f, 20, true).await;
    // 手动驱动一次 settle（调度循环同款：释放 + flush）
    {
        let hub = f.state.codex_control.clone();
        let key = f.thread_key();
        let settle = tokio::spawn(async move {
            let p = hub.adapter.projection(&key).await;
            hub.router.release_turn_if_complete(&key, &p);
            hub.router.flush_thread(&key).await
        });
        // 给 settle 一拍执行（红轮将在此窗口内派发旧消息出帧）
        tokio::time::sleep(Duration::from_millis(300)).await;
        let settled = tokio::time::timeout(Duration::from_millis(50), settle).await;
        // 红轮：flush 已把旧消息 start-turn 帧写出（settled=Some 且 peer 有帧）
        // 修后：flush 因暂停冻结 → None、零帧。断言写在这里，两侧共用。
        let dispatched = match settled {
            Ok(Ok(out)) => out.is_some(),
            _ => false, // settle 还在跑（等锁/等待）——用帧数断言兜底
        };
        let frames = count_start_turn_frames(&mut peer).await;
        assert!(
            !dispatched && frames == 0,
            "F3：打断终态先到时旧队列不得冲出（dispatched={dispatched} frames={frames}）"
        );
    }

    // —— 放行 interrupt response（受理）——
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-interrupt-turn",
        "result": { "interruptedTurnId": "t-live", "ok": true }
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(15), interrupt_task)
        .await
        .expect("interrupt in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["queuePaused"], serde_json::json!(true), "{v}");
    // 旧消息仍留队；此后任何 settle 也不得派出
    assert_eq!(
        f.hub_store()
            .get_for_device("act-f3-old", "dev-1")
            .unwrap()
            .unwrap()
            .state,
        ActionState::Queued,
        "打断后旧消息保持暂停在队"
    );
    assert_eq!(
        count_start_turn_frames(&mut peer).await,
        0,
        "全程零旧队列写出"
    );
    drop(sse);
}

/// F3 负例①（Gate C Important 收紧）：暂停行写入失败（trigger 拒绝 INSERT）→
/// **打断指令不得发出**——保护建立不了就不开工（对齐同函数 valve 读失败即放弃
/// 的处置）。红轮（当前实现）：写失败仍发出 interrupt 帧、回执 200 受理。
/// 修后契约：5xx 放弃（未向桌面发送任何指令）、零 interrupt 帧、无暂停行、
/// 旧队列留在既有守卫链（镜像 active → busy 冻结，零写出）。
#[tokio::test]
async fn f3_pause_write_failure_aborts_interrupt_without_sending() {
    let mut f = fixture_ipc_interrupt(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let (sse, _) = sse_hold_ensure(&f, &mut peer, THREAD, vec![], "active").await;
    // 旧消息入队（active → busy 冻结在队）
    {
        let app = f.app.clone();
        let task = tokio::spawn(async move {
            app.oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some(COOKIE),
                Some(send_body("F3 写失败旧消息", Some("act-f3-wf"), false)),
            ))
            .await
            .unwrap()
        });
        let r = tokio::time::timeout(Duration::from_secs(15), task)
            .await
            .expect("queue in time")
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["status"], "codex_queued", "{v}");
    }
    // 注入：拒绝一切对 codex_queue_pause 的写入
    f.conn
        .lock()
        .unwrap()
        .execute(
            "CREATE TRIGGER pause_reject BEFORE INSERT ON codex_queue_pause \
         BEGIN SELECT RAISE(ABORT, 'pause rejected by test trigger'); END",
            [],
        )
        .unwrap();

    // 打断请求：写失败 → 必须放弃（5xx、未发送任何指令）
    let body = serde_json::json!({ "sessionId": THREAD, "turnId": "t-live" });
    let app = f.app.clone();
    let interrupt_task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/turn/interrupt",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
    });
    let r = tokio::time::timeout(Duration::from_secs(15), interrupt_task)
        .await
        .expect("interrupt in time")
        .unwrap();
    assert_eq!(
        r.status(),
        500,
        "暂停保护写失败 → 打断放弃（非受理）: {}",
        body_string(r).await
    );
    // #3：写失败臂回执语义——5xx + 可读 reason；无 queuePaused 受理字段
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert!(
        v.get("queuePaused").is_none(),
        "未受理，不装配 queuePaused: {v}"
    );
    assert!(
        v["reason"]
            .as_str()
            .unwrap_or("")
            .contains("未向桌面发送任何指令"),
        "{v}"
    );
    // 零 interrupt 帧（桌面侧零改动）
    assert!(
        !peer_has_traffic(&mut peer).await,
        "写失败后不得发出任何 interrupt 帧"
    );
    // 无暂停行（写失败未落行；重启后同样不存在）
    let paused: Option<i64> = {
        let conn = f.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM codex_queue_pause WHERE thread_id = ?1",
            [THREAD],
            |r| r.get(0),
        )
        .ok()
    };
    assert_eq!(paused, Some(0), "暂停行未写入（如实，不虚称已暂停）");
    // 旧队列留在既有守卫链：镜像仍 active（打断未发出，回合未终结）→ busy 冻结
    {
        let hub = f.state.codex_control.clone();
        let key = f.thread_key();
        let settle = tokio::spawn(async move {
            let p = hub.adapter.projection(&key).await;
            hub.router.release_turn_if_complete(&key, &p);
            hub.router.flush_thread(&key).await
        });
        let out = tokio::time::timeout(Duration::from_secs(10), settle)
            .await
            .expect("settle in time")
            .expect("settle ok");
        assert!(out.is_none(), "active 镜像下 busy 门冻结旧队列");
    }
    assert_eq!(
        f.hub_store()
            .get_for_device("act-f3-wf", "dev-1")
            .unwrap()
            .unwrap()
            .state,
        ActionState::Queued,
        "旧消息保持既有守卫链在队"
    );
    assert_eq!(
        count_start_turn_frames(&mut peer).await,
        0,
        "全程零旧队列写出"
    );
    drop(sse);
}

/// F3 负例②：暂停行读取失败（trigger 拒绝 SELECT 侧路径——用函数抽象不可行，
/// 直接对 flush 的读侧注入：DROP TABLE 使 queue_pause 读必败）。打断已受理
/// （暂停行在场前的写入成功），随后读侧失败 → 仍不得放行旧队列。
#[tokio::test]
async fn f3_pause_read_failure_must_still_block_old_queue() {
    let mut f = fixture_ipc_interrupt(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let (sse, _) = sse_hold_ensure(&f, &mut peer, THREAD, vec![], "active").await;
    // 旧消息入队
    {
        let app = f.app.clone();
        let task = tokio::spawn(async move {
            app.oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some(COOKIE),
                Some(send_body("F3 读失败旧消息", Some("act-f3-rf"), false)),
            ))
            .await
            .unwrap()
        });
        let r = tokio::time::timeout(Duration::from_secs(15), task)
            .await
            .expect("queue in time")
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["status"], "codex_queued", "{v}");
    }
    // 打断受理（暂停行正常写入）
    let body = serde_json::json!({ "sessionId": THREAD, "turnId": "t-live" });
    let app = f.app.clone();
    let interrupt_task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/turn/interrupt",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
    });
    let frame = recv_within(&mut peer, 10).await;
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-interrupt-turn",
        "result": { "interruptedTurnId": "t-live", "ok": true }
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(15), interrupt_task)
        .await
        .expect("interrupt in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["queuePaused"], serde_json::json!(true), "{v}");

    // 注入：暂停表读必败（DROP TABLE → queue_pause SELECT 报错）
    f.conn
        .lock()
        .unwrap()
        .execute("DROP TABLE codex_queue_pause", [])
        .unwrap();

    // 终态 + settle：读侧失败必须保守冻结（红轮：Err 按未暂停放行）
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": THREAD, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": 22,
                "conversationState": {
                    "turns": [], "requests": [],
                    "threadRuntimeStatus": { "type": "idle" },
                    "turnHistory": codex_fixture::canonical_turn_history(
                        serde_json::json!({
                            "t-live": { "id": "t-live", "type": "turn", "status": "interrupted", "items": ["u-live"] },
                            "u-live": { "id": "u-live", "type": "user_message", "text": "seed", "ts": 1 }
                        }),
                        serde_json::json!([{ "entries": ["t-live"] }])
                    )
                }
            }
        }
    }))
    .await;
    wait_snapshot_applied(&f, 22, true).await;
    {
        let hub = f.state.codex_control.clone();
        let key = f.thread_key();
        let settle = tokio::spawn(async move {
            let p = hub.adapter.projection(&key).await;
            hub.router.release_turn_if_complete(&key, &p);
            hub.router.flush_thread(&key).await
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        let settled = tokio::time::timeout(Duration::from_millis(50), settle).await;
        let dispatched = matches!(settled, Ok(Ok(Some(_))));
        let frames = count_start_turn_frames(&mut peer).await;
        assert!(
            !dispatched && frames == 0,
            "F3②：暂停读失败不得放行旧队列（dispatched={dispatched} frames={frames}）"
        );
    }
    drop(sse);
}

/// F4 红轮①：旧动作排队 → 人工继续撤销（后继建立）→ 旧动作的派出任务在
/// **上层检查通过之后、写出之前**被放行——writer 边界未核撤销 → 首帧写出。
/// 受控定序：用 respond 的既有 interphase 缝不可行（send 不经过）；改为
/// 「旧消息 queue_only 入队 → 手动 flush 任务启动（卡在 ensure 的握手帧上）→
/// 人工继续撤销 → 放行握手 → 断言 writer 零首帧」。红轮：send_text 的撤销
/// 检查在 ensure 之前——撤销写入后检查点已过 → 帧照发。修后：写出边界
/// （connection writer 内）复核撤销 → 零首帧 + 旧动作定格 manual_resume_revoked。
#[tokio::test]
async fn f4_revoke_after_upper_check_must_block_first_write_at_writer() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let (sse, _) = sse_hold_ensure(&f, &mut peer, THREAD, vec![], "idle").await;

    // 旧消息 queue_only 入队（不直发；flush 将取队首派发）
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(send_body("F4 旧动作", Some("act-f4-old"), true)),
        ))
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["status"], "codex_queued", "{v}");

    // 定序缝：守卫判定前挂起（模拟「上层检查已过、帧未写」的在飞窗口）
    let gate = Arc::new(tokio::sync::Notify::new());
    f.state
        .codex_control
        .adapter
        .set_write_guard_gate(Some(gate.clone()));

    // 手动 flush 任务：进到写边界守卫处挂起
    let hub = f.state.codex_control.clone();
    let key = f.thread_key();
    let flush_task = tokio::spawn(async move {
        let _ = hub.adapter.ensure_thread(&key).await;
        hub.router.flush_thread(&key).await
    });
    // 轮询到达标志（write_guard_gate_reached）——替代固定睡眠（Gate C 顺带项），
    // 有界 5s：任务未到守卫即失败（不掩盖调度问题）。
    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !f.state.codex_control.adapter.write_guard_gate_reached() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "flush 任务未在时限内到达写边界守卫"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    // —— 窗口内：人工继续撤销（先 reserve 使 DAO 走「在飞定格」臂）——
    f.hub_store()
        .reserve("act-f4-old", ControlRoute::DesktopIpc)
        .expect("reserve");
    let r = action_manual_resume(
        &f,
        serde_json::json!({
            "actionId": "act-f4-old", "newActionId": "act-f4-new",
            "text": "F4 旧动作", "reason": "F4 撤销窗口"
        }),
    )
    .await;
    assert_eq!(r.status(), 200, "人工继续成功（撤销已持久）");
    assert!(
        f.conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM codex_write_revoke WHERE action_id = 'act-f4-old'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
            > 0,
        "撤销标记在库"
    );

    // 放行守卫：判定读取撤销 → 拒绝首写
    f.state.codex_control.adapter.set_write_guard_gate(None);
    gate.notify_waiters();
    let _out = tokio::time::timeout(Duration::from_secs(15), flush_task)
        .await
        .expect("flush in time")
        .expect("flush ok");
    let frames = count_start_turn_frames(&mut peer).await;
    assert_eq!(
        frames, 0,
        "F4①：撤销胜出后 writer 边界零首帧写出（frames={frames}）"
    );
    // 旧动作定格 Unknown（manual_resume_revoked）
    let view = wait_action_state(
        &f,
        "act-f4-old",
        ActionState::Unknown,
        Duration::from_secs(5),
    )
    .await;
    // 撤销定格的两种来源都合法：人工继续事务先落（manual_resume_during_dispatch，
    // DAO ⑥ 臂在飞定格）或写边界守卫先观察（manual_resume_revoked，撤销臂）——
    // 事实都是「Unknown + 撤销语义、零首次写出」。
    let code = view.receipt.as_ref().and_then(|r| r.reason_code.as_deref());
    assert!(
        code == Some("manual_resume_revoked") || code == Some("manual_resume_during_dispatch"),
        "旧动作定格为撤销 Unknown（实际回执码 {code:?}）"
    );
    drop(sse);
}

/// F4 红轮②：兼容许可在 ensure 前捕获——排队期间关闭自动适配，写出边界不重读
/// 活门 → 兼容连接（caps.send=true）把尚未写出的普通消息发出。定序：
/// 未知 build fixture（兼容档已连接）→ 新消息入队（busy 冻结）→ 关闭开关 →
/// settle 放行 → 断言零帧。红轮：compat 值在 send_text 入口取过、写出时用旧值。
#[tokio::test]
async fn f4_auto_adapt_off_must_reread_live_gate_at_write_boundary() {
    let mut f = fixture_unknown_build(vec![codex_app_session(THREAD)]);
    // 活值门注入（与生产同款）
    let gate_store = f.state.store.clone();
    f.state
        .codex_control
        .adapter
        .set_auto_adapt_gate(Some(Arc::new(move || {
            gate_store.with(multi_agents_manager_lib::remote::auto_adapt_enabled_conn)
        })));
    let mut peer = f.peer.take().unwrap();

    // 直发（缺省开、未连接）：HTTP 静态门放行（Unverified+auto=true）→ 直发
    // 路径 ensure 建连挂在 initialize 帧上。
    let app = f.app.clone();
    let send_task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(send_body("F4 在途关闭", Some("act-f4-gate"), false)),
        ))
        .await
        .unwrap()
    });
    let init = recv_within(&mut peer, 15).await;
    assert_eq!(init["method"], "initialize");

    // —— 在途窗口：关闭自动适配——此后兼容档连接即使装上（caps.send=true），
    // 也不得再写出普通消息（旧实现：能力检查被兼容档 caps.send 绕过 → 帧照发）——
    f.state.store.with(|c| {
        c.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('remote.codex_auto_adapt', 'false')",
            [],
        )
        .unwrap();
    });

    // 放行握手（兼容档装上）→ send_text 继续 → 写出边界守卫重读活门 → 拒绝。
    peer.send(&serde_json::json!({
        "type": "response", "requestId": init["requestId"],
        "resultType": "success", "method": "initialize",
        "handledByClientId": "owner-1", "result": { "clientId": "owner-1" }
    }))
    .await;
    let disc = recv_within(&mut peer, 10).await;
    assert_eq!(disc["method"], "thread-owner-discovery");
    peer.send(&discovery_response(
        disc["requestId"].as_str().unwrap(),
        "owner-1",
    ))
    .await;
    let follow = recv_within(&mut peer, 10).await;
    assert_eq!(follow["method"], "thread-stream-following-changed");
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": THREAD, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": 7,
                "conversationState": {
                    "turns": [], "requests": [],
                    "threadRuntimeStatus": { "type": "idle" },
                    "turnHistory": codex_fixture::canonical_turn_history(serde_json::json!({
                            "t1": { "id": "t1", "type": "turn", "status": "completed", "items": ["u1"] },
                            "u1": { "id": "u1", "type": "user_message", "text": "seed", "ts": 1 }
                        }), serde_json::json!([{ "entries": ["t1"] }]))
                }
            }
        }
    }))
    .await;
    let body_str = match tokio::time::timeout(Duration::from_secs(15), send_task).await {
        Ok(v) => body_string(v.unwrap()).await,
        Err(_) => {
            // 超时=帧已写出（send 在等 start-turn 应答）——直接以帧数断言失败
            let frames = count_start_turn_frames(&mut peer).await;
            panic!("F4②：send 未在时限内返回（首帧已写出 frames={frames}）");
        }
    };
    let v: serde_json::Value = serde_json::from_str(&body_str).unwrap();
    // 拒绝形态：明确零投递（desktop_ipc not_sent capability_unverified）——
    // 绝不能 accepted/queued 后帧已写出。
    assert_eq!(v["status"], "desktop_ipc", "{v}");
    assert_eq!(v["receipt"]["delivery"], "not_sent", "{v}");
    assert_eq!(v["receipt"]["reasonCode"], "capability_unverified", "{v}");
    let frames = count_start_turn_frames(&mut peer).await;
    assert_eq!(
        frames, 0,
        "F4②：在途关闭后写出边界重读活门 → 零首帧（frames={frames}）"
    );
}
/// F4 红轮③：撤销标记读取失败（DROP TABLE codex_write_revoke → SELECT 必败）
/// → 写出边界不得按未撤销放行（保守拒绝、零帧）。定序与 F4① 同款：flush 任务
/// 先启动（上层检查点已过、帧尚未写）→ 注入读必败 → 放行 → 断言零首帧。
/// 红轮：unwrap_or(false) 把读失败当未撤销 → 帧照发。
#[tokio::test]
async fn f4_revoke_read_failure_must_not_pass_write_boundary() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let (sse, _) = sse_hold_ensure(&f, &mut peer, THREAD, vec![], "idle").await;

    // 旧消息 queue_only 入队（flush 将取队首派发——与 F4① 同款路径）
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(send_body("F4 读失败动作", Some("act-f4-rf"), true)),
        ))
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["status"], "codex_queued", "{v}");

    // 注入：撤销表读必败（上层检查与 writer 边界共用同一 SELECT——两侧都必须保守）
    f.conn
        .lock()
        .unwrap()
        .execute("DROP TABLE codex_write_revoke", [])
        .unwrap();

    // flush：撤销检查读失败 → 必须保守拒绝（红轮：unwrap_or(false) 放行 → 帧出）
    let hub = f.state.codex_control.clone();
    let key = f.thread_key();
    let flush_task = tokio::spawn(async move {
        let _ = hub.adapter.ensure_thread(&key).await;
        hub.router.flush_thread(&key).await
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let _ = tokio::time::timeout(Duration::from_millis(100), flush_task).await;
    let frames = count_start_turn_frames(&mut peer).await;
    assert_eq!(
        frames, 0,
        "F4③：撤销读取失败不得放行首帧（frames={frames}）"
    );
    // Gate C 留意项 a：回执直断言——动作定格 Unknown 且 reasonCode = 独立定格码
    // write_guard_read_failed（区别于 manual_resume_revoked——DB 读故障 ≠ 人工继续）
    let view = wait_action_state(
        &f,
        "act-f4-rf",
        ActionState::Unknown,
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(
        view.receipt.as_ref().and_then(|r| r.reason_code.as_deref()),
        Some("write_guard_read_failed"),
        "读失败定格码必须独立（write_guard_read_failed）"
    );
    drop(sse);
}

// ============================================================
// 终审 F5：本地策略更新不得被健康 SSE 的 supersedes 吞掉
// ============================================================

/// F5 测试账本：与生产 StoreProbeLedger 同 DAO 的最小实现（生产类型 pub(crate)）。
struct SharedConnLedger(SharedConn);
impl ProbeLedger for SharedConnLedger {
    fn get(&self, probe_key: &str, capability: &str) -> Option<ProbeLedgerEntry> {
        let conn = self.0.lock().unwrap();
        multi_agents_manager_lib::database::dao::codex_ipc_probe::get_conn(
            &conn, probe_key, capability,
        )
        .ok()
        .flatten()
        .map(|row| ProbeLedgerEntry {
            evidenced: row.has_evidence(),
            evidenced_at: row.evidence_at,
            last_seen_at: Some(row.last_seen_at),
            state: row.state,
            next_retry_at: row.next_retry_at,
            stage: row.probe_stage,
            error_code: row.error_code,
            fingerprint: row.fingerprint,
        })
    }
    fn record(
        &self,
        probe_key: &str,
        capability: &str,
        outcome: &ProbeOutcome,
        fingerprint: Option<&str>,
    ) {
        let now = chrono::Utc::now().timestamp_millis();
        let stage = Some(outcome.stage().as_str());
        let conn = self.0.lock().unwrap();
        let recorded = match outcome {
            ProbeOutcome::Probed { .. } => {
                multi_agents_manager_lib::database::dao::codex_ipc_probe::upsert_ok_conn(
                    &conn,
                    probe_key,
                    capability,
                    stage,
                    fingerprint,
                    Some(outcome.duration_ms()),
                    now,
                )
            }
            ProbeOutcome::Failed { error_code, .. } => {
                multi_agents_manager_lib::database::dao::codex_ipc_probe::upsert_fail_conn(
                    &conn,
                    probe_key,
                    capability,
                    stage,
                    error_code,
                    fingerprint,
                    Some(outcome.duration_ms()),
                    now,
                )
            }
            ProbeOutcome::Inconclusive { error_code, .. } => {
                multi_agents_manager_lib::database::dao::codex_ipc_probe::upsert_inconclusive_conn(
                    &conn,
                    probe_key,
                    capability,
                    stage,
                    error_code,
                    fingerprint,
                    Some(outcome.duration_ms()),
                    now,
                )
            }
        };
        let _ = recorded;
    }
    fn load_schema_evidence(
        &self,
        probe_key: &str,
    ) -> Option<multi_agents_manager_lib::services::codex_desktop_ipc::probe::PersistedSchemaEvidence>
    {
        let conn = self.0.lock().unwrap();
        multi_agents_manager_lib::database::dao::codex_ipc_probe::get_schema_evidence_conn(
            &conn, probe_key,
        )
        .ok()
        .flatten()
        .map(|row| {
            multi_agents_manager_lib::services::codex_desktop_ipc::probe::PersistedSchemaEvidence {
                fingerprint: row.fingerprint,
                command_decisions: row.command_decisions,
                file_decisions: row.file_decisions,
                evidenced_at: row.evidence_at,
            }
        })
    }
    fn store_schema_evidence(
        &self,
        probe_key: &str,
        evidence: &multi_agents_manager_lib::services::codex_desktop_ipc::probe::schema_probe::SchemaEvidence,
        at_ms: i64,
    ) {
        let conn = self.0.lock().unwrap();
        let _ =
            multi_agents_manager_lib::database::dao::codex_ipc_probe::upsert_schema_evidence_conn(
                &conn,
                probe_key,
                &evidence.fingerprint,
                &evidence.command_decisions,
                &evidence.file_decisions,
                at_ms,
            );
    }
}

/// 终审 F5 验收（真实 SSE 订阅贯通 HTTP → SSE 帧落地）：未知 build 的审批卡在
/// 首快照时 schema 未取证（不可操作）→ schema 决策词证据**晚到**（持久装回路径，
/// 与 CLI 探针恢复同构）→ 同一张卡（同 requestKey）在**同一 Desktop revision/
/// 代际**下自动出现决策按钮——Desktop 不再发任何其他帧（不靠后续状态帧补偿，
/// 纯本地策略更新的重发布必须穿透 supersedes）。
#[tokio::test]
async fn sse_local_policy_update_survives_same_revision_supersedes() {
    use multi_agents_manager_lib::services::codex_desktop_ipc::probe::schema_probe::SchemaProbeOutcome;

    // 未知 build adapter：连接期 spawn 的 schema 取证注入 Unknown（确定性），
    // 稍后走持久装回路径「晚到」。
    let (io, peer) = ScriptedPeer::new();
    let adapter = Arc::new(DesktopIpcAdapter::new(
        Arc::new(StaticConnector::with_build(io, UNKNOWN_BUILD)),
        ProtocolProfile::unverified("windows", UNKNOWN_BUILD),
    ));
    let mut f = fixture_with(
        vec![codex_app_session(THREAD)],
        |store| {
            // 共享库给账本用：fixture_with 只把 conn 交给 hub_builder 之外的
            // RemoteState——账本需要独立 DeviceStore（同库不同句柄即可）。
            let _ = &store;
            Arc::new(CodexControlHub::with_adapter(store, adapter.clone(), true))
        },
        Some(peer),
    );
    // 账本：直接落 fixture 的共享内存库（与生产 StoreProbeLedger 同 DAO；测试侧
    // 最小实现，因生产实现 pub(crate) 不外露）。
    let ledger_conn = f.conn.clone();
    f.state
        .codex_control
        .adapter
        .set_probe_ledger(Arc::new(SharedConnLedger(ledger_conn)));
    f.state
        .codex_control
        .adapter
        .set_probe_key_override(Some(COMPAT_PROBE_KEY.into()));
    f.state
        .codex_control
        .adapter
        .set_schema_probe_override(Some(SchemaProbeOutcome::Unknown {
            error_code: "cli_not_found".into(),
        }));
    // 读链证据预置（与 fixture_unknown_build 的 ProbedStateLedger 同构）：探测命中
    // 账本 → ensure_read_evidence 零帧短路，drive_ensure 的一轮帧全部留给
    // follow_thread（首快照带 pending 卡）。
    f.conn
        .lock()
        .unwrap()
        .execute_batch(&format!(
            "INSERT OR REPLACE INTO codex_ipc_probe
                (probe_key, capability, state, probe_stage, attempts,
                 first_seen_at, last_seen_at, evidence_at, evidence_stage)
             VALUES ('{COMPAT_PROBE_KEY}', 'state', 'probed', 'P3', 1, 1, 1, 1, 'P3')",
        ))
        .unwrap();

    // —— 订阅先建立：SSE handler 自己 ensure_thread（live_projection），由本测试
    // 驱动假 Desktop 的握手/发现/跟随/首快照（含 command 审批 pending；schema 未
    // 取证 → 卡不可操作）。不在订阅前预驱动（首快照必须来自本订阅的 ensure）。
    let app = f.app.clone();
    let sse_task = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread/events?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    let mut peer = f.peer.take().unwrap();
    // 等待 handler 的 ensure 发起（initialize 帧到达）再喂后续帧——与 fixture 的
    // 帧序脚本一致，但由 handler 自己消费。
    drive_ensure(
        &mut peer,
        THREAD,
        vec![serde_json::json!({
            "id": 41, "method": "item/commandExecution/requestApproval",
            "params": { "command": "pnpm test", "cwd": "/w",
                        "availableDecisions": ["accept", "cancel"] }
        })],
        "active",
    )
    .await;
    let r = tokio::time::timeout(Duration::from_secs(15), sse_task)
        .await
        .expect("sse in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    let mut body = r.into_body();
    use http_body_util::BodyExt;
    let frame = tokio::time::timeout(Duration::from_secs(10), body.frame())
        .await
        .expect("first frame in time")
        .unwrap()
        .unwrap();
    let text = String::from_utf8(frame.data_ref().unwrap().to_vec()).unwrap();
    let first: serde_json::Value =
        serde_json::from_str(text.trim_start_matches("event: snapshot\ndata:").trim())
            .unwrap_or_else(|_| {
                // 兼容 axum sse 的分块格式（data: 行独立）
                serde_json::from_str(text.split("data:").nth(1).map(str::trim).unwrap_or("null"))
                    .expect("首帧必须是 ThreadProjection JSON")
            });
    // ① 验收：首快照禁用（schema 未取证 → 卡不可操作）
    let pending0 = &first["pending"][0];
    assert_eq!(pending0["requestKey"], "n:41", "{first}");
    assert_eq!(
        pending0["actionable"],
        serde_json::json!(false),
        "首快照：schema 未到，卡禁用"
    );

    // —— schema 证据「晚到」：持久决策词证据落库 + 恢复路径装回（CLI 探针已
    //    在连接期跑过并落 Unknown——装回先于单飞检查，仍可恢复）——
    f.conn.lock().unwrap().execute_batch(&format!(
        "INSERT OR REPLACE INTO codex_ipc_schema_evidence
            (probe_key, fingerprint, command_decisions, file_decisions, evidence_at)
         VALUES ('{COMPAT_PROBE_KEY}', 'fp-f5', '[\"accept\",\"cancel\"]', '[\"accept\",\"cancel\"]', 1)",
    )).unwrap();
    f.state.codex_control.adapter.ensure_schema_evidence().await;
    {
        use multi_agents_manager_lib::services::codex_desktop_ipc::pending::DecisionPolicy;
        let policy: DecisionPolicy = f.state.codex_control.adapter.approval_policy();
        assert!(!policy.is_empty(), "恢复后审批档必须非空（装回成功）");
    }

    // ②+③ 验收：schema 晚到后同卡自动出按钮；Desktop 不再发任何帧（peer 静默），
    // 纯本地策略更新必须穿透 SSE 的 supersedes（同 revision 竞争）。
    //
    // 流上可能先出现一帧更早的本地重发布（ensure_thread 内读链证据确立的
    // republish——schema 尚未到，卡仍禁用，属合法中间态）；验收点是「无需任何
    // Desktop 帧、同卡最终出现按钮」——有界读帧直到 actionable 的同卡出现。
    let mut actionable_frame: Option<serde_json::Value> = None;
    for _ in 0..6 {
        let updated = tokio::time::timeout(Duration::from_secs(10), body.frame())
            .await
            .expect("本地策略更新必须到达 SSE 下游（不被 supersedes 吞掉）")
            .unwrap()
            .unwrap();
        let text2 = String::from_utf8(updated.data_ref().unwrap().to_vec()).unwrap();
        let frame: serde_json::Value =
            serde_json::from_str(text2.split("data:").nth(1).map(str::trim).unwrap_or("null"))
                .expect("更新帧必须是 ThreadProjection JSON");
        assert_eq!(frame["threadId"], serde_json::json!(THREAD), "{frame}");
        let pending1 = &frame["pending"][0];
        assert_eq!(
            pending1["requestKey"], "n:41",
            "同卡原地更新，不换卡：{frame}"
        );
        // Desktop revision 未被伪造（本地更新不动 Desktop 侧标识）
        assert_eq!(
            frame["revision"], first["revision"],
            "不伪造 revision：{frame}"
        );
        if pending1["actionable"] == serde_json::json!(true) {
            assert_eq!(
                pending1["allowedDecisions"],
                serde_json::json!(["accept", "cancel"]),
                "{frame}"
            );
            actionable_frame = Some(frame);
            break;
        }
        // 中间态帧（更早的本地重发布）：合法，继续读下一帧
    }
    let _ = actionable_frame.expect("schema 晚到后同卡必须自动出按钮（有界帧数内）");
    // peer 静默：此后短窗内 Desktop 无任何新帧（不靠后续帧补偿）
    assert!(
        !peer_has_traffic(&mut peer).await,
        "验收③：Desktop 不再发其他帧也成立"
    );
}

// ============================================================
// 终审 F9：关闭态建连后重开——活连接上按需恢复 schema 取证
// ============================================================

/// F9 红测①（终审给定顺序）：关闭自动适配 → 建立活连接（未知 build；连接期的
/// schema 取证被活值门拦下）→ 当前 pending 审批卡在场且不可操作 → **开启**
/// （写设置 + toggle 命令链的适配器通知缝）→ 当前 pending 按需取证并**原地
/// 更新**（同卡出按钮）——全程零断开/重启、Desktop 无新帧。红轮：通知缝为
/// no-op 桩 → 卡保持锁定（轮询超时）。
#[tokio::test]
async fn f9_auto_adapt_reenable_recovers_schema_on_live_connection() {
    use multi_agents_manager_lib::services::codex_desktop_ipc::probe::schema_probe::SchemaEvidence;
    use multi_agents_manager_lib::services::codex_desktop_ipc::probe::schema_probe::SchemaProbeOutcome;

    let (io, peer) = ScriptedPeer::new();
    let adapter = Arc::new(DesktopIpcAdapter::new(
        Arc::new(StaticConnector::with_build(io, UNKNOWN_BUILD)),
        ProtocolProfile::unverified("windows", UNKNOWN_BUILD),
    ));
    let mut f = fixture_with(
        vec![codex_app_session(THREAD)],
        |store| Arc::new(CodexControlHub::with_adapter(store, adapter.clone(), true)),
        Some(peer),
    );
    // 账本 + 固定键 + Known 注入（kick 后「取证」确定性返回，绝不跑真实 CLI）
    let ledger_conn = f.conn.clone();
    f.state
        .codex_control
        .adapter
        .set_probe_ledger(Arc::new(SharedConnLedger(ledger_conn)));
    f.state
        .codex_control
        .adapter
        .set_probe_key_override(Some(COMPAT_PROBE_KEY.into()));
    f.state
        .codex_control
        .adapter
        .set_schema_probe_override(Some(SchemaProbeOutcome::Known(SchemaEvidence {
            fingerprint: "fp-f9".into(),
            command_decisions: vec!["accept".into(), "cancel".into()],
            file_decisions: vec!["accept".into(), "cancel".into()],
        })));
    // 活值门（与生产同款）：读共享库设置
    let gate_store = f.state.store.clone();
    f.state
        .codex_control
        .adapter
        .set_auto_adapt_gate(Some(Arc::new(move || {
            gate_store.with(multi_agents_manager_lib::remote::auto_adapt_enabled_conn)
        })));
    // —— 关闭态先行：连接建立即处于「关闭」——
    f.state.store.with(|c| {
        c.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('remote.codex_auto_adapt', 'false')",
            [],
        )
        .unwrap();
    });

    // 活连接 + 当前 pending 审批卡（SSE 持引用；首快照卡不可操作——schema 未取证）
    let mut peer = f.peer.take().unwrap();
    let (mut body, first) = sse_hold_ensure(
        &f,
        &mut peer,
        THREAD,
        vec![serde_json::json!({
            "id": 51, "method": "item/commandExecution/requestApproval",
            "params": { "command": "pnpm test", "cwd": "/w",
                        "availableDecisions": ["accept", "cancel"] }
        })],
        "active",
    )
    .await;
    {
        // R5 后首快照以 projection_seq 盖章，启动缓冲的冗余重复帧不再补发——
        // 初始「禁用」断言直接落在首快照本体（权威且必达）。
        let pending = &first["pending"][0];
        assert_eq!(pending["requestKey"], "n:51", "{first}");
        assert_eq!(
            pending["actionable"],
            serde_json::json!(false),
            "关闭态建连：schema 未取证，卡不可操作"
        );
    }

    // —— 开启：写设置（活值门即刻翻 true）+ toggle 命令链的适配器通知 ——
    f.state.store.with(|c| {
        c.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('remote.codex_auto_adapt', 'true')",
            [],
        )
        .unwrap();
    });
    f.state
        .codex_control
        .adapter
        .notify_auto_adapt_changed(true);

    // 当前 pending 原地更新：有界读 SSE 帧直到同卡 actionable（零断开——peer 无
    // 新握手帧； Desktop 无任何新帧）
    use http_body_util::BodyExt;
    let mut actionable = false;
    for _ in 0..8 {
        let updated = tokio::time::timeout(Duration::from_secs(10), body.frame())
            .await
            .expect("开启后的本地策略更新必须到达 SSE 下游（不需要断开/重启）")
            .unwrap()
            .unwrap();
        let text = String::from_utf8(updated.data_ref().unwrap().to_vec()).unwrap();
        let frame: serde_json::Value =
            serde_json::from_str(text.split("data:").nth(1).map(str::trim).unwrap_or("null"))
                .expect("更新帧必须是 ThreadProjection JSON");
        let pending = &frame["pending"][0];
        assert_eq!(
            pending["requestKey"], "n:51",
            "同卡原地更新，不换卡：{frame}"
        );
        if pending["actionable"] == serde_json::json!(true) {
            assert_eq!(
                pending["allowedDecisions"],
                serde_json::json!(["accept", "cancel"]),
                "{frame}"
            );
            actionable = true;
            break;
        }
    }
    assert!(
        actionable,
        "F9：开启后当前 pending 必须原地出按钮（活连接，零断开）"
    );
    // Desktop 侧零新帧（不靠后续状态帧补偿；无重连握手）
    assert!(
        !peer_has_traffic(&mut peer).await,
        "F9：恢复不要求断开——Desktop 无任何新帧"
    );
    drop(body);
}

/// F9 正例②（防过度修复）：已有持久成功证据 → 开启 + 通知 → **零重探**
/// （持久装回路径命中，capability=schema 的探测行不新增），当前 pending 仍原地
/// 出按钮。
#[tokio::test]
async fn f9_reenable_with_existing_evidence_zero_reprobe() {
    let (io, peer) = ScriptedPeer::new();
    let adapter = Arc::new(DesktopIpcAdapter::new(
        Arc::new(StaticConnector::with_build(io, UNKNOWN_BUILD)),
        ProtocolProfile::unverified("windows", UNKNOWN_BUILD),
    ));
    let mut f = fixture_with(
        vec![codex_app_session(THREAD)],
        |store| Arc::new(CodexControlHub::with_adapter(store, adapter.clone(), true)),
        Some(peer),
    );
    let ledger_conn = f.conn.clone();
    f.state
        .codex_control
        .adapter
        .set_probe_ledger(Arc::new(SharedConnLedger(ledger_conn)));
    f.state
        .codex_control
        .adapter
        .set_probe_key_override(Some(COMPAT_PROBE_KEY.into()));
    // 不注入 probe override：若意外跑了 CLI 路径会真 spawn（且记账）——用
    // 「schema 行不新增」断言零重探（正例的判据）。
    let gate_store = f.state.store.clone();
    f.state
        .codex_control
        .adapter
        .set_auto_adapt_gate(Some(Arc::new(move || {
            gate_store.with(multi_agents_manager_lib::remote::auto_adapt_enabled_conn)
        })));
    // 关闭态 + 预置持久成功证据（重启复用同款）
    f.state.store.with(|c| {
        c.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('remote.codex_auto_adapt', 'false')",
            [],
        )
        .unwrap();
    });
    f.conn.lock().unwrap().execute_batch(&format!(
        "INSERT OR REPLACE INTO codex_ipc_schema_evidence
            (probe_key, fingerprint, command_decisions, file_decisions, evidence_at)
         VALUES ('{COMPAT_PROBE_KEY}', 'fp-f9b', '[\"accept\",\"cancel\"]', '[\"accept\",\"cancel\"]', 1)",
    )).unwrap();

    // 活连接 + pending 卡（不可操作——关闭态建连，内存槽无证据）
    let mut peer = f.peer.take().unwrap();
    let (mut body, _) = sse_hold_ensure(
        &f,
        &mut peer,
        THREAD,
        vec![serde_json::json!({
            "id": 52, "method": "item/commandExecution/requestApproval",
            "params": { "command": "pnpm build", "cwd": "/w",
                        "availableDecisions": ["accept", "cancel"] }
        })],
        "active",
    )
    .await;

    // 开启 + 通知
    f.state.store.with(|c| {
        c.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('remote.codex_auto_adapt', 'true')",
            [],
        )
        .unwrap();
    });
    f.state
        .codex_control
        .adapter
        .notify_auto_adapt_changed(true);

    // 当前 pending 原地出按钮（持久装回路径）
    use http_body_util::BodyExt;
    let mut actionable = false;
    for _ in 0..8 {
        let updated = tokio::time::timeout(Duration::from_secs(10), body.frame())
            .await
            .expect("装回后的本地策略更新必须到达")
            .unwrap()
            .unwrap();
        let text = String::from_utf8(updated.data_ref().unwrap().to_vec()).unwrap();
        let frame: serde_json::Value =
            serde_json::from_str(text.split("data:").nth(1).map(str::trim).unwrap_or("null"))
                .expect("更新帧必须是 ThreadProjection JSON");
        let pending = &frame["pending"][0];
        assert_eq!(pending["requestKey"], "n:52", "{frame}");
        if pending["actionable"] == serde_json::json!(true) {
            actionable = true;
            break;
        }
    }
    assert!(actionable, "F9②：已有证据 → 开启后装回并原地出按钮");
    // 零重探：capability=schema 的探测行不新增（CLI 探针从未跑——只有探针结论
    // 才会记账；装回路径不记账）
    let schema_rows: i64 = {
        let conn = f.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM codex_ipc_probe WHERE capability = 'schema'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(
        schema_rows, 0,
        "F9②：已有成功证据 → 零重探（schema 行不新增）"
    );
    assert!(
        !peer_has_traffic(&mut peer).await,
        "零断开：Desktop 无任何新帧"
    );
    drop(body);
}

// ============================================================
// 终审 F10：迟到回执把「当前」Turn 集合误当「派发前」集合
// ============================================================

/// 【红测 F10-a】新 Turn **先广播**（正式 userMessage 延后 ~0.5s，P1 取证的常态
/// 时序）→ 迟到 success 回执到达时该 Turn 已在当前镜像但无本动作身份 → 现实现
/// 用 `pre_write_snapshot`（**当前**镜像）比对即判 `stale_turn_echo`，跳过 2s
/// 身份等待并清 turnId，槽已消费 → 正式消息随后到达也无法收敛（有效结果被误留
/// Unknown）。正确契约：比对**派发前**快照——新 Turn 不在其中 → 走有界身份等待，
/// 正式回显到达 → Accepted + 绑 Turn。
#[tokio::test]
async fn late_result_new_turn_broadcast_first_converges_via_delayed_identity() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    f.state.codex_control.adapter.set_conn_tuning(S3ConnTuning {
        rpc_timeout: Duration::from_millis(300),
        ..Default::default()
    });
    f.state.codex_control.set_wrap_up_ms(8_000);
    f.state.codex_control.set_front_budget_ms(400);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let body = send_body("新回合先广播", Some("act-f10a"), false);
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    // 种子镜像：空 turns（派发前不存在任何 Turn——迟到回执的新 Turn 不在派发前集合）
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let frame = recv_within(&mut peer, 15).await;
    assert_eq!(frame["method"], "thread-follower-start-turn");
    let r = tokio::time::timeout(Duration::from_secs(6), task)
        .await
        .expect("前台预算必须有界结束")
        .unwrap();
    assert_eq!(r.status(), 200);
    // 时序 ①：新 Turn 先广播（status 帧，无正式 userMessage）
    tokio::time::sleep(Duration::from_millis(250)).await;
    let cmid = frame["params"]["turnStart"]["request"]["clientUserMessageId"]
        .as_str()
        .expect("start-turn 带 cmid")
        .to_string();
    peer.send(&snapshot_frame(
        THREAD,
        "owner-1",
        8,
        "active",
        serde_json::json!({
            "t-new": { "id": "t-new", "type": "turn", "status": "inProgress", "items": [] }
        }),
        serde_json::json!(["t-new"]),
    ))
    .await;
    // 时序 ②：迟到 success 回执（turnId = t-new；镜像已含 t-new 但无 cmid 身份）
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "turn": { "id": "t-new", "status": "inProgress" } }
    }))
    .await;
    // 时序 ③：正式 userMessage 延后到达（P1 实测 +0.5s 量级；置入身份）
    tokio::time::sleep(Duration::from_millis(300)).await;
    peer.send(&snapshot_frame(
        THREAD,
        "owner-1",
        9,
        "active",
        serde_json::json!({
            "t-new": { "id": "t-new", "type": "turn", "status": "inProgress",
                       "items": ["u-f10a"] },
            "u-f10a": { "id": "u-f10a", "type": "user_message", "clientId": cmid,
                        "text": "新回合先广播", "ts": 2 }
        }),
        serde_json::json!(["t-new"]),
    ))
    .await;
    // 正确收敛：Accepted + 绑 t-new + late_result 溯源（红 = stale_turn_echo 误判）
    let view = wait_action_state(
        &f,
        "act-f10a",
        ActionState::Accepted,
        Duration::from_secs(10),
    )
    .await;
    let receipt = view.receipt.expect("receipt");
    assert_eq!(
        receipt.reason_code.as_deref(),
        Some("late_result"),
        "有效迟到回执应收敛为 Accepted（红 = 误判 stale_turn_echo）: {receipt:?}"
    );
    assert_eq!(receipt.turn_id.as_deref(), Some("t-new"), "{receipt:?}");
    assert_eq!(count_start_turn_frames(&mut peer).await, 0, "零重发");
    peer.close();
}

/// 【对照正例 F10-b】真实旧 Turn 回显（Turn **派发前已存在**）仍必须拒绝——
/// F10 修复（比对派发前快照）不得稀释 Gate B `late_busy_echo_never_binds_old_
/// turn_or_releases_early` 的语义。种子镜像带终态 t1，迟到回执回显 t1 →
/// stale_turn_echo 保守 Unknown、不绑 Turn、屏障保留。
#[tokio::test]
async fn late_result_pre_dispatch_old_turn_echo_still_rejected() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    f.state.codex_control.adapter.set_conn_tuning(S3ConnTuning {
        rpc_timeout: Duration::from_millis(300),
        ..Default::default()
    });
    f.state.codex_control.set_wrap_up_ms(2_000);
    f.state.codex_control.set_front_budget_ms(400);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let body = send_body("旧回显仍拒绝", Some("act-f10b"), false);
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    // 种子镜像：t1 在场（派发前已存在——与 F10-a 的关键差异）
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let frame = recv_within(&mut peer, 15).await;
    assert_eq!(frame["method"], "thread-follower-start-turn");
    let r = tokio::time::timeout(Duration::from_secs(6), task)
        .await
        .expect("前台预算必须有界结束")
        .unwrap();
    assert_eq!(r.status(), 200);
    tokio::time::sleep(Duration::from_millis(250)).await;
    // 迟到回执回显派发前已存在的 t1（无本动作身份，也永不会有）
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": { "turn": { "id": "t1", "status": "inProgress" } }
    }))
    .await;
    let view =
        wait_action_state(&f, "act-f10b", ActionState::Unknown, Duration::from_secs(8)).await;
    let receipt = view.receipt.expect("receipt");
    assert_eq!(
        receipt.reason_code.as_deref(),
        Some("stale_turn_echo"),
        "真实旧 Turn 回显必须仍被拒绝（对照正例，语义不稀释）: {receipt:?}"
    );
    assert_eq!(receipt.turn_id.as_deref(), None);
    assert!(
        f.hub_store().has_compat_fence(&f.thread_key()).unwrap(),
        "保守屏障保留"
    );
    assert_eq!(count_start_turn_frames(&mut peer).await, 0, "零重发");
    peer.close();
}

// ============================================================
// 终审 R3：重复打断失败清除上次打断的暂停（归属 token 前的红测）
// ============================================================

/// R3 红测：A 打断受理并留下暂停（token A）→ B 携显式 turnId 再次打断、桌面明确
/// 拒绝（Rejected）→ B 的撤回臂**不得清除 A 的暂停**（红轮：无条件按 thread 清行
/// → 暂停消失 → 旧队列失保护）。修后：暂停行带归属 token，撤回只清本次拥有的行。
#[tokio::test]
async fn r3_failed_second_interrupt_must_not_clear_first_interrupt_pause() {
    let mut f = fixture_ipc_interrupt(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let (sse, _) = sse_hold_ensure(&f, &mut peer, THREAD, vec![], "active").await;

    // A：打断受理（暂停建立，token A）
    let body = serde_json::json!({ "sessionId": THREAD, "turnId": "t-live" });
    let app = f.app.clone();
    let task_a = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/turn/interrupt",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
    });
    let frame_a = recv_within(&mut peer, 10).await;
    assert_eq!(frame_a["method"], "thread-follower-interrupt-turn");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame_a["requestId"],
        "resultType": "success", "method": "thread-follower-interrupt-turn",
        "result": { "interruptedTurnId": "t-live", "ok": true }
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(15), task_a)
        .await
        .expect("A in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["queuePaused"], serde_json::json!(true), "{v}");

    // B：再次打断（显式 turnId 过安全阀）→ 桌面明确拒绝（Rejected）
    let body = serde_json::json!({ "sessionId": THREAD, "turnId": "t-second" });
    let app = f.app.clone();
    let task_b = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/turn/interrupt",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
    });
    let frame_b = recv_within(&mut peer, 10).await;
    assert_eq!(frame_b["method"], "thread-follower-interrupt-turn");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame_b["requestId"],
        "resultType": "error", "method": "thread-follower-interrupt-turn",
        "error": { "message": "busy" }
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(15), task_b)
        .await
        .expect("B in time")
        .unwrap();
    assert_eq!(
        r.status(),
        409,
        "B 被桌面明确拒绝: {}",
        body_string(r).await
    );

    // 暂停仍应在（红轮：B 的撤回臂清掉了 A 的暂停）
    let paused: i64 = {
        let conn = f.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM codex_queue_pause WHERE thread_id = ?1",
            [THREAD],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(
        paused, 1,
        "R3：B 零投递不得清除 A 打断建立的暂停（paused={paused}）"
    );
    // 队列视图如实：仍暂停
    let r = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/queue?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    let qv: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(qv["queuePaused"], serde_json::json!(true), "{qv}");
    drop(sse);
}

/// R2 红测（终审给定定序）：guard **Allow 之后**帧入队、writer 停在写右判定门
/// （「guard 之后、writer 首次尝试之前」的受控窗口——write_right_gate 测试缝；
/// 复审 R-T1 起该缝位于最终判定 Allow **之后、claim 之前**，窗口内提交的撤销
/// 由 claim CAS 裁决）→
/// 窗口内人工继续事务提交撤销并建立后继 → 放行 writer → 旧动作**零首次写出**
/// （定格 manual_resume_revoked）。红轮（writer 无写右复核）：start-turn 帧在
/// 人工继续前即到达 peer（写入已发生）。
#[tokio::test]
async fn r2_revoke_committed_after_guard_allow_must_block_first_write() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    // 活连接（SSE 持引用；ensure 已完成——flush 将走快路径直入派发）
    let (sse, _) = sse_hold_ensure(&f, &mut peer, THREAD, vec![], "idle").await;

    // 旧消息 queue_only 入队
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(send_body("R2 旧动作", Some("act-r2-old"), true)),
        ))
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["status"], "codex_queued", "{v}");

    // writer 定序门：停在最终判定 Allow 之后、claim 之前（复审 R-T1 搬位）
    let gate = Arc::new(tokio::sync::Notify::new());
    f.state
        .codex_control
        .adapter
        .set_write_right_gate_for_tests(Some(gate.clone()))
        .await;

    // flush 任务：reserve → send_text → dispatch（守卫 Allow）→ conn.request →
    // writer 拾取 → 停在写右判定门
    let hub = f.state.codex_control.clone();
    let key = f.thread_key();
    let flush_task = tokio::spawn(async move {
        let _ = hub.adapter.ensure_thread(&key).await;
        hub.router.flush_thread(&key).await
    });
    // 轮询到达标志（write_right_gate_reached）——有界 5s
    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !f
            .state
            .codex_control
            .adapter
            .write_right_gate_reached_for_tests()
            .await
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "writer 未在时限内到达写右判定门"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    // —— 窗口内：人工继续事务提交撤销并建立后继 ——
    f.hub_store()
        .reserve("act-r2-old", ControlRoute::DesktopIpc)
        .expect("reserve");
    let r = action_manual_resume(
        &f,
        serde_json::json!({
            "actionId": "act-r2-old", "newActionId": "act-r2-new",
            "text": "R2 旧动作", "reason": "R2 撤销窗口"
        }),
    )
    .await;
    assert_eq!(r.status(), 200, "人工继续成功（撤销已持久）");

    // 放行 writer：写右判定读撤销 → 零首帧
    f.state
        .codex_control
        .adapter
        .set_write_right_gate_for_tests(None)
        .await;
    gate.notify_waiters();
    let _flushed = tokio::time::timeout(Duration::from_secs(15), flush_task)
        .await
        .expect("flush in time")
        .expect("flush ok");
    let frames = count_start_turn_frames(&mut peer).await;
    assert_eq!(
        frames, 0,
        "R2：guard Allow 后撤销提交，writer 首次尝试前必须拦截（frames={frames}）"
    );
    let view = wait_action_state(
        &f,
        "act-r2-old",
        ActionState::Unknown,
        Duration::from_secs(5),
    )
    .await;
    let code = view.receipt.as_ref().and_then(|r| r.reason_code.as_deref());
    assert!(
        code == Some("manual_resume_revoked") || code == Some("manual_resume_during_dispatch"),
        "旧动作定格撤销 Unknown（实际 {code:?}）"
    );
    drop(sse);
}

// ============================================================
// 终审 R3：重复打断失败清除上次打断的暂停（归属 token 前的红测）
// ============================================================

/// R2 验收补例（「关闭也覆盖排队期间」，Gate E #3 门定序版）：兼容档活连接在场 →
/// 消息排队（busy 冻结）→ settle 放行 → **writer 停在写右判定门（guard 已 Allow、
/// 帧已入队、写前）** → 窗口内关闭自动适配 → 放行 → 写右判定（compat_licensed=true、
/// 活门收回）→ 零首帧、NotSent `capability_unverified`。证明关闭窗口覆盖到
/// guard 与 writer 尝试之间（不止既有 guard）。
#[tokio::test]
async fn r2_close_during_queue_blocked_at_write_right() {
    let mut f = fixture_unknown_build(vec![codex_app_session(THREAD)]);
    let gate_store = f.state.store.clone();
    f.state
        .codex_control
        .adapter
        .set_auto_adapt_gate(Some(Arc::new(move || {
            gate_store.with(multi_agents_manager_lib::remote::auto_adapt_enabled_conn)
        })));
    let mut peer = f.peer.take().unwrap();
    let (sse, _) = sse_hold_ensure(&f, &mut peer, THREAD, vec![], "active").await;

    // 排队（busy 冻结；兼容档在场）
    {
        let app = f.app.clone();
        let task = tokio::spawn(async move {
            app.oneshot(req(
                "POST",
                "/m/api/v1/session-send",
                Some(COOKIE),
                Some(send_body("R2 排队期间关闭", Some("act-r2c"), false)),
            ))
            .await
            .unwrap()
        });
        let r = tokio::time::timeout(Duration::from_secs(15), task)
            .await
            .expect("queue in time")
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["status"], "codex_queued", "{v}");
    }

    // writer 定序门：帧停在最终判定 Allow 之后、claim 之前（guard 已 Allow；复审 R-T1 搬位）
    let gate = Arc::new(tokio::sync::Notify::new());
    f.state
        .codex_control
        .adapter
        .set_write_right_gate_for_tests(Some(gate.clone()))
        .await;

    // settle 放行（idle）：帧入队、writer 停在门上
    push_snapshot(&mut peer, THREAD, 9, "idle", vec![]).await;
    wait_snapshot_applied(&f, 9, true).await;
    {
        let hub = f.state.codex_control.clone();
        let key = f.thread_key();
        let settle = tokio::spawn(async move {
            let _ = hub.adapter.ensure_thread(&key).await;
            let out = hub.router.flush_thread(&key).await;
            out
        });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !f
            .state
            .codex_control
            .adapter
            .write_right_gate_reached_for_tests()
            .await
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "writer 未在时限内到达写右判定门"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // settle 句柄移交后台：writer 在门上、flush 未返回；放行后自然收尾
        tokio::spawn(async move {
            let _ = settle.await;
        });
    }

    // —— 窗口内：关闭自动适配（guard 已 Allow；写右判定未跑）——
    f.state.store.with(|c| {
        c.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('remote.codex_auto_adapt', 'false')",
            [],
        )
        .unwrap();
    });

    // 放行 writer：compat_licensed=true + 活门收回 → 拒绝
    f.state
        .codex_control
        .adapter
        .set_write_right_gate_for_tests(None)
        .await;
    gate.notify_waiters();

    // 队首动作明确零投递终态（NotSent capability_unverified）
    let view = wait_action_state(
        &f,
        "act-r2c",
        ActionState::Rejected,
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(
        view.receipt.as_ref().and_then(|r| r.reason_code.as_deref()),
        Some("capability_unverified"),
        "{view:?}"
    );
    assert_eq!(
        count_start_turn_frames(&mut peer).await,
        0,
        "R2：guard 与 writer 之间的关闭 → 零首帧"
    );
    drop(sse);
}

// ============================================================
// 复审 R-T1：最终判定 Allow ≠ 首次写出权——claim 两态 CAS 线性化点
// ============================================================

/// R-T1 主红绿例（多线程 runtime + 真实人工继续事务）：最终判定已读 Allow（只读
/// SELECT 释放 DB 锁）、帧未写——writer 停在**判定之后、claim 之前**的受控窗口
/// （R-T1 把 write_right_gate 自判定之前搬到此处），窗口内并行提交真实人工继续
/// （撤销 + 后继同一事务）→ 放行 → claim CAS 与撤销竞争同一持久行：撤销先提交
/// → claim 0 行 → **零首次写出**，旧动作定格撤销 Unknown。
/// 红轮（claim 短路变异 = 复审时点旧行为：判定 Allow 后无原子取得写出权）：
/// start-turn 帧写出（首帧 > 0）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t1_revoke_committed_after_final_check_allow_must_not_first_write() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let (sse, _) = sse_hold_ensure(&f, &mut peer, THREAD, vec![], "idle").await;

    // 旧消息 queue_only 入队
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(send_body("T1 旧动作", Some("act-t1-old"), true)),
        ))
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["status"], "codex_queued", "{v}");

    // writer 定序门（R-T1 搬位）：停在「最终判定 Allow 之后、claim 之前」
    let gate = Arc::new(tokio::sync::Notify::new());
    f.state
        .codex_control
        .adapter
        .set_write_right_gate_for_tests(Some(gate.clone()))
        .await;
    // 红轮有界化（变异下首帧写出 → RPC 0.25s 超时 + 收尾 3s 即结算，flush 返回
    // 后 frames 断言以「首帧写出」红；绿轮走 claim 拒绝臂不经此路径，零影响）
    f.state.codex_control.adapter.set_conn_tuning(S3ConnTuning {
        rpc_timeout: Duration::from_millis(250),
        ..Default::default()
    });
    f.state.codex_control.set_wrap_up_ms(3_000);

    // flush 任务：reserve → dispatch（守卫 Allow）→ writer 拾取 → 最终判定 Allow →
    // 停在 claim 前的门上
    let hub = f.state.codex_control.clone();
    let key = f.thread_key();
    let flush_task = tokio::spawn(async move {
        let _ = hub.adapter.ensure_thread(&key).await;
        hub.router.flush_thread(&key).await
    });
    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !f
            .state
            .codex_control
            .adapter
            .write_right_gate_reached_for_tests()
            .await
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "writer 未在时限内到达 claim 前定序门"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    // —— 窗口内（最终判定已 Allow、写出权尚未取得）：真实人工继续事务提交 ——
    // （多线程 runtime：writer 任务与 HTTP 处理并行；SQLite 写串行化保证撤销与
    // claim 恰好一方胜出——本例撤销先提交。）
    f.hub_store()
        .reserve("act-t1-old", ControlRoute::DesktopIpc)
        .expect("reserve");
    let r = action_manual_resume(
        &f,
        serde_json::json!({
            "actionId": "act-t1-old", "newActionId": "act-t1-new",
            "text": "T1 旧动作", "reason": "T1 撤销竞争窗"
        }),
    )
    .await;
    assert_eq!(r.status(), 200, "人工继续成功（撤销已持久，撤销胜出）");

    // 放行 writer：claim CAS 读到撤销 → 零首帧（红轮 40s 窗口：变异下 RPC 15s
    // 超时收尾后 flush 才返回，随后 frames 断言以「首帧写出」红）
    f.state
        .codex_control
        .adapter
        .set_write_right_gate_for_tests(None)
        .await;
    gate.notify_waiters();
    let _flushed = tokio::time::timeout(Duration::from_secs(40), flush_task)
        .await
        .expect("flush in time")
        .expect("flush ok");
    let frames = count_start_turn_frames(&mut peer).await;
    assert_eq!(
        frames, 0,
        "T1：最终 Allow 后撤销提交，claim 必须拦下首次写出（frames={frames}）"
    );
    let view = wait_action_state(
        &f,
        "act-t1-old",
        ActionState::Unknown,
        Duration::from_secs(5),
    )
    .await;
    let code = view.receipt.as_ref().and_then(|r| r.reason_code.as_deref());
    assert!(
        code == Some("manual_resume_revoked") || code == Some("manual_resume_during_dispatch"),
        "旧动作定格撤销 Unknown（实际 {code:?}）"
    );
    drop(sse);
}

/// R-T1 对照例：claim **先于**撤销取得（帧已写出、RPC 回执未到）→ 人工继续的
/// 撤销 CAS 0 行。复审 round 4（V2）语义改写：claim 只代表「派发**在途**」（收尾
/// 落 Unknown 时 claim 已演进清除）——此刻动作仍 reserved 在飞，人工继续返回
/// **独立状态 `write_right_in_flight`**（不是 converged——converged 只能表达真实
/// 终态）：不建后继、旧动作定格不做（保持 reserved 等真实回执/迟到证据收尾；
/// 收尾后的人工继续不受 claim 阻挡，见 v2_unknown_after_timeout 主例）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t1_claim_won_manual_resume_creates_no_successor() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let (sse, _) = sse_hold_ensure(&f, &mut peer, THREAD, vec![], "idle").await;

    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(send_body("T1 已越点", Some("act-t1c-old"), true)),
        ))
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["status"], "codex_queued", "{v}");

    let gate = Arc::new(tokio::sync::Notify::new());
    f.state
        .codex_control
        .adapter
        .set_write_right_gate_for_tests(Some(gate.clone()))
        .await;
    let hub = f.state.codex_control.clone();
    let key = f.thread_key();
    let _flush_task = tokio::spawn(async move {
        let _ = hub.adapter.ensure_thread(&key).await;
        hub.router.flush_thread(&key).await
    });
    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !f
            .state
            .codex_control
            .adapter
            .write_right_gate_reached_for_tests()
            .await
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "writer 未在时限内到达 claim 前定序门"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    f.hub_store()
        .reserve("act-t1c-old", ControlRoute::DesktopIpc)
        .expect("reserve");

    // 放行 writer：最终判定 Allow → claim 胜出 → 帧写出（回执不投放——动作停在
    // reserved 等真实回执；帧到达 peer 即证明 claim 已先行提交）
    f.state
        .codex_control
        .adapter
        .set_write_right_gate_for_tests(None)
        .await;
    gate.notify_waiters();
    let frame = recv_within(&mut peer, 10).await;
    assert_eq!(
        frame["method"], "thread-follower-start-turn",
        "claim 胜出路径：首帧写出（可能投递态）"
    );

    // 写右已取得后的人工继续：撤销 CAS 0 行 → 在途明确状态（V2：非 converged）、
    // 不建后继
    let r = action_manual_resume(
        &f,
        serde_json::json!({
            "actionId": "act-t1c-old", "newActionId": "act-t1c-new",
            "text": "T1 已越点", "reason": "T1 claim 先行"
        }),
    )
    .await;
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(
        v["status"], "write_right_in_flight",
        "V2：写右已先取得且在途未收尾 → 独立状态（非 converged 假收敛）: {v}"
    );
    assert!(
        f.hub_store()
            .get_for_device("act-t1c-new", "dev-1")
            .ok()
            .flatten()
            .is_none(),
        "T1：已越过不可撤回写入点 → 不建重复后继"
    );
    let view = f
        .hub_store()
        .get_for_device("act-t1c-old", "dev-1")
        .ok()
        .flatten()
        .expect("旧动作在册");
    assert_eq!(
        view.state,
        ActionState::Reserved,
        "T1：旧动作定格不做（保持 reserved 等真实回执收尾）"
    );
    drop(sse);
}

/// R-T1 反例保留：claim 写入失败（DB 故障）→ 保守拒绝首写（独立定格码
/// `write_guard_read_failed`，与 Gate C #4 同纪律——宁可不写，不可在未知撤销
/// 状态下首写）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn t1_claim_read_failure_blocks_first_write() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let (sse, _) = sse_hold_ensure(&f, &mut peer, THREAD, vec![], "idle").await;

    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(send_body("T1 claim 失败", Some("act-t1f"), true)),
        ))
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["status"], "codex_queued", "{v}");

    // 现行连接注入恒失败的 claim（生产闭包 DB 故障的等价面）
    f.state
        .codex_control
        .adapter
        .set_write_right_claim_for_tests(Some(Arc::new(|_action_id: &str, _licensed: bool| {
            multi_agents_manager_lib::services::codex_control::WriteRightClaimOutcome::ReadFailed
        })))
        .await;

    let hub = f.state.codex_control.clone();
    let key = f.thread_key();
    let flushed = tokio::time::timeout(Duration::from_secs(15), async move {
        let _ = hub.adapter.ensure_thread(&key).await;
        hub.router.flush_thread(&key).await
    })
    .await
    .expect("flush in time");
    assert!(flushed.is_some(), "flush 有界收尾");
    assert_eq!(
        count_start_turn_frames(&mut peer).await,
        0,
        "T1：claim 写失败 → 零首帧（保守拒）"
    );
    let view = wait_action_state(&f, "act-t1f", ActionState::Unknown, Duration::from_secs(5)).await;
    assert_eq!(
        view.receipt.as_ref().and_then(|r| r.reason_code.as_deref()),
        Some("write_guard_read_failed"),
        "{view:?}"
    );
    drop(sse);
}

/// R3 验收补例（Gate E #4，并发交错）：A/B 两打断**同时**在飞（各自挂起在
/// interrupt RPC 的回执上），响应按 B 先 A 后交错投放——B 明确拒绝、A 受理。
/// 断言：各自只撤自己的 token；A 受理留下的暂停存活；B 的拒绝不互相清除。
#[tokio::test]
async fn r3_concurrent_interrupts_each_withdraw_own_token_only() {
    let mut f = fixture_ipc_interrupt(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let (sse, _) = sse_hold_ensure(&f, &mut peer, THREAD, vec![], "active").await;

    // A/B 同时发起（两任务都在等 interrupt RPC 回执）
    let app_a = f.app.clone();
    let task_a = tokio::spawn(async move {
        app_a
            .oneshot(req(
                "POST",
                "/m/api/v1/codex/turn/interrupt",
                Some(COOKIE),
                Some(serde_json::json!({ "sessionId": THREAD, "turnId": "t-live" }).to_string()),
            ))
            .await
            .unwrap()
    });
    let app_b = f.app.clone();
    let task_b = tokio::spawn(async move {
        app_b
            .oneshot(req(
                "POST",
                "/m/api/v1/codex/turn/interrupt",
                Some(COOKIE),
                Some(serde_json::json!({ "sessionId": THREAD, "turnId": "t-second" }).to_string()),
            ))
            .await
            .unwrap()
    });

    // 两条 interrupt 帧都到达（并发在飞；各自带自己的 pause token）
    let frame1 = recv_within(&mut peer, 10).await;
    assert_eq!(frame1["method"], "thread-follower-interrupt-turn");
    let frame2 = recv_within(&mut peer, 10).await;
    assert_eq!(frame2["method"], "thread-follower-interrupt-turn");

    // 交错投放：先 B（error → Rejected），后 A（success → Accepted）
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame2["requestId"],
        "resultType": "error", "method": "thread-follower-interrupt-turn",
        "error": { "message": "busy" }
    }))
    .await;
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame1["requestId"],
        "resultType": "success", "method": "thread-follower-interrupt-turn",
        "result": { "interruptedTurnId": "t-live", "ok": true }
    }))
    .await;

    let r_b = tokio::time::timeout(Duration::from_secs(15), task_b)
        .await
        .expect("B in time")
        .unwrap();
    assert_eq!(r_b.status(), 409, "B 被桌面明确拒绝");
    let r_a = tokio::time::timeout(Duration::from_secs(15), task_a)
        .await
        .expect("A in time")
        .unwrap();
    assert_eq!(r_a.status(), 200, "A 受理");
    let v: serde_json::Value = serde_json::from_str(&body_string(r_a).await).unwrap();
    assert_eq!(v["queuePaused"], serde_json::json!(true), "A 受理留暂停");

    // A 的暂停存活（B 的撤回只清 B 自己的 token）
    let paused: i64 = {
        let conn = f.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM codex_queue_pause WHERE thread_id = ?1",
            [THREAD],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(
        paused, 1,
        "R3：并发交错后 A 的暂停存活（B 的拒绝不清除他人暂停）"
    );
    let r = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/queue?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    let qv: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(qv["queuePaused"], serde_json::json!(true), "{qv}");
    drop(sse);
}

// ============================================================
// 复审 R-T2：暂停行按 token 多行共存——每个在途打断自持保护
// ============================================================

/// R-T2 主红绿例（复审给定反向定序）：A 建立保护等回执 → B 也落**自己的**行 →
/// **A 先撤回**（Rejected 明确未发生）→ **B 后返回 Accepted**。多行共存下 B 的
/// 保护自持，A 的撤回不伤 B → B 回执 `queuePaused=true`、暂停行存活。
/// 红轮（单行主键：B 的前置落行冲突、不拥有保护；A 撤回清掉唯一行）：
/// B Accepted 直接 `queuePaused=false`——已受理打断的旧队列失去暂停保护
///（无 resume 不自动恢复 = 旧队列失去冻结屏障）。
#[tokio::test]
async fn t2_a_rejected_b_accepted_pause_survives() {
    let mut f = fixture_ipc_interrupt(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let (sse, _) = sse_hold_ensure(&f, &mut peer, THREAD, vec![], "active").await;

    // A/B 同时发起（各自 pause_pre 落自己的行；都在等 interrupt RPC 回执）
    let app_a = f.app.clone();
    let task_a = tokio::spawn(async move {
        app_a
            .oneshot(req(
                "POST",
                "/m/api/v1/codex/turn/interrupt",
                Some(COOKIE),
                Some(serde_json::json!({ "sessionId": THREAD, "turnId": "t-live" }).to_string()),
            ))
            .await
            .unwrap()
    });
    let app_b = f.app.clone();
    let task_b = tokio::spawn(async move {
        app_b
            .oneshot(req(
                "POST",
                "/m/api/v1/codex/turn/interrupt",
                Some(COOKIE),
                Some(serde_json::json!({ "sessionId": THREAD, "turnId": "t-second" }).to_string()),
            ))
            .await
            .unwrap()
    });
    let frame1 = recv_within(&mut peer, 10).await;
    assert_eq!(frame1["method"], "thread-follower-interrupt-turn");
    let frame2 = recv_within(&mut peer, 10).await;
    assert_eq!(frame2["method"], "thread-follower-interrupt-turn");

    // 反向定序：**A 先撤回**（error → Rejected → 只清自己 token 的行），
    // **B 后返回**（success → Accepted → 读暂停状态装配回执）
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame1["requestId"],
        "resultType": "error", "method": "thread-follower-interrupt-turn",
        "error": { "message": "busy" }
    }))
    .await;
    let r_a = tokio::time::timeout(Duration::from_secs(15), task_a)
        .await
        .expect("A in time")
        .unwrap();
    assert_eq!(r_a.status(), 409, "A 被桌面明确拒绝（撤回自己的保护）");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame2["requestId"],
        "resultType": "success", "method": "thread-follower-interrupt-turn",
        "result": { "interruptedTurnId": "t-second", "ok": true }
    }))
    .await;
    let r_b = tokio::time::timeout(Duration::from_secs(15), task_b)
        .await
        .expect("B in time")
        .unwrap();
    assert_eq!(r_b.status(), 200, "B 受理");
    let v: serde_json::Value = serde_json::from_str(&body_string(r_b).await).unwrap();
    assert_eq!(
        v["queuePaused"],
        serde_json::json!(true),
        "R-T2：A 撤回不伤 B——B 受理自持保护（红轮 false = 保护丢失）"
    );

    // 暂停行存活（B 的 token 行），队列视图如实
    let paused: i64 = {
        let conn = f.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM codex_queue_pause WHERE thread_id = ?1",
            [THREAD],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(
        paused, 1,
        "R-T2：A 撤回后 B 的暂停行存活（旧队列不被自动恢复）"
    );
    let r = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/queue?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    let qv: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(qv["queuePaused"], serde_json::json!(true), "{qv}");
    drop(sse);
}

/// R-T2 矩阵 · A 成功 / B 失败：A 受理留行；B 拒绝只撤自己的行 → A 的暂停存活。
#[tokio::test]
async fn t2_a_accepted_b_rejected_pause_survives() {
    let mut f = fixture_ipc_interrupt(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let (sse, _) = sse_hold_ensure(&f, &mut peer, THREAD, vec![], "active").await;

    let app_a = f.app.clone();
    let task_a = tokio::spawn(async move {
        app_a
            .oneshot(req(
                "POST",
                "/m/api/v1/codex/turn/interrupt",
                Some(COOKIE),
                Some(serde_json::json!({ "sessionId": THREAD, "turnId": "t-live" }).to_string()),
            ))
            .await
            .unwrap()
    });
    let app_b = f.app.clone();
    let task_b = tokio::spawn(async move {
        app_b
            .oneshot(req(
                "POST",
                "/m/api/v1/codex/turn/interrupt",
                Some(COOKIE),
                Some(serde_json::json!({ "sessionId": THREAD, "turnId": "t-second" }).to_string()),
            ))
            .await
            .unwrap()
    });
    let frame1 = recv_within(&mut peer, 10).await;
    let frame2 = recv_within(&mut peer, 10).await;
    assert_eq!(frame1["method"], "thread-follower-interrupt-turn");
    assert_eq!(frame2["method"], "thread-follower-interrupt-turn");

    // A 先受理（success），B 后被拒（error → 只撤自己的行）
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame1["requestId"],
        "resultType": "success", "method": "thread-follower-interrupt-turn",
        "result": { "interruptedTurnId": "t-live", "ok": true }
    }))
    .await;
    let r_a = tokio::time::timeout(Duration::from_secs(15), task_a)
        .await
        .expect("A in time")
        .unwrap();
    assert_eq!(r_a.status(), 200, "A 受理");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame2["requestId"],
        "resultType": "error", "method": "thread-follower-interrupt-turn",
        "error": { "message": "busy" }
    }))
    .await;
    let r_b = tokio::time::timeout(Duration::from_secs(15), task_b)
        .await
        .expect("B in time")
        .unwrap();
    assert_eq!(r_b.status(), 409, "B 被桌面明确拒绝");

    let paused: i64 = {
        let conn = f.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM codex_queue_pause WHERE thread_id = ?1",
            [THREAD],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(
        paused, 1,
        "R-T2 矩阵 A✓/B✗：B 的失败撤回只清自己的行，A 的暂停存活"
    );
    drop(sse);
}

/// R-T2 矩阵 · 双方失败：两个打断都被桌面明确拒绝 → 各自撤回自己的行 → 无行
///（如实语义：没有已受理的打断需要保护，旧队列留在既有守卫链，不再暂停）。
#[tokio::test]
async fn t2_both_interrupts_rejected_pause_lifted() {
    let mut f = fixture_ipc_interrupt(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let (sse, _) = sse_hold_ensure(&f, &mut peer, THREAD, vec![], "active").await;

    let app_a = f.app.clone();
    let task_a = tokio::spawn(async move {
        app_a
            .oneshot(req(
                "POST",
                "/m/api/v1/codex/turn/interrupt",
                Some(COOKIE),
                Some(serde_json::json!({ "sessionId": THREAD, "turnId": "t-live" }).to_string()),
            ))
            .await
            .unwrap()
    });
    let app_b = f.app.clone();
    let task_b = tokio::spawn(async move {
        app_b
            .oneshot(req(
                "POST",
                "/m/api/v1/codex/turn/interrupt",
                Some(COOKIE),
                Some(serde_json::json!({ "sessionId": THREAD, "turnId": "t-second" }).to_string()),
            ))
            .await
            .unwrap()
    });
    let frame1 = recv_within(&mut peer, 10).await;
    let frame2 = recv_within(&mut peer, 10).await;
    assert_eq!(frame1["method"], "thread-follower-interrupt-turn");
    assert_eq!(frame2["method"], "thread-follower-interrupt-turn");

    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame1["requestId"],
        "resultType": "error", "method": "thread-follower-interrupt-turn",
        "error": { "message": "busy" }
    }))
    .await;
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame2["requestId"],
        "resultType": "error", "method": "thread-follower-interrupt-turn",
        "error": { "message": "busy" }
    }))
    .await;
    let r_a = tokio::time::timeout(Duration::from_secs(15), task_a)
        .await
        .expect("A in time")
        .unwrap();
    let r_b = tokio::time::timeout(Duration::from_secs(15), task_b)
        .await
        .expect("B in time")
        .unwrap();
    assert_eq!(r_a.status(), 409);
    assert_eq!(r_b.status(), 409);

    let paused: i64 = {
        let conn = f.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM codex_queue_pause WHERE thread_id = ?1",
            [THREAD],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(
        paused, 0,
        "R-T2 矩阵 双败：各自撤回自己的行 → 无已受理打断需要保护，不再暂停"
    );
    drop(sse);
}

/// R-T2 矩阵 · resume 交错（Accepted **前**插一拍）：B 前置行已落、用户显式
/// resume（清全部行）晚于该落行 = 知情恢复（Gate C #5 用户裁决优先）→ B 受理
/// 回执如实 `queuePaused=false`、队列不自动暂停（多行语义下优先序不变）。
///（Accepted **后** resume 的清行 + FIFO 接力由既有
/// `interrupt_accepted_pauses_queue_resume_and_bypass_flow` 继续钉住。）
#[tokio::test]
async fn t2_resume_between_pre_and_acceptance_keeps_user_priority() {
    let mut f = fixture_ipc_interrupt(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let (sse, _) = sse_hold_ensure(&f, &mut peer, THREAD, vec![], "active").await;

    let app_b = f.app.clone();
    let task_b = tokio::spawn(async move {
        app_b
            .oneshot(req(
                "POST",
                "/m/api/v1/codex/turn/interrupt",
                Some(COOKIE),
                Some(serde_json::json!({ "sessionId": THREAD, "turnId": "t-live" }).to_string()),
            ))
            .await
            .unwrap()
    });
    let frame = recv_within(&mut peer, 10).await;
    assert_eq!(frame["method"], "thread-follower-interrupt-turn");

    // B 的前置行已落（帧已到达）；用户显式 resume（知情恢复，清全部行）
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/queue/resume",
            Some(COOKIE),
            Some(serde_json::json!({ "sessionId": THREAD }).to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "resume 200");

    // B 后返回受理：回执如实 false（resume 晚于落行 = 用户已知情）
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-interrupt-turn",
        "result": { "interruptedTurnId": "t-live", "ok": true }
    }))
    .await;
    let r_b = tokio::time::timeout(Duration::from_secs(15), task_b)
        .await
        .expect("B in time")
        .unwrap();
    assert_eq!(r_b.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r_b).await).unwrap();
    assert_eq!(
        v["queuePaused"],
        serde_json::json!(false),
        "R-T2：用户显式 resume 优先（Gate C #5），受理臂不回写暂停行: {v}"
    );
    let paused: i64 = {
        let conn = f.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM codex_queue_pause WHERE thread_id = ?1",
            [THREAD],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(paused, 0, "resume 清全部行（含 B 的前置行）");
    drop(sse);
}

// ============================================================
// 复审 R-T4 后端：队列暂停状态变化有自己的可靠传播（投影 + SSE）
// ============================================================

/// 从 SSE body 读下一帧并解析 ThreadProjection JSON（有界预算）。
async fn sse_next_projection(
    body: &mut axum::body::Body,
    budget: Duration,
    what: &'static str,
) -> serde_json::Value {
    use http_body_util::BodyExt;
    let frame = tokio::time::timeout(budget, body.frame())
        .await
        .expect(what)
        .expect("sse frame ok");
    let text = String::from_utf8(frame.unwrap().data_ref().unwrap().to_vec()).unwrap();
    serde_json::from_str(text.split("data:").nth(1).map(str::trim).unwrap_or("null"))
        .expect("ThreadProjection JSON")
}

/// R-T4 红测：A（SSE 订阅者，首快照 `queuePaused=false`）→ B 走**真实打断端点**、
/// RPC 超时 UnknownDelivery、连接保持健康、Desktop **零新帧**（不发任何状态帧）
/// → A 的 SSE 必须收到 `queuePaused=true` 帧（前置落行触发的投影重发布）；
/// 显式 resume → 反向 `queuePaused=false` 帧。零轮询（纯 SSE 事件流）。
/// 红轮（投影无 queuePaused / 暂停不重发布）：SSE 等不到翻转帧（超时红）。
#[tokio::test]
async fn t4_pause_state_reaches_sse_without_desktop_frames() {
    let mut f = fixture_ipc_interrupt(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let (mut sse, first) = sse_hold_ensure(&f, &mut peer, THREAD, vec![], "active").await;
    assert_eq!(
        first["queuePaused"],
        serde_json::json!(false),
        "首快照如实：未暂停（{first}）"
    );

    // RPC 0.25s 超时 → 打断 UnknownDelivery；连接保持健康（写成功、无断连）
    f.state.codex_control.adapter.set_conn_tuning(S3ConnTuning {
        rpc_timeout: Duration::from_millis(250),
        ..Default::default()
    });

    // B 真实打断端点：帧到 peer 后**不回执**（Desktop 零新帧）
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/turn/interrupt",
            Some(COOKIE),
            Some(serde_json::json!({ "sessionId": THREAD, "turnId": "t-live" }).to_string()),
        ))
        .await
        .unwrap()
    });
    let frame = recv_within(&mut peer, 10).await;
    assert_eq!(frame["method"], "thread-follower-interrupt-turn");

    // A 的 SSE 收到暂停帧（前置落行 → 投影重发布；不等 Desktop 状态帧）
    let mut saw_paused = false;
    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline {
            let v =
                sse_next_projection(&mut sse, Duration::from_secs(5), "暂停帧在时限内到达").await;
            if v["queuePaused"] == serde_json::json!(true) {
                saw_paused = true;
                break;
            }
        }
    }
    assert!(
        saw_paused,
        "R-T4：Desktop 零新帧下 A 仍必须及时看到 queuePaused=true（SSE 可靠传播）"
    );

    // Desktop 确实零新帧（除打断请求帧外，peer 未收到任何后续帧）
    {
        let got = tokio::time::timeout(Duration::from_millis(400), peer.recv()).await;
        assert!(
            got.is_err(),
            "Desktop 不应发出任何新帧：{:?}",
            got.ok().map(|f| f["method"].clone())
        );
    }

    // 打断端点按 UnknownDelivery 如实收尾（不确定、连接健康、暂停在场）
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("interrupt in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["uncertain"], serde_json::json!(true), "{v}");
    assert_eq!(v["queuePaused"], serde_json::json!(true), "{v}");

    // 显式 resume → 反向帧（queuePaused=false）
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/queue/resume",
            Some(COOKIE),
            Some(serde_json::json!({ "sessionId": THREAD }).to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let mut saw_resumed = false;
    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline {
            let v =
                sse_next_projection(&mut sse, Duration::from_secs(5), "恢复帧在时限内到达").await;
            if v["queuePaused"] == serde_json::json!(false) {
                saw_resumed = true;
                break;
            }
        }
    }
    assert!(
        saw_resumed,
        "R-T4：resume 后 A 必须收到 queuePaused=false 反向帧"
    );
}

// ============================================================
// 复审 round 4（V1）：暂停重发布的调度自激循环
// ============================================================

/// V1 红测：**真实调度循环**（`codex_control.start` 生产 scheduler，复审点名——
/// 不能只起 HTTP 路由夹具）在场；一次暂停翻转完成后 Desktop 保持静默、暂停事实
/// 不变——SSE 帧流必须停止（旧实现：settle 只要读到暂停还在就无条件
/// `republish_thread_projection`，`publish_event` 每次分配新 local_seq，scheduler
/// 收到自己的投影再次结算再发布，周期对账每拍同款——无外部输入持续循环，SSE
/// 序号递增去重挡不住）。随后显式 resume 的反向通知仍可靠到达；旧队列仍需显式
/// 恢复（静默期冻结、resume 后才按 FIFO 接力）。
/// 红轮（旧 HEAD `6e0eec94`：重发布不按状态变化去重）：静默期内持续收到投影帧。
#[tokio::test]
async fn v1_paused_settle_quiesces_and_resume_still_reaches() {
    let mut f = fixture_ipc_interrupt(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    // RPC 0.25s 超时必须在**建连之前**注入（tuning 只在新连接 spawn 时读取）——
    // 打断端点按 UnknownDelivery 有界收尾，不等真实 15s
    f.state.codex_control.adapter.set_conn_tuning(S3ConnTuning {
        rpc_timeout: Duration::from_millis(250),
        ..Default::default()
    });
    let (mut sse, first) = sse_hold_ensure(&f, &mut peer, THREAD, vec![], "idle").await;
    assert_eq!(
        first["queuePaused"],
        serde_json::json!(false),
        "首快照如实：未暂停（{first}）"
    );

    // 对账周期 20ms（旧实现的每拍重发布/自激循环在该频率下静默期内必然刷出大量
    // 帧）；生产调度循环（复审点名：真实 scheduler 在场）
    f.state.codex_control.set_reconcile_period_ms(20);
    f.state.codex_control.start(f.state.clone());

    // 一次暂停翻转：真实打断端点；帧到 peer 后不回执（Desktop 此后全程静默）
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/turn/interrupt",
            Some(COOKIE),
            Some(serde_json::json!({ "sessionId": THREAD, "turnId": "t-live" }).to_string()),
        ))
        .await
        .unwrap()
    });
    let frame = recv_within(&mut peer, 10).await;
    assert_eq!(frame["method"], "thread-follower-interrupt-turn");

    // SSE 收到 queuePaused=true 翻转帧（唯一一次）
    let mut saw_paused = false;
    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline {
            let v =
                sse_next_projection(&mut sse, Duration::from_secs(5), "暂停帧在时限内到达").await;
            if v["queuePaused"] == serde_json::json!(true) {
                saw_paused = true;
                break;
            }
        }
    }
    assert!(saw_paused, "V1：暂停翻转帧必须到达（SSE 可靠传播不受影响）");

    // 打断端点按 UnknownDelivery 如实收尾（不确定、暂停在场）
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("interrupt in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["uncertain"], serde_json::json!(true), "{v}");

    // 旧队列内容（暂停翻转**之后**入队——暂停 + 队列已空时 queue_only 允许入队，
    // S2 ②'；先入队会被首个 tick 在暂停建立前派发，不成其为「旧队列」）：
    // 暂停期间必须冻结，恢复只经显式 resume
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(send_body("V1 旧队列", Some("act-v1-old"), true)),
        ))
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["status"], "codex_queued", "{v}");

    // —— 静默期：Desktop 静默、暂停事实不变 → 帧流必须停止（红 = 持续刷帧）——
    {
        let got = tokio::time::timeout(
            Duration::from_millis(1_500),
            sse_next_projection(&mut sse, Duration::from_secs(5), "静默期帧"),
        )
        .await;
        assert!(
            got.is_err(),
            "V1：暂停事实不变时调度循环不得持续重发布（静默期仍在刷帧：{:?}）",
            got.ok().map(|v| v["queuePaused"].clone())
        );
    }
    // Desktop 侧同样零新帧
    {
        let got = tokio::time::timeout(Duration::from_millis(400), peer.recv()).await;
        assert!(
            got.is_err(),
            "V1：静默期 Desktop 不应收到任何新帧：{:?}",
            got.ok().map(|f| f["method"].clone())
        );
    }
    // 旧队列未被放行：仍 queued（暂停事实不变 ≠ 可派发——旧队列仍需明确恢复）
    let view = f
        .hub_store()
        .get_for_device("act-v1-old", "dev-1")
        .unwrap()
        .expect("旧动作在册");
    assert_eq!(
        view.state,
        ActionState::Queued,
        "V1：旧队列静默期保持冻结（不得被调度循环放行）"
    );

    // —— 显式 resume：反向通知仍可靠到达 ——
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/queue/resume",
            Some(COOKIE),
            Some(serde_json::json!({ "sessionId": THREAD }).to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let mut saw_resumed = false;
    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline {
            let v =
                sse_next_projection(&mut sse, Duration::from_secs(5), "恢复帧在时限内到达").await;
            if v["queuePaused"] == serde_json::json!(false) {
                saw_resumed = true;
                break;
            }
        }
    }
    assert!(
        saw_resumed,
        "V1：resume 后反向 queuePaused=false 帧必须到达"
    );

    // 旧队列显式恢复后按 FIFO 接力（idle 镜像 + 无 pending → start-turn 帧到 peer）
    let relay = recv_within(&mut peer, 10).await;
    assert_eq!(
        relay["method"], "thread-follower-start-turn",
        "V1：显式恢复后旧队列才接力派发"
    );
}

/// V1 一致性钉子（复审点名「部分发布点缺 queuePaused 盖章」）：暂停中的线程收到
/// Desktop 状态帧驱动的重发布（state-changed fresh 臂；client-disconnect / mark
/// stale 与它同用 `stamp_queue_pause` 单点），帧必须携带**真实暂停事实**——旧实现
/// 漏盖 → mirror 缺省 false 广播假「未暂停」。
/// 红轮（旧 HEAD `6e0eec94`）：重发布帧 `queuePaused=false`（假状态）。
#[tokio::test]
async fn v1_state_frame_republish_carries_queue_pause_fact() {
    let mut f = fixture_ipc_interrupt(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let (mut sse, first) = sse_hold_ensure(&f, &mut peer, THREAD, vec![], "idle").await;
    assert_eq!(
        first["queuePaused"],
        serde_json::json!(false),
        "首快照如实：未暂停（{first}）"
    );

    // 暂停在场（打断前置档 = 正式档的账本事实；与端点落行同一 DAO 缝）
    f.hub_store()
        .set_queue_pause(&f.thread_key(), "interrupt_pending", "tok-stamp")
        .unwrap();

    // Desktop 状态帧（revision 7 → 8）→ state-changed fresh 重发布
    push_snapshot(&mut peer, THREAD, 8, "idle", vec![]).await;
    let mut frame = None;
    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while tokio::time::Instant::now() < deadline {
            let v =
                sse_next_projection(&mut sse, Duration::from_secs(5), "状态帧重发布在时限内").await;
            if v["revision"] == serde_json::json!(8) {
                frame = Some(v);
                break;
            }
        }
    }
    let v = frame.expect("状态帧的重发布帧必须到达");
    assert_eq!(
        v["queuePaused"],
        serde_json::json!(true),
        "V1：状态帧重发布必须携带真实暂停事实（红 = mirror 缺省 false 假状态）: {v}"
    );
}

// ============================================================
// 复审 round 4（V2）：Unknown 收尾后的知情新发出口
// ============================================================

/// V2 红测（复审原文用户路径，端到端真实编排）：真实 writer 取得写右（claim）→
/// start-turn 已写出、RPC 无回执 → **真实超时收尾**（前台预算到期 + wrap-up 落
/// Unknown，非注入）→ 只读核对 unresolved → 用户阅读三件风险确认「仍要重新发送」
/// → 人工继续必须建立**唯一后继**（设计 §6.2「明确接受知情重复风险」），旧动作
/// Unknown 事实保持、保守屏障按语义解除、后继走正常发送路径。
/// 红轮（旧 HEAD `6e0eec94`：所有生产首写都留 claim、人工继续对任何 claim 直接
/// `WriteRightObtained`）：HTTP 返回 `{status:"converged", action:{state:"unknown"}}`
/// ——不建后继、不解屏障、面板「不再重复发送」，知情新发被已收尾旧尝试永久堵死。
#[tokio::test]
async fn v2_unknown_after_timeout_manual_resume_creates_unique_successor() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    f.state.codex_control.set_front_budget_ms(400);
    f.state.codex_control.set_wrap_up_ms(2_000);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let body = send_body("V2 原文", Some("act-v2-old"), false);
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/session-send",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    drive_ensure_bounded(&mut peer, THREAD, "idle").await;
    let frame = recv_within(&mut peer, 15).await;
    assert_eq!(
        frame["method"], "thread-follower-start-turn",
        "真实写出（writer 的 claim 已先行取得）"
    );
    // 无回执：前台到期 → 后台 wrap-up 收尾落 Unknown（真实超时路径）
    let r = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("前台预算有界结束")
        .unwrap();
    assert_eq!(r.status(), 200);
    let view = wait_action_state(
        &f,
        "act-v2-old",
        ActionState::Unknown,
        Duration::from_secs(6),
    )
    .await;
    assert_eq!(
        view.receipt.as_ref().and_then(|x| x.reason_code.as_deref()),
        Some("wrapup_elapsed"),
        "{view:?}"
    );
    assert!(
        f.hub_store().has_compat_fence(&f.thread_key()).unwrap(),
        "Unknown 保守屏障在册（人工继续前）"
    );

    // 只读核对：无正式证据 → unresolved（「没找到」≠「没发送」）
    let rv = action_verify(&f, "act-v2-old").await;
    assert_eq!(rv.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(rv).await).unwrap();
    assert_eq!(v["outcome"], "unresolved", "{v}");

    // —— 用户确认「仍要重新发送（可能重复）」→ 人工继续 ——
    let r = action_manual_resume(
        &f,
        serde_json::json!({
            "actionId": "act-v2-old", "newActionId": "act-v2-new",
            "text": "V2 原文", "reason": "V2 知情重发"
        }),
    )
    .await;
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(
        v["status"], "resumed",
        "V2：收尾 Unknown 后人工继续必须建唯一后继（红 = converged + unknown 假收敛）: {v}"
    );
    assert!(
        v["successorActionId"].is_string(),
        "后继 actionId 在场: {v}"
    );

    // 旧 Unknown 事实保持（不改写 NotSent/Cancelled）+ 屏障按语义解除
    let old = f
        .hub_store()
        .get_for_device("act-v2-old", "dev-1")
        .unwrap()
        .expect("旧动作在册");
    assert_eq!(old.state, ActionState::Unknown, "V2：旧动作 Unknown 保持");
    assert!(
        !f.hub_store().has_compat_fence(&f.thread_key()).unwrap(),
        "V2：人工继续解除保守屏障（barrier_force_released）"
    );

    // 后继随即走正常发送路径（唯一后继的真实派出：第二帧 start-turn）
    let succ_frame = recv_within(&mut peer, 15).await;
    assert_eq!(
        succ_frame["method"], "thread-follower-start-turn",
        "V2：后继经正常守卫路径派出"
    );
    // 幂等重放：同 old id 再确认 → already_resumed（不建第二个后继）
    let r = action_manual_resume(
        &f,
        serde_json::json!({
            "actionId": "act-v2-old", "newActionId": "act-v2-again",
            "text": "V2 原文", "reason": "V2 幂等重放"
        }),
    )
    .await;
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["status"], "already_resumed", "{v}");
    assert!(
        f.hub_store()
            .get_for_device("act-v2-again", "dev-1")
            .ok()
            .flatten()
            .is_none(),
        "V2：重放不建新后继"
    );
}

// ============================================================
// 终审 R4/R6：真实同步设置入口的通知调度 + 关闭/重开投影翻转（后端半边）
// ============================================================

/// R4 红测：**无当前 Tokio runtime 的裸线程**上调通知（真实同步命令语境）——
/// 旧实现 `Handle::try_current()` 失败即静默跳过 → schema 取证永不安排（红）。
/// 修后：tauri::async_runtime 调度（任意线程可用）→ 活连接上按需取证、当前
/// pending 原地出按钮（零断开）。
#[tokio::test]
async fn r4_notify_from_runtimeless_thread_still_kicks_schema() {
    use multi_agents_manager_lib::services::codex_desktop_ipc::probe::schema_probe::{
        SchemaEvidence, SchemaProbeOutcome,
    };

    let (io, peer) = ScriptedPeer::new();
    let adapter = Arc::new(DesktopIpcAdapter::new(
        Arc::new(StaticConnector::with_build(io, UNKNOWN_BUILD)),
        ProtocolProfile::unverified("windows", UNKNOWN_BUILD),
    ));
    let mut f = fixture_with(
        vec![codex_app_session(THREAD)],
        |store| Arc::new(CodexControlHub::with_adapter(store, adapter.clone(), true)),
        Some(peer),
    );
    let ledger_conn = f.conn.clone();
    f.state
        .codex_control
        .adapter
        .set_probe_ledger(Arc::new(SharedConnLedger(ledger_conn)));
    f.state
        .codex_control
        .adapter
        .set_probe_key_override(Some(COMPAT_PROBE_KEY.into()));
    f.state
        .codex_control
        .adapter
        .set_schema_probe_override(Some(SchemaProbeOutcome::Known(SchemaEvidence {
            fingerprint: "fp-r4".into(),
            command_decisions: vec!["accept".into(), "cancel".into()],
            file_decisions: vec!["accept".into(), "cancel".into()],
        })));
    let gate_store = f.state.store.clone();
    f.state
        .codex_control
        .adapter
        .set_auto_adapt_gate(Some(Arc::new(move || {
            gate_store.with(multi_agents_manager_lib::remote::auto_adapt_enabled_conn)
        })));
    // 关闭态建立活连接 + pending 卡（不可操作）
    f.state.store.with(|c| {
        c.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('remote.codex_auto_adapt', 'false')",
            [],
        )
        .unwrap();
    });
    let mut peer = f.peer.take().unwrap();
    let (mut body, first) = sse_hold_ensure(
        &f,
        &mut peer,
        THREAD,
        vec![serde_json::json!({
            "id": 61, "method": "item/commandExecution/requestApproval",
            "params": { "command": "pnpm test", "cwd": "/w",
                        "availableDecisions": ["accept", "cancel"] }
        })],
        "active",
    )
    .await;
    {
        // R5 后首快照以 projection_seq 盖章，启动缓冲的冗余重复帧不再补发——
        // 初始「禁用」断言直接落在首快照本体（权威且必达）。
        assert_eq!(
            first["pending"][0]["actionable"],
            serde_json::json!(false),
            "关闭态：卡不可操作 {first}"
        );
    }
    // 读链证据预置（探针零帧短路——只读探针的临时 follow/snapshot 会清掉镜像
    // pending，破坏 R6 的翻转帧断言）
    f.conn
        .lock()
        .unwrap()
        .execute_batch(&format!(
            "INSERT OR REPLACE INTO codex_ipc_probe
                (probe_key, capability, state, probe_stage, attempts,
                 first_seen_at, last_seen_at, evidence_at, evidence_stage)
             VALUES ('{COMPAT_PROBE_KEY}', 'state', 'probed', 'P3', 1, 1, 1, 1, 'P3')",
        ))
        .unwrap();

    // —— **裸线程**（无 runtime 语境）上开启 + 调通知（真实同步命令入口模拟）——
    let adapter = f.state.codex_control.adapter.clone();
    f.state.store.with(|c| {
        c.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('remote.codex_auto_adapt', 'true')",
            [],
        )
        .unwrap();
    });
    let handle = std::thread::spawn(move || {
        adapter.notify_auto_adapt_changed(true);
    });
    handle.join().expect("bare thread join");

    // 当前 pending 原地出按钮（通知被安排并执行；零断开）
    use http_body_util::BodyExt;
    let mut actionable = false;
    for _ in 0..8 {
        let updated = tokio::time::timeout(Duration::from_secs(10), body.frame())
            .await
            .expect("裸线程通知后的策略更新必须到达（R4）")
            .unwrap()
            .unwrap();
        let text = String::from_utf8(updated.data_ref().unwrap().to_vec()).unwrap();
        let frame: serde_json::Value =
            serde_json::from_str(text.split("data:").nth(1).map(str::trim).unwrap_or("null"))
                .unwrap();
        let pending = &frame["pending"][0];
        assert_eq!(pending["requestKey"], "n:61", "{frame}");
        if pending["actionable"] == serde_json::json!(true) {
            actionable = true;
            break;
        }
    }
    assert!(actionable, "R4：无 runtime 语境的通知必须仍被调度执行");
    drop(body);
}

/// R6 后端红测：设置**关闭**（活连接 + SSE 在场）→ 后端重发布线程投影，下游收到
/// 生产真实会发的翻转帧（当前 pending 从可操作 → 不可操作）；**重开** → 再翻回
/// 可操作。红轮：关闭无任何通知/重发布 → 收不到翻转帧。
#[tokio::test]
async fn r6_toggle_republishes_projection_with_availability_flip() {
    use multi_agents_manager_lib::services::codex_desktop_ipc::probe::schema_probe::{
        SchemaEvidence, SchemaProbeOutcome,
    };

    let (io, peer) = ScriptedPeer::new();
    let adapter = Arc::new(DesktopIpcAdapter::new(
        Arc::new(StaticConnector::with_build(io, UNKNOWN_BUILD)),
        ProtocolProfile::unverified("windows", UNKNOWN_BUILD),
    ));
    let mut f = fixture_with(
        vec![codex_app_session(THREAD)],
        |store| Arc::new(CodexControlHub::with_adapter(store, adapter.clone(), true)),
        Some(peer),
    );
    let ledger_conn = f.conn.clone();
    f.state
        .codex_control
        .adapter
        .set_probe_ledger(Arc::new(SharedConnLedger(ledger_conn)));
    f.state
        .codex_control
        .adapter
        .set_probe_key_override(Some(COMPAT_PROBE_KEY.into()));
    f.state
        .codex_control
        .adapter
        .set_schema_probe_override(Some(SchemaProbeOutcome::Known(SchemaEvidence {
            fingerprint: "fp-r6".into(),
            command_decisions: vec!["accept".into(), "cancel".into()],
            file_decisions: vec!["accept".into(), "cancel".into()],
        })));
    let gate_store = f.state.store.clone();
    f.state
        .codex_control
        .adapter
        .set_auto_adapt_gate(Some(Arc::new(move || {
            gate_store.with(multi_agents_manager_lib::remote::auto_adapt_enabled_conn)
        })));
    // 开启态活连接 + 可操作卡
    // 读链证据预置（探针零帧短路——只读探针的临时 follow/snapshot 会清掉镜像
    // pending，破坏 R6 的翻转帧断言）
    f.conn
        .lock()
        .unwrap()
        .execute_batch(&format!(
            "INSERT OR REPLACE INTO codex_ipc_probe
                (probe_key, capability, state, probe_stage, attempts,
                 first_seen_at, last_seen_at, evidence_at, evidence_stage)
             VALUES ('{COMPAT_PROBE_KEY}', 'state', 'probed', 'P3', 1, 1, 1, 1, 'P3')",
        ))
        .unwrap();

    let mut peer = f.peer.take().unwrap();
    let (mut body, _) = sse_hold_ensure(
        &f,
        &mut peer,
        THREAD,
        vec![serde_json::json!({
            "id": 62, "method": "item/commandExecution/requestApproval",
            "params": { "command": "pnpm test", "cwd": "/w",
                        "availableDecisions": ["accept", "cancel"] }
        })],
        "active",
    )
    .await;
    use http_body_util::BodyExt;

    // 开启态初值（服务端判定单点）：连接期 schema 已取证 + 活门开 → 决策词非空
    //（首快照帧已被 sse_hold_ensure 消费，此处以服务端权威判定确认初态）
    let pre = f
        .state
        .codex_control
        .adapter
        .projection(&f.thread_key())
        .await;
    assert!(
        !pre.pending.is_empty(),
        "开启态：pending 应在场（首帧前的镜像状态）"
    );
    assert!(
        !f.state.codex_control.adapter.approval_policy().is_empty(),
        "开启态：决策词非空（卡可操作）"
    );

    // —— 设置关闭（经 toggle 链通知适配器）→ SSE 必须收到生产发布的翻转帧 ——
    f.state.store.with(|c| {
        c.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('remote.codex_auto_adapt', 'false')",
            [],
        )
        .unwrap();
    });
    f.state
        .codex_control
        .adapter
        .notify_auto_adapt_changed(false);

    // 有界排水：中间可能夹“空投影/启动合并”帧（pending 空）——跳过直到出现同卡
    // 的不可操作翻转帧（生产重发布）。超时 = 关闭未重发布（红）。
    let mut saw_disabled = false;
    for _ in 0..10 {
        let updated = match tokio::time::timeout(Duration::from_secs(5), body.frame()).await {
            Ok(f) => f.unwrap().unwrap(),
            Err(_) => break, // 排空：后续无帧
        };
        let text = String::from_utf8(updated.data_ref().unwrap().to_vec()).unwrap();
        let frame: serde_json::Value =
            serde_json::from_str(text.split("data:").nth(1).map(str::trim).unwrap_or("null"))
                .unwrap();
        if frame["pending"].as_array().is_none_or(|a| a.is_empty()) {
            continue; // 空投影/启动合并帧：非本验收对象
        }
        let pending = &frame["pending"][0];
        assert_eq!(pending["requestKey"], "n:62", "同卡原地：{frame}");
        if pending["actionable"] == serde_json::json!(false) {
            // Gate F Major 2：关闭帧必须**真实翻转 availability**（controllable →
            // read_only）——不只 actionable 翻转。红轮（旧实现）：availability 不随
            // 设置活门翻转 → 恒 controllable → 本断言红。
            assert_eq!(
                frame["availability"],
                serde_json::json!("read_only"),
                "Gate F：关闭后 availability 必须真实翻转为 read_only：{frame}"
            );
            saw_disabled = true;
            break;
        }
    }
    assert!(saw_disabled, "R6：关闭必须重发布翻转（卡不可操作）");

    // —— 重开 → 再翻回可操作 ——
    f.state.store.with(|c| {
        c.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('remote.codex_auto_adapt', 'true')",
            [],
        )
        .unwrap();
    });
    f.state
        .codex_control
        .adapter
        .notify_auto_adapt_changed(true);
    let mut saw_reenabled = false;
    for _ in 0..10 {
        let updated = match tokio::time::timeout(Duration::from_secs(5), body.frame()).await {
            Ok(f) => f.unwrap().unwrap(),
            Err(_) => break,
        };
        let text = String::from_utf8(updated.data_ref().unwrap().to_vec()).unwrap();
        let frame: serde_json::Value =
            serde_json::from_str(text.split("data:").nth(1).map(str::trim).unwrap_or("null"))
                .unwrap();
        if frame["pending"].as_array().is_none_or(|a| a.is_empty()) {
            continue;
        }
        let pending = &frame["pending"][0];
        assert_eq!(pending["requestKey"], "n:62", "同卡原地：{frame}");
        if pending["actionable"] == serde_json::json!(true) {
            assert_eq!(
                frame["availability"],
                serde_json::json!("controllable"),
                "Gate F：重开后 availability 必须翻回 controllable：{frame}"
            );
            saw_reenabled = true;
            break;
        }
    }
    assert!(saw_reenabled, "R6：重开必须重发布翻转回可操作");
    // 零重探正例（与 F9 一致）：capability=schema 探测行不新增
    // 重开走持久装回路径：零重探（连接期 CLI 已跑过一次并记账——基线取重开前；
    // 重开后行数不增 = 装回命中、未重跑 CLI）
    let schema_rows_before: i64 = {
        let conn = f.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM codex_ipc_probe WHERE capability = 'schema'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(schema_rows_before, 1, "连接期 CLI 取证记账一次");
    drop(body);
    let schema_rows_after: i64 = {
        let conn = f.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM codex_ipc_probe WHERE capability = 'schema'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(
        schema_rows_after, schema_rows_before,
        "重开走装回路径：零重探（CLI 未重跑）"
    );
}

/// 终审 R5（真实 SSE 定序）：**首快照与事件共享本地排序轴**——快照生成时以当前
/// projection_seq 盖章后，陈旧缓冲帧（转发任务按镜像**未含 schema 档的旧策略**
/// 重发布，local_seq 更高）不得覆盖**更新首快照**。修复前首快照 local_seq=0，
/// 同 revision 的陈旧帧穿透 supersedes：启用卡被压回禁用且 Desktop 静默后永不
/// 收敛（设计 §3.4「不能出现第一次进入禁用」）；修复后陈旧帧被合并门丢弃。
#[tokio::test]
async fn sse_stale_enabled_frame_cannot_override_fresh_disabled_snapshot() {
    use multi_agents_manager_lib::services::codex_desktop_ipc::probe::schema_probe::SchemaProbeOutcome;

    let (io, peer) = ScriptedPeer::new();
    let adapter = Arc::new(DesktopIpcAdapter::new(
        Arc::new(StaticConnector::with_build(io, UNKNOWN_BUILD)),
        ProtocolProfile::unverified("windows", UNKNOWN_BUILD),
    ));
    let mut f = fixture_with(
        vec![codex_app_session(THREAD)],
        |store| {
            let _ = &store;
            Arc::new(CodexControlHub::with_adapter(store, adapter.clone(), true))
        },
        Some(peer),
    );
    let ledger_conn = f.conn.clone();
    f.state
        .codex_control
        .adapter
        .set_probe_ledger(Arc::new(SharedConnLedger(ledger_conn)));
    f.state
        .codex_control
        .adapter
        .set_probe_key_override(Some(COMPAT_PROBE_KEY.into()));
    // 读链证据预置（零帧短路 ensure_read_evidence）；schema 结论注入 Known。
    // 决策词**先落持久表**：连接期 spawn 的恢复任务把它装进连接身份（epoch 1），
    // 晚于本轮全部断言起点——安装完成由连接 #1 的闭环保证（见下）。
    f.conn
        .lock()
        .unwrap()
        .execute_batch(&format!(
            "INSERT OR REPLACE INTO codex_ipc_probe
                (probe_key, capability, state, probe_stage, attempts,
                 first_seen_at, last_seen_at, evidence_at, evidence_stage)
             VALUES ('{COMPAT_PROBE_KEY}', 'state', 'probed', 'P3', 1, 1, 1, 1, 'P3');
             INSERT OR REPLACE INTO codex_ipc_schema_evidence
                (probe_key, fingerprint, command_decisions, file_decisions, evidence_at)
             VALUES ('{COMPAT_PROBE_KEY}', 'fp-r5', '[\"accept\",\"cancel\"]',
                     '[\"accept\",\"cancel\"]', 1)",
        ))
        .unwrap();
    f.state
        .codex_control
        .adapter
        .set_schema_probe_override(Some(SchemaProbeOutcome::Unknown {
            error_code: "cli_not_found".into(),
        }));

    // —— 连接 #1：建立连接/镜像，并把 schema 档装进连接身份（恢复任务在
    //    ensure_connection 时 spawn；驱动完一轮后必然已装回）——
    let app = f.app.clone();
    let sse1 = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread/events?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    let mut peer = f.peer.take().unwrap();
    drive_ensure(
        &mut peer,
        THREAD,
        vec![serde_json::json!({
            "id": 51, "method": "item/commandExecution/requestApproval",
            "params": { "command": "pnpm test", "cwd": "/w",
                        "availableDecisions": ["accept", "cancel"] }
        })],
        "active",
    )
    .await;
    let r1 = tokio::time::timeout(Duration::from_secs(15), sse1)
        .await
        .expect("conn1 in time")
        .unwrap();
    assert_eq!(r1.status(), 200);
    // 关闭连接 #1（丢弃 body 即断流）→ 引用归零 unfollow → 镜像删除。
    drop(r1);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !f.state.codex_control.adapter.approval_policy().is_empty(),
        "前置：schema 档已装进连接身份（活值启用）"
    );

    // —— 连接 #2（被测）：重新跟随。跟随轮里转发任务处理状态帧时，镜像策略
    //    尚未含 schema 档（created fresh → from_caps 空）→ 发布**陈旧禁用帧**
    //    （local_seq 更高）入启动缓冲；随后 ensure_thread 末尾 projection()
    //    以活策略（启用）算出**更新首快照**——修复前首快照 local_seq=0，
    //    陈旧帧穿透 supersedes 把启用卡压回禁用。Desktop 此后静默。 ——
    let app2 = f.app.clone();
    let sse2 = tokio::spawn(async move {
        app2.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread/events?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    // 连接 #2 复用已初始化的 io（ensure_connection 短路）→ 无需重新握手：
    // follow_thread 的 discovery → following → 快照三轮照常。连接 #1 关闭会留下
    // 尾帧（load-complete-history / unfollow 的 following:false）——先排干。
    loop {
        let leftover = tokio::time::timeout(Duration::from_secs(2), peer.recv()).await;
        match leftover {
            Ok(f) if f["method"] == "thread-owner-discovery" => {
                peer.send(&discovery_response(
                    f["requestId"].as_str().unwrap(),
                    "owner-1",
                ))
                .await;
                break;
            }
            Ok(_) => continue, // 尾帧：排干
            Err(_) => panic!("连接 #2 的 discovery 未在 2s 内到达"),
        }
    }
    let follow2 = peer.recv().await;
    assert_eq!(follow2["method"], "thread-stream-following-changed");
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": THREAD, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": 7,
                "conversationState": {
                    "turns": [], "requests": [serde_json::json!({
                        "id": 52, "method": "item/commandExecution/requestApproval",
                        "params": { "command": "pnpm test", "cwd": "/w",
                                    "availableDecisions": ["accept", "cancel"] }
                    })],
                    "threadRuntimeStatus": { "type": "active" },
                    "turnHistory": codex_fixture::canonical_turn_history(
                        serde_json::json!({
                            "t1": { "id": "t1", "type": "turn", "status": "completed", "items": ["u1"] },
                            "u1": { "id": "u1", "type": "user_message", "text": "seed", "ts": 1 }
                        }),
                        serde_json::json!([{ "entries": ["t1"] }])
                    )
                }
            }
        }
    }))
    .await;
    let r2 = tokio::time::timeout(Duration::from_secs(15), sse2)
        .await
        .expect("conn2 in time")
        .unwrap();
    assert_eq!(r2.status(), 200);
    let mut body = r2.into_body();
    use http_body_util::BodyExt;
    let first = tokio::time::timeout(Duration::from_secs(10), body.frame())
        .await
        .expect("首快照 in time")
        .unwrap()
        .unwrap();
    let text = String::from_utf8(first.data_ref().unwrap().to_vec()).unwrap();
    let snap: serde_json::Value =
        serde_json::from_str(text.split("data:").nth(1).map(str::trim).unwrap_or("null"))
            .expect("首帧必须是 ThreadProjection JSON");
    assert_eq!(
        snap["pending"][0]["requestKey"],
        serde_json::json!("n:52"),
        "首快照为本连接跟随的卡 {snap}"
    );
    assert_eq!(
        snap["pending"][0]["actionable"],
        serde_json::json!(true),
        "更新首快照：schema 档已装（启用）{snap}"
    );

    // —— 验收：陈旧帧不得把启用卡压回禁用；此后 Desktop 静默也不收敛 ——
    // 有界读帧：任何后继帧都不得把卡翻回禁用（修复前：local_seq 更高的陈旧
    // 禁用帧穿透 supersedes → 红）。Desktop 不再发帧（不以另有帧为收敛前提）。
    for _ in 0..3 {
        match tokio::time::timeout(Duration::from_millis(1200), body.frame()).await {
            Ok(Some(frame)) => {
                let frame = frame.unwrap();
                let Some(data) = frame.data_ref() else {
                    continue; // keep-alive 注释等非数据帧
                };
                let text = String::from_utf8(data.to_vec()).unwrap();
                let frame: serde_json::Value = serde_json::from_str(
                    text.split("data:").nth(1).map(str::trim).unwrap_or("null"),
                )
                .unwrap_or(serde_json::Value::Null);
                if frame.is_null() {
                    continue;
                }
                assert_eq!(
                    frame["pending"][0]["actionable"],
                    serde_json::json!(true),
                    "陈旧缓冲帧不得覆盖更新首快照（R5）：{frame}"
                );
            }
            Ok(None) => break, // 流结束
            Err(_) => break,   // 有界窗口内无更多帧（Desktop 静默 → 正确）
        }
    }
    assert!(
        !peer_has_traffic(&mut peer).await,
        "此后无 Desktop 帧（不以另有帧为收敛前提）"
    );
}

/// Gate F Major 1：**快路径首快照同样以当前 projection_seq 盖章**——线程已跟随且
/// 镜像新鲜时（第二订阅者/手机重连），`ensure_thread` 走 `fresh_projection` 快路径；
/// 修复前其 local_seq 恒 0，启动缓冲/流式里任何先行发布的同 revision 帧（seq=N>0）
/// 都会被 supersedes 判为「更新」而覆盖新快照。验收：快路径投影的序号基线 ≥ 一切
/// 先行发布帧（M），与合并门/流式 supersedes 的 local_seq 决胜共同封死该洞。
#[tokio::test]
async fn fast_path_snapshot_sealed_with_projection_seq() {
    let mut f = fixture_unknown_build(vec![codex_app_session(THREAD)]);
    let ledger_conn = f.conn.clone();
    f.state
        .codex_control
        .adapter
        .set_probe_ledger(Arc::new(SharedConnLedger(ledger_conn)));
    f.state
        .codex_control
        .adapter
        .set_probe_key_override(Some(COMPAT_PROBE_KEY.into()));
    // 读链证据预置（ensure_read_evidence 零帧短路，drive 的一轮帧全给 follow_thread）
    f.conn
        .lock()
        .unwrap()
        .execute_batch(&format!(
            "INSERT OR REPLACE INTO codex_ipc_probe
                (probe_key, capability, state, probe_stage, attempts,
                 first_seen_at, last_seen_at, evidence_at, evidence_stage)
             VALUES ('{COMPAT_PROBE_KEY}', 'state', 'probed', 'P3', 1, 1, 1, 1, 'P3')",
        ))
        .unwrap();

    // —— 第一次订阅：完整跟随，镜像建立（conn1 保持打开 = 镜像持续新鲜）——
    let mut peer = f.peer.take().unwrap();
    let (body1, _) = sse_hold_ensure(&f, &mut peer, THREAD, vec![], "active").await;
    let _ = body1; // conn1 持引用（body 保活 = 订阅保活）；镜像新鲜由本轮 ensure 保证

    // —— 推一帧同线程状态（revision 8）：转发任务按镜像策略重发布（seq=M≥1）——
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": THREAD, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": 8,
                "conversationState": {
                    "turns": [], "requests": [],
                    "threadRuntimeStatus": { "type": "active" },
                    "turnHistory": codex_fixture::canonical_turn_history(
                        serde_json::json!({
                            "t1": { "id": "t1", "type": "turn", "status": "completed", "items": ["u1"] },
                            "u1": { "id": "u1", "type": "user_message", "text": "seed", "ts": 1 }
                        }),
                        serde_json::json!([{ "entries": ["t1"] }])
                    )
                }
            }
        }
    }))
    .await;
    wait_snapshot_applied(&f, 8, true).await;

    // —— 快路径：ensure_thread 第二次调用（镜像新鲜）→ fresh_projection ——
    let key = f.thread_key();
    let fast = f
        .state
        .codex_control
        .adapter
        .ensure_thread(&key)
        .await
        .expect("快路径 ensure");
    assert_eq!(fast.revision, Some(8));
    assert!(
        fast.local_seq >= 1,
        "快路径首快照必须以当前 projection_seq 盖章（≥ 先行发布帧 M；\
         修复前恒 0，会被 seq=M 的缓冲/流式帧覆盖）：local_seq={}",
        fast.local_seq
    );
    drop(body1);
    f.state.codex_control.stop();
}

// ============================================================
// 复审 round 3（T3）：快照携带的序号必须是内容实际观察到的版本
// ============================================================

/// T3 共用前置：未验证 build fixture（假 IPC peer 在场）+ 活值门注入 +
/// 读链/schema 决策词持久行（连接期恢复任务装回，连接 #1 闭环保证）+
/// 探针结论注入（绝不 spawn 本机 CLI）。
fn t3_fixture() -> Fixture {
    use multi_agents_manager_lib::services::codex_desktop_ipc::probe::schema_probe::SchemaProbeOutcome;
    let (io, peer) = ScriptedPeer::new();
    let adapter = Arc::new(DesktopIpcAdapter::new(
        Arc::new(StaticConnector::with_build(io, UNKNOWN_BUILD)),
        ProtocolProfile::unverified("windows", UNKNOWN_BUILD),
    ));
    let f = fixture_with(
        vec![codex_app_session(THREAD)],
        |store| Arc::new(CodexControlHub::with_adapter(store, adapter.clone(), true)),
        Some(peer),
    );
    let ledger_conn = f.conn.clone();
    f.state
        .codex_control
        .adapter
        .set_probe_ledger(Arc::new(SharedConnLedger(ledger_conn)));
    f.state
        .codex_control
        .adapter
        .set_probe_key_override(Some(COMPAT_PROBE_KEY.into()));
    // 活值门注入（with_adapter 不带生产门）：伙伴的设置翻转必须被 adapter 真实读到。
    let gate_store = f.state.store.clone();
    f.state
        .codex_control
        .adapter
        .set_auto_adapt_gate(Some(Arc::new(move || {
            gate_store.with(multi_agents_manager_lib::remote::auto_adapt_enabled_conn)
        })));
    // 读链证据 + schema 决策词先落持久表：连接期 spawn 的恢复任务把它装进连接
    // 身份（装回完成由连接 #1 的闭环 + 前置断言保证）。
    f.conn
        .lock()
        .unwrap()
        .execute_batch(&format!(
            "INSERT OR REPLACE INTO codex_ipc_probe
                (probe_key, capability, state, probe_stage, attempts,
                 first_seen_at, last_seen_at, evidence_at, evidence_stage)
             VALUES ('{COMPAT_PROBE_KEY}', 'state', 'probed', 'P3', 1, 1, 1, 1, 'P3');
             INSERT OR REPLACE INTO codex_ipc_schema_evidence
                (probe_key, fingerprint, command_decisions, file_decisions, evidence_at)
             VALUES ('{COMPAT_PROBE_KEY}', 'fp-t3', '[\"accept\",\"cancel\"]',
                     '[\"accept\",\"cancel\"]', 1)"
        ))
        .unwrap();
    f.state
        .codex_control
        .adapter
        .set_schema_probe_override(Some(SchemaProbeOutcome::Unknown {
            error_code: "cli_not_found".into(),
        }));
    f
}

/// T3 共用伙伴动作（真实设置翻转 + 真实重发布）：写 `remote.codex_auto_adapt=false`
/// → `notify_auto_adapt_changed(false)`（内部于 tauri runtime 异步重发布），阻塞至
/// 重发布事件**真正广播**——返回时禁用发布的序号已预留并广播，N 已在
/// 排序轴上。只认「卡禁用」帧（策略清空 → actionable=false）；缝窗口内唯一
/// 发布者就是这次重发布。
fn t3_flip_off_and_wait_republish(
    adapter: &Arc<DesktopIpcAdapter>,
    store: &DeviceStore,
) -> multi_agents_manager_lib::services::codex_desktop_ipc::types::ThreadProjection {
    store.with(|c| {
        c.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('remote.codex_auto_adapt', 'false')",
            [],
        )
        .unwrap();
    });
    // 先订阅再触发：重发布可能在本调用返回前就广播（撤销丢失事件）。
    let mut rx = adapter.subscribe();
    adapter.notify_auto_adapt_changed(false);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            std::time::Instant::now() < deadline,
            "T3 伙伴：重发布未在时限内广播（测试自检失败，非被测行为）"
        );
        match rx.try_recv() {
            Ok(p) if p.pending.iter().any(|pd| !pd.actionable) => return p,
            Ok(_) => continue,
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                panic!("T3 伙伴：投影广播通道关闭");
            }
        }
    }
}

/// 被测连接 #2：重新跟随（镜像已随连接 #1 关闭删除 → 完整 follow → ensure_thread
/// 末尾 `projection()` 全路径）。驱动 discovery/following/snapshot 三轮（R5 同款）。
async fn t3_drive_refollow_and_spawn_sse(f: &Fixture, peer: &mut ScriptedPeer) -> axum::body::Body {
    let app = f.app.clone();
    let sse = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread/events?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    // 连接 #1 关闭的尾帧（load-complete-history / following:false）排干，直到
    // 连接 #2 的 discovery 到达。
    loop {
        let leftover = tokio::time::timeout(Duration::from_secs(2), peer.recv()).await;
        match leftover {
            Ok(frame) if frame["method"] == "thread-owner-discovery" => {
                peer.send(&discovery_response(
                    frame["requestId"].as_str().unwrap(),
                    "owner-1",
                ))
                .await;
                break;
            }
            Ok(_) => continue, // 尾帧：排干
            Err(_) => panic!("连接 #2 的 discovery 未在 2s 内到达"),
        }
    }
    let follow = peer.recv().await;
    assert_eq!(follow["method"], "thread-stream-following-changed");
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": THREAD, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": 7,
                "conversationState": {
                    "turns": [], "requests": [serde_json::json!({
                        "id": 52, "method": "item/commandExecution/requestApproval",
                        "params": { "command": "pnpm test", "cwd": "/w",
                                    "availableDecisions": ["accept", "cancel"] }
                    })],
                    "threadRuntimeStatus": { "type": "active" },
                    "turnHistory": codex_fixture::canonical_turn_history(
                        serde_json::json!({
                            "t1": { "id": "t1", "type": "turn", "status": "completed", "items": ["u1"] },
                            "u1": { "id": "u1", "type": "user_message", "text": "seed", "ts": 1 }
                        }),
                        serde_json::json!([{ "entries": ["t1"] }])
                    )
                }
            }
        }
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(15), sse)
        .await
        .expect("被测连接在时限内响应")
        .unwrap();
    assert_eq!(r.status(), 200);
    r.into_body()
}

/// T3 主缺陷（真实 SSE 定序）：**锁外重盖章吞掉真实新策略**。旧代码
/// `projection()` 在镜像锁临界区内已盖章，却在返回前（锁外）用最新序号无条件
/// 覆盖。定序：旧启用快照读完并释放锁 →（缝内）设置关闭 + 真实重发布产生禁用
/// 投影 seq=N（Desktop revision 不变）→ 旧快照锁外盖 ≥N → 新禁用事件与旧快照同
/// revision/同序号，被去重丢弃；Desktop 随后静默，手机长期停留旧启用按钮。
/// 修复后：盖章收敛为临界区内单点（内容实际观察到的版本），seq=N > 盖章值 →
/// 禁用事件照常穿透 supersedes 到达下游。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sse_out_of_lock_restamp_cannot_swallow_real_policy_event() {
    let mut f = t3_fixture();
    let mut peer = f.peer.take().unwrap();

    // —— 连接 #1：建连 + 跟随 + 装回 schema 档；卡启用（前置）——
    let (body1, first1) = sse_hold_ensure(
        &f,
        &mut peer,
        THREAD,
        vec![serde_json::json!({
            "id": 51, "method": "item/commandExecution/requestApproval",
            "params": { "command": "pnpm test", "cwd": "/w",
                        "availableDecisions": ["accept", "cancel"] }
        })],
        "active",
    )
    .await;
    assert_eq!(
        first1["pending"][0]["actionable"],
        serde_json::json!(true),
        "前置：schema 档启用（{first1}）"
    );
    // 关闭连接 #1 → 引用归零 unfollow → 镜像删除（被测连接走完整 follow 全路径）。
    drop(body1);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !f.state.codex_control.adapter.approval_policy().is_empty(),
        "前置：schema 档已装进连接身份（活值启用）"
    );

    // —— 测试缝：projection() 临界区**之后**（旧代码「锁外重盖章」窗口）插窗。
    // 闭包自带一次性旗标：伙伴的重发布也走 projection()，不得二次触发。缝内完成
    // 整个伙伴动作并等到重发布事件广播——释放时序号 N 必已在轴上（发布点先于
    // 旧代码的重盖章 load，吞并判定因此确定）。 ——
    let adapter = f.state.codex_control.adapter.clone();
    let store = f.state.store.clone();
    let fired = std::sync::atomic::AtomicBool::new(false);
    f.state
        .codex_control
        .adapter
        .set_projection_sealed_gate(Some(Arc::new(move || {
            if fired.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            t3_flip_off_and_wait_republish(&adapter, &store);
        })));

    // —— 被测连接 #2：重新跟随；跟随轮转发任务的陈旧帧（M）与缝内重发布的真
    //    禁用帧（N>M）都进启动缓冲；首快照 = 本 projection() 的返回值。 ——
    let mut body2 = t3_drive_refollow_and_spawn_sse(&f, &mut peer).await;

    // —— 验收：真实新策略（禁用投影）必须到达下游；此后 Desktop 静默。 ——
    // 修复前：首快照（启用内容）锁外盖 ≥N → 缓冲里两条禁用帧（M、N）全部被
    // supersedes 去重丢弃 → 流上永远只有启用快照（红）。修复后：N > 临界区盖章
    // 值 → 禁用帧穿透（绿）。陈旧转发帧 M 在两版下都 ≤ 盖章值（内容也已被更新
    // 快照覆盖），不构成误绿来源。
    use http_body_util::BodyExt;
    let mut saw_disabled = false;
    let mut saw_enabled_snapshot = false;
    for _ in 0..6 {
        let frame = match tokio::time::timeout(Duration::from_millis(1500), body2.frame()).await {
            Ok(Some(f)) => f.unwrap(),
            Ok(None) => break,
            Err(_) => break, // 有界窗口内无更多帧（Desktop 静默 → 正确）
        };
        let Some(data) = frame.data_ref() else {
            continue; // keep-alive 等非数据帧
        };
        let text = String::from_utf8(data.to_vec()).unwrap();
        let frame: serde_json::Value =
            serde_json::from_str(text.split("data:").nth(1).map(str::trim).unwrap_or("null"))
                .unwrap_or(serde_json::Value::Null);
        if frame.is_null() || frame["pending"].as_array().is_none_or(|a| a.is_empty()) {
            continue;
        }
        if frame["pending"][0]["actionable"] == serde_json::json!(true) {
            saw_enabled_snapshot = true; // 旧启用快照在先（合法）
            continue;
        }
        saw_disabled = true;
        break;
    }
    assert!(
        saw_disabled,
        "T3：真实新策略（关闭→禁用投影）必须到达 SSE 下游，不得被旧内容盖章吞掉\
         （启用快照在场={saw_enabled_snapshot}；修复前禁用事件被去重丢弃，Desktop \
         静默后手机永久停留启用按钮）"
    );
    assert!(
        !peer_has_traffic(&mut peer).await,
        "此后 Desktop 静默（不以另有帧为收敛前提）"
    );
    drop(body2);
    f.state.codex_control.stop();
}

/// T3 次缺陷（真实 SSE 定序）：**caps/policy 锁外先读 + 临界区内盖章**——仅删
/// 锁外重盖章不足。旧代码在 `projection()` 顶部（等待镜像锁**之前**）读 caps/
/// policy；设置和证据变化不依赖该锁：缝内「关闭 + 真实重发布」整体越过本任务
///（重发布会推进序号，任务被锁挡在临界区外），本任务随后以**旧 caps/policy**
/// 进临界区——`set_approval_policy` 甚至把镜像策略写回旧值——并盖不晚于禁用事件
/// 的序号 → 禁用事件被去重丢弃，Desktop 静默后停留启用按钮。修复后 caps/policy
/// 与盖章同临界区：内容读到的是翻转后的世界，禁用快照本身到达下游。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sse_presnapshot_reads_cannot_swallow_concurrent_republish() {
    let mut f = t3_fixture();
    let mut peer = f.peer.take().unwrap();

    // —— 连接 #1：建连 + 跟随 + 装回 schema 档；卡启用（前置）——
    let (body1, first1) = sse_hold_ensure(
        &f,
        &mut peer,
        THREAD,
        vec![serde_json::json!({
            "id": 51, "method": "item/commandExecution/requestApproval",
            "params": { "command": "pnpm test", "cwd": "/w",
                        "availableDecisions": ["accept", "cancel"] }
        })],
        "active",
    )
    .await;
    assert_eq!(
        first1["pending"][0]["actionable"],
        serde_json::json!(true),
        "前置：schema 档启用（{first1}）"
    );
    drop(body1);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !f.state.codex_control.adapter.approval_policy().is_empty(),
        "前置：schema 档已装进连接身份（活值启用）"
    );

    // —— 测试缝：projection() 临界区**之前**插窗（旧代码 caps/policy 已在上面
    //    锁外读完；修复后读取在临界区内，本缝退化为「先跑伙伴再取快照」的无害
    //    定序）。一次性旗标同前。 ——
    let adapter = f.state.codex_control.adapter.clone();
    let store = f.state.store.clone();
    let fired = std::sync::atomic::AtomicBool::new(false);
    f.state
        .codex_control
        .adapter
        .set_projection_presnapshot_gate(Some(Arc::new(move || {
            if fired.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            t3_flip_off_and_wait_republish(&adapter, &store);
        })));

    // —— 被测连接 #2：重新跟随 + 首快照 ——
    let mut body2 = t3_drive_refollow_and_spawn_sse(&f, &mut peer).await;

    // —— 验收同主缺陷：禁用投影必须到达下游（旧代码：被同 revision/同序号去重
    //    丢弃 → 红）；此后 Desktop 静默。 ——
    use http_body_util::BodyExt;
    let mut saw_disabled = false;
    let mut saw_enabled_snapshot = false;
    for _ in 0..6 {
        let frame = match tokio::time::timeout(Duration::from_millis(1500), body2.frame()).await {
            Ok(Some(f)) => f.unwrap(),
            Ok(None) => break,
            Err(_) => break,
        };
        let Some(data) = frame.data_ref() else {
            continue;
        };
        let text = String::from_utf8(data.to_vec()).unwrap();
        let frame: serde_json::Value =
            serde_json::from_str(text.split("data:").nth(1).map(str::trim).unwrap_or("null"))
                .unwrap_or(serde_json::Value::Null);
        if frame.is_null() || frame["pending"].as_array().is_none_or(|a| a.is_empty()) {
            continue;
        }
        if frame["pending"][0]["actionable"] == serde_json::json!(true) {
            saw_enabled_snapshot = true;
            continue;
        }
        saw_disabled = true;
        break;
    }
    assert!(
        saw_disabled,
        "T3：仅删锁外重盖章不足——caps/policy 锁外先读 + 临界区内盖章同样吞掉真实\
         新策略（启用快照在场={saw_enabled_snapshot}）；内容读取与序号盖章必须同一\
         版本边界"
    );
    assert!(
        !peer_has_traffic(&mut peer).await,
        "此后 Desktop 静默（不以另有帧为收敛前提）"
    );
    drop(body2);
    f.state.codex_control.stop();
}

/// T3 fresh 路径（快路径内容一致性）：**fresh 快照的 caps 也必须与盖章同一版本
/// 边界**。镜像已新鲜时 `ensure_thread` 走 `fresh_projection`；旧代码 caps 在镜像
/// 锁外先读，缝内「关闭 + 真实重发布」整体越过本任务后，本任务以旧 caps 进临界
/// 区盖 ≥N 的序号——availability 还是旧世界（controllable），却声称「截至 N」，
/// 任何把卡压回 read_only 的真新事件都可被吞。修复后 caps 与盖章同临界区：内容
/// 观察到关闭（read_only）才允许携带 ≥N 的序号。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fresh_snapshot_caps_observed_inside_mirror_critical_section() {
    use multi_agents_manager_lib::services::codex_desktop_ipc::types::ControlAvailability;
    let mut f = t3_fixture();
    let mut peer = f.peer.take().unwrap();

    // —— 连接 #1：完整跟随 + 卡在场（holder 保活 = 镜像持续新鲜 → 快路径）——
    let (body1, first1) = sse_hold_ensure(
        &f,
        &mut peer,
        THREAD,
        vec![serde_json::json!({
            "id": 51, "method": "item/commandExecution/requestApproval",
            "params": { "command": "pnpm test", "cwd": "/w",
                        "availableDecisions": ["accept", "cancel"] }
        })],
        "active",
    )
    .await;
    assert_eq!(
        first1["pending"][0]["actionable"],
        serde_json::json!(true),
        "前置：schema 档启用（{first1}）"
    );
    assert!(
        !f.state.codex_control.adapter.approval_policy().is_empty(),
        "前置：schema 档在场"
    );

    // —— 测试缝：fresh_projection 的 caps 锁外先读窗口（一次性）。缝内完成伙伴
    //    动作并记录重发布事件的序号 N。 ——
    let adapter = f.state.codex_control.adapter.clone();
    let store = f.state.store.clone();
    let fired = std::sync::atomic::AtomicBool::new(false);
    let partner_seq = Arc::new(StdMutex::new(None));
    let seq_slot = partner_seq.clone();
    f.state
        .codex_control
        .adapter
        .set_fresh_snapshot_gate(Some(Arc::new(move || {
            if fired.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            let partner = t3_flip_off_and_wait_republish(&adapter, &store);
            *seq_slot.lock().unwrap() = Some(partner.local_seq);
        })));

    // —— 被测：快路径 ensure_thread（镜像新鲜）→ fresh_projection ——
    let key = f.thread_key();
    let fast = f
        .state
        .codex_control
        .adapter
        .ensure_thread(&key)
        .await
        .expect("快路径 ensure");
    let n = partner_seq
        .lock()
        .unwrap()
        .expect("伙伴事件序号已记录（缝内完成）");

    // —— 验收（不变式）：快照声称 seq ≥ N（「截至 N 的世界」）就必须观察到 N
    //    的内容（关闭 → send 压 false → read_only）。修复前：availability=旧世界
    //    controllable ∧ local_seq ≥ N → 旧内容盖更晚序号（红）。 ——
    assert!(
        fast.availability == ControlAvailability::ReadOnly || fast.local_seq < n,
        "fresh 快照内容必须与其序号一致（T3）：伙伴重发布事件 seq={n}，快照 \
         local_seq={}（≥{n} 即声称已观察到），availability={:?}——旧内容盖更晚序号，\
         真实新策略（read_only 化）可被去重吞掉",
        fast.local_seq,
        fast.availability
    );
    assert!(
        !fast.pending.is_empty() && !fast.pending[0].actionable,
        "策略清空（禁用卡）已被镜像吸收（伙伴 projection 推送）：{:?}",
        fast.pending
    );
    drop(body1);
    f.state.codex_control.stop();
}

/// 复审 round 4（V3，真实 SSE 定序）：**迟到的旧重发布不得靠发布点重新分配的
/// 序号覆盖新状态**。旧代码 `publish_event` 在实际发送时无条件分配更大
/// local_seq——重发布链先取好 payload（内容已盖章 seq=S），停在发布缝；窗内
/// 「关闭设置 + 新禁用策略发布」真实完成（seq=N>S）；放行后旧 payload（启用
/// 内容）被重新赋值 N+1，SSE 按序号认为它更新，恢复旧启用按钮；Desktop 随后
/// 静默 → 旧启用策略长期保留。修复后：观察已被后续发布越过（sealed < head）的
/// 重发布必须**重观察**再发布——内容版本表示实际观察到的状态，发送得晚不等于
/// 更新；迟到旧 payload 不得取得覆盖新状态的资格。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sse_late_stale_republish_cannot_override_completed_disable_publish() {
    use http_body_util::BodyExt;
    let mut f = t3_fixture();
    let mut peer = f.peer.take().unwrap();

    // —— 连接 #1：建连 + 跟随 + 装回 schema 档；卡启用（前置）——
    let (body1, first1) = sse_hold_ensure(
        &f,
        &mut peer,
        THREAD,
        vec![serde_json::json!({
            "id": 51, "method": "item/commandExecution/requestApproval",
            "params": { "command": "pnpm test", "cwd": "/w",
                        "availableDecisions": ["accept", "cancel"] }
        })],
        "active",
    )
    .await;
    assert_eq!(
        first1["pending"][0]["actionable"],
        serde_json::json!(true),
        "前置：schema 档启用（{first1}）"
    );
    // 关闭连接 #1 → 引用归零 unfollow → 镜像删除（被测连接走完整 follow）。
    drop(body1);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !f.state.codex_control.adapter.approval_policy().is_empty(),
        "前置：schema 档已装进连接身份（活值启用）"
    );

    // —— 被测连接 #2：重新跟随；SSE 基线 = 启用快照（R5/T3 同款驱动）——
    let mut body2 = t3_drive_refollow_and_spawn_sse(&f, &mut peer).await;

    // —— 发布缝：被测任务是 `notify_auto_adapt_changed(true)` 的**直发臂**重发布
    //    （第 2 次 republish_all），停在「payload 已取好（启用内容已盖章）、尚未
    //    发布」。缝内完成伙伴动作（真实写设置关闭 + 新禁用策略发布广播，seq=N
    //    已在轴上）再放行。装回臂（第 1 次）与伙伴自身（第 3 次起）照常放行。 ——
    let adapter = f.state.codex_control.adapter.clone();
    let store = f.state.store.clone();
    // 被测任务（notify(true) 的重发布）跑在 tauri runtime worker 上：缝回调在该
    // worker 内同步等待。伙伴动作必须在**别的线程/别的工作队列**上驱动——若在
    // 被阻塞 worker 上 `notify(false)`，其 spawn 的任务落该 worker 的 LIFO 槽，
    // tokio 不跨 worker 偷 LIFO 槽 → 伙伴任务饿死（实测取证）。故：独立 OS 线程
    // 经测试 runtime `block_on` 驱动真实产品路径（notify 的 spawn 自非 tauri
    // worker 上下文发出 → 落全局队列 → 空闲 worker 立即拾取）。
    let test_handle = tokio::runtime::Handle::current();
    let fire = std::sync::atomic::AtomicUsize::new(0);
    f.state
        .codex_control
        .adapter
        .set_republish_publish_gate(Some(Arc::new(move || {
            let n = fire.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n != 1 {
                return;
            }
            let adapter2 = adapter.clone();
            let store2 = store.clone();
            let h = test_handle.clone();
            let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
            std::thread::spawn(move || {
                h.block_on(async move {
                    store2.with(|c| {
                        c.execute(
                            "INSERT OR REPLACE INTO settings (key, value) VALUES ('remote.codex_auto_adapt', 'false')",
                            [],
                        )
                        .unwrap();
                    });
                    adapter2.notify_auto_adapt_changed(false);
                    // 阻塞至禁用重发布真正广播（发布序号已预留，seq=N 已在排序
                    // 轴上）；只认「卡禁用」帧。
                    let mut rx = adapter2.subscribe();
                    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
                    loop {
                        assert!(
                            tokio::time::Instant::now() < deadline,
                            "V3 伙伴：重发布未在时限内广播（测试自检失败，非被测行为）"
                        );
                        match tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
                            Ok(Ok(p)) => {
                                if p.pending.iter().any(|pd| !pd.actionable) {
                                    return;
                                }
                            }
                            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {
                                continue;
                            }
                            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                                panic!("V3 伙伴：投影广播通道关闭");
                            }
                            Err(_) => continue, // 50ms 无帧，继续有界等待
                        }
                    }
                });
                done_tx.send(()).unwrap();
            });
            done_rx
                .recv()
                .expect("V3 伙伴线程未在时限内完成（测试自检失败）");
        })));

    // —— 触发旧启用策略重发布（真实产品路径：设置通知 → 装回臂 + 直发臂）——
    f.state
        .codex_control
        .adapter
        .notify_auto_adapt_changed(true);

    // —— 验收①：禁用发布（seq=N）必须到达下游；此后任何启用帧都不得再出现
    //   （旧代码：迟到旧 payload 重赋 N+1 → SSE 认为更新 → 启用帧重现 = 红）。 ——
    let mut disabled_seen = false;
    let mut late_enabled = Option::<serde_json::Value>::None;
    for _ in 0..12 {
        let frame = match tokio::time::timeout(Duration::from_millis(2000), body2.frame()).await {
            Ok(Some(f)) => f.unwrap(),
            Ok(None) => break,
            Err(_) => break, // 有界窗口内无更多帧
        };
        let Some(data) = frame.data_ref() else {
            continue; // keep-alive 等非数据帧
        };
        let text = String::from_utf8(data.to_vec()).unwrap();
        let frame: serde_json::Value =
            serde_json::from_str(text.split("data:").nth(1).map(str::trim).unwrap_or("null"))
                .unwrap_or(serde_json::Value::Null);
        if frame.is_null() || frame["pending"].as_array().is_none_or(|a| a.is_empty()) {
            continue;
        }
        let actionable = frame["pending"][0]["actionable"] == serde_json::json!(true);
        if !disabled_seen {
            if !actionable {
                disabled_seen = true; // 窗内完成的禁用发布（seq=N）已到达
            }
            continue; // 禁用之前的启用帧（快照/装回臂）合法在先
        }
        if actionable {
            late_enabled = Some(frame); // 迟到旧 payload 覆盖新状态（缺陷）
            break;
        }
    }
    assert!(
        disabled_seen,
        "V3：缝内完成的禁用发布（seq=N）必须到达 SSE 下游（测试自检）"
    );
    assert!(
        late_enabled.is_none(),
        "V3：迟到的旧启用重发布不得覆盖已完成的禁用发布——旧代码发布点无条件重赋 \
         更大序号（N+1>N），SSE 认为它更新、恢复旧启用按钮，Desktop 静默后旧策略 \
         长期保留：{:?}",
        late_enabled
    );

    // —— 验收②：此后 Desktop 静默，状态保持禁用（不以另有帧为收敛前提）。 ——
    for _ in 0..6 {
        let frame = match tokio::time::timeout(Duration::from_millis(800), body2.frame()).await {
            Ok(Some(f)) => f.unwrap(),
            _ => break,
        };
        let Some(data) = frame.data_ref() else {
            continue;
        };
        let text = String::from_utf8(data.to_vec()).unwrap();
        let frame: serde_json::Value =
            serde_json::from_str(text.split("data:").nth(1).map(str::trim).unwrap_or("null"))
                .unwrap_or(serde_json::Value::Null);
        if frame.is_null() || frame["pending"].as_array().is_none_or(|a| a.is_empty()) {
            continue;
        }
        assert_ne!(
            frame["pending"][0]["actionable"],
            serde_json::json!(true),
            "V3：静默窗内不得重现启用状态：{frame}"
        );
    }
    assert!(
        !peer_has_traffic(&mut peer).await,
        "此后 Desktop 静默（不以另有帧为收敛前提）"
    );
    drop(body2);
    f.state.codex_control.stop();
}

/// 复审 round 5（W1/V3 收口，真实 SSE 定序）：**检查（refresh）通过之后、最终
/// 发布序号取得之前，仍是没有原子边界的裸窗**。round 4 的修复把重观察放在发布
/// 之前，但 refresh 通过与 `publish_event` 取号是两步——本缝
/// （`set_republish_publish_gate` 后置到「refresh 通过后、取号前」）钉住这一段：
/// refresh 通过（观察 = 当前头）→ 缝内「关闭设置 + 新禁用策略发布」真实完成
/// （seq=N）→ 放行 → 旧代码无条件 fetch_add 把旧启用 payload 发成 N+1，SSE 认为
/// 它更新、恢复旧启用按钮；Desktop 静默后旧策略长期保留。修复后：发布序号以
/// 观察版本 CAS 预留，观察被越过 → 重观察再发布（绝不以更晚序号发布更旧内容）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sse_publish_window_after_refresh_cannot_override_completed_disable_publish() {
    use http_body_util::BodyExt;
    let mut f = t3_fixture();
    let mut peer = f.peer.take().unwrap();

    // —— 连接 #1：建连 + 跟随 + 装回 schema 档；卡启用（前置，V3 同款）——
    let (body1, first1) = sse_hold_ensure(
        &f,
        &mut peer,
        THREAD,
        vec![serde_json::json!({
            "id": 51, "method": "item/commandExecution/requestApproval",
            "params": { "command": "pnpm test", "cwd": "/w",
                        "availableDecisions": ["accept", "cancel"] }
        })],
        "active",
    )
    .await;
    assert_eq!(
        first1["pending"][0]["actionable"],
        serde_json::json!(true),
        "前置：schema 档启用（{first1}）"
    );
    drop(body1);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !f.state.codex_control.adapter.approval_policy().is_empty(),
        "前置：schema 档已装进连接身份（活值启用）"
    );

    // —— 被测连接 #2：重新跟随；SSE 基线 = 启用快照（V3 同款驱动）——
    let mut body2 = t3_drive_refollow_and_spawn_sse(&f, &mut peer).await;

    // —— 发布缝（round 5 后置位）：被测任务是 `notify_auto_adapt_changed(true)`
    //    直发臂重发布，停在「refresh 已通过、publish_event 尚未取号」。缝内完成
    //    伙伴动作（真实写设置关闭 + 新禁用策略发布广播，seq=N 已在轴上）再放行。
    //    装回臂（第 1 次）与伙伴自身（第 3 次起）照常放行。伙伴动作经独立 OS
    //    线程 + 测试 runtime `block_on` 驱动（被阻塞 worker 的 LIFO 槽教训，V3
    //    同款）。 ——
    let adapter = f.state.codex_control.adapter.clone();
    let store = f.state.store.clone();
    let test_handle = tokio::runtime::Handle::current();
    let fire = std::sync::atomic::AtomicUsize::new(0);
    f.state
        .codex_control
        .adapter
        .set_republish_publish_gate(Some(Arc::new(move || {
            let n = fire.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n != 1 {
                return;
            }
            let adapter2 = adapter.clone();
            let store2 = store.clone();
            let h = test_handle.clone();
            let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
            std::thread::spawn(move || {
                h.block_on(async move {
                    store2.with(|c| {
                        c.execute(
                            "INSERT OR REPLACE INTO settings (key, value) VALUES ('remote.codex_auto_adapt', 'false')",
                            [],
                        )
                        .unwrap();
                    });
                    adapter2.notify_auto_adapt_changed(false);
                    // 阻塞至禁用重发布真正广播（发布序号已预留、seq=N 已在轴上）；
                    // 只认「卡禁用」帧。
                    let mut rx = adapter2.subscribe();
                    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
                    loop {
                        assert!(
                            tokio::time::Instant::now() < deadline,
                            "W1① 伙伴：重发布未在时限内广播（测试自检失败，非被测行为）"
                        );
                        match tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
                            Ok(Ok(p)) => {
                                if p.pending.iter().any(|pd| !pd.actionable) {
                                    return;
                                }
                            }
                            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {
                                continue;
                            }
                            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                                panic!("W1① 伙伴：投影广播通道关闭");
                            }
                            Err(_) => continue,
                        }
                    }
                });
                done_tx.send(()).unwrap();
            });
            done_rx
                .recv()
                .expect("W1① 伙伴线程未在时限内完成（测试自检失败）");
        })));

    // —— 触发旧启用策略重发布（真实产品路径：设置通知 → 装回臂 + 直发臂）——
    f.state
        .codex_control
        .adapter
        .notify_auto_adapt_changed(true);

    // —— 验收①：禁用发布（seq=N）必须到达下游；此后任何启用帧都不得再出现
    //   （旧代码：refresh 通过后无条件重赋 N+1 → SSE 认为更新 → 启用帧重现 = 红）。——
    let mut disabled_seen = false;
    let mut late_enabled = Option::<serde_json::Value>::None;
    for _ in 0..12 {
        let frame = match tokio::time::timeout(Duration::from_millis(2000), body2.frame()).await {
            Ok(Some(f)) => f.unwrap(),
            Ok(None) => break,
            Err(_) => break,
        };
        let Some(data) = frame.data_ref() else {
            continue;
        };
        let text = String::from_utf8(data.to_vec()).unwrap();
        let frame: serde_json::Value =
            serde_json::from_str(text.split("data:").nth(1).map(str::trim).unwrap_or("null"))
                .unwrap_or(serde_json::Value::Null);
        if frame.is_null() || frame["pending"].as_array().is_none_or(|a| a.is_empty()) {
            continue;
        }
        let actionable = frame["pending"][0]["actionable"] == serde_json::json!(true);
        if !disabled_seen {
            if !actionable {
                disabled_seen = true;
            }
            continue; // 禁用之前的启用帧（快照/装回臂）合法在先
        }
        if actionable {
            late_enabled = Some(frame); // 检查通过后的裸窗里旧 payload 取得更大序号（缺陷）
            break;
        }
    }
    assert!(
        disabled_seen,
        "W1①：缝内完成的禁用发布（seq=N）必须到达 SSE 下游（测试自检）"
    );
    assert!(
        late_enabled.is_none(),
        "W1①：refresh 通过后的发布窗内，迟到的旧启用 payload 不得取得比已完成的\
         禁用发布更大的序号——旧代码检查与取号两步无原子边界，无条件 fetch_add 把\
         旧内容发成 N+1，SSE 认为它更新：{:?}",
        late_enabled
    );

    // —— 验收②：此后 Desktop 静默，状态保持禁用（不以另有帧为收敛前提）。 ——
    for _ in 0..6 {
        let frame = match tokio::time::timeout(Duration::from_millis(800), body2.frame()).await {
            Ok(Some(f)) => f.unwrap(),
            _ => break,
        };
        let Some(data) = frame.data_ref() else {
            continue;
        };
        let text = String::from_utf8(data.to_vec()).unwrap();
        let frame: serde_json::Value =
            serde_json::from_str(text.split("data:").nth(1).map(str::trim).unwrap_or("null"))
                .unwrap_or(serde_json::Value::Null);
        if frame.is_null() || frame["pending"].as_array().is_none_or(|a| a.is_empty()) {
            continue;
        }
        assert_ne!(
            frame["pending"][0]["actionable"],
            serde_json::json!(true),
            "W1①：静默窗内不得重现启用状态：{frame}"
        );
    }
    assert!(
        !peer_has_traffic(&mut peer).await,
        "此后 Desktop 静默（不以另有帧为收敛前提）"
    );
    drop(body2);
    f.state.codex_control.stop();
}

/// 复审 round 5（W1/V3 收口，真实 SSE 定序）：**普通 state-changed 发布的等待窗
/// 同受内容版本协议约束**——复审原文指认的生产异步路径，不需要「仅靠短抢占」：
/// `handle_state_changed` 在镜像锁临界区内生成 fresh payload（启用内容）后释放
/// 锁，在 `cleanup_inflight_for(...).await` / 盖章 / 发布之间仍有等待窗。定序：
/// 缝内「关闭设置 + 新禁用策略发布」真实完成（seq=N，同 Desktop revision）→ 旧
/// handler 恢复，旧代码无条件 `publish_event` 把旧启用 payload 发成 N+1 → SSE
/// 认为它更新、恢复旧启用按钮；Desktop 静默后旧策略长期保留。修复后：发布序号
/// 以观察版本 CAS 预留，观察被越过 → 重观察再发布（新状态保持；后续无 Desktop
/// 帧也成立）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sse_state_changed_publish_window_cannot_override_completed_disable_publish() {
    use http_body_util::BodyExt;
    let mut f = t3_fixture();
    let mut peer = f.peer.take().unwrap();

    // —— 连接 #1：建连 + 跟随 + 装回 schema 档；卡启用（前置，V3 同款）——
    let (body1, first1) = sse_hold_ensure(
        &f,
        &mut peer,
        THREAD,
        vec![serde_json::json!({
            "id": 51, "method": "item/commandExecution/requestApproval",
            "params": { "command": "pnpm test", "cwd": "/w",
                        "availableDecisions": ["accept", "cancel"] }
        })],
        "active",
    )
    .await;
    assert_eq!(
        first1["pending"][0]["actionable"],
        serde_json::json!(true),
        "前置：schema 档启用（{first1}）"
    );
    drop(body1);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !f.state.codex_control.adapter.approval_policy().is_empty(),
        "前置：schema 档已装进连接身份（活值启用）"
    );

    // —— state-changed 发布缝（一次性，先于连接 #2 装配）：转发任务串行处理
    //    状态帧，故 fire#0 = 跟随轮 staging 帧（rev 7）的 handler、fire#1 = 被测
    //    rev 8 帧——定序确定。缝内伙伴动作：真实写设置关闭 + notify(false) 禁用
    //    重发布广播（seq=N，同 revision 8）完成后再放行。伙伴经独立 OS 线程驱动
    //   （被阻塞 worker 的 LIFO 槽教训，V3 同款）。 ——
    let adapter = f.state.codex_control.adapter.clone();
    let store = f.state.store.clone();
    let test_handle = tokio::runtime::Handle::current();
    let fire = std::sync::atomic::AtomicUsize::new(0);
    f.state
        .codex_control
        .adapter
        .set_state_publish_gate(Some(Arc::new(move || {
            let n = fire.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n != 1 {
                return;
            }
            let adapter2 = adapter.clone();
            let store2 = store.clone();
            let h = test_handle.clone();
            let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
            std::thread::spawn(move || {
                h.block_on(async move {
                    store2.with(|c| {
                        c.execute(
                            "INSERT OR REPLACE INTO settings (key, value) VALUES ('remote.codex_auto_adapt', 'false')",
                            [],
                        )
                        .unwrap();
                    });
                    adapter2.notify_auto_adapt_changed(false);
                    // 阻塞至禁用重发布真正广播（seq=N 已在轴上）；只认「卡禁用」帧。
                    let mut rx = adapter2.subscribe();
                    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
                    loop {
                        assert!(
                            tokio::time::Instant::now() < deadline,
                            "W1② 伙伴：重发布未在时限内广播（测试自检失败，非被测行为）"
                        );
                        match tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
                            Ok(Ok(p)) => {
                                if p.pending.iter().any(|pd| !pd.actionable) {
                                    return;
                                }
                            }
                            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {
                                continue;
                            }
                            Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                                panic!("W1② 伙伴：投影广播通道关闭");
                            }
                            Err(_) => continue,
                        }
                    }
                });
                done_tx.send(()).unwrap();
            });
            done_rx
                .recv()
                .expect("W1② 伙伴线程未在时限内完成（测试自检失败）");
        })));

    // —— 被测连接 #2：重新跟随；SSE 基线 = 启用快照（fire#0 在此路过）——
    let mut body2 = t3_drive_refollow_and_spawn_sse(&f, &mut peer).await;

    // —— 被测：真实 Desktop 状态帧（rev 8，同一张卡）驱动 handle_state_changed
    //    ——停在发布缝；伙伴禁用发布（seq=N）在窗内整体完成后放行。 ——
    push_snapshot(
        &mut peer,
        THREAD,
        8,
        "active",
        vec![serde_json::json!({
            "id": 52, "method": "item/commandExecution/requestApproval",
            "params": { "command": "pnpm test", "cwd": "/w",
                        "availableDecisions": ["accept", "cancel"] }
        })],
    )
    .await;

    // —— 验收①：禁用发布（seq=N）必须到达下游；此后任何启用帧都不得再出现
    //   （旧代码：等待窗后无条件发布旧启用 payload 为 N+1 → 启用帧重现 = 红）。 ——
    let mut disabled_seen = false;
    let mut late_enabled = Option::<serde_json::Value>::None;
    for _ in 0..12 {
        let frame = match tokio::time::timeout(Duration::from_millis(2000), body2.frame()).await {
            Ok(Some(f)) => f.unwrap(),
            Ok(None) => break,
            Err(_) => break,
        };
        let Some(data) = frame.data_ref() else {
            continue;
        };
        let text = String::from_utf8(data.to_vec()).unwrap();
        let frame: serde_json::Value =
            serde_json::from_str(text.split("data:").nth(1).map(str::trim).unwrap_or("null"))
                .unwrap_or(serde_json::Value::Null);
        if frame.is_null() || frame["pending"].as_array().is_none_or(|a| a.is_empty()) {
            continue;
        }
        let actionable = frame["pending"][0]["actionable"] == serde_json::json!(true);
        if !disabled_seen {
            if !actionable {
                disabled_seen = true;
            }
            continue; // 禁用之前的启用帧（快照/跟随轮 staging）合法在先
        }
        if actionable {
            late_enabled = Some(frame); // 等待窗后旧 payload 取得更大序号（缺陷）
            break;
        }
    }
    assert!(
        disabled_seen,
        "W1②：缝内完成的禁用发布（seq=N）必须到达 SSE 下游（测试自检）"
    );
    assert!(
        late_enabled.is_none(),
        "W1②：state-changed 等待窗（镜像锁已释放、cleanup 等待完成、发布序号未取得）\
         内完成的禁用发布不得被迟到的旧启用 payload 覆盖——旧代码发布点无条件重赋 \
         N+1，SSE 认为它更新：{:?}",
        late_enabled
    );

    // —— 验收②：此后 Desktop 静默，状态保持禁用（不以另有帧为收敛前提）。 ——
    for _ in 0..6 {
        let frame = match tokio::time::timeout(Duration::from_millis(800), body2.frame()).await {
            Ok(Some(f)) => f.unwrap(),
            _ => break,
        };
        let Some(data) = frame.data_ref() else {
            continue;
        };
        let text = String::from_utf8(data.to_vec()).unwrap();
        let frame: serde_json::Value =
            serde_json::from_str(text.split("data:").nth(1).map(str::trim).unwrap_or("null"))
                .unwrap_or(serde_json::Value::Null);
        if frame.is_null() || frame["pending"].as_array().is_none_or(|a| a.is_empty()) {
            continue;
        }
        assert_ne!(
            frame["pending"][0]["actionable"],
            serde_json::json!(true),
            "W1②：静默窗内不得重现启用状态：{frame}"
        );
    }
    assert!(
        !peer_has_traffic(&mut peer).await,
        "此后 Desktop 静默（不以另有帧为收敛前提）"
    );
    drop(body2);
    f.state.codex_control.stop();
}

// ============================================================
// 复审 round 6（Q1）：部分入口把旧 caps 绑定到更新的观察版本
// ============================================================

/// Q1 非阻塞读帧：预算内下一帧 ThreadProjection（无帧/流结束/非数据帧 → None，
/// 不 panic；keep-alive 在预算内跳过）。
async fn q1_next_sse(body: &mut axum::body::Body, budget: Duration) -> Option<serde_json::Value> {
    use http_body_util::BodyExt;
    let deadline = std::time::Instant::now() + budget;
    loop {
        let now = std::time::Instant::now();
        if now >= deadline {
            return None;
        }
        match tokio::time::timeout(deadline - now, body.frame()).await {
            Ok(Some(Ok(frame))) => {
                let Some(data) = frame.data_ref() else {
                    continue;
                };
                let text = String::from_utf8(data.to_vec()).unwrap();
                let v: serde_json::Value = serde_json::from_str(
                    text.split("data:").nth(1).map(str::trim).unwrap_or("null"),
                )
                .unwrap_or(serde_json::Value::Null);
                if v.is_null() {
                    continue;
                }
                return Some(v);
            }
            _ => return None,
        }
    }
}

/// Q1：帧 availability 是否为「旧 caps 回翻」形态（controllable / send_only）——
/// 兼容发送档未知 build 关闭自动适配后，有效 caps.send 必须 false → read_only；
/// 旧 caps（锁外读取）生成的 payload 会把 availability 回翻成 controllable。
fn q1_is_capability_rollback(frame: &serde_json::Value) -> bool {
    matches!(
        frame["availability"].as_str(),
        Some("controllable") | Some("send_only")
    )
}

/// Q1 缝内伙伴动作（独立 OS 线程 + 测试 runtime `block_on`，V3/W1 同款——被阻塞
/// worker 的 LIFO 槽教训）：真实写设置关闭 + `notify(false)` 禁用重发布，阻塞至
/// 禁用帧真正广播（seq=N 已在轴上，镜像策略已被清空）。同步函数，供缝回调直接调用。
fn q1_partner_disable_and_wait_broadcast(
    adapter: &Arc<DesktopIpcAdapter>,
    store: &DeviceStore,
    test_handle: &tokio::runtime::Handle,
) {
    let adapter2 = adapter.clone();
    let store2 = store.clone();
    let h = test_handle.clone();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        h.block_on(async move {
            store2.with(|c| {
                c.execute(
                    "INSERT OR REPLACE INTO settings (key, value) VALUES ('remote.codex_auto_adapt', 'false')",
                    [],
                )
                .unwrap();
            });
            adapter2.notify_auto_adapt_changed(false);
            // 阻塞至禁用重发布真正广播（seq=N 已在轴上）；只认「卡禁用」帧。
            let mut rx = adapter2.subscribe();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            loop {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "Q1 伙伴：禁用重发布未在时限内广播（测试自检失败，非被测行为）"
                );
                match tokio::time::timeout(Duration::from_millis(50), rx.recv()).await {
                    Ok(Ok(p)) => {
                        if p.pending.iter().any(|pd| !pd.actionable) {
                            return;
                        }
                    }
                    Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {
                        continue;
                    }
                    Ok(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                        panic!("Q1 伙伴：投影广播通道关闭");
                    }
                    Err(_) => continue,
                }
            }
        });
        done_tx.send(()).unwrap();
    });
    done_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("Q1 伙伴线程未在时限内完成（测试自检失败）");
}

/// Q1 观察边界验收循环：阶段①禁用帧必须先到（自检：availability=read_only）；
/// 阶段②其后任何帧 availability 不得回翻 controllable/send_only（回翻 = 旧 caps
/// 生成的 payload 取得更大序号覆盖新禁用状态，返回该帧 = 红）；阶段③静默窗内
/// 禁用状态保持（不以另有帧为收敛前提）。
async fn q1_assert_disable_holds(body2: &mut axum::body::Body) -> Option<serde_json::Value> {
    // 阶段①：禁用帧（缝内伙伴发布）必须到达下游
    let mut disabled_seen = false;
    for _ in 0..12 {
        let Some(frame) = q1_next_sse(body2, Duration::from_millis(2000)).await else {
            break;
        };
        if frame["pending"].as_array().is_some_and(|a| !a.is_empty())
            && frame["pending"][0]["actionable"] == serde_json::json!(false)
        {
            assert_eq!(
                frame["availability"],
                serde_json::json!("read_only"),
                "Q1 自检：禁用发布帧必须呈现 read_only（新禁用状态）：{frame}"
            );
            disabled_seen = true;
            break;
        }
        // 禁用之前的启用基线帧合法在先
    }
    assert!(
        disabled_seen,
        "Q1：缝内完成的禁用发布（seq=N）必须到达 SSE 下游（测试自检）"
    );
    // 阶段②：紧随其后的帧不得回翻
    if let Some(frame) = q1_next_sse(body2, Duration::from_millis(2500)).await {
        if q1_is_capability_rollback(&frame) {
            return Some(frame);
        }
    }
    // 阶段③：静默窗——禁用状态保持
    for _ in 0..6 {
        let Some(frame) = q1_next_sse(body2, Duration::from_millis(500)).await else {
            break;
        };
        assert!(
            !q1_is_capability_rollback(&frame),
            "Q1：静默窗内 availability 不得回翻（旧 caps 不得覆盖新禁用状态）：{frame}"
        );
    }
    None
}

/// Q1 公共前置（T3/W1 同款 staging）：连接 #1 建连 + 跟随 + 装回 schema 档（卡启用、
/// availability=controllable）→ 删镜像 → 连接 #2 重新跟随（SSE 基线 = 启用快照）。
async fn q1_stage_enabled_baseline(f: &Fixture, peer: &mut ScriptedPeer) -> axum::body::Body {
    let card = serde_json::json!({
        "id": 51, "method": "item/commandExecution/requestApproval",
        "params": { "command": "pnpm test", "cwd": "/w",
                    "availableDecisions": ["accept", "cancel"] }
    });
    let (body1, first1) = sse_hold_ensure(f, peer, THREAD, vec![card], "active").await;
    assert_eq!(
        first1["pending"][0]["actionable"],
        serde_json::json!(true),
        "前置：schema 档启用（{first1}）"
    );
    assert_eq!(
        first1["availability"],
        serde_json::json!("controllable"),
        "前置：兼容发送档在场（caps.send=true ∧ state=true）"
    );
    drop(body1);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !f.state.codex_control.adapter.approval_policy().is_empty(),
        "前置：schema 档已装进连接身份（活值启用）"
    );
    t3_drive_refollow_and_spawn_sse(f, peer).await
}

/// Q1 缺陷①（真实 SSE 定序，路径 = handle_state_changed fresh 发布）：caps 在镜像
/// 锁外读取后、尚未取得镜像锁时，窗内「关闭设置 + 新禁用策略发布 seq=N」整体完成；
/// 旧代码以锁外旧 caps 生成 payload、临界区内盖章 N、CAS(N→N+1) 合法成功——
/// availability 从 read_only 回翻 controllable，和 GET/提交门分裂。修复后 caps 与
/// 盖章同一临界区，放行后读取即新禁用状态（read_only 保持；后续无 Desktop 帧也成立）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sse_state_changed_caps_window_cannot_override_completed_disable_publish() {
    let mut f = t3_fixture();
    let mut peer = f.peer.take().unwrap();
    let mut body2 = q1_stage_enabled_baseline(&f, &mut peer).await;

    // —— caps 观察边界缝（fire#0 = 被测 rev 8 帧的 handler；后续火次直通）——
    let adapter = f.state.codex_control.adapter.clone();
    let store = f.state.store.clone();
    let test_handle = tokio::runtime::Handle::current();
    let fired = std::sync::atomic::AtomicBool::new(false);
    f.state
        .codex_control
        .adapter
        .set_caps_observe_gate(Some(Arc::new(move |path: &str| {
            if path != "state_changed" {
                return;
            }
            if fired.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            q1_partner_disable_and_wait_broadcast(&adapter, &store, &test_handle);
        })));

    // —— 被测：真实 Desktop 状态帧（rev 8）驱动 handle_state_changed，停在
    //    「caps 已读（锁外）、尚未取得镜像锁」；伙伴禁用发布在窗内整体完成后放行。——
    push_snapshot(
        &mut peer,
        THREAD,
        8,
        "active",
        vec![serde_json::json!({
            "id": 52, "method": "item/commandExecution/requestApproval",
            "params": { "command": "pnpm test", "cwd": "/w",
                        "availableDecisions": ["accept", "cancel"] }
        })],
    )
    .await;

    let rollback = q1_assert_disable_holds(&mut body2).await;
    assert!(
        rollback.is_none(),
        "Q1①：state-changed 的 caps 锁外读窗内完成的禁用发布不得被旧 caps payload \
         覆盖——旧代码 caps 在镜像锁外读取，窗内禁用发布（seq=N）完成后旧启用 payload \
         照旧盖章/预留 N+1，availability 回翻：{:?}",
        rollback
    );
    drop(body2);
    f.state.codex_control.stop();
}

/// Q1 缺陷①（路径 = publish_projection：need_recover 的 stale 发布 + 有界重拉）。
/// 未知 change 形态触发 need_recover → publish_projection 的 caps 锁外读窗内完成
/// 禁用发布 → 旧代码 stale payload 以旧 caps（controllable）覆盖新禁用（read_only）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sse_stale_publish_caps_window_cannot_override_completed_disable_publish() {
    let mut f = t3_fixture();
    let mut peer = f.peer.take().unwrap();
    let mut body2 = q1_stage_enabled_baseline(&f, &mut peer).await;

    let adapter = f.state.codex_control.adapter.clone();
    let store = f.state.store.clone();
    let test_handle = tokio::runtime::Handle::current();
    let fired = std::sync::atomic::AtomicBool::new(false);
    f.state
        .codex_control
        .adapter
        .set_caps_observe_gate(Some(Arc::new(move |path: &str| {
            if path != "publish_projection" {
                return;
            }
            if fired.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            q1_partner_disable_and_wait_broadcast(&adapter, &store, &test_handle);
        })));

    // —— 被测：未知 change 形态 → need_snapshot → publish_projection（stale 发布）
    //    停在 caps 观察窗；伙伴禁用发布窗内整体完成后放行。——
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": THREAD, "hostId": "local",
            "change": { "type": "unknown-resync", "revision": 9 }
        }
    }))
    .await;

    let rollback = q1_assert_disable_holds(&mut body2).await;
    assert!(
        rollback.is_none(),
        "Q1①：publish_projection（need_recover 的 stale 发布）的 caps 锁外读窗内完成的 \
         禁用发布不得被旧 caps payload 覆盖——availability 回翻即缺陷：{:?}",
        rollback
    );
    drop(body2);
    f.state.codex_control.stop();
}

/// Q1 缺陷①（路径 = handle_client_status_changed：owner 断开 → invalidate + 逐个
/// 发布 stale 投影）。caps 锁外读窗内完成禁用发布 → 旧代码失效投影以 controllable
/// 覆盖新禁用（read_only）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sse_client_status_caps_window_cannot_override_completed_disable_publish() {
    let mut f = t3_fixture();
    let mut peer = f.peer.take().unwrap();
    let mut body2 = q1_stage_enabled_baseline(&f, &mut peer).await;

    let adapter = f.state.codex_control.adapter.clone();
    let store = f.state.store.clone();
    let test_handle = tokio::runtime::Handle::current();
    let fired = std::sync::atomic::AtomicBool::new(false);
    f.state
        .codex_control
        .adapter
        .set_caps_observe_gate(Some(Arc::new(move |path: &str| {
            if path != "client_status_changed" {
                return;
            }
            if fired.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            q1_partner_disable_and_wait_broadcast(&adapter, &store, &test_handle);
        })));

    // —— 被测：owner 断开广播 → handle_client_status_changed 停在 caps 观察窗；
    //    伙伴禁用发布窗内整体完成后放行。——
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "client-status-changed", "version": 11,
        "params": { "clientId": "owner-1", "status": "disconnected" }
    }))
    .await;

    let rollback = q1_assert_disable_holds(&mut body2).await;
    assert!(
        rollback.is_none(),
        "Q1①：client-status-changed 的 caps 锁外读窗内完成的禁用发布不得被旧 caps \
         payload 覆盖（失效投影同样不得回翻 availability）：{:?}",
        rollback
    );
    drop(body2);
    f.state.codex_control.stop();
}

/// Q1 缺陷①（路径 = mark_all_mirrors_stale：ipc-connection-reset → 全体 stale +
/// 发布，保留视图只读）。caps 锁外读窗内完成禁用发布 → 旧代码 stale 投影以
/// controllable 覆盖新禁用（read_only）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sse_mark_all_stale_caps_window_cannot_override_completed_disable_publish() {
    let mut f = t3_fixture();
    let mut peer = f.peer.take().unwrap();
    let mut body2 = q1_stage_enabled_baseline(&f, &mut peer).await;

    let adapter = f.state.codex_control.adapter.clone();
    let store = f.state.store.clone();
    let test_handle = tokio::runtime::Handle::current();
    let fired = std::sync::atomic::AtomicBool::new(false);
    f.state
        .codex_control
        .adapter
        .set_caps_observe_gate(Some(Arc::new(move |path: &str| {
            if path != "mark_all_mirrors_stale" {
                return;
            }
            if fired.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            q1_partner_disable_and_wait_broadcast(&adapter, &store, &test_handle);
        })));

    // —— 被测：连接级 reset → mark_all_mirrors_stale 停在 caps 观察窗；伙伴禁用
    //    发布窗内整体完成后放行。——
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "ipc-connection-reset", "version": 11
    }))
    .await;

    let rollback = q1_assert_disable_holds(&mut body2).await;
    assert!(
        rollback.is_none(),
        "Q1①：mark_all_mirrors_stale 的 caps 锁外读窗内完成的禁用发布不得被旧 caps \
         payload 覆盖（全体 stale 发布同样不得回翻 availability）：{:?}",
        rollback
    );
    drop(body2);
    f.state.codex_control.stop();
}

// ============================================================
// 复审 round 6（Q1-W1 收口）：暂停 tracker「预留后、提交前」并发窗
// ============================================================

/// Q1-② 助手：真实打断端点发起暂停（peer 不回执 → RPC 250ms 超时 → UnknownDelivery
/// 200 uncertain；前置落行触发 republish）。
async fn q1_pause_via_interrupt(f: &Fixture, peer: &mut ScriptedPeer) -> serde_json::Value {
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/turn/interrupt",
            Some(COOKIE),
            Some(serde_json::json!({ "sessionId": THREAD, "turnId": "t-live" }).to_string()),
        ))
        .await
        .unwrap()
    });
    let frame = recv_within(peer, 10).await;
    assert_eq!(frame["method"], "thread-follower-interrupt-turn");
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("interrupt in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["uncertain"], serde_json::json!(true), "{v}");
    v
}

/// Q1-② 助手：真实 resume 端点（POST /codex/queue/resume）。
async fn q1_resume_via_endpoint(f: &Fixture) -> serde_json::Value {
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/queue/resume",
            Some(COOKIE),
            Some(serde_json::json!({ "sessionId": THREAD }).to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["ok"], serde_json::json!(true), "{v}");
    v
}

/// Q1-② 助手：等 SSE 出现指定 queuePaused 值（时限内未到 = panic）。
async fn q1_wait_queue_paused(
    sse: &mut axum::body::Body,
    paused: bool,
    what: &'static str,
) -> serde_json::Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "{what}（时限内未观察到 queuePaused={paused}）"
        );
        let Some(v) = q1_next_sse(sse, Duration::from_secs(2)).await else {
            continue;
        };
        if v["queuePaused"] == serde_json::json!(paused) {
            return v;
        }
    }
}

/// Q1 缺陷②（真实端点 + 真实 SSE 定序）：暂停 tracker 的「发布资格已预留（CAS
/// 成功）、tracker 尚未登记」并发窗。定序：P0 真实暂停 + R0 真实恢复（tracker 记录
/// false）→ A 真实暂停 P1 在预留缝停住（资格已取得、tracker 未登记）→ 缝内真实
/// resume R1：旧代码去重读旧 tracker=false 误判「无变化」退出，恢复事实被吞；A 放行
/// 后提交 true 并广播旧暂停——DB 已恢复而投影说暂停，tracker 停在 true → 第三次真实
/// 暂停 C（无 Desktop 新帧）被去重抑制、无通知（红）。修复后预留与登记同一临界区，
/// R1 读到待发布值照常发布恢复事实（tracker 回 false），C 照常通知（绿）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sse_pause_inflight_reserve_cannot_swallow_next_real_pause_notification() {
    let mut f = fixture_ipc_interrupt(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    // RPC 0.25s 超时 → 打断 UnknownDelivery；连接保持健康。**必须在首连之前注入**
    // （conn_tuning 在连接 spawn 时快照进连接——T4 的后置注入实际等满缺省 15s，
    // 本测三次打断，须真收窄）。
    f.state.codex_control.adapter.set_conn_tuning(S3ConnTuning {
        rpc_timeout: Duration::from_millis(250),
        ..Default::default()
    });
    let (mut sse, first) = sse_hold_ensure(&f, &mut peer, THREAD, vec![], "active").await;
    assert_eq!(
        first["queuePaused"],
        serde_json::json!(false),
        "首快照如实：未暂停（{first}）"
    );

    // —— 预留缝：只在**取得发布资格**时计数。fire#0 = P0 的 grant、fire#1 = R0 的
    //    grant、fire#2 = 被测 A 的 grant（停）；后续（含修复后窗内 R1 的 grant）直通。
    //    缝内 resume 经独立 OS 线程 + 测试 runtime block_on 驱动（W1 同款）。——
    let app = f.app.clone();
    let test_handle = tokio::runtime::Handle::current();
    let fire = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let fire2 = fire.clone();
    f.state
        .codex_control
        .adapter
        .set_pause_reserve_gate(Some(Arc::new(move || {
            let n = fire2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n != 2 {
                return;
            }
            let app2 = app.clone();
            let h = test_handle.clone();
            let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
            std::thread::spawn(move || {
                h.block_on(async move {
                    // 缝内真实 resume（R1）：clear_queue_pause → notify_settled →
                    // republish（去重判定在此读 tracker——旧代码读到未含 A 待提交
                    // 值的旧记录）。
                    let r = app2
                        .oneshot(req(
                            "POST",
                            "/m/api/v1/codex/queue/resume",
                            Some(COOKIE),
                            Some(serde_json::json!({ "sessionId": THREAD }).to_string()),
                        ))
                        .await
                        .unwrap();
                    assert_eq!(r.status(), 200, "缝内 resume 必须成功");
                });
                done_tx.send(()).unwrap();
            });
            done_rx
                .recv_timeout(Duration::from_secs(30))
                .expect("缝内 resume 未在时限内完成（测试自检失败）");
        })));

    // —— 步骤 0：P0 真实暂停 → tracker 空 → grant(fire#0) → 发布 true；R0 真实恢复
    //    → grant(fire#1) → 发布 false。tracker 记录 false（缝内 resume 去重判定的
    //    前提）。——
    q1_pause_via_interrupt(&f, &mut peer).await;
    q1_wait_queue_paused(&mut sse, true, "P0：真实暂停必须通知").await;
    q1_resume_via_endpoint(&f).await;
    q1_wait_queue_paused(&mut sse, false, "R0：真实恢复必须通知").await;

    // —— 步骤 1（被测窗）：A 真实暂停 P1 → grant(fire#2) 停在「已预留、未登记」；
    //    缝内 R1 真实恢复（旧代码：去重读 false 误抑制；新代码：照常发布 false）→
    //    放行 → A 提交 true 并广播（DB 已恢复，投影说暂停）。——
    let a = q1_pause_via_interrupt(&f, &mut peer).await;
    assert_eq!(
        a["queuePaused"],
        serde_json::json!(false),
        "A：DB 已被缝内 resume 恢复（Gate C #5 知情恢复优先），响应如实 false：{a}"
    );

    // —— 步骤 2：排干既有帧至静默（A 的旧暂停帧 / 新代码下 R1 的恢复帧都已消费）。
    while let Some(v) = q1_next_sse(&mut sse, Duration::from_millis(700)).await {
        let _ = v["queuePaused"].clone();
    }

    // —— 步骤 3：第三次真实暂停 C（无 Desktop 新帧）必须仍可通知。旧代码：tracker
    //    停在 A 的 true，C 的 republish 与 settle 重发布双双被去重抑制 → 无新帧（红）。
    let _c = q1_pause_via_interrupt(&f, &mut peer).await;
    let c_frame = q1_wait_queue_paused(
        &mut sse,
        true,
        "C 的真实暂停必须产生新的 queuePaused=true 通知（在途预留未纳入去重判定 = 缺陷）",
    )
    .await;
    assert_eq!(
        c_frame["queuePaused"],
        serde_json::json!(true),
        "C：真实暂停照常通知"
    );
    drop(sse);
    f.state.codex_control.stop();
}

// ============================================================
// 复审 round 7（Q1）：暂停重发布以缝前旧观察提交——同值恢复不作废旧观察
// ============================================================

/// round 7 Q1 缺陷定序（真实端点 + 真实 SSE）：去重判定用的暂停事实是
/// `projection()` 时点的旧观察，同值恢复（真实 resume 清行）不作废它。定序：
/// P0 真实暂停 + R0 真实恢复（tracker=false）→ 真实暂停 A 的 republish 在观察缝
/// 停住（已读到 queuePaused=true，grant 未进）→ B 真实 resume 完整跑完（与
/// tracker 同值 → Deduped，发布头不动）→ 放行 A：旧代码以旧观察 true 通过去重与
/// CAS，广播 stale true（DB 已恢复而最新投影说暂停）并把 tracker 登记成 true →
/// 第三次真实暂停 C 被同值去重吞掉（settle 因 paused=false 也不再纠偏，红）。
/// 修复后 grant 在受保护提交边界重读事实：A 重读 false==tracker → 正确去重（有界
/// 预算内无任何 true 帧）；C 重读 true≠tracker → 照常通知（绿）。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sse_pause_stale_observation_cannot_survive_same_value_resume() {
    let mut f = fixture_ipc_interrupt(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    // RPC 0.25s 超时 → 打断 UnknownDelivery；连接保持健康（round 6 同款，须在
    // 首连之前注入——conn_tuning 在连接 spawn 时快照进连接）。
    f.state.codex_control.adapter.set_conn_tuning(S3ConnTuning {
        rpc_timeout: Duration::from_millis(250),
        ..Default::default()
    });
    let (mut sse, first) = sse_hold_ensure(&f, &mut peer, THREAD, vec![], "active").await;
    assert_eq!(
        first["queuePaused"],
        serde_json::json!(false),
        "首快照如实：未暂停（{first}）"
    );

    // —— 前置：P0 真实暂停 → R0 真实恢复，tracker 记录 false（被测窗的去重前提；
    //    R0 之后所有 settle 重读 paused=false，不会再触发 republish，缝计数由此
    //    确定化）。——
    q1_pause_via_interrupt(&f, &mut peer).await;
    q1_wait_queue_paused(&mut sse, true, "P0：真实暂停必须通知").await;
    q1_resume_via_endpoint(&f).await;
    q1_wait_queue_paused(&mut sse, false, "R0：真实恢复必须通知").await;

    // —— 观察缝：R0 完成后注入 → fire#0 = 被测 A（停）；fire#1 = 缝内 B 的
    //    republish、fire#2 = C 的 republish（直通）。A 在缝内已完成观察与 refresh
    //    （读到 true），放行由测试在 B 完整跑完后经 channel 控制。——
    let fire = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let fire2 = fire.clone();
    let (arrive_tx, arrive_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let release_rx = StdMutex::new(release_rx);
    f.state
        .codex_control
        .adapter
        .set_pause_observe_gate(Some(Arc::new(move || {
            let n = fire2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n != 0 {
                return; // B / C 及后续 republish 直通
            }
            arrive_tx.send(()).unwrap(); // A 已到缝（观察完成、grant 未进）
            release_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(30))
                .expect("观察缝未在时限内放行（测试自检失败）");
        })));

    // —— 步骤 1：真实暂停 A。前置落行 → republish 在观察缝停住（端点整体停在
    //    republish await 内：interrupt RPC 尚未发出、响应未回、settle 未触发）。——
    let a_app = f.app.clone();
    let a_task = tokio::spawn(async move {
        a_app
            .oneshot(req(
                "POST",
                "/m/api/v1/codex/turn/interrupt",
                Some(COOKIE),
                Some(serde_json::json!({ "sessionId": THREAD, "turnId": "t-live" }).to_string()),
            ))
            .await
            .unwrap()
    });
    arrive_rx
        .recv_timeout(Duration::from_secs(15))
        .expect("A 未在时限内到达观察缝（前置落行后的 republish 应停在此处）");

    // —— 步骤 2：B 真实 resume 完整跑完（端点返回 = 其 republish 已收敛：与
    //    tracker 同值 → Deduped，发布头不动；DB 行已清）。——
    q1_resume_via_endpoint(&f).await;

    // —— 步骤 3：放行 A。旧代码以旧观察 true 通过去重与 CAS → 广播 stale true 并
    //    登记 tracker=true；修复后重读事实 false==tracker → Deduped 静默。——
    release_tx.send(()).unwrap();
    // A 的端点继续：interrupt RPC（250ms 超时）→ uncertain 响应；DB 已被 B 恢复，
    // queuePaused 回执如实 false（round 6 同款断言）。
    let frame = recv_within(&mut peer, 10).await;
    assert_eq!(frame["method"], "thread-follower-interrupt-turn");
    let a = tokio::time::timeout(Duration::from_secs(15), a_task)
        .await
        .expect("A interrupt in time")
        .unwrap();
    assert_eq!(a.status(), 200);
    let av: serde_json::Value = serde_json::from_str(&body_string(a).await).unwrap();
    assert_eq!(av["uncertain"], serde_json::json!(true), "{av}");
    assert_eq!(
        av["queuePaused"],
        serde_json::json!(false),
        "A：DB 已被 B 恢复，回执如实 false：{av}"
    );

    // —— 步骤 4（红测断言）：有界预算内不得出现 queuePaused=true 的帧。旧代码 A
    //    的 stale true 帧在此窗内到达（fail-fast）；修复后 A 正确去重 → 窗口静默
    //    收敛。既到帧的 queuePaused 也不得为 true。——
    while let Some(v) = q1_next_sse(&mut sse, Duration::from_millis(700)).await {
        assert_ne!(
            v["queuePaused"],
            serde_json::json!(true),
            "round7 Q1：同值恢复后放行的旧观察不得广播 queuePaused=true（DB 已恢复，\
             最新事实为未暂停）：{v}"
        );
    }

    // —— 步骤 5：第三次真实暂停 C（无 Desktop 新帧）必须照常通知。旧代码 tracker
    //    停在 A 的 stale true → C 被同值去重吞掉（无 true 帧，settle 也因
    //    paused=false 不纠偏 → 步骤 4 已先行红）；修复后重读 true≠tracker → 照常
    //    发布。——
    let _c = q1_pause_via_interrupt(&f, &mut peer).await;
    let c_frame = q1_wait_queue_paused(
        &mut sse,
        true,
        "C 的真实暂停必须产生新的 queuePaused=true 通知（旧观察存活 = 后续真实暂停被吞）",
    )
    .await;
    assert_eq!(
        c_frame["queuePaused"],
        serde_json::json!(true),
        "C：真实暂停照常通知"
    );
    drop(sse);
    f.state.codex_control.stop();
}

/// R2 对照（Gate F Major 2 第 6 点）：**已验证 build** 关闭自动适配 → availability
/// **不变**（Shipped 证据档不受开关连带影响——F3/F9/T6 语义：按已验证能力运行，
/// 设置只约束未知 build 的探测/取证/兼容授权）。重发布帧照发（下游收到真实状态），
/// 但 availability 恒 controllable。
#[tokio::test]
async fn r2_shipped_build_close_keeps_availability_unchanged() {
    use http_body_util::BodyExt;
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    // SSE 持引用 + 持住响应体（流保持打开 = follow 引用在持）
    let (mut body, _first) = sse_hold_ensure(
        &f,
        &mut peer,
        THREAD,
        vec![serde_json::json!({
            "id": 63, "method": "item/commandExecution/requestApproval",
            "params": { "command": "pnpm test", "cwd": "/w",
                        "availableDecisions": ["accept", "cancel"] }
        })],
        "active",
    )
    .await;

    // 关闭（经 toggle 链通知适配器）→ 重发布照发（下游收真实状态）
    f.state.store.with(|c| {
        c.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('remote.codex_auto_adapt', 'false')",
            [],
        )
        .unwrap();
    });
    f.state
        .codex_control
        .adapter
        .notify_auto_adapt_changed(false);

    // 有界读帧：出现带 pending 的帧即断言 availability 恒 controllable（不翻转）
    let mut saw_frame = false;
    for _ in 0..10 {
        let updated = match tokio::time::timeout(Duration::from_secs(5), body.frame()).await {
            Ok(f) => f.unwrap().unwrap(),
            Err(_) => break,
        };
        let text = String::from_utf8(updated.data_ref().unwrap().to_vec()).unwrap();
        let frame: serde_json::Value =
            serde_json::from_str(text.split("data:").nth(1).map(str::trim).unwrap_or("null"))
                .unwrap();
        if frame["pending"].as_array().is_none_or(|a| a.is_empty()) {
            continue;
        }
        saw_frame = true;
        assert_eq!(
            frame["availability"],
            serde_json::json!("controllable"),
            "已验证 build 关闭后 availability 不得连带翻转：{frame}"
        );
        break;
    }
    assert!(saw_frame, "重发布帧应已到达（下游收生产真实状态）");
    drop(body);
}

/// Gate F 收尾红测（事件驱动发布位点）：关闭自动适配后，任意后续 Desktop 状态帧
/// （回合完成/状态变化等常规流量）的重发布**仍 read_only**——红轮：事件驱动位点
/// （handle_state_changed 等）仍用裸 compat caps → availability 翻回 controllable
/// （生产可达、无覆盖）。
#[tokio::test]
async fn gf_state_frame_republish_after_close_stays_read_only() {
    use multi_agents_manager_lib::services::codex_desktop_ipc::probe::schema_probe::{
        SchemaEvidence, SchemaProbeOutcome,
    };

    let (io, peer) = ScriptedPeer::new();
    let adapter = Arc::new(DesktopIpcAdapter::new(
        Arc::new(StaticConnector::with_build(io, UNKNOWN_BUILD)),
        ProtocolProfile::unverified("windows", UNKNOWN_BUILD),
    ));
    let mut f = fixture_with(
        vec![codex_app_session(THREAD)],
        |store| Arc::new(CodexControlHub::with_adapter(store, adapter.clone(), true)),
        Some(peer),
    );
    let ledger_conn = f.conn.clone();
    f.state
        .codex_control
        .adapter
        .set_probe_ledger(Arc::new(SharedConnLedger(ledger_conn)));
    f.state
        .codex_control
        .adapter
        .set_probe_key_override(Some(COMPAT_PROBE_KEY.into()));
    f.state
        .codex_control
        .adapter
        .set_schema_probe_override(Some(SchemaProbeOutcome::Known(SchemaEvidence {
            fingerprint: "fp-gf".into(),
            command_decisions: vec!["accept".into(), "cancel".into()],
            file_decisions: vec!["accept".into(), "cancel".into()],
        })));
    // 读链证据预置（探针零帧短路——只读探针的临时 follow/空快照会清掉镜像
    // pending，破坏本测试的翻转帧断言；与 F5/r6 预置同因）
    f.conn
        .lock()
        .unwrap()
        .execute_batch(&format!(
            "INSERT OR REPLACE INTO codex_ipc_probe
                (probe_key, capability, state, probe_stage, attempts,
                 first_seen_at, last_seen_at, evidence_at, evidence_stage)
             VALUES ('{COMPAT_PROBE_KEY}', 'state', 'probed', 'P3', 1, 1, 1, 1, 'P3')",
        ))
        .unwrap();
    let gate_store = f.state.store.clone();
    f.state
        .codex_control
        .adapter
        .set_auto_adapt_gate(Some(Arc::new(move || {
            gate_store.with(multi_agents_manager_lib::remote::auto_adapt_enabled_conn)
        })));

    // 开启态活连接 + 可操作卡（SSE 持引用）
    let mut peer = f.peer.take().unwrap();
    use http_body_util::BodyExt;
    let (mut body, _first) = sse_hold_ensure(
        &f,
        &mut peer,
        THREAD,
        vec![serde_json::json!({
            "id": 71, "method": "item/commandExecution/requestApproval",
            "params": { "command": "pnpm test", "cwd": "/w",
                        "availableDecisions": ["accept", "cancel"] }
        })],
        "active",
    )
    .await;

    // 关闭（经 toggle 链通知适配器：重发布翻转 + 后续状态帧也 read_only）
    f.state.store.with(|c| {
        c.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('remote.codex_auto_adapt', 'false')",
            [],
        )
        .unwrap();
    });
    f.state
        .codex_control
        .adapter
        .notify_auto_adapt_changed(false);

    // 排水：等关闭重发布帧（read_only）到达
    let mut saw_read_only = false;
    for _ in 0..10 {
        let updated = match tokio::time::timeout(Duration::from_secs(5), body.frame()).await {
            Ok(f) => f.unwrap().unwrap(),
            Err(_) => break,
        };
        let text = String::from_utf8(updated.data_ref().unwrap().to_vec()).unwrap();
        let frame: serde_json::Value =
            serde_json::from_str(text.split("data:").nth(1).map(str::trim).unwrap_or("null"))
                .unwrap();
        if frame["pending"].as_array().is_none_or(|a| a.is_empty()) {
            continue;
        }
        if frame["availability"] == serde_json::json!("read_only") {
            saw_read_only = true;
            break;
        }
    }
    assert!(saw_read_only, "关闭重发布帧应为 read_only");

    // —— 后续 Desktop 状态帧（常规流量：回合完成 → idle 快照，requests 清空）→
    // 事件驱动重发布的 availability 必须仍 read_only（红轮：裸 caps 翻回
    // controllable）。pending 已被清空是快照的真实状态——验收点是 availability
    // 本身，不再要求卡在场。
    push_snapshot(&mut peer, THREAD, 30, "idle", vec![]).await;
    let mut checked = false;
    for _ in 0..10 {
        let updated = match tokio::time::timeout(Duration::from_secs(5), body.frame()).await {
            Ok(f) => f.unwrap().unwrap(),
            Err(_) => break,
        };
        let text = String::from_utf8(updated.data_ref().unwrap().to_vec()).unwrap();
        let frame: serde_json::Value =
            serde_json::from_str(text.split("data:").nth(1).map(str::trim).unwrap_or("null"))
                .unwrap();
        if frame["revision"].is_null() {
            continue; // 无镜像缺省帧：非本验收对象
        }
        checked = true;
        assert_eq!(
            frame["availability"],
            serde_json::json!("read_only"),
            "Gate F 收尾：状态帧重发布不得翻回 controllable：{frame}"
        );
        break;
    }
    assert!(checked, "状态帧后的重发布未到达");
    drop(body);
}

// ============================================================
// T04：契约匹配放行阻塞问答（未知 build）与秘密答案隐私
// ============================================================

/// 未知 build + 契约匹配：三题多值一帧正式响应（**不是** start-turn／steer）。
#[tokio::test]
async fn f08p_t04_unknown_build_blocking_multi_is_formal_response() {
    let mut f = fixture_ipc_unknown_blocking(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let (rk, tk) = follow_with_pending(
        &f,
        &mut peer,
        vec![serde_json::json!({
            "id": 187, "method": "item/tool/requestUserInput",
            "params": { "questions": [
                { "id": "m1", "header": "多选", "question": "选多项",
                  "isOther": true, "isSecret": false,
                  "options": [ {"label": "甲", "description": ""},
                               {"label": "乙", "description": ""},
                               {"label": "丙", "description": ""} ] },
                { "id": "m2", "header": "单选", "question": "选一个",
                  "isOther": false, "isSecret": false,
                  "options": [ {"label": "是", "description": ""},
                               {"label": "否", "description": ""} ] },
                { "id": "m3", "header": "自由", "question": "城市",
                  "isOther": true, "isSecret": false,
                  "options": [ {"label": "北京", "description": ""},
                               {"label": "上海", "description": ""} ] }
            ]}
        })],
    )
    .await;
    let body = serde_json::json!({
        "sessionId": THREAD, "actionId": "act-t04-multi",
        "requestKey": rk, "contextToken": tk,
        "action": "answer",
        "answers": { "m1": ["甲", "丙"], "m2": ["是"], "m3": ["自填文本"] },
    });
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/pending-response",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
    });
    let frame = recv_within(&mut peer, 10).await;
    assert_eq!(frame["method"], "thread-follower-submit-user-input");
    assert_eq!(frame["version"], 1);
    assert_eq!(
        frame["params"]["response"]["answers"]["m1"]["answers"],
        serde_json::json!(["甲", "丙"]),
        "多值原样记账（已核验数组契约）"
    );
    assert_eq!(
        frame["params"]["response"]["answers"]["m3"]["answers"],
        serde_json::json!(["自填文本"]),
        "isOther=true 允许自由文本"
    );
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-submit-user-input",
        "result": { "ok": true }
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("respond in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    // 不进普通消息 FIFO、不走 start-turn：之后没有任何帧。
    assert!(
        !peer_has_traffic(&mut peer).await,
        "阻塞问答不得产生 start-turn／普通消息帧"
    );
}

/// `isSecret` 答案原文绝不进审计（诊断只记动作类别/阶段/错误码）。
#[tokio::test]
async fn f08p_t04_is_secret_never_in_audit() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    let (rk, tk) = follow_with_pending(
        &f,
        &mut peer,
        vec![serde_json::json!({
            "id": "s:secret-1", "method": "item/tool/requestUserInput",
            "params": { "questions": [
                { "id": "k1", "header": "密钥", "question": "输入密钥",
                  "isOther": true, "isSecret": true, "options": [] }
            ]}
        })],
    )
    .await;
    let secret = "TUVIS-SECRET-T04-9f3a";
    let body = serde_json::json!({
        "sessionId": THREAD, "actionId": "act-t04-secret",
        "requestKey": rk, "contextToken": tk,
        "action": "answer",
        "answers": { "k1": [secret] },
    });
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/pending-response",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
    });
    let frame = recv_within(&mut peer, 10).await;
    assert_eq!(
        frame["params"]["response"]["answers"]["k1"]["answers"][0],
        secret
    );
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-submit-user-input",
        "result": { "ok": true }
    }))
    .await;
    let _ = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("respond in time")
        .unwrap();
    for row in f.codex_audits() {
        assert!(
            !row.summary.contains(secret),
            "isSecret 答案不得进入审计：{}",
            row.summary
        );
    }
}

// ============================================================
// T06：逐能力定向打断（契约匹配放行 + 无证据/关闭适配零帧）
// ============================================================

/// 未知 build + 契约匹配：打断发 v4 + expectedTurnId（静态 caps.interrupt=false）。
#[tokio::test]
async fn f08p_t06_unknown_build_interrupt_v4_targeted() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], true);
    let mut peer = f.peer.take().unwrap();
    assert!(
        !f.state.codex_control.adapter.capabilities().interrupt,
        "前提：静态能力位为 false（证明放行来自契约匹配）"
    );
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "active").await;
    let _ = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("ensure in time")
        .unwrap();
    // 连接建立后到达的契约识别结果（与生产 ensure 的安装点同构：绑定当前代际）。
    install_interrupt_contract(&f);
    let body = serde_json::json!({ "sessionId": THREAD, "turnId": "turn-a" });
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/turn/interrupt",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
    });
    let frame = recv_within(&mut peer, 10).await;
    assert_eq!(frame["method"], "thread-follower-interrupt-turn");
    assert_eq!(frame["version"], 4, "不降级 v3");
    assert_eq!(frame["params"]["expectedTurnId"], "turn-a");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-interrupt-turn",
        "result": { "interruptedTurnId": "turn-a", "ok": true }
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("interrupt in time")
        .unwrap();
    assert_eq!(r.status(), 200);
}

/// 无契约证据 / 关闭自动适配 / 无活动回合 → 503 零帧，且**不留下暂停**。
#[tokio::test]
async fn f08p_t06_refused_interrupts_are_zero_frame_and_pause_free() {
    // ① 未知 build 无契约 → 拒
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "active").await;
    let _ = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("ensure in time")
        .unwrap();
    let body = serde_json::json!({ "sessionId": THREAD, "turnId": "turn-a" });
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/turn/interrupt",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 503);
    assert!(!peer_has_traffic(&mut peer).await, "拒绝不得写出帧");
    let key = ThreadKey {
        host_id: "local".into(),
        thread_id: THREAD.into(),
    };
    assert!(
        f.state
            .codex_control
            .router
            .store()
            .queue_pause(&key)
            .unwrap()
            .is_none(),
        "拒绝的打断不得留下队列暂停"
    );

    // ② 契约匹配但关闭自动适配 → 撤回（零帧）
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], true);
    let mut peer = f.peer.take().unwrap();
    f.state
        .codex_control
        .adapter
        .set_auto_adapt_gate(Some(Arc::new(|| false)));
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "active").await;
    let _ = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("ensure in time")
        .unwrap();
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/turn/interrupt",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 503);
    assert!(!peer_has_traffic(&mut peer).await);
}

// ============================================================
// T07：空闲异步正式答题（独立动作；一次 canonical start-turn）
// ============================================================

/// 建立带**异步题**的投影：snapshot 里 agentMessage 带 `questions[]`。返回
/// (questionKey, contextToken)。
async fn follow_with_async_question(
    f: &Fixture,
    peer: &mut ScriptedPeer,
    runtime: &str,
) -> (String, String) {
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    handshake(peer, "owner-1").await;
    let disc = peer.recv().await;
    assert_eq!(disc["method"], "thread-owner-discovery");
    peer.send(&discovery_response(
        disc["requestId"].as_str().unwrap(),
        "owner-1",
    ))
    .await;
    let follow = peer.recv().await;
    assert_eq!(follow["method"], "thread-stream-following-changed");
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": THREAD, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": 9,
                "conversationState": {
                    "turns": [], "requests": [],
                    "threadRuntimeStatus": { "type": runtime },
                    "turnHistory": {"history": {
                        "isComplete": true,
                        "entitiesByKey": {"turn:t-q": {
                            "turnId": "t-q",
                            "status": "completed",
                            "items": [{
                                "type": "agentMessage",
                                "id": "call-q",
                                "text": "请选择",
                                "phase": "final_answer",
                                "delivery": "async",
                                "questions": [{"title": "选一个", "options": ["甲", "乙"]}]
                            }]
                        }},
                        "islands": [{"id": "i", "entries": [
                            {"key": "turn:t-q", "value": "turn:t-q"}
                        ]}]
                    }}
                }
            }
        }
    }))
    .await;
    let _ = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("ensure in time")
        .unwrap();
    // 连接建立后安装当前包契约（AsyncAnswerIdle + Send 的 v2 start-turn）。
    install_async_answer_contract(f);
    // 再取一次投影：此时 asyncQuestions 已按新证据给出 Pending + token。
    let r = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    let question = &v["asyncQuestions"][0];
    (
        question["questionKey"].as_str().unwrap().to_string(),
        question["contextToken"].as_str().unwrap().to_string(),
    )
}

fn install_async_answer_contract(f: &Fixture) {
    use multi_agents_manager_lib::services::codex_desktop_ipc::capability::{
        CodexOperation, ContractMatch, DesktopContractEvidence, CONTRACT_RULE_VERSION,
    };
    let mut matches = std::collections::BTreeMap::new();
    matches.insert(
        CodexOperation::Send,
        ContractMatch {
            method: "thread-follower-start-turn".into(),
            base_version: 2,
            request_rule: "conversationId+turnStart".into(),
            response_rule: "result.result".into(),
            semantic_rule: "start-turn v2".into(),
        },
    );
    matches.insert(
        CodexOperation::AsyncAnswerIdle,
        ContractMatch {
            method: "thread-follower-start-turn".into(),
            base_version: 2,
            request_rule: "conversationId+turnStart{text=envelope}".into(),
            response_rule: "result.result".into(),
            semantic_rule: "questionItemId 合成 + 单条 min(1)".into(),
        },
    );
    f.state
        .codex_control
        .adapter
        .set_contract_evidence_snapshot(DesktopContractEvidence {
            probe_key: "macos|26.1021.31345|cli|1".into(),
            package_sha256: "c".repeat(64),
            rule_version: CONTRACT_RULE_VERSION,
            matches,
            package_size: None,
            package_mtime_ms: None,
        });
}

/// 空闲正式答题 = **一次** canonical start-turn（官方信封正文），零 steer、零 FIFO。
#[tokio::test]
async fn f08p_t07_idle_answer_is_one_canonical_start_turn() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let (question_key, context_token) = follow_with_async_question(&f, &mut peer, "idle").await;
    let body = serde_json::json!({
        "sessionId": THREAD,
        "actionId": "act-async-1",
        "questionKey": question_key,
        "contextToken": context_token,
        "answer": "甲",
    });
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
    });
    let frame = recv_within(&mut peer, 10).await;
    assert_eq!(frame["method"], "thread-follower-start-turn");
    assert_eq!(frame["version"], 2);
    let text = frame["params"]["turnStart"]["request"]["input"][0]["text"]
        .as_str()
        .unwrap();
    assert!(
        text.starts_with("<send_user_message_question_reply>"),
        "正式信封：{text}"
    );
    let parsed = multi_agents_manager_lib::services::codex_desktop_ipc::async_question::parse_answer_envelope(text)
        .expect("official envelope");
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].question_item_id, question_key);
    assert_eq!(parsed[0].answer, "甲");
    assert_eq!(parsed[0].question, "选一个");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "result": {"result": {"turn": {"id": "turn-answer", "status": "inProgress"}}}
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("answer in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["delivery"], "accepted");
    // 不进普通消息 FIFO、不用 steer：没有后续帧。
    assert!(
        !peer_has_traffic(&mut peer).await,
        "不得产生 steer／普通消息帧"
    );
    let key = ThreadKey {
        host_id: "local".into(),
        thread_id: THREAD.into(),
    };
    assert!(
        f.hub_store().next_queued(&key).unwrap().is_none(),
        "异步答题不得留在普通消息 FIFO"
    );
    let row = f
        .hub_store()
        .get_for_device("act-async-1", "dev-1")
        .unwrap()
        .unwrap();
    // 账本视图不暴露 kind 字段（隐私/最小面）：动作类别由账本行验证——
    // Accepted 终态 + 独立动作 id 足以证明走的是独立动作链（FIFO 断言见上）。
    assert_eq!(row.state, ActionState::Accepted);
}

/// 运行中（busy）提交：零派发、保稿语义（409），不退回 FIFO、不自动重发。
#[tokio::test]
async fn f08p_t07_busy_answer_is_zero_dispatch() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let (question_key, context_token) = follow_with_async_question(&f, &mut peer, "active").await;
    let body = serde_json::json!({
        "sessionId": THREAD,
        "actionId": "act-async-busy",
        "questionKey": question_key,
        "contextToken": context_token,
        "answer": "甲",
    });
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap();
    // 运行中：投影侧题目应显示"暂不可提交"（temporarily_blocked），HTTP 明确拒发。
    assert!(
        r.status() == 409 || r.status() == 503 || r.status() == 200,
        "must be an explicit refusal, got {}",
        r.status()
    );
    assert!(!peer_has_traffic(&mut peer).await, "运行中零派发");
    let key = ThreadKey {
        host_id: "local".into(),
        thread_id: THREAD.into(),
    };
    assert!(f.hub_store().next_queued(&key).unwrap().is_none());
}

/// 正式答题接口**不接受**任意正文/信封：多余键（raw/method/owner/attachments）400，
/// 超长答案零派发。
#[tokio::test]
async fn f08p_t07_async_answer_rejects_raw_body_and_oversize() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let (question_key, context_token) = follow_with_async_question(&f, &mut peer, "idle").await;
    for extra in [
        serde_json::json!({ "raw": "<send_user_message_question_reply>" }),
        serde_json::json!({ "method": "thread-follower-start-turn" }),
        serde_json::json!({ "ownerClientId": "owner-1" }),
        serde_json::json!({ "attachments": [] }),
    ] {
        let mut body = serde_json::json!({
            "sessionId": THREAD,
            "actionId": "act-async-extra",
            "questionKey": question_key,
            "contextToken": context_token,
            "answer": "甲",
        });
        for (key, value) in extra.as_object().unwrap() {
            body[key] = value.clone();
        }
        let r = f
            .app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/codex/async-answer",
                Some(COOKIE),
                Some(body.to_string()),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "多余键必须拒绝：{extra}");
    }
    // 超长答案（>10_000 字符）→ 明确零投递。
    let body = serde_json::json!({
        "sessionId": THREAD,
        "actionId": "act-async-long",
        "questionKey": question_key,
        "contextToken": context_token,
        "answer": "x".repeat(10_001),
    });
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap();
    // 超限 = **明确的零投递回执**（NotSent + 机器码），不是"已发出但未知"。
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["delivery"], "not_sent");
    assert_eq!(v["reasonCode"], "answer_too_long");
    assert!(!peer_has_traffic(&mut peer).await, "非法答案零派发");
}

// ============================================================
// T08：适配详情/开关（严格白名单、同源设置、无隐藏写）
// ============================================================

/// 鉴权 + 请求体白名单：未配对设备 403；任意设置键/action 组合 400；非 Desktop
/// 会话 404；全部拒绝路径零 IPC 帧、零设置写入。
#[tokio::test]
async fn f08p_t08_auth_and_body_allowlist() {
    let mut f = fixture_ipc(vec![codex_app_session(THREAD)]);
    let mut peer = f.peer.take().unwrap();
    // ① 未配对（无 cookie）→ 403
    let r = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/adapt-status?sessionId={THREAD}"),
            None,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    // ② 任意设置键 → 400（deny_unknown_fields）
    for body in [
        serde_json::json!({ "sessionId": THREAD, "action": "set_enabled", "enabled": true, "value": "x" }),
        serde_json::json!({ "sessionId": THREAD, "action": "set_other" }),
        serde_json::json!({ "sessionId": THREAD, "action": "set_enabled" }),
        serde_json::json!({ "sessionId": THREAD, "action": "dismiss_hint", "enabled": true }),
    ] {
        let r = f
            .app
            .clone()
            .oneshot(req(
                "POST",
                "/m/api/v1/codex/adapt-settings",
                Some(COOKIE),
                Some(body.to_string()),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 400, "非法组合必须拒绝：{body}");
    }
    // ③ 非 Desktop 会话 → 404（不推断会话形态）
    let cli_session = {
        let mut s = codex_app_session(THREAD);
        s.form = multi_agents_manager_lib::session::ProcessForm::Cli;
        s
    };
    let f2 = fixture_ipc(vec![cli_session]);
    let r = f2
        .app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/adapt-status?sessionId={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    // 零 IPC 帧（拒绝路径不触发任何 Desktop 动作）
    assert!(!peer_has_traffic(&mut peer).await);
}

/// 开关与桌面设置同源：关闭后详情翻 false，且适配出的逐操作能力立即撤回；
/// 全程**零 Desktop 帧**（本地状态可靠传播，不依赖桌面再发一帧）。
#[tokio::test]
async fn f08p_t08_close_reopen_updates_other_device_without_peer_frame() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let _ = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("ensure in time")
        .unwrap();
    install_async_answer_contract(&f);
    // 首次详情：ops 有契约证据（async_answer_idle allowed）
    let r = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/adapt-status?sessionId={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let before: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    // 先把开关显式打开（缺省值取决于本机设置；测试不假定全局缺省）。
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/adapt-settings",
            Some(COOKIE),
            Some(
                serde_json::json!({
                    "sessionId": THREAD, "action": "set_enabled", "enabled": true
                })
                .to_string(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let enabled: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(enabled["autoAdaptEnabled"], true);
    assert_eq!(
        enabled["effectiveOperations"]["operations"]["async_answer_idle"]["allowed"], true,
        "契约匹配 + 适配开启时可用：{enabled}"
    );
    assert_eq!(
        before["effectiveOperations"]["operations"]["async_answer_idle"]["allowed"], true,
        "契约匹配时异步答题可用：{before}"
    );
    // 关闭自动适配（移动端路径）→ 立即撤回
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/adapt-settings",
            Some(COOKIE),
            Some(
                serde_json::json!({
                    "sessionId": THREAD, "action": "set_enabled", "enabled": false
                })
                .to_string(),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let after: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(after["autoAdaptEnabled"], false);
    assert_eq!(
        after["effectiveOperations"]["operations"]["async_answer_idle"]["allowed"], false,
        "关闭适配必须立即撤回适配动作：{after}"
    );
    assert_eq!(
        after["effectiveOperations"]["operations"]["async_answer_idle"]["reasonCode"],
        "operation_auto_adapt_off"
    );
    // 全程没有 Desktop 帧（本地状态进入同一投影，不依赖桌面事件）。
    assert!(
        !peer_has_traffic(&mut peer).await,
        "设置变化不得向 Desktop 写帧"
    );

    // 重开：恢复可用；**不自动批准/发送**（零帧）。
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/adapt-settings",
            Some(COOKIE),
            Some(
                serde_json::json!({
                    "sessionId": THREAD, "action": "set_enabled", "enabled": true
                })
                .to_string(),
            ),
        ))
        .await
        .unwrap();
    let reopened: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(reopened["autoAdaptEnabled"], true);
    assert!(!peer_has_traffic(&mut peer).await, "重开不得触发任何写动作");
}

/// 详情读取是**只读**：重复 GET 不产生 Desktop 帧，也不改设置。
#[tokio::test]
async fn f08p_t08_get_details_does_not_repeat_success_probe() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let _ = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("ensure in time")
        .unwrap();
    let mut revisions = Vec::new();
    for _ in 0..3 {
        let r = f
            .app
            .clone()
            .oneshot(req(
                "GET",
                &format!("/m/api/v1/codex/adapt-status?sessionId={THREAD}"),
                Some(COOKIE),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        revisions.push(v["effectiveOperations"]["policyRevision"].clone());
    }
    assert_eq!(revisions[0], revisions[1], "只读详情不得让策略事实漂移");
    assert_eq!(revisions[1], revisions[2]);
    assert!(!peer_has_traffic(&mut peer).await, "详情读取零 Desktop 帧");
}

// ============================================================
// T11：跨链路定序（受理后原会话正式记录落地 → 卡清、不重复提交）
// ============================================================

/// 异步题：接受一次正式答题后，原会话出现正式答复记录 → 题转 Answered；
/// 再次提交同一题 = 零帧（不重复开回合、不再补发）。
#[tokio::test]
async fn f08p_t11_formal_record_resolves_question_without_second_answer() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let (question_key, context_token) = follow_with_async_question(&f, &mut peer, "idle").await;
    let body = serde_json::json!({
        "sessionId": THREAD,
        "actionId": "act-t11-1",
        "questionKey": question_key,
        "contextToken": context_token,
        "answer": "甲",
    });
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
    });
    let frame = recv_within(&mut peer, 10).await;
    assert_eq!(frame["method"], "thread-follower-start-turn");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "result": {"result": {"turn": {"id": "turn-answer-1", "status": "inProgress"}}}
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("answer in time")
        .unwrap();
    assert_eq!(r.status(), 200);

    // Desktop 随后把「正式答复」写进原会话（普通 userMessage + 官方信封）。
    let envelope = format!(
        "<send_user_message_question_reply>\n[{{\"questionItemId\":{},\"question\":\"选一个\",\"answer\":\"甲\"}}]\n</send_user_message_question_reply>",
        serde_json::to_string(&question_key).unwrap()
    );
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": THREAD, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": 10,
                "conversationState": {
                    "turns": [], "requests": [],
                    "threadRuntimeStatus": { "type": "idle" },
                    "turnHistory": {"history": {
                        "isComplete": true,
                        "entitiesByKey": {
                            "turn:t-q": {
                                "turnId": "t-q", "status": "completed",
                                "items": [{
                                    "type": "agentMessage", "id": "call-q", "text": "请选择",
                                    "phase": "final_answer", "delivery": "async",
                                    "questions": [{"title": "选一个", "options": ["甲", "乙"]}]
                                }]
                            },
                            "turn:t-answer": {
                                "turnId": "t-answer", "status": "completed",
                                "items": [{
                                    "type": "userMessage", "id": "u-answer-1",
                                    "content": [{"type": "text", "text": envelope, "text_elements": []}]
                                }]
                            }
                        },
                        "islands": [{"id": "i", "entries": [
                            {"key": "turn:t-q", "value": "turn:t-q"},
                            {"key": "turn:t-answer", "value": "turn:t-answer"}
                        ]}]
                    }}
                }
            }
        }
    }))
    .await;
    // 等镜像推进到 rev 10（有界）。
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let r = f
            .app
            .clone()
            .oneshot(req(
                "GET",
                &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
                Some(COOKIE),
                None,
            ))
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        if v["revision"] == serde_json::json!(10) {
            assert_eq!(
                v["asyncQuestions"][0]["state"], "answered",
                "正式记录落地后题应转已答：{v}"
            );
            // 已答记录以「问题／答案」摘要呈现，正文不再重复内部信封。
            let answer_message = v["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["id"] == "u-answer-1")
                .expect("answer message");
            assert_eq!(
                answer_message["interactionAnswer"]["answer"], "甲",
                "答复以结构化摘要呈现：{answer_message}"
            );
            assert!(
                !answer_message["text"]
                    .as_str()
                    .unwrap_or("")
                    .contains("<send_user_message"),
                "正文不得残留原始信封标记：{answer_message}"
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "镜像未在时限内推进到 rev 10"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // 再次提交同一题：零帧（题已答，不再开回合）。
    let body = serde_json::json!({
        "sessionId": THREAD,
        "actionId": "act-t11-2",
        "questionKey": question_key,
        "contextToken": context_token,
        "answer": "甲",
    });
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let receipt: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(receipt["delivery"], "not_sent");
    assert!(!peer_has_traffic(&mut peer).await, "已答后不得重复提交");
}

// ============================================================
// R01：最终写出权与交互资格同一线性化点（变化先胜出 → 零首帧）
// ============================================================

/// 在「上层检查已过、claim 未判」的窗口内先应用 busy 事实，再放行：
/// claim 复核必须拒绝，零 socket 写尝试，回执明确 NotSent（不是 Unknown）。
#[tokio::test]
async fn f08p_r01_claim_revalidates_eligibility_before_first_write() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let (question_key, context_token) = follow_with_async_question(&f, &mut peer, "idle").await;
    // 定序缝：写边界守卫前挂起（= 上层资格检查已过、claim 未判）。
    let gate = Arc::new(tokio::sync::Notify::new());
    f.state
        .codex_control
        .adapter
        .set_write_guard_gate(Some(gate.clone()));
    let body = serde_json::json!({
        "sessionId": THREAD,
        "actionId": "act-r01-1",
        "questionKey": question_key,
        "contextToken": context_token,
        "answer": "甲",
    });
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
    });
    // 等到写边界守卫挂起
    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !f.state.codex_control.adapter.write_guard_gate_reached() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "异步答题未在时限内到达写边界"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    // 窗口内：Desktop 进入运行中（快照先胜出）
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": THREAD, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": 10,
                "conversationState": {
                    "turns": [], "requests": [],
                    "threadRuntimeStatus": { "type": "active" },
                    "turnHistory": {"history": {
                        "isComplete": true,
                        "entitiesByKey": {"turn:t-q": {
                            "turnId": "t-q", "status": "inProgress",
                            "items": [{
                                "type": "agentMessage", "id": "call-q", "text": "请选择",
                                "phase": "final_answer", "delivery": "async",
                                "questions": [{"title": "选一个", "options": ["甲", "乙"]}]
                            }]
                        }},
                        "islands": [{"id": "i", "entries": [
                            {"key": "turn:t-q", "value": "turn:t-q"}
                        ]}]
                    }}
                }
            }
        }
    }))
    .await;
    // 等投影吸收 busy（有界），再放行 claim
    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while f
            .state
            .codex_control
            .adapter
            .projection(&f.thread_key())
            .await
            .runtime_status
            != "active"
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "busy 快照未在时限内被投影吸收"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    f.state.codex_control.adapter.set_write_guard_gate(None);
    gate.notify_waiters();
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("answer in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["delivery"], "not_sent", "资格先失效必须明确零投递：{v}");
    assert_eq!(v["reasonCode"], "operation_runtime_active");
    assert!(
        !peer_has_traffic(&mut peer).await,
        "claim 复核拒绝 = 零 socket 写"
    );
}

/// 反向定序：claim 已胜出（窗口内无失效）→ 正常写出，不留“被复核误杀”。
#[tokio::test]
async fn f08p_r01_claim_wins_when_no_fact_changes() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let (question_key, context_token) = follow_with_async_question(&f, &mut peer, "idle").await;
    let body = serde_json::json!({
        "sessionId": THREAD,
        "actionId": "act-r01-2",
        "questionKey": question_key,
        "contextToken": context_token,
        "answer": "乙",
    });
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
    });
    let frame = recv_within(&mut peer, 10).await;
    assert_eq!(frame["method"], "thread-follower-start-turn");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "result": {"result": {"turn": {"id": "turn-r01", "status": "inProgress"}}}
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("answer in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["delivery"], "accepted");
}

// ============================================================
// R02：同题持久排他（跨 actionId / 设备 / 重启）
// ============================================================

/// 不同 actionId 提交同一题：只有第一个（受理后占用在案）能写出，第二个 409 零帧。
#[tokio::test]
async fn f08p_r02_second_action_id_is_occupied_zero_frames() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let (question_key, context_token) = follow_with_async_question(&f, &mut peer, "idle").await;
    let body = serde_json::json!({
        "sessionId": THREAD,
        "actionId": "act-r02-a",
        "questionKey": question_key,
        "contextToken": context_token,
        "answer": "甲",
    });
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
    });
    let frame = recv_within(&mut peer, 10).await;
    assert_eq!(frame["method"], "thread-follower-start-turn");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "result": {"result": {"turn": {"id": "turn-r02", "status": "inProgress"}}}
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("first answer in time")
        .unwrap();
    assert_eq!(r.status(), 200);

    // 另一个 actionId（同题、同 token、同答案）——必须被占用拦下
    let body2 = serde_json::json!({
        "sessionId": THREAD,
        "actionId": "act-r02-b",
        "questionKey": question_key,
        "contextToken": context_token,
        "answer": "甲",
    });
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer",
            Some(COOKIE),
            Some(body2.to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 409, "同题已占用 → 409");
    assert!(!peer_has_traffic(&mut peer).await, "占用拒绝 = 零帧");
}

/// Unknown（未知投递）跨重启仍排他：新 actionId 不能绕开。
#[tokio::test]
async fn f08p_r02_unknown_blocks_retry_across_reopen() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let (question_key, context_token) = follow_with_async_question(&f, &mut peer, "idle").await;
    // 直接按真实结算语义把占用记为 unknown（等价于「已发出、丢 ACK、进程退出」）。
    // 已有动作行的占用不因 boot 变化被接管（见 claim 规则）。
    f.conn
        .lock()
        .unwrap()
        .execute(
            "INSERT INTO codex_interaction_claims
              (thread_id, question_key, action_id, context_token, state, claimed_epoch,
               claimed_owner, created_at, updated_at)
             VALUES (?1, ?2, 'act-r02-unknown', ?3, 'unknown', 0, 'old-boot:1:0', 1, 1)",
            rusqlite::params![THREAD, question_key, context_token],
        )
        .unwrap();
    // 真实未知动作对应的账本行（否则会被视为"登记前崩溃"的孤儿占用而允许接管）
    f.conn
        .lock()
        .unwrap()
        .execute(
            "INSERT INTO codex_actions
              (action_id, device_id, device_name, thread_id, kind, request_key,
               context_token, payload_hash, client_message_id, state, route,
               created_at, updated_at)
             VALUES ('act-r02-unknown','dev-1','iPhone',?1,'async_question',?2,?3,'h','c',
                     'unknown','desktop_ipc',1,1)",
            rusqlite::params![THREAD, question_key, context_token],
        )
        .unwrap();
    drop(peer);
    // 重启等价：同一账本库 + **全新适配器/连接**（内存态全丢）。
    let (io2, peer2) = ScriptedPeer::new();
    let adapter2 = Arc::new(DesktopIpcAdapter::new(
        Arc::new(StaticConnector::new(io2)),
        multi_agents_manager_lib::services::codex_desktop_ipc::profile::ProtocolProfile::unverified(
            "macos",
            "26.1021.31345",
        ),
    ));
    let mut restarted = fixture_on(
        f.conn.clone(),
        vec![codex_app_session(THREAD)],
        move |store| {
            let gate_store = store.clone();
            adapter2.set_auto_adapt_gate(Some(Arc::new(move || {
                gate_store.with(multi_agents_manager_lib::remote::auto_adapt_enabled_conn)
            })));
            Arc::new(CodexControlHub::with_adapter(store, adapter2, true))
        },
        Some(peer2),
    );
    let mut peer2 = restarted.peer.take().unwrap();
    let (q2, t2) = follow_with_async_question(&restarted, &mut peer2, "idle").await;
    let body = serde_json::json!({
        "sessionId": THREAD,
        "actionId": "act-r02-new",
        "questionKey": q2,
        "contextToken": t2,
        "answer": "乙",
    });
    let r = restarted
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 409, "Unknown 占用跨重启保留");
    assert!(!peer_has_traffic(&mut peer2).await, "零帧");
}

/// 明确零投递（NotSent）释放占用：修正后可重试成功。
#[tokio::test]
async fn f08p_r02_not_sent_releases_claim_for_retry() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let (question_key, context_token) = follow_with_async_question(&f, &mut peer, "idle").await;
    // 第一次：超长答案（adapter 明确 NotSent answer_too_long）
    let body = serde_json::json!({
        "sessionId": THREAD,
        "actionId": "act-r02-long",
        "questionKey": question_key,
        "contextToken": context_token,
        "answer": "x".repeat(10_001),
    });
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["delivery"], "not_sent");
    assert!(!peer_has_traffic(&mut peer).await);

    // 第二次（新 actionId，合法答案）：占用已释放 → 正常发出
    let body = serde_json::json!({
        "sessionId": THREAD,
        "actionId": "act-r02-retry",
        "questionKey": question_key,
        "contextToken": context_token,
        "answer": "甲",
    });
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
    });
    let frame = recv_within(&mut peer, 10).await;
    assert_eq!(frame["method"], "thread-follower-start-turn");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "result": {"result": {"turn": {"id": "turn-r02-retry", "status": "inProgress"}}}
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("retry in time")
        .unwrap();
    assert_eq!(r.status(), 200);
}

// ============================================================
// R04：异步 Accepted 的回合屏障在正常终态后自动收尾（后续 FIFO 接力）
// ============================================================

/// async Accepted → 对应回合 completed + idle → 持久屏障清零、普通消息恰好发下一条。
#[tokio::test]
async fn f08p_r04_async_turn_completion_releases_barrier() {
    use multi_agents_manager_lib::services::codex_control::{payload_hash, ActionPayload};
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let (question_key, context_token) = follow_with_async_question(&f, &mut peer, "idle").await;

    // 预置一条普通消息（排队；回合屏障期间不得发出）
    let ctx = ActionContext {
        action_id: "act-r04-send".into(),
        device_id: "dev-1".into(),
        device_name: "iPhone".into(),
        kind: ActionKind::Send,
        key: f.thread_key(),
    };
    let hash = payload_hash(
        ActionKind::Send,
        &f.thread_key(),
        None,
        None,
        &ActionPayload::Text("下一条".into()),
    );
    f.hub_store()
        .register(&ctx, ActionKind::Send, None, None, &hash, Some("下一条"))
        .unwrap();

    // 正式答题 → 受理（回合 turn-r04）
    let body = serde_json::json!({
        "sessionId": THREAD,
        "actionId": "act-r04-answer",
        "questionKey": question_key,
        "contextToken": context_token,
        "answer": "甲",
    });
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
    });
    let frame = recv_within(&mut peer, 10).await;
    assert_eq!(frame["method"], "thread-follower-start-turn");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "result": {"result": {"turn": {"id": "turn-r04", "status": "inProgress"}}}
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("answer in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    // 屏障已持久：普通消息此刻不得派发
    let hub = f.state.codex_control.clone();
    let key = f.thread_key();
    assert!(
        hub.router.is_turn_active_public(&key),
        "受理后应有在册回合限制"
    );
    // 屏障在册时 FIFO 选择为空（不发下一条；不在这里调用会等待的 flush）
    assert!(
        f.hub_store().has_compat_fence(&key).unwrap_or(true),
        "受理后持久屏障应在册"
    );

    // Desktop 把 turn-r04 标 completed 且回 idle
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": THREAD, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": 12,
                "conversationState": {
                    "turns": [], "requests": [],
                    "threadRuntimeStatus": { "type": "idle" },
                    "turnHistory": {"history": {
                        "isComplete": true,
                        "entitiesByKey": {"turn:turn-r04": {
                            "turnId": "turn-r04", "status": "completed",
                            "items": [{"type": "userMessage", "id": "u-r04",
                                "content": [{"type": "text", "text": "甲", "text_elements": []}]}]
                        }},
                        "islands": [{"id": "i", "entries": [
                            {"key": "turn:turn-r04", "value": "turn:turn-r04"}
                        ]}]
                    }}
                }
            }
        }
    }))
    .await;
    // 等投影吸收终态并释放（有界）
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let p = hub.adapter.projection(&key).await;
        if hub.router.release_turn_if_complete(&key, &p) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "async 回合终态未在时限内释放屏障"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // 持久屏障清零
    assert!(
        !f.hub_store().has_compat_fence(&key).unwrap_or(true),
        "屏障必须被清（否则后续消息永远排队）"
    );
    // FIFO 恰好接力一条
    let flush_hub = hub.clone();
    let flush_key = key.clone();
    let flush = tokio::spawn(async move { flush_hub.router.flush_thread(&flush_key).await });
    let frame = recv_within(&mut peer, 10).await;
    assert_eq!(frame["method"], "thread-follower-start-turn");
    let text = frame["params"]["turnStart"]["request"]["input"][0]["text"]
        .as_str()
        .unwrap();
    assert!(text.starts_with("下一条"), "接力的应是预置普通消息：{text}");
    // 先回 ACK，让 flush 的派发在预算内结束（不依赖 120s 收尾）
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "result": {"result": {"turn": {"id": "turn-r04-next", "status": "inProgress"}}}
    }))
    .await;
    let sent = tokio::time::timeout(Duration::from_secs(15), flush)
        .await
        .expect("flush in time")
        .expect("flush task");
    assert!(sent.is_some(), "终态后应接力发下一条");
}

// ============================================================
// R05：wire cmid 必须等于账本持久 client_message_id
// ============================================================

#[tokio::test]
async fn f08p_r05_wire_client_message_id_is_the_persisted_one() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let (question_key, context_token) = follow_with_async_question(&f, &mut peer, "idle").await;
    let body = serde_json::json!({
        "sessionId": THREAD,
        "actionId": "act-r05",
        "questionKey": question_key,
        "contextToken": context_token,
        "answer": "甲",
    });
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
    });
    let frame = recv_within(&mut peer, 10).await;
    let wire_cmid = frame["params"]["turnStart"]["request"]["clientUserMessageId"]
        .as_str()
        .unwrap()
        .to_string();
    let (_, ledger_cmid) = f.hub_store().action_thread_and_cmid("act-r05").unwrap();
    assert_eq!(
        wire_cmid, ledger_cmid,
        "R05：wire cmid 必须来自 register 的持久行（迟回执/重启核对同源）"
    );
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "result": {"result": {"turn": {"id": "turn-r05", "status": "inProgress"}}}
    }))
    .await;
    let _ = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .unwrap()
        .unwrap();
}

// ============================================================
// R03：崩溃恢复为 async 保留保守回合屏障（FIFO 不得抢发）
// ============================================================

#[tokio::test]
async fn f08p_r03_recovery_keeps_async_turn_barrier() {
    use multi_agents_manager_lib::services::codex_control::{payload_hash, ActionPayload};
    let f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let ctx = ActionContext {
        action_id: "act-r03-async".into(),
        device_id: "dev-1".into(),
        device_name: "iPhone".into(),
        kind: ActionKind::AsyncQuestion,
        key: f.thread_key(),
    };
    let hash = payload_hash(
        ActionKind::AsyncQuestion,
        &f.thread_key(),
        Some("q"),
        Some("tok"),
        &ActionPayload::Text("甲".into()),
    );
    let store = f.hub_store();
    store
        .register(
            &ctx,
            ActionKind::AsyncQuestion,
            Some("q"),
            Some("tok"),
            &hash,
            None,
        )
        .unwrap();
    store
        .reserve("act-r03-async", ControlRoute::DesktopIpc)
        .unwrap();
    // 运行期意外退出：reserved 行留在账本上（模拟首次尝试后、落账前崩溃）
    let recovered = store.recover_reserved().unwrap();
    assert!(recovered >= 1, "reserved 行应被恢复为 unknown");
    let fence = store.compat_fences(&f.thread_key()).unwrap();
    assert!(
        fence.iter().any(|f| f.action_id == "act-r03-async"),
        "async 恢复必须建立保守屏障（否则普通 FIFO 会抢发）: {fence:?}"
    );
    // 预置普通消息：屏障在册 → 不得派发
    let send_ctx = ActionContext {
        action_id: "act-r03-send".into(),
        device_id: "dev-1".into(),
        device_name: "iPhone".into(),
        kind: ActionKind::Send,
        key: f.thread_key(),
    };
    let send_hash = payload_hash(
        ActionKind::Send,
        &f.thread_key(),
        None,
        None,
        &ActionPayload::Text("后续".into()),
    );
    store
        .register(
            &send_ctx,
            ActionKind::Send,
            None,
            None,
            &send_hash,
            Some("后续"),
        )
        .unwrap();
    assert!(
        f.state
            .codex_control
            .router
            .flush_thread(&f.thread_key())
            .await
            .is_none(),
        "async 未知屏障在册时普通消息零派发"
    );
}

// ============================================================
// R06：暂停时「仅本题先发」= 一次性授权真实可用（旧队列保持暂停）
// ============================================================

#[tokio::test]
async fn f08p_r06_bypass_paused_queue_allows_exactly_one_answer() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let (question_key, context_token) = follow_with_async_question(&f, &mut peer, "idle").await;
    let hub = f.state.codex_control.clone();
    let key = f.thread_key();
    // 建立持久暂停（与打断/cancel 同一通道）
    let (_token, _inserted) = hub
        .prepare_turn_stop_pause(&key, "cancel_pending")
        .expect("pause");
    let p = hub.adapter.projection(&key).await;
    assert!(p.queue_paused, "投影必须看见暂停");
    let question = p
        .async_questions
        .iter()
        .find(|q| q.question_key == question_key)
        .unwrap();
    assert!(!question.actionable, "暂停时默认不可提交");
    assert_eq!(
        question.disabled_reason_code.as_deref(),
        Some("operation_queue_paused")
    );

    // 未显式越队 → 零帧
    let body = serde_json::json!({
        "sessionId": THREAD,
        "actionId": "act-r06-no-bypass",
        "questionKey": question_key,
        "contextToken": context_token,
        "answer": "甲",
    });
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["delivery"], "not_sent");
    assert_eq!(v["reasonCode"], "operation_queue_paused");
    assert!(!peer_has_traffic(&mut peer).await, "默认暂停零帧");

    // 显式「仅本题先发」→ 恰好一帧；旧队列保持暂停
    let body = serde_json::json!({
        "sessionId": THREAD,
        "actionId": "act-r06-bypass",
        "questionKey": question_key,
        "contextToken": context_token,
        "answer": "甲",
        "bypassPausedQueue": true,
    });
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
    });
    let frame = recv_within(&mut peer, 10).await;
    assert_eq!(frame["method"], "thread-follower-start-turn");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "result": {"result": {"turn": {"id": "turn-r06", "status": "inProgress"}}}
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("bypass in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["delivery"], "accepted");
    assert!(
        hub.router.store().queue_pause(&key).unwrap().is_some(),
        "越队不解除旧队列暂停"
    );
    // 授权一次性：同 action 重放不再受理，且不会二次写出
    let body = serde_json::json!({
        "sessionId": THREAD,
        "actionId": "act-r06-bypass-2",
        "questionKey": question_key,
        "contextToken": context_token,
        "answer": "乙",
        "bypassPausedQueue": true,
    });
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap();
    assert!(
        r.status() == 409 || r.status() == 200,
        "占用/暂停至少明确拒绝之一：{}",
        r.status()
    );
    assert!(!peer_has_traffic(&mut peer).await, "重放不得二次写出");
}

// ============================================================
// R07：cancel 前置暂停由后台按真实结果收尾（前台超时/断开不遗留暂停）
// ============================================================

#[tokio::test]
async fn f08p_r07_cancel_pause_cleanup_survives_front_timeout() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    // 有命令审批待处理（cancel 的合法目标）
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    drive_ensure(
        &mut peer,
        THREAD,
        vec![serde_json::json!({
            "id": 77, "method": "item/commandExecution/requestApproval",
            "params": { "command": "pnpm test", "cwd": "/w",
                        "availableDecisions": ["accept", "cancel"] }
        })],
        "active",
    )
    .await;
    let _ = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("ensure in time")
        .unwrap();
    let hub = f.state.codex_control.clone();
    let key = f.thread_key();
    let projection = hub.adapter.projection(&key).await;
    let pending = projection.pending.first().expect("pending").clone();

    // 后台应答者：300ms 后回 error（> 前台 100ms 预算）——前台 expired，
    // 后台按真实 Rejected 收尾。
    let responder = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let frame = peer.recv().await;
        peer.send(&serde_json::json!({
            "type": "response", "requestId": frame["requestId"],
            "resultType": "error", "method": frame["method"],
            "error": { "code": "denied", "message": "test" }
        }))
        .await;
        peer
    });

    let body = serde_json::json!({
        "sessionId": THREAD,
        "actionId": "act-r07-cancel",
        "requestKey": pending.request_key,
        "contextToken": pending.context_token,
        "action": "cancel",
        "frontBudgetMs": 100,
    });
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/pending-response",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(
        v["delivery"], "unknown_delivery",
        "前台预算到期必须如实 Unknown（后台仍收尾）：{v}"
    );
    let _peer = tokio::time::timeout(Duration::from_secs(10), responder)
        .await
        .expect("responder in time")
        .expect("responder");
    // H06（第三轮）：暂停归属跟随**真实最终结果**——前台 Unknown 不销毁归属；后台
    // 收到明确 NotSent/Rejected 后由结算钩子撤本次 token 的暂停。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if hub.router.store().queue_pause(&key).unwrap().is_none() {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "后台明确零投递后应撤回本次 cancel 的暂停"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// H06：暂停创建整体在独立任务内——零预算时**根本不会**落暂停（无早退孤儿）。
#[tokio::test]
async fn f08p_h06_cancel_zero_budget_leaves_no_pause() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    drive_ensure(
        &mut peer,
        THREAD,
        vec![serde_json::json!({
            "id": 96, "method": "item/commandExecution/requestApproval",
            "params": { "command": "pnpm test", "cwd": "/w",
                        "availableDecisions": ["accept", "cancel"] }
        })],
        "active",
    )
    .await;
    let _ = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .unwrap()
        .unwrap();
    let projection = f
        .state
        .codex_control
        .adapter
        .projection(&f.thread_key())
        .await;
    let pending = projection.pending.first().expect("pending").clone();
    let body = serde_json::json!({
        "sessionId": THREAD,
        "actionId": "act-h06",
        "requestKey": pending.request_key,
        "contextToken": pending.context_token,
        "action": "cancel",
        "frontBudgetMs": 0,
    });
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/pending-response",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["delivery"], "not_sent");
    assert_eq!(v["reasonCode"], "front_budget_elapsed");
    assert!(
        f.hub_store()
            .queue_pause(&f.thread_key())
            .unwrap()
            .is_none(),
        "零预算不得留下暂停（暂停创建已在任务内、随派发资格一起拒绝）"
    );
    let frames = drain_frames(&mut peer).await;
    assert!(
        frames
            .iter()
            .all(|frame| frame["method"] != "thread-follower-command-approval-decision"),
        "零预算零审批帧：{frames:?}"
    );
}

// ============================================================
// R08：cancel 端到端（按钮词 → 真实写词 + 回合终止 + 暂停）
// ============================================================

#[tokio::test]
async fn f08p_r08_cancel_writes_cancel_word_and_pauses() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    // 同时提供 decline 与 cancel：用户选「终止本轮」必须发 cancel（不是 reject/decline）
    drive_ensure(
        &mut peer,
        THREAD,
        vec![serde_json::json!({
            "id": 88, "method": "item/commandExecution/requestApproval",
            "params": { "command": "rm -rf build", "cwd": "/w",
                        "availableDecisions": ["decline", "cancel"] }
        })],
        "active",
    )
    .await;
    let _ = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("ensure in time")
        .unwrap();
    let projection = f
        .state
        .codex_control
        .adapter
        .projection(&f.thread_key())
        .await;
    let pending = projection.pending.first().expect("pending").clone();
    let body = serde_json::json!({
        "sessionId": THREAD,
        "actionId": "act-r08-cancel",
        "requestKey": pending.request_key,
        "contextToken": pending.context_token,
        "action": "cancel",
    });
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/pending-response",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
    });
    let frame = recv_within(&mut peer, 10).await;
    assert_eq!(
        frame["params"]["decision"], "cancel",
        "「终止本轮」必须发 cancel（Reject 只拒绝请求）：{frame}"
    );
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": frame["method"],
        "result": {"ok": true}
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("cancel in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["delivery"], "accepted");
    assert!(
        f.hub_store()
            .queue_pause(&f.thread_key())
            .unwrap()
            .is_some(),
        "cancel 必须建立持久队列暂停（防止旧队列继续发）"
    );
}

/// 仅 accept/cancel 的请求：旧的「拒绝」按钮（reject）必须被明确拒绝，不能写出 decline。
#[tokio::test]
async fn f08p_r08_reject_is_impossible_when_only_cancel_offered() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    drive_ensure(
        &mut peer,
        THREAD,
        vec![serde_json::json!({
            "id": 89, "method": "item/commandExecution/requestApproval",
            "params": { "command": "pnpm build", "cwd": "/w",
                        "availableDecisions": ["accept", "cancel"] }
        })],
        "active",
    )
    .await;
    let _ = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("ensure in time")
        .unwrap();
    let projection = f
        .state
        .codex_control
        .adapter
        .projection(&f.thread_key())
        .await;
    let pending = projection.pending.first().expect("pending").clone();
    let body = serde_json::json!({
        "sessionId": THREAD,
        "actionId": "act-r08-reject",
        "requestKey": pending.request_key,
        "contextToken": pending.context_token,
        "action": "reject",
    });
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/pending-response",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap();
    // 明确零投递（NotSent + 机器码）或 4xx——两者都表示**没有写出**。
    let status = r.status();
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert!(
        status == 409 || status == 400 || (status == 200 && v["delivery"] == "not_sent"),
        "未提供的决策词必须零写出（实际 {status} {v}）"
    );
    assert!(!peer_has_traffic(&mut peer).await, "零帧");
}

// ============================================================
// R13：前台预算贯通（打断零预算 = 零帧；原生响应不追加整段确认窗）
// ============================================================

#[tokio::test]
async fn f08p_r13_interrupt_zero_budget_is_zero_frames() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    // 建立投影（含活动回合）
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "active").await;
    let _ = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("ensure in time")
        .unwrap();
    install_interrupt_contract(&f);
    let body = serde_json::json!({
        "sessionId": THREAD,
        "turnId": "turn-active",
        "frontBudgetMs": 0,
    });
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/turn/interrupt",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap();
    // 打断端点沿用既有 5xx 错误体形态（明确未送达）；关键是**零帧**且不遗留暂停。
    let status = r.status();
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(status, 503);
    assert_eq!(
        v["error"], "front_budget_exhausted",
        "零预算 = 明确零投递：{v}"
    );
    assert!(!peer_has_traffic(&mut peer).await, "零预算 = 零帧");
    // 零预算是零写尝试：不得遗留本次暂停
    assert!(
        f.hub_store()
            .queue_pause(&f.thread_key())
            .unwrap()
            .is_none(),
        "未发出打断不得冻结队列"
    );
}

#[tokio::test]
async fn f08p_r13_pending_confirm_window_does_not_reset_front_budget() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    drive_ensure(
        &mut peer,
        THREAD,
        vec![serde_json::json!({
            "id": 91, "method": "item/commandExecution/requestApproval",
            "params": { "command": "pnpm test", "cwd": "/w",
                        "availableDecisions": ["accept", "cancel"] }
        })],
        "active",
    )
    .await;
    let _ = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("ensure in time")
        .unwrap();
    // 显式打开确认窗（fixture 默认可能是 0），验证它只吃前台剩余预算。
    f.state.codex_control.set_decision_confirm_ms(5_000);
    let projection = f
        .state
        .codex_control
        .adapter
        .projection(&f.thread_key())
        .await;
    let pending = projection.pending.first().expect("pending").clone();
    let body = serde_json::json!({
        "sessionId": THREAD,
        "actionId": "act-r13-approve",
        "requestKey": pending.request_key,
        "contextToken": pending.context_token,
        "action": "approve",
        "frontBudgetMs": 300,
    });
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/pending-response",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
    });
    let frame = recv_within(&mut peer, 10).await;
    assert_eq!(frame["params"]["decision"], "accept");
    // 回 ACK，但**不**让 pending 从快照消失 → 确认窗只能吃剩余预算（≈300ms），
    // 不能追加整段 decision_confirm（默认 5s）。
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": frame["method"],
        "result": {"ok": true}
    }))
    .await;
    let started = std::time::Instant::now();
    let r = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("respond in time")
        .unwrap();
    let elapsed = started.elapsed();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["delivery"], "accepted");
    assert_eq!(v["reasonCode"], "decision_unconfirmed");
    assert!(
        elapsed < Duration::from_secs(3),
        "确认窗不得重置前台预算（实际 {elapsed:?}）"
    );
}

// ============================================================
// H01：成功证据按目标/效果（不来自台账 Accepted）
// ============================================================

#[tokio::test]
async fn f08p_h01_accepted_answer_does_not_unlock_success_until_echo() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let (question_key, context_token) = follow_with_async_question(&f, &mut peer, "idle").await;
    f.state
        .codex_control
        .adapter
        .set_probe_key_override(Some("macos|26.1021.31345|cli|1".into()));
    f.state
        .codex_control
        .adapter
        .set_package_sha_override(Some("sha-h01".into()));
    // 接受一次答题（未出现正式答复）
    let body = serde_json::json!({
        "sessionId": THREAD,
        "actionId": "act-h01",
        "questionKey": question_key,
        "contextToken": context_token,
        "answer": "甲",
    });
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
    });
    let frame = recv_within(&mut peer, 10).await;
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "result": {"result": {"turn": {"id": "turn-h01", "status": "inProgress"}}}
    }))
    .await;
    let _ = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .unwrap()
        .unwrap();
    assert!(
        f.conn
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM codex_operation_success", [], |r| r
                .get::<_, i64>(0))
            .unwrap()
            == 0,
        "仅 Accepted 不得产生 ObservedSuccess 证据"
    );

    // N02：强证明 = 本动作 ACK 的 turn + 持久 cmid 的正式 userMessage。
    let (_, action_cmid) = f
        .hub_store()
        .action_thread_and_cmid("act-h01")
        .expect("cmid");
    // 正式答复落地（准确回显）→ 再取投影 → 记录异步答题成功证据
    let envelope = format!(
        "<send_user_message_question_reply>\n[{{\"questionItemId\":{},\"question\":\"选一个\",\"answer\":\"甲\"}}]\n</send_user_message_question_reply>",
        serde_json::to_string(&question_key).unwrap()
    );
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": THREAD, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": 20,
                "conversationState": {
                    "turns": [], "requests": [],
                    "threadRuntimeStatus": { "type": "idle" },
                    "turnHistory": {"history": {
                        "isComplete": true,
                        "entitiesByKey": {
                            "turn:t-q": {
                                "turnId": "t-q", "status": "completed",
                                "items": [{
                                    "type": "agentMessage", "id": "call-q", "text": "请选择",
                                    "phase": "final_answer", "delivery": "async",
                                    "questions": [{"title": "选一个", "options": ["甲", "乙"]}]
                                }]
                            },
                            "turn:turn-h01": {
                                "turnId": "turn-h01", "status": "completed",
                                "items": [{
                                    "type": "userMessage", "id": "u-h01",
                                    "clientId": action_cmid,
                                    "content": [{"type": "text", "text": envelope, "text_elements": []}]
                                }]
                            }
                        },
                        "islands": [{"id": "i", "entries": [
                            {"key": "turn:t-q", "value": "turn:t-q"},
                            {"key": "turn:turn-h01", "value": "turn:turn-h01"}
                        ]}]
                    }}
                }
            }
        }
    }))
    .await;
    // 有界等待镜像吸收
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while f
        .state
        .codex_control
        .adapter
        .projection(&f.thread_key())
        .await
        .revision
        != Some(20)
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "正式答复快照未在时限内吸收"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // H01（第三轮）：**一次 POST 后的迟到回显自动结算**——不需要第二次 POST。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let row: i64 = f
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM codex_operation_success
                  WHERE operation = 'async_answer_idle'
                    AND probe_key = 'macos|26.1021.31345|cli|1'
                    AND package_sha256 = 'sha-h01'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        if row == 1 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "回显后应自动按原动作目标结算成功证据"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // 历史已答题重放（任意非空旧 token）不得再写任何成功证据。
    let before: i64 = f
        .conn
        .lock()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM codex_operation_success", [], |r| {
            r.get(0)
        })
        .unwrap();
    let body = serde_json::json!({
        "sessionId": THREAD,
        "actionId": "act-h01-replay",
        "questionKey": question_key,
        "contextToken": context_token,
        "answer": "甲",
    });
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let after: i64 = f
        .conn
        .lock()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM codex_operation_success", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(before, after, "重放历史已答题不得给当前目标记成功");
}

// ============================================================
// H02：最终 claim 复核设备撤销；interrupt 写边界许可
// ============================================================

#[tokio::test]
async fn f08p_h02_device_revoked_after_guard_is_zero_write() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let (question_key, context_token) = follow_with_async_question(&f, &mut peer, "idle").await;
    let gate = Arc::new(tokio::sync::Notify::new());
    f.state
        .codex_control
        .adapter
        .set_write_guard_gate(Some(gate.clone()));
    let body = serde_json::json!({
        "sessionId": THREAD,
        "actionId": "act-h02-dev",
        "questionKey": question_key,
        "contextToken": context_token,
        "answer": "甲",
    });
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
    });
    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !f.state.codex_control.adapter.write_guard_gate_reached() {
            assert!(tokio::time::Instant::now() < deadline, "未到达写边界");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    // 窗口内撤销设备（router 的早先检查已通过）
    f.state.store.with(|c| {
        multi_agents_manager_lib::remote::pairing::revoke_device(c, "dev-1").unwrap();
    });
    f.state.codex_control.adapter.set_write_guard_gate(None);
    gate.notify_waiters();
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["delivery"], "not_sent");
    assert_eq!(v["reasonCode"], "device_revoked");
    assert!(!peer_has_traffic(&mut peer).await, "撤销设备后零首次写出");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn f08p_h02_interrupt_revalidates_at_write_boundary() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "active").await;
    let _ = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .unwrap()
        .unwrap();
    install_interrupt_contract(&f);
    // 建一个**在册活动回合**（打断安全阀的目标）
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": THREAD, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": 25,
                "conversationState": {
                    "turns": [], "requests": [],
                    "threadRuntimeStatus": { "type": "active" },
                    "turnHistory": {"history": {
                        "isComplete": true,
                        "entitiesByKey": {"turn:active": {
                            "turnId": "turn-active", "status": "inProgress",
                            "items": [{"type": "agentMessage", "id": "m1", "text": "跑着"}]
                        }},
                        "islands": [{"id": "i", "entries": [
                            {"key": "turn:active", "value": "turn:active"}
                        ]}]
                    }}
                }
            }
        }
    }))
    .await;
    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let active = f
                .state
                .codex_control
                .adapter
                .mirror_active_turn_id(&f.thread_key())
                .await;
            if active.as_deref() == Some("turn-active") {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "活动回合未吸收");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    // 写边界前挂起（interrupt 无账本 action_id → 用复核会合缝，而非 guard gate）
    let barrier = Arc::new(std::sync::Barrier::new(2));
    f.state
        .codex_control
        .adapter
        .set_interaction_recheck_barrier(Some(barrier.clone()));
    let body = serde_json::json!({
        "sessionId": THREAD,
        "turnId": "turn-active",
    });
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/turn/interrupt",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
    });
    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !f.state.codex_control.adapter.interaction_recheck_reached() {
            assert!(tokio::time::Instant::now() < deadline, "未到达写边界复核");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    // 窗口内目标回合被替换（镜像活动回合变成别的）→ 写边界许可必须拒绝
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": THREAD, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": 30,
                "conversationState": {
                    "turns": [], "requests": [],
                    "threadRuntimeStatus": { "type": "active" },
                    "turnHistory": {"history": {
                        "isComplete": true,
                        "entitiesByKey": {"turn:other": {
                            "turnId": "turn-other", "status": "inProgress",
                            "items": []
                        }},
                        "islands": [{"id": "i", "entries": [
                            {"key": "turn:other", "value": "turn:other"}
                        ]}]
                    }}
                }
            }
        }
    }))
    .await;
    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while f
            .state
            .codex_control
            .adapter
            .projection(&f.thread_key())
            .await
            .revision
            != Some(30)
        {
            assert!(tokio::time::Instant::now() < deadline, "新回合快照未吸收");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    // 放行写边界复核（资格已失效 → 零帧）。会合点是同步阻塞：用阻塞池等待，
    // 不占运行时 worker（否则 current/multi 运行时都可能被两边同时堵死）。
    let release = barrier.clone();
    let _ = tokio::task::spawn_blocking(move || release.wait()).await;
    f.state
        .codex_control
        .adapter
        .set_interaction_recheck_barrier(None);
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("in time")
        .unwrap();
    assert!(
        r.status() == 503 || r.status() == 409 || r.status() == 200,
        "必须明确未送达：{}",
        r.status()
    );
    let frames = drain_frames(&mut peer).await;
    assert!(
        !frames
            .iter()
            .any(|frame| frame["method"] == "thread-follower-interrupt-turn"),
        "目标已被替换 → 零打断帧：{frames:?}"
    );
}

// ============================================================
// H03：越队只放宽暂停，不放宽 Unknown/缺页
// ============================================================

#[tokio::test]
async fn f08p_h03_bypass_does_not_release_unknown_question() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    // 题目在 notLoaded 的回合里 → 覆盖未知 → Unknown（不可提交）
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "idle").await;
    let _ = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .unwrap()
        .unwrap();
    install_async_answer_contract(&f);
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": THREAD, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": 40,
                "conversationState": {
                    "turns": [], "requests": [],
                    "threadRuntimeStatus": { "type": "idle" },
                    "turnHistory": {"history": {
                        "isComplete": true,
                        "entitiesByKey": {"turn:t-q": {
                            "turnId": "t-q", "status": "completed",
                            "itemsView": "notLoaded",
                            "items": [{
                                "type": "agentMessage", "id": "call-q", "text": "请选择",
                                "phase": "final_answer", "delivery": "async",
                                "questions": [{"title": "选一个", "options": ["甲"]}]
                            }]
                        }},
                        "islands": [{"id": "i", "entries": [
                            {"key": "turn:t-q", "value": "turn:t-q"}
                        ]}]
                    }}
                }
            }
        }
    }))
    .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let question = loop {
        let p = f
            .state
            .codex_control
            .adapter
            .projection(&f.thread_key())
            .await;
        if p.revision == Some(40) {
            break p.async_questions.first().cloned();
        }
        assert!(tokio::time::Instant::now() < deadline, "快照未吸收");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let question = question.expect("question");
    assert_eq!(
        question.state,
        multi_agents_manager_lib::services::codex_desktop_ipc::async_question::InteractionState::Unknown,
        "缺页题必须是 Unknown"
    );
    // 建立暂停（越队场景）
    let (_token, _) = f
        .state
        .codex_control
        .prepare_turn_stop_pause(&f.thread_key(), "cancel_pending")
        .unwrap();
    f.state
        .codex_control
        .adapter
        .republish_thread_projection(&f.thread_key())
        .await;
    let body = serde_json::json!({
        "sessionId": THREAD,
        "actionId": "act-h03",
        "questionKey": question.question_key,
        "contextToken": question.context_token,
        "answer": "甲",
        "bypassPausedQueue": true,
    });
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["delivery"], "not_sent", "越队不得放宽 Unknown：{v}");
    assert_eq!(v["reasonCode"], "async_answer_history_incomplete");
    let frames = drain_frames(&mut peer).await;
    assert!(
        !frames
            .iter()
            .any(|frame| frame["method"] == "thread-follower-start-turn"),
        "越队 Unknown 不得发出 start-turn：{frames:?}"
    );
}

// ============================================================
// H04：pending-response 零预算 = 零 mutation（不能用后台 120s 预算批准）
// ============================================================

#[tokio::test]
async fn f08p_h04_pending_zero_budget_is_zero_mutation() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    drive_ensure(
        &mut peer,
        THREAD,
        vec![serde_json::json!({
            "id": 95, "method": "item/commandExecution/requestApproval",
            "params": { "command": "pnpm test", "cwd": "/w",
                        "availableDecisions": ["accept", "cancel"] }
        })],
        "active",
    )
    .await;
    let _ = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .unwrap()
        .unwrap();
    let projection = f
        .state
        .codex_control
        .adapter
        .projection(&f.thread_key())
        .await;
    let pending = projection.pending.first().expect("pending").clone();
    let body = serde_json::json!({
        "sessionId": THREAD,
        "actionId": "act-h04",
        "requestKey": pending.request_key,
        "contextToken": pending.context_token,
        "action": "approve",
        "frontBudgetMs": 0,
    });
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/pending-response",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["delivery"], "not_sent", "零预算必须明确零投递：{v}");
    assert_eq!(v["reasonCode"], "front_budget_elapsed");
    let frames = drain_frames(&mut peer).await;
    assert!(
        frames
            .iter()
            .all(|frame| { frame["method"] != "thread-follower-command-approval-decision" }),
        "零预算不得派发任何审批帧：{frames:?}"
    );
    // 零预算发生在登记之前：不得留下动作行（没有可被后台收尾的在途动作）。
    assert!(
        f.hub_store()
            .get_for_device("act-h04", "dev-1")
            .unwrap()
            .is_none(),
        "零预算应在登记前明确拒绝（无在途动作）"
    );
}

// ============================================================
// H05：预派发早退不留同题占用（可再次提交）
// ============================================================

#[tokio::test]
async fn f08p_h05_early_exit_releases_question_claim() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let (question_key, context_token) = follow_with_async_question(&f, &mut peer, "idle").await;
    // 撤销设备（claim 的最终复核会拒绝 → 明确零投递）
    f.state.store.with(|c| {
        multi_agents_manager_lib::remote::pairing::revoke_device(c, "dev-1").unwrap();
    });
    let body = serde_json::json!({
        "sessionId": THREAD,
        "actionId": "act-h05",
        "questionKey": question_key,
        "contextToken": context_token,
        "answer": "甲",
    });
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap();
    // 设备门可能在 claim 之前（403）或 claim 的最终复核里（200 not_sent）——两条
    // 路径都必须**不留占用**。
    assert!(
        r.status() == 200 || r.status() == 403,
        "撤销设备必须明确拒绝：{}",
        r.status()
    );
    let claims: i64 = f
        .conn
        .lock()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM codex_interaction_claims", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(claims, 0, "早退必须释放同题占用（不得永久锁题）");
}

// ============================================================
// H02：原生响应（pending）在最终 claim 复核设备撤销
// ============================================================

#[tokio::test]
async fn f08p_h02_pending_device_revoked_after_guard_is_zero_write() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    drive_ensure(
        &mut peer,
        THREAD,
        vec![serde_json::json!({
            "id": 97, "method": "item/commandExecution/requestApproval",
            "params": { "command": "pnpm test", "cwd": "/w",
                        "availableDecisions": ["accept", "cancel"] }
        })],
        "active",
    )
    .await;
    let _ = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .unwrap()
        .unwrap();
    let projection = f
        .state
        .codex_control
        .adapter
        .projection(&f.thread_key())
        .await;
    let pending = projection.pending.first().expect("pending").clone();
    let gate = Arc::new(tokio::sync::Notify::new());
    f.state
        .codex_control
        .adapter
        .set_write_guard_gate(Some(gate.clone()));
    let body = serde_json::json!({
        "sessionId": THREAD,
        "actionId": "act-h02-pending-dev",
        "requestKey": pending.request_key,
        "contextToken": pending.context_token,
        "action": "approve",
        "frontBudgetMs": 10_000,
    });
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/pending-response",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
    });
    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !f.state.codex_control.adapter.write_guard_gate_reached() {
            assert!(tokio::time::Instant::now() < deadline, "未到达写边界");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
    f.state.store.with(|c| {
        multi_agents_manager_lib::remote::pairing::revoke_device(c, "dev-1").unwrap();
    });
    f.state.codex_control.adapter.set_write_guard_gate(None);
    gate.notify_waiters();
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["delivery"], "not_sent");
    assert_eq!(v["reasonCode"], "device_revoked");
    let frames = drain_frames(&mut peer).await;
    assert!(
        frames
            .iter()
            .all(|frame| frame["method"] != "thread-follower-command-approval-decision"),
        "撤销设备后零审批帧：{frames:?}"
    );
}

// ============================================================
// H01（第三轮）：历史已答不给当前目标授权；同 build 异 SHA 不通用
// ============================================================

#[tokio::test]
async fn f08p_h01b_history_answer_does_not_authorize_current_target() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    f.state
        .codex_control
        .adapter
        .set_probe_key_override(Some("macos|26.1021.31345|cli|1".into()));
    let (question_key, context_token) = follow_with_async_question(&f, &mut peer, "idle").await;
    // 历史里该题已被正式答复（A 目标的历史）
    let envelope = format!(
        "<send_user_message_question_reply>\n[{{\"questionItemId\":{},\"question\":\"选一个\",\"answer\":\"甲\"}}]\n</send_user_message_question_reply>",
        serde_json::to_string(&question_key).unwrap()
    );
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": THREAD, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": 41,
                "conversationState": {
                    "turns": [], "requests": [],
                    "threadRuntimeStatus": { "type": "idle" },
                    "turnHistory": {"history": {
                        "isComplete": true,
                        "entitiesByKey": {
                            "turn:t-q": {"turnId": "t-q", "status": "completed", "items": [{
                                "type": "agentMessage", "id": "call-q", "text": "请选择",
                                "phase": "final_answer", "delivery": "async",
                                "questions": [{"title": "选一个", "options": ["甲"]}]
                            }]},
                            "turn:t-answer": {"turnId": "t-answer", "status": "completed", "items": [{
                                "type": "userMessage", "id": "u-h01b",
                                "content": [{"type": "text", "text": envelope, "text_elements": []}]
                            }]}
                        },
                        "islands": [{"id": "i", "entries": [
                            {"key": "turn:t-q", "value": "turn:t-q"},
                            {"key": "turn:t-answer", "value": "turn:t-answer"}
                        ]}]
                    }}
                }
            }
        }
    }))
    .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let p = f
            .state
            .codex_control
            .adapter
            .projection(&f.thread_key())
            .await;
        if p.revision == Some(41) && p.async_questions.first().map(|q| q.state)
            == Some(multi_agents_manager_lib::services::codex_desktop_ipc::async_question::InteractionState::Answered)
        {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "已答快照未吸收");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // 当前目标（B）没有任何成功证据 → 异步答题能力不得由历史授权
    f.state
        .codex_control
        .adapter
        .set_package_sha_override(Some("sha-current-b".into()));
    let projection = f
        .state
        .codex_control
        .adapter
        .projection(&f.thread_key())
        .await;
    let async_op = projection
        .effective_operations
        .get(multi_agents_manager_lib::services::codex_desktop_ipc::capability::CodexOperation::AsyncAnswerIdle)
        .expect("status");
    // 当前目标的资格不得被历史已答提升为 **ObservedSuccess**（契约匹配等其它
    // 独立证据可照常存在；本条只验归因）。
    assert_ne!(
        async_op.evidence,
        multi_agents_manager_lib::services::codex_desktop_ipc::capability::EvidenceSource::ObservedSuccess,
        "B 目标不得由 A 的历史已答获得真实成功证据"
    );
    // 重放旧题也零成功行
    let body = serde_json::json!({
        "sessionId": THREAD,
        "actionId": "act-h01b-replay",
        "questionKey": question_key,
        "contextToken": context_token,
        "answer": "乙",
    });
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let rows: i64 = f
        .conn
        .lock()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM codex_operation_success", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(rows, 0, "历史已答不得写当前目标成功证据");
}

// ============================================================
// H04（第三轮）：A 已写等 ACK → 同 ID 零预算重放不改账本；真实 ACK 仍 Accepted
// ============================================================

#[tokio::test]
async fn f08p_h04b_zero_budget_replay_preserves_inflight_action() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    drive_ensure(
        &mut peer,
        THREAD,
        vec![serde_json::json!({
            "id": 98, "method": "item/commandExecution/requestApproval",
            "params": { "command": "pnpm test", "cwd": "/w",
                        "availableDecisions": ["accept", "cancel"] }
        })],
        "active",
    )
    .await;
    let _ = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .unwrap()
        .unwrap();
    let projection = f
        .state
        .codex_control
        .adapter
        .projection(&f.thread_key())
        .await;
    let pending = projection.pending.first().expect("pending").clone();
    let body = serde_json::json!({
        "sessionId": THREAD,
        "actionId": "act-h04b",
        "requestKey": pending.request_key,
        "contextToken": pending.context_token,
        "action": "approve",
        "frontBudgetMs": 10_000,
    });
    let app = f.app.clone();
    let first = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/pending-response",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
    });
    // 首写帧出去，但暂不回 ACK（账本停在 Reserved）
    let frame = recv_within(&mut peer, 10).await;
    assert_eq!(frame["method"], "thread-follower-command-approval-decision");
    let state = f
        .hub_store()
        .get_for_device("act-h04b", "dev-1")
        .unwrap()
        .unwrap();
    assert_eq!(
        state.state,
        multi_agents_manager_lib::services::codex_desktop_ipc::types::ActionState::Reserved
    );
    // 同 ID、同载荷、零预算重放：不得改写原动作
    let replay_body = serde_json::json!({
        "sessionId": THREAD,
        "actionId": "act-h04b",
        "requestKey": pending.request_key,
        "contextToken": pending.context_token,
        "action": "approve",
        "frontBudgetMs": 0,
    });
    let replay = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/pending-response",
            Some(COOKIE),
            Some(replay_body.to_string()),
        ))
        .await
        .unwrap();
    let rv: serde_json::Value = serde_json::from_str(&body_string(replay).await).unwrap();
    assert_eq!(
        rv["delivery"], "unknown_delivery",
        "在途动作重放应按真实状态回 Unknown"
    );
    let still = f
        .hub_store()
        .get_for_device("act-h04b", "dev-1")
        .unwrap()
        .unwrap();
    assert_eq!(
        still.state,
        multi_agents_manager_lib::services::codex_desktop_ipc::types::ActionState::Reserved,
        "零预算重放不得把在途动作改成 NotSent/Rejected"
    );
    // 真实 ACK → 原动作正常 Accepted（无第二帧）
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-command-approval-decision",
        "result": {"result": {}}
    }))
    .await;
    let receipt = tokio::time::timeout(Duration::from_secs(15), first)
        .await
        .expect("first in time")
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body_string(receipt).await).unwrap();
    assert_eq!(v["delivery"], "accepted");
    let done = f
        .hub_store()
        .get_for_device("act-h04b", "dev-1")
        .unwrap()
        .unwrap();
    assert_eq!(
        done.state,
        multi_agents_manager_lib::services::codex_desktop_ipc::types::ActionState::Accepted
    );
    let frames = drain_frames(&mut peer).await;
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame["method"] == "thread-follower-command-approval-decision")
            .count(),
        0,
        "重放与 ACK 都不得产生第二次写帧"
    );
}

// ============================================================
// H05（第三轮）：同 ID 零预算重放不得删除原 Unknown 占用
// ============================================================

#[tokio::test]
async fn f08p_h05b_replay_does_not_delete_original_claim() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let (question_key, context_token) = follow_with_async_question(&f, &mut peer, "idle").await;
    // 造一个已 Unknown 的原动作占用（真实动作行 + claim）
    let payload =
        multi_agents_manager_lib::services::codex_control::ActionPayload::Text("甲".into());
    let hash = multi_agents_manager_lib::services::codex_control::payload_hash(
        multi_agents_manager_lib::services::codex_desktop_ipc::types::ActionKind::AsyncQuestion,
        &f.thread_key(),
        Some(&question_key),
        Some(&context_token),
        &payload,
    );
    f.conn
        .lock()
        .unwrap()
        .execute(
            "INSERT INTO codex_actions
              (action_id, device_id, device_name, thread_id, kind, request_key,
               context_token, payload_hash, client_message_id, state, route,
               created_at, updated_at)
             VALUES ('act-h05b','dev-1','iPhone',?1,'async_question',?2,?3,?4,'c',
                     'unknown','desktop_ipc',1,1)",
            rusqlite::params![THREAD, question_key, context_token, hash],
        )
        .unwrap();
    // 原占用：同一 action 的 unknown claim（模拟“已发出、结果未知”）
    f.conn
        .lock()
        .unwrap()
        .execute(
            "INSERT INTO codex_interaction_claims
              (thread_id, question_key, action_id, context_token, state, claimed_epoch,
               claimed_owner, created_at, updated_at)
             VALUES (?1, ?2, 'act-h05b', ?3, 'unknown', 0, 'boot-test:0:0', 1, 1)",
            rusqlite::params![THREAD, question_key, context_token],
        )
        .unwrap();
    let body = serde_json::json!({
        "sessionId": THREAD,
        "actionId": "act-h05b",
        "questionKey": question_key,
        "contextToken": context_token,
        "answer": "甲",
        "frontBudgetMs": 0,
    });
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let claims: i64 = f
        .conn
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM codex_interaction_claims WHERE action_id = 'act-h05b'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(claims, 1, "同 ID 零预算重放不得删除原 Unknown 占用");
    let state = f
        .hub_store()
        .get_for_device("act-h05b", "dev-1")
        .unwrap()
        .unwrap();
    assert_eq!(
        state.state,
        multi_agents_manager_lib::services::codex_desktop_ipc::types::ActionState::Unknown,
        "原账本未知语义必须保持"
    );
}

// ============================================================
// H02（第三轮）：interrupt 写边界许可绑设备——撤设备即零帧
// ============================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn f08p_h02b_interrupt_device_revoked_at_write_boundary() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    drive_ensure(&mut peer, THREAD, vec![], "active").await;
    let _ = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .unwrap()
        .unwrap();
    install_interrupt_contract(&f);
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": THREAD, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": 26,
                "conversationState": {
                    "turns": [], "requests": [],
                    "threadRuntimeStatus": { "type": "active" },
                    "turnHistory": {"history": {
                        "isComplete": true,
                        "entitiesByKey": {"turn:active": {
                            "turnId": "turn-active", "status": "inProgress", "items": []
                        }},
                        "islands": [{"id": "i", "entries": [
                            {"key": "turn:active", "value": "turn:active"}
                        ]}]
                    }}
                }
            }
        }
    }))
    .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while f
        .state
        .codex_control
        .adapter
        .mirror_active_turn_id(&f.thread_key())
        .await
        .as_deref()
        != Some("turn-active")
    {
        assert!(tokio::time::Instant::now() < deadline, "活动回合未吸收");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let barrier = Arc::new(std::sync::Barrier::new(2));
    f.state
        .codex_control
        .adapter
        .set_interaction_recheck_barrier(Some(barrier.clone()));
    let body = serde_json::json!({ "sessionId": THREAD, "turnId": "turn-active" });
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/turn/interrupt",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !f.state.codex_control.adapter.interaction_recheck_reached() {
        assert!(tokio::time::Instant::now() < deadline, "未到达写边界复核");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    f.state.store.with(|c| {
        multi_agents_manager_lib::remote::pairing::revoke_device(c, "dev-1").unwrap();
    });
    let release = barrier.clone();
    let _ = tokio::task::spawn_blocking(move || release.wait()).await;
    f.state
        .codex_control
        .adapter
        .set_interaction_recheck_barrier(None);
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("in time")
        .unwrap();
    assert!(
        r.status() == 200 || r.status() == 409 || r.status() == 503,
        "必须明确未送达：{}",
        r.status()
    );
    let frames = drain_frames(&mut peer).await;
    assert!(
        frames
            .iter()
            .all(|frame| frame["method"] != "thread-follower-interrupt-turn"),
        "撤设备后零打断帧：{frames:?}"
    );
}

// ============================================================
// N03：writer 授予首写权前核硬截止（interphase 停到有效期之后 → 零首写）
// ============================================================

#[tokio::test]
async fn f08p_n03_writer_deadline_blocks_late_first_write() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    drive_ensure(
        &mut peer,
        THREAD,
        vec![serde_json::json!({
            "id": 101, "method": "item/commandExecution/requestApproval",
            "params": { "command": "pnpm test", "cwd": "/w",
                        "availableDecisions": ["accept", "cancel"] }
        })],
        "active",
    )
    .await;
    let _ = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .unwrap()
        .unwrap();
    let projection = f
        .state
        .codex_control
        .adapter
        .projection(&f.thread_key())
        .await;
    let pending = projection.pending.first().expect("pending").clone();
    let gate = Arc::new(tokio::sync::Notify::new());
    f.state
        .codex_control
        .adapter
        .set_write_guard_gate(Some(gate.clone()));
    let body = serde_json::json!({
        "sessionId": THREAD,
        "actionId": "act-n03",
        "requestKey": pending.request_key,
        "contextToken": pending.context_token,
        "action": "approve",
        "frontBudgetMs": 120,
    });
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/pending-response",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !f.state.codex_control.adapter.write_guard_gate_reached() {
        assert!(tokio::time::Instant::now() < deadline, "未到达写边界");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // 停在 interphase 超过首次写出有效期
    tokio::time::sleep(Duration::from_millis(350)).await;
    f.state.codex_control.adapter.set_write_guard_gate(None);
    gate.notify_waiters();
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert!(
        v["delivery"] == "not_sent" || v["delivery"] == "unknown_delivery",
        "到点后只能零投递/未知：{v}"
    );
    let frames = drain_frames(&mut peer).await;
    assert!(
        frames
            .iter()
            .all(|frame| frame["method"] != "thread-follower-command-approval-decision"),
        "过期未发帧不得首次写出：{frames:?}"
    );
}

// ============================================================
// N02：别端 cmid 的正式答复不得结算本动作成功
// ============================================================

#[tokio::test]
async fn f08p_n02_foreign_cmid_does_not_settle_success() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let (question_key, context_token) = follow_with_async_question(&f, &mut peer, "idle").await;
    f.state
        .codex_control
        .adapter
        .set_probe_key_override(Some("macos|26.1021.31345|cli|1".into()));
    f.state
        .codex_control
        .adapter
        .set_package_sha_override(Some("sha-n02".into()));
    let body = serde_json::json!({
        "sessionId": THREAD,
        "actionId": "act-n02",
        "questionKey": question_key,
        "contextToken": context_token,
        "answer": "甲",
    });
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
    });
    let frame = recv_within(&mut peer, 10).await;
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "result": {"result": {"turn": {"id": "turn-foreign", "status": "inProgress"}}}
    }))
    .await;
    let _ = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .unwrap()
        .unwrap();
    // 快照：同题已 Answered，但答复消息属于**另一个 clientId**
    let envelope = format!(
        "<send_user_message_question_reply>\n[{{\"questionItemId\":{},\"question\":\"选一个\",\"answer\":\"甲\"}}]\n</send_user_message_question_reply>",
        serde_json::to_string(&question_key).unwrap()
    );
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": THREAD, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": 25,
                "conversationState": {
                    "turns": [], "requests": [],
                    "threadRuntimeStatus": { "type": "idle" },
                    "turnHistory": {"history": {
                        "isComplete": true,
                        "entitiesByKey": {
                            "turn:t-q": {
                                "turnId": "t-q", "status": "completed",
                                "items": [{
                                    "type": "agentMessage", "id": "call-q", "text": "请选择",
                                    "phase": "final_answer", "delivery": "async",
                                    "questions": [{"title": "选一个", "options": ["甲"]}]
                                }]
                            },
                            "turn:turn-foreign": {
                                "turnId": "turn-foreign", "status": "completed",
                                "items": [{
                                    "type": "userMessage", "id": "u-foreign",
                                    "clientId": "another-device-cmid",
                                    "content": [{"type": "text", "text": envelope, "text_elements": []}]
                                }]
                            }
                        },
                        "islands": [{"id": "i", "entries": [
                            {"key": "turn:t-q", "value": "turn:t-q"},
                            {"key": "turn:turn-foreign", "value": "turn:turn-foreign"}
                        ]}]
                    }}
                }
            }
        }
    }))
    .await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let rows: i64 = f
        .conn
        .lock()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM codex_operation_success", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(rows, 0, "别端 cmid 的答复不得冒充本动作成功");
}

// ============================================================
// N05：零预算回放必须核对完整载荷（改答案 → 冲突，不改账本）
// ============================================================

#[tokio::test]
async fn f08p_n05_replay_with_changed_answer_conflicts() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let (question_key, context_token) = follow_with_async_question(&f, &mut peer, "idle").await;
    // 先造一个 Accepted 的原动作（含完整 payload_hash）
    let payload =
        multi_agents_manager_lib::services::codex_control::ActionPayload::Text("甲".into());
    let hash = multi_agents_manager_lib::services::codex_control::payload_hash(
        multi_agents_manager_lib::services::codex_desktop_ipc::types::ActionKind::AsyncQuestion,
        &f.thread_key(),
        Some(&question_key),
        Some(&context_token),
        &payload,
    );
    f.conn
        .lock()
        .unwrap()
        .execute(
            "INSERT INTO codex_actions
              (action_id, device_id, device_name, thread_id, kind, request_key,
               context_token, payload_hash, client_message_id, state, route,
               created_at, updated_at)
             VALUES ('act-n05','dev-1','iPhone',?1,'async_question',?2,?3,?4,'c',
                     'accepted','desktop_ipc',1,1)",
            rusqlite::params![THREAD, question_key, context_token, hash],
        )
        .unwrap();
    let body = serde_json::json!({
        "sessionId": THREAD,
        "actionId": "act-n05",
        "questionKey": question_key,
        "contextToken": context_token,
        "answer": "乙",
        "frontBudgetMs": 0,
    });
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap();
    // 端点对 action_conflict 的 HTTP 语义 = 409（机器码错误体），不是 200 回执。
    assert_eq!(r.status(), 409);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["error"], "action_conflict");
    let state: String = f
        .conn
        .lock()
        .unwrap()
        .query_row(
            "SELECT state FROM codex_actions WHERE action_id = 'act-n05'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state, "accepted", "冲突不得改原账本");
}

// ============================================================
// N06：迟到明确拒绝必须同事务释放同题占用（Unknown 不再永久排他）
// ============================================================

#[tokio::test]
async fn f08p_n06_late_rejected_settles_claim() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let (question_key, context_token) = follow_with_async_question(&f, &mut peer, "idle").await;
    let body = serde_json::json!({
        "sessionId": THREAD,
        "actionId": "act-n06",
        "questionKey": question_key,
        "contextToken": context_token,
        "answer": "甲",
        "frontBudgetMs": 100,
    });
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
    });
    // 首帧发出后不回 ACK（前台到期 → 账本 unknown → 任务继续等迟到回执）
    let frame = recv_within(&mut peer, 10).await;
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    // 迟到明确拒绝（错误回执）
    tokio::time::sleep(Duration::from_millis(150)).await;
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "error", "method": "thread-follower-start-turn",
        "error": { "code": "denied", "message": "test" }
    }))
    .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let state: Option<String> = f
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT state FROM codex_actions WHERE action_id = 'act-n06'",
                [],
                |r| r.get(0),
            )
            .ok();
        let claims: i64 = f
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM codex_interaction_claims WHERE action_id = 'act-n06'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        if state.as_deref() == Some("rejected") && claims == 0 {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "迟到明确拒绝应收敛账本并释放占用（state={state:?} claims={claims}）"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ============================================================
// B05：同组多题一次动作、一次正式交付（async-answer-group）
// ============================================================

/// 同一次异步发题含 `count` 道题（同一 turn/item），返回 (questionKey,token) 列表。
async fn follow_with_async_group(
    f: &Fixture,
    peer: &mut ScriptedPeer,
    runtime: &str,
    count: usize,
) -> Vec<(String, String)> {
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap()
    });
    handshake(peer, "owner-1").await;
    let disc = peer.recv().await;
    assert_eq!(disc["method"], "thread-owner-discovery");
    peer.send(&discovery_response(
        disc["requestId"].as_str().unwrap(),
        "owner-1",
    ))
    .await;
    let follow = peer.recv().await;
    assert_eq!(follow["method"], "thread-stream-following-changed");
    let questions: Vec<serde_json::Value> = (0..count)
        .map(|i| {
            serde_json::json!({
                "title": format!("问题{}", i + 1),
                "options": ["甲", "乙"]
            })
        })
        .collect();
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": THREAD, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": 9,
                "conversationState": {
                    "turns": [], "requests": [],
                    "threadRuntimeStatus": { "type": runtime },
                    "turnHistory": {"history": {
                        "isComplete": true,
                        "entitiesByKey": {"turn:t-q": {
                            "turnId": "t-q", "status": "completed",
                            "items": [{
                                "type": "agentMessage", "id": "call-q",
                                "text": "请选择", "phase": "final_answer",
                                "delivery": "async", "questions": questions
                            }]
                        }},
                        "islands": [{"id": "i", "entries": [
                            {"key": "turn:t-q", "value": "turn:t-q"}
                        ]}]
                    }}
                }
            }
        }
    }))
    .await;
    let _ = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("ensure in time")
        .unwrap();
    install_async_answer_contract(f);
    let r = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    v["asyncQuestions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|q| {
            (
                q["questionKey"].as_str().unwrap().to_string(),
                q["contextToken"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn group_body(action_id: &str, questions: &[(String, String)], answer: &str) -> String {
    serde_json::json!({
        "sessionId": THREAD,
        "actionId": action_id,
        "answers": questions.iter().map(|(q, t)| serde_json::json!({
            "questionKey": q, "contextToken": t, "answer": answer
        })).collect::<Vec<_>>(),
    })
    .to_string()
}

#[tokio::test]
async fn b05_group_answer_sends_one_frame_with_all_answers() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let questions = follow_with_async_group(&f, &mut peer, "idle", 5).await;
    assert_eq!(questions.len(), 5);
    let body = group_body("act-g5", &questions, "甲");
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer-group",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    let frame = recv_within(&mut peer, 10).await;
    assert_eq!(frame["method"], "thread-follower-start-turn");
    let text = frame["params"]["turnStart"]["request"]["input"][0]["text"]
        .as_str()
        .expect("text");
    let body = text
        .trim_start_matches("<send_user_message_question_reply>")
        .trim_end_matches("</send_user_message_question_reply>")
        .trim();
    let parsed: Vec<serde_json::Value> = serde_json::from_str(body).expect("envelope array");
    assert_eq!(parsed.len(), 5);
    let sent_keys: Vec<&str> = parsed
        .iter()
        .map(|entry| entry["questionItemId"].as_str().unwrap())
        .collect();
    for (q, _) in &questions {
        assert!(sent_keys.contains(&q.as_str()), "整组信封缺题 {q}: {text}");
    }
    assert!(parsed
        .iter()
        .all(|entry| entry["answer"].as_str() == Some("甲")));
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": {"result": {"turn": {"id": "turn-g5", "status": "inProgress"}}}
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["delivery"], "accepted");
    // 同一动作同载荷重放：返回原回执，绝不发第二帧。
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer-group",
            Some(COOKIE),
            Some(group_body("act-g5", &questions, "甲")),
        ))
        .await
        .unwrap()
    });
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(!peer_has_traffic(&mut peer).await, "重放不得产生第二帧");
}

#[tokio::test]
async fn b05_group_rejects_missing_member_and_changed_answer() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let questions = follow_with_async_group(&f, &mut peer, "idle", 3).await;
    // 缺一题 → 409 answers_mismatch，零帧。
    let partial = group_body("act-partial", &questions[..2], "甲");
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer-group",
            Some(COOKIE),
            Some(partial),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 409);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["error"], "answers_mismatch");
    // 正常提交后改答案重放 → 冲突，不改原动作。
    let app = f.app.clone();
    let body = group_body("act-g3", &questions, "甲");
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer-group",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    let frame = recv_within(&mut peer, 10).await;
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": {"result": {"turn": {"id": "turn-g3", "status": "inProgress"}}}
    }))
    .await;
    let _ = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .unwrap()
        .unwrap();
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer-group",
            Some(COOKIE),
            Some(group_body("act-g3", &questions, "乙")),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 409, "改答案必须冲突");
    assert!(!peer_has_traffic(&mut peer).await, "冲突不得产生新帧");
}

// ============================================================
// B01/B06：可用性与暂停一键提交
// ============================================================

#[tokio::test]
async fn b01_contract_match_makes_operations_available_consistently() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let _ = follow_with_async_group(&f, &mut peer, "idle", 2).await;
    // 当前没有任何历史成功记录：契约匹配本身必须足以放行（B01）。
    let successes: i64 = f
        .conn
        .lock()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM codex_operation_success", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(successes, 0);
    let r = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["availability"], "controllable", "契约匹配后应可交互: {v}");
    assert_eq!(
        v["effectiveOperations"]["operations"]["send"]["allowed"],
        true
    );
    assert_eq!(
        v["effectiveOperations"]["operations"]["async_answer_idle"]["allowed"],
        true
    );
    assert_eq!(
        v["effectiveOperations"]["operations"]["async_answer_idle"]["evidence"],
        "contract_matched"
    );
    assert_eq!(v["asyncQuestions"][0]["actionable"], true);
}

#[tokio::test]
async fn b06_paused_queue_group_submit_sends_once_and_keeps_pause() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let questions = follow_with_async_group(&f, &mut peer, "idle", 2).await;
    f.hub_store()
        .set_queue_pause(&f.thread_key(), "interrupt", "tok-b06")
        .unwrap();
    let body = group_body("act-b06", &questions, "甲");
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer-group",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    let frame = recv_within(&mut peer, 10).await;
    assert_eq!(frame["method"], "thread-follower-start-turn");
    // 只发一帧（本组答案），旧队列保持暂停。
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": {"result": {"turn": {"id": "turn-b06", "status": "inProgress"}}}
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(
        f.hub_store()
            .queue_pause(&f.thread_key())
            .unwrap()
            .is_some(),
        "本组提交不得恢复旧队列"
    );
}

#[tokio::test]
async fn b05_group_rejects_already_answered_and_bad_token() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let questions = follow_with_async_group(&f, &mut peer, "idle", 2).await;
    // 别端已正式回答其中一题（占用表 answered + 真实动作行，防孤儿接管）→ 整组拒绝，零帧。
    f.conn
        .lock()
        .unwrap()
        .execute(
            "INSERT INTO codex_actions
              (action_id, device_id, device_name, thread_id, kind, request_key,
               context_token, payload_hash, client_message_id, state, route,
               created_at, updated_at)
             VALUES ('other-action','dev-2','iPad',?1,'async_question',?2,?3,'h','c',
                     'unknown','desktop_ipc',1,1)",
            rusqlite::params![THREAD, questions[0].0, questions[0].1],
        )
        .unwrap();
    f.conn
        .lock()
        .unwrap()
        .execute(
            "INSERT INTO codex_interaction_claims
              (thread_id, question_key, action_id, context_token, state, claimed_epoch,
               claimed_owner, created_at, updated_at)
             VALUES (?1, ?2, 'other-action', ?3, 'answered', 0, 'other-boot:1:0', 1, 1)",
            rusqlite::params![THREAD, questions[0].0, questions[0].1],
        )
        .unwrap();
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer-group",
            Some(COOKIE),
            Some(group_body("act-answered", &questions, "甲")),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 409);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["error"], "question_already_answered");
    assert!(!peer_has_traffic(&mut peer).await, "已答组拒绝 = 零帧");
    // 改 token → 过期上下文，零帧。
    let mut bad = questions.clone();
    bad[1].1 = "tok-forged".into();
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer-group",
            Some(COOKIE),
            Some(group_body("act-badtok", &bad, "甲")),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 409);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["error"], "stale_context");
    assert!(!peer_has_traffic(&mut peer).await, "换 token = 零帧");
}

// ============================================================
// 第五轮集中复审：G01 / G05 / G07
// ============================================================

#[tokio::test]
async fn g01_adapt_on_after_readonly_connect_actually_writes() {
    let mut f = fixture_ipc_unknown_build(vec![codex_app_session(THREAD)]);
    {
        let conn = f.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO settings (key, value)
             VALUES ('remote.codex_auto_adapt','false')",
            [],
        )
        .unwrap();
    }
    let mut peer = f.peer.take().unwrap();
    let questions = follow_with_async_group(&f, &mut peer, "idle", 2).await;
    // 读链已建立（只读档），但 send 位关闭。开启自动适配（模拟用户开关）后，
    // 实际派发必须与 GET 结论一致：一帧，而不是 capability_unavailable。
    {
        let conn = f.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO settings (key, value)
             VALUES ('remote.codex_auto_adapt','true')",
            [],
        )
        .unwrap();
    }
    let body = group_body("act-g01", &questions, "甲");
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer-group",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    let frame = recv_within(&mut peer, 10).await;
    assert_eq!(frame["method"], "thread-follower-start-turn");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": {"result": {"turn": {"id": "turn-g01", "status": "inProgress"}}}
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("in time")
        .unwrap();
    assert_eq!(r.status(), 200);
}

#[tokio::test]
async fn g05_reusing_action_id_cannot_delete_original_claims() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let questions = follow_with_async_group(&f, &mut peer, "idle", 2).await;
    // A：同 ID 首次提交（未知投递，占用保留）。
    let body = serde_json::json!({
        "sessionId": THREAD,
        "actionId": "act-shared",
        "answers": questions.iter().map(|(q, t)| serde_json::json!({
            "questionKey": q, "contextToken": t, "answer": "甲"
        })).collect::<Vec<_>>(),
        "frontBudgetMs": 120,
    });
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer-group",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
    });
    let frame = recv_within(&mut peer, 10).await;
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("in time")
        .unwrap();
    assert_eq!(r.status(), 200);
    let _ = frame; // 故意不回 ACK：动作停在 Unknown，占用必须保留
    let before: i64 = f
        .conn
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM codex_interaction_claims WHERE action_id='act-shared'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(before, 2);
    // B：同 ID、不同答案 → 身份冲突，必须在占用前拒绝。
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer-group",
            Some(COOKIE),
            Some(group_body("act-shared", &questions, "乙")),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 409);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["error"], "action_conflict");
    let after: i64 = f
        .conn
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM codex_interaction_claims WHERE action_id='act-shared'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(after, 2, "冲突请求不得删除原动作占用");
    assert!(!peer_has_traffic(&mut peer).await, "冲突零帧");
}

#[tokio::test]
async fn g07_reversed_order_is_same_action_identity() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let questions = follow_with_async_group(&f, &mut peer, "idle", 2).await;
    let mut reversed = questions.clone();
    reversed.reverse();
    let body = group_body("act-order", &reversed, "甲");
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer-group",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    let frame = recv_within(&mut peer, 10).await;
    assert_eq!(frame["method"], "thread-follower-start-turn");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": {"result": {"turn": {"id": "turn-order", "status": "inProgress"}}}
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("in time")
        .unwrap();
    assert_eq!(r.status(), 200, "逆序首次提交必须成功");
    // 任意顺序重放 → 同载荷返回原回执，零第二帧。
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer-group",
            Some(COOKIE),
            Some(group_body("act-order", &questions, "甲")),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(!peer_has_traffic(&mut peer).await, "重放零帧");
}

async fn send_two_group_snapshot(peer: &mut ScriptedPeer, revision: u64, q1_extra: &[&str]) {
    let alpha: Vec<serde_json::Value> = (0..2)
        .map(|i| serde_json::json!({"title": format!("A{i}"), "options": ["甲", "乙"]}))
        .collect();
    let beta: Vec<serde_json::Value> = (0..2)
        .map(|i| {
            let mut options = vec!["甲", "乙"];
            options.extend_from_slice(q1_extra);
            serde_json::json!({"title": format!("B{i}"), "options": options})
        })
        .collect();
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": THREAD, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": revision,
                "conversationState": {
                    "turns": [], "requests": [],
                    "threadRuntimeStatus": { "type": "idle" },
                    "turnHistory": {"history": {
                        "isComplete": true,
                        "entitiesByKey": {"turn:t-q": {
                            "turnId": "t-q", "status": "completed",
                            "items": [
                                {"type": "agentMessage", "id": "call-a", "text": "A",
                                 "phase": "final_answer", "delivery": "async", "questions": alpha},
                                {"type": "agentMessage", "id": "call-b", "text": "B",
                                 "phase": "final_answer", "delivery": "async", "questions": beta}
                            ]
                        }},
                        "islands": [{"id": "i", "entries": [
                            {"key": "turn:t-q", "value": "turn:t-q"}
                        ]}]
                    }}
                }
            }
        }
    }))
    .await;
}

#[tokio::test]
async fn g06_cross_group_member_replacement_is_rejected() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let first = follow_with_async_group(&f, &mut peer, "idle", 1).await;
    let _ = first;
    send_two_group_snapshot(&mut peer, 20, &[]).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let r = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    let all = v["asyncQuestions"].as_array().unwrap();
    let group_a: Vec<_> = all
        .iter()
        .filter(|q| q["sourceItemId"] == "call-a")
        .map(|q| {
            (
                q["questionKey"].as_str().unwrap().to_string(),
                q["contextToken"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let group_b: Vec<_> = all
        .iter()
        .filter(|q| q["sourceItemId"] == "call-b")
        .map(|q| {
            (
                q["questionKey"].as_str().unwrap().to_string(),
                q["contextToken"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(group_a.len(), 2);
    assert_eq!(group_b.len(), 2);
    // [a0, b1]：每题 token 都有效、数量也等于 2，但成员跨组。
    let mixed = vec![group_a[0].clone(), group_b[1].clone()];
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer-group",
            Some(COOKIE),
            Some(group_body("act-mixed", &mixed, "甲")),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 409);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["error"], "answers_mismatch");
    assert!(!peer_has_traffic(&mut peer).await, "跨组替换零帧");
}

#[tokio::test]
async fn g04_writer_recheck_catches_non_first_member_change() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let questions = follow_with_async_group(&f, &mut peer, "idle", 2).await;
    let gate = Arc::new(tokio::sync::Notify::new());
    f.state
        .codex_control
        .adapter
        .set_write_guard_gate(Some(gate.clone()));
    let body = group_body("act-g04", &questions, "甲");
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer-group",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !f.state.codex_control.adapter.write_guard_gate_reached() {
        assert!(tokio::time::Instant::now() < deadline, "未到达写边界");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // 只改第二题（首题 token 不变）→ 最终写权复核必须拒绝整组。
    let q0 = serde_json::json!({"title": "问题1", "options": ["甲", "乙"]});
    let q1 = serde_json::json!({"title": "问题2", "options": ["甲", "乙", "丙"]});
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": THREAD, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": 30,
                "conversationState": {
                    "turns": [], "requests": [],
                    "threadRuntimeStatus": { "type": "idle" },
                    "turnHistory": {"history": {
                        "isComplete": true,
                        "entitiesByKey": {"turn:t-q": {
                            "turnId": "t-q", "status": "completed",
                            "items": [{"type": "agentMessage", "id": "call-q",
                                       "text": "请选择", "phase": "final_answer",
                                       "delivery": "async", "questions": [q0, q1]}]
                        }},
                        "islands": [{"id": "i", "entries": [
                            {"key": "turn:t-q", "value": "turn:t-q"}
                        ]}]
                    }}
                }
            }
        }
    }))
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    f.state.codex_control.adapter.set_write_guard_gate(None);
    gate.notify_waiters();
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("in time")
        .unwrap();
    assert!(
        r.status() == 409 || r.status() == 200,
        "预期明确零投递，实际 {}",
        r.status()
    );
    let frames = drain_frames(&mut peer).await;
    assert!(
        frames
            .iter()
            .all(|frame| frame["method"] != "thread-follower-start-turn"),
        "第二题换代后不得首写：{frames:?}"
    );
}

#[tokio::test]
async fn g08_partial_answered_allows_explicit_remaining_submit() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let questions = follow_with_async_group(&f, &mut peer, "idle", 3).await;
    // Desktop 正式回答第 2 题（q1）：投影从正式信封派生 Answered。
    let envelope =
        multi_agents_manager_lib::services::codex_desktop_ipc::async_question::compose_async_answer(
            "call-q", 1, "问题2", "乙",
        );
    let q =
        |i: usize| serde_json::json!({"title": format!("问题{}", i + 1), "options": ["甲", "乙"]});
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": THREAD, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": 40,
                "conversationState": {
                    "turns": [], "requests": [],
                    "threadRuntimeStatus": { "type": "idle" },
                    "turnHistory": {"history": {
                        "isComplete": true,
                        "entitiesByKey": {
                            "turn:t-q": {"turnId": "t-q", "status": "completed", "items": [
                                {"type": "agentMessage", "id": "call-q", "text": "请选择",
                                 "phase": "final_answer", "delivery": "async",
                                 "questions": [q(0), q(1), q(2)]}
                            ]},
                            "turn:t-a": {"turnId": "t-a", "status": "completed", "items": [
                                {"type": "userMessage", "id": "u-a", "clientId": "desktop-cmid",
                                 "content": [{"type": "text", "text": envelope, "text_elements": []}]}
                            ]}
                        },
                        "islands": [{"id": "i", "entries": [
                            {"key": "turn:t-q", "value": "turn:t-q"},
                            {"key": "turn:t-a", "value": "turn:t-a"}
                        ]}]
                    }}
                }
            }
        }
    }))
    .await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    // 未复核：整组拒绝。
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer-group",
            Some(COOKIE),
            Some(group_body("act-g08a", &questions, "甲")),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 409);
    // 明确复核 q1 已答，只提交剩余两题 → 一帧、信封只含两题。
    let live = [questions[0].clone(), questions[2].clone()];
    let body = serde_json::json!({
        "sessionId": THREAD,
        "actionId": "act-g08b",
        "answers": live.iter().map(|(q, t)| serde_json::json!({
            "questionKey": q, "contextToken": t, "answer": "甲"
        })).collect::<Vec<_>>(),
        "acknowledgedAnswered": [questions[1].0.clone()],
    });
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer-group",
            Some(COOKIE),
            Some(body.to_string()),
        ))
        .await
        .unwrap()
    });
    let frame = recv_within(&mut peer, 10).await;
    let text = frame["params"]["turnStart"]["request"]["input"][0]["text"]
        .as_str()
        .unwrap();
    let parsed: Vec<serde_json::Value> = serde_json::from_str(
        text.trim_start_matches("<send_user_message_question_reply>")
            .trim_end_matches("</send_user_message_question_reply>")
            .trim(),
    )
    .unwrap();
    assert_eq!(parsed.len(), 2, "只派发剩余未答题: {text}");
    assert!(!text.contains(&questions[1].0), "已确认已答题不得重发");
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": {"result": {"turn": {"id": "turn-g08", "status": "inProgress"}}}
    }))
    .await;
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("in time")
        .unwrap();
    assert_eq!(r.status(), 200);
}

#[tokio::test]
async fn g08_replay_after_all_answered_returns_ledger_without_frame() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let questions = follow_with_async_group(&f, &mut peer, "idle", 2).await;
    let body = group_body("act-echo", &questions, "甲");
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer-group",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    let frame = recv_within(&mut peer, 10).await;
    peer.send(&serde_json::json!({
        "type": "response", "requestId": frame["requestId"],
        "resultType": "success", "method": "thread-follower-start-turn",
        "handledByClientId": "owner-1",
        "result": {"result": {"turn": {"id": "turn-echo", "status": "inProgress"}}}
    }))
    .await;
    let _ = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .unwrap()
        .unwrap();
    // 正式答复两题（一次信封数组）→ 投影 Answered。
    let e0 =
        multi_agents_manager_lib::services::codex_desktop_ipc::async_question::compose_async_answer(
            "call-q", 0, "问题1", "甲",
        );
    let e1 =
        multi_agents_manager_lib::services::codex_desktop_ipc::async_question::compose_async_answer(
            "call-q", 1, "问题2", "甲",
        );
    let array = format!(
        "<send_user_message_question_reply>\n[{},{}]\n</send_user_message_question_reply>",
        e0.trim_start_matches("<send_user_message_question_reply>")
            .trim_end_matches("</send_user_message_question_reply>")
            .trim(),
        e1.trim_start_matches("<send_user_message_question_reply>")
            .trim_end_matches("</send_user_message_question_reply>")
            .trim()
    );
    let q =
        |i: usize| serde_json::json!({"title": format!("问题{}", i + 1), "options": ["甲", "乙"]});
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": THREAD, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": 50,
                "conversationState": {
                    "turns": [], "requests": [],
                    "threadRuntimeStatus": { "type": "idle" },
                    "turnHistory": {"history": {
                        "isComplete": true,
                        "entitiesByKey": {
                            "turn:t-q": {"turnId": "t-q", "status": "completed", "items": [
                                {"type": "agentMessage", "id": "call-q", "text": "请选择",
                                 "phase": "final_answer", "delivery": "async",
                                 "questions": [q(0), q(1)]}
                            ]},
                            "turn:t-a": {"turnId": "t-a", "status": "completed", "items": [
                                {"type": "userMessage", "id": "u-a", "clientId": "desktop-cmid",
                                 "content": [{"type": "text", "text": array, "text_elements": []}]}
                            ]}
                        },
                        "islands": [{"id": "i", "entries": [
                            {"key": "turn:t-q", "value": "turn:t-q"},
                            {"key": "turn:t-a", "value": "turn:t-a"}
                        ]}]
                    }}
                }
            }
        }
    }))
    .await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    // 原 ID/原载荷重放：返回账本，零新帧（即使所有题已 Answered）。
    let r = f
        .app
        .clone()
        .oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer-group",
            Some(COOKIE),
            Some(group_body("act-echo", &questions, "甲")),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    assert_eq!(v["delivery"], "accepted");
    assert!(!peer_has_traffic(&mut peer).await, "重放零帧");
}

#[tokio::test]
async fn j01_final_verdict_uses_latest_full_group_facts() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let questions = follow_with_async_group(&f, &mut peer, "idle", 2).await;
    let gate = Arc::new(tokio::sync::Notify::new());
    f.state
        .codex_control
        .adapter
        .set_final_claim_gate_for_tests(Some(gate.clone()))
        .await;
    let body = group_body("act-j01", &questions, "甲");
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer-group",
            Some(COOKIE),
            Some(body),
        ))
        .await
        .unwrap()
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !f
        .state
        .codex_control
        .adapter
        .final_claim_gate_reached()
        .await
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "未到达最终写权 gate"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // hook 已返回旧快照；此刻第二题换 token → 最终 claim 必须读新事实。
    let q0 = serde_json::json!({"title": "问题1", "options": ["甲", "乙"]});
    let q1 = serde_json::json!({"title": "问题2", "options": ["甲", "乙", "丙"]});
    peer.send(&serde_json::json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-1",
        "params": {
            "conversationId": THREAD, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": 31,
                "conversationState": {
                    "turns": [], "requests": [],
                    "threadRuntimeStatus": { "type": "idle" },
                    "turnHistory": {"history": {
                        "isComplete": true,
                        "entitiesByKey": {"turn:t-q": {
                            "turnId": "t-q", "status": "completed",
                            "items": [{"type": "agentMessage", "id": "call-q",
                                       "text": "请选择", "phase": "final_answer",
                                       "delivery": "async", "questions": [q0, q1]}]
                        }},
                        "islands": [{"id": "i", "entries": [
                            {"key": "turn:t-q", "value": "turn:t-q"}
                        ]}]
                    }}
                }
            }
        }
    }))
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    f.state
        .codex_control
        .adapter
        .set_final_claim_gate_for_tests(None)
        .await;
    gate.notify_waiters();
    let r = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("in time")
        .unwrap();
    assert!(r.status() == 409 || r.status() == 200);
    let frames = drain_frames(&mut peer).await;
    assert!(
        frames
            .iter()
            .all(|frame| frame["method"] != "thread-follower-start-turn"),
        "最终写权必须核全组新事实：{frames:?}"
    );
}

/// C01：A 在 final gate 等待时，B 用同 actionId、不同组请求不得覆盖 A 的
/// GroupBinding；B 冲突失败后 A 第二题换代，最终检查仍须以 A 组为准并零帧。
#[tokio::test]
async fn c01_conflicting_group_cannot_overwrite_bound_owner() {
    let mut f = fixture_ipc_unknown_interrupt(vec![codex_app_session(THREAD)], false);
    let mut peer = f.peer.take().unwrap();
    let questions_a = follow_with_async_group(&f, &mut peer, "idle", 2).await;

    let snapshot = |revision: u64, a_q1_title: &str| {
        serde_json::json!({
            "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
            "sourceClientId": "owner-1",
            "params": {
                "conversationId": THREAD, "hostId": "local",
                "change": {
                    "type": "snapshot", "revision": revision,
                    "conversationState": {
                        "turns": [], "requests": [],
                        "threadRuntimeStatus": { "type": "idle" },
                        "turnHistory": {"history": {
                            "isComplete": true,
                            "entitiesByKey": {
                                "turn:t-q": {
                                    "turnId": "t-q", "status": "completed",
                                    "items": [{"type": "agentMessage", "id": "call-q",
                                               "text": "请选择", "phase": "final_answer",
                                               "delivery": "async",
                                               "questions": [
                                                   {"title": "问题1", "options": ["甲", "乙"]},
                                                   {"title": a_q1_title, "options": ["甲", "乙"]}
                                               ]}]
                                },
                                "turn:t-b": {
                                    "turnId": "t-b", "status": "completed",
                                    "items": [{"type": "agentMessage", "id": "call-b",
                                               "text": "请选择", "phase": "final_answer",
                                               "delivery": "async",
                                               "questions": [
                                                   {"title": "B题1", "options": ["丙", "丁"]},
                                                   {"title": "B题2", "options": ["丙", "丁"]}
                                               ]}]
                                }
                            },
                            "islands": [
                                {"id": "i", "entries": [
                                    {"key": "turn:t-q", "value": "turn:t-q"},
                                    {"key": "turn:t-b", "value": "turn:t-b"}
                                ]}
                            ]
                        }}
                    }
                }
            }
        })
    };
    peer.send(&snapshot(10, "问题2")).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let r = f
        .app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
    let pairs = |item: &str| -> Vec<(String, String)> {
        v["asyncQuestions"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|q| q["sourceItemId"] == item)
            .map(|q| {
                (
                    q["questionKey"].as_str().unwrap().to_string(),
                    q["contextToken"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    };
    let a = pairs("call-q");
    let b = pairs("call-b");
    assert_eq!(a.len(), 2);
    assert_eq!(b.len(), 2);
    assert_eq!(questions_a.len(), 2);

    let gate = Arc::new(tokio::sync::Notify::new());
    f.state
        .codex_control
        .adapter
        .set_final_claim_gate_for_tests(Some(gate.clone()))
        .await;
    let app = f.app.clone();
    let body_a = group_body("act-c01", &a, "甲");
    let task_a = tokio::spawn(async move {
        app.oneshot(req(
            "POST",
            "/m/api/v1/codex/async-answer-group",
            Some(COOKIE),
            Some(body_a),
        ))
        .await
        .unwrap()
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !f
        .state
        .codex_control
        .adapter
        .final_claim_gate_reached()
        .await
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "未到达最终写权 gate"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // B 同 actionId、不同组：组占用键不相交，但正式动作 ID 已属于 A；B 必须
    // 冲突失败，且不能把 group_bindings[X] 改成 B。
    let app_b = f.app.clone();
    let body_b = group_body("act-c01", &b, "丙");
    let r_b = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::spawn(async move {
            app_b
                .oneshot(req(
                    "POST",
                    "/m/api/v1/codex/async-answer-group",
                    Some(COOKIE),
                    Some(body_b),
                ))
                .await
                .unwrap()
        }),
    )
    .await
    .expect("B in time")
    .unwrap();
    assert_eq!(r_b.status(), 409, "B 与既有动作身份冲突");

    // A 第二题换 token（改标题），首题保持；A 绑定若仍是 A 组则必须零帧。
    peer.send(&snapshot(11, "问题2已更新")).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    f.state
        .codex_control
        .adapter
        .set_final_claim_gate_for_tests(None)
        .await;
    gate.notify_waiters();
    let r_a = tokio::time::timeout(Duration::from_secs(15), task_a)
        .await
        .expect("A in time")
        .unwrap();
    assert!(r_a.status() == 409 || r_a.status() == 200);
    let frames = drain_frames(&mut peer).await;
    assert!(
        frames
            .iter()
            .all(|frame| frame["method"] != "thread-follower-start-turn"),
        "A 最终检查必须使用 A 自己的绑定/最新事实：{frames:?}"
    );
}
