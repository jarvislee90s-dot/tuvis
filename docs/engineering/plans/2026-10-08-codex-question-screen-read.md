# codex 问答远程作答与屏读补齐 · 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 按已终审设计说明书（`docs/engineering/specs/2026-10-08-codex-question-screen-read-design.md` v2.1）一批补齐 codex 问答六机制：解析器、GET 快照、回执快照、切题、提交闭环、探针，交付四场景×三层测试。

**Architecture:** 解析器与阶段机为纯函数内核（`inject/` 层，假终端可测）；`remote/api.rs` 只做接线（StagePlan 路由 + dispatch 臂 + GET 门 + 旗标）；锚词走 `anchor_ledger` append-only 账本；回执新增 codex 专属 dispatch 变体（不迫使 claude 路径换容器）。取证先行（Task 1 自建会话活体采集，四闸门纪律）。

**Tech Stack:** Rust（既有 Tauri 后端，无新依赖——题号头手写解析，不引 regex）+ React/TS（前端对位分支）。

**证据基线:** codex-cli 0.160.0；用户实测 12 项定案 + 截图转写 + 官方源码印证（设计说明书 §2/§6）。

**分支纪律:** 当前 worktree 与用户其他会话共享。Task 0 从 `origin/main` 建新分支并把 spec 带过去；全程只提交本计划列出的文件。

---

## 文件结构总览

| 文件 | 职责 | 动作 |
|---|---|---|
| `src-tauri/src/inject/question_screen_oc.rs` | codex 面板快照解析器（kimi 段同居先例） | 新增段 |
| `src-tauri/src/inject/anchor_ledger.rs` | 问答 footer 三槽位（submit answer/all、navigate） | 新增行+slot 常量 |
| `src-tauri/src/inject/question.rs` | 活体夹具（codex 段）+ CodexSelect/CodexAdvance 阶段机 + 永久探针 | 新增 |
| `src-tauri/src/remote/api.rs` | StagePlan 两变体 + for_action + dispatch 臂 + 序列闸 + GET 门 + advance/navBoth 旗标 + wire 合成 | 修改 |
| `src/mobile/api.ts` | 载荷 screen 类型扩展（codex 形状）+ navBoth 注释 | 修改 |
| `src/mobile/QuestionCard.tsx` | codex 题号对位分支 + unanswered 进度提示 | 修改 |
| `docs/research/refs/phase2-消息注入/2026-10-08-codex-160-question-屏读底料.md` | 取证批定案表 + 证据索引 | 新建 |
| `.agents/skills/win-console-inject-probe/references/screen-read-matrix.md` | codex 0.160.0 形态行 | 追加 |

---

### Task 0: 分支与基线

**Files:** 无代码改动。

- [ ] **Step 1: 建分支并带 spec**

```bash
cd E:\LLMproject\Github\mam-worktree2
git fetch origin main
git checkout -b feat/codex-question-screen origin/main
git checkout fix/claude-auq-single-question-tab -- docs/engineering/specs/2026-10-08-codex-question-screen-read-design.md
git commit -m "docs(specs): codex 问答屏读设计说明书 v2.1（随实施分支携带）"
```

- [ ] **Step 2: 基线确认**

Run: `git status --short && git log --oneline -2`
Expected: 干净 + 新分支头为 spec 携带提交。

---

### Task 1: 取证批（四闸门①——活体采集 S1–S4 + A–H）

**Files:**
- Create: `docs/research/refs/phase2-消息注入/2026-10-08-codex-160-question-屏读底料.md`
- Create（证据）: `%USERPROFILE%\mam-probe-m6r\evidence\codex-q-<runid>-*.txt`

**红线**：只读探针零注入做 dump；键注入只对自建会话；Esc 单按+屏读确认；结束 Stop-Process 清场。

- [ ] **Step 1: 起自建会话并进 Plan 模式**

```bash
cd .agents/skills/win-console-inject-probe/scripts
powershell -NoProfile -ExecutionPolicy Bypass -File launch-session.ps1 -HostKind conhost -Cli codex -ProjDir 'C:\Users\bunny\AppData\Local\Temp\probe-codex-q-20261008'
codex --version   # 记录，预期 0.160.0
```

记 HOST/CMD/TARGET_PID。等 15s 后 dump 基线：`cd src-tauri && MAM_PROBE_PID=<pid> cargo test --lib kimi_screen_live_dump -- --ignored --nocapture`（该探针工具无关，只 dump）。处理首启信任框（`2` 数字或 ↓+Enter，见 dump 定）。

- [ ] **Step 2: 进 Plan 模式**

```bash
powershell -NoProfile -ExecutionPolicy Bypass -File probe-type.ps1 -TargetPid <pid> -Text "/plan"
powershell -NoProfile -ExecutionPolicy Bypass -File probe-key2.ps1 -TargetPid <pid> -VtName enter
```

dump 确认底栏出现 Plan 字样。

- [ ] **Step 3: S1 两题采集**（ASCII 提示词，避免 CJK 注入边界）

```bash
powershell ... probe-type.ps1 -TargetPid <pid> -Text "Use the request_user_input tool now: ask me exactly 2 questions. Q1 single-select 'Which DB?' with options Postgres, SQLite. Q2 single-select 'Which cache?' with options Redis, Memcached, None needed. Do nothing else."
powershell ... probe-key2.ps1 -TargetPid <pid> -VtName enter
```

等 ~25s 轮询 dump 至面板出现，落证据 `codex-q-<runid>-s1-q1.txt`。**A–H 同场实验**（每步后 dump 落档）：
- **A/#1 推进语义**：发数字 `1`（probe-key2 `-Char 1` 或 `-VtName`——用字符形态 `probe-key2.ps1 -TargetPid <pid> -Char '1'`）→ dump：题号头变 `Question 2/2`？计数变 1？
- **#5 已答改答**：发 `←`（`-VtName left`）→ dump（焦点落已选项？）→ 走位 `↓` → 空格（`-Char ' '`）→ dump（计数 0→1？=反选实证）→ 再空格选回。
- **#2 归零词形**：切回 Q2 走位+空格选中 → dump：题号头是否只剩 `Question 2/2`（计数段消失）。
- **末题提交**：确认归零形态后发回车 → dump：已提交摘要屏（`Questions 2/2 answered` + answer 行）逐字落档（H 汇总半项）。
- **#3 环绕**：新弹一轮（重复提示词改 3 题 = S2），在选项区发 `↓`×N 至末项再一发 → dump 看是否回首项；`↑` 从首项再发 → dump 看是否到末项。
- **#11/H 确认屏**：S2 三题只答 2 题，末题发回车 → dump `Submit with unanswered questions?` 逐字 → 选 Go back（esc 单按）→ dump 回题屏 → 补答归零 → 回车提交。
- **S3 单题**：提示词改 1 题 → dump `Question 1/1 (1 unanswered)` + **footer 是否仍含 navigate 段**（P1-3 词形）→ `←`/`→` 各一发 dump（无效实证）→ 答题提交。
- **S4/notes 链**：提示词 1 题 + 要求 None of the above 场景 → 走位 Other → **tab 直接进输入**（不先空格——验证全局键）→ dump 输入态 → 打字（probe-type ASCII）→ **回车** → dump：单题连带交卷？（A 项定案）→ 若已交卷 dump 摘要；若未交卷继续归零+回车。再开一轮验证 tab 前置：焦点停在非 Other 行直接 tab → dump（是否自动选中焦点行 = ensure 实证）。
- **F/#9 瞬态**：新弹一轮，出题瞬间（≤300ms 内）立即 dump 一次：高亮行首是 `?` 还是 `›`。
- **G/滚回残留**：一轮答完提交后再弹新一轮 → dump：旧面板/旧题号头是否残留在滚回区。
- **E/折行**：提示词要求一个 60+ 字符的长 label 选项 → dump 描述列折行形态。

- [ ] **Step 4: 清场**

```bash
powershell -NoProfile -Command "Stop-Process -Id <host>,<cmd>,<target> -Force"
```

- [ ] **Step 5: 写底料文档并提交**（定案表逐项对照设计 §6 A–H，结论不超证据；证据索引列全部 dump 文件名）

```bash
git add docs/research/refs/phase2-消息注入/2026-10-08-codex-160-question-屏读底料.md
git commit -m "research(codex): 0.160.0 问答屏读底料取证——S1-S4 + A-H 定案（四闸门①）"
```

**门**：若任一实验结果推翻设计 §2.3 定案（如数字不推进），停下回报用户裁决后再继续——规格不超证据。

---

### Task 2: 活体夹具入档

**Files:**
- Modify: `src-tauri/src/inject/question.rs`（`live_fixtures` 模块新增 codex 段）

- [ ] **Step 1: 生成器脚本产出字面量**（复用 claude 修复同款模式——逐字保真，禁手抄）

写 `C:\Users\bunny\AppData\Local\Temp\gen_codex_fixture.py`：读 Step 1 证据文件中 `FIXTURE|` 行，转义 `\` 与 `"`，输出 Rust 字面量行（与 2026-10-08 claude 修复的 gen_fixture.py 同构，改 SRC/DST 即可）。对 S1-Q1 / S1-Q2 / S2-Q1 / S3 单题 / S4 输入态 / 确认屏 / 已提交摘要 各产一份。

- [ ] **Step 2: 入档**（`live_fixtures` 模块追加，命名与注释带证据文件名）

```rust
    // ===== codex 0.160.0 活体夹具（2026-10-08 取证批，证据见
    // docs/research/refs/phase2-消息注入/2026-10-08-codex-160-question-屏读底料.md）=====
    pub(crate) fn codex_s1_q1() -> Vec<String> { lines(&[ /* 生成器产出 */ ]) }
    pub(crate) fn codex_s1_q1_answered() -> Vec<String> { /* Q1 已答形态：计数 1、焦点落已选项（#5 实验）*/ }
    pub(crate) fn codex_s1_q2_zero() -> Vec<String> { lines(&[ /* 生成器产出：归零词形 */ ]) }
    pub(crate) fn codex_s3_single() -> Vec<String> { lines(&[ /* 生成器产出 */ ]) }
    pub(crate) fn codex_s3_focused_other() -> Vec<String> { /* 焦点在 Other 行的派生/取证屏 */ }
    pub(crate) fn codex_s4_notes_open() -> Vec<String> { lines(&[ /* 生成器产出：输入态 */ ]) }
    pub(crate) fn codex_confirm_unanswered() -> Vec<String> { lines(&[ /* 生成器产出 */ ]) }
    pub(crate) fn codex_answered_summary() -> Vec<String> { lines(&[ /* 生成器产出 */ ]) }
    pub(crate) fn codex_scrollback_residual() -> Vec<String> { /* G 实验产出：残留 + 新面板 */ }
```

- [ ] **Step 3: 编译验证 + 提交**

Run: `cd src-tauri && cargo test --lib live_fixtures 2>&1 | tail -3`（或 `cargo check --lib`）
Expected: 编译通过。
Commit: `feat(codex): 活体夹具入档（0.160.0 四场景+确认屏+摘要+残留）`

---

### Task 3: anchor_ledger 问答 footer 槽位（TDD）

**Files:**
- Modify: `src-tauri/src/inject/anchor_ledger.rs`

- [ ] **Step 1: 写失败测试**（追加到该文件 tests 模块，仿 `question_anchors_ledger_matches_pre_collection_constants`）

```rust
    /// codex 问答 footer 三槽位（2026-10-08 设计 §6.3）：navigate 在场锚 +
    /// submit answer/all 末题双验锚。append-only，evidence 指向取证批 dump。
    #[test]
    fn codex_question_footer_slots_match_live_dump() {
        let lines: Vec<String> = ["  tab to add notes | enter to submit answer | ←/→ to navigate questions | esc to interrupt"]
            .iter().map(|s| s.to_lowercase()).collect();
        assert!(detect(&lines, "codex", scenario::QUESTION, slot::Q_FOOTER_NAVIGATE).is_some());
        assert!(detect(&lines, "codex", scenario::QUESTION, slot::Q_FOOTER_SUBMIT_ANSWER).is_some());
        assert!(detect(&lines, "codex", scenario::QUESTION, slot::Q_FOOTER_SUBMIT_ALL).is_none());
        let all: Vec<String> = ["  tab to add notes | enter to submit all | ←/→ to navigate questions | esc to interrupt"]
            .iter().map(|s| s.to_lowercase()).collect();
        assert!(detect(&all, "codex", scenario::QUESTION, slot::Q_FOOTER_SUBMIT_ALL).is_some());
        assert!(detect(&all, "codex", scenario::QUESTION, slot::Q_FOOTER_SUBMIT_ANSWER).is_none());
    }
```

- [ ] **Step 2: 跑红**：`cargo test --lib codex_question_footer_slots -- --nocapture` → Expected FAIL（常量不存在）。

- [ ] **Step 3: 实现**——`slot` 模块追加三个常量；`ANCHOR_LEDGER` 追加三行（文案逐字取自取证 dump，`evidence` 字段填 dump 文件名，`version` 填 "0.160.0"）：

```rust
    /// codex 问答面板 footer 的 navigate 段（在场锚，last-pair-wins 配对用）
    pub const Q_FOOTER_NAVIGATE: &str = "q_footer_navigate";
    /// codex 问答 footer 的 submit answer 段（非末题）
    pub const Q_FOOTER_SUBMIT_ANSWER: &str = "q_footer_submit_answer";
    /// codex 问答 footer 的 submit all 段（末题双验）
    pub const Q_FOOTER_SUBMIT_ALL: &str = "q_footer_submit_all";
```

```rust
    AnchorRow {
        tool: "codex",
        scenario: scenario::QUESTION,
        slot: slot::Q_FOOTER_NAVIGATE,
        text: "←/→ to navigate questions",
        observed_version: "0.160.0",           // ← 字段名 observed_version（&'static str，非 Option）
        evidence: "codex-q-<runid>-s1-q1.txt", // ← 取证批实际文件名
    },
    // Q_FOOTER_SUBMIT_ANSWER: "enter to submit answer" / Q_FOOTER_SUBMIT_ALL: "enter to submit all" 同构两行
    // **复用（不新增）**：codex 终态回执行已在账本——(codex, QUESTION, RECEIPT,
    // "answered", 0.155.1, 戊探C)，contains 语义天然匹配 `Questions 3/3 answered`。
    // **确认屏点名（设计 §3.6）**：追加一行 (codex, QUESTION_REVIEW, TITLE,
    // "submit with unanswered questions?")——阶段机中止路径据此把「解析不出面板」
    // 细分为「屏上是未答完确认屏」专属文案（MAM 撞见 = 前序动作已错位，不代答）。
```

- [ ] **Step 4: 跑绿 + 提交**：`cargo test --lib anchor_ledger` PASS。
Commit: `feat(codex): 问答 footer 三槽位入账本（append-only）`

---

### Task 4: 解析器 `codex_question_screen_snapshot`（TDD）

**Files:**
- Modify: `src-tauri/src/inject/question_screen_oc.rs`（文件头部 pub use 区随需要导出）
- Test: 同文件 tests 模块

- [ ] **Step 1: 写失败测试**（夹具来自 Task 2；断言按设计 §3.1 契约）

```rust
    mod codex_snapshot_tests {
        use super::*;

        #[test]
        fn parses_s1_q1_fixture() {
            let snap = codex_question_screen_snapshot(&crate::inject::question::live_fixtures::codex_s1_q1());
            let s = snap.expect("S1-Q1 面板必解析");
            assert_eq!(s.question_idx, 0);
            assert_eq!(s.question_total, 2);
            assert_eq!(s.unanswered, 2);
            assert!(!s.is_last);
            assert_eq!(s.focused, Some(0));
            assert_eq!(s.options, vec!["Postgres", "SQLite", "None of the above"]); // (Recommended) 剥除、描述列剥除
            assert!(s.heading.contains("Which DB?"));
        }

        #[test]
        fn parses_zero_counter_wording() { // #2 定案：计数段消失 = 全答完
            let s = codex_question_screen_snapshot(&live_fixtures::codex_s1_q2_zero()).unwrap();
            assert_eq!(s.unanswered, 0);
            assert!(s.is_last);
        }

        #[test]
        fn last_pair_wins_on_scrollback_residual() { // G 残留：取最后一对
            let s = codex_question_screen_snapshot(&live_fixtures::codex_scrollback_residual()).unwrap();
            // 断言落在**新**面板（按新面板题号/题干——依取证批实际值填）
        }

        #[test]
        fn answered_summary_is_not_panel() { // 摘要屏 ≠ 面板（Question s 前缀天然不匹配）
            assert!(codex_question_screen_snapshot(&live_fixtures::codex_answered_summary()).is_none());
        }

        #[test]
        fn confirm_screen_is_not_panel() {
            assert!(codex_question_screen_snapshot(&live_fixtures::codex_confirm_unanswered()).is_none());
        }

        #[test]
        fn transient_marker_row_is_focused() { // #9：`? ` 前缀同认
            // 用 codex_s1_q1 派生：把 › 行换成 ? 前缀，focused 仍 Some(0)
        }
    }
```

- [ ] **Step 2: 跑红**（函数不存在，编译错即红）。

- [ ] **Step 3: 实现**（追加到文件尾；`strip` 等既有工具函数复用 kimi 段的）：

```rust
// ===== codex request_user_input 面板快照（0.160.0，设计说明书 §2.2/§3.1）=====

/// codex 问答面板快照（wire camelCase）。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexQuestionSnapshot {
    pub question_idx: usize,   // 0 起
    pub question_total: usize,
    pub unanswered: usize,     // 计数段消失 = 0（定案 #2）
    pub is_last: bool,
    pub heading: String,
    pub options: Vec<String>,
    pub focused: Option<usize>, // › / ? 高亮行（0 起）
}

/// `Question 1/2 (2 unanswered)` | `Question 2/2` → (i, n, Some(k) | None)。
/// None = 计数段消失（全答完）。`Questions 3/3 answered` 摘要头天然不匹配。
fn parse_codex_question_header(line: &str) -> Option<(usize, usize, Option<usize>)> {
    let t = line.trim();
    let rest = t.strip_prefix("Question ")?;
    let (ratio, tail) = match rest.split_once(' ') {
        Some((a, b)) => (a, Some(b)),
        None => (rest, None),
    };
    let (i, n) = ratio.split_once('/')?;
    let i: usize = i.parse().ok()?;
    let n: usize = n.parse().ok()?;
    if i == 0 || n == 0 || i > n {
        return None;
    }
    let k = tail.and_then(|t| {
        t.trim_start()
            .strip_prefix('(')
            .and_then(|t| t.split_once(' '))
            .and_then(|(num, _)| num.parse::<usize>().ok())
    });
    Some((i, n, k))
}

/// 选项行：`{空白}(› |? )?N. label{双空格描述列}`。返回 (N, label, focused)。
/// label 剥 ` (Recommended)` 尾缀与描述列（双空格截断，权限菜单 picker 同款）。
fn parse_codex_option_row(line: &str) -> Option<(usize, String, bool)> {
    let t = line.trim_start();
    let focused = t.starts_with('›') || t.starts_with('?');
    let t = if focused { t[1..].trim_start() } else { t };
    let rest = t.strip_prefix(|c: char| c.is_ascii_digit())?; // 单数字编号
    let rest = rest.strip_prefix('.')?.strip_prefix(' ')?;
    let mut label = rest.split("  ").next().unwrap_or(rest).trim_end(); // 双空格截描述列
    if let Some(stripped) = label.strip_suffix("(Recommended)") {
        label = stripped.trim_end();
    }
    let num = t.split('.').next()?.parse::<usize>().ok()?;
    if label.is_empty() { return None; }
    Some((num, label.to_string(), focused))
}

/// last-pair-wins：最后一个题号头 + 其下 20 行窗内 footer navigate 锚成对在场。
pub fn codex_question_screen_snapshot(lines: &[String]) -> Option<CodexQuestionSnapshot> {
    let lowered: Vec<String> = lines.iter().map(|l| l.to_lowercase()).collect();
    let mut header_idx = None;
    let mut parsed_hdr = None;
    for (i, l) in lines.iter().enumerate().rev() {
        if let Some(h) = parse_codex_question_header(l) {
            // 下方 20 行窗内须有 navigate footer（成对在场；lowered 传账本）
            let win = &lowered[(i + 1).min(lines.len())..(i + 21).min(lines.len())];
            if crate::inject::anchor_ledger::detect(
                win, "codex",
                crate::inject::anchor_ledger::scenario::QUESTION,
                crate::inject::anchor_ledger::slot::Q_FOOTER_NAVIGATE,
            ).is_some() {
                header_idx = Some(i);
                parsed_hdr = Some(h);
                break;
            }
        }
    }
    let (i, n, k) = parsed_hdr?;
    let hdr_idx = header_idx?;
    let mut heading = String::new();
    let mut options: Vec<String> = Vec::new();
    let mut focused: Option<usize> = None;
    let mut expect_next = 1usize;
    for l in &lines[hdr_idx + 1..] {
        if let Some((num, label, foc)) = parse_codex_option_row(l) {
            if num != expect_next { break; } // 编号跳变：面板块终点
            if foc { focused = Some(options.len()); }
            options.push(label);
            expect_next += 1;
        } else if options.is_empty() {
            let t = l.trim();
            if !t.is_empty() && !t.to_lowercase().contains("unanswered") {
                heading.extend(t.chars().filter(|c| !c.is_whitespace()));
            }
        } else if l.trim().is_empty() {
            continue; // 选项后空行（footer 前垫行）
        } else {
            break; // footer 或其他行：块终点
        }
    }
    if options.is_empty() { return None; }
    let is_last = i == n;
    Some(CodexQuestionSnapshot {
        question_idx: i - 1,
        question_total: n,
        unanswered: k.unwrap_or(0),
        is_last,
        heading,
        options,
        focused,
    })
}
```

（实现以取证批实际屏面微调——如行内仍有 `unanswered` 判空需排除题号头重复行；测试全绿为准。）

- [ ] **Step 4: 跑绿 + 提交**：`cargo test --lib codex_snapshot_tests` PASS。
Commit: `feat(codex): 面板快照解析器（last-pair-wins + 账本 footer 锚）`

---

### Task 5: `run_codex_select_stages` 阶段机（TDD）

**Files:**
- Modify: `src-tauri/src/inject/question.rs`
- Test: 同文件 tests（用 `mode::Closures` 假终端，仿 `run_select_script` 驱动器）

- [ ] **Step 1: 写驱动器 + 失败测试**（覆盖：非末题主路径 / 键丢失**重发数字**兜底 / 改答 reanswered 回执 / 末题双向走位每步复核 / 已答特判跳空格 / 归零闸双验 / 双条件闸-已交卷分支 / 身份闸中止 / 无焦点中止 / 确认屏点名中止）

```rust
    fn run_codex_select_script(
        q_idx: usize, index: usize, screens: Vec<Vec<String>>,
    ) -> (Result<crate::inject::question::CodexSelectOutcome, StageAbort>, Vec<String>) {
        // 与 run_select_script 同构：Cell 游标 + send 推进屏 + sent 记录；
        // poll 闭包返回 Result<Option<Vec<String>>, String>（轮询窗语义）
    }

    #[test]
    fn codex_select_nonfinal_digit_auto_advances() { // 主路径：数字后屏自切下一题
        let (r, sent) = run_codex_select_script(0, 0, vec![codex_s1_q1(), codex_s1_q2()]);
        assert_eq!(r.unwrap().sent_keys, vec!["1"]);  // 定案：数字=选中+自动推进，零补键
        assert_eq!(sent, vec!["1"]);
    }

    #[test]
    fn codex_select_nonfinal_digit_retry_on_lost_key() { // 键丢失：窗尽屏不动 → **重发数字**（不得补 →——会跳过未答，设计 §3.2）
        let (r, sent) = run_codex_select_script(0, 0, vec![codex_s1_q1(), codex_s1_q1(), codex_s1_q2()]);
        assert_eq!(r.unwrap().sent_keys, vec!["1", "1"]);
    }

    #[test]
    fn codex_select_reanswered_flag_when_counter_unchanged() { // 改答：题号推进而计数不变（进闸前已答）
        let (r, _) = run_codex_select_script(0, 0, vec![codex_s1_q1_answered(), codex_s1_q2()]);
        assert!(r.unwrap().reanswered);
    }

    #[test]
    fn codex_select_final_walks_stepwise_then_space_zero_gate_enter() {
        // 末题：焦点 0 → 目标 1：down+复读 → space → 归零屏 → enter
        let (r, sent) = run_codex_select_script(1, 1, vec![codex_s1_q2_focused0(), codex_s1_q2_focused1(), codex_s1_q2_zero(), codex_answered_summary()]);
        assert_eq!(r.unwrap().sent_keys, vec!["down", "space", "enter"]);
    }

    #[test]
    fn codex_select_final_walk_uses_shortest_wrap_direction() { // 目标在焦点上方：up 短于 down 环绕
        // 3 选项、焦点 2、目标 0 → up×2（down 需 1？按 (t+m-f)%m 与 (f+m-t)%m 取短者构造屏）
    }

    #[test]
    fn codex_select_final_already_answered_skips_space() { // 已答特判：入场归零且焦点即目标
        let (r, sent) = run_codex_select_script(1, 0, vec![codex_s1_q2_zero_focused0(), codex_answered_summary()]);
        assert_eq!(r.unwrap().sent_keys, vec!["enter"]);
    }

    #[test]
    fn codex_select_submit_gate_panel_gone_means_already_submitted() { // 双条件闸：面板消失零发键
        let (r, sent) = run_codex_select_script(1, 0, vec![codex_s1_q2_zero(), codex_answered_summary()]);
        let out = r.unwrap();
        assert!(out.sent_keys.is_empty() && out.already_submitted);  // 零键成功（已交卷分支）
    }

    #[test]
    fn codex_select_zero_gate_fails_on_focus_drift() { // 归零双验：计数归零但焦点漂移 → 中止不发提交键
    }

    #[test]
    fn codex_select_identity_gate_aborts_zero_keys() {
        let (r, sent) = run_codex_select_script(0, 0, vec![codex_s1_q2()]);
        let err = r.expect_err("身份闸必须中止");
        assert!(err.message.contains("不一致"));
        assert!(sent.is_empty());
    }

    #[test]
    fn codex_select_aborts_with_named_cause_on_confirm_screen() { // 确认屏点名（设计 §3.6）
        let (r, sent) = run_codex_select_script(0, 0, vec![codex_confirm_unanswered()]);
        let err = r.expect_err("确认屏必须中止");
        assert!(err.message.contains("未答完确认屏"));
        assert!(sent.is_empty());
    }

    #[test]
    fn codex_select_final_no_focus_aborts() { /* › 缺席 → 中止不猜 */ }

    #[test]
    fn codex_select_zero_gate_fail_aborts_after_space() { /* 空格后窗尽未归零 → 中止（可能反选） */ }
```

（`codex_s1_q2_focused1` 等派生夹具用既有 `move_focus` 同思路生成或直接从取证屏取。）

- [ ] **Step 2: 跑红**。

- [ ] **Step 3: 实现**（`question.rs` 追加；读屏/发键全走闭包，门禁可测）：

```rust
/// codex select 阶段机产物（回执合成用）。
pub struct CodexSelectOutcome {
    pub sent_keys: Vec<String>,
    /// 非末题改答分支：题号推进而计数未减（进闸前已答——设计 §3.2 步 3/4）
    pub reanswered: bool,
    /// 末题已交卷分支：双条件闸读到面板消失且未发提交键（如 notes 回车连带）
    pub already_submitted: bool,
}

/// codex 单选 select 阶段机（设计 §3.2/§3.3，核对版）。
/// 非末题：数字直选（定案=选中+推进原子）→ 题号到位即成功（计数减=新答、
///   未减=改答 reanswered）；窗尽未推进**重发数字**（不得补 →——会跳过未答）。
/// 末题：双向走位每拍复读（方向按最短环绕一次性选定）→ 已答特判跳空格 →
///   空格 → 归零闸**双验**（计数段消失 ∧ 焦点仍在目标）→ 双条件闸（面板在场
///   ∧ 归零才发回车；面板消失=已交卷零发键）→ 终态正向锚尽力核验（不翻供）。
/// 读屏 = `terminal.read()`（MenuTerminal 自带——run_select_stages 同款 poll+terminal
/// 两参数形态，不另设 read 闭包参数）。
pub fn run_codex_select_stages<T: MenuTerminal>(
    q_idx: usize,
    index: usize,
    poll: impl Fn() -> Result<Option<Vec<String>>, String>,
    terminal: &mut T,
) -> Result<CodexSelectOutcome, StageAbort> {
    use crate::inject::question_screen_oc::codex_question_screen_snapshot as snap_of;
    let mut sent: Vec<String> = Vec::new();
    let send_key = |t: &mut T, sent: &mut Vec<String>, k: &str| -> Result<(), StageAbort> {
        t.send(k)
            .map_err(|e| StageAbort::delivery(format!("codex 单选：{k} 投递失败（{e}）")))?;
        sent.push(k.to_string());
        t.settle();
        Ok(())
    };
    let first = terminal.read().ok_or_else(|| {
        StageAbort::screen("codex 单选：读不到屏——已中止，未发任何键；请人工核对终端")
    })?;
    // 确认屏点名（设计 §3.6，账本 QUESTION_REVIEW/TITLE 行）：解析不出面板时
    // 先判未答完确认屏——命中给专属文案（MAM 撞见 = 前序动作已错位，不代答）
    let snap = match snap_of(&first) {
        Some(s) => s,
        None => {
            let lowered: Vec<String> = first.iter().map(|l| l.to_lowercase()).collect();
            return Err(if crate::inject::anchor_ledger::detect(
                &lowered, "codex",
                crate::inject::anchor_ledger::scenario::QUESTION_REVIEW,
                crate::inject::anchor_ledger::slot::TITLE,
            ).is_some() {
                StageAbort::screen("codex：屏上是未答完确认屏（Submit with unanswered questions?）——前序动作已错位，MAM 不代答；请到终端人工处置")
            } else {
                StageAbort::screen("codex 单选：屏上解析不出问答面板（题号头/footer 锚不成立）——已中止，未发任何键；请人工核对终端")
            });
        }
    };
    if snap.question_idx != q_idx {
        return Err(StageAbort::screen(format!(
            "codex 身份闸：屏上第 {} 题与手机卡第 {} 题不一致——已中止，未发任何键；请核对终端当前题",
            snap.question_idx + 1, q_idx + 1
        )));
    }
    let k_base = snap.unanswered;
    let entry_focus = snap.focused;
    if !snap.is_last {
        // ===== 非末题：数字直选 + 推进核验 =====
        let digit = (index + 1).to_string();
        send_key(terminal, &mut sent, &digit)?;
        let land = |l: &Option<Vec<String>>| {
            l.as_ref().and_then(|l| snap_of(l)).map(|s| (s.question_idx, s.unanswered))
        };
        let after = poll().map_err(StageAbort::screen)?;
        let mut finish = |sent: Vec<String>, k2: usize| CodexSelectOutcome {
            reanswered: k2 == k_base, // 定案 #1 下「吞键但推进」不可达：未减 ⟹ 进闸前已答
            already_submitted: false,
            sent_keys: sent,
        };
        match land(&after) {
            Some((i2, k2)) if i2 == q_idx + 1 => Ok(finish(sent, k2)),
            _ => {
                send_key(terminal, &mut sent, &digit)?; // 重发数字（不是 →）
                let after2 = poll().map_err(StageAbort::screen)?;
                match land(&after2) {
                    Some((i2, k2)) if i2 == q_idx + 1 => Ok(finish(sent, k2)),
                    _ => Err(StageAbort::screen(
                        "codex 单选：数字两投后题号仍未推进——已中止；请核对终端（选项可能已选中，勿重复点选）",
                    )),
                }
            }
        }
    } else {
        // ===== 末题：双向走位（每拍复读）→ 特判 → 空格 → 归零双验 → 双条件提交 =====
        let target = index;
        let entry_zero = k_base == 0;
        let m = snap.options.len().max(1);
        let Some(f0) = entry_focus else {
            return Err(StageAbort::screen(
                "codex 末题：读不到焦点行（› 缺席）——不猜起点，已中止，未发任何键；请核对终端",
            ));
        };
        let down = (target + m - f0) % m;
        let walk_key = if down <= m - down { "down" } else { "up" };
        let mut steps = 0usize;
        loop {
            let s2 = terminal.read().and_then(|l| snap_of(&l)).ok_or_else(|| {
                StageAbort::screen("codex 末题：走位中读屏失败或面板形态崩——已中止；请核对终端")
            })?;
            if s2.focused == Some(target) { break; }
            if steps >= m + 2 {
                return Err(StageAbort::screen("codex 末题：走位有界耗尽仍未到目标行——已中止；请核对终端"));
            }
            send_key(terminal, &mut sent, walk_key)?;
            steps += 1;
        }
        // 已答特判（设计 §3.3 步 3）：入场已归零且焦点即目标 → 跳过空格（空格=反选）
        if !(entry_zero && entry_focus == Some(target)) {
            send_key(terminal, &mut sent, "space")?;
        }
        // 归零闸双验（设计 §3.3 步 5）：计数段消失 ∧ 焦点仍在目标
        let gated = poll().map_err(StageAbort::screen)?
            .and_then(|l| snap_of(&l))
            .is_some_and(|s| s.unanswered == 0 && s.focused == Some(target));
        if !gated {
            return Err(StageAbort::screen(
                "codex 末题：空格后未归零或焦点漂移（可能反选了已选项）——已中止，未发提交键；请核对终端",
            ));
        }
        // 双条件闸（设计 §3.3 步 6）
        match terminal.read().and_then(|l| snap_of(&l)) {
            None => Ok(CodexSelectOutcome { sent_keys: sent, reanswered: false, already_submitted: true }),
            Some(s) if s.unanswered == 0 => {
                send_key(terminal, &mut sent, "enter")?;
                // 终态正向锚（复用账本 QUESTION/RECEIPT 既有行 "answered"——键已发不翻供，仅审计）
                let receipt_hit = poll().map_err(StageAbort::screen)?.is_some_and(|l| {
                    let lowered: Vec<String> = l.iter().map(|x| x.to_lowercase()).collect();
                    crate::inject::anchor_ledger::detect(
                        &lowered, "codex",
                        crate::inject::anchor_ledger::scenario::QUESTION,
                        crate::inject::anchor_ledger::slot::RECEIPT,
                    ).is_some()
                });
                log::debug!("codex 末题提交后回执锚命中 = {receipt_hit}");
                Ok(CodexSelectOutcome { sent_keys: sent, reanswered: false, already_submitted: false })
            }
            Some(_) => Err(StageAbort::screen("codex 末题：归零后计数又变化——中止，未发提交键；请核对终端")),
        }
    }
```

- [ ] **Step 4: 跑绿 + 提交**：`cargo test --lib codex_select` PASS。
Commit: `feat(codex): 单选阶段机——非末题数字直选双验 + 末题提交闭环（设计 §3.2/§3.3）`

---

### Task 6: `run_codex_advance_stages` 阶段机（TDD）

**Files:** 同 Task 5。

- [ ] **Step 1: 失败测试**（到达验证 / 单题无效 NoEffect→Failed / 面板消失中止）：

```rust
    #[test]
    fn codex_advance_right_lands_next_question() {
        // Q1 → right → Q2：Ok(Arrived)，键 ["right"]
    }
    #[test]
    fn codex_advance_single_question_no_effect() {
        // S3 单题 → right → 屏不变：NoEffect（dispatch 层转 Failed「未生效，可重试」）
    }
```

- [ ] **Step 2: 跑红 → Step 3: 实现**：

```rust
/// codex 切题（设计 §3.5）：←/→ 单键 + 题号头到达验证（环形两端也验 i 变化）。
pub enum CodexAdvanceOutcome { Arrived, NoEffect }

pub fn run_codex_advance_stages<T: MenuTerminal>(
    direction: crate::inject::question::NavDirection,
    poll: impl Fn() -> Result<Option<Vec<String>>, String>,
    terminal: &mut T,
) -> Result<CodexAdvanceOutcome, StageAbort> {
    use crate::inject::question_screen_oc::codex_question_screen_snapshot as snap_of;
    let before = terminal.read().and_then(|l| snap_of(&l))
        .ok_or_else(|| StageAbort::screen("codex 切题：读不到面板——已中止，未发任何键；请核对终端"))?;
    let key = match direction { crate::inject::question::NavDirection::Prev => "left", crate::inject::question::NavDirection::Next => "right" };
    terminal.send(key).map_err(|e| StageAbort::delivery(format!("codex 切题：{key} 投递失败（{e}）")))?;
    terminal.settle();
    let after = poll().map_err(StageAbort::screen)?;
    match after.and_then(|l| snap_of(&l)) {
        Some(s) if s.question_idx != before.question_idx => Ok(CodexAdvanceOutcome::Arrived),
        Some(_) => Ok(CodexAdvanceOutcome::NoEffect),
        None => Err(StageAbort::screen("codex 切题：切题后面板消失（形态异常）——请核对终端")),
    }
}
```

- [ ] **Step 4: 跑绿 + 提交**：Commit `feat(codex): 切题阶段机（环形 ←/→ + 到达验证）`

---

### Task 6.5: `run_codex_free_text_stages` 自由作答编排（S4，TDD）

**Files:** 同 Task 5。（核对补任务：设计 §3.4 原无对应 Task——spec 覆盖缺口修复）

- [ ] **Step 1: 失败测试**（走位 Other + tab 全局键 + 覆盖=tab 清空〔戊探C footer 明示 `tab or esc to clear notes`——复用既有定案〕+ 回车分派两分支）：

```rust
    #[test]
    fn codex_free_text_walks_to_other_tabs_types_and_submits() {
        // 单题：走位到 Other（末位）→ tab（自动选中+开输入）→ 打字 → 回车后面板消失
        // → already_submitted（A 项定案前的读屏分派路径）
        let (r, sent) = run_codex_free_text_script(0, "my note", false,
            vec![codex_s3_single(), codex_s3_focused_other(), codex_s4_notes_open(), codex_answered_summary()]);
        let out = r.unwrap();
        assert_eq!(out.sent_keys, vec!["down", "down", "tab", /* 逐字符 */ ..., "enter"]);
        assert!(out.already_submitted);
    }

    #[test]
    fn codex_free_text_overwrite_clears_via_second_tab() { // 覆盖 = 再按 tab 清空（footer 明示）
    }

    #[test]
    fn codex_free_text_enter_keeps_panel_then_explicit_submit() {
        // 非连带形态：回车后仍在题屏 → 归零闸 + 双条件闸显式回车（复用 select 末题后半段）
    }
```

- [ ] **Step 2: 跑红 → Step 3: 实现**（走位段复用 Task 5 的 walk 逻辑；Other 行 = 屏读 options 中 label 含 `none of the above` 的下标；提交段复用 select 末题的归零闸+双条件闸——抽公共子函数 `codex_finalize_submit`）：

```rust
/// codex 自由作答（Other 行 note，设计 §3.4）：走位 Other → tab（全局键，TUI 自动
/// 确保选中）→ overwrite=再按 tab 清空旧文 → 逐字符打字 → 回车 → 读屏分派：
/// 面板消失 = 已交卷；仍在题屏 → 归零闸 + 双条件闸显式提交。
pub fn run_codex_free_text_stages<T: MenuTerminal>(
    q_idx: usize,
    text: &str,
    overwrite: bool,
    poll: impl Fn() -> Result<Option<Vec<String>>, String>,
    terminal: &mut T,
) -> Result<CodexSelectOutcome, StageAbort> { /* 走位（目标=Other 下标）→ tab [→ tab]
    → 逐字符 send → enter → 分派（None→already_submitted；Some→codex_finalize_submit）*/ }
```

- [ ] **Step 4: 跑绿 + 提交**：Commit `feat(codex): 自由作答编排——Other 走位 + tab 全局键 + 回车分派（设计 §3.4）`

---

### Task 7: dispatch 接线（StagePlan + 回执变体 + wire）

**Files:**
- Modify: `src-tauri/src/remote/api.rs`

- [ ] **Step 1: 回执变体**（`QuestionDispatch` 追加，不改既有变体——kimi 类型搁浅教训的规避）：

```rust
    /// **codex 单选 select 闭环**（设计 §3.2/§3.3）：`screen` = 到达帧（非末题）/
    /// 提交前末帧（末题）；`reanswered` = 改答分支（题号推进而计数未减）；
    /// `already_submitted` = 双条件闸读到面板已消失（如 notes 回车连带交卷）。
    CodexSelectDone {
        screen: Option<crate::inject::question_screen_oc::CodexQuestionSnapshot>,
        reanswered: bool,
        already_submitted: bool,
    },
    /// **codex 切题闭环**（设计 §3.5）：到达题快照。
    CodexAdvanceDone {
        direction: crate::inject::question::NavDirection,
        screen: Option<crate::inject::question_screen_oc::CodexQuestionSnapshot>,
    },
```

wire 合成（`QuestionDispatch::SelectDone =>` 相邻处，照抄该臂的 audit + `json_no_store` 模式）：

```rust
        QuestionDispatch::CodexSelectDone { screen, reanswered, already_submitted } => {
            let screen_json = screen.as_ref().map(serde_json::to_value).and_then(|r| r.ok());
            json_no_store(StatusCode::OK, serde_json::json!({
                "status": "key_sent",
                "done": true,
                "stage": "select",
                "screen": screen_json,
                "reanswered": reanswered,
                "alreadySubmitted": already_submitted,
            }))
        }
        QuestionDispatch::CodexAdvanceDone { direction, screen } => {
            let screen_json = screen.as_ref().map(serde_json::to_value).and_then(|r| r.ok());
            json_no_store(StatusCode::OK, serde_json::json!({
                "status": "key_sent",
                "done": true,
                "stage": QUESTION_STAGE_ADVANCE,
                "direction": /* 照既有 AdvanceDone 臂（api.rs:4698 起）的方向序列化逐字抄 */,
                "screen": screen_json,
            }))
        }
```

- [ ] **Step 2: StagePlan 变体 + for_action**：

```rust
    /// **codex 单选 select**（设计 §3.2/§3.3）：非末题数字直选双验 / 末题提交闭环
    CodexSelect { index: usize },
    /// **codex 切题**（设计 §3.5）
    CodexAdvance { direction: crate::inject::question::NavDirection },
```

`for_action`（放 SingleKey 通配臂**之前**，参照 `(A::Select, "claude")` 的位置）：

```rust
            (A::Select, "codex") => Self::CodexSelect { index: index.unwrap_or(0) },
            (A::Advance, "codex") => Self::CodexAdvance { direction },
```

- [ ] **Step 3: 序列闸放行（核对修正：改端点，不改内核）**——`session_question_answer` 的序列闸（`let seq = match action {...}` 块，kimi 先例在 `AnswerAction::Advance if tool_id == "kimi" => Vec::new()` 分支，注释注明「免静态序列，方向键在臂内参数化」）**追加同款分支**：

```rust
            // codex advance 免静态序列（kimi 同款先例）：方向键在 CodexAdvance 臂内
            // 参数化；`answer_key_sequence_for` 的 codex Advance 拒绝臂**保持不动**
            // （纵深防御——不经端点分派的调用方仍被内核拒绝）
            crate::inject::question::AnswerAction::Advance if tool_id == "codex" => Vec::new(),
```

（Select 不需要动：codex Select 落兜底臂取静态数字序列（非空、无害），阶段机臂忽略它。）

- [ ] **Step 4: dispatch 臂**（`dispatch_question_action` 追加，装配仿 `ClaudeSelect` 臂——`question_probe` + `question_terminal`（其 read 闭包即 `terminal.read()` 的来源，**不另传 read 参数**）+ `poll_question_stage`）：

```rust
        StagePlan::CodexSelect { index } => {
            let probe = question_probe(st, tool, pid);
            let q_idx = expected_idx.unwrap_or(0);
            let mut terminal = question_terminal(|| probe("codex-select-read"), injector, pid, spec);
            match crate::inject::question::run_codex_select_stages(
                q_idx, *index,
                || poll_question_stage(|| probe("codex-select-poll"), QUESTION_STAGE_POLL_TOTAL_MS),
                &mut terminal,
            ) {
                Ok(out) => {
                    let screen = probe("post").and_then(|l|
                        crate::inject::question_screen_oc::codex_question_screen_snapshot(&l));
                    QuestionDispatch::CodexSelectDone {
                        screen,
                        reanswered: out.reanswered,
                        already_submitted: out.already_submitted,
                    }
                }
                Err(e) => dispatch_abort(e),
            }
        }
        StagePlan::CodexAdvance { direction } => {
            let probe = question_probe(st, tool, pid);
            let mut terminal = question_terminal(|| probe("codex-adv-read"), injector, pid, spec);
            match crate::inject::question::run_codex_advance_stages(
                *direction,
                || poll_question_stage(|| probe("codex-adv-poll"), QUESTION_STAGE_POLL_TOTAL_MS),
                &mut terminal,
            ) {
                Ok(crate::inject::question::CodexAdvanceOutcome::Arrived) => {
                    let screen = probe("post").and_then(|l|
                        crate::inject::question_screen_oc::codex_question_screen_snapshot(&l));
                    QuestionDispatch::CodexAdvanceDone { direction: *direction, screen }
                }
                Ok(crate::inject::question::CodexAdvanceOutcome::NoEffect) => {
                    QuestionDispatch::Failed("codex 切题未生效（单题或屏未变化）——可重试".to_string())
                }
                Err(e) => dispatch_abort(e),
            }
        }
```

**FreeText 接线（Task 6.5 的路由）**：`for_action` 的 `(A::FreeText, "codex") => Self::CodexNotes { overwrite }` 改为 `Self::CodexFreeText { overwrite }`（新变体，dispatch 臂调 `run_codex_free_text_stages`，回执复用 `CodexSelectDone`）。既有 `CodexNotes` 阶段机保留（grep 其调用面：若 FreeText 是唯一入口则标注「保留备用」并加注释，不删——戊探C 定案的纯备注链在未来「选中+补 note」复合请求仍可用）。
**CodexToggle 审计**：`for_action` 的 `(A::Toggle, "codex")` 臂保留不动（request_user_input 无多选形态、经载荷不可达——加一行注释说明，作纵深防御）。

- [ ] **Step 5: wire 形状锁测试**（api.rs 端点测试模块，照既有 `kimi_plan_approval_card_*` 模式）：

```rust
    /// codex select 回执形状（设计 §3.7）：screen 带 questionIdx/unanswered；
    /// alreadySubmitted 仅双条件闸分支为 true。
    #[test]
    fn codex_select_receipt_shape() { /* 装配假 RemoteState → 断言 json 字段 */ }
```

- [ ] **Step 6: 跑绿 + 提交**：`cargo test --lib codex` PASS。
Commit: `feat(codex): dispatch 接线——Select/Advance 阶段机路由 + 回执变体与 wire 形状锁`

---

### Task 8: GET 快照门 + 旗标 + 前端对位

**Files:**
- Modify: `src-tauri/src/remote/api.rs:4005-4145`（旗标 + snapshot 门）
- Modify: `src/mobile/api.ts`、`src/mobile/QuestionCard.tsx`
- Test: `tests/mobile/`（若既有 QuestionCard 测试文件存在则追加，否则新建最小用例）

- [ ] **Step 1: 后端旗标**：

```rust
    let advance = matches!(tool_id.as_str(), "opencode" | "claude" | "kimi" | "codex") && questions.len() > 1;
    let nav_both = tool_id == "claude" || tool_id == "codex";  // codex ←/→ 环形双向（设计 §6.2.x）
```

（行内注释同步补一句 codex 依据。）

- [ ] **Step 2: GET 门**（`snapshot_supported` 加 `"codex"`；分支内仿 kimi 分支）：

```rust
                        if tool2 == "codex" {
                            let snap = crate::inject::question_screen_oc::codex_question_screen_snapshot(&lines);
                            // 闸门②日志自报（照 opencode 分支的 [question-screen] 模式）
                            return snap.map(|s| serde_json::to_value(s).unwrap_or_default());
                        }
```

- [ ] **Step 3: 前端类型**（api.ts 载荷 screen 类型加 codex 形状——联合或可选字段）：

```ts
  screen?: {
    review?: boolean;
    summary?: { q: string; a: string }[];
    heading?: string;
    checked?: (boolean | null)[];
    freeText?: string | null;
    freeTextPresent?: boolean;
    /** codex 面板形状（设计 §3.1）：题号对位主键 */
    questionIdx?: number;
    questionTotal?: number;
    unanswered?: number;
    isLast?: boolean;
    options?: string[];
    focused?: number | null;
  };
```

- [ ] **Step 4: QuestionCard 对位分支**（`v.screen.review / summary / heading` 判定链的最前插入，`findQuestionByHeading` 调用点之前）：

```ts
    if (typeof v.screen.questionIdx === "number") {
      // codex：题号直读对位（设计 §3.1——不依赖题干匹配），unanswered 驱动进度提示
      const qi = Math.min(v.screen.questionIdx, v.questions.length - 1);
      setMqIndex(qi);            // 组件级 useState setter（QuestionCard.tsx:313——已核对，343 行同款直调）
      setUnansweredHint(v.screen.unanswered ?? null); // 本任务新增局部 state，展示「N 题未答」（0/null → 不显）
      return;
    }
```

- [ ] **Step 5: 前端测试 + 提交**：对 `QuestionCard` 现有测试文件追加 codex screen 对位用例（mock 载荷带 questionIdx → 断言 mqIndex 纠偏 + 未答提示渲染）。
Commit: `feat(codex): GET 快照门 + advance/navBoth 旗标 + 前端题号对位`

---

### Task 9: 永久探针 + 矩阵补档 + 手工清单

**Files:**
- Modify: `src-tauri/src/inject/question.rs`（live_probe_tests 模块）
- Modify: `.agents/skills/win-console-inject-probe/references/screen-read-matrix.md`
- Create: `docs/engineering/plans/2026-10-08-codex-question-验收清单.md`

- [ ] **Step 1: 探针**（照 `claude_question_live_probe` 同款结构：dump `FIXTURE|` 行 + 快照全产物 + 账本三槽命中清单）：

```rust
    /// **codex 问答屏只读活体探针（#[ignore]，零注入——四闸门①工具/体检表）**：
    /// `MAM_PROBE_PID=<pid> cargo test --lib codex_question_live_probe -- --ignored --nocapture`
    #[test]
    #[cfg(windows)]
    #[ignore = "实机只读探针：codex 问答面板活体 dump（前置=MAM_PROBE_PID=<codex pid>）"]
    fn codex_question_live_probe() {
        // 结构照 claude_question_live_probe：pid 门 → read_screen_window →
        // FIXTURE 逐行 dump → codex_question_screen_snapshot 全产物 →
        // anchor_ledger 三槽 detect 命中清单 → 失败成因 log
    }
```

- [ ] **Step 2: 矩阵补档**：`screen-read-matrix.md` 追加 codex 0.160.0 形态行（题号头/归零词形/footer 三段/确认屏/摘要屏/环绕/选中不可见），主键 `(codex, 0.160.0, 问答面板)` 系列，锚判据/按键路径/已知边界三列。

- [ ] **Step 3: 手工验收清单落档**（设计附录 B 全文 + 每场景勾选框）。

- [ ] **Step 4: 活体体检（闸门④）**：自建 codex 会话跑 S1–S4，探针逐屏输出认识/不认识清单，全部认识才过。清场。
Commit: `feat(codex): 永久探针 + 屏读矩阵 0.160.0 形态行 + 手工验收清单`

---

### Task 10: 门禁、评审与实机验收

- [ ] **Step 1: 本地门禁**

```bash
cd src-tauri
cargo fmt
cargo test --lib        # 预期：新增全绿；已知基线红仅 4（database×2 / tailscale / usage::project——0.5.0 起在册）
cargo clippy --lib --tests 2>&1 | grep -cE "^warning|^error"   # 预期 0
cd .. && pnpm lint && pnpm format:check
```

- [ ] **Step 2: 并行派发**：code-reviewer（审查 diff：P0/P1 分级）+ test-runner（全量回归归因：与基线红对照零新增）。
- [ ] **Step 3: 评审修复后实机验收（全自动）**：自建会话四场景端到端（MAM 后端起本地实例 + 自建 codex 会话 + 走 §手工清单核对点），报告留证 `%USERPROFILE%\mam-probe-m6r\evidence\codex-q-acceptance-<date>.md`。
- [ ] **Step 4: 终提交**

```bash
git add -A && git commit -m "feat(codex): 问答远程作答与屏读六机制一批补齐（设计 v2.1 全量落地）"
```

并向用户汇报：分支可发 PR（正文按需求说明书体例列功能点/关键函数/测试场景——沿用 0.5.0 PR 惯例）。

---

## 自审记录（写完即查 + 2026-10-08 四项核对轮）

1. **Spec 覆盖**：R1→Task 8；R2→Task 7；R3→Task 4；R4→Task 6/7/8；R5→Task 5；R6→Task 9；R7→Task 2/5/6/6.5/8/9（四场景×三层）。§3.4→**Task 6.5**（核对轮补——初版漏 S4 编排任务）；§6 余留 A–H→Task 1 Step 3 逐项实验。✓
2. **占位符扫描**：Task 2/3 的夹具字节与账本 evidence 文件名依赖 Task 1 产出——以生成器脚本/取证文件名机制表达（非 TBD）；其余步骤代码完整。
3. **类型一致性**：`CodexQuestionSnapshot`（Task 4 定义，Task 7/8 消费）；`CodexSelectOutcome{sent_keys,reanswered,already_submitted}`（Task 5 定义，Task 6.5/7 消费）；`CodexAdvanceOutcome`（Task 6 定义 Task 7 消费）；slot 常量名（Task 3 定义 Task 4/5 消费）。✓
4. **四项核对轮修正（2026-10-08，接口对源码逐个过）**：
   - AnchorRow 字段实名 `observed_version: &'static str`（初版误写 `version: Option`）；
   - 序列闸改**端点分支**（kimi 先例 `Advance if tool_id == "kimi" => Vec::new()` 同款），内核拒绝臂保留作纵深防御；
   - 阶段机签名收敛为 `poll + terminal` 两参数（`MenuTerminal` 自带 `read()`，`question_terminal` 返回的 `Closures` 即其实现——`run_select_stages` 同款形态）；
   - 前端 setter 坐实 `setMqIndex`（QuestionCard.tsx:313）；
   - 兜底键从「补 →」改「**重发数字**」（定案 #1 原子性推论：i 未变 ⟹ 键被吞 ⟹ 该题未答，补 → 会跳过未答）——设计 §3.2 同步修正；
   - 走位补双向最短环绕（设计 §3.3 步 2 对齐，新增测试）；归零闸恢复**双验**（计数 ∧ 焦点——v2 重写丢失的 P0-1 后半句，设计同步恢复）；
   - 回执复用：账本已有 `(codex, QUESTION, RECEIPT, "answered")` 行直接作终态锚；确认屏点名挂 `scenario::QUESTION_REVIEW` + TITLE 新行；
   - 覆盖清空复用戊探C 定案「再按 tab」（footer 明示 `tab or esc to clear notes`）。
