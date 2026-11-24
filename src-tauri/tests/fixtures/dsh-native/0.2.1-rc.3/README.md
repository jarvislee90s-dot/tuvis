# DSH 原生契约夹具清单（0.2.1-rc.3 · T01）

本目录是 **DSH Desktop 0.2.1-rc.3 原生 wire 帧的脱敏契约夹具**（层次一：原生帧），
供 `src-tauri/tests/dsh_contract_test.rs` 与 `tests/dsh/dto.test.ts` 消费。
Tuvis 自己的 DTO wire 样本（层次二，camelCase）**不**放这里——它内联在测试文件里，
避免「原生样本」与「本应用 DTO」两层混淆。

## 证据来源与版本锚

| 项 | 值 |
| --- | --- |
| 基线版本 | DSH **0.2.1-rc.3**（Mac：`CFBundleShortVersionString`＝`CFBundleVersion`＝CLI `dsh -V`；Win：`FileVersion` 0.2.1-rc.3 / `ProductVersion` 0.2.1.0） |
| Mac 报告 | `docs/engineering/evidence/dsh-native-rpc/received/macos-report.md`（SHA-256 `cc8c66270ace781ad9ed76d4185d722728bdd0be0160da5d707d1967ba3a05af`，接收记录见同目录 `README.md`） |
| Win 报告 | `docs/engineering/evidence/dsh-native-rpc/received/windows-report.md`（SHA-256 `b058d97dab2a0acbba97b82a0327c1a0dd5b0bff0013d0a1cd0bc3650540c60c`） |
| 插件规范核对 | `docs/engineering/evidence/dsh-native-rpc/plugin-authoring-notes.md`（核对日 2026-10-29，官方文档以 master 链接记录，可能漂移） |
| 本机包 descriptor | Mac 宿主 `/Applications/DeepSeek Harness.app`（bundle id `com.deepseek.dsh`）；Win 安装路径 `D:\Program Files\Deepseek-Harness\`（非系统盘，客户端不得硬编码路径） |

**信封契约锚点（实测）**：请求 `{type:"client-request",rpcId,method,payload:{args}}`；
响应 `{type:"server-response",rpcId,result:{ok:true,value}|{ok:false,error:{code}}}`
（Win 报告 §3.2 步骤 6 实跑成功帧；Mac 报告 §2 表第 9 行 403 信封形状复用）。

## 包摘要（如实申报）

报告**未含**安装包/asar 的 SHA-256。已记录的包事实：

- Win：`app.asar` 182,434,783 字节、18,188 条目、`@deepseek-ai/**` 307 个官方包；内嵌 runtime node 24.21.0 / pnpm 11.28.5。
- Mac：`dsh/` 抽取树 6,196 文件 / 110 MiB；官方包 `package.json` version 均为 0.2.1-rc.3。

**未测项**：包内容指纹（SHA-256）须在下轮授权采（或读包预算内）补齐；在补齐前，
`ContractEvidence.package_sha256` 的真实取值来源缺位，测试一律用合成占位并标 `synthetic`。

## 夹具逐文件来源表

| 文件 | 层次 | provenance | synthetic 与否 |
| --- | --- | --- | --- |
| `windows/rpc-session-list-ok.json` | 原生响应帧 | **真实**：Win 报告 §3.2 步骤 6 实跑回帧原文（`session/list → ok:true {items:[]}`，零插件隔离实例）。`rpcId:"ok"` 为探针原始取值，非脱敏改写 | 真实帧，无合成字段 |
| `macos/rpc-unpaired-403.json` | 原生响应帧 | **真实形状**：Mac 报告 §3.3 实测 403 回帧（`/remote/` 未配对）。报告原文 error 对象尾部以「…」省略；本夹具只收录报告可见字段（`code:"unpaired"`），**不补写**未见字段 | 形状真实；省略字段如实缺位 |
| `macos/waterfall-approval-request.json` | 原生事件帧 | **合成**：字段形状按 Mac 报告 §4「审批」行实帧（`{type:"waterfall",event:"approval/request",eventId,agentId,request:{toolName,callId,reason,displayReason}}`），**全部取值为合成**（报告未印出实值；U2 未闭合） | `SYNTHETIC`（整帧） |
| `macos/waterfall-user-questions-request.json` | 原生事件帧 | **混合**：结构与 `id:"fruit_choice"`、题干「选苹果还是香蕉？」、header「水果选择」来自 Mac 报告 §4「问答题型」行实帧（真实）；`eventId`/`agentId` 与选项 `description` 值为合成（报告以 `{label,description}×2` 省略实值）；选项 label「苹果/香蕉」与报告作答值 `selected:["苹果"]` 一致 | `SYNTHETIC`（eventId/agentId/description 字段级） |

**合成标注纪律**：上表即为 synthetic 权威登记（manifest annotation 形态，计划全局约束
「模拟帧只能标 synthetic」）；帧体**不**注入 `synthetic` 字段，保持原生形状逐字节可用。
任何后续任务往本目录加帧，必须先在本表登记 provenance，否则视作未定源样本、测试不得引用。

## 未测项（本轮报告未覆盖、夹具不得冒充）的清单

以下场景**没有**任何真实帧，任何夹具化尝试都属伪造；后续任务需要时必须先补采样
（Mac 报告 §7 U2–U6 / Win 报告 §7 待人工补验 ①–⑤）：

1. waterfall 实帧的**真实字段值**（eventId 取值空间、挂起审批重放语义）——Mac U2；
2. `session/control` 参数形状、assistant 逐 token delta 帧、`updateQueue` 三动作的真实队列项、
   `commands/execute` 真跑、附件经 receiptId 绑定后被 Agent 读到——Mac U3 / Win 补验②③；
3. 断线重连与 generation 重建、两端抢答、挂起后才连接、同 requestId 幂等反证——Mac U4/F3；
4. Windows 侧只有 mux **传输层**实测（101 + 帧往返），真实会话流内容未测——Win §4；
5. 跨宿主接管 Desktop 持有会话、官方 `client.node.js` 真跑——Mac U5/U6；
6. 0.2.1-rc.3 之外的版本：未知版本按当前包契约独立识别（全局约束），**不得**用本目录样本豁免。
