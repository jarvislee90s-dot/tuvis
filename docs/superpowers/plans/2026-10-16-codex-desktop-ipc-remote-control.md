# Codex Desktop IPC 远程控制实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 Windows/macOS 上通过官方 Desktop owner IPC 控制原生 Codex Thread，并仅对确定未投递的普通消息保留 PR #117 兼容兜底。

**Architecture:** Desktop IPC 独立于 Headless，传输后端分平台，共享协议、连接、owner、状态镜像及 pending request。`CodexControlRouter` 决定主路径或兜底资格；持久化动作账本和会话串行调度保证恢复、超时及降级不自动双投。Remote API 复用现有设备门禁，移动端只消费 Tuvis 投影。

**Tech Stack:** Tauri 2、Rust 2021 / MSRV 1.89、Tokio full、serde_json、rusqlite、axum 0.8、React 19、TypeScript、Vitest。使用现有依赖，不引入 Farfield 运行时或新的 IPC sidecar。

**Spec:** `docs/superpowers/specs/2026-10-16-codex-desktop-ipc-remote-control-design.md`（2026-10-16，含用户同意纳入 command / permissions 两类审批的范围修订）。最高依据始终为 `docs/MASTER-PLAN.md`。

## Global Constraints

- 「Desktop IPC **不归入 Headless**」。
- 「Windows：Named Pipe，`\\.\pipe\codex-ipc`」。
- 「macOS：Unix Domain Socket，`$CODEX_HOME/ipc/ipc.sock`，默认 `~/.codex/ipc/ipc.sock`」。
- 「未经验证的 Desktop 版本或方法不得盲目启用对应能力」。已验证 build 精确为 Windows `26.1013.3656.0`、macOS `26.1013.31256`，不是版本下限。
- 「Pending Request ID 必须按不透明标识处理，兼容整数和字符串；不得假设 `availableDecisions` 始终存在」。
- 「实际：`ownerClientId`、`targetClientId` 必须由 Tuvis 根据当前 Owner Registry 解析」。HTTP 写请求不接受这两个字段。
- 「禁止在“不确定请求是否已经送达”的情况下自动切换到 PR #117」。`UNKNOWN_DELIVERY` 同样禁止自动重试原 IPC 动作。
- 「IPC 原始 state 不直接进入远程 API」。SSE / REST 使用同一 Tuvis 投影，IPC endpoint 不代理到网络。
- 「不得记录 API key、cookie 等敏感凭据」。审计不存原始 IPC、问题答案或权限原文，仅存关联标识和结果。
- 「interrupt / steer」「update-thread-settings」「远程切换 collaboration mode」「file approval」「更复杂的 approval decision」不在本期；不借实施补入。
- 中文计划与注释；用户可见新增文案使用中英 i18n。沿用最新 main 的错误码门禁，不能通过扩大基线掩盖新裸中文。
- 宪法 R2.3：忙 Thread 普通消息留队，空闲按序发送，不抢占正在运行的 Turn；审批/回答不能被普通消息队列堵住。
- 不改 `docs/MASTER-PLAN.md`，不接触用量采集实现/真实账本，不改 CLI 屏读和键位协议。IPC 的 APP 原生状态权威源为 owner snapshot；TUI 卡继续以屏读为准。

## Review Focus

1. 帧只写出一部分或回包丢失：记未知投递，断线重连、HTTP 重发和进程重启都不能自动双投（Task 3、7）。
2. owner A 的迟到响应/patch 在 owner B 接管后到达：旧 epoch/generation 数据不得污染新镜像或处理新审批（Task 4、5、6）。
3. 字符串 `"12"` 与整数 `12`、浏览器大整数精度及旧审批卡：标识无损往返，过期上下文必须拒绝（Task 1、6、8、9）。
4. `turns=[]`、乱序/缺帧 patch、超限状态：历史不空白，坏增量原子拒绝并重拉 snapshot，恢复前不执行交互（Task 5）。
5. 无头开关关闭、未知 Desktop build、Default 异步问题及旧 APP 补投入口：能力诚实，原生问答/审批绝不走 queue 文本补投（Task 7、8、9）。

---

## 依据、边界与分支方案

### 已核对的证据

- 本次已通过 `read_thread(turnLimit=10)` 阅读「远程控制方案评估」全部可用 5 轮 / 10 条消息摘要（无更早分页）；最后结论是共享控制层、独立 transport、两类审批分别适配、Mac Plan 阻塞问答并入 E2E，不再开独立 Probe 阶段。
- Mac 本机报告：`probe/mac-ipc-probe-report.md`。Windows 用户会话附件 `probe-report.md` / `probe.zip` 已读，包括 `harness/thread_state.py` 与 Windows start-turn、审批、问答证据。附件临时路径不作为执行依赖；下文已经记录协议必需字段，实施时创建脱敏最小夹具。
- Windows Probe：command `accept` / `decline`、Plan blocking input、重启、多 Thread 共 owner 均 PASS。Mac Probe：连接、following、owner、start-turn、permissions approve/deny PASS；command 变体、Plan blocking input、重启未实测。Default async question 是消息，不是 pending request。
- 2026-10-16 fetch 后：#117 OPEN，head `289b20db4d6bdfba50ecfdeb67f1ef6f47ab583f`（`feat/h1-h12-headless`）；main `d3fef5f`；merge-base `c5ffcd3`；`origin/main...#117` 为 **76 / 49**。PR 净差异 71 文件、约 5.49 万新增行，不能视为单独的 Codex 补丁。
- `git merge-tree --write-tree --name-only --no-messages HEAD origin/main` 预演仅列出 `tests/mobile/SessionDetail.test.tsx` 冲突。**这只证明文本合并冲突面，不证明语义正确或检查通过。**
- 工作区存在其他未跟踪的宠物素材和计划；当前 spec、probe 也未跟踪。不得 `git clean` / `reset --hard` / 全量 stash / `git add .`；只显式携带本任务文档。
- 本 checkout 无 `.trellis/`，不运行不存在的 Trellis 脚本，不为本任务新建 Trellis 体系。

### 决定：从 #117 建集成分支 merge main，再建 IPC 分支

本计划只产出文档，**尚未操作分支、merge/rebase、commit、push 或 PR**。执行期流程：

1. 按 `superpowers:using-git-worktrees` 在新 worktree 从 #117 当时的 head 创建 `codex/headless-main-integration`；记录实时 SHA，与上述快照有变化时重算冲突及差异。
2. 在该临时集成分支 `git merge --no-ff origin/main`，保持公开 #117 历史，不 force-push、不在原分支 rebase。语义整合通过 Task 0 门禁后得到集成点 **I**。
3. 从 **I** 再开 `codex/codex-desktop-ipc`，其父依赖是完整 #117 + 最新 main。IPC 提交不塞回 #117。
4. 交付时报告集成分支和 IPC 分支 SHA；默认只本地提交，推送/创建 PR 由用户后续指令决定。建议 #117 维护者先吸收集成点 I 的 main 适配并完成验收，再合并 #117；IPC PR 暂以集成分支为基，#117 入 main 后改目标 main 并检查真实 diff。若 #117 squash 合并，使用 `git rebase --onto origin/main I codex/codex-desktop-ipc` 仅搬 **I 之后的 IPC 提交**，不得重放整条 #117 历史。

不建议直接从 main cherry-pick `a7a734a`：它依赖 H3/H4/H6、路由、其他工具模块、normalize 与后续安全修复。也不建议重写 49 个已公开提交，只为获得线性历史。若用户最终不要整条 #117，应另立“抽取 Codex fallback”计划，不能悄悄在本计划中裁掉其他工具代码。

### 可复用提交与代码边界

| 提交/现有位置                                                               | 本次使用方式                                   | 原因/限制                                                                                                                        |
| --------------------------------------------------------------------------- | ---------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------- |
| `7372fa0`、`ac0d524`、`3a0fcb6`                                             | 随完整 #117 保留                               | H3 无头开关、runner/receipt/gate、无头路由基础；非独立 cherry-pick 集合                                                          |
| `a7a734a`；`inject/headless/codex.rs`                                       | **直接调用最终版本 fallback**                  | `resolve_thread`、`production_rollout_path`、queue/exec 分派与消费确认；不复制初版文件                                           |
| `47f0cdb`、`3ec5f4a`、`ccdfc6a`、`36e4f65`、`5012d75`、`a468a0b`、`289b20d` | 必须保留最终效果                               | 安全判据、诚实回执、读侧、RAII、APP 审批门、cmd 拒绝面；混合其他工具修复，不能按标题只捡 Codex 原功能                            |
| `remote/api.rs::codex_headless_dispatch_envelope`                           | 兼容分支唯一入口                               | 保留其 H3、探针、审计、可见性与回执；传原始用户文本，签名只组装一次                                                              |
| `833f72c`、`b8c55cd`；`headless/codex_app.rs`                               | 借鉴测试缝、错误分层、超时                     | 它是 stdio JSON-RPC app-server，不是 Desktop IPC；不可复用其 transport/envelope 或能力探针                                       |
| `3efb714`、`8506418`、`02dec76`                                             | 保留 CLI/其他工具；替换 Codex APP 原生交互入口 | 旧 `QuestionCardApp` 是自然语言补投；新 spec 禁止用它回答原生 pending。保留 mode 的旧能力，不新增切档功能                        |
| `headless/turn.rs` / `inject/queue.rs`                                      | 借鉴 RAII、短锁和生命周期                      | 不让 Desktop 写操作被 headless 全局并发额度限制；共用会话映射但不借用会被 fallback 再取得的锁造成重入死锁                        |
| `remote/server.rs::RemoteState`、`gate.rs`、`SseRegistry`                   | 直接扩展                                       | 同一 cookie/设备权限、吊销即断流；本机也不免认证                                                                                 |
| `database/dao/write_audit.rs`、`DeviceStore::memory()`                      | 复用表/测试缝，增加 checked 写口               | 旧 record_conn 吞写入错误，不能用于本期投递前强事务；新增 Result 写口，旧调用兼容。关联 ID 放结构化 summary，不改全仓审计 schema |
| main `22e9680` 至 `5aca6e5` 的错误码批、`scripts/ci-backend-local.sh`       | 集成时完整保留并适配 #117 新代码               | #117 旧分支的新增裸中文不会凭文本自动合并变合规；不得回退 main 门禁                                                              |

### 协议与工程值（实现决定）

| 项目              | 决定                                                                                                                                                                                                                                                                                                |
| ----------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 帧                | 4-byte LE u32 长度 + UTF-8 JSON；协议上限 **268435456 bytes**，Tuvis 本期本地接收预算 **16 MiB**，0 或超过任一上限先拒绝再分配，诚实报告 resource_limit 并关闭该连接，不能冒充无 owner。JSON 解析深度保留 serde 默认限制                                                                            |
| Envelope          | `request / response / broadcast / client-discovery-request / client-discovery-response` 五种；没有 notification envelope                                                                                                                                                                            |
| 方法版本          | initialize 不依赖 version；owner-discovery v1；following-changed/status-requested v1；state-changed v11；start-turn v2；load-complete-history、command-decision、permissions-response、submit-user-input v1；client-status-changed v0；ipc-connection-reset v1                                      |
| hostId            | discovery/following 的 **params** 使用 `hostId:"local"`；本地 follower request **envelope 不发 hostId**。若构造器显式使用 envelope hostId，只有 follower 方法按基值 +1；测试钉住两种位置，不能混淆                                                                                                  |
| capability 初始值 | Windows：send/state/command/blocking-input=true，permissions=false；Mac：send/state/permissions=true，command/blocking-input=false。false 表示本 build 尚未活体验证，并不表示协议实现不存在。Task 10 经真机 PASS 才修改相应 profile 常量                                                            |
| 身份              | 读运行中的官方 Desktop 可执行文件/bundle 版本并与本机 endpoint 归属关联；不靠 `clientType==desktop`（Mac 无此字面量），不把 initialize 当 capability 协商。版本来源缺失/多义即关闭 A 写能力                                                                                                         |
| transport         | Windows Tokio async named pipe（Overlapped），不得同步单 handle 一读一写；Mac Tokio UnixStream。优先 CODEX_HOME socket，确实不存在才检查旧 tmpdir 路径；拒绝非同 uid、非 socket、宽权限目录/socket，不更改用户权限                                                                                  |
| 有界资源          | outbound 64 帧且总字节 1 MiB；每连接 pending RPC 64；following 仅当前看板/详情及有本地待发消息的 Thread，最多 64；单 Thread 镜像 16 MiB、全体镜像 64 MiB（含应用 patch 的临时副本预算），不足则旧视图标 stale、停该 Thread 同步，投影正文尾窗 1 MiB、最多 500 条并标 truncated；不扫所有历史 Thread |
| 超时/恢复         | connect 3s；initialize 3s；discovery/read/write-ack 15s（覆盖 Desktop 的 discovery 等待）；自动重连 1/2/4/8/16/30s 上限，成功归零。只自动重试连接和只读同步，绝不重试状态改变 RPC                                                                                                                   |
| 四类 ID           | Tuvis action UUID；IPC envelope UUID；每次新 Turn 的 clientUserMessageId UUID；Desktop pending id 为 string/integer。三种 UUID 独立生成；重试同 action 不生成新 clientUserMessageId                                                                                                                 |
| Patch             | 数组路径，支持实测 add/replace/remove；严格索引/父节点校验，整批 clone 后原子应用。非法 op/path 或 revision 缺口→镜像 stale、重发 following 请求 snapshot；不照搬 Probe 的容错跳过/数组越界 append                                                                                                  |
| 审批              | command approve→`decision:"accept"`，reject→`"decline"`；permissions approve→回显当前 pending 的原 permissions、`scope:"turn"`，reject→`permissions:{},scope:"turn"`；远端不得上送自定义 permissions                                                                                                |
| User input        | `thread-follower-submit-user-input {conversationId,requestId,response:{answers:{qid:{answers:[label或自由文本]}}}}`；只处理 blocking pending，原问题 ID 为键，不按显示顺序当 ID                                                                                                                     |

## 文件结构与公共接口总约定

下述路径均相对仓库；Task 的 Files 只列其责任。`src-tauri/src/services/mod.rs` 注册两个平行服务，不移动已有 Headless。生产文件保持按职责拆分；测试模块可以另文件，不能继续向 1 万行 `remote/server.rs` 堆新域的全部测试。

| 新建位置                                                                                 | 职责                                                                          |
| ---------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------- |
| `services/codex_desktop_ipc/{mod,types,profile,codec}.rs`                                | 平台无关类型、profile/版本门、帧                                              |
| `services/codex_desktop_ipc/transport/{mod,windows,unix}.rs`                             | 本机 endpoint 解析/连接/平台身份                                              |
| `services/codex_desktop_ipc/{connection,owners,mirror,pending,adapter}.rs`               | 单连接 actor、owner、revision mirror、交互映射、门面                          |
| `services/codex_control/{mod,router,queue}.rs`                                           | 路由、持久化动作串行调度；不依赖 remote HTTP 类型                             |
| `database/dao/codex_actions.rs`                                                          | 原子动作登记/派发/结果、持久队列与重启恢复                                    |
| `remote/codex_ipc.rs`                                                                    | 认证后的 HTTP/SSE DTO、fallback bridge、审计及状态装配                        |
| `src/mobile/{codexIpcApi,useCodexThread,PendingRequestCard}.ts(x)`                       | 原生协议投影 fetch/hook、两类审批与阻塞问答卡                                 |
| `src-tauri/tests/{codex_desktop_ipc,codex_control_remote}.rs`、`tests/mobile/*.test.tsx` | 本地假 IPC 集成、HTTP 门禁和移动端验收                                        |
| `src-tauri/tests/fixtures/codex-desktop-ipc/`                                            | 精简脱敏 snapshot、patch、command、permissions、blocking-input 夹具及来源说明 |

公共类型（字段 camelCase serde、枚举值 snake_case；字段和跨模块方法为 pub，按需要 derive Debug/Clone/PartialEq/Serialize/Deserialize；实现位置 `types.rs`，后文引用不重定义）：

```rust
struct ThreadKey { host_id: String, thread_id: String } // 服务端仅构造 local
enum PendingRequestId { String(String), Integer(serde_json::Number) } // untagged；拒 float
struct OwnerLease { key: ThreadKey, client_id: String, epoch: u64, generation: u64 }
enum Delivery { NotSent, Accepted, Rejected, UnknownDelivery }
enum ActionKind { Send, CommandApproval, PermissionsApproval, UserInput }
struct ActionReceipt { action_id: String, delivery: Delivery, route: ControlRoute,
    turn_id: Option<String>, envelope_id: Option<String>, reason_code: Option<String> }
enum ActionState { Queued, Reserved, Accepted, Rejected, Unknown, Cancelled }
struct CodexActionView { action_id: String, state: ActionState, position: Option<usize>,
    receipt: Option<ActionReceipt> }
struct CodexQueueView { actions: Vec<CodexActionView>, blocked_reason_code: Option<String> }
enum ControlRoute { DesktopIpc, HeadlessCompat, None }
enum ControlAvailability { Controllable, SendOnly, CompatOnly, Unavailable }
struct CapabilitySet { send: bool, state: bool, command: bool, permissions: bool, blocking_input: bool }
struct ThreadProjection { thread_id: String, revision: Option<u64>, stale: bool,
    availability: ControlAvailability, capabilities: CapabilitySet,
    runtime_status: String, messages: Vec<ProjectedMessage>, truncated: bool,
    pending: Vec<PendingView> }
struct ProjectedMessage { id: String, role: String, kind: String, text: String, ts: Option<i64> }
enum PendingKind { CommandApproval, PermissionsApproval, UserInput, Unsupported }
struct PendingView { request_key: String, context_token: String, kind: PendingKind,
    actionable: bool, disabled_reason_code: Option<String>,
    command: Option<String>, cwd: Option<String>, reason: Option<String>,
    permission_summary: Option<String>, questions: Vec<QuestionView> }
struct QuestionView { id: String, header: String, question: String, is_other: bool,
    is_secret: bool, options: Vec<QuestionOption> }
struct QuestionOption { label: String, description: String }
enum PendingAction { Approve, Reject, Answer(std::collections::BTreeMap<String, Vec<String>>) }
```

`PendingRequestId` 不作为 JSON number 发给浏览器：`request_key` 为 `n:<十进制整数>` / `s:<原字符串>` 的无损可逆字符串。`context_token` 是服务端保存的随机 UUID，绑定 boot/epoch/generation/thread/request；不包含 owner 文本。客户端只回显 key/token，真实 lease 和权限对象仍由服务端查找。revision 的每次流式变化不让同一个 pending 的 token 无谓失效；request 内容变化、消失/重现或 owner 变化必须换 token。

未知 pending 使用 `PendingKind::Unsupported/actionable=false/disabledReasonCode=capability_unavailable`，只显示“此请求暂不支持远程处理”；不暴露 raw method/参数对象。已知但 profile 未验证也 actionable=false。`ActionKind` 只用于真正可执行的动作，不承载未知协议类别。

## Task 0：准备独立集成基线（先于功能实现）

**Files:** Modify：仅 main/#117 实际冲突文件、main 错误码门禁覆盖的 #117 新增代码；显式携带本 spec/plan 和 Mac 报告。不要暂存其他未跟踪文件。

**Interfaces:** Consumes：#117 最新 head + `origin/main`；Produces：门禁通过的集成提交 **I**、独立 IPC 分支，后续 Task 不以旧 `289b20d` 作实现基线。

- [x] **Step 1：读取 worktree skill 并创建集成 worktree**，执行上述分支流程，保存基点 SHA 和 `git status --short`；不切换/清理当前用户 checkout。
- [x] **Step 2：执行 merge 并解决文本及语义冲突**。`SessionDetail` 保留 #117 的 APP 分套/回执与 main 的文件产物、错误码/i18n 行为。main 新版本号/宠物等已合并资产以 main 为准，不从本工作区拷贝旧副本。
- [x] **Step 3：运行现有门禁找基线失败**：`pnpm install --frozen-lockfile`、`pnpm test`、`pnpm check`、`pnpm build:mobile`、`bash scripts/ci-backend-local.sh`。逐项记录非零输出，尤其运行 `node scripts/check-error-codes.mjs`（若 main 已将其并入 check，仍记录该 gate 的结果）。
- [x] **Step 4：修复合流导致的错误码与测试漂移，再运行失败的门及完整基线门禁**。不降低 main 扫描覆盖率、不加历史基线豁免来遮新代码；平台环境缺项如实列出，不能算 PASS。
- [x] **Step 5：提交集成适配并记录 I**，在 I 创建 `codex/codex-desktop-ipc`。保存不含真实家目录数据的基线结果。提交信息 `chore: integrate headless baseline with main`。

## Task 1：类型、协议 profile 与脱敏契约夹具

**Files:** Create `services/codex_desktop_ipc/{mod,types,profile}.rs`（路径前缀均为 `src-tauri/src/`）、`src-tauri/tests/fixtures/codex-desktop-ipc/{README.md,windows-snapshot.json,macos-snapshot.json,patches.json,command.json,permissions.json,user-input.json}`；Modify `services/mod.rs`。

**Interfaces:** Produces 公共类型；`PendingRequestId::{to_key(&self)->String,from_key(&str)->Result<Self,ProtocolError>}`；`profile_for(platform:&str, build:&str)->Option<ProtocolProfile>`；`ProtocolProfile { platform:String, build:String, capabilities:CapabilitySet, method_versions:BTreeMap<String,u32> }`、`ProtocolProfile::version(&self,method:&str,envelope_host:Option<&str>)->Result<u32,ProtocolError>`；`new_uuid()->String`。`ProtocolError` 定义 `UnsupportedMethod/InvalidId/InvalidFrame/InvalidState/StaleContext/Capacity/Timeout/Disconnected`。

- [ ] **Step 1：先写 profile/ID 失败测试**，核心断言：
  ```rust
  assert!(profile_for("macos", "26.1013.31257").is_none());
  let p = profile_for("macos", "26.1013.31256").unwrap();
  assert_eq!(p.version("thread-follower-start-turn", None).unwrap(), 2);
  assert_eq!(p.version("thread-follower-start-turn", Some("local")).unwrap(), 3);
  assert_eq!(PendingRequestId::from_key("n:9007199254740993").unwrap().to_key(), "n:9007199254740993");
  assert_ne!(PendingRequestId::from_key("n:12").unwrap(), PendingRequestId::from_key("s:12").unwrap());
  ```
  增加 `profiles_keep_unverified_methods_disabled`，逐格断言协议表中的初始 capability；三类 UUID 连续生成不相等且满足现有 `codex::is_uuid`。
- [ ] **Step 2：运行失败测试**：`cd src-tauri && cargo test codex_desktop_ipc::profile`；新接口缺失导致失败，先排除环境缺依赖。
- [ ] **Step 3：实现 types/profile 并建立脱敏夹具**。README 区分 OBSERVED / CODE / 未实测；fixture 中保留 `turnHistory.history.entitiesByKey/islands`、array path、整数 request id，另补字符串 ID 合同样本并标为合成。不得把原日志中的开发提示词/凭据/真实会话整包入库。
- [ ] **Step 4：运行 profile + types 测试**：`cargo test codex_desktop_ipc::`（在 `src-tauri`），全部 PASS。
- [ ] **Step 5：显式 add 本 Task 文件并提交**：`feat: define desktop ipc protocol profiles`。

## Task 2：有界帧与双平台全双工 Transport

**Files:** Create `services/codex_desktop_ipc/codec.rs`、`transport/{mod,windows,unix}.rs`；Modify `mod.rs`、必要的 `src-tauri/Cargo.toml` Windows feature（不新增依赖）。

**Interfaces:** Consumes Task 1；Produces `FrameDecoder::{new()->Self,push(&mut self,bytes:&[u8])->Result<Vec<serde_json::Value>,ProtocolError>}`、`encode_frame(&Value)->Result<Vec<u8>,ProtocolError>`；`trait IpcIo: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send`；`type BoxIo=Box<dyn IpcIo>`；`EndpointResolver::resolve(&self)->Result<DesktopEndpoint,ProtocolError>`、`async fn connect(endpoint:&DesktopEndpoint)->Result<BoxIo,ProtocolError>`。`DesktopEndpoint` 包含本机路径、平台和已核对 build，不接收 HTTP 输入。

- [ ] **Step 1：写 `fragmented_and_coalesced_frames`、`reject_length_before_allocation`、`duplex_does_not_block_write` 失败测试**：
  ```rust
  assert!(FrameDecoder::new().push(&0u32.to_le_bytes()).is_err());
  assert!(FrameDecoder::new().push(&268435457u32.to_le_bytes()).is_err());
  let frame = encode_frame(&serde_json::json!({"text":"你好🐇"})).unwrap();
  let mut d = FrameDecoder::new();
  assert!(d.push(&frame[..3]).unwrap().is_empty());
  assert_eq!(d.push(&frame[3..]).unwrap()[0]["text"], "你好🐇");
  ```
  另用同一连接 reader 等待期间 writer 仍能在 1s 内写出完整帧；Unix socket tempdir 测试优先路径、旧路径回退、错误 uid/权限/非 socket 拒绝；Windows 原生测试真实假服务端 named pipe，非 Windows 不谎报该测试已执行。
- [ ] **Step 2：运行 `cargo test codex_desktop_ipc::codec` 和本机 transport 测试，确认缺接口/未实现导致 FAIL**。
- [ ] **Step 3：实现 codec 与 transport**。帧按字节计长，UTF-8/JSON 错误关闭连接；Windows 使用 Tokio `ClientOptions`；Mac 默认路径来自实际 CODEX_HOME，旧路径仅本机 tmpdir/uid 推导。未识别/多义 build 不能伪装成已验证版本。
- [ ] **Step 4：运行本机测试及 Windows 编译门**：`cargo test codex_desktop_ipc::`；`cargo check --target x86_64-pc-windows-gnu --all-targets --features marker-helper`。都成功才标对应自动检查 PASS。
- [ ] **Step 5：提交** `feat: add cross-platform desktop ipc transport`。

## Task 3：单连接 Actor、RPC 关联与未知投递

**Files:** Create `services/codex_desktop_ipc/connection.rs`；Modify `mod.rs`；Create `src-tauri/tests/codex_desktop_ipc.rs`。

**Interfaces:** Consumes BoxIo/profile/codec；Produces `IpcConnection::spawn(io:BoxIo,profile:ProtocolProfile,epoch:u64)->ConnectionHandle`；`ConnectionHandle::request(&self,request:RpcRequest)->BoxFuture<'_,RpcOutcome>`、`subscribe()->broadcast::Receiver<ConnectionEvent>`、`shutdown()->BoxFuture<'_,()>`。`RpcRequest { method:String, params:Value, target:Option<OwnerLease>, mutating:bool,validity:Option<tokio::sync::watch::Receiver<bool>> }`；`RpcOutcome { delivery:Delivery, envelope_id:String, result:Option<Value>, reason_code:Option<String> }`；事件含 `Initialized{client_id,epoch}`、`Broadcast{epoch,frame}`、`Disconnected{epoch}`。测试本文件定义使用 Tokio duplex 的 `ScriptedPeer`，`new()->(BoxIo,Self)`、`recv()->Value`、`send(Value)`、`close()`，不连接真实 Desktop。

连接 epoch 由同一 supervisor 每次重连递增，不能每次新 actor 重置为 0。writer 在真正写帧之前检查 epoch 与 validity 活性信号，避免请求排队期间 owner 换代后仍写出。

- [ ] **Step 1：写 `rpc_matches_id_and_method`、`partial_write_is_unknown`、`discovery_is_answered_while_write_waits` 失败测试**。假 peer：先读 initialize→回 clientId；收到写请求后断线，断言 `outcome.delivery == Delivery::UnknownDelivery` 且只收到一个 mutating frame。队列满/排队时取消且尚未交给 writer 为 `NotSent`；超时后读/写均未知。
      `client-discovery-request {requestId,request:{...}}` 必须立即回 `{type:"client-discovery-response",requestId,response:{canHandle:false}}`；等待一个慢 RPC 不得堵住该回包。该控制回包有预留发送容量，不因普通 outbound 64帧已满而丢失。迟到响应 ID 不命中、method 不符不误唤醒其他请求；错误/拒绝的目标和 request ID 不可混淆。
- [ ] **Step 2：运行 `cargo test --test codex_desktop_ipc`，确认新增测试 FAIL 且实际执行新增用例，不能接受过滤为 0 tests**。
- [ ] **Step 3：实现 connection actor**。reader、single writer、RPC waiter 分离；写入交给 writer 前检查 lease epoch；writer 一旦开始尝试写帧即跨越保守投递边界，部分写/取消/timeout/EOF 都不能返回 NotSent。只在 response 明确成功且 handler 一致时 Accepted；收到明确 error 为 Rejected。连接失败释放全部 waiter；不重放 outbound。
- [ ] **Step 4：运行集成测试和 `cargo test codex_desktop_ipc::`，PASS**。增加 65 个同时 RPC 的容量测试，断言第 65 个入队前拒绝、不增加写入次数。
- [ ] **Step 5：提交** `feat: manage desktop ipc requests and delivery outcomes`。

## Task 4：Owner Registry、following 与恢复代际

**Files:** Create `services/codex_desktop_ipc/owners.rs`；Modify `connection.rs`、`mod.rs`、集成测试。

**Interfaces:** Produces `OwnerRegistry::{bind(&mut self,key:ThreadKey,client_id:String,epoch:u64)->OwnerLease,invalidate_client(&mut self,client_id:&str),reset(&mut self,new_epoch:u64),current(&self,key:&ThreadKey)->Option<OwnerLease>,is_current(&self,lease:&OwnerLease)->bool}`；`async fn discover_and_follow(conn:&ConnectionHandle,owners:&mut OwnerRegistry,key:&ThreadKey)->Result<OwnerLease,ProtocolError>`。索引双向；generation 按 Thread 单调增加，reset 后 epoch 增加。跟随管理属于单 actor，不在持互斥锁期间 await。

Registry 另产出 `validity(&self,lease:&OwnerLease)->tokio::sync::watch::Receiver<bool>`；失效/换代向旧 lease 信号发 false，adapter 将它传入 Task 3 的写请求。测试请求排队后 invalidate、writer 恢复时结果 NotSent 且未写帧；开始写后再 invalidate 只能 UnknownDelivery。

- [ ] **Step 1：写 `disconnect_invalidates_every_thread_of_owner` 与 `old_epoch_never_becomes_current` 失败测试**：
  ```rust
  let mut r = OwnerRegistry::default();
  let a = r.bind(ThreadKey{host_id:"local".into(),thread_id:"t1".into()}, "owner-a".into(), 1);
  let b = r.bind(ThreadKey{host_id:"local".into(),thread_id:"t2".into()}, "owner-a".into(), 1);
  r.invalidate_client("owner-a");
  assert!(!r.is_current(&a)); assert!(!r.is_current(&b));
  ```
  集成测试精确验证 discovery params `{hostId:"local",conversationId}`、owner 取 response.handledByClientId、随后 following broadcast v1；无 following 不允许假装同步完成。
- [ ] **Step 2：运行 `cargo test codex_desktop_ipc::owners`，FAIL**。
- [ ] **Step 3：实现 owners 与 follower 生命周期**。`client-status-changed(disconnected)` 清该 client 全部映射；reset/EOF 清全部 lease。重连后 initialize→重新 discovery→following→等 snapshot，不仅恢复 socket。收到 following-status-requested 对真实订阅回复 following；退出订阅发 false。发现 no-client-found 先核 profile 和 envelope host 规则，不以此证明 A 写请求没送达。
- [ ] **Step 4：运行 owner 单测和集成测试**；模拟 A→B 后 A 的迟到 response/state 均被拒；最多 64 跟随、移除一条释放名额，PASS。
- [ ] **Step 5：提交** `feat: track desktop owners and recover subscriptions`。

## Task 5：Revision Mirror 与统一状态投影

**Files:** Create `services/codex_desktop_ipc/mirror.rs`；Modify `types.rs`、集成测试、夹具。

**Interfaces:** Produces `ThreadMirror::{new(key:ThreadKey)->Self,apply(&mut self,lease:&OwnerLease,change:&Value)->Result<MirrorEffect,ProtocolError>,mark_stale(&mut self),project(&self,caps:CapabilitySet)->ThreadProjection}`；`MirrorEffect { changed:bool, need_snapshot:bool }`。mirror 绑定当前 lease，换 owner 先清状态，禁止仅由调用方口头保证。

- [ ] **Step 1：写 `empty_turns_uses_canonical_history`、`patch_batch_is_atomic`、`revision_gap_requires_snapshot` 失败测试**。加载两平台 fixture，断言 `turns=[]` 仍投影出其 `turnHistory.history` 的用户与助手消息、稳定 item ID 以及完整 pending；同一 streaming item replace 不能追加重复气泡。
  ```rust
  // 在相同 lease 的 revision=7 快照上，baseRevision=6 的增量不应用。
  assert!(mirror.apply(&lease, &serde_json::json!({"type":"patches","baseRevision":6,"revision":8,"patches":[]})).unwrap().need_snapshot);
  assert!(mirror.project(caps.clone()).stale);
  ```
  数组 add 是插入、remove 删除、replace 只替换现有索引；一批中第二条非法时第一条也不生效。低 revision 重复 snapshot/patch 忽略；新 epoch 不比较旧 revision 大小。
- [ ] **Step 2：运行 `cargo test codex_desktop_ipc::mirror`，FAIL**。
- [ ] **Step 3：实现 mirror 与 projection**。按 `islands[].entries` 顺序取 entitiesByKey，不遍历无序 map 当时间顺序；Windows/Mac 的 `turnHistory` wrapper 由 fixture 决定，不能只看 `turns`。从 runtime status 与 turn history 得到 active/idle/failed，缺字段为 unknown；unknown/stale 不准写。未知 item kind 只投影安全文字摘要，不下发原始工具参数。缺口重新 following 取 snapshot；只读 history 不完整时调 v1 load-complete-history 并等相应 snapshot。
- [ ] **Step 4：运行 mirror 与集成测试**；正文超过 1 MiB / 500 条明确 truncated，镜像超过 16 MiB 或总预算 64 MiB 拒绝且 stale，无界旧历史不被全量扫描，PASS。
- [ ] **Step 5：提交** `feat: mirror desktop thread state and project remote views`。

## Task 6：原生 Start Turn、两类审批及阻塞式问答

**Files:** Create `services/codex_desktop_ipc/{pending,adapter}.rs`；Modify `types.rs`、`mod.rs`、集成测试。

**Interfaces:** Produces `DesktopIpcAdapter::{async ensure_thread(&self,key:&ThreadKey)->Result<ThreadProjection,ProtocolError>,async start_turn(&self,key:&ThreadKey,action_id:&str,text:&str,client_message_id:&str)->RpcOutcome,async respond(&self,key:&ThreadKey,request_key:&str,context_token:&str,action:PendingAction)->RpcOutcome,async projection(&self,key:&ThreadKey)->ThreadProjection,shutdown(&self)->BoxFuture<'_,()>}`。adapter 持 connection/owners/mirror/pending actor，所有写入在 actor 中最后复核；响应前读到的过期 lease 不算授权。

另提供 `async unfollow(&self,key:&ThreadKey)` 和 `subscribe(&self)->broadcast::Receiver<ThreadProjection>`，供详情卸载、队列调度和 REST/SSE 共用。启动和重连由该 adapter supervisor 统一管理，watcher 只消费投影事件。

- [ ] **Step 1：写 `start_turn_targets_original_thread`、`approval_variants_never_share_wire_payload`、`answer_binds_current_pending` 失败测试**。假 owner 逐字段断言：start-turn v2、target=当前 owner、`conversationId == turnStart.request.threadId`、input text、text_elements=[]、clientUserMessageId，context={}；不发送 model/权限/collaboration 覆盖。
      command 用 `thread-follower-command-approval-decision` / 原始整数或字符串 id / accept 或 decline；permissions 用 `thread-follower-permissions-request-approval-response` / 服务端原权限对象或 `{}` / scope=turn。
      input 用 `thread-follower-submit-user-input`，两个问题以原 qid 映射，不能漏答或混入未知 qid；`isOther=false` 禁任意文本，`isOther=true` 接受自由文本，`isSecret=true` 不在审计、日志和错误文本泄漏回答。
- [ ] **Step 2：运行 `cargo test --test codex_desktop_ipc native_`，FAIL**（本 Task 新增测试名称统一以 `native_` 开头）。
- [ ] **Step 3：实现 adapter/pending 映射**。request key/token 变更规则见公共约定；同 request 正在提交时第二次响应拒绝。owner 失效、pending 消失或参数变更拒绝旧卡；profile capability=false 拒绝而不是试发。不要根据 `availableDecisions` 生成任意决策；只提供产品批准/拒绝。
- [ ] **Step 4：运行全部 native 测试**，断言无 `availableDecisions` 仍两按钮、未知 request method 仅显示不支持、Default async 消息不生成 pending 卡。测试 profile 注入可打开未实测方法以测构造器，但不得改生产 profile，PASS。
- [ ] **Step 5：提交** `feat: control native desktop turns approvals and user input`。

## Task 7：持久动作账本、普通消息 FIFO 与安全路由

**Files:** Create `services/codex_control/{mod,router,queue}.rs`、`database/dao/codex_actions.rs`；Modify `services/mod.rs`、`database/{schema.rs,dao/mod.rs,dao/write_audit.rs}`；Create `src-tauri/tests/codex_control_remote.rs` 的账本测试。

**Interfaces:** Produces `CodexControlRouter::{async send(&self,ctx:ActionContext,text:String,queue_only:bool)->RouterOutcome,async respond(&self,ctx:ActionContext,request_key:String,context_token:String,action:PendingAction)->RouterOutcome}`；`RouterOutcome = Completed(ActionReceipt) | Queued{action_id:String,position:usize} | UseFallback{permit:FallbackPermit,reason_code:String}`。`ActionContext { action_id:String, device_id:String, device_name:String, key:ThreadKey }`；`async fn flush_thread(router:&CodexControlRouter,key:&ThreadKey)->Option<RouterOutcome>`，未取得可派发队首返回 None；其返回的 UseFallback 由 hub 调度器与 HTTP 同样消费。`FallbackPermit` 字段私有且不可 Clone，携带同 action 的唯一派发权和 Thread guard，必须移入 detached bridge 直到落账；丢弃未完成 permit 将 reserved 记 unknown，不自动回 queued。

动作表 `codex_actions`（新表，复用 schema 幂等建表流程）：`seq INTEGER PRIMARY KEY AUTOINCREMENT`、`action_id TEXT UNIQUE NOT NULL`；device_id、device_name、thread_id、kind、request_key、context_token、payload_hash、client_message_id、state、route、queued_text、created_at、updated_at、receipt_json；另存 `compat_outstanding`、`compat_message_id`、`compat_turn_id`。state=`queued/reserved/accepted/rejected/unknown/cancelled`；FIFO 按 seq，不按毫秒/随机 UUID 排序；queued_text 仅普通待发消息保存，派发后清空；审批答案只存 hash，不存答案。索引 `(thread_id,state,seq)`。`reserve` 与审计起始行同事务；DB 失败零投递。内存旧库→幂等 schema init→新表可用，既有队列/审计数据不丢。

`ActionStore` 包装 DeviceStore，不跨 await 持 DB 锁；内部读取类型 `QueuedAction { seq:i64,ctx:ActionContext,raw_text:String,client_message_id:String }` 不发给浏览器。固定接口：

- `register(&self,ctx:&ActionContext,kind:ActionKind,request_key:Option<&str>,context_token:Option<&str>,payload_hash:&str,raw_text:Option<&str>)->Result<CodexActionView,StoreError>`，同时生成并持久化 client_message_id。
- `next_queued(&self,key:&ThreadKey)->Result<Option<QueuedAction>,StoreError>`，供重启后调度取得全部发送输入。
- `reserve(&self,action_id:&str,route:ControlRoute)->Result<bool,StoreError>`，CAS queued→reserved；`handoff_not_sent(&self,action_id:&str)->Result<bool,StoreError>`，只允许同 permit 把 reserved/DesktopIpc 原子改为 reserved/HeadlessCompat并记降级审计，绝不经过 queued。
- `finish(&self,receipt:&ActionReceipt)->Result<(),StoreError>`、`get_for_device(&self,action_id:&str,device_id:&str)->Result<Option<CodexActionView>,StoreError>`、`recover_reserved(&self)->Result<usize,StoreError>`。
- `list_queued(&self,key:&ThreadKey,device_id:&str)->Result<Vec<CodexActionView>,StoreError>`、`retract(&self,action_id:&str,device_id:&str)->Result<bool,StoreError>`，CAS queued→cancelled，保留 actionId 墓碑、清正文并审计；不能撤 reserved/accepted/unknown。
- `set_compat_fence(&self,action_id:&str,message_id:Option<&str>,turn_id:Option<&str>)->Result<(),StoreError>`、`has_compat_fence(&self,key:&ThreadKey)->Result<bool,StoreError>`、`clear_compat_fence(&self,action_id:&str)->Result<(),StoreError>`；Accepted/Unknown 只有原投递对应 Turn 完成的只读证据允许 clear，不因 helper 返回就 clear。明确零投递的 NotSent 由 `finish` 在同一事务清 fence，不要求不存在的 Turn 完成证据。

`StoreError=Conflict|Database(String)`。payload hash 用现有 sha2 对 kind/thread/requestKey/contextToken/原始文本或有序答案做规范化序列化；不能只 hash 文本。新增 `write_audit::record_conn_checked`：参数与原 record_conn 完全相同，返回 `rusqlite::Result<()>`，由 ActionStore 事务调用并传播错误；原 record_conn 用 checked 实现后保留原来的记录错误兼容行为。

- [ ] **Step 1：写 `same_action_never_sends_twice`、`restart_reserved_becomes_unknown`、`busy_fifo_allows_pending_response`、`fallback_only_before_dispatch` 失败测试**：
  - 同 device/thread/action/payload 返回原状态，RPC write 计数恒 1；相同 action_id 换正文/设备/thread 返回 conflict 且零新增写入。
  - writer 执行前先 `queued→reserved`，进程重启将 reserved→unknown，不能回 queued；queued 恢复仍可按序发送。
  - active 时连续 a/b 两条消息落 SQLite FIFO，idle 后只发 a，等它终态再发 b；期间审批能取得短 action 锁响应原 Turn。
  - IPC NotSent 且仍未尝试写可 UseFallback；Accepted/Rejected/UnknownDelivery 不得触发 fallback；version/profile 不匹配允许普通消息兼容，但不能解释成旧 RPC 未投递。
  - 相同毫秒 a/b 仍按 seq；关闭进程、丢弃内存、重建 hub 后能读到原文/设备/原 client_message_id 并发送；撤回与 reserve 竞态只有一个 CAS 成功，已撤 actionId 再提交不会重发；设备被吊销后的 queued 动作拒绝而不派发。
  - 立即发送、FIFO重启恢复、fallback 三条路径逐一断言正文只有一个 `[mobile <设备名>]`；A adapter不再自行追加标记，hash始终基于原文。
  - IPC已 reserved、writer前 NotSent→handoff→fallback 恰好一次；交接时崩溃恢复为 unknown。审计表安装拒绝 INSERT 的 SQLite trigger→reserve回滚、仍queued、IPC与fallback写入均0。
- [ ] **Step 2：运行 `cargo test --test codex_control_remote`，FAIL**。
- [ ] **Step 3：实现 action store/router/queue**。取 lease 与 mirror 后短串行锁复核；普通发送一 Thread 一 active Turn；pending 响应只锁 dispatch/ack，不持整个 Turn 等待期。queueOnly 持久排队；空闲事件和周期对账接力；每次真正派发前再核设备在册/远程开关。compat 仅由 remote bridge 在 H3 打开时执行；UseFallback 不等于投递成功。
      来源标记按宪法 F2.2 和现有 H8 Codex APP 行为保留：A 分支派发前调用 `normalize::compose_injection(device_name,raw_text)` 恰好一次（保留其中文/换行归一及花名拒绝面）；native adapter 接收已组装文本。持久队列/hash 存原始文本，恢复后仍只组装一次；fallback 给既有 helper 原文，由它组装，禁止传已加签名文本造成双标记。不在本期改全局签名开关的历史语义。
      compat 发送前持久 reserved 和 compat fence，bridge 持 permit 直到落账，不复用 H8 内部 TurnGuard。投递终态与 Turn 调度终态分开：H8 `queued` 只证明进原生队列，`ok` 只证明已消费；两者都不能据此清 fence 或发下一条。使用 bridge 暴露的 queued message ID/turn ID、既有只读 queue evidence 和新 owner snapshot核对：原生队列已无该消息且对应 Turn 明确完成，才 clear fence；仅 idle snapshot 或 helper 返回不够。证据不足保持 fence/后续消息留队，界面说明需回 Desktop 核对，不猜完成。fence 跨热重启与应用重启保存，A恢复不得绕过；新 pending 交互仍可继续。
      零投递例外：CLI缺失、UUID/花名/探针等明确在 spawn/write 前拒绝得到 NotSent，finish 与清 fence 同事务；这类动作没有 Turn，不等待 Turn 结束。原生 A 的调度也必须等回执中对应 turnId 的终态，而非收到任意 idle 就释放；若成功回执缺 turnId 且无法核对，按未知处理并保留顺序限制。
      为兼容 fence 提供 `CompatFence { action_id:String,queued_message_id:Option<String>,turn_id:Option<String>,rollout_path:Option<String>,rollout_baseline:Option<u64>,wire_hash:Option<String> }`（服务端内部类型）；DAO `compat_fences(&self,key:&ThreadKey)->Result<Vec<CompatFence>,StoreError>` 读取，新增证据字段随 fence 持久保存。桥接前记录 rollout基线和已组装文本hash；只读 `async fn reconcile_compat_fence(fence:&CompatFence,projection:&ThreadProjection)->Result<bool,ProtocolError>` 复用现有 queue副本查询和有界 rollout解析（最多追加尾窗2 MiB），按基线之后唯一匹配的用户输入hash关联 Turn，再核其完成。读失败、多义、已被尾窗截断均返回 false，不能用“队列没找到”单独证明执行完成；无原生owner时也可由 rollout 的明确对应Turn终态提供完成证据。只读核对不修改 Codex 数据文件。
- [ ] **Step 4：运行账本/路由测试**，补 DB 写失败、HTTP client drop 后任务继续落账、启动 reserved 标 unknown、关远程时未派发仍 queued、未知操作阻止相同 action 重放，PASS。再测 fallback滞留→helper返回→IPC恢复→应用重启，第二条仍不越序；只给 idle 证据不解锁，给原消息对应完成证据才解锁。`unknown` 只由只读核对提供证据，不自动改回 queued。
      增加 CLI缺失/探针拒绝→NotSent→fence已清→A恢复后下一条正常发送；A ack 之前遗留 idle snapshot 不触发第二次 start-turn。
- [ ] **Step 5：提交** `feat: route codex control with durable delivery tracking`。

## Task 8：Remote API、生命周期、安全与审计接线

**Files:** Create `src-tauri/src/remote/codex_ipc.rs`；Modify `remote/{mod,api,server}.rs`、`inject/headless/codex.rs`（仅给兼容内部结果补 evidence，旧 wrapper 保留）；复用 `inject/queue.rs` 既有 headless 隔离门并加回归测试；扩展 `src-tauri/tests/codex_control_remote.rs`。

**Interfaces:** `RemoteState.codex_control: Arc<CodexControlHub>` 一个字段束（含 router、adapter、事件 tx、生产 fallback bridge），测试 `CodexControlHub::disabled()` / 假体。在 `remote/codex_ipc.rs` 提供 `async fn thread_state(State<Arc<RemoteState>>,Query<ThreadQuery>)->Response`、`thread_events`、`action_status`、`pending_response`；API 路由精确如下：

| HTTP                                               | 参数/响应                                                                                                                                                                                                                                   |
| -------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| GET `/m/api/v1/codex/thread?session_id=...`        | Session 经服务端 resolve_thread→local ThreadKey；响应 ThreadProjection                                                                                                                                                                      |
| GET `/m/api/v1/codex/thread/events?session_id=...` | 首帧 `snapshot`，后续 `state` 均 ThreadProjection；64 槽 broadcast lag→重新 snapshot；复用 SseRegistry/吊销/关闭逻辑                                                                                                                        |
| GET `/m/api/v1/codex/action?action_id=...`         | 仅当前设备自己的动作回执/queued 状态，其他设备 404                                                                                                                                                                                          |
| GET `/m/api/v1/codex/queue?session_id=...`         | CodexQueueView：当前设备在该 Thread 的待发列表、跨设备 FIFO 实际位置；compat fence 时 blockedReasonCode=compat_outstanding                                                                                                                  |
| POST `/m/api/v1/codex/queue/retract`               | `{actionId}`，仅当前设备自己的 queued 动作可撤回；成功 cancelled；已派发409；不存在/其他设备404；保留 actionId 墓碑                                                                                                                         |
| POST `/m/api/v1/codex/pending-response`            | `{sessionId,actionId,requestKey,contextToken,action:"approve"                                                                                                                                                                               | "reject" | "answer",answers?:Record<string,string[]>}`；`deny_unknown_fields`，不得接收 owner/权限对象/raw method |
| POST 既有 `/session-send`                          | Codex APP 新增必需 actionId（新 mobile 传；缺失 400，不代造使重发双投）；CLI 和其他工具保持旧合同；新增 `status:"desktop_ipc",receipt:ActionReceipt` 与 `status:"codex_queued",actionId,position`，fallback 仍旧 headless 封套并附 actionId |
| GET 既有 `/session-send-info`                      | Codex APP 从同一 control projection 得 injectable/capability；A可用时不先看 H3，A不可用的兼容分支才受 H3；其余工具保留旧能力源                                                                                                              |

普通消息仍限制 `MAX_SEND_CHARS=10_000`，原生回答每题最多 10_000 字符，最多 32 题、每题 32 个答案项、总正文 64 KiB；选项必须属于当前请求，空答案拒绝。所有读写 no-store；错误状态 400 malformed、403 unauthorized/headless_disabled、404 missing、409 stale_context/action_conflict、503 capability_unavailable；未知投递是 200 的明确 receipt，不能用可安全重试的普通错误替代。

- [ ] **Step 1：写 HTTP 集成失败测试**，用 `DeviceStore::memory`、假 session source/adapter/fallback，不接触真实 ~/.tuvis。对上述每条新路由测无 cookie/过期 cookie/吊销=403，回环同样拒绝；POST 带 ownerClientId/targetClientId/raw method=400。过期 token/跨 thread requestKey=409 且 IPC write=0。
      再测试 H3=false 且 A可用→IPC正常；A不可用且 H3=false→compat拒绝；unknown/accepted 不调用 fallback；手机断开 POST 后不取消已派发写动作；SSE 被吊销即时关闭。
      对新 queue GET/撤回补认证、其他设备404、撤回竞态409与 cancelled actionId 不重发测试；H3=false 的 send-info 必须 injectable=true，不能只测绕过 composer 的 POST。
- [ ] **Step 2：运行 `cargo test --test codex_control_remote`，FAIL**。
- [ ] **Step 3：实现 HTTP bridge 与生产启停**。注册在现有 gated nest 内；start_server 装配 hub，remote 关闭取消订阅/恢复循环和未派发调度，已派发任务留至落账或 unknown；hot restart 不重放。旧 server fixture 全部显式填 disabled hub。不要在 `get_all_sessions` 三秒扫描里逐条连接 IPC；只按请求和有界关注集合工作。
      Codex APP `session-send` 和 `session-send-info` 都必须在旧 `headless_blocked` / 静态 route 之前交给 control hub，**只有兼容分支受 H3**。原生 busy 消息用 codex_actions，不入旧 inject_queue。旧 `try_flush_with` 已通过 `routing::route` 拦截 headless（含 Codex APP），普通 flush 与 jump 共同走这道门，复用该判据并测二者零终端注入；存量 APP pending 可由用户经旧队列撤回，不自动导入新账本重投。
      fallback bridge 固定入口 `async fn run_compat(st:Arc<RemoteState>,permit:FallbackPermit)->serde_json::Value`：permit 内部取原文/设备/会话映射，经现有 `codex_headless_dispatch_envelope` 执行，生成对应 ActionReceipt 与 progress证据后 `finish`。复用 helper 的回执正文，但扩展内部 evidence 返回（保留公开旧 helper wrapper）：`CompatEvidence { queued_message_id:Option<String>,turn_id:Option<String>,turn_completed:bool }`。没有证据时 turn_completed=false。旧 receipt queued/ok 映射 Delivery::Accepted；投递前明确 refusal 映射 NotSent；执行后超时/取消/不确定失败一律 UnknownDelivery，不能泛化为可安全重试的失败。
      Codex APP 的旧 `session-question/answer` 补投与 `session-approve` 键位面一律回 capability_unavailable / 引导原生端点；GET 不再把陈旧 rollout 问题广告为可处理原生卡。模式、CLI 和其他工具合同不扩权、不重构。
- [ ] **Step 4：补审计并运行集成测试**。动作 action=`codex_send/codex_approve/codex_reject/codex_answer/codex_fallback`，channel=`codex_desktop_ipc` 或实际 headless；summary 是 JSON `{actionId,threadId,requestKey?,envelopeId?,result}`，device 使用服务端设备身份。测试 send/两类审批/回答/降级/未知每项都有可追踪记录，原 permissions、答案、cookie、API key 哨兵不出现在审计或事件中；fallback 既有 headless 行保留，以 actionId 另行关联。所有新 error 经 main 错误码机制。
- [ ] **Step 5：提交** `feat: expose authenticated desktop control api`。

## Task 9：移动端能力、实时投影与原生 Pending 卡

**Files:** Create `src/mobile/{codexIpcApi.ts,useCodexThread.ts,PendingRequestCard.tsx,CodexQueuePanel.tsx}`、`tests/mobile/{codexIpcApi,useCodexThread,PendingRequestCard,CodexQueuePanel}.test.tsx`；Modify `src/mobile/{api.ts,SessionDetail.tsx,MessageComposer.tsx}`、两份 i18n locale、既有相关测试。

**Interfaces:** `fetchCodexThread(sessionId:string):Promise<ThreadProjection>`、`subscribeCodexThread(sessionId:string,onState:(s:ThreadProjection)=>void,onUnavailable:()=>void):()=>void`、`respondCodexPending(body:PendingResponseBody):Promise<ActionReceipt>`、`fetchCodexAction(actionId:string):Promise<CodexActionView>`；`useCodexThread(sessionId:string,enabled:boolean):{state:ThreadProjection|null,refresh:()=>Promise<void>,transport:"sse"|"poll"|"off"}`；`PendingRequestCard({sessionId,request,onReceipt})`。TS DTO 与后端 camelCase 逐字段相同，requestKey 永远 string；`sessionSend` Codex APP 传 actionId，其他发送保持既有参数。

另提供 `fetchCodexQueue(sessionId:string):Promise<CodexQueueView>`、`retractCodexAction(actionId:string):Promise<CodexActionView>`、`CodexQueuePanel({sessionId})`，显示本设备待发与实际 FIFO 位置、撤回按钮及 compat_outstanding 说明；面板挂载/状态跃迁/操作后重拉，禁提供插队或撤回已派发动作。

- [ ] **Step 1：写以下失败测试**：
  ```tsx
  expect(screen.getByText("仅可使用兼容模式")).toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "重试" })).not.toBeInTheDocument(); // UNKNOWN_DELIVERY
  expect(postBody.requestKey).toBe("n:9007199254740993"); // 字符串无损
  expect(postBody).not.toHaveProperty("ownerClientId");
  ```
  fixtures 分别验证 command/permissions 两按钮、isOther 自由作答、isSecret password 输入且完成换卡清草稿、两个问题全答后提交，owner 换代或 requestKey/token 变化清旧状态。Default async 消息不渲染阻塞卡；unsupported request 可见但不可操作。
  补 `native_send_with_h3_off`：真实渲染 MessageComposer、mock send-info 为A能力、H3=false，按钮仍可发送且只发一次；不能只测试 hook。queue 面板撤回后条目消失、409保留并刷新、cancelled action 不出现重试。
- [ ] **Step 2：运行 `pnpm test -- tests/mobile/PendingRequestCard.test.tsx tests/mobile/useCodexThread.test.tsx tests/mobile/codexIpcApi.test.tsx tests/mobile/CodexQueuePanel.test.tsx tests/mobile/MessageComposer.test.tsx`，FAIL**。
- [ ] **Step 3：实现 API/hook/卡及详情接线**。有 A snapshot 时用投影替换当前 Codex APP 的实时内容源，不把同一历史再与 rollout 简单 concat；stale 标记保留最后视图但禁写。四档能力按服务端数据渲染；compat 的消息回执沿旧组件，原生操作不展示“已补投为消息”。
      一次用户动作生成 actionId，双击沿同 ID，网络失败不自动换 ID/重发；用只读 action 查询核对，UNKNOWN 显示“投递结果未知，请在 Desktop 核对”，不出现自动重试按钮。Accepted 只表示已受理，不提前标 Turn 完成；pending 消失以新投影为准。
      SSE 首帧/增量同 DTO；失败按现有订阅节奏退避，连续 3 次失败降为 3s GET，仅当前详情订阅，不打开全历史；卸载 abort fetch/关闭 stream，过时响应不盖新 session。
- [ ] **Step 4：运行本 Task 和既有 `SessionDetail/MessageComposer/QuestionCardApp/QuestionCardCli` 测试**，验证 CLI 无回归、APP 不落旧补投；中英文文案齐备。`pnpm check` 与 `pnpm build:mobile` PASS。
- [ ] **Step 5：提交** `feat: render native codex desktop control on mobile`。

## Task 10：双平台 E2E、恢复/兜底回归与验收交接

**Files:** 扩展两份 Rust 集成测试、移动端测试；Create `docs/superpowers/plans/2026-10-16-codex-desktop-ipc-acceptance.md`；按证据 Modify `profile.rs` capability flags；必要时更新用户手册中英对应章节（不更改宪法）。

**Interfaces:** Consumes Task 0–9；Produces 可复现命令、自动检查结果和逐平台真实证据，不能把“已编码”写成“真机 PASS”。

- [ ] **Step 1：先补全链路失败测试**（新测试应在尚未接通的端点/事件/账本环节 FAIL）：fake Desktop→mirror→authenticated HTTP→mobile；写出半帧/完整帧后丢 ack→unknown→restart→write_count=1/fallback_count=0；owner A 断→B→旧卡409→新卡可答；active→队列→审批→idle→FIFO 发送；SSE lag 后 snapshot 恢复。
- [ ] **Step 2：修复接线直至上述测试 PASS**；不为通过测试放开未知生产 build/method。
- [ ] **Step 3：运行完整自动门禁并保存简要结果**：根目录 `pnpm test`、`pnpm check`、`pnpm build:mobile`、`node scripts/check-error-codes.mjs`；再 `bash scripts/ci-backend-local.sh`。按 CI 要求在 Windows/Linux 运行相应 job；macOS 本机与交叉 check 不能代替 Windows named pipe 运行测试。
- [ ] **Step 4：执行以下真实 Desktop E2E**，使用专用测试 Thread/临时项目，逐条记录 build、平台、操作、actionId/requestKey（脱敏）、结果、证据。切 Plan/权限策略由用户在 Desktop 完成；不通过本功能写设置。重启 Desktop 可能打断工作，应先由用户选择可重启时机，未获该操作授权则该格保持待验，继续其他项。

| 格                           | Windows                | macOS                  | PASS 判据                                                                                        |
| ---------------------------- | ---------------------- | ---------------------- | ------------------------------------------------------------------------------------------------ |
| connect + owner + following  | 必测                   | 必测                   | 版本来自实际官方进程；两个 Thread 同 owner 均可跟随，非 owner 历史不误判可控                     |
| 手机发普通消息               | 必测                   | 必测                   | 同原 Thread 出现且只有 1 个 Turn，回执 turnId 与持久化记录对应，Desktop自己执行                  |
| streaming / history          | 必测                   | 必测                   | `turns=[]` 仍完整显示历史；增量同气泡更新；完成/失败状态收口                                     |
| command approve / reject     | 已探测，产品仍复验     | Probe 未触发，产品待验 | 原 command pending 消失，批准真执行、拒绝未执行；不以 permissions 替代该格                       |
| permissions approve / reject | Probe 未触发，产品待验 | 已探测，产品仍复验     | 原权限对象回传、scope turn；文件/网络权限实际效果与拒绝证据                                      |
| Plan blocking input          | 已探测，产品仍复验     | **必须补验**           | 捕获 `item/tool/requestUserInput`、原 request id，提交后原 Turn 使用答案继续；Default async 不算 |
| Desktop restart / owner变化  | 已探测，产品仍复验     | **必须补验**           | old owner 失效、重连重新 following/snapshot、旧卡拒绝、无写请求重放                              |
| fallback                     | 必测                   | 必测                   | A确实不可用且 H3开→仅普通消息走 #117，报告真实可见性；H3关→诚实拒绝                              |
| 未知结果                     | 必测                   | 必测                   | 故障代理在发出后丢响应，UNKNOWN；重连/刷新/重启无重复动作和 fallback                             |

无法触发某方法记 NOT_TRIGGERED，保留该 profile flag=false，不宣称该平台该方法完成。为验证构造器允许独立 E2E 测试程序显式注入实验 profile，不能在发行 UI 加“绕过版本门”开关；只有闭环 PASS 才提交开启对应生产 flag。缺机器/权限/触发条件记录阻塞项，不新开无限 Probe，也不阻塞其他已确定任务的代码收口。

- [ ] **Step 5：提交验收与仅被证据支持的 capability 变更** `test: verify desktop ipc control across platforms`。若生产 flag 有改动，重跑 profile 单测和相关端点/卡测试；无变动不重复全套耗时门禁。
- [ ] **Step 6：交给本聊天 review**：给出 I、最终 HEAD、Task 完成情况、`git diff --stat I..HEAD`、各门结果、双平台待验格及验收文件；不自行 merge、不 push。完整 feature 的完成声明要求 spec 11 条逐条有证据；仅自动测试通过只能称“代码与自动检查完成”。

## Spec 覆盖与自审记录

| 需求                                      | 任务              |
| ----------------------------------------- | ----------------- |
| F1 / 3.2 owner多对一与历史边界            | 2、4、8、10       |
| F2 / 3.3 实时状态、snapshot/patch/history | 3、5、8、9、10    |
| F3 原 Thread、有序消息、宪法忙态留队      | 6、7、8、9、10    |
| 宪法 F2.2 来源标记、队列查看/撤回         | 7、8、9           |
| F4 两类原生审批                           | 6、8、9、10       |
| F5 Plan blocking input                    | 6、8、9、10       |
| F6 断线/重启恢复                          | 3、4、5、7、8、10 |
| F7 router/fallback、未知不切换            | 3、7、8、10       |
| F8 真实能力四档、未知版本关闭             | 1、5、8、9、10    |
| S1/S2 不透传IPC、设备门禁                 | 2、8、10          |
| S3/S4 服务端靶向、当前上下文              | 4、6、8、9        |
| S5 不确定结果不重试                       | 3、7、8、9、10    |
| S6 审计与凭据保护                         | 7、8、10          |
| 范围外能力保持范围外 / 双平台待验         | 1、6、8、10       |

已进行 writing-plans 五项自审：覆盖表无缺项；每个编码任务均有失败测试→实现→验证→提交；公共类型集中定义；五项 Review Focus 已分配测试；没有把方法实现体抄成计划。Task 0 是可单独验收的依赖集成，Task 1–10 为同一原生控制纵向能力，不拆成互不相干的产品子项目。后续独立子代理另做 spec/完整性审查，意见处理记录追加于本文件末尾。

### 2026-10-16 Task 0 执行追记（集成适配记录）

- merge `289b20d` + `origin/main@d3fef5f` → 冲突仅 `tests/mobile/SessionDetail.test.tsx`（与 merge-tree 预演一致：#117 新增 /session-queue、/session-mode 路由桩 vs main 纯 prettier 重排，保留 #117 侧）。merge commit `a777c5a`。
- 错误码迁移：#117 新增裸串 222 处全量上码（inject/headless 13 文件 196 处 + remote/api.rs 24 处 + 顺带迁移基线串），共引入 176+28 个新码，locale zh/en 各 +204 键；remote/api.rs 棘轮 61→29 ≤ 35。2 条 api.rs 串按 errcode.rs「原文锚定族」规则保持裸串（QuestionCardApp 前端逐字镜像 + 补投协议载荷）。
- 测试漂移修复：SessionDetail.test.tsx 未使用导入移除；NewSessionForm.test.tsx prettier 格式化；mode_audit 断言改含码形态。
- 诚实申报：执行分支上的错误码迁移由两个并行子代理执行，主控逐项核对了其报告（棘轮 0 项 / api.rs 61→29 / 锚定跳过清单 / locale 片段合并后再跑门禁全绿）。
- Task 1–3（类型/transport/connection）代码已在集成 worktree 就绪但按分支纪律**不进 I**，将从 I 开 `codex/codex-desktop-ipc` 后提交。

### 2026-10-16 独立审查处理记录

已派两个独立上下文的只读子代理，分别审查 spec/完整性和仓库落地/复用/分支方案，均实际读取了本计划及相应依据。首轮为 Needs revision，修订如下：

| 发现                                  | 修订位置/结果                                                           |
| ------------------------------------- | ----------------------------------------------------------------------- |
| 新队列缺查看/撤回，与宪法不完整对齐   | Task 7–9补原子撤回、墓碑、设备权限、端点及UI；未增加插队                |
| ActionStore无法取回重启派发输入       | QueuedAction及完整register/next_queued合同，恢复仍用原client_message_id |
| NotSent→fallback没有预留交接合同      | 唯一FallbackPermit、handoff_not_sent CAS，同action不回queued            |
| Unknown pending没有可表达DTO          | PendingKind::Unsupported、actionable与disabledReasonCode                |
| 原生发送来源标记未规定                | 统一原文存储、A/compat各自恰好组装一次，覆盖恢复测试                    |
| send-info的H3门仍会禁用composer       | 与send同源路由，新增H3关闭时真实composer发送回归                        |
| H8 helper返回不代表对应Turn结束       | 持久compat fence、只读关联证据；重启后也不越序                          |
| 审计DAO吞错，事务保证不成立           | record_conn_checked、同事务reserve与审计，拒绝INSERT触发器回归          |
| 确定NotSent没有Turn，不能等待完成解锁 | finish同事务清fence例外，CLI/探针拒绝后的恢复用例                       |

工程审查者已复核上述原工程问题及最后的NotSent例外，限定复核结论 **Ready**。规格审查者的二次调用遇服务额度限制未执行；主审已逐条检查其五项原发现的修订，不将未运行的复审写成通过。本次只修改文档，未执行产品测试、未创建实现分支；运行检查及双平台实机结论仍由执行期逐格填写。
