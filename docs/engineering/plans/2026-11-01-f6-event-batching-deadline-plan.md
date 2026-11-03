# F6 事件 500ms 合批与 5 秒期限实施计划

关联规格：[F6 事件 500ms 合批与 5 秒期限](../specs/2026-11-01-f6-event-batching-deadline.md)

## 变更边界

- 最小行为差距：通道消息没有到达时刻或事件水位；worker 只有排空即时 burst，没有跨接收边界的 500ms 合批；Event 最小间隔从扫描完成后重新起算。
- 行为所在位置：全部状态位于 `src-tauri/src/adapter/scan_bus.rs` 的唤醒消息、worker 收取/合批、Event 延迟与扫描记账。
- 只修改 Scan Bus 内部消息与调度；不改 `request_scan(ScanReason)` 的调用签名，不改 `adapter::wakeup` 轮询、不加线程、不改单飞扫描内核。

## 任务 1：纯调度失败测试

**文件：**`src-tauri/src/adapter/scan_bus.rs`。

1. 新增单调时刻可注入的纯批次规划测试：
   - `event_arrivals_499ms_apart_across_ticks_share_batch`：首 Event 在 `t=10.000s`、下一次 worker 接收时刻为 `t=10.499s`，两个请求归入同一窗口，窗口截止仍为 `t=10.500s`。
   - `event_after_500ms_window_remains_independent`：事件在 `t=10.501s` 到达，必须保留为后续批次，不能推进首批的 dirty watermark。
   - `event_deadline_counts_time_queued_during_slow_scan`：Event 在上一轮开始后第 1 秒到达，上一轮第 8 秒完成；下一批 due time 为立即，而不是完成时再等 5 秒。
   - `event_arriving_during_scan_has_newer_watermark`：扫描开始 captured watermark 后发出的事件水位更大，完成记账不得清掉该水位。
2. 先运行这些纯测试，确认现有 `event_wait_plan(elapsed_since_completion, interval)` 无法表达 500ms 窗口、请求绝对 deadline 与 dirty watermark。

## 任务 2：内部请求消息与单调水位

**文件：**`src-tauri/src/adapter/scan_bus.rs`。

1. 定义内部 `ScanRequest { reason: ScanReason, arrived_at: Instant, event_watermark: u64 }` 与单调 `EVENT_DIRTY_WATERMARK`；`request_scan(Event)` 在发送时递增水位并记录 `Instant::now()`；Action/Periodic/ColdStart 不递增 dirty 水位，但携带请求创建时已存在的 watermark。
2. 将 `WORKER_TX`、`TEST_TX`、`scan_tx`、`rebuild_channel`、`recv_reason`、`WorkerRound` 的内部通道类型改为 `ScanRequest`；所有直接 `Sender<ScanReason>` 用例改用同一请求构造器。
3. 新增纯函数按首 Event 时间与 `EVENT_BATCH_WINDOW=500ms` 切批，返回当前批 watermark 与剩余独立请求；`burst_scan_reason` 继续保证 Action 优先。
4. 运行任务 1 的纯测试。

## 任务 3：等待、期限与单飞接线

**文件：**`src-tauri/src/adapter/scan_bus.rs`。

1. 替换事件等待计划：用批次首到达时刻推导 500ms 合批结束、5 秒绝对 deadline 和 `last_scan_completed + event_min_gap`；等待期间使用 `recv_timeout`，把窗口外 Event 原样放回后续请求队列，不重置当前 deadline。
2. Action/ColdStart 等非 Event 原因立即中断等待，保持当前 stats reason 合并规则；在同一 worker 中启动后续批次，绝不调用第二份扫描。
3. 扫描启动时记下本轮 captured event watermark；Action/Periodic/ColdStart 携带的既有水位和 Event 当前批 watermark 均由该扫描覆盖。仅健康发布后确认 captured 水位。扫描中抵达的新 watermark 保留到当前扫描返回后执行；degraded 扫描不能确认该水位。worker 每圈在等待档位周期前先处理未确认水位，避免消费了通道消息却把 dirty 状态丢掉。
4. 增加 worker 级回归：慢扫描屏障期间注入 Event，释放屏障后确认 deadline 到期则立即启动唯一后续扫描；Action 在 500ms 等待中到达则立即扫描并记 Action。

## 最终检查

```powershell
Push-Location src-tauri
cargo test adapter::scan_bus::tests --lib
cargo fmt -- --check
cargo clippy --lib -- -D warnings
Pop-Location
pnpm check
git diff --check
```

测试只使用虚拟 `Instant`/同步屏障，不依赖长时间真实等待；若需要线程级测试，最多使用 1 秒的失败超时保护，不把线程协调时间计入逻辑期限。
