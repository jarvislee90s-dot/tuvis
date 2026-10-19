# Codex Desktop IPC · Windows 4894.0 真机全量探测结果（波次 A · T0–T10）

> 本文件为 2026-10-19 探测批报告的入库副本（探测计划：
> `docs/superpowers/plans/2026-10-19-codex-ipc-windows-4894-full-probe-plan.md`；
> profile 登记提交：`3946fa47`）。原始帧/日志（raw）与探针工程按纪律留在探测机
> `E:\LLMproject\Github\tuvis-verify-logs\2026-10-19-4894\`（不入库）；本仓证据面 =
> `src-tauri/tests/fixtures/codex-desktop-ipc/observed/windows-4894-*.sanitized.json`。
> 入库时仅将脱敏声明句中的本机用户名替换为 `<user>`，其余一字未改。

# Codex Desktop IPC · Windows 4894.0 真机全量探测结果（波次 A · T0–T10）

> 日期：2026-10-19 · 执行者：探测执行 Agent（Windows 真机）
> 代码基线：worktree `E:\LLMproject\Github\tuvis-pr165-review` · 分支 `codex/codex-desktop-ipc-review` ·
> HEAD `b9a9c87c20a393239222593ddbf44a9c4fdafa4a`（工作树干净）
> 目标 Desktop：`OpenAI.Codex` MSIX **26.1013.4894.0**
> 证据根：`E:\LLMproject\Github\tuvis-verify-logs\2026-10-19-4894\`（raw / sanitized / prodprobe3 / tools）
> 纪律：波次 A 全程 tuvis 应用未运行；只取证、未翻生产 flag；raw 与本文均不入库。

---

## 1. 环境事实表

| 项 | 实测值 | 证据 |
| --- | --- | --- |
| OS | Windows 11 家庭版 中文版 **10.0.26200** x64 | `raw/t0-1.jsonl`（`os`/`osVersion`） |
| Desktop 包版本 | `OpenAI.Codex` **26.1013.4894.0**（`AppxManifest.xml` 的 `Identity/@Version`） | `raw/t0-1.jsonl` `packageVersionsFromManifest` |
| exe 路径 | `C:\Program Files\WindowsApps\OpenAI.Codex_26.1013.4894.0_x64__2p2nqsd0c76g0\app\ChatGPT.exe` | T0 进程枚举（15 个 ChatGPT.exe，同一 exe） |
| 管道 | `\\.\pipe\codex-ipc` | T0 / 各任务 |
| 管道服务端 | `GetNamedPipeServerProcessId` → **PID 12536 = ChatGPT.exe** | `raw/t0-1.jsonl` `pipeServer` |
| 生产 `resolve()` | `Some("26.1013.4894.0")`（platform=windows） | `raw/t0-1.jsonl` `resolve` |
| `profile_for("windows","26.1013.4894.0")` | **None**（当前基线，预期行为；能力保守关闭） | `raw/t0-1.jsonl` `profileFor` |
| 生产 `connect_with_allowlist(["ChatGPT.exe"])` | **成功，8 ms**（本 PR 新加的服务端身份校验在真管道上通过） | `raw/t0-1.jsonl` `connectWithAllowlist` |
| tuvis 分支 / HEAD | `codex/codex-desktop-ipc-review` / `b9a9c87c`（≥ `5861a6d8`） | `git rev-parse` |
| 探针 | `prodprobe3`：`#[path]` 逐字编入 12 个生产源（types/codec/profile/history_view/mirror/owners/pending/connection/adapter/transport{mod,windows,unix}）+ 真实 `connector`；rustc/cargo 1.97.0 | `prodprobe3/`、`raw/*.jsonl` |
| 测试项目 | Desktop 项目 `tuvis-codex-ipc-probe`（根 `C:\Temp\tuvis-codex-ipc-probe`）；预检会话 `hi` 仅只读使用 | `raw/threads-list.txt` |
| 审批强度（T6/T7） | `approval=on-request` + `sandbox=managed`（用户经 UI 切换后实测入库值）；选择器截图 `raw/approval-mode-user.png` | `raw/t6-1.jsonl`、`tools/check_modes.py` 输出 |
| 协作模式（T8） | **无 UI 选择器**；用户输入 `/plan` 斜杠命令后，新 Turn 实体 `params.collaborationMode.mode = "plan"` | `raw/t8-1.jsonl`、`tools/modes.py`、用户现场确认 |
| Desktop 启动方式 | 用户手工启动；T9 重启由执行者用 `explorer shell:AppsFolder\OpenAI.Codex_2p2nqsd0c76g0!App` 拉起 | `raw/t9-desktop-restart*.log` |
| 会话创建方式 | APP UI 新建（空会话不落 session，见 T3） | 用户现场确认 + `state_5.sqlite` 查询（19:06 仅新增 4 行 marker 会话） |

**任务 → threadId（脱敏占位；完整值见 raw 帧）**

| 任务 | 占位 | 用途 |
| --- | --- | --- |
| T1/T2 | `tid-precheck` | 用户预检会话 `hi`，只读 |
| T4/T5/T9/T10 | `tid-t4` | start-turn / streaming / 重启恢复 / 多 Thread |
| T6 | `tid-t6` | command approval 双臂 |
| T7 | `tid-t7` | permissions 尝试（降级） |
| T8 | `tid-t8` | Plan 阻塞问答 |

所有者（ownerClientId）在全部任务中恒为同一个 Desktop 客户端（占位 `owner-desktop`）。

---

## 2. 能力 × 证据矩阵

| 能力 | 结论 | 证据（raw / sanitized） | 关键帧摘录 |
| --- | --- | --- | --- |
| `send` | **PASS** | `raw/t4-1.jsonl`；`sanitized/windows-4894-start-turn.sanitized.json` | `thread-follower-start-turn` v2 → `resultType:"success"`，`turn.status:"inProgress"` → 轮询至 `completed`；19 次增量帧、revision 1→20 |
| `state` | **PASS** | `raw/t1-2.jsonl`；`sanitized/windows-4894-snapshot.sanitized.json` | initialize → discovery → following → snapshot；`contentState` Loading→Ready；`load-complete-history` 成功返回 `revision:3`，随后 snapshot rev3 |
| `command` | **PASS** | `raw/t6-1.jsonl`；`sanitized/windows-4894-command-approval-{accept,decline}.sanitized.json` | 两个 `item/commandExecution/requestApproval`（id 59/62）；decision accept/decline 均 `{"ok":true}`；accept 文件真实存在、decline 文件不存在 |
| `permissions` | **NOT_AVAILABLE** | `raw/t7-1.jsonl`、`raw/t7-cleanup.log` | 未出现 `item/permissions/requestApproval`；外部文件**读取**升权实际走 `item/commandExecution/requestApproval`（id 65，`Get-Content`） |
| `blocking_input` | **PASS** | `raw/t8-1.jsonl`；`sanitized/windows-4894-plan-input.sanitized.json` | `item/tool/requestUserInput`（id 56，整数 id）→ `thread-follower-submit-user-input` v1 → `{"ok":true}`；Turn 继续到 completed；该 Turn `mode:"plan"` |

> 分析用的 `ThreadMirror::project` 采用 caps 全开（只为取真实 kind）；真实 profile caps 仍为全关（T0）。

---

## 3. 版本核对表（11 个方法 + 实收信号面）

| # | 方法 | 我方发送版本 | 实际结果 | 证据 |
| --- | --- | --- | --- | --- |
| 1 | `initialize` | 无（router 拦截） | **成功**，回执 `result.clientId` | 全部 raw |
| 2 | `thread-owner-discovery` | **1** | **成功**，`handledByClientId` + `supportsUntrustedAppInput:true` | t1/t4/t6/t7/t8/t10 |
| 3 | `thread-stream-following-changed` | **1**（broadcast） | 接受：declaration 后收到 targeted snapshot | t1-2 等 |
| 4 | `thread-stream-following-status-requested` | —（**未收到**） | NOT_TRIGGERED | 全部 raw 无此 method |
| 5 | `thread-stream-state-changed` | 接收 **11** | 接受：snapshot/patch 全量送达 | t1/t4/t5/t6/t7/t8/t10 |
| 6 | `thread-follower-start-turn` | **2** | **成功**（6 次；含 T7 忙态重复，见 §6） | t4/t6/t7/t8 |
| 7 | `thread-follower-load-complete-history` | **1** | **成功**，`result.revision` | t1-2 |
| 8 | `thread-follower-command-approval-decision` | **1** | **成功**（accept/decline 各一次 + 清理一次） | t6-1、t7-cleanup |
| 9 | `thread-follower-permissions-request-approval-response` | **未发送**（无请求） | NOT_AVAILABLE | t7-1 |
| 10 | `thread-follower-submit-user-input` | **1** | **成功** | t8-1 |
| 11 | `client-status-changed` | 接收 **0** | 接受（109 帧） | 全部 raw |
| 12 | `ipc-connection-reset` | —（**未收到**） | NOT_TRIGGERED | 全部 raw 无此 method |
| 附 | `thread-read-state-changed` | 接收 **3** | 接受（4 帧，未消费） | t6/t7 raw |

**结论：`method_versions` 表在 4894.0 上不需要漂移修正。** 所有已发方法的基值（discovery 1、following 1、start-turn 2、load-complete-history 1、command 1、submit-user-input 1）均一次命中、无版本拒绝；`no-client-found` 仅出现在「Thread 未被 Desktop 持有」的窗口（T9 重启后用户打开会话前），不是版本拒绝。`thread-follower-*` 的 hostId +1 臂未使用（envelope 不带 hostId，与本 PR 生产构造一致）。

---

## 4. T0–T10 逐任务小节

### T0 环境取证 — PASS

- 步骤：读 MSIX 清单版本 → 枚举 ChatGPT 进程 → 生产 `WindowsEndpointResolver.resolve()` → `profile_for` → 单独打开管道取服务端 PID/exe → 生产 `connect_with_allowlist`。
- 实际：包版本 `26.1013.4894.0`（15 个进程一致）；`resolve()` 端到端 `Some("26.1013.4894.0")`（第一批 P1b 的运行态复验）；`profile_for` = **None**（预期）；管道服务端 PID 12536 = `ChatGPT.exe`；白名单连接 8 ms 成功。
- 偏差：无。

### T1 只读协议往返 — PASS

- 步骤：initialize → `thread-owner-discovery` → `thread-stream-following-changed{following:true}` → targeted snapshot → 只读 `thread-follower-load-complete-history` → 等新 snapshot → `following:false` → 关闭。
- 实际（`raw/t1-2.jsonl`）：
  - 首帧 snapshot **revision 2**：`turnHistory.history.isComplete:false`，生产 `HistoryView` 可读、5 个 turn、18 条消息、`contentState:"Loading"`（生产 `ThreadMirror` 投影）。
  - `load-complete-history` v1（params `{conversationId, hostId:"local"}`，**无 target**）→ `resultType:"success"`，`result:{"revision":3}`。
  - 随后收到 rev3 snapshot：`isComplete:true`、10 个 turn、28 条消息、`contentState:"Ready"`、`stale:false`、`availability:"controllable"`。
- 失败全文：无（首个探针版本曾在 `projection_summary` 截断中文时 panic，属探针自身 bug，修复后重跑；见 §6）。
- 偏差：无。

### T2 形态一致性 — PASS（金样 5 条同构）

见 §5。两路样本：T1 的 rev3 全量 snapshot（`turn:` 键）与 T4 终态（`tail:0:local:` 键，含流式尾部形态）。

### T3 空 Thread 首帧 — **NOT_AVAILABLE**

- 步骤：按计划在 APP 新建空会话，不发消息，capture 首帧。
- 实际：用户现场确认「打开新会话不发内容不会创建 session」；数据库 `threads` 表在 19:06 只新增了 4 行（对应 t4/t6/t7/t8 四个 marker 会话），**不存在空会话行**；空会话没有 threadId，`thread-owner-discovery` 无从定位。
- 判定：空历史 `contentState == Ready` 的判据在本批**无法取得活体样本**，如实记 NOT_AVAILABLE（未伪造、未用「有消息会话」冒充）。
- 备注：这是 Desktop 产品行为（未发送的草稿不落 session），不是 Tuvis 缺陷。

### T4 start-turn v2 写实证 — PASS

- 步骤：following 就位后发 `thread-follower-start-turn` v2（prompt `tuvis-probe-start-turn-92wjbi12`，仅一句），轮询 snapshot。
- 实际（`raw/t4-1.jsonl`）：
  - 回执 `resultType:"success"`，`result.result.turn = {"id":"…","status":"inProgress","itemsView":"notLoaded"}`。
  - 生产 mirror 观测到 19 次 state-changed，revision 1→20；Turn 终态 `completed`（`HistoryView.turn_status` 可认领）；`contentState:"Ready"`、10 条消息。
  - **`params.input` 实况**（为「开场补齐」留底料）：请求侧 `turnStart.request.input = [{"type":"text","text":…,"text_elements":[]}]` 被原样接受；Desktop 落库后的 turn `params.input` 为同形数组，并**补写**了 `cwd`、`approvalPolicy`、`sandboxPolicy`、`permissions`、`collaborationMode`、`runtimeWorkspaceRoots`、`model`、`effort` 等字段——即 IPC 只负责 input/thread，其余由 Desktop 按线程设置填充。
- 偏差：无。

### T5 streaming 投影 — PASS

- 步骤：离线重放 T4 raw 的 22 个 `thread-stream-state-changed` 帧进生产 `ThreadMirror`。
- 实际（`raw/t5-run.log`）：revision **1→22 单调递增**；消息数 8→10（新增 2 条）；`contentState` `Ready`（rev1）→ **`Unsupported`（rev2，单拍）** → `Ready`（rev3 起）；同气泡 `replace` 0、追加 2（本会话无 streaming replace 样本——因 t4 会话已有历史，且该轮为短回答）。
- 偏差：未取到计划预期的「0→N」投影（依赖空 Thread，T3 不可得）；如实记录实际 8→10。rev2 的瞬态 Unsupported 见 §6。

### T6 command approval — PASS（accept / decline 双臂）

- 步骤：`tid-t6`（on-request/managed）发 A3 accept 轮 → 捕获 `item/commandExecution/requestApproval` → 探针 accept → 观察命令执行 → decline 轮 → 探针 decline。
- 实际（`raw/t6-1.jsonl`）：
  - **accept 轮**：request id **59**，`kind:"command"`，`reason:"是否允许我在当前项目沙箱可写范围之外创建指定目录和文本文件？"`，`command` 为 `New-Item …; Set-Content … accept-4zt2rp.txt`，`availableDecisions:["accept",{acceptWithExecpolicyAmendment:…},"cancel"]`。应答 `thread-follower-command-approval-decision` v1 `decision:"accept"` → `{"ok":true}`；Turn completed；`C:\Temp\tuvis-ipc-probe-approval\accept-4zt2rp.txt` **真实存在**，内容 `tuvis ipc approval probe accept`。
  - **decline 轮**：request id **62** → `decision:"decline"` → `{"ok":true}`；Turn completed；`decline-d9vamd.txt` **不存在**；模型如实回答「升权写入请求被拒绝，因此未创建目录或文件」。
- 偏差：无。UI 审批卡全程未被人工点击（探针应答后自行消解）。

### T7 permissions approval — **NOT_AVAILABLE**（premise 不成立）

- 步骤：`tid-t7`（on-request/managed）发 A3 approve 轮 prompt（读取 `C:\Temp\tuvis-ipc-probe-approval\approval-test.txt`）→ 等 `item/permissions/requestApproval` → 探针 approve；再发一轮 → 探针 reject。
- 实际（`raw/t7-1.jsonl`）：
  - **没有** `item/permissions/requestApproval`。外部文件读取的升权实际以 **`item/commandExecution/requestApproval` id 65** 出现：`Get-Content -LiteralPath 'C:\Temp\tuvis-ipc-probe-approval\approval-test.txt' -Raw`，`reason:"是否允许我读取 … 以返回其中内容？"`。
  - 探针按设计只等 permissions，故未应答 → 第一轮 Turn 停在 active（`pending:[n:65]`）。
  - 第二轮 start-turn 在忙态发出：Desktop **返回同一个 turn id**（`01a153e1-…`）并 `success`，未创建新 Turn；第二轮未产生任何新 pending。
  - 收尾：`probe decide … 65 decline` → `{"ok":true}`，Turn 释放（`raw/t7-cleanup.log`）。
- 判定：T7 记 **NOT_AVAILABLE**；`permissions` 能力证据缺失，profile 应保持 `permissions=false`。读场景在本 build 不是 permissions 请求的触发路径。

### T8 Plan 阻塞问答 — PASS

- 步骤：用户在 `tid-t8` 输入 `/plan`（模式指示变为 plan）→ 探针发 A3 T8 prompt → 捕获 `item/tool/requestUserInput` → `thread-follower-submit-user-input` 按原 qid 作答 → 观察 Turn 继续。
- 实际（`raw/t8-1.jsonl`）：
  - 新 Turn `01a153e0-…` 的 `params.collaborationMode.mode = **"plan"`**（`tools/modes.py` 复核）。
  - 请求：`method:"item/tool/requestUserInput"`, `id:56`（**整数 id**），`params.questions:[{"id":"archive_method","header":"选择归档方式","question":"…","isOther":true,"isSecret":false,"options":[2 项]}]`, `isBlocking:true`, `autoResolutionMs:null`。
  - 作答：`{"conversationId":…,"requestId":56,"response":{"answers":{"archive_method":{"answers":["按文件修改日期（推荐）"]}}}}` → `{"ok":true}`；Turn completed（rev41）。
  - **Default 模式的异步提问**（预检会话里出现过「request_user_input 不可用」的回复）未计入本任务；本任务确为 Plan 模式阻塞请求。
- 偏差：无。

### T9 Desktop 重启恢复 — **PASS**（断开→stale→重发现→重 following→新 snapshot 全链）

- 步骤（A4）：following 在场 → `taskkill /IM ChatGPT.exe /F` → 观察断开与 stale → 重启 → 观察重发现/重 following/新 snapshot。
- 实际（`raw/t9-1.jsonl`、`raw/t9b-4.jsonl`、`raw/t9-desktop-restart*.log`）：
  - **断开**：强杀后 **0.94 s** 收到 EOF（`19:28:50.578`）；生产 mirror 随即 `stale=true`，视图保留（只读）。
  - **重启**：`19:29:01` 经 `shell:AppsFolder\OpenAI.Codex_2p2nqsd0c76g0!App` 启动；约 **18 s**（第 10 次尝试）首次连上管道并完成 initialize / 进入 discovery（第 7 次起 `resolve()` 已能读到 `26.1013.4894.0`，但管道文件直到第 10 次才可连）。
  - **重启后 Desktop 不自动持有任何 Thread**：在用户点开会话之前，`thread-owner-discovery` 连续 150 次/300 s、571 次/1200 s 均 `no-client-found`（两次重启合计约 54 min）——Desktop 只「拥有」当前打开的会话，重启后不会自动恢复打开态。
  - **恢复（补测）**：用户在 UI 点开 `tid-t4` 后，监听进程第 7 次探测（`elapsedMs:12208`，即点开后 ≤2 s 内）拿到新 owner：
    - 旧 owner `66703927-…` → 新 owner **`1bc60017-…`（已换代）**；我方 clientId 也由 `95c35048-…` 变为 **`a198faa2-…`**。
    - 随即重新 following → 收到新 snapshot：`revision:1`、`stale:false`、`contentState:"Ready"`、6 条消息（旧 epoch 的 revision 24 未被复用）。
  - **恢复耗时**：从「会话被打开」到「新 snapshot 就位」≤ 2 s（探测粒度 2 s）；从强杀到恢复总计约 55 min（其中绝大部分是用户未打开会话的等待，不是协议耗时）。
- 判定：**PASS**。旧 owner 身份失效、旧 epoch 帧未被复用、恢复后 `stale=false` 均满足；唯一条件是「Desktop 侧需打开该会话」，这与「owner 是临时身份、Desktop 只持有打开中的 Thread」的设计一致。
- 偏差：首轮 300 s + 1200 s 窗口内未恢复，原因已定位为「Desktop 重启后停在工作台首页、未打开任何会话」，非 Tuvis 重试缺陷；补测后恢复链完整。

### T10 多 Thread 共 owner — PASS

- 步骤：同一连接对 `tid-t4` 与 `tid-t7` 分别 discovery → following → snapshot。
- 实际（`raw/t10-1.jsonl`）：两 Thread 的 `handledByClientId` **完全相同**（`66703927-…`），我方 clientId 为另一值；A rev23、B rev1，均 `stale:false`、`contentState:Ready`。F1（一个 ownerClientId 映射多 Thread）成立。

---

## 5. T2 一致性核对表（金样 5 条）

样本 A：T1 rev3 全量 snapshot（10 turn，`turn:` 键，`isComplete:true`）。
样本 B：T4 终态（2 turn，`tail:0:local:` 键，流式尾部）。

| # | 金样事实 | 结论 | 实测 |
| --- | --- | --- | --- |
| 1 | 实体按 `turnId` 字段认领；map key 只是引用；**实体无 `type`**；`tail:` 键尾 UUID 可与 turnId 不同 | **✓** | A：10/10 实体有 `turnId`、0 个有 `type`、键前缀仅 `turn`。B：2/2 实体有 `turnId`、0 个有 `type`、键前缀 `tail`，**2/2 的 tail 键尾 UUID ≠ 实体 turnId** |
| 2 | canonical 顺序 = `islands[].entries` 数组序；引用键取 `entry.value`（`entry.key` 可不同） | **✓** | A：10 个 entry、0 悬空引用、`entry.key==entry.value`（本样本未出现 key≠value 臂）；B：2 entry、0 悬空。顺序与镜像投影一致 |
| 3 | 正文在 `turn.items[]` 内联对象（camelCase）：`userMessage.content[].type=="text"` 的 `text`、`agentMessage.text`；`error` 无正文；`reasoning` 产品语义过滤 | **✓** | A 的 item 类型：userMessage/agentMessage/commandExecution/reasoning；B 另含 error。camelCase 文本检查全通过；`commandExecution` 以 `[unsupported item: commandExecution]` 占位（生产投影既有行为） |
| 4 | 完整性分层：Turn 列表 = `history.isComplete`；单 Turn = `itemsPagination`（缺省不作不完整） | **✓** | A：`isComplete:true` 且 10/10 turn 有 `itemsPagination`；T1 首帧 `isComplete:false`（Loading）→ load 后 true（Ready），分层判据活体复现。B（流式尾部）：`isComplete:true`，2 个 turn 无 `itemsPagination`（缺省），生产按「不作不完整论」处理 |
| 5 | 目标 Turn 实体以 `turn:<id>` / `tail:<…>` 键出现、`status:"completed"` | **✓** | A 中历史 Turn 实体键为 `turn:<uuid>`；B 中新 Turn 实体键为 `tail:0:local:<另一 uuid>`（键尾 ≠ turnId）。两种键下均以实体 `turnId` 认领、终态 `completed`（T1 的 10 个历史 turn 全部 completed；T4 的新 turn 亦 completed） |

差异帧引用：`raw/t1-2.jsonl`（A）、`raw/t4-1.jsonl`（B）、`raw/t2-t1.log`、`raw/t2-t4.log`。

**结论：Windows 4894.0 与 macOS 金样同构，无 Critical 形态差异；T4 以后写侧任务无需按 §3.2 停止。**

---

## 6. 负面与边界

1. **`no-client-found`（只读）**：`thread-owner-discovery` 空 params / 仅 hostId 恒返回
   `{"resultType":"error","error":"no-client-found"}`；只有带 `conversationId` 且该 Thread 被 Desktop 持有才成功（`raw/discovery-scan-1.jsonl`）。据此确认「discovery 不提供 Thread 列表」，threadId 必须由 APP/本地存储侧定位。
2. **忙态 start-turn 静默复用（Important）**：T7 第二轮在 Turn 进行中发 `thread-follower-start-turn` v2（新 `clientUserMessageId`、新 prompt），Desktop 返回 **同一个 turn id** 且 `resultType:"success"`；最终快照中只有 marker 的 userMessage，**第二轮 prompt 未落成 Turn/条目**。⇒ 若 Tuvis 的忙态屏障失效，消息会被「成功回执」掩盖而静默丢失；生产「忙态留队 + 终态证据」策略是必需的，不能把 start-turn 的 success 当投递成功。
3. **`contentState` 瞬态 Unsupported**：T5 重放 rev2（Turn 起始 patch）时投影为 `Unsupported` 一拍，rev3 恢复 `Ready`；T7 的 rev25 也出现同现象。生产 `content_state_of` 对「patch 后 turnHistory 暂不可读」的中间态无去抖，前端可能闪现「读取异常」。建议按 Minor 登记（不是本批阻塞项）。
4. **permissions 请求未触发**：外部文件**读**的升权走 command approval（id 65），不是 `item/permissions/requestApproval`；permissions wire 在本 build 的读场景下 NOT_TRIGGERED。
5. **Desktop 重启后不会自动打开会话（F6 的验收条件）**：两次 `taskkill /F` + 重启，管道约 18 s 恢复，但 Desktop 停在工作台首页、不打开任何会话；在用户点开会话前，`thread-owner-discovery` 在 300 s + 1200 s 两个监听窗内始终 `no-client-found`（合计约 54 min）。用户点开 `tid-t4` 后 **≤2 s** 内 discovery 成功、新 owner 换代、新 snapshot `stale=false`。⇒ 「重启恢复」的完整判据应包含「Desktop 侧打开会话」这一前置；Tuvis 的重试循环本身工作正常。
6. **探针自身失败（非产品）**：首次 T1 在 `projection_summary` 用字节切片截断中文导致 panic（`byte index 120 is not a char boundary`）；改为按字符截断后重跑通过。原始 panic 输出保留在 `raw/t1-run.log`，T1 成功证据为 `raw/t1-2.jsonl`。
7. **模型侧重连噪声**：多个 Turn 的 items 里含 `error`「Reconnecting... 2/5..5/5」（`responseStreamDisconnected`），属 Desktop↔模型网络重试，不影响 IPC 结论；已按结构保留在 raw。
8. **进程状态记录粒度（纪律偏差，如实申报）**：红线 7 要求「每个探测动作前后记录 Desktop 进程状态」。实际只在 T0（15 个 ChatGPT.exe）、T9 重启链路（0 → 8/9 → 14/15）与两次重启日志中落盘；T1–T8/T10 期间未逐动作单独记录（Desktop 全程未重启、进程数稳定）。后续批次如需严格逐条，可把 `Get-Process` 采样并入探针每个子命令的入口/出口。

---

## 7. 波次 B 验收表（E1–E12）

**未放行**：波次 B 依赖主 Agent 审计 PASS 后提交 profile 登记，本批未执行，全部记 **NOT_TRIGGERED**。

| # | 格 | 状态 |
| --- | --- | --- |
| E1 | 四档显示 | NOT_TRIGGERED |
| E2 | 连接+owner+following | NOT_TRIGGERED |
| E3 | 发送（F3） | NOT_TRIGGERED |
| E4 | streaming/history（F2） | NOT_TRIGGERED |
| E5 | 队列接力 | NOT_TRIGGERED |
| E6 | command 审批（F4） | NOT_TRIGGERED |
| E7 | permissions 审批（F4） | NOT_TRIGGERED |
| E8 | Plan 问答（F5） | NOT_TRIGGERED |
| E9 | 重启恢复（F6） | NOT_TRIGGERED |
| E10 | 降级对照（F7） | NOT_TRIGGERED |
| E11 | Unknown 不重发（F7/S5） | NOT_TRIGGERED |
| E12 | 队列撤回 | NOT_TRIGGERED |

---

## 8. 遗留与建议

### 8.1 缺陷分级（本批发现）

| 级别 | 发现 | 建议 |
| --- | --- | --- |
| **Important** | 忙态 `start-turn` 返回 success 但新 prompt 不落 Turn（T7 第二臂）；唯一可信的忙态信号是 `runtimeStatus`/终态证据，不是写回执 | 保留并强化生产忙态屏障；补一条「写 while busy 不得被 success 欺骗」的回归说明/测试；报告验收表 E5 需重点核 |
| **Important** | Desktop 重启后不会自动打开任何会话；用户点开会话前 owner 恒不存在（本批 54 min 无 client，点开后 ≤2 s 恢复） | E9 验收必须包含「用户/应用重新打开会话」这一前置；恢复判定坚持「有 snapshot 才算恢复」，不按管道可连/initialize 成功宣告恢复 |
| **Minor** | `contentState` 在 Turn 起始 patch 时瞬态 `Unsupported`（rev2 / rev25） | 前端对 Unsupported 做短暂去抖，或把「patch 中 turnHistory 暂缺」映射为 Loading |
| **Minor** | 空会话不落 session ⇒ 真空会话 Ready 判据无法活体验证 | 后续批次可用「先发一条消息再删 Turn」或 app-server 建空 thread 的方式补样本；本批不伪造 |
| **Minor** | `item/commandExecution/requestApproval.availableDecisions` 不含 `decline`（仅 accept/amendment/cancel），但 `decision:"decline"` 被接受并生效 | 生产表可继续用 decline；若未来 build 收紧，需按「decline 被拒」降级为 cancel/显式拒绝 |
| Nit | 探针在中文截断处 panic（已修，仅探针） | 无需产品动作 |

### 8.2 对 profile 登记的建议 caps（4894.0）

| 能力 | 建议值 | 依据 |
| --- | --- | --- |
| `send` | **true** | T4 start-turn v2 成功 + Turn completed |
| `state` | **true** | T1 initialize/discovery/following/snapshot/load-complete-history 全链 |
| `command` | **true** | T6 accept/decline 双臂 + 文件落盘/未落盘 |
| `permissions` | **false** | T7 NOT_AVAILABLE，无 `item/permissions/requestApproval` 证据 |
| `blocking_input` | **true** | T8 requestUserInput 原 id 作答 + Turn 继续 |

> 建议主 Agent 以本表在 PR 分支登记 `("windows","26.1013.4894.0")`；**本批未改任何生产 flag**。

### 8.3 对 `method_versions` 的建议

**不漂移**：11 个方法在 4894.0 上均按现表基值工作（§3）。无需修改版本表；`thread-follower-*` 的 hostId +1 臂仍未使用。

### 8.4 脱敏夹具清单（`sanitized/`）

| 文件 | 内容 | 备注 |
| --- | --- | --- |
| `windows-4894-snapshot.sanitized.json` | T1 rev3 全量 snapshot（10 turn / 28 msg，含 userMessage/agentMessage/commandExecution/reasoning） | 金样同构，可入 observed/ |
| `windows-4894-start-turn.sanitized.json` | start-turn v2 请求 + success 回执（含 `turn.itemsView:"notLoaded"`） | — |
| `windows-4894-command-approval-accept.sanitized.json` | `item/commandExecution/requestApproval`（accept 轮）+ decision + `{"ok":true}` | — |
| `windows-4894-command-approval-decline.sanitized.json` | 同上（decline 轮） | — |
| `windows-4894-plan-input.sanitized.json` | `item/tool/requestUserInput`（整数 id 56）+ submit-user-input + `{"ok":true}` | — |
| ~~`windows-4894-empty-thread.sanitized.json`~~ | **未生成** | T3 NOT_AVAILABLE，无样本，不伪造 |
| ~~`windows-4894-permissions-{approve,reject}.sanitized.json`~~ | **未生成** | T7 NOT_AVAILABLE，无请求帧 |

脱敏口径：UUID（含复合键/路径内子串与 dict key）→ 占位 UUID；用户/助手正文与 prompt → Synthetic；命令/输出/理由 → `<redacted>`；用户目录 → `<user>`。已扫描确认无真实 threadId、无真实正文、无 `<user>` 痕迹。

**夹具自检**：`windows-4894-snapshot.sanitized.json` 已用生产 `HistoryView` 复核（`prodprobe3 check-fixture`）——`readable:true`、`turnListComplete:true`、`health:"Ok"`、10 个 turn、§5 事实 1–3 全 `ok`（`raw/fixture-check.log`）。

### 8.5 复跑入口

```
cd E:\LLMproject\Github\tuvis-verify-logs\2026-10-19-4894\prodprobe3
cargo build --offline
target\debug\prodprobe3.exe t0
target\debug\prodprobe3.exe t1 <threadId>
target\debug\prodprobe3.exe t2 <raw.jsonl>
target\debug\prodprobe3.exe t4 <threadId>
target\debug\prodprobe3.exe t5 <t4-raw.jsonl>
target\debug\prodprobe3.exe t6 <threadId> | t7 <threadId> | t8 <threadId> | t9 <threadId> [secs] | t9b <threadId> [secs]
target\debug\prodprobe3.exe t10 <threadA> <threadB>
target\debug\prodprobe3.exe decide <threadId> <requestId> accept|decline|approve|reject
target\debug\prodprobe3.exe discovery-scan [threadId]
target\debug\prodprobe3.exe check-fixture <sanitized.json>
```

---

## 9. 审计摘要（一句话）

T1/T2/T4/T5/T6/T9/T10 均 PASS 且有 raw（+ 关键路径 sanitized）证据；T3、T7 按计划降级 NOT_AVAILABLE（产品前提不成立，未伪造）；T9 全链 PASS（断开 0.94 s → stale → 重启 → 用户打开会话后 ≤2 s 重发现/重 following/新 snapshot，owner 已换代）；`method_versions` 无漂移；建议 4894.0 的 caps 为 `send/state/command/blocking_input=true, permissions=false`。
