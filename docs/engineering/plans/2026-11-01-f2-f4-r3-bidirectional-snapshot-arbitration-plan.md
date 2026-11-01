# F2/F4 R-3 双向快照版本仲裁实施计划

> **执行要求：** 使用 `superpowers:executing-plans` 逐项推进；先新增失败回归，再实现。直接在当前功能分支执行。

**目标：** 查询轮询、快照事件、变更结算始终保留已应用的最高快照版本，并用轮询 epoch 收敛丢失的事件。

**架构：** 后端从同一 `Arc<Snapshot>` 生成 versioned snapshot DTO，并保留旧会话 IPC。前端仍把 `SessionsResponse` 暴露给组件，但轮询 `queryFn` 将 DTO 中版本元数据绑定到返回对象；React Query v5 的 `structuralSharing` 在最终缓存提交时执行版本闸。事件和变更结算同步写入仍调用同一个版本 claim helper。

**技术栈：** Rust/Tauri 2、React 19、TanStack Query 5.104.0、Vitest。

---

### Task 1：先写后端同一快照 DTO 回归

**文件：**
- 修改：`src-tauri/src/adapter/scan_bus.rs`（测试模块）

- [x] **步骤 1：新增失败测试**

新增 `snapshot_read_uses_metadata_from_published_arc`：重置状态，将 MODE 设为 Away，通过
`publish_for_test()` 发布响应，保存已发布的 `Arc<Snapshot>`，再把全局 MODE 改成 Active；调用
计划中的 `current_snapshot_read()`，断言 version、epoch、mode、degraded、data 与保存的 Arc 完全
一致，特别断言 read.mode 仍是 Away。

- [x] **步骤 2：运行测试确认失败**

运行：`cd src-tauri && cargo test --lib adapter::scan_bus::tests::snapshot_read_uses_metadata_from_published_arc`

执行记录：通过故障注入把 DTO 的 mode 来源临时改为全局 `mode()`，测试按预期失败
（实际 Active、期望发布时 Away）；恢复为同一 Arc 的 `snapshot.mode` 后，定向测试通过。

---

### Task 2：后端发布 versioned snapshot read

**文件：**
- 修改：`src-tauri/src/adapter/scan_bus.rs`
- 修改：`src-tauri/src/commands/session.rs`
- 修改：`src-tauri/src/lib.rs`

- [x] **步骤 1：把 MODE 固定进 `Snapshot`**

给 `Snapshot` 增加 `mode: Mode`。`publish()` 在创建 Arc 时采样 MODE 并一起存入；
`empty_snapshot()` 采样当前 MODE；`degraded_snapshot()` 复制源快照 MODE；测试中的直接
`Snapshot { ... }` 构造同步补齐字段。

- [x] **步骤 2：让事件模式来自同一 Arc**

移除 `snapshot_payload(snap, reason, mode, scanned_at_ms)` 的独立 mode 参数，载荷改为
`snap.mode`；`emit_snapshot_event()` 不再另读全局 MODE。保持原有事件字段名与 reason。

- [x] **步骤 3：新增读口和 Tauri command**

新增 serde camelCase DTO `SnapshotRead { version, epoch, mode, degraded, data }`。抽出
`current_snapshot_or_scan() -> Arc<Snapshot>` 作为成功首读的唯一等待内核；旧
`current_or_scan()` 返回 Arc 内的 data，新 `current_snapshot_read()` 从同一个 Arc 生成 DTO。
在 `commands/session.rs` 新增 async `get_sessions_snapshot`，用 `spawn_blocking` 调用新读口并把
`JoinError` 转为 command error，不回造空快照。保留旧 `get_all_sessions` 实现和签名。

在 `lib.rs` 的 invoke handler 注册 `commands::session::get_sessions_snapshot`。

- [x] **步骤 4：运行后端回归**

运行：`cd src-tauri && cargo test --lib adapter::scan_bus::tests::snapshot_read_uses_metadata_from_published_arc`

预期：1 passed；MODE 改变后 DTO 仍携带发布时 Arc 中的 MODE。

---

### Task 3：先写前端最终提交竞态回归

**文件：**
- 修改：`tests/scanBus/pollArbitration.test.tsx`

- [x] **步骤 1：轮询完成微任务与事件交错**

将测试 API mock 从 `getAllSessions()` 改为 `getSessionsSnapshot()`。新增 deferred 交错：轮询拿到
version N+1 后，在 Query 最终提交前的 microtask 应用 N+2 事件，断言最终缓存仍为 N+2。旧
`queryFn` 只在 await 前后检查水位，会在 check 通过后被该事件插队，该测试应在旧实现下失败。

- [x] **步骤 2：反向交错**

新增用例先让轮询最终提交 N+2，再发送 N+1 事件；断言事件不回退缓存、水位仍为 N+2。

- [x] **步骤 3：轮询 epoch 收敛事件丢失**

新增用例：创建 applied=false 变更等待器，丢弃相应 `scan-bus-snapshot` 事件，只让轮询返回
`epoch > gateEpoch` 的新 DTO；断言轮询提交后等待器结算、pending 清除、invalidate 发生一次。

- [x] **步骤 4：运行测试确认失败**

运行：`pnpm vitest run tests/scanBus/pollArbitration.test.tsx`

执行记录：实现前两条竞态用例各自复现缓存回退；另临时停用 poll epoch 通知，事件丢失用例因
`invalidateQueries` 未调用而失败。恢复最终提交仲裁和 epoch 通知后，四个前端回归文件全部通过。

---

### Task 4：实现共同的版本闸与 query final-write hook

**文件：**
- 修改：`src/lib/api/session.ts`
- 修改：`src/lib/query/optimisticSessions.ts`
- 修改：`src/lib/query/queries/sessions.ts`
- 修改：`src/lib/query/queries/scanBus.ts`

- [x] **步骤 1：定义并调用新读命令**

在 `src/lib/api/session.ts` 增加 `SessionsSnapshotRead` 类型（version/epoch/mode/degraded/data）
和 `getSessionsSnapshot()`，调用 Tauri `get_sessions_snapshot`。保留 `getAllSessions()` 作为旧 IPC
兼容函数。

- [x] **步骤 2：添加基于对象身份的查询元数据**

在 `optimisticSessions.ts` 增加 `WeakMap<object,{version,epoch}>`；查询返回 DTO 后把 version/epoch
绑定到其 `data` 对象并仍返回该 `SessionsResponse`。缓存 data 形状不变。

- [x] **步骤 3：共用版本 claim helper**

抽出 `claimSnapshotVersion(label, version, data)`：仅接受严格大于窗口水位的版本，同时保存该窗口
最后权威 data。`applySessionsSnapshot()` 和查询最终仲裁都调用此 helper。事件/结算调用
`setQueryData` 时，对对象做同步一次性授权标记；`finally` 中移除标记，避免 Query 的
`structuralSharing` 把刚授权的事件写入当作重复查询拒绝。

- [x] **步骤 4：在 `structuralSharing` 最终提交点仲裁**

`sessions.ts` 用 `getSessionsSnapshot()` 获取 DTO、登记 data metadata；`useQuery` 配置
`structuralSharing(oldData,newData)` 调用 `optimisticSessions.ts` 仲裁器。新 version 才 claim
并返回新 data；相同/更旧 version 返回现有 cache 或该窗口的最后权威 data。删除旧
`lastAppliedSnapshotVersion()` 的 `queryFn` 前置检查。

- [x] **步骤 5：让 poll 提交也登记 epoch**

查询仲裁接受 DTO 后，在 Query 数据提交后的 microtask 调用
`noteAppliedSnapshotEpoch(epoch,{version,data})`，使既有 epoch waiter 监听器可在事件丢失时收敛。
事件监听将 epoch/snapshot 一并传给 `noteAppliedSnapshotEpoch`；`settleWaiters()` 中的 snapshot
写入继续调用同一个 `applySessionsSnapshot()`。

- [x] **步骤 6：运行前端回归**

运行：`pnpm vitest run tests/scanBus/pollArbitration.test.tsx tests/scanBus/snapshotSync.test.tsx tests/scanBus/mutationSettlement.test.ts`

预期：旧事件、迟到查询、事件重复、mutation settlement 与事件丢失后的轮询都保留最高版本。

---

### Task 5：接线审查与完整定向验证

**文件：**
- 检查：`src-tauri/src/adapter/scan_bus.rs`
- 检查：`src-tauri/src/commands/session.rs`
- 检查：`src-tauri/src/lib.rs`
- 检查：`src/lib/query/optimisticSessions.ts`
- 检查：`src/lib/query/queries/sessions.ts`
- 更新：`docs/engineering/plans/2026-11-01-f2-f4-r3-bidirectional-snapshot-arbitration-plan.md`

- [x] **步骤 1：运行 Rust 定向测试与 lint**

运行：`cd src-tauri && cargo test --lib adapter::scan_bus::tests`、
`cargo test --lib commands::session::`、`cargo fmt -- --check`、
`cargo clippy --lib -- -D warnings`。

- [x] **步骤 2：运行前端定向测试**

运行：`pnpm vitest run tests/scanBus/pollArbitration.test.tsx tests/scanBus/snapshotSync.test.tsx tests/scanBus/mutationSettlement.test.ts`

- [x] **步骤 3：审查最终差异**

运行 `git diff --check`、`git diff --stat`、`git status --short`。确认没有修改旧
`get_all_sessions` 的返回类型、没有修改 `docs/MASTER-PLAN.md`，用户已有的
`docs/engineering/evidence/dsh-native-rpc/` 未跟踪目录保持原样。

---

## 验收记录

- `cargo test --lib adapter::scan_bus::tests`：52 passed。
- `cargo test --lib commands::session::`：12 passed。
- `cargo test --lib versioned_session_snapshot_command_is_registered`：1 passed；同时断言旧命令仍注册。
- `cargo fmt -- --check`、`cargo clippy --lib -- -D warnings`、`git diff --check`：通过。
- 4 个定向 Vitest 文件：28 passed。
- `pnpm check`：格式、i18n、错误码、TypeScript 与生产构建通过；ESLint 有 3 条未修改移动端文件的既有 Hook 警告，无错误。
- 真实多窗口/移动端运行验收未在本地 UI 执行；事件丢失收敛与双向乱序由 Vitest 覆盖。
