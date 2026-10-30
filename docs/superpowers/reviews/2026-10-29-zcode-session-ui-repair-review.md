# ZCode 会话状态与紧凑控制区修复 —— 整批最终复审（R6）

- 日期：2026-10-31
- 复审身份：整批最终 reviewer（新上下文，不继承任何实施者自评）
- 仓库／分支：`/Users/jarvis/Documents/tuvis-worktree2` ｜ `codex/zcode-unified-remote`
- BASE：`cb79b7cb`（`docs(zcode): record second-review P2 fixes`）
- HEAD：`81fb3e22`（`fix(mobile): stop mount reset from swallowing first-frame clicks`）
- 整批差异：15 个提交（`review-cb79b7cb..81fb3e22.diff`，592 KB）
- 验收合同：`docs/superpowers/plans/2026-10-29-zcode-session-ui-repair.md` §7（逐字执行）

> **续写说明（2026-10-31，R6 收口）**：§0—§12 是 reviewer 在 `81fb3e22` 的**原判，逐字保留**，
> 不改写、不删减。收口时点 HEAD 为 **`3b7da564`**（阻塞项修复提交）；本轮只做三件事——
> 在最终 HEAD 上重出视觉证据、更新中英手册、把两条用户裁决写进偏差表。所有处置以
> `**处置（…）**` 追加在原判之后（§0.1 是索引，§10 是逐条），历史判断仍然可读。

> 本报告所有断言均给出 `文件:行` / 测试名 / 截图文件名；查不到证据的一律写「无证据／待验」，不把未跑写通过。

---

## 0. 结论摘要

> 下表与「最重要 5 条」是 **`81fb3e22` 时点的原判**，保留不动；`3b7da564` 之后的逐项处置见 §0.1，
> 双轨结论按新时点重述见 §11.1。

| 项 | 结论 |
| --- | --- |
| 规格覆盖 | **部分达标**：U1/U2/U4/U5/U6 实现与自动证据成立；U3 代码成立但最终 HEAD 无渲染证据；U7（本文）在门禁红的情况下不能判达标 |
| 问题计数 | **阻塞 2 ／ 重要 5 ／ 次要 6** |
| 代码是否可合并 | **不可合并**：§6 门禁 10 项中 6 项红；其中 4 项（2 条 `cargo test` 编译失败、2 条 `clippy -D warnings`）由本批代码直接导致，HEAD 在文档指定命令下不可构建／不可验证 |
| 产品／实机是否完成 | **未完成**：新卡动态出现与应答、队列跨端、单一主按钮、窄屏布局四项均待实机；其中「单一主按钮」连浏览器渲染证据都没有 |

### 最重要 5 条

1. **阻塞**：`cargo test --locked --features marker-helper` 与 `--tests --features marker-helper,zcode-bridge-evidence` 在 HEAD **编译失败** —— `src-tauri/tests/zcode_control_e2e.rs:989`、`:1018`、`:1259` 的 `PendingView` 字面量缺 `detail` 字段（该字段于 `9e8acbe1` 引入）。该目标承载「两台设备同 actionId 单次派发」「换链接零帧」「SSE lag 纠偏」等既有守卫负例，当前全部不可运行。
2. **阻塞**：`cargo clippy --all-targets --features marker-helper -- -D warnings`（及 evidence 版）失败 —— `src-tauri/src/services/zcode_control/types.rs:743` `clippy::doc_lazy_continuation`（编号列表后的续行未缩进/未空行）。
3. **重要**：U3 的浏览器证据在 HEAD **不存在** —— 20 张截图属 `d0e8c894`（R4，14:51），而 `741e6005`（20:44）已删除独立打断行并把打断并入主按钮；截图中的「打断」独立行不是 HEAD 的界面。
4. **重要**：R6 要求更新的 `docs/user-manual/{zh,en}/08-mobile-remote.md` **未更新**（`git diff --stat cb79b7cb..81fb3e22 -- docs/` 无该文件）；用户可见行为（单一主按钮、队列展示、附件问号）无手册口径。
5. **重要**：运行中发送恒带 `requestedDelivery="queue"`（`src-tauri/src/services/zcode_control/command.rs:662`、`:1468-1478`），覆盖宿主 `inputRouting.mode="guide"` 的插话语义 —— 与桌面行为不一致，产品口径仍未裁决。

### 0.1 收口处置索引（`3b7da564`，2026-10-31 续写）

> 只列「原判 → 处置」的对应关系；每条的原判正文在 §10 保留不动，处置追加在条目末尾。

| 条目 | 原判 | 处置 | 复验证据 |
| --- | --- | --- | --- |
| B-1 集成测试目标不可编译 | 阻塞 | **已修**（`3b7da564`） | `git show 3b7da564 -- src-tauri/tests/zcode_control_e2e.rs`：三处 `PendingView` 字面量补 `detail`（`question_card` / `old_pending_card` = `None`，`permission_card` = `Some("ls -la /tmp/tuvis-e2e-scratch")`）；该提交记录「marker/evidence 全量 test 与 clippy 均 exit=0、fmt 通过」——**本轮未重跑 cargo，重门禁由控制方复跑**（§2.5） |
| B-2 `clippy -D warnings` 失败 | 阻塞 | **已修**（`3b7da564`） | `types.rs:743` 续行缩进 3 空格（`clippy::doc_lazy_continuation`），仅排版、语义不变；同上提交内记录 clippy 两条命令 exit=0 |
| I-1 U3 无最终 HEAD 渲染证据 | 重要 | **已修**（本轮） | 34 张 `r6-*` 截图 + `metrics.json`：11 张 `primaryAction=stop`、34/34 `strayInterruptRows=0`、34/34 无横向溢出；逐张肉眼核对（§8.2） |
| I-2 队列展开区无渲染证据 | 重要 | **已修**（本轮） | `r6-queue-paused-expanded-*`、`r6-queue-consumed-*`、`r6-queue-active-expanded-*`：`#1/#2` 正文、暂停/已消费、无编辑按钮（§8.2） |
| I-3 用户手册未更新 | 重要 | **已修**（本轮） | `docs/user-manual/{zh,en}/08-mobile-remote.md` 新增「手机控制区：一个主按钮、一行工具栏、一张卡一件事」（中英结构对等，6 条） |
| I-4 强制 `queue` 覆盖 `guide` 插话 | 重要 | **维持现状 + 用户裁决**（本轮，D-1） | 裁决原文见 §9 D-1；不进本轮代码改动，列入实机待验 |
| D-3 附件提示改常驻 | 偏差 | **维持并确认**（本轮，D-3 裁决） | 裁决原文见 §9 D-3；`r6-attach-uploading-*` / `r6-attach-failed-*` 显示常驻提示 + 主按钮禁用 |

**原结论不改口**：以上处置只清「代码阻塞 + 证据/文档缺口」，§11 的双轨结论在 §11.1 按新时点重述——
仍分「代码可否合并」与「产品／实机是否完成」两问，后者仍是**未完成**。

---

## 1. 审查方法与独立性声明

1. 先读计划全文（§1 用户裁决、§5 真实时序、§6 门禁、§7 合同）、原设计 §5、能力 parity spec R1—R6、宪法目录级约束，再读代码；**不以任何任务报告的自评为依据**。
2. 状态链逐层走读：`host（官方 wire）→ bridge → projection → capabilities（resolver）→ GET/SSE → hook → UI → command → host 确认`，并逐条对照 §5 的五个真实时序。
3. 视觉证据独立复核：打开 `metrics.json` 逐条校验（20/20 `toolbarRows=1`、无横向溢出），并**肉眼核对 3 张截图**（`toolbar-360-light-zh-100.png`、`approval-360-light-zh-100.png`、`narrow-320-light-zh-125.png`）。
4. 门禁：`/tmp/r6-gate.log`（在本复审开始前已由控制方启动，跑完于 01:41:22，`DONE`）。Cargo 五项我逐条核对了退出码与报错原文；前端四项按日志记录并对其失败项做了独立复跑（见 §2.2）；此外**自行定向复跑**了 7 个 mobile 测试文件（见 §2.3），未重跑全量 `pnpm test`、未重跑 clippy。
5. 未做的事：未接触真实钥匙串、未真机操作、未升级 ZCode、未 push。

---

## 2. 门禁结果（计划 §6，同一最终 HEAD `81fb3e22`）

### 2.1 逐条结果

| # | 命令 | 结果 | 证据 |
| --- | --- | --- | --- |
| 1 | `cargo test --locked --lib services::zcode_control::` | ✅ exit=0 | `598 passed; 0 failed`（`/tmp/r6-gate.log`） |
| 2 | `cargo test --locked --features marker-helper` | ❌ **exit=101** | `error[E0063]: missing field 'detail' in initializer of PendingView` → `tests/zcode_control_e2e.rs:989 / 1018 / 1259` |
| 3 | `cargo test --locked --tests --features marker-helper,zcode-bridge-evidence` | ❌ **exit=101** | 同上（同一测试目标） |
| 4 | `cargo clippy --all-targets --features marker-helper -- -D warnings` | ❌ **exit=101** | `src/services/zcode_control/types.rs:743` `doc list item without indentation`（`-D clippy::doc_lazy_continuation`） |
| 5 | `cargo clippy --all-targets --features marker-helper,zcode-bridge-evidence -- -D warnings` | ❌ **exit=101** | 同上 |
| 6 | `cargo fmt --check` | ✅ exit=0 | — |
| 7 | `pnpm test` | ❌ exit=1（**3 条 5s 超时，非断言失败**） | `FAIL tests/lib/displayChineseScan.test.ts`（6624ms）、`FAIL tests/mobile/InteractiveCard.test.tsx`（16903ms）、`FAIL tests/pet/usage-locale-scan.test.ts`（8535ms）；报错均为 `Error: Test timed out in 5000ms`；`Test Files 3 failed \| 187 passed (190)`。**单独复跑三条全部通过**（1.6s / 3.4s / 1.9s）——见 §2.2 |
| 8 | `pnpm check` | ❌ exit=1（前三步绿，**build 被仓库 safe-delete 闸拦下**） | format ✓ / lint ✓（`0 errors, 1 warning` 既有 `react-hooks/exhaustive-deps`）/ check:i18n ✓ / check-error-codes ✓ / `pnpm build` ✗ `[SAFE_DELETE][SAFE_DELETE_BULK_CONFIRM_REQUIRED] {"count":90,"threshold":50,"targets":["…/dist/pet"]}`（`dist/` 为 `.gitignore:5` 忽略的构建产物） |
| 9 | `pnpm build:mobile` | ✅ exit=0 | — |
| 10 | `git diff --check` | ✅ exit=0 | — |

### 2.2 前端门禁的真实结论（不粉饰）

- `pnpm test` 的 3 条失败**全部是 5 秒超时**，不是断言不成立：单独复跑

  ```
  pnpm exec vitest run tests/lib/displayChineseScan.test.ts tests/mobile/InteractiveCard.test.tsx tests/pet/usage-locale-scan.test.ts
  → 3 files / 7 passed（1.6s / 3.4s / 1.9s）
  ```

  其中 2 条（中文扫描、usage locale 扫描）与本批代码无关（本批未改 `src/` 下桌面端产物，也未改 locale 调用点）；第 3 条 `InteractiveCard` 是 R5 报告已用基线 worktree 对照过的**既有负载敏感用例**（`task-R5-report.md:88-113`）。
- **必须如实披露的干扰源**：我在门禁跑 `pnpm test` 的同时并行跑了 4 个 mobile 测试文件（§2.3），与全量套件的 worker 争抢 CPU，很可能是这 3 条超时的直接诱因。单独复跑（机器空闲）全部通过。
- 因此 `pnpm test` 的判定是：**门禁当次为红，但红因是超时且可复现为绿；不构成代码缺口**。反过来也不能写成「通过」——按 §7，它仍是「未在最终门禁中取得绿」的证据缺口（问题 I-5）。
- `pnpm check` 的 build 失败是仓库自带的 **safe-delete 批量删除闸**（`count=90 > threshold=50`，目标是 `dist/pet` 构建产物）。`dist/` 被 `.gitignore:5` 忽略，属本机产物残留；同批 `pnpm build:mobile` exit=0。仍记为红（问题 M-6），但**不是本批代码缺陷**。

### 2.3 我自行定向复跑的前端测试（HEAD，非门禁外推）

```
pnpm exec vitest run tests/mobile/ZcodePendingCard.test.tsx tests/mobile/zcodePrimaryAction.test.tsx tests/mobile/ZcodeQueuePanel.test.tsx
→ 3 files / 34 passed

pnpm exec vitest run tests/mobile/ZcodeControlPanel.test.tsx tests/mobile/ZcodeSessionIntegration.test.tsx tests/mobile/ZcodeControlAttachments.test.tsx tests/mobile/zcodeControlHook.test.tsx
→ 4 files / 86 passed
```

结论：**前端单测面在 HEAD 是绿的**（120 条）。Cargo 的 `--lib` 面也是绿的（598 条）。红的是「集成测试目标编译」与「clippy」——这两项恰是整批从未在 HEAD 上跑过的门禁条目。

### 2.4 阻塞项影响评估

`tests/zcode_control_e2e.rs` 不是「多余文件」，它承载这些**本轮必须保持的守卫负例**（文件头注释逐条列出）：两台设备共享同一账本的单次派发、Hub 未就绪时读链独立、SSE lag 纠偏前零写、换链接旧 pending 零帧、换链接后旧动作零帧。它们当前**无法编译**，因此「目标／代际／幂等／Unknown 负例」在本批只能靠 `--lib` 单测与 wire_e2e 覆盖，e2e 这一层证据链是断的。

### 2.5 收口后的门禁状态（2026-10-31，`3b7da564`）

§2.1 的十项是 **`81fb3e22` 时点**的结果，保留不改。`3b7da564` 之后：

| 项 | 状态 | 依据 |
| --- | --- | --- |
| #2 / #3 `cargo test`（marker / evidence） | **已修** | `3b7da564` 补三处 `detail` 字面量；提交记录该目标 exit=0。**本轮未重跑**（重门禁由控制方执行） |
| #4 / #5 `clippy -D warnings` | **已修** | `3b7da564` 修正 `types.rs:743` 续行缩进；提交记录两条 clippy exit=0 |
| #7 `pnpm test` 三条 5s 超时 | 未变（原判 I-5） | 超时由并行跑测试引起、单跑全绿；仍需一次无人干扰的全量绿 |
| #8 `pnpm check` safe-delete 闸 | 未变（原判 M-6） | `dist/pet` 为本机构建产物残留，非代码缺陷 |
| 轻量文档门禁（本轮实跑） | 见下 | `git diff --check` exit=0；`node scripts/check-i18n.mjs` 通过（2597 键） |

**`prettier --check "docs/user-manual/**/*.md"` 的如实记录**：命令 exit=1，输出列出**全部 27 篇**手册——
这是**既有基线**（表格列宽对齐不符合 prettier 期望），不是本轮引入。已验证方式：把 `HEAD` 版本与
本轮改动版分别喂给 `prettier` 再 diff，两者的格式化差异**行数与位置完全一致**（zh 9 对、en 14 对，
均落在 §「无头注入」与 v0.5.0 截图表格），本轮新增的「手机控制区」小节**零格式化差异**。
按任务的 `|| true` 口径，该项不阻断本轮。

---

## 3. 覆盖表 A：U1—U7 逐行（计划 §1、原设计 §5）

| 要求 | 设计／计划依据 | 最终代码位置 | 自动测试／回环证据 | 实机证据或待验 | 状态／偏差裁决 |
| --- | --- | --- | --- | --- | --- |
| **U1** 附件「＋」旁补问号，展示既有存储说明 | §1 U1；§2「MessageComposer 已有问号入口」 | `src/mobile/ZcodeControlPanel.tsx:762-774`（`zcode-attach-help`，`aria-expanded`／`aria-controls` 仅在展开时给出）；文案渲染 `:830-838`；复用键 `mobile.composer.attachHint`（`src/i18n/locales/zh.json` / `en.json`，未新写存储说明） | `tests/mobile/ZcodeControlAttachments.test.tsx:395` `describe("ZCode 面板：附件问号说明（R4/U1）")`：默认折叠 → 展开文案逐字等于 `i18n.t("mobile.composer.attachHint")` → 再次点击折叠 → **零文件选择器 / 零上传 / 零 POST**；实测通过（§2.3 含该文件的 8 条） | 浏览器截图 `attachhelp-360-light-zh-100.png`（问号展开态，含 `.tuvis-attachments/`）；真机未验 | **满足**（自动＋浏览器证据）；真机触达仍待验 |
| **U2** 「打开官方页面／模型／思考强度」合并为一行紧凑工具栏 | §1 U2；§1 布局裁决（360px/100% 一行；320px/125% 最多两行） | `src/mobile/ZcodeControlPanel.tsx:625-683`（`data-testid="zcode-control-toolbar"`，`data-layout="single-row"`、`data-max-rows="2"`；模型 select `flex-1 min-w-[4rem] max-w-full` + `title` 全名；档位与官方页入口 `shrink-0`） | `tests/mobile/ZcodeControlPanel.test.tsx:2506` `three_controls_share_one_toolbar_container_and_keep_protocol`（同容器 + 协议零变化）、`:2554` `disabled_selection_keeps_reason_semantics_next_to_toolbar`、`:2589` `toolbar_exposes_row_layout_hook_and_keeps_long_model_name_accessible`（显式声明「类名只是代理，像素证据在截图目录」） | 16 张 360/390 × 明暗 × 中英 × 100/125% + `longmodel` + `narrow-320`；`metrics.json` 20/20 `toolbarRows=1`、三层 `scrollWidth ≤ clientWidth`；肉眼核对 `toolbar-360-light-zh-100.png` 为一行、`narrow-320-light-zh-125.png` 无溢出 | **满足**（本次实现；工具栏块在 `d0e8c894..81fb3e22` **零改动**，故 R4 截图对 HEAD 仍成立）；真机系统字号待验 |
| **U3** 运行中空输入=打断；有文字/就绪附件=发送；有内容不得先打断 | §1 U3；§4-R5 | 纯判据 `src/mobile/zcode-primary-action.ts:18-29`；面板装配 `src/mobile/ZcodeControlPanel.tsx:242-295`（`running` 只取官方 `control.activeWorks` 中唯一 `primaryTurn`；多目标 → `target_ambiguous`）、`:789-808`（单按钮 `data-primary-action` / `data-primary-mode`）；后端投递意向 `src-tauri/src/services/zcode_control/command.rs:659-665`、`:1468-1478` | `tests/mobile/zcodePrimaryAction.test.tsx` 8 条（含 brief 三条逐字断言）；`tests/mobile/ZcodeControlPanel.test.tsx:866`（运行中空输入→stop，恰一个 stop）、`:887`（运行中+文字→send，零 stop）、`:909`（多目标→禁用+原因）、`:959`（busy 双击零重复）、`:1016/:1059/:1100/:1128`（就绪/失败/上传中/组合附件）、`:1163`（Unknown 不重发不清草稿）；Rust：`command.rs:4987` `running_send_requests_queue_delivery`、`wire_e2e.rs:2453` `running_send_queues_officially_and_drains_without_resend`（实跑 ok） | **无 HEAD 渲染证据**：20 张截图属 `d0e8c894`，图中仍是「打断」独立行；R5 报告 §5 自认「本轮未做浏览器量测，R4 的 20 张截图不含本任务的新文案」 | **部分**：代码与自动证据满足；**视觉待验**（见问题 I-1）。行为面另有一处偏差（运行中强制排队覆盖 guide 插话），见 D-3　**处置（2026-10-31，`3b7da564`）**：已在最终 HEAD 重出 20 张（16 矩阵 + 4 打断态）`r6-*` 截图，`strayInterruptRows=0`、无横向溢出，单一主按钮在 360/390 × 明暗 × 中英 × 100%/125% 全部成立 → **I-1 关闭**；行为偏差按用户裁决维持现状（§9 D-1） |
| **U4** 真实问答卡可远程答；蓝色、纵向、一次一题；多题本地暂存、最终一次提交 | §1 U4；原设计 §5.1 | `src/mobile/ZcodePendingCard.tsx:552-618`（只渲染 `questions[current]`，`data-layout="vertical"` + `flex-col` + 每项 `w-full`）、`:528-551`（确认页列出全部答案）、`:327-347`（`submitAnswer` 一次 `ControlAction::Answer`，按原题序/原题干/原 `value`）；外观 `:454` `tone={isPermission ? "approve" : "question"}` → `src/mobile/InteractiveCard.tsx:53-60`（sky/蓝） | `tests/mobile/ZcodePendingCard.test.tsx:286` `three_questions_show_one_at_a_time_with_vertical_options`（第 1/3 题、其他题 `queryByText` 为 null、容器 `flex-col`、`fn` 零调用）、`:387` `confirm_lists_all_three_answers_before_any_post`、`:412` `final_submit_posts_exactly_one_answer_with_original_values`、`:443` `incomplete_answers_disable_submit_and_name_the_missing_question`、`:494`（旁路 revision 不丢草稿不解锁）、`:523`（换卡/换代清草稿）、`:558`（迟到回执不串卡）、`:760` `question_card_uses_blue_tone`；`:250` `first_frame_click_is_not_swallowed_by_mount_reset`（`81fb3e22` 修复，探针为兄弟组件 `useLayoutEffect` + 原生 `click()`，报告记录 HEAD 3/3 稳定红） | 浏览器截图 `toolbar-360-light-zh-100.png` 可见蓝色「等待回答」卡+「第 1/3 题」+纵向两项+上一题/下一题/提交（该卡组件在 `d0e8c894..81fb3e22` 无视觉改动）；**真机答题未验** | **满足**（自动＋浏览器）；真机答题、宿主撤卡待验 |
| **U5** 已存在审批请求正确出现，按既有受限选项应答 | §1 U5；§1 明确排除「常驻授权/全访问开放」 | `src/mobile/ZcodePendingCard.tsx:236-238`（只放行 `allowOnce`/`deny`）、`:499-527`（纵向、`w-full`）；宿主侧 `src-tauri/src/services/zcode_control/pending.rs:399-413`（`fullAccess` 硬拒）、`:527-540`（`sensitive`、`detail` 投影）；`src-tauri/src/services/zcode_control/types.rs:744-760`（2000 字截断） | `tests/mobile/ZcodePendingCard.test.tsx:730` `renders_only_once_and_deny_vertically`、`:769` `detail_is_collapsed_read_only_and_never_submitted`；`tests/mobile/ZcodeControlPanel.test.tsx:706` `permission_options_use_host_values_and_hide_full_access`、`:758`（无可选项→就地原因）；Rust `pending.rs:772` `permission_snapshot_is_retained_with_original_options`（实跑 ok） | 浏览器截图 `approval-360-light-zh-100.png`：红色「等待批准」+ `Bash` + 命令正文 + 允许一次/拒绝纵向；**真机应答与「宿主是否弹卡」未验**（计划 §1 U5 已声明不保证每次都弹） | **满足**（自动＋浏览器）；真机应答待验 |
| **U6** 手机显示官方待处理队列（含暂停／已消费） | §1 U6；§4-R2 | 投影 `src-tauri/src/services/zcode_control/projection.rs:313-363`（`items[]/autoDrain/pauseReason`，坏形状→`known=false`，身份只取 `queueItemId`，`position` 取 `order.queuePosition`）、`:259-272`（队列随 `SessionView` 走 GET+SSE）；DTO `src/lib/api/zcodeControl.ts:255-273`（`queue?` 缺省=未知）；UI `src/mobile/ZcodeQueuePanel.tsx:46-111`（未知不渲染／空不占位／暂停保留一行／展开只读列表）；挂载 `ZcodeControlPanel.tsx:614` | Rust：`projection.rs:772` `queue_snapshot_and_delta_preserve_input_identity`、`:850` `queue_is_unknown_before_host_declaration`、`:872` `queue_snapshot_then_paused_delta_reconciles`、`:946` `queue_pause_reason_is_projected_without_guessing`；`projection.rs` 另含 `promoted_input_leaves_queue_without_duplicate_message`；e2e `wire_e2e.rs:2454` 全链（空闲未知→运行中宣告→send 带 `requestedDelivery=queue`→ACK `inputId` 与 `commandId` 同源→SSE `queue_<commandId>`→**GET 两次仍重建**→消费后消失且零新 send 帧），实跑 ok。前端：`tests/mobile/ZcodeQueuePanel.test.tsx` 10 条、`ZcodeControlPanel.test.tsx:2643` `renders_official_queue_from_session_view`、`:2672` `omitted_queue_stays_unknown_without_panel` | 截图仅折叠行（「排队 2 条 已暂停 已手动暂停」）；**展开列表无渲染证据**；真机官方队列跨端未验 | **部分**：链路与自动证据满足；展开区视觉待验（问题 I-2）；另注意暂停原因 `manual` 的写入点在 R0 记为「低置信度」　**处置（2026-10-31，`3b7da564`）**：已补 `r6-queue-paused-expanded-*`（暂停 + `#1/#2` 正文）、`r6-queue-consumed-*`（已消费：`排队 0 条` 且暂停标志保留）、`r6-queue-active-expanded-*`（未暂停）三张，均无编辑类按钮 → **I-2 关闭**；跨端真机仍待验 |
| **U7** 局部修复后整批对照设计／计划复审 | §1 U7；§7 | 本文件 | 本文件即证据；但整批门禁红（§2） | — | **部分**：规格核查已完成，但「仅阻塞清零时给出代码可合并」的合同条件未满足　**处置（2026-10-31，`3b7da564`）**：两条阻塞已修，覆盖表按修复后状态更新（U3/U6 视觉证据补齐、I-3 手册补齐），合同条件恢复；本轮**未重跑 cargo**，代码可否合并见 §11.1 的口径 |

---

## 4. 覆盖表 B：已有八项动作逐项（能力 parity spec §4.1 / R1—R6）

> 不以 `enabled=true` 计数替代真实交互：每一项都写「执行入口 → 自动证据 → 实机状态」。

| 能力 | 设计依据 | 最终代码位置 | 自动测试／回环证据 | 实机证据或待验 | 状态 |
| --- | --- | --- | --- | --- | --- |
| send | §4.1 强证据；R4 判据「revision 前进 + 新用户消息可见」 | `ZcodeControlPanel.tsx:385-404` → `hook.submit` → `POST /m/api/v1/zcode/actions` → `hub.execute` → `command.rs:627-668` `execute_send` | `ZcodeControlPanel.test.tsx:365` `send_uses_official_api_not_legacy_or_headless`、`:1378` `double_click_sends_one_action`；Rust `wire_e2e.rs` `full_chain_send_receipt_audit_and_sse`、`running_send_queues_officially_and_drains_without_resend`（ok） | 官方页已有两月使用证据（继承）；Tuvis 通道真机发送：本批未验 | 满足（代码＋回环） |
| single_select | §4.1 弱（ACK only）→ 首次确认 | `ZcodePendingCard.tsx:568-616`（单选替换）；能力位 `capabilities.singleSelect` | `ZcodePendingCard.test.tsx:286`、`:322`；Rust `hub.rs:6583` `first_question_after_empty_get_is_answerable_without_refresh`（同代 SSE 新卡→能力现算→`execute` 放行，ok） | 真机答题未验（R4 首次确认兜底） | 满足（代码）；交互待验 |
| multi_select | §4.1 未实连 → interface_matched 候选 | `ZcodePendingCard.tsx:585-592`（toggle 增删保序） | `ZcodePendingCard.test.tsx:322`；`ZcodeControlPanel.test.tsx:1605` `multi_select_submits_multiple_host_values`；`ZcodePendingCard.test.tsx:250`（多选卡首帧点击） | 未实连 | 满足（代码）；交互待验 |
| free_text | §4.1 未实连 | `ZcodePendingCard.tsx:619-658`（与选项互斥、`sensitive` 用 password + 失焦清显） | `ZcodePendingCard.test.tsx:322`、`:620` `sensitive_answers_never_reach_browser_storage`；`ZcodeControlPanel.test.tsx:1727` `typing_free_text_replaces_selected_option`、`:1756`、`:1783` | 未实连 | 满足（代码）；交互待验 |
| interrupt | §4.1 弱（ACK only） | `ZcodeControlPanel.tsx:406-415`（唯一 `foregroundExecutionId`，零 `sendText` 夹带） | `zcodePrimaryAction.test.tsx:18/42`；`ZcodeControlPanel.test.tsx:796` `interrupt_only_targets_user_selected_primary_execution`、`:866`、`:909`、`:1163` | 真机「守卫消失」未验 | 满足（代码）；交互待验 |
| approval_reply | §4.1「既有卡可」；R4 判据「目标 pending 消失 + revision 前进」 | `ZcodePendingCard.tsx:236-238/499-527`；闸 `pending.rs:399-413` | `hub.rs:6653` `first_permission_after_empty_get_is_answerable_without_refresh`（ok）；`ZcodeControlPanel.test.tsx:706/758`；`ZcodePendingCard.test.tsx:730` | 截图红卡可见；**真机应答未验** | 满足（代码＋回环）；实机待验 |
| model | §4.1 强证据（继承） | `ZcodeControlPanel.tsx:456-466` → `kind:"selection"` | `ZcodeControlPanel.test.tsx:2506`（换模型仍发同形状 selection）、`:2554`（能力关→双控件禁用+原因）、`:1816` `model_thought_controls_are_capability_gated` | 用户已实测认可（计划 §1 明示继承） | 满足（继承） |
| thought_level | §4.1 强证据（继承） | `ZcodeControlPanel.tsx:468-471` | 同上三条 | 用户已实测认可 | 满足（继承） |

**结论**：八项均有「UI 执行入口 → 官方动作 → 服务端闸 → 回环/单测」三层对应；**但没有一项在本批获得新的真机执行证据**。按 §7「不以 enabled=true 替代真实交互」，本列只能判「代码闭环、交互待验」。

---

## 5. 覆盖表 C：阅读／附件／配置／授权复用（原设计 §5、凭据复用设计 §10）

| 项 | 本批改动或继承位置 | 相关回归 | 证据 | 状态／裁决 |
| --- | --- | --- | --- | --- |
| 阅读链（看板/正文/文件） | **无本批改动**：`SessionDetail.tsx` 仅改 `approveMounted`（`:518-527`）与旧 `QuestionCard` 挂载（`:1313-1320`） | `tests/mobile/ZcodeSessionIntegration.test.tsx:227` `zcode_composer_uses_control_panel_not_legacy_send`（阅读面照常）；Rust `zcode_control_e2e.rs` 用例 2 因编译红暂不可跑（§2.4） | 前端实测通过；e2e 证据链断 | **无意回归：未见**；但阅读链的 e2e 证据本批不可运行（列入问题 B-1 影响） |
| 旧 CLI 卡不再与原生卡并存 | `SessionDetail.tsx:518-527`、`:1313-1320`（`isZcodeSession` 同时关掉 `ApproveCard` 与 `QuestionCard`） | `ZcodeSessionIntegration.test.tsx:308` `zcode_does_not_mount_legacy_question_or_approve_cards` + `:323` `non_zcode_still_mounts_legacy_approve_card`（**正向对照**，防空断言） | 实测通过 | 满足 |
| 附件上传（管线/条目/引用/清理） | `ZcodeControlPanel.tsx:330-383`（与 MessageComposer 同形，`uploadAttachment` + `attachmentMarkup`）；accepted 清条目 `:107-124` | `ZcodeControlAttachments.test.tsx` 8 条（含 R5 新增「运行中+就绪附件随正文上行」在 `ZcodeControlPanel.test.tsx:1016`） | 实测通过 | 满足（附件说明复用 `mobile.composer.attachHint`，未改存储目录） |
| 配置（链接保存/校验）与「打开官方页面」 | `ZcodeControlPanel.tsx:153-205`（只读 `official-control`，挂载不外跳；`window.open(..., noopener,noreferrer)`） | `ZcodeControlPanel.test.tsx:1332` `official_page_is_an_explicit_auxiliary_action`、`:2506` 同用例内断言 `openSpy` 与零 POST | 实测通过 | 满足 |
| 授权／凭据复用（JWT 保险箱、不落日志） | **无本批改动**（`hub.rs` producer/`parts` 未动）；新增代码零 `log::*`（对整批 diff 检索 `^+.*log::` 为空） | 凭据复用验收（`2026-10-27-zcode-credential-reuse-acceptance.md`）继承；`hub.rs` 单测 `production_reset_preserves_restored_account_authorization`（ok） | `--lib` 598 条绿 | 满足（继承，无新增风险面） |
| 冲突态「断开并抢回」 | `ZcodeControlPanel.tsx:165-200`、`:490-524` | `ZcodeControlPanel.test.tsx:2152/2165/2181/2204` 四条 | 实测通过 | 满足（R1 契约；真机抢回未验） |

---

## 6. 覆盖表 D：目标、代际、幂等、Unknown、凭据保护（宪法及既有合同）

| 守卫 | 最终执行闸位置 | 负例测试 | 结果 | 裁决 |
| --- | --- | --- | --- | --- |
| 目标绑定（session/generation） | `hub.rs:3175-3193`（代际先于一切）、`:3211-3221`（bind 后复核代际）、`command.rs:607-624` `check_action` | `hub.rs` `stale_generation_sweep_discards_without_ledger_write`（ok）；`zcode_control_e2e.rs` 换链接用例 **编译红** | 部分可跑 | **未弱化**，但 e2e 层证据断（B-1） |
| 代际（UI 侧） | `ZcodeControlPanel.tsx:264-280`（`staleGeneration` 同时关 send/stop） | `ZcodeControlPanel.test.tsx:991` `旧代际（会话投影落后于连接代际）→ 主按钮禁用并带可见原因`；`ZcodePendingCard` 侧 identity 隔离 `:494/:523/:558` | 通过 | 未弱化（单按钮合并后仍保留两道闸） |
| 幂等（actionId） | `command.rs:643-657`（`reserve_with_kind`）；hook 每提交一个 `newActionId` | `ZcodeControlPanel.test.tsx:1378` `double_click_sends_one_action`、`:1446` `pending_cannot_be_resubmitted_with_new_action_id`；`PendingCard.tsx:207-229` `submitOnce` + `settledRef` | 通过 | 未弱化 |
| Unknown 不重发 | `useZcodeControl.ts:489-497`（unknown 只置原因，不重发）；`panel:222-237` 无自动重发 | `ZcodeControlPanel.test.tsx:402` `accepted_and_unknown_keep_distinct_copy`、`:1163`（打断 Unknown 不清草稿）；Rust `wire_e2e.rs` `malformed_ack_is_unknown_without_resend`、`rejected_ack_is_terminal_without_resend`（ok） | 通过 | 未弱化 |
| 停目标歧义 | `ZcodeControlPanel.tsx:242-247`（多目标 → `null` → `target_ambiguous`） | `ZcodeControlPanel.test.tsx:909`、`:1964`（英文侧码 = `target_ambiguous`） | 通过 | 未弱化（比旧实现更严：不再「随便取第一个」） |
| 审批越界（fullAccess / allowAlways） | `pending.rs:399-413` 硬拒；UI 只渲染 `allowOnce/deny` `ZcodePendingCard.tsx:236-238` | `pending.rs:816+`（`fullAccessOption` 不进 options）、`:913`；`ZcodeControlPanel.test.tsx:706` | 通过 | **未放宽**（计划 §1 排除项被守住） |
| 权限模式切换 / 常驻授权 | 无任何入口 | `ZcodeControlPanel.test.tsx:706`（`fullAccess` 不渲染） | 通过 | **明确不做，未偷做** |
| 凭据保护 | 手机 DTO 无 token/JWT 字段（`zcodeControl.ts` 全文无 token）；无新增日志 | 代码走读 + diff 检索 | 通过 | 未弱化 |

---

## 7. 状态链逐层走读（对应 §7「至少逐层走读」）

### 7.1 逐层字段是否真接通

| 层 | 关键点 | 证据 |
| --- | --- | --- |
| host → bridge | 官方 `state.queue` / `pendingInteractions[].payload.detail` 由 `observe_conversation_frame` 分派（`bridge.rs:456-482`）；`Projection` 持有队列 | `bridge.rs:461-479`；R0 证据 `docs/superpowers/reviews/2026-10-29-zcode-r0-wire-evidence.md:116-146`（`autoDrain`/`pauseReason`/`queueItemId` 三处来源一致，置信度高） |
| bridge → projection | `apply_snapshot` 整体替换队列，`apply_patch` 带 `queue` 才换（缺席不动） | `projection.rs:274-299`；`queue()`→`view()` `:254-272` |
| projection → capabilities | **本批核心修复**：`apply_relay_session` 收「本帧现算」的能力，不再沿用最后一次 GET 的矩阵 | `hub.rs:1417-1443`（`view.capabilities = if view.ready { capabilities } else { default }`）；事件泵在**锁外**现算 `hub.rs:2039-2050` |
| capabilities ← resolver | 生产 `policy_source` = `bridge.host_evidence()` 现读 → `resolve_capabilities`（shipped + DB 账本 + 接口形状），`capabilities_for` 单一实现供 GET 与 SSE 共用 | `hub.rs:903-914`、`:1899-1918`、`:2265-2268`、`:3145-3153` |
| GET | 每次 GET 现算 + `ready=false` 强制全关 + 拒写非 paired 的旧代际 | `hub.rs:3131-3172` |
| SSE | `emit_session_event` 直接序列化整个 `SessionView`（含 `queue`） | `hub.rs:1264-1278`；`remote/zcode.rs:168` `Json(view)` |
| hook | 帧直接落地 DTO；`ready=false`/恢复各一次有界 GET；旧代次/别的会话丢弃；action 帧只纠偏不重放 | `useZcodeControl.ts:195-203`、`:309-335`、`:409-427` |
| UI | `session.queue` → `ZcodeQueuePanel`；`session.pending` → 逐题卡；`control.activeWorks` → `running`；`capabilities` → 各闸 | `ZcodeControlPanel.tsx:216-221`、`:242-295`、`:574-614` |
| command | `send` 带 `session.generation`；运行中由服务端现读 control 决定 `requestedDelivery`；`stop` 带 `expectedForegroundExecutionId` | `ZcodeControlPanel.tsx:398-415`；`command.rs:659-665`、`:1468-1478` |
| host 确认 | 回执 `accepted≠生效`；`confirmed` 才显示已生效；`unknown` 不清草稿 | `ZcodePendingCard.tsx:80-88`；`useZcodeControl.ts:489-497`；`ZcodeControlPanel.tsx:222-237` |

**字段级结论**：`queue` 字段在 Rust `SessionView`（`types.rs:933-949`，`#[serde(default)]`）→ 手机 JSON → TS `QueueView?`（`zcodeControl.ts:255-273`）→ 组件三态判定，**全链接通**；`detail` 字段 `types.rs:721` → `pending.rs:540` → `ZcodePendingCard.tsx:469-498`，**全链接通**。没有「加了字段但没人读」或「UI 读了不存在的字段」的情况。

### 7.2 §5 五个真实时序

| 时序 | 自动证据 | 判定 |
| --- | --- | --- |
| 1. 空闲无卡 GET → 新问答/审批 SSE → 无需重进即可答 | `hub.rs:6583` / `:6653`：先 GET（能力全开为假），再同代 SSE 推新卡，断言帧内 `singleSelect/approvalReply.enabled=true` 且 `execute` 放行、后端 `execute_calls=1`；另 `:6716` 两会话交替不借矩阵、`ready=false` 全关。**用的是动态 source 翻转，不是「初始全 true 夹具」** | ✅ 成立 |
| 2. 三题分题暂存 → 旁路 revision → 回改 → 确认 → 一次提交 → 撤卡 | `ZcodePendingCard.test.tsx:286/387/412/494`；撤卡端由宿主 SSE 投影变更驱动（回环层 `wire_e2e` 的卡式用例） | ✅ 成立（自动）；真机未验 |
| 3. 运行中带内容发送 → 官方入队 → 暂停 → 刷新仍显示 → 消费后收敛 | `wire_e2e.rs:2454`（完整七步，含 GET 两次重建、消费后不复活、零新 send 帧）；暂停态 `projection.rs:872/946`；UI 三态 `ZcodeQueuePanel.test.tsx:50-109` | ✅ 成立（回环）；真机未验 |
| 4. 空输入 stop 一次 → 未知保留现场不重发；目标变化零误停 | `ZcodeControlPanel.test.tsx:866/909/1163`；Rust `wire_e2e.rs` `stop` 守卫用例（`--lib` 绿） | ✅ 成立（自动）；真机未验 |
| 5. 上传/题卡/队列并存正文可读；切会话换代不串 | `ZcodeControlPanel.test.tsx:2225`（多卡切组不丢）、`:2296`（两卡各自草稿）、`ZcodePendingCard.test.tsx:523/558`；`ZcodeControlPanel.tsx:126-151`（切会话清附件/回执/展开态）+ `:147-150`（卸载 abort 在途上传） | ✅ 成立（自动）；真机未验 |

---

## 8. 交互／视觉核对（不只凭 JSX）

### 8.1 截图证据的实际覆盖边界（重要）

截图目录 `docs/superpowers/reviews/2026-10-29-zcode-ui-screenshots/` 在 **`d0e8c894`**（R4，14:51）提交，而 HEAD 含 **`741e6005`（R5，20:44）** 与 **`81fb3e22`（01:06）**。据此逐块判定：

| 区域 | 是否被 R5 改动 | 截图对该区域是否有效 |
| --- | --- | --- |
| 工具栏（`zcode-control-toolbar`） | `git diff d0e8c894..81fb3e22 -- src/mobile/ZcodeControlPanel.tsx` 中**无** toolbar/model/thought 行 | **有效**（U2 判据成立） |
| 逐题卡 / 审批卡 | R5 未动；`81fb3e22` 只改 effect 与 ref（无视觉） | **有效**（U4/U5 颜色与纵向成立） |
| 队列折叠行 | R5 未动 | **有效**（仅折叠行） |
| **发送区（主按钮／是否仍有独立打断行）** | R5 **删除**独立打断行、改为单按钮 | **无效**：图中「打断」独立行在 HEAD 已不存在 |

> **处置（2026-10-31，`3b7da564`）**：已在最终 HEAD 用同一夹具重出 **34 张 `r6-*`** 截图
> （16 张排版矩阵 + 18 张状态/组件场景），并把 R4 时期的 20 张改名为 `r4-*`（含 `r4-metrics.json`）
> 保留为历史。逐块复核结果见 §8.2；因此 §8.1 的「发送区无效」结论只适用于 `r4-*`，不再适用于现行证据。

### 8.2 最终 HEAD 重出证据的逐张核对（34 张）

方法：`preview/shoot.mjs` 用 CDP 精确设视口宽，逐场景读回真实浏览器布局量测写入 `metrics.json`；
每张 PNG 均由执行者**逐张肉眼过目**（非抽样），核对四项：非空白/非报错页、主按钮态与场景一致、
无横向溢出与裁切、无残留独立打断行。

| 场景族 | 张数 | 关键量测（全量 `metrics.json`） | 肉眼结论 |
| --- | --- | --- | --- |
| 排版矩阵 `r6-send-{360,390}-{light,dark}-{zh,en}-{100,125}` | 16 | `toolbarRows=1` 16/16；三层溢出 0/16；`modelTitle` 16/16 非空 | 工具栏单行、「打开官方页面 + 模型 + 思考强度」齐全；主按钮为「发送/Send」；无裁切 |
| 打断位 `r6-stop-*` | 4 | `primaryAction=stop`、`primaryMode=stop`、`strayInterruptRows=0` 4/4 | 主按钮即「打断/Interrupt」，下方一行说明；**没有任何独立打断行** |
| 就绪附件 `r6-send-attach-*` | 1 | `attachmentStatuses=["ready"]`、`primaryAction=send` | 空文字 + 就绪附件 → 主按钮仍是「发送」 |
| 上传中 / 失败附件 `r6-attach-*` | 3 | `primaryAction=disabled`、`primaryDisabled=true`、`attachNotice` 非空 3/3 | 主按钮置灰 + 常驻提示；条目带 `too_large` 失败原因 |
| 问答卡 `r6-question-*` | 2 | `pendingKind=userInput`、`pendingBorderColor` ≈ sky（`oklab(0.685 -0.091 -0.142 / 0.6)`）、`data-layout=vertical`、`第 1/3 题` | 蓝卡、纵向两项、一次一题；无溢出 |
| 审批卡 `r6-approval-*` | 2 | `pendingKind=permission`、边框 ≈ rose（`oklab(0.645 0.236 0.070 / 0.6)`）、`pendingDetailVisible=true`、选项 2 个 | 红卡 + `Bash` + 命令正文；只有「允许一次／拒绝」，无完全访问项 |
| 官方队列 `r6-queue-*` | 3 | `paused/2`、`paused/0`（已消费）、`active/2`；展开条目 `#1/#2` | 只读列表；暂停不因清空消失；无撤回/越队/恢复按钮 |
| 长模型名 `r6-longmodel-*` | 1 | 文字宽 468px vs 可用 124px（canvas 量测）、`title` = 全名 | 框内省略、箭头完整、全名可取 |
| 极窄档 `r6-narrow-320-light-zh-125` | 1 | `viewportWidth=320`、`toolbarRows=1`、溢出 0 | 320px/125% 仍单行、三控件可见 |
| 附件问号 `r6-attachhelp-*` | 1 | `attachHelpExpanded=true`、零上传/零发送 | 展开既有存储说明（`.tuvis-attachments/<会话>/`） |

**汇总**：34/34 `strayInterruptRows=0`；34/34 三层无横向溢出；34/34 三个工具栏控件 `right ≤ innerWidth`；
主按钮分布 20 send / 11 stop / 3 disabled，与各场景的输入、附件、官方 `activeWorks` 一一对应。
**浏览器证据取代不了真机**（系统字号、软键盘顶起、触控命中区），§12 清单继续有效。

### 8.3 逐项视觉结论（`d0e8c894` 时点原判，逐字保留）

> 本节引用的文件名是 R4 时期的原名——对应文件现已按 §8.1 处置加 `r4-` 前缀（如
> `attachhelp-360-light-zh-100.png` → `r4-attachhelp-360-light-zh-100.png`）。

| 检查项 | 证据 | 结论 |
| --- | --- | --- |
| 问号（附件说明） | `attachhelp-360-light-zh-100.png` | ✅ 可见、按需展开、非长文常驻 |
| 单行工具栏 | `r4-metrics.json` 20/20 `toolbarRows=1`；肉眼 `toolbar-360-light-zh-100.png`、`narrow-320-light-zh-125.png` | ✅ 成立（含 320px/125%、长模型名） |
| 单一主按钮 | 无 HEAD 截图；`ZcodeControlPanel.tsx:789-808` 只有一处主按钮、`tests/...:866` 断言 `zcode-interrupt` 为 null | ⚠️ **代码成立、视觉待验**（问题 I-1）　**处置**：`r6-stop-*` 4 张 + 16 张矩阵已证（§8.2），该行结论仅对 `r4-*` 有效 |
| 蓝色逐题卡 | `toolbar-360-light-zh-100.png`（蓝色「等待回答」+「第 1/3 题」+纵向两项） | ✅ 成立（浏览器） |
| 队列展开区 | 只有折叠行截图（`排队 2 条 已暂停 已手动暂停`） | ⚠️ 展开列表**无渲染证据**（问题 I-2）；另该行同时出现「已暂停」与「已手动暂停」两段文案，信息重复（次要）　**处置**：`r6-queue-*` 3 张已补展开/已消费（§8.2）；文案重复属 M-2，未改 `src/` |
| 审批卡与命令正文 | `approval-360-light-zh-100.png` | ✅ 成立（红卡、允许一次/拒绝纵向、正文折叠展开） |

---

## 9. 偏差清单（逐条：影响 + 理由 + 裁决）

| # | 偏差 | 影响 | 理由／证据 | 裁决 |
| --- | --- | --- | --- | --- |
| D-1 | 运行中发送**恒**携带 `requestedDelivery="queue"`，不读宿主 `inputRouting.mode` | 桌面若配置为 `guide`（插话），手机会强制排队；两端对同一句话的语义不同 | `command.rs:662`、`:1468-1478`；R0 证据 `:180`（省略时跟随 mode）、`:203`（选项 3 的权衡）；R2 报告 `progress.md:48` 已登记「产品口径待实机对账」 | **保留但需产品裁决**：与用户 §1 U3「按官方支持的入队语义处理」一致，属用户裁决范围内；但「覆盖 guide」这一后果未被用户明示同意 → 需在实机对账时确认　**用户裁决（2026-10-31，原文）**：「**`requestedDelivery="queue"` 维持现状**：依据 §1 U3「运行中发送按官方支持的入队语义处理，不做 Tuvis 自建队列」与 §5 时序 3「官方队列出现」；宿主 `inputRouting.mode="guide"` 时的插话表现属产品口径差异，不属本轮修改范围，列入实机待验由用户现场裁决。」 |
| D-2 | 单题问答也走「下一题 → 确认 → 提交」两步 | 单题多一次点击 | `ZcodePendingCard.tsx:416-431`（`current < total-1 ? 下一题 : 确认`）；`progress.md:60` 已登记为裁决事项 | **可接受**（防误提交；与「一组多题确认后整体提交」一致）；真机手感待验 |
| D-3 | 附件提示由「点发送才闪」改为「上传中/失败常驻」 | 提示出现更早，属可感知变化 | `ZcodeControlPanel.tsx:322-328`；R5 报告 §7.4 | **可接受**（按钮已禁用，否则用户看不到原因）　**用户裁决（2026-10-31，原文）**：「**附件「上传中/失败」提示改为常驻**：属有意行为变更（按钮已禁用、点击不触发 onClick，瞬态提示用户看不到），已在 R5 复审认可。」 |
| D-4 | `detail` **不脱敏**（只读、2000 字截断、不落盘、不进日志） | 审批卡会显示命令原文（含潜在密钥） | `types.rs:734-743`、`progress.md:59` | **可接受**（不脱敏比假掩码更安全）；但需注意 2000 字截断可能截掉尾部参数，属实机观察项 |
| D-5 | ZCode 会话完全不挂旧 `QuestionCard` / `ApproveCard` | 若官方 pending 因协议问题缺失，用户看不到任何卡 | `SessionDetail.tsx:518-527`、`:1313-1320`；计划 R1 明确要求 | **按计划执行**（旧卡对 ZCode 无写通道，只会制造「可读不可点」的假象） |
| D-6 | R6 要求的用户手册更新缺失 | 用户可见行为变化无文档 | `git diff --stat cb79b7cb..81fb3e22 -- docs/` 无 `08-mobile-remote.md` | **偏差成立，需补**（问题 I-3）　**处置（2026-10-31）**：中英手册已补「手机控制区」小节（单一主按钮 / 常驻提示 / 紧凑工具栏 / 附件问号 / 待处理卡 / 官方队列只读），结构对等；该偏差关闭 |
| D-7 | R0/R1 无 `task-R0-report.md` / `task-R1-report.md`（以 `docs/superpowers/reviews/2026-10-29-zcode-r0-*.md` + `progress.md` 代替） | 可追溯性略降 | 目录列表；`progress.md:10-17` 说明 | **可接受**（信息完整，仅形态不同） |
| D-8 | `promoted_input_leaves_queue_without_duplicate_message` 的「被消费项不复活」由投影单测 + e2e 组合证明，未单独做一条 UI 级断言 | 覆盖面略窄 | `projection.rs` 用例 + `wire_e2e.rs:2454` 第 7 步 | **可接受**（UI 只是渲染投影，无独立状态） |

**越界检查（§7 要求必须阻塞的项）**：权限模式切换、放宽审批、常驻授权、全访问开放、重写连接器、重写 OAuth/钥匙串/relay/codec/幂等账本 —— **均未发生**。`fullAccess`/`allowAlways` 仍被 `pending.rs:399-413` 与 `ZcodePendingCard.tsx:236-238` 双重拒绝；`git diff --stat` 未见 `auth/relay/codec/credential` 文件改动。

---

## 10. 问题清单（先列问题，再给结论）

### 阻塞（2）

**B-1｜集成测试目标在 HEAD 无法编译（门禁 §6 #2/#3 红）**
- 定位：`src-tauri/tests/zcode_control_e2e.rs:989`、`:1018`、`:1259` 的 `PendingView { .. }` 缺 `detail` 字段（字段由 `9e8acbe1` 引入，`types.rs:721`）。
- 实测：`cargo test --locked --features marker-helper` → `error[E0063]: missing field 'detail'`，exit=101；`--tests --features marker-helper,zcode-bridge-evidence` 同样 exit=101。
- 影响：该目标承载「两台设备同 actionId 单次派发」「换链接旧 pending/旧动作零帧」「SSE lag 纠偏前零写」等守卫负例，本批**无法据此判定这些守卫未回归**；这批提交自 `9e8acbe1` 起即处于「指定门禁不可通过」状态。
- 修复：三个字面量各补 `detail: None,`（或给该字段 `#[serde(default)]` 并改用 `..Default::default()`）。
- **处置（2026-10-31，`3b7da564`）：已修。** 三处均补了 `detail`（问答/旧代际 = `None`，审批 = `Some("ls -la /tmp/tuvis-e2e-scratch")`），语义与原判建议一致；该提交的记录称 marker / evidence 两个测试目标 exit=0。本轮未重跑 cargo（重门禁由控制方执行），故本条记为「代码已修、待重门禁复验」。

**B-2｜`clippy -D warnings` 失败（门禁 §6 #4/#5 红）**
- 定位：`src-tauri/src/services/zcode_control/types.rs:743` —— 编号列表第 4 项后的续行 `/// 若后续要加掩码…` 未缩进（`clippy::doc_lazy_continuation`）。
- 实测：两条 clippy 命令均 exit=101。
- 修复：加空行或缩进 4 空格。
- **处置（2026-10-31，`3b7da564`）：已修。** 续行缩进 3 空格（`types.rs:743`），`clippy::doc_lazy_continuation` 消除；仅排版、语义不变。同上，待控制方重跑 clippy 两条命令复验。

**合同条件说明（非独立问题）｜「代码可合并」的门槛未满足**
- 计划 R6（`docs/superpowers/plans/2026-10-29-zcode-session-ui-repair.md:191`）明确：「仅阻塞清零时给出『代码可合并、实机待验』」。B-1/B-2 未清 → 结论只能是「不可合并」。

### 重要（5）

**I-1｜U3 的浏览器/窄屏证据在 HEAD 缺失**
- 定位：截图提交 `d0e8c894`（14:51）早于 `741e6005`（20:44）；截图内仍见「打断」独立行。
- 影响：单一主按钮在 360/390、中英、100/125% 下的换行与是否与输入框同行**无渲染证据**；R5 报告 §5 亦自认「本轮未做浏览器量测」。
- 要求：重跑 `preview/shoot.sh`（夹具直接 `import @/mobile/ZcodeControlPanel`，见 `preview/src/main.tsx:17`）并补 4 张（360/390 × 中英 × 打断位/发送位）。
- **处置（2026-10-31）：已修。** 夹具补 `state=running|idle`、`draft`、`upload=hang|fail`、`queue=consumed|active`、`expand` 等参数，在最终 HEAD 上重出 **34 张 `r6-*`**（打断位 4 张 + 矩阵 16 张，超出原要求的 4 张）；量测新增 `primaryAction` / `strayInterruptRows` 等字段，34/34 无独立打断行、无横向溢出（§8.2）。

**I-2｜U6「展开后的队列列表」无渲染证据**
- 定位：`ZcodeQueuePanel.tsx:88-109`（展开列表）仅有单测；截图只见折叠行。
- 影响：逐项文本换行（`break-words whitespace-pre-wrap`）与长正文在中英/窄屏下的表现未验。
- **处置（2026-10-31）：已修。** 补 `r6-queue-paused-expanded-360-light-zh-100`、`r6-queue-consumed-360-light-zh-100`、`r6-queue-active-expanded-390-dark-en-125` 三张：展开列出 `#1/#2` 正文，125% 深色英文下折行不溢出；已消费态保留暂停标志。

**I-3｜计划 R6 要求的用户手册更新缺失**
- 定位：`docs/user-manual/{zh,en}/08-mobile-remote.md` 本批未改；正文 `:170` 仍是「复用…打断与模型 / 思考强度控件」的笼统表述，未写「运行中空输入时主按钮变为打断」「官方队列展示」「附件问号」。
- 影响：单一主按钮是**用户肌肉记忆变更**（R5 报告 §5.1 自认），无文档＝用户按旧手册找不到原「打断」按钮。
- **处置（2026-10-31）：已修。** `docs/user-manual/{zh,en}/08-mobile-remote.md` 在「ZCode 统一控制」下新增 `#### 手机控制区：一个主按钮、一行工具栏、一张卡一件事`（中英各 6 条，结构对等）：单一主按钮的三种态与「有内容不先打断」、上传中/失败附件锁发送 + 常驻原因、紧凑工具栏与长模型名可访问全名、附件「？」展开既有存储说明、待处理卡（蓝/红、纵向、一次一题、整组一次提交）、官方队列只读（含暂停/已消费、无撤回/越队/恢复）。同时把原文「打断与模型 / 思考强度控件」改为「打断已并入发送主按钮」。

**I-4｜运行中发送强制 `queue` 覆盖宿主 `guide` 插话语义，产品口径未裁决**
- 定位：`command.rs:662` + `:1468-1478`。
- 影响：桌面配置为「插话」的用户，从手机发消息会变成排队；`progress.md:48` 已登记但未闭环。
- 说明：不回滚代码，但需产品明确「手机是否应尊重桌面 mode」，或至少在手册写明「手机运行中发送＝排队」。
- **处置（2026-10-31）：维持现状（用户裁决，见 §9 D-1）。** 裁决要点：依据 §1 U3 与 §5 时序 3，运行中发送按官方入队语义处理；`inputRouting.mode="guide"` 的插话差异属产品口径差异，不属本轮修改范围，列入实机待验由用户现场裁决。手册侧已写明「运行中发出的消息进入官方自己的输入队列」。

**I-5｜`pnpm test` 在最终门禁中为红（3 条 5s 超时；已定位为负载敏感，仍需一次干净的全量绿）**
- 定位：`tests/lib/displayChineseScan.test.ts:60`、`tests/mobile/InteractiveCard.test.tsx:75`、`tests/pet/usage-locale-scan.test.ts`（均 `Test timed out in 5000ms`）。
- 复跑记录：三条单独跑 1.6s / 3.4s / 1.9s 全绿；`InteractiveCard` 在 R5 报告中已有基线 worktree 同因失败对照（`task-R5-report.md:105`）。
- 说明：我在门禁运行期间并行跑了 4 个 mobile 测试文件，与全量套件争抢 CPU，**很可能是当次超时的诱因**；但 3.4s/5s 的余量本身偏薄，需在无人干扰下补一次全量绿，才能写「门禁通过」。

### 次要（6）

- **M-1** 测试注释与实现不一致：`tests/mobile/ZcodeControlPanel.test.tsx:2483` 写「模型 select `min-w-0`/`truncate`」、`describe` 注释写 `flex-nowrap`，实际是 `flex-wrap` + `min-w-[4rem]`（`ZcodeControlPanel.tsx:630`、`:656`）。测试本身断言的是后者（`tests/mobile/ZcodeControlPanel.test.tsx:2626-2629`），因此**不是假绿**，但注释会误导后来者。
- **M-2** 队列行文案冗余：`ZcodeQueuePanel.tsx:74-81` 在 `pauseReason="manual"` 时同时渲染「已暂停」与「已手动暂停」（截图 `toolbar-360-light-zh-100.png` 可见）。
- **M-3** 旧 Codex 卡存在同构缺陷未修：`src/mobile/PendingRequestCard.tsx:316-327` 与本次修复前的 `ZcodePendingCard` 同形（effect 无条件 `setPicked({})`），存在相同的「首帧点击被吞」窗口；`task-mountreset-fix.md:166` 已记录、超出本批允许改动范围。
- **M-4** 换卡瞬间的同类窗口仍在：`task-mountreset-fix.md:163` 自认；需要「按身份键存储暂存 state」的更大改动。
- **M-5** `pauseReason="manual"` 的写入点在 R0 记为低置信度（`r0-wire-evidence.md:137`），而 UI 已为它配文案；不影响正确性（值域内透传），但「manual 是否真会出现」未证实。
- **M-6** `pnpm check` 的最后一步 `pnpm build` 被仓库 safe-delete 闸拦下：`[SAFE_DELETE][SAFE_DELETE_BULK_CONFIRM_REQUIRED] {"count":90,"threshold":50,"targets":["dist/pet"]}`。`dist/` 属 `.gitignore:5` 忽略的本机构建产物，`pnpm build:mobile` 同批 exit=0，非本批代码缺陷；但清掉 `dist/pet` 前 `pnpm check` 无法整体绿。

---

## 11. 结论

**代码是否可合并：不可合并。**
理由：§6 门禁 10 项中 6 项红——其中 4 项由本批代码直接导致（`cargo test --features marker-helper`、`--tests --features …` 编译失败；两条 `clippy -D warnings`），另外 2 项是 `pnpm test` 的 3 条 5s 超时（已定位、已单独复跑为绿）与 `pnpm check` 的 safe-delete 闸（构建产物，非代码）。B-1/B-2 是机械性修复（补 3 处字段 + 1 行文档缩进），但**在清零并复跑门禁前，本批不能给「可合并」**；且 B-1 使目标/代际/幂等的 e2e 负例当前不可运行。

**产品／实机是否完成：未完成。**
理由：八项能力无一项在本批取得新真机证据；U3 连浏览器渲染证据都没有（I-1）；U6 展开区无渲染证据（I-2）；用户手册未更新（I-3）；运行中投递语义与桌面可能不一致且未经裁决（I-4）。核心展示与状态链**已接通**（不属于「代码缺口」），因此剩余项是「证据缺口 + 文档缺口」，但按 §7「不得把未跑写通过」，只能记「实机待验」。

### 11.1 收口后结论（2026-10-31，HEAD `3b7da564`，仍按 §7 双轨）

上面两段是 `81fb3e22` 时点的原判，**不改写**。按修复后状态重述如下：

**代码可否合并：阻塞已清零，条件性可合并（待重门禁复跑）。**
- 阻塞 B-1 / B-2 已由 `3b7da564` 修复（补 3 处 `detail` 字面量 + 1 行 doc 续行缩进）；该提交记录
  `cargo test --features marker-helper`、`--tests --features marker-helper,zcode-bridge-evidence`、
  两条 `clippy -D warnings`、`cargo fmt --check` 均通过。
- **本轮的诚实边界**：只跑了轻量文档门禁（`git diff --check` exit=0、`check-i18n` 2597 键通过、
  `prettier --check` 为既有基线红且本轮零新增格式差异，见 §2.5），**没有重跑 cargo / pnpm 重项**——
  重项由控制方执行。因此准确的表述是：**代码侧阻塞已清零，重门禁复跑为绿即可合并**，
  而不是「本轮已验证可合并」。
- 原 I-5（3 条 5s 超时）与 M-6（safe-delete 闸）未变，均已有定位与复跑记录，不构成代码缺口。

**产品／实机是否完成：仍未完成。**
- 已补齐：U3 的最终 HEAD 渲染证据（I-1）、U6 展开区与已消费态证据（I-2）、中英用户手册（I-3）；
  运行中投递语义按用户裁决维持并落档（I-4 → §9 D-1）。
- 仍未完成：八项能力**没有一项**在本批取得新的真机执行证据；问答/审批/打断/队列跨端/窄屏系统字号
  全部仍待实机（§12 八条清单未变）。
- 因此**不得**因局部修复改口成「全部完成」：浏览器证据只能证明「排版与按钮态」，不能证明
  「官方宿主真的执行了该动作」。

---

## 12. 实机待验清单（移交）

1. 空闲页 → 桌面发消息 → 工作中出现新问答卡/审批卡 → 手机**不刷新**可答、可批（对应 `hub.rs:6583/6653` 的现场版）。
2. 一组 3 题：分题作答 → 返回改答 → 确认页 → 提交后宿主撤卡；确认全程无 CLI answer。
3. 运行中带文字/附件发送 → 桌面队列出现 → 暂停 → 手机刷新仍显示暂停 → 桌面消费 → 手机收敛；确认没有「手机自建队列」迹象。
4. 运行中空输入点主按钮（应为「打断」）→ 桌面回合停止；再验证恰好一次、无自动重发。
5. 360px 真机（中/英、深/浅、系统字号 100%/125%）：单行工具栏不遮挡正文、主按钮文案「发送/打断」切换不换行压字。
6. 真机点「？」：展开存储说明、不弹文件选择器。
7. 冲突态：打开官方页 → 手机出现「断开并抢回」→ 点一次恢复；仍冲突时只提示不循环。
8. 附件：真机上传图片/文件，确认 `.tuvis-attachments/` 落盘与正文引用序号对齐。
9. **插话语义现场裁决（用户裁决 D-1 的待验项）**：把桌面端 ZCode 配置成插话（`inputRouting.mode="guide"`），
   从手机在运行中发一条消息，确认官方实际是「排队」还是「插话」；据此由用户现场决定是否接受手机侧
   恒为 `queue`。本轮不改代码。

---

## 13. 证据索引

- 计划：`docs/superpowers/plans/2026-10-29-zcode-session-ui-repair.md`（§1/§5/§6/§7）
- 设计：`docs/superpowers/specs/2026-10-24-zcode-unified-remote-design.md` §5
- 能力 spec：`docs/superpowers/specs/2026-10-28-zcode-capability-parity-spec.md` R1—R6
- R0 取证：`docs/superpowers/reviews/2026-10-29-zcode-r0-wire-evidence.md`
- R0/R1 复审：`docs/superpowers/reviews/2026-10-29-zcode-r0-r1-review.md`
- 任务报告：`.superpowers/sdd/2026-10-29-zcode-session-ui-repair/task-R{2,3,4,5}-report.md`、`task-R{3,4,5}-review.md`、`task-R3-rereview{,2,3}.md`、`task-mountreset-fix.md`、`progress.md`
- 视觉：`docs/superpowers/reviews/2026-10-29-zcode-ui-screenshots/`（**R6 现行证据：34 PNG `r6-*` + `metrics.json`**；R4 历史：20 PNG `r4-*` + `r4-metrics.json`；说明见该目录 `README.md`）
- 用户手册：`docs/user-manual/zh/08-mobile-remote.md`、`docs/user-manual/en/08-mobile-remote.md`（§「手机控制区」/「The mobile control area」）
- 门禁：`/tmp/r6-gate.log`（`81fb3e22` 时点全量）、`/tmp/r6-gate-summary.txt`；`3b7da564` 的修复与门禁记录见该提交信息；本轮轻量项结果见 §2.5
- 差异：`.superpowers/sdd/2026-10-29-zcode-session-ui-repair/review-cb79b7cb..81fb3e22.diff`
