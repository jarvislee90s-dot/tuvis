# 精准窗口匹配 + 通知浮窗体验 Implementation Plan（精简版）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 实施两份已批准 spec：`specs/013-precise-window-matching`（窗口池认领 + UIA 正文匹配 + marker 换 CONOUT$）、`specs/014-notification-ux`（候选弹窗动态高度 + 渠道统一 + 通知历史）。

**本轮约定**：精简执行、每任务一 commit、每任务带验收条件；除纯函数测试外不新增自动化测试。UIA 的 windows crate API 签名以 0.57 生成绑定为准，编译器报错时按提示微调（结构性代码不变）。

**环境**：Windows（Git Bash），cargo 在 `src-tauri/` 下；TLS 报错时后台跑 `python C:\Users\bunny\AppData\Local\Temp\dsh-cargo-http-mirror.py 8765`。macOS 零回归硬约束。

---

## Part A — 精准窗口匹配（spec 013）

### Task 1: 窗口池认领（他工具窗口无条件排除）

**Files:**
- Modify: `src-tauri/src/window/win32.rs`（认领函数 + Ambiguous 候选生成重构）

- [ ] **Step 1: 写失败测试**

`win32.rs` 测试模块追加：

```rust
    #[test]
    fn claim_owner_matches_keywords() {
        assert_eq!(claim_owner("✳ Claude Code"), Some("claude"));
        assert_eq!(claim_owner("OC | 问候与开场"), Some("opencode"));
        assert_eq!(claim_owner("codex: working"), Some("codex"));
        // 中立：无命中
        assert_eq!(claim_owner("Windows PowerShell"), None);
        assert_eq!(claim_owner("MultiAgents-Manager"), None);
        // 多工具命中 → 中立
        assert_eq!(claim_owner("claude and codex"), None);
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd src-tauri && cargo test --lib claim_owner
```

预期：编译失败 `cannot find function claim_owner`。

- [ ] **Step 3: 实现认领与候选重构**

在 `SHELL_BLACKLIST` 常量后追加：

```rust
/// 工具认领关键词：窗口标题（不区分大小写）命中某工具任一关键词 → 该窗口被视为该工具的。
/// opencode 的终端标题是缩写 "OC | <会话标题>"，故含别名。
const TOOL_CLAIM_KEYWORDS: &[(&str, &[&str])] = &[
    ("claude", &["claude"]),
    ("codex", &["codex"]),
    ("opencode", &["opencode", "oc |"]),
    ("openclaw", &["openclaw"]),
];

/// 判定窗口标题被哪个工具认领；命中多个工具（罕见）视为中立返回 None
fn claim_owner(title: &str) -> Option<&'static str> {
    let t = title.to_lowercase();
    let owners: Vec<&str> = TOOL_CLAIM_KEYWORDS
        .iter()
        .filter(|(_, kws)| kws.iter().any(|k| t.contains(k)))
        .map(|(tool, _)| *tool)
        .collect();
    if owners.len() == 1 {
        Some(owners[0])
    } else {
        None
    }
}
```

将 Ambiguous 候选生成段（现"候选过滤：优先仅返回打分 > 0 …全零时回退全量"的两段 map + if is_empty，**整体**）替换为认领池模型：

```rust
        // ③ 候选池认领过滤：本工具认领的窗口 + 中立窗口；其他工具认领的窗口无条件排除。
        // 排序：先本工具认领、后中立，组内按现有打分降序。
        let agent = agent_keyword.unwrap_or_default().to_lowercase();
        let mut mine: Vec<&(i32, &(isize, String))> = Vec::new();
        let mut neutral: Vec<&(i32, &(isize, String))> = Vec::new();
        for item in scored.iter() {
            match claim_owner(&(item.1).1) {
                Some(owner) if owner != agent.as_str() => continue, // 他工具认领 → 排除
                Some(_) => mine.push(item),
                None => neutral.push(item),
            }
        }
        let candidates: Vec<WindowCandidate> = mine
            .into_iter()
            .chain(neutral)
            .map(|(s, (hwnd, title))| WindowCandidate {
                hwnd: *hwnd,
                title: title.clone(),
                process: proc_name.clone(),
                score: *s,
            })
            .collect();
        return Ok(FocusOutcome::Ambiguous(candidates));
```

（注：`agent_keyword` 传入的就是工具 id 如 "claude"，与认领表键一致；`scored` 已按分降序，两组内保持该序。）

- [ ] **Step 4: 测试通过 + 回归 + Commit**

```bash
cd src-tauri && cargo test && cargo clippy --all-targets -- -D warnings
git add src-tauri/src/window/win32.rs
git commit -m "feat(window): claim-pool candidate filtering across tools"
```

**验收条件**：新测试过；既有测试无回归。

### Task 2: UIA 正文匹配层（focus_session 增加 lastMessage）

**Files:**
- Modify: `src-tauri/Cargo.toml`（windows features）
- Modify: `src-tauri/src/window/win32.rs`（read_window_text + 归一化匹配 + 插层 + 签名）
- Modify: `src-tauri/src/commands/session.rs`（focus_session 加参）
- Modify: `src/hooks/useSessionJump.ts`（JumpTarget 加 lastMessage）
- Modify: `src/components/sessions/SessionCard.tsx`、`src/pages/notification.tsx`（传 lastMessage）

- [ ] **Step 1: 加 feature**

```toml
windows = { version = "0.57", features = ["Win32_Foundation", "Win32_UI_WindowsAndMessaging", "Win32_UI_Accessibility", "Win32_System_Com"] }
```

- [ ] **Step 2: 归一化匹配测试（先写）**

win32.rs 测试模块追加：

```rust
    #[test]
    fn normalized_tail_collapses_whitespace() {
        use super::normalized_tail;
        assert_eq!(normalized_tail("你好  世界\n\n下一行", 3), "你好 世界 下");
        assert_eq!(normalized_tail("short", 40), "short");
        // 长文本取尾部 n 个字符
        let long = "a ".repeat(60);
        assert_eq!(normalized_tail(&long, 10).chars().count(), 10);
    }
```

- [ ] **Step 3: 实现 UIA 读取与匹配**

在 win32.rs 适当位置（`force_foreground` 之后）追加：

```rust
/// 空白归一化后取尾部 n 个字符（UIA 正文匹配用：终端渲染与 jsonl 原文的差异主要在空白与折行）
fn normalized_tail(s: &str, n: usize) -> String {
    let collapsed: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars: Vec<char> = collapsed.chars().collect();
    if chars.len() <= n {
        collapsed
    } else {
        chars[chars.len() - n..].iter().collect()
    }
}

/// 空白归一化后的子串包含判断
fn normalized_contains(haystack: &str, needle: &str) -> bool {
    let h: String = haystack.split_whitespace().collect::<Vec<_>>().join(" ");
    let n: String = needle.split_whitespace().collect::<Vec<_>>().join(" ");
    !n.is_empty() && h.contains(&n)
}

/// 读取窗口的终端可见文本（UI Automation TextPattern，屏幕阅读器通道）。
/// 尝试顺序：根元素直取 → 查找 Document 类型后代（Windows Terminal 的 TermControl）。
/// COM 初始化失败 / 模式不可用 / 超时 → None（视为 miss，不报错）
fn read_window_text(hwnd_val: isize) -> Option<String> {
    use windows::core::{CoCreateInstance, Interface};
    use windows::Win32::System::Com::{
        CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Accessibility::{
        CUIAutomation, IUIAutomation, IUIAutomationCondition, IUIAutomationElement,
        IUIAutomationTextPattern, TreeScope_Descendants, UIA_ControlTypePropertyId,
        UIA_DocumentControlTypeId, UIA_TextPatternId,
    };
    unsafe {
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let co_initialized = hr.is_ok();
        let result = (|| -> Option<String> {
            let automation: IUIAutomation =
                CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok()?;
            let root: IUIAutomationElement = automation.ElementFromHandle(HWND(hwnd_val)).ok()?;

            let try_text = |el: &IUIAutomationElement| -> Option<String> {
                let pattern = el
                    .GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId)
                    .ok()?;
                let range = pattern.DocumentRange().ok()?;
                let text = range.GetText(-1).ok()?;
                let s = text.to_string_lossy();
                if s.is_empty() { None } else { Some(s) }
            };

            if let Some(s) = try_text(&root) {
                return Some(s);
            }
            // Document 类型后代
            let cond: IUIAutomationCondition = automation
                .CreatePropertyCondition(
                    UIA_ControlTypePropertyId,
                    &windows::core::VARIANT::from(windows::core::I4(
                        UIA_DocumentControlTypeId.0 as i32,
                    )),
                )
                .ok()?;
            let doc = root.FindFirst(TreeScope_Descendants, &cond).ok()?;
            try_text(&doc)
        })();
        if co_initialized {
            CoUninitialize();
        }
        result
    }
}

/// 带超时的读取（单窗口 200ms）：UIA 调用可能被无响应窗口阻塞，超时即放弃该窗口
fn read_window_text_timeout(hwnd_val: isize) -> Option<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(read_window_text(hwnd_val));
    });
    rx.recv_timeout(std::time::Duration::from_millis(200))
        .ok()
        .flatten()
}
```

- [ ] **Step 4: 插入匹配层 + 签名扩展**

`resolve_and_focus` 增加参数 `last_message: Option<&str>`（放在 `project_name` 之后）；在"① marker 精确匹配"块之后、"② 标题打分"之前插入：

```rust
        // ①5 UIA 正文匹配：候选窗口的终端可见文本含卡片最新消息尾部（归一化后）
        // 且唯一命中 → 锁定。点击跳转通常发生在任务刚结束（最终回复仍在屏幕上），命中率高。
        if let Some(msg) = last_message {
            if !msg.is_empty() {
                let tail = normalized_tail(msg, 40);
                let hits: Vec<isize> = cands
                    .iter()
                    .take(8)
                    .filter(|(hwnd, _)| {
                        read_window_text_timeout(*hwnd)
                            .map(|text| normalized_contains(&text, &tail))
                            .unwrap_or(false)
                    })
                    .map(|(hwnd, _)| *hwnd)
                    .collect();
                if hits.len() == 1 {
                    force_foreground(hits[0]);
                    return Ok(FocusOutcome::Focused);
                }
            }
        }
```

`commands/session.rs` 的 `focus_session` 加参数 `last_message: Option<String>`，`#[cfg(windows)]` 分支传入 `last_message.as_deref()`，非 Windows 分支 `let _` 一并消化。`win32.rs` 的兼容入口 `focus_window_for_pid` 调用处补 `None`。

- [ ] **Step 5: 前端传参**

`useSessionJump.ts` 的 `JumpTarget` 加 `lastMessage?: string`，`focus` 的 invoke 参数加 `lastMessage: target.lastMessage`；`SessionCard` 传 `lastMessage: session.lastMessage ?? undefined`；`notification.tsx` 传 `lastMessage: payload.lastMessage`；`useNotification.ts` 系统通知 onAction 的 invoke 也补 `lastMessage: (notification.extra?.lastMessage as string) ?? undefined`（发送侧 extra 同步加 `lastMessage`）。

- [ ] **Step 6: 冒烟验证（必做）**

临时集成测试（仿既往诊断测试，跑完删）：对本机一个真实 WT 终端窗口的 hwnd 调 `read_window_text_timeout`，断言返回含该终端可见文字的非空文本；再对含两个不同会话的窗口集合验证 `normalized_contains` 区分度。

- [ ] **Step 7: 门禁 + Commit**

```bash
cd src-tauri && cargo test && cargo clippy --all-targets -- -D warnings
cd "E:\LLMproject\Github\MultiAgents-Manager" && pnpm lint && pnpm build
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src/window/win32.rs src-tauri/src/commands/session.rs src/hooks/useSessionJump.ts src/components/sessions/SessionCard.tsx src/pages/notification.tsx src/hooks/useNotification.ts
git commit -m "feat(window): uia text matching layer with last-message tail"
```

**验收条件**：冒烟测试证明能读到真实终端文本且能区分两个不同会话；全部门禁过。

### Task 3: marker 换 CONOUT$（spike 先行）

**Files:**
- Modify: `src-tauri/src/monitor/hooks.rs`（HOOK_SCRIPT）
- Modify: `docs/engineering/plans/2026-08-24-jump-v2-notification-i18n.md`（spike 记录）

- [ ] **Step 1: 替换 marker 注入行**

将 HOOK_SCRIPT 中现 marker 段（`printf '\033]0;...' > /dev/tty ...` 一行）替换为：

```bash
# 注入窗口标题 marker（MAM:<session_id 前 8 位>）。/dev/tty 在 hook（原生进程 spawn 的 bash）
# 上下文不可达，改写 Windows 控制台设备 CONOUT$（hook 子进程继承宿主控制台）
MID=$(printf '%s' "$SESSION_ID" | cut -c1-8)
powershell -NoProfile -Command "[IO.File]::WriteAllText('CONOUT$',[char]27+\"]0;MAM:$MID\"+[char]7)" >/dev/null 2>&1 || true
```

- [ ] **Step 2: spike 实测（go/no-go）**

启动 dev 应用（刷新脚本）→ 交互终端跑 claude 会话发消息 → 检查终端标题是否出现 `MAM:<8 位>` 且与卡片前缀一致；同时观察 claude 重写标题后 marker 是否仍会在下次 hook 事件时回来。

- [ ] **Step 3: 结论记录**

在 `2026-08-24-jump-v2-notification-i18n.md` 的 spike 结论块追加一行实测结果（go：标题出现；或 no-go：CONOUT$ 也不可达/被立即覆盖，marker 层退役，UIA 为精确匹配主力）。no-go 时**保留代码但接受 miss**（有 UIA 与认领池兜底），不回滚。

- [ ] **Step 4: Commit**

```bash
cd src-tauri && cargo clippy --all-targets -- -D warnings
git add src-tauri/src/monitor/hooks.rs docs/engineering/plans/2026-08-24-jump-v2-notification-i18n.md
git commit -m "feat(hooks): marker injection via CONOUT$ with spike conclusion"
```

**验收条件**：spike 结论（go/no-go 及证据）已记录；hook 事件产出不受影响（events 目录仍出现新文件）。

---

## Part B — 通知浮窗体验（spec 014）

### Task 4: 候选弹窗动态高度

**Files:**
- Modify: `src/pages/notification.tsx`、`src-tauri/capabilities/notification.json`

- [ ] **Step 1: 高度调整函数与调用点**

`notification.tsx` import 区加 `import { LogicalSize } from "@tauri-apps/api/dpi";`，组件内（`armTimer` 旁）加：

```tsx
  // 候选列表动态高度：N 个候选按 60+N*34+16 计算，上限 400（超出内部滚动）；null 还原 110
  const applyHeight = async (count: number | null) => {
    const h = count === null ? 110 : Math.min(60 + count * 34 + 16, 400);
    try {
      await getCurrentWindow().setSize(new LogicalSize(360, h));
    } catch {
      // 非 Tauri 环境忽略
    }
  };
```

调用点：① `jump()` 的 ambiguous 分支 `setCandidates(result.windows)` 后 `applyHeight(result.windows.length)`；② 候选按钮点击处（`setCandidates(null); ...hide()` 处）加 `applyHeight(null)`；③ listen 回调 `setCandidates(null)` 后加 `applyHeight(null)`。候选列表容器加 `max-h-full overflow-y-auto`（配合 400 上限滚动）。

- [ ] **Step 2: 权限 + 验证 + Commit**

`capabilities/notification.json` 的 permissions 追加 `"core:window:allow-set-size"`。

```bash
cd src-tauri && cargo clippy -- -D warnings
cd "E:\LLMproject\Github\MultiAgents-Manager" && pnpm lint && pnpm build
git add src/pages/notification.tsx src-tauri/capabilities/notification.json
git commit -m "fix(notification): dynamic height for window picker"
```

**验收条件**：3 个候选完整可点；8 个候选时窗口 400 高滚动可选；候选清空后高度还原。

### Task 5: 渠道统一（砍开关 + 测试按钮改浮窗预览）

**Files:**
- Modify: `src/hooks/useNotification.ts`、`src/pages/settings.tsx`、`src/i18n/locales/zh.json`/`en.json`

- [ ] **Step 1: useNotification 删分支**

删除 `useSystemToast` 分支（当前 158-177 行的 if/else），浮窗 try invoke 成为唯一主路径（catch 内系统 toast 降级**原样保留**）；init effect 中加 `localStorage.removeItem("mam.useSystemNotification");`。

- [ ] **Step 2: settings 删开关 + 测试按钮改预览**

删除 `useSystemNotification` state（:33）、localStorage 读写（:70-80）与开关 JSX；将测试按钮（:381 `sendNotification({...})`）替换为浮窗预览：

```tsx
                      await invoke("show_notification_window", {
                        payload: {
                          agentType: "claude",
                          agentLabel: "Claude",
                          projectName: t("settings.notifications.testProject"),
                          statusColor: "yellow",
                          status: "waiting",
                          lastMessage: t("settings.notifications.testMessage"),
                          pid: 0,
                          sessionId: "test",
                        },
                      });
```

i18n：`settings.notifications` 命名空间加 `testProject`（zh"通知测试"/en "Notification test"）、`testMessage`（zh"这是一条测试通知（浮窗预览）"/en "This is a test notification (float preview)"）；测试按钮文案键改为浮窗语义（沿用或改名 `testFloat`）；删除不再使用的系统通知开关键。

- [ ] **Step 3: 验证 + Commit**

```bash
cd "E:\LLMproject\Github\MultiAgents-Manager" && pnpm check
git add src/hooks/useNotification.ts src/pages/settings.tsx src/i18n/locales/zh.json src/i18n/locales/en.json
git commit -m "refactor(notification): single in-app channel with float preview test"
```

**验收条件**：设置页无系统通知开关；测试按钮弹出完整浮窗；grep 确认降级路径（catch 内 sendNotification）仍在。

### Task 6: 通知历史（铃铛 + 面板 + 持久化）

**Files:**
- Create: `src/lib/notificationHistory.ts`、`src/components/notifications/NotificationBell.tsx`
- Modify: `src/hooks/useNotification.ts`（记录）、`src/pages/home.tsx`（挂铃铛）、`src/i18n/locales/zh.json`/`en.json`

- [ ] **Step 1: 历史存储模块**

```ts
// 通知历史 — localStorage 持久化（最新在前，容量 50）
export interface HistoryEntry {
  agentType: string;
  projectName: string;
  status: string;
  lastMessage: string;
  pid: number;
  sessionId: string;
  at: number;
  read: boolean;
}

const KEY = "mam-notification-history";
const CAP = 50;

export function getHistory(): HistoryEntry[] {
  try {
    return JSON.parse(localStorage.getItem(KEY) ?? "[]");
  } catch {
    return [];
  }
}

export function addHistory(entry: Omit<HistoryEntry, "read">) {
  const list = [{ ...entry, read: false }, ...getHistory()].slice(0, CAP);
  localStorage.setItem(KEY, JSON.stringify(list));
  window.dispatchEvent(new CustomEvent("mam-history-updated"));
}

export function markAllRead() {
  localStorage.setItem(KEY, JSON.stringify(getHistory().map((e) => ({ ...e, read: true }))));
  window.dispatchEvent(new CustomEvent("mam-history-updated"));
}

export function getUnreadCount(): number {
  return getHistory().filter((e) => !e.read).length;
}
```

- [ ] **Step 2: 记录接入**

`useNotification.ts` 通知触发块（"// 通知"注释处、播放提示音之前）加：

```ts
        addHistory({
          agentType: session.agentType,
          projectName: session.projectName,
          status: session.status,
          lastMessage: session.lastMessage ?? "",
          pid: session.pid,
          sessionId: session.id,
          at: Date.now(),
        });
```

- [ ] **Step 3: 铃铛组件**

`src/components/notifications/NotificationBell.tsx`：

```tsx
// 通知历史铃铛 — 未读角标 + 历史面板（点击条目跳转对应会话）
import { useEffect, useState } from "react";
import { Bell } from "lucide-react";
import { toast } from "sonner";
import { useTranslation } from "react-i18next";
import { AGENT_BADGE } from "@/lib/agentBadge";
import { useSessionJump } from "@/hooks/useSessionJump";
import {
  getHistory,
  getUnreadCount,
  markAllRead,
  type HistoryEntry,
} from "@/lib/notificationHistory";

function timeAgo(at: number, t: (k: string) => string): string {
  const mins = Math.floor((Date.now() - at) / 60000);
  if (mins < 1) return t("sessions.justNow");
  if (mins < 60) return `${mins}m`;
  return `${Math.floor(mins / 60)}h`;
}

export function NotificationBell() {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const [entries, setEntries] = useState<HistoryEntry[]>([]);
  const [unread, setUnread] = useState(0);
  const { focus } = useSessionJump();

  useEffect(() => {
    const refresh = () => {
      setEntries(getHistory());
      setUnread(getUnreadCount());
    };
    refresh();
    window.addEventListener("mam-history-updated", refresh);
    return () => window.removeEventListener("mam-history-updated", refresh);
  }, []);

  const toggle = () => {
    const next = !open;
    setOpen(next);
    if (next) markAllRead();
  };

  const jumpTo = async (e: HistoryEntry) => {
    try {
      await focus({
        pid: e.pid,
        id: e.sessionId,
        agentType: e.agentType,
        projectName: e.projectName,
        lastMessage: e.lastMessage,
      });
    } catch {
      toast.error(t("notifications.jumpFailed"));
    }
  };

  return (
    <div className="relative">
      <button
        className="hover:bg-accent relative rounded p-1.5"
        onClick={toggle}
        title={t("notifications.historyTitle")}
      >
        <Bell className="h-4 w-4" />
        {unread > 0 && (
          <span className="absolute -top-0.5 -right-0.5 flex h-4 min-w-4 items-center justify-center rounded-full bg-red-500 px-1 text-[9px] font-bold text-white">
            {unread > 99 ? "99+" : unread}
          </span>
        )}
      </button>
      {open && (
        <div className="bg-card absolute right-0 z-50 mt-2 w-96 rounded-lg border p-2 shadow-xl">
          <p className="mb-2 px-1 text-xs font-semibold">{t("notifications.historyTitle")}</p>
          <div className="max-h-80 overflow-y-auto">
            {entries.length === 0 && (
              <p className="text-muted-foreground p-4 text-center text-xs">
                {t("notifications.historyEmpty")}
              </p>
            )}
            {entries.map((e) => {
              const badge = AGENT_BADGE[e.agentType];
              return (
                <button
                  key={e.at + e.sessionId}
                  className="hover:bg-accent/50 flex w-full items-center gap-2 rounded px-2 py-1.5 text-left"
                  onClick={() => jumpTo(e)}
                >
                  {badge && (
                    <span
                      className={`inline-flex shrink-0 items-center gap-1 rounded border px-1.5 py-0.5 text-[9px] ${badge.className}`}
                    >
                      <badge.Icon className="h-2.5 w-2.5" />
                      {badge.label}
                    </span>
                  )}
                  <span className="min-w-0 flex-1 truncate text-[11px]">
                    {e.projectName} — {e.lastMessage || t("sessions.noMessage")}
                  </span>
                  <span className="text-muted-foreground shrink-0 text-[10px]">
                    {timeAgo(e.at, t)}
                  </span>
                </button>
              );
            })}
          </div>
        </div>
      )}
    </div>
  );
}
```

- [ ] **Step 4: 挂载与 i18n**

`home.tsx` 页面头部（tab 切换按钮行 `Monitor`/`Package` 所在容器）右侧追加 `<NotificationBell />`（import 按现有方式）。i18n 新增顶层 `notifications` 命名空间（zh/en 同步）：`historyTitle`（通知历史/Notification history）、`historyEmpty`（暂无通知/No notifications yet）、`jumpFailed`（会话已结束或不可跳转/Session ended or not jumpable）。

- [ ] **Step 5: 验证 + Commit**

```bash
cd "E:\LLMproject\Github\MultiAgents-Manager" && pnpm check
git add src/lib/notificationHistory.ts src/components/notifications/NotificationBell.tsx src/hooks/useNotification.ts src/pages/home.tsx src/i18n/locales/zh.json src/i18n/locales/en.json
git commit -m "feat(notifications): history bell with unread badge and jump"
```

**验收条件**：触发通知后铃铛角标 +1；打开面板角标清零且列表含该通知；点击历史条目可跳转（会话已结束时 toast）；重启应用历史仍在。

### Task 7: 全量门禁 + 人工验收清单

- [ ] **Step 1: 自动门禁**

```bash
cd src-tauri && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check
cd "E:\LLMproject\Github\MultiAgents-Manager" && pnpm check
```

- [ ] **Step 2: 人工验收清单（逐项记录 ✅/❌/待用户）**

| # | 操作 | 预期（判定标准） |
|---|------|------------------|
| 1 | 开双 claude（不同项目）+ 一个 opencode + 一个空 PowerShell + 一个 codex 会话，点 codex 卡进选择器 | 候选列表无 "Claude Code" 与 "OC |" 窗口 |
| 2 | 同环境点 claude 卡进选择器 | 候选列表无 "OC |" 窗口 |
| 3 | 点双 claude 中任一张卡 | 精确聚焦对应窗口、不弹选择器（UIA 或 marker 命中）；两张都试 |
| 4 | 交互终端跑 claude 发消息 | 标题是否出现 `MAM:<8 位>`（spike 结论记录） |
| 5 | 通知浮窗弹 ≥3 候选 | 全部可点；8 个候选滚动可选；清空后高度还原 110 |
| 6 | 设置页 | 无系统通知开关；测试按钮弹出浮窗预览 |
| 7 | 触发通知 → 看铃铛 | 角标 +1；打开面板清零；消失的通知在列表中 |
| 8 | 点历史条目 | 会话存活则跳转；已结束则 toast |
| 9 | 重启应用 | 历史仍在；中英文界面文案正确 |

- [ ] **Step 3: 汇报**

各 Task 状态（含 Task 3 spike 结论）、门禁结果、9 项验收逐项结果、`git log --oneline 794a993..HEAD`。

---

## 范围外

- 历史条目删除/搜索/筛选、SQLite 持久化
- 认领关键词用户自定义
- WT 标签页级定位、300ms ready-ack（延续后续债清单）
- macOS/Linux 行为变化
