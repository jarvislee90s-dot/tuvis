# 用量功能 · 对外接口契约（冻结）

- 日期：2026-10-03
- 状态：**冻结**——两个执行计划（① 采集与存储底座 ② 三个呈现面与导出）共用本契约；任一侧不得单方面改名或改形状，需要变更必须先改本文件
- 上游依据：`docs/engineering/specs/2026-10-03-pet-token-usage-dashboard-design.md`（需求说明书，D1–D21）
- 数据可得性依据：`docs/research/七源字段可得性矩阵.md`

---

## 1. 命名与序列化约定（沿用 MAM 既有惯例）

- Rust 侧结构体一律 `#[derive(Serialize, Deserialize)] #[serde(rename_all = "camelCase")]`（对齐 `session/model.rs`、`services/pet/manifest.rs`）。
- **可空字段的两种写法必须区分（2026-10-03 补记，落实既有约定；实现见 `model.rs:1-5`）**：本契约写 `field: T | null` 的字段，Rust 用 `Option<T>` 且**必须序列化出 `null`**（**不得**加 `skip_serializing_if`）；只有写 `field?: T`（可选、缺省即不出现）的字段才用 `Option<T>` + `skip_serializing_if`。`UsageMetrics.userEst` / `UsageDashboard.compare` / `RecentSessionUsage.title` / `WorkSummary` 各字段 / `UsageRow.isSubagent`（本次新增）**都属前者**。
- Tauri 命令名用 `snake_case`（对齐既有 `pet_*`、`get_all_sessions`），前端 `invoke` 用同名。
- 时间戳一律 **i64 毫秒（epoch ms）**；日期键一律 **`YYYY-MM-DD` 本地日在字符串**（对齐参考实现 `dateKeyOf`）。
- 错误：采集类错误用 **结构化错误码**（对齐 `services/pet/error.rs` 的 `PetRpcError` 模式），码表需三处登记（Rust 常量表 / 前端码表 / `zh.json` + `en.json`）。
- 所有新增 IPC 命令必须在**三处登记**：`commands/mod.rs`、`lib.rs` 的 `generate_handler!`、前端双侧 mock（`src/tauri-mock.ts` + `tests/msw/tauriMocks.ts`）。

---

## 2. 共享值对象（Rust 定义；前端 `src/types/usage.ts` 镜像同名接口——该文件的创建者与最终形状以计划① 为准，纪律见 §5）

```
UsageRange {
  preset: "last5h" | "today" | "last7d" | "last30d" | "custom"
  from?: string            // 仅 custom：YYYY-MM-DD
  to?: string              // 仅 custom：YYYY-MM-DD
}

UsageGroupBy = "tool" | "project" | "provider" | "model"
// usage_dashboard 接受四值；usage_records 只接受 "tool" | "project"
// （groupBy 只驱动卡片分组，卡内行固定为「供应商 / 模型」，见说明书 D6/D7）

UsageBuckets {             // 四桶，单位 token
  inputFresh: number       // 未缓存输入
  cacheRead: number
  cacheWrite: number
  output: number
}

UsageMetrics {             // 派生口径（说明书 §4）
  requestTotal: number     // 请求输入
  cacheHitRate: number     // 0–1 分数（不是百分数）
  userEst: number | null   // 用户输入(估)；不可得源为 null（绝不填 0）
  requests: number         // 请求次数
}

UsageRow {
  key: string              // 分组键（工具 id / 项目小写键 projectName.toLowerCase() / 供应商 id / 模型名）
  label: string            // 展示名（项目名原文 = project_name_from_path 的 basename；模型名原文）
                           // ⚠️ **2026-10-06 用户裁决（接口变更申报，随实现一并落地）**：
                           // `groupBy="model"` 的 `key` / `label` 由「模型名」改为 **「供应商 / 模型」**
                           // （供应商不可得时只回模型名）——与记录页卡内行（D6）**同一形状、同一实现**
                           // （`query.rs::route_label`，两处共用，不得各写一份）。**枚举值本身不变**
                           // （仍是 tool | project | provider | model），改的是 `model` 维度的行名语义；
                           // 键含供应商 ⇒ 同一模型挂在两个供应商下出**两行**，不再互相吃掉。
                           // `groupBy="provider"` 仍是合法取值（CSV 导出与内部查询照用），但**大看板
                           // 不再提供该维度的切换按钮**：供应商与模型已合并为一个 sheet（见说明书 §P1 第 5 条）。
  buckets: UsageBuckets
  metrics: UsageMetrics
  sourceKind: "measured" | "inferred" | "unknown"   // 供应商归因三态（说明书 §4.3）
  isSubagent: boolean | null   // D17：计数类需分层。true/false = 真值；null = **本档位算不出**
                               // （日档与记录页卡内行——行维度不含 session_id，见下方说明）
}

UsageAvailability {        // 「不可得就空态」的实现载体
  metric: "sessions" | "turn" | "errorModel" | "errorTurn" | "errorTool" | "interrupted" | "longestTurn"
        | "toolCalls" | "toolAvgMs" | "topTool" | "topToolMs" | "userEst"
        // 未列出的 WorkSummary 字段不可得时直接给 null，不强制配 availability 条目
  available: boolean
  reason?: string          // 不可得原因（用于 UI tooltip，i18n key 或原文）
  perSource?: Record<string, boolean>   // 逐源可得性（D16/D19：只在按工具维度显示时用）
}

TrendPoint {
  key: string              // 5h 档 = "YYYY-MM-DDTHH"；日档 = "YYYY-MM-DD"
  label: string            // 展示标签（"HH:00" 或 "M/D"）
  buckets: UsageBuckets
  metrics: UsageMetrics
}

CompareBlock {             // D14 环比；上一周期无数据时整体为 null
  prevBuckets: UsageBuckets   // 上一周期完整四桶（前端据此自行算各指标差值）
  prevMetrics: UsageMetrics   // 上一周期完整派生口径（含 prevMetrics.requests）
}
// 说明：契约只给「上一周期的完整聚合」，各项差值（含命中率百分点差）由前端计算。
// 这样 D14「各指标均带环比」在**五档时间范围**下都能成立，且**只需一次查询**，
// 无需前端对上一周期再发一次请求。

UsageDashboard {
  range: UsageRange
  groupBy: UsageGroupBy
  rows: UsageRow[]
  totals: UsageMetrics
  totalsBuckets: UsageBuckets
  hero: number             // = totals.requestTotal + buckets.output（说明书 P1 第 2 条）
  trend: TrendPoint[]
  compare: CompareBlock | null
  workSummary: WorkSummary // 工作小结区（D15–D19）
  recentSession: RecentSessionUsage | null   // 浮窗「本会话」行（P2 第 4 行）
  availability: UsageAvailability[]
  collectedAt: number      // 数据截止时间（i64 ms）
}

RecentSessionUsage {       // 「本会话」= 最近有活动的会话（不是当前打开的会话）
  sourceId: string
  sessionId: string
  title: string | null     // 会话维度表 title 可空 → UI 需兜底展示（占位文案）
  buckets: UsageBuckets    // 该会话在**当前 range 内**的用量
  metrics: UsageMetrics
}

WorkSummary {
  sessions: number | null          // D17：父子分层后的会话数
  turnsPerTool: Record<string, number | null>   // D16：只在按工具维度，不合计
  errorModel: number | null        // D18 三层分列
  errorTurn: number | null
  errorTool: number | null
  interrupted: number | null       // D18：用户主动打断单列
  toolCalls: number | null
  toolAvgMs: number | null
  topTool: { name: string; count: number } | null
  topToolMs: { name: string; ms: number } | null
  longestTurnPerTool: Record<string, { p50: number; max: number } | null>  // D19：逐工具 p50+最长
  // 任一字段为 null 表示「不可得」→ UI 显示空态，不得显示 0
}

UsageCard {                // 记录页：每工具一个卡片（D6）
  toolId: string
  toolLabel: string
  buckets: UsageBuckets
  metrics: UsageMetrics
  rows: UsageRow[]         // 卡内每行 = 「供应商 / 模型」（固定行维度，不由 groupBy 驱动）
}

UsageRecords {
  range: UsageRange
  groupBy: UsageGroupBy    // 只接受 "tool" | "project"：仅驱动卡片分组（D7）
  cards: UsageCard[]
  availability: UsageAvailability[]
  collectedAt: number
}

UsageFilters {
  toolIds?: string[]
  projects?: string[]      // 以下三个（projects/providers/models）仅供 CSV 导出与内部查询；
  providers?: string[]     // P3 记录页不暴露这三个筛选（说明书 §6 P3【默认裁定】）
  models?: string[]
  subagentMode?: "include" | "parentsOnly"   // 默认 "include"（token 含子代理，D17）；
                                             // 切 "parentsOnly" 时页脚口径文案改为「不含子代理」
                                             // ⚠️ **算不出就必须报错，不得静默忽略**（见 §3 要点 4）：
                                             // 日档（last7d / last30d / custom）的行来自日聚合表、
                                             // **不带 session_id** ⇒ 无从判断会话身份 ⇒ 该档位下
                                             // subagentMode="parentsOnly" 必须返回结构化错误
                                             // `usage-filter-unavailable`，**不得**当作"没有子代理"放行
}

**筛选语义的通用条款（2026-10-03 用户裁决，适用于 `UsageFilters` 的每一个成员）**：
任何筛选条件在**当前档位算不出**时，命令一律返回结构化错误 **`usage-filter-unavailable`**（错误码第 **9** 个），
**不得**把该条件当成"无命中"或"全放行"。**首个实例**：日档 + `subagentMode="parentsOnly"`（日聚合行不带
`session_id`）。**为什么**：静默忽略会产出「数字含子代理、而页脚文案说不含」的**界面谎言**，
违反本项目「不可得不得假装」（GC 7 / 说明书 §8.2）的第一纪律——这条纪律此前只约束**展示位**，
本条款把它扩展到**筛选位**。**前端契约**：应把该码映射为友好文案，并**在日档主动禁用或提示该开关**
（预防优于报错）；② 侧需据此补 `KNOWN_USAGE_CODES`、`usage.rpc.usage-filter-unavailable`（zh+en）
与日档下的开关禁用/提示。

UsageSettings {
  enabled: boolean                 // 用量统计总开关
  miniBarRange: "today" | "last5h" | "last7d"
  miniBarToolRows: number          // 浮窗第 3 行最多列几个工具（「分工具条数」，不是行数）；默认 3
  detailRetentionDays: number      // 默认 90
  collectIntervalMin: number       // 默认 10
  providerMapRules: string         // 用户自维护「模型名/前缀 → 供应商」规则（JSON 文本）
  exportQuote: string              // 分享图评语，空 = 走预制池
  exportPose: string               // "random" | 动作变体键
}

UsageSettingsPatch = Partial<UsageSettings>

UsageSourceStatus {
  sourceId: "claude" | "codex" | "kimi" | "opencode" | "workbuddy" | "zcode" | "dsh"
  ok: boolean
  parsedFiles: number
  newRecords: number
  errorCode?: string
}

UsageCollectResult {
  collectedAt: number
  durationMs: number
  sources: UsageSourceStatus[]
  totalNewRecords: number
}
```

**`UsageRow.isSubagent` 可空口径（2026-10-03 用户裁决；与 R1 的 `userEst` 同一逻辑）**：

- **取值语义**：`true` = 该组**只由子代理会话贡献**；`false` = 该组含父会话（**真值**）；`null` = **本档位算不出**。
- **为什么会有 `null`**：日档（`last7d` / `last30d` / `custom`）的行来自**日聚合表**（粒度「日 × 源 × 项目 × 供应商 × 模型」，**不带 `session_id`**）；记录页**卡内行**的行维度是「供应商 / 模型」（同样不带 `session_id`）→ 两者都**无从判断该组是否只由子代理贡献**。
- **纪律：「不知道」不得写成 `false`**——那等于谎称「不含子代理」。这与 R1 的 `userEst`（拿不到就写 `NULL`，不填 0）是同一逻辑，也是 §3 要点 2「不可得一律 `null`，绝不填 0」的同一口径。
- **小时档给真值**（`last5h` / `today`：明细行带 `session_id`，可按会话维度表判定）；**日档与记录页卡内行一律 `null`**。
- **调用方（前端）遇 `null` 时的 UI 契约 = 不显示任何「子代理」相关标记**；**不得**回退成 `false` 的「非子代理」语义（该标记整段不出现，既不是「非子代理」文案、也不是 `—` 式空态数值——该位不参与数值展示）。
- **序列化**：Rust 侧用 `Option<bool>` 且**不加 `skip_serializing_if`** → 该键在 wire 上**始终存在**，`null` 必须真的出现（§1 的 `T | null` 一类）。
- **不另配 `availability` 条目**：该位是行内自描述位，`null` 自身即「本档位不可得」，不需要逐指标说明。

---

## 3. Tauri 命令面（前端只调这些，共 8 条：6 条用量查询/设置 + 2 条导出落盘）

> **2026-10-07 接口变更申报（用户裁决 A2）**：两条导出落盘命令的**落盘目录**由 `~/.mam/exports/`
> 改为**系统「下载」文件夹**（macOS `~/Downloads`、Windows `%USERPROFILE%\Downloads`，以
> `dirs::download_dir()` 为准——Windows 上可能是被 OneDrive 重定向过的路径）。
> **命令签名、文件名校验、体积上限、UTF-8 BOM 规则全部不变**；变的只有目录。
> **配套**：`commands/resource.rs::reveal_allowed_roots()` 同步把导出目录加进 `reveal_dir`
> 的白名单根（否则导出成功却点不开「打开所在目录」），该不变量由
> `commands/export.rs::reveal_whitelist_contains_exports_dir` 钉住。
> debug 构建下 `MAM_HOME` 仍会重定向导出目录（`$MAM_HOME/Downloads`）以保住测试隔离。

| 命令 | 入参 | 返回 | 说明 |
|---|---|---|---|
| `usage_collect` | `force: bool` | `UsageCollectResult` | 触发一次采集。`force=false` 且距上次采集小于 `collectIntervalMin` 时直接返回上次结果（**不扫描**）。单飞：并发调用复用同一次结果 |
| `usage_dashboard` | `range: UsageRange`, `groupBy: UsageGroupBy` | `UsageDashboard` | 大看板数据（含 hero / 趋势 / 分组行 / 工作小结 / 可得性 / 环比） |
| `usage_records` | `range`, `groupBy`, `filters: UsageFilters` | `UsageRecords` | 记录页数据（每工具卡片） |
| `usage_export_csv` | `range`, `groupBy`, `filters` | `string` | 返回 CSV 文本；**落盘走 `export_save_text`**（不得用 Blob / `<a download>`，见下） |
| `usage_get_settings` | — | `UsageSettings` | |
| `usage_set_settings` | `patch: UsageSettingsPatch` | `UsageSettings` | 返回合并后的完整设置 |
| `export_save_text` | `name: string`, `content: string` | `string`（落盘绝对路径） | 文本落盘（CSV 用）。写 `系统下载目录`（2026-10-07 裁决 A2，原 `~/.mam/exports/`），`.csv` 自动前置 UTF-8 BOM |
| `export_save_bytes` | `name: string`, `base64: string` | `string`（落盘绝对路径） | 二进制落盘（分享图 PNG 用）。base64 输入，原因见下 |

**关于两条导出命令（2026-10-03 实测后新增，属契约变更）**：

- **为什么不用 `<a download>`**：实测 MAM 的 webview（wry 0.55.1 / tauri 2.11.5）在**未注册 download handler** 时对 download 类导航**直接 Cancel 且失败静默**；若 WebKit 未把该导航标成 download，则窗口会被**导航到 blob URL（界面跑飞）**。两支都不可用，故必须走 Rust 落盘。
- **为什么不走 `dialog.save` + `plugin-fs`**：`plugin-fs` 未安装（Cargo.lock 里的 `tauri-plugin-fs` 只是 dialog 的传递依赖、未 init）；`dialog:allow-save` 未声明；**且新窗口 `usage-dashboard` 不在任何 capability 的 windows 白名单里**，插件命令会被 ACL 拒。而**应用自定义命令不受 ACL 影响**（本仓无 `src-tauri/permissions/`）→ Rust 落盘是**零新插件、零新权限、零 capability 改动**的路径。
- **字节传输形式**：PNG 走 base64 字符串（JSON 数组会放大数倍体积；分享图典型 0.2–2 MB，base64 约 +33% 可接受）。CSV 走纯字符串。
- **落盘后定位**：复用既有 `commands/resource.rs` 的 `ensure_reveal_allowed` + `reveal_dir`（白名单含 `~/.mam`），**不要新写打开目录逻辑**。
- **剪贴板只作副本**：文本 `writeText` 已有 3 处先例可用；**`ClipboardItem`（复制 PNG）全仓零先例且 secure-context 前提未验证** → 复制图片不得作为导出的唯一出口。

**契约要点**：
1. **前端绝不触发全量扫描**：只有 `usage_collect` 会扫描，且它有单飞 + 最小间隔保护；三个查询命令只读本地账本。
2. **不可得一律 `null` + `availability` 说明**，**绝不填 0**（D15/D16/D19）。`UsageRow.isSubagent` 沿用同一口径（不可得 = `null`，见 §2）：日档与记录页卡内行给 `null`，前端遇 `null` **不显示子代理标记**（不得回退成 `false` 的「非子代理」语义）；该位不另配 `availability` 条目（行内自描述）。
3. `cacheHitRate` 是 **0–1 分数**，UI 负责百分号格式化（参考实现这里有 0-1 与 0-100 两套口径混用的历史坑）。
4. **筛选算不出 = 报错，不得静默忽略**（2026-10-03 用户裁决，见 §2 `UsageFilters` 的通用条款）：首个实例是**日档 + `subagentMode="parentsOnly"`**——日聚合行不带 `session_id`，无从判断会话身份，因此 `usage_records` / `usage_export_csv` 在该组合下必须返回 **`usage-filter-unavailable`**（错误码第 **9** 个，三处登记：Rust `USAGE_CODES` / 前端 `KNOWN_USAGE_CODES` / `zh.json`+`en.json` 的 `usage.rpc.*`）。
   - **同族纪律**：本条款是通用的——**今后任何一个新筛选维度**（provider / model / project…）在某个档位算不出时，都必须沿用这条，而不是"尽力而为"地放行。② 侧新增筛选时先问一句"这个档位算得出吗"。
   - **不做静默降级的理由**：放行会让合计含子代理、而页脚文案写「不含子代理」——**界面撒谎**比明确报错坏得多，且违反「不可得不得假装」。
5. **`usage_dashboard` 不接受 `filters`**（见 §3 命令表）⇒ 本条只作用于 `usage_records` 与 `usage_export_csv` 两条命令。

---

## 4. 存储形态（计划①负责；计划②只消费上面的值对象）

四张表（均在 `~/.mam/mam.db`，走既有 `schema::init` + `migration::migrate`）：

| 表 | 主键 | 关键列 | 保留 |
|---|---|---|---|
| 用量明细 | (sourceId, sessionId, **projectKey**, **hourKey**（`YYYY-MM-DDTHH`）, model, provider) | 四桶、requests、**userEst（可空）**、providerKind、cacheSemantics、**dayKey（派生列，供日聚合用）**、firstSeenAt、updatedAt | 可配（默认 90 天） |
| 日聚合 | (dayKey, sourceId, projectKey, provider, model) | 四桶、requests、**userEst（可空）**、providerKind | 永久 |
| 采集游标 | (sourceId, sessionId) | fingerprint、byteOffset、ordinal、lastCumulative、updatedAt | 永久 |
| 会话维度 | (sourceId, sessionId) | projectKey、projectLabel、title、isSubagent、parentSessionId、firstSeenAt、lastSeenAt | 永久 |

**`userEst` 在两张表里都可空**（不是 `NOT NULL DEFAULT 0`）：拿不到用户文本的源写 `NULL`，UI 显示空态；**日聚合也必须有这一列**——否则 claude/codex/kimi 在 7d/30d 视图会恒显示「—」。累加语义为「全 NULL 才是 NULL」。

**`projectKey` 是「记录级」的**（因此进明细表主键）：同一个会话文件内不同 cwd 的记录**各自成键**（D21 规则④；本机 claude 实证一个 jsonl 含两个 cwd）。**会话维度表的项目列仅为「会话级近似」**（取该会话首个带 cwd 的记录），仅供会话列表展示，**不得作为聚合依据**。

**为什么明细表按小时桶存**：【近 5 小时】这一档要求小时粒度，按日存**产不出**小时桶。故明细唯一键为 `hourKey = YYYY-MM-DDTHH`（本地时区整点），另存派生列 `dayKey` 供日聚合与保留期清理使用。日桶由 `hourKey` 前缀聚合得到，不重复落一份。

> **2026-10-07 接口变更申报（dsh 时间归属改造；用户已拍板的口径，见 `docs/engineering/plans/2026-10-07-dsh-time-attribution-fix-brief.md`）**：
> 三处**存储口径**变更，均不改表结构、不改命令面、不改前端值对象形状：
>
> 1. **dsh 的四桶改为按「事件发生时间」入账**（原：按采集时刻）。归属键仍走 `hour_key_of_host`
>    （宿主本地），命令面/字段一字未改；变的是同一笔 token 落在哪个 `hourKey` 上——
>    短窗口（近 5 小时 / 今天）因此从「采集时刻的平移值」变成事件真值。
> 2. **`cacheSemantics` 取值集合新增 `backfill`**（列不变、仍是 TEXT；Rust 侧
>    `CacheSemantics::Backfill`，`from_db` 认它，`normalize()` 对它返回 `None`——它是**来源标记**
>    而不是缓存语义）。语义：「这一段量不是实测到的，而是日志已被清理、只能从投影缓存累计差额
>    补录的部分」，**只进全天账**。
> 3. **补录行的 `hourKey` 用「日键」**（`YYYY-MM-DD`，10 字符）**而不是小时键**：这是
>    「只进全天账、不进任何短窗口」的落地机制，不是笔误。它与小时键不冲突（`usage_detail`
>    主键含 `hourKey`，两者是不同的行），且小时档取数（`query.rs::load_rows_for_keys`）
>    在区间过滤之后还有一道**精确成员**判定 `keys.contains(&hour_key)` ⇒ 10 字符的日键
>    **永远不在**任何小时窗口的键集合里；日档由 `ledger::daily_rows_of` 从明细行折叠 ⇒
>    **必然**收下这笔（「日档与小时档不得分叉」由此自动成立）。日档窗口（`近 7 天`/`近 30 天`
>    /`自定义`）按 `day_key` 取数，照常包含它。
>
> **另见（2026-10-07 事故）**：本机 dsh 的历史行是**重建**出来的（dev 热重载用半成品采集器跑了
> 真实采集；处置见用户 18:38 裁决）——事故、根因、回退点路径、重建后验收与**已知差异**
> （counter 类事实仍按 `last_event_ms` 单点归桶、补录真机零实例、界面未表达 `backfill`）
> 全部落成入库文档：`docs/release-notes/2026-10-07-dsh-ledger-rebuild-incident.md`。
>
> **配套不变量**（都有锁）：`hour_row_*` / `backfill_rows_are_day_only_and_never_land_in_an_hour_window`
> （用真实 `resolve_range` 的键集合断言）、`hour_rows_plus_backfill_equal_the_projection_cache_delta`
> （总量护栏）、`log_leading_the_projection_cache_is_never_counted_twice`（日志领先缓存一轮时
> 不得重复入账）。**历史数据不回填**：改造只对生效之后的采集生效。

**供应商三态列名映射**：同一概念有三个命名族——值对象字段 `UsageRow.sourceKind`（出参）/ 存储列 `provider_kind`（明细表与日聚合表）/ Rust 枚举 `SourceKind`。实现时按此映射，不得各自另起一名（否则 UI 三态标记会全退化 `unknown`）。

**项目键规则**（说明书 §5.1 四条规则 + D21）：`projectKey = projectName.toLowerCase()`，其中 `projectName` **复用既有 `monitor/project.rs:10 project_name_from_path`**（basename，**不做** realpath / 大小写规范化 / trim / 软链解析）；`projectLabel = projectName` 原文；`realpath` **只留档**（会话维度列 `project_realpath`），**不进展示与分组键**；**禁止**用日志目录名反推、**禁止**用 `project_id` 反解；**一个会话文件可对应多个 projectKey**（记录级 cwd 归属）。

---

## 5. 计划切分与依赖

| 计划 | 内容 | 依赖 |
|---|---|---|
| **① 采集与存储底座** | 4 张表 + DAO + 归一化口径层（语义三态 + 黄金用例）+ 7 个采集器 + 调度（单飞 / 最小间隔 / 一次扫描出全部指标）+ 8 条 IPC（6 用量 + 2 导出）+ 双端 mock | 无（本契约即其产物） |
| **② 三个呈现面与导出** | 大看板窗口（含记录页签 + 工作小结区）+ 浮窗（含详情钻取 + 右键唤回）+ 记录页 + 三种导出 + 设置项 | 计划①的 8 条命令（6 用量 + 2 导出）与值对象（本契约已冻结）；可实现于 mock 数据之上，不必等①落地 |

**执行顺序与门禁（2026-10-03 用户批准）**：

```
执行 ①
  → ① 自动化验收（cargo test 全绿 + 账本对账）
  → ① 的最小可见验收面：用户在设置页「用量采集状态」分组亲手验数字
  → 【门禁】② 对齐复核：拿 ① 的真实实现逐条核对计划②，出 delta 清单并改 ②
  → 执行 ②
  → 端到端人工验收
```

**为什么要有这道门禁**：计划② 是对着**纸面契约**写的，而本契约在规划期已改过 3 次（明细表按日→按小时桶、环比→完整聚合、命令 6→8 条）。实现 ① 时若再发现契约需微调，② 里写死的东西会一直带到验收阶段才暴露。**执行期已有一次实例**：`UsageRow.isSubagent` 由非空 `boolean` 改为 `boolean | null`（2026-10-03 用户裁决，见 §2），① 的查询层与 ② 的夹具/断言随之同步——这正是复核要抓的那一类。复核是**短任务不是重写**：契约是冻结的，① 若偏离契约那是 ① 的 bug；只有契约本身必须改时才牵动 ② —— 而这正是复核要抓的。**复核清单**至少须覆盖：命令与值对象形状、`types.ts` 的字段可空性（`title: string | null` / `userEst: number | null` / `isSubagent: boolean | null`）、双端 mock case 无重复（见下「前端类型与双端 mock 的所有权」）。

**契约变更纪律**：任何一方发现契约需改，**先回写本文件并说明理由**，再改实现/计划；不得单方面改名或改形状。

**前端类型与双端 mock 的所有权（2026-10-03 裁定）**：

- `src/types/usage.ts` 与双端 mock（`src/tauri-mock.ts` / `tests/msw/tauriMocks.ts`）的**创建者与最终形状以计划① 为准**（① 负责建立，并对齐本契约）。
- 计划② **只允许 import 类型、只补 mock case**：不得另建同名文件、不得注册第二个同名 `case`（`switch` 里重复 case 会**静默遮蔽**前者，属最难查的一类假绿）。
- 本条与「② 对齐复核」门禁联动：复核时逐条核对 ① 落地后的字段可空性与 mock case 唯一性。

**设置页的分工（避免两个计划撞同一处）**：

| | section id | 组件 | 图标 | 性质 |
|---|---|---|---|---|
| 计划① | `"usageStatus"` | `src/components/settings/UsageStatusSection.tsx` | `Activity` / `Gauge` | 最小可见验收面（采集状态 + 手动采集 + 今日四桶），② 完成后可保留为调试入口 |
| 计划② | `"usage"` | `src/components/settings/UsageSection.tsx` | `BarChart3` | 8 项正式设置 |

两者都要改 `src/pages/settings.tsx` 的同一批位置（联合类型 / menuItems / 渲染分支 / i18n），**② 的改动必须按内容锚定而非按行号锚定**（因为 ① 可能已先改过该文件）。
