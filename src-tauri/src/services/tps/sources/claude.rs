//! Claude 本机遥测读取器（Spec §4 Claude 行 + §5 接收边界；计划 Task 5）。
//!
//! ## 读取对象与口径（Spec §4 Claude 行逐条）
//!
//! 本机 `/v1/traces` 的 **OTLP JSON** 元数据（loopback + 独立 Bearer + 1MiB 上限，
//! 守卫与端口/凭证持久化在 `services::tps::install`；本模块只做**字节 → 观察**的
//! 纯解码，不开 socket、不读文件系统）。字段证据：
//! `docs/superpowers/evidence/2026-10-24-model-speed/otel-boundaries-addendum.md` §2
//! （span 属性白名单：`duration_ms` / `ttft_ms` / `first_content_ms` / `output_tokens`
//! / `attempt` / `success` / `model` / `session.id`…）+ supplement 探针
//! `observed/claude-otel.jsonl`（真机 2.1.293 实采，trace a22d4d3c… 四 span 正例）。
//!
//! * **只认** `claude_code.llm_request` span（官方 llm_request span；metrics 面没有
//!   时延直方图，traces 是计时与 token 同条的唯一入口——supplement 探针 §0 实测）；
//! * **首点 = first_content_ms**（首次 `content_block_start`，text/thinking/tool_use
//!   共用）并标 [`TimingMethod::NativeEventApprox`]——**不得称首非空 delta**（内容块
//!   开始 ≠ 首个非空内容，otel-boundaries §2 表格逐字边界）；原生 `ttft_ms`
//!   （message_start 基线）**不落观察**，仅作探针/报告层的诊断对照值；
//! * **生成窗 = duration_ms − first_content_ms**；attempt=1 且 success 且 output
//!   齐全才可能入速（spec §4）；
//! * **响应窗（V2 Spec §5.2「含等待响应」）= 完整 span `duration_ms`（含等待；工具
//!   执行在 llm_request span 之外，不在窗内）**：与生成窗同一套闸（成功证据闸 +
//!   尝试闸，已识别重试不对齐不填）；缺 first_content 的 span response-only 有效；
//! * **attempt>1（或 attempt 缺失）排除速度**：span `duration_ms` 是含重试的逻辑请求
//!   窗（otel-boundaries §2「属于包含重试的逻辑请求窗口」），成功尝试的首点基线与它
//!   不同批——生成窗/首点置空 + 诊断，原生时长保留作耗时详情（`retried.tps() == None`）；
//! * **缺 `success` 属性不默认成功**（Spec §4「success 及 output 齐全」才入速；字段
//!   可缺有实证）：观察保留、生成窗/首点置空 + 诊断；成功 span 缺 `duration_ms` /
//!   `output_tokens` 同样附诊断，绝不静默产非速观测；
//! * **缺 first_content 不借回合值补齐**：生成窗/首点留 None + 诊断，绝不拿 ttft、
//!   回合墙钟或其它请求的值 fabricate（spec §4「缺 first_content 不借回合值补齐」）；
//! * **按 interaction/trace 分回合**：`turn_key = trace_id`（原生 trace 身份），
//!   `session_key = session.id`；`stable_call_key = trace_id + span_id`（重投递幂等）；
//! * **工具 span 独立**：非 llm_request span（tool.execution 等）不产观察、不计入
//!   模型生成窗（工具间隙在 llm_request span 之外，天然不进 per-span 窗口）；v1 不
//!   采纳工具用时 series（无证据的归属不发明），`tool_ms` 恒空（与 Task 4 Kimi 同款
//!   申报）。
//!
//! ## 输出语义
//!
//! `output_tokens` 是否已含 thinking 在本机版本**未单独声明**（CHANGE-PREVIEW §3(d)
//! 列为未测试项）→ [`OutputScope::ProviderReported`]，不按字符估思考、不宣称测得
//! 隐藏思考（Spec §3）。
//!
//! ## 错误码（码表登记纪律：只用 `USAGE_CODES` 已登记码，不新增）
//!
//! * 超过 1MiB / 非 OTLP JSON → `usage-source-io`（detail 前缀 `tps-ingest-*` 定位）；
//! * span 级残缺（缺 id/时刻）→ 跳过 + 诊断，不整批失败（单条坏 span 不拖垮整批）。

use serde_json::Value;

use crate::services::tps::install::enforce_body_limit;
use crate::services::tps::model::{Outcome, OutputScope, Purpose, TimingMethod, TpsObservation};
use crate::services::usage::error::UsageError;

/// 来源 id（与 usage 域工具 id 小写串同族）
pub const SOURCE_ID: &str = "claude";
/// 宿主实例标签：本机接收器每机一份（loopback only），无多实例歧义
pub const LOCAL_HOST_INSTANCE: &str = "local-otlp";
/// 官方 llm_request span 名（otel-boundaries §2；metrics 面无时延，traces 唯一同条）
const SPAN_NAME: &str = "claude_code.llm_request";

/// 解码一条 OTLP JSON traces 报文（`/v1/traces` 的 body）。
/// 返回全部可提取的观察（含失败/取消/重试等**不入速**的观察——耗时详情候选）。
pub fn decode_trace(body: &[u8]) -> Result<Vec<TpsObservation>, UsageError> {
    decode_trace_with_diagnostics(body).map(|(obs, _)| obs)
}

/// 同 [`decode_trace`]，另带本批诊断（缺失/跳过/排除原因；Task 7 接收器可透传）。
pub fn decode_trace_with_diagnostics(
    body: &[u8],
) -> Result<(Vec<TpsObservation>, Vec<String>), UsageError> {
    // 与接收器同一条 1MiB 红线（纵深防御：即使 Task 7 忘了闸，解码层也拒绝）
    enforce_body_limit(body.len())?;
    let root: Value = serde_json::from_slice(body).map_err(|e| {
        UsageError::new(
            "usage-source-io",
            format!("tps-ingest-body: not OTLP JSON: {e}"),
        )
    })?;

    let mut diags: Vec<String> = Vec::new();
    let mut out: Vec<TpsObservation> = Vec::new();
    let mut seen: std::collections::BTreeSet<(String, String)> = Default::default();
    let mut skipped_non_llm = 0usize;

    // OTLP JSON 拓扑：resourceSpans[] → scopeSpans[] → spans[]
    for rs in root
        .get("resourceSpans")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        for ss in rs
            .get("scopeSpans")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[])
        {
            for span in ss
                .get("spans")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or(&[])
            {
                let name = span.get("name").and_then(Value::as_str).unwrap_or_default();
                if name != SPAN_NAME {
                    // 工具/生命周期 span：独立面，不入模型窗也不产观察（§4 工具 span 独立）
                    skipped_non_llm += 1;
                    continue;
                }
                let Some(raw) = RawSpan::parse(span) else {
                    diags
                        .push("llm_request span 缺 traceId/spanId/时刻，跳过（不造稳定键）".into());
                    continue;
                };
                // 同批重复投递（exporter 重试）→ 幂等只留一条
                if !seen.insert((raw.trace_id.clone(), raw.span_id.clone())) {
                    diags.push(format!(
                        "duplicate span skipped (trace {} span {}) — redelivery is idempotent",
                        short_id(&raw.trace_id),
                        short_id(&raw.span_id),
                    ));
                    continue;
                }
                build_observation(raw, &mut out, &mut diags);
            }
        }
    }
    if skipped_non_llm > 0 {
        diags.push(format!(
            "skipped non-{SPAN_NAME} spans x{skipped_non_llm} (tool/lifecycle spans are separate, not in model generation window)"
        ));
    }
    Ok((out, diags))
}

// ==== span 属性抽取（OTLP JSON 值形态：stringValue/doubleValue/intValue/boolValue）====

/// OTLP JSON 的 int64 是**字符串编码**（{"intValue":"177"}）；double 是 JSON 数值。
/// 两种编码都收（不同 SDK 版本/字段可能取其一）。
fn attr_i64(span: &Value, key: &str) -> Option<i64> {
    let v = span
        .get("attributes")?
        .as_array()?
        .iter()
        .find(|a| a.get("key").and_then(Value::as_str) == Some(key))?;
    let v = v.get("value")?;
    if let Some(s) = v.get("intValue").and_then(Value::as_str) {
        return s.parse::<i64>().ok();
    }
    if let Some(n) = v.get("intValue").and_then(Value::as_i64) {
        return Some(n);
    }
    v.get("doubleValue")
        .and_then(Value::as_f64)
        .map(|f| f as i64)
}

fn attr_string(span: &Value, key: &str) -> Option<String> {
    span.get("attributes")?
        .as_array()?
        .iter()
        .find(|a| a.get("key").and_then(Value::as_str) == Some(key))?
        .get("value")?
        .get("stringValue")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn attr_bool(span: &Value, key: &str) -> Option<bool> {
    span.get("attributes")?
        .as_array()?
        .iter()
        .find(|a| a.get("key").and_then(Value::as_str) == Some(key))?
        .get("value")?
        .get("boolValue")
        .and_then(Value::as_bool)
}

/// 解码后的 span 原料（字段层，尚不做口径判定）
struct RawSpan {
    trace_id: String,
    span_id: String,
    start_ms: i64,
    end_ms: i64,
    model: Option<String>,
    session_id: Option<String>,
    query_source: Option<String>,
    attempt: Option<i64>,
    success: Option<bool>,
    duration_ms: Option<i64>,
    output_tokens: Option<i64>,
    first_content_ms: Option<i64>,
    stop_reason: Option<String>,
    status_error: bool,
    status_message: Option<String>,
}

impl RawSpan {
    fn parse(span: &Value) -> Option<Self> {
        let trace_id = span
            .get("traceId")
            .and_then(Value::as_str)?
            .to_ascii_lowercase();
        let span_id = span
            .get("spanId")
            .and_then(Value::as_str)?
            .to_ascii_lowercase();
        let start_ns: i64 = span
            .get("startTimeUnixNano")
            .and_then(Value::as_str)?
            .parse()
            .ok()?;
        let end_ns: i64 = span
            .get("endTimeUnixNano")
            .and_then(Value::as_str)?
            .parse()
            .ok()?;
        // OTLP status：code=1 / STATUS_CODE_ERROR = 错误（success 属性缺失时的兜底判定）
        let status = span.get("status");
        let status_error = match status.and_then(|s| s.get("code")) {
            Some(Value::String(s)) => s == "STATUS_CODE_ERROR",
            Some(Value::Number(n)) => n.as_i64() == Some(1),
            _ => false,
        };
        let status_message = status
            .and_then(|s| s.get("message"))
            .and_then(Value::as_str)
            .map(str::to_string);
        Some(Self {
            trace_id,
            span_id,
            start_ms: start_ns / 1_000_000,
            end_ms: end_ns / 1_000_000,
            model: attr_string(span, "model"),
            session_id: attr_string(span, "session.id"),
            query_source: attr_string(span, "query_source")
                .or_else(|| attr_string(span, "query_source_safe")),
            attempt: attr_i64(span, "attempt"),
            success: attr_bool(span, "success"),
            duration_ms: attr_i64(span, "duration_ms"),
            output_tokens: attr_i64(span, "output_tokens"),
            first_content_ms: attr_i64(span, "first_content_ms"),
            stop_reason: attr_string(span, "stop_reason"),
            status_error,
            status_message,
        })
    }
}

/// 诊断用的短 id（前 8 字符；hex id 恒 ASCII，用 get 防御非 hex 输入的切界）
fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// purpose 映射（Spec §3「辅助标题/自动摘要与主任务分别标 purpose」）：
/// query_source 值取自真机探针（generate_session_title / repl_main_thread / away_summary）；
/// 未识别值一律 MainTask（宁可入主任务不丢样本，不猜新辅助类目）。
fn purpose_of(query_source: Option<&str>) -> Purpose {
    match query_source {
        Some("generate_session_title") => Purpose::AssistTitle,
        Some("away_summary" | "conversation_summary") => Purpose::AutoSummary,
        _ => Purpose::MainTask,
    }
}

/// 失败/取消判定（Spec §3：失败/取消不入速度，有可靠起止仍进耗时详情）。
/// stop_reason 属中阈值明确的直觉中断词 → Cancelled，其余非成功 → Failed。
fn outcome_of(raw: &RawSpan) -> (Outcome, Option<String>) {
    let ok = raw.success.unwrap_or(!raw.status_error);
    if ok {
        return (Outcome::Success, None);
    }
    let reason = raw
        .stop_reason
        .clone()
        .or_else(|| raw.status_message.clone())
        .unwrap_or_else(|| "unknown".into());
    let cancelled = matches!(
        raw.stop_reason.as_deref(),
        Some("interrupted" | "aborted" | "cancelled" | "stopped" | "user_cancelled")
    );
    let outcome = if cancelled {
        Outcome::Cancelled
    } else {
        Outcome::Failed
    };
    (outcome, Some(reason))
}

fn build_observation(raw: RawSpan, out: &mut Vec<TpsObservation>, diags: &mut Vec<String>) {
    let call = format!("{}-{}", raw.trace_id, raw.span_id);
    // 起止锚定：优先原生 duration 反推起点（与 Task 4 Kimi「ended − native」同款，
    // 保证 started/ended/native 三者自洽）；缺/负 duration（残损 span）退回 span 起始时刻
    let ended = raw.end_ms;
    let native_duration = raw.duration_ms.filter(|d| *d > 0);
    let started = native_duration.map(|d| ended - d).unwrap_or(raw.start_ms);
    let (outcome, failure_reason) = outcome_of(&raw);

    // **成功证据闸**（Spec §4「success 及 output 齐全」才入速）：`success` 属性缺失
    // 不默认成功（字段可缺有实证）——观察保留（outcome 按现状判定），但生成窗/首点
    // 一律置空 + 诊断，绝不进速度。
    let proven_success = raw.success == Some(true);
    if raw.success.is_none() {
        diags.push(format!(
            "llm_request missing success attr (trace {}): success unproven, excluded from speed (generation window/first token left empty)",
            short_id(&raw.trace_id)
        ));
    }
    // **尝试闸**：attempt≠1 / attempt 缺失 → 排除速度（逻辑窗含重试，与成功尝试
    // 首点基线不同批，otel-boundaries §2）。
    let attempt_ok = raw.attempt == Some(1);
    match raw.attempt {
        Some(1) => {}
        Some(n) => diags.push(format!(
            "retry span attempt={n} (trace {}) excluded from speed: logical window contains retries, different batch from successful-attempt first token",
            short_id(&raw.trace_id)
        )),
        // attempt 可缺（字段补查实证过 1 条缺失）——无法证明 attempt=1 → 同排除
        None => diags.push(format!(
            "attempt missing (trace {}) excluded from speed: cannot prove a single successful attempt",
            short_id(&raw.trace_id)
        )),
    }
    let speed_eligible = proven_success && attempt_ok;

    // 生成窗 = duration − first_content（Spec §4）。缺 first_content → 不借回合值补齐；
    // 只有 speed_eligible 的样本才有生成窗/首点（成功证据闸 + 尝试闸双闸）。
    let generation = if speed_eligible {
        match (native_duration, raw.first_content_ms) {
            (Some(d), Some(fc)) if d - fc > 0 => Some(d - fc),
            _ => None,
        }
    } else {
        None
    };
    // 响应窗（V2 Spec §5.2「含等待响应」）：完整 span duration（含等待；工具执行在
    // llm_request span 之外，天然不在窗内）。与生成窗同一套闸：成功证据闸 + 尝试闸
    //（已识别重试不对齐）双闸通过且 duration 可靠才填实；缺 first_content 不影响
    // 响应窗（response-only 有效，正负/同基准一致性由 Task 1 谓词把关）。
    let response_elapsed_ms = if speed_eligible {
        native_duration
    } else {
        None
    };
    if raw.first_content_ms.is_none() {
        diags.push(format!(
            "llm_request missing first_content_ms (trace {}), generation window/first token left empty (no borrowing ttft/turn values)",
            short_id(&raw.trace_id)
        ));
    }
    // 首点 = first_content（内容块开始，native-event-approx；≠ 首非空 delta，
    // 也不采用 message_start 基线的原生 ttft_ms——它只作诊断对照，不落观察）
    let first_token_at = if speed_eligible {
        raw.first_content_ms
            .filter(|fc| *fc >= 0)
            .map(|fc| started + fc)
    } else {
        None
    };
    // 其余「齐全」缺项如实诊断（Spec §4：success 及 output 齐全；无 duration 则
    // 连整调用时长都没有）——绝不静默产非速观测
    if speed_eligible && native_duration.is_none() {
        diags.push(format!(
            "llm_request missing/non-positive duration_ms (trace {}): no reliable full-call duration, generation window left empty, excluded from speed",
            short_id(&raw.trace_id)
        ));
    }
    if speed_eligible && raw.output_tokens.is_none() {
        diags.push(format!(
            "llm_request missing output_tokens (trace {}): output incomplete, excluded from speed (kept missing, not reported as 0)",
            short_id(&raw.trace_id)
        ));
    }

    out.push(TpsObservation {
        source: SOURCE_ID.into(),
        host_instance: LOCAL_HOST_INSTANCE.into(),
        stable_call_key: call,
        started_at_ms: started,
        first_token_at_ms: first_token_at,
        ended_at_ms: ended,
        native_duration_ms: native_duration,
        generation_ms: generation,
        // 同一次输出两个窗（Spec §5.1）：生成窗 = duration − first_content（first_content
        // 仍是流式口径的块开始近似）；响应窗 = 完整 span duration（含等待；见上）。
        response_elapsed_ms,
        output_tokens: raw.output_tokens,
        output_scope: OutputScope::ProviderReported,
        // 工具 span 独立（见模块头）：v1 不产工具用时 series，tool_ms 恒空（如实申报）
        tool_ms: None,
        model: raw.model.unwrap_or_else(|| "unknown".into()),
        channel: None,
        session_key: raw.session_id,
        turn_key: Some(raw.trace_id),
        purpose: purpose_of(raw.query_source.as_deref()),
        outcome,
        method: TimingMethod::NativeEventApprox,
        failure_reason,
    });
}
