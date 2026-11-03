# WorkBuddy 适配 + APP 跳转与已读机制 + 工具勾选管理 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 按 spec（`docs/engineering/specs/2026-09-03-workbuddy-app-jump-tool-toggle-design.md`）实现 W1-W5 五个阶段：通知面统一与气泡修复、APP 跳转（session 级优先 + APP 级保底）、WorkBuddy 适配器、APP 类已读机制、工具勾选管理。

**Architecture:** 复用 AgentAdapter trait 注册新工具；未读卡以 `Session.unread` 合并进 `get_all_sessions` 返回值（看板/宠物/通知管线零改造）；APP 激活走 `window/app_activation.rs`（macOS AppleScript）+ win32 兜底；工具启停以 `agent_tools.enabled`（DB）为单一事实源。

**Tech Stack:** Tauri 2 + Rust（rusqlite/sysinfo/serde）+ React 19 + TypeScript + shadcn/ui + Tailwind v4 + react-query + i18next。

## Global Constraints

- 设计文档/代码注释中文，commit message 英文（conventional commits）。
- 门禁：`cd src-tauri && cargo test && cargo clippy`；`pnpm check`（format + lint + i18n 键位齐平 + build）。前端无既有测试基建（vitest 未配置用例），前端验证 = lint/build + `pnpm tauri:dev` 手动验证。
- macOS 与 Windows 均须编译通过（`#[cfg]` 分支都要 `cargo check` 过）。
- WorkBuddy 心跳/JSONL/DB 均为未文档化私有格式：任何文件缺失或解析失败必须跳过/降级，禁止 panic。
- 未读卡生命周期为**单轨物理删除**（已读/关闭/变黄/过期/宿主退出/工具取消勾选六种终态直接删行）。
- 会话 id（session.id）即各工具原生会话标识：Codex rollout UUID、WorkBuddy sessionId UUID。

---

## Phase W1：通知面统一与宠物气泡修复（commit 1）

### Task 1: petSuppressPopup 简化 + 气泡点击即清除

**Files:**
- Modify: `src/components/pet/petConfig.ts:130-133`
- Modify: `src/components/pet/FoxbellPet.tsx:261-286`
- Modify: `src/hooks/useNotification.ts:171-172`（仅注释）

**Interfaces:**
- Consumes: 无
- Produces: `petSuppressPopup(): boolean`（语义：宠物可见即压制浮窗+系统通知）

- [x] **Step 1: 修改 petSuppressPopup**

`src/components/pet/petConfig.ts` 中（注意：`petSoundTakeover` 已是 visible-only，无需改动）：

```ts
/** 通知浮窗抑制：宠物开启即抑制——气泡是唯一通知面（spec W1，浮窗与系统通知降级路径
 *  均在 useNotification 的同一 if 内，天然一并静默） */
export function petSuppressPopup(): boolean {
  return loadVisible();
}
```

- [x] **Step 2: 气泡点击失败也清除**

`src/components/pet/FoxbellPet.tsx` 的 `jump()`（第 262-286 行），将：

```ts
    } catch {
      return; // 跳转失败：卡片保留（spec §13）
    }
    ackDone(statusStateRef.current ?? {}, card.id); // 点击已读即消（spec C2）
    setCards(cardsFromState(statusStateRef.current ?? {}));
```

改为：

```ts
    } catch {
      // 跳转失败也清除气泡（spec W1）：气泡是瞬时提醒，不因跳转失败卡死；
      // 看板上的未读状态由 W4 已读机制独立管理，不在此处丢
    }
    ackDone(statusStateRef.current ?? {}, card.id); // 点击已读即消（spec C2）
    setCards(cardsFromState(statusStateRef.current ?? {}));
```

- [x] **Step 3: 修正 useNotification 误导注释**

`src/hooks/useNotification.ts:171-172` 的注释改为（代码本身不用改，系统通知降级本来就在 `petSuppressPopup()` 守卫内部）：

```ts
        // 发送通知：应用内浮窗为主路径，失败降级系统 toast（两者都在宠物压制守卫内）
        // 宠物可见时全部静默：头顶气泡是唯一通知面（spec W1）
```

- [x] **Step 4: 验证**

Run: `pnpm check`
Expected: PASS

手动验证（`pnpm tauri:dev`）：① 宠物可见（不置顶）时触发会话状态变化 → 右下角浮窗与系统通知均不出现，宠物头顶出卡片；② 宠物可见 + 点击 Codex APP 会话气泡（macOS 当前跳转必失败）→ 气泡消失不卡死；③ 宠物隐藏 → 浮窗恢复出现。

- [x] **Step 5: Commit**

```bash
git add src/components/pet/petConfig.ts src/components/pet/FoxbellPet.tsx src/hooks/useNotification.ts
git commit -m "fix(pet): dismiss bubble on click regardless of jump result; pet-visible suppresses all popups"
```

---

## Phase W2：APP 跳转（commit 2）

### Task 2: macOS APP bundle 提取与激活模块

**Files:**
- Create: `src-tauri/src/window/app_activation.rs`
- Modify: `src-tauri/src/window/mod.rs:1-10`（注册模块）
- Test: `src-tauri/src/window/app_activation.rs`（`#[cfg(test)]` 内联）

**Interfaces:**
- Consumes: `super::applescript::execute_applescript(&str) -> Result<(), String>`（若其为私有，本任务将其改 `pub(crate)`）
- Produces:
  - `app_bundle_from_exe(exe: &str) -> Option<String>`（纯函数）
  - `bundle_matches_agent(bundle_lower: &str, agent_type: &str) -> bool`（纯函数）
  - `activate_app_bundle(bundle: &str) -> Result<(), String>`

- [x] **Step 1: 写失败测试**

创建 `src-tauri/src/window/app_activation.rs`（先只含测试，函数未定义则编译失败即"失败测试"）：

```rust
// macOS APP 激活：从进程可执行路径提取 .app bundle 并激活（W2 保底路径）
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_chatgpt_bundle_from_nested_codex() {
        assert_eq!(
            app_bundle_from_exe(
                "/Applications/ChatGPT.app/Contents/Frameworks/Codex.framework/Versions/A/Codex"
            )
            .as_deref(),
            Some("/Applications/ChatGPT.app")
        );
    }

    #[test]
    fn extracts_workbuddy_bundle() {
        assert_eq!(
            app_bundle_from_exe(
                "/Applications/WorkBuddy.app/Contents/Resources/app.asar.unpacked/cli/bin/codebuddy"
            )
            .as_deref(),
            Some("/Applications/WorkBuddy.app")
        );
    }

    #[test]
    fn returns_none_for_cli_path() {
        assert_eq!(app_bundle_from_exe("/usr/local/bin/codex"), None);
        assert_eq!(app_bundle_from_exe("/opt/homebrew/bin/claude"), None);
    }

    #[test]
    fn takes_last_app_segment_for_nested_apps() {
        // 路径含多个 .app 段时取最内层（离可执行文件最近的）
        assert_eq!(
            app_bundle_from_exe("/Applications/WorkBuddy.app/Contents/Frameworks/Helper.app/Contents/MacOS/Helper").as_deref(),
            Some("/Applications/WorkBuddy.app/Contents/Frameworks/Helper.app")
        );
    }

    #[test]
    fn bundle_matches_agent_rules() {
        assert!(bundle_matches_agent("/applications/chatgpt.app", "codex"));
        assert!(bundle_matches_agent("/applications/codex.app", "codex"));
        assert!(!bundle_matches_agent("/applications/workbuddy.app", "codex"));
        assert!(bundle_matches_agent("/applications/workbuddy.app", "workbuddy"));
        assert!(!bundle_matches_agent("/applications/chatgpt.app", "claude"));
    }
}
```

在 `window/mod.rs` 顶部模块声明区（第 1-8 行后）加：

```rust
#[cfg(target_os = "macos")]
pub mod app_activation;
```

- [x] **Step 2: 运行测试确认失败**

Run: `cd src-tauri && cargo test app_activation`
Expected: 编译失败（`app_bundle_from_exe` 未定义）

- [x] **Step 3: 实现**

在 `app_activation.rs` 顶部（测试模块之前）加入：

```rust
use std::path::Path;

/// 从可执行路径提取 .app bundle 根目录（取最内层 .app 段）
/// "/Applications/WorkBuddy.app/Contents/.../codebuddy" → "/Applications/WorkBuddy.app"
pub fn app_bundle_from_exe(exe: &str) -> Option<String> {
    let normalized = exe.replace('\\', "/");
    let idx = normalized.rfind(".app/")?;
    Some(normalized[..idx + ".app".len()].to_string())
}

/// bundle 路径（小写）是否属于该工具的宿主 APP（W2 pid 失效兜底的匹配规则）
fn bundle_matches_agent(bundle_lower: &str, agent_type: &str) -> bool {
    match agent_type {
        "codex" => bundle_lower.ends_with("chatgpt.app") || bundle_lower.ends_with("codex.app"),
        "workbuddy" => bundle_lower.ends_with("workbuddy.app"),
        // 其他工具暂无 APP 形态；新增 APP 类工具时在此补一行
        _ => false,
    }
}

/// 激活 APP（AppleScript，bundle 路径精确指定，避免同名歧义）
pub fn activate_app_bundle(bundle: &str) -> Result<(), String> {
    let script = format!(
        "activate application \"{}\"",
        bundle.replace('\"', "\\\"")
    );
    super::applescript::execute_applescript(&script)
}

/// 供 Task 3 使用的、按工具匹配 bundle 的公开包装（测试经此覆盖）
pub fn bundle_matches_agent_pub(bundle_lower: &str, agent_type: &str) -> bool {
    bundle_matches_agent(bundle_lower, agent_type)
}
```

（若 `execute_applescript` 当前非 `pub`，在 `window/applescript.rs` 将其声明改为 `pub fn`。测试中的 `bundle_matches_agent` 调用同步改名为 `bundle_matches_agent_pub`。）

- [x] **Step 4: 运行测试确认通过**

Run: `cd src-tauri && cargo test app_activation`
Expected: 5 个测试 PASS

- [x] **Step 5: Commit**

```bash
git add src-tauri/src/window/app_activation.rs src-tauri/src/window/mod.rs src-tauri/src/window/applescript.rs
git commit -m "feat(jump): macOS app bundle extraction and activation module"
```

### Task 3: focus_session 接线（App 分支 + pid 失效兜底）+ jump_supported_for

**Files:**
- Create: `src-tauri/src/window/deep_link.rs`（骨架，Task 4 填充路由）
- Modify: `src-tauri/src/session/model.rs:34-42` 与 `:100-118`（jump 矩阵 + 测试）
- Modify: `src-tauri/src/window/mod.rs`（新增 `activate_agent_app`）
- Modify: `src-tauri/src/commands/session.rs:34-75`（两个平台分支接线）
- Modify: `src-tauri/src/window/win32.rs`（文件末尾新增兜底函数）
- Test: `src-tauri/src/session/model.rs`（既有测试更新）

**Interfaces:**
- Consumes: Task 2 的 `app_activation::{app_bundle_from_exe, activate_app_bundle, bundle_matches_agent_pub}`
- Produces:
  - `window::activate_agent_app(pid: u32, agent_type: Option<&str>) -> Option<serde_json::Value>`（macOS）
  - `window::win32::reactivate_tool_app(system: &sysinfo::System, agent_type: Option<&str>) -> Result<(), String>`（Windows）
  - `window::deep_link::session_url(agent_type: &str, session_id: &str) -> Option<String>`（本任务恒 `None`，Task 4 填充）
  - `window::deep_link::open_url(url: &str) -> Result<(), String>`

- [x] **Step 1: 更新 jump_supported_for 及其测试（失败测试）**

`session/model.rs:34-42` 改为：

```rust
/// 跳转终端是否可用：Windows 下 CLI 与 App 均可窗口级聚焦（见 window/win32.rs）；
/// macOS 下 CLI 走 TTY 链路、App 走 activate application（W2）；其他平台不支持
pub fn jump_supported_for(form: ProcessForm) -> bool {
    if cfg!(windows) {
        return true;
    }
    cfg!(target_os = "macos")
}
```

`session/model.rs:100-118` 的 `jump_tests` 改为：

```rust
#[cfg(test)]
mod jump_tests {
    use super::*;

    #[test]
    fn jump_supported_matches_platform_matrix() {
        // Windows：CLI 与 App 均可窗口级聚焦；macOS：CLI 走 TTY、App 走 APP 激活（W2）；
        // 其他平台：不支持
        if cfg!(windows) {
            assert!(jump_supported_for(ProcessForm::Cli));
            assert!(jump_supported_for(ProcessForm::App));
        } else if cfg!(target_os = "macos") {
            assert!(jump_supported_for(ProcessForm::Cli));
            assert!(jump_supported_for(ProcessForm::App));
        } else {
            assert!(!jump_supported_for(ProcessForm::Cli));
            assert!(!jump_supported_for(ProcessForm::App));
        }
    }
}
```

同时 `Session` 结构体注释（第 62-65 行）更新：`/// 进程形态（CLI / 桌面 APP）` `/// 是否支持跳转（CLI=TTY 链路，App=APP 激活）`。

- [x] **Step 2: 运行测试确认失败**

Run: `cd src-tauri && cargo test jump_supported`
Expected: macOS 上 FAIL（App 现在应为 true）——本步骤仅改测试先行；随后实现已在 Step 1 同步给出（矩阵函数本体），因此本任务测试与实现同改，Step 2 运行应直接 PASS（红-绿合并为一轮，因纯配置矩阵无独立实现空间）。记录实际输出。

- [x] **Step 3: 创建 deep_link 骨架**

创建 `src-tauri/src/window/deep_link.rs`：

```rust
// 深度链接跳转（W2 第一顺位）：session 级直达依赖 URL scheme 路由格式，
// Task 4 探测；未探明前 session_url 恒返回 None，走 APP 级保底
#[cfg(target_os = "macos")]
fn open_url_macos(url: &str) -> Result<(), String> {
    std::process::Command::new("open")
        .arg(url)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("open url failed: {}", e))
}

#[cfg(windows)]
fn open_url_windows(url: &str) -> Result<(), String> {
    std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("start url failed: {}", e))
}

/// 打开外部 URL（跨平台）
pub fn open_url(url: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    return open_url_macos(url);
    #[cfg(windows)]
    return open_url_windows(url);
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = url;
        Err("当前平台不支持 URL 打开".to_string())
    }
}

/// 该工具的「直达具体会话」深度链接。路由格式未探明前恒 None（Task 4 填充）。
pub fn session_url(_agent_type: &str, _session_id: &str) -> Option<String> {
    None
}
```

在 `window/mod.rs` 模块声明区加：

```rust
#[cfg(any(target_os = "macos", windows))]
pub mod deep_link;
```

- [x] **Step 4: 实现 activate_agent_app（macOS）**

`window/mod.rs` 末尾追加：

```rust
/// APP 激活入口（W2）：优先深度链接直达会话 → pid 提取 bundle → 按工具枚举兜底。
/// 返回 Some(json) 表示跳转成功，None 表示无法激活
#[cfg(target_os = "macos")]
pub fn activate_agent_app(
    pid: u32,
    agent_type: Option<&str>,
    session_id: Option<&str>,
) -> Option<serde_json::Value> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

    // 1) 第一顺位：深度链接直达 session（路由格式由 deep_link 模块决定，未探明则跳过）
    if let (Some(agent), Some(sid)) = (agent_type, session_id) {
        if let Some(url) = deep_link::session_url(agent, sid) {
            if deep_link::open_url(&url).is_ok() {
                return Some(serde_json::json!({ "type": "focused" }));
            }
        }
    }

    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::new().with_exe(sysinfo::UpdateKind::Always),
    );

    // 2) pid 仍存活：取其 exe 提取 .app bundle 激活（pid=0/已退出时自然跳过）
    if pid > 0 {
        if let Some(proc) = system.process(sysinfo::Pid::from_u32(pid)) {
            if let Some(exe) = proc.exe() {
                if let Some(bundle) = app_activation::app_bundle_from_exe(&exe.to_string_lossy()) {
                    if app_activation::activate_app_bundle(&bundle).is_ok() {
                        return Some(serde_json::json!({ "type": "focused" }));
                    }
                }
            }
        }
    }

    // 3) pid 失效兜底：枚举该工具任一 App 形态进程，激活其宿主 bundle
    //    （自洽保证：未读卡存在 ⇒ 宿主进程必存活 ⇒ 此步必有目标）
    let target = agent_type.map(|a| a.to_lowercase());
    for (_, proc) in system.processes() {
        let Some(exe) = proc.exe() else { continue };
        let Some(bundle) = app_activation::app_bundle_from_exe(&exe.to_string_lossy()) else {
            continue;
        };
        if let Some(t) = &target {
            if !app_activation::bundle_matches_agent_pub(&bundle.to_lowercase(), t) {
                continue;
            }
        }
        if app_activation::activate_app_bundle(&bundle).is_ok() {
            return Some(serde_json::json!({ "type": "focused" }));
        }
    }
    None
}
```

- [x] **Step 5: Windows pid 失效兜底**

`window/win32.rs` 末尾追加（复用 `all_windows()`/`force_foreground()` 若可见性不足则按编译器提示调整）：

```rust
/// pid 失效兜底（W2）：枚举该工具 App 进程的可见顶层窗口并聚焦。
/// Electron 单实例应用直接重拉 exe 亦可聚焦，但窗口枚举不引入子进程，优先采用
pub fn reactivate_tool_app(
    system: &sysinfo::System,
    agent_type: Option<&str>,
) -> Result<(), String> {
    let marker = match agent_type.map(|a| a.to_lowercase()).as_deref() {
        Some("workbuddy") => "workbuddy",
        Some("codex") => "chatgpt",
        _ => return Err("未知工具，无法兜底激活".to_string()),
    };
    let wins = all_windows();
    for (hwnd, pid) in wins.by_pid.iter() {
        let Some(proc) = system.process(sysinfo::Pid::from_u32(*pid)) else { continue };
        let exe = proc
            .exe()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if exe.contains(marker) && force_foreground(*hwnd) {
            return Ok(());
        }
    }
    Err("未找到该工具的可聚焦窗口".to_string())
}
```

注意：`all_windows()` 返回的内部结构（`AllWindows`）字段名以实际代码为准——实现时先读 `win32.rs:159-170` 的 `all_windows` 与 `AllWindows` 定义，按其真实字段（窗口→PID 分组）改写上面循环的迭代方式；`force_foreground` 若为私有则改 `pub(crate)`。语义不变：找 owner 进程 exe 含 marker 的可见窗口 → 前置。

- [x] **Step 6: 接线 focus_session**

`commands/session.rs:34-75` 两分支改造。

Windows 分支（第 42-69 行）在 `match resolve_and_focus(...)` 的 `Err(e)` 臂改为：

```rust
            Err(e) => {
                // pid 失效兜底（W2）：pid 已死时按工具激活宿主 APP 窗口
                let pid_dead = system.process(sysinfo::Pid::from_u32(pid)).is_none();
                if pid_dead && reactivate_tool_app(&system, agent_type.as_deref()).is_ok() {
                    Ok(serde_json::json!({ "type": "focused" }))
                } else {
                    Err(e)
                }
            }
```

（`use crate::window::win32::reactivate_tool_app;` 或全路径调用。）

非 Windows 分支（第 70-74 行）改为：

```rust
    #[cfg(not(windows))]
    {
        let _ = (project_name, last_message);
        // CLI 形态：TTY 链路（tmux/iTerm2/Terminal.app）
        if crate::window::focus_terminal_for_pid(pid).is_ok() {
            return Ok(serde_json::json!({ "type": "focused" }));
        }
        // APP 形态 / pid 失效兜底：深度链接 → bundle 激活 → 按工具枚举（W2）
        if let Some(out) = crate::window::activate_agent_app(
            pid,
            agent_type.as_deref(),
            session_id.as_deref(),
        ) {
            return Ok(out);
        }
        Err(format!(
            "无法聚焦目标（pid={}）：进程无 TTY 且未找到宿主 APP",
            pid
        ))
    }
```

- [x] **Step 7: 验证**

Run: `cd src-tauri && cargo test && cargo clippy`
Expected: 全部 PASS（macOS 实测机）；Windows 分支经 `cargo check --target` 不可行则依赖 CI/实现期在 Windows 验证（计划注记：win32 改动需在 Windows 机器 `cargo test` 一次）。

手动验证：macOS 上打开 WorkBuddy/ChatGPT，看板点 Codex APP 会话卡 → 目标 APP 到前台（不再弹"不支持跳转"toast）。

- [x] **Step 8: Commit**

```bash
git add src-tauri/src/window/ src-tauri/src/session/model.rs src-tauri/src/commands/session.rs
git commit -m "feat(jump): app-level activation with pid-dead fallback on both platforms"
```

### Task 4: 深度链接路由探测与接线

**Files:**
- Modify: `src-tauri/src/window/deep_link.rs`
- Modify: `docs/engineering/specs/2026-09-03-workbuddy-app-jump-tool-toggle-design.md`（探测结论回写风险表）

**Interfaces:**
- Consumes: Task 3 的 `session_url` 骨架
- Produces: `session_url` 的真实路由表（或维持 None 的结论记录）

- [x] **Step 1: 探测路由格式（Spike）**（由控制器执行；实测路由：`workbuddy://chat/<sessionId>`、`codex://threads/<threadId>`，证据与注意事项见 `session_url` 文档注释及 spec 第 9 节回写）

依次执行并记录输出：

```bash
# WorkBuddy：在解包资源中搜 deep link 路由常量
grep -rnoE "workbuddy://[a-zA-Z/-]+" /Applications/WorkBuddy.app/Contents/Resources/app.asar.unpacked 2>/dev/null | sort -u | head -20
# 主 asar（二进制 grep，asar 未解包也能搜到明文）
strings /Applications/WorkBuddy.app/Contents/Resources/app.asar 2>/dev/null | grep -oE "workbuddy://[a-zA-Z/-]+" | sort -u | head -20
# Codex（ChatGPT.app）
strings "/Applications/ChatGPT.app/Contents/MacOS/ChatGPT" 2>/dev/null | grep -oE "codex://[a-zA-Z/-]+" | sort -u | head -20
find "/Applications/ChatGPT.app/Contents/Resources" -name "*.asar" -exec strings {} \; 2>/dev/null | grep -oE "codex://[a-zA-Z/-]+" | sort -u | head -20
```

判定规则：若找到形如 `workbuddy://session/<xxx>` / `codex://thread/<xxx>` 的**带参数路由**（而非仅裸 scheme），进入 Step 2 接线；若只找到裸 scheme 或无结果，`session_url` 维持 `None`，直接跳 Step 4 回写结论。

- [x] **Step 2: 填充路由表（条件执行）**

`deep_link.rs` 的 `session_url` 改为（路由以 Step 1 实测为准，下方为占位结构示例——**必须用实测值替换后提交**；若实测无路由则本步骤整体跳过）：

```rust
/// 该工具的「直达具体会话」深度链接。
/// 路由格式来源：<探测日期 + 命令输出摘录>；未探明的工具返回 None 走 APP 级保底
pub fn session_url(agent_type: &str, session_id: &str) -> Option<String> {
    match agent_type {
        // 实测示例：workbuddy://session/<sessionId>
        "workbuddy" => Some(format!("workbuddy://session/{}", session_id)),
        // 实测示例：codex://thread/<sessionId>
        "codex" => Some(format!("codex://thread/{}", session_id)),
        _ => None,
    }
}
```

- [ ] **Step 3: 验证（条件执行）**（手动 GUI 直达验证 deferred to GUI phase）

Run: `cd src-tauri && cargo test && cargo clippy && pnpm tauri:dev`
手动：macOS 点 WorkBuddy 未读卡 → 是否直达对应会话界面。若直达失败（APP 打开但停在原界面），说明路由猜测错误——回退 Step 2 为 None 并记录，APP 级保底不受影响。

- [x] **Step 4: 回写探测结论到 spec 风险表**

在 spec 第 9 节风险表「深度链接路由格式未知」一行的"应对"列追加实测结论（找到/未找到 + 具体格式或"以 APP 级保底交付"）。

- [x] **Step 5: Commit**

```bash
git add src-tauri/src/window/deep_link.rs docs/engineering/specs/2026-09-03-workbuddy-app-jump-tool-toggle-design.md
git commit -m "feat(jump): wire session-level deep links if route format discovered"
```
（已完成：commit 5bd2a39）

---

## Phase W3：WorkBuddy 适配器（commit 3）

### Task 5: WorkBuddy 骨架注册（编译绿）

**Files:**
- Modify: `src-tauri/src/session/model.rs:4-12`（AgentType）
- Modify: `src-tauri/src/adapter/mod.rs:4-8,110-124`（模块声明 + TOOL_IDS + adapter_by_id）
- Create: `src-tauri/src/adapter/workbuddy.rs`（最小实现）
- Create: `src-tauri/src/monitor/workbuddy_parser.rs`（空解析器）
- Modify: `src-tauri/src/monitor/mod.rs:2-14`（模块声明）
- Modify: `src-tauri/src/monitor/process.rs:197-200`（进程发现）
- Modify: `src/types/session.ts:3`（前端类型）

**Interfaces:**
- Produces: `AgentType::WorkBuddy`（serde `workbuddy`）、`TOOL_IDS` 含 `"workbuddy"`、`find_workbuddy_processes(system) -> Vec<AgentProcess>`、`monitor::workbuddy_parser::get_workbuddy_sessions(&[AgentProcess]) -> Vec<Session>`（本任务返回空）

- [x] **Step 1: AgentType 与注册**

`session/model.rs` 枚举加变体（保持 serde lowercase）：

```rust
pub enum AgentType {
    Claude,
    Codex,
    OpenCode,
    OpenClaw,
    Kimi,
    WorkBuddy,
}
```

`adapter/mod.rs`：模块声明区加 `pub mod workbuddy;`；`TOOL_IDS` 改为：

```rust
pub const TOOL_IDS: &[&str] = &[
    "claude",
    "codex",
    "opencode",
    "openclaw",
    "kimi",
    "workbuddy",
];
```

`adapter_by_id` match 加臂：`"workbuddy" => Some(Box::new(workbuddy::WorkBuddyAdapter)),`

- [x] **Step 2: 最小 adapter**

创建 `src-tauri/src/adapter/workbuddy.rs`：

```rust
// WorkBuddy（腾讯全场景 AI 办公工作台，Electron APP）适配器
// 会话运行时 = APP 内嵌 cli/bin/codebuddy；状态提取见 monitor/workbuddy_parser.rs

use super::*;

pub struct WorkBuddyAdapter;

impl AgentAdapter for WorkBuddyAdapter {
    fn name(&self) -> &'static str {
        "WorkBuddy"
    }
    fn agent_type(&self) -> AgentType {
        AgentType::WorkBuddy
    }
    fn process_names(&self) -> &'static [&'static str] {
        &["codebuddy"]
    }

    fn find_processes(&self, system: &System) -> Vec<AgentProcess> {
        monitor::process::find_workbuddy_processes(system)
    }

    fn find_sessions(&self, processes: &[AgentProcess]) -> Vec<Session> {
        monitor::workbuddy_parser::get_workbuddy_sessions(processes)
    }

    fn base_dir(&self) -> std::path::PathBuf {
        dirs::home_dir().unwrap_or_default().join(".workbuddy")
    }

    fn mcp_format(&self) -> McpFormat {
        McpFormat::Json
    }
    fn mcp_config_path(&self) -> Option<std::path::PathBuf> {
        Some(self.base_dir().join("mcp.json"))
    }

    fn skill_dirs(&self) -> Vec<std::path::PathBuf> {
        super::primary_skill_dir("workbuddy")
            .map(|dir| vec![dir])
            .unwrap_or_else(|| vec![self.base_dir().join("skills")])
    }

    // WorkBuddy 插件为市场化版本化管理，不纳入 MAM（spec W3）；无 hook 机制
}
```

- [x] **Step 3: 空解析器 + 进程发现 + skill 目录**

创建 `src-tauri/src/monitor/workbuddy_parser.rs`：

```rust
// WorkBuddy 会话解析：心跳文件（~/.workbuddy/sessions/<PID>.json）关联进程与会话，
// 会话历史在 ~/.workbuddy/projects/<路径编码>/<sessionId>.jsonl
// Task 6 实现完整逻辑；本文件先提供编译占位

use crate::adapter::AgentProcess;
use crate::session::Session;

pub fn get_workbuddy_sessions(_processes: &[AgentProcess]) -> Vec<Session> {
    Vec::new()
}
```

`monitor/mod.rs` 模块声明加 `pub mod workbuddy_parser;`。

`monitor/process.rs` 末尾加：

```rust
/// 发现 WorkBuddy 会话进程（codebuddy；活跃性由心跳文件过滤，见 workbuddy_parser）
/// 注：独立安装的腾讯 CodeBuddy CLI 同名进程无 ~/.workbuddy 心跳，由解析器天然排除
pub fn find_workbuddy_processes(system: &System) -> Vec<AgentProcess> {
    find_processes_by_names(system, &["codebuddy"], &["multi-agents-manager"])
}
```

`adapter/mod.rs` 的 `skill_dir_for_tool`（第 335-347 行）match 加臂：

```rust
        "workbuddy" => Some(home_dir.join(".workbuddy").join("skills")),
```

`src/types/session.ts` 第 3 行改为：

```ts
export type AgentType = "claude" | "codex" | "opencode" | "openclaw" | "kimi" | "workbuddy";
```

- [x] **Step 4: 验证编译（含穷尽匹配修复）**

Run: `cd src-tauri && cargo check`
Expected: 若 Rust 端存在对 `AgentType` 的穷尽 match 导致编译错误，按编译器指引在对应位置补 `AgentType::WorkBuddy` 臂（预期场景：无——既有代码多用 `format!("{:?}")` 与字符串键，不穷尽匹配）。PASS 后运行 `cargo test`。

- [x] **Step 5: Commit**

```bash
git add src-tauri/src/ src/types/session.ts
git commit -m "feat(adapter): WorkBuddy skeleton registration (agent type, process discovery, paths)"
```

### Task 6: workbuddy_parser 完整实现（TDD 重头）

**Files:**
- Modify: `src-tauri/src/monitor/workbuddy_parser.rs`（全部实现 + 内联测试）
- Test: 同文件 `#[cfg(test)]`

**Interfaces:**
- Consumes: `monitor::jsonl::read_recent_lines`、`monitor::project::project_name_from_path`、`session::{Session, SessionStatus, jump_supported_for}`
- Produces:
  - `mangle_project_path(cwd: &str) -> String`
  - `Heartbeat { pid, session_id, cwd, last_heartbeat_ms }` + `parse_heartbeat(&str) -> Option<Heartbeat>`
  - `heartbeat_session_id_is_uuid(&Heartbeat) -> bool`
  - `HEARTBEAT_FRESH_MS: u64 = 90_000`
  - `session_jsonl_path(home: &Path, cwd: &str, session_id: &str) -> PathBuf`
  - `derive_status_from_tail(lines: &[String]) -> SessionStatus`
  - `title_from_db(home: &Path, session_id: &str) -> Option<String>`
  - `LAST_SEEN_SESSIONS: Lazy<Mutex<HashMap<u32, String>>>`（Task 10 补偿用）
  - `get_workbuddy_sessions(&[AgentProcess]) -> Vec<Session>`

- [x] **Step 1: 写失败测试**

`workbuddy_parser.rs` 测试模块（真实格式 fixture，来自 2026-09-03 实机抓取）：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const HEARTBEAT_ACTIVE: &str = r#"{
      "pid": 11952,
      "lastHeartbeat": 1788444900119,
      "sessionId": "7005f4cd-ef8b-4b7c-bcc5-b0f914c8a58c",
      "cwd": "/Users/jarvis/Documents/MultiAgents-Manager",
      "startedAt": 1788444900112,
      "kind": "interactive",
      "updatedAt": 1788444900347
    }"#;

    const HEARTBEAT_SERVE: &str = r#"{
      "pid": 8979,
      "lastHeartbeat": 1788445813951,
      "sessionId": "interactive-8979",
      "cwd": "/private/var/folders/xx/T/workbuddy-host-cli/xxx",
      "kind": "interactive",
      "url": "http://127.0.0.1:50027"
    }"#;

    #[test]
    fn mangle_strips_leading_slash_and_replaces_separators() {
        assert_eq!(
            mangle_project_path("/Users/jarvis/Documents/MultiAgents-Manager"),
            "Users-jarvis-Documents-MultiAgents-Manager"
        );
        // Windows 形态容错：反斜杠路径同样编码
        assert_eq!(
            mangle_project_path("C:\\Users\\jarvis\\proj"),
            "C:-Users-jarvis-proj"
        );
    }

    #[test]
    fn heartbeat_uuid_session_is_real_task() {
        let hb = parse_heartbeat(HEARTBEAT_ACTIVE).unwrap();
        assert_eq!(hb.pid, 11952);
        assert!(heartbeat_session_id_is_uuid(&hb));
        let serve = parse_heartbeat(HEARTBEAT_SERVE).unwrap();
        assert!(!heartbeat_session_id_is_uuid(&serve)); // --serve 排除
    }

    #[test]
    fn heartbeat_parse_rejects_garbage() {
        assert!(parse_heartbeat("not json").is_none());
        assert!(parse_heartbeat("{}").is_none()); // 缺 sessionId
    }

    #[test]
    fn heartbeat_freshness() {
        let hb = parse_heartbeat(HEARTBEAT_ACTIVE).unwrap();
        assert!(heartbeat_is_alive(&hb, hb.last_heartbeat_ms + 1));
        assert!(!heartbeat_is_alive(&hb, hb.last_heartbeat_ms + HEARTBEAT_FRESH_MS + 1));
    }

    #[test]
    fn session_jsonl_path_layout() {
        let p = session_jsonl_path(
            std::path::Path::new("/home/u"),
            "/Users/jarvis/Documents/MultiAgents-Manager",
            "7005f4cd-ef8b-4b7c-bcc5-b0f914c8a58c",
        );
        assert_eq!(
            p,
            std::path::PathBuf::from(
                "/home/u/.workbuddy/projects/Users-jarvis-Documents-MultiAgents-Manager/7005f4cd-ef8b-4b7c-bcc5-b0f914c8a58c.jsonl"
            )
        );
    }

    #[test]
    fn tail_user_message_is_thinking() {
        let lines = vec![
            r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"跑测试"}]}"#.into(),
        ];
        assert_eq!(derive_status_from_tail(&lines), SessionStatus::Thinking);
    }

    #[test]
    fn tail_function_call_is_processing() {
        let lines = vec![
            r#"{"type":"function_call","name":"shell"}"#.into(),
        ];
        assert_eq!(derive_status_from_tail(&lines), SessionStatus::Processing);
    }

    #[test]
    fn tail_assistant_text_is_idle() {
        let lines = vec![
            r#"{"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"完成"}]}"#.into(),
        ];
        assert_eq!(derive_status_from_tail(&lines), SessionStatus::Idle);
    }

    #[test]
    fn tail_last_entry_wins() {
        let lines = vec![
            r#"{"type":"function_call","name":"shell"}"#.into(),
            r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"好"}]}"#.into(),
        ];
        assert_eq!(derive_status_from_tail(&lines), SessionStatus::Idle);
    }

    #[test]
    fn tail_empty_is_waiting() {
        assert_eq!(derive_status_from_tail(&[]), SessionStatus::Waiting);
    }
}
```

- [x] **Step 2: 运行测试确认失败**

Run: `cd src-tauri && cargo test workbuddy_parser`
Expected: 编译失败（函数未定义）

- [x] **Step 3: 实现**

`workbuddy_parser.rs` 占位函数替换为完整实现：

```rust
// WorkBuddy 会话解析：心跳文件（~/.workbuddy/sessions/<PID>.json）关联进程与会话，
// 会话历史在 ~/.workbuddy/projects/<路径编码>/<sessionId>.jsonl（OpenAI 风格 type/role/content）
// 所有文件均为未文档化私有格式：解析失败一律跳过/降级，禁止 panic（spec W3 防御性要求）

use crate::adapter::AgentProcess;
use crate::session::{jump_supported_for, AgentType, ProcessForm, Session, SessionStatus};
use once_cell::sync::Lazy;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// 心跳新鲜阈值：取 MAM 轮询周期（约 30s）的 3 倍，防止轮询间隙卡片闪烁
pub const HEARTBEAT_FRESH_MS: u64 = 90_000;

/// 每轮观测到的 pid → sessionId（Task 10 心跳消失补偿的依据）
pub static LAST_SEEN_SESSIONS: Lazy<Mutex<HashMap<u32, String>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

#[derive(Debug, Deserialize)]
pub struct Heartbeat {
    pub pid: u32,
    #[serde(rename = "sessionId")]
    pub session_id: String,
    pub cwd: String,
    #[serde(rename = "lastHeartbeat")]
    pub last_heartbeat_ms: u64,
}

pub fn parse_heartbeat(json: &str) -> Option<Heartbeat> {
    serde_json::from_str(json).ok()
}

/// sessionId 为 UUID（非 interactive-*）才是真实任务会话；--serve 常驻服务排除
pub fn heartbeat_session_id_is_uuid(hb: &Heartbeat) -> bool {
    hb.session_id.len() == 36
        && hb.session_id.bytes().filter(|c| *c == b'-').count() == 4
}

pub fn heartbeat_is_alive(hb: &Heartbeat, now_ms: u64) -> bool {
    now_ms.saturating_sub(hb.last_heartbeat_ms) < HEARTBEAT_FRESH_MS
}

/// 项目路径编码：去首分隔符后 / 与 \ 统一替换为 -
pub fn mangle_project_path(cwd: &str) -> String {
    let trimmed = cwd.trim_start_matches('/');
    trimmed.replace(['/', '\\'], "-")
}

pub fn session_jsonl_path(home: &Path, cwd: &str, session_id: &str) -> PathBuf {
    home.join(".workbuddy")
        .join("projects")
        .join(mangle_project_path(cwd))
        .join(format!("{}.jsonl", session_id))
}

/// JSONL 尾部状态推导：最后一条有效条目决定状态（spec W3 映射）
pub fn derive_status_from_tail(lines: &[String]) -> SessionStatus {
    let mut last: Option<&String> = None;
    for line in lines {
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(v) if v.get("type").is_some() => last = Some(line),
            _ => continue,
        }
    }
    let Some(line) = last else { return SessionStatus::Waiting };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return SessionStatus::Waiting;
    };
    match v["type"].as_str().unwrap_or_default() {
        "message" => match v["role"].as_str().unwrap_or_default() {
            "user" => SessionStatus::Thinking,
            _ => SessionStatus::Idle, // assistant 完成
        },
        "function_call" | "function_call_result" => SessionStatus::Processing,
        // reasoning/file-history-snapshot 等中间条目按运行中处理
        _ => SessionStatus::Processing,
    }
}

/// 会话标题：只读打开 workbuddy.db 读 sessions.title；失败降级 None（调用方再降级首条 user 消息）
pub fn title_from_db(home: &Path, session_id: &str) -> Option<String> {
    use rusqlite::OpenFlags;
    let db = home.join(".workbuddy").join("workbuddy.db");
    let conn = rusqlite::Connection::open_with_flags(
        &db,
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .ok()?;
    conn.query_row(
        "SELECT title FROM sessions WHERE id = ?1 AND deleted_at IS NULL",
        [session_id],
        |row| row.get::<_, Option<String>>(0),
    )
    .ok()
    .flatten()
    .filter(|t| !t.trim().is_empty())
}

fn heartbeat_path(home: &Path, pid: u32) -> PathBuf {
    home.join(".workbuddy").join("sessions").join(format!("{}.json", pid))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 主入口：活跃心跳的 codebuddy 进程 → 每会话一张卡
pub fn get_workbuddy_sessions(processes: &[AgentProcess]) -> Vec<Session> {
    let mut sessions = Vec::new();
    let Some(home) = dirs::home_dir() else { return sessions };
    let now = now_ms();

    for process in processes {
        // 防御：心跳文件缺失/损坏 → 跳过该进程（含独立 CodeBuddy CLI、空闲 prewarm）
        let Some(hb) = std::fs::read_to_string(heartbeat_path(&home, process.pid))
            .ok()
            .and_then(|s| parse_heartbeat(&s))
        else {
            continue;
        };
        if !heartbeat_session_id_is_uuid(&hb) || !heartbeat_is_alive(&hb, now) {
            continue;
        }

        let jsonl = session_jsonl_path(&home, &hb.cwd, &hb.session_id);
        if !jsonl.exists() {
            continue; // 会话文件未落盘（防御）
        }

        // 尾部解析（复用通用 JSONL 尾读设施；行数与 codex 一致 500）
        let lines = crate::monitor::jsonl::read_recent_lines(&jsonl, 500);
        let status = derive_status_from_tail(&lines);
        let last_message = lines
            .iter()
            .rev()
            .find_map(|l| extract_message_text(l))
            .unwrap_or_default();

        let title = title_from_db(&home, &hb.session_id)
            .or_else(|| first_user_text(&lines))
            .map(|t| t.chars().take(60).collect::<String>());

        sessions.push(Session {
            id: hb.session_id.clone(),
            agent_type: AgentType::WorkBuddy,
            project_name: crate::monitor::project::project_name_from_path(&hb.cwd),
            project_path: hb.cwd.clone(),
            title,
            git_branch: None,
            github_url: crate::monitor::git::get_github_url(&hb.cwd),
            status,
            last_message: if last_message.is_empty() { None } else { Some(last_message) },
            last_message_role: None,
            last_activity_at: jsonl
                .metadata()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| {
                    chrono::DateTime::from_timestamp(d.as_secs() as i64, 0)
                        .map(|dt| dt.to_rfc3339())
                        .unwrap_or_default()
                })
                .unwrap_or_default(),
            pid: process.pid,
            cpu_usage: process.cpu_usage,
            active_subagent_count: 0,
            form: ProcessForm::App,
            jump_supported: jump_supported_for(ProcessForm::App),
            unread: false,
        });

        // 记录本轮 pid→session（心跳消失补偿依据）
        LAST_SEEN_SESSIONS
            .lock()
            .unwrap()
            .insert(process.pid, hb.session_id.clone());
    }
    sessions
}

fn extract_message_text(line: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if v["type"].as_str()? != "message" {
        return None;
    }
    v["content"]
        .as_array()?
        .iter()
        .find_map(|c| {
            c.get("text")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string())
        })
        .filter(|s| !s.trim().is_empty())
}

fn first_user_text(lines: &[String]) -> Option<String> {
    lines
        .iter()
        .filter_map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).ok()?;
            if v["type"].as_str() == Some("message") && v["role"].as_str() == Some("user") {
                extract_message_text(l)
            } else {
                None
            }
        })
        .next()
}
```

注意：`Session` 的 `unread` 字段由 Task 9 添加——若本任务先于 Task 9 执行（推荐顺序执行，不会发生），编译器会提示。按计划顺序 Task 9 在后，因此**将 `unread: false` 字段行挪到 Task 9 是错误顺序**——正确做法：Task 6 先不加 `unread` 字段（Task 9 统一给全库构造点加）。执行本任务时构造体末两行用：

```rust
            form: ProcessForm::App,
            jump_supported: jump_supported_for(ProcessForm::App),
```

（Task 9 会以 `cargo check` 编译错误为清单，给所有 Session 构造点补 `unread: false`。）

同样，`read_recent_lines` 的实际签名以 `monitor/jsonl.rs` 为准（codex_parser 用法是 `read_recent_lines(path, RECENT_LINES)` 或仅 path——实现时对照 `codex_parser.rs:17` 的调用方式调整）。

- [x] **Step 4: 运行测试确认通过**

Run: `cd src-tauri && cargo test workbuddy_parser`
Expected: 9 个测试 PASS（实际 Step 1 代码块含 10 个测试，10 个全部 PASS）

- [ ] **Step 5: 实机冒烟**（延后至 GUI 阶段执行：WorkBuddy APP 保持开启、跑一个真实任务）

Run: `cd src-tauri && cargo test test_get_all_sessions -- --nocapture`（WorkBuddy APP 保持开启、跑一个真实任务）
Expected: 输出含 `[WorkBuddy] <项目名> Idle/Processing pid=<心跳pid> form=App jump=true`

- [x] **Step 6: Commit**

```bash
git add src-tauri/src/monitor/workbuddy_parser.rs
git commit -m "feat(adapter): WorkBuddy session parser with heartbeat filtering and tail status derivation"
```

### Task 7: 前端接入 WorkBuddy（徽标/标签/工具列）

**Files:**
- Modify: `src/lib/agentBadge.tsx:13-48`
- Modify: `src/components/resources/ResourceByKindView.tsx:30-36`
- Modify: `src/components/resources/ResourceByToolView.tsx:11-17`
- Modify: `src/pages/settings.tsx:340` 附近的工具声音配置硬编码列表（以 grep `TOOLS` 定位第三处）

**Interfaces:**
- Consumes: Task 5 的 `AgentType "workbuddy"`
- Produces: 前端展示 WorkBuddy 徽标/名称/资源列

- [x] **Step 1: agentBadge 与标签**

`agentBadge.tsx`：`getAgentLabel` 加 `if (agentType === "workbuddy") return "WorkBuddy";`；`AGENT_BADGE` 加：

```tsx
  workbuddy: {
    label: "WorkBuddy",
    // 腾讯蓝；无品牌素材暂用占位图标（同 openclaw 约定）
    className: "border-blue-500/30 bg-blue-500/15 text-blue-400",
    Icon: OpenCodeIcon,
  },
```

- [x] **Step 2: 三处 TOOLS 列表补条目**

`ResourceByKindView.tsx:30-36`、`ResourceByToolView.tsx:11-17`、settings.tsx 声音配置处，各加 `{ id: "workbuddy", label: "WorkBuddy" },`（byTool 视图标签风格与既有一致）。Task 15 会把前两处改为后端下发，此处先按现状接入保证 W3 独立可验证。

- [x] **Step 3: 验证**（`pnpm check` 全绿；手动 GUI 验证项顺延至 GUI 阶段）

Run: `pnpm check`
手动（`pnpm tauri:dev`，WorkBuddy 装有 markitdown-skill、mcp.json 有 context7）：① 看板出现 WorkBuddy 会话卡（徽标+项目名+状态色）；② 资源管理「按工具」视图出现 WorkBuddy 列且能扫描出 markitdown-skill 与 context7；③ 设置-通知声音区出现 WorkBuddy。

- [x] **Step 4: Commit**

```bash
git add src/lib/agentBadge.tsx src/components/resources/ src/pages/settings.tsx
git commit -m "feat(ui): WorkBuddy badge, labels and resource columns"
```

---

## Phase W4：APP 类已读机制（commit 4）

### Task 8: unread_sessions 表与 DAO

**Files:**
- Modify: `src-tauri/src/database/schema.rs`（第 75-82 行附近追加表）
- Create: `src-tauri/src/database/dao/unread.rs`
- Modify: `src-tauri/src/database/dao/mod.rs`、`src-tauri/src/database/mod.rs`（导出）
- Test: `src-tauri/src/database/dao/unread.rs` 内联（内存库）

**Interfaces:**
- Produces:
  - `UnreadSessionRecord { tool_id, session_id, project_name, title, last_message, turned_green_at_ms, expires_at_ms }`
  - `upsert_unread(conn, &record)`、`delete_unread(conn, tool_id, session_id)`、`list_unread(conn) -> Vec<record>`（未过期）、`clear_unread_for_tool(conn, tool_id)`、`cleanup_expired_unread(conn, now_ms)`
  - 全局连接包装：`unread::upsert(&record)` 等（内部锁 `DB`）

- [x] **Step 1: schema 加表**

`schema.rs` 的 execute_batch 内追加（既有库经 `CREATE TABLE IF NOT EXISTS` 自动建表——`connection.rs` 的 `DB` Lazy 每次启动都会跑 `schema::init`）：

```sql
        CREATE TABLE IF NOT EXISTS unread_sessions (
            tool_id          TEXT NOT NULL,
            session_id       TEXT NOT NULL,
            project_name     TEXT NOT NULL DEFAULT '',
            title            TEXT,
            last_message     TEXT,
            turned_green_at  INTEGER NOT NULL,
            expires_at       INTEGER NOT NULL,
            PRIMARY KEY (tool_id, session_id)
        );
```

- [x] **Step 2: 写失败测试**

创建 `dao/unread.rs`：

```rust
// 未读会话（APP 类绿色已完成、用户未查看）持久层 — 单轨物理删除（spec W4）
#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        conn
    }

    fn rec(tool: &str, sid: &str, at: i64) -> UnreadSessionRecord {
        UnreadSessionRecord {
            tool_id: tool.into(),
            session_id: sid.into(),
            project_name: "proj".into(),
            title: Some("标题".into()),
            last_message: Some("消息".into()),
            turned_green_at_ms: at,
            expires_at_ms: at + 24 * 3600 * 1000,
        }
    }

    #[test]
    fn upsert_then_list_and_dedupe() {
        let conn = mem();
        upsert_unread(&conn, &rec("workbuddy", "s1", 1000));
        upsert_unread(&conn, &rec("workbuddy", "s1", 2000)); // 同键覆盖
        assert_eq!(list_unread(&conn, 3000).len(), 1);
        assert_eq!(list_unread(&conn, 3000)[0].turned_green_at_ms, 2000);
    }

    #[test]
    fn delete_single_and_clear_tool() {
        let conn = mem();
        upsert_unread(&conn, &rec("workbuddy", "s1", 1000));
        upsert_unread(&conn, &rec("workbuddy", "s2", 1000));
        upsert_unread(&conn, &rec("codex", "s3", 1000));
        delete_unread(&conn, "workbuddy", "s1");
        clear_unread_for_tool(&conn, "codex");
        let left = list_unread(&conn, 2000);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].session_id, "s2");
    }

    #[test]
    fn expired_not_listed_and_cleanup_removes() {
        let conn = mem();
        upsert_unread(&conn, &rec("workbuddy", "old", 1000));
        let far = 1000 + 24 * 3600 * 1000 + 1;
        assert!(list_unread(&conn, far).is_empty());
        cleanup_expired_unread(&conn, far);
        assert!(list_unread(&conn, far).is_empty());
        // 物理删除后行数归零
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM unread_sessions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }
}
```

- [x] **Step 3: 运行测试确认失败**

Run: `cd src-tauri && cargo test dao::unread`
Expected: 编译失败

- [x] **Step 4: 实现**

`dao/unread.rs` 顶部加：

```rust
use rusqlite::{params, Connection};

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnreadSessionRecord {
    pub tool_id: String,
    pub session_id: String,
    pub project_name: String,
    pub title: Option<String>,
    pub last_message: Option<String>,
    pub turned_green_at_ms: i64,
    pub expires_at_ms: i64,
}

pub fn upsert_unread(conn: &Connection, r: &UnreadSessionRecord) {
    let _ = conn.execute(
        "INSERT INTO unread_sessions
            (tool_id, session_id, project_name, title, last_message, turned_green_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(tool_id, session_id) DO UPDATE SET
            project_name = excluded.project_name,
            title = excluded.title,
            last_message = excluded.last_message,
            turned_green_at = excluded.turned_green_at,
            expires_at = excluded.expires_at",
        params![
            r.tool_id, r.session_id, r.project_name, r.title, r.last_message,
            r.turned_green_at_ms, r.expires_at_ms
        ],
    );
}

pub fn delete_unread(conn: &Connection, tool_id: &str, session_id: &str) {
    let _ = conn.execute(
        "DELETE FROM unread_sessions WHERE tool_id = ?1 AND session_id = ?2",
        params![tool_id, session_id],
    );
}

/// 未过期未读列表（按转绿时间倒序）
pub fn list_unread(conn: &Connection, now_ms: i64) -> Vec<UnreadSessionRecord> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT tool_id, session_id, project_name, title, last_message, turned_green_at, expires_at
         FROM unread_sessions WHERE expires_at > ?1
         ORDER BY turned_green_at DESC",
    ) else { return Vec::new() };
    stmt.query_map(params![now_ms], |row| {
        Ok(UnreadSessionRecord {
            tool_id: row.get(0)?,
            session_id: row.get(1)?,
            project_name: row.get(2)?,
            title: row.get(3)?,
            last_message: row.get(4)?,
            turned_green_at_ms: row.get(5)?,
            expires_at_ms: row.get(6)?,
        })
    })
    .map(|rows| rows.filter_map(Result::ok).collect())
    .unwrap_or_default()
}

pub fn clear_unread_for_tool(conn: &Connection, tool_id: &str) {
    let _ = conn.execute(
        "DELETE FROM unread_sessions WHERE tool_id = ?1",
        params![tool_id],
    );
}

pub fn cleanup_expired_unread(conn: &Connection, now_ms: i64) {
    let _ = conn.execute(
        "DELETE FROM unread_sessions WHERE expires_at <= ?1",
        params![now_ms],
    );
}

// ---- 全局连接包装（业务侧零锁代码） ----

pub fn upsert(r: &UnreadSessionRecord) {
    let conn = crate::database::connection::DB.lock().unwrap();
    upsert_unread(&conn, r);
}

pub fn delete(tool_id: &str, session_id: &str) {
    let conn = crate::database::connection::DB.lock().unwrap();
    delete_unread(&conn, tool_id, session_id);
}

pub fn list(now_ms: i64) -> Vec<UnreadSessionRecord> {
    let conn = crate::database::connection::DB.lock().unwrap();
    list_unread(&conn, now_ms)
}

pub fn clear_tool(tool_id: &str) {
    let conn = crate::database::connection::DB.lock().unwrap();
    clear_unread_for_tool(&conn, tool_id);
}
```

`dao/mod.rs` 加 `pub mod unread;`；`database/mod.rs` 加导出：

```rust
pub use dao::unread::{
    clear_tool as clear_unread_tool, delete as delete_unread, list as list_unread_sessions,
};
pub use dao::unread::UnreadSessionRecord;
```

- [x] **Step 5: 运行测试确认通过**

Run: `cd src-tauri && cargo test dao::unread`
Expected: 3 个测试 PASS

- [x] **Step 6: Commit**

```bash
git add src-tauri/src/database/
git commit -m "feat(sessions): unread_sessions table and DAO with single-track deletion"
```

### Task 9: Session.unread 字段 + 监控循环合并与维护

**Files:**
- Modify: `src-tauri/src/session/model.rs:44-66`（字段）
- Create: `src-tauri/src/monitor/host.rs`（宿主存活判定）
- Modify: `src-tauri/src/monitor/mod.rs`（声明）
- Modify: `src-tauri/src/adapter/mod.rs:199-304`（get_all_sessions 尾部接线）
- Modify: `src/types/session.ts:10-27`
- Test: `monitor/host.rs` 内联；全库构造点由 `cargo check` 暴露

**Interfaces:**
- Consumes: Task 8 DAO
- Produces:
  - `Session.unread: bool`（serde `unread`）
  - `monitor::host::is_host_process(exe_lower: &str, tool_id: &str) -> bool`（纯函数）、`tool_host_alive(tool_id: &str) -> bool`
  - `adapter::sync_unread_sessions(&mut Vec<Session>)`（合并 + 维护，供 get_all_sessions 调用）

- [x] **Step 1: 写 host 判定失败测试**

创建 `monitor/host.rs`：

```rust
// 宿主进程存活判定（spec W4）：宿主 = .app 包内的非会话运行时进程。
// 会话进程（codebuddy/Codex 框架进程）不参与判定——APP 崩溃后的孤儿会话进程
// 不得导致误判「宿主还活着」、未读卡不清理
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workbuddy_host_is_electron_not_codebuddy() {
        assert!(is_host_process(
            "/applications/workbuddy.app/contents/macos/electron",
            "workbuddy"
        ));
        assert!(is_host_process(
            "/applications/workbuddy.app/contents/frameworks/workbuddy helper.app/contents/macos/workbuddy helper",
            "workbuddy"
        ));
        // 会话进程：不判定为宿主（孤儿防线）
        assert!(!is_host_process(
            "/applications/workbuddy.app/contents/resources/app.asar.unpacked/cli/bin/codebuddy",
            "workbuddy"
        ));
        assert!(!is_host_process("/usr/local/bin/codebuddy", "workbuddy"));
        // 其他 APP 不匹配
        assert!(!is_host_process(
            "/applications/chatgpt.app/contents/macos/chatgpt",
            "workbuddy"
        ));
    }

    #[test]
    fn codex_host_is_chatgpt_main() {
        assert!(is_host_process(
            "/applications/chatgpt.app/contents/macos/chatgpt",
            "codex"
        ));
        // 内嵌 Codex 框架进程 = 会话运行时，不算宿主
        assert!(!is_host_process(
            "/applications/chatgpt.app/contents/frameworks/codex",
            "codex"
        ));
    }
}
```

`monitor/mod.rs` 加 `pub mod host;`

- [x] **Step 2: 运行测试确认失败**

Run: `cd src-tauri && cargo test host`
Expected: 编译失败

- [x] **Step 3: 实现 host + Session.unread**

`monitor/host.rs` 顶部实现：

```rust
/// exe 路径（已小写）是否为该工具的宿主进程
pub fn is_host_process(exe_lower: &str, tool_id: &str) -> bool {
    match tool_id {
        "workbuddy" => {
            exe_lower.contains("workbuddy.app/") && !exe_lower.contains("codebuddy")
        }
        "codex" => {
            (exe_lower.contains("chatgpt.app/") || exe_lower.contains("codex.app/"))
                && !exe_lower.contains("frameworks") // 主进程在 Contents/MacOS 下
        }
        _ => false,
    }
}

/// 该工具宿主 APP 是否存活（独立 sysinfo 扫描，用未过滤的原始进程集）
pub fn tool_host_alive(tool_id: &str) -> bool {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::new().with_exe(sysinfo::UpdateKind::Always),
    );
    system.processes().iter().any(|(_, p)| {
        p.exe()
            .map(|e| is_host_process(&e.to_string_lossy().to_lowercase(), tool_id))
            .unwrap_or(false)
    })
}
```

`session/model.rs` 的 `Session` 结构体加字段（`jump_supported` 之后）：

```rust
    /// 未读标记（W4）：true = 绿色已完成且用户未查看的持久未读卡（APP 类专用）
    pub unread: bool,
```

运行 `cargo check`，按编译器错误清单在所有 `Session { ... }` 构造点补 `unread: false,`（预期文件：`monitor/claude_parser.rs`、`monitor/codex_parser.rs`、`monitor/opencode_parser.rs`、`monitor/openclaw_parser.rs`、`monitor/kimi_parser.rs`、`monitor/workbuddy_parser.rs`）。

`src/types/session.ts` 的 `Session` 接口加 `unread: boolean;`。

- [x] **Step 4: 测试 host 通过**

Run: `cd src-tauri && cargo test host`
Expected: 2 个测试 PASS

- [x] **Step 5: 实现 sync_unread_sessions 并接线**

`adapter/mod.rs`：`get_all_sessions` 在 `dedup_sessions(&mut all_sessions);`（第 201 行）之后加一行：

```rust
    // W4：APP 类未读卡合并 + 未读池维护（宿主存活检查 / 变黄删除 / 过期清理）
    sync_unread_sessions(&mut all_sessions);
```

`adapter/mod.rs` 末尾（skill_dir 函数之前）新增：

```rust
/// W4 未读机制核心：把 DB 中的未读会话合并为 Session 卡，并维护未读池
/// - 会话当前非空闲（黄/红）→ 删未读行（活跃卡可见，防同会话双卡）
/// - 转绿（Idle/Finished）→ upsert 未读行（未在池中时）
/// - 宿主 APP 进程全部退出 → 清空该工具未读行与在板未读卡
/// - 过期（24h）→ 清理
fn sync_unread_sessions(active: &mut Vec<Session>) {
    let now_ms = chrono::Utc::now().timestamp_millis();

    // 1) 活跃会话驱动未读池变更
    for s in active.iter() {
        if !matches!(s.form, ProcessForm::App) {
            continue; // 仅 APP 类参与（spec W4 范围）
        }
        let tool = format!("{:?}", s.agent_type).to_lowercase();
        let idle = matches!(
            s.status,
            SessionStatus::Idle | SessionStatus::Finished
        );
        if idle {
            crate::database::dao::unread::upsert(&crate::database::dao::unread::UnreadSessionRecord {
                tool_id: tool,
                session_id: s.id.clone(),
                project_name: s.project_name.clone(),
                title: s.title.clone(),
                last_message: s.last_message.clone(),
                turned_green_at_ms: now_ms,
                expires_at_ms: now_ms + 24 * 3600 * 1000,
            });
        } else {
            // 变黄/红：删未读（状态迁移，非重置机制）
            crate::database::dao::unread::delete(&tool, &s.id);
        }
    }

    // 2) 宿主进程退出 → 清该工具全部未读（运行中被关 + 重启残留检查统一规则）
    let unread_now = crate::database::dao::unread::list(now_ms);
    let mut dead_tools: Vec<String> = Vec::new();
    for r in &unread_now {
        if !dead_tools.contains(&r.tool_id) && !crate::monitor::host::tool_host_alive(&r.tool_id) {
            dead_tools.push(r.tool_id.clone());
        }
    }
    for t in &dead_tools {
        crate::database::dao::unread::clear_tool(t);
    }

    // 3) 过期清理
    {
        let conn = crate::database::connection::DB.lock().unwrap();
        crate::database::dao::unread::cleanup_expired_unread(&conn, now_ms);
    }

    // 4) 未读池合并为卡（跳过当前已在板的活跃会话；按转绿时间倒序追加在末尾）
    let active_keys: HashSet<(String, String)> = active
        .iter()
        .map(|s| (format!("{:?}", s.agent_type).to_lowercase(), s.id.clone()))
        .collect();
    let final_unread = crate::database::dao::unread::list(now_ms);
    for r in final_unread {
        if active_keys.contains(&(r.tool_id.clone(), r.session_id.clone())) {
            continue;
        }
        let Ok(agent_type) = serde_json::from_value::<AgentType>(serde_json::json!(r.tool_id))
        else {
            continue;
        };
        active.push(Session {
            id: r.session_id.clone(),
            agent_type,
            project_name: r.project_name.clone(),
            project_path: String::new(),
            title: r.title.clone(),
            git_branch: None,
            github_url: None,
            status: SessionStatus::Idle,
            last_message: r.last_message.clone(),
            last_message_role: None,
            last_activity_at: chrono::DateTime::from_timestamp_millis(r.turned_green_at_ms)
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_default(),
            pid: 0, // pid 失效场景：跳转走 activate_agent_app 的按工具兜底
            cpu_usage: 0.0,
            active_subagent_count: 0,
            form: ProcessForm::App,
            jump_supported: jump_supported_for(ProcessForm::App),
            unread: true,
        });
    }
}
```

（`use` 补充：`ProcessForm`、`SessionStatus`、`jump_supported_for`、`AgentType` 已在文件头 `crate::session::{...}` 引入列表中，按需追加。`serde_json::from_value::<AgentType>(json!(r.tool_id))` 依赖 AgentType 的 lowercase serde——`"workbuddy"`/`"codex"` 均合法。）

- [x] **Step 6: 验证**

Run: `cd src-tauri && cargo test && cargo clippy`
Expected: PASS

- [x] **Step 7: Commit**

```bash
git add src-tauri/src/
git commit -m "feat(sessions): merge persistent unread cards into session scan with host-liveness maintenance"
```

### Task 10: Codex APP 每会话一卡 + WorkBuddy 心跳消失补偿

**Files:**
- Modify: `src-tauri/src/monitor/codex_parser.rs:96-124`（Phase 2 重构为聚合）
- Modify: `src-tauri/src/monitor/workbuddy_parser.rs`（补偿函数）
- Modify: `src-tauri/src/adapter/mod.rs`（get_all_sessions 调补偿）
- Test: `codex_parser.rs` 内联新增聚合测试

**Interfaces:**
- Consumes: Task 9 的未读 DAO 与 `LAST_SEEN_SESSIONS`
- Produces:
  - `codex_parser::aggregate_app_sessions(parsed: &[(PathBuf, Option<Session>)], mtimes: &[std::time::SystemTime], app_processes: &[AgentProcess]) -> Vec<Session>`（纯函数）
  - `workbuddy_parser::compensate_vanished_heartbeats()`

- [x] **Step 1: 写聚合失败测试**

`codex_parser.rs` 测试模块加：（实际落在独立 `aggregate_tests` 模块；测试 1 的 mtime 由 UNIX_EPOCH 改为 now-1h/now-1min——实现的 24h 新鲜窗口以 `SystemTime::now()` 为基准，UNIX_EPOCH 样本必被过滤，测试 2 的"窗口外"断言亦证窗口为 now 基准；另用 title 区分 s1 新旧文件使"取最新"可观测）

```rust
    #[test]
    fn aggregate_groups_by_session_id_and_picks_latest() {
        let mk = |id: &str, proj: &str| Session {
            id: id.into(),
            agent_type: AgentType::Codex,
            project_name: proj.into(),
            project_path: format!("/tmp/{}", proj),
            title: None,
            git_branch: None,
            github_url: None,
            status: SessionStatus::Idle,
            last_message: None,
            last_message_role: None,
            last_activity_at: String::new(),
            pid: 0,
            cpu_usage: 0.0,
            active_subagent_count: 0,
            form: ProcessForm::App,
            jump_supported: true,
            unread: false,
        };
        let parsed = vec![
            (PathBuf::from("/a-rollout-s1-old"), Some(mk("s1", "P1"))),
            (PathBuf::from("/b-rollout-s1-new"), Some(mk("s1", "P1"))),
            (PathBuf::from("/c-rollout-s2"), Some(mk("s2", "P2"))),
        ];
        let base = std::time::SystemTime::UNIX_EPOCH;
        // s1 最新文件是第 2 个（mtime 更大）
        let mtimes = vec![base, base + std::time::Duration::from_secs(999), base];
        let host = vec![AgentProcess {
            pid: 100,
            cpu_usage: 0.0,
            cwd: None,
            form: ProcessForm::App,
        }];
        let out = aggregate_app_sessions(&parsed, &mtimes, &host);
        // 按 sessionId 聚合：s1 + s2 各一张，无重复
        assert_eq!(out.len(), 2);
        assert!(out.iter().any(|s| s.id == "s1"));
        assert!(out.iter().any(|s| s.id == "s2"));
        // 宿主在场时卡归 App 形态、pid 取宿主进程
        assert!(out.iter().all(|s| matches!(s.form, ProcessForm::App)));
        assert!(out.iter().all(|s| s.pid == 100));
    }

    #[test]
    fn aggregate_skips_matched_files_and_requires_host() {
        let mk = |id: &str| Session {
            id: id.into(),
            agent_type: AgentType::Codex,
            project_name: "P".into(),
            project_path: "/tmp/P".into(),
            title: None,
            git_branch: None,
            github_url: None,
            status: SessionStatus::Idle,
            last_message: None,
            last_message_role: None,
            last_activity_at: String::new(),
            pid: 0,
            cpu_usage: 0.0,
            active_subagent_count: 0,
            form: ProcessForm::App,
            jump_supported: true,
            unread: false,
        };
        let parsed = vec![(PathBuf::from("/x"), Some(mk("s1")))];
        let base = std::time::SystemTime::UNIX_EPOCH;
        // 24h 窗口外 → 不出卡
        let old = base + std::time::Duration::from_secs(1);
        let mut now = std::time::SystemTime::now();
        now = now
            .checked_sub(std::time::Duration::from_secs(3600))
            .unwrap_or(now);
        assert!(aggregate_app_sessions(&parsed, &[old], &[]).is_empty());
        // 宿主进程不存在 → 不出卡（活跃卡需宿主存活；持久未读由 DB 合并管线负责）
        assert!(aggregate_app_sessions(&parsed, &[now], &[]).is_empty());
    }
```

（测试文件顶部补 `use std::path::PathBuf;` 与 `use crate::adapter::AgentProcess;`——后者已有。）

- [x] **Step 2: 运行测试确认失败**

Run: `cd src-tauri && cargo test aggregate`
Expected: 编译失败

- [x] **Step 3: 实现聚合并重构 Phase 2**（顺带删除已无消费者的 `unmatched_processes` 收集段）

`codex_parser.rs` 在 `get_codex_sessions` 之前加纯函数：

```rust
/// APP 形态每会话一卡（spec W4 通用规则的 Codex 落地）：
/// 输入已按 mtime 倒序的 (文件, 会话) 与对应 mtime，取未被 CLI 认领的、24h 内有更新
/// 的文件，按 sessionId 聚合（同会话多个 rollout 取最新），宿主 App 进程在场才出卡
pub fn aggregate_app_sessions(
    parsed: &[(PathBuf, Option<Session>)],
    mtimes: &[std::time::SystemTime],
    app_processes: &[AgentProcess],
) -> Vec<Session> {
    use std::collections::HashMap;

    let Some(host) = app_processes.first() else {
        return Vec::new();
    };
    let now = std::time::SystemTime::now();
    let window = std::time::Duration::from_secs(24 * 3600);

    // sessionId → (mtime, session)，保留同会话 mtime 最新者
    let mut by_session: HashMap<String, (std::time::SystemTime, Session)> = HashMap::new();
    for ((file, session_opt), mtime) in parsed.iter().zip(mtimes.iter()) {
        let Some(session) = session_opt else { continue };
        // 该文件已被 CLI 进程认领的判定由调用方通过 parsed 子集传入（见 get_codex_sessions）
        let _ = file;
        let fresh = now.duration_since(*mtime).map(|d| d < window).unwrap_or(false);
        if !fresh {
            continue;
        }
        by_session
            .entry(session.id.clone())
            .and_modify(|e| {
                if *mtime > e.0 {
                    *e = (*mtime, session.clone());
                }
            })
            .or_insert_with(|| (*mtime, session.clone()));
    }

    by_session
        .into_values()
        .map(|(_, mut s)| {
            s.pid = host.pid;
            s.cpu_usage = host.cpu_usage;
            s.form = ProcessForm::App;
            s.jump_supported = jump_supported_for(ProcessForm::App);
            s.github_url = crate::monitor::git::get_github_url(&s.project_path);
            s
        })
        .collect()
}
```

`get_codex_sessions` 的 Phase 2（第 96-116 行整段）替换为：

```rust
    // Phase 2（W4 每会话一卡）：未被 CLI 认领的近期 rollout 按 sessionId 聚合，
    // 每会话一张卡（宿主 App 进程在场才出活跃卡；完成转绿的持久未读由 DB 管线合并）
    let app_processes: Vec<AgentProcess> = processes
        .iter()
        .filter(|p| matches!(p.form, ProcessForm::App))
        .cloned()
        .collect();
    if !app_processes.is_empty() {
        // CLI 认领 = Phase 1 已占用；剩余文件进入聚合
        let mtimes: Vec<std::time::SystemTime> = jsonl_files
            .iter()
            .map(|f| {
                f.metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
            })
            .collect();
        let unclaimed: Vec<(PathBuf, Option<Session>)> = parsed
            .iter()
            .enumerate()
            .filter(|(idx, _)| !matched_file_indices.contains(idx))
            .map(|(_, pair)| pair.clone())
            .collect();
        let unclaimed_mtimes: Vec<std::time::SystemTime> = parsed
            .iter()
            .enumerate()
            .filter(|(idx, _)| !matched_file_indices.contains(idx))
            .filter_map(|(idx, _)| mtimes.get(idx).copied())
            .collect();
        sessions.extend(aggregate_app_sessions(
            &unclaimed,
            &unclaimed_mtimes,
            &app_processes,
        ));
    }
```

（`AgentProcess` 需 `Clone`——第 37-43 行结构体已派生 `Debug, Clone`，无需改。）

- [x] **Step 4: WorkBuddy 心跳消失补偿**（锁内仅做内存快照，心跳文件判定移到锁外；`read_dir` 失败改 `continue` 只跳过该 pid，不中断其余补偿）

`workbuddy_parser.rs` 末尾加：

```rust
/// 转绿竞态补偿（spec W4）：任务完成 → prewarm 回池（心跳删除）可能只隔几秒，
/// 若两轮扫描间完成，「转绿」从未被观测 → 未读漏插。此处对上一轮见过、本轮心跳
/// 消失的 pid 读其 JSONL 终态：完成 → 补插未读；运行中被杀 → 不插
pub fn compensate_vanished_heartbeats() {
    let Some(home) = dirs::home_dir() else { return };
    let now = now_ms();

    let vanished: Vec<(u32, String)> = {
        let last_seen = LAST_SEEN_SESSIONS.lock().unwrap();
        last_seen
            .iter()
            .filter(|(pid, _)| {
                let Some(hb) = std::fs::read_to_string(heartbeat_path(&home, **pid))
                    .ok()
                    .and_then(|s| parse_heartbeat(&s))
                else {
                    return true; // 心跳文件没了 = 回池/退出
                };
                !heartbeat_is_alive(&hb, now)
            })
            .map(|(pid, sid)| (*pid, sid.clone()))
            .collect()
    };

    for (pid, session_id) in vanished {
        LAST_SEEN_SESSIONS.lock().unwrap().remove(&pid);
        // 找该会话的 JSONL：遍历 projects 下所有 <sessionId>.jsonl（会话可能换过项目目录）
        let projects_dir = home.join(".workbuddy").join("projects");
        let Ok(entries) = std::fs::read_dir(&projects_dir) else { return };
        let target = entries
            .filter_map(|e| e.ok())
            .find_map(|dir| {
                let p = dir.path().join(format!("{}.jsonl", session_id));
                p.exists().then_some(p)
            });
        let Some(jsonl) = target else { continue };
        let lines = crate::monitor::jsonl::read_recent_lines(&jsonl, 500);
        if derive_status_from_tail(&lines) != SessionStatus::Idle {
            continue; // 非完成态（运行中被杀等）→ 不补
        }
        // cwd 从首行 user 消息或 DB 反查：直接用 DB 的 cwd 字段
        let cwd = workbuddy_cwd_from_db(&home, &session_id).unwrap_or_default();
        let last_message = lines.iter().rev().find_map(|l| extract_message_text(l));
        crate::database::dao::unread::upsert(&crate::database::dao::unread::UnreadSessionRecord {
            tool_id: "workbuddy".into(),
            session_id: session_id.clone(),
            project_name: if cwd.is_empty() {
                "WorkBuddy".into()
            } else {
                crate::monitor::project::project_name_from_path(&cwd)
            },
            title: title_from_db(&home, &session_id),
            last_message,
            turned_green_at_ms: now as i64,
            expires_at_ms: now as i64 + 24 * 3600 * 1000,
        });
    }
}

fn workbuddy_cwd_from_db(home: &Path, session_id: &str) -> Option<String> {
    use rusqlite::OpenFlags;
    let db = home.join(".workbuddy").join("workbuddy.db");
    let conn = rusqlite::Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    conn.query_row(
        "SELECT cwd FROM sessions WHERE id = ?1 AND deleted_at IS NULL",
        [session_id],
        |row| row.get::<_, String>(0),
    )
    .ok()
}
```

`adapter/mod.rs` 的 `get_all_sessions` Phase 2 循环后（第 198 行 `all_sessions.extend(sessions);` 所在 for 循环之后、`dedup_sessions` 之前）加：

```rust
    // W4：WorkBuddy 心跳消失补偿（转绿未被观测的会话补插未读）
    monitor::workbuddy_parser::compensate_vanished_heartbeats();
```

- [x] **Step 5: 验证**（`cargo test` 138 全过含 2 个新 aggregate 测试、`cargo clippy --all-targets -- -D warnings` 全绿；手动冒烟验证项顺延至 GUI 阶段）

Run: `cd src-tauri && cargo test && cargo clippy`
Expected: PASS（含 aggregate 2 个新测试）

手动：WorkBuddy 跑一个短任务等它完成回池 → 看板出现绿色未读卡且跨 MAM 重启仍在；直接退出 WorkBuddy APP → 未读卡下一轮消失。

- [x] **Step 6: Commit**

```bash
git add src-tauri/src/
git commit -m "feat(sessions): per-session cards for Codex APP and WorkBuddy heartbeat-vanish compensation"
```

### Task 11: 跳转标记已读 + 前端未读卡 UI

**Files:**
- Modify: `src-tauri/src/commands/session.rs:34-75`（成功臂标记已读）
- Modify: `src-tauri/src/commands/settings.rs`（mark_session_read IPC）
- Modify: `src-tauri/src/lib.rs` invoke_handler（注册命令）
- Modify: `src/components/sessions/SessionCard.tsx`（未读徽标 + X）
- Modify: `src/hooks/useNotification.ts`（首见绿未读通知）
- Modify: `src/i18n/locales/zh.json`、`en.json`（sessions.unread 等键）

**Interfaces:**
- Consumes: Task 8 `dao::unread::delete`、Task 9 `Session.unread`
- Produces: IPC `mark_session_read(agent_type: String, session_id: String)`

- [x] **Step 1: 后端标记已读**

`commands/session.rs`：两个平台的成功返回点之前统一插入（Windows `Focused` 臂与非 Windows 的两个成功 return 前各一次；抽小函数避免重复）：

```rust
/// 跳转成功 → 标记该会话已读（仅删除对应未读行；同工具其他未读卡保留，spec W4）
fn mark_read_on_jump(session_id: &Option<String>, agent_type: &Option<String>) {
    if let (Some(sid), Some(agent)) = (session_id, agent_type) {
        crate::database::dao::unread::delete(&agent.to_lowercase(), sid);
    }
}
```

调用点：Windows `Ok(FocusOutcome::Focused)` 臂、`reactivate_tool_app` 兜底成功处、非 Windows `focus_terminal_for_pid` 成功处与 `activate_agent_app` Some 处，均在 `return Ok(json!({"type":"focused"}))` 前调用 `mark_read_on_jump(&session_id, &agent_type);`。

`commands/settings.rs` 加：

```rust
#[tauri::command]
pub fn mark_session_read(agent_type: String, session_id: String) {
    crate::database::dao::unread::delete(&agent_type.to_lowercase(), &session_id);
}
```

`lib.rs` invoke_handler 列表 `commands::settings::list_sub_agents,` 后加 `commands::settings::mark_session_read,`。

- [x] **Step 2: SessionCard 未读徽标与关闭按钮**

`src/components/sessions/SessionCard.tsx`：import `X`（lucide-react）与 `invoke`；`handleClick` 前加关闭函数：

```tsx
  const handleCloseUnread = async (e: React.MouseEvent) => {
    e.stopPropagation(); // 不触发卡片跳转
    try {
      await invoke("mark_session_read", {
        agentType: session.agentType,
        sessionId: session.id,
      });
    } catch (err) {
      console.error("mark_session_read failed:", err);
    }
  };
```

卡片标题区域（项目名旁）在 `session.unread` 时渲染：

```tsx
  {session.unread && (
    <span className="inline-flex items-center gap-1">
      <span className="h-1.5 w-1.5 rounded-full bg-emerald-400" aria-label={t("sessions.unread")} />
      <button
        onClick={handleCloseUnread}
        className="rounded p-0.5 text-muted-foreground hover:bg-muted hover:text-foreground"
        title={t("sessions.markRead")}
        aria-label={t("sessions.markRead")}
      >
        <X className="h-3 w-3" />
      </button>
    </span>
  )}
```

（插入位置：以文件内项目名渲染 JSX 为锚点，`grep -n "projectName" SessionCard.tsx` 定位。）

- [x] **Step 3: 首见绿未读触发通知**

`useNotification.ts` 的会话循环中，"首次加载不通知"分支（第 117-124 行）改为：

```ts
        // 首次加载不通知——除非是「未读绿卡」：补偿/重启场景下它从未被观测过转绿，
        // 需补一次完成通知（spec W4；5 秒同色去重防双弹）
        if (!prev) {
          prevStatuses.current.set(session.id, {
            status: session.status,
            color: statusToColor(session.status),
            at: Date.now(),
          });
          if (session.unread && statusToColor(session.status) === "green") {
            const notified = lastNotified.current.get(session.id);
            if (
              !notified ||
              notified.color !== "green" ||
              Date.now() - notified.at >= 5000
            ) {
              // 走下方统一通知流：复制最小通知体
              notifyCompletion(session);
            }
          }
          continue;
        }
```

同时把原循环体中"通知"段（第 142-207 行，从开关刷新到浮窗发送）抽为 `const notifyCompletion = async (session: Session) => { ... }`（逻辑原样搬移，供两处调用；原路径调用改为 `await notifyCompletion(session)`）。`Session` 类型从 `@/types/session` 导入。

- [x] **Step 4: i18n 键**

`zh.json` 的 `sessions` 段加：

```json
    "unread": "未读",
    "markRead": "标记已读并关闭"
```

`en.json` 对应加：

```json
    "unread": "Unread",
    "markRead": "Mark read and dismiss"
```

- [ ] **Step 5: 验证**

Run: `pnpm check`；`cd src-tauri && cargo test`
手动：① WorkBuddy 完成任务 → 绿卡带未读点；② 点卡跳转 WorkBuddy 前台 → 卡消失（同工具另一未读卡保留）；③ 点 X → 卡消失；④ 重启 MAM → 未读卡仍在；⑤ 退出 WorkBuddy → 卡消失。

- [x] **Step 6: Commit**

```bash
git add src/ src-tauri/src/ src/i18n/
git commit -m "feat(sessions): unread badge, dismiss action and jump-marks-read wiring"
```

---

## Phase W5：工具勾选管理（commit 5）

### Task 12: agent_tools DAO + 工具设置 IPC + 还原/重建服务

**Files:**
- Modify: `src-tauri/src/database/dao/agent_tool.rs`
- Modify: `src-tauri/src/database/mod.rs`（导出）
- Create: `src-tauri/src/services/tool_settings.rs`
- Modify: `src-tauri/src/services/mod.rs`（声明）
- Modify: `src-tauri/src/commands/settings.rs` + `src-tauri/src/lib.rs`
- Test: `dao/agent_tool.rs` 内联（内存库）

**Interfaces:**
- Consumes: `linker::{remove_link, copy_dir_recursive, ensure_repo_dir}`、`services::skill::{enable_skill_for_tool}`、`services::mcp::{write_mcp, remove_mcp}`、`services::toggle_plugin`、`dao::extension::{list_all_assignments, list_extensions}`
- Produces:
  - `dao::agent_tool::{ensure_tool_rows(), get_tool_enabled(tool_id) -> bool, enabled_tool_ids() -> Vec<String>, set_tool_enabled(tool_id, bool)}`
  - `services::tool_settings::{get_tool_settings() -> Vec<ToolSetting>, apply_tool_changes(Vec<ToolSettingChange>) -> ApplyResult}`（含 `managed` 标志与还原/重建）
  - IPC：`get_tool_settings` / `update_tool_settings`

- [x] **Step 1: DAO 失败测试**

`dao/agent_tool.rs` 测试模块加：

```rust
    #[test]
    fn enabled_tool_ids_seeds_and_filters() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        ensure_tool_rows_conn(&conn);
        // 默认全部启用（老用户升级零感知）
        assert_eq!(enabled_tool_ids_conn(&conn).len(), crate::adapter::TOOL_IDS.len());
        set_tool_enabled_conn(&conn, "workbuddy", false);
        assert!(!get_tool_enabled_conn(&conn, "workbuddy"));
        let ids = enabled_tool_ids_conn(&conn);
        assert!(!ids.contains(&"workbuddy".to_string()));
        assert!(ids.contains(&"claude".to_string()));
    }
```

- [x] **Step 2: 运行测试确认失败**

Run: `cd src-tauri && cargo test enabled_tool_ids`
Expected: 编译失败

- [x] **Step 3: 实现 DAO**

`dao/agent_tool.rs` 加（连接参数风格同 unread，便于内存库测试；`adapter_by_id` 提供 name/base_dir）：

```rust
/// 种子行：全部注册工具 enabled=1（INSERT OR IGNORE 幂等，应用启动时调用）
pub fn ensure_tool_rows_conn(conn: &rusqlite::Connection) {
    for id in crate::adapter::TOOL_IDS {
        if let Some(adapter) = crate::adapter::adapter_by_id(id) {
            let _ = conn.execute(
                "INSERT OR IGNORE INTO agent_tools (id, name, process_name, base_dir, hook_supported, mcp_format, enabled)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1)",
                rusqlite::params![
                    id,
                    adapter.name(),
                    adapter.process_names()[0],
                    adapter.base_dir().to_string_lossy(),
                    adapter.hook_supported() as i64,
                    format!("{:?}", adapter.mcp_format()).to_lowercase(),
                ],
            );
        }
    }
}

pub fn get_tool_enabled_conn(conn: &rusqlite::Connection, tool_id: &str) -> bool {
    conn.query_row(
        "SELECT enabled FROM agent_tools WHERE id = ?1",
        [tool_id],
        |row| row.get::<_, i64>(0),
    )
    .map(|v| v != 0)
    .unwrap_or(true) // 行缺失视为启用（防御）
}

pub fn enabled_tool_ids_conn(conn: &rusqlite::Connection) -> Vec<String> {
    let Ok(mut stmt) =
        conn.prepare("SELECT id FROM agent_tools WHERE enabled = 1 ORDER BY rowid")
    else {
        return Vec::new();
    };
    stmt.query_map([], |row| row.get::<_, String>(0))
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
}

pub fn set_tool_enabled_conn(conn: &rusqlite::Connection, tool_id: &str, enabled: bool) {
    // 行不存在时先种子（防御旧库）
    ensure_tool_rows_conn(conn);
    let _ = conn.execute(
        "UPDATE agent_tools SET enabled = ?2 WHERE id = ?1",
        rusqlite::params![tool_id, enabled as i64],
    );
}

// 全局连接包装
pub fn ensure_tool_rows() {
    let conn = crate::database::connection::DB.lock().unwrap();
    ensure_tool_rows_conn(&conn);
}
pub fn get_tool_enabled(tool_id: &str) -> bool {
    let conn = crate::database::connection::DB.lock().unwrap();
    get_tool_enabled_conn(&conn, tool_id)
}
pub fn enabled_tool_ids() -> Vec<String> {
    let conn = crate::database::connection::DB.lock().unwrap();
    enabled_tool_ids_conn(&conn)
}
pub fn set_tool_enabled(tool_id: &str, enabled: bool) {
    let conn = crate::database::connection::DB.lock().unwrap();
    set_tool_enabled_conn(&conn, tool_id, enabled);
}
```

`database/mod.rs` 导出追加；并在 `database::init()`（`migration::migrate` 之后）加 `dao::agent_tool::ensure_tool_rows();`。

- [x] **Step 4: 测试通过**

Run: `cd src-tauri && cargo test enabled_tool_ids`
Expected: PASS

- [x] **Step 5: 还原/重建服务与 IPC**

创建 `services/tool_settings.rs`：

```rust
// 工具勾选管理（spec W5）：查询（含 managed 标志）与保存时清理/重建
// 取消勾选 = skill/文件型插件链接还原为真实文件 + MAM 管理的 MCP 条目移除；
// SSOT 与 DB 分配关系保留；重新勾选按原分配重建

use crate::database::dao::{agent_tool, extension};

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolSetting {
    pub tool_id: String,
    pub name: String,
    pub enabled: bool,
    pub installed: bool,
    pub managed: bool,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolSettingChange {
    pub tool_id: String,
    pub enabled: bool,
}

#[derive(Debug, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyResult {
    pub restored: Vec<String>,   // 已还原的 skill/插件名
    pub restored_mcps: Vec<String>,
    pub rebuild_failed: Vec<String>,
}

pub fn get_tool_settings() -> Vec<ToolSetting> {
    agent_tool::ensure_tool_rows();
    crate::adapter::TOOL_IDS
        .iter()
        .filter_map(|id| {
            let adapter = crate::adapter::adapter_by_id(id)?;
            Some(ToolSetting {
                tool_id: id.to_string(),
                name: adapter.name().to_string(),
                enabled: agent_tool::get_tool_enabled(id),
                installed: adapter.base_dir().exists(),
                managed: tool_has_managed_content(id),
            })
        })
        .collect()
}

/// managed = 该工具存在启用的分配（skill/文件型插件链接或 MCP 条目）
fn tool_has_managed_content(tool_id: &str) -> bool {
    extension::list_all_assignments()
        .iter()
        .any(|a| a.agent_tool_id == tool_id && a.enabled)
}

pub fn apply_tool_changes(changes: Vec<ToolSettingChange>) -> ApplyResult {
    let mut result = ApplyResult::default();
    for c in &changes {
        let was = agent_tool::get_tool_enabled(&c.tool_id);
        if was == c.enabled {
            continue;
        }
        agent_tool::set_tool_enabled(&c.tool_id, c.enabled);
        if !c.enabled {
            disable_tool_cleanup(&c.tool_id, &mut result);
        } else {
            rebuild_tool_links(&c.tool_id, &mut result);
        }
    }
    result
}

/// 取消勾选：链接还原 + MCP 条目移除 + 未读卡清除（spec W5 清理语义）
fn disable_tool_cleanup(tool_id: &str, result: &mut ApplyResult) {
    let home = dirs::home_dir().unwrap_or_default();
    let assignments: Vec<_> = extension::list_all_assignments()
        .into_iter()
        .filter(|a| a.agent_tool_id == tool_id && a.enabled)
        .collect();
    let extensions = extension::list_extensions();

    for a in &assignments {
        let Some(ext) = extensions.iter().find(|e| e.id == a.extension_id) else {
            continue;
        };
        match ext.kind.as_str() {
            "skill" => {
                if let Some(dir) = crate::adapter::skill_dir_for_tool(tool_id, &home) {
                    let target = dir.join(&ext.name);
                    let ssot = crate::linker::ensure_repo_dir().join("skills").join(&ext.name);
                    if ssot.exists()
                        && crate::linker::check_link_health(&target)
                            != crate::linker::LinkHealth::NotLink
                    {
                        // 仅还原「MAM 建的链接」；原生目录不动
                        if crate::linker::remove_link(&target).is_ok() {
                            if crate::linker::copy_dir_recursive(&ssot, &target).is_ok() {
                                result.restored.push(ext.name.clone());
                            }
                        }
                    }
                }
            }
            "mcp" => {
                if crate::services::mcp::remove_mcp(tool_id, &ext.name).is_ok() {
                    result.restored_mcps.push(ext.name.clone());
                }
            }
            "plugin" => {
                if let Some(adapter) = crate::adapter::adapter_by_id(tool_id) {
                    if let Some(dir) = adapter.plugin_dirs().first() {
                        let target = dir.join(&ext.name);
                        let ssot =
                            crate::linker::ensure_repo_dir().join("plugins").join(&ext.name);
                        if ssot.exists()
                            && crate::linker::check_link_health(&target)
                                != crate::linker::LinkHealth::NotLink
                            && crate::linker::remove_link(&target).is_ok()
                            && crate::linker::copy_dir_recursive(&ssot, &target).is_ok()
                        {
                            result.restored.push(ext.name.clone());
                        }
                    }
                }
            }
            _ => {}
        }
    }
    // 未读卡一并清除（取消勾选立即彻底隐藏）
    crate::database::dao::unread::clear_tool(tool_id);
}

/// 重新勾选：按原分配重建（幂等；失败项记录不影响整体）
fn rebuild_tool_links(tool_id: &str, result: &mut ApplyResult) {
    let assignments: Vec<_> = extension::list_all_assignments()
        .into_iter()
        .filter(|a| a.agent_tool_id == tool_id && a.enabled)
        .collect();
    let extensions = extension::list_extensions();
    for a in &assignments {
        let Some(ext) = extensions.iter().find(|e| e.id == a.extension_id) else {
            continue;
        };
        let ok = match ext.kind.as_str() {
            "skill" => crate::services::skill::enable_skill_for_tool(&ext.name, tool_id).is_ok(),
            "plugin" => crate::services::toggle_plugin(&ext.name, tool_id, true).is_ok(),
            "mcp" => {
                // SSOT MCP 配置：<repo>/mcp/<name>.json（与 list_ssot_resources 的读取约定一致）
                let path = crate::linker::ensure_repo_dir()
                    .join("mcp")
                    .join(format!("{}.json", ext.name));
                match std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|s| serde_json::from_str(&s).ok())
                {
                    Some(config) => {
                        crate::services::mcp::write_mcp(tool_id, &ext.name, &config).is_ok()
                    }
                    None => false,
                }
            }
            _ => true,
        };
        if !ok {
            result.rebuild_failed.push(ext.name.clone());
        }
    }
}
```

注意：`LinkHealth`/`list_extensions` 返回项字段名以实际代码为准——实现时读 `linker/mod.rs:26-38`（`LinkHealth` 枚举变体）与 `dao/extension.rs`（`ExtensionRecord` 字段、`list_all_assignments` 返回类型）对齐；MCP SSOT 存储路径若与 `commands/resource.rs` 的实际读取不同（grep `mam/mcp` 或 `join("mcp")` 确认），以实际为准并同步修改此处。

`services/mod.rs` 加 `pub mod tool_settings;`。

`commands/settings.rs` 加：

```rust
#[tauri::command]
pub fn get_tool_settings() -> Vec<crate::services::tool_settings::ToolSetting> {
    crate::services::tool_settings::get_tool_settings()
}

#[tauri::command]
pub fn update_tool_settings(
    changes: Vec<crate::services::tool_settings::ToolSettingChange>,
) -> crate::services::tool_settings::ApplyResult {
    crate::services::tool_settings::apply_tool_changes(changes)
}
```

`lib.rs` invoke_handler 注册两命令。

- [x] **Step 6: 验证**

Run: `cd src-tauri && cargo test && cargo clippy`
Expected: PASS

- [x] **Step 7: Commit**

```bash
git add src-tauri/src/
git commit -m "feat(settings): tool enable/disable with SSOT restore, MCP removal and rebuild"
```

### Task 13: 启用过滤生效（扫描/资源/命令守卫）

**Files:**
- Modify: `src-tauri/src/adapter/mod.rs:127-132`（all_adapters 过滤）与 `:154-156`
- Modify: `src-tauri/src/commands/resource.rs:380-465`（list_ssot_resources 的 enabledTools）
- Modify: `src-tauri/src/commands/skill.rs`、`commands/mcp.rs`、`commands/plugin.rs`（toggle 守卫）

**Interfaces:**
- Consumes: Task 12 `dao::agent_tool::enabled_tool_ids`
- Produces: `adapter::enabled_adapters() -> Vec<Box<dyn AgentAdapter>>`；`services::tool_settings::ensure_tool_enabled(tool_id) -> Result<(), String>`

- [x] **Step 1: 会话扫描过滤**（实际产出 `adapter::enabled_adapters()`，`get_all_sessions` 改用之；`all_adapters()` 保持原样供管理入口使用）

`adapter/mod.rs`：`get_all_sessions` 第 155 行 `let adapters: Vec<Box<dyn AgentAdapter>> = all_adapters();` 改为：

```rust
    // W5：未勾选工具不参与会话扫描（看板卡/通知随之静默）
    let adapters: Vec<Box<dyn AgentAdapter>> = TOOL_IDS
        .iter()
        .filter(|id| crate::database::dao::agent_tool::get_tool_enabled(id))
        .filter_map(|id| adapter_by_id(id))
        .collect();
```

- [x] **Step 2: 资源分布过滤 + 命令守卫**（守卫点按真实代码定位：`commands/resource.rs` 的 `enable_skill_for_tool_cmd`/`disable_skill_for_tool`、`commands/mcp.rs` 的 `toggle_mcp_for_tool`、`commands/plugin.rs` 的 `toggle_plugin_for_tool`、`commands/skill.rs` 的 `assign_skill_to_subagent`）

`commands/resource.rs` 的 `list_ssot_resources`：以 `grep -n "enabledTools\|enabled_tools" src-tauri/src/commands/resource.rs` 定位组装点，将工具集合与 `dao::agent_tool::enabled_tool_ids()` 求交集（未勾选工具的列不返回；分配数据本身保留在 DB）。

`services/tool_settings.rs` 加守卫：

```rust
/// toggle 类命令守卫：未勾选工具的资源管理操作直接拒绝
pub fn ensure_tool_enabled(tool_id: &str) -> Result<(), String> {
    if agent_tool::get_tool_enabled(tool_id) {
        Ok(())
    } else {
        Err(format!("工具 {} 未启用，请先在设置-工具管理中开启", tool_id))
    }
}
```

在 `commands/skill.rs`（`enable_skill_for_tool_cmd`）、`commands/mcp.rs`（`toggle_mcp_for_tool`）、`commands/plugin.rs`（`toggle_plugin_for_tool`）的入口各加 `crate::services::tool_settings::ensure_tool_enabled(&tool_id)?;`（以各命令实际参数名为准）。

- [x] **Step 3: 验证**（`cargo test` 142 通过 + `cargo clippy --all-targets -- -D warnings` 全绿；手动 GUI 验证项顺延至 GUI 阶段）

Run: `cd src-tauri && cargo test && cargo clippy`
手动：设置里取消勾选 OpenCode 保存（先启用状态）→ 看板无 OpenCode 卡、资源分布无 OpenCode 列、对 OpenCode 的 toggle 命令报错；skill 目录被还原为真实文件（`ls -la ~/.config/opencode/skills` 验证非 `->` 链接）。

- [x] **Step 4: Commit**（ab96d2d）

```bash
git add src-tauri/src/
git commit -m "feat(settings): enforce tool enablement in scan, resources and toggle commands"
```

### Task 14: 设置页「工具管理」UI（开关 + 保存确认 + 离开拦截）

**Files:**
- Modify: `src/pages/settings.tsx`（新分区 + 交互）
- Modify: `src/i18n/locales/zh.json`、`en.json`（settings.tools 键）

**Interfaces:**
- Consumes: IPC `get_tool_settings` / `update_tool_settings`（Task 12）
- Produces: 设置分区 `"tools"`（行式开关列表 + 保存按钮 + 确认 Dialog + 未保存离开拦截）

- [x] **Step 1: i18n 键**

`zh.json` 的 `settings` 段加：

```json
    "tools": {
      "title": "工具管理",
      "hint": "取消勾选的工具将在会话监控、通知与资源管理中隐藏；被 MAM 管理的文件会还原为真实文件",
      "installed": "已安装",
      "notInstalled": "未检测到",
      "save": "保存设置",
      "discard": "放弃更改",
      "keepEditing": "继续编辑",
      "unsavedTitle": "有未保存的更改",
      "unsavedDesc": "工具开关有变更尚未保存，离开将丢失这些更改。",
      "confirmTitle": "确认应用更改",
      "confirmDesc": "即将对以下工具应用变更：",
      "enableItem": "新纳入监控（影响较小）",
      "restoreItem": "还原/回溯：skill/插件链接还原为真实文件、MCP 条目从工具配置移除",
      "confirm": "确认应用",
      "cancel": "取消",
      "applied": "已应用"
    },
```

`en.json` 对应翻译（键结构完全一致）：

```json
    "tools": {
      "title": "Tool Management",
      "hint": "Unchecked tools are hidden from session monitoring, notifications and resource management; MAM-managed files are restored to real files",
      "installed": "Installed",
      "notInstalled": "Not detected",
      "save": "Save Settings",
      "discard": "Discard Changes",
      "keepEditing": "Keep Editing",
      "unsavedTitle": "Unsaved Changes",
      "unsavedDesc": "Tool toggles have unsaved changes. Leaving now will discard them.",
      "confirmTitle": "Apply Changes",
      "confirmDesc": "The following tool changes will be applied:",
      "enableItem": "Start monitoring (minor impact)",
      "restoreItem": "Restore/rollback: skill/plugin links restored to real files, MCP entries removed from tool config",
      "confirm": "Apply",
      "cancel": "Cancel",
      "applied": "Applied"
    },
```

- [x] **Step 2: 分区与状态**

`settings.tsx`：`SettingSection` 联合类型加 `"tools"`；`SECTIONS` 数组（第 146-161 行区域）加：

```tsx
      {
        id: "tools" as SettingSection,
        label: t("settings.tools.title"),
      },
```

组件状态（与既有 useState 区并列）：

```tsx
  type ToolRow = {
    toolId: string;
    name: string;
    enabled: boolean;
    installed: boolean;
    managed: boolean;
  };
  const [toolRows, setToolRows] = useState<ToolRow[]>([]);
  const [toolDirty, setToolDirty] = useState(false);
  const [confirmOpen, setConfirmOpen] = useState(false);
  const [leaveGuard, setLeaveGuard] = useState<null | (() => void)>(null);

  const loadToolSettings = useCallback(async () => {
    try {
      const rows = await invoke<
        { toolId: string; name: string; enabled: boolean; installed: boolean; managed: boolean }[]
      >("get_tool_settings");
      setToolRows(rows);
      setToolDirty(false);
    } catch (e) {
      console.error("get_tool_settings failed:", e);
    }
  }, []);

  useEffect(() => {
    if (activeSection === "tools") void loadToolSettings();
  }, [activeSection, loadToolSettings]);
```

- [x] **Step 3: 分区渲染（行式开关）**

在既有分区渲染 switch/条件中加 `"tools"` 分支：

```tsx
  const toggleTool = (toolId: string, next: boolean) => {
    setToolRows((rows) =>
      rows.map((r) => (r.toolId === toolId ? { ...r, enabled: next } : r))
    );
    setToolDirty(true);
  };

  const changedRows = toolRows.filter(
    (r) =>
      r.enabled !==
      (savedToolEnabledRef.current[r.toolId] ?? r.enabled)
  );
```

（`savedToolEnabledRef`：`useRef<Record<string, boolean>>({})`，`loadToolSettings` 成功后写入快照。）

渲染 JSX：

```tsx
<div className="space-y-3">
  <p className="text-xs text-muted-foreground">{t("settings.tools.hint")}</p>
  <div className="rounded border divide-y">
    {toolRows.map((r) => (
      <div key={r.toolId} className="flex items-center justify-between px-3 py-2">
        <div className="flex items-center gap-2">
          <span className="text-sm font-medium">{r.name}</span>
          <span className={`text-[10px] rounded px-1.5 py-0.5 ${r.installed ? "bg-emerald-500/10 text-emerald-500" : "bg-muted text-muted-foreground"}`}>
            {r.installed ? t("settings.tools.installed") : t("settings.tools.notInstalled")}
          </span>
        </div>
        <Switch checked={r.enabled} onCheckedChange={(v) => toggleTool(r.toolId, v)} />
      </div>
    ))}
  </div>
  {toolDirty && (
    <Button onClick={() => setConfirmOpen(true)}>{t("settings.tools.save")}</Button>
  )}
</div>
```

- [x] **Step 4: 保存确认弹窗**

用既有 `Dialog` 组件（`@/components/ui/dialog`）实现 `confirmOpen`：内容列出 `changedRows`，每行按变更方向标注——开启 → `t("settings.tools.enableItem")`；关闭且 `managed` → `t("settings.tools.restoreItem")`；关闭且非 managed → 仅"停止监控"。确认执行：

```tsx
  const applyChanges = async () => {
    try {
      const result = await invoke<{ restored: string[]; restoredMcps: string[]; rebuildFailed: string[] }>(
        "update_tool_settings",
        {
          changes: changedRows.map((r) => ({ toolId: r.toolId, enabled: r.enabled })),
        }
      );
      toast.success(t("settings.tools.applied"));
      if (result.rebuildFailed.length) {
        toast.warning(`rebuild failed: ${result.rebuildFailed.join(", ")}`);
      }
      setConfirmOpen(false);
      await loadToolSettings();
      // 触发会话与资源数据刷新（react-query 失效）
      await invalidateMamQueries();
    } catch (e) {
      toast.error(String(e));
    }
  };
```

（`invalidateMamQueries`：`queryClient.invalidateQueries()` 全量失效——import 自既有 query client 工具，以 `src/lib/query/` 现有导出为准，无则直接 `useQueryClient().invalidateQueries()`。）

- [x] **Step 5: 未保存离开拦截**

分区切换处（`setActiveSection` 的调用点）包一层守卫函数：

```tsx
  const switchSection = (next: SettingSection) => {
    if (next === activeSection) return;
    if (activeSection === "tools" && toolDirty) {
      setLeaveGuard(() => () => {
        setToolDirty(false);
        setActiveSection(next);
      });
      return;
    }
    setActiveSection(next);
  };
```

`leaveGuard` 渲染 Dialog 三按钮：确认保存（关闭 leaveGuard → 打开 confirmOpen，应用成功后再执行缓存的跳转）/ `t("settings.tools.discard")`（丢弃：`setToolDirty(false)` + 执行跳转）/ `t("settings.tools.keepEditing")`（关闭弹窗留在本页）。另加 `window.addEventListener("beforeunload")`（toolDirty 时 `e.preventDefault()`）覆盖关窗口场景。页面内所有调用 `setActiveSection` 的 UI 一律改走 `switchSection`。

- [x] **Step 6: 验证**

Run: `pnpm check`
手动：① 开关切换 → 不立即生效（看板不变），出现保存按钮；② 保存 → 确认弹窗列出变更与还原提示 → 确认后看板/资源分布立即变化；③ 改开关后切分区 → 三选弹窗；④ 取消保存 → 变更丢弃。

- [x] **Step 7: Commit**

```bash
git add src/pages/settings.tsx src/i18n/
git commit -m "feat(settings): tool management section with batch save, confirm dialog and unsaved-leave guard"
```

### Task 15: 前端工具列表改后端下发 + 收尾

**Files:**
- Modify: `src-tauri/src/commands/settings.rs` + `src-tauri/src/lib.rs`（list_enabled_tools IPC）
- Create: `src/lib/query/queries/tools.ts`
- Modify: `src/components/resources/ResourceByKindView.tsx:30-36`、`ResourceByToolView.tsx:11-17`、`src/pages/settings.tsx` 声音区硬编码

**Interfaces:**
- Produces: IPC `list_enabled_tools() -> Vec<{id: String, label: String}>`；FE `useEnabledToolsQuery`

- [x] **Step 1: IPC**

`commands/settings.rs` 加：

```rust
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnabledTool {
    pub id: String,
    pub label: String,
}

/// 前端工具列的唯一下发源（W5：勾选状态驱动，替代三处硬编码 TOOLS）
#[tauri::command]
pub fn list_enabled_tools() -> Vec<EnabledTool> {
    crate::adapter::TOOL_IDS
        .iter()
        .filter(|id| crate::database::dao::agent_tool::get_tool_enabled(id))
        .filter_map(|id| {
            crate::adapter::adapter_by_id(id).map(|a| EnabledTool {
                id: id.to_string(),
                label: a.name().to_string(),
            })
        })
        .collect()
}
```

`lib.rs` 注册。注意 adapter.name() 返回 "Codex CLI"/"WorkBuddy" 等——资源视图标签以 name 直出（与 byTool 现状标签风格一致，byKind 视图原用短标签 "Claude"/"Codex"，统一为 name 可接受，视觉差异已含在本任务验收）。

- [x] **Step 2: FE query 与三处替换**

创建 `src/lib/query/queries/tools.ts`（对照 `queries/sessions.ts` 的既有写法）：

```ts
import { useQuery } from "@tanstack/react-query";
import { invoke } from "@tauri-apps/api/core";

export interface EnabledTool {
  id: string;
  label: string;
}

export function useEnabledToolsQuery() {
  return useQuery({
    queryKey: ["enabled-tools"],
    queryFn: () => invoke<EnabledTool[]>("list_enabled_tools"),
  });
}
```

`ResourceByKindView.tsx` / `ResourceByToolView.tsx`：删除本地 `TOOLS` 常量，`const { data: tools = [] } = useEnabledToolsQuery();`，原 `TOOLS.map(...)` 改 `tools.map(...)`；byTool 的挂载加载 effect 同步改为 `tools.forEach(...)` 并把 `tools` 加入依赖。`settings.tsx` 声音区硬编码列表同法替换。加载中 `tools` 为空数组 → 列区短暂空白可接受（query 秒回）。

- [x] **Step 3: 验证与收尾**（`pnpm check` + `cargo test` + `cargo clippy --all-targets -- -D warnings` 全绿；手动全链路验证项顺延至 GUI 阶段）

Run: `pnpm check`；`cd src-tauri && cargo test && cargo clippy`
手动全链路（spec 第 8 节清单）：取消勾选一个工具 → 资源分布列消失、看板无其卡片；重新勾选 → 列恢复、分配重建。

- [x] **Step 4: Commit**（fdc5d2b）

```bash
git add src/ src-tauri/src/
git commit -m "feat(settings): backend-driven enabled tool list for resource views and settings"
```

---

## 计划自审记录（Self-Review）

1. **Spec 覆盖**：W1（Task 1）、W2 深度链接第一顺位+保底+双平台 pid 兜底（Task 2-4）、W3 适配器+过滤+资源接入+防御（Task 5-7）、W4 未读池/竞态补偿/宿主清理/每会话一卡/已读信号/通知面（Task 8-11）、W5 默认全启用/全量还原+SSOT 保留/彻底隐藏/批量保存+确认+离开拦截/后端下发（Task 12-15）。spec 第 7 节跨平台：各 cfg 分支均要求编译通过；第 9 节风险应对已内嵌（防御解析、路由探测退化、还原缺项报告 rebuild_failed）。
2. **占位符扫描**：Task 4 Step 2 的路由表为「以实测值替换」的条件执行（探测失败则整步跳过，非占位）；Task 3 Step 5 与 Task 12 Step 5 标注了三处「以实际代码字段名为准」的对照点（win32 AllWindows 字段、LinkHealth 变体、MCP SSOT 路径），均附带了明确的对照文件与行号——执行者按锚点对齐而非自行发明。
3. **类型一致性**：`unread: bool` 全库构造点统一由 Task 9 的 cargo check 清单补齐；`UnreadSessionRecord` 字段在 Task 8 定义、Task 9/10 消费一致；`session_url/open_url`（Task 3 定义、Task 4 填充）、`enabled_tool_ids/get_tool_enabled`（Task 12 定义、Task 13/15 消费）签名一致。
