# ZCode / OpenCode 四项耗时指标：本机 SQLite 只读取证

取证日期：2026-10-24（Asia/Shanghai）。本报告是设计研究，不变更采集器、数据库、配置或工具运行状态。

## 结论与边界

- **ZCode 原生请求和工具账本能提供最接近目标的历史数据**。本次快照 16,123 个模型记录中，16,061 个 completed 且 output>0；其中 10,893 个同时具备 TTFT 与正生成窗口，覆盖率 **67.82%**。不能用先前 10,854/15,981 的旧比例作固定能力声明。
- **OpenCode 能回填工具区间；模型消息区间只能作为代理耗时**。v2 的 60 条 assistant 中仅 1 条有 `time.streamed`，且 streamed=completed，正生成窗口为 0；真实历史 TTFT/TPS 不可补造。
- 两工具 token 语义不同：ZCode 的 output 在本库算式中包含 reasoning；OpenCode 的 output 与 reasoning 分列相加。必须记录来源与分子语义，不能统一强减或强加 reasoning。
- 本次遵守 `docs/MASTER-PLAN.md` §5.(c) 的“能力差异显式化”“桌面体验不回退”“版本漂移受控”；只读探测未发现宪法冲突。日后遥测/轻量 Hook 只能作为可选补充，本研究不要求改 API 路由、安装插件或扩权。

## 路径、读取方式与工作量

路径来自实际生产代码，不从夹具猜测：

| 工具     | 发现代码                                                                               | 本机源                                | 本次大小            |
| -------- | -------------------------------------------------------------------------------------- | ------------------------------------- | ------------------- |
| ZCode    | `monitor/zcode_parser.rs::ZcodeRoots::from_home`；`services/usage/collectors/zcode.rs` | `~/.zcode/cli/db/db.sqlite`           | 1,214,111,744 bytes |
| OpenCode | `services/usage/collectors/opencode.rs` 与 `monitor/opencode_parser.rs`                | `~/.local/share/opencode/opencode.db` | 13,418,496 bytes    |

ZCode 的 `~/.zcode/v2/tasks-index.sqlite` 是任务索引；本次四指标以 CLI 主库为证据，未读任务正文。OpenCode 存在 `-wal`（当时 0 bytes）与 `-shm`。每次连接均使用 Python 自带 sqlite3：`file:绝对路径?mode=ro`、`PRAGMA query_only=ON`、`BEGIN` 保持单连接快照，**未使用 immutable**、未 checkpoint、未复制可能漏 WAL 的裸主库、未调用兔维斯生产采集入口。多次查询为相邻快照，后台工具仍可能写入，表中的分母对应各次实际读到的数据。

读取真实 schema 及全部用量表聚合；ZCode 三张 usage 表约 5.6 万行，JSON 深查仅其 `raw_usage_json` 的键/数字，不读取提示词、回答、工具输入和错误原文。`message` 56,852 行、`part` 220,892 行仅计数，不做全库正文解析。OpenCode 库较小，全量检查 60 个 v2 assistant / 56 个 v1 assistant 的 JSON **键与计时/token数值**，及 28 个工具计时，不输出正文。未扫描工具日志，故日志可能另有遥测不能被本报告排除。

## ZCode：真实 schema 与覆盖

### 粒度和关联

- `model_usage`：主键 `id`；`logical_request_id`、`attempt_index`（默认 0）、`session_id`、可空 `turn_id`、`trace_id`、`span_id`、`assistant_message_id`、`parent_user_message_id`。供应商/模型直接来自 `provider_id` / `model_id`，另有 variant、agent、mode、task_type、query_source。
- `turn_usage`：复合主键 `(session_id,turn_id)`；有 user_message_id、first_model_start_at、model_request_count、model_retry_count、tool_call_count 等。回合时长涵盖整轮，不是模型累计用时。
- `tool_usage`：主键 id；session_id、turn_id、trace_id、tool_call_id、tool_name；**没有直接的 model_usage.id / model_id / provider_id**。一个回合多模型时，不能照当前采集器的会话模型兜底做精确归因；应标工具所属会话/回合，模型维度归属不确定。
- `session` 本次 893 行，116 父 / 777 子，version 范围 0.16.3–0.17.0。模型用量涉及 234 个 session。父子请求在累计 token/耗时时各算一次；不要再把父回合总时长叠上子请求。
- 模型 16,123/16,123 匹配 session；15,922/16,123（98.75%）匹配 turn，虽然 16,113 行 turn_id 非空，仍有孤立关联；16,077 有 assistant_message_id 且能匹配 message，16,105 有 parent_user_message_id。工具 18,878/19,084（98.92%）匹配 turn。缺失连接不能通过内连静默丢掉样本。

### 计时字段与质量

| 表          | 起点/首事件/终点                                                  | 原生耗时                             | 记录数与终态覆盖                                                          |
| ----------- | ----------------------------------------------------------------- | ------------------------------------ | ------------------------------------------------------------------------- |
| model_usage | started_at / first_token_at / completed_at                        | duration_ms、time_to_first_token_ms  | 16,123，全量终态且 duration非负；completed16,061 / cancelled38 / error24  |
| turn_usage  | started_at / first_model_start_at / first_token_at / completed_at | duration_ms、time_to_first_token_ms  | 630，全量终态；completed569 / cancelled49 / error12                       |
| tool_usage  | started_at / first_output_at / completed_at                       | duration_ms、time_to_first_output_ms | 19,084；completed18,760 / error322 / running2；终态19,082均有非负duration |

单位为 epoch 毫秒/毫秒。模型 started_at 覆盖 UTC 2026-09-24 15:15:54.861 至 2026-10-23 16:09:21.065；不能据此认定更早会话没有用量，旧消息可能早于 usage 埋点。

- 16,123 个模型和 630 个回合的原生 duration 全都等于 completed-started。
- 模型 first_token 与 TTFT 都存在的 10,928/16,123（67.78%），每条 TTFT=first_token-started，且 completed>first_token。completed正output的分母为16,061，有效10,893（67.82%）；其余5,168不能计算流式TPS。cancelled的24条、error的11条也有first_token，不能混进成功请求平均值后隐藏状态差异。
- 缺TTFT并非只有辅助请求：main_turn 5,127/7,553 有TTFT，subagent 5,801/8,524；compact10、goal_summary_title3、session_title28、target_completion_verification5 共46条全部缺TTFT。产品若只统计主任务，必须明确排除范围并重算分母。
- 分模型的成功正output可计算流式窗：GLM-5.3-Flash主账户5,832/9,283，GLM-5.3为3,772/5,238，deepseek-v4.1-flash为896/998，minimax-m3.1-flash为254/363。另一个账户的GLM-5.3-Flash为69/102，deepseek-v4-flash为70/76，小写glm-5.3-flash为0/1。账户/供应商ID未在报告公开；不能合并后假设各模型覆盖一致。
- **工具原生duration与墙钟不同**：19,082条终态中19,079条duration短于completed-started，3条相等；差值范围[-300,023,0]ms，均值-463.65ms。静态数据只能确认两字段不等，不能证明差值全是审批等待、调度或其他何种阶段。应保存两者与差异，优先声明“原生工具耗时”，不得假称墙钟差完全等于执行CPU时间。

脱敏例子（删除ID、正文、参数，只保留相对毫秒）：模型 A 在t=0开始、t=6,259首token、t=7,757完成，output96/reasoning0，生成窗1,498ms，对应64.09 output token/s；模型 B 为0/22,540/28,601，output327，生成窗6,061ms。工具例子：墙钟139ms而原生duration133ms；另一个墙钟87,197ms而原生duration87,186ms。

### token、重试、取消、并行与流式证据

- 全部16,123条满足 `computed_total_tokens=input_tokens+output_tokens`。raw_usage_json有16,061条，全部 outputTokens=output_tokens；reasoningTokens键只在1,438条存在，与列相等的也是1,438条，缺键不能当缺失列；306条reasoning>0，没有reasoning>output的行。
- 脱敏原生例子：input21,112 / output320 / reasoning195 / computed_total21,432，raw output320 / raw reasoning195。**确认**本地账本输出与reasoning不是互斥总和；**推断**reasoning是output子集，与当前采集器策略一致。尚不能证明每个供应商上游协议含义及“首token”是否包括thinking/tool-call增量。
- 本次logical_request_id均唯一（16,123个），attempt_index全0，却有58条retry_count>0。字段存在并不等于所有内部尝试逐条留档；不能声称已获得每次失败尝试的起止，也不能从retry_count乘均值补时。retryable、cancelled_by_user、context_exceeded等标志存在；38条cancelled_by_user恰属38条模型cancelled。
- 同session严格时间相交取证：2,531条模型记录与至少一条工具区间重叠；4,654条工具与另一工具重叠；15条模型与另一模型重叠（仅两端齐全的严格 `<` 区间）。这些是“参与重叠的行数”，不是重叠对数/时间；没有自动跨父子会话相加。足以否定“整轮墙钟=串行模型+串行工具”的假设，但不证明每次相交都是实际同时执行。
- first_token_at 是流式起点候选，缺每chunk/最后token事件；completed可能含收尾与usage解析。现有窗计算只能称“原生首事件至请求完成的吞吐”，真实纯生成TPS仍需验证。

## OpenCode：两代共存与现存缺口

### schema选择与规模

现有collector主要读 `session` / `session_v2` 的累计tokens_input/output/reasoning/cache_read/cache_write，模型从会话model及消息辅助识别，消息用于请求/错误/回合等计数。该聚合粒度没有请求TTFT或模型累计耗时，必须增加独立的逐请求证据采集后才能做四指标。

本机 `session`10行、`session_v2`11行；v1 message87行、v2 session_message104行；part196行；event0行。10/10旧session、87/87旧message的ID都存在v2。**只读v2，不合并两代累计**。v2 session版本1.18.21–2.0.22，11个会话parent_id均空。v2 assistant60、user35、idle4、model-switched3、system2；assistant时间范围 UTC2026-08-24 15:23:10.971–2026-10-02 10:13:46.856，不能外推后续版本行为。

### JSON形态与计时覆盖

| 代际                            | 关联与模型                                                                     | 模型计时                               | 工具计时                                                         |
| ------------------------------- | ------------------------------------------------------------------------------ | -------------------------------------- | ---------------------------------------------------------------- |
| v1 `message.data` + `part.data` | message.id/session_id，assistant.parentID，modelID/providerID；part.message_id | `time.created/completed`               | part的`state.time.start/end`形态由解析代码支持；本次权威用v2投影 |
| v2 `session_message`            | id/session_id/type/seq；`data.model.id/providerID/variant`，content[]          | `data.time.created/streamed/completed` | `content[type=tool].id/name/state.status/time.created/completed` |

- v1 assistant56/56 JSON有效、有time和tokens；54/56有completed。v2 assistant60/60有效且有time和model，58/60（96.67%）有正消息时长，57/60（95%）有tokens，38/60 output>0。
- v2 time.streamed仅1/60（1.67%），该条created=0相对起点、streamed=completed=1,875ms，output56 / reasoning79；可生成正TPS窗为0条。`streamed`到底表示首chunk还是其他生命周期时刻，单条静态记录也无法定案。
- content共24 reasoning、26 text、28 tool。24 reasoning和28 tool都有created/completed；26 text未见time。**不能拿reasoning开始或工具开始当first-token来填另外59条缺口**。
- 28/28工具有正time区间，state.status全completed；28/28区间与所在assistant消息区间相交，其中3条与同session另一工具区间相交。消息时长涵盖工具活动已获直接证据，“消息completed-created”不能直接称不含工具的模型用时。
- v2无原生turn_id / logical_request_id / attempt_index列，assistant顶层未见同类键。seq/user可推导用户回合，但system/注入/重试语义仍需确认；工具依所属message直接关联，模型/供应商可用该message.model，不能用session当前model覆盖历史切换。
- v2 assistant finish=stop12 / tool-calls26 / error3 / 缺失19；error.type=aborted5 / provider.auth3 / provider.error11。静态消息没有完整尝试事件，不能把一条assistant等同严格一次网络请求，也不能根据错误消息重建隐藏重试。event表0行，无事件流可补齐。

### output/reasoning语义

v2 23/60条reasoning>0，16/60 reasoning>output；例子 output42/reasoning52、output87/reasoning130、output128/reasoning476，直接证明不能统一用output-reasoning计算可见token。v1具有tokens.total的37条全部满足 `total=input+output+reasoning+cache.read+cache.write`，没有一条满足total=input+output。确认本机两代迁移源的output/reasoning是独立计数；但不是每条v2有total，自身provider自报口径仍应标来源与版本。若流式时间覆盖思考+文本且分子仅output，名称必须声明为output吞吐；若使用output+reasoning，应明确为总生成token吞吐。

## 四项可得性与设计建议

| 目标                       | ZCode                                                                                                                                        | OpenCode                                                                                                                      |
| -------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------- |
| 模型累计用时，不含工具执行 | 可回填逐模型请求duration，全量终态覆盖；这是请求生命周期墙钟候选，是否包含内部工具调度、退避需实测。模型与工具可并行，不从turn总时长直接相减 | 只能回填消息区间并减同消息工具区间并集得“非工具消息活动时间代理”；包含客户端编排/等待且工具窗可能不完整，不能标为精确模型耗时 |
| 工具累计用时               | 19,082/19,084有原生duration，终态完整；保留原生duration与墙钟差，running待结算                                                               | 28/28有原生工具起止，可回填墙钟执行区间，未证实是否含审批/排队                                                                |
| 每请求平均首token等待      | completed正output中10,893/16,061有候选TTFT；报告有效n与全部n；平均按请求数而非token加权，状态/主子任务分开                                   | 仅1条streamed候选，语义未证实；不宣称可用平均TTFT，更不把总消息时间当TTFT                                                     |
| 排除等待后的流式TPS        | 同10,893条可算output/(completed-first_token)；并列声明output含reasoning与末端收尾误差，待实测后决定是否正式发布                              | 0条正生成窗，历史不可回填；未来需要原生请求起点/首chunk/末token或finish时间及token数                                          |

模型/工具“累计”先采用逐事件duration之和（并行会相加），与“经过时间”区间并集分开；若要回答占用墙钟另做并集，不宣称二者相加等于回合总时长。聚合TPS建议 `Σ有效output / Σ有效生成秒`，显示有效请求n和分母；不能混用不同集合的token总数与时长，也不能把逐请求TPS的算术均值当整体吞吐。TTFT按可观测请求均值显示；逻辑请求端到首token（含失败重试/退避）是另一个指标，当前证据不支持全量。

历史回填走独立版本化指标账本与原生ID去重，不复用旧token采集游标“0”语义。ZCode running行原地UPDATE，不能单靠rowid推进跳过未终态；OpenCode session_message内容也可UPDATE，应保留未完成记录重读。四指标字段缺失显示不可用/覆盖率，禁止补0。只存时间、计数、来源ID、模型供应商和证据质量，不增存提示词、答案、工具参数或密钥。

## 日后必须定向实测的题目

1. ZCode first_token_at 是首文本、首reasoning、首tool-call还是任意流事件；无文本/仅工具/非流式请求何时填充，缺TTFT是否与provider或版本相关。
2. ZCode model duration是否包含SDK内部重试、退避、连接等待/收尾，retry_count>0为何attempt_index始终0；同逻辑请求是否可产生多行。
3. ZCode工具duration比墙钟少的阶段是什么；审批等待、后台派发、取消、超时、并行子代理分别采用哪个边界，是否单调时钟与墙钟混用。
4. OpenCode time.streamed的准确设置事件，reasoning/text/tool流事件与message.complete的关系；新请求是否有可选原生telemetry事件可只读采集，缺时再评估轻量Hook。
5. 两工具供应商output/reasoning是否在模型/供应商升级后改变；首事件到最后token的时间窗能否与总生成token匹配，取消时partial usage是否保留。

这些实验尚未执行，不能把静态缺字段解释成工具永久不支持；本轮不为取证发送请求、切模型、装插件、启停工具或写生产配置。

## 可复跑的只读 SQL

连接示例（本地运行，绝对路径由上表展开）：

```python
import sqlite3
from pathlib import Path
p = Path.home() / '.zcode/cli/db/db.sqlite'  # OpenCode 改为对应路径
c = sqlite3.connect(p.as_uri() + '?mode=ro', uri=True)
c.execute('PRAGMA query_only=ON')
c.execute('BEGIN')
# 执行下列只读SQL；结束后c.close()。不添加immutable，不忽略WAL。
```

```sql
-- 两库：只看schema，无账号/正文数据
SELECT name,sql FROM sqlite_master WHERE type='table';
-- ZCode：成功分母、TTFT、正生成窗口
SELECT count(*) all_n,
 sum(status='completed' AND output_tokens>0) completed_output_n,
 sum(status='completed' AND output_tokens>0
     AND time_to_first_token_ms IS NOT NULL
     AND completed_at>first_token_at) eligible_n,
 sum(time_to_first_token_ms<>first_token_at-started_at) ttft_mismatch,
 sum(duration_ms<>completed_at-started_at) duration_mismatch,
 sum(reasoning_tokens>0) reasoning_n,
 sum(computed_total_tokens=input_tokens+output_tokens) total_equal_n
FROM model_usage;
SELECT status,count(*),sum(duration_ms IS NOT NULL),sum(completed_at IS NOT NULL)
FROM tool_usage GROUP BY status;
SELECT count(*),min(duration_ms-(completed_at-started_at)),
 max(duration_ms-(completed_at-started_at)),
 sum(duration_ms<completed_at-started_at)
FROM tool_usage WHERE duration_ms IS NOT NULL;
SELECT count(*) logical_n,sum(n>1) multi_row_logical,max(n)
FROM (SELECT logical_request_id,count(*) n FROM model_usage GROUP BY 1);
SELECT count(*) n,sum(t.turn_id IS NOT NULL) matched_n
FROM model_usage m LEFT JOIN turn_usage t
 ON t.session_id=m.session_id AND t.turn_id=m.turn_id;
-- OpenCode：schema选择和去重证据
SELECT count(*) old_n,sum(EXISTS(SELECT 1 FROM session_v2 v WHERE v.id=s.id)) migrated_n
FROM session s;
SELECT count(*) old_n,sum(EXISTS(SELECT 1 FROM session_message v WHERE v.id=s.id)) migrated_n
FROM message s;
SELECT count(*) assistant_n,
 sum(json_extract(data,'$.time.completed')>json_extract(data,'$.time.created')) duration_n,
 sum(json_type(data,'$.tokens') IS NOT NULL) token_n,
 sum(json_extract(data,'$.time.streamed') IS NOT NULL) streamed_n,
 sum(json_extract(data,'$.tokens.output')>0
     AND json_extract(data,'$.time.completed')>json_extract(data,'$.time.streamed')) tps_n
FROM session_message WHERE type='assistant';
-- 工具只返回类型、状态和计时覆盖，不返回input/content/metadata
SELECT count(*) tool_n,
 sum(json_extract(p.value,'$.time.completed')>json_extract(p.value,'$.time.created')) valid_n
FROM session_message m,json_each(m.data,'$.content') p
WHERE m.type='assistant' AND json_extract(p.value,'$.type')='tool';
```
