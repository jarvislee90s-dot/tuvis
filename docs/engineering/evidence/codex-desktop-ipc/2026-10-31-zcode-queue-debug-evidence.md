# ZCode 官方输入队列：W0 证据文档（抓包脱敏 / 状态机 / 命令与 ACK / 根因核对）

- 日期：2026-10-31
- 仓库：`/Users/jarvis/Documents/tuvis-worktree2` · 分支 `codex/zcode-unified-remote`
- 文档性质：**只读证据汇总**。本文档未修改任何产品代码、未修改测试、未触碰 `AGENTS.md` / `MASTER-PLAN.md`。
- 施工单出处：`docs/superpowers/plans/2026-10-31-zcode-next-round-engineering-plan-v2.md` §2（W0）

> 本文前 §§0–7 记录 W0/W5 阶段的取证快照；W6 复审后的代码修正、自动验证与仍未完成项见 §8。§8 对当前状态有更新时，以 §8 为准。真实 ZCode 实机证据仍未取得。

> ## 0. 最重要的前置声明（如实申报，不得省略）
>
> **本环境无法操作真实 ZCode 宿主**——无该机器、无实机账号、无法运行官方 ZCode。
> 施工单 W0 要求的「实机最小抓包（脱敏）」**在本环境未能执行**。
>
> 因此本文档：
> - **不包含**任何实机抓包帧、实机 ACK 原文、实机状态迁移记录；
> - 凡涉及「官方宿主当前 build 的真实行为」的结论，一律标注证据等级 **`NOT RUN（需实机）`**；
> - 允许引用的既有静态取证是 [`2026-10-29-zcode-r0-wire-evidence.md`](./2026-10-29-zcode-r0-wire-evidence.md)，它针对 **ZCode 安装包 build `3.16.2.8769`** 的**静态**包内分析（其自述为 `class = PACKAGE`，零实连、零帧捕获）——引用时一律注明「**静态，非实机**」；
> - 本仓既有 e2e / 单测夹具的结果一律注明「**合成宿主**」（fake bridge / fake session view，非真实 ZCode）。
>
> **证据等级定义**（全文统一使用）：
>
> | 等级 | 含义 |
> |---|---|
> | `静态取证` | 来自 R0 安装包静态分析的形状结论。**不是实机观测**，只证明「代码里存在该命令名/字段名」。 |
> | `代码事实` | 本仓库 HEAD 的源码可读出的事实（行号可核对）。证明「本仓怎么写」，**不**证明官方怎么响应。 |
> | `合成测试` | 本仓单测 / e2e 在**合成宿主**（fake bridge / fake session view）上的结果。证明「本仓逻辑自洽」，**不**证明官方会这样响应。 |
> | `NOT RUN（需实机）` | 本环境无法验证，必须在真实 ZCode 上取证后才能下结论。 |
>
> **一句话结论**：本轮交付的是**队列写面的控制能力**（撤回 / 修改 / 插队 / 恢复运行）与**发送可靠性修复**；「引导」**明确不可用**并已在代码与 UI 上关闭；**队列的真实状态机迁移与真实 ACK 形状至今没有任何实机证据**。

---

## 1. 基线

### 1.1 审查基线与当前 HEAD

| 项 | 值 |
|---|---|
| 审查基线 commit | `f81f062e7d5493f947c6d3dbe0dc5729c95ffb77`（简称 `f81f062e`） |
| 当前 HEAD | `1662dcc5`（`fix(zcode): close pending safety and document acceptance`） |
| 分支 | `codex/zcode-unified-remote` |
| 本文档提交 | 另起 commit，仅含本文件 |

### 1.2 本轮改动范围（`git log --oneline f81f062e..HEAD`）

| # | commit | 主题 |
|---|---|---|
| 1 | `11e5b4be` | `docs(zcode): add next-round queue plan and change proposals` |
| 2 | `4fb16ff6` | `fix(zcode): make send admission and session views consistent` |
| 3 | `db1f71e3` | `fix(zcode): keep newer session content when a stale view degrades` |
| 4 | `8d42fcae` | `fix(zcode): let a degraded view recover ready and capabilities` |
| 5 | `c23bc41c` | `fix(zcode): recover degraded views without a revision, keep Hub content` |
| 6 | `fca19c4b` | `feat(zcode): project queue write availability and dispatch guarded queue commands` |
| 7 | `751aa960` | `feat(zcode): open queue edit/resume as candidates on the evidenced build` |
| 8 | `600a5eb8` | `feat(zcode): queue row actions, edit-to-draft and resume on mobile` |
| 9 | `1662dcc5` | `fix(zcode): close pending safety and document acceptance` |

改动规模（`git diff --stat f81f062e..HEAD`）：**75 个文件、+13765 / −367**。其中核心代码集中在：

- `src-tauri/src/services/zcode_control/`：`types.rs`、`command.rs`、`projection.rs`、`hub.rs`、`action.rs`、`pending.rs`、`capability/*`
- `src/mobile/`：`ZcodeQueuePanel.tsx`、`ZcodeControlPanel.tsx`、`useZcodeControl.ts`、`ZcodePendingCard.tsx`、`queuePreview.ts`（新增）
- `src/lib/api/zcodeControl.ts`
- 文档：`docs/user-manual/{zh,en}/08-mobile-remote.md`、`docs/superpowers/reviews/2026-10-31-zcode-queue-screenshots/README.md`（+ 38 张截图与 `metrics.json`）

### 1.3 本轮实际跑过的验证（本环境，合成宿主）

| 验证 | 命令 | 结果 |
|---|---|---|
| Rust 单测（zcode_control 全模块） | `cargo test --lib zcode_control::` | **639 passed / 0 failed**（合成宿主） |
| Rust 单测（command 子模块） | `cargo test --lib zcode_control::command` | **117 passed / 0 failed**（合成宿主） |
| 移动端队列面板 | `vitest run tests/mobile/ZcodeQueuePanel.test.tsx` | **27 passed**（合成宿主） |
| 移动端 choice / pending / revision / 控制面板 / hook / api | `vitest run`（6 个文件） | **133 passed**（合成宿主） |

合计：**160 个移动端用例 + 639 个 Rust 用例通过**。**这些全部是合成宿主结果，不代表官方宿主行为。**

> 说明：首轮 `vitest` 以默认 `--pool=forks` 启动时出现 `[vitest-pool-runner]: Timeout waiting for worker to respond`（worker 启动超时，**非用例失败**，`Test Files no tests / Errors 4`）。改用 `--pool=threads --no-file-parallelism` 后全部通过。这是本机 vitest worker 启动环境问题，与被测代码无关。

---

## 2. 状态机：官方队列的已知形状

### 2.1 已知形状

| 状态 | 官方字段 | 来源 | 等级 |
|---|---|---|---|
| 队列容器存在 | `state.queue = {items[], autoDrain, pauseReason?}` | R0 §3（`XYX@640428`；`SCH@599070`；`CLI@793871` 三处一致） | `静态取证` |
| 项已入队 | `items[].dispatch.state = "queued"` | R0 §3（`XYX@603372` `Df={state:["admitted","queued","reserved","promoting","drained"]}`） | `静态取证` |
| 逐项状态机 | `queued → reserved → promoting → drained` | R0 §3 | `静态取证` |
| 队列被暂停 | `autoDrain=false`（**容器级**） | R0 §3（`CLI@14612939`、`CLI@14614082` 两处 `{…queue,autoDrain:!1,pauseReason:"stopped"/"error"}`） | `静态取证` |
| 暂停原因枚举 | `pauseReason ∈ {stopped, manual, error}` | R0 §3（`XYX@640428`） | `静态取证` |
| `stopped` / `error` 的写入点 | 轮次被取消且队列非空 → `stopped`；轮次失败且队列非空 → `error` | R0 §3（`CLI@14612939`/`CLI@14614082`） | `静态取证` |
| `manual` 的写入点 | **未找到**。枚举里存在，但 `zcode.cjs` 内只找到 `stopped`/`error` 两处写入 | R0 §6 第 1 条 | `静态取证`（**写入点未证实**） |
| 队列已恢复 | `autoDrain=true` | R0 §3 容器 schema | `静态取证` |
| 投递总闸 | `state.inputRouting.mode ∈ {startNow, enqueue, guide, reject, choice}` | R0 §3（`XYX@639779`） | `静态取证` |

### 2.2 施工单原文里的 `queued → interrupted → paused → send → resume` 与已知形状的对应关系（如实对照）

施工单 §2 观测点 1–2 描述的是一条**用户可感知的时序**。把它拆成「宿主有没有对应字段」：

| 施工单用词 | 宿主是否真有这个状态字段 | 依据 |
|---|---|---|
| `queued` | **有**：`dispatch.state = "queued"` | R0 §3（`静态取证`） |
| `interrupted` | **没有这个字段**。宿主侧对应的是「回合被取消」这一**事件**，其**结果**落在队列容器的 `autoDrain=false` + `pauseReason="stopped"`。本仓也不存在 `interrupted` 状态枚举（`rg "interrupted"` 在 `src/`、`src-tauri/src/` 只命中 Codex 用量统计与 `turn_aborted` 文案，与 ZCode 队列无关）。 | R0 §3 + `代码事实` |
| `paused` | **有**：`autoDrain=false`（容器级）+ 可选 `pauseReason` | R0 §3（`静态取证`） |
| `send` | **有**：`sendText` 命令 + ACK `result.type="inputAccepted"` + `result.delivery ∈ {startNow,queue,guide}` | R0 §4（`静态取证`） |
| `resume` | **有（命令名层面）**：`setAutoDrain{autoDrain:true}` | R0 §3（`静态取证`） |

> **结论**：`interrupted` 是**用户侧描述**，不是官方状态机节点。官方可观测的状态节点只有 `dispatch.state`（逐项）与 `autoDrain`/`pauseReason`（容器级）。这条状态机从「被打断」到「paused」的**真实跃迁条件与时序**（例如是否必然出现 `stopped`、是否可能直接落到 `error`、是否可能 `paused` 但 `dispatch.state` 仍是 `reserved`）——**`NOT RUN（需实机）`**。

### 2.3 每个跃迁「当前由谁负责」

| 跃迁 | 当前责任方 | 依据 | 等级 |
|---|---|---|---|
| `queued`（入队） | **宿主**。本仓**不建本地队列**；手机端发送时只是发 `sendText`（运行中带 `requestedDelivery="queue"`），是否入队由宿主 `inputRouting.mode` + `delivery.admitted` 决定 | R0 §3/§4；`src-tauri/src/services/zcode_control/command.rs:1943-1946` | `静态取证` + `代码事实` |
| `queued → reserved → promoting → drained`（逐项消费） | **宿主**。本仓**只读投影** `dispatch.state`，**不**驱动、不预测 | `src-tauri/src/services/zcode_control/projection.rs:395-406`（白名单投影） | `代码事实` |
| 回合被打断 → `autoDrain=false` | **宿主**。本仓只**观察** `autoDrain` 并据此置 `paused` | `src-tauri/src/services/zcode_control/projection.rs:360-366` | `代码事实` |
| `paused` 的原因判定 | **宿主**（`pauseReason` 原样透传，本仓不猜） | `src-tauri/src/services/zcode_control/projection.rs:362-366` | `代码事实` |
| `paused → resumed`（`autoDrain=true`） | **宿主**执行；**本仓发起**（发 `setAutoDrain{autoDrain:true}`），且**只**在**新鲜**队列 `paused===false` 后才敢说「已恢复」 | `src-tauri/src/services/zcode_control/command.rs:658-667`；`src/mobile/ZcodeControlPanel.tsx:915-921` | `代码事实` |
| 撤回 / 重排 / 立即执行 | **宿主**执行（`deleteQueueItem` / `reorderQueueItem` / `sendQueuedNow`）；本仓只构造载荷、发命令、等待官方状态 | `src-tauri/src/services/zcode_control/command.rs:610-670` | `代码事实` |
| 「引导」 | **无人负责——明确关闭**。本仓**不发**任何引导命令 | `src-tauri/src/services/zcode_control/capability/key.rs:128`（`Capability::QueueNow => None`）；`src/mobile/ZcodeQueuePanel.tsx:319`（按钮 `disabled` 硬编码） | `代码事实` |

### 2.4 官方是否会在 `setAutoDrain(true)` 后自动拉起下一条 —— `NOT RUN（需实机）`

施工单 §「W4 恢复运行」与上游意见书 §10 第 2 问都追问这一点。已知的是：

- 命令名 `setAutoDrain{autoDrain:boolean}` 存在（R0 §3，`静态取证`）；
- 官方**没有** `setAutoDrain` 对应的 `availability` 字段——R0 明确记载「官方没有 `setAutoDrain` 的 availability 字段」，因此本仓**不虚构**恢复资格（`src-tauri/src/services/zcode_control/types.rs:1138-1143`，`代码事实`）。

**未知**：`setAutoDrain(true)` 在「被打断且无 activeWorks」时，是否会自动触发下一条排队内容执行？**`NOT RUN（需实机）`**。本仓的对应保守设计是：只解除暂停、**不**重发任何队列项、**不**谎称工作已开始（`src-tauri/src/services/zcode_control/command.rs:665-666`，`代码事实`）。

---

## 3. 命令与 ACK 形态

### 3.1 四个队列命令的已知 payload

下表「R0 记录的官方命令表」一列**全部是 `静态取证`**（安装包内命令表里**存在该命令名与字段名**），**不**代表实机发出去会被接受。

| 命令 | R0 记录的官方命令表（`静态取证`，非实机） | 本仓实际发出的 payload（`代码事实`） | ACK 分类（`代码事实`） |
|---|---|---|---|
| `deleteQueueItem` | `{queueItemId}` | `{"queueItemId": <id>}` — `command.rs:628` | `classify_queue_ack`：`accepted`/`duplicate`→受理；`rejected`/`failed`→拒绝；`stale`/其它→未知。**不**据此宣布删除成功 |
| `reorderQueueItem` | `{queueItemId, beforeQueueItemId: string\|null}` | `{"queueItemId": <id>, "beforeQueueItemId": <id>}` — `command.rs:648-651`。**`null` 一律拒绝**（`command.rs:639-641`） | 同上 |
| `sendQueuedNow` | `{queueItemId, modelSelection?, modelExecution?, highspeedMeta?}`（可选字段） | `{"queueItemId": <id>}` — `command.rs:656`。**不带**任何可选字段 | 同上；但**能力槽位恒关闭**（见 §3.3） |
| `setAutoDrain` | `{autoDrain: boolean}` | `{"autoDrain": true}` — `command.rs:666` | 同上 |

**统一信封**（`代码事实`）：`{commandId, clientId, sessionId, type, payload, issuedAt}` — `command.rs:547-557`。走既有 `CommandPort::call` → 官方 `sendConversationCommandV4` 通道，**不新开通道**、**不**自行外套超时。

**幂等与在途纪律**（`代码事实`）：同 `actionId` 同参数 → 既有回执、**零帧**；写前**只读一次**新鲜官方队列（`command.rs:1664-1679`），拿不到就不写。

### 3.2 ACK 形状的真实状态

| 问题 | 答案 | 等级 |
|---|---|---|
| 官方 ACK 的**顶层**形状 | `{commandId, status, revisionAtDecision, result?, reasonCode?, message?, memoryEnabled?, ttftExcluded?}`，`status ∈ accepted\|rejected\|stale\|duplicate\|noop\|failed` | `静态取证`（R0 §4，`ADK@38235`） |
| 队列命令 ACK 里 `result` 的**内容** | **未知**。R0 只证实了 `sendText` 的 `result.type="inputAccepted"` / `result.delivery`；**队列四命令的 `result` 形状没有取证** | `NOT RUN（需实机）` |
| 本仓对队列 ACK 的处理 | 只读顶层 `status`。`failed` 收口为拒绝（`command.rs:511-512`），`stale`/其它 → `unknown`。**不读 `result`** | `代码事实` |
| 本仓对**发送** ACK 的处理 | 发送走**专用**分类 `classify_send_ack`（`command.rs:473-499`）：必须 `status ∈ accepted\|duplicate` **且** `result.type=="inputAccepted"` **且** `result.inputId == 本次出站 commandId` **且** `result.delivery ∈ startNow\|queue\|guide`，否则 → `unknown`（保留草稿/附件，**禁止**自动重试） | `代码事实` |

> **一处如实申报的落地差距**：`classify_send_ack` 校验出的 `delivery` **被丢弃**——`dispatch_send` 把结果绑成 `(state, reason_code, _delivery)`（`command.rs:1962`，下划线前缀即明示忽略），且 `ActionReceipt`（`types.rs:302-308`）**没有** `delivery` 字段。因此上游意见书 §3/F2 要求的「带 `delivery` 和 `inputId` 的精简回执供 UI 提示『执行中／已排队／引导中』」**只完成了校验侧，未完成回执透传与 UI 提示侧**。当前 UI 不会区分「已受理并开始执行」与「已受理但进了队列」。这是**已知未完成项**，不是本轮遗漏的隐藏缺陷。

### 3.3 `sendQueuedNow` 是否等价于「引导」—— `NOT RUN（需实机）`

**证据等级：`NOT RUN（需实机）`。本环境无法验证，本仓不做任何等价性声明。**

- 已知的**静态**事实：`sendQueuedNow` 是官方命令表里的一个命令名（R0 §3，`静态取证`）；官方投递模式里另有一个独立的 `guide` 值（`state.inputRouting.mode` 可取 `guide`、`delivery.admitted` 可取 `guide`，R0 §3，`静态取证`）。**这两者是否等价，无任何实机证据。**
- 官方是否有「对**已有队列项**执行 `guide`」的专用入口——**未知**，`NOT RUN（需实机）`。
- 施工单 §W0 观测点 5 要求记录：`sendQueuedNow` 是否真是「引导当前回合」还是「立即开启另一条」；对现有回合 A 的影响；原 B 是否仍在队列；是否重复消息。**这四项本环境一项都未取得。**

**因此本轮的实现裁决（`代码事实`）**：

1. **UI 的「引导」按钮恒禁用**，且**结构上**不接收引导回调——组件 props 里**根本没有** `onGuide`（`src/mobile/ZcodeQueuePanel.tsx:75-100`），按钮上写死 `disabled`（`src/mobile/ZcodeQueuePanel.tsx:319`）。
2. **服务端不开放 `queue_now` 能力槽位**：`shipped_state` 对 `Capability::QueueNow` 恒返回 `None`（`src-tauri/src/services/zcode_control/capability/key.rs:128`），resolver 据此恒给 `needs_evidence`（`capability/resolver.rs:548-563` 的测试即断言此点）。
3. **前端 API 层虽保留了 `queueNow` 动作类型**（`src/lib/api/zcodeControl.ts:393`），但**全仓无任何 UI 调用它**——`rg "queueNow"` 在 `src/`、`src-tauri/src/` 的非测试命中只有类型定义、能力槽位与两处「绝不接 queueNow」的注释。

**结论（与上游意见书 §1.3 完全一致）**：**若 `sendQueuedNow` 与「引导」不等价，UI 的「引导」按钮应保持禁用——本轮实现正是如此关闭的。** 在取得实机等价性证据之前，「引导」**不开放给真实用户**，也不用「撤回＋重发」或 `deleteQueueItem + sendText(guide)` 组合去模拟。

---

## 4. 代码事实核对表：上游意见书 §1.1 / §2.1 / §3 / §4 逐条核对

> 表中「改前」列 = `f81f062e`；「改后」列 = HEAD `1662dcc5`。所有行号均已用 `sed`/`rg` 核对。
> 「对应测试」全部为**合成宿主**结果。

### 4.1 §1.1 — Q1 队列只能看，不能「引导／修改／撤回／插队」（P0）

| # | 意见书 §1.1 的根因陈述 | 根因是否成立 | 改前（`f81f062e`） | 改后（HEAD） | 落地文件:行 | 对应测试（合成宿主） |
|---|---|---|---|---|---|---|
| 1.1-1 | 本轮原设计明确限定队列只读；`ZcodeQueuePanel.tsx` 文件头注明「只读；不提供插队／撤回／立即发送」，props 只有 `queue`，无点击处理 | **成立** | 文件头自述「只读、紧凑、默认一行……不提供插队/撤回/立即发送」，组件只渲染 `queue` 一个 prop | 改为「一行一条 + 引导/修改/撤回 + 恢复运行」；props 扩为 11 个（含 `canWrite`/`queueEditAllowed`/`queueResumeAllowed`/`onEditToDraft`/`onDelete`/`onMove`/`onResume` 等） | `src/mobile/ZcodeQueuePanel.tsx:1`（新文件头）、`:75-100`（props） | `tests/mobile/ZcodeQueuePanel.test.tsx`（27 例） |
| 1.1-2 | 官方能力存在但控制层未建模：`ControlAction` 只有 `Send/Snooze/Answer/Permission/Stop/Selection`；前端 `zcodeControl.ts` 无队列动作 | **成立** | `ControlAction` 恰好 6 个变体（`types.rs:345-392`）；前端动作联合恰好 6 种（`zcodeControl.ts:285-307`） | 新增 4 个变体 `QueueDelete`/`QueueReorder`/`QueueNow`/`QueueResume`；前端新增 4 种 `queueDelete`/`queueReorder`/`queueNow`/`queueResume` | `src-tauri/src/services/zcode_control/types.rs:444-481`；`src/lib/api/zcodeControl.ts:382-395` | `command.rs` `queue_write` 模块 11 例；`tests/mobile/zcodeControlApi.test.ts` |
| 1.1-3 | 投影保留了可操作身份，但遗漏写操作所需资格与部分元数据：`SessionView` 未公开 `availability.queueEdit/sendQueuedNow`、`inputRouting.mode`、附件/执行态 | **成立** | `QueueItemView` 只有 `{input_id, text, position}`；`SessionView` 无 `availability`、无 `inputRouting` | `QueueItemView` 增 `kind`/`dispatch_state`/`has_attachments`；新增 `QueueActionAvailability{known, queue_edit_allowed, send_queued_now_allowed}`；新增 `InputRoutingView{mode, reason_code}` | `types.rs:1114-1139`（item）、`:1146-1158`（availability）、`:1170-1187`（QueueView）、`:371`（InputRoutingView）；`projection.rs:350-427`、`:438-455`、`:462-476` | `projection.rs` 队列/availability/inputRouting 用例（如 `queue_snapshot_and_delta_keep_official_item_ids`、`queue_is_unknown_before_host_declaration`、`input_routing_is_minimal_and_patch_scoped`） |
| 1.1-4 | 已有静态协议证据（`deleteQueueItem` 等五个命令名），但**有命令名不等于当前账号／宿主状态允许执行**，必须结合 `state.availability` 验证 | **成立**，且本轮**严格执行**了这条 | —（`f81f062e` 无任何队列写能力） | 候选层只认「命令条目在 R0 §3 表内」+「已知 build 精确命中」；运行时另需新鲜 `availability.queueEdit.allowed == true` / `availability.sendQueuedNow.allowed == true`；`setAutoDrain` **无** availability 字段 → **不虚构**恢复资格 | `capability/key.rs:101-140`（`shipped_state`）、`:154-156`（`build_declares_queue_command`）；`command.rs:559-574`（`require_queue_edit_allowed` / `require_queue_now_allowed`） | `capability/key.rs` `shipped_table_queue_entries_are_candidates_only`、`queue_command_evidence_requires_known_build_and_listed_command`；`capability/resolver.rs` `queue_edit_is_candidate_on_evidenced_build`、`queue_now_stays_needs_evidence`、`queue_candidates_require_evidenced_build`；`command.rs` `queue_action_rejected_when_availability_disallows`、`queue_candidate_passes_when_availability_allows` |

**§1.1 小结**：四条根因**全部成立**，本轮**全部**在代码层处理。但「官方宿主在真实会话里会接受这些命令并产生预期状态变化」——**`NOT RUN（需实机）`**。

### 4.2 §2.1 — Q2 打断后队列「僵住」（P0）

| # | 意见书 §2.1 的根因陈述 | 根因是否成立 | 改后（HEAD） | 落地文件:行 | 对应测试（合成宿主） |
|---|---|---|---|---|---|
| 2.1-1 | 官方队列是**容器级**状态：回合取消且队列非空可出现 `autoDrain=false`、`pauseReason="stopped"`；错误可致 `"error"`。**停止当前执行不代表清空或恢复队列** | **成立**（`静态取证` 层面：R0 §3 两处写入点） | 本仓按容器级建模：`paused` 由 `autoDrain` 单独决定，**与 items 长度无关**；「暂停且空」必须可下发 | `projection.rs:360-366`（暂停优先于空）；`types.rs:1164-1168`（`QueueView` 注释） | `projection.rs` `queue_snapshot_then_paused_delta_reconciles`、`queue_pause_reason_is_projected_without_guessing`；`tests/mobile/ZcodeQueuePanel.test.tsx` `paused_empty_queue_still_shows_pause_flag` |
| 2.1-2 | `ZcodeQueuePanel` 识别并显示 `paused=true` 与原因文案，但**没有恢复按钮**；`ControlAction` 没有 `setAutoDrain` 写操作 —— 这是「队列一直停着」的直接产品缺口 | **成立** | 新增 `QueueResume` 动作 + `setAutoDrain{autoDrain:true}` 命令；UI 在 `paused===true` 时**常驻**「恢复运行」（**items 为空也显示**） | `types.rs:475-480`（`QueueResume`）；`command.rs:1589-1597`（`execute_queue_resume`）、`:658-667`（payload）；`src/mobile/ZcodeQueuePanel.tsx:228-243`（恢复按钮） | `command.rs` `queue_resume_only_changes_auto_drain`、`queue_resume_allowed_with_empty_items`；`tests/mobile/ZcodeQueuePanel.test.tsx` `paused_with_empty_items_still_offers_resume`、`resume_is_disabled_without_capability_or_write_gate`、`resume_done_appears_only_after_a_fresh_unpaused_queue` |
| 2.1-3 | `ZcodeControlPanel.tsx:250-290` 主按钮只在有 foreground turn 时显式打断；停止后运行态可能消失，按钮变回发送。`command.rs:661-665` 按运行态选 `requestedDelivery="queue"` 或宿主默认路由，但**不**处理 paused、`inputRouting.mode`、`choice` 及 held queue | **成立**（改前 `command.rs` 无 `inputRouting` 投影、无 `held_queue`） | 新增 `inputRouting` 投影 + `held_queue` CAS（`resolve_held_queue`）；`choice` + 非空队列时**必须**携带 `held_queue`，否则拒绝；携带时 `expectedQueueItemIds` 必须与新鲜官方队列的**完整 ID 列表与顺序**逐项相等 | `projection.rs:462-476`；`command.rs:369-425`（`resolve_held_queue`）、`:1947-1951`（载荷展平）；`types.rs:391-400`（`held_queue` 字段） | `command.rs` `choice_send_includes_expected_held_queue_ids`、`choice_send_cas_mismatch_is_stale_and_not_retried`、`choice_send_order_mismatch_is_rejected`、`choice_with_empty_queue_sends_normally`、`held_queue_without_fresh_view_is_not_ready`；`tests/mobile/ZcodeHeldQueueChoice.test.tsx` 4 例 |
| 2.1-4 | **谨慎结论**：用户实测「发不出去」已发生，但现有代码只能证明「缺恢复能力」和「发送路由未处理队列」；具体属于宿主拒绝／接受后继续排队／连接或能力门禁失效，**仍需一次脱敏 wire 对账** | **成立，且本轮**未**把它变成「已验证」 | 本轮补齐了「缺恢复能力」与「发送路由未处理队列」两处代码缺口；但**三种可能性的归因仍是 `NOT RUN（需实机）`** | 见 §5 NOT RUN #1 | 无（不可合成） |

**§2.1 小结**：三条可由代码判定的根因**成立且已修**；第四条「究竟是哪种失败」**本环境无法归因**，仍需脱敏 wire 对账。

### 4.3 §3 — 上一轮 Review 必须合入的可靠性修复（P1）

| # | 意见书 §3 的根因陈述 | 根因是否成立 | 改后（HEAD） | 落地文件:行 | 对应测试（合成宿主） |
|---|---|---|---|---|---|
| F1 | `useZcodeControl.ts:195-203` 的 `applySessionView` 只检查 `sessionId` 和 `generation`，**不比较 `revision`**；服务端 `hub.rs:3131-3171` 把异步 GET 结果写入 `inner.last_session`，同样会覆盖已发布的新 revision | **成立**。改前 `useZcodeControl.ts` 全文 **`revision` 零命中**（`git show f81f062e:src/mobile/useZcodeControl.ts \| rg revision` → 空）；服务端 `publish_session` / `should_replace_published` 当时**不存在** | 前端新增纯函数 `resolveSessionViewLanding`（返回 `replace`/`drop`/`degrade`/`caps`/`recover` 五种落地语义）与布尔外壳 `shouldApplySessionView`；服务端新增 `should_replace_published` + `publish_session`（返回 `(applied, published)`，被拒则**静默丢弃本帧**，绝不广播倒退事件） | `src/mobile/useZcodeControl.ts:187-219`；`src-tauri/src/services/zcode_control/hub.rs:1522-1536`、`:1615-1630`、`:1446-1452`（调用点） | `tests/mobile/zcodeSessionRevision.test.ts` 18 例（`sse_rev12_then_get_rev11_is_rejected`、`degraded_current_view_accepts_recovery_even_with_lower_revision`、`stale_safety_degrade_lands_as_degrade_only` 等）；`tests/mobile/zcodeControlHook.test.tsx` `old_get_cannot_rollback_newer_sse_queue`、`stale_degrade_keeps_newer_queue_content`、`recovery_never_restores_older_content` |
| F2 | `command.rs:344-352` 的 `classify_ack` 把 `accepted\|duplicate` 直接认作受理，`dispatch_send` 忽略 `result`；前端收到 accepted 就清草稿/附件。官方要求 `result.type="inputAccepted"` 并读 `result.delivery` | **成立**。改前 `classify_ack` 存在且**只看 `status`**；`dispatch_send` 无 `result` 校验 | 新增**发送专用** `classify_send_ack`：要求 `status ∈ accepted\|duplicate` ∧ `result.type=="inputAccepted"` ∧ `result.inputId == 本次出站 commandId` ∧ `result.delivery ∈ startNow\|queue\|guide`，任一缺失 → `unknown`（保留草稿/附件） | `src-tauri/src/services/zcode_control/command.rs:473-499`（新函数）、`:1977-1979`（调用点） | `command.rs` `send_ack_requires_full_input_accepted`、`send_accepted_without_input_accepted_is_unknown`、`send_ack_never_promotes_failure_states`；`tests/mobile/ZcodeControlPanel.test.tsx` `accepted_and_unknown_keep_distinct_copy`、`newly_typed_text_survives_late_accepted`、`failure_preserves_draft` |
| F3 | `command.rs:627-668,1468-1487` 只在运行时加 `requestedDelivery="queue"`；未投影 `state.inputRouting`，也不带 `heldQueueDisposition`/`expectedHeldQueueItemIds`。官方 `choice` + 非空队列时要求明确选择并携带当前队列 IDs，否则可能 `heldQueueDispositionRequired` | **成立**。改前无 `inputRouting` 投影、`Send` 动作无 `held_queue` 字段 | 新增 `input_routing` 投影 + `HeldQueueChoice` DTO + `resolve_held_queue` CAS；前端在 `choice` + 非空队列时弹「保留/清空/取消」三选，**默认不清空** | `projection.rs:462-476`；`types.rs:391-400`；`command.rs:369-425`、`:1947-1951`；`src/mobile/ZcodeControlPanel.tsx:540-545`、`:550-560`、`:1464-1503`（弹层） | `command.rs` `choice_send_includes_expected_held_queue_ids` 等 5 例；`tests/mobile/ZcodeHeldQueueChoice.test.tsx` `choice_prompts_before_send_and_defaults_to_keep`、`choice_clear_sends_clear_disposition_exactly_once`、`choice_cancel_sends_nothing_and_keeps_draft`、`choice_with_empty_queue_sends_without_prompt_or_held_queue` |

**§3 小结**：F1/F2/F3 三条根因**全部成立且全部修复**。

> **F2 的落地差距（如实申报）**：意见书 §3/F2 的完整要求还包含「带 `delivery` 和 `inputId` 的精简回执供 UI 提示『执行中／已排队／引导中』」。本轮**只完成了校验侧**（`delivery` 被严格校验后**丢弃**），**未完成回执透传与 UI 提示侧**。详见 §3.2 末尾。

### 4.4 §4 — 上一轮 Review 的次要问题（P2）

| # | 意见书 §4 的根因陈述 | 根因是否成立 | 改后（HEAD） | 落地文件:行 | 对应测试（合成宿主） |
|---|---|---|---|---|---|
| F4 | `pending.rs:275-287` 只支持 `userInput + toolName=AskUserQuestion` 的答复；但 `ZcodePendingCard` 可能按更宽泛的 `kind=userInput` 展示可选控件 → **应把「本卡可答」按 schema subtype 与 Rust 构造函数能力投影到前端** | **成立**。改前 Rust 侧有闸、前端**没有**对应字段，UI 会先渲染可选控件、用户选完才吃到 `needs_evidence` | 新增 `PendingView.answerable` 布尔（服务端投影），与 `build_user_answer` 的冻结闸**共用同一纯判定** `is_answerable_user_input`（前端**不**另建规则）；前端 `answerable=false` 时渲染**只读面**：无选项、无提交键、无输入框 | `src-tauri/src/services/zcode_control/types.rs:830`（`answerable` 字段）；`pending.rs:264-282`（纯判定）、`:305-308`（闸复用）、`:531-567`（投影赋值）；`src/mobile/ZcodePendingCard.tsx:249-251`、`:25-28`（只读渲染） | `pending.rs` `plan_approval_is_not_a_question_auto_approval`（`pending.rs:1355`）、`MCP elicitation is not answerable`（`:1429`）、`only AskUserQuestion may be answered`（`:1442`）；`tests/mobile/ZcodePendingCard.test.tsx` `unsupported_pending_subtype_is_readonly`（`:821`）、`支持子型仍可答（answerable=true 回归）`（`:871`） |
| F5 | `types.rs:724-760` 的 `detail` 投影上限 2000 字符。长 Bash 命令可能被截断，影响审批判断。**不得默认把截断命令表现为完整命令**；优先显示「内容已截断，去官方端查看完整内容」，必要时对高风险截断审批禁用「允许」、仍保留「拒绝」 | **部分成立 → 本轮按「安全降级」口径处理**。改前**确实**是 2000 字符上限（`f81f062e:types.rs:727` `pub const MAX_PENDING_DETAIL_CHARS: usize = 2000;`），但**截断事实没有随投影下发**，前端无从判断「这是全貌还是前缀」 | 新增 `PendingView.detail_truncated` 布尔 + `project_pending_detail_bounded`（同一上限、同时算截断真值，**前端不得按字符串长度自行推断**）；前端：截断时**就地说明**并**撤掉「允许一次」**，**保留**「拒绝」与「打开官方页面」 | `types.rs:836`（`detail_truncated` 字段）、`:842`（上限常量）、`:866-921`（有界投影 `PendingDetailProjection` / `project_pending_detail_bounded`）；`src/mobile/ZcodePendingCard.tsx:253`、`:432-436`（说明）、`:572-576`（撤掉允许一次） | `types.rs` `overlong_detail_is_truncated_with_ellipsis`（`:1536`）、截断真值用例（`:1592`、`:1601`）；`tests/mobile/ZcodePendingCard.test.tsx` `truncated_permission_does_not_offer_blind_allow`（`:889`）、`detail_is_collapsed_read_only_and_never_submitted`（`:772`） |

**§4 小结**：F4 **成立且已修**（前后端共用同一判定）；F5 **上限本身早于本轮存在**，本轮新增的是**截断事实下发 + 安全降级**（「不允许一次」仍可拒绝），**未**扩展全文读取能力——按意见书 §4 的原话「若要安全扩展全文必须设计受权分片读取、体积限制和敏感数据保护」，本轮**没有**做这件事。

### 4.5 核对过程中发现的**本仓内部不一致**（不在意见书条目内，如实申报）

| # | 发现 | 证据 | 影响 | 处置 |
|---|---|---|---|---|
| 4.5-1 | **前后端对「可写队列项」的 `dispatch.state` 白名单不一致**：前端 `WRITABLE_DISPATCH_STATES = {admitted, queued, reserved}`（`src/mobile/ZcodeQueuePanel.tsx:45`，用于 `dispatchWritable`，`:66-68`）；后端 `require_writable_queue_item` 要求 `dispatch_state == "queued"` **恰好等于**，其余（`admitted`/`reserved`/未知）一律 `invalid_action`（`src-tauri/src/services/zcode_control/command.rs:594-596`） | `代码事实` | 宿主若把项置为 `admitted` 或 `reserved`，手机端按钮**可点**（前端放行），点下去服务端**拒绝**（`invalid_action`），UI 表现为一次「结果不确定 / 操作在当前状态下无效」的失败。**不会**造成误删或误移（服务端 fail closed），但会给用户一次无谓的失败尝试。 | 本轮**不修**（本轮只写文档）。方向应是**收紧前端白名单到 `{queued}`** 与后端对齐，或反过来放宽后端——但后端放宽涉及「`reserved` 项能否安全撤回」的官方语义，属**实机问题**，应并入 §5 NOT RUN #12 一并取证。**当前状态下这是已知的功能不一致，不是安全漏洞。** |
| 4.5-2 | **F2 的 `delivery` 未透传到回执/UI**（详见 §3.2 末尾与 §4.3 的申报） | `代码事实` | 用户看不到官方投递结论（「执行中」vs「已排队」）。草稿安全性**不受影响**（校验侧已严）。 | 本轮**未完成**（已知未完成项，非隐藏缺陷）。 |

> **说明**：4.5-1 是本轮核对时**新发现**的、意见书未覆盖的不一致。本文档**如实记录**，但**不在本轮修改代码**（本轮授权范围是「只写文档」）。

---

## 5. NOT RUN 清单（仍需实机验证）

> 下列每一项在本环境**都无法**验证。每条写明「需要什么才能验证」。
> **在全部这些项被实机取证之前，本仓对相应能力一律保持保守关闭**（见 §6）。

| # | 待验证项 | 状态 | 需要什么才能验证 |
|---|---|---|---|
| 1 | **暂停后恢复的真实 ACK**：`setAutoDrain{autoDrain:true}` 的实机回执形状（顶层 `status`、`result` 内容、`reasonCode`），以及恢复后队列是否真的 `autoDrain=true`、下一条是否自动开始执行 | `NOT RUN（需实机）` | 真实 ZCode 宿主上：① 运行 A → 排 B/C → 打断 A → 观察到 `paused` → 发 `setAutoDrain(true)` → 抓取**脱敏** ACK 原文（只记 `status`/`result.type`/`result.delivery`/原因码，不记正文）→ 再抓一帧新鲜 `state.queue` |
| 2 | **`reorder` 的目标语义**：`beforeQueueItemId` 的真实含义（是「插到该项之前」还是「该项移到该位置」）；`null` 是否真表示「移到末尾」；官方是否接受「下移」这种需要「后继项的前驱」表达的目标 | `NOT RUN（需实机）` | 真实宿主上对同一会话的**三条**队列项分别执行：移到最前 / 上移 / 下移，逐次抓取命令 payload 与**随后的 `state.queue.items` 顺序**做对账；并单独验证一次 `beforeQueueItemId=null`（本仓目前一律拒绝，见 `command.rs:639-641`） |
| 3 | **附件项的删除语义**：带 `attachments[]` 的队列项执行 `deleteQueueItem` 是否被宿主接受；删除是否连带清理附件引用；本仓已**只投影 `hasAttachments` 布尔**、不下发附件对象 | `NOT RUN（需实机）` | 真实宿主上构造一条**带附件**的队列项 → 发 `deleteQueueItem` → 记录 ACK 与队列是否移除该项；同时核对官方网页端该附件的残留状态 |
| 4 | **`choice` 模式真实 CAS 反馈**：官方对 `heldQueueDisposition` + `expectedHeldQueueItemIds` 的**真实**接受/拒绝行为与原因码（尤其 CAS 不匹配时官方返回什么） | `NOT RUN（需实机）` | 真实宿主上把 `inputRouting.mode` 切到 `choice` 且队列非空 → 手机侧选择「清空并发送」→ 抓取信封的 `heldQueueDisposition`/`expectedHeldQueueItemIds` 与真实 ACK；再做一次**故意过期**的 CAS（先在官方端改动队列再发送）→ 记录官方原因码 |
| 5 | **「引导」等价性**：`sendQueuedNow` 究竟是「引导当前回合」还是「立即开启另一条」；是否终止当前工作；是否可在运行中执行；是否改变队列；原 item 是否仍在队列；有无消息重复；官方是否有「对已有 queue item 执行 `guide`」的专用入口 | `NOT RUN（需实机）` | 真实宿主上：一条正在跑 + 两条排队 → 对第一条执行 `sendQueuedNow` → 同时记录：当前回合如何变化、原 item 是否仍在队列、有无重复消息、是否阻断当前执行；**并**在官方网页端找一次真正的「引导」交互，对比其发出的命令名与载荷，据此判定两者是否等价。**在此之前「引导」保持禁用**（本轮已如此） |
| 6 | **2000 字截断的宿主实际长度**：真实 Bash 审批 `payload.detail` 的典型长度分布；2000 字符上限在本场景是否够用；截断后「不允许一次」的降级会不会让**本可安全批准**的短命令被误伤 | `NOT RUN（需实机）` | 真实宿主上收集一批脱敏的 `payload.detail` **长度**（只记长度与是否超过 2000，不记命令原文），统计分布；并在官方网页端比对同一张卡的完整正文长度 |
| 7 | **`availability` 的真实字段名**：官方 `state.availability` 是否真在**状态根**、键名是否恰为 `queueEdit` / `sendQueuedNow`、值是否恰为 `{allowed: true} \| {allowed: false, reasonCode}`；以及 `setAutoDrain` 是否**真的没有**对应 availability 字段 | `NOT RUN（需实机）` | 真实宿主上抓一帧**含 `availability`** 的快照/增量（脱敏），逐字核对字段路径与形状；并实测一次「官方不允许 queueEdit 时发 `deleteQueueItem`」的真实 ACK 与原因码，验证本仓「缺字段 = 不放行」的保守策略是否与官方一致 |
| 8 | 队列四命令 ACK 里 `result` 的**实际内容**（R0 只证实了 `sendText` 的 `result`） | `NOT RUN（需实机）` | 与 #1/#2/#3 同批抓包，额外记录 `result.type` 的取值集合（脱敏） |
| 9 | `pauseReason:"manual"` 是否有真实写入点（R0 明确未找到） | `NOT RUN（需实机）` | 真实宿主上**手动**暂停队列 → 观察 `pauseReason` 是 `manual` 还是别的值；本仓当前对非枚举值一律 `None`（不猜） |
| 10 | `interrupted → paused` 的**真实跃迁条件**：打断后是否**必然**出现 `paused=true` + `pauseReason="stopped"`，还是可能直接落到 `error`，或 `paused` 但项的 `dispatch.state` 已是 `reserved` | `NOT RUN（需实机）` | 真实宿主上多次执行「运行 A → 排队 B/C → 打断 A」，逐次记录 `state.queue` 的 `autoDrain`/`pauseReason` 与**每项** `dispatch.state` 的组合 |
| 11 | 真实会话里四命令是否真被官方接受（命令名存在 ≠ 账号/宿主状态允许） | `NOT RUN（需实机）` | 在真实账号 + 真实会话上逐条执行 `deleteQueueItem`/`reorderQueueItem`/`setAutoDrain`，记录真实 ACK 与状态迁移；`sendQueuedNow` 因能力槽位恒关，需先在官方网页端触发一次以确认其真实语义（与 #5 合并） |
| 12 | **`dispatch.state` 各值的真实可写性**：官方是否会把项置为 `admitted`/`reserved`；这两种状态下撤回/重排是否被宿主接受（关系到 §4.5-1 的前后端白名单该往哪边收紧） | `NOT RUN（需实机）` | 真实宿主上让一项停在 `admitted` 或 `reserved`（例如打断瞬间抓一帧 `state.queue`）→ 对该项发 `deleteQueueItem`/`reorderQueueItem` → 记录官方 ACK 与队列变化，据此决定前端白名单是否应收紧到 `{queued}` |

**NOT RUN 合计：12 项。**

---

## 6. 结论

### 6.1 本轮**已可在上游交付**的能力

| 能力 | 交付形态 | 证据等级 | 风险 |
|---|---|---|---|
| **撤回**（`deleteQueueItem`） | 手机端队列行「撤回」按钮；发命令后**不**乐观更新，等**新鲜队列**确认项消失才显示「已撤回」；等不到确认显示「结果不确定」并刷新 | 实现与测试：`代码事实` + `合成测试`；官方接受性：`NOT RUN（需实机）` | 低—中。幂等由 `actionId` + 项 ID 指纹保证；不自动重发；失败可刷新重来 |
| **修改**（= 撤回 + 回填完整原文） | 先发 `deleteQueueItem`；**官方确认该项消失后**才把**完整原文**写回输入框；**绝不自动发送**；已有草稿先问「合并／替换／取消」默认取消；等待期间用户又改输入框会**再问一次**；带附件的项**只保留撤回** | 同上 | 低—中。语义**不是**官方 `editQueueItem{newText}` 原位修改（意见书 §1.3 已明示，本轮亦未使用 `editQueueItem`） |
| **插队／重排**（`reorderQueueItem`） | 行首序号点开菜单：「移到最前／上移／下移」；目标一律**显式官方项 ID**，**绝不**传 `null`、**绝不**按数组下标定位 | 同上；`null` 语义：`NOT RUN（需实机）` | 中。「下移」需要「后继项的前驱」来表达，末项的「下移」因此**被禁用**（无法在不传 `null` 的前提下表达「移末尾」） |
| **恢复运行**（`setAutoDrain{autoDrain:true}`） | 队列 `paused===true` 时**常驻**按钮（**items 为空也显示**）；只解除暂停、**不**重发队列项、**不**重启被打断的回合；只有**新鲜**队列 `paused===false` 后才显示「队列自动处理已恢复」 | 同上 | 中。**不**保证下一条已经在跑（文案已声明）；真实 ACK 与是否自动拉起：`NOT RUN（需实机）` |
| **发送可靠性**（F1/F2/F3） | 旧 revision 不再覆盖新内容；发送只在完整 `inputAccepted` 四元组成立时才算受理（否则保留草稿）；`choice` 模式先让用户选「保留／清空」并带 CAS，默认不清空 | `代码事实` + `合成测试` | 低。F2 的**回执透传/UI 提示**侧未完成（见 §3.2） |
| **审批卡安全降级**（F4/F5） | 不可作答的子型显示只读卡（无选项/无提交/无输入框）；正文被截断时说明并撤掉「允许一次」，保留「拒绝」 | `代码事实` + `合成测试` | 低。2000 上限**沿用旧值**，宿主真实长度分布未知（`NOT RUN（需实机）`） |

### 6.2 本轮**明确不可用**的能力

| 能力 | 状态 | 依据 |
|---|---|---|
| **「引导」** | **明确不可用、明确关闭**。UI 按钮恒灰且**结构上**无回调通道（组件根本不收 `onGuide`）；服务端 `queue_now` 能力槽位恒 `None`/`needs_evidence`；前端 API 虽保留 `queueNow` 类型但**全仓无 UI 调用** | `代码事实`：`capability/key.rs:128`、`capability/resolver.rs:548-563`、`src/mobile/ZcodeQueuePanel.tsx:319`、`:75-100` |
| 「立即执行」（`sendQueuedNow`） | **代码路径存在但能力槽位恒关**，实际不可用。原因：其「立即执行」语义是否等同「引导」没有实机证据 | `代码事实`：`capability/key.rs:105-106` |
| 「恢复队列后自动拉起下一条」 | **不提供**。本仓只发 `setAutoDrain(true)`，**不**在恢复后连续派发队列项（避免重复/改变执行时序） | `代码事实`：`command.rs:665-666` |
| 「用「撤回＋重发」模拟引导」 | **明确禁止**，本轮未实现 | `代码事实`：`ZcodeQueuePanel.tsx:140` 注释与 `:319` |
| 审批正文**全文**扩展读取 | **未实现**（按意见书 §4 要求需先设计受权分片、体积限制与敏感数据保护） | `代码事实`：`types.rs:842` 仍为 2000 上限 |

### 6.3 用户可见后果（诚实说明）

1. **手机上确实多了能按的队列按钮**——撤回、修改、插队、恢复运行都能点，且每个动作都有真实的官方命令在背后。但**按钮按下去之后官方会怎么反应，仍未在真实机器上验证过**。最坏情况是官方拒绝（ACK `rejected`/`failed`）→ 手机显示错误并刷新，**不会**造成数据丢失或重复发送（幂等 + 不乐观更新 + 不自动重发）。
2. **「引导」对用户是灰的**。用户想要「让排队内容插到正在跑的回合里」，**必须回官方端**。这是一个**明确的功能缺口，不是 bug**——本仓用「灰按钮 + 常驻原因说明」表达，而不是做一个语义可能错误的假入口。
3. **「恢复运行」不等于「工作继续了」**。UI 文案与用户手册都明说：它只恢复队列的**自动处理**，**不重启**被打断的回合，也**不保证**下一条已经在跑。若官方 `setAutoDrain(true)` 实际不会拉起队列，用户会看到「已恢复自动处理」但仍无进展——**这正是 NOT RUN #1 要查清的事**。
4. **发送回执不会区分「已执行」与「已排队」**。F2 的校验侧已严（不完整 ACK 就不清草稿），但 `delivery` 未透传到 UI，用户**看不到**官方投递结论。这是一条**已知未完成项**。
5. **末项没有「下移」**。因为「移到末尾」在官方命令里只能用 `beforeQueueItemId=null` 表达，而该语义未经实机确认，本仓一律拒绝。这是**保守取舍**，代价是末项无法下移。
6. **审批卡正文超过 2000 字符时不能「允许一次」**。这是有意的安全降级——宁可让用户去官方端看，也不给「看着像完整命令、其实被截断」的一次批准机会。副作用是：真实 Bash 命令若确实很长，用户必须切到官方端操作。
7. **一种情况下按钮可点但会被服务端拒绝**。宿主若把队列项置为 `admitted` 或 `reserved`（而非 `queued`），手机端「修改／撤回」按钮**显示为可点**，点下去服务端会拒绝（§4.5-1 的前后端白名单不一致）。用户会看到一次「结果不确定 / 操作在当前状态下无效」的失败，**不会**误删或误移。这是已知的功能不一致，处置方向见 §4.5-1。

### 6.4 交付建议（据本轮证据强度）

| 建议 | 理由 |
|---|---|
| **可以交付**：撤回 / 修改 / 插队 / 恢复运行 / 发送可靠性修复 / 审批卡安全降级 | 代码路径完整、幂等与在途纪律成立、合成测试覆盖关键分支；风险以「官方拒绝 → 显示错误并刷新」为主，无静默数据丢失路径 |
| **必须保持关闭**：引导 / 立即执行 | 等价性无实机证据（NOT RUN #5、#11） |
| **建议随下一轮实机联调一并验证**：§5 全部 12 项 | 尤其 #1（恢复的真实 ACK）、#2（reorder 目标语义）、#5（引导等价性）——这三项直接决定「引导」能否开放 |

---

## 7. 引用与来源

| 来源 | 性质 | 用途 |
|---|---|---|
| `docs/superpowers/reviews/2026-10-29-zcode-r0-wire-evidence.md` | **静态取证**（build `3.16.2.8769` 安装包分析，零实连零帧捕获） | §2 状态机形状、§3 命令表与 ACK 顶层形状、`availability` / `inputRouting` / `pauseReason` 字段路径 |
| `docs/superpowers/reviews/2026-10-31-zcode-next-round-review-and-change-proposals.md` | 上游意见书（本轮根因来源） | §4 的 §1.1/§2.1/§3/§4 逐条核对 |
| `docs/superpowers/plans/2026-10-31-zcode-next-round-engineering-plan-v2.md` | 施工单 | §2 观测点、§3 命令清单、§5 NOT RUN 项来源 |
| `docs/superpowers/reviews/2026-10-31-zcode-queue-screenshots/README.md` | **合成宿主**视觉证据（38 张截图 + CDP 量测） | 仅用于说明 UI 形态；其自述第 5 节已声明「这不是实机证据」 |
| 本仓 HEAD `1662dcc5` 源码 | **代码事实** | §4 全部行号 |
| `cargo test --lib zcode_control::` / `vitest run` | **合成宿主** | §1.3、§4 测试列 |

**本文档未做的事**：未修改产品代码、未修改测试、未运行官方 ZCode、未产生任何实机抓包、未 push。
## 8. W6 独立复审后的修正与当前边界

> 历史修正：§4.5-1 与 §6.3 第 7 项记录的是 W0 快照中的前后端 dispatch 白名单不一致；当前实现已通过共用 `{queued, admitted, reserved}` 集合修正该差异。`reserved` 状态真实可撤回仍是 NOT RUN（§5 #12）。

W6 复审基线 `f81f062e..4c4050ec` 发现 HTTP 动作白名单漏了新协议字段、发送假 ACK 不符合 F2、修改回填缺乏删除因果证据、CAS 失配后没有刷新重选，以及发送投递提示可能误认队列项。复审者又构造了“另一端同时加入相同前缀正文”的场景；当前修正已停止对发送投递作本地推测。修正记录如下：

| 发现                                                       | 当前修正                                                                                                                       | 合成验证                                                                                                                                                                                              |
| ---------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| HTTP `ACTION_KEYS` 未登记 `heldQueue` 与四种 `queue*` 动作 | `src-tauri/src/remote/zcode.rs` 允许这些合同字段，并对子对象 `heldQueue` 做严格字段白名单                                      | `queue_actions_and_held_queue_send_cross_http_once_each`：请求经 HTTP/Hub/命令链，5 项各恰一帧并逐字段核对载荷                                                                                        |
| 「修改」把「队列项消失」误当作已撤回                       | `waitForQueueItemAbsent` 只报告项目当前是否存在；消失时不改草稿、不自动回填/发送，提供完整原文复制入口；中英手册已同步降级说明 | `edit_does_not_restore_when_item_disappears_without_causal_delete_evidence`、`edit_preserves_an_existing_draft_and_offers_original_for_manual_copy`、`draft_written_during_wait_is_never_overwritten` |
| F2 后合成宿主仍只回 `status=accepted`                      | `tests/common/zcode.rs` 的默认 `sendText` ACK 现在按当前 `commandId` 生成 `inputAccepted + inputId + delivery`                 | `cargo test --locked --features marker-helper`（3613 passed / 38 ignored）及 `cargo test --locked --tests --features marker-helper,zcode-bridge-evidence`（3632 passed / 38 ignored）                 |
| heldQueue CAS 失配后只显示通用错误                         | 手机收到 `invalid_action` 后拉取同会话/同代新鲜视图，仅当完整队列 ID 顺序变化时重开选择器；不自动重发                          | `cas_mismatch_refreshes_queue_and_requires_a_new_choice`                                                                                                                                              |
| 未关联 delivery 的发送用正文/队列项推测是否排队            | 已删除投递推测；ACK `accepted` 始终显示中立「已受理」，官方队列面板独立呈现队列事实                                            | `accepted_send_stays_neutral_when_same_text_item_cannot_be_attributed`（旧推测先红后绿）、`accepted_send_keeps_neutral_status_when_queue_is_unchanged`                                                |

**W6 门禁结果**：`pnpm check`、`pnpm build:mobile`、`pnpm test`（200 files / 2503 passed / 5 skipped）、两组 Clippy、Rust 格式检查和 `git diff --check` 均退出 0；Rust `cargo check --locked`、`cargo test --lib services::zcode_control::`（640/640）及上述两组 feature suites 均通过。完整前端测试有 jsdom `HTMLMediaElement` “Not implemented” 提示但退出 0；`pnpm check` 另有一条 `useEffect` ref lint warning（0 errors）；构建只报大 chunk advisory。

**仍未完成**：F2 的官方 `delivery` 仍未持久化/透传到 UI，当前 UI 不推测「已排队/执行中」，只按回执显示「已受理」；「引导」保持关闭；真实宿主命令 ACK、并发时序、暂停恢复是否拉起下一项等 §5 十二项依旧 `NOT RUN（需实机）`。队列消失仍不构成删除因果，因此「修改」是安全退化的「撤回请求 + 人工复制」，尚不能等同用户最初要求的自动回填。

## 9. 后续独立复审发现的四项修正（2026-11-01）

> 本节更新 §8 后的新 review；当前代码状态以本节为准。仍没有真实 ZCode 宿主证据。

| 发现 | 根因与修正 | 自动回归 |
|---|---|---|
| 同 revision 恢复快照把 paused queue 留空 | `degrade_view` 为安全关写会清队列；`merge_degraded_current` 对同 revision 只恢复 `ready/capabilities`。现在同会话、同代、同 revision 且恢复快照重新宣告 `queue.known=true` 时，合并回来队列及暂停态；更旧 revision 仍保留降级内容 | `same_revision_recovery_restores_cleared_queue_and_pause_state`（修改前红、修正后绿） |
| 静态命令名在普通构建开放 queueEdit/queueResume，`reserved` 也可写 | 普通构建中 `shipped_state(QueueEdit/QueueResume)=None`；只有显式 `zcode-bridge-evidence` debug probe build 可按静态形状进入候选。写状态集合收为 `{queued, admitted}`，`reserved` 在手机与 Rust 命令层均拒绝；它在测试中的官方形状保留为可识别值 | `queue_edit_and_resume_stay_closed_outside_w0_evidence_build`、证据构建下 `queue_edit_is_candidate_on_evidenced_build`、`queue_delete_accepts_admitted_state`（同时锁 reserved 零帧拒绝）、`reserved_item_stays_disabled_without_host_evidence` |
| 手机撤回/修改/插队未检查 Tuvis `queueEdit` 能力 | 前端资格改为 Tuvis `capabilities.queueEdit.enabled` 与官方 `availability.queueEdit.allowed` 两层均开；任一关时操作按钮禁用并给关闭原因 | `queue_edit_actions_require_both_tuvis_capability_and_host_availability` |
| 审批直接调用 hook.submit，绕过面板 `inFlightRef` | 问答/审批统一使用 `runAction`，队列面板同时接收共享 `writeBusy`。其他写动作在途时队列按钮置灰，重入仍由同步 ref fail closed | `permission_and_queue_writes_share_the_same_in_flight_slot` |

**未改变的边界**：队列 capability 只在显式 evidence/probe 构建可能进入候选；普通构建保持关闭。候选并不证明真实宿主接受命令，§5 原有 12 项继续 `NOT RUN（需实机）`。原始自动回填需求仍是部分完成，不因队列状态修复而升级为完成。

## 10. W6 第二轮复验摘要（2026-11-01）

最新回归覆盖本文件 §9 四项：same-revision recovery restoring paused queue、普通构建 capability 关闭及 evidence 构建候选、`reserved` 写入拒绝、手机双层能力门、审批与队列共享单写槽。定向结果为 Rust zcode_control **641 passed**、移动端相关套件 **148 passed**；完整套件结果以主复审报告 §12.3 的最终门禁更新为准。

中英手册已补充 W0 队列写停止门：真实宿主证据未闭合时普通构建只读，静态命令表不构成生产开闸依据。本文历史 §5 中的 12 项实机检查均保持 `NOT RUN`；本轮没有运行真实 ZCode，也没有抓取真实宿主帧。
