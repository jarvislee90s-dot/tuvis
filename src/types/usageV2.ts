// 用量看板 V2 冻结契约：身份与 Token 类型（本任务不声明费用/定价类型）。
// Rust 侧逐字镜像；Option 字段在 wire 上显式序列化为 null。
import type {
  UsageBuckets,
  UsageMetrics,
  UsageRange,
  UsageSourceId,
  UsageSourceKind,
  TrendPoint,
  CompareBlock,
} from "@/types/usage";

export type AttributionKind = "observed" | "userAssigned" | "unknown" | "ambiguous";
export type DistributionGroup = "billingProfile" | "tool" | "project" | "model";

/** 显示渠道类型：与账本原始身份分离，仅用于展示/筛选分类。 */
export type ChannelKind = "billingProfile" | "alias" | "provider" | "unassigned" | "ambiguous";
/** 供应商别名的两种作用域：非空 provider 精确映射 / 空 provider 的 tool+model 指派。 */
export type ProviderAliasKind = "provider" | "toolModel";

/**
 * 一条供应商别名规则。
 * - `kind="provider"`：`provider` 必填，`sourceId`/`modelId` 为 null；
 * - `kind="toolModel"`：`sourceId`/`modelId` 必填（`modelId=""` 表示未知模型），`provider` 为 null。
 */
export interface ProviderAliasRule {
  id: string;
  kind: ProviderAliasKind;
  provider: string | null;
  sourceId: UsageSourceId | null;
  modelId: string | null;
  displayName: string;
}

/** 供应商别名的完整配置（与计费配置分离，不参与 revision）。 */
export interface ProviderAliasConfig {
  aliases: ProviderAliasRule[];
}

/** 趋势筛选的渠道选项（只含可筛选的渠道类型）。 */
export interface ChannelOption {
  /** `bp:<profileId>` 或 `provider:<displayName>`。 */
  key: string;
  label: string;
  kind: ChannelKind;
  billingProfileId: string | null;
  tokenUsage: number;
}

export type TrendSelection =
  | { scope: "all" }
  | { scope: "tool"; selectedId: UsageSourceId }
  | { scope: "billingProfile"; selectedId: string }
  | { scope: "unknown"; selectedId: UsageSourceId };

export interface IdentityKey {
  sourceId: UsageSourceId;
  observedProvider: string | null;
  modelId: string;
}

export interface BillingProfile {
  id: string;
  name: string;
  kind: "officialSubscription" | "api" | "thirdPartyPlan" | "unknown";
  note: string;
  archived: boolean;
}

export interface BillingRule {
  id: string;
  identity: IdentityKey;
  match: "exact" | "prefix";
  billingProfileId: string;
  effectiveFrom: number;
  effectiveTo: number | null;
  mixed: boolean;
}

export interface PricingReference {
  providerId: string;
  modelId: string;
}

export interface PricingRule {
  id: string;
  identity: IdentityKey;
  match: "exact" | "prefix";
  reference: PricingReference;
}

export interface BillingConfig {
  revision: number;
  profiles: BillingProfile[];
  rules: BillingRule[];
  pricingRules: PricingRule[];
}

export interface Attribution {
  billingProfileId: string | null;
  attributionKind: AttributionKind;
  ruleIds: string[];
  reason: "unassigned" | "conflict" | "mixed" | "bucketBoundary" | null;
}

export interface IdentityUsage {
  identity: IdentityKey;
  sourceKind: UsageSourceKind;
  attribution: Attribution;
  buckets: UsageBuckets;
  metrics: UsageMetrics;
}

export interface DistributionRow {
  key: string;
  /** 兼容字段：渠道维度为显示名或 i18n 键；模型维度放原始 `modelId`。 */
  label: string;
  sourceIds: UsageSourceId[];
  billingProfileId: string | null;
  attributionKind: AttributionKind;
  buckets: UsageBuckets;
  metrics: UsageMetrics;
  /** 渠道稳定键；tool/project 维度为 null。 */
  channelKey: string | null;
  channelKind: ChannelKind | null;
  /** 展示用渠道名（原始 provider 经别名映射后的显示名）。 */
  displayProvider: string | null;
  /** 账本里的原始 provider（证据面，永不回写/覆盖）；仅证据唯一时为非空。 */
  observedProvider: string | null;
  /** 该分组内全部去重后的非空原始 provider（排序稳定）。 */
  observedProviders: string[];
  /**
   * 该分组是否混入了原始 provider 为空（未标注）的行。
   * 为 true 时 `observedProvider` 恒为 null（即使 `observedProviders` 只有一个非空值）。
   */
  hasEmptyProvider: boolean;
  /** 原始模型 ID（空串表示未知模型）；tool/project 维度为 null。 */
  modelId: string | null;
}

export interface UsageTrend {
  selection: TrendSelection;
  trend: TrendPoint[];
  hero: number;
  compare: CompareBlock | null;
  available: boolean;
  reason: string | null;
}

export interface UsageDistribution {
  groupBy: DistributionGroup;
  rows: DistributionRow[];
}

export interface BillingPreview {
  baseRevision: number;
  candidate: BillingConfig;
  matchedIdentities: IdentityUsage[];
  matchedTokens: number;
  ambiguousTokens: number;
  boundaryTokens: number;
}

export interface BillingCommit {
  expectedRevision: number;
  candidate: BillingConfig;
}

export interface BillingRevision {
  revision: number;
  committedAt: number;
}

// ── Task 6/9/10 费用与定价（Rust wire 逐字镜像；金额一律字符串）──────────────
export type CostStatus = "priced" | "partial" | "unpriced";
export type PricingBasis = "baseCurrent" | "insufficient" | "userOverride";

/** 覆盖价币种：按所选币种存储，显示另一币种时按当日汇率换算。 */
export type PriceCurrency = "USD" | "CNY";

/** 用户手动覆盖价的请求：四价均可空，`null` = 不覆盖该分量；空字符串非法。 */
export interface PriceOverrideRequest {
  /** 用户输入的原始/归一化模型 ID，后端再归一化。 */
  normalizedModelId: string;
  currency: PriceCurrency;
  input: string | null;
  cacheRead: string | null;
  cacheWrite: string | null;
  output: string | null;
}

/** 用户手动覆盖价的视图：目录来源与快照日期（无目录时为 null）。 */
export interface PriceOverrideView extends PriceOverrideRequest {
  updatedAt: number;
  sourceProvider: string | null;
  sourceModel: string | null;
  releaseDate: string | null;
  snapshotDate: number | null;
}

export interface CostComponent {
  tokens: number;
  /** USD 单价/金额（目录原生 USD，或 CNY 覆盖价按当日汇率换算）。 */
  unitUsdPerMillion: string | null;
  estimatedUsd: string | null;
  /** CNY 单价/金额（原生人民币价，或 USD 来源按当日汇率换算）。 */
  unitCnyPerMillion: string | null;
  estimatedCny: string | null;
  /** 该分量的 USD/CNY 值是否由另一币种按当日汇率换算而来（展示虚线）。 */
  usdConverted: boolean;
  cnyConverted: boolean;
}

export interface CostRow {
  key: string;
  identity: IdentityKey;
  sourceIds: UsageSourceId[];
  billingProfileId: string | null;
  attributionKind: AttributionKind;
  tokenUsage: number;
  cacheHitRate: number | null;
  estimatedUsd: string | null;
  estimatedCny: string | null;
  /** 行级金额是否含汇率换算分量（展示虚线下划线）。 */
  usdConverted: boolean;
  cnyConverted: boolean;
  status: CostStatus;
  pricedTokens: number;
  unpricedTokens: number;
  coverageRate: number | null;
  /** CNY 口径独立覆盖率（原生人民币价与 USD 目录价可用分量可能不同）。 */
  statusCny: CostStatus;
  pricedTokensCny: number;
  unpricedTokensCny: number;
  coverageRateCny: number | null;
  pricingReference: PricingReference | null;
  pricingSnapshotId: string | null;
  pricingBasis: PricingBasis;
  priceUpdatedAt: number | null;
  reasons: string[];
  components: {
    input: CostComponent | null;
    cacheRead: CostComponent | null;
    cacheWrite: CostComponent | null;
    output: CostComponent | null;
  };
  /** 显示渠道稳定键（展示分类，不得回写账本）。 */
  channelKey: string;
  channelKind: ChannelKind;
  displayProvider: string | null;
  /** 该费用行内全部去重后的非空原始 provider（排序稳定）。 */
  observedProviders: string[];
  /** 该行是否混入了原始 provider 为空（未标注）的行；为 true 时 identity.observedProvider 恒为 null。 */
  hasEmptyProvider: boolean;
  /** 自动匹配所用的归一化模型 ID（未匹配时为 null）。 */
  normalizedModelId: string | null;
  /** 选中目录条目的 releaseDate（来自 models.dev 快照）。 */
  pricingReleaseDate: string | null;
  /** 该行至少一个计费分量使用了用户覆盖价。 */
  priceOverride: boolean;
  /** 行成员一致配置的自定义模型名；无人配置或成员分歧时为 null（前端回退原始模型名）。 */
  displayModelName: string | null;
}

export interface PricingStatus {
  enabled: boolean;
  snapshotId: string | null;
  fetchedAt: number | null;
  stale: boolean;
  refreshing: boolean;
  lastError: string | null;
  /** 与 active 快照一起保存的当日 USD→CNY 汇率（无快照/未取到 ⇒ null）。 */
  fxRate: string | null;
  fxFetchedAt: number | null;
  /** `"er-api"` | `"cached"` | `"settings"` | `"default"`。 */
  fxSource: string | null;
  /** true = 当前汇率不是随 active 快照取到的。 */
  fxFallback: boolean;
}

/** 定价目录搜索结果（参考 API 模型下拉自动填充）。 */
export interface PricingCatalogEntry {
  providerId: string;
  modelId: string;
  /** models.dev 的 `release_date` 原文（可能缺失）。 */
  releaseDate: string | null;
  /** 至少一个费率存在（缺价 ≠ 0 价）。 */
  hasRates: boolean;
  /** 存在无法确认的计价条件（未知 cost 键 / 非纯文本模态）。 */
  conditional: boolean;
  /** 精选来源分类："official" / "hosted" / "openrouter" / null（非精选，仅 scope=all 出现）。 */
  curatedKind: string | null;
}

/** 目录搜索结果（带精选作用域与来源计数；providerCount 恒为全量目录 provider 数）。 */
export interface PricingCatalogSearch {
  entries: PricingCatalogEntry[];
  providerCount: number;
}

/** 目录条目价格预览（row3 悬停价格表）；费率串为 10^-9 定点十进制文本。 */
export interface PricingQuotePreview {
  reference: PricingReference;
  /** USD 四价（目录原生，或 CNY 覆盖价按当日汇率换算）。 */
  input: string | null;
  cacheRead: string | null;
  cacheWrite: string | null;
  output: string | null;
  /** CNY 四价（原生人民币价，或 USD 来源按当日汇率换算）。 */
  inputCny: string | null;
  cacheReadCny: string | null;
  cacheWriteCny: string | null;
  outputCny: string | null;
  cnyConverted: boolean;
  releaseDate: string | null;
  conditional: boolean;
  /** = active 快照的 fetchedAt。 */
  snapshotDate: number | null;
  fxRate: string | null;
  fxFetchedAt: number | null;
  fxFallback: boolean;
}

/** 自动匹配结果（固定精选作用域：官方优先、OpenRouter 兜底）。 */
export interface PricingAutoMatch {
  kind: "exact" | "prefix" | "none";
  reference: PricingReference | null;
  quote: PricingQuotePreview | null;
}

// ── 模型条目配置（2026-10-20 计划 §2.2/§2.3）─────────────────────────────────

/** 观察条目（A 段与 B 段的共同数据源；全历史，来自 usage_daily，与查询范围无关）。 */
export interface ObservedModelEntry {
  /** null = 空 provider 条目（此时 sourceId 必有）。 */
  providerRaw: string | null;
  /** 仅空 provider 条目非空。 */
  sourceId: UsageSourceId | null;
  /** 空串 = 未知模型。 */
  modelId: string;
  /** 后端机械归一化后的模型 ID（`GLM-5.3` → `glm-5-3`；空模型为空串）。 */
  normalizedModelId: string;
  /** 别名解析结果；null = 未指定渠道（空 provider 未指派）。 */
  displayProvider: string | null;
  /** 该条目已保存的自定义模型名。 */
  displayModelName: string | null;
  /** 计价身份覆盖（成对）。 */
  pricingProvider: string | null;
  pricingModel: string | null;
  /** 全历史 requestTotal + output，用于排序。 */
  totalTokens: number;
  /** 最近 7 天（宿主本地日界，含今日）requestTotal + output，用于条目 Top10。 */
  recentTokens7d: number;
}

/** 设置自定义模型显示名的请求；displayName 为 null = 清除（回原名）。 */
export interface ModelDisplayNameRequest {
  providerRaw: string | null;
  sourceId: UsageSourceId | null;
  modelId: string;
  displayName: string | null;
}

/** 设置计价身份覆盖的请求；reference 为 null = 清除覆盖（回自动匹配）。 */
export interface PricingIdentityRequest {
  providerRaw: string | null;
  sourceId: UsageSourceId | null;
  modelId: string;
  reference: PricingReference | null;
}

export interface UsageCosts {
  range: UsageRange;
  rows: CostRow[];
  pricing: PricingStatus;
  /** 本次行金额所用的当日 USD→CNY 汇率（含 cached/default/settings 回退）。 */
  fxRate: string | null;
  fxFetchedAt: number | null;
  fxSource: string | null;
  fxFallback: boolean;
}

// UsageRange 供后续 V2 查询 IPC 复用；此处只转出既有类型，不重复定义。
export type { UsageRange };
