# Claude / Codex 原生遥测边界增补

日期：2026-10-24。本文补充本轮只读源码核查结果。用户已确认：**正式首版不建设原生遥测接收器；缺少可靠指标时显示 `—`。** 本文说明本版禁用相应遥测速度来源的确定原因，不构成本版增强接收器的建议或兼容承诺。

## 1. 核查范围与证据等级

本轮只读现有报告、安装包元数据、Claude 安装包内嵌的可读 JavaScript，以及官方文档和 Codex 官方原始源码；未启动 CLI、调用模型、启用遥测、修改配置、安装插件或改动真实账本。

- Claude 本机 npm 包版本：`2.1.293`。版本来源为 `/Users/jarvis/.nvm/versions/node/v26.8.1/lib/node_modules/@anthropic-ai/claude-code/package.json`；实现来源为同包 `bin/claude.exe` 内嵌的可读打包 JavaScript。以下内部变量名仅用于复核该版本，不作为稳定公开接口。
- Codex 本机 npm 包版本：`0.163.0`。版本来源为 `/Users/jarvis/.nvm/versions/node/v26.8.1/lib/node_modules/@openai/codex/package.json`。安装包没有本地 Rust 源码，本轮对应读取官方 `rust-v0.163.0` tag；**npm 包版本不能直接证明当前桌面宿主进程使用同一版本**。
- 静态调用点能证明字段的实现和边界，不能证明本机端到端导出已成功、全部协议与供应商行为一致、运行期上下文关联完整或事件无丢失。

## 2. Claude Code：原生计时存在，但不能直接组成统一速度指标

[官方监测文档](https://code.claude.com/docs/en/monitoring-usage#traces-beta) 描述原生增强追踪，需启用 `CLAUDE_CODE_ENABLE_TELEMETRY`、`CLAUDE_CODE_ENHANCED_TELEMETRY_BETA` 并选择 `OTEL_TRACES_EXPORTER`；支持 OTLP gRPC、HTTP JSON 和 HTTP protobuf。它无需安装工具插件或代理模型请求，但默认关闭，且本轮没有启用。

本机 `2.1.293` 内嵌实现确认以下区别：

| 原生字段           | 实际设置点与基线                                                               | 对统一指标的限制                                                                             |
| ------------------ | ------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------- |
| `ttft_ms`          | `D7` 在 `message_start` 时以 `monotonicNow() − Cy` 设置                        | 消息起始事件不等于首个非空文本、思考或工具参数 delta，不能仅凭字段名认定为首个有效输出 Token |
| `first_content_ms` | `xve` 在首次 `content_block_start` 时设置；text、thinking、tool_use 等共用该处 | 内容块开始不等于首个非空内容 delta；非流式回退不提供该字段                                   |
| `duration_ms`      | `Fqt()` 用 span 创建至结束的单调时钟差计算                                     | 属于包含重试的逻辑请求窗口，且不是已经验证的纯生成窗口                                       |

`Cy` 在每次尝试开始时重置，`D7` / `xve` 在流初始化时重置；最终成功上报的等待值使用成功尝试的基线，而 span 时长使用逻辑请求的基线。**存在重试时，不得用 `duration_ms − ttft_ms` 或 `duration_ms − first_content_ms` 推算生成时长。** 无重试时也不能仅凭减法就证明 Token 与生成窗口语义一致。

原生 span 确有 `model`、`request_id`、`client_request_id`、`attempt`、`output_tokens` 等字段，文档还定义会话与父子追踪。它们证明存在原生关联候选，不证明历史 JSONL 的 `message.id` 与 API `request_id` 可直接合并，也不证明所有供应商、恢复路径、思考或工具参数输出均满足统一速度口径。[字段文档](https://code.claude.com/docs/en/monitoring-usage#span-attributes)

**本版结论：不接入这些遥测字段作为正式 TTFT/TPS 来源。** 现有历史 JSONL 没有已经验证的逐请求完整计时；遥测既未启用，也存在上述确定的事件语义与重试基线限制。相应指标无可靠来源时显示 `—`。

## 3. Codex：mapper TTFT 起点偏晚，首 item 不是首 Token

同版本官方 [`core/src/client.rs`](https://raw.githubusercontent.com/openai/codex/rust-v0.163.0/codex-rs/core/src/client.rs) 的调用顺序明确：

1. HTTP 路径先等待 `client.stream_request(...).await` 成功，再调用 `map_response_stream()`。
2. WebSocket 路径也先等待 `.stream_request(...).await`，再进入同一 mapper。
3. `map_response_events()` 的异步任务内才创建 `request_start = Instant::now()`。
4. `ttft_ms` 在首个 `ResponseEvent::OutputItemAdded` 时设置。
5. 收到 `Completed` 后调用 `session_telemetry.sse_event_completed(usage, ttft_ms)`。

因此，该 `ttft_ms` 的计时起点已经晚于前面的 transport 请求调用；它测量 mapper 启动后到首 output item 的等待。`OutputItemAdded` 也不等于首个非空 Token delta。**不能把这个值直接作为“发送模型请求到首个有效输出”的统一请求 TTFT。**

完成函数虽命名为 `sse_event_completed`，但 WebSocket 同样经过上述 mapper；不能声称该完成字段仅适用于 SSE，也不能仅因存在 WebSocket 事件就宣称已覆盖完整请求计时。WebSocket 还有 warmup 路径，不能未经区分就计入生成请求。

### 请求与追踪关联缺口

同版本 [`session_telemetry.rs`](https://raw.githubusercontent.com/openai/codex/rust-v0.163.0/codex-rs/otel/src/events/session_telemetry.rs) 的 completed 日志写出 output、reasoning 等 Token 与 `ttft_ms`，**未直接写入 `response_id`、`attempt`、请求起止或生成时长**。

[`shared.rs`](https://raw.githubusercontent.com/openai/codex/rust-v0.163.0/codex-rs/otel/src/events/shared.rs) 的公共事件宏补充 `conversation.id`、`model`、`app.version` 和事件时间，没有自动补齐业务请求与尝试身份。mapper 中确实存在 `response_id`，但传给另一个 `InferenceTraceAttempt.record_completed()`；该类型属于[独立 rollout trace bundle](https://raw.githubusercontent.com/openai/codex/rust-v0.163.0/codex-rs/rollout-trace/src/lib.rs)，不能将其字段当作 completed OTel 日志已经具备的字段。

静态代码中的 trace/span、response ID 和 attempt 分散存在，不等于导出的记录已经能无歧义关联；本轮没有证明异步任务的上下文传播、实际 OTLP 关联、重试身份和 transport span 的完整生命周期。HTTP 请求 duration、SSE / WebSocket 单事件 duration 也不能代替生成时长。[官方遥测说明](https://learn.chatgpt.com/docs/config-file/config-advanced#observability-and-telemetry)

**本版结论：不接入该遥测 completed TTFT/TPS 来源。** 已有回合 TTFT 仍是回合指标，不能代替逐请求平均；现有 `response_id` 与用量也不足以补造请求起点或生成窗口。相应指标无可靠来源时显示 `—`。

## 4. 已有遥测出口冲突

Codex 同版本 [`otel/src/config.rs`](https://raw.githubusercontent.com/openai/codex/rust-v0.163.0/codex-rs/otel/src/config.rs) 为 logs、traces、metrics 分别保存 exporter；每个 exporter 是单 endpoint 枚举值，没有同信号多 endpoint 列表。不同信号可以分别设置，不代表同一信号支持两个 OTLP 目标。

Claude 的 exporter 类型列表允许 `console` / `otlp` 等组合，这不证明支持两个 OTLP 目标。其受管设置还可能在启动时删除开发者级 per-signal endpoint；详细 beta tracing 的 `BETA_TRACING_ENDPOINT` 也是独立冲突面。[官方配置与受管出口规则](https://code.claude.com/docs/en/monitoring-usage#how-managed-settings-lock-the-otlp-destination)

因此，不能以原生遥测支持为由承诺“自动增加兔维斯出口且保留所有现有目标”。**已有目标不得被覆盖；原生同信号双出口能力未经证明。** 本版不建设接收器，也不改用户遥测设置，故本版不存在为性能统计接管或改写出口的动作。

## 5. 本版冻结边界

- 正式首版仅使用已验证的现有持久来源；四项指标各自判定可用性，没有可靠计时就显示 `—`。
- Claude / Codex 上述原生遥测不进入本版速度计算，不将字段候选描述为已支持，不以回合时间、消息时间或事件处理时长补造请求计时。
- 本次只读核查已经能够确定上述禁用原因；不要求用户回答可通过环境或源码确认的事实。
- 若未来另立范围研究这些能力，需重新验证事件边界、Token 同批关系、重试与协议覆盖、关联和持久性；这不是本版实施任务或完成条件。
