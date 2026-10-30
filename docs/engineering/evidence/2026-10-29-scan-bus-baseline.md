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

- 计数源：日志行 **`adapter] OpenCode:`**。每轮全量扫描每个 adapter 各打一行，取 OpenCode 一行即一轮。
- **计数行为 DEBUG 级**：Task 12 已把每轮 `adapter] OpenCode:` 由 INFO 降为 DEBUG（F9）。复测必须设 `RUST_LOG=debug`，或更窄的 `RUST_LOG=info,multi_agents_manager_lib::adapter=debug`（只放开 adapter 模块的 DEBUG，其余模块维持 info，避免日志体积失控）；`RUST_LOG=info` **不会**放出该行，直接跑会得到 `scans=0` 的假低值（见 §10.8 第 7 条）。
- 只回看日志尾部 20,000 行（显式按 **UTF-8** 解码读取）。PS 5.1 默认按 ANSI（GB2312）读会把多字节中文行合并：整文件计数仅 **23,534** 条 vs 物理 **26,152** 行；尾读窗口也会位移——同样 20,000 条在 ANSI 下覆盖 **2,229** 条 OpenCode 行，UTF-8 下为 **2,006** 条。当前日志 26,152 行 / 2,857,215 字节；每轮约 **10 行**日志，按 56–61 轮/分 ≈ **533 行/分**，20,000 行窗口仍覆盖约 **37 分钟**，5 分钟复测不会截断。
- 时间过滤按日志 UTC 前缀 `[yyyy-MM-ddTHH:mm:ss` 做字符串比较，要求日志时间戳为 UTC 且以 `[` 开头（当前格式满足）。
- 脚本统计的是「全量扫描轮数」，不区分发起方；改造后应由扫描总线单一来源产出。
- 脚本只测 **tuvis 主进程**的 CPU，不含 WebView2 子进程；前端轮询等部分开销落在 WebView2 上。因此前后对照只有在**同一构建、同一 UI 状态**下才严格可比。

## 8. 门禁目标（Task 12 验收）

| 场景 | CPU 门禁 | 扫描频次门禁 |
| --- | --- | --- |
| 挂机（Away） | ≤3% 单核 | ≤6 次/分 |
| 远控在线但无活跃会话（Idle） | ≤6% 单核 | ≤10 次/分 |
| 有活跃会话（Active） | — | ≤30 次/分 |

来源：需求 §1.2 量化目标、§4 非功能门禁汇总、F2 验收条件、实施计划 Task 12 Step 3。另：活动档会话发现 ≤2s、动作回执 p95 不劣于基线、用量不可见时滞后 ≤10 分钟。

## 9. Task 12 复测约定

1. 确认应用在跑、日志级别为 debug（`RUST_LOG=debug` 或 `RUST_LOG=info,multi_agents_manager_lib::adapter=debug`，见 §7），用 `-Seconds 300` 跑 5 分钟；
2. 建议同一状态跑两次（F10 要求同状态两次差异 ≤10%），取中位或两次都记录；
3. 同时记录 `(Get-Process).Count`、当前档位（挂机 / 远控在线空闲 / 有活跃会话）；
4. 把脚本输出**原样**追加到本文件，并补一张「基线 vs 改造后」对照表。

---

## 10. Task 12 改造后复测记录（2026-10-30）

本节是改造后的记账：**实时 5 分钟采样已完成**（经用户裁决关闭安装版后用影子数据目录运行本分支 debug 构建，方法与原始输出见 §10.2）；测不了的项一律写「未测」，不推算、不用代码推定值冒充实测值。

### 10.1 测点

- 时间：文档落笔 **2026-10-30 15:16 +0800**；活体复测窗口 **15:36–15:49 +0800**（2026-10-30T07:36–07:49Z）
- 分支：`design/scan-bus-and-adaptive-cadence`
- HEAD：Task 12 提交（`chore(scan): quiet per-round logs and record post-change measurements`）；复测二进制由该提交源码构建（`ebce4677` 之后的同一源码树，仅本证据文件在测后追加）
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
- 合并节流：距上轮扫描 **<500ms** 的**孤立** Event 被吞（`scan_bus::EVENT_DEBOUNCE`）；Action 补扫、冷启动与「同批积压含非 Event」不受此节流限制。
- 档位跃迁日志 `scan_bus: mode → {:?}` 为 INFO，**只在跃迁时打一行**，不能用来逐轮计数。
- 实测（§10.2）：Active **29.2/min**（300s / 146 次）、Away **2.0/min**（300s / 10 次）；Idle 未启用远控未测（代码推定 6/min）。事件唤醒会叠加，实际次数取决于本机事件密度。
- 扫描来源已单一化：命令层只读共享快照，托盘 / 看板 / 桌宠 / 远控 watcher 不再各自触发（对照基线 §3「扫描发起方」一行）。

### 10.5 用量采集（F7）

- 可见即采：`useUsageCollect` 在 active 的 false→true 边沿（含挂载即 true）发 `force=true`；前端 **60s** 强制去抖记在**模块作用域**（`lastForcedCollectAt`，同一 WebView 所有实例共享），防止宠物迷你条反复挂载卸载绕过最小间隔。
- 后端兜底：`ticker.rs::step_ms(interval_min) = interval_min × 60 000 + 120 000`，默认 `collect_interval_min = 10` → **步长 12 分钟**。
- **口径冲突（须裁决，本轮不改）**：需求 F7 的技术方向写「每 10 分钟（步长 = 设置间隔 + 2 分钟缓冲）」，同一节验收条件却写「整天停留在其他页：数据滞后 **≤10 分钟**」。按默认设置间隔 10 分钟算，步长 12 分钟 > 10 分钟，**两条不能同时成立**。本轮按「步长 = 间隔 + 2min」实现（Task 8），故验收第 2 条在默认设置下**设计上不达标**。可选收口：① 把验收口径改成「≤ 设置间隔 + 2 分钟」（默认即 ≤12 分钟，与 F7 技术方向自洽）；② 把默认采集间隔从 10 分钟调成 8 分钟（步长恰为 10 分钟，既满足 ≤10 分钟验收，也保住 +2min 缓冲与「相邻两次间隔 ≥ 设置值」）；③ 给步长硬顶 `min(间隔 + 2min, 10min)` —— **不可取**：间隔 > 8 分钟时步长会小于设置间隔，违反同节验收「兜底采集不会隔轮空跑（相邻两次间隔 ≥ 设置值）」。**需用户在合入前裁决。**
- 验收注意：debug 构建**默认不启动** ticker（需 `TUVIS_USAGE_TICKER=1`，见 `ticker.rs` 启动日志），拿 debug 包做「不可见 ≤10min」验收会必然假失败，须用 release 包或显式开环境变量。

### 10.6 门禁表（改造后）

| 门禁项 | 目标 | 结果 | 依据 / 备注 |
| --- | --- | --- | --- |
| 挂机（Away）CPU | ≤3% 单核 | **达标（debug 0.37%）** | §10.2 原始输出 `avgCPU=0.37%`；debug 上界，release 未复测 |
| 远控在线空闲（Idle）CPU | ≤6% 单核 | **未测** | 未启用远控；参考 Active debug 2.69% ≤6% |
| 扫描 ≤30 次/分（有活跃会话 / Active 2s） | ≤30 次/分 | **达标（debug 29.2/min）** | §10.2 `scansPerMin=29.2` |
| 扫描 ≤10 次/分（远控在线但空闲 / Idle 10s） | ≤10 次/分 | **未测**；代码推定 6/min | 未启用远控 |
| 扫描 ≤6 次/分（挂机 / Away 30s） | ≤6 次/分 | **达标（debug 2.0/min）** | §10.2 `scansPerMin=2.0` |
| 活动档会话发现 | ≤2s | **未测**；代码路径 = Active 2s 档 + 1s 事件唤醒 | `scan_bus.rs` / `wakeup.rs` |
| 用量可见 | ≤1s 看到最新数据 | **未测**；代码路径 = 激活边沿 `force` 采集 + 成功后失效整个用量域 | `src/lib/query/queries/usage.ts` |
| 用量不可见 | 滞后 ≤10 分钟 | **未达标（按设计）**：默认步长 12 分钟 | §10.5 口径冲突 |
| 日志体积 | ≤1MB/天 | **未测**（每轮 adapter 日志已降为 DEBUG；实测待运行） | §10.8 第 7 条 |
| 兼容性 | IPC 契约不变，旧前端可继续工作 | **达标**（静态核对：新增命令为增量；`scan_bus_watch` 仅新增可选 `source`，缺省 `main`，旧前端调用不变） | `commands/session.rs` / `lib.rs` |
| 单轮进程刷新 | ≤25ms（5 次取中位） | **未达标（debug 中位 29.6ms）**；release 未测 | §10.3 热路径实测 |
| 动作回执 p95 | 不劣于基线 | **未测** | 无活体压测 |

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
2. **F3 降级只有日志**：`ensure_fresh` 超时后按陈旧快照继续并在 `remote/mod.rs` 打告警（`remote: 动作路径快照补扫超时…按 degraded 继续`），但 **API 响应里没有 `degraded` 字段**，前端无从区分「新鲜」与「降级」；API 契约留待后续。
3. **F6 覆盖边界**：claude / codex / kimi 有 hook 事件；opencode 由 `opencode.db` + `-wal` 与 facts 文件覆盖；**openclaw 既无 hook 事件、也没有便宜的文件信号**（`wakeup.rs` 未覆盖），与 dsh 同理 → 二者仍靠档位兜底，Away 档下最坏 30s 才看到变化（`wakeup.rs` 模块文档已申报）。
4. **F5 残余窗口**：`dismiss_session_card` 等命令是「即发即忘」补扫，前端可能在新快照发布前多读到一帧旧快照（卡片多显示一帧）；注释里已标注留待「版本门控失效」收口。
5. **看门狗退避 500ms**：panic 后 `sleep(500ms)` 再按当前档位 `recv_timeout`，最坏恢复 = 档位间隔 + 0.5s；Away 档即 **30.5s，略超 F1 的「≤30s 自动恢复」**（超 0.5s，可接受但需记账）。
6. **`POLL_INTERVAL` 已成孤儿常量**：`src/config/constants.ts` 仍导出 `POLL_INTERVAL = 3000`，但已无代码消费者（仅 `commands/usage.rs` 的源码锁按文本扫描面匹配该字面量）；留着会让人误以为前端仍在 3s 轮询。
7. **日志降噪的副作用（口径变更）**：§7 的扫描计数源 `adapter] OpenCode:` 现在是 **debug** 级 —— `RUST_LOG=info` **不会**把它恢复（env_logger 默认 `info`，见 `lib.rs`）。后续复测必须用 `RUST_LOG=debug`，或更窄的 `RUST_LOG=info,multi_agents_manager_lib::adapter=debug`（只放开 adapter 模块，其余模块维持 info）；也可改用另一条每轮 DEBUG 行 `scan: process refresh（两段式）耗时` 计数；`scan_bus: mode →` 只在跃迁时打，**不能**逐轮计数。实施计划「计划外但必须遵守」里写的「先临时设 `RUST_LOG=info`」在此更正为 `RUST_LOG=debug`（或上述模块级过滤）。
8. **热路径耗时（已观测，未达标）**：`scan: process refresh（两段式）耗时` DEBUG 行在 §10.2 的 300s 运行中共 215 拍；Active 窗口中位 29.6ms（p95 65.9ms），末 100 拍中位 42.9ms。debug 构建下 F8 的 ≤25ms **未达标**；release 未测（release 无法用影子目录，跑真实账本不在授权内）。
