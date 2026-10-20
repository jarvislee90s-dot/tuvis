//! 账本适配与归属后的查询（Task 4）。
//!
//! * `attribute_rows` 把既有账本行（`LedgerRow`）适配成 V2 身份 + 桶级归因；
//! * `identities_with_conn` / `snapshot_with_conn` 返回完整身份与原始归属行；
//! * `trend_with_conn` / `distribution_with_conn` 提供带局部筛选的趋势与分布。
//!
//! 本模块**只读账本**：复用 `usage::query` 的取数与聚合，不扫描、不采集、不联网。
//! 所有排序都以 `(requestTotal + output)` 降序、稳定 key 升序打破平局，保证输出确定。

use std::collections::{BTreeMap, BTreeSet};

use chrono::{Duration, Local, LocalResult, NaiveDate, NaiveDateTime, TimeZone};
use rusqlite::Connection;

use crate::database::connection::DB;
use crate::services::usage::model::{
    CompareBlock, TrendPoint, UsageFilters, UsageRange, UsageSourceId,
};
use crate::services::usage::query::{
    aggregate, aggregate_source_kind, apply_filters, filter_unavailable_code,
    load_ledger_rows_with, load_rows_for_keys, session_subagent_flags, LedgerRow,
};
use crate::services::usage::range::{day_label, hour_label, resolve_range, RangeGranularity};
use crate::services::usage::semantics::hero_of;
use crate::services::usage::settings;
use crate::services::usage::v2::billing;
use crate::services::usage::v2::channel::{self, join_key, resolve_channel, ChannelResolver};
use crate::services::usage::v2::identity::resolve_bucket;
use crate::services::usage::v2::model::{
    Attribution, AttributionKind, AttributionReason, BillingConfig, BillingRule, ChannelKind,
    ChannelOption, DistributionGroup, DistributionRow, IdentityKey, IdentityUsage, TrendSelection,
    UsageDistribution, UsageTrend,
};
use crate::services::usage::UsageError;

/// 账本行 + V2 身份 + 该行所属桶的归因。
#[derive(Debug, Clone)]
pub(crate) struct AttributedRow {
    pub row: LedgerRow,
    pub identity: IdentityKey,
    pub attribution: Attribution,
}

fn unknown_boundary() -> Attribution {
    Attribution {
        billing_profile_id: None,
        attribution_kind: AttributionKind::Unknown,
        rule_ids: Vec::new(),
        reason: Some(AttributionReason::BucketBoundary),
    }
}

fn parse_source_id(raw: &str) -> Option<UsageSourceId> {
    UsageSourceId::ALL
        .into_iter()
        .find(|source| source.db_id() == raw)
}

fn optional_key(value: Option<&str>) -> String {
    match value {
        None => "n".to_string(),
        Some(value) => format!("s{value}"),
    }
}

fn identity_key(identity: &IdentityKey) -> String {
    join_key(&[
        identity.source_id.db_id(),
        &optional_key(identity.observed_provider.as_deref()),
        &identity.model_id,
    ])
}

fn local_single(value: NaiveDateTime) -> Option<i64> {
    match Local.from_local_datetime(&value) {
        LocalResult::Single(ts) => Some(ts.timestamp_millis()),
        LocalResult::Ambiguous(_, _) | LocalResult::None => None,
    }
}

fn local_midnight(date: NaiveDate) -> Option<i64> {
    local_single(date.and_hms_opt(0, 0, 0)?)
}

/// 桶键 → 宿主本地 `[start, end)`。
///
/// 小时按**本地下一小时**、日按**本地日历下一天**求终点（禁止统一加 24h 毫秒）；
/// DST 多义/不存在、解析失败或 `start >= end` 一律 `None`，由调用层记为 bucketBoundary。
fn bucket_bounds(bucket_key: &str, granularity: RangeGranularity) -> Option<(i64, i64)> {
    let (start, end) = match granularity {
        RangeGranularity::Hour => {
            let start = NaiveDateTime::parse_from_str(bucket_key, "%Y-%m-%dT%H").ok()?;
            let end = start.checked_add_signed(Duration::hours(1))?;
            (local_single(start)?, local_single(end)?)
        }
        RangeGranularity::Day => {
            let start = NaiveDate::parse_from_str(bucket_key, "%Y-%m-%d").ok()?;
            let end = start.checked_add_signed(Duration::days(1))?;
            (local_midnight(start)?, local_midnight(end)?)
        }
    };
    (start < end).then_some((start, end))
}

fn identity_of(row: &LedgerRow, source_id: UsageSourceId) -> IdentityKey {
    IdentityKey {
        source_id,
        observed_provider: (!row.provider.is_empty()).then(|| row.provider.clone()),
        model_id: row.model.clone(),
    }
}

pub(crate) fn attribute_rows(
    rows: Vec<LedgerRow>,
    granularity: RangeGranularity,
    config: &BillingConfig,
) -> Result<Vec<AttributedRow>, UsageError> {
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(source_id) = parse_source_id(&row.source_id) else {
            log::warn!(
                "usage v2: skipping ledger row with unknown sourceId {:?}",
                row.source_id
            );
            continue;
        };
        let identity = identity_of(&row, source_id);
        let attribution = match bucket_bounds(&row.bucket_key, granularity) {
            Some((start, end)) => resolve_bucket(&identity, start, end, &config.rules),
            None => unknown_boundary(),
        };
        out.push(AttributedRow {
            row,
            identity,
            attribution,
        });
    }
    Ok(out)
}

/// 身份级归因：整个查询范围为一段。空区间按 bucketBoundary 处理（不 panic）。
pub(crate) fn resolve_over_range(
    identity: &IdentityKey,
    from_ms: i64,
    to_ms: i64,
    rules: &[BillingRule],
) -> Attribution {
    if from_ms >= to_ms {
        return unknown_boundary();
    }
    resolve_bucket(identity, from_ms, to_ms, rules)
}

/// 把账本行按**完整身份**汇总为 `IdentityUsage`（identities 与 preview 共用）。
pub(crate) fn identity_usages(
    rows: Vec<LedgerRow>,
    from_ms: i64,
    to_ms: i64,
    rules: &[BillingRule],
) -> Result<Vec<IdentityUsage>, UsageError> {
    let mut groups: BTreeMap<String, (IdentityKey, Vec<LedgerRow>)> = BTreeMap::new();
    for row in rows {
        let Some(source_id) = parse_source_id(&row.source_id) else {
            log::warn!(
                "usage v2: skipping ledger row with unknown sourceId {:?}",
                row.source_id
            );
            continue;
        };
        let identity = identity_of(&row, source_id);
        groups
            .entry(identity_key(&identity))
            .or_insert_with(|| (identity, Vec::new()))
            .1
            .push(row);
    }

    let mut out: Vec<IdentityUsage> = groups
        .into_values()
        .map(|(identity, rows)| {
            let (buckets, metrics) = aggregate(&rows);
            let attribution = resolve_over_range(&identity, from_ms, to_ms, rules);
            IdentityUsage {
                source_kind: aggregate_source_kind(&rows),
                identity,
                attribution,
                buckets,
                metrics,
            }
        })
        .collect();
    out.sort_by_key(|usage| {
        (
            std::cmp::Reverse(hero_of(usage.metrics.request_total, usage.buckets.output)),
            identity_key(&usage.identity),
        )
    });
    Ok(out)
}

/// 把已归因的桶行按完整身份汇总（preview 用：归因必须来自候选规则的桶级结果）。
///
/// 组级归因规则（三项互斥）：
/// * 任一行 ambiguous ⇒ ambiguous（mixed 优先）；
/// * 全部行均为同一渠道的 userAssigned ⇒ userAssigned；
/// * 出现未知/边界行（或不同渠道）⇒ unknown/bucketBoundary（完全无归属 ⇒ unassigned）；
/// * `matchedTokens` 因此只计真正可靠归属的身份组。
pub(crate) fn attributed_identity_usages(rows: Vec<AttributedRow>) -> Vec<IdentityUsage> {
    let mut groups: BTreeMap<String, (IdentityKey, Vec<AttributedRow>)> = BTreeMap::new();
    for attributed in rows {
        groups
            .entry(identity_key(&attributed.identity))
            .or_insert_with(|| (attributed.identity.clone(), Vec::new()))
            .1
            .push(attributed);
    }

    let mut out: Vec<IdentityUsage> = groups
        .into_values()
        .map(|(identity, attributed)| {
            let ledger_rows: Vec<LedgerRow> =
                attributed.iter().map(|item| item.row.clone()).collect();
            let (buckets, metrics) = aggregate(&ledger_rows);
            let attribution = fold_bucket_attributions(
                attributed
                    .iter()
                    .map(|item| item.attribution.clone())
                    .collect(),
            );
            IdentityUsage {
                source_kind: aggregate_source_kind(&ledger_rows),
                identity,
                attribution,
                buckets,
                metrics,
            }
        })
        .collect();
    out.sort_by_key(|usage| {
        (
            std::cmp::Reverse(hero_of(usage.metrics.request_total, usage.buckets.output)),
            identity_key(&usage.identity),
        )
    });
    out
}

fn fold_bucket_attributions(parts: Vec<Attribution>) -> Attribution {
    if parts.is_empty() {
        return Attribution {
            billing_profile_id: None,
            attribution_kind: AttributionKind::Unknown,
            rule_ids: Vec::new(),
            reason: Some(AttributionReason::Unassigned),
        };
    }

    let mut rule_ids: Vec<String> = parts
        .iter()
        .flat_map(|part| part.rule_ids.iter().cloned())
        .collect();
    rule_ids.sort_unstable();
    rule_ids.dedup();

    let any_ambiguous = parts
        .iter()
        .any(|part| part.attribution_kind == AttributionKind::Ambiguous);
    if any_ambiguous {
        let mixed = parts
            .iter()
            .any(|part| part.reason == Some(AttributionReason::Mixed));
        return Attribution {
            billing_profile_id: None,
            attribution_kind: AttributionKind::Ambiguous,
            rule_ids,
            reason: Some(if mixed {
                AttributionReason::Mixed
            } else {
                AttributionReason::Conflict
            }),
        };
    }

    let mut profiles: Vec<&str> = parts
        .iter()
        .filter(|part| part.attribution_kind == AttributionKind::UserAssigned)
        .filter_map(|part| part.billing_profile_id.as_deref())
        .collect();
    profiles.sort_unstable();
    profiles.dedup();
    let all_assigned = parts
        .iter()
        .all(|part| part.attribution_kind == AttributionKind::UserAssigned);

    if all_assigned && profiles.len() == 1 {
        return Attribution {
            billing_profile_id: profiles.first().map(|id| (*id).to_string()),
            attribution_kind: AttributionKind::UserAssigned,
            rule_ids,
            reason: None,
        };
    }
    if profiles.len() > 1 {
        return Attribution {
            billing_profile_id: None,
            attribution_kind: AttributionKind::Ambiguous,
            rule_ids,
            reason: Some(AttributionReason::Conflict),
        };
    }
    if profiles.is_empty()
        && parts.iter().all(|part| {
            part.attribution_kind == AttributionKind::Unknown
                && part.reason == Some(AttributionReason::Unassigned)
        })
    {
        return Attribution {
            billing_profile_id: None,
            attribution_kind: AttributionKind::Unknown,
            rule_ids,
            reason: Some(AttributionReason::Unassigned),
        };
    }
    Attribution {
        billing_profile_id: None,
        attribution_kind: AttributionKind::Unknown,
        rule_ids,
        reason: Some(AttributionReason::BucketBoundary),
    }
}

pub fn identities_with_conn(
    conn: &Connection,
    range: &UsageRange,
    filters: &UsageFilters,
    now_ms: i64,
) -> Result<Vec<IdentityUsage>, UsageError> {
    // A-2 守卫必须在总开关之前（与旧 records 一致）。
    if let Some(code) = filter_unavailable_code(range.preset, filters) {
        return Err(UsageError::new(
            code,
            "daily buckets lack session_id; subagentMode=\"parentsOnly\" cannot be evaluated",
        ));
    }
    if !settings::load_from_conn(conn).enabled {
        return Ok(Vec::new());
    }
    let resolved = resolve_range(range, now_ms)?;
    let rows = apply_filters(
        load_ledger_rows_with(conn, &resolved),
        filters,
        &session_subagent_flags(conn),
    );
    let config = billing::load_with_conn(conn)?;
    identity_usages(
        rows,
        resolved.cur.from_ms,
        resolved.cur.to_ms,
        &config.rules,
    )
}

fn empty_trend(selection: &TrendSelection) -> UsageTrend {
    UsageTrend {
        selection: selection.clone(),
        trend: Vec::new(),
        hero: 0,
        compare: None,
        available: true,
        reason: None,
    }
}

fn unavailable_trend(selection: &TrendSelection) -> UsageTrend {
    UsageTrend {
        selection: selection.clone(),
        trend: Vec::new(),
        hero: 0,
        compare: None,
        available: false,
        reason: Some("usage-filter-unavailable".to_string()),
    }
}

/// `BillingProfile { selected_id }` 的 `selected_id` 是**渠道键**：
/// `bp:<profileId>` / `provider:<displayName>`。比较的是解析后的渠道键，
/// 因此别名、空 provider 指派与显式绑定在同一口径下筛选。
fn selection_matches(
    selection: &TrendSelection,
    attributed: &AttributedRow,
    resolver: &ChannelResolver<'_>,
) -> bool {
    match selection {
        TrendSelection::All => true,
        TrendSelection::Tool { selected_id } => attributed.identity.source_id == *selected_id,
        TrendSelection::BillingProfile { selected_id } => {
            resolve_channel(&attributed.identity, &attributed.attribution, resolver).key
                == *selected_id
        }
        // `unknown` 保留 wire 兼容，但查询层不再把它当渠道（入口已返回不可用）。
        TrendSelection::Unknown { .. } => false,
    }
}

fn selection_rows<'a>(
    selection: &TrendSelection,
    rows: &'a [AttributedRow],
    resolver: &ChannelResolver<'_>,
) -> Vec<&'a LedgerRow> {
    rows.iter()
        .filter(|attributed| selection_matches(selection, attributed, resolver))
        .map(|attributed| &attributed.row)
        .collect()
}

/// 渠道键的可用性：`bp:` 必须指向存在且未归档的 profile；`provider:` 直接比较解析键。
fn channel_selection_is_available(config: &BillingConfig, selected_id: &str) -> bool {
    match selected_id.strip_prefix("bp:") {
        Some(profile_id) => config
            .profiles
            .iter()
            .any(|profile| profile.id == profile_id && !profile.archived),
        None => selected_id.starts_with("provider:"),
    }
}

pub fn trend_with_conn(
    conn: &Connection,
    range: &UsageRange,
    selection: &TrendSelection,
    now_ms: i64,
) -> Result<UsageTrend, UsageError> {
    if !settings::load_from_conn(conn).enabled {
        return Ok(empty_trend(selection));
    }
    let resolved = resolve_range(range, now_ms)?;
    let config = billing::load_with_conn(conn)?;
    let aliases = channel::load_with_conn(conn)?;
    let resolver = ChannelResolver::new(&config, &aliases);
    match selection {
        TrendSelection::BillingProfile { selected_id }
            if !channel_selection_is_available(&config, selected_id) =>
        {
            return Ok(unavailable_trend(selection));
        }
        TrendSelection::Unknown { .. } => return Ok(unavailable_trend(selection)),
        _ => {}
    }

    let attributed = attribute_rows(
        load_ledger_rows_with(conn, &resolved),
        resolved.cur.granularity,
        &config,
    )?;
    let selected = selection_rows(selection, &attributed, &resolver);

    let label_of = |key: &str| match resolved.cur.granularity {
        RangeGranularity::Hour => hour_label(key),
        RangeGranularity::Day => day_label(key),
    };
    let trend: Vec<TrendPoint> = resolved
        .cur
        .keys
        .iter()
        .map(|key| {
            let rows: Vec<LedgerRow> = selected
                .iter()
                .filter(|row| &row.bucket_key == key)
                .map(|row| (*row).clone())
                .collect();
            let (buckets, metrics) = aggregate(&rows);
            TrendPoint {
                key: key.clone(),
                label: label_of(key),
                buckets,
                metrics,
            }
        })
        .collect();
    let hero = trend
        .iter()
        .map(|point| hero_of(point.metrics.request_total, point.buckets.output))
        .sum();

    let compare = match resolved.prev.as_ref() {
        Some(prev) => {
            let prev_attributed = attribute_rows(
                load_rows_for_keys(conn, prev.granularity, &prev.keys),
                prev.granularity,
                &config,
            )?;
            let rows: Vec<LedgerRow> = selection_rows(selection, &prev_attributed, &resolver)
                .into_iter()
                .cloned()
                .collect();
            let (prev_buckets, prev_metrics) = aggregate(&rows);
            if prev_metrics.requests == 0 && prev_buckets.total() == 0 {
                None
            } else {
                Some(CompareBlock {
                    prev_buckets,
                    prev_metrics,
                })
            }
        }
        None => None,
    };

    Ok(UsageTrend {
        selection: selection.clone(),
        trend,
        hero,
        compare,
        available: true,
        reason: None,
    })
}

struct GroupAcc {
    label: String,
    source_ids: BTreeSet<String>,
    profile: Option<String>,
    kind: AttributionKind,
    rows: Vec<LedgerRow>,
    attributions: Vec<Attribution>,
    // 渠道/模型维度的结构化字段；tool/project 维度保持 None。
    channel_key: Option<String>,
    channel_kind: Option<ChannelKind>,
    display_provider: Option<String>,
    observed_providers: BTreeSet<String>,
    /// 该组是否包含空 provider 行（混合来源时不得伪装成唯一证据）。
    has_empty_provider: bool,
    model_id: Option<String>,
}

fn fold_attributions(attributions: &[Attribution]) -> (Option<String>, AttributionKind) {
    let all_assigned = !attributions.is_empty()
        && attributions
            .iter()
            .all(|attribution| attribution.attribution_kind == AttributionKind::UserAssigned);
    if all_assigned {
        let mut profiles: Vec<&str> = attributions
            .iter()
            .filter_map(|attribution| attribution.billing_profile_id.as_deref())
            .collect();
        profiles.sort_unstable();
        profiles.dedup();
        if profiles.len() == 1 {
            return (
                profiles.first().map(|id| (*id).to_string()),
                AttributionKind::UserAssigned,
            );
        }
        return (None, AttributionKind::Ambiguous);
    }
    if attributions
        .iter()
        .any(|attribution| attribution.attribution_kind == AttributionKind::Ambiguous)
    {
        return (None, AttributionKind::Ambiguous);
    }
    (None, AttributionKind::Unknown)
}

/// 渠道解析结果 → 行级归因种类的映射（展示分类与归因语义分开存放）。
fn channel_attribution_kind(kind: ChannelKind) -> AttributionKind {
    match kind {
        ChannelKind::BillingProfile => AttributionKind::UserAssigned,
        ChannelKind::Ambiguous => AttributionKind::Ambiguous,
        ChannelKind::Alias | ChannelKind::Provider | ChannelKind::Unassigned => {
            AttributionKind::Unknown
        }
    }
}

/// 分布行的分组身份 + 结构化展示字段（Task 4）。
struct DistributionSlot {
    key: String,
    label: String,
    profile: Option<String>,
    kind: AttributionKind,
    channel_key: Option<String>,
    channel_kind: Option<ChannelKind>,
    display_provider: Option<String>,
    model_id: Option<String>,
}

/// 渠道维度的标签：显示名或 i18n 键；模型维度的 `label` 只放原始 `modelId`。
fn distribution_slot(
    group_by: DistributionGroup,
    attributed: &AttributedRow,
    resolver: &ChannelResolver<'_>,
) -> DistributionSlot {
    match group_by {
        DistributionGroup::Tool => {
            let source = attributed.identity.source_id.db_id();
            DistributionSlot {
                key: join_key(&["tool", source]),
                label: source.to_string(),
                profile: None,
                kind: attributed.attribution.attribution_kind,
                channel_key: None,
                channel_kind: None,
                display_provider: None,
                model_id: None,
            }
        }
        DistributionGroup::Project => {
            let key = attributed.row.project_key.as_str();
            let label = if attributed.row.project_label.is_empty() {
                key.to_string()
            } else {
                attributed.row.project_label.clone()
            };
            DistributionSlot {
                key: join_key(&["project", key]),
                label,
                profile: None,
                kind: attributed.attribution.attribution_kind,
                channel_key: None,
                channel_kind: None,
                display_provider: None,
                model_id: None,
            }
        }
        DistributionGroup::BillingProfile => {
            let channel = resolve_channel(&attributed.identity, &attributed.attribution, resolver);
            let label = match channel.kind {
                ChannelKind::Unassigned => "usage.label.unassignedChannel".to_string(),
                ChannelKind::Ambiguous => "usage.label.ambiguousChannel".to_string(),
                ChannelKind::BillingProfile | ChannelKind::Alias | ChannelKind::Provider => {
                    channel.display_provider.clone().unwrap_or_default()
                }
            };
            DistributionSlot {
                key: join_key(&["channel", &channel.key]),
                label,
                profile: channel.billing_profile_id.clone(),
                kind: channel_attribution_kind(channel.kind),
                channel_key: Some(channel.key),
                channel_kind: Some(channel.kind),
                display_provider: channel.display_provider,
                // 渠道行聚合多个模型，不存在单一 modelId。
                model_id: None,
            }
        }
        DistributionGroup::Model => {
            let channel = resolve_channel(&attributed.identity, &attributed.attribution, resolver);
            let source = attributed.identity.source_id.db_id();
            let model_id = attributed.identity.model_id.clone();
            let key = match channel.kind {
                // 未指派按工具拆行；可能混合额外保留原始 provider 作为证据上下文。
                ChannelKind::Unassigned => join_key(&["model", &channel.key, source, &model_id]),
                ChannelKind::Ambiguous => join_key(&[
                    "model",
                    &channel.key,
                    source,
                    &optional_key(attributed.identity.observed_provider.as_deref()),
                    &model_id,
                ]),
                // 显示渠道 + 原始模型：同显示名跨工具合并。
                ChannelKind::BillingProfile | ChannelKind::Alias | ChannelKind::Provider => {
                    join_key(&["model", &channel.key, &model_id])
                }
            };
            DistributionSlot {
                key,
                label: model_id.clone(),
                profile: channel.billing_profile_id.clone(),
                kind: channel_attribution_kind(channel.kind),
                channel_key: Some(channel.key),
                channel_kind: Some(channel.kind),
                display_provider: channel.display_provider,
                model_id: Some(model_id),
            }
        }
    }
}

pub fn distribution_with_conn(
    conn: &Connection,
    range: &UsageRange,
    group_by: DistributionGroup,
    now_ms: i64,
) -> Result<UsageDistribution, UsageError> {
    if !settings::load_from_conn(conn).enabled {
        return Ok(UsageDistribution {
            group_by,
            rows: Vec::new(),
        });
    }
    let resolved = resolve_range(range, now_ms)?;
    let config = billing::load_with_conn(conn)?;
    let aliases = channel::load_with_conn(conn)?;
    let resolver = ChannelResolver::new(&config, &aliases);
    let attributed = attribute_rows(
        load_ledger_rows_with(conn, &resolved),
        resolved.cur.granularity,
        &config,
    )?;

    let mut groups: BTreeMap<String, GroupAcc> = BTreeMap::new();
    for attributed_row in attributed {
        let slot = distribution_slot(group_by, &attributed_row, &resolver);
        let source = attributed_row.identity.source_id.db_id().to_string();
        let group = groups.entry(slot.key).or_insert_with(|| GroupAcc {
            label: slot.label,
            source_ids: BTreeSet::new(),
            profile: slot.profile,
            kind: slot.kind,
            rows: Vec::new(),
            attributions: Vec::new(),
            channel_key: slot.channel_key,
            channel_kind: slot.channel_kind,
            display_provider: slot.display_provider,
            observed_providers: BTreeSet::new(),
            has_empty_provider: false,
            model_id: slot.model_id,
        });
        match attributed_row.identity.observed_provider.as_deref() {
            Some(observed) if !observed.is_empty() => {
                group.observed_providers.insert(observed.to_string());
            }
            _ => group.has_empty_provider = true,
        }
        group.source_ids.insert(source);
        group.attributions.push(attributed_row.attribution.clone());
        group.rows.push(attributed_row.row);
    }

    let mut rows: Vec<DistributionRow> = groups
        .into_iter()
        .map(|(key, group)| {
            let (buckets, metrics) = aggregate(&group.rows);
            let (billing_profile_id, attribution_kind) = if matches!(
                group_by,
                DistributionGroup::Tool | DistributionGroup::Project
            ) {
                fold_attributions(&group.attributions)
            } else {
                (group.profile, group.kind)
            };
            let source_ids = group
                .source_ids
                .iter()
                .filter_map(|source| parse_source_id(source))
                .collect();
            // 兼容字段 `observedProvider` 只在证据唯一时暴露；同显示名合并多个
            // 原始串（或混入空 provider）时保持 null，完整证据见 `observedProviders`。
            // 工具/项目维度不声明渠道，兼容字段恒为 null。
            let observed_providers: Vec<String> =
                group.observed_providers.iter().cloned().collect();
            let observed_provider = if matches!(
                group_by,
                DistributionGroup::Tool | DistributionGroup::Project
            ) {
                None
            } else if !group.has_empty_provider && observed_providers.len() == 1 {
                observed_providers.first().cloned()
            } else {
                None
            };
            DistributionRow {
                key,
                label: group.label,
                source_ids,
                billing_profile_id,
                attribution_kind,
                buckets,
                metrics,
                channel_key: group.channel_key,
                channel_kind: group.channel_kind,
                display_provider: group.display_provider,
                observed_provider,
                observed_providers,
                has_empty_provider: group.has_empty_provider,
                model_id: group.model_id,
            }
        })
        .collect();
    rows.sort_by_key(|row| {
        (
            std::cmp::Reverse(hero_of(row.metrics.request_total, row.buckets.output)),
            row.key.clone(),
        )
    });
    Ok(UsageDistribution { group_by, rows })
}

// 2026-10-20 计划 §3.4 冻结：既有查询用例的共享取数入口。费用域已改走
// `cost_query::cost_snapshot_with_conn`，本函数生产调用面随之收窄（当前仅测试在用）；
// billing preview 走 `billing.rs` 的 `attribute_rows` + `load_ledger_rows_with` 直连路径，
// 不经本函数——**签名与行为不得改动**，也不得让费用域悄悄改回直连本函数。
#[allow(dead_code)]
pub(crate) fn snapshot_with_conn(
    conn: &Connection,
    range: &UsageRange,
    now_ms: i64,
) -> Result<Vec<AttributedRow>, UsageError> {
    if !settings::load_from_conn(conn).enabled {
        return Ok(Vec::new());
    }
    let resolved = resolve_range(range, now_ms)?;
    let config = billing::load_with_conn(conn)?;
    attribute_rows(
        load_ledger_rows_with(conn, &resolved),
        resolved.cur.granularity,
        &config,
    )
}

/// 当前范围的渠道筛选选项（Task 4，供 B 趋势筛选使用）。
///
/// * 逐行解析显示渠道，按渠道键聚合 `requestTotal + output`；
/// * 只返回 `BillingProfile / Alias / Provider`——未指定渠道与可能混合不是可筛选渠道；
/// * 归档 profile 不进入选项（历史分布行仍可见）；
/// * 同一显示名跨工具/原始串只出一项；Token 降序、key 升序。
pub fn channel_options_with_conn(
    conn: &Connection,
    range: &UsageRange,
    now_ms: i64,
) -> Result<Vec<ChannelOption>, UsageError> {
    if !settings::load_from_conn(conn).enabled {
        return Ok(Vec::new());
    }
    let resolved = resolve_range(range, now_ms)?;
    let config = billing::load_with_conn(conn)?;
    let aliases = channel::load_with_conn(conn)?;
    let resolver = ChannelResolver::new(&config, &aliases);
    let attributed = attribute_rows(
        load_ledger_rows_with(conn, &resolved),
        resolved.cur.granularity,
        &config,
    )?;

    let mut by_key: BTreeMap<String, ChannelOption> = BTreeMap::new();
    for attributed_row in attributed {
        let channel = resolve_channel(
            &attributed_row.identity,
            &attributed_row.attribution,
            &resolver,
        );
        let (kind, billing_profile_id) = match channel.kind {
            ChannelKind::BillingProfile => {
                let Some(profile_id) = channel.billing_profile_id.clone() else {
                    continue;
                };
                let archived = config
                    .profiles
                    .iter()
                    .find(|profile| profile.id == profile_id)
                    .is_none_or(|profile| profile.archived);
                if archived {
                    continue;
                }
                (ChannelKind::BillingProfile, Some(profile_id))
            }
            ChannelKind::Alias => (ChannelKind::Alias, None),
            ChannelKind::Provider => (ChannelKind::Provider, None),
            ChannelKind::Unassigned | ChannelKind::Ambiguous => continue,
        };
        let token_usage = hero_of(
            attributed_row.row.request_total,
            attributed_row.row.buckets.output,
        );
        let label = channel.display_provider.clone().unwrap_or_default();
        let key = channel.key;
        by_key
            .entry(key.clone())
            .and_modify(|option| option.token_usage += token_usage)
            .or_insert(ChannelOption {
                key,
                label,
                kind,
                billing_profile_id,
                token_usage,
            });
    }

    let mut options: Vec<ChannelOption> = by_key.into_values().collect();
    options.sort_by(|a, b| {
        b.token_usage
            .cmp(&a.token_usage)
            .then_with(|| a.key.cmp(&b.key))
    });
    Ok(options)
}

// ── 生产入口（IPC 用）：只负责取 DB 锁，业务全部走 `*_with_conn` ──────────────

fn poisoned() -> UsageError {
    UsageError::new("usage-db-failed", "db poisoned lock")
}

pub fn identities(
    range: &UsageRange,
    filters: &UsageFilters,
    now_ms: i64,
) -> Result<Vec<IdentityUsage>, UsageError> {
    let conn = DB.lock().map_err(|_| poisoned())?;
    identities_with_conn(&conn, range, filters, now_ms)
}

pub fn trend(
    range: &UsageRange,
    selection: &TrendSelection,
    now_ms: i64,
) -> Result<UsageTrend, UsageError> {
    let conn = DB.lock().map_err(|_| poisoned())?;
    trend_with_conn(&conn, range, selection, now_ms)
}

pub fn distribution(
    range: &UsageRange,
    group_by: DistributionGroup,
    now_ms: i64,
) -> Result<UsageDistribution, UsageError> {
    let conn = DB.lock().map_err(|_| poisoned())?;
    distribution_with_conn(&conn, range, group_by, now_ms)
}

pub fn channel_options(range: &UsageRange, now_ms: i64) -> Result<Vec<ChannelOption>, UsageError> {
    let conn = DB.lock().map_err(|_| poisoned())?;
    channel_options_with_conn(&conn, range, now_ms)
}

// ==== usage 自省锁：以下为排除区（测试代码），勿删勿复制 ====
#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::usage::model::{
        SourceKind, UsageBuckets, UsageFilters, UsageMetrics, UsageRangePreset, UsageSourceId,
    };
    use crate::services::usage::v2::billing;
    use crate::services::usage::v2::model::{AttributionKind, AttributionReason, ChannelKind};
    use chrono::TimeZone;
    use rusqlite::{params, Connection};
    use serde_json::{json, Value};

    fn local_ms(y: i32, m: u32, d: u32, h: u32, min: u32) -> i64 {
        chrono::Local
            .with_ymd_and_hms(y, m, d, h, min, 0)
            .single()
            .expect("测试时刻必须存在")
            .timestamp_millis()
    }

    fn now_ms() -> i64 {
        local_ms(2026, 10, 17, 12, 0)
    }

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        conn
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
        project: &str,
        provider: &str,
        model: &str,
        output: i64,
    ) {
        conn.execute(
            "INSERT INTO usage_daily
             (day_key, source_id, project_key, provider, provider_kind, model,
              input_fresh, cache_read, cache_write, output, request_total, requests,
              user_est, updated_at)
             VALUES (?1, ?2, ?3, ?4, 'measured', ?5, 10, 0, 0, ?6, 10, 1, NULL, 0)",
            params![day, source, project, provider, model, output],
        )
        .unwrap();
    }

    #[allow(clippy::too_many_arguments)] // 测试夹具：字段逐列构造，拆结构体反而降低可读性
    fn add_detail(
        conn: &Connection,
        day: &str,
        hour: &str,
        source: &str,
        project: &str,
        provider: &str,
        model: &str,
        output: i64,
    ) {
        conn.execute(
            "INSERT INTO usage_detail
             (source_id, session_id, project_key, hour_key, day_key, model, provider,
              provider_kind, input_fresh, cache_read, cache_write, output,
              request_total, requests, user_est, first_seen_at, updated_at)
             VALUES (?1, 's1', ?2, ?3, ?4, ?5, ?6, 'measured', 10, 0, 0, ?7, 10, 1, NULL, 0, 0)",
            params![source, project, hour, day, model, provider, output],
        )
        .unwrap();
    }

    fn profile(id: &str, name: &str, archived: bool) -> Value {
        json!({"id": id, "name": name, "kind": "api", "note": "", "archived": archived})
    }

    fn rule(
        id: &str,
        source: &str,
        provider: Option<&str>,
        model: &str,
        profile_id: &str,
        from: i64,
        to: Option<i64>,
    ) -> Value {
        json!({
            "id": id,
            "identity": {"sourceId": source, "observedProvider": provider, "modelId": model},
            "match": "exact",
            "billingProfileId": profile_id,
            "effectiveFrom": from,
            "effectiveTo": to,
            "mixed": false
        })
    }

    fn config(profiles: Value, rules: Value) -> BillingConfig {
        serde_json::from_value(json!({
            "revision": 0,
            "profiles": profiles,
            "rules": rules,
            "pricingRules": []
        }))
        .unwrap()
    }

    /// 直接把当前配置写进 state（revision=1），避开提交事务以保持用例聚焦查询层。
    fn put_config(conn: &Connection, config: &BillingConfig) {
        let mut stored = config.clone();
        stored.revision = 1;
        let json = serde_json::to_string(&stored).unwrap();
        conn.execute(
            "INSERT INTO usage_billing_state (singleton, revision, config_json)
             VALUES (1, 1, ?1)
             ON CONFLICT(singleton) DO UPDATE SET
                 revision = excluded.revision,
                 config_json = excluded.config_json",
            params![json],
        )
        .unwrap();
    }

    fn hero(buckets: &UsageBuckets, metrics: &UsageMetrics) -> i64 {
        metrics.request_total + buckets.output
    }

    /// 直接落一条 provider 别名（查询层用例只消费配置，不重复校验服务层）。
    fn add_provider_alias(conn: &Connection, id: &str, provider: &str, display: &str) {
        conn.execute(
            "INSERT INTO usage_provider_alias
             (id, kind, provider_raw, source_id, model_id, display_name, created_at, updated_at)
             VALUES (?1, 'provider', ?2, NULL, NULL, ?3, 0, 0)",
            params![id, provider, display],
        )
        .unwrap();
    }

    /// 直接落一条空 provider 的「工具 + 模型」指派。
    fn add_tool_model_alias(
        conn: &Connection,
        id: &str,
        source: UsageSourceId,
        model: &str,
        display: &str,
    ) {
        conn.execute(
            "INSERT INTO usage_provider_alias
             (id, kind, provider_raw, source_id, model_id, display_name, created_at, updated_at)
             VALUES (?1, 'toolModel', NULL, ?2, ?3, ?4, 0, 0)",
            params![id, source.db_id(), model, display],
        )
        .unwrap();
    }

    /// 夹具：两工具同裸模型、两个 profile、未知一组、同桶换渠道一组。
    fn attributed_fixture() -> Connection {
        let conn = mem();
        let day = "2026-10-17";
        let start = local_ms(2026, 10, 17, 0, 0);
        let mid = local_ms(2026, 10, 17, 12, 0);
        let next = local_ms(2026, 10, 18, 0, 0);
        add_daily(&conn, day, "codex", "", "", "gpt-5", 7);
        add_daily(&conn, day, "claude", "", "", "gpt-5", 11);
        add_daily(&conn, day, "kimi", "", "", "gpt-5", 13);
        add_daily(&conn, day, "codex", "", "", "gpt-switch", 17);
        put_config(
            &conn,
            &config(
                json!([
                    profile("A", "Profile A", false),
                    profile("B", "Profile B", false)
                ]),
                json!([
                    rule("r1", "codex", None, "gpt-5", "A", 0, None),
                    rule("r2", "claude", None, "gpt-5", "B", 0, None),
                    rule("r3", "codex", None, "gpt-switch", "A", start, Some(mid)),
                    rule("r4", "codex", None, "gpt-switch", "B", mid, Some(next))
                ]),
            ),
        );
        conn
    }

    #[test]
    fn attributed_hero_is_conserved_across_all_exports() {
        let conn = attributed_fixture();
        let query_range = range(UsageRangePreset::Last7d);

        let snapshot = snapshot_with_conn(&conn, &query_range, now_ms()).unwrap();
        let snapshot_hero: i64 = snapshot
            .iter()
            .map(|attributed| attributed.row.request_total + attributed.row.buckets.output)
            .sum();
        assert_eq!(
            snapshot_hero, 88,
            "原始四桶口径必须守恒（4×10 + 7+11+13+17）"
        );

        let switching = snapshot
            .iter()
            .find(|attributed| attributed.row.model == "gpt-switch")
            .unwrap();
        assert_eq!(
            switching.attribution.attribution_kind,
            AttributionKind::Ambiguous,
            "同桶换渠道必须 ambiguous，不能静默选赢家"
        );
        assert_eq!(
            switching.attribution.reason,
            Some(AttributionReason::Conflict)
        );

        let identities =
            identities_with_conn(&conn, &query_range, &UsageFilters::default(), now_ms()).unwrap();
        let identities_hero: i64 = identities
            .iter()
            .map(|identity| hero(&identity.buckets, &identity.metrics))
            .sum();
        assert_eq!(identities_hero, 88);

        let trend = trend_with_conn(&conn, &query_range, &TrendSelection::All, now_ms()).unwrap();
        assert_eq!(trend.hero, 88);

        let distribution = distribution_with_conn(
            &conn,
            &query_range,
            DistributionGroup::BillingProfile,
            now_ms(),
        )
        .unwrap();
        let distribution_hero: i64 = distribution
            .rows
            .iter()
            .map(|row| hero(&row.buckets, &row.metrics))
            .sum();
        assert_eq!(distribution_hero, 88);

        let profile_a = distribution
            .rows
            .iter()
            .find(|row| row.billing_profile_id.as_deref() == Some("A"))
            .unwrap();
        assert_eq!(
            hero(&profile_a.buckets, &profile_a.metrics),
            17,
            "明确渠道 A 只能包含可靠归属（codex/gpt-5）"
        );
        assert_eq!(profile_a.source_ids, vec![UsageSourceId::Codex]);

        let ambiguous = distribution
            .rows
            .iter()
            .find(|row| row.attribution_kind == AttributionKind::Ambiguous)
            .unwrap();
        assert_eq!(
            hero(&ambiguous.buckets, &ambiguous.metrics),
            27,
            "ambiguous 行只能包含 gpt-switch"
        );
        assert_eq!(ambiguous.source_ids, vec![UsageSourceId::Codex]);
        assert!(ambiguous.billing_profile_id.is_none());
    }

    #[test]
    fn explicit_channel_trends_exclude_ambiguous_and_unknown() {
        let conn = attributed_fixture();
        let query_range = range(UsageRangePreset::Last7d);

        let profile_a = trend_with_conn(
            &conn,
            &query_range,
            &TrendSelection::BillingProfile {
                selected_id: "bp:A".to_string(),
            },
            now_ms(),
        )
        .unwrap();
        assert!(profile_a.available);
        assert_eq!(profile_a.hero, 17);
        assert_eq!(
            profile_a
                .trend
                .iter()
                .map(|point| point.metrics.request_total)
                .sum::<i64>(),
            10,
            "渠道 A 只应命中 codex/gpt-5"
        );

        // `unknown` 变体保留 wire 兼容，但不再作为渠道筛选口径。
        for selected_id in [UsageSourceId::Codex, UsageSourceId::Kimi] {
            let unknown = trend_with_conn(
                &conn,
                &query_range,
                &TrendSelection::Unknown { selected_id },
                now_ms(),
            )
            .unwrap();
            assert!(!unknown.available, "unknown 选择必须显式不可用");
            assert_eq!(unknown.reason.as_deref(), Some("usage-filter-unavailable"));
            assert_eq!(unknown.hero, 0);
        }

        let tool = trend_with_conn(
            &conn,
            &query_range,
            &TrendSelection::Tool {
                selected_id: UsageSourceId::Codex,
            },
            now_ms(),
        )
        .unwrap();
        assert_eq!(tool.hero, 44, "工具筛选应含该工具所有归属形态（17+27）");
    }

    #[test]
    fn current_and_previous_period_use_the_same_selection() {
        let conn = mem();
        add_daily(&conn, "2026-10-17", "codex", "", "", "gpt-5", 7);
        add_daily(&conn, "2026-10-17", "claude", "", "", "gpt-5", 11);
        add_daily(&conn, "2026-10-10", "codex", "", "", "gpt-5", 3);
        add_daily(&conn, "2026-10-10", "claude", "", "", "gpt-5", 5);
        put_config(
            &conn,
            &config(
                json!([
                    profile("A", "Profile A", false),
                    profile("B", "Profile B", false)
                ]),
                json!([
                    rule("r1", "codex", None, "gpt-5", "A", 0, None),
                    rule("r2", "claude", None, "gpt-5", "B", 0, None)
                ]),
            ),
        );

        let query_range = range(UsageRangePreset::Last7d);

        let tool = trend_with_conn(
            &conn,
            &query_range,
            &TrendSelection::Tool {
                selected_id: UsageSourceId::Codex,
            },
            now_ms(),
        )
        .unwrap();
        assert_eq!(tool.hero, 17);
        let tool_prev = tool.compare.expect("上一周期有 codex 数据");
        assert_eq!(tool_prev.prev_metrics.request_total, 10);
        assert_eq!(tool_prev.prev_buckets.output, 3, "上期必须仍按 codex 筛选");

        let channel = trend_with_conn(
            &conn,
            &query_range,
            &TrendSelection::BillingProfile {
                selected_id: "bp:A".to_string(),
            },
            now_ms(),
        )
        .unwrap();
        assert_eq!(channel.hero, 17);
        let channel_prev = channel.compare.expect("上一周期有渠道 A 数据");
        assert_eq!(channel_prev.prev_metrics.request_total, 10);
        assert_eq!(
            channel_prev.prev_buckets.output, 3,
            "上期必须用同一渠道筛选（不能退化成全部工具）"
        );

        let all = trend_with_conn(&conn, &query_range, &TrendSelection::All, now_ms()).unwrap();
        let all_prev = all.compare.unwrap();
        assert_eq!(all_prev.prev_metrics.request_total, 20);
        assert_eq!(all_prev.prev_buckets.output, 8);
    }

    #[test]
    fn day_tier_does_not_read_detail_rows() {
        let conn = mem();
        add_detail(
            &conn,
            "2026-10-17",
            "2026-10-17T10",
            "codex",
            "",
            "",
            "gpt-5",
            7,
        );
        let query_range = range(UsageRangePreset::Last7d);
        let trend = trend_with_conn(&conn, &query_range, &TrendSelection::All, now_ms()).unwrap();
        assert_eq!(trend.hero, 0, "日档只读 usage_daily");
        assert_eq!(trend.trend.len(), 7);
        assert!(trend
            .trend
            .iter()
            .all(|point| point.metrics.request_total == 0));
        assert!(
            identities_with_conn(&conn, &query_range, &UsageFilters::default(), now_ms())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn identities_keep_source_id_under_project_filter() {
        let conn = mem();
        add_daily(&conn, "2026-10-17", "codex", "p1", "", "gpt-5", 7);
        add_daily(&conn, "2026-10-17", "claude", "p2", "", "gpt-5", 11);
        let filters = UsageFilters {
            projects: Some(vec!["p1".to_string()]),
            ..UsageFilters::default()
        };
        let identities =
            identities_with_conn(&conn, &range(UsageRangePreset::Last7d), &filters, now_ms())
                .unwrap();
        assert_eq!(identities.len(), 1);
        assert_eq!(identities[0].identity.source_id, UsageSourceId::Codex);
        assert_eq!(identities[0].identity.model_id, "gpt-5");
    }

    #[test]
    fn daily_parents_only_is_rejected_before_the_enabled_switch() {
        let conn = mem();
        let settings = crate::services::usage::model::UsageSettings {
            enabled: false,
            ..Default::default()
        };
        let raw = serde_json::to_string(&settings).unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('usage.settings', ?1)",
            params![raw],
        )
        .unwrap();
        let filters = UsageFilters {
            subagent_mode: Some(crate::services::usage::model::SubagentMode::ParentsOnly),
            ..UsageFilters::default()
        };
        let err = identities_with_conn(&conn, &range(UsageRangePreset::Last7d), &filters, now_ms())
            .unwrap_err();
        assert_eq!(err.code, "usage-filter-unavailable");
    }

    #[test]
    fn model_distribution_splits_profiles_and_unknown_sources_with_stable_keys() {
        let conn = mem();
        add_daily(&conn, "2026-10-17", "codex", "", "", "gpt-5", 5);
        add_daily(&conn, "2026-10-17", "claude", "", "", "gpt-5", 5);
        add_daily(&conn, "2026-10-17", "kimi", "", "", "gpt-5", 5);
        add_daily(&conn, "2026-10-17", "zcode", "", "", "gpt-5", 5);
        put_config(
            &conn,
            &config(
                json!([
                    profile("A", "Profile A", false),
                    profile("B", "Profile B", false)
                ]),
                json!([
                    rule("r1", "codex", None, "gpt-5", "A", 0, None),
                    rule("r2", "claude", None, "gpt-5", "B", 0, None)
                ]),
            ),
        );

        let distribution = distribution_with_conn(
            &conn,
            &range(UsageRangePreset::Last7d),
            DistributionGroup::Model,
            now_ms(),
        )
        .unwrap();
        assert_eq!(distribution.rows.len(), 4, "两个渠道 + 两个未知来源各一行");
        assert_eq!(
            distribution
                .rows
                .iter()
                .filter(|row| row.attribution_kind == AttributionKind::UserAssigned)
                .count(),
            2
        );
        for row in &distribution.rows {
            assert_eq!(
                row.source_ids.len(),
                1,
                "每个模型的每行只应来自一个来源：{row:?}"
            );
            assert_eq!(row.label, "gpt-5", "模型维度 label 只放原始 modelId");
            assert_eq!(row.model_id.as_deref(), Some("gpt-5"));
            if row.channel_kind != Some(ChannelKind::Unassigned) {
                assert!(row.display_provider.is_some());
            }
        }
        let profile_a = distribution
            .rows
            .iter()
            .find(|row| row.billing_profile_id.as_deref() == Some("A"))
            .unwrap();
        assert_eq!(profile_a.channel_key.as_deref(), Some("bp:A"));
        assert_eq!(profile_a.channel_kind, Some(ChannelKind::BillingProfile));
        assert_eq!(profile_a.display_provider.as_deref(), Some("Profile A"));
        let unknown_kimi = distribution
            .rows
            .iter()
            .find(|row| row.source_ids == vec![UsageSourceId::Kimi])
            .unwrap();
        assert_eq!(unknown_kimi.channel_key.as_deref(), Some("unassigned"));
        assert_eq!(unknown_kimi.channel_kind, Some(ChannelKind::Unassigned));
        assert!(unknown_kimi.display_provider.is_none());
        assert_eq!(
            unknown_kimi.key,
            join_key(&["model", "unassigned", "kimi", "gpt-5"]),
            "未指派模型行必须按工具拆行"
        );
        assert!(
            distribution
                .rows
                .windows(2)
                .all(|pair| pair[0].key < pair[1].key),
            "同 hero 时 key 必须稳定升序"
        );
    }

    #[test]
    fn bucket_boundary_inside_day_is_bucket_boundary_or_ambiguous() {
        let conn = mem();
        let start = local_ms(2026, 10, 17, 0, 0);
        let mid = local_ms(2026, 10, 17, 12, 0);
        let next = local_ms(2026, 10, 18, 0, 0);
        add_daily(&conn, "2026-10-17", "codex", "", "", "gpt-boundary", 1);
        add_daily(&conn, "2026-10-17", "codex", "", "", "gpt-switch", 2);
        put_config(
            &conn,
            &config(
                json!([
                    profile("A", "Profile A", false),
                    profile("B", "Profile B", false)
                ]),
                json!([
                    rule(
                        "boundary",
                        "codex",
                        None,
                        "gpt-boundary",
                        "A",
                        mid,
                        Some(next)
                    ),
                    rule("s1", "codex", None, "gpt-switch", "A", start, Some(mid)),
                    rule("s2", "codex", None, "gpt-switch", "B", mid, Some(next))
                ]),
            ),
        );

        let snapshot =
            snapshot_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms()).unwrap();
        let boundary = snapshot
            .iter()
            .find(|attributed| attributed.row.model == "gpt-boundary")
            .unwrap();
        assert_eq!(
            boundary.attribution.attribution_kind,
            AttributionKind::Unknown
        );
        assert_eq!(
            boundary.attribution.reason,
            Some(AttributionReason::BucketBoundary),
            "半桶无覆盖必须 bucketBoundary"
        );

        let switching = snapshot
            .iter()
            .find(|attributed| attributed.row.model == "gpt-switch")
            .unwrap();
        assert_eq!(
            switching.attribution.attribution_kind,
            AttributionKind::Ambiguous
        );
    }

    #[test]
    fn archived_or_missing_billing_profile_is_unavailable() {
        let conn = mem();
        add_daily(&conn, "2026-10-17", "codex", "", "openai", "gpt-5", 7);
        add_daily(
            &conn,
            "2026-10-17",
            "claude",
            "",
            "anthropic",
            "claude-x",
            11,
        );
        put_config(
            &conn,
            &config(
                json!([
                    profile("A", "Archived", true),
                    profile("B", "Active", false)
                ]),
                json!([
                    rule("r1", "codex", Some("openai"), "gpt-5", "A", 0, None),
                    rule("r2", "claude", Some("anthropic"), "claude-x", "B", 0, None)
                ]),
            ),
        );
        let query_range = range(UsageRangePreset::Last7d);

        // 归档渠道仍可显示历史行。
        let distribution = distribution_with_conn(
            &conn,
            &query_range,
            DistributionGroup::BillingProfile,
            now_ms(),
        )
        .unwrap();
        let archived = distribution
            .rows
            .iter()
            .find(|row| row.channel_key.as_deref() == Some("bp:A"))
            .expect("归档渠道必须仍出现在分布中");
        assert_eq!(archived.channel_kind, Some(ChannelKind::BillingProfile));
        assert_eq!(archived.display_provider.as_deref(), Some("Archived"));
        assert_eq!(hero(&archived.buckets, &archived.metrics), 17);

        // 趋势选项排除归档渠道；同一显示名跨工具只出一项由 key 聚合保证。
        let options = channel_options_with_conn(&conn, &query_range, now_ms()).unwrap();
        assert_eq!(options.len(), 1, "归档渠道不得进入趋势选项");
        assert_eq!(options[0].key, "bp:B");
        assert_eq!(options[0].label, "Active");
        assert_eq!(options[0].kind, ChannelKind::BillingProfile);
        assert_eq!(options[0].billing_profile_id.as_deref(), Some("B"));
        assert_eq!(options[0].token_usage, 21);

        for selected_id in ["bp:A", "bp:missing", "A", "unassigned", "ambiguous"] {
            let trend = trend_with_conn(
                &conn,
                &query_range,
                &TrendSelection::BillingProfile {
                    selected_id: selected_id.to_string(),
                },
                now_ms(),
            )
            .unwrap();
            assert!(!trend.available, "{selected_id} 不应可用");
            assert_eq!(trend.reason.as_deref(), Some("usage-filter-unavailable"));
            assert!(trend.trend.is_empty());
            assert_eq!(trend.hero, 0);
        }

        let unknown = trend_with_conn(
            &conn,
            &query_range,
            &TrendSelection::Unknown {
                selected_id: UsageSourceId::Codex,
            },
            now_ms(),
        )
        .unwrap();
        assert!(!unknown.available, "unknown 选择不再作为趋势筛选口径");
        assert_eq!(unknown.reason.as_deref(), Some("usage-filter-unavailable"));

        let active = trend_with_conn(
            &conn,
            &query_range,
            &TrendSelection::BillingProfile {
                selected_id: "bp:B".to_string(),
            },
            now_ms(),
        )
        .unwrap();
        assert!(active.available);
        assert_eq!(active.hero, 21);
    }

    #[test]
    fn disabled_switch_returns_empty_v2_queries() {
        let conn = attributed_fixture();
        let settings = crate::services::usage::model::UsageSettings {
            enabled: false,
            ..Default::default()
        };
        let raw = serde_json::to_string(&settings).unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('usage.settings', ?1)",
            params![raw],
        )
        .unwrap();
        let query_range = range(UsageRangePreset::Last7d);

        let trend = trend_with_conn(&conn, &query_range, &TrendSelection::All, now_ms()).unwrap();
        assert!(trend.trend.is_empty());
        assert_eq!(trend.hero, 0);
        assert!(trend.available);
        assert!(
            distribution_with_conn(&conn, &query_range, DistributionGroup::Tool, now_ms())
                .unwrap()
                .rows
                .is_empty()
        );
        assert!(
            identities_with_conn(&conn, &query_range, &UsageFilters::default(), now_ms())
                .unwrap()
                .is_empty()
        );
        assert!(snapshot_with_conn(&conn, &query_range, now_ms())
            .unwrap()
            .is_empty());
        assert!(
            channel_options_with_conn(&conn, &query_range, now_ms())
                .unwrap()
                .is_empty(),
            "总开关关闭时渠道选项必须为空"
        );
    }

    #[test]
    fn invalid_source_ids_are_skipped_without_panicking() {
        let conn = mem();
        add_daily(&conn, "2026-10-17", "not-a-tool", "", "", "gpt-5", 7);
        assert!(
            snapshot_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn overflowing_hour_bucket_key_is_boundary_not_panic() {
        let row = LedgerRow {
            bucket_key: "262143-12-31T23".to_string(),
            source_id: "codex".to_string(),
            session_id: String::new(),
            project_key: String::new(),
            project_label: String::new(),
            provider: String::new(),
            provider_kind: SourceKind::Measured,
            model: "gpt-5".to_string(),
            buckets: UsageBuckets::default(),
            request_total: 0,
            requests: 0,
            user_est: None,
        };
        let empty = BillingConfig {
            revision: 0,
            profiles: Vec::new(),
            rules: Vec::new(),
            pricing_rules: Vec::new(),
        };
        let attributed = attribute_rows(vec![row], RangeGranularity::Hour, &empty).unwrap();
        assert_eq!(attributed.len(), 1);
        assert_eq!(
            attributed[0].attribution.attribution_kind,
            AttributionKind::Unknown
        );
        assert_eq!(
            attributed[0].attribution.reason,
            Some(AttributionReason::BucketBoundary),
            "时间溢出必须降级为 bucketBoundary，不得 panic"
        );
    }

    /// `LedgerRow` 没有现成的 `UsageMetrics`：按同一口径从四桶/请求输入重算。
    #[test]
    fn preview_is_implemented_in_billing() {
        let conn = mem();
        let candidate = config(json!([]), json!([]));
        let preview = billing::preview_with_conn(
            &conn,
            &candidate,
            &range(UsageRangePreset::Last7d),
            now_ms(),
        )
        .unwrap();
        assert_eq!(preview.base_revision, 0);
    }

    // ── Task 4：趋势 / 分布 / 渠道选项使用解析后的显示渠道 ─────────────────────

    #[test]
    fn channel_distribution_merges_display_names_across_tools_and_raw_providers() {
        let conn = mem();
        let day = "2026-10-17";
        add_daily(&conn, day, "codex", "", "openai", "gpt-5", 7);
        add_daily(&conn, day, "claude", "", "openai", "gpt-5", 11);
        add_daily(&conn, day, "kimi", "", "azure-openai", "gpt-5", 13);
        add_provider_alias(&conn, "a1", "openai", "OpenAI");
        add_provider_alias(&conn, "a2", "azure-openai", "OpenAI");

        let query_range = range(UsageRangePreset::Last7d);
        let channels = distribution_with_conn(
            &conn,
            &query_range,
            DistributionGroup::BillingProfile,
            now_ms(),
        )
        .unwrap();
        assert_eq!(channels.rows.len(), 1, "同一显示名跨工具、跨原始串只出一行");
        let row = &channels.rows[0];
        assert_eq!(row.key, join_key(&["channel", "provider:OpenAI"]));
        assert_eq!(row.label, "OpenAI");
        assert_eq!(row.channel_key.as_deref(), Some("provider:OpenAI"));
        assert_eq!(row.channel_kind, Some(ChannelKind::Alias));
        assert_eq!(row.display_provider.as_deref(), Some("OpenAI"));
        assert!(
            row.observed_provider.is_none(),
            "两个原始 provider 合并后不得伪造单一证据"
        );
        assert_eq!(
            row.observed_providers,
            vec!["azure-openai".to_string(), "openai".to_string()],
            "合并渠道必须保留全部去重后的原始 provider"
        );
        assert!(
            !row.has_empty_provider,
            "组内没有空 provider 行时不得标记未标注来源"
        );
        assert!(
            row.model_id.is_none(),
            "渠道维度聚合多个模型，不填写单一 modelId"
        );
        assert_eq!(
            row.source_ids,
            vec![
                UsageSourceId::Claude,
                UsageSourceId::Codex,
                UsageSourceId::Kimi
            ]
        );
        assert_eq!(hero(&row.buckets, &row.metrics), 61);

        let models =
            distribution_with_conn(&conn, &query_range, DistributionGroup::Model, now_ms())
                .unwrap();
        assert_eq!(models.rows.len(), 1, "同显示渠道 + 同模型跨工具合并");
        let model = &models.rows[0];
        assert_eq!(model.key, join_key(&["model", "provider:OpenAI", "gpt-5"]));
        assert_eq!(model.label, "gpt-5", "模型维度 label 只放原始 modelId");
        assert_eq!(model.channel_key.as_deref(), Some("provider:OpenAI"));
        assert_eq!(model.channel_kind, Some(ChannelKind::Alias));
        assert_eq!(model.display_provider.as_deref(), Some("OpenAI"));
        assert_eq!(
            model.observed_providers,
            vec!["azure-openai".to_string(), "openai".to_string()]
        );
        assert!(!model.has_empty_provider);
        assert_eq!(model.model_id.as_deref(), Some("gpt-5"));
        assert_eq!(hero(&model.buckets, &model.metrics), 61);

        let tools =
            distribution_with_conn(&conn, &query_range, DistributionGroup::Tool, now_ms()).unwrap();
        assert!(tools.rows.iter().all(|row| {
            row.channel_key.is_none()
                && row.channel_kind.is_none()
                && row.display_provider.is_none()
                && row.observed_provider.is_none()
                && row.model_id.is_none()
        }));
    }

    #[test]
    fn merged_group_with_empty_provider_rows_has_no_single_evidence() {
        let conn = mem();
        let day = "2026-10-17";
        add_daily(&conn, day, "codex", "", "openai", "gpt-5", 7);
        add_daily(&conn, day, "claude", "", "", "gpt-5", 11);
        add_provider_alias(&conn, "a1", "openai", "OpenAI");
        add_tool_model_alias(&conn, "a2", UsageSourceId::Claude, "gpt-5", "OpenAI");

        let query_range = range(UsageRangePreset::Last7d);
        let channels = distribution_with_conn(
            &conn,
            &query_range,
            DistributionGroup::BillingProfile,
            now_ms(),
        )
        .unwrap();
        assert_eq!(channels.rows.len(), 1, "别名把空 provider 行并入同一渠道");
        let row = &channels.rows[0];
        assert_eq!(row.channel_key.as_deref(), Some("provider:OpenAI"));
        assert!(
            row.observed_provider.is_none(),
            "组内混入空 provider 行时不得把唯一非空串伪装成完整证据"
        );
        assert_eq!(
            row.observed_providers,
            vec!["openai".to_string()],
            "非空原始 provider 仍完整保留"
        );
        assert!(
            row.has_empty_provider,
            "组内混入空 provider 行时必须标记未标注来源（wire 面证据不完整）"
        );
    }

    #[test]
    fn tool_and_project_groups_keep_provider_evidence_without_single_value() {
        let conn = mem();
        let day = "2026-10-17";
        add_daily(&conn, day, "codex", "proj-a", "openai", "gpt-5", 7);
        add_daily(&conn, day, "codex", "proj-a", "azure-openai", "gpt-4", 11);
        add_daily(&conn, day, "claude", "proj-b", "", "gpt-5", 13);

        let query_range = range(UsageRangePreset::Last7d);
        let tools =
            distribution_with_conn(&conn, &query_range, DistributionGroup::Tool, now_ms()).unwrap();
        let codex = tools.rows.iter().find(|row| row.label == "codex").unwrap();
        assert_eq!(
            codex.observed_providers,
            vec!["azure-openai".to_string(), "openai".to_string()],
            "工具维度按组保留全部去重非空 provider"
        );
        assert!(
            codex.observed_provider.is_none(),
            "工具维度不暴露单值证据（即使组内恰好一个非空 provider）"
        );
        assert!(!codex.has_empty_provider, "该组没有空 provider 行");
        let claude = tools.rows.iter().find(|row| row.label == "claude").unwrap();
        assert!(
            claude.observed_providers.is_empty(),
            "组内只有空 provider 时证据列表为空"
        );
        assert!(claude.observed_provider.is_none());
        assert!(claude.has_empty_provider, "组内只有空 provider 行");

        let projects =
            distribution_with_conn(&conn, &query_range, DistributionGroup::Project, now_ms())
                .unwrap();
        let proj_a = projects
            .rows
            .iter()
            .find(|row| row.label == "proj-a")
            .unwrap();
        assert_eq!(
            proj_a.observed_providers,
            vec!["azure-openai".to_string(), "openai".to_string()],
            "项目维度按组保留全部去重非空 provider"
        );
        assert!(proj_a.observed_provider.is_none(), "项目维度不暴露单值证据");
        assert!(!proj_a.has_empty_provider);
        let proj_b = projects
            .rows
            .iter()
            .find(|row| row.label == "proj-b")
            .unwrap();
        assert!(proj_b.observed_providers.is_empty());
        assert!(proj_b.observed_provider.is_none());
        assert!(proj_b.has_empty_provider);
    }

    #[test]
    fn unassigned_channels_merge_in_one_row_while_models_split_by_tool() {
        let conn = mem();
        let day = "2026-10-17";
        add_daily(&conn, day, "codex", "", "", "gpt-5", 5);
        add_daily(&conn, day, "claude", "", "", "gpt-5", 7);
        add_daily(&conn, day, "kimi", "", "", "", 3);

        let query_range = range(UsageRangePreset::Last7d);
        let channels = distribution_with_conn(
            &conn,
            &query_range,
            DistributionGroup::BillingProfile,
            now_ms(),
        )
        .unwrap();
        assert_eq!(channels.rows.len(), 1, "未指定渠道全局只有一行");
        let channel = &channels.rows[0];
        assert_eq!(channel.key, join_key(&["channel", "unassigned"]));
        assert_eq!(channel.label, "usage.label.unassignedChannel");
        assert_eq!(channel.channel_key.as_deref(), Some("unassigned"));
        assert_eq!(channel.channel_kind, Some(ChannelKind::Unassigned));
        assert!(channel.display_provider.is_none());
        assert!(channel.has_empty_provider, "三行原始 provider 均为空");
        assert!(channel.model_id.is_none(), "渠道维度不附带工具/模型上下文");
        assert_eq!(
            channel.source_ids,
            vec![
                UsageSourceId::Claude,
                UsageSourceId::Codex,
                UsageSourceId::Kimi
            ]
        );
        assert_eq!(hero(&channel.buckets, &channel.metrics), 45);

        let models =
            distribution_with_conn(&conn, &query_range, DistributionGroup::Model, now_ms())
                .unwrap();
        assert_eq!(models.rows.len(), 3, "未指派模型必须按工具拆行");
        let codex = models
            .rows
            .iter()
            .find(|row| row.source_ids == vec![UsageSourceId::Codex])
            .unwrap();
        assert_eq!(
            codex.key,
            join_key(&["model", "unassigned", "codex", "gpt-5"])
        );
        assert_eq!(codex.label, "gpt-5");
        assert_eq!(codex.channel_kind, Some(ChannelKind::Unassigned));
        assert!(codex.has_empty_provider);
        assert_eq!(codex.model_id.as_deref(), Some("gpt-5"));
        let claude = models
            .rows
            .iter()
            .find(|row| row.source_ids == vec![UsageSourceId::Claude])
            .unwrap();
        assert_eq!(
            claude.key,
            join_key(&["model", "unassigned", "claude", "gpt-5"])
        );
        assert_ne!(codex.key, claude.key, "同名模型不得跨工具合并");
        let unknown_model = models
            .rows
            .iter()
            .find(|row| row.source_ids == vec![UsageSourceId::Kimi])
            .unwrap();
        assert_eq!(
            unknown_model.key,
            join_key(&["model", "unassigned", "kimi", ""]),
            "空 modelId 仍保留空串，前端显示「未知模型」"
        );
        assert_eq!(unknown_model.label, "");
        assert_eq!(unknown_model.model_id.as_deref(), Some(""));
    }

    #[test]
    fn tool_model_alias_assigns_empty_provider_rows_to_a_channel() {
        let conn = mem();
        add_daily(&conn, "2026-10-17", "codex", "", "", "gpt-5", 5);
        add_tool_model_alias(&conn, "t1", UsageSourceId::Codex, "gpt-5", "Codex 订阅");

        let query_range = range(UsageRangePreset::Last7d);
        let channels = distribution_with_conn(
            &conn,
            &query_range,
            DistributionGroup::BillingProfile,
            now_ms(),
        )
        .unwrap();
        assert_eq!(channels.rows.len(), 1);
        let channel = &channels.rows[0];
        assert_eq!(channel.key, join_key(&["channel", "provider:Codex 订阅"]));
        assert_eq!(channel.channel_key.as_deref(), Some("provider:Codex 订阅"));
        assert_eq!(channel.channel_kind, Some(ChannelKind::Alias));
        assert_eq!(channel.label, "Codex 订阅");
        assert_eq!(channel.display_provider.as_deref(), Some("Codex 订阅"));
        assert!(
            channel.observed_provider.is_none(),
            "空 provider 指派不产生原始 provider 证据"
        );
        assert!(channel.observed_providers.is_empty());
        assert!(
            channel.has_empty_provider,
            "规则把空 provider 行指派进渠道，证据面仍是不完整"
        );

        let models =
            distribution_with_conn(&conn, &query_range, DistributionGroup::Model, now_ms())
                .unwrap();
        assert_eq!(models.rows.len(), 1);
        assert!(models.rows[0].has_empty_provider);
        assert_eq!(
            models.rows[0].key,
            join_key(&["model", "provider:Codex 订阅", "gpt-5"])
        );

        let options = channel_options_with_conn(&conn, &query_range, now_ms()).unwrap();
        assert_eq!(options.len(), 1);
        assert_eq!(options[0].key, "provider:Codex 订阅");
        assert_eq!(options[0].kind, ChannelKind::Alias);
        assert_eq!(options[0].token_usage, 15);
    }

    #[test]
    fn explicit_billing_binding_overrides_provider_alias_in_distribution() {
        let conn = mem();
        let day = "2026-10-17";
        add_daily(&conn, day, "codex", "", "openai", "gpt-5", 7);
        add_daily(&conn, day, "claude", "", "openai", "gpt-5", 11);
        add_provider_alias(&conn, "a1", "openai", "OpenAI");
        put_config(
            &conn,
            &config(
                json!([profile("A", "Profile A", false)]),
                json!([rule("r1", "codex", Some("openai"), "gpt-5", "A", 0, None)]),
            ),
        );

        let query_range = range(UsageRangePreset::Last7d);
        let channels = distribution_with_conn(
            &conn,
            &query_range,
            DistributionGroup::BillingProfile,
            now_ms(),
        )
        .unwrap();
        assert_eq!(channels.rows.len(), 2);
        let bound = channels
            .rows
            .iter()
            .find(|row| row.channel_kind == Some(ChannelKind::BillingProfile))
            .unwrap();
        assert_eq!(bound.key, join_key(&["channel", "bp:A"]));
        assert_eq!(bound.channel_key.as_deref(), Some("bp:A"));
        assert_eq!(bound.label, "Profile A");
        assert_eq!(bound.display_provider.as_deref(), Some("Profile A"));
        assert_eq!(bound.billing_profile_id.as_deref(), Some("A"));
        assert_eq!(bound.observed_provider.as_deref(), Some("openai"));
        assert_eq!(bound.observed_providers, vec!["openai".to_string()]);
        assert!(!bound.has_empty_provider);
        assert_eq!(hero(&bound.buckets, &bound.metrics), 17);
        assert_eq!(bound.source_ids, vec![UsageSourceId::Codex]);

        let aliased = channels
            .rows
            .iter()
            .find(|row| row.channel_kind == Some(ChannelKind::Alias))
            .unwrap();
        assert_eq!(aliased.key, join_key(&["channel", "provider:OpenAI"]));
        assert_eq!(aliased.label, "OpenAI");
        assert_eq!(hero(&aliased.buckets, &aliased.metrics), 21);
        assert_eq!(aliased.source_ids, vec![UsageSourceId::Claude]);

        let models =
            distribution_with_conn(&conn, &query_range, DistributionGroup::Model, now_ms())
                .unwrap();
        assert_eq!(models.rows.len(), 2);
        assert!(models
            .rows
            .iter()
            .any(|row| row.key == join_key(&["model", "bp:A", "gpt-5"])));
        assert!(models
            .rows
            .iter()
            .any(|row| row.key == join_key(&["model", "provider:OpenAI", "gpt-5"])));
    }

    #[test]
    fn channel_options_merge_display_names_and_sort_by_token_usage() {
        let conn = mem();
        let day = "2026-10-17";
        add_daily(&conn, day, "codex", "", "openai", "gpt-5", 5);
        add_daily(&conn, day, "claude", "", "azure-openai", "gpt-5", 7);
        add_daily(&conn, day, "kimi", "", "moonshot", "kimi-k2", 13);
        add_daily(&conn, day, "workbuddy", "", "", "gpt-5", 1);
        add_daily(&conn, day, "zcode", "", "mixed", "m", 9);
        add_provider_alias(&conn, "a1", "openai", "OpenAI");
        add_provider_alias(&conn, "a2", "azure-openai", "OpenAI");
        put_config(
            &conn,
            &config(
                json!([
                    profile("A", "Profile A", false),
                    profile("B", "Profile B", false)
                ]),
                json!([
                    rule("r1", "zcode", Some("mixed"), "m", "A", 0, None),
                    rule("r2", "zcode", Some("mixed"), "m", "B", 0, None)
                ]),
            ),
        );

        let options =
            channel_options_with_conn(&conn, &range(UsageRangePreset::Last7d), now_ms()).unwrap();
        assert_eq!(options.len(), 2, "未指定渠道与可能混合不得成为选项");
        assert_eq!(options[0].key, "provider:OpenAI");
        assert_eq!(options[0].label, "OpenAI");
        assert_eq!(options[0].kind, ChannelKind::Alias);
        assert!(options[0].billing_profile_id.is_none());
        assert_eq!(
            options[0].token_usage, 32,
            "同显示名跨工具、跨原始 provider 合并 Token"
        );
        assert_eq!(options[1].key, "provider:moonshot");
        assert_eq!(options[1].label, "moonshot");
        assert_eq!(options[1].kind, ChannelKind::Provider);
        assert_eq!(options[1].token_usage, 23);
        assert!(!options
            .iter()
            .any(|option| option.key == "unassigned" || option.key == "ambiguous"));
    }

    #[test]
    fn trend_uses_channel_keys_for_current_and_previous_periods() {
        let conn = mem();
        add_daily(&conn, "2026-10-17", "codex", "", "openai", "gpt-5", 5);
        add_daily(
            &conn,
            "2026-10-17",
            "claude",
            "",
            "azure-openai",
            "gpt-5",
            7,
        );
        add_daily(&conn, "2026-10-10", "codex", "", "openai", "gpt-5", 3);
        add_daily(
            &conn,
            "2026-10-10",
            "claude",
            "",
            "azure-openai",
            "gpt-5",
            1,
        );
        add_provider_alias(&conn, "a1", "openai", "OpenAI");
        add_provider_alias(&conn, "a2", "azure-openai", "OpenAI");

        let query_range = range(UsageRangePreset::Last7d);
        let trend = trend_with_conn(
            &conn,
            &query_range,
            &TrendSelection::BillingProfile {
                selected_id: "provider:OpenAI".to_string(),
            },
            now_ms(),
        )
        .unwrap();
        assert!(trend.available);
        assert_eq!(trend.hero, 32);
        let compare = trend.compare.expect("上一周期存在同渠道数据");
        assert_eq!(compare.prev_metrics.request_total, 20);
        assert_eq!(
            compare.prev_buckets.output, 4,
            "上期必须复用同一 resolver 与同一渠道键"
        );

        let empty = trend_with_conn(
            &conn,
            &query_range,
            &TrendSelection::BillingProfile {
                selected_id: "provider:NotSeen".to_string(),
            },
            now_ms(),
        )
        .unwrap();
        assert!(
            empty.available,
            "选项来自当前范围；没有匹配行时返回空趋势而不是不可用"
        );
        assert_eq!(empty.hero, 0);
        assert!(empty.compare.is_none());
    }

    // ── 桶级归并双实现等价性钉子 ──────────────────────────────────────────────

    fn assigned_part(profile: &str, rules: &[&str]) -> Attribution {
        Attribution {
            billing_profile_id: Some(profile.to_string()),
            attribution_kind: AttributionKind::UserAssigned,
            rule_ids: rules.iter().map(|id| (*id).to_string()).collect(),
            reason: None,
        }
    }

    fn ambiguous_part(reason: AttributionReason, rules: &[&str]) -> Attribution {
        Attribution {
            billing_profile_id: None,
            attribution_kind: AttributionKind::Ambiguous,
            rule_ids: rules.iter().map(|id| (*id).to_string()).collect(),
            reason: Some(reason),
        }
    }

    fn unknown_part(reason: AttributionReason) -> Attribution {
        Attribution {
            billing_profile_id: None,
            attribution_kind: AttributionKind::Unknown,
            rule_ids: Vec::new(),
            reason: Some(reason),
        }
    }

    /// 表驱动等价性：`identity::merge_bucket`（段级解析路径）与
    /// `fold_bucket_attributions`（preview 行归并路径）必须对行为字段
    /// （kind / reason / billingProfileId）给出完全一致的结果。
    /// 已知且仅存的可见差异是信息性字段 ruleIds（assigned+boundary 混合时
    /// fold 保留 assigned 段 ruleIds），因此不在此断言。
    #[test]
    fn bucket_merge_implementations_agree_on_behavior_fields() {
        use crate::services::usage::v2::identity::merge_bucket;

        struct Case {
            name: &'static str,
            parts: Vec<Attribution>,
        }

        let cases = vec![
            Case {
                name: "空桶",
                parts: vec![],
            },
            Case {
                name: "全 assigned 同渠道",
                parts: vec![
                    assigned_part("a", &["r1"]),
                    assigned_part("a", &["r1", "r2"]),
                ],
            },
            Case {
                name: "全 assigned 异渠道",
                parts: vec![assigned_part("a", &["r1"]), assigned_part("b", &["r2"])],
            },
            Case {
                name: "含 ambiguous(mixed)",
                parts: vec![
                    ambiguous_part(AttributionReason::Mixed, &["r1"]),
                    assigned_part("a", &["r2"]),
                ],
            },
            Case {
                name: "含 ambiguous(conflict)",
                parts: vec![
                    ambiguous_part(AttributionReason::Conflict, &["r1"]),
                    assigned_part("a", &["r1"]),
                ],
            },
            Case {
                name: "assigned+unknown 混合",
                parts: vec![
                    assigned_part("a", &["r1"]),
                    unknown_part(AttributionReason::Unassigned),
                ],
            },
            Case {
                name: "全 unassigned",
                parts: vec![
                    unknown_part(AttributionReason::Unassigned),
                    unknown_part(AttributionReason::Unassigned),
                ],
            },
            Case {
                name: "assigned+boundary 混合",
                parts: vec![
                    assigned_part("a", &["r1"]),
                    unknown_part(AttributionReason::BucketBoundary),
                ],
            },
            Case {
                name: "全 boundary",
                parts: vec![unknown_part(AttributionReason::BucketBoundary)],
            },
        ];

        for case in cases {
            let merged = merge_bucket(case.parts.clone());
            let folded = fold_bucket_attributions(case.parts.clone());
            assert_eq!(
                merged.attribution_kind, folded.attribution_kind,
                "用例「{}」kind 不一致：merge={:?} fold={:?}",
                case.name, merged, folded
            );
            assert_eq!(
                merged.reason, folded.reason,
                "用例「{}」reason 不一致：merge={:?} fold={:?}",
                case.name, merged, folded
            );
            assert_eq!(
                merged.billing_profile_id, folded.billing_profile_id,
                "用例「{}」billingProfileId 不一致：merge={:?} fold={:?}",
                case.name, merged, folded
            );
        }
    }
}
// ==== usage 自省锁：排除区结束，勿删勿复制 ====
