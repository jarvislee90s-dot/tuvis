# F10 测量脚本、运行指标与验收矩阵

## 问题

当前 `measure-scan-bus.ps1` 分别求采样前后“当前存活进程树”的累计 CPU 秒再相减。WebView2 子进程若在区间内退出，它启动以来的累计时间会从 after 集合消失，导致总 CPU 差为负、平均 CPU 可被压低并假通过。脚本也没有将统计文件绑定到进程 PID/创建身份/目标路径；统计 JSON 直接覆盖写，读取侧无重试。

运行诊断目前只有最近 reason/耗时和总扫描数，不能回答 Action/Event/Periodic 的累计次数、worker 连续失败、重试退避和重启次数。最终验收表也需要区分已有历史基线、当前自动化证据与尚需真实应用交互的项目。

## 设计

### 运行身份与指标

`scan_bus_status` 与 `scan_bus_stats.json` 共用一份 `runtime` 对象：

- `runId`、`processId`、`target` 标识本次运行身份；
- 实际启动的 `actionScans`、`eventScans`、`periodicScans`、`coldStartScans`；
- 最近真实扫描耗时、worker 连续失败数、重试退避剩余毫秒、worker 重启数；
- 最近 128 个 `scan_after_mutation` 等待时长的 `mutationWaitP95Ms` 与样本数。该字段只表示**后端变更后补扫等待**，不代表完整的 UI/IPC 往返 p95。

reason 计数在单飞扫描成功取得准入时累加，不以最后一轮 reason 推测；worker round panic/未产生新发布快照记为连续失败，成功发布后清零；worker 进入 500ms 退避时记录单调截止时间，退避结束或新代接管时清零。重启计数只统计 supervisor 接管，不把首次启动算作重启。

统计文件继续以同路径、单行 JSON 发布；先写唯一同目录临时文件，再 rename 替换。mutation wait 新样本收集后也刷新 runtime 区，以便测量读取到最新 p95。

### PowerShell 测量

- 进程身份以 `PID + CreationDate` 组合；根进程 PID、目标 exe 路径、stats `runId/processId/target` 在采样前后必须一致。
- `-Tree` 进程树在两端分别枚举，但只在 PID/创建时刻键集合完全相同、每个 CPU 秒计数不回退时计算各进程增量之和；集合变化、PID 复用或 CPU 回退使本次样本无效并重试，绝不将退出进程的历史 CPU 从总量扣掉。
- 所有耗时使用 `.NET Stopwatch` 单调计时。stats 读取最多重试 5 次、间隔 50ms；仍为空/半份/身份不匹配则报错。
- 默认预热 120s、采样 300s 共 3 轮，报告各轮和中位数；稳定性按多轮 CPU 与扫描率相对中位数差计算。显式少于两轮时重复性未测，稳定性标未定义；中位数为零且样本混合零/非零时标不稳定；全零时稳定性也未定义，不能标通过。`-SelfTest` 只跑合成进程树快照与稳定性用例，不连接应用或系统进程。
- `-CpuOnly` 仍可用于旧版没有 runtime stats 的 CPU 历史基线，但不会伪报扫描计数或身份对账通过。

### 验收矩阵

新增 `docs/engineering/acceptance/2026-11-01-scan-bus-remediation.md`：每项记录范围、证据、结果和缺失前置。历史版本数字只放“历史基线”，不混作当前构建通过。脚本/单测能证明的标为自动化；需要真实多窗口、远控动作、持续运行、三天账本或 release 进程树的项目单列实机证据状态，不以代码审查替代。

## 验收

1. 进程树任一新增/退出/身份变化/CPU 计数回退都会使该轮样本无效并重测；不产生负 CPU 或平均值。
2. Stopwatch 决定 wall time；stats 有界重试且严格核对 PID、runId 和目标 exe。
3. 同目录 rename 保证读者不见空 JSON 或半份 JSON。
4. status 与 stats 包含累计 reason、失败、退避、重启及最近耗时；mutation wait p95 标签/口径明确。
5. `measure-scan-bus.ps1 -SelfTest` 覆盖稳定树、child set 变化、PID creation 变化、CPU 回退。
6. 验收矩阵逐项区分历史基线、自动化和当前构建实机证据，不对未运行的场景宣称通过。

## 修改范围

- `src-tauri/src/adapter/scan_bus.rs` 与 `src-tauri/src/commands/session.rs`：累计计数、失败/退避/重启/耗时和状态载荷。
- `scripts/measure-scan-bus.ps1`：稳定身份采样、重试、单调时钟、多轮中位数与自测。
- `docs/engineering/acceptance/2026-11-01-scan-bus-remediation.md`：最终矩阵。

不新增遥测、网络上报或扫描线程；不写 `docs/MASTER-PLAN.md`。
