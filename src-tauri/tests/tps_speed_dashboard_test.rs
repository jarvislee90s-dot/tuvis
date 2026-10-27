//! V2 区间/桶聚合与只读 dashboard IPC 集成测试（2026-10-26 桌面速度 V2 计划 Task 6）。
//!
//! 隔离纪律（2026-10-07 事故族）：全部用例走 `support::open_ledger_db(tag)` 的**用例私有
//! 库文件**（tempdir 下 `.tuvis/tuvis.db`），绝不触碰真机 `~/.tuvis` / `~/.dsh`；采集夹具
//! 是 tempdir 下的**合成**结构；桶对齐测试走 offset 钉死的纯函数，不依赖本机时区。
//!
//! 覆盖（brief Step 1/Step 4 逐条）：
//! * brief 算术：500/5s + 50/1s ⇒ stream 550/6、(stream.n, ttft.n) = (2, 3)、空桶 value=null；
//! * response 独立口径（Codex 形态：仅显式响应窗，stream n=0）；
//! * 失败/取消只进候选/可靠用时池，不入任何速度 n；双窗皆缺失 ⇒ missing_timing；
//! * 辅助用途（AssistTitle/AutoSummary）整体排除；
//! * 旧 tps_point 行对 V2 求和零影响（不截 20 条、不叠加旧点）；
//! * 同 series 跨 session 合并（session_count=2），跨 model/method 分列；
//! * 半开完成窗 `(from, to]`：==from 排除、==to 包含、跨起点整条按完成归属；
//! * 边缘部分桶 ≤ 桶宽对齐名义数+1、裁剪 span 与 isPartialBucket、跨午夜（offset 钉死）；
//! * 空闲 20 分钟不清历史，latest_* 仍暴露；
//! * 总授权 disabled ⇒ 空 series/桶但保留历史与来源状态，重开恢复；
//! * 来源状态矩阵：parser-errors / ok=false ⇒ NeedsCheck、旧 Codex 缺口前缀 ⇒ Ready
//!   （stream=false）、未安装 ⇒ Unconfigured（结构能力照报）、last_record_at 正确；
//! * 采集读取 Err ⇒ 失败 SourceState 落库且游标不动，下一成功批恢复健康；
//! * SQL 有效性 CTE 与 Task 1 Rust 谓词逐记录一致（矩阵契约测试）；
//! * EXPLAIN QUERY PLAN 走索引（观察表不全表扫描）；
//! * `derive_metrics` 纯函数（同输入同输出、无隐藏状态）。

mod support;

use multi_agents_manager_lib::database::dao::speed as speed_dao;
use multi_agents_manager_lib::services::tps::collector;
use multi_agents_manager_lib::services::tps::install;
use multi_agents_manager_lib::services::tps::model::{
    Outcome, OutputScope, PointStatus, PointTrigger, Purpose, ReadBatch, SeriesKey, SourceCursor,
    SourceState, TimingMethod, TpsObservation, TpsPoint, TpsQuality,
};
use multi_agents_manager_lib::services::tps::store;
use multi_agents_manager_lib::services::tps::v2::model::{
    SpeedDashboardQuery, SpeedDashboardSnapshot, SpeedMetricMode, SpeedRange, SpeedSort,
    SpeedSourceStatus, SpeedSourceView,
};
use multi_agents_manager_lib::services::tps::v2::query::{
    self, bucket_spans, bucket_spans_with_offset,
};
use multi_agents_manager_lib::services::tps::v2::sums::{derive_metrics, SpeedSums};
use rusqlite::Connection;
use serde_json::json;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// 固定假时钟基准（纯整数，不读系统时钟）
const BASE_MS: i64 = 1_700_000_000_000;
const HOUR_MS: i64 = 3_600_000;
const MIN_MS: i64 = 60_000;

// ==== 通用夹具 ====

/// 用例私有库（tempdir 下的真实 DB 文件，schema::init 建齐全部表与索引）
fn conn(tag: &str) -> Connection {
    support::open_ledger_db(tag)
}

fn set_usage_settings(c: &Connection, enabled: bool) {
    c.execute(
        "INSERT OR REPLACE INTO settings (key, value) VALUES ('usage.settings', ?1)",
        rusqlite::params![format!(
            r#"{{"enabled":{enabled},"miniBarRange":"today","miniBarToolRows":3,"detailRetentionDays":30,"collectIntervalMin":10,"providerMapRules":"","exportQuote":"","exportPose":"random"}}"#
        )],
    )
    .unwrap();
}

/// 观察夹具（kimi / kimi-k2 / client-native / MainTask / success 为默认，逐字段可覆写）
fn obs(key: &str, ended_at: i64, gen_ms: i64, output: i64) -> TpsObservation {
    TpsObservation {
        source: "kimi".into(),
        host_instance: "host-1".into(),
        stable_call_key: key.into(),
        started_at_ms: ended_at - gen_ms,
        first_token_at_ms: Some(ended_at - gen_ms + gen_ms / 4),
        ended_at_ms: ended_at,
        native_duration_ms: None,
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

/// 直写观察行（绕过采集；事实 UPSERT 幂等）
fn insert_obs(c: &mut Connection, rows: &[TpsObservation]) {
    store::commit_batch(
        c,
        &ReadBatch {
            observations: rows.to_vec(),
            source_state: SourceState {
                source: "kimi".into(),
                cursor: SourceCursor::empty("kimi"),
                ok: true,
                detail: None,
            },
            diagnostics: vec![],
            next_cursor: SourceCursor::empty("kimi"),
        },
    )
    .unwrap();
}

fn kimi_series() -> SeriesKey {
    SeriesKey {
        tool: "kimi".into(),
        model: "kimi-k2".into(),
        channel: None,
        method: TimingMethod::ClientNative,
    }
}

fn default_query(range: SpeedRange) -> SpeedDashboardQuery {
    SpeedDashboardQuery {
        range,
        selection: None,
        metric_mode: SpeedMetricMode::Stream,
        tool_id: None,
        search: String::new(),
        sort: SpeedSort::Tool,
    }
}

/// 落一份 SourceState（模拟既有来源状态机制写入）
fn save_source_state(c: &Connection, source: &str, ok: bool, detail: Option<&str>) {
    let st = SourceState {
        source: source.into(),
        cursor: SourceCursor::empty(source),
        ok,
        detail: detail.map(str::to_string),
    };
    store::save_state(c, &store::source_state_key(source), &json!(st)).unwrap();
}

fn find_view<'a>(snap: &'a SpeedDashboardSnapshot, source: &str) -> &'a SpeedSourceView {
    snap.source_states
        .iter()
        .find(|v| v.source == source)
        .unwrap_or_else(|| panic!("缺 {source} 的来源视图"))
}

/// 一个 value 点（该 series/时刻一条有效调用），用于 tps_point 播种
fn point_for(run: &str, at: i64) -> TpsPoint {
    TpsPoint {
        model_ms: None,
        tool_ms: None,
        avg_first_token_ms: None,
        tps: None,
        sample_count: 0,
        ttft_sample_count: 0,
        candidate_count: 0,
        timed_model_count: 0,
        timed_tool_count: 0,
        failed_count: 0,
        cancelled_count: 0,
        missing_timing_count: 0,
        latest_ended_at: None,
        age_ms: None,
        method: TimingMethod::ClientNative,
        output_scope: OutputScope::Unknown,
        quality: TpsQuality::Normal,
        sum_output_tokens: 0,
        sum_generation_ms: 0,
        ttft_sum_ms: None,
        point_id: format!("{run}:{at}:point"),
        series_key: kimi_series(),
        sampled_at: at,
        window_ms: 120_000,
        run_id: run.into(),
        trigger: PointTrigger::Scheduled,
        status: PointStatus::Value,
        cadence_ms: Some(60_000),
        sampled_keys: vec![],
    }
}

/// 合成 zcode 库（与 tps_sampling_test 同形：一条成功样本）
fn zcode_fixture_db(home: &Path, done_at: i64) {
    let path = home.join(".zcode").join("cli").join("db").join("db.sqlite");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let zc = Connection::open(&path).unwrap();
    zc.execute_batch(
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
    zc.execute(
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

// ===========================================================================
// 1. brief 算术：550/6、(2,3)、空桶 response=None
// ===========================================================================

#[test]
#[rustfmt::skip] // brief Step 1 的断言行须逐字在场（含行形），不重排
fn brief_arithmetic_stream_ttft_and_empty_bucket() {
    let mut c = conn("speed-brief-arith");
    set_usage_settings(&c, true);
    let to = BASE_MS;
    let from = to - HOUR_MS;
    // ① 500 tokens / 5s（stream 有效）
    let a = obs("a", to - 2 * MIN_MS, 5_000, 500);
    // ② 50 tokens / 1s（stream 有效）
    let b = obs("b", to - MIN_MS, 1_000, 50);
    // ③ 无输出但有可靠首点（只进 TTFT 池）
    let mut d = obs("d", to, 0, 0);
    d.output_tokens = None;
    d.generation_ms = None;
    d.first_token_at_ms = Some(d.started_at_ms + 300);
    insert_obs(&mut c, &[a, b, d]);

    let snap = query::dashboard(&c, &default_query(SpeedRange::Last1h), to).unwrap();
    assert!(!snap.disabled);
    assert_eq!(snap.range_from, from);
    assert_eq!(snap.range_to, to);
    assert_eq!(snap.series.len(), 1, "同 series 三条记录合一");
    let metrics = &snap.series[0].metrics;
    // brief Step 1 三条断言逐字（第三条在下方空桶块）
    assert!((metrics.stream.value.unwrap() - 550.0 / 6.0).abs() < 1e-9);
    assert_eq!((metrics.stream.sample_count, metrics.ttft_sample_count), (2, 3));
    assert_eq!(metrics.response.sample_count, 0, "响应口径独立计n");
    assert_eq!(metrics.response.value, None);
    assert!(metrics.avg_first_token_ms.is_some());
    assert_eq!(metrics.candidate_count, 3);
    assert_eq!(metrics.session_count, 1);
    assert_eq!(metrics.latest_stream_ended_at, Some(to - MIN_MS));

    // 空桶：无样本 value=null（不冒充 0 速度）
    let empty_bucket = snap
        .buckets
        .iter()
        .find(|b| b.metrics.candidate_count == 0)
        .expect("1h 窗内只有 3 个分钟位置，必有空桶");
    assert_eq!(empty_bucket.metrics.response.value, None);
    assert_eq!(empty_bucket.metrics.stream.value, None);
    assert_eq!(empty_bucket.metrics.stream.sample_count, 0);
}

/// response 独立口径：Codex 形态（仅显式响应窗，gen/native/首点全缺）
#[test]
fn response_metric_is_computed_independently() {
    let mut c = conn("speed-response-only");
    set_usage_settings(&c, true);
    let to = BASE_MS;
    // Codex 形态：显式响应窗 4s + 2s，输出 80 + 20；stream/TTFT 全不可入
    let mut r1 = obs("r1", to - MIN_MS, 0, 80);
    r1.source = "codex".into();
    r1.method = TimingMethod::LogBoundaryApprox;
    r1.model = "gpt-5.3".into();
    r1.generation_ms = None;
    r1.native_duration_ms = None;
    r1.first_token_at_ms = None;
    r1.response_elapsed_ms = Some(4_000);
    let mut r2 = obs("r2", to, 0, 20);
    r2.source = "codex".into();
    r2.method = TimingMethod::LogBoundaryApprox;
    r2.model = "gpt-5.3".into();
    r2.generation_ms = None;
    r2.native_duration_ms = None;
    r2.first_token_at_ms = None;
    r2.response_elapsed_ms = Some(2_000);
    insert_obs(&mut c, &[r1, r2]);

    let snap = query::dashboard(&c, &default_query(SpeedRange::Last1h), to).unwrap();
    assert_eq!(snap.series.len(), 1);
    let m = &snap.series[0].metrics;
    assert_eq!(snap.series[0].series_key.tool, "codex");
    assert_eq!(
        snap.series[0].series_key.method,
        TimingMethod::LogBoundaryApprox
    );
    assert!(
        (m.response.value.unwrap() - 100.0 / 6.0).abs() < 1e-9,
        "response = 1000×100/6000 = 100/6"
    );
    assert_eq!(m.response.sample_count, 2);
    assert_eq!(m.stream.sample_count, 0, "stream 独立有效集");
    assert_eq!(m.stream.value, None);
    assert_eq!(m.ttft_sample_count, 0, "Codex 无首点，TTFT 不伪造");
    assert_eq!(m.avg_first_token_ms, None);
    assert_eq!(
        m.model_ms,
        Some(6_000),
        "Codex 累计用时 = 响应累计用时（日志近似）"
    );
}

// ===========================================================================
// 2. 失败/取消/缺计时池
// ===========================================================================

#[test]
fn failed_cancelled_enter_candidate_and_model_ms_but_neither_speed() {
    let mut c = conn("speed-failed-pools");
    set_usage_settings(&c, true);
    let to = BASE_MS;
    let mut ok = obs("ok", to - 3 * MIN_MS, 1_000, 100);
    ok.native_duration_ms = Some(1_200);
    // 失败但有可靠原生用时（可计时失败进累计用时池，不入速度）
    let mut failed = obs("failed", to - 2 * MIN_MS, 1_000, 100);
    failed.outcome = Outcome::Failed;
    failed.failure_reason = Some("boom".into());
    failed.native_duration_ms = Some(900);
    // 取消：显式响应窗在场也不入速度（但可计时）
    let mut cancelled = obs("cancelled", to - MIN_MS, 1_000, 100);
    cancelled.outcome = Outcome::Cancelled;
    cancelled.response_elapsed_ms = Some(400);
    cancelled.generation_ms = None;
    cancelled.native_duration_ms = None;
    // 双窗皆缺失 ⇒ missing_timing
    let mut untimed = obs("untimed", to, 0, 0);
    untimed.output_tokens = None;
    untimed.generation_ms = None;
    untimed.native_duration_ms = None;
    untimed.response_elapsed_ms = None;
    untimed.first_token_at_ms = None;
    insert_obs(&mut c, &[ok, failed, cancelled, untimed]);

    let snap = query::dashboard(&c, &default_query(SpeedRange::Last1h), to).unwrap();
    assert_eq!(snap.series.len(), 1);
    let m = &snap.series[0].metrics;
    assert_eq!(m.candidate_count, 4);
    assert_eq!(m.failed_count, 1);
    assert_eq!(m.cancelled_count, 1);
    assert_eq!(m.missing_timing_count, 1);
    assert_eq!(m.stream.sample_count, 1, "失败/取消不入速度");
    assert_eq!(m.response.sample_count, 1, "只有 ok 经旧兼容路径入响应口径");
    assert_eq!(
        m.model_ms,
        Some(1_200 + 900 + 400),
        "可靠用时累计（含可计时失败/取消），不含双缺失记录"
    );
    assert_eq!(m.latest_stream_ended_at, Some(to - 3 * MIN_MS));
    assert_eq!(m.latest_response_ended_at, Some(to - 3 * MIN_MS));
}

// ===========================================================================
// 3. 辅助用途排除
// ===========================================================================

#[test]
fn aux_purposes_are_excluded_entirely() {
    let mut c = conn("speed-aux-excluded");
    set_usage_settings(&c, true);
    let to = BASE_MS;
    let main = obs("main", to - MIN_MS, 1_000, 100);
    let mut title = obs("title", to - MIN_MS + 1, 1_000, 999);
    title.purpose = Purpose::AssistTitle;
    let mut summary = obs("summary", to - MIN_MS + 2, 1_000, 999);
    summary.purpose = Purpose::AutoSummary;
    insert_obs(&mut c, &[main, title, summary]);

    let snap = query::dashboard(&c, &default_query(SpeedRange::Last1h), to).unwrap();
    assert_eq!(snap.series.len(), 1, "辅助用途不另立 series");
    let m = &snap.series[0].metrics;
    assert_eq!(m.candidate_count, 1, "候选计数也不含辅助用途");
    assert_eq!(m.stream.sample_count, 1);
    assert_eq!(m.stream.output_tokens, 100, "辅助输出的 999 不进分子");
}

// ===========================================================================
// 4. 旧 tps_point 零影响
// ===========================================================================

#[test]
fn old_tps_point_rows_have_zero_effect_on_v2_sums() {
    let mut c = conn("speed-old-points");
    set_usage_settings(&c, true);
    let to = BASE_MS;
    let a = obs("a", to - MIN_MS, 1_000, 100);
    insert_obs(&mut c, &[a]);
    // 旧点：同一 series、伪造的巨大累计（旧滚动点绝不叠加进 V2 区间值）
    let mut big = point_for("run-old", to - 2 * MIN_MS);
    big.sample_count = 20;
    big.sum_output_tokens = 999_999;
    big.sum_generation_ms = 999_999;
    store::save_points(&mut c, &[big]).unwrap();

    let snap = query::dashboard(&c, &default_query(SpeedRange::Last24h), to).unwrap();
    assert_eq!(snap.series.len(), 1);
    let m = &snap.series[0].metrics;
    assert_eq!(
        (
            m.stream.output_tokens,
            m.stream.duration_ms,
            m.stream.sample_count
        ),
        (100, 1_000, 1),
        "V2 只聚合 tps_observation，旧点不相加"
    );
}

// ===========================================================================
// 5. series 聚合与拆分
// ===========================================================================

#[test]
fn same_series_merges_across_sessions_and_splits_by_model_method() {
    let mut c = conn("speed-series-split");
    set_usage_settings(&c, true);
    let to = BASE_MS;
    let mut s1 = obs("s1", to - 3 * MIN_MS, 1_000, 100);
    s1.session_key = Some("sess-a".into());
    let mut s2 = obs("s2", to - 2 * MIN_MS, 1_000, 100);
    s2.session_key = Some("sess-b".into());
    let mut other_model = obs("s3", to - MIN_MS, 1_000, 100);
    other_model.model = "kimi-linear".into();
    other_model.session_key = Some("sess-a".into());
    let mut other_method = obs("s4", to, 1_000, 100);
    other_method.method = TimingMethod::StreamObserved;
    other_method.session_key = Some("sess-a".into());
    insert_obs(&mut c, &[s1, s2, other_model, other_method]);

    let snap = query::dashboard(&c, &default_query(SpeedRange::Last1h), to).unwrap();
    assert_eq!(snap.series.len(), 3, "跨 session 合并；model/method 分列");
    let merged = snap
        .series
        .iter()
        .find(|s| {
            s.series_key.model == "kimi-k2" && s.series_key.method == TimingMethod::ClientNative
        })
        .unwrap();
    assert_eq!(merged.metrics.session_count, 2, "同 series 跨 session 计数");
    assert_eq!(merged.metrics.stream.sample_count, 2);
}

// ===========================================================================
// 6. 半开完成窗边界
// ===========================================================================

#[test]
fn half_open_window_boundaries_and_spanning_call() {
    let mut c = conn("speed-boundary");
    set_usage_settings(&c, true);
    let to = BASE_MS;
    let from = to - HOUR_MS;
    let at_from = obs("at-from", from, 1_000, 100); // == from → 排除
    let mut spanning = obs("spanning", from + MIN_MS, 6 * MIN_MS, 100); // 起点在窗外、完成在窗内
    spanning.started_at_ms = from - 5 * MIN_MS;
    let at_to = obs("at-to", to, 1_000, 100); // == to → 包含
    insert_obs(&mut c, &[at_from, spanning, at_to]);

    let snap = query::dashboard(&c, &default_query(SpeedRange::Last1h), to).unwrap();
    assert_eq!(snap.series.len(), 1);
    let m = &snap.series[0].metrics;
    assert_eq!(
        m.candidate_count, 2,
        "(from,to] 半开：==from 排除、跨起点按完成归属、==to 包含"
    );
    assert_eq!(m.stream.sample_count, 2);
}

// ===========================================================================
// 7. 空闲 20 分钟不清历史
// ===========================================================================

#[test]
fn idle_gap_keeps_series_and_surfaces_latest() {
    let mut c = conn("speed-idle");
    set_usage_settings(&c, true);
    let to = BASE_MS;
    let a = obs("a", to - 20 * MIN_MS, 1_000, 100);
    let b = obs("b", to, 1_000, 100);
    insert_obs(&mut c, &[a, b]);

    let snap = query::dashboard(&c, &default_query(SpeedRange::Last24h), to).unwrap();
    assert_eq!(snap.series.len(), 1, "空闲不清空（无最近10分钟清空规则）");
    let m = &snap.series[0].metrics;
    assert_eq!(m.stream.sample_count, 2, "不截最近20条");
    assert_eq!(m.latest_stream_ended_at, Some(to));
}

// ===========================================================================
// 8. 桶对齐（offset 钉死）
// ===========================================================================

#[test]
fn bucket_spans_align_clip_and_respect_offset() {
    let range = SpeedRange::Last1h;
    let width = 5 * MIN_MS;
    // 非对齐窗口：首桶裁剪、末桶裁剪，总数 ≤ 名义 12 + 1
    let to = 1_700_000_063_000_i64; // 任意非对齐时刻
    let from = to - HOUR_MS;
    let spans = bucket_spans_with_offset(from, to, range, 0);
    assert!(
        (12..=13).contains(&spans.len()),
        "边缘部分桶最多额外 +1，实际 {}",
        spans.len()
    );
    assert_eq!(spans.first().unwrap().start_ms, from, "首桶裁剪到 from");
    assert_eq!(spans.last().unwrap().end_ms, to, "末桶裁剪到 to");
    assert!(spans.first().unwrap().is_partial_bucket);
    assert!(spans.last().unwrap().is_partial_bucket);
    // 连续覆盖且中间桶不裁剪
    for pair in spans.windows(2) {
        assert_eq!(pair[0].end_ms, pair[1].start_ms, "桶互不重叠且连续");
    }
    for mid in &spans[1..spans.len() - 1] {
        assert!(!mid.is_partial_bucket);
        assert_eq!(mid.end_ms - mid.start_ms, width);
    }
    // 对齐（首桶已裁剪到 from，从第二个起检查墙钟对齐；offset 0 → UTC 墙钟）
    for s in &spans[1..] {
        assert_eq!(s.start_ms.rem_euclid(width), 0, "桶边界按墙钟对齐");
    }

    // 恰好对齐的窗口：恰好名义桶数、无部分桶
    let aligned_to = 1_700_004_600_000_i64; // 5 分钟整倍数
    let aligned_from = aligned_to - HOUR_MS;
    let exact = bucket_spans_with_offset(aligned_from, aligned_to, range, 0);
    assert_eq!(exact.len(), 12);
    assert!(exact.iter().all(|s| !s.is_partial_bucket));

    // 跨午夜 + 非整小时偏移（UTC+5:30）：1h 桶按本地墙钟对齐
    let off = 19_800_i32;
    let to2 = 1_700_000_063_000_i64;
    let from2 = to2 - SpeedRange::Last24h.window_ms();
    let spans2 = bucket_spans_with_offset(from2, to2, SpeedRange::Last24h, off);
    assert!(
        (24..=25).contains(&spans2.len()),
        "24h/1h 名义 24 桶，实际 {}",
        spans2.len()
    );
    for s in &spans2[1..] {
        assert_eq!(
            (s.start_ms + (off as i64) * 1000).rem_euclid(HOUR_MS),
            0,
            "本地墙钟整点对齐"
        );
    }
    assert_eq!(spans2.first().unwrap().start_ms, from2);
    assert_eq!(spans2.last().unwrap().end_ms, to2);

    // 各档位窗宽/桶宽逐字（1m/5m/10m/1h）
    assert_eq!(SpeedRange::Last15m.window_ms(), 15 * MIN_MS);
    assert_eq!(SpeedRange::Last15m.bucket_width_ms(), MIN_MS);
    assert_eq!(SpeedRange::Last1h.window_ms(), HOUR_MS);
    assert_eq!(SpeedRange::Last1h.bucket_width_ms(), 5 * MIN_MS);
    assert_eq!(SpeedRange::Last5h.window_ms(), 5 * HOUR_MS);
    assert_eq!(SpeedRange::Last5h.bucket_width_ms(), 10 * MIN_MS);
    assert_eq!(SpeedRange::Last24h.window_ms(), 24 * HOUR_MS);
    assert_eq!(SpeedRange::Last24h.bucket_width_ms(), HOUR_MS);
}

#[test]
fn dashboard_buckets_cover_range_and_match_span_layout() {
    let mut c = conn("speed-bucket-layout");
    set_usage_settings(&c, true);
    let to = 1_700_000_063_000_i64;
    // 两条记录落不同桶
    let a = obs("a", to - 10 * MIN_MS, 1_000, 100);
    let b = obs("b", to - 2 * MIN_MS, 1_000, 100);
    insert_obs(&mut c, &[a, b]);

    let snap = query::dashboard(&c, &default_query(SpeedRange::Last1h), to).unwrap();
    // 用同一公共函数（本机 offset）对比布局：与 dashboard 内部同源
    let spans = bucket_spans(snap.range_from, snap.range_to, SpeedRange::Last1h);
    assert_eq!(snap.buckets.len(), spans.len(), "桶数 = span 数");
    for (b, s) in snap.buckets.iter().zip(&spans) {
        assert_eq!(
            (b.start_ms, b.end_ms, b.is_partial_bucket),
            (s.start_ms, s.end_ms, s.is_partial_bucket)
        );
    }
    // 记录归属：完成时刻所在桶各计 1
    let with_a = snap
        .buckets
        .iter()
        .find(|b| b.start_ms < to - 10 * MIN_MS && b.end_ms >= to - 10 * MIN_MS)
        .unwrap();
    assert_eq!(with_a.metrics.stream.sample_count, 1);
    let last = snap.buckets.last().unwrap();
    assert_eq!(last.metrics.stream.sample_count, 1);
    assert_eq!(last.metrics.stream.output_tokens, 100);
}

// ===========================================================================
// 9. disabled：空载荷但保留历史与来源状态
// ===========================================================================

#[test]
fn disabled_reports_empty_but_keeps_history_and_source_states() {
    let mut c = conn("speed-disabled");
    set_usage_settings(&c, true);
    let to = BASE_MS;
    let a = obs("a", to - MIN_MS, 1_000, 100);
    insert_obs(&mut c, &[a]);
    save_source_state(&c, "kimi", true, None);

    set_usage_settings(&c, false);
    let snap = query::dashboard(&c, &default_query(SpeedRange::Last1h), to).unwrap();
    assert!(snap.disabled);
    assert!(snap.series.is_empty(), "关闭时空 series");
    assert!(snap.buckets.is_empty());
    assert_eq!(snap.selected_series, None);
    assert_eq!(
        snap.source_states.len(),
        7,
        "来源状态仍解析（安装事实不依赖样本）"
    );

    // 重开 → 历史原样可查（不删历史）
    set_usage_settings(&c, true);
    let snap = query::dashboard(&c, &default_query(SpeedRange::Last1h), to).unwrap();
    assert!(!snap.disabled);
    assert_eq!(snap.series.len(), 1);
    assert_eq!(snap.series[0].metrics.stream.sample_count, 1);
}

// ===========================================================================
// 10. 来源状态矩阵
// ===========================================================================

#[test]
fn source_state_matrix_and_last_record_at() {
    let mut c = conn("speed-source-matrix");
    set_usage_settings(&c, true);
    let to = BASE_MS;
    // kimi：正常事实 + ok 状态 → Ready；last_record_at = MAX(ended_at)
    let a = obs("a", to - MIN_MS, 1_000, 100);
    insert_obs(&mut c, &[a]);
    save_source_state(&c, "kimi", true, None);
    // workbuddy：parser-errors 前缀 → NeedsCheck（不牵连其他源）
    save_source_state(
        &c,
        "workbuddy",
        true,
        Some("parser-errors=3; bad-payload×3; limits"),
    );
    // zcode：ok=false → NeedsCheck
    save_source_state(&c, "zcode", false, Some("usage-source-io: io error"));
    // codex：旧流式缺口前缀 → Ready（stream=false，仅响应）
    save_source_state(
        &c,
        "codex",
        true,
        Some("insufficient-generation-timing: 旧占位"),
    );
    // claude：未安装 → Unconfigured（结构能力照报）
    // dsh：结构性在册但从未读过 → Unconfigured（never-read）

    let snap = query::dashboard(&c, &default_query(SpeedRange::Last1h), to).unwrap();
    assert_eq!(snap.source_states.len(), 7, "全部注册来源一一在列");

    let kimi = find_view(&snap, "kimi");
    assert_eq!(kimi.status, SpeedSourceStatus::Ready);
    assert!(kimi.installed);
    assert!(kimi.capabilities.stream && kimi.capabilities.response);
    assert_eq!(kimi.capabilities.stream_reason, None);
    assert_eq!(kimi.last_record_at, Some(to - MIN_MS));

    let wb = find_view(&snap, "workbuddy");
    assert_eq!(
        wb.status,
        SpeedSourceStatus::NeedsCheck,
        "parser-errors ⇒ NeedsCheck"
    );
    assert!(wb.detail.as_deref().unwrap().starts_with("parser-errors="));

    let zc = find_view(&snap, "zcode");
    assert_eq!(
        zc.status,
        SpeedSourceStatus::NeedsCheck,
        "ok=false ⇒ NeedsCheck"
    );
    assert!(!zc.detail.as_deref().unwrap().contains("parser-errors"));

    let codex = find_view(&snap, "codex");
    assert_eq!(
        codex.status,
        SpeedSourceStatus::Ready,
        "旧流式缺口前缀不降级整体状态"
    );
    assert!(!codex.capabilities.stream, "Codex stream 结构性缺失");
    assert!(codex.capabilities.response, "Codex response 可用");
    assert!(codex.capabilities.stream_reason.is_some());

    let claude = find_view(&snap, "claude");
    assert_eq!(
        claude.status,
        SpeedSourceStatus::Unconfigured,
        "未安装 ⇒ Unconfigured"
    );
    assert!(!claude.installed);
    assert!(
        claude.capabilities.stream && claude.capabilities.response,
        "「支持」是计算路径不是数据保证：未安装也报结构能力"
    );

    let dsh = find_view(&snap, "dsh");
    assert_eq!(
        dsh.status,
        SpeedSourceStatus::Unconfigured,
        "从未产出读取批"
    );
    assert_eq!(dsh.last_record_at, None);
}

/// 已装 claude 但零事实 → AwaitRestart（等待宿主重启）
#[test]
fn installed_claude_without_facts_is_await_restart() {
    let c = conn("speed-await-restart");
    set_usage_settings(&c, true);
    let to = BASE_MS;
    store::save_state(
        &c,
        &install::record_state_key("claude"),
        &json!({"tool": "claude"}),
    )
    .unwrap();
    let snap = query::dashboard(&c, &default_query(SpeedRange::Last1h), to).unwrap();
    let claude = find_view(&snap, "claude");
    assert_eq!(claude.status, SpeedSourceStatus::AwaitRestart);
    assert!(claude.installed);
}

// ===========================================================================
// 11. 搜索 / 过滤 / 排序 / 选择
// ===========================================================================

#[test]
fn search_filter_sort_and_selection_fallback() {
    let mut c = conn("speed-selection");
    set_usage_settings(&c, true);
    let to = BASE_MS;
    let kimi = obs("k1", to - MIN_MS, 1_000, 100); // kimi / kimi-k2
    let mut zcode = obs("z1", to - MIN_MS + 1, 500, 500); // zcode 快
    zcode.source = "zcode".into();
    zcode.model = "deepseek-v4".into();
    let mut slow = obs("w1", to - MIN_MS + 2, 10_000, 100); // workbuddy 慢
    slow.source = "workbuddy".into();
    slow.model = "wb-model".into();
    let mut no_speed = obs("n1", to - MIN_MS + 3, 0, 0); // 无有效速度
    no_speed.source = "opencode".into();
    no_speed.model = "oc-model".into();
    no_speed.output_tokens = None;
    no_speed.generation_ms = None;
    insert_obs(&mut c, &[kimi, zcode, slow, no_speed]);

    // sort=rate：按当前模式速度降序，无数值排末
    let mut q = default_query(SpeedRange::Last1h);
    q.sort = SpeedSort::Rate;
    q.metric_mode = SpeedMetricMode::Stream;
    let snap = query::dashboard(&c, &q, to).unwrap();
    let tools: Vec<&str> = snap
        .series
        .iter()
        .map(|s| s.series_key.tool.as_str())
        .collect();
    assert_eq!(
        tools,
        vec!["zcode", "kimi", "workbuddy", "opencode"],
        "按当前模式速度降序，null 最后"
    );

    // 选择保留：选中的 series 在筛选集内 → 原样；桶只为选中 series 计算
    let sel = kimi_series();
    q.selection = Some(sel);
    let snap = query::dashboard(&c, &q, to).unwrap();
    assert_eq!(snap.selected_series.as_ref().unwrap().tool, "kimi");
    assert!(snap.buckets.iter().all(|b| b.metrics.candidate_count <= 1));

    // 选择失效回退：选一个不在集合里的 → 排序第一
    q.selection = Some(SeriesKey {
        tool: "codex".into(),
        model: "gpt".into(),
        channel: None,
        method: TimingMethod::LogBoundaryApprox,
    });
    let snap = query::dashboard(&c, &q, to).unwrap();
    assert_eq!(
        snap.selected_series.as_ref().unwrap().tool,
        "zcode",
        "回退到排序第一"
    );

    // 字面子串搜索：LIKE 通配符不生效
    q.search = "%".into();
    q.selection = None;
    let snap = query::dashboard(&c, &q, to).unwrap();
    assert!(snap.series.is_empty(), "% 必须转义为字面量，不得当通配符");
    q.search = "deep".into();
    let snap = query::dashboard(&c, &q, to).unwrap();
    assert_eq!(snap.series.len(), 1);
    assert_eq!(snap.series[0].series_key.model, "deepseek-v4");
    // 搜索也匹配 tool（字面子串；tool 与 model 双侧）
    q.search = "kimi".into();
    let snap = query::dashboard(&c, &q, to).unwrap();
    assert_eq!(snap.series.len(), 1);
    assert_eq!(snap.series[0].series_key.tool, "kimi");

    // 工具筛选
    q.search.clear();
    q.tool_id = Some("kimi".into());
    let snap = query::dashboard(&c, &q, to).unwrap();
    assert_eq!(snap.series.len(), 1);
    assert_eq!(snap.series[0].series_key.tool, "kimi");
    assert_eq!(snap.selected_series.as_ref().unwrap().tool, "kimi");
}

// ===========================================================================
// 12. 采集读取失败：失败健康态落库、游标不动、下一成功批恢复
// ===========================================================================

#[test]
fn collector_read_failure_persists_failed_state_and_recovers() {
    let temp = TempDir::new().unwrap();
    let home: PathBuf = temp.path().to_path_buf();
    let db_path = home.join(".zcode").join("cli").join("db").join("db.sqlite");
    zcode_fixture_db(&home, BASE_MS);
    let mut c = conn("speed-collector-health");
    set_usage_settings(&c, true);

    // 第一轮：成功 → 游标推进、状态健康
    let round1 = collector::run_round(&mut c, &home, BASE_MS);
    assert!(
        round1
            .reports
            .iter()
            .find(|r| r.source == "zcode")
            .unwrap()
            .ok
    );
    let cursor_after_ok = store::load_cursor(&c, "zcode").unwrap().unwrap();
    assert!(store::load_source_state(&c, "zcode").unwrap().unwrap().ok);

    // 第二轮：库文件被替换为垃圾 → 读取 Err
    let backup = temp.path().join("db.bak");
    std::fs::rename(&db_path, &backup).unwrap();
    std::fs::write(&db_path, b"definitely not a sqlite database").unwrap();
    let round2 = collector::run_round(&mut c, &home, BASE_MS + 5_000);
    let zc = round2.reports.iter().find(|r| r.source == "zcode").unwrap();
    assert!(!zc.ok, "垃圾库必须整源 Err");
    // 失败健康态落库（ok=false + 错误 detail），游标不动
    let st = store::load_source_state(&c, "zcode").unwrap().unwrap();
    assert!(!st.ok, "读取 Err 必须持久化失败 SourceState");
    assert!(st.detail.is_some(), "detail 携带错误（无原文载荷）");
    assert_eq!(
        store::load_cursor(&c, "zcode").unwrap().unwrap(),
        cursor_after_ok,
        "失败源游标保持原水位"
    );

    // 查询面：失败期间该源报 NeedsCheck
    {
        let snap =
            query::dashboard(&c, &default_query(SpeedRange::Last24h), BASE_MS + 6_000).unwrap();
        assert_eq!(
            find_view(&snap, "zcode").status,
            SpeedSourceStatus::NeedsCheck
        );
    }

    // 第三轮：恢复文件 → 健康态回来，采集继续
    std::fs::remove_file(&db_path).unwrap();
    std::fs::copy(&backup, &db_path).unwrap();
    let round3 = collector::run_round(&mut c, &home, BASE_MS + 10_000);
    assert!(
        round3
            .reports
            .iter()
            .find(|r| r.source == "zcode")
            .unwrap()
            .ok
    );
    let st = store::load_source_state(&c, "zcode").unwrap().unwrap();
    assert!(st.ok, "下一成功批恢复健康");
    assert_eq!(
        store::load_cursor(&c, "zcode").unwrap().unwrap(),
        cursor_after_ok,
        "恢复批无新事实，游标不变"
    );

    let snap = query::dashboard(&c, &default_query(SpeedRange::Last24h), BASE_MS + 11_000).unwrap();
    assert_eq!(find_view(&snap, "zcode").status, SpeedSourceStatus::Ready);
}

// ===========================================================================
// 13. SQL 有效性 CTE == Rust 谓词（矩阵契约）
// ===========================================================================

#[test]
fn sql_validity_matches_rust_predicates_matrix() {
    let mut c = conn("speed-validity-matrix");
    let to = BASE_MS;
    let mut rows: Vec<TpsObservation> = Vec::new();
    let mut push = |tag: &str, mutate: &dyn Fn(&mut TpsObservation)| {
        let mut o = obs(&format!("m-{tag}"), to - MIN_MS, 1_000, 100);
        mutate(&mut o);
        rows.push(o);
    };
    // ① stream 有效（成功 + 正输出 + 正生成窗）
    push("stream-ok", &|_| {});
    // ② 仅响应（Codex 形态：显式响应窗、gen/native/首点全缺）
    push("resp-only", &|o: &mut TpsObservation| {
        o.generation_ms = None;
        o.native_duration_ms = None;
        o.first_token_at_ms = None;
        o.response_elapsed_ms = Some(5_000);
        o.method = TimingMethod::LogBoundaryApprox;
    });
    // ③ 显式响应短于生成窗（不一致不修复）
    push("resp-short", &|o: &mut TpsObservation| {
        o.response_elapsed_ms = Some(999);
    });
    // ④ 旧记录 native 兼容（native ≥ gen，无显式响应窗）
    push("legacy-ok", &|o: &mut TpsObservation| {
        o.native_duration_ms = Some(1_500);
    });
    // ⑤ 旧记录 native < gen → 兼容路径拒绝
    push("legacy-short", &|o: &mut TpsObservation| {
        o.native_duration_ms = Some(500);
    });
    // ⑥ 失败 + native（可计时失败：只进模型用时池）
    push("failed-timed", &|o: &mut TpsObservation| {
        o.outcome = Outcome::Failed;
        o.native_duration_ms = Some(800);
    });
    // ⑦ 取消（显式响应窗在场也不入速度，但可计时）
    push("cancelled-resp", &|o: &mut TpsObservation| {
        o.outcome = Outcome::Cancelled;
        o.generation_ms = None;
        o.native_duration_ms = None;
        o.response_elapsed_ms = Some(2_000);
    });
    // ⑦b 取消 + 旧 native（兼容路径被非成功拒收，但可计时）
    push("cancelled-native", &|o: &mut TpsObservation| {
        o.outcome = Outcome::Cancelled;
        o.native_duration_ms = Some(800);
    });
    // ⑧ 负生成窗
    push("neg-gen", &|o: &mut TpsObservation| {
        o.generation_ms = Some(-1);
    });
    // ⑨ 零生成窗
    push("zero-gen", &|o: &mut TpsObservation| {
        o.generation_ms = Some(0);
    });
    // ⑩ 零响应窗（显式）
    push("zero-resp", &|o: &mut TpsObservation| {
        o.generation_ms = None;
        o.native_duration_ms = None;
        o.first_token_at_ms = None;
        o.response_elapsed_ms = Some(0);
        o.method = TimingMethod::LogBoundaryApprox;
    });
    // ⑪ 仅 TTFT（无输出、无窗、首点在场）
    push("ttft-only", &|o: &mut TpsObservation| {
        o.output_tokens = None;
        o.generation_ms = None;
        o.native_duration_ms = None;
        o.first_token_at_ms = Some(o.started_at_ms + 250);
    });
    // ⑫ 首点恰等开始（ttft == 0 → Rust >= 0 判真）
    push("ttft-zero", &|o: &mut TpsObservation| {
        o.first_token_at_ms = Some(o.started_at_ms);
        o.generation_ms = Some(2_000);
        o.output_tokens = Some(50);
    });
    // ⑬ 首点倒挂（早于开始）
    push("ttft-inverted", &|o: &mut TpsObservation| {
        o.first_token_at_ms = Some(o.started_at_ms - 1);
    });
    // ⑭ 零输出
    push("zero-out", &|o: &mut TpsObservation| {
        o.output_tokens = Some(0);
    });
    // ⑮ 负输出
    push("neg-out", &|o: &mut TpsObservation| {
        o.output_tokens = Some(-5);
    });
    insert_obs(&mut c, &rows);

    // 与生产完全相同的 CTE 文本：逐记录断言 SQL 分类 == Rust 谓词
    let sql = format!(
        "WITH cand AS ({}) SELECT stable_call_key, stream_ok, resp_ok, ttft_ok, \
         (response_ms IS NOT NULL) AS has_resp_ms, model_ms_val \
         FROM cand",
        speed_dao::validity_cte_sql("")
    );
    let mut stmt = c.prepare(&sql).unwrap();
    let mut got: Vec<(String, bool, bool, bool, bool, Option<i64>)> = stmt
        .query_map([to - HOUR_MS, to], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, bool>(1)?,
                r.get::<_, bool>(2)?,
                r.get::<_, bool>(3)?,
                r.get::<_, bool>(4)?,
                r.get::<_, Option<i64>>(5)?,
            ))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    got.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        got.len(),
        rows.len(),
        "主任务全候选必须在行集（不得 WHERE 先删）"
    );

    let mut rust_rows = rows.clone();
    rust_rows.sort_by(|a, b| a.stable_call_key.cmp(&b.stable_call_key));
    for (o, sql_row) in rust_rows.iter().zip(&got) {
        let (key, stream_ok, resp_ok, ttft_ok, has_resp_ms, model_ms_val) = sql_row;
        assert_eq!(key, &o.stable_call_key);
        assert_eq!(
            stream_ok,
            &o.is_tps_valid(),
            "[{key}] stream 有效性 SQL != Rust"
        );
        assert_eq!(
            resp_ok,
            &o.is_response_valid(),
            "[{key}] response 有效性 SQL != Rust"
        );
        assert_eq!(
            ttft_ok,
            &o.ttft_ms().is_some(),
            "[{key}] TTFT 纳入 SQL != Rust"
        );
        assert_eq!(
            has_resp_ms,
            &o.response_sample_ms().is_some(),
            "[{key}] 响应样本时长 SQL != Rust"
        );
        let rust_model_ms = o
            .native_duration_ms
            .filter(|d| *d > 0)
            .or_else(|| o.response_sample_ms());
        assert_eq!(
            model_ms_val, &rust_model_ms,
            "[{key}] 可靠模型用时 SQL != Rust"
        );
    }
}

// ===========================================================================
// 14. EXPLAIN QUERY PLAN：索引
// ===========================================================================

#[test]
fn explain_query_plan_uses_indexes_no_full_scan() {
    let c = conn("speed-explain");
    let plans = |sql: &str, params: &[&dyn rusqlite::ToSql]| -> Vec<String> {
        let mut stmt = c.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        stmt.query_map(params, |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    // series 查询（带工具过滤）：?1 from / ?2 to / ?3 tool
    let series_sql = speed_dao::series_query_sql(true, false);
    let p = plans(&series_sql, &[&0i64, &BASE_MS, &"kimi"]);
    assert!(
        !p.iter().any(|d| d.contains("SCAN tps_observation")),
        "观察表不得全表扫描: {p:?}"
    );
    assert!(
        p.iter()
            .any(|d| d.contains("idx_tps_observation_series_ended")),
        "series 查询必须走新索引: {p:?}"
    );
    // 桶查询（全 series 键过滤）：?1..?8 = from/to/anchor/width/tool/model/channel/method
    let bucket_sql = speed_dao::bucket_query_sql();
    let p = plans(
        &bucket_sql,
        &[
            &0i64,
            &BASE_MS,
            &0i64,
            &300_000i64,
            &"kimi",
            &"kimi-k2",
            &"ch",
            &"client-native",
        ],
    );
    assert!(
        p.iter()
            .any(|d| d.contains("idx_tps_observation_series_ended")),
        "桶查询必须走新索引: {p:?}"
    );
    assert!(!p.iter().any(|d| d.contains("SCAN tps_observation")));
    // 纯范围扫描（无任何 series 过滤）：?1 from / ?2 to
    let range_sql = speed_dao::series_query_sql(false, false);
    let p = plans(&range_sql, &[&0i64, &BASE_MS]);
    assert!(
        !p.iter().any(|d| d.contains("SCAN tps_observation")),
        "纯范围查询也必须走 ended 索引: {p:?}"
    );
}

// ===========================================================================
// 15. derive_metrics 纯函数
// ===========================================================================

#[test]
fn derive_metrics_is_pure() {
    let sums = SpeedSums {
        stream_output_tokens: 550,
        stream_duration_ms: 6_000,
        stream_sample_count: 2,
        ttft_sum_ms: 950,
        ttft_sample_count: 3,
        candidate_count: 3,
        session_count: 2,
        latest_stream_ended_at: Some(BASE_MS),
        ..SpeedSums::default()
    };
    let first = derive_metrics(&sums);
    let second = derive_metrics(&sums);
    assert_eq!(first, second, "同输入必须同输出");
    assert_eq!(sums.stream_output_tokens, 550, "输入不被改动");
    assert!(
        (first.stream.value.unwrap() - 550.0 / 6.0).abs() < 1e-9,
        "derive 与 brief 算术一致"
    );
    assert_eq!((first.stream.sample_count, first.ttft_sample_count), (2, 3));
    // 零合计不冒充 0 速度
    let empty = derive_metrics(&SpeedSums::default());
    assert_eq!(empty.stream.value, None);
    assert_eq!(empty.response.value, None);
    assert_eq!(empty.avg_first_token_ms, None);
    assert_eq!(empty.model_ms, None);
    assert_eq!(empty.tool_ms, None);
    assert_eq!(empty.output_scope, OutputScope::Unknown);
    assert!(!empty.scope_mixed);
}

// ===========================================================================
// 16. 序列化形态（IPC 镜像逐字）
// ===========================================================================

#[test]
fn wire_forms_are_verbatim() {
    assert_eq!(
        serde_json::to_string(&SpeedMetricMode::Stream).unwrap(),
        "\"stream\""
    );
    assert_eq!(
        serde_json::to_string(&SpeedMetricMode::Response).unwrap(),
        "\"response\""
    );
    for (v, s) in [
        (SpeedRange::Last15m, "15m"),
        (SpeedRange::Last1h, "1h"),
        (SpeedRange::Last5h, "5h"),
        (SpeedRange::Last24h, "24h"),
    ] {
        assert_eq!(serde_json::to_string(&v).unwrap(), format!("\"{s}\""));
        let back: SpeedRange = serde_json::from_str(&format!("\"{s}\"")).unwrap();
        assert_eq!(back, v);
    }
    assert_eq!(serde_json::to_string(&SpeedSort::Rate).unwrap(), "\"rate\"");
    assert_eq!(serde_json::to_string(&SpeedSort::Tool).unwrap(), "\"tool\"");
    let status: SpeedSourceStatus = serde_json::from_str("\"needs-check\"").unwrap();
    assert_eq!(status, SpeedSourceStatus::NeedsCheck);
    assert_eq!(
        serde_json::to_string(&SpeedSourceStatus::AwaitRestart).unwrap(),
        "\"await-restart\""
    );
    assert_eq!(
        serde_json::to_string(&SpeedSourceStatus::Unsupported).unwrap(),
        "\"unsupported\""
    );
}
