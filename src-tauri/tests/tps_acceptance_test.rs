//! TPS 两平台收口验收（2026-10-24 计划 Task 9；Spec §7「验收与收口」）。
//!
//! 与前 8 个任务套件的关系：**只消费既有接口**，不新增采集能力、不改测量口径——
//! 本套件把「原生来源回放 → 事实入库 → 调度拍驱动建点 → 查询快照/曲线」整条链
//! 在一次验收里走通（同一条链就是 IPC `tps_snapshot` / `tps_points` 服务的投影）。
//!
//! R12 真机验收边界：Mac 侧 = 脱敏真实结构**离线回放** + 影子存储；OpenCode 用
//! Task 6 隔离活体捕获的记录形态回放（见
//! `.superpowers/sdd/2026-10-24-tps-sampling-plan/task-6-report.md` §5）；DSH/ZCode/
//! Kimi/WorkBuddy/Claude 的「一次正常请求」入口确认**不在本轮**（需用户正常使用中
//! 自然产生样本，报告如实申报）；Windows 全部检查未运行。
//!
//! 隔离纪律（2026-10-07 事故族，与前序 TPS 套件同款）：
//! * 全部库操作走 `support::open_ledger_db(tag)` 的**用例私有库**（tempdir 真实 DB 文件），
//!   绝不触碰真机 `~/.tuvis` / `~/.dsh` / `~/.zcode` / `~/.kimi-code` / `~/.codex`；
//! * 假时钟贯穿全程（`Sampler` 是纯状态机、`produce_point` 时间入参注入）——不真等
//!   1 小时（Spec §7「用假时钟测试，不真等1小时」）；
//! * 夹具 tempdir 影子 home，形态逐字取自既有脱敏证据（同 tps_native_*_test）；
//! * 不 bind 任何固定端口、不发任何模型请求。

mod support;

use multi_agents_manager_lib::services::tps::collector;
use multi_agents_manager_lib::services::tps::model::{
    Outcome, OutputScope, PointStatus, PointTrigger, ReadBatch, SourceCursor, SourceState,
    TimingMethod, TpsObservation,
};
use multi_agents_manager_lib::services::tps::query::{self, TpsViewRange};
use multi_agents_manager_lib::services::tps::scheduler::{Sampler, TickAction};
use multi_agents_manager_lib::services::tps::sources::opencode;
use multi_agents_manager_lib::services::tps::store;
use rusqlite::Connection;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// 固定假时钟基准（纯整数，不读系统时钟）。T0 = 应用启动时刻（Startup 点锚点）；
/// 全部夹具事件都锚在 T0 前的 120 秒窗内。
const T0: i64 = 1_700_000_000_000;
/// 后台第二拍的采样时刻（T0 + 600 秒；Spec §2「每600秒生成一个采样点」）
const T1: i64 = T0 + 600_000;

// ==== 通用助手 ====

/// 用例私有库（tempdir 下的真实 DB 文件，schema::init 建齐全部表）
fn conn(tag: &str) -> Connection {
    support::open_ledger_db(tag)
}

/// 绝对行数断言（表缺失时响亮 panic，不静默当 0）
fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap_or_else(|e| panic!("计数查询失败（{table}，表缺失？）：{e}"))
}

/// 旧 usage 四桶播种（与 tps_sampling_test::seed_old_usage 同形：行数不变性基准）
fn seed_old_usage(conn: &Connection) {
    conn.execute_batch(
        "INSERT INTO usage_detail
             (source_id, session_id, project_key, hour_key, day_key, model, provider,
              provider_kind, input_fresh, cache_read, cache_write, output,
              request_total, requests, user_est, first_seen_at, updated_at)
             VALUES ('codex','s1','proj','2026-10-17T10','2026-10-17','gpt-5','openai',
                     'measured',10,2,3,4,12,1,5,100,200);
         INSERT INTO usage_daily
             (day_key, source_id, project_key, provider, provider_kind, model,
              input_fresh, cache_read, cache_write, output, request_total, requests,
              user_est, updated_at)
             VALUES ('2026-10-17','codex','proj','openai','measured','gpt-5',10,2,3,4,12,1,5,200);
         INSERT INTO usage_cursor
             (source_id, session_id, fingerprint, byte_offset, ordinal, last_cumulative,
              mtime_ms, file_size, state_json, updated_at)
             VALUES ('codex','s1','fp',42,3,99,123,456,'{}',789);
         INSERT INTO usage_session
             (source_id, session_id, project_key, project_label, project_path_raw,
              project_realpath, title, is_subagent, parent_session_id, first_seen_at, last_seen_at)
             VALUES ('codex','s1','proj','Proj','/tmp/p','/tmp/p','T',0,NULL,100,200);",
    )
    .expect("播种旧 usage 四表失败");
}

/// 直写 settings 表的 usage.settings（总授权开关；dao 的 *_conn 形态是 pub(crate)）
fn set_usage_settings(conn: &Connection, enabled: bool) {
    conn.execute(
        "INSERT OR REPLACE INTO settings (key, value) VALUES ('usage.settings', ?1)",
        rusqlite::params![format!(
            r#"{{"enabled":{enabled},"miniBarRange":"today","miniBarToolRows":3,"detailRetentionDays":30,"collectIntervalMin":10,"providerMapRules":"","exportQuote":"","exportPose":"random"}}"#
        )],
    )
    .unwrap();
}

/// 调度拍驱动的整链执行（与 collector::runtime_step 同构，但时间注入、库注入）：
/// 摘取 tick 动作后按序执行 Read（采集轮）→ EmitPoint（建点），返回建点数。
fn execute_tick(
    conn: &mut Connection,
    sampler: &mut Sampler,
    home: &Path,
    now_ms: i64,
    visible: bool,
) -> usize {
    let out = sampler.tick(now_ms, visible);
    let run_id = sampler.run_id().to_string();
    let mut written = 0usize;
    for action in &out.actions {
        match action {
            TickAction::Read => {
                collector::run_round(conn, home, now_ms);
            }
            TickAction::EmitPoint {
                at_ms,
                trigger,
                cadence_ms,
            } => {
                // 与 runtime_step 同构：点携带发点时的计划节奏（独立复核 R11）
                written += collector::produce_point_with_cadence(
                    conn,
                    &run_id,
                    *at_ms,
                    *trigger,
                    Some(*cadence_ms),
                )
                .unwrap();
            }
        }
    }
    written
}

// ==== DSH 夹具（形态 = tps_native_primary_test 同源的脱敏结构；base 参数化）====

fn dsh_session_dir(home: &Path, session: &str) -> PathBuf {
    home.join(".dsh")
        .join("sessions")
        .join("proj-a")
        .join(session)
}

fn dsh_event(kind: &str, seq: i64, time: i64, data: Value) -> String {
    json!({"type": kind, "seq": seq, "time": time, "data": data}).to_string()
}

/// 紧凑段（time0 + dt[]（长度=成员数−1）+ texts[]；首成员 time0，随后按 dt 累加）
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

fn dsh_message_event(
    seq: i64,
    time: i64,
    turn: i64,
    step: i64,
    output_tokens: i64,
    stream: &[Value],
) -> String {
    let mut message = json!({"role": "assistant",
        "content": [{"type": "text", "text": "ok"}], "id": format!("m-{seq}")});
    message["source"] = replay_source();
    let data = json!({"turn": turn, "step": step, "message": message, "stream": stream,
        "usage": {"inputTokens": 10, "outputTokens": output_tokens,
                  "totalTokens": 10 + output_tokens}});
    dsh_event("assistant/message", seq, time, data)
}

/// DSH 思考计入正例（首空 delta 跳过）：结算在 t0+120 → 生成窗 20ms、output 96。
fn write_dsh_session(home: &Path, t0: i64) {
    let lines = [
        json!({"type": "session", "version": 3, "id": "sess-acc", "createdAt": t0,
               "cwd": "/p/a", "delegationDepth": 0})
        .to_string(),
        dsh_event("turn/start", 1, t0 + 10, json!({"turn": 1})),
        dsh_event("step/start", 2, t0 + 20, json!({"turn": 1, "step": 1})),
        dsh_message_event(
            3,
            t0 + 120,
            1,
            1,
            96,
            &[
                compact_chunks("reasoning-chunks", t0 + 60, 0, &[40], &["", "think"]),
                compact_chunks("text-chunks", t0 + 110, 1, &[5, 4], &["hel", "lo", "!"]),
            ],
        ),
        dsh_event("step/end", 4, t0 + 130, json!({"turn": 1, "step": 1})),
    ];
    let dir = dsh_session_dir(home, "sess-acc");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("session.v3.jsonl"), lines.join("\n") + "\n").unwrap();
}

// ==== ZCode 夹具（列集与 tps_native_primary_test 同源）====

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

/// 一行 model_usage（first_token_at/duration_ms 可空：覆盖缺 F / 缺计时形态）
#[allow(clippy::too_many_arguments)]
fn zc_insert(
    conn: &Connection,
    logical_request_id: &str,
    status: &str,
    started_at: i64,
    first_token_at: Option<i64>,
    completed_at: i64,
    duration_ms: Option<i64>,
    output: i64,
) {
    conn.execute(
        "INSERT INTO model_usage (logical_request_id, session_id, turn_id, assistant_message_id,
             provider_id, model_id, status, started_at, first_token_at, completed_at,
             duration_ms, time_to_first_token_ms, input_tokens, output_tokens, reasoning_tokens,
             cache_read_input_tokens, cache_creation_input_tokens, computed_total_tokens,
             retry_count, task_type)
         VALUES (?1, 'sess_z', 'turn_1', 'amsg-1', 'deepseek', 'deepseek-v4.1-flash',
             ?2, ?3, ?4, ?5, ?6, ?4 - ?3, 21112, ?7, 0, 0, 0, 21112 + ?7, 0, 'interactive')",
        rusqlite::params![
            logical_request_id,
            status,
            started_at,
            first_token_at,
            completed_at,
            duration_ms,
            output,
        ],
    )
    .unwrap();
}

// ==== Kimi 夹具（形态 = tps_native_secondary_test 同源的脱敏结构）====

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

/// 嵌套循环事件行：行级 `time` 在外层，事件在 `event` 下（真机形态）
fn kimi_loop_event(time: i64, event: Value) -> String {
    json!({"type": "context.append_loop_event", "time": time, "event": event}).to_string()
}

/// Kimi 正例：同文件**顶层** llm.request 提供模型归属（真机形态，dsh-kimi.md；独立复核 R5）
/// + 循环事件内 step.end 自带 usage 与两个原生时长。
fn write_kimi_session(home: &Path, t_end: i64) {
    let lines = [
        json!({"type": "llm.request", "time": t_end - 5_000, "provider": "moonshot",
               "model": "kimi-k2.6", "turnId": "0", "turnStep": "0.1"})
        .to_string(),
        kimi_loop_event(
            t_end,
            json!({"type": "step.end", "uuid": "u-acc", "turnId": "0", "step": 1,
                   "finishReason": "end_turn",
                   "usage": {"inputOther": 10, "output": 120,
                             "inputCacheRead": 0, "inputCacheCreation": 0},
                   "llmFirstTokenLatencyMs": 519, "llmStreamDurationMs": 806}),
        ),
    ];
    std::fs::write(kimi_wire_path(home, "sess-acc"), lines.join("\n") + "\n").unwrap();
}

// ==== OpenCode 夹具（Task 6 捕获记录的形态回放）====

/// 插件事实行的 15 键 camelCase schema（缺值 null）；时间整体平移 delta 保持捕获的
/// 相对节奏（start→首非空 delta +11ms → end +5ms；started 较 start 早 80ms）。
#[allow(clippy::too_many_arguments)]
fn oc_fact_row(
    event_key: &str,
    session_key: &str,
    message_key: &str,
    row_type: &str,
    created: i64,
    started: Option<i64>,
    tokens: Option<i64>,
    finish: Option<&str>,
    has_non_empty_delta: bool,
    delta_chars: i64,
) -> String {
    json!({
        "eventKey": event_key, "sessionKey": session_key, "messageKey": message_key,
        "ordinal": 0, "attempt": 0, "type": row_type, "created": created,
        "started": started, "tokens": tokens, "finish": finish,
        "hasNonEmptyDelta": has_non_empty_delta, "deltaChars": delta_chars,
        "model": "longcat-2.5-preview-free", "provider": "opencode",
        "tokensReasoning": if row_type == "end" { Some(0) } else { None },
    })
    .to_string()
}

/// Task 6 隔离活体捕获（opencode v2.0.22，免费内置模型）的记录形态回放：
/// diag:loaded → diag:stream-asynciterable → start → 非空 delta → end(tokens=3,
/// finish=stop)。`base_created` = end 行时刻；其余按捕获相对偏移回放。
fn write_opencode_facts(home: &Path, base_created: i64) {
    let start_created = base_created - 16;
    let delta_created = base_created - 5;
    let lines = [
        oc_fact_row(
            &"d".repeat(64),
            &"a".repeat(64),
            &"m".repeat(64),
            "diag:loaded",
            base_created - 2197,
            None,
            None,
            None,
            false,
            0,
        ),
        oc_fact_row(
            &"e".repeat(64),
            &"a".repeat(64),
            &"m".repeat(64),
            "diag:stream-asynciterable",
            base_created - 2196,
            None,
            None,
            None,
            false,
            0,
        ),
        oc_fact_row(
            &"f".repeat(64),
            &"a".repeat(64),
            &"m".repeat(64),
            "start",
            start_created,
            Some(start_created - 2080),
            None,
            None,
            false,
            0,
        ),
        oc_fact_row(
            &"0".repeat(64),
            &"a".repeat(64),
            &"m".repeat(64),
            "delta",
            delta_created,
            None,
            None,
            None,
            true,
            2,
        ),
        oc_fact_row(
            &"1".repeat(64),
            &"a".repeat(64),
            &"m".repeat(64),
            "end",
            base_created,
            None,
            Some(3),
            Some("stop"),
            false,
            0,
        ),
    ];
    let dir = home.join(".tuvis").join("tps").join("opencode");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("facts.jsonl"), lines.join("\n") + "\n").unwrap();
}

/// 验收主 home：四个正例源 + codex 已装（缺口态）齐备，事件全部落在 T0 的 120 秒窗内
fn acceptance_home() -> PathBuf {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().to_path_buf();
    std::mem::forget(temp); // 用例存活期间目录不得被清理（与 support 同款）
    write_dsh_session(&home, T0 - 60_000); // 结算 = T0−59_880
    {
        let c = zcode_create_db(&home);
        // 成功：started T0−60_000 → first T0−53_741 → completed T0−52_243（生成窗 1498ms）
        zc_insert(
            &c,
            "req-ok",
            "completed",
            T0 - 60_000,
            Some(T0 - 53_741),
            T0 - 52_243,
            Some(7_757),
            96,
        );
        // 取消：可靠起止俱在，但永不出速度（Spec §3「失败/取消不入速度」）
        zc_insert(
            &c,
            "req-cancel",
            "cancelled",
            T0 - 50_000,
            Some(T0 - 43_741),
            T0 - 42_243,
            Some(7_757),
            96,
        );
        // 缺 F 且缺时长：完整缺计时候选（缺失不填 0，Spec §3）
        zc_insert(
            &c,
            "req-nof",
            "completed",
            T0 - 40_000,
            None,
            T0 - 32_243,
            None,
            50,
        );
    }
    write_kimi_session(&home, T0 - 30_000);
    write_opencode_facts(&home, T0 - 10_000);
    std::fs::create_dir_all(home.join(".codex")).unwrap(); // 已装 → 缺口态（非未配置）
    home
}

// ===========================================================================
// 验收 1：原生来源回放 → 事实 → 调度拍 → 点 → 查询快照/曲线（整条链）
// ===========================================================================

#[test]
fn native_replay_full_chain_to_points_snapshot_and_curve() {
    let home = acceptance_home();
    let mut c = conn("acc-full-chain");
    seed_old_usage(&c);
    let usage_detail_before = count(&c, "usage_detail");
    let usage_daily_before = count(&c, "usage_daily");
    let usage_cursor_before = count(&c, "usage_cursor");
    let usage_session_before = count(&c, "usage_session");
    set_usage_settings(&c, true);

    // —— 启动拍：调度器发 Read + Startup 点，链上执行 ——
    let mut sampler = Sampler::new("run-acc", true);
    let startup_points = execute_tick(&mut c, &mut sampler, &home, T0, false);
    assert_eq!(
        sampler.run_id(),
        "run-acc",
        "整链沿用调度器的 runId（断线语义由它承载）"
    );
    // 四个正例 series 各得一个 value 点（workbuddy 无事实、codex 缺口不出 series）
    assert_eq!(startup_points, 4, "启动拍恰为 4 个正例 series 各建 1 点");

    // 采集轮报告：6 个扫描源全部 ok；dsh/zcode/kimi/opencode 各提交 1 条事实
    let points = store::load_points_since(&c, 0).unwrap();
    assert_eq!(points.len(), 4);
    for p in &points {
        assert_eq!(p.status, PointStatus::Value, "启动点必须出实测值");
        assert_eq!(p.trigger, PointTrigger::Startup);
        assert_eq!(p.window_ms, 120_000);
    }

    // —— 逐 series 的 Σ/Σ 口径复核（1000×Σ输出/Σ生成毫秒）——
    let by_tool = |tool: &str| -> multi_agents_manager_lib::services::tps::model::TpsPoint {
        points
            .iter()
            .find(|p| p.series_key.tool == tool)
            .unwrap_or_else(|| panic!("缺 {tool} 的点"))
            .clone()
    };
    let dsh = by_tool("dsh");
    assert_eq!(
        dsh.sum_output_tokens, 96,
        "DSH 同消息 usage（首空 delta 跳过）"
    );
    assert_eq!(dsh.sum_generation_ms, 20, "生成窗 = 结算 − 首非空成员");
    assert!((dsh.tps.unwrap() - 4800.0).abs() < 1e-9);
    assert_eq!(
        dsh.output_scope,
        OutputScope::IncludesReasoning,
        "思考已计入"
    );
    assert_eq!(dsh.method, TimingMethod::StreamObserved);

    let zc = by_tool("zcode");
    // 取消/缺 F 不进速度：样本面只有 1 条成功；候选/分类计数如实保留
    assert_eq!(zc.sample_count, 1, "取消与缺 F 不得入速度样本");
    assert_eq!(zc.sum_output_tokens, 96);
    assert_eq!(zc.sum_generation_ms, 1_498);
    assert!((zc.tps.unwrap() - 1000.0 * 96.0 / 1498.0).abs() < 1e-9);
    assert_eq!(zc.candidate_count, 3, "候选面含取消与缺 F");
    assert_eq!(zc.cancelled_count, 1);
    assert_eq!(zc.missing_timing_count, 1, "缺 F 缺时长 = 缺计时（不填 0）");
    assert_eq!(zc.timed_model_count, 2, "成功 + 取消各有可靠整调用时长");
    assert_eq!(zc.ttft_sample_count, 1, "TTFT 只平均成功且首点可对齐的样本");

    let km = by_tool("kimi");
    assert_eq!(km.sum_output_tokens, 120);
    assert_eq!(km.sum_generation_ms, 806, "无全局 1s 下限：806ms 正窗有效");
    assert_eq!(
        km.output_scope,
        OutputScope::ProviderReported,
        "思考计数未单独声明"
    );
    assert_eq!(km.method, TimingMethod::ClientNative);

    let oc = by_tool("opencode");
    assert_eq!(oc.sum_output_tokens, 3, "Task 6 捕获正例：end 行 tokens=3");
    assert_eq!(oc.sum_generation_ms, 5, "end − 首非空 delta = 5ms");
    assert!((oc.tps.unwrap() - 600.0).abs() < 1e-9, "1000×3/5 = 600");
    assert_eq!(oc.method, TimingMethod::StreamObserved);

    // —— 后台第二拍（+600 秒）：无新事实 → 同 series 出 gap（不画 0、不沿用旧速度）——
    let gap_points = execute_tick(&mut c, &mut sampler, &home, T1, false);
    assert_eq!(gap_points, 4, "空窗仍为已知 series 产 gap");
    let pts_now = store::load_points_since(&c, 0).unwrap();
    assert_eq!(pts_now.len(), 8);
    for p in pts_now.iter().filter(|p| p.sampled_at == T1) {
        assert_eq!(p.status, PointStatus::Gap);
        assert!(p.tps.is_none(), "gap 点不得有速度值");
        assert_eq!(p.sample_count, 0);
        assert_eq!(p.trigger, PointTrigger::Scheduled);
    }

    // —— 查询快照（IPC tps_snapshot 的投影）：4 series + 全源真实状态 ——
    let snap = query::snapshot(
        &c,
        &multi_agents_manager_lib::services::tps::model::TpsSelection::default(),
        T0 + 1_000,
    )
    .unwrap()
    .snapshot
    .expect("启用态必有快照");
    assert_eq!(snap.series.len(), 4);
    let dsh_snap = snap
        .series
        .iter()
        .find(|s| s.series_key.tool == "dsh")
        .unwrap();
    let last = dsh_snap
        .last_call
        .as_ref()
        .expect("1 秒年龄内 lastCall 必在");
    assert!((last.tps.unwrap() - 4800.0).abs() < 1e-9);
    assert_eq!(last.sample_count, 1);
    assert!(
        dsh_snap.current_turn.is_some(),
        "dsh 有原生 turn → 有当前回合面"
    );
    // opencode 无原生回合键 → unknown-turn 降级（不按 session 冒充回合）
    let oc_snap = snap
        .series
        .iter()
        .find(|s| s.series_key.tool == "opencode")
        .unwrap();
    assert!(oc_snap.current_turn.is_none());
    assert_eq!(oc_snap.null_reason.as_deref(), Some("unknown-turn"));

    // —— sourceStates：7 个注册来源全覆盖，状态如实分类 ——
    let states: std::collections::BTreeMap<
        &str,
        &multi_agents_manager_lib::services::tps::model::SourceState,
    > = snap
        .source_states
        .iter()
        .map(|s| (s.source.as_str(), s))
        .collect();
    assert_eq!(states.len(), collector::REGISTERED_SOURCES.len());
    let status_of = |src: &str| -> String {
        states[src]
            .detail
            .clone()
            .unwrap_or_default()
            .split("; ")
            .next()
            .unwrap_or_default()
            .trim_start_matches("tps-status=")
            .to_string()
    };
    for src in ["dsh", "zcode", "kimi", "opencode"] {
        assert_eq!(status_of(src), "active", "{src} 有有效事实 = 有效");
    }
    // Codex 已接入 · 仅响应（V2 计划 Task 5；Spec §8.6「不能整体标为不支持」）：
    // 影子 home 有 .codex 无 rollout = 已接入、可读、零事实 → 与 workbuddy 同类
    // no-sample（不再是整体缺口态）
    assert_eq!(
        status_of("codex"),
        "no-sample",
        "已接入无 rollout = 无样本，不再整体标不支持"
    );
    assert_eq!(
        states["codex"].detail.as_deref(),
        Some("tps-status=no-sample"),
        "干净空批无来源限制原文，detail 仅机器可读状态前缀"
    );
    assert_eq!(status_of("workbuddy"), "no-sample", "已读无事实 = 无样本");
    assert_eq!(status_of("claude"), "unconfigured", "未装统计键 = 未配置");

    // —— 查询曲线（IPC tps_points 的投影）：1h 含 8 点（4 value + 4 gap），2m 只剩 gap ——
    let curve_1h = query::points(
        &c,
        &multi_agents_manager_lib::services::tps::model::TpsSelection::default(),
        TpsViewRange::Last1h,
        T1,
    )
    .unwrap();
    assert!(!curve_1h.disabled);
    assert_eq!(curve_1h.known_series.len(), 4);
    assert_eq!(curve_1h.points.len(), 8);
    assert_eq!(
        curve_1h
            .points
            .iter()
            .filter(|p| p.status == PointStatus::Value)
            .count(),
        4
    );
    let curve_2m = query::points(
        &c,
        &multi_agents_manager_lib::services::tps::model::TpsSelection::default(),
        TpsViewRange::Last2m,
        T1,
    )
    .unwrap();
    // 2m 窗（T1−120s 起）不含 T0 的 value 点 → 全 gap（旧点不冒充当前）
    assert!(curve_2m.points.iter().all(|p| p.status == PointStatus::Gap));

    // —— 旧 usage 四桶行数前后相等（TPS 链不碰旧账本）——
    assert_eq!(count(&c, "usage_detail"), usage_detail_before);
    assert_eq!(count(&c, "usage_daily"), usage_daily_before);
    assert_eq!(count(&c, "usage_cursor"), usage_cursor_before);
    assert_eq!(count(&c, "usage_session"), usage_session_before);

    // —— 同游标重读不翻倍：第三拍 Read 后事实数不增 ——
    collector::run_round(&mut c, &home, T1 + 1_000);
    let facts = store::load_recent(&c, 0).unwrap();
    let by = |src: &str| facts.iter().filter(|o| o.source == src).count();
    assert_eq!(
        (by("dsh"), by("zcode"), by("kimi"), by("opencode")),
        (1, 3, 1, 1),
        "幂等重读不翻倍（zcode 3 = 成功 + 取消 + 缺 F 的候选事实面）"
    );
}

// ===========================================================================
// 验收 2：OpenCode Task 6 捕获记录逐字回放 → 恰一条有效观察（该适配的验收口径）
// ===========================================================================

#[test]
fn opencode_task6_captured_record_replays_to_valid_observation() {
    // 捕获时刻逐字回放（task-6-report.md §5：created=1792857532020/…031/…036，
    // started=1792857529940），「现在」取终行后 1 秒（假时钟，不读系统钟）
    let end_created: i64 = 1_792_857_532_036;
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    // 逐字形态：diag 行 created 与正例行按捕获偏移；本函数按 base=end 回放同形
    write_opencode_facts(home, end_created);

    let batch =
        opencode::read_incremental(home, &SourceCursor::empty("opencode"), end_created + 1_000)
            .unwrap();
    assert!(batch.source_state.ok);
    assert_eq!(
        batch.observations.len(),
        1,
        "一条真实 start→delta→end = 恰一条观察（loaded 不充数）"
    );
    let o = &batch.observations[0];
    assert_eq!(o.source, "opencode");
    assert_eq!(o.model, "longcat-2.5-preview-free");
    assert_eq!(
        o.channel.as_deref(),
        Some("opencode"),
        "providerID 入 channel 位"
    );
    assert_eq!(
        o.started_at_ms, 1_792_857_529_940,
        "start 行宿主 started（较事件时刻早 2080ms，捕获偏移）"
    );
    assert_eq!(
        o.first_token_at_ms,
        Some(1_792_857_532_031),
        "首个非空 delta"
    );
    assert_eq!(o.ended_at_ms, end_created);
    assert_eq!(o.generation_ms, Some(5));
    assert_eq!(o.output_tokens, Some(3));
    assert_eq!(o.outcome, Outcome::Success);
    assert_eq!(o.method, TimingMethod::StreamObserved);
    assert!(o.is_tps_valid(), "捕获正例必须入速");
    assert!(
        o.stable_call_key.starts_with("oc:"),
        "稳定键形态 oc:<session>:<message>:<step>，实得 {}",
        o.stable_call_key
    );
    assert!(
        o.turn_key.is_none(),
        "step 词表无回合键 → unknown-turn 降级"
    );

    // 适配状态 = supported（有真实宿主正例；loaded ≠ 支持的判据由本用例承载）
    assert_eq!(opencode::ADAPTER_STATUS, "supported");

    // 回放观察可入库、可出 value 点（与点/查询面的衔接）
    let mut c = conn("acc-oc-replay");
    set_usage_settings(&c, true);
    store::commit_batch(
        &mut c,
        &ReadBatch {
            observations: batch.observations.clone(),
            source_state: batch.source_state.clone(),
            diagnostics: batch.diagnostics.clone(),
            next_cursor: batch.next_cursor.clone(),
        },
    )
    .unwrap();
    let written =
        collector::produce_point(&mut c, "run-oc", end_created + 2_000, PointTrigger::Manual)
            .unwrap();
    assert_eq!(written, 1);
    let pts = store::load_points_since(&c, 0).unwrap();
    assert_eq!(pts[0].status, PointStatus::Value);
    assert!((pts[0].tps.unwrap() - 600.0).abs() < 1e-9);
    assert_eq!(
        pts[0].trigger,
        PointTrigger::Manual,
        "manual 触发面同链可用"
    );
}

// ===========================================================================
// 验收 3：单源失败不阻塞其它源；旧 usage 账本快照不变；失败后曲线仍可出点
// ===========================================================================

#[test]
fn failing_source_does_not_block_others_and_old_usage_ledger_untouched() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().to_path_buf();
    // dsh 数据根 = 普通文件 → read_dir 失败（非 NotFound）→ 整源 Err
    std::fs::create_dir_all(home.join(".dsh")).unwrap();
    std::fs::write(home.join(".dsh").join("sessions"), "not a dir").unwrap();
    // zcode 正例照常
    {
        let c = zcode_create_db(&home);
        zc_insert(
            &c,
            "req-ok",
            "completed",
            T0 - 60_000,
            Some(T0 - 53_741),
            T0 - 52_243,
            Some(7_757),
            96,
        );
    }
    // kimi 正例照常（两个成功源互不牵连的更强证明）
    write_kimi_session(&home, T0 - 30_000);

    let mut c = conn("acc-failure-isolation");
    seed_old_usage(&c);
    let before = (
        count(&c, "usage_detail"),
        count(&c, "usage_daily"),
        count(&c, "usage_cursor"),
        count(&c, "usage_session"),
    );
    set_usage_settings(&c, true);

    let mut sampler = Sampler::new("run-acc-fail", true);
    let written = execute_tick(&mut c, &mut sampler, &home, T0, false);
    assert_eq!(written, 2, "zcode/kimi 不受 dsh 失败牵连，各自出点");

    let pts = store::load_points_since(&c, 0).unwrap();
    let tools: std::collections::BTreeSet<&str> =
        pts.iter().map(|p| p.series_key.tool.as_str()).collect();
    assert!(!tools.contains("dsh"), "失败源不得出点");
    assert!(tools.contains("zcode") && tools.contains("kimi"));

    // 失败源水位不动（Spec §6「读取失败不改变水位」）；V2 Task 6 起失败健康态照报
    assert!(store::load_cursor(&c, "dsh").unwrap().is_none());
    let dsh_state = store::load_source_state(&c, "dsh").unwrap().unwrap();
    assert!(!dsh_state.ok, "读取 Err 落 ok=false 失败 SourceState");
    let curve = query::points(
        &c,
        &multi_agents_manager_lib::services::tps::model::TpsSelection::default(),
        TpsViewRange::Last1h,
        T0 + 1_000,
    )
    .unwrap();
    assert_eq!(curve.points.len(), 2, "查询面只见成功源的 series");

    let after = (
        count(&c, "usage_detail"),
        count(&c, "usage_daily"),
        count(&c, "usage_cursor"),
        count(&c, "usage_session"),
    );
    assert_eq!(before, after, "旧 usage 四桶行数逐表不变");
}

// ===========================================================================
// 验收 4：24 小时保留清理（假时钟；不真等 1 小时——节奏锚点直驱）
// ===========================================================================

#[test]
fn retention_purge_drops_older_than_24h() {
    let mut c = conn("acc-purge");
    let obs = |key: &str, ended: i64| -> TpsObservation {
        TpsObservation {
            source: "kimi".into(),
            host_instance: "host-1".into(),
            stable_call_key: key.into(),
            started_at_ms: ended - 1_000,
            first_token_at_ms: Some(ended - 500),
            ended_at_ms: ended,
            native_duration_ms: Some(1_000),
            generation_ms: Some(500),
            response_elapsed_ms: None,
            output_tokens: Some(100),
            output_scope: OutputScope::ExclusiveOutput,
            tool_ms: None,
            model: "kimi-k2".into(),
            channel: None,
            session_key: Some("s".into()),
            turn_key: Some("t".into()),
            purpose: multi_agents_manager_lib::services::tps::model::Purpose::MainTask,
            outcome: Outcome::Success,
            method: TimingMethod::ClientNative,
            failure_reason: None,
        }
    };
    store::commit_batch(
        &mut c,
        &ReadBatch {
            observations: vec![obs("fresh", T0), obs("stale", T0 - 25 * 3_600_000)],
            source_state: SourceState {
                source: "kimi".into(),
                cursor: SourceCursor::empty("kimi"),
                ok: true,
                detail: None,
            },
            diagnostics: Vec::new(),
            next_cursor: SourceCursor::empty("kimi"),
        },
    )
    .unwrap();
    collector::produce_point(&mut c, "run-acc-purge", T0, PointTrigger::Startup).unwrap();

    // 清理锚点从零起：首拍即清理（节奏到达）；超窗事实被删，窗内保留
    let deleted = collector::maybe_purge(&c, T0 + 60_000).unwrap();
    assert!(deleted > 0, "首轮清理必须删掉超 24h 的行");
    let facts = store::load_recent(&c, 0).unwrap();
    assert_eq!(facts.len(), 1, "24h 窗内事实保留");
    assert_eq!(facts[0].stable_call_key, "fresh");

    // 节奏未到（1 小时内）→ 空操作；到达后无超窗行 → 0（锚点照常前移）
    assert_eq!(collector::maybe_purge(&c, T0 + 2 * 3_600_000).unwrap(), 0);
    assert_eq!(
        collector::maybe_purge(&c, T0 + 2 * 3_600_000 + 3_601_000).unwrap(),
        0,
        "已无超窗行 → 0"
    );
}

// ===========================================================================
// 验收 5：总授权关闭 → 查询报 disabled、历史完整保留；重开恢复
// ===========================================================================

#[test]
fn master_switch_disabled_reports_disabled_and_history_intact() {
    let home = acceptance_home();
    let mut c = conn("acc-disabled");
    set_usage_settings(&c, true);
    // 先在开启态跑出历史（事实 + 点）
    let mut sampler = Sampler::new("run-acc-dis", true);
    let written = execute_tick(&mut c, &mut sampler, &home, T0, false);
    assert_eq!(written, 4);
    assert_eq!(
        count(&c, "tps_observation"),
        6,
        "4 有效 + 取消 + 缺 F 的候选事实面"
    );
    assert_eq!(count(&c, "tps_point"), 4);

    // 关闭：快照/曲线如实报 disabled，不返回载荷；历史行仍在库（不删）
    set_usage_settings(&c, false);
    let now = T0 + 1_000;
    let snap = query::snapshot(
        &c,
        &multi_agents_manager_lib::services::tps::model::TpsSelection::default(),
        now,
    )
    .unwrap();
    assert!(snap.disabled);
    assert!(snap.snapshot.is_none());
    let curve = query::points(
        &c,
        &multi_agents_manager_lib::services::tps::model::TpsSelection::default(),
        TpsViewRange::Last1h,
        now,
    )
    .unwrap();
    assert!(curve.disabled);
    assert!(curve.points.is_empty(), "关闭时不返回曲线（历史仍在库）");
    assert_eq!(count(&c, "tps_observation"), 6, "历史不删");
    assert_eq!(count(&c, "tps_point"), 4, "历史不删");

    // 关闭期调度器不再发任何动作（停采样，不删历史）
    sampler.set_enabled(false, "unused");
    let out = sampler.tick(T0 + 2_000, true);
    assert!(out.actions.is_empty(), "关闭时不得调度任何读取/点");

    // 重开：新 runId，先采一次（跨关闭期不连线由 runId 承载）
    set_usage_settings(&c, true);
    sampler.set_enabled(true, "run-acc-dis-2");
    assert_eq!(sampler.run_id(), "run-acc-dis-2");
    let snap = query::snapshot(
        &c,
        &multi_agents_manager_lib::services::tps::model::TpsSelection::default(),
        T0 + 3_000,
    )
    .unwrap();
    assert!(!snap.disabled);
    assert!(snap.snapshot.is_some());
}
