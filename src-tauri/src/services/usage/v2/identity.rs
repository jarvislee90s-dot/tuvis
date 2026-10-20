//! 计费身份纯匹配器。
//!
//! 全部为纯函数，不读库、不取锁、不做 I/O。核心纪律：
//! * 身份严格匹配：`sourceId` 全等；`observedProvider` 的 `null` / 空串 / 非空严格区分；
//!   精确规则要求完整 `modelId` 全等，前缀规则按完整 `modelId` 前缀匹配，**空前缀永不匹配**。
//! * 优先级：精确 > 最长前缀；同优先级中，被**严格更窄**时间区间严格包含的候选被剔除；
//!   交叉但互不包含视为同级。
//! * 逐段选优：`resolve_bucket` 按所有生效边界切分，逐段 `resolve_at` 后 `merge_bucket`；
//!   绝不按规则数组顺序“先到先得”。
//! * 时间区间为左闭右开 `[effectiveFrom, effectiveTo)`；`resolve_bucket` 的
//!   `start >= end` 由调用层拒绝。

use crate::services::usage::v2::model::{
    Attribution, AttributionKind, AttributionReason, BillingRule, IdentityKey, RuleMatch,
};

/// 计算单个时间点 `ts` 的最优归属。
///
/// 选优顺序（不可按规则数组顺序“先到先得”）：
/// 1. 过滤身份 + 有效时间 `[effectiveFrom, effectiveTo)`；
/// 2. 先取全部 exact；没有 exact 时取最长前缀的 prefix 候选；
/// 3. 在同级候选中剔除被**严格更窄**时间区间支配（严格包含）的候选；
/// 4. 剩余候选若含 `mixed` 或指向不同渠道 ⇒ ambiguous，否则唯一渠道 userAssigned。
pub fn resolve_at(identity: &IdentityKey, ts: i64, rules: &[BillingRule]) -> Attribution {
    let matching: Vec<&BillingRule> = rules
        .iter()
        .filter(|rule| rule_identity_matches(identity, rule) && rule_is_active(rule, ts))
        .collect();
    if matching.is_empty() {
        return unknown(AttributionReason::Unassigned);
    }

    // 规则等级：exact 高于一切 prefix；prefix 之间按完整模型前缀长度降序。
    let exact: Vec<&BillingRule> = matching
        .iter()
        .copied()
        .filter(|rule| rule.match_kind == RuleMatch::Exact)
        .collect();
    let same_rank: Vec<&BillingRule> = if exact.is_empty() {
        let longest = matching
            .iter()
            .filter(|rule| rule.match_kind == RuleMatch::Prefix)
            .map(|rule| rule.identity.model_id.chars().count())
            .max();
        match longest {
            Some(len) => matching
                .iter()
                .copied()
                .filter(|rule| {
                    rule.match_kind == RuleMatch::Prefix
                        && rule.identity.model_id.chars().count() == len
                })
                .collect(),
            None => Vec::new(),
        }
    } else {
        exact
    };
    if same_rank.is_empty() {
        return unknown(AttributionReason::Unassigned);
    }

    // 同级中，时间区间被另一个候选**严格包含**得更窄时，宽区间候选被支配。
    // 交叉但互不包含（例如 [0,10) 与 [5,15)）视为同级。
    let mut kept: Vec<&BillingRule> = Vec::new();
    for i in 0..same_rank.len() {
        // 候选 i 若**严格包含**另一个同级候选 j，则 i 是更宽的区间 ⇒ 被支配。
        let dominated = (0..same_rank.len())
            .any(|j| i != j && interval_strictly_contains(same_rank[i], same_rank[j]));
        if !dominated {
            kept.push(same_rank[i]);
        }
    }

    let any_mixed = kept.iter().any(|rule| rule.mixed);
    let mut profiles: Vec<&str> = kept
        .iter()
        .map(|rule| rule.billing_profile_id.as_str())
        .collect();
    profiles.sort_unstable();
    profiles.dedup();
    let rule_ids = sorted_rule_ids(kept.iter().copied());

    if any_mixed || profiles.len() > 1 {
        return Attribution {
            billing_profile_id: None,
            attribution_kind: AttributionKind::Ambiguous,
            rule_ids,
            reason: Some(if any_mixed {
                AttributionReason::Mixed
            } else {
                AttributionReason::Conflict
            }),
        };
    }

    Attribution {
        billing_profile_id: profiles.first().map(|profile| (*profile).to_string()),
        attribution_kind: AttributionKind::UserAssigned,
        rule_ids,
        reason: None,
    }
}

/// 将若干连续子段的归因合并为整桶归因。
///
/// * 空 parts ⇒ unknown/unassigned；
/// * 任一段 ambiguous 或出现不同非空渠道 ⇒ ambiguous（mixed 优先作为原因）；
/// * 存在未知子段 ⇒ unknown/bucketBoundary；
/// * 所有子段唯一同渠道 ⇒ userAssigned，ruleIds 去重排序。
pub fn merge_bucket(parts: Vec<Attribution>) -> Attribution {
    if parts.is_empty() {
        return unknown(AttributionReason::Unassigned);
    }

    let any_ambiguous = parts
        .iter()
        .any(|part| part.attribution_kind == AttributionKind::Ambiguous);
    let any_mixed = parts.iter().any(|part| {
        part.attribution_kind == AttributionKind::Ambiguous
            && part.reason == Some(AttributionReason::Mixed)
    });
    let mut profiles: Vec<&str> = parts
        .iter()
        .filter_map(|part| part.billing_profile_id.as_deref())
        .collect();
    profiles.sort_unstable();
    profiles.dedup();
    let mut rule_ids: Vec<String> = parts
        .iter()
        .flat_map(|part| part.rule_ids.iter().cloned())
        .collect();
    rule_ids.sort_unstable();
    rule_ids.dedup();

    if any_ambiguous || profiles.len() > 1 {
        return Attribution {
            billing_profile_id: None,
            attribution_kind: AttributionKind::Ambiguous,
            rule_ids,
            reason: Some(if any_mixed {
                AttributionReason::Mixed
            } else {
                AttributionReason::Conflict
            }),
        };
    }

    let all_assigned = parts
        .iter()
        .all(|part| part.attribution_kind == AttributionKind::UserAssigned);
    if !all_assigned || profiles.is_empty() {
        // 整桶都没有任何候选 ⇒ unassigned；部分子段无覆盖 ⇒ bucketBoundary。
        let wholly_unassigned = parts.iter().all(|part| {
            part.attribution_kind == AttributionKind::Unknown
                && part.reason == Some(AttributionReason::Unassigned)
        });
        return unknown(if wholly_unassigned {
            AttributionReason::Unassigned
        } else {
            AttributionReason::BucketBoundary
        });
    }

    Attribution {
        billing_profile_id: Some(profiles[0].to_string()),
        attribution_kind: AttributionKind::UserAssigned,
        rule_ids,
        reason: None,
    }
}

/// 按所有规则生效边界切分 `[start, end)`，逐段解析后合并。
///
/// 调用层负责拒绝 `start >= end`；规则自身的非法日期区间由配置层拒绝。
pub fn resolve_bucket(
    identity: &IdentityKey,
    start: i64,
    end: i64,
    rules: &[BillingRule],
) -> Attribution {
    debug_assert!(
        start < end,
        "resolve_bucket 只接受非空左闭右开区间（start >= end 由调用层拒绝）"
    );
    let mut cuts = vec![start, end];
    for rule in rules {
        if rule.effective_from > start && rule.effective_from < end {
            cuts.push(rule.effective_from);
        }
        if let Some(effective_to) = rule.effective_to {
            if effective_to > start && effective_to < end {
                cuts.push(effective_to);
            }
        }
    }
    cuts.sort_unstable();
    cuts.dedup();
    let parts: Vec<Attribution> = cuts
        .windows(2)
        .map(|window| resolve_at(identity, window[0], rules))
        .collect();
    merge_bucket(parts)
}

fn unknown(reason: AttributionReason) -> Attribution {
    Attribution {
        billing_profile_id: None,
        attribution_kind: AttributionKind::Unknown,
        rule_ids: Vec::new(),
        reason: Some(reason),
    }
}

fn sorted_rule_ids<'a>(rules: impl Iterator<Item = &'a BillingRule>) -> Vec<String> {
    let mut ids: Vec<String> = rules.map(|rule| rule.id.clone()).collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// 身份严格匹配：null / 空串 / 非空 provider 三态互不相等。
fn rule_identity_matches(identity: &IdentityKey, rule: &BillingRule) -> bool {
    if identity.source_id != rule.identity.source_id
        || identity.observed_provider != rule.identity.observed_provider
    {
        return false;
    }
    match rule.match_kind {
        RuleMatch::Exact => identity.model_id == rule.identity.model_id,
        RuleMatch::Prefix => {
            // 空前缀规则永不匹配（含空 model 身份），避免“全匹配”式误归属。
            !rule.identity.model_id.is_empty()
                && identity.model_id.starts_with(&rule.identity.model_id)
        }
    }
}

fn rule_is_active(rule: &BillingRule, ts: i64) -> bool {
    rule.effective_from <= ts
        && rule
            .effective_to
            .is_none_or(|effective_to| ts < effective_to)
}

/// `outer` 的区间是否**严格包含** `inner` 的区间。
/// `effectiveTo = None` 视为正无穷；仅一端严格更宽才算严格包含。
fn interval_strictly_contains(outer: &BillingRule, inner: &BillingRule) -> bool {
    let starts_no_later = outer.effective_from <= inner.effective_from;
    let ends_no_earlier = match (outer.effective_to, inner.effective_to) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(outer_to), Some(inner_to)) => outer_to >= inner_to,
    };
    let strictly_wider = outer.effective_from < inner.effective_from
        || match (outer.effective_to, inner.effective_to) {
            (None, Some(_)) => true,
            (Some(outer_to), Some(inner_to)) => outer_to > inner_to,
            _ => false,
        };
    starts_no_later && ends_no_earlier && strictly_wider
}

// ==== usage 自省锁：以下为排除区（测试代码），勿删勿复制 ====
#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::usage::model::{UsageBuckets, UsageMetrics, UsageSourceId};
    use crate::services::usage::v2::model::{
        BillingCommit, BillingConfig, BillingPreview, BillingProfile, BillingProfileKind,
        BillingRevision, DistributionGroup, DistributionRow, IdentityUsage, PricingReference,
        PricingRule, SourceKind, TrendPoint, TrendSelection, UsageDistribution, UsageTrend,
    };
    use serde_json::{json, Value};

    fn identity(v: Value) -> IdentityKey {
        serde_json::from_value(v).expect("身份 JSON 必须符合契约")
    }

    fn rules(v: Value) -> Vec<BillingRule> {
        serde_json::from_value(v).expect("规则 JSON 必须完整且符合契约")
    }

    fn base_identity() -> Value {
        json!({"sourceId":"codex","observedProvider":null,"modelId":"gpt-x"})
    }

    #[test]
    fn equal_rank_different_profiles_is_ambiguous() {
        let raw = r#"{"sourceId":"codex","observedProvider":null,"modelId":"gpt-x"}"#;
        let identity: IdentityKey = serde_json::from_str(raw).unwrap();
        let rules: Vec<BillingRule> = serde_json::from_value(serde_json::json!([
          {"id":"r1","identity":identity,"match":"exact","billingProfileId":"a",
           "effectiveFrom":0,"effectiveTo":null,"mixed":false},
          {"id":"r2","identity":identity,"match":"exact","billingProfileId":"b",
           "effectiveFrom":0,"effectiveTo":null,"mixed":false}
        ]))
        .unwrap();
        let got = resolve_bucket(&identity, 100, 200, &rules);
        assert!(matches!(got.attribution_kind, AttributionKind::Ambiguous));
        assert!(got.billing_profile_id.is_none());
    }

    /// 表驱动：逐段选优、身份严格匹配与时间边界。
    #[test]
    fn table_driven_resolution_cases() {
        #[derive(Debug, Clone, Copy)]
        enum Expect {
            Assigned(&'static str),
            Ambiguous,
            Unknown,
        }

        struct Case {
            name: &'static str,
            identity: Value,
            rules: Value,
            start: i64,
            end: i64,
            expect: Expect,
            reason: Option<AttributionReason>,
            rule_ids: Option<Vec<&'static str>>,
        }

        let cases = vec![
            Case {
                name: "exact 胜 prefix",
                identity: base_identity(),
                rules: json!([
                    {"id":"exact","identity":{"sourceId":"codex","observedProvider":null,"modelId":"gpt-x"},
                     "match":"exact","billingProfileId":"exact-profile",
                     "effectiveFrom":0,"effectiveTo":null,"mixed":false},
                    {"id":"prefix","identity":{"sourceId":"codex","observedProvider":null,"modelId":"gpt"},
                     "match":"prefix","billingProfileId":"prefix-profile",
                     "effectiveFrom":0,"effectiveTo":null,"mixed":false}
                ]),
                start: 0,
                end: 100,
                expect: Expect::Assigned("exact-profile"),
                reason: None,
                rule_ids: Some(vec!["exact"]),
            },
            Case {
                name: "最长前缀",
                identity: base_identity(),
                rules: json!([
                    {"id":"short","identity":{"sourceId":"codex","observedProvider":null,"modelId":"gpt"},
                     "match":"prefix","billingProfileId":"short",
                     "effectiveFrom":0,"effectiveTo":null,"mixed":false},
                    {"id":"long","identity":{"sourceId":"codex","observedProvider":null,"modelId":"gpt-"},
                     "match":"prefix","billingProfileId":"long",
                     "effectiveFrom":0,"effectiveTo":null,"mixed":false}
                ]),
                start: 0,
                end: 100,
                expect: Expect::Assigned("long"),
                reason: None,
                rule_ids: Some(vec!["long"]),
            },
            Case {
                name: "null 不匹配非空",
                identity: base_identity(),
                rules: json!([
                    {"id":"r1","identity":{"sourceId":"codex","observedProvider":"openai","modelId":"gpt-x"},
                     "match":"exact","billingProfileId":"a",
                     "effectiveFrom":0,"effectiveTo":null,"mixed":false}
                ]),
                start: 0,
                end: 100,
                expect: Expect::Unknown,
                reason: Some(AttributionReason::Unassigned),
                rule_ids: Some(vec![]),
            },
            Case {
                name: "空串 provider 不匹配 null",
                identity: json!({"sourceId":"codex","observedProvider":"","modelId":"gpt-x"}),
                rules: json!([
                    {"id":"r1","identity":{"sourceId":"codex","observedProvider":null,"modelId":"gpt-x"},
                     "match":"exact","billingProfileId":"a",
                     "effectiveFrom":0,"effectiveTo":null,"mixed":false}
                ]),
                start: 0,
                end: 100,
                expect: Expect::Unknown,
                reason: Some(AttributionReason::Unassigned),
                rule_ids: Some(vec![]),
            },
            Case {
                name: "source 不同不匹配",
                identity: base_identity(),
                rules: json!([
                    {"id":"r1","identity":{"sourceId":"claude","observedProvider":null,"modelId":"gpt-x"},
                     "match":"exact","billingProfileId":"a",
                     "effectiveFrom":0,"effectiveTo":null,"mixed":false}
                ]),
                start: 0,
                end: 100,
                expect: Expect::Unknown,
                reason: Some(AttributionReason::Unassigned),
                rule_ids: Some(vec![]),
            },
            Case {
                name: "同目标重叠无冲突",
                identity: base_identity(),
                rules: json!([
                    {"id":"r2","identity":{"sourceId":"codex","observedProvider":null,"modelId":"gpt-x"},
                     "match":"exact","billingProfileId":"a",
                     "effectiveFrom":0,"effectiveTo":100,"mixed":false},
                    {"id":"r1","identity":{"sourceId":"codex","observedProvider":null,"modelId":"gpt-x"},
                     "match":"exact","billingProfileId":"a",
                     "effectiveFrom":50,"effectiveTo":150,"mixed":false}
                ]),
                start: 0,
                end: 150,
                expect: Expect::Assigned("a"),
                reason: None,
                rule_ids: Some(vec!["r1", "r2"]),
            },
            Case {
                name: "换渠道边界 ambiguous",
                identity: base_identity(),
                rules: json!([
                    {"id":"r1","identity":{"sourceId":"codex","observedProvider":null,"modelId":"gpt-x"},
                     "match":"exact","billingProfileId":"a",
                     "effectiveFrom":0,"effectiveTo":100,"mixed":false},
                    {"id":"r2","identity":{"sourceId":"codex","observedProvider":null,"modelId":"gpt-x"},
                     "match":"exact","billingProfileId":"b",
                     "effectiveFrom":100,"effectiveTo":200,"mixed":false}
                ]),
                start: 0,
                end: 200,
                expect: Expect::Ambiguous,
                reason: Some(AttributionReason::Conflict),
                rule_ids: Some(vec!["r1", "r2"]),
            },
            Case {
                name: "半桶无规则 unknown",
                identity: base_identity(),
                rules: json!([
                    {"id":"r1","identity":{"sourceId":"codex","observedProvider":null,"modelId":"gpt-x"},
                     "match":"exact","billingProfileId":"a",
                     "effectiveFrom":0,"effectiveTo":50,"mixed":false}
                ]),
                start: 0,
                end: 100,
                expect: Expect::Unknown,
                reason: Some(AttributionReason::BucketBoundary),
                rule_ids: Some(vec![]),
            },
            Case {
                name: "mixed 不准确归属",
                identity: base_identity(),
                rules: json!([
                    {"id":"r1","identity":{"sourceId":"codex","observedProvider":null,"modelId":"gpt-x"},
                     "match":"exact","billingProfileId":"a",
                     "effectiveFrom":0,"effectiveTo":null,"mixed":true}
                ]),
                start: 0,
                end: 100,
                expect: Expect::Ambiguous,
                reason: Some(AttributionReason::Mixed),
                rule_ids: Some(vec!["r1"]),
            },
            Case {
                name: "空 model 不允许前缀空串规则",
                identity: json!({"sourceId":"codex","observedProvider":null,"modelId":""}),
                rules: json!([
                    {"id":"r1","identity":{"sourceId":"codex","observedProvider":null,"modelId":""},
                     "match":"prefix","billingProfileId":"a",
                     "effectiveFrom":0,"effectiveTo":null,"mixed":false}
                ]),
                start: 0,
                end: 100,
                expect: Expect::Unknown,
                reason: Some(AttributionReason::Unassigned),
                rule_ids: Some(vec![]),
            },
            Case {
                name: "非空 model 也不允许空前缀规则",
                identity: base_identity(),
                rules: json!([
                    {"id":"r1","identity":{"sourceId":"codex","observedProvider":null,"modelId":""},
                     "match":"prefix","billingProfileId":"a",
                     "effectiveFrom":0,"effectiveTo":null,"mixed":false}
                ]),
                start: 0,
                end: 100,
                expect: Expect::Unknown,
                reason: Some(AttributionReason::Unassigned),
                rule_ids: Some(vec![]),
            },
        ];

        for case in cases {
            let id = identity(case.identity);
            let rs = rules(case.rules);
            let got = resolve_bucket(&id, case.start, case.end, &rs);
            match case.expect {
                Expect::Assigned(profile) => {
                    assert_eq!(
                        got.attribution_kind,
                        AttributionKind::UserAssigned,
                        "用例「{}」应为唯一归属，实际 {:?}",
                        case.name,
                        got
                    );
                    assert_eq!(
                        got.billing_profile_id.as_deref(),
                        Some(profile),
                        "用例「{}」渠道不符",
                        case.name
                    );
                }
                Expect::Ambiguous => assert_eq!(
                    got.attribution_kind,
                    AttributionKind::Ambiguous,
                    "用例「{}」应为 ambiguous，实际 {:?}",
                    case.name,
                    got
                ),
                Expect::Unknown => assert_eq!(
                    got.attribution_kind,
                    AttributionKind::Unknown,
                    "用例「{}」应为 unknown，实际 {:?}",
                    case.name,
                    got
                ),
            }
            if matches!(case.expect, Expect::Ambiguous) {
                assert!(
                    got.billing_profile_id.is_none(),
                    "用例「{}」ambiguous 不得给出渠道",
                    case.name
                );
            }
            assert_eq!(got.reason, case.reason, "用例「{}」reason 不符", case.name);
            if let Some(expected_ids) = case.rule_ids {
                let expected: Vec<String> = expected_ids.iter().map(|s| s.to_string()).collect();
                assert_eq!(got.rule_ids, expected, "用例「{}」ruleIds 不符", case.name);
            }
        }
    }

    /// provider 正向匹配：非空与非空相同、空串与空串相同（严格相等，但相等时命中）。
    #[test]
    fn providers_match_when_strictly_equal() {
        for (identity_provider, rule_provider) in
            [(Some("openai"), Some("openai")), (Some(""), Some(""))]
        {
            let id = IdentityKey {
                source_id: UsageSourceId::Codex,
                observed_provider: identity_provider.map(str::to_string),
                model_id: "gpt-x".to_string(),
            };
            let rs = rules(json!([
                {"id":"r1","identity":{
                    "sourceId":"codex",
                    "observedProvider":rule_provider,
                    "modelId":"gpt-x"
                 },"match":"exact","billingProfileId":"a",
                 "effectiveFrom":0,"effectiveTo":null,"mixed":false}
            ]));
            let got = resolve_at(&id, 10, &rs);
            assert_eq!(
                got.attribution_kind,
                AttributionKind::UserAssigned,
                "provider {identity_provider:?} 应与相同规则值匹配"
            );
            assert_eq!(got.billing_profile_id.as_deref(), Some("a"));
        }
    }

    /// 同一条规则被其他规则边界切成多段时，ruleIds 必须去重（只出现一次）。
    #[test]
    fn rule_ids_are_deduplicated_across_segments() {
        let id = identity(base_identity());
        let rs = rules(json!([
            {"id":"r1","identity":{"sourceId":"codex","observedProvider":null,"modelId":"gpt-x"},
             "match":"exact","billingProfileId":"a",
             "effectiveFrom":0,"effectiveTo":100,"mixed":false},
            // 不同 source 的规则只贡献边界，不参与本身份匹配。
            {"id":"other","identity":{"sourceId":"claude","observedProvider":null,"modelId":"gpt-x"},
             "match":"exact","billingProfileId":"b",
             "effectiveFrom":50,"effectiveTo":60,"mixed":false}
        ]));
        let got = resolve_bucket(&id, 0, 100, &rs);
        assert_eq!(got.attribution_kind, AttributionKind::UserAssigned);
        assert_eq!(got.billing_profile_id.as_deref(), Some("a"));
        assert_eq!(
            got.rule_ids,
            vec!["r1".to_string()],
            "同一规则跨子段合并后只能出现一次"
        );
    }

    /// 更窄时间区间优先：宽区间候选被严格包含它的窄区间候选支配。
    #[test]
    fn strictly_narrower_interval_wins_over_wider_one() {
        let id = identity(base_identity());
        let rs = rules(json!([
            {"id":"wide","identity":{"sourceId":"codex","observedProvider":null,"modelId":"gpt-x"},
             "match":"exact","billingProfileId":"wide",
             "effectiveFrom":0,"effectiveTo":null,"mixed":false},
            {"id":"narrow","identity":{"sourceId":"codex","observedProvider":null,"modelId":"gpt-x"},
             "match":"exact","billingProfileId":"narrow",
             "effectiveFrom":10,"effectiveTo":20,"mixed":false}
        ]));
        let got = resolve_at(&id, 15, &rs);
        assert_eq!(got.attribution_kind, AttributionKind::UserAssigned);
        assert_eq!(got.billing_profile_id.as_deref(), Some("narrow"));
        assert_eq!(got.rule_ids, vec!["narrow".to_string()]);
    }

    /// 交叉但互不包含的时间区间属同级 ⇒ 不同渠道时 ambiguous。
    #[test]
    fn crossing_intervals_are_same_rank_and_ambiguous() {
        let id = identity(base_identity());
        let rs = rules(json!([
            {"id":"r1","identity":{"sourceId":"codex","observedProvider":null,"modelId":"gpt-x"},
             "match":"exact","billingProfileId":"a",
             "effectiveFrom":0,"effectiveTo":10,"mixed":false},
            {"id":"r2","identity":{"sourceId":"codex","observedProvider":null,"modelId":"gpt-x"},
             "match":"exact","billingProfileId":"b",
             "effectiveFrom":5,"effectiveTo":15,"mixed":false}
        ]));
        let got = resolve_at(&id, 7, &rs);
        assert_eq!(got.attribution_kind, AttributionKind::Ambiguous);
        assert_eq!(got.reason, Some(AttributionReason::Conflict));
    }

    #[test]
    fn merge_bucket_empty_is_unknown_unassigned() {
        let got = merge_bucket(Vec::new());
        assert_eq!(got.attribution_kind, AttributionKind::Unknown);
        assert_eq!(got.reason, Some(AttributionReason::Unassigned));
        assert!(got.rule_ids.is_empty());
        assert!(got.billing_profile_id.is_none());
    }

    // ── 序列化契约 ────────────────────────────────────────────────────────────

    #[test]
    fn trend_selection_serializes_internal_tag_and_selected_id() {
        assert_eq!(
            serde_json::to_value(TrendSelection::All).unwrap(),
            json!({"scope":"all"})
        );
        assert_eq!(
            serde_json::to_value(TrendSelection::Tool {
                selected_id: UsageSourceId::Codex
            })
            .unwrap(),
            json!({"scope":"tool","selectedId":"codex"})
        );
        assert_eq!(
            serde_json::to_value(TrendSelection::BillingProfile {
                selected_id: "profile-a".to_string()
            })
            .unwrap(),
            json!({"scope":"billingProfile","selectedId":"profile-a"})
        );
        assert_eq!(
            serde_json::to_value(TrendSelection::Unknown {
                selected_id: UsageSourceId::WorkBuddy
            })
            .unwrap(),
            json!({"scope":"unknown","selectedId":"workbuddy"})
        );
    }

    #[test]
    fn source_ids_serialize_lowercase() {
        assert_eq!(
            serde_json::to_value(UsageSourceId::WorkBuddy).unwrap(),
            json!("workbuddy")
        );
        assert_eq!(
            serde_json::to_value(UsageSourceId::ZCode).unwrap(),
            json!("zcode")
        );
    }

    #[test]
    fn options_serialize_explicit_null() {
        let id = IdentityKey {
            source_id: UsageSourceId::Codex,
            observed_provider: None,
            model_id: "gpt-x".to_string(),
        };
        assert_eq!(
            serde_json::to_value(&id).unwrap(),
            json!({"sourceId":"codex","observedProvider":null,"modelId":"gpt-x"})
        );

        let attr = Attribution {
            billing_profile_id: None,
            attribution_kind: AttributionKind::Unknown,
            rule_ids: vec![],
            reason: None,
        };
        assert_eq!(
            serde_json::to_value(&attr).unwrap(),
            json!({"billingProfileId":null,"attributionKind":"unknown","ruleIds":[],"reason":null})
        );

        let rule = BillingRule {
            id: "r1".to_string(),
            identity: id,
            match_kind: RuleMatch::Exact,
            billing_profile_id: "a".to_string(),
            effective_from: 0,
            effective_to: None,
            mixed: false,
        };
        assert_eq!(
            serde_json::to_value(&rule).unwrap(),
            json!({
                "id":"r1",
                "identity":{"sourceId":"codex","observedProvider":null,"modelId":"gpt-x"},
                "match":"exact",
                "billingProfileId":"a",
                "effectiveFrom":0,
                "effectiveTo":null,
                "mixed":false
            })
        );
    }

    #[test]
    fn all_structures_use_camel_case_wire_names() {
        let identity = IdentityKey {
            source_id: UsageSourceId::ZCode,
            observed_provider: None,
            model_id: "m".to_string(),
        };
        let buckets = UsageBuckets {
            input_fresh: 1,
            cache_read: 2,
            cache_write: 3,
            output: 4,
        };
        let metrics = UsageMetrics {
            request_total: 6,
            cache_hit_rate: 0.5,
            user_est: None,
            requests: 1,
        };
        let attribution = Attribution {
            billing_profile_id: Some("p".to_string()),
            attribution_kind: AttributionKind::UserAssigned,
            rule_ids: vec!["r".to_string()],
            reason: None,
        };

        let profile = BillingProfile {
            id: "p".to_string(),
            name: "P".to_string(),
            kind: BillingProfileKind::ThirdPartyPlan,
            note: String::new(),
            archived: false,
        };
        assert_eq!(
            serde_json::to_value(&profile).unwrap(),
            json!({"id":"p","name":"P","kind":"thirdPartyPlan","note":"","archived":false})
        );

        let pricing_rule = PricingRule {
            id: "pr".to_string(),
            identity: identity.clone(),
            match_kind: RuleMatch::Prefix,
            reference: PricingReference {
                provider_id: "prov".to_string(),
                model_id: "model".to_string(),
            },
        };
        let config = BillingConfig {
            revision: 3,
            profiles: vec![profile],
            rules: vec![],
            pricing_rules: vec![pricing_rule],
        };
        let config_value = serde_json::to_value(&config).unwrap();
        assert_eq!(
            config_value,
            json!({
                "revision":3,
                "profiles":[{"id":"p","name":"P","kind":"thirdPartyPlan","note":"","archived":false}],
                "rules":[],
                "pricingRules":[{
                    "id":"pr",
                    "identity":{"sourceId":"zcode","observedProvider":null,"modelId":"m"},
                    "match":"prefix",
                    "reference":{"providerId":"prov","modelId":"model"}
                }]
            })
        );

        let usage = IdentityUsage {
            identity: identity.clone(),
            source_kind: SourceKind::Measured,
            attribution: attribution.clone(),
            buckets,
            metrics,
        };
        let usage_value = serde_json::to_value(&usage).unwrap();
        assert_eq!(
            usage_value,
            json!({
                "identity":{"sourceId":"zcode","observedProvider":null,"modelId":"m"},
                "sourceKind":"measured",
                "attribution":{
                    "billingProfileId":"p",
                    "attributionKind":"userAssigned",
                    "ruleIds":["r"],
                    "reason":null
                },
                "buckets":{"inputFresh":1,"cacheRead":2,"cacheWrite":3,"output":4},
                "metrics":{"requestTotal":6,"cacheHitRate":0.5,"userEst":null,"requests":1}
            })
        );

        let row = DistributionRow {
            key: "k".to_string(),
            label: "L".to_string(),
            source_ids: vec![UsageSourceId::WorkBuddy, UsageSourceId::ZCode],
            billing_profile_id: None,
            attribution_kind: AttributionKind::Unknown,
            buckets,
            metrics,
            channel_key: None,
            channel_kind: None,
            display_provider: None,
            observed_provider: None,
            observed_providers: Vec::new(),
            has_empty_provider: false,
            model_id: None,
        };
        assert_eq!(
            serde_json::to_value(&row).unwrap(),
            json!({
                "key":"k",
                "label":"L",
                "sourceIds":["workbuddy","zcode"],
                "billingProfileId":null,
                "attributionKind":"unknown",
                "buckets":{"inputFresh":1,"cacheRead":2,"cacheWrite":3,"output":4},
                "metrics":{"requestTotal":6,"cacheHitRate":0.5,"userEst":null,"requests":1},
                "channelKey":null,
                "channelKind":null,
                "displayProvider":null,
                "observedProvider":null,
                "observedProviders":[],
                "hasEmptyProvider":false,
                "modelId":null
            })
        );

        let trend = UsageTrend {
            selection: TrendSelection::All,
            trend: vec![TrendPoint {
                key: "t".to_string(),
                label: "T".to_string(),
                buckets,
                metrics,
            }],
            hero: 10,
            compare: None,
            available: true,
            reason: None,
        };
        let trend_value = serde_json::to_value(&trend).unwrap();
        assert_eq!(trend_value["selection"], json!({"scope":"all"}));
        assert_eq!(trend_value["compare"], Value::Null);
        assert_eq!(trend_value["reason"], Value::Null);
        assert!(trend_value.get("hero").is_some());

        let distribution = UsageDistribution {
            group_by: DistributionGroup::BillingProfile,
            rows: vec![row],
        };
        assert_eq!(
            serde_json::to_value(&distribution).unwrap()["groupBy"],
            json!("billingProfile")
        );

        let preview = BillingPreview {
            base_revision: 1,
            candidate: config.clone(),
            matched_identities: vec![usage],
            matched_tokens: 1,
            ambiguous_tokens: 2,
            boundary_tokens: 3,
        };
        let preview_value = serde_json::to_value(&preview).unwrap();
        for key in [
            "baseRevision",
            "candidate",
            "matchedIdentities",
            "matchedTokens",
            "ambiguousTokens",
            "boundaryTokens",
        ] {
            assert!(
                preview_value.get(key).is_some(),
                "BillingPreview 缺 camelCase 字段 {key}"
            );
        }

        let commit = BillingCommit {
            expected_revision: 1,
            candidate: config,
        };
        let commit_value = serde_json::to_value(&commit).unwrap();
        assert!(commit_value.get("expectedRevision").is_some());
        assert!(commit_value.get("candidate").is_some());

        let revision = BillingRevision {
            revision: 2,
            committed_at: 100,
        };
        assert_eq!(
            serde_json::to_value(revision).unwrap(),
            json!({"revision":2,"committedAt":100})
        );
    }
}
// ==== usage 自省锁：排除区结束，勿删勿复制 ====
