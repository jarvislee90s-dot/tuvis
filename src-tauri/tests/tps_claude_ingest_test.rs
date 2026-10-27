//! Claude 本机遥测接入与一次统计设置集成测试（2026-10-24 计划 Task 5）。
//!
//! 隔离纪律（2026-10-07 事故族，与 tps_native_primary/secondary_test.rs 同款）：
//! * decode 测试是**纯字节**测试（OTLP JSON 夹具取自真实脱敏探针的四 span 结构，
//!   证据：`docs/superpowers/evidence/2026-10-24-model-speed/otel-boundaries-addendum.md`
//!   及 supplement 探针 `observed/claude-otel.jsonl`（trace a22d4d3c…，四 span 合计
//!   dur=90136 / output=3709 / Σttft=2410 / Σfirst_content=8484））；
//! * install 测试一律走 `install::{preview_at,apply_at,uninstall_at}` 的**入参 home
//!   注入**形态，home = 用例私有 tempdir——绝不触真机 `~/.claude` / `~/.codex` /
//!   `~/.tuvis`；生产入口 `preview/apply/uninstall`（真实 home）在本文件**禁止调用**；
//! * 接收器守卫（超限/路径/Bearer/端口）是纯函数测试，**不 bind 任何 socket**。
//!
//! 覆盖（brief Step1 逐条）：
//! * Mac 四 span 原始 dur90136/out3709：原生 ttft 口径复算 42.2794 而非旧报告的 54.8
//!   （复核报告「Claude算术与首点」）；产品口径用 first_content = 45.4245 且标
//!   native-event-approx，两种基线不混（spec §4 Claude 行）；
//! * attempt>1 排除速度（`retried.tps() == None`）；
//! * 接收超限 / 无 token / 路径不符拒绝；
//! * install：preview 展示统计键与原值指纹（敏感 header 脱敏）；clean home 上
//!   append-only 落键；既有目标冲突默认保留；显式转交只替换授权键；卸载恢复旧值 /
//!   删除自有键 / 用户后改保留并报冲突。
//!
//! （brief Step3/Step4 追加）：缺 first_content 不借回合值补齐；工具 span 独立不进
//!   生成窗；首块开始不标严格非空 delta（方法标签锁定）；旧 trace 重复不增；原配置
//!   其它字段/未知项保留；预览过期 / 预览后变更拒绝应用。

use multi_agents_manager_lib::services::tps::install::{
    self, InstallChoice, DEFAULT_RECEIVER_PORT, FALLBACK_PORT_RANGE, INGEST_PATH, MAX_BODY_BYTES,
};
use multi_agents_manager_lib::services::tps::model::{
    Outcome, OutputScope, Purpose, SeriesKey, SourceCursor, SourceState, TimingMethod,
};
use multi_agents_manager_lib::services::tps::sources::claude;
use multi_agents_manager_lib::services::tps::{build_point, store, PointStatus};
use serde_json::{json, Value};
use std::path::PathBuf;
use tempfile::TempDir;

mod support;

/// 固定假时钟基准（纯整数；夹具事件时间锚在它附近，不读系统时钟）
const BASE_MS: i64 = 1_700_000_000_000;
const BASE_NS: i64 = BASE_MS * 1_000_000;
/// 采样时刻 = 基准 + 120s（四 span 的 end 全部落在 (NOW−120s, NOW] 完成窗内）
const NOW_MS: i64 = BASE_MS + 120_000;

// ==== 真实探针四 span 原始数值（supplement observed/claude-otel.jsonl trace a22d4d3c…）====
/// (duration_ms, output_tokens, ttft_ms, first_content_ms, model, query_source, start_offset_ms)
const FOUR_SPANS: [(i64, i64, i64, i64, &str, &str, i64); 4] = [
    (
        6777,
        177,
        1048,
        1049,
        "glm-5.3-flash",
        "generate_session_title",
        0,
    ),
    (
        70940,
        2992,
        508,
        4072,
        "glm-5.3-flash[1m]",
        "repl_main_thread",
        58,
    ),
    (
        3815,
        94,
        427,
        2202,
        "glm-5.3-flash[1m]",
        "repl_main_thread",
        71_330,
    ),
    (
        8604,
        446,
        427,
        1161,
        "glm-5.3-flash[1m]",
        "repl_main_thread",
        76_271,
    ),
];
/// 四 span 合计（复核报告的锚数：90,136 / 3,709 / 2,410 / 8,484）
const SUM_DUR: i64 = 90_136;
const SUM_OUT: i64 = 3_709;
const SUM_TTFT: i64 = 2_410;
const SUM_FC: i64 = 8_484;

const TRACE_ID: &str = "a22d4d3caabbccdd1122334455667788";
const SESSION_ID: &str = "sess-1";

// ==== OTLP JSON 夹具构造（wire 形态：attributes[] + stringValue/doubleValue/intValue/boolValue）====

fn attr_s(key: &str, v: &str) -> Value {
    json!({"key": key, "value": {"stringValue": v}})
}
fn attr_d(key: &str, v: f64) -> Value {
    json!({"key": key, "value": {"doubleValue": v}})
}
/// OTLP JSON 的 int64 按规范是**字符串编码**（{"intValue":"177"}）
fn attr_i(key: &str, v: &str) -> Value {
    json!({"key": key, "value": {"intValue": v}})
}
fn attr_b(key: &str, v: bool) -> Value {
    json!({"key": key, "value": {"boolValue": v}})
}

fn hex_id(prefix: u8, n: usize) -> String {
    // 定长 hex id（trace 32 / span 16），内容只需在夹具内稳定唯一
    let fill = format!("{prefix:02x}").repeat(n);
    fill
}

/// 一条 claude_code.llm_request span（offset_ms/end 时刻按原始探针锚定）。
/// `extra` 允许注入额外属性；`omit_fc` 模拟非流式回退缺 first_content_ms。
#[allow(clippy::too_many_arguments)]
fn llm_span(
    span_seq: usize,
    trace: &str,
    start_offset_ms: i64,
    dur_ms: i64,
    output: i64,
    ttft: i64,
    fc: Option<i64>,
    attempt: i64,
    success: bool,
    model: &str,
    query_source: &str,
    stop_reason: &str,
) -> Value {
    let start_ns = BASE_NS + start_offset_ms * 1_000_000;
    let end_ns = start_ns + dur_ms * 1_000_000;
    let mut attrs = vec![
        attr_s("session.id", SESSION_ID),
        attr_s("span.type", "llm_request"),
        attr_s("model", model),
        attr_s("gen_ai.request.model", model),
        attr_s("llm_request.context", "interaction"),
        attr_s("query_source", query_source),
        attr_d("input_tokens", 100.0),
        attr_i("output_tokens", &output.to_string()),
        attr_d("cache_read_tokens", 0.0),
        attr_d("cache_creation_tokens", 0.0),
        attr_b("success", success),
        attr_i("attempt", &attempt.to_string()),
        attr_d("ttft_ms", ttft as f64),
    ];
    if let Some(fc_ms) = fc {
        attrs.push(attr_d("first_content_ms", fc_ms as f64));
    }
    attrs.push(attr_s("stop_reason", stop_reason));
    // duration_ms 逐 span 轮换 doubleValue / 字符串 intValue 两种 OTLP 编码，锁解析容差
    if span_seq.is_multiple_of(2) {
        attrs.push(attr_d("duration_ms", dur_ms as f64));
    } else {
        attrs.push(attr_i("duration_ms", &dur_ms.to_string()));
    }
    json!({
        "traceId": trace,
        "spanId": hex_id((span_seq + 1) as u8, 16),
        "parentSpanId": hex_id(0xab, 16),
        "name": "claude_code.llm_request",
        "kind": "SPAN_KIND_INTERNAL",
        "startTimeUnixNano": start_ns.to_string(),
        "endTimeUnixNano": end_ns.to_string(),
        "attributes": attrs,
        "status": {"code": "STATUS_CODE_UNSET"},
    })
}

/// 工具 span（独立生命周期：不进模型生成窗，也不产模型观察）
fn tool_span(name: &str, start_offset_ms: i64, dur_ms: i64) -> Value {
    let start_ns = BASE_NS + start_offset_ms * 1_000_000;
    json!({
        "traceId": TRACE_ID,
        "spanId": hex_id(0xee, 16),
        "parentSpanId": hex_id(0xcd, 16),
        "name": name,
        "kind": "SPAN_KIND_INTERNAL",
        "startTimeUnixNano": start_ns.to_string(),
        "endTimeUnixNano": (start_ns + dur_ms * 1_000_000).to_string(),
        "attributes": [attr_d("duration_ms", dur_ms as f64), attr_s("decision", "allow")],
        "status": {"code": "STATUS_CODE_UNSET"},
    })
}

fn wrap_body(spans: Vec<Value>) -> Vec<u8> {
    json!({
        "resourceSpans": [{
            "resource": {"attributes": [
                attr_s("service.name", "claude-code"),
                attr_s("host.arch", "arm64"),
            ]},
            "scopeSpans": [{
                "scope": {"name": "com.anthropic.claude_code.tracing", "version": "2.1.293"},
                "spans": spans,
            }],
        }]
    })
    .to_string()
    .into_bytes()
}

/// 原始四 span 正例体（真实探针结构）
fn four_span_body() -> Vec<u8> {
    let spans: Vec<Value> = FOUR_SPANS
        .iter()
        .enumerate()
        .map(|(i, (dur, out, ttft, fc, model, src, start))| {
            llm_span(
                i,
                TRACE_ID,
                *start,
                *dur,
                *out,
                *ttft,
                Some(*fc),
                1,
                true,
                model,
                src,
                "end_turn",
            )
        })
        .collect();
    // span2 与 span3 之间夹一个真实存在的工具 span（独立，不进生成窗）
    let mut all = vec![spans[0].clone(), spans[1].clone()];
    all.push(tool_span("claude_code.tool.execution", 71_000, 300));
    all.extend_from_slice(&spans[2..]);
    wrap_body(all)
}

/// 单调用 TPS（观测级形态：可入速才得值，否则 None）
fn single_call_tps(
    o: &multi_agents_manager_lib::services::tps::model::TpsObservation,
) -> Option<f64> {
    if !o.is_tps_valid() {
        return None;
    }
    Some(1000.0 * o.output_tokens.unwrap() as f64 / o.generation_ms.unwrap() as f64)
}

fn assert_f64_near(actual: f64, expected: f64, what: &str) {
    assert!(
        (actual - expected).abs() < 1e-3,
        "{what}: 实际 {actual}，期望 {expected}（±1e-3）"
    );
}

// =====================================================================
// Step1 红测试组 A：decode_trace（纯字节）
// =====================================================================

#[test]
fn four_span_sample_recomputes_native_ttft_to_42_279_not_54_8() {
    let obs = claude::decode_trace(&four_span_body()).expect("正例体必须可解码");
    assert_eq!(
        obs.len(),
        4,
        "四条 llm_request 各成一条观察（工具 span 不产观察）"
    );
    assert_eq!(
        obs.iter().map(|o| o.output_tokens.unwrap()).sum::<i64>(),
        SUM_OUT,
        "输出合计必须逐字等于原始证据 3709"
    );
    assert_eq!(
        obs.iter()
            .map(|o| o.native_duration_ms.unwrap())
            .sum::<i64>(),
        SUM_DUR,
        "时长合计必须逐字等于原始证据 90136"
    );

    // —— 原生 ttft 口径复算（诊断面）：生成窗 = Σdur − Σttft = 87726 → 42.2794，
    //    不是旧报告的 54.8（70,136ms/3,711Token 的错值），也不是全时长口径 41.149
    //    （复核报告「Claude算术与首点」逐字锚数）。
    let ttft_window_tps = 1000.0 * SUM_OUT as f64 / (SUM_DUR - SUM_TTFT) as f64;
    assert_f64_near(ttft_window_tps, 42.2794, "原生 ttft 口径复算");
    assert!(
        (ttft_window_tps - 54.8).abs() > 1.0,
        "54.8 是旧报告错值，不得复现"
    );
    let full_duration_tps = 1000.0 * SUM_OUT as f64 / SUM_DUR as f64;
    assert!(
        (full_duration_tps - 41.149).abs() < 1e-3,
        "全时长口径 = 41.149（另一种错口径，须与 ttft 口径区分）"
    );

    // —— 产品口径（first_content 近似）：同一批四条观察按 duration−first_content
    //    聚合 = 45.4245（closeout 提示词第 4 条逐字锚数）。两口径并存且不相混。
    let product_tps = 1000.0 * SUM_OUT as f64 / (SUM_DUR - SUM_FC) as f64;
    assert_f64_near(product_tps, 45.4245, "产品 first_content 口径");
    let decoded_gen: i64 = obs.iter().map(|o| o.generation_ms.unwrap()).sum();
    assert_eq!(
        decoded_gen,
        SUM_DUR - SUM_FC,
        "解码观察的生成窗合计 = Σ(duration−first_content)"
    );
    assert_f64_near(
        1000.0 * SUM_OUT as f64 / decoded_gen as f64,
        45.4245,
        "按解码观察聚合的产品口径",
    );

    // 方法标签：native-event-approx（首块开始 ≠ 首非空 delta，不得称严格口径）
    assert!(
        obs.iter()
            .all(|o| o.method == TimingMethod::NativeEventApprox),
        "Claude 原生近似来源全部标 NativeEventApprox"
    );
    // 原生身份：turn = trace（interaction/trace 分回合），session 独立保存
    assert!(obs
        .iter()
        .all(|o| o.turn_key.as_deref() == Some(TRACE_ID.to_ascii_lowercase().as_str())));
    assert!(obs
        .iter()
        .all(|o| o.session_key.as_deref() == Some(SESSION_ID)));
    // source 与宿主实例
    assert!(obs.iter().all(|o| o.source == "claude"));
    // purpose 映射：标题请求 → AssistTitle，主线程 → MainTask
    let title = obs
        .iter()
        .find(|o| o.purpose == Purpose::AssistTitle)
        .expect("generate_session_title 必须映射为 AssistTitle");
    assert_eq!(title.model, "glm-5.3-flash");
    assert_eq!(
        obs.iter()
            .filter(|o| o.purpose == Purpose::MainTask)
            .count(),
        3,
        "repl_main_thread 三条为 MainTask"
    );
    // 首点基线 = first_content（started + first_content），不是原生 ttft（message_start）
    let span2 = obs
        .iter()
        .find(|o| o.native_duration_ms == Some(70_940))
        .expect("第二条（70940ms）必须存在");
    assert_eq!(
        span2.ttft_ms(),
        Some(4072),
        "观察级首点延迟取 first_content 口径（4072），不是原生 ttft_ms（508）"
    );
    // 输出语义：thinking 计数未单独声明 → ProviderReported（不宣称含/不含思考）
    assert_eq!(span2.output_scope, OutputScope::ProviderReported);
}

#[test]
fn main_task_positive_sample_flows_into_point() {
    let obs = claude::decode_trace(&four_span_body()).expect("正例体必须可解码");
    let mut known: Vec<SeriesKey> = Vec::new();
    for o in &obs {
        let k = SeriesKey {
            tool: o.source.clone(),
            model: o.model.clone(),
            channel: o.channel.clone(),
            method: o.method,
        };
        if !known.contains(&k) {
            known.push(k);
        }
    }
    let points = build_point(&obs, &known, "run-test", NOW_MS);
    // 原始模型串不归并：glm-5.3-flash 与 glm-5.3-flash[1m] 各成 series
    let main = points
        .iter()
        .find(|p| p.series_key.model == "glm-5.3-flash[1m]" && p.status == PointStatus::Value)
        .expect("主任务三条必须产出一个 Value 点");
    assert_eq!(main.sample_count, 3);
    assert_eq!(main.sum_output_tokens, 2992 + 94 + 446);
    assert_eq!(
        main.sum_generation_ms,
        (70_940 - 4072) + (3815 - 2202) + (8604 - 1161)
    );
    assert_f64_near(
        main.tps.expect("Value 点必有 TPS"),
        3532.0 * 1000.0 / 75_924.0,
        "主任务点 TPS",
    );
    assert_eq!(main.ttft_sample_count, 3, "三条成功样本首点全部可对齐");
    assert_eq!(main.method, TimingMethod::NativeEventApprox);
}

#[test]
fn attempt_gt_one_excluded_from_speed() {
    // 同一调用第二次尝试：success/output/first_content 齐全，但 attempt=2——
    // span duration 是含重试的逻辑请求窗，与成功尝试的首点基线不同批，不得入速
    let body = wrap_body(vec![llm_span(
        9,
        TRACE_ID,
        0,
        9000,
        100,
        300,
        Some(500),
        2,
        true,
        "glm-5.3-flash",
        "repl_main_thread",
        "end_turn",
    )]);
    let obs = claude::decode_trace(&body).expect("重试 span 仍应解码为观察（排除而非丢弃）");
    assert_eq!(obs.len(), 1);
    let retried = &obs[0];
    assert_eq!(
        single_call_tps(retried),
        None,
        "attempt>1 排除速度（brief 断言 `retried.tps()==None` 的观测级形态）"
    );
    assert_eq!(single_call_tps(retried), None);
    assert_eq!(retried.generation_ms, None, "不借逻辑窗凑生成窗");
    assert_eq!(
        retried.native_duration_ms,
        Some(9000),
        "原生逻辑窗保留作耗时详情/诊断"
    );
}

#[test]
fn missing_first_content_never_borrows_turn_value() {
    // 非流式回退：缺 first_content_ms——不得拿 ttft 或回合其它值补齐生成窗
    let body = wrap_body(vec![llm_span(
        8,
        TRACE_ID,
        0,
        5000,
        50,
        400,
        None,
        1,
        true,
        "glm-5.3-flash",
        "repl_main_thread",
        "end_turn",
    )]);
    let (obs, diags) = claude::decode_trace_with_diagnostics(&body).expect("解码成功");
    assert_eq!(obs.len(), 1);
    assert_eq!(
        obs[0].generation_ms, None,
        "缺 first_content → 无生成窗（不借 ttft 补）"
    );
    assert_eq!(obs[0].first_token_at_ms, None, "首点同样缺失，不造时刻");
    assert_eq!(single_call_tps(&obs[0]), None);
    assert!(
        diags.iter().any(|d| d.contains("first_content")),
        "缺 first_content 必须带诊断：{diags:?}"
    );
}

#[test]
fn failed_and_cancelled_spans_stay_out_of_speed() {
    let body = wrap_body(vec![
        llm_span(
            1,
            TRACE_ID,
            0,
            3000,
            0,
            100,
            Some(200),
            1,
            false,
            "glm-5.3-flash",
            "repl_main_thread",
            "api_error",
        ),
        llm_span(
            2,
            TRACE_ID,
            60_000,
            4000,
            30,
            100,
            Some(150),
            1,
            false,
            "glm-5.3-flash",
            "repl_main_thread",
            "interrupted",
        ),
    ]);
    let obs = claude::decode_trace(&body).expect("失败/取消 span 仍解码为观察（耗时详情候选）");
    assert_eq!(obs.len(), 2);
    let failed = obs
        .iter()
        .find(|o| o.outcome == Outcome::Failed)
        .expect("api_error → Failed");
    assert_eq!(failed.failure_reason.as_deref(), Some("api_error"));
    assert_eq!(single_call_tps(failed), None);
    let cancelled = obs
        .iter()
        .find(|o| o.outcome == Outcome::Cancelled)
        .expect("interrupted → Cancelled");
    assert_eq!(single_call_tps(cancelled), None);
    assert_eq!(
        cancelled.native_duration_ms,
        Some(4000),
        "有可靠起止仍进耗时详情"
    );
}

#[test]
fn tool_spans_stay_out_of_model_window() {
    let body = four_span_body(); // 已含 claude_code.tool.execution
    let (obs, diags) = claude::decode_trace_with_diagnostics(&body).expect("解码成功");
    assert_eq!(obs.len(), 4, "工具 span 不产模型观察");
    // 工具间隙不进生成窗：span3（3815ms，前面隔着工具 span）的生成窗仍是自己
    // 的 duration−first_content = 1613，而不是跨工具的跨度
    let span3 = obs
        .iter()
        .find(|o| o.native_duration_ms == Some(3815))
        .expect("span3 存在");
    assert_eq!(span3.generation_ms, Some(3815 - 2202));
    assert!(
        diags.iter().any(|d| d.contains("tool")),
        "工具 span 跳过必须留诊断（独立申报，不并入模型窗）：{diags:?}"
    );
}

#[test]
fn duplicate_spans_decode_once_and_reread_is_identical() {
    let body = four_span_body();
    let first = claude::decode_trace(&body).expect("首次解码");
    // 同批体内重复投递同一 span（exporter 重试的常见形态）→ 只解码一次
    let dup_body = wrap_body(vec![
        llm_span(
            0,
            TRACE_ID,
            0,
            6777,
            177,
            1048,
            Some(1049),
            1,
            true,
            "glm-5.3-flash",
            "generate_session_title",
            "end_turn",
        ),
        llm_span(
            0,
            TRACE_ID,
            0,
            6777,
            177,
            1048,
            Some(1049),
            1,
            true,
            "glm-5.3-flash",
            "generate_session_title",
            "end_turn",
        ),
    ]);
    let (dup, diags) = claude::decode_trace_with_diagnostics(&dup_body).expect("重复 span 解码");
    assert_eq!(dup.len(), 1, "同 trace+span 重复只留一条");
    assert!(
        diags.iter().any(|d| d.contains("duplicate")),
        "重复投递须有诊断"
    );
    // 旧 trace 重复解码（重投递）结果逐字一致（稳定键稳定 → 上游 UPSERT 不增）
    let second = claude::decode_trace(&body).expect("重复解码");
    assert_eq!(first, second, "同一体重复解码结果必须逐字一致");
}

#[test]
fn store_upsert_keeps_old_trace_from_duplicating() {
    // 用例私有库：同一条连接提交两批等价 ReadBatch（接收器重投递的等价形态），
    // 主键 source+host_instance+stable_call_key UPSERT → 旧行不翻倍
    let mut conn = support::open_ledger_db("tps-claude-ingest");
    let obs = claude::decode_trace(&four_span_body()).expect("解码");
    let batch = || multi_agents_manager_lib::services::tps::model::ReadBatch {
        observations: obs.clone(),
        source_state: SourceState {
            source: "claude".into(),
            cursor: SourceCursor::empty("claude"),
            ok: true,
            detail: None,
        },
        diagnostics: Vec::new(),
        next_cursor: SourceCursor::empty("claude"),
    };
    store::commit_batch(&mut conn, &batch()).expect("首次提交");
    store::commit_batch(&mut conn, &batch()).expect("重复提交（重投递）");
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM tps_observation", [], |r| r.get(0))
        .expect("计数");
    assert_eq!(
        n, 4,
        "旧 trace 重复不增（主键 source+host+call_key UPSERT）"
    );
}

#[test]
fn decode_rejects_malformed_and_oversize_bodies() {
    assert!(
        claude::decode_trace(b"not json{").is_err(),
        "非 OTLP JSON 拒绝"
    );
    assert!(
        claude::decode_trace(b"{}").is_ok(),
        "空报文 = 空观察批（非错误）"
    );
    let oversize = vec![b' '; MAX_BODY_BYTES + 1];
    let err = claude::decode_trace(&oversize).expect_err("超过 1MiB 必须拒绝");
    assert!(
        err.detail.contains("超限") || err.detail.contains("1MiB"),
        "{err:?}"
    );
}

// =====================================================================
// Step1 红测试组 B：接收器守卫（纯函数，不 bind socket）
// =====================================================================

#[test]
fn ingest_guards_reject_oversize_wrong_path_and_missing_token() {
    // 1MiB 上限：恰在界上通过，超 1 字节拒绝
    assert!(install::enforce_body_limit(MAX_BODY_BYTES).is_ok());
    let err = install::enforce_body_limit(MAX_BODY_BYTES + 1).expect_err("超限必须拒绝");
    assert!(err.detail.contains("1MiB"), "{err:?}");

    // 路径只认 /v1/traces（OTLP 仅 JSON traces，不开 metrics/logs）
    assert!(install::verify_ingest_path(INGEST_PATH).is_ok());
    assert!(install::verify_ingest_path("/v1/metrics").is_err());
    assert!(install::verify_ingest_path("/v1/logs").is_err());

    // Bearer：缺头 / 非 Bearer / 错凭证 → 拒绝；正确凭证 → 通过
    assert!(
        install::verify_bearer(None, "secret").is_err(),
        "无 token 拒绝"
    );
    assert!(install::verify_bearer(Some("Basic secret"), "secret").is_err());
    assert!(install::verify_bearer(Some("Bearer wrong"), "secret").is_err());
    assert!(install::verify_bearer(Some("Bearer secret"), "secret").is_ok());
}

#[test]
fn receiver_port_picking_is_deterministic_and_bounded() {
    assert_eq!(
        install::pick_receiver_port(&[]),
        Some(DEFAULT_RECEIVER_PORT),
        "默认固定端口 47181"
    );
    assert_eq!(
        install::pick_receiver_port(&[DEFAULT_RECEIVER_PORT]),
        Some(47182),
        "首次占用 → 从 47182 起顺延"
    );
    let mut occupied: Vec<u16> = (DEFAULT_RECEIVER_PORT..=47184).collect();
    assert_eq!(install::pick_receiver_port(&occupied), Some(47185));
    occupied.extend(FALLBACK_PORT_RANGE);
    assert_eq!(
        install::pick_receiver_port(&occupied),
        None,
        "全占用 → None（报状态，绝不擅自杀进程/乱换端口）"
    );
}

// =====================================================================
// Step1 红测试组 C：一次统计设置（全部走 *_at(home) 注入形态）
// =====================================================================

/// 用例私有 home（tempdir；设置文件在 <home>/.claude/settings.json，记录库在
/// <home>/.tuvis/tuvis.db——绝不触真机目录）
struct TestHome(TempDir);

impl TestHome {
    fn new() -> Self {
        Self(tempfile::tempdir().expect("tempdir"))
    }
    fn path(&self) -> PathBuf {
        self.0.path().to_path_buf()
    }
    fn settings_path(&self) -> PathBuf {
        self.path().join(".claude").join("settings.json")
    }
    fn settings(&self) -> Value {
        serde_json::from_str(
            &std::fs::read_to_string(self.settings_path()).expect("settings.json 应存在"),
        )
        .expect("settings.json 应是合法 JSON")
    }
    fn write_settings(&self, v: &Value) {
        std::fs::create_dir_all(self.settings_path().parent().unwrap()).unwrap();
        std::fs::write(
            self.settings_path(),
            serde_json::to_string_pretty(v).unwrap(),
        )
        .unwrap();
    }
}

/// 带既有网关/认证键与无关字段的用户设置（形态逐字对齐 supplement 探针
/// raw/claude/CHANGE-PREVIEW.md §1.2 列出的真实 env 键名——只列名不列真值）
fn seeded_settings() -> Value {
    json!({
        "autoSyncCompactRatio": 0.8,
        "contextWindows": {"max": 200000},
        "hooks": {"Stop": [{"matcher": "*", "hooks": []}]},
        "env": {
            "ANTHROPIC_AUTH_TOKEN": "sk-user-secret-do-not-touch",
            "ANTHROPIC_BASE_URL": "https://gateway.example.com",
            "ANTHROPIC_MODEL": "glm-5.3-flash",
            "CLAUDE_CODE_MAX_CONTEXT_TOKENS": "200000",
            "MY_UNRELATED_KEY": "keep-me"
        }
    })
}

/// 目标统计键集（与 install::STATISTICS_ENV_KEYS 对齐；证据：otel-boundaries-addendum
/// §2 官方键 + supplement 探针 CHANGE-PREVIEW §1.2 实测八键 + spec §5 Bearer 凭证头）
const EXPECTED_KEYS: [&str; 9] = [
    "CLAUDE_CODE_ENABLE_TELEMETRY",
    "CLAUDE_CODE_ENHANCED_TELEMETRY_BETA",
    "OTEL_TRACES_EXPORTER",
    "OTEL_METRICS_EXPORTER",
    "OTEL_LOGS_EXPORTER",
    "OTEL_EXPORTER_OTLP_PROTOCOL",
    "OTEL_EXPORTER_OTLP_ENDPOINT",
    "OTEL_EXPORTER_OTLP_HEADERS",
    "OTEL_TRACES_EXPORT_INTERVAL",
];

#[test]
fn preview_lists_statistics_keys_with_fingerprints_and_masks_sensitive_header() {
    let home = TestHome::new();
    home.write_settings(&seeded_settings());
    let before = std::fs::read_to_string(home.settings_path()).unwrap();

    let pv = install::preview_at(&home.path(), "claude").expect("preview 成功");
    assert!(!pv.preview_id.is_empty());
    assert!(
        pv.conflict_keys.is_empty(),
        "干净 home 无冲突：{:?}",
        pv.conflict_keys
    );
    let keys: Vec<&str> = pv.entries.iter().map(|e| e.key.as_str()).collect();
    assert_eq!(
        keys, EXPECTED_KEYS,
        "目标键集逐字固定（不发明 provider 新键）"
    );
    assert!(
        !keys.iter().any(|k| k.contains("ANTHROPIC")),
        "API/认证/provider/网关键绝不进安装集"
    );
    // 干净 home：全部 old_exists=false、无指纹
    assert!(pv
        .entries
        .iter()
        .all(|e| !e.old_exists && e.old_fingerprint.is_none()));
    // endpoint 指向本机固定端口
    let endpoint = pv
        .entries
        .iter()
        .find(|e| e.key == "OTEL_EXPORTER_OTLP_ENDPOINT")
        .unwrap();
    assert_eq!(
        endpoint.planned_masked,
        format!("http://127.0.0.1:{DEFAULT_RECEIVER_PORT}")
    );
    // 敏感 header：只给脱敏值 + 指纹，绝不出全文
    let headers = pv
        .entries
        .iter()
        .find(|e| e.key == "OTEL_EXPORTER_OTLP_HEADERS")
        .unwrap();
    assert!(
        !headers.planned_masked.contains("Bearer"),
        "脱敏值不得含凭证本体：{}",
        headers.planned_masked
    );
    assert!(
        headers.planned_masked.contains("sha256:"),
        "脱敏值须带指纹：{}",
        headers.planned_masked
    );
    // preview 是纯读：settings 文件未被改动
    assert_eq!(
        std::fs::read_to_string(home.settings_path()).unwrap(),
        before,
        "preview 不得写用户设置"
    );
}

#[test]
fn apply_append_only_on_clean_home_writes_only_own_env_keys() {
    let home = TestHome::new();
    home.write_settings(&seeded_settings());
    let pv = install::preview_at(&home.path(), "claude").expect("preview");

    let receipt = install::apply_at(&home.path(), &pv.preview_id, InstallChoice::AppendOnly)
        .expect("干净 home append-only 应用成功");
    assert_eq!(receipt.entries.len(), 9);
    assert!(
        receipt.entries.iter().all(|e| !e.old_exists),
        "干净 home 全部为新增键"
    );
    assert_eq!(receipt.tool, "claude");

    let settings = home.settings();
    let env = settings["env"].as_object().expect("env 仍是对象");
    for k in EXPECTED_KEYS {
        assert!(env.contains_key(k), "缺少安装键 {k}");
    }
    // API/认证/网关/未知键逐字保留；顶层其它字段保留
    assert_eq!(
        env["ANTHROPIC_AUTH_TOKEN"], "sk-user-secret-do-not-touch",
        "认证键不碰"
    );
    assert_eq!(
        env["ANTHROPIC_BASE_URL"], "https://gateway.example.com",
        "网关不碰"
    );
    assert_eq!(env["MY_UNRELATED_KEY"], "keep-me", "未知 env 键保留");
    assert_eq!(settings["autoSyncCompactRatio"], 0.8, "顶层其它字段保留");
    assert!(settings["hooks"].is_object(), "hooks 保留");
    // endpoint/协议与凭证头形态
    assert_eq!(
        env["OTEL_EXPORTER_OTLP_ENDPOINT"],
        format!("http://127.0.0.1:{DEFAULT_RECEIVER_PORT}")
    );
    assert_eq!(env["OTEL_EXPORTER_OTLP_PROTOCOL"], "http/json");
    let headers = env["OTEL_EXPORTER_OTLP_HEADERS"].as_str().unwrap();
    assert!(headers.starts_with("Authorization=Bearer "), "{headers}");

    // 安装记录（逐键 oldExists/oldValue/installedValue/指纹）与接收器配置落 tps_state
    let conn = rusqlite::Connection::open(home.path().join(".tuvis").join("tuvis.db")).unwrap();
    let record = store::load_state(&conn, &install::record_state_key("claude"))
        .expect("记录可读")
        .expect("安装记录已保存");
    assert_eq!(record["entries"].as_array().unwrap().len(), 9);
    let rc = install::load_receiver_config_at(&home.path(), "claude")
        .expect("读取接收器配置")
        .expect("接收器配置已持久化");
    assert_eq!(rc.port, DEFAULT_RECEIVER_PORT);
    assert_eq!(rc.credential.len(), 64, "独立随机凭证（64 hex）");
    // 凭证与安装头一致（同批安装）
    assert!(headers.ends_with(&rc.credential));
    // 凭证不进日志面：Debug 形态已脱敏
    assert!(
        !format!("{receipt:?}").contains(&rc.credential),
        "InstallReceipt 的 Debug 必须脱敏凭证"
    );
    // 设置文件**已存在**（本用例自己播种，0644）→ 写后保持原权限位（不放宽也不收紧）
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(home.settings_path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o644,
            "已存在设置文件保持原权限位（实际 {mode:o}）"
        );
    }
    // 新建 settings.json Unix 0600（用户私有）——单独用例见 apply_creates_missing_settings_private
}

#[test]
fn apply_on_preexisting_target_defaults_to_keep_not_overwrite() {
    let home = TestHome::new();
    let mut seeded = seeded_settings();
    seeded["env"]["OTEL_EXPORTER_OTLP_ENDPOINT"] = json!("http://localhost:4318");
    home.write_settings(&seeded);

    let pv = install::preview_at(&home.path(), "claude").expect("preview");
    assert!(
        pv.conflict_keys
            .contains(&"OTEL_EXPORTER_OTLP_ENDPOINT".to_string()),
        "既有相同信号目标必须显示冲突：{:?}",
        pv.conflict_keys
    );
    let endpoint_entry = pv
        .entries
        .iter()
        .find(|e| e.key == "OTEL_EXPORTER_OTLP_ENDPOINT")
        .unwrap();
    assert!(endpoint_entry.old_exists);
    assert_eq!(
        endpoint_entry.old_masked.as_deref(),
        Some("http://localhost:4318")
    );
    assert!(endpoint_entry.old_fingerprint.is_some(), "原值指纹必须展示");
    assert!(endpoint_entry.conflict);

    // 默认 choice=append-only：冲突 → 拒绝且**不落盘**（默认保留）
    let err = install::apply_at(&home.path(), &pv.preview_id, InstallChoice::AppendOnly)
        .expect_err("存在冲突时 append-only 必须拒绝");
    assert!(err.detail.contains("conflict"), "{err:?}");
    assert_eq!(
        home.settings()["env"]["OTEL_EXPORTER_OTLP_ENDPOINT"],
        "http://localhost:4318",
        "默认保留既有目标，不覆盖"
    );
}

#[test]
fn apply_transfer_replaces_only_authorized_statistics_key() {
    let home = TestHome::new();
    let mut seeded = seeded_settings();
    seeded["env"]["OTEL_EXPORTER_OTLP_ENDPOINT"] = json!("http://localhost:4318");
    home.write_settings(&seeded);
    let pv = install::preview_at(&home.path(), "claude").expect("preview");

    let receipt = install::apply_at(
        &home.path(),
        &pv.preview_id,
        InstallChoice::TransferStatisticsTarget,
    )
    .expect("显式转交应用成功");
    let env = home.settings()["env"].as_object().unwrap().clone();
    // 只有授权的冲突键被替换，其余原值键逐字保留
    assert_eq!(
        env["OTEL_EXPORTER_OTLP_ENDPOINT"],
        format!("http://127.0.0.1:{DEFAULT_RECEIVER_PORT}")
    );
    assert_eq!(env["ANTHROPIC_BASE_URL"], "https://gateway.example.com");
    // 逐键安装记录：转交替换键保存原值与指纹
    let endpoint = receipt
        .entries
        .iter()
        .find(|e| e.key == "OTEL_EXPORTER_OTLP_ENDPOINT")
        .expect("转交替换键必须有记录");
    assert!(endpoint.old_exists);
    assert_eq!(
        endpoint.old_value.as_ref().and_then(|v| v.as_str()),
        Some("http://localhost:4318"),
        "转交替换键记录保存原样 JSON 旧值"
    );
    assert!(endpoint.old_fingerprint.is_some());
}

#[test]
fn apply_rejects_when_managed_value_changed_after_preview() {
    let home = TestHome::new();
    home.write_settings(&seeded_settings());
    let pv = install::preview_at(&home.path(), "claude").expect("preview");
    // 预览之后用户手改了一个统计键
    let mut edited = home.settings();
    edited["env"]["OTEL_TRACES_EXPORTER"] = json!("console");
    home.write_settings(&edited);

    let err = install::apply_at(&home.path(), &pv.preview_id, InstallChoice::AppendOnly)
        .expect_err("预览后再变更必须拒绝（指纹不符）");
    assert!(err.detail.contains("changed after preview"), "{err:?}");
    assert_eq!(
        home.settings()["env"]["OTEL_TRACES_EXPORTER"],
        "console",
        "拒绝 = 不落盘，用户改动原样保留"
    );
}

#[test]
fn apply_rejects_expired_preview() {
    let home = TestHome::new();
    home.write_settings(&seeded_settings());
    let pv = install::preview_at(&home.path(), "claude").expect("preview");
    // 直接把 tps_state 里的预览记录改成已过期（不读系统时钟的过期注入）
    let conn = rusqlite::Connection::open(home.path().join(".tuvis").join("tuvis.db")).unwrap();
    let mut record = store::load_state(&conn, &install::preview_state_key(&pv.preview_id))
        .unwrap()
        .expect("预览记录存在");
    record["expiresAtMs"] = json!(1);
    store::save_state(&conn, &install::preview_state_key(&pv.preview_id), &record).unwrap();
    let err = install::apply_at(&home.path(), &pv.preview_id, InstallChoice::AppendOnly)
        .expect_err("过期预览必须拒绝");
    assert!(err.detail.contains("expired"), "{err:?}");
}

#[test]
fn uninstall_restores_transferred_old_value_and_deletes_own_keys() {
    // —— 场景 1：转交替换 → 卸载恢复旧统计值
    let home = TestHome::new();
    let mut seeded = seeded_settings();
    seeded["env"]["OTEL_EXPORTER_OTLP_ENDPOINT"] = json!("http://localhost:4318");
    home.write_settings(&seeded);
    let pv = install::preview_at(&home.path(), "claude").unwrap();
    install::apply_at(
        &home.path(),
        &pv.preview_id,
        InstallChoice::TransferStatisticsTarget,
    )
    .expect("转交应用");
    install::uninstall_at(&home.path(), "claude").expect("卸载成功");
    let env = home.settings()["env"].as_object().unwrap().clone();
    assert_eq!(
        env["OTEL_EXPORTER_OTLP_ENDPOINT"], "http://localhost:4318",
        "转交替换键卸载时恢复旧统计值"
    );
    for k in EXPECTED_KEYS {
        if k == "OTEL_EXPORTER_OTLP_ENDPOINT" {
            continue;
        }
        assert!(!env.contains_key(k), "原先不存在的自有键 {k} 卸载时删除");
    }
    assert_eq!(env["ANTHROPIC_AUTH_TOKEN"], "sk-user-secret-do-not-touch");
    assert_eq!(env["MY_UNRELATED_KEY"], "keep-me");

    // —— 场景 2：干净 home append-only → 卸载全部删除自有键
    let home2 = TestHome::new();
    home2.write_settings(&seeded_settings());
    let pv2 = install::preview_at(&home2.path(), "claude").unwrap();
    install::apply_at(&home2.path(), &pv2.preview_id, InstallChoice::AppendOnly).unwrap();
    install::uninstall_at(&home2.path(), "claude").expect("卸载成功");
    let env2 = home2.settings()["env"].as_object().unwrap().clone();
    for k in EXPECTED_KEYS {
        assert!(!env2.contains_key(k), "自有键 {k} 已删除");
    }
    assert_eq!(
        env2["ANTHROPIC_BASE_URL"], "https://gateway.example.com",
        "网关不动"
    );
    assert!(home2.settings()["hooks"].is_object(), "顶层字段保留");
    // 从未安装过的 home：卸载幂等成功
    let home3 = TestHome::new();
    install::uninstall_at(&home3.path(), "claude").expect("未安装 = 幂等成功");
}

#[test]
fn uninstall_preserves_user_modified_keys_and_reports_conflict() {
    let home = TestHome::new();
    home.write_settings(&seeded_settings());
    let pv = install::preview_at(&home.path(), "claude").unwrap();
    install::apply_at(&home.path(), &pv.preview_id, InstallChoice::AppendOnly).expect("应用");
    // 安装后用户改了其中一个键
    let mut edited = home.settings();
    edited["env"]["OTEL_TRACES_EXPORTER"] = json!("console");
    home.write_settings(&edited);

    let err =
        install::uninstall_at(&home.path(), "claude").expect_err("有用户后改键时卸载必须报冲突");
    assert!(
        err.detail.contains("OTEL_TRACES_EXPORTER"),
        "冲突详情点名保留键：{err:?}"
    );
    let env = home.settings()["env"].as_object().unwrap().clone();
    assert_eq!(
        env["OTEL_TRACES_EXPORTER"], "console",
        "用户后改项保留，不覆盖"
    );
    assert!(
        !env.contains_key("CLAUDE_CODE_ENABLE_TELEMETRY"),
        "仍匹配安装值的自有键照常删除"
    );
    // 冲突未清空 → 安装记录保留（后续用户还原后还能再卸载）
    let conn = rusqlite::Connection::open(home.path().join(".tuvis").join("tuvis.db")).unwrap();
    assert!(
        store::load_state(&conn, &install::record_state_key("claude"))
            .unwrap()
            .is_some(),
        "存在保留冲突时记录不删（剩余自有键仍可追溯）"
    );
}

#[test]
fn uninstall_retry_recognizes_already_restored_keys_and_converges() {
    // M4（三轮复审）：部分卸载的已处理条目（已恢复的转交键）不得在下次重试时被
    // 误判为用户改动——否则重试永不收敛、安装记录永久滞留（UI 恒显 installed=true）。
    let home = TestHome::new();
    let mut seeded = seeded_settings();
    // 两个转交替换键（原值 ≠ 安装值）：A = endpoint、C = protocol
    seeded["env"]["OTEL_EXPORTER_OTLP_ENDPOINT"] = json!("http://localhost:4318");
    seeded["env"]["OTEL_EXPORTER_OTLP_PROTOCOL"] = json!("grpc");
    home.write_settings(&seeded);
    let pv = install::preview_at(&home.path(), "claude").unwrap();
    install::apply_at(
        &home.path(),
        &pv.preview_id,
        InstallChoice::TransferStatisticsTarget,
    )
    .expect("转交应用");
    // 用户改 A
    let mut edited = home.settings();
    edited["env"]["OTEL_EXPORTER_OTLP_ENDPOINT"] = json!("http://localhost:9999");
    home.write_settings(&edited);

    // 第 1 次卸载：A 冲突保留；C 恢复旧值；新增键删除；记录保留
    let err1 = install::uninstall_at(&home.path(), "claude").expect_err("A 被用户改动 → 冲突保留");
    assert!(
        err1.detail.contains("OTEL_EXPORTER_OTLP_ENDPOINT"),
        "冲突详情点名保留键: {err1:?}"
    );
    assert!(
        !err1.detail.contains("OTEL_EXPORTER_OTLP_PROTOCOL"),
        "已恢复键不得计入第 1 次冲突: {err1:?}"
    );
    let env1 = home.settings()["env"].as_object().unwrap().clone();
    assert_eq!(env1["OTEL_EXPORTER_OTLP_PROTOCOL"], "grpc", "C 已恢复旧值");
    assert!(
        !env1.contains_key("CLAUDE_CODE_ENABLE_TELEMETRY"),
        "仍匹配安装值的新增键已删除"
    );

    // 第 2 次卸载：仍 err（A 未解决），但 C（current == 记录的 old_value）不得再被
    // 误判为用户改动 ← RED
    let err2 = install::uninstall_at(&home.path(), "claude").expect_err("A 冲突仍在");
    assert!(
        err2.detail.contains("OTEL_EXPORTER_OTLP_ENDPOINT"),
        "真正的用户后改键照常点名: {err2:?}"
    );
    assert!(
        !err2.detail.contains("OTEL_EXPORTER_OTLP_PROTOCOL"),
        "已恢复键（值等于记录的原值）必须视为已完成，不得误判为用户改动 → 重试永不收敛: {err2:?}"
    );
    assert_eq!(
        home.settings()["env"]["OTEL_EXPORTER_OTLP_PROTOCOL"],
        "grpc",
        "已恢复键不被重复处理"
    );

    // 用户把 A 恢复为安装值
    let mut resolved = home.settings();
    resolved["env"]["OTEL_EXPORTER_OTLP_ENDPOINT"] =
        json!(format!("http://127.0.0.1:{DEFAULT_RECEIVER_PORT}"));
    home.write_settings(&resolved);

    // 第 3 次卸载：幂等收敛——记录清除、A 恢复原目标、B 保持删除、C 保持恢复
    install::uninstall_at(&home.path(), "claude").expect("冲突解决后重试必须成功（卸载闭环）");
    let conn = rusqlite::Connection::open(home.path().join(".tuvis").join("tuvis.db")).unwrap();
    assert!(
        store::load_state(&conn, &install::record_state_key("claude"))
            .unwrap()
            .map(|v| v.is_null())
            .unwrap_or(true),
        "retained 为空 → 安装记录清除（UI 可重新安装）"
    );
    let env3 = home.settings()["env"].as_object().unwrap().clone();
    assert_eq!(
        env3["OTEL_EXPORTER_OTLP_ENDPOINT"], "http://localhost:4318",
        "A 恢复原目标"
    );
    assert_eq!(env3["OTEL_EXPORTER_OTLP_PROTOCOL"], "grpc", "C 保持恢复");
    assert!(
        !env3.contains_key("CLAUDE_CODE_ENABLE_TELEMETRY"),
        "新增键保持删除"
    );
}

#[test]
fn uninstall_converges_for_non_string_restored_values() {
    // M4 rider：先前卸载合法恢复的**非字符串**旧值（数字 1000）同样收敛——已恢复判定
    // 必须取原始 JSON Value 逐字比对（数字 1000 不得与串 "1000" 混淆，与预览/apply 的
    // 形状纪律一致），经字符串提取的比对会让非字符串恢复值永不收敛。
    let home = TestHome::new();
    let mut seeded = seeded_settings();
    // 非字符串旧值（数字 1000）= 形状冲突 → transfer 显式替换
    seeded["env"]["OTEL_TRACES_EXPORT_INTERVAL"] = json!(1000);
    home.write_settings(&seeded);
    let pv = install::preview_at(&home.path(), "claude").unwrap();
    let entry = pv
        .entries
        .iter()
        .find(|e| e.key == "OTEL_TRACES_EXPORT_INTERVAL")
        .expect("目标键在预览集内");
    assert!(
        entry.conflict && entry.old_fingerprint.is_some(),
        "非字符串旧值恒为冲突（形状判定）: {entry:?}"
    );
    install::apply_at(
        &home.path(),
        &pv.preview_id,
        InstallChoice::TransferStatisticsTarget,
    )
    .expect("转交应用");
    assert_eq!(
        home.settings()["env"]["OTEL_TRACES_EXPORT_INTERVAL"],
        "1000",
        "安装值为计划字符串"
    );
    // 用户把该键恢复为**原数字值**（精确类型 1000，非串 "1000"）
    let mut restored = home.settings();
    restored["env"]["OTEL_TRACES_EXPORT_INTERVAL"] = json!(1000);
    home.write_settings(&restored);

    // 卸载：该键 = 已恢复（Value 逐字相等）→ 视为已完成；其余新增键照常删除；
    // retained 为空 → 记录清除、一次收敛
    install::uninstall_at(&home.path(), "claude")
        .expect("非字符串已恢复值必须收敛，不得误判为用户改动报冲突");
    let conn = rusqlite::Connection::open(home.path().join(".tuvis").join("tuvis.db")).unwrap();
    assert!(
        store::load_state(&conn, &install::record_state_key("claude"))
            .unwrap()
            .map(|v| v.is_null())
            .unwrap_or(true),
        "retained 为空 → 安装记录清除"
    );
    assert_eq!(
        home.settings()["env"]["OTEL_TRACES_EXPORT_INTERVAL"],
        json!(1000),
        "恢复值保持原类型（不重复恢复也不删除）"
    );
    // 幂等：再次卸载仍成功
    install::uninstall_at(&home.path(), "claude").expect("再次卸载幂等成功");
}

#[test]
fn never_installed_home_uninstall_is_idempotent_and_tool_is_validated() {
    let home = TestHome::new();
    assert!(
        install::preview_at(&home.path(), "claude").is_ok(),
        "无设置文件也能 preview（全新增态）"
    );
    assert!(
        install::preview_at(&home.path(), "not-a-tool").is_err(),
        "未知工具拒绝"
    );
}

#[test]
fn apply_creates_missing_settings_with_private_mode() {
    // 全新增态：无 settings 文件 → apply 新建，Unix 0600 用户私有（Spec §5）
    let home = TestHome::new();
    let pv = install::preview_at(&home.path(), "claude").expect("无设置文件也能 preview");
    assert!(pv
        .entries
        .iter()
        .all(|e| !e.old_exists && e.old_fingerprint.is_none()));
    install::apply_at(&home.path(), &pv.preview_id, InstallChoice::AppendOnly)
        .expect("全新增态应用成功");
    let env = home.settings()["env"]
        .as_object()
        .expect("env 对象已建")
        .clone();
    assert_eq!(env.len(), 9, "全新文件只含自有统计键：{env:?}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(home.settings_path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "新建 settings.json 必须 0600（实际 {mode:o}）"
        );
    }
}

#[test]
fn uninstall_after_env_removal_adds_nothing_back() {
    // 用户安装后把 env 段整个删了：卸载必须幂等成功，且**不得**给用户文件补回空 env
    //（写路径的 env 补建语义不属于卸载路径）
    let home = TestHome::new();
    home.write_settings(&seeded_settings());
    let pv = install::preview_at(&home.path(), "claude").unwrap();
    install::apply_at(&home.path(), &pv.preview_id, InstallChoice::AppendOnly).expect("应用");
    let mut edited = home.settings();
    edited.as_object_mut().unwrap().remove("env");
    home.write_settings(&edited);

    install::uninstall_at(&home.path(), "claude").expect("env 已不在 = 无可恢复面，幂等成功");
    let settings = home.settings();
    assert!(
        settings.get("env").is_none(),
        "卸载不得给用户文件补回空 env：{settings:?}"
    );
    // 记录已了结：再次卸载仍成功
    install::uninstall_at(&home.path(), "claude").expect("再次卸载幂等");
}

// =====================================================================
// 评审修复验证组（Important 1–4；其余 Minor 由控制方顺延，不在本轮）
// =====================================================================

/// 剥掉 span 的某个属性（构造「字段可缺」夹具用）
fn strip_attr(mut span: Value, key: &str) -> Value {
    if let Some(attrs) = span.get_mut("attributes").and_then(Value::as_array_mut) {
        attrs.retain(|a| a.get("key").and_then(Value::as_str) != Some(key));
    }
    span
}

#[test]
fn non_string_existing_values_are_conflicts_never_absent() {
    // 评审 Important 1：键存在但非字符串（数字/布尔）= 「已有相同信号目标」——
    // preview 报冲突；append-only 拒绝且文件**字节不变**；显式转交才替换；
    // 卸载按记录原样恢复类型（数字/布尔，不是字符串化）
    let home = TestHome::new();
    let mut seeded = seeded_settings();
    seeded["env"]["OTEL_EXPORTER_OTLP_ENDPOINT"] = json!(1);
    seeded["env"]["OTEL_LOGS_EXPORTER"] = json!(true);
    home.write_settings(&seeded);
    let before = std::fs::read_to_string(home.settings_path()).unwrap();

    let pv = install::preview_at(&home.path(), "claude").expect("preview");
    for k in ["OTEL_EXPORTER_OTLP_ENDPOINT", "OTEL_LOGS_EXPORTER"] {
        let e = pv.entries.iter().find(|e| e.key == k).unwrap();
        assert!(
            e.old_exists,
            "非字符串旧值 {k} 也是「已有目标」，不算不存在"
        );
        assert!(e.conflict, "非字符串旧值 {k} 必须报冲突");
        assert!(
            e.old_masked
                .as_deref()
                .unwrap_or_default()
                .contains("non-string value"),
            "{k} 脱敏面须标非字符串：{:?}",
            e.old_masked
        );
        assert!(
            pv.conflict_keys.iter().any(|c| c == k),
            "{:?}",
            pv.conflict_keys
        );
    }
    // append-only：拒绝且一个字节都不动（默认保留不覆盖）
    let err = install::apply_at(&home.path(), &pv.preview_id, InstallChoice::AppendOnly)
        .expect_err("非字符串目标同属冲突，append-only 必须拒绝");
    assert!(err.detail.contains("conflict"), "{err:?}");
    assert_eq!(
        std::fs::read_to_string(home.settings_path()).unwrap(),
        before,
        "拒绝 = 不落盘（字节不变）"
    );
    // 显式转交：替换为字符串安装值；回执逐键保存原样 JSON 旧值
    let receipt = install::apply_at(
        &home.path(),
        &pv.preview_id,
        InstallChoice::TransferStatisticsTarget,
    )
    .expect("显式转交替换冲突键");
    let env = home.settings()["env"].as_object().unwrap().clone();
    assert_eq!(
        env["OTEL_EXPORTER_OTLP_ENDPOINT"],
        format!("http://127.0.0.1:{DEFAULT_RECEIVER_PORT}")
    );
    assert_eq!(env["OTEL_LOGS_EXPORTER"], "none");
    let ep = receipt
        .entries
        .iter()
        .find(|e| e.key == "OTEL_EXPORTER_OTLP_ENDPOINT")
        .unwrap();
    assert_eq!(
        ep.old_value.as_ref().unwrap(),
        &json!(1),
        "旧值原样（数字类型在记录里）"
    );
    let lg = receipt
        .entries
        .iter()
        .find(|e| e.key == "OTEL_LOGS_EXPORTER")
        .unwrap();
    assert_eq!(
        lg.old_value.as_ref().unwrap(),
        &json!(true),
        "旧值原样（布尔类型在记录里）"
    );
    // 卸载：原样恢复类型
    install::uninstall_at(&home.path(), "claude").expect("卸载");
    let env = home.settings()["env"].as_object().unwrap().clone();
    assert_eq!(env["OTEL_EXPORTER_OTLP_ENDPOINT"], json!(1), "恢复数字类型");
    assert_eq!(env["OTEL_LOGS_EXPORTER"], json!(true), "恢复布尔类型");
}

#[test]
fn atomic_write_leaves_no_tmp_residue() {
    // 评审 Important 2：私有原子写（临时文件按目标 mode 创建）——落盘后目录无 .tmp
    // 残留；写入期间死亡也只可能残留 0600/原 mode 的 settings.tmp，绝不世界可读
    let home = TestHome::new();
    home.write_settings(&seeded_settings());
    let pv = install::preview_at(&home.path(), "claude").unwrap();
    install::apply_at(&home.path(), &pv.preview_id, InstallChoice::AppendOnly).expect("应用");
    install::uninstall_at(&home.path(), "claude").expect("卸载");
    let leftover: Vec<String> = std::fs::read_dir(home.settings_path().parent().unwrap())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert!(
        leftover.iter().all(|n| !n.ends_with(".tmp")),
        "落盘后不得残留临时文件：{leftover:?}"
    );
}

#[test]
fn repeat_previews_keep_a_single_pending_credential() {
    // 评审 Important 3b：每工具至多 1 份待应用预览/凭证；新预览即焚被替换的旧预览
    //（tps_state 无删除通道，否则废弃预览连同内嵌 bearer 凭证永久持久化）
    let home = TestHome::new();
    home.write_settings(&seeded_settings());
    let pv1 = install::preview_at(&home.path(), "claude").unwrap();
    let pv2 = install::preview_at(&home.path(), "claude").unwrap();
    assert_ne!(pv1.preview_id, pv2.preview_id);
    let conn = rusqlite::Connection::open(home.path().join(".tuvis").join("tuvis.db")).unwrap();
    let n: i64 = conn
        .query_row(
            // 即焚 = 值改写为显式 null（dao 无删除通道），活预览只数非 null 行
            "SELECT COUNT(*) FROM tps_state WHERE key LIKE 'install:preview:%' AND value_json != 'null'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1, "待应用预览至多一份（旧预览连同其内嵌凭证即焚）");
    let pending = store::load_state(&conn, &install::pending_state_key("claude"))
        .unwrap()
        .expect("待应用槽已登记");
    assert_eq!(
        pending.as_str(),
        Some(pv2.preview_id.as_str()),
        "槽指向最新预览"
    );
    // 被替换的旧预览不可再应用（拿着废弃凭证落盘 = 凭证堆积，正是要堵的洞）
    let err = install::apply_at(&home.path(), &pv1.preview_id, InstallChoice::AppendOnly)
        .expect_err("被替换的旧预览不可应用");
    assert!(err.detail.contains("not found"), "{err:?}");
}

#[test]
fn missing_success_attribute_is_not_speed_eligible() {
    // 评审升级（Spec §4「success 及 output 齐全」）：缺 success 属性**不默认成功**——
    // 观察保留（现状 outcome 照旧），生成窗/首点置空 + 诊断，绝不入速
    let span = strip_attr(
        llm_span(
            7,
            TRACE_ID,
            0,
            6000,
            60,
            300,
            Some(500),
            1,
            true,
            "glm-5.3-flash",
            "repl_main_thread",
            "end_turn",
        ),
        "success",
    );
    let (obs, diags) = claude::decode_trace_with_diagnostics(&wrap_body(vec![span]))
        .expect("缺 success 的 span 仍解码为观察");
    assert_eq!(obs.len(), 1);
    assert_eq!(
        obs[0].outcome,
        Outcome::Success,
        "status 未报错 → 现状判定不变"
    );
    assert_eq!(single_call_tps(&obs[0]), None, "缺 success → 不入速");
    assert_eq!(obs[0].generation_ms, None, "生成窗置空");
    assert_eq!(
        obs[0].first_token_at_ms, None,
        "TTFT 分母同样排除（成功未证明）"
    );
    assert!(
        obs[0].native_duration_ms.is_some(),
        "原生时长保留作耗时详情"
    );
    assert!(diags.iter().any(|d| d.contains("success")), "{diags:?}");
}

#[test]
fn missing_duration_or_output_on_success_span_get_diagnostics() {
    // 评审升级：成功 span 缺 duration_ms / output_tokens 附诊断，不静默产非速观测
    let no_dur = strip_attr(
        llm_span(
            1,
            TRACE_ID,
            0,
            5000,
            50,
            300,
            Some(400),
            1,
            true,
            "glm-5.3-flash",
            "repl_main_thread",
            "end_turn",
        ),
        "duration_ms",
    );
    let no_out = strip_attr(
        llm_span(
            2,
            TRACE_ID,
            60_000,
            5000,
            50,
            300,
            Some(400),
            1,
            true,
            "glm-5.3-flash",
            "repl_main_thread",
            "end_turn",
        ),
        "output_tokens",
    );
    let (obs, diags) =
        claude::decode_trace_with_diagnostics(&wrap_body(vec![no_dur, no_out])).expect("解码成功");
    assert_eq!(obs.len(), 2);
    assert!(
        obs.iter().all(|o| single_call_tps(o).is_none()),
        "两 span 均不入速"
    );
    assert_eq!(
        obs[0].native_duration_ms, None,
        "缺 duration → 无整调用时长"
    );
    assert!(
        diags.iter().any(|d| d.contains("duration_ms")),
        "缺 duration_ms 须有诊断：{diags:?}"
    );
    assert!(
        diags.iter().any(|d| d.contains("output_tokens")),
        "缺 output_tokens 须有诊断：{diags:?}"
    );
}

#[test]
fn canonical_equal_non_string_still_conflicts() {
    // 评审 Round 2：规范化串**碰巧等于**计划串的非字符串旧值（数字 1 vs "1"）
    // 同样是形状冲突——preview 报冲突；append-only 拒绝且字节不变；转交替换为
    // 字符串 "1" 且回执存原样 JSON 数字；卸载恢复为数字 1
    let home = TestHome::new();
    let mut seeded = seeded_settings();
    seeded["env"]["CLAUDE_CODE_ENABLE_TELEMETRY"] = json!(1);
    home.write_settings(&seeded);
    let before = std::fs::read_to_string(home.settings_path()).unwrap();

    let pv = install::preview_at(&home.path(), "claude").expect("preview");
    let e = pv
        .entries
        .iter()
        .find(|e| e.key == "CLAUDE_CODE_ENABLE_TELEMETRY")
        .unwrap();
    assert!(e.old_exists);
    assert!(
        e.conflict,
        "数字 1 ≠ 字符串 \"1\"（形状不等即冲突）：{:?}",
        e.old_masked
    );
    assert!(pv
        .conflict_keys
        .contains(&"CLAUDE_CODE_ENABLE_TELEMETRY".to_string()));

    // append-only：拒绝且不落盘（无形状比较时这里会「成功」但保留数字值）
    let err = install::apply_at(&home.path(), &pv.preview_id, InstallChoice::AppendOnly)
        .expect_err("形状冲突必须拒绝 append-only");
    assert!(err.detail.contains("conflict"), "{err:?}");
    assert_eq!(
        std::fs::read_to_string(home.settings_path()).unwrap(),
        before,
        "拒绝 = 字节不变"
    );

    // 显式转交：文件为字符串 "1"，回执 old_value 原样保存 JSON 数字 1
    let receipt = install::apply_at(
        &home.path(),
        &pv.preview_id,
        InstallChoice::TransferStatisticsTarget,
    )
    .expect("显式转交");
    let env = home.settings()["env"].as_object().unwrap().clone();
    assert_eq!(env["CLAUDE_CODE_ENABLE_TELEMETRY"], "1", "文件里是字符串");
    let e = receipt
        .entries
        .iter()
        .find(|e| e.key == "CLAUDE_CODE_ENABLE_TELEMETRY")
        .unwrap();
    assert_eq!(
        e.old_value.as_ref().unwrap(),
        &json!(1),
        "回执存原样 JSON 数字"
    );
    assert_eq!(e.installed_value, "1");

    // 卸载：恢复为数字 1（类型精确还原）
    install::uninstall_at(&home.path(), "claude").expect("卸载");
    let env = home.settings()["env"].as_object().unwrap().clone();
    assert_eq!(
        env["CLAUDE_CODE_ENABLE_TELEMETRY"],
        json!(1),
        "恢复数字类型"
    );
    assert!(
        !env.contains_key("OTEL_TRACES_EXPORTER"),
        "其余自有键照常删除"
    );
}

#[test]
fn reinstall_preserves_first_takeover_ownership_for_uninstall() {
    // R2（独立复核 P1）：重复安装/升级不得用自有安装值重置所有权记录——
    // 所有权事实 = 首次接管前的 oldExists/oldValue；重装时当前值若等于我们自己的
    // 安装值，必须沿用首次回执，否则卸载恢复的是我们的安装值而非用户原目标。

    // —— ① 首次 transfer 替换用户原 OTLP 目标 → 幂等重装 → 卸载：恢复用户原目标
    let home = TestHome::new();
    let mut seeded = seeded_settings();
    seeded["env"]["OTEL_EXPORTER_OTLP_ENDPOINT"] = json!("http://localhost:4318");
    home.write_settings(&seeded);
    let pv = install::preview_at(&home.path(), "claude").unwrap();
    install::apply_at(
        &home.path(),
        &pv.preview_id,
        InstallChoice::TransferStatisticsTarget,
    )
    .expect("首次转交");
    let pv2 = install::preview_at(&home.path(), "claude").unwrap();
    assert!(
        pv2.conflict_keys.is_empty(),
        "幂等重装预览不应报冲突（当前全是自有安装值）: {:?}",
        pv2.conflict_keys
    );
    let receipt2 = install::apply_at(&home.path(), &pv2.preview_id, InstallChoice::AppendOnly)
        .expect("幂等重装");
    let endpoint = receipt2
        .entries
        .iter()
        .find(|e| e.key == "OTEL_EXPORTER_OTLP_ENDPOINT")
        .unwrap();
    assert!(
        endpoint.old_exists,
        "重装回执必须保留首次接管事实：原目标存在"
    );
    assert_eq!(
        endpoint.old_value,
        Some(json!("http://localhost:4318")),
        "重装回执 old_value 仍是用户原目标，不是我们的安装值"
    );
    install::uninstall_at(&home.path(), "claude").expect("卸载");
    let env = home.settings()["env"].as_object().unwrap().clone();
    assert_eq!(
        env["OTEL_EXPORTER_OTLP_ENDPOINT"], "http://localhost:4318",
        "两次安装后卸载仍逐键恢复用户原目标"
    );
    for k in EXPECTED_KEYS {
        if k == "OTEL_EXPORTER_OTLP_ENDPOINT" {
            continue;
        }
        assert!(
            !env.contains_key(k),
            "首次新增键 {k} 重装后卸载仍删除，不被当成原有键保留"
        );
    }
    assert_eq!(env["ANTHROPIC_AUTH_TOKEN"], "sk-user-secret-do-not-touch");

    // —— ② 干净 home：新增 → 重装 → 卸载：自有键全部删除，回执不得把安装值记成原有值
    let home2 = TestHome::new();
    home2.write_settings(&seeded_settings());
    let p1 = install::preview_at(&home2.path(), "claude").unwrap();
    install::apply_at(&home2.path(), &p1.preview_id, InstallChoice::AppendOnly).unwrap();
    let p2 = install::preview_at(&home2.path(), "claude").unwrap();
    let r2 = install::apply_at(&home2.path(), &p2.preview_id, InstallChoice::AppendOnly).unwrap();
    assert!(
        r2.entries
            .iter()
            .all(|e| !e.old_exists && e.old_value.is_none()),
        "重装回执不得把自有安装值记成原有值: {:?}",
        r2.entries
            .iter()
            .map(|e| (e.key.clone(), e.old_exists))
            .collect::<Vec<_>>()
    );
    install::uninstall_at(&home2.path(), "claude").unwrap();
    let env2 = home2.settings()["env"].as_object().unwrap().clone();
    for k in EXPECTED_KEYS {
        assert!(!env2.contains_key(k), "自有键 {k} 已删除");
    }

    // —— ④ 重装后用户后改仍保留并报冲突（既有语义不回退）
    let home3 = TestHome::new();
    home3.write_settings(&seeded_settings());
    let q1 = install::preview_at(&home3.path(), "claude").unwrap();
    install::apply_at(&home3.path(), &q1.preview_id, InstallChoice::AppendOnly).unwrap();
    let q2 = install::preview_at(&home3.path(), "claude").unwrap();
    install::apply_at(&home3.path(), &q2.preview_id, InstallChoice::AppendOnly).unwrap();
    let mut edited = home3.settings();
    edited["env"]["OTEL_TRACES_EXPORTER"] = json!("console");
    home3.write_settings(&edited);
    install::uninstall_at(&home3.path(), "claude").expect_err("用户后改键卸载必须报冲突");
    assert_eq!(
        home3.settings()["env"]["OTEL_TRACES_EXPORTER"],
        "console",
        "用户后改值保留"
    );
}
