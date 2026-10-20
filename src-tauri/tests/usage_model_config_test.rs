//! `usage_model_config` 表 + 模型条目配置服务的集成验收（2026-10-20 计划 Task 2）。
//!
//! 与 lib 单测（`services::usage::v2::model_config`）的分工：
//! * 本文件证明「**私有文件库**上真建表、真读写、重开可读、连续 migrate 幂等（七张 V2 表）」；
//! * lib 单测证明「观察条目聚合 / 别名解析 / 校验与 upsert 语义」。
//!
//! 测试隔离：一律 `support::open_ledger_db` 用例私有库，不碰真实 `~/.tuvis/tuvis.db`（GC 19）。

mod support;

use multi_agents_manager_lib::database::migration;
use multi_agents_manager_lib::services::usage::model::UsageSourceId;
use multi_agents_manager_lib::services::usage::v2::model::{
    ModelDisplayNameRequest, PricingIdentityRequest, PricingReference,
};
use multi_agents_manager_lib::services::usage::v2::model_config;
use rusqlite::Connection;

fn provider_request(model: &str, display: Option<&str>) -> ModelDisplayNameRequest {
    ModelDisplayNameRequest {
        provider_raw: Some("openai".to_string()),
        source_id: None,
        model_id: model.to_string(),
        display_name: display.map(str::to_string),
    }
}

fn seed_observed_daily(conn: &Connection) {
    conn.execute_batch(
        "INSERT INTO usage_daily
             (day_key, source_id, project_key, provider, provider_kind, model,
              input_fresh, cache_read, cache_write, output, request_total, requests,
              user_est, updated_at)
         VALUES
             ('2026-10-16','codex','','openai','measured','gpt-5',1,0,0,2,3,1,NULL,0),
             ('2026-10-17','codex','','openai','measured','gpt-5',1,0,0,4,5,1,NULL,0),
             ('2026-10-17','claude','','anthropic','measured','claude-x',1,0,0,6,7,1,NULL,0);",
    )
    .unwrap();
}

/// 连续 migrate 两次：七张 V2 表全部存在；旧账本四表逐值不变（DROP 七表后重建的口径）。
#[test]
fn migrate_twice_creates_seven_v2_tables_and_preserves_ledger() {
    let conn = support::open_ledger_db("model_config_migrate");
    // 模拟「旧库」：七张 V2 表全部尚未出现；旧账本四表带数据。
    conn.execute_batch(
        "DROP TABLE IF EXISTS usage_billing_state;
         DROP TABLE IF EXISTS usage_billing_revision;
         DROP TABLE IF EXISTS usage_pricing_state;
         DROP TABLE IF EXISTS usage_pricing_snapshot;
         DROP TABLE IF EXISTS usage_provider_alias;
         DROP TABLE IF EXISTS usage_price_override;
         DROP TABLE IF EXISTS usage_model_config;
         INSERT INTO usage_daily
             (day_key, source_id, project_key, provider, provider_kind, model,
              input_fresh, cache_read, cache_write, output, request_total, requests,
              user_est, updated_at)
         VALUES ('2026-10-17','codex','proj','openai','measured','gpt-5',10,2,3,4,12,1,5,200);",
    )
    .unwrap();
    let before: (String, i64, i64, i64, i64) = conn
        .query_row(
            "SELECT day_key, input_fresh, cache_read, output, request_total FROM usage_daily",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();

    migration::migrate(&conn).unwrap();
    migration::migrate(&conn).unwrap();

    for table in [
        "usage_billing_state",
        "usage_billing_revision",
        "usage_pricing_state",
        "usage_pricing_snapshot",
        "usage_provider_alias",
        "usage_price_override",
        "usage_model_config",
    ] {
        assert!(
            conn.prepare(&format!("SELECT * FROM {table} LIMIT 0"))
                .is_ok(),
            "迁移后必须存在 V2 表 {table}"
        );
    }
    let after: (String, i64, i64, i64, i64) = conn
        .query_row(
            "SELECT day_key, input_fresh, cache_read, output, request_total FROM usage_daily",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    assert_eq!(before, after, "连续迁移两次不得改动旧账本任何值");
}

/// 配置写入 → 关闭连接重开 → 配置仍在（文件库持久化；观察条目上可见）。
#[test]
fn model_config_survives_reopen() {
    let tag = "model_config_reopen";
    let path = {
        let conn = support::open_ledger_db(tag);
        seed_observed_daily(&conn);
        let entries = model_config::set_display_name_with_conn(
            &conn,
            &provider_request("gpt-5", Some("我的 GPT")),
            1_000,
        )
        .unwrap();
        assert_eq!(entries.len(), 2, "两个观测 provider 条目都在列");
        let entry = entries
            .iter()
            .find(|entry| entry.provider_raw.as_deref() == Some("openai"))
            .unwrap();
        assert_eq!(entry.display_model_name.as_deref(), Some("我的 GPT"));
        // 计价身份覆盖同时落库。
        model_config::set_pricing_identity_with_conn(
            &conn,
            &PricingIdentityRequest {
                provider_raw: Some("openai".to_string()),
                source_id: None,
                model_id: "gpt-5".to_string(),
                reference: Some(PricingReference {
                    provider_id: "openai".to_string(),
                    model_id: "gpt-5-2024-11-20".to_string(),
                }),
            },
            1_100,
        )
        .unwrap();
        std::path::PathBuf::from(conn.path().expect("私有库必须是文件库"))
    };
    let conn = Connection::open(path).unwrap();
    let now_ms = {
        use chrono::TimeZone;
        chrono::Local
            .with_ymd_and_hms(2026, 10, 20, 12, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis()
    };
    let entries = model_config::observed_entries_with_conn(&conn, now_ms).unwrap();
    let entry = entries
        .iter()
        .find(|entry| entry.provider_raw.as_deref() == Some("openai"))
        .expect("重开后 openai 条目仍在");
    assert_eq!(entry.display_model_name.as_deref(), Some("我的 GPT"));
    assert_eq!(entry.pricing_provider.as_deref(), Some("openai"));
    assert_eq!(entry.pricing_model.as_deref(), Some("gpt-5-2024-11-20"));
}

/// 表级 CHECK 与表达式唯一索引（T2 的 DDL 契约）：身份唯一、形状非法一律响亮失败。
#[test]
fn model_config_ddl_rejects_bad_shapes_and_duplicate_identity() {
    let conn = support::open_ledger_db("model_config_ddl");
    conn.execute_batch(
        "INSERT INTO usage_model_config
             (id, provider_raw, source_id, model_raw, display_name, updated_at)
         VALUES ('a', 'openai', NULL, 'gpt-5', NULL, 0);",
    )
    .unwrap();
    // 表达式唯一索引：不同 id、同一身份 → 拒绝（COALESCE 把 NULL 折叠成同一键）。
    let duplicate = conn.execute(
        "INSERT INTO usage_model_config
             (id, provider_raw, source_id, model_raw, display_name, updated_at)
         VALUES ('b', 'openai', NULL, 'gpt-5', NULL, 0);",
        [],
    );
    assert!(duplicate.is_err(), "表达式唯一索引必须拒绝同一身份的第二行");
    // 空 provider 条目必须带 source_id。
    let empty_provider_without_source = conn.execute(
        "INSERT INTO usage_model_config
             (id, provider_raw, source_id, model_raw, display_name, updated_at)
         VALUES ('c', '', NULL, 'gpt-5', NULL, 0);",
        [],
    );
    assert!(
        empty_provider_without_source.is_err(),
        "CHECK 必须拒绝空 provider 且无 source"
    );
    // pricing_* 必须成对。
    let half_pair = conn.execute(
        "INSERT INTO usage_model_config
             (id, provider_raw, source_id, model_raw, display_name, pricing_provider, updated_at)
         VALUES ('d', 'openai', NULL, 'gpt-5', NULL, 'openai', 0);",
        [],
    );
    assert!(half_pair.is_err(), "CHECK 必须拒绝半对 pricing 身份");
}

/// 幽灵身份（不在观测集）拒绝写入；空 provider 条目按 (source_id, model) 身份可配。
#[test]
fn ghost_identity_and_tool_model_identity_round_trip() {
    let conn = support::open_ledger_db("model_config_ghost");
    seed_observed_daily(&conn);
    // 未知 provider → 幽灵行，固定 detail。
    let err = model_config::set_display_name_with_conn(
        &conn,
        &ModelDisplayNameRequest {
            provider_raw: Some("ghost-provider".to_string()),
            source_id: None,
            model_id: "gpt-5".to_string(),
            display_name: Some("X".to_string()),
        },
        1_000,
    )
    .unwrap_err();
    assert_eq!(err.code, "usage-settings-invalid");
    assert_eq!(err.detail, "model config entry not observed");

    // 空 provider 身份：usage_daily 里有一行空 provider + 空模型（未知模型）。
    conn.execute(
        "INSERT INTO usage_daily
             (day_key, source_id, project_key, provider, provider_kind, model,
              input_fresh, cache_read, cache_write, output, request_total, requests,
              user_est, updated_at)
         VALUES ('2026-10-17','workbuddy','','','measured','',1,0,0,2,3,1,NULL,0)",
        [],
    )
    .unwrap();
    let entries = model_config::set_display_name_with_conn(
        &conn,
        &ModelDisplayNameRequest {
            provider_raw: None,
            source_id: Some(UsageSourceId::WorkBuddy),
            model_id: String::new(),
            display_name: Some("WorkBuddy 未知模型".to_string()),
        },
        1_200,
    )
    .unwrap();
    let entry = entries
        .iter()
        .find(|entry| entry.provider_raw.is_none())
        .expect("空 provider 条目在列");
    assert_eq!(entry.source_id, Some(UsageSourceId::WorkBuddy));
    assert_eq!(
        entry.display_model_name.as_deref(),
        Some("WorkBuddy 未知模型")
    );
}

/// 2026-10-20 条目 Top10：`recentTokens7d` = 宿主本地日界的最近 7 天（含今日）
/// requestTotal + output；`totalTokens` 仍是全历史。窗口外（第 7 天之前）不进 recent。
#[test]
fn recent_tokens_7d_covers_six_days_back_plus_today_only() {
    use chrono::TimeZone;
    let conn = support::open_ledger_db("model_config_recent_7d");
    let now_ms = chrono::Local
        .with_ymd_and_hms(2026, 10, 20, 12, 0, 0)
        .single()
        .unwrap()
        .timestamp_millis();
    // cutoff = now-6d 的宿主本地日界 = 2026-10-14：13 号在窗外，14 号恰好含入。
    let insert = |day: &str, request_total: i64, output: i64| {
        conn.execute(
            "INSERT INTO usage_daily
             (day_key, source_id, project_key, provider, provider_kind, model,
              input_fresh, cache_read, cache_write, output, request_total, requests,
              user_est, updated_at)
             VALUES (?1,'codex','','openai','measured','gpt-5',1,0,0,?3,?2,1,NULL,0)",
            rusqlite::params![day, request_total, output],
        )
        .unwrap();
    };
    // 每天各 10 requestTotal + 5 output = 15。
    insert("2026-10-13", 10, 5); // 窗外（now-7d）
    insert("2026-10-14", 10, 5); // 窗内第 1 天（cutoff 当天必须含入）
    insert("2026-10-19", 10, 5); // 窗内
    insert("2026-10-20", 10, 5); // 今日
                                 // 另一个模型只有历史用量：不出现在 recent，但保留全历史（条目不消失）。
    conn.execute(
        "INSERT INTO usage_daily
         (day_key, source_id, project_key, provider, provider_kind, model,
          input_fresh, cache_read, cache_write, output, request_total, requests,
          user_est, updated_at)
         VALUES ('2026-08-01','codex','','openai','measured','old-model',1,0,0,2,3,1,NULL,0)",
        [],
    )
    .unwrap();

    let entries = model_config::observed_entries_with_conn(&conn, now_ms).unwrap();
    let gpt = entries
        .iter()
        .find(|entry| entry.model_id == "gpt-5")
        .expect("gpt-5 条目在列");
    assert_eq!(gpt.total_tokens, 60, "全历史 = 4 天 × 15");
    assert_eq!(
        gpt.recent_tokens_7d, 45,
        "最近 7 天含今日 = 3 天 × 15（10-13 在窗外）"
    );
    let old = entries
        .iter()
        .find(|entry| entry.model_id == "old-model")
        .expect("窗口外历史条目不消失");
    assert_eq!(old.total_tokens, 5);
    assert_eq!(
        old.recent_tokens_7d, 0,
        "窗口外条目 recent=0（条目 Top10 自然沉底）"
    );
}

/// 2026-10-20 覆盖价币种迁移：老库的 `usage_price_override` 没有 currency 列 ⇒
/// 幂等 ALTER 补列并默认 'USD'（旧语义一律美元），行值原样保留；
/// 迁移后的表可继续经 DAO 写入 CNY 覆盖价。
#[test]
fn migrate_backfills_override_currency_column_defaulting_to_usd() {
    let mut conn = support::open_ledger_db("model_config_override_currency");
    // 模拟老库：手建**无 currency 列**的旧形态覆盖价表，带一行存量数据。
    conn.execute_batch(
        "DROP TABLE usage_price_override;
         CREATE TABLE usage_price_override (
             normalized_model_id TEXT PRIMARY KEY,
             input_scaled        INTEGER,
             cache_read_scaled   INTEGER,
             cache_write_scaled  INTEGER,
             output_scaled       INTEGER,
             updated_at          INTEGER NOT NULL,
             CHECK (
                 input_scaled IS NOT NULL OR cache_read_scaled IS NOT NULL
                 OR cache_write_scaled IS NOT NULL OR output_scaled IS NOT NULL
             )
         );
         INSERT INTO usage_price_override
             (normalized_model_id, input_scaled, output_scaled, updated_at)
         VALUES ('gpt-5', 1500000000, 7000000000, 42);",
    )
    .unwrap();

    migration::migrate(&conn).unwrap();
    migration::migrate(&conn).unwrap(); // 连续两次：ALTER 必须幂等

    let row: (String, i64, i64, i64) = conn
        .query_row(
            "SELECT currency, input_scaled, output_scaled, updated_at
             FROM usage_price_override WHERE normalized_model_id = 'gpt-5'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        row,
        ("USD".to_string(), 1_500_000_000, 7_000_000_000, 42),
        "存量行必须回填默认 USD 且四价/时间戳原样保留"
    );

    // 迁移后表可经 DAO 正常写 CNY（新契约），USD 存量行不受影响。
    multi_agents_manager_lib::services::usage::v2::pricing::set_override_with_conn(
        &mut conn,
        &multi_agents_manager_lib::services::usage::v2::model::PriceOverrideRequest {
            normalized_model_id: "cny-m".to_string(),
            currency: multi_agents_manager_lib::services::usage::v2::model::PriceCurrency::Cny,
            input: Some("8".to_string()),
            cache_read: None,
            cache_write: None,
            output: None,
        },
        1_000,
    )
    .unwrap();
    let cny: String = conn
        .query_row(
            "SELECT currency FROM usage_price_override WHERE normalized_model_id = 'cny-m'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cny, "CNY");

    // ALTER 补列自带 CHECK(currency IN ('USD','CNY'))：迁移后的老表同样拒绝脏币种
    // （存量行回填的 'USD' 合法，不受 CHECK 追溯校验影响）。
    let dirty = conn.execute(
        "INSERT INTO usage_price_override
             (normalized_model_id, currency, input_scaled, updated_at)
         VALUES ('eur-m', 'EUR', 1, 0)",
        [],
    );
    assert!(
        dirty.is_err(),
        "迁移后的表必须沿用 CHECK 拒绝非 USD/CNY 币种"
    );
}

/// 2026-10-20 两张新表（usage_pricing_native / usage_pricing_fx）：schema::init 建齐、
/// DROP 后 migrate 幂等重建；GC 不影响 active（快照生命周期见 pricing 单测）。
#[test]
fn migrate_recreates_native_and_fx_tables_idempotently() {
    let conn = support::open_ledger_db("model_config_native_fx_tables");
    conn.execute_batch(
        "DROP TABLE IF EXISTS usage_pricing_native;
         DROP TABLE IF EXISTS usage_pricing_fx;",
    )
    .unwrap();
    migration::migrate(&conn).unwrap();
    migration::migrate(&conn).unwrap();
    // native：currency 列带 CHECK('CNY')；fx：snapshot_id 主键。
    conn.execute(
        "INSERT INTO usage_pricing_native
             (snapshot_id, provider_id, model_id, normalized_model_id, currency,
              input_scaled, cache_read_scaled, cache_write_scaled, output_scaled,
              source_url, fetched_at)
         VALUES ('s1','zhipuai','glm-5.3','glm-5-3','CNY',1,NULL,NULL,NULL,'u',0)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO usage_pricing_fx (snapshot_id, rate_scaled, fetched_at, source_url)
         VALUES ('s1', 7200000000, 0, 'u')",
        [],
    )
    .unwrap();
    assert!(
        conn.execute(
            "INSERT INTO usage_pricing_native
                 (snapshot_id, provider_id, model_id, normalized_model_id, currency,
                  input_scaled, cache_read_scaled, cache_write_scaled, output_scaled,
                  source_url, fetched_at)
             VALUES ('s2','zhipuai','m','m','USD',1,NULL,NULL,NULL,'u',0)",
            [],
        )
        .is_err(),
        "native.currency CHECK 必须拒绝非 CNY"
    );
    let (native_rows, fx_rows): (i64, i64) = conn
        .query_row(
            "SELECT (SELECT COUNT(*) FROM usage_pricing_native),
                    (SELECT COUNT(*) FROM usage_pricing_fx)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((native_rows, fx_rows), (1, 1));
}
