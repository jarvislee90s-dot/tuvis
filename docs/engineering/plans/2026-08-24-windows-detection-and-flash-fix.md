# Windows 进程识别与闪窗修复 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 修复 Windows 上 Claude/Codex/OpenCode/OpenClaw 进程全部无法识别、以及应用启动/安装时批量闪黑窗两个问题。

**Architecture:** 进程识别的根因是 `monitor/process.rs` 的名字匹配只认 Unix 风格 argv[0]，改为"exe 路径 > 进程名 > 命令行首参"三级回退匹配（basename 归一化、跨平台分隔符、去 `.exe`）。闪窗根因是 Windows 下 spawn 控制台子进程未加 `CREATE_NO_WINDOW`（`cmd mklink`、`git`），分别用 `junction` crate 纯 API 替换 mklink、给 git 加窗口标志并延迟到会话匹配后才调用。附带修复 Claude 会话在 Windows 下的 cwd 校验与项目目录名转换。

**Tech Stack:** Rust (Tauri 2 后端)、sysinfo 0.32、junction 1.2、cargo test。

---

## 背景（执行者必读）

本应用是 Tauri 2 桌面应用（Rust 后端 + React 前端），扫描本机 AI CLI 工具（Claude/Codex/OpenCode/OpenClaw）的运行进程并展示会话状态。当前在 Windows 上存在两类问题（macOS 基本正常，**不得引入 macOS 回归**）：

1. **进程识别全灭**：`src-tauri/src/monitor/process.rs` 的 `find_processes_by_names` 只匹配命令行首参数 `== "codex"` 或 `ends_with("/codex")`（Unix 正斜杠、无扩展名）。Windows 上 argv[0] 永远是 `C:\...\codex.exe` 形态，永远匹配不上。另外提权进程（如 ChatGPT.exe 派生的 codex.exe）命令行不可读，但 `process.name()`/`process.exe()` 仍可读——所以要用多来源匹配。
2. **闪黑窗**：Windows 下 GUI 进程 spawn 控制台程序（`cmd`、`git`）若不加 `CREATE_NO_WINDOW` 标志，每次都会弹一个控制台窗口。两处触发链：
   - 应用启动时 `lib.rs:32-33` 的 `sync_imported_skill_links()` 补链 → 每个链接 spawn 一次 `cmd /C mklink /J`（`linker/mod.rs:69`）
   - 首次会话轮询时 `get_codex_sessions` 无条件解析全部 rollout 文件 → 每个项目 spawn 一次 `git remote get-url`（`parser.rs:107`，缓存只在内存，每次启动重跑）

现场实况（写计划时的取证，供理解，不要依赖其仍成立）：
- Codex 桌面版已并入 ChatGPT：Windows 上安装在 `C:\Program Files\WindowsApps\OpenAI.Codex_..._x64__2p2nqsd0c76g0\app\`，宿主进程叫 `ChatGPT.exe`，后端工作进程 `codex.exe` 由宿主派生
- macOS 上同类进程位于 `/Applications/ChatGPT.app/Contents/Resources/codex`（commit 41adeaa 已用 `.app/contents` 判断修复，保持兼容）
- Claude Code Windows 原生安装为 `claude.exe`；Claude 的 projects 目录名为 `C--Users-bunny-Desktop` 格式（所有非字母数字字符逐个变 `-`，含中文）
- OpenCode 数据在 Windows 上同样位于 `~/.local/share/opencode/opencode.db`（无需改）

**验证环境**：执行机为 Windows（Git Bash）。所有 `cargo` 命令在 `src-tauri/` 目录下执行。

---

### Task 1: 跨平台进程名匹配纯函数 `exe_matches`

**Files:**
- Modify: `src-tauri/src/monitor/process.rs`（文件末尾追加测试模块；新增函数放在 `find_processes_by_names` 上方）

- [ ] **Step 1: 写失败测试**

在 `src-tauri/src/monitor/process.rs` 文件末尾追加：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    mod exe_matches {
        use super::super::exe_matches;

        #[test]
        fn matches_bare_unix_name() {
            // 旧行为兼容：裸名（argv[0] 恰好是命令名）
            assert!(exe_matches("codex", &["codex", "Codex"]));
            assert!(exe_matches("claude", &["claude"]));
        }

        #[test]
        fn matches_unix_path_without_extension() {
            // 旧行为兼容：macOS 内嵌 codex app-server
            assert!(exe_matches(
                "/Applications/ChatGPT.app/Contents/Resources/codex",
                &["codex", "Codex"]
            ));
            assert!(exe_matches("/Users/x/.cargo/bin/codex", &["codex", "Codex"]));
        }

        #[test]
        fn matches_windows_path_with_backslash_and_exe() {
            // 本次修复的核心场景
            assert!(exe_matches(
                "C:\\Users\\bunny\\AppData\\Local\\Microsoft\\WinGet\\Packages\\Anthropic.ClaudeCode_xxx\\claude.exe",
                &["claude"]
            ));
            assert!(exe_matches(
                "C:\\Program Files\\WindowsApps\\OpenAI.Codex_26.818.5229.0_x64__2p2nqsd0c76g0\\app\\codex.exe",
                &["codex", "Codex"]
            ));
        }

        #[test]
        fn matches_process_name_only() {
            // 提权进程命令行读不到时，只剩 process.name()
            assert!(exe_matches("codex.exe", &["codex", "Codex"]));
            assert!(exe_matches("CLAUDE.EXE", &["claude"]));
            assert!(exe_matches("Codex", &["codex", "Codex"]));
        }

        #[test]
        fn rejects_partial_and_unrelated_names() {
            // 防误伤：名字相近但 basename 不同的进程不得命中
            assert!(!exe_matches("codex-plus-plus.exe", &["codex", "Codex"]));
            assert!(!exe_matches("ChatGPT.exe", &["codex", "Codex"]));
            assert!(!exe_matches("node.exe", &["claude"]));
            assert!(!exe_matches("", &["codex"]));
        }
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

```bash
cd src-tauri && cargo test exe_matches
```

预期：编译失败，报 `cannot find function exe_matches in this module`。

- [ ] **Step 3: 实现函数**

在 `src-tauri/src/monitor/process.rs` 中、`fn find_processes_by_names` 定义之前插入：

```rust
/// 归一化候选字符串：统一为 / 分隔、转小写，取 basename，去 Windows .exe 扩展名
/// （先小写再 strip，保证 "CLAUDE.EXE" 这类大写扩展名也能剥掉）
fn normalized_base(candidate: &str) -> String {
    let normalized = candidate.replace('\\', "/").to_lowercase();
    let base = normalized.rsplit('/').next().unwrap_or("");
    base.strip_suffix(".exe").unwrap_or(base).to_string()
}

/// 判断可执行文件路径 / 进程名 / argv[0] 是否匹配工具名列表（跨平台）
/// - Windows: "C:\\...\\codex.exe"、"codex.exe" 均匹配 "codex"
/// - Unix:    "/Applications/ChatGPT.app/Contents/Resources/codex"、"codex" 均匹配 "codex"
fn exe_matches(candidate: &str, process_names: &[&str]) -> bool {
    let base = normalized_base(candidate);
    !base.is_empty() && process_names.iter().any(|name| name.to_lowercase() == base)
}
```

- [ ] **Step 4: 运行测试确认通过**

```bash
cd src-tauri && cargo test exe_matches
```

预期：6 个测试全部 PASS。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/monitor/process.rs
git commit -m "feat(monitor): add cross-platform exe name matching helper"
```

---

### Task 2: 重构 `find_processes_by_names` 为三级来源匹配

匹配来源回退顺序：`process.exe()` 路径 → `process.name()` → `cmd()` 首参数。命中的候选字符串同时用于后续形态判定。此任务是接线重构（sysinfo 的 `System` 无法注入 fixture 做单测），行为正确性由 Task 1 的纯函数与现有回归测试保障。

**Files:**
- Modify: `src-tauri/src/monitor/process.rs:31-130`（整体替换 `find_processes_by_names` 函数体）

- [ ] **Step 1: 替换函数实现**

将 `find_processes_by_names` 整个函数（从 `fn find_processes_by_names(` 到对应的闭括号）替换为：

```rust
/// 通用进程发现：扫描指定进程名列表，过滤子 Agent 和孤儿
/// process_names[0] 是 CLI 名，后续可以是 APP 名
fn find_processes_by_names(
    system: &System,
    process_names: &[&str],
    our_app_names: &[&str],
) -> Vec<AgentProcess> {
    use std::collections::HashSet;

    // 对单个进程做匹配：依次尝试 exe 路径 > 进程名 > 命令行首参数，返回命中候选
    // （提权进程的命令行可能读不到，exe/name 仍可读，因此 exe/name 优先）
    fn match_candidate(process: &sysinfo::Process, names: &[&str]) -> Option<String> {
        if let Some(exe) = process.exe() {
            let exe_str = exe.to_string_lossy().to_string();
            if exe_matches(&exe_str, names) {
                return Some(exe_str);
            }
        }
        let name = process.name().to_string_lossy().to_string();
        if exe_matches(&name, names) {
            return Some(name);
        }
        if let Some(first) = process.cmd().first() {
            let first = first.to_string_lossy().to_string();
            if exe_matches(&first, names) {
                return Some(first);
            }
        }
        None
    }

    // 收集所有匹配的 PID（用于子 Agent 过滤）
    let matched_pids: HashSet<Pid> = system
        .processes()
        .iter()
        .filter(|(_, p)| match_candidate(p, process_names).is_some())
        .map(|(pid, _)| *pid)
        .collect();

    let mut processes = Vec::new();
    for (pid, process) in system.processes() {
        let Some(candidate) = match_candidate(process, process_names) else {
            continue;
        };

        // 排除自身应用
        let process_name = process.name().to_string_lossy();
        if our_app_names.iter().any(|&app| process_name.contains(app)) {
            trace!("Skipping our own app: pid={}, name={}", pid.as_u32(), process_name);
            continue;
        }

        // 判断进程形态（CLI 还是 APP），依据命中候选的路径特征（Task 3 实现 classify_form）
        let form = if process_names.len() > 1 {
            classify_form(&candidate)
        } else {
            ProcessForm::Cli
        };

        let cwd = process.cwd().map(|p| p.to_path_buf());

        // 跳过子 Agent（父进程也是同工具进程）
        if let Some(parent_pid) = process.parent() {
            if matched_pids.contains(&parent_pid) {
                debug!("Skipping sub-agent: pid={}, parent={}", pid.as_u32(), parent_pid.as_u32());
                continue;
            }
        }

        // 跳过孤儿进程（仅 CLI 形态检查 — APP 形态由 launchd / 系统启动是正常的）
        if matches!(form, ProcessForm::Cli) && is_orphaned_process(system, process) {
            warn!("Skipping orphaned CLI: pid={}, cwd={:?}", pid.as_u32(), cwd);
            continue;
        }

        debug!(
            "Found process: name={:?}, pid={}, cwd={:?}, cpu={:.1}%, form={:?}",
            process_name, pid.as_u32(), cwd, process.cpu_usage(), form
        );

        processes.push(AgentProcess {
            pid: pid.as_u32(),
            cpu_usage: process.cpu_usage(),
            cwd,
            form,
        });
    }

    processes
}
```

注意：此时代码引用了尚不存在的 `classify_form`，编译会失败——这是预期，Task 3 紧接着实现它。**本任务不单独 commit**，与 Task 3 一起验证后统一提交。

---

### Task 3: 形态判定函数 `classify_form`（含 Windows MSIX 识别）

把原来内联在 `find_processes_by_names` 里的形态判断提取为纯函数，并新增 Windows MSIX（ChatGPT 合并版 Codex 桌面端）识别。保留 macOS 两种既有判断（首字母大写、`.app/contents`）。

**Files:**
- Modify: `src-tauri/src/monitor/process.rs`（新增函数 + 测试）

- [ ] **Step 1: 写失败测试**

在 Task 1 添加的 `mod tests` 内追加：

```rust
    mod classify_form {
        use super::super::classify_form;
        use crate::session::ProcessForm;

        #[test]
        fn mac_standalone_capitalized_binary_is_app() {
            // 旧行为兼容：独立 Codex.app 的可执行文件首字母大写
            assert_eq!(classify_form("/Applications/Codex.app/Contents/MacOS/Codex"), ProcessForm::App);
        }

        #[test]
        fn mac_chatgpt_embedded_codex_is_app() {
            // 旧行为兼容（commit 41adeaa）：ChatGPT.app 内嵌 codex app-server
            assert_eq!(
                classify_form("/Applications/ChatGPT.app/Contents/Resources/codex"),
                ProcessForm::App
            );
        }

        #[test]
        fn windows_msix_codex_is_app() {
            // 本次新增：ChatGPT 合并版 Codex 桌面端（Windows MSIX 安装目录）
            assert_eq!(
                classify_form(
                    "C:\\Program Files\\WindowsApps\\OpenAI.Codex_26.818.5229.0_x64__2p2nqsd0c76g0\\app\\codex.exe"
                ),
                ProcessForm::App
            );
        }

        #[test]
        fn windows_and_unix_cli_paths_are_cli() {
            assert_eq!(classify_form("C:\\Users\\x\\.local\\bin\\claude.exe"), ProcessForm::Cli);
            assert_eq!(classify_form("/Users/x/.cargo/bin/codex"), ProcessForm::Cli);
            assert_eq!(classify_form("codex.exe"), ProcessForm::Cli);
        }
    }
```

- [ ] **Step 2: 运行测试确认失败**

```bash
cd src-tauri && cargo test classify_form
```

预期：编译失败，`cannot find function classify_form`。

- [ ] **Step 3: 实现函数**

在 `exe_matches` 函数下方插入：

```rust
/// 判断进程形态（CLI 还是 APP），依据命中候选（exe 路径 / 进程名 / argv[0]）的特征
/// - basename 首字母大写（如独立 Codex.app 的 "Codex"）→ APP
/// - 位于 macOS .app 包内（如 ChatGPT.app 内嵌的 codex app-server）→ APP
/// - 位于 Windows MSIX 安装目录（ChatGPT 合并版 Codex 桌面端）→ APP
fn classify_form(candidate: &str) -> ProcessForm {
    let normalized = candidate.replace('\\', "/");
    let base_stem = normalized.rsplit('/').next().unwrap_or("");
    let base_stem = base_stem.strip_suffix(".exe").unwrap_or(base_stem);
    // basename 首字母大写 → APP（保留原始大小写判断，不能先 lowercase）
    let exe_upper = base_stem.chars().next().map(|c| c.is_uppercase()).unwrap_or(false);
    let lower = normalized.to_lowercase();
    let in_app_bundle = lower.contains(".app/contents");
    let in_msix = lower.contains("windowsapps/openai.codex_");
    if exe_upper || in_app_bundle || in_msix {
        ProcessForm::App
    } else {
        ProcessForm::Cli
    }
}
```

- [ ] **Step 4: 运行测试确认通过（含 Task 2 接线编译）**

```bash
cd src-tauri && cargo test
```

预期：全部 PASS（包括 Task 2 的重构编译通过、既有 dao/linker 测试无回归）。

- [ ] **Step 5: Commit（Task 2 + Task 3 合并提交）**

```bash
git add src-tauri/src/monitor/process.rs
git commit -m "feat(monitor): multi-source process matching with Windows MSIX app detection"
```

---

### Task 4: `junction` crate 替换 `cmd /C mklink`（消除链接闪窗）

**Files:**
- Modify: `src-tauri/Cargo.toml`（新增 `[target.'cfg(windows)'.dependencies]` 段）
- Modify: `src-tauri/src/linker/mod.rs:62-76`（Windows 分支）
- Test: `src-tauri/tests/linker_test.rs`（追加测试）

- [ ] **Step 1: 写失败测试**

在 `src-tauri/tests/linker_test.rs` 末尾追加：

```rust
#[cfg(windows)]
#[test]
fn test_create_junction_for_dir() {
    support::setup();
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source-dir");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("SKILL.md"), "# demo").unwrap();
    let target = temp.path().join("junction-dir");
    linker::create_link(&source, &target).unwrap();
    // Junction 表现为目录，且能穿透读到源内容
    assert!(target.is_dir());
    assert!(target.join("SKILL.md").exists());
    linker::remove_link(&target).unwrap();
    assert!(!target.exists());
    // 源目录不受影响
    assert!(source.join("SKILL.md").exists());
}
```

- [ ] **Step 2: 运行测试确认现状（先验证测试本身有效）**

```bash
cd src-tauri && cargo test --test linker_test test_create_junction_for_dir
```

预期：PASS（当前 cmd mklink 实现也能建 Junction）。此测试的意义是锁定行为：替换实现后必须仍然 PASS。若当前实现此测试 FAIL，停止并报告（说明环境异常，如 mklink 需要的权限不满足——这本身也是要修复的信号）。

- [ ] **Step 3: 添加依赖**

在 `src-tauri/Cargo.toml` 中、`[target.'cfg(not(any(target_os = "android", target_os = "ios")))'.dependencies]` 段之前，新增：

```toml
[target.'cfg(windows)'.dependencies]
junction = "1.2"
```

- [ ] **Step 4: 替换实现**

将 `src-tauri/src/linker/mod.rs` 中 `create_link` 的 Windows 分支：

```rust
    #[cfg(windows)]
    {
        // Windows: 目录用 Junction，文件用 copy
        if source.is_dir() {
            // 使用 cmd 创建 Junction
            let source_str = source.to_string_lossy();
            let target_str = target.to_string_lossy();
            std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J", &target_str, &source_str])
                .output()
                .map_err(|e| format!("创建 Junction 失败: {}", e))?;
        } else {
            fs::copy(source, target).map_err(|e| format!("复制文件失败: {}", e))?;
        }
    }
```

替换为：

```rust
    #[cfg(windows)]
    {
        // Windows: 目录用 Junction（纯 API 调用，不走 cmd，避免闪控制台窗口），文件用 copy
        if source.is_dir() {
            junction::create(source, target)
                .map_err(|e| format!("创建 Junction 失败: {}", e))?;
        } else {
            fs::copy(source, target).map_err(|e| format!("复制文件失败: {}", e))?;
        }
    }
```

注意 `junction::create` 参数顺序：第一个参数是被指向的源目录，第二个参数是 Junction 本身的路径（与 `std::os::unix::fs::symlink(old, new)` 一致）。

- [ ] **Step 5: 运行测试确认通过**

```bash
cd src-tauri && cargo test --test linker_test
```

预期：全部 PASS（新增的 junction 测试 + 既有链接测试）。

- [ ] **Step 6: Commit**

```bash
git add src-tauri/Cargo.toml src-tauri/Cargo.toml.lock src-tauri/src/linker/mod.rs src-tauri/tests/linker_test.rs
git commit -m "fix(linker): create junctions via junction crate instead of cmd mklink"
```

---

### Task 5: git 子进程加 `CREATE_NO_WINDOW`（消除 git 闪窗）

**Files:**
- Modify: `src-tauri/src/monitor/parser.rs:99-122`（`get_github_url`）+ 文件内追加测试模块

- [ ] **Step 1: 写失败测试（验证 git 调用链路行为）**

在 `src-tauri/src/monitor/parser.rs` 文件末尾追加：

```rust
#[cfg(test)]
mod git_url_tests {
    use super::*;

    /// 在临时目录构造一个带 origin remote 的 git 仓库，验证 get_github_url 的完整调用链
    #[test]
    fn test_get_github_url_reads_origin() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_str().unwrap().to_string();
        let mut init = std::process::Command::new("git");
        init.args(["init"]).current_dir(&dir);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            init.creation_flags(CREATE_NO_WINDOW);
        }
        init.output().expect("git init 失败（CI/开发机均应安装 git）");
        let mut remote = std::process::Command::new("git");
        remote.args(["remote", "add", "origin", "git@github.com:some-org/some-repo.git"]).current_dir(&dir);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            remote.creation_flags(CREATE_NO_WINDOW);
        }
        remote.output().expect("git remote add 失败");

        assert_eq!(
            get_github_url(&dir).as_deref(),
            Some("https://github.com/some-org/some-repo")
        );
    }

    #[test]
    fn test_get_github_url_none_for_plain_dir() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(get_github_url(temp.path().to_str().unwrap()), None);
    }
}
```

- [ ] **Step 2: 运行测试确认现状**

```bash
cd src-tauri && cargo test git_url_tests
```

预期：PASS（此测试锁定功能行为，防 Step 3 改坏；无法从外部断言窗口标志，窗口消除靠人工验证，见 Task 9）。

- [ ] **Step 3: 给实现加窗口标志**

将 `get_github_url` 中：

```rust
    let result = (|| {
        let output = Command::new("git").args(["remote", "get-url", "origin"])
            .current_dir(project_path).output().ok()?;
        if !output.status.success() { return None; }
```

替换为：

```rust
    let result = (|| {
        let mut cmd = Command::new("git");
        cmd.args(["remote", "get-url", "origin"]).current_dir(project_path);
        // Windows 下 GUI 进程 spawn 控制台程序会闪黑窗，必须加 CREATE_NO_WINDOW
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let output = cmd.output().ok()?;
        if !output.status.success() { return None; }
```

（函数其余部分不动。）

- [ ] **Step 4: 运行测试确认通过**

```bash
cd src-tauri && cargo test git_url_tests
```

预期：PASS。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/monitor/parser.rs
git commit -m "fix(monitor): suppress console window on git subprocess (Windows)"
```

---

### Task 6: Codex 会话的 `github_url` 延迟到进程匹配后计算

当前 `get_codex_sessions` 在进程匹配**之前**解析全部 rollout 文件，`parse_codex_jsonl` 内部对每个文件调 `get_github_url` → 启动风暴式 git spawn（即使进程没匹配上、会话最终被丢弃也照跑）。改为解析时置 `None`，仅在 Phase 1/2 把会话附着到进程时才计算。

**Files:**
- Modify: `src-tauri/src/monitor/parser.rs`（三处小改：`parse_codex_jsonl` 内 1 处、`get_codex_sessions` Phase 1 与 Phase 2 各 1 处）

- [ ] **Step 1: 修改 `parse_codex_jsonl`**

在 `parse_codex_jsonl` 尾部构造 `Session` 的地方（当前约 617 行），将：

```rust
        github_url: get_github_url(&project_path),
```

改为：

```rust
        github_url: None, // 延迟到进程匹配后填充（见 get_codex_sessions），避免批量解析时风暴式 spawn git
```

- [ ] **Step 2: Phase 1 填充**

在 `get_codex_sessions` 的 Phase 1（按 cwd 精确匹配，当前约 425-431 行）：

```rust
                    let mut session = parse_codex_jsonl(file_path, proc.form)
                        .unwrap_or_else(|| session.clone());
                    session.pid = proc.pid;
                    session.cpu_usage = proc.cpu_usage;
                    session.form = proc.form;
                    session.jump_supported = matches!(proc.form, ProcessForm::Cli);
```

在 `session.jump_supported = ...;` 之后追加一行：

```rust
                    session.github_url = get_github_url(&session.project_path);
```

- [ ] **Step 3: Phase 2 填充**

在 Phase 2（未匹配进程回退最近会话文件，当前约 446-451 行）的相同位置（`session.jump_supported = ...;` 之后）同样追加：

```rust
                session.github_url = get_github_url(&session.project_path);
```

- [ ] **Step 4: 编译与回归**

```bash
cd src-tauri && cargo test
```

预期：全部 PASS（此改动无新增单测——匹配流程需要真实进程与 rollout 文件，行为由 Task 9 人工验证兜底）。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/monitor/parser.rs
git commit -m "perf(monitor): defer github_url git spawn until session matched to process"
```

---

### Task 7: `detector::which` 改为纯 PATH 扫描（去子进程 + Windows 可用）

当前用 `Command::new("which")`——Windows 没有 `which` 命令，所有工具的"CLI 可用"检测永远 false。

**Files:**
- Modify: `src-tauri/src/linker/detector.rs:44-51` + 文件内追加测试

- [ ] **Step 1: 写失败测试**

在 `src-tauri/src/linker/detector.rs` 文件末尾追加：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_which_finds_git() {
        // CI 与开发机均安装 git
        assert!(which("git"));
    }

    #[test]
    fn test_which_rejects_missing_cmd() {
        assert!(!which("mam-definitely-missing-cmd"));
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

```bash
cd src-tauri && cargo test test_which
```

预期：`test_which_finds_git` FAIL（Windows 无 `which`，返回 false）。

- [ ] **Step 3: 替换实现**

将 `detector.rs` 中的：

```rust
/// 简易 which 命令 — 检测可执行文件是否在 PATH 中
fn which(cmd: &str) -> bool {
    std::process::Command::new("which")
        .arg(cmd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
```

替换为：

```rust
/// 检测可执行文件是否在 PATH 中（纯路径扫描，不 spawn 子进程，跨平台）
/// Windows 下额外尝试 .exe 扩展名
fn which(cmd: &str) -> bool {
    let path_env = std::env::var("PATH").unwrap_or_default();
    let candidates: Vec<String> = if cfg!(windows) {
        vec![format!("{}.exe", cmd), cmd.to_string()]
    } else {
        vec![cmd.to_string()]
    };
    std::env::split_paths(&path_env).any(|dir| {
        candidates.iter().any(|name| dir.join(name).is_file())
    })
}
```

- [ ] **Step 4: 运行测试确认通过**

```bash
cd src-tauri && cargo test test_which
```

预期：PASS。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/linker/detector.rs
git commit -m "fix(detector): cross-platform PATH scan instead of which subprocess"
```

---

### Task 8: Claude 会话解析的 Windows 路径支持

四个小函数，逐个 TDD。Claude Code 的 projects 目录名规则（实测取证）：**路径中每个非 ASCII 字母数字字符（`/` `\` `:` `.` 空格 中文等）逐字符替换为 `-`**。例如 `C:\Users\bunny\Desktop` → `C--Users-bunny-Desktop`，`C:\Users\bunny\.agents\skills\extract-report` → `C--Users-bunny--agents-skills-extract-report`（本机真实存在的目录名）。

**Files:**
- Modify: `src-tauri/src/monitor/parser.rs`（`convert_path_to_dir_name`、`convert_dir_name_to_path`、`extract_cwd_from_jsonl`、新增 `project_name_from_path`、替换两处 `project_name` 计算）+ 测试

- [ ] **Step 1: 写失败测试**

在 `parser.rs` 的 `mod git_url_tests` 之前追加一个测试模块：

```rust
#[cfg(test)]
mod path_tests {
    use super::*;

    mod dir_name {
        use super::super::convert_path_to_dir_name;

        #[test]
        fn unix_paths_keep_old_behavior() {
            assert_eq!(convert_path_to_dir_name("/Users/x/proj"), "-Users-x-proj");
            assert_eq!(convert_path_to_dir_name("/Users/x/.agents/skills"), "-Users-x--agents-skills");
        }

        #[test]
        fn windows_paths() {
            assert_eq!(convert_path_to_dir_name("C:\\Users\\bunny\\Desktop"), "C--Users-bunny-Desktop");
            assert_eq!(
                convert_path_to_dir_name("C:\\Users\\bunny\\.agents\\skills\\extract-report"),
                "C--Users-bunny--agents-skills-extract-report"
            );
            // 非 ASCII 字符逐字符替换为 '-'（对照本机真实目录 C--Users-bunny-Desktop-----）
            assert_eq!(convert_path_to_dir_name("C:\\Users\\bunny\\Desktop\\桌面"), "C--Users-bunny-Desktop--");
        }
    }

    mod dir_name_reverse {
        use super::super::convert_dir_name_to_path;

        #[test]
        fn windows_drive_letter() {
            assert_eq!(convert_dir_name_to_path("C--Users-bunny-Desktop"), "C:\\Users\\bunny\\Desktop");
        }

        #[test]
        fn unix_keeps_old_behavior() {
            assert_eq!(convert_dir_name_to_path("-Users-x-proj"), "/Users/x/proj");
        }
    }

    mod valid_cwd {
        use super::super::is_valid_cwd;

        #[test]
        fn accepts_unix_and_windows_absolute() {
            assert!(is_valid_cwd("/Users/x/proj"));
            assert!(is_valid_cwd("C:\\Users\\x"));
            assert!(is_valid_cwd("c:/Users/x"));
        }

        #[test]
        fn rejects_relative_and_empty() {
            assert!(!is_valid_cwd("relative/path"));
            assert!(!is_valid_cwd(""));
            assert!(!is_valid_cwd("C"));
        }
    }

    mod project_name {
        use super::super::project_name_from_path;

        #[test]
        fn cross_platform_basename() {
            assert_eq!(project_name_from_path("C:\\Users\\bunny\\Desktop"), "Desktop");
            assert_eq!(project_name_from_path("/Users/x/proj"), "proj");
            assert_eq!(project_name_from_path("/"), "Unknown");
        }
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

```bash
cd src-tauri && cargo test path_tests
```

预期：`windows_paths`、`windows_drive_letter`、`accepts_unix_and_windows_absolute`、`cross_platform_basename` FAIL（当前实现只支持 Unix）；`is_valid_cwd`、`project_name_from_path` 编译失败（函数不存在）。`unix_*` 用例 PASS。

- [ ] **Step 3: 重写 `convert_path_to_dir_name`**

将现有实现（含 peekable 的逐字符逻辑，当前 24-43 行）整体替换为：

```rust
/// 将路径转换为 Claude projects 目录名
/// Claude Code 规则：路径中每个非 ASCII 字母数字字符（分隔符、盘符冒号、点、空格、非 ASCII）
/// 逐字符替换为 '-'
/// Unix: /Users/x/proj -> -Users-x-proj；Windows: C:\Users\x\proj -> C--Users-x-proj
pub fn convert_path_to_dir_name(path: &str) -> String {
    path.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}
```

- [ ] **Step 4: `convert_dir_name_to_path` 增加 Windows 分支**

在函数体最前面（`let name = dir_name.strip_prefix('-')...` 之前）插入：

```rust
    // Windows 盘符目录名（如 C--Users-bunny）→ C:\Users\bunny
    // 注意：目录名中 '.' 与 '-' 不可区分，还原结果仅作兜底显示，精确 cwd 以 jsonl 内记录为准
    let mut chars = dir_name.chars();
    if let (Some(first), Some(second)) = (chars.next(), chars.next()) {
        if first.is_ascii_alphabetic() && second == '-' {
            let rest: String = dir_name.chars().skip(2).collect();
            return format!("{}:\\{}", first, rest.replace('-', "\\"));
        }
    }
```

（函数原有 Unix 逻辑全部保持不变。）

- [ ] **Step 5: 新增 `is_valid_cwd` 并接入 `extract_cwd_from_jsonl`**

在 `extract_cwd_from_jsonl` 上方新增：

```rust
/// 校验 cwd 字符串形态：Unix 绝对路径（/ 开头）或 Windows 盘符路径（如 C:\... 或 c:/...）
fn is_valid_cwd(cwd: &str) -> bool {
    let bytes = cwd.as_bytes();
    cwd.starts_with('/')
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
}
```

将 `extract_cwd_from_jsonl` 中的：

```rust
            if let Some(cwd) = msg.cwd {
                if cwd.starts_with('/') { return Some(cwd); }
            }
```

替换为：

```rust
            if let Some(cwd) = msg.cwd {
                if is_valid_cwd(&cwd) { return Some(cwd); }
            }
```

- [ ] **Step 6: 新增 `project_name_from_path` 并替换两处调用**

在 `is_valid_cwd` 下方新增：

```rust
/// 从项目路径提取项目名（跨平台：兼容 / 和 \ 分隔符）
pub fn project_name_from_path(project_path: &str) -> String {
    project_path
        .rsplit(['/', '\\'])
        .find(|s| !s.is_empty())
        .unwrap_or("Unknown")
        .to_string()
}
```

将 Claude 解析中的（当前约 346 行）：

```rust
    let project_name = project_path.split('/').rfind(|s| !s.is_empty()).unwrap_or("Unknown").to_string();
```

和 Codex 解析中的（当前约 605 行，注意 Codex 处变量名相同）：

```rust
    let project_name = project_path.split('/').rfind(|s| !s.is_empty()).unwrap_or("Unknown").to_string();
```

都替换为：

```rust
    let project_name = project_name_from_path(project_path);
```

- [ ] **Step 7: 运行测试确认通过**

```bash
cd src-tauri && cargo test path_tests && cargo test
```

预期：全部 PASS。

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/monitor/parser.rs
git commit -m "fix(monitor): windows path support for claude session matching"
```

---

### Task 9: 全量验证与人工验证清单

- [ ] **Step 1: 全量自动化检查**

```bash
cd src-tauri && cargo test
cd src-tauri && cargo clippy -- -D warnings
cd src-tauri && cargo fmt --check
```

预期：全部通过。若 clippy 报警告，修复后重跑（不得用 `#[allow]` 压制）。

- [ ] **Step 2: 前端检查（本次未改前端，确认无意外破坏）**

```bash
pnpm lint
```

- [ ] **Step 3: Commit（如有格式修正）**

```bash
git add -A && git commit -m "style: cargo fmt after windows support fixes"
```

（无修正则跳过。）

- [ ] **Step 4: 人工验证（Windows 实机，需要用户配合或执行者本机操作）**

1. `pnpm tauri:dev` 启动应用：**启动过程不得出现任何黑色控制台窗口闪烁**（修复前：mklink + git 风暴）
2. 保持 ChatGPT（含 Codex）应用打开并有一个 Codex 会话在跑：首页应出现 Codex 的会话卡片（App 形态、无"跳转终端"按钮）
3. 在 Windows Terminal 里跑 `claude`：首页应出现 Claude Code 会话卡片（CLI 形态），项目名显示为目录名而非完整路径
4. 打开设置页：各工具的"CLI 可用"状态应正确（本机装有 claude/codex/opencode 的显示为可用）
5. 在资源管理器进入 `~/.mam/active/<tool>/skills/`： Junction 应正常创建，工具的 skill 目录里能看到链接目录

- [ ] **Step 5: 汇报**

汇报内容：每个 Task 的完成状态、`cargo test` 最终输出摘要、人工验证清单各项结果（或标注"待用户验证"）。

---

## 范围外（明确不做）

- 终端聚焦/跳转（`window/` 模块 AppleScript/tmux）的 Windows 等价实现——App 形态会话本就 `jump_supported=false`
- OpenCode/OpenClaw 解析器改动——其数据路径在 Windows 上本就使用 `~/.local/share` 与 `~/.config`（已实测存在），进程层修复后即可识别
- `remove_link` 对 Junction 的删除语义（现有 `is_symlink` + `remove_file` 对 Junction 有效，Task 4 测试已覆盖）
- 前端改动（无需要）
