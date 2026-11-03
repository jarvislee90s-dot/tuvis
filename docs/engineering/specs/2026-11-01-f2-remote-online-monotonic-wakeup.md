# F2 远控在线单调 TTL 与 REST 上升沿唤醒设计

## 背景

Scan Bus 的在线信号由 `remote::is_online_now()` 提供。当前 API 心跳存储 `Utc::now()` 的毫秒值，
再以墙上时间相减判断 60 秒窗口；系统时间前拨可能提前过期，后拨可能延长在线状态。

`remote/gate.rs` 在设备认证成功后调用 `note_api_request()`，但它只更新时间戳，没有通知 Scan Bus。
当 cadence 处于 Away 且没有 SSE 连接时，第一条已认证 REST 请求只改变了信号源，worker 仍可能
睡到剩余 30 秒周期结束才读取该信号并升到 Idle。

## 目标与验收

1. 60 秒 API 在线窗口完全基于 `std::time::Instant`，系统墙上时钟前拨/后拨不改变 TTL。
2. 已认证 API 请求只在“远控离线 → 在线”的上升沿请求一次 `ScanReason::Action`；连续请求
   更新 TTL 但合并为同一个在线区间，不放大扫描次数。
3. SSE 在线时继续按 SSE 信号维持在线，不因 API 心跳增加多余扫描。
4. 首个无 SSE 的 REST 请求完成鉴权后立即唤醒 Scan Bus；扫描完成时 `remote_online` 已为真，
   cadence 从 Away 升到 Idle。
5. 未认证请求、未知路径和 `/pair/pin` 现有豁免不触发 API 在线边沿；设备 roster 使用的持久
   `last_seen_at` 仍按墙上时间计算，不与进程内 API TTL 混用。

## 方案

将进程内 API 心跳改为 `Mutex<Option<Instant>>`。`online_from` 只接收 SSE 状态、最后一次 API
`Instant` 和当前 `Instant`，用 `saturating_duration_since` 与严格小于 60 秒的窗口判定。
系统时间不进入该状态或计算路径。

增加可注入单调时刻和上升沿回调的私有内核。内核在锁内读取旧状态、判断是否离线、并把记录更新
到 `max(previous, now)`，然后释放锁；只有旧状态离线时才执行回调。并发首请求因此只有一个调用者
触发唤醒，较早取时刻、较晚取得锁的请求也不会把最新时间倒写。

生产 `note_api_request()` 读取 SSE 原子计数并采样 `Instant::now()`，调用该内核；上升沿回调调用
现有 `scan_bus::request_scan(Action)`。心跳仍只在 auth gate 放行后记账，不移动鉴权边界。

## 回归测试

1. API TTL 边界测试：无心跳离线；最近 59,999ms 在线；恰满 60,000ms 离线；SSE 在场恒在线。
   测试仅传 `Instant`，在线判定和生产采样入口不读取墙上时间。
2. 无 SSE 的上升沿测试：首次已认证 API 请求通过 `request_scan(Action)` 发出一次 Action；
   窗口内后续请求不再发 Action，但延长 TTL；离线超过 60 秒后的下一次请求再发一次。
3. 并发请求合并测试：多个并发首请求只产生一次离线→在线唤醒。
4. 既有 `Cadence` 测试保证 `remote_online=true` 时目标档位不低于 Idle；上升沿 Action 测试保证
   worker 不必等 Away 周期才读取新信号。

## 修改边界

- `src-tauri/src/remote/mod.rs`：API 心跳存储、单调 TTL、上升沿内核与单测。
- `src-tauri/src/adapter/scan_bus.rs`：必要时只增加 Action 唤醒回归缝；保留 cadence 规则。
- `src-tauri/src/remote/gate.rs`：鉴权顺序不变；确认唯一心跳调用仍位于认证通过分支。

不修改 `remote::is_online()` 的持久设备 roster 口径，不改变 60 秒 TTL、REST/SSE schema、鉴权
逻辑、Scan Bus 三档时长或扫描单飞语义。
