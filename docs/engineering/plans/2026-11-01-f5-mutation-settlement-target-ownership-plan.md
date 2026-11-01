# F5 变更结算目标归属与补扫成功判定实施计划

> **执行要求：** 按步骤先写并运行失败回归，再实现最小修复；计划归档到 `docs/engineering/plans/`。

**目标：** 修复批量目标部分接管留下永久 pending，以及补扫等待竞争后把 degraded/旧快照误报 `applied=true` 的问题。

**架构：** 前端 epoch waiter 保留批量结构，只在票根目标集内移除被接管目标，并沿用原 timer。后端保留现有扫描单飞；`scan_after_mutation` 把入口 deadline 传给后续单飞等待，并依据快照 degraded 和 epoch 双条件决定 applied。

**技术栈：** React Query、TypeScript、Vitest、Rust、std::sync、Cargo。

---

### Task 1：复现前端批量部分接管丢失 waiter

**文件：**
- 修改：`tests/scanBus/mutationSettlement.test.ts`

- [x] **步骤 1：新增两种回执顺序测试**

场景一：批量 A/B 的 applied=false 回执先注册门槛 10；新票根接管 A 并以门槛 20 等待。发送 epoch 11 后只允许 B 结算，A 仍 pending；发送 epoch 21 后 A 才结算。

场景二：先创建新 A 票根并注册门槛 20，再处理迟到的旧 A/B applied=false 回执（门槛 10）。旧回执只能为仍由旧票根拥有的 B 等待；epoch 11 只结算 B，epoch 21 结算 A。

补充显式清理场景：建立 A/B waiter 后调用 `clearPendingSessions([A])`，A 被清理而 B waiter/监听器仍保留；epoch 11 仍能只结算 B。

运行：`pnpm vitest run tests/scanBus/mutationSettlement.test.ts -t "批量部分接管"`

预期：旧实现下至少一个场景会让 B pending 在 epoch 11 后仍未结算，或让 A 被旧票根清理。

- [x] **步骤 2：新增原截止时间测试**

使用 fake timers：批量 A/B waiter 以 100ms 截止；60ms 时新票根接管 A，再投递同一旧票根的重复 applied=false 回执；推进至原始 100ms 后，B 必须已经结算，A 仍 pending，且 200ms 前后只能 invalidate 一次。若部分接管重置或重复回执复制 B timer，断言失败。

运行：`pnpm vitest run tests/scanBus/mutationSettlement.test.ts -t "原截止时间"`

- [x] **步骤 3：确认失败后再改生产代码**

保存前两步失败输出，确认失败来自 B 的 pending/截止时刻断言，而不是 listener 或 QueryClient 测试装配错误。

---

### Task 2：按目标集合拆分前端 waiter

**文件：**
- 修改：`src/lib/query/optimisticSessions.ts`
- 测试：`tests/scanBus/mutationSettlement.test.ts`

- [x] **步骤 1：让 waiter 持有内部票根副本**

新增内部 helper，根据目标 `sessionKey` 做去重并生成 `PendingMutationTicket` 副本；不得修改调用方持有的 ticket。

- [x] **步骤 2：接管时只移除目标交集**

新增 waiter 目标收缩 helper：

```ts
function retainWaiterTargets(waiter: EpochWaiter, removed: SessionKey[]): boolean {
  const removedKeys = targetKeysOf(removed);
  const remaining = waiter.ticket.targets.filter((target) => !removedKeys.has(sessionKey(target)));
  if (remaining.length === waiter.ticket.targets.length) return false;
  waiter.ticket = { ...waiter.ticket, targets: remaining };
  return remaining.length > 0;
}
```

在 `cancelSupersededWaiters()` 和 `cancelWaitersForTargets()` 中，只有目标集空时才从 Set 移除并 `clearTimeout`；部分收缩时沿用原 waiter 与 timer。

- [x] **步骤 3：迟到/重复回执只等待仍归旧票根所有的目标**

在 `awaitReflectedSnapshot()` 创建 waiter 前筛选 `pending.get(sessionKey(target))?.owner === ticket.owner`。没有剩余目标时返回空取消函数；同一 QueryClient、owner 已有 waiter 时不新建 timer。

- [x] **步骤 4：运行前端结算回归**

运行：`pnpm vitest run tests/scanBus/mutationSettlement.test.ts tests/scanBus/optimisticSessions.test.ts tests/scanBus/pollArbitration.test.tsx`

预期：交叠批量的 A、B 分别只受当前 owner 结算；旧命令不得清新 pending；B 保留原 timeout。

---

### Task 3：复现后端竞争扫描误报 applied

**文件：**
- 修改：`src-tauri/src/adapter/scan_bus.rs`（测试模块及 `#[cfg(test)]` 探针）

- [x] **步骤 1：添加“空闲后被抢单飞权”屏障测试**

测试先发布种子快照并记录 gate epoch。动作线程在 `scan_after_mutation` 通过空闲检查后停在 test-only probe；竞争线程取得 `ScanFlight` 并等待释放屏障；放行动作线程后，它必须在传入 wait deadline 内结束，返回 degraded、`applied=false`，并向注入通道补排恰好一条 Action。最后释放竞争线程并验证清理单飞位。

运行：`cd src-tauri && cargo test --lib adapter::scan_bus::tests::mutation_scan_race_keeps_single_deadline_and_reports_not_applied`

预期：旧实现等待固定 `SCAN_WAIT_LIMIT`，越过短测试 deadline，且会把返回的 degraded 快照报告成 `applied=true`。

- [x] **步骤 2：补充成功谓词回归**

覆盖 `(degraded=false, epoch>gate) => applied=true`、`degraded=true => false`、`epoch<=gate => false`。失败路径必须补排 Action，不清 `IN_FLIGHT`。

---

### Task 4：统一后端 deadline 并验证 MutationScan

**文件：**
- 修改：`src-tauri/src/adapter/scan_bus.rs`

- [x] **步骤 1：保留入口绝对 deadline**

`scan_after_mutation()` 继续只在入口创建一次 `deadline`。前置等待使用 `deadline.saturating_duration_since(Instant::now())`；离开该等待后，将剩余时长传入 `scan_once_with_limit(ScanReason::Action, remaining)`，不能重新使用 `SCAN_WAIT_LIMIT`。

- [x] **步骤 2：只对满足成功谓词的结果回 true**

补扫返回后计算 `applied = !snapshot.degraded && snapshot.epoch > gate_epoch`。为 false 时调用 `note_timeout()`、`request_scan(ScanReason::Action)`，并在 `MutationScan` 中回带原 gate epoch 与实际快照。

- [x] **步骤 3：覆盖已超时和竞争后快照检查**

deadline 到达时重新读取最新 snapshot；若它已满足成功谓词可返回 true，否则返回 degraded snapshot 与 false。任何分支都不得把旧/降级快照当成功。

- [x] **步骤 4：运行后端回归**

运行：`cd src-tauri && cargo test --lib adapter::scan_bus::tests::mutation_`。

预期：成功、普通超时、竞争后超时和 Action 补排测试全部通过；无双重单飞。

---

### Task 5：最终定向验证与记录

**文件：**
- 检查：`src/lib/query/optimisticSessions.ts`
- 检查：`src-tauri/src/adapter/scan_bus.rs`
- 更新：本计划的验收记录

- [x] **步骤 1：运行完整定向验证**

运行：

```powershell
pnpm vitest run tests/scanBus/mutationSettlement.test.ts tests/scanBus/optimisticSessions.test.ts tests/scanBus/pollArbitration.test.tsx
cd src-tauri
cargo test --lib adapter::scan_bus::tests::mutation_
cargo fmt -- --check
cargo clippy --lib -- -D warnings
```

- [x] **步骤 2：检查差异与记录结果**

运行 `git diff --check`，确认未改 `docs/MASTER-PLAN.md`、IPC schema、用户提供的 evidence 目录；记录通过数量、既有警告和未运行的实机验收。

- [x] **步骤 3：提交本组代码与计划**

只暂存本计划、相关设计文档、`optimisticSessions.ts`、`mutationSettlement.test.ts`、`scan_bus.rs`，提交信息：`fix(scan): settle overlapping mutations safely`。

---

## 验收记录

- 新增的 4 个前端回归在旧逻辑下失败；目标子集收缩后通过。移除重复 waiter 去重后，截止时间测试因重复 invalidate 失败；恢复去重后通过。把 `clearPendingSessions` 临时恢复为整 waiter 取消后，显式清理回归按预期失败；恢复目标子集收缩后通过。
- 后端屏障用例在旧逻辑下耗时约 5.01s，违反 250ms deadline；修复后 1s deadline 内返回 `applied=false`，并只补排一次 Action。计时从屏障释放后开始，屏障准备阶段另验证落在 deadline 内。
- `cargo test --lib adapter::scan_bus::tests`：54 passed；`commands::session::`：12 passed；`commands::settings::`：2 passed。
- 3 个定向 Vitest 文件：26 passed。
- `cargo fmt -- --check`、`cargo clippy --lib -- -D warnings`、`git diff --check`：通过。
- `pnpm check`：格式、i18n、错误码、TypeScript 与生产构建通过；ESLint 有 3 条未修改移动端文件的既有 Hook 警告，无错误。
- 真实桌面/手机运行验收未执行；竞态与事件收敛以确定性屏障和单元测试覆盖。
