//! TPS 五原生来源含等待响应窗集成测试（2026-10-26 桌面工作区与输出速度 V2 计划 Task 3）。
//!
//! ## 每源响应窗规则（Spec §5.1/§5.2「含等待响应」列；一步一源、不从生成空值推定）
//!
//! * **DSH**：响应窗 = 模型 step 开始 → 消息结算（与起点/终点/整调用时长同边界）；
//!   含首 token 等待，不含结算之后的 step/end 工具执行；重试不能对齐 → 与流式口径
//!   一并剔除（None）。
//! * **ZCode**：响应窗 = model_usage 行的 verified `completed − started`（与行自身
//!   duration_ms 同窗，绝不延伸到后续工具执行）；成功且 retry_count=0 才填实；
//!   缺 first_token（F）的完整调用是 response-only 有效样本。
//! * **Kimi**：响应窗 = 同一 `step.end` 事件的原生 `llmFirstTokenLatencyMs +
//!   llmStreamDurationMs`（请求起点→首片段→流结束；dsh-kimi.md 指标表「模型累计用时
//!   = TTFT + streamDuration」，等待只计一次）；保留既有内部重试限制（可见同
//!   turnStep 多 request → 不填）。
//! * **Claude**：响应窗 = 完整 span `duration_ms`（含等待）；`first_content` 仍是
//!   流式口径的块开始近似。成功证据闸 + 尝试闸（已识别重试不对齐）双闸通过且
//!   duration 可靠才填实；缺 first_content 的 span 是 response-only 有效样本。
//! * **OpenCode**：响应窗 = 完整 step 窗（start 行宿主 `started` → 终行 `created`），
//!   同 session/message/step 身份；绝不借消息间墙钟拼窗；成功且无已识别未对齐重试
//!   才填实。
//!
//! ## 隔离纪律（2026-10-07 事故族，与 tps_native_*_test.rs 同款）
//!
//! 全部夹具是 tempdir 影子 home 下的**合成**结构，helper 构造器逐字取自既有
//! tps_native_primary_test / tps_native_secondary_test / tps_claude_ingest_test /
//! tps_opencode_test（形态源 = 真实脱敏证据，不重造事件形态）；显式注入 `now_ms`，
//! 绝不触真机 `~/.dsh` / `~/.zcode` / `~/.kimi` / `~/.claude` / `~/.tuvis`，
//! 绝不起 Tauri、不发模型请求。

use multi_agents_manager_lib::services::tps::model::{Outcome, SourceCursor, TpsObservation};
use multi_agents_manager_lib::services::tps::sources::{claude, dsh, kimi, opencode, zcode};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// 固定假时钟基准（纯整数，不读系统时钟；夹具事件时间都锚在它附近）
const BASE_MS: i64 = 1_700_000_000_000;
/// 测试注入的「现在」：事件 + 2 小时（< 24h 保留窗）
const NOW_MS: i64 = BASE_MS + 2 * 60 * 60 * 1000;

fn obs_with_key<'a>(batch_observations: &'a [TpsObservation], suffix: &str) -> &'a TpsObservation {
    batch_observations
        .iter()
        .find(|o| o.stable_call_key.ends_with(suffix))
        .unwrap_or_else(|| panic!("应存在 stable_call_key 以 {suffix} 结尾的观察"))
}

// ==== DSH 夹具（helper 逐字取自 tps_native_primary_test.rs）====

fn dsh_session_dir(home: &Path, session: &str) -> PathBuf {
    home.join(".dsh")
        .join("sessions")
        .join("proj-a")
        .join(session)
}

fn dsh_header(id: &str) -> String {
    json!({"type": "session", "version": 3, "id": id, "createdAt": BASE_MS,
           "cwd": "/p/a", "delegationDepth": 0})
    .to_string()
}

fn dsh_event(kind: &str, seq: i64, time: i64, data: Value) -> String {
    json!({"type": kind, "seq": seq, "time": time, "data": data}).to_string()
}

/// 紧凑段（dsh-kimi.md：time0 + dt[]（长度=成员数−1）+ texts[]）
fn compact_chunks(kind: &str, time0: i64, index: i64, dt: &[i64], texts: &[&str]) -> Value {
    json!({"type": kind, "time0": time0, "index": index, "dt": dt, "texts": texts})
}

/// replay profile = dsh-output-scope-addendum.md 固定准入形态
fn replay_source() -> Value {
    json!({"kind": "pi-ai", "provider": "deepseek", "model": "deepseek-chat",
           "replayState": {"response": {"kind": "pi-ai", "version": 2,
                                        "api": "openai-completions",
                                        "provider": "deepseek", "model": "deepseek-chat",
                                        "responseId": "resp-1", "stopReason": "end_turn"}}})
}

#[allow(clippy::too_many_arguments)]
fn dsh_message_event(
    seq: i64,
    time: i64,
    turn: i64,
    step: i64,
    output_tokens: Option<i64>,
    stream: Vec<Value>,
    source: Value,
) -> String {
    let mut message = json!({"role": "assistant",
        "content": [{"type": "text", "text": "ok"}], "id": format!("m-{seq}")});
    message["source"] = source;
    let mut data = json!({"turn": turn, "step": step, "message": message, "stream": stream});
    if let Some(out) = output_tokens {
        data["usage"] = json!({"inputTokens": 10, "outputTokens": out, "totalTokens": 10 + out});
    }
    dsh_event("assistant/message", seq, time, data)
}

fn write_session_file(dir: &Path, name: &str, lines: &[String]) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    path
}

// ==== ZCode 夹具（helper 逐字取自 tps_native_primary_test.rs）====

fn zcode_create_db(home: &Path) -> Connection {
    let path = home.join(".zcode").join("cli").join("db").join("db.sqlite");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE model_usage (
            logical_request_id TEXT NOT NULL DEFAULT '',
            attempt_index      INTEGER NOT NULL DEFAULT 0,
            session_id         TEXT NOT NULL,
            turn_id            TEXT,
            assistant_message_id TEXT,
            parent_user_message_id TEXT,
            trace_id           TEXT,
            span_id            TEXT,
            provider_id        TEXT,
            model_id           TEXT,
            variant            TEXT,
            agent              TEXT,
            mode               TEXT,
            task_type          TEXT NOT NULL DEFAULT 'interactive',
            query_source       TEXT,
            status             TEXT NOT NULL,
            started_at         INTEGER,
            first_token_at     INTEGER,
            completed_at       INTEGER,
            duration_ms        INTEGER,
            time_to_first_token_ms INTEGER,
            input_tokens       INTEGER NOT NULL DEFAULT 0,
            output_tokens      INTEGER,
            reasoning_tokens   INTEGER NOT NULL DEFAULT 0,
            cache_read_input_tokens INTEGER NOT NULL DEFAULT 0,
            cache_creation_input_tokens INTEGER NOT NULL DEFAULT 0,
            computed_total_tokens INTEGER NOT NULL DEFAULT 0,
            retry_count        INTEGER NOT NULL DEFAULT 0,
            retryable          INTEGER NOT NULL DEFAULT 0,
            cancelled_by_user  INTEGER NOT NULL DEFAULT 0
        );
        "#,
    )
    .unwrap();
    conn
}

/// 一行 model_usage（未点名的列走表默认；返回 rowid）
#[allow(clippy::too_many_arguments)]
fn zc_insert(
    conn: &Connection,
    logical_request_id: &str,
    status: &str,
    started_at: Option<i64>,
    first_token_at: Option<i64>,
    completed_at: Option<i64>,
    duration_ms: Option<i64>,
    output: Option<i64>,
    reasoning: i64,
    retry_count: i64,
    task_type: &str,
) -> i64 {
    conn.execute(
        "INSERT INTO model_usage (logical_request_id, session_id, turn_id, assistant_message_id,
             provider_id, model_id, status, started_at, first_token_at, completed_at,
             duration_ms, time_to_first_token_ms, input_tokens, output_tokens, reasoning_tokens,
             cache_read_input_tokens, cache_creation_input_tokens, computed_total_tokens,
             retry_count, task_type)
         VALUES (?1, 'sess_z', 'turn_1', 'amsg-1', 'deepseek', 'deepseek-v4.1-flash',
             ?2, ?3, ?4, ?5, ?6, ?4 - ?3, 21112, ?7, ?8, 0, 0, 21112 + COALESCE(?7,0),
             ?9, ?10)",
        rusqlite::params![
            logical_request_id,
            status,
            started_at,
            first_token_at,
            completed_at,
            duration_ms,
            output,
            reasoning,
            retry_count,
            task_type,
        ],
    )
    .unwrap();
    conn.last_insert_rowid()
}

// ==== Kimi 夹具（helper 逐字取自 tps_native_secondary_test.rs）====

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

fn kimi_loop_event(time: i64, event: Value) -> String {
    json!({"type": "context.append_loop_event", "time": time, "event": event}).to_string()
}

/// `llm.request`：真机形态为顶层行（provider/model/turnStep 与行级 time 直接在行上）
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

// ==== Claude 夹具（helper 逐字取自 tps_claude_ingest_test.rs）====

const TRACE_ID: &str = "a22d4d3caabbccdd1122334455667788";
const SESSION_ID: &str = "sess-1";
const BASE_NS: i64 = BASE_MS * 1_000_000;

fn attr_s(key: &str, v: &str) -> Value {
    json!({"key": key, "value": {"stringValue": v}})
}
fn attr_d(key: &str, v: f64) -> Value {
    json!({"key": key, "value": {"doubleValue": v}})
}
fn attr_i(key: &str, v: &str) -> Value {
    json!({"key": key, "value": {"intValue": v}})
}
fn attr_b(key: &str, v: bool) -> Value {
    json!({"key": key, "value": {"boolValue": v}})
}

fn hex_id(prefix: u8, n: usize) -> String {
    let fill = format!("{prefix:02x}").repeat(n);
    fill
}

/// 一条 claude_code.llm_request span（OTLP JSON wire 形态）
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
    attrs.push(attr_d("duration_ms", dur_ms as f64));
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

/// 工具 span（独立生命周期：不产模型观察）
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

// ==== OpenCode 夹具（helper 逐字取自 tps_opencode_test.rs）====

const SK: &str = "a1b2c3d4a1b2c3d4a1b2c3d4a1b2c3d4a1b2c3d4a1b2c3d4a1b2c3d4a1b2c3d4";
const MK: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

/// 写一行事实（键集 = 插件产出契约的 15 键；缺省值与插件 blankRow 一致）
fn row(event_key: &str, r#type: &str, created_offset: i64, extra: Value) -> String {
    let mut base = json!({
        "eventKey": event_key,
        "sessionKey": SK,
        "messageKey": MK,
        "ordinal": 1,
        "attempt": 0,
        "type": r#type,
        "created": BASE_MS + created_offset,
        "started": null,
        "tokens": null,
        "finish": null,
        "hasNonEmptyDelta": false,
        "deltaChars": 0,
        "model": null,
        "provider": null,
        "tokensReasoning": null,
    });
    if let Value::Object(map) = extra {
        for (k, v) in map {
            base[k] = v;
        }
    }
    base.to_string()
}

fn append_lines(home: &Path, lines: &[String]) {
    let path = home
        .join(".tuvis")
        .join("tps")
        .join("opencode")
        .join("facts.jsonl");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .unwrap();
    for line in lines {
        writeln!(f, "{line}").unwrap();
    }
}

fn read(
    home: &Path,
    cursor: &SourceCursor,
) -> multi_agents_manager_lib::services::tps::model::ReadBatch {
    opencode::read_incremental(home, cursor, NOW_MS).unwrap()
}

fn empty_cursor() -> SourceCursor {
    SourceCursor::empty("opencode")
}

// ===========================================================================
// DSH：响应窗 = step 开始 → 消息结算
// ===========================================================================

/// 正常记录双窗：响应窗含等待（首 token 前的 80ms）且终于消息结算，
/// 不含结算之后 step/end 的工具执行；生成窗照旧（首非空 → 结算）。
#[test]
fn dsh_response_window_covers_wait_and_excludes_tool_time() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let t0 = BASE_MS;
    let lines = vec![
        dsh_header("sess-1"),
        dsh_event("step/start", 2, t0 + 20, json!({"turn": 1, "step": 1})),
        dsh_message_event(
            3,
            t0 + 120,
            1,
            1,
            Some(96),
            vec![compact_chunks(
                "text-chunks",
                t0 + 100,
                0,
                &[5, 4],
                &["hel", "lo", "!"],
            )],
            replay_source(),
        ),
        // step/end（含工具的 step 终结）在结算之后：不是响应窗终点
        dsh_event("step/end", 4, t0 + 130, json!({"turn": 1, "step": 1})),
    ];
    let sess = dsh_session_dir(home, "sess-1");
    write_session_file(&sess, "session.v3.jsonl", &lines);

    let batch = dsh::read_incremental(home, &SourceCursor::empty("dsh"), NOW_MS).unwrap();
    assert_eq!(batch.observations.len(), 1, "恰一条消息观察");
    let native = &batch.observations[0];

    // 响应窗 = step 开始(t0+20) → 消息结算(t0+120) = 100ms（含 80ms 等待）
    assert_eq!(native.response_elapsed_ms, Some(100));
    // 生成窗照旧：首非空(t0+100) → 结算(t0+120) = 20ms；等待只在响应窗计一次
    assert_eq!(native.generation_ms, Some(20));
    assert_eq!(native.started_at_ms, t0 + 20);
    assert_eq!(native.ended_at_ms, t0 + 120, "模型终结 = 消息结算");
    assert_eq!(
        native.response_elapsed_ms.unwrap() - native.generation_ms.unwrap(),
        80,
        "响应窗 − 生成窗 = 首 token 等待（80ms，只计一次）"
    );
    assert_eq!(
        native.native_duration_ms, native.response_elapsed_ms,
        "与整调用时长同边界（同起点同终点）"
    );
    assert!(native.is_response_valid());
    assert!(native.is_tps_valid(), "流式口径同样成立（双窗并存）");
}

/// 重试不能对齐 → 流式与响应口径一并剔除（跨 attempt 的窗不是单次调用）；
/// 首个 attempt 的 Failed 观察同样无响应窗（失败不入速度）。
#[test]
fn dsh_retry_misaligned_gets_no_response_window() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let t0 = BASE_MS;
    let lines = vec![
        dsh_header("sess-1"),
        dsh_event("step/start", 2, t0 + 20, json!({"turn": 1, "step": 1})),
        dsh_event(
            "assistant/attempt",
            3,
            t0 + 500,
            json!({"turn": 1, "step": 1, "finish": {"kind": "error"}}),
        ),
        dsh_message_event(
            4,
            t0 + 1200,
            1,
            1,
            Some(96),
            vec![compact_chunks(
                "text-chunks",
                t0 + 1100,
                0,
                &[50],
                &["a", "b"],
            )],
            replay_source(),
        ),
    ];
    let sess = dsh_session_dir(home, "sess-1");
    write_session_file(&sess, "session.v3.jsonl", &lines);

    let batch = dsh::read_incremental(home, &SourceCursor::empty("dsh"), NOW_MS).unwrap();
    assert_eq!(batch.observations.len(), 2);

    let msg = batch
        .observations
        .iter()
        .find(|o| o.outcome == Outcome::Success)
        .unwrap();
    assert_eq!(msg.generation_ms, None, "重试后成功的调用流式剔除");
    assert_eq!(
        msg.response_elapsed_ms, None,
        "对齐失败 → 响应窗一并剔除（留 None）"
    );
    assert!(!msg.is_response_valid());

    let attempt = batch
        .observations
        .iter()
        .find(|o| o.outcome == Outcome::Failed)
        .unwrap();
    assert_eq!(attempt.response_elapsed_ms, None, "失败无响应速度");
    assert!(!attempt.is_response_valid());
}

/// 单时刻整块突发：流式生成窗被排除（既有点保持），但 step 开始→结算的完整响应窗
/// 可靠 → response-only 有效（Spec §5.1「缺 F 但完整响应窗可靠的记录可以只入 response」）。
#[test]
fn dsh_single_burst_stream_keeps_response_only_window() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let t0 = BASE_MS;
    let lines = vec![
        dsh_header("sess-1"),
        dsh_event("step/start", 2, t0 + 350, json!({"turn": 1, "step": 4})),
        // 非空流成员只在一个时刻（dt=0 的整块突发）→ 无生成窗
        dsh_message_event(
            3,
            t0 + 380,
            1,
            4,
            Some(11),
            vec![compact_chunks(
                "text-chunks",
                t0 + 370,
                0,
                &[0],
                &["burst-a", "burst-b"],
            )],
            replay_source(),
        ),
    ];
    let sess = dsh_session_dir(home, "sess-1");
    write_session_file(&sess, "session.v3.jsonl", &lines);

    let batch = dsh::read_incremental(home, &SourceCursor::empty("dsh"), NOW_MS).unwrap();
    assert_eq!(batch.observations.len(), 1);
    let burst = &batch.observations[0];
    assert_eq!(burst.generation_ms, None, "整块突发无流式生成窗（既有点）");
    assert_eq!(
        burst.response_elapsed_ms,
        Some(30),
        "step 开始(t0+350) → 结算(t0+380) = 30ms，响应窗照常可靠"
    );
    assert!(burst.is_response_valid(), "response-only 有效");
    assert!(!burst.is_tps_valid(), "流式口径仍被排除（既有判定不变）");
}

// ===========================================================================
// ZCode：响应窗 = completed − started（成功 + retry=0）
// ===========================================================================

/// 正常记录双窗：响应 = 行级 verified 起止差（与行自身 duration_ms 同窗），
/// 生成 = 首点 → completed；等待（started→first_token）只在响应窗计一次。
#[test]
fn zcode_full_call_response_equals_completed_minus_start() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let start = BASE_MS;
    let first = BASE_MS + 1_200;
    let done = BASE_MS + 5_000;
    {
        let conn = zcode_create_db(home);
        zc_insert(
            &conn,
            "req-native",
            "completed",
            Some(start),
            Some(first),
            Some(done),
            Some(5_000),
            Some(96),
            0,
            0,
            "main_turn",
        );
    }
    let batch = zcode::read_incremental(home, &SourceCursor::empty("zcode"), NOW_MS).unwrap();
    assert_eq!(batch.observations.len(), 1);
    let native = &batch.observations[0];

    // 响应窗 = completed − started = 5000ms（含 1200ms 等待；= 行自身 duration_ms）
    assert_eq!(native.response_elapsed_ms, Some(5000));
    // 生成窗照旧：first_token → completed = 3800ms
    assert_eq!(native.generation_ms, Some(3_800));
    assert_eq!(native.started_at_ms, start);
    assert_eq!(native.ended_at_ms, done);
    assert_eq!(
        native.response_elapsed_ms.unwrap() - native.generation_ms.unwrap(),
        1_200,
        "等待段只计入响应窗一次"
    );
    assert!(native.is_response_valid());
    assert!(native.is_tps_valid());
}

/// 缺 F（工具参数响应无首点）但 start/completed 可靠 → response-only 有效。
#[test]
fn zcode_missing_first_token_is_response_only() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let start = BASE_MS;
    let done = BASE_MS + 5_000;
    {
        let conn = zcode_create_db(home);
        zc_insert(
            &conn,
            "req-nof",
            "completed",
            Some(start),
            None,
            Some(done),
            Some(5_000),
            Some(96),
            0,
            0,
            "main_turn",
        );
    }
    let batch = zcode::read_incremental(home, &SourceCursor::empty("zcode"), NOW_MS).unwrap();
    assert_eq!(batch.observations.len(), 1);
    let missing_first = &batch.observations[0];
    assert_eq!(missing_first.generation_ms, None);
    assert_eq!(
        missing_first.response_elapsed_ms,
        Some(5_000),
        "start/completed 可靠 → 响应窗仍在"
    );
    assert!(missing_first.is_response_valid());
    assert!(
        !missing_first.is_tps_valid(),
        "流式口径仍缺 F 跳过（既有点）"
    );
}

/// retry>0（内部尝试不可对齐）/ 失败 / 取消 → 无响应窗、不入响应速度（既判定不变）。
#[test]
fn zcode_retry_failed_cancelled_get_no_response_window() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let start = BASE_MS;
    let first = BASE_MS + 1_200;
    let done = BASE_MS + 5_000;
    {
        let conn = zcode_create_db(home);
        zc_insert(
            &conn,
            "req-r",
            "completed",
            Some(start),
            Some(first),
            Some(done),
            Some(5_000),
            Some(96),
            0,
            2,
            "main_turn",
        );
        zc_insert(
            &conn,
            "req-e",
            "error",
            Some(start),
            Some(first),
            Some(done),
            Some(5_000),
            Some(96),
            0,
            0,
            "main_turn",
        );
        zc_insert(
            &conn,
            "req-c",
            "cancelled",
            Some(start),
            Some(first),
            Some(done),
            Some(5_000),
            Some(96),
            0,
            0,
            "main_turn",
        );
    }
    let batch = zcode::read_incremental(home, &SourceCursor::empty("zcode"), NOW_MS).unwrap();
    assert_eq!(batch.observations.len(), 3);

    let retried = obs_with_key(&batch.observations, "req-r");
    assert_eq!(retried.outcome, Outcome::Success);
    assert_eq!(
        retried.response_elapsed_ms, None,
        "retry>0 内部尝试不可对齐 → 跨尝试窗不填"
    );
    assert!(!retried.is_response_valid());

    for key in ["req-e", "req-c"] {
        let o = obs_with_key(&batch.observations, key);
        assert_eq!(o.response_elapsed_ms, None, "{key} 失败/取消不填响应窗");
        assert!(!o.is_response_valid());
        assert!(o.failure_reason.is_some());
    }
}

// ===========================================================================
// Kimi：响应窗 = 同事件 ttft + stream（等待只计一次）
// ===========================================================================

/// 正常记录双窗：响应 = llmFirstTokenLatencyMs + llmStreamDurationMs（同 step.end
/// 事件；等待 519ms 只计一次），生成 = stream 806ms。
#[test]
fn kimi_response_is_native_latency_plus_stream_same_event() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let wire = kimi_wire_path(home, "sess-1");
    let t_end = BASE_MS + 10_000;
    let lines = [
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
    assert_eq!(batch.observations.len(), 1);
    let native = &batch.observations[0];

    // 响应窗 = 519 + 806 = 1325ms（dsh-kimi.md「模型累计用时 = TTFT + streamDuration」）
    assert_eq!(native.response_elapsed_ms, Some(1_325));
    assert_eq!(native.generation_ms, Some(806));
    assert_eq!(native.native_duration_ms, Some(1_325), "与整调用时长同算式");
    assert_eq!(
        native.response_elapsed_ms.unwrap() - native.generation_ms.unwrap(),
        519,
        "等待段（ttft）只在响应窗计一次"
    );
    assert_eq!(native.response_sample_ms(), Some(1_325));
    assert!(native.is_response_valid());
    assert!(native.is_tps_valid());
}

/// 可见重试（同 turnStep 多 request）/ 失败 → 不填响应窗（保留既有剔除限制）。
#[test]
fn kimi_retry_and_failed_get_no_response_window() {
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
        // 失败（finishReason=error，单 request 可对齐）
        kimi_llm_request(t - 1_000, "2.1", "kimi-k2.6"),
        kimi_step_end(
            t + 500,
            "u-err",
            "2",
            1,
            "error",
            Some(12),
            Some(300),
            Some(200),
        ),
    ];
    std::fs::write(&wire, lines.join("\n") + "\n").unwrap();

    let batch = kimi::read_incremental(home, &SourceCursor::empty("kimi"), NOW_MS).unwrap();
    assert_eq!(batch.observations.len(), 2);

    let retried = obs_with_key(&batch.observations, "u-retry");
    assert_eq!(
        retried.native_duration_ms, None,
        "既有点：重试不填整调用时长"
    );
    assert_eq!(
        retried.response_elapsed_ms, None,
        "同 turnStep 多 request 不可对齐 → 响应窗不填"
    );
    assert!(!retried.is_response_valid());

    let failed = obs_with_key(&batch.observations, "u-err");
    assert_eq!(failed.outcome, Outcome::Failed);
    assert_eq!(failed.response_elapsed_ms, None, "失败不入响应速度");
    assert!(!failed.is_response_valid());
}

// ===========================================================================
// Claude：响应窗 = 完整 span duration（含等待）
// ===========================================================================

/// 正常记录双窗：响应 = span duration 5000ms（含 1000ms 等待）；生成 = duration −
/// first_content = 4000ms；span 结束之后的工具执行不在窗内。
#[test]
fn claude_response_is_full_span_duration_including_wait() {
    let body = wrap_body(vec![
        llm_span(
            0,
            TRACE_ID,
            0,
            5_000,
            120,
            300,
            Some(1_000),
            1,
            true,
            "glm-5.3-flash",
            "repl_main_thread",
            "end_turn",
        ),
        // span 结束（offset 5000）之后的工具执行：不在模型响应窗内
        tool_span("claude_code.tool.execution", 6_000, 300),
    ]);
    let obs = claude::decode_trace(&body).expect("正例体必须可解码");
    assert_eq!(obs.len(), 1, "工具 span 不产观察");
    let native = &obs[0];

    // 响应窗 = 完整 span duration（含等待；不延伸到后续工具执行）
    assert_eq!(native.response_elapsed_ms, Some(5000));
    // 生成窗照旧：duration − first_content = 4000ms
    assert_eq!(native.generation_ms, Some(4_000));
    assert_eq!(
        native.response_elapsed_ms.unwrap() - native.generation_ms.unwrap(),
        1_000,
        "等待段（first_content 之前）只计入响应窗一次"
    );
    assert!(native.is_response_valid());
    assert!(native.is_tps_valid());
}

/// 缺 first_content（非流式回退）但 duration 可靠 → response-only 有效；
/// 生成窗/首点保持既有的「不借回合值补齐」（None）。
#[test]
fn claude_missing_first_content_is_response_only() {
    let body = wrap_body(vec![llm_span(
        8,
        TRACE_ID,
        0,
        5_000,
        50,
        400,
        None,
        1,
        true,
        "glm-5.3-flash",
        "repl_main_thread",
        "end_turn",
    )]);
    let obs = claude::decode_trace(&body).expect("解码成功");
    assert_eq!(obs.len(), 1);
    let missing_first = &obs[0];
    assert_eq!(missing_first.generation_ms, None);
    assert_eq!(
        missing_first.response_elapsed_ms,
        Some(5_000),
        "完整 span duration 含等待，缺 F 不影响响应窗"
    );
    assert!(missing_first.is_response_valid());
    assert!(!missing_first.is_tps_valid(), "流式口径仍排除（既有点）");
}

/// 已识别重试（attempt=2）/ 失败 / 取消 → 双闸不过 → 无响应窗。
#[test]
fn claude_retry_failed_cancelled_get_no_response_window() {
    let body = wrap_body(vec![
        // attempt=2：逻辑窗含重试，与成功尝试首点基线不同批
        llm_span(
            9,
            TRACE_ID,
            0,
            9_000,
            100,
            300,
            Some(500),
            2,
            true,
            "glm-5.3-flash",
            "repl_main_thread",
            "end_turn",
        ),
        // 失败（success=false）
        llm_span(
            1,
            TRACE_ID,
            60_000,
            3_000,
            10,
            100,
            Some(200),
            1,
            false,
            "glm-5.3-flash",
            "repl_main_thread",
            "api_error",
        ),
        // 取消（stop_reason=interrupted → Cancelled）
        llm_span(
            2,
            TRACE_ID,
            120_000,
            4_000,
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
    let obs = claude::decode_trace(&body).expect("解码成功");
    assert_eq!(obs.len(), 3);

    let retried = obs
        .iter()
        .find(|o| o.native_duration_ms == Some(9_000))
        .unwrap();
    assert_eq!(
        retried.response_elapsed_ms, None,
        "已识别重试不对齐 → 双窗都不填"
    );
    assert!(!retried.is_response_valid());

    let failed = obs.iter().find(|o| o.outcome == Outcome::Failed).unwrap();
    assert_eq!(failed.response_elapsed_ms, None, "失败无响应速度");
    assert!(!failed.is_response_valid());

    let cancelled = obs
        .iter()
        .find(|o| o.outcome == Outcome::Cancelled)
        .unwrap();
    assert_eq!(cancelled.response_elapsed_ms, None, "取消无响应速度");
    assert!(!cancelled.is_response_valid());
}

// ===========================================================================
// OpenCode：响应窗 = 完整 step 窗（started → created）
// ===========================================================================

/// 正常记录双窗：响应 = start 行宿主 started → 终行 created（同 session/message/step
/// 身份）；含等待（900→2000 的 1100ms），不延伸到后续 step 的工具时间。
#[test]
fn opencode_response_is_complete_step_window() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    append_lines(
        home,
        &[
            row(
                "e-s",
                "start",
                1000,
                json!({"started": BASE_MS + 900, "model": "glm-5.3-flash", "provider": "volcengine"}),
            ),
            row(
                "e-d0",
                "delta",
                1500,
                json!({"deltaChars": 0, "hasNonEmptyDelta": false}),
            ),
            row(
                "e-d1",
                "delta",
                2000,
                json!({"deltaChars": 3, "hasNonEmptyDelta": true}),
            ),
            row(
                "e-e",
                "end",
                5000,
                json!({"tokens": 300, "finish": "stop", "tokensReasoning": 0}),
            ),
            // 同 message 的后续 step（工具执行后的下一次模型调用）：不并入前一步窗口
            row(
                "e-s2",
                "start",
                6000,
                json!({"started": BASE_MS + 6000, "model": "glm-5.3-flash", "provider": "volcengine"}),
            ),
            row(
                "e-e2",
                "end",
                7000,
                json!({"tokens": 150, "finish": "stop"}),
            ),
        ],
    );
    let batch = read(home, &empty_cursor());
    assert_eq!(batch.observations.len(), 2);
    let native = obs_with_key(&batch.observations, ":1");

    // 响应窗 = step 窗：started(BASE+900) → created(BASE+5000) = 4100ms
    assert_eq!(native.response_elapsed_ms, Some(4_100));
    // 生成窗照旧：首非空 delta(BASE+2000) → end(BASE+5000) = 3000ms
    assert_eq!(native.generation_ms, Some(3_000));
    assert_eq!(native.started_at_ms, BASE_MS + 900);
    assert_eq!(native.ended_at_ms, BASE_MS + 5_000);
    assert_eq!(
        native.response_elapsed_ms.unwrap() - native.generation_ms.unwrap(),
        1_100,
        "等待段（started → 首非空 delta）只在响应窗计一次"
    );
    assert_eq!(
        native.native_duration_ms, native.response_elapsed_ms,
        "与整调用时长同窗（宿主 started → end）"
    );
    assert!(native.is_response_valid());
    assert!(native.is_tps_valid());
}

/// 已识别未对齐重试 / 失败 / 取消 → 不填响应窗（既判定不变）。
#[test]
fn opencode_retry_failed_cancelled_get_no_response_window() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    append_lines(
        home,
        &[
            // 重试：start → delta → retry → end(attempt=2)
            row(
                "r-s",
                "start",
                1000,
                json!({"started": BASE_MS + 1000, "model": "m", "provider": "p"}),
            ),
            row(
                "r-d",
                "delta",
                2000,
                json!({"deltaChars": 1, "hasNonEmptyDelta": true}),
            ),
            row("r-r", "retry", 2500, json!({"attempt": 2})),
            row(
                "r-e",
                "end",
                5000,
                json!({"tokens": 300, "finish": "stop", "attempt": 2}),
            ),
            // 失败
            row(
                "f-s",
                "start",
                10_000,
                json!({"started": BASE_MS + 10_000, "model": "m", "provider": "p"}),
            ),
            row(
                "f-d",
                "delta",
                11_000,
                json!({"deltaChars": 2, "hasNonEmptyDelta": true}),
            ),
            row("f-e", "failed", 12_000, json!({"finish": "content-filter"})),
            // 取消（MessageAbortedError → cancelled 行）
            row(
                "c-s",
                "start",
                20_000,
                json!({"started": BASE_MS + 20_000, "model": "m", "provider": "p"}),
            ),
            row("c-e", "cancelled", 21_000, json!({})),
        ],
    );
    let batch = read(home, &empty_cursor());
    assert_eq!(batch.observations.len(), 3);

    let retried = obs_with_key(&batch.observations, ":1");
    assert_eq!(retried.outcome, Outcome::Success);
    assert_eq!(
        retried.response_elapsed_ms, None,
        "已识别重试不可对齐 → 跨尝试 step 窗不填"
    );
    assert!(!retried.is_response_valid());

    let failed = obs_with_key(&batch.observations, ":2");
    assert_eq!(failed.outcome, Outcome::Failed);
    assert_eq!(failed.response_elapsed_ms, None, "失败不入响应速度");
    assert!(!failed.is_response_valid());

    let cancelled = obs_with_key(&batch.observations, ":3");
    assert_eq!(cancelled.outcome, Outcome::Cancelled);
    assert_eq!(cancelled.response_elapsed_ms, None, "取消不入响应速度");
    assert!(!cancelled.is_response_valid());
}
