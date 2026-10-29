# 扫描总线与自适应节奏（Scan Bus & Adaptive Cadence）设计

- 日期：2026-10-29
- 分支：`design/scan-bus-and-adaptive-cadence`（基于 `origin/main` @ `5a53b9a7`，v0.6.3）
- 状态：**v2（已按两路子代理审阅修订）**，待用户评审
- 关联问题：空闲 CPU 11% 单核、全量扫描 56–61 次/分；用量看板"切页才采"导致数据长时间滞后

## 0. 一句话

把"每个界面各自触发全量扫描"改成**一个后端扫描器 + 一份带版本的快照 + 按 S~D 时效档自适应的节奏**；用量/TPS 采集从"看板生命周期"里摘出来，改为"可见即采 + 后端低频兜底"；事件唤醒进第一刀，兜底轮询只负责正确性。

### v2 修订摘要（相对 v1）

1. **P0 修正**：手机看板并非 3s 轮询——实际是 SSE 跃迁 + 30s 对账，REST `/sessions` 只作降级/首连；设计不得把它写成 3s。
2. **P0 修正**：用量"可见即采"必须 `force=true`（否则被 10 分钟最小间隔吞掉），并加前端 60s 去抖。
3. **P0/P1**：`Active` 档只由"活跃会话"触发；"远控在线但无活动"落 `Idle` 10s（v1 的状态机让 Idle 不可达）。
4. **P1**：事件唤醒从第二刀提前到第一刀；`Away 30s` 必须配合事件唤醒，否则 hook 事件会在 TTL(30s) 内被错过而永久丢失。
5. **P1**：新增 `ensure_fresh()` 语义——所有动作路径（发送/打断/问答 GET/resume/会话归属查询）不得直接读可能过期的快照；SSE 首帧要求快照龄 ≤2s。
6. **P1**：扫描历史上承担了副作用（未读池、状态缓存、屏读过期、WorkBuddy 补偿），快照化后这些副作用必须留在"读取/命令"层，否则 mark_read/dismiss 后会回弹。
7. **技术选型修正**：进程扫描改两段式（实测 44–57ms → 22.7ms）；**不做** opencode 长连接复用（实测 open+TOP200 仅 1.07ms）、**不做** dsh 目录 mtime 预过滤（原地 append 不改目录 mtime）；事件唤醒需同时 stat 会话事件文件（原地写不改目录 mtime）。
8. **遗漏补入**：手机配对计数每次请求跑 `System::new_all()`（`remote/mod.rs`），是总线之外的第二台全量扫描器，必须并入或限频 ≥10s。

## 1. 背景与实测现状

### 1.1 现在的扫描入口（代码事实）

| 发起方 | 位置 | 频率/语义 |
| --- | --- | --- |
| 后端 `SessionWatcher`（远控服务 bind 成功后启动） | `src-tauri/src/remote/watcher.rs` | 2s 全量扫描 + diff 跃迁广播 |
| 桌面主窗 | `src/lib/query/queries/sessions.ts`（`POLL_INTERVAL = 3000`） | 3s；`refetchIntervalInBackground:false` ⇒ **焦点驱动**，窗口失焦/后台时并不轮询 |
| 桌宠窗口 | `src/components/pet/FoxbellPet.tsx`（同一查询 hook） | 3s（桌宠常显，实际近似常驻） |
| 手机看板 | `src/mobile/Board.tsx` + `src-tauri/src/remote/api.rs` | **SSE 跃迁流 + 30s 对账**；3s 轮询仅降级路径；首连 `GET /sessions` 会现场扫一次 |
| 手机配对计数 | `src-tauri/src/remote/mod.rs` 的 `pairing_counter` | **每次请求 `System::new_all()`**（完全在扫描总线之外） |

`get_all_sessions` 内部（`src-tauri/src/adapter/mod.rs`）：

1. `sysinfo` 刷新**全部进程**，并为每个进程读取 `cmd / cwd / exe / cpu`（实测 466 个进程时 **44–57ms/轮**，子代理实测）；
2. 逐个 adapter 匹配进程（`src-tauri/src/monitor/process.rs`）；
3. 有进程的工具才解析会话（有 `(mtime,size)` 缓存，未变更零读取）；
4. 单飞护栏在 `get_all_sessions` 外层，只防"扫描进行中再来请求"（返回旧快照），**不降低扫描次数**；
5. 扫描过程中还承担副作用：未读池维护、`session_status_cache`、屏读过期清理、WorkBuddy 补偿等。

此外每次桌面 `get_all_sessions` 命令调用后都会更新托盘（`src-tauri/src/commands/session.rs`），并读取一次 hook 事件目录（`read_hook_events`，30s TTL）。

### 1.2 实机测量（2026-10-29，安装版 0.6.3）

| 指标 | 数值 | 方法 |
| --- | --- | --- |
| 空闲 CPU | 11.2% 单核（45s 内 5.05 CPU-s） | `TotalProcessorTime` 采样 |
| 全量扫描频次 | 56–61 次/分（≈1.05s 一次） | `mam.log` 中 `adapter] OpenCode:` 计数 |
| 单轮 CPU（推算） | ≈0.1s | 11.2% ÷ 58 轮/分 |
| 进程刷新单项 | **44–57ms/轮**（466 进程） | 子代理 sysinfo 0.39 基准；仅名字层 10ms，白名单二级 12.5ms，两段式合计 22.7ms |
| 机器进程数 | 475 | `Get-Process` |
| 热扫文件面 | dsh 426 文件/258MB→26ms；claude 180 个→11ms；codex 60 个→1ms；kimi 7 个 | `os.walk + stat` 计时 |
| hook 事件目录 | 308 个陈旧文件，每轮全读 **9ms** | 子代理实测 |
| opencode 开库 | open + TOP200 仅 **1.07ms**（98MB 库） | 子代理 sqlite3 直测 |
| 冷扫（应用启动那一次） | 12.5s（今天）/ 24.0s（昨天），0 条新记录 | 日志「启动采集完成」 |
| 一次 tick 的 CPU | 低于测量噪声（4s 窗口增量 0.27s，基线 0.3–0.66s） | 4s 粒度采样 |

### 1.3 用量采集的触发现状（独立问题）

`src/lib/query/queries/usage.ts` 的 `useUsageCollect(active)`：挂载采一次 + 仅当模型用量页激活且窗口可见的 10 分钟 ticker；后端只有启动后延迟 3s 的一次采集（`services/usage/collect.rs::spawn_initial_collection`），**没有周期调度**。`collectIntervalMin`（默认 10）只是最小间隔节流，不是后台频率。

实测后果：11:08 切走用量页 → 15:37 才补采，期间 OpenCode 两个会话（11:21、13:57）全部滞后 4.5 小时。

### 1.4 问题归纳

1. **同一件事被做了 3–4 遍**：四路消费者各自触发全量扫描，另有一台配对计数器；
2. **进程扫描是最大单项**：44–57ms/轮，占单轮 0.1s 的一半；
3. **采集时机与 UI 生命周期耦合**：用量数据在"没看那个页面"时完全不采；
4. **频率不可自适应**：全部空闲和正在跑任务用同一个 3s/2s 节奏；
5. **扫描承担副作用**：快照化不能简单"只读"，否则未读/状态/清理行为会漂移。

## 2. 术语

| 术语 | 定义 |
| --- | --- |
| **扫描（scan）** | 一次 `get_all_sessions_inner`：进程发现 + 各工具会话解析 + 既有副作用，产出 `SessionsResponse` |
| **快照（Snapshot）** | 一次扫描的不可变结果 + 版本号 + 扫描时刻 + 健康状态，供所有消费方读取 |
| **节奏档（Mode）** | 由活跃会话、远控在线、"有人在看"三个信号算出的扫描间隔档位 |
| **活跃会话** | `processing / thinking / compacting / waiting` 之一（等待用户输入也算） |
| **远控在线** | SSE 注册表存在连接，或最近 60s 内有 `/m/api/*` 请求 |
| **有人在看** | 主窗可见**且聚焦**，或桌宠迷你条展开/悬停，或远控在线。桌宠常显本身**不算**（否则 Away 永远不可达） |
| **事件唤醒** | 廉价信号（hook 事件目录/事件文件 mtime、用量 facts、TPS 数据文件 mtime）变化时立即补扫一次 |
| **ensure_fresh** | 动作路径专用的"保证新"语义：要求快照龄 ≤ 阈值，否则在 Scanner 单飞内补扫并等待结果 |
| **S~D 档** | 按"用户能感知到的时效"划分的数据分级（见 §5） |

## 3. 目标与非目标

### 3.1 目标

1. 空闲 CPU 从 11.2% 单核降到 **≤3%（挂机）/ ≤6%（远控在线但无活动会话）**；
2. 全量扫描频次降到 **≤30 次/分（有活跃会话）/ ≤6 次/分（挂机）**；
3. 远程时效不回退：活动档会话发现 ≤2s，动作回执 p95 不劣于基线；
4. 用量数据在"看得见时"即时（≤1s）、在"看不见时"最多滞后 10 分钟，且不再依赖用户切页。

### 3.2 非目标

- 不改远程注入/屏读协议与"卡面状态以屏读为准"的契约；
- 不重构各工具解析器内部（只优化调用时机）；
- 不引入 Windows 进程创建/退出事件（WMI/ETW）；
- 不改 TPS 调度器节奏（可见 60s / 后台 600s 已合理）；
- 不做前端纯推送改造（保留命令读取契约，见 §6.3）；
- **不做**：opencode 长连接复用、dsh 目录 mtime 预过滤、IPC 增量帧/压缩（均经实测否决，见 §13.3）。

## 4. 验收门禁与测量方法

| 指标 | 基线（2026-10-29） | 门禁 |
| --- | --- | --- |
| 空闲 CPU（单核百分比） | 11.2% | 挂机 ≤3%，远控在线空闲 ≤6% |
| 每分钟全量扫描次数 | 56–61 | 活跃 ≤30，挂机 ≤6 |
| 活动档会话发现延迟 | 3s | ≤2s |
| 远程动作回执延迟 | **实施前先记录基线** | p95 不回退 |
| 用量可见时采集延迟 | 0–10 分钟（受最小间隔吞并） | ≤1s（打开/悬停/切回页签立即采） |
| 用量不可见时滞后 | 无上限 | ≤10 分钟（ticker 步长 = 间隔 + 缓冲） |

测量脚本（实施与验收共用）：

1. `TotalProcessorTime` **5 分钟 × 3 次取中位**（45s 噪声大，子代理指出）；
2. `mam.log` 每分钟 `adapter] OpenCode:` 行数（= 扫描次数）；
3. 补充指标：扫描线程 CPU、`IO Read Operations`、`Context Switches`（验证不是"把成本挪到别处"）；
4. 手机端：开始答题 → 记录动作到回执耗时（≥20 次取 p95）；
5. 用量：切到其他页 30 分钟再切回，检查"数据截止时间"是否在 1s 内刷新。

## 5. S~D 时效分级（本设计的核心契约）

| 档 | 数据面 | 用户感知 | 主触发 | 兜底 |
| --- | --- | --- | --- | --- |
| **S** | 注入回执、问答勾选态、Review 确认 | 动作后立即 | 动作后屏读快照（现状不变） | — |
| **A** | 看板卡片状态、桌宠状态/气泡、系统通知、手机看板 | 秒级 | Scanner `Active` 2s（**仅活跃会话触发**）+ 事件唤醒 | 10s |
| **B** | 托盘图标、会话详情页、发送队列、**远控在线但无活跃会话** | 十秒级 | 快照读取 + 托盘 30s 刷新 | 60s |
| **C** | 模型用量账本、输出速度页 | 看的时候要新 | 可见/悬停/切回页签立即采（`force=true`） | 用量后端 10 分钟；TPS 后台 600s |
| **D** | 定价快照、版本更新 | 分钟/天 | 5 分钟循环 / 天级检查（现状） | — |

判定规则：

- S 档**不进扫描总线**：回执与屏读是动作驱动，扫描节奏替代不了；
- A 档只服务"正在发生的事"：没有活跃会话就没有秒级需求；
- 远控在线但空闲 → B 档 10s；手机发起动作时由 `ensure_fresh` 单独保证新鲜（§6.2）；
- C 档的"可见"由前端显式上报，**不能**绑在 Scanner 节奏上；
- D 档维持现状。

## 6. 架构

### 6.1 组件与边界

```
事件唤醒器（1s）：events 目录 + 事件文件 mtime / 用量 facts / TPS 数据文件
   │ request_scan(reason)（合并节流 ≥500ms）
   ▼
┌────────────────────────────────────────────────────┐
│ Scanner（单例，常驻后台，带看门狗自愈）               │
│  ├ Cadence 状态机（纯函数，可单测）                  │
│  ├ 单飞执行 get_all_sessions_inner（护栏下沉到此）    │
│  ├ 分级进程扫描（两段式，第二刀）                     │
│  ├ 副作用编排（未读池/状态缓存/清理，保持原语义）      │
│  └ 发布 Snapshot { version, scanned_at, data, health }│
└───────┬────────────────────┬────────────────────────┘
        │ Arc 快照（读侧克隆） │ 跃迁 broadcast（沿用 watcher_tx）
   ┌────┴─────┬──────────────┼───────────────┬──────────────┐
 命令读取    托盘(30s)      SessionWatcher   手机 SSE/GET   通知
（主窗/桌宠）              （diff→Transition）
```

| 单元 | 职责 | 依赖 | 可独立测试 |
| --- | --- | --- | --- |
| `Scanner` | 唯一扫描者；节奏循环、单飞、失败退避、副作用编排、快照发布 | `Cadence`、`get_all_sessions_inner`、事件通道 | 是（注入时钟与扫描函数） |
| `Cadence` | 输入三信号 → 输出间隔（2s/10s/30s） | 无 | 是（纯状态机） |
| `Snapshot` | 不可变结果 + `version` + `scanned_at` + `health` | 无 | 是 |
| `get_all_sessions` 命令 | 读快照；过期时走 `ensure_fresh` | `Scanner` | 是 |
| `ensure_fresh(threshold)` | 快照龄超阈值 → 在 Scanner 单飞内补扫并等待（带超时） | `Scanner` | 是 |
| `SessionWatcher` | 订阅快照，diff 出 `TransitionEvent` 并广播；**不再自己扫描** | `Scanner` | 是 |
| 事件唤醒器 | 1s 轮询目录 + 事件文件 mtime，变化即 `request_scan` | 文件系统 | 是 |
| 用量兜底 ticker | 每 `collectIntervalMin + 缓冲` 调 `collect(false)`；成功后广播失效 | usage 服务 | 是 |
| 配对计数器 | 复用 Scanner 进程快照，或自限频 ≥10s | `Scanner` | 是 |

### 6.2 接口（建议签名）

```rust
pub struct Scanner;
impl Scanner {
    pub fn start(app: AppHandle);                 // 幂等；进程内唯一；看门狗重启
    pub fn snapshot() -> Arc<Snapshot>;           // 最近一次结果（冷启动可能为空）
    pub fn ensure_fresh(max_age: Duration, wait: Duration) -> Arc<Snapshot>; // 动作路径专用
    pub fn request_scan(reason: ScanReason);      // 事件唤醒，合并节流
    pub fn mode() -> Mode;
    pub fn subscribe_mode() -> broadcast::Receiver<Mode>;
}

pub struct Snapshot {
    pub version: u64,          // 单调递增
    pub scanned_at: Instant,
    pub data: SessionsResponse, // 追加 mode/version 字段，前端零额外命令即可拿到
    pub health: ScanHealth,     // ok / degraded(reason)
}

pub enum Mode { Active, Idle, Away }   // 2s / 10s / 30s
```

`get_all_sessions` 命令语义：

1. 返回 `Scanner::snapshot()`；
2. 若 `snapshot.age > 2 × mode.interval`（Scanner 卡死/休眠唤醒），`ensure_fresh` 同步补扫一次后返回；
3. 冷启动无快照时同步扫描（保留旧行为）。

**单飞所有权**：单飞从 `get_all_sessions` 外层**下沉到 Scanner**；`get_all_sessions`、`ensure_fresh`、事件唤醒、用量 ticker 之外的一切补扫都必须走 `Scanner::scan_now()`，禁止直调 `get_all_sessions` 产生第二份快照。

### 6.3 消费方改造

| 消费方 | 现在 | 改后 |
| --- | --- | --- |
| 桌面主窗 | 3s 调命令（触发扫描），焦点驱动 | 按 `mode` 间隔调命令（**只读快照**）；窗口重新可见时立即 `refetch` 一次 |
| 桌宠窗口 | 同上 | 同上（与主窗共用同一份快照） |
| 手机看板 | SSE + 30s 对账；REST 仅降级/首连 | **保持现状**（SSE + 30s 对账）；REST 读快照，不再触发扫描 |
| 手机配对计数 | 每次请求 `System::new_all()` | 复用 Scanner 进程快照，或限频 ≥10s |
| 托盘 | 每次命令后更新 | 快照 `version` 变化时更新，**最小间隔 30s**；状态跃迁立即更新 |
| `SessionWatcher` | 2s 自扫 + diff | 订阅快照 version 变化 + diff（节奏跟随 Scanner） |
| SSE `/events` 首帧 | 现场扫一次 | 读快照；**快照龄 >2s 时先 `ensure_fresh`**，并带 `version` 供对账 |
| 动作路径（发送/打断/问答 GET/resume/归属查询） | 直调 `get_all_sessions` 现扫 | `ensure_fresh(≤2s, wait≤3s)`；失败时返回旧快照 + `degraded` |
| 会改变展示状态的命令（mark_read / dismiss / 归档） | 依赖下次扫描重算 | 命令内触发 `ensure_fresh` 或本地增量修正，**不得等下一个节奏周期** |

**副作用归属（评审 P1）**：未读池维护、`session_status_cache`、屏读过期清理、WorkBuddy 补偿等原属于扫描流程；快照化后：

- 仍随扫描执行（它们依赖扫描结果，留在 Scanner 内）；
- 但"用户动作导致的即时变化"（已读、隐藏、归档）由命令层在 `ensure_fresh` 之后重算，保证卡片不回弹。

前端模式订阅：`mode` 与 `version` 随 `SessionsResponse` 下发（子代理建议，避免 Tauri event 丢失/首帧问题）；Tauri event 仅用于"档位切换后立即重取"的提示，属于加速通道而非唯一来源。

## 7. 自适应节奏（Cadence）

### 7.1 输入信号

| 信号 | 来源 | 更新方式 |
| --- | --- | --- |
| 活跃会话 | 最近一次快照里 `processing/thinking/compacting/waiting` 是否存在 | 每次扫描后更新 |
| 远控在线 | SSE 注册表连接数 > 0，或 `/m/api/*` 最近 60s 有请求 | 连接/请求时打点 |
| 有人在看 | 主窗**可见且聚焦**；或桌宠迷你条展开/悬停；或远控在线 | 前端心跳 30s 一次，TTL 90s |

> 注意：`refetchIntervalInBackground:false` 的真实语义是"窗口失焦即停轮询"，因此"窗口可见"与"有人在看"必须分开：主窗失焦但桌宠常显时，**不能**因此判定为有人在看（否则 Away 永不可达）；同时主窗重新聚焦要立即 `refetch` 一次，避免回桌首屏等满 30s。

### 7.2 状态机

```
       活跃会话 > 0
   ┌──────────────────────┐
   │                      ▼
   │               ┌─────────────┐
   │               │ Active  2s  │
   │               └──────┬──────┘
   │   条件消失且稳定 ≥ 1 个周期 │
   │                      ▼
   │               ┌─────────────┐  远控在线 或 有人在看
   └──────────────►│ Idle  10s   │◄──────────────
                   └──────┬──────┘
        无远控 且 无人看 且稳定 ≥ 1 个周期
                          ▼
                   ┌─────────────┐
                   │ Away  30s   │
                   └─────────────┘
```

规则：

- **只有活跃会话能触发 Active**；远控在线但无活跃会话 = Idle 10s（手机动作由 `ensure_fresh` 保证）；
- 升档立即生效；降档需稳定一个周期（防抖）；
- 休眠唤醒/时钟跳变：快照龄超过 `2×间隔` 视为过期，命令侧补扫；
- 档位切换推送 Tauri event（加速通道）+ 写一条 INFO（状态跃迁才打）。

### 7.3 参数

| 档位 | 间隔 | 说明 |
| --- | --- | --- |
| Active | 2s | 与现 watcher 一致；**仅活跃会话** |
| Idle | 10s | 远控在线或有人在看，但无活跃会话 |
| Away | 30s | 无远控、无人看、无活跃会话；**必须配合事件唤醒**，否则会错过 30s TTL 的 hook 事件 |

首版固定常量，不进设置页；上线后用 §4 门禁数据决定是否调整。

## 8. 采集时机（用量 / TPS）

### 8.1 模型用量

- **可见即采（force=true）**：模型用量页隐藏→可见（`active` false→true）、桌宠迷你条挂载、设置页打开、点「立即采集」→ 立即 `usage_collect(force=true)`；前端 60s 去抖，避免来回切页连打；
  - 原因：`force=false` 会被 10 分钟最小间隔直接吞掉（v1 的 P0 错误）。
- **后端兜底**：常驻 `std::thread`（对齐 `tps/collector.rs` 的既有做法），每 `collectIntervalMin + 2 分钟缓冲` 调一次 `collect(false)`；步长必须大于最小间隔，否则隔轮空跑、实际滞后可达 20 分钟；
- **采集成功广播**：ticker 采集成功后广播一次失效事件（Tauri event），开着的看板立即重查；
- **移除**"仅页面激活期间才存在的 10 分钟 ticker"（前端只保留"激活即采"）；
- **dev 保护**：debug 构建默认不启动 ticker，需显式 `TUVIS_HOME` + 环境变量才开（采集域纪律）。

### 8.2 TPS

维持现状（后端调度器可见 60s / 后台 600s；前端快照 5s、点 60s、心跳 30s）。本设计只要求：TPS 的 UI 读取不触发会话扫描。

### 8.3 采集域纪律

用量 ticker 会真实写账本（`~/.tuvis/tuvis.db`）。开发期必须遵守 `AGENTS.md`：改 `services/usage/**` 前停掉 `pnpm tauri:dev` 或把 `TUVIS_HOME` 指向影子目录；游标字段语义变更按迁移处理。注意冷跑一次实测 12.5s，ticker 首轮会付出这个代价。

## 9. 事件唤醒（第一刀，不再是可选项）

- 监控路径（1s 轮询，每次仅几次 `stat`）：
  1. `~/.tuvis/events/` 目录 mtime + **每个活跃会话的 `<sid>.json` 文件 mtime**（实测：原地写不改目录 mtime，只盯目录会漏）；
  2. `~/.tuvis/tps/opencode/facts.jsonl`；
  3. 当前活跃会话文件 mtime（从最近快照取，最多 5 个）。
- 变化 → `request_scan(reason)`；合并节流 ≥500ms。
- 顺手清理 `events/` 中超过 5 分钟的陈旧文件（实测 308 个陈旧文件、每轮 9ms）。
- 不用 `notify` 依赖：首版避免新增依赖与跨平台差异；若后续需要进程级事件再评估。

## 10. 失败与降级

| 场景 | 行为 |
| --- | --- |
| 扫描 panic | Scanner 内单飞 guard 清位；连续失败 ≥2 → 退避 30s；**看门狗**负责循环任务重建（对齐 `watcher.rs` 的 `LOOP_HANDLE` 自愈） |
| 冷启动无快照 | 命令同步扫一次（旧行为） |
| 快照过期（休眠唤醒/Scanner 卡死） | `ensure_fresh` 补扫；超时返回旧快照 + `health=degraded` |
| 事件源不可用 | 纯轮询兜底 |
| 事件落在扫描间隙 | 1s 唤醒 + 事件文件 mtime 监控，保证在 30s TTL 内被读到；Away 30s 依赖此路径 |
| 远控断连 | 下一周期降档（防抖一个周期） |
| 窗口心跳丢失 | TTL 90s 后视为无人看；只影响降档 |
| 用量 ticker 失败 | 复用 `collect` 的错误结果；UI 显示"采集失败"，下一周期重试 |
| 动作路径快照过期 | `ensure_fresh` 等待 ≤3s；超时按旧快照继续并标 `degraded`（不伪造成功） |

## 11. 迁移、兼容与落地顺序

不改 IPC 契约：`get_all_sessions` 入参/出参形状不变（仅追加 `mode`/`version` 字段）；调用方无需同步升级。

### 第一刀（本设计主体）

1. `Scanner` + `Cadence` + `Snapshot` + 看门狗；把 `remote::SessionWatcher` 的 2s 循环并入 Scanner（单飞下沉）；
2. `get_all_sessions` 读快照 + `ensure_fresh`；动作路径（含 `settings.rs` 归属查询、`commands/session.rs` 的 388 行路径、remote 各 `session_source` 调用点）统一走 `ensure_fresh`；
3. 手机看板保持 SSE + 30s 对账；配对计数并入总线或限频；
4. SSE 首帧 `ages ≤2s` 或补扫，带 `version`；
5. 主窗/桌宠 `refetchInterval` 跟随 mode；聚焦恢复立即重取；
6. 托盘 version 变化 + 30s 限频；
7. 事件唤醒（§9）；
8. 用量：可见即采（force）+ 后端 10 分钟兜底 + 成功广播；
9. 调整 `commands/usage.rs` 的 `polling_hot_path_*` 源码锁口径（保留 `sessions.ts` 的 `refetchInterval` 兜底语义）。

### 第二刀（独立验收）

1. 进程扫描两段式：`All + nothing()` 取名字（实测 10ms）→ 白名单 PID 二次刷新（12.5ms），`exe/cmd` 用 `OnlyIfNotSet`；**白名单必须包含 node/bun/python 等解释器名**（npm-shim 宿主，如 kimi 的进程名常是 node/bun），并保留 `exe > name > argv[0]` 回退与 cwd 配对；
2. 解析器细节：dsh 缓存每会话 gen 文件路径（**不做**目录 mtime 预过滤）；opencode 维持每次开库（实测 1.07ms，长连接风险大于收益）；
3. 日志降噪（INFO 只在状态跃迁）。

## 12. 测试

| 层 | 用例 |
| --- | --- |
| `Cadence` | 档位切换/防抖/时钟跳变/心跳过期；纯函数单测 |
| `Scanner` | 单飞（并发只扫一次）、version 单调、panic 后自愈、失败退避、看门狗重建 |
| 命令层 | 快照未过期不触发扫描（**缺席断言**）；过期触发补扫；动作路径 `ensure_fresh` 超时返回 `degraded` |
| 副作用 | mark_read/dismiss 后卡片不回弹（读快照 + 命令重算） |
| Watcher | 只基于快照 diff，不自扫（源码锁：`watcher.rs` 不得再出现 `get_all_sessions`） |
| SSE | 首帧 age ≤2s 或补扫；断线重连不丢跃迁（version 对账） |
| 托盘 | 30s 限频 + 跃迁立即更新 |
| 用量 | 可见即采（force，10 分钟窗口内也生效）、ticker 步长 > 最小间隔、成功广播 invalidate、dev 默认关闭 |
| 实机 | §4 测量脚本，前后对比 |

## 13. 风险与未决点

### 13.1 风险表

| # | 风险 | 影响 | 缓解 |
| --- | --- | --- | --- |
| R1 | 快照化后动作路径读到过期数据（新建会话 404） | 远程动作失败 | `ensure_fresh` + 调用点全量审计（remote 多处 + settings + commands） |
| R2 | 副作用被"只读"化导致已读/隐藏回弹 | 体验 | 命令层重算（§6.3） |
| R3 | 托盘 30s 限频让图标反应慢 | 体验 | 跃迁立即更新 |
| R4 | "有人在看"判定失真（桌宠常显 / 主窗失焦） | Away 不可达或误降档 | 三信号分开算（§7.1）；主窗聚焦恢复立即重取 |
| R5 | 事件唤醒漏掉原地写 | 审批/问答卡晚到或被 TTL 过滤 | 同时 stat 事件文件 mtime（§9）；Away 30s 依赖唤醒 |
| R6 | 进程分级漏 npm-shim 宿主 | 工具识别不到 | 白名单含解释器名 + 三路回退（§11 第二刀） |
| R7 | 用量 ticker 开发期写真实账本 | 账本污染 | dev 默认关闭 + `AGENTS.md` 纪律 |
| R8 | 配对计数 `System::new_all()` 游离在总线外 | CPU 不降 | 并入 Scanner 或 ≥10s 限频 |
| R9 | Scanner 循环 panic 无重启路径 | 看板冻结 | 看门狗（对齐 `LOOP_HANDLE`） |
| R10 | Tauri event 无重放（多窗口/首帧） | 档位同步失败 | `mode`/`version` 随命令响应下发，event 仅加速 |

### 13.2 未决点（需要用户拍板）

1. 托盘限频 30s 是否可接受，是否需要更短（如 10s）？
2. 手机看板是否也跟随 mode（空闲 10s 读快照，省 IPC）？当前设计保持 SSE + 30s 对账不变。
3. 动作路径 `ensure_fresh` 的等待上限 3s 是否合适（超时返回旧快照 + degraded）？

### 13.3 经实测否决的方案

| 方案 | 否决理由（实测） |
| --- | --- |
| opencode 长连接复用 | open+TOP200 仅 1.07ms；长持 WAL 读句柄 / 库被替换的失效风险大于收益 |
| dsh 目录 mtime 预过滤 | dsh 原地 append，目录 mtime 不变，会漏状态更新 |
| IPC 增量帧/压缩 | 会话数 1–3，单帧 1–3KB；Arc 已省掉重复扫描与内部 clone，序列化省不掉 |

## 14. 代码参考位置

- 扫描编排与单飞：`src-tauri/src/adapter/mod.rs`（`SCAN_FLIGHT` / `get_all_sessions` / `get_all_sessions_inner` / `read_hook_events`）
- 远控 watcher / 自愈：`src-tauri/src/remote/watcher.rs`（`SCAN_INTERVAL = 2s`、`LOOP_HANDLE`）
- 手机接口与配对计数：`src-tauri/src/remote/api.rs`、`src-tauri/src/remote/mod.rs`
- 托盘更新：`src-tauri/src/commands/session.rs`、`src-tauri/src/plugins/system_tray.rs`
- 前端轮询：`src/lib/query/queries/sessions.ts`、`src/config/constants.ts`、`src/mobile/Board.tsx`
- 用量触发：`src/lib/query/queries/usage.ts`、`src-tauri/src/services/usage/collect.rs`
- TPS 节奏：`src-tauri/src/services/tps/scheduler.rs`
- 解析器：`src-tauri/src/monitor/opencode_parser.rs`、`src-tauri/src/monitor/process.rs`
