//! 会话 topic 的精确投影（生产计划 T2.2）。
//!
//! 输入是**已解包**的 conversation logical frame（`{topic,subscriptionId,fromSeq,toSeq,
//! sentAt,payload}`，官方 `zcode-protocol-v4/transport.ts:163-191`），由
//! [`BridgeManager`](super::bridge::BridgeManager) 在完成「session/topic/subscriptionId」
//! 第一道闸之后送入。
//!
//! 本层职责：
//! - **绑定校验**：generation 必须等于绑定代际；topic 必须是 `conversation/<sessionId>`；
//!   subscriptionId 必须等于本地已激活的订阅（ACK 前/旧订阅一律零应用）；
//! - **snapshot + deltas**：snapshot 整体建立视图（revision/control/pending/config），
//!   deltas 只认 `op="state.updated"` 的键级整体替换；logical `fromSeq/toSeq` 出现缺口
//!   （或重复）时置 `ready=false`（缺口必须由上层重订阅取新快照，绝不带洞继续）；
//! - **零跨会话泄漏**：视图只来自绑定 sessionId 的帧，异会话帧不产生任何状态变化。
//!
//! `ready` 的语义：收到本订阅的快照且此后没有缺口/协议不符。ACK 到达不等于快照到达，
//! 因此绑定后尚未收到首个快照时 `ready=false`。

use serde_json::{Map, Value};

use super::pending::PendingBook;
use super::types::{
    codes, BoundTarget, CapabilityView, InputRoutingView, ModelChoice, QueueActionAvailability,
    QueueItemView, QueueView, SelectionView, SessionView, ZResult, ZcodeError,
};

/// conversation topic 前缀（`transport.ts:113,205`）。
pub const CONVERSATION_TOPIC_PREFIX: &str = "conversation/";
/// 单条 logical frame 的字节上限（= codec 的 channel 上限 16 MiB；超限拒收）。
pub const MAX_LOGICAL_FRAME_BYTES: usize = 16 * 1024 * 1024;

fn unsupported() -> ZcodeError {
    ZcodeError::of(codes::UNSUPPORTED_PROTOCOL)
}

/// 单会话投影（绑定一个 [`BoundTarget`]）。
#[derive(Debug, Clone)]
pub struct Projection {
    target: BoundTarget,
    subscription_id: Option<String>,
    revision: Option<u64>,
    ready: bool,
    last_to_seq: Option<u64>,
    book: PendingBook,
    selection: Option<SelectionView>,
    /// 官方输入队列（`state.queue` 的只读投影；宿主未宣告 = `known=false`）。
    queue: QueueView,
    /// 官方 `state.inputRouting` 的最小投影（未宣告 = `None`）。
    input_routing: Option<InputRoutingView>,
    execution_terminal: Option<super::types::ExecutionTerminal>,
    capabilities: CapabilityView,
    applied_frames: usize,
    /// 单帧字节上限（超限 = `payload_too_large`，绝不让单帧撑爆投影）。
    byte_limit: usize,
}

impl Projection {
    /// 以精确绑定构造（尚未激活订阅、尚未就绪）。
    pub fn new(target: BoundTarget) -> Self {
        Self::with_limit(target, MAX_LOGICAL_FRAME_BYTES)
    }

    /// 注入单帧字节上限（桥接层按自身预算构造；测试用更小值验证边界）。
    pub fn with_limit(target: BoundTarget, byte_limit: usize) -> Self {
        let book = PendingBook::new(target.session_id.clone(), target.generation);
        Self {
            target,
            subscription_id: None,
            revision: None,
            ready: false,
            last_to_seq: None,
            book,
            selection: None,
            queue: QueueView::default(),
            input_routing: None,
            execution_terminal: None,
            capabilities: CapabilityView::default(),
            applied_frames: 0,
            byte_limit,
        }
    }

    /// 本地 barrier：订阅 ACK 落地后激活 subscriptionId（此前到达的帧由桥暂存）。
    pub fn activate(&mut self, subscription_id: &str) {
        self.subscription_id = Some(subscription_id.to_owned());
    }

    /// 解除本地 barrier（重订阅前调用：新订阅的帧重新进入暂存，旧订阅 id 立即失效）。
    pub fn deactivate(&mut self) {
        self.subscription_id = None;
        self.ready = false;
        self.last_to_seq = None;
        // 旧订阅的队列快照不得跨订阅/换代复活：解除 barrier 即视为未宣告。
        self.queue = QueueView::default();
        // 发送路由同理：旧订阅的 `inputRouting` 不得影响换代后的 choice 判定。
        self.input_routing = None;
    }

    /// 当前激活的 subscriptionId（未激活 = `None`）。
    pub fn subscription_id(&self) -> Option<&str> {
        self.subscription_id.as_deref()
    }

    /// 绑定目标（只读）。
    pub fn target(&self) -> &BoundTarget {
        &self.target
    }

    /// 是否新鲜（收到快照且此后无缺口）。
    pub fn is_ready(&self) -> bool {
        self.ready
    }

    /// 已应用的帧数（证据/测试用）。
    pub fn applied_frames(&self) -> usize {
        self.applied_frames
    }

    /// 本会话 pending 册（只读；`BridgeManager` 的 `PendingPort` 用它读宿主观察代数）。
    pub(crate) fn pending_book(&self) -> &PendingBook {
        &self.book
    }

    /// 本会话 pending 册（可写；`BridgeManager` 的 `PendingPort` 授权入口在这里置
    /// 本地 in-flight，**不**触碰宿主集合）。
    pub(crate) fn pending_book_mut(&mut self) -> &mut PendingBook {
        &mut self.book
    }

    /// 投影降级：置不新鲜并**清空队列快照**。
    ///
    /// 降级（缺序/坏帧/超限/协议不符）之后宿主队列已不可信：旧队列项不得挂在
    /// `ready=false` 的视图上继续展示（R2 接线前就必须成立，否则手机会看到
    /// 「已失效投影 + 仍在排队」的自相矛盾状态）。
    fn mark_unready(&mut self) {
        self.ready = false;
        self.execution_terminal = None;
        self.queue = QueueView::default();
        // 失效视图不得继续携带发送路由：手机只能按新鲜 ready 的状态判断 choice。
        self.input_routing = None;
    }

    /// 显式置就绪位（缺序/协议不符时由桥置 false，取新快照后由 apply 恢复）。
    pub fn set_ready(&mut self, ready: bool) {
        self.ready = ready;
    }

    /// 应用一条 conversation logical frame。
    ///
    /// 错误语义（上层据此决定是否重订阅）：
    /// - `stale_generation`：代际/订阅已过期（零应用，不改变就绪位）；
    /// - `no_session`：不是绑定会话的 topic（零应用，不改变就绪位）；
    /// - `not_ready`：尚未激活/存在 logical 缺口（置 `ready=false`）；
    /// - `unsupported_protocol`：未知 payload schema（置 `ready=false`）。
    pub fn apply(&mut self, generation: u64, frame: Value) -> ZResult<()> {
        if generation != self.target.generation {
            return Err(ZcodeError::of(codes::STALE_GENERATION));
        }
        // 字节上限（quality I-5）：单帧超预算即拒收，避免投影被单帧撑爆。
        let frame_bytes = serde_json::to_vec(&frame).map(|bytes| bytes.len());
        if frame_bytes.map_or(true, |bytes| bytes > self.byte_limit) {
            self.mark_unready();
            return Err(ZcodeError::of(codes::PAYLOAD_TOO_LARGE));
        }
        let object = frame.as_object().ok_or_else(unsupported)?;
        let expected_topic = format!("{CONVERSATION_TOPIC_PREFIX}{}", self.target.session_id);
        if object.get("topic").and_then(Value::as_str) != Some(expected_topic.as_str()) {
            return Err(ZcodeError::of(codes::NO_SESSION));
        }
        let Some(bound) = self.subscription_id.as_deref() else {
            self.mark_unready();
            return Err(ZcodeError::of(codes::NOT_READY));
        };
        if object.get("subscriptionId").and_then(Value::as_str) != Some(bound) {
            return Err(ZcodeError::of(codes::STALE_GENERATION));
        }
        let Some(payload) = object.get("payload").and_then(Value::as_object) else {
            self.mark_unready();
            return Err(unsupported());
        };
        match payload.get("kind").and_then(Value::as_str) {
            Some("snapshot") => {
                let Some(snapshot) = payload.get("snapshot") else {
                    self.mark_unready();
                    return Err(unsupported());
                };
                // logical `toSeq` 是快照水位的必填字段：缺席 = schema 不符，不能
                // 以「无水位」进入 ready（否则后续所有缺口/去重检查永久失效）。
                let Some(to_seq) = object.get("toSeq").and_then(Value::as_u64) else {
                    self.mark_unready();
                    return Err(unsupported());
                };
                self.apply_snapshot(snapshot)?;
                self.last_to_seq = Some(to_seq);
                self.ready = true;
                self.applied_frames = self.applied_frames.saturating_add(1);
                Ok(())
            }
            Some("deltas") => {
                if !self.ready {
                    // 缺口之后不得带洞应用增量；必须等新快照。
                    return Err(ZcodeError::of(codes::NOT_READY));
                }
                let Some(deltas) = payload.get("deltas").and_then(Value::as_array) else {
                    self.mark_unready();
                    return Err(unsupported());
                };
                // 增量必须携带完整 logical 水位（fromSeq/toSeq）；任一缺席都不得
                // 「照常应用 + 保持 ready」。
                let (Some(from_seq), Some(to_seq)) = (
                    object.get("fromSeq").and_then(Value::as_u64),
                    object.get("toSeq").and_then(Value::as_u64),
                ) else {
                    self.mark_unready();
                    return Err(unsupported());
                };
                if from_seq > to_seq {
                    self.mark_unready();
                    return Err(unsupported());
                }
                let Some(last) = self.last_to_seq else {
                    self.mark_unready();
                    return Err(ZcodeError::of(codes::NOT_READY));
                };
                if to_seq <= last {
                    // 重复帧（去重窗口内）：幂等忽略，不重复进账、不改 revision。
                    return Ok(());
                }
                if from_seq != last {
                    // 官方 logical 区间按 [fromSeq, toSeq] 闭合衔接：新帧必须从当前
                    // 水位起（from == last）。缺口（from > last）或重叠乱序
                    // （from < last 但 to > last）都不得带洞应用，等新快照。
                    self.mark_unready();
                    return Err(ZcodeError::of(codes::NOT_READY));
                }
                let prior_revision = self.revision;
                self.execution_terminal = None;
                let mut terminals = Vec::new();
                for delta in deltas {
                    if let Some(row) = delta
                        .get("row")
                        .filter(|_| delta.get("op").and_then(Value::as_str) == Some("row.upserted"))
                    {
                        if row.get("kind").and_then(Value::as_str) == Some("turnHeader") {
                            let result = match row.get("state").and_then(Value::as_str) {
                                Some("completedSuccess") => Some("success"),
                                Some("completedInterrupted") => Some("cancelled"),
                                Some("failed") => Some("failed"),
                                _ => None,
                            };
                            if let (Some(input), Some(result)) = (
                                row.get("sourceCommandId")
                                    .and_then(Value::as_str)
                                    .filter(|v| !v.is_empty()),
                                result,
                            ) {
                                terminals.push((input.to_owned(), result.to_owned()));
                            }
                        }
                    }
                    let Some(record) = delta.as_object() else {
                        continue;
                    };
                    if record.get("op").and_then(Value::as_str) != Some("state.updated") {
                        continue;
                    }
                    let Some(patch) = record.get("patch") else {
                        continue;
                    };
                    if !patch.is_object() {
                        self.mark_unready();
                        return Err(unsupported());
                    }
                    self.apply_patch(patch)?;
                }
                if terminals.len() == 1
                    && self
                        .revision
                        .is_some_and(|r| prior_revision.is_some_and(|old| r > old))
                {
                    let (input_id, result_type) = terminals.remove(0);
                    self.execution_terminal = Some(super::types::ExecutionTerminal {
                        input_id,
                        result_type,
                        revision: self.revision.unwrap(),
                    });
                }
                self.last_to_seq = Some(to_seq);
                self.applied_frames = self.applied_frames.saturating_add(1);
                Ok(())
            }
            _ => {
                self.mark_unready();
                Err(unsupported())
            }
        }
    }

    /// 本会话官方输入队列的只读投影（宿主未宣告 = `known=false`）。
    pub fn queue(&self) -> &QueueView {
        &self.queue
    }

    /// 当前视图（手机唯一可见的会话状态面）。
    pub fn view(&self) -> SessionView {
        SessionView {
            session_id: self.target.session_id.clone(),
            generation: self.target.generation,
            revision: self.revision,
            ready: self.ready,
            pending: self.book.pendings(),
            control: self.book.control().cloned(),
            selection: self.selection.clone(),
            capabilities: self.capabilities.clone(),
            // 队列随投影一起走：GET 与 SSE 帧共用本视图，手机刷新/重进即从宿主重建。
            queue: self.queue.clone(),
            execution_terminal: self.execution_terminal.clone(),
            input_routing: self.input_routing.clone(),
        }
    }

    fn apply_snapshot(&mut self, snapshot: &Value) -> ZResult<()> {
        let object = snapshot.as_object().ok_or_else(unsupported)?;
        self.execution_terminal = None;
        self.book.accept_snapshot(snapshot)?;
        self.revision = object.get("revision").and_then(Value::as_u64);
        self.selection = object.get("config").and_then(parse_selection);
        // 快照是整份权威状态：`queue` 缺席 = 本宿主不宣告队列（`known=false`），
        // 绝不沿用上一份快照的旧队列（旧代际/旧订阅的项不得复活）。
        let mut queue = object.get("queue").map(parse_queue).unwrap_or_default();
        // `availability` 位于**状态根**（`state.availability`），与 `queue` 平级；
        // 只有队列已宣告时才附带资格，否则它挂在「未知队列」上毫无意义。
        if queue.known {
            queue.availability = parse_queue_availability(snapshot);
        }
        self.queue = queue;
        // 快照是整份权威状态：缺 `inputRouting` = 未知（`None`），绝不沿用上一份快照。
        self.input_routing = object.get("inputRouting").and_then(parse_input_routing);
        Ok(())
    }

    fn apply_patch(&mut self, patch: &Value) -> ZResult<()> {
        let object = patch.as_object().ok_or_else(unsupported)?;
        if let Some(revision) = object.get("revision").and_then(Value::as_u64) {
            self.revision = Some(revision);
        }
        // 键级整体替换：patch 带 config 就整体换（含解析失败 → None），不做深合并。
        if object.contains_key("config") {
            self.selection = object.get("config").and_then(parse_selection);
        }
        // 同规则：patch 带 queue 就整体换（含解析失败 → 未宣告）；缺席 = 本帧不动队列。
        if let Some(queue) = object.get("queue") {
            let mut parsed = parse_queue(queue);
            if parsed.known {
                // 增量纪律：本帧带 `availability` 就按本帧重算；不带则**保持旧值**
                // （缺字段 ≠ 收回资格，也 ≠ 维持一份更旧的声明——见下一分支）。
                parsed.availability = if object.contains_key("availability") {
                    parse_queue_availability(patch)
                } else {
                    self.queue.availability.clone()
                };
            }
            self.queue = parsed;
        } else if object.contains_key("availability") && self.queue.known {
            // 队列项本帧未变、但资格声明变了：只更新资格，不动 items/暂停态。
            self.queue.availability = parse_queue_availability(patch);
        }
        // 增量 patch 缺 `inputRouting` 保持旧值；带坏形状 → 未知（绝不猜 choice）。
        if object.contains_key("inputRouting") {
            self.input_routing = object.get("inputRouting").and_then(parse_input_routing);
        }
        self.book.accept_delta_patch(patch)
    }
}

/// 解析官方 `state.queue`（`{items[], autoDrain, pauseReason?}`）。
///
/// 纪律：
/// - `items` 必须是数组、`autoDrain` 必须是布尔，否则整份队列按「未宣告」处理
///   （`known=false`）——绝不把坏形状猜成空队列；
/// - 单项坏形状只跳过该项（不丢整份队列、不丢身份）；
/// - 身份只取宿主原值：`queueItemId` 原样作 `input_id`，`order.queuePosition` 原样作
///   `position`；不按正文/数组下标重算身份；
/// - 暂停原因是官方枚举 `stopped|manual|error` 的**原样**透传；非枚举/缺席一律
///   `None`（绝不把未知值映射成某一种原因）；
/// - 数组顺序即官方展示顺序（宿主已按 `order.queuePosition` 排好），本层不重排；
/// - `kind` / `dispatch.state` 只按证据白名单投影（未知值 = `None`，绝不透传任意
///   字符串）；`has_attachments` 只回答「有没有」，不下发附件对象；
/// - `availability` 位于状态根，不在这里解析（调用方用 [`parse_queue_availability`]
///   补齐）；本函数产出的 `availability` 恒为默认（未知/全关）。
fn parse_queue(value: &Value) -> QueueView {
    let Some(object) = value.as_object() else {
        return QueueView::default();
    };
    let (Some(items), Some(auto_drain)) = (
        object.get("items").and_then(Value::as_array),
        object.get("autoDrain").and_then(Value::as_bool),
    ) else {
        return QueueView::default();
    };
    // 官方暂停是容器级状态：`autoDrain=false` 时即使 `items` 已空也保留暂停事实
    // （R0 §3：`{…queue, autoDrain:!1, pauseReason:"stopped"|"error"}`）。
    let pause_reason = object
        .get("pauseReason")
        .and_then(Value::as_str)
        .filter(|reason| matches!(*reason, "stopped" | "manual" | "error"))
        .map(str::to_owned);
    let mut queue_items = Vec::with_capacity(items.len());
    for item in items {
        let Some(record) = item.as_object() else {
            continue;
        };
        let Some(input_id) = record
            .get("queueItemId")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        else {
            continue;
        };
        let Some(text) = record.get("text").and_then(Value::as_str) else {
            continue;
        };
        let position = record
            .get("order")
            .and_then(Value::as_object)
            .and_then(|order| order.get("queuePosition"))
            .and_then(Value::as_u64)
            .and_then(|position| u32::try_from(position).ok());
        // 证据白名单（R0 §3 `Ax` schema）：只认这三种；其余取值一律 `None`
        // （前端据此禁用「修改」，绝不对未知 kind 猜语义）。
        let kind = record
            .get("kind")
            .and_then(Value::as_str)
            .filter(|kind| matches!(*kind, "sendText" | "sendGoalCommand" | "compact"))
            .map(str::to_owned);
        let dispatch_state = record
            .get("dispatch")
            .and_then(Value::as_object)
            .and_then(|dispatch| dispatch.get("state"))
            .and_then(Value::as_str)
            .filter(|state| {
                matches!(
                    *state,
                    "admitted" | "queued" | "reserved" | "promoting" | "drained"
                )
            })
            .map(str::to_owned);
        let has_attachments = record
            .get("attachments")
            .and_then(Value::as_array)
            .is_some_and(|list| !list.is_empty());
        queue_items.push(QueueItemView {
            input_id: input_id.to_owned(),
            text: text.to_owned(),
            position,
            kind,
            dispatch_state,
            has_attachments,
        });
    }
    QueueView {
        known: true,
        paused: Some(!auto_drain),
        pause_reason,
        items: queue_items,
        availability: QueueActionAvailability::default(),
    }
}

/// 解析官方 `state.availability`（入参是**状态对象**（snapshot/patch 根），不是裸
/// `availability`）。
///
/// 证据（R0 §3）：`state.availability.{queueEdit,sendQueuedNow,…} = {allowed:true} |
/// {allowed:false,reasonCode}`。纪律：
/// - 根上 `availability` 不是对象 → `known=false`（未知，绝不猜 allowed）；
/// - 对象在但缺 `queueEdit`/`sendQueuedNow` → 对应位 `false`（宿主未宣告该项 = 不放行）；
/// - `allowed` 必须**恰好**是 `true` 才放行（`"true"`/`1`/缺省一律 false）；
/// - 官方没有 `setAutoDrain` 的 availability 字段，因此**不虚构**恢复资格。
fn parse_queue_availability(state: &Value) -> QueueActionAvailability {
    let Some(available) = state.get("availability").and_then(Value::as_object) else {
        return QueueActionAvailability::default();
    };
    let allow = |name: &str| {
        available
            .get(name)
            .and_then(Value::as_object)
            .and_then(|entry| entry.get("allowed"))
            .and_then(Value::as_bool)
            == Some(true)
    };
    QueueActionAvailability {
        known: true,
        queue_edit_allowed: allow("queueEdit"),
        send_queued_now_allowed: allow("sendQueuedNow"),
    }
}

/// 解析官方 `state.inputRouting` 的最小值域（`mode` + 可选 `reasonCode`）。
///
/// - 非对象或 `mode` 非非空字符串 → `None`（未知，绝不猜 `choice`）；
/// - `reasonCode` 原样透传（非字符串/缺席 = `None`）；
/// - 其余字段一律不投影（不把宿主完整路由对象/敏感配置下发给手机）。
fn parse_input_routing(value: &Value) -> Option<InputRoutingView> {
    let object = value.as_object()?;
    let mode = object
        .get("mode")
        .and_then(Value::as_str)
        .filter(|mode| !mode.is_empty())?;
    let reason_code = object
        .get("reasonCode")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Some(InputRoutingView {
        mode: mode.to_owned(),
        reason_code,
    })
}

/// 解析 snapshot/patch 的 `config`（只读展示面；模型目录由 T6 的独立读取面填充）。
///
/// 官方快照 config 的形状是扁平的 `{provider, model, thought, thoughtLevels}`
/// （B4，无 `config.models`）：`thoughtLevels` 只属于**当前模型**，必须挂到当前模型
/// 名下保存，供 `BridgeManager::load` 在 getView 未宣告该模型档位时回退；其他模型
/// 不获得任何档位（绝不做 provider 级并集，也不虚构候选项）。
fn parse_selection(config: &Value) -> Option<SelectionView> {
    let object: &Map<String, Value> = config.as_object()?;
    let provider = object
        .get("provider")?
        .as_str()
        .filter(|text| !text.is_empty())?;
    let model = object
        .get("model")?
        .as_str()
        .filter(|text| !text.is_empty())?;
    let thought = object
        .get("thought")
        .and_then(Value::as_str)
        .map(str::to_owned);
    // 扁平档位（仅当前模型）：去重、跳过非字符串/空串，绝不带入其他模型。
    let mut snapshot_levels: Vec<String> = Vec::new();
    if let Some(levels) = object.get("thoughtLevels").and_then(Value::as_array) {
        for level in levels {
            let Some(level) = level.as_str().filter(|text| !text.is_empty()) else {
                continue;
            };
            if !snapshot_levels.iter().any(|seen| seen == level) {
                snapshot_levels.push(level.to_owned());
            }
        }
    }
    let mut models = Vec::new();
    if let Some(items) = object.get("models").and_then(Value::as_array) {
        for item in items {
            let Some(record) = item.as_object() else {
                continue;
            };
            let Some(model_id) = record.get("modelId").and_then(Value::as_str) else {
                continue;
            };
            let thought_levels = record
                .get("thoughtLevels")
                .and_then(Value::as_array)
                .map(|levels| {
                    levels
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            models.push(ModelChoice {
                model_id: model_id.to_owned(),
                thought_levels,
            });
        }
    }
    match models.iter_mut().find(|choice| choice.model_id == model) {
        Some(current) if current.thought_levels.is_empty() => {
            current.thought_levels = snapshot_levels;
        }
        Some(_) => {}
        None => models.push(ModelChoice {
            model_id: model.to_owned(),
            thought_levels: snapshot_levels,
        }),
    }
    Some(SelectionView {
        provider: provider.to_owned(),
        model: model.to_owned(),
        thought,
        models,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SESSION: &str = "sess_0f9a1c0d-0000-4000-8000-000000000001";
    const SIBLING: &str = "sess_0f9a1c0d-0000-4000-8000-000000000002";
    const SUB: &str = "conv-sub-synthetic-1";

    fn target(generation: u64) -> BoundTarget {
        BoundTarget {
            session_id: SESSION.to_owned(),
            workspace_path: "/tmp/synthetic-workspace".to_owned(),
            workspace_identity: None,
            workspace_key: "/tmp/synthetic-workspace".to_owned(),
            generation,
            bridge_generation: None,
        }
    }

    fn snapshot_frame(sub: &str, snapshot: Value, from: u64, to: u64) -> Value {
        json!({
            "topic": format!("conversation/{SESSION}"),
            "subscriptionId": sub,
            "fromSeq": from,
            "toSeq": to,
            "sentAt": 0,
            "payload": {"kind": "snapshot", "snapshot": snapshot}
        })
    }

    fn delta_frame(sub: &str, patch: Value, from: u64, to: u64) -> Value {
        json!({
            "topic": format!("conversation/{SESSION}"),
            "subscriptionId": sub,
            "fromSeq": from,
            "toSeq": to,
            "sentAt": 0,
            "payload": {"kind": "deltas", "deltas": [{"op": "state.updated", "patch": patch}]}
        })
    }

    fn base_snapshot() -> Value {
        json!({
            "protocolVersion": 1,
            "sessionId": SESSION,
            "logEpoch": "synthetic-epoch",
            "seq": 1,
            "revision": 4,
            "control": {"activeWorks": []},
            "pendingInteractions": [{
                "interactionId": "pi-synthetic-1001",
                "kind": "permission",
                "anchorRowId": 3,
                "createdAt": 1_700_000_000_000u64,
                "payload": {
                    "kind": "permission",
                    "toolCallId": "call_synthetic_1001",
                    "toolName": "Write",
                    "summary": "synthetic: write",
                    "options": [{"optionId": "allowOnce", "label": "Allow once", "kind": "allowOnce"}]
                }
            }],
            "config": {
                "provider": "synthetic-provider",
                "model": "synthetic-model-a",
                "thought": "high",
                "thoughtLevels": ["low", "high"]
            }
        })
    }

    #[test]
    fn foreign_topic_or_old_subscription_is_rejected() {
        let mut projection = Projection::new(target(1));
        projection.activate(SUB);

        // 正控：本会话 + 当前订阅的快照必须被应用（否则「零应用」断言没有意义）。
        projection
            .apply(1, snapshot_frame(SUB, base_snapshot(), 0, 1))
            .expect("matching snapshot");
        let mut foreign_and_stale_frames_applied = projection.applied_frames();
        assert_eq!(foreign_and_stale_frames_applied, 1);
        assert!(projection.view().ready);

        // 异会话 topic：workspace 级事件会广播兄弟会话帧，绝不能进本投影。
        let mut foreign = snapshot_frame(SUB, base_snapshot(), 0, 1);
        foreign["topic"] = json!(format!("conversation/{SIBLING}"));
        assert!(projection
            .apply(1, foreign)
            .unwrap_err()
            .is(codes::NO_SESSION));

        // 旧 subscriptionId（重订阅前/换代后迟到）：零应用。
        let mut stale = delta_frame(SUB, json!({"revision": 5}), 1, 2);
        stale["subscriptionId"] = json!("conv-sub-old");
        assert!(projection
            .apply(1, stale)
            .unwrap_err()
            .is(codes::STALE_GENERATION));

        // 非 conversation topic（sessions-index 等）同样拒收。
        let mut other_topic = snapshot_frame(SUB, base_snapshot(), 0, 1);
        other_topic["topic"] = json!("sessions-index");
        assert!(projection
            .apply(1, other_topic)
            .unwrap_err()
            .is(codes::NO_SESSION));

        foreign_and_stale_frames_applied = projection.applied_frames() - 1;
        assert_eq!(foreign_and_stale_frames_applied, 0);
        // 拒收不污染既有有效视图。
        assert!(projection.view().ready);
        assert_eq!(projection.view().revision, Some(4));
    }

    #[test]
    fn terminal_turn_row_projects_only_correlated_minimal_fact() {
        let mut p = Projection::new(target(1));
        p.activate(SUB);
        p.apply(1, snapshot_frame(SUB, base_snapshot(), 0, 1))
            .unwrap();
        let mut frame = delta_frame(SUB, json!({"revision": 5}), 1, 2);
        frame["payload"]["deltas"].as_array_mut().unwrap().push(json!({"op":"row.upserted", "row":{"kind":"turnHeader", "sourceCommandId":"input", "state":"completedSuccess", "text":"do not propagate"}}));
        p.apply(1, frame).unwrap();
        let v = serde_json::to_value(p.view()).unwrap();
        assert_eq!(v["executionTerminal"]["inputId"], "input");
        assert_eq!(v["executionTerminal"]["resultType"], "success");
        assert_eq!(v["executionTerminal"]["revision"], 5);
        assert!(v["executionTerminal"].get("text").is_none());
    }
    #[test]
    fn snapshot_then_deltas_project_pending_control_and_selection() {
        let mut projection = Projection::new(target(1));
        projection.activate(SUB);
        projection
            .apply(1, snapshot_frame(SUB, base_snapshot(), 0, 1))
            .expect("snapshot");

        let view = projection.view();
        assert!(view.ready);
        assert_eq!(view.session_id, SESSION);
        assert_eq!(view.revision, Some(4));
        assert_eq!(view.pending.len(), 1);
        assert_eq!(view.pending[0].interaction_id, "pi-synthetic-1001");
        assert_eq!(
            view.control.expect("control observed").active_works.len(),
            0
        );
        let selection = view.selection.expect("selection");
        assert_eq!(selection.provider, "synthetic-provider");
        assert_eq!(selection.model, "synthetic-model-a");
        assert_eq!(selection.thought.as_deref(), Some("high"));
        // 官方快照 config 只有扁平的当前模型档位（无 models 目录）：投影只把这份
        // 档位挂到**当前模型**名下，绝不虚构其他模型。BridgeManager::load 以它为
        // getView 缺档位时的当前模型回退。
        assert_eq!(selection.models.len(), 1);
        assert_eq!(selection.models[0].model_id, "synthetic-model-a");
        assert_eq!(selection.models[0].thought_levels, vec!["low", "high"]);
        assert!(!view.capabilities.send.enabled);

        projection
            .apply(
                1,
                delta_frame(
                    SUB,
                    json!({
                        "revision": 5,
                        "pendingInteractions": [],
                        "control": {"activeWorks": [{"kind": "primaryTurn", "foregroundExecutionId": "E-existing"}]},
                        "config": {"provider": "synthetic-provider", "model": "synthetic-model-b"}
                    }),
                    1,
                    2,
                ),
            )
            .expect("deltas");
        let view = projection.view();
        assert_eq!(view.revision, Some(5));
        assert!(view.pending.is_empty());
        assert_eq!(
            view.control.expect("control").active_works[0]
                .foreground_execution_id
                .as_deref(),
            Some("E-existing")
        );
        assert_eq!(
            view.selection.expect("selection").model,
            "synthetic-model-b"
        );
    }

    #[test]
    fn frames_before_activation_are_not_applied() {
        let mut projection = Projection::new(target(1));
        let error = projection
            .apply(1, snapshot_frame(SUB, base_snapshot(), 0, 1))
            .expect_err("ack barrier not released yet");
        assert!(error.is(codes::NOT_READY));
        assert_eq!(projection.applied_frames(), 0);
        assert!(!projection.view().ready);
    }

    #[test]
    fn logical_gap_marks_not_ready() {
        let mut projection = Projection::new(target(1));
        projection.activate(SUB);
        projection
            .apply(1, snapshot_frame(SUB, base_snapshot(), 0, 1))
            .expect("snapshot");
        // fromSeq 跳过 2..4：中间增量已丢，必须重取快照，不能带洞继续。
        let error = projection
            .apply(1, delta_frame(SUB, json!({"revision": 9}), 5, 6))
            .expect_err("logical gap");
        assert!(error.is(codes::NOT_READY));
        assert!(!projection.view().ready);
        assert_eq!(projection.view().revision, Some(4));
    }

    #[test]
    fn duplicate_delta_is_not_reapplied() {
        let mut projection = Projection::new(target(1));
        projection.activate(SUB);
        projection
            .apply(1, snapshot_frame(SUB, base_snapshot(), 0, 1))
            .expect("snapshot");
        let delta = delta_frame(SUB, json!({"revision": 5}), 1, 2);
        projection.apply(1, delta.clone()).expect("first delta");
        let applied_after_first = projection.applied_frames();
        projection.apply(1, delta).expect("duplicate delta");
        assert_eq!(projection.applied_frames(), applied_after_first);
        assert_eq!(projection.view().revision, Some(5));
        assert!(projection.view().ready);
    }

    #[test]
    fn old_generation_is_zero_applied() {
        let mut projection = Projection::new(target(1));
        projection.activate(SUB);
        let error = projection
            .apply(2, snapshot_frame(SUB, base_snapshot(), 0, 1))
            .expect_err("generation moved on");
        assert!(error.is(codes::STALE_GENERATION));
        assert_eq!(projection.applied_frames(), 0);
        assert!(!projection.view().ready);
    }

    #[test]
    fn unknown_payload_kind_marks_not_ready() {
        let mut projection = Projection::new(target(1));
        projection.activate(SUB);
        let mut frame = snapshot_frame(SUB, base_snapshot(), 0, 1);
        frame["payload"]["kind"] = json!("rewind");
        let error = projection.apply(1, frame).expect_err("unknown schema");
        assert!(error.is(codes::UNSUPPORTED_PROTOCOL));
        assert!(!projection.view().ready);
    }

    // -----------------------------------------------------------------------
    // Fix round 1：logical seq 必填（spec I3）与帧字节上限（quality I-5）
    // -----------------------------------------------------------------------

    #[test]
    fn snapshot_without_to_seq_is_not_ready() {
        let mut projection = Projection::new(target(1));
        projection.activate(SUB);
        let mut frame = snapshot_frame(SUB, base_snapshot(), 0, 1);
        frame.as_object_mut().expect("object").remove("toSeq");
        let error = projection
            .apply(1, frame)
            .expect_err("snapshot without toSeq");
        assert!(!projection.view().ready);
        assert_eq!(projection.applied_frames(), 0);
        assert!(error.is(codes::UNSUPPORTED_PROTOCOL) || error.is(codes::NOT_READY));
        // 之后到达的 deltas 不得在无水位的情况下被当作新鲜应用。
        assert!(projection
            .apply(1, delta_frame(SUB, json!({"revision": 5}), 1, 2))
            .is_err());
        assert!(!projection.view().ready);
    }

    #[test]
    fn delta_without_to_seq_marks_not_ready() {
        let mut projection = Projection::new(target(1));
        projection.activate(SUB);
        projection
            .apply(1, snapshot_frame(SUB, base_snapshot(), 0, 1))
            .expect("snapshot");
        let mut delta = delta_frame(SUB, json!({"revision": 5}), 1, 2);
        delta.as_object_mut().expect("object").remove("toSeq");
        assert!(projection
            .apply(1, delta)
            .expect_err("delta without toSeq")
            .is(codes::UNSUPPORTED_PROTOCOL));
        assert!(!projection.view().ready);
        assert_eq!(projection.view().revision, Some(4));
    }

    #[test]
    fn out_of_order_delta_is_not_ready() {
        let mut projection = Projection::new(target(1));
        projection.activate(SUB);
        projection
            .apply(1, snapshot_frame(SUB, base_snapshot(), 0, 1))
            .expect("snapshot");
        // 已应用 1..2 后收到 fromSeq=5：既非重复也非衔接 → 缺口，不得带洞应用。
        assert!(projection
            .apply(1, delta_frame(SUB, json!({"revision": 9}), 5, 6))
            .expect_err("gap")
            .is(codes::NOT_READY));
        assert!(!projection.view().ready);
        assert_eq!(projection.view().revision, Some(4));
    }

    // -----------------------------------------------------------------------
    // R0：队列合同（形状取自安装包 3.16.2.8769 的 `state.queue` 实证）
    // -----------------------------------------------------------------------

    /// 官方 `state.queue` 合成样本：两项，`autoDrain=false` + `pauseReason`（暂停）。
    ///
    /// 字段名/形状来自 `app.asar` 内 zod schema（见
    /// `docs/superpowers/reviews/2026-10-29-zcode-r0-wire-evidence.md` §3）：
    /// `{items[], autoDrain, pauseReason?}`，项 = `{queueItemId, text, order:{queuePosition}}`。
    fn queue_state(paused: bool, items: Value) -> Value {
        let mut queue = json!({"items": items, "autoDrain": !paused});
        if paused {
            queue["pauseReason"] = json!("stopped");
        }
        queue
    }

    fn queue_item(id: &str, text: &str, position: u64) -> Value {
        json!({
            "queueItemId": id,
            "sourceCommandId": id.trim_start_matches("queue_"),
            "clientId": "client-synthetic",
            "kind": "sendText",
            "text": text,
            "delivery": {"requested": "queue", "admitted": "queue"},
            "order": {"admissionSeq": position + 1, "queuePosition": position},
            "dispatch": {"state": "queued"},
            "admittedAt": 1_700_000_000_000u64
        })
    }

    #[test]
    fn queue_snapshot_and_delta_preserve_input_identity() {
        let mut projection = Projection::new(target(1));
        projection.activate(SUB);
        // 宿主未宣告队列时是「未知」，不是「空队列」。
        assert!(!projection.queue().known);
        assert_eq!(projection.queue().paused, None);

        let mut snapshot = base_snapshot();
        snapshot["queue"] = queue_state(
            true,
            json!([
                queue_item("queue_cmd-synthetic-1", "first queued", 0),
                queue_item("queue_cmd-synthetic-2", "second queued", 1),
            ]),
        );
        projection
            .apply(1, snapshot_frame(SUB, snapshot, 0, 1))
            .expect("snapshot with queue");
        let queue = projection.queue().clone();
        assert!(queue.known, "宿主宣告了 queue → 已知");
        assert_eq!(queue.paused, Some(true), "autoDrain=false → 暂停");
        assert_eq!(queue.items.len(), 2);
        // 身份/正文/次序全部保留宿主原值，不按正文或下标重算。
        assert_eq!(queue.items[0].input_id, "queue_cmd-synthetic-1");
        assert_eq!(queue.items[0].text, "first queued");
        assert_eq!(queue.items[0].position, Some(0));
        assert_eq!(queue.items[1].input_id, "queue_cmd-synthetic-2");
        assert_eq!(queue.items[1].position, Some(1));

        // 增量：整对象替换（官方 patch 即整键替换），消费首项 + 恢复 autoDrain。
        projection
            .apply(
                1,
                delta_frame(
                    SUB,
                    json!({
                        "revision": 5,
                        "queue": queue_state(false, json!([queue_item("queue_cmd-synthetic-2", "second queued", 0)]))
                    }),
                    1,
                    2,
                ),
            )
            .expect("queue delta");
        let queue = projection.queue().clone();
        assert_eq!(queue.paused, Some(false), "autoDrain=true → 未暂停");
        assert_eq!(queue.items.len(), 1, "已消费项不得残留");
        assert_eq!(queue.items[0].input_id, "queue_cmd-synthetic-2");
        assert_eq!(queue.items[0].position, Some(0), "宿主重排后的位置原样保留");

        // 缺 queue 键的增量不动队列（不把「本帧没提」当成「已清空」）。
        projection
            .apply(1, delta_frame(SUB, json!({"revision": 6}), 2, 3))
            .expect("unrelated delta");
        assert_eq!(projection.queue().items.len(), 1);

        // 旧代际样本丢弃：零应用，队列原样。
        assert!(projection
            .apply(
                2,
                delta_frame(
                    SUB,
                    json!({"revision": 7, "queue": queue_state(true, json!([]))}),
                    3,
                    4
                )
            )
            .is_err());
        assert_eq!(projection.queue().items.len(), 1);
        assert_eq!(projection.queue().paused, Some(false));

        // 换代（重订阅解除 barrier）：旧队列不得复活。
        projection.deactivate();
        assert!(!projection.queue().known);
        assert!(projection.queue().items.is_empty());
    }

    /// W2 主用例：官方 ID 原样保留；新白名单字段与运行时资格**只**按证据形状投影；
    /// 缺 `queue` ≠ 空队列；`paused=true + []` 仍是可恢复的暂停态。
    #[test]
    fn queue_snapshot_and_delta_keep_official_item_ids() {
        let mut projection = Projection::new(target(1));
        projection.activate(SUB);
        // 快照前：未宣告 → 未知且资格全关。
        assert!(!projection.queue().known);
        assert!(!projection.queue().availability.known);

        let mut snapshot = base_snapshot();
        snapshot["queue"] = queue_state(
            true,
            json!([
                queue_item("queue_cmd-keep-1", "first queued", 0),
                queue_item("queue_cmd-keep-2", "second queued", 1),
            ]),
        );
        // 官方资格在**状态根**：`state.availability.queueEdit/sendQueuedNow = {allowed}`。
        snapshot["availability"] = json!({
            "queueEdit": {"allowed": true},
            "sendQueuedNow": {"allowed": false, "reasonCode": "busy"},
        });
        projection
            .apply(1, snapshot_frame(SUB, snapshot, 0, 1))
            .expect("snapshot with queue availability");

        let queue = projection.queue().clone();
        assert!(queue.known);
        // 身份：仍是宿主原值，不重算、不按下标替换。
        assert_eq!(queue.items[0].input_id, "queue_cmd-keep-1");
        assert_eq!(queue.items[1].input_id, "queue_cmd-keep-2");
        // 白名单字段：kind / dispatch.state / 附件有无。
        assert_eq!(queue.items[0].kind.as_deref(), Some("sendText"));
        assert_eq!(queue.items[0].dispatch_state.as_deref(), Some("queued"));
        assert!(!queue.items[0].has_attachments);
        // 资格：只有明确 allowed=true 的项放行。
        assert!(queue.availability.known);
        assert!(queue.availability.queue_edit_allowed);
        assert!(!queue.availability.send_queued_now_allowed);

        // 增量只换 queue、**不带** availability → 资格保持旧值（缺字段 ≠ 收回）。
        projection
            .apply(
                1,
                delta_frame(
                    SUB,
                    json!({
                        "revision": 5,
                        "queue": queue_state(
                            false,
                            json!([queue_item("queue_cmd-keep-2", "second queued", 0)])
                        )
                    }),
                    1,
                    2,
                ),
            )
            .expect("queue delta without availability");
        let queue = projection.queue().clone();
        assert_eq!(queue.items.len(), 1);
        assert_eq!(
            queue.items[0].input_id, "queue_cmd-keep-2",
            "消费一项后仍保留剩余项的原 ID"
        );
        assert!(
            queue.availability.queue_edit_allowed,
            "增量缺 availability 时保持旧资格"
        );

        // 只带 availability、不动 queue：只更新资格，队列项/暂停态原样。
        projection
            .apply(
                1,
                delta_frame(
                    SUB,
                    json!({"revision": 6, "availability": {"queueEdit": {"allowed": false}}}),
                    2,
                    3,
                ),
            )
            .expect("availability-only delta");
        let queue = projection.queue().clone();
        assert_eq!(queue.items.len(), 1, "资格帧不得改动队列项");
        assert_eq!(queue.paused, Some(false));
        assert!(
            !queue.availability.queue_edit_allowed,
            "官方收回资格必须立刻反映"
        );

        // 官方明确暂停且队列已空：仍是「可恢复」的暂停事实（known=true + paused=true）。
        projection
            .apply(
                1,
                delta_frame(
                    SUB,
                    json!({"revision": 7, "queue": queue_state(true, json!([]))}),
                    3,
                    4,
                ),
            )
            .expect("paused empty queue delta");
        let queue = projection.queue().clone();
        assert!(queue.known && queue.paused == Some(true));
        assert!(queue.items.is_empty());
        assert!(
            queue.availability.known && !queue.availability.queue_edit_allowed,
            "本帧没带 availability → 保持上一帧（已被收回）的资格"
        );

        // 投影降级（缺口）必须连资格一起清空：失效视图不得残留「可写」。
        assert!(projection
            .apply(1, delta_frame(SUB, json!({"revision": 8}), 9, 10))
            .is_err());
        assert!(!projection.view().ready);
        assert!(!projection.queue().known);
        assert!(!projection.queue().availability.known);
        assert!(!projection.queue().availability.queue_edit_allowed);
    }

    /// 队列项的 `kind`/`dispatch.state` 只认证据白名单；未知值一律 `None`
    /// （不把宿主新枚举透传给手机，也不让 W4 对未知 kind 猜「可修改」）。
    #[test]
    fn queue_item_kind_and_dispatch_state_are_whitelisted() {
        let queue = super::parse_queue(&json!({
            "items": [
                {
                    "queueItemId": "queue_unknown_kind",
                    "text": "unknown kind",
                    "kind": "someFutureCommand",
                    "dispatch": {"state": "someFutureState"}
                },
                {
                    "queueItemId": "queue_compact",
                    "text": "compact",
                    "kind": "compact",
                    "dispatch": {"state": "reserved"},
                    "attachments": [{"kind": "file", "path": "/tmp/secret"}]
                }
            ],
            "autoDrain": true
        }));
        assert!(queue.known);
        assert_eq!(queue.items[0].kind, None, "白名单外 kind → None");
        assert_eq!(queue.items[0].dispatch_state, None, "白名单外态 → None");
        assert!(!queue.items[0].has_attachments);
        assert_eq!(queue.items[1].kind.as_deref(), Some("compact"));
        assert_eq!(queue.items[1].dispatch_state.as_deref(), Some("reserved"));
        assert!(queue.items[1].has_attachments, "只告知有无附件");
        // 附件内容（路径等）绝不进入 DTO。
        let wire = serde_json::to_string(&queue.items[1]).expect("可序列化");
        assert!(!wire.contains("/tmp/secret"), "附件路径不得下发：{wire}");
    }

    #[test]
    fn queue_is_unknown_before_host_declaration() {
        // 手机唯一可见的队列面是 `SessionView.queue`：宿主未宣告时必须是「未知」，
        // 而不是「空队列」——两者对用户的意义完全不同（空 = 确认没有排队）。
        let mut projection = Projection::new(target(1));
        projection.activate(SUB);
        let view = projection.view();
        assert!(!view.queue.known, "尚未收到快照时队列必须是未知");
        assert_eq!(view.queue.paused, None, "未知队列不得带暂停结论");
        assert!(view.queue.items.is_empty());

        // 旧载荷/未升级宿主的快照没有 `queue` 键：仍然未知，绝不降级成空队列。
        projection
            .apply(1, snapshot_frame(SUB, base_snapshot(), 0, 1))
            .expect("snapshot without queue");
        let view = projection.view();
        assert!(view.ready, "队列缺席不影响正文可读性");
        assert!(!view.queue.known, "缺 queue 键必须保持未知");
        assert_eq!(view.queue.paused, None, "paused=None 不得当成未暂停");
        assert!(view.queue.items.is_empty());
    }

    #[test]
    fn queue_snapshot_then_paused_delta_reconciles() {
        let mut projection = Projection::new(target(1));
        projection.activate(SUB);
        let mut snapshot = base_snapshot();
        snapshot["queue"] = queue_state(
            true,
            json!([
                queue_item("queue_cmd-reconcile-1", "queued one", 0),
                queue_item("queue_cmd-reconcile-2", "queued two", 1),
            ]),
        );
        projection
            .apply(1, snapshot_frame(SUB, snapshot, 0, 1))
            .expect("snapshot with paused queue");
        let queue = projection.view().queue;
        assert!(queue.known, "宿主宣告 queue → 已知");
        assert_eq!(queue.paused, Some(true), "autoDrain=false → 暂停");
        assert_eq!(queue.items.len(), 2);
        assert_eq!(queue.items[0].input_id, "queue_cmd-reconcile-1");
        assert_eq!(queue.items[1].position, Some(1));

        // 官方暂停是容器级状态：`autoDrain` 翻转的整对象替换必须一次对齐。
        projection
            .apply(
                1,
                delta_frame(
                    SUB,
                    json!({
                        "revision": 5,
                        "queue": queue_state(
                            false,
                            json!([queue_item("queue_cmd-reconcile-2", "queued two", 0)])
                        )
                    }),
                    1,
                    2,
                ),
            )
            .expect("unpaused queue delta");
        let queue = projection.view().queue;
        assert_eq!(queue.paused, Some(false), "autoDrain=true → 未暂停");
        assert_eq!(queue.items.len(), 1, "已消费项不得残留");
        assert_eq!(queue.items[0].input_id, "queue_cmd-reconcile-2");
        assert_eq!(queue.items[0].position, Some(0), "宿主重排后的位置原样保留");

        // 三态可区分：已知空队列 ≠ 未知。暂停标志缺席（None）也不等于未暂停。
        projection
            .apply(
                1,
                delta_frame(
                    SUB,
                    json!({"revision": 6, "queue": {"items": [], "autoDrain": true}}),
                    2,
                    3,
                ),
            )
            .expect("empty queue delta");
        let queue = projection.view().queue;
        assert!(queue.known, "宿主明确宣告空队列 → 已知");
        assert!(queue.items.is_empty());
        assert_eq!(queue.paused, Some(false));
        // 形状坏（缺 autoDrain）→ 未宣告，不是「空且未暂停」。
        projection
            .apply(
                1,
                delta_frame(SUB, json!({"revision": 7, "queue": {"items": []}}), 3, 4),
            )
            .expect("malformed queue delta");
        let queue = projection.view().queue;
        assert!(!queue.known, "缺 autoDrain 的坏形状必须按未宣告处理");
        assert_eq!(queue.paused, None, "不得把未知暂停猜成未暂停");
    }

    #[test]
    fn queue_pause_reason_is_projected_without_guessing() {
        // 官方 `pauseReason`（`state.queue.pauseReason`，枚举 `stopped|manual|error`）
        // 必须原样透传，且**暂停且队列已空**仍是可下发的暂停事实：
        // 「0 条」不能把容器级暂停吞掉（前端据此保留一行暂停标志）。
        //
        // 断言面全部走 DTO 序列化（手机看到的形状），不依赖实现内部字段名。
        fn queue_of(projection: &Projection) -> Value {
            let view = serde_json::to_value(projection.view()).expect("视图可序列化");
            view.get("queue").cloned().expect("视图必须带 queue")
        }

        let mut projection = Projection::new(target(1));
        projection.activate(SUB);
        let mut snapshot = base_snapshot();
        snapshot["queue"] = json!({"items": [], "autoDrain": false, "pauseReason": "stopped"});
        projection
            .apply(1, snapshot_frame(SUB, snapshot, 0, 1))
            .expect("snapshot with paused empty queue");
        let queue = queue_of(&projection);
        assert_eq!(queue["known"], true, "{queue}");
        assert_eq!(queue["paused"], true, "暂停且空仍是暂停：{queue}");
        assert_eq!(queue["items"].as_array().map(Vec::len), Some(0), "{queue}");
        assert_eq!(
            queue["pauseReason"], "stopped",
            "官方枚举值必须原样透传：{queue}"
        );

        // 增量整对象替换：枚举值同样原样透传（`manual` 只在枚举里，仍是官方值域）。
        projection
            .apply(
                1,
                delta_frame(
                    SUB,
                    json!({
                        "revision": 5,
                        "queue": {"items": [], "autoDrain": false, "pauseReason": "manual"}
                    }),
                    1,
                    2,
                ),
            )
            .expect("delta with manual pause");
        let queue = queue_of(&projection);
        assert_eq!(queue["paused"], true, "{queue}");
        assert_eq!(queue["pauseReason"], "manual", "{queue}");

        // 非枚举值不得猜：暂停事实保留，原因置空（绝不映射成某一种原因）。
        projection
            .apply(
                1,
                delta_frame(
                    SUB,
                    json!({
                        "revision": 6,
                        "queue": {"items": [], "autoDrain": false, "pauseReason": "suspended"}
                    }),
                    2,
                    3,
                ),
            )
            .expect("delta with unknown pause reason");
        let queue = queue_of(&projection);
        assert_eq!(queue["paused"], true, "{queue}");
        assert!(
            queue["pauseReason"].is_null(),
            "非枚举原因必须丢弃（null），不得猜：{queue}"
        );

        // 缺席（宿主只翻 autoDrain）同样不猜，暂停事实照旧。
        projection
            .apply(
                1,
                delta_frame(
                    SUB,
                    json!({"revision": 7, "queue": {"items": [], "autoDrain": false}}),
                    3,
                    4,
                ),
            )
            .expect("delta without pause reason");
        let queue = queue_of(&projection);
        assert_eq!(queue["paused"], true, "{queue}");
        assert!(queue["pauseReason"].is_null(), "{queue}");

        // 恢复 autoDrain：暂停标志消失，且不残留上一帧的原因。
        projection
            .apply(
                1,
                delta_frame(
                    SUB,
                    json!({"revision": 8, "queue": {"items": [], "autoDrain": true}}),
                    4,
                    5,
                ),
            )
            .expect("delta resumed");
        let queue = queue_of(&projection);
        assert_eq!(queue["paused"], false, "{queue}");
        assert!(queue["pauseReason"].is_null(), "{queue}");
    }

    #[test]
    fn input_routing_is_minimal_and_patch_scoped() {
        let mut projection = Projection::new(target(1));
        projection.activate(SUB);
        // 快照带 inputRouting：只投影 mode/reasonCode。
        let mut snapshot = base_snapshot();
        snapshot["inputRouting"] = json!({
            "mode": "choice",
            "reasonCode": "heldQueueDispositionRequired",
            "internal": {"secret": "not-for-phone"},
        });
        projection
            .apply(1, snapshot_frame(SUB, snapshot, 0, 1))
            .expect("snapshot with inputRouting");
        let routing = projection
            .view()
            .input_routing
            .expect("inputRouting 应被投影");
        assert_eq!(routing.mode, "choice");
        assert_eq!(
            routing.reason_code.as_deref(),
            Some("heldQueueDispositionRequired")
        );
        // 手机看到的 wire 形状：camelCase，且只含最小值域（不泄漏 internal 字段）。
        let wire = serde_json::to_value(projection.view()).expect("视图可序列化");
        assert_eq!(wire["inputRouting"]["mode"], "choice");
        assert_eq!(
            wire["inputRouting"]["reasonCode"],
            "heldQueueDispositionRequired"
        );
        assert!(wire["inputRouting"].get("internal").is_none());

        // 增量缺该键：保持旧值（本帧没提 ≠ 已清除）。
        projection
            .apply(1, delta_frame(SUB, json!({"revision": 5}), 1, 2))
            .expect("unrelated delta");
        assert_eq!(
            projection.view().input_routing.expect("保持旧值").mode,
            "choice"
        );

        // 增量带 null/坏形状：置未知（绝不猜 choice）。
        projection
            .apply(
                1,
                delta_frame(SUB, json!({"revision": 6, "inputRouting": null}), 2, 3),
            )
            .expect("delta clearing inputRouting");
        assert!(projection.view().input_routing.is_none());

        // 新快照缺该键：同样置未知（绝不沿用上一份快照）。
        let mut fresh = base_snapshot();
        fresh["inputRouting"] = json!({"mode": "choice"});
        projection
            .apply(1, snapshot_frame(SUB, fresh, 0, 1))
            .expect("snapshot with routing");
        assert!(projection.view().input_routing.is_some());
        projection
            .apply(1, snapshot_frame(SUB, base_snapshot(), 0, 1))
            .expect("snapshot without routing");
        assert!(projection.view().input_routing.is_none());

        // 失效/解除 barrier 一并作废，避免旧路由影响换代后的 choice 判定。
        projection.deactivate();
        assert!(projection.view().input_routing.is_none());
    }

    #[test]
    fn promoted_input_leaves_queue_without_duplicate_message() {
        let mut projection = Projection::new(target(1));
        projection.activate(SUB);
        let mut snapshot = base_snapshot();
        snapshot["queue"] = queue_state(
            false,
            json!([
                queue_item("queue_cmd-promote-1", "promoted text", 0),
                queue_item("queue_cmd-promote-2", "still waiting", 1),
            ]),
        );
        projection
            .apply(1, snapshot_frame(SUB, snapshot, 0, 1))
            .expect("snapshot with queue");
        assert_eq!(projection.view().queue.items.len(), 2);

        // 桌面侧消费首项：它从 items[] 消失（官方没有单独的 consumed 事件），
        // 同时 control 出现前台回合。队列只剩第二项，且不得因此多出重复项。
        projection
            .apply(
                1,
                delta_frame(
                    SUB,
                    json!({
                        "revision": 5,
                        "control": {"activeWorks": [{
                            "kind": "primaryTurn",
                            "foregroundExecutionId": "E-promoted"
                        }]},
                        "queue": queue_state(
                            false,
                            json!([queue_item("queue_cmd-promote-2", "still waiting", 0)])
                        )
                    }),
                    1,
                    2,
                ),
            )
            .expect("promotion delta");
        let view = projection.view();
        let ids: Vec<&str> = view
            .queue
            .items
            .iter()
            .map(|item| item.input_id.as_str())
            .collect();
        assert_eq!(
            ids,
            vec!["queue_cmd-promote-2"],
            "被提升的输入必须离开队列（不得与新消息重复）"
        );
        assert_eq!(
            view.control
                .expect("control observed")
                .active_works
                .first()
                .and_then(|work| work.foreground_execution_id.as_deref()),
            Some("E-promoted")
        );

        // 后续无关增量（不带 queue 键）不得让已消费项复活。
        projection
            .apply(1, delta_frame(SUB, json!({"revision": 6}), 2, 3))
            .expect("unrelated delta");
        assert_eq!(projection.view().queue.items.len(), 1);

        // 代际/订阅更替：旧队列整体作废（未知），绝不跨代际复活。
        projection.deactivate();
        let view = projection.view();
        assert!(!view.queue.known, "重订阅解除 barrier 后队列回到未知");
        assert!(view.queue.items.is_empty());
        assert_eq!(view.queue.paused, None);
    }

    #[test]
    fn malformed_queue_shape_is_unknown_not_empty() {
        let mut projection = Projection::new(target(1));
        projection.activate(SUB);
        let mut snapshot = base_snapshot();
        // `items` 不是数组：整份按未宣告处理，绝不猜成空队列。
        snapshot["queue"] = json!({"items": "nope", "autoDrain": true});
        projection
            .apply(1, snapshot_frame(SUB, snapshot, 0, 1))
            .expect("snapshot still readable");
        assert!(!projection.queue().known);
        assert_eq!(projection.queue().paused, None);
    }

    #[test]
    fn oversized_frame_is_rejected() {
        let mut projection = Projection::with_limit(target(1), 256);
        projection.activate(SUB);
        let mut snapshot = base_snapshot();
        snapshot["blob"] = json!("x".repeat(1024));
        let error = projection
            .apply(1, snapshot_frame(SUB, snapshot, 0, 1))
            .expect_err("oversized logical frame");
        assert!(error.is(codes::PAYLOAD_TOO_LARGE));
        assert!(!projection.view().ready);
        assert_eq!(projection.applied_frames(), 0);
    }
}
