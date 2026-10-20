//! Task 2 集成测试：provider 别名表、服务 CRUD、校验与迁移幂等。
//! 全部使用 `support::open_ledger_db` 的用例私有库，不触碰真实账本。

mod support;

use multi_agents_manager_lib::database::migration;
use multi_agents_manager_lib::services::usage::v2::channel;
use multi_agents_manager_lib::services::usage::v2::model::{
    ProviderAliasKind, ProviderAliasRule, UsageSourceId,
};
use multi_agents_manager_lib::services::usage::UsageError;
use rusqlite::types::ValueRef;
use rusqlite::{params, Connection};

fn provider_rule(id: &str, provider: &str, display: &str) -> ProviderAliasRule {
    ProviderAliasRule {
        id: id.to_string(),
        kind: ProviderAliasKind::Provider,
        provider: Some(provider.to_string()),
        source_id: None,
        model_id: None,
        display_name: display.to_string(),
    }
}

fn tool_rule(id: &str, source: UsageSourceId, model: &str, display: &str) -> ProviderAliasRule {
    ProviderAliasRule {
        id: id.to_string(),
        kind: ProviderAliasKind::ToolModel,
        provider: None,
        source_id: Some(source),
        model_id: Some(model.to_string()),
        display_name: display.to_string(),
    }
}

fn assert_invalid(err: UsageError, detail_prefix: &str) {
    assert_eq!(
        err.code, "usage-settings-invalid",
        "应返回 usage-settings-invalid，实际 {err:?}"
    );
    assert!(
        err.detail.starts_with(detail_prefix),
        "detail 应以 `{detail_prefix}` 开头，实际 `{}`",
        err.detail
    );
}

fn insert_raw_alias(
    conn: &Connection,
    id: &str,
    kind: &str,
    provider: Option<&str>,
    source: Option<&str>,
    model: Option<&str>,
    display: &str,
) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO usage_provider_alias
         (id, kind, provider_raw, source_id, model_id, display_name, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, 0)",
        params![id, kind, provider, source, model, display],
    )
}

#[test]
fn schema_creates_alias_table_and_partial_unique_indexes() {
    let conn = support::open_ledger_db("provider_alias_schema");

    let tables: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'table' AND name = 'usage_provider_alias'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(tables, 1, "必须建出 usage_provider_alias 表");
    for index in [
        "idx_usage_provider_alias_provider",
        "idx_usage_provider_alias_tool_model",
    ] {
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?1",
                [index],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(exists, 1, "必须建出唯一索引 {index}");
    }

    // kind 约束与两类形状 CHECK。
    assert!(insert_raw_alias(&conn, "bad-kind", "bogus", Some("x"), None, None, "X").is_err());
    assert!(insert_raw_alias(
        &conn,
        "bad-provider-shape",
        "provider",
        Some("x"),
        Some("codex"),
        None,
        "X"
    )
    .is_err());
    assert!(insert_raw_alias(
        &conn,
        "bad-provider-empty",
        "provider",
        Some(""),
        None,
        None,
        "X"
    )
    .is_err());
    assert!(insert_raw_alias(
        &conn,
        "bad-tool-shape",
        "toolModel",
        Some("x"),
        Some("codex"),
        Some("m"),
        "X"
    )
    .is_err());
    assert!(insert_raw_alias(
        &conn,
        "bad-tool-missing-model",
        "toolModel",
        None,
        Some("codex"),
        None,
        "X"
    )
    .is_err());

    // 部分唯一索引：provider 原始串唯一（空模型 toolModel 也唯一）。
    insert_raw_alias(&conn, "p1", "provider", Some("openai"), None, None, "A").unwrap();
    assert!(
        insert_raw_alias(&conn, "p2", "provider", Some("openai"), None, None, "B").is_err(),
        "同一 provider_raw 不得有第二条规则"
    );
    insert_raw_alias(&conn, "t1", "toolModel", None, Some("codex"), Some(""), "T").unwrap();
    assert!(
        insert_raw_alias(&conn, "t2", "toolModel", None, Some("codex"), Some(""), "U").is_err(),
        "同一 (source, model) 不得有第二条规则"
    );
    insert_raw_alias(
        &conn,
        "t3",
        "toolModel",
        None,
        Some("codex"),
        Some("gpt-5"),
        "G",
    )
    .unwrap();

    // 再次 init 不得重复建表/建索引或报错。
    multi_agents_manager_lib::database::schema::init(&conn);
    let tables: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'table' AND name = 'usage_provider_alias'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(tables, 1, "schema::init 必须幂等");
}

#[test]
fn service_crud_round_trip_survives_reopen() {
    let mut conn = support::open_ledger_db("provider_alias_crud");
    let path = conn
        .path()
        .expect("open_ledger_db 必须是文件库")
        .to_string();

    assert!(
        channel::load_with_conn(&conn).unwrap().aliases.is_empty(),
        "新库必须返回空配置"
    );

    let config = channel::upsert_with_conn(
        &mut conn,
        &provider_rule("p1", "openai", "OpenAI 订阅"),
        100,
    )
    .unwrap();
    assert_eq!(config.aliases.len(), 1);
    assert_eq!(config.aliases[0].display_name, "OpenAI 订阅");

    let config = channel::upsert_with_conn(
        &mut conn,
        &tool_rule("t1", UsageSourceId::Codex, "", "Codex 未知模型"),
        150,
    )
    .unwrap();
    assert_eq!(
        config.aliases.len(),
        2,
        "空 modelId 的 toolModel 指派必须可保存"
    );

    // 同 id 更新：created_at 保留、updated_at 刷新、唯一键不额外新增行。
    let config =
        channel::upsert_with_conn(&mut conn, &provider_rule("p1", "openai", "OpenAI Pro"), 200)
            .unwrap();
    assert_eq!(config.aliases.len(), 2);
    let (created_at, updated_at): (i64, i64) = conn
        .query_row(
            "SELECT created_at, updated_at FROM usage_provider_alias WHERE id = 'p1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((created_at, updated_at), (100, 200));

    // 删除不存在的 id 是幂等空操作。
    let config = channel::delete_with_conn(&mut conn, "missing").unwrap();
    assert_eq!(config.aliases.len(), 2);

    // 删除存在的 id。
    let config = channel::delete_with_conn(&mut conn, "t1").unwrap();
    assert_eq!(config.aliases.len(), 1);

    drop(conn);

    // 重开库后读取：别名持久化。
    let conn = Connection::open(&path).unwrap();
    let config = channel::load_with_conn(&conn).unwrap();
    assert_eq!(config.aliases.len(), 1);
    assert_eq!(config.aliases[0].id, "p1");
    assert_eq!(config.aliases[0].display_name, "OpenAI Pro");
}

#[test]
fn upsert_trims_provider_and_display_name_at_service_boundary() {
    let mut conn = support::open_ledger_db("provider_alias_trim");

    let config = channel::upsert_with_conn(
        &mut conn,
        &provider_rule("p1", "  openai  ", "  OpenAI  "),
        100,
    )
    .unwrap();
    assert_eq!(config.aliases[0].provider.as_deref(), Some("openai"));
    assert_eq!(config.aliases[0].display_name, "OpenAI");
    let stored: (String, String) = conn
        .query_row(
            "SELECT provider_raw, display_name FROM usage_provider_alias WHERE id = 'p1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(stored, ("openai".to_string(), "OpenAI".to_string()));

    // trim 后与既有键相同：冲突判定按入库值执行。
    let err =
        channel::upsert_with_conn(&mut conn, &provider_rule("p2", "openai", "另一个名字"), 110)
            .unwrap_err();
    assert_invalid(err, "provider alias key already exists: provider:openai");

    // 空白 provider / display name 的 trim 结果为空：按既有校验拒绝。
    let err =
        channel::upsert_with_conn(&mut conn, &provider_rule("p3", "   ", "X"), 120).unwrap_err();
    assert_invalid(err, "provider alias provider is empty: p3");
    let err =
        channel::upsert_with_conn(&mut conn, &provider_rule("p4", "x", "   "), 130).unwrap_err();
    assert_invalid(err, "provider alias display name is empty: p4");

    // toolModel 的 sourceId / modelId 保持原样（模型名可能含合法空格）。
    let config = channel::upsert_with_conn(
        &mut conn,
        &tool_rule("t1", UsageSourceId::Codex, " gpt-5 ", "  Codex  "),
        140,
    )
    .unwrap();
    let tool = config.aliases.iter().find(|rule| rule.id == "t1").unwrap();
    assert_eq!(tool.model_id.as_deref(), Some(" gpt-5 "));
    assert_eq!(tool.display_name, "Codex");
}

#[test]
fn key_conflicts_are_rejected_with_fixed_detail() {
    let mut conn = support::open_ledger_db("provider_alias_conflict");
    channel::upsert_with_conn(
        &mut conn,
        &provider_rule("p1", "openai", "OpenAI 订阅"),
        100,
    )
    .unwrap();

    let err =
        channel::upsert_with_conn(&mut conn, &provider_rule("p2", "openai", "另一个名字"), 110)
            .unwrap_err();
    assert_invalid(err, "provider alias key already exists: provider:openai");

    channel::upsert_with_conn(
        &mut conn,
        &tool_rule("t1", UsageSourceId::Codex, "gpt-5", "Codex GPT-5"),
        120,
    )
    .unwrap();
    let err = channel::upsert_with_conn(
        &mut conn,
        &tool_rule("t2", UsageSourceId::Codex, "gpt-5", "重复键"),
        130,
    )
    .unwrap_err();
    assert_invalid(
        err,
        "provider alias key already exists: toolModel:codex:gpt-5",
    );

    // 同 id 改成已被另一条规则占用的键也必须拒绝，且两条规则都保持不变。
    channel::upsert_with_conn(
        &mut conn,
        &tool_rule("t2", UsageSourceId::Codex, "gpt-5-mini", "Codex GPT-5 mini"),
        140,
    )
    .unwrap();
    let err = channel::upsert_with_conn(
        &mut conn,
        &tool_rule("t2", UsageSourceId::Codex, "gpt-5", "改名"),
        150,
    )
    .unwrap_err();
    assert_invalid(
        err,
        "provider alias key already exists: toolModel:codex:gpt-5",
    );

    let config = channel::load_with_conn(&conn).unwrap();
    assert_eq!(config.aliases.len(), 3, "冲突不得写入半条规则");
    assert!(
        config
            .aliases
            .iter()
            .any(|rule| rule.id == "t1" && rule.display_name == "Codex GPT-5"),
        "冲突时既有规则不得被覆盖"
    );
    assert!(
        config
            .aliases
            .iter()
            .any(|rule| rule.id == "t2" && rule.display_name == "Codex GPT-5 mini"),
        "同 id 冲突失败后原规则必须保持不变"
    );
}

#[test]
fn validation_rejects_bad_rules_with_fixed_details() {
    let mut conn = support::open_ledger_db("provider_alias_validation");

    let empty_id = provider_rule("", "openai", "A");
    assert_invalid(
        channel::upsert_with_conn(&mut conn, &empty_id, 1).unwrap_err(),
        "provider alias id is empty",
    );

    let mut empty_display = provider_rule("p1", "openai", "");
    empty_display.display_name = "   ".to_string();
    assert_invalid(
        channel::upsert_with_conn(&mut conn, &empty_display, 1).unwrap_err(),
        "provider alias display name is empty: p1",
    );

    let mut empty_provider = provider_rule("p1", "openai", "A");
    empty_provider.provider = Some("  ".to_string());
    assert_invalid(
        channel::upsert_with_conn(&mut conn, &empty_provider, 1).unwrap_err(),
        "provider alias provider is empty: p1",
    );

    let mut provider_with_source = provider_rule("p1", "openai", "A");
    provider_with_source.source_id = Some(UsageSourceId::Codex);
    assert_invalid(
        channel::upsert_with_conn(&mut conn, &provider_with_source, 1).unwrap_err(),
        "provider alias source/model key is invalid: p1",
    );

    let mut tool_with_provider = tool_rule("t1", UsageSourceId::Codex, "", "A");
    tool_with_provider.provider = Some("openai".to_string());
    assert_invalid(
        channel::upsert_with_conn(&mut conn, &tool_with_provider, 1).unwrap_err(),
        "provider alias source/model key is invalid: t1",
    );

    let mut tool_without_model = tool_rule("t1", UsageSourceId::Codex, "", "A");
    tool_without_model.model_id = None;
    assert_invalid(
        channel::upsert_with_conn(&mut conn, &tool_without_model, 1).unwrap_err(),
        "provider alias source/model key is invalid: t1",
    );

    let mut tool_without_source = tool_rule("t1", UsageSourceId::Codex, "", "A");
    tool_without_source.source_id = None;
    assert_invalid(
        channel::upsert_with_conn(&mut conn, &tool_without_source, 1).unwrap_err(),
        "provider alias source/model key is invalid: t1",
    );
}

fn render(value: ValueRef<'_>) -> String {
    match value {
        ValueRef::Null => "NULL".to_string(),
        ValueRef::Integer(v) => v.to_string(),
        ValueRef::Real(v) => format!("{v:?}"),
        ValueRef::Text(v) => String::from_utf8_lossy(v).into_owned(),
        ValueRef::Blob(v) => v.iter().map(|b| format!("{b:02x}")).collect(),
    }
}

fn snapshot(conn: &Connection, sql: &str) -> Vec<Vec<String>> {
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
}

#[test]
fn migrate_twice_is_idempotent_and_preserves_current_ledger() {
    let conn = support::open_ledger_db("provider_alias_migrate");
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
             VALUES ('2026-10-17','codex','proj','openai','measured','gpt-5',
                     10,2,3,4,12,1,5,200);
         INSERT INTO usage_cursor
             (source_id, session_id, fingerprint, byte_offset, ordinal, last_cumulative,
              mtime_ms, file_size, state_json, updated_at)
             VALUES ('codex','s1','fp',42,3,99,123,456,'{\"k\":1}',789);
         INSERT INTO usage_session
             (source_id, session_id, project_key, project_label, project_path_raw,
              project_realpath, title, is_subagent, parent_session_id, first_seen_at,
              last_seen_at)
             VALUES ('codex','s1','proj','Proj','/tmp/p','/tmp/p','T',0,NULL,100,200);",
    )
    .unwrap();

    let detail_sql = "SELECT * FROM usage_detail
         ORDER BY source_id, session_id, project_key, hour_key, model, provider";
    let daily_sql = "SELECT * FROM usage_daily
         ORDER BY day_key, source_id, project_key, provider, model";
    let cursor_sql = "SELECT * FROM usage_cursor ORDER BY source_id, session_id";
    let session_sql = "SELECT * FROM usage_session ORDER BY source_id, session_id";
    let queries = [detail_sql, daily_sql, cursor_sql, session_sql];
    let before: Vec<Vec<Vec<String>>> = queries.iter().map(|sql| snapshot(&conn, sql)).collect();
    assert!(
        before.iter().all(|table| !table.is_empty()),
        "前提：四张旧表都必须有数据"
    );

    // 模拟「当前形状的旧库」：六张 V2 表尚未出现。
    conn.execute_batch(
        "DROP TABLE usage_provider_alias;
         DROP TABLE usage_billing_state;
         DROP TABLE usage_billing_revision;
         DROP TABLE usage_pricing_state;
         DROP TABLE usage_pricing_snapshot;
         DROP TABLE usage_price_override;",
    )
    .unwrap();

    migration::migrate(&conn).unwrap();
    migration::migrate(&conn).unwrap();

    let after: Vec<Vec<Vec<String>>> = queries.iter().map(|sql| snapshot(&conn, sql)).collect();
    assert_eq!(before, after, "连续迁移两次不得改动旧账本任何行或游标");

    for table in [
        "usage_provider_alias",
        "usage_billing_state",
        "usage_billing_revision",
        "usage_pricing_state",
        "usage_pricing_snapshot",
        "usage_price_override",
    ] {
        assert!(
            conn.prepare(&format!("SELECT * FROM {table} LIMIT 0"))
                .is_ok(),
            "迁移后必须存在 V2 表 {table}"
        );
    }
}
