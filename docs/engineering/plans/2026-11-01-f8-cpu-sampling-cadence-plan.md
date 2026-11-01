# F8 进程 CPU 状态与卡面采样频率实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox syntax for tracking.

**Goal:** 保留 OpenClaw 每扫描轮的 CPU 状态判定，同时将卡面 CPU 展示采样降为每 30 秒一次。

**Architecture:** 两段式进程刷新继续每轮发现候选并刷新 cmd/cwd/exe；CPU 刷新计划按本轮 OpenClaw PID 与 30 秒展示截止计算。每 PID 首次读取 sysinfo `accumulated_cpu_time()` 只建立基线，后续累计 CPU 毫秒增量除以同一 PID 的单调墙钟间隔得到 CPU 百分比；卡片数值另按 30 秒 cadence 发布。此独立分母同时避开 Windows 启动差分误值和 macOS 共享 System 时钟错窗。全量/两段式活体对照改为比较 PID、cwd、exe、form。

**Tech Stack:** Rust、sysinfo 0.39、`Instant`、现有 adapter/process parser。

---

## 任务 1：先锁定 CPU 刷新计划与进程身份字段

**文件：**`src-tauri/src/adapter/mod.rs` 测试模块、`src-tauri/src/monitor/openclaw_parser.rs`。

- [ ] **Step 1：增加失败用例**

为纯函数编写用例：新 PID 首次累计读数只建立基线，下一次有效增量才推进展示 deadline；未到 `sysinfo::MINIMUM_CPU_UPDATE_INTERVAL` 时不重复采样。再验证 OpenClaw 仍在每次合格扫描中采样，非 OpenClaw 在 30 秒内不发布新展示值、恰好 30 秒发布。空候选、0/部分 updated count 不推进锚点。增加 `AgentProcess` 身份投影用例，分别改变 PID/cwd/exe/form 时投影必须不同；增加多个 PID 使用不同采样窗口仍分别正确计算的用例。

- [ ] **Step 2：运行确认失败**

运行 `cargo test adapter::candidate_whitelist_tests::cpu_ --lib` 与 `cargo test adapter::candidate_whitelist_tests::process_identity --lib`；预期新函数/字段投影未定义而失败。

- [ ] **Step 3：实现 CPU 刷新计划纯函数**

定义 `CpuSamplingState`：以 `(PID,start_time)` 跟踪每个候选的成功样本数和最近请求时刻，并保存 `last_display_sample_at`。纯 helper 输入候选、OpenClaw PID、状态、当前 `Instant`，输出符合 sysinfo 最小间隔的 CPU 刷新目标及展示 deadline 状态。

## 任务 2：生产刷新接线与兼容性对照

**文件：**`src-tauri/src/adapter/mod.rs`。

- [ ] **Step 1：拆开候选字段与 CPU 字段刷新**

第二段候选刷新保留 `.with_cmd/.with_cwd/.with_exe`，移除其中的 `.with_cpu()`。随后对纯函数返回的 PID 集单独调用 `refresh_processes_specifics(..., ProcessRefreshKind::nothing().with_cpu())`。OpenClaw PID 在达到 sysinfo 最小间隔的扫描中刷新；新 PID 两次样本间隔不足时不重复请求；展示到期时全候选一次覆盖。

- [ ] **Step 2：保存展示样本单调时刻**

新增进程级累计 CPU 基线、单调采样时间、当前状态值与独立卡片展示缓存。共享 `System` 重建时清空状态；展示 deadline 只在候选非空、每个当前 PID 都已建立有效差分且 sysinfo 更新数覆盖全部目标时推进。Windows 新 PID/System 重建通过真实 sysinfo 累计 CPU 时间验证；macOS 与 Windows 一样按 PID 使用匹配的 CPU/墙钟增量，不依赖共享 CPU 分母。OpenClaw PID 状态采样与卡片展示发布分开判定。

- [ ] **Step 3：加强活体兼容性对照**

`two_stage_refresh_preserves_all_adapter_discovery` 对每个仍存活的全量发现 PID 对照完整身份元组 `(pid,cwd,exe,form)`。若两边均无运行工具，输出「活体兼容性未覆盖」并依赖身份投影合成用例，不把空列表当作兼容通过证据。

## 任务 3：验证

- [ ] **Step 1：运行定向测试**

运行 `cargo test adapter::candidate_whitelist_tests --lib` 与 `cargo test monitor::openclaw_parser::cpu_status_tests --lib`。候选测试包括 Windows 真实 sysinfo 累计时间、新 PID/System 重建行为和不同 PID 采样窗口；后者锁定 OpenClaw `cpu_usage > 5%` 为 Processing、`≤5%` 为 Idle。

- [ ] **Step 2：质量门**

运行 `cargo fmt -- --check`、`cargo clippy --all-targets -- -D warnings`、`git diff --check`。

- [ ] **Step 3：提交**

仅暂存 `src-tauri/src/adapter/mod.rs`、`src-tauri/src/monitor/openclaw_parser.rs` 与本计划内测试，提交 `perf(scan): reduce display cpu sampling frequency`。

## 規格覆盖核对

- OpenClaw CPU 状态刷新每轮保留：任务 1、2。
- 其他候选卡面 CPU 30 秒采样、单调时间首轮及边界：任务 1、2。
- PID/cwd/exe/form 对照与零进程不作兼容证据：任务 2。
- 候选发现、cmd/cwd/exe 更新与现有扫描入口保持：任务 2。
