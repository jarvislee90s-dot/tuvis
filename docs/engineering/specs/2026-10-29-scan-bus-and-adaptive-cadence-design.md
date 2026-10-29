# 扫描总线与自适应节奏（Scan Bus & Adaptive Cadence）设计

- 日期：2026-10-29
- 分支：`design/scan-bus-and-adaptive-cadence`（基于 `origin/main` @ `5a53b9a7`，v0.6.3）
- 状态：待评审
- 关联问题：空闲 CPU 11% 单核、全量扫描 56–61 次/分；用量看板"切页才采"导致数据长时间滞后

## 0. 一句话

把"每个界面各自触发全量扫描"改成**一个后端扫描器 + 一份带版本的快照 + 按 S~D 时效档自适应的节奏**；用量/TPS 采集从"看板生命周期"里摘出来，改为"可见即采 + 后端低频兜底"。

## 1. 背景与实测现状

### 1.1 现在的扫描入口（代码事实）

| 发起方 | 位置 | 频率 |
| --- | --- | --- |
| 后端 `SessionWatcher`（远控服务 bind 成功后启动） | `src-tauri/src/remote/watcher.rs` | 2s |
| 桌面主窗 | `src/lib/query/queries/sessions.ts`（`POLL_INTERVAL = 3000`） | 3s |
| 桌宠窗口 | `src/components/pet/FoxbellPet.tsx`（同一查询 hook） | 3s |
| 手机看板 | `src-tauri/src/remote/api.rs` 的 `/m/api/v1/sessions` 直调 `adapter::get_all_sessions` | 3s（`src/mobile/Board.tsx`） |

`get_all_sessions` 内部（`src-tauri/src/adapter/mod.rs`）：

1. `sysinfo` 刷新**全部进程**，并为每个进程读取 `cmd / cwd / exe / cpu`；
2. 逐个 adapter 匹配进程（`src-tauri/src/monitor/process.rs`）；
3. 有进程的工具才解析会话（有 `(mtime,size)` 缓存，未变更零读取）；
4. 单飞护栏只防"扫描进行中再来请求"（返回旧快照），**不降低扫描次数**。

此外每次桌面 `get_all_sessions` 命令调用后都会更新托盘（`src-tauri/src/commands/session.rs`）。

### 1.2 实机测量（2026-10-29，安装版 0.6.3）

| 指标 | 数值 | 方法 |
| --- | --- | --- |
| 空闲 CPU | 11.2% 单核（45s 内 5.05 CPU-s） | `TotalProcessorTime` 采样 |
| 全量扫描频次 | 56–61 次/分（≈1.05s 一次） | `mam.log` 中 `adapter] OpenCode:` 计数 |
| 单轮 CPU（推算） | ≈0.1s | 11.2% ÷ 58 轮/分 |
| 机器进程数 | 475 | `Get-Process` |
| 热扫文件面 | dsh 426 文件/258MB→26ms；claude 180 个→11ms；codex 60 个→1ms；kimi 7 个 | `os.walk + stat` 计时 |
| 冷扫（应用启动那一次） | 12.5s（今天）/ 24.0s（昨天），0 条新记录 | 日志「启动采集完成」 |
| 一次 tick 的 CPU | 低于测量噪声（4s 窗口增量 0.27s，基线 0.3–0.66s） | 4s 粒度采样 |

### 1.3 用量采集的触发现状（独立问题）

`src/lib/query/queries/usage.ts` 的 `useUsageCollect(active)`：挂载采一次 + 仅当模型用量页激活且窗口可见的 10 分钟 ticker；后端只有启动后延迟 3s 的一次采集（`services/usage/collect.rs::spawn_initial_collection`），**没有周期调度**。`collectIntervalMin`（默认 10）只是最小间隔节流，不是后台频率。

实测后果：11:08 切走用量页 → 15:37 才补采，期间 OpenCode 两个会话（11:21、13:57）全部滞后 4.5 小时。

### 1.4 问题归纳

1. **同一件事被做了 3–4 遍**：四路消费者各自触发全量扫描；
2. **进程扫描是最大单项**：475 个进程 × 每轮读 cmd/cwd/exe/cpu；
3. **采集时机与 UI 生命周期耦合**：用量数据在"没看那个页面"时完全不采；
4. **频率不可自适应**：全部空闲和正在跑任务用同一个 3s/2s 节奏。

## 2. 术语

| 术语 | 定义 |
| --- | --- |
| **扫描（scan）** | 一次 `get_all_sessions_inner`：进程发现 + 各工具会话解析，产出 `SessionsResponse` |
| **快照（Snapshot）** | 一次扫描的不可变结果 + 版本号 + 扫描时刻，供所有消费方读取 |
| **节奏档（Mode）** | 由活动会话、远控在线、窗口可见三个信号算出的扫描间隔档位 |
| **活跃会话** | `processing / thinking / compacting / waiting` 之一（等待用户输入也算） |
| **远控在线** | SSE 注册表存在连接，或最近 60s 内有 `/m/api/*` 请求 |
| **事件唤醒** | 廉价信号（hook 事件目录、用量 facts、TPS 数据文件 mtime）变化时立即补扫一次 |
| **S~D 档** | 按"用户能感知到的时效"划分的数据分级（见 §5） |

## 3. 目标与非目标

### 3.1 目标

1. 空闲 CPU 从 11.2% 单核降到 **≤3%（挂机）/ ≤6%（远控在线但无活动会话）**；
2. 全量扫描频次降到 **≤30 次/分（有远控）/ ≤6 次/分（挂机）**；
3. 远程时效不回退：活动档会话发现 ≤2s，动作回执 p95 不劣于基线；
4. 用量数据在"看得见时"即时、在"看不见时"最多滞后 10 分钟，且不再依赖用户切页。

### 3.2 非目标

- 不改远程注入/屏读协议与"卡面状态以屏读为准"的契约；
- 不重构各工具解析器内部（只优化其调用时机与连接复用）；
- 不引入 Windows 进程创建/退出事件（WMI/ETW）；
- 不改 TPS 调度器节奏（可见 60s / 后台 600s 已合理）；
- 不做前端纯推送改造（保留命令读取契约，见 §6.3）。

## 4. 验收门禁与测量方法

| 指标 | 基线（2026-10-29） | 门禁 |
| --- | --- | --- |
| 空闲 CPU（单核百分比） | 11.2% | 挂机 ≤3%，远控在线空闲 ≤6% |
| 每分钟全量扫描次数 | 56–61 | 有远控 ≤30，挂机 ≤6 |
| 活动档会话发现延迟 | 3s | ≤2s |
| 远程动作回执延迟 | **待测（实施前先记录）** | p95 不回退 |
| 用量可见时采集延迟 | 0–10 分钟（事件驱动后 0） | ≤1s（打开/悬停/切回页签立即采） |
| 用量不可见时滞后 | 无上限（可滞后数小时） | ≤10 分钟 |

测量脚本（实施与验收共用）：

1. `TotalProcessorTime` 前后采样 45s，记录百分比；
2. `mam.log` 每分钟 `adapter] OpenCode:` 行数（= 扫描次数）；
3. 手机端：开始答题 → 记录动作到回执的耗时（≥20 次取 p95）；
4. 用量：切到其他页 30 分钟再切回，检查"数据截止时间"是否在 1s 内刷新。

## 5. S~D 时效分级（本设计的核心契约）

| 档 | 数据面 | 用户感知 | 主触发 | 兜底 |
| --- | --- | --- | --- | --- |
| **S** | 注入回执、问答勾选态、Review 确认 | 动作后立即 | 动作后屏读快照（现状不变） | — |
| **A** | 看板卡片状态、桌宠状态/气泡、系统通知、手机看板 | 秒级 | Scanner 活动档 2s + 事件唤醒 | 10s |
| **B** | 托盘图标、会话详情页、发送队列 | 十秒级 | 快照读取 + 托盘 30s 刷新 | 60s |
| **C** | 模型用量账本、输出速度页 | 看的时候要新 | 可见/悬停/切回页签立即采 | 用量后端 10 分钟；TPS 后台 600s |
| **D** | 定价快照、版本更新 | 分钟/天 | 5 分钟循环 / 天级检查（现状） | — |

判定规则：

- S 档**不进扫描总线**：回执与屏读是动作驱动，扫描节奏再快也替代不了；
- A 档是 Scanner 的高频档，只有"活跃会话或远控在线"才允许 2s；
- B 档允许读旧快照，不需要触发扫描；
- C 档的"可见"由前端显式上报（`useUsageCollect` 的 active / 浮窗挂载 / 设置页打开），**不能**绑在 Scanner 节奏上；
- D 档维持现状。

## 6. 架构

### 6.1 组件与边界

```
事件源：~/.tuvis/events 目录 mtime / 用量 facts / TPS 数据文件 mtime
   │ request_scan(reason)（合并节流 ≥500ms）
   ▼
┌──────────────────────────────────────────────┐
│ Scanner（单例，常驻后台）                      │
│  ├ Cadence 状态机（纯函数，可单测）            │
│  ├ 单飞执行 get_all_sessions_inner（沿用护栏） │
│  ├ 分级进程扫描（第二刀）                      │
│  └ 发布 Snapshot { version, scanned_at, data } │
└───────┬──────────────────┬───────────────────┘
        │ Arc 快照（读侧克隆）│ 跃迁 broadcast（沿用 watcher_tx）
   ┌────┴─────┬─────────────┼──────────────┬─────────────┐
 命令读取    托盘(30s)    SessionWatcher   手机 SSE/GET   通知
（主窗/桌宠）            （diff→Transition）
```

| 单元 | 职责 | 依赖 | 可独立测试 |
| --- | --- | --- | --- |
| `Scanner` | 唯一扫描者；节奏循环、单飞、失败退避、快照发布 | `Cadence`、`get_all_sessions_inner`、事件通道 | 是（注入时钟与扫描函数） |
| `Cadence` | 输入三信号 → 输出间隔（2s/10s/30s） | 无 | 是（纯状态机） |
| `Snapshot` | 不可变结果 + `version` + `scanned_at` + 健康状态 | 无 | 是 |
| `get_all_sessions` 命令 | 读快照；快照龄 > 2×档位间隔时同步补扫一次 | `Scanner` | 是 |
| `SessionWatcher` | 订阅快照，diff 出 `TransitionEvent` 并广播；**不再自己扫描** | `Scanner` | 是 |
| 事件唤醒器 | 1s 轮询 3 个路径的 mtime，变化即 `request_scan` | 文件系统 | 是 |
| 用量兜底 ticker | 每 10 分钟 `collect()`（复用单飞+最小间隔） | usage 服务 | 是 |

### 6.2 接口（建议签名）

```rust
// 扫描器
pub struct Scanner;
impl Scanner {
    pub fn start(app: AppHandle);              // 幂等；进程内唯一
    pub fn snapshot() -> Arc<Snapshot>;         // 最近一次结果（冷启动可能为空）
    pub fn request_scan(reason: ScanReason);    // 事件唤醒，合并节流
    pub fn mode() -> Mode;                      // 当前档位
    pub fn subscribe_mode() -> broadcast::Receiver<Mode>; // 前端/托盘订阅
}

pub struct Snapshot {
    pub version: u64,          // 单调递增，供消费方判断是否变化
    pub scanned_at: Instant,
    pub data: SessionsResponse,
    pub health: ScanHealth,    // ok / degraded(reason)
}

pub enum Mode { Active, Idle, Away }   // 2s / 10s / 30s
```

`get_all_sessions` 命令语义：

1. 返回 `Scanner::snapshot()`；
2. 若 `snapshot.age > 2 × mode.interval`（例如 Scanner 卡死/休眠唤醒），同步执行一次扫描后返回；
3. 冷启动无快照时同步扫描（保留旧行为，首个窗口不拿空数据）。

### 6.3 消费方改造

| 消费方 | 现在 | 改后 |
| --- | --- | --- |
| 桌面主窗 | 3s 调 `get_all_sessions`（触发扫描） | 按 `mode` 间隔调命令（**只读快照**，2s/10s/30s） |
| 桌宠窗口 | 同上 | 同上（与主窗共用同一份快照） |
| 手机看板 | 3s 调 `/sessions`（触发扫描） | 3s 读快照（保持 3s；扫描由 Scanner 决定），SSE 继续推跃迁 |
| 托盘 | 每次命令后更新 | 快照 `version` 变化时更新，**最小间隔 30s**；状态跃迁立即更新 |
| `SessionWatcher` | 2s 自扫 + diff | 订阅快照 version 变化 + diff（节奏自动跟随 Scanner） |
| SSE `/events` 首帧 | 现场扫一次 | 读快照；快照为空才补扫 |

前端模式订阅：Scanner 通过 Tauri event 推送 `mode` 变化（低频，仅档位切换时），前端据此调整 `refetchInterval`。

## 7. 自适应节奏（Cadence）

### 7.1 输入信号

| 信号 | 来源 | 更新方式 |
| --- | --- | --- |
| 活跃会话 | 最近一次快照里 `processing/thinking/compacting/waiting` 是否存在 | 每次扫描后更新 |
| 远控在线 | SSE 注册表连接数 > 0，或 `/m/api/*` 最近 60s 有请求 | 连接/请求时打点，Cadence 计算时读取 |
| 窗口可见 | 主窗或桌宠窗口上报 `visibilityState` 心跳（30s 一次，TTL 90s） | Tauri command + 内存标记 |

### 7.2 状态机

```
       任何时刻
          │
   ┌──────┴──────┐ 活跃会话 > 0 或 远控在线
   │             ▼
   │        ┌─────────┐  条件消失且持续 ≥ 1 个 Idle 周期
   │        │ Active  │ ───────────────────────┐
   │        │  2s     │                        │
   │        └─────────┘                        ▼
   │                                     ┌─────────┐
   └────────────────────────────────────>│  Idle   │ 10s
                                         └────┬────┘
                            无远控 且 窗口不可见 ≥ 1 个周期
                                              ▼
                                         ┌─────────┐
                                         │  Away   │ 30s
                                         └─────────┘
```

规则：

- 升档立即生效（下一轮用新间隔）；降档需稳定一个周期（防抖）；
- 休眠唤醒/时钟跳变：快照龄超过 `2×间隔` 视为过期，命令侧补扫（见 §6.2）；
- 档位切换推送 Tauri event + 写一条 INFO 日志（状态跃迁才打，不逐轮打）。

### 7.3 参数

| 档位 | 间隔 | 说明 |
| --- | --- | --- |
| Active | 2s | 与现 watcher 一致，远程时效不回退 |
| Idle | 10s | 无活跃会话但远控在线/窗口可见 |
| Away | 30s | 无远控、窗口不可见、无活跃会话 |

首版固定常量，不进设置页（避免新增配置面与迁移）；上线后用 §4 的门禁数据决定是否调整。

## 8. 采集时机（用量 / TPS）

### 8.1 模型用量

- **可见即采**：模型用量页从隐藏变可见（`active` false→true）、桌宠迷你条挂载、设置页打开、用户点「立即采集」→ 立刻 `usage_collect(force=false)`；
- **后端兜底**：新增常驻 ticker，每 `collectIntervalMin`（默认 10 分钟）调用一次 `collect`，复用现有单飞 + 最小间隔；
- **移除**"仅页面激活期间才存在的 ticker"这一语义（前端只保留"激活即采"）。

### 8.2 TPS

维持现状（后端调度器可见 60s / 后台 600s；前端快照 5s、点 60s、心跳 30s）。本设计只要求：TPS 的 UI 读取不触发会话扫描。

### 8.3 采集域纪律

用量 ticker 会真实写账本（`~/.tuvis/tuvis.db`）。开发期必须遵守 `AGENTS.md` 的采集域纪律：改 `services/usage/**` 前停掉 `pnpm tauri:dev` 或把 `TUVIS_HOME` 指向影子目录；游标字段语义变更按迁移处理。

## 9. 事件唤醒（首版轻量方案）

- 监控 3 个路径的 mtime，1s 轮询（每次只有 3 次 `stat`，成本可忽略）：
  1. `~/.tuvis/events/`（Claude hook 事件目录，30s TTL，新增/删除会改变目录 mtime）；
  2. `~/.tuvis/tps/opencode/facts.jsonl`（TPS 观察插件产出）；
  3. 当前活跃会话文件的 mtime（从最近快照中取，最多 5 个）。
- 变化 → `request_scan(reason)`；合并节流 ≥500ms（同一批事件只补扫一次）。
- 不用 `notify` 依赖：首版避免新增依赖与跨平台差异；若后续需要进程级事件再评估。

## 10. 失败与降级

| 场景 | 行为 |
| --- | --- |
| 扫描 panic | 沿用 `ScanFlightGuard` 清位；计数连续失败 ≥2 → 退避到 30s，成功一次恢复 |
| 冷启动无快照 | 命令同步扫一次（旧行为），首个窗口不拿空数据 |
| 事件源不可用 | 纯轮询兜底，不影响正确性 |
| 远控断连 | 下一次 Cadence 计算降档；不立即降（防抖一个周期） |
| 窗口心跳丢失 | TTL 90s 后视为不可见 → 只影响降档，不影响正确性 |
| 用量 ticker 失败 | 复用 `collect` 的错误结果发布；UI 显示"采集失败"，下一轮重试 |
| 快照读取失败/过期 | 命令侧补扫；补扫失败返回旧快照 + `health=degraded` |

## 11. 迁移、兼容与落地顺序

不改 IPC 契约：`get_all_sessions` 的入参/出参形状不变，调用方无需同步升级。

### 第一刀（本设计主体）

1. 抽出 `Scanner` + `Cadence` + `Snapshot`，把 `remote::SessionWatcher` 的 2s 循环并入 Scanner；
2. `get_all_sessions` 改为读快照 + 过期补扫；
3. 主窗/桌宠 `refetchInterval` 跟随 mode；手机看板保持 3s 读快照；
4. 托盘更新改为 version 变化 + 30s 限频；
5. 用量：可见即采 + 后端 10 分钟兜底 ticker。

### 第二刀（独立验收）

1. 进程扫描分级：先 PID + 名字，命中工具白名单才读 `cmd/cwd/exe`；`cpu` 字段降频；
2. 事件唤醒器（§9）；
3. `opencode` 解析器复用连接 / 合并查询；dsh 目录 mtime 预过滤；日志降噪（INFO → 仅在状态变化时）。

## 12. 测试

| 层 | 用例 |
| --- | --- |
| `Cadence` | 档位切换/防抖/时钟跳变/心跳过期；纯函数单测 |
| `Scanner` | 单飞（并发请求只扫一次）、版本单调递增、panic 后自愈、失败退避 |
| 命令层 | 快照未过期不触发扫描（**缺席断言**，照搬用量域源码锁做法）；过期触发一次补扫 |
| Watcher | 只基于快照 diff，不自扫（源码锁：`watcher.rs` 不得再出现 `get_all_sessions`） |
| 托盘 | 30s 限频 + 跃迁立即更新 |
| 用量 | 可见即采（active 边沿）、后端 ticker 周期、采集失败不影响下一轮 |
| 实机 | §4 四项测量脚本，前后对比 |

## 13. 风险与未决点

| # | 风险 | 影响 | 缓解 |
| --- | --- | --- | --- |
| R1 | `get_all_sessions` 从"每次现扫"变"读快照"，若有调用方隐式依赖实时性 | 数据陈旧 | 调用点审计（settings/remote/commands 三处），过期补扫兜底 |
| R2 | 托盘 30s 限频让用户觉得图标反应慢 | 体验 | 跃迁立即更新，周期只做兜底 |
| R3 | 窗口可见心跳依赖前端，崩溃/卡顿时心跳丢失 | 误降档 | TTL 90s + 主窗与桌宠任一上报即为可见 |
| R4 | 事件唤醒 1s mtime 轮询在机械硬盘/网络盘上抖动 | 误唤醒 | 只轮询 3 个本机路径；唤醒仍受 500ms 合并节流 |
| R5 | 进程分级后提权进程读不到 cmd/cwd | 误判形态 | 保留现有 `exe > name > argv[0]` 回退；候选集按名字命中，读不到时仍按名字判 |
| R6 | 用量 ticker 在开发期写真实账本 | 账本污染 | 遵守 `AGENTS.md` 采集域纪律；ticker 仅在 release 语义下常驻（dev 可关） |
| R7 | 休眠唤醒后快照龄过大、首屏卡顿 | 首屏 | 命令侧补扫放在 `spawn_blocking`；先返回旧快照再异步刷新（可选） |

**未决点（评审时需要拍板）**：

1. 手机看板是否也跟随 mode（3s → 空闲 10s）？本设计暂定保持 3s（读快照很便宜）。
2. `get_all_sessions` 过期补扫是"同步等结果"还是"先回旧快照、异步刷新"？本设计倾向同步（保持旧语义），代价是极端情况下首屏等待。
3. 托盘限频 30s 是否可接受，是否需要更短（如 10s）。

## 14. 代码参考位置

- 扫描编排与单飞：`src-tauri/src/adapter/mod.rs`（`SCAN_FLIGHT` / `get_all_sessions` / `get_all_sessions_inner`）
- 远控 watcher：`src-tauri/src/remote/watcher.rs`（`SCAN_INTERVAL = 2s`）
- 手机接口：`src-tauri/src/remote/api.rs`（`sessions` / `events`）
- 托盘更新：`src-tauri/src/commands/session.rs`、`src-tauri/src/plugins/system_tray.rs`
- 前端轮询：`src/lib/query/queries/sessions.ts`、`src/config/constants.ts`、`src/mobile/Board.tsx`
- 用量触发：`src/lib/query/queries/usage.ts`、`src-tauri/src/services/usage/collect.rs`
- TPS 节奏：`src-tauri/src/services/tps/scheduler.rs`
- 解析器：`src-tauri/src/monitor/opencode_parser.rs`、`src-tauri/src/monitor/process.rs`
