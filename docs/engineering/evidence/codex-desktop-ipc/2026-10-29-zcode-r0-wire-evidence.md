# ZCode 官方 wire 形状取证（R0）：审批 pending / 输入队列 / send ACK

- 日期：2026-10-29
- 仓库分支：`codex/zcode-unified-remote`
- 结论性质：**只读安装包取证**（class = PACKAGE）。凡安装包内找不到写入点的，一律记「未证实」，不推测。

## 1. 取证环境

| 项 | 值 |
|---|---|
| APP | `/Applications/ZCode.app`，`CFBundleVersion = 3.16.2.8769`（PlistBuddy 直读） |
| 主资源 | `Contents/Resources/app.asar`，341,333,085 B |
| asar sha256 | `506a2d032707a584229c8e2f02f12c20…`（前 16 位 `506a2d032707a584`） |
| 内置 CLI | `Contents/Resources/glm/zcode.cjs`，15,243,891 B，sha256 `a055234a6ca3da01…`（与 `contract.json identity.bundledCliFingerprint` 完全一致） |
| 容器解析 | 标准 asar：`[u32=4][u32=7269396][u32=7269392][u32=7269387][JSON]`，JSON 起于 16，**data base = 7269400**；目录 27,765 文件，`/out/**` 4,218 个 |
| 公开源码侧 | **本轮未取证**：本机无 `zai-org/ZCode` 源码 checkout，也未联网拉取固定 sha。计划要求的「公开源码与安装包不同的面分别记载」在本轮只完成了安装包一侧；凡下文未标注「源码」的结论，均只有安装包证据。 |
| 检索方法 | ① 解析 asar 目录 → 按路径精确切片出 `/out/host/index.js`(2.0M)、`/out/main/index.js`、`/out/main/chunk-XYX2CJ3H.js`、`/out/main/chunk-ADKRHOZV.js`、`/out/scheduler/index.js`、`/out/renderer/assets/styles-n-UNIiNg.js`；② 对 341MB 原件做 `bytes.find` 全量检索并用 span 表把命中反查回「文件 + 文件内偏移」；③ 对 `zcode.cjs` 直接偏移切片。 |
| 偏移记法 | `文件@文件内偏移 (+asar 绝对偏移)`。下文 `HOST`=`/out/host/index.js`，`XYX`=`/out/main/chunk-XYX2CJ3H.js`，`ADK`=`/out/main/chunk-ADKRHOZV.js`，`SCH`=`/out/scheduler/index.js`，`CLI`=`glm/zcode.cjs`。 |

> 关键前提：本项目文档里的 `session/sendText` **不是**官方方法名。安装包内 `session/sendText` 出现 0 次；真实通道是 `sendConversationCommandV4`，命令用信封 `{commandId, clientId, sessionId, type, payload, baseRevision?}`，`type` 取值就是命令表键名（`sendText` / `resolveInteraction` / `sendQueuedNow` / `deleteQueueItem` / `setAutoDrain` …）。证据：`ADK@36960 (+281635533)` `Do=e.enum(Object.keys(Uo))`。

## 2. 审批（permission）pending

### 2.1 卡片容器（快照内 `pendingInteractions[]`）

`XYX@641941 (+282663017)`：
```
$$=e.object({interactionId:e.string(),kind:e.enum(["permission","userInput","workspaceHookReview"]),
anchorRowId:e.number().nullable(),createdAt:L,autoResolution:Xf.optional(),
payload:e.discriminatedUnion("kind",[E$,O$,Sx])}).superRefine(...顶层 kind 必须等于 payload.kind...)
```

| 字段 | 取值 / 形状 | 证据 | 置信度 |
|---|---|---|---|
| `interactionId` | string；生成规则 `payload.requestId ?? "perm-"+toolCallId` | `CLI@14630019`（`onPermissionRequested`）、`XYX@641941` | 高 |
| `kind` | `permission` \| `userInput` \| `workspaceHookReview` | `XYX@641941`；`SCH@600583` 同形 | 高 |
| `anchorRowId` | number \| **null** | 同上 | 高 |
| `createdAt` | number（毫秒时间戳） | 同上 | 高 |
| `autoResolution` | 缺省 \| `{state:"hiddenGrace"\|"visibleCountdown",startedAt,visibleAt,deadlineAt}` \| `{state:"snoozed",startedAt,snoozedAt}` | `XYX@641745` | 高 |
| `payload.kind` | 必须与顶层 `kind` 相同（官方 superRefine 强校验） | `XYX@641941` | 高 |

### 2.2 permission payload（`E$`）

`XYX@640695 (+282661771)`：
```
E$=e.object({kind:e.literal("permission"),toolCallId,toolName,summary,detail:e.unknown(),
freeText?:bool,origin?:rt,display?:ws.catch(undefined),
fullAccessOption:ok.extend({optionId:e.literal("fullAccess"),kind:e.literal("custom")}).optional(),
options:e.array(ok)})
```

| 字段 | 取值 | 证据 | 置信度 |
|---|---|---|---|
| `toolCallId` / `toolName` | string | 同上 | 高 |
| `summary` | string = 事件里的 `reason` 原文（人类可读的审批理由） | `CLI@14630300` `summary:n.reason` | 高 |
| `detail` | **unknown**，实际写入 = 工具输入对象（Bash 即 `{command, description?}` 之类） | `CLI@14630300` `detail:n.input` | 高（`n.input` 为 CLI 内部 tool input，未逐工具展开） |
| `freeText` | permission 卡固定 `true` | `CLI@14630300` | 高 |
| `origin` | 可选（如 subagent/background 来源） | `XYX@640695` | 中（值域 `rt` 未展开） |
| `display` | 可选，渲染用展示树（`ws` 为多 kind 联合：turnHeader/tool/mcp_tool…） | `XYX@629855` | 中 |
| `options` | 数组，**无 min 长度**（与事件侧 `min(1)` 不同） | `XYX@640695` | 高 |

### 2.3 选项元素与真实枚举（最关键）

`XYX@640547 (+282661623)`：
```
T$="fullAccess", ok=e.object({optionId:e.string(),label:e.string(),
kind:e.enum(["allowOnce","allowAlways","deny","custom"]),response:Ut.optional()})
```
`Ut`（`XYX@577312`）= `{decision:"allow"|"deny"|"escalate"|"modify", reason?, modifiedInput?, permissionUpdates?:[{type:"addRules",behavior:"allow"|"deny"|"ask",rules:[{toolName,ruleContent?}]}]}`。

选项**不是**写死的 4 个，而是 CLI 现场构造（`CLI@14489746` `buildProtocolPermissionOptions`）：
```
[{kind:"allow_once",name:"Allow once",optionId:"allow_once",response:{decision:"allow",reason:"Approved once"}},
 {kind:"allow_always",name:"Always allow in this project",optionId:"allow_project",
  response:{decision:"allow",permissionUpdates:[{type:"addRules",behavior:"allow",rules:[{toolName,ruleContent?}]}],reason:"Approved for this project"}},
 {kind:"deny",name:"Deny",optionId:"deny",response:{decision:"deny",reason:"<长拒绝文案>"}}]
```
`approvalMode==="user-once"` 时被裁成只剩 `allow_once` + `deny`；`optionsPolicy==="session-always-allow"` 时中间项换成 `{kind:"allow_session",optionId:"allowSession",name:"Always allow in this session"}`；`no-always-allow` 时中间项整项消失。

落盘到卡时的映射（`CLI@14632024`）：
```
optionId: kind==="allow_once"?"allowOnce" : kind==="allow_always"?"allowAlways" : c.optionId
kind:     kind==="allow_once"?"allowOnce" : (allow_always||allow_session)?"allowAlways" : "deny"
label:    c.name ; response: c.response
```

| 结论 | 值 | 证据 | 置信度 |
|---|---|---|---|
| `option.kind` 真实值域 | `allowOnce` \| `allowAlways` \| `deny` \| `custom`（`custom` 仅 "Refine"） | `XYX@640547`；`SCH@599189`；`CLI@793871` 三处一致 | 高 |
| `option.optionId` 真实值域 | `allowOnce` \| `allowAlways` \| `allowSession` \| `deny` \| `workflowRefine` \| `fullAccess` | `CLI@14489746` + `CLI@14632024` + `CLI@733423`（`LDe="workflowRefine"`） | 高 |
| `fullAccess` 怎么出现 | **独立字段 `fullAccessOption`，不在 `options[]` 里**；`optionId:"fullAccess"`、`kind:"custom"`、`response.decision:"deny"`（远端解析等于拒绝） | `CLI@14632024` | 高 |
| `fullAccessOption` 出现条件 | `approvalMode!=="user-once" && fullAccessSupported===true && !origin && !optionsPolicy` | 同上 | 高 |
| `label` vs `name` | 卡内是 `label`；事件侧是 `name`。两者都来自 `c.name`（"Allow once"/"Always allow in this project"/"Deny"） | `CLI@14489746` | 高 |
| `response` 语义 | 每个选项自带「被选中后宿主将执行什么」，不是 UI 文案 | `XYX@577312` | 高 |

### 2.4 卡是怎么进 pending 集合的

- **快照全量**：`snapshot.pendingInteractions` 必填数组（`XYX@644901 (+282665977)` 全量 state `$5` 里 `queue:P$,pendingInteractions:e.array($$),rows:Z$`）。
- **增量也仍是全量替换**：delta 只有 `{op:"state.updated",patch:{pendingInteractions:[…]}}`，`patch` 里该键是**整数组**（`ADK@11928 (+281613361)` 的 patch schema `pendingInteractions:e.array(at).optional()`），不是 append 指令。
- **触发事件名**：会话事件流里有 `permission.requested` / `permission.resolved`（`XYX@706354` 事件联合表；`permission.requested` 字面量在 `XYX@706637`）。宿主把事件**归约**成 state 补丁，追加方式为「整数组 + 新卡」：
  `CLI@14630019`：
  ```
  onPermissionRequested(t){…c.push({op:"state.updated",
    patch:{pendingInteractions:[...this.snapshot.pendingInteractions,a]}});return c}
  ```
  同时把对应工具行 `status` 置 `pendingApproval` 并写 `approvalInteractionId`（同一函数）。
- 撤卡也是全量过滤：`CLI@14622264` `pendingInteractions.filter(f=>!…toolCallId…)`。
- 事件侧 payload（`Lj`，`XYX@705113`）与卡内 payload **不同形**：`{requestId?,toolCallId,toolName,riskLevel:"low"|"medium"|"high"|"critical",reason,input,suggestedPermissionUpdates?,approvalMode?:"user-once",origin?,options:[{optionId,kind:<自由 string>,name,description?,response}]}`（`yh`，`XYX@688955`）。→ **远端控制器应以卡为准，事件里的 `kind` 是未收敛的自由字符串**。

## 3. 输入队列（queue）

**队列存在，且是快照一级字段，不是猜测。**

`XYX@640428 (+282661504)`：
```
P$=e.object({items:e.array(w$),autoDrain:e.boolean(),pauseReason:e.enum(["stopped","manual","error"]).optional()})
w$=Ax.extend({dispatch:Df.extend({state:e.enum(["queued","reserved","promoting"])}),toolDisallowlist?:string[]})
```
`XYX@603501 (+282624577)`（队列项本体 `Ax`）：
```
Ax=e.object({inputOrigin?:["desktop","mobile"],botGroupSource?,conversationQuotes?,
 sourceCommandId,queueItemId,clientId,kind:["sendText","sendGoalCommand","compact"],text,
 attachments:[…],modelSelection?,mode?,planEnabled?,sharedContextRefs?,delivery:AO,order:zO,
 steer:OO,dispatch:Df,admittedAt,highspeed?,provenance?,source?}).strict()
```
`XYX@602951` `AO={requested:["auto","startNow","queue","guide"],admitted:["startNow","queue","guide"],fallbackReasonCode?}`；`zO={admissionSeq,queuePosition?}`；`OO={state:["notRequested","submitting","steering","guided","fellBack"],reasonCode?}`；`XYX@603372` `Df={state:["admitted","queued","reserved","promoting","drained"],reservationId?}`。

| 问题 | 真实字段 / 取值 | 证据 | 置信度 |
|---|---|---|---|
| 队列容器 | `state.queue = {items[], autoDrain, pauseReason?}` | `XYX@640428`；`SCH@599070`；`CLI@793871` 三处一致 | 高 |
| 队列项身份 | `queueItemId`（**= `"queue_"+commandId`**，确定性派生） | `CLI@14552718` `function I5e(e){return \`queue_${e}\`}`（名 `queueItemIdForCommand`） | 高 |
| 关联发送命令 | `sourceCommandId`（= 该次 sendText 的 `commandId`）、`clientId`、`provenance.{sourceCommandId,queueItemId?,clientId?}` | `CLI@14791230`（`buildQueueItem`） | 高 |
| 正文 | `text`（**全文，无 preview 字段**） | `XYX@603501`；`CLI@14791230` | 高 |
| 附件 | `attachments[]`（引用式，非 base64） | 同上 | 高 |
| 次序 | `order.admissionSeq`（每会话单调 +1）+ `order.queuePosition`（**0 基，= 队列内下标**）；数组顺序即展示顺序 | `CLI@14554498`（`x=(this.admissionSeq.get(a)??0)+1`）；`CLI@1380103`（`.map((f,h)=>({…intent:{…,queuePosition:h}}))`） | 高 |
| 投递决策 | `delivery.requested` ∈ auto/startNow/queue/guide，`delivery.admitted` ∈ startNow/queue/guide，`delivery.fallbackReasonCode` | `XYX@602951` | 高 |
| 暂停状态 | **容器级**：`autoDrain:false` + `pauseReason`。观测到的写入只有 `"stopped"`（轮次被取消且队列非空）与 `"error"`（轮次失败且队列非空）；`"manual"` 只在枚举里 | `CLI@14612939`、`CLI@14614082`（两处 `{…queue,autoDrain:!1,pauseReason:"stopped"/"error"}`） | 高（前两值）/ 低（manual 写入点未找到） |
| 逐项状态机 | `dispatch.state`: `queued→reserved→promoting→drained`（`admitted` 只出现在命令表全集） | `XYX@603372`、`CLI@762795` | 高 |
| steer 子态 | `steer.state`: `notRequested/submitting/steering/guided/fellBack` | `XYX@602951` 区段 | 高 |
| 入队 | 无独立 append 事件；`state.updated` 的 `patch.queue` 整对象替换；ACK 侧同时给 `result.type="inputAccepted"`、`delivery:"queue"` | `ADK@11928`、`CLI@14762292` | 高 |
| 出队/消费 | `dispatch.state` 迁移 + 从 `items[]` 消失（无单独「consumed」事件） | `XYX@640428` | 中（仅见投影，未见显式 drain 事件名） |
| 移除 | 命令 `deleteQueueItem{queueItemId}` | `ADK@35392`、`SCH@842997` | 高 |
| 重排 | 命令 `reorderQueueItem{queueItemId, beforeQueueItemId: string\|null}` | 同上 | 高 |
| 编辑 | 命令 `editQueueItem{queueItemId, newText}` | 同上 | 高 |
| 立即执行 | 命令 `sendQueuedNow{queueItemId, modelSelection?, modelExecution?, highspeedMeta?}` | 同上 | 高 |
| 暂停/恢复 | 命令 `setAutoDrain{autoDrain:boolean}` | `ADK@35463`、`CLI@14778278`（`app.setQueueAutoDrain`） | 高 |
| 清空/保留旧队列 | sendText 的 `heldQueueDisposition:"clearQueueAndSend"\|"keepQueueAndSend"` + `expectedHeldQueueItemIds[]`（CAS） | `ADK@33035`、`CLI@14510809`（`inputRouting.mode==="choice"` 且队列非空时**必须**带，否则 `heldQueueDispositionRequired`） | 高 |
| 运行时内部名 | 队列即「pending input 册」：`reservePendingInputById / markPendingInputPromoting / releasePendingInputReservation / removePendingInputById / clearAllPendingInputs / editPendingInputById / reorderPendingInput / drainPendingInput` | `CLI@13125870` | 高 |
| 第二条（易混）通道 | **运行中 steer 队列**：事件 `turn.steerQueued` payload `{pendingInputId,inputId?,queryId?,input,inputPreview,inputSize,commandKind?,source?,toolDisallowlist?,delivery:"queue"|"guide",targetTurnId,queueLength,intent?}`；`turn.steerDrained` `{pendingInputIds[],queryIds?,targetTurnId,injectedMessageIds[],drainedInputs[]?}` | `XYX@701594`（`Aj`）、`XYX@701947`（`zj`）、`XYX@706354`（事件表） | 高 |
| 投递模式总闸 | `state.inputRouting = {mode:["startNow","enqueue","guide","reject","choice"], reasonCode?}`；`state.availability.{queueEdit,sendQueuedNow,…}={allowed:true}\|{allowed:false,reasonCode}` | `XYX@639779`、`XYX@639313` 区段 | 高 |

**队列不存在性核查（反向证据）**：候选标识符 `queuedInputs`、`inputQueue`、`pendingSends` 在 asar 内命中 **0**；`queue`/`queued` 相关表达**全部**收敛到上面 `state.queue` 与 `pendingInputId` 两处，无第三种。

## 4. send ACK

命令结果对象（`ADK@38235 (+281639668)`）：
```
_o=e.object({memoryEnabled?:bool,ttftExcluded?:"capacity",commandId,status:
 e.enum(["accepted","rejected","stale","duplicate","noop","failed"]),reasonCode?,message?,
 revisionAtDecision:e.number(),result:Oo.optional()})
```
`Oo`（`ADK@37225`）里与输入相关的两个分支：
```
{type:"inputAccepted",delivery:["startNow","queue","guide"],inputId,messageId?}
{type:"inputDisposition",delivery:["startNow","queue","guide"]}
```
批量查询回包 `ADK@38550`：`{results:[{key:{sessionId,commandId}, result:_o|"unknown"}], clock?}`。

| 问题 | 真实字段 / 取值 | 证据 | 置信度 |
|---|---|---|---|
| 有没有 accepted | **有**，但不在顶层叫 accepted：`status:"accepted"` | `ADK@38235` | 高 |
| 有没有 delivery | 有，但**嵌套**在 `result.delivery`，且必须先 `result.type==="inputAccepted"` | `ADK@37225` | 高 |
| 有没有 queued | **没有** `queued` 布尔；排队 = `result.delivery==="queue"` | `CLI@14762292`：`$.admission.kind==="queued"?{type:"inputAccepted",delivery:"queue",inputId:t.commandId}:{…delivery:"startNow"…}` | 高 |
| `inputId` 是什么 | **就是发送时的 `commandId`**（原样回显，不是新 id） | `CLI@14762292`（两分支均 `inputId:t.commandId`） | 高 |
| `messageId` | 可选，此路径未填 | `ADK@37225` | 高 |
| `status` 全值域 | `accepted/rejected/stale/duplicate/noop/failed` | `ADK@38235`；`SCH@845424`；`CLI@1234081` 三处一致 | 高 |
| 其它顶层字段 | `commandId`、`revisionAtDecision`（必填）、`reasonCode?`、`message?`、`memoryEnabled?`、`ttftExcluded?` | 同上 | 高 |
| 官方怎么判定成功 | 先 `status∈{accepted,duplicate}`，再**强校验** `result?.type==="inputAccepted"`，否则抛 `Input admission outcome is unknown` | `HOST@1031329 (+280560695)` | 高 |
| 官方怎么判排队 | `result.delivery==="queue"` → 卡片显示「等待执行」；`"startNow"` → 「执行中」 | `HOST@1031329` 同函数尾部 | 高 |
| `requestedDelivery` 省略时的默认 | **跟随宿主 `state.inputRouting.mode`**：`c.requestedDelivery ?? (mode==="guide"?"guide": mode==="enqueue"?"queue":"startNow")` | `CLI@14762292` | 高 |
| 附带证据：在途命令册 | `state.pendingCommands[] = {commandId,clientId,type,state:["accepted","executing"],at}` | `XYX@641941` 区段 `M$` | 高 |

## 5. 与现有实现的差距（对照 `pending.rs` / `projection.rs`）

### 5.1 已覆盖（形状正确）

| 实现 | 官方对应 | 判定 |
|---|---|---|
| `parse_pending` 读 `interactionId/kind/anchorRowId/createdAt/payload.kind` 并强校验顶层=payload | `$$` | 一致 |
| `pending_kind` 三分支（含 `workspaceHookReview`） | `kind` 枚举 | 一致 |
| `parse_permission_options` 读 `optionId/label/kind`（官方卡内三者必填） | `ok` | 形状一致；**投影已改逐项宽容**（坏项跳过并计入 `invalid_option_skipped`，不再整卡丢弃，见 5.2#13） |
| `parse_questions` 读 `question/header/options[].{value,label,description,preview}/multiSelect` | `z$`/`A$` | 一致 |
| `parse_control` 要求 `activeWorks` 为数组、`kind` 必填、`foregroundExecutionId` 可缺 | `rk` | 一致（故意窄） |
| snapshot 与 `state.updated` 都按「整键替换」处理 `pendingInteractions` | 官方 patch 即整数组 | 一致 |
| `fullAccess` 一律拒绝 | `fullAccessOption.response.decision==="deny"`，远端解析本就等于拒绝 | 一致且更保守 |

### 5.2 当前被丢弃 / 缺失（按严重度）

| # | 差距 | 官方字段 | 影响 |
|---|---|---|---|
| 1 | **完全没有队列模型**：`SessionView` 无 `queue`，snapshot/`patch.queue` 被忽略，也无 `deleteQueueItem/reorderQueueItem/editQueueItem/sendQueuedNow/setAutoDrain` | `P$`/`Ax`/`Df` | 手机端永远看不到「已排队」列表，也无法取消/重排/立即发送 |
| 2 | **ACK 只读 `status`，丢弃 `result`** | `result.type=inputAccepted/delivery/inputId/messageId` | 无法区分「已开始」与「已排队」；用户以为发出去了，其实还在队列里 |
| 3 | **不发 `requestedDelivery`**，完全依赖宿主 `inputRouting.mode` | `sendText.requestedDelivery` | 运行中发送时行为由宿主决定，产品无法表达「插话/排队」意图 |
| 4 | **不处理 `heldQueueDisposition` / `expectedHeldQueueItemIds`** | `CLI@14510809` | 宿主 `mode==="choice"` 且队列非空时 sendText 必被拒（`heldQueueDispositionRequired`），当前会表现为 `rejected` |
| 5 | 丢弃 `option.response`（`decision/permissionUpdates`） | `Ut` | 无法预判该选项实际效果（如「Always allow in this project」会写 addRules） |
| 6 | 不读 `payload.fullAccessOption`（独立字段，不在 `options[]`） | `E$` | UI 无法展示「Full access」项；现有 `optionId==="fullAccess"` 硬闸仍安全，但属字符串兜底而非形状取证 |
| 7 | `option.kind` 白名单只有 `allowOnce`/`deny` | 真实卡常见 `allowAlways`（optionId `allowAlways`/`allowSession`） | 「Always allow in this project」「Always allow in this session」被 `forbidden_option`；策略上可接受，但需明示这是**产品选择**而非协议不支持 |
| 8 | 丢弃 `payload.detail`、`payload.display`、`payload.origin` | `E$` | 手机端只能看 `summary` 一行，Bash 的 `command` 正文（在 `detail`）看不到 |
| 9 | userInput 丢弃 `currentQuestionIndex`、`answerDrafts`、`input`、`schema`（仅取 `schema.interaction`） | `O$` | 多题分页/草稿无法还原；`plan_approval`/MCP elicitation 子类只读了名字 |
| 10 | control 只留 `activeWorks[].{kind,foregroundExecutionId}` | `_$`/`rk` | 丢弃 `phase/sessionEnded/canStop/stopState/stopTargetKind/lastError/apiRetry` 与 `activeWorks[].{sourceCommandId,startedAt}`；`stopTargetKind` 本是判「是否 primaryTurn」的直接依据 |
| 11 | `classify_ack` 把 `stale`/`failed` 归入 `Unknown` | `status` 全值域含 `stale`/`failed` | 保守但语义丢失：`stale` 本可映射「刷新后重试」，`failed` 本可映射「确定失败」 |
| 12 | 忽略 `state.pendingCommands[]`、`state.inputRouting`、`state.availability.{queueEdit,sendQueuedNow}` | 同名 | 无法在 UI 上正确置灰队列按钮 |
| 13 | （已修，2026-10-29 R0）单个坏选项曾让整张审批卡被丢弃 | `ok`/`z$` | 已改逐项宽容 + `invalid_option_skipped` 计数；**现场「审批消失」仍未归因**（无真实帧） |

## 6. 无法证实项（如实列出，未作为结论）

1. **`pauseReason:"manual"` 的写入点**：枚举存在（`XYX@640428`），但 `zcode.cjs` 内仅找到 `"stopped"`/`"error"` 两处写入；手动暂停只观测到 `setAutoDrain(false)` → 内部 `queueAutoDrain` 变更事件，未见 `pauseReason` 赋值。**不能断言手动暂停会带 `manual`**。
2. **`delivery.requested:"auto"` 的语义**：值域里有 `auto`，但未找到任何写入 `"auto"` 的位置（CLI 一律写 `startNow`/`queue`/`guide`）。
3. **队列项的「消费/出队」是否有独立事件名**：只见到 `dispatch.state` 迁移与 `items[]` 消失；`turn.steerDrained` 属于 steer 通道，**不等于** queue 出队事件。未找到 `queue.drained` 之类事件名。
4. **`attachments[]` 元素完整形状**：卡/队列里只确认是引用对象数组，元素 schema（`bs`/`VH`）未展开取证。
5. **`display`（`ws`）联合的全部 kind**：只确认它是「多 kind 展示树」且 `.catch(undefined)` 允许解析失败，未逐 kind 枚举。
6. **`origin`（`rt`）值域**：仅确认存在，未取到枚举。
7. **`workspaceHookReview`（`Sx`）**：只读到开头字段（`reviewFlowId/generation/interactionId/sessionId/taskId/runId/workspaceIdentity/bundleDigest/items[]`），未完整取证；本轮不涉及。
8. **Bash 审批的具体 `detail` 内容**：确认 `detail = 事件 payload.input`（`CLI@14630300`），但 CLI 的 tool input 未按工具逐一展开，故「Bash 一定是 `{command,…}`」属推断，未取证。
9. **服务端（云 relay）语义**：本报告全部来自客户端安装包；relay 服务端如何生成/去重这些状态不在取证范围。
10. **运行时实测**：本轮零实连、零帧捕获；所有结论均为静态包内形状，未与真实流量对齐（建议后续用一次真实 queued 发送做 live 对账）。
