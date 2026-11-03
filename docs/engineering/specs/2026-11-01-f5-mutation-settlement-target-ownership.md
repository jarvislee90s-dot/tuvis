# F5 变更结算的目标归属与扫描结果判定

## 背景

Scan Bus 的前端 pending 按目标会话记录 owner，但 applied=false 等待器仍以整批票根为单位。批量 A/B 等待期间用户再次操作 A 时，旧实现删除整张 A/B 等待器；新票根只接管 A，B 的 pending 因而失去 epoch 和超时结算路径。若旧批量命令回执在新单卡等待器之后到达，也可能再次取消或覆盖整批等待状态。

后端 `scan_after_mutation` 先等待 `IN_FLIGHT` 清零，再调用带固定 5 秒等待上限的 `scan_once`，并无条件将其结果标成 `applied=true`。另一线程可在空闲检查后先取得单飞权；此时本调用会多等一次完整内部上限，并可能把 degraded 或变更前快照报告为成功。

## 目标

1. 每个 pending 目标都由仍有效的票根或明确超时负责收口；接管 A 不得取消同批 B。
2. 被保留目标的 epoch 等待和超时截止时刻不因拆分、迟到回执或另一目标操作而重置。
3. `applied=true` 只在补扫结果满足 `!degraded && snapshot.epoch > gate_epoch` 时返回。
4. 等待在飞扫描时始终使用 `scan_after_mutation` 入口创建的同一 deadline；超时仍保留 pending，并补排 Action。
5. 保持现有 MutationOutcome、IPC、会话缓存形状和扫描单飞接口兼容。

## 设计

### 前端：票根目标子集

- `PendingMutationTicket` 继续由一个 owner、递增 seq 和目标列表组成；批量 API 不变。
- `EpochWaiter` 内部持有票根副本。创建 waiter 时只纳入当前 pending owner 仍属于该票根的目标，避免已被新票根接管的目标重新进入旧等待。
- 新票根接管目标时，旧 waiter 只移除交集。没有被接管的目标保留在原 waiter；若还有目标则沿用同一个 timer，截止时间不变，若目标集为空才移除 waiter 并清 timer。
- 显式清理目标时采用相同的集合差逻辑，避免清 A 时误取消 B。
- 同一票根的迟到或重复 applied=false 回执不创建第二个 waiter、不重置 timer；如果票根当前没有自己拥有的 pending 目标，则直接忽略。
- waiter 到期或 epoch 越过门槛时，只清仍由该 owner 持有的目标；同一 QueryClient 的一批目标仍合并为一次 invalidate。

不采用“每个目标各自创建 timer”的实现：它容易让同一批快照产生多次 invalidate，也增加监听器和截止时序状态。保留一个 waiter 并收缩其内部目标子集，可保持原 timer 和批量 invalidate 行为。

### 后端：同一 deadline 与成功谓词

- `scan_after_mutation` 在入口创建绝对 deadline。等待既有扫描时每次只等待剩余时长；越过 deadline 后立刻返回当前快照的 degraded 副本、`applied=false`，记录超时并补排一条 Action。
- 既有扫描结束后，调用 `scan_once_with_limit(Action, remaining)`，不再重置为完整 `SCAN_WAIT_LIMIT`。它若在空闲检查后被其他扫描抢先，内部 Busy 等待也只用剩余预算。
- 对 `scan_once_with_limit` 返回值统一验证 `!snapshot.degraded && snapshot.epoch > gate_epoch`。不满足时返回 `applied=false`、保持 gate 不变、记录超时并补排 Action；满足才返回 true。
- 扫描单飞、epoch 递增点和已发布快照语义不变。由本调用实际取得单飞权后进行的扫描仍沿用现有同步执行路径；本设计的 deadline 限制等待其他扫描的预算，不强杀扫描。
- 测试用屏障固定“动作入口确认空闲 → 另一线程抢得单飞 → 动作继续”的顺序，并检查真实返回值、耗时、degraded 和唯一补排的 Action。

## 验收

- 批量 A/B 已等待时接管 A：B 在 A/B 两种回执顺序下仍可由自己的 epoch 门槛收口；重复旧回执不会创建第二个 timer；A 的新票根仍有效。
- 部分接管不重置 B 的原始超时；超时只收口 B，不误清新 A。
- 旧或 degraded 快照永不报告 `applied=true`；有效新快照满足两个条件时才报告 true。
- 屏障测试中，空闲检查后的竞争扫描不会使动作额外等待 5 秒；结果为 `applied=false`，pending 继续等待，且只补排一次 Action。
- 现有单卡、批量、超时、重复点击、版本闸和扫描单飞回归继续通过。

## 修改边界

- 前端：`src/lib/query/optimisticSessions.ts`、`tests/scanBus/mutationSettlement.test.ts`。
- 后端：`src-tauri/src/adapter/scan_bus.rs` 及同模块单元测试。
- 计划与验收记录：`docs/engineering/plans/2026-11-01-f5-mutation-settlement-target-ownership-plan.md`。

不改 F5 的卡片分类、公开 IPC/Mobile schema、变更命令写入内容、Scan Bus 的 worker 生命周期或 `docs/MASTER-PLAN.md`。
