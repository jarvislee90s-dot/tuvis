//! TPS 调度 / 采集 / 查询 / 视图租约与 IPC 集成测试（2026-10-24 计划 Task 7）。
//!
//! 隔离纪律（2026-10-07 事故族）：
//! * 假时钟测试：调度器是纯状态机，时间由入参注入——不真等 1 小时（Spec §7）；
//! * 全部库操作走 `support::open_ledger_db(tag)` 的**用例私有库**（tempdir 下真实 DB
//!   文件），绝不触碰真机 `~/.tuvis` / `~/.dsh` / `~/.claude`；
//! * 采集夹具是 tempdir 下的**合成**结构（DSH 坏根 / ZCode 合成库）；
//! * 接收器端口探测只用**临时端口**（bind :0 / 临时监听占位）——**绝不 bind
//!   47181–47190**（brief 安全约束逐字）。
//!
//! 覆盖（brief Step1/Step4 逐条）：
//! * 后台 1 小时 → 恰好 6 个 Scheduled 点（+1 Startup；锚点断言逐字）；
//! * 可见期 5 秒增量读取 / 60 秒点；
//! * 租约 90 秒过期 → `SamplerMode::Background`（锚点断言逐字）；已退出视图不能
//!   永远维持前台；
//! * manual 最小间隔 5 秒；
//! * 120 秒点内最多 20 条有效调用；
//! * Token 区间切换对 TPS 状态零影响；
//! * 总开关关闭 → 停调度、查询报 disabled 不删历史；重开 → 新 runId 断线；
//! * 故障注入：单源失败不丢其他 series、旧 usage_* 行数前后相等、失败源游标不动；
//! * SQLite 查询只尾查（EXPLAIN QUERY PLAN 锁索引）；
//! * 空窗仍产 gap；known_series 来自近 24h 存储；
//! * 快照 sourceStates 是真实来源状态（绝不引用 policy 层占位形态），状态分类
//!   未配置/待重启/无样本/字段不足/有效可机器解析；
//! * 接收器守卫（Bearer/超限/坏体/端口冲突报状态）与凭证不回传 webview；
//! * IPC 错误与 API 错误一致（同一 reason 分类法）。

mod support;

use multi_agents_manager_lib::commands::tps::{install_error_reason, TpsInstallReceiptDto};
use multi_agents_manager_lib::services::tps::collector;
use multi_agents_manager_lib::services::tps::install::{
    self, InstallChoice, ReceiverConfig, DEFAULT_RECEIVER_PORT, MAX_BODY_BYTES,
};
use multi_agents_manager_lib::services::tps::model::{
    Outcome, OutputScope, PointStatus, PointTrigger, Purpose, ReadBatch, SourceCursor, SourceState,
    TimingMethod, TpsObservation, TpsSelection,
};
use multi_agents_manager_lib::services::tps::query::{self, TpsViewRange};
use multi_agents_manager_lib::services::tps::receiver::{self, IngestOutcome};
use multi_agents_manager_lib::services::tps::scheduler::{Sampler, SamplerMode, TickAction};
use multi_agents_manager_lib::services::tps::store;
use rusqlite::Connection;
use serde_json::json;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// 固定假时钟基准（纯整数，不读系统时钟）
const BASE_MS: i64 = 1_700_000_000_000;

// ==== 通用夹具 ====

/// 用例私有库（tempdir 下的真实 DB 文件，schema::init 建齐全部表）
fn conn(tag: &str) -> Connection {
    support::open_ledger_db(tag)
}

/// 绝对行数断言（表名只在测试内拼接；表缺失时响亮 panic，不静默当 0）
fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap_or_else(|e| panic!("计数查询失败（{table}，表缺失？）：{e}"))
}

/// 有效观察夹具（与 policy/store 测试同形：Success / MainTask / client-native / kimi）
fn obs(key: &str, ended_at: i64, gen_ms: i64, output: i64) -> TpsObservation {
    TpsObservation {
        source: "kimi".into(),
        host_instance: "host-1".into(),
        stable_call_key: key.into(),
        started_at_ms: ended_at - gen_ms,
        first_token_at_ms: Some(ended_at - gen_ms + gen_ms / 4),
        ended_at_ms: ended_at,
        native_duration_ms: Some(gen_ms),
        generation_ms: Some(gen_ms),
        response_elapsed_ms: None,
        output_tokens: Some(output),
        output_scope: OutputScope::ExclusiveOutput,
        tool_ms: None,
        model: "kimi-k2".into(),
        channel: None,
        session_key: Some("s1".into()),
        turn_key: Some("t1".into()),
        purpose: Purpose::MainTask,
        outcome: Outcome::Success,
        method: TimingMethod::ClientNative,
        failure_reason: None,
    }
}

/// 直写 settings 表的 usage.settings（dao 的 *_conn 形态是 pub(crate)，集成测试直拼 SQL）
fn set_usage_settings(conn: &Connection, enabled: bool) {
    conn.execute(
        "INSERT OR REPLACE INTO settings (key, value) VALUES ('usage.settings', ?1)",
        rusqlite::params![format!(
            r#"{{"enabled":{enabled},"miniBarRange":"today","miniBarToolRows":3,"detailRetentionDays":30,"collectIntervalMin":10,"providerMapRules":"","exportQuote":"","exportPose":"random"}}"#
        )],
    )
    .unwrap();
}

/// 旧 usage 四桶播种（与 tps_store_test::seed_old_usage 同形：故障注入用例的行数基准）
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

/// ZCode 合成库（列集与 tps_native_primary_test 同源；一行成功样本，完成时刻在 NOW 窗内）
fn zcode_fixture_db(home: &Path, done_at: i64) {
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
    conn.execute(
        "INSERT INTO model_usage (logical_request_id, session_id, turn_id, assistant_message_id,
             provider_id, model_id, status, started_at, first_token_at, completed_at,
             duration_ms, time_to_first_token_ms, input_tokens, output_tokens, reasoning_tokens,
             cache_read_input_tokens, cache_creation_input_tokens, computed_total_tokens,
             retry_count, task_type)
         VALUES ('lr-1', 'sess_z', 'turn_1', 'amsg-1', 'deepseek', 'deepseek-v4.1-flash',
             'completed', ?1, ?2, ?3, ?4, ?5, 21112, 96, 0, 0, 0, 21208, 0, 'interactive')",
        rusqlite::params![
            done_at - 7757,
            done_at - 1498,
            done_at,
            7757,
            done_at - 7757
        ],
    )
    .unwrap();
}

/// 最小 OTLP JSON 报文（一条 claude_code.llm_request span；成功 / attempt=1 /
/// output / duration / first_content 齐全 → 可入速）
fn otlp_body(trace: &str, span: &str, start_ms: i64, dur_ms: i64, output: i64) -> Vec<u8> {
    let attr_s = |k: &str, v: &str| json!({"key": k, "value": {"stringValue": v}});
    let attr_d = |k: &str, v: f64| json!({"key": k, "value": {"doubleValue": v}});
    json!({
        "resourceSpans": [{
            "scopeSpans": [{
                "spans": [{
                    "traceId": trace,
                    "spanId": span,
                    "name": "claude_code.llm_request",
                    "startTimeUnixNano": (start_ms * 1_000_000).to_string(),
                    "endTimeUnixNano": ((start_ms + dur_ms) * 1_000_000).to_string(),
                    "attributes": [
                        attr_s("session.id", "sess-1"),
                        attr_s("model", "claude-sonnet"),
                        attr_d("output_tokens", output as f64),
                        attr_d("duration_ms", dur_ms as f64),
                        attr_d("first_content_ms", 200.0),
                        json!({"key": "success", "value": {"boolValue": true}}),
                        json!({"key": "attempt", "value": {"intValue": "1"}}),
                    ],
                }],
            }],
        }]
    })
    .to_string()
    .into_bytes()
}

// ===========================================================================
// brief Step 1：假时钟红测试（调度器纯状态机）
// ===========================================================================

/// 后台 1 小时 → 恰好 6 个 Scheduled 点（+1 Startup 点）；后台绝不跑 5 秒 timer
#[test]
fn background_one_hour_produces_exactly_six_scheduled_points() {
    let mut s = Sampler::new("run-1", true);
    let t0 = BASE_MS;
    let first = s.tick(t0, false);
    assert_eq!(first.mode, SamplerMode::Background);
    assert!(
        matches!(
            first.actions.as_slice(),
            [
                TickAction::Read,
                TickAction::EmitPoint {
                    trigger: PointTrigger::Startup,
                    ..
                }
            ]
        ),
        "启动后先采一次：一条 Read + 一个 Startup 点，实际 {:?}",
        first.actions
    );

    // 逐分钟推进假时钟，数 1 小时内的 Scheduled 点
    let mut scheduled_points_in_hour = 0usize;
    let mut reads_in_hour = 0usize;
    for k in 1..=60 {
        let out = s.tick(t0 + k * 60_000, false);
        for a in &out.actions {
            match a {
                TickAction::EmitPoint {
                    trigger: PointTrigger::Scheduled,
                    ..
                } => {
                    scheduled_points_in_hour += 1;
                }
                TickAction::Read => reads_in_hour += 1,
                TickAction::EmitPoint { trigger, .. } => {
                    panic!("后台期不得出现 {trigger:?} 点")
                }
            }
        }
        assert_eq!(out.mode, SamplerMode::Background);
    }
    assert_eq!(
        scheduled_points_in_hour, 6,
        "brief 逐字锚点：后台1小时恰好6个计划点"
    );
    assert_eq!(reads_in_hour, 6, "后台每 600 秒读取一次（每点一次读取）");
    // 相邻计划点间距必须 ≥ 600s（后台不存在 5 秒 timer 的证据）
    // （间距由计数 6 隐含：60 次每分钟 tick 只发 6 点 = 间隔 600s）
}

/// 至少一个可见视图：每 5 秒有界增量读取、每 60 秒一个 Scheduled 点
#[test]
fn foreground_view_yields_5s_reads_and_60s_points() {
    let mut s = Sampler::new("run-1", true);
    let t0 = BASE_MS;
    s.tick(t0, false); // Startup 点，锚点 = t0

    s.heartbeat("view-a", true, t0);
    let mut reads = 0usize;
    let mut points = 0usize;
    // 每 5 秒 tick 一次（可见期 1 小时）
    for k in 1..=720 {
        let out = s.tick(t0 + k * 5_000, true);
        assert_eq!(out.mode, SamplerMode::Foreground, "可见期恒为前台");
        for a in &out.actions {
            match a {
                TickAction::Read => reads += 1,
                TickAction::EmitPoint {
                    trigger: PointTrigger::Scheduled,
                    ..
                } => points += 1,
                TickAction::EmitPoint { trigger, .. } => panic!("可见期不得出现 {trigger:?} 点"),
            }
        }
    }
    assert_eq!(reads, 720, "可见期每 5 秒一次有界增量读取");
    assert_eq!(points, 60, "可见期每 60 秒一个曲线点（1 小时 60 点）");
}

/// 租约 90 秒过期 → 恢复后台（brief 逐字锚点 after_lease_expiry.mode）；
/// 已退出视图（不再心跳）不能永远维持前台
#[test]
fn lease_expiry_returns_to_background_and_dead_view_cannot_hold_foreground() {
    let mut s = Sampler::new("run-1", true);
    let t0 = BASE_MS;
    s.tick(t0, false); // Startup
    s.heartbeat("view-a", true, t0); // 租约至 t0+90s

    assert_eq!(s.tick(t0 + 5_000, false).mode, SamplerMode::Foreground);
    assert_eq!(
        s.tick(t0 + 89_999, false).mode,
        SamplerMode::Foreground,
        "90 秒内租约活着"
    );
    let after_lease_expiry = s.tick(t0 + 90_000, false);
    assert_eq!(
        after_lease_expiry.mode,
        SamplerMode::Background,
        "brief 逐字锚点：租约过期恢复后台"
    );
    assert!(
        after_lease_expiry.actions.is_empty(),
        "刚回到后台时 600s 锚点未到 → 本 tick 无动作"
    );
    // 此后继续推进（不再心跳）：永远不再产生可见期节奏的读取/点
    for k in 1..=12 {
        let out = s.tick(t0 + 90_000 + k * 5_000, false);
        assert_eq!(out.mode, SamplerMode::Background);
        assert!(
            !out.actions.iter().any(|a| matches!(a, TickAction::Read)),
            "已退出视图不得借 5 秒 timer 续读取"
        );
    }
}

/// 心跳续约：30 秒一次的心跳把租约顶到「最近一次心跳 + 90 秒」
#[test]
fn heartbeat_renews_lease_and_visible_false_drops_it() {
    let mut s = Sampler::new("run-1", true);
    let t0 = BASE_MS;
    s.tick(t0, false);
    s.heartbeat("v", true, t0);
    // t0+80s 再心跳一次 → 租约顶到 t0+170s
    s.heartbeat("v", true, t0 + 80_000);
    assert_eq!(
        s.tick(t0 + 100_000, false).mode,
        SamplerMode::Foreground,
        "续约后 90s 界已顺延"
    );
    assert_eq!(
        s.tick(t0 + 170_000, false).mode,
        SamplerMode::Background,
        "顺延后的界同样过期"
    );
    // visible=false 的心跳立即交还租约（前端决定可见性；隐藏即交）
    s.heartbeat("v", true, t0 + 200_000);
    assert_eq!(s.tick(t0 + 201_000, false).mode, SamplerMode::Foreground);
    s.heartbeat("v", false, t0 + 202_000);
    assert_eq!(
        s.tick(t0 + 202_500, false).mode,
        SamplerMode::Background,
        "显式隐藏立即回后台"
    );
}

/// manual 刷新最小间隔 5 秒；manual 点 trigger=Manual
#[test]
fn manual_refresh_is_rate_limited_to_5_seconds() {
    let mut s = Sampler::new("run-1", true);
    let t0 = BASE_MS;
    s.tick(t0, false);

    let first = s.manual_refresh(t0).expect("首次 manual 必放行");
    assert!(
        matches!(
            first.as_slice(),
            [
                TickAction::Read,
                TickAction::EmitPoint {
                    trigger: PointTrigger::Manual,
                    ..
                }
            ]
        ),
        "manual = 读取 + 一个 Manual 点，实际 {:?}",
        first
    );
    assert!(s.manual_refresh(t0 + 4_999).is_none(), "5 秒内限频拒绝");
    let second = s.manual_refresh(t0 + 5_000).expect("恰好 5 秒放行");
    assert!(matches!(
        second.as_slice(),
        [
            TickAction::Read,
            TickAction::EmitPoint {
                trigger: PointTrigger::Manual,
                ..
            }
        ]
    ));
}

/// 总开关关闭 → 停调度（tick 无动作、mode=Disabled）；重开 → 新 runId + Startup 点
#[test]
fn master_switch_disables_scheduling_and_reenable_starts_new_run_id() {
    let mut s = Sampler::new("run-1", true);
    let t0 = BASE_MS;
    s.tick(t0, false);
    assert_eq!(s.run_id(), "run-1");

    s.set_enabled(false, "unused");
    let out = s.tick(t0 + 1_000, true);
    assert_eq!(out.mode, SamplerMode::Disabled);
    assert!(out.actions.is_empty(), "关闭时不得调度任何读取/点");
    assert!(
        s.manual_refresh(t0 + 2_000).is_none(),
        "关闭时 manual 也不得放行"
    );

    s.set_enabled(true, "run-2");
    let out = s.tick(t0 + 3_000, false);
    assert_eq!(s.run_id(), "run-2", "重开必须换新 runId（跨关闭期不连线）");
    assert!(
        matches!(
            out.actions.as_slice(),
            [
                TickAction::Read,
                TickAction::EmitPoint {
                    trigger: PointTrigger::Startup,
                    ..
                }
            ]
        ),
        "重开先采一次（Startup），实际 {:?}",
        out.actions
    );
    // 重复 set_enabled(true) 不得再换 runId（运行循环每秒调它）
    s.set_enabled(true, "run-3");
    assert_eq!(s.run_id(), "run-2", "已启用时的重复 enable 是无操作");
}

// ===========================================================================
// 采集编排：单飞 / 隔离失败 / 提交后推进 / 预算
// ===========================================================================

/// ≤20 条/series/点：30 条有效事实只取最近 20（brief Step1「120秒点内最多20调用」的编排面）
#[test]
fn produce_point_caps_samples_at_twenty_per_series() {
    let mut c = conn("sampling-cap20");
    let at = BASE_MS;
    let rows: Vec<TpsObservation> = (0..30)
        .map(|i| obs(&format!("c{i:02}"), at - i * 1000, 1000, 1))
        .collect();
    store::commit_batch(
        &mut c,
        &ReadBatch {
            observations: rows,
            source_state: SourceState {
                source: "kimi".into(),
                cursor: SourceCursor::empty("kimi"),
                ok: true,
                detail: None,
            },
            diagnostics: Vec::new(),
            next_cursor: SourceCursor {
                source: "kimi".into(),
                value: "off-1".into(),
            },
        },
    )
    .unwrap();
    let written = collector::produce_point(&mut c, "run-1", at, PointTrigger::Scheduled).unwrap();
    assert_eq!(written, 1);
    let pts = store::load_points_since(&c, 0).unwrap();
    assert_eq!(pts.len(), 1);
    assert_eq!(pts[0].sample_count, 20, "每点每 series 最多 20 条有效");
    assert_eq!(pts[0].sampled_keys.len(), 20);
    assert_eq!(pts[0].sum_output_tokens, 20, "最旧 10 条不得入样");
    assert_eq!(pts[0].run_id, "run-1");
}

/// 空窗仍为 known_series 产 gap 点（不画 0、不沿用旧速度）；known_series 来自近 24h 存储
#[test]
fn empty_window_still_yields_gap_for_known_series_from_storage() {
    let mut c = conn("sampling-gap");
    let at = BASE_MS;
    // 24h 前的旧事实：构成「已知 series」但不进 120s 窗
    store::commit_batch(
        &mut c,
        &ReadBatch {
            observations: vec![obs("old", at - 6 * 3_600_000, 1000, 10)],
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
    let written = collector::produce_point(&mut c, "run-1", at, PointTrigger::Scheduled).unwrap();
    assert_eq!(written, 1, "known_series（近 24h 存储）缺一不可");
    let pts = store::load_points_since(&c, 0).unwrap();
    assert_eq!(pts[0].status, PointStatus::Gap);
    assert!(pts[0].tps.is_none());
    assert_eq!(pts[0].sample_count, 0);
}

/// 故障注入：单源失败不丢其他 series；旧 usage_* 行数前后相等；失败源游标/状态不动
#[test]
fn failing_source_is_isolated_and_old_usage_tables_untouched() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().to_path_buf();
    // dsh 数据根 = 一个普通文件 → read_dir 失败（非 NotFound）→ 整源 Err
    std::fs::create_dir_all(home.join(".dsh")).unwrap();
    std::fs::write(home.join(".dsh").join("sessions"), "not a dir").unwrap();
    // zcode = 合成库（一行成功样本）
    zcode_fixture_db(&home, BASE_MS);

    let mut c = conn("sampling-isolation");
    seed_old_usage(&c);
    let usage_detail_before = count(&c, "usage_detail");
    let usage_daily_before = count(&c, "usage_daily");
    let usage_cursor_before = count(&c, "usage_cursor");
    let usage_session_before = count(&c, "usage_session");

    let round = collector::run_round(&mut c, &home, BASE_MS);
    let dsh = round.reports.iter().find(|r| r.source == "dsh").unwrap();
    let zcode = round.reports.iter().find(|r| r.source == "zcode").unwrap();
    assert!(!dsh.ok, "dsh 坏根必须整源失败");
    assert!(dsh.error_code.is_some());
    assert!(zcode.ok, "zcode 不受 dsh 失败牵连");
    assert!(zcode.committed >= 1, "zcode 样本必须入库");
    assert_eq!(round.total_committed, zcode.committed, "失败源不计入提交");

    // zcode 事实落库、游标推进；dsh 水位不动（读取失败不改变水位）
    assert!(store::load_cursor(&c, "zcode").unwrap().is_some());
    assert!(
        store::load_cursor(&c, "dsh").unwrap().is_none(),
        "失败源不得推进水位"
    );
    // V2 Task 6：失败健康态落库（ok=false + 错误 detail，无载荷原文）
    let dsh_state = store::load_source_state(&c, "dsh").unwrap().unwrap();
    assert!(!dsh_state.ok, "读取 Err 必须持久化失败 SourceState");
    assert!(
        dsh_state.detail.as_deref().is_some_and(|d| !d.is_empty()),
        "detail 携带错误码+定位"
    );
    let facts = store::load_recent(&c, 0).unwrap();
    assert!(
        facts.iter().any(|o| o.source == "zcode"),
        "其他源的 series 完整保留"
    );

    // 旧 usage 四桶行数逐表相等（TPS 写入不碰旧账本）
    assert_eq!(count(&c, "usage_detail"), usage_detail_before);
    assert_eq!(count(&c, "usage_daily"), usage_daily_before);
    assert_eq!(count(&c, "usage_cursor"), usage_cursor_before);
    assert_eq!(count(&c, "usage_session"), usage_session_before);
}

/// 24 小时保留清理走慢节奏：1 小时内第二次调用是空操作，跨过节奏后再次清理
#[test]
fn purge_runs_on_slow_cadence() {
    let mut c = conn("sampling-purge");
    let at = BASE_MS;
    store::commit_batch(
        &mut c,
        &ReadBatch {
            observations: vec![
                obs("fresh", at, 1000, 10),
                obs("stale", at - 25 * 3_600_000, 1000, 10),
            ],
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
    let deleted = collector::maybe_purge(&c, at + 60_000).unwrap();
    assert!(deleted > 0, "首轮清理删掉超窗行");
    assert_eq!(store::load_recent(&c, 0).unwrap().len(), 1);
    let again = collector::maybe_purge(&c, at + 2 * 3_600_000).unwrap();
    assert_eq!(again, 0, "1 小时节奏内第二轮是空操作");
    let later = collector::maybe_purge(&c, at + 2 * 3_600_000 + 3_601_000).unwrap();
    assert_eq!(later, 0, "节奏到达但已无超窗行 → 0（且节奏锚点前移）");
}

/// SQLite 查询只尾查：DAO 的窗口查询必须走 ended/sampled 索引（EXPLAIN QUERY PLAN 锁）
#[test]
fn sqlite_queries_are_tail_only_with_index_backed_plans() {
    let c = conn("sampling-tail");
    // 与 dao/tps.rs 两条窗口查询同形的 SQL（WHERE > ? + ORDER BY 同列）
    let plans = |sql: &str| -> Vec<String> {
        let mut stmt = c.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        rows
    };
    let obs_plan = plans(
        "SELECT stable_call_key FROM tps_observation WHERE ended_at_ms > 1 ORDER BY ended_at_ms",
    );
    assert!(
        obs_plan
            .iter()
            .any(|d| d.contains("idx_tps_observation_ended")),
        "事实查询必须走 ended 索引（只尾查），实际 {obs_plan:?}"
    );
    let point_plan =
        plans("SELECT tick_id FROM tps_point WHERE sampled_at > 1 ORDER BY sampled_at");
    assert!(
        point_plan
            .iter()
            .any(|d| d.contains("idx_tps_point_sampled")),
        "采样点查询必须走 sampled 索引（只尾查），实际 {point_plan:?}"
    );
}

// ===========================================================================
// 总开关与查询面：disabled 报告不删历史；Token 区间零影响
// ===========================================================================

/// 总开关关闭：查询报 disabled、不读不删历史；重开后新 runId 断线由 runId 字段承载
#[test]
fn master_switch_off_reports_disabled_and_keeps_history() {
    let mut c = conn("sampling-disabled");
    set_usage_settings(&c, false);
    // 关闭态下也允许写入历史（历史保留语义：查询层只读，不写不删）
    store::commit_batch(
        &mut c,
        &ReadBatch {
            observations: vec![obs("h1", BASE_MS, 1000, 10)],
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
    collector::produce_point(&mut c, "run-1", BASE_MS + 1000, PointTrigger::Scheduled).unwrap();

    let snap = query::snapshot(&c, &TpsSelection::default(), BASE_MS + 2000).unwrap();
    assert!(snap.disabled, "关闭时查询必须报 disabled");
    assert!(snap.snapshot.is_none(), "关闭时不出快照载荷");
    let pts = query::points(
        &c,
        &TpsSelection::default(),
        TpsViewRange::Last1h,
        BASE_MS + 2000,
    )
    .unwrap();
    assert!(pts.disabled);
    assert!(pts.points.is_empty(), "关闭时不返回曲线（历史仍在库）");
    assert_eq!(count(&c, "tps_observation"), 1, "历史不删");
    assert_eq!(count(&c, "tps_point"), 1, "历史不删");

    // 重开 → 查询恢复；新 runId 由调度器承载（断线语义见 scheduler 用例）
    set_usage_settings(&c, true);
    let snap = query::snapshot(&c, &TpsSelection::default(), BASE_MS + 3000).unwrap();
    assert!(!snap.disabled);
    assert!(snap.snapshot.is_some());
}

/// Token 区间切换（usage.settings.miniBarRange）对 TPS 曲线/快照零影响（Spec §7 第一条）
#[test]
fn token_range_switch_has_zero_effect_on_tps_state() {
    let mut c = conn("sampling-token-range");
    set_usage_settings(&c, true);
    store::commit_batch(
        &mut c,
        &ReadBatch {
            observations: vec![obs("a", BASE_MS, 5000, 500)],
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
    collector::produce_point(&mut c, "run-1", BASE_MS + 60_000, PointTrigger::Scheduled).unwrap();
    let now = BASE_MS + 61_000;
    let before_pts =
        query::points(&c, &TpsSelection::default(), TpsViewRange::Last1h, now).unwrap();
    let before_snap = query::snapshot(&c, &TpsSelection::default(), now).unwrap();

    // 切 Token 区间 today → 30d（TPS 域不读这个字段）
    set_usage_settings(&c, true);
    c.execute(
        "UPDATE settings SET value = replace(value, '\"miniBarRange\":\"today\"', '\"miniBarRange\":\"30d\"') WHERE key = 'usage.settings'",
        [],
    )
    .unwrap();

    let after_pts = query::points(&c, &TpsSelection::default(), TpsViewRange::Last1h, now).unwrap();
    let after_snap = query::snapshot(&c, &TpsSelection::default(), now).unwrap();
    assert_eq!(
        serde_json::to_string(&before_pts).unwrap(),
        serde_json::to_string(&after_pts).unwrap(),
        "Token 区间切换不得改变 TPS 曲线"
    );
    assert_eq!(
        serde_json::to_string(&before_snap).unwrap(),
        serde_json::to_string(&after_snap).unwrap(),
        "Token 区间切换不得改变 TPS 快照"
    );
}

// ===========================================================================
// 快照 sourceStates：真实来源状态 + 状态分类（绝不引用 policy 层占位形态）
// ===========================================================================

/// sourceStates 必须覆盖全部注册来源且来自存储（真实状态）；未读过 = unconfigured；
/// codex 缺口标记保留；有效来源 = active；tps-status= 前缀机器可解析
#[test]
fn snapshot_surfaces_real_source_states_with_status_taxonomy() {
    let mut c = conn("sampling-sourcestates");
    set_usage_settings(&c, true);
    // zcode：真实 ok 状态 + 一条有效事实 → active
    store::commit_batch(
        &mut c,
        &ReadBatch {
            observations: vec![{
                let mut o = obs("z1", BASE_MS, 1000, 100);
                o.source = "zcode".into();
                o.model = "deepseek-v4.1-flash".into();
                o
            }],
            source_state: SourceState {
                source: "zcode".into(),
                cursor: SourceCursor {
                    source: "zcode".into(),
                    value: "row-42".into(),
                },
                ok: true,
                detail: None,
            },
            diagnostics: Vec::new(),
            next_cursor: SourceCursor {
                source: "zcode".into(),
                value: "row-42".into(),
            },
        },
    )
    .unwrap();
    // codex：字段不足缺口标记（Task 4 形态）
    store::commit_batch(
        &mut c,
        &ReadBatch {
            observations: Vec::new(),
            source_state: SourceState {
                source: "codex".into(),
                cursor: SourceCursor::empty("codex"),
                ok: true,
                detail: Some(
                    "insufficient-generation-timing: metrics 管道已通但当前没有同调用可靠生成窗"
                        .into(),
                ),
            },
            diagnostics: Vec::new(),
            next_cursor: SourceCursor::empty("codex"),
        },
    )
    .unwrap();
    // claude：装了统计键（install 记录在）但还没收到任何遥测 → 待重启
    store::save_state(
        &c,
        &install::record_state_key("claude"),
        &json!({"tool": "claude", "entries": []}),
    )
    .unwrap();

    let snap = query::snapshot(&c, &TpsSelection::default(), BASE_MS + 1000)
        .unwrap()
        .snapshot
        .expect("启用态必有快照");

    let states: std::collections::BTreeMap<&str, &SourceState> = snap
        .source_states
        .iter()
        .map(|s| (s.source.as_str(), s))
        .collect();
    assert_eq!(
        states.len(),
        collector::REGISTERED_SOURCES.len(),
        "sourceStates 必须覆盖全部注册来源（占位形态只从观察来源推导，会漏源）"
    );
    for src in collector::REGISTERED_SOURCES {
        assert!(
            states.contains_key(src),
            "注册来源 {src} 必须在 sourceStates"
        );
    }

    let status_of = |src: &str| -> String {
        let detail = states[src].detail.clone().unwrap_or_default();
        detail
            .split("; ")
            .next()
            .unwrap_or_default()
            .trim_start_matches("tps-status=")
            .to_string()
    };
    assert_eq!(status_of("zcode"), "active", "有有效事实的来源 = 有效");
    assert_eq!(status_of("codex"), "insufficient-timing", "字段不足");
    assert!(
        states["codex"]
            .detail
            .as_deref()
            .unwrap()
            .contains("insufficient-generation-timing"),
        "codex 原缺口标记必须原样保留"
    );
    assert_eq!(
        status_of("claude"),
        "await-restart",
        "已装未收到遥测 = 待重启"
    );
    assert_eq!(status_of("dsh"), "unconfigured", "从未产出读取批 = 未配置");
    // zcode 的真实游标来自存储（不是占位的空游标）
    assert_eq!(states["zcode"].cursor.value, "row-42");
    // series 快照面照常工作（与 sourceStates 解耦）
    assert_eq!(snap.series.len(), 1);
}

/// 状态分类纯函数的判据表（未配置/待重启/无样本/字段不足/读失败/有效）
#[test]
fn classify_source_status_covers_taxonomy() {
    use multi_agents_manager_lib::services::tps::query::{classify_source_status, TpsSourceStatus};
    let st = |ok: bool, detail: Option<&str>| SourceState {
        source: "x".into(),
        cursor: SourceCursor::empty("x"),
        ok,
        detail: detail.map(str::to_string),
    };
    assert_eq!(
        classify_source_status("dsh", &None, 0, 0, false),
        TpsSourceStatus::Unconfigured
    );
    assert_eq!(
        classify_source_status(
            "claude",
            &Some(st(false, Some("tps-receiver-port-conflict"))),
            0,
            0,
            true
        ),
        TpsSourceStatus::ReadFailed,
        "读取失败/端口冲突必须如实单列"
    );
    assert_eq!(
        classify_source_status("claude", &Some(st(true, None)), 0, 0, true),
        TpsSourceStatus::AwaitRestart,
        "已装未收到遥测 = 待重启"
    );
    assert_eq!(
        classify_source_status("opencode", &Some(st(true, None)), 0, 0, false),
        TpsSourceStatus::Unconfigured,
        "观察插件未安装 = 未配置"
    );
    assert_eq!(
        classify_source_status(
            "codex",
            &Some(st(true, Some("insufficient-generation-timing: …"))),
            0,
            0,
            false
        ),
        TpsSourceStatus::InsufficientTiming
    );
    assert_eq!(
        classify_source_status("dsh", &Some(st(true, None)), 3, 0, false),
        TpsSourceStatus::NoSample,
        "有事实全无效 = 无样本"
    );
    assert_eq!(
        classify_source_status("kimi", &Some(st(true, None)), 5, 2, false),
        TpsSourceStatus::Active
    );
}

// ===========================================================================
// 接收器：端口探测（真实 bind）/ ingest 守卫 / 端口冲突报状态 / 凭证不回传
// ===========================================================================

/// 端口探测走真实 bind（测试只用临时端口：候选 = [已占临时端口, 0]）：
/// 被占 → 跳过；可用 → 返回真实绑定端口；全占用 → None
#[test]
fn probe_free_port_in_uses_real_bind_and_reports_exhaustion() {
    // 占住一个临时端口（bind :0 由 OS 分配，绝不碰 47181–47190）
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let occupied_port = occupied.local_addr().unwrap().port();
    // 候选 1 = 被占端口（探测必须跳过）；候选 2 = :0（必可用）
    let picked = receiver::probe_free_port_in(&[occupied_port, 0]).expect("存在可用候选");
    assert_ne!(picked, occupied_port, "被占端口必须被真实 bind 探测排除");
    // 全占用 → None（调用方报状态，绝不静默换端口）
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let held_port = held.local_addr().unwrap().port();
    assert_eq!(
        receiver::probe_free_port_in(&[held_port]),
        None,
        "全部候选被占 → None（报状态，不换端口）"
    );
    // 默认候选表 = 47181 + 顺延区间（形态锁；生产才真 bind 它们）
    let candidates = receiver::default_port_candidates();
    assert_eq!(candidates.first(), Some(&DEFAULT_RECEIVER_PORT));
    assert_eq!(candidates.len(), 10, "47181 + 47182–47190");
}

/// ingest 守卫：Bearer 不符 / 超限 / 坏体 拒绝；合法报文提交为 claude ReadBatch，
/// 诊断流入 sourceState（成功属性漂移不静默缩样本）
#[test]
fn ingest_guards_and_commit_flow() {
    let mut c = conn("sampling-ingest");
    let credential = "a".repeat(64);
    let body = otlp_body(&"t1".repeat(16), &"s1".repeat(8), BASE_MS, 5000, 300);
    let now = BASE_MS + 10_000;

    // Bearer 缺失 / 不符 → 拒绝
    assert!(matches!(
        receiver::ingest(
            &mut receiver::IngestDeps {
                expected_credential: &credential,
                conn: &mut c
            },
            None,
            Some(body.len()),
            &body,
            now
        ),
        IngestOutcome::Unauthorized
    ));
    assert!(matches!(
        receiver::ingest(
            &mut receiver::IngestDeps {
                expected_credential: &credential,
                conn: &mut c
            },
            Some("Bearer wrong"),
            Some(body.len()),
            &body,
            now
        ),
        IngestOutcome::Unauthorized
    ));
    // 超 1MiB → 在缓冲完整 body 之前就拒绝（Content-Length 闸）
    assert!(matches!(
        receiver::ingest(
            &mut receiver::IngestDeps {
                expected_credential: &credential,
                conn: &mut c
            },
            Some(&format!("Bearer {credential}")),
            Some(MAX_BODY_BYTES + 1),
            &[],
            now
        ),
        IngestOutcome::TooLarge
    ));
    // 坏体 → 400 语义（非 OTLP JSON；`{}` 是合法零 span 报文，会 Accepted 0——不是坏体）
    assert!(matches!(
        receiver::ingest(
            &mut receiver::IngestDeps {
                expected_credential: &credential,
                conn: &mut c
            },
            Some(&format!("Bearer {credential}")),
            Some(8),
            b"not-json",
            now
        ),
        IngestOutcome::BadBody(_)
    ));

    // 合法报文 → 提交为 claude 批（观察 + 诊断入库）
    let out = receiver::ingest(
        &mut receiver::IngestDeps {
            expected_credential: &credential,
            conn: &mut c,
        },
        Some(&format!("Bearer {credential}")),
        Some(body.len()),
        &body,
        now,
    );
    let accepted = match out {
        IngestOutcome::Accepted { observations, .. } => observations,
        other => panic!("合法报文必须 Accepted，实际 {other:?}"),
    };
    assert_eq!(accepted, 1);
    let facts = store::load_recent(&c, 0).unwrap();
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].source, "claude");
    assert_eq!(facts[0].method, TimingMethod::NativeEventApprox);
    // claude 的状态/游标入库（同事务）；无诊断时 detail 不造噪音
    let st = store::load_source_state(&c, "claude").unwrap().unwrap();
    assert!(st.ok);
    // 幂等重投递：同 trace+span 重读不翻倍
    let again = receiver::ingest(
        &mut receiver::IngestDeps {
            expected_credential: &credential,
            conn: &mut c,
        },
        Some(&format!("Bearer {credential}")),
        Some(body.len()),
        &body,
        now,
    );
    assert!(matches!(
        again,
        IngestOutcome::Accepted {
            observations: 1,
            ..
        }
    ));
    assert_eq!(
        store::load_recent(&c, 0).unwrap().len(),
        1,
        "重投递幂等（稳定键 UPSERT）"
    );
}

/// 端口冲突报状态：持久化端口被占 → Err（detail 带 tps-receiver-port-conflict），
/// 把 claude 来源状态写成 ok=false（Spec §5「以后端口冲突报状态，不杀进程或擅自换端口」）
#[test]
fn persisted_port_conflict_reports_status_and_never_switches() {
    // 占住一个临时端口（绝不 bind 47181–47190）
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = held.local_addr().unwrap().port();

    let c = conn("sampling-port-conflict");
    let config = ReceiverConfig {
        port,
        credential: "b".repeat(64),
        created_at_ms: BASE_MS,
    };
    // 冲突绑定失败 → Err
    let err = receiver::bind_receiver_port(port).unwrap_err();
    assert!(
        err.detail.contains("tps-receiver-port-conflict"),
        "冲突 detail 必须带机器可定位前缀，实际 {}",
        err.detail
    );
    assert!(
        err.detail.starts_with(&format!(
            "{prefix}: persisted port {port} is occupied",
            prefix = receiver::PORT_CONFLICT_PREFIX
        )),
        "绝不静默换端口的语义锁在文案契约上（机器前缀 + 端口），实际 {}",
        err.detail
    );
    // 报状态：claude sourceState = ok=false + 同前缀
    receiver::record_port_conflict(&c, &config, BASE_MS);
    let st = store::load_source_state(&c, "claude").unwrap().unwrap();
    assert!(!st.ok);
    assert!(st
        .detail
        .as_deref()
        .unwrap()
        .contains("tps-receiver-port-conflict"));
}

/// 安装回执 IPC DTO：凭证/敏感旧值绝不回传 webview（脱敏 + 指纹）
#[test]
fn install_receipt_dto_masks_credential_and_sensitive_old_values() {
    let temp = TempDir::new().unwrap();
    let home: PathBuf = temp.path().to_path_buf();
    let preview = install::preview_at(&home, "claude").unwrap();
    let receipt = install::apply_at(&home, &preview.preview_id, InstallChoice::AppendOnly).unwrap();
    let raw_credential = receipt
        .receiver
        .as_ref()
        .expect("claude 回执带接收器")
        .credential
        .clone();

    let dto = TpsInstallReceiptDto::from(&receipt);
    let wire = serde_json::to_string(&dto).unwrap();
    assert!(
        !wire.contains(&raw_credential),
        "回执 IPC 载荷绝不能携带明文 Bearer 凭证"
    );
    assert!(
        dto.receiver.as_ref().map(|r| r.port > 0).unwrap_or(false),
        "DTO 保留端口供 UI 展示"
    );
    // 敏感键（OTEL_EXPORTER_OTLP_HEADERS）的旧值在 DTO 里绝不外显
    for e in &dto.entries {
        if e.key == "OTEL_EXPORTER_OTLP_HEADERS" {
            assert!(
                e.old_value.is_none(),
                "敏感键旧值必须置空（指纹已在 oldFingerprint）"
            );
        }
    }
}

/// IPC 错误与 API 错误一致：同一 reason 分类法覆盖 conflict / stale / expired / not-found
#[test]
fn ipc_error_reasons_consistent_with_api_errors() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().to_path_buf();
    let settings_dir = home.join(".claude");
    std::fs::create_dir_all(&settings_dir).unwrap();

    // 1) not-found：不存在的 preview id
    let err = install::apply_at(&home, "pv-missing", InstallChoice::AppendOnly).unwrap_err();
    assert_eq!(install_error_reason(&err.detail), Some("preview-not-found"));

    // 2) conflict：已有不同信号目标，append-only 拒绝
    std::fs::write(
        settings_dir.join("settings.json"),
        r#"{"env":{"OTEL_TRACES_EXPORTER":"jaeger"}}"#,
    )
    .unwrap();
    let preview = install::preview_at(&home, "claude").unwrap();
    let err = install::apply_at(&home, &preview.preview_id, InstallChoice::AppendOnly).unwrap_err();
    assert_eq!(install_error_reason(&err.detail), Some("conflict"));

    // 3) stale：预览后设置漂移 → 拒绝
    let preview = install::preview_at(&home, "claude").unwrap();
    std::fs::write(
        settings_dir.join("settings.json"),
        r#"{"env":{"OTEL_TRACES_EXPORTER":"user-changed"}}"#,
    )
    .unwrap();
    let err = install::apply_at(&home, &preview.preview_id, InstallChoice::AppendOnly).unwrap_err();
    assert_eq!(install_error_reason(&err.detail), Some("stale-preview"));

    // 4) expired：过期时刻判定走同前缀族（直接用前缀断言分类，不引时钟）
    assert_eq!(
        install_error_reason("tps-install-preview-expired: preview pv-1 expired"),
        Some("expired-preview")
    );
    // 5) 卸载冲突族
    assert_eq!(
        install_error_reason("tps-uninstall-conflict: keys changed by user"),
        Some("uninstall-conflict")
    );
    // 6) 非安装族错误 → None（不瞎分类）
    assert_eq!(install_error_reason("usage-db-failed: 其他错误"), None);
}

/// IPC 命令面形态锁：七个命令逐字注册 + 关键签名逐字（Spec §6 IPC 契约）
#[test]
fn seven_ipc_commands_are_registered_verbatim() {
    let lib_src = include_str!("../src/lib.rs");
    for cmd in [
        "commands::tps::tps_snapshot",
        "commands::tps::tps_points",
        "commands::tps::tps_refresh",
        "commands::tps::tps_view_heartbeat",
        "commands::tps::tps_install_preview",
        "commands::tps::tps_install_apply",
        "commands::tps::tps_uninstall",
    ] {
        assert!(
            lib_src.contains(cmd),
            "lib.rs generate_handler 必须注册 {cmd}"
        );
    }
    let cmd_src = include_str!("../src/commands/tps.rs");
    // 去空白 + 折叠「尾参逗号」：rustfmt 把多行签名的末参写成 `TpsSelection,`，
    // 归一化后与单行契约串逐字比对（逗号本身仍是参数分隔的判定位）
    let compact = |s: &str| {
        s.chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .replace(",)", ")")
    };
    let cmd_compact = compact(cmd_src);
    for sig in [
        "pub async fn tps_snapshot(selection: TpsSelection)",
        "pub async fn tps_points(selection: TpsSelection, view_range: TpsViewRange)",
        "pub async fn tps_refresh(selection: TpsSelection)",
        "pub async fn tps_view_heartbeat(view_id: String, visible: bool)",
        "pub async fn tps_install_preview(tool: String)",
        "pub async fn tps_install_apply(preview_id: String, choice: InstallChoice)",
        "pub async fn tps_uninstall(tool: String)",
    ] {
        assert!(
            cmd_compact.contains(&compact(sig)),
            "命令签名必须逐字匹配 Spec §6：{sig}"
        );
    }
}

/// IPC 选择器 camelCase 镜像锁（Spec §6：TpsSelection={toolId?,modelId?,channelKey?,method?}）
#[test]
fn selection_wire_form_is_camel_case() {
    let sel = TpsSelection {
        tool_id: Some("kimi".into()),
        model_id: Some("kimi-k2".into()),
        channel_key: None,
        method: Some(TimingMethod::ClientNative),
    };
    let wire = serde_json::to_value(&sel).unwrap();
    assert!(wire.get("toolId").is_some(), "toolId camelCase 键");
    assert!(wire.get("modelId").is_some(), "modelId camelCase 键");
    assert!(
        wire.get("channelKey").is_some(),
        "channelKey camelCase 键（值为 null）"
    );
    assert!(wire.get("method").is_some());
    assert!(wire.get("tool_id").is_none(), "snake_case 键不得上 IPC");
    // view_range 串形态
    assert_eq!(
        serde_json::to_string(&TpsViewRange::Last2m).unwrap(),
        "\"2m\""
    );
    assert_eq!(
        serde_json::to_string(&TpsViewRange::Last10m).unwrap(),
        "\"10m\""
    );
    assert_eq!(
        serde_json::to_string(&TpsViewRange::Last1h).unwrap(),
        "\"1h\""
    );
    assert_eq!(
        serde_json::to_string(&TpsViewRange::Today).unwrap(),
        "\"today\""
    );
}

/// view_range 起点换算（2m/10m/1h 固定回看；today = 本地当日零点）
#[test]
fn view_range_since_boundaries() {
    let now = BASE_MS;
    assert_eq!(TpsViewRange::Last2m.since_ms(now), now - 120_000);
    assert_eq!(TpsViewRange::Last10m.since_ms(now), now - 600_000);
    assert_eq!(TpsViewRange::Last1h.since_ms(now), now - 3_600_000);
    let today = TpsViewRange::Today.since_ms(now);
    assert!(today <= now, "today 起点不得晚于 now");
    assert!(
        now - today < 24 * 3_600_000,
        "today 起点是本地当日零点（距 now 不足 24h）"
    );
}

/// 评审 Important 2：>500 字节全中文诊断的截断必须**字符边界安全**——旧实现
/// `String::truncate(500)` 按字节切、第 500 字节落在多字节字符中间即 panic
/// （ingest 内 panic → spawn_blocking 转 JoinError → 持续 503 → 确定性丢数据）
#[test]
fn bounded_detail_truncates_chinese_diagnostics_on_char_boundary() {
    // 夹具前提：500 恰在 3 字节字符中间（旧实现必 panic 的形态）
    let raw = "诊断".repeat(600);
    assert!(
        !raw.is_char_boundary(500),
        "夹具前提失效：字节 500 应落在多字节字符中间"
    );
    // 8 条 × 3600 字节，join 后远超 500 字节
    let diags: Vec<String> = (0..8).map(|i| format!("{}-{i}", raw)).collect();
    let out = receiver::bounded_detail(&diags).expect("有诊断必得 detail");
    assert!(out.len() <= 500, "截断后必须 ≤500 字节，实际 {}", out.len());
    assert_eq!(
        out.len(),
        receiver::floor_char_boundary(raw.as_bytes(), 500),
        "截断点必须是原始字节的向下字符边界"
    );
    // 切出的串不含半个字符（最后字符完整可独立解码）
    let last = out.chars().last().expect("非空");
    assert_ne!(last, '\0');
    // 短诊断原样保留（不误伤）
    let short = vec!["ok".to_string()];
    assert_eq!(receiver::bounded_detail(&short).as_deref(), Some("ok"));
}

/// 实时 HTTP 请求（手工最小 HTTP/1.1；Connection: close → read_to_end 收尾）
async fn post_ingest(port: u16, credential: &str, body: &[u8]) -> Result<u16, String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let attempt = async {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .map_err(|e| e.to_string())?;
        let head = format!(
            "POST /v1/traces HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {credential}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream
            .write_all(head.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        stream.write_all(body).await.map_err(|e| e.to_string())?;
        let mut buf = Vec::new();
        stream
            .read_to_end(&mut buf)
            .await
            .map_err(|e| e.to_string())?;
        let text = String::from_utf8_lossy(&buf);
        text.split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u16>().ok())
            .ok_or_else(|| format!("bad response head: {}", text.get(..80).unwrap_or(&text)))
    };
    tokio::time::timeout(std::time::Duration::from_secs(5), attempt)
        .await
        .map_err(|_| "timeout".to_string())?
}

/// 评审 Important 1 的实时路径覆盖：127.0.0.1:**0** 真实启动服务任务（绝不 bind
/// 47181–47190）→ 一次真实 HTTP 往返（from_std 路径在 debug 构建不 panic——tokio
/// 对阻塞套接字的 assert 已由 bind_receiver_port 的 set_nonblocking 解除）→ 优雅
/// 停机 → 任务退出**清守卫**（可再次启动 = 可重试）
#[tokio::test]
async fn live_serve_task_on_ephemeral_port_serves_one_request_and_releases_guard() {
    // 进程级 HOME 重定向 + 全局 DB（handler 的 ingest 走全局库；绝不触真机 home）
    support::setup();
    let home = dirs::home_dir().expect("setup 后 HOME 必指向临时目录");
    // 播种接收器配置（preview_at 走纯端口选择不 bind；apply 把凭证/端口落临时库）
    let preview = install::preview_at(&home, "claude").unwrap();
    install::apply_at(&home, &preview.preview_id, InstallChoice::AppendOnly).unwrap();
    let credential = install::load_receiver_config_at(&home, "claude")
        .unwrap()
        .expect("apply 后必有接收器配置")
        .credential;

    let std_listener = receiver::bind_receiver_port(0).unwrap();
    let port = std_listener.local_addr().unwrap().port();
    let handle = receiver::spawn_serve(std_listener).expect("守卫空闲必须能启动");

    // 就绪窗口内重试连接（20ms × 100 = 2s 上限），一次真实 HTTP 往返
    let body = otlp_body(&"f1".repeat(16), &"9a".repeat(8), BASE_MS, 5000, 300);
    let mut status = None;
    let mut last_err = String::new();
    for _ in 0..100 {
        match post_ingest(port, &credential, &body).await {
            Ok(s) => {
                status = Some(s);
                break;
            }
            Err(e) => {
                last_err = e;
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }
    }
    assert_eq!(
        status,
        Some(200),
        "实时请求必须 200（最近错误：{last_err}）"
    );

    // 端到端：请求确实经 handler → ingest → 提交进全局库
    let conn = Connection::open(home.join(".tuvis").join("tuvis.db")).unwrap();
    let facts = store::load_recent(&conn, 0).unwrap();
    assert!(
        facts.iter().any(|o| o.source == "claude"),
        "实时请求必须落为 claude 事实"
    );

    // 优雅停机 + 等任务退出：任务不得 panic（from_std 阻塞检查已解除）
    receiver::shutdown();
    handle
        .await
        .expect("serve 任务不得 panic（tokio 阻塞套接字检查已由 set_nonblocking 解除）");
    // 任务退出必须清守卫 → 可再次启动（重试能力证明）
    let again = receiver::bind_receiver_port(0).unwrap();
    assert!(
        receiver::spawn_serve(again).is_some(),
        "服务任务退出后守卫必须已清（可重试）"
    );
    receiver::shutdown();
}

/// R3（独立复核 P1）：源 IO 不持全局 DB 锁——采集轮拆成「载游标（短锁）→ 读源（无 conn）
/// → 提交（短锁）」。读阶段签名不含 Connection；本测试在**持有**本库互斥锁的情况下执行
/// 读阶段并完成，证明读源不需要数据库句柄（生产 runtime 路径的全局 DB 锁窗因此只覆盖
/// 载游标与提交两段）。三阶段结果与整轮便捷入口语义一致（游标推进、重读幂等）。
#[test]
fn source_io_runs_without_holding_the_db_lock() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().to_path_buf();
    zcode_fixture_db(&home, BASE_MS);
    let db = std::sync::Mutex::new(conn("sampling-lockfree"));

    // 阶段 1：短锁载游标
    let cursors = {
        let c = db.lock().unwrap();
        collector::load_cursors(&c)
    };
    assert_eq!(cursors.len(), collector::SCANNED_SOURCES.len());

    // 阶段 2：锁被本线程持有时读源仍能完成——读阶段不需要 conn
    let reads = {
        let _held = db.lock().unwrap();
        collector::read_sources(&home, BASE_MS, cursors)
    };
    let (_, zcode_read) = reads.iter().find(|(s, _)| *s == "zcode").unwrap();
    assert!(
        !zcode_read.as_ref().unwrap().observations.is_empty(),
        "zcode 样本读到"
    );

    // 阶段 3：短锁提交；报告与整轮一致
    let round = {
        let mut c = db.lock().unwrap();
        collector::commit_reads(&mut c, reads)
    };
    let zcode = round.reports.iter().find(|r| r.source == "zcode").unwrap();
    assert!(zcode.ok);
    assert!(round.total_committed >= 1);
    {
        let c = db.lock().unwrap();
        assert!(
            store::load_cursor(&c, "zcode").unwrap().is_some(),
            "提交后游标推进"
        );
    }
    // 整轮便捷入口再跑一次：零新增（幂等），与三阶段语义一致
    let again = {
        let mut c = db.lock().unwrap();
        collector::run_round(&mut c, &home, BASE_MS)
    };
    assert_eq!(again.total_committed, 0, "重读不重复入库");
}

/// R11（独立复核 P2）：点随身携带**发点时生效的计划节奏**——可见 60 s、后台 600 s；startup /
/// manual 点带当时模式的节奏；落库读回不丢。前端据此判「缺失的计划采样点」而非通用阈值。
#[test]
fn points_carry_schedule_cadence_by_mode_and_persist() {
    use multi_agents_manager_lib::services::tps::scheduler::{
        point_cadence_ms, Sampler, SamplerMode, TickAction, BACKGROUND_INTERVAL_MS,
        VISIBLE_POINT_INTERVAL_MS,
    };
    assert_eq!(
        point_cadence_ms(SamplerMode::Foreground),
        VISIBLE_POINT_INTERVAL_MS
    );
    assert_eq!(
        point_cadence_ms(SamplerMode::Background),
        BACKGROUND_INTERVAL_MS
    );

    fn emit_cadence(actions: &[TickAction], want: PointTrigger) -> Option<i64> {
        actions.iter().find_map(|a| match a {
            TickAction::EmitPoint {
                trigger,
                cadence_ms,
                ..
            } if *trigger == want => Some(*cadence_ms),
            _ => None,
        })
    }

    let mut s = Sampler::new("run-cadence", true);
    // 启动点：无租约 = 后台节奏
    let out = s.tick(BASE_MS, false);
    assert_eq!(
        emit_cadence(&out.actions, PointTrigger::Startup),
        Some(600_000)
    );
    // 可见：60 秒计划点带 60000
    s.heartbeat("v1", true, BASE_MS + 1);
    let out = s.tick(BASE_MS + 60_000, true);
    assert_eq!(
        emit_cadence(&out.actions, PointTrigger::Scheduled),
        Some(60_000)
    );
    // 可见态下 manual 点带 60000
    let manual = s.manual_refresh(BASE_MS + 70_000).expect("限频外可刷新");
    assert_eq!(emit_cadence(&manual, PointTrigger::Manual), Some(60_000));
    // 租约过期回后台：600 秒计划点带 600000
    let out = s.tick(BASE_MS + 70_000 + 600_000, false);
    assert_eq!(out.mode, SamplerMode::Background);
    assert_eq!(
        emit_cadence(&out.actions, PointTrigger::Scheduled),
        Some(600_000)
    );

    // 落库读回：cadence_ms 随点持久化
    let mut c = conn("sampling-cadence");
    let o = obs("c-1", BASE_MS + 60_000 - 1_000, 1_000, 100);
    let pts = multi_agents_manager_lib::services::tps::policy::build_point_with_schedule(
        &[o],
        &[],
        "run-cadence",
        BASE_MS + 60_000,
        PointTrigger::Scheduled,
        Some(60_000),
    );
    assert_eq!(pts[0].cadence_ms, Some(60_000));
    store::save_points(&mut c, &pts).unwrap();
    let back = store::load_points_since(&c, 0).unwrap();
    assert_eq!(back.len(), 1);
    assert_eq!(back[0].cadence_ms, Some(60_000), "cadence 落库读回不丢");
}
