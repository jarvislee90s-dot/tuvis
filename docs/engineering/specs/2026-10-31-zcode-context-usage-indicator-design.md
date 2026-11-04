# ZCode 手机端上下文占用指示设计

> 日期：2026-10-31  
> 范围：可选 O2「模型左侧上下文占用圆环」  
> 状态：**NO-GO（数据证据门未通过）**。本文界定调查与条件化实现，不代表当前已有可信上下文数据，也不授权直接开发或显示估算百分比。  
> 依据：`docs/superpowers/reviews/2026-10-31-zcode-next-round-review-and-change-proposals.md` §6、`docs/superpowers/plans/2026-10-31-zcode-next-round-engineering-plan-v2.md` §8、当前 ZCode 控制 DTO 与用量采集代码、W0 脱敏证据文档。  
> 实机状态：本环境没有 ZCode 官方宿主或账号；官方实时字段、压缩后变化和容量口径均 **NOT RUN（需实机）**。

## 0. 结论

当前代码没有提供某个 ZCode 会话的「上下文已用 token」与「上下文窗口容量」这对同口径数据。现有 ZCode 用量采集按请求读取 `model_usage` 的输入、输出、缓存、推理等 token 桶，并汇入用量统计；这些数值回答的是一段时间内发生了多少请求用量，不能代表当前上下文窗口还占多少。

因此目前不应在模型选择左侧画圆环、百分比或容量数字。只有找到并验证同一会话、同一模型、同一可信时点的已用量与容量，且在模型切换、上下文压缩、会话切换后仍能正确更新，才进入实现评审。用量账本累计数、模型名容量表和价格数据均不满足此条件。

## 1. 目的与使用场景

手机用户切换 ZCode 模型时，若官方确实提供当前会话的上下文使用量与该模型窗口容量，用户希望在模型控件旁快速了解上下文空间的使用程度。圆环只负责呈现这项读数，不触发压缩、切模或任何宿主命令。

设计前提：这项信息必须有可核对的来源和时点。若数据缺失、过期、模型不匹配或来源不可验证，宁可明确显示未知，也不从历史请求 token 猜一个百分比。

## 2. 当前代码与根因

### 2.1 控制面投影没有上下文统计

- Rust `SelectionView` 目前只有 `provider`、`model`、`thought`、`models`，没有窗口容量或当前上下文使用量（`src-tauri/src/services/zcode_control/types.rs`）。
- `SessionView` 目前投影 `session_id`、`generation`、`revision`、`ready`、pending、control、selection、capabilities、queue 和 `input_routing`，没有 `contextUsage`（同文件）。
- 前端模型工具栏读取 `session.selection`，现有输入只用于当前模型、供应商、思考档位和可选模型目录（`src/mobile/ZcodeControlPanel.tsx`、`src/lib/api/zcodeControl.ts`）。

所以现有远程控制状态并不能支持“已用/总量”圆环。前端单独加 SVG 或给每个模型硬编码容量，只会把缺数据包装成看似精确的读数。

### 2.2 用量账本是逐请求/时间范围累计

ZCode 用量采集器逐请求读取 `model_usage` 行，其中含 `session_id`、`provider_id`、`model_id`、输入/输出/推理/缓存 token、`computed_total_tokens` 和请求状态（`src-tauri/src/services/usage/collectors/zcode.rs`）。这些行按时间归入用量统计；用量服务对各项按时间范围聚合。其含义是请求用量记录，不是宿主当前保留在上下文窗口里的内容快照。

即使某次请求的 token 数恰好接近窗口容量，也不能推出后续时刻的当前占用：系统提示、历史消息、工具返回、缓存读写、压缩策略、截断以及模型切换都会改变该口径。累计量更不能在压缩后正确下降。

### 2.3 当前根因是缺可信数据对，不是缺 UI

圆环需要 `usedContextTokens` 和 `contextWindowTokens`。当前 DTO 与用量采集均没有这对字段。W0 证据文档也没有可引用的官方实时上下文读数或压缩后读数；这不证明官方永远没有该能力，只表示当前证据不足，必须先取证再决定是否开发。

## 3. 目标与非目标

### 目标

- 在数据证据通过后，以简洁的圆环显示当前 ZCode 会话的上下文使用比例。
- 同时提供已用/总量、取样时间和数据来源，不能只靠颜色表达。
- 数据与当前 session、generation、revision、provider、model 对齐；切换、降级或过期时安全地回到未知态。
- 手机窄屏和辅助技术用户能理解未知、正常、接近上限和异常数据状态。

### 非目标

- 不把整个会话生命周期累计 token、用量页的日/周/月总量或单条请求 token 当作当前窗口占用。
- 不通过模型名称、价格目录或静态模型参数表推测容量。
- 不以价格、请求数、剩余轮次、字符数或窗口剩余百分比代替真实上下文读数。
- 不自动触发压缩、切换模型或改变 ZCode 执行设置。
- 不为显示圆环而增加新的采集频率或读取未验证的私有文件/接口。

## 4. 可行路线与取舍

| 路线                    | 做法                                                                                                | 优点                                                            | 限制与决定                                                                                                                       |
| ----------------------- | --------------------------------------------------------------------------------------------------- | --------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------- |
| A. 官方实时会话状态     | 从官方 session snapshot/delta 或已公开只读统计接口取出使用量和窗口容量                              | 若字段随同会话状态更新，容易与 revision、模型切换和压缩事件对齐 | 当前字段形状和语义未知；先做脱敏实机探针。**推荐优先调查，不代表现在可实现**                                                     |
| B. 本地会话日志         | 若官方本地文件明确记录同一会话的实时 context usage 与模型窗口容量，则只读解析并验证其更新和压缩语义 | 避免增加第二条远程请求通道，可能能获得更多细节                  | 当前 `model_usage` 只证实逐请求 token 账本，不是上下文快照；只有另一个明确字段通过语义验证才可考虑。不得从请求记录自行重建上下文 |
| C. 数据不足时不显示功能 | 不加入圆环，保留现有模型工具栏                                                                      | 不制造伪精确数据，不增加维护面                                  | 用户暂时看不到上下文占用；这是当前应采用的路线                                                                                   |

**建议**：先按 A 调查官方实时字段，同时只读核对是否存在明确的本地快照字段；若两处都没有可信的同口径数据，O2 维持 NO-GO。不得为了满足布局提案而先造灰色百分比或模型容量映射表。

## 5. 数据证据门

要从 NO-GO 转为“可以排实现计划”，至少需对当前目标 ZCode build 取得以下证据：

1. 找到原始字段和来源位置，记录它是官方实时字段还是明确的本地 session snapshot；不能只记录 UI 截图上的数字。
2. 证明 `usedContextTokens` 表示当前上下文窗口已用 token，而非本次请求 token、生命周期累计 token 或账单累计。
3. 证明 `contextWindowTokens` 是该会话当前模型的容量，单位与 used 值一致；如果容量未知，不得从模型名字典补齐。
4. 同一 session、provider、model、revision 或可比取样时点能绑定这两个数；混合时间或混合模型的数据不能组合。
5. 在长对话中观察 used 值随上下文增长；触发官方压缩后观察占用能下降或以另一个明确的压缩后口径更新。
6. 切换模型后确认模型身份与窗口容量一起变化；跨会话、重连和断线恢复后不能复用旧会话读数。
7. 对缺字段、未授权、源失联、解析失败和过期状态确认真实结果，不把缺失值当成 0。

建议探针只记录字段路径、类型、数值单位、`sessionId`/模型身份的脱敏摘要、revision/取样时间及状态迁移；不得记录 JWT、Cookie、完整消息正文或任何敏感 prompt。

## 6. 通过证据门后的候选数据合同

下列结构只是**条件化草案**。探针未通过之前，不新增 Rust/TS 字段、不落库、不在 UI 消费。

```ts
interface ContextUsageView {
  known: boolean;
  usedTokens: number | null;
  windowTokens: number | null;
  provider: string | null;
  modelId: string | null;
  source: "official" | "verified_log" | "unknown";
  observedAt: number | null;
  observedRevision: number | null;
}
```

建议作为 `SessionView.contextUsage?: ContextUsageView | null` 的会话级状态，与 `sessionId`、`generation` 和 session revision 一起投影；如果官方只提供 selection snapshot 的同一原子读数，类型最终应由实际数据所有者决定。不得单独缓存一个脱离会话身份的 UI 数值。

### 数据有效条件

只有全部条件同时成立才可绘制比例：

- `known === true`，来源是经证据确认的 `official` 或 `verified_log`；
- `usedTokens`、`windowTokens` 是有限非负数，且 `windowTokens > 0`；
- provider/modelId 与当前 `SelectionView` 完全匹配；
- 取样与当前 session/generation/revision 对齐，并在通过实机验证的时效窗内；
- 数据不是仅凭模型名或用量账本推算出来的。

若 `usedTokens > windowTokens`，这表示观测口径不一致或确有超限，不能静默压成“100% 精确”；显示应进入异常/超限表述，并保留原值供“已用/容量”详情核对。圆环绘制值可限幅，但可访问文案不得掩盖超限事实。

## 7. 条件化 UI 设计

- 只有证据门通过并经产品评审后，才在模型选择左侧放置轻量圆环；它是读数，不是按钮，也不改变现有模型选择行为。
- 有效数据时显示有界比例；点按或聚焦后提供“已用 / 总量、占比、来源、取样时间”。颜色只能作为辅助，不作为唯一信息载体。
- 在圆环确已获准发布但某个会话没有有效值时，显示问号/灰色未知态，提示“暂无上下文数据”；不得显示 `0%`。
- 断线、`ready=false`、会话切换、generation 改变、当前模型变化、revision 不匹配、数据过期或解析错误时清除旧值并显示未知态，不保留旧模型的环形进度。
- 工具栏仍需保留“打开官方页面”、模型和思考档位。优先保证模型名称与操作可用；360px 正常字级维持紧凑，320px + 125% 可有限换行，圆环不能挤掉按钮或制造横向溢出。
- 屏幕阅读器文案至少包含当前模型、已用/总量、比例或未知状态；超限状态应读作“超过已知窗口容量”，不能仅说“100%”。

## 8. 时序、失效与边界

- 初始化：只随 session GET/挂载加载，不建立高频轮询。官方增量如含 usage 字段，则与现有 session revision 单调规则一起合并。
- 模型切换：selection 回执不等于上下文 usage 已与新模型同步；在新鲜状态确认 provider/model 匹配前显示未知。
- 上下文压缩：使用源自身的压缩后快照/事件更新；若源只给累计 usage，压缩后不能回落，则该源不满足当前上下文占用定义。
- 断线与恢复：无新鲜数据时清旧值；恢复后以当前 session 的新鲜快照重建，不能从手机本地值补回。
- 读取失败：不影响主控制区可用性；仅环形指标退到未知，并如实标示来源不可用。
- 并发事件：若 usage 与 selection 分属不同 revision，不能拼接后显示；等待同版本组合或保持未知。

## 9. 验收与回归

当且仅当第 5 节证据门通过，后续实现计划至少应覆盖：

1. 同会话同模型的 used 与容量来源、单位、时点一致；消费量增加时显示上升。
2. 上下文压缩后，真实数据按宿主语义回落或更新；不能用会话累计 token 替代。
3. 模型切换后容量与模型标识同步；旧模型数值不会短暂显示在新模型旁。
4. 会话切换、generation/revision 改变、断线降级与重连、字段缺失和源错误均清除旧值并退到未知。
5. 0 用量、0/负容量、负数、NaN/Infinity、used 超容量、时间戳过期和 provider/model 不匹配均有确定行为。
6. 320px/360px、100%/125% 字级与浅/深色下工具栏可用；不遮挡官方页面入口或模型控件。
7. 读屏能读出数值、单位、模型与来源；未知和超限态不只依赖颜色。
8. 测试数据明确标注合成宿主。合成测试只能证明本地投影/UI逻辑，不得替代真实宿主字段语义和压缩行为验证。

## 10. NO-GO 条件与开放问题

任一情形成立则继续维持 NO-GO，不做环形占用百分比：

- 只找到逐请求 token 或时间范围累计值；
- 找到 used 值但找不到可信窗口容量，或两者单位/统计范围不一致；
- 数值无法绑定到 session、provider、model 和同一取样时点；
- 压缩后不能证明数据下降/更新，或模型切换后无法确认容量来源；
- 只能通过静态模型名单、估价目录或人工经验推断容量；
- 当前 host build 或账号资格下的字段来源未核验。

开放问题留给 W0/后续实机探针：官方状态里是否存在上下文占用字段？本地 ZCode 数据库是否有当前会话快照（而非 request ledger）？窗口容量由当前模型、账号套餐、路由、配置还是其他因素决定？压缩操作是否会立即生成可观察的新值？字段更新频率和过期边界是什么？

## 11. 参考与代码事实

- 上游修改意见：`docs/superpowers/reviews/2026-10-31-zcode-next-round-review-and-change-proposals.md` §6。
- 工程计划：`docs/superpowers/plans/2026-10-31-zcode-next-round-engineering-plan-v2.md` §8。
- 当前选择/会话投影：`src-tauri/src/services/zcode_control/types.rs` 中 `SelectionView` 与 `SessionView`；两者均无 `contextUsage`。
- 当前 ZCode 逐请求用量采集：`src-tauri/src/services/usage/collectors/zcode.rs` 中 `ModelUsageRow`、`load_model_usage` 和 `collect`；`UsageBuckets` 定义在 `src-tauri/src/services/usage/model.rs`。
- W0 证据边界：`docs/superpowers/reviews/2026-10-31-zcode-queue-debug-evidence.md` §0、§2、§5；上下文占用实机数据 **NOT RUN**。
- 静态 wire 形状来源：`docs/superpowers/reviews/2026-10-29-zcode-r0-wire-evidence.md`。其中针对 ZCode `3.16.2.8769` 的静态分析不是当前 build 的实机抓包，也不能证明上下文字段存在或不存在。
