# OpenCode/OpenClaw 会话匹配 + Windows 窗口跳转 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 实施两份已批准的 spec——`specs/006-opencode-openclaw-session-matching/spec.md`（OpenCode/OpenClaw 会话匹配修复）与 `specs/007-windows-window-jump/spec.md`（Windows 窗口级跳转 + SessionCard i18n）。

**Architecture:** 匹配侧：归一化函数增强为统一分隔符方向（`\`→`/`），OpenCode 改为按 `session.directory` 主匹配 + worktree 前缀回退，OpenClaw 接入同一归一化。跳转侧：新增 `window/win32.rs`，用"进程祖先链收集 PID → EnumWindows 找链上进程的可见顶层窗口 → SW_RESTORE + SetForegroundWindow"统一聚焦 CLI 终端与 ChatGPT App 窗口；`jump_supported_for` 在 Windows 放开两种形态；SessionCard 文案统一并接入 i18n。

**Tech Stack:** Rust (Tauri 2)、sysinfo 0.32、windows 0.57（已在依赖树中，新增为直接依赖）、i18next。

---

## 背景（执行者必读）

先完整阅读两份 spec：`specs/006-opencode-openclaw-session-matching/spec.md` 和 `specs/007-windows-window-jump/spec.md`。关键取证结论（实机验证过的事实）：

- OpenCode 的 SQLite（`~/.local/share/opencode/opencode.db`）存**正斜杠**路径（`E:/LLMproject/...`），sysinfo 进程 cwd 是**反斜杠 + 尾部分隔符**（`E:\LLMproject\...\`）——现有 `normalize_cwd_for_match` 只去尾杠 + Windows 小写，没统一分隔符方向
- 实机场景：用户在 `E:/LLMproject/deepseek-harness` 启动 opencode，project.worktree 是嵌套子目录（cwd 是 worktree 祖先），现有 worktree 规则匹配不上；而 `session.directory` 与进程 cwd 精确对应
- ChatGPT 桌面版（MSIX 安装于 `WindowsApps\OpenAI.Codex_...\app\`）内嵌的 `codex.exe` 父链包含 `ChatGPT.exe` 主进程——CLI（父链含终端宿主）与 App（父链含 ChatGPT 主进程）可用同一套"祖先链找窗口"逻辑聚焦
- windows crate 0.57.0 已在 Cargo.lock（tauri 传递依赖），直接加为 `[target.'cfg(windows)'.dependencies]` 不会引入版本冲突
- i18n 现状：zh/en 各 93 键完全对齐，顶层命名空间为 `app/greet/about/theme/language/tray/updater/settings/releaseVersion`，本计划新增 `sessions` 命名空间，**两语言键集必须保持对齐**

**环境**：执行机 Windows（Git Bash），cargo 命令在 `src-tauri/` 下执行。若依赖下载报 TLS 错误（SteamTools MITM），后台启动镜像：`python C:\Users\bunny\AppData\Local\Temp\dsh-cargo-http-mirror.py 8765`（依赖已在 `~/.cargo` 缓存时通常无需）。

**约束**：macOS 零回归（TTY 链路代码原样搬入 cfg 块，不改逻辑）；Unix 下 cwd 大小写归一化绝不启用。

---

### Task 1: `normalize_cwd_for_match` 增强（分隔符统一 + pub(crate)）

**Files:**
- Modify: `src-tauri/src/monitor/parser.rs:153` 附近（函数本体 + 既有测试 `normalize_cwd_tests`）

- [ ] **Step 1: 更新既有测试为新语义（先改测试）**

`normalize_cwd_tests` 中 `trims_trailing_separators` 与 `unix_paths_trim_only` 的断言统一改为期望正斜杠输出：

```rust
    #[test]
    fn trims_trailing_separators() {
        // Windows 下 sysinfo 返回的 cwd 带尾部反斜杠；分隔符统一为正斜杠
        let expected = if cfg!(windows) { "e:/x/y" } else { "E:/x/y" };
        assert_eq!(normalize_cwd_for_match("E:\\x\\y\\"), expected);
        assert_eq!(normalize_cwd_for_match("E:\\x\\y"), expected);
        // 反斜杠与正斜杠输入等价（OpenCode db 存正斜杠，sysinfo 存反斜杠）
        assert_eq!(
            normalize_cwd_for_match("E:\\x\\y\\"),
            normalize_cwd_for_match("E:/x/y/")
        );
    }

    #[test]
    fn unix_paths_trim_only() {
        let expected = if cfg!(windows) { "/users/x/proj" } else { "/Users/x/proj" };
        assert_eq!(normalize_cwd_for_match("/Users/x/proj/"), expected);
        // Unix 路径中的反斜杠（罕见）也被统一为正斜杠，两侧同规不影响相等性
        assert_eq!(normalize_cwd_for_match("/Users/x/proj\\"), expected);
    }
```

（`drive_letter_case_normalized_on_windows_only` 与 `root_normalizes_to_empty` 两个测试不需要改动。）

- [ ] **Step 2: 运行测试确认失败**

```bash
cd src-tauri && cargo test normalize_cwd_tests
```

预期：`trims_trailing_separators` FAIL（当前实现保留反斜杠）。

- [ ] **Step 3: 修改实现**

将（当前 parser.rs:153 附近）：

```rust
fn normalize_cwd_for_match(cwd: &str) -> String {
    let trimmed = cwd.trim_end_matches(['/', '\\']);
    if cfg!(windows) {
        trimmed.to_lowercase()
    } else {
        trimmed.to_string()
    }
}
```

替换为：

```rust
pub(crate) fn normalize_cwd_for_match(cwd: &str) -> String {
    let trimmed = cwd.trim_end_matches(['/', '\\']).replace('\\', "/");
    if cfg!(windows) {
        trimmed.to_lowercase()
    } else {
        trimmed
    }
}
```

（同时更新函数上方文档注释，追加一行：`/// - 统一分隔符为正斜杠（OpenCode db 存正斜杠、sysinfo cwd 存反斜杠，两侧同规归一化后可比）`）

- [ ] **Step 4: 运行测试确认通过 + 全量回归**

```bash
cd src-tauri && cargo test
```

预期：全部 PASS（Claude/Codex 既有测试不受影响——两侧同规归一化不改变相等性）。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/monitor/parser.rs
git commit -m "feat(monitor): unify path separators in cwd normalization"
```

---

### Task 2: `cwd_equivalent` 纯函数

**Files:**
- Modify: `src-tauri/src/monitor/parser.rs`（函数放 `normalize_cwd_for_match` 下方；测试追加到 `normalize_cwd_tests` 平级）

- [ ] **Step 1: 写失败测试**

在 `parser.rs` 测试区域追加：

```rust
#[cfg(test)]
mod cwd_equivalent_tests {
    use super::cwd_equivalent;

    #[test]
    fn separator_direction_and_trailing_are_equivalent() {
        assert!(cwd_equivalent("E:\\LLMproject\\x\\", "E:/LLMproject/x"));
        assert!(cwd_equivalent("e:/llmproject/x", "E:\\LLMproject\\x\\"));
    }

    #[test]
    fn case_rules_follow_platform() {
        if cfg!(windows) {
            assert!(cwd_equivalent("E:/X", "e:/x"));
        } else {
            assert!(!cwd_equivalent("/Users/X", "/Users/x"));
        }
    }

    #[test]
    fn different_paths_are_not_equivalent() {
        assert!(!cwd_equivalent("E:/a", "E:/b"));
        assert!(!cwd_equivalent("E:/a", "E:/a/sub"));
        assert!(!cwd_equivalent("", "E:/a"));
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

```bash
cd src-tauri && cargo test cwd_equivalent_tests
```

预期：编译失败，`cannot find function cwd_equivalent`。

- [ ] **Step 3: 实现函数**

在 `normalize_cwd_for_match` 下方插入：

```rust
/// 判断两个 cwd 字符串归一化后是否指向同一目录（用于进程 cwd ↔ 会话 directory 匹配）
pub(crate) fn cwd_equivalent(a: &str, b: &str) -> bool {
    normalize_cwd_for_match(a) == normalize_cwd_for_match(b)
}
```

- [ ] **Step 4: 运行测试确认通过**

```bash
cd src-tauri && cargo test cwd_equivalent_tests
```

预期：3 个测试 PASS。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/monitor/parser.rs
git commit -m "feat(monitor): add cwd_equivalent helper"
```

---

### Task 3: OpenCode 匹配重构（directory 主匹配 + worktree 回退）

匹配层级重排：① `session.directory` 归一化精确匹配（含 global 会话，取代原 global SQL 回退）；② project worktree 前缀匹配（归一化后）。Session 构造去重为 `build_session_from_row` 共用函数。

**Files:**
- Modify: `src-tauri/src/monitor/opencode_parser.rs`

- [ ] **Step 1: 重写 `get_opencode_sessions` 的匹配主体**

将文件顶部 import 区补充（与其他 use 并列）：

```rust
use super::parser::{cwd_equivalent, normalize_cwd_for_match};
```

将 `get_opencode_sessions` 中"cwd -> process 映射"到函数结尾的整个匹配段（从 `// cwd -> process 映射` 注释起，到 `info!` 之前）替换为：

```rust
    // 归一化 cwd -> process 映射（统一分隔符、去尾部、Windows 下小写）
    let mut cwd_to_process: HashMap<String, &AgentProcess> = HashMap::new();
    for process in processes {
        if let Some(cwd) = &process.cwd {
            cwd_to_process.insert(normalize_cwd_for_match(&cwd.to_string_lossy()), process);
        }
    }

    // 最近会话行（主匹配数据源，归一化比较在 Rust 侧做）
    let recent: Vec<(String, String, Option<String>, i64)> = conn
        .prepare("SELECT id, directory, title, time_updated FROM session ORDER BY time_updated DESC LIMIT 200")
        .ok()
        .map(|mut stmt| {
            stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .ok()
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
        })
        .unwrap_or_default();

    let mut sessions = Vec::new();
    let mut matched_pids: HashSet<u32> = HashSet::new();

    // ---- 主匹配：session.directory（会话启动目录）与进程 cwd 归一化相等 ----
    // （含 global 会话；取代原按 directory 精确 SQL 的 global 回退——SQL 精确匹配无法
    //   处理分隔符/大小写差异，归一化比较必须在 Rust 侧做）
    for process in processes {
        let Some(cwd) = &process.cwd else { continue };
        let cwd_str = cwd.to_string_lossy();
        if let Some((session_id, directory, title, time_updated)) = recent
            .iter()
            .find(|(_, dir, _, _)| cwd_equivalent(dir, &cwd_str))
        {
            matched_pids.insert(process.pid);
            if let Some(session) = build_session_from_row(
                &conn,
                session_id,
                directory,
                title.as_deref(),
                None,
                *time_updated,
                process,
            ) {
                sessions.push(session);
            }
        }
    }

    // ---- 回退匹配：project worktree 前缀（进程 cwd 等于或在 worktree 之下）----
    let projects: Vec<(String, String, Option<String>)> = conn
        .prepare("SELECT id, worktree, name FROM project WHERE id != 'global'")
        .ok()
        .map(|mut stmt| {
            stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })
            .ok()
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
        })
        .unwrap_or_default();

    for (project_id, worktree, name) in &projects {
        let wt = normalize_cwd_for_match(worktree);
        let matching_process = cwd_to_process
            .iter()
            .find(|(cwd, proc)| {
                !matched_pids.contains(&proc.pid)
                    && (*cwd == wt.as_str() || cwd.starts_with(&format!("{}/", wt)))
            })
            .map(|(_, p)| *p);

        if let Some(process) = matching_process {
            debug!(
                "OpenCode project {} matched to pid={}",
                worktree, process.pid
            );
            matched_pids.insert(process.pid);
            if let Some(session) =
                get_latest_session_for_project(&conn, project_id, name.as_deref(), process)
            {
                sessions.push(session);
            }
        }
    }
```

（`info!` 收尾保持不变。）

- [ ] **Step 2: 提取 `build_session_from_row` 并改造 `get_latest_session_for_project`**

将 `get_latest_session_for_project` 整个函数替换为：

```rust
/// 获取项目的最新会话
fn get_latest_session_for_project(
    conn: &Connection,
    project_id: &str,
    project_name: Option<&str>,
    process: &AgentProcess,
) -> Option<Session> {
    let (session_id, directory, title, time_updated) = conn
        .prepare("SELECT id, directory, title, time_updated FROM session WHERE project_id = ? ORDER BY time_updated DESC LIMIT 1")
        .ok()?
        .query_row([project_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .ok()?;

    build_session_from_row(
        conn,
        &session_id,
        &directory,
        title.as_deref(),
        project_name,
        time_updated,
        process,
    )
}

/// 由会话行构造 Session（主匹配与项目匹配共用）
fn build_session_from_row(
    conn: &Connection,
    session_id: &str,
    directory: &str,
    title: Option<&str>,
    project_name_override: Option<&str>,
    time_updated: i64,
    process: &AgentProcess,
) -> Option<Session> {
    let (last_role, last_message) = get_last_message_info(conn, session_id);
    let last_msg_time = get_last_message_time(conn, session_id);

    let status = determine_opencode_status(
        process.cpu_usage,
        last_role.as_deref(),
        last_msg_time,
        time_updated,
    );
    let last_activity_at = ms_to_iso(time_updated);

    let title = title.unwrap_or("").to_string();
    let project_name = project_name_override
        .map(String::from)
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| {
            directory
                .rsplit(['/', '\\'])
                .find(|s| !s.is_empty())
                .unwrap_or("Unknown")
                .to_string()
        });
    let display_message = last_message.or_else(|| {
        if !title.is_empty() {
            Some(title.clone())
        } else {
            None
        }
    });

    Some(Session {
        id: session_id.to_string(),
        agent_type: AgentType::OpenCode,
        project_name,
        project_path: directory.to_string(),
        git_branch: None,
        github_url: None,
        status,
        last_message: display_message,
        last_message_role: last_role,
        last_activity_at,
        pid: process.pid,
        cpu_usage: process.cpu_usage,
        active_subagent_count: 0,
        form: process.form,
        jump_supported: jump_supported_for(process.form),
        title: Some(title),
    })
}
```

- [ ] **Step 3: 删除 `get_global_session`**

整个 `get_global_session` 函数删除（其功能已被主匹配覆盖，且原 SQL 精确匹配在 Windows 上本就无法命中）。确认文件中不再有对它的调用。

- [ ] **Step 4: 编译与全量回归**

```bash
cd src-tauri && cargo test && cargo clippy --all-targets -- -D warnings
```

预期：全部 PASS，无 dead_code 警告。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/monitor/opencode_parser.rs
git commit -m "fix(monitor): opencode session matching by directory with normalized cwd"
```

---

### Task 4: OpenClaw 匹配接入归一化

**Files:**
- Modify: `src-tauri/src/monitor/openclaw_parser.rs:58-88`

- [ ] **Step 1: 修改映射构建与 workspace 比较**

文件 import 区补充：

```rust
use super::parser::normalize_cwd_for_match;
```

将（当前 58-64 行）：

```rust
    // cwd -> process 映射
    let mut cwd_to_process: HashMap<String, &AgentProcess> = HashMap::new();
    for process in processes {
        if let Some(cwd) = &process.cwd {
            cwd_to_process.insert(cwd.to_string_lossy().to_string(), process);
        }
    }
```

替换为：

```rust
    // cwd -> process 映射（归一化：统一分隔符、去尾部、Windows 下小写）
    let mut cwd_to_process: HashMap<String, &AgentProcess> = HashMap::new();
    for process in processes {
        if let Some(cwd) = &process.cwd {
            cwd_to_process.insert(normalize_cwd_for_match(&cwd.to_string_lossy()), process);
        }
    }
```

将（当前 76-79 行）：

```rust
        let matching_process = cwd_to_process
            .iter()
            .find(|(cwd, _)| *cwd == workspace || cwd.starts_with(&format!("{}/", workspace)))
            .map(|(_, p)| *p);
```

替换为：

```rust
        let ws = normalize_cwd_for_match(workspace);
        let matching_process = cwd_to_process
            .iter()
            .find(|(cwd, _)| **cwd == ws || cwd.starts_with(&format!("{}/", ws)))
            .map(|(_, p)| *p);
```

（90-104 行的默认 agent 回退段不改。）

- [ ] **Step 2: 编译与回归**

```bash
cd src-tauri && cargo test
```

预期：全部 PASS。

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/monitor/openclaw_parser.rs
git commit -m "fix(monitor): openclaw workspace matching with normalized cwd"
```

---

### Task 5: windows crate 依赖 + 祖先链收集纯函数

**Files:**
- Modify: `src-tauri/Cargo.toml`
- Create: `src-tauri/src/window/win32.rs`

- [ ] **Step 1: 添加依赖**

在 `src-tauri/Cargo.toml` 的 `[target.'cfg(windows)'.dependencies]` 段（junction 所在段）追加一行：

```toml
windows = { version = "0.57", features = ["Win32_Foundation", "Win32_UI_WindowsAndMessaging"] }
```

- [ ] **Step 2: 写失败测试（祖先链收集）**

创建 `src-tauri/src/window/win32.rs`，初始内容：

```rust
// Windows 窗口聚焦 — 进程祖先链 + EnumWindows（纯 Win32 API，不 spawn 子进程）

#[cfg(test)]
mod tests {
    use super::collect_ancestor_pids_with;
    use std::collections::HashSet;

    #[test]
    fn collects_chain_until_no_parent() {
        // 5 -> 3 -> 1 -> 无父
        let set = collect_ancestor_pids_with(5, |p| match p {
            5 => Some(3),
            3 => Some(1),
            _ => None,
        });
        assert_eq!(set, HashSet::from([5, 3, 1]));
    }

    #[test]
    fn stops_on_cycle() {
        // 7 -> 8 -> 7（环），不得死循环
        let set = collect_ancestor_pids_with(7, |p| if p == 7 { Some(8) } else { Some(7) });
        assert_eq!(set, HashSet::from([7, 8]));
    }

    #[test]
    fn includes_self_when_no_parent() {
        let set = collect_ancestor_pids_with(42, |_| None);
        assert_eq!(set, HashSet::from([42]));
    }
}
```

- [ ] **Step 3: 运行测试确认失败**

```bash
cd src-tauri && cargo test --lib win32
```

预期：编译失败，`cannot find function collect_ancestor_pids_with`。

- [ ] **Step 4: 实现祖先链收集**

在 `win32.rs` 测试模块上方插入：

```rust
use std::collections::HashSet;

/// 沿父进程链收集 PID 集合（含起始 PID 自身）；父进程查询由闭包注入便于单测
/// 遇到环（重复 PID）或父进程缺失即停止；max_depth 防御异常深链
fn collect_ancestor_pids_with(pid: u32, parent_of: impl FnMut(u32) -> Option<u32>) -> HashSet<u32> {
    let mut set = HashSet::new();
    let mut current = pid;
    for _ in 0..64 {
        if !set.insert(current) {
            break; // 已见过 → 环
        }
        match parent_of(current) {
            Some(p) => current = p,
            None => break,
        }
    }
    set
}

/// 收集指定进程的祖先链 PID 集合（含自身）
fn collect_ancestor_pids(system: &sysinfo::System, pid: u32) -> HashSet<u32> {
    collect_ancestor_pids_with(pid, |p| {
        system
            .process(sysinfo::Pid::from_u32(p))
            .and_then(|proc| proc.parent())
            .map(|pp| pp.as_u32())
    })
}
```

- [ ] **Step 5: 运行测试确认通过**

```bash
cd src-tauri && cargo test --lib win32
```

预期：3 个测试 PASS。（此时 win32.rs 尚未被 `window/mod.rs` 引用，若编译报文件未挂载，属正常——Task 6 接线。若 cargo 因孤儿文件不编译导致测试没跑，先完成 Task 6 Step 1 的 mod 声明再回跑。）

- [ ] **Step 6: Commit（与 Task 6 合并提交，见 Task 6 Step 5）**

---

### Task 6: EnumWindows 聚焦实现 + 平台分发

**Files:**
- Modify: `src-tauri/src/window/win32.rs`（追加聚焦实现）
- Modify: `src-tauri/src/window/mod.rs`

- [ ] **Step 1: `window/mod.rs` 挂载模块并重构平台分发**

将 mod 声明区（当前 1-5 行）：

```rust
mod applescript;
mod iterm;
mod terminal_app;
mod tmux;
```

替换为：

```rust
#[cfg(target_os = "macos")]
mod applescript;
#[cfg(target_os = "macos")]
mod iterm;
#[cfg(target_os = "macos")]
mod terminal_app;
#[cfg(target_os = "macos")]
mod tmux;
#[cfg(windows)]
pub mod win32;
```

将 `focus_terminal_for_pid`（连同其上方 Wayland 检测）整体替换为：

```rust
/// 通过 PID 聚焦对应的终端/应用窗口
pub fn focus_terminal_for_pid(pid: u32) -> Result<(), String> {
    #[cfg(windows)]
    {
        return win32::focus_window_for_pid(pid);
    }
    #[cfg(target_os = "macos")]
    {
        // 获取进程的 TTY
        let tty = get_tty_for_pid(pid)?;

        // 依次尝试：tmux → iTerm2 → Terminal.app
        if tmux::focus_tmux_pane_by_tty(&tty).is_ok() {
            return Ok(());
        }
        if iterm::focus_iterm_by_tty(&tty).is_ok() {
            return Ok(());
        }
        terminal_app::focus_terminal_app_by_tty(&tty)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = pid;
        Err("当前平台不支持终端跳转".to_string())
    }
}
```

（原顶部的 Wayland 检测块删除——Linux 现在统一走 else 分支报错，语义等价。）

给 `get_tty_for_pid` 函数加上 cfg 门控（放在其定义上方）：

```rust
/// 通过 ps 命令获取进程的 TTY
#[cfg(target_os = "macos")]
fn get_tty_for_pid(pid: u32) -> Result<String, String> {
```

（函数体不变。）

- [ ] **Step 2: `win32.rs` 追加聚焦实现**

在 `collect_ancestor_pids` 下方追加：

```rust
use windows::Win32::Foundation::{HWND, LPARAM, BOOL};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowLongW, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
    SetForegroundWindow, ShowWindow, SwitchToThisWindow, GWL_EXSTYLE, SW_RESTORE,
    WS_EX_TOOLWINDOW,
};

struct EnumContext {
    pid_set: HashSet<u32>,
    found: Option<HWND>,
}

unsafe extern "system" fn enum_windows_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let ctx = &mut *(lparam.0 as *mut EnumContext);
    if !IsWindowVisible(hwnd).as_bool() {
        return BOOL(1); // 继续枚举
    }
    let style = GetWindowLongW(hwnd, GWL_EXSTYLE);
    if (style & WS_EX_TOOLWINDOW.0 as i32) != 0 {
        return BOOL(1); // 跳过工具窗口
    }
    let mut pid: u32 = 0;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if ctx.pid_set.contains(&pid) && ctx.found.is_none() {
        ctx.found = Some(hwnd);
        return BOOL(0); // Z 序最上的第一个命中即停止
    }
    BOOL(1)
}

/// 聚焦 PID 祖先链上进程拥有的可见顶层窗口
/// CLI 场景：链上含终端宿主（WindowsTerminal/mintty/conhost/Code.exe 等）
/// App 场景：链上含 ChatGPT.exe 主进程（内嵌 codex.exe 的父进程）
fn focus_window(pid_set: &HashSet<u32>) -> Result<(), String> {
    let mut ctx = EnumContext {
        pid_set: pid_set.clone(),
        found: None,
    };
    let lparam = LPARAM(&mut ctx as *mut EnumContext as isize);
    unsafe {
        let _ = EnumWindows(Some(enum_windows_proc), lparam);
    }
    let hwnd = ctx
        .found
        .ok_or_else(|| "未找到可聚焦的窗口（终端可能已关闭）".to_string())?;

    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        if SetForegroundWindow(hwnd).as_bool() {
            return Ok(());
        }
    }
    // 降级：SwitchToThisWindow（Win32 标记为 deprecated 但仍可用）
    #[allow(deprecated)]
    unsafe {
        SwitchToThisWindow(hwnd, true);
    }
    Ok(())
}

/// 聚焦指定进程所在终端 / 应用窗口（focus_session IPC 入口）
pub fn focus_window_for_pid(pid: u32) -> Result<(), String> {
    let system = sysinfo::System::new_all();
    let ancestors = collect_ancestor_pids(&system, pid);
    focus_window(&ancestors)
}
```

- [ ] **Step 3: 编译 + clippy + 全量测试**

```bash
cd src-tauri && cargo test && cargo clippy --all-targets -- -D warnings
```

预期：全部通过。若 `windows` crate feature 报缺失 API，按编译器提示补充对应 feature 名（预期只需 `Win32_Foundation` 与 `Win32_UI_WindowsAndMessaging`）。

- [ ] **Step 4: 冒烟验证（本机）**

启动一个终端跑 `claude`，在项目根目录写临时测试（模仿此前诊断测试的做法）调用 `multi_agents_manager_lib::window::focus_terminal_for_pid(pid)`，观察终端窗口是否被置前。验证后删除临时测试。

- [ ] **Step 5: Commit（Task 5 + Task 6 合并提交）**

```bash
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src/window/win32.rs src-tauri/src/window/mod.rs
git commit -m "feat(window): windows window-level focus via ancestor pids and EnumWindows"
```

---

### Task 7: `jump_supported_for` 放开 Windows

**Files:**
- Modify: `src-tauri/src/session/model.rs:36-38`（函数）+ `jump_tests`（测试）

- [ ] **Step 1: 更新测试（先改测试）**

将 `jump_tests` 中的测试替换为：

```rust
    #[test]
    fn jump_supported_matches_platform_matrix() {
        // Windows：CLI 与 App 均可窗口级聚焦；macOS：仅 CLI（TTY 链路）；其他平台：不支持
        if cfg!(windows) {
            assert!(jump_supported_for(ProcessForm::Cli));
            assert!(jump_supported_for(ProcessForm::App));
        } else if cfg!(target_os = "macos") {
            assert!(jump_supported_for(ProcessForm::Cli));
            assert!(!jump_supported_for(ProcessForm::App));
        } else {
            assert!(!jump_supported_for(ProcessForm::Cli));
            assert!(!jump_supported_for(ProcessForm::App));
        }
    }
```

- [ ] **Step 2: 运行测试确认失败**

```bash
cd src-tauri && cargo test jump_tests
```

预期：Windows 上 FAIL（当前 App 返回 false）。

- [ ] **Step 3: 修改实现**

将：

```rust
pub fn jump_supported_for(form: ProcessForm) -> bool {
    matches!(form, ProcessForm::Cli) && cfg!(target_os = "macos")
}
```

替换为：

```rust
pub fn jump_supported_for(form: ProcessForm) -> bool {
    if cfg!(windows) {
        // Windows：CLI 与 App 均可窗口级聚焦（见 window/win32.rs）
        return true;
    }
    matches!(form, ProcessForm::Cli) && cfg!(target_os = "macos")
}
```

- [ ] **Step 4: 运行测试确认通过**

```bash
cd src-tauri && cargo test jump_tests
```

预期：PASS。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/session/model.rs
git commit -m "feat(session): enable jump for cli and app sessions on windows"
```

---

### Task 8: SessionCard 文案统一 + i18n

**Files:**
- Modify: `src/i18n/locales/zh.json`、`src/i18n/locales/en.json`
- Modify: `src/components/sessions/SessionCard.tsx`

- [ ] **Step 1: 添加 i18n 键**

`zh.json` 顶层新增命名空间（与其他顶层键并列）：

```json
"sessions": {
  "jumpToTerminal": "点击跳转到终端",
  "jumpUnsupported": "当前平台或形态不支持跳转",
  "jumpFailed": "跳转失败: {{error}}",
  "noMessage": "（无消息）",
  "justNow": "刚刚",
  "appBadge": "桌面 App 会话"
}
```

`en.json` 顶层新增（键集与 zh 严格一致）：

```json
"sessions": {
  "jumpToTerminal": "Click to jump to terminal",
  "jumpUnsupported": "Jump is not supported for this session type on this platform",
  "jumpFailed": "Jump failed: {{error}}",
  "noMessage": "(no message)",
  "justNow": "just now",
  "appBadge": "Desktop App session"
}
```

- [ ] **Step 2: 改造 SessionCard.tsx**

① import 区追加：

```tsx
import { useTranslation } from "react-i18next";
```

② `formatRuntime` 改为接收翻译函数（第 27-39 行）：

```tsx
function formatRuntime(lastActivityAt: string, t: (key: string) => string): string {
  if (!lastActivityAt || lastActivityAt === "Unknown") return "--";
  // 尝试解析 ISO 时间戳或 Claude 的时间格式
  const date = new Date(lastActivityAt);
  if (isNaN(date.getTime())) return lastActivityAt.slice(0, 19);
  const diff = Date.now() - date.getTime();
  if (diff < 0) return t("sessions.justNow");
  const mins = Math.floor(diff / 60000);
  if (mins < 1) return t("sessions.justNow");
  if (mins < 60) return `${mins}m`;
  const hours = Math.floor(mins / 60);
  return `${hours}h${mins % 60}m`;
}
```

③ 组件内取 t（`SessionCard` 函数体首行，`const badge = ...` 之前）：

```tsx
  const { t } = useTranslation();
```

④ 不可跳转点击提示（第 46-48 行）：

```tsx
    if (!session.jumpSupported) {
      toast.info(t("sessions.jumpUnsupported"));
      return;
    }
```

⑤ 跳转失败 toast（第 53 行）：

```tsx
      toast.error(t("sessions.jumpFailed", { error: e }));
```

⑥ 卡片 title（第 65 行）：

```tsx
      title={session.jumpSupported ? t("sessions.jumpToTerminal") : t("sessions.jumpUnsupported")}
```

⑦ App 徽标 tooltip（第 79 行，仅替换 title 属性值，外层条件不动）：

```tsx
              <span className="text-[9px] opacity-60" title={t("sessions.appBadge")}>
```

⑧ 无消息占位（第 101 行）：

```tsx
        {session.lastMessage || t("sessions.noMessage")}
```

⑨ 找到 `formatRuntime` 的调用处，补第二参数 `t`（形如 `formatRuntime(session.lastActivityAt)` → `formatRuntime(session.lastActivityAt, t)`）。

- [ ] **Step 3: 检查与验证**

```bash
cd "E:\LLMproject\Github\MultiAgents-Manager" && pnpm lint && pnpm build
node -e "const zh=require('./src/i18n/locales/zh.json'),en=require('./src/i18n/locales/en.json');const f=(o,p='')=>Object.entries(o).flatMap(([k,v])=>typeof v==='object'?f(v,p+k+'.'):[p+k]);const z=new Set(f(zh)),e=new Set(f(en));console.log('zh:',z.size,'en:',e.size,'diff:',[...z].filter(k=>!e.has(k)).concat([...e].filter(k=>!z.has(k))));"
```

预期：lint/build 通过；键数输出 zh 与 en 均为 99（93 + 6），diff 为空数组。

- [ ] **Step 4: Commit**

```bash
git add src/i18n/locales/zh.json src/i18n/locales/en.json src/components/sessions/SessionCard.tsx
git commit -m "fix(ui): unify jump hint messages and add session card i18n"
```

---

### Task 9: 全量验证与人工验证清单

- [ ] **Step 1: 全量自动化门禁**

```bash
cd src-tauri && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check
cd "E:\LLMproject\Github\MultiAgents-Manager" && pnpm lint
```

预期：全部通过。若 `cargo fmt --check` 失败，`cargo fmt` 修正后并入当前任务提交，**不得单独开全仓库 fmt 大提交**。

- [ ] **Step 2: 人工验证（Windows 实机，`pnpm tauri:dev`）**

对应 spec 006 验收场景：

1. PowerShell 中 `cd E:\某项目` 后运行 `opencode` 并发消息 → 首页出现 OpenCode 卡片，项目名为目录名，状态随对话变化
2. 以小写盘符 `cd e:\某项目` 启动 opencode → 卡片出现
3. 在项目父目录启动 opencode（worktree 为子目录的场景）→ 卡片出现
4. 在项目子目录启动 opencode → 卡片出现（worktree 前缀回退）

对应 spec 007 验收场景：

5. Windows Terminal 跑 `claude` → 点击卡片，终端窗口置前
6. 终端最小化 → 点击卡片，窗口恢复并置前
7. 独立 PowerShell / Git Bash 窗口跑 opencode → 点击卡片，对应窗口置前
8. VS Code 集成终端跑 claude → 点击卡片，VS Code 窗口置前
9. ChatGPT 桌面版有 Codex 会话 → 点击 App 卡片，ChatGPT 窗口置前；最小化时点击可恢复
10. 切换界面语言中/英 → 卡片悬停提示、点击 toast、无消息占位、"刚刚" 均跟随切换
11. 同一终端多标签页场景：点击卡片窗口置前即可（允许停留当前标签页）
12. （如有 macOS 条件）CLI 卡片跳转行为与修复前一致（TTY 链路），App 卡片仍提示不支持——本次仅将原代码原样搬入 cfg 块，未改逻辑，此项为回归确认

- [ ] **Step 3: 汇报**

每个 Task 状态、`cargo test` 摘要、人工验证各项结果（无法实机验证的标注"待用户验证"）、`git log --oneline 0fdbd5c..HEAD`。

---

## 范围外（明确不做）

- Windows Terminal 标签页级定位（UI Automation，二期）
- macOS 的 ChatGPT App 窗口聚焦（AppleScript activate，未来按需）
- Linux / Wayland 终端聚焦
- 提权终端（跨完整性级别）的强制置前保障——SetForegroundWindow 失败降级后仍失败则报错
- OpenCode db 历史脏数据（directory 与 worktree 均不匹配进程 cwd）的兜底显示
- SessionCard 之外其余 16 个硬编码 i18n 组件的清理
- SessionCard 的 AGENT_BADGE 缺 openclaw 条目（存量问题，另立任务）
