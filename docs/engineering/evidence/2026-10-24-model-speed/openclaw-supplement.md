# OpenClaw 补充静态探测

日期：2026-10-24（北京时间）。仅只读现有文件，未启动工具、写配置或操作真实用量账本。

## 范围与采样

项目注册了八个工具适配器，但 `UsageSourceId::ALL` 只有七个采集源，不含 OpenClaw。为响应「各个工具都探测」，仍对 OpenClaw 做补充检查；本轮不据此隐式扩建第八套费用采集器。

路径依据：`src-tauri/src/adapter/openclaw.rs`、`src-tauri/src/monitor/openclaw_parser.rs`。当前适配器主要读取 `~/.openclaw/openclaw.json` 的工作区/agent 配置，不承担请求计时采集。

本机 `~/.openclaw/agents/*/sessions/*.jsonl` 共枚举到 562 个文件。按文件修改时间选最近 4 个，每文件最多读前 4 MiB / 20,000 行。文件大小依次为 94,713 / 269,268 / 262,234 / 219,630 字节，本轮全部在预算内，共 133 条可解析记录。包含一份 trace 记录及三份普通会话记录，统计不是全量覆盖率。

## 观测

- 113 条 `message`：3 条用户、46 条 assistant、64 条 toolResult。46 条 assistant 均有 `message.usage`，键包括 `input/output/cacheRead/cacheWrite/totalTokens/cost`。
- 普通记录有顶层及 `message.timestamp`；模型/供应商可在 `message.model/provider` 读取，另有 `model_change`。这些时间戳不是首 token 证据。
- 18 条记录带 `message.details.durationMs`，可作为部分工具原生耗时候选；其计时起止、审批是否包含、与 callId 的配对仍需逐工具核实。
- trace 样本含 `session.started`、`prompt.submitted`、`model.completed`、`session.ended`，公共字段含 `ts/seq/sourceSeq/sessionId/runId/provider/modelId`。`model.completed.data` 包含 `truncated/originalBytes/limitBytes/reason/usage/promptCache/droppedFields`；该样本没有可直接使用的请求开始/首输出边界，且存在截断标记。
- 本轮没有找到原生 TTFT 或可验证的首输出后生成时间。不扫描正文中的字段名来制造计时证据。

## 四项结论

| 指标              | 本轮结论                                            |
| ----------------- | --------------------------------------------------- |
| 模型用时          | 未确认可靠逐请求边界；不能用相邻 assistant 时间代替 |
| 工具调用用时      | 部分原生 duration 候选；不是全量保证                |
| 首 token 平均等待 | 样本无证据                                          |
| 流式 TPS          | 有输出计数，没有有效计时窗口，不能计算              |

历史可回填仅能说「有 Token 与部分工具耗时线索」；不能承诺首 token/TPS。OpenClaw 本身尚未进入现有费用统计链路，若要产品接入须另补身份、Token 语义、去重与兼容设计。

## 可复跑方法与后续实测

1. 枚举上述 glob，先 stat，选最近 4 个文件；只读并限定每文件字节和行数。
2. 仅计数记录类型、角色、`message.usage` 和计时键路径，不保存正文、密钥或工具参数。
3. 后续如要启用该源，先核实工具原生 trace 是否有未截断、可关联的请求开始及首输出事件；再在影子环境做单请求、工具执行、流式与取消场景对照。不由字段名称推断计时语义。
