# Windows 后续修复（cwd 匹配 / 跳转 / with_exe / MAM_HOME）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 修复上一轮 Windows 兼容性修复（591d3c1..a39d7c2）后实测残留的四个问题：Claude/OpenCode 会话不显示、Codex 卡片误显示跳转按钮、Windows 点击跳转必报错、MAM_HOME 环境变量泄漏进生产构建。

**Architecture:** 会话匹配失败的根因是 cwd 字符串两侧不可比（进程侧带尾部路径分隔符、盘符大小写与 Claude 记录的不一致），新增归一化纯函数统一两侧。跳转按钮在 Windows/Linux 下禁用（终端聚焦只实现了 macOS）。System 刷新配置补上 `with_exe`，让 ChatGPT 内嵌 codex.exe 能拿到 exe 路径从而正确判定 App 形态。MAM_HOME 用 `debug_assertions` 门控到非 release 构建。

**Tech Stack:** Rust (Tauri 2 后端)、sysinfo 0.32、cargo test。

---

## 背景（执行者必读）

上一轮修复后，进程层匹配（`exe_matches` 三级来源匹配）已实测有效：`claude.exe`、`opencode.exe`、ChatGPT 内嵌 `codex.exe` 都能被 `find_*_processes` 找到。但用户实机验证仍发现四个问题，本轮逐一修复。以下取证结论均来自实机诊断（写计划时的现场，不要假设仍成立）：

1. **Claude 会话不显示**：`claude.exe` 进程能找到（form=Cli、cwd=`E:\LLMproject\ABSreport\`），但会话匹配失败。原因有两个：
   - sysinfo 在 Windows 返回的 cwd **带尾部反斜杠**，转成 Claude projects 目录名后尾部多一个 `-`，而 Claude 实际目录名（如 `e--LLMproject-ABSreport`）没有；
   - Claude 保留用户 `cd` 时敲入的**盘符大小写**（实机同时存在 `E--LLMproject-*` 和 `e--LLMproject-ABSreport` 两种目录），而目录名比较是大小写敏感的。
2. **Codex 卡片误显示跳转按钮**（点击报 `Failed to get TTY: program not found`）：`adapter/mod.rs` 的 System 刷新配置只开了 `cmd`/`cwd`/`cpu`，**没开 `with_exe`**，应用内 `process.exe()` 恒为 None；提权的 codex.exe 命令行也读不到，只能靠裸名 `codex.exe` 匹配 → `classify_form` 无路径特征 → 误判 CLI → 显示跳转按钮。点击后走 `window/mod.rs` 的 `ps` 命令（Unix only，Windows 没有）→ 报错。
3. **Windows 下所有跳转都会失败**：终端聚焦（`window/` 模块）只实现了 macOS 的 iTerm2/Terminal.app/tmux，Windows/Linux 下任何 CLI 卡片点跳转都会报同样的 `ps` 错误。
4. **MAM_HOME 泄漏**（代码审查 Important-1）：`database/connection.rs` 的 `app_data_home()` 在生产构建同样读取 `MAM_HOME`，且全仓只有数据库这一处走它——环境变量误设会导致 DB 与 skills/plugins 指向不同 home，数据割裂。集成测试（dao/linker）在 debug profile 下运行，`debug_assertions` 为 true，用它门控即可两全。

**约束**：不得引入 macOS 回归（`cargo test` 中"旧行为兼容"用例是防线）。Unix 文件系统大小写敏感，cwd 归一化**只能在 Windows 下转小写**。

**验证环境**：执行机为 Windows（Git Bash）。cargo 命令在 `src-tauri/` 目录下执行。若 cargo 下载依赖报 TLS 错误（SteamTools MITM），先跑 `python C:\Users\bunny\AppData\Local\Temp\dsh-cargo-http-mirror.py 8765`（后台），依赖已在 `~/.cargo` 缓存时通常无需此步。

---

### Task 1: cwd 归一化纯函数 `normalize_cwd_for_match`

**Files:**
- Modify: `src-tauri/src/monitor/parser.rs`（函数放在 `is_valid_cwd` 附近；测试追加到文件末尾已有的 `#[cfg(test)] mod` 区域）

- [ ] **Step 1: 写失败测试**

在 `src-tauri/src/monitor/parser.rs` 文件末尾的测试区域（与 `mod path_tests`、`mod git_url_tests` 平级）追加：

```rust
#[cfg(test)]
mod normalize_cwd_tests {
    use super::normalize_cwd_for_match;

    #[test]
    fn trims_trailing_separators() {
        // Windows 下 sysinfo 返回的 cwd 带尾部反斜杠
        let expected = if cfg!(windows) { "e:\\x\\y" } else { "E:\\x\\y" };
        assert_eq!(normalize_cwd_for_match("E:\\x\\y\\"), expected);
        assert_eq!(normalize_cwd_for_match("E:\\x\\y"), expected);
    }

    #[test]
    fn unix_paths_trim_only() {
        let expected = if cfg!(windows) { "/users/x/proj" } else { "/Users/x/proj" };
        assert_eq!(normalize_cwd_for_match("/Users/x/proj/"), expected);
    }

    #[test]
    fn drive_letter_case_normalized_on_windows_only() {
        if cfg!(windows) {
            assert_eq!(normalize_cwd_for_match("E:\\X"), normalize_cwd_for_match("e:\\x"));
        }
    }

    #[test]
    fn root_normalizes_to_empty() {
        // 根路径归一化为空串，调用方按"无有效 cwd"处理（进入 unmatched 分支）
        assert_eq!(normalize_cwd_for_match("/"), "");
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

```bash
cd src-tauri && cargo test normalize_cwd_tests
```

预期：编译失败，`cannot find function normalize_cwd_for_match`。

- [ ] **Step 3: 实现函数**

在 `parser.rs` 的 `is_valid_cwd` 函数下方插入：

```rust
/// 归一化 cwd 字符串用于"进程 cwd ↔ 会话 cwd"匹配：
/// - 去尾部路径分隔符（Windows 下 sysinfo 返回的 cwd 带尾部反斜杠，如 "E:\x\y\"）
/// - Windows 下整体转小写（盘符/路径大小写随用户 cd 写法不同，文件系统实际不区分；
///   Unix 文件系统大小写敏感，保持原样）
/// - 根路径（"/"）归一化为空串，调用方按无有效 cwd 处理
fn normalize_cwd_for_match(cwd: &str) -> String {
    let trimmed = cwd.trim_end_matches(['/', '\\']);
    if cfg!(windows) {
        trimmed.to_lowercase()
    } else {
        trimmed.to_string()
    }
}
```

- [ ] **Step 4: 运行测试确认通过**

```bash
cd src-tauri && cargo test normalize_cwd_tests
```

预期：4 个测试 PASS。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/monitor/parser.rs
git commit -m "feat(monitor): add cwd normalization helper for session matching"
```

---

### Task 2: Claude 会话匹配接入归一化

三处比较全部修复：进程 cwd 侧（目录名小写化 + 归一化）、实际目录名比较侧、会话文件 cwd 侧。此任务为接线改动（需要真实进程与 `~/.claude/projects` fixture，不做单测），行为正确性由 Task 1 纯函数与 Task 7 人工验证保障。

**Files:**
- Modify: `src-tauri/src/monitor/parser.rs`（`get_claude_sessions`，当前约 236-295 行）

- [ ] **Step 1: 修改进程 cwd 映射构建**

将：

```rust
    for process in processes {
        if let Some(cwd) = &process.cwd {
            let cwd_str = cwd.to_string_lossy().to_string();
            expected_dir_names.insert(convert_path_to_dir_name(&cwd_str));
            cwd_to_processes.entry(cwd_str).or_default().push(process);
        }
    }
```

替换为：

```rust
    for process in processes {
        if let Some(cwd) = &process.cwd {
            let normalized = normalize_cwd_for_match(&cwd.to_string_lossy());
            // 目录名比较大小写不敏感：Claude 保留用户 cd 时敲入的盘符/路径大小写
            // （实机同时存在 E--xxx 与 e--xxx 两种目录），sysinfo 返回的可能是另一种
            expected_dir_names.insert(convert_path_to_dir_name(&normalized).to_lowercase());
            cwd_to_processes.entry(normalized).or_default().push(process);
        }
    }
```

- [ ] **Step 2: 修改目录名比较**

将：

```rust
            if !expected_dir_names.contains(dir_name) {
                continue;
            }
```

替换为：

```rust
            if !expected_dir_names.contains(&dir_name.to_lowercase()) {
                continue;
            }
```

- [ ] **Step 3: 修改会话文件 cwd 归一化**

将：

```rust
                let file_cwd =
                    extract_cwd_from_jsonl(f).unwrap_or_else(|| convert_dir_name_to_path(dir_name));
                cwd_to_files.entry(file_cwd).or_default().push(f.clone());
```

替换为：

```rust
                let file_cwd = extract_cwd_from_jsonl(f)
                    .unwrap_or_else(|| convert_dir_name_to_path(dir_name));
                // 与进程 cwd 同一归一化域（Windows 下小写、无尾部分隔符），两侧才可比
                cwd_to_files
                    .entry(normalize_cwd_for_match(&file_cwd))
                    .or_default()
                    .push(f.clone());
```

注：归一化后的 `project_path`（Windows 下为小写）会流入 `session.project_path`，项目名显示（`project_name_from_path` 取 basename）不受影响；`get_github_url` 在 Windows 下以大小写不敏感路径执行 git，亦不受影响。

- [ ] **Step 4: 编译与回归**

```bash
cd src-tauri && cargo test
```

预期：全部 PASS（含既有 `path_tests`，无回归）。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/monitor/parser.rs
git commit -m "fix(monitor): case-insensitive and separator-normalized claude session matching"
```

---

### Task 3: Codex 会话匹配接入归一化

Codex 的 Phase 1 按进程 cwd ↔ rollout 文件 cwd 精确匹配，两侧字符串同样存在尾部反斜杠/大小写不可比问题。

**Files:**
- Modify: `src-tauri/src/monitor/parser.rs`（`get_codex_sessions`，当前约 496-520 行）

- [ ] **Step 1: 修改进程 cwd 映射构建**

将：

```rust
    for process in processes {
        match &process.cwd {
            Some(cwd) => {
                let cwd_str = cwd.to_string_lossy().to_string();
                if cwd_str == "/" || cwd_str.is_empty() {
                    unmatched_processes.push(process);
                } else {
                    cwd_to_processes.entry(cwd_str).or_default().push(process);
                }
            }
            None => unmatched_processes.push(process),
        }
    }
```

替换为（注：`"/"` 归一化后即为空串，原 `== "/"` 判断并入 `is_empty`）：

```rust
    for process in processes {
        match &process.cwd {
            Some(cwd) => {
                // 归一化：去尾部分隔符、Windows 下转小写（与 rollout 中记录的 cwd 保持可比）
                let normalized = normalize_cwd_for_match(&cwd.to_string_lossy());
                if normalized.is_empty() {
                    unmatched_processes.push(process);
                } else {
                    cwd_to_processes.entry(normalized).or_default().push(process);
                }
            }
            None => unmatched_processes.push(process),
        }
    }
```

- [ ] **Step 2: 修改 Phase 1 查找键**

将（当前约 517 行）：

```rust
            if let Some(procs) = cwd_to_processes.get(&session.project_path) {
```

替换为：

```rust
            if let Some(procs) = cwd_to_processes.get(&normalize_cwd_for_match(&session.project_path)) {
```

（`session.project_path` 保持文件原值用于展示，仅查找键归一化。）

- [ ] **Step 3: 编译与回归**

```bash
cd src-tauri && cargo test
```

预期：全部 PASS。

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/monitor/parser.rs
git commit -m "fix(monitor): normalize cwd keys for codex session matching"
```

---

### Task 4: `jump_supported_for` 辅助函数并替换 7 处赋值

**Files:**
- Modify: `src-tauri/src/session/model.rs`（`ProcessForm` 定义处，当前约 28 行附近）+ 测试
- Modify: `src-tauri/src/monitor/parser.rs`（4 处：458、525、547、761 行附近）
- Modify: `src-tauri/src/monitor/opencode_parser.rs`（2 处：194、255 行附近）
- Modify: `src-tauri/src/monitor/openclaw_parser.rs`（1 处：161 行附近）

- [ ] **Step 1: 写失败测试**

在 `src-tauri/src/session/model.rs` 文件末尾追加：

```rust
#[cfg(test)]
mod jump_tests {
    use super::*;

    #[test]
    fn jump_only_supported_for_cli_on_macos() {
        // 终端聚焦（window/ 模块）目前只实现 macOS；其他平台任何形态都不可跳转
        if cfg!(target_os = "macos") {
            assert!(jump_supported_for(ProcessForm::Cli));
            assert!(!jump_supported_for(ProcessForm::App));
        } else {
            assert!(!jump_supported_for(ProcessForm::Cli));
            assert!(!jump_supported_for(ProcessForm::App));
        }
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

```bash
cd src-tauri && cargo test jump_tests
```

预期：编译失败，`cannot find function jump_supported_for`。

- [ ] **Step 3: 实现函数**

在 `session/model.rs` 的 `pub enum ProcessForm` 定义之后插入：

```rust
/// 跳转终端是否可用：仅 CLI 形态，且仅 macOS
/// （window/ 模块的终端聚焦只实现了 macOS 的 iTerm2/Terminal.app/tmux；
/// 其他平台点击会调用 Unix ps 命令而失败，故直接禁用）
pub fn jump_supported_for(form: ProcessForm) -> bool {
    matches!(form, ProcessForm::Cli) && cfg!(target_os = "macos")
}
```

- [ ] **Step 4: 运行测试确认通过**

```bash
cd src-tauri && cargo test jump_tests
```

预期：PASS。

- [ ] **Step 5: 替换 7 处赋值**

① `src-tauri/src/monitor/parser.rs` 顶部 import（当前第 8 行）：

```rust
use crate::session::ProcessForm;
```

改为：

```rust
use crate::session::{ProcessForm, jump_supported_for};
```

② `parser.rs` `get_claude_sessions` 内的 Claude Session 构造（当前约 458 行）：

```rust
        jump_supported: matches!(process.form, ProcessForm::Cli),
```

改为：

```rust
        jump_supported: jump_supported_for(process.form),
```

③ `parser.rs` Codex Phase 1（当前约 525 行）：

```rust
                    session.jump_supported = matches!(proc.form, ProcessForm::Cli);
```

改为：

```rust
                    session.jump_supported = jump_supported_for(proc.form);
```

④ `parser.rs` Codex Phase 2（当前约 547 行）：

```rust
                session.jump_supported = matches!(process.form, ProcessForm::Cli);
```

改为：

```rust
                session.jump_supported = jump_supported_for(process.form);
```

⑤ `parser.rs` `parse_codex_jsonl` 的 Session 构造默认值（当前约 761 行）：

```rust
        jump_supported: true,
```

改为：

```rust
        jump_supported: jump_supported_for(ProcessForm::Cli), // 由调用方按进程形态覆盖
```

⑥ `src-tauri/src/monitor/opencode_parser.rs` 顶部 import（当前第 5 行）：

```rust
use crate::session::{AgentType, ProcessForm, Session, SessionStatus};
```

改为：

```rust
use crate::session::{jump_supported_for, AgentType, ProcessForm, Session, SessionStatus};
```

该文件内两处（当前约 194、255 行）：

```rust
        jump_supported: matches!(process.form, ProcessForm::Cli),
```

都改为：

```rust
        jump_supported: jump_supported_for(process.form),
```

⑦ `src-tauri/src/monitor/openclaw_parser.rs` 顶部 import（当前第 5 行）同样加 `jump_supported_for`（与⑥相同写法），该文件内一处（当前约 161 行）：

```rust
        jump_supported: matches!(process.form, ProcessForm::Cli),
```

改为：

```rust
        jump_supported: jump_supported_for(process.form),
```

- [ ] **Step 6: 编译与回归**

```bash
cd src-tauri && cargo test
```

预期：全部 PASS。若报 `unused import: ProcessForm`（某文件替换后不再直接使用 ProcessForm），按编译器提示从该文件的 use 列表移除它。

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/session/model.rs src-tauri/src/monitor/parser.rs src-tauri/src/monitor/opencode_parser.rs src-tauri/src/monitor/openclaw_parser.rs
git commit -m "fix(session): disable terminal jump on non-macos platforms"
```

---

### Task 5: System 刷新配置补 `with_exe`

这是"Codex 卡片误显示跳转按钮"的另一半根因：没有 exe 路径时，`classify_form` 对提权进程（cmd 读不到）只能拿到裸名 `codex.exe`，无法识别 Windows MSIX 安装目录特征。无法为刷新配置写单测，靠 Task 7 人工验证。

**Files:**
- Modify: `src-tauri/src/adapter/mod.rs`（当前约 126-142 行，两处刷新配置）

- [ ] **Step 1: 修改初始化配置**

将：

```rust
            System::new_with_specifics(
                RefreshKind::new().with_processes(
                    ProcessRefreshKind::new()
                        .with_cmd(sysinfo::UpdateKind::Always)
                        .with_cwd(sysinfo::UpdateKind::Always)
                        .with_cpu(),
                ),
            )
```

替换为：

```rust
            System::new_with_specifics(
                RefreshKind::new().with_processes(
                    ProcessRefreshKind::new()
                        .with_cmd(sysinfo::UpdateKind::Always)
                        .with_cwd(sysinfo::UpdateKind::Always)
                        // exe 路径是 Windows MSIX 形态判定（classify_form）的关键输入：
                        // 缺失时 ChatGPT 内嵌 codex.exe 会被误判为 CLI（提权进程 cmd 也读不到）
                        .with_exe(sysinfo::UpdateKind::Always)
                        .with_cpu(),
                ),
            )
```

- [ ] **Step 2: 修改增量刷新配置**

将：

```rust
        system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::new()
                .with_cmd(sysinfo::UpdateKind::Always)
                .with_cwd(sysinfo::UpdateKind::Always)
                .with_cpu(),
        );
```

替换为：

```rust
        system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::new()
                .with_cmd(sysinfo::UpdateKind::Always)
                .with_cwd(sysinfo::UpdateKind::Always)
                .with_exe(sysinfo::UpdateKind::Always)
                .with_cpu(),
        );
```

- [ ] **Step 3: 编译与回归**

```bash
cd src-tauri && cargo test
```

预期：全部 PASS。

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src/adapter/mod.rs
git commit -m "fix(adapter): refresh process exe path for app-form detection"
```

---

### Task 6: MAM_HOME 门控到非 release 构建

集成测试（tests/ 目录）以 debug profile 运行（`debug_assertions = true`），门控后测试重定向不受影响；release 生产构建彻底忽略该变量。既有 dao/linker 集成测试就是本改动的回归防线（它们依赖 `tests/support.rs` 设置的 MAM_HOME 重定向，若门控写错会直接失败）。

**Files:**
- Modify: `src-tauri/src/database/connection.rs`（当前约 7-15 行）

- [ ] **Step 1: 修改 `app_data_home`**

将：

```rust
/// 应用数据主目录：优先取 MAM_HOME 环境变量（测试重定向用），否则用 dirs::home_dir()
/// Windows 下 dirs::home_dir 指向真实用户目录且无法用 HOME 重定向，故提供专用覆盖变量
fn app_data_home() -> std::path::PathBuf {
    if let Some(home) = std::env::var_os("MAM_HOME") {
        if !home.is_empty() {
            return std::path::PathBuf::from(home);
        }
    }
    dirs::home_dir().unwrap_or_default()
}
```

替换为：

```rust
/// 应用数据主目录：MAM_HOME 环境变量仅在 debug/test 构建（debug_assertions）生效，
/// 用于集成测试重定向数据目录（Windows 下 dirs::home_dir 无法用 HOME 重定向）；
/// release 生产构建一律使用真实用户目录，防止环境变量误设导致 DB 与 skills/plugins 数据割裂
fn app_data_home() -> std::path::PathBuf {
    if cfg!(debug_assertions) {
        if let Some(home) = std::env::var_os("MAM_HOME") {
            if !home.is_empty() {
                return std::path::PathBuf::from(home);
            }
        }
    }
    dirs::home_dir().unwrap_or_default()
}
```

- [ ] **Step 2: 运行全部测试（集成测试即回归验证）**

```bash
cd src-tauri && cargo test
```

预期：全部 PASS（dao/linker 集成测试通过说明 MAM_HOME 重定向在 debug 下仍生效）。

- [ ] **Step 3: Commit**

```bash
git add src-tauri/src/database/connection.rs
git commit -m "fix(database): restrict MAM_HOME override to debug builds"
```

---

### Task 7: 全量验证与人工验证清单

- [ ] **Step 1: 全量自动化检查**

```bash
cd src-tauri && cargo test
cd src-tauri && cargo clippy --all-targets -- -D warnings
cd src-tauri && cargo fmt --check
cd "E:\LLMproject\Github\MultiAgents-Manager" && pnpm lint
```

预期：全部通过。

- [ ] **Step 2: 人工验证（Windows 实机，`pnpm tauri:dev`）**

1. 启动无闪黑窗（上轮修复的回归项）
2. ChatGPT（含 Codex）运行且有会话：Codex 卡片为 App 形态、**无跳转按钮**（此前误显示且点击报 `Failed to get TTY`）
3. 终端里跑 `claude`（无论 cd 时盘符大小写如何）：Claude 卡片出现，项目名显示为目录名，**无跳转按钮**（Windows 未实现终端聚焦）
4. 终端里跑 `opencode`：OpenCode 卡片出现（若仍不出现，记录 `~/.local/share/opencode/opencode.db` 是否存在及报错日志，勿自行扩大改动范围）
5. macOS 侧（如有条件）：CLI 卡片跳转按钮仍在且可点击（`jump_supported_for` 的平台门控不应影响 macOS）

- [ ] **Step 3: 汇报**

汇报内容：每个 Task 完成状态、`cargo test` 最终摘要、人工验证各项结果（或标注"待用户验证"）、commit 列表（`git log --oneline a39d7c2..HEAD`）。

---

## 范围外（明确不做，另立计划）

- `detect_tools` 的版本检测 UI（参照 cc-switch 的需求）——后端 detector 已存在但无前端调用
- 17 个组件文件的 i18n 硬编码中文清理
- CI 增加 windows-latest 矩阵
- detector 支持 PATHEXT（识别 npm 的 .cmd/.bat shim）
- Cargo.toml 补 `rust-version`（MSRV）
- 全仓 37 处 `dirs::home_dir()` 收敛为单一 `app_home()`（MAM_HOME 门控已是它的最小止血）
- Windows 终端聚焦实现（当前策略为诚实禁用跳转按钮）
