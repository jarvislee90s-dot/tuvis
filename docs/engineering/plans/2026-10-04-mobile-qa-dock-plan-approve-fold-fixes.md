# 手工验收三问题修复批：问答卡下方停靠 · claude 计划批准卡 · 过程折叠修复

> 日期：2026-10-04 · 分支：`feat/phase2-injection`（PR #98 合并前一并评审，commit 本地不 push，等用户验收）
> 来源：用户手工验收反馈三条（原话见会话记录），计划经用户批准后执行。

## §1 根因（均已代码实证）

### ① 问答/审批弹窗在页面顶部
ApproveCard / QuestionCard / ModeBar 以内联块挂在消息滚动区**上方**（`SessionDetail.tsx` 分屏分支 :1250-1270、正文分支 :1339-1359，两分支同构），一出现就把整页消息流顶下去。移动端已有统一蓝系 token（`InteractiveCard.tsx` TONES.question，审批卡与问答卡 2026-09-24 起并成一套），问题只在挂载位置。

### ② claude 计划批准无法从卡片操作（root cause 链）
1. claude 计划批准 = ExitPlanMode 尾挂 tool_use（无 tool_result）→ `determine_status`（`monitor/status.rs:129`）判 has_tool_use 但非 user_input_tool（`is_waiting_for_user_input` 工具表只有 AskUserQuestion，`status.rs:20`）→ 状态 = **Processing（黄）**，与用户截图黄点吻合；
2. 审批卡挂载门 `approveMounted = status==="waiting" || planPending`（`SessionDetail.tsx:385`），而 `isPlanPending` 对 claude **显式 return false**（`:239`，丁T2 裁决「claude 走既有 waiting 门」——该裁决的前提「claude 计划批准会落 Waiting」实际不成立）→ 两门全关 → **审批卡永不挂载**；
3. 后端其实读得到对话框：composer 的 presence 探测独立打 GET（`MessageComposer.tsx:287`），屏读解析带计划标题锚（`dialog.rs:251-279`）能解析出 1/2/3——只是卡片没有消费方；
4. 键序档案（`dialog.rs:362-364` R1 实证）：**claude 计划批准框数字键无效（三样本零效果），`↓×k+Enter` 才有效**——与用户要求「方向键切过去再选」吻合，`navigation_sequence`（从唯一高亮位算循环步进，`dialog.rs:383`）机制现成；
5. 选项 3（Tell Claude what to change）无任何反馈打字通道——需新端点 + 实机探测定案编辑态屏面。

### ③ 过程折叠按钮失效
`collapseAll`（`SessionDetail.tsx:941-944`）只清空 `expandedOverride` 回落默认，而运行态 wire 默认 tool-result 展开（`remote/content.rs:156/:250`）→ 未手动折叠过任何条时点击 = no-op；且运行态「全折叠」态数学上不可达，按钮单向卡死。该 bug 仅运行态表现（总结态默认全折叠故正常），与用户场景一致。

## §2 实施

### T1（问题③）过程折叠修复 — src/mobile/SessionDetail.tsx
- 新 state `processAllCollapsed`（默认 false）；`isCollapsed` 判序：单条 override → `processAllCollapsed && isProcessKind(m.kind)` → 原链（plan/plan-file 恒展开豁免 → 总结规则 → wire collapsed）不变；
- `collapseAll` = 置 true；`expandAll` = 置 false + 既有强制展开 false-for-all 保留；BookmarkBar 的 `allCollapsed` 改由该位驱动；单条 toggle 优先级最高不动；总结模式横幅（复用同两函数）自动获益不回归；
- 效果：sticky——点「过程折叠」后**新到达**的工具结果也默认收起（不随 10s 轮询翻回）；
- vitest：运行态点折叠 → tool-result 收起 + 按钮翻转；新消息到达仍收起；展开恢复默认；总结态不回归。

### T2（问题①）下方停靠 + 蓝系统一 — src/mobile/
- `SessionDetail.tsx` 两分支把 ApproveCard / QuestionCard / ModeBar 三块从 bookmarkBar 之后移到 **MessageScrollArea 之后、MessageComposer 之前**，外包 dock 容器：`shrink-0 max-h-[55vh] overflow-y-auto`（长卡内滚，消息区始终可见，composer 提示「请用上方卡片按钮」语义不变——卡就在输入框正上方）；dock 以「有无卡」为 key 重挂，入场 slide-up 动画（mobile.css 加 keyframes，200ms；实现繁琐则降级无动画）；
- 蓝系统一微调：审批卡/问答卡选项行统一行形态（圆角/内距/选中态 token 同源）；问答卡选项**不**加数字徽标（编号=将注入的键位，只在 dialog 分支真实——`ApproveCard.tsx:300-304` 既有「不撒谎」纪律）；消息流内 plan 灰卡/plan-file 绿卡是**内容卡**不是弹窗，不动；
- vitest 全量回归。

### T3（问题②-探测先行）实机探测 — win-console-inject-probe 技能
自建 claude 会话走 plan 流程取证：①屏读确认 `1./2./3.` 选项簇与标题锚同屏；②方向键 ↓×k+Enter 选 1/2/3 各自生效（选 1/2 退出计划、选 3 进反馈编辑）；③**选 3 后的编辑态屏面签名**（输入行位置/占位文案）；④打字+Enter 提交 → 留在计划模式开新一轮；⑤退格清空（right×L+backspace×L）在该行有效性。产出追加归档 `docs/research/refs/phase2-消息注入/`（带 run-id）；**锚文案/判据全取探测证据，禁止手造**；结束 taskkill 清场。

### T4（问题②-后端）状态层 + 键序 + 反馈端点
- **状态层**（`monitor/status.rs`）：`is_waiting_for_user_input` 工具表增 `ExitPlanMode`（名字级，与 AskUserQuestion 同款；「全部 tool_use 块均 user-input 才 Waiting」的保守条件不动）。仅 claude_parser 消费该函数，影响面闭合。测试：尾挂 ExitPlanMode → Waiting；与其他工具混合 → 不 Waiting；有 tool_result 后不误判；
- **键序**（`inject/dialog.rs` + `remote/api.rs`）：dialog.rs 增单点判据 `screen_has_plan_title(lines)`；POST /session-approve 的 dialog 分支现场重解析后，若命中计划标题锚 → keys = `navigation_sequence`（无高亮 → Err 拒，不猜起点），投递后验证窗内对话框未消失 → **failed + 可读中文**（不回退数字——R1 实证数字在计划框无效）；非计划对话框维持 E2① DigitFirstWithVerify 一字不动；
- **GET 载荷**：`/session-approve-options` 增 `planDialog: true`（标题锚在场）+ `feedbackOption: "dialog:<n>"`（label 含 "tell claude"（大小写不敏感，屏上原文）的选项 id）；
- **反馈端点** `POST /m/api/v1/session-plan-feedback`（server.rs 路由 + api.rs handler，编排放 inject/approve.rs，文本通道复用 question.rs 的 FreeTextTerminal 缝）：`{sessionId, action: "start"|"type"|"clear", text?}`——start=现场重解析（对话框须仍在）→ 导航选到反馈项+Enter → 屏读验证 → `{status:"editor_ready", screen}`；type=send_text 打字（submit=true 尾随 Enter 提交开新一轮；false 仅暂存）；clear=退格清空（长度取屏读编辑行可见长度，按 T3 证据定案，轮次/长度封顶）。守卫复用 `try_acquire_inflight`；审计 action="approve"（摘要区分「计划反馈」）。

### T5（问题②-前端）计划批准卡 + 反馈条
- `api.ts`：ApproveOptionsView 增 `planDialog?` / `feedbackOption?`；新 `sessionPlanFeedback()`；
- `ApproveCard.tsx`：planDialog 形态标题「计划批准」；选项 1/2 照常点；feedbackOption 行渲染为「告诉 Claude 要改什么」入口 → 点击调 start 成功后上报 SessionDetail（新回调 prop）；
- 新组件 `PlanFeedbackBar.tsx`（蓝系，与问答卡自由作答按钮组同款交互）：SessionDetail 在 `planFeedbackActive` 时于 dock 渲染之（审批卡此时随状态转黄自然卸载，反馈条独立存活）——textarea + 「发送」(type submit:true) / 「覆盖写入」(clear+type 暂存) / 「清空」(clear)；发送成功 → 「已发送，Claude 正在修改计划」→ 自隐；会话切换/卸载即清态。刷新后不恢复反馈态（终端直接打字即可，如实登记已知边界）。

### T6 门禁 + 实机验证
- cargo fmt/check/clippy --all-targets + windows-gnu 交叉 check/clippy + cargo test；pnpm lint + build:mobile + vitest 全量；
- 实机矩阵：claude plan 全链（切换模式→计划产出→黄点变红点→dock 出「计划批准」卡→方向键选 1/2 退出计划→选 3 反馈条→打字提交→计划模式新一轮）；问答卡下方停靠视觉核对；过程折叠两态核对；
- 顺带诊断点：实机核对该场景 GET /session-question 载荷（用户截图出现过 questionBlocked 提示，若为陈旧 question 标记所致则登记后批）。

## §3 红线与明示不做
- 不碰 `docs/MASTER-PLAN.md`；探测八条铁律；锚文案不手造；测试零接触真实 `~/.mam`；
- 不做：消息流内容卡（plan 灰卡/plan-file 绿卡）改色；overlay 弹层形态（用 in-flow 停靠）；shift+tab 带反馈批准路径（T3 只登记不实现）。
