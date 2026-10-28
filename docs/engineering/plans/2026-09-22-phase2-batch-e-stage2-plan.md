# 批次戊 · Stage 2 编码计划 v2（任务 × 定案表 × TDD 夹具 × 文件地图）

> 2026-09-22 v2（用户指令压缩：13 任务→8，按域合并）。依据：设计文稿 `2026-09-22-phase2-batch-e-design.md`（§0 裁1–裁20）+ **spec** `docs/engineering/specs/2026-09-22-phase2-batch-e-interactive-closure-design.md`（行为契约与评审基准）+ 差距矩阵 `2026-09-22-phase2-batch-e-gap-matrix.md`（§6.3–§6.6）+ 键序大词典**清淤版**（唯一键序事实源）+ 定案表汇总（docs/research/refs/phase2-消息注入/2026-09-22-戊探定案表汇总.md）。
> **执行序：E1（危害级矛盾，最高优先）→E2→E3 清 A 类矛盾；E4–E7 补 B 类新能力；E8 收口。**
> 执行方式：执行 agent 在独立窗口顺序执行（本计划为唯一 SSOT）；**评审由主控窗口进行——单评审 agent 一轮，规格（对 spec §2–§7）+质量合并**；评审发现的 Important/Critical 由执行 agent 修复后同评审复审。
> TDD：夹具先行失败测试 → 实现 → 端到端 `#[ignore]`。
> 红线：不 push、不动 main、不动 `docs/MASTER-PLAN.md`；不修改设计文稿/差距矩阵/本计划/spec 的裁决与任务定义（验收清单条目更新除外）；六门禁（cargo 一律 `CARGO_TARGET_DIR=target/gate-run` 且在 `src-tauri/` 下；bin 须 `--features hook-listener`；vitest 终跑勿与 cargo 并行）；实机项一律 `#[ignore]`；台账如实；每任务独立 commit（`feat(mfix-e): E<N> …`，文档 `docs(mfix-e): …`，评审修复 `fix(mfix-e): E<N> …`）；Esc 类注入单次+屏读确认；opencode **Ctrl+C 一律禁注**（裁19）；kimi 审批框数字禁用。

## §0 编号映射（v1 S2-T* → v2 E*；旧两套戊编号一并废止）

| v2 | 合并自 v1 | 设计旧戊 | 矩阵旧戊 | 覆盖 |
|---|---|---|---|---|
| E1 | S2-T1+T10 | 戊8+T1 新增+opencode 扩入 | —（矩阵 §2 插队行） | 插队与队列全链（A1 危害+三家扩展+整队回执+Ctrl+C 禁注） |
| E2 | S2-T2+T5 | 戊5+A2 新增 | 戊4+A2 新增 | 对话框与审批修正（数字优先双路径+锚点+布局契约） |
| E3 | S2-T3+T4+T11 | 戊6+戊7 | 戊5+戊6+戊8 | 权限菜单与模式回读（两家锚点+守卫+Default 推断+回执锚+高亮） |
| E4 | S2-T6（+T9-kimi 面） | 戊3+戊4（kimi 面） | 戊3 | kimi 问答全链（多选/多题/Other+composer 入口） |
| E5 | S2-T7（+T9-codex 面） | 戊2+戊4（codex 面） | 戊1 | codex 多题卡与 Tab 备注（N1+#78 codex） |
| E6 | S2-T8（+T9-opencode 面） | 戊1+戊4（opencode 面） | 戊2 | opencode 多题/多选卡（N4） |
| E7 | S2-T12+T9 残余 | 戊9+戊4（统一面） | 戊7 | UI 统一与入口一致性（裁11+占位+#78 收尾） |
| E8 | S2-T13 | 丁T6' | 戊9 | 收口 |

依赖：E1/E2/E3 相互独立（E1 最先）；E4/E5/E6 相互独立；E7 依赖 E4–E6（入口/占位对齐其端点）；E8 最后。

## §1 夹具库（TDD 输入，先建后开工）

全部取自探测档案正文「裁15 逐行原文」节（`docs/research/refs/phase2-消息注入/2026-09-22-戊探*.md`，本地持久；`%TEMP%` 探测目录不保证存活，仅作补充），拷入**仓库根** `tests/fixtures/e-stage2/`（跨语言先例=`tests/fixtures/plan_pending_cases.json`：Rust 侧经 `../tests/fixtures/e-stage2/` 读、vitest 同路径读；文件头加 `# source: <档案名 §节>` 注释行）：

| 夹具文件 | 来源 | 用途 |
|---|---|---|
| `claude-recall-state.txt` / `claude-interrupted-state.txt` | 戊探F claude 段 §2.2（撤回态=消息回输入行；中断态=`Interrupted` 标记） | E1 |
| `claude-approve-dialog-e2.txt` / `-e1.txt` | 戊探E（误纳复现 30 行+干净对照） | E2 |
| `codex-perm-menu-idle.txt` / `-busy.txt` | 戊探D codex 段（4 dump 行号一致；busy 混屏） | E3 |
| `kimi-perm-menu-{permission,yolo,auto}.txt` + `kimi-perm-receipt.txt` | 戊探D kimi 段+K-1 回执实录 | E3 |
| `kimi-question-multiselect.txt` / `kimi-question-multi-q.txt` / `kimi-question-review.txt` | 用户 K-5 实录三屏+戊探B 单题 Review 屏 | E4 |
| `codex-notes-inline.txt` + rollout `user_note` 行 | 戊探C | E5 |
| `opencode-confirm-page.txt` / `-checked.txt` | 戊探A | E6 |

## §2 任务详情

### E1 · 插队与队列全链（A1 危害优先 + 三家扩展；原 T1+T10）
- **定案输入**：词典 §1 Esc/多条队列行、§2 Esc/Tab 原生排队行、§3 busy/Ctrl+S 行、§4 busy/插队节（清淤版）；spec §5。
- **改动**（`inject/queue.rs` + `inject/mode.rs`）：
  ① **claude 撤回窗口防护**：`interrupt_first` 在 `wait_turn_stopped` **Stopped 且有屏读**路径上、投递正文前，纯函数判据检查**输入行残留**（判据陷阱：`❯` 同时出现在输入行与队列展示区——排除已知 hint 串后取**最底部** ❯ 行+相对状态栏位置，以两份夹具驱动定案）；命中 → **中止投递**+回执「未投递：终端输入行有残留内容（可能是被撤回的消息），请人工确认」（不自动清空——清空键未实测）；StillRunning/Unverifiable 路径维持现状（注释写明分派理由）。
  ② **codex 插队**：打字→Tab 入队→Esc×1 直插（回退=草稿+Esc+手动 Enter）。
  ③ **opencode 插队**：Esc 打断直插；**全链键序未定面**（草稿 Esc 后去向）`#[ignore]` 实机首测定案并回填词典，实现不得预填。
  ④ **kimi**：busy 直接投递=排队制（无需 Esc）；**条件项** Ctrl+S 立即插队——产品 Rust 引擎通道 `#[ignore]` 复验**通过才上**，不通过只留排队制，结论落档。
  ⑤ 回执按**整队批量**（裁16）；`supports_interrupt` 扩展；**opencode Ctrl+C 全局禁注**（注入引擎键黑名单，裁19）。
- **TDD**：纯函数两夹具正反例；「Stopped+残留→不投递+回执如实」「Stopped+干净→照常投递」；变异锁（判据恒 false→撤回用例红）；既有 5+6 例投递时机锁全绿；三家插队键序路由锁；Ctrl+C 黑名单锁；kimi 排队回执锁（不谎报 delivered）。
- **DoD**：上述全绿+C-25/C-47 判据更新（含撤回防护）；codex/opencode `#[ignore]` 实机插队各一（或如实登记未跑）；kimi Ctrl+S 复验结论落档。

### E2 · 对话框与审批修正（A2+N5+裁12；原 T2+T5）
- **定案输入**：词典 §1 计划批准框行（数字直选终裁+渲染等待规则）+对话框标题行锚行；spec §3；戊探E。
- **改动**（`inject/approve.rs`+`inject/dialog.rs`+前端卡）：
  ① claude 审批键序改 **DigitFirstWithVerify**：注入前屏读确认选项簇已渲染（未就绪在 `MENU_POLL_TOTAL_MS` 窗内轮询）→发数字→屏读验证生效→未生效导航回退（既有 NavigateConfirm）。**kimi 审批档不动+加锁**（数字禁令）。
  ② 选项簇定位改「**离底栏最近合格簇+标题行锚**」，弃最长簇；e2 误纳夹具（正文编号 3–8 项与真选项同屏）永久回归。
  ③ 卡体总高度上限+长内容默认折叠+点开（裁12：不得把 composer 顶出首屏）。
- **TDD**：数字成功/数字无效转导航/未渲染不注入/kimi 维持导航锁×2；e2 夹具解析出 3 真选项+e1 对照不回归；布局组件测试（上限/折叠/展开）。
- **DoD**：claude 计划批准框数字路径 `#[ignore]` 实机成功；N5 误纳关闭。

### E3 · 权限菜单与模式回读（A4+A5+N8；原 T3+T4+T11）
- **定案输入**：词典 §2 权限菜单三态/busy 换档行、§3 权限菜单定位/切档回执/×busy 行；spec §4；戊探D+K-1。
- **改动**（`inject/mode.rs`+守卫+前端）：
  ① 菜单解析分家——codex：标题 `Update Model Permissions`↔footer 短语窗+编号项（折行不产新项）+`(current)`；kimi：标题 `Select permission mode`↔footer 窗+**两行组**+`▶`/`← current`；关键词簇判据**退役删除**（依据=戊探D 五锚实证+N6 归因）。
  ② 守卫收窄：`blocks_control_injection` 仅拦「对话框在场」（注入前单次屏读快照，D20 例外条款），busy 一律放行（裁17）；前端问答待决→权限/模式**置灰+原因文案**；kimi 待决态**含回车整条硬拒绝**。
  ③ 回读：codex Default=底栏无模式词的**缺席推断**；kimi 权限档=切档回执锚 `Permission mode: <档名>`+`← current`；claude 环序四档底栏词表。
  ④ 前端模式栏**当前档高亮**+漂移口径（kimi 批准后自动切出 plan 如实显示）。
- **TDD**：两家菜单夹具定位锁（kimi「总是询问」跨折行可达——N6 失败行 `Never interrupts you;…` 正确归属）；codex busy 混屏 dump 菜单块不被污染；busy 放行锁×2/待决拒绝锁×2；Default 缺席推断词表锁；回执锚解析锁；高亮组件测试。
- **DoD**：kimi「总是询问」`#[ignore]` 实机切换成功（回执 `Permission mode: Always Ask`）；N2①②③+N6+N8 关闭面。

### E4 · kimi 问答全链（N7+A3；原 T6+T9-kimi 面）
- **定案输入**：词典 §3 问答节（清淤版）+K-5 实录；spec §2。
- **改动**（`inject/question.rs`+前端卡+composer）：
  ① 多选=切勾单次（数字/回车/空格任一）→tab 切 Submit→'1'（char/VK）或 Enter 确认。
  ② 多题=**DigitAdvance**（数字直选+自动推进，**禁尾 Enter**——A3 禁令）；**Review 汇总屏**（`Ready to submit your answers?`+[1]/[2]）解析为提交判据。
  ③ Other=行数字→打字→回车→Review→提交；单题 TwoPhaseSelect 维持。
  ④ composer freeText kimi 序列（Other 行）+卡内入口。
- **TDD**：多选切勾单次净效果锁（N7 双切抵消回归）；多题无尾 Enter 锁（K-5 两题屏夹具）；Review 汇总屏解析；Other 全链；落账 `interaction.resolved`（method）对账；composer 路由锁。
- **DoD**：`#[ignore]` 实机多选+多题各一全链。

### E5 · codex 多题卡与 Tab 备注（N1+#78 codex 面；原 T7+T9-codex 面）
- **定案输入**：词典 §2 request_user_input 节（Tab/数字/切题行）；spec §2；戊探C。
- **改动**（前端多题卡+`inject/question.rs`+composer）：
  ① 前端多题卡逐题渲染（题干+选项+已答态）。
  ② 键序：数字即选+自动推进/`←→` 切题/末题 Enter=submit all/**Tab=备注文本框 toggle**（框内打字含数字入文本、框内 Esc 退回；多题每题 `[Tab→备注→Enter]`）。
  ③ 落账对账 `answers.<qid>.answers` 第二元素 `user_note: <全文>`；composer 备注/freeText 入口。
- **TDD**：键序档路由锁；备注 toggle 夹具（footer 变化）；多题卡渲染组件测试；rollout 对账夹具。
- **DoD**：`#[ignore]` 实机 2 题全链（1 题带备注）。

### E6 · opencode 多题/多选卡（N4；原 T8+T9-opencode 面）
- **定案输入**：词典 §4（清淤版）；spec §2；戊探A。
- **改动**（前端逐题卡+`inject/question.rs`+composer）：
  ① 前端逐题卡（tabs 形态，`(○)/(✓)` 已答标记）。
  ② 键序：tab 前向切页（h/l 亦是切页键——打字守卫：不在题页裸打字）/↑↓ 定位/**enter 或数字=toggle**（空格禁用）/Confirm 页 enter 提交/own answer=↓定位→**enter 开输入行**→打字→enter（裸打字守卫）。
  ③ esc 取消=`state.error`+dismissed 回执；SQLite 对账（answers **二维数组按题序**+`multiple`，副本查询）。
- **TDD**：toggle 双向锁；Confirm 提交锁；own answer 开行守卫锁；二维数组对账；composer 入口。
- **DoD**：`#[ignore]` 实机多选+多题各一。

### E7 · UI 统一与入口一致性（裁11+#78 收尾；原 T12+T9 残余；依赖 E4–E6）
- **改动**：①配色（裁11）：审批**对话框卡**并入 sky 蓝系；**二元审批**默认保留红系（可随 UI 方案调整——裁11 留的活口，注明）。②composer 占位四态一致性+freeText 入口文案跨家一致（对齐 E4–E6 端点）。③issue #78 关闭手续（三家自由作答实机各一或如实登记）。
- **TDD**：tone 映射快照；composer 占位/freeText 文案快照。
- **DoD**：N3 关闭；#78 可关。

### E8 · 收口（原 T13）
- 全六门禁+fmt/clippy 0 警告+`pnpm check`+`build:mobile`；验收清单 **I 节**（人工项从定案表生成，含 `#[ignore]` 清单）；**C-4 旧口径统一**（删批次乙「排空回执」表述，指向 C-47+E1 撤回防护）；**N2 守卫回归确认**；台账+handover；**停下等主控 review**。

## §3 验收（对 spec §8）

spec §2–§7 逐条规格符合（评审基准）；gap 矩阵 §2 四家对照全 ✅（逐格以 DoD 为准）；卡死场景清零；issue #78 关闭；六门禁全绿。实机项允许「登记未跑」如实标注，不得谎报。

## §4 执行纪律（全局）

1. 执行 agent 独立窗口顺序执行 E1→E8，不并行；每任务=夹具失败测试→实现→门禁面→单 commit→台账行→简报。
2. 评审=主控窗口**单评审 agent 一轮**（规格对 spec §2–§7+质量合并）；Important/Critical 修复后同评审复审（`fix(mfix-e): E<N> …`）。
3. 夹具必须来自真机屏读原文（§1 清单），禁止手造「看起来也行」的相邻态（批次丁 H-4① 教训）。
4. 词典（清淤版）为唯一键序事实源；与实机/代码冲突 → 停下上台账（裁20：用户实测优先），不擅自改词典。
5. Esc 类注入单次+屏读确认；Ctrl+C（opencode）与 kimi 审批框数字为永久禁令。
6. 完成判据=DoD 全绿，不含「计划写完」。
7. 台账：`.superpowers/sdd/2026-09-18-phase2-injection-mainline/progress.md` 文末「批次戊 Stage 2」节逐任务追记；commit 前 fmt/clippy+受影响测试面全绿，E8 跑全六门禁。
8. 长跑接力：若会话上下文吃紧，commit+台账写明进度断点，新会话以台账为接力凭据续跑。
