//! 实时问答 waterfall（T-live）：常驻订阅 `$events`，把 `user-questions/request`
//! 变成可作答的待答项，并回执 `$events/result`。
//!
//! ## 两条问答路径的分工（下一轮别接错）
//!
//! 官方 DSH 有两条**互斥**的作答入口（取证见
//! `docs/engineering/evidence/2026-11-23-dsh-live-question-gap/root-cause.md`）：
//!
//! | 场景 | 通道 | 本仓实现 |
//! | --- | --- | --- |
//! | 回合仍在跑、用户当场待答（投影 `state == "open"` / 无投影条目） | `user-questions/request` waterfall + `POST /api/$events/result` | **本模块（live）** |
//! | 回合已结束、投影 `state == "continued"` 的补答 | `userQuestions/answer(agent, callId, {answers})` | [`super::questions`]（continued） |
//!
//! * **什么时候用 live**：手机端要回答「此刻正在等用户」的提问（`ask_user_question`
//!   工具调用尚未返回）。这类调用**不会**进入 `userQuestions` 投影——
//!   `dsh-user-questions/src/index.ts` 的投影折叠只对 timed 工具（schema 声明
//!   `timeout` 参数）建 `active` 条目，普通调用在投影里完全不存在。
//! * **什么时候用 continued**：投影已把题组标成 `continued`（超时后回合继续），
//!   走 `userQuestions/answer`。live 表里不会出现这类题。
//! * 两者不互为兜底：对 `open` 调 `userQuestions/answer` 恒返回 `false`（先例见
//!   `questions.rs` 模块文档），对 `continued` 发 `$events/result` 也没有对应 eventId。
//!
//! ## 协议事实（全部取自已安装官方包 `@deepseek-ai/dsh-api-gateway`，不猜）
//!
//! * **开流**：mux `open` 帧，`endpoint = "$events"`，`payload` 必须恰好是
//!   `{"args":{}}`（`lib/index.js` `openRemoteEvents`：多余键 → `gateway/arguments-invalid`）。
//!   本模块经 [`super::actions::PeerChannel::subscribe`] → [`super::codec::encode_open`]
//!   产生该帧。
//! * **首帧**：`{type:"ready", clientId, host}`。`clientId` 是后续回执的必填字段，
//!   也把回执绑定到「这一代事件流」（`isRemoteEventClientId` 要求非空字符串）。
//! * **事件帧**：
//!   - `{type:"waterfall", event, eventId, agentId, request}` —— **需要答案**；
//!   - `{type:"emit", event, args}` —— 只广播，**不回应**。
//! * **回执**：`POST /api/$events/result`。官方浏览器客户端（`lib/client.node.js`
//!   的 `answer()`）发出的 body 是
//!   `{type:"client-request", rpcId, method:"$events/result", payload:{"args":{clientId,eventId,outcome}}}`
//!   ——**不是** `{"result": …}`：网关 `parseRemoteEventResultPayload` 硬要求
//!   `payload` 恰好一个键 `args`。本模块因此复用 [`super::codec::encode_request`]
//!   （它产生的正是 `payload:{"args":…}`）。
//! * `outcome.kind` 三选一（`parseRemoteEventResult`）：
//!   `"result"`（可带 `value`）/ `"next"`（不处理，交给下一个 answerer）/
//!   `"rejected"`（必须带 `error:{name,message,code?,details?}`）。
//! * **必须复用同一条 PeerChannel**：`dispatchRpc` 校验 `client.peer !== peer` →
//!   `gateway/interactions-forbidden`。回执与开流都经
//!   [`super::actions::PeerChannel::for_instance`]（每实例单例）。
//! * **不回执 = 永久挂起**：`receiveRemoteEventResult` 只有在收到本 client 的结果时
//!   才 `removeRemoteEventDelivery`；只要有 delivery 未移除，事件就留在
//!   `pendingRemoteEvents` 里，提问端一直等。因此**每个收到的 waterfall 都必须回执**：
//!   问答事件回 `result`/`rejected`，其余事件回 `next`（等价于「从未订阅」，安全，
//!   也不会误吞审批）。
//! * **重订阅会补投未决事件**：`openRemoteEvents` 会把 `pendingRemoteEvents` 里尚未
//!   投递给本 client 的条目补发。所以断线重订不需要本地补扫——本模块在断开时清空
//!   待答表，新代际由宿主重新投递、重新入表（旧代际的 eventId 一律拒绝作答）。
//! * **宿主会主动撤单**：某条未决事件被别处结算（另一 answerer 抢答成功 / Agent
//!   Context 释放 / 提问端取消）时，`finishRemoteEvent` 给每个投递过它的 client 推
//!   一帧 `{type:"cancel", eventId}`。收到即把该条移出待答表，**且不回执**——它已不在
//!   `pendingRemoteEvents` 里，回执是无意义写；此后同一 eventId 作答按「不在待答集」
//!   明确拒绝（[`IngestOutcome::Cancelled`]）。
//!
//! ## 会话归属：`agentId` 就是 `sessionId`（已证，不是推测）
//!
//! * `dsh-agent/src/index.ts` 的 `AgentRegistry::enter()`：`agent.id !== agent.session.id`
//!   直接抛错——官方**强制** Agent 身份与会话身份同一命名空间；
//! * `dsh-api-session-controller/src/state/controller.ts` 的 `pendingInput(sessionId)`
//!   自己就拿 `agentId` 与 `sessionId` 比较：`if (String(agentId) !== sessionId) continue`；
//! * waterfall 帧的 `agentId` 来自 `dsh-api-remotes/lib/index.js` 的
//!   `{ value: agent.ctx, subject: agent, agentId: agent.id }`，即同一个 id。
//!
//! 因此本模块按 `(instanceId, sessionId)` **精确**筛选待答项，不退化成实例维度。
//! 帧上缺 `agentId`（网关要求非空，理论上不可达）时该条**不匹配任何会话**——
//! fail-closed，不猜归属。
//!
//! ## 有界与代际
//!
//! * 待答表每实例最多 [`MAX_PENDING_LIVE_QUESTIONS`] 条，超限淘汰**最旧**一条并
//!   **给它回 `next`**（沉默 = 永久挂起，见 [`IngestOutcome`]）；已确认回执的
//!   eventId 另存有界 FIFO（[`MAX_SETTLED_EVENT_IDS`]）以拒绝重复回执；
//! * 每次开流推进**代际**与**流令牌**：流断开（`end_stream`）、实例换代
//!   （`reset_instance`）都清空该实例的待答表与已回执集合——旧代际的提问绝不在
//!   新代际被作答；
//! * **P1-1 同步封存**：实例切换 / 关闭 / 撤权 / 凭据交付都调 `reset_live_questions`
//!   （内部**先停驱动再清状态**，否则旧任务可能把一条 ready 写进新代际）。
//!
//! ## 状态机（Blocker 2：先回执成功，才置 settled）
//!
//! 待答条目三态（[`PendingState`]）：
//!
//! ```text
//!   Ready ──claim──> InFlight ──宿主 ok──> （移出表 + 记 settled）
//!                       │
//!                       ├──NotExecuted──────> Ready（可原样重试）
//!                       ├──DeliveryUnknown─> UnknownDelivery（可重试，不宣称成功）
//!                       ├──宿主 cancel 帧──> （移出表，无回执）
//!                       └──Peer 终态───────> Ready + 挂起订阅（等新凭据）
//! ```
//!
//! 关键不变量：**只有宿主确认回执被接受才移出待答表并记 settled**。认领只置
//! in-flight（条目仍在表内，用户仍看得到「提交中」）；回执发送在**分离任务**里
//! 完成，HTTP 请求被客户端取消也不会让条目卡在 in-flight。
//!
//! ## 安全边界：带 `intent` 的提问永不代答（Blocker 1）
//!
//! `dsh-plan-mode` 的 `exit_plan_mode` 与 `dsh-cua` 的授权询问都会带
//! `intent:{kind:"plan-review"|"approval",…}`（已核官方源码）。这类提问是**动作
//! 确认**而非选项选择——批准即执行计划 / 授予域权限。本模块的两道闸：
//!
//! 1. **入表层**（[`LiveQuestionStore::ingest`]）：带 `intent` 的问答帧**不入表**，
//!    回 `next` 交官方控制台（手机端根本不显示它）；
//! 2. **作答层**（[`LiveQuestionStore::claim`]）：即便表里意外出现这类条目也返回
//!    `DSH_CAPABILITY_DENIED`，**零写宿主**。
//!
//! 判定只认「`intent` 字段存在且非 null」——**任何 kind 都算危险**，包括未知
//! kind（官方明确保留扩展空间）。策略文字见 [`BLOCKER_INTENT_POLICY`]。

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use futures::{future::BoxFuture, FutureExt};

use super::error::{codes, DshError};
use super::model::{NativeResponse, NativeResult, Target};
use super::questions::{self, DshQuestion, DshQuestionAnswerItem};
use super::transport;

/// 事件流端点（网关 `REMOTE_EVENT_STREAM_ENDPOINT`）。
pub const EVENTS_ENDPOINT: &str = "$events";
/// 事件回执端点（网关 `$events/result`；mux 上是 `$events`，一元上是 `$events/result`）。
pub const EVENTS_RESULT_ENDPOINT: &str = "$events/result";
/// 问答事件名（`dsh-api-remotes` 转发清单里的 `waterfall` 条目）。
pub const QUESTION_EVENT: &str = "user-questions/request";
/// 待答表上限（每实例）。
pub const MAX_PENDING_LIVE_QUESTIONS: usize = 32;
/// 已回执 eventId 的记忆上限（每实例；只为拒绝重复回执，不承载业务状态）。
pub const MAX_SETTLED_EVENT_IDS: usize = 256;
/// `$events` 首帧（ready）的等待上限。
///
/// P2-1：宿主接受 `open` 却不下发 ready 时，若无限等待，`live` 会永久停在
/// 「已连接但收不到任何东西」——前端无法与「确实没有提问」区分。超时即按订阅
/// 失败处理（驱动退避重订），`live` 保持 false。
pub const READY_TIMEOUT: Duration = Duration::from_secs(10);

const WATERFALL_TYPE: &str = "waterfall";
const READY_TYPE: &str = "ready";

// ============================================================
// 帧解析（纯函数；单测直接喂构造 JSON）
// ============================================================

/// 一条实时待答的提问（来自 waterfall，而非投影）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveQuestionRequest {
    /// waterfall 的 `eventId`——回执的关联键（回执必须原样带回）。
    pub event_id: String,
    /// `request.wait.callId`（官方 `AskUserQuestionRequestEvent.wait?.callId`）。
    /// 阻塞式提问可以没有 `wait`，故为 `Option`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    /// 题面（复用投影路径的同一 schema；`intent` 只有实时路径会带）。
    pub questions: Vec<DshQuestion>,
    /// 发起 Agent；官方语义 = 会话 ID（见模块文档「会话归属」）。
    ///
    /// 入表时**保证非空**：缺 `agentId` 的帧不入表（见 [`LiveQuestionStore::ingest`]）。
    pub agent_id: String,
    /// 本地接收时刻（毫秒；仅用于展示与排序，不参与协议）。
    pub received_at_ms: i64,
}

/// 一次**原子**的待答读取（GET 路由必须用它，不能分两次读）。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveQuestionSnapshot {
    /// 读取瞬间的流代际（诊断；同一响应内不会混两代）。
    pub generation: u64,
    /// 读取瞬间是否已收到 ready（= 此刻能收到新提问）。
    pub live: bool,
    /// 该会话的待答项（含 in-flight / unknown-delivery 条目）。
    pub pending: Vec<LiveQuestionRequest>,
}

/// 解析后的 `waterfall` 帧。
///
/// `event` / `agentId` 都是 `Option`：官方网关要求非空，但**畸形帧必须能被
/// 分辨出来**（缺 `event` → 回 next；缺 `agentId` → 回 next），而不是在解析层
/// 直接丢弃（丢弃 = 沉默 = 提问端永久挂起）。
#[derive(Debug, Clone, PartialEq)]
pub struct LiveWaterfallFrame {
    pub event: Option<String>,
    pub event_id: String,
    pub agent_id: Option<String>,
    /// `projectRemoteEventRequest` 已剥掉 `agent`/`signal`，余下是纯 JSON 请求。
    pub request: serde_json::Value,
}

/// 解析 ready 帧的 `clientId`（非 ready 或字段非法 → `None`）。
pub fn parse_ready_frame(value: &serde_json::Value) -> Option<String> {
    if value.get("type").and_then(|v| v.as_str()) != Some(READY_TYPE) {
        return None;
    }
    non_empty_str(value.get("clientId")).map(str::to_string)
}

/// 解析 `waterfall` 帧；非 waterfall / **缺 `eventId`** → `None`。
///
/// 缺 `eventId` 时**无法回执**（回执的唯一关联键），这类帧只能丢弃——但这是
/// 唯一允许丢弃的 frame 级缺口；缺 `event`/`agentId` 的帧由 [`LiveWaterfallFrame`]
/// 带出（`None`）并回 `next`。
pub fn parse_waterfall(value: &serde_json::Value) -> Option<LiveWaterfallFrame> {
    if value.get("type").and_then(|v| v.as_str()) != Some(WATERFALL_TYPE) {
        return None;
    }
    Some(LiveWaterfallFrame {
        event: non_empty_str(value.get("event")).map(str::to_string),
        event_id: non_empty_str(value.get("eventId"))?.to_string(),
        agent_id: non_empty_str(value.get("agentId")).map(str::to_string),
        request: value
            .get("request")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
    })
}

/// 兼容别名：等同 [`parse_waterfall`]（旧调用点/测试沿用此名）。
pub fn parse_waterfall_frame(value: &serde_json::Value) -> Option<LiveWaterfallFrame> {
    parse_waterfall(value)
}

/// 把 `user-questions/request` 的 `request` 投影成待答项。
///
/// 结构纪律（与投影路径一致）：`questions` 必须是非空数组，且**每题都能解析出
/// 非空 `id`**；任何一题不可信 → **整条拒绝**（返回 `Err(诊断)`），不渲染半份题面。
///
/// `agent_id` 由调用方传入（已确认非空）——不在这里从帧上重读，避免「解析一份、
/// 归属判另一份」的双真相。
pub fn live_question_request(
    frame: &LiveWaterfallFrame,
    agent_id: &str,
    received_at_ms: i64,
) -> Result<LiveQuestionRequest, String> {
    let Some(items) = frame.request.get("questions").and_then(|v| v.as_array()) else {
        return Err("user-questions/request carries no questions array".to_string());
    };
    if items.is_empty() {
        return Err("user-questions/request carries an empty questions array".to_string());
    }
    let mut questions = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let question = questions::parse_question(item)
            .ok_or_else(|| format!("question at index {index} has no usable id"))?;
        questions.push(question);
    }
    Ok(LiveQuestionRequest {
        event_id: frame.event_id.clone(),
        call_id: frame
            .request
            .pointer("/wait/callId")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string),
        questions,
        agent_id: agent_id.to_string(),
        received_at_ms,
    })
}

/// 解析 `{type:"cancel", eventId}`（宿主结算/取消某条未决事件时推给每个投递过的
/// client，见 `dsh-api-gateway` 的 `finishRemoteEvent`）。
pub fn parse_cancel_frame(value: &serde_json::Value) -> Option<String> {
    if value.get("type").and_then(|v| v.as_str()) != Some("cancel") {
        return None;
    }
    non_empty_str(value.get("eventId")).map(str::to_string)
}

fn non_empty_str(value: Option<&serde_json::Value>) -> Option<&str> {
    value
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

// ============================================================
// 回执 body（与官方 parseRemoteEventResult 逐键对齐）
// ============================================================

/// 回执的 `args`（= 官方客户端的 `result` 对象；envelope 由
/// [`super::codec::encode_request`] 包成 `payload:{"args":…}`）。
pub fn build_result_args(
    client_id: &str,
    event_id: &str,
    outcome: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        "clientId": client_id,
        "eventId": event_id,
        "outcome": outcome,
    })
}

/// 完整的一元请求体（测试据此与官方形状逐键比对）。
pub fn encode_result_request(
    rpc_id: &str,
    client_id: &str,
    event_id: &str,
    outcome: serde_json::Value,
) -> Result<Vec<u8>, DshError> {
    super::codec::encode_request(
        rpc_id,
        EVENTS_RESULT_ENDPOINT,
        &build_result_args(client_id, event_id, outcome),
    )
}

/// `outcome.kind = "result"`：`value` 就是官方 `ask()` 的 `answer` 参数形状
/// （`{answers:[{id,selected,custom?}]}`）。
pub fn result_outcome(native_answers: Vec<serde_json::Value>) -> serde_json::Value {
    serde_json::json!({ "kind": "result", "value": { "answers": native_answers } })
}

/// `outcome.kind = "next"`：不处理，交给下一个 answerer（= 从未订阅的等价语义）。
pub fn next_outcome() -> serde_json::Value {
    serde_json::json!({ "kind": "next" })
}

/// `outcome.kind = "rejected"`：`error` 形状按 `parseRemoteEventRejection`
/// （`name` 非空字符串 + `message` 字符串）。
pub fn rejected_outcome(detail: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": "rejected",
        "error": { "name": "Error", "message": detail },
    })
}

// ============================================================
// 待答表（纯内存状态机；不触网，单测全覆盖）
// ============================================================

/// 一条待答帧的处置结论（驱动循环据此决定回执）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestOutcome {
    /// 与待答无关（ready / emit / 未知形态 / 代际不符 / 重复投递）。
    Ignored,
    /// 问答事件：已入表，**暂不回执**（等用户作答）。
    Question(LiveQuestionRequest),
    /// 必须回 `next`（携带 eventId）：沉默会让提问端**永久挂起**。
    ///
    /// 覆盖：非问答 waterfall（如 `approval/request`）/ 满表被淘汰的最旧一条 /
    /// 缺 `agentId` 的问答帧（入表也永远查不到）/ 带 `intent` 的问答帧（见
    /// [`BLOCKER_INTENT_POLICY`]）/ 缺 `event` 但有 `eventId` 的帧。
    ForwardToNext { event_id: String, event: String },
    /// 问答事件但 `request` 形状不可信：回 `rejected`，让提问端立刻拿到明确错误
    /// 而不是挂死（官方客户端对「监听器抛错」也是回 `rejected`）。
    Reject { event_id: String, detail: String },
    /// 宿主主动撤单：`{type:"cancel", eventId}`——事件已被别处结算（另一 answerer
    /// 抢答成功 / Agent Context 释放 / 提问端取消）。必须**移出待答表**且**不回执**
    /// （该事件已不在 `pendingRemoteEvents`，回执是无效写）。
    Cancelled { event_id: String },
}

/// Blocker 1 的策略声明（实现处注释的单一来源，避免下一轮又放开）。
///
/// **带 `intent` 的问答事件一律不入待答表、回 `next` 交给官方控制台。**
///
/// 理由：`intent` 标记的是**动作确认**而不是选项选择——
/// * `dsh-plan-mode` 的 `exit_plan_mode` 发 `intent:{kind:"plan-review", approve}`
///   （`src/index.ts` 实测），批准即离开 plan mode 并立刻执行计划；
/// * `dsh-cua` 发 `intent:{kind:"approval", approve, reject}`，批准即授予整段
///   会话的域权限。
///
/// 手机端不在动作确认的授权界面上（无计划全文阅读、无权限域说明），代答等于让
/// 用户在没读到后果的情况下授权。因此**只显示、不代答**——本实现选择「只回
/// `next`」而不是「进只读列表」：只读列表会让用户以为「手机看到了却答不了」是
/// 缺陷，而实际上这是刻意的安全边界；`next` 让官方控制台成为唯一 answerer，
/// 语义最干净（等价于我们从未订阅该类事件）。
pub const BLOCKER_INTENT_POLICY: &str =
    "questions carrying an official `intent` are action confirmations (plan-review / \
     approval); the phone never answers them. They are not enqueued and are forwarded with \
     `next` so the official console stays the sole answerer";

/// 一次**提交中**的回执认领。
///
/// **Blocker 2 的核心不变量**：认领**不移出**待答表、**不写** settled——条目转入
/// [`PendingState::InFlight`]，只有宿主确认回执被接受后才移出并记 settled。
#[derive(Debug, Clone)]
pub struct ClaimedLiveQuestion {
    pub request: LiveQuestionRequest,
    /// 回执必填：与开流同一代的 clientId。
    pub client_id: String,
    /// 认领时的代际（回填/结算时用于拒绝跨代）。
    generation: u64,
    /// 认领时的流序号（同代内每次重订也换代——`clientId` 与它一一对应）。
    epoch_token: u64,
}

/// 待答条目的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingState {
    /// 等用户作答。
    Ready,
    /// 已认领，回执在途（**仍在表内**，但不再可被第二个请求认领）。
    InFlight,
    /// 回执已送出但结局未知：不宣称成功，但**允许重试**（宿主端同 eventId
    /// 二次回执是安全 no-op：`receiveRemoteEventResult` 先 `removeRemoteEventDelivery`，
    /// 事件已结算时 `pending === undefined` 直接 return）。
    UnknownDelivery,
}

#[derive(Debug, Clone)]
struct PendingEntry {
    request: LiveQuestionRequest,
    state: PendingState,
}

#[derive(Debug, Default)]
struct InstanceLiveState {
    authorization_epoch: u64,
    suspended: bool,
    /// 事件流代际（每次开流 +1；换代即作废旧表）。
    generation: u64,
    /// 每次 `begin_stream` 递增的流令牌（同代内重订也不可跨用 clientId）。
    epoch_token: u64,
    /// 最近一次 ready 帧的 clientId（无 ready → None，此时不可回执）。
    client_id: Option<String>,
    /// 待答表（含 in-flight 与 unknown-delivery 条目——**只有确认受理才移出**）。
    pending: VecDeque<PendingEntry>,
    /// 已确认受理的 eventId（有界 FIFO；拒绝重复回执）。
    settled: VecDeque<String>,
}

/// 实时待答表（每实例一份状态；线程安全、无 IO）。
#[derive(Debug, Default)]
pub struct LiveQuestionStore {
    instances: Mutex<HashMap<String, InstanceLiveState>>,
}

impl LiveQuestionStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn with_state<T>(&self, instance_id: &str, f: impl FnOnce(&mut InstanceLiveState) -> T) -> T {
        let mut map = self
            .instances
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        f(map.entry(instance_id.to_string()).or_default())
    }

    fn read_state<T>(
        &self,
        instance_id: &str,
        f: impl FnOnce(&InstanceLiveState) -> T,
    ) -> Option<T> {
        let map = self
            .instances
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        map.get(instance_id).map(f)
    }

    #[cfg(test)]
    fn begin_stream(&self, instance_id: &str, client_id: &str) -> u64 {
        self.begin_authorized_stream(
            instance_id,
            client_id,
            self.authorization_epoch(instance_id),
        )
        .expect("test stream must have current authorization")
    }

    fn authorization_epoch(&self, instance_id: &str) -> u64 {
        self.read_state(instance_id, |state| state.authorization_epoch)
            .unwrap_or_default()
    }

    fn begin_authorized_stream(
        &self,
        instance_id: &str,
        client_id: &str,
        epoch: u64,
    ) -> Option<u64> {
        self.with_state(instance_id, |state| {
            if state.authorization_epoch != epoch || state.suspended {
                return None;
            }
            state.generation = state.generation.wrapping_add(1);
            state.epoch_token = state.epoch_token.wrapping_add(1);
            state.client_id = Some(client_id.to_string());
            state.pending.clear();
            Some(state.generation)
        })
    }

    /// 流令牌（与 clientId 一一对应；回执据此拒绝跨流回填）。
    pub fn epoch_token(&self, instance_id: &str) -> Option<u64> {
        self.read_state(instance_id, |state| state.epoch_token)
    }

    /// 流终结（end/断链/取消）：仅当前代际可清空（旧代际的迟到终结不得清掉新表）。
    ///
    /// 清空的同时**推进代际**：终结之后从这条流上再飘来的帧（reader 队列里尚未
    /// 消费完的 item）一律按旧代际丢弃——否则「断开后残留帧」会重新填表，把已经
    /// 失效的提问又变成可作答卡。
    ///
    /// 返回是否真的清空了一代。
    pub fn end_stream(&self, instance_id: &str, generation: u64) -> bool {
        self.with_state(instance_id, |state| {
            if state.generation != generation {
                return false;
            }
            state.generation = state.generation.wrapping_add(1);
            state.client_id = None;
            state.pending.clear();
            log::info!(
                "dsh live questions: $events stream generation {generation} ended; pending table cleared"
            );
            true
        })
    }

    fn invalidate_authorization(state: &mut InstanceLiveState, suspended: bool) {
        state.authorization_epoch = state.authorization_epoch.wrapping_add(1);
        state.generation = state.generation.wrapping_add(1);
        state.epoch_token = state.epoch_token.wrapping_add(1);
        state.client_id = None;
        state.pending.clear();
        state.settled.clear();
        state.suspended = suspended;
    }

    pub fn reset_instance(&self, instance_id: &str) {
        self.with_state(instance_id, |state| {
            Self::invalidate_authorization(state, false)
        });
    }

    fn suspend_driver(&self, instance_id: &str, epoch: u64) {
        self.with_state(instance_id, |state| {
            if state.authorization_epoch == epoch {
                Self::invalidate_authorization(state, true);
            }
        });
    }

    fn suspend_claim(&self, instance_id: &str, claimed: &ClaimedLiveQuestion) -> bool {
        self.with_state(instance_id, |state| {
            if state.generation != claimed.generation || state.epoch_token != claimed.epoch_token {
                return false;
            }
            Self::invalidate_authorization(state, true);
            true
        })
    }

    /// 当前代际（测试与诊断用；未开过流 → None）。
    pub fn generation(&self, instance_id: &str) -> Option<u64> {
        self.read_state(instance_id, |state| state.generation)
    }

    /// 当前代际的 clientId（无 ready → None）。
    pub fn client_id(&self, instance_id: &str) -> Option<String> {
        self.read_state(instance_id, |state| state.client_id.clone())
            .flatten()
    }

    /// 一次**原子**状态读取：待答列表 + live 标记 + 代际。
    ///
    /// **为什么必须一次读**：分两次读（先 `pending` 再 `is_live`）之间可能正好发生
    /// 换代/断流重订，于是响应里会混入「旧代的待答项 + 新代的 live 标记」。GET
    /// 路由必须用本方法，不能分别调用 `pending`/`is_live`。
    pub fn snapshot(&self, instance_id: &str, session_id: &str) -> LiveQuestionSnapshot {
        self.read_state(instance_id, |state| LiveQuestionSnapshot {
            generation: state.generation,
            live: state.client_id.is_some(),
            pending: state
                .pending
                .iter()
                // in-flight / unknown-delivery 也要返回：用户需要看到「正在提交」
                // 与「上一次结局未知、可重试」，而不是凭空消失。
                .filter(|entry| entry.request.agent_id == session_id)
                .map(|entry| entry.request.clone())
                .collect(),
        })
        .unwrap_or_default()
    }

    /// 把一帧入表并给出处置结论。
    ///
    /// **必须回执的三种拒收**（Blocker 3）：满表淘汰最旧一条、带 `intent` 的问答帧、
    /// 缺 `agentId` 的问答帧——三者都回 `next`，绝不让提问端挂死。
    pub fn ingest(
        &self,
        instance_id: &str,
        generation: u64,
        value: &serde_json::Value,
    ) -> IngestOutcome {
        // 宿主主动撤单（`finishRemoteEvent` 对每个投递过的 client 推一帧
        // `{type:"cancel", eventId}`）：事件已被别处结算。移出待答表（**含
        // in-flight 条目**——Blocker 2），且不回执（已不在 pendingRemoteEvents）。
        if let Some(event_id) = parse_cancel_frame(value) {
            return self.with_state(instance_id, |state| {
                if state.generation != generation {
                    return IngestOutcome::Ignored;
                }
                let position = state
                    .pending
                    .iter()
                    .position(|entry| entry.request.event_id == event_id);
                if let Some(position) = position {
                    let entry = state.pending.remove(position).expect("position checked");
                    log::info!(
                        "dsh live questions: host cancelled event {event_id} (was {:?}; settled elsewhere)",
                        entry.state
                    );
                    IngestOutcome::Cancelled { event_id }
                } else {
                    IngestOutcome::Ignored
                }
            });
        }
        let Some(frame) = parse_waterfall(value) else {
            // emit / ready / 无 eventId 的畸形帧：无需回执，也不进表。
            return IngestOutcome::Ignored;
        };
        // 有 eventId 但缺 event：**不能静默忽略**（Blocker 3）——回 next 让宿主
        // 继续走下一个 answerer，而不是把这条卡在 pending 里等到提问端超时。
        let Some(event) = frame.event.clone() else {
            log::warn!(
                "dsh live questions: waterfall frame {} carries no event name; forwarding with next",
                frame.event_id
            );
            return IngestOutcome::ForwardToNext {
                event_id: frame.event_id,
                event: "<missing>".to_string(),
            };
        };
        if event != QUESTION_EVENT {
            return IngestOutcome::ForwardToNext {
                event_id: frame.event_id,
                event,
            };
        }
        // 缺 agentId：入表也永远查不到（按会话筛选必空）→ 不回执就是永久挂起。
        // 回 next 交给官方控制台（Blocker 3）。
        let Some(agent_id) = frame.agent_id.clone() else {
            log::warn!(
                "dsh live questions: question event {} carries no agentId; forwarding with next",
                frame.event_id
            );
            return IngestOutcome::ForwardToNext {
                event_id: frame.event_id,
                event,
            };
        };
        let request =
            match live_question_request(&frame, &agent_id, chrono::Utc::now().timestamp_millis()) {
                Ok(request) => request,
                Err(detail) => {
                    return IngestOutcome::Reject {
                        event_id: frame.event_id,
                        detail,
                    }
                }
            };
        // Blocker 1：带 intent 的动作确认类提问**不入表**、回 next 交官方控制台。
        // 见 [`BLOCKER_INTENT_POLICY`] 与 [`super::questions::intent_blocked_questions`]。
        if let Some(blocked) = questions::intent_blocked_questions(&request.questions) {
            log::info!(
                "dsh live questions: event {} carries an intent (question {}); not enqueueing, \
                 forwarding with next so the official console answers it",
                request.event_id,
                blocked.id
            );
            return IngestOutcome::ForwardToNext {
                event_id: frame.event_id,
                event,
            };
        }
        self.with_state(instance_id, |state| {
            if state.generation != generation {
                // 旧代际迟到帧：不入表（绝不让旧提问在新代际被作答）。
                // 也不回执：这段流已经结束，宿主的投递已随 client 移除。
                return IngestOutcome::Ignored;
            }
            if state.settled.iter().any(|id| id == &request.event_id) {
                // 已确认受理的 eventId 又被投递（重订补投）：不再出卡。
                return IngestOutcome::Ignored;
            }
            if state
                .pending
                .iter()
                .any(|entry| entry.request.event_id == request.event_id)
            {
                // 重复投递同一条：保持首次的顺序、时刻与状态。
                return IngestOutcome::Ignored;
            }
            if state.pending.len() >= MAX_PENDING_LIVE_QUESTIONS {
                // Blocker 3：淘汰必须回执。用 next 把这条还给宿主的下一个
                // answerer，而不是让它跟着我们的表一起消失。
                let droppable = state
                    .pending
                    .iter()
                    .position(|entry| entry.state == PendingState::Ready);
                let declined = droppable.and_then(|position| state.pending.remove(position));
                let event_id = match declined {
                    Some(dropped) => {
                        state.pending.push_back(PendingEntry {
                            request,
                            state: PendingState::Ready,
                        });
                        dropped.request.event_id
                    }
                    None => request.event_id,
                };
                return IngestOutcome::ForwardToNext {
                    event_id,
                    event: QUESTION_EVENT.to_string(),
                };
            }
            state.pending.push_back(PendingEntry {
                request: request.clone(),
                state: PendingState::Ready,
            });
            IngestOutcome::Question(request)
        })
    }

    /// 当前待答列表（**按会话精确筛选**：`agentId == sessionId`）。
    ///
    /// 帧上缺 `agentId` 的条目不会入表（见 [`Self::ingest`]）——fail-closed，不猜归属。
    pub fn pending(&self, instance_id: &str, session_id: &str) -> Vec<LiveQuestionRequest> {
        self.snapshot(instance_id, session_id).pending
    }

    /// 条目的当前状态（诊断/测试；不在表 → None）。
    #[doc(hidden)]
    pub fn entry_state(&self, instance_id: &str, event_id: &str) -> Option<PendingState> {
        self.read_state(instance_id, |state| {
            state
                .pending
                .iter()
                .find(|entry| entry.request.event_id == event_id)
                .map(|entry| entry.state)
        })
        .flatten()
    }

    /// **仅测试**：绕过 ingest 的 intent/归属过滤，把一条待答项硬塞进表。
    ///
    /// 用途 = 验证「第二道闸」——即便表里意外出现一条带 `intent` 的项，
    /// [`Self::claim`] 仍必须拒绝作答（纵深防御）。
    #[doc(hidden)]
    pub fn force_enqueue_for_test(&self, instance_id: &str, request: LiveQuestionRequest) {
        let event_id = request.event_id.clone();
        self.with_state(instance_id, |state| {
            state.pending.retain(|e| e.request.event_id != event_id);
            state.pending.push_back(PendingEntry {
                request,
                state: PendingState::Ready,
            });
        });
    }

    /// 认领一次回执：把条目置为 [`PendingState::InFlight`]（**不移出、不写 settled**）。
    ///
    /// 拒绝（都不写宿主、不改状态）：
    /// * 该 eventId **已确认受理** → `ACTION_STATE_CONFLICT`（不重复写宿主）；
    /// * 已在 in-flight → `ACTION_STATE_CONFLICT`（并发提交被拒，等第一次收敛）；
    /// * 不在待答表（已答/已取消/超时/上一代流）→ `ACTION_STATE_CONFLICT`；
    /// * 归属不符（`agentId != sessionId`）→ `SESSION_OWNERSHIP_UNPROVEN`；
    /// * 该题带 `intent` → `CAPABILITY_DENIED`（**Blocker 1 的第二道闸**：即便
    ///   表里意外有这条，也不许写宿主）；
    /// * 尚无 ready（没有 clientId）→ `CONNECTION_CLOSED`。
    pub fn claim(
        &self,
        instance_id: &str,
        session_id: &str,
        event_id: &str,
    ) -> Result<ClaimedLiveQuestion, DshError> {
        self.with_state(instance_id, |state| {
            if state.settled.iter().any(|id| id == event_id) {
                return Err(DshError::new(
                    codes::ACTION_STATE_CONFLICT,
                    format!(
                        "live question {event_id} was already answered; refusing to write the host twice"
                    ),
                ));
            }
            let Some(client_id) = state.client_id.clone() else {
                return Err(DshError::new(
                    codes::CONNECTION_CLOSED,
                    "live question stream has no ready clientId; cannot carry a result",
                ));
            };
            let epoch_token = state.epoch_token;
            let Some(position) = state
                .pending
                .iter()
                .position(|entry| entry.request.event_id == event_id)
            else {
                return Err(DshError::new(
                    codes::ACTION_STATE_CONFLICT,
                    format!(
                        "live question {event_id} is not pending (answered, cancelled, timed out, \
                         or belongs to a previous event-stream generation)"
                    ),
                ));
            };
            let entry = &state.pending[position];
            if entry.request.agent_id != session_id {
                return Err(DshError::new(
                    codes::SESSION_OWNERSHIP_UNPROVEN,
                    format!("live question {event_id} does not belong to session {session_id}"),
                ));
            }
            if entry.state == PendingState::InFlight {
                return Err(DshError::new(
                    codes::ACTION_STATE_CONFLICT,
                    format!("live question {event_id} already has a receipt in flight"),
                ));
            }
            // Blocker 1 第二道闸：intent 类提问永不写宿主（表里本不该有，防御性拒绝）。
            if let Some(blocked) = questions::intent_blocked_questions(&entry.request.questions) {
                return Err(DshError::new(
                    codes::CAPABILITY_DENIED,
                    format!(
                        "live question {event_id} is an action confirmation (intent on question {}); \
                         answering it from the phone is refused — use the official console",
                        blocked.id
                    ),
                ));
            }
            let request = entry.request.clone();
            state.pending[position].state = PendingState::InFlight;
            Ok(ClaimedLiveQuestion {
                request,
                client_id,
                generation: state.generation,
                epoch_token,
            })
        })
    }

    /// 宿主确认受理：移出待答表并记 settled（**只有这一步写 settled**）。
    pub fn settle(&self, instance_id: &str, claimed: &ClaimedLiveQuestion) -> bool {
        let event_id = claimed.request.event_id.as_str();
        self.with_state(instance_id, |state| {
            if state.generation != claimed.generation || state.epoch_token != claimed.epoch_token {
                // 换代/换流后的迟到结算：不写新代表。
                return false;
            }
            let position = state
                .pending
                .iter()
                .position(|entry| entry.request.event_id == event_id);
            match position {
                Some(position) => {
                    state.pending.remove(position);
                }
                None => return false,
            }
            state.settled.push_back(event_id.to_string());
            while state.settled.len() > MAX_SETTLED_EVENT_IDS {
                state.settled.pop_front();
            }
            true
        })
    }

    /// 确定未写出 / 送达未知时把条目放回可答（清除 in-flight）。
    ///
    /// * `UnknownDelivery` → 状态记 [`PendingState::UnknownDelivery`]（不宣称成功，
    ///   但允许重试：宿主端同 eventId 二次回执是安全 no-op）；
    /// * 其它（`NotExecuted` / 预提交被拒 / 校验失败）→ 回 [`PendingState::Ready`]。
    ///
    /// **P1-3：代次必须先比对再做任何状态修改**——旧代际的迟到回填绝不能动新表。
    /// 返回是否回填成功。
    pub fn release(
        &self,
        instance_id: &str,
        claimed: &ClaimedLiveQuestion,
        state: PendingState,
    ) -> bool {
        let event_id = claimed.request.event_id.as_str();
        self.with_state(instance_id, |live| {
            // 代次先行：不匹配直接返回，**不做任何修改**（settled 也不碰）。
            if live.generation != claimed.generation || live.epoch_token != claimed.epoch_token {
                return false;
            }
            let position = live
                .pending
                .iter()
                .position(|entry| entry.request.event_id == event_id);
            match position {
                Some(position) => {
                    live.pending[position].state = state;
                    true
                }
                None => false,
            }
        })
    }

    /// 把 in-flight 条目当作已结算（宿主 cancel 帧到达时调用；仅诊断用返回值）。
    ///
    /// cancel 帧本身已在 [`Self::ingest`] 里移出条目 —— 本方法只用于「cancel 到达
    /// 时条目还在 in-flight、需要中断等待」的可观测断言，不改变表（已移出）。
    #[doc(hidden)]
    pub fn forget_in_flight(&self, instance_id: &str, event_id: &str) -> bool {
        self.with_state(instance_id, |state| {
            let before = state.pending.len();
            state
                .pending
                .retain(|entry| entry.request.event_id != event_id);
            state.pending.len() != before
        })
    }
}

// ============================================================
// 注入面：事件流 / 回执发送器（生产 = PeerChannel；测试 = 假流/假发送器）
// ============================================================

/// 一条事件流句柄（`$events` 的逻辑流）。`next()` 返回 `None` = 流终结。
pub trait LiveEventStream: Send {
    fn next<'a>(&'a mut self) -> BoxFuture<'a, Option<serde_json::Value>>;
}

/// 事件流工厂（参数 = 实例 ID）。生产 = 该实例的 PeerChannel 上开 `$events`。
pub type LiveEventStreamFactory = Arc<
    dyn Fn(String) -> BoxFuture<'static, Result<Box<dyn LiveEventStream>, DshError>> + Send + Sync,
>;

/// 回执结局（相位敏感）。
#[derive(Debug)]
pub enum LiveReceiptOutcome {
    /// 宿主受理了这次回执 RPC（`{ok:true}`）——**不等于答案已生效**（见
    /// [`LiveQuestionAnswerOutcome`]）。
    Accepted,
    /// 宿主业务拒绝（`{ok:false,error}`）：已确定写出，不回填。
    HostRejected { code: String, detail: String },
    /// 确定未写出（发送前失败/宿主闸门拒绝）：条目回到可答。
    NotExecuted { error: DshError },
    /// 送达未知：条目记 unknown-delivery（允许重试，不宣称成功）。
    DeliveryUnknown { error: DshError },
    /// **Peer 终态**（401/409）：这条连接不会自己恢复，必须等新凭据交付。
    /// 驱动必须停止重试（P1-2），否则就是空转。
    PeerTerminal { error: DshError },
}

/// 回执发送器。
pub trait LiveResultSender: Send + Sync {
    /// 发出 `$events/result`。`target` = 已核验的会话目标（自动 `next` 时可不带）。
    ///
    /// 返回 `Err` 只用于**确定未写出**的本地失败（无当代凭据/建连失败等）；
    /// 已经交出字节的结局一律用 [`LiveReceiptOutcome`] 表达。
    fn post_result<'a>(
        &'a self,
        instance_id: &'a str,
        rpc_id: &'a str,
        args: &'a serde_json::Value,
        target: Option<(&'a Target, &'a super::production::SendAcceptance)>,
    ) -> BoxFuture<'a, Result<LiveReceiptOutcome, DshError>>;
}

/// 生产事件流：共享长生命周期 PeerChannel 上的 `$events` 逻辑流。
struct PeerLiveEventStream(transport::StreamHandle);

impl LiveEventStream for PeerLiveEventStream {
    fn next<'a>(&'a mut self) -> BoxFuture<'a, Option<serde_json::Value>> {
        Box::pin(async move {
            match self.0.next().await {
                Some(Ok(event)) => Some(event.value),
                Some(Err(error)) => {
                    // 流级错误：连接已不可用，按流终结处理（驱动循环会重订）。
                    log::warn!(
                        "dsh live questions: $events stream error: {}: {}",
                        error.code,
                        error.detail
                    );
                    None
                }
                None => None,
            }
        })
    }
}

/// 生产回执发送器（与开流共用 [`super::actions::PeerChannel::for_instance`] 单例）。
pub struct PeerLiveResultSender;

impl LiveResultSender for PeerLiveResultSender {
    fn post_result<'a>(
        &'a self,
        instance_id: &'a str,
        rpc_id: &'a str,
        args: &'a serde_json::Value,
        target: Option<(&'a Target, &'a super::production::SendAcceptance)>,
    ) -> BoxFuture<'a, Result<LiveReceiptOutcome, DshError>> {
        Box::pin(async move {
            let channel = super::actions::PeerChannel::for_instance(instance_id.to_string());
            let response = match channel
                .request_classified_accepted(rpc_id, EVENTS_RESULT_ENDPOINT, args, target)
                .await
            {
                Ok(response) => response,
                // 传输/预提交层的确定失败：区分「Peer 终态」与「可重试的本地失败」。
                Err(error) => {
                    return Ok(if is_peer_terminal(error.code) {
                        LiveReceiptOutcome::PeerTerminal { error }
                    } else {
                        LiveReceiptOutcome::NotExecuted { error }
                    })
                }
            };
            Ok(match response {
                Ok(NativeResponse {
                    result: NativeResult::Success { .. },
                    ..
                }) => LiveReceiptOutcome::Accepted,
                Ok(NativeResponse {
                    result: NativeResult::Failure { error },
                    ..
                }) => {
                    // 宿主 typed 失败：`PEER_UNAUTHORIZED` 等也是终态（cookie 已死）。
                    if is_peer_terminal(&error.code) {
                        LiveReceiptOutcome::PeerTerminal {
                            error: DshError::new(
                                codes::PEER_UNAUTHORIZED,
                                format!(
                                    "host rejected the connection during result delivery ({})",
                                    error.code
                                ),
                            ),
                        }
                    } else {
                        LiveReceiptOutcome::HostRejected {
                            code: error.code,
                            detail: error.message.unwrap_or_default(),
                        }
                    }
                }
                Err(transport::UnaryFailure::NotExecuted(error)) => {
                    if is_peer_terminal(error.code) {
                        LiveReceiptOutcome::PeerTerminal { error }
                    } else {
                        LiveReceiptOutcome::NotExecuted { error }
                    }
                }
                Err(transport::UnaryFailure::DeliveryUnknown(error)) => {
                    LiveReceiptOutcome::DeliveryUnknown { error }
                }
            })
        })
    }
}

/// 该错误码是否表示「这条 Peer 已终态，重试无意义，必须等新凭据交付」。
///
/// 依据 [`super::transport`] 的终态语义：`401`（cookie 随 socket 生死绑定）与
/// `409`（一 Peer 一 socket）都是**不自动重连的终态**。撤销后必须等授权域交付
/// 新代际凭据——原地空转重试只会刷日志（P1-2）。
pub fn is_peer_terminal(code: &str) -> bool {
    matches!(
        code,
        codes::PEER_UNAUTHORIZED | codes::PEER_CONFLICT | codes::PEER_REVOKED
    )
}

// ============================================================
// 服务：订阅保活 + 待答查询 + 回执
// ============================================================

#[derive(Clone)]
struct LiveWiring {
    stream_factory: LiveEventStreamFactory,
    result_sender: Arc<dyn LiveResultSender>,
}

/// 实时问答服务（hub 持有的单例）。
///
/// **未装配**（[`LiveQuestionService::new`]）时 [`LiveQuestionService::ensure_subscribed`]
/// 与 [`LiveQuestionService::answer_admitted`] 返回 `DSH_UNSUPPORTED`，绝不冒充已支持。
pub struct LiveQuestionService {
    store: Arc<LiveQuestionStore>,
    wiring: RwLock<Option<LiveWiring>>,
    /// 每实例的常驻驱动任务（断开重订在任务内自循环；换代时 abort）。
    drivers: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
    /// **仅测试**：把重订退避压到毫秒级（生产恒 [`transport::recovery_backoff`]）。
    #[cfg(test)]
    backoff_override: Mutex<Option<Duration>>,
}

impl Default for LiveQuestionService {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveQuestionService {
    /// 未装配实例（测试基线 / disabled hub）：表可查但不可订阅。
    pub fn new() -> Self {
        Self {
            store: Arc::new(LiveQuestionStore::new()),
            wiring: RwLock::new(None),
            drivers: Mutex::new(HashMap::new()),
            #[cfg(test)]
            backoff_override: Mutex::new(None),
        }
    }

    /// 生产装配：`$events` 订阅 + `$events/result` 回执都经每实例 PeerChannel 单例，
    /// 满足网关「回执方必须与开流方同一 Peer」的校验。
    pub fn production() -> Self {
        let service = Self::new();
        service.install_wiring(
            Arc::new(|instance_id: String| {
                Box::pin(async move {
                    let channel = super::actions::PeerChannel::for_instance(instance_id);
                    // payload 必须恰好是 {"args":{}}（网关 openRemoteEvents 硬校验）：
                    // codec::encode_open 把本次传入的 {} 包成 payload:{"args":{}}。
                    let handle = channel
                        .subscribe(EVENTS_ENDPOINT, &serde_json::json!({}))
                        .await?;
                    Ok(Box::new(PeerLiveEventStream(handle)) as Box<dyn LiveEventStream>)
                })
            }),
            Arc::new(PeerLiveResultSender),
        );
        service
    }

    /// 注入事件流工厂与回执发送器（生产装配 / 测试注入）。
    pub fn install_wiring(
        &self,
        stream_factory: LiveEventStreamFactory,
        result_sender: Arc<dyn LiveResultSender>,
    ) {
        *self
            .wiring
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(LiveWiring {
            stream_factory,
            result_sender,
        });
    }

    /// 待答表（只读；不需要装配——未装配时恒空）。
    pub fn pending(&self, instance_id: &str, session_id: &str) -> Vec<LiveQuestionRequest> {
        self.store.pending(instance_id, session_id)
    }

    /// **原子**读取：`live` 与 `pending` 来自同一次状态快照（P2-1）。
    ///
    /// GET 路由必须用它——分别读会在换代窗口里混出「旧代待答 + 新代 live」。
    pub fn snapshot(&self, instance_id: &str, session_id: &str) -> LiveQuestionSnapshot {
        self.store.snapshot(instance_id, session_id)
    }

    /// 订阅健康（`true` = 已收到 ready，即此刻能收到新提问）。
    ///
    /// 消费方据此区分「确证没有实时待答」（`live: true` + 空表）与「实时通道还没
    /// 起来，所以看不到待答」（`live: false`）——两者绝不能折叠成一个空数组。
    pub fn is_live(&self, instance_id: &str) -> bool {
        self.store.client_id(instance_id).is_some()
    }

    pub fn is_suspended(&self, instance_id: &str) -> bool {
        self.store
            .read_state(instance_id, |state| state.suspended)
            .unwrap_or(false)
    }

    /// 诊断/测试用：底层表。
    #[doc(hidden)]
    pub fn store(&self) -> &Arc<LiveQuestionStore> {
        &self.store
    }

    /// 确保该实例的 `$events` 流在位（**幂等**）。
    ///
    /// * 驱动任务已在跑（含正在退避重订）→ 直接返回；
    /// * 任务已结束（理论上不发生，循环自恢）或从未开过 → 起一个常驻任务；
    /// * 未装配 → `DSH_UNSUPPORTED`。
    ///
    /// **常驻语义**：任务随服务存活，流断开后按 [`transport::recovery_backoff`]
    /// 自动重订；宿主会把未决事件补投给新 clientId（见模块文档）。
    pub fn ensure_subscribed(&self, instance_id: &str) -> Result<(), DshError> {
        if self.wiring_snapshot().is_none() {
            return Err(DshError::new(
                codes::UNSUPPORTED,
                "dsh live question subscription is not wired",
            ));
        }
        let mut drivers = self
            .drivers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let epoch = self.store.authorization_epoch(instance_id);
        if self.is_suspended(instance_id) {
            return Err(DshError::new(
                codes::PEER_UNAUTHORIZED,
                "live question subscription awaits new credentials",
            ));
        }
        if let Some(handle) = drivers.get(instance_id) {
            if !handle.is_finished() {
                return Ok(());
            }
        }
        let service = Arc::new(self.shallow_clone());
        let instance = instance_id.to_string();
        let handle = tokio::spawn(run_driver(service, instance, epoch));
        drivers.insert(instance_id.to_string(), handle);
        Ok(())
    }

    /// 封存授权，并使旧驱动的迟到 ready 和回执失效。
    pub fn reset_instance(&self, instance_id: &str) {
        let mut drivers = self
            .drivers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.store.reset_instance(instance_id);
        if let Some(handle) = drivers.remove(instance_id) {
            handle.abort();
        }
    }

    /// 回执一条实时待答（用户已作答）。
    ///
    /// **Blocker 2 的状态机**（顺序即语义）：
    /// 1. `claim` → 条目置 **in-flight**（**仍在表内**，未被认领第二次）；
    /// 2. 题目级校验失败 → 立即复原为 ready（**零写宿主**），返回错误；
    /// 3. 回执**在分离任务里发出**并把结果归类回表（见下）；HTTP 请求被客户端
    ///    取消也不会让条目卡在 in-flight——任务已 spawn，状态照常收敛；
    /// 4. 归一分流：
    ///    * 宿主 `{ok:true}` → `settle`：移出待答表 + 记 settled（**只有这里**）；
    ///    * `NotExecuted`（零字节出门可证）→ 复原 ready，用户可重试；
    ///    * `DeliveryUnknown` → 记 unknown-delivery（**不宣称成功**，允许重试：
    ///      宿主端同 eventId 二次回执是安全 no-op）；
    ///    * `HostRejected` / `PeerTerminal` → 复原 ready 并返回明确错误；
    ///      终态码还会停掉这条订阅（P1-2：等新凭据交付，不空转）。
    ///
    /// **返回值语义（P1-4）**：`receipt_accepted == true` 只表示「宿主接受了这次
    /// 回执 RPC」——它**不是**「本次答案已生效」（后者只有投影/消息流能证明，
    /// 且 live 路径本来就不写账本）。
    #[cfg(test)]
    async fn answer(
        self: &Arc<Self>,
        instance_id: &str,
        session_id: &str,
        target: &Target,
        event_id: &str,
        answers: &[DshQuestionAnswerItem],
    ) -> Result<LiveQuestionAnswerOutcome, DshError> {
        self.answer_admitted(instance_id, session_id, target, event_id, answers, None)
            .await
    }

    pub(crate) async fn answer_admitted(
        self: &Arc<Self>,
        instance_id: &str,
        session_id: &str,
        target: &Target,
        event_id: &str,
        answers: &[DshQuestionAnswerItem],
        acceptance: Option<super::production::SendAcceptance>,
    ) -> Result<LiveQuestionAnswerOutcome, DshError> {
        let wiring = self.wiring_snapshot().ok_or_else(|| {
            DshError::new(
                codes::UNSUPPORTED,
                "dsh live question answering is not wired",
            )
        })?;
        let claimed = match &acceptance {
            Some(acceptance) => {
                super::production::with_send_acceptance(instance_id, acceptance, || {
                    self.store.claim(instance_id, session_id, event_id)
                })??
            }
            None => self.store.claim(instance_id, session_id, event_id)?,
        };
        if let Err(error) =
            questions::validate_answers_for_questions(&claimed.request.questions, answers)
        {
            // 校验失败 = 我们零写宿主：条目回到可答。
            self.store
                .release(instance_id, &claimed, PendingState::Ready);
            return Err(error);
        }
        let native = questions::native_answers_for(&claimed.request.questions, answers);
        let args = build_result_args(
            &claimed.client_id,
            &claimed.request.event_id,
            result_outcome(native),
        );
        let rpc_id = format!(
            "dsh-live-question-{}",
            chrono::Utc::now().timestamp_millis()
        );

        // 请求断开时仍由同一任务完成发送和结算。
        let instance = instance_id.to_string();
        let target = target.clone();
        let service = Arc::clone(self);
        tokio::spawn(async move {
            let result_target = acceptance.as_ref().map(|accepted| (&target, accepted));
            let send = wiring
                .result_sender
                .post_result(&instance, &rpc_id, &args, result_target);
            let outcome = match std::panic::AssertUnwindSafe(send).catch_unwind().await {
                Ok(outcome) => outcome,
                Err(_) => Ok(LiveReceiptOutcome::DeliveryUnknown {
                    error: DshError::new(codes::STATE_IO, "live question receipt task panicked"),
                }),
            };
            service.apply_receipt_outcome(&instance, &claimed, outcome)
        })
        .await
        .map_err(|error| {
            DshError::new(
                codes::STATE_IO,
                format!("live question receipt task failed: {error}"),
            )
        })?
    }

    /// 把一次回执结局写回待答表并产出对外结果（`answer` 与自动回执共用）。
    fn apply_receipt_outcome(
        &self,
        instance_id: &str,
        claimed: &ClaimedLiveQuestion,
        outcome: Result<LiveReceiptOutcome, DshError>,
    ) -> Result<LiveQuestionAnswerOutcome, DshError> {
        match outcome {
            Ok(LiveReceiptOutcome::Accepted) => {
                // 只有宿主确认受理才移出待答表 + 记 settled。
                self.store.settle(instance_id, claimed);
                Ok(LiveQuestionAnswerOutcome {
                    receipt_accepted: true,
                })
            }
            Ok(LiveReceiptOutcome::HostRejected { code, detail }) => {
                self.store
                    .release(instance_id, claimed, PendingState::Ready);
                Err(DshError::new(
                    codes::STREAM_ERROR,
                    format!("host rejected the live question answer ({code}: {detail})"),
                ))
            }
            Ok(LiveReceiptOutcome::NotExecuted { error }) => {
                // 零字节出门可证：复原 ready，用户可重试。
                self.store
                    .release(instance_id, claimed, PendingState::Ready);
                Err(error)
            }
            Ok(LiveReceiptOutcome::DeliveryUnknown { error }) => {
                // 不宣称成功、不自动重投；状态记 unknown-delivery 且允许重试。
                self.store
                    .release(instance_id, claimed, PendingState::UnknownDelivery);
                Err(DshError::new(
                    codes::WRITE_TIMEOUT,
                    format!(
                        "live question answer delivery is unknown ({}); the answer may or may not \
                         have reached the host — retrying the same event is safe",
                        error.code
                    ),
                ))
            }
            Ok(LiveReceiptOutcome::PeerTerminal { error }) => {
                // P1-2：Peer 终态（401/409/撤销）→ 不空转重试，等新凭据交付。
                self.suspend_claim(instance_id, claimed);
                Err(error)
            }
            Err(error) => {
                // 预提交被拒（无当代凭据/开关/换代）：确定未写出 → 复原 ready。
                if is_peer_terminal(error.code) {
                    self.suspend_claim(instance_id, claimed);
                } else {
                    self.store
                        .release(instance_id, claimed, PendingState::Ready);
                }
                Err(error)
            }
        }
    }

    fn suspend_claim(&self, instance_id: &str, claimed: &ClaimedLiveQuestion) {
        let mut drivers = self
            .drivers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.store.suspend_claim(instance_id, claimed) {
            if let Some(handle) = drivers.remove(instance_id) {
                handle.abort();
            }
        }
    }

    /// 供常驻任务使用的最小共享副本（store 与 wiring 共享，drivers 不带来竞争）。
    fn shallow_clone(&self) -> Self {
        Self {
            store: Arc::clone(&self.store),
            wiring: RwLock::new(self.wiring_snapshot()),
            drivers: Mutex::new(HashMap::new()),
            #[cfg(test)]
            backoff_override: Mutex::new(
                *self
                    .backoff_override
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()),
            ),
        }
    }

    fn wiring_snapshot(&self) -> Option<LiveWiring> {
        self.wiring
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn backoff_for(&self, attempt: u32) -> Duration {
        #[cfg(test)]
        {
            if let Some(overridden) = *self
                .backoff_override
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
            {
                return overridden;
            }
        }
        transport::recovery_backoff(attempt)
    }
}

/// 回执结果（P1-4：语义精确化——**不是**「答案已生效」）。
///
/// 路由层映射为 `{"receiptAccepted": true, "receiptOnly": true}`（并保留
/// `hostAccepted` 作为同值别名，兼容既有前端读取点）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveQuestionAnswerOutcome {
    /// 宿主**接受了这次回执 RPC**（`$events/result` 返回 `{ok:true}`）。
    ///
    /// **它只证明回执被送达并被网关接收**——不证明用户的答案已经作用到会话上
    /// （答案生效要等宿主把 waterfall 结算后推进回合，那只能从投影/消息流观测）。
    /// 前端不得把它显示成「已答」，最多显示「已提交」。
    pub receipt_accepted: bool,
}

impl LiveQuestionAnswerOutcome {
    /// 「已受理」的语义备注（供路由与文档引用，避免下一轮又误读成已生效）。
    pub const SEMANTICS: &'static str = "receipt accepted by the host gateway; the answer's \
         effect is not observable on this path — read the session projection or message stream \
         to confirm it was applied";
}

/// 实时回执请求体（`POST /m/api/v1/dsh/questions/live/answer`）。
///
/// 与投影路径的 [`super::questions::DshQuestionAnswerRequest`] **刻意不同**：
/// 这里没有 `actionId` / `revisionToken`（waterfall 无账本语义，幂等靠 `eventId`），
/// 多一个 `eventId`（回执关联键）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DshLiveQuestionAnswerRequest {
    pub instance_id: String,
    pub session_id: String,
    /// waterfall 帧的 `eventId`（与开流 `clientId` 配对）。
    ///
    /// **P1-3 残余风险（已评估，本轮不做 token 化）**：裸 `eventId` 不绑定代际，
    /// 一个停留在旧页面的客户端可能拿着上一代的 eventId 来提交。防线是
    /// [`LiveQuestionStore::claim`] 只认**当前**表内的条目（换代即清表），因此旧
    /// eventId 会得到 `ACTION_STATE_CONFLICT` 而不是误答新题。残余风险仅限于：
    /// 若同一 eventId 在**同一代**内被重复投递（理论上宿主不会复用 eventId，它
    /// 用 `randomUUID` 生成），则旧页面可以答到较新的同一事件——但那条事件本来就
    /// 是同一个提问，答案语义相同。**结论：不引入 answerToken**（会把 GET/POST
    /// 契约复杂化，收益不抵风险）。
    pub event_id: String,
    pub answers: Vec<DshQuestionAnswerItem>,
}

// ============================================================
// 常驻驱动循环
// ============================================================

/// 常驻：开流 → 等 ready → 逐帧入表/回执 → 流终结则按退避重订（永不主动退出）。
async fn run_driver(service: Arc<LiveQuestionService>, instance_id: String, epoch: u64) {
    let mut attempt = 0u32;
    loop {
        if service.store.authorization_epoch(&instance_id) != epoch {
            return;
        }
        match run_authorized_stream(&service, &instance_id, epoch).await {
            Ok(()) => {
                // 已 ready 过的流正常终结（**逻辑断开**：end 帧 / socket 断）——这是
                // 可原地重订的情形，宿主会把未决事件补投给新 clientId。退避从基值
                // 重新开始，避免「一次偶发断链就退到 10s 上限」。
                attempt = 0;
            }
            Err(error) => {
                // **P1-2 物理失效**：Peer 终态（401/409/撤销）不会自己恢复——必须等
                // 授权域交付新代际凭据。原地重试只会无限刷 401，所以直接退出驱动；
                // `reset_instance` 已清空待答表，`ensure_subscribed` 会被
                // `is_suspended` 挡住，直到 `reset_instance`（凭据交付路径）解除。
                if is_peer_terminal(error.code) {
                    log::warn!(
                        "dsh live questions: $events subscription for {instance_id} hit a peer \
                         terminal state ({}): {}; stopping the driver until new credentials arrive",
                        error.code,
                        error.detail
                    );
                    service.store.suspend_driver(&instance_id, epoch);
                    return;
                }
                attempt = attempt.saturating_add(1);
                log::warn!(
                    "dsh live questions: $events subscription for {instance_id} failed ({}): {}",
                    error.code,
                    error.detail
                );
            }
        }
        tokio::time::sleep(service.backoff_for(attempt)).await;
    }
}

/// 跑**一条**流的完整生命周期（便于单测；驱动循环负责重订）。
///
/// 返回 `Ok(())` = 流被正常终结；`Err` = 开流失败或在 ready 之前终结。
#[cfg(test)]
async fn run_stream_once(
    service: &Arc<LiveQuestionService>,
    instance_id: &str,
) -> Result<(), DshError> {
    run_authorized_stream(
        service,
        instance_id,
        service.store.authorization_epoch(instance_id),
    )
    .await
}

async fn run_authorized_stream(
    service: &Arc<LiveQuestionService>,
    instance_id: &str,
    epoch: u64,
) -> Result<(), DshError> {
    let wiring = service.wiring_snapshot().ok_or_else(|| {
        DshError::new(
            codes::UNSUPPORTED,
            "dsh live question subscription is not wired",
        )
    })?;
    let mut stream = (wiring.stream_factory)(instance_id.to_string()).await?;
    // 官方契约：`open $events` 之后**首个 item 必须是 ready**
    // （`openRemoteEvents` 先 yield `{...REMOTE_EVENT_STREAM_READY, clientId, host}`）。
    //
    // P2-1：ready 有超时——宿主接受 open 但不下发首帧时，绝不能永久停在这里把
    // `live` 当成已连接（那样前端会一直显示「实时通道正常」却永远收不到提问）。
    let first = tokio::time::timeout(READY_TIMEOUT, stream.next())
        .await
        .map_err(|_| {
            DshError::new(
                codes::READ_TIMEOUT,
                format!(
                    "$events stream sent no ready frame within {}ms; treating the subscription \
                     as failed",
                    READY_TIMEOUT.as_millis()
                ),
            )
        })?
        .ok_or_else(|| {
            DshError::new(
                codes::STREAM_ERROR,
                "$events stream ended before any frame (expected a ready frame first)",
            )
        })?;
    let client_id = parse_ready_frame(&first).ok_or_else(|| {
        DshError::new(
            codes::STREAM_ERROR,
            format!(
                "$events first frame is not the expected ready frame (type={:?}); \
                 protocol mismatch, refusing to guess",
                first
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("<none>")
            ),
        )
    })?;
    let Some(generation) = service
        .store
        .begin_authorized_stream(instance_id, &client_id, epoch)
    else {
        return Ok(());
    };
    while let Some(value) = stream.next().await {
        let receipt = emit_receipt_for_ingest(
            &service.store,
            &wiring,
            instance_id,
            generation,
            &client_id,
            &value,
        )
        .await;
        // Blocker 3：自动回执（next/rejected）无法确认送达 → 结束这条订阅，
        // 让重订把未决事件重新补投一遍（宿主会给我们新 clientId）。
        if let ReceiptDisposition::Unconfirmed(detail) = receipt {
            service.store.end_stream(instance_id, generation);
            return Err(DshError::new(codes::STREAM_ERROR, detail));
        }
    }
    service.store.end_stream(instance_id, generation);
    Ok(())
}

/// 自动回执的发送结论（决定驱动循环是否结束这条订阅）。
#[derive(Debug, PartialEq, Eq)]
enum ReceiptDisposition {
    /// 无需回执，或宿主已受理。
    Settled,
    /// 无法确认送达（重试后仍 NotExecuted / 送达未知 / 宿主拒收 / Peer 终态）。
    /// 携带诊断；驱动据此结束订阅，由重订补投恢复。
    Unconfirmed(String),
}

/// 自动回执的有界重试次数（只对「确定未写出」重试——已交出的字节重投会重复
/// 结算语义，虽然宿主幂等，但没有必要刷请求）。
const AUTO_RECEIPT_RETRIES: usize = 2;

/// 入表并按需发出「非问答 waterfall → next」「畸形问答 → rejected」「满表淘汰
/// → next」「缺 agentId/event → next」四类回执。
///
/// 问答事件本身**故意不回执**：回执就是用户答案，由
/// [`LiveQuestionService::answer_admitted`] 发。
async fn emit_receipt_for_ingest(
    store: &Arc<LiveQuestionStore>,
    wiring: &LiveWiring,
    instance_id: &str,
    generation: u64,
    client_id: &str,
    value: &serde_json::Value,
) -> ReceiptDisposition {
    if store.generation(instance_id) != Some(generation) {
        return ReceiptDisposition::Settled;
    }
    let (event_id, outcome) = match store.ingest(instance_id, generation, value) {
        IngestOutcome::Ignored | IngestOutcome::Question(_) | IngestOutcome::Cancelled { .. } => {
            return ReceiptDisposition::Settled
        }
        // 沉默 = 提问端永久挂起。`next` = 「我不处理」，交给下一个 answerer
        // （等价于从未订阅；也不会误吞审批/动作确认）。
        IngestOutcome::ForwardToNext { event_id, event } => {
            log::debug!("dsh live questions: forwarding {event} event {event_id} with next");
            (event_id, next_outcome())
        }
        IngestOutcome::Reject { event_id, detail } => {
            log::warn!("dsh live questions: rejecting malformed event {event_id}: {detail}");
            (event_id, rejected_outcome(&detail))
        }
    };
    let mut last_detail = String::new();
    for attempt in 0..=AUTO_RECEIPT_RETRIES {
        let args = build_result_args(client_id, &event_id, outcome.clone());
        let rpc_id = format!(
            "dsh-live-question-event-{}-{attempt}",
            chrono::Utc::now().timestamp_millis()
        );
        // **不传 target**：自动回执（next/rejected）不承载用户动作，因此不参与
        // 「发送目标代际复核/开关提交门」——那是给用户意图的写出口准备的。这里
        // 只需要与开流同一个 PeerChannel（网关的 same-peer 校验），而通道本身
        // 由 `PeerChannel::for_instance` 绑定在实例上。副作用：宿主若已换代，
        // 回执会得到 401/409 → 归为 PeerTerminal → 订阅挂起（P1-2）。
        let result = wiring
            .result_sender
            .post_result(instance_id, &rpc_id, &args, None)
            .await;
        match result {
            Ok(LiveReceiptOutcome::Accepted) => return ReceiptDisposition::Settled,
            Ok(LiveReceiptOutcome::HostRejected { code, detail }) => {
                // 宿主业务拒绝：换 next 重试一次是无意义的（同一条事件同样被拒），
                // 直接判「无法确认」并交给重订。
                last_detail =
                    format!("host rejected the receipt for event {event_id}: {code}: {detail}");
                break;
            }
            Ok(LiveReceiptOutcome::NotExecuted { error }) => {
                // 确定未写出 → 有界重试（连接可能正在恢复）。
                last_detail = format!(
                    "receipt for event {event_id} was not executed: {}: {}",
                    error.code, error.detail
                );
                if attempt < AUTO_RECEIPT_RETRIES {
                    tokio::time::sleep(transport::recovery_backoff(attempt as u32)).await;
                    continue;
                }
            }
            Ok(LiveReceiptOutcome::DeliveryUnknown { error }) => {
                last_detail = format!(
                    "receipt for event {event_id} has unknown delivery: {}: {}",
                    error.code, error.detail
                );
                break;
            }
            Ok(LiveReceiptOutcome::PeerTerminal { error }) => {
                last_detail = format!(
                    "peer became terminal while acknowledging event {event_id}: {}: {}",
                    error.code, error.detail
                );
                break;
            }
            Err(error) => {
                last_detail = format!(
                    "receipt for event {event_id} failed: {}: {}",
                    error.code, error.detail
                );
                break;
            }
        }
    }
    // 宿主可能对 `rejected` 不满意（某些事件只接受 result/next）：退化为 `next`
    // 再试一次——「不处理」对任何 waterfall 都是合法结局。
    if !matches!(outcome, serde_json::Value::Null)
        && outcome.get("kind").and_then(|k| k.as_str()) == Some("rejected")
    {
        let args = build_result_args(client_id, &event_id, next_outcome());
        let rpc_id = format!(
            "dsh-live-question-event-{}-fallback",
            chrono::Utc::now().timestamp_millis()
        );
        if matches!(
            wiring
                .result_sender
                .post_result(instance_id, &rpc_id, &args, None)
                .await,
            Ok(LiveReceiptOutcome::Accepted)
        ) {
            return ReceiptDisposition::Settled;
        }
    }
    log::warn!("dsh live questions: {last_detail}");
    ReceiptDisposition::Unconfirmed(last_detail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const INSTANCE: &str = "instance-1";
    const SESSION: &str = "session-abc";

    fn waterfall(
        event: &str,
        event_id: &str,
        agent: &str,
        request: serde_json::Value,
    ) -> serde_json::Value {
        json!({
            "type": "waterfall",
            "event": event,
            "eventId": event_id,
            "agentId": agent,
            "request": request,
        })
    }

    fn question_frame(event_id: &str, agent: &str) -> serde_json::Value {
        waterfall(
            QUESTION_EVENT,
            event_id,
            agent,
            json!({
                "questions": [{
                    "id": "q1",
                    "question": "which one?",
                    "detail": "extra context for the picker",
                    "header": "Pick",
                    "multiSelect": true,
                    "options": [
                        { "label": "A", "description": "first" },
                        { "label": "B" }
                    ]
                }, {
                    "id": "q2",
                    "question": "second?",
                    "options": [{"label": "yes"}]
                }],
                "wait": { "callId": "call-9", "timed": true }
            }),
        )
    }

    /// 建好一条已 ready 的流代际，返回其 generation。
    fn ready_store() -> LiveQuestionStore {
        let store = LiveQuestionStore::new();
        store.begin_stream(INSTANCE, "client-1");
        store
    }

    fn generation_of(store: &LiveQuestionStore) -> u64 {
        store.generation(INSTANCE).expect("stream started")
    }

    fn answer(id: &str, selected: &[&str], custom: Option<&str>) -> DshQuestionAnswerItem {
        DshQuestionAnswerItem {
            id: id.to_string(),
            selected: selected.iter().map(|s| s.to_string()).collect(),
            custom: custom.map(str::to_string),
        }
    }

    /// ① waterfall 帧解析：options/description/multiSelect/detail/header/callId 全部保全。
    #[test]
    fn waterfall_frame_is_projected_into_live_request() {
        let store = ready_store();
        let generation = generation_of(&store);
        let outcome = store.ingest(INSTANCE, generation, &question_frame("evt-1", SESSION));
        let IngestOutcome::Question(request) = outcome else {
            panic!("expected a live question, got {outcome:?}");
        };
        assert_eq!(request.event_id, "evt-1");
        assert_eq!(request.call_id.as_deref(), Some("call-9"));
        assert_eq!(request.agent_id, SESSION);
        assert_eq!(request.questions.len(), 2);
        let first = &request.questions[0];
        assert_eq!(first.id, "q1");
        assert_eq!(first.question, "which one?");
        assert_eq!(
            first.detail.as_deref(),
            Some("extra context for the picker")
        );
        assert_eq!(first.header.as_deref(), Some("Pick"));
        assert_eq!(first.multi_select, Some(true));
        let options = first.options.as_ref().expect("options preserved");
        assert_eq!(options[0].label, "A");
        assert_eq!(options[0].description.as_deref(), Some("first"));
        assert_eq!(options[1].label, "B");
        assert_eq!(options[1].description, None);
        assert_eq!(first.multi_select, Some(true), "multiSelect must survive");
        assert_eq!(
            store.pending(INSTANCE, SESSION),
            vec![request],
            "同一份载荷必须能从待答表按会话读回"
        );
    }

    /// ② 畸形 request（无 questions / 空 questions / 题面缺 id）→ 不产待答项、不 panic。
    #[test]
    fn malformed_question_request_yields_no_pending_entry() {
        let store = ready_store();
        let generation = generation_of(&store);
        let cases = [
            // 完全没有 questions 字段
            waterfall(QUESTION_EVENT, "e-none", SESSION, json!({})),
            // questions 不是数组
            waterfall(
                QUESTION_EVENT,
                "e-not-array",
                SESSION,
                json!({"questions": "nope"}),
            ),
            // 空 questions
            waterfall(QUESTION_EVENT, "e-empty", SESSION, json!({"questions": []})),
            // 题干缺 id（整条不可信 → 整条拒绝）
            waterfall(
                QUESTION_EVENT,
                "e-no-id",
                SESSION,
                json!({"questions": [{"question": "no id here"}]}),
            ),
            // 混入一道坏题：整条拒绝，不出半份题面
            waterfall(
                QUESTION_EVENT,
                "e-mixed",
                SESSION,
                json!({"questions": [{"id": "ok", "question": "fine"}, {"question": "bad"}]}),
            ),
        ];
        for case in cases {
            let expected = case["eventId"]
                .as_str()
                .expect("eventId in fixture")
                .to_string();
            let outcome = store.ingest(INSTANCE, generation, &case);
            assert!(
                matches!(outcome, IngestOutcome::Reject { ref event_id, .. } if event_id == &expected),
                "malformed request must be rejected, got {outcome:?}"
            );
        }
        assert!(store.pending(INSTANCE, SESSION).is_empty());
        // 畸形帧本身（非 JSON 对象/缺 eventId）也不能 panic。
        for junk in [
            json!("not-an-object"),
            json!({"type": "waterfall", "event": QUESTION_EVENT}),
            json!({"type": "emit", "event": QUESTION_EVENT, "args": []}),
        ] {
            assert_eq!(
                store.ingest(INSTANCE, generation, &junk),
                IngestOutcome::Ignored
            );
        }
        // Blocker 3：有 eventId 但缺 event → 回 next（不许静默忽略，否则提问端挂死）。
        assert_eq!(
            store.ingest(
                INSTANCE,
                generation,
                &json!({"type": "waterfall", "eventId": "e-x"})
            ),
            IngestOutcome::ForwardToNext {
                event_id: "e-x".into(),
                event: "<missing>".into(),
            }
        );
        assert!(store.pending(INSTANCE, SESSION).is_empty());
    }

    /// ③ 同一 eventId 二次回执被拒，且发送器只被调用一次。
    #[tokio::test]
    async fn second_receipt_for_same_event_is_refused_without_second_write() {
        let service = wired_service();
        service.store().begin_stream(INSTANCE, "client-1");
        let generation = generation_of(service.store());
        let _ = service
            .store()
            .ingest(INSTANCE, generation, &question_frame("evt-dup", SESSION));
        let target = target();
        let first = service
            .answer(
                INSTANCE,
                SESSION,
                &target,
                "evt-dup",
                &[answer("q1", &["A"], None), answer("q2", &["yes"], None)],
            )
            .await
            .expect("first answer accepted");
        assert!(first.receipt_accepted);
        let second = service
            .answer(
                INSTANCE,
                SESSION,
                &target,
                "evt-dup",
                &[answer("q1", &["B"], None), answer("q2", &["yes"], None)],
            )
            .await
            .expect_err("duplicate receipt must be refused");
        assert_eq!(second.code, codes::ACTION_STATE_CONFLICT);
        assert!(
            second.detail.contains("already answered"),
            "rejection must say why: {}",
            second.detail
        );
        assert_eq!(
            service.send_count(),
            1,
            "host must be written exactly once for one eventId"
        );
    }

    /// ④ 待答表有界：超限时**淘汰最旧并把淘汰项以 `next` 回给宿主**（Blocker 3）。
    #[test]
    fn pending_table_is_bounded_and_evicts_the_oldest_with_next() {
        let store = ready_store();
        let generation = generation_of(&store);
        let total = MAX_PENDING_LIVE_QUESTIONS + 5;
        let mut evicted = Vec::new();
        for index in 0..total {
            let event_id = format!("evt-{index}");
            let outcome = store.ingest(INSTANCE, generation, &question_frame(&event_id, SESSION));
            match outcome {
                IngestOutcome::Question(_) => {}
                IngestOutcome::ForwardToNext { event_id, .. } => evicted.push(event_id),
                other => panic!("unexpected ingest outcome {other:?}"),
            }
        }
        assert_eq!(
            evicted,
            vec!["evt-0", "evt-1", "evt-2", "evt-3", "evt-4"],
            "淘汰必须回 next 且按最旧优先——沉默会让这几条提问永久挂起"
        );
        let pending = store.pending(INSTANCE, SESSION);
        assert_eq!(pending.len(), MAX_PENDING_LIVE_QUESTIONS);
        assert_eq!(
            pending.first().map(|q| q.event_id.as_str()),
            Some("evt-5"),
            "oldest entries must be dropped first"
        );
        assert_eq!(
            pending.last().map(|q| q.event_id.as_str()),
            Some(format!("evt-{}", total - 1).as_str())
        );
    }

    /// ⑤ 换代 / 断开 → 待答表清空；旧代际迟到帧回不来。
    #[test]
    fn generation_end_and_reset_clear_the_pending_table() {
        let store = ready_store();
        let generation = generation_of(&store);
        store.ingest(INSTANCE, generation, &question_frame("evt-1", SESSION));
        assert_eq!(store.pending(INSTANCE, SESSION).len(), 1);

        // 断开：清空
        assert!(store.end_stream(INSTANCE, generation));
        assert!(store.pending(INSTANCE, SESSION).is_empty());
        assert!(store.client_id(INSTANCE).is_none());

        // 旧代际迟到帧不入新表
        assert_eq!(
            store.ingest(INSTANCE, generation, &question_frame("evt-late", SESSION)),
            IngestOutcome::Ignored
        );
        assert!(store.pending(INSTANCE, SESSION).is_empty());

        // 重订 → 新代际可收；实例换代（reset）再清空
        let next_generation = store.begin_stream(INSTANCE, "client-2");
        assert_ne!(next_generation, generation);
        store.ingest(INSTANCE, next_generation, &question_frame("evt-2", SESSION));
        assert_eq!(store.pending(INSTANCE, SESSION).len(), 1);
        store.reset_instance(INSTANCE);
        assert!(store.pending(INSTANCE, SESSION).is_empty());
        // reset 之后旧代际的 end_stream 不得再影响新状态
        assert!(!store.end_stream(INSTANCE, next_generation));
        assert_eq!(
            store.ingest(INSTANCE, next_generation, &question_frame("evt-3", SESSION)),
            IngestOutcome::Ignored
        );
    }
    /// ⑥ 回执 body 与官方 `parseRemoteEventResult` 期望逐键一致。
    #[test]
    fn result_envelope_matches_the_official_wire_shape() {
        let answers = vec![json!({"id": "q1", "selected": ["A"]})];
        let value = questions::native_answers_for(
            &[DshQuestion {
                id: "q1".into(),
                question: "pick".into(),
                detail: None,
                header: None,
                options: None,
                multi_select: None,
                intent: None,
            }],
            &[answer("q1", &["A"], None)],
        );
        assert_eq!(value, answers, "答案投影必须与官方 answer 参数同形");

        let bytes = encode_result_request("rpc-1", "client-1", "evt-1", result_outcome(value))
            .expect("envelope encodes");
        let envelope: serde_json::Value = serde_json::from_slice(&bytes).expect("valid JSON");
        assert_eq!(
            envelope,
            json!({
                "type": "client-request",
                "rpcId": "rpc-1",
                "method": "$events/result",
                // 官方网关 parseRemoteEventResultPayload 硬要求 payload 只有一个 args 键
                "payload": {
                    "args": {
                        "clientId": "client-1",
                        "eventId": "evt-1",
                        "outcome": {
                            "kind": "result",
                            "value": { "answers": [{"id": "q1", "selected": ["A"]}] }
                        }
                    }
                }
            })
        );
        // 另外两种 outcome 也必须满足 parseRemoteEventResult 的 exactKeys 约束。
        assert_eq!(next_outcome(), json!({"kind": "next"}));
        assert_eq!(
            rejected_outcome("bad request"),
            json!({
                "kind": "rejected",
                "error": { "name": "Error", "message": "bad request" }
            })
        );
        let next_args = build_result_args("client-1", "evt-2", next_outcome());
        assert_eq!(next_args.as_object().map(|o| o.len()), Some(3));
    }

    /// ⑦ 非问答 waterfall（如 `approval/request`）不进表，且结论要求回 `next`。
    #[test]
    fn non_question_waterfall_is_forwarded_with_next_and_never_enters_the_table() {
        let store = ready_store();
        let generation = generation_of(&store);
        let approval = waterfall(
            "approval/request",
            "approval-1",
            SESSION,
            json!({"tool": "bash", "command": "ls"}),
        );
        assert_eq!(
            store.ingest(INSTANCE, generation, &approval),
            IngestOutcome::ForwardToNext {
                event_id: "approval-1".into(),
                event: "approval/request".into(),
            }
        );
        assert!(
            store.pending(INSTANCE, SESSION).is_empty(),
            "审批事件绝不能变成可作答的问答卡"
        );
        // 回执形状：与问答同为 clientId/eventId 配对，但 outcome 是 next。
        assert_eq!(
            build_result_args("client-1", "approval-1", next_outcome()),
            json!({
                "clientId": "client-1",
                "eventId": "approval-1",
                "outcome": { "kind": "next" }
            })
        );
    }

    /// emit 帧只广播：不回执、不入表。
    #[test]
    fn emit_frames_are_ignored() {
        let store = ready_store();
        let generation = generation_of(&store);
        let emit = json!({
            "type": "emit",
            "event": "api-session/status",
            "args": [{"sessionId": SESSION}],
        });
        assert_eq!(
            store.ingest(INSTANCE, generation, &emit),
            IngestOutcome::Ignored
        );
        assert!(store.pending(INSTANCE, SESSION).is_empty());
    }

    /// 待答按会话精确筛选（agentId == sessionId），不串会话。
    #[test]
    fn pending_is_scoped_to_the_exact_session() {
        let store = ready_store();
        let generation = generation_of(&store);
        store.ingest(INSTANCE, generation, &question_frame("evt-a", "session-a"));
        store.ingest(INSTANCE, generation, &question_frame("evt-b", "session-b"));
        let a = store.pending(INSTANCE, "session-a");
        let b = store.pending(INSTANCE, "session-b");
        assert_eq!(a.len(), 1);
        assert_eq!(b.len(), 1);
        assert_eq!(a[0].event_id, "evt-a");
        assert_eq!(b[0].event_id, "evt-b");
        assert!(store.pending(INSTANCE, "session-c").is_empty());
        // 跨会话作答被拒（且不会把题从原会话删掉）。
        let error = store
            .claim(INSTANCE, "session-b", "evt-a")
            .expect_err("cross-session claim must be refused");
        assert_eq!(error.code, codes::SESSION_OWNERSHIP_UNPROVEN);
        assert_eq!(store.pending(INSTANCE, "session-a").len(), 1);
    }

    /// 宿主撤单帧（`{type:"cancel",eventId}`）→ 移出待答表且**不回执**；
    /// 撤单后同一 eventId 不能再作答（宿主已结算，回执是无效写）。
    #[tokio::test]
    async fn host_cancel_removes_the_pending_entry_without_a_receipt() {
        let (service, sender) = wired_service_with_frames(vec![
            json!({"type": "ready", "clientId": "client-1", "host": {"home": "/h"}}),
            question_frame("evt-gone", SESSION),
            json!({"type": "cancel", "eventId": "evt-gone"}),
        ]);
        run_stream_once(&service, INSTANCE)
            .await
            .expect("stream ran to completion");
        assert!(
            sender.posted().is_empty(),
            "撤单无需回执（事件已不在宿主的 pendingRemoteEvents 里）"
        );
        assert!(
            service.store().pending(INSTANCE, SESSION).is_empty(),
            "撤单后必须移出待答表"
        );

        // 流仍在位时的作答尝试：撤单后同一 eventId 明确拒绝（不写宿主）。
        let store = ready_store();
        let generation = generation_of(&store);
        store.ingest(INSTANCE, generation, &question_frame("evt-gone-2", SESSION));
        assert_eq!(
            store.ingest(
                INSTANCE,
                generation,
                &json!({"type": "cancel", "eventId": "evt-gone-2"})
            ),
            IngestOutcome::Cancelled {
                event_id: "evt-gone-2".into()
            }
        );
        let error = store
            .claim(INSTANCE, SESSION, "evt-gone-2")
            .expect_err("cancelled event cannot be claimed");
        assert_eq!(error.code, codes::ACTION_STATE_CONFLICT);
        assert!(store.pending(INSTANCE, SESSION).is_empty());
    }

    /// 同一 eventId 重复投递只入表一次（重订补投幂等）。
    #[test]
    fn duplicate_delivery_of_one_event_is_idempotent() {
        let store = ready_store();
        let generation = generation_of(&store);
        store.ingest(INSTANCE, generation, &question_frame("evt-1", SESSION));
        assert_eq!(
            store.ingest(INSTANCE, generation, &question_frame("evt-1", SESSION)),
            IngestOutcome::Ignored
        );
        assert_eq!(store.pending(INSTANCE, SESSION).len(), 1);
    }

    /// 确定未写出（NotExecuted）→ 回填待答，允许重试；等价一次写。
    #[tokio::test]
    async fn not_executed_receipt_restores_the_pending_entry() {
        let service = wired_service();
        service.store().begin_stream(INSTANCE, "client-1");
        let generation = generation_of(service.store());
        service
            .store()
            .ingest(INSTANCE, generation, &question_frame("evt-retry", SESSION));
        service.fail_next_with_not_executed();
        let target = target();
        let answers = [answer("q1", &["A"], None), answer("q2", &["yes"], None)];
        let error = service
            .answer(INSTANCE, SESSION, &target, "evt-retry", &answers)
            .await
            .expect_err("not-executed must surface an error");
        assert_eq!(error.code, codes::CONNECTION_CLOSED);
        assert_eq!(
            service.store().pending(INSTANCE, SESSION).len(),
            1,
            "确定未写出必须回填，否则用户永远答不了这条提问"
        );
        // 回填后可正常作答（同一 eventId 只成功写一次）。
        service
            .answer(INSTANCE, SESSION, &target, "evt-retry", &answers)
            .await
            .expect("retry after definite non-delivery succeeds");
        assert_eq!(service.send_count(), 2);
        assert!(service.store().pending(INSTANCE, SESSION).is_empty());
    }

    // ============================================================
    // Blocker 1：intent（动作确认类）必须被保留并拒绝作答
    // ============================================================

    /// 构造一条带官方 `intent` 的问答帧（`dsh-plan-mode` 的 exit_plan_mode 形态）。
    fn plan_review_frame(event_id: &str, agent: &str) -> serde_json::Value {
        waterfall(
            QUESTION_EVENT,
            event_id,
            agent,
            json!({
                "questions": [{
                    "id": "plan-review",
                    "header": "Plan review",
                    "question": "Approve this plan and leave plan mode?",
                    "detail": "# the plan markdown",
                    "options": [
                        { "label": "Approve" },
                        { "label": "Keep planning" }
                    ],
                    "intent": {
                        "kind": "plan-review",
                        "approve": "Approve",
                        "callId": "call-plan-1"
                    }
                }],
                "wait": { "callId": "call-plan-1" }
            }),
        )
    }

    /// **Blocker 1 主回归**：带 intent 的问答帧不入可答表、回 next（交官方控制台），
    /// 且即便有人硬塞进来作答也被拒、**宿主零写入**。
    #[tokio::test]
    async fn intent_questions_are_never_enqueued_and_never_answered() {
        // ① 解析层：intent 必须被保留（不能被吞成 None）。
        let frame = plan_review_frame("evt-plan", SESSION);
        let parsed = parse_waterfall(&frame).expect("frame parses");
        let request =
            live_question_request(&parsed, SESSION, 1_700_000_000_000).expect("request parses");
        let intent = request.questions[0]
            .intent
            .as_ref()
            .expect("intent must be preserved, not dropped");
        assert_eq!(intent["kind"], "plan-review");
        assert_eq!(intent["approve"], "Approve");
        assert_eq!(intent["callId"], "call-plan-1");

        // ② 入表层：不入表 + 回 next。
        let store = ready_store();
        let generation = generation_of(&store);
        assert_eq!(
            store.ingest(INSTANCE, generation, &frame),
            IngestOutcome::ForwardToNext {
                event_id: "evt-plan".into(),
                event: QUESTION_EVENT.into(),
            },
            "intent 类提问必须回 next 交给官方控制台，而不是变成可答卡"
        );
        assert!(
            store.pending(INSTANCE, SESSION).is_empty(),
            "手机端绝不能显示动作确认类提问为可作答"
        );

        // ③ 作答层（第二道闸）：手工塞进表也必须被拒、且不写宿主。
        let service = wired_service();
        service.store().begin_stream(INSTANCE, "client-1");
        // 绕过 ingest 的 intent 过滤，直接构造一条带 intent 的待答项。
        let smuggled = LiveQuestionRequest {
            event_id: "evt-smuggled".into(),
            call_id: Some("call-plan-1".into()),
            questions: request.questions.clone(),
            agent_id: SESSION.into(),
            received_at_ms: 0,
        };
        service.store().force_enqueue_for_test(INSTANCE, smuggled);
        let error = service
            .answer(
                INSTANCE,
                SESSION,
                &target(),
                "evt-smuggled",
                &[answer("plan-review", &["Approve"], None)],
            )
            .await
            .expect_err("action confirmations must be refused");
        assert_eq!(
            error.code,
            codes::CAPABILITY_DENIED,
            "intent 类拒答必须是能力门拒绝"
        );
        assert!(
            error.detail.contains("action confirmation"),
            "拒绝必须说清是动作确认类：{}",
            error.detail
        );
        assert_eq!(
            service.send_count(),
            0,
            "宿主必须零写入（绝不代用户批准计划）"
        );
        assert_eq!(
            service.store().entry_state(INSTANCE, "evt-smuggled"),
            Some(PendingState::Ready),
            "被拒后条目保持可答状态未变（没有偷偷移出）"
        );
    }

    /// 未知 intent kind 同样按危险处理（官方保留扩展空间，不得放行）。
    #[test]
    fn unknown_intent_kind_is_treated_as_dangerous() {
        let store = ready_store();
        let generation = generation_of(&store);
        let frame = waterfall(
            QUESTION_EVENT,
            "evt-future",
            SESSION,
            json!({
                "questions": [{
                    "id": "q1",
                    "question": "future action?",
                    "options": [{"label": "ok"}],
                    "intent": { "kind": "some-future-kind", "approve": "ok" }
                }]
            }),
        );
        assert_eq!(
            store.ingest(INSTANCE, generation, &frame),
            IngestOutcome::ForwardToNext {
                event_id: "evt-future".into(),
                event: QUESTION_EVENT.into(),
            }
        );
        assert!(store.pending(INSTANCE, SESSION).is_empty());
    }

    /// 投影路径不带 intent（`.strict()` 且无该键）→ `intent` 必须解析为 None。
    #[test]
    fn projection_questions_carry_no_intent() {
        let groups = questions::groups_from_projection(&json!({
            "values": { "userQuestions": { "active": [{
                "callId": "call-1",
                "state": "continued",
                "questions": [{
                    "id": "q1", "question": "pick", "options": [{"label": "A"}]
                }]
            }] } }
        }));
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].questions[0].intent, None,
            "continued 投影本来就不保留 intent（官方 schema 不含该键）"
        );
    }

    // ============================================================
    // Blocker 2：先回执成功，才置 settled
    // ============================================================

    /// **Blocker 2 主回归**：认领期间条目仍在表内且为 in-flight；宿主受理后才移出
    /// 并记 settled。送达未知 → unknown-delivery，允许重试。
    #[tokio::test]
    async fn settlement_happens_only_after_the_host_accepts_the_receipt() {
        let service = wired_service();
        service.store().begin_stream(INSTANCE, "client-1");
        let generation = generation_of(service.store());
        service
            .store()
            .ingest(INSTANCE, generation, &question_frame("evt-ok", SESSION));
        let target = target();
        let answers = [answer("q1", &["A"], None), answer("q2", &["yes"], None)];

        // ① 认领后：仍在表内，状态 in-flight（**没有**被移出，**没有**写 settled）。
        let claimed = service
            .store()
            .claim(INSTANCE, SESSION, "evt-ok")
            .expect("first claim succeeds");
        assert_eq!(
            service.store().entry_state(INSTANCE, "evt-ok"),
            Some(PendingState::InFlight),
            "认领只置 in-flight，不得移出待答表"
        );
        assert_eq!(
            service.store().pending(INSTANCE, SESSION).len(),
            1,
            "提交中也要让用户看到这条（否则会以为它凭空消失）"
        );
        // 第二次认领被拒（并发提交收敛到一次）。
        let busy = service
            .store()
            .claim(INSTANCE, SESSION, "evt-ok")
            .expect_err("concurrent claim must be refused");
        assert_eq!(busy.code, codes::ACTION_STATE_CONFLICT);
        assert!(busy.detail.contains("in flight"), "{}", busy.detail);
        // 手工回填成 ready，走真实 answer 路径。
        assert!(service
            .store()
            .release(INSTANCE, &claimed, PendingState::Ready));

        // ② 宿主受理 → 移出 + settled。
        let outcome = service
            .answer(INSTANCE, SESSION, &target, "evt-ok", &answers)
            .await
            .expect("host accepted");
        assert!(outcome.receipt_accepted);
        assert_eq!(
            service.store().entry_state(INSTANCE, "evt-ok"),
            None,
            "宿主受理后才移出待答表"
        );
        assert!(service.store().pending(INSTANCE, SESSION).is_empty());
        // 已 settled → 再次作答被拒（且不重复写宿主）。
        let dup = service
            .answer(INSTANCE, SESSION, &target, "evt-ok", &answers)
            .await
            .expect_err("second answer refused");
        assert_eq!(dup.code, codes::ACTION_STATE_CONFLICT);
        assert_eq!(service.send_count(), 1, "宿主只被写一次");
    }

    #[tokio::test]
    async fn abandoned_answer_request_still_settles_the_receipt() {
        struct WaitingSender {
            started: tokio::sync::Notify,
            finish: tokio::sync::Notify,
        }
        impl LiveResultSender for WaitingSender {
            fn post_result<'a>(
                &'a self,
                _instance_id: &'a str,
                _rpc_id: &'a str,
                _args: &'a serde_json::Value,
                _target: Option<(
                    &'a Target,
                    &'a crate::services::dsh_control::production::SendAcceptance,
                )>,
            ) -> BoxFuture<'a, Result<LiveReceiptOutcome, DshError>> {
                Box::pin(async move {
                    self.started.notify_one();
                    self.finish.notified().await;
                    Ok(LiveReceiptOutcome::Accepted)
                })
            }
        }
        let service = Arc::new(LiveQuestionService::new());
        let sender = Arc::new(WaitingSender {
            started: tokio::sync::Notify::new(),
            finish: tokio::sync::Notify::new(),
        });
        service.install_wiring(Arc::new(|_| unreachable!()), sender.clone());
        service.store().begin_stream(INSTANCE, "client-1");
        service.store().ingest(
            INSTANCE,
            generation_of(service.store()),
            &question_frame("evt-abandoned", SESSION),
        );
        let waiter_service = Arc::clone(&service);
        let waiter = tokio::spawn(async move {
            waiter_service
                .answer(
                    INSTANCE,
                    SESSION,
                    &target(),
                    "evt-abandoned",
                    &[answer("q1", &["A"], None), answer("q2", &["yes"], None)],
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), sender.started.notified())
            .await
            .unwrap();
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        sender.finish.notify_one();
        tokio::task::yield_now().await;
        assert_eq!(service.store().entry_state(INSTANCE, "evt-abandoned"), None);
    }

    /// 送达未知 → 状态 unknown-delivery（不宣称成功），且允许重试。
    #[tokio::test]
    async fn unknown_delivery_is_recorded_and_remains_retryable() {
        let service = wired_service();
        service.store().begin_stream(INSTANCE, "client-1");
        let generation = generation_of(service.store());
        service.store().ingest(
            INSTANCE,
            generation,
            &question_frame("evt-unknown", SESSION),
        );
        service.script(&[FakeFailure::DeliveryUnknown]);
        let target = target();
        let answers = [answer("q1", &["A"], None), answer("q2", &["yes"], None)];
        let error = service
            .answer(INSTANCE, SESSION, &target, "evt-unknown", &answers)
            .await
            .expect_err("unknown delivery is not success");
        assert_eq!(error.code, codes::WRITE_TIMEOUT);
        assert!(
            error.detail.contains("unknown") && error.detail.contains("safe"),
            "必须如实申报结局未知且说明可安全重试：{}",
            error.detail
        );
        assert_eq!(
            service.store().entry_state(INSTANCE, "evt-unknown"),
            Some(PendingState::UnknownDelivery),
            "结局未知必须单独记状态，不能冒充已答也不能消失"
        );
        // 重试（宿主端同 eventId 二次回执是安全 no-op）。
        service
            .answer(INSTANCE, SESSION, &target, "evt-unknown", &answers)
            .await
            .expect("retry after unknown delivery is allowed");
        assert_eq!(service.store().entry_state(INSTANCE, "evt-unknown"), None);
    }

    /// 宿主在提交中推 cancel → 条目被终结（不在表里，也不会卡在 in-flight）。
    #[test]
    fn host_cancels_an_in_flight_entry() {
        let store = ready_store();
        let generation = generation_of(&store);
        store.ingest(INSTANCE, generation, &question_frame("evt-race", SESSION));
        let claimed = store
            .claim(INSTANCE, SESSION, "evt-race")
            .expect("claim succeeds");
        assert_eq!(
            store.entry_state(INSTANCE, "evt-race"),
            Some(PendingState::InFlight)
        );
        // 宿主撤单（另一 answerer 抢答成功）。
        assert_eq!(
            store.ingest(
                INSTANCE,
                generation,
                &json!({"type": "cancel", "eventId": "evt-race"})
            ),
            IngestOutcome::Cancelled {
                event_id: "evt-race".into()
            }
        );
        assert_eq!(
            store.entry_state(INSTANCE, "evt-race"),
            None,
            "撤单必须终结 in-flight 条目，不能让它永卡提交中"
        );
        // 迟到的受理结算不得把已撤单的事件记成 settled。
        assert!(
            !store.settle(INSTANCE, &claimed),
            "撤单后再结算必须失败（否则会伪造一条已答记录）"
        );
    }

    /// 收件箱覆写：`NotExecuted` 回 ready（可重试），不是 unknown。
    #[tokio::test]
    async fn not_executed_returns_the_entry_to_ready() {
        let service = wired_service();
        service.store().begin_stream(INSTANCE, "client-1");
        let generation = generation_of(service.store());
        service
            .store()
            .ingest(INSTANCE, generation, &question_frame("evt-nego", SESSION));
        service.fail_next_with_not_executed();
        let target = target();
        let answers = [answer("q1", &["A"], None), answer("q2", &["yes"], None)];
        let error = service
            .answer(INSTANCE, SESSION, &target, "evt-nego", &answers)
            .await
            .expect_err("not executed is an error");
        assert_eq!(error.code, codes::CONNECTION_CLOSED);
        assert_eq!(
            service.store().entry_state(INSTANCE, "evt-nego"),
            Some(PendingState::Ready),
            "确定未写出必须回到可答（用户可原样重试）"
        );
    }

    /// 宿主业务拒绝 → 明确失败、条目回 ready；不宣称成功。
    #[tokio::test]
    async fn host_rejected_receipt_surfaces_an_error_and_restores_ready() {
        let service = wired_service();
        service.store().begin_stream(INSTANCE, "client-1");
        let generation = generation_of(service.store());
        service.store().ingest(
            INSTANCE,
            generation,
            &question_frame("evt-refused", SESSION),
        );
        service.script(&[FakeFailure::HostRejected]);
        let answers = [answer("q1", &["A"], None), answer("q2", &["yes"], None)];
        let error = service
            .answer(INSTANCE, SESSION, &target(), "evt-refused", &answers)
            .await
            .expect_err("host rejection is an error");
        assert_eq!(error.code, codes::STREAM_ERROR);
        assert!(
            error.detail.contains("gateway/invocation-unavailable"),
            "必须带宿主原始码：{}",
            error.detail
        );
        assert_eq!(
            service.store().entry_state(INSTANCE, "evt-refused"),
            Some(PendingState::Ready),
            "宿主拒绝后条目仍可重试（不是已答）"
        );
    }

    /// Peer 终态（401）→ 条目回 ready + 订阅挂起（等新凭据，不空转）。
    #[tokio::test]
    async fn peer_terminal_receipt_suspends_the_instance() {
        let service = wired_service();
        service.store().begin_stream(INSTANCE, "client-1");
        let generation = generation_of(service.store());
        service
            .store()
            .ingest(INSTANCE, generation, &question_frame("evt-401", SESSION));
        service.script(&[FakeFailure::PeerTerminal]);
        let answers = [answer("q1", &["A"], None), answer("q2", &["yes"], None)];
        let error = service
            .answer(INSTANCE, SESSION, &target(), "evt-401", &answers)
            .await
            .expect_err("peer terminal is an error");
        assert_eq!(error.code, codes::PEER_UNAUTHORIZED);
        assert!(
            service.is_suspended(INSTANCE),
            "401 必须挂起订阅（否则会无限重开一条必然失败的流）"
        );
        // 挂起即 reset_instance → 表已清空。
        assert!(service.store().pending(INSTANCE, SESSION).is_empty());
        let reopen = service
            .ensure_subscribed(INSTANCE)
            .expect_err("suspended instance must not reopen");
        assert_eq!(reopen.code, codes::PEER_UNAUTHORIZED);
    }

    // ============================================================
    // Blocker 3：每次拒收/淘汰都必须回执
    // ============================================================

    /// 缺 `agentId` 的问答帧：不入表 + 回 next（入表也永远查不到 = 永久挂起）。
    #[test]
    fn question_without_agent_id_is_forwarded_with_next() {
        let store = ready_store();
        let generation = generation_of(&store);
        let frame = json!({
            "type": "waterfall",
            "event": QUESTION_EVENT,
            "eventId": "evt-no-agent",
            "request": {"questions": [{"id": "q1", "question": "?"}]}
        });
        assert_eq!(
            store.ingest(INSTANCE, generation, &frame),
            IngestOutcome::ForwardToNext {
                event_id: "evt-no-agent".into(),
                event: QUESTION_EVENT.into(),
            }
        );
        assert!(store.pending(INSTANCE, SESSION).is_empty());
    }

    /// 自动回执无法确认送达 → 结束这条订阅（交给重订补投），不是只记日志。
    #[tokio::test]
    async fn unconfirmed_auto_receipt_ends_the_subscription() {
        // 审批事件需要回 next；让发送器一直「确定未写出」（超过有界重试）。
        let (service, sender) = wired_service_with_frames_with_script(
            vec![
                json!({"type": "ready", "clientId": "client-1", "host": {"home": "/h"}}),
                waterfall(
                    "approval/request",
                    "approval-1",
                    SESSION,
                    json!({"tool": "bash"}),
                ),
            ],
            vec![
                FakeFailure::NotExecuted,
                FakeFailure::NotExecuted,
                FakeFailure::NotExecuted,
            ],
        );
        let error = run_stream_once(&service, INSTANCE)
            .await
            .expect_err("unconfirmed receipt must fail the stream");
        assert_eq!(error.code, codes::STREAM_ERROR);
        assert!(
            error.detail.contains("not executed"),
            "必须留下可诊断原因：{}",
            error.detail
        );
        assert!(
            sender.calls() >= 3,
            "确定未写出必须有界重试（含首试），实际 {} 次",
            sender.calls()
        );
        // 流被判失败后待答表清空（重订会补投）。
        assert!(service.store().pending(INSTANCE, SESSION).is_empty());
    }

    /// 宿主受理后正常继续（不误判为需要断订）。
    #[tokio::test]
    async fn confirmed_auto_receipt_keeps_the_stream_running() {
        let (service, sender) = wired_service_with_frames(vec![
            json!({"type": "ready", "clientId": "client-1", "host": {"home": "/h"}}),
            waterfall(
                "approval/request",
                "approval-1",
                SESSION,
                json!({"tool": "bash"}),
            ),
            question_frame("evt-q", SESSION),
        ]);
        run_stream_once(&service, INSTANCE)
            .await
            .expect("流应正常跑完");
        assert_eq!(sender.calls(), 1, "只有审批需要即时回执");
        assert_eq!(sender.posted()[0]["outcome"], json!({"kind": "next"}));
    }

    /// P1-2：Peer 终态（401）→ 驱动停摆并挂起，不空转重试。
    #[tokio::test]
    async fn peer_terminal_state_suspends_the_subscription() {
        let service = LiveQuestionService::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_factory = Arc::clone(&calls);
        service.install_wiring(
            Arc::new(move |_instance: String| {
                calls_for_factory.fetch_add(1, Ordering::SeqCst);
                Box::pin(async {
                    Err(DshError::new(
                        codes::PEER_UNAUTHORIZED,
                        "fake: cookie is dead (401)",
                    ))
                })
            }),
            Arc::new(FakeSender::new()),
        );
        // 退避压到 1ms，若驱动真的空转就会反复计数。
        *service
            .backoff_override
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(Duration::from_millis(1));
        let service = Arc::new(service);
        let driver = tokio::spawn(run_driver(
            Arc::clone(&service),
            INSTANCE.to_string(),
            service.store.authorization_epoch(INSTANCE),
        ));
        // 给驱动足够时间空转（若实现错误会有几十次调用）。
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(
            driver.is_finished(),
            "Peer 终态后驱动必须退出，而不是无限重试"
        );
        driver.await.expect("driver task joins");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "401 只能尝试一次：终态不自动重连（等新凭据交付）"
        );
        assert!(
            service.is_suspended(INSTANCE),
            "必须挂起，否则 ensure_subscribed 会立刻重开一条必然 401 的流"
        );
        // 挂起期间 ensure_subscribed 直接拒绝。
        let error = service
            .ensure_subscribed(INSTANCE)
            .expect_err("suspended subscription must not reopen");
        assert_eq!(error.code, codes::PEER_UNAUTHORIZED);
        // 凭据交付（reset_instance）解除挂起。
        service.reset_instance(INSTANCE);
        assert!(!service.is_suspended(INSTANCE));
    }

    /// P2-1：ready 首帧超时 → 订阅判定失败（不把「已连接但无首帧」当健康）。
    #[tokio::test]
    async fn ready_timeout_fails_the_subscription() {
        let sender = FakeSender::new();
        let service = LiveQuestionService::new();
        service.install_wiring(
            // 永不下发任何帧的流。
            Arc::new(|_instance: String| {
                Box::pin(async { Ok(Box::new(SilentStream) as Box<dyn LiveEventStream>) })
            }),
            Arc::new(sender),
        );
        let service = Arc::new(service);
        // 直接构造一个短超时路径：用 tokio::time::pause 前先确认常量值合理。
        assert!(READY_TIMEOUT >= Duration::from_secs(1));
        // 用 timeout 包住 run_stream_once 并给足够余量，验证它在 READY_TIMEOUT 后返回错误。
        let outcome = tokio::time::timeout(
            READY_TIMEOUT + Duration::from_secs(5),
            run_stream_once(&service, INSTANCE),
        )
        .await
        .expect("run_stream_once must not hang forever")
        .expect_err("no ready frame must fail the subscription");
        assert_eq!(outcome.code, codes::READ_TIMEOUT);
        assert!(!service.is_live(INSTANCE), "live 必须保持 false");
    }

    /// 永不下发任何帧的流（ready 超时用例用）。
    struct SilentStream;

    impl LiveEventStream for SilentStream {
        fn next<'a>(&'a mut self) -> BoxFuture<'a, Option<serde_json::Value>> {
            Box::pin(std::future::pending())
        }
    }

    /// 带可编程回执脚本的装配（自动回执重试用例用）。
    fn wired_service_with_frames_with_script(
        frames: Vec<serde_json::Value>,
        script: Vec<FakeFailure>,
    ) -> (Arc<LiveQuestionService>, FakeSender) {
        let sender = FakeSender::with_script(script);
        let service = LiveQuestionService::new();
        let service_ref = &service;
        // 退避压到 1ms（自动回执的有界重试用真实退避会拖慢用例）。
        *service_ref
            .backoff_override
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(Duration::from_millis(1));
        let frames = Arc::new(Mutex::new(Some(frames)));
        service.install_wiring(
            Arc::new(move |_instance: String| {
                let frames = Arc::clone(&frames);
                Box::pin(async move {
                    let frames = frames
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .take()
                        .unwrap_or_default();
                    Ok(Box::new(FakeStream {
                        frames: frames.into(),
                    }) as Box<dyn LiveEventStream>)
                })
            }),
            Arc::new(sender.clone()),
        );
        (Arc::new(service), sender)
    }

    /// 驱动单次流生命周期：ready → 问答入表不回声 → 审批回 next → 流终结清表。
    #[tokio::test]
    async fn stream_run_answers_ready_and_forwards_non_question_waterfalls() {
        let (service, sender) = wired_service_with_frames(vec![
            json!({"type": "ready", "clientId": "client-1", "host": {"home": "/home/u"}}),
            question_frame("evt-q", SESSION),
            waterfall(
                "approval/request",
                "approval-1",
                SESSION,
                json!({"tool": "bash"}),
            ),
            json!({"type": "emit", "event": "api-session/status", "args": []}),
        ]);
        run_stream_once(&service, INSTANCE)
            .await
            .expect("stream ran to completion");
        let posted = sender.posted();
        assert_eq!(posted.len(), 1, "只有非问答 waterfall 需要即时回执");
        assert_eq!(posted[0]["eventId"], "approval-1");
        assert_eq!(posted[0]["clientId"], "client-1");
        assert_eq!(posted[0]["outcome"], json!({"kind": "next"}));
        assert_eq!(
            posted[0]["eventId"], "approval-1",
            "问答事件必须等用户作答，不得自动回执"
        );
        assert!(
            service.store().pending(INSTANCE, SESSION).is_empty(),
            "流终结（断开）必须清空待答表"
        );
    }

    /// 首帧不是 ready → 如实报错（不跳帧猜协议），且待答表保持空。
    #[tokio::test]
    async fn stream_without_ready_first_frame_fails_loudly() {
        let (service, sender) = wired_service_with_frames(vec![
            json!({"type": "waterfall", "event": QUESTION_EVENT, "eventId": "e1", "agentId": SESSION,
                   "request": {"questions": [{"id": "q1", "question": "?"}]}}),
        ]);
        let error = run_stream_once(&service, INSTANCE)
            .await
            .expect_err("first frame must be ready");
        assert_eq!(error.code, codes::STREAM_ERROR);
        assert!(
            error.detail.contains("first frame"),
            "错误必须说清是首帧不符：{}",
            error.detail
        );
        assert!(service.store().pending(INSTANCE, SESSION).is_empty());
        assert!(sender.posted().is_empty(), "无 clientId 时不得发出任何回执");
    }

    /// 未装配：ensure_subscribed / answer 都如实 DSH_UNSUPPORTED，不冒充已支持。
    #[tokio::test]
    async fn unwired_service_reports_unsupported() {
        let service = Arc::new(LiveQuestionService::new());
        let error = service
            .ensure_subscribed(INSTANCE)
            .expect_err("unwired subscription must be refused");
        assert_eq!(error.code, codes::UNSUPPORTED);
        let answer_error = service
            .answer(
                INSTANCE,
                SESSION,
                &target(),
                "evt-none",
                &[answer("q1", &["A"], None)],
            )
            .await
            .expect_err("unwired answering must be refused");
        assert_eq!(answer_error.code, codes::UNSUPPORTED);
        assert!(service.pending(INSTANCE, SESSION).is_empty());
    }

    // ---- 测试夹具 ----

    fn target() -> Target {
        Target {
            host_id: INSTANCE.to_string(),
            epoch: 7,
            session_id: SESSION.to_string(),
        }
    }

    #[derive(Clone)]
    struct FakeSender {
        inner: Arc<FakeSenderInner>,
    }

    /// 假发送器的可编程失败模式。
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum FakeFailure {
        /// `NotExecuted`（零字节出门可证）。
        NotExecuted,
        /// `DeliveryUnknown`（已交出但结局未知）。
        DeliveryUnknown,
        /// `HostRejected`（宿主业务拒绝）。
        HostRejected,
        /// `PeerTerminal`（401/409，必须停订阅）。
        PeerTerminal,
    }

    struct FakeSenderInner {
        posted: Mutex<Vec<serde_json::Value>>,
        calls: AtomicUsize,
        /// 接下来 N 次调用的行为（按顺序消耗；空 = 全部 Accepted）。
        script: Mutex<std::collections::VecDeque<FakeFailure>>,
    }

    impl LiveResultSender for FakeSender {
        fn post_result<'a>(
            &'a self,
            _instance_id: &'a str,
            _rpc_id: &'a str,
            args: &'a serde_json::Value,
            _target: Option<(
                &'a Target,
                &'a crate::services::dsh_control::production::SendAcceptance,
            )>,
        ) -> BoxFuture<'a, Result<LiveReceiptOutcome, DshError>> {
            Box::pin(async move {
                self.inner.calls.fetch_add(1, Ordering::SeqCst);
                self.inner
                    .posted
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push(args.clone());
                let scripted = self
                    .inner
                    .script
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .pop_front();
                Ok(match scripted {
                    None => LiveReceiptOutcome::Accepted,
                    Some(FakeFailure::NotExecuted) => LiveReceiptOutcome::NotExecuted {
                        error: DshError::new(
                            codes::CONNECTION_CLOSED,
                            "fake: connection closed before send",
                        ),
                    },
                    Some(FakeFailure::DeliveryUnknown) => LiveReceiptOutcome::DeliveryUnknown {
                        error: DshError::new(codes::WRITE_TIMEOUT, "fake: delivery unknown"),
                    },
                    Some(FakeFailure::HostRejected) => LiveReceiptOutcome::HostRejected {
                        code: "gateway/invocation-unavailable".into(),
                        detail: "fake: host refused".into(),
                    },
                    Some(FakeFailure::PeerTerminal) => LiveReceiptOutcome::PeerTerminal {
                        error: DshError::new(codes::PEER_UNAUTHORIZED, "fake: peer revoked"),
                    },
                })
            })
        }
    }

    impl FakeSender {
        fn new() -> Self {
            Self {
                inner: Arc::new(FakeSenderInner {
                    posted: Mutex::new(Vec::new()),
                    calls: AtomicUsize::new(0),
                    script: Mutex::new(std::collections::VecDeque::new()),
                }),
            }
        }

        fn with_script(script: Vec<FakeFailure>) -> Self {
            let sender = Self::new();
            *sender
                .inner
                .script
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = script.into();
            sender
        }

        fn posted(&self) -> Vec<serde_json::Value> {
            self.inner
                .posted
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
        }

        fn calls(&self) -> usize {
            self.inner.calls.load(Ordering::SeqCst)
        }
    }

    struct FakeStream {
        frames: std::collections::VecDeque<serde_json::Value>,
    }

    impl LiveEventStream for FakeStream {
        fn next<'a>(&'a mut self) -> BoxFuture<'a, Option<serde_json::Value>> {
            Box::pin(async move { self.frames.pop_front() })
        }
    }

    // 用例辅助（挂在测试内的轻量包装，避免为 fake 造 trait 对象）。
    struct WiredService {
        service: Arc<LiveQuestionService>,
        sender: FakeSender,
    }

    impl std::ops::Deref for WiredService {
        type Target = Arc<LiveQuestionService>;
        fn deref(&self) -> &Self::Target {
            &self.service
        }
    }

    impl WiredService {
        fn send_count(&self) -> usize {
            self.sender.calls()
        }

        fn fail_next_with_not_executed(&self) {
            self.script(&[FakeFailure::NotExecuted]);
        }

        fn script(&self, failures: &[FakeFailure]) {
            let mut script = self
                .sender
                .inner
                .script
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            for failure in failures {
                script.push_back(*failure);
            }
        }
    }

    fn wired_service_with_frames(
        frames: Vec<serde_json::Value>,
    ) -> (Arc<LiveQuestionService>, FakeSender) {
        let sender = FakeSender::new();
        let service = LiveQuestionService::new();
        let frames = Arc::new(Mutex::new(Some(frames)));
        service.install_wiring(
            Arc::new(move |_instance: String| {
                let frames = Arc::clone(&frames);
                Box::pin(async move {
                    let frames = frames
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .take()
                        .unwrap_or_default();
                    Ok(Box::new(FakeStream {
                        frames: frames.into(),
                    }) as Box<dyn LiveEventStream>)
                })
            }),
            Arc::new(sender.clone()),
        );
        (Arc::new(service), sender)
    }

    fn wired_service() -> WiredService {
        let (service, sender) = wired_service_with_frames(Vec::new());
        WiredService { service, sender }
    }
}
