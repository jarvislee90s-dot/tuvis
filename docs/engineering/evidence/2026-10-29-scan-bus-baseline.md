# 扫描总线改造 · 基线测量记录

- 日期：2026-10-29
- 分支：`design/scan-bus-and-adaptive-cadence`（基于 `origin/main` @ `5a53b9a7`，对应安装版 v0.6.3）
- 文档定位：Task 0 的基线证据；Task 12 在同一文件追加改造后复测，形成前后对照。
- 基线来源：`docs/engineering/specs/2026-10-29-scan-bus-and-adaptive-cadence-requirements.md` §1.1（2026-10-29 同日实测，安装版 0.6.3）

## 1. 结论先说

- **本次没有取新鲜实时样本**：安装版 Tuvis 当前未运行，且任务禁止启动 `pnpm tauri:dev` 或安装版应用，无法进行 5 分钟现场采样（原因见 §2）。
- 本文记录的数值是**同日在安装版 v0.6.3 上的实测结果**（需求说明书 §1.1），测量方法与脚本一致（`TotalProcessorTime` 采样 + 日志计数），可作为改造前基线。
- 测量脚本 `scripts/measure-scan-bus.ps1` 已落盘，语法校验与冒烟执行均通过（见 §5）；**Task 12 将用它产出改造后的复测值**并追加到本文件。

## 2. 为什么没有新鲜实时样本

- 安装版 Tuvis 当前未运行：`Get-Process tuvis` 无匹配进程（2026-10-29T09:42:57Z 与 09:45:57Z 两次核对均为未运行）。
- `C:\Users\bunny\.tuvis\logs\mam.log` 止于 **2026-10-29T09:41:35Z**，尾部为正常收尾（`tps/claude: 接收器已停止（端口 47181）`、`tailscale 通道全停有残留…`），未见 panic；记录本文件时日志仍无新增（26,152 行 / 2,857,215 字节）。
- 任务明确禁止 `pnpm tauri:dev` 与启动安装版应用，因此不能为凑样本而拉起进程。
- 需求 §1.1 的数值产生于同日、同一台机器、同一安装版，量级足以作为门禁对照；Task 12 复测时用同一脚本即可保证口径一致。

## 3. 基线原始数值

来源：需求说明书 §1.1「现象（2026-10-29 实测，安装版 0.6.3）」。

| 指标 | 基线值 | 说明 |
| --- | --- | --- |
| 空闲 CPU | **11.2% 单核** | 45s 采样消耗 5.05 CPU-s（`TotalProcessorTime` 差值） |
| 全量扫描频次 | **56–61 次/分** | ≈1.05s 一次 |
| 单轮扫描成本 | **≈0.1s CPU** | 其中进程刷新 44–57ms（466 进程） |
| 用量采集 | 仅在模型用量页激活时每 10 分钟一次，切走即停 | 实测数据滞后 4.5 小时 |
| 扫描发起方 | 后端 watcher 2s、桌面主窗 3s、桌宠 3s、手机 REST（配对计数 `System::new_all()`）各自触发 | 改造动因 |

说明：11.2% 来自单次 45s 采样，噪声较大；F10 要求验收时用 5 分钟 ×3 取中位，Task 12 按该口径复测。基线 CPU 是安装版 v0.6.3 上的 **45s 单次采样**，Task 12 用 **300s 采样**，采样窗口不同会带来小幅系统性差异（窗口越长越平滑），前后对照时需计入。

## 4. 本机进程数

- 基线时刻：**2026-10-29T09:41:41Z 实测 `(Get-Process).Count` = 421**（Task 0 基线测量记录）。
- 写入本文件时复核：2026-10-29T09:45:57Z `(Get-Process).Count` = **370**——应用已退出后的瞬时值，**不是基线样本**；进程数随时间波动，Task 12 复测时必须一并记录该值，便于同机同负载比较。
- 与上表「466 进程」的关系：466 为扫描器枚举进程数、采样时刻不同，两个计数不要求相等。

## 5. 测量脚本与校验

脚本：`scripts/measure-scan-bus.ps1`（以实施计划 Task 0 代码块为底本，评审后按 §6 加固了退出/日志/编码守卫；按仓库既有 `.ps1` 约定存为 **UTF-8 with BOM**）。

### 5.1 语法校验（实际输出）

命令（Git Bash 下用单引号包裹整条命令，避免 bash 展开 `$t`；路径改用双引号）：

```powershell
powershell -NoProfile -Command '$t=$null;$e=$null;[void][System.Management.Automation.Language.Parser]::ParseFile("E:\LLMproject\Github\tuvis\scripts\measure-scan-bus.ps1",[ref]$t,[ref]$e); if($e.Count){$e|ForEach-Object{$_.Message}; exit 1} else {"PARSE_OK"}'
```

输出：

```text
PARSE_OK
```

### 5.2 冒烟执行（非基线样本）

目标进程用 PowerShell 宿主自身（`-ProcessId $PID`），只验证脚本可执行与日志过滤生效，**不产生 Tuvis 基线数据**：

```powershell
powershell -NoProfile -Command '$id = $PID; & "E:\LLMproject\Github\tuvis\scripts\measure-scan-bus.ps1" -ProcessId $id -Seconds 2'
```

输出：

```text
pid=28408 wall=2.0s cpu=0.03s avgCPU=1.55% scans=0 scansPerMin=0.0
```

`scans=0` 与日志止于 2026-10-29T09:41:35Z 一致，说明时间过滤生效。

### 5.3 编码坑（务必保留 BOM）

首次以 UTF-8 **无 BOM** 保存时，冒烟输出为 `pid=23160 wall=2.0s cpu=0.05s avgCPU=2.32% scans=2229 scansPerMin=66,265.6`（2229 恰为日志尾部全部 `adapter] OpenCode:` 行）。根因：Windows PowerShell 5.1 按系统 ANSI（本机 GB2312）解码无 BOM 的 UTF-8 文件，中文注释行末字节吞掉了换行，`$since = …` 整行被并入注释，过滤条件退化为 `"["`，匹配全部日志行。

- **仅做语法解析（PARSE_OK）发现不了该问题**，必须看冒烟输出或 AST（注释 token 是否跨越到下一行）。
- 已按仓库既有 `.agents/skills/**/*.ps1` 的约定改存 UTF-8 with BOM（内容不变，`file` 显示 `UTF-8 (with BOM)`），复测 `scans=0` 正常。
- 后续任何人改写本脚本时，不要去掉 BOM，也不要把中文注释放回无 BOM 的 UTF-8 文件。

### 5.4 守卫回归（评审后补测）

| 场景 | 命令要点 | 实际结果 | 退出码 |
| --- | --- | --- | --- |
| 进程不存在 | `-ProcessId 999999 -Seconds 2` | `Get-Process` 抛 `NoProcessFoundForGivenId` | 1 |
| 采样期间进程退出 | 先起一个约 1s 后自退的 `powershell`，再 `-Seconds 4` | `进程 <pid> 在测量期间退出，样本无效` | 1 |
| 日志缺失 | 存活 PID + `-LogPath C:\nope\missing.log` | `日志文件不存在: C:\nope\missing.log` | 1 |
| 采样秒数越界 | `-Seconds 0` | 参数绑定失败（`ValidateRange(1, 86400)`） | 1 |
| 正常采样 | 存活 PID + 真实日志 `-Seconds 2` | `pid=<pid> wall=2.0s cpu=…s avgCPU=…% scans=0 scansPerMin=0.0`（格式未变） | 0 |

## 6. 脚本用法

```powershell
# 默认：自动找 tuvis 进程，采样 60s
powershell -NoProfile -File scripts/measure-scan-bus.ps1

# 验收：采样 5 分钟
powershell -NoProfile -File scripts/measure-scan-bus.ps1 -Seconds 300

# 指定进程与日志
powershell -NoProfile -File scripts/measure-scan-bus.ps1 -ProcessId 1234 -Seconds 300 -LogPath "D:\logs\mam.log"
```

输出字段：

| 字段 | 含义 |
| --- | --- |
| `pid` | 被测进程 ID |
| `wall` | 实际采样墙钟秒数 |
| `cpu` | 窗口内消耗的 CPU 秒（`TotalProcessorTime` 差值） |
| `avgCPU` | `100 × cpu / wall`，**相对单核的百分比**（100% = 占满 1 个核，不是整机多核占比） |
| `scans` | 窗口内日志中的全量扫描轮数 |
| `scansPerMin` | `scans / (wall / 60)` |

退出/异常：日志文件缺失、目标进程在采样期间退出、`-Seconds` 越界都会直接抛错并以**非零退出码**结束，需修复后重跑，不做静默兜底。

这些守卫是**评审后补加**的：PowerShell 的 `Get-Process` 对已退出 PID 默认只报**非终止错误**，原先采样结束时进程若已退出会得到 `$null` 进程对象、`cpu=0.00s` 且退出码 0，在 ≤3% 门禁下构成**静默假通过**；日志缺失时原脚本同样会打印 `scans=0` 并退出 0。现脚本在顶部设置 `$ErrorActionPreference = 'Stop'`，采样后显式判进程存活，读取日志前用 `Test-Path -LiteralPath` 检查存在性，并用 `[ValidateRange(1, 86400)]` 约束采样秒数。

## 7. 扫描计数口径与已知限制

- **现行计数源（R4 起）**：`~/.tuvis/logs/scan_bus_stats.json` 单行覆盖写（`total` + 最近一轮元数据）。脚本取前后 `total` 差值计扫描次数；`-Seconds ≥ 60` 且 `total` 未增长会**直接抛错**，计数回退（应用重启）同样抛错。
- **已退役计数源**：Fix 1–R3 的每轮 INFO `scan_bus: scan reason=…`（R4 为满足 F9 日志体积已删除，档位切换 INFO 保留）；§10.9 的历史原始输出仍按当时口径记录。最老的 `adapter] OpenCode:` 计数早已废弃。
- 历史口径说明：Task 12 曾把 `adapter] OpenCode:` 降为 DEBUG；更早的基线计数用它，仅用于理解旧数据。
- 只回看日志尾部 20,000 行（显式按 **UTF-8** 解码读取）。PS 5.1 默认按 ANSI（GB2312）读会把多字节中文行合并：整文件计数仅 **23,534** 条 vs 物理 **26,152** 行；尾读窗口也会位移——同样 20,000 条在 ANSI 下覆盖 **2,229** 条 OpenCode 行，UTF-8 下为 **2,006** 条。当前日志量级下 20,000 行窗口覆盖约 **30+ 分钟**，5 分钟复测不会截断。
- 时间过滤按日志 UTC 前缀 `[yyyy-MM-ddTHH:mm:ss` 做字符串比较，要求日志时间戳为 UTC 且以 `[` 开头（当前格式满足）。
- 脚本统计的是「全量扫描轮数」，不区分发起方；改造后由扫描总线单一来源产出（含 Event/Action 补扫）。
- 脚本只测 **tuvis 主进程**的 CPU，不含 WebView2 子进程；前端轮询等部分开销落在 WebView2 上。因此前后对照只有在**同一构建、同一 UI 状态**下才严格可比。

## 8. 门禁目标（Task 12 验收）

| 场景 | CPU 门禁 | 扫描频次门禁 |
| --- | --- | --- |
| 挂机（Away） | ≤3% 单核 | ≤6 次/分 |
| 远控在线但无活跃会话（Idle） | ≤6% 单核 | ≤10 次/分 |
| 有活跃会话（Active） | — | ≤30 次/分 |

来源：需求 §1.2 量化目标、§4 非功能门禁汇总、F2 验收条件、实施计划 Task 12 Step 3。另：活动档会话发现 ≤2s、动作回执 p95 不劣于基线、用量不可见时滞后 ≤ 设置间隔 + 2 分钟（默认 12 分钟；2026-10-30 F7 裁决，取代旧「≤10 分钟」口径）。

## 9. Task 12 复测约定

1. 确认应用在跑、日志级别放行 **info**（`RUST_LOG` 未设置即默认 info；扫描计数行是 INFO，不再需要 adapter 的 DEBUG）；
2. 建议同一状态跑两次（F10 要求同状态两次差异 ≤10%），取中位或两次都记录；
3. 同时记录 `(Get-Process).Count`、当前档位（挂机 / 远控在线空闲 / 有活跃会话）；
4. 把脚本输出**原样**追加到本文件，并补一张「基线 vs 改造后」对照表。

---

## 10. Task 12 改造后复测记录（2026-10-30）

本节是改造后的记账：**实时 5 分钟采样已完成**（经用户裁决关闭安装版后用影子数据目录运行本分支 debug 构建，方法与原始输出见 §10.2）；测不了的项一律写「未测」，不推算、不用代码推定值冒充实测值。

### 10.1 测点

- 时间：文档落笔 **2026-10-30 15:16 +0800**；活体复测窗口 **15:36–15:49 +0800**（2026-10-30T07:36–07:49Z）
- 分支：`design/scan-bus-and-adaptive-cadence`
- HEAD：Task 12 提交（`chore(scan): quiet per-round logs and record post-change measurements`）；复测二进制由该提交源码构建（`41ffd3c2` 之后的同一源码树，仅本证据文件在测后追加）
- 本机进程数：`(Get-Process).Count` = **468**；同一时段的扫描器枚举 **481–487**（sysinfo 与 PowerShell 口径不同，与 §4 的说明一致）
- 本任务 Step 1 的改动：`get_all_sessions_inner` 的每轮 `"{}: {} processes, {} sessions"` 由 `log::info!` 降为 `log::debug!`（`src-tauri/src/adapter/mod.rs`）。档位跃迁日志 `scan_bus: mode → {:?}` 保持 INFO；`scan: process refresh（两段式）耗时 …` 保持 DEBUG。

### 10.2 实时复测：方法、原始输出与口径

- 用户裁决（2026-10-30）：允许关闭安装版 Tuvis 完成复测、测完重启。执行时安装版已退出（原 PID 38720 不再存在），单实例锁释放。
- 构建：`cd src-tauri && CARGO_PROFILE_DEV_DEBUG=0 cargo build --bin tuvis -j 1`（debug profile，1m46s）。
- 影子数据目录：`C:\Users\bunny\AppData\Local\Temp\tuvis-measure-20261030`（`TUVIS_HOME`，debug 构建生效）。账本落在 `<shadow>\.tuvis\tuvis.db`，真实 `~/.tuvis/tuvis.db` 未被触碰。
- 日志：既有 logger 用 `dirs::home_dir()` 定位，仍写真实 `~/.tuvis/logs/mam.log`（日志非账本）；测量脚本按 `[since` 时间过滤计数，不读历史行。
- 启动环境：`RUST_LOG=info,multi_agents_manager_lib::adapter=debug`（每轮 adapter 行已降为 DEBUG，见 §10.8 第 7 条）。

**Active 档（有活跃会话）**：影子库中 WorkBuddy 有 1 个 `Processing` 会话；启动后最小化主窗（不影响活跃判定）。档位日志 `scan_bus: mode → Active`（07:36:38Z）。300s 原始输出：

```text
pid=47476 wall=300.0s cpu=8.06s avgCPU=2.69% scans=146 scansPerMin=29.2
```

**Away 档（挂机）**：把影子库 `agent_tools.workbuddy` 置 `enabled=0`（只改影子库），重启并最小化主窗；档位日志 `scan_bus: mode → Away`（07:43:48Z）。300s 原始输出：

```text
pid=19132 wall=300.0s cpu=1.13s avgCPU=0.37% scans=10 scansPerMin=2.0
```

- **口径限制（重要）**：两次样本均为 **debug（未优化）构建**，CPU 应视为**上界**（release 通常更快）；均为单次 300s 样本，未按 F10 的「5 分钟 ×3 取中位」做三次；远控在线（Idle）档未启用远控，未测。
- 测完已停止 debug 实例并重启安装版（新 PID 37496，`D:\Program Files\Tuvis\tuvis.exe`）。

### 10.3 可用的性能证据：进程刷新两段式（F8）

以下都是**实跑数据**，不是估算。

1. Task 11 实施轮报告（本机、同分支）：冷启动全量刷新 **65–110ms** vs 两段式 **约 30.5ms**，进程 485–528、候选 20–31。原始日志行未归档，此处为转述。
2. Task 12 复现（本次实跑）：直调活体回归用例 `adapter::candidate_whitelist_tests::two_stage_refresh_preserves_all_adapter_discovery`，连跑 4 次（每条都在**全新** `System::new()` 上各跑一次，等价「冷」调用）：

```text
two-stage: 487 processes, 19 candidates, full refresh 48.9544ms vs two-stage 29.0835ms
two-stage: 481 processes, 18 candidates, full refresh 64.8132ms vs two-stage 31.0124ms
two-stage: 481 processes, 18 candidates, full refresh 77.0651ms vs two-stage 33.6506ms
two-stage: 481 processes, 18 candidates, full refresh 64.1178ms vs two-stage 35.8791ms
```

命令：`cd src-tauri && CARGO_PROFILE_TEST_DEBUG=0 CARGO_PROFILE_DEV_DEBUG=0 cargo test -j 1 two_stage_refresh_preserves_all_adapter_discovery -- --nocapture`

| 指标 | 4 次范围 | 中位 | 与 Task 11 报告 |
| --- | --- | --- | --- |
| 全量刷新（旧路径，冷） | 49.0–77.1ms | ≈64.5ms | 65–110ms，同量级 |
| 两段式（新路径，冷） | 29.1–35.9ms | ≈32.3ms | 30.5ms，同量级 |
| 候选进程数 | 18–19 | — | 20–31（当时运行的工具更多） |

- **热路径实测（本轮新增，来自 §10.2 运行日志）**：解析 `scan: process refresh（两段式）耗时` DEBUG 行（215 拍）：Active 窗口 146 拍 **min 22.4 / 中位 29.6 / p95 65.9 / max 78.3ms**；Away 窗口 10 拍中位 45.4ms；末 100 拍中位 42.9ms。**F8 的「≤25ms（5 次取中位）」在 debug 构建下未达标**（中位 29.6ms）；release 未测（release 无法用 TUVIS_HOME 影子目录，跑真实账本不在授权内）。
- 两段式相对全量在冷路径上降到约 **一半**（中位 32.3ms vs 64.5ms），候选集约 4%。

### 10.4 节奏与扫描次数（代码级证据 + 活体复测）

- 档位间隔：**Active 2s / Idle 10s / Away 30s**（`src-tauri/src/adapter/cadence.rs::Mode::interval`）；升档立即生效，降档需候选档位稳定一个完整间隔才落地（`Cadence::observe`）。
- 事件唤醒：**1s 级 mtime 轮询**（`src-tauri/src/adapter/wakeup.rs`，每拍数次 `stat`：事件目录 + 前 8 个活跃会话事件文件 + opencode facts + `opencode.db` / `-wal`），变化即 `request_scan(ScanReason::Event)`。
- 事件延后（Fix 1/R2 终态）：孤立 Event 距上轮扫描不足 `event_min_gap = min(档位间隔, 5s)` 时，扫描线程用 `recv_timeout(剩余)` **延后**等待（非丢弃）；期间收到 Event 继续合并、收到 Action/ColdStart 立即中断补扫；Action 补扫/冷启动不受限制。旧的 500ms `EVENT_DEBOUNCE` 已被取代。
- 档位跃迁日志 `scan_bus: mode → {:?}` 为 INFO，**只在跃迁时打一行**，不能用来逐轮计数。
- 实测（§10.2）：Active **29.2/min**（300s / 146 次）、Away **2.0/min**（300s / 10 次）；Idle 未启用远控未测（代码推定 6/min）。事件唤醒会叠加，实际次数取决于本机事件密度。
- 扫描来源已单一化：命令层只读共享快照，托盘 / 看板 / 桌宠 / 远控 watcher 不再各自触发（对照基线 §3「扫描发起方」一行）。

### 10.5 用量采集（F7）

- 可见即采：`useUsageCollect` 在 active 的 false→true 边沿（含挂载即 true）发 `force=true`；前端 **60s** 强制去抖记在**模块作用域**（`lastForcedCollectAt`，同一 WebView 所有实例共享），防止宠物迷你条反复挂载卸载绕过最小间隔。
- 后端兜底：`ticker.rs::step_ms(interval_min) = interval_min × 60 000 + 120 000`，默认 `collect_interval_min = 10` → **步长 12 分钟**。
- **口径裁决（2026-10-30）**：需求 F7 技术方向（步长 = 设置间隔 + 2 分钟）与旧验收条件（≤10 分钟）自相矛盾。**用户裁决：验收口径改为「不可见滞后 ≤ 设置间隔 + 2 分钟（默认 12 分钟）」**，保留防空跑缓冲；本轮不改步长实现。
- 验收注意：debug 构建**默认不启动** ticker（需 `TUVIS_USAGE_TICKER=1`，见 `ticker.rs` 启动日志），拿 debug 包做「不可见 ≤10min」验收会必然假失败，须用 release 包或显式开环境变量。

### 10.6 门禁表（改造后；2026-10-30 Fix 轮口径修订）

> 口径修订（2026-10-30 评审 Fix 轮）：① 用量不可见滞后按 F7 裁决改为「≤ 设置间隔 + 2 分钟（默认 12 分钟）」；② F3 的验收口径是**诊断字段**（`scan_bus_status.degraded` / `snapshotAgeMs`，Fix 2 已提供），不扩展动作回执；③ 扫描计数改用 stats 文件 `total` 差值（R4，§7），复测原始输出见 §10.9/§10.10；④ F8 的 ≤25ms 目标按 debug 实测标注，release 未测。

| 门禁项 | 目标 | 结果 | 依据 / 备注 |
| --- | --- | --- | --- |
| 挂机（Away）CPU | ≤3% 单核 | **达标（Fix 轮稳态 0.59% / 首轮 3.23%）** | §10.9；首轮与启动采集重叠（47s）为离群，稳态两次 0.59% 与 §10.2 的 0.37%（无 UI）同向 |
| 远控在线空闲（Idle）CPU | ≤6% 单核 | **未测** | 未启用远控；参考 Active 稳态 2.61% ≤6% |
| 扫描 ≤30 次/分（有活跃会话 / Active 2s） | ≤30 次/分 | **达标（Fix 轮 29.4 / 29.2，两次差 0.7%）** | §10.9 原始输出 |
| 扫描 ≤10 次/分（远控在线但空闲 / Idle 10s） | ≤10 次/分 | **未测**；代码推定 6/min | 未启用远控 |
| 扫描 ≤6 次/分（挂机 / Away 30s） | ≤6 次/分 | **达标（Fix 轮 2.0 / 2.0，两次差 0%）** | §10.9 原始输出 |
| 活动档会话发现 | ≤2s | **未测**；代码路径 = Active 2s 档 + 1s 事件唤醒 | `scan_bus.rs` / `wakeup.rs` |
| 用量可见 | ≤1s 看到最新数据 | **未测**；代码路径 = 激活边沿 `force` 采集 + 成功后失效整个用量域 | `src/lib/query/queries/usage.ts` |
| 用量不可见 | 滞后 ≤ 设置间隔 + 2 分钟（默认 12 分钟） | **设计达标（未活体测）**：`step_ms = 间隔 × 60s + 120s`，默认 12 分钟 | F7 2026-10-30 裁决；debug 默认关 ticker，活体验收须 release 或 `TUVIS_USAGE_TICKER=1` |
| 日志体积 | 扫描总线新增日志 ≤1MB/天（dsh 告警归 F9） | **未测（R4 已删除逐轮 INFO）** | §10.8 第 7 条 / §10.11 |
| 兼容性 | IPC 契约不变，旧前端可继续工作 | **达标**（静态核对：新增命令为增量；`scan_bus_status` 仅追加字段；`scan_bus_watch` 仅新增可选 `source`） | `commands/session.rs` / `lib.rs` |
| 单轮进程刷新 | ≤25ms（5 次取中位） | **未达标（debug）**；release 未测 | §10.3；Fix 轮 Active 窗口单轮扫描 35–44ms（含解析，非纯进程刷新） |
| 动作回执 p95 | 不劣于基线 | **未测** | 无活体压测 |
| F3 补扫降级可见性 | `scan_bus_status` 可查 `degraded`/`snapshotAgeMs` | **达标（Fix 2）** | 单测 `status_reports_degraded_flag_and_snapshot_age`；看门狗纯函数 120s 阈值/去重单测 |
| F5 标已读/隐藏即时性 | 点击后 ≤1s 卡片消失（用户感知口径，2026-10-31 终稿） | **达标（F5-opt 活体 11/12ms）** | 前端乐观隐藏 `setQueryData`，命令后 finally `invalidateQueries` 回取（失败回滚）；命令内部等飞 ≤2s 不作为门禁；§10.13.3（单 run） |


### 10.7 工程门禁（本次实跑，原始结果）

命令与结果（Windows / Git Bash；`CARGO_PROFILE_TEST_DEBUG=0 CARGO_PROFILE_DEV_DEBUG=0` 降 debuginfo，避免本机 OOM）：

| 命令 | 结果 |
| --- | --- |
| `cargo test -j 1 --no-fail-fast` | **EXIT=101**；45 个测试目标：**3915 passed / 13 failed / 64 ignored**；失败集 = 本机改造前既有 13 条（见下），无新增 |
| `cargo clippy --all-targets -j 2 -q -- -D warnings` | **EXIT=0**，输出 0 行（零警告） |
| `cargo fmt --check` | **EXIT=0**，无输出 |
| `pnpm check`（format:check → lint → check:i18n → check-error-codes → tsc + vite build） | **EXIT=0**；Prettier 全符合；ESLint **0 errors / 3 warnings**（均为 `src/mobile` 既有告警）；i18n 2827 键对齐；错误码 338 码对账通过；`vite build` 成功 |

失败清单（**全部为改造前既有失败**，均属本机环境类：缺 `dist-mobile` 构建产物 / Windows 路径与 HOME 差异 / 本地目录与实时通道 / 时间戳类，与扫描总线无关）：

| 目标 | 失败用例 | 现场 |
| --- | --- | --- |
| lib（9 条） | `remote::server::tests::static_serving_matrix`、`remote::server::tests::png_asset_served_with_image_mime`、`remote::server::tests::board_entry_carries_reach_marker_for_tailscale_probe` | `dist-mobile` 静态产物缺失（`404 vs 200`、`dist-mobile 缺少 png 产物：先跑 pnpm build:mobile`）——本机未构建移动端产物 |
| lib | `remote::server::tests::file_endpoints_are_gated_and_safe` | 文件池身份键 Windows 路径大小写（`c:/users/...` vs `C:\Users\...`） |
| lib | `monitor::claude_parser::title_tests::get_claude_sessions_badge_end_to_end_new_layout` | 项目目录匹配（`/work/demo` → `-work-demo`） |
| lib | `monitor::codex_parser::report_flag_tests::report_flag_true_when_task_complete_within_window` | 时间窗判定 |
| lib | `monitor::opencode_parser::v2_tests::report_flag_true_when_child_updated_within_window` | 时间窗判定 |
| lib | `services::subagent_detection_tests::detect_subagents_existing_dir_lists_agents`、`detect_subagents_missing_dir_is_supported_empty` | 本机真实 subagent 目录内容（实际列出 `chinese-dev*` 等，用例期望固定夹具名） |
| `tests/tps_codex_source_test`（3 条） | `aged_fully_consumed_files_are_pruned_from_cursor`、`stale_new_files_skipped_but_cursored_files_always_resume`、`old_placeholder_cursor_starts_fresh_and_never_reinterpreted` | `cannot set file timestamp to 0` / 反斜杠路径 |
| `tests/tps_sampling_test`（1 条） | `live_serve_task_on_ephemeral_port_serves_one_request_and_releases_guard` | `实时请求必须落为 claude 事实`（本机实时通道） |

本机改造前基线：13 项失败是在本分支各 Task 的回归运行中于父提交上复现的环境性失败（dist-mobile 未构建 3 项、Windows 路径大小写 1 项、parser/HOME/子 agent 检测约 5 项、tps 采样/游标 4 项）；本系列改动未新增失败（改造前集合与 HEAD 集合逐名一致），不构成 BLOCKED。

### 10.8 已知缺口与遗留（如实申报）

1. **F9 的 dsh 代际路径缓存未做**：计划明确拆到后续计划（需改 `dsh.rs::scan_raw_logs` 内部缓存层，单独评审）；本轮只完成 F9 的日志部分。
2. **F3 降级诊断（Fix 2 已收口）**：`ensure_fresh` 超时仍按陈旧快照继续并打告警；现在 `scan_bus_status` 返回 `degraded`（扫描在飞超 120s，需重启恢复）与 `snapshotAgeMs`（快照龄），符合 F3「诊断字段即可」的裁决。看门狗只告警不强杀（清在飞位会放进第二份并发扫描）。
3. **F6 覆盖边界**：claude / codex / kimi 有 hook 事件；opencode 由 `opencode.db` + `-wal` 与 facts 文件覆盖；**openclaw 既无 hook 事件、也没有便宜的文件信号**（`wakeup.rs` 未覆盖），与 dsh 同理 → 二者仍靠档位兜底，Away 档下最坏 30s 才看到变化（`wakeup.rs` 模块文档已申报）。
4. **F5 即时性（F5-opt 终稿）**：命令为 async（R1，补扫在阻塞池内，允许等飞 ≤2s）；用户感知由**前端乐观隐藏**保证——点击即 `setQueryData` 更新缓存，命令结束（成功/失败）后在 finally `invalidateQueries` 回取权威快照（失败回滚）。活体见 §10.13.3（11ms，单 run；旧 §10.12.3 已作废）。
5. **看门狗退避 500ms**：panic 后 `sleep(500ms)` 再按当前档位 `recv_timeout`，最坏恢复 = 档位间隔 + 0.5s；Away 档即 **30.5s，略超 F1 的「≤30s 自动恢复」**（超 0.5s，可接受但需记账）。
6. **`POLL_INTERVAL` 已删除（Fix 7）**：`src/config/constants.ts` 的孤儿常量已移除，前端热路径源码锁改为只按 `refetchInterval` 内容发现（`commands/usage.rs`）。
7. **日志降噪与计数口径（R4 已收口）**：每轮 INFO 已删除，计数改读 `scan_bus_stats.json`（§7）；`RUST_LOG` 不再影响扫描计数。R4 前的 §10.9 历史输出仍按当时的 INFO 行口径记录。
8. **热路径耗时（已观测，未达标）**：`scan: process refresh（两段式）耗时` DEBUG 行在 §10.2 的 300s 运行中共 215 拍；Active 窗口中位 29.6ms（p95 65.9ms），末 100 拍中位 42.9ms。debug 构建下 F8 的 ≤25ms **未达标**；release 未测（release 无法用影子目录，跑真实账本不在授权内）。Fix 轮 Active（WorkBuddy 单会话）整轮扫描 `duration_ms` 35–44ms（含解析，非纯进程刷新），量级一致。
9. **启动采集的影响（Fix 轮新增）**：debug 构建即使 ticker 默认关闭，启动时仍会跑一次 `spawn_initial_collection`（本次 7 源 / 966 条 / 47.5s）。复测时若采样窗口与它重叠，CPU 会明显偏高（本轮两次首轮离群即此因）；正式验收应在启动采集完成后再开始计时。
10. **dsh 解析成本（F9 延后项）**：本机 DeepSeek Harness 桌面宿主在场时，dsh 适配器每轮解析 2 个会话（「未知代际 v4」降级卡），单轮扫描中位 220ms、max 916ms，Active CPU 被推到 ~19%。该项属 F9 第二刀（dsh 代际路径缓存，计划明确延后），本轮以影子库禁用 dsh 隔离；不代表扫描总线退化。

---

### 10.9 评审 Fix 轮复测（2026-10-30 17:49–18:25 +0800）

#### 10.9.1 目的与口径

> 历史口径说明（R4 后已退役）：本节记录 Fix 轮当时用每轮 INFO `scan_bus: scan reason=…` 计数；R4 起改为 stats 文件 `total` 差值（见 §10.10 / §10.11），INFO 行已删除。
- **构建**：debug（`CARGO_PROFILE_DEV_DEBUG=0 cargo build --bin tuvis -j 1`，49.3s），源码快照 = Fix 5 提交 `3a8d9de7`（Fix 6 只改脚本与本文档）。
- **UI 条件（与 §10.2 不同）**：§10.2 的 debug 二进制指向 `http://localhost:1420` 但当时没有 Vite 服务，前端实际未加载（后端-only）。本轮先启动 `pnpm dev`（仅 Vite，非 `pnpm tauri:dev`）再启动应用，主窗与桌宠均真实加载；测量仍只计 `tuvis.exe` 主进程（WebView2 子进程不在内）。
- **影子账本**：`TUVIS_HOME=C:\Users\bunny\AppData\Local\Temp\tuvis-measure-20261030`；真实 `~/.tuvis/tuvis.db` 由安装版维护（复测前后 mtime 17:46 → 18:29 均来自安装版，debug 实例未写）。
- **RUST_LOG**：未设置（env_logger 默认 info）→ 脚本输出 `rust_log=<未设置，env_logger 默认 info>`（控制台中文乱码是 PowerShell 输出编码，不影响日志与计数）。
- **Active 夹具（合成、已清理）**：本机复测时段没有真实活跃会话（WorkBuddy 未运行、dsh 最近会话为 10-24）。为复现 Active 档，在真实 `~/.workbuddy` 下临时新建：
  - 宿主进程：把 `powershell.exe` 复制为 `C:\Users\bunny\tuvis-measure-fixture\WorkBuddy.exe` 并 `Start-Sleep 3600`（满足 `is_host_process` 的 workbuddy exe 判据）；
  - 心跳 `~/.workbuddy/sessions/<pid>.json`（UUID 会话 id、kind=serve，每 20s 刷新 `lastHeartbeat`）；
  - 会话 JSONL `~/.workbuddy/projects/c-Users-bunny-tuvis-measure-fixture/<uuid>.jsonl`（每 60s 追加一行 `{"type":"function_call",…}` → 尾部语义 ToolCall → Processing）。
  - 测完已删除心跳、JSONL、项目目录与夹具目录，并结束夹具进程；真实账本未受影响（`~/.workbuddy` 仅新增可识别的新文件，已清理；遗留的 `36020.json` 为复测前 17:21 的真实旧心跳，未动）。
- **dsh 隔离**：本机 DeepSeek Harness 桌面宿主在跑，dsh 适配器每轮解析 2 个会话（WARN「未知代际 v4」），单轮扫描中位 **220ms**（max 916ms），会把 Active CPU 推到 ~19%。dsh 解析优化属 F9 第二刀（已延后），与本改造无关 → 影子库把 dsh 置 `enabled=0` 后测 Active；Away 阶段同样保持禁用。

#### 10.9.2 原始输出（脚本原样）

Active（WorkBuddy 合成会话 Processing、主窗最小化、档位日志 `mode → Active`）：

```text
pid=23028 wall=300.0s cpu=31.72s avgCPU=10.57% scans=147 scansPerMin=29.4
pid=23028 wall=300.0s cpu=7.84s avgCPU=2.61% scans=146 scansPerMin=29.2
```

Away（移除心跳夹具、档位日志 `mode → Away`）：

```text
pid=43004 wall=300.0s cpu=9.70s avgCPU=3.23% scans=10 scansPerMin=2.0
pid=43004 wall=300.0s cpu=1.77s avgCPU=0.59% scans=10 scansPerMin=2.0
```

- **扫描次数两次差异**：Active 147 vs 146（0.7%）、Away 10 vs 10（0%）→ 满足 F10「同状态两次差异 ≤10%」。
- **CPU 首轮离群**：两次首轮都在应用启动后 ~1–2 分钟开始，与 `usage: 启动采集完成（7 源 / 966 条新记录 / 47465ms）` 的启动采集重叠（debug 构建仍跑启动采集，无论 ticker 是否启用）；第二轮为稳态。Away 稳态 0.59% ≤3%；Active 稳态 2.61%（无 Active CPU 门禁，参考值）。
- **无 UI 阶段的两次退出**：Vite 未启动时（前端空白）两个实例分别在 300s 采样中自行退出（日志止于 `tps/claude: 接收器已停止`，无 panic / Windows 应用错误事件）。启动 Vite 后实例稳定完成两次 300s。退出原因未定位，但与本改造无直接证据（无 panic、无 degraded 事件）；如实记账。

#### 10.9.3 与 §10.2 的差异

| 项 | §10.2（Task 12） | §10.9（Fix 轮） |
| --- | --- | --- |
| 计数源 | `adapter] OpenCode:`（DEBUG，需 RUST_LOG 放开） | `scan_bus: scan reason=`（INFO，默认） |
| 前端 | 未加载（无 Vite） | 已加载（`pnpm dev`），主窗最小化 |
| Active 会话来源 | 真实 WorkBuddy Processing | 合成 WorkBuddy 心跳/JSONL 夹具（已清理） |
| dsh | 未特别处理 | 影子库禁用（隔离 F9 已知解析成本） |
| Active 次数/CPU | 146 / 2.69% | 147·146 / 10.57%·2.61% |
| Away 次数/CPU | 10 / 0.37% | 10·10 / 3.23%·0.59% |

#### 10.9.4 Fix 1 活体验收（事件唤醒：延后而非丢弃）

- debug 构建的事件目录随 `TUVIS_HOME` 重定向到 `<shadow>/.tuvis/events`（真实 `~/.tuvis/events` 不被消费——首次误写真实目录时确实未触发扫描，已删除）。
- 影子事件文件 `feedface-0000-4000-8000-000000000002.json` 写入时刻 **11:05:34Z**；日志在 **11:05:39Z** 出现 `scan_bus: scan reason=Event version=4`。
- 这 5s 正是 Fix 1 的 `event_min_gap = min(档位间隔 30s, 5s)`：上一轮周期扫描在 11:05:34Z，孤立 Event 被**延后**到最小间隔后照常扫描。修复前该事件会落在 <500ms 窗口内被 `return` 丢弃（Away 档下一次周期扫描要等 30s，hook 文件 30s TTL 已过期）——活体行为与设计一致。

#### 10.9.5 Fix 4 活体验收（F5：点单卡 X → 卡片 ≤1s 消失）

- 环境同 §10.9：debug 构建（源码含 Fix 4）+ `TUVIS_HOME` 影子目录 + `pnpm dev` 前端 + 合成 WorkBuddy Processing 卡；额外给 WebView2 加 `--remote-debugging-port=9223`，经 CDP 在**真实主窗页面**执行点击与计时（非模拟事件）。
- 操作：找到卡片头部的「暂不显示（会话状态变化后恢复）」按钮（`dismiss_session_card` 路径），记录 `performance.now()` 后 `btn.click()`，每 20ms 轮询卡片项目名是否从 DOM 消失。
- 实际结果：**`{"gone":true,"elapsedMs":75,"polls":2}`** —— 卡片在点击后 **75ms** 消失（门禁 ≤1s），与「命令内同步补扫 + 成功后 invalidateQueries」的设计一致。
- 测后已清理合成夹具与调试端口，安装版重启。

---

### 10.10 第三轮评审（R1–R7）Release 验收（2026-10-30 16:39–17:28Z，本地次日 00:39–01:28）

本节按 F10 新口径（release / 进程树 / 预热后 3×5 分钟中位 / stats 计数）记录 R5，并汇总 R1–R4、R6–R7 的验收证据。

#### 10.10.1 构建与环境

- 源码：HEAD `7e19de20c772b7641323e3ec2a42272bbbf27db7`（R7 提交；本节为随后唯一的文档提交）。
- 构建：`pnpm build` 后 `CARGO_BUILD_JOBS=2 pnpm exec tauri build --no-bundle --features marker-helper`（release profile：lto=true / codegen-units=1 / strip=true）。
- 产物：`src-tauri/target/release/tuvis.exe`，48,291,328 B，sha256 `4f5168e80ffab23d8c1bdeb77262f5ba85f18693ba8b186729bace7517583a4e`。
- 运行：真实 HOME（用户授权），未设 `TUVIS_HOME`；stats = `~/.tuvis/logs/scan_bus_stats.json`，日志 = `~/.tuvis/logs/mam.log`。
- 时间线（UTC）：启动 **16:39:32**；启动采集完成 16:39:48（12.5s，已覆盖预热要求）；窗口隐藏后 `mode → Away` **16:44:59**（watch 心跳 TTL 90s 到期后降档）；SSE 配对成功并开始 Idle 采样 **17:04:51**（snapshot version 74）；结束 17:28。
- Idle 模拟：本机 `POST /m/api/v1/pair/pin`（PIN 取自设置）→ 200 + `Set-Cookie`；`GET /m/api/v1/events` → 200，测量期间保持 SSE 连接（真实配对流程，真实 DB 新增设备名 `measure-idle`）。
- **Active：未测**。opencode v2.2.2 已安装，但未启动真实会话（避免未经授权的模型调用/计费），按要求如实记未测。

#### 10.10.2 原始输出（脚本原样）

Away（无远控 + 主窗隐藏）：

```text
pid=34412 wall=300.6s cpu=1.44s avgCPU=0.48% scans=10 scansPerMin=2.0 stats_last_reason=Periodic stats_last_duration_ms=51 stats_process_refresh_ms=16 stats_updated_at=2026-10-30T16:50:00.420Z cpu_tree=11.42s avgCPU_tree=3.80% tree_procs=9
pid=34412 wall=300.6s cpu=1.59s avgCPU=0.53% scans=10 scansPerMin=2.0 stats_last_reason=Periodic stats_last_duration_ms=54 stats_process_refresh_ms=18 stats_updated_at=2026-10-30T16:55:01.005Z cpu_tree=10.17s avgCPU_tree=3.38% tree_procs=9
pid=34412 wall=300.5s cpu=1.42s avgCPU=0.47% scans=10 scansPerMin=2.0 stats_last_reason=Periodic stats_last_duration_ms=51 stats_process_refresh_ms=15 stats_updated_at=2026-10-30T17:00:01.599Z cpu_tree=10.58s avgCPU_tree=3.52% tree_procs=9
```

Idle（SSE 在线、无活跃会话）：

```text
pid=34412 wall=300.5s cpu=2.58s avgCPU=0.86% scans=30 scansPerMin=6.0 stats_last_reason=Periodic stats_last_duration_ms=55 stats_process_refresh_ms=17 stats_updated_at=2026-10-30T17:12:33.950Z cpu_tree=10.86s avgCPU_tree=3.61% tree_procs=9
pid=34412 wall=300.6s cpu=2.80s avgCPU=0.93% scans=30 scansPerMin=6.0 stats_last_reason=Periodic stats_last_duration_ms=50 stats_process_refresh_ms=13 stats_updated_at=2026-10-30T17:17:35.773Z cpu_tree=12.45s avgCPU_tree=4.14% tree_procs=9
pid=34412 wall=300.6s cpu=2.56s avgCPU=0.85% scans=30 scansPerMin=6.0 stats_last_reason=Periodic stats_last_duration_ms=52 stats_process_refresh_ms=16 stats_updated_at=2026-10-30T17:22:37.519Z cpu_tree=11.59s avgCPU_tree=3.86% tree_procs=9
```

- 3 次中位差异：扫描次数 Away 2.0/2.0/2.0、Idle 6.0/6.0/6.0（0%）；进程树 CPU：Away 中位 3.52%（最大偏离 8.0%），Idle 中位 3.86%（最大偏离 7.3%）；主进程 CPU：Idle 中位 0.86%（最大偏离 8.1%），Away 中位 0.48%（run2 0.53% 偏离 10.4%，绝对值差 0.05pp，属 0.5% 量级的量化噪声）。
- **F8（process refresh，release）**：stats 连续 6 个样本 `16, 14, 17, 18, 13, 21` ms → 排序 13/14/16/17/18/21，**中位 16.5ms ≤25ms 达标**。
- **F7 兜底 ticker（release 默认开启）**：`usage: 兜底 ticker 已启动（步长 12 分钟）`（16:39:32）；采集完成 16:51:32（254ms）、17:03:33（252ms）、17:15:33（255ms）→ 相邻间隔 **12m01s**，`≤ 设置间隔 + 2 分钟（12 分钟）` 达标。
- **用量页可见刷新**：CDP 真实点击「模型用量」→「数据截止时间」由 `01:25:49` 刷新为 `01:27:24`，**317ms ≤1s 达标**（另一次首访仅渲染页面的 label 出现耗时 190ms）。
- **F6 事件唤醒（release）**：向真实 `~/.tuvis/events` 写入事件文件后，stats 在 **966ms** 出现 `last_reason=Event`（≤5s 达标）；debug 影子轮次另测 377ms（§10.9）。
- **日志体积（release 运行段 16:39:32–17:27:49，48m17s）**：551 行 / 76,308 B → 外推约 **2.27MB/天**；分源：`dsh:` **424 行**（未知代际告警等，F9 延后项）、`漂移L3` 16、`扫描` 8、`usage/dsh` 7、`usage` 6、`scan_bus` **2**（仅档位跃迁）、其余少量。扫描总线已无逐轮 INFO；R4 的「Active 10 分钟 ≤60KB」在 debug 影子轮次实测 **444B**（§10.9.2），达标。

#### 10.10.3 门禁表（release 口径）

| 门禁项 | 目标 | 结果 | 证据 |
| --- | --- | --- | --- |
| 挂机 CPU（仅主进程） | ≤3% 单核 | **达标 0.48%** | §10.10.2 Away 三次中位 |
| 挂机 CPU（进程树含 WebView2） | ≤3% 单核 | **未达标 3.52%**（9 个 WebView2 进程） | 同上 `avgCPU_tree` |
| 远控在线空闲 CPU | ≤6% 单核 | **达标**：主 0.86% / 树 3.86% | §10.10.2 Idle 中位 |
| 扫描频次（Active） | ≤30 次/分 | **未测**（无真实活跃会话） | — |
| 扫描频次（Idle） | ≤10 次/分 | **达标 6.0** | Idle 三次 `scansPerMin=6.0` |
| 扫描频次（Away） | ≤6 次/分 | **达标 2.0** | Away 三次 `scansPerMin=2.0` |
| 活动档会话发现 | ≤2s | **未测**（无 Active 会话） | — |
| 动作回执 p95 | 不劣于基线 | **未测** | — |
| F6 事件唤醒 | ≤5s | **达标 966ms**（release）/ 377ms（debug） | stats `last_reason=Event` |
| F7 用量不可见滞后 | ≤12 分钟 | **达标**（ticker 间隔 12m01s） | §10.10.2 日志 |
| 用量可见刷新 | ≤1s | **达标 317ms** | CDP 探针 |
| F8 进程刷新 | ≤25ms（release） | **达标 16.5ms**（6 样本中位） | stats 样本 |
| 日志体积 | ≤1MB/天 | **未达标（外推 2.27MB/天）**；扫描总线自身无逐轮 INFO，主体为 dsh 解析告警（F9 延后） | §10.10.2 分源 |
| F5 卡片消失 | ≤1s | **达标 76ms** | R1 debug 构建 + CDP 真实点击（下方 10.10.4） |
| 兼容性 | IPC 契约不变 | **达标**（静态核对：新增命令为增量，回执字段为新增） | 源码 |
| 同状态 3 次差异 | ≤10% | **基本达标**：扫描 0%、树 CPU ≤8.0%、Idle 主 CPU ≤8.1%；Away 主 CPU 单次 10.4%（0.05pp 量化噪声） | §10.10.2 |

#### 10.10.4 R1–R4、R6–R7 验收摘要

- **R1**：源码锁（两单卡命令 async + `spawn_blocking`、无 `scan_now_blocking`）通过；批量 10 项只产生 **1 次扫描**、version 递增通过；前端 `applied=false` 仍失效缓存且 console.warn 用例通过；活体（R1 debug 构建 + 影子 HOME + 合成 WorkBuddy 卡，CDP 真实点击单卡 X）→ 卡片 **76ms** 消失（≤1s）。
- **R2**：`event_wait_plan` / `wait_event_defer` 单测覆盖 Event 合并、Action 立即中断、超时照常扫描；源码锁禁止 Event 分支出现 `sleep(`。
- **R3**：`Snapshot.degraded`（空/超时快照为 true，且不被当作新鲜快照）；`ensure_fresh(2s,100ms)` 在飞且无新快照 → 返回 degraded=true、`scan_bus_status.degraded=true`、无快照时 `snapshotAgeMs=null`；`status_degraded` 纯函数验证 30s 窗口恢复 false。
- **R4**：stats 单行 JSON 字段单测 + 覆盖写单测；debug 影子 10 分钟 Active：日志增量 **444B**、`stats.total` 增量 **287** = 扫描次数、事件写入后 **377ms** stats 记录 `last_reason=Event`。
- **R6**：既有 8 条 SSE 用例全绿；新增「快照等待期间 `disconnect_all` 能关断」用例通过（等待期连接已注册、吊销后计数归零、流立即结束）。
- **R7**：新增源码锁（`tool_host_alive_in` 分支 + `is_host_process` match arm 均须登表）；变异验证：临时删除 zcode 表项 → 用例变红，恢复后变绿。

#### 10.10.5 偏差与已知影响

1. **挂机进程树 CPU 3.52% 超出 3%**：主进程仅 0.48%，差额来自 9 个 WebView2 进程（主窗/桌宠/通知窗渲染）。本次未调整前端渲染或桌宠动画，如实记为未达标。
2. **Active 未测**：无真实 opencode 会话（避免未经授权的模型调用）；Active 档扫描频次/会话发现/CPU 仍无 release 数据。
3. **Away 降档等待 90s**：窗口隐藏后 `scan_bus_watch` 停止续期，watching TTL 90s 到期才降 Away（设计如此）；测量在降档后开始。
4. **日志体积 2.27MB/天**：主体是 dsh 解析告警（424/551 行），属 F9 第二刀（dsh 代际路径缓存/日志降噪）范围，本批未做；扫描总线每轮 INFO 已删除（全段仅 2 条档位跃迁）。
5. **真实账本写入（授权内）**：release 用真实 HOME 运行，启动采集/兜底采集写入真实账本（0 条新记录）；SSE 模拟新增配对设备 `measure-idle`（正常配对流程，可在设置-远程设备中吊销）。
6. **F5 活体在 debug 影子构建完成**：release 环境无会话卡可点；R1 代码路径与 release 相同（仅数据目录隔离不同），记录为 76ms。

---

### 10.11 第四轮收尾（R8/R9 + E1–E5，2026-10-31 10:50–11:25 +0800）

#### 10.11.1 构建与样本

- 源码：HEAD `a459a96901d3c1357950618b9db1639aaadbc607`（R9）；release 构建 `pnpm build` + `CARGO_BUILD_JOBS=2 pnpm exec tauri build --no-bundle --features marker-helper`。
- 产物：`src-tauri/target/release/tuvis.exe`，48,315,904 B，sha256 `6a2e2f6ea4bc00f74dbd17dc98851ca8e5c57755351837837f4273236c7ecad0`。
- 基线：安装版 **0.6.3**（`D:\Program Files\Tuvis\tuvis.exe`），同机、同窗口状态（主窗最小化）；0.6.3 无 stats 文件，用脚本新增的 `-CpuOnly` 只测 CPU。
- 分支：真实 HOME + release；opencode 会话来自免费模型 `opencode/deepseek-v4.1-flash-free`（已配置，零成本）；测量期间真实生成，额度在测量结束后耗尽（`Free usage limit reached`）。

#### 10.11.2 E1：0.6.3 进程树基线（300s/次）

Away（无远控、主窗最小化）：

```text
pid=35748 wall=300.6s cpu=17.06s avgCPU=5.68% mode=cpu_only cpu_tree=27.20s avgCPU_tree=9.05% tree_procs=9
pid=35748 wall=300.8s cpu=51.59s avgCPU=17.15% mode=cpu_only cpu_tree=63.86s avgCPU_tree=21.23% tree_procs=9
pid=35748 wall=300.5s cpu=55.16s avgCPU=18.35% mode=cpu_only cpu_tree=66.89s avgCPU_tree=22.26% tree_procs=9
```

Idle（SSE 已连：`PAIR_STATUS 200` / `SSE_STATUS 200`，快照首帧正常）：

```text
pid=35748 wall=300.5s cpu=49.73s avgCPU=16.55% mode=cpu_only cpu_tree=62.03s avgCPU_tree=20.64% tree_procs=9
pid=35748 wall=300.5s cpu=51.17s avgCPU=17.03% mode=cpu_only cpu_tree=63.67s avgCPU_tree=21.19% tree_procs=9
pid=35748 wall=300.4s cpu=49.59s avgCPU=16.51% mode=cpu_only cpu_tree=61.16s avgCPU_tree=20.36% tree_procs=9
```

基线中位：Away main **17.15%** / tree **21.23%**；Idle main **16.55%** / tree **20.64%**。

#### 10.11.3 E2：Active（release + 真实 opencode 会话）

真实 opencode 会话（免费模型，连续短提示约 3.4s/轮）保持活动，3 次 300s：

```text
pid=24072 wall=300.6s cpu=10.16s avgCPU=3.38% scans=146 scansPerMin=29.1 stats_last_reason=Event stats_last_duration_ms=60 stats_process_refresh_ms=18 stats_updated_at=2026-10-31T03:08:19.106Z cpu_tree=27.08s avgCPU_tree=9.01% tree_procs=9
pid=24072 wall=300.5s cpu=11.45s avgCPU=3.81% scans=145 scansPerMin=29.0 stats_last_reason=Event stats_last_duration_ms=56 stats_process_refresh_ms=20 stats_updated_at=2026-10-31T03:13:19.909Z cpu_tree=31.70s avgCPU_tree=10.55% tree_procs=9
pid=24072 wall=300.5s cpu=10.22s avgCPU=3.40% scans=146 scansPerMin=29.2 stats_last_reason=Event stats_last_duration_ms=60 stats_process_refresh_ms=15 stats_updated_at=2026-10-31T03:18:21.860Z cpu_tree=29.67s avgCPU_tree=9.88% tree_procs=9
```

- 中位：main **3.40%** / tree **9.88%** / **29.1 次/分**（≤30 达标）。
- **会话发现延迟（旧口径，已被 §10.12.4 取代）**：`2455 / 448 / 3000 / 2935 / 2879` ms（另 1 次 20s 超时）；成功样本中位 **2879ms**。该口径 t0 含 opencode CLI 启动，且用自动标题里的 token 做探测（opencode 会重写标题，1 次超时**推断**为探测失效——§10.13 已降级为推断，未证伪事件漏唤醒）——终稿改为 **t0=会话落库**，见 §10.12.4（中位 898ms ≤2s）。

#### 10.11.4 E3：F8 原始样本与 F5 原始 JSON（当前构建）

- F8 `last_process_refresh_ms` 连续 6 次原值：`15, 17, 18, 12, 28, 18` → 排序 `12/15/17/18/18/28`，**中位 17.5ms**（≤25 达标）。
- F5（release + Active，真实点击单卡 X）：

```json
{"gone":true,"elapsedMs":2090,"polls":100,"button":"暂不显示（会话状态变化后恢复）"}
```

  更正（E7）：§10.9.5 的 75ms 与 §10.10.4 的 76ms 均为 **Active 档**（合成 Processing 卡）的点击值，不是空闲值。2090ms 是 R1 设计下「命令 await 补扫后再失效缓存」在 Active 档的峰值。用户感知口径终稿（e4320c1c）要求前端乐观隐藏：F5-opt 后当前 release 两次活体点击 **11ms / 12ms**（原 §10.12.3 已作废；终稿单 run 见 §10.13.3）；命令内部等飞 ≤2s 不再计入门禁，R8 的降级补排保证最终收敛。

#### 10.11.5 E4：稳定性与工程门禁（HEAD a459a969）

- `cargo test --test codex_ipc_remote` 连跑 10 次：**9 通过 / 1 失败**；唯一失败 `t2_a_accepted_b_rejected_pause_survives`（本次），上一轮整包高负载下曾失败 `r3_concurrent_interrupts_each_withdraw_own_token_only`（不同用例）。该用例单独连跑 **30/30 通过**；失败仅出现在整包并行+机器负载下，属既有并发时序抖动，R6（SSE 注册守卫）不触及 pause/token 逻辑，未发现 R6 引入的确定性回归。
- `cargo test --lib`（连续 5 次，加固 SSE 等待用例后）：**3168 passed / 6 failed**，5 次失败集完全一致：
  1. `monitor::claude_parser::title_tests::get_claude_sessions_badge_end_to_end_new_layout`
  2. `monitor::codex_parser::report_flag_tests::report_flag_true_when_task_complete_within_window`
  3. `monitor::opencode_parser::v2_tests::report_flag_true_when_child_updated_within_window`
  4. `remote::server::tests::file_endpoints_are_gated_and_safe`
  5. `services::subagent_detection_tests::detect_subagents_existing_dir_lists_agents`
  6. `services::subagent_detection_tests::detect_subagents_missing_dir_is_supported_empty`
- 集成目标既有失败 4 条（tps）：`aged_fully_consumed_files_are_pruned_from_cursor`、`old_placeholder_cursor_starts_fresh_and_never_reinterpreted`、`stale_new_files_skipped_but_cursored_files_always_resume`、`live_serve_task_on_ephemeral_port_serves_one_request_and_releases_guard`。
- 与改造前 13 条基线逐名对照：以上 10 条均为原 13 条的子集（3 条 dist-mobile 静态用例因本机已有 `dist` 产物转为通过）；**无新增失败**。
- `cargo clippy --all-targets -j 2 -D warnings`：**0 告警**。
- `pnpm test`：**202 文件 / 2515 passed / 5 skipped**；`pnpm check`：**EXIT 0**（3 条既有 mobile warning）。

#### 10.11.6 E5：日志口径（scan_bus vs dsh）

release 运行窗口 03:00:50Z–03:23:02Z（约 22.2 分钟）：

| 来源 | 行数 | 字节 | 外推/天 | 口径 |
| --- | --- | --- | --- | --- |
| `scan_bus`（仅档位跃迁） | 5 | **479 B** | **≈30 KB/天** | 门禁 ≤1MB/天 → **达标** |
| `dsh`（解析告警，F9 延后） | 1186 | 160,182 B | ≈10.4 MB/天 | 不计入本批 |
| 全部日志 | 2358 | 308,982 B | ≈20.0 MB/天 | — |

注：dsh 行数在本轮 Active 下显著升高（每轮扫描告警），属 F9 第二刀（dsh 代际路径缓存/日志降噪）范围。

#### 10.11.7 最终门禁表（2026-10-31，第四轮）

| 门禁项 | 目标 | 结果 | 依据 |
| --- | --- | --- | --- |
| 挂机 CPU（主进程） | ≤3% | **达标 0.48%** | §10.10（release）；基线 17.15% |
| 挂机 CPU（进程树含 WebView2） | 不劣于 0.6.3 同口径基线 | **达标（不劣于基线）**：本批 3.52% vs 基线 21.23%（-83.4%）；绝对 ≤3% 仍差 0.52pp | §10.10 / §10.11.2 |
| 远控空闲 CPU（main/tree） | ≤6%；tree 不劣于基线 | **main 0.86% 达标；tree 3.86% ≤6%，且远优于基线 20.64%** | §10.10 / §10.11.2 |
| 扫描频次（Active） | ≤30 次/分 | **达标 29.1**（29.1/29.0/29.2） | §10.11.3 |
| 扫描频次（Idle/Away） | ≤10 / ≤6 | **达标 6.0 / 2.0** | §10.10 |
| 活动档会话发现 | ≤2s（t0=会话落库，终稿） | **达标（中位 898ms，5 样本）** | §10.12.4（§10.11.3 旧口径已作废） |
| F6 事件唤醒 | ≤5s | **达标 1369ms**（当前构建；上轮 966ms/377ms） | §10.11 |
| F7 用量不可见 | ≤12 分钟 | **达标**：ticker 12m01s | §10.10 |
| 用量可见刷新 | ≤1s | **达标 317ms** | §10.10 |
| F8 进程刷新 | ≤25ms | **达标 17.5ms**（6 样本中位，当前构建） | §10.11.4 |
| F5 卡片消失 | 点击后 ≤1s（乐观隐藏终稿） | **达标 11ms**（单 run，§10.13.3） | §10.13.3 |
| 日志（扫描总线新增） | ≤1MB/天 | **达标 ≈30KB/天**；dsh 告警归 F9 | §10.11.6 |
| 工程门禁 | 全绿（既有环境失败除外） | **达标**：clippy 0、pnpm test/check 绿、lib 5 连跑稳定 | §10.11.5 |
| 同状态 3 次差异 | ≤10% | 扫描 0.3%；tree CPU 8.0%（Active 9.01/10.55/9.88）；基线本身波动更大 | §10.11.2/3 |

#### 10.11.8 偏差与剩余风险

1. **F5 在 Active 档 2090ms**：命令等 `scan_after_mutation`（在飞+补扫）后才失效缓存；R8 只保证超时后补排、最终收敛。若要 ≤1s，需要前端乐观隐藏或版本门控失效（后续单独裁决）。
2. **会话发现 2879ms**：含 opencode CLI 启动与会话落库，事件轮询 1s + Active 2s。若只看扫描侧应 ≤2s；按"发起→卡片"口径未达标。
3. **codex_ipc_remote 偶发**：整包并行+高负载下 1/10；单用例 30/30；判为既有并发时序抖动，非 R6/R9 确定性回归。
4. **免费模型额度已耗尽**：Active 复测不可再重复（测量期间真实生成，额度在结束后耗尽）。
5. dsh 日志告警（≈10.4MB/天）仍待 F9 第二刀。

---

### 10.12 第五轮收尾（R10 / R11 / F5-opt / E6 / E7，2026-10-31 12:00–12:35 +0800）

#### 10.12.1 构建与环境

- 源码：HEAD `b734a9700c5aad29dc5bd31bc5dc1bc2f2c32c8e`（R10/R11 + F5-opt；E7 仅文档）。
- release：`pnpm build` + `CARGO_BUILD_JOBS=2 pnpm exec tauri build --no-bundle --features marker-helper`；产物 sha256 `20f139f25813ecd523664bbcea1f07ec46d4e9077ff8b495921ceddf88c099c9`。
- 真实 HOME；远端在线口径设备复用已配对 `e6`（`mam_device=0d4ddc32…`；设备上限 10 已满，未再新建配对）。
- 模型：`opencode/nemotron-3.5-lightning-free`（deepseek/glm/mimo 免费额度耗尽，minimax 不可用）。

#### 10.12.2 R10：degraded 窗口条件复位（单测）

- 实现：`scan_once` 在赢得单飞时捕获 `started_ms` 并传入 `publish`；`publish` 仅当 `started_ms > LAST_TIMEOUT_MS` 时用 CAS 清零。
- 用例：
  - `publish_keeps_timeout_window_when_scan_started_before_it` → **ok**（`set_last_timeout(now)` 后，`started=now-1` 发布仍 `degraded=true`；`started=now+1` 发布后复位 false）。
  - `publish_resets_degraded_timeout_window`（R9 用例改造）→ **ok**。
- 活体：release 无法注入慢扫描（无测试钩子），本项以单测为准；F3 的 30s 窗口不再被「启动早于超时的在飞扫描」毫秒级抹平。

#### 10.12.3 F5-opt 活体验收（release + Active，真实点击）

> **本条作废（2026-10-31 第六轮）**：原 JSON 混用了两次运行的数据——点击时 `status.mode=away/version=83`，命令契约却返回 `version=73`，两次观测不属同一 run；`applied` 也在点击后重复调用命令才取得。不得据此引用版本单调性或 mode 一致性。**单 run 重录见 §10.13.3**（同一 run 内 before → 点击 → after → 契约，version 单调、mode 与环境一致）。原 JSON 已从本节移除，避免被再次误引。

- 旧记录（保留备查）：两次点击 11ms / 12ms；卡片在命令返回前即消失；组件测试 2 条（`tests/session/optimisticHide.test.tsx`）。该节仅说明乐观隐藏方向可行，数值与状态字段不再作为门禁依据。

#### 10.12.4 E6：会话发现重测（t0 = 会话落库，终稿口径）

- 方法：t0 = `session_v2.time_created`（新 rowid 首次出现，50ms 轮询）；t1 = `GET /m/api/v1/sessions`（显示路径读同一快照）返回该 session id；Active 档由 `opencode run -c` 循环维持。
- 原始样本（ms）：`2019 / 816 / 898 / 1515 / 267` → 排序 `267/816/898/1515/2019`。
- **统计口径（09b95a3c：中位 + 最大值）**：中位 **898ms ≤2s 达标**；**最大值 2019ms（超线 19ms）**须如实申报——按「中位 ≤2s」门禁达标，按最大值看越线；该样本为边界样本（opencode CLI 启动/落库波动），本批未见扫描链异常证据。
- **上一轮 20s 超时归因（推断，非已证伪）**：已证部分——token 5 对应 `session_v2` rowid=5，`time_created=03:03:56.663Z`，会话正常落库；当时自动标题为 "Database indexing basics"（未保留 token），探测标记失效。**推断部分**——「探测标记失效导致 20s 超时、而非事件漏唤醒」：`mam.log.old` 同期只有档位切换行（03:01:02Z Away / 03:02:30Z、03:02:46Z Active / 03:22:42Z Away），每轮扫描明细已按降噪纪律移除，stats 仅保留最后一轮——**没有逐轮时间线能证明超时当时卡片已在板上**，无法排除其他链路因素，只能记为推断。
- 偏差：t0 取 DB 行写入时刻；REST 快照轮询 50ms 粒度；样本 0 的 2019ms 为边界样本（中位达标、最大值越线）。

#### 10.12.5 R11 清理

- R8 用例补 `try_recv` 空断言（补排只一条 Action）；
- `reset_for_test` 一并清 `TEST_TX`；
- 修复 `wait_event_defer` 拼断文档、握手用例 `CleanupStream` 旧注释；
- §10.4 的 `EVENT_DEBOUNCE`、§10.6/§10.8 的 `scan_now_blocking` 描述、§10.11.3/§10.11.4 的旧 F5/发现口径已按终稿纠偏。

#### 10.12.6 E7 工程门禁（HEAD b734a970）

- `cargo test --lib`：**3170 passed / 6 failed**（与 §10.11.5 同名的既有环境性失败：claude 布局、codex/opencode 时间窗、Windows 路径大小写、subagent 目录 ×2）。
- `cargo clippy --all-targets -j 2 -D warnings`：**0 告警**。
- `pnpm test`：**204 文件 / 2521 passed / 5 skipped**。
- `pnpm check`：**EXIT 0**（3 条既有 mobile warning）。
- 既有 tps 4 条失败不变（§10.11.5）。

#### 10.12.7 最终门禁表（终稿 e4320c1c；2026-10-31 按 09b95a3c 修订 F5 与发现口径）

| 门禁项 | 目标 | 结果 | 依据 |
| --- | --- | --- | --- |
| 挂机 CPU（主进程） | ≤3% | **达标 0.48%**（基线 17.15%） | §10.10 / §10.11.2 |
| 挂机 CPU（进程树含 WebView2） | **不劣于 0.6.3 基线** | **达标：3.52% vs 21.23%（-83.4%）**；绝对 ≤3% 列后续渲染优化项 | §10.11.2 |
| 远控空闲 CPU（main/tree） | ≤6% / 不劣于基线 | **达标 0.86% / 3.86%**（基线 16.55% / 20.64%） | §10.10 |
| 扫描频次（Active） | ≤30 次/分 | **达标 29.1**（29.1/29.0/29.2） | §10.11.3 |
| 扫描频次（Idle / Away） | ≤10 / ≤6 | **达标 6.0 / 2.0** | §10.10 |
| 活动档会话发现 | ≤2s（t0=会话落库；统计口径 = 中位，同时报最大值） | **中位 898ms 达标**；最大值 **2019ms（超线 19ms）** | §10.12.4 |
| F6 事件唤醒 | ≤5s | **达标 1369ms** | §10.11 |
| F7 用量不可见 / 可见 | ≤12 分钟 / ≤1s | **达标 12m01s / 317ms** | §10.10 |
| F8 进程刷新 | ≤25ms | **达标 17.5ms**（6 样本中位） | §10.11.4 |
| F5 未读态消失（普通未读卡标已读） | 点击后 ≤1s，卡面保留 | **达标（组件测试）**；无 App 形态工具在线，未做活体 | `tests/session/optimisticHide.test.tsx`（G1，5 用例） |
| F5 整卡消失（数据驱动聚合绿卡标已读 / 隐藏 / 归档） | 点击后 ≤1s | **达标**：组件测试 + 单 run 活体 **11ms**（opencode 隐藏路径） | 组件测试同上；活体 §10.13.3；codex/zcode/dsh App idle/finished 镜像后端 `green_card_is_data_driven` |
| F3 degraded 诊断窗口 | 超时后 30s 可观测 | **达标（单测）**：启动早于超时的发布不再清零 | §10.12.2 / §10.13.2（CAS 冲突用例） |
| 日志（扫描总线新增） | ≤1MB/天 | **达标 ≈30KB/天**；dsh 告警归 F9 | §10.11.6 |
| 工程门禁 | 全绿（既有环境失败除外） | **达标** | §10.12.6 / §10.13.4 |

---

### 10.13 第六轮最终微修（G1 / G2 / G3，2026-10-31 14:00–15:00 +0800）

#### 10.13.1 构建与环境

- 源码：HEAD `ea88833f`（G1 `b0289e0d` + G3 `ea88833f`；代码已在两提交入库，本节仅新增文档）。
- release：`pnpm build` + `CARGO_BUILD_JOBS=2 pnpm exec tauri build --no-bundle --features marker-helper`；编译 **9m29s**，产物 `src-tauri/target/release/tuvis.exe` 48,315,904 B，sha256 `e7d459a0f3ac98a1d140ea576225e8c90933bc83aeea0f01d22b77e4a29e4fb3`。
- 运行：真实 HOME；测量期间停掉安装版、以本产物启动（`WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9223`），CDP 选主窗 `http://tauri.localhost/`（排除宠物窗 `#/pet`）；测后已重启安装版（pid 30296）。
- 夹具：opencode CLI 会话 `ses_e6f64023affexAVw5UW7XCTCQV`（项目 `E:/LLMproject/f5-fixture`，`form=cli`，`status=idle`，`unread=false`）；本机无 codex/zcode/dsh/claude 等 App 形态工具进程，无远控客户端在线。

#### 10.13.2 G3：R10 CAS 冲突用例（单测）

- 把条件复位抽成可测的 `clear_timeout_after(started_ms, load, cas)`（publish 传入真实原子操作），新增 3 条用例：
  - `clear_timeout_keeps_newer_timeout_on_cas_conflict` → **ok**（CAS `Err(150)`：发布期间并发记录更新的超时，按最新值重判并停止写入，新 30s 窗口保留）；
  - `clear_timeout_stops_when_concurrent_publish_already_cleared` → **ok**（CAS `Err(0)`：并发发布已清零，收敛退出）；
  - `clear_timeout_clears_once_when_cas_succeeds` → **ok**（`Ok` 只清一次；窗口为 0 时一次 CAS 都不发）。
- `cargo test --lib scan_bus`：**26 passed / 0 failed**（含既有 R10 `publish_keeps_timeout_window_when_scan_started_before_it` / `publish_resets_degraded_timeout_window`）。

#### 10.13.3 F5 单 run 重录（release，真实点击，无跨 run 混用）

同一进程、同一 run 内依次采集：`scan_bus_status` → 点击 X（DOM 脱离计时）→ `scan_bus_status` → 同 run 直接调用同一命令取契约 → `scan_bus_status`。

```json
{
  "statusBefore": { "mode": "away", "version": 8, "degraded": false, "snapshotAgeMs": 17055 },
  "cardsBefore": [
    { "projectName": "f5-fixture", "unread": false, "xLabel": "暂不显示（会话状态变化后恢复）" }
  ],
  "session": {
    "id": "ses_e6f64023affexAVw5UW7XCTCQV",
    "agentType": "opencode",
    "status": "idle",
    "unread": false,
    "form": "cli",
    "projectName": "f5-fixture"
  },
  "action": {
    "kind": "dismiss",
    "expect": "gone",
    "timing": { "elapsedMs": 11 },
    "cardsAfterClick": [],
    "statusAfterClick": { "mode": "away", "version": 10, "degraded": false, "snapshotAgeMs": 148 },
    "contract": {
      "cmd": "dismiss_session_card",
      "args": {
        "agentType": "opencode",
        "sessionId": "ses_e6f64023affexAVw5UW7XCTCQV",
        "status": "idle"
      },
      "result": { "applied": true, "version": 11 }
    },
    "statusAfterContract": { "mode": "away", "version": 11, "degraded": false, "snapshotAgeMs": 2 }
  },
  "versionSequence": [8, 10, 11, 11],
  "versionMonotonic": true,
  "note": "single run: __TAURI_INTERNALS__.invoke is not interceptable for the UI's own call; applied/version come from a same-run direct call of the same command after the click (contract inference), never from another run."
}
```

- **整卡口径**：点击到卡片 DOM 脱离 **11ms**（≤1s 达标）；命令返回前卡片已消失（乐观隐藏），命令结束后 invalidate 回取权威快照。
- **version 单调**：`8 → 10 → 11 → 11`（点击的补扫把 8 推到 10，契约补扫推到 11；后读状态与契约同版或更新）。
- **mode 与运行状态一致**：全部样本 `away`——运行中无活跃会话（唯一 opencode 会话 `idle`）、无远控客户端在线，符合 Away 档定义；同一启动内的前置探测中，opencode 回合处理期间 `mode=active`、回合结束后 `mode=away`，档位跟随会话状态。
- **applied 为同 run 命令契约推断**：`__TAURI_INTERNALS__.invoke` 是 own-prop 且前端模块缓存引用，无法截获 UI 自身调用的回执；`applied=true/version=11` 来自点击后在同一页面、同一 run 直接调用 `dismiss_session_card` 的返回，**未跨 run 拼接**。
- **未读态口径未做活体**：`sync_unread_sessions` 仅 App 形态参与未读池；本机无 App 形态工具进程，无法产生普通未读卡（数据驱动绿卡同缺）。该口径以 G1 组件测试为准（§10.13.4），如实申报。
- 原 §10.12.3（11/12ms 混用两次运行）已标记作废。

#### 10.13.4 G1 组件验收与工程门禁

- 组件测试 `tests/session/optimisticHide.test.tsx`（5 passed）；Harness 与 home 同构（`useSyncExternalStore` 订阅 pending → `applyPendingSessions` → 卡片）：
  - codex App idle 未读卡：invoke 挂起时整卡 **≤1s 消失**（waitFor timeout=1000），无 invalidate；
  - claude 普通未读卡：invoke 挂起时卡面保留、`Unread` 标记消失；
  - 轮询回写旧快照（`setQueryData` 旧数据）：整卡不复活 / 普通未读卡未读态不复活；
  - 命令失败：invalidate 回取权威快照后卡片**回滚可见**。
- 单元测试 `tests/scanBus/optimisticSessions.test.ts`（7 passed）：数据驱动规则（codex/zcode/dsh + App + idle/finished）、非未读整卡移除、普通未读仅置 `unread=false`、pending hidden/read 防回写、批量移除、缓存缺失空操作。
- 门禁：
  - `pnpm test`：**204 文件 / 2527 passed / 5 skipped**；
  - `pnpm check`：**EXIT 0**（既有 mobile chunk warning 不变）；
  - `cargo test --lib scan_bus`：**26 passed**；
  - `cargo clippy --all-targets -j 2 -- -D warnings`：**0 告警**。

#### 10.13.5 偏差与剩余未测

- 普通未读卡（未读态口径）与数据驱动绿卡（codex/zcode/dsh）**未做 release 活体**：本机无 App 形态工具进程，无法构造对应未读卡；以组件测试覆盖（§10.13.4）。
- F5 活体覆盖的是 **隐藏（dismiss）路径**；`applied` 为同 run 契约推断（理由见 §10.13.3）；一键清除批量路径未做活体（单测覆盖批量乐观移除）。
- 会话发现最大值 2019ms 超 2s 线 19ms（终稿门禁按中位判定，最大值如实申报，§10.12.4）。
- 20s 超时归因降级为「推断」（§10.12.4）：已有证据仅证明标题未保留 token 与会话已落库，无逐轮扫描时间线。
