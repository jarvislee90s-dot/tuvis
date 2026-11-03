# claude 问答卡与多选/多题键序复刻（2026-09-24）

> 分支 `feat/claude-auq-multiselect`（基线 main@78bf114 / v0.5.0-beta.1）。
> 性质：修缺陷 + 功能补齐批次。宪法（`docs/MASTER-PLAN.md`）无冲突，未改动。

## 0. 背景与根因

用户报告：claude 问答题在手机端被误判成「审批单选」（红色二元卡，允许=勾第 1 项、拒绝=取消），多选题/多题无法远程作答。三层根因：

1. **Bug A（阻断）**：`~/.claude/settings.json` 每个事件下 helper 与旧 bash 兜底**双注册**——两者同触发、双写同一事件文件，bash 只写基础字段且**后写覆盖** helper 的富载荷（`tool_name`/`tool_input` 丢失）→ AUQ 识别整体失效 → 裸 `Notification` 被通用审批映射接走 → 出审批卡。注册器的 `hooks_file_verified` 只查「规格命令在文件某处出现」，helper 在场即判已核验 → **跳过注册**，bash 残留永不清理。
2. **Bug B（功能）**：claude 多选 toggle 发**数字**（K8 旧证据），用户实机（**2.1.278**，npm 2026-09-22 自 2.1.251 升级）证明多选屏数字**完全无反应**——空格=切勾、↑↓=题内移动、←→=切题（`Type something`/`Next` 两行不生效）、`Next`+回车=下一题、Submit 页 1/esc。`space` 不在注入键域。
3. **Bug C（设计）**：二元审批卡红色并排小按钮，与问答卡（蓝色纵向编号列表）不同套。

**证据纪律**：全部新键序为用户手工实机取证（单样本），档案 `docs/research/refs/phase2-消息注入/2026-09-24-claude多选多题键序-用户实机取证.md`（含 K8 冲突三证并记、版本漂移申报、空格注入形态待复验）。实现**全部走屏读闭环**：每键发前定位、发后核验，形态不符即干净中止——单样本风险收敛为「可读的失败」而非「答错」。

## 1. 改动清单

### Stage 0 —— 根修 + 取证归档

| 文件 | 改动 |
|---|---|
| `docs/research/refs/phase2-消息注入/2026-09-24-claude多选多题键序-用户实机取证.md` | 新增：环境（2.1.278）、屏面转抄（页签栏 `← ☒ … ✔ Submit →`、无编号 `Next` 行、勾选框字形族 `[ ]`/`[✓]`/`[✔]`）、键序定案表、K8 冲突三证并记、复验条件 |
| `2026-09-22-键序大词典.md` §1 | claude 多选表按「用户实测推翻直接删」规则改写：数字→无反应、新增 空格/↑↓/←→/Next+回车 行 + 多题表单小节；版本口径注记 |
| `README.md`（research） | 索引加行 |
| `monitor/hooks.rs` | `hooks_file_verified` 升级为 JSON 解析级「每事件我方条目**恰好一条**且等于规格」判据；`register_hooks_in_file` 新增 `prune_extra_ours_entries`（跨条目只留第一条我方命令，掏空条目删除；用户命令永不触碰）；`register_all_hooks` 调用点传脚本路径。回归锁 ×3 |

### Stage 1 —— 单题多选闭环

| 文件 | 改动 |
|---|---|
| `inject/engine.rs` | `control_records` 增 `"space" => (0x20, 0x20)`（VK_SPACE+字符，与 enter/esc/tab 同形）；裸 `" "` 仍域外 |
| `inject/windows_console.rs` | 域错误文案补 space |
| `inject/question.rs` | ① 行块解析 `parse_question_rows`（以推进行为锚、向上收编号连续递减块——天然剔除滚回区杂行；分隔线下排除；勾选框按括号内容语义判）；② `run_toggle_stages` 闭环切勾（读屏定位 → 方向感知走位每步复核 → 空格 → **屏读核验翻转**，未翻转中止）；③ `run_submit_stages` 走位方向感知（新增 `up_steps`）+ 推进行标签集 {`Submit`,`Next`}（Next 精确相等防正文冒充）+ 第 1.5 段「已在 Review 屏直接确认」；④ Toggle 静态数字序列废止（显式 Err）；⑤ `action_supported` Toggle/Advance 门 |
| `remote/api.rs` | `StagePlan::ClaudeToggle{target}` + 派发臂 + `QuestionDispatch::ToggleDone{checked,verified}`（回执带屏读真值 `checked`）；段名常量 `toggle-row`；`stage_from_abort` 增切勾/切题关键词臂 |
| `mobile/ApproveCard.tsx` | 裁11 活口收口（用户裁决「完全并成一套」）：tone 恒 `question`、二元也走纵向编号列表（reject 徽标如实显示 `esc`）；PlanBody/plan-pending 的 rose 硬编码改 sky；警示脚注保留琥珀 |
| `mobile/QuestionCard.tsx` | 多选卡渲染勾选框字形 `[ ]`/`[✓]`（与终端同形）；toggle 回执带 `checked` 时以屏读真值同步本地态（不盲翻），缺失回落盲翻 |
| `mobile/api.ts` | stage 并集 + `toggle-row`/`advance`；`QuestionAnswerResult` + `checked`/`advanced` |

### Stage 2 —— claude 接入多题流（复用 1e14c56 地基）

| 文件 | 改动 |
|---|---|
| `inject/question.rs` | `run_advance_stages`（走位到 `Next`+回车+读屏分类：下一题页/Review 屏/中止；**已在 Review 屏=零按键 `advanced:false`**——← 回退未实测不乱试）；`multi_question_submit_supported` 放行 ClaudeFull；Advance 静态序列显式 Err |
| `remote/api.rs` | 三闸门纳入 claude（GET `multiQuestion`/`advance` 旗标、POST 多题闸门）；claude 单题载荷上的 advance → 400 bad_request（防误触）；`StagePlan::ClaudeAdvance` + 派发臂 + `AdvanceDone{advanced}` 回执；`bad_request` 归 400 |
| `mobile/QuestionCard.tsx` | advance 回执 `advanced:false` 时不推进（停在确认卡）；段名文案 advance |

## 2. 测试

- Rust：新增回归锁——hooks 双注册收敛 ×3、space 键域、行块解析 ×5（实机 dump/用户截图/滚回区剔除/单选空块/字形族）、闭环切勾 ×6（happy/走下/走上/未翻转中止/无焦点/目标缺席）、`toggle_sequence_is_no_longer_digit`（原 K8 锁改写）、端点级切勾 ×2、claude 多题 ×4（旗标+toggle/走位切题/Review 零键/已在 Review 直接确认）、`multi_questions` 拒绝载体换 zcode、`action_gates` 改写。三个改动模块全绿（inject 315 / server 146 / hooks 50）；全量 30 红均为 main 既有的 junction/symlink 环境测试（stash 验证与本批无关）。
- 前端：审批卡并蓝锁改写、勾选框字形 + `checked` 真值优先（含「回执仍 true 不翻回」反例）、盲翻回落。**825 全绿**；`pnpm check` 过。
- `#[ignore]` 实机占位：`claude_space_injection_form_live_probe`（空格形态复验指引——手工按键 ≠ 注入事件，回执「翻转」中止时的处置路径）。

## 3. 实机验收清单（待用户执行）

1. 启动 MAM 一次 → 读 `~/.claude/settings.json` 确认每事件只剩 helper 单条（bash 残留被自动清理）；
2. claude 问**多选单题** → 手机出蓝卡（不再有红色「等待批准」）→ 勾两项（卡面 `[✓]` 与终端同步）→ 提交 → Review 确认 → 会话文件出现 `Your questions have been answered: …`；
3. claude 问**三个问题（含多选）** → 逐题作答（多选子题勾选不切页）→「切换题目」逐题推进 → 确认卡「提交答案」→ 同上核验；
4. 二元审批卡与问答卡并排同形态（蓝、纵向编号）；
5. 若第 2 步回执报「空格已发出但屏读未见到勾选态翻转」→ 按 `claude_space_injection_form_live_probe` 指引复验空格形态（候选 char 形态）。

## 4. 已知边界（显式申报）

- ← **回退切题**未实现（用户实测该键在 `Type something`/`Next` 两行不生效，回退是复合动作未取证）；确认卡的「返回题目修改」在 claude 上为零按键不推进。
- 多选屏的 `Type something` 自由作答维持拒绝（复评 F6-3 口径不变）；`Chat about this` 行不触达。
- `families.rs` 的 `verified_with: "2.1.251"` 未改（A 族注入形态未在 2.1.278 复验——版本漂移只在取证档案申报）。
- 验收后应把「手机端→终端」逐键屏读对账追加进取证档案（≥3 取样升格「实测」）。
- **单选数字路径（select）也应随验收复验**（评审建议）：2.1.251→2.1.278 已推翻一条 K 结论，Select 分支的证据同样视为待版本对账。
- Review 屏锚（标题/副题）判「窗口内任意位置命中」（评审 Minor-5）：与既有第 4 段 `poll_review` 同暴露面（本批第 1.5 段未引入新类别），滚回区残留的 Review 文案理论上可误判——证据表明 Review 屏为瞬态屏，留观察。

## 5. 评审追记（2026-09-24，requesting-code-review 批）

评审结论 0 Critical / 2 Important / 6 Minor，判定 With fixes。已修：

- **I1 切勾目标身份核验**：`run_toggle_stages` 第 1 段新增 `screen_question_matches`——「页签栏→首个编号行」区域的去空白拼接须包含载荷题干；不一致（用户已手动 ←/→ 切题、手机卡与终端脱钩）= 零键中止。此前闭环只验按键效果、不验目标身份，脱钩时切勾会作用到别的题上。回归锁：不一致零键中止 + 载荷为屏面子串时放行。
- **I2 二元审批徽标谎报**：原实现给二元 reject 硬编码 `esc`、approve 显示序号 `1`——codex 的允许键实为 `y`，「所见即所按」只对 claude 成立。修正为**徽标只在 dialog 分支渲染**（屏读选项才有真实编号）；二元键位本就不外泄 UI（载荷仅 id/label 契约），不渲染是唯一不撒谎形态。此为对计划「reject 徽标显示 esc」一节的**有意偏差**（该写法经查证不成立）。
- **M3 走位方向校验**：位移必须**沿目标方向**恰 1 行（原只校验绝对值，反向位移会乒乓走到上限）。
- **M4 分隔线判定收紧**：只认制表符族 `─`/`━`（≥4 字符），ASCII `----`（markdown 水平线）不再触发分隔判定饿死行块。
- **M6 前端测试**：`advanced:false` 不推进 mqIndex 的回归锁补齐。
- 未修（如实申报）：M5（Review 锚窗口内任意命中——既有暴露面，见上节）；M7（stage_from_abort 关键词耦合——测试已锁定，重构 StageAbort 带显式段名留待后续批次）；M8（三处 dispatch 闭包重复——符合该文件既有风格）。

## 6. 实机验收事故与修复追记（2026-10-02，二轮评审批 `a06600c` 之后的实机批次）

**事故**：合并前实机验收触发两类失败——①多选卡点选报「读不到问答屏（屏读窗尽或不可用）」，零键中止；②「切换题目」终端**确实切了题**（走位+回车都成功）但卡片不推进，报「切题回车后既未进入下一题页、也未出现 Review 确认屏」。用户截图 + 活体只读探针（`screen_read_probe::dump_and_parse` / `dump_raw`，零注入）取证定位。

**根因两层，均在行块解析器**（屏读本身逐行准确——活体 dump 实证）：

1. **分隔线闩锁**：旧 `parse_question_rows` 见 `─` 分隔线即把其后**所有**行永久判「线下」——多题流滚回区的 Planning 块上下边框正是 `─` 分隔线且在题屏**上方**，闩锁提前翻转 → 整个题屏判死，解析恒 0 行（活体 q1 屏实证：`Next` 行明明在场仍 0 行）。这层缺陷打击**所有**多题题屏，与停在第几题无关。
2. **单选子题无锚**：多题流**单选子题**没有独立 `Submit`/`Next` 行（活体取证：`Type something.` 后直接分隔线），唯一 Submit 字样在页签栏里——旧解析无锚可用 → q2 屏恒 0 行 → 切题分类段无法确认「新页」。

**活体取证结论**（`references/screen-read-matrix.md` 首批实例，fixtures 逐字入档 `inject::question::live_fixtures`）：`→` 在选项行=切题、**末题直达 Review**（未答完有 ⚠ 警告行，可跳题）；`←` 从 Review 退回上一题（←/→ 是含 Review 的导航环）；焦点在 `Type something` 行时 ←/→ **无效**（归文本编辑光标——两行例外机理）；`↓`×4 焦点逐行走到 Type something 行。

**修复（本批）**：

- **解析器双锚**：闩锁移除；锚 A 取**最靠后**推进行（原「第一处」会被滚回区样行劫持）；新增**锚 B 页签栏**（复合签名：`←` 开头 + `→` 结尾 + 含 submit）向下收连续递增块、分隔线即停——单选子题屏可解析（产物无 Advance 行）。
- **切题双路径**：`run_advance_stages` 按首屏形态分流——有推进行走既有 Next+回车；单选子题发 `→`（焦点在自由作答行先 `↑` 回选项行并复核）；统一分类器 `classify_next_page`（Review 就绪 ∨ 焦点离开推进行 ∨ **题干区变化**）。
- **三态首段轮询**（修复 `a06600c` I2 的观测面回归）：「整窗读不到屏」与「屏在但形态不符」分开报，后者回执带**屏面摘要**（证据自带失败）。
- **回归锁**：活体夹具内核锁 ×10 + 端点锁 ×2（单选 `→` 全链、脱钩身份闸）+ 段名表新文案 ×9。Rust 1454 全绿。

**仍申报的边界**：← 回退切题仍未实现（本批取证补了 ← 从 Review 退回的证据，但「从任意题回退」的键路径未接入编排）；q3+ 屏形未活体取证（fixtures 中伪 q3 为派生）；Enter 单选即选语义未取证（用户真实作答时顺带观察）。

## 7. ←/→ 双向切题导航（2026-10-02 第二批，用户裁决）

**需求**：进入第 2 题及 Review 确认屏后无法返回之前的题修改——确认卡「返回题目修改」实为无效按钮（后端已在 Review 屏 → `advanced:false` 零按键）。用户裁决：**去掉该按钮，直接模拟 ←/→ 方向键**——除 `Type something` 行外，←/→ 通用对应上一题/下一题（活体取证背书：导航环含 Review、末题 `→` 直达 Review、`Type something` 行 ←/→ 归文本编辑）。

**改动**：

- **内核**（`inject/question.rs`）：`NavDirection { Prev, Next }`；`run_advance_stages` 方向化重写——Review 屏：Next=零按键 `advanced:false`（已在终点）/ Prev=发 `←`；题屏：FreeText 焦点先 `↑` 回选项行（两行例外先决，两方向共用）→ 发 `→`/`←` → 分类。分类器 `classify_navigation` 方向感知：Review 在场且 Prev = 键被吞中止；解析出题屏且**题干区变化** = 已切。**走位+回车路径退役**（`walk_direction_to_advance`/`probe_submit_row` 保留给 Submit 流程）。
- **协议**：answer 请求加 `direction: "prev"|"next"`（缺省 next，opencode 旧客户端零破坏；非法值 400）；回执 `AdvanceDone` 加 `direction` echo；审计 summary `advance::advance:prev|next`。
- **GET 能力旗标**：新增 `navBoth`（仅 claude 多题=true）——前端分流：navBoth=true 渲染 ◀ 上一题/下一题 ▶ 双钮（单选/多选都渲染，第 1 题隐藏 ◀）+ 确认卡「◀ 返回上一题修改」发 prev；false（opencode）维持旧单钮 + tab 回绕行为零变化。
- **前端**（`QuestionCard.tsx`/`api.ts`）：advance 分支按方向移动 mqIndex；prev 移动需回执 `direction:"prev"` echo 确认（opencode 旧回执无 echo → 维持回绕，跨工具零回归）。
- **测试**：Rust 内核 +5（prev 题页/Review/被吞/先决/next 复用）+ 端点 +2（走位断言重写为 `["right"]`、prev-from-review `["left"]`）；前端 +2（◀ 隐藏/发 prev、确认卡 prev echo）+ 既有断言重写。

**已知边界（申报）**：导航返回后卡面勾选态以**前端本地记忆**渲染（GET 载荷无 per-question 已答状态——既有限制，本批不扩；终端侧状态为准）；←/→ 在多选子题题页的行为为用户断言 + 闭环验证兜底（题页间 ← 未逐键活体取证，验收时实测确认）。

**实机验收追加项**：q1 勾选 → ◀/▶ 往返 → 下一题 → 确认卡 ◀ 返回修改 → 重新提交全链。

## 8. 多选自由作答 + 确认卡答案清单（2026-10-02 第三批，用户实测取证）

**取证结论**（用户实机 + 只读探针）：多选屏 `Type something` 行（勾选框行）**直接打字符即内联编辑**（无需先按空格/回车），文字替换行内容、勾选自动置 `[✓]`；**打完字按回车 = 取消勾选**（文字保留但 Review 不保存）——正确保存 = 打字后不发任何键，切题时自然落盘。

**改动**：

- **后端**：`run_multi_select_free_text_stages`（定位 kind==FreeText 行 → 方向键走位（多选屏数字无效）→ 字符通道打字 → 屏读核验「文字入行 + 勾选保持」→ **零后续键**）；`action_supported` 放行 claude 多选 freeText（kimi/codex 维持拒绝）；`StagePlan::ClaudeMultiFreeText`；该行已有内容时不覆盖（追加行为未取证，中止引导终端清空）。
- **解析器**：FreeText 行判定扩展——label 前缀 **或**「块内编号最大且带勾选框」（打字后 label 变用户文字，前缀判据失效；claude 的 Type something 恒为最后追加项）。
- **advance 先决扩展**：`↑` 离开自由作答行后核验该行**文字与勾选保留**（防止半途状态被切题存入）。
- **前端**：多选题卡自由作答输入框（`freeText && navBoth && multiSelect`；发送带 questionIndex；成功记录 `mqFreeText` 不置终态）；确认卡**已选答案清单**（`mqChecked`/新 `mqSelected`（单选）/`mqFreeText` 三源渲染，未答显示占位，脚注「以终端 Review 页为准」——本地记忆可能与终端漂移的申报）。

**实机验收追加**：多选题输入自由作答 → 发送 → 屏读核验通过（卡面「已写入」）→ 下一题 → 确认卡清单显示文本 → 提交 → Review 页含该答案。

## 9. 屏读快照权威源评审收口（2026-10-03 第二轮评审，本地未推送批次）

评审范围 `697d70c..79e8330`（日志写文件 / 分类轮询 / 快照权威源三批），结论 With fixes：1C/8I/7M。处置：

- **C1 Review 屏误判题屏**：`question_screen_snapshot` 加 Review 前置判定（Ready → None）——anchor B 会把「页签栏+编号确认项」解析成题屏，题干区含答毕回显会错跳题并清勾选态；前端 `advanced:false` → 直接进确认卡。回归锁 ×1。
- **I1 freeText 三态**：快照加 `free_text_present`（占位/打字/行缺席可分）；GET 占位确认 → 清本地残留；回执 `text:null` = 未核验 → 不虚报「已写入」，琥珀提示 + 保持可编辑。测试 +1。
- **I2/I3 快照对位校验**：`findQuestionByHeading`（空串跳过/精确优先/多义放弃——不猜纪律）；advance 应用前 heading 必须唯一对位到 dest 且勾选数与载荷选项数一致。
- **I4 交互纪元**：每次动作自增，晚到的旧快照（纪元已变）不再把已前进的卡拉回去。
- **I5 freeText 首段三态**：`poll_question_screen`——形态不符中止带屏面摘要（不再误报「读不到问答屏」，观测面教训与 toggle 路径对齐）。
- **I6 中止后重拉**：aborted → 前端自动重拉 GET（键可能在窗尽后才被消费，快照随 GET 生效）。
- **I7 单题勾选同步**：GET 快照在单题卡映射进 `setChecked`。
- **I8**：`snapshot_to_json` 单点去重；快照 Rust 测试 ×2（Review None / 三态判别）。
- **M 系列择要**：M1 轮询窗真实时长入档（≈2.9s）；M2 Review 标题宽判据起点归类；M3 panic hook 写日志；M4 日志 5MB 轮转；M5 断链/类型修正；M6 AGENTS.md 补申报（单选选中项无屏读标记，mqSelected 仍本地记忆）；M7 advanced=false 跳过读屏。

## 10. 实测验收通过（2026-10-03）+ commit 重组

用户确认整轮实测通过（多选/单选自由作答、数字直选自适应、编辑覆盖、清空、
◀/▶ 切题、确认卡清单、重开页面对位）。本分支 28 个 commit 按逻辑批次重组
为 5 个后合并 main。
