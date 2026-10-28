# 用量配置页与费用表重构执行计划（配置两段式 · 精选价目来源 · 小时明细计费 · D 卡单表）

> 日期：2026-10-20 · 状态：待执行（只写计划，未改实现）
> 产品规格：`docs/engineering/specs/2026-10-17-usage-dashboard-v2-design.md`（§7.2 供应商别名与定价自动匹配仍然有效）。
> 既有计划**只读参考**：`docs/engineering/plans/2026-10-19-provider-alias-pricing-automation-plan.md`（已全部执行完毕，其产物是本计划的改动基线）。**本计划不修改它**；两者冲突时，以本计划 + 用户 2026-10-20 确认的八条需求为准。
> 代码根：`E:\LLMproject\Github\mam-worktree-usage-v2`；分支 `usage-dashboard-v2`，基线提交 `430b2adc`（工作区干净）。实施前先 `git rev-parse HEAD` 复核。
> 最高约束：`docs/MASTER-PLAN.md`。本计划不得修改宪法；发现冲突立即停下报告用户。

---

## 0. 冻结口径（用户 2026-10-20 确认，不得自行更改）

> **2026-10-20 第二轮修订已覆盖本节第 1–4、6 条**（删除 A 段、条目内同时改供应商名/模型名、默认 Top10、
> 来源优先级改为直营官方 > Ollama Cloud > OpenRouter、原生人民币价/当日汇率、D 卡卡片化与默认展开）。
> 以 §13「追加修订」为准；本节保留为初版决策记录。

1. **配置页形态**：加宽弹窗（约 1100px），**两段式**（A 段供应商显示名 + B 段模型条目）；去掉固定 min-w，**禁止横向滚动**；条目多时只纵向滚动。
2. **A 段供应商显示名**：列出账本**所有已观测**原始 provider（含历史出现、当前范围无用量；空 provider 按「工具+模型」单列）；逐个编辑自定义名，**未编辑 = 原名**。**渠道 = 供应商别名**（统一概念）；现有 billingProfile/绑定规则保留为「**高级覆盖**」，**默认隐藏**。
3. **B 段模型条目三行**：
   - row1：原始供应商 + 原始模型（只读）；
   - row2：自定义供应商名（只读，来自 A 段映射）+ 自定义模型名（可编辑，可从原始模型名复制）；**模型显示名键 = (显示供应商, 原始模型)**；
   - row3：计价身份（目录 provider/model，**仅下拉/搜索选择，无自由输入**）；默认自动匹配并显示匹配结果，可手动覆盖；悬停显示价格表。
4. **价目来源**：默认只展示**官方 + OpenRouter**；提供「显示全部 N 个来源」开关（当前目录 N=232，动态取自快照）；自动匹配**默认只在精选来源内**；**官方优先、OpenRouter 兜底**。官方集合（12 家）：`openai / anthropic / google / deepseek / zhipuai / zhipuai-coding-plan / moonshotai / alibaba / xai / mistral / meta / ollama-cloud`。
5. **费用计算粒度**：7d/30d/自定义（上限 31 天，既有 `MAX_CUSTOM_SPAN_DAYS=31` 不变）的费用查询改为**读取 `usage_detail` 小时明细并按日聚合**；dsh 因此可全量计价；若明细缺失（用户调低 detailRetentionDays 或清理）**回退日聚合**，dsh 保持 output-only。5h/当日不变。
6. **D 卡表格**：单表列 = 自定义供应商名/自定义模型名 | 估价 | 总 Token | 缓存命中率 | 计费详情；每行展开为「四行三列」小表（行 = 新增输入/缓存命中/输出/合计；列 = Token 用量 / 成本明细（金额）/ 单价（如 $4.77/1M tokens））；合计行 Token+金额合计、**单价留空**；小字只留归一化模型 + 快照获取时间；**去掉价格来源/模型发布日**；不准确原因一行用 `·` 分割。保留「按渠道/按模型」作为**排序/分组切换**（默认按渠道）。表头加 USD/CNY 快捷切换（既有 displayCurrency + 设置页汇率保留）。
7. **其他**：删除 A 卡「缓存写入（解释）」整块；全局计量维度统一「新增输入 / 缓存命中 / 输出 / 合计」；缓存写仅在有数据时显示（费用明细已实现隐藏空行，A 卡随整块删除）。
8. **不动清单**：不改 Collector/游标/Token 口径/历史账本；不改 `docs/MASTER-PLAN.md`；**不新增错误码**；i18n zh/en 同步；迁移幂等；`services/usage` 自省锁钉子同步。

**本期不做**：C 卡接入自定义模型名（后续可选）；费用 CSV 加列（列集不变，仅行值随计费粒度变化）；全局费用合计/渠道小计；按时间区分同原始串双订阅（沿用既有已知限制）；旧 `providerMapRules` 恢复。

---

## 1. 设计决策与权衡

| 决策 | 选择 | 权衡 / 理由 |
| --- | --- | --- |
| 模型配置存储键 | 按**观测身份**存（`provider_raw` / 空 provider 的 `source_id` + `model_raw`），**不**直接用显示名串做主键 | 显示名会随 A 段改名漂移，按显示名存储会导致改名后配置成孤儿、需级联迁移。身份键稳定，展示键（显示供应商, 原始模型）在读取时经别名解析派生。代价：「同显示名合并多个原始 provider」时同一展示键可能对应多行配置，需要合并规则（见 §3.5，分歧时回退、编辑时批量写）。 |
| A 段存储 | 复用既有 `usage_provider_alias` 表与 upsert/delete 命令，**不加表** | 别名语义（渠道=供应商别名）与表结构完全吻合；未编辑=无规则。代价：A 段 UI 从「规则列表」变为「观测条目列表 + 就地编辑」，纯前端改造。 |
| 计价身份覆盖 | 新列在 `usage_model_config`（`pricing_provider`/`pricing_model` 成对可空），不建独立表 | 显示名与计价身份同属「一个模型条目」的配置，同键同行避免两表 join 与两份 upsert。代价：一条命令只改一半字段时必须保留另一半（DAO 按 identity 整行读改写）。 |
| 费用明细回退粒度 | **整段回退**（range 内 usage_daily 有任一天 usage_detail 无覆盖 → 整段走日聚合），不做逐日混合 | 行内可信语义保持单一、可解释（避免同一行一半全价一半 output-only）；检测只需两个索引 `DISTINCT day_key` 查询。代价：31 天窗口内只要有残缺天，dsh 整段退回 output-only（保守、诚实）。 |
| dsh 明细可信 | `CostBucketProvenance::Detail` 时全部源 `trusted=true`（含 dsh）；`Daily` 时维持 `basis_trusted` | 明细行四桶是归一化后的互斥规范桶（落库前已消化语义差异），与 Hour 档同源同口径。代价：需用 dsh 真实形态夹具锁定「明细可信」判据，防止误信。 |
| 精选来源索引 | `PriceCatalog` 新增 `normalized_curated`（解析期建好），自动匹配走它；`normalized`（全量）保留 | 运行期过滤会在「最新优先去重已选中非精选条目」时失效——必须在解析期就按精选集合独立去重。代价：内存翻倍（约 9000 模型 × 2 索引，进程级缓存已覆盖冷启动代价）。 |
| 自动匹配作用域 | 恒为 Curated（官方 > OpenRouter）；「显示全部」只影响目录搜索可见范围，不改变自动匹配 | 用户要非精选来源价时，在 B 段 row3 显式选择（= 计价身份覆盖），是显式用户意志，优先级自然高于自动匹配。代价：非精选来源模型默认未估价——符合「默认精选」的冻结口径，缺口由显式选择补。 |
| 计价优先级链 | 覆盖价（四价）> `PricingRule`（高级覆盖参考价规则）> **计价身份覆盖**（新）> 自动匹配（精选） | 价格直给最显式；`PricingRule` 是带版本/预览/撤销的高级显式规则且身份粒度更细（spec §3.2「匹配范围更窄者优先」）；计价身份是渠道+模型级轻量显式选择；自动匹配是推断。 |
| D 卡展示字段 | `CostRow` 追加 `display_model_name`，由后端按行成员配置解析 | 与 `displayProvider` 同一模式（展示层解析、不回写账本），前端不用自行 join 配置。代价：行成员配置分歧（>1 个不同自定义名）时置 null 回退原始模型名（诚实降级）。 |
| D 卡移除字段 | 价格来源/模型发布日仅从 **UI 移除**；wire 字段与 CSV 列保留 | CSV 消费方与 `PriceOverrideManager` 仍需该信息；减少破坏面。 |
| 配置弹窗替换面 | 新 `UsageConfigDialog` 取代 `ProviderAliasManager` 的设置页挂载；BillingProfilesManager/PriceOverrideManager 移入「高级覆盖」折叠区；`PricingStatusPanel` 留设置页原位 | 定价目录状态不属于绑定规则，不动可减少测试波及。代价：`ProviderAliasManager.tsx` 与其测试退役删除（records 页 `ProviderAliasDialog` 独立保留）。 |
| 计量维度文案 | 仅改 i18n **值**（zh「新增输入/输出」、en "New input"/"Output"），不改字段名/CSV 列/口径 | 口径冻结（需求 8）与文案统一（需求 7）并行不悖；分享图/文本摘要引用同键自动跟随。 |
| 弹窗宽度 | `max-w-[min(1100px,calc(100vw-2rem))]`，无固定 min-w | 1040×720 基准窗口自动收窄到 ≈1008px；内部布局全部 `min-w-0` + 换行，横向永不滚动。 |

---

## 2. 数据模型与接口变更

> 命名均为**计划建议**：实现时可调整以贴合现有风格，但必须保持行为契约、迁移幂等与调用方一致；偏离须在执行记录登记并同步所有受影响文件。

### 2.1 新增 SQLite 表（写入 `src-tauri/src/database/schema.rs`；`migration::migrate` 幂等建表）

```sql
-- 模型条目配置（2026-10-20 计划）：自定义模型显示名 + 计价身份覆盖。
-- 按**观测身份**存储：非空 provider 用 (provider_raw, model_raw)（source_id 为 NULL，跨工具生效）；
-- 空 provider 条目用 (source_id, model_raw)（provider_raw 为空串）。展示键（显示供应商, 原始模型）
-- 在读取时经 usage_provider_alias 解析派生——别名改名/删除不需要迁移本表。
-- display_name 为 NULL = 未自定义（显示原始模型名）；pricing_* 成对为 NULL = 未覆盖（走高级规则/自动匹配）。
CREATE TABLE IF NOT EXISTS usage_model_config (
    id               TEXT PRIMARY KEY,
    provider_raw     TEXT NOT NULL,
    source_id        TEXT,
    model_raw        TEXT NOT NULL,
    display_name     TEXT,
    pricing_provider TEXT,
    pricing_model    TEXT,
    updated_at       INTEGER NOT NULL,
    CHECK (
        (provider_raw <> '' AND source_id IS NULL)
        OR (provider_raw = '' AND source_id IS NOT NULL)
    ),
    CHECK ((pricing_provider IS NULL) = (pricing_model IS NULL)),
    CHECK (pricing_provider IS NULL OR (pricing_provider <> '' AND pricing_model <> ''))
);
-- SQLite 表级 PRIMARY KEY 不折叠 NULL：必须用表达式唯一索引锁身份键。
CREATE UNIQUE INDEX IF NOT EXISTS idx_usage_model_config_identity
    ON usage_model_config(provider_raw, COALESCE(source_id, ''), model_raw);
```

- 迁移幂等：连续 `migrate` 两次不报错；旧六张 V2 表逐值不变；`database::migration` 的 DROP/存在性断言扩展到**七张** V2 表。
- 不改 `usage_provider_alias` / `usage_price_override` / billing / pricing 既有表。

### 2.2 Rust wire 类型（`src-tauri/src/services/usage/v2/model.rs`）

统一 `serde(rename_all = "camelCase")`，`Option` 显式序列化 null：

```rust
/// 观察条目（A 段与 B 段的共同数据源；全历史，来自 usage_daily，与查询范围无关）。
pub struct ObservedModelEntry {
    pub provider_raw: Option<String>,      // None = 空 provider 条目（此时 source_id 必有）
    pub source_id: Option<UsageSourceId>,  // 仅空 provider 条目非空
    pub model_id: String,                  // 空串 = 未知模型
    pub display_provider: Option<String>,  // 别名解析结果；None = 未指定渠道（空 provider 未指派）
    pub display_model_name: Option<String>,// 该条目已保存的自定义模型名
    pub pricing_provider: Option<String>,  // 计价身份覆盖
    pub pricing_model: Option<String>,
    pub total_tokens: i64,                 // 全历史 requestTotal+output，用于排序
}

pub struct ModelDisplayNameRequest {
    pub provider_raw: Option<String>,
    pub source_id: Option<UsageSourceId>,
    pub model_id: String,
    pub display_name: Option<String>,      // None / trim 后空串 = 清除（回原名）
}

pub struct PricingIdentityRequest {
    pub provider_raw: Option<String>,
    pub source_id: Option<UsageSourceId>,
    pub model_id: String,
    pub reference: Option<PricingReference>, // None = 清除覆盖（回自动匹配）
}

/// 目录搜索（带精选作用域与来源计数）。
pub struct PricingCatalogSearch {
    pub entries: Vec<PricingCatalogEntry>,
    pub provider_count: i64,               // 目录内全部 provider 数（"显示全部 N 个来源"）
}
// 既有 PricingCatalogEntry 追加：
//   pub curated_kind: Option<String>,      // "official" | "openrouter" | None=非精选（scope=all 才可能出现）

/// 目录条目价格预览（B 段 row3 悬停价格表）。费率串复用 rate_string（10^-9 定点十进制）。
pub struct PricingQuotePreview {
    pub reference: PricingReference,
    pub input: Option<String>,
    pub cache_read: Option<String>,
    pub cache_write: Option<String>,
    pub output: Option<String>,
    pub release_date: Option<String>,
    pub conditional: bool,
    pub snapshot_date: Option<i64>,        // = active 快照 fetched_at
}

/// 自动匹配结果（固定精选作用域）。
pub struct PricingAutoMatch {
    pub kind: String,                      // "exact" | "prefix" | "none"
    pub reference: Option<PricingReference>,
    pub quote: Option<PricingQuotePreview>,
}

// 既有 CostRow 追加（§3.5 解析规则）：
//   pub display_model_name: Option<String>,
```

### 2.3 TS 镜像（`src/types/usageV2.ts`）

逐字镜像 2.2：`ObservedModelEntry`、`ModelDisplayNameRequest`、`PricingIdentityRequest`、`PricingCatalogSearch`、`PricingQuotePreview`、`PricingAutoMatch`；`PricingCatalogEntry` 追加 `curatedKind: string | null`；`CostRow` 追加 `displayModelName: string | null`。

### 2.4 新增 IPC 命令（沿用 `commands/usage_v2.rs` 纪律：`async` + `spawn_blocking`，命令层不取 DB 锁、不触发采集；参数 camelCase）

| TS 包装（`src/lib/api/usageV2.ts`） | IPC 命令 | 参数 | 返回 |
| --- | --- | --- | --- |
| `usageObservedEntriesGet()` | `usage_observed_entries_get` | 无 | `ObservedModelEntry[]` |
| `usageModelDisplayNameSet(request)` | `usage_model_display_name_set` | `{ request }` | `ObservedModelEntry[]` |
| `usagePricingIdentitySet(request)` | `usage_pricing_identity_set` | `{ request }` | `ObservedModelEntry[]` |
| `usagePricingCatalogSearch(query, limit?, scope)` | `usage_pricing_catalog_search` | `{ query, limit, scope }`（`scope: "curated"\|"all"`，缺省 `"curated"`） | `PricingCatalogSearch` |
| `usagePricingAutoMatch(modelId)` | `usage_pricing_auto_match` | `{ modelId }` | `PricingAutoMatch` |
| `usagePricingQuoteGet(providerId, modelId)` | `usage_pricing_quote_get` | `{ providerId, modelId }` | `PricingQuotePreview \| null` |

- 注册到 `src-tauri/src/lib.rs` 的 `generate_handler!`，紧接现有 19 条 V2 命令之后（共 25 条）。
- 服务层同时提供 `*_with_conn` 测试入口与持 `DB` 锁的生产入口（既有模式）。
- **既有命令不变**：`usage_pricing_catalog`（高级覆盖参考价选择器继续用全量口径）、`usage_provider_alias_upsert/delete`（A 段复用）、覆盖价三命令、`usage_costs`/`usage_cost_export_csv`（行值随 §3.4 变化，形状不变）。

### 2.5 错误与校验（不新增错误码）

- 校验失败 `usage-settings-invalid`；SQLite 失败 `usage-db-failed`；自定义范围非法沿用 `usage-range-invalid`。固定 detail 前缀：
  - `model config identity is empty`
  - `model config display name is empty after trim`（仅当请求意图为设置却给了空白）
  - `model config pricing reference is incomplete`
  - `model config entry not observed`（写入目标身份不在观测集——防幽灵行）
  - `pricing catalog scope is invalid`
- `display_name` / `reference` 传 null = 清除，属合法操作，不报错。

---

## 3. 后端改造要点

### 3.1 观察条目查询（新文件 `src-tauri/src/services/usage/v2/model_config.rs`）

- 数据源：`SELECT source_id, provider, model, SUM(request_total), SUM(output) FROM usage_daily GROUP BY source_id, provider, model`（日聚合永久保留 ⇒ 天然覆盖「含历史出现、当前范围无用量」）。usage 总开关关闭 ⇒ 空数组（与 `identities_with_conn` 一致）。
- 每行构造观测身份，经 `usage_provider_alias` 解析 `display_provider`（复用 `channel.rs` 的别名映射；不触 billing attribution）；join `usage_model_config` 填 `display_model_name / pricing_*`；`total_tokens = SUM(request_total) + SUM(output)`。
- 排序：`display_provider` 升序（None 排最后）→ `model_id` 升序。
- 空模型（`model_id=""`）照常返回，前端显示「未知模型」。
- 写入命令：按 identity 键 upsert（查 `idx_usage_model_config_identity`）；写入前校验目标身份**确在观测集**（防幽灵行）；`usage_observed_entries_get` 同一实现返回刷新后的全量列表。

### 3.2 精选来源与目录服务（`pricing.rs`）

```rust
pub const CURATED_OFFICIAL_PROVIDERS: [&str; 12] =
    ["openai","anthropic","google","deepseek","zhipuai","zhipuai-coding-plan",
     "moonshotai","alibaba","xai","mistral","meta","ollama-cloud"];
pub const CURATED_OPENROUTER_PROVIDER: &str = "openrouter";
pub fn is_curated(provider_id: &str) -> bool;
```

- `PriceCatalog` 追加 `normalized_curated: BTreeMap<String, NormalizedQuote>` 与 `provider_count: usize`。
- 解析期建精选索引：同一归一化 ID 的报价先在**官方 12 家**内按既有去重链（`release_date` 最新 → 有价 → 非条件 → 字典序）选一；官方无候选则**OpenRouter** 内同规则选一；两者皆无 ⇒ 不进精选索引。`normalized`（全量）构建逻辑不变。
- 匹配作用域：`enum MatchScope { Curated, All }`；`match_model_scoped(catalog, raw, scope)`。Curated 走 `normalized_curated`、All 走 `normalized`；**前缀回退在所选作用域索引内**，规则不变（候选唯一或同价）。`cost_query` 自动匹配与 `usage_pricing_auto_match` 固定 `Curated`。
- `catalog_entries_with_conn` 保留不动（全量、高级覆盖在用）。新 `catalog_search_scoped_with_conn(conn, query, limit, scope)`：
  - `curated`：只返回 `is_curated(provider_id)` 的条目；`all`：现行为。条目带 `curated_kind`（official/openrouter/None）。
  - 返回 `PricingCatalogSearch { entries, provider_count }`（provider_count 恒为全量目录 provider 数）。
- `quote_preview_with_conn(conn, provider_id, model_id)`：查 `quotes` 精确键 ⇒ `PricingQuotePreview`（四价 rate_string、release_date、conditional、快照 fetched_at）；无条目 ⇒ `Ok(None)`。无 active 快照 ⇒ `Ok(None)`（前端区分「无快照」）。
- `auto_match_with_conn(conn, model_id)`：`match_model_scoped(Curated)` ⇒ `PricingAutoMatch`（kind + reference + quote 预览）。

### 3.3 计价优先级链（`cost_query.rs::resolve_row_pricing` 调整为四级）

1. **四价手动覆盖** `usage_price_override`（既有，逐分量最高优先，`pricingBasis=userOverride`）；
2. **`PricingRule`**（billing config 高级覆盖参考价规则；identity 级精确/前缀；走 `quotes` 精确键）；
3. **计价身份覆盖**（新）：按行 identity 查 `usage_model_config.pricing_*` → `quotes` 精确键取价；目录无该条目 ⇒ `noPrice`（保留候选 reference）；命中后**不再做**自动匹配与前缀回退；
4. **自动匹配**（归一化，`MatchScope::Curated`，官方 > OpenRouter；前缀回退在精选索引内）。

规则冲突（同级多 reference）语义不变（`pricingReferenceAmbiguous`）。`pricingBasis` 语义不变：计价身份覆盖选中目录条目后，费用仍来自目录费率 ⇒ `baseCurrent`（不是 userOverride——该标记只留给四价覆盖）。

### 3.4 费用查询小时明细聚合与回退（`cost_query.rs` + 复用 `v2/query.rs` 归因）

- 新增内部枚举 `enum CostBucketProvenance { Detail, Daily }` 与 `cost_snapshot_with_conn(conn, range, now) -> (Vec<AttributedRow>, CostBucketProvenance)`：
  - `granularity == Hour`（5h/当日）：现路径（usage_detail 逐行），`Provenance::Detail`；
  - `granularity == Day`（7d/30d/自定义≤31 天）：**覆盖检测**——`SELECT DISTINCT day_key FROM usage_daily WHERE day_key BETWEEN ? AND ?` 与同谓词的 `usage_detail` 查询（均走 `idx_usage_daily_day` / `idx_usage_detail_day`）；daily 天集 ⊆ detail 天集 ⇒ `Provenance::Detail`，否则 `Provenance::Daily`（整段回退）。
  - Detail 路径取数：`SELECT day_key, source_id, provider, provider_kind, model, SUM(input_fresh), SUM(cache_read), SUM(cache_write), SUM(output), SUM(request_total), SUM(requests) FROM usage_detail WHERE day_key BETWEEN ? AND ? GROUP BY day_key, source_id, provider, model`，映射为 `LedgerRow { bucket_key=day_key, session_id="", project_key="", project_label="", user_est=None, … }`（费用不消费 project/user_est，置空是安全降维；SQL `SUM` 对全 NULL 的 `user_est` 天然返 NULL，如将来需要可直加该列）。随后走既有 `attribute_rows`（Day 粒度桶界归因）。
  - Daily 路径：现 `snapshot_with_conn` 行为。
- `costs_with_conn` / `cost_csv_with_conn` 切到 `cost_snapshot_with_conn`；**`snapshot_with_conn` 签名与行为不变**（billing preview 不受影响）。
- 可信判据：`trusted = provenance == Detail || basis_trusted(source, granularity)`。即 Detail 下 dsh 全量计价（四桶）；Daily 下维持 dsh output-only（`safe_components_output_only` + `untrustedInputBuckets` reasons）。其余源两 provenance 均 trusted。
- reasons 汇总语义不变（组内 BTreeSet 并集）；Daily 回退时 dsh 行照旧出现 `untrustedSource`/`untrustedInputBuckets`，明细路径不出现。
- 既有契约保留：无 total/无渠道小计、缺价 ≠ 0、`unpriced_tokens` 一致性校验、行键长度前缀 tuple（行键组成不变——provenance 不进行键：同一 range 只有一个 provenance，不会混淆）。

### 3.5 模型显示名解析进 `CostRow`（`cost_query.rs` 分组阶段）

- `GroupAcc` 追加 `display_model_names: BTreeSet<String>`（成员 identity 的 `usage_model_config.display_name` 非空才收）。
- 输出 `display_model_name = (集合恰有一个元素) then 该值 else None`：全部成员一致 ⇒ 生效；无人配置或**分歧** ⇒ null（前端回退原始 `modelId`，诚实降级）。分歧是「同显示名合并多个原始 provider」的已知限制面（§10）。
- B 段写入语义与该解析规则对齐：前端对「同展示键多成员」的条目编辑时，对**全部成员 identity** 批量 upsert（收敛一致）；后端 `usage_model_display_name_set` 单身份写入不改成员（批量由前端逐成员调用）。

### 3.6 自省锁

- T2 新增 `v2/model_config.rs` ⇒ `USAGE_SOURCES` 33→34，分类断言 27/6→28/6（新文件含测试模块），`services/usage/mod.rs` 文档同步。
- T3/T4 修改 `pricing.rs`/`cost_query.rs` ⇒ 面尺寸 `(lines, chars)` 钉子按实测同步（流程见 §8.3）。

---

## 4. 前端改造要点

### 4.1 数据层、mock 与 i18n

- `src/lib/api/usageV2.ts`：新增 6 个包装（§2.4）。
- `src/lib/query/queries/usageV2.ts`：
  - `useUsageObservedEntriesQuery()` → `["usage","v2","observedEntries"]`
  - `useUsagePricingCatalogSearchQuery(query, scope, opts)` → `["usage","v2","pricingCatalogSearch",trimmed,scope]`
  - `useUsagePricingAutoMatchQuery(modelId, opts)` → `["usage","v2","pricingAutoMatch",modelId]`（`enabled` 门控）
  - `useUsagePricingQuoteQuery(providerId, modelId, opts)` → `["usage","v2","pricingQuote",providerId,modelId]`
  - 两个 set 命令成功后 `invalidateQueries({ queryKey: USAGE_KEY })`；不新增轮询。
- `src/tauri-mock.ts` + `tests/msw/tauriMocks.ts` + `src/lib/usage/mockFixtures.ts`：
  - 新增可变 mock 状态：observedEntries（含非空 provider、空 provider 的「工具+模型」条目、历史 provider 当前无用量、多工具同显示名）、模型配置读写、catalog search（curated 过滤 + `providerCount: 232`）、auto_match/quote 预览；`resetUsageV2MockState` 复位。
  - `mockUsageCosts` 行追加 `displayModelName`；补一条 dsh 明细全价行变体（D 卡新表格展示用）。
  - 两端 mock 形状一致（`usageMockParity` 锁）。
- i18n（zh/en 一次性补齐，后续 UI 任务只消费键）：
  - 新键族 `usage.config.*`：title/desc/advanced/advancedHint；`providerSection.{title,desc,raw,emptyProviderEntry,displayNamePlaceholder,mergeHint,empty,searchPlaceholder}`；`modelSection.{title,desc,rawIdentity,displayName,displayNameHint,copyRaw,pricingIdentity,pricingAutoMatched,pricingOverridden,pricingNone,clearOverride,showAllSources,curatedOnly,empty,noMatch,prefixMatched}`；`modelSection.pricePreview.{input,cacheRead,cacheWrite,output,releaseDate,conditional,snapshotDate}`；`modelSection.sources.{official,openrouter,other}`。
  - 新键族 `usage.cost.table.*`：`{colName,colAmount,colToken,colHitRate,colDetail,total,tokenAmount,unitPriceBlank,currencyToggle,reasonsLine}`（组件明细行复用既有 `usage.cost.component.*`）。
  - **删除**：`usage.grid.cacheWrite`、`usage.cacheWrite.body`、`usage.cacheWrite.hint`（zh+en 同步删）。
  - **值统一**（不改键名、不改字段/CSV 列）：zh `usage.grid.requestInput`、`usage.mini.requestInput` → 「新增输入」；`usage.grid.output`、`usage.mini.output` → 「输出」；`usage.mini.toolsBasis` → 「按工具 新增输入：」；`usage.hero.sub` → 「新增输入 + 输出（不含缓存写入与用户输入估算）」；`usage.cost.component.input` → 「新增输入」、`usage.cost.component.output` → 「输出」；en 对应 `New input` / `Output`。`usage.grid.cacheMiss`（未缓存命中输入）是 A 卡派生指标，保留现文案。
  - `usage.cost.view.channel/model` 键复用（语义已是渠道/模型）。

### 4.2 配置弹窗（新 `src/components/usage/UsageConfigDialog.tsx` + `UsageConfigProviderSection.tsx` + `UsageConfigModelSection.tsx`）

- **壳**：`DialogContent` 尺寸 `max-h-[85vh] w-full max-w-[min(1100px,calc(100vw-2rem))] overflow-y-auto`；**禁止横向滚动**（容器 `overflow-x-hidden`，行内全部 `min-w-0`/`truncate`/换行，长模型名走 title tooltip）；条目多只纵向滚。Props `{t, open?, onOpenChange?, hideTrigger?}`（与现有 manager 同构）。
- **A 段（供应商显示名）**：数据 = `useUsageObservedEntriesQuery()` 按 display 键折叠为条目行——非空 provider 按解析后显示键合并、空 provider 按「工具+模型」单列；每行 = 原始标识（只读，含「近 30 天无用量也在列」的全历史语义说明）+ 自定义名输入框（placeholder=原名；blur/Enter 且值变化即保存）。
  - 保存 = `usageProviderAliasUpsert`（非空 provider → `kind:"provider"`；空 provider → `kind:"toolModel"`）；清空已存在规则的自定义名 = `usageProviderAliasDelete`（回原名）。未编辑 = 无规则 = 原名。
  - 同一显示名被多个原始串使用时显示「将合并为一个渠道」提示（复用既有 `mergeHint` 语义）。
- **B 段（模型条目）**：同一查询按展示键（显示供应商, 原始模型）枚举；每条目三行：
  - row1（只读）：原始供应商 + 原始模型；展示键下有多个原始来源（别名合并）时列出全部去重原始 provider 与工具（证据不丢）；空模型显示「未知模型」。
  - row2：自定义供应商名（只读，A 段映射结果；未指派显示「未指定渠道」）+ 自定义模型名输入框 + 「复制原始模型名」按钮 + 清除按钮（回 null）。保存 = `usageModelDisplayNameSet`；展示键多成员时逐成员调用。
  - row3：计价身份 combobox——搜索走 `useUsagePricingCatalogSearchQuery(query, scope)`；默认 `scope="curated"`，开关「显示全部 N 个来源」（N=providerCount）切 `scope="all"`；**只可选择，无自由输入**。默认展示自动匹配结果（`useUsagePricingAutoMatchQuery(modelId)`：命中显示 `official/openrouter 来源 · exact/prefix` 徽标，未命中显示「精选来源未命中」）；已覆盖显示「已覆盖」徽标 + 清除按钮（回 null）。悬停/聚焦显示价格表浮层（`aria-describedby` + CSS hover/focus 可见、键盘可达）：四价 + 发布日 + 条件价标记 + 快照日期（`useUsagePricingQuoteQuery`）。保存 = `usagePricingIdentitySet`。
- **高级覆盖（默认隐藏）**：弹窗底部原生 `<details>`（`aria-expanded` 由 details 原生语义承担，键盘可操作），内含两个入口按钮分别打开既有 `BillingProfilesManager` 与 `PriceOverrideManager`（受控复用，`hideTrigger`）；摘要行说明「计费渠道绑定 / 参考价规则 / 四价覆盖，供高级场景使用」。**默认折叠，不在 A/B 段露出。**
- **入口切换**：`src/components/settings/UsageSection.tsx` 的「供应商别名」触发器替换为「显示与计价配置」（UsageConfigDialog）；移除设置页的 BillingProfilesManager 独立触发器（已移入高级覆盖）；`PricingStatusPanel` 原位不动。
- `src/components/usage/ProviderAliasManager.tsx` **退役删除**，`tests/usage/v2-provider-alias-ui.test.tsx` 由新 `tests/usage/v2-config-dialog.test.tsx` 取代。records 页 `ProviderAliasDialog`/`ProviderAliasFields` 不动（`ProviderAliasFields` 若被 Manager 独占则一并清理，被 Dialog 共用则保留——实施时按 import 实况定）。

### 4.3 D 卡表格重构（新 `src/components/usage/UsageCostTable.tsx` + `UsageCostDetailGrid.tsx`；`UsageCostCard` 改壳；`UsageCostRow` 退役删除）

- **单表**（CSS grid 行布局，5 列：`minmax(0,1fr) auto auto auto auto`，无横向滚动）：
  - 列 = 自定义供应商名/自定义模型名（`row.displayProvider ?? channelName` / `row.displayModelName ?? modelId`，空模型「未知模型」）| 估价 | 总 Token（fmtTokens 缩写）| 缓存命中率 | 计费详情（展开按钮 `aria-expanded`/`aria-controls`）。
  - 估价列：unpriced/null → 「未估价」；partial → 「已估价部分 $X · 覆盖 Y%」；priced → 金额。金额走 `formatMoney`（displayCurrency 换算；缺价绝不显示 $0.00）。
- **展开详情（四行三列小表）**：行 = 新增输入 / 缓存命中 / 输出 / 合计（`components.cacheWrite` 非 null 时在缓存命中与输出之间插入「缓存写入」行）；列 = Token 用量（fmtTokens）/ 成本明细（金额）/ 单价（`formatUnitPrice` + `{{value}} {{currency}} / 1M tokens`）；**合计行 = tokenUsage + estimatedUsd，单价单元格留空**；无数据的 component 显示 `—`（既有 `componentUnpriced`）。
- **小字区**：一行「归一化模型 `<normalizedModelId> · 快照 <priceUpdatedAt>`」；一行不准确原因 `row.reasons.map(reasonText).join(" · ")`；`priceOverride` 保留徽标；`pricingBasis` 文案保留一行（基准价/用户覆盖价/信息不足）。**删除**：价格来源（pricingReference）行、模型发布日（pricingReleaseDate）行、覆盖率独立展示（已并入估价列 partial 文案）。
- **分组/排序切换**：按渠道（默认）/ 按模型两态按钮（`aria-pressed`，复用 `usage.cost.view.*`）。按渠道 = 渠道组段（组头 = 显示名/i18n 键，**无金额小计**），组内行 Token 降序；按模型 = 平铺单表（行首含自定义供应商名），Token 降序。Top5/「显示更多」沿用全卡行全局降序逻辑。
- **币种快捷切换**：卡头摘要区（或表头上方）USD/CNY 两态按钮（`aria-pressed`），调 `useDisplayCurrency().setCurrency`；设置页汇率输入保留不动。
- **契约保留**：无全局费用总额、无渠道金额小计、缺价 ≠ 0、`data-testid="usage-cost-*"` 前缀沿用 + 新增 `usage-cost-table-*`。
- `src/lib/usage/costRows.ts`：`organizeCosts` 增加「平铺」模式（按模型视图直接返回单组）；渠道视图逻辑保留。

### 4.4 A 卡与文案统一

- `src/components/usage/UsageSummary.tsx`：删除 `usage-cache-write` 整个 `<details>` 块；7 行指标网格不变（键值文案由 i18n 统一生效）。`src/lib/usage/summaryV2.ts` 若 cacheWrite 字段仅剩未使用导出则一并清理。
- 分享文本/迷你条引用 `usage.mini.*`/`usage.grid.*` 键，值统一自动跟随；相关断言更新（§5 各任务测试面）。

---

## 5. 任务表

> 约定：每个任务先写失败测试并运行确认红测原因；再最小实现；绿测；自查；交独立 Review（规格符合性 → 代码质量两轮）。**每个后端任务修改 `services/usage/**` 后必须在同一任务内同步自省锁钉子**（§8.3）。修改 `src-tauri/src/services/usage/**` 前先停 `pnpm tauri:dev`（AGENTS.md 采集域纪律）。

### Task 0：只读基线与探测

- **目标**：确认工作区与基线测试，核对 models.dev provider 总数（「显示全部 N 个来源」的 mock 基准），不写实现。
- **涉及文件**：无（只读）。结论追加到本文件 §12。
- **前置**：无。
- **实现要点**：`git rev-parse HEAD`（期望 `430b2adc` 或更新，工作区干净）；确认未运行 `pnpm tauri:dev`；跑基线：`pnpm vitest run tests/usage tests/pet/usage-*`；`cargo test --manifest-path src-tauri/Cargo.toml -j 4 --lib services::usage::v2`；`cargo test --manifest-path src-tauri/Cargo.toml -j 4 --test usage_cost_test --test usage_ipc_test`；只读 `curl -k -s --max-time 20 -r 0-200000 https://models.dev/api.json | head -c 2000` 核对 provider 数量级与官方 12 家 `provider_id` 原文（不上传任何本地数据）。
- **验收**：基线命令退出码与结果记录完整；无代码改动；基线红测不得用改测试掩盖。
- **可并行**：否（第一任务）。

### Task 1：wire 契约与 TS 镜像

- **目标**：落地 §2.2/§2.3 全部新类型与字段，后续任务只依赖该契约。
- **涉及文件**：`src-tauri/src/services/usage/v2/model.rs`；`src/types/usageV2.ts`；`src-tauri/tests/usage_ipc_test.rs`（serde 断言）。
- **前置**：Task 0。
- **TDD**：`usage_ipc_test.rs` 先写 serde 形状测试（新类型字段、`curatedKind` 值域、`Option` 为 null、`CostRow.displayModelName`）；运行确认编译失败于类型缺失。
- **实现要点**：按 §2.2 逐字；不加 `skip_serializing_if`；不在 `model.rs` 内加测试模块（保持分类不变，面尺寸钉子会漂移需同步）。
- **验收**：serde 测试绿；`pnpm exec tsc --noEmit` 绿；自省锁绿。
- **验证命令**：`cargo test --manifest-path src-tauri/Cargo.toml -j 4 --test usage_ipc_test`；`cargo test --manifest-path src-tauri/Cargo.toml -j 4 --lib services::usage::collect::tests`；`pnpm exec tsc --noEmit`。
- **可并行**：否。

### Task 2：`usage_model_config` 表 + 观察条目 + 模型配置服务

- **目标**：新表 + DAO + `v2/model_config.rs`（观察条目查询、显示名/计价身份读写、display_provider 解析）。
- **涉及文件**：新增 `src-tauri/src/services/usage/v2/model_config.rs`、`src-tauri/src/database/dao/usage_model_config.rs`；修改 `src-tauri/src/database/schema.rs`、`src-tauri/src/database/migration.rs`、`src-tauri/src/database/dao/mod.rs`、`src-tauri/src/services/usage/v2/mod.rs`、`src-tauri/src/services/usage/collect.rs`（登记新文件 + 钉子）、`src-tauri/src/services/usage/mod.rs`（自省锁文档 33→34、27/6→28/6）；新增 `src-tauri/tests/usage_model_config_test.rs`。
- **前置**：Task 1。
- **TDD**：单测——观察条目聚合（含空 provider 单列、历史 provider 在列、空模型、usage 关闭早退）；display_provider 别名解析（provider 规则/toolModel 规则/未指派 None）；身份 upsert（表达式唯一索引防重、`pricing_*` 成对校验、清除语义、幽灵身份拒绝）；集成测试——`support::open_ledger_db("model_config")` 私有库建表/读写/重开读取/连续 migrate 两次七表幂等、旧六表逐值不变。
- **验收**：上述测试绿；迁移幂等；自省锁 34 文件/28 测试模块区/6 空区。
- **验证命令**：`cargo test --manifest-path src-tauri/Cargo.toml -j 4 --lib services::usage::v2::model_config`；`cargo test --manifest-path src-tauri/Cargo.toml -j 4 --test usage_model_config_test`；`cargo test --manifest-path src-tauri/Cargo.toml -j 4 --lib database::migration`；`cargo test --manifest-path src-tauri/Cargo.toml -j 4 --lib services::usage::collect::tests`。
- **可并行**：否（schema/自省锁共同面，后端任务全串行）。

### Task 3：精选来源与目录服务

- **目标**：精选常量、`normalized_curated` 索引、作用域匹配、catalog_search / quote_preview / auto_match 服务。
- **涉及文件**：`src-tauri/src/services/usage/v2/pricing.rs`；`src-tauri/src/services/usage/collect.rs`（面尺寸钉子）；`src-tauri/tests/fixtures/usage-v2/pricing/*.json`（补 curated 形态夹具：同归一化 ID 官方/OpenRouter/非精选并存）。
- **前置**：Task 2（串行，抢钉子）。
- **TDD**：精选索引构建（官方最新优先；无官方 OpenRouter 兜底；两者皆无落选）；`match_model_scoped` Curated/All 两口径；前缀回退作用域内生效；非精选条目 Curated 不命中；`catalog_search_scoped` 过滤与 provider_count；quote_preview 四价/conditional/无快照 None；**既有自动匹配用例改写为 Curated 口径**（非精选夹具从「命中」改为「不命中」并补 All 口径对照）——不得删除保守性断言。
- **验收**：定价单测绿；既有 `usage_cost_test` 若依赖非精选自动命中需同任务内更新夹具并保持红绿纪律。
- **验证命令**：`cargo test --manifest-path src-tauri/Cargo.toml -j 4 --lib services::usage::v2::pricing`；`cargo test --manifest-path src-tauri/Cargo.toml -j 4 --lib services::usage::collect::tests`。
- **可并行**：否。

### Task 4：费用查询小时明细聚合 + 优先级链 + displayModelName

- **目标**：`cost_snapshot_with_conn`（Detail/Daily + 覆盖检测回退）；dsh 明细全量计价；四级优先级接入；`CostRow.display_model_name` 解析；性能基准测试。
- **涉及文件**：`src-tauri/src/services/usage/v2/cost_query.rs`；`src-tauri/src/services/usage/collect.rs`（钉子）；`src-tauri/tests/usage_cost_test.rs`；`src-tauri/tests/fixtures/usage-v2/`（dsh 明细形态夹具）。
- **前置**：Task 2、Task 3。
- **TDD**：
  - 明细路径：7d 范围 detail 全覆盖 ⇒ 行值与逐日累加一致、dsh 行四桶全价（`pricingBasis=baseCurrent`、无 `untrustedSource`）；
  - 回退路径：删一天 detail ⇒ 整段走 daily、dsh output-only + `untrustedInputBuckets`、其余源不变；
  - 5h/当日路径回归不变；billing preview 不受影响（`snapshot_with_conn` 不动）；
  - 优先级链：四价覆盖 > PricingRule > 计价身份覆盖（含目录缺条目 noPrice）> Curated 自动匹配；计价身份命中 ⇒ `baseCurrent`；
  - `display_model_name`：单成员一致生效、无人配置 null、多成员分歧 null；
  - CSV：列集逐字不变，dsh 行金额随明细路径变化；
  - 性能基准（`#[ignore]` 或打印式）：合成 31 天 ≥10 万 detail 行跑 `costs_with_conn`，打印行数/耗时并断言正确性（不断言墙钟上限，数据记入 §12）。
- **验收**：费用/CSV 测试绿；回退开关行为正确；基准数据已记录。
- **验证命令**：`cargo test --manifest-path src-tauri/Cargo.toml -j 4 --test usage_cost_test`；`cargo test --manifest-path src-tauri/Cargo.toml -j 4 --lib services::usage::v2::cost`；`cargo test --manifest-path src-tauri/Cargo.toml -j 4 --lib services::usage::v2::query`；`cargo test --manifest-path src-tauri/Cargo.toml -j 4 --lib services::usage::collect::tests`。
- **可并行**：否。

### Task 5：IPC + 前端数据层 + mock + i18n

- **目标**：6 条新命令暴露；hooks；mock 状态与夹具；全部 zh/en 键（含删除与值统一）。
- **涉及文件**：`src-tauri/src/commands/usage_v2.rs`、`src-tauri/src/lib.rs`、`src-tauri/tests/usage_ipc_test.rs`；`src/lib/api/usageV2.ts`、`src/lib/query/queries/usageV2.ts`、`src/tauri-mock.ts`、`tests/msw/tauriMocks.ts`、`src/lib/usage/mockFixtures.ts`、`src/i18n/locales/zh.json`、`src/i18n/locales/en.json`、`tests/usage/v2-api.test.tsx`、`tests/usage/v2-queries.test.tsx`、`tests/usage/usageMockParity.test.ts`。
- **前置**：Task 1–4。
- **TDD**：v2-api 六命令名/参数/返回形状逐字断言（旧 19 条不回归）；v2-queries 四个新 hook 的 queryKey 与 `enabled` 门控、写后 `USAGE_KEY` 失效、读请求不触发 `usage_collect`；usageMockParity 两端形状一致；usage_ipc_test 命令恰好注册一次、不取 DB 锁不触发采集。
- **实现要点**：i18n 按 §4.1 全量（新增 + 删除 + 值统一一次做完）；`pnpm check:i18n` 键集一致。
- **验收**：API/hooks/mock/IPC 测试绿；i18n 键集一致；错误码门禁通过。
- **验证命令**：`pnpm vitest run tests/usage/v2-api.test.tsx tests/usage/v2-queries.test.tsx tests/usage/usageMockParity.test.ts`；`cargo test --manifest-path src-tauri/Cargo.toml -j 4 --test usage_ipc_test`；`pnpm check:i18n`；`node scripts/check-error-codes.mjs`。
- **可并行**：否（后续 UI 任务共同前置）。

### Task 6：配置弹窗（A/B 段 + 高级覆盖折叠 + 入口切换）

- **目标**：两段式加宽弹窗落地；设置页/D 卡之外的入口切换；ProviderAliasManager 退役。
- **涉及文件**：新增 `src/components/usage/UsageConfigDialog.tsx`、`src/components/usage/UsageConfigProviderSection.tsx`、`src/components/usage/UsageConfigModelSection.tsx`、`tests/usage/v2-config-dialog.test.tsx`；修改 `src/components/settings/UsageSection.tsx`；删除 `src/components/usage/ProviderAliasManager.tsx`、`tests/usage/v2-provider-alias-ui.test.tsx`（`ProviderAliasFields.tsx` 按 import 实况处置）。
- **前置**：Task 5。
- **TDD**：A 段——观测条目全历史在列（含当前无用量）、空 provider 按「工具+模型」单列、未编辑=原名（placeholder）、保存/清除走正确命令、同名合并提示；B 段——三行结构、row1 原始证据、row2 只读显示名 + 自定义模型名 + 复制按钮 + 清除、row3 仅下拉选择（无自由输入）、默认自动匹配徽标、覆盖/清除、显示全部开关切换 scope、悬停价格表内容、展示键多成员批量写；高级覆盖默认折叠（内容不可见、可展开）；弹窗无横向滚动（容器 class 断言 + 长名截断）、键盘可达（details/summary、aria-expanded）。
- **实现要点**：不修改 locale；`BillingProfilesManager`/`PriceOverrideManager` 受控复用不改内部；records 页浮窗不动。
- **验收**：新测试绿；`tests/pet/usage-settings.test.tsx`、`tests/usage/v2-cost-settings.test.tsx`、`tests/usage/v2-pricing-override.test.tsx` 更新后绿（入口位置变化）。
- **验证命令**：`pnpm vitest run tests/usage/v2-config-dialog.test.tsx tests/pet/usage-settings.test.tsx tests/usage/v2-cost-settings.test.tsx tests/usage/v2-pricing-override.test.tsx`；`pnpm exec tsc --noEmit`。
- **可并行**：是（与 Task 8；文件不重叠）。

### Task 7：D 卡表格重构

- **目标**：单表 + 展开小表 + 分组/排序切换 + 币种快捷切换；UsageCostRow 退役。
- **涉及文件**：新增 `src/components/usage/UsageCostTable.tsx`、`src/components/usage/UsageCostDetailGrid.tsx`、`tests/usage/v2-cost-table.test.tsx`；修改 `src/components/usage/UsageCostCard.tsx`、`src/components/usage/UsageCostRow.tsx`（删除）、`src/lib/usage/costRows.ts`、`tests/usage/v2-cost-card.test.tsx`、`tests/usage/v2-page.test.tsx`、`tests/usage/v2-display-currency.test.tsx`、`tests/usage/v2-cost-export.test.tsx`（若依赖行 DOM）。
- **前置**：Task 5、Task 6（manage 按钮改开 `UsageConfigDialog`，单向 import）。
- **TDD**：主表五列内容与顺序；展开小表四行三列（cacheWrite 有数据才出现、合计行单价空）；缺价「未估价」非 $0；partial 覆盖文案并入估价列；小字只含归一化模型 + 快照时间；无价格来源/发布日；reasons `·` 一行；按渠道默认分组无小计、按模型平铺含供应商名；USD/CNY 切换联动金额与单价；无全局合计字段；长模型名截断无横向滚动。
- **实现要点**：`organizeCosts` 加平铺模式；`UsageCostCard` 保留加载/错误/启用定价/stale/Top5 逻辑，渲染体换新表；manage 按钮目标替换。
- **验收**：新测试与更新后的 page/currency/export 测试绿；`tests/pet/usage-cards.test.tsx` 回归绿。
- **验证命令**：`pnpm vitest run tests/usage/v2-cost-table.test.tsx tests/usage/v2-cost-card.test.tsx tests/usage/v2-page.test.tsx tests/usage/v2-display-currency.test.tsx tests/usage/v2-cost-export.test.tsx tests/pet/usage-cards.test.tsx`；`pnpm exec tsc --noEmit`。
- **可并行**：否（依赖 Task 6 的组件；与 Task 8 可并行——文件不重叠时按批次表执行）。

### Task 8：A 卡缓存写入块删除

- **目标**：删除「缓存写入（解释）」整块；计量维度值统一随 Task 5 i18n 生效后的断言更新。
- **涉及文件**：`src/components/usage/UsageSummary.tsx`（`src/lib/usage/summaryV2.ts` 如有死代码一并清）；`tests/usage/v2-summary.test.tsx`；`tests/pet/usage-export-text.test.tsx`、`tests/pet/usage-en-surfaces.test.tsx`、`tests/pet/usage-minibar.test.tsx`（值统一断言）。
- **前置**：Task 5。
- **TDD**：v2-summary 断言 `usage-cache-write` 不存在、7 行网格与 hero 不变；export-text/en-surfaces/minibar 断言新文案（新增输入/输出/New input/Output）。
- **验收**：上述测试绿；`tests/pet/usage-cards.test.tsx`、`tests/pet/usage-contract-parity.test.tsx` 回归绿。
- **验证命令**：`pnpm vitest run tests/usage/v2-summary.test.tsx tests/pet/usage-export-text.test.tsx tests/pet/usage-en-surfaces.test.tsx tests/pet/usage-minibar.test.tsx tests/pet/usage-cards.test.tsx`；`pnpm exec tsc --noEmit`。
- **可并行**：是（与 Task 6 文件不重叠；与 Task 7 也不重叠）。

### Task 9：文档同步

- **目标**：双语文档反映本轮用户可感知变更。
- **涉及文件**：`README.md`、`README.en.md`、`docs/product/manual/zh/11-usage.md`、`docs/product/manual/en/11-usage.md`、`CHANGELOG.md`。
- **前置**：Task 6–8。
- **实现要点**：只写本轮增量——配置弹窗两段式与高级覆盖、自定义模型名、计价身份选择、精选价目来源与「显示全部」、7d/30d 明细计费与 dsh 全价（含回退说明）、D 卡新表格与币种切换、A 卡缓存写入块移除与「新增输入」口径措辞；不改 MASTER-PLAN；不改 capabilities.json/能力矩阵（无多终端能力面变化）。
- **验收**：`pnpm check` 绿；文档无旧口径残留（「请求输入/产出」措辞核对）。
- **验证命令**：`pnpm check:i18n`；`git diff --check`。
- **可并行**：否（依赖 UI 最终形状）。

### Task 10：集成验收 + 全量回归

- **目标**：跨模块验收测试；迁移/自省锁/错误码/i18n/clippy/fmt/全量回归收口；独立 Review 两轮。
- **涉及文件**：`tests/usage/v2-acceptance.test.tsx`（扩充）；受影响的既有断言；`src-tauri/src/services/usage/collect.rs`（最终钉子）；`src-tauri/src/database/migration.rs`（七表幂等断言）。
- **前置**：Task 0–9。
- **TDD/验收**：
  - acceptance 扩充：A 段改名 → B 段展示键与 D 卡行名联动、Token 总量不变；空 provider 指派后 B 段可配；计价身份覆盖生效且高于自动匹配、低于 PricingRule；7d 费用 = 明细逐日累加、删明细后回退日聚合（dsh output-only）；D 卡展开合计 = 各分项和；币种切换不改导出 CSV（USD）；无全局金额。
  - 自省锁最终对账（文件数/分类/面尺寸实测）；迁移七表；错误码无新增；i18n 逐键核对英文值（不只看键集）。
  - 最终独立 Review：规格符合性轮 + 代码质量轮；问题退回修复复审。
- **验收**：全部命令退出 0；已知边界落档；一次性交付用户验收（内部绿 ≠ 用户验收）。
- **验证命令**：`pnpm vitest run tests/usage tests/pet/usage-*`；`pnpm check`；`cargo test --manifest-path src-tauri/Cargo.toml -j 4`；`cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -j 4 -- -D warnings`；`cargo fmt --manifest-path src-tauri/Cargo.toml -- --check`；`cargo test --manifest-path src-tauri/Cargo.toml -j 4 --lib database::migration`；`cargo test --manifest-path src-tauri/Cargo.toml -j 4 --lib services::usage::collect::tests`。
- **可并行**：否（最终收口）。

---

## 6. 实施顺序与派发批次

依赖图：

```text
T0 → T1 → T2 → T3 → T4 → T5 ─┬→ T6 ─→ T7 ─┐
                              ├→ T8 ───────┼→ T9 → T10
                              └────────────┘
（T6 与 T8 并行；T7 依赖 T6 的 UsageConfigDialog；T9 依赖 T6–T8；后端 T2/T3/T4 严格串行）
```

| 批次 | 任务 | 并行度 | 说明 |
| --- | --- | --- | --- |
| B0 | T0 | 1 | 只读基线。 |
| B1 | T1 | 1 | 契约先行。 |
| B2 | T2 | 1 | 新表 + 新文件 + 自省锁登记。 |
| B3 | T3 | 1 | 与 T2 共享 collect.rs 钉子面，串行。 |
| B4 | T4 | 1 | 依赖 T2/T3；费用查询收口。 |
| B5 | T5 | 1 | IPC + 数据层 + mock + i18n 一次性。 |
| B6 | T6、T8 | 2 | 文件不重叠：T6（config dialog + UsageSection）/ T8（UsageSummary + 相关断言）。 |
| B7 | T7 | 1 | 依赖 T6 组件存在；改 UsageCostCard/Table 域。 |
| B8 | T9 | 1 | 文档。 |
| B9 | T10 | 1 | 全量回归 + 独立审查。 |

**并行纪律**：

- 同一时刻只允许一个执行 Agent 修改 `src-tauri/src/services/usage/**`、`database/schema.rs`、`database/dao/mod.rs`、`commands/usage_v2.rs`、`lib.rs`。后端任务（T1–T5）因此全串行。
- B6 文件所有权：T6 = `UsageConfigDialog/ProviderSection/ModelSection.tsx`、`UsageSection.tsx`、`v2-config-dialog.test.tsx`、删除 `ProviderAliasManager.tsx`/`v2-provider-alias-ui.test.tsx`；T8 = `UsageSummary.tsx`、`v2-summary.test.tsx`、pet 侧文案断言文件。
- T7 与 T8 理论不重叠，但为避免 `tests/pet/usage-cards.test.tsx` 被两边同时改，按批次表先 B6 后 B7。
- 任何任务需要动不属于自己的文件（尤其 locale、mock fixtures、types、collect.rs），停下报告协调者，由所属任务补齐；不得并行抢改。
- 每任务内部：执行 Agent 红绿自查；独立 Review Agent 两轮（规格符合性 → 代码质量）；问题退回修复复审，未关闭不推进。

---

## 7. 性能与回退验证

- **明细聚合规模**：31 天窗口、≥10 万 `usage_detail` 行的合成库（≈2000 会话 × 31 天 × 若干模型/provider）跑 `costs_with_conn` 与 `cost_csv_with_conn`：正确性断言（分组/金额/回退开关）+ 打印耗时；`GROUP BY day_key` 服务端聚合后行数 ≈ `usage_daily` 行数，内存不放大。实测数据记入 §12（开发机参考目标：< 2s；不作为 CI 硬门禁）。
- **覆盖检测成本**：两个 `DISTINCT day_key` 索引范围查询，O(天数)；有负向用例锁「无 detail 表行时回退不 panic」。
- **回退语义**：Detail → dsh 全价；任一天缺失 → 整段 Daily → dsh output-only + reasons；5h/当日不受影响；billing preview 不受影响。
- **前端**：不新增轮询；observed entries 仅在弹窗打开时 `enabled` 拉取（避免设置页常驻查询）；catalog search 输入防抖沿用现有模式。

---

## 8. 回归与门禁

### 8.1 测试隔离

- 修改 `src-tauri/src/services/usage/**` 前停止 `pnpm tauri:dev`；测试只用内存库或 `support::open_ledger_db(tag)` 私有临时库；不写真实账本。
- 前端只用合成夹具与 mock；公开目录探测只 GET `https://models.dev/api.json`，不上传本地数据。

### 8.2 命令清单

| 范围 | 命令 | 期望 |
| --- | --- | --- |
| Rust 定向 | `cargo test --manifest-path src-tauri/Cargo.toml -j 4 --lib services::usage::v2::model_config` | 0 |
| Rust 定向 | `cargo test --manifest-path src-tauri/Cargo.toml -j 4 --lib services::usage::v2::pricing` | 0 |
| Rust 定向 | `cargo test --manifest-path src-tauri/Cargo.toml -j 4 --lib services::usage::v2::query` | 0 |
| Rust 定向 | `cargo test --manifest-path src-tauri/Cargo.toml -j 4 --lib services::usage::v2::cost` | 0 |
| Rust 集成 | `cargo test --manifest-path src-tauri/Cargo.toml -j 4 --test usage_model_config_test` | 0 |
| Rust 集成 | `cargo test --manifest-path src-tauri/Cargo.toml -j 4 --test usage_cost_test` | 0 |
| Rust 集成 | `cargo test --manifest-path src-tauri/Cargo.toml -j 4 --test usage_billing_test` | 0 |
| Rust 集成 | `cargo test --manifest-path src-tauri/Cargo.toml -j 4 --test usage_ipc_test` | 0 |
| Rust 迁移 | `cargo test --manifest-path src-tauri/Cargo.toml -j 4 --lib database::migration` | 0（七表） |
| Rust 自省锁 | `cargo test --manifest-path src-tauri/Cargo.toml -j 4 --lib services::usage::collect::tests` | 0（34 文件） |
| Rust 全量 | `cargo test --manifest-path src-tauri/Cargo.toml -j 4` | 0（Windows 预存 `with_home` 缺陷项与基线一致即记录放行） |
| Rust lint | `cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -j 4 -- -D warnings` | 0 |
| Rust fmt | `cargo fmt --manifest-path src-tauri/Cargo.toml -- --check` | 0 |
| 前端定向 | `pnpm vitest run tests/usage tests/pet/usage-*` | 0 |
| 前端全门禁 | `pnpm check` | 0 |
| i18n | `pnpm check:i18n` | 0 |
| 错误码 | `node scripts/check-error-codes.mjs` | 0（无新增） |
| 类型 | `pnpm exec tsc --noEmit` | 0 |

Windows 内存紧张时 `cargo -j 4`（必要时 `-j 2`）；冷编译耗时不判超时失败。

### 8.3 自省锁钉子同步流程（每个后端任务必做）

1. 跑 `cargo test --manifest-path src-tauri/Cargo.toml -j 4 --lib services::usage::collect::tests::scan_face_size_is_pinned`。
2. 失败信息给实测 `(lines, chars)`，更新 `collect.rs` 断言与注释（轮次、判据面数字）。
3. T2 额外：`USAGE_SOURCES` 33→34（`("v2/model_config.rs", include_str!("v2/model_config.rs"))`）、分类断言 27/6→28/6、`services/usage/mod.rs` 文档同步。
4. 重跑至绿；禁止删断言或缩扫描面「修绿」。

### 8.4 迁移幂等

- `schema.rs` 只追加 `CREATE TABLE IF NOT EXISTS` / `CREATE UNIQUE INDEX IF NOT EXISTS`；表达式唯一索引在旧 SQLite 上可用（rusqlite bundled 版本支持表达式索引）。
- `migration.rs`：DROP 七张 V2 表 → 写旧四表数据/游标 → `migrate` 两次 → 旧表逐值一致、七张新表存在。不改旧表主键、不重写历史、不加 user_version 序列。

### 8.5 错误码与 i18n

- 不新增 `USAGE_CODES`；校验失败 `usage-settings-invalid`。
- `pnpm check:i18n` 只保键集；T10 逐键核对新增/改动英文值；键删除必须 zh/en 同步删，防止单侧漂移。

---

## 9. 验收标准

1. 配置弹窗 ≈1100px 两段式；无固定 min-w、任何条目数量下无横向滚动、仅纵向滚动；键盘与 aria 语义齐全；深浅主题正常。
2. A 段列出全部已观测原始 provider（含历史出现、当前无用量）；空 provider 按「工具+模型」单列；逐条编辑自定义名，未编辑显示原名；保存后 C/D/趋势分组按显示名合并（回归既有别名语义）。
3. 「渠道 = 供应商别名」文案统一；billingProfile/绑定规则/四价覆盖收进「高级覆盖」默认折叠，功能不回归（绑定预览/撤销/覆盖价 CRUD）。
4. B 段每条目三行齐备：row1 原始证据只读；row2 只读显示名 + 可编辑自定义模型名（可复制原名、可清除），显示名键 =（显示供应商, 原始模型），D 卡行名应用自定义模型名；row3 计价身份仅可选择、默认显示精选自动匹配结果、可覆盖/清除、悬停显示四价/发布日/条件价/快照日期。
5. 价目目录默认只展示官方 12 家 + OpenRouter；「显示全部 N 个来源」可切换且 N 与快照一致（mock 基准 232）；自动匹配恒在精选内且官方优先、OpenRouter 兜底；非精选来源只能经 row3 显式选择生效。
6. 7d/30d/自定义费用与 CSV 读取小时明细按日聚合；dsh 全量计价；删除任一天明细后整段回退日聚合且 dsh output-only（reasons 可见）；5h/当日与 billing preview 行为不变。
7. D 卡为单表五列 + 行展开四行三列小表；合计行 Token+金额合计、单价空；小字仅归一化模型 + 快照时间；无价格来源/发布日；不准确原因 `·` 一行；按渠道默认分组、按模型平铺；无全局合计、无渠道小计、缺价显示「未估价」不为 $0。
8. 表头 USD/CNY 快捷切换生效且与设置页汇率联动；导出 CSV 仍为 USD 且列集不变。
9. A 卡无「缓存写入（解释）」块；全局计量文案统一「新增输入/缓存命中/输出/合计」（分享图/文本摘要/迷你条跟随）；Token 口径与总量不变。
10. 迁移幂等（七张 V2 表、旧表逐值不变）；自省锁钉子实测同步；无新增错误码；i18n zh/en 键集与值核对通过；`pnpm check`、前端全量、Rust 全量/定向、clippy/fmt 全绿。

---

## 10. 风险与已知边界

| 风险 | 影响 | 缓解 |
| --- | --- | --- |
| 同一显示名合并多个原始 provider（别名语义固有） | B 段同一展示键多成员；模型配置可能分歧 | row1 列全部原始证据；编辑批量写收敛；后端 `display_model_name` 分歧时 null 回退原名；已知限制沿用 spec §3.2，不拆账。 |
| 空 provider 未指派条目 | displayProvider=None（未指定渠道） | B 段仍可配（键含 source_id），A 段提示先指派；配置随指派自动迁移显示键（身份键稳定）。 |
| 精选来源覆盖不足 | 部分模型精选内无报价 → 默认未估价 | B 段显示「精选来源未命中」；用户开「显示全部」显式选择非精选来源（计价身份覆盖）；仍无 → 未估价（诚实降级）。 |
| 明细保留期调低/清理 | 31 天窗口整段回退日聚合，dsh output-only | 覆盖检测 + 整段回退；reasons 披露；A 卡页脚既有「N 天前无明细」提示呼应；不做逐日混合。 |
| dsh 明细可信判据 | 误信导致伪精确金额 | 明细四桶为归一化互斥规范桶（与 Hour 档同源）；dsh 明细形态夹具锁测试；Daily 路径维持 `basis_trusted` 现状。 |
| 旧自动匹配用例口径翻转 | Task 3 改 Curated 后既有夹具红测 | 属预期行为变化：更新夹具并保留保守性断言（前缀回退/条件价/多候选异价），执行记录登记。 |
| `display_model_name`/新增 wire 字段 | 旧 mock/夹具缺字段 | 字段可空 + 前端回退原始名；`usageMockParity` 锁两端形状。 |
| 弹窗内容超高 | 长列表难用 | 纵向滚动 + 分区标题吸顶不强制；A/B 段条目均带搜索/折叠空间（后续可选，不本期承诺）。 |
| 自省锁漂移 | 后端测试红 | §8.3 流程逐任务同步；新增文件登记一处（`v2/model_config.rs`）。 |
| i18n 键删除/值改动 | 键集或英文侧漂移 | 删除 zh/en 同步；T10 逐键核对；`usage-locale-scan`/`usage-en-surfaces` 断言兜底。 |
| 1100px 弹窗在 1040 窗口 | 视觉溢出 | `min(1100px, calc(100vw-2rem))`；grid 全部 `min-w-0`；宽度断言入测试。 |
| 计价身份与 PricingRule 双轨困惑 | 用户不知哪个生效 | 优先级链固定且文档化（§3.3）；行详情 `pricingBasis` 与 reasons 可核对；高级覆盖折叠区摘要说明。 |

---

## 11. 提交边界建议

按功能域小提交，禁止 `git add .`；每次只暂存本任务明确路径。

1. `feat(usage): extend v2 contracts for model config and curated pricing`（T1）
2. `feat(usage): persist model display names and pricing identities`（T2）
3. `feat(usage): restrict auto pricing match to curated sources`（T3）
4. `feat(usage): price costs from hourly detail with daily fallback`（T4）
5. `feat(usage): expose observed entries and pricing identity ipc`（T5 后端部分，可与 6 合并）
6. `feat(usage): add mocks fixtures and i18n for usage config`（T5 前端部分）
7. `feat(usage): add two-section usage config dialog`（T6）
8. `feat(usage): rebuild cost card as single table with detail grid`（T7）
9. `refactor(usage): drop cache-write explainer and unify meter labels`（T8）
10. `docs(usage): document usage config and cost table changes`（T9）
11. `test(usage): cover usage config and detail pricing acceptance`（T10 测试）

中间提交只是工作记录，不代表用户验收；最终一次性交付。

---

## 12. 执行记录（实施后由主 Agent 追加）

> 本轮只写计划，未执行任何任务。实施时按任务追加：任务编号、HEAD、红测命令与真实失败原因、绿测命令与退出码、Review 两轮结论、修复复审、提交、未解决问题、自省锁轮次与实测 `(lines, chars)`、Task 0 的 provider_count 实测值、Task 4 的性能实测（行数/耗时）。不得预填通过，不得另建本功能证据文件。

### 2026-10-20 执行记录（主 Agent 追加）

- **基线**：`19e789c0`。任务提交：T1 `737419df`；T2 `cb4f3a0e`；T3 `2406618f`；T4 `0d524392`；T5 后端 `d89cee0b` / 前端数据层 `7bc7cb38` / flaky `b1e155ff`；T6 `9db3eda5`；T8 `b04b8f96`；T7 `b73fca5f`；T9 `4a30da8f`；T10 `59bf9647`；标签/格式 `11dfae49`；复审修复 `030564a1`、`161207d0`、`eb0fa693`、`60aaef1b`、`e46ed747`、`04ee5848`。
- **T0**：HEAD/工作区确认；`pnpm tauri:dev` 已停；models.dev `provider_count=232`，官方 12 家原文在场；基线绿测 `services::usage::v2` 136、`usage_cost_test` 36、`usage_ipc_test` 14、前端 usage 208/5 skip。
- **自省锁**：T2 新增 `v2/model_config.rs`，33→34 文件、27/6→28/6；第 41–45 轮同步面尺寸，最终钉子 `12422 行 / 277406 字符`（后续纯注释修复未漂移）。
- **迁移**：七张 V2 表连续 migrate 两次、旧六表逐值不变。
- **T4 性能**：104,160 行 `usage_detail` / 31 天 / 42 成员组 / **128ms**（复审复跑 142ms），正确性断言通过；`#[ignore]` 手动档、无墙钟门禁。
- **T10**：新增 7 组验收 + 双语核对；记录页别名浮窗 8 条测试随 `eb0fa693` 恢复。
- **最终回归**（受测 `e46ed747`，HEAD `04ee5848` 仅文档一行）：默认 `pnpm vitest run` 两轮 **1718 通过 / 0 失败 / 5 跳过**；`pnpm check` EXIT 0（format/lint/i18n 1639 键/error-codes/build）；Rust lib **2569 通过 / 5 预存失败 / 43 ignored**；集成 **167 通过 / 0 失败 / 13 ignored**；clippy/fmt 绿；真实账本未写入。
- **独立复审**：后端可合入（2 个 P2 注释/记录已修）；前端 1 个 P1 + 5 个 P2 全部关闭；文档 2 个 Minor + Nits 关闭或登记；复核后均判定可合入。
- **已知边界**：同原始 provider 串无法拆分（除高级覆盖）；精选来源之外仅在“显示全部”可选；明细保留期调低/清理时 7d/30d 费用整段回退日聚合（dsh output-only）；高级覆盖默认隐藏；前端 4 条 Nit（`settings.usage.billingHint` 死键、PricePreview 底部裁剪、combobox aria/方向键、autoMatch 失败静默）留待后续清理。

---

## 13. 2026-10-20 追加修订（第二轮，用户逐项确认）

本节覆盖 §0–§12 中与下列口径冲突的表述；实施仍在本 PR 内，不拆 PR。

### 13.1 决策

1. **条件价**：`conditional` 报价不再丢弃；有基础文本四价就按基础价估算，行内 reasons 标 `conditionalBaseRate`；缺价仍为 null，不显示 0。
2. **来源优先级**：同一归一化模型按「直营官方（含 `-cn`）> Ollama Cloud > OpenRouter」取价；修正 `zhipuai` 行被 `ollama-cloud` 覆盖的问题。
3. **原生人民币价 + 当日汇率**：刷新时解析 models.dev 源 TOML 明确写出的人民币四价（智谱 / Moonshot CN / MiniMax CN 等），存 `usage_pricing_native`；无人民币数值的来源按当日汇率换算。当日 USD→CNY 存 `usage_pricing_fx`，回退顺序 er-api → cached → 设置页备用 → 7.2。
4. **双币种与虚线**：USD 用目录 USD 原价，CNY 优先原生人民币价；缺失侧按当日汇率换算。原生价正常显示，换算价加虚线下划线，卡头图例说明。
5. **覆盖价币种**：四价覆盖增加 `USD/CNY` 选择并落库；另一显示币种按当日汇率换算。
6. **配置弹窗**：删除 A 段「供应商显示名（渠道）」；供应商名/模型名都在「供应商/模型条目」内编辑，供应商名对同一原始 provider 全局生效；空 provider 条目留在列表内命名（toolModel 别名）。
7. **条目 Top10**：默认最近 7 天 `requestTotal+output` 前 10；底部展开全部；展开后提供搜索。
8. **D 卡**：默认展开；每条模型一张卡片，明细在卡内；主表头/明细列头实体字（前景色）；明细四列补「项目 / Token 用量 / 金额 / 单价」；卡头新增「同步价格」手动强刷。

### 13.2 实施映射

| 变更 | 主要文件 |
| --- | --- |
| 来源优先级 / 条件价 | `src-tauri/src/services/usage/v2/pricing.rs`、`cost_query.rs` |
| 原生人民币价/汇率 | 新 `v2/native_price.rs`、`v2/currency.rs`、`database/{schema,migration}.rs`、`database/dao/usage_billing.rs` |
| 双币种 wire | `v2/model.rs`、`src/types/usageV2.ts`、`src/lib/usage/mockFixtures.ts` |
| 覆盖价币种 | `database/schema.rs`、`dao/usage_billing.rs`、`pricing.rs`、`PriceOverrideManager.tsx` |
| Top10 / 条目内改名 | `v2/model_config.rs`、`UsageConfigModelSection.tsx`、删除 `UsageConfigProviderSection.tsx` |
| D 卡卡片/表头/同步/虚线 | `UsageCostTable.tsx`、`UsageCostDetailGrid.tsx`、`ConvertedPrice.tsx`、`UsageCostCard.tsx`、`displayCurrency.ts` |
| 默认展开 | `src/pages/usage-dashboard.tsx` |
| 文档 | 本 Spec §7.3、README zh/en、用户手册 zh/en 11、CHANGELOG |

### 13.3 追加门禁

- 前端：A 段删除后的条目内改名、Top10/展开/搜索、卡片结构、明细四列头、原生/换算虚线、同步按钮 loading/成功/失败、默认展开。
- 后端：来源优先级、条件价、人民币解析三种格式、汇率回退、双币种分量级 converted、覆盖价币种、Top10 字段、迁移幂等。
- 自省锁：新增 `v2/native_price.rs`、`v2/currency.rs` 后文件数 34→36，面尺寸重新钉住。


---

## 14. 2026-10-20 第三轮修订：配置共用弹窗 / 高级区降级 / 覆盖价快捷入口

> 本节覆盖 §0–§13 中与下列口径冲突的表述（配置弹窗、高级覆盖折叠区、BillingProfilesManager 归属等）。
> 当晚用户先裁决内嵌折叠卡、随后收回并裁决「显示与计价配置 + 高级覆盖放在同一个弹窗」，以下为最终形态。

### 14.1 决策

1. 配置回到**一个共用 Dialog**（`UsageConfigDialog`）：设置页用「打开配置」按钮触发，D 卡「管理配置」直接打开；弹窗上半是供应商/模型条目，下半是可折叠的高级覆盖（默认收起）。打开才挂载内容并拉取观察条目，关闭即卸载停发。
2. 配置弹窗移除 BillingProfilesManager / 参考价规则 UI；记录页入口与后端数据保留。高级区只留四价覆盖。
3. Radix Tooltip + Info 图标；新增 `ui/tooltip.tsx`、`UsageInfoTip`，`main.tsx` 挂 Provider。弹窗打开时初始焦点落在标题，避免 InfoTip 的 Tooltip 先消费 Esc。
4. 四价覆盖保留列表，并在每个模型条目加快捷入口（预填原始模型名、已有覆盖进编辑、显示已应用徽标）。
5. `ObservedModelEntry` 增加 `normalizedModelId`，作为快捷入口/徽标/已有覆盖查找的唯一键；空模型空串。
6. 设置页 `PricingStatusPanel` 移除覆盖管理器，只留启用/刷新/错误 + 「已移至上方配置弹窗」提示。

### 14.2 实施映射

| 变更 | 主要文件 |
| --- | --- |
| 配置弹窗壳 / 帮助 | `src/components/usage/{UsageConfigDialog,UsageConfigPanel,UsageDisclosure,UsageInfoTip}.tsx`、`src/components/ui/tooltip.tsx`、`src/main.tsx` |
| 入口切换 | `src/components/settings/UsageSection.tsx`（打开配置按钮）、`UsageCostCard.tsx`（管理配置 → 弹窗） |
| 覆盖价入口 | `UsageConfigModelSection.tsx`、`PriceOverrideManager.tsx`、`PricingStatusPanel.tsx` |
| 数据契约 | `src-tauri/src/services/usage/v2/{model,model_config}.rs`、`src/types/usageV2.ts`、mock 夹具 |
| 文案 | `src/i18n/locales/{zh,en}.json` |
| 测试 | `tests/usage/v2-config-*`、`v2-cost-card`、`v2-pricing-override`、`v2-acceptance`、`usage-settings`、`usageMockParity`；后端 `usage_ipc_test` / `usage_model_config_test` |
| 文档 | 本 Spec §7.4、README zh/en、用户手册 zh/en 11、CHANGELOG |

### 14.3 门禁

- 弹窗关闭时不挂载面板、不拉 observed entries；打开才拉取，关闭即停发；Esc / Close 可关闭并卸载。
- 弹窗初始焦点在标题，第一次 Esc 不被 InfoTip Tooltip 消费。
- 高级区只含四价覆盖，无 billing manager；记录页旧版入口仍在。
- Info 图标 hover/focus 出 tooltip；快捷入口预填/已有覆盖编辑/徽标/空模型禁用。
- `normalizedModelId` wire 冻结与归一化值；自省锁钉子更新。
- 前端全量 + `pnpm check`；Rust usage 域 + clippy/fmt + 自省锁；真实账本不写入。

---

## 15. 2026-10-21 修订：记录页渠道展示与入口收敛

> 本节覆盖 §0–§14 中「记录页行名恒为原始供应商/模型」「未归属行指定渠道 / 已归属行修改关联 / 含糊行查看规则」的旧口径。

### 15.1 决策

1. 记录页行名改为**供应商别名优先**：`别名 / 原始模型`；无别名回退原始 `row.label`（空 provider 无别名仍为裸模型名）。原始身份保留在 `title` 与行数据中。
2. 同一行映射出多个显示名（project 卡跨工具聚合）时保守回退原始 label；记录页只应用供应商别名，不应用自定义模型名。
3. 普通行（`unassigned` / `observed` / `assigned`）不再显示「指定渠道 / 修改关联」；只有 `ambiguous` 行保留「绑定计费归属」（`BillingAssignmentDialog`）。所有行保留「供应商显示名 / 编辑」。
4. `BillingProfilesManager` 收进记录页页脚「高级：计费归属与参考价规则」入口；后端表与历史配置不删除、不迁移。
5. 文案去歧义：别名层 =「供应商显示名 / 供应商别名」，计费层 =「计费归属」；管理器标题改为「计费归属与参考价规则」。
6. 同一窗口内别名保存后记录页行名与看板即时更新（同一 `usage_provider_alias` 表 + `["usage"]` 失效链）；设置页 ↔ D 卡跨 WebView 不保证即时，本轮不加广播。

### 15.2 实施映射

| 变更 | 主要文件 |
| --- | --- |
| 别名显示名 | `ProviderAliasFields.tsx::displayIdentityLabel`、`UsageRecordsPanel.tsx::displayLabelOf` |
| 行内入口收敛 | `UsageRecordsPanel.tsx` 行渲染（仅 ambiguous 保留绑定按钮） |
| 旧版入口 | `UsageRecordsPanel.tsx` 页脚 `usage-records-billing-advanced` |
| 文案 | `src/i18n/locales/{zh,en}.json`（`usage.records.*` / `usage.billing.manage*`） |
| 测试 | `tests/usage/v2-identity-ui.test.tsx`、`v2-provider-alias-dialog.test.tsx`、`v2-acceptance.test.tsx`、`tests/pet/usage-records.test.tsx` |
| 文档 | Spec §7.5、README zh/en、用户手册 zh/en 11、CHANGELOG、release note |

### 15.3 门禁

- 别名保存后同一窗口内行名立即更新；`title` 与行数据仍是原始 `row.label`。
- 普通行无 assign 按钮；ambiguous 行有「绑定计费归属」且能打开 Dialog。
- 页脚高级入口能打开 `BillingProfilesManager`，标题为「计费归属与参考价规则」。
- 不改后端 schema / 账本 / 采集；显式计费绑定语义不变。
- 前端全量 + `pnpm check`；真实账本不写入。


---

## 16. 2026-10-21 修订：自定义价格规则与分时计费

1. 覆盖规则扩展 `time_enabled/time_timezone/time_windows/offpeak_*` 列；固定价保持旧行为，分时价新增时区、多个高峰窗口与低谷四价。
2. 时段固定周一至周五、整点、可跨午夜、不可重叠；不处理节假日；低谷缺项回退目录价；时区预设上海/UTC/本地，仅影响 API 计费峰谷计算。
3. 费用查询在存在分时规则时按小时明细计价，输出一行一模型/渠道，单价加权；日聚合回退按低谷价估算并标注。
4. 自动匹配改为「官方 > OpenRouter」、移除 Ollama Cloud 与 zhipuai-coding-plan、跳过全零候选；手动 0 价保留。
5. 配置弹窗：交互内容加边框、图例、改过琥珀色、三键排序、宽度 1040/840；删除旧版说明与重复定价提示。
6. 明细保留期上限 90 天；工作小结文案改为只统计会话数/工具调用次数。
