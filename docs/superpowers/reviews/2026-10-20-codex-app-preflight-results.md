# Codex APP 集成 · 前置证据与决策归档（2026-10-20）

> 本文件是 `docs/superpowers/plans/2026-10-20-codex-app-integration-and-acceptance.md`
> Task 1 的交付物：把 §1–2 的决定、平台/build/CLI/启动方式、证据层与缺口整理入库。
> **不改写** 2026-10-19 的探测报告（`docs/superpowers/reviews/2026-10-19-codex-desktop-ipc-windows-4894-probe-results.md`），
> 也不把当时未测的 permissions 说成已测。
>
> 证据分层（全文同一口径）：
> - **WIN-LIVE**：本机 Windows 桌面真机直连官方 Desktop IPC / 真实 CLI 的原始帧；
> - **MAC-RELAY**：Mac 执行者回传的私有报告（原帧与用户正文在 Mac 侧，不在本仓）；
> - **FIXTURE**：仓库内脱敏夹具（源帧可追溯到 WIN-LIVE 或既定金样）；
> - **DERIVED**：在 FIXTURE 上做一处明示修改的派生样本；
> - **STATIC**：上游源码/文档静态核对，不替代本机实测。

## 1. 已确认决定（承接计划 §1.2）

| # | 决定 | 本轮落地 |
| --- | --- | --- |
| 1 | 一个新交付 PR（base=最新 main），提交按「通用底座 / Codex 兼容发送 / Desktop IPC / 修正与验收」分层；不整包合入 #117 | 本分支 `codex/codex-app-delivery`；来源清单见 `2026-10-20-codex-app-extraction-ledger.md` |
| 2 | 通用底座按「可复用、由本轮 Codex 实际调用、能验证」抽取，不引入未消费框架 | `inject/headless/{runner,receipt,gate,turn,codex}.rs` 只保留 Codex 消费面 |
| 3 | ZCode / dsh / WorkBuddy / H11 三家 CLI / 旧 codex app-server 控制面留在 #117 续作 | 路由枚举只有 `CodexQueue`/`CodexExec`；`headless/mod.rs` 只注册 5 个模块 |
| 4 | 已有 Turn 的 `params.input` 开场显示后备；不创建空 session、不改投递真值 | Task 6：`mirror::opening_input` + 两遍投影；投递/屏障零改动 |
| 5 | Unsupported 只延迟前端错误提示；后端 contentState/禁写/stale/屏障不去抖 | Task 7：`useDelayedUnsupported`（500ms） |
| 6 | E11 使用默认关闭的确定性停点（真实 Accepted 后、终态落账前） | Task 8：`codex_control/e2e_stop.rs` + release 编译门 |
| 7 | E10 分别验「Desktop 在场 queue」与「Desktop 缺席 exec」两臂，且两臂都验 H3 关闭拒绝 | 验收表 E10（产品验收报告） |
| 8 | exec 按真实非交互 CLI 语义交付，不承诺继承 Desktop 权限，不承担远程审批 | 兼容回执文案与报告边界；`approval_policy=on-request` 仅为 spawn 入参 |
| 9 | 保留 PATH CLI 契约；缺失时诚实拒绝兼容发送；IPC 与 CLI 可用性分列 | `codex::production_exe` + 独立 `ipc_available()` |
| 10 | Windows permissions 本轮不解锁；4894 profile 继续 false；只归档新增审批证据 | `profile.rs` 的 4894 已登记 `permissions=false`（既有提交 `3946fa47`） |
| 11 | 双平台按各自精确 build 各跑一次适用表；真实手机补一次主路径 | Windows 本机执行；Mac 交接见 §5 |
| 12 | 先合 main、随下一常规版本发布；本阶段不发版；随后整理 #117 续作分支 | 本阶段不打 tag；Task 12 交付 handoff |

## 2. 平台 / build / CLI / 启动方式

| 项 | Windows（本轮执行） | macOS（回传，见 §5） |
| --- | --- | --- |
| OS | Windows 11 家庭版 中文版 10.0.26200 x64 | 由 Mac 执行者记录（回传报告） |
| Desktop 包 | `OpenAI.Codex` MSIX **26.1013.4894.0**（`AppxManifest.xml` Identity Version） | **26.1013.40934** |
| 主进程 | `ChatGPT.exe`（同一 exe 多进程；管道服务端 PID 12536） | ChatGPT.app |
| IPC 端点 | `\\.\pipe\codex-ipc` | `$CODEX_HOME/ipc/ipc.sock`（默认 `~/.codex/ipc/ipc.sock`） |
| 启动方式 | 用户手工启动；T9 重启由执行者用 `explorer shell:AppsFolder\OpenAI.Codex_2p2nqsd0c76g0!App` 拉起 | Finder/正常 GUI 启动（PATH 契约仍需按 E10 子格实测） |
| CLI | codex-cli **0.162.1**（PATH 可发现；npm 垫片形态实测） | 独立 npm **0.163.0**（arm64；shell 与 shell 启动的影子 Tuvis dev 进程 PATH 可发现） |
| 能力档 | `send/state/command/blocking_input=true`，`permissions=false` | `send/state=true`，其余 false |
| 审批/协作 | 探测期 `on-request`+`managed`；Plan 会话由用户输入 `/plan`（`params.collaborationMode.mode="plan"`） | 回传报告记录 |

## 3. 能力 × 证据层（PASS 与缺口）

| 能力 | 结论 | 证据层 | 证据位置 |
| --- | --- | --- | --- |
| `send` | PASS | WIN-LIVE | 2026-10-19 报告 T4；`observed/windows-4894-start-turn.sanitized.json` |
| `state` | PASS | WIN-LIVE | 报告 T1/T2；`observed/windows-4894-snapshot.sanitized.json` |
| `command` | PASS（accept/decline 双臂） | WIN-LIVE | 报告 T6；`observed/windows-4894-command-approval-*.sanitized.json` |
| `permissions` | **NOT_AVAILABLE**（请求未触发；读升权走 command approval） | WIN-LIVE | 报告 T7；profile 保持 false |
| `blocking_input` | PASS（整数 requestId + 原 qid 作答） | WIN-LIVE | 报告 T8；`observed/windows-4894-plan-input.sanitized.json` |
| 原生空首帧 | **NOT_AVAILABLE**（未发送的草稿不落 session） | WIN-LIVE + 产品行为 | 报告 T3；不伪造空会话 |
| 重启恢复 | PASS（断开 0.94s → stale → 重启 → 用户打开会话后 ≤2s 恢复、owner 换代） | WIN-LIVE | 报告 T9（含补测） |
| 忙态 start-turn | **Important**：返回同一 turn id + success，但新 prompt 不落 Turn | WIN-LIVE | 报告 §6-2；Task 8 忙态矩阵 |
| 开场过渡 | 过渡 164–382ms `turnId=null`；正式 userMessage 后到 515–544ms | FIXTURE | 既有 observed 金样；Task 6 后备 |
| Mac 只读收敛 | 首 snapshot revision 3、10/10 completed、Ready/idle、`stale=false` | MAC-RELAY | 回传《macOS重开只读收敛回传报告》 |

**Windows permissions 双臂（仅证据归档）**：2026-10-19 首句前 Default 的新无项目会话出现真实
`item/permissions/requestApproval`；原权限对象批准、空对象拒绝、`scope=turn`；批准文件存在、
拒绝无命令且文件不存在。**审批应答成功 ≠ 权限档切换**：本轮没有切档实测，4894 `permissions`
继续 false，产品上不提供权限切换入口。原始请求对象不入公开 raw（仅报告与私有日志）。

## 4. Task 1 新增夹具（FIXTURE/DERIVED，逐字段对照）

| 夹具 | 级别 | 源 | 与源帧的差异 |
| --- | --- | --- | --- |
| `observed/windows-4894-opening.derived.sanitized.json` | DERIVED | `raw/t4-1.jsonl`（T4 rev3 snapshot）经 `windows-4894-snapshot.sanitized.json` | 仅删去某 Turn 的正式 `userMessage` item；`params.input` / `clientUserMessageId` / 数组顺序 / `null` / 其余字段逐字保留 |
| `observed/windows-4894-busy-negative.derived.sanitized.json` | DERIVED | `raw/t7-1.jsonl` 忙态负例（第二次 start-turn 返回同一 turn id、无新 userMessage） | 仅把某 Turn 的 `params.clientUserMessageId`/`input` 换成合成第二条 prompt（文本为 Synthetic）；`items` 保留第一条真实 userMessage |

保留项：整数 requestId（`command-approval-*`、`plan-input` 夹具）、数组顺序、`null`、clientId
与 `params.clientUserMessageId` 的对应关系、时间差（`createdAt/updatedAt` 原值）。正文与 ID
仍为 Synthetic/占位 UUID；无真实 threadId、用户名或凭据。

## 5. Mac 报告的归属与边界

- 本章所有 Mac 结论均为 **MAC-RELAY**：由 Mac 执行者回传，原帧/用户正文保存在其私有持久目录
  （`~/Documents/tuvis-verify-logs/2026-10-19-macos/` 与 `2026-10-20-macos/`），未进入本仓；
  当前 Windows 会话**没有**直接读取 Mac 原帧，不把转交报告写成当前执行者亲测。
- 新完整 snapshot 不伪装成「补载 RPC 成功」，也不覆盖早期 PARTIAL；Mac 只读收敛记录保留其
  分阶段含义（早期 5 Turn/Loading → 后续新连接 10 Turn/Ready）。
- Mac 上 command/permissions/blocking_input 继续关闭；不复制 Windows 4894 证据、不复制旧 31256 证据。

## 6. 后续补采引用

- Windows 4894 原帧/日志：`E:\LLMproject\Github\tuvis-verify-logs\2026-10-19-4894\`（含 `raw/`、
  `sanitized/`、`prodprobe3/`、`tools/`），按纪律留在探测机。
- Mac 私有目录：见 §5；需要复核时由 Mac 执行者提供脱敏片段，不整目录转发。
- Task 10 产品验收证据：`docs/superpowers/reviews/2026-10-20-codex-app-product-acceptance.md`。
