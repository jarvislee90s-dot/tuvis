# Codex APP 交付 · 来源清单与抽取账本（2026-10-20）

> 计划 Task 2 交付物。记录正向集成工作区、全部来源 SHA、逐层归属、混合文件拆分方式、
> main-only 保留清单与「不交付项」的去向。
>
> **结论先行**：本轮从 `origin/main`（`77fcdb48`）正向接出，只搬运
> 「由 Codex 实际消费」的底座 + Codex queue/exec + Desktop IPC，未整包合入 #117；
> 所有不交付的增量在 #117 原分支与 Task 12 的续作 handoff 中保留来源。

## 1. 基线与工作区

| 角色 | 引用 | SHA | 说明 |
| --- | --- | --- | --- |
| 正向集成起点 | `origin/main` | `77fcdb487e8bd32eb4109cf8a274e7663a0ef43c` | fetch 后核对；未被旧分支覆盖 |
| 冻结代码基线 | `origin/codex/codex-desktop-ipc` | `42cac262fd80d4a8e679fe138df75b6b4d48357e` | #165 实现 + 已修审查项 + 本批 Windows 4894 profile/探测报告 |
| 旧 IPC 集成点 | `origin/codex/headless-main-integration` | `d5cc28b1eb20133ee37b788013c6d318e4731d46` | IPC 真实增量的起点（`d5cc28b1..42cac262`，77 文件） |
| #117 原来源 | `origin/feat/h1-h12-headless` | `289b20db4d6bdfba50ecfdeb67f1ef6f47ab583f` | 原始提交 / 混合 hunk 溯源；执行时核对 head 未漂移 |
| #117 合流基线 | merge-base(main, ipc) | `d3fef5f7408047d4583ec1605b4bfb7c0bb3ccad` | 2026-10-16；main 从此点前进 43 文件 |

- 工作树：`E:\LLMproject\Github\tuvis-pr165-review`（规划工作区）保持原样，仅有计划文件未跟踪。
- 隔离交付工作区：`E:\LLMproject\Github\tuvis-codex-app-delivery`（分支
  `codex/codex-app-delivery`，从 `origin/main` 建 worktree）。
- 只读核对命令（执行时各一行）：
  - `git fetch origin main codex/codex-desktop-ipc feat/h1-h12-headless`
  - `git diff --name-status origin/main...42cac262`
  - `git diff --name-status d5cc28b1..42cac262`
  - `git log --reverse --oneline d5cc28b1..42cac262`

## 2. main-only 保留清单（不得被旧分支整文件覆盖）

main 自 merge-base 前进 43 个文件。与本轮重叠的只有 5 个，全部按「main 版本 + 本层增量」
合并，而不是覆盖：

| 文件 | main-only 变化 | 本轮处理 |
| --- | --- | --- |
| `AGENTS.md` | 发版文档纪律改为独立 `CHANGELOG.en.md`（新增 2026-10-17 行） | 保留 main 版；本轮发版义务按 main 的 AGENTS 执行 |
| `src-tauri/src/remote/api.rs` | +5 行（附件/错误码相关） | 保留 main 版，只插入 Codex 分派/ H3 门 / hub 路由 |
| `src-tauri/src/remote/server.rs` | +62 行（新路由/测试） | 保留 main 版，只加 `codex_control` 字段与 `/codex/*` 路由 |
| `src-tauri/src/services/mod.rs` | +114 行（tracking/subagent 等模块） | 保留 main 版，只加 `codex_control`/`codex_desktop_ipc` 两行 |
| `src/i18n/locales/{zh,en}.json` | 各 +6 行（tracking 文案） | 保留 main 键；只合并 `mobile.codex.*` 与 `settings.remote.headless*` 子树 |

其他 main-only 改动（`CHANGELOG.en.md`、`monitor/**`、`remote/content.rs`、
`remote/tunnel.rs`、`presetList`、`RemoteSection` 的 tracking 文案等）全部照 main 保留。

**路线更正**：旧整合计划「先合 #117 再改 #165 base」的路线已被本决策取代；本轮不做旧
PR 对齐 main 的合并，只从 main 正向抽取。

## 3. 逐层归属

### 3.1 共用无头底座（来源：#117 `ac0d524c`/`36e4f657`/`3ec5f4a6` 等，最终版 = 42cac262）

| 目标 | 来源 | 处理 | 验证 |
| --- | --- | --- | --- |
| `inject/headless/runner.rs` | #117 底座原文件 | 逐字搬运；进程树清理/并发 2/watchdog 600000/clamp 未改 | `cargo test --lib inject::headless` |
| `inject/headless/receipt.rs` | 同上 | 逐字搬运 | 同上 + `tests/fixtures/headless_stages.json` |
| `inject/headless/turn.rs` | 同上 | 逐字搬运（串行锁/审计词/命令垫片判据） | 同上 |
| `inject/headless/gate.rs` | 同上 | **裁剪**：删除 zcode 两级探针/L0/provider-config/`probe` 入口，只留 Codex `queue --help` 探针 + 缓存/TTL | `cargo test --lib inject::headless::gate` |
| `inject/headless/mod.rs` | 同上 | **裁剪**：只注册 `codex/gate/receipt/runner/turn`；`PermissionSpec` 只留 `CodexExec` | 编译 + 单测 |
| `inject/mod.rs` 的 `audit_write_channel` | #117 H6 | 搬运薄壳重构（既有调用方零改动） | `inject::headless` 审计测试 |

**不搬运**：`cli_three.rs` / `wb_acp.rs` / `dsh_acp.rs` / `zcode.rs` / `zcode_create.rs` /
`codex_app.rs` / `inject/confirm.rs` 的 L14 改动 / `window/tty_map.rs` 的 L13 靶向闸 /
`adapter/mod.rs` 的共享进程快照 —— 见 §5。

### 3.2 Codex 兼容发送（来源：`a7a734ad` + IPC 分支修正，最终版 = 42cac262）

| 目标 | 来源 | 处理 |
| --- | --- | --- |
| `inject/headless/codex.rs` | #117 H8 + 后续评审修正 | 逐字搬运（PATH/rollout UUID/presence/queue-exec/消费确认/writer lock/cmd 垫片拒绝） |
| `inject/normalize.rs` | #117 裁决 24b | 逐字搬运（`compose_injection` → `Result`：花名白名单 + 展示截断） |
| `inject/routing.rs` | #117 路由表 | **重写为 Codex-only**：`HeadlessKind::{CodexQueue,CodexExec}`、`Channel::Headless`、`headless_kind_of`、`has_terminal_candidate`；其余工具门/终端行为逐字保留 |
| `inject/queue.rs` | #117 Task 8 义务 2 | 只加「投递侧重判」（`headless_kind_of` → `Deferred`）；不搬 L13/L14 |
| `monitor/codex_thread_parser.rs` | #117 | 仅 `latest_versioned` 提为 `pub(crate)` |
| `remote/api.rs` | #117 H8 + IPC Task 9 | 只加 `headless_blocked`、`audit_headless_ctx`、`headless_envelope`、`channel_wire` 的 Headless 臂、`CodexTurnCtx`/`run_codex_turn`/`codex_headless_dispatch*`、session-send 的 hub/H3/⑤b 与 send-info 的 codex 能力档 |
| `remote/mod.rs` | #117 H3/H4 + IPC Task 8 | 只加 H3/H4 键与解析、`apply_headless_status`、两个命令、`init_headless_limits`、`codex_control` 字段与 stop |
| `src/mobile/{SessionDetail,MessageComposer,api}.tsx/ts` | IPC `d5cc28b1..42cac262` | **改写接入**：保留 main 结构，只加 Codex 投影/原生卡/队列面板/actionId/回执分支 |
| `src/mobile/{codexIpcApi,useCodexThread,PendingRequestCard,CodexQueuePanel}` | IPC 增量 | 逐字搬运 |
| `src/components/settings/RemoteSection.tsx`、`src/lib/api/remote.ts` | #117 H3/H4 设置面 | 只接无头开关 + 超时/并发；一次性说明弹窗未接（偏差见 Task 9 报告） |

### 3.3 Desktop IPC + 动作账本（来源：`d5cc28b1..42cac262` 全量增量）

| 目标 | 来源 | 处理 |
| --- | --- | --- |
| `services/codex_desktop_ipc/**`（13 文件） | IPC 增量 | 逐字搬运；`types.rs` 依赖 `codex::is_uuid` 保留 |
| `services/codex_control/{mod,queue,router}.rs` | IPC 增量 | 逐字搬运 |
| `remote/codex_ipc.rs` | IPC 增量 | 逐字搬运（handler/SSE/队列/hub 生命周期） |
| `database/dao/codex_actions.rs` | IPC 增量 | 逐字搬运 |
| `database/{schema,migration}.rs` | IPC 增量 | 表 + partial index + `barrier_force_released_at` 幂等迁移 |
| `database/dao/write_audit.rs` | IPC 增量 | 逐字搬运（`record_conn_checked`） |
| `remote/server.rs` | IPC 增量 | 只加字段与 `/codex/*` 路由、serve 时 `start` |
| `tests/{codex_desktop_ipc,codex_ipc_remote,codex_control_remote}.rs` | IPC 增量 | 搬运；删除 2 个依赖 #117 补投臂的用例、补 `codex_control` 字段 |
| `tests/fixtures/codex-desktop-ipc/**` | IPC 增量 | 逐字搬运 + 本批 2 个 DERIVED 开场/忙态样本 |

### 3.4 本轮新增（非搬运）

| 文件 | 任务 | 说明 |
| --- | --- | --- |
| `services/codex_control/e2e_stop.rs` | Task 8 | E11 确定性停点；release 编译门 |
| `src/mobile/useDelayedUnsupported.ts` + 测试 | Task 7 | 500ms 提示去抖 |
| `mirror.rs` 的 `opening_input` + 两遍投影 + 4 单测 | Task 6 | `params.input` 开场后备 |
| `docs/superpowers/reviews/2026-10-20-codex-app-*.md` | Task 1/2/9/10/12 | 证据/账本/复审/验收/handoff |

## 4. 混合文件拆分记录

| 文件 | 旧分支含 | 本轮取法 |
| --- | --- | --- |
| `remote/server.rs` | 1008 个 hunk，含 zcode 新建/无头取消/claude 审批/app-server 等路由与测试 | 从 main 出发，只加 `codex_control` 字段（21 个字面量 + STATE）、7 条 `/codex/*` 路由、serve 的 `start` |
| `remote/api.rs` | 124 hunk，含 zcode/wb/dsh/CLI/审批/补投分派 | 从 main 出发，只加 Codex 分派与 hub/H3；`session_send` 的 ⑤b 只有两个变体臂 |
| `inject/queue.rs` | L13 靶向闸 + L14 回执诚实化 + Task 8 重判 | 只加 Task 8 重判（12 行 + 注释） |
| `inject/confirm.rs` | L14 插队回执 | 不取；只修 normalize 签名变化影响的 2 处单测 |
| `src/mobile/SessionDetail.tsx` | app 面分套（ModeBarApp/QuestionCardApp/NewSessionForm）+ IPC 投影 | 不取 app 分套；在 main 详情页加 Codex 投影/原生卡/队列面板（不可写档保留旧 QuestionCard） |
| `src/mobile/MessageComposer.tsx` | zcode 无头回合上报 + IPC actionId/回执 | 只取 actionId 与三类新回执；不取 zcode 回合卡 |
| `Cargo.toml` | JobObjects/Pipes feature + test-util + 新依赖 | 取 feature/test-util；无新增运行时依赖 |
| locale | 两批合并 +284 行 | 只合 `mobile.codex.*`（46 键）与 `settings.remote.headless*`（10 键） |

## 5. 不交付项与去向

| 不交付项 | 来源 | 去向 |
| --- | --- | --- |
| zcode 无头/新建（`zcode.rs`、`zcode_create.rs`、路由 zcode 臂、CLI 新建端点） | #117 | #117 续作分支（Task 12 handoff） |
| WorkBuddy ACP（`wb_acp.rs`、端点、移动端审批卡分支） | #117 | 同上 |
| dsh ACP（`dsh_acp.rs`、路由 DshAcp） | #117 | 同上 |
| claude/kimi/opencode 无头（`cli_three.rs`、H11 路由、审批桥） | #117 | 同上 |
| codex app-server 控制面（`codex_app.rs`、模式切换/中断/补投/GET tiers） | #117 | 同上 |
| L13 靶向闸（`window/tty_map.rs`、`target_evidence`） | #117 | 同上 |
| L14 插队回执诚实化（`confirm.rs` + queue 合流） | #117 | 同上 |
| 移动端旧 APP 控制按钮/新建框架（ModeBarApp、NewSessionForm、CreateSessionSheet 改版） | #117 | 同上 |
| 原生空首帧（不创建空 session、不伪造） | 产品边界 | 保持缺口，产品文档如实登记 |
| Windows permissions 解锁 | 用户裁决 | 保持 false；只归档审批证据 |

## 6. 校验入口

- 编译：`cargo check --lib`、`cargo check --tests`
- 三组集成测试：`cargo test --test codex_desktop_ipc`（42）、`--test codex_ipc_remote`（50）、
  `--test codex_control_remote`（34）
- 移动端：`pnpm exec vitest run tests/mobile/{codexIpcApi,useCodexThread,SessionDetail,MessageComposer,PendingRequestCard,CodexQueuePanel,useDelayedUnsupported}.test.tsx`
- 完整门禁与逐项结果见 `2026-10-20-codex-app-integration-review.md`。
