# 文件类三源：模型耗时、工具耗时与速度的只读可得性探测

日期：2026-10-24。范围：Claude Code、Codex、WorkBuddy。本轮只读真实日志与 SQLite，未启动工具、未更改配置、未安装插件、未触发采集、未修改账本或生产代码。

## 1. 结论与证据等级

现有历史不能完整支持「按现有区间 + 工具 + 模型费用行」显示四项精确指标。**Codex 已有真实的回合 TTFT；Claude 已有累计 API/工具时长快照；WorkBuddy 已有 reasoning token 明细**，不能把这些新发现写成完全不存在。但它们各自都不足以证明逐请求流式 TPS。

| 来源        | 模型累计用时（排除工具执行）                                                                            | 工具累计用时                                                                     | 平均首 token 等待（逐模型请求）     | 排除等待后的流式 TPS                                                         |
| ----------- | ------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------- | ----------------------------------- | ---------------------------------------------------------------------------- |
| Claude Code | `cost-state.totalAPIDuration*` 是候选累计值；无请求时间、无逐模型时长、无快照 timestamp，不能准确切区间 | assistant→tool result 时间差可估，原生累计快照可交叉核验；审批/排队/并行边界未明 | 历史样本无请求起点/首 token；不可得 | 不可得                                                                       |
| Codex       | `task_complete.duration_ms` 是回合墙钟；不能减工具总和当模型时长                                        | 新 `item_completed` 有工具原生 duration 和起止；旧 response item 可配对估算      | 回合 TTFT 可得；逐请求平均仍不可得  | token usage 有 response_id，但无该请求的开始/首 token/生成结束时间，故不可得 |
| WorkBuddy   | 无请求原生时长；相邻消息差不作模型时长                                                                  | `callId` 配对时间差可估；无原生 duration                                         | 不可得                              | 不可得                                                                       |

「可估」应单列质量标记与覆盖分母，不能作为精确值混进同一分母。模型请求墙钟（包含请求等待）与首 token 后的流式时长必须分开；隐藏思考的首输出、首个可见字符也必须分开。工具并发的调用时长求和可以大于实际经过时间。

## 2. 方法、样本与限制

先枚举、stat、按 mtime 选样，再读取；每文件最多尾部 8 MiB、最后 20,000 行。Claude/WorkBuddy 取最新 2 文件 + 最大 3 文件（去重），兼顾最新与有实质内容的旧样本；Codex 取最新 5 文件。取样顺序为 `(mtime, path)` 降序，最大文件并列按原顺序。mtime 只用于取样，绝不当请求或 token 时间。

首轮样本如下（后续补查仍只读这类有界选样，Codex 活动日志有追加）：

| 来源         | 文件库存（仅 stat）     | 读取文件 | 逐文件有效行数            | 合计 | 读取字节合计 |
| ------------ | ----------------------- | -------- | ------------------------- | ---- | ------------ |
| Claude       | 24，6,570,892 字节      | 5        | 92 / 56 / 1496 / 166 / 61 | 1871 | 3,724,435    |
| Codex 活跃库 | 82，约 295 MB，持续增长 | 5        | 39 / 43 / 382 / 54 / 489  | 1007 | 7,244,302    |
| Codex 归档库 | 5，1,912,113 字节       | 0        | 未采内容                  | —    | 0            |
| WorkBuddy    | 10，1,094,837 字节      | 5        | 8 / 6 / 167 / 66 / 40     | 287  | 905,970      |

首轮 15 文件均未触发尾截断，均 0 解析失败，单次读取前后文件大小未变。Claude 样本 mtime 为 2026-09-19 至 10-07，WorkBuddy 为 09-04 至 10-13，Codex 为 10-23 UTC（本地 10-24）。最新不等于最新产品版本代表性；Claude 最新两份包含既有闭环探测会话，WorkBuddy 最新两份很短。它们只是本机有界样本，不是全库、全版本或全平台承诺。

后续补查 Codex 的分母由 116 个 token_usage_record 增至 133、item_completed 增至 336，说明活动文件不断追加；下文明确区分首轮分母与补查分母，不拿它们相互算覆盖率。没有复制原始会话文字、命令、参数、结果或密钥到报告。

若未来触发尾读：第一段残行需丢弃，session_meta / turn_context / 尚未结束的工具起点可能不在窗口，所有关联率仅限窗口内；不能据此宣称全会话缺字段或丢调用。

## 3. 数据库与现有账本

读取 SQLite 使用 `file:…?mode=ro`、`PRAGMA query_only=ON`；没有以 immutable 绕过 WAL，也未执行迁移或写入。

- `~/.tuvis/tuvis.db`：`usage_detail` 的粒度为 `(source_id, session_id, project_key, hour_key, model, provider)`，有四桶 token、requests、tool_calls、tool_ms、turn_ms、tool_stats；没有 request_id、TTFT、模型请求起止或流式时长。`usage_daily` 只有 token/request 等日聚合，连工具时长也未保留。当前库不能从聚合反推这些新指标。
- 只读聚合快照：Claude 为 62 明细行 / 357 requests / 389 tool_calls / 19,727,232 tool_ms；Codex 为 770 / 15,941 / 20,158 / 153,659,591；WorkBuddy 为 6 / 62 / 71 / 626,007。这是当前账本快照，**不是样本覆盖率、真值或完整性认证**。
- `~/.codex/state_5.sqlite`：`threads` 有 rollout_path、model_provider、model、tokens_used、created_at(_ms)、updated_at(_ms) 等；无逐请求计时表。这些线程时间不能充当请求时间。
- `~/.workbuddy/workbuddy.db`：9 条 sessions；sessions 有 id/cwd/model/created_at/updated_at 等。`session_usage` 的列为 session_id/used/size/updated_at/credit_json，没有请求起止或首 token。used/size 是上下文填充信息，不能当本轮 token 桶。文件库存有 10 份 JSONL，不等于数据库枚举的 9 条会话；本次未查差集，不推断原因。

代码依据：`src-tauri/src/services/usage/collectors/{claude,codex,workbuddy}.rs`、`src-tauri/src/database/schema.rs`、`src-tauri/src/monitor/{codex_parser,workbuddy_parser}.rs`。遵守 `docs/MASTER-PLAN.md` 的只读与分期边界；未发现本研究需要更改宪法的冲突。

## 4. Claude Code

### 4.1 路径与结构

原始源：`~/.claude/projects/**/*.jsonl`，包括子代理文件。样本携带版本 2.1.278 / 2.1.287 / 2.1.289。事件有 assistant、user、system/turn_duration、cost-state、附件与界面状态记录。

- assistant：`sessionId`、`uuid`、`parentUuid`、`timestamp`、`message.id/model/usage`，部分有 `thinkingDurationMs`。
- 工具：assistant 的 `message.content[].type=tool_use`；回执 user 的 `sourceToolAssistantUUID` 指向 assistant.uuid，内容项另有 tool_use_id。现采集器按 assistant uuid 配对，单条 assistant 多工具时仅留第一个计时，不能认为覆盖每项并发工具。
- `system/subtype=turn_duration`：原生 durationMs；样本 16 条，1492–4,247,097 ms，是回合时长，不是单次模型生成。

### 4.2 token 与关联

695 assistant 行只有 249 个不同 message.id；213 个 id 出现多行，其中 70 组 usage 随行变化。不能逐 assistant 行累加 token 或把每行当一次请求。现采集器已有去重/累计处理，新计时必须沿用同一请求身份口径。

695/695 行有 input_tokens/output_tokens；578/695 有缓存字段，577/695 有 `output_tokens_details.thinking_tokens`，这些 thinking 数值样本全部为 0。31 行有 thinkingDurationMs（80–4039 ms），它不是输出 token 生成时长，更不能当可见正文的流式时长。样本不足以验证非零 thinking 与 output 的包含关系；不得把 reasoning 再加到 output。input/cache 走既有逐源缓存语义归一。

没有 `requestId` 顶层字段（0/695）；可用 sessionId + message.id 作本机去重候选，但它不是已验证的原生 request/attempt 身份。uuid/parentUuid 是会话消息链，不能据此产生 API 开始时刻。模型在 message.model；无可靠逐请求 provider/attempt 全链。provider 的规则推断仍须保留 inferred 标签。

### 4.3 新发现：cost-state 不能直接塞进模型费用行

9 条快照、5 个 sessionId，字段：

| 字段                           | 样本条数 | 数值范围（ms，单位依据字段形态仍应实测复核） |
| ------------------------------ | -------- | -------------------------------------------- |
| totalAPIDuration               | 9        | 15,503–2,349,599                             |
| totalAPIDurationWithoutRetries | 9        | 15,430–2,347,419                             |
| totalToolDuration              | 9        | 0–2,657,024                                  |
| totalDuration                  | 9        | 946,683–297,656,820                          |

这些行**没有 timestamp**，只有 startTime/sessionId。某会话 4 快照的 API/工具累计值非递减，且其中连续 3 快照 API/工具值完全相同而 totalDuration 继续增加，符合累计快照形态；另一会话的 2 快照却出现 2 个 startTime，必须研究恢复进程/计时基线重置。此证据不等于已经证明全会话终身累计或不含并行。

modelUsage 每模型只有 inputTokens/outputTokens/thinkingTokens/cacheReadInputTokens/cacheCreationInputTokens/costUSD/webSearchRequests，**没有每模型 duration**。快照有 1–4 个模型；不能按 token 比例分摊时长。也不能将无 timestamp 的累计快照归入相邻消息小时、文件 mtime 小时。累计值是否含子代理并行、重试退避、连接等待、恢复前时间，均未实测定案。它可用于未来会话总量对账候选，当前不能给任意区间、任意模型行精确回填。

### 4.4 工具、失败与流式边界

329 个带工具的 assistant 行、329 条 sourceToolAssistantUUID 回执，329/329 在窗口内能关联；这只是记录级覆盖，不代表所有内容项覆盖。时间差包含日志落盘与编排，可能包含权限等待。只有消息成品时间，没有请求 start、first token 或 token delta 时间；连续多条相同 message.id 也不能证明第一条就是首 token。

样本见 1 条 isApiErrorMessage；工具失败可看 tool_result.is_error，用户拒绝工具现有判据另记 interrupted。没有完整重试 attempt 时间序列，cost-state 的有/无重试累计差不能替代重试明细。取消、失败没有完成 usage 时，应保留缺失计数，不能算 0 TPS。

### 4.5 未来原生遥测候选（本轮未启用）

[Claude 官方监测文档](https://code.claude.com/docs/en/monitoring-usage) 已定义增强追踪：llm_request 的 duration_ms 含重试、ttft_ms、first_content_ms、request_id/client_request_id/attempt/usage；first_content_ms 表示成功尝试首内容块，非流式回退缺失。工具父 span 含权限等待，tool.execution 子 span 才是执行体。该规范可成为未来前向采集候选，**不等于本机历史 JSONL 已有这些字段**。

## 5. Codex

### 5.1 路径、关联与 token

原始源为 `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`、`~/.codex/archived_sessions/*.jsonl`；本次未读归档内容。session_meta 给会话与 model_provider，turn_context 给 turn_id/model，部分 thread_settings_applied 给模型/供应商。多代理有 parent_thread_id/root_turn_id，但根回合不是计费请求身份。

首轮 116 条 `token_usage_record`；补查 133 条的 payload **键集固定**：`response_id, root_turn_id, session_id, thread_id, thread_token_usage, turn_id, turn_token_usage, usage`。133/133 各身份字段为字符串。**无 model_call_id、duration、ttft、request_start 或 attempt 字段**。response_id 已是有价值的响应关联键，但记录 timestamp 是用量记录时间，不是已证明的生成开始或结束。

首轮 event_msg/token_count 115 条，包含 total_token_usage/last_token_usage；新 token_usage_record 与它们是同一用量的不同投影，不能双计。usage 是请求增量候选；turn_token_usage/thread_token_usage 为累计量，需原生契约验证及去重，不可逐条加总。补查 128 条时有 128/128 满足 total=input+output、output>=reasoning_output；算术与 reasoning 细分形态支持 reasoning 是 output 子集的当前口径，不能再次相加，也不能将 reasoning 当可见文本 token。

### 5.2 真实回合 TTFT 已在本机存在

首轮 14 task_started、10 task_complete，后者 10/10 有 duration_ms（56,832–674,699）与 time_to_first_token_ms（4327–105,823），并带 started_at/completed_at/turn_id/root_turn_id。

[官方 TurnCompleteEvent 源码](https://raw.githubusercontent.com/openai/codex/main/codex-rs/protocol/src/protocol.rs) 明确：duration_ms 是回合起止差，time_to_first_token_ms 是回合开始至第一个模型 token。它只能支持「有此字段的已完成回合 TTFT」，一轮多次请求时，后续请求没有自己的 TTFT；不能取该值除以 requests，不能充当逐请求平均。也不能把回合时间减去工具时长总和作为模型时间：工具可并行，回合还有审批、调度、重试及本地处理。

### 5.3 工具与 item 生命周期的新证据

首轮 item_completed 302 条；补查 336 条中 started_at_ms/completed_at_ms 均为数值（336/336）。补查中 CommandExecution 117/117、McpToolCall 39/39 有原生 duration 对象 `{secs,nanos}`，同时有原生起止；Reasoning 104、AgentMessage 35 有 item 起止但无 duration 对象。不能把 Reasoning/AgentMessage item 开始当成 enclosing model request 开始；其 first output 时刻也未等于首 token。

现采集器读取 response_item function/custom_tool_call 及对应 *_output 的 call_id 配对，并读旧 event_msg/mcp_tool_call_end.duration。新 item_completed 提供另一投影；未来接入必须按调用身份去重，不能把两套工具记录相加。CommandExecution 的持续进程/异步完成、MCP 审批是否计入 item duration，仍需定向实测；工具关联到模型时应记调用发起模型，不能盲用回执时最近模型。

### 5.4 失败、取消、重试与流式

原生事件可有 error、task_complete.error、turn_aborted.reason；现采集器已有 interrupted/error 分类，首轮样本没有覆盖取消/重试场景。response_item 是完整条目或结果，token_count 是记账，不是 SSE token 流；rollout 存在 Reasoning/AgentMessage 不足以证明每条请求使用了流式。样本没有逐请求 attempt 与起止，失败后重试的分母、失败已生成 token 均未闭合。

[Codex 官方遥测说明](https://learn.chatgpt.com/docs/config-file/config-advanced) 列出 api_request、SSE/WebSocket 与回合 TTFT。文档将 SSE/WebSocket 事件 duration 描述为事件处理耗时；**不可把它们逐条求和作为生成时间**。[官方 session_telemetry 源码](https://raw.githubusercontent.com/openai/codex/main/codex-rs/otel/src/events/session_telemetry.rs) 的 sse_event_completed 确实接收 ttft_ms 并在 response.completed 发出 token 与该字段；这一函数未同时列出 response_id/attempt/请求起止。未来需验证它与 transport span、turn_id、response_id、重试的实际关联，再判断能否组成请求计时；仅凭字段名与事件 timestamp 不闭环。

## 6. WorkBuddy

### 6.1 路径与本机现状

`~/.workbuddy/workbuddy.db` 提供会话枚举与 cwd；`~/.workbuddy/projects/<编码项目>/<sessionId>.jsonl` 才是消息与 usage 原始源。模型以 providerData.model 为准，DB sessions.model 不能当每条请求模型。

287 行：64 message、60 reasoning、74 function_call、72 function_call_result、15 file-history-snapshot、2 ai-title。63 条 message.usage：56 在 function_call、7 在 message；只读 assistant message 会漏 56/63。245 行有 providerData 的 model/messageId/traceId/conversationRequestId 等。

### 6.2 request 与 reasoning 的新增证据

- providerData.messageId：245 条、63 个不同值；63 条 usage 的 messageId 也恰为 63 个不同值。它是请求/响应关联候选，必须用实测验证重试或多工具响应是否复用，不能仅按数量认定请求 ID。
- providerData.traceId 与 conversationRequestId：各 245 条、仅 7 个不同值；它们明显是较宽分组，不能当 63 次模型请求的唯一键。顶层 traceId 为 0，不得读错路径。
- providerData.requestModelId/requestModelName/model 可关联模型；供应商没有可靠原生标识，不能从 requestModelName 猜 endpoint/provider。没有原生 turn ID 与 attempt 序列。

message.usage 的 output_tokens 63/63 等于 rawUsage.completion_tokens；total_tokens 63/63 等于 input+output。rawUsage.completion_tokens_details.reasoning_tokens 与 rawUsage.completion_thinking_tokens 在 63/63 相等，60/63 为正，范围 0–9593。**reasoning 不应再加进 output**；这是基于当前 rawUsage 形态与算术的口径判断，仍需供应方语义验证。现采集器 RawUsage.reasoning 固定 0，确实没有吸收这个细分；本轮仅记录缺口。

cache_read_input_tokens 在 message.usage 61/63 在场，rawUsage 同名字段却 63/63 为 0；缓存不可从错误投影读取。rawUsage/prompt_tokens_details.cached_tokens 等另有值；沿用既有 message.usage 口径，不新造 token 估算。

### 6.3 工具、时长与失败

74 function_call、72 result，72/72 result 的 callId 在窗口内匹配，结果行 id 与调用 id 不是同一个身份；result.parentId 另指调用行。工具时间差可以估算，74 次调用中 2 次无结果，缺失不是 0。行 timestamp 是消息事件时间，不是已证明的工具执行入口；权限与队列等待仍可能包含在差值中。

本样本深查对象层无 duration/latency/ttft/first_token/first_content 类数值字段。reasoning 和最终工具/文本成品行不提供首 token。status 有 completed 114 条、incomplete 2 条；providerData.error 对象 2 条，有 isNetworkError/isStreamTimeout/isRetryable 形态；工具 rawResponse 有 is_error 5 条及 exitCode/interrupted 等。它们可辅助分类，不能证明发生了几次实际重试或给出每次耗时。

WorkBuddy 在项目宪法中是无公开 CLI/API/存储规范的封闭源；本轮不承诺存在可配置的原生遥测。优先向官方核实请求计时/导出能力；若仅有成品 JSONL，继续标不可得，不能为追指标更改 API 路由或擅装插件。

## 7. 定向实测计划（未执行）

只对静态数据无法定案的部分做小样本、单变量实验；先约定隔离会话、允许的原生遥测/Hook、输出白名单与临时数据目录。本轮不启用任何一项。

| 场景                                   | 必须闭合的问题                                                                             | 只保留的证据                                                                |
| -------------------------------------- | ------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------- |
| 每源单请求、无工具、短/长输出各一次    | 请求开始、首模型 token、首可见内容、最后 token、usage 记录是哪些不同时间                   | 脱敏 session/request/response/turn/attempt ID、单调计时、token 数、流式标记 |
| 一回合先工具后继续生成                 | 第二次模型调用能否获得独立起止/TTFT；WorkBuddy messageId 是否一请求一值                    | 每请求身份与生命周期，模型实际值                                            |
| 两工具并行，含一个延迟工具             | 工具 duration 是执行体还是含审批；累计是 sum 还是 union；模型与工具是否重叠                | call ID、开始/结束、原生 duration、审批等待                                 |
| 人工延后批准工具                       | 把等待与工具执行分离；Claude tool.execution 与 tool span 差值是否对齐                      | blocked_on_user/执行时间，禁止把人工等待算推理                              |
| 用户中途取消、网络失败后一次重试       | attempts 是否齐全，usage 是否只有最终成功；partial token/重试退避如何留分母                | outcome、attempt、usage 在场性、计时边界                                    |
| 非流式回退/仅 reasoning/仅工具调用输出 | first token 是否先于可见正文，first_content 缺失是否代表非流式                             | 类型标志、首事件时间，不留内容                                              |
| 同 session 恢复进程与切模型            | Claude cost-state 的 startTime/累计重置、跨模型时长能否拆；Codex/WorkBuddy模型身份是否准确 | 累计快照数值、基线 epoch、模型与请求关系                                    |
| 跨小时请求、父子代理并行               | 精确区间归属与父子重复扣除，费用行能否关联同请求                                           | 时间区间、parent ID、originator、去重键                                     |

Claude 优先验证官方增强原生遥测是否覆盖本机版本、并与 message.uuid/request_id 对齐；Hook 可测工具起止，不能凭 Stop/PreToolUse 事件推导全部模型请求 TTFT。Codex 优先验证 response.completed 的 ttft_ms 和请求 span 关联；回合 Hook 只给一轮结束时刻，不能制造多请求计时。WorkBuddy 先核实原生导出能力，再决定实验能否进行。

实测验收应同时报告「已完成请求总数 / 有起止数 / 有首 token 数 / 明确流式数 / 具有一致 token 口径数 / 失败取消数 / 可关联模型数」。只在同一批完整请求上累计 token 与流式时长，TPS 用比值 `Σ输出token / Σ流式秒`，不平均请求 TPS；分子包含 reasoning 与否必须和首输出、结束边界采用相同定义。沿用主设计口径，排除的是首 token 等待时间，不从供应商输出计数机械扣除一个 Token；对短输出保留测量边界说明。请求时长与流式时长缺失时留空，不填 0。

## 8. 可复跑只读探针

下列示例只输出文件数量、字节、mtime、事件类型计数、白名单字段在场数与类型；不会打印会话文本、参数、结果、ID 值或配置。它不运行工具，不调用兔维斯采集器。动态日志会增长，复跑数字变化属正常，需附读取时点。

```python
from pathlib import Path
from collections import Counter
import json

LIMIT = 8 * 1024 * 1024
ROOTS = {
    "claude": Path.home() / ".claude/projects",
    "codex": Path.home() / ".codex/sessions",
    "workbuddy": Path.home() / ".workbuddy/projects",
}
PATHS = (
    "timestamp", "durationMs", "totalAPIDuration", "totalToolDuration",
    "message/usage/output_tokens", "message/id", "thinkingDurationMs",
    "providerData/messageId", "providerData/traceId",
    "providerData/rawUsage/completion_tokens_details/reasoning_tokens",
    "payload/response_id", "payload/turn_id", "payload/duration_ms",
    "payload/time_to_first_token_ms", "payload/started_at_ms",
    "payload/completed_at_ms", "payload/item/duration",
)

def get_path(value, path):
    for key in path.split("/"):
        if not isinstance(value, dict) or key not in value:
            return None
        value = value[key]
    return value

for source, root in ROOTS.items():
    files = sorted(
        [(p.stat().st_mtime, p.stat().st_size, p) for p in root.rglob("*.jsonl")],
        key=lambda item: (item[0], str(item[2])), reverse=True,
    )
    chosen = (files[:5] if source == "codex" else
              list(dict.fromkeys(files[:2] + sorted(files, key=lambda x: x[1], reverse=True)[:3])))
    events, fields = Counter(), Counter()
    print(source, "inventory", len(files), "selected", len(chosen))
    for index, (mtime, size, path) in enumerate(chosen, 1):
        offset = max(0, size - LIMIT)
        with path.open("rb") as handle:
            handle.seek(offset)
            if offset:
                handle.readline()  # 丢掉可能被截断的首行
            lines = handle.read(LIMIT).splitlines()[-20000:]
        valid = invalid = 0
        for line in lines:
            try:
                row = json.loads(line)
            except (ValueError, UnicodeError):
                invalid += 1
                continue
            valid += 1
            payload = row.get("payload")
            subtype = payload.get("type", "") if isinstance(payload, dict) else row.get("subtype", "")
            events[(row.get("type", "?"), subtype)] += 1
            for field in PATHS:
                value = get_path(row, field)
                if value is not None:
                    fields[(field, type(value).__name__)] += 1
        print(index, "bytes", size, "mtime", mtime, "offset", offset,
              "valid", valid, "invalid", invalid,
              "size_changed", path.stat().st_size != size)
    print("events", dict(events))
    print("field_presence", dict(fields))
```

账本 schema 可用以下只读方式核实（不读用户正文）：

```python
import sqlite3
from pathlib import Path

db = Path.home() / ".tuvis/tuvis.db"
with sqlite3.connect(f"file:{db}?mode=ro", uri=True) as conn:
    conn.execute("PRAGMA query_only=ON")
    print([(row[1], row[2]) for row in conn.execute("PRAGMA table_info(usage_detail)")])
    print(list(conn.execute("""
        SELECT source_id, COUNT(*), SUM(requests), SUM(tool_calls), SUM(tool_ms)
        FROM usage_detail WHERE source_id IN ('claude','codex','workbuddy')
        GROUP BY source_id
    """)))
```
