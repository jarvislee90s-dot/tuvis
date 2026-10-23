# 执行计划：Codex APP 未知 build 兼容（#172）

> 给执行 Agent 的施工单。Review 由 Tuvis 负责人（本会话）负责，按 commit 审。
> 设计依据：`C:\Users\bunny\Desktop\2026-10-21-Codex-APP-版本兼容与能力探测设计.md`（v4）
> 路线图：`C:\Users\bunny\Desktop\2026-10-21-多build适配路线图.md`

---

## 0. 角色与边界

- **执行 Agent**：按任务实现、自测、提交；**不合并、不打 tag、不发布、不自动 push**。
- **Review**：按 commit 审 diff；不合格退回重做，不接受“先合并再修”。
- **用户**：只做两件事——批准仓库准备工作、配合一次真机审批验收。

---

## 1. 基线（先回答：从哪起）

**从远端 `main` 起，基准 commit = `0e53cb62fac2e4adf15f019f7630ece49cbde675`。**

- 这是 PR #170（usage dashboard V2）合并后的 main；
- PR #171 的 merge commit `0ad8d763e5bdc55e0934ef82af2eedd1b91cbc15` 是它的**祖先**，不要在 `0ad8d763` 上起分支；
- 分支名：`feat/codex-ipc-unknown-build-compat`；
- 不 rebase 任何已有分支，不 force-push。

验证命令：

```powershell
git fetch origin
git rev-parse origin/main   # 必须输出 0e53cb62fac2e4adf15f019f7630ece49cbde675
```

---

## 2. 仓库准备（已完成，2026-10-21）

现状：工作区已原地重建，**不需要**再建文件夹或 clone。

- 原 `.git` 是坏 gitfile（指向已删除的 `MultiAgents-Manager/.git/worktrees/...`），已移到
  `%TEMP%\tuvis-pr165-review.gitfile.bak` 留档；
- 已 `git init` 并从远端 `main` 拉取，baseline = `0e53cb62fac2e4adf15f019f7630ece49cbde675`；
- 已创建分支 `feat/codex-ipc-unknown-build-compat`，工作区干净（`git status` 为空）；
- 旧 worktree 的 36 个未跟踪残留（PR117 的 `codex_app.rs`、`zcode.rs`、`ModeBarApp.tsx` 等）会破坏
  TypeScript 构建，已整体 **stash**：`stash@{0}: leftovers from pr165 worktree (PR117 files) 2026-10-21`
  （可恢复，不是删除）；
- `node_modules/` 仍是旧树留下的（被 gitignore），**必须**重新安装。

执行 Agent 第一步只做依赖安装与预热：

```powershell
cd E:\LLMproject\Github\tuvis-pr165-review
git rev-parse HEAD          # 必须是 0e53cb62fac2e4adf15f019f7630ece49cbde675
git branch --show-current   # feat/codex-ipc-unknown-build-compat
pnpm install --frozen-lockfile
pnpm build:mobile
cd src-tauri; cargo check --features marker-helper
```

磁盘：E 盘当前约 32 GB 可用。若安装或构建因空间/网络失败，**停下来报告**，不要自行清理磁盘。

---

## 3. 不可违反的约束

1. 不改 `docs/MASTER-PLAN.md`；不改冻结 spec。需要改文字时停下，交用户裁决。
2. **不做** `permissions`、`plan_implementation`、`collaboration_mode`、`requestUserInput` 的探测或开放；本单只到 L3 的 command/file 审批。
3. 不 force-push、不 merge、不打 tag、不发布；未经用户明确指示不 push。
4. 不批量删除；不 `rm -rf` / `Remove-Item -Recurse`；单文件删除需用户确认。
5. 不读/不输出 `~/.codex/auth.json`、keychain、`.env`、任何 token。
6. 只改本任务相关文件；不顺手重构、不重排格式、不改无关测试。
7. 代码注释中文，标识符英文；每个 commit 自身可编译、相关测试可跑。
8. 遇到协议形态与本文不符：**停下记录证据**，不要猜、不要放宽门。

---

## 4. 设计要点（施工所需的最小事实）

### 4.1 五层模型

| 层 | 策略 |
| --- | --- |
| L0 语法 | 直接继承（四个已验证 build 方法表一致） |
| L1 读取 | 惰性只读探测 P0–P3，通过即用 |
| L2 发送 | 读探测过 + 设置开关（默认关）+ 回执为准 |
| L3 审批 | **command/file 默认开**；schema 探测通过才开；permissions/问答不做 |
| L4 权限/模式 | 不做 |

### 4.2 实测量（Windows 4894，2026-10-21）

- initialize 200–440 ms；discovery 11–13 ms（有 owner）；follow→snapshot 15–21 ms；无 owner `no-client-found` ~2 s。
- snapshot：`params.change = {type:"snapshot", revision:N, conversationState:{...}}`；
  P3 判据 = `change.type=="snapshot"` + `revision` 数值 + `conversationState` 含 `turns/requests/turnHistory`。
- **现存 bug**：router 错误响应无 `method` 字段（`{"type":"response","requestId":…,"resultType":"error","error":"no-client-found"}`），
  生产 `connection.rs:649` 要求 method 匹配 → 会卡到 15 s 超时。本单 T0 修。

### 4.3 官方 schema 探针（已实测）

```bash
<bundled codex> app-server generate-json-schema --out <temp> --experimental
```

- 0.23 s；产出 40+ schema 文件；
- command/file approval 的 decision 枚举：`accept | acceptForSession | decline | cancel`；
  `cancel` = 拒绝并立即中断本轮；
- `PermissionsRequestApprovalResponse` 不是 accept/deny，而是权限对象 → **不开放**；
- `ToolRequestUserInputResponse` 是结构化 answers → **不开放**；
- 该命令是 experimental：失败/缺失 → schema=unknown，审批退回“到桌面处理”。

### 4.4 账本

```text
probe_key = platform|build|cli_id|probe_epoch
```

- `PROBE_EPOCH` 常量：方法表/解析期望/探测步骤变更即 +1；
- 表 `codex_ipc_probe(probe_key, capability, state, probe_stage, error_code, attempts, first_seen_at, last_seen_at, next_retry_at, last_duration_ms)`，`PRIMARY KEY(probe_key, capability)`；
- 只写非 shipped build；`state ∈ probed|failed|inconclusive`，`verified` 只来自 shipped 表。

### 4.5 路由纪律

- `state` 与 `send` 解耦：`state` 独立决定“能不能看”，`send` 独立决定“能不能发”；
- desktop-owned 会话：发不到 A 就**诚实失败**，**不得**静默落 headless；
- 审批：未知 build 只给 **允许（accept）** / **终止本轮（cancel）**；已知 build 才给“拒绝（decline）”。

---

## 5. 任务清单（按顺序，每任务一个 commit）

> 每个任务必须写测试；测试要真的跑，不接受 0 tests 当 PASS。

### T0 修 router 错误响应 method 缺省

- 文件：`src-tauri/src/services/codex_desktop_ipc/connection.rs`
- 行为：`requestId` 命中且 `method` 缺省 → 按该 pending 完成；`method` 存在但不一致才放回。
- 测试：
  - `method_less_error_response_completes_pending`
  - `method_mismatch_keeps_pending`
- commit：`fix(codex-ipc): accept method-less error responses`

### T1 拆 `state` / `send` 耦合

- 文件：`mirror.rs`、`remote/codex_ipc.rs`、`remote/api.rs`、`adapter.rs`、`src/mobile/codexIpcApi.ts`
- 行为：
  - `ControlAvailability` 增加 `read_only`；
  - `availability_of`：`state && !send` → `read_only`；
  - `ipc_available()` 拆为 `send_available()` / `state_available()`，逐个改调用点；
  - 兜底判据（2026-10-21 裁决 **B**）：保留 `build_send_verified`；**未知 build（无写证据）不兜底**，**已验证 build 断链保留 R8 兜底**（这是明确例外，不是遗漏）；
  - **send-info 必须与 send 同源**（本次 review 必修）：
    - `codex_app_form && !send_available && !build_send_verified` → `injectable:false` + `reasonCode:"capability_unverified"` + 同一句 reason；
    - 已验证 build 断链（`send_available=false` 但 `build_send_verified=true`）→ 仍走原 H3/route 分支：H3 开 → `headless` 可注入；H3 关 → `headless_disabled`；
    - 判断必须放在 H3 分支**之前**，否则未知 build + H3 关会错报 `headless_disabled`。
- 测试：
  - availability matrix；
  - 未知 build + H3 开/关：send 403 `capability_unverified`；send-info `injectable:false` + 同码；
  - 已验证 build 断链 + H3 开：send 200 `headless`，send-info `injectable:true`（两端口径一致）；
  - `form=Cli` 的 headless 会话不受影响（用例名要说清是 Cli，不要写成 App）。
- commit：`refactor(codex-ipc): decouple state and send availability`（已有），修复追加 `fix(codex-ipc): align send-info with send gate`

### T2 指纹模块

- 新文件：`src-tauri/src/services/codex_desktop_ipc/probe/mod.rs`、`probe/fingerprint.rs`
- 行为：
  - `ProbeKey { platform, build, cli_id, epoch }` + `to_key()`；
  - Windows `cli_id`：`codex.exe --version`（2 s 超时）解析 `codex-cli X`；失败 → 运行中 `codex` 的 `bin/<hash>`；再失败 → None；
  - macOS `cli_id`：读 `Contents/Resources/codex-cli/codex-package.json` 的 `version`；
  - `PROBE_EPOCH: u32 = 1`。
- 测试：key 归一化、epoch 入 key、两平台样例、缺文件 → None。
- commit：`feat(codex-ipc): add probe fingerprint`

### T3 账本

- 文件：`src-tauri/src/database/schema.rs`、`src-tauri/src/database/dao/codex_ipc_probe.rs`、`dao/mod.rs`
- 行为：建表（IF NOT EXISTS）+ DAO：`get` / `upsert_ok` / `upsert_fail` / `upsert_inconclusive`（含 `next_retry_at`）。
- 测试：upsert 幂等、attempts 累加、退避、只一行。
- commit：`feat(db): add codex ipc probe ledger`

### T4 探测执行器

- 新文件：`probe/runner.rs`
- 行为：
  - `ensure_state(key, thread_id)`：P0 `ensure_connection` → P1 discovery（带 `sourceClientId`+`timeoutMs`）→ P2 follow → P3 判据；
  - 单飞（同 key 一把异步锁）；总硬上限 15 s；P0/P1/P2/P3 = 1/2/3/3 s；
  - follow 用临时引用，**立即 unfollow**，不增加 `gate` 计数；
  - `client-discovery-request` 回 `canHandle:false`；被动广播只记链路存活。
- 测试：ScriptedPeer 四场景（同构/版本漂移/字段缺失/断链）+ 单飞 + 超时。
- commit：`feat(codex-ipc): add read probe runner`

### T5 adapter/hub 接入

- 文件：`adapter.rs`、`remote/codex_ipc.rs`、`profile.rs`
- 行为：
  - `profile_for` 未命中 → `ProtocolProfile::unverified()`（不是 closed）；
  - 首次 `state` 访问触发 T4；探测中投影 `read_only`/probing；通过后内存 profile 提升 `state=true`；
  - 连接重建/换 build → 重算 key、查账本；
  - debug-only `TUVIS_CODEX_IPC_FORCE_UNVERIFIED=1`（`cfg!(debug_assertions)`）。
- 测试：shipped 不探测；账本命中零帧；强制开关生效；断链不伪造。
- commit：`feat(codex-ipc): wire probe into adapter`

### T6 官方 schema 探针

- 新文件：`probe/schema_probe.rs`
- 行为：
  - 定位随包 CLI（复用 T2 路径逻辑）；
  - 跑 `app-server generate-json-schema --out <temp> --experimental`（3 s 超时，temp 目录用完即删）；
  - 解析 `CommandExecutionRequestApprovalResponse.json` / `FileChangeRequestApprovalResponse.json` 的 decision 枚举；
  - 记录 `schema_fingerprint`（文件名+大小的 sha256）与 `approval_decisions`；
  - 失败 → `schema=unknown`，审批不开放。
- 测试：用固定 schema 样例解析；命令失败降级；临时目录清理。
- commit：`feat(codex-ipc): add official schema probe`

### T7 L3 审批（command/file）

- 文件：`mirror.rs`（pending 解析）、`adapter.rs`（decision 派发）、`remote/codex_ipc.rs`（能力）、`src/mobile/PendingRequestCard.tsx`、i18n
- 行为：
  - 从 snapshot `conversationState.requests` 读 pending（先抓一条真实快照做 fixture，见 §7）；
  - 未知 build：schema 通过 → 允许 `accept` / `cancel`；schema 未知 → 只显示“到桌面处理”；
  - 已知 build：保留 `decline`；
  - 决策方法：`thread-follower-command-approval-decision` v1；新增 `thread-follower-file-approval-decision` v1；
  - 回执：decision 后等 requestId 从新 snapshot 消失，5 s 超时 → “未确认” + 可终止；
  - permissions / requestUserInput 一律不开放。
- 测试：未知 build 只有 accept/cancel；不发 permissions/command 帧负例；超时不重发。
- commit：`feat(codex-ipc): support compat approvals for command and file`

### T8 L2 兼容发送

- 文件：`adapter.rs`、`remote/codex_ipc.rs`、设置页、i18n
- 行为：设置项「兼容发送（实验）」默认关；读探测过才能开；发送结果映射账本（accepted→probed / NotSent→failed / Unknown→inconclusive+`delivery_unknown`）；Unknown 不重发。
- 测试：开关关闭拒绝；NotSent 允许一次 fallback；Unknown 不重发。
- commit：`feat(codex-ipc): add opt-in compat send`

### T9 错误码 / i18n / 徽标

- 文件：`remote/codex_ipc.rs`、`src/mobile/*`、`src/i18n/locales/{zh,en}.json`、`docs/capabilities.json`
- 行为：
  - reasonCode：`desktop_build_unverified`、`probe_failed:<stage>`、`protocol_drift_suspected`、`probe_inconclusive`、`delivery_unknown`；
  - 三态徽标：已验证 / 兼容探测（实验）/ 未解锁 + 原因；
  - 探测中骨架 + 可取消；
  - README 能力矩阵注记、`docs/capabilities.json` 说明。
- 测试：i18n key 齐全；`pnpm check:i18n`。
- commit：`feat(codex-ipc): expose compat states and reasons`

### T10 收尾

- 全量门禁（见 §6）；
- 更新设计稿中的实现差异（如有）；
- commit：`test(codex-ipc): add unknown-build coverage`（或按实际拆分）。

---

## 6. 门禁（每个 checkpoint 与最终都要跑）

```powershell
cd E:\LLMproject\Github\tuvis-pr165-review\src-tauri
cargo fmt --check
cargo check --features marker-helper
cargo clippy --all-targets --features marker-helper -- -D warnings
cargo test --features marker-helper
```

```powershell
cd E:\LLMproject\Github\tuvis-pr165-review
pnpm build:mobile
pnpm check
pnpm exec vitest run --no-file-parallelism
node scripts/check-error-codes.mjs
```

已知基线：Windows 本机 5 个 lib 测试失败（HOME 隔离），不是回归；报告里要单独列出。
新增测试必须实际执行；`0 tests` 不算通过。

---

## 7. 真机验收（需要用户配合，一次）

1. 用户打开 Desktop 的一个会话；
2. 让 Codex 执行一条需要审批的命令（例如 `echo hi`，由用户触发）；
3. 手机/网页端出现审批卡：
   - 点「允许」→ Desktop 命令开始执行，卡片消失；
   - 再触发一次，点「终止本轮」→ 回合立即中断，卡片消失；
4. 记录：decision 请求帧、回执、snapshot 中 requestId 消失的时间；
5. 把这条真实快照脱敏存为 `src-tauri/tests/fixtures/codex-desktop-ipc/observed/windows-4894-pending-command.sanitized.json`（作为 T7 的 fixture）。

**没有用户在场，不得触发任何审批/发送。**

---

## 8. 审查点（执行 Agent 到点暂停，交 commit）

1. **T0+T1 后**：结构改动是否只碰必要文件；availability 矩阵测试是否齐；
2. **T5 后**：探测是否真的惰性、单飞、账本命中零帧；force 开关只在 debug；
3. **T7 后**：未知 build 是否只有 accept/cancel；permissions 是否真的一个帧都不发；
4. **最终**：全量门禁 + 真机验收证据 + `git diff --stat`。

Review 不通过时：在原分支追加修复 commit，不 amend 已审 commit，不 force-push。

---

## 9. 停止条件（遇到就停，不要猜）

- `generate-json-schema` 输出与本文枚举不一致；
- Desktop IPC decision 被拒/无回执，且两次修正仍失败；
- 需要改 `MASTER-PLAN.md` 或冻结 spec；
- 需要开放 permissions/plan/问答才能继续；
- 测试连续两次全量失败且原因不明；
- 仓库 HEAD 不是 `0e53cb62` 或工作区出现未预期改动（先停下报告，不要 reset 掩盖）。

---

## 10. 交付报告模板

```text
分支：feat/codex-ipc-unknown-build-compat
基线：0e53cb62fac2e4adf15f019f7630ece49cbde675
commits：
  <sha> <subject>
  ...
改动统计：<git diff --stat origin/main...HEAD 摘要>
门禁：
  fmt / check / clippy / cargo test：<结果>
  pnpm build:mobile / check / vitest：<结果>
  check-error-codes：<结果>
真机验收：<允许/终止 两次结果 + fixture 路径>
未完成/偏差：<逐条>
```
