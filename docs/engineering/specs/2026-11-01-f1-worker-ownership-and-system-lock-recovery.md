# F1 worker 执行权与共享进程状态恢复设计

## 背景

当前 Scan Bus 使用工作线程代次 `WORKER_GEN` 限制心跳写入，并由 supervisor 在心跳超时且
`IN_FLIGHT == false` 时重建唤醒通道。通道断开只能让旧 worker 在下一次 `recv` 时退出；若它
已经离开等待路径并阻塞在单圈工作中，恢复后仍可能继续执行扫描、托盘更新或 MODE 发布。
仅检查代次后再执行操作也不够，因为检查与接管之间仍有竞态。

进程发现状态 `SHARED_SYSTEM` 由 `Mutex<Option<System>>` 保护。进程刷新和 adapter 进程发现
都在锁内执行；若锁内 panic，Rust 会 poison 该锁。现有多个访问点使用 `lock().unwrap()`，
因此之后每轮扫描会再次 panic，Scan Bus 的外层恢复无法让进程发现路径恢复。

## 目标与验收

1. `SHARED_SYSTEM` 锁内 panic 后，下一次访问丢弃可能处于不一致状态的 `System`、清除 poison，
   并可继续扫描与发布比之前更高版本的快照。
2. worker 接管有一个明确的线性化点。旧 worker 一旦失去当前代执行权，不能再启动扫描、
   消费待处理事件后继续工作、更新托盘或发布 MODE。
3. 扫描入口与执行权交接使用同一同步机制；supervisor 在交接时重新检查 `IN_FLIGHT`。
4. 保留 `IN_FLIGHT` 单飞语义；任何恢复路径都不得清空或伪造该标志。
5. 回归测试以屏障固定旧 worker 阻塞、事件排队、新代首扫完成、旧代恢复的顺序，并验证旧代
   不产生扫描或状态更新；另验证 poison 恢复后下一次访问与快照发布成功。

## 方案

### 共享 `System` 锁恢复

在 `src-tauri/src/adapter/mod.rs` 增加统一的锁获取辅助函数。正常时返回现有 guard；遇到
`PoisonError` 时取回 guard，将 `Option<System>` 设为 `None`，调用 `clear_poison()`，记录一次
恢复日志后返回 guard。这样下一次进程刷新会通过既有 `get_or_insert_with(System::new)` 创建
干净实例。

`get_all_sessions_inner()` 的进程刷新、宿主存活过滤，以及 `sync_unread` 的宿主检查都改用
该入口，避免某个残留的 `lock().unwrap()` 再次把恢复路径变成永久 panic。恢复逻辑不尝试
修补旧 `System` 的内部状态。

### worker 执行权交接

在 `src-tauri/src/adapter/scan_bus.rs` 增加一把执行权门闩。代次切换、扫描准入、心跳和
MODE/托盘副作用都经过该门闩；实际扫描不持有门闩，而由 `IN_FLIGHT` 表示扫描仍在运行。
这样交接与准入有共同的线性化点，同时 supervisor 仍可在长扫描期间检查 hang 状态。

- worker 在等待返回、完成事件合并、事件延迟结束后检查当前代次；失效代次退出，不再处理
  后续工作。
- `scan_once_with_limit()` 在门闩内检查可选的 worker 代次，并通过单飞标志准入扫描，同时
  记录扫描起点与 epoch；随后释放门闩执行扫描。所有 worker、动作和冷启动扫描入口共用此
  准入。扫描及发布期间 `IN_FLIGHT` 保持为真，supervisor 复核到它后不得接管。
- 托盘刷新、MODE 写入及其 UI 事件发布都在代次授权下执行，不能用一次脱离同步的原子读取
  代替门闩。
- supervisor 的 Respawn 路径取得同一门闩后再次读取 `IN_FLIGHT`，并确认心跳仍已过期。
  若扫描已经开始或 worker 刚恢复并续期心跳，则放弃本次交接；由后续巡检再次判断。若没有
  扫描且心跳仍旧，通道替换与新代次激活作为一次交接完成，旧 worker 随后所有授权操作都会
  失败。扫描 RAII 清理也在门闩内清除 `IN_FLIGHT`，避免它与 supervisor 的复核或下一轮扫描
  准入交错。
- 心跳也受当前代次保护，旧代无法在交接边界覆盖新代的心跳。

门闩只在扫描准入/结束、代次交接和状态副作用期间持有，不围住实际扫描、通道等待、事件延迟
等待或其他可能长时间阻塞的路径。托盘和 MODE 写入需要与接管串行化，以保证交接完成后旧代
不再产生这些副作用。

### 测试设计

1. 在隔离的 `Mutex<Option<System>>` 上持锁 panic；经生产恢复辅助函数再次取得锁，断言旧
   `System` 已丢弃、锁不再 poisoned；随后通过测试扫描源发布快照，断言版本严格递增。
2. 用真实 worker 循环和屏障控制单圈行为：旧 worker 进入屏障前接收原因，并让旧通道积压
   `Event`；supervisor 执行交接，新 worker 的 `ColdStart` 扫描完成；释放旧 worker 后，断言
   旧代扫描计数与状态发布计数均未增加，旧 worker 退出，当前 worker 仍为单个。
3. 测试扫描启动与接管竞争：两者只能有一个先取得门闩；若扫描先获准，supervisor 复核到
   `IN_FLIGHT` 后不得启动新代；若交接先完成，旧代不得进入扫描。另覆盖 supervisor 读到旧
   心跳后旧 worker 恢复续期，接管复核必须取消这次重启。

测试不得调用真实桌面应用或修改真实用户数据。时间顺序由 channel/barrier 控制，不依赖长
`sleep` 作为竞态证明。

## 预期修改边界

- `src-tauri/src/adapter/mod.rs`：共享 `System` 锁恢复辅助函数和 poison 回归；替换全部共享
  锁访问点。
- `src-tauri/src/adapter/scan_bus.rs`：worker 执行权门闩、代次授权、接管复核与并发回归。

不修改 `docs/MASTER-PLAN.md`。本批不处理 F2–F10，不调整扫描档位、事件覆盖策略、动作等待
期限或移动端 API，也不通过强制清理 `IN_FLIGHT` 恢复扫描。

## 风险与约束

- 门闩与 `WORKER_TX` 的锁顺序必须固定；`request_scan` 只短暂访问 `WORKER_TX`，不得反向持有
  通道锁再获取执行权门闩。
- 测试复位必须在同一门闩下清理 generation 与通道状态，避免旧测试 worker 在 reset 后获得
  新代权限。
- 如果真实调用链显示存在不经 worker 的扫描入口，仍保留其现有 `IN_FLIGHT` 单飞路径；不能
  为了 generation 检查把它们错误地当成 worker 扫描。
