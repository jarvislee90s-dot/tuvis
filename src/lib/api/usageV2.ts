// V2 身份 / Token IPC 唯一入口（Task 5）。参数名逐字对齐契约，JS 侧一律 camelCase。
// 读/写命令都只读或写本地账本；本文件**没有任何扫描入口**。
import { invoke } from "@tauri-apps/api/core";
import type { UsageFilters, UsageRange } from "@/types/usage";
import type {
  BillingCommit,
  BillingConfig,
  BillingPreview,
  BillingRevision,
  ChannelOption,
  DistributionGroup,
  IdentityUsage,
  ModelDisplayNameRequest,
  ObservedModelEntry,
  PriceOverrideRequest,
  PriceOverrideView,
  PricingAutoMatch,
  PricingCatalogEntry,
  PricingCatalogSearch,
  PricingIdentityRequest,
  PricingQuotePreview,
  PricingStatus,
  ProviderAliasConfig,
  ProviderAliasRule,
  TrendSelection,
  UsageCosts,
  UsageDistribution,
  UsageTrend,
} from "@/types/usageV2";

/** 当前生效的计费配置（无提交时 revision=0 的空配置） */
export async function usageBillingGet(): Promise<BillingConfig> {
  return await invoke<BillingConfig>("usage_billing_get");
}

/** 保存前预览：候选配置 + 查询范围；**不写库** */
export async function usageBillingPreview(
  candidate: BillingConfig,
  range: UsageRange
): Promise<BillingPreview> {
  return await invoke<BillingPreview>("usage_billing_preview", { candidate, range });
}

/** 乐观并发提交：expectedRevision 与当前不一致时 reject `usage-settings-invalid` */
export async function usageBillingCommit(request: BillingCommit): Promise<BillingRevision> {
  return await invoke<BillingRevision>("usage_billing_commit", { request });
}

/** 撤销到历史版本：以新 revision 追加一次提交，不删除历史 */
export async function usageBillingUndo(
  expectedRevision: number,
  targetRevision: number
): Promise<BillingRevision> {
  return await invoke<BillingRevision>("usage_billing_undo", {
    expectedRevision,
    targetRevision,
  });
}

/** 完整身份 + 归因 + Token（只读账本） */
export async function usageIdentities(
  range: UsageRange,
  filters: UsageFilters
): Promise<IdentityUsage[]> {
  return await invoke<IdentityUsage[]>("usage_identities", { range, filters });
}

/** 带局部筛选的趋势（只读账本） */
export async function usageTrend(
  range: UsageRange,
  selection: TrendSelection
): Promise<UsageTrend> {
  return await invoke<UsageTrend>("usage_trend", { range, selection });
}

/** 带分组维度的分布（只读账本） */
export async function usageDistribution(
  range: UsageRange,
  groupBy: DistributionGroup
): Promise<UsageDistribution> {
  return await invoke<UsageDistribution>("usage_distribution", { range, groupBy });
}

/** 当前定价状态（是否启用、快照时间、是否过期/刷新中） */
export async function usagePricingGet(): Promise<PricingStatus> {
  return await invoke<PricingStatus>("usage_pricing_get");
}

/** 切换定价开关；开启时后端只启动后台刷新，不阻塞等待网络 */
export async function usagePricingSetEnabled(enabled: boolean): Promise<PricingStatus> {
  return await invoke<PricingStatus>("usage_pricing_set_enabled", { enabled });
}

/** 显式手动刷新公开价格（成功后后端会发 usage-pricing-updated 事件） */
export async function usagePricingRefresh(): Promise<PricingStatus> {
  return await invoke<PricingStatus>("usage_pricing_refresh");
}

/** 搜索 active 定价目录（大小写不敏感；无快照返回空数组） */
export async function usagePricingCatalog(
  query: string,
  limit?: number
): Promise<PricingCatalogEntry[]> {
  return await invoke<PricingCatalogEntry[]>("usage_pricing_catalog", { query, limit });
}

/** 模型级等价费用行（只读账本；**不刷新价格**） */
export async function usageCosts(range: UsageRange, fallbackRate?: string): Promise<UsageCosts> {
  return await invoke<UsageCosts>("usage_costs", { range, fallbackRate });
}

/** 费用 CSV 文本（不含 BOM、无总计行） */
export async function usageCostExportCsv(range: UsageRange): Promise<string> {
  return await invoke<string>("usage_cost_export_csv", { range });
}

// ── Task 6：供应商别名 / 渠道选项 / 手动覆盖价 ───────────────────────────────

/** 当前供应商别名配置（与计费配置分离，不参与 revision） */
export async function usageProviderAliasesGet(): Promise<ProviderAliasConfig> {
  return await invoke<ProviderAliasConfig>("usage_provider_aliases_get");
}

/** 按 id 新增或覆盖一条别名；目标键被其它规则占用时 reject `usage-settings-invalid` */
export async function usageProviderAliasUpsert(
  rule: ProviderAliasRule
): Promise<ProviderAliasConfig> {
  return await invoke<ProviderAliasConfig>("usage_provider_alias_upsert", { rule });
}

/** 删除一条别名（不存在的 id 为幂等空操作） */
export async function usageProviderAliasDelete(id: string): Promise<ProviderAliasConfig> {
  return await invoke<ProviderAliasConfig>("usage_provider_alias_delete", { id });
}

/** 趋势筛选可用的显示渠道（只含 billingProfile / alias / provider） */
export async function usageChannelOptions(range: UsageRange): Promise<ChannelOption[]> {
  return await invoke<ChannelOption[]>("usage_channel_options", { range });
}

/** 全部手动覆盖价（无 active 目录时来源字段为 null，覆盖值仍可用） */
export async function usagePricingOverridesGet(): Promise<PriceOverrideView[]> {
  return await invoke<PriceOverrideView[]>("usage_pricing_overrides_get");
}

/** 写入一条覆盖价并返回完整列表；四价至少一项，否则 reject `usage-settings-invalid` */
export async function usagePricingOverrideSet(
  request: PriceOverrideRequest
): Promise<PriceOverrideView[]> {
  return await invoke<PriceOverrideView[]>("usage_pricing_override_set", { request });
}

/** 删除指定归一化模型的覆盖价并返回完整列表 */
export async function usagePricingOverrideDelete(
  normalizedModelId: string
): Promise<PriceOverrideView[]> {
  return await invoke<PriceOverrideView[]>("usage_pricing_override_delete", {
    normalizedModelId,
  });
}

// ── 2026-10-20 计划 Task 5：观察条目 / 模型条目配置 / 精选价目目录 ─────────────

/** 观察条目（A 段与 B 段的共同数据源；全历史，与查询范围无关） */
export async function usageObservedEntriesGet(): Promise<ObservedModelEntry[]> {
  return await invoke<ObservedModelEntry[]>("usage_observed_entries_get");
}

/** 设置自定义模型显示名（null = 清除回原名）；返回刷新后的全量观察条目 */
export async function usageModelDisplayNameSet(
  request: ModelDisplayNameRequest
): Promise<ObservedModelEntry[]> {
  return await invoke<ObservedModelEntry[]>("usage_model_display_name_set", { request });
}

/** 设置计价身份覆盖（null = 清除回自动匹配）；返回刷新后的全量观察条目 */
export async function usagePricingIdentitySet(
  request: PricingIdentityRequest
): Promise<ObservedModelEntry[]> {
  return await invoke<ObservedModelEntry[]>("usage_pricing_identity_set", { request });
}

/** 作用域定价目录搜索（scope 缺省 "curated"；providerCount 恒为全量目录 provider 数） */
export async function usagePricingCatalogSearch(
  query: string,
  limit?: number,
  scope?: "curated" | "all"
): Promise<PricingCatalogSearch> {
  return await invoke<PricingCatalogSearch>("usage_pricing_catalog_search", {
    query,
    limit,
    scope,
  });
}

/** 自动匹配预览（固定精选作用域：直营官方 > Ollama Cloud > OpenRouter） */
export async function usagePricingAutoMatch(
  modelId: string,
  fallbackRate?: string
): Promise<PricingAutoMatch> {
  return await invoke<PricingAutoMatch>("usage_pricing_auto_match", {
    modelId,
    fallbackRate,
  });
}

/** 目录条目价格预览（精确键四价 + 快照日期；无条目 / 无快照 ⇒ null） */
export async function usagePricingQuoteGet(
  providerId: string,
  modelId: string,
  fallbackRate?: string
): Promise<PricingQuotePreview | null> {
  return await invoke<PricingQuotePreview | null>("usage_pricing_quote_get", {
    providerId,
    modelId,
    fallbackRate,
  });
}
