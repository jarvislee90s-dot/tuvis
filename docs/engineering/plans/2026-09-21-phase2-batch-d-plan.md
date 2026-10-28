# 批次丁 · 全终端问答/审批/模式闭环 · 实施计划（v3 细化版）

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development。**待用户批准开工。** 前置：批次乙+丙+R1 已合并验收。
> **Goal:** 四家终端「终端在等用户 → 红灯 → 卡片 → 安全作答/切换 → 回显」全链闭环。
> **依据档案：** 三轮实测取证（2026-09-21：11 截图+审计表+rollout+状态链代码勘察）+ 三家官方文档调研（opencode.ai/docs、developers.openai.com/codex 对齐 rust-v0.155.1、moonshotai.github.io/kimi-code）+ 批次丙 R1 键序档机制。
> **红线：** 不 push、不动 main、不动宪法；六门禁全绿（cargo 用 CARGO_TARGET_DIR=target/gate-run、bin 须 --features hook-listener；vitest 终跑勿与 cargo 并行）；实机测试 #[ignore]；台账如实；每任务独立 commit（`feat(mfix-d): T<N> …`）。

## §0 用户裁决清单（2026-09-21 用户逐条裁定，刚性需求——实现者不得重新设计，只能照做）

| # | 裁决 | 精确含义 |
|---|---|---|
| 裁1 | **计划双卡政策** | 计划正文卡与计划文件卡**并存，各家人能上就都上**，不为强求一致而砍掉某家已有能力。kimi 计划正文=读 plan 文件渲染（用户已确认通过文件看 plan 的方式可接受，但正文渲染容易做就做）；codex 计划不落文件→只有正文卡；claude 两者都有。**任何工具的计划都不允许以 JSON/未渲染形态留在消息流里** |
| 裁2 | **签名后置** | 普通消息的 `[mobile 设备名]` 签名**从消息最前面移到最后面**（用户动机：正文在前可读、溯源不变）。`/` 开头的命令消息**裸注入不带货**（前缀和后缀都会破坏命令/参数），其溯源走 MAM 审计页（`action=slash`+设备名）——终端不留痕是可接受的，审计页必须留 |
| 裁3 | **自由文本不得误触选项（安全裁决）** | 对话框待决时，用户在任何输入框打字发送，**绝不允许**被终端对话框理解成选项选择（本轮实测：composer 发消息=回车注入=切换高亮项，勾选被误开/误关）。两个入口（卡内输入框、composer）都必须收敛到「自由作答序列」，raw 直发在此状态下被禁止 |
| 裁4 | **多选两段+提交闭环** | 多选题点选项=仅勾选；必须显式提交；提交后工具的后续阶段（claude Review 屏等）由 MAM 自动走完，**不得停在半路**只显示「已发送按键」 |
| 裁5 | **模式切换后必须知道切到了哪** | 「切换后不知道是什么模式=白做」。二维工具（codex/kimi）出模式组+权限组两组按钮；单轴工具（claude/opencode）出切换钮+当前模式回显。回读失败显示「请人工核对」，不假装成功 |
| 裁6 | **opencode `Build` 改显「默认」** | 术语对齐 |
| 裁7 | **codex 旧枚举标 legacy** | untrusted / on-failure 官方已退役/deprecated，UI 与代码不得作为可选档 |
| 裁8 | **N 选项卡选项文本来自屏读** | 解析失败降级二元卡+警示，不猜选项不盲出键（批次丙红线延续） |
| 裁9 | **对话框在场=注入红线（全工具）** | 任何控制类注入前屏读检测对话框，在场即拒绝并提示 |
| 裁10 | **kimi server API 延后** | 用户未开 server——本批 kimi 全走注入；server API 端点（approvals/questions REST）记入 §5 备查，开产后可升级 |

## §1 问题清单与根因（三轮实测取证）

| # | 现象 | 根因 | 任务 |
|---|---|---|---|
| 1 | codex 问答卡不出（Question 1/3 待决只有 JSON 折叠行，黄点） | codex 解析器无"用户输入类工具→等待"识别：function_call 一律=Processing；问答卡挂载锚红灯 → 永不挂载（问答端点本身不看状态，断点只在状态链+挂载条件） | 丁T1 |
| 2 | opencode question 卡不出（黄点）、发数字不达意 | 同构：tool 部件一律=Running→Processing；opencode 无钩子通道，标记叠加不生效 | 丁T1 |
| 3 | kimi Ready-to-build 无卡 → 打字被当成选项（误批准实锤） | 状态红灯 ✓ 但审批卡要求映射表，kimi 不在默认映射表；wire `interaction`(plan_review) 结构化数据现成未用 | 丁T2 |
| 4 | codex「Implement this plan?」无选项卡 | 三无对话框（不落盘/无钩子/无状态变化）；修法=「计划提案→预期对话框」+屏读解析 | 丁T2 |
| 5 | 模式按钮变"选第一项"；待批准时切换变直发/打断回合 | 模式切换注入零守卫（审计 17:36-38 连点 17 次全落进待决对话框） | 丁T3 |
| 6 | claude 多选题：composer 打自由文本 → 选项 1 被勾选/反勾 | 消息注入以回车结尾，多选对话框 **Enter=切换高亮行**（K9）；composer 不知道对话框在场 | 丁T3 |
| 7 | claude 多选提交停在 Review 屏 | 提交序列未处理 Review 确认屏（K10 第三段） | 丁T5 |
| 8 | 手打 `/permissons` 被前缀毁掉 | compose_injection 无条件前缀（rollout 10:04 实锤） | 丁T3 |
| 9 | 切换模式后不知道当前模式 | 三家回读词表未校准 | 丁T4 |
| 10 | codex 多问题问答（Question 1/3 + ←→）未支持 | 键序档未含多题导航 | 丁T2 |
| 11 | codex 计划卡 markdown 不完整（计划 label 已出 ✓） | 升格已通、渲染层支持不全（17:19 rollout 行 140 夹具） | 丁T5 |
| 12 | kimi 问答 TwoPhaseSelect 数字键可靠性未验 | R1 复审 Important-1 | 丁T2 |

## §2 统一交互契约（四家一致的用户可见行为规范）

### 2.1 三层语义（先分清，防混用——执行者必读）

| 层 | 是什么 | 谁负责 | 说明 |
|---|---|---|---|
| **红灯（看板语义）** | 看板/列表上该会话显示红点=「终端在等用户」 | 丁T1 状态链（解析器） | 修复后四家语义一致：claude（AUQ）、kimi（interaction）、codex（request_user_input 待决）、opencode（question 待决）都红灯 |
| **卡片挂载** | 详情页是否尝试渲染卡片 | 丁T1 放宽 + 丁T2 新卡 | 问答卡：非结束态都挂载（可用性自隐兜底）；审批卡：红灯∨标记∨预期态 |
| **出卡（可用性）** | 卡片真正显示与否 | 数据形态门（端点） | 问答=「尾部有未答 questions[] 形态」；审批=「Waiting∨标记 ∧ 映射/屏读」。**出卡的钥匙是数据形态，不是状态**——状态只是门牌 |

澄清（用户问过的"黄灯红灯哪个对"）：修复前 claude/kimi 红灯✓、codex/opencode 黄灯✗——四家行为不一致才是问题；丁T1 后统一为「等用户即红灯」。正常运行的上下文**不会**误出卡：问答出卡要求「未答 questions 形态在尾部且其后无结果」，运行中的问题早已带结果被排除。

### 2.2 计划双卡矩阵（裁1）

| 工具 | 正文卡（markdown） | 文件卡（路径+预览） | 正文内容来源 |
|---|---|---|---|
| claude | ✓（ExitPlanMode input.plan 消息） | ✓（.claude/plans 已豁免） | 消息 |
| codex | ✓（proposed_plan 消息，丁T5 补渲染） | ✗（计划不落文件） | 消息 |
| kimi | ✓（**读 plan 文件渲染**——路径取工具结果/系统提醒中 `Plan file:` 行；目录已豁免） | ✓（已上线） | 文件 |
| opencode | 不适用（无独立计划机制，计划即普通消息文本，本就渲染） | 不适用 | — |

### 2.3 点选与提交（裁4）

- 单选：点选项=作答（注入该工具的直选序列）。
- 多选：点选项=仅切换勾选（卡片本地勾选态+注入切换键）；**必须点「提交勾选」才发送**。
- **提交=阶段机闭环**：提交后以屏读状态为锚逐段推进（题目 n → Review 屏 → 确认屏 → 终态），每段注入前先屏读确认当前段，全部走完卡片才显示完成；中途屏读失败→如实回执+引导终端。**禁止**一次性盲发序列后显示「已发送按键」了事。
- 多题卡（codex Question n/N、kimi 多题）：v1 **只读**（显示题目列表+「请在终端完成作答」），多题的逐题注入列入下批（需实机定导航序）。

### 2.4 自由文本三入口一出口（裁3）

三个入口，**同一个作答序列出口**（各工具的自由作答注入序列，`#[ignore]` 实机定案）：
1. 问答卡内嵌输入框（主入口；v1 仅单题卡提供）；
2. composer：**问答卡在场时自动转「作为回答发送」**（placeholder 改文案+发送走自由作答序列）；
3. 审批卡/计划待确认卡在场时：composer 直发**被拦截**，提示「终端等待审批，请用卡片按钮」（此类对话框没有自由作答语义，放行=误触选项——kimi 误批准实锤）。
出入规则：问答自由作答序列按工具定案（claude 定位 Type something→文本→Enter；codex tab notes；opencode 选 own answer→文本→Enter；kimi Other/feedback），全部 `#[ignore]` 实机定案后固化。

### 2.5 签名规则（裁2）

- 普通消息：`{正文} [mobile {设备名}]`——签名在**末尾**。
- `/` 开头消息：裸注入（无前缀无签名），审计记 `action=slash`+设备名。
- **stamp 适配（必做，防真 bug）**：确认戳若取注入文本尾部，后置签名会让所有同设备消息共享同一尾部→**跨消息假命中**。stamp 计算必须取**签名之前的正文尾部**（实现时核 confirm.rs 的戳 derives 口径并补测试：两条同设备不同正文，第一条的戳不得命中第二条）。
- 连带改造：队列预览剥签名、「修改」回填剥签名，均从头部改尾部（正则同步）。

### 2.6 模式栏规格（裁5/6/7）

| 工具 | 结构 | 档位（屏显标签） | 切换机制 | 回读源 |
|---|---|---|---|---|
| codex | **两组**（正交） | 模式组：默认/计划；权限组：只读/默认/完全信任 | 模式：`/plan`（运行中不可用→如实回执）；权限：`/permissions` 两段式（命令→菜单→屏读定位→导航确认） | 底栏模式文本（Default 靠缺席推断的口径处理） |
| kimi | **两组**（正交） | 模式组：默认/计划；权限组：总是询问/按需询问/永不询问 | 模式：`/plan on\|off`（带参直达）；权限：`/permission` 两段式（`/yolo`、`/auto` 为预选直达变体） | 底栏 `plan` 前缀等（注意批准后自动切出 plan 的漂移，回读失败不假装） |
| claude | **单轴**（四档环序） | 默认/接受编辑/计划/完全信任（屏幕词表以实测环序 acceptEdits→plan→auto→manual 校准） | 切换钮=shift+tab 一步+回读确认（回读失败→「请人工核对」） | 底栏词表 |
| opencode | **单轴**（两档） | 默认(原 Build)/计划 | 切换钮=shift+tab+回读（唯一机制，官方无斜杠切档） | 状态栏 Build/Plan（已校准 5/5） |

### 2.7 对话框在场=注入红线（裁8/9）

控制类注入（模式切换/斜杠命令）前，屏读可见窗口检测「编号选项对话框」——在场即拒绝+「终端有待决对话框，请先处理」回执。检测原语工具无关单点实现（复用批次丙 T5 解析器）。

### 2.8 兜底

解析失败/形态未知→降级+明示（二元卡+degradedHint / 只读卡 /「请在终端操作」），会话永不被前端卡死。

### 2.9 终端交互的动态轮询（用户 2026-09-22 裁定，刚性）

任何通过输入改变终端状态的操作——斜杠命令（`/plan`、`/permissions`、`/permission` 等）、
shift+tab 切档、上下键在菜单/对话框中选择、以及任何后续步骤依赖终端新状态的操作——
**一律不得「输入完直接操作下一步」**。必须：

1. **动态轮询**：循环屏读直至**读到判据本身**（不是「屏幕变了」），命中即刻停止轮询
   （不浪费固定延迟）。**禁止**用固定睡眠替代轮询；
2. **有界**：设轮询上限（步长 + 总窗），超时**如实回执**，且回执要能区分
   「未及确认」（屏上仍是旧状态）与「不符」（屏上明确是别的状态）——不得退化成统一的「失败」；
3. **例外（决策前的瞬时快照）**：注入**前**判断「此刻屏上有没有对话框 / 高亮在哪一项」
   这类**决策依据**单次读即返回，**不轮询**——守卫语义是「此刻的快照」，
   给守卫加轮询会把「拒绝注入」变成「等 N 秒再拒绝」，语义相反。

**依据（用户实机观察，2026-09-22）**：① claude 切档后回读**太快**→ 报「回读与预期不符」，
但目视终端**已切到**预期档、刷新浏览器即一致（屏幕重绘未及，非切换失败）；
② codex `/permissions` 菜单**屏读没时间**→ 需等屏幕更新再发后续键。
**实现侧落地**：常量收成单一事实源（`inject::timing`）+ 每条自裁值附 `#[ignore]` 实测项。

## §3 任务分解（5+收口）

### 丁T1 · 红灯与状态链（问题 1/2）
**Files**: `monitor/codex_parser.rs`、`monitor/opencode_parser.rs`、`src/mobile/SessionDetail.tsx`、测试
- [ ] codex_parser：`function_call(name=request_user_input)` 按 **call_id 配对**无对应 output → Waiting；有 output → 不触发。APP/SQLite 路径同核。
- [ ] opencode_parser：尾部 question part 且 `state.metadata` 无 answers → Waiting；有 answers → 不触发。
- [ ] QuestionCard 挂载放宽：非结束态挂载+组件可用性自隐（问答端点不看状态；成本=详情页每轮一 GET）。
- [ ] **回归锁（丁1 引入的新矛盾，必测）**：codex 问题待决=Waiting 后，审批卡**不得**借机误出（审批 detect 门须护住：问题文本不含审批 marker → approve 不可用；补显式回归测试）；同理 opencode。
- [ ] 「运行中不误红灯」回归（已答/历史问题不触发）。
- 验收：codex/opencode 问题待决 → 看板红灯+问答卡；回答后回落；正常运行零误报。

### 丁T2 · 缺失的卡片（问题 3/4/10/12）
**Files**: `remote/api.rs`、`inject/question.rs`、kimi wire 解析、`src/mobile/`、`inject/dialog.rs`
- [ ] kimi 计划审批卡：数据源=wire `interaction.request`（plan_review/ExitPlanMode）→ 卡片（Approve/Reject/Revise+状态回显，**不含 plan 正文**——正文由 2.2 的 kimi 正文卡承担）；注入=导航确认（R1 序列：解析高亮算循环步进，无高亮/多行高亮/越界一律 Err）。
- [ ] kimi 计划正文卡：数据源=工具结果/系统提醒中 `Plan file:` 行 → 读文件渲染 markdown（§2.2）。
- [ ] kimi 审批/问答互斥：审批在场 → 问答卡不可用（沿用批次丙隔离规则，扩到 kimi 新卡）。
- [ ] codex「计划待确认」：预期态=**尾部存在计划提案消息**（proposed_plan，从消息尾部派生，无新存储）→ 详情页提示条+「检查终端对话框」按钮 → **扩展 approve-options 端点门**（Waiting∨标记∨计划预期态）→ 屏读解析 Implement-this-plan 选项 → N 选项卡（codex=数字直选档）+ 计划全文聚合（T8 机制）。预期态清除：下一个用户消息注入或检查未命中时清除。
- [ ] codex 多问题键序（Question n/N：每题 enter 提交、←→ 导航、全部答完后的终态判定）——`#[ignore]` 实机探测定案后实现；定案前多题卡只读（§2.3）。
- [ ] kimi 问答 TwoPhaseSelect 数字可靠性实测（R1 Important-1）→ 分族定案。
- 验收：kimi Ready-to-build 三钮卡 Reject 必达；kimi 计划正文卡出；codex 计划待确认全链（提示条→检查→N 选项→点按生效）；多问题可答。

### 丁T3 · 注入守卫与签名后置（问题 5/6/8）
**Files**: `inject/dialog.rs`、`remote/api.rs`（mode+session-send）、`inject/normalize.rs`、`inject/confirm.rs`（stamp 适配）、`database/dao/write_audit.rs`、`src/mobile/MessageComposer.tsx`
- [ ] 对话框在场检测原语（§2.7）：屏读可见窗口存在编号选项对话框——单点实现，工具无关。
- [ ] 接入①：模式切换（shift+tab 与斜杠两路）注入前检测，在场 → 拒绝+中文回执。
- [ ] 接入②：composer 分流（§2.4）——问答卡在场→转自由作答序列；审批/计划待确认在场→拦截+提示。前端按卡片在场判定即可（后端权威守卫在注入时刻的屏读，接入①已覆盖控制类）。
- [ ] 签名后置（§2.5 全项，**含 stamp 适配与跨消息假命中测试**——两条同设备不同正文互不命中）。
- [ ] 审计 `action=slash`：词表三处同步（inject/mod.rs 注释、write_audit.rs 注释、server audit_action_vocab 测试）。
- [ ] `#[ignore]` 实机：三场景（待决点模式钮→拒；composer 发送→自由作答；移动端 `/permissions`→触发+审计）。
- 验收：图5/6/7 三场景消失；审计页 slash 可查设备。

### 丁T4 · 模式二维与回读全开（问题 9）
**Files**: `inject/mode.rs`、`src/mobile/ModeBar.tsx`、`remote/api.rs`（mode 路径）
- [ ] 按 §2.6 规格表实现：codex/kimi 两组（模式组钮+权限组钮，权限两段式注入）；claude/opencode 单轴+回显。
- [ ] 回读校准：三家词表逐家屏快照回归（kimi 漂移口径：批准后自动切出 plan，回读失败不假装）；claude 环序表按实测 [acceptEdits, plan, auto, manual]。
- [ ] opencode `Build`→「默认」；codex untrusted/on-failure 标 legacy 不可选。
- [ ] 约束落地：codex `/plan` 运行中不可用 → 如实回执。
- [ ] `#[ignore]` 实机：四家各切一轮，回读逐例核对。
- 验收：四家切换后回显当前模式；「模式未知」从常态变例外。

### 丁T5 · 提交闭环与渲染（问题 7/11）
**Files**: `inject/question.rs`、`src/mobile/QuestionCard.tsx`、content.rs 渲染、测试
- [ ] 阶段机提交（§2.3）：claude 多选 提交→屏读检测 Review 屏→'1' 确认→终态；检测不到 Review 如实回执；卡片进行中态替代「已发送按键」。
- [ ] 问答卡内嵌自由文本输入框（§2.4 入口 1，v1 仅单题卡）。
- [ ] codex 计划卡 markdown 补全：17:19 rollout 行 140 实录做夹具（`###` 标题/列表/粗体渲染）；核对同条升格路径为何此前未触发。
- 验收：多选全链一气呵成；卡内自由要求不错勾选项；codex 计划卡渲染完整。

### 丁T6 · 收口
- [ ] 六门禁全绿；验收清单 G 节（批次丁人工项）；台账批次丁节+handover 新档；停下等主线 review。

## §4 总验收（八条）

①codex/opencode 问题待决红灯+问答卡可作答、运行中零误报；②kimi 计划审批三钮卡 Reject 必达+计划正文卡；③codex 计划待确认卡可见可点且聚合计划全文；④对话框待决时模式按钮被拒有提示、composer 转自由作答不错勾选；⑤移动端斜杠可触发+审计留痕+普通消息签名后置溯源不变+确认戳无跨消息假命中；⑥四家切换后回显当前模式（二维两组/单轴一组）；⑦claude 多选勾选→提交→Review 一气呵成；⑧codex 计划卡 markdown 完整+kimi 计划正文卡。六门禁全绿。

## §5 不入本计划

- kimi server API 程序化应答（用户未开 server；官方 REST/WS 端点已调研归档，开产后可升级）；
- opencode 会话内权限切档（官方无此能力，权限属 opencode.json 配置域）；
- codex Permission Profiles（beta）；协议路线（claude SDK/codex app-server 托管，三期）；
- codex 打断式插队（esc interrupt 底栏存在，需探测后另批统一）；
- zcode/dsh/workbuddy/openclaw 交互卡（沿用既有判定）；
- 多题卡逐题注入（v1 只读；导航序实测定案后下批）。
