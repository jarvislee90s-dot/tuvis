# 跳转 v2 + 通知浮窗 + i18n 全量 Implementation Plan（精简版）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 实施两份已批准 spec：`specs/008-jump-v2-and-notification-window/spec.md` 与 `specs/009-i18n-full-coverage/spec.md`。

**本轮约定（用户明确要求）**：小步快做，**测试最少化、人工验证为主**——除既有测试的必要适配和 i18n 门禁脚本外，不新增自动化测试；每个任务交付后即 commit。

**架构：** 跳转侧重写 `win32.rs` 为"近祖优先 + shell 黑名单 + 消歧降级"，hook 注入 session marker 到终端标题；通知侧新增独立无边框置顶小窗（不夺焦点，点击联动跳转）；i18n 侧审计 + 批量接入 + 键对齐门禁脚本。

---

## 背景与两条跳转路径（执行者必读）

上一版跳转的缺陷（实机取证）：祖先链 PID 集合不分远近 + Z 序任取 → Windows Terminal **单进程 7 窗口**随机命中；explorer.exe（拥有 2 个文件资源管理器窗口）混在 VS Code/ChatGPT 的祖先链里抢命中。本轮重写为**按祖先距离从近到远**定位。

### CLI 类与 App 类的跳转路径（明确区分，代码统一实现、语义不同）

**CLI 类**（claude / codex CLI / opencode / openclaw 的终端会话）：

```
卡片点击 → agent PID
  → 父链从近到远扫描（跳过 shell 黑名单）
  → 第一个"拥有可见顶层窗口"的祖先 = 终端宿主
     典型链: claude.exe → pwsh.exe → OpenConsole.exe(无窗口) → WindowsTerminal.exe(多窗口!)
  → 宿主只有一个窗口 → 直接聚焦
  → 宿主有多个窗口（WT 单进程多窗口）→ 消歧：
       ① 标题含 "MAM:<sessionId 前 8 位>"（hook 注入的 marker）→ 精确锁定
       ② 标题打分: 含项目名 +2、含工具名 +1，最高分唯一 → 锁定
       ③ 仍歧义 → 返回候选列表，前端弹窗口选择器（用户点选，永远正确）
```

**App 类**（ChatGPT 内嵌 Codex）：

```
卡片点击 → codex.exe PID
  → 父链从近到远扫描（跳过 shell 黑名单）
  → 第一个"拥有可见顶层窗口"的祖先 = ChatGPT.exe 主进程
     （实测 ChatGPT.exe 恰有 1 个可见窗口，标题 "ChatGPT"）
  → 唯一窗口 → 直接聚焦（无需消歧，无 marker 无标题打分）
```

**共同聚焦动作**（所有路径共用）：最小化则 `SW_RESTORE` → `SetForegroundWindow` → 失败则 `SwitchToThisWindow` → 再失败则"最小化再恢复"抖动后重试 `SetForegroundWindow` → 全失败报错。

### 本轮范围裁剪（相对 spec 008，经用户确认"从简"）

- **UIA 内容匹配（spec 第 3b 层）本轮不做**：hook marker 覆盖 claude/codex 重复会话；opencode 重复会话由窗口选择器兜底（永远正确）。降级链为 1→2→3a→4，完整性不受破坏
- `AttachThreadInput` 前台强抢不做，用"最小化/恢复抖动"替代（更简单可靠）
- 系统通知的 AUMID 修复不做（spec 已界定）

**环境**：Windows（Git Bash），cargo 在 `src-tauri/` 下执行；TLS 报错时后台跑 `python C:\Users\bunny\AppData\Local\Temp\dsh-cargo-http-mirror.py 8765`。**macOS 零回归**是硬约束。

---

### Task 1: `win32.rs` v2 重构 + `focus_session` 扩参 + `focus_hwnd` 命令

**Files:**
- Modify: `src-tauri/src/window/win32.rs`（重写核心）
- Modify: `src-tauri/src/commands/session.rs`（focus_session 签名）
- Modify: `src-tauri/src/lib.rs`（注册 focus_hwnd）

- [ ] **Step 1: 祖先链返回值 HashSet → 有序 Vec（近→远）**

`collect_ancestor_pids_with` 与 `collect_ancestor_pids` 返回值改为 `Vec<u32>`（保持插入顺序即近→远；环检测改用 `contains`）：

```rust
fn collect_ancestor_pids_with(
    pid: u32,
    mut parent_of: impl FnMut(u32) -> Option<u32>,
) -> Vec<u32> {
    let mut chain = Vec::new();
    let mut current = pid;
    for _ in 0..64 {
        if chain.contains(&current) {
            break; // 环
        }
        chain.push(current);
        match parent_of(current) {
            Some(p) => current = p,
            None => break,
        }
    }
    chain
}

fn collect_ancestor_pids(system: &sysinfo::System, pid: u32) -> Vec<u32> {
    collect_ancestor_pids_with(pid, |p| {
        system
            .process(sysinfo::Pid::from_u32(p))
            .and_then(|proc| proc.parent())
            .map(|pp| pp.as_u32())
    })
}
```

同步修改既有 3 个 `win32` 测试的断言（`HashSet::from([...])` → 有序 Vec 比较，环测试断言前两个元素为 `[7, 8]` 即可）。跑 `cargo test --lib win32` 确认通过。

- [ ] **Step 2: 重写枚举与消歧核心**

删除旧的 `EnumContext` / `enum_windows_proc` / `focus_window`，替换为：

```rust
use windows::Win32::Foundation::{HWND, LPARAM, BOOL};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowLongW, GetWindowTextW, GetWindowThreadProcessId, IsIconic,
    SetForegroundWindow, ShowWindow, SwitchToThisWindow, GWL_EXSTYLE, SW_MINIMIZE, SW_RESTORE,
    WS_EX_TOOLWINDOW,
};

/// 不可作为跳转宿主的系统 shell / 服务进程（其窗口与目标会话无关）
const SHELL_BLACKLIST: &[&str] = &[
    "explorer.exe",
    "sihost.exe",
    "svchost.exe",
    "ctfmon.exe",
    "runtimebroker.exe",
    "applicationframehost.exe",
    "searchhost.exe",
    "shellexperiencehost.exe",
    "startmenuexperiencehost.exe",
    "taskhostw.exe",
    "dwm.exe",
];

/// 候选窗口（歧义时返回给前端选择器）
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowCandidate {
    pub hwnd: isize,
    pub title: String,
    pub process: String,
}

struct AllWindows {
    by_pid: std::collections::HashMap<u32, Vec<(isize, String)>>,
}

unsafe extern "system" fn enum_all_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let ctx = &mut *(lparam.0 as *mut AllWindows);
    if !IsWindowVisible(hwnd).as_bool() {
        return BOOL(1);
    }
    if (GetWindowLongW(hwnd, GWL_EXSTYLE) & WS_EX_TOOLWINDOW.0 as i32) != 0 {
        return BOOL(1);
    }
    let mut pid: u32 = 0;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    let mut buf = [0u16; 256];
    let len = GetWindowTextW(hwnd, &mut buf);
    let title = String::from_utf16_lossy(&buf[..len as usize]);
    ctx.by_pid.entry(pid).or_default().push((hwnd.0, title));
    BOOL(1)
}

/// 一次性枚举全部可见顶层窗口，按 PID 分组
fn all_windows() -> AllWindows {
    let mut ctx = AllWindows {
        by_pid: std::collections::HashMap::new(),
    };
    let lparam = LPARAM(&mut ctx as *mut AllWindows as isize);
    unsafe {
        let _ = EnumWindows(Some(enum_all_proc), lparam);
    }
    ctx
}

/// 聚焦单个窗口：恢复最小化 → 置前，多级降级
fn force_foreground(hwnd_val: isize) -> bool {
    let hwnd = HWND(hwnd_val);
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        if SetForegroundWindow(hwnd).as_bool() {
            return true;
        }
        #[allow(deprecated)]
        SwitchToThisWindow(hwnd, true);
        if SetForegroundWindow(hwnd).as_bool() {
            return true;
        }
        // 最后手段：最小化再恢复，强制窗口进入前台
        let _ = ShowWindow(hwnd, SW_MINIMIZE);
        let _ = ShowWindow(hwnd, SW_RESTORE);
        SetForegroundWindow(hwnd).as_bool()
    }
}

/// 跳转解析结果
pub enum FocusOutcome {
    Focused,
    Ambiguous(Vec<WindowCandidate>),
}

/// 解析并聚焦（CLI 与 App 统一入口，路径差异见模块头注释）
/// session_marker: 如 "MAM:1ba8e2f7"（hook 注入的标题标记，精确匹配用）
pub fn resolve_and_focus(
    system: &sysinfo::System,
    pid: u32,
    session_marker: Option<&str>,
    agent_keyword: Option<&str>,
    project_name: Option<&str>,
) -> Result<FocusOutcome, String> {
    let windows = all_windows();

    for ancestor in collect_ancestor_pids(system, pid) {
        // 黑名单进程的窗口与目标会话无关（explorer 的文件管理器窗口等）
        let proc_name = system
            .process(sysinfo::Pid::from_u32(ancestor))
            .map(|p| p.name().to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if SHELL_BLACKLIST.contains(&proc_name.as_str()) {
            continue;
        }
        let Some(cands) = windows.by_pid.get(&ancestor) else { continue };
        if cands.is_empty() {
            continue;
        }
        if cands.len() == 1 {
            let (hwnd, _) = cands[0];
            force_foreground(hwnd);
            return Ok(FocusOutcome::Focused);
        }
        // 多窗口消歧（Windows Terminal 单进程多窗口场景）
        // ① marker 精确匹配
        if let Some(marker) = session_marker {
            let hits: Vec<_> = cands
                .iter()
                .filter(|(_, t)| t.to_lowercase().contains(&marker.to_lowercase()))
                .collect();
            if hits.len() == 1 {
                force_foreground(hits[0].0);
                return Ok(FocusOutcome::Focused);
            }
        }
        // ② 标题打分：项目名 +2、工具名 +1
        let score = |title: &str| -> i32 {
            let t = title.to_lowercase();
            let mut s = 0;
            if let Some(p) = project_name {
                if !p.is_empty() && t.contains(&p.to_lowercase()) {
                    s += 2;
                }
            }
            if let Some(a) = agent_keyword {
                if !a.is_empty() && t.contains(&a.to_lowercase()) {
                    s += 1;
                }
            }
            s
        };
        let mut scored: Vec<_> = cands.iter().map(|c| (score(&c.1), c)).collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0));
        if scored.len() >= 2 && scored[0].0 > scored[1].0 && scored[0].0 > 0 {
            force_foreground(scored[0].1 .0);
            return Ok(FocusOutcome::Focused);
        }
        // ③ 歧义 → 交给前端选择器
        let candidates = cands
            .iter()
            .map(|(hwnd, title)| WindowCandidate {
                hwnd: *hwnd,
                title: title.clone(),
                process: proc_name.clone(),
            })
            .collect();
        return Ok(FocusOutcome::Ambiguous(candidates));
    }
    Err("未找到可聚焦的窗口（终端可能已关闭）".to_string())
}

/// 前端选择器点选后按句柄聚焦
pub fn focus_hwnd(hwnd_val: isize) -> Result<(), String> {
    if force_foreground(hwnd_val) {
        Ok(())
    } else {
        Err("窗口聚焦被系统拒绝（窗口可能已关闭）".to_string())
    }
}

/// 兼容旧入口（focus_session 内部使用）
pub fn focus_window_for_pid(pid: u32) -> Result<(), String> {
    let system = sysinfo::System::new_all();
    match resolve_and_focus(&system, pid, None, None, None) {
        Ok(FocusOutcome::Focused) => Ok(()),
        Ok(FocusOutcome::Ambiguous(_)) => Err("存在多个候选窗口，请重试以打开选择器".to_string()),
        Err(e) => Err(e),
    }
}
```

（`window/mod.rs` 中 `#[cfg(windows)] pub mod win32;` 与 `focus_terminal_for_pid` 的 windows 分支调用 `win32::focus_window_for_pid` 保持不变。）

- [ ] **Step 3: `focus_session` 命令扩参与新命令**

`src-tauri/src/commands/session.rs` 中 `focus_session` 替换为：

```rust
#[tauri::command]
pub fn focus_session(
    pid: u32,
    session_id: Option<String>,
    agent_type: Option<String>,
    project_name: Option<String>,
) -> Result<serde_json::Value, String> {
    #[cfg(windows)]
    {
        let system = sysinfo::System::new_all();
        let marker = session_id
            .as_deref()
            .map(|id| format!("MAM:{}", &id.chars().take(8).collect::<String>()));
        return match crate::window::win32::resolve_and_focus(
            &system,
            pid,
            marker.as_deref(),
            agent_type.as_deref(),
            project_name.as_deref(),
        ) {
            Ok(crate::window::win32::FocusOutcome::Focused) => {
                Ok(serde_json::json!({ "type": "focused" }))
            }
            Ok(crate::window::win32::FocusOutcome::Ambiguous(windows)) => {
                Ok(serde_json::json!({ "type": "ambiguous", "windows": windows }))
            }
            Err(e) => Err(e),
        };
    }
    #[cfg(not(windows))]
    {
        let _ = (session_id, agent_type, project_name);
        crate::window::focus_terminal_for_pid(pid).map(|_| serde_json::json!({ "type": "focused" }))
    }
}

#[tauri::command]
pub fn focus_hwnd(hwnd: isize) -> Result<(), String> {
    #[cfg(windows)]
    {
        crate::window::win32::focus_hwnd(hwnd)
    }
    #[cfg(not(windows))]
    {
        let _ = hwnd;
        Err("当前平台不支持".to_string())
    }
}
```

在 `src-tauri/src/lib.rs` 的 `generate_handler!` 列表中追加 `commands::session::focus_hwnd`（跟随现有 focus_session 的注册路径写法）。

- [ ] **Step 4: 编译与回归**

```bash
cd src-tauri && cargo test && cargo clippy --all-targets -- -D warnings
```

预期：全部通过（既有测试仅 win32 祖先链 3 个断言适配）。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/window/win32.rs src-tauri/src/commands/session.rs src-tauri/src/lib.rs
git commit -m "feat(window): nearest-ancestor window resolution with marker and title disambiguation"
```

---

### Task 2: 前端接线 — SessionCard 传参与窗口选择器

**Files:**
- Modify: `src/components/sessions/SessionCard.tsx`（handleClick 与选择器）

- [ ] **Step 1: 修改 `handleClick`（当前 44-56 行附近）**

```tsx
  const handleClick = async () => {
    if (!session.jumpSupported) {
      toast.info(t("sessions.jumpUnsupported"));
      return;
    }
    try {
      const result = await invoke<{
        type: string;
        windows?: { hwnd: number; title: string; process: string }[];
      }>("focus_session", {
        pid: session.pid,
        sessionId: session.id,
        agentType: session.agentType,
        projectName: session.projectName,
      });
      if (result.type === "ambiguous" && result.windows && result.windows.length > 0) {
        setPendingWindows(result.windows);
      }
    } catch (e) {
      toast.error(t("sessions.jumpFailed", { error: e }));
    }
  };
```

组件内加状态 `const [pendingWindows, setPendingWindows] = useState<{ hwnd: number; title: string; process: string }[] | null>(null);`（`useState` 按现有 import 方式补充）。

- [ ] **Step 2: 窗口选择器弹层（组件 JSX 末尾追加）**

```tsx
      {pendingWindows && (
        <div
          className="fixed inset-0 z-50 flex items-center justify-center bg-black/50"
          onClick={() => setPendingWindows(null)}
        >
          <div
            className="w-96 rounded-lg border bg-card p-4 shadow-xl"
            onClick={(e) => e.stopPropagation()}
          >
            <p className="mb-3 text-sm font-medium">{t("sessions.pickWindow")}</p>
            <div className="flex flex-col gap-2">
              {pendingWindows.map((w) => (
                <button
                  key={w.hwnd}
                  className="truncate rounded border px-3 py-2 text-left text-xs hover:bg-accent"
                  onClick={async () => {
                    setPendingWindows(null);
                    try {
                      await invoke("focus_hwnd", { hwnd: w.hwnd });
                    } catch (e) {
                      toast.error(t("sessions.jumpFailed", { error: e }));
                    }
                  }}
                  title={w.title}
                >
                  {w.title || "(无标题)"} — {w.process}
                </button>
              ))}
            </div>
          </div>
        </div>
      )}
```

- [ ] **Step 3: i18n 键**

`zh.json` 的 `sessions` 命名空间加 `"pickWindow": "多个窗口匹配，请选择目标窗口"`；`en.json` 对应加 `"pickWindow": "Multiple windows matched — pick the target"`（键集保持对齐，99→100）。

- [ ] **Step 4: 验证与 Commit**

```bash
cd "E:\LLMproject\Github\MultiAgents-Manager" && pnpm lint && pnpm build
git add src/components/sessions/SessionCard.tsx src/i18n/locales/zh.json src/i18n/locales/en.json
git commit -m "feat(ui): window picker for ambiguous jump targets"
```

---

### Task 3: hook 标题注入（spike 先行）+ 卡片前缀统一 8 位

**Files:**
- Modify: `src-tauri/src/monitor/hooks.rs`（HOOK_SCRIPT 常量）
- Modify: `src-tauri/src/monitor/parser.rs`（两处 `min(12)` → `min(8)`）

- [x] **Step 1: Spike — 验证两条前提（人工，任一不成立则本任务跳过、仅做 Step 4）**

  > **spike 结论（2026-08-24 补录）**：前提 A 不成立（实跑 claude 会话成功、生成了 session_id，但 `~/.mam/events` 无新事件文件——根因是 claude `settings.json` 无 hooks 段、hook 脚本未被执行，DB 中 hooks_registered=true 却未实际注册到 claude）/ 前提 B 成立（子进程向控制台写 `ESC]0;MAM:test1234 BEL` 序列后，窗口标题实测变为 `MAM:test1234`）。判定 no-go，marker 层保持未启用，窗口歧义由标题打分 + 选择器兜底。
  > **2026-08-25 更新**：前提 A 根因（注册假阳性）已修复——dev 启动即注册 hooks 段（命令形态 `bash "...status-hook.sh"`），实跑 claude 会话后 `~/.mam/events/` 正常产出事件文件；marker 已随脚本启用，hook 内 `/dev/tty` 写入在本轮自动化（无头管道）环境报 "No such device or address"、交互终端下的标题效果待用户验证，窗口歧义仍由候选打分过滤兜底。
  > **2026-08-25 二次更新（CONOUT$ spike，spec 013）**：marker 注入改走 CONOUT$（`powershell [IO.File]::WriteAllText('CONOUT$', ESC + "]0;MAM:<8位>" + BEL)`）。实测：脚本刷新机制正常（dev 启动即幂等重写）；hook 事件产出不受影响（`~/.mam/events/` 出现 spike 会话事件文件 session_id=spike7a3b…）；但**标题是否出现 MAM: 前缀无法在本自动化环境确证**（无交互终端可观测；wt→bash 引号链路的模拟靶窗口不可靠）。另修正计划笔误：bash 双引号内 `] 前的反斜杠会被保留致 OSC 序列退化为 ST+字面文本，已去除。判定 **no-go（未确证）**：marker 代码保留不回滚，精确匹配主力切换为 UIA 正文匹配、认领池过滤兜底；交互终端实测留待用户执行。
  > **2026-08-25 终审（用户交互终端复测）**：hook 事件正常触发（含 Stop 事件）但终端标题始终无 `MAM:` 前缀——CONOUT$ 通道在 hook 上下文同样不可达。**最终判决 no-go，marker 默认禁用**（`MAM_MARKER:-0`，代码保留待轻量 helper 等新通道）；精确匹配主力 = UIA 正文匹配（已并行化，实测单窗 127-400ms）+ 项目名精确层（codex 标题=项目目录名形态）。

前提 A（hook 在本机实际生效）：跑一个 claude 会话发条消息，检查 `~/.mam/events/` 是否出现新事件文件——没有则 hook 通道未工作，marker 无从注入，no-go。
前提 B（子进程写标题可达）：在 Git Bash 终端执行：

```bash
printf '\033]0;MAM:test1234\007' > /dev/tty
```

终端标题栏出现 `MAM:test1234` 即成立。

两项都成立 → 继续 Step 2；否则记录结论、跳到 Step 4。

- [ ] **Step 2: HOOK_SCRIPT 注入 marker**

`hooks.rs` 的 `HOOK_SCRIPT` 常量中，在写事件文件的那行 `echo ... > "$EVENTS_DIR/$PPID.json"` 之后追加一行：

```bash
# 注入窗口标题 marker（MAM:<session_id 前 8 位>），供 MultiAgents Manager 跳转精确定位
printf '\\033]0;MAM:%s\\007' "$(printf '%s' "$SESSION_ID" | cut -c1-8)" > /dev/tty 2>/dev/null || true
```

（注意：HOOK_SCRIPT 是 Rust 原始字符串常量，`\\033` 写法以实际常量定界符为准——保持与现有 `\\{` 转义风格一致。）

- [ ] **Step 3: 强制刷新已存在的脚本**

`ensure_hook_script` 当前只在文件不存在时写入。将 `if !script_path.exists()` 分支改为"总是重写"（脚本由应用托管、无用户自定义价值，幂等）：

```rust
    let script_path = hooks_dir.join("status-hook.sh");
    let _ = fs::write(&script_path, HOOK_SCRIPT);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = fs::metadata(&script_path) {
            let mut perms = metadata.permissions();
            perms.set_mode(0o755);
            let _ = fs::set_permissions(&script_path, perms);
        }
    }
    script_path
```

（删除原 `info!("Hook 脚本已创建", ...)` 与外层 if。）

- [ ] **Step 4: 卡片前缀 12 → 8 位**

`parser.rs` 中两处（Claude 的 `session_title`、Codex 的 `codex_title`）：

```rust
// 前
let session_title = session_id[..session_id.len().min(12)].to_string();
// 后
let session_title = session_id[..session_id.len().min(8)].to_string();
```

（Codex 处变量名 `codex_title`，同样 `min(12)` → `min(8)`。）

- [ ] **Step 5: 编译回归与 Commit**

```bash
cd src-tauri && cargo test && cargo clippy --all-targets -- -D warnings
git add src-tauri/src/monitor/hooks.rs src-tauri/src/monitor/parser.rs
git commit -m "feat(hooks): inject MAM session marker into terminal title and shorten card prefix"
```

---

### Task 4: 自定义通知浮窗

**Files:**
- Create: `src/pages/notification.tsx`
- Modify: `src/main.tsx`（hash 路由分流）
- Create: `src-tauri/src/commands/notification.rs`
- Modify: `src-tauri/src/lib.rs`（注册命令）、`src-tauri/src/commands/mod.rs`（挂载模块）
- Modify: `src/hooks/useNotification.ts`、`src/pages/settings.tsx`（系统通知开关）

- [ ] **Step 1: 通知窗口页面 `src/pages/notification.tsx`**

```tsx
import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";

interface NotificationPayload {
  agentType: string;
  projectName: string;
  statusColor: "red" | "yellow" | "green";
  statusLabel: string;
  lastMessage: string;
  pid: number;
  sessionId: string;
}

export default function NotificationPage() {
  const [payload, setPayload] = useState<NotificationPayload | null>(null);
  const timerRef = useRef<number | null>(null);

  useEffect(() => {
    const win = getCurrentWindow();
    let unlisten: (() => void) | null = null;
    const armTimer = () => {
      if (timerRef.current) window.clearTimeout(timerRef.current);
      timerRef.current = window.setTimeout(() => win.hide(), 6000);
    };
    listen<NotificationPayload>("notification:new", (e) => {
      setPayload(e.payload);
      win.show();
      armTimer();
    }).then((fn) => (unlisten = fn));
    return () => unlisten?.();
  }, []);

  if (!payload) return <div className="h-full w-full" />;

  const jump = async () => {
    getCurrentWindow().hide();
    try {
      await invoke("focus_session", {
        pid: payload.pid,
        sessionId: payload.sessionId,
        agentType: payload.agentType,
        projectName: payload.projectName,
      });
    } catch {
      // 跳转失败不弹新提示（通知窗口环境无 toast 容器）
    }
  };

  return (
    <div
      className="flex h-screen w-screen cursor-pointer items-center gap-3 rounded-lg border bg-card p-3 shadow-2xl"
      onMouseEnter={() => timerRef.current && window.clearTimeout(timerRef.current)}
      onMouseLeave={() => {
        if (timerRef.current) window.clearTimeout(timerRef.current);
        timerRef.current = window.setTimeout(() => getCurrentWindow().hide(), 3000);
      }}
      onClick={jump}
    >
      <span
        className="h-3 w-3 shrink-0 rounded-full"
        style={{ background: payload.statusColor }}
      />
      <div className="min-w-0 flex-1">
        <p className="truncate text-xs font-semibold">
          {payload.agentType} · {payload.projectName} · {payload.statusLabel}
        </p>
        <p className="mt-1 line-clamp-2 text-[11px] opacity-70">{payload.lastMessage}</p>
      </div>
    </div>
  );
}
```

- [ ] **Step 2: `src/main.tsx` 分流通知窗口**

```tsx
const NotificationPage = lazy(() => import("./pages/notification"));
// pageMap 定义保持不变，在其下方替换 PageComponent 计算：
const isNotificationWindow = window.location.hash === "#/notification";
const PageComponent = isNotificationWindow
  ? NotificationPage
  : pageMap[pathname as keyof typeof pageMap] ?? HomePage;
```

（通知窗口无需 QueryClientProvider 的数据——AppWrapper 结构不动，NotificationPage 走同一 Suspense。）

- [ ] **Step 3: Rust 侧窗口管理 `src-tauri/src/commands/notification.rs`**

```rust
// 自定义通知浮窗 — 独立无边框置顶小窗，轮转 3 个槽位堆叠

use serde::Deserialize;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotificationPayload {
    pub agent_type: String,
    pub project_name: String,
    pub status_color: String,
    pub status_label: String,
    pub last_message: String,
    pub pid: u32,
    pub session_id: String,
}

const SLOTS: usize = 3;
const W: f64 = 360.0;
const H: f64 = 110.0;
const MARGIN: f64 = 16.0;

#[tauri::command]
pub fn show_notification_window(app: AppHandle, payload: NotificationPayload) -> Result<(), String> {
    // 找一个隐藏的槽位；全忙则复用第 0 个（顶替最旧，简化策略）
    let mut slot = 0;
    for i in 0..SLOTS {
        if let Some(w) = app.get_webview_window(&format!("notification-{i}")) {
            if !w.is_visible().unwrap_or(false) {
                slot = i;
                break;
            }
        } else {
            slot = i;
            break;
        }
    }
    // 右下角定位 + 槽位纵向堆叠
    let (mx, my) = (1920.0, 1080.0);
    if let Ok(m) = app.primary_monitor() {
        mx = m.size().width as f64 / m.scale_factor();
        my = m.size().height as f64 / m.scale_factor();
    }
    let x = mx - W - MARGIN;
    let y = my - H - MARGIN - 48.0 - (slot as f64) * (H + 8.0);

    let label = format!("notification-{slot}");
    match app.get_webview_window(&label) {
        Some(w) => {
            let _ = w.set_position(tauri::Position::Logical(tauri::LogicalPosition::new(x, y)));
        }
        None => {
            let _ = WebviewWindowBuilder::new(
                &app,
                &label,
                WebviewUrl::App("index.html#/notification".into()),
            )
            .title("mam-notification")
            .decorations(false)
            .always_on_top(true)
            .skip_taskbar(true)
            .resizable(false)
            .transparent(true)
            .visible(false) // 页面收到事件后才 show，避免白屏
            .inner_size(W, H)
            .position(x, y)
            .build()
            .map_err(|e| format!("创建通知窗口失败: {}", e))?;
        }
    }
    // 定向发送到该槽位窗口（emit 全局广播会让所有槽位同时弹出）
    // 延迟发送规避"页面 JS 尚未注册 listener"的创建竞态；偶发丢失则该条不显示，下一条正常
    let app2 = app.clone();
    let payload2 = payload.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        let _ = app2.emit_to(&label, "notification:new", &payload2);
    });
    Ok(())
}
```

（`focusable(false)` 若当前 Tauri 版本的 builder 支持（`WebviewWindowBuilder::focusable`）则加上——不夺键盘焦点的首选；不支持则依赖 `always_on_top + 不调用 set_focus`，并在验收中确认打字不被打断，必要时改用 `.focused(false)`。）

`src-tauri/src/commands/mod.rs` 挂载 `pub mod notification;`；`lib.rs` 的 `generate_handler!` 注册 `commands::notification::show_notification_window`。

- [ ] **Step 4: `useNotification.ts` 改造**

原 `sendNotification(...)` 调用处（保留权限/去重/声音逻辑不动）替换为分支：

```ts
const useSystemToast = localStorage.getItem("mam.useSystemNotification") === "1";
if (useSystemToast) {
  await sendNotification({ title, body }); // 原有调用原样保留
} else {
  await invoke("show_notification_window", {
    payload: {
      agentType,
      projectName,
      statusColor,
      statusLabel,
      lastMessage,
      pid,
      sessionId,
    },
  });
}
```

（变量从现有构造 title/body 的代码处取值；`agentType/projectName/pid/sessionId/statusColor/lastMessage` 在现有循环里均可从 session 对象获得；`statusLabel` 复用文件顶部已有的 `STATUS_LABELS` 表。）

- [ ] **Step 5: 设置页"使用系统通知"开关**

在 `src/pages/settings.tsx` 的通知设置区（提示音开关所在处），照抄同文件现有开关的组件与存储模式新增一项：

- 标签："使用系统通知（默认关闭，使用应用内浮窗）"
- 存储：`localStorage` 键 `mam.useSystemNotification`（`"1"` 开 / 其他关）
- i18n：settings 命名空间新增键（zh/en 同步，如 `settings.useSystemNotification` / 英文对应）

- [ ] **Step 6: 验证与 Commit**

```bash
cd src-tauri && cargo clippy --all-targets -- -D warnings
cd "E:\LLMproject\Github\MultiAgents-Manager" && pnpm lint && pnpm build
git add src/pages/notification.tsx src/main.tsx src-tauri/src/commands/notification.rs src-tauri/src/commands/mod.rs src-tauri/src/lib.rs src/hooks/useNotification.ts src/pages/settings.tsx src/i18n/locales/zh.json src/i18n/locales/en.json
git commit -m "feat(notification): in-app floating notification window with click-to-jump"
```

---

### Task 5: i18n 键对齐门禁脚本

**Files:**
- Create: `scripts/check-i18n.mjs`
- Modify: `package.json`（scripts）

- [ ] **Step 1: 编写脚本**

```js
#!/usr/bin/env node
// 校验 zh/en 语言文件键集一致；不一致时列出差异并以非零码退出
import { readFileSync } from "node:fs";

const flat = (obj, prefix = "") =>
  Object.entries(obj).flatMap(([k, v]) =>
    typeof v === "object" && v !== null ? flat(v, `${prefix}${k}.`) : [`${prefix}${k}`]
  );

const zh = JSON.parse(readFileSync("src/i18n/locales/zh.json", "utf8"));
const en = JSON.parse(readFileSync("src/i18n/locales/en.json", "utf8"));
const zk = new Set(flat(zh));
const ek = new Set(flat(en));
const missEn = [...zk].filter((k) => !ek.has(k));
const missZh = [...ek].filter((k) => !zk.has(k));

if (missEn.length || missZh.length) {
  console.error(`i18n 键不一致: en 缺 ${missEn.length} 个:`, missEn);
  console.error(`i18n 键不一致: zh 缺 ${missZh.length} 个:`, missZh);
  process.exit(1);
}
console.log(`i18n 键对齐通过（${zk.size} 键）`);
```

- [ ] **Step 2: 注册并验证**

`package.json` 的 `scripts` 加 `"check:i18n": "node scripts/check-i18n.mjs"`；将 `check` 命令追加该子命令（保持现有组合方式）。验证：

```bash
cd "E:\LLMproject\Github\MultiAgents-Manager" && pnpm check:i18n
```

预期：输出键数并退出 0（构造临时缺键样例验证非零退出后还原）。

- [ ] **Step 3: Commit**

```bash
git add scripts/check-i18n.mjs package.json
git commit -m "chore(i18n): add key parity check gate"
```

---

### Task 6: i18n 硬编码批量接入

**Files:**
- Modify: 审计清单所列组件（约 16 个，见 Step 1）
- Modify: `src/i18n/locales/zh.json`、`en.json`

- [ ] **Step 1: 审计产出权威清单**

```bash
cd "E:\LLMproject\Github\MultiAgents-Manager" && grep -rlP "[\x{4e00}-\x{9fff}]" src/components src/pages --include="*.tsx" | grep -v "language-toggle"
```

（排除已完成的 SessionCard 与有意保留的 language-toggle；其余全部在本任务处理。）

- [ ] **Step 2: 逐文件接入（统一模式）**

每个文件按同一模式执行：

1. `import { useTranslation } from "react-i18next";` + 组件内 `const { t } = useTranslation();`
2. 中文串替换为 `t("namespace.key")`；含拼接的（如 `` `共 ${n} 项` ``）改插值键 `t("x.total", { n })` + JSON 里 `"total": "共 {{n}} 项"`
3. zh/en 同步加键（沿用各文件主题就近的现有命名空间；无对应的建新子命名空间，camelCase）
4. 键集对齐：每完成 3 个文件跑一次 `pnpm check:i18n`

**专有名词保留原样不翻译**：Claude/Codex/OpenCode/OpenClaw、`AGENT_BADGE` 的 label、技术名词（JSON/TOML 等）。

- [ ] **Step 3: 验证与 Commit**

```bash
cd "E:\LLMproject\Github\MultiAgents-Manager" && pnpm check:i18n && pnpm lint && pnpm build
git add -A src/
git commit -m "fix(i18n): migrate all hardcoded UI strings to i18n"
```

---

### Task 7: 全量门禁与人工验证清单

- [ ] **Step 1: 自动门禁**

```bash
cd src-tauri && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check
cd "E:\LLMproject\Github\MultiAgents-Manager" && pnpm check
```

fmt 失败则 `cargo fmt` 修正并入当前提交，不开独立 fmt 大提交。

- [ ] **Step 2: 人工验证清单（Windows 实机，`pnpm tauri:dev`）**

跳转（对应 spec 008 故事 1）：

1. 三个 WT 窗口分别跑 claude/codex/opencode（不同项目）→ 逐一点击卡片，各归各窗
2. 同项目双 claude 会话 → 点击其一聚焦正确窗口；marker 生效时直接命中，未生效弹选择器、点选后聚焦
3. 多个文件资源管理器窗口置前 → 点击 VS Code / ChatGPT / 终端卡片，绝不跳到资源管理器
4. ChatGPT 最小化 → 点击 App 卡片 → 恢复置前
5. VS Code 集成终端跑 claude → 点击 → 聚焦该 VS Code 窗口
6. 终端关闭后点击卡片 → toast 提示"未找到可聚焦的窗口"
7. 跳转期间在别的窗口打字 → 聚焦动作本身不吞键盘输入（除目标窗口获得焦点外无异常）

marker 与卡片（spec 008 故事 2）：

8. hook 生效的 claude 会话 → 终端标题含 `MAM:<8 位>` 且与卡片前缀一致（spike no-go 则此项标注跳过）

通知浮窗（spec 008 故事 3）：

9. 应用最小化/托盘状态下任务完成 → 右下角浮出自定义通知卡（非系统 toast）
10. 通知显示期间在其他窗口打字不被打断（焦点不夺取）
11. 6 秒自动消失；悬停保留、移开 3 秒后消失
12. 连续 3 条通知纵向堆叠；点击通知卡跳到对应终端
13. 打开"使用系统通知"开关 → 走系统 toast；关闭 → 回到浮窗

i18n（spec 009 故事 1）：切换 English 与中文各巡检一遍——首页、资源三视图、MCP、设置全部子页（含新开关）、错误边界，无中文残留（专有名词除外）。

- [ ] **Step 3: 汇报**

每个 Task 状态（含 Task 3 spike 的 go/no-go 结论）、门禁结果、人工清单逐项结果（无法验证标注"待用户验证"）、`git log --oneline e1ed533..HEAD`。

---

## 范围外（本轮明确不做）

- UIA 窗口内容匹配（spec 008 第 3b 层）——marker + 选择器已覆盖本轮场景，UIA 留待下轮按需
- AttachThreadInput 前台强抢
- Windows Terminal 标签页级定位
- 系统通知 AUMID 修复、多显示器通知分发、勿扰时段
- OpenCode/OpenClaw 的 hook 机制补齐
- SessionCard AGENT_BADGE 缺 openclaw 条目（存量，另立）
