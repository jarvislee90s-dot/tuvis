# F1/F3 冷启动首读与动作等待期限实施计划

> **执行要求：** 使用 `superpowers:executing-plans` 逐项推进；新增行为先写失败回归。该子项目已获用户批准，直接在当前功能分支执行。

**目标：** 冷启动读取等到首个成功快照再返回；动作读取无论是否存在扫描 sender，都严格遵守同一个等待 deadline。

**架构：** `current_or_scan()` 用「是否存在已发布快照」区分初始化未完成和成功空结果，初始化读取等待首个成功发布。`ensure_fresh()` 统一建立 deadline；无 sender 且无在飞扫描时启动去重的后台 fallback 扫描，超时只返回 degraded，不清扫描单飞状态。

**技术栈：** Rust、Tauri 2、`std::sync`、现有 Scan Bus 与 `cargo test --lib` / `cargo clippy`。

---

### Task 1：新增冷启动首读等待回归

**文件：**
- 修改：`src-tauri/src/adapter/scan_bus.rs`（测试模块）

- [x] **步骤 1：新增失败测试**

新增 `current_or_scan_waits_for_successful_initial_snapshot`：持有测试总线锁，设置一次性
`SCAN_START_PROBE` 屏障；启动真实 `scan_once(ColdStart)` 线程并等待其取得单飞权、停在屏障；
再启动 `current_or_scan()` 读取线程。用 channel 等待 `SCAN_WAIT_LIMIT + 200ms`，断言读线程
没有返回。随后释放扫描屏障，等待扫描成功发布空结果，断言读线程返回空 `SessionsResponse`，
且 `snapshot()` 存在并 `degraded == false`。旧实现会在 5 秒后把 degraded 空回执作为正常空列表
提前返回，因此该断言在旧实现下失败。

- [x] **步骤 2：运行测试确认失败**

运行：`cd src-tauri && cargo test --lib adapter::scan_bus::tests::current_or_scan_waits_for_successful_initial_snapshot`

预期：旧实现提前返回空列表，`recv_timeout` 收到结果，回归断言失败；测试随后仍释放屏障并
join 两个线程，避免遗留扫描线程。

---

### Task 2：实现初始化读取等待成功快照

**文件：**
- 修改：`src-tauri/src/adapter/scan_bus.rs`
- 修改：`src-tauri/src/commands/session.rs`（更新入口注释，不改响应类型）

- [x] **步骤 1：修改 `current_or_scan()`**

实现循环：

```rust
pub fn current_or_scan() -> SessionsResponse {
    loop {
        if let Some(snapshot) = snapshot() {
            return snapshot.data.clone();
        }
        if !IN_FLIGHT.load(Ordering::SeqCst) {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                scan_once(ScanReason::ColdStart)
            }));
            if let Ok(snapshot) = result {
                if !snapshot.degraded {
                    return snapshot.data.clone();
                }
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
```

保持初始化读取与动作读取不同：这里等待首个成功发布，不使用 `SCAN_WAIT_LIMIT` 降级为空。
桌面与远程调用已在 `spawn_blocking` 内执行，不阻塞 UI 主线程或 Tokio worker。

- [x] **步骤 2：运行首读回归**

运行：`cd src-tauri && cargo test --lib adapter::scan_bus::tests::current_or_scan_waits_for_successful_initial_snapshot`

预期：1 passed；阻塞期间无响应，发布成功空快照后返回空列表。

---

### Task 3：新增无 sender 与慢扫描的动作 deadline 回归

**文件：**
- 修改：`src-tauri/src/adapter/scan_bus.rs`（测试模块）

- [x] **步骤 1：新增无 sender fallback 屏障测试**

新增 `ensure_fresh_without_sender_times_out_while_fallback_scan_continues`：不安装测试 sender，
先发布版本 N，再用 `SCAN_START_PROBE` 屏障阻塞后续扫描。在线程中调用
`ensure_fresh(Duration::ZERO, Duration::from_millis(50))`。断言调用在 250ms 内返回，结果
`degraded == true`，且扫描仍在飞；释放屏障后等待版本大于 N 且 `IN_FLIGHT == false`。这同时
证明 no-sender 路径将扫描放到后台，并在调用超时后让它继续完成。

- [x] **步骤 2：新增已有慢扫描 deadline 测试**

新增 `ensure_fresh_uses_caller_deadline_for_existing_scan`：发布版本 N，安装有效测试 sender，
将 `IN_FLIGHT` 置真；在线程中调用 `ensure_fresh(Duration::ZERO, Duration::from_millis(50))`。
断言 250ms 内得到 degraded 结果、版本仍为 N、`IN_FLIGHT` 仍为真；之后清除测试占位并 join。
不得等内部 5 秒 `SCAN_WAIT_LIMIT`。

- [x] **步骤 3：运行两个回归确认失败**

运行：

```powershell
cd src-tauri
cargo test --lib adapter::scan_bus::tests::ensure_fresh_without_sender_times_out_while_fallback_scan_continues
cargo test --lib adapter::scan_bus::tests::ensure_fresh_uses_caller_deadline_for_existing_scan
```

预期：无 sender fallback 屏障测试在旧实现下超过 250ms 并失败；已有慢扫描用例作为 deadline
和不清 `IN_FLIGHT` 的兼容性基线，旧实现该分支可能通过，不能因此删除该回归。

---

### Task 4：统一动作读取 deadline 并增加后台 fallback

**文件：**
- 修改：`src-tauri/src/adapter/scan_bus.rs`

- [x] **步骤 1：增加 fallback 扫描单飞闩**

增加 `FALLBACK_SCAN_LAUNCHED: AtomicBool` 与命名线程启动函数。线程用 RAII guard 在正常返回、
扫描 panic、线程创建失败时清闩；仅当 sender 缺失或发送失败、且当前没有 `IN_FLIGHT` 扫描时
启动。后台闭包调用既有 `scan_once(ScanReason::Action)`，扫描准入继续由 `try_start_scan()`
和 `IN_FLIGHT` 单飞负责，不创建第二条并行全量扫描路径。发送 Action 成功时不启动 fallback；
发现已有在飞扫描时不重复排队。

- [x] **步骤 2：把 deadline 提到 `ensure_fresh()` 入口**

新鲜快照仍立即返回。否则在安排补扫前计算 `deadline = Instant::now() + wait`；按 sender 状态
发送一次 Action 或启动 fallback，然后用这个 deadline 轮询快照。每轮睡眠时长取
`min(20ms, deadline - now)`。超时后 `note_timeout()` 并返回 `degraded_snapshot()`，不得清除
`IN_FLIGHT` 或同步等待 `scan_once()`。

- [x] **步骤 3：运行动作 deadline 回归**

运行：

```powershell
cd src-tauri
cargo test --lib adapter::scan_bus::tests::ensure_fresh_without_sender_times_out_while_fallback_scan_continues
cargo test --lib adapter::scan_bus::tests::ensure_fresh_uses_caller_deadline_for_existing_scan
```

预期：2 passed；两条路径都在 250ms 内返回 degraded，后台扫描占位直到显式释放，随后能完成并
发布新版本。

---

### Task 5：调整远控启动顺序与调用注释

**文件：**
- 修改：`src-tauri/src/lib.rs`
- 修改：`src-tauri/src/commands/session.rs`
- 修改：`src-tauri/src/remote/api.rs`（冷启动读取说明）
- 修改：`src-tauri/src/remote/mod.rs`（显示快照读取说明）

- [x] **步骤 1：先启动 Scan Bus**

在 `.setup()` 中 `APP_HANDLE.set(...)` 之后、`remote::restore_on_launch()` 之前调用
`adapter::scan_bus::start(app.handle().clone())`。删除原来 usage 初始化之后的重复调用；wakeup
仍保持在 Scan Bus 之后启动。这样首个远控 listener 启动时已有单飞首扫在运行。

- [x] **步骤 2：更新冷启动读取注释**

更新 `commands/session.rs::get_all_sessions` 的注释，说明它读取已发布快照；没有首次成功快照时
等待 ColdStart 成功发布，依然通过 `spawn_blocking`，不把初始化 pending 转成成功空列表。
保持 `SessionsResponse` 字段及 Tauri 命令签名不变。

- [x] **步骤 3：核对 setup 顺序**

运行：`rg -n -C 2 'scan_bus::start|remote::restore_on_launch|wakeup::spawn_runtime' src-tauri/src/lib.rs`

预期：`scan_bus::start` 行序先于 `remote::restore_on_launch`，后于 APP_HANDLE 落位，且先于
`wakeup::spawn_runtime`。

同时新增并运行 `lib::scan_bus_start_order_tests::scan_bus_starts_after_app_handle_and_before_remote_listener_restore`，
防止后续调整 setup 顺序时重新打开首读窗口。

---

### Task 6：F1/F3 定向验证与记录

**文件：**
- 检查：`src-tauri/src/adapter/scan_bus.rs`
- 检查：`src-tauri/src/lib.rs`
- 更新：`docs/engineering/plans/2026-11-01-f1-f3-initial-read-and-action-deadline-plan.md`

- [x] **步骤 1：格式和定向测试**

运行：`cd src-tauri && cargo fmt -- --check` 与 `cargo test --lib adapter::`。

预期：格式检查通过，adapter 测试全部通过。

- [x] **步骤 2：Rust lint**

运行：`cd src-tauri && cargo clippy --lib -- -D warnings`

预期：退出码 0，无 warning。

- [x] **步骤 3：差异审查与状态记录**

运行：`git diff --check`、`git diff --stat`、`git status --short`。确认没有改动
`docs/MASTER-PLAN.md`，`docs/engineering/evidence/dsh-native-rpc/` 保持原样；标注单测与真实
release 冷启动验收的证据级别。

## 验收记录（2026-11-01）

| 命令 | 结果 |
| --- | --- |
| `cargo test --lib adapter::` | 166 passed，0 failed |
| `cargo test --lib scan_bus_starts_after_app_handle_and_before_remote_listener_restore` | 1 passed，0 failed |
| `cargo fmt -- --check` | 通过 |
| `cargo clippy --lib -- -D warnings` | 通过，0 warnings |
| `git diff --check` | 通过 |

TDD 红测证据：旧首读在 5.01s 后提前返回 degraded 空列表；旧 no-sender `ensure_fresh()` 被
`SCAN_START_PROBE` 屏障挡住时，调用方没有在 250ms deadline 内返回。修复后首读等待成功快照，
no-sender fallback 在后台完成，动作读取按同一 deadline 返回 degraded。真实桌面/远控启动慢首扫
场景与 release 构建仍待活体验收；单测使用进程私有临时数据目录。

## 验收记录（2026-11-01）

| 命令 | 结果 |
| --- | --- |
| `cargo test --lib adapter::` | 166 passed，0 failed |
| `cargo test --lib scan_bus_starts_after_app_handle_and_before_remote_listener_restore` | 1 passed，0 failed |
| `cargo fmt -- --check` | 通过 |
| `cargo clippy --lib -- -D warnings` | 通过，0 warnings |
| `git diff --check` | 通过 |

TDD 红测证据：旧 `current_or_scan()` 在 5 秒单飞等待后提前返回 degraded 空列表；旧无 sender
`ensure_fresh()` 在扫描屏障处阻塞调用方超过 250ms。修复后两条回归通过。adapter 测试使用
进程私有临时数据目录。真实桌面启动、移动 REST 首屏和 release 构建慢首扫仍待活体验收。
