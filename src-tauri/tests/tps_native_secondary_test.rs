//! TPS 原生次源（Kimi / WorkBuddy）读取与 Codex 缺口态集成测试（2026-10-24 计划 Task 4）。
//!
//! 隔离纪律（2026-10-07 事故族，与 tps_native_primary_test.rs 同款）：全部夹具是 tempdir
//! 下的**合成**结构，形态逐字取自真实脱敏证据——Kimi 取
//! `docs/superpowers/evidence/2026-10-24-model-speed/dsh-kimi.md`（wire.jsonl 嵌套
//! `context.append_loop_event.event` step.end 自带 usage + 两个原生时长；llm.request
//! 带 provider/model/turnStep）；WorkBuddy 取 probes/2026-10-24-model-speed-supplement-*
//! 的 `qua.end` 单行字段（Mac 5.8.3 正常路线）与 Windows sdk 会话日志的
//! `usage_update {"hasUsage":true}`（无数值）——绝不触真机 `~/.kimi` / `~/.kimi-code` /
//! `~/.workbuddy` / `~/.codex` / `~/.tuvis`：`read_incremental(home, …)` 以**入参 home
//! 为唯一根**，测试传 tempdir 即天然隔离。
//!
//! 覆盖（brief Step1 逐条）：
//! * Kimi 嵌套 step 事件 timing+usage 同条对齐，806ms 正窗有效（无全局 1s 下限）
//! * WorkBuddy qua.end 同请求 completion/(total−ttft) 成功入速；标题 purpose 不入主曲线
//! * Windows 只有 hasUsage=true 不计（该路线 insufficient-evidence 缺口态）
//! * Codex turn−TTFT 不产 TPS（observations 为空 + insufficient-generation-timing）
//!
//! （brief Step3/Step4 追加）：取消/无 usage/失败跳过速度、provider-reported 思考留
//!   元数据不做内容估算、SDK 内部重试标来源限制、同键多 llm.request 剔除速度、
//!   残缺尾行重读不丢、字段缺失诊断不报 0。

use multi_agents_manager_lib::services::tps::model::{
    Outcome, OutputScope, Purpose, SeriesKey, SourceCursor, TimingMethod, TpsObservation,
};
use multi_agents_manager_lib::services::tps::sources::{codex, kimi, workbuddy};
use multi_agents_manager_lib::services::tps::{build_point, PointStatus};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// 固定假时钟基准（纯整数，不读系统时钟；夹具事件时间都锚在它附近）
const BASE_MS: i64 = 1_700_000_000_000;
/// 测试注入的「现在」：事件 + 2 小时（< 24h 保留窗）
const NOW_MS: i64 = BASE_MS + 2 * 60 * 60 * 1000;

// ==== 通用断言助手 ====

/// 单调用 TPS（brief 断言 `x.tps()` 的观测级形态：可入速才得值，否则 None）
fn single_call_tps(o: &TpsObservation) -> Option<f64> {
    if !o.is_tps_valid() {
        return None;
    }
    Some(1000.0 * o.output_tokens.unwrap() as f64 / o.generation_ms.unwrap() as f64)
}

// ==== Kimi 夹具（形态 = dsh-kimi.md 的脱敏结构）====

/// `<home>/.kimi-code/`：session_index.jsonl + `sessions/wd_p/<sid>/agents/main/wire.jsonl`
/// （session_index/sessionDir 形态与 services/usage/collectors/kimi.rs 的真机夹具一致）
fn kimi_wire_path(home: &Path, sid: &str) -> PathBuf {
    let agent_dir = home
        .join(".kimi-code")
        .join("sessions")
        .join("wd_p")
        .join(sid)
        .join("agents")
        .join("main");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(
        home.join(".kimi-code").join("session_index.jsonl"),
        format!(
            "{}\n",
            json!({"sessionId": sid, "sessionDir": format!("sessions/wd_p/{sid}"),
                   "workDir": "/p/kimi-proj"})
        ),
    )
    .unwrap();
    agent_dir.join("wire.jsonl")
}

/// 嵌套循环事件行：行级 `time` 在外层，事件在 `event` 下（dsh-kimi.md / 真机形态）
fn kimi_loop_event(time: i64, event: Value) -> String {
    json!({"type": "context.append_loop_event", "time": time, "event": event}).to_string()
}

/// `llm.request`：模型归属来源——**真机形态为顶层行**（dsh-kimi.md「顶层 llm.request 共
/// 811 条，全部有 provider、model、turnStep」；嵌套在循环事件内的数量为零），provider /
/// model / turnStep 与行级 `time` 直接在行上（独立复核 R5：此前夹具把它包进
/// `context.append_loop_event`，掩盖了读取器丢弃全部真实请求的缺陷）
fn kimi_llm_request(time: i64, turn_step: &str, model: &str) -> String {
    json!({"type": "llm.request", "time": time, "provider": "moonshot", "model": model,
           "turnId": turn_step.split('.').next().unwrap(), "turnStep": turn_step})
    .to_string()
}

/// `step.end`：同事件自带 usage 与两个原生时长（dsh-kimi.md「关键证据」）
#[allow(clippy::too_many_arguments)]
fn kimi_step_end(
    time: i64,
    uuid: &str,
    turn: &str,
    step: i64,
    finish: &str,
    output: Option<i64>,
    ttft_ms: Option<i64>,
    stream_ms: Option<i64>,
) -> String {
    let mut event = json!({"type": "step.end", "uuid": uuid, "turnId": turn, "step": step,
                           "finishReason": finish});
    if let Some(out) = output {
        event["usage"] = json!({"inputOther": 10, "output": out,
                                "inputCacheRead": 0, "inputCacheCreation": 0});
    }
    if let Some(t) = ttft_ms {
        event["llmFirstTokenLatencyMs"] = json!(t);
    }
    if let Some(s) = stream_ms {
        event["llmStreamDurationMs"] = json!(s);
    }
    kimi_loop_event(time, event)
}

// ==== WorkBuddy 夹具（形态 = supplement-macos §4.4/§11.2 qua.end + supplement-windows §7.5）====

/// `<home>/.workbuddy/logs/<date>/` 日志根（cli_host 与 sdk/conversations 同根）
fn wb_logs_dir(home: &Path) -> PathBuf {
    let dir = home.join(".workbuddy").join("logs").join("2026-10-24");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn wb_sdk_log_path(home: &Path, name: &str) -> PathBuf {
    let dir = wb_logs_dir(home).join("sdk").join("conversations");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

/// epoch ms → 夹具日志行的 RFC3339 毫秒时刻（与解析器 round-trip）
fn rfc3339_ms(ms: i64) -> String {
    let dt = chrono::DateTime::from_timestamp_millis(ms).unwrap();
    dt.format("%Y-%m-%dT%H:%M:%S%.3f%:z").to_string()
}

/// 真机日志框架的行前缀时刻（独立复核 R4）：`~/.workbuddy/logs/<date>/*.log` 每行形态为
/// `[9/3/2026, 10:03:57 PM.945] [Info] [pid=…] [Tag] …`——`M/D/YYYY, h:mm:ss AM|PM.mmm`，
/// 12 小时制、毫秒缀在 AM/PM 之后、**无时区偏移（本地时间）**。按本地时区格式化。
fn wb_real_prefix(ts_ms: i64) -> String {
    use chrono::TimeZone;
    let dt = chrono::Local.timestamp_millis_opt(ts_ms).single().unwrap();
    format!("[{}]", dt.format("%-m/%-d/%Y, %-I:%M:%S %p.%3f"))
}

/// 一条 qua.end 日志行（Mac 5.8.3 正常路线：单请求单行、同一身份、全量载荷）。
/// 行前缀 = 真机日志框架形态（见 `wb_real_prefix`），载荷字段名逐字取自
/// supplement-macos §4.4 的字段清单。
fn wb_qua_end_line(ts_ms: i64, fields: Value) -> String {
    format!(
        "{} [Info] [pid=1] [SseMonitor] agent.sse.qua.end {}",
        wb_real_prefix(ts_ms),
        fields
    )
}

/// RFC3339 前缀变体（兼容锁：若日志框架改为 ISO 时刻仍可解析）
fn wb_qua_end_line_rfc3339(ts_ms: i64, fields: Value) -> String {
    format!(
        "[{}] [Info] [pid=1] [SseMonitor] agent.sse.qua.end {}",
        rfc3339_ms(ts_ms),
        fields
    )
}

/// 同请求完整 qua.end 身份（conversation_id / conversation_request_id / message_id /
/// model 四元组 + request_type；数值与 supplement-macos §11.2 样本 2 对齐）
fn wb_qua_identity(msg: &str, creq: &str, request_type: &str) -> Value {
    json!({
        "conversation_id": "conv-1",
        "conversation_request_id": creq,
        "prompt_request_id": format!("preq-{msg}"),
        "trace_id": "trace-1",
        "message_id": msg,
        "model": "hy3",
        "agent_name": "main",
        "scene": "chat",
        "request_type": request_type
    })
}

// ==== Codex 夹具（形态 = file-sources.md §5 的脱敏结构）====

fn codex_rollout_path(home: &Path, name: &str) -> PathBuf {
    let dir = home
        .join(".codex")
        .join("sessions")
        .join("2026")
        .join("10")
        .join("24");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

/// 一条 rollout 行（`{timestamp, type, payload}` 信封）
fn codex_line(ts_ms: i64, kind: &str, payload: Value) -> String {
    json!({"timestamp": rfc3339_ms(ts_ms), "type": kind, "payload": payload}).to_string()
}

// ===========================================================================
// Kimi 用例
// ===========================================================================

/// 嵌套 step 事件 timing+usage 同条对齐；806ms 正窗有效（无全局 1s 下限，Spec §3）；
/// provider-reported 思考未声明 → 留元数据（ProviderReported），不做内容估算
#[test]
fn kimi_nested_step_timing_usage_same_record_subsecond_valid() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let wire = kimi_wire_path(home, "sess-1");
    let t_end = BASE_MS + 10_000;
    let lines = [
        // 新形态：同文件前序 llm.request 提供模型归属（turnStep = turnId.step）
        kimi_llm_request(t_end - 5_000, "0.1", "kimi-k2.6"),
        kimi_step_end(
            t_end,
            "u-1",
            "0",
            1,
            "end_turn",
            Some(120),
            Some(519),
            Some(806),
        ),
    ];
    std::fs::write(&wire, lines.join("\n") + "\n").unwrap();

    let batch = kimi::read_incremental(home, &SourceCursor::empty("kimi"), NOW_MS).unwrap();
    assert_eq!(batch.observations.len(), 1, "恰一条 step.end 观察");
    let kimi_subsecond = &batch.observations[0];

    // brief 断言：806ms 原生正窗有效（不套 1s 阈值）
    assert!(single_call_tps(kimi_subsecond).is_some());
    let tps = single_call_tps(kimi_subsecond).unwrap();
    assert!(
        (tps - 1000.0 * 120.0 / 806.0).abs() < 1e-9,
        "1000×120/806 ≈ 148.9，实得 {tps}"
    );

    // 同条对齐：usage 与两个原生时长同事件，无需相邻 usage.record 猜配
    assert_eq!(kimi_subsecond.output_tokens, Some(120));
    assert_eq!(kimi_subsecond.generation_ms, Some(806));
    // 原生时长锚定：started = 事件时刻 − (ttft+stream)；首 token = 结束 − stream
    assert_eq!(kimi_subsecond.ended_at_ms, t_end);
    assert_eq!(kimi_subsecond.native_duration_ms, Some(519 + 806));
    assert_eq!(kimi_subsecond.started_at_ms, t_end - 519 - 806);
    assert_eq!(kimi_subsecond.first_token_at_ms, Some(t_end - 806));
    assert_eq!(kimi_subsecond.ttft_ms(), Some(519));
    // provider-reported：kimi usage 无独立 reasoning 字段 → 思考计数未单独声明，
    // 留元数据，不按 think 文本另加估算
    assert_eq!(kimi_subsecond.output_scope, OutputScope::ProviderReported);
    assert_eq!(kimi_subsecond.outcome, Outcome::Success);
    assert_eq!(kimi_subsecond.method, TimingMethod::ClientNative);
    assert_eq!(
        kimi_subsecond.model, "kimi-k2.6",
        "模型来自同文件同 turnStep 的 llm.request"
    );
    assert_eq!(kimi_subsecond.turn_key.as_deref(), Some("0"), "原生 turnId");
    assert_eq!(kimi_subsecond.session_key.as_deref(), Some("sess-1"));
    assert_eq!(kimi_subsecond.purpose, Purpose::MainTask);
    assert_eq!(kimi_subsecond.stable_call_key, "sess-1:main:u-1");

    // SDK 内部重试不可识别 → 来源限制明示（不谎称零重试）
    let detail = batch.source_state.detail.clone().unwrap_or_default();
    assert!(
        detail.contains("重试"),
        "来源限制必须声明内部重试：{detail}"
    );
}

/// 已知失败与无 usage 跳过速度：finishReason=error → Failed；缺 usage → 记缺失不报 0；
/// streamDuration=0（整块突发/非流式）→ 无生成窗
#[test]
fn kimi_error_and_missing_usage_skip_speed() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let wire = kimi_wire_path(home, "sess-1");
    let t = BASE_MS + 10_000;
    let lines = [
        kimi_llm_request(t - 4_000, "0.1", "kimi-k2.6"),
        kimi_llm_request(t - 3_000, "0.2", "kimi-k2.6"),
        kimi_llm_request(t - 2_000, "0.3", "kimi-k2.6"),
        // e1：finishReason=error（失败不入速度，有可靠起止保留耗时事实）
        kimi_step_end(t, "u-err", "0", 1, "error", Some(12), Some(300), Some(200)),
        // e2：usage 缺席（2391/2402 之外的 11 个）→ output=None，不报 0
        kimi_step_end(
            t + 1_000,
            "u-nousage",
            "0",
            2,
            "end_turn",
            None,
            Some(100),
            Some(400),
        ),
        // e3：streamDuration=0 → 无生成窗，不入速
        kimi_step_end(
            t + 2_000,
            "u-zero",
            "0",
            3,
            "end_turn",
            Some(9),
            Some(50),
            Some(0),
        ),
    ];
    std::fs::write(&wire, lines.join("\n") + "\n").unwrap();

    let batch = kimi::read_incremental(home, &SourceCursor::empty("kimi"), NOW_MS).unwrap();
    assert_eq!(batch.observations.len(), 3, "三条 step.end 各一条观察");

    let err = batch
        .observations
        .iter()
        .find(|o| o.stable_call_key.ends_with("u-err"))
        .unwrap();
    assert_eq!(err.outcome, Outcome::Failed);
    assert_eq!(err.generation_ms, None, "失败不入速度");
    assert!(err.failure_reason.is_some());
    assert!(!err.is_tps_valid());

    let nousage = batch
        .observations
        .iter()
        .find(|o| o.stable_call_key.ends_with("u-nousage"))
        .unwrap();
    assert_eq!(nousage.output_tokens, None, "缺 usage 是缺失，不得伪装成 0");
    assert!(!nousage.is_tps_valid());

    let zero = batch
        .observations
        .iter()
        .find(|o| o.stable_call_key.ends_with("u-zero"))
        .unwrap();
    assert_eq!(zero.generation_ms, None, "0ms 流窗不入速度");
    assert!(!zero.is_tps_valid());

    assert!(batch.diagnostics.iter().any(|d| d.contains("usage")));
    assert!(batch
        .diagnostics
        .iter()
        .any(|d| d.contains("llmStreamDurationMs") || d.contains("0")));
}

/// 同 turnStep 多条 llm.request（重试）→ 无法对齐哪次请求产出了该 step 的 usage →
/// 剔除速度 + 模型进未知桶（addendum §7.1「保留全部 attempt，禁止 map 覆盖」）
#[test]
fn kimi_retry_ambiguous_requests_strip_speed() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let wire = kimi_wire_path(home, "sess-1");
    let t = BASE_MS + 10_000;
    let lines = [
        // 同 turnStep 两条 request（重试，保留全部、不覆盖）
        kimi_llm_request(t - 4_000, "1.1", "kimi-k2.6"),
        kimi_llm_request(t - 2_000, "1.1", "kimi-k2.6"),
        kimi_step_end(
            t,
            "u-retry",
            "1",
            1,
            "end_turn",
            Some(64),
            Some(200),
            Some(600),
        ),
    ];
    std::fs::write(&wire, lines.join("\n") + "\n").unwrap();

    let batch = kimi::read_incremental(home, &SourceCursor::empty("kimi"), NOW_MS).unwrap();
    assert_eq!(batch.observations.len(), 1);
    let o = &batch.observations[0];
    assert_eq!(o.generation_ms, None, "重试不可对齐 → 剔除速度");
    assert_eq!(o.first_token_at_ms, None, "起点不可对齐 → TTFT 不可用");
    assert_eq!(o.native_duration_ms, None);
    assert_eq!(o.output_tokens, Some(64), "输出事实保留（不造 0）");
    assert!(!o.is_tps_valid());
    assert!(
        batch.diagnostics.iter().any(|d| d.contains("retries")),
        "剔除原因必须进诊断：{:?}",
        batch.diagnostics
    );
}

/// 旧形态（无 llm.request）：模型进显式未知桶，但 usage+timing 同条对齐仍可入速
/// （dsh-kimi.md「模型缺失样本可进入工具级总览，但模型级应进未知桶」）
#[test]
fn kimi_old_shape_without_request_enters_unknown_model_bucket() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let wire = kimi_wire_path(home, "sess-1");
    let t = BASE_MS + 10_000;
    let lines = [kimi_step_end(
        t,
        "u-old",
        "2",
        1,
        "end_turn",
        Some(80),
        Some(400),
        Some(1_000),
    )];
    std::fs::write(&wire, lines.join("\n") + "\n").unwrap();

    let batch = kimi::read_incremental(home, &SourceCursor::empty("kimi"), NOW_MS).unwrap();
    assert_eq!(batch.observations.len(), 1);
    let o = &batch.observations[0];
    assert_eq!(o.model, "unknown", "无请求证据 → 显式未知桶");
    assert_eq!(
        o.generation_ms,
        Some(1_000),
        "timing/usage 同条对齐 → 速度仍可入"
    );
    assert!(o.is_tps_valid());
}

/// 残缺尾行重读不丢：撕裂尾行不推进水位；补全后恰读一次
#[test]
fn kimi_torn_tail_line_reread_not_lost() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let wire = kimi_wire_path(home, "sess-1");
    let t = BASE_MS + 10_000;
    let complete = [
        kimi_llm_request(t - 1_000, "0.1", "kimi-k2.6"),
        kimi_step_end(t, "u-1", "0", 1, "end_turn", Some(10), Some(50), Some(120)),
    ];
    // 撕裂尾行：第二条 step.end 写了一半、无换行（EOF 打断）
    let torn = kimi_step_end(
        t + 2_000,
        "u-2",
        "0",
        2,
        "end_turn",
        Some(8),
        Some(40),
        Some(90),
    );
    let torn_half = &torn[..torn.len() / 2];
    std::fs::write(&wire, complete.join("\n") + "\n" + torn_half).unwrap();

    let batch1 = kimi::read_incremental(home, &SourceCursor::empty("kimi"), NOW_MS).unwrap();
    assert_eq!(batch1.observations.len(), 1, "只有完整行出观察");
    let cursor1 = batch1.next_cursor.clone();

    // 撕裂行未完成：重读零观察、水位不越过
    let batch_torn = kimi::read_incremental(home, &cursor1, NOW_MS).unwrap();
    assert_eq!(batch_torn.observations.len(), 0, "残行不出观察");
    assert_eq!(
        batch_torn.next_cursor.value, cursor1.value,
        "残行不推进水位"
    );

    // 补全残行（只补缺失的后半段）→ 恰读一次，不丢不重
    let mut body = std::fs::read_to_string(&wire).unwrap();
    body.push_str(&torn[torn.len() / 2..]);
    body.push('\n');
    std::fs::write(&wire, body).unwrap();
    let batch2 = kimi::read_incremental(home, &batch_torn.next_cursor, NOW_MS).unwrap();
    assert_eq!(batch2.observations.len(), 1, "补全后恰读一次");
    assert_eq!(batch2.observations[0].stable_call_key, "sess-1:main:u-2");
    let keys: std::collections::BTreeSet<_> = batch2
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    assert_eq!(keys.len(), 1, "不重复计数");
}

/// 增量只读新增：第二轮只读追加；推进后游标重读为空（入参 cursor 只读）
#[test]
fn kimi_incremental_reads_only_new_tail() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let wire = kimi_wire_path(home, "sess-1");
    let t = BASE_MS + 10_000;
    std::fs::write(
        &wire,
        [
            kimi_llm_request(t - 1_000, "0.1", "kimi-k2.6"),
            kimi_step_end(t, "u-1", "0", 1, "end_turn", Some(10), Some(50), Some(120)),
        ]
        .join("\n")
            + "\n",
    )
    .unwrap();

    let batch1 = kimi::read_incremental(home, &SourceCursor::empty("kimi"), NOW_MS).unwrap();
    assert_eq!(batch1.observations.len(), 1);
    let cursor1 = batch1.next_cursor.clone();
    assert_ne!(cursor1.value, "");

    // 追加第二个 step
    let mut body = std::fs::read_to_string(&wire).unwrap();
    body.push_str(&kimi_llm_request(t + 1_000, "0.2", "kimi-k2.6"));
    body.push('\n');
    body.push_str(&kimi_step_end(
        t + 2_000,
        "u-2",
        "0",
        2,
        "end_turn",
        Some(20),
        Some(60),
        Some(300),
    ));
    body.push('\n');
    std::fs::write(&wire, body).unwrap();

    let batch2 = kimi::read_incremental(home, &cursor1, NOW_MS).unwrap();
    assert_eq!(batch2.observations.len(), 1, "只读新增行");
    assert_eq!(batch2.observations[0].stable_call_key, "sess-1:main:u-2");

    let batch2_again = kimi::read_incremental(home, &batch2.next_cursor, NOW_MS).unwrap();
    assert_eq!(batch2_again.observations.len(), 0, "重读为空（不重复计数）");
}

/// 未装 kimi（数据根不存在）= 正常空态：ok 批、零观察、游标原样
#[test]
fn kimi_missing_root_is_empty_ok_batch() {
    let dir = tempfile::tempdir().unwrap();
    let batch = kimi::read_incremental(dir.path(), &SourceCursor::empty("kimi"), NOW_MS).unwrap();
    assert!(batch.observations.is_empty());
    assert!(batch.source_state.ok);
    assert_eq!(batch.next_cursor.value, "");
}

// ===========================================================================
// WorkBuddy 用例
// ===========================================================================

/// qua.end 同请求成功入速：completion_tokens/(total_cost_ms−ttft_ms)，核对成功 finish
/// 与 model/request 身份（supplement-macos §11.2 样本 2 的脱敏数值 → TPS ≈ 58.6）
#[test]
fn wb_qua_end_same_request_success_enters_speed() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let t_end = BASE_MS + 30_000;
    // total 19950 / ttft 2626 / completion 1015 / finish tool_calls（同请求一行内齐全）
    let mut fields = wb_qua_identity("msg-2", "creq-2", "chat");
    fields["total_cost_ms"] = json!(19_950);
    fields["sse_connect_cost_ms"] = json!(2_623);
    fields["ttft_ms"] = json!(2_626);
    fields["completion_tokens"] = json!(1_015);
    fields["prompt_tokens"] = json!(100);
    fields["total_tokens"] = json!(1_115);
    fields["finish_flag"] = json!("");
    fields["finish_reason"] = json!("tool_calls");
    fields["model_error"] = json!("");
    let log = wb_logs_dir(home).join("cli_host__0__test.log");
    std::fs::write(&log, wb_qua_end_line(t_end, fields) + "\n").unwrap();

    let batch =
        workbuddy::read_incremental(home, &SourceCursor::empty("workbuddy"), NOW_MS).unwrap();
    assert_eq!(batch.observations.len(), 1, "恰一条 qua.end 观察");
    let o = &batch.observations[0];

    // 生成窗 = total_cost_ms − ttft_ms（同请求两段，等待段天生分开）
    assert_eq!(o.generation_ms, Some(19_950 - 2_626));
    assert_eq!(o.output_tokens, Some(1_015));
    let tps = single_call_tps(o).unwrap();
    assert!(
        (tps - 1000.0 * 1_015.0 / 17_324.0).abs() < 1e-9,
        "证据数 ≈ 58.6 tok/s，实得 {tps}"
    );
    // 时刻锚定：结束 = 日志行时刻；开始 = 结束 − total；首 token = 开始 + ttft
    assert_eq!(o.ended_at_ms, t_end);
    assert_eq!(o.started_at_ms, t_end - 19_950);
    assert_eq!(o.first_token_at_ms, Some(t_end - 19_950 + 2_626));
    assert_eq!(o.native_duration_ms, Some(19_950));
    assert_eq!(o.ttft_ms(), Some(2_626));
    // completion_tokens 是供应商总数（含思考与工具参数）→ 已含思考，不再相加；
    // 内容侧 text/think_* 子计数疑为估算，绝不加进分子
    assert_eq!(o.output_scope, OutputScope::IncludesReasoning);
    assert_eq!(o.outcome, Outcome::Success);
    assert_eq!(o.method, TimingMethod::ClientNative);
    assert_eq!(o.model, "hy3");
    assert_eq!(o.session_key.as_deref(), Some("conv-1"));
    assert_eq!(
        o.turn_key.as_deref(),
        Some("creq-2"),
        "原生 conversation-request 身份"
    );
    assert_eq!(o.stable_call_key, "msg-2");
    assert_eq!(o.purpose, Purpose::MainTask);
}

/// 标题类辅助请求标 AssistTitle、默认不进主任务曲线（Spec §3）；
/// 主任务同行入速
#[test]
fn wb_title_purpose_excluded_from_main_curve() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let t = BASE_MS + 30_000;
    // 标题类：request_type=conversation*topic（supplement-macos §11.2 样本 1 形态）
    let mut title = wb_qua_identity("msg-title", "creq-title", "conversation_send_topic");
    title["total_cost_ms"] = json!(583);
    title["ttft_ms"] = json!(526);
    title["completion_tokens"] = json!(20);
    title["finish_reason"] = json!("stop");
    // 主任务正常行
    let mut main = wb_qua_identity("msg-main", "creq-main", "chat");
    main["total_cost_ms"] = json!(4_230);
    main["ttft_ms"] = json!(1_070);
    main["completion_tokens"] = json!(162);
    main["finish_reason"] = json!("stop");
    let log = wb_logs_dir(home).join("cli_host__0__test.log");
    std::fs::write(
        &log,
        [wb_qua_end_line(t, title), wb_qua_end_line(t + 1_000, main)].join("\n") + "\n",
    )
    .unwrap();

    let batch =
        workbuddy::read_incremental(home, &SourceCursor::empty("workbuddy"), NOW_MS).unwrap();
    assert_eq!(batch.observations.len(), 2);
    let title_obs = batch
        .observations
        .iter()
        .find(|o| o.stable_call_key == "msg-title")
        .unwrap();
    let main_obs = batch
        .observations
        .iter()
        .find(|o| o.stable_call_key == "msg-main")
        .unwrap();
    assert_eq!(title_obs.purpose, Purpose::AssistTitle, "标题类标辅助用途");
    assert_eq!(main_obs.purpose, Purpose::MainTask);

    // 主曲线（build_point 的样本面限主任务）：两行同 model 同 series，标题行不得进样本面
    // ——该 series 出 value 点但样本只含主任务行（标题不贡献分子/样本数，Spec §3）
    let at = t + 2_000;
    let points = build_point(&batch.observations, &[], "run-1", at);
    let series = SeriesKey {
        tool: "workbuddy".into(),
        model: "hy3".into(),
        channel: None,
        method: TimingMethod::ClientNative,
    };
    let point = points
        .iter()
        .find(|p| p.series_key == series)
        .expect("同 model series 应在点集里");
    assert_eq!(point.status, PointStatus::Value, "主任务行出 value 点");
    assert_eq!(point.sample_count, 1, "样本面只有主任务行（标题被排除）");
    assert_eq!(
        point.sum_output_tokens, 162,
        "标题行的 20 token 不得进主曲线分子"
    );
    assert_eq!(
        point.sampled_keys,
        vec!["msg-main".to_string()],
        "入样键只有主任务行"
    );
}

/// Windows 正常路线只有 hasUsage=true 而无数值 usage → 不计速度，
/// 该路线返回 insufficient-evidence 缺口状态，不借任何旧数据凑值
#[test]
fn wb_windows_hasonly_usage_not_counted_insufficient_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let t = BASE_MS + 30_000;
    // supplement-windows §7.5：sdk 会话日志有流时刻，usage_update 只含 hasUsage=true、无数值
    let log = wb_sdk_log_path(home, "conv-1.log");
    let lines = [
        format!("[{}] [Info] sse connected", rfc3339_ms(t)),
        format!(
            "[{}] [Info] agent_message_chunk len=42",
            rfc3339_ms(t + 1_000)
        ),
        format!(
            "[{}] [Info] usage_update {{\"hasUsage\":true}}",
            rfc3339_ms(t + 2_000)
        ),
        format!("[{}] [Info] PHASE_IDLE", rfc3339_ms(t + 3_000)),
    ];
    std::fs::write(&log, lines.join("\n") + "\n").unwrap();

    let batch =
        workbuddy::read_incremental(home, &SourceCursor::empty("workbuddy"), NOW_MS).unwrap();
    assert!(
        batch.observations.is_empty(),
        "hasUsage=true 单独不产观察/不入速"
    );
    assert!(
        batch
            .diagnostics
            .iter()
            .any(|d| d.contains("hasUsage") || d.contains("insufficient-evidence")),
        "缺数值 usage 必须进诊断：{:?}",
        batch.diagnostics
    );
    let detail = batch.source_state.detail.clone().unwrap_or_default();
    assert!(
        detail.contains("insufficient-evidence"),
        "来源状态必须声明 Windows 路线缺口：{detail}"
    );
    assert!(
        batch.source_state.ok,
        "缺口态不是读取失败（ok 批 + 明示缺口）"
    );
}

/// qua 行只有 hasUsage 标志而无数值 completion_tokens：成功事实保留但绝不入速
#[test]
fn wb_qua_row_without_numeric_tokens_not_counted() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let t = BASE_MS + 30_000;
    let mut fields = wb_qua_identity("msg-w", "creq-w", "chat");
    fields["total_cost_ms"] = json!(3_000);
    fields["ttft_ms"] = json!(500);
    fields["hasUsage"] = json!(true); // 只有标志，无数值
    fields["finish_reason"] = json!("stop");
    let log = wb_logs_dir(home).join("cli_host__0__test.log");
    std::fs::write(&log, wb_qua_end_line(t, fields) + "\n").unwrap();

    let batch =
        workbuddy::read_incremental(home, &SourceCursor::empty("workbuddy"), NOW_MS).unwrap();
    assert_eq!(batch.observations.len(), 1, "成功事实保留");
    let o = &batch.observations[0];
    assert_eq!(o.output_tokens, None, "无数值 usage 是缺失，不得伪装成 0");
    assert!(!o.is_tps_valid(), "无数值 usage 绝不入速");
    assert!(batch.diagnostics.iter().any(|d| d.contains("usage")));
}

/// 取消 / 失败不入速度：finish_flag=canceled → Cancelled；model_error → Failed；
/// 有可靠起止仍保留事实面
#[test]
fn wb_cancelled_and_failed_rows_skip_speed() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let t = BASE_MS + 30_000;
    let mut cancelled = wb_qua_identity("msg-c", "creq-c", "chat");
    cancelled["total_cost_ms"] = json!(2_000);
    cancelled["ttft_ms"] = json!(300);
    cancelled["completion_tokens"] = json!(64);
    cancelled["finish_flag"] = json!("canceled"); // onStreamEnd 的 aborted 位
    let mut failed = wb_qua_identity("msg-f", "creq-f", "chat");
    failed["total_cost_ms"] = json!(1_500);
    failed["ttft_ms"] = json!(200);
    failed["completion_tokens"] = json!(8);
    failed["finish_reason"] = json!("error");
    failed["model_error"] = json!("stream_timeout");
    let log = wb_logs_dir(home).join("cli_host__0__test.log");
    std::fs::write(
        &log,
        [
            wb_qua_end_line(t, cancelled),
            wb_qua_end_line(t + 1_000, failed),
        ]
        .join("\n")
            + "\n",
    )
    .unwrap();

    let batch =
        workbuddy::read_incremental(home, &SourceCursor::empty("workbuddy"), NOW_MS).unwrap();
    assert_eq!(batch.observations.len(), 2);
    let c = batch
        .observations
        .iter()
        .find(|o| o.outcome == Outcome::Cancelled)
        .unwrap();
    assert_eq!(c.generation_ms, None, "取消不入速度");
    assert!(c.failure_reason.is_some());
    assert!(!c.is_tps_valid());
    let f = batch
        .observations
        .iter()
        .find(|o| o.outcome == Outcome::Failed)
        .unwrap();
    assert_eq!(f.generation_ms, None, "失败不入速度");
    assert!(f
        .failure_reason
        .as_deref()
        .unwrap()
        .contains("stream_timeout"));
    assert!(!f.is_tps_valid());
}

/// 残缺尾行重读不丢 + 增量只读新增（qua 日志同一套字节界纪律）
#[test]
fn wb_torn_tail_line_reread_and_incremental() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let t = BASE_MS + 30_000;
    let mut row1 = wb_qua_identity("msg-1", "creq-1", "chat");
    row1["total_cost_ms"] = json!(2_000);
    row1["ttft_ms"] = json!(300);
    row1["completion_tokens"] = json!(64);
    row1["finish_reason"] = json!("stop");
    let mut row2 = wb_qua_identity("msg-2", "creq-2", "chat");
    row2["total_cost_ms"] = json!(3_000);
    row2["ttft_ms"] = json!(400);
    row2["completion_tokens"] = json!(96);
    row2["finish_reason"] = json!("stop");
    let line2 = wb_qua_end_line(t + 1_000, row2);

    let log = wb_logs_dir(home).join("cli_host__0__test.log");
    std::fs::write(
        &log,
        wb_qua_end_line(t, row1) + "\n" + &line2[..line2.len() / 2],
    )
    .unwrap();

    let batch1 =
        workbuddy::read_incremental(home, &SourceCursor::empty("workbuddy"), NOW_MS).unwrap();
    assert_eq!(batch1.observations.len(), 1, "只有完整行出观察");
    let cursor1 = batch1.next_cursor.clone();

    let batch_torn = workbuddy::read_incremental(home, &cursor1, NOW_MS).unwrap();
    assert_eq!(batch_torn.observations.len(), 0, "残行不出观察");
    assert_eq!(
        batch_torn.next_cursor.value, cursor1.value,
        "残行不推进水位"
    );

    // 补全 → 恰读一次（只补缺失的后半段）
    let mut body = std::fs::read_to_string(&log).unwrap();
    body.push_str(&line2[line2.len() / 2..]);
    body.push('\n');
    std::fs::write(&log, body).unwrap();
    let batch2 = workbuddy::read_incremental(home, &batch_torn.next_cursor, NOW_MS).unwrap();
    assert_eq!(batch2.observations.len(), 1, "补全后恰读一次");
    assert_eq!(batch2.observations[0].stable_call_key, "msg-2");
}

/// 未装 workbuddy（数据根不存在）= 正常空态
#[test]
fn wb_missing_root_is_empty_ok_batch() {
    let dir = tempfile::tempdir().unwrap();
    let batch =
        workbuddy::read_incremental(dir.path(), &SourceCursor::empty("workbuddy"), NOW_MS).unwrap();
    assert!(batch.observations.is_empty());
    assert!(batch.source_state.ok);
    assert_eq!(batch.next_cursor.value, "");
}

// ===========================================================================
// Codex 用例（已接入 · 仅响应：日志近似响应窗；无流式窗/TTFT/生成窗，stream 仍不可用，
// 绝不从回合墙钟造窗）
// ===========================================================================

/// Spec §6.2.4「不用整轮耗时补洞」：rollout 只有 session_meta + task_complete（回合
/// duration 与回合 TTFT 齐全）+ 无对应生成项/输入边界的 token_usage_record 封口 ⇒
/// 依旧零观察（回合墙钟不入任何响应窗）；游标升级为 v1 并真实推进，未锚定封口留诊断
#[test]
fn codex_turn_minus_ttft_produces_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let t = BASE_MS + 30_000;
    // 真实结构（file-sources.md §5.1/5.2 + codex-structure-facts.md）：session_meta +
    // event_msg/task_complete（回合 duration 与回合 TTFT）+ 顶层 token_usage_record
    // 封口（response_id + 整数 usage；无 turn_context → 无模型归属；无输入边界/生成项
    // → 窗不成立）
    let lines = [
        codex_line(
            t,
            "session_meta",
            json!({"id": "th-1", "model_provider": "openai", "cli_version": "0.164.0"}),
        ),
        codex_line(
            t + 120_000,
            "event_msg",
            json!({"type": "task_complete", "duration_ms": 124_306,
                   "time_to_first_token_ms": 102_537, "turn_id": "turn-1"}),
        ),
        codex_line(
            t + 121_000,
            "token_usage_record",
            json!({"response_id": "resp-1", "turn_id": "turn-1",
                   "session_id": "th-1",
                   "usage": {"input_tokens": 100, "output_tokens": 200}}),
        ),
    ];
    let path = codex_rollout_path(home, "rollout-2026-10-24.jsonl");
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();

    let batch = codex::read_incremental(home, &SourceCursor::empty("codex"), NOW_MS).unwrap();
    let codex_turn_only = &batch.observations;
    // 核心不变量（Spec §6.2.4）：turn−TTFT 齐备也产不出任何观察（不造值、不用回合
    // 墙钟凑 TPS）；未锚定封口同样跳过（缺模型归属）
    assert!(codex_turn_only.is_empty());
    // 流式缺口语言保持真实（stream 仍不可用），响应口径明示已接入
    let detail = batch.source_state.detail.clone().unwrap_or_default();
    assert!(
        detail.starts_with("response-only:") && detail.contains("insufficient-generation-timing"),
        "来源状态 = 仅响应已接入 + stream 缺口保留：{detail}"
    );
    // 文件被真实打开并解码：未锚定封口（缺 turn_context 模型归属）有逐行诊断
    assert!(
        batch
            .diagnostics
            .iter()
            .any(|d| d.contains("missing-model skipped")),
        "无模型归属的封口必须诊断跳过：{:?}",
        batch.diagnostics
    );
    assert!(batch.source_state.ok, "零观察是正常读取结果，不是失败");
    // 游标已升级为版本化 JSON 并推进（不再是占位透传）
    let cursor: Value =
        serde_json::from_str(&batch.next_cursor.value).expect("游标必须是本源 v1 JSON");
    assert_eq!(cursor["format_version"], 1);
    assert_eq!(
        cursor["files"][0]["cold_start"], true,
        "窗内无输入边界，冷启动未转热"
    );
    assert_eq!(
        cursor["files"][0]["offset"],
        std::fs::metadata(&path).unwrap().len(),
        "三行全部读尽，offset 推进到文件尾"
    );
}

/// 未装 codex = 正常空态
#[test]
fn codex_missing_root_is_empty_ok_batch() {
    let dir = tempfile::tempdir().unwrap();
    let batch = codex::read_incremental(dir.path(), &SourceCursor::empty("codex"), NOW_MS).unwrap();
    assert!(batch.observations.is_empty());
    assert!(batch.source_state.ok);
}

/// R3（独立复核 P1）回归锁——偏移消费语义：已消费的大前缀追加少量新行后，第二读只
/// 从水位起消费（前缀里的坏 JSON 行在首读产生解析失败诊断；第二读若从头消费，同样的
/// 诊断会再次出现）。**读取字节量**与历史长度无关的判别证据在 `sources::read_tail` 的
/// 单测（读到的字节数 == 追加字节数，6 MiB 前缀）与 sources 内唯一读路径（无 fs::read）；
/// 本测试负责锁住源级的水位推进不回退、不重解。
#[test]
fn kimi_large_consumed_prefix_is_not_reread_on_append() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let wire = kimi_wire_path(home, "sess-big");
    let t = BASE_MS + 10_000;
    // ~2 MiB 已消费前缀：10 万行坏 JSON（每行都会触发「解析失败」诊断）+ 一对有效事件
    let mut body = String::with_capacity(2_200_000);
    for i in 0..100_000u32 {
        body.push_str(&format!("{{not-json-line-{i}\n"));
    }
    body.push_str(&kimi_llm_request(t - 1_000, "0.1", "kimi-k2.6"));
    body.push('\n');
    body.push_str(&kimi_step_end(
        t,
        "u-1",
        "0",
        1,
        "end_turn",
        Some(10),
        Some(50),
        Some(120),
    ));
    body.push('\n');
    std::fs::write(&wire, &body).unwrap();
    let prefix_len = std::fs::metadata(&wire).unwrap().len();
    assert!(
        prefix_len > 1_500_000,
        "前缀必须足够大才有意义: {prefix_len}"
    );

    let batch1 = kimi::read_incremental(home, &SourceCursor::empty("kimi"), NOW_MS).unwrap();
    assert_eq!(batch1.observations.len(), 1);
    assert!(
        batch1.diagnostics.iter().any(|d| d.contains("JSON")),
        "首读必须看到前缀的解析失败诊断: {:?}",
        batch1.diagnostics
    );

    // 追加一对新事件
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&wire)
            .unwrap();
        f.write_all(kimi_llm_request(t + 1_000, "0.2", "kimi-k2.6").as_bytes())
            .unwrap();
        f.write_all(b"\n").unwrap();
        f.write_all(
            kimi_step_end(
                t + 2_000,
                "u-2",
                "0",
                2,
                "end_turn",
                Some(20),
                Some(60),
                Some(300),
            )
            .as_bytes(),
        )
        .unwrap();
        f.write_all(b"\n").unwrap();
    }
    let batch2 = kimi::read_incremental(home, &batch1.next_cursor, NOW_MS).unwrap();
    assert_eq!(batch2.observations.len(), 1, "只出新增观察");
    assert_eq!(batch2.observations[0].stable_call_key, "sess-big:main:u-2");
    assert!(
        !batch2.diagnostics.iter().any(|d| d.contains("JSON")),
        "第二读不得再出现前缀的解析失败诊断（= 前缀被重读重解）: {:?}",
        batch2.diagnostics
    );
}

/// R4（独立复核 P1）：真实前缀 → 观察 → 采样点 → 查询整链；RFC3339 兼容仍在；坏前缀只诊断不造时刻
#[test]
fn wb_real_timestamp_prefix_flows_to_point_and_rfc3339_stays_compatible() {
    use multi_agents_manager_lib::services::tps::{build_point, PointStatus};
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let t = NOW_MS - 30_000;
    let mut fields = wb_qua_identity("msg-real", "creq-real", "agent");
    fields["completion_tokens"] = json!(1015);
    fields["total_cost_ms"] = json!(19950);
    fields["ttft_ms"] = json!(2626);
    fields["sse_connect_cost_ms"] = json!(2623);
    fields["finish_reason"] = json!("stop");
    fields["finish_flag"] = json!(true);
    let real_line = wb_qua_end_line(t, fields.clone());
    assert!(
        real_line.starts_with('[')
            && real_line.contains(", ")
            && (real_line.contains(" AM.") || real_line.contains(" PM.")),
        "夹具前缀必须是真机形态: {real_line}"
    );
    // 坏前缀：日期字段缺失 → 不造时刻、不入速、仍可继续消费后续行
    let bad_line = format!(
        "[13/45/2026, 99:99:99 XM.000] [Info] [pid=1] [SseMonitor] agent.sse.qua.end {}",
        wb_qua_identity("msg-bad", "creq-bad", "agent")
    );
    let log = wb_logs_dir(home).join("cli_host.log");
    std::fs::write(&log, format!("{bad_line}\n{real_line}\n")).unwrap();

    let batch =
        workbuddy::read_incremental(home, &SourceCursor::empty("workbuddy"), NOW_MS).unwrap();
    assert_eq!(
        batch.observations.len(),
        1,
        "真实前缀的那条进入观察；坏前缀的不进: {:?}",
        batch.diagnostics
    );
    let o = &batch.observations[0];
    assert_eq!(
        o.ended_at_ms, t,
        "结束时刻 = 行前缀时刻（本地时区解释），不是 mtime"
    );
    let tps = single_call_tps(o).expect("有效样本");
    assert!(
        (tps - 1015.0 * 1000.0 / (19950.0 - 2626.0)).abs() < 0.05,
        "tps={tps}"
    );
    assert!(
        batch
            .diagnostics
            .iter()
            .any(|d| d.contains("prefix") || d.contains("时刻")),
        "坏前缀必须留诊断: {:?}",
        batch.diagnostics
    );
    // 整链：进 120s 点（结束时刻在窗内才可能）
    let points = build_point(&batch.observations, &[], "run-r4", NOW_MS);
    let value_point = points
        .iter()
        .find(|p| p.status == PointStatus::Value)
        .expect("真实前缀时刻解析正确 → 调用落在 120s 窗内 → 出 value 点");
    assert_eq!(value_point.sample_count, 1);

    // RFC3339 兼容锁
    let home2 = tempfile::tempdir().unwrap();
    let log2 = wb_logs_dir(home2.path()).join("cli_host.log");
    std::fs::write(&log2, format!("{}\n", wb_qua_end_line_rfc3339(t, fields))).unwrap();
    let batch2 =
        workbuddy::read_incremental(home2.path(), &SourceCursor::empty("workbuddy"), NOW_MS)
            .unwrap();
    assert_eq!(batch2.observations.len(), 1, "RFC3339 前缀仍可解析");
    assert_eq!(batch2.observations[0].ended_at_ms, t);
}
