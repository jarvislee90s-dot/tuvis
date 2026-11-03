# M3 · 看板打磨 + 实时与内容 + 文件预览 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 移动看板打磨为产品级体验（品牌/结构/皮肤）+ 状态变化 2 秒级到达（SSE）+ 点开会话看 ZCode 式对话（含文件预览）。

**Architecture:** 前端三条路由（看板 / 会话详情 / 文件预览浮层）；后端新增 SessionWatcher（tokio 任务，2s 快照 diff→broadcast）、SSE 端点、会话内容读取层（八工具 adapter 扩展）、文件读取端点（限 cwd 只读）。数据同源 P8 原则不变。

**Tech Stack:** axum SSE（现有服务器扩展）· React 19（`src/mobile/` 扩展）· `react-markdown` + `remark-gfm`（markdown 渲染）· `highlight.js`（代码高亮）· 无新 Rust crate（全部用现有依赖实现）。

## Global Constraints

- 需求唯一来源：`docs/research/一期待办-v6摘选-私有.md` M3 节（含全部定稿裁决）。
- **前置依赖**：M2 分支 `feat/m2-remote-board` 必须先合并 main（**核查确认：origin/main 当前仍在 v0.4.1 = M1 only，M2 尚未合并**——执行者需等 M2 PR 合入后再切分支；如已合并则 rebase main 后直接开干）。
- **P8 数据同源不变**：sessions 端点直调 `adapter::get_all_sessions`，禁止复制聚合逻辑。
- **文件预览安全红线**：文件读取 API 仅限该会话 project cwd 内、只读、文本 ≤500KB、图片 ≤5MB。路径穿越校验（canonicalize 后 starts_with cwd）。
- **文件预览交付顺序**：ZCode 先行（part 表已知），其余 7 工具在 Task 9 一边探测一边实现。**文件预览放在计划靠后位置**（Task 8-9），前方的 UI/实时/内容消息流不被文件系统不确定性阻塞。
- **去重口径**：全部在 SessionWatcher 跃迁判定层完成——消费者（SSE/推送/桌面通知）只收边沿事件，移动端不独立去重。
- 代码注释中文、标识符英文；每 Task 结束 `cargo test` + `pnpm build:mobile && pnpm check:i18n` 全绿才 commit（conventional commits）；不 push。
- **CI 同步检查项**（M2 教训）：新依赖（`react-markdown` 等）需同时更新 CI workflow 的 cache 配置（如有）。
- 桌面体验零回归（宪法原则 5）。
- clippy 门禁用 `--all-targets` 措辞。

## 依赖新增（事后核查确认均不在当前 Cargo.toml / package.json）

**Rust 导入提醒**：api.rs 现有 use 块不含 `Query`——Task 7/8 新 handler 使用 `axum::extract::Query`，需追加导入。

**Rust（Cargo.toml [dependencies]）**：
- `tokio-stream = { version = "0.1", features = ["sync"] }`（SSE broadcast 流）
- `futures = "0.3"`（SSE stream chain）

**前端（pnpm add）**：
- `react-markdown remark-gfm rehype-highlight highlight.js`

## File Structure

```
src-tauri/src/remote/
├── mod.rs           # 已有：生命周期/命令
├── server.rs        # 已有：路由（追加 SSE + session-messages + file 端点）
├── api.rs           # 已有：handlers（追加三个新端点）
├── watcher.rs       # 新增：SessionWatcher（2s 快照 diff → broadcast）
├── content.rs       # 新增：会话内容读取层（八工具 adapter 统一出口）
└── files.rs         # 新增：文件路径提取 + 安全读取

src/mobile/
├── App.tsx          # 修改：三路由（board / detail / 含文件浮层）
├── Board.tsx        # 修改：P8 六条 UI
├── SessionDetail.tsx # 新增：ZCode 式对话视图
├── FilePreview.tsx  # 新增：文件预览（分屏/全屏浮窗）
├── board-logic.ts   # 修改：活跃排序/品牌色映射/受管过滤
├── theme.ts         # 新增：日/夜皮肤管理
└── api.ts           # 修改：SSE + 会话内容 + 文件 API
```

---

### Task 1: Host 信息 API + 看板页头（P8a 版本号 + P8b 本机名）

**Files:**
- Modify: `src-tauri/src/remote/mod.rs`（`remote_status` 扩展 host 字段）
- Modify: `src/mobile/Board.tsx`（页头品牌行）
- Test: `src-tauri/src/remote/mod.rs` 底部

**Interfaces:**
- Produces: `remote_status()` 返回值新增 `"host": {"name": "JARVIS-Win", "platform": "windows", "version": "0.4.1"}`。
- Produces: 前端 `HostInfo` 类型。

- [ ] **Step 1: 写失败测试**

```rust
#[test]
fn remote_status_includes_host_info() {
    let st = remote_status();
    let host = st.get("host").expect("remote_status 应含 host 字段");
    assert!(host.get("name").is_some());
    assert!(host.get("version").is_some());
}
```

- [ ] **Step 2: 跑测试确认失败** → `cargo test remote_status_includes`

- [ ] **Step 3: 实现**

`mod.rs` 的 `remote_status()` 中追加：

```rust
    // P8a+P8b：品牌版本号 + 本机名称（双机双子域辨识）
    let host_name = crate::database::dao::settings::get_setting("remote.host_name")
        .unwrap_or_else(|| sysinfo::System::host_name().unwrap_or_else(|| "MAM".into()));
    let platform = if cfg!(target_os = "macos") { "macos" }
        else if cfg!(windows) { "windows" } else { "linux" };
    // host 值合并进 json!({...}) 里：
    //   "host": { "name": host_name, "platform": platform, "version": env!("CARGO_PKG_VERSION") }
    // P8d 受管名单（chips 过滤数据源）：
    //   "enabledTools": TOOL_IDS.iter().filter(|id| dao::agent_tool::get_tool_enabled(id)).collect()
```

注意：`sysinfo` 已有依赖（Cargo.toml 现有），`System::host_name()` 是其静态方法返回 `Option<String>`，无需新增 crate。

`Board.tsx` 页头加品牌行：`<header className="flex items-center gap-2 px-4 pt-4 pb-2"><span className="text-lg font-bold">MAM</span><span className="text-xs text-slate-400">v{host.version}</span><span className="ml-auto text-sm">{host.name}</span></header>`

- [ ] **Step 4: 跑测试确认通过** → `cargo test remote_status_includes`
- [ ] **Step 5: `pnpm build:mobile` 确认编译** → Commit `feat(m3-board): P8a/P8b 页头品牌版本+本机名`

---

### Task 2: P7 修正（0.0.0.0 主显示地址）+ P8c 卡片主行重排

**Files:**
- Modify: `src-tauri/src/remote/mod.rs`（`remote_status` 的 `url` 字段修正）
- Modify: `src/mobile/Board.tsx` + `src/mobile/board-logic.ts`（卡片结构）
- Test: `src-tauri/src/remote/mod.rs` 已有测试扩展

**Interfaces:**
- P7 修正：`remote_status()` 在 `bind == "0.0.0.0"` 时，`url` 字段返回局域网 IP 而非 0.0.0.0。
- P8c：卡片主行 = 工具名+项目名+右侧状态点；副行 = 会话标题+最新消息预览（现有数据重新排布）。

- [ ] **Step 1: P7 修正——写失败测试**

```rust
#[test]
fn url_uses_lan_ip_when_bound_to_all_interfaces() {
    // 设置 bind 为 0.0.0.0 → url 不应包含 "0.0.0.0"
    crate::database::dao::settings::set_setting(KEY_BIND, "0.0.0.0");
    crate::database::dao::settings::set_setting(KEY_PUBLIC_ACK, "true");
    let st = remote_status();
    let url = st["url"].as_str().unwrap();
    assert!(!url.contains("0.0.0.0"), "url 不应含 0.0.0.0: {url}");
    // 清理
    crate::database::dao::settings::set_setting(KEY_BIND, "127.0.0.1");
    crate::database::dao::settings::set_setting(KEY_PUBLIC_ACK, "false");
}
```

- [ ] **Step 2: 确认失败** → `cargo test url_uses_lan_ip`

- [ ] **Step 3: 实现 P7 修正**

`remote_status()` 中：

```rust
    // P7 v6 修正：对外绑定时主显示地址须为可达 IP，不显示 0.0.0.0（绑定地址不可连）
    let display_url = if bind == "0.0.0.0" {
        let lan = local_lan_ips();
        format!("http://{}:{}/m", lan.first().unwrap_or(&"127.0.0.1".into()), port)
    } else {
        format!("http://{bind}:{port}/m")
    };
```

- [ ] **Step 4: P8c 卡片重排**（Board.tsx）

```tsx
// 卡片主行：工具名+项目名+右侧状态点（P8c 定稿）
// 副行：会话标题+最新消息预览（现有数据重新排布，无新 API）
<div className="rounded-lg border border-slate-800 bg-slate-900 p-3">
  <div className="flex items-center gap-2">
    <ToolIcon toolId={s.agentType} size={16} />
    <span className="text-sm font-medium">{TOOL_LABELS[s.agentType]}</span>
    <span className="text-sm text-slate-400">{s.projectName}</span>
    <span className={`ml-auto h-2.5 w-2.5 rounded-full ${STATUS_DOT_COLOR[s.status]}`} />
  </div>
  <div className="mt-1.5 text-xs text-slate-500">
    {s.title && <span className="mr-2">{s.title}</span>}
    {s.lastMessage && <span className="truncate">{s.lastMessage}</span>}
  </div>
</div>
```

- [ ] **Step 5: 跑测试+构建** → `cargo test && pnpm build:mobile`
- [ ] **Step 6: Commit** `feat(m3-board): P7 0.0.0.0 修正 + P8c 卡片主行重排`

---

### Task 3: P8d 受管过滤 + P8e 品牌色多行折叠 chips + 活跃排序

**Files:**
- Modify: `src/mobile/board-logic.ts` + `src/mobile/Board.tsx`
- Test: `tests/mobile/board-logic.test.ts`（vitest）

**Interfaces:**
- Produces: `board-logic.ts` 导出 `TOOL_BRAND_COLORS`（八工具品牌色映射）、`sortChipsByActivity(tools, sessions)`、`filterEnabledTools(tools, enabledIds)`、`collapseChips(chips, maxRow)`。

- [ ] **Step 1: 写失败测试**（vitest）

```ts
import { describe, expect, it } from "vitest";
import { TOOL_BRAND_COLORS, sortChipsByActivity, filterEnabledTools } from "@/mobile/board-logic";

describe("P8d/P8e board-logic", () => {
  it("品牌色映射覆盖八工具", () => {
    for (const t of ["claude","codex","opencode","openclaw","kimi","workbuddy","zcode","dsh"]) {
      expect(TOOL_BRAND_COLORS[t]).toBeTruthy();
    }
  });
  it("按最新活跃排序", () => {
    const sessions = [
      { agentType: "claude", lastActivityAt: "2026-09-15T10:00:00Z" },
      { agentType: "zcode", lastActivityAt: "2026-09-15T12:00:00Z" },
      { agentType: "claude", lastActivityAt: "2026-09-15T11:00:00Z" },
    ];
    const sorted = sortChipsByActivity(["claude","zcode"], sessions);
    expect(sorted[0]).toBe("zcode"); // 12:00 最新
  });
  it("仅显示受管工具 ∩ 有卡工具", () => {
    const result = filterEnabledTools(["claude","codex","zcode"], new Set(["claude","zcode"]));
    expect(result).toEqual(["claude","zcode"]);
    expect(result).not.toContain("codex");
  });
});
```

- [ ] **Step 2: 确认失败** → `pnpm test`
- [ ] **Step 3: 实现**

`board-logic.ts`：

```ts
// P8e 品牌色（对齐桌面端 ToolIcon 色系）
export const TOOL_BRAND_COLORS: Record<string, string> = {
  claude: "#D97757",   // 橙
  codex: "#8B5CF6",    // 紫
  opencode: "#10B981", // 绿
  openclaw: "#F59E0B", // 琥珀
  kimi: "#3B82F6",     // 蓝
  workbuddy: "#EF4444", // 红
  zcode: "#6366F1",    // 靛蓝
  dsh: "#4D6BFE",      // 深蓝
};

export function sortChipsByActivity(tools: string[], sessions: {agentType: string; lastActivityAt: string}[]): string[] {
  const latest = new Map<string, number>();
  for (const s of sessions) {
    const t = new Date(s.lastActivityAt).getTime() || 0;
    latest.set(s.agentType, Math.max(latest.get(s.agentType) ?? 0, t));
  }
  return [...tools].sort((a, b) => (latest.get(b) ?? 0) - (latest.get(a) ?? 0));
}

// P8d 受管过滤数据来源：enabled 集合从 remote_status() 返回值新增的
// "enabledTools" 字段获取（后端从 dao::agent_tool 读已启用工具 id 列表）
export function filterEnabledTools(tools: string[], enabled: Set<string>): string[] {
  return tools.filter(t => enabled.has(t));
}
```

`Board.tsx`：chips 区域改为多行 flex-wrap + 折叠/展开按钮（`useState collapsed`，超一行时 `flex-nowrap overflow-hidden h-[36px]` + "展开"按钮切 `flex-wrap`），排序调 `sortChipsByActivity`，chips 底色用 `TOOL_BRAND_COLORS[tool]`。

- [ ] **Step 4: 跑测试+构建** → `pnpm test && pnpm build:mobile`
- [ ] **Step 5: Commit** `feat(m3-board): P8d/P8e 受管过滤+品牌色+多行折叠+活跃排序`

---

### Task 4: P8f 日/夜双皮肤

**Files:**
- Create: `src/mobile/theme.ts`
- Modify: `src/mobile/App.tsx`（主题提供）、`Board.tsx`（色类切换）、`mobile.html`（初始主题脚本防闪白）

- [ ] **Step 1: 实现**

`theme.ts`：

```ts
// P8f 日/夜双皮肤：默认跟随系统、手动切换持久化（localStorage）
export type Theme = "light" | "dark";
const KEY = "mam-theme";

export function getInitialTheme(): Theme {
  const saved = localStorage.getItem(KEY) as Theme | null;
  if (saved === "light" || saved === "dark") return saved;
  return matchMedia("(prefers-color-scheme: light)").matches ? "light" : "dark";
}

export function toggleTheme(): Theme {
  const cur = getInitialTheme();
  const next = cur === "dark" ? "light" : "dark";
  localStorage.setItem(KEY, next);
  document.documentElement.classList.toggle("dark", next === "dark");
  return next;
}
```

`Board.tsx` 顶部加切换按钮（Sun/Moon 图标）；tailwind 类用 `dark:` 前缀切换（`bg-white dark:bg-slate-950` 双态）。`mobile.html` `<head>` 加防闪白内联脚本：

```html
<script>
  const t = localStorage.getItem("mam-theme");
  if (t === "light" || (!t && matchMedia("(prefers-color-scheme: light)").matches)) {
    document.documentElement.classList.remove("dark");
  } else {
    document.documentElement.classList.add("dark");
  }
</script>
```

- [ ] **Step 2: 构建验证** → `pnpm build:mobile`
- [ ] **Step 3: Commit** `feat(m3-board): P8f 日/夜双皮肤`

---

### Task 5: SessionWatcher（A3 后端事件桥）

**Files:**
- Create: `src-tauri/src/remote/watcher.rs`
- Modify: `src-tauri/src/remote/mod.rs`（启动 watcher、注入 RemoteState）

**Interfaces:**
- Produces:

```rust
// watcher.rs
pub struct SessionWatcher;
pub struct TransitionEvent {
    pub session_id: String,
    pub agent_type: String,
    pub from: String,
    pub to: String,
    pub project_name: String,
    pub last_message: Option<String>,
    pub ts: i64,
}
impl SessionWatcher {
    /// 启动 2s 周期 watcher（tokio::spawn），返回 broadcast sender
    pub fn start() -> tokio::sync::broadcast::Sender<TransitionEvent>;
}
```

- [ ] **Step 1: 写失败测试**（watcher 的 diff 纯函数）

`watcher.rs` 底部：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Session, SessionStatus};

    fn session(id: &str, status: SessionStatus, project: &str) -> Session {
        Session { id: id.into(), agent_type: crate::session::AgentType::Claude,
            project_name: project.into(), project_path: String::new(),
            title: None, git_branch: None, github_url: None,
            status, last_message: None, last_message_role: None,
            last_activity_at: String::new(), pid: 1, cpu_usage: 0.0,
            active_subagent_count: 0, form: crate::session::ProcessForm::Cli,
            jump_supported: false, unread: false }
    }

    #[test]
    fn diff_emits_only_state_changes() {
        let prev = vec![session("a", SessionStatus::Processing, "p1"),
                        session("b", SessionStatus::Idle, "p2")];
        let curr = vec![session("a", SessionStatus::Waiting, "p1"),  // changed → emit
                        session("b", SessionStatus::Idle, "p2"),      // same → skip
                        session("c", SessionStatus::Processing, "p3")]; // new → emit
        let events = diff_transitions(&prev, &curr);
        assert_eq!(events.len(), 2, "仅 a 和 c 触发事件");
        assert_eq!(events[0].from, "processing");
        assert_eq!(events[0].to, "waiting");
    }

    #[test]
    fn diff_no_change_no_event() {
        let prev = vec![session("a", SessionStatus::Idle, "p")];
        let curr = vec![session("a", SessionStatus::Idle, "p")];
        assert!(diff_transitions(&prev, &curr).is_empty());
    }
}
```

- [ ] **Step 2: 确认失败** → `cargo test remote::watcher`
- [ ] **Step 3: 实现**

```rust
// SessionWatcher：2s 周期快照 diff → broadcast（跃迁判定唯一去重点）
// 消费者（SSE/推送/桌面通知）只收边沿事件，不独立去重

use tokio::sync::broadcast;
use crate::session::{Session, SessionsResponse};

#[derive(Debug, Clone, serde::Serialize)]
pub struct TransitionEvent {
    pub session_id: String,
    pub agent_type: String,
    pub from: String,
    pub to: String,
    pub project_name: String,
    pub last_message: Option<String>,
    pub ts: i64,
}

pub struct SessionWatcher;

impl SessionWatcher {
    pub fn start() -> broadcast::Sender<TransitionEvent> {
        let (tx, _) = broadcast::channel(64);
        let tx_clone = tx.clone();
        tokio::spawn(async move {
            let mut prev: Vec<Session> = Vec::new();
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                let resp = crate::adapter::get_all_sessions();
                let events = diff_transitions(&prev, &resp.sessions);
                for e in events {
                    let _ = tx_clone.send(e);
                }
                prev = resp.sessions;
            }
        });
        tx
    }
}

/// 纯函数：快照 diff → 边沿事件（可单测；去重唯一来源）
pub fn diff_transitions(prev: &[Session], curr: &[Session]) -> Vec<TransitionEvent> {
    let prev_map: std::collections::HashMap<&str, &Session> =
        prev.iter().map(|s| (s.id.as_str(), s)).collect();
    let now = chrono::Utc::now().timestamp_millis();
    curr.iter()
        .filter_map(|c| {
            let p = prev_map.get(c.id.as_str())?;
            if p.status != c.status {
                Some(TransitionEvent {
                    session_id: c.id.clone(),
                    agent_type: format!("{:?}", c.agent_type).to_lowercase(),
                    from: format!("{:?}", p.status).to_lowercase(),
                    to: format!("{:?}", c.status).to_lowercase(),
                    project_name: c.project_name.clone(),
                    last_message: c.last_message.clone(),
                    ts: now,
                })
            } else { None }
        })
        .collect()
}
```

`mod.rs`：`RemoteState` 增加 `pub watcher_tx: broadcast::Sender<TransitionEvent>`；`start_server` 时创建 watcher 并存入 state；`lib.rs` 的 setup 中远程启动时也启动 watcher。

- [ ] **Step 4: 跑测试** → `cargo test remote::watcher`
- [ ] **Step 5: Commit** `feat(m3-realtime): SessionWatcher 事件桥（2s diff→broadcast）`

---

### Task 6: SSE 端点 + 前端实时提醒（C1 后半）

**Files:**
- Modify: `src-tauri/src/remote/api.rs` + `server.rs`（SSE 路由）
- Modify: `src/mobile/api.ts`（SSE 客户端+降级）
- Modify: `src/mobile/Board.tsx`（横幅/提示音/振动）

**Interfaces:**
- Produces: `GET /m/api/v1/events`（text/event-stream；**gate 自动覆盖**——端点加在 `api_router()` 内，走 nest 的内层 gate）
  - 首帧：`event: snapshot\ndata: {全量 SessionsResponse}\n\n`
  - 增量：`event: transition\ndata: {TransitionEvent}\n\n`
  - 心跳：`: ping\n\n`（15s）
- 前端：SSE 连接 → 断线退避重连 → 2 次失败降级轮询 3s

- [ ] **Step 1: SSE handler**（api.rs）

```rust
use axum::response::Sse;
use futures::stream::Stream;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

pub async fn events(
    State(st): State<std::sync::Arc<RemoteState>>,
) -> Sse<impl Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>> {
    let rx = st.watcher_tx.subscribe();
    let snapshot = (st.session_source)();
    let initial = futures::stream::once(async move {
        Ok::<_, std::convert::Infallible>(
            axum::response::sse::Event::default()
                .event("snapshot")
                .data(serde_json::to_string(&snapshot).unwrap_or_default())
        )
    });
    let transitions = BroadcastStream::new(rx)
        .filter_map(|msg| async move {
            msg.ok().map(|e| {
                Ok(axum::response::sse::Event::default()
                    .event("transition")
                    .data(serde_json::to_string(&e).unwrap_or_default()))
            })
        });
    // 心跳省略实现（tokio::time::interval + ready 合并），MVP 先不做
    Sse::new(initial.chain(transitions))
        .keep_alive(axum::response::sse::KeepAlive::default())
}
```

`server.rs` 路由追加：`.route("/m/api/v1/events", get(api::events))`

注意：需在 Cargo.toml 加 `tokio-stream = { version = "0.1", features = ["sync"] }` 和 `futures = "0.3"`。

- [ ] **Step 2: 前端 SSE 客户端**（api.ts）

```ts
export function connectEvents(
  onSnapshot: (data: unknown) => void,
  onTransition: (data: unknown) => void,
  onDegraded: () => void,  // 降级回调
): () => void {            // 返回 close
  let es: EventSource | null = null;
  let failures = 0;
  let pollTimer: ReturnType<typeof setInterval> | null = null;

  function connect() {
    es = new EventSource("/m/api/v1/events");
    es.addEventListener("snapshot", (e) => { failures = 0; onSnapshot(JSON.parse(e.data)); });
    es.addEventListener("transition", (e) => { failures = 0; onTransition(JSON.parse(e.data)); });
    es.onerror = () => {
      es?.close();
      failures++;
      if (failures >= 2) { degradeToPolling(); }
      else { setTimeout(connect, 1000 * failures); }  // 退避
    };
  }

  function degradeToPolling() {
    es?.close(); es = null;
    onDegraded();
    pollTimer = setInterval(async () => {
      const data = await fetchSessions<unknown>();
      if (data) onSnapshot(data);
    }, 3000);
  }

  connect();
  return () => { es?.close(); if (pollTimer) clearInterval(pollTimer); };
}
```

- [ ] **Step 3: Board.tsx 接入**（替换 setInterval 轮询为 SSE + 降级；transition 事件触发横幅+提示音+振动）
- [ ] **Step 4: 构建验证** → `cargo test && pnpm build:mobile && pnpm test`
- [ ] **Step 5: Commit** `feat(m3-realtime): SSE 端点+前端实时提醒+轮询降级`

---

### Task 7: 会话内容读取 API（C2 后端）

**Files:**
- Create: `src-tauri/src/remote/content.rs`
- Modify: `src-tauri/src/remote/api.rs` + `server.rs`

**Interfaces:**
- Produces: `GET /m/api/v1/session-messages?agent_type=&session_id=&limit=200&before=`
  - 返回 `{messages: [{seq, role, content, kind, ts, toolName?, toolArgs?, isCollapsed}]}`
  - `kind`：`"user" | "assistant" | "thinking" | "tool-call" | "tool-result"`

- [ ] **Step 1: 实现 content.rs（分工具读取）**

```rust
// 会话内容读取层：八工具统一出口（P9）
// 每个工具从其原生存储中读全量消息流，映射为统一格式

use serde::Serialize;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMessage {
    pub seq: i64,
    pub role: String,         // user / assistant
    pub kind: String,         // user / assistant / thinking / tool-call / tool-result
    pub content: String,      // 文本内容或工具摘要
    pub ts: Option<i64>,
    pub tool_name: Option<String>,
    pub tool_args: Option<String>,  // JSON 字符串
    pub collapsed: bool,      // thinking 和 tool-call 默认折叠
}

pub fn read_session_messages(agent_type: &str, session_id: &str, limit: usize) -> Result<Vec<SessionMessage>, String> {
    match agent_type {
        "zcode" => read_zcode_messages(session_id, limit),
        "dsh" => read_dsh_messages(session_id, limit),
        "claude" => read_jsonl_messages(agent_type, session_id, limit),
        "codex" => read_jsonl_messages(agent_type, session_id, limit),
        "kimi" => read_jsonl_messages(agent_type, session_id, limit),
        "workbuddy" => read_jsonl_messages(agent_type, session_id, limit),
        "opencode" => read_opencode_messages(session_id, limit),
        "openclaw" => read_openclaw_messages(session_id, limit),
        _ => Err(format!("未知工具: {agent_type}")),
    }
}

// ZCode：查 cli/db/db.sqlite 的 message + part 表
fn read_zcode_messages(session_id: &str, limit: usize) -> Result<Vec<SessionMessage>, String> {
    let db_path = dirs::home_dir().unwrap_or_default().join(".zcode/cli/db/db.sqlite");
    let conn = rusqlite::Connection::open_with_flags(&db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("打开 ZCode db 失败: {e}"))?;
    let mut stmt = conn.prepare(
        "SELECT m.sequence, m.role, p.kind, p.data, m.time_created
         FROM message m JOIN part p ON p.message_id = m.id
         WHERE m.session_id = ?1 ORDER BY m.sequence DESC, p.time_created DESC LIMIT ?2"
    ).map_err(|e| format!("查询失败: {e}"))?;
    // ... 逐行映射为 SessionMessage（kind 映射：text→assistant/user、reasoning→thinking、tool-call→tool-call）
    todo!("实现映射——执行者参照 M0 spike 判定 A 的 part kind 实测值")
}

// dsh：解压 session.v3.jsonl.zstd 读事件流
fn read_dsh_messages(session_id: &str, limit: usize) -> Result<Vec<SessionMessage>, String> {
    // 1. 找到 sessions/<projectKey>/<session-dir>/ 下的代际文件
    // 2. decode_zstd_frames 解压
    // 3. 遍历事件：user/message→user、assistant/message→assistant、
    //    data.message.content 中 type=="reasoning"→thinking、type=="tool-call"→tool-call
    todo!("实现——执行者参照 dsh/mod.rs 已有 decode 逻辑")
}

// 通用 JSONL 类（claude/codex/kimi/workbuddy）：读尾部 N 条
fn read_jsonl_messages(agent_type: &str, session_id: &str, limit: usize) -> Result<Vec<SessionMessage>, String> {
    // 各工具的 JSONL 路径推导逻辑已在现有 parser 中，此处复用路径推导+追加解析
    todo!("实现——执行者参照各 parser 的路径推导")
}

fn read_opencode_messages(session_id: &str, limit: usize) -> Result<Vec<SessionMessage>, String> {
    todo!("OpenCode SQLite")
}

fn read_openclaw_messages(session_id: &str, limit: usize) -> Result<Vec<SessionMessage>, String> {
    todo!("OpenClaw state.json")
}
```

**实现注意**：每个 `todo!()` 是一个子任务，执行者参照现有 parser（`monitor/*_parser.rs`）的路径推导与解析逻辑。ZCode 和 dsh 最成熟（M0 已探明格式），其余参照现有 parser。

- [ ] **Step 2: API 端点**（api.rs）

```rust
pub async fn session_messages(
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let agent = params.get("agent_type").ok_or(StatusCode::BAD_REQUEST)?;
    let sid = params.get("session_id").ok_or(StatusCode::BAD_REQUEST)?;
    let limit = params.get("limit").and_then(|s| s.parse().ok()).unwrap_or(200);
    match crate::remote::content::read_session_messages(agent, sid, limit) {
        Ok(msgs) => Ok(Json(serde_json::json!({ "messages": msgs }))),
        Err(e) => { log::warn!("read_session_messages: {e}"); Err(StatusCode::NOT_FOUND) }
    }
}
```

- [ ] **Step 3: 路由注册 + 测试**（用黄金夹具验证 ZCode/dsh 映射正确性）
- [ ] **Step 4: Commit** `feat(m3-content): 八工具会话内容读取 API`

---

### Task 8: ZCode 式会话详情页（C2 前端）+ ZCode 文件预览

**Files:**
- Create: `src/mobile/SessionDetail.tsx`
- Create: `src/mobile/FilePreview.tsx`
- Modify: `src/mobile/App.tsx`（路由）、`api.ts`（session-messages + file API）
- Create: `src-tauri/src/remote/files.rs`（文件路径提取 + 安全读取）
- Modify: `src-tauri/src/remote/api.rs` + `server.rs`（file 端点）
- 前端依赖：`pnpm add react-markdown remark-gfm rehype-highlight`

**Interfaces:**
- Produces: `GET /m/api/v1/file?session_id=&path=` → `{content: string, mime: string, size: usize}` 或二进制（图片）
- SessionDetail 组件 props：`{sessionId, agentType, onBack}` → 渲染 ZCode 式对话视图
- FilePreview 组件 props：`{filePath, sessionId, mode: "split"|"fullscreen", onClose}`

- [ ] **Step 1: 文件路径提取 + 安全读取**（files.rs）

```rust
// 文件预览：从会话事件中提取涉及的文件路径 + 安全读取（限 cwd 只读）

/// 从会话消息中提取文件路径列表（用于文件面板展示）
pub fn extract_file_paths(agent_type: &str, session_id: &str) -> Vec<String> {
    match agent_type {
        "zcode" => extract_zcode_paths(session_id),
        _ => Vec::new(), // 其他工具在 Task 9 逐个实现
    }
}

/// ZCode：从 part 表的 tool-call 数据中提取文件路径参数
fn extract_zcode_paths(session_id: &str) -> Vec<String> {
    let db_path = dirs::home_dir().unwrap_or_default().join(".zcode/cli/db/db.sqlite");
    let Ok(conn) = rusqlite::Connection::open_with_flags(&db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else { return Vec::new() };
    // 查询 tool-call 类型的 part，从 data JSON 中提取 file_path / path / filename 字段
    let Ok(mut stmt) = conn.prepare(
        "SELECT p.data FROM part p JOIN message m ON p.message_id = m.id
         WHERE m.session_id = ?1 AND p.kind = 'tool-call'"
    ) else { return Vec::new() };
    // ... 解析 JSON 提取路径字段，去重
    todo!("实现——part.data 中 tool-call 参数的文件路径提取")
}

/// 安全读取文件（限 cwd、只读、大小限制）
pub fn read_file_safe(session_cwd: &str, path: &str) -> Result<(Vec<u8>, String), String> {
    let full = std::path::Path::new(path);
    // 安全：canonicalize 后必须在 session cwd 内
    let cwd = std::path::Path::new(session_cwd).canonicalize().map_err(|e| e.to_string())?;
    let canon = full.canonicalize().map_err(|e| e.to_string())?;
    if !canon.starts_with(&cwd) {
        return Err("路径越界：文件不在项目目录内".into());
    }
    let meta = canon.metadata().map_err(|e| e.to_string())?;
    let max = 500 * 1024; // 500KB 文本 / 5MB 图片（按 MIME 分支）
    if meta.len() > max as u64 {
        return Err(format!("文件过大: {} bytes", meta.len()));
    }
    let bytes = std::fs::read(&canon).map_err(|e| e.to_string())?;
    let mime = mime_from_ext(canon.extension().and_then(|e| e.to_str()).unwrap_or(""));
    Ok((bytes, mime))
}

fn mime_from_ext(ext: &str) -> String {
    match ext {
        "md" => "text/markdown", "rs" => "text/rust", "ts" | "tsx" => "text/typescript",
        "js" | "mjs" => "text/javascript", "py" => "text/python",
        "png" => "image/png", "jpg" | "jpeg" => "image/jpeg", "gif" => "image/gif",
        "svg" => "image/svg+xml", "json" => "application/json",
        _ => "text/plain",
    }.to_string()
}
```

- [ ] **Step 2: file 端点**（api.rs）

```rust
pub async fn read_file(
    Query(params): Query<HashMap<String, String>>,
) -> Result<axum::response::Response, StatusCode> {
    let sid = params.get("session_id").ok_or(StatusCode::BAD_REQUEST)?;
    let path = params.get("path").ok_or(StatusCode::BAD_REQUEST)?;
    // 查该会话的 cwd（从最近一次 sessions 快照中找）
    // 从最近一次会话快照中找该会话的 project_path（cwd）——不另立函数，直查
    let resp = crate::adapter::get_all_sessions();
    let cwd = resp.sessions.iter().find(|s| s.id == *sid)
        .map(|s| s.project_path.clone())
        .ok_or(StatusCode::NOT_FOUND)?;
    match crate::remote::files::read_file_safe(&cwd, path) {
        Ok((bytes, mime)) if mime.starts_with("image/") => {
            Ok(([(axum::http::header::CONTENT_TYPE, mime)], bytes).into_response())
        }
        Ok((bytes, mime)) => {
            let text = String::from_utf8_lossy(&bytes);
            Ok(Json(serde_json::json!({ "content": text, "mime": mime })).into_response())
        }
        Err(e) => Err(StatusCode::FORBIDDEN),
    }
}
```

- [ ] **Step 3: SessionDetail.tsx（ZCode 式对话视图）**

```tsx
// P9 ZCode 式会话详情：运行中展示明细（思考折叠可展开/工具可展开），
// 完成后自动折叠只显示最终总结；文件链接可点开预览
export function SessionDetail({ sessionId, agentType, onBack }: Props) {
  const [messages, setMessages] = useState<SessionMessage[]>([]);
  const [expanded, setExpanded] = useState<Set<number>>(new Set());
  const [filePreview, setFilePreview] = useState<{path: string; mode: "split"|"fullscreen"} | null>(null);
  // ... 拉取 /m/api/v1/session-messages → 渲染消息列表
  // kind=="thinking" → 默认 collapsed，点击展开
  // kind=="tool-call" → 默认 collapsed，点击展开（显示工具名+参数摘要）
  // kind=="assistant" 且非最后一条 → 直接显示
  // 会话状态==Idle/Finished → 过程消息自动折叠，只显示最后的 assistant 消息
  // 消息中的文件路径 → 可点击链接 → setFilePreview
}
```

- [ ] **Step 4: FilePreview.tsx（分屏/全屏浮窗）**

```tsx
// 文件预览：左侧对话+右侧文件分屏，或全屏浮窗
export function FilePreview({ filePath, sessionId, mode, onClose }: Props) {
  const [content, setContent] = useState<string>("");
  const [mime, setMime] = useState<string>("");
  // 拉取 /m/api/v1/file?session_id=&path=
  // mime==image/* → <img src={url} />
  // mime==text/markdown → <ReactMarkdown> 渲染
  // 其他文本 → <pre><code> 带语法高亮
}
```

- [ ] **Step 5: App.tsx 路由 + 构建验证**
- [ ] **Step 6: Commit** `feat(m3-content): ZCode 式会话详情+ZCode 文件预览`

---

### Task 9: 其余 7 工具文件预览（边探测边实现）

**Files:**
- Modify: `src-tauri/src/remote/files.rs`（追加 7 个提取器）
- Modify: `src-tauri/src/remote/content.rs`（确认消息中的文件路径链接化）

**Interfaces:** 同 Task 8 的 `extract_file_paths`，按工具逐个实现。

- [ ] **Step 1: 按"格式已知度"排序实现**

实现顺序（从最成熟到最需探测）：
1. **Claude Code**：JSONL 中 `tool_use` 块的 `input.file_path` 字段（Read/Write/Edit 工具）
2. **Codex**：rollout JSONL 中工具调用的参数
3. **dsh**：事件流 `tool/call` 的 `toolArguments` 中的路径
4. **Kimi**：wire.jsonl 中同上
5. **OpenCode**：SQLite message content 中 tool 相关字段
6. **WorkBuddy**：JSONL 中同 Claude 模式
7. **OpenClaw**：state.json / agent sqlite

每个提取器一个测试用例（黄金夹具或合成数据），格式不确定的**先探测再实现**（造一次性 /tmp 会话验证格式）。

- [ ] **Step 2: 逐个实现+测试** → `cargo test remote::files`
- [ ] **Step 3: Commit** `feat(m3-content): 七工具文件路径提取（边探测边实现）`

---

### Task 10: M3 验收

- [ ] **Step 1: 全量门禁** → `cargo test && cargo clippy --all-targets -- -D warnings && pnpm check && pnpm test && pnpm build:mobile`
- [ ] **Step 2: 手动验收清单**（需用户+手机）

| 场景 | 验收 |
|---|---|
| P8a/P8b | 页头显示 MAM+版本号+本机名 |
| P7 修正 | 0.0.0.0 绑定时显示局域网 IP 而非 0.0.0.0 |
| P8c | 卡片=工具名+项目名+右侧状态点/副行=标题+消息 |
| P8d/P8e | chips 仅受管工具、品牌色、多行折叠、活跃排序 |
| P8f | 日/夜皮肤切换持久化 |
| C1 实时 | 状态变化 2 秒内手机收到横幅+声音+振动 |
| C1 降级 | 断 SSE → 2 次失败 → 3s 轮询 |
| C2 消息 | 点开卡片看完整对话（ZCode 式交互） |
| C2 文件 | ZCode 会话中的文件可点开预览（markdown 渲染/图片显示/代码高亮） |
| 桌面回归 | 现有功能零变化 |

- [ ] **Step 3: Commit** `docs(m3): 验收记录`

---

## Self-Review 记录

- **Spec 覆盖**：P8+ 六条（Task 1-4）、P7 修正（Task 2）、A3 SessionWatcher（Task 5）、C1 SSE+降级（Task 6）、C2 消息 API（Task 7）、C2 前端 ZCode 式（Task 8）、文件预览 ZCode 先行（Task 8）、其余 7 工具（Task 9）——全覆盖。
- **占位符**：content.rs 和 files.rs 中有 `todo!()` 标注——这是**有意的执行时实现点**（八工具各自的路径推导参照现有 parser，无法在计划中预写完整代码），每个 todo 附带了参照指引（"参照 monitor/xxx_parser.rs"或"参照 M0 spike 判定 A"）。非 TBD 空洞。
- **类型一致性**：`TransitionEvent` 在 Task 5/6 一致；`SessionMessage` 在 Task 7/8 一致；`read_file_safe` 签名在 Task 8 内一致。
- **风险**：Task 7/8 的 `todo!()` 区域是最大执行不确定性——执行者需要读现有 parser 代码并做格式探测；计划已标注参照路径。
- **接口名对照修正（2026-09-15 二次核查，7 处）**：①`STATUS_COLORS`→`STATUS_DOT_COLOR`；②新增 `TOOL_LABELS` 导出；③`hostname::get()`→`sysinfo::System::host_name()`；④`find_session_cwd()` 改直查；⑤`Query` 导入提醒；⑥gate 覆盖说明；⑦P8d enabled 来源。
- **事后核查修订（2026-09-15）**：①补充依赖新增清单（tokio-stream/futures/react-markdown 等均不在当前项目中）；②明确 M2 未合并现状——M3 需等 M2 PR 合入 main 后切分支；③CI 同步检查项已在 Global Constraints 声明（M2 教训）。
