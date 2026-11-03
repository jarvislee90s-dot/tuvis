# DSH 总生成输出计数的补证与首版准入

日期：2026-10-24。本次只读安装包及最近一份持久日志，无模型请求、配置修改或真实账本写入。

## 已确定事实

只读解析 `/Applications/DeepSeek Harness.app/Contents/Resources/app.asar`：

- `dsh/node_modules/@earendil-works/pi-ai/dist/api/openai-completions.js` 将 `rawUsage.completion_tokens` 写入 `usage.output`，源码明确注释 `OpenAI completion_tokens already includes reasoning_tokens`。reasoning另存细分，不再相加。
- `dsh/node_modules/@deepseek-ai/dsh-llm-pi-ai/lib/index.js` 的 `mapUsage` 将同次调用 `usage.output` 原样写入规范 `outputTokens`，不按可见字符串重数Token。
- 最近一份 `assistant/message` 的 `data.message.source.replayState.response` 实际保留 `kind/version/api/provider/model/responseId/stopReason`；观察到 `kind=pi-ai,version=2,api=openai-completions`。本补查只证明形态存在，不声明全部历史覆盖。
- 同一 `assistant/message` 有 `data.usage.outputTokens` 与持久 stream，后者实际含 reasoning-chunks、text-chunks、tool-call-chunks；可按原始成员时间恢复客户端首个非空输出。

## 固定规则

首版只允许上述 replay profile（pi-ai/2/openai-completions）把原生 `outputTokens` 标为 `provider-total-includes-reasoning`；还要满足同消息模型/供应商与response一致、无step重试、流成员非空并分布在至少两个时刻、输出数和正生成时间等主设计条件。不能以会话当前配置或provider名称猜profile。缺profile或其他profile固定标unknown-output-scope，TPS为空。

这项证明确定的是供应商协议的总输出计数及其与同次响应的归属，不是逐Token服务端生成时间。正式指标始终是“供应商总生成Token / 客户端首有效输出至该响应结算的窗口”，包含可观测思考、正文、工具参数；不能宣称服务端纯解码速度、隐藏思考独立速度，或证明网络没有缓冲。非流式、只有单时刻输出、缺起点及不一致计数继续排除。

此前 native-timing-addendum §6.2 的“总output含义缺证据则为空”仍适用于未命中此明确profile的记录；本次补证不能扩大为所有DSH适配器均已确认。定稿引用本文件与固定profile，不再留给执行者选择。
