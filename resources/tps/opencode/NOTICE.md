# NOTICE — tuvis-tps-observer（OpenCode 观察插件分发）

## 上游参考与许可

本插件的生命周期（setup → 订阅 → teardown）、事件分派与结束校准思路参考了开源项目
**ChiR24/opencode-tps-meter**（SPDX-License-Identifier: MIT）。

- **锁定上游 commit**：`a6e050d2cbe340a3906e7233cdc880c3199355e4`
  （2026-10-24，v0.4.2，`docs(release-notes): describe v0.4.1 and v0.4.2 for users`）
- **上游许可核验**：该 commit 时点仓库内**无 LICENSE 文件**；许可声明在其
  `package.json` 的 `"license": "MIT"` 字段（本目录 LICENSE 文件内的标准 MIT
  授权文本即 "MIT" 标识所指之 canonical 文本，逐字保留）。
- **借用范围（仅思路，不搬代码）**：
  - `src/v2/server.ts`：`{id, setup}` 形态、`ctx.event.subscribe()` 的 `for await`
    消费循环、AbortSignal 生命周期、事件循环 try/catch 兜底；
  - `src/v2/meter.ts`：`eventTime()` 宿主时刻优先 + 24h 时钟偏移护栏、
    `session.step.ended` 为结算面、finish 语义（`tool-calls` = 正常步骤收尾）；
  - **未引入**：其 UI/TUI 渲染、字符估算（tokenCounter/calibration）、以及其未实现的
    retry 处理路径（本插件的 retry 语义按兔维斯 spec §3 另行定义：识别而不入速）。
- 本插件实现为原创（兔维斯 Task 6），事实行键集、稳定 hash 键、有界失败与
  JSONL 落盘均为本仓库自有设计，随兔维斯主仓库以 MIT 分发。

## 宿主形态证据（锁定 opencode v2.0.22，本机 /Users/jarvis/.opencode/bin/opencode）

- 插件默认导出校验：`{id, setup}` 或 `{id, effect}`（二进制内文案
  "Plugin must export a default definition with an id and an effect or setup function."）
- `event.subscribe` 返回 AsyncIterable（内部 `Plugin.Event.makeStreams` /
  `Stream.toAsyncStreamable`）；消费必须用 `for await`（iterable 自身无 `next`）
- wire 事件信封 `{id, created, type, data}`（二进制事件构造点）；
  2.1.x 世代 `session.next.*` 词表按别名兼容（@opencode-ai/sdk 1.17.13 类型）
- 插件目录发现：`["plugin", "plugins"]` 双目录（二进制）；本分发装 `plugins/`

## 分发说明

- `tuvis-tps-observer.ts` — 插件本体（纯 JavaScript 以 .ts 分发：宿主 2.0.22 目录发现
  不装载 `.mjs`，2026-10-24 隔离实测；安装目标：`~/.config/opencode/plugins/`）
- `package.json` — 分发清单（**不**安装进 plugins 目录：目录发现会把其中每个文件
  当插件模块 import；零依赖闭包，安装无需任何包管理步骤）
- `LICENSE` / 本文件 — 许可与出处（嵌入 Tuvis 二进制，`install.rs` include_str!）
