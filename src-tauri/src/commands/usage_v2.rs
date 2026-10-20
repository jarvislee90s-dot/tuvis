//! V2 身份 / Token IPC（Task 5）。纪律：
//! * 全部命令 `async` + `spawn_blocking` 出主线程；
//! * 命令层**不取 DB 锁、不触发采集**——锁与只读逻辑都在服务层的生产入口；
//! * 参数名即 JS 侧 camelCase 键名（不使用 `rename_all`）。
//!
//! 锁：`tests/usage_ipc_test.rs` 的
//! `v2_commands_are_registered_and_obey_the_lock_discipline`。

use crate::services::usage::error::UsageError;
use crate::services::usage::model::{UsageFilters, UsageRange};
use crate::services::usage::v2::model::{
    BillingCommit, BillingConfig, BillingPreview, BillingRevision, ChannelOption,
    DistributionGroup, IdentityUsage, ModelDisplayNameRequest, ObservedModelEntry,
    PriceOverrideRequest, PriceOverrideView, PricingAutoMatch, PricingCatalogEntry,
    PricingCatalogSearch, PricingIdentityRequest, PricingQuotePreview, PricingStatus,
    ProviderAliasConfig, ProviderAliasRule, TrendSelection, UsageCosts, UsageDistribution,
    UsageTrend,
};
use crate::services::usage::v2::pricing::MatchScope;
use crate::services::usage::v2::{billing, channel, cost_query, model_config, pricing, query};

fn task_failed(label: &str, error: impl std::fmt::Display) -> UsageError {
    UsageError::internal(format!("{label} task failed: {error}"))
}

#[tauri::command]
pub async fn usage_billing_get() -> Result<BillingConfig, UsageError> {
    tauri::async_runtime::spawn_blocking(billing::load)
        .await
        .map_err(|e| task_failed("billing get", e))?
}

#[tauri::command]
pub async fn usage_billing_preview(
    candidate: BillingConfig,
    range: UsageRange,
) -> Result<BillingPreview, UsageError> {
    tauri::async_runtime::spawn_blocking(move || {
        billing::preview(&candidate, &range, crate::services::usage::now_ms())
    })
    .await
    .map_err(|e| task_failed("billing preview", e))?
}

#[tauri::command]
pub async fn usage_billing_commit(request: BillingCommit) -> Result<BillingRevision, UsageError> {
    tauri::async_runtime::spawn_blocking(move || {
        billing::commit(&request, crate::services::usage::now_ms())
    })
    .await
    .map_err(|e| task_failed("billing commit", e))?
}

#[tauri::command]
pub async fn usage_billing_undo(
    expected_revision: i64,
    target_revision: i64,
) -> Result<BillingRevision, UsageError> {
    tauri::async_runtime::spawn_blocking(move || {
        billing::undo(
            expected_revision,
            target_revision,
            crate::services::usage::now_ms(),
        )
    })
    .await
    .map_err(|e| task_failed("billing undo", e))?
}

#[tauri::command]
pub async fn usage_identities(
    range: UsageRange,
    filters: UsageFilters,
) -> Result<Vec<IdentityUsage>, UsageError> {
    tauri::async_runtime::spawn_blocking(move || {
        query::identities(&range, &filters, crate::services::usage::now_ms())
    })
    .await
    .map_err(|e| task_failed("identities", e))?
}

#[tauri::command]
pub async fn usage_trend(
    range: UsageRange,
    selection: TrendSelection,
) -> Result<UsageTrend, UsageError> {
    tauri::async_runtime::spawn_blocking(move || {
        query::trend(&range, &selection, crate::services::usage::now_ms())
    })
    .await
    .map_err(|e| task_failed("trend", e))?
}

#[tauri::command]
pub async fn usage_distribution(
    range: UsageRange,
    group_by: DistributionGroup,
) -> Result<UsageDistribution, UsageError> {
    tauri::async_runtime::spawn_blocking(move || {
        query::distribution(&range, group_by, crate::services::usage::now_ms())
    })
    .await
    .map_err(|e| task_failed("distribution", e))?
}

#[tauri::command]
pub async fn usage_pricing_get() -> Result<PricingStatus, UsageError> {
    tauri::async_runtime::spawn_blocking(|| pricing::status(crate::services::usage::now_ms()))
        .await
        .map_err(|e| task_failed("pricing get", e))?
}

#[tauri::command]
pub async fn usage_pricing_set_enabled(enabled: bool) -> Result<PricingStatus, UsageError> {
    // `set_enabled(true)` 只启动后台刷新，不在此等待网络。
    tauri::async_runtime::spawn_blocking(move || pricing::set_enabled(enabled))
        .await
        .map_err(|e| task_failed("pricing set enabled", e))?
}

#[tauri::command]
pub async fn usage_pricing_refresh() -> Result<PricingStatus, UsageError> {
    tauri::async_runtime::spawn_blocking(|| {
        pricing::refresh(true, crate::services::usage::now_ms())
    })
    .await
    .map_err(|e| task_failed("pricing refresh", e))?
}

/// 参考 API 模型下拉搜索：只读 active 快照；无快照返回空数组。
#[tauri::command]
pub async fn usage_pricing_catalog(
    query: Option<String>,
    limit: Option<u32>,
) -> Result<Vec<PricingCatalogEntry>, UsageError> {
    tauri::async_runtime::spawn_blocking(move || pricing::catalog_entries(query.as_deref(), limit))
        .await
        .map_err(|e| task_failed("pricing catalog", e))?
}

#[tauri::command]
pub async fn usage_costs(
    range: UsageRange,
    fallback_rate: Option<String>,
) -> Result<UsageCosts, UsageError> {
    tauri::async_runtime::spawn_blocking(move || {
        cost_query::costs(
            &range,
            crate::services::usage::now_ms(),
            fallback_rate.as_deref(),
        )
    })
    .await
    .map_err(|e| task_failed("costs", e))?
}

#[tauri::command]
pub async fn usage_cost_export_csv(range: UsageRange) -> Result<String, UsageError> {
    tauri::async_runtime::spawn_blocking(move || {
        cost_query::cost_csv(&range, crate::services::usage::now_ms())
    })
    .await
    .map_err(|e| task_failed("cost csv", e))?
}

// ── Task 6：供应商别名 / 渠道选项 / 手动覆盖价 ───────────────────────────────

#[tauri::command]
pub async fn usage_provider_aliases_get() -> Result<ProviderAliasConfig, UsageError> {
    tauri::async_runtime::spawn_blocking(channel::load)
        .await
        .map_err(|e| task_failed("provider aliases get", e))?
}

#[tauri::command]
pub async fn usage_provider_alias_upsert(
    rule: ProviderAliasRule,
) -> Result<ProviderAliasConfig, UsageError> {
    tauri::async_runtime::spawn_blocking(move || {
        channel::upsert(&rule, crate::services::usage::now_ms())
    })
    .await
    .map_err(|e| task_failed("provider alias upsert", e))?
}

#[tauri::command]
pub async fn usage_provider_alias_delete(id: String) -> Result<ProviderAliasConfig, UsageError> {
    tauri::async_runtime::spawn_blocking(move || channel::delete(&id))
        .await
        .map_err(|e| task_failed("provider alias delete", e))?
}

#[tauri::command]
pub async fn usage_channel_options(range: UsageRange) -> Result<Vec<ChannelOption>, UsageError> {
    tauri::async_runtime::spawn_blocking(move || {
        query::channel_options(&range, crate::services::usage::now_ms())
    })
    .await
    .map_err(|e| task_failed("channel options", e))?
}

#[tauri::command]
pub async fn usage_pricing_overrides_get() -> Result<Vec<PriceOverrideView>, UsageError> {
    tauri::async_runtime::spawn_blocking(|| pricing::overrides(crate::services::usage::now_ms()))
        .await
        .map_err(|e| task_failed("pricing overrides get", e))?
}

#[tauri::command]
pub async fn usage_pricing_override_set(
    request: PriceOverrideRequest,
) -> Result<Vec<PriceOverrideView>, UsageError> {
    tauri::async_runtime::spawn_blocking(move || {
        pricing::set_override(&request, crate::services::usage::now_ms())
    })
    .await
    .map_err(|e| task_failed("pricing override set", e))?
}

#[tauri::command]
pub async fn usage_pricing_override_delete(
    normalized_model_id: String,
) -> Result<Vec<PriceOverrideView>, UsageError> {
    tauri::async_runtime::spawn_blocking(move || pricing::delete_override(&normalized_model_id))
        .await
        .map_err(|e| task_failed("pricing override delete", e))?
}

// ── 2026-10-20 计划 Task 5：观察条目 / 模型条目配置 / 精选价目目录 ─────────────

/// 观察条目（供应商/模型条目数据源；全历史，来自 usage_daily，与查询范围无关）。
#[tauri::command]
pub async fn usage_observed_entries_get() -> Result<Vec<ObservedModelEntry>, UsageError> {
    tauri::async_runtime::spawn_blocking(|| {
        model_config::observed_entries(crate::services::usage::now_ms())
    })
    .await
    .map_err(|e| task_failed("observed entries get", e))?
}

/// 设置自定义模型显示名；`displayName=null` = 清除（回原名）。返回刷新后的全量列表。
#[tauri::command]
pub async fn usage_model_display_name_set(
    request: ModelDisplayNameRequest,
) -> Result<Vec<ObservedModelEntry>, UsageError> {
    tauri::async_runtime::spawn_blocking(move || {
        model_config::set_display_name(&request, crate::services::usage::now_ms())
    })
    .await
    .map_err(|e| task_failed("model display name set", e))?
}

/// 设置计价身份覆盖；`reference=null` = 清除（回自动匹配）。返回刷新后的全量列表。
#[tauri::command]
pub async fn usage_pricing_identity_set(
    request: PricingIdentityRequest,
) -> Result<Vec<ObservedModelEntry>, UsageError> {
    tauri::async_runtime::spawn_blocking(move || {
        model_config::set_pricing_identity(&request, crate::services::usage::now_ms())
    })
    .await
    .map_err(|e| task_failed("pricing identity set", e))?
}

/// 作用域定价目录搜索（`scope`: "curated"（缺省）| "all"）。
#[tauri::command]
pub async fn usage_pricing_catalog_search(
    query: Option<String>,
    limit: Option<u32>,
    scope: Option<String>,
) -> Result<PricingCatalogSearch, UsageError> {
    tauri::async_runtime::spawn_blocking(move || {
        let scope = match scope.as_deref() {
            None => MatchScope::Curated,
            Some(raw) => MatchScope::parse(raw)?,
        };
        pricing::catalog_search_scoped(query.as_deref(), limit, scope)
    })
    .await
    .map_err(|e| task_failed("pricing catalog search", e))?
}

/// 自动匹配预览（固定精选作用域：直营官方 > Ollama Cloud > OpenRouter）。
#[tauri::command]
pub async fn usage_pricing_auto_match(
    model_id: String,
    fallback_rate: Option<String>,
) -> Result<PricingAutoMatch, UsageError> {
    tauri::async_runtime::spawn_blocking(move || {
        pricing::auto_match(&model_id, fallback_rate.as_deref())
    })
    .await
    .map_err(|e| task_failed("pricing auto match", e))?
}

/// 目录条目价格预览（row3 悬停价格表）；无条目 / 无快照 ⇒ null。
#[tauri::command]
pub async fn usage_pricing_quote_get(
    provider_id: String,
    model_id: String,
    fallback_rate: Option<String>,
) -> Result<Option<PricingQuotePreview>, UsageError> {
    tauri::async_runtime::spawn_blocking(move || {
        pricing::quote_preview(&provider_id, &model_id, fallback_rate.as_deref())
    })
    .await
    .map_err(|e| task_failed("pricing quote get", e))?
}
