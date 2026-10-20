# 用量看板 V2 增量执行计划：Provider 别名与定价自动匹配

> 日期：2026-10-19 · 状态：待执行（只写计划，未改实现）
> 唯一产品规格：`docs/superpowers/specs/2026-10-17-usage-dashboard-v2-design.md`，尤其是 §7.2 与 §2.3/§2.4/§2.5/§2.7/§3.1/§3.2/§4.1/§4.2/§6。
> 既有执行计划仅作格式与历史参考：`docs/superpowers/plans/2026-10-17-usage-dashboard-v2-plan.md`。**本计划不修改它**，两者冲突时以本计划 + Spec §7.2 为准。
> 代码根：`E:\LLMproject\Github\mam-worktree-usage-v2`；基线分支 `usage-dashboard-v2`，基线提交 `455c240b`（spec 修订）。实施前先复核 HEAD 与工作区。
> 最高约束：`docs/MASTER-PLAN.md`。本计划不得修改宪法；发现冲突立即停下报告用户。

---

## 0. 冻结口径（不得自行更改）

1. **渠道维度 = provider 显示名**。非空 provider 按原始串精确映射到用户显示名；不同原始串可映射到同一显示名并跨工具合并。空 provider 不设自动规则，用户对「工具 + 原始模型」逐条指派，保存后回溯生效。显式 `billingProfileId` 绑定是覆盖层，优先级最高。工具不参与渠道分类。
2. **Token 统计与 API 换算分离**。统计键 = 显示名/原始 provider + 原始 `modelId`；API 换算按归一化模型 ID 匹配 models.dev。无别名时非空 provider 按原始串聚合；空 provider 未指派时渠道合并为一行“未指定渠道”，模型维度暂按工具拆行；空 model 显示“未知模型”。
3. **定价照抄 CC-switch 外部数据模型**。只使用 models.dev，不内置种子价；默认每日刷新 + 手动刷新；失败沿用最后成功快照并展示数据源/快照日期；同一归一化模型全渠道同价（models.dev 同名模型按 `release_date` 最新优先去重）；额外保存来源 provider 与快照日期；允许手动覆盖任意模型四价，后续同步不得覆盖用户值。
4. **匹配边界**。模型 ID 机械归一化后精确匹配；前缀回退仅“唯一且同价”；跨模型族替代仍需用户授权；保守估价（partial/unpriced）规则不变。
5. **费用行**。按「显示名/模型」拆分；不同 provider 分行但共用同一归一化模型单价；展开行/设置页展示价格来源与快照日期。
6. **配置入口**。设置页集中管理别名（显示该 provider/条目被哪些工具与模型使用）；记录页浮窗可直达编辑。
7. **本次不改** Collector、游标、Token 口径、历史账本、`docs/MASTER-PLAN.md`；不新增错误码；i18n zh/en 同步；迁移幂等。

**本期不做**：全局/渠道费用小计、费用总趋势、真实账单、订阅额度抓取、按 Token 比例分摊、跨模型族自动同价、改写账本 `provider`/`model`。

---

## 1. 设计决策与权衡

| 决策 | 选择 | 权衡 / 理由 |
| --- | --- | --- |
| 别名存储 | 新增独立表 `usage_provider_alias`，不塞进 `BillingConfig` | 别名只影响展示/分组，不是付款归因；独立表避免把渠道版本/撤销语义强加到展示层。代价：别名没有历史版本与撤销；改名是覆盖式，可再次编辑修正。 |
| 别名生效时间 | 无生效日期，查询时全量回溯 | 符合“保存后回溯生效”。代价：无法按时间区分同一原始串的两个订阅；Spec 已把该情形列为已知限制。 |
| 渠道稳定键 | `bp:<profileId>` / `provider:<displayName>` | 显式绑定与 provider 显示名分离；同显示名跨工具合并。代价：用户改名会改变 `provider:` 键，趋势选中项失效时需回退“全部”。 |
| 空 provider 指派 | `sourceId + modelId` 精确键，`modelId` 可为空串 | 允许“该工具的所有空模型”也被指派；不提供全局空 provider 规则。代价：逐条指派成本高，UI 需给快捷入口。 |
| 价格目录结构 | 保留 `PriceCatalog.quotes`（精确键）+ 新增内存归一化索引 `normalized` | 用户授权参考模型仍走精确键；自动匹配只走归一化索引。快照 payload 已保存来源 provider，`fetched_at` 已保存快照日期，不新增 quote 表。代价：每次进程冷启动首次触达目录需重建 9000+ 模型索引（现有进程级缓存已覆盖）。 |
| 同名模型去重 | 按 `release_date` 最新优先；同日期时“有价 > 无价、非条件 > 条件”，再按 provider/model 字典序 | 严格遵循“最新优先”，同时避免同日期重复条目把有价条目挤掉。代价：若唯一的最新条目本身无价/条件价，仍按保守规则未估价。 |
| 手动覆盖 | 独立表 `usage_price_override`，以归一化模型 ID 为主键，四价逐项可空 | 同一归一化模型全渠道同价；刷新只写快照表，绝不碰覆盖表。代价：不能按 provider 给同一模型不同覆盖价；这是“全渠道同价”的直接结果。 |
| 前缀回退 | 仅自动匹配使用；要求候选唯一或全部同价，且剩余后缀属于已知噪声词 | 防止 `codex-max` 误配 `codex`；不放松跨模型族。代价：少部分可估模型会保持未估价，符合保守原则。 |
| 趋势渠道筛选 | 沿用 `TrendSelection::billingProfile` 变体，`selectedId` 改为渠道键 | 避免新增 wire 变体与旧前端分支。代价：字段名仍叫 `billingProfile`，必须在类型注释中明确“渠道键”语义；`unknown` 变体保留但不再作为 UI 选项。 |
| 费用 CSV | 在既有列后**追加**渠道/来源/覆盖列 | 不破坏既有 CSV 解析器；没有全局合计行。代价：列数变化需要同步 mock 与测试。 |
| 错误处理 | 不新增错误码；别名/覆盖校验失败用 `usage-settings-invalid` | 符合冻结口径；前端继续走 `usage.rpc.<code>` + detail。 |

---

## 2. 数据模型与接口变更

### 2.1 新增 SQLite 表

本节及 2.2–2.4 的表名、列名、类型名、命令名均为**计划建议**：实现时可调整命名以贴合现有代码风格，但必须保持行为契约、迁移幂等与调用方一致；任何偏离都要在执行记录中登记并同步所有受影响文件。

写入 `src-tauri/src/database/schema.rs` 现有 DDL 字符串；`migration::migrate` 通过 `schema::init` 幂等建表。

```sql
-- provider 别名：非空 provider 按原始串精确映射；空 provider 按「工具 + 原始模型」逐条指派。
CREATE TABLE IF NOT EXISTS usage_provider_alias (
    id           TEXT PRIMARY KEY,
    kind         TEXT NOT NULL CHECK(kind IN ('provider','toolModel')),
    provider_raw TEXT,
    source_id    TEXT,
    model_id     TEXT,
    display_name TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    CHECK (
        (kind = 'provider' AND provider_raw IS NOT NULL AND provider_raw <> ''
         AND source_id IS NULL AND model_id IS NULL)
        OR
        (kind = 'toolModel' AND provider_raw IS NULL
         AND source_id IS NOT NULL AND model_id IS NOT NULL)
    )
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_usage_provider_alias_provider
    ON usage_provider_alias(provider_raw) WHERE provider_raw IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS idx_usage_provider_alias_tool_model
    ON usage_provider_alias(source_id, model_id) WHERE provider_raw IS NULL;

-- 用户手动覆盖价：按归一化模型 ID 全局生效；四个分量可分别覆盖。
CREATE TABLE IF NOT EXISTS usage_price_override (
    normalized_model_id TEXT PRIMARY KEY,
    input_scaled        INTEGER,
    cache_read_scaled   INTEGER,
    cache_write_scaled  INTEGER,
    output_scaled       INTEGER,
    updated_at          INTEGER NOT NULL,
    CHECK (
        input_scaled IS NOT NULL OR cache_read_scaled IS NOT NULL
        OR cache_write_scaled IS NOT NULL OR output_scaled IS NOT NULL
    ),
    CHECK (
        (input_scaled IS NULL OR input_scaled >= 0)
        AND (cache_read_scaled IS NULL OR cache_read_scaled >= 0)
        AND (cache_write_scaled IS NULL OR cache_write_scaled >= 0)
        AND (output_scaled IS NULL OR output_scaled >= 0)
    )
);
```

说明：

- `*_scaled` 单位与现有 `pricing::parse_rate` 一致：10^-9 USD / 百万 Token，避免浮点。入库前校验 scaled 在 `i64` 范围内；超出返回 `usage-settings-invalid`，不得静默截断。视图字符串用新增 `rate_string`（10^-9 定点转十进制），不要复用金额的 `usd_string`（后者是 10^-15 口径）。
- `usage_pricing_snapshot` 不新增列。来源 provider/model 与 `release_date` 由 payload + 解析结果提供；快照日期由 `fetched_at` 提供。
- 迁移幂等要求：连续 `migrate` 两次不报错、不重复建索引；旧四张账本表逐值不变；新表 `IF NOT EXISTS`。迁移测试的 DROP/存在性断言扩展到六张 V2 表（原四张 + 两张新表）。
- 不改 `usage_billing_state` / `usage_billing_revision` 的 JSON 形状；别名与覆盖价不进入 `BillingConfig` revision。

### 2.2 Rust wire 类型（`src-tauri/src/services/usage/v2/model.rs`）

新增/扩展以下类型，统一 `serde(rename_all = "camelCase")`，`Option` 显式序列化 null：

```rust
pub enum ChannelKind {
    BillingProfile, // 显式 billingProfile 绑定
    Alias,          // 命中 provider 别名或空 provider 的 toolModel 指派
    Provider,       // 未配置别名的非空原始 provider
    Unassigned,     // 空 provider 未指派（渠道维度全局一行）
    Ambiguous,      // 规则冲突/混合，不能声称是某个渠道
}

pub enum ProviderAliasKind { Provider, ToolModel }

pub struct ProviderAliasRule {
    pub id: String,
    pub kind: ProviderAliasKind,
    pub provider: Option<String>,       // kind=provider
    pub source_id: Option<UsageSourceId>, // kind=toolModel
    pub model_id: Option<String>,       // kind=toolModel；空串表示该工具的未知模型
    pub display_name: String,
}

pub struct ProviderAliasConfig { pub aliases: Vec<ProviderAliasRule> }

pub struct ChannelOption {
    pub key: String,                    // bp:<id> | provider:<displayName>
    pub label: String,
    pub kind: ChannelKind,
    pub billing_profile_id: Option<String>,
    pub token_usage: i64,
}

pub struct PriceOverrideRequest {
    pub normalized_model_id: String,    // 用户输入的原始/归一化模型 ID，后端再归一化
    pub input: Option<String>,
    pub cache_read: Option<String>,
    pub cache_write: Option<String>,
    pub output: Option<String>,
}

pub struct PriceOverrideView {
    pub normalized_model_id: String,
    pub input: Option<String>,
    pub cache_read: Option<String>,
    pub cache_write: Option<String>,
    pub output: Option<String>,
    pub updated_at: i64,
    pub source_provider: Option<String>,
    pub source_model: Option<String>,
    pub release_date: Option<String>,
    pub snapshot_date: Option<i64>,
}
```

扩展既有类型：

```rust
// DistributionRow 追加（tool/project 维度保持 None）
pub channel_key: Option<String>,
pub channel_kind: Option<ChannelKind>,
pub display_provider: Option<String>,
pub observed_provider: Option<String>,
pub model_id: Option<String>,

// CostRow 追加
pub channel_key: String,
pub channel_kind: ChannelKind,
pub display_provider: Option<String>,
pub normalized_model_id: Option<String>,
pub pricing_release_date: Option<String>,
pub price_override: bool,

// PricingBasis 追加
UserOverride, // 至少一个分量来自用户覆盖价
```

字段语义：

- `DistributionRow.label` 仍是兼容字段。渠道维度为显示名或 i18n 键；模型维度放原始 `modelId`（空串表示未知模型），前端不得再从 `label` 拆身份。
- `CostRow.identity` 保持原始身份（`observedProvider`/`modelId` 原样），`channelKey`/`channelKind`/`displayProvider` 是展示分类，**不得回写账本**。
- `pricingReference` 仍表示实际采用的参考目录键；用户授权规则与自动匹配都填这里。`pricingReleaseDate` 是选中目录条目的发布日期。
- `priceOverride=true` 表示该行至少一个计费分量使用了用户覆盖价；`pricingBasis=userOverride`。

### 2.3 TS 镜像（`src/types/usageV2.ts`）

逐字镜像 2.2，追加：

```ts
export type ChannelKind =
  | "billingProfile" | "alias" | "provider" | "unassigned" | "ambiguous";
export type ProviderAliasKind = "provider" | "toolModel";

export interface ProviderAliasRule {
  id: string;
  kind: ProviderAliasKind;
  provider: string | null;
  sourceId: UsageSourceId | null;
  modelId: string | null;
  displayName: string;
}
export interface ProviderAliasConfig { aliases: ProviderAliasRule[] }

export interface ChannelOption {
  key: string;
  label: string;
  kind: ChannelKind;
  billingProfileId: string | null;
  tokenUsage: number;
}

export interface PriceOverrideRequest {
  normalizedModelId: string;
  input: string | null;
  cacheRead: string | null;
  cacheWrite: string | null;
  output: string | null;
}
export interface PriceOverrideView extends PriceOverrideRequest {
  updatedAt: number;
  sourceProvider: string | null;
  sourceModel: string | null;
  releaseDate: string | null;
  snapshotDate: number | null;
}
```

`DistributionRow` 追加 `channelKey/channelKind/displayProvider/observedProvider/modelId`（全部可空）；`CostRow` 追加 `channelKey/channelKind/displayProvider/normalizedModelId/pricingReleaseDate/priceOverride`；`PricingBasis` 追加 `"userOverride"`。

### 2.4 新增 IPC 命令

沿用 `src-tauri/src/commands/usage_v2.rs` 的纪律：`async` + `spawn_blocking`，命令层不取 DB 锁、不触发采集；参数名即 camelCase。

| TS 包装（`src/lib/api/usageV2.ts`） | IPC 命令 | 参数 | 返回 |
| --- | --- | --- | --- |
| `usageProviderAliasesGet()` | `usage_provider_aliases_get` | 无 | `ProviderAliasConfig` |
| `usageProviderAliasUpsert(rule)` | `usage_provider_alias_upsert` | `{ rule }` | `ProviderAliasConfig` |
| `usageProviderAliasDelete(id)` | `usage_provider_alias_delete` | `{ id }` | `ProviderAliasConfig` |
| `usageChannelOptions(range)` | `usage_channel_options` | `{ range }` | `ChannelOption[]` |
| `usagePricingOverridesGet()` | `usage_pricing_overrides_get` | 无 | `PriceOverrideView[]` |
| `usagePricingOverrideSet(request)` | `usage_pricing_override_set` | `{ request }` | `PriceOverrideView[]` |
| `usagePricingOverrideDelete(normalizedModelId)` | `usage_pricing_override_delete` | `{ normalizedModelId }` | `PriceOverrideView[]` |

注册到 `src-tauri/src/lib.rs` 的 `generate_handler!`，紧接现有 12 条 V2 命令之后。服务层同时提供 `*_with_conn` 测试入口与不带 conn 的生产入口：

```rust
channel::load_with_conn(&Connection) -> Result<ProviderAliasConfig, UsageError>
channel::upsert_with_conn(&mut Connection, &ProviderAliasRule, now_ms) -> Result<ProviderAliasConfig, UsageError>
channel::delete_with_conn(&mut Connection, &str) -> Result<ProviderAliasConfig, UsageError>
query::channel_options_with_conn(&Connection, &UsageRange, now_ms) -> Result<Vec<ChannelOption>, UsageError>
pricing::overrides_with_conn(&Connection, now_ms) -> Result<Vec<PriceOverrideView>, UsageError>
pricing::set_override_with_conn(&mut Connection, &PriceOverrideRequest, now_ms) -> Result<Vec<PriceOverrideView>, UsageError>
pricing::delete_override_with_conn(&mut Connection, &str) -> Result<Vec<PriceOverrideView>, UsageError>
```

### 2.5 错误与校验细节

不新增错误码，全部使用既有 `usage-settings-invalid`（校验）或 `usage-db-failed`（SQLite/JSON）。固定 detail 前缀：

- `provider alias id is empty`
- `duplicate provider alias id: <id>`
- `provider alias display name is empty: <id>`
- `provider alias provider is empty: <id>`
- `provider alias source/model key is invalid: <id>`
- `provider alias key already exists: <key>`
- `price override normalizedModelId is empty`
- `price override must set at least one rate`
- `price override rate is invalid: <component>`

别名 upsert 语义：按 `id` 更新；若 `id` 不存在则插入；若目标键（`provider_raw` 或 `source_id+model_id`）已被另一条规则占用，返回 `provider alias key already exists`，不静默合并/覆盖。删除不存在的 id 为幂等空操作，返回当前配置。

---

## 3. 查询层改造要点

### 3.1 ProviderAliasResolver（新文件 `src-tauri/src/services/usage/v2/channel.rs`）

纯函数 + 配置服务，不读账本、不联网。核心类型：

```rust
pub(crate) struct ResolvedChannel {
    pub key: String,                 // bp:<id> | provider:<displayName> | unassigned | ambiguous
    pub kind: ChannelKind,
    pub display_provider: Option<String>,
    pub billing_profile_id: Option<String>,
}

pub(crate) struct ChannelResolver<'a> {
    profiles: HashMap<&'a str, &'a str>,             // id -> name
    by_provider: HashMap<&'a str, &'a str>,          // raw provider -> display name
    by_tool_model: HashMap<(&'a str, &'a str), &'a str>, // (tool, model) -> display name
}

pub(crate) fn resolve_channel(
    identity: &IdentityKey,
    attribution: &Attribution,
    resolver: &ChannelResolver,
) -> ResolvedChannel;
```

优先级严格按 Spec §2.4：

1. `attribution.attribution_kind == UserAssigned` 且 `billing_profile_id` 有值 → `BillingProfile`，键 `bp:<id>`，显示名取 profile 名；profile 缺失（不应发生）→ 按 `Ambiguous` 保守处理。
2. `attribution_kind == Ambiguous` → `Ambiguous`，键 `ambiguous`，显示名 null。
3. `observed_provider` 非空 → 先查 `by_provider`（原始串精确、区分大小写）；命中 `Alias`，否则 `Provider`。键均为 `provider:<displayName>`。
4. `observed_provider` 为空 → 查 `by_tool_model[(source_id, model_id)]`；命中 `Alias`，否则 `Unassigned`，键 `unassigned`。
5. 工具不进入渠道键；只有“空 provider 未指派”的模型行与费用行在行键中附加工具上下文，渠道维度仍合并为一行。

`join_key` 从 `query.rs` 提升为 `channel.rs` 的 `pub(crate)` 工具（长度前缀），`query.rs`/`cost_query.rs` 复用，禁止裸拼接。

### 3.2 趋势（`trend_with_conn`）

- `TrendSelection::BillingProfile { selected_id }` 的 `selected_id` 改为**渠道键**：`bp:<profileId>` 或 `provider:<displayName>`。
- `bp:` 选择：profile 必须存在且未归档，否则 `available=false` + `reason="usage-filter-unavailable"`；与现有行为一致。
- `provider:` 选择：直接按行解析后的渠道键比较；没有匹配行时返回空趋势但 `available=true`（选项来自当前范围，通常不会出现）。
- `Tool` 选择不变。`Unknown` 变体保留 wire 兼容，但**不再作为 UI 选项**；后端对该变体返回 `available=false` + `usage-filter-unavailable`，避免把“未指定渠道”伪装成渠道。
- 当期与上期使用同一 resolver 与同一 selection；A 保持全局，C/D 不联动。
- 选中渠道归档/改名后：前端选项里找不到该键 → 显示不可得并允许回到“全部”。

### 3.3 分布（`distribution_with_conn`）

渠道维度（`DistributionGroup::BillingProfile`，UI 文案改为“按渠道”）：

- 行键 = `channel:<channel_key>`；同一显示名跨工具合并。
- `Unassigned` 全局一行，键 `channel:unassigned`，标签 i18n `usage.label.unassignedChannel`，**不附工具**。
- `Ambiguous` 全局一行，键 `channel:ambiguous`，标签 i18n `usage.label.ambiguousChannel`；这是“不可分账”的行，不按工具拆。
- `source_ids` 去重排序；原始 `observed_provider` 保留在结构化字段与 tooltip。

模型维度（`DistributionGroup::Model`）：

- `BillingProfile/Alias/Provider`：键 = `model|channel_key|model_id`；同显示名跨工具合并；标签由前端用 `displayProvider + modelId` 组合。
- `Unassigned`：键 = `model|unassigned|source_id|model_id`，**按工具拆行**；标签前端组合 `未指定渠道 · <tool> / <model>`。
- `Ambiguous`：键 = `model|ambiguous|source_id|observed_provider|model_id`；标签前端组合 `渠道可能混合 · <原始 provider 或工具> / <model>`。
- 空 `model_id` 的键仍用空串；前端显示 `usage.label.unknownModel`。禁止用“裸模型名”跨订阅合并。
- `DistributionRow` 填 `channel_key/channel_kind/display_provider/observed_provider/model_id`；tool/project 维度这些字段为 null。
- 排序仍按 `requestTotal + output` 降序、key 升序；命中率按组重算。

### 3.4 渠道选项（`channel_options_with_conn`）

- 取当前范围账本行，逐行解析渠道，按渠道键聚合 Token。
- 只返回 `BillingProfile/Alias/Provider`；排除 `Unassigned/Ambiguous`（Spec §2.3 明确“未指定渠道不作为趋势筛选选项”）。
- 排除归档 profile；同一显示名跨工具只出一项。
- 排序：Token 降序、key 升序。返回 `ChannelOption`，供 B 趋势筛选使用。
- 总开关关闭返回空数组；筛选不可用错误保持既有 `usage-filter-unavailable`（本查询不带 filters，不触发该错误）。

### 3.5 费用行（`cost_query.rs`）

- 先 `snapshot_with_conn` 取已归因行；再加载 billing config、provider aliases、price overrides、active catalog。
- 每行解析渠道：显式绑定 → alias → raw provider → unassigned/ambiguous。
- 费用行键 = 长度前缀 tuple：
  `channel_key + 上下文(source_id/observed_provider，仅 unassigned/ambiguous) + original_model + normalized_model_id + pricing_reference + basis + snapshot_id + price_override + "USD"`。
- 不同显示渠道即使模型相同也分行；同一显示渠道+模型+参考价+口径+快照+覆盖标记合并。
- `unassigned`/`ambiguous` 的行键附加工具/原始 provider 上下文，避免不同工具或不同未知来源混成一行；渠道视图仍只有一个“未指定渠道”/“渠道可能混合”组。
- 不生成任何渠道小计或全局合计。

### 3.6 记录页身份行

- `identities_with_conn` 保持返回**原始身份**（`IdentityKey` + `sourceKind` + `attribution` + Token），不追加别名/显示名字段；这是证据面，避免把展示层结果伪装成日志实测。
- 记录页卡内行仍显示旧 `usageRecords` 的原始 `row.label`（`provider / model` 或裸模型）。别名不改写该标签。
- `ProviderAliasDialog` 在打开时用同一条解析规则（非空 provider 精确匹配；空 provider 用 `sourceId+modelId`）从 `ProviderAliasConfig` 计算当前显示名，并同时展示只读的原始 provider/model；保存后 C/D/设置页按新显示名分组，记录页行名保持原始证据。
- 同名候选（同一 `row.label` 对应多条完整身份）必须全部列出让用户显式选择；不得按数组顺序默认选中，也不得拆分 label 猜身份。
- 原始 provider 始终保留在 `IdentityUsage.identity.observedProvider`，不被别名覆盖。

---

## 4. 定价管线改造要点

### 4.1 模型 ID 机械归一化（`pricing::normalize_model_id`）

按顺序执行，全部是确定性文本规则：

1. `trim` + 小写。
2. 若含 `/`，只取最后一个 `/` 之后的部分（剥 vendor/命名空间前缀）。
3. `.` 和 `_` → `-`；连续 `-` 折叠；去首尾 `-`。
4. 循环剥尾（每轮后重新折叠，直到不再变化）：
   - 日期尾：`-\d{4}-\d{2}-\d{2}`、`-\d{8}`、`-\d{4}-\d{2}`（例如 `-2024-08-06`、`-20241022`、`-2026-07`）。
   - `-preview-\d{2}-\d{2}`（例如 `-preview-06-05`）。
   - 版本尾：`-v\d+` 及 `-v\d+(-\d+)*`（例如 `-v1`、`-v1-2`）。
   - 噪声尾：`-preview`、`-latest`、`-exp`、`-experimental`。
   - 推理档位尾：`-thinking`、`-non-thinking`、`-nothinking`、`-reasoning`、`-reasoning-effort-(minimal|low|medium|high)`、`-effort-(minimal|low|medium|high)`、以及裸 `-(minimal|low|medium|high)`。**不剥** `-max`、`-xhigh`、`-ultra`、`-chat`、`-instruct`、`-base`，避免把模型族/模态差异折叠。
5. 空串原样返回；空模型永不参与目录匹配。

示例（测试必须锁住）：

| 原始 | 归一化 |
| --- | --- |
| `openai/gpt-4o-2024-08-06` | `gpt-4o` |
| `anthropic/claude-3.5-sonnet-20241022` | `claude-3-5-sonnet` |
| `google/gemini-2.5-pro-preview-06-05` | `gemini-2-5-pro` |
| `ollama-cloud/glm-5.3-flash` | `glm-5-3-flash` |
| `gpt-5.1-codex-high` | `gpt-5-1-codex` |
| `gpt-5.1-codex-max` | `gpt-5-1-codex-max` |
| `qwen3-max` | `qwen3-max` |
| `gpt-4` | `gpt-4` |
| `claude-sonnet-4-5-v1` | `claude-sonnet-4-5` |

### 4.2 目录解析与去重

- `RawModel` 增加 `release_date: Option<String>`（models.dev 实际字段；`YYYY`、`YYYY-MM`、`YYYY-MM-DD` 均可能出现）。
- `PriceQuote` 增加 `release_date: Option<String>` 与 `normalized_model_id: String`。
- `PriceCatalog` 增加 `normalized: BTreeMap<String, NormalizedQuote>`；`NormalizedQuote` 包含选中的 `reference/rates/conditional/release_date/normalized_model_id`。
- 去重排序（同一归一化 ID 的多条 provider 报价）：
  1. `release_date` 降序（解析为 `(year, month, day)`；缺失/非法视为 `(0,0,0)`）。
  2. 有至少一项费率优先。
  3. 非 conditional 优先。
  4. provider ID 升序、model ID 升序（确定性）。
- 选中条目即使无价/conditional 也保留为“来源”；下游按保守规则返回 unpriced/partial。**不得**用其他 provider 的价格补洞。
- `quotes` 精确键仍保留，供用户 `PricingRule` 引用。
- 来源 provider/model 与 `release_date` 不另建 quote 表：它们随快照 payload 一起落库，查询时从 active 快照解析；`CostRow` 与 `PriceOverrideView` 输出选中值，快照日期 = `fetched_at`。这样避免“payload 与 quote 表双写不一致”，离线回退仍可核对来源。

### 4.3 自动匹配与前缀回退

```rust
pub enum ModelMatchKind { Exact, Prefix, None }
pub struct ModelMatch<'a> {
    pub kind: ModelMatchKind,
    pub quote: Option<&'a NormalizedQuote>,
    pub normalized_query: String,
}
pub fn match_model<'a>(catalog: &'a PriceCatalog, raw_model_id: &str) -> ModelMatch<'a>;
```

- 空模型 → `None`。
- 归一化精确命中 → `Exact`。
- 精确未命中时做前缀回退：候选 = 归一化目录键 `c`，满足 `query.starts_with(c)`、`c != query`、`c.len() >= 4`，且剩余后缀只包含 `-` 分隔的已知噪声词（`preview/latest/exp/experimental/beta/alpha/thinking/reasoning/high/medium/low/minimal/v<digits>/<digits>`）。
  - 候选唯一 → 采用。
  - 候选多个但所有候选费率完全一致 → 取最长候选（同价，确定性）。
  - 候选多个且费率不一致 → `None`，行级 reason `pricingPrefixAmbiguous`。
- 不做“目录键以 query 为前缀”的反向匹配；不按相似度、不做跨模型族替代。
- 用户 `PricingRule` 仍优先于自动匹配；它走精确 `(providerId, modelId)` 键，不受归一化影响。`PricingRule` 命中时不再做自动前缀回退。

### 4.4 手动覆盖优先级与同步保护

- 覆盖表按归一化模型 ID 全局生效；`PriceOverrideRequest.normalized_model_id` 入库前先 `normalize_model_id`。
- 覆盖查找顺序：先按**原始模型的归一化 ID**；没有再按**自动匹配选中的目录归一化 ID**。
- 分量解析：
  - 覆盖值 `Some` → 使用覆盖值。
  - 否则，若目录/用户规则报价 `conditional=false` → 使用目录费率。
  - 否则 → 该分量不可估价。
- `pricingBasis`：至少一个分量来自覆盖 → `userOverride`；全部来自目录/规则且非条件 → `baseCurrent`；没有任何费率 → `insufficient`。
- 刷新流程只写 `usage_pricing_snapshot` / `usage_pricing_state`，**绝不写** `usage_price_override`；成功后也不清理覆盖。
- 覆盖价可以离线保存；无 active catalog 时 `source_provider/source_model/release_date/snapshot_date` 为 null，但金额仍可按覆盖价计算。若该行没有采用目录参考（纯覆盖价），`pricingReference/pricingSnapshotId/priceUpdatedAt` 也置空，详情只显示“用户覆盖价”，不冒充目录来源。视图中的四价字符串用 `rate_string` 从 scaled 值还原，保持十进制、无科学计数。
- 覆盖只影响 API 等效费用，不改 Token、不改账本、不产生全局合计。

### 4.5 刷新节奏与失败回退

- 保持现有 `spawn_pricing_refresh_loop`：5 分钟检查一次，active 缺失或 `now - fetched_at >= 24h` 或 `last_error` 非空时刷新；等价于“默认每日刷新”。手动刷新继续 `force=true`。
- 失败只写 `last_error`，保留旧 active 与 `fetched_at`；`PricingStatus.stale` 语义不变。
- 设置页与 D 卡继续展示 `models.dev`、快照 ID、快照日期、stale/lastError。
- 不新增订阅额度抓取、不新增价格历史查询；历史范围仍标注“当前价回算”。

### 4.6 保守估价不变

- 不修改 `cost.rs::safe_components` / `basis_trusted` / `estimate_components` 的互斥与覆盖率规则。
- `Subset + cacheWrite`、条件价、上下文阶梯、模态不纯文本等继续返回 partial/unpriced。
- 覆盖率分母仍是规范处理 Token（`requestTotal + output`）；缺价不是 0 价。
- 本次只替换“参考价从哪来”，不改变“哪些桶可计价”。

---

## 5. 前端改造要点

### 5.1 数据层、mock 与 i18n

- `src/lib/api/usageV2.ts`：新增 7 个包装，参数逐字对齐 2.4。
- `src/lib/query/queries/usageV2.ts`：
  - `useUsageProviderAliasesQuery()` → key `["usage","v2","providerAliases"]`
  - `useUsageChannelOptionsQuery(range)` → key `["usage","v2","channelOptions",range]`
  - `useUsagePriceOverridesQuery()` → key `["usage","v2","priceOverrides"]`
  - 保存/删除后统一 `invalidateQueries({ queryKey: USAGE_KEY })`；不新增轮询。
- `src/tauri-mock.ts` 与 `src/lib/usage/mockFixtures.ts`：
  - 新增别名/覆盖价的可变 mock 状态与 `resetUsageV2MockState` 复位。
  - 更新 `mockUsageDistribution`、`mockUsageCosts`、`mockUsageCostCsv` 的新字段/新表头。
  - 两端 mock（`src/tauri-mock.ts` 与 `tests/msw/tauriMocks.ts` 共用 fixtures）形状一致。
- i18n：本功能全部 zh/en 键在任务 6 一次性补齐；任务 7–9 只消费键，不再改 locale 文件。至少包含：
  - `usage.label.unknownModel`
  - `usage.records.aliasAction`
  - `usage.alias.*`：title/desc/manage/empty/add/edit/delete/kind/kindProvider/kindToolModel/provider/sourceTool/model/displayName/current/nameRequired/providerRequired/usedBy/usageWindow/mergeHint/providerScopeHint/toolModelScopeHint/readonlyIdentity/saved/deleteConfirm
  - `usage.cost.pricingSource/pricingReleaseDate/normalizedModel/priceOverride/userOverride`
  - `usage.cost.reason.pricingPrefixAmbiguous`
  - `usage.pricing.overrides.*`：title/desc/empty/add/edit/delete/model/modelPlaceholder/input/cacheRead/cacheWrite/output/save/cancel/atLeastOne/invalidRate/source/releaseDate/snapshotDate/noCatalog/saved/deleteConfirm
  - 把 `usage.group.billingProfile` 从“按计费渠道”改为“按渠道”（en `By channel`）；保留 `usage.trend.unknownChannel` 键以免键集漂移，但 UI 不再使用。

### 5.2 设置页：供应商别名管理

- 新增 `src/components/usage/ProviderAliasManager.tsx`（受控 dialog，Props 至少 `{t, open?, onOpenChange?, hideTrigger?}`）。
- 挂载到 `src/components/settings/UsageSection.tsx`，位于“计费渠道与规则”与“定价目录状态”之间。
- 列表：显示规则类型、原始 provider 或工具+模型、显示名、更新时间；同一显示名的多条规则标注“将合并为一个渠道”。
- 使用情况：`useUsageIdentitiesQuery({preset:"last30d"}, {}, {enabled: open})`，按规则键匹配：
  - provider 规则：`identity.observedProvider === rule.provider`
  - toolModel 规则：`identity.observedProvider === null && identity.sourceId === rule.sourceId && identity.modelId === rule.modelId`
  - 展示涉及的工具、模型、Token 合计（`requestTotal + output`）；窗口文案“近 30 天观测”。
- 新增/编辑：类型选择、原始 provider 输入、工具下拉（七源）、模型输入（允许空串表示未知模型）、显示名；保存走 `usageProviderAliasUpsert`，失败保留草稿。
- 删除：二次确认；走 `usageProviderAliasDelete`；不物理删除账本。
- 不恢复 `providerMapRules` JSON 编辑框；旧字段与旧规则原样保留。

### 5.3 记录页：供应商别名浮窗

- `src/components/usage/UsageRecordsPanel.tsx` 每行新增轻量按钮“供应商别名”（已有别名时显示“修改别名”）。与“指定渠道/修改关联/查看规则”并列，但不改变原按钮语义。
- 新增 `src/components/usage/ProviderAliasDialog.tsx`：
  - Props：`{open,onOpenChange,identities,currentAliases,t}`；复用现有 `AssignmentLoader` 的按卡限定 identities 查询与同名候选显式选择逻辑（把 loader 泛化为 `intent: "billing"|"alias"`，或新增同构 loader，禁止复制两套查询）。
  - 只读展示：来源工具、原始供应商（可空）、完整模型名、当前显示名。
  - 非空 provider：规则作用于该原始 provider 的所有工具与模型；空 provider：规则只作用于该 `sourceId + modelId`。
  - 保存：非空 provider 构造 `kind=provider`；空 provider 构造 `kind=toolModel`；已有同键规则复用其 id 更新，否则 `crypto.randomUUID()` 新建。
  - 保存成功 `invalidateQueries({queryKey:USAGE_KEY})`；不写账本、不触发采集。
  - 空 provider 的“未知模型”允许 `modelId=""` 指派。
- 行名仍显示旧 `usageRecords` 的原始标签；别名只在 C/D、设置页与浮窗中体现。原始 provider 不覆盖。

### 5.4 B 趋势筛选

- `src/components/usage/UsageTrendFilters.tsx` 的 Props 从 `profiles` 改为 `channels: ChannelOption[]`；页面用 `useUsageChannelOptionsQuery(range)` 提供。
- 模式仍为 `全部 / 按工具 / 按渠道`；渠道下拉只列 `ChannelOption`，不列“未指定渠道/可能混合”。
- 目标值 = `channel.key`；`TrendSelection` 的 `billingProfile.selectedId` 存该键。
- 选中项不在 options 中或后端 `available=false` → 显示原因 + “回到全部”；A 不联动。
- 删除当前 UI 中的每工具 `unknown` 选项；`usage.trend.unknownChannel` 键保留但不再渲染。

### 5.5 C 分布卡

- `src/lib/usage/distributionV2.ts` 的 `DistributionV2Row` 追加 `channelKind/displayProvider/observedProvider/modelId`。
- `UsageDistributionCard.nameOf`：
  - tool/project：沿用 label。
  - channel：`BillingProfile/Alias/Provider` 用 `displayProvider`；`Unassigned` 用 `t("usage.label.unassignedChannel")`；`Ambiguous` 用 `t("usage.label.ambiguousChannel")`。**不再追加工具后缀**。
  - model：`displayProvider` 或 i18n 键 + ` / ` + `modelId || t("usage.label.unknownModel")`；`Unassigned` 且空 provider 时前缀 `t("usage.label.unassignedChannel") + " · " + usageAgentLabel(sourceId)`；`Ambiguous` 同理用原始 provider 或工具。
- tooltip/title 保留原始 provider 与完整模型；同一显示名跨工具合并后只出一行。
- Top5/显示更多/折叠逻辑不变。

### 5.6 D 费用卡与费用行

- `src/lib/usage/costRows.ts`：
  - 渠道视图直接用 `row.channelKey` 分组、`row.displayProvider` 或 `channelKind` 生成标签；不再从 `profiles` 字典猜。
  - 模型视图每个 `CostRow` 一项；标签由行自身的 `displayProvider/channelKind` 决定。
  - 不产生金额小计。
- `UsageCostCard.groupDisplayName`：不再拼接全部 sourceIds；渠道组标签只显示显示名/i18n 键。
- `UsageCostRow`：
  - 行首 = `displayName / modelId`；`Unassigned`/`Ambiguous` 加工具/原始 provider 上下文。
  - 金额、Token、命中率、覆盖率规则不变。
  - 详情新增：价格来源 `pricingReference`（provider/model）、`pricingReleaseDate`、`normalizedModelId`、`priceOverride` 标记；`pricingBasis=userOverride` 显示“用户覆盖价”。
  - 缺价仍显示 `—（未估价）`，不显示 `$0.00`。
- 不新增渠道小计/全局金额/费用趋势。
- 页面 `src/pages/usage-dashboard.tsx` 接入 `useUsageChannelOptionsQuery`，把 `channels` 传给 `UsageTrendFilters`；其余折叠/范围状态不变。

### 5.7 费用 CSV

- 后端 `CSV_HEADER` 在既有列后追加：
  `channel_key,channel_kind,display_provider,normalized_model_id,pricing_release_date,price_override`
- 保留 `billing_profile_id,billing_profile_name,source_ids,observed_provider,model_id,attribution_kind,token_usage,estimated_usd,status,priced_tokens,unpriced_tokens,coverage_rate,pricing_provider,pricing_model,pricing_snapshot_id,pricing_basis,price_updated_at`。
- 无 BOM、无总计行；公式注入中和规则不变；缺金额为空字段而不是 0。
- 同步 `mockUsageCostCsv` 与 `v2-cost-export.test.tsx` 的表头断言。

---

## 6. 任务表

> 约定：每个任务先写失败测试并运行确认红测原因；再最小实现；绿测；自查；交给独立 Review Agent。每个后端任务修改 `services/usage/**` 生产代码后，必须在同一任务内同步自省锁钉子（见 §8.3），否则 `collect::tests` 必红。

### Task 0：只读基线与探测

- **目标**：确认工作区、基线测试与 models.dev 字段事实，不写实现。
- **涉及文件**：无（只读）。执行记录追加到本文件末尾。
- **前置**：无。
- **实现要点**：
  1. `git rev-parse HEAD`、`git status --short`；确认基线为 `455c240b` 或记录实际值；确认未运行 `pnpm tauri:dev`。
  2. 只读探测 models.dev：`curl -k -s --max-time 20 -r 0-200000 https://models.dev/api.json`，确认 `release_date` 形态与同名模型多 provider 现象；不发送任何本地会话/用量数据。
  3. 跑基线：`pnpm vitest run tests/usage tests/pet/usage-*`；`cargo test --manifest-path src-tauri/Cargo.toml -j 4 --lib services::usage::v2`；`cargo test --manifest-path src-tauri/Cargo.toml -j 4 --test usage_cost_test`；`cargo test --manifest-path src-tauri/Cargo.toml -j 4 --test usage_billing_test`；`cargo test --manifest-path src-tauri/Cargo.toml -j 4 --test usage_ipc_test`。
  4. 记录红测/环境限制；基线失败不得用改测试掩盖。
- **验收**：基线命令退出码与结果记录完整；未产生代码改动。
- **验证命令**：上述命令。
- **可并行**：否（第一任务）。

### Task 1：wire 契约与 TS 类型

- **目标**：建立 2.2/2.3 的全部新类型与字段，后续任务只依赖该契约。
- **涉及文件**：
  - `src-tauri/src/services/usage/v2/model.rs`
  - `src/types/usageV2.ts`
  - `src-tauri/tests/usage_ipc_test.rs`（序列化断言；与 Task 6 串行）
  - `src-tauri/src/services/usage/collect.rs`（自省锁钉子）
- **前置**：Task 0。
- **TDD**：先在 `usage_ipc_test.rs` 写 serde 形状测试（新类型字段、`channelKind`/`providerAliasKind` 字符串、`Option` 为 null、`PricingBasis=userOverride`）；运行确认编译失败于类型缺失。
- **实现要点**：
  - 按 2.2 增加/扩展类型；`ProviderAliasKind` 序列化为 `provider`/`toolModel`；`ChannelKind` 为 camelCase。
  - `DistributionRow`/`CostRow` 新字段全部显式序列化为 null/值，不加 `skip_serializing_if`。
  - TS 逐字镜像；`PriceOverrideView` 继承请求字段。
  - 在 `model.rs` 末尾保留自省锁排除区标记；**不要**在 `model.rs` 内加测试模块（避免分类变化）。
- **验收**：serde 测试通过；TS `tsc` 通过；`scan_face_size_is_pinned` 已同步。
- **验证命令**：
  - `cargo test --manifest-path src-tauri/Cargo.toml -j 4 --test usage_ipc_test`
  - `cargo test --manifest-path src-tauri/Cargo.toml -j 4 --lib services::usage::collect::tests::scan_face_size_is_pinned`
  - `pnpm exec tsc --noEmit`
- **可并行**：否。

### Task 2：Provider 别名持久化与渠道解析

- **目标**：落地 `usage_provider_alias` 表、DAO、`channel.rs` 解析器与服务；渠道维度可独立单测。
- **涉及文件**：
  - 新增 `src-tauri/src/services/usage/v2/channel.rs`
  - 新增 `src-tauri/src/database/dao/usage_alias.rs`
  - 修改 `src-tauri/src/database/dao/mod.rs`
  - 修改 `src-tauri/src/services/usage/v2/mod.rs`
  - 修改 `src-tauri/src/database/schema.rs`
  - 修改 `src-tauri/src/database/migration.rs`
  - 修改 `src-tauri/src/services/usage/collect.rs`（登记新文件 + 钉子 + 计数）
  - 修改 `src-tauri/src/services/usage/mod.rs`（自省锁文档 32→33、26/6→27/6）
  - 新增 `src-tauri/tests/usage_provider_alias_test.rs`
- **前置**：Task 1。
- **TDD**：
  - `channel.rs` 单测：provider 精确映射、不同原始串同显示名合并、同原始串跨工具合并、空 provider 的 toolModel 指派、未指派合并 unassigned、歧义不套别名、显式绑定优先、归档 profile 仍解析但选项排除、校验拒绝空显示名/重复键/kind 形状错误。
  - 集成测试：使用 `mod support;` 与 `support::open_ledger_db("provider_alias")` 建私有临时库；建表/CRUD/重开库后读取/删除幂等/键冲突拒绝；连续 migrate 两次旧账本不变。
  - 运行确认红测于模块/表/DAO 缺失。
- **实现要点**：
  - DDL 按 2.1；DAO 全部返回 `Result`，不吞 SQL 错。
  - `upsert` 事务内先查 id 与目标键；冲突返回固定 detail；成功后返回完整配置。
  - `channel.rs` 实现 `ChannelResolver`/`resolve_channel`/`join_key`；生产入口取 DB 锁，`*_with_conn` 供测试。文件末尾必须放一对自省锁排除区标记并把 `#[cfg(test)] mod tests` 包在中间（否则 `scan_face` 会判红）。
  - `USAGE_SOURCES` 增至 33 项，`v2/channel.rs` 用 `include_str!` 登记；分类断言改为 `(27, 6)`（channel.rs 有测试模块）；更新注释中的文件数/面尺寸。
- **验收**：上述测试全绿；迁移幂等；渠道解析优先级与 Spec §2.4 逐条一致。
- **验证命令**：
  - `cargo test --manifest-path src-tauri/Cargo.toml -j 4 --lib services::usage::v2::channel`
  - `cargo test --manifest-path src-tauri/Cargo.toml -j 4 --test usage_provider_alias_test`
  - `cargo test --manifest-path src-tauri/Cargo.toml -j 4 --lib database::migration`
  - `cargo test --manifest-path src-tauri/Cargo.toml -j 4 --lib services::usage::collect::tests`
- **可并行**：否（与 Task 3 共享 schema/DAO mod/自省锁文件，必须串行）。

### Task 3：定价归一化、目录去重与手动覆盖

- **目标**：实现 `normalize_model_id`、归一化目录索引、release_date 去重、前缀回退、覆盖价表与解析。
- **涉及文件**：
  - `src-tauri/src/services/usage/v2/pricing.rs`
  - `src-tauri/src/database/dao/usage_billing.rs`（覆盖价 DAO）
  - `src-tauri/src/database/schema.rs`
  - `src-tauri/src/database/migration.rs`
  - `src-tauri/src/services/usage/collect.rs`（钉子）
  - `src-tauri/tests/fixtures/usage-v2/pricing/{normalize,duplicates,prefix}.json`（新增）
- **前置**：Task 2（串行，避免 schema/钉子冲突）。
- **TDD**：
  - `pricing.rs` 单测：归一化示例表全部命中；不误剥 `gpt-4`/`qwen3-max`；release_date 解析；同归一化 ID 多 provider 按最新日期选一条；同日期有价/非条件优先；前缀回退唯一/同价/多候选异价/危险后缀；空模型不匹配。
  - 覆盖价：四分量逐项覆盖、部分覆盖产生 partial、conditional 分量被覆盖后可计价、覆盖价优先级、刷新不动覆盖表、无目录仍可用覆盖价。
  - 运行确认红测于函数/表缺失。
- **实现要点**：
  - 按 4.1–4.4；`parse_rate` 复用，覆盖价入库前转 scaled。
  - 目录解析仍严格：`cost` 非数字/负数/指数/超精度整目录失败；未知 cost 键/模态不纯文本标 conditional。
  - `PriceCatalog` 保留 `quotes` 并新增 `normalized`；进程级缓存继续按 snapshot id 复用。
  - `usage_price_override` DAO：`load_overrides_conn`、`upsert_override_conn`、`delete_override_conn`；刷新流程无任何写覆盖表调用。
- **验收**：所有定价/覆盖单测绿；刷新失败仍保留旧快照；覆盖价在刷新后不变。
- **验证命令**：
  - `cargo test --manifest-path src-tauri/Cargo.toml -j 4 --lib services::usage::v2::pricing`
  - `cargo test --manifest-path src-tauri/Cargo.toml -j 4 --lib database::migration`
  - `cargo test --manifest-path src-tauri/Cargo.toml -j 4 --lib services::usage::collect::tests`
- **可并行**：否。

### Task 4：Token 查询层接入显示渠道

- **目标**：趋势、分布、渠道选项全部使用解析后的显示渠道；保留原始 provider 字段。
- **涉及文件**：
  - `src-tauri/src/services/usage/v2/query.rs`
  - `src-tauri/src/services/usage/collect.rs`（钉子）
- **前置**：Task 2。
- **TDD**：
  - `query.rs` 单测：两工具同原始 provider → 渠道一行；两个原始串同显示名 → 合并；空 provider 未指派 → 渠道一行且模型按工具拆行；空 model → `unknownModel` 组合；显式绑定覆盖别名；归档 profile 在分布可见但选项排除；趋势 `bp:`/`provider:` 选择与上期同口径；`unknown` selection 不可用。
  - 运行确认红测于新字段/新函数缺失。
- **实现要点**：
  - `distribution_with_conn` 加载 aliases/resolver，按 3.3 生成键/标签/结构化字段。
  - `trend_with_conn` 按 3.2 解析；比较期复用同一 selection/resolver。
  - 新增 `channel_options_with_conn` 与生产入口 `channel_options`。
  - 保持 `snapshot_with_conn` 签名不变，避免影响 billing preview；渠道解析在消费方进行。
- **验收**：分布/趋势/选项测试绿；旧 `usage::query` 测试不回归；A 的全局 hero 不变。
- **验证命令**：
  - `cargo test --manifest-path src-tauri/Cargo.toml -j 4 --lib services::usage::v2::query`
  - `cargo test --manifest-path src-tauri/Cargo.toml -j 4 --lib services::usage::query`
  - `cargo test --manifest-path src-tauri/Cargo.toml -j 4 --lib services::usage::collect::tests`
- **可并行**：否。

### Task 5：费用查询接入自动定价与渠道行

- **目标**：费用行按显示渠道/原始模型拆分，自动匹配归一化价，应用覆盖价，扩展 CSV。
- **涉及文件**：
  - `src-tauri/src/services/usage/v2/cost_query.rs`
  - `src-tauri/src/services/usage/collect.rs`（钉子）
  - `src-tauri/tests/usage_cost_test.rs`
  - `src-tauri/tests/fixtures/usage-v2/pricing/*.json`（补充）
- **前置**：Task 2、Task 3、Task 4。
- **TDD**：
  - 同归一化模型不同显示渠道 → 两行、同一单价；同显示渠道跨工具 → 一行；`unassigned`/`ambiguous` 行保留工具/原始 provider 上下文。
  - 无 `PricingRule` 时自动匹配成功；用户 `PricingRule` 仍优先；前缀回退唯一/同价；多候选异价 → unpriced + `pricingPrefixAmbiguous`。
  - 覆盖价生效、`priceOverride=true`、`pricingBasis=userOverride`；刷新后覆盖不变；无目录时覆盖价仍可计价。
  - `pricingReference/pricingReleaseDate/pricingSnapshotId/priceUpdatedAt` 填充正确。
  - CSV 表头逐字断言、追加列、无总计行、公式中和不变。
  - 运行确认红测。
- **实现要点**：
  - 按 3.5/4.3/4.4 重写 `select_reference`/`resolve_row_pricing`；删除“仅 measured + 非空 provider 才可精确回退”的旧门槛（自动匹配不依赖 `provider_kind`）。
  - 先加载一次 aliases/overrides/catalog，避免逐行查库。
  - 行键包含渠道键、原始模型、归一化模型、参考键、bases、快照、覆盖标记、USD。
  - `estimated_usd`/覆盖率/状态语义不变。
- **验收**：费用与 CSV 测试绿；没有 total/小计字段；覆盖表在刷新后保持不变。
- **验证命令**：
  - `cargo test --manifest-path src-tauri/Cargo.toml -j 4 --test usage_cost_test`
  - `cargo test --manifest-path src-tauri/Cargo.toml -j 4 --lib services::usage::v2::cost`
  - `cargo test --manifest-path src-tauri/Cargo.toml -j 4 --lib services::usage::collect::tests`
- **可并行**：否。

### Task 6：IPC、前端数据层、mock 与 i18n

- **目标**：把 7 条新命令暴露给前端，建立查询 hooks、mock 状态与全部中英文键。
- **涉及文件**：
  - `src-tauri/src/commands/usage_v2.rs`
  - `src-tauri/src/lib.rs`
  - `src-tauri/tests/usage_ipc_test.rs`
  - `src/lib/api/usageV2.ts`
  - `src/lib/query/queries/usageV2.ts`
  - `src/tauri-mock.ts`
  - `tests/msw/tauriMocks.ts`
  - `src/lib/usage/mockFixtures.ts`
  - `src/i18n/locales/zh.json`
  - `src/i18n/locales/en.json`
  - `tests/usage/v2-api.test.tsx`
  - `tests/usage/v2-queries.test.tsx`
  - `tests/usage/usageMockParity.test.ts`
- **前置**：Task 1–5。
- **TDD**：
  - `v2-api`：7 条命令名、参数键、返回形状逐字断言；旧 12 条不回归。
  - `v2-queries`：三个新 hook 的 queryKey；不同 range 不同键；读请求不触发 `usage_collect`；保存后 `USAGE_KEY` 失效。
  - `usageMockParity`：两端 mock 对新命令/新字段形状一致。
  - `usage_ipc_test`：新命令恰好注册一次；命令层不取 DB 锁、不触发采集。
  - 运行确认红测。
- **实现要点**：
  - 命令 `async` + `spawn_blocking`；服务生产入口持锁。
  - mock 状态含 aliases/overrides；`resetUsageV2MockState` 复位；upsert/delete 真实更新 mock 状态并拒绝重复键。
  - 更新 `mockUsageDistribution/mockUsageCosts/mockUsageCostCsv` 的结构与新表头。
  - 一次性补齐 5.1 的 zh/en 键；运行 `pnpm check:i18n`。
- **验收**：API/hooks/mock/IPC 测试绿；i18n 键集一致；错误码门禁通过。
- **验证命令**：
  - `pnpm vitest run tests/usage/v2-api.test.tsx tests/usage/v2-queries.test.tsx tests/usage/usageMockParity.test.ts`
  - `cargo test --manifest-path src-tauri/Cargo.toml -j 4 --test usage_ipc_test`
  - `pnpm check:i18n`
  - `node scripts/check-error-codes.mjs`
- **可并行**：否（后续三个 UI 任务都依赖它）。

### Task 7：别名 UI（设置页 + 记录页浮窗）

- **目标**：用户可在设置页集中管理别名，在记录页每行浮窗直达编辑；显示使用范围。
- **涉及文件**：
  - 新增 `src/components/usage/ProviderAliasManager.tsx`
  - 新增 `src/components/usage/ProviderAliasDialog.tsx`
  - 新增 `src/components/usage/ProviderAliasFields.tsx`（可选，供两者复用）
  - 修改 `src/components/usage/UsageRecordsPanel.tsx`
  - 修改 `src/components/settings/UsageSection.tsx`
  - 新增 `tests/usage/v2-provider-alias-ui.test.tsx`
- **前置**：Task 6。
- **TDD**：
  - 设置页：列出规则、显示使用工具/模型/Token、近 30 天窗口、同显示名合并提示、新增/编辑/删除、空显示名/空 provider 拒绝、失败保留草稿。
  - 记录页：行按钮打开浮窗；只读展示来源工具/原始 provider/完整模型；同名候选显式选择；非空 provider 规则作用于该 provider；空 provider 保存 `tool+model`；已有规则预填并更新；保存后失效 `USAGE_KEY`。
  - 运行确认红测。
- **实现要点**：
  - 复用 `AssignmentLoader` 的 identities 查询与候选选择；用 `intent` 分派 billing/alias，不复制查询。
  - 不修改 `BillingAssignmentDialog` 的计费语义；两个浮窗独立。
  - 不修改 locale 文件；缺键报告协调者，由 Task 6 补。
- **验收**：别名 UI 测试绿；记录页既有“指定渠道/修改关联/查看规则”回归绿；保存后设置与记录页显示一致。
- **验证命令**：
  - `pnpm vitest run tests/usage/v2-provider-alias-ui.test.tsx tests/pet/usage-records.test.tsx tests/pet/usage-settings.test.tsx`
  - `pnpm exec tsc --noEmit`
- **可并行**：是（与 Task 8、Task 9 文件不重叠）。

### Task 8：手动覆盖价 UI

- **目标**：设置页可查看/新增/编辑/删除任意归一化模型四价，展示来源 provider、release_date、快照日期与覆盖标记。
- **涉及文件**：
  - 新增 `src/components/usage/PriceOverrideManager.tsx`
  - 修改 `src/components/usage/PricingStatusPanel.tsx`
  - 新增 `tests/usage/v2-pricing-override.test.tsx`
- **前置**：Task 6。
- **TDD**：
  - 列表/新增/编辑/删除；四价均可空但至少一项；非法数字拒绝；无 active catalog 仍可保存并显示“无目录来源”；有目录时展示 sourceProvider/sourceModel/releaseDate/snapshotDate；保存后失效 costs。
  - 运行确认红测。
- **实现要点**：
  - 表单四价用文本输入，提交字符串；空串转 null。
  - `PricingStatusPanel` 在状态区下方挂载 manager；不改变启用/刷新行为。
  - 不修改 locale；不新增轮询。
- **验收**：覆盖价 UI 测试绿；定价状态面板既有测试回归绿。
- **验证命令**：
  - `pnpm vitest run tests/usage/v2-pricing-override.test.tsx tests/usage/v2-cost-settings.test.tsx`
  - `pnpm exec tsc --noEmit`
- **可并行**：是（与 Task 7、Task 9）。

### Task 9：C/B/D 展示改造

- **目标**：C 用显示渠道与结构化模型行；B 用渠道选项；D 用渠道键分组并展示价格来源。
- **涉及文件**：
  - `src/components/usage/UsageDistributionCard.tsx`
  - `src/components/usage/UsageTrendFilters.tsx`
  - `src/components/usage/UsageCostCard.tsx`
  - `src/components/usage/UsageCostRow.tsx`
  - `src/lib/usage/costRows.ts`
  - `src/lib/usage/distributionV2.ts`
  - `src/pages/usage-dashboard.tsx`
  - `tests/usage/v2-distribution.test.tsx`
  - `tests/usage/v2-cost-card.test.tsx`
  - `tests/usage/v2-page.test.tsx`
- **前置**：Task 6。
- **TDD**：
  - C：同名模型不再出无法区分的重复行；同显示名跨工具合并；未指定渠道渠道维度一行；模型维度按工具拆行；空模型显示“未知模型”；原始 provider 在 title。
  - B：渠道下拉来自 `ChannelOption`，无“未指定渠道”；选择 `provider:` 键后曲线/峰值/环比同口径；A/C 不联动；失效时回退全部。
  - D：同模型不同显示渠道两行同单价；渠道组标签不拼工具；unassigned/ambiguous 行含工具上下文；详情显示 source provider/release date/snapshot/override；无总计/小计。
  - 运行确认红测。
- **实现要点**：
  - `costRows.ts` 改用 `row.channelKey/channelKind/displayProvider`；可删除 `profiles` 参数（同步调用方与测试）。
  - `UsageDistributionCard.nameOf` 按 5.5；`UsageTrendFilters` Props 改为 `channels`。
  - `UsageCostCard` 移除 sourceIds 拼接；`UsageCostRow` 增加详情行与覆盖徽标。
  - `usage-dashboard.tsx` 接 `useUsageChannelOptionsQuery`。
- **验收**：三个测试文件绿；旧 cards/page 回归绿；无金额总计字段。
- **验证命令**：
  - `pnpm vitest run tests/usage/v2-distribution.test.tsx tests/usage/v2-cost-card.test.tsx tests/usage/v2-page.test.tsx tests/pet/usage-cards.test.tsx tests/pet/usage-page.test.tsx`
  - `pnpm exec tsc --noEmit`
- **可并行**：是（与 Task 7、Task 8）。

### Task 10：费用 CSV 与文档同步

- **目标**：费用 CSV 输出新列；README/用户手册/CHANGELOG 反映新行为。
- **涉及文件**：
  - `src/lib/usage/mockFixtures.ts`（mock CSV 表头）
  - `tests/usage/v2-cost-export.test.tsx`
  - `README.md`
  - `README.en.md`
  - `docs/user-manual/zh/11-usage.md`
  - `docs/user-manual/en/11-usage.md`
  - `CHANGELOG.md`
- **前置**：Task 5、Task 6、Task 9。
- **TDD**：`v2-cost-export.test.tsx` 断言新表头、追加列、无总计行、只传 range、不调用旧 Token CSV；先红后绿。
- **实现要点**：
  - `mockUsageCostCsv` 与后端表头逐字一致；公式中和不变。
  - 文档只描述本次增量：别名、空 provider 指派、自动定价、release_date 去重、覆盖价、费用 CSV 新列；不改 MASTER-PLAN。
  - 中英文 README 与用户手册对应章节同步；CHANGELOG 未发布小节记录。
- **验收**：导出测试绿；`pnpm check` 绿；文档无旧口径残留。
- **验证命令**：
  - `pnpm vitest run tests/usage/v2-cost-export.test.tsx tests/usage/usageMockParity.test.ts`
  - `pnpm check:i18n`
  - `git diff --check`
- **可并行**：否（依赖 Task 9 的 mock/UI 最终形状）。

### Task 11：集成验收、自省锁与全量回归

- **目标**：补齐跨模块验收测试，完成自省锁/迁移/错误码/i18n/全量回归与最终独立审查。
- **涉及文件**：
  - `tests/usage/v2-acceptance.test.tsx`
  - 既有 `tests/usage/*`、`tests/pet/usage-*` 中受影响的断言
  - `src-tauri/src/services/usage/collect.rs`（最终钉子/计数）
  - `src-tauri/src/services/usage/mod.rs`（自省锁文档）
  - `src-tauri/src/database/migration.rs`（六表幂等断言）
- **前置**：Task 0–10。
- **TDD/验收**：
  - 新增 acceptance：未知 provider → 设置别名 → C/D 分组变化且 Token 总量不变；空 provider 逐条指派回溯；显式绑定覆盖别名；费用自动定价 + 覆盖价；D 同模型不同渠道两行同单价；无总费用；撤销/重载后原始 identity 不变。
  - 更新旧测试中受新渠道键/标签/费用行字段影响的断言；不得删除空态、真零、主题、金额缺席断言。
  - 自省锁：33 文件 / 27 测试模块区 / 6 空区；钉子数字为实测值；`USAGE_SOURCES` 与磁盘逐一相等。
  - 迁移幂等：六张 V2 表存在；旧四表逐值不变；连续 migrate 两次。
  - 错误码：`node scripts/check-error-codes.mjs` 通过；无新增码。
  - i18n：zh/en 键集一致；逐键核对新文案英文质量（不能只看键集）。
  - 最终 Review Agent 做规格符合性 + 代码质量两轮；发现问题退回实现任务修复并复审。
- **验收**：全部命令退出 0；已知限制落档；最终一次性交付用户验收。
- **验证命令**：
  - `pnpm vitest run tests/usage tests/pet/usage-*`
  - `pnpm check`
  - `cargo test --manifest-path src-tauri/Cargo.toml -j 4`
  - `cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -j 4 -- -D warnings`
  - `cargo fmt --manifest-path src-tauri/Cargo.toml -- --check`
  - `cargo test --manifest-path src-tauri/Cargo.toml -j 4 --lib database::migration`
  - `cargo test --manifest-path src-tauri/Cargo.toml -j 4 --lib services::usage::collect::tests`
- **可并行**：否（最终收口）。

---

## 7. 实施顺序与派发批次

| 批次 | 任务 | 并行度 | 说明 |
| --- | --- | --- | --- |
| B0 | Task 0 | 1 | 只读基线，无代码改动。 |
| B1 | Task 1 | 1 | 契约先行；后续所有任务依赖。 |
| B2 | Task 2 | 1 | 新建 channel.rs，必须串行处理 schema/DAO/自省锁。 |
| B3 | Task 3 | 1 | 与 Task 2 共享 schema/DAO mod/钉子，必须串行。 |
| B4 | Task 4 | 1 | 依赖 Task 2；修改 query.rs 与钉子。 |
| B5 | Task 5 | 1 | 依赖 Task 3/4；修改 cost_query.rs 与钉子。 |
| B6 | Task 6 | 1 | IPC + 前端数据层 + mock + i18n；后续 UI 的共同前置。 |
| B7 | Task 7、Task 8、Task 9 | 3 | 文件不重叠，可并行派发；共享只读依赖为 Task 6 产物。 |
| B8 | Task 10 | 1 | 依赖 Task 9 的最终 mock/UI 形状。 |
| B9 | Task 11 | 1 | 全量回归 + 最终独立审查。 |

**并行纪律**：

- 同一时刻只允许一个执行 Agent 修改 `services/usage/**` 或 `schema.rs`/`dao/mod.rs`/`collect.rs`。这些文件是自省锁与迁移的共同面，冲突代价高。
- B7 三个任务的文件所有权：
  - Task 7：`ProviderAliasManager.tsx`、`ProviderAliasDialog.tsx`、`ProviderAliasFields.tsx`、`UsageRecordsPanel.tsx`、`UsageSection.tsx`、`v2-provider-alias-ui.test.tsx`。
  - Task 8：`PriceOverrideManager.tsx`、`PricingStatusPanel.tsx`、`v2-pricing-override.test.tsx`。
  - Task 9：`UsageDistributionCard.tsx`、`UsageTrendFilters.tsx`、`UsageCostCard.tsx`、`UsageCostRow.tsx`、`costRows.ts`、`distributionV2.ts`、`usage-dashboard.tsx`、`v2-distribution.test.tsx`、`v2-cost-card.test.tsx`、`v2-page.test.tsx`。
- 任何任务需要修改不属于自己的文件（尤其 locale、mock fixtures、types）时，停下报告协调者，由对应任务补齐；不得并行抢改。
- 每任务内部：执行 Agent 完成红绿与自查；独立 Review Agent 先规格符合性、再代码质量；有问题退回修复并复审。执行者不得自审代替。

---

## 8. 回归与门禁

### 8.1 测试隔离

- 修改 `src-tauri/src/services/usage/**` 前停止 `pnpm tauri:dev`；测试只使用内存库或 `support::open_ledger_db(tag)` 私有临时库；不启动真实采集、不写真实账本。
- 前端只用合成夹具与 mock；不把真实用量提交到仓库。
- 公开目录探测只 GET `https://models.dev/api.json`，不上传任何本地身份/用量。

### 8.2 命令清单

| 范围 | 命令 | 期望 |
| --- | --- | --- |
| Rust 定向 | `cargo test --manifest-path src-tauri/Cargo.toml -j 4 --lib services::usage::v2::channel` | 0 |
| Rust 定向 | `cargo test --manifest-path src-tauri/Cargo.toml -j 4 --lib services::usage::v2::pricing` | 0 |
| Rust 定向 | `cargo test --manifest-path src-tauri/Cargo.toml -j 4 --lib services::usage::v2::query` | 0 |
| Rust 定向 | `cargo test --manifest-path src-tauri/Cargo.toml -j 4 --lib services::usage::v2::cost` | 0 |
| Rust 集成 | `cargo test --manifest-path src-tauri/Cargo.toml -j 4 --test usage_provider_alias_test` | 0 |
| Rust 集成 | `cargo test --manifest-path src-tauri/Cargo.toml -j 4 --test usage_cost_test` | 0 |
| Rust 集成 | `cargo test --manifest-path src-tauri/Cargo.toml -j 4 --test usage_billing_test` | 0 |
| Rust 集成 | `cargo test --manifest-path src-tauri/Cargo.toml -j 4 --test usage_ipc_test` | 0 |
| Rust 迁移 | `cargo test --manifest-path src-tauri/Cargo.toml -j 4 --lib database::migration` | 0 |
| Rust 自省锁 | `cargo test --manifest-path src-tauri/Cargo.toml -j 4 --lib services::usage::collect::tests` | 0 |
| Rust 全量 | `cargo test --manifest-path src-tauri/Cargo.toml -j 4` | 0（环境不支持项单独记录，不放宽） |
| Rust lint | `cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -j 4 -- -D warnings` | 0 |
| Rust fmt | `cargo fmt --manifest-path src-tauri/Cargo.toml -- --check` | 0 |
| 前端定向 | `pnpm vitest run tests/usage tests/pet/usage-*` | 0 |
| 前端全门禁 | `pnpm check` | 0 |
| i18n | `pnpm check:i18n` | 0 |
| 错误码 | `node scripts/check-error-codes.mjs` | 0 |
| 类型 | `pnpm exec tsc --noEmit` | 0 |

Windows 内存紧张时 `cargo` 使用 `-j 4`，必要时 `-j 2`；冷编译耗时长，不得因超时误判失败。

### 8.3 自省锁钉子同步流程（每个后端任务必做）

1. 运行 `cargo test --manifest-path src-tauri/Cargo.toml -j 4 --lib services::usage::collect::tests::scan_face_size_is_pinned`。
2. 失败信息给出实测 `(lines, chars)`；把 `src-tauri/src/services/usage/collect.rs` 中 `assert_eq!((lines, chars), (旧值))` 更新为实测值。
3. 更新 `collect.rs` 注释中的“第 N 轮同步”、判据面数字、`no_source_timezone_is_fed_into_any_key_function` 文档。
4. Task 2 额外：
   - `USAGE_SOURCES` 数组长度 32→33，新增 `("v2/channel.rs", include_str!("v2/channel.rs"))`。
   - `usage_source_scan_covers_every_file_on_disk` 分类断言 26/6→27/6（channel.rs 有测试模块）。
   - `services/usage/mod.rs` 文档中的 32 文件/26/6 同步。
   - `collect.rs` 中“全部 32 个文件”“实测（32 个文件逐一对账）”等注释同步为 33。
5. 重新运行自省锁用例直至绿。禁止删除断言或缩小扫描面来“修绿”。

### 8.4 迁移幂等

- `schema.rs` 只追加 `CREATE TABLE IF NOT EXISTS` / `CREATE UNIQUE INDEX IF NOT EXISTS`。
- `migration.rs` 的旧库形状测试：DROP 六张 V2 表 → 写入旧四表数据/游标 → `migrate` 两次 → 旧四表逐值一致、六张新表存在。
- 不修改旧表主键、不重写历史、不新增 user_version 序列。

### 8.5 错误码与 i18n

- 不新增 `USAGE_CODES` 条目；别名/覆盖校验失败用 `usage-settings-invalid`。
- `pnpm check:i18n` 只保键集；Task 11 必须逐键核对新增英文值，不能只看键集通过。
- 所有新按钮/空态/错误/详情均有 zh/en；键盘与 `aria-expanded`/`aria-controls` 沿用现有组件规范。

---

## 9. 验收标准

1. 近 7 天总 Token 与账本一致；未改 Collector/游标/Token 口径；A 的全局 hero 不随 B/C/D 筛选变化。
2. provider 别名按原始串精确映射；不同原始串同显示名跨工具合并；别名不回写账本，原始 provider 保留在结构化行与 tooltip。
3. 空 provider 的「工具 + 原始模型」逐条指派保存后回溯生效；未指派时渠道维度合并一行“未指定渠道”，模型维度按工具拆行；空模型显示“未知模型”。
4. 显式 `billingProfileId` 绑定优先于别名；归档渠道仍可显示历史行，但不进入趋势选项。
5. B 趋势可按渠道键（`bp:`/`provider:`）筛选，曲线/峰值/环比同口径；A/C 不联动；未指定渠道/渠道可能混合不作为选项。
6. C 默认按渠道、Top5、显示更多；同一显示名跨工具合并；同名模型不再产生无法区分的重复行；无美元列。
7. D 费用行按显示渠道/模型拆分；同归一化模型不同显示渠道分行但单价相同；无全局金额、渠道小计、费用趋势；缺价显示 `—（未估价）`。
8. 自动匹配覆盖归一化模型 ID；多 provider 按 `release_date` 最新去重；来源 provider 与 `release_date` 可在行详情/设置页核对；前缀回退仅唯一或同价时采用；跨模型族不自动替代。
9. 手动覆盖任意模型四价；覆盖后 `priceOverride=true`/`pricingBasis=userOverride`；刷新同步不覆盖用户值；无目录时覆盖价仍可用。
10. 记录页浮窗可查看只读来源并保存别名；设置页集中管理并显示使用工具/模型；两处保存后的分组一致。
11. 费用 CSV 追加渠道/来源/覆盖列，无总计行；Token CSV/分享图/迷你条不变。
12. 迁移幂等；旧四表逐值不变；自省锁 33 文件/27 测试模块区/6 空区；无新增错误码；i18n zh/en 键齐全；深浅主题与键盘可用。

---

### 9.1 Spec 覆盖矩阵

| Spec 条款 | 落点 | 任务 |
| --- | --- | --- |
| §2.3 渠道=provider 显示名、显式绑定优先、未指定渠道不作为选项 | 渠道解析、趋势选项 | 2、4、9 |
| §2.4 渠道维度顺序、同显示名跨工具合并、模型维度按工具拆行、空模型 | 分布分组/标签 | 2、4、9 |
| §2.5 费用行按显示名/模型拆分、无总额/小计、价格来源/日期 | 费用分组/详情 | 3、5、8、9 |
| §2.7 记录页浮窗、设置页别名管理、费用 CSV | UI/导出 | 7、8、10 |
| §3.1 displayProvider 与 observedProvider 分离、不回写账本 | wire 类型/解析 | 1、2、4、5 |
| §3.2 精确匹配、别名查询层解析、空 provider 逐条指派回溯 | 别名表/解析 | 2、7 |
| §4.1 models.dev、每日刷新、失败回退、归一化匹配、release_date 去重、前缀回退、覆盖价 | 定价管线 | 3、5、8 |
| §4.2 同模型全渠道同价、分行、保守 partial/unpriced | 费用查询 | 5、9 |
| §5 ProviderAliasResolver 查询层解析 | channel.rs | 2、4 |
| §6 验收 11–13 | 验收测试 | 7、9、11 |
| §7.2 全部冻结口径 | 本计划 §0 | 全任务 |

---

## 10. 风险与边界

| 风险 | 影响 | 缓解 |
| --- | --- | --- |
| 两个订阅在日志中的原始 provider 串完全相同 | 别名层无法拆分，渠道被合并 | 明确接受为已知限制；只能用显式计费绑定或记录页逐条指派，不按 Token 比例拆账。 |
| 空 provider 无自动规则 | 未指派时只能显示“未指定渠道” | 记录页/设置页提供逐条指派；模型维度按工具拆行避免误导。 |
| 用户改名导致渠道键变化 | 趋势选中项失效 | 前端选项找不到键时显示不可得并允许回到“全部”；不静默改选。 |
| 归一化过度折叠模型族 | 错误同价 | 只剥列出的日期/版本/推理档位尾；不剥 `max/chat/instruct/base`；前缀回退要求噪声后缀；跨模型族必须用户授权。 |
| 最新 release_date 条目无价/条件价 | 该归一化模型整体未估价 | 严格遵循最新优先；同日期用有价/非条件优先；不拿其他 provider 价格补洞。 |
| 手动覆盖全局生效 | 不能按 provider 给同模型不同覆盖价 | 符合“同归一化模型全渠道同价”；用户如确有不同渠道价，只能通过显式参考模型规则区分模型族，不能拆同模型。 |
| 覆盖价与自动同步冲突 | 同步覆盖用户值 | 刷新只写快照/状态表，覆盖表独立；测试锁住刷新后覆盖不变。 |
| 别名没有版本/撤销 | 误改无法回滚 | 别名是展示层、可再次编辑；计费规则仍走带版本配置；不把别名塞进 billing revision。 |
| 渠道解析性能 | 大范围查询逐行解析 | 配置一次加载、HashMap 查找；目录归一化索引进程级缓存；前缀回退只在 miss 时线性扫描约 9000 条。 |
| 自省锁钉子随改动漂移 | 后端测试红 | 每个后端任务按 §8.3 同步；Task 2 同步文件计数与分类。 |
| 新表迁移不幂等 | 旧库升级失败 | `IF NOT EXISTS` + 连续 migrate 测试；不改旧表。 |
| i18n 键集通过但英文值漂移 | 英文界面不完整 | Task 11 逐键核对新文案，不只跑 `check:i18n`。 |
| 记录页仍显示原始标签 | 用户以为别名未生效 | 浮窗显示当前别名；C/D/设置页按别名分组；文档说明记录页行名保留原始证据。 |
| 归档 profile 同名 | 显示名与别名冲突 | 渠道键前缀区分 `bp:`/`provider:`；UI 可显示类型标记；不合并不同语义。 |

---

## 11. 提交边界建议

按功能域小提交，禁止 `git add .`；每次只暂存本任务明确路径。

1. `feat(usage): extend v2 contracts for provider aliases and price overrides`（Task 1）
2. `feat(usage): persist provider aliases and resolve display channels`（Task 2）
3. `feat(usage): normalize model prices and store manual overrides`（Task 3）
4. `feat(usage): group token queries by display channel`（Task 4）
5. `feat(usage): auto-match equivalent costs and extend cost csv`（Task 5）
6. `feat(usage): expose alias and override ipc with mocks`（Task 6）
7. `feat(usage): manage provider aliases in settings and records`（Task 7）
8. `feat(usage): manage manual price overrides`（Task 8）
9. `feat(usage): show display channels in trend distribution and cost cards`（Task 9）
10. `test(usage): cover provider alias and automatic pricing acceptance`（Task 11 测试）
11. `docs(usage): document provider aliases and automatic pricing`（Task 10 文档）

中间提交只是工作记录，不代表用户验收；最终仍需一次性交付用户。

---

## 12. 执行记录（实施后由主 Agent 追加）

> 本轮只写计划，未执行任何任务。实施时按任务追加：任务编号、HEAD、红测命令与真实失败原因、绿测命令与退出码、Review 规格轮/质量轮结论、修复复审、提交、未解决问题、自省锁轮次与实测 `(lines, chars)`。不得预填通过，不得另建本功能证据文件。

### 2026-10-19 执行记录

- **基线**：`85c767b5`（spec/plan 提交）→ 实施提交 `ab9cd4e3`…`00bc333f` → 复核修复 `625b9633`、`0bf71274`、`933b3e4d`、`a32917f3`、`dc3c7f45`、`9cc74ebd`、`c7ee0c6f`、`7a78a913`。
- **任务提交**：Task 1 `ab9cd4e3`；Task 2 `60ebcb05`；Task 3 `a0e6b6ad`；Task 4 `7cdb86c0`；Task 5 `e4d9c9cf`；Task 6 `401fc0ef`；Task 7 `50110d5a`；Task 8 `42c03be5`；Task 9 `fa863261`；Task 10 `d867799e`；Task 11 `00bc333f`。各任务 TDD 红/绿原因、定向测试计数与偏离见各执行代理报告；Task 10 因依赖任务已完成，红测未成立（已如实登记）。
- **自省锁**：面尺寸第 29→37 轮同步，最终 `11409 行 / 253321 字符`；文件数 33（新增 `v2/channel.rs`）、27 测试模块区 / 6 空区。
- **迁移**：六张 V2 表幂等；旧四表逐值不变（`database::migration` 11 项）。
- **最终回归（HEAD `7a78a913`，代码状态 `c7ee0c6f`）**：
  - 默认 `pnpm vitest run` 连续 2 次：157 文件 / 1647 通过 / 0 失败 / 5 跳过（Windows `maxWorkers: 4`）。
  - `pnpm check`：通过（format/lint/i18n 1576 键/error-codes/build）。
  - `cargo test --no-fail-fast -j 4`：lib 2543 通过 / 5 失败 / 43 ignored；集成 151 通过 / 0 失败 / 12 ignored。5 个失败与基线完全一致，均为 `with_home` 未在 Windows 重定向 USERPROFILE 的预存环境缺陷。
  - `cargo clippy --all-targets -j 4 -- -D warnings`、`cargo fmt -- --check`：通过。
  - `real_db_isolation_test` 通过；真实 `~/.tuvis/tuvis.db` mtime 前后一致。
- **独立复审**：后端 / 前端 / 文档三路，均无 P0/P1；P1（记录页别名命中预览、文档旧称）与主要 P2（证据保留、前缀回退、conditional 同价、trim、迁移六表、趋势加载态、覆盖价可达性、C 卡证据提示）已修复并复核关闭。
- **已知偏离（不阻塞合并）**：别名规则 `updatedAt` 未进入 wire/UI；dev mock 未完整模拟后端别名/归一化语义；`bp:` 与 `provider:` 同显示名时无视觉区分；别名预览查询失败时的错误态分支待补；工具/项目维度不展示 provider 证据（已入文档）。同原始 provider 串无法拆分已作为已知限制写入规格与用户文档。
