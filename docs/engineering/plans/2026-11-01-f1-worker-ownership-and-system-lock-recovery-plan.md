# F1 worker 执行权与共享进程状态恢复实施计划

> **执行要求：** 使用 `superpowers:executing-plans` 逐项推进；先写并运行失败回归，再实现。当前任务在既有功能分支内执行，不新建 worktree。

**目标：** 修复共享进程状态锁 poison 后无法恢复，以及 Scan Bus 接管后旧 worker 仍可能继续工作的 F1 缺口。

**架构：** `SHARED_SYSTEM` 遇到 poisoned guard 时丢弃 `System` 并清除 poison。Scan Bus 增加执行权门闩，使 worker 代次交接、扫描准入、心跳与托盘/MODE 副作用可线性化；扫描本体继续由 `IN_FLIGHT` 单飞，不在 supervisor 中强制清位。

**技术栈：** Rust、Tauri 2、`std::sync`、现有 `cargo test --lib` 与 `cargo clippy`。

---

### Task 1：先写共享进程状态锁 poison 回归

**文件：**
- 修改：`src-tauri/src/adapter/mod.rs`（测试模块）

- [x] **步骤 1：新增失败测试**

新增 `poisoned_shared_system_lock_discards_state_and_is_reusable`：在独立的
`Arc<Mutex<Option<System>>>` 上启动线程，在线程持锁时 panic；断言锁已 poisoned；调用计划中的
`lock_shared_system(&lock)`，断言 guard 内为 `None`，释放 guard 后 `lock.is_poisoned()` 为
`false`。线程 panic 通过 `join().is_err()` 捕获，不让测试本身失败。

- [x] **步骤 2：运行测试确认失败**

运行：`cd src-tauri && cargo test --lib adapter::tests::poisoned_shared_system_lock_discards_state_and_is_reusable`

预期：因 `lock_shared_system` 尚未定义而编译失败。

---

### Task 2：实现共享 `System` 锁恢复

**文件：**
- 修改：`src-tauri/src/adapter/mod.rs`

- [x] **步骤 1：实现锁获取辅助函数**

实现 `lock_shared_system(lock: &Mutex<Option<System>>) -> MutexGuard<'_, Option<System>>`：

```rust
match lock.lock() {
    Ok(guard) => guard,
    Err(poisoned) => {
        let mut guard = poisoned.into_inner();
        *guard = None;
        lock.clear_poison();
        log::error!("adapter: SHARED_SYSTEM 锁已污染，丢弃旧 System 并重建");
        guard
    }
}
```

按编译器要求导入 `MutexGuard`。将 `get_all_sessions_inner()` 的进程刷新和宿主存活过滤，
以及 `sync_unread_sessions()` 的宿主检查，全部改为调用此辅助函数。

- [x] **步骤 2：运行 poison 回归**

运行：`cd src-tauri && cargo test --lib adapter::tests::poisoned_shared_system_lock_discards_state_and_is_reusable`

预期：1 条测试通过，恢复后的下一次锁获取成功且不再 poisoned。

- [x] **步骤 3：新增并运行快照续发回归**

在 Scan Bus 测试中先执行同一隔离锁的 poison/恢复，再通过现有 `publish_for_test` 连续发布两份
测试响应，断言第二份 `version == first.version + 1`。运行：
`cd src-tauri && cargo test --lib adapter::scan_bus::tests::poison_recovery_allows_next_snapshot_publication`

预期：1 条测试通过；该测试只使用内存快照，不启动应用、不读真实会话目录。

---

### Task 3：为扫描准入与代次交接建立同步原语

**文件：**
- 修改：`src-tauri/src/adapter/scan_bus.rs`（状态、扫描内核、supervisor）

- [x] **步骤 1：新增执行权顺序回归**

新增测试覆盖三种确定顺序：扫描准入先设置 `IN_FLIGHT` 时，接管复核不得更换代次；接管先
更新代次时，旧代扫描准入必须被拒绝；supervisor 读到旧心跳后若 worker 已续期，交接必须被
拒绝。断言交接代码不调用 `IN_FLIGHT.store(false, ..)`。

- [x] **步骤 2：运行测试确认失败**

运行：`cd src-tauri && cargo test --lib adapter::scan_bus::tests::scan_admission_and_takeover_share_execution_gate`

预期：因门闩和准入辅助函数尚未实现而编译失败。

- [x] **步骤 3：实现门闩和扫描准入**

增加 `WORKER_EXECUTION: Lazy<Mutex<()>>`。扫描内核使用一个短临界区完成可选 generation
校验、`IN_FLIGHT` 单飞准入、扫描起始时间和 epoch 记账，然后释放门闩执行扫描。外部动作扫描
也经同一准入；worker 扫描额外传入 generation。等待已有扫描时释放门闩，并在重试准入时重新
核验 generation。扫描 RAII 清理在门闩内清 `IN_FLIGHT` 和扫描起点。

supervisor 的实际接管函数在门闩内重新检查 `IN_FLIGHT` 和心跳是否仍过期；扫描已开始或旧
worker 已续期时，本拍不接管。否则在同一临界区替换唤醒通道、启用新 generation 并启动新
worker。初始启动也通过门闩分配 generation。

- [x] **步骤 4：运行准入回归**

运行：`cd src-tauri && cargo test --lib adapter::scan_bus::tests::scan_admission_and_takeover_share_execution_gate`

预期：1 条测试通过；已有 `ensure_fresh` 与单飞超时测试仍保持原有语义。

---

### Task 4：阻止接管后的旧 worker 恢复执行

**文件：**
- 修改：`src-tauri/src/adapter/scan_bus.rs`（worker 循环、生产 round、supervisor 测试）

- [x] **步骤 1：新增屏障时序回归**

新增 `old_generation_cannot_resume_work_after_takeover`，使用 channel/barrier 和真实
`spawn_worker` 循环：旧代收到首个原因后阻塞；向旧通道排入 `Event`；执行真实接管，新代
完成一轮注入式扫描与状态更新；释放旧代。断言发布版本与旧代恢复前一致、旧代扫描数和托盘/MODE
副作用计数均未增加、旧 worker 退出且只有一个当前 worker。

- [x] **步骤 2：运行回归确认失败**

运行：`cd src-tauri && cargo test --lib adapter::scan_bus::tests::old_generation_cannot_resume_work_after_takeover`

预期：旧实现会在释放屏障后继续执行假扫描/状态更新，测试失败。

- [x] **步骤 3：把 generation 检查接入真实 worker 路径**

`WorkerRound` 接收 worker generation。循环在收到原因后、批量取出事件前后、事件延迟结束后
检查代次；失效时退出。扫描使用带 generation 的准入入口。`scan_worker_round` 在更新托盘、
写入 MODE 和 emit 档位事件前，通过执行权门闩完成代次校验及副作用。心跳写入也与接管串行化。

- [x] **步骤 4：运行屏障回归**

运行：`cd src-tauri && cargo test --lib adapter::scan_bus::tests::old_generation_cannot_resume_work_after_takeover`

预期：1 条测试通过；旧代不能消费积压事件后继续操作、不能再次扫描或更新状态。

---

### Task 5：完成 F1 定向验证与记录

**文件：**
- 检查：`src-tauri/src/adapter/mod.rs`
- 检查：`src-tauri/src/adapter/scan_bus.rs`
- 更新：`docs/engineering/plans/2026-11-01-f1-worker-ownership-and-system-lock-recovery-plan.md`

- [x] **步骤 1：运行 adapter 定向单元测试**

运行：`cd src-tauri && cargo test --lib adapter::`

预期：adapter 模块所有测试完成；若出现既有失败，逐项对照本轮新增失败并记录，不以忽略 CI
代替验证。

- [x] **步骤 2：运行 Rust lint**

运行：`cd src-tauri && cargo clippy --lib -- -D warnings`

预期：退出码 0，无 warning。

- [x] **步骤 3：审查最终差异与计划状态**

运行：`git diff --check`、`git diff --stat` 与 `git status --short`。确认没有改动
`docs/MASTER-PLAN.md`，且用户已有的 `docs/engineering/evidence/dsh-native-rpc/` 未跟踪目录
保持原样。将本计划中已完成步骤勾选，并记录测试命令、结果和未做的真实应用故障注入验收。

预期：工作区差异只包含 F1 实现、回归测试、设计与计划记录；用户已有的证据目录不变。真实
线程故障注入仍如实列为后续运行验收，不把单测当成活体证据。

## 验收记录（2026-11-01）

| 命令 | 结果 |
| --- | --- |
| `cargo fmt` | 通过 |
| `cargo test --lib adapter::scan_bus::tests` | 48 passed，0 failed |
| `cargo test --lib adapter::scan_bus::tests::scan_admission_and_takeover_share_execution_gate` | 1 passed，0 failed；覆盖扫描在飞、心跳恢复和 generation 失效三种交接判据 |
| `cargo test --lib adapter::tests::poisoned_shared_system_lock_discards_state_and_is_reusable` | 1 passed，0 failed；真实 `SHARED_SYSTEM` 被 poison 后清除并可继续发布递增版本 |
| `cargo test --lib adapter::` | 163 passed，0 failed |
| `cargo clippy --lib -- -D warnings` | 通过，0 warnings |
| `git diff --check` | 通过 |

真实 release 构建中的线程卡死/恢复故障注入、多窗口运行仍未做；该证据属于活体验收，不能由以上单测替代。
