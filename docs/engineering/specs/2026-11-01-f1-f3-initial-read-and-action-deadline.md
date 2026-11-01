# F1/F3 冷启动首读与动作等待期限设计

## 背景

`scan_bus::current_or_scan()` 在没有快照时直接调用 `scan_once()` 并只返回其 `data`。
若另一个冷启动扫描已在运行超过 `SCAN_WAIT_LIMIT`，`scan_once()` 会返回 degraded 空快照；
`current_or_scan()` 把它转成普通 `SessionsResponse`，调用方无法区分“扫描尚未完成”和“成功扫描，
结果确实为空”。桌面命令和远程 GET 都使用该入口。

`ensure_fresh()` 的超时 deadline 目前只覆盖有扫描 sender 的分支。没有 sender 时直接同步调用
`scan_once()`，既不受调用方的 `wait` 限制，也无法在调用方超时后让扫描继续后台完成。远控启动
顺序还在 Scan Bus 启动前恢复 listener，冷启动请求可能落入该窗口。

## 目标与验收

1. 冷启动首读只有在成功快照发布后才能返回；成功扫描的空 `SessionsResponse` 仍正常返回。
   首次扫描未完成或 degraded 空占位不能被伪装成权威空列表。
2. Scan Bus 在远控 listener 恢复前启动；首个扫描可以在后台继续，读取方等待其成功发布。
3. `ensure_fresh()` 对 sender 存在、sender 缺失、已有慢扫描等分支都使用入口处创建的同一
   deadline。超时时返回 `degraded=true` 的快照，并保留正在运行的扫描及 `IN_FLIGHT`。
4. 没有 sender 且没有在飞扫描时，动作补扫在后台启动并去重；调用方达到 deadline 后返回，后台
   扫描仍可完成并发布。
5. 保持 `SessionsResponse`、桌面 IPC 与移动 REST 响应结构不变；初始化等待与动作等待分开，
   不放宽动作超时来解决首屏问题。

## 方案

### 初始化读取

用 `snapshot() == None` 表示尚未有成功扫描，用 `Some(snapshot)`（即使 `data.sessions` 为空）
表示成功发布。degraded 超时产物继续只作调用方临时回执，不写入 `LAST`。

`current_or_scan()` 先读已发布快照；若没有快照且没有扫描在飞，尝试同步占用唯一单飞执行一次
ColdStart；若已有扫描在飞，等待它完成。任何一次调用若拿到 degraded 回执或扫描 panic，都继续
等待下一次成功发布，不把它转换成普通空响应。桌面命令与远程 GET 已在 `spawn_blocking` 中调用
此入口，因此等待不会占用 UI 线程或 Tokio async worker。

### 动作读取

`ensure_fresh()` 在检查快照之后、安排补扫之前创建一个 deadline；从安排补扫到轮询结束都使用
同一 deadline。快照已经新鲜时立即返回。

- 有 sender 且没有扫描在飞：向常驻 worker 发送一次 Action。
- 有 sender 且扫描已在飞：不重复排队，等待这次扫描。
- 无 sender 且扫描已在飞：不启动第二次扫描，只等待这次扫描。
- 无 sender、无扫描在飞，或现有 sender 已断开：启动去重的 fallback 扫描线程。fallback 仍调用
  `scan_once()`，因此继续受 `IN_FLIGHT` 单飞保护；启动闩由 RAII guard 在成功、panic 或线程
  创建失败路径释放。

每轮等待都先检查同一个 deadline，并最多睡到该 deadline。超时记入 degraded 诊断窗口并返回
`degraded_snapshot()`；不清 `IN_FLIGHT`，不取消后台扫描。

### 启动顺序

在 `src-tauri/src/lib.rs` 的 Tauri setup 中，`APP_HANDLE` 落位后先调用
`adapter::scan_bus::start()`，再调用 `remote::restore_on_launch()`。wakeup runtime 继续在
Scan Bus 启动后启动。这样远控 listener 对外提供请求时，常驻扫描器和首扫已经开始；首扫未完成
时由 `current_or_scan()` 等待成功快照。

## 回归测试

1. 首读测试：先由屏障阻塞一条已在飞的首扫，超过旧的 5 秒单飞等待上限后，`current_or_scan()`
   仍未返回；释放屏障并发布成功空快照后，首读返回该空结果。
2. 无 sender fallback 测试：用 `SCAN_START_PROBE` 屏障阻塞后台 fallback 扫描；短 `wait` 的
   `ensure_fresh()` 在约定期限内返回 degraded，而扫描仍在飞；释放屏障后扫描发布更高版本并
   清理单飞位。
3. 已有慢扫描测试：扫描已在飞且 sender 存在时，动作读取在自己的短 deadline 内返回 degraded，
   不等待内部 `SCAN_WAIT_LIMIT`，也不清在飞位。
4. 已有快照测试：年龄在 `max_age` 内时不排队、不启动 fallback。
5. 启动顺序测试：`scan_bus::start()` 的调用位置先于 `remote::restore_on_launch()`。

屏障和 `SCAN_START_PROBE` 固定并发顺序；不依靠真实远控连接或桌面窗口。lib 单测的数据目录由
`database::connection::app_data_home()` 指向进程私有临时目录。

## 修改边界

- `src-tauri/src/adapter/scan_bus.rs`：初始化读取等待、动作 deadline、fallback 启动闩、回归测试。
- `src-tauri/src/lib.rs`：调整 Scan Bus 与远控恢复的先后顺序。
- `src-tauri/src/commands/session.rs`、`src-tauri/src/remote/api.rs`、
  `src-tauri/src/remote/mod.rs`：更新读取语义注释，IPC 与 REST 响应结构不变。
- `src-tauri/src/lib.rs`：增加启动顺序回归，确保 AppHandle 落位后 Scan Bus 先于远控恢复和 wakeup。

不修改 `docs/MASTER-PLAN.md`、`SessionsResponse`、远程 API schema、F2 cadence 策略或 F4/F5
前端仲裁逻辑。
