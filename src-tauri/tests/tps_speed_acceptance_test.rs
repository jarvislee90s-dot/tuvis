//! 整链验收（2026-10-26 桌面工作区与输出速度 V2 计划 Task 13 / Spec §11 验收表）。
//!
//! ## 被测整链
//!
//! 「真实读取器 → commit_batch 同事务落库 → V2 只读 dashboard 查询」的**接线整体**，
//! 四条整链反例（brief Step1 逐条）：
//!
//! 1. **WorkBuddy 旧 EOF 恢复整链**：v1 游标（旧 EOF 形态）压在 17 行字符串数值夹具上
//!    ⇒ 恢复发事实 ⇒ V2 dashboard 同一 series 双指标（区间聚合与各桶聚合都 =
//!    1000×Σ输出/Σ毫秒）；wb-known-6711 单记录链精确锚定 ≈63.85 TPS / ≈50.51 Token/s；
//!    重放幂等（UPSERT 不翻倍）；旧 usage 账本行与其他来源游标原样；
//! 2. **Codex 原生读取整链**：codex-normal 夹具过真实 `read_incremental` + `commit_batch`
//!    ⇒ dashboard 里 response 有值而 (stream.value, avg_first_token_ms) == (None, None)
//!    （TTFT 全域为 null，桶亦然）；复制历史重读 ⇒ 单行不翻倍；
//! 3. **混合污染**：成功 + 取消 + 重复身份同窗 ⇒ 两个速度只用有效子集；取消的可靠用时
//!    只进可靠累计用时与候选/取消计数，不入任何速度 n；重复身份只计一次；
//! 4. **旧 usage 独立**：整条 TPS 链前后 usage_* 各表逐行快照不变；
//!    `purge_before`（24h 截止）只删 tps_observation / tps_point 两表，usage_* 与
//!    tps_state（游标/来源状态）不动。
//!
//! ## 隔离纪律（2026-10-07 事故族，与 tps_workbuddy_v2_test / tps_codex_source_test 同款）
//!
//! 全部用例只触 tempdir 影子 home（读取器以入参 home 为唯一根）与
//! `support::open_ledger_db` 的用例私有库；绝不触真机 `~/.workbuddy` / `~/.codex` /
//! `~/.tuvis`，绝不起 Tauri、不发模型请求、不等待真实时钟（dashboard 的 asOf 显式注入）。

mod support;

use multi_agents_manager_lib::database::dao::tps as tps_dao;
use multi_agents_manager_lib::services::tps::model::{
    Outcome, OutputScope, PointStatus, PointTrigger, Purpose, ReadBatch, SeriesKey, SourceCursor,
    SourceState, TimingMethod, TpsObservation, TpsPoint, TpsQuality,
};
use multi_agents_manager_lib::services::tps::sources::{codex, workbuddy};
use multi_agents_manager_lib::services::tps::store;
use multi_agents_manager_lib::services::tps::v2::model::{
    SpeedDashboardQuery, SpeedDashboardSnapshot, SpeedMetricMode, SpeedRange, SpeedSeriesSummary,
    SpeedSort,
};
use multi_agents_manager_lib::services::tps::v2::query;
use rusqlite::types::ValueRef;
use rusqlite::Connection;
use serde_json::json;
use std::path::{Path, PathBuf};

// ==== 常量（与既有夹具/测试同源）====

/// Codex 夹具时间锚点：2026-10-26T00:00:00.000Z
const ANCHOR_MS: i64 = 1_792_972_800_000;
/// 固定假时钟基准（混合污染 / 清理用例；不读系统时钟）
const BASE_MS: i64 = 1_700_000_000_000;
const MIN_MS: i64 = 60_000;

// ==== 影子 home / 夹具 / 私有库 ====

/// 17 条真实 qua.end（已脱敏，数值保持整数字符串形态——旧解析器整源跳过的根因）
fn wb_fixture_text() -> String {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tps-v2/workbuddy-qua.txt");
    std::fs::read_to_string(&path).expect("读取 workbuddy 夹具失败")
}

/// 夹具里 total=6711 / ttft=1402 / completion=339 的那条（message_id = wb-known-6711）
fn wb_known_line() -> String {
    wb_fixture_text()
        .lines()
        .find(|l| l.contains("\"wb-known-6711\""))
        .expect("夹具缺 wb-known-6711 行")
        .to_string()
}

/// WorkBuddy 影子 home + `.workbuddy/logs/2026-10-26/` 日期目录（tempdir 泄漏保活至用例结束）
fn wb_shadow_home(tag: &str, body: &str) -> (PathBuf, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join(tag);
    let log = home
        .join(".workbuddy")
        .join("logs")
        .join("2026-10-26")
        .join("cli_host__0__acceptance.log");
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    std::fs::write(&log, body).unwrap();
    std::mem::forget(temp); // 用例存活期间影子目录不得被清理（support.rs 同款）
    (home, log)
}

/// Codex 影子 home + `.codex/sessions/2026/10/26/`（tempdir 泄漏保活至用例结束）
fn codex_shadow_home(tag: &str) -> (PathBuf, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join(tag);
    let sessions = home.join(".codex").join("sessions").join("2026/10/26");
    std::fs::create_dir_all(&sessions).unwrap();
    std::mem::forget(temp);
    (home, sessions)
}

fn codex_fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(format!("tests/fixtures/tps-v2/codex-{name}.jsonl"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("读取夹具 codex-{name} 失败：{e}"))
}

/// 「现在」：24h 门只看文件 mtime（夹具复制后 = 测试时刻），事件时刻不设窗
fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// 每条用例独占一个私有账本库（tempdir 下的真实 DB 文件，schema::init 建齐全部表）
fn conn(tag: &str) -> Connection {
    support::open_ledger_db(tag)
}

fn count(c: &Connection, table: &str) -> i64 {
    c.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap_or_else(|e| panic!("计数查询失败（{table}，表缺失？）：{e}"))
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

// ==== 快照助手 ====

/// 旧 usage 账本快照：全部 `usage%` 表的**逐行内容**（表序/行序/列序稳定），用于
/// 「TPS 整链前后逐行不变」断言。表清单来自 sqlite_master（新增 usage 表自动纳入）。
fn usage_snapshot(c: &Connection) -> String {
    let names: Vec<String> = {
        let mut stmt = c
            .prepare(
                "SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'usage%' ORDER BY name",
            )
            .unwrap();
        stmt.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    let mut out = String::new();
    for name in &names {
        out.push_str(&format!("== {name}\n"));
        let mut stmt = c
            .prepare(&format!("SELECT * FROM \"{name}\" ORDER BY rowid"))
            .unwrap();
        let ncols = stmt.column_count();
        let mut rows = stmt.query([]).unwrap();
        while let Some(row) = rows.next().unwrap() {
            let mut cells = Vec::with_capacity(ncols);
            for i in 0..ncols {
                let cell = match row.get_ref(i).unwrap() {
                    ValueRef::Null => "∅".to_string(),
                    ValueRef::Integer(v) => format!("i{v}"),
                    ValueRef::Real(v) => format!("f{v:?}"),
                    ValueRef::Text(t) => format!("t{}", String::from_utf8_lossy(t)),
                    ValueRef::Blob(b) => format!("b{}", b.len()),
                };
                cells.push(cell);
            }
            out.push_str(&cells.join("\u{1}"));
            out.push('\n');
        }
    }
    out
}

/// 播种旧 usage 四表（与 tps_sampling_test::seed_old_usage 同形）
fn seed_old_usage(c: &Connection) {
    c.execute_batch(
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
    .expect("播种旧 usage 表失败");
}

/// 播种一行 WorkBuddy 旧 Token/费用游标（恢复前后逐列比对——只恢复 TPS，不动旧账本）
fn seed_old_usage_cursor(c: &Connection) {
    c.execute_batch(
        "INSERT INTO usage_cursor
             (source_id, session_id, fingerprint, byte_offset, ordinal, last_cumulative,
              mtime_ms, file_size, state_json, updated_at)
             VALUES ('workbuddy','s1','fp',42,3,99,123,456,'{}',789);",
    )
    .expect("播种旧 usage_cursor 失败");
}

/// 旧 usage_cursor 行的逐列内容（source_id, byte_offset, ordinal, last_cumulative, mtime_ms, updated_at）
fn usage_cursor_row(c: &Connection) -> (String, i64, i64, i64, i64, i64) {
    c.query_row(
        "SELECT source_id, byte_offset, ordinal, last_cumulative, mtime_ms, updated_at
         FROM usage_cursor WHERE source_id='workbuddy' AND session_id='s1'",
        [],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        },
    )
    .expect("读旧 usage_cursor 行失败")
}

/// 其他来源游标（tps_state 的 `cursor:<source>` 键）原始文本
fn raw_state(c: &Connection, key: &str) -> Option<String> {
    c.query_row("SELECT value FROM tps_state WHERE key = ?1", [key], |r| {
        r.get::<_, String>(0)
    })
    .ok()
}

/// 旧解析器写下的 v1 游标形态：只有 files 字节界（无 format_version → 反序列化为 v1）
fn wb_v1_eof_cursor(log: &Path) -> SourceCursor {
    let size = std::fs::metadata(log).unwrap().len();
    SourceCursor {
        source: "workbuddy".into(),
        value: json!({
            "files": { log.to_string_lossy(): { "offset": size, "skip_lines": 0 } }
        })
        .to_string(),
    }
}

// ==== dashboard 查询助手 ====

fn speed_query(range: SpeedRange) -> SpeedDashboardQuery {
    SpeedDashboardQuery {
        range,
        selection: None,
        metric_mode: SpeedMetricMode::Stream,
        tool_id: None,
        search: String::new(),
        sort: SpeedSort::Tool,
    }
}

fn find_series<'a>(snap: &'a SpeedDashboardSnapshot, tool: &str) -> &'a SpeedSeriesSummary {
    snap.series
        .iter()
        .find(|s| s.series_key.tool == tool)
        .unwrap_or_else(|| {
            panic!(
                "缺 {tool} 的 series；实有 {:?}",
                snap.series
                    .iter()
                    .map(|s| s.series_key.tool.clone())
                    .collect::<Vec<_>>()
            )
        })
}

/// 一次直写观察批（绕过采集；走真实 commit_batch 事务路径）
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

/// 采样点夹具（清理作用域断言用）
fn point_at(series: SeriesKey, at: i64) -> TpsPoint {
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
        point_id: format!("{at}:point"),
        series_key: series,
        sampled_at: at,
        window_ms: 120_000,
        run_id: "acceptance".into(),
        trigger: PointTrigger::Scheduled,
        status: PointStatus::Value,
        cadence_ms: Some(600_000),
        sampled_keys: vec![],
    }
}

fn kimi_series() -> SeriesKey {
    SeriesKey {
        tool: "kimi".into(),
        model: "kimi-k2".into(),
        channel: None,
        method: TimingMethod::ClientNative,
    }
}

// ===========================================================================
// 1. WorkBuddy 旧 EOF 游标 → 恢复 → V2 dashboard 双指标整链 + 重放幂等
// ===========================================================================

#[test]
fn workbuddy_v1_eof_recovery_whole_chain_dual_metrics_and_idempotent_replay() {
    let (home, log) = wb_shadow_home("acc-wb-full", &wb_fixture_text());
    let mut c = conn("acc-wb-full");
    set_usage_settings(&c, true);
    seed_old_usage_cursor(&c);
    // 他源游标（dsh）在案：恢复绝不创建/改写其他来源的水位
    store::save_state(
        &c,
        "cursor:dsh",
        &json!({"source": "dsh", "value": "{\"keep\":1}"}),
    )
    .unwrap();
    let dsh_before = raw_state(&c, "cursor:dsh");
    let usage_row_before = usage_cursor_row(&c);

    // ① v1 EOF 游标恢复：17 条事实全部发出
    let old_cursor = wb_v1_eof_cursor(&log);
    let recovered = workbuddy::read_incremental(home.as_path(), &old_cursor, now_ms()).unwrap();
    assert_eq!(recovered.observations.len(), 17, "恢复批恰 17 条");
    store::commit_batch(&mut c, &recovered).unwrap();
    assert_eq!(count(&c, "tps_observation"), 17);

    // ② V2 dashboard：同一 series 双指标（区间聚合 = 1000×Σ输出/Σ毫秒，由恢复批在测试内复算）
    let to = recovered
        .observations
        .iter()
        .map(|o| o.ended_at_ms)
        .max()
        .unwrap()
        + MIN_MS;
    let snap = query::dashboard(&c, &speed_query(SpeedRange::Last1h), to).unwrap();
    assert!(!snap.disabled);
    assert_eq!(snap.series.len(), 1, "17 条同 model/method 合一 series");
    let m = &find_series(&snap, "workbuddy").metrics;
    let sum_out: i64 = recovered
        .observations
        .iter()
        .map(|o| o.output_tokens.unwrap())
        .sum();
    let sum_gen: i64 = recovered
        .observations
        .iter()
        .map(|o| o.generation_ms.unwrap())
        .sum();
    let sum_resp: i64 = recovered
        .observations
        .iter()
        .map(|o| o.response_sample_ms().unwrap())
        .sum();
    assert_eq!(m.stream.sample_count, 17);
    assert_eq!(m.response.sample_count, 17, "字符串兼容后两条速度都有");
    assert!((m.stream.value.unwrap() - 1000.0 * sum_out as f64 / sum_gen as f64).abs() < 1e-9);
    assert!((m.response.value.unwrap() - 1000.0 * sum_out as f64 / sum_resp as f64).abs() < 1e-9);
    // TTFT 独立池：17 条全部有可对齐首点（qua.end 的 ttft_ms）
    assert_eq!(m.ttft_sample_count, 17);
    assert!(m.avg_first_token_ms.is_some());

    // 桶聚合逐桶对账：有样本的桶 = 该桶内记录的 Σ/Σ（区间与桶同口径）
    let spans = multi_agents_manager_lib::services::tps::v2::query::bucket_spans(
        snap.range_from,
        snap.range_to,
        SpeedRange::Last1h,
    );
    assert_eq!(snap.buckets.len(), spans.len());
    let mut buckets_with_samples = 0;
    for (bucket, span) in snap.buckets.iter().zip(&spans) {
        let in_bucket: Vec<&TpsObservation> = recovered
            .observations
            .iter()
            .filter(|o| o.ended_at_ms > span.start_ms && o.ended_at_ms <= span.end_ms)
            .collect();
        assert_eq!(bucket.start_ms, span.start_ms);
        assert_eq!(bucket.end_ms, span.end_ms);
        if in_bucket.is_empty() {
            assert_eq!(bucket.metrics.stream.value, None, "空桶不推算速度");
            continue;
        }
        buckets_with_samples += 1;
        let b_out: i64 = in_bucket.iter().map(|o| o.output_tokens.unwrap()).sum();
        let b_gen: i64 = in_bucket.iter().map(|o| o.generation_ms.unwrap()).sum();
        let b_resp: i64 = in_bucket
            .iter()
            .map(|o| o.response_sample_ms().unwrap())
            .sum();
        assert_eq!(bucket.metrics.stream.sample_count, in_bucket.len() as i64);
        assert!(
            (bucket.metrics.stream.value.unwrap() - 1000.0 * b_out as f64 / b_gen as f64).abs()
                < 1e-9,
            "桶流式聚合 = 1000×Σ输出/Σ生成毫秒"
        );
        assert!(
            (bucket.metrics.response.value.unwrap() - 1000.0 * b_out as f64 / b_resp as f64).abs()
                < 1e-9,
            "桶响应聚合 = 1000×Σ输出/Σ响应毫秒"
        );
        assert_eq!(
            bucket.metrics.response.sample_count,
            in_bucket.len() as i64,
            "桶内响应口径独立但同集"
        );
    }
    assert!(
        buckets_with_samples >= 2,
        "夹具跨多个 5 分钟桶，桶聚合才可对账"
    );

    // ③ wb-known-6711 单记录整链：区间聚合 == 桶聚合 == 该记录自身
    //    输出 339 / total 6711 / ttft 1402 → 流式 ≈63.85 TPS、响应 ≈50.51 Token/s（Spec §9）
    let (home2, log2) = wb_shadow_home("acc-wb-known", &(wb_known_line() + "\n"));
    let mut c2 = conn("acc-wb-known");
    set_usage_settings(&c2, true);
    let recovered2 =
        workbuddy::read_incremental(home2.as_path(), &wb_v1_eof_cursor(&log2), now_ms()).unwrap();
    assert_eq!(recovered2.observations.len(), 1);
    store::commit_batch(&mut c2, &recovered2).unwrap();
    let to2 = recovered2.observations[0].ended_at_ms + MIN_MS;
    let snap2 = query::dashboard(&c2, &speed_query(SpeedRange::Last1h), to2).unwrap();
    let m2 = &find_series(&snap2, "workbuddy").metrics;
    assert!(
        (m2.stream.value.unwrap() - 63.85).abs() < 0.01,
        "流式 ≈63.85"
    );
    assert!(
        (m2.response.value.unwrap() - 50.51).abs() < 0.01,
        "响应 ≈50.51"
    );
    let bucket2 = snap2
        .buckets
        .iter()
        .find(|b| b.metrics.stream.sample_count > 0)
        .expect("单记录必落一个桶");
    assert_eq!(bucket2.metrics.stream.sample_count, 1);
    assert!(
        (bucket2.metrics.stream.value.unwrap() - 63.85).abs() < 0.01,
        "桶聚合同值"
    );
    assert!((bucket2.metrics.response.value.unwrap() - 50.51).abs() < 0.01);

    // ④ 重放幂等：同一 v1 游标再恢复一遍 → 同批事实 UPSERT → 行数与 dashboard n 不变
    let replay = workbuddy::read_incremental(home.as_path(), &old_cursor, now_ms()).unwrap();
    assert_eq!(replay.observations.len(), 17, "恢复可重入");
    store::commit_batch(&mut c, &replay).unwrap();
    assert_eq!(count(&c, "tps_observation"), 17, "UPSERT 不翻倍");
    let snap3 = query::dashboard(&c, &speed_query(SpeedRange::Last1h), to).unwrap();
    let m3 = &find_series(&snap3, "workbuddy").metrics;
    assert_eq!(m3.stream.sample_count, 17, "重放后 n 仍 17");

    // ⑤ 他源游标与旧 usage 账本逐列原样
    assert_eq!(raw_state(&c, "cursor:dsh"), dsh_before, "他源游标原样");
    assert!(
        store::load_cursor(&c, "dsh").unwrap().is_some(),
        "dsh 游标仍在案"
    );
    assert_eq!(
        usage_cursor_row(&c),
        usage_row_before,
        "旧 usage_cursor 行原样"
    );
    assert_eq!(count(&c, "usage_cursor"), 1);
}

// ===========================================================================
// 2. Codex 原生读取整链：仅 response、TTFT 全空、复制历史单行
// ===========================================================================

#[test]
fn codex_native_reader_response_only_in_dashboard_and_duplicate_pass_single_row() {
    let (home, sessions) = codex_shadow_home("acc-codex-chain");
    let rollout = sessions.join("rollout-normal.jsonl");
    std::fs::write(&rollout, codex_fixture("normal")).unwrap();

    let mut c = conn("acc-codex-chain");
    set_usage_settings(&c, true);

    // ① 真实读取器（read_incremental + commit_batch 整链）
    let batch = codex::read_incremental(home.as_path(), &SourceCursor::empty("codex"), now_ms())
        .expect("read_incremental 不应整源失败");
    assert_eq!(batch.observations.len(), 1, "normal 夹具恰一发一观察");
    store::commit_batch(&mut c, &batch).unwrap();
    assert_eq!(count(&c, "tps_observation"), 1);

    // ② dashboard：response 有值，stream 与 TTFT 全域为 null（brief 断言逐字）
    let to = ANCHOR_MS + MIN_MS; // 覆盖夹具唯一观察的完成时刻（ANCHOR+5400）
    let snap = query::dashboard(&c, &speed_query(SpeedRange::Last1h), to).unwrap();
    assert_eq!(snap.series.len(), 1);
    let codex = &find_series(&snap, "codex").metrics;
    assert!(codex.response.value.is_some());
    assert_eq!((codex.stream.value, codex.avg_first_token_ms), (None, None));
    // 数值锚定：输出 100 / 响应窗 5000ms → 20.0 Token/s（1000×Σ/Σ）
    assert!(
        (codex.response.value.unwrap() - 20.0).abs() < 1e-9,
        "response = 1000×100/5000 = 20.0"
    );
    assert_eq!(codex.response.sample_count, 1);
    assert_eq!(codex.stream.sample_count, 0);
    assert_eq!(codex.ttft_sample_count, 0, "Codex 无逐响应 TTFT，不伪造");
    assert_eq!(
        codex.model_ms,
        Some(5_000),
        "Codex 累计用时 = 响应累计用时（日志近似）"
    );
    assert_eq!(
        find_series(&snap, "codex").series_key.method,
        TimingMethod::LogBoundaryApprox,
        "方法 = log-boundary-approx"
    );
    // TTFT 全域为 null：所有桶亦然
    assert!(!snap.buckets.is_empty());
    for b in &snap.buckets {
        assert_eq!(b.metrics.avg_first_token_ms, None);
        assert_eq!(b.metrics.ttft_sample_count, 0);
        assert_eq!(b.metrics.stream.value, None);
    }

    // ③ 复制历史去重：同 response_id 的第二个文件再读一遍 → DB UPSERT 单行
    let copy = sessions.join("rollout-copy.jsonl");
    std::fs::write(&copy, codex_fixture("normal")).unwrap();
    let batch2 =
        codex::read_incremental(home.as_path(), &batch.next_cursor, now_ms()).expect("第二轮读取");
    assert_eq!(
        batch2.observations.len(),
        1,
        "新文件解码器首见即发（跨文件去重是 DB 的职责）"
    );
    store::commit_batch(&mut c, &batch2).unwrap();
    assert_eq!(count(&c, "tps_observation"), 1, "复制历史不得翻倍");
    let snap2 = query::dashboard(&c, &speed_query(SpeedRange::Last1h), to).unwrap();
    let codex2 = &find_series(&snap2, "codex").metrics;
    assert_eq!(codex2.response.sample_count, 1, "dashboard 口径同样单行");
    assert!(
        (codex2.response.value.unwrap() - 20.0).abs() < 1e-9,
        "数值不变"
    );
    assert_eq!(codex2.stream.value, None);
}

// ===========================================================================
// 3. 混合污染：成功 + 取消 + 重复身份 → 速度只用有效子集
// ===========================================================================

#[test]
fn mixed_pollution_speeds_use_valid_subset_and_duplicates_count_once() {
    let mut c = conn("acc-mixed");
    set_usage_settings(&c, true);
    let to = BASE_MS;

    // ① 唯一有效成功样本：400 tokens / 4s → 100 TPS
    let mut ok = obs("ok-1", to - MIN_MS, 4_000, 400);
    ok.native_duration_ms = Some(4_000);
    // ② 取消：带可靠原生用时（可计时）→ 只进可靠累计用时与候选/取消计数，不入速度
    let mut cancelled = obs("cx-1", to - 2 * MIN_MS, 4_000, 400);
    cancelled.outcome = Outcome::Cancelled;
    cancelled.generation_ms = None;
    cancelled.first_token_at_ms = None;
    cancelled.native_duration_ms = Some(900);
    insert_obs(&mut c, &[ok.clone(), cancelled]);

    // ③ 重复身份：同 stable_call_key 再提交一次 → 单行（UPSERT 幂等）
    insert_obs(&mut c, std::slice::from_ref(&ok));

    let snap = query::dashboard(&c, &speed_query(SpeedRange::Last1h), to).unwrap();
    assert_eq!(snap.series.len(), 1);
    let m = &snap.series[0].metrics;
    // 速度只用有效子集：n=1（重复不翻倍、取消不入）
    assert_eq!(m.stream.sample_count, 1);
    assert_eq!(m.response.sample_count, 1);
    assert!(
        (m.stream.value.unwrap() - 100.0).abs() < 1e-9,
        "400/4s = 100 TPS"
    );
    assert_eq!(m.candidate_count, 2, "成功 + 取消 = 2 候选（重复只计一次）");
    assert_eq!(m.cancelled_count, 1);
    assert_eq!(m.failed_count, 0);
    // 取消的可靠用时只进可靠累计用时池：4000（成功）+ 900（取消）
    assert_eq!(m.model_ms, Some(4_900), "可计时取消只膨胀累计用时");
    assert_eq!(m.ttft_sample_count, 1, "TTFT 独立池只含成功样本");
}

// ===========================================================================
// 4. 旧 usage 独立：整链前后逐行不变；purge_before 只删 tps 两表
// ===========================================================================

#[test]
fn old_usage_ledger_untouched_by_whole_chain_and_purge_scoped_to_tps() {
    let mut c = conn("acc-usage-isolation");
    set_usage_settings(&c, true);
    seed_old_usage(&c);
    let old_usage_before = usage_snapshot(&c);
    assert!(
        old_usage_before.contains("== usage_cursor"),
        "快照必须覆盖 usage 表"
    );

    // 整条 TPS 链跑在同一个私有库上：
    // ① WorkBuddy v1 恢复 + 提交
    let (wb_home, wb_log) = wb_shadow_home("acc-usage-wb", &wb_fixture_text());
    let wb_batch =
        workbuddy::read_incremental(wb_home.as_path(), &wb_v1_eof_cursor(&wb_log), now_ms())
            .unwrap();
    assert_eq!(wb_batch.observations.len(), 17);
    store::commit_batch(&mut c, &wb_batch).unwrap();
    // ② Codex 真实读取 + 提交
    let (cx_home, cx_sessions) = codex_shadow_home("acc-usage-codex");
    std::fs::write(
        cx_sessions.join("rollout-normal.jsonl"),
        codex_fixture("normal"),
    )
    .unwrap();
    let cx_batch =
        codex::read_incremental(cx_home.as_path(), &SourceCursor::empty("codex"), now_ms())
            .unwrap();
    store::commit_batch(&mut c, &cx_batch).unwrap();
    // ③ V2 dashboard 只读查询（两档位）+ 采样点播种
    let to = ANCHOR_MS + MIN_MS;
    let _ = query::dashboard(&c, &speed_query(SpeedRange::Last1h), to).unwrap();
    let _ = query::dashboard(&c, &speed_query(SpeedRange::Last24h), to).unwrap();
    store::save_points(&mut c, &[point_at(kimi_series(), to)]).unwrap();

    let old_usage_after = usage_snapshot(&c);
    assert_eq!(old_usage_after, old_usage_before);

    // ④ purge_before（24h 截止）作用域：只删 tps_observation / tps_point，usage 与 tps_state 不动
    //    播种超窗 tps 行（ended_at/sampled_at = cutoff−1000）与窗内行
    let cutoff = to - store::TPS_RETENTION_MS;
    let stale = obs("stale-obs", cutoff - 1_000, 1_000, 100);
    let fresh = obs("fresh-obs", to - MIN_MS, 1_000, 100);
    store::commit_batch(&mut c, &single_batch(&stale)).unwrap();
    store::commit_batch(&mut c, &single_batch(&fresh)).unwrap();
    store::save_points(
        &mut c,
        &[
            point_at(kimi_series(), cutoff - 1_000),
            point_at(kimi_series(), to),
        ],
    )
    .unwrap();
    assert_eq!(count(&c, "tps_observation"), 20, "17 WB + 1 Codex + 2 kimi");
    // 点：早前播种的 (to) 点与后播的 (to) 点同 (point_id, series) → UPSERT 单行，共 2 行
    assert_eq!(count(&c, "tps_point"), 2);

    let removed = store::purge(&c, to).unwrap();
    assert_eq!(removed, 2, "恰删 1 观察行 + 1 点行");
    assert_eq!(count(&c, "tps_observation"), 19, "超窗观察删除、窗内保留");
    assert_eq!(count(&c, "tps_point"), 1, "超窗点删除、窗内保留");
    assert!(
        store::load_cursor(&c, "workbuddy").unwrap().is_some(),
        "游标（tps_state）不随清理删除"
    );
    assert!(store::load_source_state(&c, "codex").unwrap().is_some());
    // usage_* 在清理后仍逐行不变
    let old_usage_after_purge = usage_snapshot(&c);
    assert_eq!(old_usage_after_purge, old_usage_before);
    // dao 直呼同缝（brief 的 purge_before 锚点）：cutoff 严格半开界，无行可删时返回 0
    let removed2 = tps_dao::purge_before_conn(&c, cutoff).unwrap();
    assert_eq!(removed2, 0);
}

/// 单行观察批便捷构造（清理作用域用例）
fn single_batch(o: &TpsObservation) -> ReadBatch {
    ReadBatch {
        observations: vec![o.clone()],
        source_state: SourceState {
            source: o.source.clone(),
            cursor: SourceCursor::empty(&o.source),
            ok: true,
            detail: None,
        },
        diagnostics: vec![],
        next_cursor: SourceCursor::empty(&o.source),
    }
}
