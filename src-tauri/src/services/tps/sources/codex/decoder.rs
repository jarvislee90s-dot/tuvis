//! Codex 纯响应分段解码器（2026-10-26 桌面工作区与输出速度 V2 计划 Task 4；Spec §6）。
//!
//! ## 定位与边界
//!
//! **纯事件状态机**：把 rollout JSONL 行解成「按原生 `response_id` 键控的含等待响应
//! 观察」。不做任何文件 IO / DB 读写 / 原文持久化——行由调用方（Task 5 的有界读取器）
//! 逐行递入；状态里只有 **ID 摘要、时刻（epoch ms）、模型/渠道字符串、计数与未决标记**
//! （关联 ID 在解码边界即转 SHA-256 摘要），绝无 prompt、reasoning、工具正文、路径或
//! 原始 payload。`generation_ms` / `first_token_at_ms` / `native_duration_ms` 恒为
//! `None`——Codex 无流式窗，不推造首 Token、不伪装原生测量（Spec §6.2.5）。
//!
//! ## 状态机（行型 → 迁移；真实时序依据 codex-structure-facts.md）
//!
//! 真实一回合的行序：`task_started` → user/developer `message`（**输入边界**）→
//! `turn_context`（**模型归属**）→ 生成项（`reasoning` / assistant `message` /
//! `function_call` / `custom_tool_call`）→ `token_usage_record`（**封口**）→
//! `*_tool_call_output`（**工具返回 = 下一响应起点候选**）→ 伴随 `token_count` → …
//!
//! * **timestamp**：每行顶层 RFC3339（含毫秒）→ epoch ms；缺/坏 → 诊断并整行忽略，
//!   绝不因此重置状态。
//! * **输入边界**（更新 `boundary_ts` = 下一响应起点候选）：user/developer `message`、
//!   `function_call_output` / `custom_tool_call_output`。真实行序里工具返回总在**其后**
//!   响应的封口之后成组到达（call_id 与上一响应的工具调用对应）——「最后一组匹配工具
//!   返回的末条或更晚的有效输入，取封口后最近生成项之前的最后一条」由此自然成立；复制
//!   历史里的陈旧工具返回同样按输入边界处理，其携带的残留生成项由歧义/回合规则丢弃。
//! * **生成项**：`reasoning`、assistant `message`、`function_call`、`custom_tool_call`。
//!   首个生成项把 `boundary_ts` 冻结为响应起点；此后每项刷新终点候选。
//!   **`agent_message` 取舍（文档化）**：真实文件中它只出现在子代理协作回合内、介于
//!   输入边界与封口之间（模型发出的跨代理消息），故按生成项计；若未来出现在别的相位，
//!   现有「无边界则封口跳过」规则保证它不会造出无起点的窗。
//! * **`turn_context`**：更新模型归属（流内最近一次，绝不以当前设置补历史）+ 回合
//!   身份摘要；**丢弃**当时的未决封口/未封口生成项（新回合上下文使旧碎片不可归属；
//!   正常行序里它到达时两者皆空，无扰动）。不动 `boundary_ts`（它总在输入之后到达，
//!   首响应起点仍 = 最后一条输入边界）。
//! * **`task_started`**：同款丢弃 + **清空** `boundary_ts`（新回合起点只从本回合自己的
//!   输入起算，不借上一回合的边界）+ 回合身份摘要。
//! * **封口 `token_usage_record`**（唯一发射点）：`payload.response_id` 须为字符串、
//!   `payload.usage.output_tokens` 须为整数（0 允许作为候选，有效性归 Task 1 谓词），
//!   且流内已见过 model，否则诊断跳过（缺上下文/缺身份不凑窗）。摘要已发射过 →
//!   `duplicate response_id` 诊断，**完全不动状态**（尤其不移动下一响应起点）。正常序
//!   （生成项 ≥1）即封口即发射：起点 = 冻结的输入边界，终点 = 最后生成项，
//!   `response_elapsed_ms = 终点 − 起点` 且必须 > 0，否则诊断跳过。**延迟序**（封口先
//!   到、零生成项）→ 保存未决 [`PendingSeal`]（捕获封口行到达时的边界快照）：生成项
//!   随后在下一边界前出现 → 由下一个封口/边界关闭判定并提交（终点 = 末个生成项）；
//!   其间先遇到 `task_started` / `turn_context` / `task_complete` / `turn_aborted` →
//!   丢弃未决 + 诊断（回合边界前生成项未出现即不成立），先遇到工具返回/输入且尚无
//!   生成项 → 同样丢弃。**已发射的封口观察绝不回撤**（状态里根本不存已发射观察）。
//! * **封口即阶段边界**（终审裁决 Important 2，2026-10-26）：观察**成功提交**后
//!   `boundary_ts` 置 None——本响应的输入边界已随之消费，下一响应只认**新鲜**输入
//!   边界，缺失则以 `missing-input-boundary` 保守跳过，绝不复用封口前的旧边界（双倒序
//!   序列会否则把上一响应的生成段+间隔整个并入下一响应窗：时长虚高、速度低估）。
//!   只在提交时清、不在封口行到达时清（倒序配对需要封口前的边界才能结算生成项）；
//!   未决封口结算不受清空影响——它已捕获自己的边界快照（`PendingSeal::start_ts`）。
//!   两个已申报的结算语义：**封口行关闭**未决（延迟写序）用捕获快照起点；**输入边界
//!   关闭**未决要求生成项自冻结的新鲜起点（有中间提交则 None → 保守跳过）。
//!   **单调护栏**（二轮复审 B2）：快照自身也可能陈旧——封口→生成→封口→生成→封口且
//!   中间无输入/工具返回行时，未决封口捕获的 `start_ts` 是上一响应已消费的旧边界，
//!   `.or(生成项冻结起点)` 救不回。故提交前再查 `last_committed_end`（本文件流内最近
//!   一次成功发射的终点）：候选起点早于它 → `stale-start-after-commit` 保守跳过，绝不
//!   发射重叠窗；护栏只前进、跳过不回退（见 [`CodexDecoderState::last_committed_end`]）。
//! * **伴随 `event_msg/token_count`**：永不发射、永不动边界；只把
//!   `info.last_token_usage.output_tokens` 与最近一次有效封口对账，不符出诊断。
//!   `event_msg/item_completed` 不作响应起止、不推首 Token；`event_msg/task_complete`
//!   只丢弃未闭合状态，**绝不用 `duration_ms − time_to_first_token_ms` 拼响应窗**。
//! * **`turn_aborted`**（真实行名已在真机核实：`event_msg` + `payload.type=turn_aborted`，
//!   载荷键 type/root_turn_id/turn_id/reason/started_at）：与 `task_complete` 同款丢弃；
//!   已封口观察不回撤。错误/重试标记按防御性名单（`error`/`stream_error`/`turn_failed`）
//!   同款丢弃——本机样本未出现这些行名，且无论何种情况都**不以回合时长补窗**。
//! * **其余行型**（`session_meta` 记渠道/会话摘要、`world_state`、`compacted`、
//!   `item_completed`、`additional_tools`…）：只取身份/渠道或静默忽略，不构成边界。
//!
//! ## 裁决与如实申报
//!
//! * **Purpose = MainTask**：真机 rollout 核对 `collaboration_mode_kind` 恒为
//!   `default`、`turn_attribution.turn_trigger` ∈ {composer, scheduled_task_setup,
//!   automation_heartbeat_scheduled, None}——无任何可识别的「辅助标题/自动摘要」标记，
//!   首版一律 MainTask，不猜。
//! * 未决状态可 serde 往返（Task 5 跨批次持久化）；`seen_response_digests` 随状态持久，
//!   复制历史跨批次同样只计一次。**已发射摘要集有界**（Task 5：上限
//!   [`DIGEST_CAP`] = 128，FIFO 驱逐——游标每个采集轮（可见节奏 5s）整体重写，上限
//!   压住单文件状态体积；同流内重复封口天然相邻，128 已远超真实去重距离）——同文件
//!   超长历史的第 129+ 个响应在被驱逐后若同 id 再现，同文件内会重复发射一次，由 DB
//!   对 `stable_call_key` 的 UPSERT（同 host_instance）兜底幂等；跨文件去重本就是
//!   DB 的职责，不归解码器。

use std::collections::{HashSet, VecDeque};

use chrono::DateTime;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::SOURCE_ID;
use crate::services::tps::model::{Outcome, OutputScope, Purpose, TimingMethod, TpsObservation};

/// 单行解码结果：观察 + 诊断（诊断只含行型与原因，不含任何正文/原值；摘要前缀 ≤ 8 字符）
#[derive(Debug, Clone, Default)]
pub struct DecodeResult {
    pub observations: Vec<TpsObservation>,
    pub diagnostics: Vec<String>,
}

/// 已发射摘要集的有界上限（Task 5；裁决 R2：128 而非 512——游标每采集轮整体重写，
/// 压住单文件状态体积；同流重复封口相邻，跨文件去重由 DB UPSERT 兜底）：
/// FIFO 驱逐最老摘要，驱逐后的同 id 再现重复发射一次、UPSERT 收敛
const DIGEST_CAP: usize = 128;

/// 解码器状态：只有 ID 摘要 / 时刻 / 模型与渠道字符串 / 计数与未决标记。
/// Task 5 将把它随游标持久化（跨批次去重 + 未决封口续算），故全字段 serde 友好。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CodexDecoderState {
    /// 已**发射**观察的 response_id 摘要全集（同流/跨批去重；诊断跳过的封口不入集，
    /// 其身份未被消费，之后同 id 合格再现仍可正常发射）。有界：上限 [`DIGEST_CAP`]，
    /// FIFO 驱逐序见 [`CodexDecoderState::digest_order`]
    seen_response_digests: HashSet<String>,
    /// 已发射摘要的 FIFO 驱逐序（与 `seen_response_digests` 同步维护；serde 持久化，
    /// 旧状态缺字段 = 无序要求，从空起算）
    #[serde(default)]
    digest_order: VecDeque<String>,
    /// 流内最近一次 `turn_context.payload.model`（模型归属；未见 model 的封口诊断跳过）
    current_model: Option<String>,
    /// `session_meta.payload.model_provider`（渠道；未见为 None）
    current_channel: Option<String>,
    /// session_id 摘要（session_meta / token_usage_record）
    session_digest: Option<String>,
    /// 回合身份摘要（最近一次 task_started / turn_context / token_usage_record 的 turn_id）
    turn_digest: Option<String>,
    /// 最后一条输入边界时刻（当前响应起点候选；task_started 清空，turn_context 不动；
    /// **观察成功提交后置空**——封口即阶段边界，下一响应只认新鲜边界，绝不复用封口前的）
    boundary_ts: Option<i64>,
    /// 未封口响应的生成项累积（Some = 封口前已有 ≥1 生成项）
    open_response: Option<OpenResponse>,
    /// 先于生成项到达的封口（延迟序未决，自带边界快照上下文；serde 持久化的核心目标）
    pending_seal: Option<PendingSeal>,
    /// 最近一次**成功发射**观察的终点 ms（二轮复审 B2 单调护栏，`#[serde(default)]`
    /// 兼容旧游标）：本文件流内已提交窗的单调下界——下一候选窗起点早于它 = 陈旧起点
    /// 快照（封口→生成→封口→生成→封口且中间无输入/工具返回行时，未决封口捕获的
    /// `start_ts` 是上一响应消费过的旧边界）→ 提交前拒绝发射（保守跳过，身份不消费）。
    /// 只在成功发射后前进、任何跳过都不回退；`invalidate_inflight` / 回合边界也不清
    /// （已发射事实不因在途状态丢弃而消失，重叠防线必须保留）。
    /// **升级空窗（scoped 评审 ② 如实申报）**：旧游标经 serde default 首轮加载为
    /// None，若其恰带 stale 待决快照，升级后至多漏防一窗（一次性有界）；自次轮起护栏
    /// 生效。
    #[serde(default)]
    last_committed_end: Option<i64>,
    /// 最近一次**有效**封口的 output_tokens（伴随 token_count 对账基准；0 合法）
    last_seal_output_tokens: Option<i64>,
}

/// 未封口响应的生成项累积（起点在首个生成项时刻冻结，此后边界不可能移动——
/// 边界行此刻到达即触发歧义丢弃）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct OpenResponse {
    /// 响应起点（首个生成项前最后一条输入边界；None = 无边界可起算，封口时诊断跳过）
    start_ts: Option<i64>,
    /// 最后生成项时刻（响应终点候选）
    last_ts: i64,
    /// 生成项计数（≥1 恒成立）
    item_count: u64,
}

/// 延迟序未决封口：身份与输出已定、窗未定（等生成项补齐后再提交）。
/// `start_ts` = 封口行到达时的输入边界快照（本响应起点候选，终审裁决 Important 2）——
/// 延迟级联里更早响应的中间提交会把 `boundary_ts` 清空，未决封口凭这份自捕获的上下文
/// 结算，不受清空影响；快照缺失（旧游标状态缺字段 / 封口时本就无边界）回退生成项
/// 冻结的起点。serde 持久化的核心目标（跨批次续算）。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingSeal {
    response_digest: String,
    output_tokens: i64,
    /// 封口行到达时的边界快照（`#[serde(default)]` 兼容旧游标状态）
    #[serde(default)]
    start_ts: Option<i64>,
}

/// 喂入一行 rollout JSON；返回该行产生的观察与诊断。
/// 纯函数：只改 `state`，无任何 IO；行序即真实落盘序。
pub fn push_record(
    state: &mut CodexDecoderState,
    row: &serde_json::Value,
    host_instance: &str,
) -> DecodeResult {
    let mut out = DecodeResult::default();
    // 无可解析 timestamp：诊断并整行忽略（绝不重置状态）
    let Some(ts) = row
        .get("timestamp")
        .and_then(serde_json::Value::as_str)
        .and_then(parse_rfc3339_ms)
    else {
        out.diagnostics.push(format!(
            "{SOURCE_ID}: missing-or-bad-timestamp line-ignored"
        ));
        return out;
    };
    let row_type = row
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let payload = row.get("payload");
    match row_type {
        "session_meta" => on_session_meta(state, payload),
        "turn_context" => on_turn_context(state, payload, &mut out),
        "response_item" => on_response_item(state, payload, ts, host_instance, &mut out),
        "token_usage_record" => on_token_usage_record(state, payload, host_instance, &mut out),
        "event_msg" => on_event_msg(state, payload, &mut out),
        // world_state / compacted / inter_agent_communication_metadata …：与计时/身份无关
        _ => {}
    }
    out
}

/// RFC3339（含毫秒）→ epoch ms
fn parse_rfc3339_ms(s: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

/// 把一个已发射摘要记入有界集合：到顶先 FIFO 驱逐最老者，再插入（Task 5）
fn remember_digest(state: &mut CodexDecoderState, digest: String) {
    if state.seen_response_digests.len() >= DIGEST_CAP
        && !state.seen_response_digests.contains(&digest)
    {
        if let Some(oldest) = state.digest_order.pop_front() {
            state.seen_response_digests.remove(&oldest);
        }
    }
    if state.seen_response_digests.insert(digest.clone()) {
        state.digest_order.push_back(digest);
    }
}

/// 丢弃全部在途未决状态（Task 5 读取器用：超大行/坏行使行内依赖的未决片段不可信）：
/// 未决封口、未封口生成项与当前输入边界一并清除——**下一个可靠输入边界到来前不再
/// 计速**（读取起点/损坏点绝不当请求起点）。返回丢弃的片段数（0–3，供诊断计数）。
pub fn invalidate_inflight(state: &mut CodexDecoderState) -> usize {
    let mut dropped = 0;
    if state.pending_seal.take().is_some() {
        dropped += 1;
    }
    if state.open_response.take().is_some() {
        dropped += 1;
    }
    if state.boundary_ts.take().is_some() {
        dropped += 1;
    }
    dropped
}

/// 是否还挂着未决封口（延迟序：封口先到、生成项未补齐）。读取器用它做游标剪枝
/// 门（裁决 R2b）：文件安静超窗且读尽但仍有未决封口 → 保留游标条目，等下轮补齐
/// 判定，绝不把跨轮未决状态随剪枝丢掉。
pub fn has_pending_seal(state: &CodexDecoderState) -> bool {
    state.pending_seal.is_some()
}

fn sha256_hex(s: &str) -> String {
    let digest = Sha256::digest(s.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

// ==== 行型处理 ====

/// `session_meta`：只取渠道与会话身份，不构成边界
fn on_session_meta(state: &mut CodexDecoderState, payload: Option<&serde_json::Value>) {
    let Some(p) = payload else { return };
    if let Some(provider) = p.get("model_provider").and_then(serde_json::Value::as_str) {
        state.current_channel = Some(provider.to_string());
    }
    if let Some(sid) = p.get("session_id").and_then(serde_json::Value::as_str) {
        state.session_digest = Some(sha256_hex(sid));
    }
}

/// `turn_context`：模型归属 + 回合身份；丢弃当时的未决/未封口碎片（正常行序下两者皆空）
fn on_turn_context(
    state: &mut CodexDecoderState,
    payload: Option<&serde_json::Value>,
    out: &mut DecodeResult,
) {
    let Some(p) = payload else { return };
    if let Some(model) = p.get("model").and_then(serde_json::Value::as_str) {
        state.current_model = Some(model.to_string());
    }
    drop_turn_state(state, "turn_context", out);
    if let Some(turn_id) = p.get("turn_id").and_then(serde_json::Value::as_str) {
        state.turn_digest = Some(sha256_hex(turn_id));
    }
}

/// `event_msg` 分派（回合开始/终态、伴随对账、防御性错误标记；item_completed 等忽略）
fn on_event_msg(
    state: &mut CodexDecoderState,
    payload: Option<&serde_json::Value>,
    out: &mut DecodeResult,
) {
    let Some(p) = payload else { return };
    match p
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
    {
        "task_started" => {
            drop_turn_state(state, "task_started", out);
            // 新回合：起点只从本回合自己的输入起算（真实行序 task_started 先于输入）
            state.boundary_ts = None;
            if let Some(turn_id) = p.get("turn_id").and_then(serde_json::Value::as_str) {
                state.turn_digest = Some(sha256_hex(turn_id));
            }
        }
        // 回合终态：只丢弃未闭合状态；已发射的封口观察绝不回撤。
        // duration_ms / time_to_first_token_ms 是回合墙钟，绝不入任何响应窗。
        "task_complete" | "turn_aborted" => drop_turn_state(
            state,
            p.get("type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default(),
            out,
        ),
        // 防御性错误/重试标记（本机样本未出现该行名）：不可归属即丢弃，不以回合时长补窗
        "error" | "stream_error" | "turn_failed" => drop_turn_state(state, "error", out),
        // 伴随对账：永不发射、永不动边界
        "token_count" => {
            let last_out = p
                .get("info")
                .and_then(|i| i.get("last_token_usage"))
                .and_then(|u| u.get("output_tokens"))
                .and_then(serde_json::Value::as_i64);
            if let (Some(sealed), Some(counted)) = (state.last_seal_output_tokens, last_out) {
                if sealed != counted {
                    out.diagnostics.push(format!(
                        "{SOURCE_ID}: token-count-mismatch counted={counted} sealed={sealed} reconcile-only"
                    ));
                }
            }
        }
        _ => {}
    }
}

/// 回合边界/终态通用的未闭合状态丢弃（各带诊断；什么都没有则静默）
fn drop_turn_state(state: &mut CodexDecoderState, row_kind: &str, out: &mut DecodeResult) {
    if state.pending_seal.take().is_some() {
        out.diagnostics
            .push(format!("{SOURCE_ID}: {row_kind} pending-seal-dropped"));
    }
    if state.open_response.take().is_some() {
        out.diagnostics
            .push(format!("{SOURCE_ID}: {row_kind} open-response-dropped"));
    }
}

/// `response_item` 分派：输入边界 / 生成项 / 忽略
fn on_response_item(
    state: &mut CodexDecoderState,
    payload: Option<&serde_json::Value>,
    ts: i64,
    host_instance: &str,
    out: &mut DecodeResult,
) {
    let Some(p) = payload else { return };
    let item_type = p
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let role = p.get("role").and_then(serde_json::Value::as_str);
    match (item_type, role) {
        // 输入边界：user/developer 消息、工具返回
        ("message", Some("user" | "developer")) => {
            input_boundary(state, ts, "message", host_instance, out)
        }
        ("function_call_output" | "custom_tool_call_output", _) => {
            input_boundary(state, ts, item_type, host_instance, out)
        }
        // 生成项：reasoning / assistant message / 模型发出的工具调用 / 跨代理消息（见模块头取舍）
        ("message", Some("assistant")) => generation_item(state, ts),
        ("reasoning" | "function_call" | "custom_tool_call" | "agent_message", _) => {
            generation_item(state, ts)
        }
        // additional_tools / configuration_update 等：非输入非生成
        _ => {}
    }
}

/// 输入边界（user/developer 消息、工具返回）：更新起点候选。
/// 有未决封口 → 此刻关闭其判定（生成项已出现则提交，否则丢弃）——边界关闭者要求
/// 生成项自冻结的新鲜起点（见 [`resolve_pending_seal`]，中间提交清空边界 → 保守跳过）；
/// 否则有未封口生成项 → 歧义（封口前插入输入/工具返回），诊断丢弃后本行成为新边界。
fn input_boundary(
    state: &mut CodexDecoderState,
    ts: i64,
    row_kind: &str,
    host_instance: &str,
    out: &mut DecodeResult,
) {
    if state.pending_seal.is_some() {
        resolve_pending_seal(state, row_kind, false, host_instance, out);
    } else if state.open_response.take().is_some() {
        out.diagnostics.push(format!(
            "{SOURCE_ID}: {row_kind} generation-before-seal ambiguous open-response-dropped"
        ));
    }
    state.boundary_ts = Some(ts);
}

/// 生成项：首个生成项把当前边界冻结为起点，此后刷新终点候选
fn generation_item(state: &mut CodexDecoderState, ts: i64) {
    match state.open_response.as_mut() {
        Some(open) => {
            open.last_ts = ts;
            open.item_count += 1;
        }
        None => {
            state.open_response = Some(OpenResponse {
                start_ts: state.boundary_ts,
                last_ts: ts,
                item_count: 1,
            });
        }
    }
}

/// 关闭未决封口的判定（关闭者 = 下一封口 / 输入 / 工具返回）：生成项已出现 → 提交，
/// 否则丢弃（延迟序不成立，绝不猜窗）。
/// 起点按关闭者分两路（终审裁决 Important 2）：
/// * `closer_is_seal`（封口行关闭，延迟写序：响应的生成项滞后于自己的封口行）→ 用
///   未决捕获的边界快照（中间提交清空 `boundary_ts` 不影响本结算，见
///   [`PendingSeal::start_ts`]；快照缺失回退生成项冻结的起点）；
/// * 输入边界关闭 → 用生成项冻结的起点；若曾有中间提交清空边界则为 None →
///   `missing-input-boundary` 保守跳过（生成项先于新鲜边界出现，绝不复用封口前旧边界）。
fn resolve_pending_seal(
    state: &mut CodexDecoderState,
    closer_kind: &str,
    closer_is_seal: bool,
    host_instance: &str,
    out: &mut DecodeResult,
) {
    let Some(pending) = state.pending_seal.take() else {
        return;
    };
    match state.open_response.take() {
        Some(open) => {
            let start = if closer_is_seal {
                pending.start_ts.or(open.start_ts)
            } else {
                open.start_ts
            };
            commit_response(
                pending.response_digest,
                pending.output_tokens,
                open,
                start,
                host_instance,
                state,
                out,
            );
        }
        None => out.diagnostics.push(format!(
            "{SOURCE_ID}: deferred-seal-dropped closer={closer_kind}"
        )),
    }
}

/// 封口 `token_usage_record`：唯一发射点（正常序）或未决保存点（延迟序）
fn on_token_usage_record(
    state: &mut CodexDecoderState,
    payload: Option<&serde_json::Value>,
    host_instance: &str,
    out: &mut DecodeResult,
) {
    let Some(p) = payload else { return };
    // 身份：response_id 须为字符串
    let Some(response_id) = p.get("response_id").and_then(serde_json::Value::as_str) else {
        out.diagnostics.push(format!(
            "{SOURCE_ID}: token-usage-record response-id-not-string skipped"
        ));
        return;
    };
    // 输出：output_tokens 须为整数（0 允许作为候选；有效性归 Task 1 谓词）
    let Some(output_tokens) = p
        .get("usage")
        .and_then(|u| u.get("output_tokens"))
        .and_then(serde_json::Value::as_i64)
    else {
        out.diagnostics.push(format!(
            "{SOURCE_ID}: token-usage-record output-tokens-not-integer skipped"
        ));
        return;
    };
    let digest = sha256_hex(response_id);
    // 复制历史/伴随重放：同 id 只计一次，且完全不动状态（尤其不移动下一响应起点）
    if state.seen_response_digests.contains(&digest) {
        out.diagnostics.push(format!(
            "{SOURCE_ID}: token-usage-record duplicate-response-id prefix={} counted-once",
            &digest[..8]
        ));
        return;
    }
    // 模型归属：流内未见 turn_context → 诊断跳过，绝不以当前设置补历史
    if state.current_model.is_none() {
        out.diagnostics.push(format!(
            "{SOURCE_ID}: token-usage-record missing-model skipped"
        ));
        return;
    };
    // 封口身份入会话/回合摘要；对账簿记只记**有效**封口
    if let Some(session_id) = p.get("session_id").and_then(serde_json::Value::as_str) {
        state.session_digest = Some(sha256_hex(session_id));
    }
    if let Some(turn_id) = p.get("turn_id").and_then(serde_json::Value::as_str) {
        state.turn_digest = Some(sha256_hex(turn_id));
    }
    state.last_seal_output_tokens = Some(output_tokens);
    // 封口行到达时的边界快照：若本行走延迟序，捕获为本响应的起点候选（它自己的上下文）。
    // 必须先于 resolve 捕获——下面关闭更早未决的提交会清空 boundary_ts（封口即阶段边界）。
    let boundary_at_seal = state.boundary_ts;
    // 更早的未决封口在此关闭（延迟序：生成项出现在两个封口之间 → 归前一响应）
    resolve_pending_seal(state, "token_usage_record", true, host_instance, out);
    match state.open_response.take() {
        // 正常序：生成项已就位 → 立即结算（起点 = 冻结的输入边界）
        Some(open) => {
            let start = open.start_ts;
            commit_response(
                digest,
                output_tokens,
                open,
                start,
                host_instance,
                state,
                out,
            );
        }
        // 延迟序：零生成项 → 保存未决（自带边界快照），等生成项在下一边界前出现
        None => {
            state.pending_seal = Some(PendingSeal {
                response_digest: digest,
                output_tokens,
                start_ts: boundary_at_seal,
            });
        }
    }
}

/// 结算一个响应观察（正常序与未决封口结算共用）：窗 = start（语义由关闭者决定，见
/// [`resolve_pending_seal`]）→ 最后生成项；非正窗 / 缺起点 / 缺 model 诊断跳过
/// （跳过者不入已发射集合，身份未被消费）。未决封口成立时 model 必已可见（封口时已
/// 检查且流内不回退），此处兜底同一诊断。
/// **成功提交后 `boundary_ts` 置 None**（封口即阶段边界，终审裁决 Important 2）：下一
/// 响应只认新鲜输入边界，缺失以 `missing-input-boundary` 保守跳过，绝不复用封口前的
/// 旧边界——否则双倒序序列会把上一响应的生成段+间隔整个并入下一响应窗（时长虚高、
/// 速度低估）。只在提交时清、不在封口行到达时清（倒序配对需要封口前的边界）。
/// **单调护栏**（二轮复审 B2）：候选起点早于 `last_committed_end`（本文件流内最近一次
/// 成功发射的终点）= 陈旧起点快照（无边界行的封口→生成→封口→生成→封口序列里，未决
/// 封口的 `start_ts` 是上一响应已消费的旧边界）→ `stale-start-after-commit` 保守跳过，
/// 绝不发射重叠窗；护栏只前进、任何跳过不回退。
fn commit_response(
    response_digest: String,
    output_tokens: i64,
    open: OpenResponse,
    start: Option<i64>,
    host_instance: &str,
    state: &mut CodexDecoderState,
    out: &mut DecodeResult,
) {
    let Some(model) = state.current_model.clone() else {
        out.diagnostics.push(format!(
            "{SOURCE_ID}: token-usage-record missing-model skipped"
        ));
        return;
    };
    let Some(start) = start else {
        out.diagnostics.push(format!(
            "{SOURCE_ID}: token-usage-record missing-input-boundary skipped"
        ));
        return;
    };
    let elapsed = open.last_ts - start;
    if elapsed <= 0 {
        out.diagnostics.push(format!(
            "{SOURCE_ID}: token-usage-record non-positive-window elapsed_ms={elapsed} skipped"
        ));
        return;
    }
    // 单调护栏：与已发射窗重叠（起点早于上一发射终点）= 陈旧起点，保守跳过
    if let Some(prev_end) = state.last_committed_end {
        if start < prev_end {
            out.diagnostics.push(format!(
                "{SOURCE_ID}: token-usage-record stale-start-after-commit \
                 start_ms={start} last_end_ms={prev_end} skipped"
            ));
            return;
        }
    }
    remember_digest(state, response_digest.clone());
    out.observations.push(TpsObservation {
        source: SOURCE_ID.to_string(),
        host_instance: host_instance.to_string(),
        stable_call_key: response_digest,
        started_at_ms: start,
        // 无流式窗：首点/生成窗/原生时长恒缺失，不推造（Spec §6.2.5）
        first_token_at_ms: None,
        ended_at_ms: open.last_ts,
        native_duration_ms: None,
        generation_ms: None,
        response_elapsed_ms: Some(elapsed),
        output_tokens: Some(output_tokens),
        // 真机样本 usage.output_tokens ≥ reasoning_output_tokens（含思考）
        output_scope: OutputScope::IncludesReasoning,
        tool_ms: None,
        model,
        channel: state.current_channel.clone(),
        session_key: state.session_digest.clone(),
        turn_key: state.turn_digest.clone(),
        // 真机无辅助标题/自动摘要标记（见模块头申报），一律主任务
        purpose: Purpose::MainTask,
        outcome: Outcome::Success,
        method: TimingMethod::LogBoundaryApprox,
        failure_reason: None,
    });
    // 封口即阶段边界：本响应的输入边界已随本观察消费（见函数头申报）。
    state.boundary_ts = None;
    // 单调护栏只在成功发射后前进（open.last_ts = 刚发射观察的终点）
    state.last_committed_end = Some(open.last_ts);
}
