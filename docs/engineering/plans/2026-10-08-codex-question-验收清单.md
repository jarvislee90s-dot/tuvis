# codex 问答屏读 · 手工验收清单

> 对应设计：`docs/engineering/specs/2026-10-08-codex-question-screen-read-design.md` §4（场景验收规格，S1–S4 × 六核对点，S4 追加 notes 链与 esc 中止）
> 屏形事实源：`docs/engineering/specs/2026-10-08-codex-160-question-屏读底料.md`（2026-10-09 取证批，四闸门①活体底料）
> 形态矩阵：`.agents/skills/win-console-inject-probe/references/screen-read-matrix.md` codex 0.160.0 系列
> 永久探针（体检工具）：`MAM_PROBE_PID=<codex pid> cargo test --lib codex_question_live_probe -- --ignored --nocapture`（只读，零注入）

## 前置条件

- [ ] codex-cli **0.160.0 及以上**（形态词形以 0.160.0 取证为准；大版本升级须先过屏读矩阵「维护触发点①」复验再验收）
- [ ] codex 处于 **Plan mode**（取证批形态即 Plan mode 下采集；面板形态本身不依赖 mode，但保持同构可对表）
- [ ] MAM 与会话**配对成功**（手机端问答卡出卡以配对 + pending-question 门为前提）
- [ ] 宿主 = conhost 自建探针会话（`launch-session.ps1 -HostKind conhost -Cli codex -ProjDir <新目录>`），或用户知情的人工会话
- [ ] 信任框：新 ProjDir 首启会触发——用 `-EnterVK` 处置并 dump 落档（基线证据）

## codex 特有边界（验收前必读，逐条影响核对点解释）

1. **notes 远程作答已停用**：0.160.0 实测 notes 文本**不随卷提交**（回车把焦点行当答案提交，用户文本静默丢失）→ 远程自由作答请求返回**具名中止**而非注入。核对文案：`codex 自由作答（Other/notes）经实测不能安全代答：备注文本不会随卷提交（回车会把当前高亮项当作答案提交）。请到终端直接作答`。
2. **切题键 = h/l（vim 键位），箭头族无效**：VT 序列（ESC[A-D）与真 VK 箭头在面板上全部无效（屏面零变化）。任何「箭头切题/走位」预期对 codex 不适用。
3. **单题无切题面**：单题 footer 只有三段（无 `←/→ to navigate questions` 段），h/l 无切题效果——「切题」核对点在 S3 如实登记为「不适用/无效验证」。
4. **确认屏 0.160.0 未复现**：末题含未答题时 Enter **直接交卷**（answer 记为当时焦点项，无任何确认）。**用户侧警示：在末题/单题面板上按回车（含 notes 态回车）= 无提示交卷**——远程提交键的末题分支编排禁用数字、提交前归零闸必须先绿。
5. **esc = 中断回合（非取消）**：`-Vk 0x1B -Scan 0x01` 直接杀整个回答回合（`■ Conversation interrupted`）；孤立 0x1B 无效。esc 不是「关闭面板/取消输入」键。
6. **选中态只在属性层**：字符层无选中标记（`›` 仅表焦点）——人工目验以**行高亮**为准，屏读侧以解析器快照 + 计数段增减为准。

---

## S1 两题全单选（Question 1/2 → 2/2）

提示词（与底料同款）：`Use the request_user_input tool now: ask me exactly 2 questions. Q1 single-select 'Which DB?' with options Postgres, SQLite. Q2 single-select 'Which cache?' with options Redis, Memcached, None needed. Do nothing else.`

- [ ] **出卡**：面板出现（题号头 `Question 1/2 (2 unanswered)` + footer 四段），MAM 出卡且 GET 快照对位（题干/选项/焦点一致）
- [ ] **点选**：远程点选 Q1 选项 → 数字注入生效，卡面勾选态翻转且回执携带快照（屏读为准核验，非本地记忆）
- [ ] **切题**：数字推进自动跳 Q2（`Question 2/2`）；h/l 切回 Q1 焦点落已选项；l 再切回 Q2
- [ ] **末题提交**：Q2 答完后题号头归零词形（`Question 2/2` 计数段消失）→ 提交 → 摘要屏 `Questions 2/2 answered` 且 answer 行与所选项一致
- [ ] **漂移恢复 + 续答**：终端手动作答/空格反选（计数段重现 `Question 1/2 (1 unanswered)`）后回 MAM → GET 刷新卡面对齐屏读真值，续答不重复不丢
- [ ] **重启恢复**：MAM 重启后重开页面 → GET 初始化纠偏把卡面拉回终端当前屏（漂移场景覆盖）；面板已交卷则回摘要/正常屏

## S2 三题上限（1/3 → 3/3）

- [ ] **出卡**：三题卡出卡，题号头 `Question 1/3 (3 unanswered)`，状态栏行插入变体不破坏解析（footer 锚命中）
- [ ] **点选**：逐题点选推进（1→2→3），未答计数随每题递减
- [ ] **切题**：h/l 环形交叉（末题 l 回首题、首题 h 到末题）；j/k 走位双向环绕（末项 j 回首项）
- [ ] **末题提交**：3/3 归零后提交 → 摘要屏 `Questions 3/3 answered` 三组 answer 与所选项一致
- [ ] **漂移恢复 + 续答**：中途终端手动改答/跳题后 GET 对位；卡面与屏上当前题一致后续答
- [ ] **重启恢复**：三题中途重启 MAM → GET 拉回当前题号与已答态（未答数一致）

## S3 单题（Question 1/1）

- [ ] **出卡**：单题面板出卡，footer **三段**（无导航段）+ 题号头仍带 `(1 unanswered)`（单题也带计数段——与任务书旧预期不符的实测修正）
- [ ] **点选**：点选生效（j/k 走位 + 空格 或数字直选），计数段消失
- [ ] **切题**：h/l 均无效果（单题无切题面——如实验证「无效」而非跳过）
- [ ] **末题提交**：单题 = 末题 → 提交直接出摘要 `Questions 1/1 answered`；**含未答提交 = 焦点项直接交卷（确认屏未复现，用户侧警示生效）**
- [ ] **漂移恢复 + 续答**：终端手动改选后 GET 对位
- [ ] **重启恢复**：重启后 GET 对位或如实「面板已交卷」

## S4 Other/notes 链（单题变体）

- [ ] **出卡**：诱导 Other 的题（如 `Which database do you want for a hello-world script?` Oracle RAC/DB2）出卡
- [ ] **点选**：点选常规选项正常；焦点行 tab 复合键（选中 + 开 notes 输入态）在终端侧验证字符层与属性层证据（0x0030 高亮条）
- [ ] **切题**：单题变体下 h/l 无效（同 S3）
- [ ] **末题提交**：notes 态回车 = **连带整卷提交且 answer 落焦点行、notes 文本丢失**（0.160.0 实测定案——验证摘要无 notes 内容行）
- [ ] **漂移恢复 + 续答**：notes 打字后焦点跳回第 1 项——GET/回执快照如实反映焦点与计数，不假装同步 notes 文本
- [ ] **重启恢复**：重启后对位（notes 态字符层与普通面板同形，快照按面板解析不误报）
- [ ] **【S4 追加】notes 链具名中止**：远程自由作答请求 → 返回 `CodexFreeTextRetired` 具名中止文案（见头部边界 1），**零注入**（回执确认未发任何键）
- [ ] **【S4 追加】esc 中止行为**：面板存在时 esc 强中断 = 回答回合被杀（`■ Conversation interrupted - use /feedback if something went wrong`）——验证 MAM 中止路径不发 esc 取消语义（esc 不是取消键）；中止回执后前端自动重拉 GET，卡面按「面板已消失」如实降级

---

## 活体体检记录（闸门④：只读探针逐屏「认识/不认识」）

> 探针：`codex_question_live_probe`（零注入）。体检纪律：每个屏形探针后核对解析器产物与锚命中清单，全部必须「认识」；「不认识」→ 落证据 → 修判据（`cargo test --lib codex_snapshot_tests` 全绿）→ 重跑体检。

| # | 日期 | codex 版本 | 屏形 | 认识/不认识 | 证据文件 |
|---|---|---|---|---|---|
| 0 | 2026-10-09 | 0.160.0 | 升级提示框（非面板）+ 主界面基线 | 认识（快照 None + 全锚 MISS = 正确分派；探针零误报） | `codex-q-accept-baseline-updatebox.txt` |
| 1 | 2026-10-09 | 0.160.0 | S1 Q1 面板（两题首屏，footer 四段） | 认识（快照 Some：idx=0/total=2/unanswered=2/focused=Some(0)；NAVIGATE+SUBMIT_ANSWER 命中） | `codex-q-accept-s1-probe.txt` |
| 2 | 2026-10-09 | 0.160.0 | S1 数字推进后 Q2 面板（4 选项，footer `submit all`） | 认识（idx=1/is_last=true；SUBMIT_ALL 命中。本帧 footer 抓到读取抖动形态 `tnavigate`/`esc tosinterrupt` → NAVIGATE 如实 MISS，三槽任一判据成立——账本吸收漂移活体验证） | `codex-q-accept-s1-q2-probe.txt` |
| 3 | 2026-10-09 | 0.160.0 | S1 归零态（`Question 2/2` 计数段消失） | 认识（unanswered=0，SUBMIT_ALL 命中） | `codex-q-accept-s1-zero-probe.txt` |
| 4 | 2026-10-09 | 0.160.0 | 摘要屏（`Questions 2/2 answered`） | 认识（快照 None + RECEIPT 锚 `answered` 命中——摘要非面板，分派正确） | `codex-q-accept-s1-summary-probe2.txt` |
| 5 | 2026-10-09 | 0.160.0 | S3 单题面板（footer 三段 + 状态栏行变体） | 认识（total=1/is_last=true；SUBMIT_ANSWER 命中、NAVIGATE 如实 MISS——单题无导航段与底料一致） | `codex-q-accept-s3-probe.txt` |

体检结论：**闸门④通过**——六个屏形全「认识」，解析器判据零改动（唯一探针侧修正：None 成因诊断的「footer 锚」判定收窄为只含三个 Q_FOOTER 槽，避免摘要屏上把终态 RECEIPT 锚误报成「头词形漂移」——纯诊断文案修复，不影响解析）。
清场记录：自建会话 HOST=26300 / CMD=27892 / TARGET=28288 于体检后 `Stop-Process` 清场，tasklist 复核三 PID 均消失；用户自有 codex.exe（33752 / 28768，启动前基线快照）原样健在。会话记录 `codex-q-accept-session.txt`。
