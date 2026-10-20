//! 模型级等价费用查询与 CSV（Task 10 / Task 5）。
//!
//! 只读账本 + 当前配置 + 当前价格快照；不扫描、不联网、不写库。
//! * 费用行按**显示渠道 + 原始模型**拆分（不同渠道同模型分行、同渠道跨工具合并）；
//! * 定价顺序：手动覆盖 > 用户 `PricingRule` > 归一化自动匹配（含安全前缀回退）；
//! * 行键为长度前缀 tuple，明确渠道 / 未知上下文的模型不会被错误合并；
//! * 没有 total/小计字段——金额只在每行内表达。

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use rusqlite::Connection;

use crate::database::connection::DB;
use crate::services::usage::model::{SourceKind, UsageBuckets, UsageSourceId};
use crate::services::usage::query::LedgerRow;
use crate::services::usage::range::{granularity_of, resolve_range, RangeGranularity};
use crate::services::usage::v2::billing;
use crate::services::usage::v2::channel::{
    self, join_key, resolve_channel, ChannelResolver, ResolvedChannel,
};
use crate::services::usage::v2::cost::{
    basis_trusted, cny_string, estimate_components, safe_components, safe_components_output_only,
    usd_string, Estimate,
};
use crate::services::usage::v2::currency::{
    currency_view, merge_override_views, resolve_fx_with_conn, CurrencyRates, FxRate, NativePrices,
};
use crate::services::usage::v2::model::{
    AttributionKind, BillingConfig, ChannelKind, CostComponent, CostComponents, CostRow,
    CostStatus, IdentityKey, PriceCurrency, PricingBasis, PricingReference, RuleMatch, UsageCosts,
    UsageRange,
};
use crate::services::usage::v2::model_config::RowConfigIndex;
use crate::services::usage::v2::pricing::{
    self, match_model_scoped, normalize_model_id, EffectiveOverride, MatchScope, NormalizedQuote,
    PriceCatalog,
};
use crate::services::usage::v2::query::{attribute_rows, AttributedRow};
use crate::services::usage::UsageError;

const CSV_HEADER: &str = "billing_profile_id,billing_profile_name,source_ids,observed_provider,model_id,attribution_kind,token_usage,estimated_usd,status,priced_tokens,unpriced_tokens,coverage_rate,pricing_provider,pricing_model,pricing_snapshot_id,pricing_basis,price_updated_at,channel_key,channel_kind,display_provider,normalized_model_id,pricing_release_date,price_override";

/// 费用查询取数来源（2026-10-20 计划 §3.4）。
///
/// * `Detail`：7d/30d/自定义窗口内 usage_daily 的每个有数天都被 usage_detail 覆盖
///   ⇒ 按**小时明细按日聚合**取数（dsh 全量计价，四桶可信）；
/// * `Daily`：任一天明细缺失 ⇒ **整段回退**日聚合（dsh output-only，不做逐日混合）。
///
/// 5h/当日（Hour 粒度）恒走逐行明细 ⇒ `Detail`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CostBucketProvenance {
    Detail,
    Daily,
}

/// 明细覆盖检测（计划 §3.4）：两个索引上的 `DISTINCT day_key` 范围查询，O(天数)。
/// usage_daily 有数的天集 ⊆ usage_detail 天集 ⇒ `Detail`；否则（含 usage_detail 全空）
/// `Daily`。空 daily 天集 ⊆ 任意 ⇒ `Detail`（明细是权威数据，直接读明细）。
fn detail_provenance(
    conn: &Connection,
    first_day: &str,
    last_day: &str,
) -> Result<CostBucketProvenance, UsageError> {
    let distinct_days = |sql: &str| -> Result<Vec<String>, UsageError> {
        let mut stmt = conn
            .prepare(sql)
            .map_err(|e| internal(format!("cost query: coverage probe failed: {e}")))?;
        let rows = stmt
            .query_map([first_day, last_day], |row| row.get::<_, String>(0))
            .map_err(|e| internal(format!("cost query: coverage probe failed: {e}")))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| internal(format!("cost query: coverage probe failed: {e}")))
    };
    let daily_days =
        distinct_days("SELECT DISTINCT day_key FROM usage_daily WHERE day_key BETWEEN ?1 AND ?2")?;
    if daily_days.is_empty() {
        return Ok(CostBucketProvenance::Detail);
    }
    let detail_days =
        distinct_days("SELECT DISTINCT day_key FROM usage_detail WHERE day_key BETWEEN ?1 AND ?2")?;
    let covered = daily_days.iter().all(|day| detail_days.contains(day));
    Ok(if covered {
        CostBucketProvenance::Detail
    } else {
        CostBucketProvenance::Daily
    })
}

/// 明细按日聚合取数（计划 §3.4 的 Detail 路径）：`GROUP BY day_key, source_id, provider, model`
/// 服务端聚合后映射为 `LedgerRow`（费用不消费 project/user_est ⇒ 置空是安全降维；
/// `provider_kind` 取组内最小值——仅诊断语义，不参与费用可信判定）。
fn load_detail_daily_rows(
    conn: &Connection,
    first_day: &str,
    last_day: &str,
) -> Result<Vec<LedgerRow>, UsageError> {
    let mut stmt = conn
        .prepare(
            "SELECT day_key, source_id, provider, MIN(provider_kind), model,
                    SUM(input_fresh), SUM(cache_read), SUM(cache_write), SUM(output),
                    SUM(request_total), SUM(requests)
             FROM usage_detail
             WHERE day_key BETWEEN ?1 AND ?2
             GROUP BY day_key, source_id, provider, model",
        )
        .map_err(|e| internal(format!("cost query: detail aggregation failed: {e}")))?;
    let rows = stmt
        .query_map([first_day, last_day], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, i64>(9)?,
                row.get::<_, i64>(10)?,
            ))
        })
        .map_err(|e| internal(format!("cost query: detail aggregation failed: {e}")))?;
    let mut out = Vec::new();
    for row in rows {
        let (
            day_key,
            source_id,
            provider,
            provider_kind,
            model,
            input_fresh,
            cache_read,
            cache_write,
            output,
            request_total,
            requests,
        ) = row.map_err(|e| internal(format!("cost query: detail row read failed: {e}")))?;
        out.push(LedgerRow {
            bucket_key: day_key,
            source_id,
            // 费用行不消费会话/项目/用户估算维度；置空是安全降维（聚合键里没有它们）。
            session_id: String::new(),
            project_key: String::new(),
            project_label: String::new(),
            provider,
            provider_kind: match provider_kind.as_str() {
                "inferred" => SourceKind::Inferred,
                "unknown" => SourceKind::Unknown,
                _ => SourceKind::Measured,
            },
            model,
            buckets: UsageBuckets {
                input_fresh,
                cache_read,
                cache_write,
                output,
            },
            request_total,
            requests,
            user_est: None,
        });
    }
    Ok(out)
}

/// 费用查询取数（§3.4）：返回归因行 + 本次取数的 provenance。
/// 本函数是费用域专用；billing preview 走 billing.rs 的直连归因路径
/// （`attribute_rows` + `load_ledger_rows_with`），不经本函数、也不经 `snapshot_with_conn`。
pub(crate) fn cost_snapshot_with_conn(
    conn: &Connection,
    range: &UsageRange,
    now_ms: i64,
) -> Result<(Vec<AttributedRow>, CostBucketProvenance), UsageError> {
    if !crate::services::usage::settings::load_from_conn(conn).enabled {
        return Ok((Vec::new(), CostBucketProvenance::Daily));
    }
    let resolved = resolve_range(range, now_ms)?;
    let config = crate::services::usage::v2::billing::load_with_conn(conn)?;
    let provenance = match resolved.cur.granularity {
        // 5h/当日：现路径逐行明细 ⇒ Detail。
        RangeGranularity::Hour => CostBucketProvenance::Detail,
        RangeGranularity::Day => detail_provenance(
            conn,
            resolved
                .cur
                .keys
                .first()
                .map(String::as_str)
                .unwrap_or_default(),
            resolved
                .cur
                .keys
                .last()
                .map(String::as_str)
                .unwrap_or_default(),
        )?,
    };
    let rows = match provenance {
        CostBucketProvenance::Detail => {
            if resolved.cur.granularity == RangeGranularity::Day {
                load_detail_daily_rows(
                    conn,
                    resolved
                        .cur
                        .keys
                        .first()
                        .map(String::as_str)
                        .unwrap_or_default(),
                    resolved
                        .cur
                        .keys
                        .last()
                        .map(String::as_str)
                        .unwrap_or_default(),
                )?
            } else {
                crate::services::usage::query::load_ledger_rows_with(conn, &resolved)
            }
        }
        CostBucketProvenance::Daily => {
            crate::services::usage::query::load_ledger_rows_with(conn, &resolved)
        }
    };
    let attributed = attribute_rows(rows, resolved.cur.granularity, &config)?;
    Ok((attributed, provenance))
}

fn internal(detail: impl Into<String>) -> UsageError {
    UsageError::internal(detail.into())
}

fn kind_str(kind: AttributionKind) -> &'static str {
    match kind {
        AttributionKind::Observed => "observed",
        AttributionKind::UserAssigned => "userAssigned",
        AttributionKind::Unknown => "unknown",
        AttributionKind::Ambiguous => "ambiguous",
    }
}

fn channel_kind_str(kind: ChannelKind) -> &'static str {
    match kind {
        ChannelKind::BillingProfile => "billingProfile",
        ChannelKind::Alias => "alias",
        ChannelKind::Provider => "provider",
        ChannelKind::Unassigned => "unassigned",
        ChannelKind::Ambiguous => "ambiguous",
    }
}

fn status_str(status: CostStatus) -> &'static str {
    match status {
        CostStatus::Priced => "priced",
        CostStatus::Partial => "partial",
        CostStatus::Unpriced => "unpriced",
    }
}

fn basis_str(basis: PricingBasis) -> &'static str {
    match basis {
        PricingBasis::BaseCurrent => "baseCurrent",
        PricingBasis::Insufficient => "insufficient",
        PricingBasis::UserOverride => "userOverride",
    }
}

fn pricing_rule_matches(
    identity: &IdentityKey,
    rule: &crate::services::usage::v2::model::PricingRule,
) -> bool {
    if identity.source_id != rule.identity.source_id
        || identity.observed_provider != rule.identity.observed_provider
    {
        return false;
    }
    match rule.match_kind {
        RuleMatch::Exact => identity.model_id == rule.identity.model_id,
        RuleMatch::Prefix => {
            !rule.identity.model_id.is_empty()
                && identity.model_id.starts_with(&rule.identity.model_id)
        }
    }
}

/// 用户 `PricingRule` 的选定结果；`Conflict` = 同级参考价冲突。
enum UserSelection {
    Reference(PricingReference),
    Conflict,
    None,
}

/// 精确 > 最长前缀；同级不同 reference ⇒ `Conflict`（不得猜价）。
fn select_user_reference(identity: &IdentityKey, config: &BillingConfig) -> UserSelection {
    let matching: Vec<&crate::services::usage::v2::model::PricingRule> = config
        .pricing_rules
        .iter()
        .filter(|rule| pricing_rule_matches(identity, rule))
        .collect();
    let exact: Vec<&crate::services::usage::v2::model::PricingRule> = matching
        .iter()
        .copied()
        .filter(|rule| rule.match_kind == RuleMatch::Exact)
        .collect();
    let chosen: Vec<&crate::services::usage::v2::model::PricingRule> = if !exact.is_empty() {
        exact
    } else {
        let longest = matching
            .iter()
            .filter(|rule| rule.match_kind == RuleMatch::Prefix)
            .map(|rule| rule.identity.model_id.chars().count())
            .max();
        match longest {
            Some(length) => matching
                .iter()
                .copied()
                .filter(|rule| {
                    rule.match_kind == RuleMatch::Prefix
                        && rule.identity.model_id.chars().count() == length
                })
                .collect(),
            None => Vec::new(),
        }
    };

    let references: BTreeSet<(String, String)> = chosen
        .iter()
        .map(|rule| {
            (
                rule.reference.provider_id.clone(),
                rule.reference.model_id.clone(),
            )
        })
        .collect();
    if references.len() > 1 {
        return UserSelection::Conflict;
    }
    match references.into_iter().next() {
        Some((provider_id, model_id)) => UserSelection::Reference(PricingReference {
            provider_id,
            model_id,
        }),
        None => UserSelection::None,
    }
}

/// 自动匹配结果（按原始模型缓存，避免逐行扫描归一化目录）。
#[derive(Clone)]
struct AutoMatch {
    quote: Option<NormalizedQuote>,
    prefix_ambiguous: bool,
}

fn cached_auto_match(
    identity: &IdentityKey,
    catalog: Option<&(String, i64, Arc<PriceCatalog>)>,
    cache: &mut HashMap<String, AutoMatch>,
) -> AutoMatch {
    let Some((_, _, catalog)) = catalog else {
        return AutoMatch {
            quote: None,
            prefix_ambiguous: false,
        };
    };
    if let Some(cached) = cache.get(&identity.model_id) {
        return cached.clone();
    }
    // 自动匹配**恒为精选作用域**（直营官方 > Ollama Cloud > OpenRouter；2026-10-20 计划 §3.2）。
    let matched = match_model_scoped(catalog, &identity.model_id, MatchScope::Curated);
    let resolved = AutoMatch {
        quote: matched.quote.cloned(),
        prefix_ambiguous: matched.prefix_ambiguous,
    };
    cache.insert(identity.model_id.clone(), resolved.clone());
    resolved
}

/// 目录报价的查询期视图（用户规则走精确键，自动匹配走归一化索引）。
#[derive(Clone)]
struct QuoteView {
    reference: PricingReference,
    rates: [Option<i128>; 4],
    conditional: bool,
    release_date: Option<String>,
    normalized_model_id: Option<String>,
}

impl QuoteView {
    fn from_normalized(quote: &NormalizedQuote) -> Self {
        Self {
            reference: quote.reference.clone(),
            rates: quote.rates,
            conditional: quote.conditional,
            release_date: quote.release_date.clone(),
            normalized_model_id: non_empty(&quote.normalized_model_id),
        }
    }
}

fn non_empty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_string())
}

struct RowPricing {
    reference: Option<PricingReference>,
    usd: CurrencyRates,
    cny: CurrencyRates,
    basis: PricingBasis,
    snapshot_id: Option<String>,
    fetched_at: Option<i64>,
    normalized_model_id: Option<String>,
    release_date: Option<String>,
    price_override: bool,
    reasons: Vec<String>,
}

fn insufficient_pricing(reference: Option<PricingReference>) -> RowPricing {
    RowPricing {
        reference,
        usd: CurrencyRates::default(),
        cny: CurrencyRates::default(),
        basis: PricingBasis::Insufficient,
        snapshot_id: None,
        fetched_at: None,
        normalized_model_id: None,
        release_date: None,
        price_override: false,
        reasons: Vec::new(),
    }
}

/// 目录报价的双币种视图：USD 用目录原生；CNY 优先用 `usage_pricing_native`，
/// 没有明确人民币价时按当日汇率换算并逐分量标记 `converted`。
fn catalog_currency_views(
    quote: &QuoteView,
    native: &NativePrices,
    fx: &FxRate,
) -> Result<(CurrencyRates, CurrencyRates), UsageError> {
    let usd = CurrencyRates {
        rates: quote.rates,
        converted: [false; 4],
    };
    let cny = match quote
        .normalized_model_id
        .as_deref()
        .and_then(|normalized| native.rates_for(normalized, &quote.reference.provider_id))
    {
        Some(rates) => CurrencyRates {
            rates,
            converted: [false; 4],
        },
        None => currency_view(quote.rates, PriceCurrency::Usd, PriceCurrency::Cny, fx)?,
    };
    Ok((usd, cny))
}

/// 解析一行的参考价与双币种费率（计划 §3.5 / §4.3 / §4.4；2026-10-20 币种扩展）。
#[allow(clippy::too_many_arguments)] // 四级定价链 + 原生人民币/汇率注入；拆结构体会掩盖调用面
fn resolve_row_pricing(
    identity: &IdentityKey,
    config: &BillingConfig,
    catalog: Option<&(String, i64, Arc<PriceCatalog>)>,
    overrides: &BTreeMap<String, EffectiveOverride>,
    native: &NativePrices,
    fx: &FxRate,
    model_config: &RowConfigIndex,
    auto_cache: &mut HashMap<String, AutoMatch>,
) -> Result<RowPricing, UsageError> {
    let mut reasons: Vec<String> = Vec::new();
    let mut reference: Option<PricingReference> = None;
    let mut quote: Option<QuoteView> = None;
    let mut auto_normalized: Option<String> = None;
    let mut user_conflict = false;

    match select_user_reference(identity, config) {
        UserSelection::Reference(selected) => reference = Some(selected),
        UserSelection::Conflict => user_conflict = true,
        UserSelection::None => {
            // 第 3 级：计价身份覆盖（usage_model_config.pricing_*，2026-10-20 计划 §3.3）——
            // 目录有该精确键 ⇒ 直接取价（**不再做**自动匹配与前缀回退）；
            // 目录无该条目 ⇒ noPrice 但保留候选 reference（不静默回退自动匹配）。
            if let Some((pricing_provider, pricing_model)) = model_config.pricing_identity(identity)
            {
                let selected = PricingReference {
                    provider_id: pricing_provider,
                    model_id: pricing_model,
                };
                let found = catalog.and_then(|(_, _, catalog)| {
                    catalog
                        .quotes
                        .get(&(selected.provider_id.clone(), selected.model_id.clone()))
                });
                match found {
                    Some(found) => {
                        quote = Some(QuoteView {
                            reference: selected.clone(),
                            rates: found.rates,
                            conditional: found.conditional,
                            release_date: found.release_date.clone(),
                            normalized_model_id: non_empty(&found.normalized_model_id),
                        });
                        reference = Some(selected);
                    }
                    None => {
                        reference = Some(selected);
                        reasons.push("noPrice".to_string());
                    }
                }
            } else {
                // 第 4 级：自动匹配（**恒为精选作用域**，见 `cached_auto_match`）。
                let matched = cached_auto_match(identity, catalog, auto_cache);
                if matched.prefix_ambiguous {
                    reasons.push("pricingPrefixAmbiguous".to_string());
                } else if let Some(matched_quote) = matched.quote.as_ref() {
                    auto_normalized = non_empty(&matched_quote.normalized_model_id);
                    reference = Some(matched_quote.reference.clone());
                    quote = Some(QuoteView::from_normalized(matched_quote));
                } else {
                    reasons.push("noPrice".to_string());
                }
            }
        }
    }

    // 用户规则走精确目录键；找不到目录条目时保留候选 reference（noPrice）。
    if quote.is_none() {
        if let (Some(selected), Some((_, _, catalog))) = (reference.as_ref(), catalog) {
            if let Some(found) = catalog
                .quotes
                .get(&(selected.provider_id.clone(), selected.model_id.clone()))
            {
                quote = Some(QuoteView {
                    reference: selected.clone(),
                    rates: found.rates,
                    conditional: found.conditional,
                    release_date: found.release_date.clone(),
                    normalized_model_id: non_empty(&found.normalized_model_id),
                });
            }
        }
    }

    // 手动覆盖：先按原始模型的归一化 ID，再按自动匹配选中的目录归一化 ID。
    let normalized_raw = normalize_model_id(&identity.model_id);
    let effective_override = non_empty(&normalized_raw)
        .and_then(|key| overrides.get(&key))
        .or_else(|| {
            auto_normalized
                .as_deref()
                .and_then(|key| overrides.get(key))
        })
        .filter(|override_rates| override_rates.rates.iter().any(Option::is_some));

    if let Some(effective_override) = effective_override {
        // 覆盖价是最优先来源：它能把规则冲突/前缀歧义下无法确定的行按覆盖价计价。
        reasons.retain(|reason| reason != "noPrice");
        let (catalog_usd, catalog_cny) = match quote.as_ref() {
            Some(quote) => catalog_currency_views(quote, native, fx)?,
            None => (CurrencyRates::default(), CurrencyRates::default()),
        };
        let uses_catalog = (0..4).any(|index| {
            effective_override.rates[index].is_none()
                && (catalog_usd.rates[index].is_some() || catalog_cny.rates[index].is_some())
        });
        let (usd, cny) = merge_override_views(
            effective_override.rates,
            effective_override.currency,
            catalog_usd,
            catalog_cny,
            fx,
        )?;
        let basis = if !usd.has_rates() && !cny.has_rates() {
            PricingBasis::Insufficient
        } else {
            PricingBasis::UserOverride
        };
        if uses_catalog && quote.as_ref().is_some_and(|quote| quote.conditional) {
            reasons.push("conditionalBaseRate".to_string());
        }
        return Ok(RowPricing {
            // 纯覆盖价（没有任何分量采用目录参考）不得冒充目录来源。
            reference: if uses_catalog {
                quote
                    .as_ref()
                    .map(|quote| quote.reference.clone())
                    .or_else(|| reference.clone())
            } else {
                None
            },
            usd,
            cny,
            basis,
            snapshot_id: if uses_catalog {
                catalog.map(|(snapshot_id, _, _)| snapshot_id.clone())
            } else {
                None
            },
            fetched_at: if uses_catalog {
                catalog.map(|(_, fetched_at, _)| *fetched_at)
            } else {
                None
            },
            normalized_model_id: if uses_catalog {
                quote
                    .as_ref()
                    .and_then(|quote| quote.normalized_model_id.clone())
                    .or_else(|| auto_normalized.clone())
            } else {
                // 纯覆盖价没有目录来源，但覆盖键本身来自原始模型的归一化 ID；
                // 保留它供展示/核对，不冒充目录 reference。
                auto_normalized
                    .clone()
                    .or_else(|| non_empty(&normalized_raw))
            },
            release_date: uses_catalog
                .then(|| quote.as_ref().and_then(|quote| quote.release_date.clone()))
                .flatten(),
            price_override: true,
            reasons,
        });
    }

    if user_conflict {
        reasons.push("pricingReferenceAmbiguous".to_string());
        let mut out = insufficient_pricing(None);
        out.reasons = reasons;
        return Ok(out);
    }

    let Some(quote) = quote else {
        if reference.is_some() {
            reasons.push("noPrice".to_string());
        }
        let mut out = insufficient_pricing(reference);
        out.reasons = reasons;
        return Ok(out);
    };

    let (snapshot_id, fetched_at) = catalog
        .map(|(snapshot_id, fetched_at, _)| (Some(snapshot_id.clone()), Some(*fetched_at)))
        .unwrap_or((None, None));
    let (usd, cny) = catalog_currency_views(&quote, native, fx)?;
    let mut out = RowPricing {
        reference: Some(quote.reference.clone()),
        usd,
        cny,
        basis: PricingBasis::Insufficient,
        snapshot_id: None,
        fetched_at: None,
        normalized_model_id: quote.normalized_model_id.clone(),
        release_date: quote.release_date.clone(),
        price_override: false,
        reasons,
    };
    if out.usd.has_rates() || out.cny.has_rates() {
        // 只有真正拿到至少一项费率才宣称 baseCurrent。条件价（多模态/阶梯）也按
        // 基础文本价估算，但显式标注（2026-10-20 用户裁决）。
        out.basis = PricingBasis::BaseCurrent;
        out.snapshot_id = snapshot_id;
        out.fetched_at = fetched_at;
        if quote.conditional {
            out.reasons.push("conditionalBaseRate".to_string());
        }
    } else {
        // quote 存在但全缺价：不是“使用了基准价”，只能 Insufficient。
        out.reasons.push("noPrice".to_string());
    }
    Ok(out)
}

fn row_key(
    channel: &ResolvedChannel,
    identity: &IdentityKey,
    normalized_model_id: Option<&str>,
    reference: Option<&PricingReference>,
    basis: PricingBasis,
    snapshot_id: Option<&str>,
    price_override: bool,
) -> String {
    // 未指定渠道/可能混合必须附加工具上下文；其他渠道跨工具合并。
    let (context_source, context_provider) = match channel.kind {
        ChannelKind::Unassigned | ChannelKind::Ambiguous => (
            identity.source_id.db_id(),
            identity.observed_provider.as_deref().unwrap_or(""),
        ),
        ChannelKind::BillingProfile | ChannelKind::Alias | ChannelKind::Provider => ("", ""),
    };
    let (provider, model) = reference
        .map(|reference| (reference.provider_id.as_str(), reference.model_id.as_str()))
        .unwrap_or(("", ""));
    join_key(&[
        channel.key.as_str(),
        context_source,
        context_provider,
        identity.model_id.as_str(),
        normalized_model_id.unwrap_or(""),
        provider,
        model,
        basis_str(basis),
        snapshot_id.unwrap_or(""),
        if price_override { "1" } else { "0" },
        "USD",
    ])
}

struct RowCalc {
    key: String,
    attributed: AttributedRow,
    channel: ResolvedChannel,
    token_usage: i64,
    request_total: i64,
    cache_read: i64,
    pricing: RowPricing,
    estimate_usd: Estimate,
    estimate_cny: Estimate,
    reasons: Vec<String>,
}

fn degraded_estimate(token_usage: i64) -> Estimate {
    let token_usage = token_usage.max(0);
    Estimate {
        components: [None; 4],
        amount_scaled: None,
        priced_tokens: 0,
        unpriced_tokens: token_usage,
        coverage_rate: (token_usage > 0).then_some(0.0),
        status: CostStatus::Unpriced,
    }
}

#[allow(clippy::too_many_arguments)] // 四级定价链的显式依赖注入；拆结构体会掩盖调用面
fn calculate_row(
    attributed: AttributedRow,
    granularity: crate::services::usage::range::RangeGranularity,
    provenance: CostBucketProvenance,
    config: &BillingConfig,
    resolver: &ChannelResolver<'_>,
    catalog: Option<&(String, i64, Arc<PriceCatalog>)>,
    overrides: &BTreeMap<String, EffectiveOverride>,
    native: &NativePrices,
    fx: &FxRate,
    model_config: &RowConfigIndex,
    auto_cache: &mut HashMap<String, AutoMatch>,
) -> Result<RowCalc, UsageError> {
    let identity = &attributed.identity;
    let row = &attributed.row;
    // 可信判据（§3.4）：明细四桶是归一化后的互斥规范桶（落库前已消化语义差异，
    // 与 Hour 档同源同口径）⇒ Detail 下全部源（含 dsh）trusted；
    // Daily 回退维持 basis_trusted（dsh 日聚合 output-only）。
    let trusted = provenance == CostBucketProvenance::Detail
        || basis_trusted(identity.source_id, granularity);
    // dsh 日档：日聚合无法区分逐请求缓存语义 ⇒ 输入/缓存桶不可信；
    // output 与缓存语义无关，仍单独计价（其余 untrusted 形态整行未估价）。
    let dsh_day_output_only = !trusted
        && identity.source_id == UsageSourceId::Dsh
        && granularity == RangeGranularity::Day;
    let safe = if trusted {
        safe_components(&row.buckets, row.request_total, true)
    } else if dsh_day_output_only {
        safe_components_output_only(&row.buckets)
    } else {
        [None; 4]
    };
    let pricing = resolve_row_pricing(
        identity,
        config,
        catalog,
        overrides,
        native,
        fx,
        model_config,
        auto_cache,
    )?;
    let channel = resolve_channel(identity, &attributed.attribution, resolver);

    let checked_usage = row.request_total.checked_add(row.buckets.output);
    let token_usage =
        checked_usage.unwrap_or_else(|| row.request_total.saturating_add(row.buckets.output));
    let mut estimate_overflow = checked_usage.is_none();
    let mut estimate_for = |rates: [Option<i128>; 4]| -> Estimate {
        match estimate_components(&row.buckets, row.request_total, safe, rates) {
            Ok(estimate) => estimate,
            Err(_) => {
                estimate_overflow = true;
                degraded_estimate(token_usage)
            }
        }
    };
    let estimate_usd = estimate_for(pricing.usd.rates);
    let estimate_cny = estimate_for(pricing.cny.rates);

    let mut reasons = pricing.reasons.clone();
    if estimate_overflow {
        reasons.push("estimateOverflow".to_string());
    }
    if !trusted {
        reasons.push("untrustedSource".to_string());
        if dsh_day_output_only {
            // 输入/缓存桶仍无可靠性证据；output 已按与缓存语义无关的部分单独计价。
            reasons.push("untrustedInputBuckets".to_string());
        }
    } else if safe.iter().all(Option::is_none) {
        reasons.push("overlapOrUnrecoverable".to_string());
    } else if row.buckets.cache_write > 0 && safe[0].is_none() && safe[2].is_none() {
        reasons.push("cacheWriteOverlap".to_string());
    }
    if estimate_usd.unpriced_tokens > 0 || estimate_cny.unpriced_tokens > 0 {
        reasons.push("unpricedComponent".to_string());
    }
    reasons.sort_unstable();
    reasons.dedup();

    let key = row_key(
        &channel,
        identity,
        pricing.normalized_model_id.as_deref(),
        pricing.reference.as_ref(),
        pricing.basis,
        pricing.snapshot_id.as_deref(),
        pricing.price_override,
    );
    Ok(RowCalc {
        key,
        token_usage,
        request_total: row.request_total,
        cache_read: row.buckets.cache_read,
        pricing,
        estimate_usd,
        estimate_cny,
        reasons,
        attributed,
        channel,
    })
}

#[derive(Default)]
struct ComponentAcc {
    tokens: i64,
    amount_scaled: i128,
    rate_scaled: i128,
    converted: bool,
}

#[derive(Default)]
struct CurrencyAcc {
    amount_scaled: i128,
    priced_tokens: i64,
    unpriced_tokens: i64,
    components: [Option<ComponentAcc>; 4],
    any_converted: bool,
}

struct GroupAcc {
    key: String,
    identity: IdentityKey,
    source_ids: BTreeSet<String>,
    billing_profile_id: Option<String>,
    attribution_kind: AttributionKind,
    channel_key: String,
    channel_kind: ChannelKind,
    display_provider: Option<String>,
    observed_providers: BTreeSet<String>,
    has_empty_provider: bool,
    normalized_model_id: Option<String>,
    release_date: Option<String>,
    price_override: bool,
    /// 成员身份在 `usage_model_config` 里配置的自定义模型名（非空才收）；
    /// 输出时恰有一个不同值 ⇒ 生效，无人配置或分歧 ⇒ null（§3.5 诚实降级）。
    display_model_names: BTreeSet<String>,
    token_usage: i64,
    request_total: i64,
    cache_read: i64,
    usd: CurrencyAcc,
    cny: CurrencyAcc,
    reasons: BTreeSet<String>,
    reference: Option<PricingReference>,
    snapshot_id: Option<String>,
    basis: PricingBasis,
    price_updated_at: Option<i64>,
}

fn add_signed(value: i64, delta: i64, what: &str) -> Result<i64, UsageError> {
    value
        .checked_add(delta)
        .ok_or_else(|| internal(format!("cost query: {what} overflow")))
}

fn accumulate_currency(
    acc: &mut CurrencyAcc,
    estimate: &Estimate,
    converted: [bool; 4],
) -> Result<(), UsageError> {
    acc.priced_tokens = add_signed(acc.priced_tokens, estimate.priced_tokens, "priced tokens")?;
    acc.unpriced_tokens = add_signed(
        acc.unpriced_tokens,
        estimate.unpriced_tokens,
        "unpriced tokens",
    )?;
    if let Some(amount) = estimate.amount_scaled {
        acc.amount_scaled = acc
            .amount_scaled
            .checked_add(amount)
            .ok_or_else(|| internal("cost query: amount overflow"))?;
    }
    for (index, component) in estimate.components.iter().enumerate() {
        let Some(component) = component else {
            continue;
        };
        let entry = acc.components[index].get_or_insert_with(ComponentAcc::default);
        entry.tokens = add_signed(entry.tokens, component.tokens, "component tokens")?;
        entry.amount_scaled = entry
            .amount_scaled
            .checked_add(component.amount_scaled)
            .ok_or_else(|| internal("cost query: component amount overflow"))?;
        entry.rate_scaled = component.rate_scaled;
        entry.converted |= converted[index];
        acc.any_converted |= converted[index];
    }
    Ok(())
}

pub fn costs_with_conn(
    conn: &Connection,
    range: &UsageRange,
    now_ms: i64,
    fallback_rate: Option<&str>,
) -> Result<UsageCosts, UsageError> {
    let pricing_status = pricing::status_with_conn(conn, now_ms)?;
    let config = billing::load_with_conn(conn)?;
    let aliases = channel::load_with_conn(conn)?;
    let resolver = ChannelResolver::new(&config, &aliases);
    let catalog = pricing::load_active_catalog_with_conn(conn)?;
    let snapshot_id = catalog.as_ref().map(|(id, _, _)| id.as_str());
    let native = NativePrices::load_with_conn(conn, snapshot_id)?;
    let fx = resolve_fx_with_conn(conn, snapshot_id, fallback_rate)?;
    // 配置一次性加载，避免逐行查库；自动匹配按原始模型缓存。
    let overrides = pricing::load_effective_overrides_with_conn(conn)?;
    // 行级模型配置一次性加载（计价身份覆盖 + displayModelName 解析）。
    let model_config = RowConfigIndex::load_with_conn(conn)?;
    let mut auto_cache: HashMap<String, AutoMatch> = HashMap::new();
    let granularity = granularity_of(range.preset);
    // 费用域专用取数：Detail（明细按日聚合）/ Daily（日聚合回退）+ provenance。
    // billing preview 走 billing.rs 直连路径（`attribute_rows` + `load_ledger_rows_with`），不经本函数。
    let (attributed, provenance) = cost_snapshot_with_conn(conn, range, now_ms)?;

    let mut groups: BTreeMap<String, GroupAcc> = BTreeMap::new();
    for attributed_row in attributed {
        let calculated = calculate_row(
            attributed_row,
            granularity,
            provenance,
            &config,
            &resolver,
            catalog.as_ref(),
            &overrides,
            &native,
            &fx,
            &model_config,
            &mut auto_cache,
        )?;
        let source = calculated.attributed.identity.source_id.db_id().to_string();
        let attribution = &calculated.attributed.attribution;
        let group = groups
            .entry(calculated.key.clone())
            .or_insert_with(|| GroupAcc {
                key: calculated.key.clone(),
                identity: calculated.attributed.identity.clone(),
                source_ids: BTreeSet::new(),
                billing_profile_id: attribution.billing_profile_id.clone(),
                attribution_kind: attribution.attribution_kind,
                channel_key: calculated.channel.key.clone(),
                channel_kind: calculated.channel.kind,
                display_provider: calculated.channel.display_provider.clone(),
                observed_providers: BTreeSet::new(),
                has_empty_provider: false,
                normalized_model_id: calculated.pricing.normalized_model_id.clone(),
                release_date: calculated.pricing.release_date.clone(),
                price_override: calculated.pricing.price_override,
                display_model_names: BTreeSet::new(),
                token_usage: 0,
                request_total: 0,
                cache_read: 0,
                usd: CurrencyAcc::default(),
                cny: CurrencyAcc::default(),
                reasons: BTreeSet::new(),
                reference: calculated.pricing.reference.clone(),
                snapshot_id: calculated.pricing.snapshot_id.clone(),
                basis: calculated.pricing.basis,
                price_updated_at: calculated.pricing.fetched_at,
            });
        group.source_ids.insert(source);
        match calculated.attributed.identity.observed_provider.as_deref() {
            Some(observed) if !observed.is_empty() => {
                group.observed_providers.insert(observed.to_string());
            }
            _ => group.has_empty_provider = true,
        }
        if let Some(name) = model_config.display_name(&calculated.attributed.identity) {
            group.display_model_names.insert(name);
        }
        group.token_usage = add_signed(group.token_usage, calculated.token_usage, "token usage")?;
        group.request_total = add_signed(
            group.request_total,
            calculated.request_total,
            "request total",
        )?;
        group.cache_read = add_signed(group.cache_read, calculated.cache_read, "cache read")?;
        accumulate_currency(
            &mut group.usd,
            &calculated.estimate_usd,
            calculated.pricing.usd.converted,
        )?;
        accumulate_currency(
            &mut group.cny,
            &calculated.estimate_cny,
            calculated.pricing.cny.converted,
        )?;
        group.reasons.extend(calculated.reasons);
    }

    let mut rows: Vec<CostRow> = groups
        .into_values()
        .map(|group| {
            let CurrencyAcc {
                amount_scaled: usd_amount,
                priced_tokens: usd_priced,
                unpriced_tokens: usd_unpriced_acc,
                components: mut usd_components,
                any_converted: usd_converted,
            } = group.usd;
            let CurrencyAcc {
                amount_scaled: cny_amount,
                priced_tokens: cny_priced,
                unpriced_tokens: cny_unpriced_acc,
                components: mut cny_components,
                any_converted: cny_converted,
            } = group.cny;

            let usd_unpriced = group
                .token_usage
                .checked_sub(usd_priced)
                .ok_or_else(|| internal("cost query: unpriced token overflow"))?;
            let cny_unpriced = group
                .token_usage
                .checked_sub(cny_priced)
                .ok_or_else(|| internal("cost query: unpriced token overflow"))?;
            if usd_unpriced < 0 || cny_unpriced < 0 {
                return Err(internal("cost query: priced tokens exceed total"));
            }
            if usd_unpriced != usd_unpriced_acc || cny_unpriced != cny_unpriced_acc {
                return Err(internal(
                    "cost query: per-row and aggregate unpriced tokens diverged",
                ));
            }
            let coverage_rate = coverage_of(group.token_usage, usd_priced);
            let coverage_rate_cny = coverage_of(group.token_usage, cny_priced);
            let status = cost_status_of(group.token_usage, usd_priced);
            let status_cny = cost_status_of(group.token_usage, cny_priced);
            let estimated_usd = (usd_priced > 0).then(|| usd_string(usd_amount));
            let estimated_cny = (cny_priced > 0).then(|| cny_string(cny_amount));
            let mut components: [Option<CostComponent>; 4] = [None, None, None, None];
            for index in 0..4 {
                let usd_component = usd_components[index].take();
                let cny_component = cny_components[index].take();
                let tokens = match (&usd_component, &cny_component) {
                    (Some(usd), Some(cny)) if usd.tokens != cny.tokens => {
                        return Err(internal(
                            "cost query: USD/CNY component token counts diverged",
                        ));
                    }
                    (Some(usd), _) => usd.tokens,
                    (_, Some(cny)) => cny.tokens,
                    (None, None) => continue,
                };
                let unit = |component: &ComponentAcc| -> Result<String, UsageError> {
                    let scaled = component
                        .rate_scaled
                        .checked_mul(1_000_000)
                        .ok_or_else(|| internal("cost query: unit overflow"))?;
                    Ok(usd_string(scaled))
                };
                components[index] = Some(CostComponent {
                    tokens,
                    unit_usd_per_million: usd_component.as_ref().map(unit).transpose()?,
                    estimated_usd: usd_component
                        .as_ref()
                        .map(|component| usd_string(component.amount_scaled)),
                    unit_cny_per_million: cny_component.as_ref().map(unit).transpose()?,
                    estimated_cny: cny_component
                        .as_ref()
                        .map(|component| cny_string(component.amount_scaled)),
                    usd_converted: usd_component
                        .as_ref()
                        .is_some_and(|component| component.converted),
                    cny_converted: cny_component
                        .as_ref()
                        .is_some_and(|component| component.converted),
                });
            }
            let [input, cache_read, cache_write, output] = components;
            // displayModelName：恰有一个不同值 ⇒ 生效；无人配置或分歧 ⇒ null（§3.5）。
            let display_model_name = (group.display_model_names.len() == 1)
                .then(|| group.display_model_names.iter().next().cloned())
                .flatten();
            let observed_providers: Vec<String> = group.observed_providers.into_iter().collect();
            let has_empty_provider = group.has_empty_provider;
            // 与分布侧对齐：仅当恰好一个非空 provider 且无空 provider 行时保留单值证据。
            let mut identity = group.identity;
            identity.observed_provider = (!has_empty_provider && observed_providers.len() == 1)
                .then(|| observed_providers[0].clone());
            Ok(CostRow {
                key: group.key,
                identity,
                source_ids: group
                    .source_ids
                    .iter()
                    .filter_map(|source| {
                        UsageSourceId::ALL
                            .into_iter()
                            .find(|candidate| candidate.db_id() == source)
                    })
                    .collect(),
                billing_profile_id: group.billing_profile_id,
                attribution_kind: group.attribution_kind,
                token_usage: group.token_usage,
                cache_hit_rate: (group.request_total > 0)
                    .then(|| group.cache_read as f64 / group.request_total as f64),
                estimated_usd,
                estimated_cny,
                usd_converted,
                cny_converted,
                status,
                status_cny,
                priced_tokens: usd_priced,
                unpriced_tokens: usd_unpriced,
                coverage_rate,
                priced_tokens_cny: cny_priced,
                unpriced_tokens_cny: cny_unpriced,
                coverage_rate_cny,
                pricing_reference: group.reference,
                pricing_snapshot_id: group.snapshot_id,
                pricing_basis: group.basis,
                price_updated_at: group.price_updated_at,
                reasons: group.reasons.into_iter().collect(),
                components: CostComponents {
                    input,
                    cache_read,
                    cache_write,
                    output,
                },
                channel_key: group.channel_key,
                channel_kind: group.channel_kind,
                display_provider: group.display_provider,
                observed_providers,
                has_empty_provider,
                normalized_model_id: group.normalized_model_id,
                pricing_release_date: group.release_date,
                price_override: group.price_override,
                display_model_name,
            })
        })
        .collect::<Result<Vec<_>, UsageError>>()?;
    rows.sort_by_key(|row| (std::cmp::Reverse(row.token_usage), row.key.clone()));

    Ok(UsageCosts {
        range: range.clone(),
        rows,
        pricing: pricing_status,
        fx_rate: Some(fx.rate_text),
        fx_fetched_at: fx.fetched_at,
        fx_source: Some(fx.source.to_string()),
        fx_fallback: fx.fallback,
    })
}

fn coverage_of(token_usage: i64, priced_tokens: i64) -> Option<f64> {
    (token_usage > 0).then(|| priced_tokens as f64 / token_usage as f64)
}

fn cost_status_of(token_usage: i64, priced_tokens: i64) -> CostStatus {
    if token_usage > 0 && priced_tokens == token_usage {
        CostStatus::Priced
    } else if priced_tokens > 0 && priced_tokens < token_usage {
        CostStatus::Partial
    } else {
        CostStatus::Unpriced
    }
}

fn csv_cell(value: &str) -> String {
    let needs_neutralizing = matches!(
        value.chars().next(),
        Some('=' | '+' | '-' | '@' | '\t' | '\r' | '\n')
    );
    let value = if needs_neutralizing {
        format!("'{value}")
    } else {
        value.to_string()
    };
    format!("\"{}\"", value.replace('"', "\"\""))
}

pub fn cost_csv_with_conn(
    conn: &Connection,
    range: &UsageRange,
    now_ms: i64,
) -> Result<String, UsageError> {
    let costs = costs_with_conn(conn, range, now_ms, None)?;
    let config = billing::load_with_conn(conn)?;
    let mut out = String::from(CSV_HEADER);
    out.push('\n');
    for row in &costs.rows {
        let profile_name = row
            .billing_profile_id
            .as_deref()
            .and_then(|id| config.profiles.iter().find(|profile| profile.id == id))
            .map(|profile| profile.name.clone())
            .unwrap_or_default();
        let source_ids = row
            .source_ids
            .iter()
            .map(|source| source.db_id())
            .collect::<Vec<_>>()
            .join(",");
        let (pricing_provider, pricing_model) = row
            .pricing_reference
            .as_ref()
            .map(|reference| (reference.provider_id.as_str(), reference.model_id.as_str()))
            .unwrap_or(("", ""));
        // 单值证据列只在「恰好一个非空 provider 且无未标注行」时填写；
        // 完整证据仍由 `observedProviders` 字段（非 CSV 列）承载。
        let observed_provider = if row.has_empty_provider || row.observed_providers.len() != 1 {
            String::new()
        } else {
            row.observed_providers[0].clone()
        };
        let cells = [
            row.billing_profile_id.clone().unwrap_or_default(),
            profile_name,
            source_ids,
            observed_provider,
            row.identity.model_id.clone(),
            kind_str(row.attribution_kind).to_string(),
            row.token_usage.to_string(),
            row.estimated_usd.clone().unwrap_or_default(),
            status_str(row.status).to_string(),
            row.priced_tokens.to_string(),
            row.unpriced_tokens.to_string(),
            row.coverage_rate
                .map(|rate| format!("{rate:.6}"))
                .unwrap_or_default(),
            pricing_provider.to_string(),
            pricing_model.to_string(),
            row.pricing_snapshot_id.clone().unwrap_or_default(),
            basis_str(row.pricing_basis).to_string(),
            row.price_updated_at
                .map(|value| value.to_string())
                .unwrap_or_default(),
            row.channel_key.clone(),
            channel_kind_str(row.channel_kind).to_string(),
            row.display_provider.clone().unwrap_or_default(),
            row.normalized_model_id.clone().unwrap_or_default(),
            row.pricing_release_date.clone().unwrap_or_default(),
            row.price_override.to_string(),
        ];
        out.push_str(
            &cells
                .iter()
                .map(|cell| csv_cell(cell))
                .collect::<Vec<_>>()
                .join(","),
        );
        out.push('\n');
    }
    Ok(out)
}

// ── 生产入口（IPC 用）：只取 DB 锁，业务走 `*_with_conn` ─────────────────────

fn poisoned() -> UsageError {
    UsageError::new("usage-db-failed", "db poisoned lock")
}

pub fn costs(
    range: &UsageRange,
    now_ms: i64,
    fallback_rate: Option<&str>,
) -> Result<UsageCosts, UsageError> {
    let conn = DB.lock().map_err(|_| poisoned())?;
    costs_with_conn(&conn, range, now_ms, fallback_rate)
}

pub fn cost_csv(range: &UsageRange, now_ms: i64) -> Result<String, UsageError> {
    let conn = DB.lock().map_err(|_| poisoned())?;
    cost_csv_with_conn(&conn, range, now_ms)
}

// ==== usage 自省锁：以下为排除区（测试代码），勿删勿复制 ====
#[cfg(test)]
mod tests {
    use super::*;

    fn channel(key: &str, kind: ChannelKind) -> ResolvedChannel {
        ResolvedChannel {
            key: key.to_string(),
            kind,
            display_provider: None,
            billing_profile_id: None,
        }
    }

    #[test]
    fn row_key_separates_pricing_basis_and_snapshot() {
        let identity = IdentityKey {
            source_id: UsageSourceId::Codex,
            observed_provider: None,
            model_id: "m".to_string(),
        };
        let bp = channel("bp:A", ChannelKind::BillingProfile);
        let base = row_key(
            &bp,
            &identity,
            None,
            None,
            PricingBasis::BaseCurrent,
            Some("snap:1"),
            false,
        );
        let insufficient = row_key(
            &bp,
            &identity,
            None,
            None,
            PricingBasis::Insufficient,
            Some("snap:1"),
            false,
        );
        let other_snapshot = row_key(
            &bp,
            &identity,
            None,
            None,
            PricingBasis::BaseCurrent,
            Some("snap:2"),
            false,
        );
        assert_ne!(base, insufficient, "同 key 不同 pricingBasis 不得合并");
        assert_ne!(base, other_snapshot, "不同快照不得合并");
    }

    #[test]
    fn row_key_only_adds_tool_context_for_unassigned_and_ambiguous() {
        let codex = IdentityKey {
            source_id: UsageSourceId::Codex,
            observed_provider: None,
            model_id: "m".to_string(),
        };
        let claude = IdentityKey {
            source_id: UsageSourceId::Claude,
            observed_provider: None,
            model_id: "m".to_string(),
        };
        let provider = channel("provider:openai", ChannelKind::Provider);
        assert_eq!(
            row_key(
                &provider,
                &codex,
                None,
                None,
                PricingBasis::Insufficient,
                None,
                false
            ),
            row_key(
                &provider,
                &claude,
                None,
                None,
                PricingBasis::Insufficient,
                None,
                false
            ),
            "明确渠道跨工具必须合并"
        );
        let unassigned = channel("unassigned", ChannelKind::Unassigned);
        assert_ne!(
            row_key(
                &unassigned,
                &codex,
                None,
                None,
                PricingBasis::Insufficient,
                None,
                false
            ),
            row_key(
                &unassigned,
                &claude,
                None,
                None,
                PricingBasis::Insufficient,
                None,
                false
            ),
            "未指定渠道必须保留工具上下文"
        );
    }
}
// ==== usage 自省锁：排除区结束，勿删勿复制 ====
