# 二期收尾 · Phase P+ 探测批 + H1 前置修复 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为「APP 类消息注入」实现批产出五类探测定案（zcode 八问 / codex 五问 / WorkBuddy 五问 / OpenClaw+dsh 写通道 / Mac 段），并先行交付 H1（dsh 桌面端读侧接入）代码修复。

**Architecture:** 探测任务零产品代码（证据落盘 `~/mam-probe-closure/<run-id>/`，结论不超证据）；唯一代码任务是 H1——扩展 `monitor/dsh/mod.rs` 的单源宿主判定核 `cmdline_is_dsh_host`（进程发现 + 宿主存活两处共用零漂移），跳转分派复用 `win32::focus_window_for_pid` / `app_activation` 既有基建。

**Tech Stack:** Rust（sysinfo 进程扫描）/ Tauri cfg 分平台 / zcode.cjs 无头 CLI / codex queue / codebuddy CLI / curl / python-sqlite3（DB 只读副本查询）。

**上位 spec:** `docs/engineering/specs/2026-09-27-phase2-closure-app-injection-design.md`（H1/H2 与 §5 探测任务书编号）。基线分支 `feat/phase2-closure-app-injection`（off `origin/main 78bf114`）。

---

## 0. 总纪律（违反则该实验作废重做）

1. **只碰本次探测自建的会话**：测试会话一律跑在 run-id 临时目录；绝不向用户真实会话投递任何消息。**读**用户数据目录（心跳/DB/会话文件定位）允许，**写**只许写探测自建会话与证据目录。
2. **run-id 与证据目录**：`run-id = yyyymmdd-HHmmss`；证据根 `~/mam-probe-closure/<run-id>/`，下设 `run.log`（总台账，追加式带时间戳）与各任务子目录。**永不覆盖**。
3. **结束清场**：杀掉本次探测起的进程（无头 CLI / 独立 codebuddy）；**保留**证据与临时项目目录；不动用户 APP 的登录态与配置。
4. **探测任务零代码修改、零 commit**（Task 1/2 的 H1 代码任务除外，其改动仅限列明文件）；不 push、不动分支。
5. **结论不得超过证据**：每个定案值指向一条日志/采样；测不了的格子写「不可构造 + 尝试过什么」。工具缺失（如本机无 python/sqlite3）→ 降级文件级信号（mtime/文件出现）并登记工具缺口，不硬凑。
6. Windows 段先读项目技能 `.agents/skills/win-console-inject-probe/SKILL.md` 的纪律章节（本批不注入终端键序，但清场/取证/追加日志纪律同源）。
7. **环境变量隔离构造首装态**：`CODEX_HOME`（codex）/ `CODEBUDDY_CONFIG_DIR`（codebuddy）指向临时目录即得干净环境；**用后即还原**。
8. **token 成本纪律**：无头探测会真实调用用户账号（zcode 套餐 / codex / codebuddy），探针 prompt 一律最短（`hi`/`probe-1`），每格最多 2 次重复；超次先登记再补做。
9. **用户协助点（USER-ASSIST）**：需要 GUI 动作（在 APP 里创建测试会话、重启 APP、关 APP）的步骤明确标注；其余步骤 agent 直接执行。
10. 探测独立于 MAM 应用（MAM 在跑不影响；探测会话上板属正常，忽略）。

**产物对照表**：

| 产物 | 位置 |
|---|---|
| 探测证据与台账 | `~/mam-probe-closure/<run-id>/`（本地不入库） |
| 探测报告 + 五类定案表 | `docs/research/refs/phase2-消息注入/2026-10-03-app-injection-probe-report.md`（本地，research 不入库） |
| spec 定案栏回填 + 裁决门议程 | 仓库 spec 文件（Task 8 commit） |
| H1 代码 | `src-tauri/src/monitor/dsh/mod.rs`、`src-tauri/src/commands/session.rs`（Task 1/2 commit） |
| Mac 段报告 | 用户执行后以消息全文回传（§Mac 段模板） |

---

### Task 0: 环境基线（P0）

**Files:** Create: `~/mam-probe-closure/<run-id>/run.log`、`p0-baseline.md`

- [ ] **Step 1: 建 run-id 与证据目录**

```bash
RUN_ID=$(date +%Y%m%d-%H%M%S)
EV=~/mam-probe-closure/$RUN_ID
mkdir -p $EV && echo "$(date +%FT%T) run-id=$RUN_ID 探测启动" >> $EV/run.log && echo $RUN_ID
```

Expected: 打印 run-id（后续任务用 `$EV` 指代该目录；每次新 Bash 调用需重设 `EV=~/mam-probe-closure/<run-id>`）。

- [ ] **Step 2: 采集版本基线，写入 p0-baseline.md**

```bash
{ echo "## P0 基线 $(date +%FT%T)"
  echo "- OS: $(uname -s) $(powershell.exe -NoProfile -Command '[Environment]::OSVersion.Version' | tr -d '\r')"
  echo "- zcode CLI: $(ELECTRON_RUN_AS_NODE=1 'D:/Program Files/ZCode/ZCode.exe' 'D:/Program Files/ZCode/resources/glm/zcode.cjs' --version 2>&1)"
  echo "- ZCode.exe 产品版本: $(powershell.exe -NoProfile -Command '(Get-Item "D:/Program Files/ZCode/ZCode.exe").VersionInfo.ProductVersion' | tr -d '\r')"
  echo "- codex: $(codex --version 2>&1)"
  echo "- WorkBuddy 心跳版本: $(grep -o '"version": *"[^"]*"' ~/.workbuddy/sessions/$(ls -t ~/.workbuddy/sessions | head -1) | head -1)"
  echo "- dsh 桌面端: $(powershell.exe -NoProfile -Command '(Get-Item "D:/Program Files/Deepseek-Harness/DeepSeek Harness.exe").VersionInfo.ProductVersion' | tr -d '\r')"
  echo "- node: $(node --version 2>&1) / npm: $(npm --version 2>&1)"
  echo "- python: $(python --version 2>&1) / sqlite3: $(sqlite3 --version 2>&1 | head -1)"
  echo "- openclaw: $(where openclaw 2>&1 | head -1)"
} | tee $EV/p0-baseline.md
```

Expected: 每项有版本号或「未找到」；python/sqlite3 缺失按纪律 5 登记降级口径。**git 零改动。**

---

### Task 1: H1-a dsh 桌面端宿主判定扩展（TDD）

**Files:**
- Modify: `src-tauri/src/monitor/dsh/mod.rs`（`cmdline_is_dsh_host` 核 ~L104-117 + 文档注释；tests 模块 ~L387 追加用例）

- [ ] **Step 1: 写失败测试**（追加到 `dual_tokens_qualify_as_host` 同级；`cmd(&[...])` 助手已在 tests 模块，构造 `Vec<OsString>`）

```rust
    #[test]
    fn desktop_host_tokens_qualify() {
        // 实测桌面端 cmdline（v0.2.0-rc.2，2026-09-27 取证，spec §3 证据 4）：
        // "DeepSeek Harness.exe" --expose-internals
        //   "…\app.asar\dsh\node_modules\@deepseek-ai\dsh-desktop-host\lib\index.js"
        //   "…\app.asar\dsh"  "C:\Users\<u>\.dsh\profiles\desktop"  …
        assert!(cmdline_is_dsh_host(&cmd(&[
            r"D:\Program Files\Deepseek-Harness\DeepSeek Harness.exe",
            "--expose-internals",
            r"D:\Program Files\Deepseek-Harness\resources\app.asar\dsh\node_modules\@deepseek-ai\dsh-desktop-host\lib\index.js",
            r"D:\Program Files\Deepseek-Harness\resources\app.asar\dsh",
            r"C:\Users\bunny\.dsh\profiles\desktop",
            r"D:\Program Files\Deepseek-Harness\resources\runtime\primary-runtime",
        ])));
        // POSIX 形态路径同样命中（跨平台口径，macOS 桌面端对齐探测回填前先保口径）
        assert!(cmdline_is_dsh_host(&cmd(&[
            "/Applications/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness",
            "/Applications/DeepSeek Harness.app/Contents/Resources/app.asar/dsh/node_modules/@deepseek-ai/dsh-desktop-host/lib/index.js",
            "/Users/u/.dsh/profiles/desktop",
        ])));
    }

    #[test]
    fn desktop_tokens_do_not_match_unrelated_electron() {
        // --expose-internals 是 Electron 通用旗子，不能单独作判据；
        // 无 dsh-desktop-host / profiles\desktop 特征的进程不命中
        assert!(!cmdline_is_dsh_host(&cmd(&[
            r"C:\Apps\SomeElectron.exe",
            "--expose-internals",
            r"C:\Apps\some\lib\index.js",
        ])));
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test desktop_host_tokens`
Expected: `desktop_host_tokens_qualify` FAIL（现有核只认 `dsh`+`web` 双令牌）；`desktop_tokens_do_not_match_unrelated_electron` 可能已 PASS（保留作防回归）。

- [ ] **Step 3: 最小实现**——`cmdline_is_dsh_host` 扩桌面特征（单源：进程发现与 `host::tool_host_alive_in` 共用，零其他改动）；同步改函数文档注释

```rust
/// dsh 宿主 cmdline 判定门（单源，M0 §5 + H1 桌面端扩展）：两类形态任一命中即宿主——
/// ① web 宿主：cmdline 含 "dsh"（精确令牌，或 /dsh、\dsh 路径结尾）与 "web"（精确令牌）；
/// ② 桌面宿主（v0.2.0+）：DeepSeek Harness.exe 内嵌 dsh-desktop-host 进程，无 node 令牌，
///    特征 = 任一参数含 "dsh-desktop-host"（包路径子串，最强特征）或以
///    `\.dsh\profiles\desktop` / `/.dsh/profiles/desktop` 结尾（用户数据 profile 路径）；
///    --expose-internals 为 Electron 通用旗子，单独过弱，不作独立判据。
/// find_dsh_processes（进程发现）与 host::tool_host_alive_in（宿主存活判定）
/// 共用本口径，防两处判定漂移
pub fn cmdline_is_dsh_host(cmd: &[std::ffi::OsString]) -> bool {
    let tokens: Vec<String> = cmd
        .iter()
        .map(|a| a.to_string_lossy().to_string())
        .collect();
    let has_dsh = tokens
        .iter()
        .any(|t| t == "dsh" || t.ends_with("/dsh") || t.ends_with("\\dsh"));
    let has_web = tokens.iter().any(|t| t == "web");
    let is_desktop = tokens.iter().any(|t| {
        t.contains("dsh-desktop-host")
            || t.ends_with("\\.dsh\\profiles\\desktop")
            || t.ends_with("/.dsh/profiles/desktop")
    });
    (has_dsh && has_web) || is_desktop
}
```

- [ ] **Step 4: 跑全模块测试 + 门禁**

Run: `cd src-tauri && cargo test dsh && cargo clippy --all-targets -- -D warnings && cargo fmt --check`
Expected: dsh 全绿（含既有 `dual_tokens_qualify_as_host` 等用例不回归）；clippy/fmt 干净。

- [ ] **Step 5: 实机核验（MAM 重启后桌面端会话上板）**

USER-ASSIST：重启 MAM（`pnpm tauri:dev` 或安装版）→ 看板出现桌面端 dsh 会话卡（当前桌面端正在跑的 PersonalAffairs-Meetings 会话）。记录到 `run.log`。

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/monitor/dsh/mod.rs
git commit -m "feat(dsh): H1 桌面端宿主判定接入——判定门扩 dsh-desktop-host/profiles 特征（进程发现与存活判定单源共用）"
```

---

### Task 2: H1-b dsh 桌面端跳转聚焦

**Files:**
- Modify: `src-tauri/src/monitor/dsh/mod.rs`（新增 `find_dsh_desktop_host_pid` + 桌面特征单源助手）
- Modify: `src-tauri/src/commands/session.rs`（dsh 跳转分派：桌面宿主优先，web 路径保留）

- [ ] **Step 1: 写失败测试**（dsh/mod.rs tests；`cmd` 助手同 Task 1）

```rust
    #[test]
    fn desktop_kernel_matches_only_desktop() {
        let desktop = cmd(&[
            r"D:\Program Files\Deepseek-Harness\DeepSeek Harness.exe",
            r"...\@deepseek-ai\dsh-desktop-host\lib\index.js",
            r"C:\Users\bunny\.dsh\profiles\desktop",
        ]);
        let web = cmd(&["node", "dsh", "web"]);
        assert!(cmdline_is_dsh_desktop_host(&desktop));
        assert!(!cmdline_is_dsh_desktop_host(&web));
    }
```

- [ ] **Step 2: 确认失败**

Run: `cd src-tauri && cargo test desktop_kernel`
Expected: FAIL——`cmdline_is_dsh_desktop_host` 未定义。

- [ ] **Step 3: 实现**——桌面特征提取为单源助手，判定门复用之；新增跳转用的宿主 pid 探测（复刻 `find_dsh_processes` 的扫描形态）

```rust
/// 桌面宿主特征单源（cmdline_is_dsh_host 桌面分支与跳转分派共用）
pub fn cmdline_is_dsh_desktop_host(cmd: &[std::ffi::OsString]) -> bool {
    let tokens: Vec<String> = cmd
        .iter()
        .map(|a| a.to_string_lossy().to_string())
        .collect();
    tokens.iter().any(|t| {
        t.contains("dsh-desktop-host")
            || t.ends_with("\\.dsh\\profiles\\desktop")
            || t.ends_with("/.dsh/profiles/desktop")
    })
}
```

`cmdline_is_dsh_host` 的 `is_desktop` 分支改为 `cmdline_is_dsh_desktop_host(cmd)`（DRY，测试同 Task 1 全绿）。新增：

```rust
/// 当前桌面上 dsh 桌面端宿主进程 pid（H1 跳转分派用；无则 None → 走 web 宿主路径）
pub fn find_dsh_desktop_host_pid(system: &sysinfo::System) -> Option<u32> {
    system
        .processes()
        .iter()
        .find(|(_, p)| !p.cmd().is_empty() && cmdline_is_dsh_desktop_host(p.cmd()))
        .map(|(pid, _)| pid.as_u32())
}
```

- [ ] **Step 4: 跳转分派**——`commands/session.rs` 的 dsh 分支改为跨平台前置（现有 `#[cfg(not(windows))]` 块内 macOS web 路径**保留为回落**）。在进入平台分派之前插入：

```rust
        // dsh（H1）：桌面端宿主在场 → 聚焦桌面 APP 窗口；不在场才回落 web 宿主路径
        if agent_type.as_deref() == Some("dsh") {
            let system = sysinfo::System::new_all();
            if let Some(pid) = crate::monitor::dsh::find_dsh_desktop_host_pid(&system) {
                #[cfg(windows)]
                {
                    match crate::window::win32::focus_window_for_pid(pid) {
                        Ok(()) => {
                            mark_read_on_jump(&app, &session_id, &agent_type);
                            return Ok(serde_json::json!({ "via": "dsh-desktop" }));
                        }
                        Err(e) => return Err(format!("无法聚焦 DeepSeek Harness 窗口：{e}")),
                    }
                }
                #[cfg(target_os = "macos")]
                {
                    use tauri::Manager; // cfg 块内导入惯例见文件头注释
                    if let Some(p) = system.process(sysinfo::Pid::from_u32(pid)) {
                        if let Some(exe) = p.exe().and_then(|e| e.to_str()) {
                            if let Some(bundle) = crate::window::app_activation::app_bundle_from_exe(exe) {
                                if crate::window::app_activation::activate_app_bundle(&bundle).is_ok() {
                                    mark_read_on_jump(&app, &session_id, &agent_type);
                                    return Ok(serde_json::json!({ "via": "dsh-desktop" }));
                                }
                            }
                        }
                    }
                    // macOS 激活失败 → 落入下方 web 路径（不 return）
                }
                #[cfg(not(any(windows, target_os = "macos")))]
                {
                    let _ = pid; // Linux 不支持聚焦（window/mod 既有口径）
                }
            }
        }
```

注意：该块插入点在函数内现有 `#[cfg(not(windows))]` 块**之前**；macOS web 回落路径（`focus_dsh_tab`）与 Windows 无桌面宿主时的行为（现有：无 dsh 跳转 → 维持原样）不变。`app_activation::app_bundle_from_exe` 入参是 exe 路径（见其签名 `&str`）。

- [ ] **Step 5: 门禁**

Run: `cd src-tauri && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check`
Expected: 全绿（Windows 本机跑；Linux CI 不可达分支由 cfg 保证编译）。

- [ ] **Step 6: 实机核验（USER-ASSIST）**：MAM 里点桌面端 dsh 会话卡跳转 → 前置 DeepSeek Harness 窗口；网页版 dsh 会话（若在跑）跳转行为不变。记录 `run.log`。

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/monitor/dsh/mod.rs src-tauri/src/commands/session.rs
git commit -m "feat(dsh): H1 桌面端跳转聚焦——win32 按 pid / macOS 应用激活，web 宿主路径保留回落"
```

---

### Task 3: PZ · zcode 八问（探测）

**Files:** Create: `$EV/pz/`（每个子问一节追加进 `$EV/pz/notes.md` + 原始输出文件）

约定：`ZC()` 展开为 `ELECTRON_RUN_AS_NODE=1 "D:/Program Files/ZCode/ZCode.exe" "D:/Program Files/ZCode/resources/glm/zcode.cjs"`；临时项目 `$EV/pz-proj`（mkdir）。八问 = ①无头新建出现条件 ②resume 全链 ③可见性三态 ④surface 差异 ⑤并发 ⑥`--json` 回执 ⑦`--mode` ⑧斜杠与版本。

- [ ] **Step 1: PZ-0 夹具 + 问①新建出现条件（H10 依赖）**——无头新建会话

```bash
EV=~/mam-probe-closure/<run-id>; mkdir -p $EV/pz $EV/pz-proj
ELECTRON_RUN_AS_NODE=1 "D:/Program Files/ZCode/ZCode.exe" "D:/Program Files/ZCode/resources/glm/zcode.cjs" \
  --prompt "hi [mam-probe]" --cwd "$EV/pz-proj" --mode build --json 2>&1 | tee $EV/pz/pz0-new.json
```

Expected: JSON 输出；提取 `sessionId`（`sess_...`）记入 notes.md。若报「工作区不在册/需信任」类错误 → **本身就是问①的定案数据**（新目录不可无头开局的边界，H10 候选列表口径依据），照录。

- [ ] **Step 2: PZ-1 无头 resume 全链**（用 PZ-0 的 sess_id）

```bash
ELECTRON_RUN_AS_NODE=1 "D:/Program Files/ZCode/ZCode.exe" "D:/Program Files/ZCode/resources/glm/zcode.cjs" \
  --prompt "probe-2 reply in one word [mam-probe]" --resume <sess_id> --cwd "$EV/pz-proj" --mode build --json 2>&1 | tee $EV/pz/pz1-resume.json
```

Expected: JSON 含新回合；记注入→落盘时延（对照 `~/.zcode/cli/` 或 v2 库文件 mtime 前后差）**与 turn 总时长**（JSON 时长字段或墙钟差——H4 watchdog 默认值的定案数据，裁决门回填）。

- [ ] **Step 3: PZ-2 可见性三态**——APP 运行中，每 2s 采 `~/.zcode/v2/tasks-index.sqlite*` mtime 共 60s：

```bash
EV=~/mam-probe-closure/<run-id>
for i in $(seq 1 30); do stat -c '%y %n' ~/.zcode/v2/tasks-index.sqlite* 2>/dev/null | tr '\n' ' '; echo; sleep 2; done > $EV/pz/pz2-mtime.txt
grep -c "$(date +%Y-%m-%d)" $EV/pz/pz2-mtime.txt
```

然后 USER-ASSIST：在 ZCode APP 里查看临时项目是否出现该会话（不重启 / 手动刷新 / 重启 APP 三态分别记录；重启前先确认 APP 内有无刷新手势）。写入 notes.md「判定 F 兑现层级」。

- [ ] **Step 4: PZ-3 `--surface` A/B**——同 PZ-1 命令分别加 `--surface desktop` 与 `--surface terminal` 各跑一次（新 sess 或同 sess 追加均可），diff 两次 JSON 与库行差异（python 可用则查 DB 副本，否则记 JSON 差异）。

- [ ] **Step 5: PZ-4 并发**——长 turn（`--prompt "count from 1 to 30 slowly, one number per line"`）进行中（另一终端）再发第二条 resume → 记录第二条行为（排队？报错？串行？）。再 USER-ASSIST：APP 内打开同一会话发消息与无头并发一次。写入 notes.md。

- [ ] **Step 6: PZ-5 `--json` 回执结构**——整理 pz0/pz1 JSON 的字段清单（sessionId / 末条 assistant / token 用量 / 耗时字段是否在场），定「回执归一」可解析面。

- [ ] **Step 7: PZ-6 `--mode` 四档**——build/edit/plan/yolo 各跑一次最短 prompt（`hi`），记每档行为差异（是否有审批停顿/plan 输出/直接执行）与**建议默认档**。

- [ ] **Step 8: PZ-7 斜杠命令**——`--prompt "/model"` 与 `--prompt "/new"` 各一次：输出是命令执行结果还是字面文本？记「斜杠等效性」定案（三期 F3.1 取证）。

- [ ] **Step 9: PZ-8 版本基线**——P0 已采 CLI 版本；`--version` 与 APP 产品版本对照记入；`--help` 全文存 `$EV/pz/help.txt`（flag 面漂移基线）。

- [ ] **Step 10: 清场（安全口径：按探测路径过滤，勿按 zcode.cjs 过滤——会误杀 ZCode APP 自身的 app-server 子进程）**

```bash
powershell.exe -NoProfile -Command "Get-CimInstance Win32_Process | Where-Object { \$_.CommandLine -match 'mam-probe-closure' } | ForEach-Object { Stop-Process -Id \$_.ProcessId -Force }"
```

notes.md 收尾：八问逐条一行结论。

---

### Task 4: PC · codex 五问（探测）

**Files:** Create: `$EV/pc/notes.md` + 输出文件

- [ ] **Step 1: PC-0 夹具（USER-ASSIST）**——用户在 **Codex 桌面 APP** 里以 `$EV/pc-proj`（mkdir 先建）为目录新建会话并发一句 `hi` 后置空闲。agent 记录落点：

```bash
EV=~/mam-probe-closure/<run-id>; mkdir -p $EV/pc $EV/pc-proj
ls -t ~/.codex/sessions/*/*/*/*.jsonl | head -3; tail -c 600 "$(ls -t ~/.codex/sessions/*/*/*/*.jsonl | head -1)"
```

Expected: 最新 rollout 即 APP 会话；提取其 session id（rollout 首行 `session_id` 字段或文件名 UUID），记入 notes.md。

- [ ] **Step 2: PC-1 queue 触达（空闲态）**

```bash
codex queue --thread <session_id> --message "probe-1 [mam-probe]" 2>&1 | tee $EV/pc/pc1-queue-idle.txt; echo "exit=$?"
```

Expected: 退出码与输出照录；随后 60s 内每 5s `tail` rollout 是否追加该消息；USER-ASSIST：APP 内是否出现排队消息并执行（记录「APP 原生排队实时性」）。

- [ ] **Step 2b: PC-1b queue 触达（运行态）**——USER-ASSIST：让该 APP 会话跑一个长任务（如 `list the files in this directory one by one, slowly`）；任务进行中 agent 再 queue 一条 `probe-1b [mam-probe]` → 记录行为（排队等待 turn 结束？插队？报错？）与 APP 内呈现，写入 notes.md「空闲/运行态差异」。
——`codex queue` 的 stdout 是否有 runId/确认；`codex agents --help` 与 `codex agents`（若在场）能否观测队列状态；结论写「投递成功/被消费信号」定案。

- [ ] **Step 4: PC-3 exec resume 兜底**——同一 APP 会话：①APP 开着时 `codex -C "$EV/pc-proj" exec resume <id> "probe-2 [mam-probe]" --skip-git-repo-check 2>&1 | tee $EV/pc/pc3-exec-open.txt`；②USER-ASSIST 关 APP 后重复一次（pc3-exec-closed.txt）。记录单写者锁报错形态/落盘成功/索引滞后。**flag 形态（Mac 段实测纠正，L10 回填）**：`-C` 是 codex 全局 flag，**必须置于子命令之前**——置于 `exec resume` 之后会 exit 2（`unexpected argument '-C' found`）。

- [ ] **Step 5: PC-4 id 映射**——比对三处 id：rollout 文件名 UUID / rollout 首行 session_id / MAM 侧 codex 会话 id（读 `~/.codex/session_index.jsonl` 尾行）；`--thread` 试 UUID 与「会话名」两种形态各一次。写映射规则定案。

- [ ] **Step 6: PC-5 daemon 前置**——APP 关闭状态下：`codex app-server daemon --help` 看子命令；跑 queue 是否成功；`codex app-server daemon bootstrap`（或对等子命令）后再试一次。记「daemon 自启/引导」定案与超时预算。

- [ ] **Step 7: 清场 + notes.md 五问一行结论。**

---

### Task 5: PW · WorkBuddy 五问（探测）

**Files:** Create: `$EV/pw/notes.md`

- [ ] **Step 1: PW-1/5 配置目录与端口拓扑**（纯读）

```bash
EV=~/mam-probe-closure/<run-id>; mkdir -p $EV/pw
grep -l "url\|endpoint" ~/.workbuddy/sessions/*.json | wc -l
for f in $(ls -t ~/.workbuddy/sessions/*.json | head -5); do grep -o '"url": *"[^"]*"' $f; done | tee $EV/pw/pw5-ports.txt
find ~/.workbuddy -maxdepth 3 -name "settings.json" 2>/dev/null | head; find ~/.workbuddy -maxdepth 4 -type d -name "*codebuddy*" 2>/dev/null | head -5
```

Expected: 端口是每会话异端口还是同端口（拓扑定案）；定位内嵌 codebuddy 的配置目录与 `settings.json`（serve 密码/token 字段）。

- [ ] **Step 2: PW-2 端点 API 面**（只读 GET）

```bash
PORT=<PW-1 最新心跳端口>; TOK=<settings.json 里的 token>
curl -s -m 5 -H "Authorization: Bearer $TOK" -H "X-CodeBuddy-Request: 1" "http://127.0.0.1:$PORT/api/openapi.json" | head -c 3000 | tee $EV/pw/pw2-openapi-head.txt
curl -s -m 5 -o /dev/null -w "%{http_code}\n" "http://127.0.0.1:$PORT/api/docs"
```

Expected: openapi 内容（找 `jobs`、`reply`、`sessions` 端点）或 401/404（鉴权失败也照录，换 `X-Access-Token` 头再试一次）。

- [ ] **Step 3: PW-3 jobs/reply 投递（USER-ASSIST 前置）**——用户在 WorkBuddy 里以 `$EV/pw-proj` 建一个测试会话并空闲；agent 对**该自建会话**按 openapi 定案端点 POST 一条 `probe-1 [mam-probe]`；观察 APP 内出现与否 + 会话 JSONL 追加。**只投自建会话。**

- [ ] **Step 4: PW-4 独立 CLI 复刻**——隔离首装验证（不动 WB 配置）：

```bash
EV=~/mam-probe-closure/<run-id>
npm install @tencent-ai/codebuddy-code --prefix "$EV/cb-cli" 2>&1 | tail -2
CB="$EV/cb-cli/node_modules/.bin/codebuddy"
CODEBUDDY_CONFIG_DIR="$EV/cb-config" "$CB" --version 2>&1; CODEBUDDY_CONFIG_DIR="$EV/cb-config" "$CB" --help 2>&1 | grep -E "print|resume|serve|output-format" | head
```

Expected: 独立 CLI 版本（对照内嵌 2.115 偏差）与无头 flag 在场；然后（若 PW-1 定位到 WB 配置目录）`CODEBUDDY_CONFIG_DIR=<WB 目录> "$CB" --resume <自建 WB 会话 id> -p "probe-2"` 实测复刻路线（写入 WB 会话存储 = 路线 B 成立）。失败照录。

- [ ] **Step 5: notes.md 五问一行结论 + 三路线（A/B/C）建议。**

---

### Task 6: PL-A · OpenClaw 半天轻探测

- [ ] **Step 1: 在场与 API 面**——`where openclaw` / `openclaw --version` / `openclaw sessions --help`（子命令在场性）；`~/.openclaw` 目录形态；gateway 在跑则只读探 API 面（config 里的端口 → `curl -s http://127.0.0.1:<port>/`）。未安装 → 登记「不可构造：本机无 openclaw」+ 以官方文档佐证 sessions CLI 形态。
- [ ] **Step 2: 一格结论**——`$EV/pl/openclaw.md`：「存在写通道（是/否）+ 依据 + 集成形态草图」。

### Task 7: PL-B · dsh 写通道源码通读 + 端点实测

- [ ] **Step 1: 源码通读**（本地仓库 `E:\LLMproject\deepseek-harness\deepseek-harness`，零运行时风险）。**已核实的入口指针**：① `packages/host/` = web GUI 宿主半边（`webserver/` 具名路由表 + SPA 服务 + open-in-app 路由——桌面端与 web 同核，本地端口 19387 最可能就是这套 webserver 路由，**会话投递端点优先在这里找**）；② `apps/desktop/lib/main.js` = 打包压缩产物（单字母标识符），只用于取证 host 进程的 spawn 参数与端口发现机制，**不做正文通读**，桌面壳源码看 `apps/desktop/` 下有无 src；③ 其余编程面包：`packages/api`、`packages/sdk`、`packages/acp`、`packages/webhook`、`packages/jobs`。重点问题不变：**有没有「向指定会话投递用户消息」的端点或 SDK 方法**。产出 `$EV/pl/dsh-source.md`（含关键文件:行号引文）。
- [ ] **Step 2: 端点实测**（只读）——对在跑桌面 host 端口（PW 式发现：`Get-NetTCPConnection` 过滤 DeepSeek Harness pid）按源码揭示的路径 GET 健康类/只读端点；**POST 仅当源码证实存在会话投递端点且目标为自建会话时**（先在桌面 APP 用 `$EV/pl-proj` USER-ASSIST 建测试会话）。
- [ ] **Step 3: 一格结论**——`$EV/pl/dsh.md`：「存在写通道（是/否）+ 依据 + 集成形态草图（host API/ACP/SDK）」。

---

### Task 8: 探测报告与 spec 定案回填

- [ ] **Step 1: 汇总报告**——`docs/research/refs/phase2-消息注入/2026-10-03-app-injection-probe-report.md`：每系 PASS/FAIL/不可达一行 + 定案表（H7/H8/H9/H10/H2 通道参数、`--mode` 默认档、watchdog/超时预算值、并发结论、回执字段、id 映射、dsh/OpenClaw 写通道结论）。
- [ ] **Step 2: spec 回填 commit**——更新 spec：附录 A「定案状态」栏、H 节内「PZ/PC/PW/PL 定案」占位值、证据台账等级；形成裁决门议程（WorkBuddy 路线 / H11 并入否 / 验收线与超时定值 / dsh 写通道结论）。

```bash
git add docs/engineering/specs/2026-09-27-phase2-closure-app-injection-design.md
git commit -m "docs(spec): Phase P+ 探测定案回填 + 裁决门议程（探测报告见 research 本地档）"
```

- [ ] **Step 3: 向用户汇报**——五类定案 + 裁决门四事，等待裁决。

---

## Mac 段任务书（用户执行；报告全文回传）

> 环境：独立 clone 本仓库（分支 `feat/phase2-closure-app-injection`）到 `/tmp/mam-closure-probe/repo`；证据 `/tmp/mam-closure-probe/<run-id>/`。全程不碰真实会话；工具版本先记 `--version`。参考 `references/mac-verification.md`（若有）与上位 spec §5 Mac 段定义。

### Mac-1 · APP 对齐探测（三工具核心格）

1. **zcode on Mac**：定位 `/Applications/ZCode.app/Contents/Resources/glm/zcode.cjs` 与宿主二进制；用 `ELECTRON_RUN_AS_NODE=1 <二进制> <cjs> --prompt "hi [mam-probe]" --cwd /tmp/mam-closure-probe/<run-id>/pz --json` 复刻 Windows PZ-0/PZ-1/PZ-3 三格（新建/resume/surface 差异）；APP 重启可见性照 Windows PZ-2 记录。**记录 Mac 调用形态与 Windows 的差异**（spec §5 表脚注回填）。
2. **WorkBuddy on Mac**：`~/.workbuddy/sessions/*.json` 心跳 url 字段在不在（端口拓扑与 Windows 是否一致）；配置目录路径形态。只读。
3. **codex APP on Mac**：若装有 Codex 桌面 APP——自建会话后 `codex queue --thread <id> --message "probe-1 [mam-probe]"` 复刻 PC-1；未装则登记「不可构造」。

### Mac-2 · M7 终验补课（三通道 × 注入实机）

1. 仓库内跑既有 `#[ignore]` 实机注入 E2E（m9r 系）：`cd src-tauri && cargo test -- --ignored`（带实机环境；逐条记录 PASS/FAIL/超时）。
2. 人工抽查最简链（claude 必测，其余量力）：MAM `pnpm tauri:dev` + 远程开启 + 手机 PIN 配对 → 对 tmux / iTerm2 / Terminal.app 各一个 claude 会话发一条消息 → 终端出现 `[mobile <设备名>]` 输入；黄排队一条 + 「立即发送」插队一次；锁屏状态下再发一条（记可用性）。每路一行 PASS/FAIL + 截图。

### Mac-3 · session-create Mac 段交叉验证（仅 1–2 核心格，不与在跑任务重复）

对照 `docs/engineering/plans/2026-09-27-remote-session-create-probe.md` 的 M1（屏读可达性）：仅复测 **tmux capture-pane** 与 **Terminal.app AppleScript 读屏** 各一格，与在跑报告互为印证；不重复其余。

### Mac-4 · 回传报告模板（最终消息全文返回，同时落 `/tmp/mam-closure-probe/<run-id>/report.md`）

```
① 环境基线（macOS 版本 / zcode·codex·WB 版本）
② Mac-1 三工具逐格结论（含 zcode Mac 调用形态差异）
③ Mac-2 终验清单（#[ignore] E2E 逐条 + 三通道人工抽查 + 锁屏）
④ Mac-3 交叉验证两格结论
⑤ 关键证据摘录（命令 + 输出片段）
⑥ 遗留问题与清理说明
```

---

## Windows 段补测清单（GUI USER-ASSIST 待办，2026-10-03 探测执行后落账）

> 用户统一补测时逐项做；每项完成后把结果（成/败/现象）回给主线，主线补录证据档 `~/mam-probe-closure/20261003-022408/` 对应 notes。

1. **H1 实机核验**：MAM（带 `08ef620` 重启）确认桌面 dsh 会话卡上板 + 点卡跳转聚焦 DeepSeek Harness 窗口。
2. **ZCode APP 三态定案**：查 `sess_7e371afd`——不刷新 / 手动刷新 / 重启 APP 三态分别记录（**重点：无头会话 35 分钟 0 收录之后重启是否收录**，判定 F 兑现层级定案）；APP 内打开同会话与无头并发一次；`--mode` 档位在 APP 内的呈现。
3. **Codex APP 夹具（PC-0）→ 触发 PC-1/PC-3 补测**：以 `~/mam-probe-closure/20261003-022408/pc-proj` 建会话发 `hi` 置空闲 + 告知主线哪个 codex 进程是 APP → 主线补 PC-1（空闲+运行态 queue 投递）与 PC-3（exec resume 兜底，APP 开/关两态）。
4. **WorkBuddy 夹具（PW-2/3）**：启动 WorkBuddy 以 `~/mam-probe-closure/20261003-022408/pw-proj` 建测试会话 → 主线补 GET openapi.json 面 + `jobs/reply` 投递实测（WorkBuddy 路线终裁素材）。
5. **遗留登记**：preset_v2_test 5 个既有失败（疑环境依赖，独立排查）；codex 0.160.0 queue flag 面复验；dsh 19387 端口归属与版本漂移；本机 sqlite3 CLI 缺失（python 已代偿）。

## Mac 段补充注意事项（基于 Windows 段结果，2026-10-03）

- **zcode**：Windows 实测无头会话 **35 分钟未被 APP 任务列表收录**（索引存活下）——Mac 的「重启后是否收录」格升级为判定 F 兑现层级的**关键定案格**；`--surface desktop` 对收录的影响 Windows 未定案，Mac 必测；Mac 调用形态（宿主二进制路径 + `ELECTRON_RUN_AS_NODE`）必录（spec §5 表脚注回填）。
- **codex**：Windows 侧 PC 实测因缺夹具未完成——Mac 若装有 Codex APP，PC-1（空闲+运行态 queue）升级为**主证格**（Windows 缺口 Mac 先出）；开工先记 `codex --version` 并复验 queue flag 面（Windows 遗留 0.160.0 疑云）。
- **WorkBuddy**：Windows 未定位到 serve 凭据（settings.json 无 token）——Mac 增查 **Keychain** 与配置目录（Electron 凭据在 macOS 常入 Keychain）；心跳 url 拓扑对齐照旧。
- **dsh 桌面端（若 Mac 装有）**：19387→401 现象交叉验证（归属未定案，版本漂移嫌疑）；Mac 桌面端版本与 Windows rc.2 对照。
- **M7 终验与 Mac-3 交叉验证任务不变，优先级最高**。
- 清场过滤串注意：Mac 的探测目录是 `mam-closure-probe`（Windows 是 `mam-probe-closure`，目录名不同，kill 过滤串别照抄）。

## 自审记录（writing-plans Self-Review）

1. **Spec 覆盖**：spec §5 探测任务书 PZ 八问→Task 3（新建出现条件 = Step 1，resume/可见性/surface/并发/json/mode/斜杠版本 = Step 2-9）；PC 五问（含空闲+运行态）→Task 4（PC-1b 补运行态）；PW 五问→Task 5；PL→Task 6/7；Mac 段三部分→Mac-1/2/3；H1 两半→Task 1/2；报告与裁决门→Task 8。无缺口。
2. **占位符扫描**：探测步骤均给出具体命令与期望证据；`<run-id>`/`<sess_id>`/`<session_id>`/`<PORT>`/`<TOK>` 为运行时变量，均标注来源步骤。无 TBD。
3. **类型/签名一致性**：`cmdline_is_dsh_host(&[OsString]) -> bool`、`cmdline_is_dsh_desktop_host(&[OsString]) -> bool`、`find_dsh_desktop_host_pid(&System) -> Option<u32>`、`focus_window_for_pid(u32)`、`app_bundle_from_exe(&str) -> Option<String>` 与在产签名一致（Task 2 Step 4 引用处参数形态核对过）。
