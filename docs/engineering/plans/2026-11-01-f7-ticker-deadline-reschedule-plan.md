# F7 用量 ticker 截止时间重算实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox syntax for tracking.

**Goal:** 让用量兜底采集按最近真实采集的单调完成时刻调度，并能被真实采集或设置修改及时唤醒。

**Architecture:** `collect::Flight` 保存最近真实扫描完成 `Instant`，服务层最小间隔和 ticker deadline 共用该锚点。ticker 用 revision + Condvar 等到最近 deadline；采集真实完成或用量设置成功保存时增加 revision，ticker 重新读取设置和锚点。所有采集仍通过原 `collect(false)` 单飞路径。

**Tech Stack:** Rust、`std::time::Instant`、`Mutex`、`Condvar`、现有用量采集服务与 Tauri 命令。

---

## 任务 1：为单调采集完成锚点写回归

**文件：**

- 修改 `src-tauri/src/services/usage/collect.rs`
- 测试 `src-tauri/src/services/usage/collect.rs` 内已有单测模块

- [ ] **Step 1：增加可注入单调时钟的测试内核调用**

新增一个私有 `collect_with_clock(force, now_ms, clock, runner)`，其中 `clock: &dyn Fn() -> Instant`；公共 `collect_with` 调用它并传 `&Instant::now`。先保持原有生产逻辑不变，让测试可注入起始与完成时间。

- [ ] **Step 2：增加 3→12→15 分钟失败测试**

用虚拟单调时钟设置 10 分钟间隔：首轮 runner 将虚拟时钟从 0 推到 3 分钟后结束；t=12 分钟 `collect(false)` 应复用首轮结果；t=15 分钟应启动第二次 runner。另在两次调用间改变 `now_ms` 到更早/更晚值，确认墙钟值不影响门槛。

- [ ] **Step 3：运行定向测试确认失败**

运行 `cargo test services::usage::collect::tests::ticker_anchor --lib`。预期：锚点仍缺少单调完成时刻，测试失败。

- [ ] **Step 4：保存真实扫描单调完成时刻**

将 `Flight.last_at_ms` 改为 `last_completed_at: Option<Instant>`。等待现有飞行结束后取 `clock()` 判定；runner 返回后再取 `clock()` 并写回完成时刻。cache reuse 不写回。给 ticker 增加只读 `last_real_scan_completed_at()` getter。

- [ ] **Step 5：重跑采集门闩用例**

运行 `cargo test services::usage::collect::tests --lib`。预期：已有单飞、最小间隔、缓存复用和异常恢复用例通过；新增单调时间用例通过。

## 任务 2：提取可唤醒 ticker deadline 调度器

**文件：**

- 修改 `src-tauri/src/services/usage/ticker.rs`

- [ ] **Step 1：增加 deadline 纯函数和 revision 信号类型**

保留 `step_ms`，新增 `due_at(anchor, interval_min) = anchor + Duration::from_millis(step_ms(interval_min.clamp(1, 1440)))`。增加 `SchedulerSignal { revision: Mutex<u64>, changed: Condvar }`，提供 `revision()`、`notify()`、`wait_for_change(revision, timeout)`。测试截止时刻、缩短设置后的重算时刻、等待前通知不丢失。

- [ ] **Step 2：先加 panic 重试判定测试**

增加纯函数 `next_wake_at(due, retry_after)`；有未取消的 retry 点时必须使用 retry 点，即使原 due 已经过期；没有 retry 点时才使用正常 due。验证 panic 产生 `now + 500ms` 重试点；外部设置通知可唤醒并重算，但不能绕过 panic 退避。若新的真实采集推进了锚点，scheduler 清除旧 retry 点。

- [ ] **Step 3：重写常驻线程等待**

线程启动时保存 `started_at`。每圈读取 revision、设置和 `collect::last_real_scan_completed_at()`；锚点缺失时用 `started_at`。无待重试时等待 `due_at`；panic 后在重试点到达前等待 `retry_after`，即使原 deadline 已过期也不得绕过 500ms 退避。新真实采集推进锚点时清除旧重试点；被通知或超时后重算；deadline 到期仅调用 `collect(false)`。等待期间不持有 revision 锁，所有等待均可被通知打断。

## 任务 3：接入真实采集与设置变更通知

**文件：**

- 修改 `src-tauri/src/services/usage/collect.rs`
- 修改 `src-tauri/src/services/usage/settings.rs` 或 `src-tauri/src/commands/usage.rs`

- [ ] **Step 1：真采完成后通知**

`collect_with_clock` 在写回 `Flight` 完成锚点和 `LAST_RESULT`、释放所有锁之后通知 ticker。cache reuse 早退前不通知。复用现有调用栈确保启动、手动、可见、ticker 真采共用该通知。

- [ ] **Step 2：成功保存间隔设置后通知**

仅在 `settings::save` 写后回读成功后调用 `ticker::notify_scheduler()`；失败保存不改变调度器。测试通过 source contract 或已有设置保存用例确认顺序。

- [ ] **Step 3：增补单飞与复用测试**

验证同一截止点并发调用仍只进入一个 runner；缓存复用不改 `last_real_scan_completed_at`，设置变更信号使等待退出并重算。

## 任务 4：最终验证与提交

**文件：**`src-tauri/src/services/usage/{collect.rs,ticker.rs,settings.rs}` 及对应工程文档。

- [ ] **Step 1：运行 Rust 定向测试**

运行 `cargo test services::usage::ticker --lib` 与 `cargo test services::usage::collect::tests --lib`。预期两组全部通过。

- [ ] **Step 2：运行格式和 lint 检查**

运行 `cargo fmt -- --check` 与 `cargo clippy --lib -- -D warnings`。预期均成功。

- [ ] **Step 3：提交本组**

运行 `git diff --check` 后，仅暂存本组实现与工程文档，提交 `fix(usage): reschedule ticker from real collection time`。

## 规格覆盖核对

- 最近真实采集时间、3→12→15 分钟时序：任务 1。
- 单调最小间隔及不移动缓存锚点：任务 1、3。
- 当前间隔动态重算与设置变更唤醒：任务 2、3。
- 启动/手动/可见/后台真实扫描唤醒：任务 3。
- panic 退避恢复与单飞：任务 2、3。
- debug 默认开关和调用服务路径不变：任务 2。
