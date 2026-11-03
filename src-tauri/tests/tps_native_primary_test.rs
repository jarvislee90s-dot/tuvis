//! TPS 原生主源（DSH / ZCode）读取集成测试（2026-10-24 计划 Task 3）。
//!
//! 隔离纪律（2026-10-07 事故族）：全部夹具是 tempdir 下的**合成**结构，形态逐字取自
//! 真实脱敏证据（docs/superpowers/evidence/2026-10-24-model-speed/ 下的 dsh-kimi.md、
//! sqlite-sources.md、dsh-output-scope-addendum.md），绝不触真机 `~/.dsh` / `~/.zcode` /
//! `~/.tuvis` —— `read_incremental(home, …)` 以**入参 home 为唯一根**（不认 DSH_HOME /
//! ZCODE_HOME 覆盖），测试传 tempdir 即天然隔离。
//!
//! 覆盖（brief Step1 逐条）：
//! * DSH 紧凑 dt 流首个空 delta 跳过（首 token = 首个非空成员，非 block-start/usage）
//! * DSH 思考计入输出（provider-reported 已含思考 → IncludesReasoning，不再相加）
//! * DSH 同消息 outputTokens 归同一次调用
//! * ZCode 缺 F / 失败 / retry 跳过速度
//! * ZCode 重复代际（同一 logical_request 重放）只计一次
//!
//! （brief Step4/Step5 追加）：最后成功 attempt 可对齐才用起点否则剔除、只读新增不全量
//!   解压、缺字段诊断不报 0、两源正例出有效点、rowid 尾 200 预算、running 水位闸、
//!   陈旧文件不读（24h 新鲜窗）。

use multi_agents_manager_lib::services::tps::model::{
    Outcome, OutputScope, Purpose, SourceCursor, TimingMethod, TpsObservation,
};
use multi_agents_manager_lib::services::tps::sources::{dsh, zcode};
use multi_agents_manager_lib::services::tps::{build_point, PointStatus};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// 固定假时钟基准（纯整数，不读系统时钟；夹具事件时间都锚在它附近）
const BASE_MS: i64 = 1_700_000_000_000;
/// 测试注入的「现在」：事件 + 2 小时（< 24h 保留窗，pending 状态不被剪枝）
const NOW_MS: i64 = BASE_MS + 2 * 60 * 60 * 1000;

// ==== 通用断言助手 ====

/// 单调用 TPS（brief 断言 `x.tps()` 的观测级形态：可入速才得值，否则 None）
fn single_call_tps(o: &TpsObservation) -> Option<f64> {
    if !o.is_tps_valid() {
        return None;
    }
    Some(1000.0 * o.output_tokens.unwrap() as f64 / o.generation_ms.unwrap() as f64)
}

fn obs_with_key<'a>(batch_observations: &'a [TpsObservation], suffix: &str) -> &'a TpsObservation {
    batch_observations
        .iter()
        .find(|o| o.stable_call_key.ends_with(suffix))
        .unwrap_or_else(|| panic!("应存在 stable_call_key 以 {suffix} 结尾的观察"))
}

/// 字符串集合字面量（键集合断言用）
fn str_set(items: &[&str]) -> std::collections::BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

// ==== DSH 夹具（形态 = dsh-kimi.md 的脱敏结构）====

/// DSH 会话目录：`<home>/.dsh/sessions/<project>/<session>/`
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

/// 紧凑段（dsh-kimi.md：time0 + dt[]（长度=成员数−1）+ texts[]；首成员 time0，随后按 dt 累加）
fn compact_chunks(kind: &str, time0: i64, index: i64, dt: &[i64], texts: &[&str]) -> Value {
    json!({"type": kind, "time0": time0, "index": index, "dt": dt, "texts": texts})
}

/// 工具参数紧凑段（dsh-kimi.md：time0、index、dt[]、id、name、args[]）
fn tool_call_chunks(
    time0: i64,
    index: i64,
    dt: &[i64],
    id: &str,
    name: &str,
    args: &[&str],
) -> Value {
    json!({"type": "tool-call-chunks", "time0": time0, "index": index, "dt": dt,
           "id": id, "name": name, "args": args})
}

/// replay profile = dsh-output-scope-addendum.md 固定准入形态
/// （pi-ai / 2 / openai-completions，且 response 的 provider/model 与消息 source 一致）
fn replay_source() -> Value {
    json!({"kind": "pi-ai", "provider": "deepseek", "model": "deepseek-chat",
           "replayState": {"response": {"kind": "pi-ai", "version": 2,
                                        "api": "openai-completions",
                                        "provider": "deepseek", "model": "deepseek-chat",
                                        "responseId": "resp-1", "stopReason": "end_turn"}}})
}

fn plain_source() -> Value {
    json!({"kind": "pi-ai", "provider": "deepseek", "model": "deepseek-chat"})
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

/// 「思考计入」正例：reasoning 首成员为空 delta（time0 处 ""，+40ms 处 "think"），
/// 随后 text 三块；settlement 在 T0+120。首非空 = T0+100 → 生成窗 20ms。
fn dsh_thinking_lines(session: &str) -> Vec<String> {
    let t0 = BASE_MS;
    vec![
        dsh_header(session),
        dsh_event("turn/start", 1, t0 + 10, json!({"turn": 1})),
        dsh_event("step/start", 2, t0 + 20, json!({"turn": 1, "step": 1})),
        dsh_message_event(
            3,
            t0 + 120,
            1,
            1,
            Some(96),
            vec![
                // 首个成员是空 delta：必须跳过，不得当首 token（也不是 block-start/usage）
                compact_chunks("reasoning-chunks", t0 + 60, 0, &[40], &["", "think"]),
                compact_chunks("text-chunks", t0 + 110, 1, &[5, 4], &["hel", "lo", "!"]),
                // chunk 型记录是 block-start/usage/finish 标记，不参与首 token
                json!({"type": "chunk", "time": t0 + 115,
                       "chunk": {"type": "usage", "index": 1, "blockType": "text"}}),
            ],
            replay_source(),
        ),
        // step/end 在结算之后（含工具的 step 终结 ≠ 模型终结）
        dsh_event("step/end", 4, t0 + 130, json!({"turn": 1, "step": 1})),
    ]
}

fn write_session_file(dir: &Path, name: &str, lines: &[String]) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    path
}

fn write_session_frames(dir: &Path, name: &str, batches: &[Vec<String>]) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    let mut bytes = Vec::new();
    for b in batches {
        // 每个追加批次独立一帧（monitor/dsh/decode.rs 的容器形态）
        bytes.extend_from_slice(
            &zstd::stream::encode_all(b.join("\n").add_nl().as_bytes(), 3).unwrap(),
        );
    }
    std::fs::write(&path, bytes).unwrap();
    path
}

trait AddNl {
    fn add_nl(&self) -> String;
}
impl AddNl for String {
    fn add_nl(&self) -> String {
        format!("{self}\n")
    }
}

// ==== ZCode 夹具（形态 = sqlite-sources.md 的脱敏列名与数值）====

fn zcode_db_path(home: &Path) -> PathBuf {
    home.join(".zcode").join("cli").join("db").join("db.sqlite")
}

/// 真机脱敏列名建表（sqlite-sources.md §「粒度和关联」/「计时字段与质量」；
/// 列集是证据文档点名的子集，SELECT 按列名对位，与真机多余列无关）
fn zcode_create_db(home: &Path) -> Connection {
    let path = zcode_db_path(home);
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

/// 证据文档（sqlite-sources.md「脱敏原生例子」）的成功行：
/// t=0 开始 / +6259 首 token / +7757 完成，output 96 / reasoning 0 → 生成窗 1498ms
const ZC_START: i64 = BASE_MS + 5_000;
const ZC_FIRST: i64 = ZC_START + 6_259;
const ZC_DONE: i64 = ZC_START + 7_757;

/// zcode 游标 JSON → (watermark, op_completed_at, op_rowid)（F2 起机会游标为
/// (completed_at, rowid) 完成时间复合游标）
fn zcode_cursor_of(cursor: &SourceCursor) -> (i64, i64, i64) {
    let v: Value = serde_json::from_str(&cursor.value).unwrap();
    (
        v["watermark"].as_i64().unwrap_or(0),
        v["op_completed_at"].as_i64().unwrap_or(0),
        v["op_rowid"].as_i64().unwrap_or(0),
    )
}

// ===========================================================================
// DSH 用例
// ===========================================================================

/// 紧凑 dt 首空跳过 + 思考计入 + 同消息 Token + 模型终结（结算）为终点
#[test]
fn dsh_compact_dt_first_empty_skip_thinking_counted_model_end() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let sess = dsh_session_dir(home, "sess-1");
    write_session_file(&sess, "session.v3.jsonl", &dsh_thinking_lines("sess-1"));

    let batch = dsh::read_incremental(home, &SourceCursor::empty("dsh"), NOW_MS).unwrap();
    assert!(batch.source_state.ok);
    assert_eq!(batch.observations.len(), 1, "恰一条消息观察");
    let o = &batch.observations[0];

    // 首空 delta 跳过：首 token = T0+100（首个非空成员），不是 T0+60
    assert_eq!(o.first_token_at_ms, Some(BASE_MS + 100));
    // 生成窗 = 结算 − 首非空 = 20ms；不是从空 delta 起算的 60ms
    assert_eq!(o.generation_ms, Some(20));
    // 思考计入输出：usage.outputTokens 原样（provider-reported 已含思考 → 不再相加），
    // 且范围标 IncludesReasoning（addendum 固定 profile 准入）
    assert_eq!(o.output_tokens, Some(96));
    assert_eq!(o.output_scope, OutputScope::IncludesReasoning);
    // 模型终结：ended = 消息结算 T0+120，而不是含工具的 step/end T0+130
    assert_eq!(o.ended_at_ms, BASE_MS + 120);
    assert_eq!(o.started_at_ms, BASE_MS + 20);
    assert_eq!(o.native_duration_ms, Some(100));
    assert_eq!(o.ttft_ms(), Some(80));
    assert_eq!(o.outcome, Outcome::Success);
    assert_eq!(o.method, TimingMethod::StreamObserved);
    assert_eq!(o.model, "deepseek-chat");
    assert_eq!(o.session_key.as_deref(), Some("sess-1"));
    assert_eq!(o.turn_key.as_deref(), Some("1"));
    assert_eq!(o.purpose, Purpose::MainTask);
    assert_eq!(single_call_tps(o), Some(4800.0));
}

/// 同消息 outputTokens 归同一次调用：两步两消息各拿各的数
#[test]
fn dsh_same_message_tokens_stay_per_call() {
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
                t0 + 110,
                0,
                &[5],
                &["a", "b"],
            )],
            replay_source(),
        ),
        dsh_event("step/start", 5, t0 + 200, json!({"turn": 1, "step": 2})),
        dsh_message_event(
            6,
            t0 + 300,
            1,
            2,
            Some(7),
            vec![compact_chunks(
                "text-chunks",
                t0 + 290,
                0,
                &[5],
                &["x", "y"],
            )],
            replay_source(),
        ),
    ];
    let sess = dsh_session_dir(home, "sess-1");
    write_session_file(&sess, "session.v3.jsonl", &lines);

    let batch = dsh::read_incremental(home, &SourceCursor::empty("dsh"), NOW_MS).unwrap();
    assert_eq!(batch.observations.len(), 2);
    let o1 = obs_with_key(&batch.observations, ":1:1");
    let o2 = obs_with_key(&batch.observations, ":1:2");
    // 各归各的同消息 usage，不串、不累计
    assert_eq!(o1.output_tokens, Some(96));
    assert_eq!(o2.output_tokens, Some(7));
    assert_eq!(o1.generation_ms, Some(10));
    assert_eq!(o2.generation_ms, Some(10));
}

/// 工具参数紧凑段参与首 token（Spec §4：首非空「思考/正文/参数」至结算）
#[test]
fn dsh_tool_param_chunks_count_as_first_token() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let t0 = BASE_MS;
    let lines = vec![
        dsh_header("sess-1"),
        dsh_event("step/start", 2, t0 + 20, json!({"turn": 1, "step": 1})),
        // 流只有 tool-call-chunks：首非空参数块 = 首 token；空参数 delta 跳过
        dsh_message_event(
            3,
            t0 + 200,
            1,
            1,
            Some(64),
            vec![tool_call_chunks(
                t0 + 100,
                0,
                &[50, 10],
                "call-1",
                "Bash",
                &["", r#"{"x":"#, "1}"],
            )],
            replay_source(),
        ),
    ];
    let sess = dsh_session_dir(home, "sess-1");
    write_session_file(&sess, "session.v3.jsonl", &lines);

    let batch = dsh::read_incremental(home, &SourceCursor::empty("dsh"), NOW_MS).unwrap();
    assert_eq!(batch.observations.len(), 1);
    let o = &batch.observations[0];
    // 首 token = 首个非空参数块（t0+150），不是空 delta（t0+100）
    assert_eq!(o.first_token_at_ms, Some(t0 + 150));
    // 两个非空参数块分布在两个时刻（+150/+160）→ 正生成窗
    assert_eq!(o.generation_ms, Some(50));
    assert_eq!(o.output_scope, OutputScope::IncludesReasoning);
}

/// 代际重复只读最优一代：v2（plain）+ v3（zstd 同内容并存）与单 v3 的观察集逐键相等
#[test]
fn dsh_replayed_generations_count_once() {
    // 两个独立 home：各只放一个会话，读数不受另一会话干扰
    let once_home = tempfile::tempdir().unwrap();
    // 一次读取：只有 v3
    let once_dir = dsh_session_dir(once_home.path(), "sess-1");
    write_session_frames(
        &once_dir,
        "session.v3.jsonl.zstd",
        &[dsh_thinking_lines("sess-1")],
    );
    let once =
        dsh::read_incremental(once_home.path(), &SourceCursor::empty("dsh"), NOW_MS).unwrap();

    // 重放读取：同内容 v2（plain，旧代际）+ v3（zstd，新代际）并存
    let replay_home = tempfile::tempdir().unwrap();
    let replay_dir = dsh_session_dir(replay_home.path(), "sess-1");
    write_session_file(
        &replay_dir,
        "session.v2.jsonl",
        &dsh_thinking_lines("sess-1"),
    );
    write_session_frames(
        &replay_dir,
        "session.v3.jsonl.zstd",
        &[dsh_thinking_lines("sess-1")],
    );
    let replayed =
        dsh::read_incremental(replay_home.path(), &SourceCursor::empty("dsh"), NOW_MS).unwrap();

    // brief：assert_eq!(dsh_replayed.len(), dsh_once.len())
    assert_eq!(replayed.observations.len(), once.observations.len());
    let once_keys: std::collections::BTreeSet<_> = once
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    let replay_keys: std::collections::BTreeSet<_> = replayed
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    assert_eq!(
        once_keys.len(),
        replay_keys.len(),
        "同会话历史拷贝不得重复累计"
    );
}

/// 最后成功 attempt 可对齐才用其起点：step 内有 attempt（重试）→ 消息速度剔除（不猜），
/// 首个 attempt 以 step/start 为起点出 Failed 观察（可靠起止面）
#[test]
fn dsh_retry_without_alignable_start_removes_speed() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let t0 = BASE_MS;
    let lines = vec![
        dsh_header("sess-1"),
        dsh_event("step/start", 2, t0 + 20, json!({"turn": 1, "step": 1})),
        // 无表面消息的尝试结算（finish.kind=error）—— step 从此带重试
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
    assert_eq!(
        batch.observations.len(),
        2,
        "attempt(Failed) + 消息(Success) 各一条"
    );

    let attempt = batch
        .observations
        .iter()
        .find(|o| o.outcome == Outcome::Failed)
        .unwrap();
    // 首个 attempt 的起点 = step/start（可对齐）
    assert_eq!(attempt.started_at_ms, t0 + 20);
    assert_eq!(attempt.ended_at_ms, t0 + 500);
    assert!(attempt.failure_reason.is_some());

    let msg = batch
        .observations
        .iter()
        .find(|o| o.outcome == Outcome::Success)
        .unwrap();
    // 无法定位最后成功尝试的起点 → 剔除该调用速度（不猜），但保留事实面
    assert_eq!(msg.generation_ms, None, "重试后成功的调用速度必须剔除");
    assert_eq!(
        msg.native_duration_ms, None,
        "跨 attempt 的时长不是单次调用时长"
    );
    assert_eq!(msg.first_token_at_ms, None, "起点不可对齐 → TTFT 不可用");
    assert_eq!(msg.output_tokens, Some(96), "输出事实保留（不造 0）");
    assert!(!msg.is_tps_valid());
    assert!(
        batch.diagnostics.iter().any(|d| d.contains("retries")),
        "剔除原因必须进诊断：{:?}",
        batch.diagnostics
    );
}

/// 增量只读新增：zstd 每批一帧，第二轮只解新帧；同游标重读为空（游标只读、幂等）
#[test]
fn dsh_incremental_reads_only_new_tail() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let t0 = BASE_MS;
    let sess = dsh_session_dir(home, "sess-1");
    let path = write_session_frames(
        &sess,
        "session.v3.jsonl.zstd",
        &[
            // 帧 1：header + step1 + 消息1
            vec![
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
                        t0 + 110,
                        0,
                        &[5],
                        &["a", "b"],
                    )],
                    replay_source(),
                ),
            ],
        ],
    );

    let batch1 = dsh::read_incremental(home, &SourceCursor::empty("dsh"), NOW_MS).unwrap();
    assert_eq!(batch1.observations.len(), 1);
    let cursor1 = batch1.next_cursor.clone();
    assert_ne!(cursor1.value, "", "首轮后游标应已落位");

    // 追加帧 2（真实 dsh 追加形态：新批次 = 新帧接在文件尾）
    let frame2 = [
        dsh_event("step/start", 5, t0 + 200, json!({"turn": 1, "step": 2})),
        dsh_message_event(
            6,
            t0 + 300,
            1,
            2,
            Some(7),
            vec![compact_chunks(
                "text-chunks",
                t0 + 290,
                0,
                &[5],
                &["x", "y"],
            )],
            replay_source(),
        ),
    ];
    let mut bytes = std::fs::read(&path).unwrap();
    bytes.extend_from_slice(
        &zstd::stream::encode_all(frame2.join("\n").add_nl().as_bytes(), 3).unwrap(),
    );
    std::fs::write(&path, bytes).unwrap();

    let batch2 = dsh::read_incremental(home, &cursor1, NOW_MS).unwrap();
    assert_eq!(batch2.observations.len(), 1, "第二轮只读新增帧");
    assert_eq!(batch2.observations[0].stable_call_key, "sess-1:1:2");

    // 用推进后的游标重读：空批（不重复计数）；入参 cursor1 只读（值未被改写）
    let cursor2 = batch2.next_cursor.clone();
    let batch2_again = dsh::read_incremental(home, &cursor2, NOW_MS).unwrap();
    assert_eq!(batch2_again.observations.len(), 0);
    assert_eq!(cursor2.value, batch2_again.next_cursor.value);
}

/// 尾帧撕裂（EOF 打断，设计内常态）：水位不越过撕裂帧；帧补全后恰读一次
#[test]
fn dsh_torn_tail_frame_does_not_advance_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let t0 = BASE_MS;
    let sess = dsh_session_dir(home, "sess-1");
    let frame1 = vec![
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
                t0 + 110,
                0,
                &[5],
                &["a", "b"],
            )],
            replay_source(),
        ),
    ];
    let frame2 = [
        dsh_event("step/start", 5, t0 + 200, json!({"turn": 1, "step": 2})),
        dsh_message_event(
            6,
            t0 + 300,
            1,
            2,
            Some(7),
            vec![compact_chunks(
                "text-chunks",
                t0 + 290,
                0,
                &[5],
                &["x", "y"],
            )],
            replay_source(),
        ),
    ];
    let path = write_session_frames(&sess, "session.v3.jsonl.zstd", &[frame1]);
    let batch1 = dsh::read_incremental(home, &SourceCursor::empty("dsh"), NOW_MS).unwrap();
    assert_eq!(batch1.observations.len(), 1);
    let cursor1 = batch1.next_cursor.clone();

    // 追加帧 2 并截断（模拟 EOF 打断的撕裂尾帧）
    let mut bytes = std::fs::read(&path).unwrap();
    let full_frame2 = zstd::stream::encode_all(frame2.join("\n").add_nl().as_bytes(), 3).unwrap();
    bytes.extend_from_slice(&full_frame2[..full_frame2.len() - 5]);
    std::fs::write(&path, bytes).unwrap();

    let batch_torn = dsh::read_incremental(home, &cursor1, NOW_MS).unwrap();
    assert_eq!(batch_torn.observations.len(), 0, "撕裂尾帧不出观察");
    // 水位未越过撕裂帧（下轮重解整个尾帧）
    assert_eq!(
        batch_torn.next_cursor.value, cursor1.value,
        "撕裂帧不推进水位"
    );

    // 尾帧补全 → 恰读一次，不丢不重
    let mut bytes = std::fs::read(&path).unwrap();
    bytes.truncate(bytes.len() - (full_frame2.len() - 5));
    bytes.extend_from_slice(&full_frame2);
    std::fs::write(&path, bytes).unwrap();
    let batch2 = dsh::read_incremental(home, &batch_torn.next_cursor, NOW_MS).unwrap();
    assert_eq!(batch2.observations.len(), 1, "补全后恰读一次");
    assert_eq!(batch2.observations[0].stable_call_key, "sess-1:1:2");
}

/// 字段缺失返回诊断不报 0：缺 usage → output=None（不是 0）；缺 step/start → 不产观察
///（起点不可造）。独立复核 R8（Spec §4「不是只认旧稿的单一 replay profile」）：无
/// pi-ai/2/openai-completions profile 但同调用时间/输出完整的消息**仍是有效样本**——输出按
/// provider-reported 如实标「思考计数未单独声明」（不强行标已含思考），生成窗照常；真正缺
/// 可靠生成窗（如单时刻整块突发）才排除速度。
#[test]
fn dsh_missing_fields_yield_diagnostics_not_zero() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let t0 = BASE_MS;
    let lines = vec![
        dsh_header("sess-1"),
        dsh_event("step/start", 2, t0 + 20, json!({"turn": 1, "step": 1})),
        // m1：缺 usage.outputTokens
        dsh_message_event(
            3,
            t0 + 120,
            1,
            1,
            None,
            vec![compact_chunks(
                "text-chunks",
                t0 + 110,
                0,
                &[5],
                &["a", "b"],
            )],
            replay_source(),
        ),
        dsh_event("step/start", 5, t0 + 200, json!({"turn": 1, "step": 2})),
        // m2：source 无 replayState（profile 不符）但时间/输出完整 → 有效，provider-reported 范围
        dsh_message_event(
            6,
            t0 + 300,
            1,
            2,
            Some(7),
            vec![compact_chunks(
                "text-chunks",
                t0 + 290,
                0,
                &[5],
                &["x", "y"],
            )],
            plain_source(),
        ),
        dsh_event("step/start", 7, t0 + 350, json!({"turn": 1, "step": 4})),
        // m4：无 profile 且非空流成员只在一个时刻（整块突发）→ 真正缺可靠生成窗，排除速度
        dsh_message_event(
            9,
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
            plain_source(),
        ),
        // m3：step 3 没有 step/start（起点缺失 → 跳过 + 诊断，不造起点）
        dsh_message_event(
            8,
            t0 + 400,
            1,
            3,
            Some(9),
            vec![compact_chunks(
                "text-chunks",
                t0 + 390,
                0,
                &[5],
                &["p", "q"],
            )],
            replay_source(),
        ),
    ];
    let sess = dsh_session_dir(home, "sess-1");
    write_session_file(&sess, "session.v3.jsonl", &lines);

    let batch = dsh::read_incremental(home, &SourceCursor::empty("dsh"), NOW_MS).unwrap();
    assert_eq!(batch.observations.len(), 3, "缺起点的消息不产观察");

    let m1 = obs_with_key(&batch.observations, ":1:1");
    assert_eq!(m1.output_tokens, None, "缺 usage 是缺失，不得伪装成 0");
    assert!(!m1.is_tps_valid());

    let m2 = obs_with_key(&batch.observations, ":1:2");
    assert_eq!(
        m2.output_scope,
        OutputScope::ProviderReported,
        "无 profile 的 provider-reported 输出如实标「思考计数未单独声明」，不标已含思考也不标无事实"
    );
    assert_eq!(
        m2.generation_ms,
        Some(10),
        "可靠生成窗照常：t0+300 − 首非空 t0+290"
    );
    assert_eq!(m2.output_tokens, Some(7));
    assert!(
        m2.is_tps_valid(),
        "无旧 profile 但时间/输出完整 = 有效样本（Spec §4）"
    );

    let m4 = obs_with_key(&batch.observations, ":1:4");
    assert_eq!(
        m4.generation_ms, None,
        "单时刻整块突发 = 无可靠生成窗 → 排除速度"
    );
    assert!(!m4.is_tps_valid());
    assert_eq!(m4.output_scope, OutputScope::ProviderReported);

    assert!(
        !batch.diagnostics.is_empty(),
        "三类缺失都必须留下诊断：{:?}",
        batch.diagnostics
    );
    assert!(batch.diagnostics.iter().any(|d| d.contains("outputTokens")));
    assert!(batch.diagnostics.iter().any(|d| d.contains("profile")));
    assert!(batch.diagnostics.iter().any(|d| d.contains("step/start")));
}

/// 正例出有效点：读取产物进 build_point 出 value 点（Σ/Σ 口径）
#[test]
fn dsh_positive_example_builds_value_point() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let sess = dsh_session_dir(home, "sess-1");
    write_session_file(&sess, "session.v3.jsonl", &dsh_thinking_lines("sess-1"));

    let batch = dsh::read_incremental(home, &SourceCursor::empty("dsh"), NOW_MS).unwrap();
    let valid: Vec<_> = batch
        .observations
        .iter()
        .filter(|o| o.is_tps_valid())
        .collect();
    assert_eq!(valid.len(), 1, "恰一条正例入速");

    let at = batch.observations[0].ended_at_ms + 1_000;
    let points = build_point(&batch.observations, &[], "run-1", at);
    assert_eq!(points.len(), 1);
    let p = &points[0];
    assert_eq!(p.status, PointStatus::Value);
    assert_eq!(p.sample_count, 1);
    assert_eq!(p.sum_output_tokens, 96);
    assert_eq!(p.sum_generation_ms, 20);
    let tps = p.tps.unwrap();
    assert!((tps - 4800.0).abs() < 1e-9, "1000×96/20 = 4800，实得 {tps}");
}

/// 24h 新鲜窗（L3 预算）：陈旧且无游标的会话文件不打开不解码；触新后才读
#[test]
fn dsh_stale_session_file_is_not_read() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let sess = dsh_session_dir(home, "sess-old");
    let path = write_session_file(&sess, "session.v3.jsonl", &dsh_thinking_lines("sess-old"));

    // 真钟「现在」（毫秒）
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let set_mtime = |ms: i64| {
        let f = std::fs::File::options().write(true).open(&path).unwrap();
        f.set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms as u64)),
        )
        .unwrap();
    };
    // mtime 拨到 25 小时前，now_ms 也用真钟 → 超窗且无游标 → 不读
    set_mtime(now - 25 * 60 * 60 * 1000);
    let batch = dsh::read_incremental(home, &SourceCursor::empty("dsh"), now).unwrap();
    assert_eq!(
        batch.observations.len(),
        0,
        "超窗且无游标的会话不读（不全量解压历史）"
    );

    // 触新（mtime 拉回现在）→ 正常读取
    set_mtime(now + 1_000);
    let batch = dsh::read_incremental(home, &SourceCursor::empty("dsh"), now).unwrap();
    assert_eq!(batch.observations.len(), 1, "触新后照常读取");
}

/// R3'/NB1（评审追记）：plain 会话里的超大单行（≥ 8 MiB 尾读上限仍无换行）必须走**有界
/// 分块扫描**（扫描位置持久化在游标 oversized_upto），绝不整余量重读、绝不把超大行字节当
/// 事件解析；**只有整块无换行才起扫**（块内有换行 = 块尾只是骑界行，不起扫）。确定性
/// 设计：超大行恰 9 MiB（上限 8 MiB）→
/// * 第 1 轮：读界截在超大行中部，块内含前缀换行 → 只消费合法前缀（1 条观察），不起扫；
/// * 第 2 轮（文件未变）：从超大行行首读满 8 MiB 无任何换行 → 起扫（oversized_upto =
///   行首 + 8 MiB，留「bounded chunks」诊断，水位不在文件尾）；
/// * 第 3 轮（文件未变）：续扫余下 1 MiB 找到换行 → 跳过超大行（留「skipped」诊断）+
///   消费其后的合法行（第 2 条观察）；超大行的字节从未进 JSON 解析（无解析失败诊断）。
#[test]
fn dsh_oversized_plain_line_is_skipped_in_bounded_chunks_and_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let t0 = BASE_MS;
    // 前缀：合法头 + step/start + 一条完整消息
    let prefix = [
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
                t0 + 110,
                0,
                &[5],
                &["a", "b"],
            )],
            replay_source(),
        ),
    ];
    // 超大行：恰 9 MiB 无换行（尾部带换行，其后还有一条合法事件行）
    let oversized_line: String = "x".repeat(9 * 1024 * 1024);
    let suffix = [
        dsh_event("step/start", 5, t0 + 200, json!({"turn": 2, "step": 1})),
        dsh_message_event(
            6,
            t0 + 300,
            2,
            1,
            Some(7),
            vec![compact_chunks(
                "text-chunks",
                t0 + 290,
                0,
                &[5],
                &["x", "y"],
            )],
            replay_source(),
        ),
    ];
    let sess = dsh_session_dir(home, "sess-1");
    std::fs::create_dir_all(&sess).unwrap();
    let path = sess.join("session.v3.jsonl");
    let mut body = prefix.join("\n") + "\n";
    body += &oversized_line;
    body += "\n";
    body += &(suffix.join("\n") + "\n");
    std::fs::write(&path, &body).unwrap();
    let file_len = body.len() as u64;
    let dir_key = sess.to_string_lossy().to_string();

    // 游标 JSON 里的 per-file 位置（oversized_upto / offset 是「每轮实际工作量」的代理断言面）
    let pos_of = |cursor: &SourceCursor| -> (u64, u64) {
        let v: Value = serde_json::from_str(&cursor.value).unwrap();
        let p = &v["sessions"][&dir_key];
        (
            p["oversized_upto"].as_u64().unwrap_or(0),
            p["offset"].as_u64().unwrap_or(0),
        )
    };

    // 第 1 轮：读界截在超大行中部，但块内含合法前缀的换行 → 只消费前缀，**不起扫**
    //（NB1：块内有换行 = 块尾只是骑界行；超大行判定 = 整块无换行）
    let batch1 = dsh::read_incremental(home, &SourceCursor::empty("dsh"), NOW_MS).unwrap();
    let keys1: std::collections::BTreeSet<_> = batch1
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    assert_eq!(keys1, str_set(&["sess-1:1:1"]), "第 1 轮只消费合法前缀");
    let (upto0, offset0) = pos_of(&batch1.next_cursor);
    assert_eq!(upto0, 0, "块内有换行：不起扫");
    assert!(offset0 > 0 && offset0 < file_len, "水位停在超大行行首");

    // 第 2 轮（文件未变）：从超大行行首读满 8 MiB 仍无任何换行 → 真·超大行，起扫
    let batch2 = dsh::read_incremental(home, &batch1.next_cursor, NOW_MS).unwrap();
    assert!(
        batch2.observations.is_empty(),
        "超大行扫描轮不产观察: {:?}",
        batch2.observations.len()
    );
    assert!(
        batch2
            .diagnostics
            .iter()
            .any(|d| d.contains("single line exceeds tail read cap")),
        "超大单行必须留「有界分块前扫」诊断: {:?}",
        batch2.diagnostics
    );
    let (oversized_upto1, offset1) = pos_of(&batch2.next_cursor);
    assert_eq!(
        oversized_upto1,
        offset1 + super_cap(),
        "扫描位置 = base + 8 MiB 有界前扫的终点（不是整余量）"
    );
    assert!(
        offset1 < file_len,
        "起扫轮后水位不在文件尾（超大行未消费、未跳过完成）"
    );

    // 第 3 轮（文件未变）：续扫余下 1 MiB 找到换行 → 跳过超大行 + 消费其后合法行
    let batch3 = dsh::read_incremental(home, &batch2.next_cursor, NOW_MS).unwrap();
    let keys3: std::collections::BTreeSet<_> = batch3
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    assert_eq!(
        keys3,
        str_set(&["sess-1:2:1"]),
        "第 3 轮恰消费超大行之后的合法行（不重放前缀、不解析超大行）"
    );
    assert!(
        batch3
            .diagnostics
            .iter()
            .any(|d| d.contains("oversized line skipped")),
        "跳过完成必须留诊断: {:?}",
        batch3.diagnostics
    );
    assert!(
        !batch3.diagnostics.iter().any(|d| d.contains("JSON")),
        "超大行的字节绝不进事件解析（无解析失败诊断）: {:?}",
        batch3.diagnostics
    );
    let (oversized_upto2, offset2) = pos_of(&batch3.next_cursor);
    assert_eq!(oversized_upto2, 0, "扫描完成后扫描状态归零");
    assert_eq!(offset2, file_len, "水位恰落在文件尾（跳过整行 + 消费尾行）");

    // 第 4 轮：重读为空（不重复计数）
    let batch4 = dsh::read_incremental(home, &batch3.next_cursor, NOW_MS).unwrap();
    assert!(batch4.observations.is_empty(), "到尾后重读为空");
}

/// B1（三轮评审）：扫描步进必须留 3 字节重叠——下一帧魔数若骑在块尾最后 1–3 字节，
/// `frame_offsets`（4 字节整窗检测）本轮不可见；步进若推到块尾，下轮从魔数序列内部
/// 起扫，该帧永不再被找到 → 被并进「巨帧跳过」静默丢失。夹具：f2 的魔数恰好落在
/// 第一个 8 MiB 窗口的倒数第 2 字节起。
#[test]
fn dsh_zstd_scan_overlap_finds_magic_straddling_window_edge() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let t0 = BASE_MS;
    let sess = dsh_session_dir(home, "sess-b");
    let frame1 = [
        dsh_header("sess-b"),
        dsh_event("step/start", 2, t0 + 20, json!({"turn": 1, "step": 1})),
        dsh_message_event(
            3,
            t0 + 120,
            1,
            1,
            Some(96),
            vec![compact_chunks(
                "text-chunks",
                t0 + 110,
                0,
                &[5],
                &["a", "b"],
            )],
            replay_source(),
        ),
    ];
    let frame2 = [
        dsh_event("step/start", 5, t0 + 200, json!({"turn": 2, "step": 1})),
        dsh_message_event(
            6,
            t0 + 300,
            2,
            1,
            Some(7),
            vec![compact_chunks(
                "text-chunks",
                t0 + 290,
                0,
                &[5],
                &["x", "y"],
            )],
            replay_source(),
        ),
    ];
    let f1 = zstd::stream::encode_all(frame1.join("\n").add_nl().as_bytes(), 3).unwrap();
    let f2 = zstd::stream::encode_all(frame2.join("\n").add_nl().as_bytes(), 3).unwrap();
    let bomb =
        zstd::stream::encode_all(lcg_nibbles(12 * 1024 * 1024, 0x5EED).as_slice(), 1).unwrap();
    // 垫字节（半字节流高半字节恒 0，凑不出魔数首字节 0x28）：把 f2 的魔数精确顶到
    // 「炸弹轮读窗」的界上（F1 后炸弹从帧 1 末尾起以足额预算分类，读窗 =
    // [f1_end, f1_end + 8 MiB)）——窗最后 3 字节 = 魔数前 2 字节 + 魔数首字节……
    // 即魔数骑在窗界上
    let f1_end = f1.len() as u64;
    let magic_at = f1_end + super_cap() - 2;
    let junk_len = (magic_at as usize) - f1.len() - bomb.len();
    assert!(junk_len > 0, "夹具前提：炸弹压缩体短于首个读窗");
    let junk = lcg_nibbles(junk_len, 0xA11CE);
    std::fs::create_dir_all(&sess).unwrap();
    let path = sess.join("session.v3.jsonl.zstd");
    let mut bytes = f1.clone();
    let bomb_start = bytes.len() as u64;
    bytes.extend_from_slice(&bomb);
    bytes.extend_from_slice(&junk);
    bytes.extend_from_slice(&f2);
    std::fs::write(&path, &bytes).unwrap();
    let file_len = bytes.len() as u64;
    let dir_key = sess.to_string_lossy().to_string();
    let pos_of = |cursor: &SourceCursor| -> (u64, u64) {
        let v: Value = serde_json::from_str(&cursor.value).unwrap();
        let p = &v["sessions"][&dir_key];
        (
            p["oversized_upto"].as_u64().unwrap_or(0),
            p["offset"].as_u64().unwrap_or(0),
        )
    };

    // 第 1 轮：炸弹帧只拿到「剩余预算」（< 单帧上限）→ 不当场判炸弹（余量截断 ≠
    // 单帧超限）：帧 1 观察照常，水位停帧 1 末尾，留预算耗尽诊断（F1）
    let batch1 = dsh::read_incremental(home, &SourceCursor::empty("dsh"), NOW_MS).unwrap();
    let keys1: std::collections::BTreeSet<_> = batch1
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    assert_eq!(keys1, str_set(&["sess-b:1:1"]));
    assert!(
        batch1
            .diagnostics
            .iter()
            .any(|d| d.contains("batch decode budget exhausted")),
        "本轮余量截断必须留预算诊断: {:?}",
        batch1.diagnostics
    );
    assert_eq!(
        pos_of(&batch1.next_cursor),
        (0, f1_end),
        "水位停在炸弹帧起点（帧 1 末尾）、无扫描态"
    );

    // 第 2 轮（文件未变）：足额预算的读窗截在炸弹压缩流体内、骑界魔数只露前 2 字节
    //（i + 4 > 窗长不可见）→ 检测进扫描态；扫描位必须留在读界 − 3（3 字节重叠），
    // 让骑界魔数完整落在下一窗内
    let batch2 = dsh::read_incremental(home, &batch1.next_cursor, NOW_MS).unwrap();
    assert!(
        batch2
            .diagnostics
            .iter()
            .any(|d| d.contains("beyond decode budget")),
        "足额预算下解压超预算必须留诊断: {:?}",
        batch2.diagnostics
    );
    assert_eq!(
        pos_of(&batch2.next_cursor),
        (f1_end + super_cap() - 3, bomb_start),
        "扫描位 = 炸弹轮读界 − 3（重叠），水位停在帧起点"
    );

    // 第 3 轮（文件未变）：骑界魔数在第二窗可见 → 定位帧尾、越过、消费帧 2
    let batch3 = dsh::read_incremental(home, &batch2.next_cursor, NOW_MS).unwrap();
    let keys3: std::collections::BTreeSet<_> = batch3
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    assert_eq!(
        keys3,
        str_set(&["sess-b:2:1"]),
        "骑界魔数的帧必须被找到并消费（旧步进下永久丢失）"
    );
    assert_eq!(pos_of(&batch3.next_cursor), (0, file_len));

    // 第 4 轮：重读为空
    let batch4 = dsh::read_incremental(home, &batch3.next_cursor, NOW_MS).unwrap();
    assert!(batch4.observations.is_empty(), "到尾后重读为空");
}

/// NB3（二轮评审）：文件收缩到水位与扫描位**之间**（offset ≤ size < oversized_upto）
/// 同样必须按收缩重置——否则扫描位恒在文件尾之外，每轮便宜返回，状态永久卡死。
#[test]
fn dsh_shrink_below_scan_position_resets_state() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let sess = dsh_session_dir(home, "sess-s");
    std::fs::create_dir_all(&sess).unwrap();
    let path = sess.join("session.v3.jsonl");
    // 文件 1：一条 9 MiB 无换行大行（第 1 轮读满 8 MiB 无换行 → 起扫，offset = 0）
    std::fs::write(&path, format!("{}\n", "x".repeat(9 * 1024 * 1024))).unwrap();
    let batch1 = dsh::read_incremental(home, &SourceCursor::empty("dsh"), NOW_MS).unwrap();
    let dir_key = sess.to_string_lossy().to_string();
    let pos_of = |cursor: &SourceCursor| -> (u64, u64) {
        let v: Value = serde_json::from_str(&cursor.value).unwrap();
        let p = &v["sessions"][&dir_key];
        (
            p["oversized_upto"].as_u64().unwrap_or(0),
            p["offset"].as_u64().unwrap_or(0),
        )
    };
    assert_eq!(
        pos_of(&batch1.next_cursor),
        (super_cap(), 0),
        "夹具前提：扫描进行中（offset = 0，扫描位 = 8 MiB）"
    );

    // 文件 2：重写为小文件（size > offset 但 < oversized_upto——旧收缩守卫判不到）
    let t0 = BASE_MS;
    let fresh = [
        dsh_header("sess-s"),
        dsh_event("step/start", 2, t0 + 20, json!({"turn": 1, "step": 1})),
        dsh_message_event(
            3,
            t0 + 120,
            1,
            1,
            Some(96),
            vec![compact_chunks(
                "text-chunks",
                t0 + 110,
                0,
                &[5],
                &["a", "b"],
            )],
            replay_source(),
        ),
    ];
    let new_body = fresh.join("\n") + "\n";
    std::fs::write(&path, &new_body).unwrap();
    let new_len = new_body.len() as u64;
    assert!(new_len < super_cap(), "夹具前提：新文件小于扫描位");

    // 第 2 轮：必须识别收缩 → 重置扫描态 → 从头重读新内容（稳定键幂等，不双计）
    let batch2 = dsh::read_incremental(home, &batch1.next_cursor, NOW_MS).unwrap();
    assert!(
        batch2.diagnostics.iter().any(|d| d.contains("收缩")),
        "收缩必须留诊断: {:?}",
        batch2.diagnostics
    );
    let keys2: std::collections::BTreeSet<_> = batch2
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    assert_eq!(
        keys2,
        str_set(&["sess-s:1:1"]),
        "收缩后的新内容必须被正常解析（不得卡死在文件尾之外的扫描位上）"
    );
    assert_eq!(
        pos_of(&batch2.next_cursor),
        (0, new_len),
        "扫描态归零、水位到新文件尾"
    );

    // 第 3 轮：重读为空
    let batch3 = dsh::read_incremental(home, &batch2.next_cursor, NOW_MS).unwrap();
    assert!(batch3.observations.is_empty(), "到尾后重读为空");
}

/// NB1（二轮评审）：**骑界普通行**不是超大行——块内已有若干完整行、块尾一条正在写的
/// 普通行恰好跨过 8 MiB 读界时，不得起扫（那会把这条普通行整体跳过不解析 = 丢数据）。
/// 正确语义：只有**整块从头到尾无任何换行**（offset == base，什么都没消费）才是真
/// 「行 ≥ 8 MiB」；骑界普通行走旧路径——下轮从行边界有界续读并正常解析。
#[test]
fn dsh_crossing_normal_line_is_parsed_not_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let t0 = BASE_MS;
    // 大 payload 消息行（pad 字段撑大行宽，事件本身合法可解析）
    let padded_message = |seq: i64, time: i64, turn: i64, step: i64, pad: usize| {
        let mut v: Value = serde_json::from_str(&dsh_message_event(
            seq,
            time,
            turn,
            step,
            Some(96),
            vec![compact_chunks(
                "text-chunks",
                time - 10,
                0,
                &[5],
                &["a", "b"],
            )],
            replay_source(),
        ))
        .unwrap();
        v["pad"] = Value::String("a".repeat(pad));
        v.to_string()
    };
    let lines = [
        dsh_header("sess-x"),
        // line1+line2 ≈ 4 MiB（完整消费）
        dsh_event("step/start", 2, t0 + 20, json!({"turn": 1, "step": 1})),
        padded_message(3, t0 + 120, 1, 1, 4 * 1024 * 1024),
        // line3 小 + line4 ≈ 4 MiB：line4 起点 ≈ 4 MiB，终点 ≈ 8 MiB——骑过读界
        dsh_event("step/start", 5, t0 + 200, json!({"turn": 2, "step": 1})),
        padded_message(6, t0 + 300, 2, 1, 4 * 1024 * 1024),
    ];
    let sess = dsh_session_dir(home, "sess-x");
    std::fs::create_dir_all(&sess).unwrap();
    let path = sess.join("session.v3.jsonl");
    let body = lines.join("\n") + "\n";
    std::fs::write(&path, &body).unwrap();
    let file_len = body.len() as u64;
    let dir_key = sess.to_string_lossy().to_string();
    let pos_of = |cursor: &SourceCursor| -> (u64, u64) {
        let v: Value = serde_json::from_str(&cursor.value).unwrap();
        let p = &v["sessions"][&dir_key];
        (
            p["oversized_upto"].as_u64().unwrap_or(0),
            p["offset"].as_u64().unwrap_or(0),
        )
    };

    // 第 1 轮：读界截在 line4 中部——完整行照常消费（M1），骑界行**不起扫**
    let batch1 = dsh::read_incremental(home, &SourceCursor::empty("dsh"), NOW_MS).unwrap();
    let keys1: std::collections::BTreeSet<_> = batch1
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    assert_eq!(
        keys1,
        str_set(&["sess-x:1:1"]),
        "第 1 轮恰消费读界前的完整行"
    );
    assert!(
        !batch1
            .diagnostics
            .iter()
            .any(|d| d.contains("single line exceeds") || d.contains("oversized line")),
        "骑界普通行不得触发超大行扫描/跳过诊断: {:?}",
        batch1.diagnostics
    );
    let (upto1, offset1) = pos_of(&batch1.next_cursor);
    assert_eq!(upto1, 0, "不起扫：扫描态保持归零");
    assert!(
        offset1 > 0 && offset1 < file_len,
        "水位停在骑界行行首（下轮有界续读）"
    );

    // 第 2 轮：骑界行已写完 → 从行首有界续读并**正常解析为观察**
    let batch2 = dsh::read_incremental(home, &batch1.next_cursor, NOW_MS).unwrap();
    let keys2: std::collections::BTreeSet<_> = batch2
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    assert_eq!(
        keys2,
        str_set(&["sess-x:2:1"]),
        "骑界普通行必须在后续轮次被正常解析为观察（不得静默跳过丢数据）"
    );
    assert!(
        !batch2
            .diagnostics
            .iter()
            .any(|d| d.contains("single line exceeds") || d.contains("oversized line")),
        "{:?}",
        batch2.diagnostics
    );
    let (upto2, offset2) = pos_of(&batch2.next_cursor);
    assert_eq!(upto2, 0);
    assert_eq!(offset2, file_len, "水位到文件尾");

    // 第 3 轮：重读为空
    let batch3 = dsh::read_incremental(home, &batch2.next_cursor, NOW_MS).unwrap();
    assert!(batch3.observations.is_empty(), "到尾后重读为空");
}

/// R3'/NB2（评审追记）：zstd 单帧解压超预算（压缩 < 8 MiB、解压 > 8 MiB = 解压炸弹）
/// 且**块内下一帧魔数可见 = 帧尾已知**时，必须有界解码 + 整帧跳过（水位越过帧尾、下轮
/// 不再重读），绝不 decode_all 全量解压。前置一小帧合法事件、后随一帧合法事件：两者
/// 观察照常产出（炸弹帧内容零解析）。帧尾不可知（块内末帧）的变体见
/// `dsh_zstd_bomb_beyond_cap_is_scanned_bounded_and_skipped`。
///
/// M1（三轮复审）后的轮次划分：炸弹帧的**分类解码**（为判定炸弹最多解 8 MiB）计入
/// 整批解码预算，与前置小帧合计耗尽本轮预算 → 后随合法帧当轮不解不跳、下轮恢复
///（「预算耗尽前的帧照常产出、余下帧下轮续」的既定验收语义）。
///
/// F1（四轮复审）后的轮次划分：解码预算升级为**整源共享**且单帧只拿剩余余量——炸弹
/// 帧第 1 轮只拿到「8 MiB − 前置小帧」的余量，按「余量截断 ≠ 单帧超限」不当场判炸弹
///（不消费不越过，水位停帧 1 末尾）；第 2 轮以足额预算完成分类（越过整帧），后随帧
/// 第 3 轮恢复。炸弹内容零解析、有界、不反复重解的契约不变。
#[test]
fn dsh_zstd_frame_beyond_decode_budget_is_skipped_not_decoded_wholesale() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let t0 = BASE_MS;
    let sess = dsh_session_dir(home, "sess-1");
    // 帧 1（小、合法）：一条完整事件
    let frame1 = [
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
                t0 + 110,
                0,
                &[5],
                &["a", "b"],
            )],
            replay_source(),
        ),
    ];
    // 帧 2（炸弹）：解压 12 MiB（> 8 MiB 预算），压缩后远小于 8 MiB 尾读上限
    let bomb_text = {
        let line = "x".repeat(64);
        let mut s = String::new();
        for _ in 0..(12 * 1024 * 1024 / (64 + 1)) {
            s += &line;
            s += "\n";
        }
        s
    };
    let frame3 = [
        dsh_event("step/start", 5, t0 + 200, json!({"turn": 2, "step": 1})),
        dsh_message_event(
            6,
            t0 + 300,
            2,
            1,
            Some(7),
            vec![compact_chunks(
                "text-chunks",
                t0 + 290,
                0,
                &[5],
                &["x", "y"],
            )],
            replay_source(),
        ),
    ];
    std::fs::create_dir_all(&sess).unwrap();
    let path = sess.join("session.v3.jsonl.zstd");
    let f1 = zstd::stream::encode_all(frame1.join("\n").add_nl().as_bytes(), 3).unwrap();
    let bomb = zstd::stream::encode_all(bomb_text.as_bytes(), 1).unwrap();
    let mut bytes = f1.clone();
    bytes.extend_from_slice(&bomb);
    bytes.extend_from_slice(
        &zstd::stream::encode_all(frame3.join("\n").add_nl().as_bytes(), 3).unwrap(),
    );
    std::fs::write(&path, &bytes).unwrap();
    let file_len = bytes.len() as u64;
    assert!(
        file_len < super_cap(),
        "夹具前提：三帧压缩后一并落在单次尾读窗内（炸弹帧尾 = 下一帧魔数，块内已知）"
    );
    let f1_end = f1.len() as u64;
    let bomb_end = (f1.len() + bomb.len()) as u64;
    let dir_key = sess.to_string_lossy().to_string();
    let offset_of = |cursor: &SourceCursor| -> u64 {
        let v: Value = serde_json::from_str(&cursor.value).unwrap();
        v["sessions"][&dir_key]["offset"].as_u64().unwrap_or(0)
    };

    // 第 1 轮（F1 后的轮次划分）：前置小帧观察照常；炸弹帧只拿到「剩余预算」（< 单帧
    // 上限）→ 余量截断 ≠ 单帧超限，**不当场判炸弹**：本帧不消费不越过（水位停帧 1
    // 末尾），与后随合法帧一并 deferred，留预算耗尽诊断
    let batch1 = dsh::read_incremental(home, &SourceCursor::empty("dsh"), NOW_MS).unwrap();
    let keys1: std::collections::BTreeSet<_> = batch1
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    assert_eq!(
        keys1,
        str_set(&["sess-1:1:1"]),
        "炸弹帧前的合法帧观察照常产出"
    );
    assert!(
        batch1
            .diagnostics
            .iter()
            .any(|d| d.contains("batch decode budget exhausted")),
        "本轮余量截断必须留预算诊断: {:?}",
        batch1.diagnostics
    );
    assert!(
        !batch1
            .diagnostics
            .iter()
            .any(|d| d.contains("beyond decode budget") || d.contains("JSON")),
        "余量截断不得误判炸弹，炸弹帧内容绝不进事件行解析: {:?}",
        batch1.diagnostics
    );
    assert_eq!(
        offset_of(&batch1.next_cursor),
        f1_end,
        "水位停在炸弹帧起点（= 帧 1 末尾）：余量截断的帧不消费不越过"
    );

    // 第 2 轮（文件未变）：新预算足额 → 炸弹分类成立（顶满单帧上限仍有剩余输出），
    // 留「beyond budget」诊断且水位越过整帧；后随合法帧因分类解码耗尽本轮预算再次
    // deferred（下轮恢复）
    let batch2 = dsh::read_incremental(home, &batch1.next_cursor, NOW_MS).unwrap();
    assert!(
        batch2.observations.is_empty(),
        "本轮只做炸弹分类（零观察），后随帧 deferred"
    );
    assert!(
        batch2
            .diagnostics
            .iter()
            .any(|d| d.contains("beyond decode budget")),
        "足额预算下炸弹必须被判出并留诊断: {:?}",
        batch2.diagnostics
    );
    assert!(
        !batch2.diagnostics.iter().any(|d| d.contains("JSON")),
        "炸弹帧内容绝不进事件解析: {:?}",
        batch2.diagnostics
    );
    assert_eq!(
        offset_of(&batch2.next_cursor),
        bomb_end,
        "水位必须越过整颗炸弹帧（帧尾已知，无需额外 IO），停在帧 3 之前"
    );

    // 第 3 轮：后随合法帧恢复产出（炸弹帧不重解、无重复诊断）
    let batch3 = dsh::read_incremental(home, &batch2.next_cursor, NOW_MS).unwrap();
    let keys3: std::collections::BTreeSet<_> = batch3
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    assert_eq!(
        keys3,
        str_set(&["sess-1:2:1"]),
        "后随合法帧下轮恢复（炸弹帧不再重解、无重复诊断）: {:?}",
        batch3.diagnostics
    );
    assert!(
        batch3.diagnostics.is_empty(),
        "水位已在炸弹帧之后 → 无重复诊断: {:?}",
        batch3.diagnostics
    );
    assert_eq!(offset_of(&batch3.next_cursor), file_len, "水位到文件尾");

    // 第 4 轮（文件未变）：便宜空批
    let batch4 = dsh::read_incremental(home, &batch3.next_cursor, NOW_MS).unwrap();
    assert!(batch4.observations.is_empty(), "不重复消费");
}

/// 确定性伪随机半字节序列（xorshift32 取高 4 位；16 符号、全周期统计质量 →
/// zstd ~2:1 压缩比。炸弹夹具用：压缩体 ≥ 读界，而读窗内的压缩体可解出 ≥ 预算的输出。
/// 不用 LCG 低位——其低位周期仅 2^5，会被 zstd 压到 ~10000:1）
fn lcg_nibbles(len: usize, seed: u32) -> Vec<u8> {
    let mut state = seed | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            ((state >> 27) & 0x0F) as u8
        })
        .collect()
}

/// NB2（二轮评审）：**压缩体 ≥ 8 MiB** 的炸弹/巨帧——块内末帧时帧尾不可知（压缩流超出
/// 读界）。修复后不得把水位落到压缩流中部（下轮扫不到魔数 → MAGIC_MISSING → bad 冻死），
/// 而是镜像 plain 的扫描态：水位停在帧起点、`oversized_upto` 承载帧扫描位，后续轮
/// 8 MiB 步进**只扫下一帧魔数**（不解码帧体）；找到魔数 = 帧尾 → 越过并消费其后帧。
///
/// F1（四轮复审）后的轮次划分：前置小帧先吃掉少量共享预算 → 炸弹第 1 轮被余量截断
///（defer，不判炸弹）；第 2 轮足额预算完成分类并进扫描态；第 3 轮扫到帧尾、越过并
/// 消费其后帧。扫描有界、魔数骑界重叠、炸弹内容零解析的契约不变。
#[test]
fn dsh_zstd_bomb_beyond_cap_is_scanned_bounded_and_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let t0 = BASE_MS;
    let sess = dsh_session_dir(home, "sess-z");
    let frame1 = [
        dsh_header("sess-z"),
        dsh_event("step/start", 2, t0 + 20, json!({"turn": 1, "step": 1})),
        dsh_message_event(
            3,
            t0 + 120,
            1,
            1,
            Some(96),
            vec![compact_chunks(
                "text-chunks",
                t0 + 110,
                0,
                &[5],
                &["a", "b"],
            )],
            replay_source(),
        ),
    ];
    let frame2 = [
        dsh_event("step/start", 5, t0 + 200, json!({"turn": 2, "step": 1})),
        dsh_message_event(
            6,
            t0 + 300,
            2,
            1,
            Some(7),
            vec![compact_chunks(
                "text-chunks",
                t0 + 290,
                0,
                &[5],
                &["x", "y"],
            )],
            replay_source(),
        ),
    ];
    // 炸弹帧：24 MiB 半压缩载荷（16 符号伪随机 ≈ 2:1）→ 压缩体 ~11 MiB > 8 MiB 读界，
    // 且首窗（8 MiB 压缩体）即可解出 ≥ 8 MiB 输出（超解码预算，走 BeyondBudget 检测）
    let bomb =
        zstd::stream::encode_all(lcg_nibbles(24 * 1024 * 1024, 0x5EED).as_slice(), 1).unwrap();
    assert!(
        bomb.len() > 8 * 1024 * 1024,
        "夹具前提：炸弹压缩体超出单次尾读窗（实际 {} 字节）",
        bomb.len()
    );
    std::fs::create_dir_all(&sess).unwrap();
    let path = sess.join("session.v3.jsonl.zstd");
    let f1 = zstd::stream::encode_all(frame1.join("\n").add_nl().as_bytes(), 3).unwrap();
    let f2 = zstd::stream::encode_all(frame2.join("\n").add_nl().as_bytes(), 3).unwrap();
    let mut bytes = f1.clone();
    let bomb_start = bytes.len() as u64;
    let f1_end = bomb_start;
    bytes.extend_from_slice(&bomb);
    bytes.extend_from_slice(&f2);
    std::fs::write(&path, &bytes).unwrap();
    let file_len = bytes.len() as u64;
    let dir_key = sess.to_string_lossy().to_string();
    let pos_of = |cursor: &SourceCursor| -> (u64, u64) {
        let v: Value = serde_json::from_str(&cursor.value).unwrap();
        let p = &v["sessions"][&dir_key];
        (
            p["oversized_upto"].as_u64().unwrap_or(0),
            p["offset"].as_u64().unwrap_or(0),
        )
    };

    // 第 1 轮：前置小帧观察照常；炸弹帧只拿到「剩余预算」（< 单帧上限）→ 按新契约
    // 不当场判炸弹（余量截断 ≠ 单帧超限）：不消费不越过，留预算耗尽诊断（F1）
    let batch1 = dsh::read_incremental(home, &SourceCursor::empty("dsh"), NOW_MS).unwrap();
    let keys1: std::collections::BTreeSet<_> = batch1
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    assert_eq!(keys1, str_set(&["sess-z:1:1"]), "炸弹前的小帧观察照常产出");
    assert!(
        batch1
            .diagnostics
            .iter()
            .any(|d| d.contains("batch decode budget exhausted")),
        "本轮余量截断必须留预算诊断: {:?}",
        batch1.diagnostics
    );
    assert_eq!(
        pos_of(&batch1.next_cursor),
        (0, f1_end),
        "水位停在炸弹帧起点（帧 1 末尾）、无扫描态：余量截断的帧不消费不越过"
    );

    // 第 2 轮（文件未变）：足额预算的读窗截在炸弹压缩流中部——检测进扫描态；扫描位
    // 必须留在「读界 − 3」（3 字节重叠），让骑界魔数完整落在下一窗内
    let batch2 = dsh::read_incremental(home, &batch1.next_cursor, NOW_MS).unwrap();
    assert!(
        batch2
            .diagnostics
            .iter()
            .any(|d| d.contains("beyond decode budget")),
        "足额预算下解压超预算必须留诊断: {:?}",
        batch2.diagnostics
    );
    assert_eq!(
        pos_of(&batch2.next_cursor),
        (f1_end + super_cap() - 3, bomb_start),
        "offset 停在帧起点，oversized_upto = 读界 − 3（帧扫描位，留魔数骑界重叠）"
    );

    // 第 3 轮（文件未变）：从扫描位 8 MiB 步进扫魔数（只扫不解）→ 找到帧 2 魔数 =
    // 炸弹帧尾 → 越过 + 消费帧 2（炸弹体零解码零解析）
    let batch3 = dsh::read_incremental(home, &batch2.next_cursor, NOW_MS).unwrap();
    let keys3: std::collections::BTreeSet<_> = batch3
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    assert_eq!(
        keys3,
        str_set(&["sess-z:2:1"]),
        "炸弹帧之后的帧必须被消费（帧尾经魔数扫描定位）"
    );
    assert!(
        !batch3.diagnostics.iter().any(|d| d.contains("JSON")),
        "炸弹帧内容绝不进事件解析: {:?}",
        batch3.diagnostics
    );
    let (upto3, offset3) = pos_of(&batch3.next_cursor);
    assert_eq!(upto3, 0, "越过炸弹帧后扫描态归零");
    assert_eq!(offset3, file_len, "水位到文件尾");

    // 第 4 轮：重读为空
    let batch4 = dsh::read_incremental(home, &batch3.next_cursor, NOW_MS).unwrap();
    assert!(batch4.observations.is_empty(), "到尾后重读为空");
}

/// NB2（二轮评审）：**仍在写**的炸弹帧（EOF 截在压缩流中部）——检测到解压超预算后保持
/// 帧扫描位等下轮（只扫魔数、不重复解码）；帧写完后扫到下一帧魔数才越过并补采其后帧。
///
/// F1（四轮复审）后的轮次划分：前置小帧先吃掉少量共享预算 → 炸弹第 1 轮被余量截断
///（defer，不判炸弹）；第 2 轮足额预算判出炸弹并进扫描态；第 3 轮原地等待（不重复
/// 解码/诊断）；第 4 轮帧写完后越过并补采其后帧。
#[test]
fn dsh_zstd_partially_written_bomb_waits_for_completion_then_crosses() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let t0 = BASE_MS;
    let sess = dsh_session_dir(home, "sess-w");
    let frame1 = [
        dsh_header("sess-w"),
        dsh_event("step/start", 2, t0 + 20, json!({"turn": 1, "step": 1})),
        dsh_message_event(
            3,
            t0 + 120,
            1,
            1,
            Some(96),
            vec![compact_chunks(
                "text-chunks",
                t0 + 110,
                0,
                &[5],
                &["a", "b"],
            )],
            replay_source(),
        ),
    ];
    let frame2 = [
        dsh_event("step/start", 5, t0 + 200, json!({"turn": 2, "step": 1})),
        dsh_message_event(
            6,
            t0 + 300,
            2,
            1,
            Some(7),
            vec![compact_chunks(
                "text-chunks",
                t0 + 290,
                0,
                &[5],
                &["x", "y"],
            )],
            replay_source(),
        ),
    ];
    // 高压缩比炸弹（1 KiB 随机块重复 12288 次 = 12 MiB，压缩后 ~十几 KiB），
    // 截取压缩字节前 95% 模拟「EOF 于帧中部」——前缀已可解出 ≥ 8 MiB（超解码预算）
    let block = lcg_nibbles(1024, 0xBEEF);
    let mut payload = Vec::with_capacity(12 * 1024 * 1024);
    for _ in 0..12_288 {
        payload.extend_from_slice(&block);
    }
    let bomb_full = zstd::stream::encode_all(payload.as_slice(), 3).unwrap();
    let cut = bomb_full.len() * 95 / 100;
    std::fs::create_dir_all(&sess).unwrap();
    let path = sess.join("session.v3.jsonl.zstd");
    let f1 = zstd::stream::encode_all(frame1.join("\n").add_nl().as_bytes(), 3).unwrap();
    let f2 = zstd::stream::encode_all(frame2.join("\n").add_nl().as_bytes(), 3).unwrap();
    let mut bytes = f1.clone();
    let bomb_start = bytes.len() as u64;
    let f1_end = bomb_start;
    bytes.extend_from_slice(&bomb_full[..cut]);
    std::fs::write(&path, &bytes).unwrap();
    let truncated_len = bytes.len() as u64;
    let dir_key = sess.to_string_lossy().to_string();
    let pos_of = |cursor: &SourceCursor| -> (u64, u64) {
        let v: Value = serde_json::from_str(&cursor.value).unwrap();
        let p = &v["sessions"][&dir_key];
        (
            p["oversized_upto"].as_u64().unwrap_or(0),
            p["offset"].as_u64().unwrap_or(0),
        )
    };

    // 第 1 轮：帧内 EOF——前置小帧观察照常；炸弹帧只拿到「剩余预算」（< 单帧上限）→
    // 不当场判炸弹（余量截断 ≠ 单帧超限）：不消费不越过，留预算耗尽诊断（F1）
    let batch1 = dsh::read_incremental(home, &SourceCursor::empty("dsh"), NOW_MS).unwrap();
    let keys1: std::collections::BTreeSet<_> = batch1
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    assert_eq!(keys1, str_set(&["sess-w:1:1"]));
    assert!(
        batch1
            .diagnostics
            .iter()
            .any(|d| d.contains("batch decode budget exhausted")),
        "本轮余量截断必须留预算诊断: {:?}",
        batch1.diagnostics
    );
    assert_eq!(
        pos_of(&batch1.next_cursor),
        (0, f1_end),
        "水位停在炸弹帧起点（帧 1 末尾）、无扫描态：余量截断的帧不消费不越过"
    );

    // 第 2 轮（文件未变）：足额预算 → 检测到解压超预算 = 炸弹 + 帧扫描位等下轮
    //（只扫魔数、不重复解码）；水位停帧起点、扫描位 = 文件尾，不落 bad
    //（下轮文件长了必须还能续）
    let batch2 = dsh::read_incremental(home, &batch1.next_cursor, NOW_MS).unwrap();
    assert!(
        batch2
            .diagnostics
            .iter()
            .any(|d| d.contains("beyond decode budget")),
        "足额预算下截断帧解压超预算必须留诊断: {:?}",
        batch2.diagnostics
    );
    assert_eq!(
        pos_of(&batch2.next_cursor),
        (truncated_len - 3, bomb_start),
        "水位停在帧起点，扫描位 = 文件尾 − 3（重叠，等待帧写完）"
    );

    // 第 3 轮（文件未变）：扫描位已在文件尾 → 便宜空批（只扫魔数，不重复解码帧体）
    let batch3 = dsh::read_incremental(home, &batch2.next_cursor, NOW_MS).unwrap();
    assert!(batch3.observations.is_empty(), "帧未写完：无观察");
    assert!(
        batch3.diagnostics.is_empty(),
        "等待轮不重复诊断/解码: {:?}",
        batch3.diagnostics
    );
    assert_eq!(
        pos_of(&batch3.next_cursor),
        (truncated_len - 3, bomb_start),
        "扫描位原地保持"
    );

    // 第 4 轮：帧写完（补上剩余压缩体）+ 其后新帧 → 扫到帧 2 魔数 = 炸弹帧尾 →
    // 越过 + 消费帧 2
    bytes.extend_from_slice(&bomb_full[cut..]);
    bytes.extend_from_slice(&f2);
    std::fs::write(&path, &bytes).unwrap();
    let file_len = bytes.len() as u64;
    let batch4 = dsh::read_incremental(home, &batch3.next_cursor, NOW_MS).unwrap();
    let keys4: std::collections::BTreeSet<_> = batch4
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    assert_eq!(
        keys4,
        str_set(&["sess-w:2:1"]),
        "帧写完后必须越过炸弹帧并补采其后帧"
    );
    let (upto4, offset4) = pos_of(&batch4.next_cursor);
    assert_eq!(upto4, 0, "越过炸弹帧后扫描态归零");
    assert_eq!(offset4, file_len, "水位到文件尾");

    // 第 5 轮：重读为空
    let batch5 = dsh::read_incremental(home, &batch4.next_cursor, NOW_MS).unwrap();
    assert!(batch5.observations.is_empty(), "到尾后重读为空");
}

/// M1（三轮复审）：解码预算必须约束**整批**而非逐帧重置——单帧各自低于 8 MiB 上限、
/// 合计超预算的多个小压缩帧，本轮解码/驻留总量必须受限：预算耗尽即停，其余帧不消费
/// 不越过（水位停在最后已解码单元末尾），下一轮从那里续；各帧均未超单帧预算时不得
/// 出现炸弹诊断。
///
/// F1（四轮复审）后的精确口径：预算由 read_incremental 建立、**整源共享**，且单帧只拿
/// 剩余余量——三个各 ~4 MiB 的合法帧每轮恰好容下一个（第 1 帧解完后余 ~4 MiB < 第 2 帧
/// 解码量 → 余量截断 defer），逐轮一帧、不丢不重；「N 帧同轮连解」的旧语义（帧内重置
/// 预算）被推翻。
#[test]
fn dsh_batch_decode_budget_bounds_whole_batch_not_per_frame() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let t0 = BASE_MS;
    let sess = dsh_session_dir(home, "sess-bb");
    // 每帧：step/start + 一条 pad 字段撑到 ~4 MiB 解码量的合法消息（单帧 < 8 MiB 预算，
    // 压缩后仅数 KB —— 复审反例形态：压缩总量极小、解码总量超批预算）
    let big_frame = |with_header: bool, turn: i64, seq: i64| -> Vec<u8> {
        let mut lines = Vec::new();
        if with_header {
            lines.push(dsh_header("sess-bb"));
        }
        lines.push(dsh_event(
            "step/start",
            seq,
            t0 + turn * 100,
            json!({"turn": turn, "step": 1}),
        ));
        let mut msg: Value = serde_json::from_str(&dsh_message_event(
            seq + 1,
            t0 + turn * 100 + 90,
            turn,
            1,
            Some(96),
            vec![compact_chunks(
                "text-chunks",
                t0 + turn * 100 + 10,
                0,
                &[5],
                &["a", "b"],
            )],
            replay_source(),
        ))
        .unwrap();
        msg["pad"] = Value::String("a".repeat(4 * 1024 * 1024));
        lines.push(msg.to_string());
        zstd::stream::encode_all(lines.join("\n").add_nl().as_bytes(), 3).unwrap()
    };
    let f1 = big_frame(true, 1, 2);
    let f2 = big_frame(false, 2, 5);
    let f3 = big_frame(false, 3, 8);
    std::fs::create_dir_all(&sess).unwrap();
    let path = sess.join("session.v3.jsonl.zstd");
    let mut bytes = f1.clone();
    bytes.extend_from_slice(&f2);
    bytes.extend_from_slice(&f3);
    std::fs::write(&path, &bytes).unwrap();
    let f1_end = f1.len() as u64;
    let f2_end = (f1.len() + f2.len()) as u64;
    let file_len = bytes.len() as u64;
    assert!(
        file_len < super_cap(),
        "夹具前提：三帧压缩后一并落在单次尾读窗内（实际 {file_len} 字节）"
    );
    let dir_key = sess.to_string_lossy().to_string();
    let offset_of = |cursor: &SourceCursor| -> u64 {
        let v: Value = serde_json::from_str(&cursor.value).unwrap();
        v["sessions"][&dir_key]["offset"].as_u64().unwrap_or(0)
    };

    // 第 1 轮：帧 1 解码后余 ~4 MiB < 帧 2 解码量 → 帧 2 被余量截断（defer）、帧 3
    // 未起解：水位停在帧 1 末尾，无炸弹诊断
    let batch1 = dsh::read_incremental(home, &SourceCursor::empty("dsh"), NOW_MS).unwrap();
    let keys1: std::collections::BTreeSet<_> = batch1
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    assert_eq!(
        keys1,
        str_set(&["sess-bb:1:1"]),
        "整批预算耗尽前的帧照常产出观察: {:?}",
        batch1.diagnostics
    );
    assert_eq!(
        batch1.observations.len(),
        1,
        "整源共享 8 MiB 预算只容一帧（各 ~4 MiB），不得逐帧重置后全量解码三帧"
    );
    assert!(
        batch1
            .diagnostics
            .iter()
            .any(|d| d.contains("batch decode budget exhausted")),
        "余量截断必须留预算诊断: {:?}",
        batch1.diagnostics
    );
    assert!(
        !batch1
            .diagnostics
            .iter()
            .any(|d| d.contains("beyond decode budget")),
        "各帧均未超单帧预算，不得出现炸弹诊断: {:?}",
        batch1.diagnostics
    );
    assert_eq!(
        offset_of(&batch1.next_cursor),
        f1_end,
        "水位停在最后已解码单元末尾（帧 2/3 未被消费也未越过后跳过）"
    );

    // 第 2 轮：从帧 1 末尾续读同一窗 → 新预算容下帧 2，帧 3 再次被余量截断
    let batch2 = dsh::read_incremental(home, &batch1.next_cursor, NOW_MS).unwrap();
    let keys2: std::collections::BTreeSet<_> = batch2
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    assert_eq!(
        keys2,
        str_set(&["sess-bb:2:1"]),
        "预算余量下轮续读（后续有效帧下轮恢复）: {:?}",
        batch2.diagnostics
    );
    assert_eq!(
        offset_of(&batch2.next_cursor),
        f2_end,
        "水位停在帧 2 末尾（帧 3 未被消费）"
    );

    // 第 3 轮：帧 3 恢复产出
    let batch3 = dsh::read_incremental(home, &batch2.next_cursor, NOW_MS).unwrap();
    let keys3: std::collections::BTreeSet<_> = batch3
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    assert_eq!(
        keys3,
        str_set(&["sess-bb:3:1"]),
        "帧 3 下轮恢复: {:?}",
        batch3.diagnostics
    );
    assert_eq!(offset_of(&batch3.next_cursor), file_len, "水位到文件尾");

    // 第 4 轮：重读为空
    let batch4 = dsh::read_incremental(home, &batch3.next_cursor, NOW_MS).unwrap();
    assert!(batch4.observations.is_empty(), "到尾后重读为空");
}

/// 四轮复审 F1 反例①：解码预算必须在**一次源采集**内跨会话共享——旧语义在每个会话的
/// decode_tail 里重新发放 8 MiB，N 个「压缩体极小、解码量大、无观察」的合法会话把单轮
/// 解码量放大到 N×8 MiB（观察条数预算不被合法非事件内容消耗，拦不住这个放大）。
/// 修复后：read_incremental 持有整轮共享预算，向会话、帧逐层传递；因本轮余量不足而
/// 未处理的正常帧**不消费不越过**（水位原地，下轮以新预算续读），并留预算耗尽诊断，
/// 绝不误判炸弹/跳过。
#[test]
fn dsh_decode_budget_is_shared_across_sessions_within_one_pass() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    // 每会话一帧：session 头 + 768 行 ~4 KiB 合法非事件 JSON（tool/call 不产观察、
    // 不耗观察预算），解码 ~3 MiB、压缩后仅数 KB —— 复审反例形态
    let big_frame = |session: &str| -> Vec<u8> {
        let mut lines = vec![dsh_header(session)];
        for i in 0..768 {
            lines.push(format!(
                "{{\"type\":\"tool/call\",\"seq\":{i},\"pad\":\"{}\"}}",
                "x".repeat(4000)
            ));
        }
        zstd::stream::encode_all(lines.join("\n").add_nl().as_bytes(), 3).unwrap()
    };
    // 写入顺序 = 会话名逆序：mtime 降序（同毫秒回退 dir_key 升序）都给出
    // m1→m2→m3→m4 的确定性消费序（最新的会话优先吃预算）
    let mut lens = std::collections::BTreeMap::new();
    for name in ["sess-m4", "sess-m3", "sess-m2", "sess-m1"] {
        let sess = dsh_session_dir(home, name);
        let bytes = big_frame(name);
        lens.insert(name.to_string(), bytes.len() as u64);
        std::fs::create_dir_all(&sess).unwrap();
        std::fs::write(sess.join("session.v3.jsonl.zstd"), &bytes).unwrap();
    }
    let offset_of = |cursor: &SourceCursor, name: &str| -> u64 {
        let v: Value = serde_json::from_str(&cursor.value).unwrap();
        let key = dsh_session_dir(home, name).to_string_lossy().to_string();
        v["sessions"][&key]["offset"].as_u64().unwrap_or(0)
    };

    // 第 1 轮：整轮 8 MiB 预算只够 m1 + m2（各 ~3 MiB）→ m3 被余量截断、m4 未起解：
    // 二者水位原地（下轮续），留预算耗尽诊断，绝不误判炸弹/跳过
    let batch1 = dsh::read_incremental(home, &SourceCursor::empty("dsh"), NOW_MS).unwrap();
    assert!(
        batch1.observations.is_empty(),
        "合法非事件内容不产观察（反例前提：观察预算拦不住解码放大）"
    );
    assert!(
        batch1
            .diagnostics
            .iter()
            .any(|d| d.contains("batch decode budget exhausted")),
        "整轮共享预算耗尽必须留诊断: {:?}",
        batch1.diagnostics
    );
    assert!(
        !batch1
            .diagnostics
            .iter()
            .any(|d| d.contains("beyond decode budget") || d.contains("skipped")),
        "余量不足的正常帧不得误判炸弹/跳过: {:?}",
        batch1.diagnostics
    );
    for name in ["sess-m1", "sess-m2"] {
        assert_eq!(
            offset_of(&batch1.next_cursor, name),
            lens[name],
            "{name} 在共享预算内完整消费（单轮累计解码 ≤ 8 MiB 的直接体现）"
        );
    }
    for name in ["sess-m3", "sess-m4"] {
        assert_eq!(
            offset_of(&batch1.next_cursor, name),
            0,
            "{name} 因本轮余量不足未处理：不解不跳，水位原地（下轮以新预算续）"
        );
    }

    // 第 2 轮：新预算续读 → m3 + m4 恢复（不丢不重）
    let batch2 = dsh::read_incremental(home, &batch1.next_cursor, NOW_MS).unwrap();
    for name in ["sess-m1", "sess-m2", "sess-m3", "sess-m4"] {
        assert_eq!(
            offset_of(&batch2.next_cursor, name),
            lens[name],
            "{name} 第二轮到文件尾（未处理会话下轮可继续）"
        );
    }
    assert!(
        !batch2
            .diagnostics
            .iter()
            .any(|d| d.contains("batch decode budget exhausted")),
        "余量充足的轮次不得再报预算诊断: {:?}",
        batch2.diagnostics
    );

    // 第 3 轮：重读为空
    let batch3 = dsh::read_incremental(home, &batch2.next_cursor, NOW_MS).unwrap();
    assert!(batch3.observations.is_empty(), "到尾后重读为空");
}

/// 四轮复审 F1 反例②（同项补充）：同一 tail 内的下一帧只拿**剩余**预算——两个各
/// ~7 MiB 的合法帧：第 1 轮解完帧 1（余 ~1 MiB）后帧 2 被余量截断 → 不是炸弹、不
/// 丢弃：水位停帧 2 起点（= 帧 1 末尾），下轮以新预算整帧重解；旧语义在帧内重置
/// 预算，两帧连解同轮驻留 ~14 MiB 解码文本。
#[test]
fn dsh_frame_gets_only_remaining_budget_and_defers_not_bomb() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let sess = dsh_session_dir(home, "sess-d");
    let pad_lines = |count: usize| -> Vec<String> {
        (0..count)
            .map(|i| {
                format!(
                    "{{\"type\":\"tool/call\",\"seq\":{i},\"pad\":\"{}\"}}",
                    "x".repeat(4000)
                )
            })
            .collect()
    };
    let mut lines = vec![dsh_header("sess-d")];
    lines.extend(pad_lines(1792)); // 帧 1：~7 MiB 解码量（< 8 MiB 单帧上限，不是炸弹）
    let f1 = zstd::stream::encode_all(lines.join("\n").add_nl().as_bytes(), 3).unwrap();
    let f2 = zstd::stream::encode_all(pad_lines(1792).join("\n").add_nl().as_bytes(), 3).unwrap();
    std::fs::create_dir_all(&sess).unwrap();
    let path = sess.join("session.v3.jsonl.zstd");
    let mut bytes = f1.clone();
    bytes.extend_from_slice(&f2);
    std::fs::write(&path, &bytes).unwrap();
    let f1_end = f1.len() as u64;
    let file_len = bytes.len() as u64;
    assert!(
        file_len < super_cap(),
        "夹具前提：两帧压缩后一并落在单次尾读窗内（实际 {file_len} 字节）"
    );
    let dir_key = sess.to_string_lossy().to_string();
    let offset_of = |cursor: &SourceCursor| -> u64 {
        let v: Value = serde_json::from_str(&cursor.value).unwrap();
        v["sessions"][&dir_key]["offset"].as_u64().unwrap_or(0)
    };

    // 第 1 轮：帧 1 完整解码（余 ~1 MiB）→ 帧 2 被余量截断：不消费不越过、无炸弹诊断
    let batch1 = dsh::read_incremental(home, &SourceCursor::empty("dsh"), NOW_MS).unwrap();
    assert!(batch1.observations.is_empty(), "合法非事件内容不产观察");
    assert!(
        batch1
            .diagnostics
            .iter()
            .any(|d| d.contains("batch decode budget exhausted")),
        "余量截断必须留预算诊断: {:?}",
        batch1.diagnostics
    );
    assert!(
        !batch1
            .diagnostics
            .iter()
            .any(|d| d.contains("beyond decode budget") || d.contains("skipped")),
        "余量截断不是炸弹，不得留跳过诊断: {:?}",
        batch1.diagnostics
    );
    assert_eq!(
        offset_of(&batch1.next_cursor),
        f1_end,
        "检查点停在帧 2 起点（= 帧 1 末尾），帧 2 未被越过，下轮以新预算重解"
    );

    // 第 2 轮：帧 2 完整解码（新预算）
    let batch2 = dsh::read_incremental(home, &batch1.next_cursor, NOW_MS).unwrap();
    assert!(
        batch2.diagnostics.is_empty(),
        "余量充足轮次无诊断: {:?}",
        batch2.diagnostics
    );
    assert_eq!(offset_of(&batch2.next_cursor), file_len, "水位到文件尾");

    // 第 3 轮：重读为空
    let batch3 = dsh::read_incremental(home, &batch2.next_cursor, NOW_MS).unwrap();
    assert!(batch3.observations.is_empty(), "到尾后重读为空");
}

/// 确定性伪随机字节序列（xorshift32 全字节；近不可压缩 ~1:1）——用于把后续帧的
/// 起点顶到指定偏移而自身解码量仍低于单帧预算（复审 M2 反例夹具）
fn lcg_bytes(len: usize, seed: u32) -> Vec<u8> {
    let mut state = seed | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state >> 24) as u8
        })
        .collect()
}

/// zstd 魔数出现次数（夹具前提守卫：魔数意外出现在伪随机压缩体内会改变帧布局）
fn count_zstd_magic(bytes: &[u8]) -> usize {
    const MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];
    bytes.windows(4).filter(|w| *w == MAGIC).count()
}

/// M2（三轮复审）反例①：**普通压缩小帧跨读界**不是巨帧——前缀帧把一个完整合法
/// 事件帧（解码 ≪ 预算）的起点顶到 ~7.9 MiB 处、压缩体延过 8 MiB 读界时，不得按
/// 「超窗巨帧」进扫描态永久丢弃；必须停在帧起点、读界翻倍重读，若干有界轮次内
/// 被正常解析为观察，全程无「beyond decode budget」/跳过诊断。
#[test]
fn dsh_straddling_small_frame_is_pending_then_parsed_not_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let t0 = BASE_MS;
    let sess = dsh_session_dir(home, "sess-p");
    // 跨界小帧：合法事件 + 4 KiB 伪随机 hex pad（抬高压缩体到 ~2 KiB，让读窗内
    // 只露出前缀 < 800 字节 = 截断；解码总量仍远低于预算）
    let mut msg: Value = serde_json::from_str(&dsh_message_event(
        6,
        t0 + 300,
        2,
        1,
        Some(7),
        vec![compact_chunks(
            "text-chunks",
            t0 + 290,
            0,
            &[5],
            &["x", "y"],
        )],
        replay_source(),
    ))
    .unwrap();
    msg["pad"] = Value::String(
        lcg_nibbles(2048, 0x70D)
            .iter()
            .map(|b| format!("{b:x}"))
            .collect::<String>(),
    );
    let frame2 = [
        dsh_event("step/start", 5, t0 + 200, json!({"turn": 2, "step": 1})),
        msg.to_string(),
    ];
    let f2 = zstd::stream::encode_all(frame2.join("\n").add_nl().as_bytes(), 3).unwrap();
    assert!(
        f2.len() > 1_500,
        "夹具前提：跨界帧压缩体足够大（读窗只露出其前缀），实际 {f2_len}",
        f2_len = f2.len()
    );
    // 前缀帧：近不可压缩随机字节（解码 < 8 MiB 预算、压缩体 ~7.9 MiB）；另置一枚
    // 极小头帧供应会话 id（前缀帧解码内容是废字节，无法承载事件行）
    let f0 = zstd::stream::encode_all(dsh_header("sess-p").add_nl().as_bytes(), 3).unwrap();
    let prefix_payload = lcg_bytes(super_cap() as usize - 1_100, 0x5EED);
    let f1 = zstd::stream::encode_all(prefix_payload.as_slice(), 1).unwrap();
    assert!(
        (f1.len() as u64) < super_cap(),
        "夹具前提：前缀帧解码量低于单帧预算（压缩体 {}）",
        f1.len()
    );
    std::fs::create_dir_all(&sess).unwrap();
    let path = sess.join("session.v3.jsonl.zstd");
    let mut bytes = f0.clone();
    bytes.extend_from_slice(&f1);
    let f2_start = bytes.len() as u64;
    bytes.extend_from_slice(&f2);
    std::fs::write(&path, &bytes).unwrap();
    let file_len = bytes.len() as u64;
    assert_eq!(
        count_zstd_magic(&bytes),
        3,
        "夹具前提：三帧各一枚魔数（伪随机压缩体内不得混入魔数，否则帧布局改变）"
    );
    assert!(
        super_cap() < file_len && f2_start < super_cap(),
        "夹具前提：跨界帧起点在 8 MiB 读界内、文件越过读界"
    );
    let dir_key = sess.to_string_lossy().to_string();
    let state_of = |cursor: &SourceCursor| -> (u64, u64, u64) {
        let v: Value = serde_json::from_str(&cursor.value).unwrap();
        let p = &v["sessions"][&dir_key];
        (
            p["offset"].as_u64().unwrap_or(0),
            p["oversized_upto"].as_u64().unwrap_or(0),
            p["pending_cap"].as_u64().unwrap_or(0),
        )
    };
    let no_skip_diag = |diags: &[String]| {
        !diags.iter().any(|d| {
            d.contains("beyond decode budget")
                || d.contains("giant frame")
                || d.contains("degrad")
                || d.contains("skipped")
        })
    };

    // 第 1 轮：跨界帧只露前缀 → 未决帧（水位停帧起点，不起扫描态、读界翻倍）
    let batch1 = dsh::read_incremental(home, &SourceCursor::empty("dsh"), NOW_MS).unwrap();
    assert!(
        no_skip_diag(&batch1.diagnostics),
        "跨界小帧绝不得被判巨帧/炸弹: {:?}",
        batch1.diagnostics
    );
    assert!(
        batch1
            .observations
            .iter()
            .all(|o| !o.stable_call_key.contains("sess-p:2:1")),
        "跨界帧本轮确实未消费（夹具前提）"
    );
    assert_eq!(
        state_of(&batch1.next_cursor),
        (f2_start, 0, 2 * super_cap()),
        "水位停帧起点、无扫描态、读界翻倍至 16 MiB"
    );

    // 第 2 轮：16 MiB 读界覆盖整帧 → 正常解析为观察（绝无跳过）
    let batch2 = dsh::read_incremental(home, &batch1.next_cursor, NOW_MS).unwrap();
    let keys2: std::collections::BTreeSet<_> = batch2
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    assert!(
        no_skip_diag(&batch2.diagnostics),
        "跨界小帧完整进入读窗后必须正常解码，不得有跳过诊断: {:?}",
        batch2.diagnostics
    );
    assert_eq!(
        keys2,
        str_set(&["sess-p:2:1"]),
        "跨读界的普通小帧必须在有界轮次内被正常解析为观察（不得永久丢弃）"
    );
    assert_eq!(
        state_of(&batch2.next_cursor),
        (file_len, 0, 0),
        "水位到文件尾，未决读界归零"
    );

    // 第 3 轮：重读为空
    let batch3 = dsh::read_incremental(home, &batch2.next_cursor, NOW_MS).unwrap();
    assert!(batch3.observations.is_empty(), "到尾后重读为空");
}

/// M2（三轮复审）对照锁②：同构造但跨界帧**解压 ≥ 8 MiB**（真炸弹）——无论轮次如何
/// 演进，炸弹帧必须被有界跳过（零解析、不反复重解），其后合法帧照常产出。
#[test]
fn dsh_straddling_bomb_frame_is_still_skipped_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let t0 = BASE_MS;
    let sess = dsh_session_dir(home, "sess-pb");
    let frame3 = [
        dsh_event("step/start", 8, t0 + 400, json!({"turn": 3, "step": 1})),
        dsh_message_event(
            9,
            t0 + 500,
            3,
            1,
            Some(7),
            vec![compact_chunks("text-chunks", t0 + 490, 0, &[5], &["z"])],
            replay_source(),
        ),
    ];
    let f3 = zstd::stream::encode_all(frame3.join("\n").add_nl().as_bytes(), 3).unwrap();
    let bomb =
        zstd::stream::encode_all(lcg_nibbles(24 * 1024 * 1024, 0x5EED).as_slice(), 1).unwrap();
    // 极小头帧供应会话 id + 前缀帧（近不可压缩随机字节）把炸弹顶到 ~7.9 MiB 处
    let f0 = zstd::stream::encode_all(dsh_header("sess-pb").add_nl().as_bytes(), 3).unwrap();
    let prefix_payload = lcg_bytes(super_cap() as usize - 1_100, 0x5EED);
    let f1 = zstd::stream::encode_all(prefix_payload.as_slice(), 1).unwrap();
    std::fs::create_dir_all(&sess).unwrap();
    let path = sess.join("session.v3.jsonl.zstd");
    let mut bytes = f0.clone();
    bytes.extend_from_slice(&f1);
    let bomb_start = bytes.len() as u64;
    bytes.extend_from_slice(&bomb);
    bytes.extend_from_slice(&f3);
    std::fs::write(&path, &bytes).unwrap();
    let file_len = bytes.len() as u64;
    assert_eq!(
        count_zstd_magic(&bytes),
        4,
        "夹具前提：四帧各一枚魔数（伪随机体内不得混入魔数，否则帧布局改变）"
    );
    assert!(
        bomb_start < super_cap() && file_len > 2 * super_cap(),
        "夹具前提：炸弹起点在首窗内、文件（前缀 + 炸弹压缩体）远超读界（bomb_start={bomb_start}, file_len={file_len}）"
    );

    // 有界轮次内：炸弹被跳过（零解析）、其后帧产出观察、水位到文件尾
    let dir_key = sess.to_string_lossy().to_string();
    let offset_of = |cursor: &SourceCursor| -> u64 {
        let v: Value = serde_json::from_str(&cursor.value).unwrap();
        v["sessions"][&dir_key]["offset"].as_u64().unwrap_or(0)
    };
    let mut cursor = SourceCursor::empty("dsh");
    let mut saw_bomb_diag = false;
    let mut got_frame3 = false;
    for round in 1..=8 {
        let batch = dsh::read_incremental(home, &cursor, NOW_MS).unwrap();
        saw_bomb_diag |= batch
            .diagnostics
            .iter()
            .any(|d| d.contains("beyond decode budget"));
        got_frame3 |= batch
            .observations
            .iter()
            .any(|o| o.stable_call_key == "sess-pb:3:1");
        assert!(
            !batch
                .observations
                .iter()
                .any(|o| o.stable_call_key.starts_with("sess-pb:")
                    && o.stable_call_key != "sess-pb:3:1"),
            "炸弹帧内容绝不产出观察（第 {round} 轮）"
        );
        cursor = batch.next_cursor;
        if got_frame3 && offset_of(&cursor) == file_len {
            break;
        }
    }
    assert!(saw_bomb_diag, "真炸弹必须留「beyond decode budget」诊断");
    assert!(got_frame3, "炸弹帧之后的合法帧必须被消费");
    assert_eq!(
        offset_of(&cursor),
        file_len,
        "水位越过炸弹帧到文件尾（不反复重读）"
    );
}

/// M2（三轮复审）反例③：未决帧读界翻倍的**上限 64 MiB**——每轮更大读窗仍解不动
///（解码错误非输入不足可愈合，如帧体损坏）的帧，读界按 8→16→32→64 MiB 翻倍期间
/// 始终保持未决（不起扫描态、不跳过）；64 MiB 仍读不全 → 按真巨帧降级（诚实诊断 +
/// 有界扫描态），读界不得无限增长。
#[test]
fn dsh_pending_frame_degrades_to_scan_at_64mib_cap() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let sess = dsh_session_dir(home, "sess-pd");
    // 待损坏帧：~2 MiB 半随机载荷（解码量 ~2 MiB 远低于 8 MiB 预算 → 每轮都在到达
    // 解码预算前因损坏报错 = Truncated），压缩后破坏中段 128 字节
    let payload_vec = lcg_nibbles(2 * 1024 * 1024, 0xC0DE);
    let mut corrupt = zstd::stream::encode_all(payload_vec.as_slice(), 1).unwrap();
    let mid = corrupt.len() / 2;
    for b in &mut corrupt[mid..mid + 128] {
        *b ^= 0x5A;
    }
    // 文件 = 损坏帧（起点 0）+ 63 MiB 无魔数填充 → 每一轮读窗都不到文件尾（非 EOF，
    // 走未决路径而非「文件尾小帧等待」语义），且 64 MiB 读界仍读不到文件尾
    let filler = lcg_bytes(63 * 1024 * 1024, 0xD00D);
    let mut frame = corrupt;
    frame.extend_from_slice(&filler);
    std::fs::create_dir_all(&sess).unwrap();
    let path = sess.join("session.v3.jsonl.zstd");
    std::fs::write(&path, &frame).unwrap();
    let file_len = frame.len() as u64;
    assert_eq!(
        count_zstd_magic(&frame),
        1,
        "夹具前提：仅帧首一枚魔数（填充与破坏不得引入魔数）"
    );
    assert!(
        file_len > 64 * 1024 * 1024,
        "夹具前提：文件越过 64 MiB 读界上限（实际 {file_len}）"
    );
    let dir_key = sess.to_string_lossy().to_string();
    let state_of = |cursor: &SourceCursor| -> (u64, u64, u64) {
        let v: Value = serde_json::from_str(&cursor.value).unwrap();
        let p = &v["sessions"][&dir_key];
        (
            p["offset"].as_u64().unwrap_or(0),
            p["oversized_upto"].as_u64().unwrap_or(0),
            p["pending_cap"].as_u64().unwrap_or(0),
        )
    };

    // 第 1–3 轮：读界 8→16→32→64 MiB 翻倍序列，始终保持未决（不起扫描态）
    let mut cursor = SourceCursor::empty("dsh");
    let expect_caps = [2 * super_cap(), 4 * super_cap(), 8 * super_cap()];
    for (round, cap) in expect_caps.iter().enumerate() {
        let batch = dsh::read_incremental(home, &cursor, NOW_MS).unwrap();
        assert_eq!(
            state_of(&batch.next_cursor),
            (0, 0, *cap),
            "第 {} 轮：水位停帧起点、无扫描态、读界翻倍至 {cap}",
            round + 1
        );
        assert!(
            !batch
                .diagnostics
                .iter()
                .any(|d| d.contains("degrad") || d.contains("giant frame")),
            "读界未到上限前不得降级: {:?}",
            batch.diagnostics
        );
        cursor = batch.next_cursor;
    }

    // 第 4 轮：64 MiB 读界仍解不动/读不全 → 降级为有界扫描态（诚实诊断）
    let batch4 = dsh::read_incremental(home, &cursor, NOW_MS).unwrap();
    assert_eq!(
        state_of(&batch4.next_cursor),
        (0, 8 * super_cap() - 3, 0),
        "降级：水位停帧起点、扫描位 = 64 MiB 读界 − 3（魔数骑界重叠）、未决读界归零"
    );
    assert!(
        batch4.diagnostics.iter().any(|d| d.contains("degrad")),
        "64 MiB 仍读不全必须留降级诊断: {:?}",
        batch4.diagnostics
    );
}

/// 8 MiB 尾读/解码预算（与 sources::TAIL_READ_MAX_BYTES 同值；测试夹具的确定性锚点）
fn super_cap() -> u64 {
    8 * 1024 * 1024
}

// ===========================================================================
// ZCode 用例
// ===========================================================================

/// 正例：证据文档脱敏例（0/6259/7757，output 96）→ 生成窗 1498ms、TPS ≈ 64.09，
/// 且能出 value 点
#[test]
fn zcode_positive_example_builds_value_point() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    {
        let conn = zcode_create_db(home);
        zc_insert(
            &conn,
            "req-a",
            "completed",
            Some(ZC_START),
            Some(ZC_FIRST),
            Some(ZC_DONE),
            Some(7_757),
            Some(96),
            0,
            0,
            "main_turn",
        );
    }
    let batch = zcode::read_incremental(home, &SourceCursor::empty("zcode"), NOW_MS).unwrap();
    assert_eq!(batch.observations.len(), 1);
    let o = &batch.observations[0];
    // 生成窗 = completed − 首点 = 1498ms（证据数）
    assert_eq!(o.generation_ms, Some(1_498));
    assert_eq!(o.ttft_ms(), Some(6_259));
    assert_eq!(o.native_duration_ms, Some(7_757));
    assert_eq!(o.output_tokens, Some(96));
    // reasoning=0 → 输出无思考成分（互斥面）
    assert_eq!(o.output_scope, OutputScope::ExclusiveOutput);
    assert_eq!(o.method, TimingMethod::ClientNative);
    assert_eq!(o.outcome, Outcome::Success);
    assert_eq!(o.model, "deepseek-v4.1-flash");
    assert_eq!(o.turn_key.as_deref(), Some("turn_1"));
    let tps = single_call_tps(o).unwrap();
    assert!(
        (tps - 1000.0 * 96.0 / 1498.0).abs() < 1e-9,
        "证据数 64.09 tok/s，实得 {tps}"
    );

    let at = o.ended_at_ms + 1_000;
    let points = build_point(&batch.observations, &[], "run-1", at);
    assert_eq!(points.len(), 1);
    assert_eq!(points[0].status, PointStatus::Value);
    assert_eq!(points[0].sample_count, 1);
}

/// 缺 F 的完成行跳过速度（不推造参数首点）：brief `zcode_missing_f.tps() == None`
#[test]
fn zcode_missing_first_token_skips_speed() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    {
        let conn = zcode_create_db(home);
        zc_insert(
            &conn,
            "req-f",
            "completed",
            Some(ZC_START),
            None,
            Some(ZC_DONE),
            Some(7_757),
            Some(96),
            0,
            0,
            "main_turn",
        );
    }
    let batch = zcode::read_incremental(home, &SourceCursor::empty("zcode"), NOW_MS).unwrap();
    let missing_f = &batch.observations[0];
    // brief：assert_eq!(zcode_missing_f.tps(), None)
    assert_eq!(single_call_tps(missing_f), None);
    assert_eq!(missing_f.generation_ms, None, "缺 F 不造生成窗");
    assert_eq!(missing_f.first_token_at_ms, None);
    assert!(!missing_f.is_tps_valid());
    assert!(
        batch.diagnostics.iter().any(|d| d.contains("first_token")),
        "缺 F 必须留诊断：{:?}",
        batch.diagnostics
    );
}

/// 失败 / 取消跳过速度，但保留候选面（失败有可靠起止仍进耗时详情）
#[test]
fn zcode_failed_and_cancelled_skip_speed() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    {
        let conn = zcode_create_db(home);
        zc_insert(
            &conn,
            "req-e",
            "error",
            Some(ZC_START),
            Some(ZC_FIRST),
            Some(ZC_DONE),
            Some(7_757),
            Some(96),
            0,
            0,
            "main_turn",
        );
        zc_insert(
            &conn,
            "req-c",
            "cancelled",
            Some(ZC_START),
            Some(ZC_FIRST),
            Some(ZC_DONE),
            Some(7_757),
            Some(96),
            0,
            0,
            "main_turn",
        );
    }
    let batch = zcode::read_incremental(home, &SourceCursor::empty("zcode"), NOW_MS).unwrap();
    assert_eq!(batch.observations.len(), 2);
    let failed = batch
        .observations
        .iter()
        .find(|o| o.outcome == Outcome::Failed)
        .unwrap();
    let cancelled = batch
        .observations
        .iter()
        .find(|o| o.outcome == Outcome::Cancelled)
        .unwrap();
    assert!(!failed.is_tps_valid() && failed.generation_ms.is_none());
    assert!(!cancelled.is_tps_valid() && cancelled.generation_ms.is_none());
    assert!(failed.failure_reason.is_some());
    assert!(cancelled.failure_reason.is_some());
}

/// retry_count > 0 跳过速度（内部重试不可对齐），诊断留痕
#[test]
fn zcode_retry_skips_speed() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    {
        let conn = zcode_create_db(home);
        zc_insert(
            &conn,
            "req-r",
            "completed",
            Some(ZC_START),
            Some(ZC_FIRST),
            Some(ZC_DONE),
            Some(7_757),
            Some(96),
            0,
            2,
            "main_turn",
        );
    }
    let batch = zcode::read_incremental(home, &SourceCursor::empty("zcode"), NOW_MS).unwrap();
    let o = &batch.observations[0];
    assert_eq!(o.outcome, Outcome::Success);
    assert_eq!(o.generation_ms, None, "retry>0 不入速度");
    assert!(!o.is_tps_valid());
    assert!(
        batch.diagnostics.iter().any(|d| d.contains("retry")),
        "{:?}",
        batch.diagnostics
    );
}

/// 重复代际（同一 logical_request 重放两行）只计一次
#[test]
fn zcode_duplicate_generation_counts_once() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    {
        let conn = zcode_create_db(home);
        zc_insert(
            &conn,
            "req-dup",
            "completed",
            Some(ZC_START),
            Some(ZC_FIRST),
            Some(ZC_DONE),
            Some(7_757),
            Some(96),
            0,
            0,
            "main_turn",
        );
        // 同一 call 的重放（同 logical_request_id，代际更新行）
        zc_insert(
            &conn,
            "req-dup",
            "completed",
            Some(ZC_START),
            Some(ZC_FIRST),
            Some(ZC_DONE),
            Some(7_757),
            Some(96),
            0,
            0,
            "main_turn",
        );
    }
    let batch = zcode::read_incremental(home, &SourceCursor::empty("zcode"), NOW_MS).unwrap();
    assert_eq!(batch.observations.len(), 1, "同一 call 重放只计一次");
    assert!(batch
        .diagnostics
        .iter()
        .any(|d| d.contains("重放") || d.contains("重复")));
}

/// rowid 水位增量：第二轮只取新行；同游标重读为空
#[test]
fn zcode_incremental_rowid_tail_does_not_replay() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    {
        let conn = zcode_create_db(home);
        zc_insert(
            &conn,
            "req-1",
            "completed",
            Some(ZC_START),
            Some(ZC_FIRST),
            Some(ZC_DONE),
            Some(7_757),
            Some(96),
            0,
            0,
            "main_turn",
        );
        zc_insert(
            &conn,
            "req-2",
            "completed",
            Some(ZC_START + 10),
            Some(ZC_FIRST + 10),
            Some(ZC_DONE + 10),
            Some(7_757),
            Some(50),
            0,
            0,
            "main_turn",
        );
    }
    let batch1 = zcode::read_incremental(home, &SourceCursor::empty("zcode"), NOW_MS).unwrap();
    assert_eq!(batch1.observations.len(), 2);
    let cursor1 = batch1.next_cursor.clone();
    assert_ne!(cursor1.value, "");

    // 追加两行
    {
        let conn = Connection::open(zcode_db_path(home)).unwrap();
        zc_insert(
            &conn,
            "req-3",
            "completed",
            Some(ZC_START + 20),
            Some(ZC_FIRST + 20),
            Some(ZC_DONE + 20),
            Some(7_757),
            Some(10),
            0,
            0,
            "main_turn",
        );
        zc_insert(
            &conn,
            "req-4",
            "completed",
            Some(ZC_START + 30),
            Some(ZC_FIRST + 30),
            Some(ZC_DONE + 30),
            Some(7_757),
            Some(5),
            0,
            0,
            "main_turn",
        );
    }
    let batch2 = zcode::read_incremental(home, &cursor1, NOW_MS).unwrap();
    assert_eq!(batch2.observations.len(), 2, "只取 rowid 水位之后的新行");
    let keys: std::collections::BTreeSet<_> = batch2
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    assert!(keys.contains("req-3") && keys.contains("req-4"));

    // 用推进后的游标重读：空批（不重复计数）
    let batch2_again = zcode::read_incremental(home, &batch2.next_cursor, NOW_MS).unwrap();
    assert_eq!(batch2_again.observations.len(), 0);
}

/// running 行是原地 UPDATE 收尾：水位停在在飞行之前；收尾后下轮补采
#[test]
fn zcode_running_row_blocks_watermark_until_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    {
        let conn = zcode_create_db(home);
        zc_insert(
            &conn,
            "req-a",
            "completed",
            Some(ZC_START),
            Some(ZC_FIRST),
            Some(ZC_DONE),
            Some(7_757),
            Some(96),
            0,
            0,
            "main_turn",
        );
        zc_insert(
            &conn,
            "req-b",
            "running",
            Some(NOW_MS - 1_000),
            Some(NOW_MS - 900),
            None,
            None,
            None,
            0,
            0,
            "main_turn",
        );
        zc_insert(
            &conn,
            "req-c",
            "completed",
            Some(ZC_START + 40),
            Some(ZC_FIRST + 40),
            Some(ZC_DONE + 40),
            Some(7_757),
            Some(9),
            0,
            0,
            "main_turn",
        );
    }
    let batch1 = zcode::read_incremental(home, &SourceCursor::empty("zcode"), NOW_MS).unwrap();
    let keys1: std::collections::BTreeSet<_> = batch1
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    assert!(
        keys1.contains("req-a"),
        "running 行之前的连续终态前缀照常消费"
    );
    assert!(
        !keys1.contains("req-b"),
        "running 行本身不得入观察（终态未到）"
    );
    assert!(
        keys1.contains("req-c"),
        "M3 收尾：running 行之后的完成行经机会采样入观察（不挡已完成的独立调用）"
    );
    assert_eq!(
        zcode_cursor_of(&batch1.next_cursor),
        (1, ZC_DONE + 40, 3),
        "水位停在在飞行之前（不跨在飞行行）；机会游标 = 本轮最新完成事实 (completed_at, rowid)"
    );

    // req-b 原地 UPDATE 收尾 → 下轮补采 b、c
    {
        let conn = Connection::open(zcode_db_path(home)).unwrap();
        conn.execute(
            "UPDATE model_usage SET status='completed', completed_at=?1, duration_ms=?2,
                 output_tokens=96 WHERE logical_request_id='req-b'",
            rusqlite::params![NOW_MS, 1_498],
        )
        .unwrap();
    }
    let batch2 = zcode::read_incremental(home, &batch1.next_cursor, NOW_MS).unwrap();
    let keys2: std::collections::BTreeSet<_> = batch2
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    assert_eq!(keys2, str_set(&["req-b", "req-c"]));
}

/// 僵尸 running 行（24h 安全阀）：水位强制跨过，不冻死；该行无终态不入观察
#[test]
fn zcode_zombie_running_row_is_crossed() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    {
        let conn = zcode_create_db(home);
        // 崩溃残留：running 且 started_at 早于 25h（NOW_MS 基准下）
        zc_insert(
            &conn,
            "req-zombie",
            "running",
            Some(BASE_MS - 25 * 60 * 60 * 1000),
            None,
            None,
            None,
            None,
            0,
            0,
            "main_turn",
        );
        zc_insert(
            &conn,
            "req-after",
            "completed",
            Some(ZC_START),
            Some(ZC_FIRST),
            Some(ZC_DONE),
            Some(7_757),
            Some(96),
            0,
            0,
            "main_turn",
        );
    }
    let batch = zcode::read_incremental(home, &SourceCursor::empty("zcode"), NOW_MS).unwrap();
    let keys: std::collections::BTreeSet<_> = batch
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    assert_eq!(
        keys,
        str_set(&["req-after"]),
        "僵尸行被跨过（无终态不入观察），后面的行不被水位冻死"
    );
    assert_eq!(
        zcode_cursor_of(&batch.next_cursor),
        (2, 0, 0),
        "水位越过僵尸行（无新鲜 running 行挡住 → 无机会窗）"
    );
}

/// 僵尸行之后的新鲜 running 行：水位必须停在它之前（越过即永久丢失其完成观测）；
/// 该行原地 UPDATE 收尾后与后续行一起补采
#[test]
fn zcode_zombie_then_fresh_running_keeps_watermark_behind() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    {
        let conn = zcode_create_db(home);
        // 僵尸行在前（started_at 25h 前 = 崩溃残留，安全阀跨过）
        zc_insert(
            &conn,
            "req-zombie",
            "running",
            Some(BASE_MS - 25 * 60 * 60 * 1000),
            None,
            None,
            None,
            None,
            0,
            0,
            "main_turn",
        );
        // 新鲜 running 行在后（正在生成，稍后原地 UPDATE 收尾）
        zc_insert(
            &conn,
            "req-live",
            "running",
            Some(NOW_MS - 1_000),
            Some(NOW_MS - 900),
            None,
            None,
            None,
            0,
            0,
            "main_turn",
        );
        // 其后已有完成的行
        zc_insert(
            &conn,
            "req-after",
            "completed",
            Some(ZC_START),
            Some(ZC_FIRST),
            Some(ZC_DONE),
            Some(7_757),
            Some(96),
            0,
            0,
            "main_turn",
        );
    }
    let batch1 = zcode::read_incremental(home, &SourceCursor::empty("zcode"), NOW_MS).unwrap();
    let keys1: std::collections::BTreeSet<_> = batch1
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    assert!(
        !keys1.contains("req-zombie") && !keys1.contains("req-live"),
        "僵尸行不入观察，新鲜 running 行不入观察（终态未到）"
    );
    assert_eq!(
        zcode_cursor_of(&batch1.next_cursor),
        (1, ZC_DONE, 3),
        "水位停在新鲜 running 行之前（僵尸行已被跨过）；其后完成行经机会采样入观察、
         机会游标推进到本轮最新完成事实 (completed_at, rowid)"
    );
    assert!(
        keys1.contains("req-after"),
        "M3 收尾：新鲜 running 头之后的完成行经机会采样入观察（不挡已完成的独立调用），\
         水位仍不动: {keys1:?}"
    );

    // 新鲜行原地 UPDATE 收尾 → 与其后的完成行一起补采；僵尸行在水位之后永不再读
    {
        let conn = Connection::open(zcode_db_path(home)).unwrap();
        conn.execute(
            "UPDATE model_usage SET status='completed', completed_at=?1, duration_ms=?2,
                 output_tokens=64, time_to_first_token_ms=?1 - ?3
             WHERE logical_request_id='req-live'",
            rusqlite::params![NOW_MS, 1_498, NOW_MS - 1_000],
        )
        .unwrap();
    }
    let batch2 = zcode::read_incremental(home, &batch1.next_cursor, NOW_MS).unwrap();
    let keys: std::collections::BTreeSet<_> = batch2
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    assert_eq!(
        keys,
        str_set(&["req-live", "req-after"]),
        "收尾的新鲜行与其后完成行被补采，完成观测不丢"
    );
}

/// 单次读取最多尾 200 行（Spec §2 单次原生 SQL 每宿主最多尾 200 行）。
/// 独立复核 R7 后的契约（M3 起尾读 DESC，最新优先）：
/// * 积压为**陈旧**行（结束于 10 分钟保护窗之前）→ 首轮跳到近期尾部，读最新 200 行并留
///   「skipped 50 stale rows」诊断，其余陈旧行不再追赶（第二轮为空）；
/// * 积压为**近期**行（保护窗内完成）→ 被跳过区由补采找回，尾读取**最新** 150 行；
///   尾窗之外的更旧余量（51..100，读时已 > 120s 进不了任何采样窗）按「最新优先」
///   跳过语义落在水位之下不再读——尾读 DESC 推翻的正是「余量按 rowid 排队续读」。
#[test]
fn zcode_read_is_bounded_to_200_rows() {
    // —— 陈旧积压（夹具结束时刻在 NOW_MS 前约 2 小时）
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    {
        let conn = zcode_create_db(home);
        for i in 0..250 {
            zc_insert(
                &conn,
                &format!("req-{i}"),
                "completed",
                Some(ZC_START + i),
                Some(ZC_FIRST + i),
                Some(ZC_DONE + i),
                Some(7_757),
                Some(9),
                0,
                0,
                "main_turn",
            );
        }
    }
    let batch1 = zcode::read_incremental(home, &SourceCursor::empty("zcode"), NOW_MS).unwrap();
    assert_eq!(batch1.observations.len(), 200, "单次读取不超过 200 行");
    assert!(
        batch1
            .diagnostics
            .iter()
            .any(|d| d.contains("skipped 50 stale rows")),
        "陈旧积压：尾窗外 50 行被跳过且留诊断: {:?}",
        batch1.diagnostics
    );
    let batch2 = zcode::read_incremental(home, &batch1.next_cursor, NOW_MS).unwrap();
    assert_eq!(batch2.observations.len(), 0, "陈旧行不再追赶");

    // —— 近期积压（全部在 10 分钟保护窗内完成）：不跳过，200 + 50 余量
    let dir2 = tempfile::tempdir().unwrap();
    let home2 = dir2.path();
    {
        let conn = zcode_create_db(home2);
        for i in 0..250 {
            let end = NOW_MS - 5 * 60_000 + i;
            zc_insert(
                &conn,
                &format!("fresh-{i}"),
                "completed",
                Some(end - 7_757),
                Some(end - 1_498),
                Some(end),
                Some(7_757),
                Some(9),
                0,
                0,
                "main_turn",
            );
        }
    }
    let b1 = zcode::read_incremental(home2, &SourceCursor::empty("zcode"), NOW_MS).unwrap();
    assert_eq!(b1.observations.len(), 200, "近期积压仍受单次 200 行上限");
    assert!(
        !b1.diagnostics.iter().any(|d| d.contains("stale")),
        "保护窗内的行一行不跳（补采找回了被跳过区）: {:?}",
        b1.diagnostics
    );
    let newest: std::collections::BTreeSet<_> = b1
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    assert!(
        newest.contains("fresh-249")
            && newest.contains("fresh-100")
            && !newest.contains("fresh-99"),
        "尾读 DESC 优先最新 150 行（rowid 101..250）: {:?}",
        b1.diagnostics
    );
    let b2 = zcode::read_incremental(home2, &b1.next_cursor, NOW_MS).unwrap();
    assert_eq!(
        b2.observations.len(),
        0,
        "尾窗之外的更旧余量在水位之下，不再追赶（尾读 DESC：最新优先）"
    );
}

/// started_at 缺失：整行跳过 + 诊断，绝不伪装成 0/1970 起点
#[test]
fn zcode_null_started_at_is_diagnostic_not_zero() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    {
        let conn = zcode_create_db(home);
        zc_insert(
            &conn,
            "req-null",
            "completed",
            None,
            Some(ZC_FIRST),
            Some(ZC_DONE),
            Some(7_757),
            Some(96),
            0,
            0,
            "main_turn",
        );
    }
    let batch = zcode::read_incremental(home, &SourceCursor::empty("zcode"), NOW_MS).unwrap();
    assert_eq!(batch.observations.len(), 0, "缺起点不产观察");
    assert!(
        batch.diagnostics.iter().any(|d| d.contains("started_at")),
        "{:?}",
        batch.diagnostics
    );
}

/// 未装 zcode（库文件不存在）= 正常空态：ok 批、零观察、游标原样
#[test]
fn zcode_missing_db_is_empty_ok_batch() {
    let dir = tempfile::tempdir().unwrap();
    let batch = zcode::read_incremental(dir.path(), &SourceCursor::empty("zcode"), NOW_MS).unwrap();
    assert!(batch.observations.is_empty());
    assert!(batch.source_state.ok);
    assert_eq!(batch.next_cursor.value, "");
}

/// 辅助请求标 purpose（Spec §3：辅助标题/自动摘要默认不进主任务曲线）
#[test]
fn zcode_task_type_marks_title_and_summary_purpose() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    {
        let conn = zcode_create_db(home);
        zc_insert(
            &conn,
            "req-t",
            "completed",
            Some(ZC_START),
            Some(ZC_FIRST),
            Some(ZC_DONE),
            Some(7_757),
            Some(96),
            0,
            0,
            "session_title",
        );
        zc_insert(
            &conn,
            "req-s",
            "completed",
            Some(ZC_START + 1),
            Some(ZC_FIRST + 1),
            Some(ZC_DONE + 1),
            Some(7_757),
            Some(9),
            0,
            0,
            "goal_summary_title",
        );
        zc_insert(
            &conn,
            "req-m",
            "completed",
            Some(ZC_START + 2),
            Some(ZC_FIRST + 2),
            Some(ZC_DONE + 2),
            Some(7_757),
            Some(9),
            0,
            0,
            "main_turn",
        );
    }
    let batch = zcode::read_incremental(home, &SourceCursor::empty("zcode"), NOW_MS).unwrap();
    let title = batch
        .observations
        .iter()
        .find(|o| o.stable_call_key == "req-t")
        .unwrap();
    let summary = batch
        .observations
        .iter()
        .find(|o| o.stable_call_key == "req-s")
        .unwrap();
    let main = batch
        .observations
        .iter()
        .find(|o| o.stable_call_key == "req-m")
        .unwrap();
    assert_eq!(title.purpose, Purpose::AssistTitle);
    assert_eq!(summary.purpose, Purpose::AutoSummary);
    assert_eq!(main.purpose, Purpose::MainTask);
}

/// R7（独立复核 P2）：冷启动/积压不得从最旧记录追赶。
/// ①>200 条旧完成行 + 最后一条近期完成行：空游标首轮即读到近期调用并可进 120s 点；
/// ②旧游标 + 大量堆积 + 近期行：同样跳到近期尾部；
/// ③陈旧区里的新鲜 running 行：水位不得越过它（原地 UPDATE 收尾后必须被读到）。
#[test]
fn zcode_cold_start_and_backlog_recover_recent_tail_first() {
    fn old_completed(conn: &Connection, id: &str, end: i64) {
        zc_insert(
            conn,
            id,
            "completed",
            Some(end - 7_757),
            Some(end - 6_000),
            Some(end),
            Some(7_757),
            Some(96),
            0,
            0,
            "main_turn",
        );
    }
    // ① 冷启动
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let recent_end = NOW_MS - 60_000;
    {
        let conn = zcode_create_db(home);
        for i in 0..500 {
            old_completed(
                &conn,
                &format!("old-{i}"),
                NOW_MS - 2 * 3_600_000 + i as i64 * 10,
            );
        }
        old_completed(&conn, "recent-1", recent_end);
    }
    let batch = zcode::read_incremental(home, &SourceCursor::empty("zcode"), NOW_MS).unwrap();
    assert!(
        batch
            .observations
            .iter()
            .any(|o| o.stable_call_key.contains("recent-1")),
        "冷启动首轮必须读到近期调用；实际 {} 条观察，诊断 {:?}",
        batch.observations.len(),
        batch.diagnostics
    );
    assert!(
        batch.diagnostics.iter().any(|d| d.contains("stale")),
        "跳过陈旧行必须留诊断: {:?}",
        batch.diagnostics
    );
    let points = build_point(&batch.observations, &[], "run-1", NOW_MS);
    assert!(
        points.iter().any(|p| p.status == PointStatus::Value),
        "近期调用进 120s 点"
    );
    let again = zcode::read_incremental(home, &batch.next_cursor, NOW_MS).unwrap();
    assert!(again.observations.is_empty(), "到尾后重读为空");

    // ② 积压恢复：游标停在 rowid 3，之后堆了 400 条旧行 + 1 条近期行
    let dir2 = tempfile::tempdir().unwrap();
    let home2 = dir2.path();
    {
        let conn = zcode_create_db(home2);
        for i in 0..3 {
            old_completed(&conn, &format!("seen-{i}"), NOW_MS - 3 * 3_600_000 + i);
        }
        for i in 0..400 {
            old_completed(
                &conn,
                &format!("stale-{i}"),
                NOW_MS - 2 * 3_600_000 + i as i64 * 10,
            );
        }
        old_completed(&conn, "recent-2", recent_end);
    }
    let stale_cursor = SourceCursor {
        source: "zcode".into(),
        value: "3".into(),
    };
    let batch2 = zcode::read_incremental(home2, &stale_cursor, NOW_MS).unwrap();
    assert!(
        batch2
            .observations
            .iter()
            .any(|o| o.stable_call_key.contains("recent-2")),
        "积压恢复首轮必须读到近期调用；诊断 {:?}",
        batch2.diagnostics
    );

    // ③ 陈旧区里的新鲜 running 行：跳跃不得越过它
    let dir3 = tempfile::tempdir().unwrap();
    let home3 = dir3.path();
    let running_rowid;
    {
        let conn = zcode_create_db(home3);
        for i in 0..300 {
            old_completed(
                &conn,
                &format!("a-{i}"),
                NOW_MS - 2 * 3_600_000 + i as i64 * 10,
            );
        }
        running_rowid = zc_insert(
            &conn,
            "long-running",
            "running",
            Some(NOW_MS - 90_000),
            None,
            None,
            None,
            None,
            0,
            0,
            "main_turn",
        );
        for i in 0..300 {
            old_completed(&conn, &format!("b-{i}"), NOW_MS - 3_600_000 + i as i64 * 10);
        }
    }
    let batch3 = zcode::read_incremental(home3, &SourceCursor::empty("zcode"), NOW_MS).unwrap();
    assert!(
        batch3
            .observations
            .iter()
            .all(|o| !o.stable_call_key.contains("long-running")),
        "新鲜 running 行本身不得入观察（终态未到）"
    );
    assert!(
        batch3
            .observations
            .iter()
            .any(|o| o.stable_call_key.contains("b-")),
        "M3 收尾：新鲜 running 头之后的完成行经机会采样入观察（不挡已完成的独立调用）"
    );
    let (watermark3, _t3, _r3) = zcode_cursor_of(&batch3.next_cursor);
    assert!(
        watermark3 < running_rowid,
        "水位必须停在新鲜 running 行之前: cursor={watermark3} running={running_rowid}"
    );
    // 原地 UPDATE 收尾后：该行必须被读到
    {
        let conn = Connection::open(zcode_db_path(home3)).unwrap();
        conn.execute(
            "UPDATE model_usage SET status='completed', first_token_at=?1, completed_at=?2, duration_ms=?3, output_tokens=96 WHERE rowid=?4",
            rusqlite::params![NOW_MS - 40_000, NOW_MS - 30_000, 60_000, running_rowid],
        )
        .unwrap();
    }
    let batch4 = zcode::read_incremental(home3, &batch3.next_cursor, NOW_MS).unwrap();
    assert!(
        batch4
            .observations
            .iter()
            .any(|o| o.stable_call_key.contains("long-running")),
        "收尾后的原 running 行必须被读到（不因后方堆积而被跳过）；诊断 {:?}",
        batch4.diagnostics
    );
}

/// R7' 反例 1（评审追记）：第一条「近期完成」行卡在陈旧区最前面时，旧保护探针把跳转
/// 整个取消（target = rowid−1 = 0），升序尾读被 200 行陈旧历史占满——近期尾部多轮之后
/// 才被读到。修复后：水位跳转不再受「近期完成行」约束（只受新鲜 running 行约束），
/// 被跳过区域里的近期完成行由独立的补采查询**同轮**找回，尾读照常直达近期尾部。
///
/// 断言的轮次划分说明：jump = max−200、补采吃掉预算（tail LIMIT = 200−recovered）时，
/// 尾读 **DESC 取最新 199 行**（rowid 304..502）——最新尾行当轮即得（M3：升序尾读会
/// 把它挤到下一轮，后台 600 秒/轮节奏下错过一切采样点），被挤出的只是尾窗外更陈旧的
/// rowid 303（跳过语义内）。本测断言其意图面：近期完成行同轮找回 + 尾区不被陈旧历史
/// 挤占 + 新鲜尾行当轮取得 + 到尾后重读为空。
#[test]
fn zcode_recent_completion_behind_stale_history_is_recovered_same_round() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    {
        let conn = zcode_create_db(home);
        // rowid 1：5 分钟前完成（旧保护探针会卡在它上面）
        zc_insert(
            &conn,
            "req-protected",
            "completed",
            Some(NOW_MS - 5 * 60_000 - 7_757),
            Some(NOW_MS - 5 * 60_000 - 1_498),
            Some(NOW_MS - 5 * 60_000),
            Some(7_757),
            Some(96),
            0,
            0,
            "main_turn",
        );
        // rowids 2..501：一小时前完成的陈旧积压
        for i in 0..500 {
            zc_insert(
                &conn,
                &format!("req-stale-{i}"),
                "completed",
                Some(ZC_START + i),
                Some(ZC_FIRST + i),
                Some(ZC_DONE + i),
                Some(7_757),
                Some(9),
                0,
                0,
                "main_turn",
            );
        }
        // rowid 502：30 秒前完成（新鲜尾行）
        zc_insert(
            &conn,
            "req-newest",
            "completed",
            Some(NOW_MS - 30_000 - 7_757),
            Some(NOW_MS - 30_000 - 1_498),
            Some(NOW_MS - 30_000),
            Some(7_757),
            Some(64),
            0,
            0,
            "main_turn",
        );
    }
    let batch1 = zcode::read_incremental(home, &SourceCursor::empty("zcode"), NOW_MS).unwrap();
    let keys1: std::collections::BTreeSet<_> = batch1
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    assert!(
        keys1.contains("req-protected") && keys1.contains("req-newest"),
        "第 1 轮必须补采被跳过区的近期完成行，且 DESC 尾读当轮取得最新尾行（反例核心事实）；实际 keys {keys1:?}，诊断 {:?}",
        batch1.diagnostics
    );
    assert!(
        !keys1.contains("req-stale-0") && !keys1.contains("req-stale-100"),
        "被跳过区域里的陈旧行（rowid ≤ 302）不得挤占第 1 轮（跳转必须生效，不得被第一条近期完成行取消）"
    );
    assert_eq!(
        batch1.observations.len(),
        200,
        "第 1 轮 = 补采 1 行 + 尾窗 199 行，恰满单批上限"
    );
    assert!(
        batch1.diagnostics.iter().any(|d| d.contains("stale")),
        "跳过陈旧行必须留诊断: {:?}",
        batch1.diagnostics
    );
    assert!(
        batch1.observations.len() <= 200,
        "单批 ≤ 200 行（补采 + 尾读合计）"
    );

    // 第 2 轮（库未变）：尾读 DESC 已在首轮取得最新行（rowid 304..502 含 req-newest），
    // 水位已到尾 → 空批；被挤出的只有尾窗外更陈旧的 rowid 303（跳过语义内，不追赶）
    let batch2 = zcode::read_incremental(home, &batch1.next_cursor, NOW_MS).unwrap();
    assert!(
        batch2.observations.is_empty(),
        "补采行与已消费行不重读，无遗漏样本: {:?}",
        batch2
            .observations
            .iter()
            .map(|o| o.stable_call_key.clone())
            .collect::<Vec<_>>()
    );
}

/// M3（三轮复审）原反例 + 后台节奏：补采占去一个名额后，尾读必须**优先最新行**
///（DESC 取最新 (200−补采) 行再反转）。升序尾读会把唯一在 120 秒窗内的新鲜样本
///（rowid 502）挤出当轮——后台 600 秒/轮的节奏下，下一轮读到它时已过去 630 秒，
/// 错过一切采样点。修复后：第 1 轮同时取得补采行与最新尾行；时钟 +600s 的下一轮
/// 为空（无遗漏样本）。
#[test]
fn zcode_tail_read_takes_newest_rows_first_surviving_background_cadence() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    {
        let conn = zcode_create_db(home);
        // rowid 1：5 分钟前完成（被跳过区里的近期完成行，补采找回）
        zc_insert(
            &conn,
            "req-protected",
            "completed",
            Some(NOW_MS - 5 * 60_000 - 7_757),
            Some(NOW_MS - 5 * 60_000 - 1_498),
            Some(NOW_MS - 5 * 60_000),
            Some(7_757),
            Some(96),
            0,
            0,
            "main_turn",
        );
        // rowids 2..501：一小时前完成的陈旧积压
        for i in 0..500 {
            zc_insert(
                &conn,
                &format!("req-stale-{i}"),
                "completed",
                Some(ZC_START + i),
                Some(ZC_FIRST + i),
                Some(ZC_DONE + i),
                Some(7_757),
                Some(9),
                0,
                0,
                "main_turn",
            );
        }
        // rowid 502：30 秒前完成（当轮唯一仍在 120 秒窗内的样本）
        zc_insert(
            &conn,
            "req-newest",
            "completed",
            Some(NOW_MS - 30_000 - 7_757),
            Some(NOW_MS - 30_000 - 1_498),
            Some(NOW_MS - 30_000),
            Some(7_757),
            Some(64),
            0,
            0,
            "main_turn",
        );
    }
    // 第 1 轮：补采 rowid 1 + 尾读最新 199 行（rowid 304..502，含 502）
    let batch1 = zcode::read_incremental(home, &SourceCursor::empty("zcode"), NOW_MS).unwrap();
    let keys1: std::collections::BTreeSet<_> = batch1
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    assert!(
        keys1.contains("req-protected") && keys1.contains("req-newest"),
        "第 1 轮必须同时含被跳过区的近期完成行与最新尾行: keys {keys1:?}，诊断 {:?}",
        batch1.diagnostics
    );
    assert_eq!(
        batch1.observations.len(),
        200,
        "单批恰满 200（补采 1 + 尾读 199）"
    );
    assert!(
        !keys1.contains("req-stale-0") && !keys1.contains("req-stale-100"),
        "陈旧行仍不得挤占第 1 轮（DESC 丢掉的只是尾窗外更陈旧的行，跳过语义内）"
    );
    // 出点断言：唯一在窗内的样本（502）进 120s 采样点
    let points = build_point(&batch1.observations, &[], "run-m3", NOW_MS);
    assert!(
        points.iter().any(|p| p.status == PointStatus::Value),
        "唯一新鲜样本必须进 120s 点"
    );

    // 第 2 轮：时钟 +600s（后台节奏）→ 空批（502 第 1 轮已读，无遗漏样本）
    let batch2 = zcode::read_incremental(home, &batch1.next_cursor, NOW_MS + 600_000).unwrap();
    assert!(
        batch2.observations.is_empty(),
        "后台下一轮不得再冒出遗漏样本: {:?}",
        batch2
            .observations
            .iter()
            .map(|o| o.stable_call_key.clone())
            .collect::<Vec<_>>()
    );
}

/// M3 收尾（三轮复审 remainder）：新鲜 running 行卡在**积压区头部**时，其后的近期完成
/// 调用不得被全部挡在采样窗之外——「近期完成事实恢复」与「未终态水位保护」解耦：
/// 机会采样在**绝不越过** running 行的前提下读取其后终态行（稳定键幂等入观察、水位
/// 不动、机会窗游标逐轮推进）；running 行原地 UPDATE 收尾后，正常路径从水位恢复推进
///（其间被机会采过的行重读一次，幂等自愈）。
#[test]
fn zcode_opportunistic_sample_beyond_fresh_running_head() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    {
        let conn = zcode_create_db(home);
        // rowid 1：新鲜 running（积压区头部，正在生成、稍后原地 UPDATE 收尾）
        zc_insert(
            &conn,
            "req-head-running",
            "running",
            Some(NOW_MS - 90_000),
            None,
            None,
            None,
            None,
            0,
            0,
            "main_turn",
        );
        // rowids 2..501：一小时前完成的陈旧积压
        for i in 0..500 {
            zc_insert(
                &conn,
                &format!("req-stale-{i}"),
                "completed",
                Some(ZC_START + i),
                Some(ZC_FIRST + i),
                Some(ZC_DONE + i),
                Some(7_757),
                Some(9),
                0,
                0,
                "main_turn",
            );
        }
        // rowid 502：30 秒前完成（当轮唯一仍在 120 秒窗内的样本）
        zc_insert(
            &conn,
            "req-newest",
            "completed",
            Some(NOW_MS - 30_000 - 7_757),
            Some(NOW_MS - 30_000 - 1_498),
            Some(NOW_MS - 30_000),
            Some(7_757),
            Some(64),
            0,
            0,
            "main_turn",
        );
    }
    // 第 1 轮：机会采样产出 502（及其前的最新终态行），水位/游标不得越过 rowid 1
    let batch1 = zcode::read_incremental(home, &SourceCursor::empty("zcode"), NOW_MS).unwrap();
    let keys1: std::collections::BTreeSet<_> = batch1
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    assert!(
        keys1.contains("req-newest"),
        "机会采样必须取得 running 行之后的近期完成调用: keys {keys1:?}，诊断 {:?}",
        batch1.diagnostics
    );
    assert!(
        !keys1.iter().any(|k| k.contains("req-head-running")),
        "running 行本身不得入观察（终态未到）"
    );
    assert!(
        batch1
            .diagnostics
            .iter()
            .any(|d| d.contains("opportunistic sample beyond fresh-running head")),
        "机会采样必须留诊断: {:?}",
        batch1.diagnostics
    );
    assert!(
        batch1.observations.len() <= 200,
        "机会采样受单批 ≤200 行硬闸约束"
    );
    let cursor1: Value = serde_json::from_str(&batch1.next_cursor.value).unwrap();
    assert_eq!(
        cursor1["watermark"], 0,
        "水位停在新鲜 running 行之前（绝不越过）"
    );
    assert_eq!(
        (
            cursor1["op_completed_at"].as_i64(),
            cursor1["op_rowid"].as_i64()
        ),
        (Some(NOW_MS - 30_000), Some(502)),
        "机会游标 = 本轮机会采样的最新完成事实 (completed_at, rowid)"
    );

    // 第 2 轮（库不变）：机会窗已推进到本轮最大 rowid → 空批（不重复产观察）
    let batch2 = zcode::read_incremental(home, &batch1.next_cursor, NOW_MS).unwrap();
    assert!(
        batch2.observations.is_empty(),
        "机会窗内无新终态行 → 空批: {:?}",
        batch2
            .observations
            .iter()
            .map(|o| o.stable_call_key.clone())
            .collect::<Vec<_>>()
    );

    // 旧格式游标（纯 rowid 十进制串，升级自前版二进制）向后兼容：等价
    // {watermark: n, opportunistic_until: 0}
    let legacy = SourceCursor {
        source: "zcode".into(),
        value: "0".into(),
    };
    let batch_legacy = zcode::read_incremental(home, &legacy, NOW_MS).unwrap();
    assert!(
        batch_legacy
            .observations
            .iter()
            .any(|o| o.stable_call_key == "req-newest"),
        "旧格式游标必须可解析并按水位 0 等价工作"
    );

    // rowid 1 原地 UPDATE 收尾 → 下一轮正常路径从水位恢复推进：rowid 1 被补采
    {
        let conn = Connection::open(zcode_db_path(home)).unwrap();
        conn.execute(
            "UPDATE model_usage SET status='completed', first_token_at=?1, completed_at=?2,
                 duration_ms=7757, output_tokens=64
             WHERE logical_request_id='req-head-running'",
            rusqlite::params![NOW_MS - 30_000 - 1_498, NOW_MS - 30_000],
        )
        .unwrap();
    }
    let batch3 = zcode::read_incremental(home, &batch2.next_cursor, NOW_MS).unwrap();
    let keys3: std::collections::BTreeSet<_> = batch3
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    assert!(
        keys3.contains("req-head-running"),
        "running 行收尾后必须被补采（水位从未越过它，终态不丢）: keys {keys3:?}，诊断 {:?}",
        batch3.diagnostics
    );
}

/// 四轮复审 F2 反例：机会游标 = 「本轮最大终态 rowid」会系统性排除**更小 rowid、更晚
/// 完成**的并行调用——rowid 1/2 都在飞行、rowid 3 已完成时，首轮机会采样把游标推到
/// rowid 3；rowid 2 随后原地收尾（completed_at 晚于 rowid 3）却被 rowid 下界挡住，
/// 直到 rowid 1 也收尾才有机会被读到（采样窗口早已错过）。修复后机会游标改
/// (completed_at, rowid) **完成时间复合游标**：原地完成的事实带更晚的 completed_at →
/// 下轮必被机会采样发现，rowid 序不再构成排除面；主水位保护、不重复计数、200 行上界
/// 全部保持。
#[test]
fn zcode_opportunistic_cursor_follows_completion_time_not_rowid() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    {
        let conn = zcode_create_db(home);
        // rowid 1：新鲜 running（最后收尾）
        zc_insert(
            &conn,
            "req-1",
            "running",
            Some(NOW_MS - 90_000),
            Some(NOW_MS - 89_000),
            None,
            None,
            None,
            0,
            0,
            "main_turn",
        );
        // rowid 2：新鲜 running（先于 rowid 1 原地收尾）
        zc_insert(
            &conn,
            "req-2",
            "running",
            Some(NOW_MS - 80_000),
            Some(NOW_MS - 79_000),
            None,
            None,
            None,
            0,
            0,
            "main_turn",
        );
        // rowid 3：30 秒前完成（首轮唯一可采样本）
        zc_insert(
            &conn,
            "req-3",
            "completed",
            Some(NOW_MS - 30_000 - 7_757),
            Some(NOW_MS - 30_000 - 1_498),
            Some(NOW_MS - 30_000),
            Some(7_757),
            Some(64),
            0,
            0,
            "main_turn",
        );
    }
    // 第 1 轮：机会采样取得 rowid 3，水位不动（不越过在飞行行）
    let batch1 = zcode::read_incremental(home, &SourceCursor::empty("zcode"), NOW_MS).unwrap();
    let keys1: std::collections::BTreeSet<_> = batch1
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    assert_eq!(
        keys1,
        str_set(&["req-3"]),
        "第 1 轮只取得已完成的 rowid 3: {keys1:?}"
    );
    let cur1: Value = serde_json::from_str(&batch1.next_cursor.value).unwrap();
    assert_eq!(
        cur1["watermark"], 0,
        "水位停在新鲜 running 行之前（绝不越过）"
    );

    // 第 2 轮：时钟 +600s（后台节奏），rowid 2 原地收尾（completed_at = 新当前 −30s），
    // rowid 1 仍在飞行 → rowid 2 必须同轮成为有效样本（反例核心：rowid 下界不得排除它）
    let now2 = NOW_MS + 600_000;
    {
        let conn = Connection::open(zcode_db_path(home)).unwrap();
        conn.execute(
            "UPDATE model_usage SET status='completed', first_token_at=?1, completed_at=?2,
                 duration_ms=7757, output_tokens=64 WHERE rowid=2",
            rusqlite::params![now2 - 30_000 - 1_498, now2 - 30_000],
        )
        .unwrap();
    }
    let batch2 = zcode::read_incremental(home, &batch1.next_cursor, now2).unwrap();
    let keys2: std::collections::BTreeSet<_> = batch2
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    assert_eq!(
        keys2,
        str_set(&["req-2"]),
        "原地收尾的 rowid 2 必须被机会采样发现（更小 rowid、更晚完成）: {keys2:?}，诊断 {:?}",
        batch2.diagnostics
    );
    let points = build_point(&batch2.observations, &[], "run-f2", now2);
    assert!(
        points.iter().any(|p| p.status == PointStatus::Value),
        "rowid 2 是当轮仍在 120 秒窗内的有效样本"
    );
    let cur2: Value = serde_json::from_str(&batch2.next_cursor.value).unwrap();
    assert!(
        cur2["watermark"].as_i64().unwrap_or(0) < 1,
        "水位仍停在 rowid 1 之前（它还在飞行）"
    );
    // 机会游标推进到各轮取得的最新 (completed_at, rowid)
    assert_eq!(
        (cur1["op_completed_at"].as_i64(), cur1["op_rowid"].as_i64()),
        (Some(NOW_MS - 30_000), Some(3)),
        "第 1 轮机会游标 = 本轮最新完成事实 (completed_at, rowid)"
    );
    assert_eq!(
        (cur2["op_completed_at"].as_i64(), cur2["op_rowid"].as_i64()),
        (Some(now2 - 30_000), Some(2)),
        "第 2 轮机会游标推进到 rowid 2 的完成事实"
    );

    // 第 3 轮：rowid 1 也收尾 → 正常路径从水位恢复推进：rowid 1 被补采，
    // 2/3 幂等重读一次（稳定键 UPSERT，批内键唯一、无重复计数）
    let now3 = now2 + 600_000;
    {
        let conn = Connection::open(zcode_db_path(home)).unwrap();
        conn.execute(
            "UPDATE model_usage SET status='completed', first_token_at=?1, completed_at=?2,
                 duration_ms=7757, output_tokens=64 WHERE rowid=1",
            rusqlite::params![now3 - 30_000 - 1_498, now3 - 30_000],
        )
        .unwrap();
    }
    let batch3 = zcode::read_incremental(home, &batch2.next_cursor, now3).unwrap();
    let keys3: std::collections::BTreeSet<_> = batch3
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    assert_eq!(
        keys3,
        str_set(&["req-1", "req-2", "req-3"]),
        "收尾后正常路径补采 rowid 1 并幂等重读 2/3（无重复观察形态）: {keys3:?}"
    );
    assert_eq!(
        batch3.observations.len(),
        3,
        "批内稳定键唯一（机会采样过的行重读不产生第二份观察）"
    );
    let cur3: Value = serde_json::from_str(&batch3.next_cursor.value).unwrap();
    assert_eq!(cur3["watermark"], 3, "水位推进到尾部");

    // 第 4 轮：库不变 → 空批（store 级幂等：同游标重读不产生新观察）
    let batch4 = zcode::read_incremental(home, &batch3.next_cursor, now3).unwrap();
    assert!(
        batch4.observations.is_empty(),
        "幂等：无新观察: {:?}",
        batch4
            .observations
            .iter()
            .map(|o| o.stable_call_key.clone())
            .collect::<Vec<_>>()
    );
}

/// 四轮复审 F2 验收：并行调用**交错完成**——完成顺序与 rowid 序相反（rowid 3 → 2 → 1
/// 逐轮收尾），每行都必须在**它完成的当轮**被采样（后两轮期间 rowid 1 一直在飞行、
/// 水位不动）；任何一方的游标推进都不得排除更小 rowid 的后续完成事实。
#[test]
fn zcode_interleaved_in_place_completions_are_all_sampled() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    {
        let conn = zcode_create_db(home);
        zc_insert(
            &conn,
            "req-a",
            "running",
            Some(NOW_MS - 90_000),
            Some(NOW_MS - 89_000),
            None,
            None,
            None,
            0,
            0,
            "main_turn",
        );
        zc_insert(
            &conn,
            "req-b",
            "running",
            Some(NOW_MS - 80_000),
            Some(NOW_MS - 79_000),
            None,
            None,
            None,
            0,
            0,
            "main_turn",
        );
        zc_insert(
            &conn,
            "req-c",
            "running",
            Some(NOW_MS - 70_000),
            Some(NOW_MS - 69_000),
            None,
            None,
            None,
            0,
            0,
            "main_turn",
        );
        zc_insert(
            &conn,
            "req-d",
            "completed",
            Some(NOW_MS - 30_000 - 7_757),
            Some(NOW_MS - 30_000 - 1_498),
            Some(NOW_MS - 30_000),
            Some(7_757),
            Some(64),
            0,
            0,
            "main_turn",
        );
    }
    // 第 1 轮：只取得 rowid 4
    let batch1 = zcode::read_incremental(home, &SourceCursor::empty("zcode"), NOW_MS).unwrap();
    let keys1: std::collections::BTreeSet<_> = batch1
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    assert_eq!(
        keys1,
        str_set(&["req-d"]),
        "第 1 轮只取得已完成的 rowid 4: {keys1:?}"
    );
    let cur1: Value = serde_json::from_str(&batch1.next_cursor.value).unwrap();
    assert_eq!(cur1["watermark"], 0, "水位停在新鲜 running 行之前");

    // 第 2 轮：rowid 3 收尾（completed_at 晚于 rowid 4）→ 当轮采样；rowid 1/2 仍在飞行
    let now2 = NOW_MS + 600_000;
    {
        let conn = Connection::open(zcode_db_path(home)).unwrap();
        conn.execute(
            "UPDATE model_usage SET status='completed', first_token_at=?1, completed_at=?2,
                 duration_ms=7757, output_tokens=64 WHERE rowid=3",
            rusqlite::params![now2 - 30_000 - 1_498, now2 - 30_000],
        )
        .unwrap();
    }
    let batch2 = zcode::read_incremental(home, &batch1.next_cursor, now2).unwrap();
    let keys2: std::collections::BTreeSet<_> = batch2
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    assert_eq!(
        keys2,
        str_set(&["req-c"]),
        "rowid 3 完成当轮即被采样（不得重取 rowid 4 也不得排除 rowid 3）: {keys2:?}，诊断 {:?}",
        batch2.diagnostics
    );
    let cur2: Value = serde_json::from_str(&batch2.next_cursor.value).unwrap();
    assert!(
        cur2["watermark"].as_i64().unwrap_or(0) < 1,
        "水位不动（rowid 1 仍在飞行）"
    );

    // 第 3 轮：rowid 2 收尾 → 当轮采样（rowid 1 仍在飞行）
    let now3 = now2 + 600_000;
    {
        let conn = Connection::open(zcode_db_path(home)).unwrap();
        conn.execute(
            "UPDATE model_usage SET status='completed', first_token_at=?1, completed_at=?2,
                 duration_ms=7757, output_tokens=64 WHERE rowid=2",
            rusqlite::params![now3 - 30_000 - 1_498, now3 - 30_000],
        )
        .unwrap();
    }
    let batch3 = zcode::read_incremental(home, &batch2.next_cursor, now3).unwrap();
    let keys3: std::collections::BTreeSet<_> = batch3
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    assert_eq!(
        keys3,
        str_set(&["req-b"]),
        "rowid 2 完成当轮即被采样（更小 rowid 的更晚完成不被游标排除）: {keys3:?}，诊断 {:?}",
        batch3.diagnostics
    );

    // 第 4 轮：rowid 1 收尾 → 正常路径从水位恢复推进，四行幂等重读（批内键唯一）
    let now4 = now3 + 600_000;
    {
        let conn = Connection::open(zcode_db_path(home)).unwrap();
        conn.execute(
            "UPDATE model_usage SET status='completed', first_token_at=?1, completed_at=?2,
                 duration_ms=7757, output_tokens=64 WHERE rowid=1",
            rusqlite::params![now4 - 30_000 - 1_498, now4 - 30_000],
        )
        .unwrap();
    }
    let batch4 = zcode::read_incremental(home, &batch3.next_cursor, now4).unwrap();
    let keys4: std::collections::BTreeSet<_> = batch4
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    assert_eq!(
        keys4,
        str_set(&["req-a", "req-b", "req-c", "req-d"]),
        "交错完成的三行都被采样（第二批幂等重读，批内键唯一）: {keys4:?}"
    );
    assert_eq!(batch4.observations.len(), 4, "批内稳定键唯一");

    // 第 5 轮：库不变 → 空批
    let batch5 = zcode::read_incremental(home, &batch4.next_cursor, now4).unwrap();
    assert!(batch5.observations.is_empty(), "幂等：无新观察");
}

/// 四轮复审 F2 验收：两行 completed_at **完全相同**——复合游标 (completed_at, rowid)
/// 的 rowid 决胜位保证两行都只入账一次：首轮一并取得（游标停在同刻里的最大 rowid），
/// 下轮既不因「同刻」重取、也不漏掉同刻的更小 rowid。
#[test]
fn zcode_identical_completed_at_rows_ingested_once_via_composite_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let done_at = NOW_MS - 30_000;
    {
        let conn = zcode_create_db(home);
        zc_insert(
            &conn,
            "req-r1",
            "running",
            Some(NOW_MS - 90_000),
            Some(NOW_MS - 89_000),
            None,
            None,
            None,
            0,
            0,
            "main_turn",
        );
        // rowid 2、3：同一时刻完成（completed_at 逐字相等）
        zc_insert(
            &conn,
            "req-r2",
            "completed",
            Some(done_at - 7_757),
            Some(done_at - 1_498),
            Some(done_at),
            Some(7_757),
            Some(64),
            0,
            0,
            "main_turn",
        );
        zc_insert(
            &conn,
            "req-r3",
            "completed",
            Some(done_at - 7_757),
            Some(done_at - 1_498),
            Some(done_at),
            Some(64),
            Some(32),
            0,
            0,
            "main_turn",
        );
    }
    // 第 1 轮：机会采样一并取得同刻两行，各恰一次
    let batch1 = zcode::read_incremental(home, &SourceCursor::empty("zcode"), NOW_MS).unwrap();
    let keys1: std::collections::BTreeSet<_> = batch1
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    assert_eq!(
        keys1,
        str_set(&["req-r2", "req-r3"]),
        "同刻完成的两行首轮一并取得: {keys1:?}"
    );
    assert_eq!(batch1.observations.len(), 2, "各恰一次");
    let cur1: Value = serde_json::from_str(&batch1.next_cursor.value).unwrap();
    assert_eq!(
        (cur1["op_completed_at"].as_i64(), cur1["op_rowid"].as_i64()),
        (Some(done_at), Some(3)),
        "机会游标停在同刻里的最大 rowid（rowid 决胜位）"
    );

    // 第 2 轮：同刻更小 rowid 已被 (completed_at, rowid) 复合界覆盖 → 不重取（恰一次）
    let batch2 = zcode::read_incremental(home, &batch1.next_cursor, NOW_MS).unwrap();
    assert!(
        batch2.observations.is_empty(),
        "复合游标同刻不重取: {:?}",
        batch2
            .observations
            .iter()
            .map(|o| o.stable_call_key.clone())
            .collect::<Vec<_>>()
    );

    // 第 3 轮：rowid 1 收尾 → 正常路径幂等重读三行
    {
        let conn = Connection::open(zcode_db_path(home)).unwrap();
        conn.execute(
            "UPDATE model_usage SET status='completed', first_token_at=?1, completed_at=?2,
                 duration_ms=7757, output_tokens=64 WHERE rowid=1",
            rusqlite::params![NOW_MS + 570_000 - 1_498, NOW_MS + 570_000],
        )
        .unwrap();
    }
    let batch3 = zcode::read_incremental(home, &batch2.next_cursor, NOW_MS + 570_000).unwrap();
    let keys3: std::collections::BTreeSet<_> = batch3
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    assert_eq!(keys3, str_set(&["req-r1", "req-r2", "req-r3"]));
    assert_eq!(batch3.observations.len(), 3, "批内稳定键唯一");
}

/// 四轮复审 F2 验收：旧游标升级**不得把旧值解释成新语义**——旧 JSON 游标里的
/// `opportunistic_until`（旧「最大已见终态 rowid」下界）在新实现里是未知字段，serde
/// 默认忽略：旧值 3 不得压制 rowid 1–3 的采样；纯数字旧游标仍映射 watermark。
#[test]
fn zcode_legacy_cursor_with_opportunistic_until_is_not_reinterpreted() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    {
        let conn = zcode_create_db(home);
        zc_insert(
            &conn,
            "req-1",
            "running",
            Some(NOW_MS - 90_000),
            Some(NOW_MS - 89_000),
            None,
            None,
            None,
            0,
            0,
            "main_turn",
        );
        zc_insert(
            &conn,
            "req-2",
            "running",
            Some(NOW_MS - 80_000),
            Some(NOW_MS - 79_000),
            None,
            None,
            None,
            0,
            0,
            "main_turn",
        );
        zc_insert(
            &conn,
            "req-3",
            "completed",
            Some(NOW_MS - 30_000 - 7_757),
            Some(NOW_MS - 30_000 - 1_498),
            Some(NOW_MS - 30_000),
            Some(7_757),
            Some(64),
            0,
            0,
            "main_turn",
        );
    }
    // 旧 JSON 游标：未知字段被忽略，旧值 3 不得当机会窗下界（否则 rowid 3 被压制）
    let legacy = SourceCursor {
        source: "zcode".into(),
        value: r#"{"watermark":0,"opportunistic_until":3}"#.into(),
    };
    let batch = zcode::read_incremental(home, &legacy, NOW_MS).unwrap();
    let keys: std::collections::BTreeSet<_> = batch
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    assert!(
        keys.contains("req-3"),
        "旧游标的 opportunistic_until 不得解释成新语义（rowid 3 不被压制）: {keys:?}"
    );
    assert!(
        !keys.contains("req-1") && !keys.contains("req-2"),
        "在飞行行照常不入观察"
    );

    // 纯数字旧游标仍映射 watermark："3" → rowid ≤ 3 全部跳过
    let numeric = SourceCursor {
        source: "zcode".into(),
        value: "3".into(),
    };
    let batch2 = zcode::read_incremental(home, &numeric, NOW_MS).unwrap();
    assert!(
        batch2.observations.is_empty(),
        "纯数字旧游标 = watermark 3（1–3 都在水位之下）"
    );
}

/// R7' 反例 2（评审追记）：保护探针 `... OR status='running' ... LIMIT 64` 会被 64 条僵尸
/// running 行占满截断，藏在其后的**新鲜 running 行**探测不到 → 跳转越过它，该行原地
/// UPDATE 收尾后的终态永远读不到（违反「新鲜 running 行绝不被越过」水位保证）。修复后：
/// 新鲜 running 的约束改为单条 SQL 聚合探测（新鲜度在 SQL 内判定，无 LIMIT 截断）。
#[test]
fn zcode_truncated_probe_cannot_cross_fresh_running_row() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    {
        let conn = zcode_create_db(home);
        // rowids 1..64：僵尸 running（started 25h 前 ≥ 24h 阈值 = 崩溃残留，不构成保护）
        for i in 0..64 {
            zc_insert(
                &conn,
                &format!("req-zombie-{i}"),
                "running",
                Some(NOW_MS - 25 * 60 * 60 * 1000),
                None,
                None,
                None,
                None,
                0,
                0,
                "main_turn",
            );
        }
        // rowid 65：新鲜 running（正在生成，稍后原地 UPDATE 收尾）
        zc_insert(
            &conn,
            "req-fresh-running",
            "running",
            Some(NOW_MS - 90_000),
            None,
            None,
            None,
            None,
            0,
            0,
            "main_turn",
        );
        // rowids 66..565：一小时前完成的陈旧行
        for i in 0..500 {
            zc_insert(
                &conn,
                &format!("req-after-{i}"),
                "completed",
                Some(ZC_START + i),
                Some(ZC_FIRST + i),
                Some(ZC_DONE + i),
                Some(7_757),
                Some(9),
                0,
                0,
                "main_turn",
            );
        }
    }
    let batch1 = zcode::read_incremental(home, &SourceCursor::empty("zcode"), NOW_MS).unwrap();
    let (watermark1, _t1, _r1) = zcode_cursor_of(&batch1.next_cursor);
    assert!(
        watermark1 <= 64,
        "水位必须停在新鲜 running 行（rowid 65）之前，不得被僵尸行截断的探针带过去: cursor={watermark1}"
    );
    assert!(
        batch1
            .observations
            .iter()
            .all(|o| !o.stable_call_key.contains("req-fresh-running")),
        "新鲜 running 行本身不得入观察（终态未到）: {}",
        batch1.observations.len()
    );
    assert!(
        batch1.observations.len() <= 200,
        "机会采样（其后完成行照常入观察）受单批 ≤200 硬闸"
    );

    // 新鲜行原地 UPDATE 收尾 → 下轮补采（它在水位目标之后的被跳过区域内，走补采查询）
    {
        let conn = Connection::open(zcode_db_path(home)).unwrap();
        conn.execute(
            "UPDATE model_usage SET status='completed', first_token_at=?1, completed_at=?2,
                 duration_ms=7757, output_tokens=64 WHERE logical_request_id='req-fresh-running'",
            rusqlite::params![NOW_MS - 30_000 - 1_498, NOW_MS - 30_000],
        )
        .unwrap();
    }
    let batch2 = zcode::read_incremental(home, &batch1.next_cursor, NOW_MS).unwrap();
    let keys2: std::collections::BTreeSet<_> = batch2
        .observations
        .iter()
        .map(|o| o.stable_call_key.clone())
        .collect();
    assert!(
        keys2.contains("req-fresh-running"),
        "收尾后的原新鲜 running 行必须被补采（不因僵尸行占满探针而被永久越过）；诊断 {:?}",
        batch2.diagnostics
    );
}
