# F5 宠物、跳转与手机归档变更结算实施计划

> **执行要求：** 按步骤先写并运行失败回归，再实现最小修复；文档保存在 `docs/engineering/`。

**目标：** 将桌面卡片已经验证的乐观更新、pending 过滤和 epoch 结算接到宠物 X、跳转已读和手机 APP 软归档。

**架构：** 前端使用共享会话票根；read 操作有独立 intent，宠物状态机消费 pending。focus 回执可选携带写入后的 gate epoch。手机 Board 对 REST 请求与 SSE 连接分别使用代次闸，并在归档时暂停旧 SSE、权威回取后重连；远端 hide/unhide 写入后请求 Scan Bus Action。

**技术栈：** React 19、TanStack Query、TypeScript、Vitest、Tauri IPC、Rust、Axum、Tokio。

---

### Task 1：新增显式已读意图 helper 与回归

**文件：**
- 修改：`src/lib/query/optimisticSessions.ts`
- 修改：`tests/scanBus/optimisticSessions.test.ts`

- [x] **步骤 1：测试 explicit read 不走 dismiss 分类**

种入 read/unread 卡和另一个未读会话，调用计划中的 `applyOptimisticSessionRead(qc, {agentType:"claude", id:"r1"})`；断言目标 `unread=false`、卡片和计数不变、另一会话不变，pending 仍可压住旧快照回写。

运行：`pnpm vitest run tests/scanBus/optimisticSessions.test.ts -t "显式标已读"`

预期：因 helper 不存在而失败；测试不得通过 `getSessionDismissBehavior` 或 dismiss 命令模拟 read。

- [x] **步骤 2：实现显式 read helper**

实现 `applyOptimisticSessionRead(queryClient, target)`：创建单目标票根，调用 `cancelSupersededWaiters`，将 pending mode 置为 `read`，只更新匹配会话的 `unread=false`，不修改 total/waiting count。无 Query cache 仍登记 pending。

- [x] **步骤 3：运行 pending 与 mutation regression**

运行：`pnpm vitest run tests/scanBus/optimisticSessions.test.ts tests/scanBus/mutationSettlement.test.ts`

---

### Task 2：宠物 X 复用变更票根

**文件：**
- 修改：`src/components/pet/FoxbellPet.tsx`
- 测试：`tests/pet/foxbell-cards.test.tsx`

- [x] **步骤 1：新增宠物旧快照回归**

模拟宠物卡 X 的 read 与 dismiss 两种语义；断言立即调对应 command，注入旧 sessions query snapshot 后，宠物状态派生结果仍不重建卡。再模拟 `mark_session_read` reject 后的新旧权威 unread 快照，操作前的 green 状态必须恢复；applied=false 时 pending 不提前清除。

运行：`pnpm vitest run tests/pet/foxbell-cards.test.tsx -t "旧 query 快照|未读绿宠物卡|已读命令失败后"`

- [x] **步骤 2：接入统一票根并在状态计算前应用 pending**

X handler 用共享 `getSessionDismissBehavior(session)` 选择 mark-read 或 dismiss，再调用 `applyOptimisticSessionMutation()` 与 `settleSessionMutation()`。数据派生改为先 `applyPendingSessions(data.sessions)` 再更新 `sessionIndexRef` 和 `computePetStatus()`。点击立即从宠物卡状态移除目标。

- [x] **步骤 3：验证宠物与卡片既有语义**

运行：`pnpm vitest run tests/pet/foxbell-cards.test.tsx tests/pet/foxbell-events.test.tsx tests/session/dismissCard.test.tsx`。

---

### Task 3：直达和歧义跳转使用 read waiter

**文件：**
- 修改：`src/hooks/useSessionJump.ts`
- 修改：`src-tauri/src/commands/session.rs`
- 测试：`tests/session/sessionJumpRead.test.tsx`
- 检查：`tests/notification/focusAction.test.tsx`

- [x] **步骤 1：测试直达与歧义读回执**

通过真实 `QueryClient` 测试 hook：直达结果含 `readGateEpoch` 时，缓存立即 unread=false、卡片保留，且不额外调用 `mark_session_read`；epoch 未越门槛前 pending 继续保护。无 gate 的 macOS `via=deep-link` 不做 fallback 写。歧义结果不标已读，`focusHwnd` 成功后即使没有 sessions cache 仍调用 `mark_session_read`；缓存存在时应用 read pending。失败则不创建 read。

运行：`pnpm vitest run tests/session/sessionJumpRead.test.tsx`

- [x] **步骤 2：focus 写回 gate epoch**

让 `mark_read_on_jump()` 在 DB 删除未读行并写 tombstone 后读取 `scan_epoch()`，再排 `ScanReason::Action`，返回 `Option<u64>`。所有成功直达分支将可选 `readGateEpoch` 加到现有 JSON 对象，保留旧 `type` / `via` / `windows` 字段；ambiguous 与失败不带 gate。

- [x] **步骤 3：接入 hook 的 read pending 和兼容回退**

在 `useSessionJump()` 从 `QueryClientContext` 取当前 WebView client。目标 unread 不为 false且当前 client 有会话缓存时，直达 gate 回执创建 `applyOptimisticSessionRead`，以 `{applied:false,version:0,epoch:readGateEpoch}` 调用 `settleSessionMutation`；旧后端缺 gate 时，在有缓存且非 `via=deep-link` 的路径用 `mark_session_read` 获取 MutationOutcome。缺 gate 的 `via=deep-link` 保留 macOS 未验证跳转不读的裁决。歧义 `focusHwnd` 成功后无论有无缓存都持久调用 `mark_session_read`，有缓存时同步创建 read pending；失败不发 read。

- [x] **步骤 4：验证 focus 回执与跨窗口已读回归**

运行：`cd src-tauri && cargo test --lib commands::session::`，以及
`pnpm vitest run tests/session/sessionJumpRead.test.tsx tests/session/sessionCardHeader.test.tsx tests/notification/bellJump.test.tsx tests/session/jumpFallbackHint.test.ts`。

---

### Task 4：手机软归档做乐观隐藏和旧响应隔离

**文件：**
- 修改：`src/mobile/Board.tsx`
- 测试：`tests/mobile/Board.test.tsx`

- [x] **步骤 1：添加旧 REST 与旧 SSE 回归**

用 deferred fetch 启动一条旧 `/sessions` 请求；确认 APP 归档后，在同一轮交互立即移除卡片，再让旧 REST 返回旧卡，卡片不得复活。另保留旧 `MockEventSource`、归档时触发重连；旧连接随后发旧 snapshot 和 queued transition 必须被忽略，新连接首帧接管数据。另以两个并发归档票根验证第一个结束时不会解除第二个 pending/重连事件流。

运行：`pnpm vitest run tests/mobile/Board.test.tsx -t "archive"`

- [x] **步骤 2：失败回滚回归**

让 `hideSession` reject；确认 pending 被清、卡片按归档前快照恢复、旧请求仍不能覆盖恢复后的新一代状态，新 SSE 可继续上报。

- [x] **步骤 3：实现 response generation 与 pending owner**

在 Board 内维护 `(agentType,sessionId) -> token` pending map、REST generation 和 SSE generation。所有 `tick`、transition 补拉、SSE snapshot/transition 的提交都校验其代次并过滤 pending key。确认 APP 归档时立即写 pending、移除目标并停旧 SSE；CLI `closeSession` 路径保持原行为。

- [x] **步骤 4：实现成功回取与恢复**

软归档 API 成功后推进 REST generation，再 `fetchSessions()`；目标缺席则清本 token pending 并提交权威结果。若目标仍在或回取失败，保留 pending，重连 SSE / 后续 tick 再对账。HTTP 失败时清本 token pending、恢复原快照并重启 SSE。旧 token 不得清除新 token。

- [x] **步骤 5：运行手机回归**

运行：`pnpm vitest run tests/mobile/Board.test.tsx tests/mobile/ArchiveBoard.test.tsx`。

---

### Task 5：hide/unhide 写后通知 Scan Bus

**文件：**
- 修改：`src-tauri/src/remote/api.rs`
- 测试：`src-tauri/src/remote/server.rs`
- 测试：`src-tauri/src/commands/session.rs`（focus read gate 顺序）

- [x] **步骤 1：添加调用顺序测试**

给 `/session-hide` 和 `/session-unhide` 注入 Scan Bus test sender；断言 hide 完成 `unread_mark_read` 与 `board_hidden_hide` 后，unhide 完成 `board_hidden_unhide` 后，各收到一条 `ScanReason::Action`。`removed=0` 的 unhide 保持幂等且不要求额外扫描。

运行：`cd src-tauri && cargo test --lib remote::server::tests::board_close_hide_tests`

- [x] **步骤 2：在写入提交后排 Action**

`session_hide` 两个写入 closure 全部成功后调用 `scan_bus::request_scan(Action)`；`session_unhide` 在 removed>0 时调用一次。保持已有 HTTP 状态码、请求 JSON、响应 `ok` 与 `removed` 字段。

- [x] **步骤 3：运行远程端点回归**

运行：`cd src-tauri && cargo test --lib remote::server::tests::board_close_hide_tests`、
`cargo test --lib remote::api::tests::sse_snapshot_frame_carries_version`。

---

### Task 6：全组验证与提交

**文件：**
- 检查：`src/lib/query/optimisticSessions.ts`
- 检查：`src/hooks/useSessionJump.ts`
- 检查：`src/components/pet/FoxbellPet.tsx`
- 检查：`src/mobile/Board.tsx`
- 检查：`src-tauri/src/commands/session.rs`
- 检查：`src-tauri/src/remote/api.rs`
- 更新：本计划验收记录

- [x] **步骤 1：运行定向回归**

运行：

```powershell
pnpm vitest run tests/scanBus/optimisticSessions.test.ts tests/scanBus/mutationSettlement.test.ts tests/pet/foxbell-cards.test.tsx tests/pet/foxbell-events.test.tsx tests/session/dismissCard.test.tsx tests/session/sessionJumpRead.test.tsx tests/session/sessionCardHeader.test.tsx tests/session/jumpFallbackHint.test.ts tests/notification/bellJump.test.tsx tests/notification/focusAction.test.tsx tests/mobile/Board.test.tsx tests/mobile/ArchiveBoard.test.tsx
cd src-tauri
cargo test --lib adapter::scan_bus::tests::mutation_
cargo test --lib commands::session::
cargo test --lib remote::server::tests::board_close_hide_tests
cargo fmt -- --check
cargo clippy --lib -- -D warnings
```

- [x] **步骤 2：运行项目检查并审查差异**

运行 `pnpm check` 与 `git diff --check`；确认 `docs/MASTER-PLAN.md` 未修改、mobile action wire contract 原字段保持、用户 evidence 目录原样。

- [x] **步骤 3：提交**

只暂存本计划、spec、列出的实现与测试文件，提交信息：`fix(session): settle pet jump and mobile archive actions`。

---

## 验收记录

- 显式已读 helper 测试：未实现时因 helper 缺失失败；实现后通过，并确认聚合绿卡保留卡面/计数且旧快照 unread 被 pending 覆盖。
- 宠物 X 测试：旧实现下旧 query 快照使卡片复活；共享票根修复后通过。命令 reject 时，以 deferred active-query invalidation 复现 pending 仍在的回取窗口，rollback 在结算后恢复；另一窗口的 `session-read` 不会取消失败 dismiss 的 rollback。
- 跳转回归：direct focus 写后 readGateEpoch、旧 epoch 等待、无缓存通知浮窗歧义成功、macOS 未验证 deep-link 不读、歧义失败不读均通过；Rust gate 顺序与兼容字段测试 2 passed。
- 手机 Board：旧 REST、旧 SSE snapshot/transition、失败回滚、并发归档 owner 四类回归通过；12 个相关 Vitest 文件合计 115 passed。
- `cargo test --lib commands::session::`：14 passed；`remote::server::tests::board_close_hide_tests`：10 passed；SSE version frame：1 passed。
- `cargo fmt -- --check`、`cargo clippy --lib -- -D warnings`、`git diff --check`：通过。
- `pnpm check`：格式、i18n、错误码、TypeScript 和生产构建通过；ESLint 有 3 条未修改移动端 Hook 警告，无错误。
- 未在真实手机与多 WebView 运行安装构建；该项属于全组实机验收，不以单测替代。
