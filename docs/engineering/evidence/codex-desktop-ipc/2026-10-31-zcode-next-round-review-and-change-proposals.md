# ZCode 手机远控：实机问题根因与下一轮修改意见（PR #190）

> **日期**：2026-10-31  
> **仓库／审查分支**：[`jarvislee90s-dot/tuvis` PR #190](https://github.com/jarvislee90s-dot/tuvis/pull/190)，`codex/zcode-unified-remote`  
> **代码基线**：`f81f062e7d5493f947c6d3dbe0dc5729c95ffb77`（本次读取时 HEAD）  
> **上位设计**：`docs/superpowers/plans/2026-10-29-zcode-session-ui-repair.md`（U1–U7）  
> **协议证据**：`docs/superpowers/reviews/2026-10-29-zcode-r0-wire-evidence.md`（针对当时 ZCode 3.16.2.8769 安装包的静态取证；**不是本次实机抓包**）  
> **目的**：汇合用户这次实机反馈、上一轮 Review 的 P1/P2 发现，形成可交付下一轮开发的意见。**本文件为设计／修改建议；未修改产品代码、未运行测试、未证明官方宿主实机行为。**

## 0. 本轮最终目标、优先级与边界

**最优先目标**：让远程用户可控制官方输入队列、在中断造成队列暂停后恢复执行，同时杜绝“发送显示成功但未真正受理／状态倒退／旧队列导致发送失败”的问题。新增模型体验功能按用户意愿作为可选项，不阻塞主线交付。

| 等级 | 主题 | 本轮建议 |
|---|---|---|
| **P0** | Q1 队列“引导／修改／撤回”＋插队；Q2 暂停队列“恢复运行” | 必做；需包含前后端与宿主能力门，不是只画按钮 |
| **P1** | F1 同代际旧 GET 覆盖新 SSE；F2 发送 ACK 误判；F3 `choice` 模式发送拒绝 | 随 P0 一起修；特别是 F2/F3 直接影响发送与队列可靠性 |
| **P2** | F4 非 `AskUserQuestion` 卡错误地可编辑；F5 Bash 审批正文仅 2000 字截断 | 修正或明确产品降级、安全约束 |
| **可选 A** | O1 供应商 → 模型二级选择 | 先做受控协议可行性验证，再决定是否开发 |
| **可选 B** | O2 模型左侧上下文占用圆环 | 先证实“实际上下文使用量／容量”的可信来源，否则不显示伪精确百分比 |
| **可选 C** | O3 macOS Keychain／安全 Cookie 反复确认 | 优先定位弹窗来源；能减提示但不承诺绕过 OS 授权或重装后绝对零提示 |

**不变约束**：只依赖官方真实队列和命令，不另造本地待发送队列；写动作必须走现有配对、目标会话、代际、能力和幂等边界；不读取或打印 JWT、Keychain 秘密、浏览器 Cookie、队列完整正文；不自动替用户扩大权限或更换安全策略；不把本轮可选功能的探测结果当成已具备的能力。

---

## 1. Q1：排队项目只能看，不能“引导／修改／撤回／插队”（P0）

### 1.1 已证实的代码根因

1. **本轮原设计明确限定队列只读**。`src/mobile/ZcodeQueuePanel.tsx` 文件头注明“只读；不提供插队／撤回／立即发送”；组件 props 只有 `queue`，列表只显示编号与完整正文，无点击处理函数。
2. **官方能力存在，但控制层未建模**。`src-tauri/src/services/zcode_control/types.rs:342–395` 的 `ControlAction` 只有 `Send/Snooze/Answer/Permission/Stop/Selection`；`src/lib/api/zcodeControl.ts:284–307` 也没有队列动作，故手机端没有合法可调用的操作。
3. **队列投影保留了可操作身份，但遗漏写操作所需的资格和部分元数据**。`QueueItemView` 已保留官方 `queueItemId`（以 `inputId` 暴露）、原文与 `queuePosition`，`projection.rs:313–363` 解析 `state.queue`。但 `SessionView` 未公开官方 `state.availability.queueEdit/sendQueuedNow`、`state.inputRouting.mode`、队列项附件／执行态等信息，前端无法判断操作何时允许。
4. **已有静态协议证据**见 [R0 wire §3](https://github.com/jarvislee90s-dot/tuvis/blob/f81f062e7d5493f947c6d3dbe0dc5729c95ffb77/docs/superpowers/reviews/2026-10-29-zcode-r0-wire-evidence.md#L110-L150)：`deleteQueueItem`、`reorderQueueItem`、`editQueueItem`、`sendQueuedNow`、`setAutoDrain`。**有命令名不等于当前账号／宿主状态允许执行**，必须结合 `state.availability` 验证。

### 1.2 UI 合同（准确按用户需求）

展开队列后，**每项恰一行**：

`#1  修复登录状态后继续运行…        [引导] [修改] [撤回]`

- **序号**：优先真实 `order.queuePosition + 1`；缺失时不要假造“官方序号”，可显示“—”或仅按视觉列表顺序做无权威标识。
- **正文**：按 **20 个 Unicode 字符／用户可见字素（grapheme clusters）** 截断，超出尾附 `…`；换行转空格以便一行展示。保留 `title`／无障碍完整文案，禁止从被截断文字构造任何提交载荷。
- **按键顺序和名称固定为 `[引导] [修改] [撤回]`**。窄屏时优先小字号、短标签和横向空间分配；实在放不下允许整行响应式布局，但不可遮挡正文或失去操作。
- **队列重排（插队）**：用户需求还包括调整位置；建议采用行内拖动手柄或长按拖动，不增加第四个文字按钮。移动后显示真实新队列位置。若触屏拖动不可靠，退化为可访问的移动菜单（“移到最前／上移／下移”），不能仅实现“引导”就声称完成了“重排”。
- **按钮门控**：来自官方 `availability`、当前 `ready`、代际及精确队列项状态；不允许时禁用并显示原因。每个 item 单独 loading，别的项目和草稿不得误受影响；处理高频 SSE 不跳焦点。

### 1.3 三个按钮各自的准确语义

| 按键 | 期望行为 | 代码实现建议与注意事项 |
|---|---|---|
| **引导** | 将该条排队消息优先作用于当前工作，而不是普通排队等待 | **先实机证实语义**：R0 有 `sendQueuedNow{queueItemId}`，可作为候选；但“立即执行”和官方 `guide`（对当前运行的 turn 进行 steer）未必等价。**不能直接把 `sendQueuedNow` 冒称 `guide`**。验证它是否会终止当前工作、是否可在运行中执行、是否改变队列。如果达不到用户所指的“引导”，要找到宿主真实可用的引导命令／选择相应交互；否则禁用并说明，不能使用“撤回＋重发”假冒。 |
| **修改** | **撤回旧项目，同时将旧正文恢复到发送输入框**，用户自行继续修改并重新发送 | 这是用户明确的“撤回并回填”，**不是官方 `editQueueItem{newText}` 原位修改**。先保存本地正文，向宿主发送 `deleteQueueItem{queueItemId}`；收到可证实的成功／官方队列确认移除后，把**原文**填入输入框并聚焦，且旧项消失。若撤回失败／超时不确定，不改输入框、不重发、不默默复制成第二条。原输入框已有内容时须明确选择“合并／替换／取消”，绝不静默覆盖。 |
| **撤回** | 仅从官方队列移除该条，**不**回填到输入框、**不**发送 | 调 `deleteQueueItem{queueItemId}`；在 ACK/新鲜状态确认前显示“撤回中”，不先本地删掉后假装成功。失败、已消费、已被另一设备操作时提示真实状态并刷新。 |

**附件特别约束**：队列目前只投影 `text`，而官方队列项可能有 `attachments`。带附件的“修改＝恢复输入框”必须先有安全、可靠的附件引用还原方案；否则禁用此操作或确认后明确提示“只能恢复文字，附件需重新添加”，不可暗中丢失附件，也不可把队列原始 JSON/敏感本机路径直接暴露给手机。`editQueueItem` 可作为另一个**不满足本次“修改”定义**的未来能力，不能偷换产品语义。

**竞态与幂等**：所有队列动作应使用现有 `actionId`／动作账本机制，动作身份基于 `queueItemId` + 目标会话 + 代际 + 动作参数；重试不能生成双次官方操作。队列项已被消费、另一端提前删除、重排后位置变化或会话切换时，必须 fail closed 并刷新状态；不能按数组序号定位目标。对具有副作用的“修改”，要以官方删除结果确认作为回填条件，避免“旧项仍在 + 输入框重复一份”。

### 1.4 建议最小修改点

- `src-tauri/src/services/zcode_control/types.rs`：新增队列动作 DTO（例如 `QueueDelete/QueueReorder/QueueNow/QueueResume`），以及 `QueueView` 权威可操作状态；如果采用引导命令，应使用实际验证过的宿主 payload。
- `projection.rs`、`bridge.rs`／`hub.rs`：读取 `availability`／`inputRouting` 的最小白名单字段，按会话和代际发布；防旧 revision 重放。
- `command.rs`、`action.rs`、`capability/*`：精确构造官方命令；写前重新核对目标项、能力、官方状态及版本；ACK 与后续状态确认；持久幂等和审计只写操作类别／项标识摘要，不写正文。
- `src/lib/api/zcodeControl.ts`、`useZcodeControl.ts`、`ZcodeControlPanel.tsx`、`ZcodeQueuePanel.tsx`：联通动作、状态、禁用文案、撤回回填、窄屏交互。
- `src/i18n/locales/{zh,en}.json`（按项目实际 locale 位置）、手机手册中英文、端到端测试及真实手机截图。

### 1.5 Q1 验收条件

- 同一会话有三条真实队列，按顺序显示 `#1/#2/#3` 和 **前 20 字 + …**；每项都有三个指定按钮；一次重排后移动端与官方网页顺序一致。
- 仅撤回第二项：第一／第三项不变；远端队列中第二项消失；输入框不变。
- “修改”第二项：官方确认该项消失后输入框出现**完整未截断原文**，光标可编辑；绝不自动发送。草稿已有内容须明确处理；附件语义明确。
- “引导”必须通过实机证明与 UI 文案一致：运行中的 turn 如何变化、原 item 是否仍在队列、有无消息重复、是否阻断当前执行。**没证明之前此按钮不得对真实用户开放。**
- 双端同时编辑同一条、item 已消费、回执迟到／超时／掉线、capability 中途撤销：零双派发、零无声数据丢失、最终可恢复刷新。

---

## 2. Q2：打断后队列“僵住”，发新内容也不出去（P0）

### 2.1 已证实事实和待验证的根因链

- R0 取证显示官方队列是**容器级**状态：当回合取消且队列非空，可出现 `autoDrain=false`、`pauseReason="stopped"`；错误也可导致 `pauseReason="error"`。**停止当前执行不代表清空或恢复队列**。
- 当前 `ZcodeQueuePanel` 识别并显示 `paused=true`、原因文案，**但没有恢复按钮**；`ControlAction` 没有 `setAutoDrain` 写操作。这是“队列一直停着”的直接产品缺口。
- 当前 `ZcodeControlPanel.tsx:250–290` 主按钮只在官方 activeWorks 有 foreground turn 时显式打断；停止后运行态可能消失，按钮变回发送。`command.rs:661–665` 按运行态选择 `requestedDelivery="queue"` 或宿主默认路由，但**不**处理 paused、`inputRouting.mode`、`choice` 及 held queue。因此新的输入可能被拒绝、继续积压或无法触发消费。
- **谨慎结论**：用户实测“发不出去”是已发生的问题，但现有代码只能证明缺恢复能力和发送路由未处理队列；具体属于**宿主拒绝、接受后继续排队、还是连接／能力门禁失效**，仍需一次脱敏 wire 对账，不能直接断言都是同一原因。

### 2.2 推荐行为

1. `queue.known=true && queue.paused=true` 时**常驻清晰的 `[恢复运行]` 按钮**；**即使 items 为空也显示**，因为暂停是容器状态。按钮旁说明“将恢复官方队列自动处理”；不与“开始新会话”混淆。
2. 用户点击后调用官方 `setAutoDrain{autoDrain:true}`，按 ACK + 随后的新鲜 `state.queue.autoDrain=true` 判断恢复成功；若没有可执行 turn 或宿主没有自动触发，**不谎称工作已开始**，区分“已恢复自动处理”和“正在执行”。必要时通过真实支持的官方动作启动下一条，但不得隐式重发全部项目。
3. 如果暂停来自 `error`，显示错误来源并遵守宿主允许的恢复流程；`stopped` 可以手动恢复但不能静默自动恢复（用户刚刚点了“打断”，自动恢复会违背中断意图）。
4. 当 **paused + 新发送** 时明确显示决策：正常排队／恢复后处理／立即执行（只有官方支持时）。不得让按钮假装“已发送并执行”；即使被官方受理为 queued，也应显示“已排队，当前已暂停”。
5. `inputRouting.mode="choice"` 且队列非空时提供保留旧队列或清空旧队列的明确选择及对应 CAS（详见 F3），不默认销毁其他排队内容。
6. 移动端与官方网页对恢复、排队序号和消费顺序必须一致；断线后靠 GET/SSE 恢复权威状态，不以本地定时器自造消费。

### 2.3 Q2 验收条件

- A 正在运行 → B、C 排队 → 用户打断 A → 官方队列报告 paused=true → 手机出现 `[恢复运行]`，B/C 完整保留；点击后 paused=false，B/C 按官方顺序继续消费或准确显示后续阻塞理由。
- 暂停但队列已空，也能恢复 `autoDrain`，防止后来发送继续卡住。
- 暂停时再发 D：UI 给出真实 `accepted/rejected/queued` 结果；D 不能无声消失；恢复后不得重复发送 D。
- 发送／恢复并发、两台手机同时恢复、SSE 先到而 POST 后到、连接中断／会话切换：动作幂等、状态单调、正确禁用。

---

## 3. 上一轮 Review：必须合入的可靠性修复（P1）

### F1 — 同代际 GET／SSE 视图回退

**根因**：`src/mobile/useZcodeControl.ts:195–203` 的 `applySessionView` 只检查 `sessionId` 和 `generation`，不比较 `revision`；一个慢 GET 的旧 revision 可覆盖先到的 SSE 新 revision，从而让新队列项目、暂停状态、审批卡突然“回退”。服务端 `src-tauri/src/services/zcode_control/hub.rs:3131–3171` 将异步 GET 的结果写入 `inner.last_session`，同样需要检查是否覆盖已发布的更新 revision。

**修复要求**：前后端各实施同代际单调 revision 守卫；旧 revision 丢弃，等 revision 按事件序列或 authoritative refresh 决定；针对 `ready=false` 的真实降级设计独立的安全处理，不得为了防回退而屏蔽合法掉线／丢帧降级。测试 `GET(r10)` 延迟至 `SSE(r11)` 后到、跨会话、代际变更与未就绪降级。

### F2 — send ACK 仅看 `status` 导致假成功／清草稿

**根因**：`src-tauri/src/services/zcode_control/command.rs:344–352` 的 `classify_ack` 把 `accepted|duplicate` 直接认作受理，`dispatch_send` 忽略 `result`；`src/mobile/useZcodeControl.ts:490–494` 收到 accepted 就允许清除草稿／附件。官方 R0 证据要求 `result.type="inputAccepted"` 并读取 `result.delivery`（`startNow|queue|guide`），否则“输入受理结果未知”。

**修复要求**：发送专用 ACK 校验，与 permission/model 等 ACK 判据分开；只有 `accepted|duplicate` 且 `result.type="inputAccepted"`、合法 `delivery`、`inputId` 对应本次出站 `commandId` 时才认“已受理”；带 `delivery` 和 `inputId` 的精简回执供 UI 提示“执行中／已排队／引导中”；缺失或冲突字段为 `unknown` 并保留草稿／附件，**禁止自动重试可能已受理的发送**。覆盖 rejected/stale/failed/noop、重复 ACK、超时、回执与 SSE 竞态。

### F3 — `inputRouting.mode="choice"` 时未处理旧队列的选择

**根因**：`command.rs:627–668,1468–1487` 只在运行时添加 `requestedDelivery="queue"`；未投影 `state.inputRouting`，也不带 `heldQueueDisposition` / `expectedHeldQueueItemIds`。R0 证据显示 `choice` + 非空队列时官方要求明确选择 `clearQueueAndSend` 或 `keepQueueAndSend` 和当前队列 IDs，否则可能拒绝 `heldQueueDispositionRequired`。

**修复要求**：发送前按新鲜官方 `inputRouting` 精确判断；`choice` + 非空队列弹“**保留排队消息并发送** / **清空排队消息并发送** / 取消”，默认**不清空**；携带当前官方队列 ids 做 CAS，过期则刷新后让用户再选，绝不悄悄清空或篡改 `requestedDelivery`。`guide/enqueue/reject` 分支应有明确行为及可读理由；之前 U3 已裁决运行中优先 queue 的意图，不得未经重新产品裁决静默改为 guide。

---

## 4. 上一轮 Review：次要问题（P2）

### F4 — 非 AskUserQuestion 的可操作假象

`src-tauri/src/services/zcode_control/pending.rs:275–287` 只支持 `userInput + toolName=AskUserQuestion` 的答复；但 `ZcodePendingCard` 可能按更宽泛 `kind=userInput` 和通用 capability 展示可选控件。**应把“本卡可答”按 schema subtype 与 Rust 构造函数能力投影到前端**，不支持的 `plan_approval`／MCP elicitation 显示只读与解释，不提交注定失败的动作。测试支持／不支持子型混排、动态新增和切组。

### F5 — 审批详情长度与安全可见性

`src-tauri/src/services/zcode_control/types.rs:724–760` 的 `detail` 投影上限为 2000 字符。长 Bash 命令或大参数可能被截断，影响用户审批判断。**不得默认把截断命令表现为完整命令**；优先显示“内容已截断，去官方端查看完整内容”，必要时对高风险截断审批禁用“允许”、仍保留“拒绝”；若要安全扩展全文必须设计受权分片读取、体积限制和敏感数据保护，不能直接把任意长 shell 命令推入推送／日志。

---

## 5. 可选 O1：二级供应商 → 模型选择

### 代码层可行性判断

**前端改动本身不难，服务端仍有实质限制。**`src/mobile/ZcodeControlPanel.tsx:432–470,650–660` 只用 `session.selection.models` 做单层模型选择。官方 `model-selection.getView()` 的候选响应**含** `providers[].providerId / models[]`，但 `src-tauri/src/services/zcode_control/selection.rs:200–265` **主动只保留当前 provider**；`SelectionView` 只含单个 `provider`；`selection.rs:26–59` 在 `switchModelConfig` 中将 provider 强制取自当前 session，根本不接受其他 provider 目标。这是原设计为了防其他账号目录泄漏和错切换而定的安全限制，不能仅改前端。

**建议分阶段**：

1. **协议／权限探针（独立验收）**：确认官方当前 build 的 getView 是否只返回已登录／可用的 providers，以及 `switchModelConfig` 在**本会话**切换到另一 provider 的真实语义、是否需要额外授权、是否可保留会话／上下文；验证 revision CAS、档位列表、权限回退和多账号隔离。不应通过用户传入任意字符串跳过白名单。
2. **若探针通过**：后端 `SelectionView` 改为 `{current:{provider,model,thought},providers:[{providerId,models:[{modelId,thoughtLevels}]}]}` 的受限公开目录；`ControlAction::Selection` 显式携带 provider，但服务端必须再次验证它属于宿主授权的可切换目录。`switchModelConfig` 携带经过校验的目标三元组和 `baseRevision`，等待 revision 前进且三元组精确吻合，失败则保留旧选择。
3. **UI**：模型区域改为 `供应商 ▸ 模型` 二段选择，思考档位仍按**目标模型**动态刷新；更换 provider 不立即发第二个隐式命令，只有最终确定一个目标模型与档位才发送**一次** selection；窄屏保持紧凑、全名可查看。
4. **若不支持**：保留当前单 provider 菜单，不呈现灰色但看似能选的跨供应商假入口；可以只做分组展示当前 provider，而不能称为跨供应商切换已实现。

**验收**：跨 provider 成功与失败／未授权 provider 不可枚举／切换后的实时快照一致／档位按模型约束／两端同步／无额外账户授权泄漏。

## 6. 可选 O2：模型左侧上下文使用情况圆环

### 根因与数据门槛

目前 `SessionView` 只公开 `selection/control/queue/pending/capabilities/revision`，`SelectionView` 仅有 `provider/model/thought/models`，**没有当前会话上下文已用 token、窗口容量或压缩后占用量**。不能把 Tuvis 既有的“会话累计输入输出 token”当成**当前上下文占用**，两者并非同一口径。

**推荐口径**：圆环代表 `usedContextTokens / contextWindowTokens`；百分比仅当这两个数都来自同一会话、同一模型、同一可信时点的官方或可验证数据时绘制，保持 0–100% 有界，并说明使用量可能包含系统提示、工具结果、缓存及压缩。若官方未提供可靠指标，圆环显示 `?`／禁用并解释“暂无上下文数据”，**不得按累计 token 估算精确进度**。换模型、压缩上下文、会话切换时重置／刷新，并显示取样时间。

**拟改**：先在官方快照、事件、可访问的只读统计通道中取证；若有同口径数据，扩充 `SessionView.contextUsage`（含 `source/observedAt/revision/used/max`），在 `ZcodeControlPanel` 的模型左侧显示轻量圆环，悬浮／点击展示“已用 / 总量／占比／来源”，极窄屏不能把官方链接和模型选择挤到不可用；无数据也不影响模型切换。无真实数据时本功能留作研究项。

**验收**：占用上升／会话压缩下降／模型切换容量变化、0 和超界保护、未知值、断线旧值、移动端视觉与读屏文案；严禁将账本累计 token 当成上下文窗口。

## 7. 可选 O3：每次打开 Tuvis 都要求确认“安全 Cookie／钥匙串”

### 当前代码事实与可能原因

- ZCode 授权复用已经存在：`src-tauri/src/services/zcode_control/credential_store.rs:260–305` 使用 macOS `keyring` 原生 Apple Keychain，正式服务名 `com.jarvis.tuvis.zcode`、开发版 `com.jarvis.tuvis.dev.zcode`，账户 `primary`；启动时 `AuthService::restore` 会读取凭据。**服务名稳定并不等于系统永不弹授权框**。
- 当前仓库 `docs/operations/RELEASE.md` 说明 macOS 发布包**未完整 Apple 签名**；`.github/workflows/release.yml` 的 macOS 构建未看到 Developer ID 证书注入／公证步骤，Tauri updater 的 minisign 签名不等于 macOS 的应用代码签名。macOS Keychain 按信任的应用身份／代码签名控制访问，应用升级、重装、运行路径或开发版二进制变更都可能触发重新确认。
- **但“安全 Cookie”提示的确切条目尚未确定**：也可能来自其他凭据源／浏览器安全存储／工具子进程，而不是 `com.jarvis.tuvis.zcode`。手机配对使用的 HTTP `mam_device` Cookie 与 macOS Keychain 授权弹窗不是同一个机制。
- Apple 文档明确：`Always Allow` 对可信应用有效，但被修改或更新的 app 有时需要再次授权；因此“同一台电脑，版本更新或重装后**绝对**不再提示”不能在绕过 macOS 权限系统的前提下无条件保证。

### 安全的实施顺序

1. **先定位弹窗而不是改钥匙串访问规则**：在实机记录弹窗中“请求方应用名／凭据条目名”、Tuvis 构建来源（debug、DMG、自动更新）、版本和实际 `.app` 路径；日志只记 `load/save/delete` 操作类型与脱敏错误、`codesign` 状态，不打印秘密值和密码。区分“每次启动读取授权”与“每次启动真的弹系统授权”。
2. **稳定应用身份**：将正式 macOS 发布路径改为同一 Developer ID Team、同一 bundle identifier、同一签名要求并配置公证、验证 CI 产物的 `codesign --verify`、designated requirement；debug 和 release 分离命名空间，发布不得跨版本换身份。此项可能涉及 Apple 开发者证书／发布流程成本。
3. **复核 Keychain 策略**：确保条目使用合适的 app-scoped 权限／可访问性；必要时评估 Data Protection Keychain 与 access group，但必须先确认签名和 entitlement 可用、迁移机制安全、与 Tauri / `keyring` 后端兼容，不准粗暴设置“所有应用可访问”、存明文、读写浏览器 Cookie 或关闭系统安全提示。
4. **减少无意义访问**：有界合并重复 restore 请求，同进程内有效 JWT 不反复读取 OS；锁定／拒绝保留现有错误态，不假称授权失效；“记住授权”“忘记授权”的语义不变。
5. **平台实机验收**：全新安装授权一次 → 退出重启 3 次 → 同签名版本升级 → 删除 app 但**保留 Keychain 条目**后重装 → debug/release 交叉 → 被拒绝／锁定／换用户。记录系统提示次数，**结果按“可消除的重复提示”和“系统仍合法要求确认的情形”分别报告**；不以 mock store 测试代替。

**用户期望的可执行表述**：在同一台机器、同一系统账号、同一可信签名身份和仍受系统信任的 Keychain 条件下，尽量做到**首次允许后正常重启、常规升级无需再次确认**；重装尽量继承可信身份，但系统重置、安全策略变化、应用身份变更等情况允许重新提示，绝不绕过 OS 安全边界。

来源：Apple [Keychain ACL](https://developer.apple.com/documentation/security/access-control-lists)、[更新后的已信任 app 可能再次询问](https://support.apple.com/en-ca/guide/keychain-access/kyca1331/mac)、[Always Allow](https://support.apple.com/en-gb/guide/mac-help/kychn002/26/mac/26)、[macOS 签名要求](https://developer.apple.com/library/archive/technotes/tn2206/)。

---

## 8. 分阶段实施建议（直接转 coding plan）

| 阶段 | 内容 | 合格产物／停止条件 |
|---|---|---|
| **T0：证据与合同** | 实机最小抓包（脱敏）确认 queued → interrupted → paused → send → resume；验证 `sendQueuedNow` 与“引导”语义；固定 protocol build 与 capability；界定“修改”附件及草稿冲突 | 状态机／命令 ACK 样例、真实错误码、场景矩阵；语义不明的“引导”不进入生产开闸 |
| **T1：发送一致性** | F1 revision 单调性、F2 inputAccepted ACK、F3 choice CAS | 测试先红后绿，发送成功不误清草稿，历史队列不会被静默清空 |
| **T2：队列写操作** | 新增服务端队列动作，官方 availability 精确投影、权限／幂等／在途守卫；撤回、重排、经验证的引导 | fake host + 双端冲突测试通过；无本地自建队列，真实命令可以验证 |
| **T3：移动 UI 与恢复** | 三按钮每行展示、20 字截断、修改＝删除后回填、恢复运行；状态／错误／回执；窄屏和辅助功能 | 真机 A→B/C→打断→恢复→继续，撤回／编辑／插队可完成 |
| **T4：P2 与文档收口** | 审批卡子类型门控、正文截断安全态；中英文文案、手册、方案与源码一致性 review | 用户可理解的安全降级；所有测试与截图有可追溯证据 |
| **T5：可选研究（不阻塞 T1–T4）** | O1 跨 provider 真实协议；O2 context 使用量来源；O3 Mac 安全弹窗定位和签名／Keychain 验收 | 每项单独 go/no-go；未取得有效数据不得为了 UI 效果开假功能 |

### 必测自动化用例

**Rust/protocol**：队列 itemId 精确定位、delete/reorder/now/autoDrain 的官方 payload，ACK 受理 vs 实际状态确认，capability false／missing／stale，revision 和 generation 交错，held queue CAS 变化，send inputAccepted 三类 delivery，超时／断线／重复 actionId、两客户端竞争，正文／凭据不写日志。

**移动端**：20 字正确截断（含 emoji、组合字符、换行）、长中文／英文、360px/390px/320px 及 125% 字号、暗亮主题、中文／英文、三按钮焦点与禁用、恢复按钮 paused true+empty 仍出现、撤回回填不丢现有草稿、附件冲突、并发新 SSE、切会话与迟到 ACK。

**真实宿主 E2E（合并主线的必要条件）**：官方 ZCode 3.16.2.8769 同 build 与手机 / Tuvis 桥；一条正在跑 + 两条待排队；逐项使用引导／重排／修改／撤回；停止后保留队列并恢复；发送新消息成功；官方端与手机端对得上，没有重复消息或永久不可恢复的暂停。需记录脱敏状态字段／命令名／ACK 类型和 revision，不记录消息正文与授权秘密。

**回归**：保留 U1–U7 原本已完成的附件说明、单行工具栏、发送／打断主按钮、真实问答和审批；避免新引导按钮偷换 U3 既有“有文字就发送、空输入且运行中才打断”的规则。运行仓库既定 `pnpm test`、`pnpm check`、`pnpm build:mobile`、Cargo 测试／Clippy／fmt；GitHub PR 当前 HEAD 需要另行确认，旧文档报告的通过不代表下一轮代码通过。

## 9. 验收勾选表（开发完成后逐条填写证据）

- [ ] Q1 三按钮和队列插队都可操作，**引导**语义已按真实官方行为证明；不支持则说明并不开放。
- [ ] Q1 “修改”先确认撤回，再回填**完整原文**；含附件、已有草稿、竞态／断线不丢数据。
- [ ] Q2 打断后 paused=true 能看到并触发“恢复运行”；暂停空队列也能恢复。
- [ ] Q2 暂停期间新发送不会假成功或静默消失，恢复后队列能继续流转。
- [ ] F1 同代际旧 GET 无法覆盖新 SSE；合法 ready=false 安全降级仍生效。
- [ ] F2 仅完整 `inputAccepted` ACK 才清草稿；区分 startNow/queue/guide。
- [ ] F3 `choice` 提供“保留／清空”明确选择且 CAS 失败不会无声清队列。
- [ ] F4 子型不支持的审批／问题卡不会呈现假可执行动作。
- [ ] F5 已截断 Bash 等审批内容明确标记，敏感操作不会因不可见部分被误授权。
- [ ] O1/O2/O3 分别留下“实现／不实现”及证据；未完成不阻塞 P0/P1。
- [ ] 自动化测试、真实宿主联调、移动端截图、Mac 安全权限实测的报告**各自单列**；不得相互冒充。

## 10. 待工程验证的关键问题（不是先问用户的阻塞项）

1. “引导”的真实预期与官方 `sendQueuedNow` 是否一一对应？官方是否提供可对**已有 queue item**执行 `guide` 的专用入口？**通过实机／宿主调用证据判断，不能用按钮名推导命令。**
2. `setAutoDrain(true)` 在被中断且没有 activeWorks 时是否自动触发下一条排队内容？若不会，官方有哪些可用的恢复执行入口？
3. “安全 Cookie”弹窗的**具体服务名与请求应用**是什么？是 Tuvis ZCode Keychain，还是外部工具／浏览器项？当前 macOS 发行构建实际签名状态与 repo 文档是否一致？
4. 当前官方 build 是否允许跨 provider 切换？上下文用量是否有**真实的会话级 token 占用／容量**来源？若无，两个可选体验项保持不实现。

---

### 审查参考（基线固定至本次 HEAD）

- [PR #190](https://github.com/jarvislee90s-dot/tuvis/pull/190)
- [ZcodeQueuePanel.tsx](https://github.com/jarvislee90s-dot/tuvis/blob/f81f062e7d5493f947c6d3dbe0dc5729c95ffb77/src/mobile/ZcodeQueuePanel.tsx)
- [ZcodeControlPanel.tsx](https://github.com/jarvislee90s-dot/tuvis/blob/f81f062e7d5493f947c6d3dbe0dc5729c95ffb77/src/mobile/ZcodeControlPanel.tsx)
- [useZcodeControl.ts](https://github.com/jarvislee90s-dot/tuvis/blob/f81f062e7d5493f947c6d3dbe0dc5729c95ffb77/src/mobile/useZcodeControl.ts)
- [Rust ControlAction / QueueView](https://github.com/jarvislee90s-dot/tuvis/blob/f81f062e7d5493f947c6d3dbe0dc5729c95ffb77/src-tauri/src/services/zcode_control/types.rs)
- [command.rs](https://github.com/jarvislee90s-dot/tuvis/blob/f81f062e7d5493f947c6d3dbe0dc5729c95ffb77/src-tauri/src/services/zcode_control/command.rs)
- [selection.rs](https://github.com/jarvislee90s-dot/tuvis/blob/f81f062e7d5493f947c6d3dbe0dc5729c95ffb77/src-tauri/src/services/zcode_control/selection.rs)
- [credential_store.rs](https://github.com/jarvislee90s-dot/tuvis/blob/f81f062e7d5493f947c6d3dbe0dc5729c95ffb77/src-tauri/src/services/zcode_control/credential_store.rs)
- [R0 协议取证](https://github.com/jarvislee90s-dot/tuvis/blob/f81f062e7d5493f947c6d3dbe0dc5729c95ffb77/docs/superpowers/reviews/2026-10-29-zcode-r0-wire-evidence.md)
- [macOS Release CI](https://github.com/jarvislee90s-dot/tuvis/blob/f81f062e7d5493f947c6d3dbe0dc5729c95ffb77/.github/workflows/release.yml)
