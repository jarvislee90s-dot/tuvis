//! Task 3 集成测试：计费配置的版本提交、撤销、校验与账本守恒。
//! 全部使用 `support::open_ledger_db` 的用例私有库，不触碰真实账本。

mod support;

use multi_agents_manager_lib::database::dao::usage_billing as dao;
use multi_agents_manager_lib::services::usage::model::{UsageRange, UsageRangePreset};
use multi_agents_manager_lib::services::usage::v2::billing;
use multi_agents_manager_lib::services::usage::v2::model::{BillingCommit, BillingConfig};
use multi_agents_manager_lib::services::usage::v2::pricing;
use multi_agents_manager_lib::services::usage::UsageError;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Value};

fn valid_config_value() -> Value {
    json!({
        "revision": 0,
        "profiles": [{
            "id": "profile-a", "name": "工作套餐", "kind": "api",
            "note": "", "archived": false
        }],
        "rules": [{
            "id": "rule-a",
            "identity": {
                "sourceId": "codex", "observedProvider": null, "modelId": "gpt-5"
            },
            "match": "exact", "billingProfileId": "profile-a",
            "effectiveFrom": 0, "effectiveTo": null, "mixed": false
        }],
        "pricingRules": []
    })
}

fn valid_config() -> BillingConfig {
    serde_json::from_value(valid_config_value()).unwrap()
}

fn assert_invalid(err: UsageError, label: &str) {
    assert_eq!(
        err.code, "usage-settings-invalid",
        "{label} 应返回 usage-settings-invalid，实际 {err:?}"
    );
}

/// 历史快照里的 revision 列表（升序）。用于断言失败路径没有多写脏历史行。
fn revision_rows(conn: &Connection) -> Vec<i64> {
    let mut stmt = conn
        .prepare("SELECT revision FROM usage_billing_revision ORDER BY revision")
        .unwrap();
    stmt.query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

/// Plan Task 3 Step 1 的逐字调用序列。
#[test]
fn commit_and_undo_follow_the_plan_sequence() {
    let mut conn = support::open_ledger_db("billing_revision");
    let original = billing::load_with_conn(&conn).unwrap();
    assert_eq!(original.revision, 0);
    let candidate: BillingConfig = serde_json::from_value(json!({
     "revision":0,"profiles":[{"id":"profile-a","name":"工作套餐","kind":"api",
     "note":"","archived":false}],"rules":[],"pricingRules":[]
    }))
    .unwrap();
    let request = BillingCommit {
        expected_revision: 0,
        candidate,
    };
    let saved = billing::commit_with_conn(&mut conn, &request, 1000).unwrap();
    assert_eq!(saved.revision, 1);
    assert!(billing::commit_with_conn(&mut conn, &request, 1001).is_err());
    let undone = billing::undo_with_conn(&mut conn, 1, 0, 1002).unwrap();
    assert_eq!(undone.revision, 2);
    assert!(billing::load_with_conn(&conn).unwrap().profiles.is_empty());
}

#[test]
fn empty_state_loads_revision_zero_and_empty_config() {
    let conn = support::open_ledger_db("billing_empty");
    let config = billing::load_with_conn(&conn).unwrap();
    assert_eq!(config.revision, 0);
    assert!(config.profiles.is_empty());
    assert!(config.rules.is_empty());
    assert!(config.pricing_rules.is_empty());
}

#[test]
fn revision_conflict_has_exact_detail() {
    let mut conn = support::open_ledger_db("billing_conflict");
    let request = BillingCommit {
        expected_revision: 0,
        candidate: valid_config(),
    };
    billing::commit_with_conn(&mut conn, &request, 1000).unwrap();

    let err = billing::commit_with_conn(&mut conn, &request, 1001).unwrap_err();
    assert_invalid(err.clone(), "旧 revision");
    assert_eq!(err.detail, "billing revision conflict");
    assert_eq!(
        revision_rows(&conn),
        vec![1],
        "冲突失败不得追加历史行（不能出现 revision 2）"
    );
}

#[test]
fn committed_config_round_trips_through_history_and_state() {
    let mut conn = support::open_ledger_db("billing_roundtrip");
    let request = BillingCommit {
        expected_revision: 0,
        candidate: valid_config(),
    };
    let saved = billing::commit_with_conn(&mut conn, &request, 1000).unwrap();
    assert_eq!(saved.committed_at, 1000);

    let loaded = billing::load_with_conn(&conn).unwrap();
    let mut expected = valid_config();
    expected.revision = 1;
    assert_eq!(loaded.revision, 1);
    assert_eq!(loaded, expected);

    let historical = dao::load_revision_conn(&conn, 1).unwrap().unwrap();
    assert_eq!(historical.revision, 1);
    assert_eq!(historical, expected);
    assert!(dao::load_revision_conn(&conn, 0).unwrap().is_none());
    assert!(dao::load_revision_conn(&conn, 2).unwrap().is_none());
}

#[test]
fn undo_to_missing_target_is_rejected_and_keeps_current_state() {
    let mut conn = support::open_ledger_db("billing_undo_missing");
    let request = BillingCommit {
        expected_revision: 0,
        candidate: valid_config(),
    };
    billing::commit_with_conn(&mut conn, &request, 1000).unwrap();

    let err = billing::undo_with_conn(&mut conn, 1, 99, 1001).unwrap_err();
    assert_invalid(err, "撤销到不存在的 revision");
    assert_eq!(billing::load_with_conn(&conn).unwrap().revision, 1);
    assert_eq!(
        revision_rows(&conn),
        vec![1],
        "目标缺失的撤销不得追加历史行（不能出现 revision 2）"
    );
}

#[test]
fn undo_with_stale_expected_is_rejected() {
    let mut conn = support::open_ledger_db("billing_undo_conflict");
    let request = BillingCommit {
        expected_revision: 0,
        candidate: valid_config(),
    };
    billing::commit_with_conn(&mut conn, &request, 1000).unwrap();

    let err = billing::undo_with_conn(&mut conn, 0, 0, 1001).unwrap_err();
    assert_invalid(err.clone(), "撤销的旧 expected");
    assert_eq!(err.detail, "billing revision conflict");
    assert_eq!(billing::load_with_conn(&conn).unwrap().revision, 1);
}

#[test]
fn undo_to_previous_content_creates_a_new_revision() {
    let mut conn = support::open_ledger_db("billing_undo_content");
    let first = BillingCommit {
        expected_revision: 0,
        candidate: valid_config(),
    };
    billing::commit_with_conn(&mut conn, &first, 1000).unwrap();

    let mut second_candidate = valid_config();
    second_candidate.profiles[0].name = "第二版".to_string();
    let second = BillingCommit {
        expected_revision: 1,
        candidate: second_candidate,
    };
    billing::commit_with_conn(&mut conn, &second, 1001).unwrap();
    assert_eq!(
        billing::load_with_conn(&conn).unwrap().profiles[0].name,
        "第二版"
    );

    let undone = billing::undo_with_conn(&mut conn, 2, 1, 1002).unwrap();
    assert_eq!(undone.revision, 3);
    let restored = billing::load_with_conn(&conn).unwrap();
    assert_eq!(restored.revision, 3);
    assert_eq!(restored.profiles[0].name, "工作套餐");
}

#[test]
fn invalid_candidate_is_rejected_before_any_write() {
    let mut conn = support::open_ledger_db("billing_invalid");
    let mut candidate = valid_config();
    candidate.profiles[0].name = "   ".to_string();
    let request = BillingCommit {
        expected_revision: 0,
        candidate,
    };

    let err = billing::commit_with_conn(&mut conn, &request, 1000).unwrap_err();
    assert_invalid(err, "空名");
    assert_eq!(billing::load_with_conn(&conn).unwrap().revision, 0);
    assert!(
        revision_rows(&conn).is_empty(),
        "无效 candidate 必须在任何写库之前被拒绝"
    );
}

#[test]
fn corrupt_state_json_surfaces_as_db_failed() {
    let conn = support::open_ledger_db("billing_corrupt_state");
    conn.execute(
        "INSERT INTO usage_billing_state (singleton, revision, config_json)
         VALUES (1, 1, '{bad')",
        [],
    )
    .unwrap();

    let err = billing::load_with_conn(&conn).unwrap_err();
    assert_eq!(
        err.code, "usage-db-failed",
        "损坏 JSON 必须显式报错，不得吞成空配置"
    );
    assert!(
        err.detail.contains("billing config decode failed"),
        "detail 应指出解码失败，实际 {err:?}"
    );
}

#[test]
fn validate_config_rejects_each_invalid_shape() {
    let base = valid_config_value();

    let mut empty_name = base.clone();
    empty_name["profiles"][0]["name"] = json!("  ");
    assert_invalid(
        billing::validate_config(&serde_json::from_value(empty_name).unwrap()).unwrap_err(),
        "空名",
    );

    let mut duplicate_profile = base.clone();
    duplicate_profile["profiles"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "id": "profile-a", "name": "重复", "kind": "unknown",
            "note": "", "archived": false
        }));
    assert_invalid(
        billing::validate_config(&serde_json::from_value(duplicate_profile).unwrap()).unwrap_err(),
        "重复 profile id",
    );

    let mut duplicate_rule = base.clone();
    let mut rule = duplicate_rule["rules"][0].clone();
    rule["identity"]["modelId"] = json!("other-model");
    duplicate_rule["rules"].as_array_mut().unwrap().push(rule);
    assert_invalid(
        billing::validate_config(&serde_json::from_value(duplicate_rule).unwrap()).unwrap_err(),
        "重复 rule id",
    );

    let mut dangling = base.clone();
    dangling["rules"][0]["billingProfileId"] = json!("missing");
    assert_invalid(
        billing::validate_config(&serde_json::from_value(dangling).unwrap()).unwrap_err(),
        "悬空引用",
    );

    let mut reversed = base.clone();
    reversed["rules"][0]["effectiveFrom"] = json!(200);
    reversed["rules"][0]["effectiveTo"] = json!(100);
    assert_invalid(
        billing::validate_config(&serde_json::from_value(reversed).unwrap()).unwrap_err(),
        "日期逆序",
    );

    let mut zero_length = base.clone();
    zero_length["rules"][0]["effectiveFrom"] = json!(100);
    zero_length["rules"][0]["effectiveTo"] = json!(100);
    assert_invalid(
        billing::validate_config(&serde_json::from_value(zero_length).unwrap()).unwrap_err(),
        "零长度区间",
    );

    let mut empty_prefix = base;
    empty_prefix["rules"][0]["match"] = json!("prefix");
    empty_prefix["rules"][0]["identity"]["modelId"] = json!("");
    assert_invalid(
        billing::validate_config(&serde_json::from_value(empty_prefix).unwrap()).unwrap_err(),
        "空前缀",
    );
}

// ── 账本守恒 ─────────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
struct TableSnapshot {
    columns: Vec<String>,
    rows: Vec<Vec<String>>,
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

fn snapshot_table(conn: &Connection, sql: &str) -> TableSnapshot {
    let mut stmt = conn.prepare(sql).expect("快照查询必须可准备");
    let columns = stmt
        .column_names()
        .iter()
        .map(|name| (*name).to_string())
        .collect();
    let rows = stmt
        .query_map([], |row| {
            let mut values = Vec::new();
            for index in 0..row.as_ref().column_count() {
                values.push(render(row.get_ref(index)?));
            }
            Ok(values)
        })
        .expect("快照查询必须可执行")
        .collect::<Result<Vec<_>, _>>()
        .expect("快照行必须可读");
    TableSnapshot { columns, rows }
}

fn snapshot_ledger(conn: &Connection) -> Vec<TableSnapshot> {
    vec![
        snapshot_table(
            conn,
            "SELECT * FROM usage_detail
             ORDER BY source_id, session_id, project_key, hour_key, model, provider",
        ),
        snapshot_table(
            conn,
            "SELECT * FROM usage_daily
             ORDER BY day_key, source_id, project_key, provider, model",
        ),
        snapshot_table(
            conn,
            "SELECT * FROM usage_cursor ORDER BY source_id, session_id",
        ),
        snapshot_table(
            conn,
            "SELECT * FROM usage_session ORDER BY source_id, session_id",
        ),
    ]
}

#[test]
fn commit_and_undo_leave_the_ledger_byte_for_byte_unchanged() {
    let mut conn = support::open_ledger_db("billing_conservation");

    // 新表必须在私有库初始化后存在。
    for table in ["usage_billing_state", "usage_billing_revision"] {
        conn.prepare(&format!("SELECT * FROM {table} LIMIT 0"))
            .unwrap_or_else(|e| panic!("{table} 未由 schema::init 建出: {e}"));
    }

    conn.execute(
        "INSERT INTO usage_detail
         (source_id, session_id, project_key, hour_key, day_key, model, provider,
          provider_kind, input_fresh, cache_read, cache_write, output,
          request_total, requests, user_est, first_seen_at, updated_at)
         VALUES ('codex','s1','proj','2026-10-17T10','2026-10-17','gpt-5','openai',
                 'measured',10,2,3,4,12,1,5,100,200)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO usage_daily
         (day_key, source_id, project_key, provider, provider_kind, model,
          input_fresh, cache_read, cache_write, output, request_total, requests,
          user_est, updated_at)
         VALUES ('2026-10-17','codex','proj','openai','measured','gpt-5',
                 10,2,3,4,12,1,5,200)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO usage_cursor
         (source_id, session_id, fingerprint, byte_offset, ordinal, last_cumulative,
          mtime_ms, file_size, state_json, updated_at)
         VALUES ('codex','s1','fp',42,3,99,123,456,'{\"k\":1}',789)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO usage_session
         (source_id, session_id, project_key, project_label, project_path_raw,
          project_realpath, title, is_subagent, parent_session_id, first_seen_at,
          last_seen_at)
         VALUES ('codex','s1','proj','Proj','/tmp/p','/tmp/p','T',0,NULL,100,200)",
        [],
    )
    .unwrap();

    let before = snapshot_ledger(&conn);
    assert!(
        before.iter().all(|table| !table.rows.is_empty()),
        "前提：四张旧表都必须有数据，否则守恒断言是空的"
    );

    let request = BillingCommit {
        expected_revision: 0,
        candidate: valid_config(),
    };
    billing::commit_with_conn(&mut conn, &request, 1000).unwrap();
    billing::undo_with_conn(&mut conn, 1, 0, 1001).unwrap();

    let after = snapshot_ledger(&conn);
    assert_eq!(before, after, "提交/撤销计费配置不得改动任何旧账本行或游标");
    assert_eq!(billing::load_with_conn(&conn).unwrap().revision, 2);
}

// ── Task 17：定价引用与 active catalog 校验 ─────────────────────────────────

const VALID_CATALOG: &str = include_str!("../../tests/fixtures/usage-v2/pricing/valid.json");
const EMPTY_CATALOG: &str = r#"{"q":{"models":{}}}"#;
/// 只含 `legacy/old` 一个精确键的旧版目录（用于先提交合法引用、再换版将其下架）。
const DELISTED_CATALOG: &str = r#"{
    "legacy":{"models":{"old":{
        "cost":{"input":1,"output":2},
        "modalities":{"input":["text"],"output":["text"]}
    }}}
}"#;

fn billing_range() -> UsageRange {
    UsageRange {
        preset: UsageRangePreset::Last7d,
        from: None,
        to: None,
    }
}

fn config_with_pricing_reference(provider: &str, model: &str) -> BillingConfig {
    let mut config = valid_config();
    config.pricing_rules = vec![serde_json::from_value(json!({
        "id": format!("pricing-{provider}-{model}"),
        "identity": {"sourceId": "codex", "observedProvider": null, "modelId": "gpt-5"},
        "match": "exact",
        "reference": {"providerId": provider, "modelId": model}
    }))
    .unwrap()];
    config
}

fn activate_catalog(conn: &mut Connection, id: &str, payload: &str, fetched_at: i64) {
    dao::activate_snapshot_conn(conn, id, "https://models.dev/api.json", fetched_at, payload)
        .unwrap();
}

#[test]
fn new_pricing_rule_requires_an_active_catalog_for_preview_and_commit() {
    let conn = support::open_ledger_db("billing_pricing_no_catalog");
    let candidate = config_with_pricing_reference("p", "m");

    let preview_error =
        billing::preview_with_conn(&conn, &candidate, &billing_range(), 1_000).unwrap_err();
    assert_invalid(preview_error.clone(), "preview 无快照");
    assert_eq!(preview_error.detail, "pricing catalog unavailable");

    let mut conn = conn;
    let commit_error = billing::commit_with_conn(
        &mut conn,
        &BillingCommit {
            expected_revision: 0,
            candidate,
        },
        1_000,
    )
    .unwrap_err();
    assert_invalid(commit_error.clone(), "commit 无快照");
    assert_eq!(commit_error.detail, "pricing catalog unavailable");
    assert_eq!(billing::load_with_conn(&conn).unwrap().revision, 0);
}

#[test]
fn catalog_backed_pricing_reference_passes_preview_and_commit() {
    let mut conn = support::open_ledger_db("billing_pricing_ok");
    activate_catalog(&mut conn, "snap-valid", VALID_CATALOG, 1_000);
    let candidate = config_with_pricing_reference("p", "m");

    let preview = billing::preview_with_conn(&conn, &candidate, &billing_range(), 1_000).unwrap();
    assert_eq!(preview.base_revision, 0);
    assert_eq!(preview.candidate.revision, 1);

    let saved = billing::commit_with_conn(
        &mut conn,
        &BillingCommit {
            expected_revision: 0,
            candidate,
        },
        1_001,
    )
    .unwrap();
    assert_eq!(saved.revision, 1);
}

#[test]
fn unknown_pricing_reference_is_rejected_with_exact_detail() {
    let mut conn = support::open_ledger_db("billing_pricing_missing_key");
    activate_catalog(&mut conn, "snap-valid", VALID_CATALOG, 1_000);
    let candidate = config_with_pricing_reference("p", "missing");

    let preview_error =
        billing::preview_with_conn(&conn, &candidate, &billing_range(), 1_000).unwrap_err();
    assert_invalid(preview_error.clone(), "preview 键不存在");
    assert_eq!(
        preview_error.detail,
        "pricing reference not found: p/missing"
    );

    let commit_error = billing::commit_with_conn(
        &mut conn,
        &BillingCommit {
            expected_revision: 0,
            candidate,
        },
        1_000,
    )
    .unwrap_err();
    assert_invalid(commit_error.clone(), "commit 键不存在");
    assert_eq!(
        commit_error.detail,
        "pricing reference not found: p/missing"
    );
}

#[test]
fn unchanged_pricing_rules_can_be_saved_offline() {
    let mut conn = support::open_ledger_db("billing_pricing_unchanged_offline");
    activate_catalog(&mut conn, "snap-valid", VALID_CATALOG, 1_000);
    billing::commit_with_conn(
        &mut conn,
        &BillingCommit {
            expected_revision: 0,
            candidate: config_with_pricing_reference("p", "m"),
        },
        1_000,
    )
    .unwrap();

    // 目录下线后，仅修改非定价字段仍可保存（pricingRules Vec 未变）。
    conn.execute(
        "UPDATE usage_pricing_state SET active_snapshot_id = NULL",
        [],
    )
    .unwrap();
    let mut next = billing::load_with_conn(&conn).unwrap();
    next.profiles[0].name = "改名".to_string();
    let saved = billing::commit_with_conn(
        &mut conn,
        &BillingCommit {
            expected_revision: 1,
            candidate: next,
        },
        1_001,
    )
    .unwrap();
    assert_eq!(saved.revision, 2);
}

#[test]
fn undo_restores_delisted_pricing_reference_without_online_validation() {
    let mut conn = support::open_ledger_db("billing_pricing_undo_delisted");
    activate_catalog(&mut conn, "snap-valid", VALID_CATALOG, 1_000);
    let candidate = config_with_pricing_reference("p", "m");
    billing::commit_with_conn(
        &mut conn,
        &BillingCommit {
            expected_revision: 0,
            candidate: candidate.clone(),
        },
        1_000,
    )
    .unwrap();
    billing::commit_with_conn(
        &mut conn,
        &BillingCommit {
            expected_revision: 1,
            candidate,
        },
        1_001,
    )
    .unwrap();

    // 历史 revision 1 引用的 p/m 已从当前目录下架。
    activate_catalog(&mut conn, "snap-empty", EMPTY_CATALOG, 1_002);
    let undone = billing::undo_with_conn(&mut conn, 2, 1, 1_003).unwrap();
    assert_eq!(undone.revision, 3);
    let restored = billing::load_with_conn(&conn).unwrap();
    assert_eq!(restored.pricing_rules.len(), 1);
    assert_eq!(restored.pricing_rules[0].reference.provider_id, "p");
    assert_eq!(restored.pricing_rules[0].reference.model_id, "m");
}

fn two_pricing_rules() -> BillingConfig {
    let mut config = valid_config();
    config.pricing_rules = vec![
        serde_json::from_value(json!({
            "id": "pricing-a",
            "identity": {"sourceId": "codex", "observedProvider": null, "modelId": "gpt-5"},
            "match": "exact",
            "reference": {"providerId": "p", "modelId": "m"}
        }))
        .unwrap(),
        serde_json::from_value(json!({
            "id": "pricing-b",
            "identity": {"sourceId": "codex", "observedProvider": null, "modelId": "gpt-5"},
            "match": "prefix",
            "reference": {"providerId": "p", "modelId": "m"}
        }))
        .unwrap(),
    ];
    config
}

#[test]
fn offline_removal_of_one_pricing_rule_is_allowed() {
    let mut conn = support::open_ledger_db("billing_pricing_offline_remove");
    activate_catalog(&mut conn, "snap-valid", VALID_CATALOG, 1_000);
    billing::commit_with_conn(
        &mut conn,
        &BillingCommit {
            expected_revision: 0,
            candidate: two_pricing_rules(),
        },
        1_000,
    )
    .unwrap();

    // 目录下线后，只删除一条候选规则（其余均已在 current 中）应允许保存。
    conn.execute(
        "UPDATE usage_pricing_state SET active_snapshot_id = NULL",
        [],
    )
    .unwrap();
    let mut next = billing::load_with_conn(&conn).unwrap();
    next.pricing_rules.pop();
    let saved = billing::commit_with_conn(
        &mut conn,
        &BillingCommit {
            expected_revision: 1,
            candidate: next,
        },
        1_001,
    )
    .unwrap();
    assert_eq!(saved.revision, 2);
}

#[test]
fn delisted_pricing_reference_does_not_block_adding_a_new_rule() {
    let mut conn = support::open_ledger_db("billing_pricing_delisted_plus_new");
    // 首次提交时 legacy/old 在目录中精确存在（历史合法）。
    activate_catalog(&mut conn, "snap-legacy", DELISTED_CATALOG, 1_000);
    billing::commit_with_conn(
        &mut conn,
        &BillingCommit {
            expected_revision: 0,
            candidate: config_with_pricing_reference("legacy", "old"),
        },
        1_000,
    )
    .unwrap();

    // 目录换版：legacy/old 已下架。存量规则引用随之失效，但本次只新增一条
    // 合法键规则 ⇒ 校验只针对新增/变更引用，不得全量拒绝整次保存。
    activate_catalog(&mut conn, "snap-valid", VALID_CATALOG, 1_001);
    let mut next = billing::load_with_conn(&conn).unwrap();
    next.pricing_rules.push(
        serde_json::from_value(json!({
            "id": "pricing-new",
            "identity": {"sourceId": "codex", "observedProvider": null, "modelId": "gpt-5"},
            "match": "exact",
            "reference": {"providerId": "p", "modelId": "m"}
        }))
        .unwrap(),
    );
    let saved = billing::commit_with_conn(
        &mut conn,
        &BillingCommit {
            expected_revision: 1,
            candidate: next,
        },
        1_002,
    )
    .unwrap();
    assert_eq!(saved.revision, 2);

    let loaded = billing::load_with_conn(&conn).unwrap();
    assert_eq!(
        loaded.pricing_rules.len(),
        2,
        "存量下架引用与新增规则都必须保留"
    );
}

#[test]
fn revision_conflict_wins_over_catalog_validation() {
    let mut conn = support::open_ledger_db("billing_pricing_conflict_priority");
    billing::commit_with_conn(
        &mut conn,
        &BillingCommit {
            expected_revision: 0,
            candidate: valid_config(),
        },
        1_000,
    )
    .unwrap();

    // 当前 revision=1、expected=0，且候选新增了 pricing rule、无 active catalog。
    // 冲突必须优先返回，而不是 pricing catalog unavailable。
    let error = billing::commit_with_conn(
        &mut conn,
        &BillingCommit {
            expected_revision: 0,
            candidate: config_with_pricing_reference("p", "m"),
        },
        1_001,
    )
    .unwrap_err();
    assert_invalid(error.clone(), "冲突优先");
    assert_eq!(error.detail, "billing revision conflict");
}

// ── 快照 GC：激活时只保留「active + 最近 1 个历史快照」 ──────────────────────

fn snapshot_rows(conn: &Connection) -> Vec<(String, i64, String)> {
    let mut stmt = conn
        .prepare("SELECT id, fetched_at, payload_json FROM usage_pricing_snapshot ORDER BY fetched_at, id")
        .unwrap();
    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

fn active_snapshot_id(conn: &Connection) -> Option<String> {
    conn.query_row(
        "SELECT active_snapshot_id FROM usage_pricing_state WHERE singleton = 1",
        [],
        |row| row.get(0),
    )
    .optional()
    .unwrap()
}

#[test]
fn activation_gc_keeps_active_and_one_history_snapshot() {
    let mut conn = support::open_ledger_db("pricing_snapshot_gc");

    // 连续激活 3 个快照（id 各不相同）：最旧的必须被同事务 GC 掉。
    activate_catalog(&mut conn, "snap-oldest", VALID_CATALOG, 1_000);
    activate_catalog(&mut conn, "snap-history", DELISTED_CATALOG, 2_000);
    activate_catalog(&mut conn, "snap-active", EMPTY_CATALOG, 3_000);

    assert_eq!(
        snapshot_rows(&conn),
        vec![
            (
                "snap-history".to_string(),
                2_000,
                DELISTED_CATALOG.to_string()
            ),
            ("snap-active".to_string(), 3_000, EMPTY_CATALOG.to_string()),
        ],
        "表内只能剩 active + 最近 1 个历史快照，旧 payload 必须已删"
    );
    assert_eq!(
        active_snapshot_id(&conn).as_deref(),
        Some("snap-active"),
        "active 必须指向最新快照"
    );

    // 离线回退路径：无网络时唯一的目录来源是 active 指针（load_active_catalog_with_conn）。
    // GC 后当前 active 必须照常可读。
    let (id, fetched_at, catalog) = pricing::load_active_catalog_with_conn(&conn)
        .unwrap()
        .unwrap();
    assert_eq!(id, "snap-active");
    assert_eq!(fetched_at, 3_000);
    assert!(catalog.quotes.is_empty());

    // 保留的历史快照必须内容完整、可整体解析：把 active 指针退回它（回退兜底语义），
    // 目录读取路径仍能取到并解析出 DELISTED_CATALOG 的 legacy/old 报价。
    conn.execute(
        "UPDATE usage_pricing_state SET active_snapshot_id = 'snap-history'",
        [],
    )
    .unwrap();
    let (id, fetched_at, catalog) = pricing::load_active_catalog_with_conn(&conn)
        .unwrap()
        .unwrap();
    assert_eq!(id, "snap-history");
    assert_eq!(fetched_at, 2_000);
    assert_eq!(catalog.quotes.len(), 1);
    assert!(catalog
        .quotes
        .contains_key(&("legacy".to_string(), "old".to_string())));
}
