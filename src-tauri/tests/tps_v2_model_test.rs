//! 桌面四页导航与输出速度 V2 · Task 1 红测试：响应事实、旧库迁移与旧数据兼容。
//!
//! 覆盖（brief Step 1 逐条）：
//! * 100 输出 / 生成 4000ms / 响应 5000ms → 流式 25 tok/s、响应 20 Token/s（测试内由
//!   记录字段现算两个派生数；区间/桶聚合归 Task 6，本任务只锁事实与谓词）
//! * 旧正流式 + native 5000 可兼容（无显式响应窗的旧记录）
//! * 旧 retry 无生成窗：即使 native > 0 也不入响应（旧流式规则已拒绝的记录不得被救回）
//! * 负值 / 0 / 比生成窗短 / 失败 / 取消不能入速度
//! * 旧表补列前行值与旧 usage 快照保留；迁移重跑幂等；批内中途失败表不变
//! * serde 旧记录缺键默认 null、序列化显式 null；UPSERT 重读保留新字段
//! * `log-boundary-approx` 枚举串（serde + DAO 库串）逐字对齐
//!
//! 隔离纪律（2026-10-07 事故族）：迁移/往返用例全部走 tempdir 私有库文件，
//! 绝不触碰 `~/.tuvis` / `~/.codex` / `~/.claude`。

mod support;

use multi_agents_manager_lib::database::{migration, schema};
use multi_agents_manager_lib::services::tps::model::{
    Outcome, OutputScope, Purpose, ReadBatch, SourceCursor, SourceState, TimingMethod,
    TpsObservation,
};
use multi_agents_manager_lib::services::tps::store;
use rusqlite::Connection;

/// 固定假时钟基准（纯整数，不读系统时钟）
const BASE_MS: i64 = 1_700_000_000_000;

/// 基础观察夹具：Success / MainTask / client-native / kimi，生成 4000ms、
/// 原生完整时长 5000ms、输出 100 token（与 store/sampling/policy 夹具同形；
/// `response_elapsed_ms` 缺省 None = 无显式响应窗的旧记录形态，用例按需覆写）。
fn obs(key: &str) -> TpsObservation {
    TpsObservation {
        source: "kimi".into(),
        host_instance: "host-1".into(),
        stable_call_key: key.into(),
        started_at_ms: BASE_MS - 5_000,
        first_token_at_ms: Some(BASE_MS - 4_000),
        ended_at_ms: BASE_MS,
        native_duration_ms: Some(5_000),
        generation_ms: Some(4_000),
        response_elapsed_ms: None,
        output_tokens: Some(100),
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

/// brief Step 1 主反例：响应口径数值、旧记录兼容路径与不可救回路径。
#[test]
fn response_metric_and_legacy_migration() {
    // ==== ① 数值例：100 输出 / 生成 4000ms / 响应 5000ms → 流式 25、响应 20 ====
    let mut sample = obs("sample");
    sample.response_elapsed_ms = Some(5_000);
    assert_eq!(sample.response_sample_ms(), Some(5000));
    assert!(sample.is_tps_valid(), "旧流式谓词不受新字段影响");
    assert!(sample.is_response_valid());
    // 两个派生数在测试内由记录字段现算（聚合归 Task 6）
    let output = sample.output_tokens.unwrap() as f64;
    let stream = 1000.0 * output / sample.generation_ms.unwrap() as f64;
    let response = 1000.0 * output / sample.response_sample_ms().unwrap() as f64;
    assert_eq!(stream, 25.0, "流式 = 1000×输出/生成窗");
    assert_eq!(response, 20.0, "响应 = 1000×输出/含等待响应窗");

    // ==== ② 旧正流式 + native 5000 可兼容（无显式响应窗）====
    let legacy = obs("legacy"); // response=None, native=5000, gen=4000, Success, 100 tok
    assert_eq!(
        legacy.response_sample_ms(),
        Some(5000),
        "旧记录以原生完整时长兼容响应口径"
    );
    assert!(legacy.is_response_valid());

    // ==== ③ 旧 retry 无生成窗：即使 native > 0 也不入响应 ====
    let mut retry_without_generation = obs("retry");
    retry_without_generation.generation_ms = None;
    retry_without_generation.first_token_at_ms = None; // retry 不可对齐，首点同样缺
    assert!(!retry_without_generation.is_response_valid());
    assert_eq!(
        retry_without_generation.response_sample_ms(),
        None,
        "旧流式规则已拒绝的记录（retry 无生成窗）不得被响应口径救回"
    );

    // ==== ④ 负 / 零 / 比生成窗短：不一致数据不修复，一律 None ====
    let mut negative = obs("negative");
    negative.response_elapsed_ms = Some(-1);
    assert_eq!(negative.response_sample_ms(), None, "负响应时长不入");
    assert!(!negative.is_response_valid());
    let mut zero = obs("zero");
    zero.response_elapsed_ms = Some(0);
    assert_eq!(zero.response_sample_ms(), None, "零响应时长不入");
    let mut shorter = obs("shorter");
    shorter.response_elapsed_ms = Some(3_999); // < 生成窗 4000，同基准不一致
    assert_eq!(
        shorter.response_sample_ms(),
        None,
        "显式响应窗短于同记录生成窗 = 不一致数据，绝不修复"
    );
    assert!(!shorter.is_response_valid());
    // 旧记录兼容路径的同基准检查：native 短于生成窗也不入
    let mut shrink = obs("shrink");
    shrink.native_duration_ms = Some(3_999);
    assert_eq!(
        shrink.response_sample_ms(),
        None,
        "旧记录 native 短于生成窗，同基准检查不通过"
    );

    // ==== ⑤ 失败 / 取消：显式响应窗在也入不了速度 ====
    let mut failed = obs("failed");
    failed.outcome = Outcome::Failed;
    failed.failure_reason = Some("boom".into());
    failed.response_elapsed_ms = Some(5_000);
    assert_eq!(
        failed.response_sample_ms(),
        Some(5000),
        "时长事实照实可取（失败有可靠起止仍进耗时详情）"
    );
    assert!(!failed.is_response_valid(), "失败不入速度");
    let mut cancelled = obs("cancelled");
    cancelled.outcome = Outcome::Cancelled;
    cancelled.failure_reason = Some("user-abort".into());
    cancelled.response_elapsed_ms = Some(5_000);
    assert!(!cancelled.is_response_valid(), "取消不入速度");

    // ==== ⑥ 显式响应窗 + 无生成窗（Codex 形态）：仅 response 可入 ====
    let mut codex_like = obs("codex-like");
    codex_like.generation_ms = None;
    codex_like.first_token_at_ms = None;
    codex_like.response_elapsed_ms = Some(5_000);
    assert!(!codex_like.is_tps_valid(), "无生成窗不入旧流式口径");
    assert_eq!(
        codex_like.response_sample_ms(),
        Some(5000),
        "显式正响应窗优先，生成窗缺失不阻拦（生成窗缺失 ≠ 不一致）"
    );
    assert!(codex_like.is_response_valid());
}

/// serde 兼容（旧记录缺键默认 null、序列化显式 null）+ DAO 往返（UPSERT 保留新字段、
/// `log-boundary-approx` 库串逐字往返）。
#[test]
fn response_field_serde_null_and_upsert_roundtrip() {
    // ==== serde：缺键 = 旧记录 → None；序列化恒显式 null（不省键）====
    let mut sample = obs("serde");
    sample.response_elapsed_ms = Some(5_000);
    let text = serde_json::to_string(&sample).unwrap();
    assert!(
        text.contains(r#""responseElapsedMs":5000"#),
        "IPC camelCase 逐字：{text}"
    );
    let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
    value
        .as_object_mut()
        .unwrap()
        .remove("responseElapsedMs")
        .expect("前提：序列化产物带 responseElapsedMs 键");
    let old_record: TpsObservation =
        serde_json::from_value(value).expect("旧记录（缺新键）必须可反序列化");
    assert_eq!(old_record.response_elapsed_ms, None, "旧记录缺键默认 null");
    let old_text = serde_json::to_string(&old_record).unwrap();
    assert!(
        old_text.contains(r#""responseElapsedMs":null"#),
        "None 序列化为显式 null，不省键：{old_text}"
    );

    // ==== 枚举串：kebab-case 逐字 ====
    assert_eq!(
        serde_json::to_string(&TimingMethod::LogBoundaryApprox).unwrap(),
        "\"log-boundary-approx\""
    );

    // ==== DAO 往返：新字段写入读回、UPSERT 保留、新枚举库串往返 ====
    let mut conn = support::open_ledger_db("tps-v2-roundtrip");
    store::commit_batch(&mut conn, &batch(vec![sample.clone()])).unwrap();
    let loaded = store::load_recent(&conn, 0).unwrap();
    assert_eq!(loaded, vec![sample], "新字段随事实整行往返");

    // 同主键重读更新鲜：换新方法 + 新响应时长 → UPSERT 整行覆盖（不翻倍不残留）
    let mut updated = obs("serde");
    updated.method = TimingMethod::LogBoundaryApprox;
    updated.response_elapsed_ms = Some(6_000);
    store::commit_batch(&mut conn, &batch(vec![updated.clone()])).unwrap();
    let loaded = store::load_recent(&conn, 0).unwrap();
    assert_eq!(loaded.len(), 1, "UPSERT 不增行");
    assert_eq!(
        loaded[0].response_elapsed_ms,
        Some(6_000),
        "UPSERT 保留新字段"
    );
    assert_eq!(
        loaded[0].method,
        TimingMethod::LogBoundaryApprox,
        "log-boundary-approx 库串逐字往返"
    );

    // 再写 None：读回 None（可空列直写直读，不填 0）
    let cleared = obs("serde");
    store::commit_batch(&mut conn, &batch(vec![cleared.clone()])).unwrap();
    let loaded = store::load_recent(&conn, 0).unwrap();
    assert_eq!(loaded, vec![cleared], "None 直写直读");
}

/// 旧库迁移：无新列的旧事实表经 migrate 幂等补列，旧行保留（新列 NULL）、
/// 旧 usage 快照逐字节不变；批内中途失败不落任何行。
#[test]
fn migration_adds_response_elapsed_ms_and_preserves_old_data() {
    let temp = tempfile::tempdir().unwrap();
    let mut conn = Connection::open(temp.path().join("legacy.db")).unwrap();

    // 上一版形状的事实表（无 response_elapsed_ms，列集逐字同旧 schema.rs）
    conn.execute_batch(
        "CREATE TABLE tps_observation (
            source             TEXT NOT NULL,
            host_instance      TEXT NOT NULL,
            stable_call_key    TEXT NOT NULL,
            started_at_ms      INTEGER NOT NULL CHECK(started_at_ms >= 0),
            first_token_at_ms  INTEGER,
            ended_at_ms        INTEGER NOT NULL CHECK(ended_at_ms >= 0),
            native_duration_ms INTEGER,
            generation_ms      INTEGER,
            output_tokens      INTEGER,
            output_scope       TEXT NOT NULL,
            tool_ms            INTEGER,
            model              TEXT NOT NULL,
            channel            TEXT,
            session_key        TEXT,
            turn_key           TEXT,
            purpose            TEXT NOT NULL,
            outcome            TEXT NOT NULL,
            method             TEXT NOT NULL,
            failure_reason     TEXT,
            PRIMARY KEY (source, host_instance, stable_call_key)
        );
        INSERT INTO tps_observation
         (source, host_instance, stable_call_key, started_at_ms, first_token_at_ms,
          ended_at_ms, native_duration_ms, generation_ms, output_tokens, output_scope,
          tool_ms, model, channel, session_key, turn_key, purpose, outcome, method,
          failure_reason)
        VALUES
         ('kimi','host-1','legacy-1',100,200,300,300,200,100,'exclusiveOutput',NULL,
          'kimi-k2',NULL,'s1','t1','mainTask','success','client-native',NULL),
         ('kimi','host-1','legacy-2',400,NULL,500,NULL,NULL,NULL,'unknown',NULL,
          'm',NULL,NULL,NULL,'mainTask','failed','stream-observed','boom');",
    )
    .unwrap();
    // 与生产启动序一致：老形状表由 IF NOT EXISTS 原样保留，其余表按新 DDL 建齐
    schema::init(&conn);
    // 旧 usage 四桶播种（与 tps_store_test::seed_old_usage 同款，锁「迁移不碰旧账本」）
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
    .unwrap();

    // 前提断言（防假绿）：新列在迁移前确实不存在
    assert!(
        !columns(&conn, "tps_observation").contains(&"response_elapsed_ms".to_string()),
        "前提不成立：旧形状表本应无 response_elapsed_ms"
    );

    let usage_sqls = [
        "SELECT * FROM usage_detail ORDER BY source_id, session_id, project_key, hour_key, model, provider",
        "SELECT * FROM usage_daily ORDER BY day_key, source_id, project_key, provider, model",
        "SELECT * FROM usage_cursor ORDER BY source_id, session_id",
        "SELECT * FROM usage_session ORDER BY source_id, session_id",
    ];
    let old_obs_sql = "SELECT source, host_instance, stable_call_key, started_at_ms, \
         first_token_at_ms, ended_at_ms, native_duration_ms, generation_ms, output_tokens, \
         output_scope, tool_ms, model, channel, session_key, turn_key, purpose, outcome, \
         method, failure_reason FROM tps_observation ORDER BY stable_call_key";
    let before_old_usage = snapshot_tables(&conn, &usage_sqls);
    let before_old_obs = snapshot_tables(&conn, &[old_obs_sql]);
    assert_eq!(before_old_obs[0].len(), 2, "前提：两条旧行在场");
    assert!(
        before_old_usage.iter().all(|t| !t.is_empty()),
        "前提：旧 usage 四表都有数据"
    );

    migration::migrate(&conn).unwrap();

    // 补列在场且可空（INTEGER 无 NOT NULL/DEFAULT → 旧行全 NULL = 无显式响应窗）
    let cols = columns(&conn, "tps_observation");
    assert!(
        cols.contains(&"response_elapsed_ms".to_string()),
        "迁移必须补出 response_elapsed_ms 列"
    );
    let (n_col, n_notnull): (usize, i64) = {
        let mut st = conn.prepare("PRAGMA table_info(tps_observation)").unwrap();
        st.query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, i64>(3)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .into_iter()
            .filter(|(name, _)| name == "response_elapsed_ms")
            .fold((0, 0), |(n, nn), (_, notnull)| (n + 1, nn + notnull))
    };
    assert_eq!(n_col, 1, "列只补一次");
    assert_eq!(
        n_notnull, 0,
        "新列必须可空（旧行 NULL = 旧记录无显式响应窗）"
    );

    // 旧行值逐字段保留（原 19 列逐字节不变），新列读回 NULL
    let after_old_obs = snapshot_tables(&conn, &[old_obs_sql]);
    assert_eq!(before_old_obs, after_old_obs, "补列不得改动旧行原值");
    let nulls: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tps_observation WHERE response_elapsed_ms IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(nulls, 2, "旧行新列全 NULL，不填 0");

    // 旧 usage 快照逐字节保留
    let after_migration_old_usage = snapshot_tables(&conn, &usage_sqls);
    assert_eq!(after_migration_old_usage, before_old_usage);

    // 迁移重跑幂等：不再重复加列、数据不动
    migration::migrate(&conn).unwrap();
    assert_eq!(
        columns(&conn, "tps_observation")
            .iter()
            .filter(|c| **c == "response_elapsed_ms")
            .count(),
        1,
        "迁移重跑是 no-op（不重复加列）"
    );
    assert_eq!(snapshot_tables(&conn, &[old_obs_sql]), after_old_obs);

    // 批内中途失败：ok 行 + ended_at_ms=-1 毒行同批 → 整事务回滚，表不变
    let mut poison = obs("poison");
    poison.ended_at_ms = -1; // epoch 非负 CHECK → 该行 INSERT 失败
    let result = store::commit_batch(
        &mut conn,
        &ReadBatch {
            observations: vec![obs("ok-row"), poison],
            source_state: SourceState {
                source: "kimi".into(),
                cursor: SourceCursor::empty("kimi"),
                ok: true,
                detail: None,
            },
            diagnostics: Vec::new(),
            next_cursor: SourceCursor::empty("kimi"),
        },
    );
    assert!(result.is_err(), "毒行必须让批失败");
    assert_eq!(
        snapshot_tables(&conn, &[old_obs_sql]),
        after_old_obs,
        "事务中途失败 → 事实表原样（ok 行也不残留）"
    );
    assert_eq!(
        snapshot_tables(&conn, &usage_sqls),
        before_old_usage,
        "失败批同样不碰旧 usage"
    );
}

// ==== 本文件私有辅助 ====

/// ReadBatch 夹具（与 tps_store_test 同形：游标推进 + 来源状态 ok）
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

/// 表列名清单（PRAGMA table_info）
fn columns(conn: &Connection, table: &str) -> Vec<String> {
    let mut st = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .unwrap();
    st.query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

/// 逐字节快照（与 database/migration.rs 测试同款：ValueRef 规范化渲染）
fn snapshot_tables(conn: &Connection, sqls: &[&str]) -> Vec<Vec<Vec<String>>> {
    fn render(value: rusqlite::types::ValueRef<'_>) -> String {
        use rusqlite::types::ValueRef;
        match value {
            ValueRef::Null => "NULL".to_string(),
            ValueRef::Integer(v) => v.to_string(),
            ValueRef::Real(v) => format!("{v:?}"),
            ValueRef::Text(v) => String::from_utf8_lossy(v).into_owned(),
            ValueRef::Blob(v) => v.iter().map(|b| format!("{b:02x}")).collect(),
        }
    }
    sqls.iter()
        .map(|sql| {
            let mut stmt = conn.prepare(sql).unwrap();
            stmt.query_map([], |row| {
                let mut values = Vec::new();
                for index in 0..row.as_ref().column_count() {
                    values.push(render(row.get_ref(index)?));
                }
                Ok(values)
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
        })
        .collect()
}
