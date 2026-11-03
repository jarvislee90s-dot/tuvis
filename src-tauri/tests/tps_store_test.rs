//! TPS 三表存储集成测试（计划 Task 2 / Spec §6 固定工程契约：tps_observation / tps_point /
//! tps_state + 24 小时保留）。
//!
//! 隔离纪律（2026-10-07 事故族）：全部用例走 `support::open_ledger_db(tag)` 的**用例私有库
//! 文件**（tempdir 下的 `.tuvis/tuvis.db`），绝不触碰真机 `~/.tuvis` / `~/.dsh`；私有库里的
//! 旧 usage_* 四表只由本文件的固定播种行构成，用来锁「TPS 写入与保留清理不碰旧账本」。
//!
//! 覆盖（brief Step 1 逐条）：
//! * 事实重复 UPSERT 不增行（`after_twice == before_second`）
//! * 点 (tick_id, series_key) 唯一，重存不重复
//! * gap 点落库并读回（不画 0）
//! * 24 小时清理：删旧留新并返回删除数
//! * 旧 usage_* 四桶/费用/游标行数在 TPS 全部写路径前后逐表相等（`old_usage_after == old_usage_before`）
//! * 批内中途失败 → 事实/游标/来源状态全部未落（同事务原子性）；恢复后只补事实，不补造过去的采样点
//! * `load_recent(since_ms)` 只返回 `ended_at > since_ms` 的事实
mod support;

use multi_agents_manager_lib::services::tps::model::{
    Outcome, OutputScope, PointStatus, Purpose, ReadBatch, SeriesKey, SourceCursor, SourceState,
    TimingMethod, TpsObservation, TpsPoint,
};
use multi_agents_manager_lib::services::tps::{build_point, store};
use rusqlite::Connection;

/// 固定假时钟基准（纯整数，不读系统时钟）
const BASE_MS: i64 = 1_700_000_000_000;
/// 24 小时（字面量入测：锁住「24 小时」这个数字本身，不间接依赖实现常量）
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// 用例私有库：tempdir 下的真实 DB 文件（schema::init 建齐含 TPS 三表在内的全部表）
fn conn() -> Connection {
    support::open_ledger_db("tps-store")
}

/// 绝对行数断言（表名只在测试内拼接；表缺失时响亮 panic，不静默当 0）
fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap_or_else(|e| panic!("计数查询失败（{table}，表缺失？）：{e}"))
}

/// 有效观察夹具：Success / MainTask / client-native / kimi（与 policy.rs 测试同形）
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

fn series_kimi() -> SeriesKey {
    SeriesKey {
        tool: "kimi".into(),
        model: "kimi-k2".into(),
        channel: None,
        method: TimingMethod::ClientNative,
    }
}

/// ReadBatch 夹具：游标推进 off-1 → off-2，来源状态 ok（Spec §6 ReadBatch 四字段）
fn batch(observations: Vec<TpsObservation>) -> ReadBatch {
    ReadBatch {
        observations,
        source_state: SourceState {
            source: "kimi".into(),
            cursor: SourceCursor {
                source: "kimi".into(),
                value: "off-1".into(),
            },
            ok: true,
            detail: None,
        },
        diagnostics: vec![],
        next_cursor: SourceCursor {
            source: "kimi".into(),
            value: "off-2".into(),
        },
    }
}

/// 一个 value 点（该时刻窗内恰有一条有效调用）
fn kimi_value_points(run: &str, at: i64) -> Vec<TpsPoint> {
    build_point(&[obs("call-1", at, 1000, 100)], &[], run, at)
}

/// 一个 gap 点（known_series 在场、窗内无观察 → 空窗 gap）
fn kimi_gap_points(run: &str, at: i64) -> Vec<TpsPoint> {
    build_point(&[], &[series_kimi()], run, at)
}

/// 播种旧 usage 四表各一行（列清单与 database/migration.rs 既有测试同款，满足全部 NOT NULL）
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

// ===== brief Step 1 红测试清单 =====

/// 事实重复 UPSERT 不增行（`after_twice == before_second`）；游标推进与事实同一事务可见
#[test]
fn commit_batch_upserts_facts_idempotently() {
    let mut c = conn();
    store::commit_batch(&mut c, &batch(vec![obs("a", BASE_MS, 1000, 10)])).unwrap();
    let before_second = count(&c, "tps_observation");
    assert_eq!(before_second, 1);
    // 同一调用（同主键三元组）重读重写：行数不得增长
    let mut second = batch(vec![obs("a", BASE_MS, 1000, 10)]);
    second.next_cursor.value = "off-3".into();
    store::commit_batch(&mut c, &second).unwrap();
    let after_twice = count(&c, "tps_observation");
    assert_eq!(
        after_twice, before_second,
        "事实 UPSERT：同主键重写不得增行"
    );
    assert_eq!(
        store::load_cursor(&c, "kimi").unwrap().unwrap().value,
        "off-3",
        "游标推进必须与事实同事务提交（Spec §6：事实与 nextCursor 同事务提交后才推进）"
    );
}

/// 点 (tick_id, series_key) 唯一：同点重存 UPSERT 不重复；不同 tick 各自成行
#[test]
fn points_resave_same_tick_and_series_do_not_duplicate() {
    let mut c = conn();
    let pts = kimi_value_points("run-1", BASE_MS);
    store::save_points(&mut c, &pts).unwrap();
    let before_second = count(&c, "tps_point");
    assert_eq!(before_second, 1);
    store::save_points(&mut c, &pts).unwrap();
    assert_eq!(
        count(&c, "tps_point"),
        before_second,
        "同 (tick_id, series_key) 重存必须 UPSERT，不得重复"
    );
    // 新采样时刻 → 新 tick → 另一行（同一调用允许进入不同点，Spec §6）
    store::save_points(&mut c, &kimi_value_points("run-1", BASE_MS + 60_000)).unwrap();
    assert_eq!(count(&c, "tps_point"), 2);
}

/// gap 点必须落库并读回：status=gap、tps=None（不画 0）、sampled_keys 空、series 原样
#[test]
fn gap_points_survive_storage_round_trip() {
    let mut c = conn();
    let pts = kimi_gap_points("run-1", BASE_MS);
    assert_eq!(
        pts[0].status,
        PointStatus::Gap,
        "前提：夹具产出的就是 gap 点"
    );
    store::save_points(&mut c, &pts).unwrap();
    let loaded = store::load_points_since(&c, 0).unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].status, PointStatus::Gap);
    assert!(loaded[0].tps.is_none(), "gap 不画 0");
    assert_eq!(loaded[0].sample_count, 0);
    assert!(loaded[0].sampled_keys.is_empty());
    assert_eq!(loaded[0].series_key, series_kimi(), "series 键原样往返");
}

/// load_recent(since_ms) 只返回 ended_at > since_ms 的事实（严格大于，边界不返回）
#[test]
fn load_recent_returns_only_observations_ended_after_since() {
    let mut c = conn();
    store::commit_batch(
        &mut c,
        &batch(vec![
            obs("newer", BASE_MS - 1000, 500, 10),
            obs("older", BASE_MS - 2000, 500, 10),
        ]),
    )
    .unwrap();
    let got = store::load_recent(&c, BASE_MS - 1500).unwrap();
    assert_eq!(got.len(), 1, "只返回 ended_at 严格大于 since 的事实");
    assert_eq!(got[0].stable_call_key, "newer");
    assert!(
        store::load_recent(&c, BASE_MS - 1000).unwrap().is_empty(),
        "恰好等于 since 不返回（严格大于）"
    );
}

/// 24 小时清理：按事实 ended_at / 点 sampled_at 删旧留新并返回删除数；
/// 旧 usage_* 的保留（含超期明细行）不受影响
#[test]
fn purge_deletes_only_rows_older_than_24h() {
    let mut c = conn();
    seed_old_usage(&c);
    // 事实：一条 25h 前（该删）、一条 1h 前（该留）
    store::commit_batch(
        &mut c,
        &batch(vec![
            obs("old", BASE_MS - DAY_MS - 3_600_000, 1000, 10),
            obs("fresh", BASE_MS - 3_600_000, 1000, 10),
        ]),
    )
    .unwrap();
    // 点：一条 25h 前、一条 1h 前（run 不同 → tick 不同 → 两行）
    store::save_points(
        &mut c,
        &kimi_value_points("run-old", BASE_MS - DAY_MS - 3_600_000),
    )
    .unwrap();
    store::save_points(&mut c, &kimi_value_points("run-new", BASE_MS - 3_600_000)).unwrap();
    assert_eq!(count(&c, "tps_observation"), 2);
    assert_eq!(count(&c, "tps_point"), 2);

    let deleted = store::purge(&c, BASE_MS).unwrap();
    assert_eq!(deleted, 2, "恰好删除两条超 24h 的行（事实+点各一）");
    assert_eq!(count(&c, "tps_observation"), 1, "新鲜事实保留");
    assert_eq!(count(&c, "tps_point"), 1, "新鲜点保留");
    let recent = store::load_recent(&c, 0).unwrap();
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].stable_call_key, "fresh");

    // 旧账本四表连播种行带超期明细一并原样（「不变更旧用量保留设置」，Spec §2）
    for t in [
        "usage_detail",
        "usage_daily",
        "usage_cursor",
        "usage_session",
    ] {
        assert_eq!(count(&c, t), 1, "purge 不得触碰旧表 {t}");
    }
}

/// 旧 usage_* 四桶/费用/游标在 TPS 全部写路径（commit_batch / save_points / save_state /
/// purge）前后逐表行数相等（`old_usage_after == old_usage_before`）
#[test]
fn tps_writes_leave_old_usage_tables_untouched() {
    let mut c = conn();
    seed_old_usage(&c);
    let tables = [
        "usage_detail",
        "usage_daily",
        "usage_cursor",
        "usage_session",
    ];
    let old_usage_before: Vec<i64> = tables.iter().map(|t| count(&c, t)).collect();

    store::commit_batch(&mut c, &batch(vec![obs("a", BASE_MS, 1000, 10)])).unwrap();
    store::save_points(&mut c, &kimi_value_points("run-1", BASE_MS)).unwrap();
    store::save_state(&c, "sched:meta", &serde_json::json!({ "tick": 1 })).unwrap();
    store::purge(&c, BASE_MS + 2 * DAY_MS).unwrap();

    let old_usage_after: Vec<i64> = tables.iter().map(|t| count(&c, t)).collect();
    assert_eq!(
        old_usage_after, old_usage_before,
        "TPS 写入不得改动旧账本任何表"
    );
}

/// 批内中途失败 → 事实/游标/来源状态全部未落（同事务原子性）；
/// 恢复后只补事实，不补造过去缺失的采样点（补点是调度器职责，本用例锁存储面）
#[test]
fn commit_batch_is_atomic_on_mid_batch_failure() {
    let mut c = conn();
    let mut b = batch(vec![obs("good-1", BASE_MS, 1000, 10)]);
    // ended_at_ms 的 epoch 非负 CHECK 是批中段失败的确定性注入缝（首行合法、次行必炸）
    let mut bad = obs("bad", BASE_MS, 1000, 10);
    bad.ended_at_ms = -1;
    b.observations.push(bad);

    assert!(store::commit_batch(&mut c, &b).is_err(), "批内失败必须冒泡");
    assert_eq!(
        count(&c, "tps_observation"),
        0,
        "原子性：合法首行也不得残留"
    );
    assert!(
        store::load_state(&c, &store::cursor_state_key("kimi"))
            .unwrap()
            .is_none(),
        "原子性：失败批的水位不得推进"
    );
    assert!(
        store::load_state(&c, &store::source_state_key("kimi"))
            .unwrap()
            .is_none(),
        "原子性：失败批的来源状态不得落库"
    );

    // 恢复：修正后的批重新提交 → 事实补上；采样点不补造（恢复后只从当前时间产新点，Spec §2）
    store::commit_batch(&mut c, &batch(vec![obs("good-1", BASE_MS, 1000, 10)])).unwrap();
    assert_eq!(count(&c, "tps_observation"), 1);
    assert_eq!(
        count(&c, "tps_point"),
        0,
        "恢复只补事实，不生成过去缺失的采样点"
    );
}

/// 单事务 ≤200 行硬上限（Spec §6「单批最多200条」）：超限整批拒绝，不留半写
#[test]
fn oversized_batches_are_rejected_without_partial_write() {
    let mut c = conn();
    let obs_list: Vec<TpsObservation> = (0..201)
        .map(|i| obs(&format!("c{i:03}"), BASE_MS - i as i64, 1000, 1))
        .collect();
    assert!(
        store::commit_batch(&mut c, &batch(obs_list)).is_err(),
        "201 条事实超 200 上限，必须拒绝"
    );
    assert_eq!(count(&c, "tps_observation"), 0, "超限批不得半写");
    assert!(
        store::load_state(&c, &store::cursor_state_key("kimi"))
            .unwrap()
            .is_none(),
        "超限批的水位也不得推进"
    );
    let pts: Vec<TpsPoint> = (0..201)
        .flat_map(|i| kimi_value_points(&format!("r{i}"), BASE_MS - i as i64))
        .collect();
    assert!(
        store::save_points(&mut c, &pts).is_err(),
        "点批同受 200 上限"
    );
    assert_eq!(count(&c, "tps_point"), 0);
}

/// 列映射全量守恒：观察（全 Some / 全 None 两种形态）与点（value + gap）逐字段往返
#[test]
fn observation_and_point_round_trip_all_fields() {
    let mut c = conn();
    // 全 Some 形态（含可选渠道/工具用时）
    let mut full = obs("full", BASE_MS, 4000, 300);
    full.tool_ms = Some(250);
    full.channel = Some("ch-1".into());
    // 全 None 形态（失败调用：缺计时为缺失，不填 0——失败也落事实表，耗时详情口径需要）
    let mut none = obs("none", BASE_MS - 1, 0, 0);
    none.outcome = Outcome::Failed;
    none.failure_reason = Some("boom".into());
    none.first_token_at_ms = None;
    none.native_duration_ms = None;
    none.generation_ms = None;
    none.output_tokens = None;
    none.tool_ms = None;
    none.session_key = None;
    none.turn_key = None;
    none.channel = None;
    store::commit_batch(&mut c, &batch(vec![full.clone(), none.clone()])).unwrap();
    let got = store::load_recent(&c, 0).unwrap();
    assert_eq!(got.len(), 2);
    assert!(got.contains(&full), "全 Some 观察逐字段守恒");
    assert!(
        got.contains(&none),
        "全 None 观察逐字段守恒（None 不许变 0/串值）"
    );

    // 点：policy 真实产物 value 点整点相等（age_ms 由 sampled_at 确定性重算，应逐字段一致）
    let pts = kimi_value_points("run-1", BASE_MS);
    let want = pts[0].clone();
    store::save_points(&mut c, &pts).unwrap();
    let loaded = store::load_points_since(&c, 0).unwrap();
    assert_eq!(loaded, vec![want], "点整点往返守恒");
}

/// tps_state 的 key/JSON 通道：commit_batch 空批也必须落游标与来源状态
/// （Spec §6「sourceState 始终返回，即使无数据」）；通用键供安装记录/调度元数据使用
#[test]
fn state_json_round_trips_per_key() {
    let mut c = conn();
    store::commit_batch(&mut c, &batch(vec![])).unwrap();
    let cur = store::load_cursor(&c, "kimi")
        .unwrap()
        .expect("空批也必须落游标");
    assert_eq!(cur.value, "off-2");
    let st = store::load_source_state(&c, "kimi")
        .unwrap()
        .expect("空批也必须落来源状态");
    assert!(st.ok);
    assert_eq!(st.source, "kimi");

    store::save_state(&c, "install:kimi", &serde_json::json!({ "fp": "a1" })).unwrap();
    assert_eq!(
        store::load_state(&c, "install:kimi").unwrap(),
        Some(serde_json::json!({ "fp": "a1" })),
        "通用 key/JSON 必须原样往返"
    );
    assert!(
        store::load_state(&c, "no-such-key").unwrap().is_none(),
        "缺键返回 None，不猜"
    );
}
