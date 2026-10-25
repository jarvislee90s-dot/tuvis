//! 工作区桥、任务索引与会话精确绑定（生产计划 T2.2，**只读**）。
//!
//! # 施工顺序（与固定源码/参照实现逐条对齐）
//!
//! 1. `bootstrap-request` → `workspace-list-request`（只读 listing，双源去重）；
//! 2. `workspace-bridge-open{requestId, bridgeSessionId, workspaceKey}`（枚举期**不**带
//!    taskId）→ 等 `workspace-bridge-ready` 的**精确回显**（身份只含请求声明过的字段，
//!    绝不虚构 `bridgeGeneration`/`recoveryId`）；
//! 3. 等桥 ChannelServer 的 `Initialize(200)`（`04 01 06 C8 01 00`；官方 ChannelClient
//!    在它之前不发任何请求）→ `helloConversationV4` → `initializeConversationV4`
//!    `{kind:"clientHello", protocolVersion:3, clientId, clientKind:"web", appVersion}`
//!    （不伪装 desktop-continuous，不提升权限）；
//! 4. WindowController：`EventListen(onDynamicControllerFrame)` → `subscribeControllerV4
//!    {topic:"controller/tasks-index", visibility:"foreground"}`（没有自造的
//!    tasks-index-request）；
//! 5. 精确绑定：会话桥 `workspace-bridge-open{..., workspaceKey, taskId}` → ready →
//!    Initialize → hello/clientHello → `EventListen(onDynamicConversationFrame, workspace)`
//!    → `subscribeConversationV4{workspacePath, workspaceIdentity?, sessionId}` →
//!    ACK 后本地 `activate(subscriptionId)` 放行暂存帧（**不存在** activateConversationV4
//!    RPC；ACK 也不代表快照/control 已到）。
//!
//! # 纪律
//!
//! - **一个 actor 独占全部出站 seq/messageSeq 分配**（[`Command`] 队列串行化发送，
//!   并发 `&self` 调用不会乱序）；requestId 在状态锁内单点分配，发送在锁外进行，
//!   绝不在状态锁内 await 网络。
//! - `bind` 只认**精确相等**的 `taskId == TargetKey.sessionId`：标题/cwd 相似度、手工去
//!   横杠、大小写归一一律不参与；两种 ID 形态（`sess_<uuid>` / `session_<32hex>`）原样
//!   保留。同 taskId 命中多行（跨 workspace 或同 workspace 重复）→ `target_ambiguous`。
//! - 入站帧按 bridgeSessionId/身份、物理 `seq`/`messageSeq`、topic、subscriptionId、
//!   代际逐层过滤；任何缺口/未知 schema/协议不符 → `ready=false`，重订阅取只读快照，
//!   期间控制面零写。
//! - 本模块**不构造任何会话写命令**（T3 起才在 `command.rs` 引入）。

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};
use tokio::sync::{mpsc, oneshot, watch, Mutex as AsyncMutex, Notify};

use super::codec::{
    decode_channel, encode_ack, encode_call, encode_frame, encode_listen, ChannelMessage,
    FrameAssembler, RpcIdentity, RESPONSE_TYPE_EVENT_FIRE, RESPONSE_TYPE_INITIALIZE,
    RESPONSE_TYPE_PROMISE_ERROR, RESPONSE_TYPE_PROMISE_SUCCESS,
};
use super::projection::{Projection, CONVERSATION_TOPIC_PREFIX};
use super::types::{codes, BoundTarget, RelaySink, SessionView, TargetKey, ZResult, ZcodeError};

/// agent 服务 channel（`channels.ts:114`）。
pub const AGENT_CHANNEL: &str = "zcode-agent";
/// 窗口 controller channel（`channels.ts:112`）。
pub const WINDOW_CONTROLLER_CHANNEL: &str = "window-controller";
/// 会话握手两步（`zcodeAgent.ts:881,883`）。
pub const AGENT_HELLO_METHOD: &str = "helloConversationV4";
/// clientHello 回送。
pub const AGENT_CLIENT_HELLO_METHOD: &str = "initializeConversationV4";
/// 会话订阅方法（`zcodeAgent.ts:940`）。
pub const AGENT_SUBSCRIBE_CONVERSATION_METHOD: &str = "subscribeConversationV4";
/// 会话 workspace 级下行帧动态事件（`zcodeAgent.ts:956-960`）。
pub const AGENT_CONVERSATION_FRAME_EVENT: &str = "onDynamicConversationFrame";
/// controller 动态帧事件（`windowController.ts:75-78`）。
pub const CONTROLLER_DYNAMIC_FRAME_EVENT: &str = "onDynamicControllerFrame";
/// controller 订阅方法。
pub const CONTROLLER_SUBSCRIBE_METHOD: &str = "subscribeControllerV4";
/// controller 任务索引 topic（`controller.ts:9`）。
pub const CONTROLLER_TASKS_INDEX_TOPIC: &str = "controller/tasks-index";
/// v4 wire 协议版本（`core.ts:7`）。
pub const V4_WIRE_PROTOCOL_VERSION: u64 = 3;
/// 工作区桥打开请求类型（app payload）。
pub const WORKSPACE_BRIDGE_OPEN: &str = "workspace-bridge-open";
/// 工作区桥打开成功应答类型。
pub const WORKSPACE_BRIDGE_READY: &str = "workspace-bridge-ready";
/// 工作区桥打开失败应答类型。
pub const WORKSPACE_BRIDGE_ERROR: &str = "workspace-bridge-error";
/// bootstrap 只读请求类型。
pub const BOOTSTRAP_REQUEST: &str = "bootstrap-request";
/// workspace 列表只读请求类型。
pub const WORKSPACE_LIST_REQUEST: &str = "workspace-list-request";
/// 桥 Initialize 的 wire 字节（`serialize([200]) + serialize(undefined)`）。
pub const INITIALIZE_BYTES: [u8; 6] = [0x04, 0x01, 0x06, 0xc8, 0x01, 0x00];
/// 单条 conversation 帧暂存上限（ACK 前/重订阅窗口；防恶意帧撑爆内存）。
pub const MAX_STAGED_FRAMES: usize = 1024;

/// clientHello 的 Tuvis 客户端身份（web 终端；不伪装 desktop-continuous）。
const CLIENT_KIND: &str = "web";
/// clientHello 的 appVersion（Tuvis 版本标识，不含机器/账号信息）。
const APP_VERSION: &str = "tuvis";
/// clientHello 的 clientId（稳定、无秘密）。
const CLIENT_ID: &str = "tuvis-remote";

fn unsupported() -> ZcodeError {
    ZcodeError::of(codes::UNSUPPORTED_PROTOCOL)
}

fn stale() -> ZcodeError {
    ZcodeError::of(codes::STALE_GENERATION)
}

fn lock(state: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 生产时序（探针既有口径；测试用 [`BridgeTiming::test`] 缩短）。
#[derive(Debug, Clone, Copy)]
pub struct BridgeTiming {
    /// app payload（bootstrap/workspace-list/bridge-open）应答窗：10s。
    pub app_request: Duration,
    /// 桥 Initialize 门：15s。
    pub initialize: Duration,
    /// 桥 RPC（hello/clientHello/subscribe/普通读调用）：10s。
    pub rpc: Duration,
    /// controller 快照收帧窗：8s。
    pub task_window: Duration,
}

impl Default for BridgeTiming {
    fn default() -> Self {
        Self {
            app_request: Duration::from_secs(10),
            initialize: Duration::from_secs(15),
            rpc: Duration::from_secs(10),
            task_window: Duration::from_secs(8),
        }
    }
}

impl BridgeTiming {
    /// 测试时序（毫秒级，不真等生产窗口）。
    #[cfg(test)]
    pub(crate) const fn test() -> Self {
        Self {
            app_request: Duration::from_millis(500),
            initialize: Duration::from_millis(500),
            rpc: Duration::from_millis(500),
            task_window: Duration::from_millis(120),
        }
    }
}

/// 出站命令（唯一写 socket 的路径；actor 在此分配 seq/messageSeq 并串行化发送）。
enum Command {
    /// 发送一条普通 RPC（请求/应答配对由状态锁里的 waiter 完成）。
    SendRpc {
        bridge_id: String,
        id: u32,
        channel: String,
        method: String,
        args: Vec<Value>,
        reply: oneshot::Sender<ZResult<()>>,
    },
    /// 发送一条事件订阅（EventListen）。
    SendListen {
        bridge_id: String,
        id: u32,
        channel: String,
        event: String,
        arg: Value,
        reply: oneshot::Sender<ZResult<()>>,
    },
    /// 发送一条已构造好的 data payload（app payload / rpc-frame-ack）。
    SendFrame {
        payload: Value,
        reply: Option<oneshot::Sender<ZResult<()>>>,
    },
    /// 清空 seq/messageSeq 计数（代际变化后旧桥作废）。
    Reset,
}

/// 每桥出站计数（actor 独占；`seq`/`messageSeq` 从 1 起）。
#[derive(Default)]
struct SeqCounters {
    seq: u64,
    message_seq: u64,
}

/// 跨桥共享的可变状态（所有字段都在同一把状态锁下线性化）。
struct Inner {
    generation: u64,
    bridges: HashMap<String, Bridge>,
    sessions: HashMap<String, SessionEntry>,
    workspaces: Option<WorkspaceCache>,
    app_waiters: HashMap<String, oneshot::Sender<Value>>,
    next_app_request: u64,
    next_bridge_id: u64,
    /// controller 任务索引帧到达通知（收帧窗等待用）。
    enum_notify: Arc<Notify>,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            generation: 0,
            bridges: HashMap::new(),
            sessions: HashMap::new(),
            workspaces: None,
            app_waiters: HashMap::new(),
            next_app_request: 0,
            next_bridge_id: 0,
            enum_notify: Arc::new(Notify::new()),
        }
    }
}

/// 本代际的 workspace listing 与任务索引缓存（一次枚举，多次绑定共享）。
struct WorkspaceCache {
    generation: u64,
    entries: Vec<WorkspaceEntry>,
    active_key: Option<String>,
    enum_bridge: Option<String>,
    controller_subscribed: bool,
    rows: BTreeMap<(String, String), TaskRow>,
    snapshot_seen: bool,
}

/// 一条 workspace listing 条目（只读解析产物）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkspaceEntry {
    workspace_path: String,
    workspace_identity: Option<String>,
    workspace_key: String,
}

/// 一条任务索引行（`address{workspacePath,taskId,workspaceIdentity?}`）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct TaskRow {
    workspace_path: String,
    task_id: String,
    workspace_identity: Option<String>,
}

/// 一个逻辑桥（= 一次 workspace-bridge-open）。
struct Bridge {
    identity: RpcIdentity,
    assembler: FrameAssembler,
    started: Instant,
    initialized: watch::Sender<bool>,
    /// 与 `pending` 共用一个单调 request id 分配器（事件订阅也占号）。
    next_request_id: u32,
    pending: HashMap<u32, oneshot::Sender<ChannelMessage>>,
    listener: Option<Listener>,
    /// 最近一次入站物理 `seq`（`None` = 尚未绑定，首帧任意）。
    last_seq: Option<u64>,
    last_message_seq: u64,
}

impl Bridge {
    fn new(bridge_id: &str) -> Self {
        let (initialized, _) = watch::channel(false);
        let identity = RpcIdentity::new(bridge_id);
        Self {
            assembler: FrameAssembler::new(identity.clone()),
            identity,
            started: Instant::now(),
            initialized,
            next_request_id: 0,
            pending: HashMap::new(),
            listener: None,
            last_seq: None,
            last_message_seq: 0,
        }
    }
}

/// 事件监听路由（每个桥至多一个监听；id 即 EventListen 请求 id）。
#[derive(Debug, Clone, Copy)]
struct Listener {
    id: u32,
    kind: ListenerKind,
}

/// 监听类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListenerKind {
    Controller,
    Conversation,
}

/// 一个已绑定会话（投影 + ACK 前暂存 + 重同步标记）。
struct SessionEntry {
    target: BoundTarget,
    bridge_id: String,
    projection: Projection,
    staged: Vec<StagedFrame>,
    needs_resync: bool,
    stats: SessionStats,
}

impl SessionEntry {
    fn new(target: BoundTarget, bridge_id: String) -> Self {
        let projection = Projection::new(target.clone());
        Self {
            target,
            bridge_id,
            projection,
            staged: Vec::new(),
            needs_resync: false,
            stats: SessionStats::default(),
        }
    }
}

/// ACK 前暂存的一条 logical frame。
struct StagedFrame {
    subscription_id: String,
    frame: Value,
}

/// 单会话证据计数（暂存放行 / 只读重订阅；值本身不含任何业务数据）。
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SessionStats {
    /// ACK 前暂存、激活后成功放行的帧数。
    pub staged_delivered: u64,
    /// 只读重订阅次数。
    pub resyncs: u64,
}

/// 桥管理器：同一 relay 上按 workspace/session 打开多个逻辑桥。
pub struct BridgeManager {
    shared: Arc<Shared>,
    /// 串行化绑定流程（bootstrap/枚举只做一次；并发 bind 不重复开桥）。
    setup: AsyncMutex<()>,
    /// 串行化只读重订阅（缺口后的 snapshot 刷新）。
    resync: AsyncMutex<()>,
}

/// 管理器与 actor 共享的部分（状态锁 + sender 通道）。
struct Shared {
    state: Mutex<Inner>,
    relay: Arc<dyn RelaySink>,
    timing: BridgeTiming,
    commands: mpsc::UnboundedSender<Command>,
}

impl BridgeManager {
    /// 生产构造（时序取官方口径）。
    pub fn new(relay: Arc<dyn RelaySink>) -> Self {
        Self::with_timing(relay, BridgeTiming::default())
    }

    /// 注入时序（测试专用；生产不注册配置/IPC）。
    pub(crate) fn with_timing(relay: Arc<dyn RelaySink>, timing: BridgeTiming) -> Self {
        let (commands, receiver) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            state: Mutex::new(Inner::default()),
            relay,
            timing,
            commands,
        });
        tokio::spawn(run_actor(shared.clone(), receiver));
        Self {
            shared,
            setup: AsyncMutex::new(()),
            resync: AsyncMutex::new(()),
        }
    }

    /// relay 事件泵入口：一条已配对的 `data` payload。
    ///
    /// 只做解析/路由，**不 await 网络**（ACK 走 actor 队列），因此事件泵不会被慢路径拖住；
    /// 缺口/协议不符只置 `ready=false` 并标记重同步，读面下次访问时取新快照。
    pub async fn on_data(&self, payload: Value) -> ZResult<()> {
        let (result, acks) = {
            let mut inner = lock(&self.shared.state);
            inner.handle_data(&payload)
        };
        for ack in acks {
            let _ = self.shared.commands.send(Command::SendFrame {
                payload: ack,
                reply: None,
            });
        }
        result
    }

    /// 连接代际变化：丢弃旧代际的全部桥/投影/临时绑定。
    pub async fn invalidate(&self, generation: u64) {
        {
            let mut inner = lock(&self.shared.state);
            if generation < inner.generation {
                return;
            }
            inner.generation = generation;
            inner.bridges.clear();
            inner.sessions.clear();
            inner.workspaces = None;
            inner.app_waiters.clear();
        }
        let _ = self.shared.commands.send(Command::Reset);
    }

    /// 精确绑定一个会话（bootstrap → 枚举 → 精确 taskId → 会话桥 → 会话订阅）。
    pub async fn bind(&self, key: &TargetKey) -> ZResult<BoundTarget> {
        let session_id = key.session_id.as_str();
        // 原样匹配：空/首尾空白/任何归一化都不接受。
        if session_id.is_empty() || session_id.trim() != session_id {
            return Err(ZcodeError::of(codes::NO_SESSION));
        }
        let _setup = self.setup.lock().await;
        let generation = lock(&self.shared.state).generation;
        if let Some(target) = {
            let inner = lock(&self.shared.state);
            inner
                .sessions
                .get(session_id)
                .filter(|entry| entry.target.generation == generation)
                .map(|entry| entry.target.clone())
        } {
            return Ok(target);
        }

        let workspaces = self.shared.load_workspaces(generation).await?;
        let rows = self.shared.load_tasks(generation).await?;
        let mut matches = rows.iter().filter(|row| row.task_id == session_id);
        let row = match (matches.next(), matches.next()) {
            (None, _) => return Err(ZcodeError::of(codes::NO_SESSION)),
            (Some(row), None) => row.clone(),
            (Some(_), Some(_)) => return Err(ZcodeError::of(codes::TARGET_AMBIGUOUS)),
        };
        let entry_identity = workspaces
            .iter()
            .find(|entry| entry.workspace_path == row.workspace_path)
            .and_then(|entry| entry.workspace_identity.clone());
        let workspace_identity = row.workspace_identity.clone().or(entry_identity);
        let workspace_key = workspace_identity
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| row.workspace_path.clone());
        let target = BoundTarget {
            session_id: session_id.to_owned(),
            workspace_path: row.workspace_path.clone(),
            workspace_identity,
            workspace_key,
            generation,
        };
        if lock(&self.shared.state).generation != generation {
            return Err(stale());
        }
        self.shared.open_session(&target).await?;
        Ok(target)
    }

    /// 读取绑定会话的投影（必要时先按只读方式重取快照）。
    pub async fn session(&self, target: &BoundTarget) -> ZResult<SessionView> {
        let needs_resync = {
            let inner = lock(&self.shared.state);
            if inner.generation != target.generation {
                return Err(stale());
            }
            let Some(entry) = inner.sessions.get(&target.session_id) else {
                return Err(ZcodeError::of(codes::NO_SESSION));
            };
            if entry.target.generation != target.generation {
                return Err(stale());
            }
            entry.needs_resync
        };
        if needs_resync {
            self.shared.resync(target, &self.resync).await?;
        }
        let inner = lock(&self.shared.state);
        let entry = inner
            .sessions
            .get(&target.session_id)
            .ok_or_else(|| ZcodeError::of(codes::NO_SESSION))?;
        Ok(entry.projection.view())
    }

    /// 在绑定会话的桥上发一次 RPC（读调用；T3 起才用于写命令）。
    ///
    /// `ready=false` 时**零发送**（缺口/未取到快照期间控制面零写）。
    pub async fn call(
        &self,
        target: &BoundTarget,
        channel: &str,
        method: &str,
        args: Vec<Value>,
    ) -> ZResult<Value> {
        let (bridge_id, needs_resync) = {
            let inner = lock(&self.shared.state);
            if inner.generation != target.generation {
                return Err(stale());
            }
            let Some(entry) = inner.sessions.get(&target.session_id) else {
                return Err(ZcodeError::of(codes::NO_SESSION));
            };
            (entry.bridge_id.clone(), entry.needs_resync)
        };
        if needs_resync {
            self.shared.resync(target, &self.resync).await?;
        }
        {
            let inner = lock(&self.shared.state);
            let ready = inner
                .sessions
                .get(&target.session_id)
                .map(|entry| entry.projection.is_ready())
                .unwrap_or(false);
            if !ready {
                return Err(ZcodeError::of(codes::NOT_READY));
            }
        }
        let message = self
            .shared
            .rpc_call(&bridge_id, channel, method, args, self.shared.timing.rpc)
            .await?;
        Ok(message.body)
    }

    /// 测试观测：单会话的暂存/重同步计数（证据只落数值）。
    #[cfg(test)]
    pub(crate) fn session_stats(&self, session_id: &str) -> Option<SessionStats> {
        lock(&self.shared.state)
            .sessions
            .get(session_id)
            .map(|entry| entry.stats)
    }

    /// 测试观测：不触发重同步的只读视图快照。
    #[cfg(test)]
    pub(crate) fn peek_view(&self, session_id: &str) -> Option<SessionView> {
        lock(&self.shared.state)
            .sessions
            .get(session_id)
            .map(|entry| entry.projection.view())
    }
}

impl Shared {
    /// 只读 app payload 请求（bootstrap/workspace-list/workspace-bridge-open）。
    async fn app_request(
        &self,
        request_type: &str,
        extra: Option<Map<String, Value>>,
        generation: u64,
    ) -> ZResult<Value> {
        let (request_id, receiver) = {
            let mut inner = lock(&self.state);
            inner.next_app_request = inner.next_app_request.wrapping_add(1);
            let request_id = format!("tuvis-{request_type}-{:08x}", inner.next_app_request);
            let (sender, receiver) = oneshot::channel();
            inner.app_waiters.insert(request_id.clone(), sender);
            (request_id, receiver)
        };
        let mut payload = extra.unwrap_or_default();
        payload.insert("zcode_type".to_owned(), Value::from(request_type));
        payload.insert("requestId".to_owned(), Value::from(request_id.clone()));
        let (reply_sender, reply_receiver) = oneshot::channel();
        if self
            .commands
            .send(Command::SendFrame {
                payload: Value::Object(payload),
                reply: Some(reply_sender),
            })
            .is_err()
        {
            lock(&self.state).app_waiters.remove(&request_id);
            return Err(ZcodeError::offline());
        }
        let wait = async move {
            match reply_receiver.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => return Err(error),
                Err(_) => return Err(ZcodeError::offline()),
            }
            receiver.await.map_err(|_| ZcodeError::offline())
        };
        let result = match tokio::time::timeout(self.timing.app_request, wait).await {
            Ok(Ok(payload)) => {
                if lock(&self.state).generation != generation {
                    Err(stale())
                } else {
                    Ok(payload)
                }
            }
            Ok(Err(error)) => Err(error),
            Err(_) => Err(ZcodeError::of(codes::NOT_READY)),
        };
        if result.is_err() {
            lock(&self.state).app_waiters.remove(&request_id);
        }
        result
    }

    /// 双 listing（bootstrap + workspace-list）去重后的 workspace 条目。
    async fn load_workspaces(&self, generation: u64) -> ZResult<Vec<WorkspaceEntry>> {
        {
            let inner = lock(&self.state);
            if let Some(cache) = &inner.workspaces {
                if cache.generation == generation && !cache.entries.is_empty() {
                    return Ok(cache.entries.clone());
                }
            }
        }
        let bootstrap = self
            .app_request(BOOTSTRAP_REQUEST, None, generation)
            .await?;
        let listing = self
            .app_request(WORKSPACE_LIST_REQUEST, None, generation)
            .await?;
        let mut entries: Vec<WorkspaceEntry> = Vec::new();
        let mut active_key: Option<String> = None;
        for payload in [&bootstrap, &listing] {
            let Some(result) = payload.get("result") else {
                continue;
            };
            if active_key.is_none() {
                active_key = result
                    .get("mobileViewState")
                    .and_then(|view| view.get("activeWorkspaceKey"))
                    .and_then(Value::as_str)
                    .or_else(|| result.get("activeWorkspaceKey").and_then(Value::as_str))
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned);
            }
            let Some(items) = result.get("workspaces").and_then(Value::as_array) else {
                continue;
            };
            for item in items {
                let Some(entry) = parse_workspace_entry(item) else {
                    continue;
                };
                // 同一 (path, identity) 只保留一份（实连 bootstrap/workspace-list 同源）。
                let duplicate = entries.iter().any(|known| {
                    known.workspace_path == entry.workspace_path
                        && known.workspace_identity == entry.workspace_identity
                });
                if !duplicate {
                    entries.push(entry);
                }
            }
        }
        if entries.is_empty() {
            return Err(ZcodeError::of(codes::NO_SESSION));
        }
        {
            let mut inner = lock(&self.state);
            if inner.generation != generation {
                return Err(stale());
            }
            inner.workspaces = Some(WorkspaceCache {
                generation,
                entries: entries.clone(),
                active_key,
                enum_bridge: None,
                controller_subscribed: false,
                rows: BTreeMap::new(),
                snapshot_seen: false,
            });
        }
        Ok(entries)
    }

    /// 主动任务索引（WindowController 订阅）枚举；一次枚举多会话绑定共享。
    async fn load_tasks(&self, generation: u64) -> ZResult<Vec<TaskRow>> {
        let (existing_bridge, workspace_key) = {
            let inner = lock(&self.state);
            let Some(cache) = &inner.workspaces else {
                return Err(ZcodeError::of(codes::NOT_READY));
            };
            if cache.generation != generation {
                return Err(stale());
            }
            if cache.snapshot_seen {
                return Ok(cache.rows.values().cloned().collect());
            }
            let active = cache.active_key.as_ref();
            let workspace = cache
                .entries
                .iter()
                .find(|entry| active == Some(&entry.workspace_key))
                .or_else(|| {
                    active.and_then(|key| {
                        cache
                            .entries
                            .iter()
                            .find(|entry| &entry.workspace_path == key)
                    })
                })
                .unwrap_or(&cache.entries[0]);
            (cache.enum_bridge.clone(), workspace.workspace_key.clone())
        };

        let bridge_id = match existing_bridge {
            Some(bridge_id) => bridge_id,
            None => {
                let bridge_id = self
                    .open_bridge(generation, &workspace_key, None, false)
                    .await?;
                let mut inner = lock(&self.state);
                if let Some(cache) = inner.workspaces.as_mut() {
                    cache.enum_bridge = Some(bridge_id.clone());
                }
                bridge_id
            }
        };

        let subscribed = lock(&self.state)
            .workspaces
            .as_ref()
            .map(|cache| cache.controller_subscribed)
            .unwrap_or(false);
        if !subscribed {
            let listener_id = self.register_listener(&bridge_id, ListenerKind::Controller)?;
            self.send_listen(
                &bridge_id,
                listener_id,
                WINDOW_CONTROLLER_CHANNEL,
                CONTROLLER_DYNAMIC_FRAME_EVENT,
                Value::Null,
            )
            .await?;
            let params = Value::Object(
                [
                    (
                        "topic".to_owned(),
                        Value::from(CONTROLLER_TASKS_INDEX_TOPIC),
                    ),
                    ("visibility".to_owned(), Value::from("foreground")),
                ]
                .into_iter()
                .collect(),
            );
            let message = self
                .rpc_call(
                    &bridge_id,
                    WINDOW_CONTROLLER_CHANNEL,
                    CONTROLLER_SUBSCRIBE_METHOD,
                    vec![params],
                    self.timing.rpc,
                )
                .await?;
            message
                .body
                .get("ack")
                .and_then(|ack| ack.get("subscriptionId"))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| ZcodeError::of(codes::NOT_READY))?;
            let mut inner = lock(&self.state);
            if let Some(cache) = inner.workspaces.as_mut() {
                cache.controller_subscribed = true;
            }
        }

        // 有界收帧窗：等首个 controller 帧（snapshot 或 deltas）。
        let deadline = Instant::now() + self.timing.task_window;
        loop {
            if lock(&self.state)
                .workspaces
                .as_ref()
                .map(|cache| cache.snapshot_seen)
                .unwrap_or(false)
            {
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let notify = lock(&self.state).enum_notify.clone();
            let notified = notify.notified();
            let _ = tokio::time::timeout(deadline.saturating_duration_since(now), notified).await;
        }
        let inner = lock(&self.state);
        Ok(inner
            .workspaces
            .as_ref()
            .map(|cache| cache.rows.values().cloned().collect())
            .unwrap_or_default())
    }

    /// 打开一个逻辑桥（ready 精确回显 → Initialize 门 → 会话桥补 hello/clientHello）。
    async fn open_bridge(
        &self,
        generation: u64,
        workspace_key: &str,
        task_id: Option<&str>,
        session: bool,
    ) -> ZResult<String> {
        let bridge_id = {
            let mut inner = lock(&self.state);
            if inner.generation != generation {
                return Err(stale());
            }
            inner.next_bridge_id = inner.next_bridge_id.wrapping_add(1);
            let bridge_id = format!("tuvis-bridge-{:08x}", inner.next_bridge_id);
            inner
                .bridges
                .insert(bridge_id.clone(), Bridge::new(&bridge_id));
            bridge_id
        };
        match self
            .open_bridge_inner(generation, &bridge_id, workspace_key, task_id, session)
            .await
        {
            Ok(()) => Ok(bridge_id),
            Err(error) => {
                self.drop_bridge(&bridge_id);
                Err(error)
            }
        }
    }

    async fn open_bridge_inner(
        &self,
        generation: u64,
        bridge_id: &str,
        workspace_key: &str,
        task_id: Option<&str>,
        session: bool,
    ) -> ZResult<()> {
        let mut extra = Map::new();
        extra.insert("bridgeSessionId".to_owned(), Value::from(bridge_id));
        extra.insert("workspaceKey".to_owned(), Value::from(workspace_key));
        if let Some(task_id) = task_id {
            extra.insert("taskId".to_owned(), Value::from(task_id));
        }
        let response = self
            .app_request(WORKSPACE_BRIDGE_OPEN, Some(extra), generation)
            .await?;
        let object = response.as_object().ok_or_else(unsupported)?;
        match object.get("zcode_type").and_then(Value::as_str) {
            Some(WORKSPACE_BRIDGE_READY) => {}
            Some(WORKSPACE_BRIDGE_ERROR) => return Err(ZcodeError::of(codes::NOT_READY)),
            // 未知/残缺 ready：fail-closed，绝不带着可疑身份进入后续流程。
            _ => return Err(unsupported()),
        }
        let nested = object.get("bridge").and_then(Value::as_object);
        let echoed = nested
            .and_then(|bridge| bridge.get("bridgeSessionId"))
            .and_then(Value::as_str)
            .or_else(|| object.get("bridgeSessionId").and_then(Value::as_str));
        if echoed != Some(bridge_id) {
            return Err(unsupported());
        }
        // 请求未声明 bridgeGeneration/recoveryId → ready 回显同样必须缺席
        // （APP 接收侧对身份做严格相等，多写字段的桥不可信）。
        let nested_generation = nested.and_then(|bridge| bridge.get("bridgeGeneration"));
        if object.get("bridgeGeneration").is_some()
            || object.get("recoveryId").is_some()
            || nested_generation.is_some()
        {
            return Err(unsupported());
        }

        // Initialize 门：官方 ChannelClient 在 200 之前不发任何请求。
        let mut initialized = {
            let inner = lock(&self.state);
            inner
                .bridges
                .get(bridge_id)
                .map(|bridge| bridge.initialized.subscribe())
                .ok_or_else(stale)?
        };
        if !*initialized.borrow_and_update() {
            let _ =
                tokio::time::timeout(self.timing.initialize, initialized.wait_for(|value| *value))
                    .await;
        }
        let ready = lock(&self.state)
            .bridges
            .get(bridge_id)
            .map(|bridge| *bridge.initialized.borrow())
            .unwrap_or(false);
        if !ready {
            return Err(ZcodeError::of(codes::NOT_READY));
        }

        if session {
            let hello = self
                .rpc_call(
                    bridge_id,
                    AGENT_CHANNEL,
                    AGENT_HELLO_METHOD,
                    Vec::new(),
                    self.timing.rpc,
                )
                .await?;
            let body = hello.body.as_object().ok_or_else(unsupported)?;
            if body.get("kind").and_then(Value::as_str) != Some("hello") {
                return Err(unsupported());
            }
            if body.get("protocolVersion").and_then(Value::as_u64) != Some(V4_WIRE_PROTOCOL_VERSION)
            {
                return Err(unsupported());
            }
            if body
                .get("connectionId")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .is_none()
            {
                return Err(unsupported());
            }
            let client_hello = Value::Object(
                [
                    ("kind".to_owned(), Value::from("clientHello")),
                    (
                        "protocolVersion".to_owned(),
                        Value::from(V4_WIRE_PROTOCOL_VERSION),
                    ),
                    ("clientId".to_owned(), Value::from(CLIENT_ID)),
                    ("clientKind".to_owned(), Value::from(CLIENT_KIND)),
                    ("appVersion".to_owned(), Value::from(APP_VERSION)),
                ]
                .into_iter()
                .collect(),
            );
            self.rpc_call(
                bridge_id,
                AGENT_CHANNEL,
                AGENT_CLIENT_HELLO_METHOD,
                vec![client_hello],
                self.timing.rpc,
            )
            .await?;
        }
        Ok(())
    }

    /// 建立会话桥 + 会话 topic 订阅，并完成 ACK 后本地激活。
    async fn open_session(&self, target: &BoundTarget) -> ZResult<()> {
        let bridge_id = self
            .open_bridge(
                target.generation,
                &target.workspace_key,
                Some(&target.session_id),
                true,
            )
            .await?;
        let inserted = {
            let mut inner = lock(&self.state);
            if inner.generation == target.generation {
                inner.sessions.insert(
                    target.session_id.clone(),
                    SessionEntry::new(target.clone(), bridge_id.clone()),
                );
                true
            } else {
                false
            }
        };
        if !inserted {
            self.drop_bridge(&bridge_id);
            return Err(stale());
        }
        let subscription = match self.subscribe_conversation(target, &bridge_id).await {
            Ok(subscription) => subscription,
            Err(error) => {
                self.drop_bridge(&bridge_id);
                return Err(error);
            }
        };
        let mut inner = lock(&self.state);
        if let Some(entry) = inner.sessions.get_mut(&target.session_id) {
            activate_and_flush(entry, &subscription);
        }
        Ok(())
    }

    /// EventListen(onDynamicConversationFrame, workspace) → subscribeConversationV4 → ACK。
    async fn subscribe_conversation(
        &self,
        target: &BoundTarget,
        bridge_id: &str,
    ) -> ZResult<String> {
        let listener_id = self.register_listener(bridge_id, ListenerKind::Conversation)?;
        let mut workspace = Map::new();
        workspace.insert(
            "workspacePath".to_owned(),
            Value::from(target.workspace_path.clone()),
        );
        if let Some(identity) = &target.workspace_identity {
            workspace.insert(
                "workspaceIdentity".to_owned(),
                Value::from(identity.clone()),
            );
        }
        self.send_listen(
            bridge_id,
            listener_id,
            AGENT_CHANNEL,
            AGENT_CONVERSATION_FRAME_EVENT,
            Value::Object(workspace.clone()),
        )
        .await?;
        workspace.insert(
            "sessionId".to_owned(),
            Value::from(target.session_id.clone()),
        );
        let message = self
            .rpc_call(
                bridge_id,
                AGENT_CHANNEL,
                AGENT_SUBSCRIBE_CONVERSATION_METHOD,
                vec![Value::Object(workspace)],
                self.timing.rpc,
            )
            .await?;
        message
            .body
            .get("ack")
            .and_then(|ack| ack.get("subscriptionId"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| ZcodeError::of(codes::NOT_READY))
    }

    /// 缺口/协议不符后的只读重订阅（取新快照；不发送任何写命令）。
    async fn resync(&self, target: &BoundTarget, gate: &AsyncMutex<()>) -> ZResult<()> {
        let _guard = gate.lock().await;
        let (bridge_id, needs_resync) = {
            let inner = lock(&self.state);
            if inner.generation != target.generation {
                return Err(stale());
            }
            let Some(entry) = inner.sessions.get(&target.session_id) else {
                return Err(ZcodeError::of(codes::NO_SESSION));
            };
            (entry.bridge_id.clone(), entry.needs_resync)
        };
        if !needs_resync {
            return Ok(());
        }
        {
            let mut inner = lock(&self.state);
            if let Some(entry) = inner.sessions.get_mut(&target.session_id) {
                // 先解除旧订阅 barrier：新订阅的帧（新 subscriptionId）在 ACK 前暂存。
                entry.projection.deactivate();
                entry.staged.clear();
                entry.needs_resync = false;
                entry.stats.resyncs = entry.stats.resyncs.saturating_add(1);
            }
        }
        match self.subscribe_conversation(target, &bridge_id).await {
            Ok(subscription) => {
                let mut inner = lock(&self.state);
                if let Some(entry) = inner.sessions.get_mut(&target.session_id) {
                    activate_and_flush(entry, &subscription);
                }
                Ok(())
            }
            Err(error) => {
                let mut inner = lock(&self.state);
                if let Some(entry) = inner.sessions.get_mut(&target.session_id) {
                    entry.needs_resync = true;
                }
                Err(error)
            }
        }
    }

    fn register_listener(&self, bridge_id: &str, kind: ListenerKind) -> ZResult<u32> {
        let mut inner = lock(&self.state);
        let bridge = inner.bridges.get_mut(bridge_id).ok_or_else(stale)?;
        bridge.next_request_id = bridge.next_request_id.wrapping_add(1).max(1);
        let id = bridge.next_request_id;
        bridge.listener = Some(Listener { id, kind });
        Ok(id)
    }

    fn register_rpc(&self, bridge_id: &str) -> ZResult<(u32, oneshot::Receiver<ChannelMessage>)> {
        let mut inner = lock(&self.state);
        let bridge = inner.bridges.get_mut(bridge_id).ok_or_else(stale)?;
        bridge.next_request_id = bridge.next_request_id.wrapping_add(1).max(1);
        let id = bridge.next_request_id;
        let (sender, receiver) = oneshot::channel();
        bridge.pending.insert(id, sender);
        Ok((id, receiver))
    }

    fn unregister_rpc(&self, bridge_id: &str, id: u32) {
        let mut inner = lock(&self.state);
        if let Some(bridge) = inner.bridges.get_mut(bridge_id) {
            bridge.pending.remove(&id);
        }
    }

    async fn rpc_call(
        &self,
        bridge_id: &str,
        channel: &str,
        method: &str,
        args: Vec<Value>,
        timeout: Duration,
    ) -> ZResult<ChannelMessage> {
        let (id, receiver) = self.register_rpc(bridge_id)?;
        let (reply_sender, reply_receiver) = oneshot::channel();
        if self
            .commands
            .send(Command::SendRpc {
                bridge_id: bridge_id.to_owned(),
                id,
                channel: channel.to_owned(),
                method: method.to_owned(),
                args,
                reply: reply_sender,
            })
            .is_err()
        {
            self.unregister_rpc(bridge_id, id);
            return Err(ZcodeError::offline());
        }
        let wait = async move {
            match reply_receiver.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => return Err(error),
                Err(_) => return Err(ZcodeError::offline()),
            }
            receiver.await.map_err(|_| ZcodeError::offline())
        };
        match tokio::time::timeout(timeout, wait).await {
            Ok(Ok(message)) => {
                if message.message_type == RESPONSE_TYPE_PROMISE_ERROR {
                    // 上游原文（可能含路径/正文）绝不下发，只映射冻结码。
                    Err(ZcodeError::unknown())
                } else {
                    Ok(message)
                }
            }
            Ok(Err(error)) => {
                self.unregister_rpc(bridge_id, id);
                Err(error)
            }
            Err(_) => {
                self.unregister_rpc(bridge_id, id);
                Err(ZcodeError::of(codes::NOT_READY))
            }
        }
    }

    async fn send_listen(
        &self,
        bridge_id: &str,
        id: u32,
        channel: &str,
        event: &str,
        arg: Value,
    ) -> ZResult<()> {
        let (reply_sender, reply_receiver) = oneshot::channel();
        if self
            .commands
            .send(Command::SendListen {
                bridge_id: bridge_id.to_owned(),
                id,
                channel: channel.to_owned(),
                event: event.to_owned(),
                arg,
                reply: reply_sender,
            })
            .is_err()
        {
            return Err(ZcodeError::offline());
        }
        match tokio::time::timeout(self.timing.rpc, reply_receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(ZcodeError::offline()),
            Err(_) => Err(ZcodeError::of(codes::NOT_READY)),
        }
    }

    fn drop_bridge(&self, bridge_id: &str) {
        let mut inner = lock(&self.state);
        inner.bridges.remove(bridge_id);
        inner
            .sessions
            .retain(|_, entry| entry.bridge_id != bridge_id);
        if let Some(cache) = inner.workspaces.as_mut() {
            if cache.enum_bridge.as_deref() == Some(bridge_id) {
                cache.enum_bridge = None;
                cache.controller_subscribed = false;
            }
        }
    }
}

impl Inner {
    /// 入站路由：只做纯内存工作，返回值 = 本次需要发出的 rpc-frame-ack。
    fn handle_data(&mut self, payload: &Value) -> (ZResult<()>, Vec<Value>) {
        let mut acks = Vec::new();
        let Some(object) = payload.as_object() else {
            return (Ok(()), acks);
        };
        match object.get("zcode_type").and_then(Value::as_str) {
            Some("rpc-frame-ack") => (Ok(()), acks),
            Some("rpc-frame") => {
                let result = self.handle_frame(object, &mut acks);
                (result, acks)
            }
            _ => {
                if let Some(request_id) = object.get("requestId").and_then(Value::as_str) {
                    if let Some(waiter) = self.app_waiters.remove(request_id) {
                        let _ = waiter.send(payload.clone());
                    }
                }
                (Ok(()), acks)
            }
        }
    }

    fn handle_frame(&mut self, object: &Map<String, Value>, acks: &mut Vec<Value>) -> ZResult<()> {
        let Some(bridge_id) = object
            .get("bridgeSessionId")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            return Ok(());
        };
        if !self.bridges.contains_key(&bridge_id) {
            // 旧桥/异桥帧：静默丢弃（身份核实由 FrameAssembler 再做一次）。
            return Ok(());
        }
        let (seq, message_seq) = match (
            object.get("seq").and_then(Value::as_u64),
            object.get("messageSeq").and_then(Value::as_u64),
        ) {
            (Some(seq), Some(message_seq)) if seq > 0 && message_seq > 0 => (seq, message_seq),
            _ => {
                // 缺 seq/messageSeq = 物理层不可重同步，置不新鲜。
                self.mark_bridge_unready(&bridge_id);
                return Err(unsupported());
            }
        };
        let mut gap = false;
        {
            let Some(bridge) = self.bridges.get(&bridge_id) else {
                return Ok(());
            };
            if let Some(last) = bridge.last_seq {
                if seq <= last {
                    // 重复帧：只确认，不重复交付。
                    return Ok(());
                }
                if seq > last.saturating_add(1) {
                    gap = true;
                }
            }
            if message_seq <= bridge.last_message_seq {
                return Ok(());
            }
        }
        if gap {
            // 物理缺口：本帧及之前丢失的消息都不可信 → 丢弃并重订阅取新快照。
            self.mark_bridge_unready(&bridge_id);
            return Ok(());
        }

        let assembled = {
            let Some(bridge) = self.bridges.get_mut(&bridge_id) else {
                return Ok(());
            };
            let now_ms = u64::try_from(bridge.started.elapsed().as_millis()).unwrap_or(u64::MAX);
            bridge.last_seq = Some(seq);
            bridge.last_message_seq = message_seq;
            bridge
                .assembler
                .accept(Value::Object(object.clone()), now_ms)
        };
        let delivered = match assembled {
            Ok(delivered) => delivered,
            Err(error) => {
                // 坏帧/超限/TTL：整桥置不新鲜，要求重同步（不吞错）。
                self.mark_bridge_unready(&bridge_id);
                return Err(error);
            }
        };
        if !delivered.is_empty() {
            if let Some(bridge) = self.bridges.get(&bridge_id) {
                acks.push(encode_ack(&bridge.identity, message_seq));
            }
        }

        let mut events: Vec<(ListenerKind, Value)> = Vec::new();
        for bytes in delivered {
            let Ok(message) = decode_channel(&bytes) else {
                self.mark_bridge_unready(&bridge_id);
                return Err(unsupported());
            };
            match message.message_type {
                RESPONSE_TYPE_INITIALIZE => {
                    if let Some(bridge) = self.bridges.get(&bridge_id) {
                        bridge.initialized.send_replace(true);
                    }
                }
                RESPONSE_TYPE_PROMISE_SUCCESS | RESPONSE_TYPE_PROMISE_ERROR => {
                    if let Some(id) = message.id {
                        if let Some(bridge) = self.bridges.get_mut(&bridge_id) {
                            if let Some(waiter) = bridge.pending.remove(&id) {
                                let _ = waiter.send(message);
                            }
                        }
                    }
                }
                RESPONSE_TYPE_EVENT_FIRE => {
                    if let Some(id) = message.id {
                        if let Some(listener) = self
                            .bridges
                            .get(&bridge_id)
                            .and_then(|bridge| bridge.listener)
                        {
                            if listener.id == id {
                                events.push((listener.kind, message.body));
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        for (kind, body) in events {
            match kind {
                ListenerKind::Controller => self.handle_controller_frame(&body),
                ListenerKind::Conversation => {
                    let session_id = self
                        .sessions
                        .iter()
                        .find(|(_, entry)| entry.bridge_id == bridge_id)
                        .map(|(session_id, _)| session_id.clone());
                    if let Some(session_id) = session_id {
                        self.handle_conversation_frame(&session_id, body);
                    }
                }
            }
        }
        Ok(())
    }

    /// controller 任务索引帧（snapshot 整体替换 / deltas upsert+removed）。
    fn handle_controller_frame(&mut self, body: &Value) {
        let Some(object) = body.as_object() else {
            return;
        };
        if object.get("topic").and_then(Value::as_str) != Some(CONTROLLER_TASKS_INDEX_TOPIC) {
            return;
        }
        let Some(payload) = object.get("payload").and_then(Value::as_object) else {
            return;
        };
        let mut changed = false;
        {
            let Some(cache) = self.workspaces.as_mut() else {
                return;
            };
            match payload.get("kind").and_then(Value::as_str) {
                Some("snapshot") => {
                    let Some(tasks) = payload
                        .get("snapshot")
                        .and_then(|snapshot| snapshot.get("tasks"))
                        .and_then(Value::as_array)
                    else {
                        return;
                    };
                    cache.rows.clear();
                    for task in tasks {
                        if let Some(row) = parse_task_row(task) {
                            cache
                                .rows
                                .insert((row.workspace_path.clone(), row.task_id.clone()), row);
                        }
                    }
                    cache.snapshot_seen = true;
                    changed = true;
                }
                Some("deltas") => {
                    let Some(deltas) = payload.get("deltas").and_then(Value::as_array) else {
                        return;
                    };
                    for delta in deltas {
                        let Some(record) = delta.as_object() else {
                            continue;
                        };
                        match record.get("op").and_then(Value::as_str) {
                            Some("task.upserted") => {
                                if let Some(row) = record.get("task").and_then(parse_task_row) {
                                    cache.rows.insert(
                                        (row.workspace_path.clone(), row.task_id.clone()),
                                        row,
                                    );
                                    changed = true;
                                }
                            }
                            Some("task.removed") => {
                                let address = record.get("address").and_then(Value::as_object);
                                let path = address
                                    .and_then(|address| address.get("workspacePath"))
                                    .and_then(Value::as_str);
                                let task_id = address
                                    .and_then(|address| address.get("taskId"))
                                    .and_then(Value::as_str);
                                if let (Some(path), Some(task_id)) = (path, task_id) {
                                    cache.rows.remove(&(path.to_owned(), task_id.to_owned()));
                                    changed = true;
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        if changed {
            self.enum_notify.notify_waiters();
        }
    }

    /// conversation 事件帧：session/topic/subscription 三层过滤 + ACK 前暂存。
    fn handle_conversation_frame(&mut self, session_id: &str, body: Value) {
        let Some(entry) = self.sessions.get_mut(session_id) else {
            return;
        };
        let Some(object) = body.as_object() else {
            mark_session_unready(entry);
            return;
        };
        if object.get("wireVersion").and_then(Value::as_u64) != Some(V4_WIRE_PROTOCOL_VERSION) {
            mark_session_unready(entry);
            return;
        }
        let expected_topic = format!("{CONVERSATION_TOPIC_PREFIX}{session_id}");
        if object.get("topic").and_then(Value::as_str) != Some(expected_topic.as_str()) {
            // workspace 级事件会广播兄弟会话帧：异会话零应用。
            return;
        }
        let Some(subscription) = object
            .get("subscriptionId")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            mark_session_unready(entry);
            return;
        };
        match object.get("kind").and_then(Value::as_str) {
            Some("complete") => {
                let Some(frame) = object.get("frame").cloned() else {
                    mark_session_unready(entry);
                    return;
                };
                let Some(frame_object) = frame.as_object() else {
                    mark_session_unready(entry);
                    return;
                };
                if frame_object.get("topic").and_then(Value::as_str)
                    != Some(expected_topic.as_str())
                    || frame_object.get("subscriptionId").and_then(Value::as_str)
                        != Some(subscription.as_str())
                {
                    mark_session_unready(entry);
                    return;
                }
                match entry.projection.subscription_id().map(str::to_owned) {
                    None => {
                        if entry.staged.len() >= MAX_STAGED_FRAMES {
                            mark_session_unready(entry);
                            return;
                        }
                        entry.staged.push(StagedFrame {
                            subscription_id: subscription,
                            frame,
                        });
                    }
                    Some(bound) if bound == subscription => {
                        apply_frame(entry, frame);
                    }
                    // 旧 subscriptionId（换代/重订阅后迟到）：零应用。
                    Some(_) => {}
                }
            }
            // 未支持的 wire 形态（fragment 等）：fail-closed，取新快照。
            _ => mark_session_unready(entry),
        }
    }

    /// 物理缺口/坏帧：整桥水位清空 + 所属会话置不新鲜（下次读触发重订阅）。
    fn mark_bridge_unready(&mut self, bridge_id: &str) {
        if let Some(bridge) = self.bridges.get_mut(bridge_id) {
            bridge.assembler.reset();
            bridge.last_seq = None;
            bridge.last_message_seq = 0;
        }
        for entry in self.sessions.values_mut() {
            if entry.bridge_id == bridge_id {
                mark_session_unready(entry);
            }
        }
    }
}

/// ACK 到达后的本地 barrier 放行：只交付匹配 subscriptionId 的暂存帧。
fn activate_and_flush(entry: &mut SessionEntry, subscription: &str) {
    entry.projection.activate(subscription);
    let staged = std::mem::take(&mut entry.staged);
    for item in staged {
        if item.subscription_id != subscription {
            continue;
        }
        if apply_frame(entry, item.frame) {
            entry.stats.staged_delivered = entry.stats.staged_delivered.saturating_add(1);
        }
    }
}

/// 应用一帧；缺口/未知 schema 时标记重同步（stale/异会话只丢弃）。
fn apply_frame(entry: &mut SessionEntry, frame: Value) -> bool {
    match entry.projection.apply(entry.target.generation, frame) {
        Ok(()) => true,
        Err(error) => {
            if error.is(codes::NOT_READY) || error.is(codes::UNSUPPORTED_PROTOCOL) {
                mark_session_unready(entry);
            }
            false
        }
    }
}

fn mark_session_unready(entry: &mut SessionEntry) {
    entry.needs_resync = true;
    entry.projection.set_ready(false);
}

fn parse_workspace_entry(value: &Value) -> Option<WorkspaceEntry> {
    let object = value.as_object()?;
    let workspace_path = object
        .get("workspacePath")?
        .as_str()
        .filter(|value| !value.is_empty())?;
    let workspace_identity = object
        .get("workspaceIdentity")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let workspace_key = workspace_identity
        .clone()
        .unwrap_or_else(|| workspace_path.to_owned());
    Some(WorkspaceEntry {
        workspace_path: workspace_path.to_owned(),
        workspace_identity,
        workspace_key,
    })
}

fn parse_task_row(value: &Value) -> Option<TaskRow> {
    let object = value.as_object()?;
    let address = object.get("address")?.as_object()?;
    let workspace_path = address
        .get("workspacePath")?
        .as_str()
        .filter(|value| !value.is_empty())?;
    let task_id = address
        .get("taskId")?
        .as_str()
        .filter(|value| !value.is_empty())?;
    Some(TaskRow {
        workspace_path: workspace_path.to_owned(),
        task_id: task_id.to_owned(),
        workspace_identity: address
            .get("workspaceIdentity")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

/// 出站 actor：全部 seq/messageSeq 分配与 socket 写入的唯一串行点。
async fn run_actor(shared: Arc<Shared>, mut commands: mpsc::UnboundedReceiver<Command>) {
    let mut counters: HashMap<String, SeqCounters> = HashMap::new();
    while let Some(command) = commands.recv().await {
        match command {
            Command::SendRpc {
                bridge_id,
                id,
                channel,
                method,
                args,
                reply,
            } => {
                let result = send_rpc_frame(
                    &shared,
                    &mut counters,
                    &bridge_id,
                    id,
                    &channel,
                    &method,
                    args,
                )
                .await;
                let _ = reply.send(result);
            }
            Command::SendListen {
                bridge_id,
                id,
                channel,
                event,
                arg,
                reply,
            } => {
                let result = send_listen_frame(
                    &shared,
                    &mut counters,
                    &bridge_id,
                    id,
                    &channel,
                    &event,
                    arg,
                )
                .await;
                let _ = reply.send(result);
            }
            Command::SendFrame { payload, reply } => {
                let result = shared.relay.send_data(payload).await;
                if let Some(reply) = reply {
                    let _ = reply.send(result);
                }
            }
            Command::Reset => counters.clear(),
        }
    }
}

async fn send_rpc_frame(
    shared: &Shared,
    counters: &mut HashMap<String, SeqCounters>,
    bridge_id: &str,
    id: u32,
    channel: &str,
    method: &str,
    args: Vec<Value>,
) -> ZResult<()> {
    let identity = {
        let inner = lock(&shared.state);
        inner
            .bridges
            .get(bridge_id)
            .map(|bridge| bridge.identity.clone())
    }
    .ok_or_else(stale)?;
    let bytes = encode_call(id, channel, method, args)?;
    let counter = counters.entry(bridge_id.to_owned()).or_default();
    counter.seq = counter.seq.saturating_add(1);
    counter.message_seq = counter.message_seq.saturating_add(1);
    let frame = encode_frame(&identity, counter.seq, counter.message_seq, &bytes)?;
    shared.relay.send_data(frame).await
}

async fn send_listen_frame(
    shared: &Shared,
    counters: &mut HashMap<String, SeqCounters>,
    bridge_id: &str,
    id: u32,
    channel: &str,
    event: &str,
    arg: Value,
) -> ZResult<()> {
    let identity = {
        let inner = lock(&shared.state);
        inner
            .bridges
            .get(bridge_id)
            .map(|bridge| bridge.identity.clone())
    }
    .ok_or_else(stale)?;
    let bytes = encode_listen(id, channel, event, arg)?;
    let counter = counters.entry(bridge_id.to_owned()).or_default();
    counter.seq = counter.seq.saturating_add(1);
    counter.message_seq = counter.message_seq.saturating_add(1);
    let frame = encode_frame(&identity, counter.seq, counter.message_seq, &bytes)?;
    shared.relay.send_data(frame).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex as StdMutex;
    use std::sync::Weak;

    use base64::Engine as _;
    use futures::future::BoxFuture;
    use serde_json::json;

    use crate::services::zcode_control::codec::{
        encode_frame, RpcIdentity, RESPONSE_TYPE_EVENT_FIRE, RESPONSE_TYPE_PROMISE_SUCCESS,
    };
    use crate::services::zcode_control::projection::CONVERSATION_TOPIC_PREFIX;

    const SESSION_A: &str = "sess_0f9a1c0d-0000-4000-8000-000000000001";
    const SESSION_B: &str = "sess_0f9a1c0d-0000-4000-8000-000000000002";
    const SESSION_HEX: &str = "session_0123456789abcdef0123456789abcdef";
    const WS_A: &str = "/tmp/synthetic-workspace-a";
    const WS_B: &str = "/tmp/synthetic-workspace-b";

    // -----------------------------------------------------------------------
    // 最小 wire 序列化 / 反序列化（只供 fake 服务端构造入站 Channel 消息）
    // -----------------------------------------------------------------------

    fn write_vql(out: &mut Vec<u8>, value: u32) {
        if value == 0 {
            out.push(0);
            return;
        }
        let mut v = value;
        loop {
            let mut byte = (v & 0x7f) as u8;
            v >>= 7;
            if v > 0 {
                byte |= 0x80;
            }
            out.push(byte);
            if v == 0 {
                return;
            }
        }
    }

    fn write_value(out: &mut Vec<u8>, value: &Value) {
        match value {
            Value::Number(number) => {
                let raw = number.as_u64().expect("test wire integers are unsigned");
                out.push(6);
                write_vql(out, u32::try_from(raw).expect("fits u32"));
            }
            Value::String(text) => {
                out.push(1);
                write_vql(out, u32::try_from(text.len()).expect("fits u32"));
                out.extend_from_slice(text.as_bytes());
            }
            Value::Array(items) => {
                out.push(4);
                write_vql(out, u32::try_from(items.len()).expect("fits u32"));
                for item in items {
                    write_value(out, item);
                }
            }
            other => {
                let encoded = serde_json::to_vec(other).expect("json encode");
                out.push(5);
                write_vql(out, u32::try_from(encoded.len()).expect("fits u32"));
                out.extend_from_slice(&encoded);
            }
        }
    }

    fn channel_message(header: Value, body: &Value) -> Vec<u8> {
        let mut out = Vec::new();
        write_value(&mut out, &header);
        write_value(&mut out, body);
        out
    }

    fn promise_success(id: u32, body: &Value) -> Vec<u8> {
        channel_message(json!([RESPONSE_TYPE_PROMISE_SUCCESS, id]), body)
    }

    fn read_vql(bytes: &[u8], pos: &mut usize) -> u32 {
        let mut value: u32 = 0;
        let mut shift = 0;
        loop {
            let byte = bytes[*pos];
            *pos += 1;
            value |= u32::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return value;
            }
            shift += 7;
        }
    }

    fn read_value(bytes: &[u8], pos: &mut usize) -> Value {
        let tag = bytes[*pos];
        *pos += 1;
        match tag {
            0 => Value::Null,
            6 => Value::from(read_vql(bytes, pos)),
            1 | 2 | 3 | 5 => {
                let len = read_vql(bytes, pos) as usize;
                let slice = &bytes[*pos..*pos + len];
                *pos += len;
                if tag == 5 {
                    serde_json::from_slice(slice).expect("json body")
                } else {
                    Value::String(String::from_utf8(slice.to_vec()).expect("utf8"))
                }
            }
            4 => {
                let len = read_vql(bytes, pos) as usize;
                Value::Array((0..len).map(|_| read_value(bytes, pos)).collect())
            }
            other => panic!("unsupported tag {other}"),
        }
    }

    #[derive(Debug, Clone)]
    struct TestMessage {
        kind: u32,
        id: Option<u32>,
        channel: Option<String>,
        method: Option<String>,
        body: Value,
    }

    fn decode_message(bytes: &[u8]) -> TestMessage {
        let mut pos = 0;
        let header = read_value(bytes, &mut pos);
        let header = header.as_array().expect("header is an array");
        let body = if pos < bytes.len() {
            read_value(bytes, &mut pos)
        } else {
            Value::Null
        };
        TestMessage {
            kind: u32::try_from(header[0].as_u64().expect("kind")).expect("kind fits"),
            id: header
                .get(1)
                .and_then(Value::as_u64)
                .map(|value| u32::try_from(value).expect("id fits")),
            channel: header.get(2).and_then(Value::as_str).map(str::to_owned),
            method: header.get(3).and_then(Value::as_str).map(str::to_owned),
            body,
        }
    }

    // -----------------------------------------------------------------------
    // fake 对端：记录出站帧、按脚本注入应答与事件
    // -----------------------------------------------------------------------

    #[derive(Clone)]
    struct PeerConfig {
        workspaces: Vec<Value>,
        tasks: Vec<Value>,
        active_workspace_key: Option<String>,
        /// 故意错回显的 bridgeSessionId（ready 校验测试）。
        ready_echo: Option<String>,
        /// ready 里多写 bridgeGeneration（请求未声明 → 必须拒绝）。
        ready_extra_generation: bool,
        hello_protocol: u64,
        refuse_open: bool,
        controller_snapshot: bool,
        conversation_snapshot: Option<Value>,
        pre_ack_cards: Vec<Value>,
        /// 该桥第 N 个入站帧之后注入物理 seq 缺口。
        gap_after_frames: Option<usize>,
        resync_snapshot: Option<Value>,
    }

    impl Default for PeerConfig {
        fn default() -> Self {
            Self {
                workspaces: Vec::new(),
                tasks: Vec::new(),
                active_workspace_key: None,
                ready_echo: None,
                ready_extra_generation: false,
                hello_protocol: V4_WIRE_PROTOCOL_VERSION,
                refuse_open: false,
                controller_snapshot: true,
                conversation_snapshot: None,
                pre_ack_cards: Vec::new(),
                gap_after_frames: None,
                resync_snapshot: None,
            }
        }
    }

    #[derive(Default)]
    struct PeerInner {
        sent: Vec<Value>,
        seq: HashMap<String, u64>,
        message_seq: HashMap<String, u64>,
        initialized: HashSet<String>,
        listen: HashMap<String, u32>,
        conversation_subscriptions: u32,
        frames_sent: HashMap<String, usize>,
        gap_injected: HashSet<String>,
    }

    struct Peer {
        config: PeerConfig,
        inner: StdMutex<PeerInner>,
        manager: StdMutex<Weak<BridgeManager>>,
    }

    impl Peer {
        fn new(config: PeerConfig) -> Arc<Self> {
            Arc::new(Self {
                config,
                inner: StdMutex::new(PeerInner::default()),
                manager: StdMutex::new(Weak::new()),
            })
        }

        fn attach(&self, manager: &Arc<BridgeManager>) {
            *self.manager.lock().expect("peer lock") = Arc::downgrade(manager);
        }

        fn sent(&self) -> Vec<Value> {
            self.inner.lock().expect("peer lock").sent.clone()
        }

        async fn handle(&self, payload: Value) {
            let responses = {
                let mut inner = self.inner.lock().expect("peer lock");
                inner.sent.push(payload.clone());
                self.respond(&mut inner, &payload)
            };
            let manager = self.manager.lock().expect("peer lock").upgrade();
            if let Some(manager) = manager {
                for response in responses {
                    manager.on_data(response).await.expect("inbound payload");
                }
            }
        }

        fn respond(&self, inner: &mut PeerInner, payload: &Value) -> Vec<Value> {
            let mut out = Vec::new();
            match payload.get("zcode_type").and_then(Value::as_str) {
                Some(BOOTSTRAP_REQUEST) => out.push(json!({
                    "zcode_type": "bootstrap-response",
                    "requestId": payload.get("requestId").cloned().unwrap_or(Value::Null),
                    "result": {
                        "workspaces": self.config.workspaces,
                        "mobileViewState": {"activeWorkspaceKey": self.config.active_workspace_key}
                    }
                })),
                Some(WORKSPACE_LIST_REQUEST) => out.push(json!({
                    "zcode_type": "workspace-list-response",
                    "requestId": payload.get("requestId").cloned().unwrap_or(Value::Null),
                    "result": {
                        "workspaces": self.config.workspaces,
                        "activeWorkspaceKey": self.config.active_workspace_key
                    }
                })),
                Some(WORKSPACE_BRIDGE_OPEN) => self.respond_open(inner, payload, &mut out),
                Some("rpc-frame") => self.respond_rpc(inner, payload, &mut out),
                _ => {}
            }
            out
        }

        fn respond_open(&self, inner: &mut PeerInner, payload: &Value, out: &mut Vec<Value>) {
            let request_id = payload.get("requestId").cloned().unwrap_or(Value::Null);
            if self.config.refuse_open {
                out.push(json!({
                    "zcode_type": WORKSPACE_BRIDGE_ERROR,
                    "requestId": request_id,
                    "reason": "synthetic-refused",
                    "error": "synthetic"
                }));
                return;
            }
            let requested = payload
                .get("bridgeSessionId")
                .and_then(Value::as_str)
                .expect("bridgeSessionId");
            let echoed = self
                .config
                .ready_echo
                .clone()
                .unwrap_or_else(|| requested.to_owned());
            let mut ready = json!({
                "zcode_type": WORKSPACE_BRIDGE_READY,
                "requestId": request_id,
                "bridgeSessionId": echoed,
                "bridge": {"bridgeSessionId": echoed, "kind": "workspace"}
            });
            if self.config.ready_extra_generation {
                ready["bridgeGeneration"] = json!(1);
            }
            out.push(ready);
            if inner.initialized.insert(echoed.clone()) {
                out.push(self.frame(inner, &echoed, INITIALIZE_BYTES.to_vec()));
            }
        }

        fn frame(&self, inner: &mut PeerInner, bridge: &str, bytes: Vec<u8>) -> Value {
            let identity = RpcIdentity::new(bridge);
            let seq = self.next_seq(inner, bridge);
            let message_seq = {
                let counter = inner.message_seq.entry(bridge.to_owned()).or_insert(0);
                *counter += 1;
                *counter
            };
            encode_frame(&identity, seq, message_seq, &bytes).expect("encode inbound frame")
        }

        fn next_seq(&self, inner: &mut PeerInner, bridge: &str) -> u64 {
            let previous = *inner.frames_sent.get(bridge).unwrap_or(&0);
            let counter = inner.seq.entry(bridge.to_owned()).or_insert(0);
            *counter += 1;
            if let Some(threshold) = self.config.gap_after_frames {
                if previous >= threshold && inner.gap_injected.insert(bridge.to_owned()) {
                    *counter += 1;
                }
            }
            *inner.frames_sent.entry(bridge.to_owned()).or_insert(0) += 1;
            *counter
        }

        fn respond_rpc(&self, inner: &mut PeerInner, payload: &Value, out: &mut Vec<Value>) {
            let bridge = payload
                .get("bridgeSessionId")
                .and_then(Value::as_str)
                .expect("bridgeSessionId")
                .to_owned();
            let data = payload
                .get("dataBase64")
                .and_then(Value::as_str)
                .expect("dataBase64");
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(data)
                .expect("base64");
            let message = decode_message(&bytes);
            match message.kind {
                102 => {
                    if let Some(id) = message.id {
                        inner.listen.insert(bridge, id);
                    }
                }
                100 => {
                    let id = message.id.expect("request id");
                    let method = message.method.clone().unwrap_or_default();
                    let first = message.body.get(0).cloned().unwrap_or(Value::Null);
                    match method.as_str() {
                        AGENT_HELLO_METHOD => {
                            let hello = json!({
                                "kind": "hello",
                                "protocolVersion": self.config.hello_protocol,
                                "connectionId": "synthetic-connection-0001",
                                "clientMode": "web-remote-replayable",
                                "deliveryProfile": "web",
                                "serverTime": 0
                            });
                            out.push(self.frame(inner, &bridge, promise_success(id, &hello)));
                        }
                        AGENT_CLIENT_HELLO_METHOD => {
                            out.push(self.frame(inner, &bridge, promise_success(id, &Value::Null)));
                        }
                        CONTROLLER_SUBSCRIBE_METHOD => {
                            let ack = json!({"ack": {
                                "subscriptionId": "ctrl-sub-synthetic-1",
                                "mode": "snapshot",
                                "logEpoch": "synthetic-epoch"
                            }});
                            out.push(self.frame(inner, &bridge, promise_success(id, &ack)));
                            if self.config.controller_snapshot {
                                let listen = inner.listen.get(&bridge).copied().unwrap_or(0);
                                let event = json!({
                                    "topic": CONTROLLER_TASKS_INDEX_TOPIC,
                                    "subscriptionId": "ctrl-sub-synthetic-1",
                                    "logEpoch": "synthetic-epoch",
                                    "fromSeq": 0,
                                    "toSeq": 1,
                                    "sentAt": 0,
                                    "payload": {
                                        "kind": "snapshot",
                                        "snapshot": {
                                            "protocolVersion": 1,
                                            "logEpoch": "synthetic-epoch",
                                            "tasks": self.config.tasks
                                        }
                                    }
                                });
                                out.push(self.frame(
                                    inner,
                                    &bridge,
                                    channel_message(
                                        json!([RESPONSE_TYPE_EVENT_FIRE, listen]),
                                        &event,
                                    ),
                                ));
                            }
                        }
                        AGENT_SUBSCRIBE_CONVERSATION_METHOD => {
                            let session_id = first
                                .get("sessionId")
                                .and_then(Value::as_str)
                                .expect("sessionId")
                                .to_owned();
                            inner.conversation_subscriptions += 1;
                            let count = inner.conversation_subscriptions;
                            let subscription = format!("conv-sub-synthetic-{count}");
                            let listen = inner.listen.get(&bridge).copied().unwrap_or(0);
                            let topic = format!("{CONVERSATION_TOPIC_PREFIX}{session_id}");
                            if count == 1 && !self.config.pre_ack_cards.is_empty() {
                                let wire = test_wire(
                                    &topic,
                                    &subscription,
                                    json!({
                                        "kind": "snapshot",
                                        "snapshot": conversation_snapshot(&session_id, 2, self.config.pre_ack_cards.clone())
                                    }),
                                );
                                out.push(self.frame(
                                    inner,
                                    &bridge,
                                    channel_message(
                                        json!([RESPONSE_TYPE_EVENT_FIRE, listen]),
                                        &wire,
                                    ),
                                ));
                            }
                            let ack = json!({"ack": {
                                "subscriptionId": subscription,
                                "mode": "snapshot",
                                "logEpoch": "synthetic-epoch"
                            }});
                            out.push(self.frame(inner, &bridge, promise_success(id, &ack)));
                            let snapshot = if count == 1 {
                                self.config.conversation_snapshot.clone()
                            } else {
                                self.config.resync_snapshot.clone()
                            };
                            if let Some(snapshot) = snapshot {
                                let wire = test_wire(
                                    &topic,
                                    &subscription,
                                    json!({"kind": "snapshot", "snapshot": snapshot}),
                                );
                                out.push(self.frame(
                                    inner,
                                    &bridge,
                                    channel_message(
                                        json!([RESPONSE_TYPE_EVENT_FIRE, listen]),
                                        &wire,
                                    ),
                                ));
                            }
                        }
                        _ => {
                            out.push(self.frame(
                                inner,
                                &bridge,
                                promise_success(id, &json!({"method": method})),
                            ));
                        }
                    }
                }
                _ => {}
            }
        }
    }

    impl RelaySink for Peer {
        fn send_data(&self, payload: Value) -> BoxFuture<'_, ZResult<()>> {
            Box::pin(async move {
                self.handle(payload).await;
                Ok(())
            })
        }
    }

    // -----------------------------------------------------------------------
    // 合成夹具
    // -----------------------------------------------------------------------

    fn workspace_entry(path: &str, identity: Option<&str>, label: &str) -> Value {
        let mut entry = json!({
            "workspacePath": path,
            "label": label,
            "kind": "project",
            "workspacePurpose": "project"
        });
        if let Some(identity) = identity {
            entry["workspaceIdentity"] = json!(identity);
        }
        entry
    }

    fn task_row(path: &str, task_id: &str, title: &str) -> Value {
        json!({
            "address": {"workspacePath": path, "taskId": task_id},
            "meta": {"title": title, "taskId": task_id, "workspacePath": path}
        })
    }

    fn permission_card(interaction_id: &str) -> Value {
        json!({
            "interactionId": interaction_id,
            "kind": "permission",
            "anchorRowId": 12,
            "createdAt": 1_700_000_000_000u64,
            "payload": {
                "kind": "permission",
                "toolCallId": "call_synthetic_0001",
                "toolName": "Write",
                "summary": "synthetic: write a scratch file",
                "options": [{"optionId": "allowOnce", "label": "Allow once", "kind": "allowOnce"}]
            }
        })
    }

    fn conversation_snapshot(session_id: &str, revision: u64, cards: Vec<Value>) -> Value {
        json!({
            "protocolVersion": 1,
            "sessionId": session_id,
            "logEpoch": "synthetic-epoch",
            "seq": revision,
            "revision": revision,
            "control": {"activeWorks": []},
            "pendingInteractions": cards,
            "config": {
                "provider": "synthetic-provider",
                "model": "synthetic-model-a",
                "thought": "high"
            }
        })
    }

    fn test_wire(topic: &str, subscription: &str, payload: Value) -> Value {
        let (from, to) = match payload.get("kind").and_then(Value::as_str) {
            Some("snapshot") => (0, 1),
            _ => (1, 2),
        };
        json!({
            "wireVersion": V4_WIRE_PROTOCOL_VERSION,
            "kind": "complete",
            "deliveryKind": "initial",
            "logicalFrameId": "synthetic-logical-1",
            "logicalFrameOrdinal": 1,
            "topic": topic,
            "subscriptionId": subscription,
            "frame": {
                "topic": topic,
                "subscriptionId": subscription,
                "fromSeq": from,
                "toSeq": to,
                "sentAt": 0,
                "payload": payload
            }
        })
    }

    async fn manager_with(config: PeerConfig) -> (Arc<BridgeManager>, Arc<Peer>) {
        let peer = Peer::new(config);
        let manager = Arc::new(BridgeManager::with_timing(
            peer.clone(),
            BridgeTiming::test(),
        ));
        peer.attach(&manager);
        manager.invalidate(1).await;
        (manager, peer)
    }

    fn app_payloads(peer: &Peer, zcode_type: &str) -> Vec<Value> {
        peer.sent()
            .into_iter()
            .filter(|payload| payload.get("zcode_type").and_then(Value::as_str) == Some(zcode_type))
            .collect()
    }

    fn decode_outbound(payload: &Value) -> TestMessage {
        let data = payload
            .get("dataBase64")
            .and_then(Value::as_str)
            .expect("dataBase64");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .expect("base64");
        decode_message(&bytes)
    }

    fn channel_frames(peer: &Peer) -> Vec<TestMessage> {
        peer.sent()
            .iter()
            .filter(|payload| {
                payload.get("zcode_type").and_then(Value::as_str) == Some("rpc-frame")
            })
            .map(decode_outbound)
            .collect()
    }

    fn basic_config() -> PeerConfig {
        PeerConfig {
            workspaces: vec![
                workspace_entry(WS_A, Some("ws-a-identity"), "A"),
                workspace_entry(WS_B, Some("ws-b-identity"), "B"),
            ],
            tasks: vec![
                task_row(WS_A, SESSION_A, "hi"),
                task_row(WS_B, SESSION_B, "hi"),
            ],
            conversation_snapshot: Some(conversation_snapshot(SESSION_A, 2, Vec::new())),
            ..PeerConfig::default()
        }
    }

    fn key(session_id: &str) -> TargetKey {
        TargetKey {
            session_id: session_id.to_owned(),
        }
    }

    // -----------------------------------------------------------------------
    // 用例
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn same_title_different_workspaces_is_not_a_mapping() {
        let config = PeerConfig {
            workspaces: vec![
                workspace_entry(WS_A, Some("ws-a-identity"), "A"),
                workspace_entry(WS_B, Some("ws-b-identity"), "B"),
            ],
            tasks: vec![
                task_row(WS_A, "sess_dup_exact_0001", "hi"),
                task_row(WS_B, "sess_dup_exact_0001", "hi"),
                task_row(WS_A, SESSION_A, "hi"),
                task_row(WS_B, SESSION_B, "hi"),
            ],
            ..PeerConfig::default()
        };
        let (manager, peer) = manager_with(config).await;
        let mapper_for_duplicate_exact_task_ids = &manager;
        let duplicate_key = key("sess_dup_exact_0001");
        assert!(mapper_for_duplicate_exact_task_ids
            .bind(&duplicate_key)
            .await
            .is_err());
        assert!(manager
            .bind(&duplicate_key)
            .await
            .expect_err("duplicate exact task ids must not be guessed")
            .is(codes::TARGET_AMBIGUOUS));

        // 正控：同标题但 id 唯一的两个会话各自精确绑定到自己的 workspace。
        let a = manager.bind(&key(SESSION_A)).await.expect("bind A");
        let b = manager.bind(&key(SESSION_B)).await.expect("bind B");
        assert_eq!(a.workspace_path, WS_A);
        assert_eq!(b.workspace_path, WS_B);
        assert_ne!(a.workspace_path, b.workspace_path);

        // 标题本身绝不作映射键（没有按标题/相似度查找的入口）。
        assert!(manager
            .bind(&key("hi"))
            .await
            .expect_err("title is not an id")
            .is(codes::NO_SESSION));

        // 枚举桥共享：一次枚举 + 两个会话桥。
        assert_eq!(app_payloads(&peer, WORKSPACE_BRIDGE_OPEN).len(), 3);
    }

    #[tokio::test]
    async fn both_id_forms_are_preserved_verbatim() {
        let config = PeerConfig {
            workspaces: vec![
                workspace_entry(WS_A, Some("ws-a-identity"), "A"),
                workspace_entry(WS_B, None, "B"),
            ],
            tasks: vec![
                task_row(WS_A, SESSION_A, "same title"),
                task_row(WS_B, SESSION_HEX, "same title"),
            ],
            ..PeerConfig::default()
        };
        let (manager, peer) = manager_with(config).await;
        let a = manager.bind(&key(SESSION_A)).await.expect("bind sess_");
        let b = manager
            .bind(&key(SESSION_HEX))
            .await
            .expect("bind session_");
        assert_eq!(a.session_id, SESSION_A);
        assert_eq!(b.session_id, SESSION_HEX);
        assert_eq!(a.workspace_path, WS_A);
        assert_eq!(b.workspace_path, WS_B);
        // 不做任何去横杠/大小写归一：去掉前缀的写法不得命中。
        assert!(manager
            .bind(&key(SESSION_HEX.trim_start_matches("session_")))
            .await
            .expect_err("no normalization")
            .is(codes::NO_SESSION));

        let opens = app_payloads(&peer, WORKSPACE_BRIDGE_OPEN);
        let task_ids: Vec<&str> = opens
            .iter()
            .filter_map(|payload| payload.get("taskId").and_then(Value::as_str))
            .collect();
        assert_eq!(task_ids, vec![SESSION_A, SESSION_HEX]);
    }

    #[tokio::test]
    async fn bind_sequence_uses_bootstrap_then_listing_then_exact_task_id() {
        let (manager, peer) = manager_with(basic_config()).await;
        let target = manager.bind(&key(SESSION_A)).await.expect("bind");
        assert_eq!(target.workspace_key, "ws-a-identity");

        let sent = peer.sent();
        assert_eq!(sent[0]["zcode_type"], json!(BOOTSTRAP_REQUEST));
        assert_eq!(sent[1]["zcode_type"], json!(WORKSPACE_LIST_REQUEST));
        assert_eq!(sent[2]["zcode_type"], json!(WORKSPACE_BRIDGE_OPEN));

        let opens = app_payloads(&peer, WORKSPACE_BRIDGE_OPEN);
        assert_eq!(opens.len(), 2);
        // 枚举期省略 taskId（官方 taskId? 可选；本模块只在绑定会话后携带）。
        assert!(opens[0].get("taskId").is_none());
        assert_eq!(opens[0]["workspaceKey"], json!("ws-a-identity"));
        // 会话桥携带精确 taskId + 宿主 workspaceKey。
        assert_eq!(opens[1]["taskId"], json!(SESSION_A));
        assert_eq!(opens[1]["workspaceKey"], json!("ws-a-identity"));

        let frames = channel_frames(&peer);
        let sequence: Vec<(u32, Option<String>, Option<String>)> = frames
            .iter()
            .map(|frame| (frame.kind, frame.channel.clone(), frame.method.clone()))
            .collect();
        assert_eq!(
            sequence,
            vec![
                (
                    102,
                    Some(WINDOW_CONTROLLER_CHANNEL.to_owned()),
                    Some(CONTROLLER_DYNAMIC_FRAME_EVENT.to_owned())
                ),
                (
                    100,
                    Some(WINDOW_CONTROLLER_CHANNEL.to_owned()),
                    Some(CONTROLLER_SUBSCRIBE_METHOD.to_owned())
                ),
                (
                    100,
                    Some(AGENT_CHANNEL.to_owned()),
                    Some(AGENT_HELLO_METHOD.to_owned())
                ),
                (
                    100,
                    Some(AGENT_CHANNEL.to_owned()),
                    Some(AGENT_CLIENT_HELLO_METHOD.to_owned())
                ),
                (
                    102,
                    Some(AGENT_CHANNEL.to_owned()),
                    Some(AGENT_CONVERSATION_FRAME_EVENT.to_owned())
                ),
                (
                    100,
                    Some(AGENT_CHANNEL.to_owned()),
                    Some(AGENT_SUBSCRIBE_CONVERSATION_METHOD.to_owned())
                ),
            ]
        );

        // controller 订阅形状（不自造 tasks-index-request）。
        assert_eq!(
            frames[1].body,
            json!([{"topic": CONTROLLER_TASKS_INDEX_TOPIC, "visibility": "foreground"}])
        );
        // hello 无参 = 空数组（官方 channel.call(..., []) 序列化）。
        assert_eq!(frames[2].body, json!([]));
        let client_hello = &frames[3].body[0];
        assert_eq!(client_hello["protocolVersion"], json!(3));
        assert_eq!(client_hello["clientKind"], json!("web"));
        assert_eq!(client_hello["kind"], json!("clientHello"));
        assert_ne!(client_hello["clientKind"], json!("desktop"));
        // 会话 EventListen 携带 workspace 参数（无 identity 时保持缺席）。
        let listen_arg = &frames[4].body;
        assert_eq!(listen_arg["workspacePath"], json!(WS_A));
        assert_eq!(listen_arg["workspaceIdentity"], json!("ws-a-identity"));
        assert_eq!(frames[5].body[0]["sessionId"], json!(SESSION_A));
    }

    #[tokio::test]
    async fn event_before_subscribe_ack_is_staged_not_lost() {
        let config = PeerConfig {
            workspaces: vec![workspace_entry(WS_A, None, "A")],
            tasks: vec![task_row(WS_A, SESSION_A, "hi")],
            pre_ack_cards: vec![permission_card("pi-synthetic-staged-1")],
            conversation_snapshot: None,
            ..PeerConfig::default()
        };
        let (manager, _peer) = manager_with(config).await;
        let target = manager.bind(&key(SESSION_A)).await.expect("bind");
        let view = manager.session(&target).await.expect("session");
        let matching_staged_frames_delivered_after_ack = view.pending.len();
        assert_eq!(matching_staged_frames_delivered_after_ack, 1);
        assert_eq!(view.pending[0].interaction_id, "pi-synthetic-staged-1");
        assert_eq!(
            manager
                .session_stats(SESSION_A)
                .expect("stats")
                .staged_delivered,
            1
        );
    }

    #[tokio::test]
    async fn wrong_ready_echo_is_rejected() {
        let config = PeerConfig {
            ready_echo: Some("tuvis-bridge-forged-0001".to_owned()),
            ..basic_config()
        };
        let (manager, _peer) = manager_with(config).await;
        let error = manager.bind(&key(SESSION_A)).await.expect_err("wrong echo");
        assert!(error.is(codes::UNSUPPORTED_PROTOCOL));

        // 正控：同一 listing 下正常回显可以绑定。
        let (ok_manager, _ok_peer) = manager_with(basic_config()).await;
        assert!(ok_manager.bind(&key(SESSION_A)).await.is_ok());
    }

    #[tokio::test]
    async fn ready_echo_must_not_add_unrequested_identity() {
        // 请求未声明 bridgeGeneration，ready 却多写 → 身份不成立（严格相等口径）。
        let config = PeerConfig {
            ready_extra_generation: true,
            ..basic_config()
        };
        let (manager, _peer) = manager_with(config).await;
        assert!(manager
            .bind(&key(SESSION_A))
            .await
            .expect_err("extra identity")
            .is(codes::UNSUPPORTED_PROTOCOL));
    }

    #[tokio::test]
    async fn wrong_protocol_version_is_rejected() {
        let config = PeerConfig {
            hello_protocol: 2,
            ..basic_config()
        };
        let (manager, peer) = manager_with(config).await;
        let error = manager.bind(&key(SESSION_A)).await.expect_err("hello v2");
        assert!(error.is(codes::UNSUPPORTED_PROTOCOL));
        // 握手未过：一个会话订阅/事件监听帧都不发。
        let frames = channel_frames(&peer);
        assert!(frames
            .iter()
            .all(|frame| frame.method.as_deref() != Some(AGENT_SUBSCRIBE_CONVERSATION_METHOD)));
        assert!(frames
            .iter()
            .all(|frame| frame.method.as_deref() != Some(AGENT_CONVERSATION_FRAME_EVENT)));
    }

    #[tokio::test]
    async fn missing_seq_triggers_readonly_resync() {
        let config = PeerConfig {
            workspaces: vec![workspace_entry(WS_A, None, "A")],
            tasks: vec![task_row(WS_A, SESSION_A, "hi")],
            conversation_snapshot: Some(conversation_snapshot(SESSION_A, 2, Vec::new())),
            gap_after_frames: Some(4),
            resync_snapshot: Some(conversation_snapshot(
                SESSION_A,
                11,
                vec![permission_card("pi-synthetic-after-resync")],
            )),
            ..PeerConfig::default()
        };
        let (manager, peer) = manager_with(config).await;
        let target = manager.bind(&key(SESSION_A)).await.expect("bind");
        // 第 5 帧（首个快照）物理 seq 跳号 → 该帧丢弃、投影不新鲜。
        let stale = manager.peek_view(SESSION_A).expect("bound");
        assert!(!stale.ready);
        assert_eq!(stale.revision, None);

        // 读取面触发只读重订阅（不是任何 v4 写命令）。
        let resynced = manager.session(&target).await.expect("session");
        assert!(resynced.ready);
        assert_eq!(resynced.revision, Some(11));
        assert_eq!(resynced.pending.len(), 1);
        assert_eq!(manager.session_stats(SESSION_A).expect("stats").resyncs, 1);

        let subscribes = channel_frames(&peer)
            .into_iter()
            .filter(|frame| frame.method.as_deref() == Some(AGENT_SUBSCRIBE_CONVERSATION_METHOD))
            .count();
        assert_eq!(
            subscribes, 2,
            "initial subscribe + one read-only resubscribe"
        );
    }

    #[tokio::test]
    async fn old_generation_is_stale_for_reads_and_calls() {
        let (manager, _peer) = manager_with(basic_config()).await;
        let target = manager.bind(&key(SESSION_A)).await.expect("bind");
        assert_eq!(target.generation, 1);
        manager.invalidate(2).await;
        assert!(manager
            .session(&target)
            .await
            .expect_err("old generation session")
            .is(codes::STALE_GENERATION));
        assert!(manager
            .call(&target, AGENT_CHANNEL, "syntheticRead", vec![])
            .await
            .expect_err("old generation call")
            .is(codes::STALE_GENERATION));

        // 新代际必须重新绑定（旧映射不授权新连接）。
        let rebound = manager.bind(&key(SESSION_A)).await.expect("rebind");
        assert_eq!(rebound.generation, 2);
    }

    #[tokio::test]
    async fn missing_workspace_identity_is_not_invented() {
        let config = PeerConfig {
            workspaces: vec![workspace_entry(WS_A, None, "A")],
            tasks: vec![task_row(WS_A, SESSION_A, "hi")],
            ..PeerConfig::default()
        };
        let (manager, peer) = manager_with(config).await;
        let target = manager.bind(&key(SESSION_A)).await.expect("bind");
        assert_eq!(target.workspace_identity, None);
        assert_eq!(target.workspace_key, WS_A);

        let frames = channel_frames(&peer);
        let listen = frames
            .iter()
            .find(|frame| frame.method.as_deref() == Some(AGENT_CONVERSATION_FRAME_EVENT))
            .expect("conversation listen");
        assert_eq!(listen.body["workspacePath"], json!(WS_A));
        assert!(listen.body.get("workspaceIdentity").is_none());
        let subscribe = frames
            .iter()
            .find(|frame| frame.method.as_deref() == Some(AGENT_SUBSCRIBE_CONVERSATION_METHOD))
            .expect("conversation subscribe");
        assert!(subscribe.body[0].get("workspaceIdentity").is_none());
    }

    #[tokio::test]
    async fn call_requires_ready_and_returns_bridge_body() {
        let config = PeerConfig {
            conversation_snapshot: None,
            ..basic_config()
        };
        let (manager, peer) = manager_with(config).await;
        let target = manager.bind(&key(SESSION_A)).await.expect("bind");
        assert!(manager
            .call(&target, AGENT_CHANNEL, "syntheticRead", vec![])
            .await
            .expect_err("no snapshot yet")
            .is(codes::NOT_READY));
        assert!(channel_frames(&peer)
            .iter()
            .all(|frame| frame.method.as_deref() != Some("syntheticRead")));

        let (ready_manager, ready_peer) = manager_with(basic_config()).await;
        let ready_target = ready_manager.bind(&key(SESSION_A)).await.expect("bind");
        let body = ready_manager
            .call(
                &ready_target,
                AGENT_CHANNEL,
                "syntheticRead",
                vec![json!({})],
            )
            .await
            .expect("read call");
        assert_eq!(body, json!({"method": "syntheticRead"}));
        let calls = channel_frames(&ready_peer)
            .into_iter()
            .filter(|frame| frame.method.as_deref() == Some("syntheticRead"))
            .count();
        assert_eq!(calls, 1);
    }

    #[tokio::test]
    async fn foreign_bridge_frames_are_ignored() {
        let (manager, peer) = manager_with(basic_config()).await;
        let target = manager.bind(&key(SESSION_A)).await.expect("bind");
        let before = manager.peek_view(SESSION_A).expect("bound");

        // 未知 bridgeSessionId 的合法帧：静默丢弃，不碰已绑定投影。
        let forged = encode_frame(
            &RpcIdentity::new("tuvis-bridge-foreign-0001"),
            1,
            1,
            &channel_message(
                json!([RESPONSE_TYPE_EVENT_FIRE, 7]),
                &test_wire(
                    &format!("{CONVERSATION_TOPIC_PREFIX}{SESSION_A}"),
                    "conv-sub-synthetic-1",
                    json!({
                        "kind": "snapshot",
                        "snapshot": conversation_snapshot(SESSION_A, 99, Vec::new())
                    }),
                ),
            ),
        )
        .expect("forged frame");
        manager
            .on_data(forged)
            .await
            .expect("ignore foreign bridge");

        let after = manager.peek_view(SESSION_A).expect("bound");
        assert_eq!(after.revision, before.revision);
        assert_eq!(after.session_id, target.session_id);
        assert!(!peer.sent().is_empty());
    }

    #[tokio::test]
    async fn refused_bridge_open_maps_to_not_ready() {
        let config = PeerConfig {
            refuse_open: true,
            ..basic_config()
        };
        let (manager, peer) = manager_with(config).await;
        assert!(manager
            .bind(&key(SESSION_A))
            .await
            .expect_err("refused")
            .is(codes::NOT_READY));
        assert!(channel_frames(&peer).is_empty());
    }

    #[tokio::test]
    async fn bind_rejects_empty_or_padded_ids() {
        let (manager, _peer) = manager_with(basic_config()).await;
        assert!(manager.bind(&key("")).await.is_err());
        assert!(manager.bind(&key("   ")).await.is_err());
        assert!(manager
            .bind(&key(&format!(" {SESSION_A} ")))
            .await
            .expect_err("verbatim ids only")
            .is(codes::NO_SESSION));
    }
}
