# 扫描总线改造验收矩阵

- 日期：2026-11-01
- 基线版本：v0.6.3；历史记录见 [扫描总线基线测量](../evidence/2026-10-29-scan-bus-baseline.md)
- 当前代码分支：`design/scan-bus-and-adaptive-cadence`
- 口径：本表区分代码/自动化验证与当前构建的实机证据。历史测量不计入改造后通过；实机项未运行即保持待验。

## 1. 整改组覆盖

| 组 | 整改范围 | 代码/文档证据 | 当前自动化证据 | 当前构建实机状态 |
| --- | --- | --- | --- | --- |
| 1 | F1 worker 执行权、panic 恢复与共享 System 锁重建 | `9d4dc6cc`；设计见 `2026-11-01-f1-worker-ownership-and-system-lock-recovery.md` | 通过：Scan Bus 定向套件 66 条覆盖执行权、panic 恢复与接管 | 待实机：真实 panic/线程退出恢复、旧代不能继续执行 |
| 2 | F1/F3 冷启动读取与动作期限分流 | `e5e06c49`；设计见 `2026-11-01-f1-f3-initial-read-and-action-deadline.md` | 通过：Scan Bus 定向套件 66 条覆盖首扫屏障、无 sender、慢扫与动作 deadline | 待实机：慢扫描下首屏不误报空、degraded 动作响应 |
| 3 | F2 在线 TTL 使用单调时钟并由 REST 上升沿唤醒 | `5e8a6b3e`；计划见 `2026-11-01-f2-remote-online-monotonic-wakeup-plan.md` | 通过：Rust remote/Scan Bus 定向覆盖单调时间与 REST 合并唤醒 | 待实机：断开 SSE 后由真实 REST 首次认证即时升档 |
| 4 | F2/F4 轮询、事件与变更结算双向版本仲裁 | `f3e672ae`、`c03fc4c7`；设计见 `2026-11-01-f2-f4-r3-bidirectional-snapshot-arbitration.md` | 通过：快照仲裁/同步、变更结算及 `useSessions` 新 IPC mock 定向用例通过 | 待实机：主窗、宠物 WebView 和手机 SSE/REST 重连 |
| 5 | F5 重叠变更等待器按目标结算 | `643ac84b`、`be99b19b`；计划见 `2026-11-01-f5-mutation-settlement-target-ownership-plan.md` | 通过：Scan Bus 定向套件覆盖部分交叠、回执顺序与超时结算 | 待实机：普通未读、聚合绿卡、单卡和批量组合 |
| 6 | F5 宠物关闭、跳转已读与手机归档接入保护链 | `728f9b91`、`8331ac71`；计划见 `2026-11-01-f5-action-entrypoint-settlement-plan.md` | 通过：前端/后端定向测试涵盖旧快照、跳转慢扫、手机旧响应和回滚 | 待实机：真实宠物卡片、跳转和手机归档全链路 |
| 7 | F6 按工具运行时可用性判定事件覆盖 | `4734bf4f`、`ba7d59d5`；设计见 `2026-11-01-f6-runtime-event-coverage.md` | 通过：Scan Bus 66 条含 WorkBuddy/ZCode 覆盖；wakeup 29 条含首拍启动对账；dsh 48 条通过 | 待实机：至少 30 分钟真实事件零丢失、helper 缺席及清理边界 |
| 8 | F6 R-1 陈旧事件安全隔离清理 | `e883c852`、`4a0406a3`；设计见 `2026-11-01-f6-r1-quarantine-cleanup.md` | 通过：Scan Bus 定向套件覆盖并发创建、同路径替换、相等 mtime 与纯清理 | 待实机：当前构建事件清理延迟与边界 |
| 9 | F6 事件 500ms 合批和绝对动作期限 | `5f517a81`、`b223bbe2`、`380013d4`；设计见 `2026-11-01-f6-event-batching-deadline.md` | 通过：Scan Bus 定向套件覆盖跨拍合批、慢扫事件、Action 抢占与水位 | 待实机：当前构建 30 分钟真实事件端到端延迟 |
| 10 | F7 采集 ticker 按最近真实采集和设置重算截止时间 | `ed963988`、`6cfd5e62`、`716f302d`；设计见 `2026-11-01-f7-ticker-deadline-reschedule.md` | 通过：usage Rust 定向套件 490 passed / 2 ignored | 待实机：真实多窗口竞争、手动/可见/启动采集插队与改间隔 |
| 11 | F7 恢复可见与设置首开回执刷新 | `f9893901`、`cafaebd4`、`721a2e91`；设计见 `2026-11-01-f7-visibility-and-settings-refresh.md` | 通过：定向 Vitest 37 passed；`pnpm check` 通过 | 待实机：多 WebView 隐藏/恢复与设置首开刷新 |
| 12 | F8 显示 CPU 采样降频；F9 dsh 路径缓存与告警去重 | F8：`57b88eb2`、`53dbf1bb`、`a1c845b3`；F9：`4f7a4348`、`8affb7e6`；CPU 复审修复见本记录 §4 | 通过：CPU 候选测试 13 条，含 Windows 实际 sysinfo 累计时间、新 PID/System 重建、独立采样窗口及部分批次结算；dsh 测试 48 条 | 待实机：Windows/macOS release 三档 CPU/扫描频次和 PID/cwd/exe/form 配对；F9 热扫成本、三天多工具账本/会话对照 |
| 13 | F10 稳定进程树测量、运行指标、原子 stats 与最终矩阵 | 实施代码/脚本/本矩阵；设计见 `2026-11-01-f10-measurement-and-runtime-observability.md` | 通过：Scan Bus 66 条；Windows PowerShell 5.1 `-SelfTest` 覆盖零中位数与单轮分类；树进程 1 轮冒烟显示重复性未测；见下方门禁 | 待实机：改造后 release 三档测量、两组三次中位数重复性、动作 p95 前后基线与当前值 |

> 表内「纳入定向套件」表示本轮按实施计划运行对应自动化命令后，只有命令通过才可改记为已通过；不以代码审阅或旧版数据代替实跑结果。

## 2. 自动化门禁记录

| 门禁 | 命令/范围 | 结果 |
| --- | --- | --- |
| Scan Bus | `cargo test adapter::scan_bus::tests --lib` | 66 passed |
| Wakeup 事件监控 | `cargo test adapter::wakeup::tests --lib` | 29 passed |
| 用量采集 | `cargo test services::usage:: --lib` | 490 passed / 2 ignored |
| dsh 监控 | `cargo test monitor::dsh:: --lib` | 48 passed |
| CPU 进程候选 | `cargo test adapter::candidate_whitelist_tests --lib` | 本轮复审后 13 passed；覆盖 Windows 实际 sysinfo 累计 CPU 时间、新 PID/System 重建、PID 复用及退出候选下 OpenClaw 忙转闲 |
| 手机端归档/写后扫描 | `cargo test remote::server::tests::board_close_hide_tests --lib` | 10 passed；含同步测试串行设施下的三个异步路由用例 |
| OpenClaw CPU 判据 | `cargo test monitor::openclaw_parser::cpu_status_tests --lib` | 1 passed |
| Rust lint/格式 | `cargo clippy --all-targets -- -D warnings`；`cargo fmt --manifest-path src-tauri/Cargo.toml -- --check`；`git diff --check` | 通过 |
| 前端复审定向 | 三个新旧缓存/宠物/用量回归文件 | 45 passed |
| 前端完整检查 | `pnpm check` | 通过；lint 有 3 条未改动移动端文件警告；Vite 配置有 native loader 提示 |
| Windows release 构建 | `pnpm tauri build --no-bundle --features marker-helper` | 前一轮构建通过（release profile 28m49s）；当时产物 `src-tauri/target/release/tuvis.exe`（48,557,568 bytes，SHA-256 `7AF4E71C04D5DEFD0A5831440C86F7A3EFE622C19B30DC72B6DE3BEBE6873E06`）；有非致命 Vite/linker 提示，未安装或启动。该历史产物不包含 CPU 部分批次修复；最终构建身份见 PR 验证记录 |
| 测量脚本自检 | `powershell -NoProfile -File scripts/measure-scan-bus.ps1 -SelfTest` | Windows PowerShell 5.1 通过：稳定树、子进程新增/退出、PID 创建身份变化、CPU 回退；全零稳定性未定义、混合零/非零标不稳定、单轮重复性未测 |
| 测量脚本冒烟 | Windows PowerShell 5.1 下 `-CpuOnly -WarmupSeconds 0 -Seconds 1 -Runs 1 -Tree`，目标为 PowerShell 自身 | 通过：输出非负 CPU，树采样识别 1 个进程并标「重复性未测」；不读取 Tuvis stats 或会话，因此不是产品基线 |

## 3. 全仓补充检查

以下为最终代码检查结果；失败项保留失败状态。

| 门禁 | 结果 | 判定 |
| --- | --- | --- |
| Rust 全量 | `cargo test --manifest-path src-tauri/Cargo.toml --lib -- --test-threads=1`：3295 passed / 6 failed / 46 ignored（3347 tests） | 6 条失败为 Claude badge、Codex/OpenCode report flag 时间窗、Windows 文件池路径大小写、subagent 本机目录 ×2；与 [2026-10-29 基线记录](../evidence/2026-10-29-scan-bus-baseline.md) §10.15.5 记载的既有失败集一致，六项分别单跑仍失败，失败名称及现场与改造前基线逐项一致 |
| 前端全量 | `pnpm test`：2580 passed / 0 failed / 5 skipped（2585 tests） | 通过；首次并行检查时 `tests/speed/speed-acceptance.test.tsx` 一项超时，单文件 6 项通过，最终全量重跑通过 |
| F2/F4 前端定向 | 6 个 snapshot arbitration、sync、mutation settlement、useSessions 与文案扫描文件 | 35 passed |

## 4. 基于 `2c0a4e9f` 的复审修复

| 复审项 | 修改与回归证据 | 状态 |
| --- | --- | --- |
| 同版本快照阻止失败操作回滚 | 失败/超时结算从当前窗口已登记的权威快照重建 Query 缓存，再叠加其它有效 pending；真实挂载 `useSessionsQuery` 与 QueryClient 的回归用例验证。 | 已修复；纳入 45 条前端定向测试 |
| 宠物卡片回滚后索引缺失 | 回滚时同步恢复目标 session 索引；组件用例验证恢复后可再次关闭、可跳转。较新快照已删除会话时不复活旧卡：新增用例先断言失败，再修复并通过。 | 已修复；纳入 45 条前端定向测试 |
| 设置页旧 dashboard 覆盖新值 | `loadToday` 请求序号拒绝迟到结果；采集去重水位仅在刷新成功提交后推进，失败允许同时间戳事件重试。 | 已修复；纳入 45 条前端定向测试 |
| Windows CPU 启动样本错误 | 不再按刷新次数猜差分就绪；使用每 PID 的 `accumulated_cpu_time()` 增量除以同 PID 单调墙钟间隔。首个读数仅建基线；新 PID/System 重建的真实 sysinfo 行为已在本机 Windows 测试。 | 已修复；CPU 定向测试 12 passed |
| macOS CPU 分母错窗 | 不再消费 sysinfo 共享窗口的 `cpu_usage()` 百分比；各 PID 以自身累计 CPU 时间与自身墙钟区间计算，避免不同 PID 采样时刻不一致时放大显示值。 | 代码及数据窗口测试通过；macOS 实机待验 |
| 完整 Clippy 与 await 持锁 | 移除冗余整数转换；三个路由测试把同步串行锁放到 async runtime 外持有。`cargo clippy --all-targets -- -D warnings` 与三个路由用例所在 10 条测试通过。 | 已修复；全目标 Clippy 通过 |
| 部分 CPU 批次丢弃存活 PID 样本 | 累计 CPU 采集过滤 `exists=false` 与 PID 创建时间不符的进程；状态和基线按有效 PID 独立更新。测试复现两个目标、一个更新：OpenClaw 从 95.41% 降到 1.58%，同时完整展示截止时间保持不变。该部分批次测试注入有效 PID 读数；退出进程的 `exists()` 过滤有源码核验，尚无真实退出进程的集成夹具。 | 已修复；CPU 候选测试 13 passed；全目标 Clippy 通过 |

## 5. 必须保留为待实机的验收

以下项目依赖当前 release 进程、真实窗口/手机连接或持续运行数据。历史 release 产物未启动，且不包含 CPU 部分批次修复；最终构建身份见 PR 验证记录。启动 release 会连接真实用户目录（`app_data_home()` 忽略 `TUVIS_HOME`）；安装版仍是 v0.6.3（SHA-256 `7997886F47C7EBBEC05C795A02B989FA0E35C7038B0F163AB5393240831FC065`）。因此未写用户用量账本，并将以下项保留待实机：

1. F1：当前构建真实 panic/退出恢复与单一扫描执行权。
2. F2/F8：当前 release 三档 CPU/扫描频次及 PID、cwd、exe、form 配对对照。
3. F3：至少 20 次新建后立即手机发送，并覆盖慢扫超时/degraded。
4. F4：真实主窗与宠物多 WebView、手机 SSE/REST 重连。
5. F5：普通未读、聚合绿卡、单卡、批量、跳转、宠物、手机归档。
6. F6：当前构建至少 30 分钟真实事件零丢失、清理边界与端到端延迟。
7. F7：真实多 WebView 竞争、恢复可见、ticker 插队和改间隔。
8. F9：热扫成本下降及多工具三天账本/会话对照。
9. F10：改造前与当前动作 p95，以及两组各三轮 release 测量的中位数重复性。

历史基线只见 [2026-10-29 基线记录](../evidence/2026-10-29-scan-bus-baseline.md)，其内容为 v0.6.3 数据，不属于本轮改造后验收。
