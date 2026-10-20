# 用量看板 V2 统一实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task.（若本环境无此技能，按本文件「单一工作流与 Agent 协议」一节执行——该协议已完整内嵌，不因技能缺失而阻塞。） Steps use checkbox (`- [ ]`) syntax for tracking. 用户已选定执行 Agent + 独立 Review Agent 的内部循环；不再询问执行方式。

**Goal:** 一次性交付四核心板块加辅助工作活动、可撤销计费身份、逐模型 API 等效费用和独立费用 CSV，并交用户整体验收。

**Architecture:** 保留原 Token 账本与兼容接口，先建立身份/查询，再完成定价/估算，最后整合 UI 与导出。所有任务属于同一工作流，按下表依赖顺序推进；内部测试和审查是推进门槛，不是分阶段向用户交付。

**Tech Stack:** 既有 Rust、SQLite、Tauri 2、React、TypeScript、React Query、Vitest、reqwest、sha2；不新增 UI/图表体系。

---

## Spec 与文档边界

- 唯一功能规格：[用量看板 V2 设计](../specs/2026-10-17-usage-dashboard-v2-design.md)，以本次仓库核对修订稿为准。
- 唯一执行入口为本文件，接口契约、文件清单、源代码对照、探测证据、任务和最终验收均已内嵌。不要求另读本功能的 contract/review/evidence 文件。
- 最高约束仍是 `docs/MASTER-PLAN.md`；本次未修改宪法。
- 代码核对基线 `cccf009afaa6d49e322e3a5d4f4a856f86a417b8`，2026-10-17。实施时先确认实际 HEAD 和工作区变化，按符号复核接口，不按旧行号整段覆盖。
- 本次只交付 Spec + Plan，不启动实现、红绿测试、构建、真机采集或实现子代理。以下命令均为未来获准执行本计划后运行。

## 单一工作流与 Agent 协议

主 Agent 保存任务状态和依赖；执行 Agent 负责 TDD、实现和修复；独立 Review Agent 负责审查。每个任务只允许一个执行者写代码，Review Agent 不改实现。一个 Review Agent 可依次承担规格符合性与代码质量两轮检查，符合用户指定的两个工作角色；不能将两轮合并为执行者自查。

每任务内部循环：

1. 主 Agent 向执行 Agent 提供本任务全文、相关 Spec 条款、已完成依赖、真实接口与允许修改路径；避免要求子代理自己拼接其他计划。
2. 执行 Agent 写失败测试，运行并确认因缺失目标行为失败，再最小实现、运行通过、自查并提交任务证据。文档/只读探测任务不编造红测，用具体检查代替。
3. Review Agent 先核规格符合性：逐条核任务/Spec、无额外功能、数据口径与降级正确；通过后再核代码质量、测试是否有意义、类型/接口一致、锁序与性能。
4. 任一轮发现问题，交回本任务执行 Agent 修复并补测；Review Agent 对新版本重审。质量修复改变行为时重新跑规格审查。两轮无未关闭问题才能勾选任务并推进后继。
5. 按任务暂存和形成小提交。不得为了通过审查删除有效断言、扩大允许修改范围或关闭门禁。中间提交仅是工作记录，不标记功能已被用户验收。

建议每个新任务使用独立上下文的执行者/审查者；同一任务的修复复审复用原角色上下文。任何时刻不并行派多个执行者修改共享契约。内部可自主推进，不逐任务请求用户验收或询问是否继续。

发现与宪法冲突、必须改变冻结产品行为、或无法自主解决的真实阻塞时，主 Agent 记录原因并向用户裁决；普通测试失败和审查意见由内部循环解决，不推给用户。所有任务通过后再做一次全功能独立审查与完整验证，最后一次性给用户验收。

### 每任务交接格式

```text
执行 Agent：任务编号 / 修改路径 / 红测命令与真实失败原因 / 最小实现摘要 /
绿测命令与结果 / 自查 / 提交或diff / 未解决问题。
Review Agent：审查版本 / 规格结论 / 质量结论 / 问题路径与行号 /
复现证据 / 修复要求 / 复审结论。
主 Agent：仅在两轮均通过后勾选；执行记录写在本文件末尾。
```

## 任务依赖与唯一执行顺序

| 任务 | 目标 | 直接前置 |
| --- | --- | --- |
| 1 | 只读门禁、源证据、现有测试基线 | 无 |
| 2 | 身份类型与纯匹配 | 1 |
| 3 | 配置存储、版本提交、撤销 | 2 |
| 4 | 账本适配、趋势、分布、身份预览 | 3 |
| 5 | 身份/Token IPC、Mock、查询缓存 | 4 |
| 6 | 缓存安全桶判定 | 2、4 |
| 7 | 目录解析与定点费率 | 6 |
| 8 | 价格快照、启用与刷新 | 3、7 |
| 9 | 金额、状态与覆盖率 | 6–8 |
| 10 | 费用查询与CSV后端 | 4、9 |
| 11 | 定价/费用IPC、Mock、事件 | 5、8、10 |
| 12 | Token页面、折叠、趋势筛选、Top5 | 5 |
| 13 | 记录绑定与渠道管理 | 4、5、12 |
| 14 | 分享摘要与Token回归 | 12、13 |
| 15 | 费用组织与金额格式 | 11 |
| 16 | D费用卡与最终页面顺序 | 12、15 |
| 17 | 定价设置与授权参考模型 | 8、11、13、16 |
| 18 | 独立费用导出动作 | 10、16、17 |
| 19 | 双语/完整回归/总审查/统一交付 | 1–18 |

即使部分任务技术上可独立开展，本轮默认按1→19串行推进，以保持唯一执行者和统一契约；不按原三份计划分别交付。

## Global Constraints

1. 修改 `src-tauri/src/services/usage/**` 前停止 `pnpm tauri:dev`。测试只用内存库或 `support::open_ledger_db(tag)`；需要全局 DB 的集成测试先调用已有 `support::setup()`。不启动真实采集验证迁移。
2. 不改七个 Collector、`DeltaBuilder`、游标语义、Token 去重键、`normalize`、真实历史、明细保留期或 200 行批量预算。
3. Token 总量恒为 `requestTotal + output`。缓存写放详情；删除展示/文本/分享图的 `userEst`，保留 DB/IPC 兼容字段。
4. 现有落库 `provider` 可能受旧规则影响。V2 的 `observedProvider` 表示“既有账本所留供应商”，必须同时返回 `sourceKind`；`inferred` 明示“既有推断供应商”，不宣称恢复日志原文。
5. 付款渠道必须用户指定；日志 provider 不自动变成付款账户。`observed` 归因类型保留协议取值，V1 不产生没有账户证据的 observed 渠道。
6. 旧 `providerMapRules` 保留、不自动转换成渠道规则、不恢复 JSON 编辑框、不重采历史。
7. 只在同渠道身份 + 模型 + 参考价 + 价格口径 + USD 的模型行内累加；无全局费用总计、渠道头小计或费用总趋势。
8. 未知按来源工具区分，含糊不进入明确渠道筛选。不按 Token 比例拆分历史日桶。
9. 新命令统一 `Result<_, UsageError>`。复用 `usage-settings-invalid`、`usage-filter-unavailable`、`usage-db-failed`、`usage-source-io`、`usage-internal`，不新增错误码。
10. 查询键 `['usage','v2',域,...完整参数]`；写成功失效 `USAGE_KEY`。不进三秒轮询；网络等待时不持 DB 锁。
11. Token CSV 列和原始整数口径不变；固定分享模板仅删除估算用户输入指标。C 的新渠道分组不得传给旧 `UsageGroupBy` 或旧 CSV。
12. 所有新文案同步 zh/en；深浅主题、键盘、加载/错误重试/不可得同批交付。

## File Structure

新增 `src-tauri/src/services/usage/v2/`：`model.rs` 唯一 wire 类型；`identity.rs` 纯匹配；`billing.rs` 配置、预览、提交、撤销；`query.rs` 账本适配和 Token 查询；`pricing.rs` 定价校验与刷新；`cost.rs` 纯估算；`cost_query.rs` 费用查询/CSV；`mod.rs` 导出。

新增 `src-tauri/src/database/dao/usage_billing.rs` 管新表；`src-tauri/src/commands/usage_v2.rs` 仅转发；`src/types/usageV2.ts`、`src/lib/api/usageV2.ts`、`src/lib/query/queries/usageV2.ts` 分别负责 TS 契约、IPC、缓存。

## 冻结的 TS 数据类型

任务2建立身份与 Token 部分；任务7–11扩展定价/费用。复用类型从 `@/types/usage` 导入。

```ts
import type { UsageBuckets, UsageMetrics, UsageRange, UsageSourceId,
  UsageSourceKind, TrendPoint, CompareBlock } from '@/types/usage';
export type AttributionKind = 'observed'|'userAssigned'|'unknown'|'ambiguous';
export type DistributionGroup = 'billingProfile'|'tool'|'project'|'model';
export type TrendSelection =
  | { scope:'all' }
  | { scope:'tool'; selectedId:UsageSourceId }
  | { scope:'billingProfile'; selectedId:string }
  | { scope:'unknown'; selectedId:UsageSourceId };
export interface IdentityKey {
  sourceId:UsageSourceId; observedProvider:string|null; modelId:string;
}
export interface BillingProfile {
  id:string; name:string;
  kind:'officialSubscription'|'api'|'thirdPartyPlan'|'unknown';
  note:string; archived:boolean;
}
export interface BillingRule {
  id:string; identity:IdentityKey; match:'exact'|'prefix'; billingProfileId:string;
  effectiveFrom:number; effectiveTo:number|null; mixed:boolean;
}
export interface PricingReference { providerId:string; modelId:string }
export interface PricingRule {
  id:string; identity:IdentityKey; match:'exact'|'prefix'; reference:PricingReference;
}
export interface BillingConfig {
  revision:number; profiles:BillingProfile[]; rules:BillingRule[];
  pricingRules:PricingRule[];
}
export interface Attribution {
  billingProfileId:string|null; attributionKind:AttributionKind; ruleIds:string[];
  reason:'unassigned'|'conflict'|'mixed'|'bucketBoundary'|null;
}
export interface IdentityUsage {
  identity:IdentityKey; sourceKind:UsageSourceKind; attribution:Attribution;
  buckets:UsageBuckets; metrics:UsageMetrics;
}
export interface DistributionRow {
  key:string; label:string; sourceIds:UsageSourceId[];
  billingProfileId:string|null; attributionKind:AttributionKind;
  buckets:UsageBuckets; metrics:UsageMetrics;
}
export interface UsageTrend {
  selection:TrendSelection; trend:TrendPoint[]; hero:number;
  compare:CompareBlock|null; available:boolean; reason:string|null;
}
export interface UsageDistribution { groupBy:DistributionGroup; rows:DistributionRow[] }
export interface BillingPreview {
  baseRevision:number; candidate:BillingConfig; matchedIdentities:IdentityUsage[];
  matchedTokens:number; ambiguousTokens:number; boundaryTokens:number;
}
export interface BillingCommit { expectedRevision:number; candidate:BillingConfig }
export interface BillingRevision { revision:number; committedAt:number }
export interface PricingStatus {
  enabled:boolean; snapshotId:string|null; fetchedAt:number|null;
  stale:boolean; refreshing:boolean; lastError:string|null;
}
export type CostStatus = 'priced'|'partial'|'unpriced';
export type PricingBasis = 'baseCurrent'|'insufficient';
export interface CostComponent {
  tokens:number; unitUsdPerMillion:string; estimatedUsd:string;
}
export interface CostRow {
  key:string; identity:IdentityKey; sourceIds:UsageSourceId[];
  billingProfileId:string|null; attributionKind:AttributionKind;
  tokenUsage:number; cacheHitRate:number|null; estimatedUsd:string|null;
  status:CostStatus; pricedTokens:number; unpricedTokens:number;
  coverageRate:number|null; pricingReference:PricingReference|null;
  pricingSnapshotId:string|null; pricingBasis:PricingBasis; priceUpdatedAt:number|null;
  reasons:string[];
  components:{input:CostComponent|null;cacheRead:CostComponent|null;
    cacheWrite:CostComponent|null;output:CostComponent|null};
}
export interface UsageCosts { range:UsageRange; rows:CostRow[]; pricing:PricingStatus }
```

Rust 逐一镜像，结构体与字符串枚举使用 `serde(rename_all="camelCase")`；`TrendSelection` 使用内部 tag `scope`。复用旧七源枚举的小写序列化。Option 全部显式序列化 null。金额用字符串，不将设计稿的 `decimal` 当成现有 Rust 类型。

## 新增 IPC 与服务签名

TS 包装函数放 `src/lib/api/usageV2.ts`，参数逐字映射：

| TS 函数与返回 | IPC | 参数对象 |
| --- | --- | --- |
| `usageBillingGet(): Promise<BillingConfig>` | `usage_billing_get` | 无 |
| `usageBillingPreview(candidate: BillingConfig, range: UsageRange): Promise<BillingPreview>` | `usage_billing_preview` | `{candidate,range}` |
| `usageBillingCommit(request: BillingCommit): Promise<BillingRevision>` | `usage_billing_commit` | `{request}` |
| `usageBillingUndo(expectedRevision:number,targetRevision:number): Promise<BillingRevision>` | `usage_billing_undo` | `{expectedRevision,targetRevision}` |
| `usageIdentities(range:UsageRange,filters:UsageFilters): Promise<IdentityUsage[]>` | `usage_identities` | `{range,filters}` |
| `usageTrend(range:UsageRange,selection:TrendSelection): Promise<UsageTrend>` | `usage_trend` | `{range,selection}` |
| `usageDistribution(range:UsageRange,groupBy:DistributionGroup): Promise<UsageDistribution>` | `usage_distribution` | `{range,groupBy}` |
| `usagePricingGet(): Promise<PricingStatus>` | `usage_pricing_get` | 无 |
| `usagePricingSetEnabled(enabled:boolean): Promise<PricingStatus>` | `usage_pricing_set_enabled` | `{enabled}` |
| `usagePricingRefresh(): Promise<PricingStatus>` | `usage_pricing_refresh` | 无 |
| `usageCosts(range:UsageRange): Promise<UsageCosts>` | `usage_costs` | `{range}` |
| `usageCostExportCsv(range:UsageRange): Promise<string>` | `usage_cost_export_csv` | `{range}` |

包装实现范式（每条替换具体类型与表内命令名，禁止 query 层写命令字面量）：

```ts
export function usageTrend(range:UsageRange, selection:TrendSelection) {
  return invoke<UsageTrend>('usage_trend', {range, selection});
}
```

Rust 服务均给出 `*_with_conn` 测试入口与不带 conn 的生产入口。`query::trend_with_conn(conn:&Connection,range:&UsageRange,selection:&TrendSelection,now_ms:i64)->Result<UsageTrend,UsageError>`；`distribution_with_conn` 将 selection 换为 `DistributionGroup`；`identities_with_conn` 换为 `&UsageFilters`。`billing::{load_with_conn,preview_with_conn,commit_with_conn,undo_with_conn}` 参数依次为 `(conn)`、`(conn,candidate,range,now_ms)`、`(&mut conn,request,now_ms)`、`(&mut conn,expected_revision,target_revision,now_ms)`。返回依次为 `Result<BillingConfig/BillingPreview/BillingRevision/BillingRevision,UsageError>`。

## 身份匹配与时间语义

- `sourceId + observedProvider（null 与非空严格区分）+ 完整 modelId`；空串 provider 在适配边界转 null。禁止拆 UI label。ID 新建草稿时 `crypto.randomUUID()`；后台验证唯一、引用存在，保存后不可改写 ID。
- 精确 > 前缀，前缀按长度降序。同级候选时间范围严格包含关系中，更窄范围优先；交叉但互不包含算同级。并列同目标可合并，不同目标或 `mixed` 必须 ambiguous。
- 时间区间 `[effectiveFrom,effectiveTo)`；今后默认当前毫秒。历史重算显式选择日期和确认。源账本只有小时/日粒度，不能准确拆分边界桶。
- 将桶按所有生效边界分成子区间，逐段执行优先级。所有子段唯一同渠道才归属；含未覆盖子段为 `unknown/bucketBoundary`；多渠道/冲突为 ambiguous。不以 updated_at 猜事件时间，不按比例分 Token。
- 日桶只能可靠整日归属；今日与近7天的渠道覆盖可能不同，UI 常驻粒度解释；全局 Token 恒定。小时趋势只读 detail，日趋势只读 daily，不按保留期内 detail 拼接历史日桶。
- `PricingRule` 与渠道规则分离；价格引用默认只接受可靠 observed provider+model 的上游精确键，或用户明确选择。旧 inferred provider 不能自动授权价格别名。

## 新表与迁移

追加到 `src-tauri/src/database/schema.rs` 现有 DDL 字符串，不建立另一套版本号：

```sql
CREATE TABLE IF NOT EXISTS usage_billing_state (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1), revision INTEGER NOT NULL,
 config_json TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS usage_billing_revision (
 revision INTEGER PRIMARY KEY, committed_at INTEGER NOT NULL, config_json TEXT NOT NULL
);
```

无 state 行返回 revision=0 空配置。提交用短事务比较 expectedRevision，写 revision+1 的历史快照和 state；失败回滚。undo 读取目标快照作为新 revision 提交，0 表示空配置；不删除历史。归档渠道仍能显示旧记录，不能物理删除仍被引用的渠道。

现有 `migration::migrate(&Connection)` 会调用 `schema::init`。迁移测试保存旧四表行数、桶和游标，连续迁移两次后逐值一致；不触发预览版重建分支，不修改正式表主键。


## 按职责汇总的文件修改清单

### 身份与Token文件


| 操作 | 路径 | 责任 |
| --- | --- | --- |
| 新增 | `src/types/usageV2.ts` | 契约中的身份/Token 类型 |
| 新增 | `src-tauri/src/services/usage/v2/{mod,model,identity,billing,query}.rs` | 镜像、纯匹配、配置与读取 |
| 新增 | `src-tauri/src/database/dao/usage_billing.rs` | 配置快照 DAO |
| 修改 | `src-tauri/src/database/{schema,migration}.rs`、`dao/mod.rs` | 新表和迁移回归 |
| 修改 | `src-tauri/src/services/usage/mod.rs` | `pub mod v2;` |
| 新增 | `src-tauri/src/commands/usage_v2.rs` | 七条新命令 |
| 修改 | `src-tauri/src/commands/mod.rs`、`src-tauri/src/lib.rs` | 模块与 invoke 注册 |
| 新增 | `src/lib/api/usageV2.ts`、`src/lib/query/queries/usageV2.ts` | 包装与缓存 |
| 新增 | `src/lib/usage/summaryV2.ts`、`distributionV2.ts` | 独立 UI 派生，不影响旧导出 |
| 新增 | `src/components/usage/{UsageFold,UsageTrendFilters,BillingAssignmentDialog,BillingProfilesManager}.tsx` | 可访问折叠、筛选、可视化管理 |
| 修改 | `src/components/usage/{UsageSummary,UsageTrendCard,UsageDistributionCard,UsageRecordsPanel,UsageWorkSummary}.tsx` | 页面职责和轻量操作 |
| 修改 | `src/pages/usage-dashboard.tsx` | 独立状态/查询编排 |
| 修改 | `src/components/settings/UsageSection.tsx` | 渠道入口 |
| 修改 | `src/lib/usage/{exportText,useExportShare}.ts` | 删除 userEst 展示 |
| 修改 | `src/tauri-mock.ts`、`src/lib/usage/mockFixtures.ts`、`src/i18n/locales/{zh,en}.json` | mock 和文案 |
| 新增 | `tests/usage/v2-{api,queries,summary,distribution,identity-ui,page}.test.tsx` | 关键行为测试 |
| 新增 | `src-tauri/tests/usage_billing_test.rs` | 私有库集成 |


### 定价与估算文件


- 新增 `src-tauri/src/services/usage/v2/{pricing,cost,cost_query}.rs`；扩展 `v2/{mod,model}.rs`。
- 修改 `src-tauri/src/database/schema.rs`；扩展 `src-tauri/src/database/dao/usage_billing.rs`。
- 修改 `src-tauri/src/commands/usage_v2.rs`、`src-tauri/src/lib.rs`（后五条 IPC）。
- 扩展 `src/types/usageV2.ts`、`src/lib/api/usageV2.ts`、`src/lib/query/queries/usageV2.ts`；mock 后五条命令。
- 新增 `tests/fixtures/usage-v2/pricing/{valid,missing,invalid,conditional}.json`（人工价格夹具，无真实用量）。
- 新增 `src-tauri/tests/usage_cost_test.rs`；定价和金额纯测试放各模块内。
- 将定价探测结果追加到本文件末尾的执行记录。


### 费用界面与导出文件


- 新增 `src/components/usage/{UsageCostCard,UsageCostRow,PricingStatusPanel}.tsx`。
- 新增 `src/lib/usage/{costRows,costFormat}.ts`；前者仅组织和排序，后者只格式化字符串金额。
- 修改 `src/pages/usage-dashboard.tsx`、`src/components/settings/UsageSection.tsx`、`src/components/usage/UsageExportActions.tsx`、`BillingProfilesManager.tsx`。
- 修改 `src/lib/query/queries/usageV2.ts`、`src/lib/api/usageV2.ts`（使用任务11的接口，不发明另一套命令）。
- 新增 `tests/usage/v2-{cost-card,cost-settings,cost-export,acceptance}.test.tsx`。
- 修改 `src/i18n/locales/{zh,en}.json`、`src/tauri-mock.ts`、`src/lib/usage/mockFixtures.ts`。
- 更新 README 双语、用户手册用量/设置/导出章节、CHANGELOG 未发布小节；发版时再走 AGENTS 两轮检查。


## Tasks：按依赖执行

## Task 1：只读门禁与测试基线

- [x] 1. 执行 `git status --short`，记录执行时基线；读取七个 `collectors/*.rs` 内 provider/model 的提取位置及 `research/七源字段可得性矩阵.md`。将源、落盘 provider 是否实测、模型是否剥前缀、账户字段能否保留写入 `docs/superpowers/plans/2026-10-17-usage-dashboard-v2-plan.md`。不可把“日志有字段”当成“账本有字段”。本任务只读仓库，不读密钥和会话正文。
- [x] 2. 执行 `pnpm test -- tests/pet/usage-api.test.ts tests/pet/usage-page.test.tsx tests/pet/usage-queries.test.tsx tests/pet/usage-export-text.test.ts tests/pet/usage-export-image.test.ts`。期望退出 0；若失败，记录已有失败，不修改测试掩盖基线。
- [x] 3. 执行 `cargo test --manifest-path src-tauri/Cargo.toml --lib services::usage::provider` 和 `cargo test --manifest-path src-tauri/Cargo.toml --lib services::usage::semantics`，期望通过。检查测试不触及真实账本。
- [x] 4. 在本文件末尾记录五项结论：账户不可自动恢复；混合缓存语义不可由最后写入值恢复；小时/日双路径存在；旧规则是 provider 推断而非付款身份；定价匹配由任务7继续探测。若发现与宪法冲突，停止并向用户报告，禁止改宪法。
- [x] 5. 仅提交本文件末尾的执行记录：`git add docs/superpowers/plans/2026-10-17-usage-dashboard-v2-plan.md`；`git commit -m "docs: verify usage v2 attribution boundaries"`。

## Task 2：身份类型与纯规则解析器

- [x] 1. 新建 `v2/model.rs` 和 `src/types/usageV2.ts`，复制契约的身份/Token 类型；先不声明费用类型。新建 `v2/identity.rs` 单测，锁住以下完整测试输入与结果：

```rust
#[test]
fn equal_rank_different_profiles_is_ambiguous() {
    let raw = r#"{"sourceId":"codex","observedProvider":null,"modelId":"gpt-x"}"#;
    let identity: IdentityKey = serde_json::from_str(raw).unwrap();
    let rules: Vec<BillingRule> = serde_json::from_value(serde_json::json!([
      {"id":"r1","identity":identity,"match":"exact","billingProfileId":"a",
       "effectiveFrom":0,"effectiveTo":null,"mixed":false},
      {"id":"r2","identity":identity,"match":"exact","billingProfileId":"b",
       "effectiveFrom":0,"effectiveTo":null,"mixed":false}
    ])).unwrap();
    let got = resolve_bucket(&identity, 100, 200, &rules);
    assert!(matches!(got.attribution_kind, AttributionKind::Ambiguous));
    assert!(got.billing_profile_id.is_none());
}
```

另写表驱动：exact 胜 prefix；最长前缀；null 不匹配非空；source 不同不匹配；同目标重叠无冲突；换渠道边界 ambiguous；半桶无规则 unknown；mixed 不准确归属；空 model 不允许前缀空串规则。用 `serde_json` 造完整规则避免遗漏字段。
- [x] 2. 执行 `cargo test --manifest-path src-tauri/Cargo.toml --lib services::usage::v2::identity`，确认失败于新解析函数未实现，而非 JSON 类型错误。
- [x] 3. 实现以下纯函数；逐段选优，禁止按规则顺序选择：

```rust
pub fn resolve_bucket(identity: &IdentityKey, start: i64, end: i64,
                      rules: &[BillingRule]) -> Attribution {
    let mut cuts = vec![start, end];
    for r in rules {
        if r.effective_from > start && r.effective_from < end {
            cuts.push(r.effective_from);
        }
        if let Some(t) = r.effective_to {
            if t > start && t < end { cuts.push(t); }
        }
    }
    cuts.sort_unstable(); cuts.dedup();
    let parts: Vec<Attribution> = cuts.windows(2)
        .map(|w| resolve_at(identity, w[0], rules)).collect();
    merge_bucket(parts)
}
```

`resolve_at(identity:&IdentityKey,ts:i64,rules:&[BillingRule])->Attribution`：过滤身份及有效时间；先 exact，再最长前缀；剔除被严格更窄时间区间支配的候选。剩余不同 profile 或 mixed 返回 ambiguous，否则唯一目标 userAssigned；无候选 unknown。`merge_bucket(parts:Vec<Attribution>)->Attribution`：任何 ambiguous 或不同非空 profile 返回 ambiguous；存在未知返回 unknown/bucketBoundary；全部同目标返回该目标，ruleIds 去重排序。空 parts 返回 unknown/unassigned。时间 start>=end 由调用层拒绝。
- [x] 4. 运行同一测试，全部绿；补序列化测试核对 scope=tool、billingProfile、unknown 的 selectedId，Option 为 null，sourceId=workbuddy/zcode 小写。
- [x] 5. 提交本任务类型和 identity 文件：`git commit -m "feat: add deterministic billing identity resolver"`。暂存只列上述实际修改路径，禁止 `git add .`。

## Task 3：配置快照与可撤销保存

- [x] 1. 在 `usage_billing_test.rs` 使用 `mod support; let mut conn=support::open_ledger_db("billing_revision");`。写以下调用序列：

```rust
let original = billing::load_with_conn(&conn).unwrap();
assert_eq!(original.revision, 0);
let candidate: BillingConfig = serde_json::from_value(serde_json::json!({
 "revision":0,"profiles":[{"id":"profile-a","name":"工作套餐","kind":"api",
 "note":"","archived":false}],"rules":[],"pricingRules":[]
})).unwrap();
let request = BillingCommit { expected_revision:0, candidate };
let saved = billing::commit_with_conn(&mut conn, &request, 1000).unwrap();
assert_eq!(saved.revision, 1);
assert!(billing::commit_with_conn(&mut conn, &request, 1001).is_err());
let undone = billing::undo_with_conn(&mut conn, 1, 0, 1002).unwrap();
assert_eq!(undone.revision, 2);
assert!(billing::load_with_conn(&conn).unwrap().profiles.is_empty());
```

测试中的 profile-a 为固定测试 ID；生产可接受不透明非空唯一 ID，不强制 UUID 格式，以免与测试冲突。补空名、重复 ID、悬空引用、日期逆序、空前缀拒绝；旧表账本/游标提交前后完全一致。
- [x] 2. 执行 `cargo test --manifest-path src-tauri/Cargo.toml --test usage_billing_test`，确认失败于新服务/表尚未实现。
- [x] 3. 在 schema 添加契约两张表；DAO 实现 `load_config_conn`、`commit_config_conn`、`load_revision_conn`，均返回 `Result`，不吞 SQL 错误。事务先读取 revision，再校验 expected，序列化配置并写历史和 state。本任务实现 load/commit/undo 三函数；preview 依赖任务4的查询，完整实现移入任务4，不能在此调用尚未落地的查询。

```sql
INSERT INTO usage_billing_revision(revision,committed_at,config_json) VALUES(?1,?2,?3);
INSERT INTO usage_billing_state(singleton,revision,config_json) VALUES(1,?1,?3)
ON CONFLICT(singleton) DO UPDATE SET revision=excluded.revision,config_json=excluded.config_json;
```

`revision` 的 JSON 值也必须改为新版本，不能写入旧 revision。事务失败返回 usage-db-failed；revision 冲突返回 usage-settings-invalid，detail 为 `billing revision conflict`。规则合法但业务匹配冲突可以保存，页面必须展示冲突，不能静默选赢家。
- [x] 4. 跑集成测试；在 migration.rs 补“当前形状旧账本→连续 migrate 两次”用例，比较旧表的全部行与游标。运行 `cargo test --manifest-path src-tauri/Cargo.toml --lib database::migration`。
- [x] 5. `git commit -m "feat: persist reversible billing configuration revisions"`，只暂存本任务 schema、DAO、服务、测试和模块导出。

## Task 4：V2 账本适配、独立趋势与分布

- [x] 1. 在 `v2/query.rs` 单测构造现有 LedgerRow：两工具同裸模型、两个 profile、未知一组、同桶换渠道一组。直接用内存 DB 的旧 DDL 插入 detail/daily，禁止经过 collect。断言全局 hero 守恒；明确渠道只含可靠归属；工具/渠道的当期和上期使用同筛选；日档不读取 detail；记录身份接口在项目筛选下保留 sourceId。
- [x] 2. 执行 `cargo test --manifest-path src-tauri/Cargo.toml --lib services::usage::v2::query`，确认接口缺失导致红测。
- [x] 3. 实现契约 `trend_with_conn`、`distribution_with_conn`、`identities_with_conn`。复用现有 `range::resolve_range`、`query::{load_ledger_rows_with,load_rows_for_keys,aggregate,apply_filters}`（已为 pub(crate)，无需全局公开）。增加内部适配结构：

```rust
pub(crate) struct AttributedRow {
    pub row: crate::services::usage::query::LedgerRow,
    pub identity: IdentityKey,
    pub attribution: Attribution,
}
```

`attribute_rows(rows:Vec<LedgerRow>,granularity:RangeGranularity,config:&BillingConfig)->Result<Vec<AttributedRow>,UsageError>` 在本文件定义；bucket_key 按宿主本地时区转换成完整桶 `[start,end)`，使用 chrono 的 LocalResult，DST 多义/不存在时返回该桶 unknown/bucketBoundary，不 panic。小时/日边界不能统一写 24h 毫秒相加，日边界按本地日历求下一天。

分组键序列化 tuple，禁止用可碰撞的 `/` 拼接。渠道未知键含 sourceId+attributionKind；model 键含渠道身份或 unknown source + observedProvider + 完整 modelId；project 用旧 project_key。分组按 `requestTotal+output` 降序、key 升序打破平局，命中率重算，不平均各组百分比。`sourceIds` 去重排序。

趋势只筛选 B 的 rows，用相同 selection 再筛上期 rows；无上期实测量 compare=null。缺对象或渠道归档返回 available=false+reason，前端保留“全部”出口。全零不造线，TrendPoint 沿用原有空桶行为；是否整卡空态由过滤后的真实总量判断。

在本任务一并实现 `billing::preview_with_conn`：不写库，返回候选配置在查询范围内的命中、冲突、边界 Token，matchedTokens仅计可靠归属，三项互斥。复用本任务身份查询，避免任务3依赖尚未完成的任务4。

新增 `snapshot_with_conn(conn,range,now_ms)->Result<Vec<AttributedRow>,UsageError>` 给任务10复用；它只调用现有读取和规则归因，不引入扫描。总开关关闭返回空数据。`identities` 复用现有日档 parentsOnly 守卫，失败码保持 usage-filter-unavailable。
- [x] 4. 运行上述单测及 `cargo test --manifest-path src-tauri/Cargo.toml --lib services::usage::query`，全绿且旧 dashboard 字段不变。
- [x] 5. `git commit -m "feat: query attributed token distribution and scoped trends"`。

## Task 5：IPC、Mock 与查询失效

- [x] 1. `tests/usage/v2-api.test.tsx` mock invoke，逐一调用前七条包装，断言契约表中的命令、参数和返回值；`v2-queries.test.tsx` 用既有 QueryClient 测试模式断言 range/selection/groupBy 不同产生不同请求，保存成功失效 usage 前缀，读请求绝不调用 usageCollect。

```ts
it('趋势只向独立命令传局部筛选', async () => {
  await usageTrend({preset:'last7d'}, {scope:'tool',selectedId:'codex'});
  expect(invoke).toHaveBeenCalledWith('usage_trend', {
    range:{preset:'last7d'}, selection:{scope:'tool',selectedId:'codex'},
  });
});
```

- [x] 2. 执行 `pnpm test -- tests/usage/v2-api.test.tsx tests/usage/v2-queries.test.tsx`，确认失败于新模块缺失。
- [x] 3. 包装实现按公共契约逐条调用 invoke；新 `commands/usage_v2.rs` 各命令使用 `spawn_blocking` 包裹服务调用，命令层不取 DB 锁。登记 commands/mod.rs、lib.rs；服务生产入口持锁只调用 with_conn，不嵌套获取全局锁。查询 Hook 的完整实现模式：

```ts
export function useUsageTrendQuery(range:UsageRange,selection:TrendSelection) {
  return useQuery({queryKey:['usage','v2','trend',range,selection],
    queryFn:()=>usageTrend(range,selection),staleTime:30_000,refetchInterval:false});
}
export function useUsageDistributionQuery(range:UsageRange,groupBy:DistributionGroup) {
  return useQuery({queryKey:['usage','v2','distribution',range,groupBy],
    queryFn:()=>usageDistribution(range,groupBy),staleTime:30_000,refetchInterval:false});
}
```

身份查询增加 filters 入 key；billing 查询 key 为 `['usage','v2','billing']`。所有 import 从契约规定模块导入。mock 新分支用人工合成账本夹具，不把真实用量提交；保存/撤销更新 mock revision 并拒绝旧 revision，不能恒成功。
- [x] 4. 运行前端上述测试及 `cargo test --manifest-path src-tauri/Cargo.toml --test usage_ipc_test`。扩展新命令的 serde/注册检查；旧命令层自省锁不删除，新增命令文件也纳入“不取 DB 锁、不触发采集”的检查范围。
- [x] 5. `git commit -m "feat: expose usage v2 ipc and query hooks"`。

## Task 6：证明可计价基础，锁住保守分类

- [x] 1. 只读复核 `semantics::{normalize,resolve_semantics}`、`collectors/mod.rs::DeltaBuilder::detail`、`ledger::daily_rows_of`、DAO upsert。记录哪些计费条件已丢失；证据最少包含 Exclusive、Subset 有写、Subset 无写、混合、Backfill 五种。模型 ID 在源解析时被剥前缀的事实如实记录，不承诺完整原始模型字符串可恢复。
- [x] 2. 先在 `v2/mod.rs` 导出 `cost` 模块，费用公共类型按前文契约建立，再在 cost.rs 写纯测试的基础输入：

```rust
#[test]
fn overlapping_cache_write_cannot_be_charged_twice() {
    let b = UsageBuckets { input_fresh:80, cache_read:20, cache_write:10, output:30 };
    let safe = safe_components(&b, 100, true);
    assert_eq!(safe, [None, Some(20), None, Some(30)]);
}
#[test]
fn disjoint_input_without_write_is_recoverable() {
    let b = UsageBuckets { input_fresh:80, cache_read:20, cache_write:0, output:30 };
    assert_eq!(safe_components(&b,100,true),[Some(80),Some(20),Some(0),Some(30)]);
}
```

再测 `trusted=false` 全 None、负数/不一致总量全 None、reasoning 不重复追加、空总量 coverage=null、真实 0 价格和缺价不同。
- [x] 3. 运行 `cargo test --manifest-path src-tauri/Cargo.toml --lib services::usage::v2::cost`，确认新函数缺失红测，然后实现：

```rust
pub fn safe_components(b:&UsageBuckets, request_total:i64, trusted:bool)
    -> [Option<i64>;4] {
    if !trusted || request_total<0 || b.input_fresh<0 || b.cache_read<0
        || b.cache_write<0 || b.output<0 { return [None;4]; }
    let Some(base)=b.input_fresh.checked_add(b.cache_read) else {return [None;4]};
    let Some(with_write)=base.checked_add(b.cache_write) else {return [None;4]};
    if request_total==with_write {
        [Some(b.input_fresh),Some(b.cache_read),Some(b.cache_write),Some(b.output)]
    } else if request_total>=base && request_total<with_write {
        [None,Some(b.cache_read),None,Some(b.output)]
    } else { [None;4] }
}
```

`trusted` 不是“模型有价格”，而是源与数据形态的静态证据许可：七源标准 normalize 路径、非负规范桶与输出已合并 reasoning，且上游价是可适用的文本基准价。未来新增源默认 false。现有 backfill 的来源标记未进入 query 行，无法逐行可靠区分时，对相应源/档位采用更保守许可，不能从最后写入 sem 推断。许可函数定义为 `basis_trusted(source:UsageSourceId,granularity:RangeGranularity)->bool`，其白名单只能来自本步骤证据；证据不足返回 false，并向用户报告该源暂不能估价。

上式仅恢复聚合桶的互斥数量，不证明缓存 TTL、上下文阶梯适用；这些由 任务7 价格条件降级。发生重叠时整个普通输入与缓存写都留空，只保留已证明的 read/output，覆盖分母仍为 requestTotal+output。
- [x] 4. 重跑测试并记录推导：对已证明的 normalize 路径，`sum(fresh+read+write)-requestTotal` 等于 Subset 写重叠总量；非负增量前提下相等意味着无重叠。不得把该推导推广到未知历史数据形态。在本文件末尾的执行记录确定是否满足本 V1 保守策略；不满足则该源整行未估价，不开启额外采集。
- [x] 5. `git commit -m "feat: classify safe billing components from existing aggregates"`。

## Task 7：目录解析、精确键与定点价格

- [x] 1. 人工夹具使用以下完整 JSON；missing 删除 cost；invalid 将 input 改为 -1；conditional 加 `cost.context_over_200k` 对象。

```json
{"p":{"id":"p","name":"参考商","models":{"m":{"id":"m","name":"M",
 "cost":{"input":2,"output":10,"cache_read":0.2,"cache_write":2.5},
 "modalities":{"input":["text"],"output":["text"]}}}}}
```

pricing.rs 测试 parse_catalog 接受此对象；missing 保留模型但其报价缺失；invalid 不替换快照；conditional 的输入/缓存项不能完整估价；输入 provider/model 键必须精确，不用 canonical_model_id 自动授权别名。
- [x] 2. 执行 `cargo test --manifest-path src-tauri/Cargo.toml --lib services::usage::v2::pricing`，确认红测。
- [x] 3. 定义内部类型并实现解析：

```rust
#[derive(Debug, Clone)]
pub struct PriceQuote {
    pub reference: PricingReference,
    pub rates: [Option<i128>;4],
    pub conditional: bool,
}
pub struct PriceCatalog {
    pub quotes: std::collections::BTreeMap<(String,String),PriceQuote>,
}
pub fn parse_rate(s:&str)->Option<i128> {
    if s.starts_with('-') || s.contains(['e','E']) {return None;}
    let (whole,frac)=s.split_once('.').unwrap_or((s,""));
    if frac.len()>9 || !whole.chars().all(|c|c.is_ascii_digit())
      || !frac.chars().all(|c|c.is_ascii_digit()) || whole.is_empty() {return None;}
    let scale=10_i128.pow((9-frac.len()) as u32);
    whole.parse::<i128>().ok()?.checked_mul(1_000_000_000)?
      .checked_add(if frac.is_empty(){0}else{frac.parse::<i128>().ok()?.checked_mul(scale)?})
}
```

费率单位为每百万 Token 的 10^-9 USD。`parse_catalog(bytes:&[u8])->Result<PriceCatalog,UsageError>` 使用 serde_json::Value 按 provider/models 外键查；cost 可缺。已出现数值不能是负、非数值或溢出；未知 cost 字段标记 conditional，未知非价格元数据忽略。数字先由 JSON Number 转字符串，解析失败保守拒绝该报价；不能先转 f64。模型 modalities 非纯文本或无法确认类型的报价基准条件不够，保留条目并标记 conditional。空目录、错误顶层结构视整体无效；模型缺价不导致全目录失败。

conditional 只允许已独立证明无条件的项；无法判断受影响项时整报价未估价。不要从 `limit.context` 猜是否触发价格阶梯，不将 reasoning 价再次乘已合并 output。
- [x] 4. 重跑 pricing 测试；加入已明确 input=0 的值通过、缺键为 None、超大数和精度超过9位降级、不同 provider 同 model 为两报价、未知阶梯字段不能静默按基准完整估价。
- [x] 5. `git commit -m "feat: parse validated public pricing without guessed aliases"`。

## Task 8：有效快照、启用与 24h 刷新

- [x] 1. pricing.rs 用内存库和注入下载闭包写测试：首次 disabled 不请求；enabled 无快照请求一次；23h 不请求；24h 请求；并发刷新单飞；无效响应/超时保留旧 active；重启可读旧快照；关闭后不自动请求；失败更新 lastError 而不更新时间。
- [x] 2. 运行 pricing 过滤测试，确认新存储/调度逻辑红测。
- [x] 3. schema 增加：

```sql
CREATE TABLE IF NOT EXISTS usage_pricing_state (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1), enabled INTEGER NOT NULL DEFAULT 0,
 active_snapshot_id TEXT, last_error TEXT
);
CREATE TABLE IF NOT EXISTS usage_pricing_snapshot (
 id TEXT PRIMARY KEY, source_url TEXT NOT NULL, fetched_at INTEGER NOT NULL,
 payload_json TEXT NOT NULL
);
```

`snapshotId` 定义为 `sha256(payload) + ':' + fetched_at`，哈希部分用于内容去重诊断，时间部分保留每次有效获取版本。重复 payload 仍保留旧版本，刷新成功更新 active 的获取时间。禁止以模型发布日期充当价格日期；显示快照获取时间。

DAO 定义 `pricing_status_conn(conn,now_ms)->Result<PricingStatus,UsageError>`、`activate_snapshot_conn(&mut conn,id,url,fetched_at,payload)->Result<(),UsageError>`、`set_pricing_enabled_conn(conn,enabled)->Result<(),UsageError>`。activate 一个事务写新 snapshot+切 active并清last_error；失败旧 active 不变。`stale = active缺失 || now_ms-fetched_at>=24小时 || last_error非空`，手动刷新失败也不能继续显示“最新”。只下载固定 `https://models.dev/api.json`，GET，无 query/body 的本地内容，无授权头；20s 超时、20MiB 响应上限、检查 HTTP 状态；网络线程不持 DB 锁，下载后先 parse 再短锁写入。上限按流式累计字节执行，不能只相信 Content-Length。

生产 `refresh(force:bool,now_ms:i64)->Result<PricingStatus,UsageError>` 使用 tokio Mutex 单飞；按期刷新再次检查 enabled/active 时间。启动时只在已经 enabled 时安排后台刷新；新增 `spawn_pricing_refresh_loop()`，在 lib.rs setup 后台启动，5分钟检查一次24h过期条件，关闭时不请求。首次启用立即后台刷新；手动刷新返回状态供 UI 展示进行中；不依赖窗口必须打开。所有源缓存存在 tuvis.db，不创建有 Windows rename 覆盖问题的临时价格文件。

可测试内核 `refresh_with_conn(conn:&mut Connection,now_ms:i64,force:bool,fetch:impl FnOnce()->Result<Vec<u8>,UsageError>)->Result<PricingStatus,UsageError>` 仅用于私有连接；生产分阶段解锁实现相同校验，不把 DB conn 跨 await。shutdown 由 Tauri runtime 结束，不另外启动常驻进程。
- [x] 4. 运行单飞/失败回退测试；验证无快照 status fetchedAt=null、stale=true，关闭仍可用最后有效价格离线估算；从未启用且无缓存时 CostRow 金额为空。网络失败不影响旧 dashboard/records。
- [x] 5. `git commit -m "feat: refresh public pricing snapshots with offline fallback"`。

## Task 9：金额、覆盖率与估价状态

- [x] 1. cost.rs 测试 [fresh=100,read=20,write=10,out=30]，requestTotal=130，费率 [2,.2,2.5,10]，金额 USD=.000529，pricedTokens=160，coverage=1；缺缓存写价时 coverage=150/160；Subset [80,20,10,30] requestTotal=100 仅 read/output 可计价，覆盖50/130；价格全零 status=priced 且金额非 null；无价 status=unpriced、estimatedUsd=null。
- [x] 2. 运行 cost 测试，确认尚无完整估算返回值的红测。
- [x] 3. 增加纯接口 `estimate_components(b:&UsageBuckets,request_total:i64,safe:[Option<i64>;4],rates:[Option<i128>;4])->Result<Estimate,UsageError>`，`Estimate {components:[Option<ComponentEstimate>;4],amount_scaled:Option<i128>,priced_tokens:i64,unpriced_tokens:i64,coverage_rate:Option<f64>,status:CostStatus}`。`ComponentEstimate {tokens:i64,rate_scaled:i128,amount_scaled:i128}`。每项 amount_scaled=tokens*rate_scaled，用 checked_mul/add；最终金额单位10^-15 USD，所有行合并完成才转字符串，禁止每请求先四舍五入。

```rust
pub fn usd_string(value:i128)->String {
    let whole=value/1_000_000_000_000_000;
    let fraction=value%1_000_000_000_000_000;
    let tail=format!("{fraction:015}");
    let tail=tail.trim_end_matches('0');
    if tail.is_empty(){whole.to_string()}else{format!("{whole}.{tail}")}
}
```

只对 safe 与 price 同时 Some 的互斥项计算；0 Token 且缺价不产生未计价 Token。至少有一个正 Token 项实际可计价，或总量>0且全部对应费率明确为0时 amount 为 Some；无可计价正量时 None。pricedTokens 为已计价互斥项数量和，不加重叠写；unpriced=规范总量-priced，负值/溢出直接返回 internal 并由行级查询降为未估价。total=0 时 coverage=null，UI 不展示假100%。status：total>0且priced=total 为 priced；0<priced<total 为 partial；否则 unpriced。

estimatedUsd 字符串精度完整保留，UI 再格式化；conditional 若仍采用可证明的基准报价，pricingBasis=baseCurrent，明确“当前基准价回算，非历史实际价”；条件不可确认的桶不计价，reasons 带 `pricingConditionUnknown`。
- [x] 4. 重跑 cost 测试，增加大整数、舍入一次、缺价非零价替代、推理 output 只算一次、混合语义整普通输入不计价的用例。
- [x] 5. `git commit -m "feat: estimate model cost with explicit token coverage"`。

## Task 10：费用查询、引用规则与 CSV

- [x] 1. 新 `usage_cost_test.rs` 用私有库写四组：相同模型不同 profile 不合并；同 profile 不同 reference 不合并；未知按 source/provider/model 保持上下文；相同 key 不同 pricingBasis 不合并。断言结果没有任何 total 金额字段，原 Token/游标均不变。CSV 以以下 header 精确断言：

```text
billing_profile_id,billing_profile_name,source_ids,observed_provider,model_id,attribution_kind,token_usage,estimated_usd,status,priced_tokens,unpriced_tokens,coverage_rate,pricing_provider,pricing_model,pricing_snapshot_id,pricing_basis,price_updated_at
```

- [x] 2. 运行 `cargo test --manifest-path src-tauri/Cargo.toml --test usage_cost_test`，确认新查询尚未实现。
- [x] 3. 实现 `cost_query::costs_with_conn(conn:&Connection,range:&UsageRange,now_ms:i64)->Result<UsageCosts,UsageError>`；`cost_csv_with_conn` 相同参数返回 String。复用任务4的 `snapshot_with_conn`，无网络/扫描。保留未估价行。reference 先匹配用户 PricingRule（exact>最长prefix，同级冲突不选）；无规则只对 sourceKind=measured 的精确 provider+model 尝试目录键，失败不猜 alias。用户在任务17选择替代模型时新增 PricingRule。

内部估价分组键是 tuple：`(resolved_channel_key, original_model, pricing_reference, pricing_basis, snapshot_id, "USD")`。unknown resolved_channel_key 含 source/provider/model 与 attributionKind；明确相同渠道+模型+reference+basis 可跨 source 累计，用 sourceIds 记录集合。CostRow.identity 仅是代表身份，组件不能用它重新绑定整组，绑定入口继续走 IdentityUsage。混合 sourceKind 的行保留来源信息供详情解释。

先逐 ledger row 分类/估价，再合并相同 key；不能先把 mixed rows 全合成一个缓存语义。组件金额相加后再格式化，coverage 按汇总 priced / 汇总规范 Token 重算。单一 output price 存在不等于整行已估价。

CSV 实现 `csv_cell(s:&str)->String`：双引号替换成两个双引号，整格加双引号；纯整数/coverage按原值输出，缺金额空字段而非0，无 BOM（既有保存层负责）。不加总计行；不依赖 D 当前显示Top5或分组模式；导出所选区间全部费用行。字段头和状态为稳定机器标识；公式前缀的用户名称按现有导出策略处理，不能破坏CSV列数。
- [x] 4. 运行集成测试；补价格快照失败/无缓存/历史90天外仅daily、覆盖率守恒、CSV逗号和双引号/换行名称测试。金额/Token必须同一行身份。
- [x] 5. `git commit -m "feat: query and export model-level equivalent cost rows"`。

## Task 11：IPC 与后端集成检查

- [x] 1. 扩展 `tests/usage/v2-api.test.tsx`，断言后五条命令的参数；CostRow 中金额为string|null，snapshotId为string|null，components四键全部出现，覆盖率0–1或null，无 totalCost。
- [x] 2. 按公共契约加入后五条 IPC/包装/TS镜像/mock。费用读查询不刷新价格；pricingSetEnabled(true) 只启动后台公开目录请求。useUsageCostsQuery key为 `['usage','v2','costs',range]`；useUsagePricingQuery key为 `['usage','v2','pricing']`。刷新完成失效 pricing/costs 查询，通过事件 `usage-pricing-updated` 广播给已打开窗口，监听清理函数卸载时调用；不要增加 Token 三秒轮询。
- [x] 3. `cargo test --manifest-path src-tauri/Cargo.toml --test usage_cost_test`；`cargo test --manifest-path src-tauri/Cargo.toml --lib services::usage::v2`；`pnpm test -- tests/usage/v2-api.test.tsx tests/usage/v2-queries.test.tsx`；`pnpm check`，期望全绿。
- [x] 4. 人工检查下载的请求只有公开URL；先在线获得快照，再断网请求费用仍有旧快照+stale标记；旧Token查询仍成功。仅用合成账本，禁止真实账本force重采。
- [x] 5. 本文件末尾的执行记录列出每源/档位的可信许可、目录匹配条件与降级案例，补 `CHANGELOG.md` 未发布小节；`git commit -m "docs: record pricing coverage and validate cost backend"`。

## Task 12：总览、折叠、趋势和 Top 5 分布

- [x] 1. 新 `v2-summary.test.tsx` 锁住如下数值派生；`v2-page.test.tsx` 断言 A/B 无折叠钮、C 初始展开、E 收起，切范围/页签状态不丢；趋势选择 codex 后全局 hero 不变；分布切换不改变趋势。`v2-distribution.test.tsx` 构造 7 行，默认 5，展开 7，按 hero 排序，命中率显示且无美元列。

```ts
expect(summaryNumbers({requestTotal:100,cacheHitRate:.3,userEst:9,requests:2},
  {inputFresh:60,cacheRead:30,cacheWrite:10,output:20})).toEqual({
  total:120,requestInput:100,cacheRead:30,cacheMiss:70,output:20,
  hitRate:.3,requests:2,
});
```

- [x] 2. 执行 `pnpm test -- tests/usage/v2-summary.test.tsx tests/usage/v2-distribution.test.tsx tests/usage/v2-page.test.tsx`，确认期望行为尚未实现。
- [x] 3. `summaryV2.ts` 实现：

```ts
export function summaryNumbers(m:UsageMetrics,b:UsageBuckets) {
  return {total:m.requestTotal+b.output,requestInput:m.requestTotal,
    cacheRead:b.cacheRead,cacheMiss:m.requestTotal-b.cacheRead,output:b.output,
    hitRate:m.cacheHitRate,requests:m.requests};
}
```

UsageSummary 删除 userEst 局部变量、GridKey 和该行；四核心按 requestTotal/cacheRead/cacheMiss/output 顺序，辅助 hitRate/requests，asOf 保留。cacheMiss 上期值同公式；原 tokenUnmeasured 的空态保护沿用；cacheWrite 放 details 解释。网格大数字不换行。

新增可复用壳，Card 内仍使用原组件内容；原三张卡拆出 body 渲染以避免 Card 嵌套：

```tsx
export function UsageFold({id,title,open,onOpenChange,children}: {
  id:string;title:string;open:boolean;onOpenChange:(v:boolean)=>void;
  children:React.ReactNode;
}) {
  return <Card><CardHeader><CardTitle>{title}</CardTitle><CardAction>
    <Button variant="ghost" size="sm" aria-expanded={open} aria-controls={id}
      onClick={()=>onOpenChange(!open)}>{open?'−':'+'}</Button>
  </CardAction></CardHeader>{open&&<CardContent id={id}>{children}</CardContent>}</Card>;
}
```

按钮补 `aria-label={title}`，减号/加号不作为唯一可访问名称。页面最上层维护 `useState({distribution:true,cost:false,work:false})`，不放到 board 条件子树；另有行数展开状态。E 卡头 sessions/toolCalls 摘要用真实数值/null；高级时长在 E 展开后第二个 details 内，保留 idle 提示，不合计错误和 turn。

新增 `UsageTrendFilters` 使用标签明确的原生 select 和现有 Button；all/tool/billingProfile 模式各只有一个对象，未知渠道选项映射 scope=unknown。选择数据从七源和 billing/identities 查询获取。把筛选插槽放在 UsageTrendCard 卡头；目前该卡空态早退会丢卡头，改成总返回 Card，只替换内容区。busy 同时隐藏旧峰值和旧环比。

`distributionV2.ts` 返回全部正值行的排序和相对条，不改旧 `distributionRows` 的 10 行常量，避免影响固定分享。页面 C 默认 billingProfile；四个按钮 billingProfile/tool/project/model；显示缓存率，最多 5 行可展开。label 用服务返回值，unknown label 由 i18n 翻译，完整模型保留 title。

页面旧 `groupBy:UsageGroupBy` 保留给固定分享/Token CSV（初始 tool），新建 `distributionGroup:DistributionGroup` 给 C；导出条必须明确 Token CSV 按工具，不能误称按渠道导出。B 数据来自新 trend，A 来自旧 dashboard，C 来自新 distribution；分区请求错误只替换对应卡体。
- [x] 4. 跑定向测试并更新旧 `tests/pet/usage-{page,cards,worksummary}.test.tsx` 中已被新设计替代的顺序/10行断言。不得删除空态、真零、主题与金额缺席断言。
- [x] 5. `git commit -m "feat: reorganize token dashboard with local filters and folds"`。

## Task 13：记录页关联 Dialog 与设置规则管理

- [x] 1. `v2-identity-ui.test.tsx` mock `usageIdentities` 返回同模型不同工具两身份，项目卡点指定渠道时显示两个可选身份，不能合并。断言打开即展示只读 source/provider/model；默认 exact 和“今后”；先预览再允许保存；修改候选后旧预览作废；保存冲突保留草稿并重拉；撤销产生新版本；空名不能保存。
- [x] 2. 执行 `pnpm test -- tests/usage/v2-identity-ui.test.tsx`，确认新组件缺失红测。
- [x] 3. 新 Dialog Props 明确为 `{open:boolean,onOpenChange(v:boolean):void,identities:IdentityUsage[],range:UsageRange,t:TFn}`，新 Manager Props 为 `{t:TFn}`。读取 billing query，局部草稿结构为 BillingConfig。新建渠道使用 `crypto.randomUUID()`；新增规则复制完整 identity，不读截断文本。候选变化清 `preview`；提交只使用当前 preview.baseRevision+candidate：

```ts
const preview = await usageBillingPreview(candidate, range);
const saved = await usageBillingCommit({
  expectedRevision:preview.baseRevision,candidate:preview.candidate,
});
await queryClient.invalidateQueries({queryKey:USAGE_KEY});
```

实际交互将两句分别置于“预览”和“确认保存”按钮，不能在同次点击连续执行。显示匹配工具、模型、范围、matched/ambiguous/boundary Token。mixed 开关文案“相同日志身份可能混合多个账户，无法拆分”。历史区间选择日期需二次预览，不可默认勾选。规则管理支持列出、改日期/目标/前缀、停用（移出当前快照）、查看冲突和选择历史 revision 撤销；档案保留。

记录页保持旧 cards/rows，每行新增“指定渠道/修改关联/查看规则”。点击后再按卡片限制查询：tool 卡使用 `{...filters,toolIds:[card.toolId]}`；project 卡使用 `{...filters,projects:[card.toolId]}`，其中 project 卡的 toolId 实际是项目键，不能当 sourceId。后台返回带 sourceId 的完整 IdentityUsage。只用完整身份构造旧路由显示名（provider 非空为 `provider + ' / ' + modelId`，空为 modelId）与所点行的完整 row.label 比较，绝不拆分字符串恢复身份。存在同名碰撞时展示所有匹配候选让用户选择；不能静默选第一项。既有行会跨渠道聚合时，Dialog 同时展示各候选当前归属及 Token。保留 SourceBadge，并加用户指定/含糊标记。设置 UsageSection 增“计费渠道与规则”按钮打开 Manager，不恢复 providerMapRules 文本框。
- [x] 4. 运行新 UI 测试及 `pnpm test -- tests/pet/usage-records.test.tsx tests/pet/usage-settings.test.tsx`；逐个验证关闭对话框不误保存、预览失败可重试、用户无法选 archived 新渠道。
- [x] 5. `git commit -m "feat: manage billing assignments through preview dialogs"`。

## Task 14：分享摘要、双语与中间回归

- [x] 1. 在 `tests/pet/usage-export-text.test.ts` 与 `usage-export-image.test.ts` 加 userEst=999 的夹具，断言摘要和 layout 输入均不包含用户输入估值；Token CSV 测试维持原始列。先执行确认红测。
- [x] 2. `exportText.ts` 删除 userEst 的 lines.push；`useExportShare.ts` 删除 metrics 数组的 userEst 项，其他模板结构、立绘、导图尺寸与旧数据源保持。新增 cacheMiss 核心展示键：zh `未缓存命中输入` / en `Cache-miss input`；渠道 `未指定渠道` / `Unassigned channel`；混合 `渠道可能混合` / `Channel may be mixed`；边界 `当前聚合粒度无法拆分此时间边界` / `This time boundary cannot be split at the current aggregation granularity`。所有其余新按钮/状态键同步 zh/en，保留兼容 userEst 翻译键。
- [x] 3. 执行 `pnpm test -- tests/usage tests/pet`、`pnpm check`、`cargo test --manifest-path src-tauri/Cargo.toml --test usage_billing_test`、`cargo test --manifest-path src-tauri/Cargo.toml --lib services::usage`。期望退出 0；因环境缺工具失败应如实记录，不能报通过。
- [x] 4. 用浏览器 mock 页面在 1040×720 和最小宽度 840，zh/en×浅/深主题取四组截图；A/B 完整可见、C 标题可见，D 在任务16接入，当前为内部检查，E 默认收起；切页签/范围折叠保留。普通截图不显示已折叠卡体，导图仍为固定模板。验收环境用 `pnpm dev`，不启动 tauri 真实采集。
- [x] 5. 更新 `README.md`、`README.en.md` 的看板/渠道行为和中英文用户手册实际受影响章节（先 `rg --files docs/user-manual` 定位用量章节）；`CHANGELOG.md` 未发布小节记录 Token 相关变化。`git commit -m "docs: describe token dashboard changes"`。此为内部文档同步，任务19再统一核对；不创建 release tag 或假定版本号。

## Task 15：费用组织纯函数与金额格式

- [x] 1. costRows 测试放合成 CostRow 夹具：相同模型两个profile分开；unknown两个source分开；channel模式无金额小计；model模式每行含渠道名；Top5仅隐藏显示不丢行；全未估价仍按Token降序。金额格式测试：null → 未估价，'0' → '$0.00'，'0.000529' → '$0.000529'，大数不转科学计数。
- [x] 2. 执行 `pnpm test -- tests/usage/v2-cost-card.test.tsx`，确认新模块缺失红测。
- [x] 3. `costRows.ts` 定义 `CostView='channel'|'model'`；`organizeCosts(rows:readonly CostRow[],profiles:readonly BillingProfile[],view:CostView):{key:string;label:string;rows:CostRow[]}[]`。按 tokenUsage降序/key升序稳定排序，channel 模式 Map 键为 billingProfileId 或序列化 `['unknown',sourceIds,attributionKind,identity.observedProvider]`，model 模式每个 CostRow 一个组织项；标签查profile字典，unknown用翻译键交给组件翻译。组织项不含金额字段，不在纯函数内乘价。

金额格式完整实现：

```ts
export function formatUsd(value:string|null):string|null {
  if(value===null) return null;
  if(!/^\d+(\.\d+)?$/.test(value)) return null;
  const [whole,raw='']=value.split('.');
  const fraction=raw.replace(/0+$/,'');
  const grouped=whole.replace(/\B(?=(\d{3})+(?!\d))/g,',');
  return `$${grouped}.${fraction.padEnd(2,'0')}`;
}
```

完整金额保留在title及详情，普通长小数可用视觉截断但不能显示'$0.00'掩盖小额正值。`formatUsd` 不做浮点转换。
- [x] 4. 重跑上述测试，新增超大金额、非法字符串降级、两种视图保留相同row.key集合的断言。
- [x] 5. `git commit -m "feat: organize equivalent cost rows without subtotals"`。

## Task 16：D 费用卡与行解释

- [x] 1. `v2-cost-card.test.tsx` 渲染 defaultOpen=false，断言无费用行；点击展开后 channel 模式默认、5行和更多；切 model 后同模型不同渠道各自显示。partial 行同时出现“已估价部分”和覆盖率；unpriced 行出现Token与未估价，不出现'$0.00'；明确0价显示'$0.00'；不存在总计字段/渠道小计。
- [x] 2. 执行该测试，确认缺新组件或行为未实现红测。
- [x] 3. 新 `UsageCostCard` Props：`{range:UsageRange;open:boolean;onOpenChange(v:boolean):void;t:TFn}`。查询任务11的 costs 与任务5的 billing；使用任务12的 UsageFold。D 收起时标题/估价说明/管理渠道入口可见，正文不挂载；启用下载必须用户显式操作，展开卡不自动启用。加载/错误只影响D，可重试；stale快照展示日期及降级标记；无快照显示开启定价入口。

`UsageCostRow` Props：`{row:CostRow;channelName:string;t:TFn}`。行首渠道/完整模型，列为金额、Token、命中率、覆盖率；详情使用原生 details/summary，内容为四项tokens/单价/金额、snapshotId、priceUpdatedAt、baseCurrent说明与reasons。不存在的component显示“该部分未估价”，而不是0。金额渲染规则：

```tsx
const amount=formatUsd(row.estimatedUsd);
const amountText=row.status==='unpriced'||amount===null
  ? t('usage.cost.unpriced')
  : row.status==='partial'
    ? t('usage.cost.partialAmount',{amount}) : amount;
```

`coverageRate===null` 显示EM_DASH，其他用现有fmtPct；Token用fmtInt/title完整值与fmtTokens语言缩写；不把 attributions 的 unknown 当作没 Token。查看更多全卡按Token前5，展开保留全部组织项，无空渠道容器。Top5状态与C、D折叠分离，切范围不重置卡折叠。

页面在C后/E前插入：

```tsx
<UsageCostCard range={range} open={folds.cost}
  onOpenChange={cost=>setFolds(v=>({...v,cost}))} t={t}/>
```

open 状态使用任务12的页面顶层状态；不得将费用卡塞入旧dashboard错误分支，使Token失败拖垮独立费用状态。
- [x] 4. 跑费用卡和 `tests/usage/v2-page.test.tsx`，验证A/B固定、C展开、D/E折叠的最终顺序；键盘Tab/Enter/Space能操作，aria-expanded/controls正确。
- [x] 5. `git commit -m "feat: show explainable model-level equivalent api costs"`。

## Task 17：定价设置、明确参考模型与手动刷新

- [x] 1. `v2-cost-settings.test.tsx` 测试首次未开启、点击开启后可见后台刷新；失败显示上次有效时间和重试；关闭停止后续自动请求；参考模型必须用户选择并预览，不能按相似名自动保存；渠道改名不修改ID/历史Token。
- [x] 2. 执行设置测试确认红测；只mock公开下载响应，不访问真实账号。
- [x] 3. `PricingStatusPanel` Props 为 `{t:TFn}`，复用任务11的pricing查询；按钮触发 usagePricingSetEnabled/Refresh，显示刷新状态/失败原因/来源models.dev/时间。监听任务11的更新事件后失效查询；仅在手动刷新进行中可短暂重查状态，完成/失败/卸载立即停止，不给Token增加轮询。UsageSection 增定价目录状态入口，与任务13的渠道规则管理并列。

BillingProfilesManager 扩展“参考API模型”编辑：输入明确 providerId+modelId（目录精确键，文本字段仅用于键名，不让用户编辑规则 JSON），可从公开目录进行搜索时新增本地目录查询接口必须先更新公共契约；本V1优先采用精确键输入与后台校验，不引入未定义搜索接口。preview 显示原日志身份和目标参考键，并标用户授权；确认后作为独立PricingRule保存，失败保留草稿。不将profile.kind影响公式，不接受月费字段推算单价。

为避免前端提交任意无效价格引用，本任务扩展billing preview/commit，在pricingRules变更时校验当前有效目录存在对应键；无快照给usage-settings-invalid并提示先启用/刷新。删除或撤销已有配置不依赖在线网络，验证使用最后有效本地快照；撤销历史目录已无该模型时保留引用并标unpriced，不阻止撤销。
- [x] 4. 跑设置及身份UI测试；验证开启外部请求前清楚说明只下载公开价目表、本地用量不上传；原Token总开关与定价开关分别有效。
- [x] 5. `git commit -m "feat: manage pricing status and authorized model references"`。

## Task 18：独立费用 CSV 导出

- [x] 1. `v2-cost-export.test.tsx` mock usageCostExportCsv/exportSaveText/revealDir，点击费用CSV断言传完整range、不传B局部筛选、不混入旧usageExportCsv；D Top5仅展示不截断导出；失败提示且不伪报保存成功。

```ts
expect(usageCostExportCsv).toHaveBeenCalledWith({preset:'last7d'});
expect(usageExportCsv).not.toHaveBeenCalled();
expect(exportSaveText).toHaveBeenCalledWith(
  expect.stringMatching(/^tuvis-api-equivalent-/),expect.any(String));
```

- [x] 2. 执行测试确认红测。
- [x] 3. UsageExportActions 增一个明确命名动作“费用CSV”，复用既有保存接缝和错误展示。调用序列如下，filename 用当前时间稳定格式且.csv后缀：

```ts
const content=await usageCostExportCsv(range);
const path=await exportSaveText(filename,content);
await revealDir(path);
```

与旧CSV、复制Token摘要、导出图片并列，费用CSV只包含当前时间窗全部模型费用行，不含费用总计。保存失败/定位失败区别显示：保存成功但定位失败仍提供绝对路径，不提示重新导出造成误解。不在前端加第二个BOM；复用已有后端保存实现。
- [x] 4. 运行新导出测试及 `pnpm test -- tests/pet/usage-export-csv.test.tsx tests/pet/usage-export-text.test.ts tests/pet/usage-export-image.test.ts tests/pet/usage-export-layout.test.ts tests/pet/usage-minibar.test.tsx`；原Token CSV和mini bar口径不变。
- [x] 5. `git commit -m "feat: export equivalent cost rows separately from token usage"`。

## Task 19：双语文案、完整回归与交付

- [x] 1. 文案至少包含下列对应值，在现有usage命名空间下增加cost：

```json
{"title":"API 等效费用","disclaimer":"按模型参考 API 价估算，非实际支出",
 "unpriced":"—（未估价）","partialAmount":"已估价部分 {{amount}}",
 "coverage":"估价覆盖率","baseCurrent":"当前基准价回算",
 "stale":"价格快照已过期，正在使用最近有效版本","exportCsv":"费用 CSV"}
```

```json
{"title":"API equivalent cost","disclaimer":"Estimated from reference API prices, not actual spend",
 "unpriced":"— (unpriced)","partialAmount":"Priced portion {{amount}}",
 "coverage":"Pricing coverage","baseCurrent":"Recalculated at current base prices",
 "stale":"Pricing snapshot is stale; using the last valid version","exportCsv":"Cost CSV"}
```

其他新输入/按钮/失败原因逐键提供英文，不能只跑键集一致就宣布英文完整。
- [x] 2. 新 `v2-acceptance.test.tsx` 用合成夹具串起：未知身份→预览绑定→确认→趋势/分布失效更新→D同模型不同渠道两行→撤销→原Token总量恢复同一值；预览不改变界面归属，确认失败不乐观篡改成功态。mock key和真实IPC参数一一对应。
- [x] 3. 执行 `pnpm test -- tests/usage tests/pet tests/settings/UsageStatusSection.test.tsx`、`pnpm check`、`cargo test --manifest-path src-tauri/Cargo.toml`、`cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings`、`cargo fmt --manifest-path src-tauri/Cargo.toml -- --check`。期望退出0；测试中的环境不支持项单独记录，不改门禁放行。
- [x] 4. 浏览器mock视觉矩阵：1040×720与840宽；zh/en、浅/深；默认布局、D展开、E高级详情、长模型、无价、partial、stale、零缓存命中。首屏完整A/B且C标题可见；吸顶不遮挡；数字不溢出。普通截图尊重折叠，固定分享图仍固定模板。验证离线Token正常且D用缓存/未估价，而非全页错误。
- [x] 5. 主 Agent 调用独立 Review Agent 对全部最终 diff 做整体验证：重新按 Spec 十项验收、接口契约、全部依赖检查一遍；发现问题交执行 Agent 修复并复审，不能仅汇总各任务旧报告。完成后再同步 README.md/README.en.md 与实际受影响用户手册章节，CHANGELOG未发布小节记录功能与行为变化。无需此时编写假定版本release notes；正式发版前以真实上一tag做逐commit覆盖清单，交用户确认，然后同步对应release notes、capabilities.json与README矩阵并跑中英对照。`git commit -m "docs: describe equivalent pricing and complete usage v2 validation"`。


- [ ] 6. 所有内部任务、红绿测试、整体验证及最终 Review 通过后，主 Agent 一次性交付用户：完整功能说明、Spec十项验收清单逐项证据、命令/退出码、视觉截图、已知数据限制、修改摘要及提交信息。用户随后整体验收；内部通过不能代替用户确认。不在任务12或任务14等中间位置要求用户验收。

## 集成复核附件（本计划内嵌）

## 设计覆盖矩阵

| 设计章节/要求 | 实施任务 | 复核结果 |
| --- | --- | --- |
| §1 七源采集/去重不变、只读账本 | 公共约束1–3，任务1/任务4，任务6 | 不改Collector/游标/历史事实 |
| §1 无全局费用/订阅额度/代理路由 | 公共约束7，任务10，任务15/任务16 | 没有总费用接口或顶层费用页签 |
| §2.1 独立窗口、五档范围、两页签、吸顶 | 任务12/任务14，任务19 | 沿用旧范围与窗口，不新增时钟口径 |
| §2.1 A/B不可折叠、C开、D/E关 | 任务12，任务16 | 页面顶层保存窗口内状态 |
| §2.1 1040×720、840最小宽、原主题 | 任务14，任务19 | 视觉验收覆盖中英/浅深 |
| §2.1 普通截图与固定分享模板区分 | 任务14，任务18/任务19 | 不改所见即所得导图 |
| §2.2 请求/命中/未命中/输出、命中率/次数 | 任务12 | 公式与上期比较一致，缓存写只详情 |
| §2.2 移除userEst展示/分享、兼容列保留 | 任务14，任务18 | CSV兼容列保留，页面/人读摘要移除 |
| §2.3 趋势内工具/渠道筛选、A/C不联动 | 任务4、5、12 | 独立selection与查询，峰值/环比同筛选 |
| §2.3 空态、无旧峰值、缺上期无百分比 | 任务4/任务12 | 从过滤后数据派生，不用全局hero |
| §2.4 四维分布、渠道默认、Token降序 | 任务4/任务12 | 新维度类型不污染旧枚举 |
| §2.4 Top5/更多/命中率、无美元列 | 任务12 | 独立纯函数，不改分享10行助手 |
| §2.5 D渠道/模型视图、无任何小计 | 任务10，任务15/任务16 | 分组对象没有金额汇总字段 |
| §2.5 四分项/价格日期/口径/覆盖率 | 任务7/任务9/任务10，任务16 | 快照取回时间与当前价回算说明可见 |
| §2.5 缺价非免费、部分价不冒充全价 | 任务9，任务15/任务16 | null与明确0分离 |
| §2.6 E摘要、错误分层、高级耗时 | 任务12 | 无错误总数，无跨工具turn合计 |
| §2.7 记录每行绑定与可视化设置 | 任务13 | 同一旧行可能含多个来源，Dialog再选择 |
| §2.7 Token导出不变/独立费用CSV | 任务14，任务10，任务18 | 旧CSV不接受新渠道枚举 |
| §3 原身份/付款渠道/参考价分离 | 公共类型，任务2–任务5，任务10 | sourceKind与attributionKind不混用 |
| §3 精确/前缀/冲突/日期/混合/撤销 | 公共匹配规则，任务2/任务3/任务13 | 不按先后覆盖；回滚生成新revision |
| §4.1 models.dev公开GET、24h/手动/离线 | 任务7/任务8/任务11，任务17 | 下载不持DB锁，不发本地数据 |
| §4.1 别名必须授权、复杂价格降级 | 任务7/任务10，任务17 | 不按相似模型名补价 |
| §4.2 互斥桶/Subset写/推理/覆盖率 | 任务6/任务9 | 重叠普通输入不计价，输出不重复加reasoning |
| §4.3 历史兼容、90天后不伪造明细 | 公共边界，任务4，任务6/任务10 | daily直接查询，信息缺口显式降级 |
| §5 模块、输入输出、UI边界 | 公共文件结构/类型/IPC表 | 新函数由任务明确定义 |
| §6 空态/性能/隐私/双语/全部验收 | 任务14，任务11，任务19验收表 | 本轮只定义检查，不假报测试通过 |
| §7 五项探测门禁、依赖任务与统一交付 | 任务1、6、7及前文契约 | 门禁是相应编码任务的前置动作 |

## 现有接口与设计逻辑的差异

| 现有证据 | 实际情况 | 计划处理 |
| --- | --- | --- |
| `src/lib/api/usage.ts:32` | `usageDashboard(range,groupBy)`，没有filters/selection | 保持原签名，新usageTrend与usageDistribution |
| `src-tauri/src/services/usage/query.rs:454` | `dashboard(range:&UsageRange,group_by:UsageGroupBy,now_ms:i64)` | 不给旧函数凭空传第四参数 |
| `query.rs:467` | 注入连接版额外首参conn | 新V2也分别定义生产/with_conn入口 |
| `src/types/usage.ts` | UsageGroupBy仅tool/project/provider/model | 新DistributionGroup含billingProfile，旧CSV继续旧枚举 |
| `src/lib/api/usage.ts:44`、`query.rs:631` | 记录卡仅tool/project，卡内行供应商/模型 | 不把渠道卡维度塞入usageRecords |
| `query.rs:103` | LedgerRow有source/provider/model，日档session为空 | 查询层复用原身份，不解析UI label |
| `query.rs:124/138/244/258/341` | load/filters/aggregate已为pub(crate) | V2同crate可直接调用，不需要复制一套SQL |
| `src-tauri/src/database/dao/usage.rs:176` | cache_semantics最后写覆盖 | 不能据此给聚合行套单语义；任务6数学与源证据双门禁 |
| `dao/usage.rs` DailyAggRow/DetailAggRow | 不返回逐请求语义、账户、TTL/上下文条件 | V1保守部分计价；不虚构CostBasis现有接口 |
| `src-tauri/src/services/usage/collectors/mod.rs:174` | detail按DetailKey先行聚合，返回可变DetailDelta | 不以ledger中一行等同一请求 |
| `src-tauri/src/services/usage/provider.rs:51` | resolve_provider返回(String,SourceKind)，旧规则可推断provider | 独立Attribution，旧provider不重写 |
| `src-tauri/src/services/usage/range.rs:57/144` | today/last5h小时，其余日；宿主本地时区 | V2复用resolve_range，无新UTC分桶 |
| `src-tauri/src/database/migration.rs:24` | migrate调用schema::init且探测旧表形状 | 新表IF NOT EXISTS，无臆造迁移版本号 |
| `src/components/usage/UsageTrendCard.tsx` | 无筛选插槽，空态提前return丢卡头 | 新增插槽，常驻卡头，仅替换内容区 |
| `src/components/usage/UsageDistributionCard.tsx:43` | 实际可见仅tool/project/model三按钮 | V2恢复为四个新产品维度，非宣称原来已有渠道 |
| `src/lib/usage/distribution.ts:13` | 通用助手Top10且被分享复用 | 新distributionV2控制Top5，旧模板不被连带修改 |
| `src/lib/usage/useExportShare.ts:51/60` | userEst确实参与分享图metrics | 删除该指标装配，不能只改UsageSummary |
| `src/components/settings/UsageSection.tsx:270` | JSON编辑框已撤下但旧规则保留 | 新可视化渠道管理，保留旧规则兼容 |
| `src-tauri/src/commands/usage.rs` | 命令spawn_blocking，命令层不取DB锁，有源码锁 | 新usage_v2遵循锁序并扩展检查面 |
| `src/lib/query/queries/usage.ts` | 读不轮询，collect才失效usage前缀 | 新查询纳入同前缀；价格单独后台刷新事件 |
| `src-tauri/Cargo.toml:48` | Rust lib名multi_agents_manager_lib | 集成测试用真实crate名，不写tuvis_lib |
| Cargo.toml:85/88 | 已有reqwest、sha2 | 不为价格功能重复引入HTTP/哈希依赖 |

## 规格与计划对照时已修正的问题

- 记录绑定一度计划放在卡片操作区，已改回设计要求的每行入口；同名候选再显式选择，不从拼接字符串拆身份。
- 页面渠道分布类型一度容易流入原CSV，已拆开旧导出groupBy和新distributionGroup，并要求导出入口说明按工具。
- Top5不修改全局DIST_MAX_ROWS，防止固定分享模板被意外变更。
- 快照ID已统一为内容哈希+获取时间，避免同内容刷新后日期陈旧与两种定义冲突。
- `profile-a`测试ID与生产UUID校验规则存在冲突，已明确后台验证不透明非空唯一ID，不强制UUID格式。
- 新费用界面不使用伪十进制类型；所有调用方统一使用estimatedUsd:string|null。

## 未完成的实施探测及明确出口

| 门禁 | 本轮已确认 | 执行时必须补充 | 不通过的出口 |
| --- | --- | --- | --- |
| 七源渠道证据 | 现有账本没有可靠付款账户标识 | 任务1按七源整理提取点和历史范围 | 只允许用户指定，混合不拆 |
| 缓存计费条件 | builder/DAO/daily丢逐请求语义 | 任务6证明每源/粒度安全许可，运行Subset/混合回归 | 受影响桶partial/unpriced，不自动改采集 |
| 小时/日渠道趋势 | 原query双路径均可复用 | 任务4边界/DST/同筛选上期测试 | boundary未知，不伪造小时 |
| 公开目录匹配 | 实际GET成功且模型可能无cost | 任务7合成复杂条件+本地已观测键匹配覆盖（不上传） | 缺价/条件不足未估价，保留Token |
| 旧providerMapRules | 原机制为前缀推断provider | 任务1验证历史provider_kind，任务3旧表守恒 | 不自动迁移/重写，保持inferred |


## 编写计划时已取得的只读证据



- 旧 dashboard TS/Rust只接受range/groupBy，无趋势selection；V2必须新增入口。
- LedgerRow只带source/provider/model/project等旧身份；日档没有session_id。
- DetailDelta在DeltaBuilder按DetailKey聚合；detail DAO的cache_semantics是最后写覆盖；daily不保留该语义。逐请求互斥/TTL/上下文条件不能从该列恢复。
- normalize的Exclusive：request_total=input_raw+cache_read+cache_write；Subset：request_total=input_raw，input_fresh=max(input_raw-cache_read,0)，cache_write仍单列；output已含reasoning。
- 旧resolve_provider优先显式字段/模型前缀，其次旧用户规则；规则命中为inferred。此字段不是付款身份。
- query按last5h/today读取detail，其他档读取daily；现有pub(crate)读取/aggregate/helpers可由V2模块复用。
- migration为幂等DDL+形状探测，不存在为本功能可直接递增的统一user_version序列。
- 公开请求 `GET https://models.dev/api.json` 在本轮成功；顶层provider对象含models字典；实际观察到模型条目没有cost字段。只读取公开目录，不发送本地身份或用量。
- 公开README标注cost为USD/百万Token，可出现input/output/cache_read/cache_write和reasoning等其他报价条件；保守解析不得把缺价填0。


## 执行记录（获准实施后由主 Agent 追加）

本轮仅编写文档，尚未实施任何任务，所有任务保持未勾选。实施时按任务追加以下字段：任务编号、源/粒度事实证据、真实命令与退出码、红测原因、绿测结果、Review规格轮/质量轮结论、修复复审、提交、未解决问题。文档/只读任务用实际检查结果替代不适用的红绿测试。不得预填通过，不另建本功能证据文件。

### 2026-10-17 Task 1 执行记录（只读门禁与测试基线）

**基线**：`git rev-parse HEAD` = `267e880b6b9c5b60e4fe87900f6741691efa8587`，分支 `main`。附件解包带入 macOS AppleDouble `._*` 文件与 5 个脚本的执行位丢失；已仅删除 `._*` 并恢复执行位，`git status --short` 为空后才开始。注：Plan 引用的 `research/七源字段可得性矩阵.md` 在仓库中不存在，以下证据改由七个 `collectors/*.rs`、`semantics.rs`、`provider.rs`、`query.rs`、DAO upsert 与 `schema.rs` 直接核对。

**1. 七源身份与模型证据（只读代码核对）**

| 源 | 模型来源 | 供应商来源 / 三态 | 模型是否被改写 | 付款账户 |
| --- | --- | --- | --- | --- |
| claude | `message.model`，缺失时用会话 `last_model` 兜底 | 无显式字段；模型名内嵌 `/` 前缀→Measured，否则旧规则→Inferred，再否则 Unknown（`provider.rs:resolve_provider`） | 否 | 无 |
| codex | `turn_context.model` 跨轮继承 | 同上（无显式字段） | 否 | 无 |
| kimi | `usage.record.model` / 会话状态继承 | 模型名 `vendor/model` 的 `vendor` 前缀→Measured，且**前缀从模型名剥离** | **是**（`ollama-cloud/glm-5.3-flash` → 模型 `glm-5.3-flash`） | 无 |
| opencode | `session.model` JSON 的 `id/modelID` | 同对象 `providerID`→Measured | 否 | 无 |
| workbuddy | jsonl `providerData.model`（DB 模型会过期，不用） | 无显式供应商字段；模型名内嵌 `/` 前缀→Measured，否则旧规则→Inferred，再否则 Unknown | 否 | 无 |
| zcode | `model_usage.model_id`；turn/tool 行用会话最近模型补位 | 同表 `provider_id`→Measured | 否 | 无 |
| dsh | `modelSelection.val.lastUsed.model`，可为空 | 同对象 `provider` 有值→Measured；无显式值时仍可能模型前缀→Measured、旧规则→Inferred，否则 Unknown | 否 | 无 |

结论：**账户不可自动恢复**。七源均无订阅/API 付款账户字段；既有 `provider` 可能是日志实测、模型名前缀实测或旧 `providerMapRules` 推断，不能当作付款渠道。历史 `provider_kind=inferred` 必须保留并显式标注。

**2. 缓存语义与计价条件丢失证据**

- `DeltaBuilder::detail` 先按 `(source, session, project, hour, model, provider)` 聚合，随后 DAO 对 `cache_semantics` 执行 `last-write-wins`（`dao/usage.rs` upsert）；`usage_daily` 根本不保留该列。
- 因此**混合缓存语义不可由最后写入值恢复**；逐请求缓存 TTL、上下文阶梯、reasoning 计费条件也不在聚合行中。
- `Subset + cacheWrite` 重叠只能靠聚合等式 `Σ(requestTotal)` 与 `Σ(fresh+read+write)` 的关系识别“存在重叠”，不能恢复具体请求；受影响桶须降级为 partial/unpriced（Task 6/9）。

**3. 小时/日双路径**

- `resolve_range`：`last5h`/`today` → 小时明细；`last7d`/`last30d`/`custom` → 日聚合。
- 日档 `LedgerRow.session_id` 恒为空，且没有 `cache_semantics`、没有 turns/error/interrupted/tool 计数与时长样本（`requests`/`request_total` 仍保留）；日档不能按会话或逐请求条件判定，也不能与小时档拼接伪造历史。
- V2 趋势/分布/费用必须分别调用现有 `load_ledger_rows_with`（日档只读 daily，不读 detail）。

**4. 旧规则兼容**

`providerMapRules` 命中只产生 `SourceKind::Inferred` 的 `provider` 字符串，不是付款身份；V2 不迁移、不重写、不恢复 JSON 编辑框，仅在用户显式绑定时以该既有身份作为匹配键之一。

**5. 定价探测（Task 7 继续）**

只读 `GET https://models.dev/api.json`：HTTP 200，5.5 MB；顶层 232 个 provider，8,991 个 model，8,538 个带 `cost`；实际观察到的 cost 键含 `input/output/cache_read/cache_write/context_over_200k/tiers/reasoning/input_audio/output_audio`，`modalities` 存在非纯文本组合。因此 Task 7 必须按条件字段保守降级，缺价不得补 0。

**测试基线（真实命令与退出码）**

| 命令（cwd=/mnt/tos/workspace/tuvis-base） | 退出码 | 结果 |
| --- | --- | --- |
| `pnpm test -- tests/pet/usage-api.test.ts tests/pet/usage-page.test.tsx tests/pet/usage-queries.test.tsx tests/pet/usage-export-text.test.ts tests/pet/usage-export-image.test.ts` | 0 | 145 files / 1507 passed / 5 skipped（Vitest 实际加载了全部匹配用例） |
| `CARGO_TARGET_DIR=/tmp/tuvis-target cargo test --manifest-path src-tauri/Cargo.toml --lib services::usage::provider` | 0 | 3 passed / 0 failed |
| `CARGO_TARGET_DIR=/tmp/tuvis-target cargo test --manifest-path src-tauri/Cargo.toml --lib services::usage::semantics` | 0 | 8 passed / 0 failed |
| `CARGO_TARGET_DIR=/tmp/tuvis-target cargo test --manifest-path src-tauri/Cargo.toml --lib services::usage::collect::tests`（Task 2 修复后复跑） | 0 | 23 passed / 0 failed |

测试隔离：Rust 单测不建库或只使用临时 SQLite；集成测试使用 `support::open_ledger_db(tag)` 的私有 tempdir 库；前端只读静态/MSW 夹具；未启动 `pnpm tauri:dev`，未触发真实采集，未写真实账本。基线首轮 Vitest 在共享挂载盘上因 worker 启动超时失败（`Timeout waiting for worker to respond`）；将 `node_modules` 复制到本地 `/tmp` 并软链后同一命令 4.5 秒通过。该失败为环境 I/O 问题，不是测试失败，未据此修改任何断言。

**宪法核对**：本任务未修改 `docs/MASTER-PLAN.md`；未发现 Spec/Plan/实现证据与宪法冲突（尤其 SSOT、能力差异显式化、只读账本与文档权威条款）。

### Task 2–11 执行记录（内部循环，按依赖顺序）

以下命令 cwd 均为 `/mnt/tos/workspace/tuvis-base`；Rust 一律使用 `CARGO_TARGET_DIR=/tmp/user/tuvis-target`（共享挂载盘上的默认 target 有 I/O/空间问题，已如实登记为环境限制）。前端只跑定向 `pnpm exec vitest run`；未启动 tauri、未真实采集、未写真实账本。

| 任务 | 提交 | 红测（真实失败原因） | 绿测（退出码 / 结果） | 独立 Review |
| --- | --- | --- | --- | --- |
| 2 身份类型与纯解析 | `02bf67af`、`9ab03203`、`1914a6ae` | `services::usage::v2::identity` 5 failed：resolve_bucket/at/merge 未实现（101） | 11 passed；collect::tests 23 passed；fmt/clippy 0 | 规格通过；首轮发现 3 条自省锁回归与 provider 正向/去重测试缺口，修复后复审关闭 |
| 3 配置快照与撤销 | `525ec09d`、`01bf3e19` | `usage_billing_test` 9 failed：billing/DAO/表未实现（101） | 11 passed；migration 11；collect 23；fmt/clippy 0 | 规格/质量通过；Minor（空 pricingRule id、脏历史断言、JSON 损坏）修复后关闭 |
| 4 账本适配/趋势/分布 | `83d67da3`、`d5084b45` | `v2::query` 12 failed：attribute_rows/trend/distribution/identities/snapshot/preview 未实现（101） | query 13；v2 40；旧 query 49+1 ignored；collect 23；billing 11；fmt/clippy 0 | 规格通过；发现小时桶 `+Duration` 溢出 panic，修复为 checked 并补测试后关闭 |
| 5 身份/Token IPC | `c832b3ef` | 前端 2 文件失败（模块可解析缺失）；usage_ipc_test 失败（usage_v2 模块不存在） | v2-api+v2-queries 7 passed；usage_ipc_test 10；v2 42；旧命令锁 11；tsc/fmt/clippy 0 | 规格通过；mock undo 不恢复历史内容列为 Task 13 前必须项（Task 11 已修） |
| 6 安全计价桶 | `e8899c41` | `v2::cost` 8 failed：safe_components/basis_trusted 未实现（101） | cost 10；v2 50；collect 23；fmt/clippy 0 | 通过；补 write>0 非重叠、溢出测试与文档措辞后关闭 |
| 7 价格目录解析 | `81f42442`、`367848f9` | `v2::pricing` 12 failed：parse_rate/parse_catalog 未实现（101） | pricing 22；v2 74；collect 23；fmt/clippy 0 | 首轮发现 serde_json 未启用 arbitrary_precision（f64 提前收敛）与 modalities 未知键漏标，启用 feature + 补测试后关闭 |
| 8 快照/24h 刷新 | `d2de7c08` | 执行者为事后变异验证（临时 unimplemented，9 failed）；**非严格 red-first，如实登记为流程偏差** | pricing 22；v2 74；migration 11；collect 23；fmt/clippy 0；并发单飞 fetch=1 | 规格/质量通过；要求补 activate 回滚/下载边界测试（非阻塞） |
| 9 金额/覆盖率 | `fee04588` | `v2::cost` 9 failed：estimate_components/usd_string 未实现（101） | cost 19；v2 83；collect 23；fmt/clippy 0 | 通过；`0.000529`、`150/160`、`50/130`、全 0 价、缺价、溢出均锁住 |
| 10 费用查询/CSV | `aa62ebd1`、`6674d5e7` | `usage_cost_test` 失败：cost_query 模块未实现（101） | cost_test 17；v2 84；旧 query 49+1 ignored；collect 23；fmt/clippy 0 | 首轮发现缺价 basis 错标、单行溢出拖垮全表；修复后复审关闭 |
| 11 定价/费用 IPC/事件 | `a8bbdadd` | 前端 4 failed（API/hook 未实现）；usage_ipc_test 失败（5 命令缺失） | v2-api+v2-queries 11；usage_ipc_test 10；cost_test 17；v2 84；collect 23；`pnpm check` 0；tsc/fmt/clippy 0 | 复审范围见下；含 mock undo 历史恢复、12 条命令锁纪律、事件失效 |

**自省锁**：每个新增 `services/usage/**` 文件都已在 `collect.rs::USAGE_SOURCES` 注册并同步面尺寸/分类钉子；Task 11 后为 32 文件、26 测试模块区 + 6 空区。期间所有自省测试在干净提交上实测通过。

**已知环境限制**：`/tmp/user` 构建盘多次在并发构建时写满，已清理可重建产物；全量 `cargo test --lib` 的两个 dsh `chmod 0000` 权限用例在 root 容器下必然失败（root 仍可读），远程静态资源用例需 `pnpm build:mobile` 产物。二者均在最终回归中单独标注，不计为功能失败。

### Task 12–19 执行记录（前端/集成/最终收口）

| 任务 | 提交 | 红测/绿测与 Review |
| --- | --- | --- |
| 12 页面折叠/筛选/Top5 | `a2909fd0`、`63ca7d88` | 红：v2-summary/distribution/page 4 failed（新模块缺失）；绿：12 tests；旧 cards/page/worksummary 回归 46 tests。Review 首轮发现 B 假加载态、错误码直印、ambiguous 键缺失；修复后 22 tests + tsc/i18n/lint 0，复审关闭。 |
| 13 记录页绑定/渠道管理 | `174a01e0`、`914352e8`、`ca239d20` | 首轮实现与测试同批（未保留 red-first，登记为流程偏差）；复审变异验证多候选自动选中/冲突后 stale preview/Manager 冲突误报可红。共 3 轮修复：受控输入、冲突清 preview、规则冲突语义、归属标记跨工具/项目作用域。最终 36 tests + tsc/i18n/lint 0，关闭。 |
| 14 分享摘要/双语/文档 | `112324f5`、`07f2b630` | 无 red-first；复审用回退变异验证 7 条测试可红（userEst 删除/cacheMiss）。5 文件 42 tests；README/README.en/manual zh+en/CHANGELOG 更新；未打 tag。关闭。 |
| 15 费用组织/金额格式 | `5a042c7a` | 红：v2-cost-card 模块缺失；绿：7 tests。变异验证跨渠道合并/unknown source/浮点格式均红。关闭。 |
| 16 D 费用卡 | `f6035fbd`、`54075411` | 红：组件缺失；绿：23 tests。复审发现渠道 Top5 按组而非全卡、未订阅 pricing 事件、unknown 渠道不可区分；修复并补全局 Top5 集合/事件订阅/来源工具测试。25 tests + tsc/i18n/lint 0，关闭。 |
| 17 定价设置/授权参考模型 | `1c069239`（后端）、`897631e4`、`719c79ce`（前端） | 后端红：新增 pricing rule 未校验仍 Ok（2 failed）；绿：16 tests。前端红：模块缺失；复审发现占位文案、授权不重置、观测身份缺失；修复后补测试。首屏密度（A 三列/趋势 112/p-4）经 1040 与 840 截图复核 C 标题可见。 |
| 18 费用 CSV | `eb46b2cc` | 红：按钮缺失 4 failed；绿：4 tests；旧导出 5 文件 42 tests。验证只传 range、不调旧 CSV、文件名前缀、保存/定位失败区分、无第二 BOM。关闭。 |
| 19 文案/验收/回归 | `42fd8ae9` | 新增 `v2-acceptance.test.tsx`；全量前端 `tests/usage tests/pet` 469 passed / 5 skipped；`pnpm check` 0。最终独立复审与完整命令见交付报告。 |

**跨任务补充证据**：`f2852f00` 修正 mock pricing stale 不含 enabled，并验证 enable→refresh→disable 后 stale=false；Task 13 前的 mock undo 历史恢复要求由 `a8bbdadd` 关闭。

**TDD 偏差（如实登记）**：Task 13 首轮、Task 14、Task 16 复审修复、Task 17 前端修复、Task 19 acceptance 的实现与测试同批完成，未保留独立 red-first 运行记录；均通过事后回退/变异验证证明测试可红。Task 8 同样为事后 unimplemented 变异。不得将这些条目声称为严格 red-first。

### 复审批次（2026-10-19，评审修复二批）

主会话对 PR #170 组织三路独立复审（Rust 后端 / 前端 / 文档双语）+ 本机全量回归；据此派发二批修复（实现与测试同批，登记为流程偏差）：

| 问题 | 修复 | 验证 |
| --- | --- | --- |
| P1 定价快照表无界增长 | `activate_snapshot_conn` 激活事务内 GC：保留 active + 最近 1 个历史快照，其余删除；离线回退语义不变 | `activation_gc_keeps_active_and_one_history_snapshot`（含回退路径可解析断言） |
| P2 桶级归并双实现无等价钉子 | 只加测试：`merge_bucket` vs `fold_bucket_attributions` 9 场景行为字段等价 | `bucket_merge_implementations_agree_on_behavior_fields` |
| P2 迁移幂等测试对 pricing 两表恒真 | DROP/存在性断言扩为四张表 | 既有迁移测试扩展 |
| P2 CSV 中和测试只覆盖 `=` | 参数化补 `+` `-` `@` `\t` 四前缀 | `csv_cells_neutralize_every_dangerous_formula_prefix` |
| P2 刷新链路测试内核与生产入口重复 | 共享内核 `run_refresh` + `RefreshHost`（fetch/emit 注入），生产行为不变；自省锁面尺寸第 28 轮同步（10120/221775） | 既有并发单飞/IPC 源码契约测试原样通过 |
| P2 绑定 Dialog 默认渠道在 billing 未就绪时卡死「新建」 | 默认值落 `!billing.isPending` 后，区分自动填充与手选 | `4b` 回归用例 |
| P2 Manager last7d 身份查询常驻 | `enabled: dialogOpen` 门控（hook 加可选参数） | `5b` 调用计数断言 |
| P2 改名草稿被后台失效链清空 | 草稿重建仅随 `revision` 变化 | `6b` 回归用例 |
| P2 pricingHintText 正则匹配后端文案的隐式契约 | JSDoc 登记依赖的后端 detail 清单，正则收紧为前缀逐字匹配；不加错误码 | 测试 12 改用后端真实文案 |

二批复审：后端/前端复审代理逐项复核通过（见最终报告）；本机回归全绿。

### 已知限制（收口落档，随 PR 描述同步）

1. **定价目录解析在持 DB 锁期间进行**：`load_active_catalog_with_conn` 缓存未命中时在全局锁内解析 ~5.5MB/9000+ 模型；缓解 = 进程级 `CATALOG_CACHE` 按 snapshot_id 缓存（刷新成功后预热），冷窗口 = 进程启动后首次触达目录的查询。无跨网络 I/O 持锁（下载与解析在锁外，分段短锁）。
2. **后台刷新失败无退避**：5 分钟固定间隔；失败写 `last_error` 使 stale 恒真 → 无限重试。
3. **下载期间关闭定价仍会激活新快照**：`refresh()` 仅在下载前读 enabled；激活不复查。快照激活同事务无半写；并发刷新有单飞锁防护。快照表现在有 GC（保留 active + 1 历史），该限制的副作用已收敛为多一次快照写入。
4. **D 卡折叠态仍会发出 costs/pricing 查询**（hooks 在组件顶层，折叠只影响正文挂载）；billing 管理器的 last7d identities 查询已改为打开时才发。

以上限制经复审逐条核实属实，接受为已知边界，留待后续迭代。
