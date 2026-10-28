# 原生计时与归属源码核查增补

日期：2026-10-24。本文补充已有静态报告遗漏的字段和计时边界，供正式设计引用。核查过程只读本机安装源码、已有证据与少量字段，没有修改工具配置、启停工具、发送模型请求或写入真实账本。本文不代表实测验收。

## 1. 正式产品口径及证据门槛

用户已确认：四项指标均为累计工作量，不展示真实经过时长或阶段占比。工具调用耗时统一使用 call→result 窗口，包含窗口内的等待；窗口外审批不补入。总生成输出包含思考。失败、取消仅计有可靠边界的模型耗时，不进入 TTFT/TPS。一个样本的完整计时区间须落入所选范围才参与统计。不增加原生遥测接收器。

因此，原生工具执行 duration 仅保留为来源证据或对账值，不能自动替代统一的调用窗口。源缺少可靠边界、完整区间、同批输出、重试识别或总生成口径证据时，对应正式指标固定为空；不能用候选可算值占位，也不能写成“后续再决定是否显示”。未来如补足证据，应通过新的来源验证与规则版本启用，不改变本轮的空值结论。

## 2. 本机来源与适用限制

| 来源  | 本次核查入口                                                                                                    | 定位方式与限制                                                                                                                                                       |
| ----- | --------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Kimi  | `~/.kimi-code/bin/kimi` 的内嵌 JavaScript                                                                       | 既有报告记录文件 188,155,456 字节，SHA256 `4bab6f96c2c289368b7f05a04f66737c53a59ce740f932d8f149240584758efb`。下文偏移为本次读取的文件字节偏移附近，升级后不可沿用。 |
| ZCode | `/Applications/ZCode.app/Contents/Resources/glm/zcode.cjs`                                                      | 下文偏移为读取并解码后的源码字符串索引附近，不保证等于文件字节偏移；压缩源码升级后符号名与位置会变。未新增可执行版本探测。                                           |
| DSH   | `/Applications/DeepSeek Harness.app/Contents/Resources/app.asar` 中 `dsh/node_modules/@deepseek-ai/` 的打包源码 | 通过只读解析 asar 索引读取有关成员；以下以包内路径与函数名定位。另核对官方原始源码。                                                                                 |

既有数据库/日志的计数与覆盖率沿用 `sqlite-sources.md`、`dsh-kimi.md`，本增补没有重复全量扫描。当前安装实现不能反向证明全部历史版本实现相同；历史样本仍需版本与结构门控。

## 3. Kimi：工具原生 duration 与窗口的遗漏及边界

### 3.1 已静态确定

原报告仅列出 `tool.call→tool.result`，遗漏了持久结果中的原生 `event.result.durationMs`。当前安装源码有如下顺序：

```text
正常内置审批 beforeExecute
→ willExecute hooks
→ dispatchToolCall / tool.call
→ ToolScheduler 队列
→ scheduler.start：开始计原生 duration
→ task.execute / runSingleExecution
→ task 返回：停止计原生 duration
→ finalizeToolResult / 最终 hooks / 结果处理
→ tool.result
```

证据位置：

- `AgentPermissionGate` 构造器在 `toolExecutor.onBeforeExecuteTool` 注册 `adjudicate`，其中等待 `toolApproval.resolvePermissionResolution`：偏移约 `101325089` 起。
- `prepareToolCall` 先等待 `beforeExecuteEmitter.fireBeforeExecute`、`willExecuteEmitter.fireAsync`，之后才 `dispatchToolCall`：偏移约 `103679000`。
- `executeBatch` 在 `ToolScheduler.start` 内取 `Date.now()`，`task.execute` 返回后计算 `Math.max(0, Date.now() - startedAt)`：偏移约 `103682142`。
- `finalizeTimedResult` 在计时结束后执行 `finalizeToolResult`，并把 `timedResult.durationMs` 继续传递：偏移约 `103677000`。
- `appendMachineToolResult` 把 duration 写进持久 `tool.result.result.durationMs`：偏移约 `103738511`。

据此，当前正常内置审批路径下，原生 duration 排除前置审批、调度队列和最终 hooks；call→result 窗口排除前置审批，但包含队列及最终结果处理。不能把 call 窗口笼统描述为“包含审批”，也不能把 duration 笼统描述为纯 CPU 时间：工具内部等待、子任务及 MCP 服务内部排队仍可位于执行期间。

### 3.2 对正式设计的影响

正式工具指标使用配对 call/result 的完整窗口。窗口外的正常前置审批没有证据可补入，按确认口径不补。原生 duration 可用于解释与窗口的差异，但不替代窗口，也不把两者差额命名为审批时长。历史 `result.durationMs` 的完整覆盖率未新增扫描，不宣称全部历史均有此字段。

### 3.3 未做的实测

最小场景为一次延迟批准的短工具，以及一组资源冲突的并行短工具，用于核对持久事件与源码顺序、调度等待是否进入窗口。该实测不影响当前已确定的产品选择。额外的工具内部权限路径尚未逐工具核查，不能从正常前置审批路径推及全部工具。

## 4. Kimi：模型时长、首点与内部重试错配

### 4.1 已静态确定

`ModelRequesterImpl.runRequest` 在请求构建及凭证解析前设置 `requestStartedAt`，首次 `llm.streaming.part` 到达时记录 `firstChunkAt`，在 `llm.done` 记录 `streamEndedAt`。`buildStreamTiming` 的公式为：

```text
firstTokenLatencyMs = max(0, firstChunkAt - requestStartedAt)
streamDurationMs = max(0, outputEndedAt - firstChunkAt)
outputEndedAt = streamEndedAt ?? Date.now()
```

对应源码：`buildStreamTiming` 约偏移 `98726191`；`runRequest` 约偏移 `98728974`；事件 switch 约偏移 `98730425`。模型计时是客户端请求/流生命周期，含首次请求构建、凭证解析、流消费与缓冲，不是服务端纯解码时间。两个原生时长已经相邻，无需再从 streamDuration 减去 TTFT。

完整 switch 直接证明了内部重试风险：再次 `llm.sent` 会重置 `requestStartedAt`、accumulator、usage、finish、messageId，却未重置 `firstChunkAt`；`llm.request.retrying` 也仅重置消息/用量相关状态，没有重置首点。因此，首点来自旧尝试、usage 来自后续尝试的错配路径在代码上存在，不能将“字段为正”视为同批证据。

`step.end` 同一事件携带 `usage.output` 与两个原生 timing，可避免通过相邻 `usage.record` 猜配。写入位置约偏移 `103741132`。不过“同事件持久化”不能消除上述内部重试错配。

`llm.request` schema 另有可选 `attempt: string`，约偏移 `103591490`；原报告遗漏此可选字段。一次有界小日志字段补查看到 1 条 request 未带 attempt，该结果仅证明字段可缺，不能推断全量覆盖或尝试含义。`turnStep` 或 request 条数也不能证明下层 SDK 没有内部重试。

### 4.2 对正式指标的限制

历史仅有拼装后的 `content.part`，缺少完整逐流事件与下层重试账本；首次 part 是否非空有效输出、总 output 是否覆盖全部思考、是否真实多段流式，不能由同一 step.end 的正时长独立证明。凡不能排除重试错配或不能证明总生成计数与生成窗口对齐的样本，正式 TTFT/TPS 为空。不能将“客户端流速候选”当成已启用的正式指标。

失败、取消只有边界可靠且满足完整区间条件时才计模型时长，不进入速度统计。发生已知重试错配时，不能仅用被重置/夹零后的两个时长之和声称可靠逻辑请求耗时。

### 4.3 未做的实测

最小场景：普通长流、含思考流、首次片段已到后失败再成功的一次本地协议夹具。最后一项针对首点重置缺口；没有可观测尝试证据的历史不能靠新场景补造字段。

## 5. ZCode：首点只取正文或思考，不含工具参数

### 5.1 已静态确定

安装源码 `_I` 写入 model usage：终点为写事实时 `Date.now()`，duration 为终点减 `startedAt`，usage 优先取本次 `result.usage`，失败时可回退网络失败事件的 usage；约源码索引 `13026500`。这不是最后一个生成 token 的独立计时。

`r6o` 从本次网络事件起点之后查找 `ModelStreaming`，只接受 `payload.kind` 为 `text_delta` 或 `reasoning_delta` 且 delta 非空，返回首条事件时间；约源码索引 `13032400`。因此工具参数增量不在该首点判据中。仅工具调用输出或思考/正文之前已开始生成工具参数的请求，不能假设首点覆盖全部生成输出。

`retry_count` 由本次 `ModelNetworkStatus` 中 `model_retry_scheduled` 的条数计算；`logical_request_id`、`attempt_index` 还需共同用于逻辑组去重。不能因本机 attempt_index 均为 0 就宣布没有内部重试。

工具事实至少有两条写入路径：`ToolCallResult.payload.duration` 写入 `duration_ms`（约源码索引 `13031281`），以及工具执行结果 `durationMs` 直接写入（约 `13290507`）。起止来自事件时间或结果对象。此次没有追完所有底层 duration 生产点，因此只能确认原生值的写入来源，不能把原生值与墙钟窗口的差额解释成审批/队列。

### 5.2 对正式指标的限制

静态候选至少须满足成功、正输出、retry_count=0、逻辑组无多尝试歧义、首点齐全和正生成窗口。但这仍不能证明 provider 无隐藏重试、实际多段流式、全部思考计数及工具参数生成均被时间窗覆盖。缺少这些证据的正式速度指标为空；不能因数据库同一行能相除就发布总生成 TPS。

工具耗时采用已验证可配对的调用事件窗口，不用原生 duration 自动替换。原表 `started_at` 是否就是产品定义的 call 发起点，应由事件写入路径证明；不能只因列名存在就断言包含某种等待。

### 5.3 未做的实测

最小场景：普通长流、仅工具参数输出、含思考输出，以及一次延迟审批的短工具。它们分别核对首点覆盖、总生成计数语义与 call 窗口边界；没有执行这些场景。

## 6. DSH：逻辑 step 重试必须剔除速度样本

### 6.1 已静态确定

安装包 `dsh-session-stats/lib/types/projection.js` 与[官方投影源码](https://raw.githubusercontent.com/deepseek-ai/deepseek-harness/master/packages/session/session-stats/src/projection.ts)一致：模型时长从 `step/start` 至组装成功的 `assistant/message`，工具时长按 callId 配对 `tool/call→tool/result`。失败 attempt 中出现的首个有效输出可跨重试保留，但 decodeTokens 使用最终成功消息的 outputTokens。

所以跨 attempt 保留首点、再使用最终成功输出的组，分子分母确实可能不属于同一批。它不是“字段语义尚未决定”，而是明确的速度排除条件。原生 sessionStats 累计值只能对账，不能直接作为严格无重试总生成速度。

安装包 `dsh-llm-retry/lib/index.js` 证明持久 `llm/retry` 数据含 `retryId`、`turn`、`step`、`provider`、`policyKey`、`retry` 等字段。恢复 `session+turn+step` 事件组后，出现 `assistant/attempt` 或 `llm/retry` 的组排除 TTFT/TPS。只有从完整事件链证明单次成功尝试且流/usage 对齐的组才有资格继续检查。

流式紧凑段必须展开到逐成员，检查首个非空 text/reasoning/tool-args delta，不能不验空就采用 time0。两个以上非空成员在不同时间到达，只证明客户端记录到多时刻输出；不能单独证明服务端没有缓冲或适配器没有隐藏重试。

### 6.2 对正式指标的限制

取消未组装消息的步骤不具备原生投影完整计时，不填 0。成功 step 的 start→结算含步骤内开销；若只有 step 级证据，不能改称精确网络请求级耗时。审批/队列是否位于工具两个事件之间，此次未查到完整生产点证据；保留窗口本身，不命名差额，不补窗口外等待。

总 output 是否包含全部思考、隐藏输出是否被流窗口覆盖等仍缺证据的样本，正式 TPS 为空。不得因首点与正窗口可算就启用。

定稿补证已见[dsh-output-scope-addendum.md](dsh-output-scope-addendum.md)：仅pi-ai/version2/openai-completions且source一致的持久profile可证明原生总输出映射，其余profile继续按本节为空。该客户端统计不作无缓冲或纯服务端解码承诺。

### 6.3 未做的实测

最小场景为一次失败 attempt 后成功的可控本地夹具、一次含思考长流，以及一次延迟审批的短工具。已有源码确定的重试剔除规则无需等实测才采用。

## 7. 模型与工具的保守精确归属算法

以下为实现规则，不是需要用户选择的产品口径：

1. 模型请求以事件自身的原始工具、供应商、模型身份为准。ZCode 直接使用 model_usage.provider_id/model_id；DSH 使用成功消息 message.source.provider/model；Kimi 使用同会话、同 agent 文件、同 turnStep 且无歧义对应的 llm.request.provider/model，保留全部 attempt，禁止用 map 覆盖掉重试。
2. 工具先定位产生调用的 assistant 消息或 step，再继承该发起请求的模型。result 只继承所配对 call 的身份，不采用完成时的当前模型。
3. ZCode 可核验 `tool_call_id → part.callID → 所属 message → model_usage.assistant_message_id` 的精确连接。该连接是待真实 schema 校验的实施路径；所有连接唯一且模型一致时才赋值，否则未知。不能以回合内最近模型或会话末模型补齐。
4. DSH 由 tool/call.turn/step 连接产生该工具调用的成功消息，并核对 callId。若一个组存在多个候选或代际重复未消解，归属未知。
5. Kimi 由 tool.call.stepUuid/turnId/step 定位对应 step 请求，result 用 parentUuid/toolCallId 回连。老记录没有精确请求证据时，不能把此前某条 usage.record.model 沿时间线自动继承为每个后续 step 的精确模型。
6. 缺连接、冲突、模型切换不明或多尝试歧义都进入未知桶。工具级可得不推出模型级可得，计时覆盖与模型归属覆盖分别计算。

## 8. 设计条文应如何保持一致

四项正式指标中，工具项统一为调用窗口，不再把“纯执行 duration 优先”与“调用含等待”并列作为可互换实现。事实层可并存原生 duration、call/result 起止、来源与边界说明；只有调用窗口参与本轮正式工具聚合。Kimi 的正常前置审批在窗口外，是已知边界，按已确认口径不补入。

速度项要求总生成计数、首点和生成结束属于同一有效尝试，并有思考与流式语义证据。上述三源目前的可算候选与正式可用集合必须区分；缺证据即空。实测计划是未来补证清单，不构成本轮继续显示候选数值的许可，也不引入额外遥测接收器。
