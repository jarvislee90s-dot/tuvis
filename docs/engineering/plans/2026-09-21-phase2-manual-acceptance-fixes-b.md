# 二期收尾 · 手工验收修复批·批次丙（审批交互统一与 plan 全链）· 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development 或 executing-plans。步骤用 checkbox 跟踪。**批次乙（25f7bae）已交付停下等主线评审；本计划经用户 2026-09-21 口述需求 + 两轮实机测试证据起草，执行前需用户批准开工。**
> **Goal:** 修复两轮手工验收暴露的审批/问答/plan 交互问题：幽灵审批标记、问答卡形态化匹配（codex/opencode 接入）、通用 N 选项审批卡、模式切换、plan 文件识别与渲染、claude 打断式插队，并以前端**统一交互卡 UI**收敛各家审批界面风格差异。
> **执行环境：** Windows 实机（分支 `feat/phase2-injection`）。
> **依据档案（执行前读）：** `docs/research/refs/phase2-消息注入/2026-09-21-claude-askuserquestion-按键语义探测.md`（K1–K11 键位定案）；`docs/research/refs/phase2-消息注入/2026-09-20-问卷交互跨工具矩阵.md`（四家存储×键击×可解析）；`docs/research/refs/phase2-消息注入/2026-09-21-happy项目审批与模式切换调研.md`（UI/状态机标准答案）；本计划 §1 取证根因（全部本机实锤，勿重新调研）。
> **红线：** 不 push、不动 main、六门禁全绿（cargo 用 `CARGO_TARGET_DIR=target/gate-run`，bin 测试须 `--features hook-listener`；vitest 终跑勿与 cargo 并行）、实机测试一律 `#[ignore]`、设计级口径变化只上台账。
> **与 M6R–M9R 加强文档的关系：** 按「实测结论不回写设计文档」惯例，本批范围不追记 `2026-09-18-phase2-m6r-m9r-injection-hardening-design.md`，以本计划为唯一执行 SSOT（M6R–M9R 设计文档保持验收基线原样）。

## §1 问题清单与根因（两轮实测 · 2026-09-21，本机取证实锤）

| # | 问题 | 根因（已取证） | 任务 |
|---|---|---|---|
| 1 | claude 问答 pending 时红卡 允许/拒绝 顶出、问答卡被压死 | **幽灵审批标记**：claude 对 AskUserQuestion 待答也发 `Notification(permission_prompt)`（09:07:16 事件实证）→ 批次甲把它注册为审批信号 → 误写 `approval_wait_marks`（09:07:45「等待审批」实证）→ 审批卡出 + T8 隔离约束（审批标记在场→问答卡 None）反向压死问答卡 | 丙 T1 |
| 2 | claude 问答卡通道 A（hook）全程未触发 | 部署缺陷：`~/.mam/bin/mam-hook-listener.exe` 为 00:45 旧构建（早于 T8 提交 05:59），事件无 `tool_name` 载荷 → 问答分支永不识别 | 丙 T2 |
| 3 | codex `request_user_input` 不出卡（只显示原始 JSON） | T8 通道 B 按工具名 `AskUserQuestion` 精确匹配；codex 工具名不同但 **args 形态相同**（rollout 09:30:45 pending 即落盘，`questions[{header,id,options,question}]` 结构化，答案按 question id 键控） | 丙 T3 |
| 4 | opencode question 不出卡（JSON 不换行裸奔） | 同 #3：工具名 `question`、args 形态相同（图4 实证 tool-call 带完整 questions JSON 进消息流）；卡片化后 JSON 裸奔问题自然消失 | 丙 T3 |
| 5 | codex 计划渲染带 `</proposed_plan>` 标签残留 | 计划是含 `<proposed_plan>…</proposed_plan>` 标签的 assistant **消息**（非工具调用），T1 升格（tool_call input.plan 形态）接不住 | 丙 T4 |
| 6 | **审批对话框是 N 选一，卡片只有 允许/拒绝 二元**——claude 计划批准（1=auto mode/2=manual/3=tell-claude+shift+tab 带反馈批准）、codex Implement this plan（1/2/3）、kimi Ready to build（1=Approve/2=Reject/3=Revise）三类对话框全部降级成二元卡，用户盲发数字碰运气（图2/图3 实证：点允许=注入"1"恰好命中推荐项） | 审批映射表只建模二元 approve/reject；对话框选项文本未解析（Windows CONOUT$ 屏读能力已有，`stuck_on_input_line`/`screen_probe` 同源） | 丙 T5 |
| 7 | **模式切换无 UI**（opencode/kimi 卡在 plan 只读无法退出；claude 计划批准后切档不可见；codex 走 /plan、/permissions） | 无任何模式切换功能。切换机制：claude/opencode/kimi=shift+tab 循环（opencode 状态栏明示 "Plan · DeepSeek…"，模式可屏读确认）；codex=斜杠命令注入 | 丙 T6 |
| 8 | **kimi plan 文件不渲染也不可预览**（图1：Write 写入 `~/.kimi-code/sessions/.../agents/main/plans/*.md`，手机只见工具结果一行） | `.kimi-code` 整目录在 SENSITIVE_DIRS 黑名单；EXEMPT_SUBPATHS 是段精确静态表（`.claude/plans`/`.codex/plans`），接不住 kimi 深路径（含会话 id 变量段）；且无「计划文件引用→渲染卡片」机制 | 丙 T7 |
| 9 | claude 计划批准时 plan 内容与审批卡分离（计划在消息流/plan 文件里，批准卡孤立） | 交互卡主体不聚合 plan 内容（happy 的 ExitPlanToolView = plan markdown 即卡片主体，标准答案） | 丙 T8 |
| 10 | **claude 插队不打断**：busy 时「立即发送」只是进入 TUI 内部队列（仍排队中），终端需按 Esc 中断当前回合新消息才进 | jump 路径只注入正文，不处理「运行中会话需先中断」语义；Esc 中断后队列消息去向未探测（前置探测项） | 丙 T9 |
| 11 | 审批界面各家风格不一（红卡/问答卡/计划卡三套组件三种交互，有的能渲染 plan 有的不能） | 前端三卡各自为政，无统一交互卡模型 | 丙 T10（§2） |

**实现红线（取证实证，违反即返工）**：
1. 问答标记与审批标记的**互斥裁决**必须保住 T8 的隔离测试面（问答在场→审批卡不可用；修复方向=误标不写 + 问答写入时清审批标记，双保险）；
2. helper 重构建必须带 `--features hook-listener`（收口实测教训：单建此 bin 默认 feature 关）；部署后跑一次 `#[ignore]` 实机自检验证 PreToolUse(AUQ) 载荷真实触发（批次乙未做过这一段实机验证）；
3. N 选项卡**解析失败必须降级**为现二元卡+防重警示（不猜选项、不盲出键——happy「兜底渲染」原则：会话永不被前端卡死）；
4. 模式切换屏读失败（WT 宿主/macOS）降级为盲切+「请人工核对终端模式」提示，不假装成功。

## §2 统一交互卡 UI 设计基准（前端一套设计语言，§3 T10 据此改造）

### 2.1 现状功能点盘点（三套组件各自为政）

| 现有组件 | 功能点 | 风格现状 |
|---|---|---|
| ApproveCard（红卡） | 等待批准徽标 + 允许/拒绝二元 + 严格档 hintOnly + drift 提示 + 409/404 分诊 | 红系；无选项展开能力 |
| QuestionCard（问答卡） | 题干(header+question) + 编号选项(label+description) + 单选直答/多选勾选+提交/取消 + 自由文本引导 + 多问题只读 + 自隐 | 浅蓝系；与红卡结构迥异 |
| plan 卡（消息流内） | 「计划」标签 + renderMarkdown | 绿标签；不参与审批 |
| Composer 排队/回执 | 完整队列列表 + 逐条三钮 + 单回执槽（submitted/delivered/failed/gone） | 独立域 |
| 后端数据源 | approve options（映射表二元）/ question（questions[] 结构化）/ 屏读（对话框文本，未结构化）/ plan（消息流 or 文件） | 分散，无聚合 |

### 2.2 统一模型：单一「交互卡」（InteractiveCard）容器

所有「等待用户输入」场景渲染同一容器，差异只在**主体**与**动作区**数据：

```
┌─ 状态色条 + 类型徽标 + 工具名/来源 ──────────────┐
│  主体 BODY：                                      │
│   · approve → 提示文本(+ plan markdown 聚合, T8)  │
│   · question → 题干 + 选项结构化表单              │
│   · dialog → 屏读解析出的 N 个选项原文            │
│  动作区 ACTIONS：编号按钮网格（1..N + 取消）       │
│   主行动高亮 / 中性次行动 / 危险红拒绝            │
│  底注 FOOTER：键位提示 · 未确认落盘提示 · 分诊文案 │
└──────────────────────────────────────────────────┘
```

### 2.3 按钮模型（统一 Action 结构）

`{ label, tone: primary|neutral|dangerous, action: 注入序列|语义应答 }`：
- 问答卡选项 = 编号按钮（单选直答/多选勾选态+提交——键序按各工具矩阵）；
- N 选项审批 = 屏读解析的编号按钮（超 9 个或解析失败→降级二元）；
- 自由文本 = 统一引导「用下方输入框直接回复」（不做注入表单，happy 同款取舍）；
- 取消/拒绝 = esc 或映射拒绝键（危险红）。

### 2.4 数据供给与兜底

- 后端聚合端点 `/session-interact`（聚合 approve/question/dialog/plan/mode 五类信号，前端单 hook 消费）；迁移期可先前端聚合既有分端点，聚合端点为收口目标；
- **兜底渲染**（happy 原则）：未知形态回退 JSON 展示+可取消；解析失败的对话框回退二元卡；多问题只读卡维持现状——任何形态都不允许把会话卡死；
- 视觉 token：等待批准=红、问答=蓝、计划确认=绿（沿用终端内色彩语义：kimi plan 绿框/claude 问题蓝选中）；徽标+色条统一，正文排版共享。

### 2.5 不做

不做各工具像素级复刻 TUI；不做自定义主题；不把消息流卡片改造成交互卡（交互只发生在底部交互卡，消息流只读）。

## §3 任务分解

### T1 · 幽灵审批标记修复（图2/图3 根因，最高优先）
- [ ] helper 透传 `Notification.message` 字段（事件文件可选字段，向后兼容同 T8 先例）；adapter 侧：message 含 AskUserQuestion/问题语义 → 不写审批标记（判据以实测 message 原文为准，先实机取证一次 claude 提问时 Notification 的 message 原文，`#[ignore]`）；
- [ ] 双保险：问答标记写入时同步清除该会话审批标记（互斥裁决，防其他未知误标路径）；
- [ ] 测试：误标不写/问答清审批/正常审批不受影响三分；隔离测试面回归。
- 验收：claude 问答 pending → 问答卡出现、无红卡；真实审批 → 红卡照常。

### T2 · helper 构建部署保障
- [ ] 构建文档/发版脚本固化 `--features hook-listener`；`#[ignore]` 实机自检：PreToolUse(AUQ) 事件带 tool_name+tool_input 落盘（通道 A 端到端首次实机验证）；
- 验收：`~/.mam/bin` helper 含新载荷；claude 问答触发通道 A 建卡。

### T3 · 问答卡形态化匹配（codex/opencode 接入）
- [ ] 通道 B 判据从「工具名==AskUserQuestion」扩为「**args 形态**：顶层 questions[] 且元素含 question+options[]」（工具名只作日志线索）——codex `request_user_input`、opencode `question`、kimi（已通）一并接住；
- [ ] codex 注入序列实现（数字直选即交/↑↓+Enter/Tab 备注/Esc——按矩阵源码级键位）+ `#[ignore]` 实机复验一次（矩阵遗留条件项）；
- [ ] opencode/kimi 注入按各家矩阵键序实现或先只读（按实机复验结果定，未验不出键）；
- 验收：codex/opencode 触发 question → 卡片渲染题干+选项（不再 JSON 裸奔）；作答链路通。

### T4 · codex `<proposed_plan>` 升格 plan 一等卡
- [ ] codex 解析器：assistant 消息被 `<proposed_plan>…</proposed_plan>` 包裹 → 剥标签升格 `kind="plan"`（与 T1 形态升格同构）；
- 验收：codex 计划以常驻计划卡渲染、无标签残留。

### T5 · 通用 N 选项审批卡（屏读解析）
- [ ] Windows 屏读解析审批对话框选项列表（`1. xxx / 2. xxx / …` 行模式；claude 计划批准/codex Implement this plan/kimi Ready to build 三类先例）→ 下发 N 选项 → 统一交互卡渲染编号按钮；
- [ ] 选项键序=数字字符（claude/kimi/opencode A 族实测）或 VK（codex B 族）；解析失败/超 9 选项 → 降级现二元卡+防重警示（红线 3）；
- [ ] `#[ignore]` 实机：三类对话框各验一次选项解析与按钮生效；
- 验收：图2/图3 场景——手机显示 1/2/3 选项按钮且标注语义（如「1. Yes, and use auto mode」），点按生效。
### T6 · 模式切换
- [ ] 统一模式枚举（计划/默认/接受编辑/完全信任/只读——对齐 happy 的 8 值收敛为 MAM 5 值）+ 各家映射表（happy `executionPolicy` 语义搬用）；
- [ ] 注入实现：codex=`/plan`、`/permissions` 斜杠注入；claude/opencode/kimi=shift+tab 注入（VK 组合，引擎键域扩展）+ 状态栏屏读验证（Windows）；屏读失败降级盲切+人工核对提示（红线 4）；
- [ ] 交互卡头部显示当前模式；
- [ ] `#[ignore]` 实机：四家各切一次 plan↔执行，屏读回读模式正确；
- 验收：图1 场景——opencode 远程一键退出 plan mode。

### T7 · plan 文件识别、豁免与渲染（图1 kimi 诉求）
- [ ] EXEMPT_SUBPATHS 扩展支持**尾段序列匹配**：`.kimi-code/…/agents/main/plans` 子树放行预览（段精确不误伤；凭据面照拦）；
- [ ] 计划文件引用识别：工具结果/消息中的 plan 文件路径（`Wrote N bytes to …plans/x.md`、`Plan file: …`、`Planning: …`）→ 消息流渲染「计划文件卡」（文件名+可点预览）；
- [ ] claude 侧核对 `.claude/plans` 豁免既有面回归；
- 验收：kimi 写完 plan → 手机出计划文件卡 → 点开可读全文（图1 场景闭环）。

### T8 · 审批点 plan 聚合
- [ ] 计划确认类审批卡主体聚合 plan 内容：claude=消息流 ExitPlanMode input.plan 或 `.claude/plans` 文件读取；codex=T4 升格消息；kimi=T7 plan 文件读取；
- 验收：三类计划批准对话框的卡片即见计划全文（不再要用户去消息流翻）。

### T9 · claude 打断式插队
- [ ] 前置探测（`#[ignore]` 实机）：claude busy 时注入正文→Esc 的语义链（Esc 中断回合后 TUI 队列消息去向、Esc 是否清输入行、K2 教训「数字后补 Esc=中断」在此处反而是**想要**的行为）；
- [ ] jump 路径按探测结论实现「Esc 中断 + 消息投递」变体（仅 claude；其他工具维持现语义），回执与审计如实（新 action 或 jump 变体标记）；
- 验收：claude 运行中点「立即发送」→ 当前回合中断、消息即刻进入。

### T10 · 统一交互卡 UI 改造
- [ ] 按 §2 基准重构 ApproveCard/QuestionCard → InteractiveCard 组件族（视觉 token/按钮模型/兜底渲染）；plan 卡与审批聚合卡同语言；
- [ ] 既有测试面（ApproveCard/QuestionCard/SessionDetail）适配，行为锁不松；
- 验收：四家工具触发任意「等待用户」场景 → 手机呈现同一套设计语言（差异只在主体数据与按钮集）。

### T11 · 收口
- [ ] 六门禁全绿；验收清单增补（C-23 N 选项审批 / C-24 模式切换 / C-25 打断式插队 / plan 文件预览扩展项）；台账记批；停下等评审。

## §4 总验收（十条）

①claude 问答卡在真实会话稳定出现（无幽灵红卡）；②codex/opencode 问答卡出卡（JSON 裸奔消失）；③三类 N 选项审批对话框（claude 计划批准/codex implement/kimi ready-to-build）手机端可见选项且点按生效；④codex 计划无标签残留、以一等卡渲染；⑤kimi plan 文件出「计划文件卡」且可预览；⑥三类计划批准卡聚合 plan 全文；⑦opencode 远程可退出 plan mode，四家模式切换可用且回显当前模式；⑧claude 打断式插队生效；⑨四家审批/问答/计划界面呈现统一设计语言；⑩helper 通道 A 实机自检通过。六门禁全绿。

## §5 不入本计划

- 协议路线（happy 式 SDK 托管/codex app-server）：三期，调研已归档 `docs/research/refs/phase2-消息注入/2026-09-21-happy项目审批与模式切换调研.md`；
- macOS 屏读缺失下的模式切换精确回显（降级盲切已覆盖）；
- zcode/dsh/workbuddy/openclaw 全部交互卡（沿用既有判定：三期/观察级/无机制）；
- 消息合并/重复折叠（用户裁决不修，2026-09-20）；
- 全面状态语义重构（另行批次）。
