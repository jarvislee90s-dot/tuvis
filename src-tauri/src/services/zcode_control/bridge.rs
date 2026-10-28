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
//!   保留。**跨 workspace** 同 taskId 命中多行 → `target_ambiguous`；同 workspace 的
//!   重复行按 `(workspacePath, taskId)` 折叠为同一目标。
//! - 入站帧按 bridgeSessionId/身份、物理 `seq`/`messageSeq`、topic、subscriptionId、
//!   代际逐层过滤；任何缺口/未知 schema/协议不符 → `ready=false`，重订阅取只读快照，
//!   期间控制面零写。
//! - 本模块**不构造任何会话写命令**（T3 起才在 `command.rs` 引入）。

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{Map, Value};
use tokio::sync::{mpsc, oneshot, watch, Mutex as AsyncMutex, Notify};

use super::capability::confirm::ConfirmationObservation;
use super::capability::state::ZcodeInterfaceEvidence;
use super::codec::{
    crc32_hex, decode_channel, encode_ack, encode_call, encode_frame, encode_listen,
    ChannelMessage, FrameAssembler, RpcIdentity, RESPONSE_TYPE_EVENT_FIRE,
    RESPONSE_TYPE_INITIALIZE, RESPONSE_TYPE_PROMISE_ERROR, RESPONSE_TYPE_PROMISE_SUCCESS,
};
use super::projection::{Projection, CONVERSATION_TOPIC_PREFIX};
use super::selection::parse_selection_view;
use super::types::{
    codes, BoundTarget, CommandPort, HostBuildProbe, HostEvidence, PendingKind, PendingPort,
    PendingView, RelaySink, SelectionPort, SelectionView, SessionView, StopPort, TargetKey,
    ZResult, ZcodeError,
};

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
/// controller 退订方法（`windowController.ts:77`；窗闭/缺口后尽力退订）。
pub const CONTROLLER_UNSUBSCRIBE_METHOD: &str = "unsubscribeControllerV4";
/// 会话退订方法（`zcodeAgent.ts:940-946`；重订阅前释放旧订阅）。
pub const AGENT_UNSUBSCRIBE_CONVERSATION_METHOD: &str = "unsubscribeConversationV4";
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
/// 单条 conversation 帧暂存条数上限（ACK 前/重订阅窗口；防恶意帧撑爆内存）。
///
/// 与 codec 的物理分片 `MAX_STAGED_FRAMES` 同名不同义（那是 physical wire 层），
/// 这里限定的是已解包的 conversation logical frame。
pub const MAX_STAGED_CONVERSATION_FRAMES: usize = 1024;
/// controller 任务索引行数上限（合同 `maxRows=1024`，超出拒收并计数）。
pub const MAX_TASK_ROWS: usize = 1024;
/// 单代际最多枚举的 workspace 数（每个 workspace 一条枚举桥）。
pub const MAX_ENUM_WORKSPACES: usize = 16;
/// 单会话并发分片重组组数上限（参照实现 `CONVERSATION_MAX_FRAGMENT_ASSEMBLIES`）。
pub const MAX_FRAGMENT_ASSEMBLIES: usize = 32;
/// 单条 logical frame 的最大分片数（参照实现同值）。
pub const MAX_LOGICAL_FRAGMENTS: u64 = 1024;
/// 单会话连续重同步次数上限（超过即停在 `ready=false`，不无限重订阅）。
pub const MAX_RESYNCS: u32 = 8;
/// 单个 workspace 枚举桥连续错过 controller 收帧窗（或订阅失败）的重订阅上限。
///
/// 超过即停用该桥的任务索引（不再重订阅），直到换代或该桥物理缺口重置预算；
/// 防止「订阅成功但快照永不到」把每次 `bind` 都变成一次重订阅。
pub const MAX_CONTROLLER_MISSES: u32 = 8;

/// 桥接域的资源界限（与超时一起注入；生产默认值见 [`Default`]）。
#[derive(Debug, Clone, Copy)]
pub(crate) struct BridgeLimits {
    /// 单会话 ACK 前暂存帧的总字节预算。
    pub staged_bytes: usize,
    /// 单条 conversational logical frame 进入投影的字节上限。
    pub projection_bytes: usize,
    /// 绑定会话总数上限。
    pub sessions: usize,
    /// 逻辑桥总数上限。
    pub bridges: usize,
    /// 出站命令队列深度上限（背压可见，避免无界内存）。
    pub command_queue: usize,
}

impl Default for BridgeLimits {
    fn default() -> Self {
        Self {
            staged_bytes: 16 * 1024 * 1024,
            projection_bytes: super::projection::MAX_LOGICAL_FRAME_BYTES,
            sessions: 256,
            bridges: 512,
            command_queue: 256,
        }
    }
}

impl BridgeLimits {
    /// 测试界限（小预算，验证边界而不分配 MB 级负载）。
    #[cfg(test)]
    pub(crate) const fn test() -> Self {
        Self {
            staged_bytes: 64 * 1024,
            projection_bytes: 64 * 1024,
            sessions: 8,
            bridges: 16,
            command_queue: 8,
        }
    }
}

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
    /// 同一会话两次只读重同步的最小间隔（指数退避基数；防重订阅自激）。
    pub resync_backoff: Duration,
    /// controller 收帧窗错过后重订阅的基础退避（指数增长；防 tight loop）。
    pub controller_backoff: Duration,
}

impl Default for BridgeTiming {
    fn default() -> Self {
        Self {
            app_request: Duration::from_secs(10),
            initialize: Duration::from_secs(15),
            rpc: Duration::from_secs(10),
            task_window: Duration::from_secs(8),
            resync_backoff: Duration::from_secs(2),
            controller_backoff: Duration::from_secs(2),
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
            resync_backoff: Duration::from_millis(60),
            controller_backoff: Duration::from_millis(60),
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
        /// 写前在 actor 内复核的会话就绪闸（`call` 专用；T3 写命令的前置）。
        guard: Option<RpcGuard>,
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
    /// 尽力发送一条读面生命周期 RPC（退订）；无 waiter、失败静默。
    SendRpcNowait {
        bridge_id: String,
        identity: RpcIdentity,
        id: u32,
        channel: String,
        method: String,
        args: Vec<Value>,
    },
    /// 清空 seq/messageSeq 计数（代际变化后旧桥作废）。
    Reset,
}

/// `call` 的就绪闸：actor 在真正写帧前复核（gap 与 check 之间零写窗口）。
#[derive(Debug, Clone)]
struct RpcGuard {
    session_id: String,
    generation: u64,
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
    /// 已通过 `helloConversationV4` 校验（本代际真实观测）。
    hello_known: bool,
    /// 已成功应用有效 conversation snapshot/delta（schema 已识别）。
    schema_known: bool,
    /// hello 校验到的 v4 wire 协议版本（未握手 = `None`）。
    protocol_version: Option<u32>,
    /// 本代际接口形状证据（T3 桥观察采集；invalidate 清空）。
    interface: ZcodeInterfaceEvidence,
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
            hello_known: false,
            schema_known: false,
            protocol_version: None,
            interface: ZcodeInterfaceEvidence::default(),
        }
    }
}

/// 接口形状采集（T3；spec R3「静默接口解析」）：把真实 hello / 会话帧 / 选择目录
/// 流量里的**方法存在性与形状布尔**喂给 [`ZcodeInterfaceEvidence`]。
///
/// 采集纪律：
/// - 全部只读、零 I/O、幂等 OR 置位（只见过的形状不因后续帧消失而回退）；
/// - 输入 JSON 的题干、选项、答案、命令、JWT、链接等正文**只用于形状判定**，
///   绝不落任何字段——本类型 12 个字段全是 bool，正文无处可存；
/// - 观察挂在「已过 topic/subscriptionId 闸的本代际真实下行帧」上（会话帧在
///   [`apply_frame`] 进入投影前观察；hello 在 kind/协议/connectionId 校验通过后
///   观察；目录在 [`parse_selection_view`] 解析成功后观察）。
///
/// 方法实现放 bridge 侧（控制者裁决允许）：wire 形状（hello 方法表、logical frame
/// 的 snapshot/deltas 包装、getView 目录）是桥的领域知识，`capability/` 纯核保持
/// 零 serde_json 依赖。
impl ZcodeInterfaceEvidence {
    /// hello 方法表存在性：`method_send` / `method_resolve_interaction` /
    /// `method_stop` / `method_selection_get` / `method_selection_switch`。
    ///
    /// - hello 应答带显式方法表（`methods`：方法名数组或方法描述对象）→ 按成员
    ///   判定（缺席方法保持 false，绝不猜测）；
    /// - 无显式方法表 → 只对**已校验的 v4/3 协议面**背书五条控制方法（v3 合同
    ///   钉源 + 实机探针取证；函数自含协议位核对，保证只对 v4 背书）。
    pub(crate) fn observe_hello(&mut self, body: &Value) {
        let Some(object) = body.as_object() else {
            return;
        };
        if let Some(table) = object.get("methods") {
            let has = |name: &str| match table {
                Value::Array(items) => items.iter().any(|item| item.as_str() == Some(name)),
                Value::Object(map) => map.contains_key(name),
                _ => false,
            };
            self.method_send = has(super::command::AGENT_SEND_COMMAND_METHOD);
            self.method_resolve_interaction = has(super::command::RESOLVE_INTERACTION_TYPE);
            self.method_stop = has(super::command::STOP_TYPE);
            self.method_selection_get = has(MODEL_SELECTION_GET_VIEW_METHOD);
            self.method_selection_switch = has(super::selection::SWITCH_MODEL_CONFIG_TYPE);
            return;
        }
        if object.get("protocolVersion").and_then(Value::as_u64) == Some(V4_WIRE_PROTOCOL_VERSION) {
            self.method_send = true;
            self.method_resolve_interaction = true;
            self.method_stop = true;
            self.method_selection_get = true;
            self.method_selection_switch = true;
        }
    }

    /// conversation logical frame（`{topic,subscriptionId,fromSeq,toSeq,payload}`）：
    /// `payload.kind = snapshot`（整体）或 `deltas`（`state.updated` 键级整体替换）。
    pub(crate) fn observe_conversation_frame(&mut self, frame: &Value) {
        let Some(payload) = frame.get("payload") else {
            return;
        };
        match payload.get("kind").and_then(Value::as_str) {
            Some("snapshot") => {
                if let Some(snapshot) = payload.get("snapshot") {
                    self.observe_state_object(snapshot);
                }
            }
            Some("deltas") => {
                for delta in payload
                    .get("deltas")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if delta.get("op").and_then(Value::as_str) == Some("state.updated") {
                        if let Some(patch) = delta.get("patch") {
                            self.observe_state_object(patch);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// 单张 pending 卡：`has_user_input_pending` / `has_permission_pending` /
    /// `has_multi_select` / `has_free_text`。
    ///
    /// 接受两种形状：wire 卡（`kind` 顶层、形状在 `payload` 包装内）与平铺形
    /// （`kind`/`questions` 同层；`kind` 记 `user_input`，与 wire 记法 `userInput`
    /// 同义）。自由作答形状 = `payload.freeText: true`（与 `pending.rs` 的
    /// `free_text_allowed` 同源）**或**任一题 `isOther: true`。
    ///
    /// 入参泛型 [`std::borrow::Borrow`]：直接喂 `Value`（独立观察）或 `&Value`
    /// （帧遍历路径，零克隆）皆可。
    pub(crate) fn observe_pending<B: std::borrow::Borrow<Value>>(&mut self, card: B) {
        let card = card.borrow();
        let Some(object) = card.as_object() else {
            return;
        };
        let payload = object.get("payload").and_then(Value::as_object);
        let kind = object.get("kind").and_then(Value::as_str).or_else(|| {
            payload
                .and_then(|shape| shape.get("kind"))
                .and_then(Value::as_str)
        });
        let shape = payload.unwrap_or(object);
        match kind {
            Some("userInput") | Some("user_input") => {
                self.has_user_input_pending = true;
                for question in shape
                    .get("questions")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if question.get("multiSelect").and_then(Value::as_bool) == Some(true) {
                        self.has_multi_select = true;
                    }
                    if question.get("isOther").and_then(Value::as_bool) == Some(true) {
                        self.has_free_text = true;
                    }
                }
                if shape.get("freeText").and_then(Value::as_bool) == Some(true) {
                    self.has_free_text = true;
                }
            }
            Some("permission") => self.has_permission_pending = true,
            _ => {}
        }
    }

    /// `model-selection.getView` 组装载荷（`SelectionPort::load` 在
    /// [`parse_selection_view`] 成功后喂入）：`has_model_catalog` /
    /// `has_thought_levels`。
    ///
    /// 只认**当前会话 provider**（`config.provider` 由 load 从会话视图合成）的
    /// 目录条目；其他账号 provider 的模型/档位绝不背书（与解析层的过滤纪律一致）。
    pub(crate) fn observe_selection_catalog(&mut self, raw: &Value) {
        let Some(object) = raw.as_object() else {
            return;
        };
        let Some(provider) = object
            .get("config")
            .and_then(|config| config.get("provider"))
            .and_then(Value::as_str)
        else {
            return;
        };
        for entry in object
            .get("providers")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if entry.get("providerId").and_then(Value::as_str) != Some(provider) {
                continue;
            }
            let Some(models) = entry.get("models").and_then(Value::as_array) else {
                continue;
            };
            if !models.is_empty() {
                self.has_model_catalog = true;
            }
            for model in models {
                let levels = model
                    .get("config")
                    .and_then(|config| config.get("optionSpecs"))
                    .and_then(|specs| specs.get("reasoningLevel"))
                    .and_then(|spec| spec.get("values"))
                    .and_then(Value::as_array);
                if levels.is_some_and(|levels| !levels.is_empty()) {
                    self.has_thought_levels = true;
                }
            }
        }
    }

    /// snapshot / `state.updated` patch 的共用形状面（`control` /
    /// `pendingInteractions` / `config`）。
    fn observe_state_object(&mut self, object: &Value) {
        if let Some(control) = object.get("control") {
            self.has_active_work |= control
                .get("activeWorks")
                .and_then(Value::as_array)
                .is_some_and(|works| !works.is_empty());
        }
        for card in object
            .get("pendingInteractions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            self.observe_pending(card);
        }
        if let Some(config) = object.get("config") {
            self.has_thought_levels |= config
                .get("thoughtLevels")
                .and_then(Value::as_array)
                .is_some_and(|levels| !levels.is_empty());
        }
    }

    /// 全清（断连/换代 invalidate；语义 = 回到「什么都未见」）。
    pub(crate) fn invalidate(&mut self) {
        *self = Self::default();
    }
}

/// 本代际的 workspace listing 与任务索引缓存（一次枚举，多次绑定共享）。
///
/// controller tasks-index 按**桥的 workspace 作用域**下发（官方订阅带
/// `visibility:"foreground"`）。「单枚举桥返回全局任务索引」是**未验证假设**，
/// 因此这里为每个候选 workspace 各开一条枚举桥并合并行（同键去重）；即使官方
/// 实际返回全局列表，合并结果也只是重复行按 `workspacePath\0taskId` 折叠。
struct WorkspaceCache {
    generation: u64,
    entries: Vec<WorkspaceEntry>,
    active_key: Option<String>,
    /// workspaceKey → 枚举桥/订阅状态。
    enum_bridges: BTreeMap<String, EnumBridge>,
    rows: BTreeMap<(String, String), TaskRow>,
    /// 因 1024 行上限被拒的 controller 行数（证据计数）。
    rows_rejected: u64,
}

/// 单个 workspace 的枚举桥状态。
struct EnumBridge {
    bridge_id: String,
    workspace_path: String,
    controller_subscription: Option<String>,
    subscribed: bool,
    snapshot_seen: bool,
    /// 连续错过收帧窗（或订阅失败）的次数（快照到达后清零）。
    snapshot_misses: u32,
    /// 下一次允许重订阅的最早时刻（指数退避；防 tight loop）。
    next_subscribe_at: Option<Instant>,
}

impl WorkspaceCache {
    fn rows_ready(&self) -> bool {
        !self.enum_bridges.is_empty()
            && self
                .enum_bridges
                .values()
                .all(|bridge| bridge.snapshot_seen)
    }

    /// 枚举顺序：active workspace 优先，其余按 listing 顺序；上限
    /// [`MAX_ENUM_WORKSPACES`]（每 workspace 一条桥，防无界枚举）。
    fn candidate_workspaces(&self) -> Vec<(String, String, Option<String>)> {
        let mut candidates: Vec<(String, String, Option<String>)> = Vec::new();
        let mut push = |entry: &WorkspaceEntry| {
            if candidates.len() >= MAX_ENUM_WORKSPACES {
                return;
            }
            if candidates
                .iter()
                .any(|(key, _, _)| key == &entry.workspace_key)
            {
                return;
            }
            candidates.push((
                entry.workspace_key.clone(),
                entry.workspace_path.clone(),
                entry.workspace_identity.clone(),
            ));
        };
        if let Some(active) = &self.active_key {
            if let Some(entry) = self
                .entries
                .iter()
                .find(|entry| &entry.workspace_key == active)
                .or_else(|| {
                    self.entries
                        .iter()
                        .find(|entry| &entry.workspace_path == active)
                })
            {
                push(entry);
            }
        }
        for entry in &self.entries {
            push(entry);
        }
        candidates
    }
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
    /// 最近一次入站的逻辑消息号 `messageSeq`。同一逻辑消息的全部物理分片共享
    /// 该值（只有物理 `seq` 逐片递增），因此水位只能用于丢弃**严格更旧**的
    /// 消息；同值必须交给 [`FrameAssembler`] 按 `(messageSeq, fragmentIndex)`
    /// 去重/组装，否则首片之后的物理分片会被整条丢弃。
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
    /// 会话日志头指纹（T5 首次确认的 send 观察面）：快照 `logEpoch:seq` 的 id 形状
    /// 组合，只标识「日志头在哪」，**绝不**承载任何正文。快照整体替换、patch 前进
    /// `seq`、两者皆缺保持不变；无完整 (epoch, seq) 对 = `None`（无指纹证据）。
    log_fingerprint: Option<String>,
    staged: Vec<StagedFrame>,
    staged_bytes: usize,
    /// 暂存帧/在途分片的总字节预算（来自 [`BridgeLimits::staged_bytes`]）。
    staged_budget: usize,
    fragments: ConversationFragments,
    needs_resync: bool,
    /// 连续重同步次数（成功后清零；超过 [`MAX_RESYNCS`] 停在 ready=false）。
    resync_attempts: u32,
    last_resync_at: Option<Instant>,
    stats: SessionStats,
}

impl SessionEntry {
    fn new(
        target: BoundTarget,
        bridge_id: String,
        projection_bytes: usize,
        staged_budget: usize,
    ) -> Self {
        let projection = Projection::with_limit(target.clone(), projection_bytes);
        Self {
            target,
            bridge_id,
            projection,
            log_fingerprint: None,
            staged: Vec::new(),
            staged_bytes: 0,
            staged_budget,
            fragments: ConversationFragments::default(),
            needs_resync: false,
            resync_attempts: 0,
            last_resync_at: None,
            stats: SessionStats::default(),
        }
    }
}

/// ACK 前暂存的一条 logical frame。
struct StagedFrame {
    subscription_id: String,
    frame: Value,
}

/// 单会话的分片重组状态（第二层：应用级 logical frame 分片）。
#[derive(Default)]
struct ConversationFragments {
    assemblies: HashMap<String, FragmentAssembly>,
}

impl ConversationFragments {
    fn clear(&mut self) {
        self.assemblies.clear();
    }

    /// 在途分片总字节（用于会话级预算）。
    fn in_flight_bytes(&self) -> usize {
        self.assemblies
            .values()
            .map(|assembly| usize::try_from(assembly.logical_bytes).unwrap_or(usize::MAX))
            .fold(0usize, usize::saturating_add)
    }

    /// 接收一片 `kind:"fragment"` 的 wire candidate（TS `acceptFragment` 对位实现）。
    ///
    /// 键 = `topic\0subscriptionId\0logicalFrameId`；片数/总字节/CRC 冲突或片序号越界
    /// 一律 fail-closed；缺片由一个更高的 `logicalFrameOrdinal` 到来时判定为缺口。
    fn accept(
        &mut self,
        object: &Map<String, Value>,
        topic: &str,
        subscription: &str,
        budget: usize,
    ) -> FragmentOutcome {
        let Some(logical_frame_id) = object
            .get("logicalFrameId")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        else {
            return FragmentOutcome::Fault;
        };
        let Some(fragment_index) = object.get("fragmentIndex").and_then(Value::as_u64) else {
            return FragmentOutcome::Fault;
        };
        let Some(fragment_count) = object.get("fragmentCount").and_then(Value::as_u64) else {
            return FragmentOutcome::Fault;
        };
        let Some(logical_bytes) = object.get("logicalBytes").and_then(Value::as_u64) else {
            return FragmentOutcome::Fault;
        };
        let Some(checksum) = object
            .get("checksum")
            .and_then(|checksum| checksum.get("value"))
            .and_then(Value::as_str)
        else {
            return FragmentOutcome::Fault;
        };
        let Some(data_base64) = object.get("dataBase64").and_then(Value::as_str) else {
            return FragmentOutcome::Fault;
        };
        let ordinal = object
            .get("logicalFrameOrdinal")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if fragment_count == 0
            || fragment_count > MAX_LOGICAL_FRAGMENTS
            || fragment_index >= fragment_count
            || logical_bytes == 0
            || usize::try_from(logical_bytes).map_or(true, |bytes| bytes > budget)
        {
            return FragmentOutcome::Fault;
        }
        let Ok(chunk) = base64::engine::general_purpose::STANDARD.decode(data_base64) else {
            return FragmentOutcome::Fault;
        };
        if chunk.is_empty() {
            return FragmentOutcome::Fault;
        }
        let key = format!("{topic}\u{0}{subscription}\u{0}{logical_frame_id}");
        // 缺口判定：同一订阅下更新的 logical frame 已开始，而旧的仍未收齐 →
        // 旧帧必定缺片（wire 候选按 physical message 连续下发），不得无限等待。
        let stale: Vec<String> = self
            .assemblies
            .iter()
            .filter(|(known, assembly)| known.as_str() != key && assembly.ordinal < ordinal)
            .map(|(known, _)| known.clone())
            .collect();
        if !stale.is_empty() {
            for known in stale {
                self.assemblies.remove(&known);
            }
            return FragmentOutcome::Fault;
        }
        if !self.assemblies.contains_key(&key) {
            if self.assemblies.len() >= MAX_FRAGMENT_ASSEMBLIES
                || self
                    .in_flight_bytes()
                    .saturating_add(usize::try_from(logical_bytes).unwrap_or(usize::MAX))
                    > budget
            {
                return FragmentOutcome::Fault;
            }
            let chunks = (0..fragment_count).map(|_| None).collect();
            self.assemblies.insert(
                key.clone(),
                FragmentAssembly {
                    fragment_count,
                    logical_bytes,
                    checksum: checksum.to_owned(),
                    ordinal,
                    chunks,
                    received: 0,
                },
            );
        }
        let Some(assembly) = self.assemblies.get_mut(&key) else {
            return FragmentOutcome::Fault;
        };
        if assembly.fragment_count != fragment_count
            || assembly.logical_bytes != logical_bytes
            || assembly.checksum != checksum
            || assembly.ordinal != ordinal
        {
            self.assemblies.remove(&key);
            return FragmentOutcome::Fault;
        }
        let slot = usize::try_from(fragment_index).expect("fragment index fits usize");
        if assembly.chunks[slot].is_some() {
            // 重复片：静默忽略（内存状态不变）。
            return FragmentOutcome::Incomplete;
        }
        assembly.chunks[slot] = Some(chunk);
        assembly.received += 1;
        if assembly.received < usize::try_from(fragment_count).expect("fragment count fits") {
            return FragmentOutcome::Incomplete;
        }
        let Some(assembly) = self.assemblies.remove(&key) else {
            return FragmentOutcome::Fault;
        };
        let expected = usize::try_from(assembly.logical_bytes).expect("logical bytes fit");
        let mut joined = Vec::with_capacity(expected);
        for piece in assembly.chunks.into_iter().flatten() {
            joined.extend_from_slice(&piece);
        }
        if joined.len() != expected
            || crc32_hex(&joined) != assembly.checksum
            // UTF-8 校验：重组结果必须是合法 JSON 文本（官方按 UTF-8 字节拼接）。
            || std::str::from_utf8(&joined).is_err()
        {
            return FragmentOutcome::Fault;
        }
        let Ok(frame) = serde_json::from_slice::<Value>(&joined) else {
            return FragmentOutcome::Fault;
        };
        if frame.get("topic").and_then(Value::as_str) != Some(topic)
            || frame.get("subscriptionId").and_then(Value::as_str) != Some(subscription)
        {
            return FragmentOutcome::Fault;
        }
        FragmentOutcome::Complete(frame)
    }
}

/// 一组在途分片（键 = topic\0subscriptionId\0logicalFrameId）。
struct FragmentAssembly {
    fragment_count: u64,
    logical_bytes: u64,
    checksum: String,
    ordinal: u64,
    chunks: Vec<Option<Vec<u8>>>,
    received: usize,
}

/// 一片分片的处理结果。
enum FragmentOutcome {
    /// 尚未收齐（或重复片）：静默忽略。
    Incomplete,
    /// 重组完成：logical frame。
    Complete(Value),
    /// schema/CRC/片数不符：fail-closed（会话置不新鲜 + 只读重同步）。
    Fault,
}

/// 单会话证据计数（暂存放行 / 只读重订阅；值本身不含任何业务数据）。
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SessionStats {
    /// ACK 前暂存、激活后成功放行的帧数。
    pub staged_delivered: u64,
    /// 只读重订阅次数。
    pub resyncs: u64,
    /// 成功重组的 logical frame 分片数。
    pub fragments_reassembled: u64,
    /// 分片 schema/CRC/缺口故障次数（每次都要求重同步）。
    pub fragment_faults: u64,
    /// 因条数/字节预算被拒的帧数。
    pub frames_rejected: u64,
}

/// 桥管理器：同一 relay 上按 workspace/session 打开多个逻辑桥。
pub struct BridgeManager {
    shared: Arc<Shared>,
    /// 宿主 APP build 探针（默认读本机安装；测试注入固定值）。
    host_build: HostBuildProbe,
    /// 串行化绑定流程（bootstrap/枚举只做一次；并发 bind 不重复开桥）。
    setup: AsyncMutex<()>,
    /// 串行化只读重订阅（缺口后的 snapshot 刷新）。
    resync: AsyncMutex<()>,
}

/// 一帧 `data` 的处理结果：既有错误/ACK 语义 + 本帧造成的会话视图变化。
///
/// `changed` **无论** `result` 成败都会计算：坏 CRC/解码/序号错误同样会把投影标成
/// `ready=false`，上层（Hub 事件泵）必须据此广播降级，否则手机一直停在旧卡面，
/// 直到一次无关 GET 才发现失效。`result` 只表示本帧是否被吸收，不改变「已发生的
/// 状态变化必须被发布」这一纪律。顺序按 sessionId 稳定排序。
#[derive(Debug)]
pub struct DataOutcome {
    /// 本帧是否被吸收（`Err` = 未吸收；ACK/退订仍照常尽力发出）。
    pub result: ZResult<()>,
    /// 本帧造成的会话视图变化（无变化 = 空）。
    pub changed: Vec<SessionView>,
}

/// 管理器与 actor 共享的部分（状态锁 + sender 通道）。
struct Shared {
    state: Mutex<Inner>,
    relay: Arc<dyn RelaySink>,
    timing: BridgeTiming,
    limits: BridgeLimits,
    /// 有界命令队列：半开/拥塞时满队列立即以 `offline` 暴露，不无界增长。
    commands: mpsc::Sender<Command>,
}

impl BridgeManager {
    /// 生产构造（时序取官方口径）。
    ///
    /// **必须在 Tokio runtime 内构造**：内部会 `tokio::spawn` 出站 actor；
    /// 在 runtime 外调用会 panic（T7.1 Hub 在 Tauri runtime 中装配）。
    pub fn new(relay: Arc<dyn RelaySink>) -> Self {
        Self::with_timing(relay, BridgeTiming::default())
    }

    /// 注入时序（测试专用；生产不注册配置/IPC）。
    pub(crate) fn with_timing(relay: Arc<dyn RelaySink>, timing: BridgeTiming) -> Self {
        Self::with_timing_limits(relay, timing, BridgeLimits::default())
    }

    /// 注入时序与资源界限（测试专用；生产取默认界限）。
    pub(crate) fn with_timing_limits(
        relay: Arc<dyn RelaySink>,
        timing: BridgeTiming,
        limits: BridgeLimits,
    ) -> Self {
        let (commands, receiver) = mpsc::channel(limits.command_queue.max(1));
        let shared = Arc::new(Shared {
            state: Mutex::new(Inner::default()),
            relay,
            timing,
            limits,
            commands,
        });
        tokio::spawn(run_actor(shared.clone(), receiver));
        Self {
            shared,
            host_build: super::host::default_probe(),
            setup: AsyncMutex::new(()),
            resync: AsyncMutex::new(()),
        }
    }

    /// 注入宿主 build 探针（测试专用；生产固定读本机安装）。
    #[cfg(test)]
    pub(crate) fn with_host_build(mut self, host_build: HostBuildProbe) -> Self {
        self.host_build = host_build;
        self
    }

    /// 本轮运行真实观测到的宿主证据（未握手/未应用 schema = 未知）。
    ///
    /// `app_build` 每次调用现读（锁外读 plist，绝不在状态锁内做文件 IO）。
    pub fn host_evidence(&self) -> HostEvidence {
        let app_build = (self.host_build)();
        let inner = lock(&self.shared.state);
        HostEvidence {
            app_build,
            protocol_version: inner.protocol_version,
            schema_known: inner.schema_known,
            hello_known: inner.hello_known,
            interface: inner.interface,
        }
    }

    /// relay 事件泵入口：一条已配对的 `data` payload。
    ///
    /// **兼容包装**：[`Self::on_data_outcome`] 的 `result` + `changed` 合并成旧签名
    /// （`Ok(changed)` / `Err(error)`）。既有调用方/测试可继续只关心错误；需要「错误
    /// 帧也要发布降级视图」的调用方（Hub 事件泵）必须走 `on_data_outcome`。
    pub async fn on_data(&self, payload: Value) -> ZResult<Vec<SessionView>> {
        let DataOutcome { result, changed } = self.on_data_outcome(payload).await;
        result.map(|()| changed)
    }

    /// relay 事件泵入口（完整结果）：一条已配对的 `data` payload。
    ///
    /// 只做解析/路由，**不 await 网络**（ACK 走 actor 队列），因此事件泵不会被慢路径拖住；
    /// 缺口/协议不符/坏 CRC/未知 schema 只置 `ready=false` 并标记重同步，读面下次访问时
    /// 取新快照。
    ///
    /// **无论 `result` 成败**都在同一把状态锁内做前后快照 diff（按 sessionId 稳定排序）：
    /// 错误帧同样可能已经改写了投影（例如把会话标成不新鲜），这些变化必须原样上报，
    /// 否则上层无法广播降级。ACK/退订仍照常尽力发出。
    pub async fn on_data_outcome(&self, payload: Value) -> DataOutcome {
        let (result, acks, releases, changed) = {
            let mut inner = lock(&self.shared.state);
            // 前/后快照与 `handle_data` 在同一把状态锁内：视图差异就是「本帧」
            // 的效果，不会混入锁外其它调用者的读写。
            let before: HashMap<String, SessionView> = inner
                .sessions
                .iter()
                .map(|(session_id, entry)| (session_id.clone(), entry.projection.view()))
                .collect();
            let (result, acks, releases) = inner.handle_data(&payload);
            // 成败都做 diff：错误帧的降级/恢复视图同样是「本帧真实发生的状态变化」。
            let mut changed = Vec::new();
            for (session_id, entry) in inner.sessions.iter() {
                let after = entry.projection.view();
                if before.get(session_id) != Some(&after) {
                    changed.push(after);
                }
            }
            // 稳定顺序（HashMap 迭代序随机）：同一批变化的事件顺序可复现。
            changed.sort_by(|left, right| left.session_id.cmp(&right.session_id));
            (result, acks, releases, changed)
        };
        for ack in acks {
            let _ = self.shared.commands.try_send(Command::SendFrame {
                payload: ack,
                reply: None,
            });
        }
        for release in releases {
            let _ = self.shared.commands.try_send(release);
        }
        DataOutcome { result, changed }
    }

    /// 连接代际变化：丢弃旧代际的全部桥/投影/临时绑定。
    pub async fn invalidate(&self, generation: u64) {
        let releases = {
            let mut inner = lock(&self.shared.state);
            if generation < inner.generation {
                return;
            }
            // 尽力退订（读面生命周期帧，零写）：会话订阅 + controller 订阅。
            let session_subscriptions: Vec<(String, String)> = inner
                .sessions
                .values()
                .filter_map(|entry| {
                    entry
                        .projection
                        .subscription_id()
                        .map(|subscription| (entry.bridge_id.clone(), subscription.to_owned()))
                })
                .collect();
            let controller_subscriptions: Vec<(String, String)> = inner
                .workspaces
                .as_ref()
                .map(|cache| {
                    cache
                        .enum_bridges
                        .values()
                        .filter_map(|bridge| {
                            bridge
                                .controller_subscription
                                .clone()
                                .map(|subscription| (bridge.bridge_id.clone(), subscription))
                        })
                        .collect()
                })
                .unwrap_or_default();
            let mut releases = Vec::new();
            for (bridge_id, subscription) in session_subscriptions {
                if let Some(command) = inner.release_command(
                    &bridge_id,
                    AGENT_CHANNEL,
                    AGENT_UNSUBSCRIBE_CONVERSATION_METHOD,
                    &subscription,
                ) {
                    releases.push(command);
                }
            }
            for (bridge_id, subscription) in controller_subscriptions {
                if let Some(command) = inner.release_command(
                    &bridge_id,
                    WINDOW_CONTROLLER_CHANNEL,
                    CONTROLLER_UNSUBSCRIBE_METHOD,
                    &subscription,
                ) {
                    releases.push(command);
                }
            }
            inner.generation = generation;
            inner.bridges.clear();
            inner.sessions.clear();
            inner.workspaces = None;
            inner.app_waiters.clear();
            // 证据只属本代际：换代后握手/schema 事实全部作废（未重新取证 = 未知），
            // 接口形状证据一并全清（未重新观察 = 全未见）。
            inner.hello_known = false;
            inner.schema_known = false;
            inner.protocol_version = None;
            inner.interface.invalidate();
            releases
        };
        for release in releases {
            let _ = self.shared.commands.try_send(release);
        }
        let _ = self.shared.commands.try_send(Command::Reset);
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
                .filter(|entry| entry.target.bridge_generation() == generation)
                .map(|entry| entry.target.clone())
        } {
            return Ok(target);
        }
        if lock(&self.shared.state).sessions.len() >= self.shared.limits.sessions {
            // 会话数有界：必须由上层释放（换代）或显式回收，绝不无界增长。
            return Err(ZcodeError::of(codes::PAYLOAD_TOO_LARGE));
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
            bridge_generation: None,
        };
        if lock(&self.shared.state).generation != generation {
            return Err(stale());
        }
        self.shared.open_session(&target).await?;
        Ok(target)
    }

    /// 读取绑定会话的投影（必要时先按只读方式重取快照）。
    pub async fn session(&self, target: &BoundTarget) -> ZResult<SessionView> {
        self.session_within(target, None).await
    }

    /// 有界只读视图：`budget = Some(t)` 时整次读（含内部重订阅的每个 RPC）不超过
    /// `t`，到期以 `not_ready` 收口（不取消 future、不留下未激活的重订阅）。
    async fn session_within(
        &self,
        target: &BoundTarget,
        budget: Option<Duration>,
    ) -> ZResult<SessionView> {
        let deadline = budget.map(|budget| {
            Instant::now()
                .checked_add(budget)
                .unwrap_or_else(Instant::now)
        });
        let needs_resync = {
            let inner = lock(&self.shared.state);
            if inner.generation != target.bridge_generation() {
                return Err(stale());
            }
            let Some(entry) = inner.sessions.get(&target.session_id) else {
                return Err(ZcodeError::of(codes::NO_SESSION));
            };
            if entry.target.bridge_generation() != target.bridge_generation() {
                return Err(stale());
            }
            entry.needs_resync
        };
        if needs_resync {
            self.shared
                .resync_until(target, &self.resync, deadline)
                .await?;
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
        self.call_with_timeout(target, channel, method, args, self.shared.timing.rpc)
            .await
    }

    /// 带显式应答窗的 RPC（T3 写命令：ACK 窗由调用方给出，如 `command::ACK_TIMEOUT` 20s）。
    ///
    /// `timeout` 直接作为本调用 RPC 待答条目的期限：超窗时**本层**清账
    /// （`unregister_rpc`）并返回 `not_ready`——因此调用方不得再外套超时，
    /// 否则取消 future 会绕过清理，在 pending 里留下永不回收的 waiter。
    pub async fn call_with_timeout(
        &self,
        target: &BoundTarget,
        channel: &str,
        method: &str,
        args: Vec<Value>,
        timeout: Duration,
    ) -> ZResult<Value> {
        let (bridge_id, needs_resync) = {
            let inner = lock(&self.shared.state);
            if inner.generation != target.bridge_generation() {
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
        // 就绪闸随命令一起交给 actor，在真正写帧前复核：并发缺口 →
        // 「检查通过、写前已失效」的窗口必须零写（quality I-4）。
        let guard = RpcGuard {
            session_id: target.session_id.clone(),
            generation: target.bridge_generation(),
        };
        let message = self
            .shared
            .rpc_call(&bridge_id, channel, method, args, timeout, Some(guard))
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

    /// 测试观测：在途 RPC waiter 数（超时清账/迟到应答用例）。
    #[cfg(test)]
    pub(crate) fn pending_count(&self) -> usize {
        lock(&self.shared.state)
            .bridges
            .values()
            .map(|bridge| bridge.pending.len())
            .sum()
    }

    /// 测试观测：已收集的 controller 任务行数（1024 上限断言）。
    #[cfg(test)]
    pub(crate) fn task_rows(&self) -> usize {
        lock(&self.shared.state)
            .workspaces
            .as_ref()
            .map(|cache| cache.rows.len())
            .unwrap_or(0)
    }

    /// 测试观测：因行数上限被拒的 controller 行数。
    #[cfg(test)]
    pub(crate) fn task_rows_rejected(&self) -> u64 {
        lock(&self.shared.state)
            .workspaces
            .as_ref()
            .map(|cache| cache.rows_rejected)
            .unwrap_or(0)
    }
}

/// 官方 snooze 命令的**信封 type**（`command.ts:250-253`）——RPC method 仍是
/// [`AGENT_SEND_COMMAND_METHOD`](super::command::AGENT_SEND_COMMAND_METHOD)，
/// 命令种类只由信封 `type` 区分（与 sendText 同构）。
pub const SNOOZE_INTERACTION_TYPE: &str = "snoozeInteractionAutoResolution";

fn epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// snooze 的 ACK 分类（官方 `CommandAck.status`）。
///
/// `accepted`/`duplicate`/`noop` 都是「已暂停」的成功口径：官方语义是「首次有效
/// 操作永久暂停本次自动结束；重复/迟到幂等 noop」（探针 `interactionBridge.ts:655-656`）
/// ——卡若因先前操作已 snoozed，前置条件其实已满足，不得把 noop 折成硬失败（否则
/// 每次作答都会二次 snooze → noop → 零 resolve，卡片永久不可答）。`rejected` 是
/// 宿主显式拒绝（硬失败，零 resolve）；无法解释的应答按保守 `unknown`。
fn classify_snooze_ack(ack: &Value) -> ZResult<()> {
    match ack.get("status").and_then(Value::as_str) {
        Some("accepted") | Some("duplicate") | Some("noop") => Ok(()),
        Some("rejected") => Err(ZcodeError::of(codes::INVALID_ACTION)),
        _ => Err(ZcodeError::unknown()),
    }
}

impl BridgeManager {
    /// 在状态锁内读**精确目标**的宿主 pending 观察：该会话册的观察代数 + 该 id
    /// 是否仍在册。目标换代/未绑定 → `None`（绝不拿兄弟会话的证据顶替）。
    fn host_pending_observation(
        &self,
        target: &BoundTarget,
        interaction_id: &str,
    ) -> Option<(u64, bool)> {
        let inner = lock(&self.shared.state);
        if inner.generation != target.bridge_generation() {
            return None;
        }
        let entry = inner.sessions.get(&target.session_id)?;
        if !entry.target.same_binding(target) {
            return None;
        }
        let book = entry.projection.pending_book();
        Some((
            book.host_pending_revision(),
            book.host_has_pending(interaction_id),
        ))
    }

    /// T5 确认观察：精确会话的 revision / 当前选择 provider / 日志头指纹。
    ///
    /// 状态锁内**纯读**（零网络、零重订阅、零出站帧）：动作提交路径的 baseline 与
    /// 事件泵的确认观察共用同一来源。会话不存在 → `None`（绝不拿兄弟会话顶替）。
    pub(crate) fn confirmation_observation(
        &self,
        session_id: &str,
    ) -> Option<ConfirmationObservation> {
        let inner = lock(&self.shared.state);
        let entry = inner.sessions.get(session_id)?;
        let view = entry.projection.view();
        Some(ConfirmationObservation {
            revision: view.revision,
            provider: view.selection.as_ref().map(|s| s.provider.clone()),
            log_fingerprint: entry.log_fingerprint.clone(),
        })
    }

    /// T5 确认观察：精确会话的日志头指纹（事件泵观察面；缺席 = 无证据）。
    pub(crate) fn log_fingerprint(&self, session_id: &str) -> Option<String> {
        lock(&self.shared.state)
            .sessions
            .get(session_id)?
            .log_fingerprint
            .clone()
    }
}

/// 按**宿主 pending 卡**的题型推导本次应答所需的全部能力名。
///
/// 稳定顺序固定为 `single_select` → `multi_select` → `free_text`（去重）：
/// 单选/多选按每题的 `multi_select` 归类（客户端选了几个值与此无关），
/// `free_text_allowed` 再叠加自由文本。只有宿主明确建模的问答/审批卡能给出能力名；
/// 其他子类（workspace hook 复核等）一律 `needs_evidence`，绝不按客户端自报猜测。
fn required_capabilities_for_card(card: &PendingView) -> ZResult<Vec<&'static str>> {
    match card.kind {
        PendingKind::Permission => Ok(vec!["approval_reply"]),
        PendingKind::UserInput => {
            // 官方 schema 要求问答至少一题；缺题形状无法推导所需能力 → 保守拒绝。
            if card.questions.is_empty() {
                return Err(ZcodeError::of(codes::NEEDS_EVIDENCE));
            }
            let mut single = false;
            let mut multi = false;
            let mut free_text = false;
            for question in &card.questions {
                if question.multi_select {
                    multi = true;
                } else {
                    single = true;
                }
                if question.free_text_allowed {
                    free_text = true;
                }
            }
            let mut required = Vec::with_capacity(3);
            if single {
                required.push("single_select");
            }
            if multi {
                required.push("multi_select");
            }
            if free_text {
                required.push("free_text");
            }
            Ok(required)
        }
        PendingKind::WorkspaceHookReview => Err(ZcodeError::of(codes::NEEDS_EVIDENCE)),
    }
}

impl PendingPort for BridgeManager {
    /// 精确目标授权：同会话 + 同代际 + 同 workspace 绑定 + 卡仍在册 + 未消费/在途。
    ///
    /// 只置本地 in-flight（重复提交闸），**不**触碰宿主集合；投影不新鲜期间零授权
    /// （`not_ready`，卡片可能已被宿主撤掉/改动）。
    fn authorize_pending<'a>(
        &'a self,
        target: &'a BoundTarget,
        interaction_id: &'a str,
    ) -> futures::future::BoxFuture<'a, ZResult<PendingView>> {
        Box::pin(async move {
            let mut inner = lock(&self.shared.state);
            if inner.generation != target.bridge_generation() {
                return Err(stale());
            }
            let Some(entry) = inner.sessions.get_mut(&target.session_id) else {
                return Err(ZcodeError::of(codes::NO_SESSION));
            };
            // 精确目标：workspace 归属/代际都必须与绑定产物逐字段相等
            // （手机自报的路径不得成为锚）。
            if !entry.target.same_binding(target) {
                return Err(stale());
            }
            if !entry.projection.is_ready() {
                return Err(ZcodeError::of(codes::NOT_READY));
            }
            entry
                .projection
                .pending_book_mut()
                .authorize(target, interaction_id)
        })
    }

    /// 只读推导**精确目标**上宿主卡要求的全部能力（不置在途、不消费、不发帧）。
    ///
    /// 目标校验与 [`Self::authorize_pending`] 同序同码：错代际/错绑定 → `stale`、
    /// 未绑定 → `no_session`、投影不新鲜 → `not_ready`、卡不在册 → `stale_pending`。
    /// 差别只在**不**改写任何状态：本方法可在能力闸判定阶段反复调用。
    fn required_capabilities<'a>(
        &'a self,
        target: &'a BoundTarget,
        interaction_id: &'a str,
    ) -> futures::future::BoxFuture<'a, ZResult<Vec<&'static str>>> {
        Box::pin(async move {
            let inner = lock(&self.shared.state);
            if inner.generation != target.bridge_generation() {
                return Err(stale());
            }
            let Some(entry) = inner.sessions.get(&target.session_id) else {
                return Err(ZcodeError::of(codes::NO_SESSION));
            };
            if !entry.target.same_binding(target) {
                return Err(stale());
            }
            if !entry.projection.is_ready() {
                return Err(ZcodeError::of(codes::NOT_READY));
            }
            let card = entry
                .projection
                .pending_book()
                .pending(interaction_id)
                .ok_or_else(|| ZcodeError::of(codes::STALE_PENDING))?;
            required_capabilities_for_card(card)
        })
    }

    /// 暂停自动结束计时：经既有写命令路径**恰一帧**官方
    /// `snoozeInteractionAutoResolution{interactionId}`；`timeout` 原样交给桥的
    /// RPC 待答条目（超窗由桥清账并返回 `not_ready` → 本层按保守 `unknown`）。
    fn snooze<'a>(
        &'a self,
        target: &'a BoundTarget,
        interaction_id: &'a str,
        client_id: &'a str,
        timeout: Duration,
    ) -> futures::future::BoxFuture<'a, ZResult<()>> {
        Box::pin(async move {
            if interaction_id.trim().is_empty() || interaction_id.trim() != interaction_id {
                return Err(ZcodeError::of(codes::INVALID_ACTION));
            }
            if client_id.trim().is_empty() {
                return Err(ZcodeError::of(codes::INVALID_ACTION));
            }
            let envelope = serde_json::json!({
                "commandId": super::command::new_command_id(),
                // clientId 与 resolve/send 同源：只来自可信调用方上下文（配对门/本机 DB）。
                "clientId": client_id,
                "sessionId": target.session_id,
                "type": SNOOZE_INTERACTION_TYPE,
                "payload": { "interactionId": interaction_id },
                "issuedAt": epoch_ms(),
            });
            let ack = CommandPort::call(
                self,
                target,
                AGENT_CHANNEL,
                super::command::AGENT_SEND_COMMAND_METHOD,
                vec![envelope],
                timeout,
            )
            .await
            .map_err(|error| {
                // 超窗与「就绪闸拒写」在桥层同为 not_ready：不可区分 → 保守 unknown。
                if error.is(codes::NOT_READY) {
                    ZcodeError::unknown()
                } else {
                    error
                }
            })?;
            classify_snooze_ack(&ack)
        })
    }

    fn host_pending_revision(&self, target: &BoundTarget) -> u64 {
        self.host_pending_observation(target, "")
            .map(|(revision, _)| revision)
            .unwrap_or(0)
    }

    fn host_has_pending(&self, target: &BoundTarget, interaction_id: &str) -> bool {
        self.host_pending_observation(target, interaction_id)
            .map(|(_, present)| present)
            .unwrap_or(false)
    }

    /// 有界轮询**精确目标**的宿主终态：代数前进**且**该 id 不在宿主集合 → `true`；
    /// 超时或等待期间目标换代/绑定消失 → `false`。
    ///
    /// 每次判定在状态锁内完成、锁外 `sleep`（≤ [`PENDING_CLEARED_POLL`]），绝不忙等、
    /// 绝不持锁 await；本地消费/授权不推进代数，只有目标自己的整体替换才算数。
    fn wait_host_cleared<'a>(
        &'a self,
        target: &'a BoundTarget,
        interaction_id: &'a str,
        baseline_revision: u64,
        timeout: Duration,
    ) -> futures::future::BoxFuture<'a, bool> {
        Box::pin(async move {
            let deadline = Instant::now()
                .checked_add(timeout)
                .unwrap_or_else(Instant::now);
            loop {
                match self.host_pending_observation(target, interaction_id) {
                    Some((revision, present)) => {
                        if revision > baseline_revision && !present {
                            return true;
                        }
                    }
                    // 目标换代/绑定消失：旧基线不再可比，绝不拿别的会话证据确认。
                    None => return false,
                }
                let now = Instant::now();
                if now >= deadline {
                    return false;
                }
                let remaining = deadline.saturating_duration_since(now);
                tokio::time::sleep(super::types::PENDING_CLEARED_POLL.min(remaining)).await;
            }
        })
    }

    /// 本地消费：只改**精确绑定会话**的册（错会话/错代际/卡不在册 → 零副作用）。
    fn mark_resolved_local(&self, target: &BoundTarget, interaction_id: &str) {
        self.with_exact_book(target, |book| book.mark_resolved_local(interaction_id));
    }

    /// 确证零写后释放本地在途（写后未知不调用）。
    fn clear_local_inflight(&self, target: &BoundTarget, interaction_id: &str) {
        self.with_exact_book(target, |book| book.clear_local_inflight(interaction_id));
    }
}

impl StopPort for BridgeManager {
    /// 精确目标（会话 + 代际 + workspace 绑定）的只读视图；缺口时按只读方式重取快照。
    ///
    /// 任一不匹配都拒绝，绝不退化为「按 sessionId 找一张能读的投影」——兄弟会话的
    /// control 不得成为 stop 守卫来源。`timeout` 是整次读的硬预算：内部重订阅的
    /// 每个 RPC 只拿剩余时间，到期以 `not_ready` 收口（不取消、不留下未激活订阅）。
    fn session<'a>(
        &'a self,
        target: &'a BoundTarget,
        timeout: Duration,
    ) -> futures::future::BoxFuture<'a, ZResult<SessionView>> {
        Box::pin(async move {
            {
                let inner = lock(&self.shared.state);
                if inner.generation != target.bridge_generation() {
                    return Err(stale());
                }
                let Some(entry) = inner.sessions.get(&target.session_id) else {
                    return Err(ZcodeError::of(codes::NO_SESSION));
                };
                if !entry.target.same_binding(target) {
                    return Err(stale());
                }
            }
            // 复用既有只读读面：必要时重订阅取新快照（预算内）；视图仍由精确会话
            // 投影产出。
            BridgeManager::session_within(self, target, Some(timeout)).await
        })
    }

    /// 有界轮询精确目标的停止终态：revision 前进**且**目标 execution 缺席才 `true`；
    /// 其余情形（含 control 未知、投影不新鲜、目标换代/绑定消失）一律轮询到期限后
    /// `false`——绝不拿兄弟会话/旧代际的证据确认，也不把第一个「无证据」当结论
    /// （重同步/缺口期间恢复的已确认 control 仍可在窗前完成判定）。锁内判定、锁外
    /// sleep，绝不持锁 await。
    fn wait_stop_cleared<'a>(
        &'a self,
        target: &'a BoundTarget,
        execution_id: &'a str,
        baseline_revision: u64,
        timeout: Duration,
    ) -> futures::future::BoxFuture<'a, bool> {
        Box::pin(async move {
            let deadline = Instant::now()
                .checked_add(timeout)
                .unwrap_or_else(Instant::now);
            loop {
                if let Some((revision, present)) = self.stop_observation(target, execution_id) {
                    if revision > baseline_revision && !present {
                        return true;
                    }
                }
                let now = Instant::now();
                if now >= deadline {
                    return false;
                }
                let remaining = deadline.saturating_duration_since(now);
                tokio::time::sleep(super::types::STOP_CLEARED_POLL.min(remaining)).await;
            }
        })
    }
}

/// 官方模型目录 channel（`channels.ts:133`；探针 `selectionBridge.ts`）。
pub const MODEL_SELECTION_CHANNEL: &str = "model-selection";
/// 官方模型目录方法（无参 `getView()`）。
pub const MODEL_SELECTION_GET_VIEW_METHOD: &str = "getView";

impl SelectionPort for BridgeManager {
    /// 精确目标的只读视图 + `model-selection.getView()` 目录（只保留当前会话 provider）。
    ///
    /// 顺序：绑定/代际复核（零副作用）→ 有界只读投影（必要时重订阅取新快照）→
    /// `getView`（显式 `timing.rpc` 预算）→ [`parse_selection_view`] 组装。任一失败按
    /// 冻结码返回；**绝不**返回其他账号的 provider 目录，也绝不写任何命令帧。
    fn load<'a>(
        &'a self,
        target: &'a BoundTarget,
    ) -> futures::future::BoxFuture<'a, ZResult<(SessionView, SelectionView)>> {
        Box::pin(async move {
            {
                let inner = lock(&self.shared.state);
                if inner.generation != target.bridge_generation() {
                    return Err(stale());
                }
                let Some(entry) = inner.sessions.get(&target.session_id) else {
                    return Err(ZcodeError::of(codes::NO_SESSION));
                };
                if !entry.target.same_binding(target) {
                    return Err(stale());
                }
            }
            let budget = self.shared.timing.rpc;
            let view = self.session_within(target, Some(budget)).await?;
            if view.session_id != target.session_id {
                return Err(ZcodeError::of(codes::NO_SESSION));
            }
            if view.generation != target.bridge_generation() {
                return Err(stale());
            }
            if !view.ready {
                return Err(ZcodeError::of(codes::NOT_READY));
            }
            // 当前选择三项只来自精确目标的快照 config（绝不取客户端自报/目录显示名）。
            // 快照把官方扁平 `config.thoughtLevels` 只挂在当前模型名下（projection
            // `parse_selection`）：这里合成 `config.thoughtLevels`，供解析层在 getView
            // 未宣告当前模型档位时回退；其他模型拿不到这份档位。
            let (provider, model, thought, thought_levels) = {
                let current = view
                    .selection
                    .as_ref()
                    .ok_or_else(|| ZcodeError::of(codes::NOT_READY))?;
                let levels = current
                    .models
                    .iter()
                    .find(|choice| choice.model_id == current.model)
                    .map(|choice| choice.thought_levels.clone())
                    .unwrap_or_default();
                (
                    current.provider.clone(),
                    current.model.clone(),
                    current.thought.clone(),
                    levels,
                )
            };
            let config = serde_json::json!({
                "provider": provider,
                "model": model,
                "thought": thought,
                "thoughtLevels": thought_levels,
            });
            // 官方 getView 无参（探针 `bridgeRequest(..., {channel:"model-selection",
            // method:"getView"})`）；应答可能带 data/result 包装，解析层同规则解包。
            let body = self
                .call_with_timeout(
                    target,
                    MODEL_SELECTION_CHANNEL,
                    MODEL_SELECTION_GET_VIEW_METHOD,
                    Vec::new(),
                    budget,
                )
                .await?;
            let mut raw = if body.is_object() {
                body
            } else {
                serde_json::json!({})
            };
            if let Some(object) = raw.as_object_mut() {
                object.insert("config".to_owned(), config);
            }
            let selection = parse_selection_view(&raw)?;
            // T3：目录解析成功即采集目录/档位形状（只认当前 provider；失败不观察）。
            {
                let mut inner = lock(&self.shared.state);
                inner.interface.observe_selection_catalog(&raw);
            }
            Ok((view, selection))
        })
    }

    /// 有界轮询**新鲜**选择：只有投影 `ready`、revision 前进且 provider/model/thought
    /// 精确相等才 `true`；超时、目标换代/绑定不符、投影缺口（not ready）→ `false`
    /// （not ready 期间即便旧值恰等于目标也不是新鲜证据）。只读本地投影（不重复打
    /// 目录 RPC），每次读只拿剩余期限作预算；锁内判定、锁外 sleep。
    fn wait_selection<'a>(
        &'a self,
        target: &'a BoundTarget,
        provider: &'a str,
        model: &'a str,
        thought: &'a str,
        baseline_revision: u64,
        timeout: Duration,
    ) -> futures::future::BoxFuture<'a, bool> {
        Box::pin(async move {
            if !self.exact_session_bound(target) {
                return false;
            }
            let deadline = Instant::now()
                .checked_add(timeout)
                .unwrap_or_else(Instant::now);
            loop {
                // 绑定可能在等待期间被同代际改写：每轮都按精确目标复核。
                if !self.exact_session_bound(target) {
                    return false;
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return false;
                }
                match self.session_within(target, Some(remaining)).await {
                    Ok(view) => {
                        let fresh = view.ready
                            && view.session_id == target.session_id
                            && view.generation == target.bridge_generation()
                            && view
                                .revision
                                .is_some_and(|revision| revision > baseline_revision);
                        if fresh {
                            if let Some(current) = view.selection.as_ref() {
                                if current.provider == provider
                                    && current.model == model
                                    && current.thought.as_deref() == Some(thought)
                                {
                                    return true;
                                }
                            }
                        }
                    }
                    // 目标已换代/绑定消失：旧基线不可比，立即收口（绝不拿兄弟会话确认）。
                    Err(error)
                        if error.is(codes::NO_SESSION) || error.is(codes::STALE_GENERATION) =>
                    {
                        return false;
                    }
                    // 瞬时不可读（缺口/重订阅窗口）：不是结论，继续轮询到期限。
                    Err(_) => {}
                }
                let now = Instant::now();
                if now >= deadline {
                    return false;
                }
                let remaining = deadline.saturating_duration_since(now);
                tokio::time::sleep(super::types::SELECTION_POLL.min(remaining)).await;
            }
        })
    }
}

impl BridgeManager {
    /// 精确绑定复核（锁内、零 IO）：代际一致且同 session 的绑定完全相等。
    ///
    /// `session_within` 只校验代际（同代际改写绑定在正常流程不可达），等待类端口
    /// 需要更严的「同 workspace/identity 绑定」口径，避免拿兄弟投影确认。
    fn exact_session_bound(&self, target: &BoundTarget) -> bool {
        let inner = lock(&self.shared.state);
        if inner.generation != target.bridge_generation() {
            return false;
        }
        inner
            .sessions
            .get(&target.session_id)
            .is_some_and(|entry| entry.target.same_binding(target))
    }

    /// 状态锁内读**精确目标**的（投影 revision，目标 execution 是否仍在 control 观测里）。
    ///
    /// 目标换代/绑定不符/投影不新鲜/control 未知/revision 未知 → `None`（无证据，
    /// 绝不把兄弟会话或旧代际的 control 当成本目标的停止终态）。
    fn stop_observation(&self, target: &BoundTarget, execution_id: &str) -> Option<(u64, bool)> {
        let inner = lock(&self.shared.state);
        if inner.generation != target.bridge_generation() {
            return None;
        }
        let entry = inner.sessions.get(&target.session_id)?;
        if !entry.target.same_binding(target) {
            return None;
        }
        let view = entry.projection.view();
        if !view.ready {
            return None;
        }
        let revision = view.revision?;
        let control = view.control.as_ref()?;
        let present = control
            .active_works
            .iter()
            .any(|work| work.foreground_execution_id.as_deref() == Some(execution_id));
        Some((revision, present))
    }
}

impl BridgeManager {
    /// 在状态锁内对**精确目标**（同会话 + 同代际 + 同 workspace 绑定）的 pending
    /// 册做一次本地状态变更；任一不匹配 → 不做任何事。
    fn with_exact_book(
        &self,
        target: &BoundTarget,
        f: impl FnOnce(&mut super::pending::PendingBook),
    ) {
        let mut inner = lock(&self.shared.state);
        if inner.generation != target.bridge_generation() {
            return;
        }
        let Some(entry) = inner.sessions.get_mut(&target.session_id) else {
            return;
        };
        if !entry.target.same_binding(target) {
            return;
        }
        f(entry.projection.pending_book_mut());
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
            .await
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
                enum_bridges: BTreeMap::new(),
                rows: BTreeMap::new(),
                rows_rejected: 0,
            });
        }
        Ok(entries)
    }

    /// 主动任务索引（WindowController 订阅）枚举：按候选 workspace 各开一条枚举桥
    /// 并合并行（同键去重）。「单枚举桥返回全局任务索引」是未验证假设（spec I2），
    /// 因此不能只枚举 active workspace；fake 也按桥的 workspace 作用域下发。
    /// 一次枚举多会话绑定共享；缺口后由 [`Self::mark_bridge_unready`] 失效并在此重订阅。
    ///
    /// 收帧窗错过（订阅 ACK 到了但快照没来）不闩死：窗口结束时撤销 `subscribed`
    /// 并记一次 miss，下一次 `load_tasks` 在指数退避后重新 `subscribeControllerV4`；
    /// miss 超过 [`MAX_CONTROLLER_MISSES`] 即停用该桥索引（有界重试）。
    async fn load_tasks(&self, generation: u64) -> ZResult<Vec<TaskRow>> {
        let candidates = {
            let inner = lock(&self.state);
            let Some(cache) = &inner.workspaces else {
                return Err(ZcodeError::of(codes::NOT_READY));
            };
            if cache.generation != generation {
                return Err(stale());
            }
            if cache.rows_ready() {
                return Ok(cache.rows.values().cloned().collect());
            }
            cache.candidate_workspaces()
        };

        for (workspace_key, workspace_path, _identity) in &candidates {
            let existing = {
                let inner = lock(&self.state);
                let Some(cache) = &inner.workspaces else {
                    return Err(ZcodeError::of(codes::NOT_READY));
                };
                cache
                    .enum_bridges
                    .get(workspace_key)
                    .filter(|bridge| inner.bridges.contains_key(&bridge.bridge_id))
                    .map(|bridge| {
                        (
                            bridge.bridge_id.clone(),
                            bridge.subscribed,
                            bridge.snapshot_misses,
                            bridge.next_subscribe_at,
                        )
                    })
            };
            let bridge_id = match existing {
                Some((bridge_id, subscribed, misses, next_subscribe_at)) => {
                    if subscribed {
                        // 已有订阅在等首个快照（当前收帧窗内）→ 不重复订阅。
                        continue;
                    }
                    if misses >= MAX_CONTROLLER_MISSES {
                        // 有界重试耗尽：该 workspace 的索引停用（换代/缺口重置）。
                        continue;
                    }
                    if next_subscribe_at.is_some_and(|at| Instant::now() < at) {
                        // 退避窗内：本轮不重订阅（防 tight loop）。
                        continue;
                    }
                    bridge_id
                }
                None => {
                    let bridge_id = self
                        .open_bridge(generation, workspace_key, None, false)
                        .await?;
                    let mut inner = lock(&self.state);
                    let Some(cache) = inner.workspaces.as_mut() else {
                        return Err(ZcodeError::of(codes::NOT_READY));
                    };
                    if cache.generation != generation {
                        return Err(stale());
                    }
                    cache.enum_bridges.insert(
                        workspace_key.clone(),
                        EnumBridge {
                            bridge_id: bridge_id.clone(),
                            workspace_path: workspace_path.clone(),
                            controller_subscription: None,
                            subscribed: false,
                            snapshot_seen: false,
                            snapshot_misses: 0,
                            next_subscribe_at: None,
                        },
                    );
                    bridge_id
                }
            };
            // 重订阅窗口：旧 subscriptionId 必须在发请求前摘掉。新 id 只有 ACK 才可知，
            // 而 ACK 与首个快照可能落在同一事件泵批次（on_data 顺序处理），
            // 保留旧 id 会把新快照误判为「旧订阅迟到帧」而零应用。
            // 旧订阅尽力退订（读面生命周期帧；失败不影响重订阅）。
            let (previous_subscription, release) = {
                let mut inner = lock(&self.state);
                let previous = inner
                    .workspaces
                    .as_mut()
                    .and_then(|cache| cache.enum_bridges.get_mut(workspace_key))
                    .and_then(|bridge| bridge.controller_subscription.take());
                let release = previous.as_deref().and_then(|subscription| {
                    inner.release_command(
                        &bridge_id,
                        WINDOW_CONTROLLER_CHANNEL,
                        CONTROLLER_UNSUBSCRIBE_METHOD,
                        subscription,
                    )
                });
                (previous, release)
            };
            if let Some(command) = release {
                let _ = self.commands.try_send(command);
            }
            match self.subscribe_controller(&bridge_id).await {
                Ok(subscription) => {
                    let mut inner = lock(&self.state);
                    if let Some(cache) = inner.workspaces.as_mut() {
                        if let Some(bridge) = cache.enum_bridges.get_mut(workspace_key) {
                            bridge.controller_subscription = Some(subscription);
                            bridge.subscribed = true;
                        }
                    }
                }
                Err(error) => {
                    // 订阅失败同样计入有界退避：网络错误不得变成每次 bind 都重试。
                    let mut inner = lock(&self.state);
                    if let Some(cache) = inner.workspaces.as_mut() {
                        if let Some(bridge) = cache.enum_bridges.get_mut(workspace_key) {
                            // 旧订阅可能仍在服务端存活：恢复旧 id 以继续过滤其迟到帧。
                            bridge.controller_subscription = previous_subscription;
                            bridge.subscribed = false;
                            bridge.snapshot_misses = bridge.snapshot_misses.saturating_add(1);
                            bridge.next_subscribe_at = Some(
                                Instant::now()
                                    + controller_backoff(
                                        self.timing.controller_backoff,
                                        bridge.snapshot_misses,
                                    ),
                            );
                        }
                    }
                    return Err(error);
                }
            }
        }

        // 有界收帧窗：等全部枚举桥的首个 controller 快照（或窗口到期）。
        let deadline = Instant::now() + self.timing.task_window;
        loop {
            if lock(&self.state)
                .workspaces
                .as_ref()
                .map(WorkspaceCache::rows_ready)
                .unwrap_or(false)
            {
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let notify = lock(&self.state).enum_notify.clone();
            // 先登记通知再复查条件：快照落在两步之间时不会丢唤醒（M-4）。
            let notified = notify.notified();
            if lock(&self.state)
                .workspaces
                .as_ref()
                .map(WorkspaceCache::rows_ready)
                .unwrap_or(false)
            {
                break;
            }
            let _ = tokio::time::timeout(deadline.saturating_duration_since(now), notified).await;
        }
        {
            let mut inner = lock(&self.state);
            let Some(cache) = inner.workspaces.as_mut() else {
                return Err(ZcodeError::of(codes::NOT_READY));
            };
            if cache.generation != generation {
                return Err(stale());
            }
            if !cache.rows_ready() {
                let now = Instant::now();
                for bridge in cache.enum_bridges.values_mut() {
                    if bridge.subscribed && !bridge.snapshot_seen {
                        // 窗口内没等到快照：撤销订阅位并进入退避，下一次读可重订阅。
                        bridge.subscribed = false;
                        bridge.snapshot_seen = false;
                        bridge.snapshot_misses = bridge.snapshot_misses.saturating_add(1);
                        bridge.next_subscribe_at = Some(
                            now + controller_backoff(
                                self.timing.controller_backoff,
                                bridge.snapshot_misses,
                            ),
                        );
                    }
                }
            }
        }
        let inner = lock(&self.state);
        Ok(inner
            .workspaces
            .as_ref()
            .map(|cache| cache.rows.values().cloned().collect())
            .unwrap_or_default())
    }

    /// EventListen(onDynamicControllerFrame) → subscribeControllerV4 → ACK。
    async fn subscribe_controller(&self, bridge_id: &str) -> ZResult<String> {
        let listener_id = self.register_listener(bridge_id, ListenerKind::Controller)?;
        self.send_listen(
            bridge_id,
            listener_id,
            WINDOW_CONTROLLER_CHANNEL,
            CONTROLLER_DYNAMIC_FRAME_EVENT,
            Value::Null,
            self.timing.rpc,
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
                bridge_id,
                WINDOW_CONTROLLER_CHANNEL,
                CONTROLLER_SUBSCRIBE_METHOD,
                vec![params],
                self.timing.rpc,
                None,
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
            if inner.bridges.len() >= self.limits.bridges {
                // 逻辑桥总数有界：超限直接拒绝，绝不无界增长。
                return Err(ZcodeError::of(codes::PAYLOAD_TOO_LARGE));
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
                    None,
                )
                .await?;
            let body = hello.body.as_object().ok_or_else(unsupported)?;
            if body.get("kind").and_then(Value::as_str) != Some("hello") {
                return Err(unsupported());
            }
            let protocol_version = body
                .get("protocolVersion")
                .and_then(Value::as_u64)
                .ok_or_else(unsupported)?;
            if protocol_version != V4_WIRE_PROTOCOL_VERSION {
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
            // 只有 kind/协议/connectionId 全部校验通过才记证据；迟到的旧桥握手
            // （换代/已丢弃）不得为当前代际背书。
            {
                let mut inner = lock(&self.state);
                if inner.bridges.contains_key(bridge_id) {
                    inner.hello_known = true;
                    inner.protocol_version =
                        Some(u32::try_from(protocol_version).map_err(|_| unsupported())?);
                    // T3：hello 校验通过即采集方法存在性（显式方法表或 v4 协议面）。
                    inner.interface.observe_hello(&hello.body);
                }
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
                None,
            )
            .await?;
        }
        Ok(())
    }

    /// 建立会话桥 + 会话 topic 订阅，并完成 ACK 后本地激活。
    async fn open_session(&self, target: &BoundTarget) -> ZResult<()> {
        let bridge_id = self
            .open_bridge(
                target.bridge_generation(),
                &target.workspace_key,
                Some(&target.session_id),
                true,
            )
            .await?;
        let inserted = {
            let mut inner = lock(&self.state);
            if inner.generation == target.bridge_generation() {
                inner.sessions.insert(
                    target.session_id.clone(),
                    SessionEntry::new(
                        target.clone(),
                        bridge_id.clone(),
                        self.limits.projection_bytes,
                        self.limits.staged_bytes,
                    ),
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
        let subscription = match self.subscribe_conversation(target, &bridge_id, None).await {
            Ok(subscription) => subscription,
            Err(error) => {
                self.drop_bridge(&bridge_id);
                return Err(error);
            }
        };
        let mut inner = lock(&self.state);
        // 先落到 &mut Inner 再拆字段借用（MutexGuard 的 DerefMut 不能拆字段）。
        let state = &mut *inner;
        let applied = match state.sessions.get_mut(&target.session_id) {
            Some(entry) => activate_and_flush(entry, &subscription, &mut state.interface),
            None => false,
        };
        if applied {
            inner.schema_known = true;
        }
        Ok(())
    }

    /// EventListen(onDynamicConversationFrame, workspace) → subscribeConversationV4 → ACK。
    ///
    /// `deadline = Some(d)` 时两个 RPC 各自只拿剩余预算（到期 → `not_ready`）。
    async fn subscribe_conversation(
        &self,
        target: &BoundTarget,
        bridge_id: &str,
        deadline: Option<Instant>,
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
        let listen_budget = self.step_budget(deadline)?;
        self.send_listen(
            bridge_id,
            listener_id,
            AGENT_CHANNEL,
            AGENT_CONVERSATION_FRAME_EVENT,
            Value::Object(workspace.clone()),
            listen_budget,
        )
        .await?;
        workspace.insert(
            "sessionId".to_owned(),
            Value::from(target.session_id.clone()),
        );
        let subscribe_budget = self.step_budget(deadline)?;
        let message = self
            .rpc_call(
                bridge_id,
                AGENT_CHANNEL,
                AGENT_SUBSCRIBE_CONVERSATION_METHOD,
                vec![Value::Object(workspace)],
                subscribe_budget,
                None,
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

    /// 单步 RPC 预算：无 deadline 沿用 `timing.rpc`；有 deadline 时取剩余（到期即
    /// `not_ready`，绝不发一个必然超期的请求）。
    fn step_budget(&self, deadline: Option<Instant>) -> ZResult<Duration> {
        let Some(deadline) = deadline else {
            return Ok(self.timing.rpc);
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ZcodeError::of(codes::NOT_READY));
        }
        Ok(remaining.min(self.timing.rpc))
    }

    /// 缺口/协议不符后的只读重订阅（取新快照；不发送任何写命令）。
    ///
    /// 有界退避：连续失败按 `resync_backoff * 2^(n-1)` 冷却，超过 [`MAX_RESYNCS`]
    /// 停在 `ready=false`，绝不让「读一次 → 重订阅一次」自激（C1）。
    async fn resync(&self, target: &BoundTarget, gate: &AsyncMutex<()>) -> ZResult<()> {
        self.resync_until(target, gate, None).await
    }

    /// [`Self::resync`] 的有界版本：`deadline = Some(d)` 时等重订阅闸与每个内部
    /// RPC 都只拿剩余预算（到期 → `not_ready`，由 Err 分支把 `needs_resync` 置回
    /// true，下一次读可继续重试）。绝不把调用方拖过 deadline，也不依赖取消
    /// 在途重订阅来收口（只取消「等闸」这种无副作用的等待）。
    async fn resync_until(
        &self,
        target: &BoundTarget,
        gate: &AsyncMutex<()>,
        deadline: Option<Instant>,
    ) -> ZResult<()> {
        // 另一个重订阅在途时，等闸也必须受预算约束（否则单读会被别人的 2×10s
        // 拖长）。等闸 future 无副作用，超时丢弃安全。
        let _guard = match deadline {
            None => gate.lock().await,
            Some(deadline) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(ZcodeError::of(codes::NOT_READY));
                }
                match tokio::time::timeout(remaining, gate.lock()).await {
                    Ok(guard) => guard,
                    Err(_) => return Err(ZcodeError::of(codes::NOT_READY)),
                }
            }
        };
        let (bridge_id, needs_resync, old_subscription) = {
            let inner = lock(&self.state);
            if inner.generation != target.bridge_generation() {
                return Err(stale());
            }
            let Some(entry) = inner.sessions.get(&target.session_id) else {
                return Err(ZcodeError::of(codes::NO_SESSION));
            };
            (
                entry.bridge_id.clone(),
                entry.needs_resync,
                entry.projection.subscription_id().map(str::to_owned),
            )
        };
        if !needs_resync {
            return Ok(());
        }
        let _backoff = {
            let mut inner = lock(&self.state);
            let Some(entry) = inner.sessions.get_mut(&target.session_id) else {
                return Err(ZcodeError::of(codes::NO_SESSION));
            };
            if entry.resync_attempts >= MAX_RESYNCS {
                // 连续重同步耗尽：停在 ready=false，不再发订阅（防抖动放大）。
                return Err(ZcodeError::of(codes::NOT_READY));
            }
            let backoff = self
                .timing
                .resync_backoff
                .saturating_mul(1u32 << entry.resync_attempts.saturating_sub(1).min(16));
            if let Some(last) = entry.last_resync_at {
                let ready_at = last + backoff;
                if Instant::now() < ready_at {
                    return Err(ZcodeError::of(codes::NOT_READY));
                }
            }
            // 先解除旧订阅 barrier：新订阅的帧（新 subscriptionId）在 ACK 前暂存。
            entry.projection.deactivate();
            entry.fragments.clear();
            entry.staged.clear();
            entry.staged_bytes = 0;
            entry.needs_resync = false;
            entry.resync_attempts = entry.resync_attempts.saturating_add(1);
            entry.last_resync_at = Some(Instant::now());
            entry.stats.resyncs = entry.stats.resyncs.saturating_add(1);
            backoff
        };
        // 尽力退订旧订阅（读面生命周期帧；失败不影响重订阅）。
        if let Some(subscription) = old_subscription {
            let release = {
                let mut inner = lock(&self.state);
                inner.release_command(
                    &bridge_id,
                    AGENT_CHANNEL,
                    AGENT_UNSUBSCRIBE_CONVERSATION_METHOD,
                    &subscription,
                )
            };
            if let Some(release) = release {
                let _ = self.commands.try_send(release);
            }
        }
        match self
            .subscribe_conversation(target, &bridge_id, deadline)
            .await
        {
            Ok(subscription) => {
                let mut inner = lock(&self.state);
                let mut still_unready = false;
                // 先落到 &mut Inner 再拆字段借用（MutexGuard 的 DerefMut 不能拆字段）。
                let state = &mut *inner;
                let applied = match state.sessions.get_mut(&target.session_id) {
                    Some(entry) => {
                        let applied =
                            activate_and_flush(entry, &subscription, &mut state.interface);
                        if entry.projection.is_ready() {
                            entry.resync_attempts = 0;
                            entry.last_resync_at = None;
                            entry.needs_resync = false;
                        } else {
                            // 新订阅也没能建立新鲜投影（例如快照仍是坏分片）：
                            // 本次读不得把「重订阅成功」当就绪。
                            still_unready = true;
                        }
                        applied
                    }
                    None => false,
                };
                if applied {
                    inner.schema_known = true;
                }
                if still_unready {
                    Err(ZcodeError::of(codes::NOT_READY))
                } else {
                    Ok(())
                }
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
        guard: Option<RpcGuard>,
    ) -> ZResult<ChannelMessage> {
        let (id, receiver) = self.register_rpc(bridge_id)?;
        let (reply_sender, reply_receiver) = oneshot::channel();
        let fail_fast = guard.is_some();
        let queued = if fail_fast {
            // 用户面调用：队列满必须立即以真实 offline 暴露（不伪造 not_ready）。
            self.commands
                .try_send(Command::SendRpc {
                    bridge_id: bridge_id.to_owned(),
                    id,
                    channel: channel.to_owned(),
                    method: method.to_owned(),
                    args,
                    guard,
                    reply: reply_sender,
                })
                .is_ok()
        } else {
            // 握手/订阅等内部步骤：等容量（整体受各自超时约束）。
            self.commands
                .send(Command::SendRpc {
                    bridge_id: bridge_id.to_owned(),
                    id,
                    channel: channel.to_owned(),
                    method: method.to_owned(),
                    args,
                    guard,
                    reply: reply_sender,
                })
                .await
                .is_ok()
        };
        if !queued {
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
        timeout: Duration,
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
            .await
            .is_err()
        {
            return Err(ZcodeError::offline());
        }
        match tokio::time::timeout(timeout, reply_receiver).await {
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
            cache
                .enum_bridges
                .retain(|_, bridge| bridge.bridge_id != bridge_id);
        }
        inner.enum_notify.notify_waiters();
    }
}

impl Inner {
    /// 为一条读面生命周期 RPC（退订）分配 request id 并构造命令。
    ///
    /// 尽力而为：桥已不存在时返回 `None`（不重试、不报错）。
    fn release_command(
        &mut self,
        bridge_id: &str,
        channel: &str,
        method: &str,
        subscription_id: &str,
    ) -> Option<Command> {
        let bridge = self.bridges.get_mut(bridge_id)?;
        bridge.next_request_id = bridge.next_request_id.wrapping_add(1).max(1);
        Some(Command::SendRpcNowait {
            bridge_id: bridge_id.to_owned(),
            identity: RpcIdentity::new(bridge_id),
            id: bridge.next_request_id,
            channel: channel.to_owned(),
            method: method.to_owned(),
            args: vec![serde_json::json!({"subscriptionId": subscription_id})],
        })
    }

    /// 入站路由：只做纯内存工作；返回值 = 本次要发出的 rpc-frame-ack 与生命周期命令。
    fn handle_data(&mut self, payload: &Value) -> (ZResult<()>, Vec<Value>, Vec<Command>) {
        let mut acks = Vec::new();
        let mut releases = Vec::new();
        let Some(object) = payload.as_object() else {
            return (Ok(()), acks, releases);
        };
        match object.get("zcode_type").and_then(Value::as_str) {
            Some("rpc-frame-ack") => (Ok(()), acks, releases),
            Some("rpc-frame") => {
                let result = self.handle_frame(object, &mut acks, &mut releases);
                (result, acks, releases)
            }
            _ => {
                if let Some(request_id) = object.get("requestId").and_then(Value::as_str) {
                    if let Some(waiter) = self.app_waiters.remove(request_id) {
                        let _ = waiter.send(payload.clone());
                    }
                }
                (Ok(()), acks, releases)
            }
        }
    }

    fn handle_frame(
        &mut self,
        object: &Map<String, Value>,
        acks: &mut Vec<Value>,
        releases: &mut Vec<Command>,
    ) -> ZResult<()> {
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
                self.mark_bridge_unready(&bridge_id, releases);
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
            // `messageSeq` 是**逻辑消息号**：一条逻辑消息可拆成多条物理帧下发，
            // 全部共享同一 `messageSeq`，只有物理 `seq` 递增。只有严格更旧的消息
            // 才是重复/迟到；同值必须放行给 `FrameAssembler`（它按
            // `(messageSeq, fragmentIndex)` 指纹去重与组装），否则首片之后的
            // 物理分片会被整条丢弃。物理 `seq` 的重复/缺口由上面的 `seq` 检查
            // 拦截；同一 `messageSeq` 换新 `seq` 的重传帧由 `FrameAssembler`
            // 的分片指纹去重。
            if message_seq < bridge.last_message_seq {
                return Ok(());
            }
        }
        if gap {
            // 物理缺口：本帧及之前丢失的消息都不可信 → 丢弃并重订阅取新快照。
            self.mark_bridge_unready(&bridge_id, releases);
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
                self.mark_bridge_unready(&bridge_id, releases);
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
                self.mark_bridge_unready(&bridge_id, releases);
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
                ListenerKind::Controller => self.handle_controller_frame(&bridge_id, &body),
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

    /// controller 任务索引帧（snapshot 整体替换本 workspace 的行 / deltas upsert+removed）。
    fn handle_controller_frame(&mut self, bridge_id: &str, body: &Value) {
        let Some(object) = body.as_object() else {
            return;
        };
        if object.get("topic").and_then(Value::as_str) != Some(CONTROLLER_TASKS_INDEX_TOPIC) {
            return;
        }
        let Some(payload) = object.get("payload").and_then(Value::as_object) else {
            return;
        };
        // 帧必须来自本枚举桥且 topic/subscription 对齐（同桥验证）。
        let Some((workspace_path, subscription)) = self.workspaces.as_ref().and_then(|cache| {
            cache
                .enum_bridges
                .values()
                .find(|bridge| bridge.bridge_id == bridge_id)
                .map(|bridge| {
                    (
                        bridge.workspace_path.clone(),
                        bridge.controller_subscription.clone(),
                    )
                })
        }) else {
            return;
        };
        if let Some(subscription) = &subscription {
            if object.get("subscriptionId").and_then(Value::as_str) != Some(subscription.as_str()) {
                // 旧订阅（缺口重订阅后迟到）：零应用。
                return;
            }
        }
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
                    // 只替换本 workspace 的行（其它枚举桥的行保持不变）。
                    cache.rows.retain(|(path, _), _| path != &workspace_path);
                    for task in tasks {
                        if let Some(row) = parse_task_row(task) {
                            insert_task_row(cache, row);
                        }
                    }
                    if let Some(bridge) = cache
                        .enum_bridges
                        .values_mut()
                        .find(|bridge| bridge.bridge_id == bridge_id)
                    {
                        bridge.snapshot_seen = true;
                        // 快照到达即证明订阅有效（也覆盖「窗错过后的迟到快照」自愈）。
                        bridge.subscribed = true;
                        bridge.snapshot_misses = 0;
                        bridge.next_subscribe_at = None;
                    }
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
                                    insert_task_row(cache, row);
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

    /// conversation 事件帧：session/topic/subscription 三层过滤 + 应用级分片重组 +
    /// ACK 前暂存。
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
        let mut applied = false;
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
                applied = stage_or_apply(entry, frame, subscription, &mut self.interface);
            }
            Some("fragment") => {
                let budget = entry.staged_budget;
                match entry
                    .fragments
                    .accept(object, &expected_topic, &subscription, budget)
                {
                    FragmentOutcome::Incomplete => {}
                    FragmentOutcome::Complete(frame) => {
                        entry.stats.fragments_reassembled =
                            entry.stats.fragments_reassembled.saturating_add(1);
                        applied = stage_or_apply(entry, frame, subscription, &mut self.interface);
                    }
                    FragmentOutcome::Fault => {
                        entry.fragments.clear();
                        entry.stats.fragment_faults = entry.stats.fragment_faults.saturating_add(1);
                        mark_session_unready(entry);
                    }
                }
            }
            _ => mark_session_unready(entry),
        }
        // 只有 schema 解析成功并进入投影才算识别（坏帧/暂存/异订阅不算）。
        if applied {
            self.schema_known = true;
        }
    }

    /// 物理缺口/坏帧：整桥水位清空 + 所属会话置不新鲜（下次读触发重订阅）。
    ///
    /// 若该桥是枚举桥：**只**失效本 workspace 的任务行（清快照位、退订旧 controller
    /// 订阅），下一次 `load_tasks` 重新订阅取新快照，绝不把带洞的任务行当新鲜（spec I1）；
    /// 其它 workspace 的枚举桥与行保持新鲜（N1：单桥传输故障不得让其它工作区不可绑定）。
    fn mark_bridge_unready(&mut self, bridge_id: &str, releases: &mut Vec<Command>) {
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
        let controller_subscription = self
            .workspaces
            .as_ref()
            .and_then(|cache| {
                cache
                    .enum_bridges
                    .values()
                    .find(|bridge| bridge.bridge_id == bridge_id)
                    .map(|bridge| bridge.controller_subscription.clone())
            })
            .flatten();
        let mut cleared_workspace: Option<String> = None;
        if let Some(cache) = self.workspaces.as_mut() {
            if let Some(bridge) = cache
                .enum_bridges
                .values_mut()
                .find(|bridge| bridge.bridge_id == bridge_id)
            {
                cleared_workspace = Some(bridge.workspace_path.clone());
                bridge.snapshot_seen = false;
                bridge.subscribed = false;
                bridge.controller_subscription = None;
                // 缺口是确定性失败：重试预算重置（下一次读立即重订阅）。
                bridge.snapshot_misses = 0;
                bridge.next_subscribe_at = None;
            }
            if let Some(workspace_path) = &cleared_workspace {
                // 行按 workspace 分区存储：只清本 workspace（N1）。
                cache
                    .rows
                    .retain(|(row_workspace, _), _| row_workspace != workspace_path);
            }
        }
        if cleared_workspace.is_some() {
            if let Some(subscription) = controller_subscription {
                if let Some(command) = self.release_command(
                    bridge_id,
                    WINDOW_CONTROLLER_CHANNEL,
                    CONTROLLER_UNSUBSCRIBE_METHOD,
                    &subscription,
                ) {
                    releases.push(command);
                }
            }
            self.enum_notify.notify_waiters();
        }
    }
}

/// controller 重订阅退避：`base * 2^(misses-1)`（与会话重同步同构）。
fn controller_backoff(base: Duration, misses: u32) -> Duration {
    base.saturating_mul(1u32 << misses.saturating_sub(1).min(8))
}

/// 任务索引行入库：按 `workspacePath\0taskId` 去重，总行数受合同上限约束
/// （`maxRows=1024`；超出拒收并计数，绝不无界增长）。
fn insert_task_row(cache: &mut WorkspaceCache, row: TaskRow) {
    let key = (row.workspace_path.clone(), row.task_id.clone());
    if !cache.rows.contains_key(&key) && cache.rows.len() >= MAX_TASK_ROWS {
        cache.rows_rejected = cache.rows_rejected.saturating_add(1);
        return;
    }
    cache.rows.insert(key, row);
}

/// ACK 前暂存 / 已激活直通（应用级分片重组成功后也走这里）。
///
/// 返回 `true` = 本帧已进入投影（schema 解析成功）；暂存/异订阅不产生证据。
fn stage_or_apply(
    entry: &mut SessionEntry,
    frame: Value,
    subscription: String,
    interface: &mut ZcodeInterfaceEvidence,
) -> bool {
    match entry.projection.subscription_id().map(str::to_owned) {
        None => {
            let frame_bytes = serde_json::to_vec(&frame).map(|bytes| bytes.len());
            let frame_bytes = match frame_bytes {
                Ok(bytes) => bytes,
                Err(_) => {
                    entry.stats.frames_rejected = entry.stats.frames_rejected.saturating_add(1);
                    mark_session_unready(entry);
                    return false;
                }
            };
            if entry.staged.len() >= MAX_STAGED_CONVERSATION_FRAMES
                || entry.staged_bytes.saturating_add(frame_bytes) > entry.staged_budget
            {
                // 条数/字节双预算：超限即拒收并置不新鲜（绝不无界增长）。
                entry.stats.frames_rejected = entry.stats.frames_rejected.saturating_add(1);
                mark_session_unready(entry);
                return false;
            }
            entry.staged_bytes = entry.staged_bytes.saturating_add(frame_bytes);
            entry.staged.push(StagedFrame {
                subscription_id: subscription,
                frame,
            });
            false
        }
        Some(bound) if bound == subscription => apply_frame(entry, frame, interface),
        // 旧 subscriptionId（换代/重订阅后迟到）：零应用。
        Some(_) => false,
    }
}

/// ACK 到达后的本地 barrier 放行：只交付匹配 subscriptionId 的暂存帧。
///
/// 返回 `true` = 至少一帧 schema 解析成功并进入投影（供宿主证据置位）。
fn activate_and_flush(
    entry: &mut SessionEntry,
    subscription: &str,
    interface: &mut ZcodeInterfaceEvidence,
) -> bool {
    entry.projection.activate(subscription);
    let staged = std::mem::take(&mut entry.staged);
    entry.staged_bytes = 0;
    let mut applied = false;
    for item in staged {
        if item.subscription_id != subscription {
            continue;
        }
        if apply_frame(entry, item.frame, interface) {
            entry.stats.staged_delivered = entry.stats.staged_delivered.saturating_add(1);
            applied = true;
        }
    }
    applied
}

/// 应用一帧；缺口/未知 schema 时标记重同步（stale/异会话只丢弃）。
fn apply_frame(
    entry: &mut SessionEntry,
    frame: Value,
    interface: &mut ZcodeInterfaceEvidence,
) -> bool {
    // T3：形状观察在投影应用前做——帧已过 topic/subscriptionId 闸，属本代际
    // 订阅上的真实下行流量；观察是幂等 OR 的存在性记录（宿主宣告过的形状不因
    // 本帧应用成败回退），且避免为观察克隆整帧。
    interface.observe_conversation_frame(&frame);
    // T5：日志头指纹同步推进（send 确认的「新用户消息可见」观察面；id 形状，
    // 不存正文）。快照应用成败不回退指纹——指纹跟随帧内容，不跟随投影状态。
    entry.log_fingerprint = advance_log_fingerprint(entry.log_fingerprint.as_deref(), &frame);
    match entry
        .projection
        .apply(entry.target.bridge_generation(), frame)
    {
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

/// 从一条会话帧推进日志头指纹（T5；spec R4「新用户消息可见」的观察面）。
///
/// 语义：会话日志的头位置由 `(logEpoch, seq)` 标识——新消息（含 send 回显的
/// 用户消息）追加即前进。快照整体替换两个键；`state.updated` patch 通常只带
/// `seq`（沿用旧 epoch）；两者皆缺（pending/control/config 等其他字段的变化）
/// 指纹不变。合成规则：
/// - 无完整 (epoch, seq) 对（从未见过快照或字段缺席）→ `None`：无指纹证据，
///   send 确认保守不升级（绝不用半个指纹伪造「变化」）；
/// - 指纹是 id 形状的 `"{epoch}:{seq}"`，不含任何正文。
fn advance_log_fingerprint(previous: Option<&str>, frame: &Value) -> Option<String> {
    let Some(payload) = frame.get("payload") else {
        return previous.map(str::to_owned);
    };
    // 本帧暴露的 (epoch, seq) 片段：快照整体替换、patch 逐个喂入。
    let mut epoch: Option<String> = None;
    let mut seq: Option<u64> = None;
    let mut touched = false;
    let mut feed = |object: &Value| {
        if let Some(value) = object.get("logEpoch").and_then(Value::as_str) {
            epoch = Some(value.to_owned());
            touched = true;
        }
        if let Some(value) = object.get("seq").and_then(Value::as_u64) {
            seq = Some(value);
            touched = true;
        }
    };
    match payload.get("kind").and_then(Value::as_str) {
        Some("snapshot") => {
            if let Some(snapshot) = payload.get("snapshot") {
                feed(snapshot);
            }
        }
        Some("deltas") => {
            for patch in payload
                .get("deltas")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if patch.get("op").and_then(Value::as_str) == Some("state.updated") {
                    if let Some(patch) = patch.get("patch") {
                        feed(patch);
                    }
                }
            }
        }
        // 其他 payload（controller 帧等）：与会话日志无关，指纹不变。
        _ => return previous.map(str::to_owned),
    }
    if !touched {
        return previous.map(str::to_owned);
    }
    // 本帧缺的半边沿用旧指纹的对应半边（patch 通常只前进 seq）。
    let (previous_epoch, previous_seq) = previous
        .and_then(|value| value.rsplit_once(':'))
        .map(|(epoch, seq)| (Some(epoch.to_owned()), seq.parse::<u64>().ok()))
        .unwrap_or((None, None));
    match (epoch.or(previous_epoch), seq.or(previous_seq)) {
        (Some(epoch), Some(seq)) => Some(format!("{epoch}:{seq}")),
        // 缺任一半：无可用指纹证据（保守方向——send 确认等完整对）。
        _ => None,
    }
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
async fn run_actor(shared: Arc<Shared>, mut commands: mpsc::Receiver<Command>) {
    let mut counters: HashMap<String, SeqCounters> = HashMap::new();
    while let Some(command) = commands.recv().await {
        match command {
            Command::SendRpc {
                bridge_id,
                id,
                channel,
                method,
                args,
                guard,
                reply,
            } => {
                // 就绪闸在 actor 内、真正写帧前复核：`call` 的预检之后若发生
                // 并发缺口，这里必须以 `not_ready` 收口且零写（quality I-4）。
                let ready = match &guard {
                    Some(guard) => {
                        let inner = lock(&shared.state);
                        inner.generation == guard.generation
                            && inner
                                .sessions
                                .get(&guard.session_id)
                                .map(|entry| entry.projection.is_ready())
                                .unwrap_or(false)
                    }
                    None => true,
                };
                let result = if ready {
                    send_rpc_frame(
                        &shared,
                        &mut counters,
                        &bridge_id,
                        id,
                        &channel,
                        &method,
                        args,
                    )
                    .await
                } else {
                    Err(ZcodeError::of(codes::NOT_READY))
                };
                let _ = reply.send(result);
            }
            Command::SendRpcNowait {
                bridge_id,
                identity,
                id,
                channel,
                method,
                args,
            } => {
                // 读面生命周期帧：无 waiter、失败静默（退订是尽力而为）。
                // identity 随命令携带：invalidate 已清空 bridges，仍须发出退订帧。
                let _ = emit_rpc_frame(
                    &shared,
                    &mut counters,
                    &bridge_id,
                    &identity,
                    id,
                    &channel,
                    &method,
                    args,
                )
                .await;
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
    emit_rpc_frame(
        shared, counters, bridge_id, &identity, id, channel, method, args,
    )
    .await
}

/// 用显式身份发一条 RPC 帧（退订路径：桥可能已被代际清理）。
#[allow(clippy::too_many_arguments)]
async fn emit_rpc_frame(
    shared: &Shared,
    counters: &mut HashMap<String, SeqCounters>,
    bridge_id: &str,
    identity: &RpcIdentity,
    id: u32,
    channel: &str,
    method: &str,
    args: Vec<Value>,
) -> ZResult<()> {
    let bytes = encode_call(id, channel, method, args)?;
    let counter = counters.entry(bridge_id.to_owned()).or_default();
    counter.seq = counter.seq.saturating_add(1);
    counter.message_seq = counter.message_seq.saturating_add(1);
    let frame = encode_frame(identity, counter.seq, counter.message_seq, &bytes)?;
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

    use futures::future::BoxFuture;
    use serde_json::json;

    use crate::services::zcode_control::codec::{
        crc32_hex, encode_frame, RpcIdentity, RESPONSE_TYPE_EVENT_FIRE,
        RESPONSE_TYPE_PROMISE_SUCCESS,
    };
    use crate::services::zcode_control::command::AGENT_SEND_COMMAND_METHOD;
    use crate::services::zcode_control::projection::CONVERSATION_TOPIC_PREFIX;
    use crate::services::zcode_control::types::{CapabilityPolicy, CapabilityView};

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
        /// 从第 N 次 controller 订阅起才下发快照（收帧窗错过用例）。
        controller_snapshot_from_attempt: u32,
        /// 同一枚举桥第 2 次 controller 订阅时返回的（全量）任务行。
        controller_resubscribe_tasks: Option<Vec<Value>>,
        conversation_snapshot: Option<Value>,
        /// 把首个快照拆成 `kind:"fragment"` 两片下发（CRC32 按 UTF-8 字节）。
        fragmented_snapshot: bool,
        /// 重订阅（第 2+ 次）的快照同样分片下发（退避/抖动用例）。
        fragmented_resync: bool,
        /// 分片发送顺序（可重复以制造 duplicate；缺片以制造 incomplete）。
        fragment_order: Vec<usize>,
        /// 覆盖分片 CRC（坏 checksum fail-closed 用例）。
        fragment_checksum_override: Option<String>,
        /// 这些 RPC 方法不应答（RPC 超时清账用例）。
        silent_methods: Vec<String>,
        /// 第 N 次（1-based）及之后才静默的方法（重订阅有界读用例：首次绑定订阅
        /// 必须成功，后续重订阅不应答）。
        silent_methods_from_call: Vec<(String, u32)>,
        pre_ack_cards: Vec<Value>,
        /// 该桥第 N 个入站帧之后注入物理 seq 缺口。
        gap_after_frames: Option<usize>,
        resync_snapshot: Option<Value>,
        /// 写命令（`sendConversationCommandV4`）的 ACK 应答体（T4.1 B1：
        /// accepted/duplicate/noop/rejected 分类）；`None` = 既有的
        /// `{"method": <method>}` 回显（未知状态）。
        command_ack: Option<Value>,
        /// `model-selection.getView` 的应答体（T6 SelectionPort.load；合成目录）。
        model_selection_view: Option<Value>,
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
                controller_snapshot_from_attempt: 1,
                controller_resubscribe_tasks: None,
                conversation_snapshot: None,
                fragmented_snapshot: false,
                fragmented_resync: false,
                fragment_order: vec![0, 1],
                fragment_checksum_override: None,
                silent_methods: Vec::new(),
                silent_methods_from_call: Vec::new(),
                pre_ack_cards: Vec::new(),
                gap_after_frames: None,
                resync_snapshot: None,
                command_ack: None,
                model_selection_view: None,
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
        /// 每桥 controller 订阅次数（第 2 次起用 resubscribe 行）。
        controller_subscriptions: HashMap<String, u32>,
        /// 每桥 controller 快照实际下发的 taskId（workspace 作用域断言）。
        controller_rows_by_bridge: HashMap<String, Vec<String>>,
        /// bridge → workspacePath（fake 的 workspace 作用域过滤依据）。
        bridge_workspace: HashMap<String, String>,
        /// 每桥物理 seq/messageSeq 单调校验（fake 自己验证出站帧）。
        last_wire: HashMap<String, (u64, u64)>,
        order_faults: u64,
        /// 每次 RPC 请求的 (bridge, method, requestId)。
        requests: Vec<(String, String, u32)>,
        frames_sent: HashMap<String, usize>,
        gap_injected: HashSet<String>,
        wire_sequences: Vec<(String, u64, u64)>,
    }

    struct Peer {
        config: PeerConfig,
        inner: StdMutex<PeerInner>,
        manager: StdMutex<Weak<BridgeManager>>,
        /// 下一次 `send_data` 的写闩（模拟半开 socket / actor 被占住）。
        gate: StdMutex<Option<Arc<tokio::sync::Semaphore>>>,
        stalled: std::sync::atomic::AtomicBool,
    }

    impl Peer {
        fn new(config: PeerConfig) -> Arc<Self> {
            Arc::new(Self {
                config,
                inner: StdMutex::new(PeerInner::default()),
                manager: StdMutex::new(Weak::new()),
                gate: StdMutex::new(None),
                stalled: std::sync::atomic::AtomicBool::new(false),
            })
        }

        fn attach(&self, manager: &Arc<BridgeManager>) {
            *self.manager.lock().expect("peer lock") = Arc::downgrade(manager);
        }

        fn sent(&self) -> Vec<Value> {
            self.inner.lock().expect("peer lock").sent.clone()
        }

        /// 让下一条出站帧停在 actor 里（写闩），返回放行句柄。
        fn hold_next_send(&self) -> Arc<tokio::sync::Semaphore> {
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            *self.gate.lock().expect("peer lock") = Some(gate.clone());
            gate
        }

        fn stalled(&self) -> bool {
            self.stalled.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn order_faults(&self) -> u64 {
            self.inner.lock().expect("peer lock").order_faults
        }

        fn request_ids(&self, method: &str) -> Vec<u32> {
            self.inner
                .lock()
                .expect("peer lock")
                .requests
                .iter()
                .filter(|(_, known, _)| known == method)
                .map(|(_, _, id)| *id)
                .collect()
        }

        fn last_request_id(&self, method: &str) -> Option<u32> {
            self.request_ids(method).last().copied()
        }

        fn controller_rows(&self, bridge: &str) -> Vec<String> {
            self.inner
                .lock()
                .expect("peer lock")
                .controller_rows_by_bridge
                .get(bridge)
                .cloned()
                .unwrap_or_default()
        }

        fn enum_bridge_ids(&self) -> Vec<String> {
            self.sent()
                .iter()
                .filter(|payload| {
                    payload.get("zcode_type").and_then(Value::as_str) == Some(WORKSPACE_BRIDGE_OPEN)
                        && payload.get("taskId").is_none()
                })
                .filter_map(|payload| payload.get("bridgeSessionId").and_then(Value::as_str))
                .map(str::to_owned)
                .collect()
        }

        fn session_bridge_id(&self, session_id: &str) -> String {
            self.sent()
                .iter()
                .rev()
                .find(|payload| {
                    payload.get("zcode_type").and_then(Value::as_str) == Some(WORKSPACE_BRIDGE_OPEN)
                        && payload.get("taskId").and_then(Value::as_str) == Some(session_id)
                })
                .and_then(|payload| payload.get("bridgeSessionId").and_then(Value::as_str))
                .map(str::to_owned)
                .expect("session bridge opened")
        }

        fn wire_sequences(&self) -> Vec<(String, u64, u64)> {
            self.inner.lock().expect("peer lock").wire_sequences.clone()
        }

        /// 该桥已注册的 EventListen 请求 id（注入 conversation 事件用）。
        fn listen_id(&self, bridge: &str) -> u32 {
            *self
                .inner
                .lock()
                .expect("peer lock")
                .listen
                .get(bridge)
                .expect("listen registered")
        }

        async fn handle(&self, payload: Value) {
            let gate = { self.gate.lock().expect("peer lock").take() };
            if let Some(gate) = gate {
                // 写闩：先让测试看见「actor 已停住」，再等放行。
                self.stalled
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                let _ = gate.acquire().await;
            }
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

        /// 注入一条带新物理 seq 的入站帧（模拟迟到应答）；返回本次变化的会话视图。
        async fn inject_frame(&self, bridge: &str, bytes: Vec<u8>) -> Vec<SessionView> {
            let frame = {
                let mut inner = self.inner.lock().expect("peer lock");
                self.frame(&mut inner, bridge, bytes)
            };
            let manager = self.manager.lock().expect("peer lock").upgrade();
            if let Some(manager) = manager {
                manager.on_data(frame).await.unwrap_or_default()
            } else {
                Vec::new()
            }
        }

        /// 注入一条迟到/重复的 promise 应答（携带指定 requestId）。
        async fn inject_response(&self, bridge: &str, id: u32, body: &Value) {
            self.inject_frame(bridge, promise_success(id, body)).await;
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
            // 记录该桥的 workspace：controller 任务索引按桥的 workspace 作用域下发
            // （fake 不再把全局行无条件抹平，见 quality I-2）。
            let workspace_key = payload
                .get("workspaceKey")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if let Some(path) = self
                .config
                .workspaces
                .iter()
                .find(|entry| {
                    entry.get("workspaceIdentity").and_then(Value::as_str) == Some(workspace_key)
                        || entry.get("workspacePath").and_then(Value::as_str) == Some(workspace_key)
                })
                .and_then(|entry| entry.get("workspacePath"))
                .and_then(Value::as_str)
            {
                inner
                    .bridge_workspace
                    .insert(requested.to_owned(), path.to_owned());
            }
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
            // 物理 seq/messageSeq 必须逐桥严格递增（fake 自带失败证据）。
            let seq = payload.get("seq").and_then(Value::as_u64);
            let message_seq = payload.get("messageSeq").and_then(Value::as_u64);
            match (seq, message_seq) {
                (Some(seq), Some(message_seq)) => {
                    let previous = inner.last_wire.get(&bridge).copied().unwrap_or((0, 0));
                    if seq != previous.0 + 1 || message_seq != previous.1 + 1 {
                        inner.order_faults = inner.order_faults.saturating_add(1);
                    }
                    inner.last_wire.insert(bridge.clone(), (seq, message_seq));
                    inner
                        .wire_sequences
                        .push((bridge.clone(), seq, message_seq));
                }
                _ => {
                    inner.order_faults = inner.order_faults.saturating_add(1);
                }
            }
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
                    inner.requests.push((bridge.clone(), method.clone(), id));
                    let call_index = inner
                        .requests
                        .iter()
                        .filter(|(_, seen, _)| seen == &method)
                        .count() as u32;
                    let silenced = self
                        .config
                        .silent_methods
                        .iter()
                        .any(|silent| silent == &method)
                        || self.config.silent_methods_from_call.iter().any(
                            |(silent, from_call)| silent == &method && call_index >= *from_call,
                        );
                    if silenced {
                        // 不应答：用于验证 RPC 超时清账与迟到应答丢弃。
                        return;
                    }
                    let first = message.body.get(0).cloned().unwrap_or(Value::Null);
                    match method.as_str() {
                        AGENT_UNSUBSCRIBE_CONVERSATION_METHOD | CONTROLLER_UNSUBSCRIBE_METHOD => {
                            out.push(self.frame(
                                inner,
                                &bridge,
                                promise_success(id, &json!({"unsubscribed": true})),
                            ));
                        }
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
                            let subscription = format!("ctrl-sub-synthetic-{id}");
                            let ack = json!({"ack": {
                                "subscriptionId": subscription,
                                "mode": "snapshot",
                                "logEpoch": "synthetic-epoch"
                            }});
                            out.push(self.frame(inner, &bridge, promise_success(id, &ack)));
                            if self.config.controller_snapshot {
                                let attempt = inner
                                    .controller_subscriptions
                                    .entry(bridge.clone())
                                    .or_insert(0);
                                *attempt += 1;
                                let attempt = *attempt;
                                // 首次订阅故意不下发快照（收帧窗错过 → 重订阅用例）。
                                if attempt < self.config.controller_snapshot_from_attempt {
                                    return;
                                }
                                let source = if attempt >= 2 {
                                    self.config
                                        .controller_resubscribe_tasks
                                        .as_ref()
                                        .unwrap_or(&self.config.tasks)
                                } else {
                                    &self.config.tasks
                                };
                                // workspace 作用域过滤：只下发该桥所属 workspace 的行。
                                let workspace_path = inner
                                    .bridge_workspace
                                    .get(&bridge)
                                    .cloned()
                                    .unwrap_or_default();
                                let rows: Vec<Value> = source
                                    .iter()
                                    .filter(|row| {
                                        row.get("address")
                                            .and_then(|address| address.get("workspacePath"))
                                            .and_then(Value::as_str)
                                            == Some(workspace_path.as_str())
                                    })
                                    .cloned()
                                    .collect();
                                inner.controller_rows_by_bridge.insert(
                                    bridge.clone(),
                                    rows.iter()
                                        .filter_map(|row| {
                                            row.get("address")
                                                .and_then(|address| address.get("taskId"))
                                                .and_then(Value::as_str)
                                                .map(str::to_owned)
                                        })
                                        .collect(),
                                );
                                let listen = inner.listen.get(&bridge).copied().unwrap_or(0);
                                let event = json!({
                                    "topic": CONTROLLER_TASKS_INDEX_TOPIC,
                                    "subscriptionId": subscription,
                                    "logEpoch": "synthetic-epoch",
                                    "fromSeq": 0,
                                    "toSeq": 1,
                                    "sentAt": 0,
                                    "payload": {
                                        "kind": "snapshot",
                                        "snapshot": {
                                            "protocolVersion": 1,
                                            "logEpoch": "synthetic-epoch",
                                            "tasks": rows
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
                                let fragmented = if count == 1 {
                                    self.config.fragmented_snapshot
                                } else {
                                    self.config.fragmented_resync
                                };
                                self.push_conversation_snapshot(
                                    inner,
                                    &bridge,
                                    listen,
                                    &topic,
                                    &subscription,
                                    &snapshot,
                                    u64::from(count),
                                    fragmented,
                                    out,
                                );
                            }
                        }
                        _ => {
                            let body = if method == MODEL_SELECTION_GET_VIEW_METHOD {
                                self.config.model_selection_view.clone()
                            } else {
                                self.config.command_ack.clone()
                            }
                            .unwrap_or_else(|| json!({"method": method}));
                            out.push(self.frame(inner, &bridge, promise_success(id, &body)));
                        }
                    }
                }
                _ => {}
            }
        }

        /// 会话快照下发：完整帧直通；`fragmented_snapshot` 时按 UTF-8 字节两片 + CRC32。
        #[allow(clippy::too_many_arguments)]
        fn push_conversation_snapshot(
            &self,
            inner: &mut PeerInner,
            bridge: &str,
            listen: u32,
            topic: &str,
            subscription: &str,
            snapshot: &Value,
            ordinal: u64,
            fragmented: bool,
            out: &mut Vec<Value>,
        ) {
            let payload = json!({"kind": "snapshot", "snapshot": snapshot});
            if !fragmented {
                let wire = test_wire(topic, subscription, payload);
                out.push(self.frame(
                    inner,
                    bridge,
                    channel_message(json!([RESPONSE_TYPE_EVENT_FIRE, listen]), &wire),
                ));
                return;
            }
            let logical = json!({
                "topic": topic,
                "subscriptionId": subscription,
                "fromSeq": 0,
                "toSeq": 1,
                "sentAt": 0,
                "payload": payload
            });
            let bytes = serde_json::to_vec(&logical).expect("logical frame json");
            let split = (bytes.len() / 2).max(1);
            let chunks = [&bytes[..split], &bytes[split..]];
            let checksum = self
                .config
                .fragment_checksum_override
                .clone()
                .unwrap_or_else(|| crc32_hex(&bytes));
            let logical_id = format!("synthetic-logical-{ordinal}");
            for index in &self.config.fragment_order {
                let chunk = chunks[*index];
                let wire = json!({
                    "wireVersion": V4_WIRE_PROTOCOL_VERSION,
                    "kind": "fragment",
                    "deliveryKind": "initial",
                    "logicalFrameId": logical_id,
                    "logicalFrameOrdinal": ordinal,
                    "topic": topic,
                    "subscriptionId": subscription,
                    "fragmentIndex": *index,
                    "fragmentCount": chunks.len(),
                    "logicalBytes": bytes.len(),
                    "checksum": {"algorithm": "crc32", "value": checksum},
                    "dataBase64": base64::engine::general_purpose::STANDARD.encode(chunk)
                });
                out.push(self.frame(
                    inner,
                    bridge,
                    channel_message(json!([RESPONSE_TYPE_EVENT_FIRE, listen]), &wire),
                ));
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

    /// 注入宿主 build 探针的 fake 对端（证据用例；不读真实 `/Applications`）。
    async fn manager_with_host_build(
        config: PeerConfig,
        host_build: HostBuildProbe,
    ) -> (Arc<BridgeManager>, Arc<Peer>) {
        let peer = Peer::new(config);
        let manager = Arc::new(
            BridgeManager::with_timing(peer.clone(), BridgeTiming::test())
                .with_host_build(host_build),
        );
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

        // 按候选 workspace 各一条枚举桥（合并索引才能发现跨 workspace 的重复 id），
        // 再加两个会话桥。
        assert_eq!(app_payloads(&peer, WORKSPACE_BRIDGE_OPEN).len(), 4);
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
        // 两个候选 workspace 各一条枚举桥（合并索引），最后一条是会话桥。
        assert_eq!(opens.len(), 3);
        // 枚举期省略 taskId（官方 taskId? 可选；本模块只在绑定会话后携带）。
        assert!(opens[0].get("taskId").is_none());
        assert!(opens[1].get("taskId").is_none());
        assert_eq!(opens[0]["workspaceKey"], json!("ws-a-identity"));
        assert_eq!(opens[1]["workspaceKey"], json!("ws-b-identity"));
        // 会话桥携带精确 taskId + 宿主 workspaceKey。
        assert_eq!(opens[2]["taskId"], json!(SESSION_A));
        assert_eq!(opens[2]["workspaceKey"], json!("ws-a-identity"));

        let frames = channel_frames(&peer);
        let sequence: Vec<(u32, Option<String>, Option<String>)> = frames
            .iter()
            .map(|frame| (frame.kind, frame.channel.clone(), frame.method.clone()))
            .collect();
        let controller_listen = (
            102,
            Some(WINDOW_CONTROLLER_CHANNEL.to_owned()),
            Some(CONTROLLER_DYNAMIC_FRAME_EVENT.to_owned()),
        );
        let controller_subscribe = (
            100,
            Some(WINDOW_CONTROLLER_CHANNEL.to_owned()),
            Some(CONTROLLER_SUBSCRIBE_METHOD.to_owned()),
        );
        assert_eq!(
            sequence,
            vec![
                controller_listen.clone(),
                controller_subscribe.clone(),
                controller_listen,
                controller_subscribe,
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
        assert_eq!(frames[4].body, json!([]));
        let client_hello = &frames[5].body[0];
        assert_eq!(client_hello["protocolVersion"], json!(3));
        assert_eq!(client_hello["clientKind"], json!("web"));
        assert_eq!(client_hello["kind"], json!("clientHello"));
        assert_ne!(client_hello["clientKind"], json!("desktop"));
        // 会话 EventListen 携带 workspace 参数（无 identity 时保持缺席）。
        let listen_arg = &frames[6].body;
        assert_eq!(listen_arg["workspacePath"], json!(WS_A));
        assert_eq!(listen_arg["workspaceIdentity"], json!("ws-a-identity"));
        assert_eq!(frames[7].body[0]["sessionId"], json!(SESSION_A));
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

    /// 宿主证据只来自本代际的真实握手/快照：bind 前未知，握手+快照后全面置位，
    /// 换代（invalidate）立即回到未知；已取证宿主策略仍全关。
    #[tokio::test]
    async fn host_evidence_tracks_real_handshake_and_resets_on_invalidate() {
        let host_build: HostBuildProbe = Arc::new(|| Some("3.16.2.8769".to_owned()));
        let (manager, _peer) = manager_with_host_build(basic_config(), host_build).await;

        let before = manager.host_evidence();
        assert_eq!(before.app_build.as_deref(), Some("3.16.2.8769"));
        assert_eq!(before.protocol_version, None);
        assert!(!before.hello_known);
        assert!(!before.schema_known);
        assert_eq!(
            before.policy(),
            CapabilityView::default(),
            "bind 前不得带宿主证据"
        );

        let target = manager.bind(&key(SESSION_A)).await.expect("bind");
        assert!(manager.session(&target).await.expect("session").ready);
        let after = manager.host_evidence();
        assert!(after.hello_known, "hello 校验通过必须置位");
        assert_eq!(
            after.protocol_version,
            Some(3),
            "协议版本取自校验过的 hello"
        );
        assert!(after.schema_known, "有效快照应用后 schema 必须置位");
        assert_eq!(
            after.policy(),
            CapabilityPolicy::for_host("3.16.2.8769", 3, true, true)
        );
        assert_eq!(
            after.policy(),
            CapabilityView::default(),
            "已知宿主仍不得开启生产能力"
        );

        manager.invalidate(2).await;
        let reset = manager.host_evidence();
        assert_eq!(reset.app_build.as_deref(), Some("3.16.2.8769"));
        assert_eq!(reset.protocol_version, None);
        assert!(!reset.hello_known);
        assert!(!reset.schema_known);
        assert_eq!(reset.policy(), CapabilityView::default());
    }

    /// 握手被拒（协议不符）不得留下 hello 证据；schema 未应用即不清零为 true。
    #[tokio::test]
    async fn rejected_hello_records_no_evidence() {
        let host_build: HostBuildProbe = Arc::new(|| Some("3.16.2.8769".to_owned()));
        let config = PeerConfig {
            hello_protocol: 2,
            ..basic_config()
        };
        let (manager, _peer) = manager_with_host_build(config, host_build).await;
        assert!(manager.bind(&key(SESSION_A)).await.is_err());
        let evidence = manager.host_evidence();
        assert!(!evidence.hello_known);
        assert_eq!(evidence.protocol_version, None);
        assert!(!evidence.schema_known);
    }

    /// 坏快照（CRC 故障）不算 schema 已识别：hello 已过但 schema_known 仍为 false。
    #[tokio::test]
    async fn schema_fault_does_not_mark_schema_known() {
        let host_build: HostBuildProbe = Arc::new(|| Some("3.16.2.8769".to_owned()));
        let config = PeerConfig {
            fragmented_snapshot: true,
            fragment_checksum_override: Some("00000000".to_owned()),
            conversation_snapshot: Some(conversation_snapshot(SESSION_A, 7, Vec::new())),
            ..single_workspace_config()
        };
        let (manager, _peer) = manager_with_host_build(config, host_build).await;
        manager.bind(&key(SESSION_A)).await.expect("bind");
        let evidence = manager.host_evidence();
        assert!(evidence.hello_known, "hello 本身已通过校验");
        assert!(!evidence.schema_known, "坏快照不得冒充 schema 已识别");
    }

    // =======================================================================
    // T3：接口形状采集（observe 只存形状布尔；invalidate 全清）
    // =======================================================================

    /// brief Step 1 逐字用例：pending 形状（multiSelect / isOther）→ 形状位。
    #[test]
    fn pending_shape_records_multi_and_free_text() {
        let mut ev = ZcodeInterfaceEvidence::default();
        ev.observe_pending(json!({
            "kind": "user_input",
            "questions": [{ "multiSelect": true, "isOther": true }]
        }));
        assert!(ev.has_user_input_pending);
        assert!(ev.has_multi_select);
        assert!(ev.has_free_text);
    }

    /// brief Step 1 逐字用例：invalidate 把形状位全部清空。
    #[test]
    fn invalidate_clears_interface_evidence() {
        let mut ev = ZcodeInterfaceEvidence::default();
        ev.method_send = true;
        ev.has_active_work = true;
        ev.invalidate();
        assert!(!ev.method_send);
        assert!(!ev.has_active_work);
    }

    /// 含真实感正文（题干/选项/命令/答案）的样本过 observe 后**只有形状位**变化：
    /// 与全量布尔面逐字段相等——本类型 12 个字段全是 bool，相等断言即证明
    /// 正文（题目、选项、命令、答案）无处可存（spec R3「只记录存在性与形状」）。
    #[test]
    fn observed_evidence_stays_shape_only_and_never_stores_content() {
        let mut ev = ZcodeInterfaceEvidence::default();
        // wire 卡形（kind 顶层 + payload 包装）携带机密正文：只允许点亮形状位。
        ev.observe_pending(json!({
            "interactionId": "pi-secret-1",
            "kind": "userInput",
            "payload": {
                "kind": "userInput",
                "prompt": "机密题干：请选择部署区域？",
                "freeText": true,
                "questions": [{
                    "question": "机密题干：请选择部署区域？",
                    "header": "Region",
                    "multiSelect": true,
                    "options": [{"value": "secret-option", "label": "机密选项"}]
                }],
                "options": [{"optionId": "allowOnce", "label": "机密审批项"}]
            }
        }));
        // snapshot 帧（logical frame 包装）携带机密命令与档位：同样只点亮形状位。
        ev.observe_conversation_frame(&json!({
            "topic": "conversation/sess_x",
            "subscriptionId": "sub-1",
            "fromSeq": 0u64,
            "toSeq": 1u64,
            "payload": {
                "kind": "snapshot",
                "snapshot": {
                    "control": {"activeWorks": [
                        {"kind": "primaryTurn", "foregroundExecutionId": "E-1", "command": "secret-cmd"}
                    ]},
                    "pendingInteractions": [],
                    "config": {
                        "provider": "secret-provider",
                        "model": "secret-model",
                        "thoughtLevels": ["secret-level"]
                    }
                }
            }
        }));
        assert_eq!(
            ev,
            ZcodeInterfaceEvidence {
                has_user_input_pending: true,
                has_multi_select: true,
                has_free_text: true,
                has_active_work: true,
                has_thought_levels: true,
                ..ZcodeInterfaceEvidence::default()
            },
            "证据必须只有形状位差异，正文字段不存在（12 个 bool 逐字段比对）"
        );
    }

    /// hello 方法存在性：显式方法表按成员判定；无表时只对已校验的 v4 协议面
    /// 背书（hello 校验通过才会走到 observe）；未知协议不背书。
    #[test]
    fn hello_records_method_existence() {
        // 显式方法表：成员判定，缺席方法保持 false。
        let mut ev = ZcodeInterfaceEvidence::default();
        ev.observe_hello(&json!({
            "kind": "hello",
            "protocolVersion": 3u64,
            "methods": ["sendConversationCommandV4", "resolveInteraction", "stop"]
        }));
        assert!(ev.method_send);
        assert!(ev.method_resolve_interaction);
        assert!(ev.method_stop);
        assert!(!ev.method_selection_get);
        assert!(!ev.method_selection_switch);

        // 无显式方法表：v4/3 协议面本身携带五条控制方法（合同钉源 + 实机探针）。
        let mut ev = ZcodeInterfaceEvidence::default();
        ev.observe_hello(&json!({
            "kind": "hello",
            "protocolVersion": 3u64,
            "connectionId": "synthetic-connection-0001"
        }));
        assert!(ev.method_send);
        assert!(ev.method_resolve_interaction);
        assert!(ev.method_stop);
        assert!(ev.method_selection_get);
        assert!(ev.method_selection_switch);

        // 非 v4 协议（未取证面）不得背书任何方法。
        let mut ev = ZcodeInterfaceEvidence::default();
        ev.observe_hello(&json!({"kind": "hello", "protocolVersion": 2u64}));
        assert!(!ev.method_send);
        assert!(!ev.method_resolve_interaction);
        assert!(!ev.method_stop);
        assert!(!ev.method_selection_get);
        assert!(!ev.method_selection_switch);
    }

    /// 桥级端到端：握手（v4 协议面方法位）+ 快照（问答卡/审批卡/扁平档位）+
    /// control 增量（activeWorks）+ 选择目录（getView）→ 形状位逐步置位；
    /// 换代（invalidate）全清。
    #[tokio::test]
    async fn bridge_collects_interface_evidence_and_invalidate_clears_it() {
        const PROVIDER: &str = "synthetic-provider";
        const SUB: &str = "conv-sub-synthetic-1";
        let mut snapshot = conversation_snapshot(
            SESSION_A,
            2,
            vec![
                ask_user_question_card("pi-single"),
                multi_select_card("pi-multi"),
                mixed_free_text_card("pi-free"),
                permission_card("pi-permission"),
            ],
        );
        snapshot["config"] = json!({
            "provider": PROVIDER,
            "model": "synthetic-model-a",
            "thought": "low",
            "thoughtLevels": ["low", "high"],
        });
        let config = PeerConfig {
            workspaces: vec![workspace_entry(WS_A, Some("ws-a-identity"), "A")],
            tasks: vec![task_row(WS_A, SESSION_A, "hi")],
            conversation_snapshot: Some(snapshot),
            model_selection_view: Some(json!({
                "providers": [
                    {"providerId": "acct-other", "models": [{"modelId": "other-1"}]},
                    {"providerId": PROVIDER, "models": [
                        {"modelId": "synthetic-model-a", "config": {"optionSpecs": {"reasoningLevel": {"values": ["low", "high"]}}}}
                    ]}
                ]
            })),
            ..PeerConfig::default()
        };
        let (manager, peer) = manager_with(config).await;

        let before = manager.host_evidence().interface_evidence();
        assert_eq!(
            before,
            ZcodeInterfaceEvidence::default(),
            "bind 前接口形状证据全空"
        );

        let target = manager.bind(&key(SESSION_A)).await.expect("bind");
        manager.session(&target).await.expect("session");
        let after_bind = manager.host_evidence().interface_evidence();
        // hello（v4 协议面）→ 五个方法位。
        assert!(after_bind.method_send, "v4 协议面 send 方法");
        assert!(
            after_bind.method_resolve_interaction,
            "v4 协议面 resolve 方法"
        );
        assert!(after_bind.method_stop, "v4 协议面 stop 方法");
        assert!(
            after_bind.method_selection_get,
            "v4 协议面 selection.get 方法"
        );
        assert!(
            after_bind.method_selection_switch,
            "v4 协议面 selection.switch 方法"
        );
        // 基线快照 → pending 形状 + 扁平档位；activeWorks 为空不置位。
        assert!(after_bind.has_user_input_pending, "问答卡在场");
        assert!(after_bind.has_multi_select, "multiSelect=true 的题在场");
        assert!(after_bind.has_free_text, "freeText=true 的卡在场");
        assert!(after_bind.has_permission_pending, "审批卡在场");
        assert!(after_bind.has_thought_levels, "快照扁平档位在场");
        assert!(!after_bind.has_active_work, "基线快照 activeWorks 为空");
        assert!(!after_bind.has_model_catalog, "尚未读目录");

        // control 增量：activeWorks 非空 → 打断形状。
        let active_works = json!([{"kind": "primaryTurn", "foregroundExecutionId": "E-1"}]);
        push_control_delta_on(&peer, SESSION_A, SUB, 1, 2, Some(3), active_works).await;
        let with_work = manager.host_evidence().interface_evidence();
        assert!(with_work.has_active_work, "运行回合在场");

        // 选择目录：getView 应答（只认当前 provider）→ 目录形状。
        let (_, selection) = SelectionPort::load(manager.as_ref(), &target)
            .await
            .expect("load");
        assert_eq!(selection.provider, PROVIDER);
        let with_catalog = manager.host_evidence().interface_evidence();
        assert!(with_catalog.has_model_catalog, "当前 provider 目录非空");

        // 换代（invalidate）：接口形状证据与握手证据一并全清。
        manager.invalidate(2).await;
        let reset = manager.host_evidence().interface_evidence();
        assert_eq!(reset, ZcodeInterfaceEvidence::default(), "换代后全清");
    }

    /// 其他账号 provider 的目录绝不背书 `has_model_catalog`（观察只认当前会话
    /// provider，与 `parse_selection_view` 的过滤纪律一致）。
    #[tokio::test]
    async fn selection_catalog_evidence_ignores_other_providers() {
        let config = PeerConfig {
            workspaces: vec![workspace_entry(WS_A, Some("ws-a-identity"), "A")],
            tasks: vec![task_row(WS_A, SESSION_A, "hi")],
            conversation_snapshot: Some(conversation_snapshot(SESSION_A, 2, Vec::new())),
            // 目录只有其他账号 provider（有模型条目）：
            model_selection_view: Some(json!({
                "providers": [
                    {"providerId": "acct-other", "models": [
                        {"modelId": "other-1", "config": {"optionSpecs": {"reasoningLevel": {"values": ["x"]}}}}
                    ]}
                ]
            })),
            ..PeerConfig::default()
        };
        let (manager, _peer) = manager_with(config).await;
        let target = manager.bind(&key(SESSION_A)).await.expect("bind");
        // load 成功（退化为当前模型 + 快照档位），但目录形状不背书其他账号。
        let (_, _selection) = SelectionPort::load(manager.as_ref(), &target)
            .await
            .expect("load");
        let evidence = manager.host_evidence().interface_evidence();
        assert!(
            !evidence.has_model_catalog,
            "其他账号 provider 的目录不得置位 has_model_catalog"
        );
        assert!(
            !evidence.has_thought_levels,
            "其他账号 provider 的档位不得置位 has_thought_levels"
        );
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
    async fn bridge_call_uses_bridge_generation() {
        let (manager, _peer) = manager_with(basic_config()).await;
        let bound = manager.bind(&key(SESSION_A)).await.expect("bind");
        let bridge_generation = bound.generation;

        // 公开代际被覆写、但没有 bridge_generation：桥按投影代际判旧 → stale。
        let mut public_only = bound.clone();
        public_only.generation = bridge_generation + 100;
        assert!(manager
            .call(
                &public_only,
                AGENT_CHANNEL,
                "syntheticRead",
                vec![json!({})],
            )
            .await
            .expect_err("公开代际不得冒充桥代际")
            .is(codes::STALE_GENERATION));

        // Hub 盖好的目标：公开代际分叉，桥内 call/session/精确绑定一律按 bridge_generation。
        let mut stamped = bound.clone();
        stamped.bridge_generation = Some(bridge_generation);
        stamped.generation = bridge_generation + 100;
        let body = manager
            .call(&stamped, AGENT_CHANNEL, "syntheticRead", vec![json!({})])
            .await
            .expect("按桥代际必须可读");
        assert_eq!(body, json!({"method": "syntheticRead"}));
        let view = manager
            .session(&stamped)
            .await
            .expect("按桥代际必须可读投影");
        assert_eq!(view.session_id, SESSION_A);
        assert!(
            manager.exact_session_bound(&stamped),
            "精确绑定复核按桥代际"
        );
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

    /// 换代后（Hub 的 Data-lag 失效/KICKED 冲突都走同一条 `invalidate` 路径）：
    /// 旧 `bridgeSessionId` 的迟到合法帧必须被忽略、不得复活旧投影；重新 bind 必须
    /// 开一条**新**桥（bridge id 单调递增）。
    #[tokio::test]
    async fn invalidate_drops_old_bridge_frames_and_rebind_uses_fresh_bridge() {
        let config = PeerConfig {
            // 重新订阅需要一份快照，重绑后才能恢复 ready。
            resync_snapshot: Some(conversation_snapshot(SESSION_A, 5, Vec::new())),
            ..basic_config()
        };
        let (manager, peer) = manager_with(config).await;
        let _first = manager.bind(&key(SESSION_A)).await.expect("bind");
        let old_bridge = peer.session_bridge_id(SESSION_A);
        assert!(
            manager.peek_view(SESSION_A).expect("bound").ready,
            "初始快照已就绪"
        );

        // Hub Data 溢出（apply_relay_lag）与 KICKED 冲突都推进投影代际并作废旧桥。
        manager.invalidate(2).await;
        assert!(manager.peek_view(SESSION_A).is_none(), "换代必须释放旧投影");

        // 旧桥的迟到快照（revision 99）：若旧桥未被作废，它会把投影复活成 revision 99。
        let stale = encode_frame(
            &RpcIdentity::new(&old_bridge),
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
        .expect("stale frame");
        manager
            .on_data(stale)
            .await
            .expect("旧 bridgeSessionId 按未知桥忽略");
        assert!(
            manager.peek_view(SESSION_A).is_none(),
            "旧 bridgeSessionId 的迟到帧不得复活投影"
        );

        let second = manager.bind(&key(SESSION_A)).await.expect("rebind");
        let new_bridge = peer.session_bridge_id(SESSION_A);
        assert_ne!(new_bridge, old_bridge, "重新 bind 必须开新桥");
        assert!(
            bridge_ordinal(&new_bridge) > bridge_ordinal(&old_bridge),
            "新 bridge id 必须单调递增（{old_bridge} -> {new_bridge}）"
        );
        let view = manager.session(&second).await.expect("session");
        assert!(view.ready, "重绑后由 resync 快照恢复 ready");
        assert_eq!(view.revision, Some(5), "新投影来自新桥的快照");
    }

    fn bridge_ordinal(bridge_id: &str) -> u32 {
        u32::from_str_radix(
            bridge_id
                .strip_prefix("tuvis-bridge-")
                .expect("bridge id prefix"),
            16,
        )
        .expect("bridge id hex")
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

    // =======================================================================
    // Fix round 1
    // =======================================================================

    async fn wait_until(mut condition: impl FnMut() -> bool, what: &str) {
        for _ in 0..1000 {
            if condition() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        panic!("等待条件超时：{what}");
    }

    async fn manager_with_limits(
        config: PeerConfig,
        limits: BridgeLimits,
    ) -> (Arc<BridgeManager>, Arc<Peer>) {
        let peer = Peer::new(config);
        let manager = Arc::new(BridgeManager::with_timing_limits(
            peer.clone(),
            BridgeTiming::test(),
            limits,
        ));
        peer.attach(&manager);
        manager.invalidate(1).await;
        (manager, peer)
    }

    fn single_workspace_config() -> PeerConfig {
        PeerConfig {
            workspaces: vec![workspace_entry(WS_A, Some("ws-a-identity"), "A")],
            tasks: vec![task_row(WS_A, SESSION_A, "hi")],
            conversation_snapshot: Some(conversation_snapshot(SESSION_A, 2, Vec::new())),
            ..PeerConfig::default()
        }
    }

    // -----------------------------------------------------------------------
    // C1：应用级 conversation 分片重组
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn conversation_topic_fragments_reassembled() {
        let config = PeerConfig {
            fragmented_snapshot: true,
            conversation_snapshot: Some(conversation_snapshot(
                SESSION_A,
                7,
                vec![permission_card("pi-synthetic-fragment-1")],
            )),
            ..single_workspace_config()
        };
        let (manager, _peer) = manager_with(config).await;
        let target = manager.bind(&key(SESSION_A)).await.expect("bind");
        let view = manager.session(&target).await.expect("session");
        assert!(view.ready);
        assert_eq!(view.revision, Some(7));
        assert_eq!(view.pending.len(), 1);
        assert_eq!(view.pending[0].interaction_id, "pi-synthetic-fragment-1");
        let stats = manager.session_stats(SESSION_A).expect("stats");
        assert!(stats.fragments_reassembled >= 1);
        assert_eq!(stats.fragment_faults, 0);
        assert_eq!(stats.resyncs, 0);
    }

    /// 物理层回归（复审 #1）：同一条逻辑消息的所有物理分片共享 `messageSeq`，
    /// 只有物理 `seq` 逐片递增。旧实现把「`messageSeq` 不大于水位」整帧丢弃，
    /// 且首片到达就推进水位 → 第二片起被误判为重复，整条消息永远无法重组。
    #[tokio::test]
    async fn physical_fragments_sharing_message_seq_are_both_delivered() {
        const SUBSCRIPTION: &str = "conv-sub-synthetic-1";
        let config = PeerConfig {
            // 初始 bind 不下发快照：下面手工注入一条完整的物理分片消息。
            conversation_snapshot: None,
            ..single_workspace_config()
        };
        let (manager, peer) = manager_with(config).await;
        manager.bind(&key(SESSION_A)).await.expect("bind");
        let bridge = peer.session_bridge_id(SESSION_A);
        let listen = peer.listen_id(&bridge);
        let topic = format!("{CONVERSATION_TOPIC_PREFIX}{SESSION_A}");

        // 完整 logical snapshot（revision=42，带一张权限卡）。
        let wire = test_wire(
            &topic,
            SUBSCRIPTION,
            json!({
                "kind": "snapshot",
                "snapshot": conversation_snapshot(
                    SESSION_A,
                    42,
                    vec![permission_card("pi-physical-frag-1")],
                ),
            }),
        );
        // 一条 Channel 消息 = 一条逻辑消息，在**物理层**按字节切成两片：
        // 两片共享 messageSeq，fragmentIndex 0/1，物理 seq 连续递增。
        let channel = channel_message(json!([RESPONSE_TYPE_EVENT_FIRE, listen]), &wire);
        assert!(
            channel.len() > 1,
            "fixture must split into two non-empty chunks"
        );
        let split = (channel.len() / 2).max(1);
        let chunks = [&channel[..split], &channel[split..]];

        // 取桥当前已见水位：两片都必须严格大于它（否则会被物理 seq 去重挡掉）。
        let (seq_base, message_seq) = {
            let inner = lock(&manager.shared.state);
            let bridge_state = inner.bridges.get(&bridge).expect("bridge registered");
            (
                bridge_state.last_seq.unwrap_or(0),
                bridge_state.last_message_seq.saturating_add(1),
            )
        };
        let template = encode_frame(
            &RpcIdentity::new(&bridge),
            seq_base + 1,
            message_seq,
            &channel,
        )
        .expect("encode physical frame");
        let mut payloads = Vec::new();
        for (index, chunk) in chunks.iter().enumerate() {
            let mut payload = template.clone();
            payload["seq"] = json!(seq_base + 1 + index as u64);
            payload["fragmentIndex"] = json!(index);
            payload["fragmentCount"] = json!(2);
            payload["messageBytes"] = json!(channel.len());
            payload["checksum"]["value"] = json!(crc32_hex(&channel));
            payload["dataBase64"] = json!(base64::engine::general_purpose::STANDARD.encode(chunk));
            payloads.push(payload);
        }

        // 该逻辑消息（同桥同 messageSeq）的 ACK 计数：一条逻辑消息只允许一次。
        let acks = |peer: &Peer| {
            peer.sent()
                .into_iter()
                .filter(|payload| {
                    payload.get("zcode_type").and_then(Value::as_str) == Some("rpc-frame-ack")
                        && payload.get("bridgeSessionId").and_then(Value::as_str)
                            == Some(bridge.as_str())
                        && payload.get("ackMessageSeq").and_then(Value::as_u64) == Some(message_seq)
                })
                .count()
        };

        // 第一片只进入物理重组：半条消息绝不进册，也不得 ACK。
        manager
            .on_data(payloads[0].clone())
            .await
            .expect("first physical fragment accepted");
        assert!(
            !manager.peek_view(SESSION_A).expect("bound").ready,
            "half a logical message must not be ready"
        );
        // 让出若干次：若实现错误地对首片发 ACK，必须在这里可见。
        for _ in 0..3 {
            tokio::task::yield_now().await;
        }
        assert_eq!(acks(&peer), 0, "half a logical message must not be acked");

        // 第二片共享同一 messageSeq：旧实现会在这里静默丢弃。
        manager
            .on_data(payloads[1].clone())
            .await
            .expect("second physical fragment accepted");
        let view = manager.peek_view(SESSION_A).expect("bound");
        assert!(view.ready, "both physical fragments must be reassembled");
        assert_eq!(view.revision, Some(42));
        assert_eq!(view.pending.len(), 1);
        assert_eq!(view.pending[0].interaction_id, "pi-physical-frag-1");

        // 先等第一条 ACK 出现，再留稳定窗：若误发第二条（例如首片也 ACK），
        // 它必须在稳定窗内到达并被下面的精确计数抓住。
        wait_until(|| acks(&peer) >= 1, "logical message ack").await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            acks(&peer),
            1,
            "one logical message must be acked exactly once"
        );
        assert_eq!(
            manager
                .session_stats(SESSION_A)
                .expect("stats")
                .fragment_faults,
            0,
            "sharing messageSeq is not a fragment fault"
        );
    }

    #[tokio::test]
    async fn fragment_crc_fault_goes_unready() {
        let config = PeerConfig {
            fragmented_snapshot: true,
            fragment_checksum_override: Some("00000000".to_owned()),
            conversation_snapshot: Some(conversation_snapshot(SESSION_A, 7, Vec::new())),
            resync_snapshot: Some(conversation_snapshot(
                SESSION_A,
                26,
                vec![permission_card("pi-synthetic-after-fragment-fault")],
            )),
            ..single_workspace_config()
        };
        let (manager, _peer) = manager_with(config).await;
        let target = manager.bind(&key(SESSION_A)).await.expect("bind");
        let stale = manager.peek_view(SESSION_A).expect("bound");
        assert!(!stale.ready, "坏 checksum 不得进入 ready");
        assert_eq!(stale.revision, None);
        assert!(
            manager
                .session_stats(SESSION_A)
                .expect("stats")
                .fragment_faults
                >= 1
        );

        // 读面触发一次只读重同步（重同步快照为完整帧）。
        let recovered = manager.session(&target).await.expect("session");
        assert!(recovered.ready);
        assert_eq!(recovered.revision, Some(26));
        assert_eq!(recovered.pending.len(), 1);
        assert_eq!(manager.session_stats(SESSION_A).expect("stats").resyncs, 1);
    }

    #[tokio::test]
    async fn fragment_gap_and_duplicate_are_handled() {
        // 缺片（只有 index 0）：不得交付、不得 ready。
        let gap_config = PeerConfig {
            fragmented_snapshot: true,
            fragment_order: vec![0],
            conversation_snapshot: Some(conversation_snapshot(SESSION_A, 9, Vec::new())),
            ..single_workspace_config()
        };
        let (gap_manager, _gap_peer) = manager_with(gap_config).await;
        let gap_target = gap_manager.bind(&key(SESSION_A)).await.expect("bind");
        let gap_view = gap_manager.peek_view(SESSION_A).expect("bound");
        assert!(!gap_view.ready);
        assert_eq!(gap_view.revision, None);
        // 重订阅后仍无快照（fake 不下发）→ 保持不新鲜，绝不带半帧 ready。
        assert!(
            !gap_manager
                .session(&gap_target)
                .await
                .expect("session view")
                .ready
        );

        // 乱序 + 重复片（1,0,1）：仍应恰好重组一次并进册。
        let config = PeerConfig {
            fragmented_snapshot: true,
            fragment_order: vec![1, 0, 1],
            conversation_snapshot: Some(conversation_snapshot(
                SESSION_A,
                7,
                vec![permission_card("pi-synthetic-fragment-out-of-order")],
            )),
            ..single_workspace_config()
        };
        let (manager, _peer) = manager_with(config).await;
        let target = manager.bind(&key(SESSION_A)).await.expect("bind");
        let view = manager.session(&target).await.expect("session");
        assert!(view.ready);
        assert_eq!(view.pending.len(), 1);
        assert_eq!(
            manager
                .session_stats(SESSION_A)
                .expect("stats")
                .fragment_faults,
            0
        );
    }

    #[tokio::test]
    async fn resync_backoff_prevents_tight_loop() {
        let config = PeerConfig {
            fragmented_snapshot: true,
            fragmented_resync: true,
            fragment_checksum_override: Some("00000000".to_owned()),
            conversation_snapshot: Some(conversation_snapshot(SESSION_A, 7, Vec::new())),
            resync_snapshot: Some(conversation_snapshot(SESSION_A, 8, Vec::new())),
            ..single_workspace_config()
        };
        let (manager, peer) = manager_with(config).await;
        let target = manager.bind(&key(SESSION_A)).await.expect("bind");
        assert!(!manager.peek_view(SESSION_A).expect("bound").ready);

        // 第一次读：允许一次重订阅。
        assert!(manager.session(&target).await.is_err());
        let subscribes = |peer: &Peer| {
            channel_frames(peer)
                .into_iter()
                .filter(|frame| {
                    frame.method.as_deref() == Some(AGENT_SUBSCRIBE_CONVERSATION_METHOD)
                })
                .count()
        };
        assert_eq!(subscribes(&peer), 2);

        // 冷却窗内的第二次读：不得再发订阅（无 tight loop）。
        assert!(manager
            .session(&target)
            .await
            .expect_err("backoff")
            .is(codes::NOT_READY));
        assert_eq!(subscribes(&peer), 2);
        assert_eq!(manager.session_stats(SESSION_A).expect("stats").resyncs, 1);

        // 冷却窗过去后允许下一次重订阅。
        tokio::time::sleep(BridgeTiming::test().resync_backoff + Duration::from_millis(30)).await;
        assert!(manager.session(&target).await.is_err());
        assert_eq!(subscribes(&peer), 3);
        assert_eq!(manager.session_stats(SESSION_A).expect("stats").resyncs, 2);
    }

    // -----------------------------------------------------------------------
    // controller 索引失效（spec I1 / quality I-2）
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn controller_gap_reindexes_before_binding() {
        let config = PeerConfig {
            workspaces: vec![workspace_entry(WS_A, Some("ws-a-identity"), "A")],
            tasks: vec![task_row(WS_A, SESSION_A, "hi")],
            controller_resubscribe_tasks: Some(vec![task_row(
                WS_A,
                "sess_0f9a1c0d-0000-4000-8000-0000000000c3",
                "hi",
            )]),
            ..PeerConfig::default()
        };
        let (manager, peer) = manager_with(config).await;
        assert!(manager.bind(&key(SESSION_A)).await.is_ok());
        let enum_bridge = peer.enum_bridge_ids()[0].clone();

        // controller 物理缺口（缺 seq）：必须让索引失效，而不是继续用旧行。
        let gap = json!({"zcode_type": "rpc-frame", "bridgeSessionId": enum_bridge});
        assert!(manager.on_data(gap).await.is_err());

        // 旧行不得再被当成可绑定目标。
        let stale = manager
            .bind(&key("sess_0f9a1c0d-0000-4000-8000-0000000000c3"))
            .await
            .expect("fresh reindex finds new row");
        assert_eq!(stale.workspace_path, WS_A);
        let controller_subscribes = channel_frames(&peer)
            .into_iter()
            .filter(|frame| frame.method.as_deref() == Some(CONTROLLER_SUBSCRIBE_METHOD))
            .count();
        assert!(controller_subscribes >= 2, "缺口后必须重新订阅任务索引");
        let rows = peer.controller_rows(&enum_bridge);
        assert_eq!(
            rows,
            vec!["sess_0f9a1c0d-0000-4000-8000-0000000000c3".to_owned()],
            "fresh snapshot replaces stale rows"
        );
    }

    /// 单枚举桥的物理缺口只允许让**本 workspace** 的任务行失效：其它 workspace
    /// 的索引必须继续可用（N1），缺口桥随后自愈重订阅。
    #[tokio::test]
    async fn enum_bridge_gap_keeps_other_workspace_rows() {
        let session_b_extra = "sess_0f9a1c0d-0000-4000-8000-0000000000b2";
        let session_a_extra = "sess_0f9a1c0d-0000-4000-8000-0000000000a2";
        let config = PeerConfig {
            workspaces: vec![
                workspace_entry(WS_A, Some("ws-a-identity"), "A"),
                workspace_entry(WS_B, Some("ws-b-identity"), "B"),
            ],
            tasks: vec![
                task_row(WS_A, SESSION_A, "hi"),
                task_row(WS_B, SESSION_B, "hi"),
                task_row(WS_B, session_b_extra, "hi"),
            ],
            controller_resubscribe_tasks: Some(vec![
                task_row(WS_A, SESSION_A, "hi"),
                task_row(WS_A, session_a_extra, "hi"),
                task_row(WS_B, SESSION_B, "hi"),
                task_row(WS_B, session_b_extra, "hi"),
            ]),
            ..PeerConfig::default()
        };
        let (manager, peer) = manager_with(config).await;
        assert!(manager.bind(&key(SESSION_A)).await.is_ok());
        assert_eq!(manager.task_rows(), 3, "两枚举桥的行都已入索引");

        let enum_bridges = peer.enum_bridge_ids();
        assert_eq!(enum_bridges.len(), 2);
        let bridge_a = enum_bridges
            .iter()
            .find(|bridge| peer.controller_rows(bridge).contains(&SESSION_A.to_owned()))
            .expect("workspace A enum bridge")
            .clone();
        let bridge_b = enum_bridges
            .iter()
            .find(|bridge| *bridge != &bridge_a)
            .expect("workspace B enum bridge")
            .clone();
        assert!(peer
            .controller_rows(&bridge_b)
            .contains(&SESSION_B.to_owned()));

        // A 的枚举桥物理缺口：只清 A 的行，B 的行保持新鲜。
        let gap = json!({"zcode_type": "rpc-frame", "bridgeSessionId": bridge_a});
        assert!(manager.on_data(gap).await.is_err());
        assert_eq!(
            manager.task_rows(),
            2,
            "缺口只允许失效本 workspace 的行（N1）"
        );

        // B 的 exact taskId（尚未绑定）在无换代的情况下仍可绑定。
        let b_extra = manager
            .bind(&key(session_b_extra))
            .await
            .expect("B row survives A gap");
        assert_eq!(b_extra.workspace_path, WS_B);

        // A 的枚举桥在同一次 load_tasks 中重订阅并恢复，新行可绑定。
        let a_extra = manager
            .bind(&key(session_a_extra))
            .await
            .expect("A reindex recovers");
        assert_eq!(a_extra.workspace_path, WS_A);

        // 只有 A 桥发生第二次 controller 订阅；B 桥不受波及。
        let subscribes_for = |bridge: &str| {
            peer.sent()
                .into_iter()
                .filter(|payload| {
                    payload.get("zcode_type").and_then(Value::as_str) == Some("rpc-frame")
                        && payload.get("bridgeSessionId").and_then(Value::as_str) == Some(bridge)
                })
                .filter(|payload| {
                    decode_outbound(payload).method.as_deref() == Some(CONTROLLER_SUBSCRIBE_METHOD)
                })
                .count()
        };
        assert_eq!(subscribes_for(&bridge_a), 2);
        assert_eq!(subscribes_for(&bridge_b), 1);
    }

    /// controller 收帧窗错过（订阅 ACK 到了但快照没来）不得永久闩死：
    /// 退避窗过后允许一次新的 subscribeControllerV4，快照到达即恢复绑定。
    #[tokio::test]
    async fn controller_snapshot_timeout_resubscribes() {
        let config = PeerConfig {
            workspaces: vec![workspace_entry(WS_A, Some("ws-a-identity"), "A")],
            tasks: Vec::new(),
            controller_snapshot_from_attempt: 2,
            controller_resubscribe_tasks: Some(vec![task_row(WS_A, SESSION_A, "hi")]),
            ..PeerConfig::default()
        };
        let (manager, peer) = manager_with(config).await;
        let controller_subscribes = || {
            channel_frames(&peer)
                .into_iter()
                .filter(|frame| frame.method.as_deref() == Some(CONTROLLER_SUBSCRIBE_METHOD))
                .count()
        };

        // 首次订阅收到 ACK 但收帧窗内无快照 → 本次绑定无行可用。
        let first = manager
            .bind(&key(SESSION_A))
            .await
            .expect_err("首窗无快照不得绑定成功");
        assert!(first.is(codes::NO_SESSION));
        assert_eq!(controller_subscribes(), 1);

        // 退避窗内不得立刻重订阅（防 tight loop），但也不得永久闩死。
        let blocked = manager
            .bind(&key("sess_0f9a1c0d-0000-4000-8000-00000000dead"))
            .await
            .expect_err("退避窗内没有新行");
        assert!(blocked.is(codes::NO_SESSION));
        assert_eq!(controller_subscribes(), 1, "退避窗内不得重订阅");

        tokio::time::sleep(BridgeTiming::test().controller_backoff + Duration::from_millis(30))
            .await;
        let target = manager
            .bind(&key(SESSION_A))
            .await
            .expect("退避窗过后必须允许重订阅并恢复");
        assert_eq!(target.workspace_path, WS_A);
        assert_eq!(target.generation, 1, "恢复不得依赖换代");
        assert_eq!(controller_subscribes(), 2);
    }

    #[tokio::test]
    async fn task_rows_are_capped_per_contract() {
        let mut tasks = Vec::new();
        for index in 0..MAX_TASK_ROWS {
            tasks.push(task_row(
                WS_A,
                &format!("sess_0f9a1c0d-0000-4000-8000-{index:012}"),
                "hi",
            ));
        }
        let overflow = "sess_0f9a1c0d-0000-4000-8000-00000000ffff";
        tasks.push(task_row(WS_A, overflow, "hi"));
        let config = PeerConfig {
            workspaces: vec![workspace_entry(WS_A, Some("ws-a-identity"), "A")],
            tasks,
            ..PeerConfig::default()
        };
        let (manager, _peer) = manager_with(config).await;
        assert_eq!(
            manager
                .bind(&key(overflow))
                .await
                .expect_err("beyond 1024 rows")
                .code,
            codes::NO_SESSION
        );
        assert_eq!(manager.task_rows(), MAX_TASK_ROWS);
        assert!(manager.task_rows_rejected() >= 1);
    }

    // -----------------------------------------------------------------------
    // 多 workspace：fake 按桥作用域（quality I-2 / spec I2）
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn workspace_scope_isolated_per_bridge() {
        let config = PeerConfig {
            active_workspace_key: Some("ws-b-identity".to_owned()),
            workspaces: vec![
                workspace_entry(WS_A, Some("ws-a-identity"), "A"),
                workspace_entry(WS_B, Some("ws-b-identity"), "B"),
            ],
            tasks: vec![
                task_row(WS_A, SESSION_A, "hi"),
                task_row(WS_B, SESSION_B, "hi"),
            ],
            ..PeerConfig::default()
        };
        let (manager, peer) = manager_with(config).await;
        let a = manager.bind(&key(SESSION_A)).await.expect("bind A");
        let b = manager.bind(&key(SESSION_B)).await.expect("bind B");
        assert_eq!(a.workspace_path, WS_A);
        assert_eq!(b.workspace_path, WS_B);

        // 每个枚举桥只看到自己 workspace 的行（fake 不再抹平作用域）。
        let enum_bridges = peer.enum_bridge_ids();
        assert_eq!(enum_bridges.len(), 2);
        let mut seen: Vec<(String, String)> = Vec::new();
        for bridge in enum_bridges {
            let path = if peer
                .controller_rows(&bridge)
                .contains(&SESSION_A.to_owned())
            {
                WS_A
            } else {
                WS_B
            };
            let rows = peer.controller_rows(&bridge);
            assert!(
                rows.iter().all(|row| {
                    (path == WS_A && row == SESSION_A) || (path == WS_B && row == SESSION_B)
                }),
                "bridge rows must stay in one workspace: {rows:?}"
            );
            seen.push((path.to_owned(), bridge));
        }
        assert_eq!(seen.len(), 2);
    }

    // -----------------------------------------------------------------------
    // Actor 内复核 ready（quality I-4）
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn gap_between_check_and_send_writes_zero_frames() {
        let (manager, peer) = manager_with(single_workspace_config()).await;
        let target = manager.bind(&key(SESSION_A)).await.expect("bind");
        let session_bridge = peer.session_bridge_id(SESSION_A);

        let gate = peer.hold_next_send();
        let first = {
            let manager = manager.clone();
            let target = target.clone();
            tokio::spawn(async move {
                manager
                    .call(&target, AGENT_CHANNEL, "firstWrite", vec![json!({})])
                    .await
            })
        };
        wait_until(|| peer.stalled(), "actor stalled on first write").await;

        let second = {
            let manager = manager.clone();
            let target = target.clone();
            tokio::spawn(async move {
                manager
                    .call(&target, AGENT_CHANNEL, "syntheticWrite", vec![json!({})])
                    .await
            })
        };
        wait_until(
            || manager.pending_count() >= 2,
            "second call passed precheck",
        )
        .await;

        // 预检之后、actor 写之前注入物理缺口（session 置 not ready）。
        let gap = json!({"zcode_type": "rpc-frame", "bridgeSessionId": session_bridge});
        assert!(manager.on_data(gap).await.is_err());
        gate.add_permits(1);

        let _ = first.await.expect("first task");
        let error = second
            .await
            .expect("second task")
            .expect_err("gap must zero the write");
        assert!(error.is(codes::NOT_READY));
        let writes = channel_frames(&peer)
            .into_iter()
            .filter(|frame| frame.method.as_deref() == Some("syntheticWrite"))
            .count();
        assert_eq!(writes, 0);
        assert!(manager.pending_count() <= 1);
    }

    // -----------------------------------------------------------------------
    // 字节与会话数上限（quality I-5）
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn staged_bytes_are_bounded() {
        let mut card = permission_card("pi-synthetic-huge");
        card["payload"]["summary"] = json!("x".repeat(4096));
        let config = PeerConfig {
            pre_ack_cards: vec![card],
            conversation_snapshot: None,
            ..single_workspace_config()
        };
        let limits = BridgeLimits {
            staged_bytes: 1024,
            ..BridgeLimits::test()
        };
        let (manager, _peer) = manager_with_limits(config, limits).await;
        let _target = manager.bind(&key(SESSION_A)).await.expect("bind");
        let stats = manager.session_stats(SESSION_A).expect("stats");
        assert_eq!(stats.staged_delivered, 0, "超字节预算的暂存帧必须拒收");
        assert!(stats.frames_rejected >= 1);
        assert!(!manager.peek_view(SESSION_A).expect("bound").ready);
    }

    #[tokio::test]
    async fn session_cap_refuses_further_binds() {
        let limits = BridgeLimits {
            sessions: 1,
            ..BridgeLimits::test()
        };
        let config = PeerConfig {
            workspaces: vec![
                workspace_entry(WS_A, Some("ws-a-identity"), "A"),
                workspace_entry(WS_B, Some("ws-b-identity"), "B"),
            ],
            tasks: vec![
                task_row(WS_A, SESSION_A, "hi"),
                task_row(WS_B, SESSION_B, "hi"),
            ],
            ..PeerConfig::default()
        };
        let (manager, _peer) = manager_with_limits(config, limits).await;
        assert!(manager.bind(&key(SESSION_A)).await.is_ok());
        assert_eq!(
            manager
                .bind(&key(SESSION_B))
                .await
                .expect_err("session cap")
                .code,
            codes::PAYLOAD_TOO_LARGE
        );
    }

    // -----------------------------------------------------------------------
    // 有界命令队列（quality I-6）
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn full_command_queue_returns_offline() {
        let limits = BridgeLimits {
            command_queue: 1,
            ..BridgeLimits::test()
        };
        let (manager, peer) = manager_with_limits(single_workspace_config(), limits).await;
        let target = manager.bind(&key(SESSION_A)).await.expect("bind");

        let gate = peer.hold_next_send();
        let first = {
            let manager = manager.clone();
            let target = target.clone();
            tokio::spawn(async move {
                manager
                    .call(&target, AGENT_CHANNEL, "firstWrite", vec![json!({})])
                    .await
            })
        };
        wait_until(|| peer.stalled(), "actor stalled").await;

        let second = {
            let manager = manager.clone();
            let target = target.clone();
            tokio::spawn(async move {
                manager
                    .call(&target, AGENT_CHANNEL, "secondWrite", vec![json!({})])
                    .await
            })
        };
        let third = {
            let manager = manager.clone();
            let target = target.clone();
            tokio::spawn(async move {
                manager
                    .call(&target, AGENT_CHANNEL, "thirdWrite", vec![json!({})])
                    .await
            })
        };
        // 队列满的一方必须立即拿到真实的 offline（不是伪造 not_ready）。
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            second.is_finished() || third.is_finished(),
            "队列满必须立即失败"
        );
        gate.add_permits(1);

        let first = first.await.expect("first task");
        let second = second.await.expect("second task");
        let third = third.await.expect("third task");
        assert!(first.is_ok(), "in-flight 写应在放行后完成");
        let errors: Vec<_> = [&second, &third]
            .into_iter()
            .filter_map(|result| result.as_ref().err())
            .collect();
        assert_eq!(errors.len(), 1, "恰有一个排队命令被拒");
        assert!(errors[0].is(codes::OFFLINE));
        let written = channel_frames(&peer)
            .into_iter()
            .filter(|frame| {
                matches!(
                    frame.method.as_deref(),
                    Some("firstWrite" | "secondWrite" | "thirdWrite")
                )
            })
            .count();
        assert_eq!(written, 2, "failed command zero writes");
    }

    // -----------------------------------------------------------------------
    // 订阅释放（quality I-3）
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn invalidate_releases_conversation_and_controller_subscriptions() {
        let (manager, peer) = manager_with(single_workspace_config()).await;
        let _target = manager.bind(&key(SESSION_A)).await.expect("bind");
        assert!(manager.peek_view(SESSION_A).is_some());

        manager.invalidate(2).await;
        wait_until(
            || {
                channel_frames(&peer)
                    .iter()
                    .any(|frame| frame.method.as_deref() == Some(CONTROLLER_UNSUBSCRIBE_METHOD))
            },
            "controller unsubscribe",
        )
        .await;

        let frames = channel_frames(&peer);
        let conversation = frames
            .iter()
            .position(|frame| {
                frame.method.as_deref() == Some(AGENT_UNSUBSCRIBE_CONVERSATION_METHOD)
            })
            .expect("conversation unsubscribe");
        let controller = frames
            .iter()
            .position(|frame| frame.method.as_deref() == Some(CONTROLLER_UNSUBSCRIBE_METHOD))
            .expect("controller unsubscribe");
        assert!(
            conversation < controller,
            "先释放会话订阅，再释放 controller"
        );
        assert_eq!(frames[conversation].channel.as_deref(), Some(AGENT_CHANNEL));
        assert_eq!(
            frames[conversation].body,
            json!([{"subscriptionId": "conv-sub-synthetic-1"}])
        );
        assert_eq!(
            frames[controller].channel.as_deref(),
            Some(WINDOW_CONTROLLER_CHANNEL)
        );
        assert_eq!(
            frames[controller].body,
            json!([{"subscriptionId": "ctrl-sub-synthetic-2"}])
        );
        assert!(manager.peek_view(SESSION_A).is_none(), "投影随代际释放");
    }

    #[tokio::test]
    async fn resync_unsubscribes_old_subscription_first() {
        let config = PeerConfig {
            gap_after_frames: Some(4),
            resync_snapshot: Some(conversation_snapshot(SESSION_A, 11, Vec::new())),
            ..single_workspace_config()
        };
        let (manager, peer) = manager_with(config).await;
        let target = manager.bind(&key(SESSION_A)).await.expect("bind");
        assert!(!manager.peek_view(SESSION_A).expect("bound").ready);
        assert!(manager.session(&target).await.expect("session").ready);

        let methods: Vec<String> = channel_frames(&peer)
            .into_iter()
            .filter(|frame| frame.channel.as_deref() == Some(AGENT_CHANNEL))
            .filter_map(|frame| frame.method)
            .collect();
        let unsubscribe = methods
            .iter()
            .position(|method| method == AGENT_UNSUBSCRIBE_CONVERSATION_METHOD)
            .expect("unsubscribe emitted");
        let resubscribe = methods
            .iter()
            .enumerate()
            .filter(|(index, method)| {
                *index > unsubscribe && *method == AGENT_SUBSCRIBE_CONVERSATION_METHOD
            })
            .map(|(index, _)| index)
            .next()
            .expect("resubscribe");
        assert!(unsubscribe < resubscribe, "退订必须先于重订阅");
    }

    // -----------------------------------------------------------------------
    // 测试缺口（quality I-7）
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn rpc_timeout_clears_pending_and_late_reply_is_ignored() {
        let config = PeerConfig {
            silent_methods: vec!["silentRead".to_owned()],
            ..single_workspace_config()
        };
        let (manager, peer) = manager_with(config).await;
        let target = manager.bind(&key(SESSION_A)).await.expect("bind");
        let session_bridge = peer.session_bridge_id(SESSION_A);

        assert!(manager
            .call(&target, AGENT_CHANNEL, "silentRead", vec![json!({})])
            .await
            .expect_err("timeout")
            .is(codes::NOT_READY));
        assert_eq!(manager.pending_count(), 0, "超时必须清账");

        let late_id = peer.last_request_id("silentRead").expect("request id");
        peer.inject_response(&session_bridge, late_id, &json!({"late": true}))
            .await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(manager.pending_count(), 0, "迟到应答不得复活 waiter");

        // 后续调用不受迟到 id 影响。
        let body = manager
            .call(&target, AGENT_CHANNEL, "syntheticRead", vec![json!({})])
            .await
            .expect("read");
        assert_eq!(body, json!({"method": "syntheticRead"}));
    }

    #[tokio::test]
    async fn duplicate_response_after_success_is_ignored() {
        let (manager, peer) = manager_with(single_workspace_config()).await;
        let target = manager.bind(&key(SESSION_A)).await.expect("bind");
        let session_bridge = peer.session_bridge_id(SESSION_A);
        let id = {
            let body = manager
                .call(&target, AGENT_CHANNEL, "syntheticRead", vec![json!({})])
                .await
                .expect("read");
            assert_eq!(body, json!({"method": "syntheticRead"}));
            peer.last_request_id("syntheticRead").expect("id")
        };
        // 重复应答（同一 requestId，新物理 seq）：静默丢弃。
        peer.inject_response(&session_bridge, id, &json!({"duplicate": true}))
            .await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(manager.pending_count(), 0);
        assert!(manager
            .call(&target, AGENT_CHANNEL, "syntheticRead2", vec![])
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn request_ids_are_unique_under_concurrency() {
        let (manager, peer) = manager_with(single_workspace_config()).await;
        let target = manager.bind(&key(SESSION_A)).await.expect("bind");
        let calls = ["readA", "readB", "readC", "readD"].map(|method| {
            let manager = manager.clone();
            let target = target.clone();
            tokio::spawn(async move {
                manager
                    .call(&target, AGENT_CHANNEL, method, vec![json!({})])
                    .await
                    .map(|body| (method.to_owned(), body))
            })
        });
        let mut ids = Vec::new();
        for call in calls {
            let (method, body) = call.await.expect("task").expect("call");
            assert_eq!(body, json!({"method": method}));
            ids.extend(peer.request_ids(&method));
        }
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 4, "并发 requestId 必须唯一");
        assert_eq!(peer.order_faults(), 0, "seq/messageSeq 必须严格递增");
    }

    // =======================================================================
    // T4.1 Slice B1：pending 授权与 snooze 端口（合成夹具，不读真实账号/DB）
    // =======================================================================

    /// 问答卡（AskUserQuestion；题干/选项全为合成值）。
    fn ask_user_question_card(interaction_id: &str) -> Value {
        json!({
            "interactionId": interaction_id,
            "kind": "userInput",
            "anchorRowId": 31,
            "createdAt": 1_700_000_000_500u64,
            "autoResolution": {"state": "visibleCountdown"},
            "payload": {
                "kind": "userInput",
                "prompt": "synthetic: pick one",
                "freeText": false,
                "toolCallId": "call_synthetic_b1",
                "toolName": "AskUserQuestion",
                "schema": {"interaction": "AskUserQuestion"},
                "questions": [{
                    "question": "Which color?",
                    "header": "Color",
                    "options": [
                        {"value": "Red", "label": "Red"},
                        {"value": "Blue", "label": "Blue"}
                    ]
                }]
            }
        })
    }

    fn pending_config(cards: Vec<Value>) -> PeerConfig {
        PeerConfig {
            workspaces: vec![workspace_entry(WS_A, Some("ws-a-identity"), "A")],
            tasks: vec![task_row(WS_A, SESSION_A, "hi")],
            conversation_snapshot: Some(conversation_snapshot(SESSION_A, 2, cards)),
            ..PeerConfig::default()
        }
    }

    /// 多选题卡（`multiSelect=true`，`freeText=false`）。
    fn multi_select_card(interaction_id: &str) -> Value {
        json!({
            "interactionId": interaction_id,
            "kind": "userInput",
            "anchorRowId": 32,
            "createdAt": 1_700_000_000_600u64,
            "payload": {
                "kind": "userInput",
                "prompt": "synthetic: pick many",
                "freeText": false,
                "toolCallId": "call_synthetic_multi",
                "toolName": "AskUserQuestion",
                "schema": {"interaction": "AskUserQuestion"},
                "questions": [{
                    "question": "Which colors?",
                    "header": "Colors",
                    "multiSelect": true,
                    "options": [
                        {"value": "Red", "label": "Red"},
                        {"value": "Blue", "label": "Blue"}
                    ]
                }]
            }
        })
    }

    /// 混合题卡：单选题 + 宿主允许自由文本（`freeText=true`）。
    fn mixed_free_text_card(interaction_id: &str) -> Value {
        json!({
            "interactionId": interaction_id,
            "kind": "userInput",
            "anchorRowId": 33,
            "createdAt": 1_700_000_000_700u64,
            "payload": {
                "kind": "userInput",
                "prompt": "synthetic: pick one or type",
                "freeText": true,
                "toolCallId": "call_synthetic_mixed",
                "toolName": "AskUserQuestion",
                "schema": {"interaction": "AskUserQuestion"},
                "questions": [
                    {
                        "question": "Which color?",
                        "header": "Color",
                        "options": [
                            {"value": "Red", "label": "Red"},
                            {"value": "Blue", "label": "Blue"}
                        ]
                    },
                    {
                        "question": "Sizes?",
                        "header": "Size",
                        "options": [
                            {"value": "S", "label": "S"},
                            {"value": "M", "label": "M"}
                        ]
                    }
                ]
            }
        })
    }

    /// 注入一帧宿主 `snapshot`（pending 集合整体替换 → 观察代数 +1）；
    /// 返回本次变化的会话视图。
    async fn push_pending_snapshot(peer: &Peer, cards: Vec<Value>) -> Vec<SessionView> {
        push_pending_snapshot_on(peer, SESSION_A, "conv-sub-synthetic-1", 5, cards).await
    }

    /// 向指定会话的当前订阅注入一帧宿主 `snapshot`；返回本次变化的会话视图。
    async fn push_pending_snapshot_on(
        peer: &Peer,
        session_id: &str,
        subscription: &str,
        revision: u64,
        cards: Vec<Value>,
    ) -> Vec<SessionView> {
        let bridge = peer.session_bridge_id(session_id);
        let listen = peer.listen_id(&bridge);
        let topic = format!("{CONVERSATION_TOPIC_PREFIX}{session_id}");
        let wire = test_wire(
            topic.as_str(),
            subscription,
            json!({
                "kind": "snapshot",
                "snapshot": conversation_snapshot(session_id, revision, cards)
            }),
        );
        peer.inject_frame(
            &bridge,
            channel_message(json!([RESPONSE_TYPE_EVENT_FIRE, listen]), &wire),
        )
        .await
    }

    fn command_frames(peer: &Peer, method: &str) -> Vec<TestMessage> {
        channel_frames(peer)
            .into_iter()
            .filter(|frame| frame.method.as_deref() == Some(method))
            .collect()
    }

    /// 授权返回**精确卡**；错会话/错代际/卡已撤/重复提交一律 stale，零宿主改动。
    #[tokio::test]
    async fn authorize_pending_returns_exact_card_and_rejects_stale() {
        const PENDING: &str = "pi-synthetic-b1-0001";
        let (manager, _peer) =
            manager_with(pending_config(vec![ask_user_question_card(PENDING)])).await;
        let target = manager.bind(&key(SESSION_A)).await.expect("bind");
        let view_card = manager.session(&target).await.expect("session").pending[0].clone();

        let authorized = manager
            .authorize_pending(&target, PENDING)
            .await
            .expect("exact target authorizes the projected card");
        assert_eq!(
            authorized, view_card,
            "must return the host-projected card verbatim"
        );
        assert_eq!(authorized.interaction_id, PENDING);
        assert_eq!(
            authorized.questions[0].question_text, "Which color?",
            "question text is the host original value"
        );

        // 授权不动宿主集合/观察代数（本地 in-flight ≠ 宿主收口）。
        let revision = manager.host_pending_revision(&target);
        assert!(manager.host_has_pending(&target, PENDING));

        let duplicate = manager
            .authorize_pending(&target, PENDING)
            .await
            .expect_err("duplicate local submit while in flight");
        assert!(duplicate.is(codes::STALE_PENDING));

        let unknown_anchor = manager
            .authorize_pending(&target, "pi-synthetic-b1-9999")
            .await
            .expect_err("unknown anchor");
        assert!(unknown_anchor.is(codes::STALE_PENDING));

        let mut stale_generation = target.clone();
        stale_generation.generation += 1;
        assert!(manager
            .authorize_pending(&stale_generation, PENDING)
            .await
            .expect_err("generation moved on")
            .is(codes::STALE_GENERATION));

        let mut sibling = target.clone();
        sibling.session_id = SESSION_B.to_owned();
        assert!(manager
            .authorize_pending(&sibling, PENDING)
            .await
            .expect_err("unbound sibling session")
            .is(codes::NO_SESSION));

        assert_eq!(manager.host_pending_revision(&target), revision);
        assert!(
            manager.host_has_pending(&target, PENDING),
            "stale attempts must not clear the host set"
        );
    }

    /// 宿主卡题型推导**全部**所需能力；读取只读（不置本地在途）。
    #[tokio::test]
    async fn required_capabilities_follow_host_card_types() {
        const MULTI: &str = "pi-synthetic-caps-multi";
        const MIXED: &str = "pi-synthetic-caps-mixed";
        const PERMISSION: &str = "pi-synthetic-caps-permission";

        // 多选题卡 → 只需 multi_select（与客户端勾选数量无关）。
        let (manager, _peer) = manager_with(pending_config(vec![multi_select_card(MULTI)])).await;
        let target = manager.bind(&key(SESSION_A)).await.expect("bind");
        assert_eq!(
            manager
                .required_capabilities(&target, MULTI)
                .await
                .expect("host card caps"),
            vec!["multi_select"]
        );
        // 只读：同一张卡随后仍可正常授权（证明没有偷偷置 in-flight）。
        manager
            .authorize_pending(&target, MULTI)
            .await
            .expect("required_capabilities must not set local in-flight");

        // 混合单选 + 自由文本 → 两项都要，稳定顺序 single_select、free_text。
        let (manager, _peer) =
            manager_with(pending_config(vec![mixed_free_text_card(MIXED)])).await;
        let target = manager.bind(&key(SESSION_A)).await.expect("bind");
        assert_eq!(
            manager
                .required_capabilities(&target, MIXED)
                .await
                .expect("host card caps"),
            vec!["single_select", "free_text"]
        );

        // 权限审批卡 → approval_reply（与问答题型无关）。
        let (manager, _peer) =
            manager_with(pending_config(vec![permission_card(PERMISSION)])).await;
        let target = manager.bind(&key(SESSION_A)).await.expect("bind");
        assert_eq!(
            manager
                .required_capabilities(&target, PERMISSION)
                .await
                .expect("host card caps"),
            vec!["approval_reply"]
        );
    }

    /// 缺卡/错代际/未绑定/投影不新鲜：能力推导按 `authorize_pending` 同语义报错，
    /// 绝不按「没有卡」放行成空能力集。
    #[tokio::test]
    async fn required_capabilities_reject_missing_stale_or_unready() {
        const PENDING: &str = "pi-synthetic-caps-errors";
        let (manager, _peer) =
            manager_with(pending_config(vec![ask_user_question_card(PENDING)])).await;
        let target = manager.bind(&key(SESSION_A)).await.expect("bind");

        let missing = manager
            .required_capabilities(&target, "pi-synthetic-caps-absent")
            .await
            .expect_err("missing card");
        assert!(missing.is(codes::STALE_PENDING));

        let mut stale_generation = target.clone();
        stale_generation.generation += 1;
        assert!(manager
            .required_capabilities(&stale_generation, PENDING)
            .await
            .expect_err("generation moved on")
            .is(codes::STALE_GENERATION));

        let mut sibling = target.clone();
        sibling.session_id = SESSION_B.to_owned();
        assert!(manager
            .required_capabilities(&sibling, PENDING)
            .await
            .expect_err("unbound sibling session")
            .is(codes::NO_SESSION));

        // 无会话快照 = 投影不新鲜：不得返回任何能力名（不猜题型）。
        let mut unready_config = pending_config(Vec::new());
        unready_config.conversation_snapshot = None;
        let (unready_manager, _peer) = manager_with(unready_config).await;
        let unready_target = unready_manager.bind(&key(SESSION_A)).await.expect("bind");
        assert!(unready_manager
            .required_capabilities(&unready_target, PENDING)
            .await
            .expect_err("unready projection")
            .is(codes::NOT_READY));
    }

    /// snooze 恰一帧官方 `snoozeInteractionAutoResolution{interactionId}`；ACK 映射固定。
    #[tokio::test]
    async fn snooze_sends_one_frame_and_maps_ack() {
        const PENDING: &str = "pi-synthetic-b1-0002";
        const SNOOZE_TYPE: &str = "snoozeInteractionAutoResolution";
        const CLIENT: &str = "phone-synthetic-b1";

        // accepted / duplicate / noop → Ok（noop = 已暂停的幂等回放，前置条件已满足）。
        for ack in [
            json!({"status": "accepted"}),
            json!({"status": "duplicate"}),
            json!({"status": "noop"}),
        ] {
            let config = PeerConfig {
                command_ack: Some(ack.clone()),
                ..pending_config(vec![ask_user_question_card(PENDING)])
            };
            let (manager, peer) = manager_with(config).await;
            let target = manager.bind(&key(SESSION_A)).await.expect("bind");

            manager
                .snooze(&target, PENDING, CLIENT, Duration::from_millis(120))
                .await
                .unwrap_or_else(|error| panic!("accepted/duplicate/noop must succeed: {error:?}"));

            let frames = command_frames(&peer, AGENT_SEND_COMMAND_METHOD);
            assert_eq!(
                frames.len(),
                1,
                "exactly one official command frame ({ack})"
            );
            let frame = &frames[0];
            assert_eq!(frame.channel.as_deref(), Some(AGENT_CHANNEL));
            let params = &frame.body[0];
            assert_eq!(params["workspacePath"], json!(WS_A));
            assert_eq!(
                params.get("workspaceIdentity"),
                Some(&json!("ws-a-identity"))
            );
            let envelope = &params["envelope"];
            assert_eq!(envelope["type"], json!(SNOOZE_TYPE));
            assert_eq!(envelope["payload"], json!({"interactionId": PENDING}));
            assert_eq!(
                envelope["clientId"],
                json!(CLIENT),
                "snooze 必须用可信设备 id（与 resolve 同源）"
            );
            assert_eq!(
                envelope["sessionId"],
                json!(SESSION_A),
                "target session only"
            );
            assert_eq!(
                params.as_object().expect("params").len(),
                3,
                "no extra wire fields"
            );
        }

        // rejected → Err(invalid_action)（硬失败）；恰一帧。
        let config = PeerConfig {
            command_ack: Some(json!({"status": "rejected"})),
            ..pending_config(vec![ask_user_question_card(PENDING)])
        };
        let (manager, peer) = manager_with(config).await;
        let target = manager.bind(&key(SESSION_A)).await.expect("bind");
        assert!(manager
            .snooze(&target, PENDING, CLIENT, Duration::from_millis(120))
            .await
            .expect_err("rejected command")
            .is(codes::INVALID_ACTION));
        assert_eq!(command_frames(&peer, AGENT_SEND_COMMAND_METHOD).len(), 1);

        // 应答窗超窗（桥按调用方期限收口）→ Err(unknown)，零重发。
        let config = PeerConfig {
            silent_methods: vec![AGENT_SEND_COMMAND_METHOD.to_owned()],
            ..pending_config(vec![ask_user_question_card(PENDING)])
        };
        let (manager, peer) = manager_with(config).await;
        let target = manager.bind(&key(SESSION_A)).await.expect("bind");
        assert!(manager
            .snooze(&target, PENDING, CLIENT, Duration::from_millis(60))
            .await
            .expect_err("ack window expired")
            .is(codes::UNKNOWN));
        assert_eq!(
            command_frames(&peer, AGENT_SEND_COMMAND_METHOD).len(),
            1,
            "timeout must never resend"
        );
    }

    /// 宿主终态：只有**目标会话册**代数前进**且**该 id 消失才 true；卡仍在/无帧都 false。
    #[tokio::test]
    async fn wait_host_cleared_requires_revision_advance() {
        const PENDING: &str = "pi-synthetic-b1-0003";
        let (manager, peer) =
            manager_with(pending_config(vec![ask_user_question_card(PENDING)])).await;
        let target = manager.bind(&key(SESSION_A)).await.expect("bind");
        let baseline = manager.host_pending_revision(&target);
        assert_eq!(
            baseline, 1,
            "initial host snapshot is the first replacement"
        );
        assert!(manager.host_has_pending(&target, PENDING));

        // 无宿主帧：代数没前进 → 超时 false（绝不把「本地等待」当撤卡）。
        assert!(
            !manager
                .wait_host_cleared(&target, PENDING, baseline, Duration::from_millis(30))
                .await,
            "no host replacement must not confirm"
        );

        // 代数前进但卡仍在：false。
        push_pending_snapshot(&peer, vec![ask_user_question_card(PENDING)]).await;
        assert!(manager.host_pending_revision(&target) > baseline);
        assert!(
            !manager
                .wait_host_cleared(&target, PENDING, baseline, Duration::from_millis(30))
                .await,
            "revision advanced but the card is still on the host"
        );

        // 代数前进 + 卡消失：在途等待在撤卡帧到达后 true。
        let waiter = {
            let manager = manager.clone();
            let target = target.clone();
            let id = PENDING.to_owned();
            tokio::spawn(async move {
                manager
                    .wait_host_cleared(&target, &id, baseline, Duration::from_millis(600))
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        push_pending_snapshot(&peer, Vec::new()).await;
        assert!(
            waiter.await.expect("waiter task"),
            "advance + absent is confirmed"
        );
        assert!(!manager.host_has_pending(&target, PENDING));

        // 撤卡之后再用旧基线：代数已前进且 id 缺席 → 立即 true（不会卡住）。
        assert!(
            manager
                .wait_host_cleared(&target, PENDING, baseline, Duration::from_millis(30))
                .await
        );
    }

    /// 宿主观察代数单调不减（每次整体替换 +1；本轮测试内严格递增）。
    #[tokio::test]
    async fn host_pending_revision_is_monotonic() {
        const PENDING: &str = "pi-synthetic-b1-0004";
        let (manager, peer) =
            manager_with(pending_config(vec![ask_user_question_card(PENDING)])).await;
        let target = manager.bind(&key(SESSION_A)).await.expect("bind");

        let mut previous = manager.host_pending_revision(&target);
        assert!(previous >= 1);
        for cards in [
            vec![ask_user_question_card(PENDING)],
            Vec::new(),
            vec![ask_user_question_card(PENDING)],
        ] {
            push_pending_snapshot(&peer, cards).await;
            let current = manager.host_pending_revision(&target);
            assert!(
                current > previous,
                "each host replacement advances the revision ({current} > {previous})"
            );
            previous = current;
        }
        assert!(manager.host_pending_revision(&target) >= previous);
    }

    /// 宿主终态证据必须**按目标会话册**取：兄弟会话的撤卡/换代绝不冒充本卡收口。
    #[tokio::test]
    async fn wait_host_cleared_is_target_scoped() {
        const PENDING_A: &str = "pi-synthetic-b1-0005";
        const PENDING_B: &str = "pi-synthetic-b1-0006";
        let config = PeerConfig {
            workspaces: vec![
                workspace_entry(WS_A, Some("ws-a-identity"), "A"),
                workspace_entry(WS_B, Some("ws-b-identity"), "B"),
            ],
            tasks: vec![
                task_row(WS_A, SESSION_A, "hi"),
                task_row(WS_B, SESSION_B, "hi"),
            ],
            conversation_snapshot: Some(conversation_snapshot(
                SESSION_A,
                2,
                vec![ask_user_question_card(PENDING_A)],
            )),
            // 第二个会话订阅走 resync 快照（Peer 的订阅序号 2 → B）。
            resync_snapshot: Some(conversation_snapshot(
                SESSION_B,
                2,
                vec![ask_user_question_card(PENDING_B)],
            )),
            ..PeerConfig::default()
        };
        let (manager, peer) = manager_with(config).await;
        let target_a = manager.bind(&key(SESSION_A)).await.expect("bind A");
        let target_b = manager.bind(&key(SESSION_B)).await.expect("bind B");
        assert_eq!(target_a.generation, target_b.generation);
        let baseline_a = manager.host_pending_revision(&target_a);
        assert!(manager.host_has_pending(&target_a, PENDING_A));
        assert!(manager.host_has_pending(&target_b, PENDING_B));

        // ① 兄弟会话（B）撤卡：A 的代数/集合不动，等待不得被判 true。
        push_pending_snapshot_on(&peer, SESSION_B, "conv-sub-synthetic-2", 5, Vec::new()).await;
        assert!(
            !manager.host_has_pending(&target_b, PENDING_B),
            "B 的撤卡帧必须落在 B 册"
        );
        assert_eq!(
            manager.host_pending_revision(&target_a),
            baseline_a,
            "B 的帧不得推进 A 的观察代数"
        );
        assert!(manager.host_has_pending(&target_a, PENDING_A));
        assert!(
            !manager
                .wait_host_cleared(&target_a, PENDING_A, baseline_a, Duration::from_millis(30))
                .await,
            "other session's clear must not confirm A"
        );

        // ② 换代清册后旧目标不可再用：即使兄弟会话重新挂册把全局代数推过基线，
        //    旧 targets 也绝不能被判 confirmed（目标已换代 → false）。
        let stale_a = target_a.clone();
        manager.invalidate(target_a.generation + 1).await;
        let _rebound_b = manager.bind(&key(SESSION_B)).await.expect("rebind B");
        push_pending_snapshot_on(&peer, SESSION_B, "conv-sub-synthetic-3", 6, Vec::new()).await;
        assert_eq!(
            manager.host_pending_revision(&stale_a),
            0,
            "stale target has no host evidence"
        );
        assert!(!manager.host_has_pending(&stale_a, PENDING_A));
        assert!(
            !manager
                .wait_host_cleared(&stale_a, PENDING_A, baseline_a, Duration::from_millis(30))
                .await,
            "generation change must never be confirmed by sibling evidence"
        );
    }

    // =======================================================================
    // T4.2 Slice A：stop 授权端口（合成夹具，不读真实账号/DB）
    // =======================================================================

    /// 带 control 的会话快照（execution id 全为合成值）。
    fn control_snapshot(session_id: &str, revision: u64, execution_ids: &[&str]) -> Value {
        let works: Vec<Value> = execution_ids
            .iter()
            .map(|id| json!({"kind": "primaryTurn", "foregroundExecutionId": id}))
            .collect();
        let mut snapshot = conversation_snapshot(session_id, revision, Vec::new());
        snapshot["control"] = json!({"activeWorks": works});
        snapshot
    }

    /// 注入一条 control 增量（只有 `state.updated` 的键级整体替换）。
    ///
    /// `revision = None` 模拟「撤项但宿主 revision 未前进」的迟到/迟滞帧：观察到的
    /// control 变了，但没有任何可确认的宿主水位。返回本次变化的会话视图。
    #[allow(clippy::too_many_arguments)]
    async fn push_control_delta_on(
        peer: &Peer,
        session_id: &str,
        subscription: &str,
        from: u64,
        to: u64,
        revision: Option<u64>,
        active_works: Value,
    ) -> Vec<SessionView> {
        let bridge = peer.session_bridge_id(session_id);
        let listen = peer.listen_id(&bridge);
        let topic = format!("{CONVERSATION_TOPIC_PREFIX}{session_id}");
        let mut patch = json!({"control": {"activeWorks": active_works}});
        if let Some(revision) = revision {
            patch["revision"] = json!(revision);
        }
        let wire = json!({
            "wireVersion": V4_WIRE_PROTOCOL_VERSION,
            "kind": "complete",
            "deliveryKind": "initial",
            "logicalFrameId": format!("synthetic-control-{to}"),
            "logicalFrameOrdinal": to,
            "topic": topic,
            "subscriptionId": subscription,
            "frame": {
                "topic": topic,
                "subscriptionId": subscription,
                "fromSeq": from,
                "toSeq": to,
                "sentAt": 0,
                "payload": {
                    "kind": "deltas",
                    "deltas": [{"op": "state.updated", "patch": patch}]
                }
            }
        });
        peer.inject_frame(
            &bridge,
            channel_message(json!([RESPONSE_TYPE_EVENT_FIRE, listen]), &wire),
        )
        .await
    }

    // =======================================================================
    // Data 变化的会话视图（Hub 发布面的桥侧证据）
    // =======================================================================

    /// Data 帧必须报告**本次变化**的会话视图：绑定后一条 conversation delta 返回恰好
    /// 一条该会话的视图（revision/control 已更新）；重放同一帧（投影不变）返回空。
    ///
    /// 上层（Hub 事件泵）就是靠这份返回值把会话变化发布给手机；只更新桥内投影而
    /// 不报告变化的实现必须在这里失败。
    #[tokio::test]
    async fn on_data_reports_changed_session_views() {
        const EXEC: &str = "E-synthetic-view-change";
        const SUB: &str = "conv-sub-synthetic-1";
        let (manager, peer) = manager_with(single_workspace_config()).await;
        let target = manager.bind(&key(SESSION_A)).await.expect("bind");
        let baseline = manager.session(&target).await.expect("session");
        assert!(baseline.ready, "订阅快照必须先建立就绪投影");
        assert_eq!(baseline.revision, Some(2));

        // revision 前进 + control 出现：必须恰好报告一条本会话视图。
        let active_works = json!([{"kind": "primaryTurn", "foregroundExecutionId": EXEC}]);
        let changed =
            push_control_delta_on(&peer, SESSION_A, SUB, 1, 2, Some(3), active_works.clone()).await;
        assert_eq!(changed.len(), 1, "一帧只改变一个已绑定会话");
        let view = &changed[0];
        assert_eq!(view.session_id, SESSION_A);
        assert!(view.ready);
        assert_eq!(view.revision, Some(3), "变化视图必须带新 revision");
        let control = view.control.as_ref().expect("control 已观测");
        assert_eq!(control.active_works.len(), 1);
        assert_eq!(
            control.active_works[0].foreground_execution_id.as_deref(),
            Some(EXEC)
        );
        assert_eq!(view.generation, baseline.generation, "桥投影代际不变");

        // 重放同一 logical frame（toSeq 不前进）：投影守恒 → 空（不得重复上报）。
        let unchanged =
            push_control_delta_on(&peer, SESSION_A, SUB, 1, 2, Some(3), active_works).await;
        assert!(unchanged.is_empty(), "无变化的帧不得报告会话变化");
        assert_eq!(
            manager.session(&target).await.expect("session").revision,
            Some(3)
        );
    }

    /// 坏帧（CRC 校验失败）同样必须报告它造成的投影降级：`on_data_outcome` 返回
    /// `Err` **且** `changed` 含 `ready=false` 的会话；无变化的错误帧 `changed` 为空；
    /// 随后有效快照恢复 `ready=true`（证明自愈路径）。
    ///
    /// 旧实现只在 `result.is_ok()` 时做前后 diff：错误帧把投影标成不新鲜后，降级视图
    /// 被整包丢弃，Hub 因此永远收不到失效通知（手机停留在旧卡面，直到一次无关 GET）。
    #[tokio::test]
    async fn on_data_outcome_reports_degraded_views_on_frame_error() {
        const SUB: &str = "conv-sub-synthetic-1";
        let (manager, peer) = manager_with(single_workspace_config()).await;
        let target = manager.bind(&key(SESSION_A)).await.expect("bind");
        let baseline = manager.session(&target).await.expect("session");
        assert!(baseline.ready, "订阅快照必须先建立就绪投影");
        assert_eq!(baseline.revision, Some(2));

        let bridge = peer.session_bridge_id(SESSION_A);
        let listen = peer.listen_id(&bridge);
        let topic = format!("{CONVERSATION_TOPIC_PREFIX}{SESSION_A}");

        // 逐帧构造合法消息再破坏物理 CRC（seq/messageSeq 由 fake 严格递增）。
        let bad_frame = || {
            let mut inner = peer.inner.lock().expect("peer lock");
            let mut frame = peer.frame(
                &mut inner,
                &bridge,
                channel_message(
                    json!([RESPONSE_TYPE_EVENT_FIRE, listen]),
                    &test_wire(
                        &topic,
                        SUB,
                        json!({
                            "kind": "snapshot",
                            "snapshot": conversation_snapshot(SESSION_A, 9, Vec::new()),
                        }),
                    ),
                ),
            );
            frame["checksum"]["value"] = json!("00000000");
            frame
        };

        // 第一次坏帧：投影 ready true→false，必须仍然上报这条降级视图。
        let outcome = manager.on_data_outcome(bad_frame()).await;
        let error = outcome.result.expect_err("坏 CRC 必须返回错误");
        assert!(error.is(codes::UNSUPPORTED_PROTOCOL));
        assert_eq!(outcome.changed.len(), 1, "错误帧必须仍报告降级视图");
        let degraded = &outcome.changed[0];
        assert_eq!(degraded.session_id, SESSION_A);
        assert!(!degraded.ready, "错误帧已把投影标成不新鲜");
        assert_eq!(degraded.revision, Some(2), "降级不得改写既有 revision");
        assert_eq!(degraded.generation, baseline.generation);
        assert!(!manager.peek_view(SESSION_A).expect("bound").ready);

        // 投影已不新鲜后的再次坏帧：无变化 → changed 为空；兼容包装 on_data 仍返回 Err。
        let outcome = manager.on_data_outcome(bad_frame()).await;
        assert!(outcome.result.is_err());
        assert!(outcome.changed.is_empty(), "无变化的错误帧不得伪造变化");
        let error = manager
            .on_data(bad_frame())
            .await
            .expect_err("on_data 兼容包装不得吞错");
        assert!(error.is(codes::UNSUPPORTED_PROTOCOL));

        // 有效快照（同订阅）恢复 ready=true，并报告恢复视图。
        let recovered_frame = {
            let mut inner = peer.inner.lock().expect("peer lock");
            peer.frame(
                &mut inner,
                &bridge,
                channel_message(
                    json!([RESPONSE_TYPE_EVENT_FIRE, listen]),
                    &test_wire(
                        &topic,
                        SUB,
                        json!({
                            "kind": "snapshot",
                            "snapshot": conversation_snapshot(SESSION_A, 9, Vec::new()),
                        }),
                    ),
                ),
            )
        };
        let outcome = manager.on_data_outcome(recovered_frame).await;
        assert!(outcome.result.is_ok(), "有效快照必须被吸收");
        assert_eq!(outcome.changed.len(), 1, "恢复也必须报告变化");
        assert!(outcome.changed[0].ready, "恢复视图必须 ready=true");
        assert_eq!(outcome.changed[0].revision, Some(9));
        assert!(manager.peek_view(SESSION_A).expect("bound").ready);
    }

    // =======================================================================
    // T6：SelectionPort 生产实现（合成夹具，不读真实账号/DB）
    // =======================================================================

    /// 带官方扁平 `config.thoughtLevels` 的会话快照（B4 形状，无 `config.models`）。
    fn selection_snapshot(
        session_id: &str,
        revision: u64,
        provider: &str,
        model: &str,
        thought: Option<&str>,
        levels: &[&str],
    ) -> Value {
        let mut snapshot = conversation_snapshot(session_id, revision, Vec::new());
        snapshot["config"] = json!({
            "provider": provider,
            "model": model,
            "thought": thought,
            "thoughtLevels": levels,
        });
        snapshot
    }

    /// 注入一条选择增量（config 键级整体替换 + revision 前进）；返回变化的会话视图。
    #[allow(clippy::too_many_arguments)]
    async fn push_selection_delta_on(
        peer: &Peer,
        session_id: &str,
        subscription: &str,
        from: u64,
        to: u64,
        revision: u64,
        provider: &str,
        model: &str,
        thought: &str,
    ) -> Vec<SessionView> {
        let bridge = peer.session_bridge_id(session_id);
        let listen = peer.listen_id(&bridge);
        let topic = format!("{CONVERSATION_TOPIC_PREFIX}{session_id}");
        let patch = json!({
            "revision": revision,
            "config": {"provider": provider, "model": model, "thought": thought},
        });
        let wire = json!({
            "wireVersion": V4_WIRE_PROTOCOL_VERSION,
            "kind": "complete",
            "deliveryKind": "initial",
            "logicalFrameId": format!("synthetic-selection-{to}"),
            "logicalFrameOrdinal": to,
            "topic": topic,
            "subscriptionId": subscription,
            "frame": {
                "topic": topic,
                "subscriptionId": subscription,
                "fromSeq": from,
                "toSeq": to,
                "sentAt": 0,
                "payload": {
                    "kind": "deltas",
                    "deltas": [{"op": "state.updated", "patch": patch}]
                }
            }
        });
        peer.inject_frame(
            &bridge,
            channel_message(json!([RESPONSE_TYPE_EVENT_FIRE, listen]), &wire),
        )
        .await
    }

    /// stop 端口的会话视图必须按**精确目标**取：兄弟会话的 control 绝不冒充本目标，
    /// 伪造 workspace 绑定/未绑定会话一律拒绝。
    #[tokio::test]
    async fn stop_port_session_is_target_scoped() {
        const EXEC_A: &str = "E-synthetic-stop-a";
        const EXEC_B: &str = "E-synthetic-stop-b";
        let config = PeerConfig {
            workspaces: vec![
                workspace_entry(WS_A, Some("ws-a-identity"), "A"),
                workspace_entry(WS_B, Some("ws-b-identity"), "B"),
            ],
            tasks: vec![
                task_row(WS_A, SESSION_A, "hi"),
                task_row(WS_B, SESSION_B, "hi"),
            ],
            // 会话 A 走首个订阅；会话 B 走重订阅（订阅序号 2 → B）。
            conversation_snapshot: Some(control_snapshot(SESSION_A, 2, &[EXEC_A])),
            resync_snapshot: Some(control_snapshot(SESSION_B, 2, &[EXEC_B])),
            ..PeerConfig::default()
        };
        let (manager, _peer) = manager_with(config).await;
        let target_a = manager.bind(&key(SESSION_A)).await.expect("bind A");
        let target_b = manager.bind(&key(SESSION_B)).await.expect("bind B");

        let view_a = StopPort::session(manager.as_ref(), &target_a, Duration::from_secs(1))
            .await
            .expect("A session");
        let view_b = StopPort::session(manager.as_ref(), &target_b, Duration::from_secs(1))
            .await
            .expect("B session");
        assert_eq!(view_a.session_id, SESSION_A);
        assert_eq!(view_b.session_id, SESSION_B);
        assert_eq!(
            view_a
                .control
                .as_ref()
                .expect("A control")
                .active_works
                .iter()
                .filter_map(|work| work.foreground_execution_id.as_deref())
                .collect::<Vec<_>>(),
            vec![EXEC_A],
            "session A must only see its own observed execution"
        );
        assert_eq!(
            view_b
                .control
                .as_ref()
                .expect("B control")
                .active_works
                .iter()
                .filter_map(|work| work.foreground_execution_id.as_deref())
                .collect::<Vec<_>>(),
            vec![EXEC_B],
            "session B must only see its own observed execution"
        );

        // 同 sessionId 但 workspace 绑定被改写（手机自报形状）不是精确目标。
        let mut forged = target_a.clone();
        forged.workspace_key = "ws-b-identity".to_owned();
        assert!(
            StopPort::session(manager.as_ref(), &forged, Duration::from_secs(1))
                .await
                .expect_err("forged workspace binding")
                .is(codes::STALE_GENERATION),
            "workspace binding mismatch must never read another target's projection"
        );

        // 未绑定的会话：no_session（绝不借 A 的投影顶替）。
        let mut unbound = target_a.clone();
        unbound.session_id = "sess_0f9a1c0d-0000-4000-8000-00000000ffff".to_owned();
        assert!(
            StopPort::session(manager.as_ref(), &unbound, Duration::from_secs(1))
                .await
                .expect_err("unbound sibling")
                .is(codes::NO_SESSION)
        );
    }

    /// 停止终态必须**同时**满足「该目标 revision 前进」且「目标 execution 缺席」：
    /// 缺一项都不确认；有界轮询在两项都满足后返回 true。
    #[tokio::test]
    async fn wait_stop_cleared_requires_revision_and_target_absent() {
        const EXEC: &str = "E-synthetic-stop-1";
        const SUB: &str = "conv-sub-synthetic-1";
        let config = PeerConfig {
            workspaces: vec![workspace_entry(WS_A, Some("ws-a-identity"), "A")],
            tasks: vec![task_row(WS_A, SESSION_A, "hi")],
            conversation_snapshot: Some(control_snapshot(SESSION_A, 2, &[EXEC])),
            ..PeerConfig::default()
        };
        let (manager, peer) = manager_with(config).await;
        let target = manager.bind(&key(SESSION_A)).await.expect("bind");
        let view = StopPort::session(manager.as_ref(), &target, Duration::from_secs(1))
            .await
            .expect("session");
        let baseline = view.revision.expect("host revision");
        assert_eq!(baseline, 2);

        // 无宿主帧：revision 未前进 → false（绝不把「本地等待」当停止）。
        assert!(
            !manager
                .wait_stop_cleared(&target, EXEC, baseline, Duration::from_millis(30))
                .await,
            "no host frame must not confirm"
        );

        // 目标从 control 消失但 revision 未前进：false（缺水位不算证据）。
        push_control_delta_on(&peer, SESSION_A, SUB, 1, 2, None, json!([])).await;
        let current = manager.session(&target).await.expect("session");
        assert_eq!(current.revision, Some(2), "delta without revision keeps it");
        assert!(current
            .control
            .as_ref()
            .expect("control")
            .active_works
            .is_empty());
        assert!(
            !manager
                .wait_stop_cleared(&target, EXEC, baseline, Duration::from_millis(30))
                .await,
            "target absent without a revision advance must not confirm"
        );

        // revision 前进但目标仍在：false。
        push_control_delta_on(
            &peer,
            SESSION_A,
            SUB,
            2,
            3,
            Some(3),
            json!([{"kind": "primaryTurn", "foregroundExecutionId": EXEC}]),
        )
        .await;
        assert!(
            !manager
                .wait_stop_cleared(&target, EXEC, baseline, Duration::from_millis(30))
                .await,
            "revision advance with the target still observed must not confirm"
        );

        // 等待中「revision 前进 + 目标缺席」：true。
        let baseline = 3;
        let waiter = {
            let manager = manager.clone();
            let target = target.clone();
            let execution = EXEC.to_owned();
            tokio::spawn(async move {
                manager
                    .wait_stop_cleared(&target, &execution, baseline, Duration::from_millis(600))
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        push_control_delta_on(&peer, SESSION_A, SUB, 3, 4, Some(4), json!([])).await;
        assert!(
            waiter.await.expect("waiter task"),
            "advance + absent is confirmed by bounded polling"
        );
    }

    /// control 未知不是结论：`wait_stop_cleared` 必须轮询到期限（首个 `None` 不提前
    /// 返回），且等待中恢复的已确认 control 必须能完成判定。
    #[tokio::test]
    async fn wait_stop_cleared_polls_unknown_control_to_deadline() {
        const EXEC: &str = "E-synthetic-stop-2";
        const SUB: &str = "conv-sub-synthetic-1";
        let mut without_control = conversation_snapshot(SESSION_A, 2, Vec::new());
        without_control
            .as_object_mut()
            .expect("snapshot object")
            .remove("control");
        let config = PeerConfig {
            workspaces: vec![workspace_entry(WS_A, Some("ws-a-identity"), "A")],
            tasks: vec![task_row(WS_A, SESSION_A, "hi")],
            conversation_snapshot: Some(without_control),
            ..PeerConfig::default()
        };
        let (manager, peer) = manager_with(config).await;
        let target = manager.bind(&key(SESSION_A)).await.expect("bind");
        let view = StopPort::session(manager.as_ref(), &target, Duration::from_secs(1))
            .await
            .expect("session");
        assert!(view.control.is_none(), "夹具必须是未知 control");
        let baseline = view.revision.expect("host revision");

        // ① 一直未知：false 只由期限给出（首个 None 不是结论）。
        let started = Instant::now();
        assert!(
            !manager
                .wait_stop_cleared(&target, EXEC, baseline, Duration::from_millis(80))
                .await
        );
        assert!(
            started.elapsed() >= Duration::from_millis(50),
            "unknown control 必须轮询到期限，不能首个 None 就返回"
        );

        // ② 等待中恢复已确认 control（目标不在场）且 revision 前进 → true。
        let waiter = {
            let manager = manager.clone();
            let target = target.clone();
            tokio::spawn(async move {
                manager
                    .wait_stop_cleared(&target, EXEC, baseline, Duration::from_millis(600))
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        push_control_delta_on(&peer, SESSION_A, SUB, 1, 2, Some(3), json!([])).await;
        assert!(
            waiter.await.expect("waiter task"),
            "恢复的已确认 control 必须能完成判定"
        );
    }

    /// 停止终态严格按**精确目标**取证：兄弟会话撤掉自己的执行不得确认本目标；
    /// 本目标仍在 control 里（哪怕多了新执行）不得确认；只有原目标确已消失才确认。
    #[tokio::test]
    async fn wait_stop_cleared_is_target_scoped_for_stop() {
        const EXEC_A: &str = "E-synthetic-stop-a";
        const EXEC_B: &str = "E-synthetic-stop-b";
        const NEW: &str = "E-synthetic-stop-new";
        const SUB_A: &str = "conv-sub-synthetic-1";
        const SUB_B: &str = "conv-sub-synthetic-2";
        let config = PeerConfig {
            workspaces: vec![
                workspace_entry(WS_A, Some("ws-a-identity"), "A"),
                workspace_entry(WS_B, Some("ws-b-identity"), "B"),
            ],
            tasks: vec![
                task_row(WS_A, SESSION_A, "hi"),
                task_row(WS_B, SESSION_B, "hi"),
            ],
            conversation_snapshot: Some(control_snapshot(SESSION_A, 2, &[EXEC_A])),
            resync_snapshot: Some(control_snapshot(SESSION_B, 2, &[EXEC_B])),
            ..PeerConfig::default()
        };
        let (manager, peer) = manager_with(config).await;
        let target_a = manager.bind(&key(SESSION_A)).await.expect("bind A");
        let _target_b = manager.bind(&key(SESSION_B)).await.expect("bind B");
        let baseline = StopPort::session(manager.as_ref(), &target_a, Duration::from_secs(1))
            .await
            .expect("A session")
            .revision
            .expect("A revision");

        // ① 兄弟会话（B）撤掉自己的执行：A 的目标仍在 → 不得确认（轮询到期限）。
        push_control_delta_on(&peer, SESSION_B, SUB_B, 1, 2, Some(3), json!([])).await;
        assert!(
            !manager
                .wait_stop_cleared(&target_a, EXEC_A, baseline, Duration::from_millis(60))
                .await,
            "other session's clear must not confirm A"
        );
        let view_a = StopPort::session(manager.as_ref(), &target_a, Duration::from_secs(1))
            .await
            .expect("A session");
        assert!(
            view_a
                .control
                .expect("A control")
                .active_works
                .iter()
                .any(|work| work.foreground_execution_id.as_deref() == Some(EXEC_A)),
            "B 的帧不得污染 A 的 control"
        );

        // ② revision 前进、原目标仍在、场上多了新执行 → 不得确认。
        push_control_delta_on(
            &peer,
            SESSION_A,
            SUB_A,
            1,
            2,
            Some(3),
            json!([
                {"kind": "primaryTurn", "foregroundExecutionId": EXEC_A},
                {"kind": "primaryTurn", "foregroundExecutionId": NEW}
            ]),
        )
        .await;
        assert!(
            !manager
                .wait_stop_cleared(&target_a, EXEC_A, baseline, Duration::from_millis(60))
                .await,
            "revision advance with the target still observed must not confirm"
        );

        // ③ 原目标确已消失（新执行仍在场）且 revision 再前进 → 确认。
        let waiter = {
            let manager = manager.clone();
            let target_a = target_a.clone();
            tokio::spawn(async move {
                manager
                    .wait_stop_cleared(&target_a, EXEC_A, 3, Duration::from_millis(600))
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        push_control_delta_on(
            &peer,
            SESSION_A,
            SUB_A,
            2,
            3,
            Some(4),
            json!([{"kind": "primaryTurn", "foregroundExecutionId": NEW}]),
        )
        .await;
        assert!(
            waiter.await.expect("waiter task"),
            "原目标确已消失时（即便新执行在场）必须确认"
        );
    }

    /// 精确目标的会话读把**硬预算**分给内部重订阅 RPC：宿主静默时按预算
    /// `not_ready` 收口，绝不把调用方拖过窗（I-1：10s 窗不得被单读拖成 ~30s）。
    #[tokio::test]
    async fn stop_port_session_honours_budget_during_resync() {
        let config = PeerConfig {
            workspaces: vec![workspace_entry(WS_A, None, "A")],
            tasks: vec![task_row(WS_A, SESSION_A, "hi")],
            conversation_snapshot: Some(conversation_snapshot(SESSION_A, 2, Vec::new())),
            // 首个快照物理 seq 跳号 → 投影不新鲜，下一次读触发只读重订阅。
            gap_after_frames: Some(4),
            // 首次绑定订阅必须成功；第 2 次（读触发的重订阅）起宿主静默，
            // 只有读预算能收口。
            silent_methods_from_call: vec![(AGENT_SUBSCRIBE_CONVERSATION_METHOD.to_owned(), 2)],
            ..PeerConfig::default()
        };
        let (manager, _peer) = manager_with(config).await;
        let target = manager.bind(&key(SESSION_A)).await.expect("bind");
        assert!(!manager.peek_view(SESSION_A).expect("bound").ready);

        let started = Instant::now();
        let error = StopPort::session(manager.as_ref(), &target, Duration::from_millis(80))
            .await
            .expect_err("a silent resubscribe must not outlive the read budget");
        let elapsed = started.elapsed();
        assert!(
            error.is(codes::NOT_READY),
            "预算到期如实收口, got {error:?}"
        );
        assert!(
            elapsed >= Duration::from_millis(50),
            "应等到预算到期而不是立刻失败，got {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "单次读不得超过自己的预算（80ms），got {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn physical_seq_and_message_seq_are_strictly_increasing() {
        let (manager, peer) = manager_with(single_workspace_config()).await;
        let target = manager.bind(&key(SESSION_A)).await.expect("bind");
        assert!(manager
            .call(&target, AGENT_CHANNEL, "readOne", vec![])
            .await
            .is_ok());
        assert!(manager
            .call(&target, AGENT_CHANNEL, "readTwo", vec![])
            .await
            .is_ok());
        assert_eq!(peer.order_faults(), 0);
        let mut per_bridge: HashMap<String, Vec<(u64, u64)>> = HashMap::new();
        for (bridge, seq, message_seq) in peer.wire_sequences() {
            per_bridge
                .entry(bridge)
                .or_default()
                .push((seq, message_seq));
        }
        for (bridge, sequences) in per_bridge {
            for (expected, (seq, message_seq)) in (1u64..).zip(sequences) {
                assert_eq!(seq, expected, "bridge {bridge} seq");
                assert_eq!(message_seq, expected, "bridge {bridge} messageSeq");
            }
        }
    }

    // =======================================================================
    // T6 Fix round 1：SelectionPort 生产实现（load / wait_selection）
    // =======================================================================

    /// `load` 走官方 `model-selection.getView`（无参只读）并只保留当前 provider 目录。
    #[tokio::test]
    async fn selection_load_queries_get_view_and_filters_current_provider() {
        const PROVIDER: &str = "synthetic-provider";
        let config = PeerConfig {
            workspaces: vec![workspace_entry(WS_A, Some("ws-a-identity"), "A")],
            tasks: vec![task_row(WS_A, SESSION_A, "hi")],
            conversation_snapshot: Some(selection_snapshot(
                SESSION_A,
                2,
                PROVIDER,
                "synthetic-model-a",
                Some("low"),
                &["low", "high"],
            )),
            model_selection_view: Some(json!({
                "data": {"providers": [
                    {"providerId": "acct-other", "models": [
                        {"modelId": "other-1", "config": {"optionSpecs": {"reasoningLevel": {"values": ["x"]}}}}
                    ]},
                    {"providerId": PROVIDER, "models": [
                        {"modelId": "synthetic-model-a", "config": {"optionSpecs": {"reasoningLevel": {"values": ["low", "high"]}}}},
                        {"modelId": "synthetic-model-b", "config": {"optionSpecs": {"reasoningLevel": {"values": ["low"]}}}}
                    ]}
                ]}
            })),
            ..PeerConfig::default()
        };
        let (manager, peer) = manager_with(config).await;
        let target = manager.bind(&key(SESSION_A)).await.expect("bind");
        let (view, selection) = SelectionPort::load(manager.as_ref(), &target)
            .await
            .expect("load");
        assert_eq!(view.session_id, SESSION_A);
        assert!(view.ready);
        assert_eq!(selection.provider, PROVIDER);
        assert_eq!(selection.model, "synthetic-model-a");
        assert_eq!(selection.thought.as_deref(), Some("low"));
        let ids: Vec<&str> = selection
            .models
            .iter()
            .map(|choice| choice.model_id.as_str())
            .collect();
        assert_eq!(ids, ["synthetic-model-a", "synthetic-model-b"]);
        assert!(!ids.contains(&"other-1"), "其他账号 provider 目录绝不下发");

        let requests: Vec<TestMessage> = channel_frames(&peer)
            .into_iter()
            .filter(|message| message.method.as_deref() == Some(MODEL_SELECTION_GET_VIEW_METHOD))
            .collect();
        assert_eq!(requests.len(), 1, "恰一次只读 getView");
        assert_eq!(
            requests[0].channel.as_deref(),
            Some(MODEL_SELECTION_CHANNEL)
        );
        assert_eq!(requests[0].body, json!([]), "getView 必须无参");
    }

    /// 快照扁平 `config.thoughtLevels` 必须经**生产** `BridgeManager::load` 回退到
    /// 当前模型（getView 未宣告该模型档位时）；不是只在纯解析器里成立。
    #[tokio::test]
    async fn snapshot_thought_levels_fallback_works_through_bridge_load() {
        const PROVIDER: &str = "synthetic-provider";
        let config = PeerConfig {
            workspaces: vec![workspace_entry(WS_A, Some("ws-a-identity"), "A")],
            tasks: vec![task_row(WS_A, SESSION_A, "hi")],
            conversation_snapshot: Some(selection_snapshot(
                SESSION_A,
                2,
                PROVIDER,
                "synthetic-model-a",
                Some("high"),
                &["low", "high"],
            )),
            // 当前 provider 目录在，但当前模型没有 optionSpecs：唯一合法来源是快照。
            model_selection_view: Some(json!({
                "providers": [{
                    "providerId": PROVIDER,
                    "models": [{"modelId": "synthetic-model-a"}],
                }]
            })),
            ..PeerConfig::default()
        };
        let (manager, _peer) = manager_with(config).await;
        let target = manager.bind(&key(SESSION_A)).await.expect("bind");
        let (_, selection) = SelectionPort::load(manager.as_ref(), &target)
            .await
            .expect("load");
        let current = selection
            .models
            .iter()
            .find(|choice| choice.model_id == selection.model)
            .expect("current model");
        assert_eq!(
            current.thought_levels,
            vec!["low", "high"],
            "快照扁平档位必须经生产 load 回退到当前模型"
        );
    }

    /// 生效确认必须有**新鲜且 ready** 的精确选择：revision 未前进、模型不等、
    /// 投影 not ready 一律 false；目标换代/绑定不符立即收口。
    #[tokio::test]
    async fn wait_selection_requires_ready_and_exact_match() {
        const PROVIDER: &str = "synthetic-provider";
        const SUB: &str = "conv-sub-synthetic-1";
        let config = PeerConfig {
            workspaces: vec![workspace_entry(WS_A, Some("ws-a-identity"), "A")],
            tasks: vec![task_row(WS_A, SESSION_A, "hi")],
            conversation_snapshot: Some(selection_snapshot(
                SESSION_A,
                2,
                PROVIDER,
                "synthetic-model-a",
                Some("low"),
                &["low", "high"],
            )),
            ..PeerConfig::default()
        };
        let (manager, peer) = manager_with(config).await;
        let target = manager.bind(&key(SESSION_A)).await.expect("bind");
        let baseline = manager
            .session(&target)
            .await
            .expect("session")
            .revision
            .expect("revision");
        assert_eq!(baseline, 2);

        // ① revision 未前进：即便已在目标值也不是「已生效」的新鲜证据。
        assert!(
            !manager
                .wait_selection(
                    &target,
                    PROVIDER,
                    "synthetic-model-a",
                    "low",
                    baseline,
                    Duration::from_millis(40),
                )
                .await
        );

        // ② 等待中 revision 前进 + 三项精确 → true。
        let waiter = {
            let manager = manager.clone();
            let target = target.clone();
            tokio::spawn(async move {
                manager
                    .wait_selection(
                        &target,
                        "synthetic-provider",
                        "synthetic-model-b",
                        "low",
                        baseline,
                        Duration::from_millis(600),
                    )
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        push_selection_delta_on(
            &peer,
            SESSION_A,
            SUB,
            1,
            2,
            3,
            PROVIDER,
            "synthetic-model-b",
            "low",
        )
        .await;
        assert!(waiter.await.expect("waiter task"), "新鲜精确选择必须确认");

        // ③ 模型不等：false（当前已是 model-b）。
        assert!(
            !manager
                .wait_selection(
                    &target,
                    PROVIDER,
                    "synthetic-model-a",
                    "low",
                    baseline,
                    Duration::from_millis(40),
                )
                .await
        );

        // ④ 投影 not ready（revision 已前进且三项精确）：绝不假确认。
        lock(&manager.shared.state)
            .sessions
            .get_mut(SESSION_A)
            .expect("bound session")
            .projection
            .set_ready(false);
        assert!(
            !manager
                .wait_selection(
                    &target,
                    PROVIDER,
                    "synthetic-model-b",
                    "low",
                    baseline,
                    Duration::from_millis(40),
                )
                .await,
            "not ready must never confirm"
        );

        // ⑤ 同 sessionId 但 workspace 绑定不符：立即 false（绝不读兄弟投影）。
        let mut forged = target.clone();
        forged.workspace_key = "ws-b-identity".to_owned();
        assert!(
            !manager
                .wait_selection(
                    &forged,
                    PROVIDER,
                    "synthetic-model-b",
                    "low",
                    baseline,
                    Duration::from_millis(120),
                )
                .await,
            "forged binding must never confirm"
        );
    }
}
