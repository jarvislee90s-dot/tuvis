# Codex Desktop IPC 实施 Agent 提示词

下面正文可直接复制给执行 Agent。

---

你负责 Tuvis 的 Codex Desktop IPC 远程控制实现；当前聊天的 Agent 负责后续独立 review。请实际完成编码、测试和本地提交，不仅输出方案。

仓库：`/Users/jarvis/Documents/MultiAgents-Manager/tuvis`。

先完整读取：

1. `AGENTS.md`、`docs/MASTER-PLAN.md`（最高设计依据，禁止修改）。
2. `docs/superpowers/specs/2026-10-16-codex-desktop-ipc-remote-control-design.md`。
3. `docs/superpowers/plans/2026-10-16-codex-desktop-ipc-remote-control.md`。
4. `probe/mac-ipc-probe-report.md`；若交接环境没有报告，以计划中记录的协议事实开工并申报证据附件缺失，不能伪称读过。

使用 `superpowers:using-git-worktrees` 准备独立 worktree，使用 `superpowers:executing-plans` 逐项执行计划。实施架构已经确定，普通实现选择自行处理；发现与宪法冲突必须停止相关工作向用户报告，不能改宪法。不要重新展开产品 brainstorming，也不另开一轮长期 Probe。

用户已裁决：本期包含 command approval 和 permissions approval；同一套跨平台协议/控制层，Windows Named Pipe、Mac Unix socket 两个后端。Mac Plan 阻塞问答、command 变体与重启恢复的实测缺口并入最终 E2E，不阻塞其余编码；未验证的方法保持 profile 禁用，不能把 mock 通过写成实机 PASS。

分支流程按计划 Task 0 执行：

- PR #117 仍未合并，原分支 `feat/h1-h12-headless`。先 fetch 并记录最新 head/main；从 #117 新建 `codex/headless-main-integration`，merge 最新 main，保留公开历史。不要在原 #117 分支 rebase/force-push，也不要从 main 单独 cherry-pick H8 初版。
- 整合 main 的错误码/i18n、文件产物等变更，跑完基线门禁，记录集成点 **I**。
- 再从 I 开 `codex/codex-desktop-ipc`，仅提交本期 IPC 变更。若已存在适用的集成/IPC worktree，检查状态后续用，不能 reset 用户工作。
- 本机初始快照是 #117 `289b20d`、main `d3fef5f`；它们不是固定执行基点，实时发生变化时重新检查。当前工作区有其他未跟踪文档和宠物素材，禁止 `git clean`、全量 stash 或 `git add .`。精确携带本任务 spec/plan/提示词。

实现严格遵守：

- Desktop IPC 独立于 `inject/headless`，不把 app-server stdio 客户端当 Desktop IPC transport。直接复用 #117 最终版 Codex queue/exec fallback，保留后续安全修复，不复制旧初版。
- owner/client 身份由服务端发现；原始 IPC、socket、owner 参数不向网络代理。所有新端点复用现有设备 cookie 门禁，回环也无豁免。
- 按方法版本门、envelope/params hostId 区分、following 前提、turnHistory、整数/字符串 requestId 与 generation/epoch 实现；不得依赖 `clientType==desktop` 或 `availableDecisions` 在场。
- 普通消息才有 fallback；审批/结构化回答绝不补投为普通消息。阻断旧 Codex APP 的问答补投入口，CLI 屏读逻辑保留。
- `UNKNOWN_DELIVERY` 不自动重试、不自动 fallback；断线、HTTP client drop、服务热重启/应用重启都不能导致重复执行。实现计划里的持久动作账本、FIFO 与上下文过期校验。
- 新队列要能查看和撤回未发项，重启后使用持久化原文/设备/client message ID 继续；fallback 的 queued/ok 回执不证明 Turn 已结束，保留持久调度限制直到有对应完成证据。明确零投递的 NotSent 才可在落账事务中直接解除该限制。
- A 路径受远程认证；H3 无头开关只控制兼容分支，不能把 A 一并挡掉。忙 Thread 留队，审批/问答可以继续原 Turn。
- 不增加 interrupt/steer、切模式/模型、file approval 等范围外能力；不动真实用量账本，不启动可能写真实账本的 Tuvis 开发进程做自动化测试。

每个 Task 按失败测试→最小实现→验证→本地提交推进，并勾选计划进度。测试用 memory DB/tempdir/fake IPC，不连接真实用户 Thread。必要的小幅接口修正可以同步计划并说明理由；范围变更或协议证据冲突先报告，不靠猜测放开功能。不要因一般实现选择反复询问用户。

门禁按计划执行：`pnpm test`、`pnpm check`、`pnpm build:mobile`、`node scripts/check-error-codes.mjs`、`bash scripts/ci-backend-local.sh`，确认新测试实际运行，不能接受 0 tests 被当 PASS。Windows named pipe 必须在 Windows 原生测试；交叉编译不能代替运行证据。Mac/Windows 真机缺项分别记录，无法触发写 NOT_TRIGGERED，不写“不支持”或“已完成”。重启 Desktop 会影响用户正在进行的工作，安排时机需取得该操作授权，其余工作继续推进。

不要 push、创建 PR 或 merge main；完成后交给原聊天 review。最终报告必须包含：两个分支/worktree 路径、集成点 I、IPC HEAD、各 Task 提交、`git diff --stat I..HEAD`、自动门禁结果、双平台验收表、待验/失败项与计划偏差。代码与自动检查完成、产品双平台验收完成须分开报告。若环境限制导致某项不能完成，留下可继续的准确状态，不把未完成项勾掉。
