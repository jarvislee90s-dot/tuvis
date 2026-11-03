# DSH 与 Kimi Code 四项速度指标静态探测

日期：2026-10-24。工作方式：真实持久文件只读；没有运行 CLI、发请求、改配置、启停工具、写账本或修改生产代码。本报告是设计证据与后续实测计划，不能当成实测验收。

## 结论

- **DSH：四项均有原始数据基础，但原生 sessionStats 不能直接满足按区间、模型查询。** 最近 6 个代际文件中 465 条成功结算消息全部含原始流时间、输出用量、模型身份和对应 step/start。应从事件恢复逐 step 样本，缓存只用于会话总量核对。重试后的首 token 选择与缓冲流的 TPS 可信度需要额外标记。
- **Kimi：四项均有静态可用基础。不能因顶层 usage.record 缺 turnId 而判为不可得。** 嵌套 step.end 自身同时持有 usage、llmFirstTokenLatencyMs、llmStreamDurationMs；2402 条结束事件中 2391 条具备这三者。无需靠相邻 usage.record 猜测输出 token。工具调用可按文件内 toolCallId 全量配对。模型归属仍须分层：有 llm.request 的新形态可直接关联，旧形态只能采用显式继承或标记为未知，不能把“本轮首见模型”当成性能样本的精确模型。

## 边界与既有实现

已读根 AGENTS.md、docs/MASTER-PLAN.md 的分期、只读内容接入及 D14。此轮仅静态研究，不扩权、不改宪法。既有入口：

- DSH：src-tauri/src/monitor/dsh/{mod,log,decode,projcache}.rs 与 services/usage/collectors/dsh.rs。根为 DSH_HOME 覆盖，否则 ~/.dsh；投影缓存为 storages/session_projcache/sessions/*.json，日志为 sessions/<项目>/<会话>/session[.vN].jsonl[.zstd]。
- Kimi：monitor/kimi_parser.rs、services/usage/collectors/kimi.rs。KIMI_CODE_HOME 显式覆盖，否则优先 ~/.kimi-code，再回退 ~/.kimi。通过同根 session_index.jsonl 定位，必须包含 agents/*/wire.jsonl，不能只读 main。
- 现有 DSH 增量账本有投影水位/历史补录纪律；新增性能指标应使用独立语义版本与水位，不复用旧 token 水位，不拿会话累计差值伪造任意历史区间。

## 样本分母与读取预算

| 源           | 文件枚举                             | 实际内容读取                                                                                                     | 覆盖与限制                                                                             |
| ------------ | ------------------------------------ | ---------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------- |
| DSH 原始日志 | 409 个候选、267875282 压缩/原始字节  | 按 stat.mtime 降序最近 6 个文件，3327694 压缩字节，11544850 解压字节，3023 JSON 记录；系统 zstd 六次退出码均为 0 | 不是 6 个独立会话的保证：代际文件可能同属会话；未全量解压，不能推断 409 文件全量覆盖率 |
| DSH 投影缓存 | 399 个 JSON                          | 逐份小缓存读字段，不解压历史                                                                                     | 399/399 均有 sessionStats.val 的六个计时/分母字段                                      |
| Kimi         | 全部 82 个 wire.jsonl，19564260 字节 | 全部 82 个有界文件结构扫描，42 个 main、40 个其他 agent                                                          | 此本机文件集全量；历史事件时间为 UTC 2026-06-23 至 2026-09-20，mtime 较新不代表事件新  |

DSH 六文件事件时间范围为 UTC 2026-10-20 16:57:16.878 至 2026-10-23 15:20:39.163。只输出数字、字段名、类型与脱敏形态，没有输出正文、参数值、端点或密钥。

## DSH：字段、关联与公式

### 原始记录形态

- session 头：id、createdAt、cwd、delegationDepth；会话身份来自文件头，不从路径文本猜测。
- step/start：time、data.turn、data.step；step/end 同键。6 文件共有 469/469 个 start/end。
- assistant/message：time、data.turn/step、data.message.source.{provider,model}、data.usage.outputTokens、data.stream。465/465 都有 source.model、outputTokens、非空 stream，且 465/465 可与同文件 step/start 配对。
- request/header：data.header.config.{provider,model}；request/context：data.{provider,model,contextWindow}；model/selection：data.{provider,model}。它们是配置/上下文快照，不是每次请求都有一条：样本分别仅 6、5、4 条。优先采用成功消息的 source，其次沿事件序列继承已确认配置，缺失保留未知。
- assistant/attempt：同 turn/step，表示无表面消息的尝试结算；样本 4 条，3 条非空 stream，3 个 finish.kind=error，1 条空 stream。**没有稳定 requestId/attemptId 可直接做跨来源关联**，需用 session+代际序列+step+尝试序号形成内部键。
- tool/call：data.callId、name、turn/step；tool/result：data.message.source.callId。样本 559/559。并行调用累计用时可相加但会超过回合墙钟；它是调用等待墙钟，可能包含审批、调度、异步结果等待，不应标为 CPU 执行时间。job/registered、job/settled 各 62 条，另有 startedAt/finishedAt 可用于后续分离后台任务，但不能与 call/result 重复记账。

### 流时间确实持久保存，不能只读 stream[].time

流有四种记录：

| stream 记录      | 时间与载荷                                                        |
| ---------------- | ----------------------------------------------------------------- |
| chunk            | time、chunk.type；样本 2972 条，含 block-start/end、usage、finish |
| reasoning-chunks | time0、index、dt[]、texts[]；355 段                               |
| text-chunks      | time0、index、dt[]、texts[]；104 段                               |
| tool-call-chunks | time0、index、dt[]、id、name、args[]；559 段                      |

全部 1018 个紧凑段满足 dt 长度=成员数−1；共有 217226 个间隔，105361 个为零，负数 0，最大 3620ms。原始 chunk 边界与时间不是被扔掉，而是紧凑存储：第一成员 time0，随后按 dt 逐个累加。统计首 token 时必须判断非空 delta，不能将 block-start 或 usage 当作 token；工具参数与 reasoning 是否参与首 token，必须与采用的口径一同记录。[上游流协议](https://github.com/deepseek-ai/deepseek-harness/blob/master/docs/subsystems/llm-streaming.md)说明这种压缩保留原始成员与时间。

本机按“非空紧凑段第一成员”做结构验证：465/465 消息均有候选，首候选距 start 为 627–127010ms，结算距首候选为 45–123541ms，非正解码时长 0。该扫描未逐成员校验首成员为空的情形，因此这些范围只用于证明数据在场，不作为最终指标值。原始 chunk 邻接出现大量同毫秒记录，且最后 chunk 到结算为 0–11ms，说明必须保留缓冲/突发警示，不能把记录时间直接声称为服务端真实解码节奏。

### 四指标可得性

| 指标              | 逐样本方案                                                                        | 约束                                                                  |
| ----------------- | --------------------------------------------------------------------------------- | --------------------------------------------------------------------- |
| 模型累计用时      | 对成功组装消息，message.time − step/start.time                                    | 不含后续工具，但会含 step 内重试/退避/请求构建；不能当纯 GPU 时间     |
| 工具累计用时      | result.time − call.time，按 callId 配对                                           | 未闭合调用不填 0；并行累计相加                                        |
| 平均首 token 等待 | sum(first_nonempty_delta.time − start.time)/有效首 token 样本数                   | 逻辑 step 与网络请求/尝试不等价；跨重试不能只看最终成功 stream        |
| 流式 TPS          | sum(与有效流窗口对应的 outputTokens) ×1000 / sum(message.time − first_token.time) | 分子分母同批；窗口非正、非流式/整块突发需排除并统计；不可平均逐条 TPS |

399/399 缓存提供 llmMs、toolMs、ttftMs、ttftSteps、decodeMs、decodeTokens，但没有按时间和逐模型拆分。缓存只做会话核对，不能按当前模型拆整段历史。[原生投影](https://raw.githubusercontent.com/deepseek-ai/deepseek-harness/master/packages/session/session-stats/src/projection.ts)以 step 开始到消息结算计模型用时，跨 attempt 保留首次 token；取消而未组装消息的步骤不进入计时。这也说明它的 TTFT 分母是成功结算的逻辑步骤，并非每次网络尝试。该原生统计可作为兼容口径，不能未经裁决直接当作用户“每次请求”的严格定义。

### output/reasoning 与回填

465 条 data.usage 均无独立 reasoning 字段，不能把流文本长度换算成 reasoning token 后与 output 相加。保持供应商 outputTokens 的原生含义，标为总输出；只有能证实与流式时间覆盖同一内容的样本才进 TPS。思考以整块回传、隐藏思考不增量下发，会让总 output 除以可见流时长偏高，静态无法排除该问题。

有原始 start、settlement、stream、usage 的历史可逐事件回填；仅投影缓存的历史最多展示会话总量，区间/模型性能指标为空。代际恢复先确定版本链与事件 seq，避免将同会话历史拷贝重复累计。失败/取消保留质量计数与缺口，不能自动补出用时或 0 TPS。

## Kimi：字段、关联与公式

### 关键证据：step.end 自带用量

全部 82 文件：嵌套 context.append_loop_event 共 12317 条，分别为 tool.call 2574、tool.result 2574、step.begin 2437、step.end 2402、content.part 2330。step.end 位于顶层 event，具有：

```text
context.append_loop_event.time                         毫秒事件时间
context.append_loop_event.event.type                  step.end
context.append_loop_event.event.{uuid,turnId,step}      唯一步骤身份（turnId 字符串）
context.append_loop_event.event.usage                  inputOther/output/inputCacheRead/inputCacheCreation
context.append_loop_event.event.llmFirstTokenLatencyMs 首 token 等待
context.append_loop_event.event.llmStreamDurationMs    首流片段至流结束
```

2402 个结束步骤中：2391 个 usage 非空（99.54%）；同样 2391 个同时有两个原生时长；TTFT 全为正（540–77643ms），streamDuration 2389 个为正、2 个为 0（范围 0–343577ms）。finishReason 分布为 tool_use 2181、end_turn 210、error 11。849 个还提供 llmRequestBuildMs、llmServerFirstTokenMs、llmServerDecodeMs、llmClientConsumeMs，6 个提供 llmClientBlockedMs；这些是新形态可选字段，不能强制要求历史全有。

顶层 usage.record 2395 条全部没有 turnId/requestId/stepId，只有 10 条有 agentId。验证“step.end 后的下一条 usage.record”：2383 对可见，2381 对 usage 四桶相同，2377 对时间差 0–1ms。存在未配对和不一致，说明按相邻记录关联不应作为权威路径。**直接用 step.end.usage.output 和同一 event 内时长即可唯一配对**；不要把 usage.record 的 output 再累加一遍。

### 时长含义的本机代码佐证

只读检索本机 ~/.kimi-code/bin/kimi 的内嵌 JavaScript（未执行二进制）。文件 188155456 字节，SHA256 为 `4bab6f96c2c289368b7f05a04f66737c53a59ce740f932d8f149240584758efb`。可定位函数 buildStreamTiming：

```text
firstTokenLatencyMs = max(0, firstChunkAt - requestStartedAt)
streamDurationMs = max(0, outputEndedAt - firstChunkAt)
```

firstChunkAt 在首次 llm.streaming.part 到达时设置；stepEnd 投影把 step.timing 对应字段写入 event。故 streamDuration 已排除首 token 等待，不能再减 TTFT；模型时长可取两者之和，不包括后续工具执行。它是客户端观察到的流耗时，包含本地消费和缓冲，不等于纯服务端解码时间。注意首次 part 的类型/内容是否必为非空 token 未逐 provider 验证；字段名相同不保证所有历史版本实现一致。

还观察到 llm.sent 再次发生会重置 requestStartedAt、消息 accumulator、usage，但片段中没有同步重置 firstChunkAt 的明确证据；所以重试时长是否跨 attempt、是否可能保留旧首片段，必须定向实测，不能凭正数就认证为每次请求。

### 身份、模型与工具

- 关联键：session（索引/目录身份）+ agent 文件身份 + event.turnId + event.step，uuid 是 step 自身身份；content.part/tool.call 带 stepUuid，可进一步核对。
- 顶层 llm.request 共 811 条，全部有 provider、model、turnStep，无 requestId；turnStep 脱敏形态为 `#.#`，对应 turnId.step。按同文件、前序最近同 turnStep 关联，661/2402 个 step.end 有 request，650/2391 个带计时结束步骤有 request。重试可能同键多条 request，需保留尝试序号，不能简单 map 覆盖丢掉尝试。
- 老事件可从同文件 usage.record.model 继承，但首次模型出现前的事件需等待并检验；跨模型会话必须在事件时间线上归属，不能把末次模型或其他文件首见模型补给历史。模型缺失样本可进入工具级总览，但模型级应进未知桶并标质量。
- 工具 call/result 在每文件按 toolCallId 配对，2574/2574（100%），时间差负数 0、零值 119；result 只有 parentUuid/toolCallId/result，没有 turnId/step，应继承配对 call 的身份和模型。累计时间为顶层 time 差，同样可能包含调度与等待，后台任务实际工作结束不一定等于结果返回。
- content.part：1676 个 think、654 个 text，只有已拼装 part 与事件时间，没有保存逐 token chunk 到达序列。因此无法用 content.part 重建精确首 token，应该使用 step.end 的原生 timing。不要把 content.part.time 当成 stream 的第一个 token 时间。

### 指标与回填

| 指标              | 方案                                                  | 本机静态覆盖                                                                |
| ----------------- | ----------------------------------------------------- | --------------------------------------------------------------------------- |
| 模型累计用时      | sum(TTFT + streamDuration)，同一 event 的完整步骤     | 2391/2402 个结束步骤；35 个 begin 未闭合，不填 0                            |
| 工具累计用时      | sum(result.time − call.time)                          | 2574/2574 可配对，负数 0、零值 119；仍需防重复调用键                        |
| 平均首 token 等待 | sum(llmFirstTokenLatencyMs)/有效步骤数                | 2391 个；对“请求”是否包含重试需实测裁决                                     |
| 流式 TPS          | sum(event.usage.output)×1000/sum(llmStreamDurationMs) | 最多 2389 个正分母样本；非流式/整块、隐藏思考、重试语义验证前标为客户端流速 |

usage.output 为唯一原生输出字段，未见独立 reasoning token，不能按 think 文本另加估算。原生 output 是否包含全部思考 token、每个 provider 是否一致，现有记录不足以确认，需向定向实测提供 provider 输出用量对照。回填可覆盖留有原生 timing 的历史，缺 timing 的 error/取消/旧步骤不从回合墙钟推造；模型绑定明确的只占部分，计时覆盖与模型绑定覆盖必须分开申报。

## 入库、查询与后续实测门槛

建议逐 step/调用保存可加分子分母与质量状态，再折叠现有区间/工具/模型维度：model_ms、tool_ms、ttft_ms_sum、ttft_count、decode_ms_sum、decode_output_tokens、timing_count、request_count、unknown_model_count、failed/cancelled/zero_window/buffered_count。缺数据为 NULL/不参与分母。以完成事件时间归桶并保留开始时间；跨区间事件是按完成归属还是裁切需主设计统一，不能按日志 mtime。

本轮未启动实测。下一轮每源最少覆盖：普通短答、长流式输出、含思考、工具调用/并行工具、同回合连续 step、显式换模型、首 token 前取消、流中取消、一次重试后成功、重试耗尽、流缓冲/非流式整块、后台任务。记录前后文件大小/mtime、仅新追加部分和屏幕计时，对齐请求发送、首非空 token、流结束、工具调用/结果。验收重点：

1. DSH 验证原始紧凑段展开与首非空 delta；跨 attempt 是否按逻辑 step 聚合；隐藏思考与突发缓冲能否识别；取消与工具等待边界。
2. Kimi 验证 TTFT 的首 part 是否非空 token；内部 retry 与 step retry 的字段重置；output 含思考口径；时长是否混入客户端阻塞；新 request 模型关联与旧 schema 共存。
3. 没有 attempt 精确开始/结束与用量的失败样本，不能纳入严格“每次网络请求”的 TTFT/TPS；保留部分可用状态并展示分母。

## 可复跑的只读方法

本次用系统 Python 标准库作结构扫描，用已有 /opt/homebrew/bin/zstd 解码，未安装依赖。复跑顺序固定为 stat 枚举 → 限定文件数/大小 → 解码/JSON 解析 → 仅汇总字段。以下代码仅打印计数，不输出正文与路径身份：

```python
from pathlib import Path
from collections import Counter
import json, subprocess

root = Path.home()
files = sorted((root/'.dsh/sessions').rglob('session*.jsonl*'),
               key=lambda p: p.stat().st_mtime, reverse=True)[:6]
assert sum(p.stat().st_size for p in files) < 10_000_000
counts = Counter()
for p in files:
    if p.suffix == '.zstd':
        r = subprocess.run(['zstd', '-dc', str(p)], capture_output=True, check=True)
        raw = r.stdout
    else:
        raw = p.read_bytes()
    assert len(raw) < 50_000_000
    for line in raw.splitlines():
        v = json.loads(line)
        counts[v['type']] += 1
print('dsh_files', len(files), 'types', counts)

files = list((root/'.kimi-code/sessions').rglob('wire.jsonl'))
assert sum(p.stat().st_size for p in files) < 30_000_000
counts = Counter()
for p in files:
    for line in p.open():
        v = json.loads(line)
        if v['type'] == 'context.append_loop_event':
            e = v['event']
            counts[e['type']] += 1
            if e['type'] == 'step.end':
                for key in ('usage', 'llmFirstTokenLatencyMs', 'llmStreamDurationMs'):
                    counts['has_' + key] += key in e
print('kimi_files', len(files), 'counts', counts)
```

若未来文件集超过预算，应缩小样本或分批，不调高预算后全量读取。DSH 活跃尾帧可能残缺：本次六文件均完整解码；生产适配必须沿用既有 decode_zstd_frames 的分帧容错并记录 torn_frames，不能悄悄把损坏前缀当全量历史。
