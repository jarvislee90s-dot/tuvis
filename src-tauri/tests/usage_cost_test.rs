//! Task 10：模型级等价费用查询与 CSV（只读账本）。
//! 所有用例使用 `support::open_ledger_db` 的私有库；不触碰真实账本、不访问网络。

mod support;

use multi_agents_manager_lib::services::usage::model::{UsageRange, UsageRangePreset};
use multi_agents_manager_lib::services::usage::v2::cost_query::{
    cost_csv_with_conn, costs_with_conn,
};
use multi_agents_manager_lib::services::usage::v2::model::{
    ChannelKind, CostStatus, PriceCurrency, PriceOverrideRequest, PriceRuleKind, PricingBasis,
    TimePriceRule, TimePriceTimezone, TimePriceWindow,
};
use multi_agents_manager_lib::services::usage::v2::model::{
    ModelDisplayNameRequest, PricingIdentityRequest, PricingReference,
};
use multi_agents_manager_lib::services::usage::v2::model_config;
use multi_agents_manager_lib::services::usage::v2::pricing;
use multi_agents_manager_lib::services::usage::UsageError;
use rusqlite::{params, Connection};
use serde_json::{json, Value};

fn local_ms(y: i32, m: u32, d: u32, h: u32, min: u32) -> i64 {
    use chrono::TimeZone;
    chrono::Local
        .with_ymd_and_hms(y, m, d, h, min, 0)
        .single()
        .unwrap()
        .timestamp_millis()
}

fn now_ms() -> i64 {
    local_ms(2026, 10, 17, 12, 0)
}

fn range(preset: UsageRangePreset) -> UsageRange {
    UsageRange {
        preset,
        from: None,
        to: None,
    }
}

fn add_daily(
    conn: &Connection,
    day: &str,
    source: &str,
    provider: &str,
    provider_kind: &str,
    model: &str,
    output: i64,
) {
    conn.execute(
        "INSERT INTO usage_daily
         (day_key, source_id, project_key, provider, provider_kind, model,
          input_fresh, cache_read, cache_write, output, request_total, requests,
          user_est, updated_at)
         VALUES (?1, ?2, '', ?3, ?4, ?5, 10, 0, 0, ?6, 10, 1, NULL, 0)",
        params![day, source, provider, provider_kind, model, output],
    )
    .unwrap();
}

/// 小时档明细行（Last5h 只读 `usage_detail`）；桶形状与日档用例保持一致：
/// `requestTotal = fresh + read + write = 130`，因此四个分量都满足互斥推导。
fn add_detail(conn: &Connection, day: &str, hour: &str, source: &str, provider: &str) {
    conn.execute(
        "INSERT INTO usage_detail
         (source_id, session_id, project_key, hour_key, day_key, model, provider,
          provider_kind, input_fresh, cache_read, cache_write, output,
          request_total, requests, user_est, first_seen_at, updated_at)
         VALUES (?1, 's1', '', ?2, ?3, 'm', ?4, 'measured', 100, 20, 10, 30, 130, 1, NULL, 0, 0)",
        params![source, hour, day, provider],
    )
    .unwrap();
}

/// 指定四桶的小时明细行（分时价格用例）：`requestTotal = fresh + read + write`，
/// 保证 `safe_components` 的互斥恢复口径与生产一致。
#[allow(clippy::too_many_arguments)]
fn add_detail_buckets(
    conn: &Connection,
    hour_key: &str,
    source: &str,
    provider: &str,
    model: &str,
    input_fresh: i64,
    cache_read: i64,
    cache_write: i64,
    output: i64,
) {
    let day_key: String = hour_key.chars().take(10).collect();
    let request_total = input_fresh + cache_read + cache_write;
    conn.execute(
        "INSERT INTO usage_detail
         (source_id, session_id, project_key, hour_key, day_key, model, provider,
          provider_kind, input_fresh, cache_read, cache_write, output,
          request_total, requests, user_est, first_seen_at, updated_at)
         VALUES (?1, 's1', '', ?2, ?3, ?4, ?5, 'measured', ?6, ?7, ?8, ?9, ?10, 1, NULL, 0, 0)",
        params![
            source,
            hour_key,
            day_key,
            model,
            provider,
            input_fresh,
            cache_read,
            cache_write,
            output,
            request_total
        ],
    )
    .unwrap();
}

fn timed_override_request(
    model: &str,
    peak_input: &str,
    peak_output: Option<&str>,
    offpeak_input: Option<&str>,
    offpeak_output: Option<&str>,
    timezone: TimePriceTimezone,
    windows: &[(&str, &str)],
) -> PriceOverrideRequest {
    PriceOverrideRequest {
        normalized_model_id: model.to_string(),
        currency: PriceCurrency::Usd,
        input: Some(peak_input.to_string()),
        cache_read: None,
        cache_write: None,
        output: peak_output.map(str::to_string),
        time_rule: Some(TimePriceRule {
            timezone,
            windows: windows
                .iter()
                .map(|(start, end)| TimePriceWindow {
                    start: (*start).to_string(),
                    end: (*end).to_string(),
                })
                .collect(),
            offpeak_input: offpeak_input.map(str::to_string),
            offpeak_cache_read: None,
            offpeak_cache_write: None,
            offpeak_output: offpeak_output.map(str::to_string),
        }),
    }
}

fn profile(id: &str, name: &str) -> Value {
    json!({"id": id, "name": name, "kind": "api", "note": "", "archived": false})
}

fn billing_rule(
    id: &str,
    source: &str,
    provider: Option<&str>,
    model: &str,
    profile_id: &str,
) -> Value {
    json!({
        "id": id,
        "identity": {"sourceId": source, "observedProvider": provider, "modelId": model},
        "match": "exact",
        "billingProfileId": profile_id,
        "effectiveFrom": 0,
        "effectiveTo": null,
        "mixed": false
    })
}

fn pricing_rule(
    id: &str,
    source: &str,
    provider: Option<&str>,
    model: &str,
    match_kind: &str,
    ref_provider: &str,
    ref_model: &str,
) -> Value {
    json!({
        "id": id,
        "identity": {"sourceId": source, "observedProvider": provider, "modelId": model},
        "match": match_kind,
        "reference": {"providerId": ref_provider, "modelId": ref_model}
    })
}

fn put_config(conn: &Connection, profiles: Value, rules: Value, pricing_rules: Value) {
    let config = json!({
        "revision": 1,
        "profiles": profiles,
        "rules": rules,
        "pricingRules": pricing_rules
    });
    conn.execute(
        "INSERT INTO usage_billing_state (singleton, revision, config_json)
         VALUES (1, 1, ?1)
         ON CONFLICT(singleton) DO UPDATE SET revision = 1, config_json = excluded.config_json",
        params![config.to_string()],
    )
    .unwrap();
}

fn put_pricing(conn: &Connection, payload: &str, fetched_at: i64) {
    // 与生产一致的内容寻址 id：进程级目录缓存按 id 复用，固定 id 会让并行用例串用目录。
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(payload.as_bytes());
    let hash: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    let id = format!("{hash}:{fetched_at}");
    conn.execute(
        "INSERT INTO usage_pricing_snapshot (id, source_url, fetched_at, payload_json)
         VALUES (?1, 'https://models.dev/api.json', ?2, ?3)",
        params![id, fetched_at, payload],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO usage_pricing_state (singleton, enabled, active_snapshot_id, last_error)
         VALUES (1, 1, ?1, NULL)
         ON CONFLICT(singleton) DO UPDATE SET
             enabled = 1, active_snapshot_id = excluded.active_snapshot_id, last_error = NULL",
        params![id],
    )
    .unwrap();
}

fn priced_model(model_name: &str, include_write: bool) -> Value {
    let mut cost = json!({"input": 2, "cache_read": 1, "output": 10});
    if include_write {
        cost["cache_write"] = json!(3);
    }
    json!({
        "id": model_name,
        "cost": cost,
        "modalities": {"input": ["text"], "output": ["text"]}
    })
}

fn catalog(models: Value) -> String {
    json!({"openai": {"id": "openai", "name": "OpenAI", "models": models}}).to_string()
}

fn obj_keys(value: &Value) -> Vec<String> {
    let mut keys: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    keys
}

fn csv_records(text: &str) -> Vec<Vec<String>> {
    let mut records = Vec::new();
    let mut record = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if quoted {
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    quoted = false;
                }
            } else {
                field.push(ch);
            }
        } else if ch == '"' {
            quoted = true;
        } else if ch == ',' {
            record.push(std::mem::take(&mut field));
        } else if ch == '\n' {
            record.push(std::mem::take(&mut field));
            records.push(std::mem::take(&mut record));
        } else if ch != '\r' {
            field.push(ch);
        }
    }
    if !field.is_empty() || !record.is_empty() {
        record.push(field);
        records.push(record);
    }
    records
}

fn snapshot_table(conn: &Connection, sql: &str) -> Vec<Vec<String>> {
    let mut stmt = conn.prepare(sql).unwrap();
    stmt.query_map([], |row| {
        let mut values = Vec::new();
        for index in 0..row.as_ref().column_count() {
            values.push(match row.get_ref(index)? {
                rusqlite::types::ValueRef::Null => "NULL".to_string(),
                rusqlite::types::ValueRef::Integer(v) => v.to_string(),
                rusqlite::types::ValueRef::Real(v) => format!("{v:?}"),
                rusqlite::types::ValueRef::Text(v) => String::from_utf8_lossy(v).into_owned(),
                rusqlite::types::ValueRef::Blob(v) => {
                    v.iter().map(|b| format!("{b:02x}")).collect()
                }
            });
        }
        Ok(values)
    })
    .unwrap()
    .collect::<Result<Vec<_>, _>>()
    .unwrap()
}

#[test]
fn same_model_different_profiles_are_not_merged() {
    let conn = support::open_ledger_db("cost_profiles");
    add_daily(&conn, "2026-10-17", "codex", "", "measured", "m", 5);
    add_daily(&conn, "2026-10-17", "claude", "", "measured", "m", 7);
    put_config(
        &conn,
        json!([profile("A", "Profile A"), profile("B", "Profile B")]),
        json!([
            billing_rule("r1", "codex", None, "m", "A"),
            billing_rule("r2", "claude", None, "m", "B")
        ]),
        json!([
            pricing_rule("p1", "codex", None, "m", "exact", "openai", "m"),
            pricing_rule("p2", "claude", None, "m", "exact", "openai", "m")
        ]),
    );
    put_pricing(
        &conn,
        &catalog(json!({"m": priced_model("m", true)})),
        1_000,
    );

    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    assert_eq!(costs.rows.len(), 2, "不同 profile 绝不合并");
    let profiles: Vec<&str> = costs
        .rows
        .iter()
        .filter_map(|row| row.billing_profile_id.as_deref())
        .collect();
    assert!(profiles.contains(&"A"));
    assert!(profiles.contains(&"B"));
    assert!(costs.rows.iter().all(|row| row.estimated_usd.is_some()));
    assert!(costs
        .rows
        .iter()
        .all(|row| row.status == CostStatus::Priced));
}

#[test]
fn same_profile_different_pricing_reference_is_not_merged() {
    let conn = support::open_ledger_db("cost_refs");
    add_daily(&conn, "2026-10-17", "codex", "prov1", "measured", "m", 5);
    add_daily(&conn, "2026-10-17", "codex", "prov2", "measured", "m", 7);
    put_config(
        &conn,
        json!([profile("A", "Profile A")]),
        json!([
            billing_rule("r1", "codex", Some("prov1"), "m", "A"),
            billing_rule("r2", "codex", Some("prov2"), "m", "A")
        ]),
        json!([
            pricing_rule("p1", "codex", Some("prov1"), "m", "exact", "openai", "m1"),
            pricing_rule("p2", "codex", Some("prov2"), "m", "exact", "openai", "m2")
        ]),
    );
    put_pricing(
        &conn,
        &catalog(json!({
            "m1": priced_model("m1", true),
            "m2": priced_model("m2", true)
        })),
        1_000,
    );

    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    assert_eq!(costs.rows.len(), 2, "同 profile 不同 reference 不得合并");
    let refs: Vec<&str> = costs
        .rows
        .iter()
        .filter_map(|row| row.pricing_reference.as_ref().map(|r| r.model_id.as_str()))
        .collect();
    assert!(refs.contains(&"m1"));
    assert!(refs.contains(&"m2"));
    assert!(costs
        .rows
        .iter()
        .all(|row| row.billing_profile_id.as_deref() == Some("A")));
}

#[test]
fn unknown_context_keeps_source_provider_and_model() {
    let conn = support::open_ledger_db("cost_unknown");
    add_daily(&conn, "2026-10-17", "codex", "openai", "measured", "m", 5);
    add_daily(
        &conn,
        "2026-10-17",
        "claude",
        "anthropic",
        "measured",
        "m",
        7,
    );

    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    assert_eq!(costs.rows.len(), 2);
    for row in &costs.rows {
        assert_eq!(row.billing_profile_id, None);
        assert_eq!(
            row.attribution_kind,
            multi_agents_manager_lib::services::usage::v2::model::AttributionKind::Unknown
        );
    }
    let providers: Vec<Option<String>> = costs
        .rows
        .iter()
        .map(|row| row.identity.observed_provider.clone())
        .collect();
    assert!(providers.contains(&Some("openai".to_string())));
    assert!(providers.contains(&Some("anthropic".to_string())));
}

#[test]
fn cost_rows_have_no_aggregate_total_fields() {
    let conn = support::open_ledger_db("cost_no_total");
    add_daily(&conn, "2026-10-17", "codex", "", "measured", "m", 5);
    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    let value = serde_json::to_value(&costs).unwrap();
    assert_eq!(
        obj_keys(&value),
        vec![
            "fxFallback",
            "fxFetchedAt",
            "fxRate",
            "fxSource",
            "pricing",
            "range",
            "rows"
        ],
        "UsageCosts 带 2026-10-20 的 fx 面"
    );
    let row = &value["rows"][0];
    assert_eq!(
        obj_keys(row),
        vec![
            "attributionKind",
            "billingProfileId",
            "cacheHitRate",
            "channelKey",
            "channelKind",
            "cnyConverted",
            "components",
            "coverageRate",
            "coverageRateCny",
            "displayModelName",
            "displayProvider",
            "estimatedCny",
            "estimatedUsd",
            "hasEmptyProvider",
            "identity",
            "key",
            "normalizedModelId",
            "observedProviders",
            "priceOverride",
            "priceRuleKind",
            "priceUpdatedAt",
            "pricedTokens",
            "pricedTokensCny",
            "pricingBasis",
            "pricingReference",
            "pricingReleaseDate",
            "pricingSnapshotId",
            "reasons",
            "sourceIds",
            "status",
            "statusCny",
            "timeBreakdown",
            "tokenUsage",
            "unpricedTokens",
            "unpricedTokensCny",
            "usdConverted",
        ],
        "CostRow 带双币种字段（2026-10-20）"
    );
    for key in obj_keys(row) {
        let lower = key.to_lowercase();
        assert!(
            !lower.contains("total") && !lower.contains("subtotal"),
            "费用行不得出现总计字段：{key}"
        );
    }
    assert_eq!(
        obj_keys(&row["components"]),
        vec!["cacheRead", "cacheWrite", "input", "output"]
    );
}

#[test]
fn ledger_tokens_and_cursor_are_unchanged() {
    let conn = support::open_ledger_db("cost_conservation");
    add_daily(&conn, "2026-10-17", "codex", "", "measured", "m", 5);
    conn.execute(
        "INSERT INTO usage_cursor
         (source_id, session_id, fingerprint, byte_offset, ordinal, last_cumulative,
          mtime_ms, file_size, state_json, updated_at)
         VALUES ('codex','s1','fp',42,3,99,123,456,'{\"k\":1}',789)",
        [],
    )
    .unwrap();
    let before_daily = snapshot_table(
        &conn,
        "SELECT * FROM usage_daily ORDER BY day_key, source_id",
    );
    let before_cursor = snapshot_table(
        &conn,
        "SELECT * FROM usage_cursor ORDER BY source_id, session_id",
    );

    costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();

    assert_eq!(
        before_daily,
        snapshot_table(
            &conn,
            "SELECT * FROM usage_daily ORDER BY day_key, source_id"
        )
    );
    assert_eq!(
        before_cursor,
        snapshot_table(
            &conn,
            "SELECT * FROM usage_cursor ORDER BY source_id, session_id"
        )
    );
}

#[test]
fn csv_header_is_frozen_and_cells_are_escaped() {
    let conn = support::open_ledger_db("cost_csv");
    add_daily(&conn, "2026-10-17", "codex", "", "measured", "m", 5);
    let evil_name = "=SUM(\"a\",1)\nnext";
    put_config(
        &conn,
        json!([profile("A", evil_name)]),
        json!([billing_rule("r1", "codex", None, "m", "A")]),
        json!([pricing_rule(
            "p1", "codex", None, "m", "exact", "openai", "m"
        )]),
    );
    put_pricing(
        &conn,
        &catalog(json!({"m": priced_model("m", true)})),
        1_000,
    );

    let csv = cost_csv_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms()).unwrap();
    let header = "billing_profile_id,billing_profile_name,source_ids,observed_provider,model_id,attribution_kind,token_usage,estimated_usd,status,priced_tokens,unpriced_tokens,coverage_rate,pricing_provider,pricing_model,pricing_snapshot_id,pricing_basis,price_updated_at,channel_key,channel_kind,display_provider,normalized_model_id,pricing_release_date,price_override,price_rule_kind";
    assert!(csv.starts_with(&format!("{header}\n")));
    assert!(!csv.starts_with('\u{feff}'));

    let records = csv_records(&csv);
    assert_eq!(records.len(), 2, "header + 1 data row，无总计行");
    assert_eq!(records[0].len(), 24);
    assert_eq!(records[1].len(), 24, "引号内的逗号/换行不得破坏列数");
    assert_eq!(records[1][1], format!("'{evil_name}"), "公式前缀必须先中和");
    assert_eq!(records[1][2], "codex");
    assert_eq!(records[1][16], "1000");
    assert_eq!(records[1][23], "none", "无自定义规则必须标记 none");
    assert!(!records
        .iter()
        .any(|record| record.iter().any(|cell| cell.eq_ignore_ascii_case("total"))));
}

/// 公式注入的中和不能只钉 `=` 一种前缀：`csv_cell` 对 `= + - @ \t \r \n` 开头的值
/// 都必须补 `'` 前缀。参数化恶意渠道名逐前缀断言。
#[test]
fn csv_cells_neutralize_every_dangerous_formula_prefix() {
    for evil_name in ["+cmd", "-1", "@x", "\tev"] {
        let conn = support::open_ledger_db("cost_csv_injection");
        add_daily(&conn, "2026-10-17", "codex", "", "measured", "m", 5);
        put_config(
            &conn,
            json!([profile("A", evil_name)]),
            json!([billing_rule("r1", "codex", None, "m", "A")]),
            json!([pricing_rule(
                "p1", "codex", None, "m", "exact", "openai", "m"
            )]),
        );
        put_pricing(
            &conn,
            &catalog(json!({"m": priced_model("m", true)})),
            1_000,
        );

        let csv = cost_csv_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms()).unwrap();
        let records = csv_records(&csv);
        assert_eq!(records.len(), 2, "header + 1 data row");
        assert_eq!(
            records[1][1],
            format!("'{evil_name}"),
            "前缀 {evil_name:?} 必须被 `'` 中和"
        );
        // 中和后的单元格不得被表格软件按公式求值：原始危险前缀不能裸露在引号外。
        assert!(
            !records[1][1].starts_with(evil_name),
            "前缀 {evil_name:?} 不得原样输出"
        );
    }
}

#[test]
fn missing_snapshot_keeps_tokens_but_no_amount() {
    let conn = support::open_ledger_db("cost_no_snapshot");
    add_daily(&conn, "2026-10-17", "codex", "", "measured", "m", 5);

    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    let row = &costs.rows[0];
    assert_eq!(row.token_usage, 15);
    assert_eq!(row.estimated_usd, None);
    assert_eq!(row.status, CostStatus::Unpriced);
    assert_eq!(row.priced_tokens, 0);
    assert_eq!(row.unpriced_tokens, 15);
    assert_eq!(row.coverage_rate, Some(0.0));
    assert!(row.reasons.iter().any(|reason| reason == "noPrice"));
    assert_eq!(row.pricing_snapshot_id, None);
    assert_eq!(row.price_updated_at, None);
    assert!(costs.pricing.stale);
    assert_eq!(costs.pricing.snapshot_id, None);

    let csv = cost_csv_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms()).unwrap();
    let records = csv_records(&csv);
    assert_eq!(records[1][7], "", "缺金额必须是空字段，不能写 0");
    assert_eq!(records[1][8], "unpriced");
    assert_eq!(records[1][11], "0.000000");
}

#[test]
fn daily_only_custom_range_is_queryable() {
    let conn = support::open_ledger_db("cost_daily_only");
    add_daily(&conn, "2026-01-01", "codex", "", "measured", "m", 5);
    let custom = UsageRange {
        preset: UsageRangePreset::Custom,
        from: Some("2026-01-01".to_string()),
        to: Some("2026-01-01".to_string()),
    };
    let costs = costs_with_conn(&conn, &custom, now_ms(), None).unwrap();
    assert_eq!(costs.rows.len(), 1, "日聚合-only 历史区间必须可查询");
    assert_eq!(costs.rows[0].token_usage, 15);
}

#[test]
fn conditional_quote_is_priced_at_base_rate_with_reason() {
    const CONDITIONAL: &str =
        include_str!("../../tests/fixtures/usage-v2/pricing/conditional.json");
    let conn = support::open_ledger_db("cost_conditional");
    add_daily(&conn, "2026-10-17", "codex", "p", "measured", "m", 5);
    // 2026-10-20 Task 3：自动匹配收窄到精选来源，非精选 provider p 不再被自动命中；
    // 本用例改走 PricingRule（精确键路径与作用域无关）。
    put_config(
        &conn,
        json!([]),
        json!([]),
        json!([pricing_rule(
            "p1",
            "codex",
            Some("p"),
            "m",
            "exact",
            "p",
            "m"
        )]),
    );
    put_pricing(&conn, CONDITIONAL, 1_000);

    // 2026-10-20 条件价放宽：多模态/上下文阶梯不再导致整行未估价——
    // 费用行用基础文本价估算，reasons 标注 conditionalBaseRate。
    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    let row = &costs.rows[0];
    // input 10 × $2 + output 5 × $10 = $0.00007（基础文本价，阶梯价不参与）。
    assert_eq!(row.estimated_usd.as_deref(), Some("0.00007"));
    assert_eq!(
        row.status,
        CostStatus::Priced,
        "四桶全部按基础价计价 ⇒ priced（不是 unpriced）"
    );
    assert_eq!(row.pricing_basis, PricingBasis::BaseCurrent);
    assert_eq!(row.priced_tokens, 15);
    assert_eq!(row.unpriced_tokens, 0);
    assert!(
        row.reasons
            .iter()
            .any(|reason| reason == "conditionalBaseRate"),
        "条件价按基础价估算必须带 conditionalBaseRate：{:?}",
        row.reasons
    );
    assert!(
        !row.reasons
            .iter()
            .any(|reason| reason == "unpricedComponent"),
        "条件价不再产生未估价分量：{:?}",
        row.reasons
    );

    // 双币种面：无原生人民币价 ⇒ CNY 按兜底汇率 7.2 换算（converted=true）。
    // 10 × ¥14.4 + 5 × ¥72 = ¥0.000504。
    assert_eq!(row.estimated_cny.as_deref(), Some("0.000504"));
    assert_eq!(row.status_cny, CostStatus::Priced);
    assert_eq!(row.priced_tokens_cny, 15);
    assert_eq!(row.coverage_rate_cny, Some(1.0));
    assert!(!row.usd_converted, "USD 是目录原生币种，不得标 converted");
    assert!(row.cny_converted, "CNY 由 USD 换算而来，必须标 converted");
    let input = row.components.input.as_ref().unwrap();
    assert!(!input.usd_converted);
    assert!(input.cny_converted);
    assert_eq!(input.unit_usd_per_million.as_deref(), Some("2"));
    assert_eq!(input.unit_cny_per_million.as_deref(), Some("14.4"));
    // wire 形态：CostComponent 的 USD/CNY 单价与金额 + 逐分量 converted 标记。
    let input_wire = serde_json::to_value(row.components.input.as_ref().unwrap()).unwrap();
    assert_eq!(
        obj_keys(&input_wire),
        vec![
            "cnyConverted",
            "estimatedCny",
            "estimatedUsd",
            "tokens",
            "unitCnyPerMillion",
            "unitUsdPerMillion",
            "usdConverted"
        ],
        "CostComponent 的 wire 字段逐字冻结（双币种面）"
    );
}

#[test]
fn partial_price_coverage_is_conserved() {
    let conn = support::open_ledger_db("cost_partial");
    // 带 cache_write 的两行（request_total = fresh+read+write），缺 cache_write 价。
    conn.execute(
        "INSERT INTO usage_daily
         (day_key, source_id, project_key, provider, provider_kind, model,
          input_fresh, cache_read, cache_write, output, request_total, requests,
          user_est, updated_at)
         VALUES ('2026-10-17','codex','','', 'measured','m', 10, 0, 2, 5, 12, 1, NULL, 0)",
        [],
    )
    .unwrap();
    put_config(
        &conn,
        json!([profile("A", "Profile A")]),
        json!([billing_rule("r1", "codex", None, "m", "A")]),
        json!([pricing_rule(
            "p1", "codex", None, "m", "exact", "openai", "m"
        )]),
    );
    put_pricing(
        &conn,
        &catalog(json!({"m": priced_model("m", false)})),
        1_000,
    );

    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    let row = &costs.rows[0];
    assert_eq!(row.status, CostStatus::Partial);
    assert_eq!(row.priced_tokens + row.unpriced_tokens, row.token_usage);
    let expected = row.priced_tokens as f64 / row.token_usage as f64;
    assert!((row.coverage_rate.unwrap() - expected).abs() < 1e-12);
}

// ── Task 10 复审修复 ────────────────────────────────────────────────────────

#[test]
fn all_missing_rates_cannot_claim_base_current() {
    const MISSING: &str = include_str!("../../tests/fixtures/usage-v2/pricing/missing.json");
    let conn = support::open_ledger_db("cost_all_missing");
    add_daily(&conn, "2026-10-17", "codex", "p", "measured", "m", 5);
    // 同上：非精选 provider 经 PricingRule 精确键取价（全缺价 ⇒ noPrice + 候选 reference）。
    put_config(
        &conn,
        json!([]),
        json!([]),
        json!([pricing_rule(
            "p1",
            "codex",
            Some("p"),
            "m",
            "exact",
            "p",
            "m"
        )]),
    );
    put_pricing(&conn, MISSING, 1_000);

    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    let row = &costs.rows[0];
    assert_eq!(row.pricing_basis, PricingBasis::Insufficient);
    assert_eq!(row.estimated_usd, None);
    assert_eq!(row.priced_tokens, 0);
    assert_eq!(row.unpriced_tokens, 15);
    assert!(row.reasons.iter().any(|reason| reason == "noPrice"));
    assert_eq!(
        row.pricing_reference
            .as_ref()
            .map(|reference| reference.model_id.as_str()),
        Some("m"),
        "PricingRule 候选 reference 必须如实记录"
    );
}

#[test]
fn estimate_overflow_degrades_only_that_row() {
    let conn = support::open_ledger_db("cost_estimate_overflow");
    // 费率 × 百万级 token 溢出 i128：该行必须降级为 Unpriced，而不是整表 Err。
    // 27 个 9（scaled=1e36）原本只撑爆估算层；修复前 USD→CNY 换算会先在
    // currency.rs::usd_to_cny 溢出并令整表 Err——现已按分量降级，本用例即回归钉。
    conn.execute(
        "INSERT INTO usage_daily
         (day_key, source_id, project_key, provider, provider_kind, model,
          input_fresh, cache_read, cache_write, output, request_total, requests,
          user_est, updated_at)
         VALUES ('2026-10-17','codex','','p','measured','m', 1000000, 0, 0, 0, 1000000, 1, NULL, 0)",
        [],
    )
    .unwrap();
    let huge_catalog = r#"{"openai":{"models":{"m":{"cost":{"input":999999999999999999999999999},"modalities":{"input":["text"],"output":["text"]}}}}}"#;
    put_pricing(&conn, huge_catalog, 1_000);

    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    assert_eq!(costs.rows.len(), 1);
    let row = &costs.rows[0];
    assert_eq!(row.token_usage, 1_000_000);
    assert_eq!(row.status, CostStatus::Unpriced);
    assert_eq!(row.priced_tokens, 0);
    assert_eq!(row.unpriced_tokens, 1_000_000);
    assert_eq!(row.coverage_rate, Some(0.0));
    assert_eq!(row.estimated_usd, None);
    assert_eq!(
        row.coverage_rate_cny,
        Some(0.0),
        "CNY 口径同样降级（不因换算成功而虚报覆盖率）"
    );
    assert!(row
        .reasons
        .iter()
        .any(|reason| reason == "estimateOverflow"));
}

#[test]
fn token_usage_overflow_also_degrades_instead_of_failing_the_table() {
    let conn = support::open_ledger_db("cost_usage_overflow");
    conn.execute(
        "INSERT INTO usage_daily
         (day_key, source_id, project_key, provider, provider_kind, model,
          input_fresh, cache_read, cache_write, output, request_total, requests,
          user_est, updated_at)
         VALUES ('2026-10-17','codex','','','measured','huge', 9223372036854775807, 0, 0, 1, 9223372036854775807, 1, NULL, 0)",
        [],
    )
    .unwrap();

    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    let row = &costs.rows[0];
    assert_eq!(row.token_usage, i64::MAX);
    assert_eq!(row.status, CostStatus::Unpriced);
    assert_eq!(row.priced_tokens, 0);
    assert_eq!(row.unpriced_tokens, i64::MAX);
    assert!(row
        .reasons
        .iter()
        .any(|reason| reason == "estimateOverflow"));
}

#[test]
fn pricing_reference_prefers_exact_over_longest_prefix() {
    let conn = support::open_ledger_db("cost_exact_prefix");
    add_daily(&conn, "2026-10-17", "codex", "", "measured", "gpt-5", 5);
    put_config(
        &conn,
        json!([profile("A", "Profile A")]),
        json!([billing_rule("r1", "codex", None, "gpt-5", "A")]),
        json!([
            pricing_rule("short", "codex", None, "gpt", "prefix", "openai", "short"),
            pricing_rule("exact", "codex", None, "gpt-5", "exact", "openai", "exact")
        ]),
    );
    put_pricing(
        &conn,
        &catalog(json!({
            "short": priced_model("short", true),
            "exact": priced_model("exact", true)
        })),
        1_000,
    );

    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    assert_eq!(
        costs.rows[0]
            .pricing_reference
            .as_ref()
            .map(|reference| reference.model_id.as_str()),
        Some("exact")
    );
}

#[test]
fn same_rank_reference_conflict_is_ambiguous_and_unpriced() {
    let conn = support::open_ledger_db("cost_ref_conflict");
    add_daily(&conn, "2026-10-17", "codex", "", "measured", "m", 5);
    put_config(
        &conn,
        json!([profile("A", "Profile A")]),
        json!([billing_rule("r1", "codex", None, "m", "A")]),
        json!([
            pricing_rule("a", "codex", None, "m", "exact", "openai", "m1"),
            pricing_rule("b", "codex", None, "m", "exact", "openai", "m2")
        ]),
    );

    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    let row = &costs.rows[0];
    assert_eq!(row.pricing_reference, None);
    assert_eq!(row.status, CostStatus::Unpriced);
    assert_eq!(row.estimated_usd, None);
    assert!(row
        .reasons
        .iter()
        .any(|reason| reason == "pricingReferenceAmbiguous"));
}

#[test]
fn inferred_or_empty_provider_now_uses_automatic_match() {
    let conn = support::open_ledger_db("cost_no_alias");
    add_daily(&conn, "2026-10-17", "codex", "p", "inferred", "m", 5);
    add_daily(&conn, "2026-10-17", "claude", "", "measured", "m", 7);
    put_pricing(
        &conn,
        &catalog(json!({"m": priced_model("m", true)})),
        1_000,
    );

    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    assert_eq!(costs.rows.len(), 2);
    for row in &costs.rows {
        // Task 5 起自动匹配不依赖 provider_kind：归一化精确命中即可估价。
        assert_eq!(
            row.pricing_reference
                .as_ref()
                .map(|reference| reference.model_id.as_str()),
            Some("m")
        );
        assert_eq!(row.normalized_model_id.as_deref(), Some("m"));
        assert!(row.estimated_usd.is_some());
    }
}

#[test]
fn csv_null_coverage_is_an_empty_cell() {
    let conn = support::open_ledger_db("cost_csv_null_coverage");
    conn.execute(
        "INSERT INTO usage_daily
         (day_key, source_id, project_key, provider, provider_kind, model,
          input_fresh, cache_read, cache_write, output, request_total, requests,
          user_est, updated_at)
         VALUES ('2026-10-17','codex','','','measured','zero', 0, 0, 0, 0, 0, 0, NULL, 0)",
        [],
    )
    .unwrap();

    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    assert_eq!(costs.rows[0].token_usage, 0);
    assert_eq!(costs.rows[0].coverage_rate, None);

    let csv = cost_csv_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms()).unwrap();
    let records = csv_records(&csv);
    assert_eq!(records[1].len(), 24);
    assert_eq!(records[1][11], "", "coverage null 必须是空字段");
    assert_eq!(records[1][7], "", "amount null 必须是空字段");
}

// ── Task 5：自动定价、显示渠道行与手动覆盖价 ─────────────────────────────────

fn priced_model_with_release(model_name: &str, release_date: &str) -> Value {
    let mut value = priced_model(model_name, true);
    value["release_date"] = json!(release_date);
    value
}

fn override_request(
    normalized_model_id: &str,
    input: Option<&str>,
    cache_read: Option<&str>,
    cache_write: Option<&str>,
    output: Option<&str>,
) -> PriceOverrideRequest {
    PriceOverrideRequest {
        normalized_model_id: normalized_model_id.to_string(),
        currency: PriceCurrency::Usd,
        input: input.map(str::to_string),
        cache_read: cache_read.map(str::to_string),
        cache_write: cache_write.map(str::to_string),
        output: output.map(str::to_string),
        time_rule: None,
    }
}

#[test]
fn automatic_match_prices_without_pricing_rule() {
    let conn = support::open_ledger_db("cost_auto_match");
    add_daily(
        &conn,
        "2026-10-17",
        "codex",
        "openai",
        "measured",
        "openai/gpt-4o-2024-08-06",
        5,
    );
    put_pricing(
        &conn,
        &catalog(json!({
            "gpt-4o-2024-08-06":
                priced_model_with_release("gpt-4o-2024-08-06", "2024-08-06")
        })),
        1_000,
    );

    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    assert_eq!(costs.rows.len(), 1);
    let row = &costs.rows[0];
    assert_eq!(row.pricing_basis, PricingBasis::BaseCurrent);
    assert_eq!(
        row.pricing_reference
            .as_ref()
            .map(|reference| (reference.provider_id.as_str(), reference.model_id.as_str())),
        Some(("openai", "gpt-4o-2024-08-06"))
    );
    assert_eq!(row.normalized_model_id.as_deref(), Some("gpt-4o"));
    assert_eq!(
        row.observed_providers,
        vec!["openai".to_string()],
        "费用行必须保留原始 provider 证据"
    );
    assert_eq!(row.pricing_release_date.as_deref(), Some("2024-08-06"));
    assert!(row.pricing_snapshot_id.is_some());
    assert_eq!(row.price_updated_at, Some(1_000));
    assert_eq!(row.estimated_usd.as_deref(), Some("0.00007"));
    assert!(!row.price_override);
    assert_eq!(row.channel_key, "provider:openai");
    assert_eq!(row.channel_kind, ChannelKind::Provider);
    assert_eq!(row.display_provider.as_deref(), Some("openai"));
}

#[test]
fn same_normalized_model_different_channels_split_with_identical_unit_prices() {
    let conn = support::open_ledger_db("cost_channel_split");
    add_daily(
        &conn,
        "2026-10-17",
        "codex",
        "openai",
        "measured",
        "gpt-4o-2024-08-06",
        5,
    );
    add_daily(
        &conn,
        "2026-10-17",
        "claude",
        "azure",
        "measured",
        "gpt-4o-2024-08-06",
        7,
    );
    put_pricing(
        &conn,
        &catalog(json!({
            "gpt-4o-2024-08-06":
                priced_model_with_release("gpt-4o-2024-08-06", "2024-08-06")
        })),
        1_000,
    );

    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    assert_eq!(costs.rows.len(), 2, "不同显示渠道即使同模型也必须分行");
    let mut channels: Vec<&str> = costs
        .rows
        .iter()
        .map(|row| row.channel_key.as_str())
        .collect();
    channels.sort_unstable();
    assert_eq!(channels, vec!["provider:azure", "provider:openai"]);
    for row in &costs.rows {
        assert_eq!(row.channel_kind, ChannelKind::Provider);
        assert_eq!(row.normalized_model_id.as_deref(), Some("gpt-4o"));
        assert_eq!(
            row.components
                .input
                .as_ref()
                .and_then(|component| component.unit_usd_per_million.as_deref()),
            Some("2"),
            "同一归一化模型在不同渠道必须共用同一单价"
        );
    }
    assert!(costs.rows.iter().all(|row| row.estimated_usd.is_some()));
}

#[test]
fn same_display_channel_across_tools_merges_into_one_row() {
    let conn = support::open_ledger_db("cost_channel_merge");
    add_daily(&conn, "2026-10-17", "codex", "openai", "measured", "m", 5);
    add_daily(&conn, "2026-10-17", "claude", "openai", "measured", "m", 7);
    put_pricing(
        &conn,
        &catalog(json!({"m": priced_model("m", true)})),
        1_000,
    );

    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    assert_eq!(costs.rows.len(), 1, "同显示渠道跨工具必须合并");
    let row = &costs.rows[0];
    assert_eq!(row.channel_key, "provider:openai");
    assert_eq!(row.token_usage, 32);
    assert_eq!(
        row.observed_providers,
        vec!["openai".to_string()],
        "同原始 provider 跨工具合并后证据唯一"
    );
    let mut sources: Vec<&str> = row.source_ids.iter().map(|source| source.db_id()).collect();
    sources.sort_unstable();
    assert_eq!(sources, vec!["claude", "codex"]);
}

#[test]
fn unassigned_rows_keep_tool_context_in_row_key() {
    let conn = support::open_ledger_db("cost_unassigned_context");
    add_daily(&conn, "2026-10-17", "codex", "", "measured", "m", 5);
    add_daily(&conn, "2026-10-17", "claude", "", "measured", "m", 7);

    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    assert_eq!(costs.rows.len(), 2, "未指定渠道仍按工具上下文拆行");
    assert!(costs.rows.iter().all(|row| row.channel_key == "unassigned"
        && row.channel_kind == ChannelKind::Unassigned
        && row.observed_providers.is_empty()));
    assert_ne!(costs.rows[0].key, costs.rows[1].key);
}

#[test]
fn merged_alias_channel_keeps_every_raw_provider() {
    let conn = support::open_ledger_db("cost_alias_merge_evidence");
    add_daily(&conn, "2026-10-17", "codex", "openai", "measured", "m", 5);
    add_daily(
        &conn,
        "2026-10-17",
        "claude",
        "azure-openai",
        "measured",
        "m",
        7,
    );
    conn.execute(
        "INSERT INTO usage_provider_alias
         (id, kind, provider_raw, source_id, model_id, display_name, created_at, updated_at)
         VALUES ('a1', 'provider', 'openai', NULL, NULL, 'OpenAI', 0, 0),
                ('a2', 'provider', 'azure-openai', NULL, NULL, 'OpenAI', 0, 0)",
        [],
    )
    .unwrap();
    put_pricing(
        &conn,
        &catalog(json!({"m": priced_model("m", true)})),
        1_000,
    );

    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    assert_eq!(costs.rows.len(), 1, "同显示名跨原始 provider 合并为一行");
    let row = &costs.rows[0];
    assert_eq!(row.channel_key, "provider:OpenAI");
    assert_eq!(row.channel_kind, ChannelKind::Alias);
    assert_eq!(
        row.observed_providers,
        vec!["azure-openai".to_string(), "openai".to_string()],
        "合并渠道的费用行必须保留全部去重后的原始 provider"
    );
    assert!(
        row.identity.observed_provider.is_none(),
        "多个原始 provider 合并后单值证据必须为空"
    );
    assert!(
        !row.has_empty_provider,
        "组内没有空 provider 行时 has_empty_provider 为 false"
    );
}

#[test]
fn cost_row_flags_group_with_empty_provider_rows() {
    let conn = support::open_ledger_db("cost_mixed_empty_provider");
    add_daily(&conn, "2026-10-17", "codex", "openai", "measured", "m", 5);
    add_daily(&conn, "2026-10-17", "claude", "", "measured", "m", 7);
    conn.execute(
        "INSERT INTO usage_provider_alias
         (id, kind, provider_raw, source_id, model_id, display_name, created_at, updated_at)
         VALUES ('a1', 'provider', 'openai', NULL, NULL, 'OpenAI', 0, 0),
                ('a2', 'toolModel', NULL, 'claude', 'm', 'OpenAI', 0, 0)",
        [],
    )
    .unwrap();

    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    assert_eq!(costs.rows.len(), 1, "别名把空 provider 行并入同一渠道");
    let row = &costs.rows[0];
    assert_eq!(row.channel_key, "provider:OpenAI");
    assert_eq!(
        row.observed_providers,
        vec!["openai".to_string()],
        "非空原始 provider 仍完整保留"
    );
    assert!(
        row.has_empty_provider,
        "组内混入空 provider 行时必须显式标记证据不完整"
    );
    assert!(
        row.identity.observed_provider.is_none(),
        "唯一非空串不得伪装成完整单值证据"
    );
}

#[test]
fn csv_observed_provider_is_blank_for_merged_or_unlabeled_evidence() {
    // 场景 1：两个原始 provider 合并为同一显示渠道 → 单值列留空。
    let conn = support::open_ledger_db("cost_csv_merged_provider");
    add_daily(&conn, "2026-10-17", "codex", "openai", "measured", "m", 5);
    add_daily(
        &conn,
        "2026-10-17",
        "claude",
        "azure-openai",
        "measured",
        "m",
        7,
    );
    conn.execute(
        "INSERT INTO usage_provider_alias
         (id, kind, provider_raw, source_id, model_id, display_name, created_at, updated_at)
         VALUES ('a1', 'provider', 'openai', NULL, NULL, 'OpenAI', 0, 0),
                ('a2', 'provider', 'azure-openai', NULL, NULL, 'OpenAI', 0, 0)",
        [],
    )
    .unwrap();
    let csv = cost_csv_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms()).unwrap();
    let records = csv_records(&csv);
    assert_eq!(records.len(), 2, "header + 合并后的 1 行");
    assert_eq!(
        records[1].len(),
        24,
        "固定 24 列契约（末尾追加 price_rule_kind）"
    );
    assert_eq!(
        records[1][3], "",
        "多个原始 provider 合并时 observed_provider 列必须留空"
    );

    // 场景 2：唯一非空 provider 混入空 provider 行 → 同样留空。
    let conn = support::open_ledger_db("cost_csv_unlabeled_provider");
    add_daily(&conn, "2026-10-17", "codex", "openai", "measured", "m", 5);
    add_daily(&conn, "2026-10-17", "claude", "", "measured", "m", 7);
    conn.execute(
        "INSERT INTO usage_provider_alias
         (id, kind, provider_raw, source_id, model_id, display_name, created_at, updated_at)
         VALUES ('a1', 'provider', 'openai', NULL, NULL, 'OpenAI', 0, 0),
                ('a2', 'toolModel', NULL, 'claude', 'm', 'OpenAI', 0, 0)",
        [],
    )
    .unwrap();
    let csv = cost_csv_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms()).unwrap();
    let records = csv_records(&csv);
    assert_eq!(records.len(), 2, "header + 混合后的 1 行");
    assert_eq!(records[1].len(), 24);
    assert_eq!(
        records[1][3], "",
        "存在未标注行时 observed_provider 列必须留空"
    );
}

#[test]
fn ambiguous_rows_keep_tool_context_in_row_key() {
    let conn = support::open_ledger_db("cost_ambiguous_context");
    add_daily(&conn, "2026-10-17", "codex", "", "measured", "m", 5);
    add_daily(&conn, "2026-10-17", "claude", "", "measured", "m", 7);
    put_config(
        &conn,
        json!([profile("A", "Profile A"), profile("B", "Profile B")]),
        json!([
            billing_rule("r1", "codex", None, "m", "A"),
            billing_rule("r2", "codex", None, "m", "B"),
            billing_rule("r3", "claude", None, "m", "A"),
            billing_rule("r4", "claude", None, "m", "B")
        ]),
        json!([]),
    );

    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    assert_eq!(costs.rows.len(), 2, "渠道可能混合仍按工具上下文拆行");
    assert!(costs
        .rows
        .iter()
        .all(|row| row.channel_key == "ambiguous" && row.channel_kind == ChannelKind::Ambiguous));
    assert_ne!(costs.rows[0].key, costs.rows[1].key);
}

#[test]
fn user_pricing_rule_still_wins_over_automatic_match() {
    let conn = support::open_ledger_db("cost_rule_priority");
    add_daily(
        &conn,
        "2026-10-17",
        "codex",
        "openai",
        "measured",
        "gpt-4o-2024-08-06",
        5,
    );
    put_config(
        &conn,
        json!([profile("A", "Profile A")]),
        json!([billing_rule(
            "r1",
            "codex",
            Some("openai"),
            "gpt-4o-2024-08-06",
            "A"
        )]),
        json!([pricing_rule(
            "p1",
            "codex",
            Some("openai"),
            "gpt-4o-2024-08-06",
            "exact",
            "openai",
            "custom"
        )]),
    );
    put_pricing(
        &conn,
        &catalog(json!({
            "gpt-4o-2024-08-06": priced_model("gpt-4o-2024-08-06", true),
            "custom": priced_model("custom", true)
        })),
        1_000,
    );

    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    let row = &costs.rows[0];
    assert_eq!(
        row.pricing_reference
            .as_ref()
            .map(|reference| reference.model_id.as_str()),
        Some("custom"),
        "用户 PricingRule 必须优先于自动匹配"
    );
    assert_eq!(row.channel_kind, ChannelKind::BillingProfile);
}

#[test]
fn unique_noise_prefix_fallback_is_used() {
    let conn = support::open_ledger_db("cost_prefix_unique");
    add_daily(
        &conn,
        "2026-10-17",
        "codex",
        "p",
        "measured",
        "claude-3-5-sonnet-beta",
        5,
    );
    put_pricing(
        &conn,
        &catalog(json!({"claude-3-5-sonnet": priced_model("claude-3-5-sonnet", true)})),
        1_000,
    );

    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    let row = &costs.rows[0];
    assert_eq!(row.pricing_basis, PricingBasis::BaseCurrent);
    assert_eq!(
        row.pricing_reference
            .as_ref()
            .map(|reference| reference.model_id.as_str()),
        Some("claude-3-5-sonnet")
    );
    assert_eq!(
        row.normalized_model_id.as_deref(),
        Some("claude-3-5-sonnet")
    );
    assert!(row
        .reasons
        .iter()
        .all(|reason| reason != "pricingPrefixAmbiguous"));
}

#[test]
fn ambiguous_noise_prefix_is_unpriced_with_reason() {
    let conn = support::open_ledger_db("cost_prefix_ambiguous");
    add_daily(
        &conn,
        "2026-10-17",
        "codex",
        "p",
        "measured",
        "qwen-alpha-alpha",
        5,
    );
    // 两个候选同模型族但费率不同：不得猜价。
    let payload = catalog(json!({
        "qwen": json!({"id": "qwen", "cost": {"input": 1},
            "modalities": {"input": ["text"], "output": ["text"]}}),
        "qwen-alpha": json!({"id": "qwen-alpha", "cost": {"input": 2},
            "modalities": {"input": ["text"], "output": ["text"]}})
    }));
    put_pricing(&conn, &payload, 1_000);

    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    let row = &costs.rows[0];
    assert_eq!(row.pricing_reference, None);
    assert_eq!(row.pricing_basis, PricingBasis::Insufficient);
    assert_eq!(row.estimated_usd, None);
    assert!(
        row.reasons
            .iter()
            .any(|reason| reason == "pricingPrefixAmbiguous"),
        "多候选异价必须输出 pricingPrefixAmbiguous：{:?}",
        row.reasons
    );
    assert!(!row.reasons.iter().any(|reason| reason == "noPrice"));
}

#[test]
fn manual_override_applies_over_catalog_and_survives_refresh_failure() {
    let mut conn = support::open_ledger_db("cost_override");
    add_daily(
        &conn,
        "2026-10-17",
        "codex",
        "openai",
        "measured",
        "gpt-4o",
        5,
    );
    put_pricing(
        &conn,
        &catalog(json!({"gpt-4o": priced_model("gpt-4o", true)})),
        1_000,
    );
    pricing::set_override_with_conn(
        &mut conn,
        &override_request("gpt-4o", Some("5"), None, None, None),
        now_ms(),
    )
    .unwrap();
    let overrides_before = pricing::overrides_with_conn(&conn, now_ms()).unwrap();

    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    let row = &costs.rows[0];
    assert!(row.price_override);
    assert_eq!(row.pricing_basis, PricingBasis::UserOverride);
    assert_eq!(
        row.price_rule_kind,
        multi_agents_manager_lib::services::usage::v2::model::PriceRuleKind::Fixed,
        "固定价覆盖必须标记 fixed"
    );
    assert_eq!(row.time_breakdown, None);
    assert_eq!(
        row.components
            .input
            .as_ref()
            .and_then(|component| component.unit_usd_per_million.as_deref()),
        Some("5"),
        "覆盖分量必须生效"
    );
    assert_eq!(
        row.components
            .cache_write
            .as_ref()
            .and_then(|component| component.unit_usd_per_million.as_deref()),
        Some("3"),
        "未覆盖的分量继续使用目录费率"
    );
    assert!(row.pricing_reference.is_some());
    assert!(row.pricing_snapshot_id.is_some());
    let amount_before = row.estimated_usd.clone();

    pricing::refresh_with_conn(&mut conn, now_ms(), true, || {
        Err(UsageError::internal("offline in test".to_string()))
    })
    .unwrap();

    assert_eq!(
        pricing::overrides_with_conn(&conn, now_ms()).unwrap(),
        overrides_before,
        "刷新失败不得改写覆盖表"
    );
    let after = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    assert_eq!(after.rows[0].estimated_usd, amount_before);
    assert!(after.rows[0].price_override);
}

#[test]
fn manual_override_prices_without_catalog_and_hides_source_fields() {
    let mut conn = support::open_ledger_db("cost_override_no_catalog");
    add_daily(&conn, "2026-10-17", "codex", "", "measured", "m", 5);
    pricing::set_override_with_conn(
        &mut conn,
        &override_request("m", Some("5"), None, None, Some("20")),
        now_ms(),
    )
    .unwrap();

    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    let row = &costs.rows[0];
    assert!(row.price_override);
    assert_eq!(row.pricing_basis, PricingBasis::UserOverride);
    assert_eq!(
        row.price_rule_kind,
        multi_agents_manager_lib::services::usage::v2::model::PriceRuleKind::Fixed
    );
    assert_eq!(row.estimated_usd.as_deref(), Some("0.00015"));
    assert_eq!(row.status, CostStatus::Priced);
    assert_eq!(row.pricing_reference, None, "纯覆盖价不得冒充目录来源");
    assert_eq!(row.pricing_snapshot_id, None);
    assert_eq!(row.price_updated_at, None);
    assert_eq!(row.pricing_release_date, None);
    assert_eq!(
        row.normalized_model_id.as_deref(),
        Some("m"),
        "纯覆盖价保留覆盖键的归一化 ID（供核对），但不冒充目录 reference"
    );
}

// ── 分时价格（2026-10-21）──────────────────────────────────────────────────

/// DeepSeek 两个高峰窗口（09–12、14–18）：高峰小时用高峰价、午休与夜间用低谷价，
/// `timeBreakdown` 与加权单价必须按真实小时分布给出。
#[test]
fn time_override_uses_peak_and_offpeak_rates_per_window() {
    let mut conn = support::open_ledger_db("cost_time_windows");
    for hour in ["09", "11", "14", "17"] {
        add_detail_buckets(
            &conn,
            &format!("2026-10-16T{hour}"),
            "codex",
            "deepseek",
            "deepseek-chat",
            1_000_000,
            0,
            0,
            0,
        );
    }
    for hour in ["08", "12", "18", "23"] {
        add_detail_buckets(
            &conn,
            &format!("2026-10-16T{hour}"),
            "codex",
            "deepseek",
            "deepseek-chat",
            1_000_000,
            0,
            0,
            0,
        );
    }
    pricing::set_override_with_conn(
        &mut conn,
        &timed_override_request(
            "deepseek-chat",
            "5",
            None,
            Some("2"),
            None,
            TimePriceTimezone::Local,
            &[("09:00", "12:00"), ("14:00", "18:00")],
        ),
        now_ms(),
    )
    .unwrap();

    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    assert_eq!(costs.rows.len(), 1, "同一渠道/模型聚合成一行");
    let row = &costs.rows[0];
    assert_eq!(row.token_usage, 8_000_000);
    assert!(row.price_override);
    assert_eq!(row.pricing_basis, PricingBasis::UserOverride);
    assert_eq!(row.price_rule_kind, PriceRuleKind::Time);
    let breakdown = row
        .time_breakdown
        .as_ref()
        .expect("分时行必须带 timeBreakdown");
    assert_eq!(breakdown.peak_tokens, 4_000_000, "09/11/14/17 为高峰");
    assert_eq!(breakdown.offpeak_tokens, 4_000_000, "08/12/18/23 为低谷");
    assert_eq!(breakdown.daily_fallback_tokens, 0);
    assert_eq!(row.estimated_usd.as_deref(), Some("28"), "4M×5 + 4M×2 = 28");
    let input = row.components.input.as_ref().expect("input 必须计价");
    assert_eq!(input.tokens, 8_000_000);
    assert_eq!(
        input.unit_usd_per_million.as_deref(),
        Some("3.5"),
        "跨小时单价必须用金额/Token 加权，不得取最后一个小时的费率"
    );
    assert_eq!(input.estimated_usd.as_deref(), Some("28"));
    assert!(
        !row.reasons
            .iter()
            .any(|reason| reason == "timePriceDailyFallback"),
        "小时明细可用时不得走日聚合回退：{:?}",
        row.reasons
    );

    let csv = cost_csv_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms()).unwrap();
    let records = csv_records(&csv);
    assert_eq!(records[1].len(), 24);
    assert_eq!(records[1][23], "time", "CSV 末列必须标记分时价");
}

/// 手动显式 0 价是最优先来源：不得被目录价补洞（与自动匹配跳过全 0 相反）。
#[test]
fn manual_explicit_zero_rate_is_used_instead_of_catalog_price() {
    let mut conn = support::open_ledger_db("cost_override_zero");
    add_detail_buckets(
        &conn,
        "2026-10-16T10",
        "codex",
        "openai",
        "m",
        1_000_000,
        0,
        0,
        0,
    );
    put_pricing(
        &conn,
        &catalog(json!({"m": priced_model("m", true)})),
        1_000,
    );
    pricing::set_override_with_conn(
        &mut conn,
        &override_request("m", Some("0"), None, None, None),
        now_ms(),
    )
    .unwrap();

    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    let row = &costs.rows[0];
    assert!(row.price_override);
    assert_eq!(row.pricing_basis, PricingBasis::UserOverride);
    assert_eq!(row.price_rule_kind, PriceRuleKind::Fixed);
    assert_eq!(
        row.estimated_usd.as_deref(),
        Some("0"),
        "显式 0 价金额就是 0"
    );
    assert_eq!(
        row.components
            .input
            .as_ref()
            .unwrap()
            .unit_usd_per_million
            .as_deref(),
        Some("0"),
        "不得被目录 input=2 补洞"
    );
}

/// 跨午夜窗口（22–02）：工作日深夜与次日凌晨都算高峰，周六深夜/周日凌晨不算。
#[test]
fn time_override_handles_cross_midnight_and_weekend_boundaries() {
    let mut conn = support::open_ledger_db("cost_time_cross_midnight");
    for hour in [
        "2026-10-15T23", // Thursday
        "2026-10-16T01", // Friday, from Thursday
        "2026-10-17T01", // Saturday, from Friday
    ] {
        add_detail_buckets(
            &conn,
            hour,
            "codex",
            "deepseek",
            "deepseek-chat",
            1_000_000,
            0,
            0,
            0,
        );
    }
    for hour in ["2026-10-16T12", "2026-10-16T21", "2026-10-17T12"] {
        add_detail_buckets(
            &conn,
            hour,
            "codex",
            "deepseek",
            "deepseek-chat",
            1_000_000,
            0,
            0,
            0,
        );
    }
    pricing::set_override_with_conn(
        &mut conn,
        &timed_override_request(
            "deepseek-chat",
            "5",
            None,
            Some("1"),
            None,
            TimePriceTimezone::Local,
            &[("22:00", "02:00")],
        ),
        now_ms(),
    )
    .unwrap();

    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    let row = &costs.rows[0];
    let breakdown = row.time_breakdown.as_ref().unwrap();
    assert_eq!(breakdown.peak_tokens, 3_000_000);
    assert_eq!(breakdown.offpeak_tokens, 3_000_000);
    assert_eq!(row.estimated_usd.as_deref(), Some("18"), "3M×5 + 3M×1 = 18");
    assert_eq!(
        row.components
            .input
            .as_ref()
            .unwrap()
            .unit_usd_per_million
            .as_deref(),
        Some("3")
    );
}

/// 日聚合回退（无小时明细）：无法区分峰谷 ⇒ 整段按低谷价估算，标注 reason 与
/// `dailyFallbackTokens`；低谷缺项的分量回退目录价。
#[test]
fn time_override_daily_fallback_uses_offpeak_and_reports_reason() {
    let mut conn = support::open_ledger_db("cost_time_daily_fallback");
    add_daily(&conn, "2026-10-16", "codex", "", "measured", "m", 5);
    put_pricing(
        &conn,
        &catalog(json!({"m": priced_model("m", true)})),
        1_000,
    );
    pricing::set_override_with_conn(
        &mut conn,
        &timed_override_request(
            "m",
            "5",
            Some("20"),
            Some("1"),
            None,
            TimePriceTimezone::Local,
            &[("09:00", "12:00")],
        ),
        now_ms(),
    )
    .unwrap();

    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    let row = &costs.rows[0];
    assert_eq!(row.token_usage, 15);
    assert_eq!(row.price_rule_kind, PriceRuleKind::Time);
    let breakdown = row.time_breakdown.as_ref().expect("回退行仍属分时规则");
    assert_eq!(breakdown.peak_tokens, 0);
    assert_eq!(breakdown.offpeak_tokens, 0);
    assert_eq!(breakdown.daily_fallback_tokens, 15);
    assert!(
        row.reasons
            .iter()
            .any(|reason| reason == "timePriceDailyFallback"),
        "日聚合回退必须标注 timePriceDailyFallback：{:?}",
        row.reasons
    );
    // input 10 tokens × 低谷 1；output 5 tokens × 目录 10（低谷未给 output）。
    assert_eq!(row.estimated_usd.as_deref(), Some("0.00006"));
    assert_eq!(
        row.components
            .input
            .as_ref()
            .unwrap()
            .unit_usd_per_million
            .as_deref(),
        Some("1"),
        "日聚合回退按低谷价估算"
    );
    assert_eq!(
        row.components
            .output
            .as_ref()
            .unwrap()
            .unit_usd_per_million
            .as_deref(),
        Some("10"),
        "低谷 output 缺项必须回退目录价"
    );
    assert!(row.pricing_reference.is_some(), "回退分量仍使用目录来源");
}

/// 小时明细下低谷缺项同样回退目录价（不是 0 价，也不是高峰价）。
#[test]
fn time_override_offpeak_missing_component_falls_back_to_catalog() {
    let mut conn = support::open_ledger_db("cost_time_offpeak_fallback");
    add_detail_buckets(
        &conn,
        "2026-10-16T12",
        "codex",
        "openai",
        "m",
        1_000_000,
        0,
        0,
        1_000_000,
    );
    put_pricing(
        &conn,
        &catalog(json!({"m": priced_model("m", true)})),
        1_000,
    );
    pricing::set_override_with_conn(
        &mut conn,
        &timed_override_request(
            "m",
            "5",
            Some("20"),
            Some("2"),
            None,
            TimePriceTimezone::Local,
            &[("09:00", "12:00")],
        ),
        now_ms(),
    )
    .unwrap();

    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    let row = &costs.rows[0];
    let breakdown = row.time_breakdown.as_ref().unwrap();
    assert_eq!(breakdown.offpeak_tokens, 2_000_000);
    assert_eq!(breakdown.peak_tokens, 0);
    assert_eq!(
        row.estimated_usd.as_deref(),
        Some("12"),
        "input 1M×低谷 2 + output 1M×目录 10 = 12"
    );
    assert_eq!(
        row.components
            .input
            .as_ref()
            .unwrap()
            .unit_usd_per_million
            .as_deref(),
        Some("2")
    );
    assert_eq!(
        row.components
            .output
            .as_ref()
            .unwrap()
            .unit_usd_per_million
            .as_deref(),
        Some("10")
    );
    assert_eq!(row.price_rule_kind, PriceRuleKind::Time);
    assert_eq!(row.pricing_basis, PricingBasis::UserOverride);

    // CSV 的 price_rule_kind 与行内标记一致（不依赖高峰判定）。
    let csv = cost_csv_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms()).unwrap();
    let records = csv_records(&csv);
    assert_eq!(records[1].len(), 24);
    assert_eq!(records[1][23], "time");
}

#[test]
fn csv_appends_channel_and_pricing_context_columns() {
    let mut conn = support::open_ledger_db("cost_csv_channels");
    add_daily(
        &conn,
        "2026-10-17",
        "codex",
        "openai",
        "measured",
        "gpt-4o-2024-08-06",
        5,
    );
    put_pricing(
        &conn,
        &catalog(json!({
            "gpt-4o-2024-08-06":
                priced_model_with_release("gpt-4o-2024-08-06", "2024-08-06")
        })),
        1_000,
    );
    pricing::set_override_with_conn(
        &mut conn,
        &override_request("gpt-4o", Some("5"), None, None, None),
        now_ms(),
    )
    .unwrap();

    let csv = cost_csv_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms()).unwrap();
    let header = "billing_profile_id,billing_profile_name,source_ids,observed_provider,model_id,attribution_kind,token_usage,estimated_usd,status,priced_tokens,unpriced_tokens,coverage_rate,pricing_provider,pricing_model,pricing_snapshot_id,pricing_basis,price_updated_at,channel_key,channel_kind,display_provider,normalized_model_id,pricing_release_date,price_override,price_rule_kind";
    assert!(csv.starts_with(&format!("{header}\n")));
    let records = csv_records(&csv);
    assert_eq!(records.len(), 2, "header + 1 data row，无总计行");
    assert_eq!(records[0].len(), 24);
    assert_eq!(records[1].len(), 24);
    assert_eq!(records[1][15], "userOverride");
    assert_eq!(records[1][17], "provider:openai");
    assert_eq!(records[1][18], "provider");
    assert_eq!(records[1][19], "openai");
    assert_eq!(records[1][20], "gpt-4o");
    assert_eq!(records[1][21], "2024-08-06");
    assert_eq!(records[1][22], "true");
    assert_eq!(records[1][23], "fixed", "CSV 末列必须标记固定价覆盖");
    assert!(!records
        .iter()
        .any(|record| record.iter().any(|cell| cell.eq_ignore_ascii_case("total"))));
}

// ── dsh 日档：只计价 output（2026-10-19 修复）──────────────────────────────

#[test]
fn dsh_day_row_prices_output_only() {
    let conn = support::open_ledger_db("cost_dsh_day_output_only");
    conn.execute(
        "INSERT INTO usage_daily
         (day_key, source_id, project_key, provider, provider_kind, model,
          input_fresh, cache_read, cache_write, output, request_total, requests,
          user_est, updated_at)
         VALUES ('2026-10-17','dsh','','edenai','measured','m', 100, 20, 10, 30, 130, 1, NULL, 0)",
        [],
    )
    .unwrap();
    put_pricing(
        &conn,
        &catalog(json!({"m": priced_model("m", true)})),
        1_000,
    );

    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    let row = &costs.rows[0];
    assert_eq!(row.token_usage, 160);
    assert_eq!(row.status, CostStatus::Partial, "只计价 output ⇒ partial");
    assert_eq!(row.priced_tokens, 30);
    assert_eq!(row.unpriced_tokens, 130);
    assert!(
        (row.coverage_rate.unwrap() - 30.0 / 160.0).abs() < 1e-12,
        "覆盖率分母仍是 requestTotal+output：{:?}",
        row.coverage_rate
    );
    assert_eq!(row.estimated_usd.as_deref(), Some("0.0003"));

    let output = row.components.output.as_ref().expect("output 必须计价");
    assert_eq!(output.tokens, 30);
    assert_eq!(output.unit_usd_per_million.as_deref(), Some("10"));
    assert_eq!(output.estimated_usd.as_deref(), Some("0.0003"));
    assert!(
        row.components.input.is_none()
            && row.components.cache_read.is_none()
            && row.components.cache_write.is_none(),
        "日聚合无法区分缓存语义：输入/缓存读/缓存写必须保持未估价"
    );
    assert!(
        row.reasons
            .iter()
            .any(|reason| reason == "untrustedInputBuckets"),
        "必须给出输入桶不可信的明确原因：{:?}",
        row.reasons
    );
    assert!(row
        .reasons
        .iter()
        .any(|reason| reason == "unpricedComponent"));
}

#[test]
fn dsh_day_negative_output_stays_unpriced() {
    let conn = support::open_ledger_db("cost_dsh_day_negative_output");
    conn.execute(
        "INSERT INTO usage_daily
         (day_key, source_id, project_key, provider, provider_kind, model,
          input_fresh, cache_read, cache_write, output, request_total, requests,
          user_est, updated_at)
         VALUES ('2026-10-17','dsh','','','measured','m', 10, 0, 0, -1, 10, 1, NULL, 0)",
        [],
    )
    .unwrap();
    put_pricing(
        &conn,
        &catalog(json!({"m": priced_model("m", true)})),
        1_000,
    );

    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    let row = &costs.rows[0];
    assert_eq!(row.status, CostStatus::Unpriced);
    assert_eq!(row.estimated_usd, None);
    assert_eq!(row.priced_tokens, 0);
    assert_eq!(row.token_usage, 9, "requestTotal 10 + output -1");
    assert!(row.components.output.is_none(), "负 output 不得计价");
}

#[test]
fn zcode_day_row_keeps_full_pricing() {
    let conn = support::open_ledger_db("cost_zcode_day_full");
    conn.execute(
        "INSERT INTO usage_daily
         (day_key, source_id, project_key, provider, provider_kind, model,
          input_fresh, cache_read, cache_write, output, request_total, requests,
          user_est, updated_at)
         VALUES ('2026-10-17','zcode','','','measured','m', 100, 20, 10, 30, 130, 1, NULL, 0)",
        [],
    )
    .unwrap();
    put_pricing(
        &conn,
        &catalog(json!({"m": priced_model("m", true)})),
        1_000,
    );

    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    let row = &costs.rows[0];
    assert_eq!(row.status, CostStatus::Priced, "其他源日档保持 full 行为");
    assert_eq!(row.priced_tokens, 160);
    assert_eq!(row.unpriced_tokens, 0);
    assert_eq!(row.estimated_usd.as_deref(), Some("0.00055"));
    assert!(
        row.components.input.is_some()
            && row.components.cache_read.is_some()
            && row.components.cache_write.is_some()
            && row.components.output.is_some(),
        "zcode 日档四个分量都必须计价"
    );
    assert!(!row
        .reasons
        .iter()
        .any(|reason| reason == "untrustedInputBuckets"));
}

#[test]
fn dsh_hour_row_keeps_full_pricing() {
    let conn = support::open_ledger_db("cost_dsh_hour_full");
    add_detail(&conn, "2026-10-17", "2026-10-17T11", "dsh", "edenai");
    put_pricing(
        &conn,
        &catalog(json!({"m": priced_model("m", true)})),
        1_000,
    );

    // now = 2026-10-17 12:00 → Last5h 的完整小时桶为 07..11。
    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last5h), now_ms(), None).unwrap();
    assert_eq!(costs.rows.len(), 1);
    let row = &costs.rows[0];
    assert_eq!(row.status, CostStatus::Priced, "dsh 小时档保持 full 行为");
    assert_eq!(row.priced_tokens, 160);
    assert_eq!(row.unpriced_tokens, 0);
    assert_eq!(row.estimated_usd.as_deref(), Some("0.00055"));
    assert!(
        row.components.input.is_some()
            && row.components.cache_read.is_some()
            && row.components.cache_write.is_some()
            && row.components.output.is_some(),
        "dsh 小时档四个分量都必须计价"
    );
    assert!(!row
        .reasons
        .iter()
        .any(|reason| reason == "untrustedInputBuckets"));
}

// ── 2026-10-20 计划 Task 4：小时明细聚合计费（Detail/Daily 整段回退）──────────

/// 明细行夹具：四桶齐全且 `request_total = fresh + read + write`（互斥推导成立）。
#[allow(clippy::too_many_arguments)]
fn add_detail_row(
    conn: &Connection,
    day: &str,
    source: &str,
    provider: &str,
    model: &str,
    fresh: i64,
    read: i64,
    write: i64,
    output: i64,
    request_total: i64,
) {
    conn.execute(
        "INSERT INTO usage_detail
         (source_id, session_id, project_key, hour_key, day_key, model, provider,
          provider_kind, input_fresh, cache_read, cache_write, output,
          request_total, requests, user_est, first_seen_at, updated_at)
         VALUES (?1, 's1', '', ?2, ?3, ?4, ?5, 'measured', ?6, ?7, ?8, ?9, ?10, 1, NULL, 0, 0)",
        params![
            source,
            format!("{day}T10"),
            day,
            model,
            provider,
            fresh,
            read,
            write,
            output,
            request_total
        ],
    )
    .unwrap();
}

/// 明细全覆盖夹具：3 天 ×（codex/openai/gpt-5 + dsh/zhipu/gpt-5），
/// usage_daily 同形同值（Detail 路径的权威数据是 usage_detail）。
fn seed_detail_covered(conn: &Connection) {
    for (index, day) in ["2026-10-15", "2026-10-16", "2026-10-17"]
        .iter()
        .enumerate()
    {
        let fresh = 10 + index as i64;
        // codex 行：四桶齐全。
        conn.execute(
            "INSERT INTO usage_daily
             (day_key, source_id, project_key, provider, provider_kind, model,
              input_fresh, cache_read, cache_write, output, request_total, requests,
              user_est, updated_at)
             VALUES (?1, 'codex', '', 'openai', 'measured', 'gpt-5', ?2, 4, 2, 6, ?3, 1, NULL, 0)",
            params![day, fresh, fresh + 6],
        )
        .unwrap();
        add_detail_row(
            conn,
            day,
            "codex",
            "openai",
            "gpt-5",
            fresh,
            4,
            2,
            6,
            fresh + 6,
        );
        // dsh 行：四桶齐全（Detail 路径下应全量计价）。
        conn.execute(
            "INSERT INTO usage_daily
             (day_key, source_id, project_key, provider, provider_kind, model,
              input_fresh, cache_read, cache_write, output, request_total, requests,
              user_est, updated_at)
             VALUES (?1, 'dsh', '', 'zhipu', 'measured', 'gpt-5', ?2, 2, 0, 3, ?3, 1, NULL, 0)",
            params![day, fresh, fresh + 2],
        )
        .unwrap();
        add_detail_row(
            conn,
            day,
            "dsh",
            "zhipu",
            "gpt-5",
            fresh,
            2,
            0,
            3,
            fresh + 2,
        );
    }
}

#[test]
fn detail_full_coverage_prices_all_buckets_including_dsh() {
    let conn = support::open_ledger_db("cost_detail_full");
    seed_detail_covered(&conn);
    put_pricing(
        &conn,
        &catalog(json!({"gpt-5": priced_model("gpt-5", true)})),
        1_000,
    );

    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    assert_eq!(costs.rows.len(), 2, "codex 与 dsh 各一行");
    for row in &costs.rows {
        assert_eq!(row.status, CostStatus::Priced, "{:?}", row.identity);
        assert_eq!(
            row.pricing_basis,
            PricingBasis::BaseCurrent,
            "明细路径走目录基准价"
        );
        assert!(
            !row.reasons.iter().any(|reason| reason == "untrustedSource"),
            "明细四桶可信：不得出现 untrustedSource"
        );
        assert!(
            !row.reasons
                .iter()
                .any(|reason| reason == "untrustedInputBuckets"),
            "明细路径 dsh 输入/缓存桶可计价"
        );
        assert!(
            row.components.input.is_some()
                && row.components.cache_read.is_some()
                && row.components.cache_write.is_some()
                && row.components.output.is_some(),
            "四个分量全价"
        );
    }
    // dsh 行：全价（旧日档 output-only 下 input 分量必为 null——本断言锁明细增益）。
    let dsh_source = multi_agents_manager_lib::services::usage::model::UsageSourceId::Dsh;
    let dsh = costs
        .rows
        .iter()
        .find(|row| row.identity.source_id == dsh_source)
        .unwrap();
    let input = dsh.components.input.as_ref().unwrap();
    assert_eq!(
        input.tokens,
        10 + 11 + 12,
        "明细三天 input 聚合（10+11+12）"
    );
    // codex 行 token 总量 = Σ(request_total + output) = (16+6)+(17+6)+(18+6) = 69。
    let codex_source = multi_agents_manager_lib::services::usage::model::UsageSourceId::Codex;
    let codex = costs
        .rows
        .iter()
        .find(|row| row.identity.source_id == codex_source)
        .unwrap();
    assert_eq!(
        codex.token_usage,
        16 + 17 + 18 + 3 * 6,
        "requestTotal(16+17+18) + output(6×3)"
    );
}

#[test]
fn missing_detail_day_falls_back_to_daily_for_the_whole_range() {
    let conn = support::open_ledger_db("cost_detail_gap");
    seed_detail_covered(&conn);
    // 删掉一天的明细 ⇒ daily 天集 ⊄ detail 天集 ⇒ 整段回退日聚合（不做逐日混合）。
    conn.execute("DELETE FROM usage_detail WHERE day_key = '2026-10-16'", [])
        .unwrap();
    put_pricing(
        &conn,
        &catalog(json!({"gpt-5": priced_model("gpt-5", true)})),
        1_000,
    );

    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    let dsh_source = multi_agents_manager_lib::services::usage::model::UsageSourceId::Dsh;
    let dsh = costs
        .rows
        .iter()
        .find(|row| row.identity.source_id == dsh_source)
        .unwrap();
    assert!(
        dsh.reasons.iter().any(|reason| reason == "untrustedSource")
            && dsh
                .reasons
                .iter()
                .any(|reason| reason == "untrustedInputBuckets"),
        "回退日聚合后 dsh 必须 output-only + reasons 披露：{:?}",
        dsh.reasons
    );
    assert!(
        dsh.components.input.is_none() && dsh.components.cache_read.is_none(),
        "回退路径 dsh 输入/缓存桶保持未估价"
    );
    assert!(dsh.components.output.is_some(), "output 仍单独计价");
    // 其余源两 provenance 均 trusted：codex 照常全价，总量 = usage_daily 三天行值。
    let codex_source = multi_agents_manager_lib::services::usage::model::UsageSourceId::Codex;
    let codex = costs
        .rows
        .iter()
        .find(|row| row.identity.source_id == codex_source)
        .unwrap();
    assert_eq!(codex.status, CostStatus::Priced);
    assert!(codex.components.input.is_some());
    assert_eq!(
        codex.token_usage,
        16 + 17 + 18 + 3 * 6,
        "回退日聚合与明细聚合同值（夹具同形）"
    );
}

#[test]
fn empty_detail_table_falls_back_without_panic() {
    let conn = support::open_ledger_db("cost_detail_empty");
    add_daily(
        &conn,
        "2026-10-17",
        "codex",
        "openai",
        "measured",
        "gpt-5",
        5,
    );
    put_pricing(
        &conn,
        &catalog(json!({"gpt-5": priced_model("gpt-5", true)})),
        1_000,
    );

    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    assert_eq!(costs.rows.len(), 1, "无明细行时整段回退日聚合，不 panic");
    assert_eq!(costs.rows[0].status, CostStatus::Priced);
}

/// 计价身份覆盖的四级优先级链（§3.3）：
/// 四价覆盖 > PricingRule > 计价身份覆盖 > Curated 自动匹配。
#[test]
fn pricing_identity_override_sits_between_rule_and_auto_match() {
    let mut conn = support::open_ledger_db("cost_identity_chain");
    add_daily(
        &conn,
        "2026-10-17",
        "codex",
        "openai",
        "measured",
        "gpt-5",
        5,
    );
    put_pricing(
        &conn,
        &catalog(json!({
            "gpt-5": priced_model("gpt-5", true),
            "gpt-5-custom": priced_model("gpt-5-custom", true),
            "rule-model": priced_model("rule-model", true)
        })),
        1_000,
    );
    let identity_request = |reference: Option<PricingReference>| PricingIdentityRequest {
        provider_raw: Some("openai".to_string()),
        source_id: None,
        model_id: "gpt-5".to_string(),
        reference,
    };
    let reference_of = |model: &str| {
        Some(PricingReference {
            provider_id: "openai".to_string(),
            model_id: model.to_string(),
        })
    };

    // (a) 仅计价身份覆盖 > 自动匹配：命中目录精确键 ⇒ baseCurrent + 自定义 reference。
    model_config::set_pricing_identity_with_conn(
        &conn,
        &identity_request(reference_of("gpt-5-custom")),
        1_000,
    )
    .unwrap();
    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    let row = &costs.rows[0];
    assert_eq!(
        row.pricing_reference.as_ref().unwrap().model_id,
        "gpt-5-custom",
        "计价身份覆盖压过 Curated 自动匹配"
    );
    assert_eq!(
        row.pricing_basis,
        PricingBasis::BaseCurrent,
        "身份覆盖仍走目录费率"
    );
    assert!(!row.price_override);

    // (b) PricingRule > 计价身份覆盖。
    put_config(
        &conn,
        json!([]),
        json!([]),
        json!([pricing_rule(
            "p1",
            "codex",
            Some("openai"),
            "gpt-5",
            "exact",
            "openai",
            "rule-model"
        )]),
    );
    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    assert_eq!(
        costs.rows[0].pricing_reference.as_ref().unwrap().model_id,
        "rule-model",
        "高级覆盖参考价规则压过计价身份"
    );

    // (c) 四价手动覆盖 > 一切：按原始模型归一化 ID 覆盖 output 分量。
    pricing::set_override_with_conn(
        &mut conn,
        &PriceOverrideRequest {
            normalized_model_id: "gpt-5".to_string(),
            currency: PriceCurrency::Usd,
            input: None,
            cache_read: None,
            cache_write: None,
            output: Some("99".to_string()),
            time_rule: None,
        },
        now_ms(),
    )
    .unwrap();
    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    let row = &costs.rows[0];
    assert_eq!(row.pricing_basis, PricingBasis::UserOverride);
    assert!(row.price_override);

    // (d) 身份覆盖指向目录缺失条目 ⇒ noPrice（保留候选 reference，不回退自动匹配）。
    let conn2 = support::open_ledger_db("cost_identity_ghost_quote");
    add_daily(
        &conn2,
        "2026-10-17",
        "codex",
        "openai",
        "measured",
        "gpt-5",
        5,
    );
    put_pricing(
        &conn2,
        &catalog(json!({"gpt-5": priced_model("gpt-5", true)})),
        1_000,
    );
    model_config::set_pricing_identity_with_conn(
        &conn2,
        &identity_request(reference_of("not-in-catalog")),
        1_000,
    )
    .unwrap();
    let costs = costs_with_conn(&conn2, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    let row = &costs.rows[0];
    assert_eq!(row.status, CostStatus::Unpriced);
    assert!(
        row.reasons.iter().any(|reason| reason == "noPrice"),
        "目录缺条目必须 noPrice：{:?}",
        row.reasons
    );
    assert_eq!(
        row.pricing_reference.as_ref().unwrap().model_id,
        "not-in-catalog",
        "noPrice 时必须保留候选 reference（不静默回退自动匹配）"
    );
}

/// displayModelName 解析（§3.5）：全部成员一致 ⇒ 生效；无人配置或分歧 ⇒ null。
/// 分歧面用「同显示名合并两个原始 provider」（openai + azure-openai → OpenAI）构造——
/// 两个**不同配置身份**落进同一费用行才是 §1 决策表的已知限制面。
#[test]
fn display_model_name_resolution_agrees_or_falls_back() {
    let conn = support::open_ledger_db("cost_display_model");
    add_daily(
        &conn,
        "2026-10-17",
        "codex",
        "openai",
        "measured",
        "gpt-5",
        5,
    );
    add_daily(
        &conn,
        "2026-10-17",
        "claude",
        "azure-openai",
        "measured",
        "gpt-5",
        7,
    );
    conn.execute(
        "INSERT INTO usage_provider_alias
         (id, kind, provider_raw, source_id, model_id, display_name, created_at, updated_at)
         VALUES ('a1', 'provider', 'openai', NULL, NULL, 'OpenAI', 0, 0),
                ('a2', 'provider', 'azure-openai', NULL, NULL, 'OpenAI', 0, 0)",
        [],
    )
    .unwrap();
    put_config(
        &conn,
        json!([profile("A", "Profile A")]),
        json!([
            billing_rule("r1", "codex", Some("openai"), "gpt-5", "A"),
            billing_rule("r2", "claude", Some("azure-openai"), "gpt-5", "A")
        ]),
        json!([]),
    );
    put_pricing(
        &conn,
        &catalog(json!({"gpt-5": priced_model("gpt-5", true)})),
        1_000,
    );
    let set_name = |provider: &str, name: Option<&str>| {
        model_config::set_display_name_with_conn(
            &conn,
            &ModelDisplayNameRequest {
                provider_raw: Some(provider.to_string()),
                source_id: None,
                model_id: "gpt-5".to_string(),
                display_name: name.map(str::to_string),
            },
            1_000,
        )
        .unwrap();
    };

    // 无人配置 ⇒ null。
    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    assert_eq!(costs.rows.len(), 1, "同显示渠道同模型合并为一行");
    assert_eq!(costs.rows[0].display_model_name, None);

    // 两成员一致 ⇒ 生效。
    set_name("openai", Some("My GPT"));
    set_name("azure-openai", Some("My GPT"));
    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    assert_eq!(
        costs.rows[0].display_model_name.as_deref(),
        Some("My GPT"),
        "全成员一致的自定义模型名必须生效"
    );

    // 分歧 ⇒ null（诚实降级，不猜）。
    set_name("azure-openai", Some("Other"));
    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    assert_eq!(
        costs.rows[0].display_model_name, None,
        "成员配置分歧必须回退 null"
    );

    // 单成员行：配置即生效。
    let conn2 = support::open_ledger_db("cost_display_model_single");
    add_daily(
        &conn2,
        "2026-10-17",
        "codex",
        "openai",
        "measured",
        "gpt-5",
        5,
    );
    put_pricing(
        &conn2,
        &catalog(json!({"gpt-5": priced_model("gpt-5", true)})),
        1_000,
    );
    model_config::set_display_name_with_conn(
        &conn2,
        &ModelDisplayNameRequest {
            provider_raw: Some("openai".to_string()),
            source_id: None,
            model_id: "gpt-5".to_string(),
            display_name: Some("Solo".to_string()),
        },
        1_000,
    )
    .unwrap();
    let costs = costs_with_conn(&conn2, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    assert_eq!(costs.rows[0].display_model_name.as_deref(), Some("Solo"));
}

/// 性能基准（#[ignore]：手动跑 `cargo test -- --ignored` 记录数据，不断言墙钟上限）：
/// 31 天 × ≥10 万 usage_detail 行，`costs_with_conn` 明细聚合路径的正确性 + 耗时打印。
#[test]
#[ignore = "性能基准：手动运行并记录行数/耗时（计划 §7）"]
fn benchmark_100k_detail_rows_costs_query() {
    let conn = support::open_ledger_db("cost_bench_100k");
    let days: i64 = 31;
    let sources = [
        "claude",
        "codex",
        "kimi",
        "opencode",
        "workbuddy",
        "zcode",
        "dsh",
    ];
    let models = ["m0", "m1", "m2", "m3", "m4", "m5"];
    let providers = ["p0", "p1", "p2", "p3", "p4", "p5", "p6", "p7"];
    let sessions = 80;
    let mut rows = 0i64;
    conn.execute("BEGIN", []).unwrap();
    for day_index in 0..days {
        let day = format!("2026-09-{:02}", 1 + day_index);
        for (source_index, source) in sources.iter().enumerate() {
            for (model_index, model) in models.iter().enumerate() {
                let provider = providers[(source_index + model_index) % providers.len()];
                for session in 0..sessions {
                    conn.execute(
                        "INSERT INTO usage_detail
                         (source_id, session_id, project_key, hour_key, day_key, model, provider,
                          provider_kind, input_fresh, cache_read, cache_write, output,
                          request_total, requests, user_est, first_seen_at, updated_at)
                         VALUES (?1, ?2, '', ?3, ?4, ?5, ?6, 'measured', 6, 2, 1, 3, 9, 1, NULL, 0, 0)",
                        params![
                            source,
                            format!("s{session}"),
                            format!("{day}T10"),
                            day,
                            model,
                            provider
                        ],
                    )
                    .unwrap();
                    rows += 1;
                }
            }
        }
    }
    conn.execute("COMMIT", []).unwrap();
    assert!(rows >= 100_000, "基准要求 ≥10 万明细行，实际 {rows}");

    // 全部模型都进精选目录（openai 官方）⇒ 明细路径全价。
    let mut models_json = json!({});
    for model in models {
        models_json[model] = priced_model(model, true);
    }
    put_pricing(&conn, &catalog(models_json), 1_000);

    let started = std::time::Instant::now();
    // 自定义 31 天窗口（上限 MAX_CUSTOM_SPAN_DAYS=31）：09-01..10-01 全部落在窗口内。
    let custom = UsageRange {
        preset: UsageRangePreset::Custom,
        from: Some("2026-09-01".to_string()),
        to: Some("2026-10-01".to_string()),
    };
    let costs = costs_with_conn(&conn, &custom, now_ms(), None).unwrap();
    let elapsed = started.elapsed();
    // 正确性：42 个身份组（7 源 × 6 模型）；每组 token =
    // 31 天 × 80 会话 × (request_total 9 + output 3) = 29760。
    assert_eq!(costs.rows.len(), sources.len() * models.len());
    for row in &costs.rows {
        assert_eq!(row.token_usage, 31 * 80 * (9 + 3));
        assert_eq!(row.status, CostStatus::Priced);
    }
    println!(
        "benchmark_100k_detail_rows_costs_query: detail_rows={rows} groups={} elapsed_ms={}",
        costs.rows.len(),
        elapsed.as_millis()
    );
}

// ── 2026-10-20 双币种费用行（原生人民币价 / CNY 覆盖价 / per-currency 口径）────

/// 种入 `usage_pricing_native` 的原生人民币价（替换 active 快照同名条目）。
fn put_native(
    conn: &mut Connection,
    snapshot_id: &str,
    provider_id: &str,
    model_id: &str,
    rates: [Option<i128>; 4],
) {
    use multi_agents_manager_lib::database::dao::usage_billing::{
        replace_native_prices_conn, NativePriceRecord,
    };
    let normalized = pricing::normalize_model_id(model_id);
    replace_native_prices_conn(
        conn,
        snapshot_id,
        &[NativePriceRecord {
            provider_id: provider_id.to_string(),
            model_id: model_id.to_string(),
            normalized_model_id: normalized,
            rates,
            source_url: format!("https://example.test/{provider_id}/{model_id}.toml"),
            fetched_at: 1_000,
        }],
    )
    .unwrap();
}

#[test]
fn native_cny_prices_win_without_conversion_flag_and_coverage_diverges_per_currency() {
    let mut conn = support::open_ledger_db("cost_native_cny");
    // openai（无人民币来源）与 zhipuai（有）各一行账本；glm 行带 1 个 cache_write
    // token（fresh 8 + read 1 + write 1 = request_total 10，无写重叠），
    // 让「USD 全价 / CNY 缺 cacheWrite 价」的口径分歧可见。
    add_daily(&conn, "2026-10-17", "codex", "openai", "measured", "m", 5);
    conn.execute(
        "INSERT INTO usage_daily
         (day_key, source_id, project_key, provider, provider_kind, model,
          input_fresh, cache_read, cache_write, output, request_total, requests,
          user_est, updated_at)
         VALUES ('2026-10-17','codex','','','measured','glm-5.3', 8, 1, 1, 5, 10, 1, NULL, 0)",
        [],
    )
    .unwrap();
    let payload = r#"{
        "openai": {"models": {"m": {"cost": {"input": 2, "output": 10},
            "modalities": {"input": ["text"], "output": ["text"]}}}},
        "zhipuai": {"models": {"glm-5.3": {"cost": {"input": 2, "cache_read": 1,
            "cache_write": 3, "output": 10},
            "modalities": {"input": ["text"], "output": ["text"]}}}}
    }"#;
    put_pricing(&conn, payload, 1_000);
    let snapshot_id =
        multi_agents_manager_lib::database::dao::usage_billing::load_active_snapshot_meta_conn(
            &conn,
        )
        .unwrap()
        .map(|meta| meta.0)
        .expect("active 快照必须在场");

    // 原生人民币价只覆盖 input/cacheRead/output（无 cacheWrite）——
    // USD 目录价齐全 ⇒ CNY 口径缺一个分量，状态与覆盖率必须分币种各自成立。
    put_native(
        &mut conn,
        &snapshot_id,
        "zhipuai",
        "glm-5.3",
        [
            Some(8_000_000_000),
            Some(2_000_000_000),
            None,
            Some(28_000_000_000),
        ],
    );

    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    assert_eq!(costs.rows.len(), 2);
    let glm = costs
        .rows
        .iter()
        .find(|row| row.identity.model_id == "glm-5.3")
        .unwrap();
    // 原生人民币价生效：input 8 × ¥8 + read 1 × ¥2 + output 5 × ¥28 = ¥0.000206，
    // 不是 USD 价 × 7.2（那会是 ¥0.0014976）。
    assert_eq!(glm.estimated_cny.as_deref(), Some("0.000206"));
    assert!(
        !glm.cny_converted,
        "原生人民币分量不得标 converted（转换面才标）"
    );
    let glm_input = glm.components.input.as_ref().unwrap();
    assert!(!glm_input.cny_converted);
    assert_eq!(glm_input.unit_cny_per_million.as_deref(), Some("8"));
    assert_eq!(glm_input.unit_usd_per_million.as_deref(), Some("2"));
    assert!(!glm_input.usd_converted, "USD 目录原生分量不得标 converted");
    // USD 口径全价（15/15）；CNY 口径 cacheWrite 缺原生价 ⇒ Partial（14/15）。
    assert_eq!(glm.status, CostStatus::Priced);
    assert_eq!(glm.priced_tokens, 15);
    assert_eq!(glm.coverage_rate, Some(1.0));
    assert_eq!(
        glm.status_cny,
        CostStatus::Partial,
        "CNY 口径缺 cacheWrite 价 ⇒ 独立降级为 partial"
    );
    assert_eq!(glm.priced_tokens_cny, 14);
    assert_eq!(glm.unpriced_tokens_cny, 1);
    let expected_cny_coverage = 14.0 / 15.0;
    assert!((glm.coverage_rate_cny.unwrap() - expected_cny_coverage).abs() < 1e-12);

    // openai（无人民币来源）⇒ CNY 按 7.2 兜底换算并标 converted。
    let openai = costs
        .rows
        .iter()
        .find(|row| row.identity.model_id == "m")
        .unwrap();
    assert_eq!(openai.estimated_usd.as_deref(), Some("0.00007"));
    assert_eq!(openai.estimated_cny.as_deref(), Some("0.000504"));
    assert!(openai.cny_converted);
    assert!(!openai.usd_converted);
    assert_eq!(costs.fx_rate.as_deref(), Some("7.2"));
    assert_eq!(costs.fx_source.as_deref(), Some("default"));
    assert!(costs.fx_fallback);
}

#[test]
fn cny_override_prices_convert_into_usd_row_with_converted_flag() {
    let conn = support::open_ledger_db("cost_cny_override");
    add_daily(&conn, "2026-10-17", "codex", "", "measured", "m", 5);
    put_config(&conn, json!([]), json!([]), json!([]));
    put_pricing(
        &conn,
        &catalog(json!({"m": priced_model("m", true)})),
        1_000,
    );
    // CNY 覆盖价：input ¥8（约 $1.11 @7.2）；其余分量回落目录 USD 价。
    let mut mutable = conn;
    pricing::set_override_with_conn(
        &mut mutable,
        &PriceOverrideRequest {
            normalized_model_id: "m".to_string(),
            currency: PriceCurrency::Cny,
            input: Some("8".to_string()),
            cache_read: None,
            cache_write: None,
            output: None,
            time_rule: None,
        },
        2_000,
    )
    .unwrap();
    let conn = mutable;

    let costs = costs_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms(), None).unwrap();
    let row = &costs.rows[0];
    let input = row.components.input.as_ref().unwrap();
    // CNY 覆盖价原样落 CNY 面（不标 converted）；USD 面换算（8/7.2 ≈ 1.111111111）标 converted。
    assert_eq!(input.unit_cny_per_million.as_deref(), Some("8"));
    assert!(!input.cny_converted, "覆盖价自身币种的分量不得标 converted");
    assert_eq!(input.unit_usd_per_million.as_deref(), Some("1.111111111"));
    assert!(input.usd_converted, "换算出的 USD 分量必须标 converted");
    assert!(row.usd_converted);
    // output 分量回落目录 USD 价（10），其 CNY 面按 7.2 换算（72）标 converted。
    let output = row.components.output.as_ref().unwrap();
    assert_eq!(output.unit_usd_per_million.as_deref(), Some("10"));
    assert!(!output.usd_converted);
    assert_eq!(output.unit_cny_per_million.as_deref(), Some("72"));
    assert!(output.cny_converted);
    assert_eq!(row.pricing_basis, PricingBasis::UserOverride);
    assert!(row.price_override);
}
