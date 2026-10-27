// V2 数据层（Task 5）：四类只读查询。
//
// 查询键一律 `["usage", "v2", <域>, ...参数]`，因此保存/撤销后一次
// `invalidateQueries({ queryKey: USAGE_KEY })` 即可覆盖旧看板与全部 V2 查询。
//
// 本文件含 `refetchInterval`（恒 `false`）→ 会落进旧 `commands/usage.rs` 的静态锁扫描面。
// 该锁断言这些文件里不含四条旧用量命令的 snake_case 字面量；故**注释里也不写**它们，
// 要引用只写中文名或本文件的 camelCase 包装名。读请求绝不触发扫描。
import { useEffect } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { listen } from "@tauri-apps/api/event";
import {
  usageBillingGet,
  usageChannelOptions,
  usageCosts,
  usageDistribution,
  usageIdentities,
  usageModelDisplayNameSet,
  usageObservedEntriesGet,
  usagePricingAutoMatch,
  usagePricingGet,
  usagePricingCatalog,
  usagePricingCatalogSearch,
  usagePricingIdentitySet,
  usagePricingOverridesGet,
  usagePricingQuoteGet,
  usageProviderAliasesGet,
  usageTrend,
} from "@/lib/api/usageV2";
import type { UsageFilters, UsageRange } from "@/types/usage";
import type {
  BillingConfig,
  ChannelOption,
  DistributionGroup,
  IdentityUsage,
  ModelDisplayNameRequest,
  ObservedModelEntry,
  PriceOverrideView,
  PricingAutoMatch,
  PricingCatalogEntry,
  PricingCatalogSearch,
  PricingIdentityRequest,
  PricingQuotePreview,
  PricingStatus,
  ProviderAliasConfig,
  TrendSelection,
  UsageCosts,
  UsageDistribution,
  UsageTrend,
} from "@/types/usageV2";

// 统一从旧数据层复用前缀，避免两份 `["usage"]` 常量漂移。
import { USAGE_KEY } from "./usage";
export { USAGE_KEY };

/**
 * 当前计费配置（保存/撤销后由 `USAGE_KEY` 前缀失效带走）。
 * `opts.enabled`（默认 true）＝ Task 7 active 门控（评审修复轮 2）：BillingProfilesManager
 * 随记录面板恒挂载，页面隐藏期间停发（含 focus refetch）。
 */
export function useUsageBillingQuery(opts?: { enabled?: boolean }) {
  return useQuery<BillingConfig>({
    queryKey: ["usage", "v2", "billing"],
    queryFn: usageBillingGet,
    staleTime: 30_000,
    refetchInterval: false,
    enabled: opts?.enabled ?? true,
  });
}

export function useUsageIdentitiesQuery(
  range: UsageRange,
  filters: UsageFilters,
  /** `enabled: false` 时不发查询（恒挂载的宿主组件用它做打开门控） */
  opts?: { enabled?: boolean }
) {
  return useQuery<IdentityUsage[]>({
    queryKey: ["usage", "v2", "identities", range, filters],
    queryFn: () => usageIdentities(range, filters),
    staleTime: 30_000,
    refetchInterval: false,
    enabled: opts?.enabled ?? true,
  });
}

export function useUsageTrendQuery(
  range: UsageRange,
  selection: TrendSelection,
  /** `enabled: false` 时不发查询（渠道选项未就绪的空键选择用它门控） */
  opts?: { enabled?: boolean }
) {
  return useQuery<UsageTrend>({
    queryKey: ["usage", "v2", "trend", range, selection],
    queryFn: () => usageTrend(range, selection),
    staleTime: 30_000,
    refetchInterval: false,
    enabled: opts?.enabled ?? true,
  });
}

export function useUsageDistributionQuery(
  range: UsageRange,
  groupBy: DistributionGroup,
  /** `enabled: false` 时不发查询（Task 7 四页宿主的隐藏页用它门控） */
  opts?: { enabled?: boolean }
) {
  return useQuery<UsageDistribution>({
    queryKey: ["usage", "v2", "distribution", range, groupBy],
    queryFn: () => usageDistribution(range, groupBy),
    staleTime: 30_000,
    refetchInterval: false,
    enabled: opts?.enabled ?? true,
  });
}

/**
 * 费用行只读账本；**不触发价格刷新**。fallbackRate = 设置页备用汇率（仅无快照汇率时用）。
 * `opts.enabled`（默认 true）＝ Task 7 的 active 门控（评审 Important 2）：嵌入四页宿主后
 * 隐藏期间不发请求（含 focus refetch——queryClient 未禁 refetchOnWindowFocus，只能靠 enabled 挡）。
 */
export function useUsageCostsQuery(
  range: UsageRange,
  fallbackRate: string,
  opts?: { enabled?: boolean }
) {
  return useQuery<UsageCosts>({
    queryKey: ["usage", "v2", "costs", range, fallbackRate],
    queryFn: () => usageCosts(range, fallbackRate),
    staleTime: 30_000,
    refetchInterval: false,
    enabled: opts?.enabled ?? true,
  });
}

export function useUsagePricingQuery(opts?: { enabled?: boolean }) {
  return useQuery<PricingStatus>({
    queryKey: ["usage", "v2", "pricing"],
    queryFn: usagePricingGet,
    staleTime: 30_000,
    refetchInterval: false,
    enabled: opts?.enabled ?? true,
  });
}

/**
 * 参考 API 模型目录搜索：query 进键，trim 后至少 2 个字符才发请求。
 * `opts.enabled` 供宿主在「无快照」时门控（无快照搜索必然为空）。
 */
export function useUsagePricingCatalogQuery(query: string, opts?: { enabled?: boolean }) {
  const trimmed = query.trim();
  return useQuery<PricingCatalogEntry[]>({
    queryKey: ["usage", "v2", "pricingCatalog", trimmed],
    queryFn: () => usagePricingCatalog(trimmed, 20),
    staleTime: 30_000,
    refetchInterval: false,
    enabled: (opts?.enabled ?? true) && trimmed.length >= 2,
  });
}

// ── Task 6：别名 / 渠道选项 / 覆盖价（只读；写侧由调用方统一失效 USAGE_KEY）──

/**
 * 供应商别名配置（设置页 / 记录页浮窗共用的唯一查询入口）。
 * `opts.enabled`（默认 true）＝ Task 7 active 门控（评审 Important 2）：记录页隐藏期间不发。
 */
export function useUsageProviderAliasesQuery(opts?: { enabled?: boolean }) {
  return useQuery<ProviderAliasConfig>({
    queryKey: ["usage", "v2", "providerAliases"],
    queryFn: usageProviderAliasesGet,
    staleTime: 30_000,
    refetchInterval: false,
    enabled: opts?.enabled ?? true,
  });
}

/** 趋势筛选用显示渠道；range 进键，切范围即换查询。 */
export function useUsageChannelOptionsQuery(range: UsageRange, opts?: { enabled?: boolean }) {
  return useQuery<ChannelOption[]>({
    queryKey: ["usage", "v2", "channelOptions", range],
    queryFn: () => usageChannelOptions(range),
    staleTime: 30_000,
    refetchInterval: false,
    enabled: opts?.enabled ?? true,
  });
}

/** 手动覆盖价列表（保存 / 删除后由调用方 `invalidateQueries({queryKey: USAGE_KEY})`） */
export function useUsagePriceOverridesQuery() {
  return useQuery<PriceOverrideView[]>({
    queryKey: ["usage", "v2", "priceOverrides"],
    queryFn: usagePricingOverridesGet,
    staleTime: 30_000,
    refetchInterval: false,
  });
}

/**
 * 订阅 `usage-pricing-updated`：刷新成功或失败后失效 pricing 与全部 costs 查询。
 * 不引入任何轮询；卸载时解除监听（若卸载时 listen 仍未 resolve，则 resolve 后立即解除）。
 */
export function useUsagePricingEvents() {
  const queryClient = useQueryClient();
  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void listen("usage-pricing-updated", () => {
      void queryClient.invalidateQueries({ queryKey: ["usage", "v2", "pricing"] });
      void queryClient.invalidateQueries({ queryKey: ["usage", "v2", "costs"] });
    }).then((off) => {
      if (disposed) {
        off();
      } else {
        unlisten = off;
      }
    });
    return () => {
      disposed = true;
      unlisten?.();
      unlisten = undefined;
    };
  }, [queryClient]);
}

// ── 2026-10-20 计划 Task 5：观察条目 / 精选目录（读）；模型配置（写后失效 USAGE_KEY）──

/** 观察条目（A 段与 B 段共同数据源）。宿主用 `opts.enabled` 做「弹窗打开才拉取」门控。 */
export function useUsageObservedEntriesQuery(opts?: { enabled?: boolean }) {
  return useQuery<ObservedModelEntry[]>({
    queryKey: ["usage", "v2", "observedEntries"],
    queryFn: usageObservedEntriesGet,
    staleTime: 30_000,
    refetchInterval: false,
    enabled: opts?.enabled ?? true,
  });
}

/**
 * 精选目录搜索：query + scope 进键，trim 后至少 2 个字符才发请求；
 * scope="curated"（默认，只含直营官方（含中国站变体）+ Ollama Cloud + OpenRouter）| "all"（显示全部 N 个来源）。
 */
export function useUsagePricingCatalogSearchQuery(
  query: string,
  scope: "curated" | "all",
  opts?: { enabled?: boolean }
) {
  const trimmed = query.trim();
  return useQuery<PricingCatalogSearch>({
    queryKey: ["usage", "v2", "pricingCatalogSearch", trimmed, scope],
    queryFn: () => usagePricingCatalogSearch(trimmed, 20, scope),
    staleTime: 30_000,
    refetchInterval: false,
    enabled: (opts?.enabled ?? true) && trimmed.length >= 2,
  });
}

/** 自动匹配预览（固定精选作用域）；`modelId` 空串时不发查询。 */
export function useUsagePricingAutoMatchQuery(
  modelId: string,
  fallbackRate: string,
  opts?: { enabled?: boolean }
) {
  return useQuery<PricingAutoMatch>({
    queryKey: ["usage", "v2", "pricingAutoMatch", modelId, fallbackRate],
    queryFn: () => usagePricingAutoMatch(modelId, fallbackRate),
    staleTime: 30_000,
    refetchInterval: false,
    enabled: (opts?.enabled ?? true) && modelId.trim().length > 0,
  });
}

/** 目录条目价格预览（悬停价格表）；键缺失时不发查询。 */
export function useUsagePricingQuoteQuery(
  providerId: string | null,
  modelId: string | null,
  fallbackRate: string,
  opts?: { enabled?: boolean }
) {
  const ready = Boolean(providerId && modelId);
  return useQuery<PricingQuotePreview | null>({
    queryKey: ["usage", "v2", "pricingQuote", providerId ?? "", modelId ?? "", fallbackRate],
    queryFn: () => usagePricingQuoteGet(providerId as string, modelId as string, fallbackRate),
    staleTime: 30_000,
    refetchInterval: false,
    enabled: (opts?.enabled ?? true) && ready,
  });
}

/** 写自定义模型显示名；成功后失效 USAGE_KEY（观察条目与费用行联动刷新）。 */
export function useUsageModelDisplayNameSet() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (request: ModelDisplayNameRequest) => usageModelDisplayNameSet(request),
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: USAGE_KEY });
    },
  });
}

/** 写计价身份覆盖；成功后失效 USAGE_KEY（费用行计价随身份覆盖联动刷新）。 */
export function useUsagePricingIdentitySet() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (request: PricingIdentityRequest) => usagePricingIdentitySet(request),
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: USAGE_KEY });
    },
  });
}
