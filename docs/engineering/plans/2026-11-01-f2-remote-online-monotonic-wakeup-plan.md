# F2 远控在线单调 TTL 与 REST 上升沿唤醒实施计划

> **执行要求：** 按步骤先写失败回归再改实现；在当前功能分支内连续执行，不再请求分组批准。

**目标：** API 在线 TTL 不受系统墙钟跳变影响，远控离线后的首个认证 REST 请求立即唤醒 Scan Bus，窗口内后续请求不放大扫描。

**架构：** 用进程内 `Instant` 保存 API 最近访问时刻；用同一把 mutex 原子判断在线边沿并更新时间，锁外只在离线→在线时请求一次 Action。SSE 计数继续作为独立在线信号。

**技术栈：** Rust `std::time::Instant`、`Mutex<Option<Instant>>`、现有 Scan Bus Action sender 与 `cargo test --lib` / Clippy。

---

### Task 1：新增单调 TTL 与上升沿失败回归

**文件：**
- 修改：`src-tauri/src/remote/mod.rs`（测试模块）

- [x] **步骤 1：更新 TTL 边界测试**

将 `online_from_window_boundaries` 改为传入 `Option<Instant>` 与 `now: Instant`：SSE 在场恒在线；
无心跳离线；`last + 59_999ms` 在线；`last + 60_000ms` 离线。

- [x] **步骤 2：新增认证 REST 上升沿测试**

新增 `api_request_wakes_scan_bus_once_per_online_edge`：使用本地 `Mutex<Option<Instant>>`，并用
`scan_bus::test_guard()` 与 `set_test_tx_for_test()` 捕获 Action。`t0` 首次离线→在线必须送一个
Action；`t0+20s` 与 `t0+59_999ms` 续期不再送；`t0+120s`（相对上一请求已过 60s）再次离线→在线，
必须再送一个 Action。断言唤醒总数为 2。

- [x] **步骤 3：新增并发合并测试**

新增 `concurrent_api_requests_emit_one_rising_edge`：两个线程通过 `Barrier` 同时调用注入内核，
共享 `Mutex<Option<Instant>>` 与 `AtomicUsize` 回调计数；断言最终在线且回调只执行一次。

- [x] **步骤 4：固定生产接线和鉴权边界**

检查 `note_api_request()` 函数体中采样 `Instant::now()` 并在上升沿调用
`scan_bus::request_scan(Action)`；源码回归检查 `remote/gate.rs` 的调用仍位于 `if ok { ... }` 成功
分支，不移动认证门。

- [x] **步骤 5：运行测试确认失败**

运行：

```powershell
cd src-tauri
cargo test --lib remote::tests::online_from_window_boundaries
cargo test --lib remote::tests::api_request_wakes_scan_bus_once_per_online_edge
cargo test --lib remote::tests::concurrent_api_requests_emit_one_rising_edge
```

预期：旧实现因墙钟毫秒签名与缺少上升沿回调而无法编译或断言失败。

---

### Task 2：实现单调时间窗口与一次性 Action 唤醒

**文件：**
- 修改：`src-tauri/src/remote/mod.rs`

- [x] **步骤 1：将 API 最近时刻改为 `Instant`**

将 `LAST_API_AT_MS: AtomicI64` 改为 `Lazy<Mutex<Option<Instant>>>`，并定义 60 秒
`API_ONLINE_TTL`。`online_from(sse, last: Option<Instant>, now: Instant)` 在 SSE 不在线时使用
`now.saturating_duration_since(last) < API_ONLINE_TTL`；`None` 表示从未有已认证请求。

- [x] **步骤 2：实现并发安全的边沿内核**

实现 `note_api_request_at(last, sse, now, on_rising_edge)`：锁内把 `now` 钳到
`max(previous, now)`，判断此前是否在线并更新时刻；释放锁后只在离线→在线时执行一次回调。
这样并发请求不会重复触发，也不会让较早取到的 `Instant` 覆盖较新的值。

- [x] **步骤 3：接入生产心跳**

`note_api_request()` 读取 `server::any_connection()`、采样 `Instant::now()` 并调用内核；上升沿
回调使用 `scan_bus::request_scan(ScanReason::Action)`。`is_online_now()` 用相同 `Instant::now()`
读取最近时刻并调用 `online_from`。保留 `is_online(registry_hit,last_seen_at,now)` 的持久 roster
墙钟语义。

- [x] **步骤 4：运行上升沿与 TTL 回归**

运行：

```powershell
cd src-tauri
cargo test --lib remote::tests::online_from_window_boundaries
cargo test --lib remote::tests::api_request_wakes_scan_bus_once_per_online_edge
cargo test --lib remote::tests::concurrent_api_requests_emit_one_rising_edge
```

预期：3 passed；模拟 `Instant` 的 59,999/60,000ms 边界正确，连续与并发请求在每个在线区间只
发送一次 Action。

---

### Task 3：F2 定向验证与记录

**文件：**
- 检查：`src-tauri/src/remote/mod.rs`
- 检查：`src-tauri/src/remote/gate.rs`
- 检查：`src-tauri/src/adapter/scan_bus.rs`
- 更新：`docs/engineering/plans/2026-11-01-f2-remote-online-monotonic-wakeup-plan.md`

- [x] **步骤 1：验证 gate 认证边界与单调时钟源码**

运行新增源码回归，确认心跳调用只在 gate 成功分支，API TTL 路径只使用 `Instant`，不读取
`chrono::Utc::now()`。

- [x] **步骤 2：运行远控与扫描定向测试**

运行：`cd src-tauri && cargo test --lib remote::tests`、`cargo test --lib adapter::scan_bus::tests`。

预期：远控单测与 51 条 Scan Bus 单测全部通过。

- [x] **步骤 3：运行 lint 与差异检查**

运行：`cd src-tauri && cargo fmt -- --check`、`cargo clippy --lib -- -D warnings`；在仓库根运行
`git diff --check`、`git status --short`。确认 `docs/MASTER-PLAN.md` 未改，用户已有证据目录未改。

预期：格式、Clippy、差异检查通过；真实设备时间跳变和移动端在线升档仍作为后续活体验收记录，
不由纯函数测试代替。

## 验收记录（2026-11-01）

| 命令 | 结果 |
| --- | --- |
| `cargo test --lib remote::tests` | 68 passed，0 failed |
| `cargo test --lib remote::gate::tests` | 5 passed，0 failed |
| `cargo test --lib adapter::scan_bus::tests` | 51 passed，0 failed |
| `cargo fmt -- --check` | 通过 |
| `cargo clippy --lib -- -D warnings` | 通过，0 warnings |
| `git diff --check` | 通过 |

旧实现下的 TTL 签名/边沿回归首先编译失败；修复后 TTL 边界、单请求唤醒、并发请求合并、生产
gate 认证分支和 cadence 即时升档均通过。未在宿主系统上实际调整墙钟，也未做移动设备 REST
活体验收；这些仍列为 F2/F10 运行验收证据。
