# F8 进程 CPU 状态与卡面采样频率

## 背景

`refresh_processes_two_stage` 已将命令行、cwd、exe 限定到候选 PID，但每轮仍对全部候选进程 `.with_cpu()`。会话卡面的 CPU 数字只用于展示；OpenClaw 解析器却用 CPU 值 `>5%` 判定 Processing，不能一并停采。sysinfo 首次 `cpu_usage()` 可能只建立内部基线，且 macOS 的差分分母来自共享 System 刷新窗口，因此改用公开的累计 CPU 时间和每 PID 自己的单调采样窗口计算百分比。

## 设计

### 每轮状态采样

- 两段式第一段每轮只刷新进程名；候选 PID 的 cmd/cwd/exe 仍每轮 `Always` 刷新，保留 PID 复用与工具形态判断正确性。
- 每轮从候选集识别 OpenClaw PID，并在距该 PID 上次 CPU 请求达到 `sysinfo::MINIMUM_CPU_UPDATE_INTERVAL` 时读取累计 CPU 时间。更短的 Action 补扫沿用上个样本。OpenClaw 状态解析读取按该 PID 累计 CPU 毫秒增量和同一段单调墙钟时间计算的新值；卡片展示读取独立缓存，二者不互相改变采样/发布节奏。
- 新 PID 使用 `(PID, start_time)` 跟踪。首次累计 CPU 时间只建立基线，不作为有效 CPU 样本；后续同 PID 读数与墙钟间隔配对形成有效样本。PID 复用或共享 `System` 重建会清空对应基线，必须重新建立。

### 卡面展示采样

- 卡片展示值每 30 秒发布一次；进程 CPU 数字沿用最近一次合格展示样本。新 PID 首个累计读数只作为基线，第二个读数才可计算 CPU 百分比。
- 各 PID 使用自己的累计 CPU 增量与单调墙钟采样窗口。由此可按需只刷新 OpenClaw 状态进程或到期展示进程；不依赖 sysinfo 的共享 CPU 分母，也不把不同 PID 的刷新时刻混用。
- 命令行、cwd、exe 与 PID 枚举节奏不变。共享 `System` 重建时清空所有累计时间基线。

### 兼容性比较

全量刷新与两段式比较必须逐个对齐进程身份：PID、cwd、exe、ProcessForm。仅 PID/名字相等不足以证明 CLI/App 归类与项目路径配对不变。若本机没有运行任何工具，该次活体比较明确标记为「未覆盖」，不报告兼容通过；身份投影另有合成用例锁定全部字段。

## 验收

1. OpenClaw 在扫描间隔达到 sysinfo 最小 CPU 更新间隔时刷新；短间隔补扫复用最近有效值。新 PID 首次读取只建立累计 CPU 基线，下一次读数按同 PID 的墙钟窗口形成百分比。
2. 空候选、CPU 刷新返回 0、部分 PID 未更新都不得推进 30 秒展示样本锚点；共享 System 重建后重新初始化全部 CPU 基线。Windows 使用真实 sysinfo/System 测试覆盖新 PID 与 System 重建；纯数据用例覆盖不同 PID 的独立采样窗口。
3. 展示 CPU 采样边界使用单调时间，`t=29.999s` 不刷新，`t=30s` 刷新。
4. 进程兼容性对照覆盖 PID/cwd/exe/form，并对两边为空明确标记未覆盖。
5. 所有适配器候选发现与 OpenClaw 的 CPU 状态判定保持原行为；不触碰真实 usage ledger。

## 修改范围

- `src-tauri/src/adapter/mod.rs`：CPU 刷新状态、候选 PID 计划、共享 System 生命周期重置与身份对照。
- `src-tauri/src/monitor/openclaw_parser.rs`：锁定既有 5% Processing 判据。
