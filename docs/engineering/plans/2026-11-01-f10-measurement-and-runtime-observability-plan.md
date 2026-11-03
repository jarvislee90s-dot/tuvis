# F10 测量脚本与运行可观测性实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox syntax for tracking.

**Goal:** 修复进程树 CPU 测量错误，增加扫描运行累计指标与身份校验，并交付逐项验收矩阵。

**Architecture:** Scan Bus 原子计数与有限 mutation latency 窗口汇入 `runtime` 结构，同时被 Tauri 状态命令和 stats 文件使用。stats 通过同目录临时文件 rename 原子发布。PowerShell 以 PID+创建时刻对齐每个进程的 CPU 增量，树集合不稳定就废弃重测，耗时只读 Stopwatch。

**Tech Stack:** Rust atomics/Mutex/Condvar、serde_json、PowerShell 5.1+、Win32 CIM、.NET Stopwatch。

---

## 任务 1：Scan Bus 运行计数与失败态

**文件：**

- 修改 `src-tauri/src/adapter/scan_bus.rs`
- 修改 `src-tauri/src/commands/session.rs`

- [x] **Step 1：增加 runtime payload 失败测试**

断言 runtime 至少包含 `runId/processId/target/lastDurationMs/actionScans/eventScans/periodicScans/consecutiveFailures/retryBackoffRemainingMs/workerRestarts/mutationWaitP95Ms`；初始值计数为 0、无退避、p95 样本数为 0。reason 累加函数分别记录 Action/Event/Periodic/ColdStart。

- [x] **Step 2：补计数与 round 状态**

在扫描准入成功点递增对应 reason counter。worker round 返回无新发布或 panic 时递增 consecutive failures，产生新快照时清零；panic 睡眠期间设置单调 retry deadline，醒来/新代接管清零。每次 publish 更新 last duration；`WORKER_RESTARTS` 仍只在 supervisor 通道重建时增加。

- [x] **Step 3：记录 mutation wait p95**

对 `scan_after_mutation` 的有界等待用固定容量 128 的 `VecDeque<u64>` 记毫秒，按 nearest-rank 计算 p95；容量满时丢弃最老样本。命名和注释明确它只测后端补扫等待，不把它等同 UI/IPC 完整往返。

- [x] **Step 4：接入 status 与 stats JSON**

`scan_bus_status()` 添加嵌套 runtime 对象；`stats_json` 添加相同对象。更新命令/JSON 测试，旧字段保持。

## 任务 2：stats 原子写与覆盖测试

**文件：**`src-tauri/src/adapter/scan_bus.rs`。

- [x] **Step 1：先写覆盖写回归**

连续写入两份不同 JSON 后读取者必须只见第二份合法完整 JSON，旧 key/内容不残留；目录中不留 temp file。

- [x] **Step 2：实现同目录临时文件发布**

以 PID+原子序号生成唯一临时名，`create_new` 后写完整 payload 并 `sync_all`；关闭文件后 `rename(temp,path)` 替换。失败时删除 temp 并 warn。scan 与 runtime 更新共用发布锁，避免旧 snapshot 元数据覆盖新 stats。

## 任务 3：修复测量脚本并增加自测

**文件：**`scripts/measure-scan-bus.ps1`。

- [x] **Step 1：写 PowerShell 自测分支**

`-SelfTest` 使用合成快照验证：稳定 PID/CreationDate 集与单调 CPU 计数可计算增量；子进程增删、PID creation 变化、任一 CPU 回退均拒绝样本。

- [x] **Step 2：实现身份稳定的采样**

递归采集 root + `msedgewebview2*` 子进程的 PID/CreationDate/CPU 秒；before/after 键集合相等后逐 PID 求差再合计。整棵树有变化或回退时丢弃当前轮并在 `MaxRetries` 内重试。

- [x] **Step 3：加时间/目标/统计防护**

用 `[System.Diagnostics.Stopwatch]` 算 elapsed；检查 `Get-Process.Path` 与 stats `processId/runId/target` 对应；`Read-ScanStats` 最多 5 次短间隔重试；任一不符、进程退出或次数门禁失败时报错。

- [x] **Step 4：支持 120s 预热和三轮中位数**

默认预热 120 秒、每轮采样 300 秒、3 轮，报告 root CPU / 进程树 CPU / scansPerMin / runtime reason counts / mutation wait p95；`CpuOnly` 仍支持旧版 baseline。稳定性差异>10% 时只标记样本不稳定，不假称通过；少于两轮时重复性未测并标未定义；中位数为零时，混合样本标不稳定，全零标未定义且不算通过。

- [x] **Step 5：运行脚本自测**

运行 `powershell -NoProfile -File scripts/measure-scan-bus.ps1 -SelfTest`。该模式不得启动应用、读用户 stats 或采集真实会话。

## 任务 4：验收矩阵与最终门禁

**文件：**`docs/engineering/acceptance/2026-11-01-scan-bus-remediation.md`。

- [x] **Step 1：登记每组实现证据与实机待项**

将 13 组按 F1–F10 映射到 commits、定向测试和脚本；历史 release 数据只列历史基线。当前 release CPU 三档、20 次新会话手机发送、多 WebView、30 分钟事件、用量滞后、三天账本等未实际跑过时标「待实机」，不标通过。

- [x] **Step 2：整合自动化质量门**

运行 `cargo fmt -- --check`、`cargo clippy --lib -- -D warnings`、`cargo test adapter::scan_bus::tests --lib`、`cargo test services::usage:: --lib`、`cargo test monitor::dsh:: --lib`、`cargo test adapter::candidate_whitelist_tests --lib`、`pnpm check` 与相关 Vitest 定向用例。

- [x] **Step 3：提交**

分开提交 runtime/status、atomic stats、PowerShell 与验收矩阵，保持每个提交可独立审查。

## 规格覆盖核对

- stats 原子写、有限读取重试、运行身份校验：任务 2、3。
- PID+创建时间的树差值、样本变化作废、单调 wall：任务 3。
- reason 累计、失败/退避/重启/最近耗时：任务 1。
- action p95 口径、最终 F1–F10 证据矩阵：任务 1、4。
