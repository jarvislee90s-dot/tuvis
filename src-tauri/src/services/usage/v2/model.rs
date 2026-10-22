//! 用量看板 V2 的身份与 Token wire 类型（冻结 TS 契约的 Rust 镜像）。
//!
//! 约定：
//! * 结构体与字符串枚举统一 `serde(rename_all = "camelCase")`；
//! * `TrendSelection` 使用内部 tag `scope`；
//! * 复用旧 `usage::model` 的 Token 值对象，**不重复定义**；
//! * `Option` 一律**显式序列化 null**（不加 `skip_serializing_if`）。
//! * 费用/定价相关类型随 Task 1 契约先行；查询/定价逻辑由后续任务实现。

use serde::{Deserialize, Serialize};

// 复用旧 Token / 时间 / 来源类型；`UsageSourceKind` 是 TS 契约名，Rust 旧名是 `SourceKind`。
pub use crate::services::usage::model::SourceKind as UsageSourceKind;
pub use crate::services::usage::model::{
    CompareBlock, SourceKind, TrendPoint, UsageBuckets, UsageMetrics, UsageRange, UsageSourceId,
};

/// 归因可信度（与旧 `sourceKind` 的“供应商可得性”分开，不混用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AttributionKind {
    Observed,
    UserAssigned,
    Unknown,
    Ambiguous,
}

/// 分布分组维度（渠道是 V2 新增维度）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DistributionGroup {
    BillingProfile,
    Tool,
    Project,
    Model,
}

/// 显示渠道的类型：与账本原始身份分离，仅用于展示/筛选分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ChannelKind {
    /// 显式 `billingProfile` 绑定（优先级最高）。
    BillingProfile,
    /// 命中 provider 别名，或空 provider 的 `toolModel` 指派。
    Alias,
    /// 未配置别名的非空原始 provider。
    Provider,
    /// 空 provider 未指派（渠道维度全局一行）。
    Unassigned,
    /// 规则冲突/混合，不能声称是某个渠道。
    Ambiguous,
}

/// 供应商别名的两种作用域。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProviderAliasKind {
    /// 非空原始 provider，精确映射（跨工具生效）。
    Provider,
    /// 空 provider 的「工具 + 原始模型」逐条指派。
    ToolModel,
}

/// 一条供应商别名规则。
///
/// * `kind = provider`：`provider` 必填，`source_id`/`model_id` 为 null；
/// * `kind = toolModel`：`source_id`/`model_id` 必填（`model_id` 空串表示未知模型），
///   `provider` 为 null。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderAliasRule {
    pub id: String,
    pub kind: ProviderAliasKind,
    pub provider: Option<String>,
    pub source_id: Option<UsageSourceId>,
    pub model_id: Option<String>,
    pub display_name: String,
}

/// 供应商别名的完整配置（与计费配置分离，不参与 revision）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderAliasConfig {
    pub aliases: Vec<ProviderAliasRule>,
}

/// 趋势筛选的渠道选项（只含可筛选的渠道类型）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelOption {
    /// `bp:<profileId>` 或 `provider:<displayName>`。
    pub key: String,
    pub label: String,
    pub kind: ChannelKind,
    pub billing_profile_id: Option<String>,
    pub token_usage: i64,
}

/// 覆盖价的计价币种（2026-10-20 用户裁决）：按所选币种存储，显示另一币种时按当日汇率换算。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PriceCurrency {
    #[default]
    #[serde(rename = "USD")]
    Usd,
    #[serde(rename = "CNY")]
    Cny,
}

impl PriceCurrency {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Usd => "USD",
            Self::Cny => "CNY",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "USD" => Some(Self::Usd),
            "CNY" => Some(Self::Cny),
            _ => None,
        }
    }
}

/// 分时价格使用的时区口径（2026-10-21 用户裁决：固定预设，不引入完整时区库）。
///
/// `bucket_key` 由采集侧按**宿主本地**生成；非 `local` 预设会先把宿主本地时间
/// 换算到目标时区，再判断星期与时段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum TimePriceTimezone {
    /// 系统本地：直接按 `hour_key` 解释，不做换算。
    Local,
    /// 中国标准时间（UTC+8）。
    #[default]
    AsiaShanghai,
    /// UTC。
    Utc,
}

impl TimePriceTimezone {
    pub const ALL: [Self; 3] = [Self::AsiaShanghai, Self::Utc, Self::Local];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::AsiaShanghai => "asiaShanghai",
            Self::Utc => "utc",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|item| item.as_str() == value)
    }
}

/// 高峰窗口（整点边界；结束时间不包含在窗口内）。
///
/// `start > end` 表示跨午夜（例如 `22:00`–`02:00`）。星期固定为周一至周五，
/// 不处理法定节假日；跨午夜窗口的下一天凌晨仍属于前一天的高峰。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimePriceWindow {
    pub start: String,
    pub end: String,
}

/// 分时价格规则（高峰四价沿用 `PriceOverrideRequest` 的 input/cache_read/...，
/// 这里只存时区、窗口与低谷四价）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimePriceRule {
    #[serde(default)]
    pub timezone: TimePriceTimezone,
    pub windows: Vec<TimePriceWindow>,
    #[serde(default)]
    pub offpeak_input: Option<String>,
    #[serde(default)]
    pub offpeak_cache_read: Option<String>,
    #[serde(default)]
    pub offpeak_cache_write: Option<String>,
    #[serde(default)]
    pub offpeak_output: Option<String>,
}

/// 用户手动覆盖价的请求：四价均可空，`None` = 不覆盖该分量；空字符串非法。
///
/// `time_rule` 为空 = 固定价；非空 = 分时价，此时四价为**高峰**价格，
/// 低谷价格由 `time_rule.offpeak_*` 给出。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PriceOverrideRequest {
    /// 用户输入的原始/归一化模型 ID，后端再归一化。
    pub normalized_model_id: String,
    /// 四价的计价币种；缺省（旧客户端/旧夹具）按 USD。
    #[serde(default)]
    pub currency: PriceCurrency,
    pub input: Option<String>,
    pub cache_read: Option<String>,
    pub cache_write: Option<String>,
    pub output: Option<String>,
    /// 为空 = 固定价；非空 = 分时价。
    #[serde(default)]
    pub time_rule: Option<TimePriceRule>,
}

/// 用户手动覆盖价的视图：包含目录来源与快照日期（无目录时为 null）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PriceOverrideView {
    pub normalized_model_id: String,
    pub currency: PriceCurrency,
    pub input: Option<String>,
    pub cache_read: Option<String>,
    pub cache_write: Option<String>,
    pub output: Option<String>,
    /// 分时规则；`None` = 固定价。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_rule: Option<TimePriceRule>,
    pub updated_at: i64,
    pub source_provider: Option<String>,
    pub source_model: Option<String>,
    pub release_date: Option<String>,
    pub snapshot_date: Option<i64>,
}

/// 趋势局部筛选：单次只激活一个维度；`all` 不改变全局总览。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "camelCase")]
pub enum TrendSelection {
    All,
    Tool {
        #[serde(rename = "selectedId")]
        selected_id: UsageSourceId,
    },
    BillingProfile {
        #[serde(rename = "selectedId")]
        selected_id: String,
    },
    Unknown {
        #[serde(rename = "selectedId")]
        selected_id: UsageSourceId,
    },
}

/// 稳定匹配键：来源工具 + 既有账本供应商（null 与空串严格区分）+ 完整模型 ID。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IdentityKey {
    pub source_id: UsageSourceId,
    /// `null` 与非空、空串是三种不同身份；不得在适配层之外合并。
    pub observed_provider: Option<String>,
    pub model_id: String,
}

/// 用户维护的实际订阅/API 计费渠道（系统生成不可变 ID）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BillingProfile {
    pub id: String,
    pub name: String,
    pub kind: BillingProfileKind,
    pub note: String,
    pub archived: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BillingProfileKind {
    OfficialSubscription,
    Api,
    ThirdPartyPlan,
    Unknown,
}

/// 匹配方式：默认精确；前缀必须由用户显式开启。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleMatch {
    Exact,
    Prefix,
}

/// 身份 → 计费渠道规则。时间区间为 `[effectiveFrom, effectiveTo)`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BillingRule {
    pub id: String,
    pub identity: IdentityKey,
    #[serde(rename = "match")]
    pub match_kind: RuleMatch,
    pub billing_profile_id: String,
    pub effective_from: i64,
    /// `None` = 无上界；必须显式序列化 null。
    pub effective_to: Option<i64>,
    /// 相同日志身份可能混合多个真实账户，不能据此精确拆账。
    pub mixed: bool,
}

/// 参考 API 报价键（与实际订阅分离）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PricingReference {
    pub provider_id: String,
    pub model_id: String,
}

/// 身份 → 参考报价规则（PricingRule 与渠道归属规则分离）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PricingRule {
    pub id: String,
    pub identity: IdentityKey,
    #[serde(rename = "match")]
    pub match_kind: RuleMatch,
    pub reference: PricingReference,
}

/// 带版本的完整计费配置快照。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BillingConfig {
    pub revision: i64,
    pub profiles: Vec<BillingProfile>,
    pub rules: Vec<BillingRule>,
    pub pricing_rules: Vec<PricingRule>,
}

/// 归因结果原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AttributionReason {
    Unassigned,
    Conflict,
    Mixed,
    BucketBoundary,
}

/// 单条记录/单个时间段的归因结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Attribution {
    /// `None` = 没有可靠渠道；必须显式序列化 null。
    pub billing_profile_id: Option<String>,
    pub attribution_kind: AttributionKind,
    pub rule_ids: Vec<String>,
    /// `None` = 已唯一归属；必须显式序列化 null。
    pub reason: Option<AttributionReason>,
}

/// 记录页身份行：完整身份 + 旧供应商可得性 + 归因 + Token。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IdentityUsage {
    pub identity: IdentityKey,
    pub source_kind: SourceKind,
    pub attribution: Attribution,
    pub buckets: UsageBuckets,
    pub metrics: UsageMetrics,
}

/// 分布表的一行。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DistributionRow {
    pub key: String,
    /// 兼容字段：渠道维度为显示名或 i18n 键；模型维度放原始 `model_id`。
    pub label: String,
    pub source_ids: Vec<UsageSourceId>,
    pub billing_profile_id: Option<String>,
    pub attribution_kind: AttributionKind,
    pub buckets: UsageBuckets,
    pub metrics: UsageMetrics,
    /// 渠道稳定键（`bp:<id>` / `provider:<displayName>` / `unassigned` / `ambiguous`）；
    /// tool/project 维度为 null。
    pub channel_key: Option<String>,
    pub channel_kind: Option<ChannelKind>,
    /// 展示用渠道名（原始 provider 经别名映射后的显示名）。
    pub display_provider: Option<String>,
    /// 账本里的原始 provider（证据面，永不回写/覆盖）。
    ///
    /// 仅在**该组恰好一个非空原始 provider 且不包含任何空 provider 行**时为 `Some`；
    /// 同显示名合并多个原始串（或混入空 provider）时为 `None`，完整证据见
    /// [`Self::observed_providers`]。
    pub observed_provider: Option<String>,
    /// 该分组内全部去重后的非空原始 provider（排序稳定；tool/project 维度同样保留证据）。
    pub observed_providers: Vec<String>,
    /// 该分组是否混入了原始 provider 为空（未标注）的行。
    ///
    /// 为 `true` 时 [`Self::observed_provider`] 恒为 `None`，即使
    /// `observed_providers` 恰好只有一个非空值；展示层应在工具提示中提示证据不完整。
    pub has_empty_provider: bool,
    /// 原始模型 ID（空串表示未知模型）；tool/project 维度为 null。
    pub model_id: Option<String>,
}

/// 带局部筛选的趋势结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageTrend {
    pub selection: TrendSelection,
    pub trend: Vec<TrendPoint>,
    pub hero: i64,
    pub compare: Option<CompareBlock>,
    pub available: bool,
    pub reason: Option<String>,
}

/// 分布结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageDistribution {
    pub group_by: DistributionGroup,
    pub rows: Vec<DistributionRow>,
}

/// 保存前预览（不写库）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BillingPreview {
    pub base_revision: i64,
    pub candidate: BillingConfig,
    pub matched_identities: Vec<IdentityUsage>,
    pub matched_tokens: i64,
    pub ambiguous_tokens: i64,
    pub boundary_tokens: i64,
}

/// 提交请求（乐观并发）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BillingCommit {
    pub expected_revision: i64,
    pub candidate: BillingConfig,
}

/// 已提交的配置版本。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BillingRevision {
    pub revision: i64,
    pub committed_at: i64,
}

// ── 费用 / 定价公共类型（Task 6；金额一律字符串，不引入 decimal 依赖）──────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CostStatus {
    Priced,
    Partial,
    Unpriced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PricingBasis {
    BaseCurrent,
    Insufficient,
    /// 至少一个计费分量来自用户手动覆盖价。
    UserOverride,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CostComponent {
    pub tokens: i64,
    /// USD 单价与金额（原生 USD 来源，或 CNY 覆盖价按当日汇率换算；见 `usd_converted`）。
    pub unit_usd_per_million: Option<String>,
    pub estimated_usd: Option<String>,
    /// CNY 单价与金额（原生人民币来源，或 USD 来源按当日汇率换算；见 `cny_converted`）。
    pub unit_cny_per_million: Option<String>,
    pub estimated_cny: Option<String>,
    /// 该分量的 USD/CNY 值是否由另一种币种按当日汇率换算而来（前端加虚线下划线）。
    pub usd_converted: bool,
    pub cny_converted: bool,
}

/// 四个费用组件**按固定顺序** input / cacheRead / cacheWrite / output。
/// `None` = 该部分不可安全计价（**不是 0 价**）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CostComponents {
    pub input: Option<CostComponent>,
    pub cache_read: Option<CostComponent>,
    pub cache_write: Option<CostComponent>,
    pub output: Option<CostComponent>,
}

/// 费用行的价格规则形态（CSV / UI 标记）：无自定义规则、固定价、分时价。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PriceRuleKind {
    None,
    Fixed,
    Time,
}

impl PriceRuleKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Fixed => "fixed",
            Self::Time => "time",
        }
    }
}

/// 分时价格行的 Token 分布（高峰 / 低谷 / 日聚合回退）。
///
/// 日聚合回退时无法按小时拆分，整段按低谷价估算，单独计入 `daily_fallback_tokens`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimePriceBreakdown {
    pub peak_tokens: i64,
    pub offpeak_tokens: i64,
    pub daily_fallback_tokens: i64,
}

/// 费用查询的一行。`token_usage` 是规范 Token 总量（旧口径 hero 的组成部分），
/// 不是四桶结构；`Option` 全部显式序列化为 null。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CostRow {
    pub key: String,
    pub identity: IdentityKey,
    pub source_ids: Vec<UsageSourceId>,
    pub billing_profile_id: Option<String>,
    pub attribution_kind: AttributionKind,
    pub token_usage: i64,
    pub cache_hit_rate: Option<f64>,
    pub estimated_usd: Option<String>,
    /// 行级 CNY 金额（原生人民币价，或 USD 价按当日汇率换算；见 `cny_converted`）。
    pub estimated_cny: Option<String>,
    /// 行级 USD/CNY 金额是否含汇率换算而来的分量（前端加虚线下划线）。
    pub usd_converted: bool,
    pub cny_converted: bool,
    pub status: CostStatus,
    pub priced_tokens: i64,
    pub unpriced_tokens: i64,
    pub coverage_rate: Option<f64>,
    /// CNY 口径的独立覆盖率（原生人民币价与 USD 目录价的可用分量可能不同）。
    pub status_cny: CostStatus,
    pub priced_tokens_cny: i64,
    pub unpriced_tokens_cny: i64,
    pub coverage_rate_cny: Option<f64>,
    pub pricing_reference: Option<PricingReference>,
    pub pricing_snapshot_id: Option<String>,
    pub pricing_basis: PricingBasis,
    pub price_updated_at: Option<i64>,
    pub reasons: Vec<String>,
    pub components: CostComponents,
    /// 显示渠道稳定键（展示分类，**不得回写账本**）。
    pub channel_key: String,
    pub channel_kind: ChannelKind,
    pub display_provider: Option<String>,
    /// 该费用行内全部去重后的非空原始 provider（排序稳定）。
    pub observed_providers: Vec<String>,
    /// 该行是否混入了原始 provider 为空（未标注）的行。
    ///
    /// 为 `true` 时 [`Self::identity`] 的 `observed_provider` 恒为 `None`，
    /// 即使 `observed_providers` 恰好只有一个非空值；展示层应提示证据不完整。
    pub has_empty_provider: bool,
    /// 自动匹配所用的归一化模型 ID（未匹配时为 null）。
    pub normalized_model_id: Option<String>,
    /// 选中目录条目的 release_date（来自 models.dev 快照）。
    pub pricing_release_date: Option<String>,
    /// 该行至少一个计费分量使用了用户覆盖价。
    pub price_override: bool,
    /// 覆盖规则形态：无 / 固定 / 分时。
    pub price_rule_kind: PriceRuleKind,
    /// 分时价格行的 Token 分布；固定价或未匹配时为 null。
    pub time_breakdown: Option<TimePriceBreakdown>,
    /// 该行成员在 `usage_model_config` 里一致配置的自定义模型名；
    /// 无人配置或成员分歧（>1 个不同自定义名）时为 null（前端回退原始模型名）。
    pub display_model_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PricingStatus {
    pub enabled: bool,
    pub snapshot_id: Option<String>,
    pub fetched_at: Option<i64>,
    pub stale: bool,
    pub refreshing: bool,
    pub last_error: Option<String>,
    /// 与 active 快照一起保存的当日 USD→CNY 汇率（无快照/未取到 ⇒ null）。
    pub fx_rate: Option<String>,
    pub fx_fetched_at: Option<i64>,
    /// 汇率来源：`"er-api"`（随快照取到）/ `"cached"`（沿用上次成功）/
    /// `"settings"`（设置页备用）/ `"default"`（内置 7.2 兜底）。与 `FxRate::source` 四值一致。
    pub fx_source: Option<String>,
    /// `true` = 当前汇率不是随 active 快照取到的（展示层需标注）。
    pub fx_fallback: bool,
}

/// 定价目录搜索结果（参考 API 模型下拉自动填充）。
///
/// 只暴露选择所需的展示信息；具体四价仍由费用查询按归一化模型解析。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PricingCatalogEntry {
    pub provider_id: String,
    pub model_id: String,
    /// models.dev 的 `release_date` 原文（可能缺失）。
    pub release_date: Option<String>,
    /// 至少一个费率存在（缺价 ≠ 0 价）。
    pub has_rates: bool,
    /// 存在无法确认的计价条件（未知 cost 键 / 非纯文本模态）。
    pub conditional: bool,
    /// 精选来源分类：`"official"`（直营官方，含中国站变体）/ `"hosted"`（Ollama Cloud）/
    /// `"openrouter"` / `null`（非精选；只有 `scope=all` 的目录搜索才会出现）。
    pub curated_kind: Option<String>,
}

/// 目录搜索结果（带精选作用域与来源计数）。
///
/// `provider_count` 恒为**全量目录** provider 数（「显示全部 N 个来源」的 N），
/// 与当前搜索的 scope 无关。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PricingCatalogSearch {
    pub entries: Vec<PricingCatalogEntry>,
    pub provider_count: i64,
}

/// 目录条目价格预览（B 段 row3 悬停价格表）。费率串复用 rate_string（10^-9 定点十进制）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PricingQuotePreview {
    pub reference: PricingReference,
    /// USD 四价（数据源原生 USD，或 CNY 覆盖价按当日汇率换算）。
    pub input: Option<String>,
    pub cache_read: Option<String>,
    pub cache_write: Option<String>,
    pub output: Option<String>,
    /// CNY 四价（原生人民币来源，或 USD 来源按当日汇率换算；见 `cny_converted`）。
    pub input_cny: Option<String>,
    pub cache_read_cny: Option<String>,
    pub cache_write_cny: Option<String>,
    pub output_cny: Option<String>,
    pub cny_converted: bool,
    pub release_date: Option<String>,
    pub conditional: bool,
    /// = active 快照的 fetched_at。
    pub snapshot_date: Option<i64>,
    /// 预览所用当日汇率（USD→CNY）；无则 null。
    pub fx_rate: Option<String>,
    pub fx_fetched_at: Option<i64>,
    pub fx_fallback: bool,
}

/// 自动匹配结果（固定精选作用域：官方优先、OpenRouter 兜底）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PricingAutoMatch {
    /// `"exact"` | `"prefix"` | `"none"`。
    pub kind: String,
    pub reference: Option<PricingReference>,
    pub quote: Option<PricingQuotePreview>,
}

// ── 模型条目配置（2026-10-20 计划 §2.2）──────────────────────────────────────

/// 观察条目（A 段与 B 段的共同数据源；全历史，来自 usage_daily，与查询范围无关）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservedModelEntry {
    /// `None` = 空 provider 条目（此时 `source_id` 必有）。
    pub provider_raw: Option<String>,
    /// 仅空 provider 条目非空。
    pub source_id: Option<UsageSourceId>,
    /// 空串 = 未知模型。
    pub model_id: String,
    /// 后端机械归一化后的模型 ID（`GLM-5.3` → `glm-5-3`）。
    /// **归一化后为空的模型（含空模型）一律空串**；前端按空串禁用覆盖入口。
    /// 供配置条目快捷覆盖价预填/已覆盖徽标使用，前端不重复实现归一化。
    pub normalized_model_id: String,
    /// 别名解析结果；`None` = 未指定渠道（空 provider 未指派）。
    pub display_provider: Option<String>,
    /// 该条目已保存的自定义模型名。
    pub display_model_name: Option<String>,
    /// 计价身份覆盖（成对；单侧为 null 视为未覆盖）。
    pub pricing_provider: Option<String>,
    pub pricing_model: Option<String>,
    /// 全历史 requestTotal + output，用于排序。
    pub total_tokens: i64,
    /// 最近 7 天（宿主本地日界，含今日）的 requestTotal + output，用于条目 Top10。
    pub recent_tokens_7d: i64,
}

/// 设置自定义模型显示名的请求；`display_name` 为 null = 清除（回原名）；
/// trim 后空白串 = 拒绝（报 invalid 错误，不是清除）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelDisplayNameRequest {
    pub provider_raw: Option<String>,
    pub source_id: Option<UsageSourceId>,
    pub model_id: String,
    pub display_name: Option<String>,
}

/// 设置计价身份覆盖的请求；`reference` 为 null = 清除覆盖（回自动匹配）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PricingIdentityRequest {
    pub provider_raw: Option<String>,
    pub source_id: Option<UsageSourceId>,
    pub model_id: String,
    pub reference: Option<PricingReference>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageCosts {
    pub range: UsageRange,
    pub rows: Vec<CostRow>,
    pub pricing: PricingStatus,
    /// 本次行金额所用的当日 USD→CNY 汇率（含 cached/default 回退）。
    pub fx_rate: Option<String>,
    pub fx_fetched_at: Option<i64>,
    pub fx_source: Option<String>,
    pub fx_fallback: bool,
}
// ==== usage 自省锁：以下为排除区（测试代码），勿删勿复制 ====
// ==== usage 自省锁：排除区结束，勿删勿复制 ====
