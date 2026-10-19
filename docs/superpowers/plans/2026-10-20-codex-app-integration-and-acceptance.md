# Codex APP 通用底座抽取、集成与双平台验收实施计划

> **执行代理必读：**使用 `superpowers:executing-plans` 逐任务执行；如用户选择代理分工，再使用 `superpowers:subagent-driven-development`。勾选项用于执行跟踪。本次会话只交付计划，不执行以下实现、分支整理、推送、合并或发版动作。

**目标：**从最新 main 正向抽取可复用且由 Codex 实际使用的 APP 底座、Codex 普通消息 queue/exec 兼容发送和 Desktop IPC，交付一个分层 PR；通过适用的双平台产品验收后合入 main，并整理 #117 的剩余续作分支。

**架构：**冻结 spec 的 F1–F8 保持不变，Desktop Owner IPC 为主路径；只有明确零投递时，普通消息才有兼容路径资格。通用进程、门控、回执、审计等底座与工具驱动分开；不把 #117 其他工具及旧 Codex 控制框架整包带入。能力按精确 build 和本平台证据登记，未解锁交互诚实拒绝。

**技术栈：**Tauri 2、Rust/tokio/rusqlite、React 19、TypeScript、现有 REST/SSE、Vitest、现有 Rust 假 IPC 集成测试；不引入新的运行时或协议栈。

---

## 1. 权威、基线与已确认范围

### 1.1 权威顺序与执行前核对

- 宪法：`docs/MASTER-PLAN.md`。未经用户另行明确授权，不修改。发现冲突先报告，由用户裁决。
- 冻结设计：`docs/superpowers/specs/2026-10-16-codex-desktop-ipc-remote-control-design.md`。本计划补集成与验收，不重开 F1–F8。
- 原实现及验收：`docs/superpowers/plans/2026-10-16-codex-desktop-ipc-remote-control.md`、`docs/superpowers/plans/2026-10-16-codex-desktop-ipc-acceptance.md`。
- 波次 B 原表：`docs/superpowers/plans/2026-10-19-codex-ipc-windows-4894-full-probe-plan.md` §5。本计划 §5 对 E9/E10/E11 的修订优先于旧步骤；保留旧记录的历史含义。
- 本轮发版义务采用整合后 main 的 `AGENTS.md`，包括独立的 `CHANGELOG.en.md`；用户已授权代理翻译受影响英文资产。

本计划写成时已 fetch，读取位置如下；**执行时再次 fetch、记录新 SHA 并重审变化，不把这些值当作可跳过核对的常量**：

| 角色          | 引用与已核对提交                                                      | 用途                                                |
| ------------- | --------------------------------------------------------------------- | --------------------------------------------------- |
| 冻结代码基线  | `codex/codex-desktop-ipc`，`42cac262fd80d4a8e679fe138df75b6b4d48357e` | #165 的实现、已修审查项与已有夹具来源               |
| 正向集成起点  | `origin/main`，`77fcdb487e8bd32eb4109cf8a274e7663a0ef43c`             | 保留 main 的功能、依赖、迁移、错误码及双语契约      |
| 旧 IPC 集成点 | `codex/headless-main-integration@d5cc28b1`                            | 识别 #165 的真实 IPC 增量；#165 当前 base 不是 main |
| #117 原来源   | `feat/h1-h12-headless@289b20db`                                       | 原始提交/混合 hunk 溯源，执行时核对 PR 实际 head    |

### 1.2 共同决定

1. 一个新交付 PR，base 为最新 main；提交按“通用底座／Codex 兼容发送／Desktop IPC／本轮修正与验收”分层。#117、#165 暂保留供追溯，不整包合入 #117。
2. 通用底座按“可复用、由本轮 Codex 实际调用、能够验证”抽取。保留必要接口，不引入未被消费的未来框架。
3. ZCode、DeepSeek Harness、WorkBuddy 驱动、额外 CLI 无头能力、旧 Codex app-server 切档/interrupt 等留在 #117 续作。main 已有其他工具读链路和终端功能照常保留。
4. 补已有 Turn 的 `params.input` 用户开场显示后备；不创建空 session，不改变发送逻辑或投递真值。
5. Unsupported 只延迟前端错误提示；后端即时 contentState、禁写、stale 和屏障不去抖。
6. E11 使用测试专用、默认关闭的确定性停点，命中真实发送响应到达后、动作终态落账前的窗口。
7. E10 分别验 Desktop 在场的 queue 兼容、Desktop 缺席的 exec 兼容，两臂都验 H3 关闭时拒绝。
8. exec 按真实非交互 CLI 语义交付，不承诺继承 Desktop 权限，不承担远程审批，不自动切档、提权或改成自动审批。
9. 保留 PATH CLI 契约；不新增显式路径配置或 App 内置 CLI 自动发现。缺失时诚实拒绝兼容发送；IPC 本身可用性与 CLI 可用性分别显示。
10. Windows permissions 本轮**不解锁**，4894 profile 继续 false；只归档新增审批证据。权限档切换没有本轮实证且在冻结范围外，不做。command 审批不因此关闭。
11. 双平台按各自精确 build 的已解锁能力各完整跑一次适用表；真实手机补一次主路径，不额外承诺“三次重复”。失败项修复后复验。
12. 本阶段先合入 main，随下一常规版本发布；本阶段不单独发正式版、不提前定版本号。新交付合入后，本轮整理只含剩余增量的续作分支，并保留原分支备份与来源。

### 1.3 不做与不可宣称

- 不做模型/模式/权限远程切换、interrupt/steer、文件审批等冻结 spec §5 范围外能力。
- 不把空 Thread 调查升级为创建会话功能；不靠发一句话、空 input Turn、清空旧历史伪造原生空首帧。
- 不因某平台缺少审批样本将其 flag 改为 true；不将 CLI、假 IPC、生产内核、产品 E2E 混写为一个 PASS。
- 本次交付是已解锁能力的阶段收口，不宣称冻结 spec 的双平台 F1–F8 全部真机验收完毕。

## 2. 已完成的前置证据及其设计影响

以下包含本机原帧核对及用户回传的 Mac 报告；Mac 私有原帧尚未被当前 Windows 会话直接读取。归档时标明来源，不把转交报告伪装成当前执行者亲测。

| 项目                | 已有结论                                                                                                                        | 对实现/验收的约束                                                                                       |
| ------------------- | ------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------- |
| Windows Desktop     | 26.1013.4894.0，精确 profile 已登记 send/state/command/blocking_input；permissions=false                                        | 保持精确门，不把 profile None 的旧探针二进制记录当当前事实                                              |
| Mac Desktop         | 26.1013.40934，精确 profile 仅 send/state=true                                                                                  | command/permissions/blocking_input 继续关闭；不复制旧 31256 或 Windows 证据                             |
| Windows queue       | CLI 0.162.1；现有 ignored 单例生产内核通过，约 10.7 秒消费；同 Thread 一条消息、一个新 Turn、完成                               | 支持保留 Codex 兼容内核；HTTP/H3/UI/审计仍须 E10                                                        |
| Windows exec        | Desktop 缺席，生产 npm 垫片的 CLI 直调同 Thread 完成；一条用户消息/回复；约 127 秒，含网络重连                                  | CLI 层正面证据，未覆盖生产 runner；不把 127 秒当固定延迟或调整超时的依据                                |
| Mac PATH CLI        | 独立 npm 0.163.0，arm64；shell 和 shell 启动的影子 Tuvis dev 进程 PATH 可发现                                                   | 实际进程 PATH 契约通过不等于 router 已投递；Finder 启动仍未验                                           |
| Mac queue           | 新独立 CLI 455ms 入队，约 4.1 秒 marker，11.8 秒完成；同 Thread 一次消费、无滞留                                                | 属 CLI + Desktop 消费者证据；60 秒是消费确认预算，不是整轮完成预算                                      |
| Mac exec            | Desktop 缺席，123205ms、exit 0；同 Thread 唯一新 Turn，用户/助手 marker 各一次                                                  | 同样只是 CLI 层，不替代 E10；重连与 HTTPS fallback 单列                                                 |
| Desktop 重启/重开   | Win/Mac 均先观察到 owner 不在场，打开目标 Thread 后恢复；Mac 早期 5 Turn/Loading，后续新连接完整 10 Turn/Ready                  | 恢复必须等当前 owner 的新鲜 snapshot；Loading 不当空历史，不承诺自动持有；后续 Ready 不覆盖早期 PARTIAL |
| Mac 只读收敛        | 最新首 snapshot revision 3，10/10 completed，items 完整、Ready/idle、无 pending、stale=false；未发 hydrate                      | 不把此结果写成“本轮 load-complete-history 已测”；按需补载条件保留                                       |
| exec 有效策略       | 两版均传 on-request，实际 never；Win workspace-write，Mac read-only/managed                                                     | 纠正有效策略承诺；审批与沙箱分别记录，不保证任意任务/工具能成功                                         |
| Windows permissions | 首句前 Default 的新无项目会话真实 requestApproval；原权限对象批准、空对象拒绝、scope=turn；批准文件存在、拒绝无命令且文件不存在 | 有审批双臂证据，不是切权限档证据；本轮按用户选择归档、不开 profile                                      |
| 原生空会话          | app-server 能创建/命名 0 Turn Thread，但 Desktop 列表不可达，无 owner/空 canonical 首帧                                         | 保留原生空首帧缺口，派生夹具只能标派生                                                                  |
| 开场与 Unsupported  | 被引用实体 turnId=null 的过渡 164–382ms；正常正式 userMessage 后到约 515–544ms；clientId 与 params.clientUserMessageId 对应     | null turnId 不作有效 Turn；前端错误提示可延迟；正式项替换后备不靠正文去重                               |
| 忙态负例            | 忙态 start-turn 可返回旧 Turn success，且出现带 input 的临时实体，最终没有新 userMessage                                        | input/回执 success 均不能放行派发屏障、确认新 Turn 或证明送达；产品已有屏障须回归验证                   |

上游静态解释（已读取对应 tag，不修改本地 CLI）：

- [0.162.1 exec 的 headless override](https://github.com/openai/codex/blob/rust-v0.162.1/codex-rs/exec/src/lib.rs#L586-L621)、[0.163.0 同路径](https://github.com/openai/codex/blob/rust-v0.163.0/codex-rs/exec/src/lib.rs#L586-L621)设置 `Some(AskForApproval::Never)`。
- [0.163.0 配置优先级](https://github.com/openai/codex/blob/rust-v0.163.0/codex-rs/core/src/config/mod.rs#L3852-L3867)采用覆盖值优先；[exec 构造分支](https://github.com/openai/codex/blob/rust-v0.163.0/codex-rs/exec/src/lib.rs#L778-L812)对 AutoReview 有条件例外，不能概括为所有版本/配置永远 never。本轮不增加 AutoReview 能力。
- [官方非交互说明](https://learn.chatgpt.com/docs/non-interactive-mode)用于说明 exec 的运行方式，不替代本机实际策略和版本证据。

本机过程材料位置：Windows 仓外 `E:\LLMproject\Github\tuvis-verify-logs\preflight-2026-10-19\`；后续工作区忽略目录 `.superpowers/preflight-2026-10-19/`；Mac 私有持久目录 `~/Documents/tuvis-verify-logs/2026-10-19-macos/` 与 `2026-10-20-macos/`。这些目录不会随 Git 自动分发。Task 1 必须生成可移交的脱敏入库摘要；原帧、用户正文、设备凭据不提交。

## 3. 文件责任与抽取边界

| 层             | 主要文件/目录                                                                                                                                                      | 本轮责任                                                                               |
| -------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------- |
| 共用无头底座   | `src-tauri/src/inject/headless/{mod,runner,receipt,gate,turn}.rs`                                                                                                  | Codex 实际用到的生命周期、并发/超时、版本/协议探针、取消清理、回执和审计接口           |
| 发送共用入口   | `src-tauri/src/inject/{mod,routing,normalize}.rs`；`src-tauri/src/remote/{mod,api,server}.rs`                                                                      | 必要 H3/路由/来源标记/配置接线；保留 main 既有终端行为，不带其他新驱动分支             |
| Codex 兼容     | `src-tauri/src/inject/headless/codex.rs`                                                                                                                           | PATH 发现、presence、queue/exec、消费证据、writer lock 处理与诚实回执                  |
| IPC 传输与状态 | `src-tauri/src/services/codex_desktop_ipc/`                                                                                                                        | 原有 transport/identity/connection/owners/adapter/mirror/history/pending/profile/types |
| 动作控制       | `src-tauri/src/services/codex_control/{mod,router,queue}.rs`                                                                                                       | 单次派发、FIFO、Unknown、fallback permit、native/compat 屏障                           |
| 数据与远程接线 | `src-tauri/src/database/{schema,migration}.rs`、`database/dao/{mod,codex_actions}.rs`、`remote/codex_ipc.rs`                                                       | 主路径动作账本及 API/SSE；迁移按新 main 编号重接，不覆盖历史                           |
| 移动 UI        | `src/mobile/codexIpcApi.ts`、`src/mobile/useCodexThread.ts`、`src/mobile/PendingRequestCard.tsx`、`src/mobile/CodexQueuePanel.tsx`、`src/mobile/SessionDetail.tsx` | 复用已存在 UI；只接本轮路由，不带旧 APP 控制按钮                                       |
| 设置与文案     | `src/components/settings/RemoteSection.tsx`、`src/lib/api/remote.ts`、`src/i18n/locales/{zh,en}.json`                                                              | Codex 实用的 H3/超时/并发、能力/失败/恢复说明；保留 main 新文案                        |
| 新显示小模块   | `src/mobile/useDelayedUnsupported.ts`、`tests/mobile/useDelayedUnsupported.test.tsx`                                                                               | 只管理错误提示延迟，不修改 ThreadProjection                                            |
| 测试专用停点   | `src-tauri/src/services/codex_control/e2e_stop.rs`、`src-tauri/Cargo.toml`                                                                                         | debug + 显式 feature + 精确 action 的一次性停点；默认和发行构建不可启用                |
| 验证           | `src-tauri/tests/{codex_desktop_ipc,codex_ipc_remote,codex_control_remote,headless_e2e}.rs`、`tests/mobile/{useCodexThread,codexIpcApi,SessionDetail}.test.tsx`    | 保留有意义的原测试，按场景分离其他工具依赖                                             |

明确留在续作：`headless/{zcode,zcode_create,wb_acp,dsh_acp,cli_three,codex_app}.rs` 的新增能力，及移动端旧 `ModeBarApp`、APP 交互/新建框架的未被本轮使用部分。共享文件中的对应 hunk 同样不能混入。若 Codex 编译暴露依赖，追到最小被消费接口抽取，而非把整套其他驱动注册回来。

## 4. 执行顺序与检查点

命令工作目录约定：`pnpm` 和 Git 命令在交付工作区根目录执行；未带 `--manifest-path` 的 Cargo 命令在该工作区 `src-tauri/` 执行。运行真实模型的 ignored 测试必须单独点名、绑定测试 Thread；不能执行整套 ignored 测试。本文中的任务勾选项均为未来动作。

依赖顺序：Task 1 → 2 → 3 → 4 → 5；Task 6/7/8 可在集成接口稳定后分别实施；Task 9 全部适用自动门禁通过后进入 Task 10；重新评审、真实验收都通过后才 Task 11 合入；Task 12 整理剩余分支。Task 13 是下一常规版本发布时的门禁，不能在本阶段打 tag。

每个代码任务按“先写能复现目标风险的测试 → 确认失败原因 → 最小改动 → 定向测试通过 → 本任务提交”执行。不要为纯文档/搬运增加镜像实现的无效测试；不要把原有失败重新归因成环境而不验证。

### Task 1：归档前置证据与决策

**文件：**新建 `docs/superpowers/reviews/2026-10-20-codex-app-preflight-results.md`；更新 `src-tauri/tests/fixtures/codex-desktop-ipc/observed/README.md`（仅增加真实新增样本时列样本）；新增经 raw 对照的开场/忙态脱敏样本。

- [x] 将 §1–2 的决定、平台/build/CLI/启动方式、证据层、PASS 与缺口整理入库；旧 4894 报告不改写为“当时已测 permissions”。增加后续补采引用。
- [x] Windows permissions 双臂仅作证据归档，明确“审批应答成功 ≠ 权限档切换”；4894 flag 继续 false。若无必要不增加公开 raw 请求对象。
- [x] 从原始帧提取“正常 placeholder → 真实 turnId → 正式 userMessage”与“忙态临时实体但无新 Turn”样本；脱敏仍保留整数 requestId、数组顺序、null、clientId 对应与时间差。样本与原帧逐字段核对；派生负例命名 `derived`。
- [x] Mac 报告记录为 Mac 执行者回传；原始文件由 Mac 侧持久保存。新完整 snapshot 不伪装成补载 RPC 成功，也不覆盖早期 PARTIAL。
- [x] 提交 `docs: record Codex app preflight evidence and decisions`，只暂存本任务明确文件。

**检查点：**仓库中的摘要可独立解释后续设计；隐私正文/凭据、完整进程环境、仓外探针工程均不入库。原生空首帧仍是 NOT_AVAILABLE。

### Task 2：建正向集成工作区与来源清单

**文件：**新建 `docs/superpowers/reviews/2026-10-20-codex-app-extraction-ledger.md`。

- [x] 记录当前工作树状态，fetch main 与两个源分支、核对 PR base/head。规划文件和未提交文件不得被 checkout 覆盖。
- [x] 按 worktree 技能从**最新 origin/main**建立隔离交付工作区，建议分支 `codex/codex-app-delivery`；本轮规划工作区不直接变成旧分支的大合并。
- [x] 读取新 main 的 AGENTS、依赖锁、数据库迁移和错误码规则，记录 main SHA、两个源 SHA 及 diff 范围。
- [x] 在来源清单逐文件/函数/hunk 记录：来源提交、依赖、归“底座/兼容/IPC/续作/不相关”、迁移后的目标位置、验证方法。重点拆 `remote/api.rs`、`remote/server.rs`、`SessionDetail.tsx`、locale、Cargo/锁文件等混合文件。

只读核对命令（各占一行执行）：

```bash
git fetch origin main codex/codex-desktop-ipc feat/h1-h12-headless
git diff --name-status origin/main...42cac262fd80d4a8e679fe138df75b6b4d48357e
git diff --function-context origin/main..42cac262fd80d4a8e679fe138df75b6b4d48357e -- src-tauri/src/inject/headless/codex.rs
git log --reverse --oneline d5cc28b1..42cac262fd80d4a8e679fe138df75b6b4d48357e
```

- [x] 将 main-only 改动列入保留清单；不能用旧分支整文件覆盖新 main。旧整合计划中“先合 #117 再改 #165 base”的路线标明已被本决策取代。
- [x] 提交来源清单，检查没有代码能力先行混入。

**检查点：**每一项新交付增量有源；每一项不交付增量有留存去向。若发现宪法冲突，停在受影响项并报告，不擅改宪法。

### Task 3：抽取由 Codex 消费的共用底座

**文件：**§3 的共用底座、`src-tauri/src/remote/mod.rs`、`src-tauri/src/lib.rs` 中现有 remote Tauri 命令接线、设置与配置 DTO；测试保留在对应模块和现有 `headless_e2e.rs` 的 Codex 单例。

- [x] 从冻结源搬运已经实现的 runner/receipt/gate/turn 所需代码；先列调用依赖闭包。保留进程树清理、默认并发 2、watchdog 600000ms 及其 clamp，不因本轮延迟样本改常量。
- [x] 接入 H3、超时/并发、回执和既有 `write_audit`；设备身份从 gate 上下文传入，不由 runner 伪造；不额外引入重复审计表。
- [x] `headless/mod.rs` 仅注册本轮实际需要的底座与 Codex。`PermissionSpec` 等共享类型保留实际调用的成员；不为让旧全量测试编译而注册其他驱动。
- [x] 在 `routing.rs` 和远程 API 只加入 Codex 兼容分支；main 已有终端路由及其他工具既有行为保持。设置 UI 仅接本轮用到的配置，不宣传另外三家 APP 可用。
- [x] 定向测试 runner 超时/取消只清理自己的树、并发额度归还、H3 默认关闭、配置损坏 clamp、设备审计字段。复用原有测试，缺失项补失败用例。
- [x] 运行 `cargo test --lib inject::headless`；前端设置改动跑相关设置测试与 `pnpm check:i18n`。预期所选测试无失败，无真实模型/安装探测副作用。
- [x] 提交 `feat: extract shared app execution foundation for Codex`。

### Task 4：抽取并收口 Codex queue/exec 兼容契约

**文件：**`headless/codex.rs`、必要 `routing.rs`、`remote/api.rs`/`server.rs` 接线、`headless/mod.rs` 中 Codex 权限构造、`src/mobile/SessionDetail.tsx`、中英文 locale；更新对应模块测试。

- [x] 搬运现有 Codex UUID/rollout/cwd 绑定、PATH resolver、presence、queue/exec 分派、消费确认、writer lock 与 fence 所需逻辑。不引入旧 `headless/codex_app.rs` 控制接口。
- [x] 保留来源标记单点组装；Windows npm `.cmd/.bat` 的正文、路径等参数仍过同源元字符拒绝，不用原生 exe 绕过生产 resolver。
- [x] 先补契约测试：PATH 缺失为明确不可达；H3 关闭零 spawn；APP 在场走 queue；缺席走 exec；只在允许的明确 writer-lock/零投递分支改道；未知结果不重试。每个测试断言 spawn 次数、route 和账本，不只检查字符串。
- [x] 审计 `PermissionSpec::codex_exec_default`、`exec_resume_argv` 及调用者的现有配置/reviewer分支，形成参数决定记录。旧 `on-request` 不再被称为有效权限保证：只有确认无必要语义后才移除，否则保留兼容输入并明确其局限；两种实现都不得自动改成 bypass、切档或 AutoReview。移除时同步原参数位置测试，保留 `-C` 全局前置及 `--skip-git-repo-check`。
- [x] 修正文案：兼容模式受 CLI 自身权限约束，可能与 Desktop 不同；发送/消费、执行结果、审批受限分别说明。CLI 缺失时不连带假报 IPC 不可用。
- [x] 保留消费 30×2000ms 预算；入队成功不是消费成功，消费成功不是整轮任务完成。真实未知/失败不诱导用户重复发送。
- [x] 运行 `cargo test --lib inject::headless::codex`、`cargo test --lib inject::routing` 及受影响远程端点测试；逐条比较 spawn/receipt/audit/fence。
- [x] 提交 `feat: integrate Codex message compatibility routes`，契约纠偏可独立提交以便审阅。

**检查点：**实际策略不同被如实说明，不承诺任意用户任务均可执行；不增加远程权限操作。Mac Finder/正常启动的发现或拒绝留到 Task 10，不能用 shell dev PASS 替代。

### Task 5：搬运 Desktop IPC 与动作账本，重新接 main

**文件：**§3 的两个 services 目录、`remote/codex_ipc.rs`、数据库 schema/migration/DAO、services/remote/lib 模块注册、必要前端 DTO/卡片/队列；对应三组 Rust 集成测试和移动测试。

- [x] 以 `d5cc28b1..42cac262` 识别 IPC 增量，按来源清单正向接 main。已有完整实现从源搬运，不重造 transport/router/pending 协议。
- [x] 接 services 注册、AppState/RemoteState 初始化、远程路由和 SSE；原认证、设备吊销、no-store、错误码/i18n 与 main 门禁同步保留。
- [x] 将 codex_actions/屏障迁移接到最新 main 序列；测试已有 main 数据库升级和空库。动作 reserve + 起始审计、终态 + 屏障的原子性及 DB 失败零投递规则不变。
- [x] 保留每次写前的 owner generation、context token、目标 Request ID 原类型；Unknown 不回 queued，审批不降级，不暴露原始协议对象。
- [x] Windows 4894 permissions=false、Mac 40934 command/permissions/blocking_input=false；保留其他精确 profile 的原有证据，不全局清零或放宽版本。
- [x] 移动端只接 Codex IPC 投影、审批/问答卡和队列；旧模式/权限/interrupt APP 控件不注册。加载/不完整/Unsupported 与真空会话语义分开。
- [x] 移动端 4 个 IPC 测试已从冻结源补回（70 例）：actionId 生命周期、unknown 锁死、not_sent 重试、SSE 降级、队列撤回/强制继续。
- [x] 分别运行 `cargo test --test codex_desktop_ipc`、`cargo test --test codex_ipc_remote`、`cargo test --test codex_control_remote`，以及 `pnpm exec vitest run tests/mobile/codexIpcApi.test.tsx tests/mobile/useCodexThread.test.tsx tests/mobile/SessionDetail.test.tsx`。预期全部适用用例通过；不得接真实 Desktop 来满足假 IPC 测试。
- [x] 提交 `feat: integrate Codex Desktop IPC control on main`。

### Task 6：已有 Turn 开场用户消息的显示后备

**文件：**`history_view.rs`、`mirror.rs`、上述 observed 夹具及 Rust 镜像测试。

- [x] 给 `TurnRef` 增加 `pub params: Option<&'a Value>`，`HistoryView::turns()` 从当前被引用实体借用 `entity.get("params")`；不扩展它对 null turnId/孤立实体的接纳条件。
- [x] 先写镜像回归：真实有效 Turn 有 input 而无正式 userMessage → 一条用户后备；后续正式项到达 → 同一逻辑消息、正式正文取代后备；跨 Turn 相同正文不合并；键迁移副本不重复；null turnId/孤立实体不产生用户后备。失败必须指向现有漏投影，而不是修改投递状态机。
- [x] 采用如下纯提取函数；它只处理已经由 HistoryView 验明身份的 Turn，`params.input` 非文本块不猜内容：

```rust
fn opening_input(turn_id: &str, params: &Value) -> Option<(String, String)> {
    let client = params.get("clientUserMessageId")?.as_str()?;
    if client.is_empty() { return None; }
    let text = params.get("input")?.as_array()?.iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>().join("\n");
    if text.is_empty() { return None; }
    Some((format!("{turn_id}/opening/{client}"), text))
}
```

- [x] `project_messages` 分两遍处理 canonical 被引用的有效 Turn：第一遍收集已有可渲染 userMessage 的 turnId；第二遍仅在该 Turn 没有正式用户项时加入后备。跨键副本先看到后备、后看到正式项时也必须由第一遍压住后备。
- [x] 后备复用 `ProjectedMessage`：role=user、kind=userMessage、ts=None；ID 使用上方 turnId+client ID。正式 userMessage.clientId 与 params.clientUserMessageId 相等时使用相同投影 ID；不相等时不按正文猜身份，显示正式项并移除该 Turn 的后备。额外 userMessage 保留原 item ID。
- [x] 保持 item ID/逻辑开场 ID 两类去重，继续使用现有 500 条/1MiB 尾窗和显式截断；input 后备不参与 HistoryView 健康、完整性、runtime、送达、动作完成或屏障释放。
- [x] 增加具体用例名 `opening_input_then_user_message_keeps_one_identity`、`opening_does_not_merge_equal_text_across_turns`、`opening_ignores_null_turn_and_unreferenced_entity`、`opening_formal_copy_wins_during_key_migration`。其中正常样本与忙态负例必须来自 Task 1 可追溯的帧，派生修改另标。
- [x] 运行 `cargo test --lib services::codex_desktop_ipc::mirror` 和 `cargo test --lib services::codex_desktop_ipc::history_view`；移动端确认 replace 不额外追加气泡。
- [x] 提交 `fix: project opening user text from existing turn input`。

### Task 7：前端 Unsupported 提示去抖

**文件：**新建 `src/mobile/useDelayedUnsupported.ts`、`tests/mobile/useDelayedUnsupported.test.tsx`；修改 `SessionDetail.tsx` 与其测试，不改后端 contentState。

- [x] 用 Vitest fake timers 写风险用例：不足窗口恢复则不显示；持续错误达到窗口显示；恢复立即隐藏；切会话/卸载不受旧 timer 影响；Loading/Partial 仍即时显示；任何状态均不假显示“暂无消息”。
- [x] 起始窗口取 500ms：覆盖已观察的 164–382ms null-ID 过渡；这是提示窗口，不是协议超时或真值。实现如下 hook，sessionKey 使用当前会话身份，enabled=false/会话关闭时传 active=false：

```typescript
import { useEffect, useState } from "react";

export const UNSUPPORTED_NOTICE_MS = 500;

export default function useDelayedUnsupported(sessionKey: string, active: boolean): boolean {
  const [shownFor, setShownFor] = useState<string | null>(null);
  useEffect(() => {
    setShownFor(null);
    if (!active) return;
    const timer = setTimeout(() => setShownFor(sessionKey), UNSUPPORTED_NOTICE_MS);
    return () => clearTimeout(timer);
  }, [sessionKey, active]);
  return active && shownFor === sessionKey;
}
```

- [x] 在组件顶层无条件调用 hook：active 为 `codexWritable && codexState?.contentState === "unsupported"`；健康提示条件改为 `contentState !== "ready" && (contentState !== "unsupported" || showUnsupported)`。原 Ready+0 的空历史判据、消息数组、按钮禁用、stale 和 pending 直接用原投影。
- [x] 调整原立即断言 Unsupported 的测试，让 fake timer 跨过窗口；补真实短过渡未闪烁的序列，不依赖墙钟 sleep。执行 `pnpm exec vitest run tests/mobile/useDelayedUnsupported.test.tsx tests/mobile/SessionDetail.test.tsx`，再 `pnpm lint`。
- [x] 提交 `fix: debounce transient Codex content error notice`。

### Task 8：忙态回归与测试专用 Unknown 停点

**文件：**`codex_control/router.rs`、`queue.rs` 的必要修正；新建 `codex_control/e2e_stop.rs`，模块注册与 `src-tauri/Cargo.toml`；`codex_control_remote.rs`、`codex_ipc_remote.rs` 及模块测试。

- [x] 先补忙态风险回归：首次镜像已 busy、排队中变 busy、请求前再核 busy、完成的是别的 Turn、旧 Turn success、临时 input 无真实 Turn、Unknown 恢复。对每例断言真实 dispatch 调用次数、action 状态和屏障；不要从探针忙态行为直接推断现有产品必有 bug。
- [x] 回归要求：busy 不发送 start-turn；队首只在绑定 Turn 的可信终态后放行；Unknown/缺有效身份保守屏障不自动解除；审批/问答应答不被普通消息 FIFO 堵住。仅修失败处，不重写已成立的屏障机制。
- [x] Cargo 加默认关闭 feature `codex-ipc-e2e-stop = []`；以下编译门放在始终编译的 `codex_control/mod.rs`，不能放进仅 debug 注册的停点模块中；发行构建显式拒绝这一 feature：

```rust
#[cfg(all(feature = "codex-ipc-e2e-stop", not(debug_assertions)))]
compile_error!("codex-ipc-e2e-stop is only allowed in debug evidence builds");
```

- [x] debug+feature 的停点只在 `dispatch.send_text(...).await` 已返回 Accepted、真实 turnId 非空、进入 `finish_with_turn_barrier` 之前调用。用 `TUVIS_E2E_STOP_ACTION` 精确匹配一个 action；目录固定在 debug `TUVIS_HOME` 下的 `codex-e2e-stop/`，拒绝无影子 home 的启用。不得增加 HTTP 开关、发行设置项或通配所有请求的停点。
- [x] 停点用独占文件 `reached.json` 保存 action/turn/阶段、flush 后暂停；最多 120 秒，出现 `release` 文件或超时即继续原落账。重复同 action 不再停；缺 feature、无匹配或非 Accepted 全部零影响。日志/文件不记录正文或凭据。
- [x] 自动测试停点守卫（不匹配/非 Accepted/无影子数据不启用）、一次性、主动 release、超时收尾；模块逻辑以临时目录测试，不强杀测试执行器。普通集成测试仍用假派发，不消耗模型。
- [x] 构建默认 debug、默认 release 的正向检查；`cargo check --release --features codex-ipc-e2e-stop` 应在编译门明确失败。发布工作流不传此 feature；E11 专用构建标记、源码 SHA 和二进制 hash 单列。
- [x] 执行三组 Codex Rust 集成测试与相关模块测试；提交 `test: add deterministic Codex unknown-delivery checkpoint`。

**检查点：**真实 E11 只有看见精确 `reached.json`、确认动作仍 reserved 后才强杀 Tuvis；未命中/已超时不能宣称取得 Unknown 窗口。

### Task 9：整合自动门禁与复审材料

**文件：**必要的已列实现文件；新建 `docs/superpowers/reviews/2026-10-20-codex-app-integration-review.md`；沿用 main CI 配置，不回退阈值。

- [x] 运行 `pnpm install --frozen-lockfile`，`pnpm test`，`pnpm check`，`pnpm build:mobile`。锁文件仅随真实新增依赖或 main 必要适配变化，不从旧分支覆盖。
- [x] 按整合后的 `scripts/ci-backend-local.sh` 跑同等 feature 的后端门禁；至少包含 `cargo fmt --check`、`cargo check --features marker-helper`、`cargo clippy --all-targets --features marker-helper -- -D warnings`、`cargo test --features marker-helper`，以及 CI 要求的 Windows 目标检查。Mac 原生 Unix、Windows 原生 named pipe 分别验证。
- [x] 失败先归因。旧报告中的其他工具测试失败不能作为新交付豁免；未抽取驱动的测试不得留下悬空编译依赖。保留 main 已有测试覆盖。不得用忽略测试、提高错误码棘轮、取消门禁让它变绿。
- [x] 复核 diff：源 SHA→新提交来源、共用底座实际消费者、权限边界、幂等/Unknown/屏障、迁移、来源标记、主路径与兼容边界、前后端契约、双语值。
- [ ] 形成复审材料，逐项记录命令/平台/结果与未测格；重新评审新 PR 的实际 diff，不以 #165 曾过审代替。

## 5. Task 10：双平台产品波次 B 与手机主路径

**文件：**新建 `docs/superpowers/reviews/2026-10-20-codex-app-product-acceptance.md`；更新原双平台验收表的后续状态/引用，不覆盖旧批次。

### 5.1 运行纪律

- [ ] Windows 4894、Mac 40934 各预约一次完整测试窗口；时间预算不代替通过。若实际 build 漂移，停止该 build 的写验收，重新取得精确 profile 证据；不放宽门。
- [ ] 使用整合后的同一交付提交和受控影子数据；禁止与独立 harness 同时跟随/写目标会话。记录 OS、Desktop build、CLI path/version/hash、Tuvis SHA/hash、启动方式、H3 和设备配对事实。
- [ ] 桌面已配对浏览器完整跑适用表。真实手机只补一次主路径：打开原会话、查看内容、发送一条、观察回执/回复及 Desktop 同会话可见；记录手机平台、网络通道和证据，不将桌面浏览器称为手机。
- [ ] 手动建测试项目、选权限/Plan、正常重启 Desktop 由用户在选定时窗完成；不自动操作禁止的安全设置或关闭承载工作的宿主。
- [ ] 每格独立 marker/actionId，先 idle、无 pending、目标身份确定；上一格未知不重发。记录 action/turn（脱敏）、DB/审计、UI截图、Desktop 正史与时序。修复后只重跑受影响格及其依赖链，不机械重复所有已通过检查。

### 5.2 E1–E12 修订验收表

“关闭态通过”只表示能力门/拒绝正确，**不表示该交互功能 PASS**；功能状态仍记关闭/未验。两个平台都不得把 N/A 或缺口涂成成功。

| 格                   | 操作与判据                                                                                                                         | Windows 4894                | Mac 40934                                            |
| -------------------- | ---------------------------------------------------------------------------------------------------------------------------------- | --------------------------- | ---------------------------------------------------- |
| E1 能力显示          | 有 owner/无 owner、IPC 可用/不可用、CLI 可发现/缺失分别核四档及具体能力；文案与真实状态一致                                        | 必验                        | 必验；记录实际启动 PATH                              |
| E2 连接跟随          | 官方进程身份、initialize、owner、两个专用 Thread 跟随不串台；新鲜 snapshot、非 stale                                               | 必验                        | 必验；本轮前置未测多 Thread 不能直接勾               |
| E3 普通发送          | HTTP→router→IPC；accepted 与账本一致，同原 Thread 恰一个新 Turn，Desktop 可见，来源标记一次                                        | 必验                        | 必验                                                 |
| E4 streaming/history | 真实流式 replace/追加、已有 Turn 开场后备→正式项、前端错误提示去抖；Ready/Loading/Partial/Unsupported 各守语义；必要时按需 hydrate | 必验                        | 必验；原生空首帧仍登记缺口                           |
| E5 队列接力          | 忙态连发两条，首条精确 Turn 完成才派下一条；busy 期间 start-turn 计数不增加；一次消息只落一次                                      | 必验                        | 必验                                                 |
| E6 command           | 两个独立命令审批场景，批准有真实效果、拒绝无效果，pending 消失；绑定原请求与 Turn                                                  | 功能必验                    | flag=false：只验关闭态，不强求触发                   |
| E7 permissions       | 当前两平台均关闭；存在/构造请求也不得可操作或绕过服务端门。新增 Windows 真机证据仅归档                                             | 关闭态必验；功能不验        | 关闭态必验；功能不验                                 |
| E8 Plan 阻塞问答     | 用户本地先进入 Plan；真实 blocking request 原 qid/选项作答，原 Turn 继续；非阻塞通知不算                                           | 功能必验                    | flag=false：只验关闭态                               |
| E9 恢复              | 正常重启 Desktop：旧 owner/token 作废；未打开 Thread 时诚实等待/不可控；用户打开后新鲜 snapshot 才恢复；未写重放                   | 必验；不承诺 2 秒或自动持有 | 必验；历史可先 Loading，最终内容完整收敛；分阶段留证 |
| E10 兼容双臂         | 见下文两种明确前提；每臂 H3 开成功与 H3 关零派发拒绝，审计/route/fence 一致                                                        | 两臂必验                    | 两臂必验；使用实际可发现独立 CLI                     |
| E11 Unknown          | 专用停点命中真实 Accepted 响应、未落终态；只强杀影子 Tuvis；重启 reserved→unknown，屏障在册、无自动二投                            | 必验                        | 必验                                                 |
| E12 撤回             | 队列中未派发项撤回；UI、持久动作状态及审计一致；正在派发/已接受项不冒充撤回成功                                                    | 必验                        | 必验                                                 |

**E10 两臂：**

1. Desktop 在场且持有测试 Thread，IPC 在派发前明确不可用/NotSent（使用已有测试注入或受控拒连构造，不破坏真实服务）→ H3 开，只发一次 queue，消费/Turn/审计对应；H3 关零兼容派发。不得把已写后断连的 Unknown 当这个场景。
2. Desktop 确实缺席，CLI 可发现 → H3 开，只发一次 exec，原 Thread 追加/终态、有效策略/沙箱、Desktop 重开后打开会话可见；H3 关零兼容派发。127/123 秒前置延迟不改变 600 秒 watchdog。

Mac 正常 GUI/Finder 启动另列启动子格：核实该 Tuvis 实际 PATH；存在 CLI 时验证发现，不存在时明确不可达、零 spawn。不要只用 launchd PATH 推断结果，不静默使用 App 内置 CLI。可使用可隔离数据的实际 debug 应用验证；不能在未隔离时为此启动 release 写真实账本。若正常启动无法安全实测，保留待验，不写成通过；交付文档须明确已有证据只覆盖哪种启动方式。

**E11 精确步骤：**

1. 专用 feature debug 构建运行于影子 TUVIS_HOME，只为本次 action 设置停点；独立观察端确认 reached 标记、action/turn 匹配、账本仍 reserved。
2. 在停点 120 秒内强杀的对象仅是本次影子 Tuvis，不杀 Desktop、模型或其他用户任务。未命中停点不得做“发送后立即杀”的竞态替代。
3. 重启普通无停点构建，检查 Unknown、屏障及正史；等真实 Turn 完成仍不自动重投未知 action。仅后续明确的人工作用（force-continue）可解除对应屏障，须显示原文/目标及重复风险并留审计；解除不能重发原 action。
4. 调用原 actionId 重试仍返回原状态；新 action 在屏障解除前不派发。记录原消息恰一次、审计恰当、无 queue/exec 自动补投。

**完成条件：**每个平台全部适用功能格与关闭态门控通过；真实手机主路径通过；失败修复并复验；缺口单列。不得把权限关闭和原生空首帧缺口作为“全 F1–F8 已验”的依据。

### Task 11：新交付 PR 复审、合入 main

**文件：**来源清单、复审报告、产品验收报告、受影响 README/手册/能力矩阵；新 PR 描述。

- [x] 推送前检查当前分支的授权与项目纪律；当前任务只写计划，不在本会话推送。执行阶段按用户已确定的新交付 PR 路线创建 PR，base=main，提交层次可审阅。
- [x] PR 描述重写为最终交付边界：共用底座+Codex、两种兼容前置、精确 build 能力、permissions 关闭、空首帧缺口、exec 权限语义、真实验证层级。链接来源 #117/#165，不沿用旧“包含四家 APP 全部可用”的文案。
- [x] 新 PR 创建后附着到执行聊天；#117/#165 保留供来源追溯，注明替代关系与未交付范围，不把旧 PR 对齐 main 当作本轮整合路线。
- [ ] 主分支发生新变化时按最新 main 解决冲突、复核语义和真实 diff，重跑受影响检查与必要验收；新 PR 再评审通过、所有适用产品格满足后才合入。
- [x] 更新项目功能文档/能力矩阵反映实际交付；本阶段不打 tag、不开独立正式版。发布文档最终版本小节和全量覆盖检查放在下一常规版本的 Task 13。

### Task 12：整理 #117 剩余续作分支

**前提：**新交付 PR 已合入 main；不在整合尚未定案时重写原来源分支。

- [x] 保存原 #117 head 的本地可恢复备份分支，建议 `codex/backup-pr117-20261020`，记录完整 SHA；不删除旧分支，不 force-push。
- [x] 从新 main 建 `codex/headless-remaining`，按 Task 2 来源清单重放剩余工具与其他剩余增量。混合提交按 hunk 分离；不能简单 rebase 整条旧历史或只凭 commit hash/patch-id 自动认定重复已去除。
- [x] 对每个原 hunk 明确“已由新 main 交付／续作保留／因 main 等价变化不再需要”及其依据。未交付的 ZCode/dsh/WB、其他 CLI、旧 Codex 控制与不相关修正有明确去向，不静默丢失。
- [x] 做必要接口对齐，使剩余代码建立在已抽出的底座上；不把公共底座和本轮 Codex 再次重复引入，不承诺本阶段把其他工具修好或合入。
- [ ] 运行剩余分支的编译与相关测试，记录真实失败和未验能力。若其他工具仍失败，报告位置及来源，不能为绿色删除实现或把产品状态写可用。
- [ ] 产出 `docs/superpowers/reviews/2026-10-20-pr117-remainder-handoff.md`，记录备份、续作、来源对应、接口变化、检查和下一次入口；旧 PR 后续是否更新/关闭由具体续作安排决定，本计划不要求覆盖旧远端历史。

### Task 13：下一常规版本的发版文档与双语门禁

**文件：**`CHANGELOG.md`、`CHANGELOG.en.md`、`docs/release-notes/` 下最终确定版本文件、`README.md`/`README.en.md`、`docs/capabilities.json`、locale、`docs/user-manual/{zh,en}/08-mobile-remote.md` 和受影响故障排查/快速开始章节。

- [ ] 确定实际上一正式 tag 和本次 release HEAD，逐 commit 归类全量变更；不能仅扫描本计划 PR。先交用户确认无遗漏，再核对文档覆盖。版本号随常规版本整体清单确定，不预填一个新版本。
- [ ] 第一轮覆盖检查：全部变更进入中文 CHANGELOG；用户可感知变更覆盖 release notes；默认值/设置/启动方式/兼容权限语义写升级须知；README 概览和相关章节反映现状；capabilities.json 与 README 矩阵同步。
- [ ] 第二轮双语检查：locale 键和值、README、手册、独立 CHANGELOG.en.md 按中文 diff 逐条对照；`pnpm check:i18n` 只校键，不代替英文值检查。按用户授权代理翻译；CHANGELOG 两侧各自单语、版本/日期/分区对齐；release notes 保持纯中文。
- [ ] 用量/工程项也归入覆盖检查，不能因与 Codex 无关漏掉。此次 evidence 不支持的能力不在发布宣称中解锁。
- [ ] 两轮检查均通过，且实际发版操作授权与常规流程满足后才 tag/发布；未满足不发布。本计划写成不构成执行发布动作。

## 6. 复审与计划完成判据

- [ ] 逐项对 F1–F8：F1→Task 5/E1–2；F2→Task 6–7/E4；F3→E3/E5/E12；F4→E6 与 E7 关闭态；F5→Windows E8/Mac 关闭态；F6→E9；F7→Task 4/8/E10–11；F8→E1 与文档。未解锁项不是功能验收完成。
- [ ] 核对 §1 全部用户决定都有任务承接；特别检查不整包 #117、剩余分支本轮整理、只随常规版本发布。
- [ ] 代码中的新标识、模块、配置和测试名与计划一致；例示开场 ID 只用于投影，不偷偷变成投递身份。
- [ ] 计划阶段完成的产物只有本文；实现、发布和分支整理勾选项均应仍未勾选。此后执行者须按提交和证据逐项更新，不将计划的预期输出记为已运行结果。


---

## 执行记录（2026-10-20，交付分支 `codex/codex-app-delivery`）

- 工作区：`E:\LLMproject\Github\tuvis-codex-app-delivery`（worktree，base `origin/main` `77fcdb48`）。
- 提交：`67b01a13`（Task 1/2 文档与夹具）、`d7d1d732`（Task 3/4 底座+Codex 兼容）、
  `d6d2d348`（Task 5 IPC+移动端+Task 6–8 修正）、后续 `docs:` 提交（Task 9/10/12）。
- 门禁：`cargo fmt --check`、`cargo clippy --all-targets --features marker-helper -- -D warnings`、
  `cargo test --features marker-helper`（2631 passed / 5 例为 main 基线既有 HOME 隔离失败，
  已在 pristine main 复核）、三组 Codex 集成 42/50/34 全绿、`pnpm test`（150 文件/1589 例）、
  `pnpm check`、`pnpm build:mobile`、`pnpm check:i18n`、`check-error-codes` 全绿。
- Task 6：`TurnRef.params` + `opening_input` + 两遍投影；4 个具名用例 + 2 个 observed DERIVED 样本。
- Task 7：`useDelayedUnsupported`（500ms）+ 5 个 fake-timer 用例 + 详情页健康提示接线。
- Task 8：`codex_control/e2e_stop.rs`（4 单测）+ release 编译门；忙态矩阵由既有 34 例控制面测试与
  `codex_ipc_remote` 的 busy/FIFO/Unknown 用例覆盖（逐项对应见 Task 9 复审材料）。
- Task 10：自动化层全绿；**PRODUCT 适用表未执行**（无用户在场的 Desktop 会话操作/审批/重启窗口，
  也无真实手机），按纪律不冒充通过；交接顺序见 `2026-10-20-codex-app-product-acceptance.md` §4。
- Task 11：PR 在独立复审与修复后创建；本阶段不打 tag、不发版。
- Task 12：续作 handoff 已入库（`2026-10-20-codex-app-remaining-handoff.md`）；#117/#165 原分支保留。
- PR：https://github.com/jarvislee90s-dot/tuvis/pull/171（base main；复审后创建）。