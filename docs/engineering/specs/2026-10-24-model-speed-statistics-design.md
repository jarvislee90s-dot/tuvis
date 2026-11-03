# 模型速度统计正式设计

> **历史稿，已替代**：当前依据为[TPS短时抽样收口设计](2026-10-24-tps-sampling-design.md)。下面旧区间/禁用表/保留期不再作为实现要求。

> **执行暂停（2026-10-24 用户重新评估）**：本稿六个工具TPS不可用，不能满足跨工具TPS这一核心目标。用户正在重新考虑插件/依赖与运行时采集入口；本文和配套计划不得继续作为编码依据，待新技术路线确认后重定稿。已有四项统计口径保留。

> 日期：2026-10-24（北京时间）
> 状态：用户已确认产品口径；本文冻结首版行为与工程契约。配套[实施计划](../plans/2026-10-24-model-speed-statistics-plan.md)另列；本文不代表功能已实现。
> 分支：`codex/model-speed-design`；基线为本地 main `b90f6c8e`。远端曾查询到 `35698a33`，同步失败；实施开始前同步主分支并检查接口变化，不以此改变本文产品口径。
> 上位依据：`docs/MASTER-PLAN.md`。本文取代前序口径稿及本文件此前研究草案；历史探测报告保留快照意义，冲突时以本文和源码核查增补为准。

## 1. 已确认的产品决策

| 决策       | 固定行为                                                                                                    |
| ---------- | ----------------------------------------------------------------------------------------------------------- |
| 核心目标   | 客户端首个有效输出后，到该次输出完成的总生成 TPS，包含思考、正文、工具参数；不称服务端纯解码速度            |
| 四项主指标 | 模型用时、工具调用用时、平均首 token、流式 TPS；不增加评分或实际经过时间                                    |
| 模型用时   | 累计模型调用处理时间，包括有可靠计时的成功、失败、取消；不包含后续工具执行                                  |
| 工具用时   | 持久调用开始事件到结果事件的窗口，包含窗口内等待；窗口之前的审批无证据不补造                                |
| 失败与取消 | 计入可靠模型用时；不进入两项速度统计；详情拆分耗时与缺失数量                                                |
| 重试       | 能单独对齐的成功尝试按尝试计；无法分离的重试组不进入速度统计，耗时只记一次                                  |
| 首版范围   | 接入七个现有费用采集源；每项独立可得，缺证据显示 `—`；不增加遥测接收器、Hook、插件、模型代理或 API 路由修改 |
| 时间边界   | 样本完整位于所选区间才计入；跨区间整条剔除，不截断、不分摊；不改 Token/费用账本                             |
| 未知模型   | 对应工具下单列“未知模型”；无费用依据显示 `—`，不猜归属                                                      |
| 展示       | 四项常驻模型费用卡，独立于计费详情折叠；多个工具逐工具列出，不生成跨工具合并 TPS                            |

并发请求逐项累计，父工具等待子代理时也保留调用窗口；累计模型与工具时间存在包含/重叠，禁止相加后标为回合实际时长。主界面不做回合阶段占比或“剩余时间即思考时间”。

## 2. 数值与样本契约

### 2.1 定义

所有时间以整数毫秒保存；S 为模型调用开始，F 为首次非空思考/正文/工具参数输出，E 为该调用终结。有效请求必须具有稳定身份；消息开始、内容块开始、连接完成不等于 F。

- 模型用时：有效终态调用的处理毫秒之和。优先使用逐源规定的开始/结束差，不用回合总时长减工具耗时。
- 工具调用用时：同一 callId 的结果时间减调用时间；原生执行 duration 只作证据，不能与窗口混合求和。
- 平均首 token：成功、计时对齐样本的 `Σ(F−S) / n`；失败、取消及不明重试不入 n。
- TPS：成功、流式、计数范围对齐样本的 `1000 × Σ总生成Token / Σ(E−F)`。分子分母必须同批，不平均逐条 TPS，不机械扣一个 Token。

TTFT 与 TPS 各有独立分母；TTFT 可得不代表 TPS 可得。TPS 分子不用费用行全部 output。输出包含关系按源固定：含 reasoning 的 output 不再加 reasoning；互斥字段只有在时间窗也覆盖 reasoning 时才相加。不可确认输出范围就排除 TPS。

### 2.2 非正常数据

- 模型状态固定为 `success/failed/cancelled/inflight/unknown`。工具使用相同状态；正常失败的工具仍属于调用累计。
- inflight、缺起止、结束早于开始：不累计时长；记录缺失原因。不得用采集时刻替代结束。
- 正常零毫秒工具保留为 0，样本数为 0 才显示 `—`。F=E、零生成时长、output<=0：不计 TPS；合法 F=S 可计 0 毫秒 TTFT。
- 失败/取消即使有 partial usage，仍不计 TTFT/TPS；原 Token 采集不受影响。
- 成功且含重试的逻辑调用：没有可靠尝试边界时，整组剔除速度；模型时间按整个已观测逻辑调用一次累计，包含其中的退避，不再叠加子尝试时间。
- 工具窗口已包含的队列、结果处理、等待原样保留；不知道的审批时间不通过差额推导。
- 缺模型不会让计时丢失；缺工具/会话/稳定事件身份则拒绝该事实并计诊断。

### 2.3 区间筛选

复用 `range::resolve_range` 的 from/to 和宿主时区，不改变现有五档时间含义。区间为 `[A,B)`，统一判定：`S >= A && E < B && E <= 查询截止时刻 && terminal`。工具使用自己的调用/结果时间；模型使用自己的调用/终结时间。

从区间外开始或在区间外结束的样本，其四项贡献全部排除。恰在 B 结束也排除；S=A 可纳入。跨区间样本只在详情报排除数量，不从已入选速度分子保留其 Token。区间内部跨小时/跨日不排除。

性能与费用允许样本不同：费用沿用既有归桶，性能按完整包含过滤，卡片固定说明“性能仅统计区间内完整样本，与费用 Token 范围可能不同”。正在进行的调用不显示临时 TPS。

模型时间按 success/failed/cancelled 三类在详情分别汇总；无法对齐的重试只单列计数，不伪造失败尝试耗时。另列已观测但起止缺失的数量。总模型用时不保证等于参与速度统计的等待加生成。

## 3. 首版逐源规则

[静态证据索引](../evidence/2026-10-24-model-speed/README.md)。数据存在不等于产品指标成立；以下是编码时的固定能力表，不由执行者自行放宽。

| 源          | 模型时间                                                                                                                             | 工具窗口                                                                   | TTFT                                                                                                         | TPS                                                                                            |
| ----------- | ------------------------------------------------------------------------------------------------------------------------------------ | -------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------ | ---------------------------------------------------------------------------------------------- |
| ZCode       | model_usage.started_at → completed_at，包含可靠失败/取消                                                                             | tool_usage.started_at → completed_at；不使用其 duration_ms 代替窗口        | 成功、无已记录重试、单一逻辑尝试、首非空文本/思考齐全，且 tool_call_count=0 时采用 first_token_at−started_at | 本版禁用：历史字段不足以排除单块返回及首点前工具参数/隐藏输出错配                              |
| DSH         | 同 step 的 step/start → assistant/message 或 assistant/attempt；多尝试只计已闭合的逻辑模型窗口一次，不取含工具的 step/end 作模型结束 | tool/call → tool/result，按 callId                                         | 单次成功 step，逐成员展开 stream 后取首非空有效 delta；有 assistant/attempt 或 llm/retry 的组排除速度        | 同 TTFT，并命中§3.2明确总输出profile；至少两个非空输出成员且时间不同、outputTokens>0，否则null |
| Kimi        | 本版禁用：原生时长在内部重试中存在首点不重置，历史没有完整尝试事件供可靠排除                                                         | tool.call → tool.result；不以 result.durationMs 代替窗口                   | 本版禁用：原生首 part 不保证首非空 Token，且重试基线错配                                                     | 本版禁用：同上；保留原生 usage/timing 作证据，不作为正常速度                                   |
| OpenCode    | 本版禁用：assistant 消息时间包含工具                                                                                                 | v2 content 工具 created→completed；无 v2 库/表时用 v1 state.time.start→end | 本版禁用                                                                                                     | 本版禁用                                                                                       |
| Claude Code | 本版禁用：累计 cost-state 无请求时间及逐模型拆分                                                                                     | assistant tool_use 到对应 tool_result 的持久事件时间，按 tool_use_id       | 本版禁用                                                                                                     | 本版禁用                                                                                       |
| Codex       | 本版禁用：回合 duration 不能当模型请求时长                                                                                           | 优先新 item 工具起止，缺失则同 callId 的旧 begin/result；不相加两代记录    | 本版禁用：回合 TTFT 及 mapper/OutputItemAdded 均不符合首有效输出定义                                         | 本版禁用                                                                                       |
| WorkBuddy   | 本版禁用                                                                                                                             | 同 callId 的 function_call → function_call_result 时间                     | 本版禁用                                                                                                     | 本版禁用                                                                                       |

“本版禁用”是确定的降级行为：接口返回 null+原因，UI 显示 `—`，不是留给执行者的研究任务。未来启用新来源必须新设计及证据审查，本次 Agent 不增设配置开关来绕过能力表。未知版本仅按可验证结构解析时长；速度只接受下列固定判据，缺任何必要字段即拒绝。

### 3.1 ZCode

主键沿用 model_usage.id；logical_request_id 非空时检查该逻辑组的尝试。retry_count>0、attempt_index非零、同逻辑请求多行均使速度失效。TTFT 只处理成功、非工具调用的样本；first_token_at 仅首非空 text_delta/reasoning_delta，因此不能给工具参数输出冒充首 token。总输出含 reasoning，不相加。

工具 model 归属固定连接 `tool_call_id → part.callID → 所属 message → model_usage.assistant_message_id`：所有边唯一且模型一致才赋值；缺边/多义归未知模型。时长窗口与原生 duration 均存，工具窗口为主指标。

### 3.2 DSH

按现有代际解析逻辑读取会话历史；稳定事实键为会话头 id + turn + step（工具加 callId），不把文件代际名作为新会话。重复出现同一步骤使用内容相同的已结算事实；冲突保留诊断并排除对应指标，不能累加副本。

stream 的 chunk 直接取原始 time；紧凑段从 time0 按 dt 累加，每个成员逐项验空。有效类型为非空 text、reasoning、tool-argument delta；block-start/end、usage、finish 不算输出。原生跨 attempt 保留首点却采用最终 output 的公式不用于本版 TPS。

TPS总输出scope固定检查同一消息的`data.message.source.replayState.response`：`kind=pi-ai && version=2 && api=openai-completions`，并且其中provider/model与消息source完全一致。此profile的原生completion_tokens已含reasoning，经usage.output原样映射outputTokens；来源证明见[总输出补证](../evidence/2026-10-24-model-speed/dsh-output-scope-addendum.md)。其他profile或缺失/不一致一律unknown-output-scope，TTFT仍可独立有效，TPS为null。禁止按provider显示名或当前配置猜API。

该TPS只表示供应商总输出在客户端首有效输出至该消息结算窗口内的吞吐，不承诺服务端无缓冲或隐藏思考独立耗时；数据中没有的首点不从服务端计数反推。原始stream含思考时首点必须覆盖该思考，不能仅取正文开始。

模型/供应商取该成功消息 source；工具通过 call 的 turn/step 归属发起消息。失败步骤只有独立可验证模型起止才计时：最后失败 assistant/attempt 终结可封闭逻辑窗口；只有 step/end 而没有模型终结时，记起止缺失，不把后续工具时间加进模型。

### 3.3 其余源

- Kimi 以 agent 文件 + toolCallId 配对；result 继承 call 的 stepUuid/turn/step。模型仅取同 agent、同 turnStep、无多义的 llm.request，禁止继承会话最后模型；缺失归未知。前置审批发生在 tool.call 之前，不计入该已观测窗口。
- OpenCode 有 v2 时不再采 v1 镜像；工具归属所在 assistant 的原始 model/provider。原地 UPDATE 的未终态对象持续重读，不能仅按 rowid 越过。
- Claude 按每个 content.tool_use.id 分别建立调用，结果按 tool_use_id 配对；不能把 assistant.uuid 当成整批工具唯一身份。同 message.id 多段记录去重，不能重复工具起点。
- Codex 同 session+callId 合并新旧投影，新起止完整者优先；新记录只有 duration 而无起止时用可配对旧事件窗口，不把 duration 虚构成开始。tool 发起时模型来自当时 turn_context，不取结果时模型；没有唯一上下文则未知。
- WorkBuddy 以 callId 配对、调用的 providerData.model 归属模型，供应商维持原有未知三态。不能用 messageId 或 traceId 代替工具 callId。
- OpenClaw 仅保留探测报告，本版不增加 UsageSourceId 或费用源。

## 4. 固定数据结构

性能独立保存，不增加任何费用 Token。首版只新增两张表，不做小时/日性能聚合；精确完整包含筛选直接查询有索引事实，避免聚合丢失跨界信息。

### 4.1 usage_performance_fact

| 字段                                                                                | SQLite 类型及约束                                | 语义                                                                              |
| ----------------------------------------------------------------------------------- | ------------------------------------------------ | --------------------------------------------------------------------------------- |
| source_id, fact_key                                                                 | TEXT NOT NULL，联合主键                          | 工具及源内稳定事件身份；不使用采集时间作键                                        |
| kind                                                                                | TEXT NOT NULL CHECK IN(model,tool)               | 样本种类                                                                          |
| session_id, project_key                                                             | TEXT NOT NULL                                    | 继承现有项目规范                                                                  |
| model_id                                                                            | TEXT NOT NULL                                    | 原始模型；空串为未知模型                                                          |
| provider_kind, provider_value                                                       | TEXT NOT NULL / TEXT NULL                        | kind为measured/inferred/unknown证据强度；value的null/空串/非空保持区别            |
| parent_session_id, turn_id, logical_request_id, attempt_id, call_id                 | TEXT NULL                                        | 原始关联证据；不猜值                                                              |
| is_subagent                                                                         | INTEGER NULL CHECK IN(0,1)                       | 不可判为 null，不伪装父会话                                                       |
| outcome                                                                             | TEXT NOT NULL                                    | success/failed/cancelled/inflight/unknown                                         |
| started_at_ms, first_output_at_ms, ended_at_ms                                      | INTEGER NULL                                     | 原始时间，缺失保留 null                                                           |
| native_duration_ms, model_ms, tool_window_ms, ttft_ms, generation_ms, output_tokens | INTEGER NULL                                     | 非负；只有通过各指标规则者填贡献列                                                |
| timing_basis, output_scope, rule_version                                            | TEXT NOT NULL / TEXT NOT NULL / INTEGER NOT NULL | 来源口径、输出包含关系、规则版本，首版版本=1                                      |
| exclusion_reasons_json                                                              | TEXT NOT NULL DEFAULT '[]'                       | `{metric,code}`数组，分别解释模型/工具/TTFT/TPS排除，不因速度无效抹去可靠模型时间 |
| source_revision, observed_at_ms                                                     | TEXT NOT NULL / INTEGER NOT NULL                 | 源结构指纹及观测时间，不作统计归属                                                |

索引固定为 `(source_id,ended_at_ms,started_at_ms)`、`(source_id,model_id,ended_at_ms)`、`(source_id,session_id,call_id)`。逻辑调用与尝试不同时写 model_ms 贡献；优先逻辑窗口，尝试只作证据。未知模型仍可保存工具窗口。

### 4.2 usage_performance_cursor

主键 `(source_id,file_key)`；字段 `cursor_version INTEGER=1`、`offset/ordinal`、`fingerprint`、`state_json`、`updated_at_ms`。文件状态保存 pending 调用/步骤与已结算稳定键；SQLite 源保存未终态主键待回看。

不复用 Token 游标，不把旧 0 当新性能水位。首次历史回填状态固定 `pending/running/complete/failed`，记录错误码及已处理数，失败后从已提交水位恢复。

源级回填状态存同表`file_key=@source-state`行的state_json；其offset/ordinal为0、fingerprint为空，只是状态元数据，不参与文件续读或数值统计。

state_json固定记录`retainedFromMs/coveredFromMs/backfillRevision/state/errorCode/processedCount/unrecoverableRanges`，最后一项为`{fromMs,toMs}`数组。减少保留期后清理事实并将coveredFromMs推进至新的保留起点；增加保留期时若新起点早于coveredFromMs，state重置pending、backfillRevision递增，独立回放新增窗口。性能文件水位按该revision重新建立，Token游标原样保留。源历史已删除时结束回填并保留不可恢复范围诊断，不伪造新事实或永久反复重扫；coveredFromMs表示已完成扫描的起点，不保证源历史完整，诊断不得隐去。

### 4.3 持久化与保留

事实 UPSERT 与对应水位同一事务，单批最多200条；相同源主键覆盖更完整终态而非追加。终态数据冲突不重计并给 source-conflict 诊断。跨源推测/时间接近不用于合并事实，本版没有遥测第二来源。

明细保留期沿用现有 detailRetentionDays，默认90日；性能不借用旧 usage_daily。查询区间起点早于保留截止时，四项全部 null+retention-expired，不把残存数据当整段结果。减少保留期后删除相应性能事实；增加保留期以独立性能回填恢复源仍存在的历史，无源数据就报告不可恢复。

正常同一文件一遍读取同时产出 Token 与性能。回填重读只产出性能，对旧 Token 水位之前的记录不得再产出费用增量。复用现有后台采集单飞、SessionFileScan与锁序；不加入3秒会话轮询，不由查询触发扫描。以10MiB读取预算分批推进历史回填（DSH压缩帧完整性沿用现有容错）；预算耗尽让出线程，下一次后台批次续读。取消回填保留已提交事实和水位，不撤销正常用量。

## 5. 查询与接口

新增只读 IPC `usage_performance(range, filters)`，沿用 UsageRange/UsageFilters，async + spawn_blocking；命令层不持DB锁、不扫描，服务层用一次只读快照聚合。总用量开关关闭时返回 disabled 空响应，不初始化真实采集。

响应为 `PerformanceResult{rangeFromMs,rangeToMs,asOfMs,retentionFromMs,backfillState,rows,sourceStatuses}`。每行包含：

- identity：原始工具、provider三态、模型；project过滤沿用既有解析。
- metrics：`modelMs/toolMs/avgFirstTokenMs/tps`，均显式 number|null。
- counts：`modelSamples/toolSamples/ttftSamples/tpsSamples/success/failed/cancelled/missingTiming/crossBoundary`。
- details：`successModelMs/failedModelMs/cancelledModelMs`，分别 number|null；`retryExcludedCount`、`reasons`。

空分母返回 null；合法测得0毫秒返回0。耗时/Token内部64位整数累加、检查溢出，非法贡献排除并报 invalid-value；平均只在末端做浮点除法。原因枚举固定：disabled/no-source-data/unsupported-timing/missing-start/missing-end/invalid-value/inflight/failed/cancelled/retry-unaligned/unknown-output-scope/non-streaming/zero-generation/unknown-model/source-conflict/cross-boundary/retention-expired/backfill-incomplete。

sourceStatuses逐工具给出`sourceId/readState/backfillState/metricSupport/reasons`，readState为ok/failed/absent/disabled，metricSupport为四指标各自的布尔值；禁用项即使费用卡有Token也返回unsupported-timing。sourceStatuses不使用全局空行代替逐源原因。

详情计数规则：有完整起止时，区间内样本与跨边界样本分别计数；只有开始时刻的缺结束样本按开始在区间内计missingTiming，只有结束时刻的缺开始样本按结束在区间内计missingTiming；两者皆缺只记源级诊断，不按observed_at分配到当前区间。crossBoundary只计与区间相交但未完整包含的可定位样本。失败/取消耗时为空时，详情显示缺失而非0。

尚在回填或源读取失败时，可返回已观测值，但详情标明已采样数量和状态，不称全量。不新增“部分”指标档位或用已估价覆盖率代替计时覆盖。某源不支持的模型指标不影响该源工具指标和其他源。

渠道/别名/模型显示名复用当前查询规则；不改原始身份。按事实跨度匹配既有渠道规则，跨度内归属不唯一则保持含糊，不能强指某渠道。父子代理按现有 include/parentsOnly 规则；现有日档 parentsOnly不可判守卫保持不变。不能先按显示模型名合并后遗失工具身份。

PerformanceRow固定附带`channelKey`及原始identity；CostRow新增只读`performanceIdentities: IdentityKey[]`，由费用分组时保留原始成员（去重），不改变费用计算或row.key。前端按channelKey和成员精确匹配，不用显示名匹配。相同原始身份被不同价格分片重复列卡时，重复展示同一工具性能并固定标注“该模型在本区间的性能”；不把这些重复展示值再作合计。只有provider证据强度不同而原始identity相同的事实可按同身份折叠，null/空串provider仍分开。

## 6. 确定的页面行为

现有每张模型费用卡主行下方增加四项2×2性能网格，始终显示，不放在计费详情折叠中。顺序为模型用时、工具调用用时、平均首 token、流式 TPS。多工具费用卡内逐工具网格，不给跨工具合计TPS或新的全局性能排行榜。

耗时复用既有 fmtDur；TPS显示1位小数、单位tok/s，TTFT复用毫秒/秒格式。缺值显示 `—`，提示源不支持/无有效样本等实际原因，不填0。固定说明累计时长可重叠、跨边界排除、性能与费用样本范围不同。

每组有独立“性能详情”入口，展示有效样本数、成功/失败/取消耗时、缺计时和跨边界排除数、回填状态；不显示提示词或原始响应。主界面不增加第五项失败耗时，也不展示服务端解码/纯正文速度。

费用有行而性能rows为空时也保留四项网格：前端同时传递sourceStatuses、backfillState和查询loading/error/ready状态。读取失败显示失败原因，无源数据、能力禁用及回填未完分别解释；不能仅按空数组统一显示“无数据”。性能查询失败不改变费用结果。

费用行和性能行按工具+原始身份精确匹配；性能无对应费用时，保留性能行，费用显示 `—`。未知模型统一在对应工具/渠道上下文的“未知模型”卡；不得混入某个已知模型，也不得因费用缺失被内连丢弃。仅有工具无模型的行，模型指标为 `—`，工具窗口正常显示。

中文/英文分别在 zh/en locale同步；操作与统计口径进双语手册，功能变更进CHANGELOG中英两侧；README只按文档分工增加功能一句话。本版不改分享图片模板、不改原有Token/费用查询或旧工作活动工具耗时数值。

## 7. 验收用例与禁止事项

| 场景                                     | 确定结果                                            |
| ---------------------------------------- | --------------------------------------------------- |
| 成功请求S=0,F=2000,E=7000,O=500          | 等待2000ms，生成5000ms，TPS=100；模型7000ms         |
| 第二成功请求等待1000ms、生成1000ms、O=50 | 两条平均等待1500ms；合并TPS=550/6，而非(100+50)/2   |
| 再加失败请求3000ms                       | 模型总时长12000ms，失败详情3000ms；两项速度不变     |
| 工具窗口0→10000，原生duration6000        | 工具累计10000ms；保留6000证据，不混合求和           |
| 两工具并行各10秒                         | 工具累计20秒正确，不显示成实际经过20秒              |
| [A,B)内有S=A/E<B                         | 纳入；S<A、E=B、E>B均整条排除；区间内部跨日正常纳入 |
| tool结果未到、模型仍在输出               | 不纳入临时耗时和速度；完成后幂等更新                |
| 取消有partial usage                      | 原Token路径照常；性能仅计可靠模型时长，不计速度     |
| DSH retry/attempt                        | 整组速度排除；同逻辑窗口不重复叠加尝试              |
| Kimi、Claude、Codex等本版禁用项          | 始终null及明确原因，不能因字段名字相似启用          |
| 同事实重复读、DSH代际复制、OpenCode双代  | 性能不增加；原Token账本不因回填增加                 |
| 模型切换、缺发起模型                     | 按调用发起身份；多义归未知模型卡，不归给结果时模型  |
| 查询跨性能保留边界                       | 四项null+retention-expired，不混合部分历史          |
| 源失败/回填未完                          | 独立源状态与样本数可见，其余源继续工作              |

实现验收以真实脱敏结构样本与只读重算对账，不从旧文档手抄不真实夹具。先停开发应用或隔离TUVIS_HOME，再修改usage域；集成验证只写影子数据库。单次调用、工具并发、取消和重试的动态场景在隔离数据根复核，不为扩大兼容性而改本版能力表。没有安全实测条件的禁用项保持禁用，不阻塞其他固定行为的编码。

源码核查已确认的首点、审批及重试问题写入证据增补；不再把这些事实包装成待用户选择。实施前同步主分支并检查API/类型是否变化；同步失败时停止实施，保留设计，不在旧基线上悄悄编码。

## 8. 设计完成与交接

用户已确认Q1—Q9及完整包含区间规则，无待裁决产品项。后续执行者必须按本文的能力表、字段、算法、降级和验收实现；不建设遥测接收器、不新增Hook、不放宽首输出口径、不新增工具源。

用户随后已授权编写superpowers实施计划及执行Agent提示词。本轮按本文编写计划、核对依赖和验收，但不写产品实现。计划不得把已冻结行为重新改写成开放选择；执行结果返回后由本会话review。
