# Codex APP 远程停止当前回合 · 专项设计（已实现并真机验收）

> 日期：2026-10-22。
> 状态：实现完成；macOS `26.1013.40934` 真机端到端验收通过（打断 → 回合终态 → 屏障释放 → 队列接力）。Windows 与其他 build 未实测，能力位保持关闭。
> 分支：`codex/remote-interrupt-turn`；基线：`origin/main@6fb3868e`。
> 依据：《Codex Desktop IPC 远程控制 · 需求说明书》（冻结设计，提交 `e39d7665`）§5 明列「interrupt / steer」**不在本 Spec 范围**，「这些能力可以在后续专项 Probe / Spec 中独立增加」——本文即该专项 Spec；同批 Probe 取证见 `docs/superpowers/reviews/2026-10-22-codex-interrupt-probe-results.md`。

## 一、目标与背景

远程端对 Codex APP 会话只提供「等它做完」的语义：桌面回合未终结时，手机端新消息按「忙态留队 / FIFO 按序」进 `codex_actions` 队列，队列视图给 `blockedReasonCode = native_turn_pending`，并注明「等待 Turn 完成后自动继续」。

这条自动恢复路径的前提是**回合自己会结束**。回合若长期不结束（长任务、卡在等待、或只是忘了关），手机端就没有任何出口：

- 队列的「人工解除」按钮按设计只对 `native_delivery_unknown` / `compat_outstanding` 两类**无自动恢复路径**的屏障可见，`native_turn_pending` 明确不给；
- 「人工解除」即便给了也只是清屏障，桌面仍忙时写前忙态守卫会把消息放回队首（`busy_before_write`），消息照样发不出去；
- 桌面上那个「问题」模态并不在协议请求通道里（见 §3），手机端既看不到也答不了。

因此需要一个**真的让桌面停下**的远程出口：打断当前回合 → 回合转终态 → 在册屏障释放 → 队列接力下一条。

## 二、范围

### 2.1 本期增量

1. 能力位 `CapabilitySet.interrupt`，按 Desktop build 精确解锁（**未实测的 build 保持 false**）。
2. 方法表登记 `thread-follower-interrupt-turn`（基值 4），参数形态与 Desktop 侧方法表一致。
3. `adapter.interrupt_turn()`：取 owner lease → 组装 params → dispatch。
4. 终态门 `turn_is_terminal` 认 `interrupted`（**与能力位同批**——不认它，打断后屏障反而永久在册）。
5. 端点 `POST /m/api/v1/codex/turn/interrupt`：`expectedTurnId` 取在册原生屏障绑定的 turnId 作安全阀；审计留痕。
6. 手机端队列面板「停止桌面回合并继续」按钮：两步确认 + 风险如实披露 + 结果如实回报。

### 2.2 边界

- **只打断，不恢复**：本能力不负责续跑、不负责重发被打断回合未完成的工作——被打断的工作就是丢失，文案如实披露。
- **不碰 steer**：`thread-follower-steer-turn`（回合运行中追加指令）同属冻结 spec §5 的 out-of-scope，本期不做，避免一次开两个口子。
- **不放开 permissions / command / blocking_input**：本轮只新增 `interrupt` 一个能力位，其余保持原状。
- **不改变屏障语义**：屏障仍由终态证据驱动释放，本能力只提供「让回合进入终态」的手段。
- **不新增 pending 通道**：Codex APP 的「问题」模态是 `AgentMessage.questions[]` 形态而非协议 request，手机端仍不提供问答卡（见 §5.3）。

## 三、协议事实（Probe 取证摘要）

Desktop 侧方法表（打包产物静态核对 + 真机探针）：

| 事实 | 值 |
| --- | --- |
| 方法名 | `thread-follower-interrupt-turn` |
| 基值版本 | **4**（带 envelope hostId 时为 5） |
| 参数 | `{conversationId, mode, expectedTurnId}`；`mode` 取 `user-stop` / `system` |
| 返回 | `{interruptedTurnId, ok}`（`goalPauseError` 仅在 `mode=user-stop` 且 goal 暂停失败时出现，仍带 `interruptedTurnId`） |
| 安全阀 | `expectedTurnId` 与当前活动回合不符 → Desktop 静默不动作（返回 `interruptedTurnId: null`） |
| 打断后回合状态 | `interrupted`（Desktop 把该 Turn 的 status 置为 `interrupted`） |
| 版本协商 | Desktop 对「params 缺 `expectedTurnId` 的 interrupt」期望版本 **3**；带上则期望基值 4 —— 故本实现**始终带上 `expectedTurnId`** |

## 四、设计

### 4.1 能力位与门控

`CapabilitySet` 新增 `interrupt: bool`。`false` 的含义与其他能力位一致：**该 build 尚未活体验证，不表示协议实现不存在**。仅 macOS `26.1013.40934` 开启（真机证据见 Probe 记录）；Windows 4894 / 3656、macOS 31256 保持 false——「未经验证的 Desktop 版本或方法不得盲目启用对应能力」（冻结 spec §3.5）。

未开启时端点返回 503 `capability_unavailable`，**零 IPC 写入**（有集成用例钉死）。

### 4.2 终态门

`mirror.rs turn_is_terminal` 原只认 `completed` / `failed`。Desktop 打断后把回合记为 `interrupted`——不认它的话：打断 → 回合 `interrupted` → 终态门不放行 → 屏障永久在册 → 队列永远暂停（比打断前更糟：用户以为已解围，横幅却消失了）。认它之后闭环成立。

`runtime_status` 派生不受影响：`interrupted` 不是 `inProgress`，不触发 `active` 判定。

### 4.3 端点与安全阀

```
POST /m/api/v1/codex/turn/interrupt
{ "sessionId": "...", "turnId": "..."? }
→ 200 { "ok": true, "uncertain": false, "interruptedTurnId": "...|null", "expectedTurnId": "..." }
→ 200 { "ok": true, "uncertain": true, "interruptedTurnId": null,
        "expectedTurnId": "...", "reasonCode": "timeout|…" }   // RPC 超时等：结果未知
→ 409 { "error": "no_expected_turn"|"stale_context"|"desktop_rejected", "reason": "<中文>" }
→ 503 { "error": "capability_unavailable"|…, "reason": "<中文>" }   // 未送达/未验证
```

- `turnId` 缺省时，服务端取**在册原生屏障绑定的 turnId**（`codex_actions` 中 route=desktop_ipc ∧ state=accepted ∧ compat_outstanding=1 ∧ compat_turn_id 非空，取 seq 最大一条）作 `expectedTurnId`；这也是「打断的正是卡住我的那个回合」的精确性保证。
- **无阀拒发（2026-10-22 复审 P1-1 补强）**：请求未带 `turnId` 且在册屏障查不到绑定 turnId → 409 `no_expected_turn` 拒绝（零 IPC 写入）；DB 读失败 → 500。两路都**绝不降级为「无阀照发」**——adapter 层对 `expectedTurnId == None` 同样硬门拒发（缺该参数的 interrupt Desktop 照收不误，等于无阀打断当前活动回合，含桌面手开的无关回合）。
- Desktop 侧无可打断回合时回 `interruptedTurnId: null`——端点如实回 null，**不谎报打断成功**，也不当作错误。
- **投递结果三分支（2026-10-22 复审 P2-3 补强）**：明确受理 → 200 `uncertain:false`；RPC 超时等「帧已交 writer 但结果未知」→ 200 `uncertain:true`（打断可能已生效，不得报「停止失败」诱导重按）；Desktop 明确拒绝（Rejected）→ 409 `desktop_rejected`（不是 503——服务本身可用）。拒绝类错误响应统一带中文可读 `reason`，无能力 build 的打断按钮由队列视图新增的 `canInterrupt` 能力位在**前端源头隐藏**（P2-2）。
- 端点只负责「发出打断」并触发一次结算；屏障释放由终态证据驱动（`release_turn_if_complete`），回合终态帧到达时调度循环自动接力。
- 破坏性动作进审计（`codex_interrupt`），摘要只含 id 与结果词，不含正文；**拒绝/不确定/明确拒绝各臂同样留审计**（P3-1），结果词区分 `interrupted` / `no_active_turn` / `uncertain:<reason>` / `rejected` / `refused:<reason>`。

### 4.4 前端

队列面板在 `blockedReasonCode === "native_turn_pending"` 时展示「停止桌面回合并继续」——与「人工解除」是两种不同的东西：解除只清屏障（消息仍要等桌面空闲才投得出去），打断是真的让桌面停下。

两步确认（未确认不发任何请求），确认文案如实披露「该回合未完成的工作会丢失」；打断成功／结果不确定（指令已发出但未知，劝阻连续重按）／桌面无可打断回合／失败，**四种结果**分别如实回报（不静默）。按钮仅 `canInterrupt === true`（队列视图下发的 interrupt 能力位）可见——无能力 build 在源头隐藏，不给渲染出来只会 503 的假出口（复审 P2-2）。

## 五、风险与诚实边界

### 5.1 打断会丢工作

这是**破坏性动作**。前端用两步确认 + 风险披露承担这一层；服务端不做「该不该打断」的判断。

### 5.2 能力位是 build 级的

Desktop 升级后 build 变化 → 精确匹配失效 → 能力位自动回落 false（拒绝打断）而不是盲发未验证方法。这是刻意的保守方向。

### 5.3 不解决「问题模态」

Codex APP 的「问题」是 `AgentMessage.questions[]` 形态（模型提问挂在消息上），**不是**协议 request，因此 `conversationState.requests[]` 为空，tuvis 的 pending 投影拿不到它（只认 `item/commandExecution/requestApproval` / `item/permissions/requestApproval` / `item/tool/requestUserInput` 三个 method）。本能力**不改变**这一点：打断能让回合停下，但「在手机端回答那个提问」仍不在本期范围。

### 5.4 与「人工解除」的分工

`native_turn_pending` 仍不给人工解除按钮：它的自动恢复路径现在多了一条**用户主动**的路（打断），与「自动」路径互补而非替代。

## 六、改动明细（文件级索引）

> 本节是「怎么改的」的落点：改动落在哪、为什么必须成对、关键代码形态。PR 描述与本节同源。

### 6.1 为什么必须成对改

打断的难点不在「发一个请求」，而在**打断之后链路要自己走完**。原链路上四个环节会因「回合没结束」而卡住：

```
手机发消息
  → ① 屏障闸：native_turn_pending（accepted 且绑定了 turnId）——一个 Turn 未终结不投第二条
  → ② 写前忙态守卫：桌面还 active → NotSent(busy_before_write) → 动作放回队首
  → ③ 终态门：turn_is_terminal 要求该 turnId 在镜像中以 completed/failed 在场
  → ④ 屏障释放 → 队列接力下一条
```

打断只解决「让回合停下来」——即让 ③ 的门能被打开。所以能力位与终态门**必须同批**：不认 `interrupted` 的话，打断 → 回合变 `interrupted` → 终态门不放行 → 屏障**永久**在册 → 队列永远暂停（比打断前更糟：横幅消失、消息照堵）。

同理，② 决定了「人工解除」不是解法（只清屏障，桌面仍忙时消息照被放回队首）——所以不是放开解除按钮，而是新增一个真的让桌面停下的动作。

### 6.2 文件级清单

| 文件 | 改动 | 关键点 |
| --- | --- | --- |
| `codex_desktop_ipc/types.rs` | `CapabilitySet` 增 `interrupt: bool` | 语义同既有能力位：`false` = 未活体验证，**不代表协议不存在** |
| `codex_desktop_ipc/profile.rs` | 方法表登记 `thread-follower-interrupt-turn`（基值 4）；仅 macOS `26.1013.40934` 置 `interrupt: true` | 其余 build 保持 false；不因单平台实测而跨 build 放行 |
| `codex_desktop_ipc/mirror.rs` | `turn_is_terminal` 增 `Some("interrupted")` | 与能力位同批的理由见 §6.1 |
| `codex_desktop_ipc/adapter.rs` | 新增 `interrupt_turn()` | 能力门在**发帧之前**（未开启 → 零 IPC 写入）；`mode: "user-stop"`；`expectedTurnId` 始终带 |
| `database/dao/codex_actions.rs` | 新增 `outstanding_native_turn_id_conn()` | 取 route=desktop_ipc ∧ state=accepted ∧ fence=1 ∧ turnId 非空 ∧ seq 最大的一条 |
| `services/codex_control/queue.rs` | 新增 `outstanding_native_turn_id()` 包装 | 供端点取安全阀参数 |
| `remote/codex_ipc.rs` | 新增 `POST /codex/turn/interrupt` 端点 + `codex_interrupt` 审计 | 端点**不直接清屏障**（只由终态证据释放）；桌面无可打断回合时如实回 `null` |
| `remote/server.rs` | 注册路由 | 走既有 gated nest，门禁结构性覆盖 |
| `mobile/codexIpcApi.ts` | 新增 `interruptCodexTurn()` | 三种结果分别如实回报，不静默 |
| `mobile/CodexQueuePanel.tsx` | 新增按钮 + 两步确认臂 | 仅 `native_turn_pending` 可见；确认前零请求；风险文案披露「工作会丢失」 |
| `i18n/locales/{zh,en}.json` | 各增 7 键 + 横幅补一句 | 键集由 `check:i18n` 保证两侧对齐 |

### 6.3 关键代码形态

```rust
// 终态门（mirror.rs）——不认 interrupted 则打断后屏障永久在册
matches!(self.turn_status(turn_id).as_deref(),
         Some("completed") | Some("failed") | Some("interrupted"))
```

```rust
// adapter.interrupt_turn()——能力门在发帧之前；expectedTurnId 始终带
if !self.current_caps().interrupt { return not_sent(&envelope_id, "capability_unavailable"); }
let mut params = json!({ "conversationId": key.thread_id, "mode": "user-stop" });
if let Some(t) = expected_turn_id { params["expectedTurnId"] = json!(t); }
self.dispatch("thread-follower-interrupt-turn", version, params, lease, validity).await
```

```sql
-- DAO：在册原生屏障绑定的 turnId（安全阀来源）
SELECT compat_turn_id FROM codex_actions
 WHERE thread_id = ?1 AND route = 'desktop_ipc' AND state = 'accepted'
   AND compat_outstanding = 1 AND compat_turn_id IS NOT NULL
 ORDER BY seq DESC LIMIT 1
```

## 七、验收证据（macOS `26.1013.40934`）

| 项 | 结果 |
| --- | --- |
| 协议契约探针（故意错的 `expectedTurnId`） | 方法名 / 版本 4 / params 形态 / owner 路由全对；`{interruptedTurnId: null, ok: true}`，**未打断任何回合** |
| 端到端（真打断） | 发消息 → `accepted` + turnId → 屏障在册 `native_turn_pending` → 打断 → 200 `{ok: true, interruptedTurnId: <turnId>}` → 屏障释放（`blockedReasonCode: null`、DB `fence=0`）→ 日志「Turn 终态证据齐备，释放顺序限制」 |
| 自动化 | Rust：3 个新集成用例 + 终态门 / DAO / profile 单测；前端：面板 6 个新用例；全量 `cargo test --lib` 与 `pnpm test` 绿 |
| 未验证 | Windows（4894 / 3656）与其他 macOS build 的 `interrupt` 能力——保持关闭 |
