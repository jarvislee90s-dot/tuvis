# F2/F4 R-3 双向快照版本仲裁设计

## 背景

现有前端已用 `applySessionsSnapshot()` 为事件和变更结算设置版本水位，但 `useSessionsQuery()`
轮询仍调用旧 `get_all_sessions`，没有版本号。当前 `queryFn` 只在返回前检查一次水位；检查通过
后，事件仍可能先写入更高版本，再被 React Query 延迟提交的旧查询结果覆盖。

反方向也必须封住：轮询可能先提交 N+2，但如果版本水位没有在该次提交时推进，迟到的 N+1 事件
仍会通过事件闸。必须在 Query 的最终缓存写入点仲裁，而不是只在 `queryFn` 开始或结束时检查。

## 目标与验收

1. 保留旧 `get_all_sessions` IPC，新增 `get_sessions_snapshot`，回执包含
   `version / epoch / mode / degraded / data`。
2. 回执字段都来自同一个 `Arc<Snapshot>`；`mode` 进入 `Snapshot`，命令不得分别读取快照和全局
   MODE 再拼装。
3. 桌面前端会话查询改用新命令；缓存对组件仍提供原 `SessionsResponse` 形状。
4. 查询最终提交、事件写入、变更结算共用版本水位；更低或相同版本不得覆盖缓存。Query 轮询
   只有在最终提交成功时才推进 version 与已应用 epoch。
5. 已应用 epoch 也用于收敛事件丢失后的变更等待器；pending 生命周期仍保持现有有界策略。
6. 回归覆盖旧查询在新事件后提交、旧事件在新轮询后到达、查询提交微任务与事件交错、无插队轮询
   正常提交、事件丢失后的轮询 epoch 收敛。

## 方案

### 后端单 Arc 快照读取

在 `adapter::Snapshot` 增加 `mode`，发布时把该值与 version、epoch、degraded、data 一起存入同一
Arc。`scan-bus-snapshot` 事件载荷也从 `snap.mode` 读取，避免再次拼接全局 MODE。

新增 `SnapshotRead` DTO 和 `scan_bus::current_snapshot_read()`：先取得一个成功发布的 Arc，再从
该 Arc 一次性复制五个字段。`commands::session::get_sessions_snapshot` 在 `spawn_blocking` 中调用
该读口并注册为 Tauri command。旧 `get_all_sessions` 保持不变，旧客户端不受影响。

### 前端轮询的最终提交仲裁

`getSessionsSnapshot()` 返回完整 DTO。轮询 `queryFn` 只从该 DTO 取出 `data`，并用 WeakMap 按
对象身份暂存 version/epoch；缓存对组件仍是 `SessionsResponse`。

在 `useSessionsQuery()` 配置 React Query v5 `structuralSharing(oldData,newData)`。本仓库安装的
`@tanstack/query-core` 5.104.0 中，`Query.setData()` 在 dispatch 缓存更新前调用 `replaceData()`，
后者调用自定义 `structuralSharing`；该同步点就是轮询结果的最终提交仲裁点。若查询数据携带的
version 不大于窗口水位，返回现有缓存对象；若更大，则通过同一个 `claimSnapshotVersion()` 推进
水位并返回查询数据。已应用 epoch 在最终提交后的 microtask 中登记和结算，避免在缓存尚未提交时
清 pending。

事件和变更结算继续调用 `applySessionsSnapshot()`，它使用同一个 `claimSnapshotVersion()`，再同步
写 Query 缓存。为了避免此同步写经过 `structuralSharing` 时因水位刚推进而被自己拒绝，写入期间对
该 data 对象设置一次性授权标记，并在 `finally` 中清除。无版本 metadata 的普通乐观更新照常通过，
不推进权威版本水位。

### 事件丢失时的 epoch 收敛

成功提交的轮询快照在缓存提交后调用 `noteAppliedSnapshotEpoch(epoch, snapshot)`。该函数更新已应用
epoch，并将符合 `epoch > gateEpoch` 的等待器交给现有 `settleWaiters()`。因此即使广播事件丢失，
下一次轮询拿到的权威快照也能完成变更结算；pending 清理前缓存已由同版本 snapshot 写入。

## 回归测试

1. Rust 快照 DTO：发布时设 MODE=Away，再把全局 MODE 改为 Active；`current_snapshot_read()` 仍返回
   发布 Arc 中的 Away，且 version/epoch/data 与该 Arc 一致。
2. Query 微任务交错：轮询请求取得 N+1；在 `queryFn` 已返回、Query 最终提交之前安排 N+2 事件；
   最终缓存必须保留 N+2。
3. 反向交错：轮询最终提交 N+2 后再发 N+1 事件；缓存仍为 N+2。
4. 无插队轮询：新版本查询照常写入缓存并推进最大 version/epoch。
5. 事件丢失收敛：挂起 applied=false 变更等待器，只让轮询提交 epoch 更大的 snapshot；等待器
   结算、pending 清除、invalidate 按现有规则发生。
6. 旧 IPC 命令仍可调用；新 `useSessionsQuery` 使用 `get_sessions_snapshot`。

## 修改边界

- Rust：`src-tauri/src/adapter/scan_bus.rs`、`src-tauri/src/commands/session.rs`、
  `src-tauri/src/lib.rs`。
- 前端：`src/lib/api/session.ts`、`src/lib/query/optimisticSessions.ts`、
  `src/lib/query/queries/sessions.ts`、`src/lib/query/queries/scanBus.ts`。
- 回归：`src-tauri/src/adapter/scan_bus.rs` 单测、`tests/scanBus/pollArbitration.test.tsx`、
  `tests/scanBus/snapshotSync.test.tsx`、`tests/scanBus/mutationSettlement.test.ts`。

不修改 `docs/MASTER-PLAN.md`、REST 响应结构、Session 字段或 F5 pending 超时策略；旧 IPC 命令
继续兼容旧前端。
