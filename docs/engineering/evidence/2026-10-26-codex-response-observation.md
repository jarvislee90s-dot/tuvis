# Codex 含等待响应吞吐：只读依据

日期：2026-10-26。用途：支撑输出速度 V2 的新增观测。未发送模型请求、读取认证文件、修改配置或真实账本。

## 1. 来源与边界

[codex-tps 的 analysis.go](https://github.com/adenta/codex-tps/blob/main/analysis.go)按用户/开发者/工具返回等输入边界到最后模型生成项的时间，配合 `last_token_usage` 计算 effective TPS。它包含首 Token 等待，不是流式生成速度，也不提供逐响应 TTFT。

参考其边界分段思路，不能原样接入其每次 token_count 都作为新样本的行为。当前 Codex 日志还包含带 response_id 的 token_usage_record，应以该原生响应身份去重，避免伴随事件重复计数。

## 2. 本机结构核对

只读最近三份 rollout 的有限尾部（每份至多 3 MiB）和必要的头部元数据，CLI版本包含 `0.163.0-alpha.11.1`、`0.164.0-alpha.11.1`，provider=openai。用户说明使用 GPT 订阅，本次未打开 auth.json 验证账户。

共同存在的结构：

- `event_msg.task_started`、`turn_context`：回合身份与模型上下文。
- `response_item`：用户输入、reasoning、assistant message、function/custom tool call 及对应 output。
- `token_usage_record.payload`：response_id、thread/session/turn/root_turn_id、usage、turn/thread累计用量。
- `usage`：整数 output_tokens、reasoning_output_tokens、input/cache/total 等。
- 同时有 `event_msg.token_count.info.last_token_usage`，与原生记录不能各记一遍。

按原生 response_id 及输出整数，结合可恢复的局部输入/输出边界做离线候选检查：三份有限内容中分别得到29、5、101个正窗口候选，其中一份尾部有一条缺完整边界而跳过。数量是当时文件快照，不是正式接入覆盖率；未把候选率当成已经验收的吞吐值。

这足以确认当前机器有可实现的含等待响应观测入口。它不能证明日志边界等于精确API发出/流结束时间，正式数值必须标为“日志近似”；也不能据此生成逐响应首 Token 时间或流式速度。

## 3. 接入时必须补齐的验证

1. 使用原生 response_id 作为稳定响应键；原生与伴随 token_count、复制/分叉历史不重复计数。
2. 首个响应从当前回合有效输入边界开始；后续从上一响应完成之后最后一组匹配工具返回/有效输入开始。最后模型生成项为结束，不取 task_complete 或工具结束作响应结束。
3. 生成项与输入/工具项交错、边界不完整、模型归属缺失、无法排除失败重试等条目降级，不把整回合时间补进去。
4. 核正常文本、reasoning、单工具、多工具、子代理、取消及重复用量事件；纯工具执行间隔不进入响应分母。
5. 原生 usage 与工具/文本日志先后顺序按真实样本核对，支持必要的未决状态；不能把“第一份候选脚本能算出数值”当成最终解析器验收。

不要求用户重新进行双平台全面探测。实施使用已有真实结构的脱敏重放和影子库；无法恢复的调用保持无值。

## 4. 材质参考

Apple 的[Materials](https://developer.apple.com/design/human-interface-guidelines/materials)及[Meet Liquid Glass](https://developer.apple.com/videos/play/wwdc2025/219/)将玻璃主要用于控制与导航层，并强调内容可读性、避免玻璃层叠。Tuvis 的统计文本/曲线使用稳定平面，玻璃仅用于导航与浮层；跨平台CSS材质是视觉近似，不承诺系统级原生光学效果。
