# 远程新建会话 · Phase C 实施计划（单批 C0–C12）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 移动端一键「新建会话」：指定目录 + 四家终端工具（claude/codex/kimi/opencode），主机起终端 → 锚点式处置信任/更新弹窗 → 以发起设备身份注入首句 → 会话物化上板并自动跳转。

**Architecture:** 服务端新增创建状态机（`inject/create.rs`，缝驱动可测：spawn/屏读/注入/确认复用 `RemoteState` 既有缝）；弹窗识别走锚点账本新增「新建会话」场景（append-only，逐字取探测取证）；三个新端点挂 `/m/api/v1`；配对不确定信号暴露到会话数据并在投递回执提示。前端看板入口 + 表单 + 进度态。**单批一次做成**，末尾统一评审停点。

**Tech Stack:** Rust（axum 端点、sysinfo 进程锚定、CONOUT$ 屏读缝）+ React/TS（`src/mobile/`）。

**上位文档（执行前必读，顺序不可换）：**
1. `AGENTS.md`（项目纪律：语言规范/构建门禁命令/扫描预算契约）
2. `docs/MASTER-PLAN.md`（宪法；本功能 = D21 + F2.8）
3. `docs/engineering/specs/2026-09-27-remote-session-create-design.md`（§4 管线语义与红线为准）
4. 本计划（任务书 SSOT）
5. `docs/research/refs/phase2-消息注入/2026-09-27-新建会话四家探测定案.md`（**锚文案逐字、键序、时延、物化判据唯一来源**——账本条目与键序常量从这里抄，禁止手造）

**接口对账记录（2026-10-02 两轮核实，执行者勿改判；发现计划与实况冲突时停下登记回报，不自行改判）：**
- `SpawnSpec` 是**枚举**（`resume.rs:53`：`Windows{program,args,cwd,new_console}` / `MacosApplescript{script}`）——env 扩展加在 Windows 变体；macOS 走脚本内联 `env K=V`（归 Mac 后补批）。
- `resume.rs::windows_terminal_path` 当前为私有 `#[cfg(windows)] fn`——C3 顺带升 `pub(crate)`。
- `Injector` 域（`engine.rs:424`）：`locate_and_inject_spec(pid, text, spec)` / `locate_and_send_key_spec(pid, key, spec)`；键域已支持单字符 `'2'`（`engine.rs control_records` is_ascii_alphanumeric 分支）。
- `RemoteState` 既有缝（`server.rs:283`）：`session_source` / `store` / `injector` / `resume_spawner: Arc<dyn Fn(SpawnSpec)->Result<(),String>>` / `confirm_probe: Arc<dyn Fn(&str,&str,&str)->bool>` / `screen_probe: Arc<dyn Fn(&str,u32)->Option<Vec<String>>>`（首参 sid 仅日志用）/ `archive_source: Box<dyn Fn()->Vec<SessionArchiveRow>>`。
- `monitor::cwd::normalize_cwd_for_match` 为 `pub(crate)`（同 crate 直用）。
- **`remote/files.rs::SENSITIVE_DIRS` 是主目录凭据清单**（.ssh/.aws/.claude/.mam/AppData/Library 等，**不含 windows/system32**），且其设计语义为「仅主目录范围内拦截」——create 域复用该表须声明为**策略扩展（全局段匹配）**并**另补系统目录清单**（C2）。其 `is_sensitive_path` 函数是 home 域语义，不复用。
- `session_archive.last_seen` 为 RFC3339 UTC（`archive.rs`）。
- 桌面审计页对 `action` **原样渲染**（`AuditLogSection.tsx:108`）——新增 `create`/`dialog` 零前端适配。
- enabledTools 数据源 = `host_source()`（settings DAO 同源）。
- `confirm::stamp_of` 内部自带 `strip_mobile_signature`——直接喂 compose 产物即可。
- `Session` 结构体被全部 adapter 构造——**不加字段**；配对标记在端点层包 JSON（C7）。

---

## §0 总则（红线，违反即返工）

1. **TDD**：每任务先失败测试再实现；纯核与状态机必须 fake 缝单测覆盖，实机验证归 `#[ignore]` E2E。
2. **门禁**：后端任务收尾 `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`；前端任务 `pnpm check && pnpm test && pnpm build:mobile`；C12 六门禁全量终跑。
3. **提交纪律**：每任务至少一个 commit，**全部本地不 push**；C12 后唯一停点：主线评审 → 用户真机验收。
4. **零接触红线**：单测用内存库（`DeviceStore::memory`）/ tempdir，绝不写真实 `~/.mam`（E2E 例外：审计/队列落真实库为验收语义，沿用 m9r_e2e 口径）；E2E 只碰自建临时目录会话，结束 `taskkill /T /F` 清场。
5. **键序红线（探测实证，测试锁死）**：claude 信任框默认 `❯ No, exit` → 必须 `down`+`enter`（直按 enter=退出）；codex 更新框默认 `Update now` → 必须字符 `'2'`（esc 无效、enter 禁用）；codex/kimi 信任框 enter 直通。
6. **锚文案来源红线**：账本 `text` 逐字取探测定案表 §4/§5/§7，`evidence` 指向 `create-probe/20260927-100552` 或 Mac 报告；缺锚先补探测，禁止手造。
7. **路径约束（v1）**：纯 ASCII（`non_ascii_path`）、本地卷（UNC 拒绝 `not_local_volume`）、黑名单（`blacklisted`，= SENSITIVE_DIRS 全局段匹配 ∪ 系统目录清单）、绝对路径、缺失递归创建（失败 `mkdir_failed`）。
8. **执行方式**：可用子代理逐任务实现，但 git commit、门禁运行、验收标准核对一律在主会话统一做；子代理不碰 git。
9. **计划冲突处置**：任何「计划写的接口/表/文案与代码实况不符」→ 停下登记进简报，不自行改判（宪法精神：冲突交裁决）。

## §1 文件结构总览

| 文件 | 动作 | 职责 | 任务 |
|---|---|---|---|
| `docs/MASTER-PLAN.md` | 改 | D21 + F2.8 | C0 |
| `src-tauri/src/inject/anchor_ledger.rs` | 改 | 四场景常量 + 账本条目 | C1 |
| `src-tauri/src/inject/create_path.rs` | 建 | 路径校验纯核（双表黑名单） | C2 |
| `src-tauri/src/remote/files.rs` | 改 | `SENSITIVE_DIRS` 升 pub(crate) | C2 |
| `src-tauri/src/inject/resume.rs` | 改 | SpawnSpec::Windows 增 env + create 规格 + wt 路径升可见 | C3 |
| `src-tauri/src/inject/create.rs` | 建 | 进程锚定 + 状态机内核 | C4/C5 |
| `src-tauri/src/remote/server.rs`、`api.rs`、`mod.rs` | 改 | 任务注册缝 + 三端点 + 审计 + 配对信号 | C6/C7 |
| `src-tauri/src/commands/session.rs` | 改 | `running_projects_from_processes` 升 pub(crate) | C7 |
| `src-tauri/tests/create_e2e.rs` | 建 | 四家实机 E2E + HTTP 端到端总测试 | C8/C9 |
| `src/mobile/api.ts` | 改 | 三方法 + 类型 | C10 |
| `src/mobile/CreateSessionSheet.tsx`、`Board.tsx` | 建/改 | 表单+进度+跳转 | C11 |
| `docs/release-notes/create-acceptance-checklist.md` | 建 | 手工验收清单 | C12 |

---

### Task C0: 宪法 D21 裁决落正文

**Files:** Modify `docs/MASTER-PLAN.md`

- [ ] **Step 1** §1.2 二期功能表末尾（F2.7 之后）追加：

```markdown
| F2.8 | 远程新建会话 | 手机端指定目录+工具（四家终端 CLI），主机起终端开新会话：锚点式处置信任/更新弹窗 → 发起设备身份注入首句 → 会话物化上板自动跳转（2026-10-02 D21 提前纳入二期收尾） |
```

- [ ] **Step 2** §2.2 二期输入输出表末尾追加：

```markdown
| F2.8 新建会话 | `/m/api/session-create`（tool、projectPath、firstMessage?） | 起终端（裸命令+DISABLE_AUTOUPDATER=1）→ 屏读锚点处置弹窗 → 首句注入（发起设备签名）→ 新会话文件物化；回执 taskId → phase 流转 → sessionId；审计 create/dialog/send 三类行 |
```

- [ ] **Step 3** 决策记录表（D20 之后）追加：

```markdown
| D21 | **远程新建会话提前为二期收尾功能**：原属三期「会话管理操作」域，因与二期注入链强耦合提前；边界=四家固定 CLI 裸命令、无任意参数，目录受黑名单+ASCII+本地卷约束，不构成「不暴露任意 shell」红线突破 | 2026-09-26 |
```

- [ ] **Step 4** Commit：`docs(charter): D21 远程新建会话提前为二期收尾（F2.8）`

**验收标准**：① 三处插入位置正确（F2.7 后 / 2.2 表末 / D20 后）；② 未动 MASTER-PLAN 其他任何行（`git diff` 仅三段新增）；③ 单独 commit。

### Task C1: 锚点账本「新建会话」四场景

**Files:** Modify `src-tauri/src/inject/anchor_ledger.rs`

- [ ] **Step 1: 失败测试**（tests 模块追加）：

```rust
#[test]
fn create_scenarios_anchors_hit_verbatim() {
    use super::detect;
    let hit = |tool: &str, sc: &str, sl: &str, line: &str| {
        detect(&[line.to_lowercase()], tool, sc, sl).is_some()
    };
    // 信任框：claude（标题 + 双选项行都在 CONFIRM 槽）
    assert!(hit("claude", scenario::CREATE_TRUST, slot::TITLE,
        "Quick safety check: Is this a project you created or one you trust?"));
    assert!(hit("claude", scenario::CREATE_TRUST, slot::CONFIRM, "❯ No, exit"));
    assert!(hit("claude", scenario::CREATE_TRUST, slot::CONFIRM, "Yes, I trust this folder"));
    // codex 双形态：Windows 0.156.1（Folder access）与 Mac 0.155.1（长问句）同槽并存
    assert!(hit("codex", scenario::CREATE_TRUST, slot::TITLE, "Folder access"));
    assert!(hit("codex", scenario::CREATE_TRUST, slot::TITLE,
        "Do you trust the contents of this directory?"));
    assert!(hit("codex", scenario::CREATE_TRUST, slot::CONFIRM, "1. Trust and continue"));
    assert!(hit("codex", scenario::CREATE_TRUST, slot::CONFIRM, "› 1. Yes, continue"));
    // kimi（per-folder）
    assert!(hit("kimi", scenario::CREATE_TRUST, slot::TITLE, "Trust this folder?"));
    // 更新框：codex 实机构造 + claude 非阻塞横幅
    assert!(hit("codex", scenario::CREATE_UPDATE, slot::TITLE, "Update available"));
    assert!(hit("codex", scenario::CREATE_UPDATE, slot::CONFIRM, "1. Update now"));
    assert!(hit("claude", scenario::CREATE_UPDATE, slot::PRESENT, "✔ Update installed · Restart to apply"));
    // idle：claude 双版本同槽多文案（2026-09-23 裁决：版本不参与筛选）
    assert!(hit("claude", scenario::CREATE_IDLE, slot::PRESENT, "? for shortcuts"));
    assert!(hit("claude", scenario::CREATE_IDLE, slot::PRESENT, "auto mode on (shift+tab to cycle)"));
    assert!(hit("codex", scenario::CREATE_IDLE, slot::PRESENT, "Ask Codex to do anything"));
    assert!(hit("kimi", scenario::CREATE_IDLE, slot::PRESENT, "No session yet"));
    assert!(hit("opencode", scenario::CREATE_IDLE, slot::PRESENT, "Ask anything"));
    // 首装不可处置态（识别 → 专属失败码）
    assert!(hit("claude", scenario::CREATE_ONBOARD, slot::TITLE, "Unable to connect to Anthropic services"));
    assert!(hit("codex", scenario::CREATE_ONBOARD, slot::TITLE, "Sign in with ChatGPT"));
    // 既有场景零影响（append-only 基线）
    assert!(!super::candidates("codex", scenario::PERMISSION_MENU, slot::TITLE).is_empty());
}
```

- [ ] **Step 2** `cargo test create_scenarios_anchors --lib` → 编译错（常量不存在）。
- [ ] **Step 3: 实现**——`scenario` 模块追加四常量（`CREATE_TRUST`/`CREATE_UPDATE`/`CREATE_IDLE`/`CREATE_ONBOARD`，wire 词 `create_trust` 等，doc 注明 spec §4.4 与探测出处）；`ANCHOR_LEDGER` 末尾追加 **18 条** `AnchorRow`：claude trust（TITLE=`quick safety check`；CONFIRM=`no, exit` + `yes, i trust this folder`）、codex trust（TITLE=`folder access` + `do you trust the contents of this directory` 双形态；CONFIRM=`1. trust and continue` + `yes, continue`）、kimi trust（TITLE=`trust this folder?`）、codex update（TITLE=`update available`；CONFIRM=`1. update now`）、claude update（PRESENT=`update installed · restart to apply`）、idle 四家五条（claude 两条同槽：`? for shortcuts` + `auto mode on (shift+tab to cycle)`；codex `ask codex to do anything`；kimi `no session yet`；opencode `ask anything`）、onboard 两条（claude `unable to connect to anthropic services`；codex `sign in with chatgpt`）。每条 `text` 小写逐字、`evidence` 指向 `create-probe/20260927-100552` 对应段或 Mac 报告 §③（**形态见 Step 1 断言的原文**）。
- [ ] **Step 4** 测试过 → 门禁 → Commit `feat(create): 锚点账本新建会话四场景（探测逐字取证，codex 双形态同槽）`

**验收标准**：① 新测试 PASS 且既有账本测试全绿（append-only 零回归）；② `ANCHOR_LEDGER` 条目数 = 原数 + 18，全部带非空 `evidence`；③ `cargo test --lib` 全量零失败。

### Task C2: 路径校验纯核（双表黑名单）

**Files:** Create `src-tauri/src/inject/create_path.rs`；Modify `src/inject/mod.rs`（挂模块）、`src/remote/files.rs`（`const SENSITIVE_DIRS` → `pub(crate) const`，调用点零改动）

- [ ] **Step 1: 失败测试**：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_empty_relative_nonascii_unc() {
        assert_eq!(validate("", "windows").err().unwrap().code, "empty");
        assert_eq!(validate(r"relative\path", "windows").err().unwrap().code, "not_absolute");
        assert_eq!(validate(r"E:\项目\demo", "windows").err().unwrap().code, "non_ascii_path");
        assert_eq!(validate("/项目/demo", "macos").err().unwrap().code, "non_ascii_path");
        assert_eq!(validate(r"\\server\share\p", "windows").err().unwrap().code, "not_local_volume");
        assert_eq!(validate("//server/share", "macos").err().unwrap().code, "not_local_volume");
    }
    #[test]
    fn windows_requires_drive_form_macos_requires_slash() {
        assert_eq!(validate(r"/tmp/x", "windows").err().unwrap().code, "bad_windows_form");
        assert!(validate(r"C:\Users\u\proj", "windows").is_ok());
        assert!(validate("/Users/u/proj", "macos").is_ok());
    }
    #[test]
    fn credential_blacklist_hits_globally() {
        // SENSITIVE_DIRS（读侧表）在 create 域升级为全局段匹配（策略扩展，文件头声明）
        assert_eq!(validate(r"D:\keys\.ssh\k", "windows").err().unwrap().code, "blacklisted");
        assert_eq!(validate("/Users/u/.claude/x", "macos").err().unwrap().code, "blacklisted");
    }
    #[test]
    fn system_dirs_blocked_via_create_table() {
        // 系统关键目录：SENSITIVE_DIRS 不含，须由 CREATE_SYSTEM_DIRS 拦
        assert_eq!(validate(r"C:\Windows\System32\x", "windows").err().unwrap().code, "blacklisted");
        assert_eq!(validate(r"C:\Program Files\app", "windows").err().unwrap().code, "blacklisted");
        assert_eq!(validate("/System/Volumes", "macos").err().unwrap().code, "blacklisted");
        assert_eq!(validate("/usr/bin", "macos").err().unwrap().code, "blacklisted");
        // 普通路径不误伤（用户目录名撞系统段的情况不在 v1 处理面，如实登记）
        assert!(validate(r"D:\work\proj", "windows").is_ok());
        assert!(validate("/Users/u/work/proj", "macos").is_ok());
    }
}
```

- [ ] **Step 2** 确认失败（模块不存在）。
- [ ] **Step 3: 实现**：

```rust
//! 新建会话路径校验纯核（spec §2）。只判不建；创建（create_dir_all）在状态机校验段执行。
//! 黑名单 = 两张表：
//! ① remote::files::SENSITIVE_DIRS（读侧凭据表）——create 域**策略扩展**为全局段匹配
//!    （读侧原语义是「仅主目录内」，见 files.rs 表注释；扩展理由：起 CLI 的目录同等敏感）；
//! ② CREATE_SYSTEM_DIRS（create 专属）——系统关键目录，读侧从不需要（低敏）但建会话必须拦。
pub struct PathReject { pub code: &'static str, pub message: String }

/// 系统关键目录（create 专属；windows=段匹配，macos=前缀匹配）
const CREATE_SYSTEM_DIRS_WIN: &[&str] = &["windows", "program files", "program files (x86)", "programdata"];
const CREATE_SYSTEM_DIRS_MAC: &[&str] = &["/system", "/library", "/private", "/bin", "/sbin", "/etc", "/usr"];

pub fn validate(path: &str, platform: &str) -> Result<(), PathReject> {
    let p = path.trim();
    if p.is_empty() { return Err(rej("empty", "路径为空")); }
    if !p.is_ascii() { return Err(rej("non_ascii_path", "v1 路径限纯 ASCII（CJK 路径留后批）")); }
    if p.starts_with(r"\\") || p.starts_with("//") {
        return Err(rej("not_local_volume", "v1 仅本地卷，UNC/网络路径不入本批"));
    }
    if platform == "windows" {
        let b = p.as_bytes();
        let drive = b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/');
        if !drive { return Err(rej("bad_windows_form", "Windows 路径须为 X:\\ 绝对形态")); }
    } else if !p.starts_with('/') {
        return Err(rej("not_absolute", "macOS 路径须为 / 开头绝对路径"));
    }
    if hits_blacklist(p, platform) { return Err(rej("blacklisted", "路径命中危险目录黑名单")); }
    Ok(())
}
fn hits_blacklist(p: &str, platform: &str) -> bool {
    let low = p.to_ascii_lowercase().replace('/', "\\");
    let seg_hit = crate::remote::files::SENSITIVE_DIRS.iter().any(|d| {
        let d = d.to_ascii_lowercase().replace('/', "\\");
        low.contains(&format!("\\{d}\\")) || low.ends_with(&format!("\\{d}"))
    });
    if seg_hit { return true; }
    if platform == "windows" {
        CREATE_SYSTEM_DIRS_WIN.iter().any(|d| {
            let d = d.to_ascii_lowercase();
            low.contains(&format!("\\{d}\\")) || low.ends_with(&format!("\\{d}"))
        })
    } else {
        CREATE_SYSTEM_DIRS_MAC.iter().any(|pfx| low.replace('\\', "/").starts_with(pfx))
    }
}
fn rej(code: &'static str, msg: &str) -> PathReject { PathReject { code, message: msg.to_string() } }
```

- [ ] **Step 4** 测试过 → 门禁 → Commit `feat(create): 路径校验纯核（凭据表全局扩展+系统目录表）`

**验收标准**：① 四个测试函数全 PASS（含 `.ssh` 全域拦截与 System32 拦截两条不同表的路径）；② `cargo test --lib` 零回归；③ files.rs 仅可见性改动（diff 一行）。

### Task C3: 起窗规格（SpawnSpec env 扩展）

**Files:** Modify `src-tauri/src/inject/resume.rs`

- [ ] **Step 1: 失败测试**（`#[cfg(windows)]` 标注——CI linux 不编译此测试）：

```rust
#[cfg(windows)]
#[test]
fn create_spawn_spec_carries_env_and_bare_command() {
    let s = build_create_spawn_spec(None, r"C:\proj", "claude");
    match s {
        SpawnSpec::Windows { program, args, env, .. } => {
            assert_eq!(program, "conhost.exe");
            assert_eq!(args, vec!["cmd", "/k", "claude"]);
            assert!(env.iter().any(|(k, v)| k == "DISABLE_AUTOUPDATER" && v == "1"));
        }
        _ => panic!("Windows 平台必须是 Windows 变体"),
    }
    let w = build_create_spawn_spec(Some(r"C:\wt\wt.exe"), r"C:\proj", "codex");
    match w {
        SpawnSpec::Windows { args, env, .. } => {
            assert_eq!(args.last().unwrap(), "codex");
            assert!(!env.is_empty());
        }
        _ => panic!(),
    }
    // resume 既有规格 env 恒空（零回归锚）
    match build_spawn_command_windows(None, r"C:\p", "claude --resume x") {
        SpawnSpec::Windows { env, .. } => assert!(env.is_empty()),
        _ => panic!(),
    }
}
```

- [ ] **Step 2** 确认失败（`env` 字段与函数均不存在）。
- [ ] **Step 3: 实现**——① `SpawnSpec::Windows` 增 `env: Vec<(String, String)>`（`build_spawn_command_windows` 两处构造补 `env: Vec::new()`）；② `spawn_terminal` Windows 分支 `Command` 追加 `.envs(spec.env.iter().map(|(k, v)| (k, v)))`；③ `windows_terminal_path` 升 `pub(crate)`（C6 用）；④ 新增：

```rust
/// 新建会话起窗规格：裸工具命令 + DISABLE_AUTOUPDATER=1（spec §4.2 环境红线；
/// S2 实测 conhost/WT 双宿主 4/4 透传，形态A=spawn 显式设 env）。
/// macOS（MacosApplescript 变体）归 Mac 后补批：脚本内联 `env K=V ` 前缀（Mac 探测 M2 实证）。
#[cfg(windows)]
pub fn build_create_spawn_spec(wt: Option<&str>, cwd: &str, tool: &str) -> SpawnSpec {
    let mut spec = build_spawn_command_windows(wt, cwd, tool);
    if let SpawnSpec::Windows { env, .. } = &mut spec {
        *env = vec![("DISABLE_AUTOUPDATER".to_string(), "1".to_string())];
    }
    spec
}
```

- [ ] **Step 4** 测试过 → 门禁 → Commit `feat(create): 起窗规格携带 DISABLE_AUTOUPDATER（SpawnSpec env 扩展+wt 路径升可见）`

**验收标准**：① 新测试 PASS（三段断言含 resume 零回归锚）；② `cargo test --lib` 全量零失败（resume 既有 spawn 测试不破）；③ linux CI 兼容（cfg(windows) 测试不编译）。

### Task C4: TUI 进程锚定

**Files:** Create `src-tauri/src/inject/create.rs`（先只放本函数）+ `mod.rs` 挂模块

- [ ] **Step 1: 失败测试**：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::cwd::normalize_cwd_for_match as norm;
    #[test]
    fn tui_candidate_matches_cwd_and_name_case_insensitive() {
        assert!(is_tui_candidate("claude", &norm(r"E:\proj\demo"), "claude.exe", &norm("e:/proj/demo")));
        assert!(is_tui_candidate("codex", &norm(r"E:\p"), "node.exe", &norm(r"E:\p")));
        assert!(!is_tui_candidate("claude", &norm(r"E:\other"), "claude.exe", &norm(r"E:\p")));
        assert!(!is_tui_candidate("kimi", &norm(r"E:\p"), "cmd.exe", &norm(r"E:\p")));
        assert!(!is_tui_candidate("kimi", &norm(r"E:\p"), "powershell.exe", &norm(r"E:\p")));
    }
}
```

- [ ] **Step 2** 确认失败 → **Step 3: 实现**：

```rust
/// TUI 进程名集合（m9r_e2e launch_cli 同口径：原生 exe / node / bun；kimi 独立 exe）
fn tui_names(tool: &str) -> Vec<String> {
    let mut v = vec![format!("{tool}.exe"), "node.exe".to_string(), "bun.exe".to_string()];
    if tool == "kimi" { v.insert(0, "kimi.exe".to_string()); }
    v
}
fn is_tui_candidate(tool: &str, proc_cwd: &str, proc_name: &str, want_cwd: &str) -> bool {
    proc_cwd == want_cwd && tui_names(tool).iter().any(|n| n.eq_ignore_ascii_case(proc_name))
}
/// 起窗后轮询发现 TUI pid（sysinfo cwd+进程名；超时 Err 中文回执）。
/// 刷新口径与 resume 效果回查同源（RefreshKind with_cmd+with_cwd Always）。
pub fn find_tui_pid(tool: &str, dir: &std::path::Path, timeout: std::time::Duration) -> Result<u32, String> {
    let want = crate::monitor::cwd::normalize_cwd_for_match(&dir.to_string_lossy());
    let t0 = std::time::Instant::now();
    loop {
        let system = sysinfo::System::new_with_specifics(
            sysinfo::RefreshKind::new().with_processes(
                sysinfo::ProcessRefreshKind::new()
                    .with_cmd(sysinfo::UpdateKind::Always)
                    .with_cwd(sysinfo::UpdateKind::Always),
            ),
        );
        for (pid, process) in system.processes() {
            if let Some(cwd) = process.cwd() {
                if is_tui_candidate(tool,
                    &crate::monitor::cwd::normalize_cwd_for_match(&cwd.to_string_lossy()),
                    &process.name().to_string_lossy(), &want) {
                    return Ok(pid.as_u32());
                }
            }
        }
        if t0.elapsed() >= timeout {
            return Err(format!("起窗后 {timeout:?} 内未发现 {tool} 进程（目录 {}）", dir.display()));
        }
        std::thread::sleep(std::time::Duration::from_millis(1200));
    }
}
```

- [ ] **Step 4** 测试过 → 门禁 → Commit `feat(create): TUI 进程锚定（cwd+进程名，中间壳排除）`

**验收标准**：① 判据测试 5 条断言全 PASS；② `find_tui_pid` 编译进 lib 且 clippy 零警告（实机验证归 C8）。

### Task C5: 创建状态机内核

**Files:** Modify `src-tauri/src/inject/create.rs`

- [ ] **Step 1: 失败测试**（RefCell 驱动 fake，跨平台可编译）：

```rust
#[cfg(test)]
mod pipeline_tests {
    use super::*;
    use std::cell::RefCell;
    struct CreateDeps<'a> {
        screen: &'a dyn Fn(u32) -> Option<Vec<String>>,
        send_key: &'a dyn Fn(u32, &str) -> Result<(), String>,
        send_text: &'a dyn Fn(u32, &str) -> Result<(), String>,
    }
    /// 屏读按调用序弹 screens；键序从 outcome.keys_sent 断言（无需外部桥接）
    fn deps(screens: Vec<Vec<String>>, text_called: Option<&std::cell::Cell<bool>>) -> CreateDeps<'static> {
        let screens: &'static RefCell<std::vec::IntoIter<Vec<String>>> =
            Box::leak(Box::new(RefCell::new(screens.into_iter())));
        let text_flag: &'static std::cell::Cell<bool> = Box::leak(Box::new(std::cell::Cell::new(false)));
        if let Some(c) = text_called { /* 外部观察用同一 Cell 引用 */ }
        CreateDeps {
            screen: &|_pid| screens.borrow_mut().next(),
            send_key: &|_, _| Ok(()),
            send_text: &|_, _| { text_flag.set(true); Ok(()) },
        }
    }
    fn params(tool: &str) -> Params { Params { tool: tool.into(), dir: "C:\\t".into(),
        first_message: "hi".into(), composed: "hi [mobile test-dev]".into() } }

    #[test]
    fn claude_trust_requires_down_then_enter() {
        let d = deps(vec![
            vec!["Quick safety check: Is this a project you created or one you trust?".into(), "❯ No, exit".into()],
            vec!["Quick safety check".into()],
            vec!["manual mode on · ? for shortcuts".into()],
        ], None);
        let out = run_pipeline(&d, &params("claude"));
        assert!(matches!(out.status, CreateStatus::WaitingMaterialize));
        assert_eq!(out.keys_sent, vec!["down", "enter"]); // 红线：直按 enter=退出
    }
    #[test]
    fn codex_update_then_trust_with_char2() {
        let d = deps(vec![
            vec!["Update available".into(), "1. Update now (runs npm install -g @openai/codex)".into(), "2. Skip".into()],
            vec!["Update available".into()],
            vec!["Folder access".into(), "Trust this folder?".into(), "1. Trust and continue".into()],
            vec!["Ask Codex to do anything".into()],
        ], None);
        let out = run_pipeline(&d, &params("codex"));
        assert!(matches!(out.status, CreateStatus::WaitingMaterialize));
        assert_eq!(out.keys_sent, vec!["2", "enter"]); // '2'=Skip（esc 无效、enter 禁用）
    }
    #[test]
    fn unrecognized_screen_fails_without_any_key() {
        let d = deps((0..16).map(|_| vec!["某种未识别界面".to_string()]).collect(), None);
        let out = run_pipeline(&d, &params("claude"));
        assert!(matches!(out.status, CreateStatus::Failed { code, .. } if code == "unrecognized_screen"));
        assert!(out.keys_sent.is_empty()); // 不盲打
    }
    #[test]
    fn onboard_screen_fails_with_specific_code() {
        let d = deps((0..2).map(|_| vec!["Unable to connect to Anthropic services".into()]).collect(), None);
        let out = run_pipeline(&d, &params("claude"));
        assert!(matches!(out.status, CreateStatus::Failed { code, .. } if code == "network_wall"));
        let d2 = deps((0..2).map(|_| vec!["Sign in with ChatGPT".into()]).collect(), None);
        let o2 = run_pipeline(&d2, &params("codex"));
        assert!(matches!(o2.status, CreateStatus::Failed { code, .. } if code == "login_wall"));
    }
    #[test]
    fn first_message_blocked_until_idle_anchor() {
        let flag = std::cell::Cell::new(false);
        // 屏幕停在信任框 → 阶段窗耗尽 failed；text 注入恒未发生
        let screens: &'static RefCell<std::vec::IntoIter<Vec<String>>> =
            Box::leak(Box::new(RefCell::new((0..16).map(|_| vec!["Quick safety check".to_string()]).collect::<Vec<_>>().into_iter())));
        let f: &'static std::cell::Cell<bool> = Box::leak(Box::new(flag));
        let d = CreateDeps {
            screen: &|_p| screens.borrow_mut().next(),
            send_key: &|_, _| Ok(()),
            send_text: &|_, _| { f.set(true); Ok(()) },
        };
        let out = run_pipeline(&d, &params("kimi"));
        assert!(matches!(out.status, CreateStatus::Failed { .. }));
        assert!(!flag.get());
    }
}
```

- [ ] **Step 2** 确认失败 → **Step 3: 实现**：

```rust
pub enum CreateStatus {
    OpeningTerminal, DialogHandling, InjectingFirst, WaitingMaterialize,
    Done { session_id: String },
    Failed { phase: &'static str, code: String, message: String },
}
pub struct Params { pub tool: String, pub dir: PathBuf, pub first_message: String, pub composed: String }
pub struct CreateOutcome { pub status: CreateStatus, pub keys_sent: Vec<String>,
    pub dialog_log: Vec<(String, Vec<String>)> } // (场景, 键序) 供审计逐条留痕

/// 弹窗处置键序（探测定案 §4/§5）：账本管「认屏」，键序是引擎域常量（与 approve 映射同构）
fn disposal_keys(tool: &str, scenario: &str) -> Vec<&'static str> {
    match (tool, scenario) {
        ("claude", s) if s == scenario::CREATE_TRUST => vec!["down", "enter"],
        ("codex", s) if s == scenario::CREATE_UPDATE => vec!["2"],
        ("codex", _) | ("kimi", _) => vec!["enter"],
        _ => vec!["enter"],
    }
}
/// 状态机主循环（纯内核 + 缝）。语义：
/// ① 2s 步距屏读（deps.screen），None 计一次未识别；连续 15 轮（30s）无任何锚命中
///    → Failed{code:"unrecognized_screen"}，不发任何键；
/// ② CREATE_ONBOARD 命中即 Failed（claude→network_wall / codex→login_wall，其余工具
///    按文案归属），不发键；
/// ③ CREATE_UPDATE 的 PRESENT 形态（claude 横幅）不处置不阻断（探测定案：非阻塞）；
///    TITLE+CONFIRM 形态（codex 框）按 disposal_keys 处置，处置前重读确认场景在场；
/// ④ CREATE_IDLE/PRESENT 命中 → send_text(composed)（一次）→ WaitingMaterialize；
/// ⑤ 发键间隔 1.5s、发后 2.5s 沉降（探测 P3a 实测处置→idle 1–4.5s）；
/// ⑥ keys_sent/dialog_log 全程记录。物化轮询不在本层（归 C6 调用层）。
pub fn run_pipeline(deps: &CreateDeps, p: &Params) -> CreateOutcome { /* 按上述六条语义实现 */ }
```

- [ ] **Step 4** 测试过 → 门禁 → Commit `feat(create): 创建状态机内核（锚点驱动处置+idle 门+首句注入）`

**验收标准**：① 五个测试全 PASS（含键序红线两条与「不盲打」两条）；② 状态机为纯内核零 IO（除缝调用）；③ clippy 零警告。

### Task C6: API 三端点 + 任务注册表 + 审计

**Files:** Modify `src-tauri/src/remote/server.rs`（RemoteState + 路由）、`api.rs`、`mod.rs`；`src-tauri/src/inject/create.rs`（物化发现）

- [ ] **Step 1: 失败端点测试**（server.rs 既有内存库 + fake 缝模式，六组断言）：

```rust
#[test]
fn session_create_contract() {
    // ① 非法路径 → 400 {error:"bad_request", reasonCode:"non_ascii_path"}
    // ② 未装/未启用工具 → 400 reasonCode:"tool_unavailable"
    // ③ 合法 → 200 {taskId}；任务在飞（非终态）第二个请求 → 409 {error:"conflict"}
    // ④ status：未知 taskId → 404；在飞 → {phase, detail?}；终态任务可查不占单飞额度
    // ⑤ 审计三类行：action=create（result=ok / failed:<code>）、action=dialog
    //    （summary=场景+键序）、action=send（首句，device_name=发起设备 cookie 名）
    // ⑥ create-projects：archive fake 给 7 天内/8 天前/非 ASCII 三条 + session fake 一条活跃
    //    → 响应只含 7 天内与活跃项（去重、lastActiveAt 排序、字段
    //    {path, lastActiveAt, tools[], activeTools[]}）
}
```

- [ ] **Step 2** 确认失败 → **Step 3: 实现**：
  - `RemoteState` 增 `pub create_tasks: Arc<Mutex<HashMap<u64, CreateTaskShared>>>`（自增 id；`CreateTaskShared{phase, detail, session_id, spawned_pid}`）；STATE 装配空表。
  - **工具门** `tool_available(tool)`：enabledTools（`host_source()` 同源）∩ 安装探测（PATH 目录扫描 `tool.exe`，OnceLock 缓存）。
  - **POST /session-create**：校验（工具门→`create_path::validate`→`create_dir_all` 失败 400 `mkdir_failed`）→ 单飞 409（仅非终态占用）→ `spawn_blocking` 管线：`build_create_spawn_spec(resume::windows_terminal_path().ok(), dir, tool)` → `st.resume_spawner` → `find_tui_pid`（30s）→ `run_pipeline`（缝：screen=`st.screen_probe("", pid)`、key/text=`st.injector.locate_and_send_key_spec`/`locate_and_inject_spec` + `families::family_for(tool)`（None→`FALLBACK_SPEC` 口径，登记日志））→ 物化：`discover_new_session(tool, dir, since)`（claude=项目 slug 目录 mtime>since 最新 jsonl 取 sid；codex=今日 rollout 且首行 cwd 匹配；kimi=sessions 下 wd_* 新目录 session id；opencode=**拷 db+wal+shm 三件套**查 session 表 by directory——P4 红线）→ `st.confirm_probe(tool, sid, stamp)`（stamp=`confirm::stamp_of(&composed)`，内部自剥签名）命中 → Done；审计三类行（物化前 session_id 传 `""`；失败行 result=`failed:<code>`；Done 附 codex hooks 信任提示进 detail——读 T5 信号健康度同源状态，无现成查询口则读 settings KV 一行判断，不新建机制）。
  - **GET /session-create/status**：快照 `{phase, detail?, sessionId?}`；未知 404。
  - **GET /create-projects?days=**：session_source ∪ archive_source（RFC3339 过 N 天），过 ASCII 检查，去重排序，`activeTools` 由快照同目录活跃会话聚合。
- [ ] **Step 4** 端点测试过 → fmt/clippy/test → Commit `feat(create): session-create 三端点+内存任务态+审计三类行`

**验收标准**：① 六组断言全 PASS；② 单飞语义=仅非终态占用（done/failed 后可再建）；③ 审计三类行在内存库可查且设备名正确；④ 常规门禁零回归。

### Task C7: 配对不确定信号暴露

**Files:** Modify `src-tauri/src/commands/session.rs`（`running_projects_from_processes` 升 `pub(crate)`，桌面门逻辑零改动）、`src-tauri/src/remote/api.rs`

- [ ] **Step 1: 失败测试**：/sessions 响应每会话含 `pairingAmbiguous`（fake 快照两条同工具同项目 + 进程计数 ≥2 → true；单会话 → false）；session-send 成功回执追加 `pairingHint`（同态 true，**不拦截**，delivered 语义不变）。
- [ ] **Step 2: 实现**：api.rs 增 `fn pairing_ambiguous_set() -> HashSet<(String, String)>`（sysinfo 快照 → `running_projects_from_processes` 计数 ≥2 → (tool, project_name) 小写归一——与桌面门 require_evidence 同判据）；**不动 `Session` 结构体**——/sessions handler 序列化后按 set 打标（serde_json::Value 层附加字段，零 adapter 触碰）；session_send 成功回执 JSON 追加 `pairingHint`。
- [ ] **Step 3** 测试过 → 门禁 → Commit `feat(create): 配对不确定信号暴露（端点层打标+投递回执提示，不拦截）`

**验收标准**：① 两条断言 PASS；② `Session` 结构体与其全部构造点零改动（`git diff` 证明）；③ 桌面跳转门行为零变化（既有 commands/session.rs 测试全绿）。

### Task C8: 四家实机 E2E

**Files:** Create `src-tauri/tests/create_e2e.rs`（全 `#[ignore]`；复用 m9r_e2e 的 Ev/清场纪律；不依赖 MAM dev——直调生产内核 + 真缝）

- [ ] **Step 1: 写 E2E**：

```rust
/// 四家矩阵：全新 temp 目录 → build_create_spawn_spec 起窗 → find_tui_pid →
/// run_pipeline（真屏读/真注入缝）→ discover_new_session → confirm stamp →
/// 断言会话文件首条用户消息 == "hi [mobile <名>]"（compose 签名逐字）→ taskkill 清场。
/// 附加断言：claude 轮 keys_sent == [down, enter]；codex 双框轮 == [2, enter]（键序红线实机复核）。
#[test] #[ignore = "实机显式跑"] fn e2e_create_matrix_four_tools() { /* … */ }
```

- [ ] **Step 2** 实机跑 `cargo test --test create_e2e -- --ignored --nocapture --test-threads=1` 四家全过；实跑台账写文件头注释（m9r_e2e 同款：日期/耗时/断言/证据目录）。可选辅助：computer-use 对终端窗口目检/截图佐证存证据目录。
- [ ] **Step 3** Commit `test(create): 四家新建全链实机 E2E + 台账`

**验收标准**：① 四家全 PASS（信任框路径真实触发——全新目录保证）；② keys_sent 实机断言与键序红线一致；③ 会话文件首条用户消息逐字节等于 compose 产物；④ 清场零残留（taskkill 树 + 证据留存）。

### Task C9: HTTP 端到端总测试（产品路径全链）

**Files:** Modify `src-tauri/tests/create_e2e.rs`

- [ ] **Step 1: 写用例**（`#[ignore]`；模式 = m9r_e2e 的 http-full-chain：内存库起 axum + 真注入器/真 spawner）：

```rust
/// 端到端总测试（移动端等效路径）：临时目录 → POST /m/api/v1/session-create（带设备
/// cookie）→ 轮询 /session-create/status 至 done{sessionId} → GET /sessions 含新会话卡
/// （pairingAmbiguous=false）→ POST /session-send 第二条消息 delivered → 审计五类行可查
/// （create + dialog + send×2 + flush）→ taskkill 清场。
#[test] #[ignore = "实机显式跑"] fn e2e_create_http_full_chain() { /* … */ }
```

- [ ] **Step 2** 实机跑通过 → Commit `test(create): HTTP 端到端总测试（创建→物化→上板→二次投递→审计全链）`

**验收标准**：① 全链一次通过（不做分段 mock）；② sessionId 三处一致（status 回执 / sessions 卡 / 会话文件）；③ 审计五行齐且第二句 send 的设备名同 cookie；④ 清场零残留。

### Task C10: 移动端 API 客户端

**Files:** Modify `src/mobile/api.ts`
- [ ] 失败测试（vitest，mock fetch）→ 实现 `fetchCreateProjects(days)` / `createSession(body)` / `fetchCreateStatus(taskId)`（409/404/400 reasonCode 分支类型化）→ `pnpm test` 过 → Commit。

**验收标准**：① 三方法 + 类型齐；② 409/404/400-reasonCode 三分支各有测试且 PASS；③ `pnpm check` 零新告警。

### Task C11: 新建会话 UI（表单 + 进度 + 跳转）

**Files:** Create `src/mobile/CreateSessionSheet.tsx`；Modify `src/mobile/Board.tsx`（头部操作区「+」入口）、`mobile.css`（如需）
- [ ] 失败测试（vitest）→ 实现三段：
  1. **表单**：工具四选（host.enabledTools 置灰未启用项并标原因）；目录候选下拉（create-projects，显示 lastActiveAt+tools）+ 手填切换（前端形态提示，服务端权威）；首句默认占位 `hi`；**黄字**：选中 tool ∈ 该项目 `activeTools` →「该项目已有该工具的活跃会话，多实例下后续消息路由可能混淆」（≥1 语义，与 pairingAmbiguous 分层）。
  2. **进度态**：phase → 中文（开终端/处置弹窗/注入首句/等待上板），2s 轮询；`done.sessionId` → 看板快照见新卡后跳 SessionDetail（复用既有导航）；`failed` → 分阶段原因 + 重试；404 →「任务已失效（主机可能重启），请重试」。
  3. 409 →「已有创建任务进行中」。
- [ ] `pnpm check && pnpm test && pnpm build:mobile` → Commit。

**验收标准**：① 黄字按 activeTools 触发（vitest 断言 ≥1 语义）；② phase 全五态中文映射有测试；③ done→跳转路径复用既有导航（不新造路由）；④ 三门禁过。

### Task C12: 验收清单 + 六门禁终跑 + 交付停点

**Files:** Create `docs/release-notes/create-acceptance-checklist.md`
- [ ] 清单覆盖 spec §7 全部人工项：四家全链（含信任框/更新提示路径）、未识别界面报错、多实例黄字、路径边界（黑名单两表/盘不存在/深层创建/非 ASCII/UNC）、审计三类行逐条、**用户真机手机走查**（新建→进度→跳转→收发）。
- [ ] **六门禁终跑**：cargo fmt --check / clippy --all-targets -D warnings / cargo test（lib+main+dao+linker+preset_v2 基线口径）/ pnpm check / pnpm test / pnpm build:mobile；E2E 两例（C8/C9）复跑记录。
- [ ] Commit → **唯一停点**：交付简报（任务×commit×门禁×验收标准达成表 + E2E 实跑数字 + 自裁决清单 + 计划冲突登记）→ 主线评审 → 用户真机验收 → 统一 push。

**验收标准**：① 清单文件覆盖上述全项且每项带「预期/实测/证据」三栏；② 六门禁全绿（preset_v2 恒定 5 败基线除外，沿用既有口径）；③ 简报齐四件套；④ 未 push（`git status` 本地领先 N 提交）。

---

## Self-Review（2026-10-02 三轮审计记录）

- **二轮修正**：SpawnSpec 枚举事实 / is_sensitive_path home 域语义 / normalize 可见性 / RFC3339 / codex 信任框双形态同槽 / s_CREATE_UPDATE 笔误 / CreateOutcome 统一。
- **三轮修正（本轮）**：① C2 原测试断言 `C:\Windows\System32` 命中黑名单——实况 `SENSITIVE_DIRS` 无系统目录且语义限主目录内 → 双表设计（凭据表全局扩展声明 + CREATE_SYSTEM_DIRS）+ 对应测试拆两条；② C3 测试补 `#[cfg(windows)]`（CI linux 兼容）+ resume 零回归锚 + `windows_terminal_path` 升可见挂 C3；③ C5 测试辅助改为 RefCell 泄漏式（原 `&dyn Fn` 捕获迭代器不可编译，外部 keys 桥接逻辑有 bug——统一从 `out.keys_sent` 断言）；④ C6 stamp 措辞（stamp_of 自剥签名）/ family_for None 回落 FALLBACK_SPEC 登记；⑤ C7 明确不动 Session 结构体（端点层 Value 打标）；⑥ 每任务补「验收标准」块；⑦ C8/C12 登记 computer-use 可选辅助。
- **Spec 覆盖**：§2→C6/C10/C11；§3→C6（phase 无 validating 已同步 spec）；§4.1–4.8→C2/C3/C4/C5/C6；§5→C7；§6→C0；§7→C8/C9/C12。
- **占位扫描**：`run_pipeline` 体内为六条语义齐备的实现填写型骨架；`find_tui_pid` 全码已给；E2E 两例为断言齐备骨架（复用 m9r_e2e 成熟模式）；其余步骤均含完整代码或逐字内容。
- **类型一致性**：`CreateStatus`/`CreateOutcome{status, keys_sent, dialog_log}`/`Params{composed}`/`PathReject.code`（empty/not_absolute/non_ascii_path/not_local_volume/bad_windows_form/blacklisted）在 C2/C5/C6/C11 间一致；spec §2 原因码一一对应（`mkdir_failed`/`tool_unavailable` 为端点层码）。
