# 移动端子 agent 运行 chip：C3 真机验收（甲）与衍生特性记录（乙）

日期：2026-10-08 ｜ 分支 `feat/mobile-subagent-chips`（未合入）｜ 应用：MAM dev 版 +
远程接入 + 手机看板；实验期间零代码改动（本文档除外）、未 commit、未 push。

本文档分两部分：**甲部分** = 主线任务「C3 真机验收」的判定与根因；**乙部分** = 验收过程
**衍生的新特性需求与扩展取证**（数据与结论引用甲部分证据，独立立项）。

**主线结论：C3 FAIL（消失环节）**——三阶段取证全部完成，根因定位、修复计划可定稿（甲.5）。

---

# 甲部分：主线验收——子 agent chip（claude 全链路）

## 甲.1 实验设置（含前提修正）

- **实验对象变更**：原计划用执行 agent 自身会话做实验。执行前自查发现本执行环境**不以
  claude 格式落盘**（`~/.claude/projects/` 下无 `E--LLMproject-Github-mam-worktree` 项目目录；
  探针 subagent 亦不在任何 `subagents/` 留痕）——判据读取不到该会话。经用户裁决改用
  **真实 Claude Code 会话**作为实验对象：用户在 `E:\LLMproject\Github\Test2` 新开会话。
- 父会话锚点：`~/.claude/projects/E--LLMproject-Github-Test2/8cd10b60-79bb-4bd0-9a1c-734bf5dbd307.jsonl`
- 应用：MAM dev 版（`pnpm tauri:dev`，mam-worktree 工作树）+ 远程接入 + 手机看板。
- 角色分工：用户向 Test2 会话粘贴驱动提示词（spawn / 续跑），执行方秒级只读取证 + 口径播报，
  用户回报手机所见。

## 甲.2 时间线（本地 UTC+8；文件内引用为 UTC）

| 时刻 | 动作 | 落盘证据 | 用户手机所见 |
|---|---|---|---|
| 22:53:07 | Test2 会话活跃 | 父 .jsonl 持续追加 | 看板可见 Test2 会话卡 |
| 22:55:53 | Test2 会话 `tool_use Agent` | 父 .jsonl 14:55:53Z | —— |
| 22:55:56 | **spawn** | `8cd10b60…/subagents/agent-abg-progress-test-3b76c22694757d64.meta.json`：`taskKind=in_process_teammate`、`teamName=session-8cd10b60`、`name/agentType=bg-progress-test`；子转写首行 ts=14:55:56.488Z | —— |
| 22:56:19 | 阶段 1 自查 | 父 .jsonl 中该 agentId 出现 **0 次**（=spawn 态运行中，判据正确） | —— |
| 22:57:02 | —— | —— | **chip 出现**：`◉ bg-progress-test 1:06 · 4.24万`（时长自走、token 在涨）→ **出现 ✓** |
| 23:02:26 / 23:02:38 | agent 完成 | 父 .jsonl 出现 **`<teammate-message teammate_id="bg-progress-test" …>`** 两条（完成报告 + `{"type":"idle_notification","from":"bg-progress-test"…}`）；**无 `task-notification`、无 `<task-id>`；agentId 字符串全程 0 次** | **chip 未消失，时长继续走** |
| 23:02:29（6m33s） | 用户在主任务看到回报；claude 终端 footer 已清空该 agent | 子转写 23:02:35 终答「任务已全部完成」 | **消失 ✗（FAIL）** |
| 23:08:53 | **续跑**（用户驱动） | 父 .jsonl `tool_use SendMessage`：`input.to = "bg-progress-test"`（**名字**，非 meta 文件名 id）；子转写 23:09:06 恢复写入（唤醒 ✓） | chip 持续走（从未消失，无「复现」可言） |
| 23:11:34 / 23:11:45 | 续跑完成 | 第二组 teammate-message（结果 `355/113≈3.141593` + 第二次 `idle_notification`）；子转写 23:11:37 终答 | chip 仍未消失 |
| 23:20:28 / 23:23:31 | 第二个 teammate（并发标本，见乙.3.3） | `agent-abg-progress-test-2-ef2d485433592526.meta.json`；完成后同样仅 teammate-message | 双 chip 同屏（乙.3.3） |

## 甲.3 三阶段判定

- **阶段 1 出现：PASS**。名称=meta 的 agentType、时长自走（锚=子转写首条 timestamp）、token 增长。
- **阶段 2 消失：FAIL**。完成通知不是 spec §2.1 的 `<task-notification><task-id>…` 形态，判据收不到
  Stop 事件。
- **阶段 3 续跑：证据采集完成**。SendMessage 事件确认为 `tool_use SendMessage`（与判据预期工具名
  一致），但 `input.to` 为**名字**；「复现」观察在本场景不适用（chip 从未消失）；二次完成同样失明。

**总结论：C3 FAIL（消失环节）**。手机所见与预期在「出现」环节一致，「消失/再消失」两环不符。

## 甲.4 根因：同机并存两套后台子 agent 机制，事件格式不同

| | 机制 A：经典后台（spec §2.1 依据；2026-10-08 下午 bea26e09 会话） | 机制 B：teammate（本次 Test2） |
|---|---|---|
| meta.json 特征 | `spawnDepth:1`、有 `toolUseId`、**无 taskKind** | `taskKind=in_process_teammate`、`teamName`、`name` 字段、`spawnDepth:0` |
| agentId 形态 | 纯 16-hex（`ae78013828b11ec80`） | 名字前缀（`abg-progress-test-3b76c22694757d64`） |
| 完成通知 | `<task-notification>…<task-id>该id</task-id>…`（父 .jsonl 实存 5 处） | `<teammate-message teammate_id="名字" …>` + `{"type":"idle_notification","from":"名字"}`（父 .jsonl 实录）；**task-notification 全程 0** |
| 父 .jsonl 中的 id | 通知内含 id | **agentId 长串 0 次**；对外标识是名字 |
| 续跑 | （spec §2.1：SendMessage to=该id） | `SendMessage` 但 `to=名字` |

两机制由 meta.json 的 `taskKind` 字段可靠区分。机制选择标准未定（待议：与 spawn 提示词/模型配置
相关？本次 teammate 的 model 串为 `ccs-claude-zhipu--glm-5.3-flash[1m]`）。

## 甲.5 修复立项（下一批次；判据变更，需先修订 spec §2.1/§4.1 再实现）

1. **teammate 机制事件分支**（`monitor/subagents/claude.rs`，以 meta `taskKind=in_process_teammate`
   为开关）：
   - Stop 信号 = teammate-message 行内 `{"type":"idle_notification","from":"<名字>"}`
     （`teammate_id` 属性可作辅助匹配；普通（非 idle）teammate-message 是否计为活动信号：待议，不入本批）；
   - **名字→id 归一**：以注册表内 meta 的 `name` 字段建映射，Stop/Resume 双向归一（经典机制
     `to=长 id` 路径不受影响）；
   - Resume = `tool_use SendMessage`（`input.to` 经同一映射归一）。
2. **spec 修订**：§2.1/§4.1 追记两机制并存事实与各自事件格式（本报告甲.4 表格为底稿）。
3. **回归夹具**：teammate 形态（meta 带 taskKind/name；teammate-message + idle_notification 行；
   SendMessage to=名字）→ 锁「完成即消失、续跑即复现」。
4. **关联**：issue #134（看板徽标）不受影响；机制 A 行为与既有测试不变。

## 甲.6 附注

- 残留 chip 处置：关闭 Test2 的 claude 进程后，会话卡从看板消失，chip 随之消失（spec §8.2 既有
  口径：残留随会话中断整区消失）。
- 既有测试全绿与本发现不冲突：单元/集成测试锁定的是机制 A（spec §2.1 取证形态）；机制 B 是
  本次实机新取证。
- 证据原件（只读）：父 .jsonl 与 `subagents/agent-abg-progress-test-3b76c22694757d64.{meta.json,jsonl}`、
  `agent-abg-progress-test-2-ef2d485433592526.{meta.json,jsonl}`（Test2 项目目录内）。

---

# 乙部分：衍生新特性需求与扩展取证（引用甲部分证据）

> 甲部分验证了 chip 的「状态判定」链路；乙部分记录验收过程**衍生出的新特性需求**（用户提出，
> 待立项）与支撑其设计的扩展取证。乙不重复甲的数据，仅引用。

## 乙.1 需求备忘（用户 2026-10-08 深夜提出，待立项；不在验收批实现）

1. **点击 chip 查看子 agent 实时输出**（后台 agent 正在吐出的上下文信息，如「步骤 3/12 完成…」）。
2. **活跃/历史分层**：派发即「走字」；任务完成回报后停止走字，转入「子 Agent 触发清单」留存。
3. **界面**：(a) 活跃置顶继续走字；(b) 历史以折叠目录展示，点击看历史记录；(c) 多 agent 并行
   各自成卡（名字/运行时长/token），点卡片看内部信息。
4. **通知分类开关**：子 agent 回报导致主会话转绿的提醒，默认保留、可按人关闭（「有的人需要
   提醒，有的人不需要」）——用户确认现状（回报=任务完成可提醒）合理，但要可选。

**可行性对照**：

| 需求 | 数据源与结论 | 依赖 |
|---|---|---|
| 1 实时输出 | 子转写 `agent-<id>.jsonl` 全量消息流已实证可解析（乙.3.1）；后端既有 512KB 尾窗读取先例（monitor/jsonl.rs）与增量读通道；缺「子 agent 详情端点」+ 前端详情视图 | 端点契约扩展 |
| 2 活跃/历史 | meta.json 永久留存已实证（乙.3.2，触发清单可回溯构建） | **甲.5 修复**（有停止信号才能分层） |
| 3 多卡片 | 后端已返回列表（spawnTs 升序）、前端已多 chip 渲染（有测试）；真机并发已过验（乙.3.3） | 无 |
| 4 通知开关 | 判定信号已在其位（乙.2，无需额外探测） | 建议与甲.5 的 spec 修订一并立项 |

**契约注意**：详情端点与载荷状态字段均为端点契约扩展 → 须修订 spec 并立新批次（验收批不动）。

## 乙.2 需求 4（通知分类开关）：可行性与实现思路——无需额外探测

- **现状**：MAM 通知由**状态颜色跃迁**驱动（`useNotification`：waiting=红 / processing·thinking=
  黄 / idle·finished=绿；同色 5s 去重、绿有 3s 稳定窗；已有全局开关 `notifications_enabled`）。
- **判定信号已在其位**：通知载荷的 `lastMessage` 自带「成因特征」——
  - 机制 B（teammate）：`Another Claude session sent a message: <teammate-message …`（乙.3.4
    时刻表 4 条即此形态）；
  - 机制 A（经典）：`<task-notification>…` 块；
  - 主会话自身动作完成转绿则**无**该特征（实证：23:09:43 的弹窗 lastMessage 是主 agent 自己的
    SendMessage 回执复述——开关不会误伤这类提醒）。
- **实现思路**：新增 KV 开关（如 `notify_subagent_report`，默认开）；在 `notifyCompletion` 入口
  按 lastMessage 特征过滤；或由适配器在解析时对「子 agent 回报触发的跃迁」直接打标（更干净，
  与甲.5 的事件解析分支共享判定，二选一）。**两种路线都不需要新的磁盘探测或事件源**。
- **时机**：与甲.5 的 spec 修订一并立项（同一批评审，事件格式与通知口径一次定稿）。

## 乙.3 扩展取证

### 乙.3.1 三份子转写结构普查（详情视图渲染器的输入清单）

| | Test2-teammate（213KB/83 行） | 经典-Explore（394KB/67 行） | 经典-Plan（482KB/84 行） |
|---|---|---|---|
| 行类型 | user 18 / attachment 26 / assistant 39 | user 19 / attachment 19 / assistant 29 | user 23 / attachment 21 / assistant 40 |
| 内容块 | thinking 6、text 17、tool_use 16、tool_result 16 | thinking 10、tool_use 18、tool_result 18、text 1 | thinking 12、text 6、tool_use 22、tool_result 22 |
| 工具 | Bash 14、**SendMessage 2**（向 team-lead 回报=完成信号在子侧的形态） | Bash 2、Glob 2、Read 7、Grep 7 | Read 12、Glob 1、Grep 7、Bash 2 |
| 首条 user = 任务原文 | ✓（teammate 形态：`<teammate-message teammate_id="team-lead" summary="…">任务…`） | ✓（裸任务文本） | ✓（裸任务文本） |

结论：**两机制子转写格式一致**（详情视图单一渲染器通吃）；任务原文可从首条 user 行提取；
thinking 块存在（视图需决定折叠/隐藏）；attachment 行为噪声需过滤；单文件 200–500KB 量级
（尾窗读取成本可控）。另发现：teammate 完成时**子转写内**亦有 `tool_use SendMessage{to:"team-lead"}`
——完成信号在子侧的旁证（父侧 idle_notification 仍为主信号）。

### 乙.3.2 历史清单可回溯

下午会话（bea26e09）两个已完成 agent 的 meta.json 至今在盘且可解析 → 「子 Agent 触发清单」
可对既有会话回溯构建（无需新落盘）。注意（乙.3.3 观察）：meta mtime 会被 resume 触碰，
不可当派发时间用（应取子转写首行 ts）。

### 乙.3.3 并发 + 混合状态标本（已捕获，23:20–23:21）

23:20:28 在 Test2 派生第二个 teammate（`bg-progress-test-2`，green，同 taskKind），此时同会话
双 meta 并存（#1 已完成 idle、#2 运行中）。用户手机实测（约 23:21:14）：

```
◉ bg-progress-test   25:18 · 40.94万   ← zombie（已完成，判据失明仍在走）
◉ bg-progress-test-2  0:46 ·  6.37万   ← 真活跃
```

- 双 chip 同屏、spawnTs 升序（#1 在前）✓；两枚各自独立走字 ✓（spec §6 多卡片形态真机过验）；
- **时长锚定验证**：#1 的 25:18 与其 spawn（22:55:56）到回报时刻分秒吻合——spawnTs 锚 + 客户端
  自走在 25 分钟尺度上准确；
- 当前实现下 zombie 与活跃 chip 并排（teammate 判据未修的如实形态）——正是甲.5 修复后的
  「活跃置顶 + 历史折叠」所欲消除的对照标本；
- agent#2 结果验证：6 步全跑、123+456=579，23:23:21 回报 + 23:23:31 idle（完成即第二 zombie）。

### 乙.3.4 MAM 事件弹窗与子 agent 的相关性（应用通知历史实录；乙.2 需求 4 的实证底稿）

实验期间用户侧不时弹 MAM 事件弹窗。从 dev 应用通知历史（localStorage `mam-notification-history`）
提取的 Test2 会话弹窗时刻表（共 7 条，其余会话条目未列）：

```
23:02:46 idle      ← agent#1 完成报告 + 回合结束（lastMessage = "Another Claude session sent a message…"）
23:09:43 idle      ← 主会话发完 SendMessage 续跑、回合结束（lastMessage = 主 agent 自己的回执复述）
23:11:23 processing┐
23:11:29 idle      ├← agent#1 二次回报前后的状态抖动
23:11:36 thinking  │
23:11:53 idle      ← agent#1 二次回报 + 回合结束
23:23:40 idle      ← agent#2 完成报告 + 回合结束
```

判定：**子 agent 纯跑动期间零弹窗**（22:56:14–23:02:26 静默窗无任何条目）。弹窗全部落在
「主会话被唤醒 → 回合结束」的边界上（状态以 idle 绿为主，伴 processing/thinking 黄的瞬时抖动，
每次颜色变化都触发浮窗，仅同色 5s 去重）。体感「跑动中也弹」的一例实证（23:09:43）实为
**主会话发完 SendMessage 后自己的回合结束**，不是子 agent 轮询活动触发——MAM 状态机只消费
主会话的事件（钩子/父 jsonl），子转写写入不进入其状态判定。

对乙.2 的支撑：7 条中 **4 条的 lastMessage 带「Another Claude session sent a message」特征**
（即「子 agent 回报转绿」），开关即针对它们；23:09:43 那条无特征（主 agent 自身动作），
开关不会误伤。
