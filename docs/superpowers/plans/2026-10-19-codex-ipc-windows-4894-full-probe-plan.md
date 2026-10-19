# Codex Desktop IPC · Windows 4894.0 真机全量探测计划（第二批 · 一次跑完）

> 日期：2026-10-19
> 执行者：**探测执行 Agent**（Windows 真机）；报告接收与审计：主 Agent（判据见 §8，不达标打回重补，不二次立案）
> 代码基线：worktree `E:\LLMproject\Github\tuvis-pr165-review`，分支 `codex/codex-desktop-ipc-review`（HEAD ≥ `5861a6d8`，已含 15 项审查修复）
> 目标 Desktop：**OpenAI.Codex MSIX `26.1013.4894.0`**（本机已装，`C:\Program Files\WindowsApps\OpenAI.Codex_26.1013.4894.0_x64__2p2nqsd0c76g0\app\ChatGPT.exe`）
> 上游依据：`docs/superpowers/specs/2026-10-16-codex-desktop-ipc-remote-control-design.md`（F1–F8 冻结）·
> `docs/superpowers/plans/2026-10-16-codex-desktop-ipc-acceptance.md`（真机验收表）·
> `docs/superpowers/reviews/2026-10-17-codex-desktop-ipc-windows-validation-results.md`（第一批：传输层已绿、协议面 NOT_TRIGGERED）·
> `src-tauri/tests/fixtures/codex-desktop-ipc/observed/README.md`（金样 5 条钉死事实 + 脱敏约定）

---

## 0. 本批要一次性拿到的全部东西（缺一返工）

| # | 产出 | 支撑什么 |
| --- | --- | --- |
| 1 | 4894.0 **只读协议往返**全程帧捕获（initialize/discovery/following/snapshot/load-complete-history） | F1/F2 + `state` 能力证据 |
| 2 | 4894.0 **实体级 wire 形态**与 macOS 金样的一致性核对结论 | Windows 侧 canonical 读模型可信（第一批遗留「实体级真实形态未验」） |
| 3 | **空 Thread / 新建首帧**实机样本（PR 未验项） | contentState=Ready（真空会话）判据 |
| 4 | **start-turn v2 写实证**（请求+回执+Turn 终态） | F3 + `send` 能力证据；顺带采 `params.input` 实况（开场补齐的底料） |
| 5 | **command approval** accept/decline 双臂证据 | F4 + `command` 能力证据 |
| 6 | **permissions approval** approve/reject 双臂证据 | F4 + `permissions` 能力证据 |
| 7 | **Plan 阻塞问答**捕获 + `submit-user-input` 回执 | F5 + `blocking_input` 能力证据 |
| 8 | **Desktop 重启恢复**全程记录 | F6（PR 未验项：重启恢复） |
| 9 | **方法版本核对表**：11 个方法在 4894.0 上的实收版本 | method_versions 表是否有漂移（一次性的最大风险，见 §3.3） |
| 10 | **tuvis E2E 全清单**（验收表 9 格：发送/streaming/审批×2/问答/重启/降级/Unknown 不重发/四档显示） | F1–F8 可交付结论 + 验收记录 Windows 列 |

**总纪律：本批只取证，不翻生产 flag。** profile 登记在报告审计 PASS 后由主 Agent 单独提交
（探针分支的一次性登记仅服务 E2E，见 §6，绝不进 PR 分支）。

---

## 1. 用户已提供 / 执行前必须确认的前提

| # | 前提 | 未满足时的降级 |
| --- | --- | --- |
| P1 | Codex Desktop 可运行、已登录，探测时段内允许关闭/重启 | T9 记 NOT_TRIGGERED，其余照常 |
| P2 | 一个**专用测试会话**（新建，可污染），不用真实工作会话 | 无降级——没有测试会话不得开工 |
| P3 | APP 内该会话的审批设置为「执行命令需批准」 | T6 记 NOT_AVAILABLE + 截图证据 |
| P4 | 一个测试目录（建议 `C:\Users\bunny\tuvis-probe\`，内放 1 个文本文件）供权限审批场景 | T7 记 NOT_AVAILABLE |
| P5 | 测试会话可切 **Plan collaboration mode**（APP 内手动切，远程切换不在本期） | T8 记 NOT_AVAILABLE + 证据，禁止伪造 |
| P6 | tuvis 远程端已配对（已有 matebook-chrome）+ 本机浏览器可开远程 UI | 波次 B 的移动端格改用本机 Chrome，如实标注 |
| P7 | 磁盘 ≥ 2 GB（探针构建 + 捕获日志） | 先清理再开工 |

---

## 2. 红线（违反任何一条 = 本批作废）

1. **不修改 PR 分支的任何生产代码**；一次性 profile 登记只发生在探针分支（§6）。
2. **不放宽版本门、不扩大匹配、不翻生产 capability flag**（2026-10-17 报告 §5-F1 残留裁决的原话）。
3. 测试会话内**只执行无害命令**（白名单：`echo` / `dir` / `Get-Content` 读探针目录内文件）；审批决定按探测步骤指示操作，不即兴。
4. **脱敏后才可入库**：ID → 占位 UUID、正文/提示词 → Synthetic 占位（结构、类型、块数、字段名保持原样）；raw 帧只落 `E:\LLMproject\Github\tuvis-verify-logs\2026-10-19-4894\`（不入库）。不得把 token/cookie/真实会话内容写进任何将入库的文件。
5. 波次 A（harness）期间 **tuvis 应用不运行**；波次 B（E2E）期间 harness 不连。两者绝不并存（Desktop 多客户端并发行为未知，不拿探测矩阵赌它）。
6. 失败**必须留全文转录**（证据自带失败）：报错原文、帧 hex/jsonl、时间戳。不允许「失败，已重试通过」一句话带过。
7. 每个探测动作前后记录 Desktop 进程状态（`Get-Process ChatGPT`，包版本从 `AppxManifest.xml` 读）。

---

## 3. 波次 A · harness 直连真管道（tuvis 不运行）

### 3.1 重建探针工程

10-17 批的 `prodprobe2` 工程已不在磁盘，按原报告 §8.1 的方案重建：

1. 新建 `E:\LLMproject\Github\tuvis-verify-logs\2026-10-19-4894\prodprobe3\`（Cargo bin，`tokio` full + `serde_json` + `windows` crate 同 PR 分支 `src-tauri/Cargo.toml` 的版本与 feature）。
2. 生产源文件用 **`#[path]` 逐字编入**（不要 path 依赖整个 lib——会撞 tauri/webview2 静态库与 `msvcrt.lib` 桩的 LNK4003，第一批已踩坑）。编入清单（相对 `src-tauri/src/`）：
   `services/codex_desktop_ipc/types.rs`、`codec.rs`、`profile.rs`、`history_view.rs`、`mirror.rs`、`owners.rs`、`pending.rs`、`connection.rs`、`adapter.rs`、`transport/mod.rs`、`transport/windows.rs`，及它们的父 `mod.rs` 路径修正（`#[path]` 层级照抄 10-17 报告的模式；mirror/adapter 新增的子依赖一并带上）。
3. 探针 main 按「子命令」组织，每个探测任务一个子命令（`probe t0`…`probe t9`），全部帧输出到 `raw\<task>-<n>.jsonl`（一帧一行：时间戳 + 方向 + 完整 JSON）。
4. 复用生产 `connect_with_allowlist()`（服务端 exe 白名单传 `ChatGPT.exe`）——**顺带在真机上验证本 PR 新加的管道服务端身份校验**。

### 3.2 任务矩阵

| 任务 | 步骤 | 采集物 | PASS 判据 |
| --- | --- | --- | --- |
| **T0 环境取证** | Desktop 启动 → `probe t0`：生产 `resolve()` 取版本、管道服务端 PID/exe、`profile_for("windows", <读到的版本>)` 结果 | raw + 一行摘要 | `resolve()` 端到端 = `Some("26.1013.4894.0")`（第一批 P1b 的运行态复验）；`profile_for` = None（当前基线，**预期行为**） |
| **T1 只读协议往返** | initialize → thread-owner-discovery → `following:true` → 收 targeted snapshot → 只读 `load-complete-history` → `following:false` → 关闭（照 observed/README 记载的 mac 探针同序） | 全程帧 | 快照可被生产 `HistoryView` 解析；`contentState` 判定 = Ready；discovery/following 无版本错误 |
| **T2 形态一致性** | 对 T1 snapshot 逐项核对金样 5 条事实（observed/README）：entries.value 引用 / turnId 认领 / 实体无 type / 内联 camelCase items / 完整性分层（history 层 isComplete + itemsPagination） | 核对表 + 差异帧 | 与 macOS 金样**同构**；任何不符 → **停止 T4 以后全部写侧任务**，完整上报（Critical 发现：Windows 不同构） |
| **T3 空 Thread 首帧** | 在 Desktop 新建一个会话，切到该会话，capture 首帧 snapshot | 帧 + contentState 判定 | 空历史判定为 Ready（真实 0 条），不是 Loading/Unsupported；若首帧不是该形态，如实记录实际形态与判定 |
| **T4 start-turn 写实证** | `thread-follower-start-turn` v2（userMessage，正文用 `tuvis-probe <随机串>`）→ 轮询 snapshot 至该 Turn 终态 | 请求帧 + 回执帧 + 终态帧 | Turn 创建 inProgress → completed；turnId 在投影可认领；**顺带记录请求体里 `params.input` 是否可带开场输入及其实况形态**（为「开场补齐」留底料；若请求键集被 4894 拒绝见 §3.3） |
| **T5 streaming 投影** | T4 执行期间不停车持续收 `thread-stream-state-changed` | 全部增量帧 | 能看到 revision 递增 + 同气泡 replace/追加；喂给生产 mirror 得到投影 0→N 条、Loading→Ready |
| **T6 command approval** | 发一条会触发命令审批的 prompt（命令限白名单 `echo tuvis-probe-ok` / `dir`）→ 捕获 `item/commandExecution/requestApproval` → **accept** → 观察命令真执行、Turn 继续 → 再触发一次 → **decline** → 命令不执行 | 两类决定各自的请求/应答/后续帧 | 原 pending 消失；accept 后命令执行、decline 后不执行；应答 wire 与 `thread-follower-command-approval-decision` 表一致 |
| **T7 permissions approval** | 发一条触发权限请求的任务（读取 P4 目录内文件 / 打开该目录）→ 捕获 permissions 请求 → approve → 再触发 → reject | 同上 | 原权限对象回显、scope=turn；reject 后动作未执行 |
| **T8 Plan 阻塞问答** | 用户在 APP 把测试会话切 **Plan** 模式 → 发会触发阻塞提问的 prompt → 捕获 `request_user_input`（原 request id）→ `thread-follower-submit-user-input` 作答 → Turn 继续 | 请求/作答/继续帧 | 捕获原 request id 并成功作答；Default 模式异步问题**不算数**（不产生 pending request） |
| **T9 重启恢复** | following 在场 → 用户关闭 Desktop → 观察断开与 stale → 重开 Desktop → 自动重连/重发现/重 following/新 snapshot | 断开→恢复全程帧 + 时间线 | 旧 owner 身份失效（旧 epoch 帧不得复用）；恢复后 stale=false；记录各步耗时 |
| **T10 多 Thread 共 owner** | Desktop 开第二个会话 → discovery → 两 Thread 分别 following/snapshot | discovery 帧 ×2 | 一个 ownerClientId 映射多 Thread 成立（F1） |

### 3.3 版本失配排查（本批最大的「一次性」风险，规则先行）

method_versions 表是 3656.0/40934 时代定的（state-changed v11、start-turn v2、其余 v1；`thread-follower-*` 带 hostId 时基值 +1）。4894.0 若 bump 过版本，症状是**方法被拒或 `no-client-found`**（spec §3.5 约束 1）。规则：

1. 某方法首次失败 → 原样捕获错误帧，**不要盲目重试**；
2. 对该方法做**受控小矩阵**：基值 −1 / 基值 / 基值 +1（`thread-follower-*` 另乘 hostId 有无双臂），每个组合一次尝试并记录；
3. 命中即停，记录「方法 × 实收版本」；全部方法汇总成 **§0 产出 9 的版本核对表**；
4. 若某方法在 ±1 内全失败 → 记 NOT_AVAILABLE + 全文转录，不许猜更大的版本号。

---

## 4. 波次 A 收尾（暂停点）

1. 生成 `sanitized\` 夹具（对齐 observed/ 惯例，建议命名：`windows-4894-snapshot.sanitized.json`、`windows-4894-empty-thread.sanitized.json`、`windows-4894-start-turn.sanitized.json`、`windows-4894-command-approval-{accept,decline}.sanitized.json`、`windows-4894-permissions-{approve,reject}.sanitized.json`、`windows-4894-plan-input.sanitized.json`）。
2. 按 §8 写 `results.md`（放 verify-logs 目录，不入库）。
3. **停在这里**，通知主 Agent 审计。波次 B 依赖 §6 的探针分支构建，可在等待审计期间先行构建，但**不执行**。

---

## 5. （预留）波次 B · tuvis 真应用 E2E —— 主 Agent 审计 PASS 后放行

> 审计判据：T1–T10 全部 PASS/如实 NOT_AVAILABLE；T2 同构结论成立；版本核对表完整；
> sanitized 夹具抽查与 raw 一致。审计 PASS 后主 Agent 提交 profile 登记（4894.0，caps 逐项
> 按证据），随后才允许执行本波次。

在探针分支（§6）构建 tuvis（`pnpm install && pnpm tauri:dev` 或 debug 构建），远程 UI（本机 Chrome / 已配对设备）对照验收表逐格打勾：

| # | 格 | 操作 | PASS 判据 |
| --- | --- | --- | --- |
| E1 | 四档显示 | 打开测试会话详情 | 显示「可控制」（Controllable），不是「仅可使用兼容模式」 |
| E2 | 连接+owner+following | 同上 | 投影出现、非 stale；日志含管道身份校验通过 |
| E3 | 发送（F3） | 远程发一条消息 | 回执 `desktop_ipc/accepted`；**同原 Thread 恰 1 Turn**；turnId 与账本对应；消息出现在 Desktop 原会话 |
| E4 | streaming/history（F2） | E3 执行中观察 | 消息墙实时增长；contentState Ready；完成后 Desktop 与投影一致 |
| E5 | 队列接力 | 连发两条 | 第一 Turn 完成后第二条自动接力；审计时序 queued→accepted |
| E6 | command 审批（F4） | 远程触发命令审批 → 批准卡操作 approve / 另一次 reject | 原 Turn 继续/不执行；原 pending 消失 |
| E7 | permissions 审批（F4） | 同上 permissions 场景 | 同验收表判据 |
| E8 | Plan 问答（F5） | Plan 会话触发阻塞问答 → 远程作答 | 原 Turn 继续 |
| E9 | 重启恢复（F6） | tuvis 在跑时重启 Desktop | 自动重连重 following；投影恢复；无写重放 |
| E10 | 降级对照（F7） | H3 开 + A 路断（关 Desktop）→ 远程发消息 → 再 H3 关重试 | 开：走 #117 兼容（APP 消费回执）；关：诚实拒绝，不降级 |
| E11 | Unknown 不重发（F7/S5） | E3 发送后立刻强杀 tuvis 进程 → 重启 tuvis | 该动作 reserved→unknown；**不自动重发**；队列屏障在册（可用 force-continue 人工解除——顺带验收新端点） |
| E12 | 队列撤回 | 排队中撤回一条 | 行消失、审计留痕 |

---

## 6. 探针分支（一次性登记，绝不进 PR 分支）

1. `git -C E:\LLMproject\Github\tuvis-pr165-review switch -c probe/4894-e2e`（自当前 HEAD 开出）。
2. 在 `profile.rs` 的 `profile_for` 增加 `("windows","26.1013.4894.0")` 分支，**caps 逐项按波次 A 证据填写**（哪项没证据就 false），函数注释标注 `PROBE-ONLY：真机证据审计后的正式登记由主 Agent 落 PR 分支`。
3. 该分支不 push、不合并；E2E 构建只用它。审计后主 Agent 会以证据为准在 PR 分支重做登记并删此分支。

---

## 7. 产物目录约定

```
E:\LLMproject\Github\tuvis-verify-logs\2026-10-19-4894\
├── prodprobe3\            # 探针工程（可复跑）
├── raw\                   # 全部原始帧 jsonl / 日志（不入库）
├── sanitized\             # 脱敏夹具（审计后随主 Agent 入库 tests/fixtures/codex-desktop-ipc/observed/）
├── results.md             # 报告（§8 格式）
└── e2e\                   # 波次 B 截图 / 日志 / 审计导出
```

## 8. 报告格式（`results.md` 必须逐节具备，缺节打回）

1. **环境事实表**：OS、Desktop 包版本（AppxManifest 读取值）、exe 路径、管道服务端 PID/exe、`resolve()` 实测值、tuvis 分支与 HEAD。
2. **能力 × 证据矩阵**：`send / state / command / permissions / blocking_input` × `PASS / FAIL / PARTIAL / NOT_AVAILABLE`，每格注明证据文件名（raw + sanitized）与关键帧摘录。
3. **版本核对表**：11 个方法 × 发送版本 × 结果（含 §3.3 矩阵结论）；method_versions 是否需要漂移修正的明确结论。
4. **T0–T10 逐任务小节**：步骤复述、实际观察、与预期的偏差、失败全文转录。
5. **T2 一致性核对表**：金样 5 条事实逐条 ✓/✗ + 差异帧引用。
6. **负面与边界**：遇到的所有错误帧原文（no-client-found / 版本拒绝 / 忙 / 断连）。
7. **波次 B 验收表**（E1–E12，若已放行）：逐格 PASS/FAIL/NOT_TRIGGERED + 截图/日志文件名。
8. **遗留与建议**：发现的新缺陷（Critical/Important/Minor 分级）、对 profile 登记的建议 caps、对 method_versions 修正的建议值。

---

## 9. 明确不在本批范围（不许顺手做）

- 远程切换 collaboration mode / 权限档（三期 app-server 臂，摘门条件另档）；
- interrupt / steer、update-thread-settings、file approval、多窗口高级协同（spec §5）；
- `params.input` 开场补齐的**实现**（本批只采底料）；
- 修改 `docs/MASTER-PLAN.md`、spec、既有测试基线。
