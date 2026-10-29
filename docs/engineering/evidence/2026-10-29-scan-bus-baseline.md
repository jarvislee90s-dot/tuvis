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

- 计数源：日志行 **`adapter] OpenCode:`**。每轮全量扫描每个适配器各打一行，取 OpenCode 一行即一轮。
- **依赖 INFO 级别**。Task 12 Step 1 会把每轮 INFO 降为 debug（F9）；复测前必须临时把日志级别调回 info（计划内注明可用 `RUST_LOG=info`），或改用 `scan_bus` 的档位/版本日志计数。**不改级别直接跑会得到 `scans=0` 的假低值。**
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

1. 确认应用在跑、日志级别为 info（见 §7），用 `-Seconds 300` 跑 5 分钟；
2. 建议同一状态跑两次（F10 要求同状态两次差异 ≤10%），取中位或两次都记录；
3. 同时记录 `(Get-Process).Count`、当前档位（挂机 / 远控在线空闲 / 有活跃会话）；
4. 把脚本输出**原样**追加到本文件，并补一张「基线 vs 改造后」对照表。
