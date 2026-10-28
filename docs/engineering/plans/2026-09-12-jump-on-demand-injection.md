# 跳转按需注入与 hook 通道修复实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 落地 spec `docs/engineering/specs/2026-09-12-jump-on-demand-injection-hook-channel-design.md`——跳转前按需注入 marker（claude/codex 多开自动锁对）、hook 周期注入退役、SessionStart 引号绕开、hook 事件键 session_id 化。

**Architecture:** helper（`mam-marker.exe`）收敛为 `--pid` 唯一形态；MAM 的 `focus_session` 在判定链前对 claude/codex CLI 会话「注入 + 有界等待 ≤500ms」；`status-hook.sh` 回归纯事件记录且事件文件按 session_id 分键；hook 注册命令在路径无空格时不加引号并原地迁移旧带引号条目。

**Tech Stack:** Rust（Tauri 2 后端）、bash hook 脚本、Win32 API（windows crate 0.57：Console / ToolHelp / WindowsAndMessaging）。

## Global Constraints

- 代码注释用中文，git commit message 用英文；不改无关文件。
- marker 口径**两处互引**（改动后）：`src-tauri/src/commands/session.rs`（匹配侧）与 `src-tauri/src/bin/mam-marker.rs`（注入侧）——`MAM:` + session_id 剥连字符前 12 位，改动必须同步两处注释。
- 零回归原则：helper 缺失 / 附加失败 / 等待超时，一律静默跳过注入，跳转行为回落现状。
- win32.rs 与 mam-marker.rs 的 Windows 代码在 mac 上**只能编译验证不能运行**：`cd src-tauri && cargo check/clippy --target x86_64-pc-windows-gnu --all-targets`（CI 同款门禁）；运行验证属第四轮 Windows 实机验收，不在本计划内。
- 工作分支：`feat/marker-and-issue-batch`（基于当前 HEAD 524b588，先 `git pull`）。
- 每个任务一个 commit，验证通过才提交。

---

### Task 1: hook 注册命令去引号 + 旧条目原地迁移

**Files:**
- Modify: `src-tauri/src/monitor/hooks.rs`（`register_hooks_for_tool` 约 97-200 行、`register_all_hooks` 约 254-280 行）

**Interfaces:**
- Produces: `fn quote_bash_command(path_str: &str) -> String`（纯函数，路径含空格 `bash "p"`，否则 `bash p`）；`fn hook_command_for(script_path: &Path) -> String`（Windows 用 quote_bash_command，其他平台裸路径）——Task 2 的 `expected_cmd` 推导复用 `hook_command_for`。

**背景**：claude 在 Windows 经 `powershell -Command "<用户command>"` 包装执行钩子；注册命令 `bash "C:\…\status-hook.sh"` 的内层双引号破坏外层引号配对 → SessionStart 报错（实证）。路径无空格时去引号即可绕开。旧机器上已注册的带引号条目必须**原地改写**（不能追加：追加会双写事件、且旧条目继续报错）。

- [ ] **Step 1: 写失败测试**

在 `src-tauri/src/monitor/hooks.rs` 末尾（`register_all_hooks` 之后）加测试模块：

```rust
#[cfg(test)]
mod command_quote_tests {
    use super::quote_bash_command;

    #[test]
    fn no_space_path_is_unquoted() {
        // 无空格路径不加引号：消除 powershell -Command 包装层的引号嵌套
        // （claude Windows 侧 SessionStart 报错根因，2026-09-12 第三轮探测 C1）
        assert_eq!(
            quote_bash_command(r"C:\Users\bunny\.mam\hooks\status-hook.sh"),
            r"bash C:\Users\bunny\.mam\hooks\status-hook.sh"
        );
    }

    #[test]
    fn spaced_path_keeps_quotes() {
        // 含空格路径必须保引号（已知残留：该形态下 SessionStart 报错可能复现，spec 3.4）
        assert_eq!(
            quote_bash_command(r"C:\Users\John Doe\.mam\hooks\status-hook.sh"),
            r#"bash "C:\Users\John Doe\.mam\hooks\status-hook.sh""#
        );
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test quote_bash_command`
Expected: 编译失败 `cannot find function quote_bash_command`。

- [ ] **Step 3: 实现纯函数 + 接线**

在 `register_hooks_for_tool` 上方加：

```rust
/// Windows hook 命令：路径含空格才加引号。claude 在 Windows 经
/// `powershell -Command "<command>"` 包装执行钩子，内层双引号会破坏外层配对
/// （SessionStart 报错根因）；无空格路径去引号即绕开
fn quote_bash_command(path_str: &str) -> String {
    if path_str.contains(' ') {
        format!("bash \"{path_str}\"")
    } else {
        format!("bash {path_str}")
    }
}

/// 当前平台注册的 hook 命令（注册 / 核验 / 去重三处同源，单一事实源）
fn hook_command_for(script_path: &std::path::Path) -> String {
    let s = script_path.to_string_lossy().to_string();
    if cfg!(windows) {
        quote_bash_command(&s)
    } else {
        s
    }
}
```

把 `register_hooks_for_tool` 内的命令构造（现为 `if cfg!(windows) { format!("bash \"{}\"", …) } else { … }`）替换为：

```rust
    let command_str = hook_command_for(&script_path);
```

- [ ] **Step 4: 旧条目原地迁移**

把 `register_hooks_for_tool` 事件循环里的「检查是否已注册（避免重复）」整块（现以 `let already = arr.iter().any(...)` 判 `c.contains(&command_str)`）替换为（`already`/`migrated_this_event` 为**每个事件**的局部判定，外层另有 `let mut migrated_any = false;` 在循环前声明、每轮 `migrated_any |= migrated_this_event;`）：

```rust
        // 已注册检测 + 旧形态迁移：条目 command 含脚本路径即视为我们注册的。
        // 与当前命令一致 → 跳过；含路径但形态旧（如带引号旧格式）→ 原地改写
        // 为当前命令（追加会造成双写事件且旧条目继续触发 SessionStart 报错）
        let mut already = false;
        let mut migrated_this_event = false;
        if let Some(arr) = hooks_obj.get_mut(&event_name).and_then(|v| v.as_array_mut()) {
            for entry in arr.iter_mut() {
                let Some(cmds) = entry.get_mut("hooks").and_then(|h| h.as_array_mut()) else {
                    continue;
                };
                for h in cmds.iter_mut() {
                    let Some(c) = h
                        .get("command")
                        .and_then(|c| c.as_str())
                        .map(|s| s.to_string())
                    else {
                        continue;
                    };
                    if !c.contains(&script_path_str) {
                        continue; // 用户自己的 hook 条目，不动
                    }
                    if c == command_str {
                        already = true;
                    } else {
                        h["command"] = serde_json::json!(command_str);
                        migrated_this_event = true;
                    }
                }
            }
        }
        migrated_any |= migrated_this_event;
        if already {
            debug!("Hook 已注册: {}", event_name);
            continue;
        }
```

并把函数尾部写盘条件 `if added > 0 {` 改为 `if added > 0 || migrated_any {`（`migrated_any` 在事件循环前声明）。

- [ ] **Step 5: 核验处同源**

把 `register_all_hooks` 里内联的 `expected_cmd` 计算（`if cfg!(windows) { format!("bash \"{}\"", …) } else { … }`）替换为：

```rust
        let expected_cmd = hook_command_for(&script_path);
```

（保留其后 `.map(|c| c.contains(&expected_cmd))` 的核验语义：旧带引号条目核验不通过 → 触发 `register_hooks_for_tool` → 走迁移路径改写。）

- [ ] **Step 6: 跑测试 + 全量快检**

Run: `cd src-tauri && cargo test quote_bash_command && cargo fmt && cargo clippy --all-targets -- -D warnings`
Expected: 2 个测试 PASS；clippy 无告警。

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/monitor/hooks.rs
git commit -m "fix(hooks): unquote bash command for space-free paths, migrate stale entries (#43 C1)

claude wraps hook commands in powershell -Command on Windows; the inner
double quotes broke the outer pairing (SessionStart error). Space-free
paths now register unquoted; stale quoted entries are rewritten in
place instead of appended. Registration, verification and dedup derive
from one hook_command_for source."
```

---

### Task 2: hook 脚本纯事件化 + 事件键 session_id 化

**Files:**
- Modify: `src-tauri/src/monitor/hooks.rs`（`HOOK_SCRIPT` 常量约 11-38 行、`read_hook_events` 约 203-236 行）
- Modify: `src-tauri/src/adapter/mod.rs:306`（消费端一行）

**Interfaces:**
- Produces: `pub fn read_hook_events() -> HashMap<String, HookEvent>`（键从 PPID 改为 session_id 字符串）；`HookEvent` 结构不变。消费方仅 `adapter/mod.rs` 一处。

**背景**：claude 脱管 hook 进程（$PPID 恒 1），全部事件互覆在 `1.json`，消费端按 pid 永远取不到——claude 的 hook 状态通道（Stop 宽限等）事实中断。stdin 自带 session_id，改用它分键。同时按 spec 改动三删除 marker 注入块（hook 周期注入退役）。

- [ ] **Step 1: 写失败测试**

在 `src-tauri/src/monitor/hooks.rs` 测试区（Task 1 的模块之外新增）：

```rust
#[cfg(test)]
mod event_channel_tests {
    use super::*;
    use std::io::Write;

    fn write_event(dir: &std::path::Path, name: &str, event: &str, age_secs: i64) {
        let ts = chrono::Utc::now().timestamp() - age_secs;
        let body = format!(
            r#"{{"event":"{event}","session_id":"sid-x","cwd":"/tmp","ts":{ts},"last_event_at":"2026-09-12T00:00:00Z"}}"#
        );
        let mut f = std::fs::File::create(dir.join(format!("{name}.json"))).unwrap();
        f.write_all(body.as_bytes()).unwrap();
    }

    /// 用独立 tempdir 作为 events 目录跑 read_hook_events（经 MAM_HOME 重定向不可行，
    /// 该函数直接拼 home 路径——测试以子进程隔离或直接抽取核心逻辑。此处选择抽取：
    /// 见 Step 3 的 read_hook_events_from，read_hook_events 成为其薄包装）
    #[test]
    fn events_are_keyed_by_session_id_with_ttl() {
        let tmp = tempfile::tempdir().unwrap();
        write_event(tmp.path(), "01a08083-5ca0", "Stop", 0);
        write_event(tmp.path(), "0f1e2d3c-4b5a", "Stop", 120); // 过期
        write_event(tmp.path(), "1", "Stop", 0); // 旧 PPID 形态孤儿（合法字符，30s 后自然消失）
        let m = read_hook_events_from(tmp.path());
        assert_eq!(m.len(), 2);
        assert!(m.contains_key("01a08083-5ca0"));
        assert!(m.contains_key("1"));
        assert!(!m.contains_key("0f1e2d3c-4b5a"));
    }

    #[test]
    fn script_uses_session_id_key_and_has_no_marker_block() {
        // spec 改动三：hook 周期注入退役；事件键 session_id 化（脚本内容回归锁）
        assert!(HOOK_SCRIPT.contains("$SESSION_ID.json"));
        assert!(!HOOK_SCRIPT.contains("MAM_MARKER"));
        assert!(!HOOK_SCRIPT.contains("mam-marker"));
        assert!(HOOK_SCRIPT.contains("^[A-Za-z0-9-]+$")); // 白名单守卫在场
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test event_channel_tests`
Expected: 编译失败 `cannot find function read_hook_events_from`；`script_uses_session_id_key` 断言失败。

- [ ] **Step 3: 改 HOOK_SCRIPT 与读取函数**

`HOOK_SCRIPT`：删除整个 marker 注入块（`# 注入窗口标题 marker…` 起至 `fi` 止，约 10 行），并把写事件行改为：

```bash
# 事件以 session_id 为键（$PPID 在 claude 脱管 hook 进程里恒为 1，多会话互覆——
# 2026-09-12 第三轮探测 C2 实证；session_id 来自 stdin）。字符白名单外的值
# 直接丢弃（防路径注入；合法 UUID 形态永不触发）。同会话覆盖=保留最新状态
if printf '%s' "$SESSION_ID" | grep -qE '^[A-Za-z0-9-]+$'; then
  echo "{\"event\":\"$EVENT\",\"session_id\":\"$SESSION_ID\",\"cwd\":\"$CWD\",\"ts\":$TS,\"last_event_at\":\"$LAST_EVENT_AT\"}" > "$EVENTS_DIR/$SESSION_ID.json"
fi
```

`read_hook_events` 重构为薄包装 + 可测核心：

```rust
/// 读取所有 Hook 事件文件，返回 session_id → 事件数据的映射（键由脚本侧
/// 文件名承载；旧 PPID 形态文件 30s TTL 内短暂并存、键永不匹配任何会话，无害）
pub fn read_hook_events() -> HashMap<String, HookEvent> {
    let events_dir = dirs::home_dir()
        .unwrap_or_default()
        .join(".mam")
        .join("events");
    read_hook_events_from(&events_dir)
}

/// 核心逻辑（tempdir 可测）：文件名即 session_id，白名单校验 + 30s TTL 过滤
fn read_hook_events_from(events_dir: &std::path::Path) -> HashMap<String, HookEvent> {
    let mut events = HashMap::new();
    if !events_dir.exists() {
        return events;
    }
    let valid_sid = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
    };
    if let Ok(entries) = fs::read_dir(events_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(filename) = path.file_name().and_then(|n| n.to_str()) {
                let Some(sid) = filename.strip_suffix(".json") else {
                    continue;
                };
                if !valid_sid(sid) {
                    continue;
                }
                if let Ok(content) = fs::read_to_string(&path) {
                    if let Ok(event) = serde_json::from_str::<HookEvent>(&content) {
                        let now = chrono::Utc::now().timestamp();
                        if now - event.ts < 30 {
                            events.insert(sid.to_string(), event);
                        }
                    }
                }
            }
        }
    }
    events
}
```

- [ ] **Step 4: 消费端切键（类型变更强制同提交）**

`src-tauri/src/adapter/mod.rs:306`：

```rust
        if let Some(event) = hook_events.get(&session.id) {
```

（原 `hook_events.get(&session.pid)`；grace map 仍按 `session.pid` 键，不动。）

- [ ] **Step 5: 跑测试 + 快检**

Run: `cd src-tauri && cargo test event_channel_tests && cargo test && cargo fmt && cargo clippy --all-targets -- -D warnings`
Expected: 新测试 PASS；全量测试无回归（事件消费相关既有测试如涉及会被类型变更一并修正，不允许留编译错）。

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/monitor/hooks.rs src-tauri/src/adapter/mod.rs
git commit -m "feat(hooks): session_id-keyed event files, drop periodic marker block (#43 C2)

claude detaches its hook processes (PPID=1 across MSYS boundary), so
all events overwrote each other in 1.json and the pid-keyed consumer
never matched -- the claude status channel was effectively dead. Event
files are now named by the session_id from hook stdin (charset-
whitelisted against path injection, stale events dropped). The periodic
marker injection block is retired per spec (on-demand injection
replaces it); read_hook_events returns a session_id-keyed map and the
consumer reads by session.id. Old PPID-named files expire via the
existing 30s TTL; no migration."
```

---

### Task 3: mam-marker 收敛为 --pid 唯一形态

**Files:**
- Modify: `src-tauri/src/bin/mam-marker.rs`（全文件重构定位逻辑；B/A 双路线与纯函数保留）

**Interfaces:**
- Produces: CLI `mam-marker --pid <目标进程pid> <session_id>`；`fn parse_marker_args(args: &[String]) -> Option<(u32, String)>`（纯函数，mac 可测）。Task 4 以 `["--pid", pid, session_id]` 调用本程序。

**背景**：spec 3.1——hook 周期注入退役后父链自走无生产调用方，删除；MAM 主进程以 `--pid` 指定附加目标（第三轮已用 PowerShell `AttachConsole(pid)` 实证外部附加可行）。

- [ ] **Step 1: 写失败测试**

在 `mam-marker.rs` 的 `mod tests` 中追加：

```rust
    #[test]
    fn parse_args_accepts_pid_form() {
        let args = vec![
            "--pid".to_string(),
            "1234".to_string(),
            "01a08083-5ca0-4948".to_string(),
        ];
        assert_eq!(
            parse_marker_args(&args),
            Some((1234, "01a08083-5ca0-4948".to_string()))
        );
    }

    #[test]
    fn parse_args_rejects_missing_flag_or_bad_pid() {
        assert_eq!(parse_marker_args(&["1234".into(), "sid".into()][..]), None);
        assert_eq!(
            parse_marker_args(&["--pid".into(), "abc".into(), "sid".into()][..]),
            None
        );
        assert_eq!(parse_marker_args(&[]), None);
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test --bin mam-marker parse_args`
Expected: 编译失败 `cannot find function parse_marker_args`。

- [ ] **Step 3: 实现参数解析与定位收敛**

文件顶层（非 cfg 部分）加纯函数：

```rust
/// 参数解析：`--pid <目标进程> <session_id>` 唯一形态（spec 2026-09-12 §3.1：
/// hook 周期注入退役后父链自走无生产调用方，删除）
fn parse_marker_args(args: &[String]) -> Option<(u32, String)> {
    match args {
        [flag, pid, sid] if flag == "--pid" => {
            let pid = pid.parse::<u32>().ok()?;
            if sid.is_empty() { None } else { Some((pid, sid.clone())) }
        }
        _ => None,
    }
}
```

`fn main` 改为：

```rust
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    #[cfg(windows)]
    {
        match parse_marker_args(&args) {
            Some((pid, sid)) => std::process::exit(win::run(pid, &sid)),
            None => {
                eprintln!("usage: mam-marker --pid <target-pid> <session_id>");
                std::process::exit(2)
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = args;
        eprintln!("mam-marker is Windows-only（issue #43 窗口标题标记注入）");
        std::process::exit(2);
    }
}
```

`mod win` 内部改造（B/A 双路线保留，定位轴全部换成目标 pid）：

- `run` 签名改为 `pub fn run(target_pid: u32, session_id: &str) -> i32`；
- `route_b`：删除 `ATTACH_PARENT_PROCESS` 与父链回退，直接 `FreeConsole()` 后 `AttachConsole(target_pid)`（失败即返回 false 并记日志「B: 附加目标 {pid} 控制台失败（会话活动中/已退出/权限）」）；
- `route_a`：`ancestor_pids(snap)` 改为从 `target_pid` 起步行走（把现有「自身 pid 起步」改为参数起步：`fn ancestor_chain_from(start: u32, snap: &…) -> Vec<u32>`，包含 start 自身）；
- 末尾日志恢复的 `AttachConsole(ATTACH_PARENT_PROCESS)` 保留（手动调试时可见；被 MAM spawn 时天然无控制台、静默失败）；
- 头部文档注释按 spec 3.1/3.2 更新：调用方改为 MAM 按需注入（`window/win32.rs::inject_marker_on_demand`），删除「hook 调用」表述；marker 口径互引改为两处（session.rs / 本文件）。

- [ ] **Step 4: 跑测试 + 双端编译验证**

Run: `cd src-tauri && cargo test --bin mam-marker && cargo fmt && cargo clippy --target x86_64-pc-windows-gnu --all-targets -- -D warnings`
Expected: 纯函数测试全 PASS（含 Task 3 新增 2 个）；windows 交叉 clippy 无告警（`ATTACH_PARENT_PROCESS` 若仅剩日志恢复处使用则保留 import，否则移除）。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/bin/mam-marker.rs
git commit -m "refactor(marker): collapse helper to --pid-only attach form (#43)

The self parent-chain walk existed solely for the retired periodic hook
injection. The helper now takes --pid <target> <session_id>: route B
attaches the target's console directly, route A resolves the TARGET's
ancestor chain for the single-window SetWindowTextW fallback. B->A
dual-route execution is unchanged (non-overlapping failure modes, B has
no receipt)."
```

---

### Task 4: focus_session 按需注入接线

**Files:**
- Modify: `src-tauri/src/window/win32.rs`（新增 `inject_marker_on_demand`，cfg(windows)）
- Modify: `src-tauri/src/commands/session.rs`（windows 分支接线 + 纯门控函数）

**Interfaces:**
- Consumes: Task 3 的 helper CLI（`--pid <pid> <session_id>`）；既有 `all_windows()`。
- Produces: `pub fn inject_marker_on_demand(session_id: &str, pid: u32, marker: &str)`（win32）；`fn on_demand_marker_applies(agent: Option<&str>, form: Option<&str>) -> bool`（session.rs，`#[cfg(any(windows, test))]`，mac 可测）。

- [ ] **Step 1: 写失败测试（门控纯函数）**

`src-tauri/src/commands/session.rs` 测试区（若无则在文件末尾新建 `#[cfg(test)] mod on_demand_tests`）：

```rust
#[cfg(test)]
mod on_demand_tests {
    // 门控契约（spec §3.2）：仅 claude/codex 且仅 CLI 形态注入；其余零开销跳过
    use super::on_demand_marker_applies;

    #[test]
    fn applies_to_claude_and_codex_cli_only() {
        assert!(on_demand_marker_applies(Some("claude"), Some("cli")));
        assert!(on_demand_marker_applies(Some("codex"), Some("cli")));
        assert!(on_demand_marker_applies(Some("claude"), None)); // form 缺失按 CLI 处理
        assert!(!on_demand_marker_applies(Some("claude"), Some("app")));
        assert!(!on_demand_marker_applies(Some("kimi"), Some("cli")));   // 标题键够用
        assert!(!on_demand_marker_applies(Some("opencode"), Some("cli"))); // 同上
        assert!(!on_demand_marker_applies(None, Some("cli")));
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test on_demand_tests`
Expected: 编译失败 `cannot find function on_demand_marker_applies`。

- [ ] **Step 3: 实现门控函数与注入函数**

`commands/session.rs`（windows 分支附近，`#[cfg(any(windows, test))]` 门控）：

```rust
/// 按需注入门控（spec §3.2）：仅无可靠静态标题键的 claude/codex 且 CLI 形态。
/// kimi/opencode 标题键已实测够用，注入只会污染其标题
#[cfg(any(windows, test))]
fn on_demand_marker_applies(agent: Option<&str>, form: Option<&str>) -> bool {
    matches!(agent, Some("claude" | "codex")) && form != Some("app")
}
```

`window/win32.rs` 末尾（`focus_hwnd` 之后）新增：

```rust
/// 跳转前按需注入（spec 2026-09-12 §3.2）：spawn helper 附加目标会话控制台贴
/// marker，随后有界等待 marker 出现在任一候选窗口标题（≤500ms——第三轮实测
/// idle 态存活 ≥15s，等待的是 ConPTY 渲染延迟）。全部失败路径静默返回，
/// 判定链照旧执行（零回归）
pub fn inject_marker_on_demand(session_id: &str, pid: u32, marker: &str) {
    use std::os::windows::process::CommandExt;
    if marker.is_empty() {
        return;
    }
    let helper = dirs::home_dir()
        .unwrap_or_default()
        .join(".mam")
        .join("bin")
        .join("mam-marker.exe");
    if !helper.is_file() {
        return; // 未随包分发/被清理：合法状态，跳过
    }
    // CREATE_NO_WINDOW：GUI 进程 spawn 控制台子程序防闪窗
    let _ = std::process::Command::new(&helper)
        .arg("--pid")
        .arg(pid.to_string())
        .arg(session_id)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(0x0800_0000)
        .spawn();
    let marker_lc = marker.to_lowercase();
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
    while std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
        let wins = all_windows();
        if wins
            .by_pid
            .values()
            .any(|v| v.iter().any(|(_, t)| t.to_lowercase().contains(&marker_lc)))
        {
            break;
        }
    }
}
```

`focus_session` windows 分支：在 marker 构造之后、`running_projects` 计算之前插入：

```rust
        // 按需注入（spec 2026-09-12 §3.2）：claude/codex CLI 会话在判定链前贴
        // marker，① 层即可精确命中；helper 缺失/失败/超时全部静默回落
        if on_demand_marker_applies(agent_type.as_deref(), form.as_deref()) {
            if let Some(sid) = session_id.as_deref() {
                if let Some(m) = marker.as_deref() {
                    crate::window::win32::inject_marker_on_demand(sid, pid, m);
                }
            }
        }
```

- [ ] **Step 4: 跑测试 + 双端验证**

Run: `cd src-tauri && cargo test on_demand_tests && cargo fmt && cargo clippy --all-targets -- -D warnings && cargo clippy --target x86_64-pc-windows-gnu --all-targets -- -D warnings`
Expected: 门控测试 PASS；两端 clippy 无告警。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/window/win32.rs src-tauri/src/commands/session.rs
git commit -m "feat(jump): on-demand marker injection ahead of the resolve chain (#43)

For claude/codex CLI sessions: spawn the helper against the session's
pid, wait bounded (<=500ms) for the marker to surface in a candidate
window title, then run the existing five-layer chain where layer 1
matches exactly. Idle sessions hold the marker >=15s (round-3 probe);
active ones simply fall through. Every failure path is silent --
missing helper, attach failure, timeout -- leaving current behavior
intact."
```

---

### Task 5: 全量回归收尾

**Files:**
- 无新文件；汇总验证 + 修复任何遗留。

- [ ] **Step 1: Rust 全量（mac）**

Run: `cd src-tauri && cargo fmt --check && MAM_HOME=$(mktemp -d) cargo test`
Expected: 全部 PASS（基线 444 + 本计划新增约 6 个）。

- [ ] **Step 2: Windows 交叉门禁（CI 同款）**

Run: `cd src-tauri && cargo check --target x86_64-pc-windows-gnu --all-targets && cargo clippy --target x86_64-pc-windows-gnu --all-targets -- -D warnings`
Expected: 无告警。

- [ ] **Step 3: 前端零回归确认**

Run: `pnpm check && pnpm test -- --run`
Expected: 通过（本计划无前端改动，确认无意外波及）。

- [ ] **Step 4: 推送**

Run: `git push`
Expected: 推送成功，CI（含 windows 交叉门禁）应全绿；用 `gh run watch` 盯完为止。

- [ ] **Step 5: 收尾 commit（仅在有遗留修正时）**

如 Step 1-3 产生修正：`git commit -m "chore: round-4 prep regression fixes"`；否则无 commit。

---

## 完成定义（对照 spec 验收）

- 本计划只覆盖**代码与 mac/CI 可验证部分**；spec §6.2 的 Windows 实机验收（claude 双开自动锁对、codex 双开自动锁对、SessionStart 无报错、事件分键生效、无 helper 零回归）属第四轮实机验收，由 MAM 侧 reviewer（本会话）出探测提示词另行执行。
- 全部任务完成后：PR #56 保持 draft，等第四轮验收结论再转正。
