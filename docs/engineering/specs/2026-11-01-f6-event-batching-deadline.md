# F6 事件 500ms 合批与 5 秒期限

## 现状与差距

Scan Bus 通道目前只携带 `ScanReason`。worker 收到原因后先排空当前 burst，再按 `last_scan.elapsed()` 延迟 Event；因此没有独立的 500ms 窗口，跨 worker/tick 边界的近邻事件可能拆成两轮。延迟又从上一轮扫描完成时起算：事件若在慢扫描期间到达，扫描结束后仍可能再等满 5 秒，超过事件自己的期限。

本组为每次扫描请求携带单调到达时刻和事件 dirty 水位；近邻 Event 按首次到达时刻做固定 500ms 合批，窗口外的独立 Event 保持待扫；每个批次的 5 秒 deadline 从批次首个请求时起算，已排队时间全部计入。Action 仍可立即中断节流；扫描仍只允许单飞。

## 方案比较

1. **保留 `ScanReason`，另设全局“最近事件时间”原子量**：并发发送会覆盖首事件时刻，扫描中的新事件也难以和当前扫描区分，拒绝。
2. **内部消息携带原因、单调到达时间和 dirty 水位**：worker 能按发送时间分批、保留独立后续请求，并判断某次扫描实际覆盖到哪个事件水位。推荐此方案；对外 `request_scan(ScanReason)` 与 IPC 保持不变。
3. **扫描期间另起补扫线程**：会破坏 Scan Bus 单飞，不采用。

## 时序与状态

- 内部 `ScanRequest` 包含 `reason`、单调 `arrived_at: Instant`、事件序号 `event_watermark`。
- `request_scan(Event)` 在发送时记录到达时间，并递增全局单调 dirty 水位；不是 worker 后来取到通道的时间。非 Event 原因不递增它，但携带创建时已存在的水位，以保留 Action/Periodic 优先级。
- 固定合批窗从首个 Event 的 `arrived_at` 起算 500ms。窗口内（`arrived_at - first_arrived < 500ms`）的 Event 合为当前批；窗口外请求进入独立后续批，不被当前批清掉。跨接收 tick 的近邻 Event 使用原始到达时刻判定。
- 批次可扫描的最早时刻为 `max(first_arrived + 500ms, last_scan_completed + event_min_gap)`；最迟时刻为 `first_arrived + 5s`。实际等待终点是前述最早时刻与 5 秒 deadline 中较早者；deadline 已过则立即扫。慢扫描期间排队的时间已计入，扫描完成后不得重置完整 5 秒等待。
- Event 等待期间收到 Action/ColdStart/Periodic，保持现有非 Event 优先级并立即离开 Event 等待。该非 Event 扫描可以覆盖其请求携带水位之前已到达的 Event；扫描启动之后的新事件水位保留待扫。
- Event 批只确认本批 500ms 窗口内的事件水位；窗口外的独立批保留，即使它在当前 Event 扫描开始前已排队，也仍有自己的后续扫描机会。Action/ColdStart/Periodic 若在其后启动，可用该非 Event 请求携带的最新水位覆盖当时已到达的事件。
- 每次扫描在启动时捕获本轮 Event watermark；只有健康发布后的快照才确认该水位。扫描期间到达的请求水位更大，必须保留并触发后续扫描。degraded/超时快照不确认事件水位；worker 在当前扫描结束后继续处理未确认水位，仍经原单飞与 generation 门闩，不并行开第二个扫描器。
- 其他扫描原因、档位节奏、托盘更新和 stats reason 语义保持不变；burst 含 Action 时本轮 stats 仍记 Action。

## 非目标

- 不改变 `request_scan` 的公开签名、IPC、事件文件 TTL 或 1 秒唤醒轮询。
- 不引入第二个扫描线程、等待期间裸 `sleep` 或墙上时钟期限。
- 不把 500ms 合批窗与现有最大 5 秒 Event 最小间隔混成一个起算点。

## 验收

1. 固定单调时钟下，相距 499ms 且跨 worker 接收 tick 的 Event 只形成一个批次；500ms 窗口外事件形成独立待扫批次。
2. 慢扫描期间到达的 Event 保留原到达时刻和 dirty 水位；若扫描完成时其 5 秒 deadline 已过，下一轮立即执行，不再追加完整 5 秒。
3. 扫描进行期间到达的 Event 水位大于本轮 captured watermark，当前扫描完成后仍触发后续 Event 扫描；不丢事件、不并发扫描。
4. Action 在合批或间隔等待期间到达时立即中断并执行，reason 记 Action；Action 扫描之后到达的 Event 仍保留。
5. 所有期限使用 `Instant`，所有入口继续经过同一 worker 单飞。
