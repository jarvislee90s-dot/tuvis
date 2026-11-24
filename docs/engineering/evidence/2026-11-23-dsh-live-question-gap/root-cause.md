# 实时待答为什么无法作答：两条问答入口被我接错了一条

## 结论（一句话）

官方 DSH 有**两条互斥的作答入口**：`userQuestions/answer` 只服务 **`continued`**（回合结束后补答），
而**正常实时待答（`open`）走 `user-questions/request` waterfall + `$events/result` 回执**。
我上一轮只实现了前者，所以我建的可作答卡永远不会在实时提问时出现——**不是 UI 缺陷，是取数通道选错**。

## 证据 1：投影只为「timed 工具」建立 active 条目

`@deepseek-ai/dsh-user-questions/lib/index.js` 的投影折叠：

```js
case "tool/call": {
  if (!fold.timed || event.data.name !== "ask_user_question") return fold;   // ← 非 timed 直接跳过
  ... active: [..., { callId, questions, state: "open" }]
}
```

`timed` 的判定：工具 schema 必须声明 `timeout` 参数。

```js
function isTimedAskUserQuestionSchema(tool) {
  return tool.name === "ask_user_question" && "timeout" in tool.parameters.properties;
}
```

## 证据 2：本机真实会话的 schema 没有 timeout

只读解析最新转写 `session-aed8892b-…`（未读凭据、未发消息）：

```
request/header 总数: 10
ask_user_question 参数集（全部形态）: {('questions',)}
带 timeout 的 header 数: 0
→ 投影 timed = false
```

因此该会话的提问**从未进入** `userQuestions.active[]`，我这侧读到空集。

## 证据 3：`answer` 只认 `continued`，对 `open` 直接返回 false

```js
answer(agent, callId, answer) {
  this.assertLiveRoot(agent);
  const question = this.continued(agent).find((item) => item.callId === callId);
  if (question === void 0) return false;      // ← open 的题在这里就出局
  ...
}
```

即便我能把 `open` 组塞进快照，`userQuestions/answer` 也无法作答，只会得到 `false`。

## 证据 4：实时待答的真实入口是 waterfall

官方客户端（`dsh-client-ui-user-questions/lib/client.js`）注册的是事件瀑布，不是轮询：

```js
ctx.remote.$on(`user-questions/request`, (request, next) => answerQuestion(ctx, this, request, next, cards))
```

- 服务端 `ask()` 通过 `ctx.waterfall("user-questions/request", request, noAnswerer)` 把请求**推**给已注册的
  answerer；没人接就抛 `NO_PROVIDER`；
- 客户端拿到后建卡，答案通过 **`POST /api/$events/result`** 回执
  （`{clientId, eventId, outcome:{kind:"next"|..., value}}`）送回；
- `attachWait`（mux 流）只用于 **timed** 前台倒计时，与这条链互斥——我上轮已正确避开它，但也因此
  完全没接上 waterfall。

## 证据 5：该通路的准入条件（决定可行性）

`dsh-api-gateway` 的 `dispatchRpc("$events/result")`：

```js
requireInteractions(peer, endpoint);                                   // peer.interactions === "answer"
if (peer.principal.kind !== "owner") throw ... "only owner Peers answer forwarded Remote events";
```

即：回执方必须是 **owner principal 且 interactions=answer**。官方控制台页之所以能答，是因为它是宿主
自己的浏览器会话（`client-connection` 为该浏览器发布的就是 `{kind:"owner"}` peer）。

我们后端的 Peer 是插件签发的受限 peer，是否具备 owner 身份需在实现时逐一核验——
**若不能取得 owner，这条链在隔离条件下不可达**，届时必须如实交接而不是硬造。

## 控制台侧已具备的一半

早期控制台曾通过业务放行表转发 `$events` 与 `/api/$events/result`。现役控制台按总纲纯转发，由官方宿主处理两端的业务权限。手机原生页仍需要自己的已授权 Peer。

## 第二个现象：卡片闪一下又消失

已确认一个自身缺陷：`read_question_snapshot` 把**读取失败**与**确实没有题组**都折叠成 `Ok(None)`，
前端又把它当成 `verified:false` 卸载卡片；而这次读取与发送共用同一条 PeerChannel 槽位，
一旦被占用/超时，卡片就会消失、下个周期再出现——与「闪一下又消失」吻合。
该项与通道选择无关，应独立修：读失败要保留上一份同目标快照并显示可诊断状态，不得等同于「无题组」。

## 因此需要的改动

1. **修闪烁**（必做、独立）：读取三态化（成功 / 失败保留上次 / 确证无题组），失败不卸载卡片。
2. **接 waterfall**（让实时待答可答的唯一路径）：后端作为 gateway 客户端订阅 `$events`、
   接 `user-questions/request`、以 `$events/result` 回执；先验证 owner/interactions 准入是否可得。
3. 若 (2) 的准入不可得：手机端只读卡明确写「此类型提问请在控制台作答」并给入口，
   把限制如实交接，不宣称已可作答。

## 本轮未取证

- 未在浏览器里抓 `/dsh/questions` 的返回序列（Safari 未开「允许 Apple Events 执行 JavaScript」），
  因此「闪烁」的精确时序未逐帧确认；上述缺陷是**代码级必然**，不是推测的现象归因。
- 未验证我们 peer 能否取得 owner principal（需实现时探测）。

---

# owner 准入探测结果（静态证据链，已闭合）

## 结论：**owner 准入成立**

`$events/result` 只接受 owner principal；而我们的插件凭据恰好产生 owner principal。
逐环节对照（均取自已安装官方包与本仓源码）：

| 环节 | 证据 |
| --- | --- |
| 插件兑换带 `interactions=answer` | `packages/dsh-remote-access/src/index.ts:490`：`GET /connect?token=…&interactions=answer` |
| 宿主据此发布 peer | `dsh-client-connection`：`publishPeer(interactions)` → `new Peer(owner, { principal: { kind: "owner" }, interactions })` |
| cookie 携带该 peer id | 同上 `authorizeConnect`：`id: publish(interactions)` 写入 `version:2` cookie |
| API 请求解析回同一 peer | `authenticateApi()` 返回 `payload.id`；`admit()` → `this.peers.get(id).peer` |
| mux 升级取同一 peer | `admitConnection()` 同样走 `admit()`，`ownsSocket` 保证一条 socket |
| `$events` 订阅只需 answer | `openWireStream`：`requireInteractions(peer, "$events")`，**无 owner 断言** |
| `$events/result` 需 owner + answer | `dispatchRpc`：`requireInteractions` 且 `peer.principal.kind !== "owner"` 才拒 |

因此 **订阅 `$events` 与回执 `$events/result` 在现有插件凭据下都可达**，无需新配对、新入口或提权。

## 由此确定的线路细节

- `open` 帧 payload 必须是 `{"args": {}}`（空对象）——`openRemoteEvents` 对多余键直接
  `gateway/arguments-invalid`；
- 首帧是 `{type:"ready", clientId, host}`，`clientId` 是后续回执的必填字段；
- 事件帧 `{type:"waterfall" | "emit", event, eventId, agentId, request}`；
  `waterfall` 才需要答案，`emit` 只广播；
- 回执 `POST /api/$events/result`，body 是**单键包装**：
  `{"args": {clientId, eventId, outcome: {kind:"result", value} | {kind:"next"} | {kind:"rejected", error}}}`。官方 `parseRemoteEventResultPayload` 要求外层只有 `args`；
- 回执方必须与开流方**同一个 peer**（`client.peer !== peer` → `interactions-forbidden`），
  所以订阅与回执必须复用同一条 PeerChannel；
- `user-questions/request` 的 `request` 形状即官方 `ask()` 传入的
  `{questions, agent?, signal?}` 投影（`projectRemoteEventRequest` 去信号后转成纯 JSON）。

---

# 活体实测（本轮，真实会话，只读）

在用户 Safari 已登录的 `/m` 页内用同步 XHR 读本机端点（未读凭据、未写宿主）：

**会话**：`session-aed8892b-…`，instance `6f984ac6…`，状态 `processing`。

**题组端点**：

```
GET /m/api/v1/dsh/questions?instanceId=6f984ac6…&sessionId=session-aed8892b-…
→ 200 {"verified":true, "snapshot":{"groups":[], "revisionToken":"50fe65e6…", "asOfSeq":357}}
```

`verified:true` 说明**投影读取成功**（不是读失败），`groups:[]` 说明投影里**确实没有可作答题组**。

**同时刻消息流**（同会话）：

```
ask_user_question 调用 seq: [20, 52, 59]
窗口最后四条: 56:assistant, 57:user, 58:thinking, 59:tool-call
```

`seq 59` 是**最新一条消息**且正是一次 `ask_user_question`，其后没有任何 `tool-result`
——即此刻**确实有一个提问在等用户作答**，而投影同时刻为空。

这条对照排除了「恰好没在等」的巧合，把根因钉在协议分支上：
该提问不是 timed 工具（schema 无 `timeout`），所以投影不建 `active` 条目；
真正等待它的是 `user-questions/request` waterfall，而我只订阅了投影与 `answer`。

---

# 补充：`agentId` 与会话归属（实现前必须钉死）

## 结论：`agentId == sessionId`（官方自己的判据）

`@deepseek-ai/dsh-api-session-controller/lib/index.js` 的 `pendingInput(sessionId)` 直接拿
**agentId 与 sessionId 比较**：

```js
pendingInput(sessionId) {
  const kinds = new Set();
  for (const { event, agentId } of this.ctx.typertGateway.getPendingRemoteEvents()) {
    if (String(agentId) !== sessionId) continue;      // ← 同一命名空间
    kinds.add(event === "approval/request" ? "approval"
            : event === "user-questions/request" ? "question" : "other");
  }
  return kinds;
}
```

即：waterfall 帧上的 `agentId` 就是主会话的 `sessionId`。因此**按会话归属筛选实时待答是可行的**，
不需要额外映射表，也不必退化成「按实例维度」的粗粒度暴露。

## 另一个有用的发现：宿主自己就有「待答输入」投影

同一函数说明宿主已经把 pending remote events 汇总成「approval / question」两类。
这意味着 `approval/request` 与 `user-questions/request` 走**同一条 waterfall 通道**——
本任务只处理后者，但订阅层应保留 event 名以便将来接审批，不必重做通道。

## 帧上的字段（据此实现）

- `waterfall` 帧：`{type:"waterfall", event, eventId, agentId, request}`
  - `request` 是官方 `ask()` 的投影，**已剥掉 `agent` 与 `signal`**
    （`projectRemoteEventRequest` 明确删除这两个键，其余键必须是无损 JSON）；
  - 因此 `request.questions[]` 可直接喂给现有的 `questions.rs::parse_question`；
- `emit` 帧：`{type:"emit", event, args}`，只广播、**不可回执**。

## 回执语义（实现必须照此）

`receiveRemoteEventResult`（`dsh-api-gateway`）：

```js
this.removeRemoteEventDelivery(pending, client);
if (outcome.kind === "result")   settleRemoteEvent(pending, {kind:"result", value});
else if (outcome.kind === "rejected") cancelRemoteEvent(pending, ...);
else if (pending.deliveries.size === 0) settleRemoteEvent(pending, {kind:"next"});
```

推论（重要）：

1. **不回执 = 永久挂起**：只要还有 delivery 未移除，事件就留在 `pendingRemoteEvents` 里，
   提问端会一直等。所以**订阅方有义务对每个收到的 waterfall 事件给出答复**。
2. **`next` 是"我不处理"**：移除本方 delivery；当没有任何 answerer 还剩时，
   以 `next` 结算（瀑布继续 → 无人接管则 `NO_PROVIDER`）。这与「从未订阅」等价，安全。
3. 因此对**不是我们处理的事件**（如 `approval/request`）也应回 `next`，而不是沉默。
4. **重订阅会补投未决事件**：`openRemoteEvents` 会把 `pendingRemoteEvents` 里尚未被本 client
   投递过的条目补发（`for (const pending of this.pendingRemoteEvents.values()) this.deliverRemoteEvent(...)`）。
   —— 这让长连接断开重订后能自动追回错过的提问，不需要额外补扫。
5. **回执必须来自投递过它的同一个 client**：`if (pending === undefined || !pending.deliveries.has(client)) return;`
   即 eventId 必须与开流时拿到的 `clientId` 配对，且同一条 peer。

---

# 阻断项：`user-questions/request` 同时承载审批与计划审查

## 事实（已独立复核）

`intent` 是题目上的官方字段，`dsh-plan-mode` 用它把「批准计划并退出 plan mode」表达成一次普通提问：

```js
// @deepseek-ai/dsh-plan-mode/lib/index.js
answer = await interaction.ask({
  questions: [{
    id: "plan-review", header: "Plan review",
    question: "Approve this plan and leave plan mode?", detail: args.plan,
    options: [{label:"Approve",...},{label:"Keep planning",...}],
    intent: { kind: "plan-review", approve: "Approve", callId: exec.callId }   // ← 关键
  }],
  agent, signal: exec.signal
});
```

`ask()` 把 `intent` 一路带到 `user-questions/request` waterfall；`intent.kind` 的取值至少包括
`"approval"` 与 `"plan-review"`（`lib/index.js` 的 `ask()` 里对两者分别有校验分支）。

## 为什么这是阻断

我们的实时问答链路把 `request.questions[]` 直接 `parse_question` 后当作**普通可作答题**渲染
（`parse_question` 只取 `id/question/detail/header/options/multiSelect`，**丢弃 `intent`**）。
后果：手机端可以对一条 `intent=plan-review` 的提问点「Approve」，
**等于批准退出 plan mode**——越过「执行/计划审批只在控制台」的现行设计边界
（Spec §1.1 第 3 条 / §8.2 R05–R07「审批走控制台」）。

这不是理论风险：`dsh-plan-mode` 就是这么发问的，且它与普通 `ask_user_question` 走**同一个 event 名**，
仅靠 `intent` 区分。

## 修复要求

1. 待答项必须**保留 `intent`**（至少 `kind` 与 `approve` 标签），不得丢弃；
2. 带 `intent` 的提问在手机端**只读**，并明确引导「请在控制台作答」；
3. 后端在 `answer_live_question` 拒绝带 `intent` 的 eventId（纵深防御，不只靠 UI）；
4. 回归：构造 `intent.kind = "plan-review"` 的 waterfall 帧，断言
   - 待答项带 intent，
   - 提交被**明确拒绝**（不是静默成功），
   - 宿主零写入。

## 另外两个已确认阻断（同一轮 review）

- **提交时序**：`answer_live_question` 在收到宿主确认**之前**就把 eventId 记为 settled。
  若回执失败（取消 / 送达未知 / 宿主拒绝），题会从本地待答表消失，用户再也看不到它；
  正确做法是**先认领（防止并发双击）→ 回执 → 仅成功才置 settled**，失败则放回待答表并报错。
- **溢出丢弃未回执**：待答表超上限时丢最旧一条，但只记日志、未发 `next`。
  按「沉默 = 永久挂起」语义，那条提问会一直等。必须在丢弃时补发 `next`。
