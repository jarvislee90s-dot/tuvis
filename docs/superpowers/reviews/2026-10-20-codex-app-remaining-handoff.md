# Codex APP 交付后的续作分支与交接（#117 剩余增量）

> 计划 Task 12 交付物。本轮只整理**剩余增量**的来源与交接，不删除、不改写 #117 / #165
> 原分支（两者保留作追溯）。本阶段不发版、不预填版本号。

## 1. 来源与保留

| 分支 / 引用 | SHA | 状态 | 用途 |
| --- | --- | --- | --- |
| `feat/h1-h12-headless`（#117） | `289b20db4d6bdfba50ecfdeb67f1ef6f47ab583f` | **保留、不推送改动** | 其他驱动与旧 APP 控制框架的原始提交 |
| `codex/headless-main-integration` | `d5cc28b1eb20133ee37b788013c6d318e4731d46` | **保留** | #117 与旧 main 的合流点；IPC 增量起点 |
| `codex/codex-desktop-ipc`（#165） | `42cac262fd80d4a8e679fe138df75b6b4d48357e` | **保留** | 本轮 IPC 代码与夹具来源 |
| `codex/codex-app-delivery`（本轮） | 见 §5 | 交付分支 | 底座 + Codex 兼容 + IPC |
| `origin/main` | `77fcdb487e8bd32eb4109cf8a274e7663a0ef43c` | 基线 | 本轮正向集成起点 |

**续作分支命名建议**：`codex/app-controls-followup`（从本轮 `codex/codex-app-delivery`
的合并结果或 main 起），只承接下表增量；**不要**再从 `feat/h1-h12-headless` 整包合入。

## 2. 剩余增量清单（#117 中未随本轮交付的部分）

| 增量 | 主要文件（#117 / 42cac262） | 依赖前置 | 备注 |
| --- | --- | --- | --- |
| zcode 无头 + 新建 | `inject/headless/zcode.rs`、`zcode_create.rs`；`remote/api.rs` 的 `run_zcode_turn`/`session_create_zcode*`；`routing.rs` 的 `HeadlessKind::Zcode` | 底座（本轮已交付） | 会新增路由臂与两个端点 |
| WorkBuddy ACP | `inject/headless/wb_acp.rs`；`run_wb_turn`；`HeadlessKind::WbAcp` | 底座 | 端点发现双源（心跳/端口指纹） |
| dsh ACP | `inject/headless/dsh_acp.rs`；`HeadlessKind::DshAcp`；`run_dsh_turn` | 底座 | 需要 `@deepseek-ai/dsh-acp` |
| claude/kimi/opencode 无头 | `inject/headless/cli_three.rs`；`HeadlessKind::{ClaudeP,KimiP,OpencodeRun}`；审批卡/取消端点 | 底座 | 含 claude 双向审批桥 |
| codex app-server 控制面 | `inject/headless/codex_app.rs`；`remote/api.rs` 的 mode/menu/interrupt/补投/GET tiers；`QuestionCardApp`/`ApproveCard` 的 app 分支 | 本轮 IPC | **权限/模式切换属冻结 spec §5 外**，续作不得顺手解锁 |
| L13 靶向闸 | `window/tty_map.rs`；`RemoteState.target_evidence`；`inject/queue.rs` 的 `AmbiguousTarget` | 无 | 与 Codex 无直接关系 |
| L14 插队回执诚实化 | `inject/confirm.rs`；`queue.rs` 的 `resolve_jump_receipt` | 无 | 影响终端注入回执 |
| 移动端旧 APP 控制 UI | `ModeBarApp.tsx`、`NewSessionForm.tsx`、`CreateSessionSheet.tsx`、`QuestionCardApp.tsx`、`ModeBarCli/QuestionCardCli` 改名 | 续作路由 | 与本轮 Codex 投影并存 |
| 旧 Codex APP 补投臂 | `remote/api.rs` 的 `codex_app_answer_repost_text` / `native_only 503` | app-server 控制面 | 本轮对 Codex APP 问答仍走既有 409 止血门 |
| 空首帧产品功能 | 无（产品边界） | — | 不创建空 session、不伪造；如实保留缺口 |

## 2.1 Task 12 执行状态（如实）

- **本轮未执行**：`codex/backup-pr117-20261020` 备份分支、从新 main 建
  `codex/headless-remaining`、逐 hunk 重放与“已交付/续作/不再需要”判定。
- 原因：该任务原本在**新 PR 合入 main 之后**执行；本轮只交付来源清单与本 handoff 文档。
- 计划 Task 12 的对应项已恢复为**待办**（不勾选），交由合入后的执行者按 §3 继续。

## 3. 续作执行清单（可直接交给下一位执行者）

1. 从本轮交付合入后的 main 建分支；`git fetch` 后记录新 SHA。
2. 对每个增量：从 `42cac262` 取**最终版文件**，按本轮同法做「main 版本 + 只加该增量」，
   禁止整包替换 `remote/server.rs` / `remote/api.rs` / `inject/queue.rs`。
3. 必跑的回归：
   - `cargo test --features marker-helper`（Windows 上 5 例 HOME 隔离失败属基线，见
     验收报告 §5，不用修）；
   - `cargo test --test codex_desktop_ipc/codex_ipc_remote/codex_control_remote`；
   - `pnpm test`、`pnpm check`、`pnpm build:mobile`；
   - `node scripts/check-error-codes.mjs`（新增中文错误串必须带码，棘轮只降不升）。
4. 权限面：Windows `permissions` 仍 false；新增 approval 证据只能改 profile 的
   capabilities 字段并附实测 build，不得改默认开。
5. 移动端：新增通道词必须与 `HeadlessKind::wire_name()` 同源；`actionId` 只对
   `headless_codex_*` 携带，其他工具请求体保持零漂移。
6. 发版：本轮合入 main 后随下一常规版本发布；续作同样不单独发版。

## 4. 证据回收目录约定

- Windows：`E:\LLMproject\Github\tuvis-verify-logs\2026-10-20-*`（raw 不提交）。
- Mac：Mac 执行者私有目录 `~/Documents/tuvis-verify-logs/2026-10-20-macos/`；只回传脱敏
  摘要与结论，不整目录转发。
- 入库证据：`src-tauri/tests/fixtures/codex-desktop-ipc/observed/`（脱敏、DERIVED 必须
  在 `_source` 与 README 标明来源与唯一修改）。

## 5. 本轮交付提交

| 提交 | 内容 |
| --- | --- |
| `67b01a13` | 前置证据 / 来源账本 / 计划 / DERIVED 夹具 |
| `d7d1d732` | 通用无头底座 + Codex queue/exec 兼容 |
| `d6d2d348` | Desktop IPC + 动作账本 + 移动端投影 + Task 6–8 修正 |
| `8d3e237b` | 双平台验收边界、本交接、能力矩阵与计划勾选 |
| `bf9b2949` | 第一轮复审收口（文档诚实化 / 死码 / 孤儿文案） |
| `bf13b4d8` | 第二轮复审 D–G 收口（补 4 个移动端测试 / 迁移回归 / 删 ⑤b 死臂 / 双语值） |
| （第三轮修复提交） | 7 项审查缺陷：忙态/旧 Turn、签名冻结、昵称门收窄、详情页接线、兼容前设备复核、SSE 首帧、H3 原因 |
| （最终 docs 提交） | 复审记录按最终 HEAD 更新 |
