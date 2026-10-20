//! 计费配置服务（Task 3/4）：校验、读取、带版本提交与可撤销，以及保存前预览。
//!
//! 预览只读账本并返回候选规则的命中/冲突/边界统计，不回写配置；规则合法但业务匹配
//! 冲突允许保存——冲突展示由查询/UI 负责，不能静默选赢家。

use std::collections::HashSet;

use rusqlite::Connection;

use crate::database::connection::DB;
use crate::database::dao::usage_billing as dao;
use crate::services::usage::query::load_ledger_rows_with;
use crate::services::usage::range::resolve_range;
use crate::services::usage::semantics::hero_of;
use crate::services::usage::v2::model::{
    AttributionKind, AttributionReason, BillingCommit, BillingConfig, BillingPreview,
    BillingRevision, RuleMatch, UsageRange,
};
use crate::services::usage::v2::pricing;
use crate::services::usage::v2::query;
use crate::services::usage::UsageError;

fn invalid(detail: impl Into<String>) -> UsageError {
    UsageError::new("usage-settings-invalid", detail)
}

pub fn empty_config() -> BillingConfig {
    BillingConfig {
        revision: 0,
        profiles: Vec::new(),
        rules: Vec::new(),
        pricing_rules: Vec::new(),
    }
}

/// 配置合法性校验（纯函数）。
///
/// 只校验结构与引用完整性；**不做匹配解析**——规则合法但业务匹配冲突允许保存，
/// 由后续预览/展示层显式提示，不能静默选赢家。
pub fn validate_config(config: &BillingConfig) -> Result<(), UsageError> {
    let mut profile_ids: HashSet<&str> = HashSet::new();
    for profile in &config.profiles {
        if profile.id.is_empty() {
            return Err(invalid("billing profile id is empty"));
        }
        if !profile_ids.insert(profile.id.as_str()) {
            return Err(invalid(format!(
                "duplicate billing profile id: {}",
                profile.id
            )));
        }
        if profile.name.trim().is_empty() {
            return Err(invalid(format!(
                "billing profile name is empty: {}",
                profile.id
            )));
        }
    }

    let mut rule_ids: HashSet<&str> = HashSet::new();
    for rule in &config.rules {
        if rule.id.is_empty() {
            return Err(invalid("billing rule id is empty"));
        }
        if !rule_ids.insert(rule.id.as_str()) {
            return Err(invalid(format!("duplicate billing rule id: {}", rule.id)));
        }
        // archived 渠道仍是合法引用（历史记录要能继续显示）。
        if !profile_ids.contains(rule.billing_profile_id.as_str()) {
            return Err(invalid(format!(
                "billing profile not found: {}",
                rule.billing_profile_id
            )));
        }
        if rule
            .effective_to
            .is_some_and(|effective_to| rule.effective_from >= effective_to)
        {
            return Err(invalid(format!(
                "billing rule effective range must be non-empty: {}",
                rule.id
            )));
        }
        if rule.match_kind == RuleMatch::Prefix && rule.identity.model_id.is_empty() {
            return Err(invalid(format!(
                "billing rule prefix modelId is empty: {}",
                rule.id
            )));
        }
    }

    let mut pricing_rule_ids: HashSet<&str> = HashSet::new();
    for rule in &config.pricing_rules {
        if rule.id.is_empty() {
            return Err(invalid("pricing rule id is empty"));
        }
        if !pricing_rule_ids.insert(rule.id.as_str()) {
            return Err(invalid(format!("duplicate pricing rule id: {}", rule.id)));
        }
        if rule.reference.provider_id.is_empty() || rule.reference.model_id.is_empty() {
            return Err(invalid(format!(
                "pricing reference must have providerId and modelId: {}",
                rule.id
            )));
        }
        if rule.match_kind == RuleMatch::Prefix && rule.identity.model_id.is_empty() {
            return Err(invalid(format!(
                "pricing rule prefix modelId is empty: {}",
                rule.id
            )));
        }
    }

    Ok(())
}

pub fn load_with_conn(conn: &Connection) -> Result<BillingConfig, UsageError> {
    dao::load_config_conn(conn)
}

/// 候选 pricingRules 与当前配置不同时，校验所有**新增/变更**引用在 active catalog 中精确存在。
///
/// * 规则未变（Vec 相等）⇒ 不做在线校验（离线保存非定价字段）；
/// * 存量规则（id 与引用键均未变）⇒ 跳过校验：其引用可能已被目录下架（历史合法）；
/// * 纯删除（候选规则均已在当前配置中）⇒ 允许离线删除；
/// * 存在新增/变更规则但无 active catalog ⇒ `pricing catalog unavailable`；
/// * 任一**新增/变更**引用键不在目录精确键中 ⇒ `pricing reference not found: <provider>/<model>`。
fn validate_pricing_references(
    conn: &Connection,
    candidate: &BillingConfig,
    current: &BillingConfig,
) -> Result<(), UsageError> {
    if candidate.pricing_rules == current.pricing_rules {
        return Ok(());
    }
    let only_removals = candidate
        .pricing_rules
        .iter()
        .all(|rule| current.pricing_rules.contains(rule));
    let active = pricing::load_active_catalog_with_conn(conn)?;
    let Some((_snapshot_id, _fetched_at, catalog)) = active else {
        // 无 active catalog：新增/变更引用必须拒绝；纯删除允许离线完成。
        return if only_removals {
            Ok(())
        } else {
            Err(invalid("pricing catalog unavailable"))
        };
    };
    for rule in &candidate.pricing_rules {
        // 存量规则（id 与引用键均未变）可能引用已下架模型——那是历史合法配置，
        // 不得因目录换版而拒绝本次保存；只有新增/变更的引用才要求精确命中目录。
        let unchanged = current
            .pricing_rules
            .iter()
            .any(|existing| existing.id == rule.id && existing.reference == rule.reference);
        if unchanged {
            continue;
        }
        let key = (
            rule.reference.provider_id.clone(),
            rule.reference.model_id.clone(),
        );
        if !catalog.quotes.contains_key(&key) {
            return Err(invalid(format!(
                "pricing reference not found: {}/{}",
                rule.reference.provider_id, rule.reference.model_id
            )));
        }
    }
    Ok(())
}

pub fn commit_with_conn(
    conn: &mut Connection,
    request: &BillingCommit,
    now_ms: i64,
) -> Result<BillingRevision, UsageError> {
    validate_config(&request.candidate)?;
    // 冲突优先于在线目录校验，保持既有错误优先级。
    let current = load_with_conn(conn)?;
    if current.revision != request.expected_revision {
        return Err(invalid("billing revision conflict"));
    }
    validate_pricing_references(conn, &request.candidate, &current)?;
    dao::commit_config_conn(conn, &request.candidate, request.expected_revision, now_ms)
}

pub fn undo_with_conn(
    conn: &mut Connection,
    expected_revision: i64,
    target_revision: i64,
    now_ms: i64,
) -> Result<BillingRevision, UsageError> {
    // 先核对 expected：冲突优先于“目标版本不存在”，与 commit 的语义保持一致。
    if dao::load_config_conn(conn)?.revision != expected_revision {
        return Err(invalid("billing revision conflict"));
    }
    if target_revision < 0 {
        return Err(invalid(format!(
            "billing revision not found: {target_revision}"
        )));
    }

    // revision 0 = 空配置（从未提交）；历史版本必须存在。
    let target = if target_revision == 0 {
        empty_config()
    } else {
        dao::load_revision_conn(conn, target_revision)?
            .ok_or_else(|| invalid(format!("billing revision not found: {target_revision}")))?
    };
    validate_config(&target)?;

    // 以新 revision 再提交一次：不删除历史，撤销后 load 的 revision 是新版本。
    dao::commit_config_conn(conn, &target, expected_revision, now_ms)
}

pub fn preview_with_conn(
    conn: &Connection,
    candidate: &BillingConfig,
    range: &UsageRange,
    now_ms: i64,
) -> Result<BillingPreview, UsageError> {
    // 只读：当前 state 给出 baseRevision；候选配置不回写。
    let base = dao::load_config_conn(conn)?;
    validate_config(candidate)?;
    validate_pricing_references(conn, candidate, &base)?;
    let mut candidate = candidate.clone();
    candidate.revision = base
        .revision
        .checked_add(1)
        .ok_or_else(|| invalid("billing revision overflow"))?;

    // 与 snapshot 对齐：关闭态返回空预览（baseRevision / candidate 仍正常，不碰账本）。
    if !crate::services::usage::settings::load_from_conn(conn).enabled {
        return Ok(BillingPreview {
            base_revision: base.revision,
            candidate,
            matched_identities: Vec::new(),
            matched_tokens: 0,
            ambiguous_tokens: 0,
            boundary_tokens: 0,
        });
    }

    let resolved = resolve_range(range, now_ms)?;
    let identities = query::attributed_identity_usages(query::attribute_rows(
        load_ledger_rows_with(conn, &resolved),
        resolved.cur.granularity,
        &candidate,
    )?);

    let mut matched_identities = Vec::new();
    let mut matched_tokens = 0i64;
    let mut ambiguous_tokens = 0i64;
    let mut boundary_tokens = 0i64;
    for usage in identities {
        let tokens = hero_of(usage.metrics.request_total, usage.buckets.output);
        match (usage.attribution.attribution_kind, usage.attribution.reason) {
            (AttributionKind::UserAssigned, _) => {
                matched_tokens += tokens;
                matched_identities.push(usage);
            }
            (AttributionKind::Ambiguous, _) => {
                ambiguous_tokens += tokens;
                matched_identities.push(usage);
            }
            (AttributionKind::Unknown, Some(AttributionReason::BucketBoundary)) => {
                boundary_tokens += tokens;
                matched_identities.push(usage);
            }
            // unassigned 及其他形态不属于“有匹配/冲突/边界归属”，不计 Token。
            _ => {}
        }
    }

    Ok(BillingPreview {
        base_revision: base.revision,
        candidate,
        matched_identities,
        matched_tokens,
        ambiguous_tokens,
        boundary_tokens,
    })
}

// ── 生产入口（IPC 用）：只负责取 DB 锁，业务全部走 `*_with_conn` ──────────────
// 命令层不得自己取锁；这里 guard 存活期间只调用对应的 with_conn，不嵌套取锁。

fn poisoned() -> UsageError {
    UsageError::new("usage-db-failed", "db poisoned lock")
}

pub fn load() -> Result<BillingConfig, UsageError> {
    let conn = DB.lock().map_err(|_| poisoned())?;
    load_with_conn(&conn)
}

pub fn preview(
    candidate: &BillingConfig,
    range: &UsageRange,
    now_ms: i64,
) -> Result<BillingPreview, UsageError> {
    let conn = DB.lock().map_err(|_| poisoned())?;
    preview_with_conn(&conn, candidate, range, now_ms)
}

pub fn commit(request: &BillingCommit, now_ms: i64) -> Result<BillingRevision, UsageError> {
    let mut conn = DB.lock().map_err(|_| poisoned())?;
    commit_with_conn(&mut conn, request, now_ms)
}

pub fn undo(
    expected_revision: i64,
    target_revision: i64,
    now_ms: i64,
) -> Result<BillingRevision, UsageError> {
    let mut conn = DB.lock().map_err(|_| poisoned())?;
    undo_with_conn(&mut conn, expected_revision, target_revision, now_ms)
}

// ==== usage 自省锁：以下为排除区（测试代码），勿删勿复制 ====
#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::usage::v2::model::{
        BillingProfile, BillingProfileKind, BillingRule, IdentityKey, PricingReference,
        PricingRule, UsageSourceId,
    };
    use serde_json::json;

    fn base_config() -> BillingConfig {
        serde_json::from_value(json!({
            "revision": 0,
            "profiles": [{
                "id": "profile-a", "name": "工作套餐", "kind": "api",
                "note": "", "archived": false
            }],
            "rules": [{
                "id": "rule-a",
                "identity": {"sourceId": "codex", "observedProvider": null, "modelId": "gpt-5"},
                "match": "exact", "billingProfileId": "profile-a",
                "effectiveFrom": 0, "effectiveTo": null, "mixed": false
            }],
            "pricingRules": []
        }))
        .unwrap()
    }

    fn assert_invalid(config: &BillingConfig, label: &str) {
        let err = validate_config(config).expect_err(label);
        assert_eq!(err.code, "usage-settings-invalid", "{label}: {err:?}");
    }

    #[test]
    fn accepts_a_complete_valid_config() {
        validate_config(&base_config()).expect("合法配置必须通过");
    }

    #[test]
    fn archived_profiles_are_still_valid_references() {
        let mut config = base_config();
        config.profiles[0].archived = true;
        validate_config(&config).expect("archived 渠道仍可被既有规则引用");
    }

    #[test]
    fn rejects_empty_profile_name() {
        let mut config = base_config();
        config.profiles[0].name = "   ".to_string();
        assert_invalid(&config, "空名");
    }

    #[test]
    fn rejects_duplicate_profile_ids() {
        let mut config = base_config();
        let duplicate = BillingProfile {
            id: "profile-a".to_string(),
            name: "另一个".to_string(),
            kind: BillingProfileKind::Unknown,
            note: String::new(),
            archived: false,
        };
        config.profiles.push(duplicate);
        assert_invalid(&config, "重复 profile id");
    }

    #[test]
    fn rejects_empty_profile_id() {
        let mut config = base_config();
        config.profiles[0].id = String::new();
        config.rules.clear();
        assert_invalid(&config, "空 profile id");
    }

    #[test]
    fn rejects_duplicate_rule_ids() {
        let mut config = base_config();
        let mut duplicate = config.rules[0].clone();
        duplicate.identity.model_id = "other-model".to_string();
        config.rules.push(duplicate);
        assert_invalid(&config, "重复 rule id");
    }

    #[test]
    fn rejects_empty_rule_id() {
        let mut config = base_config();
        config.rules[0].id = String::new();
        assert_invalid(&config, "空 rule id");
    }

    #[test]
    fn rejects_dangling_billing_profile_reference() {
        let mut config = base_config();
        config.rules[0].billing_profile_id = "missing-profile".to_string();
        assert_invalid(&config, "悬空 profile 引用");
    }

    #[test]
    fn rejects_reversed_or_zero_length_effective_range() {
        let mut reversed = base_config();
        reversed.rules[0].effective_from = 200;
        reversed.rules[0].effective_to = Some(100);
        assert_invalid(&reversed, "日期逆序");

        let mut zero = base_config();
        zero.rules[0].effective_from = 100;
        zero.rules[0].effective_to = Some(100);
        assert_invalid(&zero, "零长度区间");
    }

    #[test]
    fn rejects_empty_prefix_rule() {
        let mut config = base_config();
        config.rules[0].match_kind = RuleMatch::Prefix;
        config.rules[0].identity.model_id = String::new();
        assert_invalid(&config, "渠道规则空前缀");
    }

    #[test]
    fn rejects_duplicate_pricing_rule_ids() {
        let mut config = base_config();
        let pricing = PricingRule {
            id: "pricing-a".to_string(),
            identity: IdentityKey {
                source_id: UsageSourceId::Codex,
                observed_provider: None,
                model_id: "gpt-5".to_string(),
            },
            match_kind: RuleMatch::Exact,
            reference: PricingReference {
                provider_id: "openai".to_string(),
                model_id: "gpt-5".to_string(),
            },
        };
        config.pricing_rules.push(pricing.clone());
        config.pricing_rules.push(pricing);
        assert_invalid(&config, "重复 pricingRule id");
    }

    #[test]
    fn rejects_empty_pricing_rule_id() {
        let mut config = base_config();
        config.pricing_rules.push(PricingRule {
            id: String::new(),
            identity: IdentityKey {
                source_id: UsageSourceId::Codex,
                observed_provider: None,
                model_id: "gpt-5".to_string(),
            },
            match_kind: RuleMatch::Exact,
            reference: PricingReference {
                provider_id: "openai".to_string(),
                model_id: "gpt-5".to_string(),
            },
        });
        assert_invalid(&config, "空 pricingRule id");
    }

    #[test]
    fn rejects_empty_pricing_reference() {
        let mut config = base_config();
        config.pricing_rules.push(PricingRule {
            id: "pricing-a".to_string(),
            identity: IdentityKey {
                source_id: UsageSourceId::Codex,
                observed_provider: None,
                model_id: "gpt-5".to_string(),
            },
            match_kind: RuleMatch::Exact,
            reference: PricingReference {
                provider_id: String::new(),
                model_id: "gpt-5".to_string(),
            },
        });
        assert_invalid(&config, "空 providerId");

        let mut config = base_config();
        config.pricing_rules.push(PricingRule {
            id: "pricing-a".to_string(),
            identity: IdentityKey {
                source_id: UsageSourceId::Codex,
                observed_provider: None,
                model_id: "gpt-5".to_string(),
            },
            match_kind: RuleMatch::Exact,
            reference: PricingReference {
                provider_id: "openai".to_string(),
                model_id: String::new(),
            },
        });
        assert_invalid(&config, "空 modelId");
    }

    #[test]
    fn rejects_empty_prefix_on_pricing_rule() {
        let mut config = base_config();
        config.pricing_rules.push(PricingRule {
            id: "pricing-a".to_string(),
            identity: IdentityKey {
                source_id: UsageSourceId::Codex,
                observed_provider: None,
                model_id: String::new(),
            },
            match_kind: RuleMatch::Prefix,
            reference: PricingReference {
                provider_id: "openai".to_string(),
                model_id: "gpt-5".to_string(),
            },
        });
        assert_invalid(&config, "定价规则空前缀");
    }

    #[test]
    fn empty_config_is_revision_zero_with_no_entries() {
        let config = empty_config();
        assert_eq!(config.revision, 0);
        assert!(config.profiles.is_empty());
        assert!(config.rules.is_empty());
        assert!(config.pricing_rules.is_empty());
    }

    // 让 `BillingRule` 的导入在测试模块内被使用（避免 unused import）。
    #[test]
    fn base_config_has_one_exact_rule() {
        let config = base_config();
        let rule: &BillingRule = &config.rules[0];
        assert_eq!(rule.match_kind, RuleMatch::Exact);
    }

    fn local_ms(y: i32, m: u32, d: u32, h: u32, min: u32) -> i64 {
        use chrono::TimeZone;
        chrono::Local
            .with_ymd_and_hms(y, m, d, h, min, 0)
            .single()
            .expect("测试时刻必须存在")
            .timestamp_millis()
    }

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        conn
    }

    fn add_daily(conn: &Connection, source: &str, model: &str, output: i64) {
        conn.execute(
            "INSERT INTO usage_daily
             (day_key, source_id, project_key, provider, provider_kind, model,
              input_fresh, cache_read, cache_write, output, request_total, requests,
              user_est, updated_at)
             VALUES ('2026-10-17', ?1, '', '', 'measured', ?2, 10, 0, 0, ?3, 10, 1, NULL, 0)",
            rusqlite::params![source, model, output],
        )
        .unwrap();
    }

    /// 预览三值互斥、只计可靠归属，且**绝不写库**。
    #[test]
    fn preview_classifies_identities_without_writing() {
        let conn = mem();
        add_daily(&conn, "codex", "gpt-5", 7); // matched
        add_daily(&conn, "codex", "gpt-switch", 17); // ambiguous
        add_daily(&conn, "codex", "gpt-boundary", 5); // boundary
        add_daily(&conn, "codex", "gpt-unknown", 3); // unassigned，不计

        let day_start = local_ms(2026, 10, 17, 0, 0);
        let noon = local_ms(2026, 10, 17, 12, 0);
        let next_day = local_ms(2026, 10, 18, 0, 0);
        let rule = |id: &str, model: &str, profile: &str, from: i64, to: Option<i64>| {
            json!({
                "id": id,
                "identity": {"sourceId": "codex", "observedProvider": null, "modelId": model},
                "match": "exact", "billingProfileId": profile,
                "effectiveFrom": from, "effectiveTo": to, "mixed": false
            })
        };
        let candidate: BillingConfig = serde_json::from_value(json!({
            "revision": 0,
            "profiles": [
                {"id": "A", "name": "Profile A", "kind": "api", "note": "", "archived": false},
                {"id": "B", "name": "Profile B", "kind": "api", "note": "", "archived": false}
            ],
            "rules": [
                rule("matched", "gpt-5", "A", 0, None),
                rule("switch-a", "gpt-switch", "A", day_start, Some(noon)),
                rule("switch-b", "gpt-switch", "B", noon, Some(next_day)),
                rule("boundary", "gpt-boundary", "A", noon, Some(next_day))
            ],
            "pricingRules": []
        }))
        .unwrap();

        let range = UsageRange {
            preset: crate::services::usage::model::UsageRangePreset::Last7d,
            from: None,
            to: None,
        };
        let preview =
            preview_with_conn(&conn, &candidate, &range, local_ms(2026, 10, 17, 12, 0)).unwrap();

        assert_eq!(preview.base_revision, 0);
        assert_eq!(preview.candidate.revision, 1, "候选 revision = base + 1");
        assert_eq!(
            preview.matched_identities.len(),
            3,
            "只返回 matched / ambiguous / boundary 三类身份组"
        );
        assert_eq!(preview.matched_tokens, 17, "4 桶 hero=requestTotal+output");
        assert_eq!(preview.ambiguous_tokens, 27);
        assert_eq!(preview.boundary_tokens, 15);

        let matched = preview
            .matched_identities
            .iter()
            .find(|usage| usage.identity.model_id == "gpt-5")
            .unwrap();
        assert_eq!(
            matched.attribution.attribution_kind,
            AttributionKind::UserAssigned
        );
        assert_eq!(matched.attribution.billing_profile_id.as_deref(), Some("A"));
        assert!(
            !preview
                .matched_identities
                .iter()
                .any(|usage| usage.identity.model_id == "gpt-unknown"),
            "未归属身份不计入三项 Token"
        );

        // 预览不得写 state / 历史。
        assert_eq!(load_with_conn(&conn).unwrap().revision, 0);
        let revisions: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage_billing_revision", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(revisions, 0);
    }

    #[test]
    fn preview_is_empty_when_usage_switch_is_disabled() {
        let conn = mem();
        add_daily(&conn, "codex", "gpt-5", 7);
        let settings = crate::services::usage::model::UsageSettings {
            enabled: false,
            ..Default::default()
        };
        let raw = serde_json::to_string(&settings).unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('usage.settings', ?1)",
            rusqlite::params![raw],
        )
        .unwrap();

        let range = UsageRange {
            preset: crate::services::usage::model::UsageRangePreset::Last7d,
            from: None,
            to: None,
        };
        let preview =
            preview_with_conn(&conn, &base_config(), &range, local_ms(2026, 10, 17, 12, 0))
                .unwrap();
        assert_eq!(preview.base_revision, 0);
        assert_eq!(preview.candidate.revision, 1);
        assert!(preview.matched_identities.is_empty());
        assert_eq!(
            (
                preview.matched_tokens,
                preview.ambiguous_tokens,
                preview.boundary_tokens
            ),
            (0, 0, 0)
        );
    }
}
// ==== usage 自省锁：排除区结束，勿删勿复制 ====
