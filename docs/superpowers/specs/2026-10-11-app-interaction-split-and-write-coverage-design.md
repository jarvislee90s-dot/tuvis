# APP 交互面分套与写通道全覆盖 · 设计文档（v2 · 已回填探测结论）

> 日期：2026-10-11（v1 草案）→ 2026-10-12（v2 探测回填）· 分支：`feat/h1-h12-headless`（PR #117，H1–H12 无头批已实现）
> 状态：**v2——裁决 28–34 已全部裁定（见 §8 结果列，其中 30 经用户改判），Windows 协议探测已回填（§1.5 台账）；可按 §7 分期进入实现批**
> 上位文档：`docs/MASTER-PLAN.md`（宪法，本设计**零宪法改动诉求**）；`docs/superpowers/specs/2026-09-27-phase2-closure-app-injection-design.md`（H 系总设计，其 §2 裁决表已编至 **27**——本文裁决点延续该编号从 28 起）
> 直接输入：① Task 15 统一手工测试（2026-10-11 晚 23:10–23:23，Windows 真机）四路失败 + systematic-debugging 根因定位；② **`research/refs/phase2-消息注入/2026-10-11-windows-app-write-channel-probe.md`（下称「探测报告」——本文全部协议面断言的证据台账）**；③ 研究库综合报告（Mac 段定案）；④ UI 组件现状盘点（Explore 全量盘点，回填 §3.7）
> 用户口径（2026-10-11，设计前提）：① codex 最接近完成；② codex APP 版与 CLI 版 UI 组件两套——现有逻辑固化为 CLI 版，APP 版新建分支（读侧已正常，缺写侧）；③ 目标 = codex / zcode / workbuddy / dsh 四个 APP 都能远程发消息；④ **探测优先——所有行为断言须有探测证据，不瞎写**；⑤ **dsh 并入本批**（裁决 30 改判）

---

## 1. 背景与四路根因摘要

Task 15 手工测试在 Windows 真机暴露四路失败。根因已定位（systematic-debugging，证据归档于探测报告）：

### 根因① codex APP：模式/权限切换与问答作答走错通道（**设计缺口，非环境故障**）

- **消息发送是好的**：H8 `headless_codex_queue` 成功（审计 `15:11:55 … result=ok · 1108ms`）。锚点：`src-tauri/src/remote/api.rs:2486`（`session_send`）→ `api.rs:2560-2595`（无头分派点）。
- **模式/权限切换与问答作答仍走 CLI 终端控制台通道**（审计 `channel=real`）：模式 `api.rs:10748`（`session_mode_switch`，无形态门）；问答 `api.rs:6920` → `api.rs:8089`（`dispatch_question_action`，零无头分支）。codex APP（ChatGPT 桌面 GUI）没有控制台 → `AttachConsole 0x80070006` →「附加目标控制台失败」「读不到屏——零投递」「aborted:select」。
- **定性**：H8 已登记边界（「插队 = turn/steer、审批 = turn/start approvalPolicy——本批不做，登记为协议面后续项」）。本设计 = 该后续项立项。**协议面现已 Windows 活体实证**（探测报告 §1）：`turn/start.approvalPolicy`（untrusted/on-request/never + granular，回合级生效后续沿用）、`turn/steer`（busy 插话不打断，6.8s 实证）、`turn/interrupt`（1.5s → status=interrupted）、`thread/read` 只读族跨实例可用。
- **卡面权威源纪律迁移**（AGENTS.md「屏读为准」）：APP 会话无屏可读，权威源 = **协议读回（thread/read 族）+ rollout 事件**；闭环核验语义保持（核对不符 → 回退 + 重试），不得退化为「本地记忆即真值」。

### 根因② zcode：版本门探针超时阈值与真实回合时延失配（复现实证）

- 探针 = 真实最小回合（`gate.rs:21` `PROBE_TIMEOUT_MS=15_000` 硬杀 + `PROBE_PROMPT="hi"`）。
- **实测**（探测报告 §2）：空目录 18s/19s exit=0；repo 目录 15s/16s exit=0；15s 内零输出被杀一次。CLI 本体健康（`--version`/`--help` 0.9s，0.16.9，flag 面未变）。
- 双重缺陷：a) 15s 阈值 < 实测 15–19s → 恒超时拒发；b) `gate.rs:392-393` Timeout 分支先判死不看已捕获 stdout。放大器 = 失败缓存 5min TTL；误导面 = 审计行 `· 0ms`（回执默认时长非真实耗时）。

### 根因③ WorkBuddy 消息不上看板：**工具未勾选**（配置问题）+ Windows 端点发现缺陷（探测新抓）

- 用户库 `agent_tools` 表 `workbuddy` 行 enabled=0 → W5 规则（`adapter/mod.rs:500,628`）→ 扫描无 WB 适配器（日志零行佐证）→ 看板无卡。
- **探测追加缺陷**（探测报告 §3）：Windows 内嵌 WB 远控端点在场（127.0.0.1:32462，`CodeBuddy Remote Control` 标题指纹命中），但 **`/health` 被鉴权挡（AUTH_REQUIRED）→ 生产 `health_ok()` 恒 false → 端口指纹双确认必败**——即使勾选了工具，H9 发送也会因端点发现失败而拒发。修复入 P0。

### 根因④ dsh：`dsh_headless_pending` 是 H13 范围裁决 → **用户已裁并入本批**

- `routing.rs:23-26/206-211`：H13 刻意缺席。用户裁决 30 改判「并入本批」。
- **探测实锤**（探测报告 §4）：dsh desktop 0.2.0-rc.2 **官方内嵌 ACP stdio 服务器**（`@deepseek-ai/dsh-acp`，`pnpm dsh --profile acp` 启动）——session/new / **session/list（可按 cwd 筛选）** / **session/resume（仅非活跃会话）** / session/prompt / **session/cancel** / session/set_config_option（model/reasoning_effort）/ **session/request_permission（权限提示可自动回答）**；刻意无 mode/load/fork/SSE。resume 仅非活跃 = 与桌面 APP 打开态互斥（三家写者语义一致）。

---

## 2. 动作 × 工具通道路由矩阵（目标态 · v2 已按探测刷新）

> 每格：**通道 + 权威源 + 状态**（✅ 已交付 / 🔧 本设计新增〔探测已背书〕 / 🔧? 本设计新增〔含未验项，标注〕 / ⬜ 不支持〔如实〕）。
> 「权威源」= 该格卡面状态的最终核对依据（「屏读为准」裁决在 APP 侧的替代面）。

| 动作 | codex-CLI（终端） | codex-APP | zcode | workbuddy | dsh（已裁并入本批） |
|---|---|---|---|---|---|
| **发消息** | ✅ 终端注入 + 屏读/队列确认 | ✅ H8 queue（APP 开）/ exec resume（APP 关）；权威源 = queue 消费确认 + rollout 追加 | ✅ H7 spawn 无头；权威源 = 会话库读回 | 🔧 H9 修正版（探测报告 §3）：端点发现改 connect 判据（P0）；投递 = `session/new`+`session/prompt`（**绑 APP 当前会话**，靶向限制如实标注）；权威源 = `workbuddy.db` + APP 内可见 | 🔧 H13：dsh-acp stdio `session/prompt`；**非活跃会话可 resume 靶向**（session/list 按 cwd 筛选 → resume → prompt）、活跃会话（桌面打开）= 单写者互斥如实拒；权威源 = `~/.dsh` sessions 日志（H1 已交付） |
| **模式切换** | ✅ 菜单键序 + 屏读核验 | 🔧 P1：`turn/start.approvalPolicy`（活体已证参数受理；**行为级生效留 M21**；须附极小输入——载体三选一见 §3.4） | ⬜ 不支持（`--mode` spawn 时给定，裁决 14 单一 yolo；三期 F3.1） | ⬜ **探测证伪**：Windows 版 mode 四档无通道（load 被 writer_occupied 挡 / new 无 mode 回显 / session/update 不存在）——Mac 定案 8 不适用于 Windows，维持只读 | ⬜ dsh-acp 刻意无 mode（README 明示）——如实不支持 |
| **权限档切换** | ✅ 同上（/permissions 菜单） | 🔧 P1：同模式格（approvalPolicy 即权限档） | ⬜ 同模式格 | ⬜ 同模式格 | 🔧? `session/request_permission` **逐次应答面在场**（README；活体未验）——首批只透传展示不代答，代答列 P2 |
| **问答卡作答** | ✅ 键序 + 屏读快照闭环 | 🔧 P1 **降级补投**（裁决 28b）：作答文本经 queue 投递 + 引导回 APP + `screen=null`；原生文本作答面无（探测：ServerRequest 只有审批三族 + elicitation，无问答文本面）；审批类可协议应答（P2 持连语义） | ⬜ 不在问答矩阵（`api.rs:6994` 名单外）；卡自隐 | ⬜ 未取证；如实不支持（卡自隐） | ⬜ 未取证（request_permission 是权限不是问答）；卡自隐 |
| **中止** | ✅ 终端键序 | 🔧 P1：在飞协议回合 `turn/interrupt`（1.5s 活体）；**queue 滞留项无撤回原语**（如实报）；exec resume 路 = H4 kill | ✅ H4 取消 | ⬜ cancel 未验未接线（维持 H9 登记；P2 可选） | 🔧? `session/cancel` / `$/cancel_request`（README 面；活体未验——M25 顺带首验） |
| **卡面权威源读取** | ✅ 终端屏读 | 🔧 **独立实例 `thread/read` 族跨实例只读已活体实证**（APP 持写者不挡读）；屏读快照恒 null 保守降级 | ✅ 会话库读回 | ✅ workbuddy.db + projects/ 转写（H12 双源） | ✅ ~/.dsh sessions 日志（H1） |

**三条通读结论**（不变，证据升级）：

1. 「发消息」四家：codex-APP ✅ / zcode ✅（探针修复即通）/ WB 🔧（勾选 + 指纹修复）/ dsh 🔧（dsh-acp，官方协议面文档实证）。
2. 「模式/权限/问答」三动作：codex 补 APP 臂（协议面活体背书）；zcode ⬜（无原语）、WB ⬜（**探测证伪，Windows 无通道**）、dsh ⬜（刻意无 mode）——**不趁机扩权，如实标注**。
3. 权威源随通道走；闭环核验语义跨通道不变。**写者语义三家一致**（codex writer 锁 / WB writer_occupied / dsh resume 仅非活跃）——「活跃会话由宿主 APP 持写者，MAM 只能旁路投递或排队」是 APP 通道的共性约束，路由设计统一吸收。

---

## 3. codex APP / CLI 组件分套设计（根因① 的解）

### 3.1 设计原则（用户口径的直接转写）

1. **CLI 版固化**：现有模式/权限/问答的终端交互逻辑原样保留为 CLI 版——不重写、不重构，圈进「CLI 形态专属」分派分支；
2. **APP 版新建**：APP 会话动作走独立分支（后端 = 按 `AppPresence` 分派动作臂；前端 = 双套组件），与 CLI 版零共享可变状态；
3. **读侧不动**：APP 会话卡面展示已正常（rollout 读链照旧），只补写侧。

### 3.2 前端能力位：字段盘点结论

| 字段 | 位置 | 现状 | 结论 |
|---|---|---|---|
| `Session.form`（`"cli"`/`"app"`） | sessions 载荷（`session/model.rs:94`） | 已存在；**详情页零消费**（Board 已用 `s.form==="cli"` 分关闭/归档）——分套唯一正确挂点 = SessionDetail 卡片停靠区（cardDock），`form` 已随 session prop 传入 | **分套主判据，零新增字段** |
| send-info `channels[]`（`headless_*`） | `GET /session-send-info` | 已存在，composer 已消费 | 发消息能力位已齐（链路已通道化，见 3.7） |
| GET `/session-mode`（`groups`/`switchKind`） | `api.rs:9571` 起 | 已存在；`unsupported → 不渲染` | 扩一档 `surface: "terminal"|"app"`（后端单点，防前端复制分派逻辑成第二真相源） |
| GET `/session-question`（`questions`/`available`/`screen`） | 问答载荷端点 | 已存在；`available=false` 自隐 | 载荷结构不动；APP 会话 `screen` 语义 = rollout 读回合成 |

**盘点更正（v1 已申报，维持）**：`src/components/common/mode-toggle.tsx` 是桌面主题切换（30 行），不进改动面。

### 3.3 前端双套渲染与降级

```
SessionDetail.tsx（cardDock 挂载点，按 GET 载荷 surface 选套）
 ├─ ModeBar：surface=terminal → ModeBarCli（现逻辑整体固化改名）
 │            surface=app      → ModeBarApp（协议读写、无屏读核验）
 ├─ QuestionCard：surface=terminal → QuestionCardCli（固化改名）
 │                surface=app      → QuestionCardApp（读沿用；作答 = 降级补投〔裁决 28b〕）
 ├─ ApproveCard / PlanFeedbackBar：CLI 套整体保留；APP 形态显式排除（防「请人工核对终端」文案漏出）
 └─ MessageComposer：不改逻辑——已通道化；只分词汇表与卡片在场探针（3.7）
```

- **ModeBarApp**：当前档来自协议读回；读不到 →「模式未知，请人工核对 APP」（沿用 `current===null` 降级语义）；点切档 → POST → 回执带协议读回核对（不符 → hint + 自动重拉 GET，不拿回执改本地状态）。
- **QuestionCardApp**：题目渲染照旧；作答 = 每题「补投作答」入口（单选 =「选择第 N 项：<文本>」；自由作答 = 原文补投），回执如实「已补投为消息，请回 APP 核对题面」+ `screen=null`——**永不声称已作答**（补投是自然语言，选项翻转不保证）。
- **灰置分层**（裁决 31b）：工具级无机制 → 不渲染（既有 `unsupported`）；形态级暂缺（steer/queue 撤回）→ **灰置 + 一句原因**。
- **降级链**：`surface=app` 但协议门未过 → GET 载荷给降级档 → ModeBarApp 只读 chips + 原因；QuestionCardApp 读侧不依赖协议面不受影响。

### 3.4 后端按 `AppPresence` 分派的动作臂（协议面已探测定案）

分派判据复用 H8 单点：`codex::presence_of(production_presence(form, pid))`；分派条件 = **`form==App && tool==codex`**（与 `session_send` 路由口径对齐）。

| 端点 | CLI 臂（固化不动） | APP 臂（P1，方法名均已活体/文档实证） |
|---|---|---|
| `POST /session-mode`（`api.rs:10748`） | 菜单键序 + 屏读核验 | 档位切换。**载体三选一（P1 实现时按干扰最小定，M21 验收）**：① `turn/start {approvalPolicy, input:<极小输入>}`——回合级生效后续沿用（活体已证参数受理；代价 = 会话里多一条极小消息）；② `thread/resume {threadId, approvalPolicy}`（仅 APP 关闭态可用——活跃态被 writer 锁拒）；③ `config/value/write`（写 config.toml 全局默认，粒度粗，兜底）。回执 = 协议读回三态（命中/不符/未生效） |
| `GET /session-mode` | 屏读轴 | APP 会话：`current` 改 `thread/read` 只读读回（跨实例已实证）；下发 `surface:"app"` |
| `POST /session-question`（`api.rs:6920`） | 键序 + 屏读闭环 | **降级补投**（裁决 28b）：作答文本模板经 H8 queue 通道投递（复用 `codex_headless_dispatch` + 串行锁），回执 `screen=null` + 引导文案；审计 `action=question, channel=headless_codex_queue`（实际走向） |
| 无头中止端点 | —（终端会话不在此面） | 在飞协议回合 → `turn/interrupt {threadId, turnId}`（1.5s 活体）；queue 滞留项 → 如实「已入队未消费，暂无撤回原语」；exec resume 路 → H4 kill |

**H3 总开关管辖扩展**：APP 臂四动作与 `session_send` 同门（`headless_disabled` 403 同码同文案，单点沿用）——「无头 = 绕过终端可视确认直接驱动 Agent」对协议动作同样成立。CLI 侧模式/问答不受 H3（现状不变）。**行为变化点，M24 显式回归**。

**审计口径**：action 词沿用既有；channel 列记实际走向，新增审计词 `codex_appserver`（不动 `HeadlessKind` 路由枚举——发送路由与动作分派两概念不搅）。

### 3.5 协议面连接管理（探测定案）

- **读路径（权威源）**：spawn 独立 `codex app-server`（stdio JSONL，**裸方言：响应帧无 `jsonrpc` 字段**）→ `initialize` → `thread/read`/`thread/items/list`/`thread/turns/list` 按 rollout UUID 定点读（monitor 已有 UUID 映射）。**跨实例只读已活体实证**（APP 持写者不挡读）。裸实例 `thread/list` 恒 0 线程（不枚举磁盘线程）——读回必须定点 by id，不做枚举。
- **写路径（关键未验项）**：对 APP 持有的活跃线程，独立实例 `thread/resume` 被 -32600 `already has an active writer` 拒（活体实证）→ **活跃线程的 interrupt/档位写必须走 daemon 控制插座**（`~/.codex/app-server-control/app-server-control.sock`，AF_UNIX；`codex app-server proxy` 子命令 = 官方转发器，本次 Git-Bash/Python 下零输出【未验，探测报告 §5.1】）。**P1 实现首任务 = Rust 侧打通 daemon 通道**（socket2 AF_UNIX 直连或 proxy 成帧定案）；打通失败的兜底 = 档位切换单走「APP 关闭态 resume」+「打开态如实置灰（活跃回合档位切换暂不可用）」，interrupt 同理——**不假装可写**。
- **生命周期 = per-action 短命会话**（裁决 32a）：每动作 spawn → initialize → 调用 → 读回 → 退出。常驻池维持三期 F3.3 边界。例外预留：P2 审批应答（ServerRequest 三族需回合期间持连接收请求）作为唯一持连 carve-out。
- **版本门**：协议面动作前置可用性探测（spawn + initialize + 方法在场）进 `ProbeCache` 同款缓存纪律；CLI↔APP 版本双坐标（本机实测 0.162.1 同版——漂移风险仍按风险 14 维持）。schema 317 文件已归档（0.162.1 官方导出）作素材，**运行时以探测为准**。

### 3.6 卡面权威源：APP 侧的「屏读为准」替代

| 面 | CLI 版（不变） | APP 版（新） |
|---|---|---|
| 模式当前档 | 终端屏读 | `thread/read` 协议读回；GET 重拉即核对 |
| 切档回执核验 | 屏读翻转判据 | 协议读回三态——不符 → 如实报 + 前端不采信本地记忆重拉 GET |
| 问答勾选态/自由作答 | 屏读快照四路同源 | rollout 事件读回（读侧已有）；补答回执 `screen=null` + 引导核对，永不声称已生效 |
| 读回不可用时 | 维持本地状态（不假清不假同步） | 同款保守降级：「未知，请人工核对 APP」 |

### 3.7 APP 版 UI 与终端版差异盘点 + 通用发消息结论 + 渲染分叉纪律（回应用户三问）

**（a）APP 版与终端版 UI 差异与功能缺失清单**（Explore 盘点，锚点见交付）：

- **必然失效（无控制台即无此能力）**：ModeBar 整条（shift+tab/菜单键序/屏读回读/`blocked_by_dialog|question` 守卫）；QuestionCard 的屏读权威源机制（`screen` 快照、`checked` 翻转核验、heading 对位、noteText 回填）与九段键序阶段机；ApproveCard 的终端对话框屏读（`dialog:<n>` 选项来自屏读）；PlanFeedbackBar 整条（claude 终端编辑行打字通道，标题还硬编码 Claude 品牌——分套时一并参数化）；MessageComposer 的卡片在场四态分流与「已送达终端/投递至终端输入」词汇、TUI 队列 UI（jump/edit/retract 的「忙时留队转闲 flush」语义对无头通道已天然不生效，按通道隐藏即可）。
- **纯展示、本来就跨形态可用**：消息历史流（按 `kind` 数据驱动渲染，`message-render.tsx` 明示不看 toolName）、折叠/总结模式、文件面板/预览、子 agent 清单、书签/字号、HeadlessReceiptCard + 分诊表（通道级产物，codex queue 的 queued 回执已在此渲染）、HeadlessApprovalCard（stdio 桥，channel 门扩表可复用）。
- **CLI 专属文案清单**（APP 套必须换词）：「请人工核对终端（当前）模式」「已发送切换，请人工核对终端模式」「终端有待答的问题…」「请在终端完成作答」「已发送但未能屏读核验…」「输入消息发送到终端…」「已投递至终端输入…」「终端备注（屏读）」等约 30 条（ModeBar/QuestionCard/MessageComposer/PlanFeedbackBar/ApproveCard/SessionDetail 六处）——APP 套词表统一「APP/会话」措辞，不出现「终端/屏读/按键」。

**（b）APP 版能写一个通用的发消息逻辑吗？——能，且应当**：

- 现状已半就位：MessageComposer 发送链路**按通道路由不按 form**（`send-info.channels` 的 `headless_*` 前缀 → `POST /session-send` → `HeadlessReceipt{status: ok|queued|failed|cancelled, …}` → `receipt.status==="ok"` 才清空输入 → 回执上浮 HeadlessReceiptCard → 详情页 10s 轮询刷新消息流）。四家 APP 的发消息**全部收敛到这一个契约**：codex-APP（queue/exec）、zcode（spawn 回合）、WB（ACP prompt）、dsh（ACP stdio prompt）在后端是四个通道实现，在前端是**同一套 composer + 回执卡**——分叉只在后端，前端零 tool 硬编码。
- **唯一契约缺口（前后端合解）**：codex queue 的 `queued` 是 POST 终态，回执卡不会从「排队中」翻转「已消费」——需要后端在消费/回合产出时给信号（消费确认侦测已交付，回执侧补一个状态翻转面：轮询窗口内命中消费 → 回执卡追加「APP 已消费」行，或消息流尾部 assistant 出现即翻转）；dsh/WB 的 prompt 回执天然带回合产出（WB 实测 6.8s 出 agentMessage 流），无此缺口。
- 结论：**不新建「APP 版 MessageComposer」**——共用现件 + APP 词汇表 + 隐藏 CLI 卡片探针；新写的是后端通道与（codex 的）消费翻转信号。

**（c）APP 版需要像终端版那样按工具差异化内部渲染吗？——不需要**：

- 终端版当年各异，根因是**数据源异构**（屏读文本按工具解析）；APP 版数据源统一为**结构化事件流/rollout/协议读回**，前端已有纪律：**后端下发能力位（available/unsupported/channels/supported/structure），前端数据驱动渲染、零 tool 名 switch**（仓内先例：message-render 不看 toolName、isPlanPending 工具族门、QuestionCard 能力旗、ModeBar structure、子 agent supported 位——全部后端驱动）。
- 视觉面：全局主题机制（remote_ui_config 六皮肤 + 昼夜）不动；按工具只保留**看板品牌檐色**（既有 TOOL_BRAND_COLORS）——详情页内容渲染不引入按工具/按形态的皮肤分叉。
- APP 套与 CLI 套的**形态分叉**（surface 字段）是能力位分叉，不是工具分叉：同一工具（codex）两种形态两套组件，四家 APP 共用同一套 APP 组件——差异全部由后端载荷表达。

---

## 4. zcode 探针修复设计（根因② 的解；裁决 29c = A+B 叠加）

### 4.1 两方案（维持 v1 论证，实测数据升级）

- **方案 B（证据优先于时序，主修）**：`gate.rs:392-393` Timeout 分支改为**超时被杀后先 join 已捕获 stdout**，`parse_json_skipping_prefix(stdout)` 命中 → Pass（实现注意：确认 `RunnerCfg` kill 路径不丢弃缓冲）；仅零输出才 Fail。
- **方案 A（阈值 + 文案，兜底）**：`PROBE_TIMEOUT_MS` 15s → **45s**（实测 15–19s ×2 余量）；超时 hint 改「探针回合超时（本机实测最小回合 15–19s，阈值已放宽至 45s）——反复超时请检查网络/模型服务」，替换「工具可能挂死或缺 provider config」误导归因。

四条论证（B 修逻辑缺陷、A 单独不充分、B 后 A 仍有价值〔真零输出宽窗〕、成本为零）维持 v1 §4.1。

### 4.2 顺带修复

1. `version_gate_receipt` duration=0 误导：调用点透传 `probe()` 实测墙钟（`with_duration_ms`），审计行出真实耗时。
2. 移动端 version_gate 分诊文案：「CLI flag 面已变」→「版本门控未通过（探针回合未出有效回执；详见原因行）」——跨语言锁钉两侧同步（`tests/mobile/SessionDetail.test.tsx`）；`headless_stages.json` 不动。

### 4.3 探针两级化（裁决 33a = 采纳）

L0 快面（零模型成本）= exe/cjs/provider-config 在场性 stat + `--help` 能起能印；L1 真回合（`hi`）= L0 过且无成功缓存才跑。红线：L0 串联闸门**不替代** L1（Task 8 实证 `--help` 漏判静默形态）。净收益 = 挡结构性坏的 token 浪费 + 失败原因确定性提前；不解决根因②本身（B+A 主修）。

---

## 5. WorkBuddy：勾选恢复 + Windows 端点发现修复 + 首验（根因③ 的解 · v2 按探测改写）

### 5.1 配置修复（用户操作）+ 指纹修复（P0 代码）

- 勾选：设置 → 工具管理 → 勾选 WorkBuddy → W5 扫描恢复 → H12 双源上板（M18）。
- **指纹修复（P0，探测新抓的必然失败点，已有用户侧复现）**：`wb_acp.rs` 端口指纹双确认的 `/health` 判据（`health_ok` 严格 `{"status":"UP"}`）在 Windows 内嵌版被 AUTH_REQUIRED 挡。**用户实测互证（00:13）**：WB 内开关已开、工具已勾选，远程发送仍拒发「候选 4 个端口双确认 0 命中」（审计 `16:13:27Z`）——端点在监听但 MAM 发现不了，**拒发回执里「请开启远程控制」的引导在该形态下是误导**（开关不是瓶颈）。修法：**双确认第二腿改为 `POST /api/v1/acp/connect` 活体探测**（免鉴权、即刻返回 connectionId = 比 health 更强的「是 WB 且可用」证据；探测后连接即弃）；`{"status":"UP"}` 形态保留为兼容分支（Mac 旧版）。标题指纹（`CodeBuddy Remote Control`）维持第一腿。修复后「请开启远程控制」引导只在 connect 也失败（端点真没开）时出现，语义恢复准确。

### 5.2 投递语义（探测改写：new+prompt 单臂 + 靶向限制）

- Windows 实测：`session/load` 恒 `writer_occupied`（不可用）；**`session/new` 绑定 APP 当前活跃会话**（无视 cwd 参数——实测返回 7 天前的既有会话）。
- **H9 Windows 投递臂 = `session/new` + `session/prompt`**，语义如实：**投递目标是「WB APP 当前打开的会话」**，不是 MAM 卡指定的会话——**靶向错位风险必须如实透出**：回执/审计标注「已投递至 WorkBuddy 当前会话（sessionId=…）」，前端回执卡显示该 sessionId；若与卡面会话不一致 → 明示「消息已进入 APP 当前会话，与所选会话不同」。Mac 心跳路（load+prompt 双臂）维持既有代码不动（版本分叉由端点发现形态决定走哪臂）。
- **误投申报（诚实记录）**：探测中 WB-PROBE-OK 一条回合因 new 绑定语义落进用户既有会话「询问模型身份」（scratch 会话）——该语义正是本节风险来源，已转为设计约束。

### 5.3 首验清单（修订）

| # | 用例 | 通过判据 |
|---|---|---|
| M18 | WB 勾选恢复上板（双向） | 勾选 → ≤3s 出卡；取消 → 无卡 |
| M26 | WB 端点发现修复（P0） | 指纹双确认新判据下端点可发现（本机 32462 类端口）；无 WB 远控端点时如实 `refused` |
| M27 | WB 当前会话投递（= M8 正式执行） | 消息落 APP 当前会话并执行（APP 内可见）；回执含末条 assistant 摘要 + 「已投递至当前会话」标注；sessionId 与卡面不一致时明示 |
| M28 | WB 已结束/他选会话投递 | 如实回执（writer_occupied / 当前会话与目标不一致的明示）——不谎报成功 |

mode 读写（Windows 无通道）、session/cancel 接线：维持三期/P2 登记。

### 5.4 未启用工具错误引导（裁决 34a = 设置页文案级）

设置页勾选框旁补一句「取消勾选的工具有看板与远程能力」；`no_session` 载荷不附 hint（不为边缘态扩查询面）。

---

## 6. dsh H13（根因④ 的解 · v2：已裁并入本批，探测背书）

### 6.1 范围（一句话）：**「发消息 + 中止」的 dsh-acp stdio 批，并入本批 P1 并行轨（用户裁决 30 改判）**

- **写通道**：官方 `@deepseek-ai/dsh-acp`（asar 内嵌，`pnpm dsh --profile acp` 启动，JSON-RPC stdio，stdout 纯协议流量）；
- **会话靶向（探测定案的差异化能力）**：`session/list`（分页 + **可选 cwd 筛选**）→ 命中目标 → `session/resume`（**仅非活跃会话**）→ `session/prompt`。三类目标态：① 桌面未打开（非活跃）→ resume+prompt **精确靶向**（比 WB 强）；② 桌面打开中（活跃）→ 单写者互斥，如实拒「会话正被桌面使用，请关闭后在投」（或登记 queue 语义待验——README 未提 queue，**不做臆测**）；③ 无既有会话 → `session/new`（新会话投递，MAM 无头新建语义同族）；
- **中止**：`session/cancel` / `$/cancel_request`（README 面，活体未验——M25 顺带首验）；
- **request_permission**：dsh-acp 会向客户端发权限提示（一次性允许/拒绝）——**首批处置 = 如实展示不代答**（收到即上屏「dsh 请求权限：<内容>」，代答按钮列 P2）；
- **读链路零改动**（H1 已交付）；**路由接线**：`routing.rs` 加 `HeadlessKind::DshAcp` + 替换 `dsh_headless_pending` 分支；H3 自然接管 dsh（行为变化：开关关 → dsh 发送置灰标因，验收覆盖）；
- **范围边界（不做清单）**：mode（dsh-acp 刻意无）、fork/delete/load（刻意无）、19387 web HTTP（维持 401 双结案备选）、webhook（弃）。

### 6.2 实现首任务（本批内，非独立探测批）

spawn 机制实机定案【未验，探测报告 §5.6】：`pnpm dsh --profile acp` 从 MAM 侧如何启动（候选：`~/.dsh/dsh-runtimes/…/node.exe` + asar 内 dsh 目录载入 / 桌面 host `--expose-internals` 通道 / dsh-subagent-acp 启动器）；`provider`/`model` 会话配置来源（读 `~/.dsh` 既有配置）；profile 落盘策略（本机 `~/.dsh/profiles/` 现无 acp）。协议行为全链 M25 验收。

---

## 7. 分期实施建议（v2 重排：dsh 并入 P1 并行轨）

### P0 · 修复止血批（zcode 通 + codex APP 止血 + WB 勾选与指纹修复）

| 文件 | 改动 | 根因 |
|---|---|---|
| `src-tauri/src/inject/headless/gate.rs` | ① Timeout 先 join stdout 再判（B）；② 阈值 45s（A）；③ hint 文案；④ duration 透传；⑤ L0 快面（裁决 33a） | ② |
| `src-tauri/src/remote/api.rs` | codex APP 止血门：`session_mode_switch`/`session_mode_menu`/`session_question_answer` 入口判 `(codex, App)` → 409 `app_form_pending`（「APP 会话该操作走协议通道，开发中——请在 APP 内操作」） | ① |
| `src-tauri/src/inject/headless/wb_acp.rs` | 指纹双确认第二腿改 connect 活体探测（§5.1） | ③ |
| `src/mobile/SessionDetail.tsx`（+测试） | version_gate 分诊文案（两侧同步） | ② |
| 设置页文案（裁决 34a）+ 用户勾选 WB | 文案 + M18 | ③ |

**验收门**：M18/M19/M20/M26；回归 M3/M6/M16 + CLI codex 模式/问答直通。**单点**：不动 routing/HeadlessKind/H3 门/stages 夹具。

### P1 · codex APP 写侧 + dsh H13（两并行轨，文件域不重叠；routing 单点仅 dsh 轨触）

**codex 轨**：

| 文件 | 改动 |
|---|---|
| `src-tauri/src/inject/headless/codex_app.rs`（新） | app-server JSON-RPC 客户端（执行缝注入，零真实 daemon 依赖）：spawn initialize / thread/read 读回 / 档位写三载体 / interrupt / **daemon 控制插座通道（socket2 AF_UNIX 或 proxy——首任务打通，失败走兜底置灰）** |
| `src-tauri/src/remote/api.rs` | §3.4 四臂 + H3 同门 + 补投组装 + 审计词 + `surface` 字段 |
| gate.rs（或 probe 臂） | `codex_appserver` 可用性探测进 ProbeCache |
| `src/mobile/ModeBar.tsx`→`ModeBarCli.tsx`+`ModeBarApp.tsx`（新）；`QuestionCard.tsx`→双套（新）；SessionDetail/api.ts | 选套 + 类型扩展（3.3/3.7） |

**dsh 轨**：

| 文件 | 改动 |
|---|---|
| `src-tauri/src/inject/headless/dsh_acp.rs`（新） | dsh-acp stdio 客户端（spawn 机制首任务定案；session/list→resume/new→prompt、cancel、request_permission 透传） |
| `src-tauri/src/inject/routing.rs` | `HeadlessKind::DshAcp` + 分支替换 `dsh_headless_pending`（注释已预留）；H3 自然接管 |
| `src-tauri/src/remote/api.rs` ⑤b + send-info | dsh 分支 + 能力位 |

**P1 验收门**：M21（codex 档位协议切换 + 回执核对）/ M22（interrupt）/ M23（问答补投）/ M24（H3 回归）/ M25（dsh 全链 + cancel 首验）/ M27；回归：CLI codex 模式/问答全量（固化件零行为变化）、M12（dsh 读侧）。

### P2 · 协议升级（steer 已活体，无探测前置）+ WB 可选

| 项 | 内容 |
|---|---|
| codex steer 插队 | `turn/steer`（expectedTurnId；busy 插话不打断已活体）——实现在飞检测 + 入口 |
| codex 审批协议应答 | ServerRequest 三族（commandExecution/fileChange/permissions requestApproval）持连应答（裁决 32 唯一 carve-out）；HeadlessApprovalCard channel 门扩表 |
| dsh request_permission 代答 | 逐次允许/拒绝按钮（首批只展示） |
| WB 可选 | session/cancel 接线；模型选择展示（availableModels 面已实测可读） |

---

## 8. 裁决点清单（28–34 · 已全部裁定）

| # | 议题 | 裁决结果（2026-10-11 用户） | 备注 |
|---|---|---|---|
| 28 | codex APP 问答作答：等探测 vs 先降级 | **b 先降级**（补投 + 引导回 APP） | 探测补强：原生文本作答面确认无（ServerRequest 只有审批三族+elicitation）——b 即终态方案，审批类 P2 升级 |
| 29 | zcode 探针修法 | **c = A+B 叠加** | 实测 15–19s 已入 §4 |
| 30 | dsh H13 时机 | **改判：并入本批**（原推荐 b 独立批，用户裁决提前） | → §6/§7 P1 并行轨 |
| 31 | APP 版不支持动作 UI | **b 灰置 + 原因** | 工具级隐藏、形态级灰置分层 |
| 32 | app-server 连接形态 | **a per-action 短命** | P2 审批应答为唯一持连 carve-out；探测补强：写活跃线程须 daemon 通道（首任务） |
| 33 | zcode 探针两级化 | **a 采纳** | L0 串联闸门不替代 L1 |
| 34 | 未启用工具错误引导 | **a 设置页文案级** | — |

---

## 9. 测试计划纲要

### 9.1 单元/集成（纯核，零网络零真实用户目录）

| 域 | 用例要点 | 锚点 |
|---|---|---|
| 探针语义（P0） | 超时+stdout 有 JSON→Pass（B 主断言）；超时+零输出→Fail 新文案；duration=真实墙钟；L0 失败不触 L1；缓存 TTL 不回归 | `gate.rs` tests |
| APP 止血门/分派臂（P0/P1） | `(codex,App)` 409→P1 接臂；`form=Cli` 直通；APP 臂 H3 关→403；补投三型模板+`screen=null`；审计词钉值（`codex_appserver`/`headless_codex_queue`） | `remote/server.rs` tests |
| 协议客户端缝（P1） | initialize 失败/方法缺席/读回三态/interrupt 回执/**裸方言解析（无 jsonrpc 字段）**/daemon 通道握手失败兜底 | `codex_app.rs` tests |
| WB 指纹（P0） | connect 活体判据（新第二腿）；`{"status":"UP"}` 兼容分支；AUTH_REQUIRED health 不再误杀 | `wb_acp.rs` tests |
| WB 投递（P1） | new+prompt 单臂；靶向不一致标注文案；writer_occupied 如实回执 | `wb_acp.rs`/`server.rs` tests |
| routing（P1 dsh 轨） | `DshAcp` 钉值（`headless_dsh_acp`）；`dsh_headless_pending` 分支删除翻转断言；H3 绑定集含 dsh | `routing.rs` tests |
| dsh 客户端缝（P1） | list/resume/new 三态分派；活跃会话如实拒；request_permission 透传不代答；cancel 回执 | `dsh_acp.rs` tests |
| 前端双套（P1） | ModeBarApp 三态+降级+灰置；QuestionCardApp 补投+`screen=null` 不翻转；固化件既有用例原样平移；surface 选套；APP 词汇表（禁「终端/屏读」字样断言） | `tests/mobile/*` |

### 9.2 真机验收（M 系，P0: M18–M20/M26；P1: M21–M25/M27/M28；P2 各项自带）

| # | 用例 | 批 | 判据 |
|---|---|---|---|
| M18 | WB 勾选恢复上板（双向） | P0 | ≤3s 出卡/无卡 |
| M19 | zcode 发送恢复 | P0 | 探针过 → 回执含 assistant 摘要+tokens；审计行真实耗时 |
| M20 | codex APP 止血门 | P0 | 切档/作答 → 明确「开发中」拒绝（不再现控制台错误） |
| M26 | WB 端点发现修复 | P0 | **复现基线（修前）**：00:13 拒发（开关开、4 候选 0 命中，审计 `16:13:27Z`）。**修后判据**：同操作 → 端点发现命中（connect 活体判据）→ 投递臂接管；端点真未开（WB 内开关关）→ 才报「请开启远程控制」 |
| M21 | codex APP 档位协议切换 | P1 | APP 内 approvalPolicy 生效（**行为级验证——档位改变后工具调用审批行为随之变化**）+ 回执核对 |
| M22 | codex APP 中止 | P1 | interrupt 生效（参照活体 1.5s 量级）；queue 滞留项如实 |
| M23 | codex APP 问答补投 | P1 | 补投入队 → APP 消费 → 引导文案 + screen=null |
| M24 | H3 门扩展回归 | P1 | 关：APP 四动作 403、dsh 置灰；CLI codex 不受影响 |
| M25 | dsh ACP 全链 | P1 | 非活跃会话 resume+prompt 精确命中；活跃会话如实拒；cancel 首验；request_permission 展示 |
| M27 | WB 当前会话投递 | P1 | 消息落 APP 当前会话（APP 内可见）+ 靶向标注 |
| M28 | WB 他选/已结束会话 | P1 | 如实回执不谎报 |

---

## 10. 风险与已知边界（v2 刷新）

| # | 风险/边界 | 处置 |
|---|---|---|
| 1 | codex queue 滞留（消费前提 = thread 被 APP 打开；Mac 实测 19.2min+） | H8 前置校验已交付；补投扩大 queue 面 → 滞留语义如实透出 + 引导「回 APP 打开会话」 |
| 2 | app-server 版本漂移（0.162.1 现态；协议无版本承诺；v1/v2 双协议目录并存） | 双坐标探测 + 方法在场性探测进 ProbeCache；schema 归档仅素材；v1/v2 选用 P1 定 |
| 3 | **codex daemon 控制插座通道未打通（P1 首任务）**：活跃线程写动作（interrupt/档位）唯一路径；失败兜底 = 关闭态 resume + 打开态置灰 | 如实不假装可写；M21/M22 验收显式覆盖两态 |
| 4 | **WB 靶向错位**：new 绑 APP 当前会话（实测），无法指定会话 | 回执明示目标 sessionId；不一致即明示；M27/M28 |
| 5 | **WB 误投用户会话先例**（探测申报） | 已转设计约束（4）；投递审计带 sessionId 可追溯 |
| 6 | dsh spawn 机制未定案（acp profile 未落盘本机） | P1 dsh 轨首任务实机定案；README 协议面为文档实证，行为 M25 首验 |
| 7 | 探针成本（真回合 ~20k tokens；zcode 实测 inputTokens 19772/次） | 两级化 + B/A 主修；TTL 维持 5min |
| 8 | 屏读纪律 APP 替代不完整（协议读回失败 → 「未知请核对」） | 保守降级沿用；补投永不声称生效 |
| 9 | H3 管辖扩展（APP 四动作 + dsh）行为变化 | M24 显式回归；CLI 侧断言入 server 用例 |
| 10 | 三家写者语义一致性（codex writer 锁 / WB writer_occupied / dsh resume 仅非活跃）| 路由设计统一吸收：**活跃会话 = 宿主 APP 持写者，MAM 旁路投递或如实拒**——不与宿主争写者 |
| 11 | steer 跨客户端可达性 | **已活体验证**（独立实例对自己的 busy 回合 steer 通；对 APP 发起回合的 steer = M 系 P2 验收补充项） |
| 12 | 固化改名前端破坏面 | 固化件既有测试原样平移 = 零行为变化机器证明 |

---

## 11. 二轮探测回填（2026-10-12 Task 0 实测，gate 全闭环）

> 证据台账 = `research/refs/phase2-消息注入/2026-10-12-probe-task0-codex.md` 与 `…-dsh.md`（留本地）；本节为正文 §3/§6 的**定案修订层**，冲突处以本节为准。真实回合消耗合计 21 次（申报见计划回填节）。

**T0.1（写路径架构，推翻一轮关键假设）**：daemon 控制插座协议 = HTTP Upgrade → RFC 6455 WebSocket（客户端帧 mask）→ text 帧 JSONL JSON-RPC（initialize 应答 1ms；**std 无 Windows AF_UNIX，生产须 socket2**）；`codex app-server proxy` 判死（纯字节转发、无 WS 握手）。**决定性负结论：daemon 路对 APP 持有线程同样 `-32600 already has an active writer`（锁 = codex-home 跨进程级、无 takeover）——「写 APP 活跃会话」在 0.162.1 无任何通道**。降级主腿两路全通：非活跃线程裸 spawn resume 0.38s + turn；daemon WS resume 306ms + turn + **interrupt 31ms → interrupted**。锁语义：硬杀写者后 ≤65s 过期；旧线程 pin 模型失效需 model 覆写。
→ §3.4/§3.5 修订：**写路径主路 = 裸 spawn 短命实例（零 WS 依赖）**；daemon WS 通道收窄登记 P2（本批-7）；proxy 移出候选。档位写臂**仅非活跃（APP 关闭）线程可用**，打开态如实拒「会话正被 APP 使用，请关闭 APP 后重试或在 APP 内操作」。**中止范围收窄**：interrupt 仅对 **MAM 自持连接的在飞协议回合**（daemon WS 31ms 活体）；APP 发起回合与 queue 滞留项一律如实置灰。

**T0.2（档位载体定案）**：主载体 = **`turn/start.approvalPolicy`**（行为级翻转实证：never → 无审批面直接执行/sandbox 拒；on-request → +13.5s 出 `item/commandExecution/requestApproval`，帧形状归档）；兜底 = `thread/resume.approvalPolicy`（rollout `turn_context.approval_policy` 逐回合精确跟随，持久层）；`config/value/write` **末位弃用**（untrusted 档被 0.162.1 拒写 "no longer supported" + 全局副作用）。**M21 判据语义修正：on-request 审批面是 agent 裁量，行为级判据必须 untrusted/never 对照；触发输入须「OS 可写 + sandbox 写集外」（OS ACL 拒绝不触发审批面）。**

**T0.5（读回定案）**：`thread/read`/`thread/items/list`/`thread/turns/list` 数据在 `result.data`、游标为不透明 JSON 串、items 带 `{turnId,item,…}` 包装；**RPC 读回族全族不暴露 approvalPolicy** → §3.6 GET 臂 `current` 定案 = **rollout 尾部 `turn_context.approval_policy`**（monitor 已解析 rollout，追加字段；null → 默认档展示「模式未知，请人工核对 APP」）；全局默认可用 `config/read`。

**T0.3/T0.4（dsh 定案）**：spawn = `<dsh runtime node.exe> <asar解包>\dsh\node_modules\@deepseek-ai\dsh\lib\bin.js --profile acp`（`DSH_HOME` 透传 + 关遥测；acp profile 为 shipped 模板自动落盘；陷阱：`--patch` 对 acp config 整对象替换）。协议链活体：initialize（动态 image 位）/ session/list（keyset-cursor 分页 + cwd 筛选 76→11）/ resume（跨进程单写者锁 "already owned by an active write handle"）/ new（必填 `mcpServers:[]`）/ set_config_option（值为 JSON 字符串）/ close（释放锁）。**余额阻断三项**（`Insufficient Balance`，token 消耗 0）：update 成功流、在飞 cancel、request_permission 触发——按源码形状登记（update 六变体 / prompt 完成 `{stopReason}` / cancel notification / permission allow-once·reject-once），实现先行、M25 充值后真机兜底。

---

## 附：与宪法及上位文档对照

- **宪法零改动诉求**：不要求修改 `docs/MASTER-PLAN.md`（dsh 写通道 D14 已由裁决 16 出评；APP 协议动作属二期注入域内通道扩展）。
- **上位 spec（2026-09-27）**：本设计 = 其 H8 边界「协议面后续项」立项 + H13 立项（并入本批）+ H6 版本门控修订 + H9 Windows 修正；裁决 1–27 维持，28–34 为增量（已裁）。冲突时以宪法为准并停下报用户裁决。
- **证据纪律**：本文全部协议断言可溯源至探测报告（活体/文档/未验三级标注）；未验项（daemon 成帧 / dsh spawn / dsh cancel / WB cancel）均在对应批次首任务或 M 系验收中闭环，**不预写为事实**。
