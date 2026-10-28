# 二期 M7–M9 · 终端注入主线 + 审批应答 + Windows 注入 · 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
> **执行环境**：Windows 电脑。macOS 专属执行层用 `cfg(target_os = "macos")` 门控（编译期剔除，Windows 不编译不运行）；**纯核（命令构造/归一/路由/队列判定/映射解析/INPUT_RECORD 构造）全部跨平台可测，Windows 上必须全绿**。macOS 实机项 → 登记回传清单（Task 9/14）。

**Goal:** 手机（已过 PIN 门禁设备）给终端里的 agent 会话发消息（打字+回车、`[mobile 设备名]` 前缀、多行 `\n` 归一、黄排队/可输入态 flush/插队/撤回）、红卡一键审批应答（Claude/Codex 按键映射）、Windows ConPTY 通道同口径注入；全链写审计。

**Architecture:** 新顶层模块 `src-tauri/src/inject/`（normalize/routing/engine/queue/approve 纯核优先 + cfg 分层执行器）；队列表与审计表入 SQLite（既有 schema/dao 模式）；写端点挂既有 `/m/api/v1` 子路由（PIN gate 结构性覆盖）；flush 由 SessionWatcher 跃迁事件驱动（订阅既有 broadcast）；移动端 SessionDetail 挂输入区与审批卡。

**Tech Stack:** Rust（axum/rusqlite/sysinfo/windows crate 0.57 已有，**零新增依赖**）+ React 19 mobile（纯 fetch，ApiError 模式）。

**Spec:** 宪法 `docs/MASTER-PLAN.md`（D1–D18）→ 二期总 spec `docs/engineering/specs/2026-09-18-phase2-message-injection-design.md` v1.4（W1–W6、裁决 1–17）→ 批次设计 `docs/engineering/specs/2026-09-18-phase2-m6-m9-injection-design.md` v2.1（T2/T3/T4）。**裁决不得重议。**

## Global Constraints（每任务隐含遵守）

1. 端点一律 `/m/api/v1/` 前缀，挂在 `server.rs::api_router` 内（gate 内层结构性覆盖，未知路径 403）；响应带 `Cache-Control: no-store`（与 sessions/file 同规）。
2. 数据同源铁律：会话查找只经 `(st.session_source)()`（生产 = adapter::get_all_sessions），**禁止**另写查找/复制聚合逻辑，禁止修改 adapter。
3. 测试零接触真实 `~/.mam` / 网络：dao 用 `Connection::open_in_memory` + `schema::init`；端点测试注入假 session_source 与假 Injector（RemoteState 注入缝模式，见 server.rs:236）。
4. `store.with` 持 DB 锁不可重入：新代码**不得**在任何 `with`/`DB.lock()` 临界区内再调 `get_setting`/`set_setting`（M4 实锤死锁教训）。
5. 会话扫描预算契约不受影响：本计划不新增扫描路径；inject 全部被动触发。
6. 平台分层：`#[cfg(target_os = "macos")]` / `#[cfg(windows)]` 只包执行层；纯核无 cfg。macOS 单测 `#[cfg(all(test, target_os = "macos"))]`（Windows 跳过不编译）。Windows 上既有 `cfg(unix)` 用例照旧。
7. 移动端文案中文内联（SessionDetail/PairPage 既有惯例，不走 i18n）；桌面设置新 UI 走 i18n zh/en 成对（check:i18n 卡关）。
8. 每任务六门禁全绿才算完：`cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` / `cargo test` / `pnpm check` / `pnpm test` / `pnpm build:mobile`。
9. 红线：不 push、不建 PR、不动 main、不改宪法与三份设计文档本体（冲突停下上台账）；裁决 1–17 不重议。

## 关键复用点（已实证，直接用）

- **tty 解析**：`window::get_tty_for_pid(pid)`（`ps -p pid -o tty=`，macOS）——现为私有，Task 4 改 `pub(crate)`。
- **tmux pane 定位**：`tmux list-panes -a -F "#{pane_tty} #{session_name}:#{window_index}.#{pane_index}"`（window/tmux.rs 既有输出格式）。
- **AppleScript 执行**：`window/applescript.rs::execute_applescript`（22 行 osascript 包装，macOS）——Task 4 改 `pub(crate)`。
- **设备身份**：`remote::gate::extract_device(&HeaderMap) -> Option<String>`（cookie → 设备 id）；设备名查 `pairing::device_valid` 同表（`remote_devices` 命令的 SELECT 形态：`SELECT name FROM remote_devices WHERE id=?`）。
- **跃迁事件**：`st.watcher_tx.subscribe()`，`TransitionEvent{session_id, from, to, ts}`（camelCase wire）。
- **状态→红黄绿**：红=Waiting；黄=Processing/Thinking/Compacting；绿=Idle/Finished（session/model.rs 六态）。
- **Windows 祖先链**：`window::win32::collect_ancestor_pids`（近→远 PID 序列）——Task 15 改 `pub(crate)`。
- **windows crate 0.57** 已在 `[target.'cfg(windows)'.dependencies]` 且含 `Win32_System_Console`（Cargo.toml:86-90）——M9 零新增依赖。

## 跨任务接口契约（先读这里，再读你的任务）

```rust
// inject/normalize.rs（Task 1）
pub fn normalize_newlines(text: &str) -> String;                  // '\n' → "\\n"（字面两字符）
pub fn compose_injection(device_name: &str, text: &str) -> String; // "[mobile {name}] {归一正文}"
pub fn summarize(text: &str, max_chars: usize) -> String;
pub const AUDIT_SUMMARY_CHARS: usize = 80;   // 审计摘要截断长度（W5 只存摘要）

// inject/routing.rs（Task 2）
pub enum Channel { Tmux, Iterm2, TerminalApp, WindowsConsole }
pub enum Visibility { Realtime, AfterRefresh }
pub enum RouteOutcome {
    Injectable { candidates: Vec<Channel>, visibility: Visibility },
    NotInjectable { reason_code: &'static str, reason: String },
} // reason_code ∈ "app_form"|"blackbox"|"headless_only"|"no_process"|"platform"
pub fn route(agent_tool_id: &str, form: ProcessForm, pid: u32, platform: &str) -> RouteOutcome;

// inject/engine.rs（Task 4/15）
pub trait Injector: Send + Sync {
    fn name(&self) -> &'static str;
    /// 定位（pid→tty/pane/console）并注入一行文本 + 回车；失败返回中文错误（进回执/审计）
    fn locate_and_inject(&self, pid: u32, text: &str) -> Result<(), String>;
    /// 注入单键（审批应答用；key ∈ "1".."9"|"y"|"n"|"a".."z"|"enter"|"esc"）
    fn locate_and_send_key(&self, pid: u32, key: &str) -> Result<(), String>;
}
pub struct RealInjector;   // 生产装配：macOS 三通道链 / Windows ConPTY（cfg 内部分派）
// 纯函数（跨平台可测）：tmux_send_args / tmux_enter_args / tmux_key_args /
// applescript_escape / iterm_write_script / terminal_do_script / iterm_send_key_script /
// key_to_windows_vk / text_to_key_records（Task 4 与 Task 14 各自定义）

// database/dao/inject_queue.rs（Task 3）
pub struct QueueRow { pub id: i64, pub session_id: String, pub agent_type: String,
    pub device_id: String, pub device_name: String, pub content: String,
    pub enqueued_at: i64, pub sent_at: Option<i64>, pub failed_reason: Option<String> }
pub fn enqueue_conn(conn,&…) -> i64;  pub fn pending_for_session_conn(conn, sid) -> Vec<QueueRow>;
pub fn next_pending_conn(conn, sid) -> Option<QueueRow>;  pub fn mark_sent_conn(conn, id, now);
pub fn mark_failed_conn(conn, id, reason);  pub fn retract_conn(conn, sid, id) -> bool;

// database/dao/write_audit.rs（Task 3）
pub struct AuditRow { pub ts: i64, pub device_name: String, pub agent_type: String,
    pub session_id: String, pub channel: String, pub action: String,
    pub summary: String, pub result: String }
pub fn record_conn(conn, ts, device_id, device_name, agent_type, session_id, channel, action, summary, result);
pub fn recent_conn(conn, limit) -> Vec<AuditRow>;   // action ∈ send|queue|flush|jump|retract|approve|reject|fail
// 审计写入口统一走 inject::audit_write 辅助（Task 6 提供）：DB 落库 + events::audit
// 日志留痕并行（M4 T2e 惯例：target remote_audit 单行结构化可 grep）

// inject/queue.rs（Task 5）
pub fn is_running(status: &SessionStatus) -> bool;       // Processing|Thinking|Compacting
pub fn is_input_ready(status: &SessionStatus) -> bool;   // Waiting|Idle|Finished
/// 单次投递内核（循环与端点共用）：jump=true 越过「仍在运行」复核（裁决 12 插队语义），
/// 但仍要求快照中会话存在（红·中断挂起，W2）。成功/失败都写审计（DB + events::audit 日志并行）
pub fn flush_one(st: &server::RemoteState, session_id: &str, jump: bool) -> bool;
pub fn spawn_flush_loop(state: Arc<server::RemoteState>); // serve() 内挂载（Task 6 接线）

// inject/approve.rs（Task 10）
/// 首批只做批准/拒绝（spec W6 定死）；「不要再问」等扩展键位待 Task 13 实测取证后回填追加
pub struct ApproveOption { pub id: String, pub label: String, pub key: String }
pub struct ToolMapping { pub tool: String, pub verified_with: String,
    pub prompt_markers: Vec<String>, pub options: Vec<ApproveOption> }
pub fn load_mappings() -> Vec<ToolMapping>;            // KV "inject.approve_map" JSON，缺省用默认表
pub fn detect(m: &ToolMapping, last_message: &str) -> bool;  // marker 小写 contains
pub fn option_by_id<'a>(m: &'a ToolMapping, id: &str) -> Option<&'a ApproveOption>;
pub fn is_version_drift(verified_with: &str, current: &str) -> bool; // "probe-pending" 恒 true
pub fn cached_cli_version(cli: &str) -> Option<String>;  // 进程级缓存 spawn `<cli> --version`

// RemoteState 新增缝（Task 6，server.rs struct + mod.rs STATE 装配）
pub injector: std::sync::Arc<dyn inject::engine::Injector>,
```

---

### Task 1: inject 模块骨架 + 归一纯核

**Files:**
- Create: `src-tauri/src/inject/mod.rs`（模块声明 + use 接线进 `lib.rs` 的 `mod inject;`）
- Create: `src-tauri/src/inject/normalize.rs`
- Modify: `src-tauri/src/lib.rs`（顶层 `mod inject;`，位置挨着 `mod window;`）

**Interfaces:** Produces `normalize_newlines` / `compose_injection` / `summarize`（契约见上）。

- [ ] **Step 1: 写失败测试**（`normalize.rs` 尾部）

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// 裁决 6：换行符 → 字面 \n 两字符；其余原样（含已有的字面 \n 不动）
    #[test]
    fn newlines_become_literal_backslash_n() {
        assert_eq!(normalize_newlines("第一行\n第二行"), "第一行\\n第二行");
        assert_eq!(normalize_newlines("已是字面\\n"), "已是字面\\n");
        assert_eq!(normalize_newlines("无换行"), "无换行");
        assert_eq!(normalize_newlines("a\r\nb"), "a\\r\\nb"); // \r\n 整体归一，杜绝裸 \r
    }

    /// W1：[mobile 设备名] 前缀 + 归一正文
    #[test]
    fn compose_prefixes_and_normalizes() {
        assert_eq!(compose_injection("iPhone", "改一下\n继续"), "[mobile iPhone] 改一下\\n继续");
    }

    /// 审计摘要：超长截断加省略号
    #[test]
    fn summarize_truncates() {
        assert_eq!(summarize("abcdef", 4), "abcd…");
        assert_eq!(summarize("abc", 4), "abc");
    }
}
```

- [ ] **Step 2: 跑测确认失败**：`cd src-tauri && cargo test normalize`（编译失败 = 函数不存在）
- [ ] **Step 3: 最小实现**

```rust
//! 注入消息归一（裁决 6 定死）：手机多行输入 → 字面 `\n` 拼接单行——终端侧永远是
//! 「打字 + 一次回车」，避开 TUI 多次提交与 paste-buffer 粘贴确认两个版本敏感雷区；
//! 桌面不换行展示为已登记的已知限制（二期 spec 附录 C #6）。

/// 换行符（\n 与 \r\n）→ 字面 `\n` 两字符，其余原样
pub fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\\n").replace('\n', "\\n").replace('\r', "\\n")
}

/// 组装最终注入文本：`[mobile <设备花名>] <归一正文>`（W1 来源标记，桌面一眼可辨）
pub fn compose_injection(device_name: &str, text: &str) -> String {
    format!("[mobile {}] {}", device_name, normalize_newlines(text))
}

/// 审计摘要（W5：只存摘要不入全文，防审计库膨胀）
pub fn summarize(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let cut: String = text.chars().take(max_chars).collect();
        format!("{cut}…")
    }
}
```

`mod.rs` 初始内容：`pub mod normalize;`（后续任务逐个追加模块声明）。
- [ ] **Step 4: 跑测通过** + `cargo fmt` + 全量 `cargo test`
- [ ] **Step 5: Commit** `feat(m7-inject): 注入模块骨架 + 多行 \n 归一纯核（裁决 6）`

### Task 2: 路由表纯核（W3）

**Files:** Create `src-tauri/src/inject/routing.rs`；Modify `inject/mod.rs` 追加 `pub mod routing;`

**Interfaces:** Produces `Channel/Visibility/RouteOutcome/route`（契约见上）。消费 `session::model::ProcessForm`。

- [ ] **Step 1: 失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn cli() -> ProcessForm { ProcessForm::Cli }
    fn app() -> ProcessForm { ProcessForm::App }

    /// APP 形态不可注入（WorkBuddy/Codex APP 等——computer-use 唯一通路属 D7 应急级）
    #[test]
    fn app_form_not_injectable() {
        let r = route("claude", app(), 100, "macos");
        assert!(matches!(r, RouteOutcome::NotInjectable { reason_code: "app_form", .. }));
    }

    /// WorkBuddy 黑盒 / dsh 另评 / ZCode 走无头（M11）/ OpenClaw gateway 另评
    #[test]
    fn tool_gates() {
        for (tool, code) in [("workbuddy", "blackbox"), ("dsh", "blackbox"), ("zcode", "headless_only"), ("openclaw", "blackbox")] {
            let r = route(tool, cli(), 100, "macos");
            assert!(matches!(r, RouteOutcome::NotInjectable { reason_code, .. } if reason_code == code), "{tool}");
        }
    }

    /// pid=0（未读卡兜底/进程不在）不可注入
    #[test]
    fn dead_process_not_injectable() {
        assert!(matches!(route("claude", cli(), 0, "macos"),
            RouteOutcome::NotInjectable { reason_code: "no_process", .. }));
    }

    /// macOS 可注入家（claude/codex/opencode/kimi CLI）：三通道候选按由简到繁（裁决 14）
    #[test]
    fn macos_candidates_ordered() {
        let r = route("kimi", cli(), 42, "macos");
        let RouteOutcome::Injectable { candidates, visibility } = r else { panic!() };
        assert_eq!(candidates, vec![Channel::Tmux, Channel::Iterm2, Channel::TerminalApp]);
        assert!(matches!(visibility, Visibility::Realtime));
    }

    /// Windows：单通道候选（宿主可达性由 M6 结论与引擎层处理）
    #[test]
    fn windows_single_channel() {
        assert!(matches!(route("claude", cli(), 42, "windows"),
            RouteOutcome::Injectable { candidates, .. } if candidates == vec![Channel::WindowsConsole]));
    }

    /// 其他平台不支持
    #[test]
    fn linux_not_supported() {
        assert!(matches!(route("claude", cli(), 42, "linux"),
            RouteOutcome::NotInjectable { reason_code: "platform", .. }));
    }
}
```

- [ ] **Step 2: 确认失败** → **Step 3: 实现**

```rust
//! 注入路由表（W3，纯核）：会话属性 → 通道决策 + 可见性预期。路由按宿主形态与
//! 工具门判定，**工具无关的终端宿主一律优先终端注入**（无头对已开 TUI 会分叉，
//! spec 裁决「路由规则按宿主形态，工具无关」）。M11 无头落地后此处仅追加分支。

use crate::session::model::ProcessForm;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel { Tmux, Iterm2, TerminalApp, WindowsConsole }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility { Realtime, AfterRefresh }

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteOutcome {
    Injectable { candidates: Vec<Channel>, visibility: Visibility },
    NotInjectable { reason_code: &'static str, reason: String },
}

/// 黑盒/另评工具（宪法附 A 矩阵）：WorkBuddy ❌、dsh ◐ 另评、OpenClaw ◐ gateway、
/// ZCode 固定走无头（D6 在册限定，M11）
fn tool_gate(tool: &str) -> Option<(&'static str, String)> {
    match tool {
        "workbuddy" => Some(("blackbox", "WorkBuddy 黑盒，无外部写 API".into())),
        "dsh" => Some(("blackbox", "dsh 写通道另评（web API/插件生态）".into())),
        "openclaw" => Some(("blackbox", "OpenClaw 走 gateway，另评".into())),
        "zcode" => Some(("headless_only", "ZCode 走无头通道（M11）".into())),
        _ => None,
    }
}

pub fn route(agent_tool_id: &str, form: ProcessForm, pid: u32, platform: &str) -> RouteOutcome {
    if pid == 0 {
        return RouteOutcome::NotInjectable { reason_code: "no_process", reason: "会话进程不存在".into() };
    }
    if let Some((code, reason)) = tool_gate(agent_tool_id) {
        return RouteOutcome::NotInjectable { reason_code: code, reason };
    }
    if form == ProcessForm::App {
        return RouteOutcome::NotInjectable { reason_code: "app_form", reason: "APP 形态无外部写 API（computer-use 属应急预案 D7）".into() };
    }
    match platform {
        "macos" => RouteOutcome::Injectable {
            candidates: vec![Channel::Tmux, Channel::Iterm2, Channel::TerminalApp],
            visibility: Visibility::Realtime,
        },
        "windows" => RouteOutcome::Injectable { candidates: vec![Channel::WindowsConsole], visibility: Visibility::Realtime },
        _ => RouteOutcome::NotInjectable { reason_code: "platform", reason: "当前平台不支持注入".into() },
    }
}
```

- [ ] **Step 4: 过测 + fmt** → **Step 5: Commit** `feat(m7-inject): 路由表纯核（工具门 + 宿主形态优先 + 平台候选，W3）`

### Task 3: 队列表与审计表（schema + dao）

**Files:** Modify `src-tauri/src/database/schema.rs`（execute_batch 末尾追加两表）；Create `src-tauri/src/database/dao/inject_queue.rs`、`src-tauri/src/database/dao/write_audit.rs`；Modify `src-tauri/src/database/dao/mod.rs`（声明两模块）

**Interfaces:** Produces `QueueRow`/`AuditRow` 与全部 `*_conn` 函数（契约见上）。消费 `DB.lock()` 全局连接（照抄 dao/settings.rs 形态：对外 `pub fn` 包装 + `*_conn` 内存库可测）。

- [ ] **Step 1: schema 追加**（`execute_batch` 的 r#" 字符串末尾，与 presets 表同级缩进）

```sql
CREATE TABLE IF NOT EXISTS inject_queue (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  session_id    TEXT NOT NULL,
  agent_type    TEXT NOT NULL,
  device_id     TEXT NOT NULL,
  device_name   TEXT NOT NULL,
  content       TEXT NOT NULL,
  jumped        INTEGER NOT NULL DEFAULT 0,
  enqueued_at   INTEGER NOT NULL,
  sent_at       INTEGER,
  failed_reason TEXT
);
CREATE INDEX IF NOT EXISTS idx_inject_queue_session ON inject_queue(session_id, id);
CREATE TABLE IF NOT EXISTS write_audit (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  ts          INTEGER NOT NULL,
  device_id   TEXT NOT NULL,
  device_name TEXT NOT NULL,
  agent_type  TEXT NOT NULL,
  session_id  TEXT NOT NULL,
  channel     TEXT NOT NULL,
  action      TEXT NOT NULL,
  summary     TEXT NOT NULL,
  result      TEXT NOT NULL
);
```

- [ ] **Step 2: dao 失败测试**（两文件各自 `#[cfg(test)] mod tests`，`fn mem()` 照抄 dao/settings.rs：`Connection::open_in_memory` + `schema::init`）

```rust
// inject_queue.rs tests（要点）
#[test] fn enqueue_returns_id_and_pending_lists_in_order() {
    let c = mem();
    let a = enqueue_conn(&c, "s1", "claude", "d1", "iPhone", "msg-a", 1000);
    let b = enqueue_conn(&c, "s1", "claude", "d1", "iPhone", "msg-b", 1001);
    assert!(b > a);
    let pending = pending_for_session_conn(&c, "s1");
    assert_eq!(pending.len(), 2);
    assert_eq!(pending[0].content, "msg-a"); // FIFO
    assert_eq!(next_pending_conn(&c, "s1").unwrap().id, a);
}
#[test] fn mark_sent_removes_from_pending() {
    /* enqueue → mark_sent_conn(id, 2000) → pending 空；行保留（审计可查 sent_at=Some(2000)) */
}
#[test] fn mark_failed_records_reason() { /* mark_failed_conn → pending 空，failed_reason 落库 */ }
#[test] fn retract_only_own_session() { /* retract_conn(c,"s1",对 id) true；跨会话 id false */ }
```

```rust
// write_audit.rs tests（要点）
#[test] fn record_and_recent_desc() {
    let c = mem();
    record_conn(&c, 1000, "d1", "iPhone", "claude", "s1", "tmux", "send", "[mobile iPhone] hi", "ok");
    record_conn(&c, 1001, "d1", "iPhone", "codex", "s2", "tmux", "queue", "…", "ok");
    let rows = recent_conn(&c, 10);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].ts, 1001); // 最新在前
}
```

- [ ] **Step 3: 确认失败 → 实现**（形态照抄 settings.rs：`pub fn enqueue(...) { let conn = DB.lock().unwrap(); enqueue_conn(&conn, ...) }`；SQL 用 `params![]`；`pending_for_session_conn` 过滤 `sent_at IS NULL AND failed_reason IS NULL ORDER BY id`）
- [ ] **Step 4: 全测 + fmt + clippy** → **Step 5: Commit** `feat(m7-inject): 队列表 + 写审计表（schema/dao，内存库测试）`

### Task 4: 注入引擎——构造纯核 + macOS 执行层 + Injector 缝

**Files:** Create `src-tauri/src/inject/engine.rs`；Modify `inject/mod.rs`、`window/mod.rs`（`get_tty_for_pid` 与 `applescript.rs` 的 `execute_applescript` 改 `pub(crate)`，`mod applescript` 改 `pub(crate) mod`）

**Interfaces:** Produces `Injector` trait / `RealInjector` / 全部构造纯函数。**注意：`RealInjector` 的执行体 macOS/Windows 各自 cfg，Windows 编译期只有 trait + 纯函数可用——Task 14 才补 Windows 执行体（届时 `RealInjector` 的 windows 分支从「未接入占位」替换为真实现）。**

- [ ] **Step 1: 构造纯函数失败测试**（跨平台，engine.rs tests）

```rust
#[test] fn tmux_literal_and_enter() {
    // -l 字面量杜绝 ":开头被当键名"（参考实现铁则）
    assert_eq!(tmux_send_args("ses:0.1", "hi"), vec!["send-keys", "-t", "ses:0.1", "-l", "--", "hi"]);
    assert_eq!(tmux_enter_args("ses:0.1"), vec!["send-keys", "-t", "ses:0.1", "Enter"]);
}
#[test] fn tmux_key_args_literal_vs_named() {
    assert_eq!(tmux_key_args("t", "1"), vec!["send-keys", "-t", "t", "-l", "--", "1"]);
    assert_eq!(tmux_key_args("t", "esc"), vec!["send-keys", "-t", "t", "Escape"]);
    assert_eq!(tmux_key_args("t", "enter"), vec!["send-keys", "-t", "t", "Enter"]);
    assert_eq!(tmux_key_args("t", "bad!"), vec!["send-keys", "-t", "t", "-l", "--", "bad!"]); // 未知一律字面
}
#[test] fn applescript_escape_backslash_and_quotes() {
    assert_eq!(applescript_escape(r#"a"b\c"#), r#"a\"b\\c"#);
}
#[test] fn iterm_script_finds_tty_and_writes_without_newline() {
    let s = iterm_write_script("ttys005", r#"say "hi""#);
    assert!(s.contains(r#"tty of s contains "ttys005""#));
    assert!(s.contains(r#"write s text "say \"hi\"" newline NO"#)); // 先文本后回车两步（可校验）
    assert!(s.contains(r#"write s text """#)); // 补回车
}
#[test] fn iterm_send_key_script_dispatch() {
    let s = iterm_send_key_script("ttys005", "1");
    assert!(s.contains(r#"tty of s contains "ttys005""#));
    assert!(s.contains(r#"keystroke "1""#));
    assert!(iterm_send_key_script("ttys005", "esc").contains("key code 53")); // esc 键码
}
#[test] fn terminal_script_uses_do_script() {
    let s = terminal_do_script("ttys005", "hi");
    assert!(s.contains(r#"tty of w is "/dev/ttys005""#));
    assert!(s.contains(r#"do script "hi" in w"#));
}
```

- [ ] **Step 2: 确认失败 → Step 3: 实现纯函数 + trait + macOS 执行层**

```rust
//! 注入引擎：构造层（纯函数，跨平台可测）+ 执行层（cfg 分层）。
//! 语义铁则（参考实现《终端注入通道-参考实现》）：tmux 必须 -l 字面量；iTerm2
//! `write text` 先文本（newline NO）后回车两步；Terminal.app `do script`（自带回车，
//! 注意转义）；一律「打字+回车」，不用 paste-buffer。
pub trait Injector: Send + Sync {
    fn name(&self) -> &'static str { "real" }
    fn locate_and_inject(&self, pid: u32, text: &str) -> Result<(), String>;
    fn locate_and_send_key(&self, pid: u32, key: &str) -> Result<(), String>;
}

pub struct RealInjector;

pub fn tmux_send_args(target: &str, text: &str) -> Vec<String> { /* 测试即规格 */ }
pub fn tmux_enter_args(target: &str) -> Vec<String> { /* … */ }
/// 单键：数字/字母字面 -l；enter/esc 走键名（无 -l）
pub fn tmux_key_args(target: &str, key: &str) -> Vec<String> {
    match key {
        "enter" => vec!["send-keys","-t",target,"Enter"].map(String::from),
        "esc"   => vec!["send-keys","-t",target,"Escape"].map(String::from),
        _       => vec!["send-keys","-t",target,"-l","--",key].map(String::from),
    }
}
pub fn applescript_escape(text: &str) -> String { text.replace('\\', "\\\\").replace('"', "\\\"") }
pub fn iterm_write_script(tty_suffix: &str, escaped_text: &str) -> String { /* 测试即规格，format! 模板 */ }
pub fn iterm_send_key_script(tty_suffix: &str, key: &str) -> String {
    // keystroke "1" / key code 53（esc）/ keystroke return —— 按键分派
}
pub fn terminal_do_script(tty_suffix: &str, escaped_text: &str) -> String { /* … */ }

/// macOS 执行层：pid → tty（window::get_tty_for_pid）→ 三通道依次尝试：
/// tmux（list-panes 命中 pane target → send-keys 两连发）→ iTerm2（AS 定位+写入）
/// → Terminal.app（AS）。任一通道成功即返回；全败返回中文错误。
#[cfg(target_os = "macos")]
impl Injector for RealInjector {
    fn locate_and_inject(&self, pid: u32, text: &str) -> Result<(), String> {
        let tty = crate::window::get_tty_for_pid(pid).map_err(|e| format!("定位终端失败：{e}"))?;
        let suffix = tty.rsplit('/').next().unwrap_or(&tty).to_string();
        if let Some(target) = find_tmux_pane(&tty) {
            return run_tmux(&tmux_send_args(&target, text))
                .and_then(|_| run_tmux(&tmux_enter_args(&target)))
                .map_err(|e| format!("tmux 注入失败：{e}"));
        }
        let esc = applescript_escape(text);
        if crate::window::applescript::execute_applescript(&iterm_write_script(&suffix, &esc)).is_ok() {
            return Ok(());
        }
        crate::window::applescript::execute_applescript(&terminal_do_script(&suffix, &esc))
            .map_err(|e| format!("iTerm2/Terminal 注入失败：{e}"))
    }
    fn locate_and_send_key(&self, pid: u32, key: &str) -> Result<(), String> {
        /* 同链路：tmux_key_args / iterm_send_key_script / terminal 变体（do script 不适用于
           单键——Terminal.app 通道按键改 keystroke 前置 activate，登记回传清单实测复核） */
    }
}

#[cfg(target_os = "macos")]
fn find_tmux_pane(tty: &str) -> Option<String> {
    // Command::new("tmux").args(["list-panes","-a","-F","#{pane_tty} #{session_name}:#{window_index}.#{pane_index}"])
    // 输出解析对齐 window/tmux.rs（首列包含 tty → 第二列 target）
}
#[cfg(target_os = "macos")]
fn run_tmux(args: &[String]) -> Result<(), String> { /* Command::new("tmux").args(args) status.success 判定 */ }

/// Windows 执行体占位（Task 15 依 M6 结论替换；此前 Windows 上 routing 已给
/// WindowsConsole，故此占位不会被生产触达——Task 6 端点测试用 FakeInjector）
#[cfg(windows)]
impl Injector for RealInjector {
    fn locate_and_inject(&self, _pid: u32, _text: &str) -> Result<(), String> {
        Err("Windows 通道未接入（Task 15）".into())
    }
    fn locate_and_send_key(&self, _pid: u32, _key: &str) -> Result<(), String> {
        Err("Windows 通道未接入（Task 15）".into())
    }
}

/// 非 macos 非 windows 平台：编译兜底（routing 已拦，此处防御）
#[cfg(not(any(target_os = "macos", windows)))]
impl Injector for RealInjector {
    fn locate_and_inject(&self, _pid: u32, _text: &str) -> Result<(), String> { Err("平台不支持".into()) }
    fn locate_and_send_key(&self, _pid: u32, _key: &str) -> Result<(), String> { Err("平台不支持".into()) }
}
```

- [ ] **Step 4: 全测（纯函数测试跨平台跑）+ fmt + clippy** → **Step 5: Commit** `feat(m7-inject): 注入引擎构造纯核 + macOS 三通道执行层（Injector 缝）`

### Task 5: 队列状态判定纯核 + flush 内核（watcher 驱动）

**Files:** Create `src-tauri/src/inject/queue.rs`；Modify `inject/mod.rs`（serve() 的循环接线放 Task 6 一并合入编译）

**Interfaces:** Produces `is_running`/`is_input_ready`/`flush_one(st, sid, jump)`/`spawn_flush_loop`。消费 Task 3 dao + Task 4 Injector + `st.watcher_tx` + `st.session_source` + `st.injector`（缝在 Task 6 加，届时一起过门禁）。

- [ ] **Step 1: 纯核失败测试**

```rust
#[test] fn status_classes() {
    use crate::session::model::SessionStatus::*;
    for s in [Processing, Thinking, Compacting] { assert!(is_running(&s)); assert!(!is_input_ready(&s)); }
    for s in [Waiting, Idle, Finished] { assert!(!is_running(&s)); assert!(is_input_ready(&s)); }
}
```

- [ ] **Step 2: 实现**

```rust
//! 状态门控队列（W2，裁决 12/可输入态口径）：黄入队；会话回到可输入态
//! （红·等待 / 绿·完成空闲——agent 把光标交回输入框的任何时刻）逐条 flush；
//! 一次一条，等下一可输入态 = 单会话串行。红·中断（快照中会话消失）不 flush，挂起明示。
pub fn is_running(status: &SessionStatus) -> bool {
    matches!(status, SessionStatus::Processing | SessionStatus::Thinking | SessionStatus::Compacting)
}
pub fn is_input_ready(status: &SessionStatus) -> bool {
    matches!(status, SessionStatus::Waiting | SessionStatus::Idle | SessionStatus::Finished)
}

/// flush 循环：订阅跃迁事件 → to 为可输入态的会话 → 取队首 → 快照复核（会话仍在且
/// 仍可输入；中断/消失则挂起不消费）→ 注入 → 审计 flush + mark_sent / mark_failed。
/// spawn 用 tauri::async_runtime（与 watcher 同裁决：任意线程可用）；DB/注入走
/// spawn_blocking。错误只记日志不 panic（下一跃迁自会重试队首）。
pub fn spawn_flush_loop(state: std::sync::Arc<crate::remote::server::RemoteState>) {
    let mut rx = state.watcher_tx.subscribe();
    tauri::async_runtime::spawn(async move {
        while let Ok(ev) = rx.recv().await {
            if !is_input_ready_str(&ev.to) { continue; }
            let st = state.clone();
            let _ = tokio::task::spawn_blocking(move || flush_one(&st, &ev.session_id)).await;
        }
    });
}

/// 单次 flush 内核（同步，循环与「立即发送」共用）。返回是否实际发出。
/// jump=false（常规跃迁驱动）：快照复核——会话找不到（红·中断/消失）→ 不消费挂起；
/// 找到但 is_running（竞态：又一轮开跑）→ 不消费，等下个事件。
/// jump=true（插队，裁决 12）：**跳过 is_running 复核**（用户显式要求即刻送达——运行中
/// TUI 把消息放进自身输入缓冲），仅保留「会话存在」复核。
pub fn flush_one(st: &crate::remote::server::RemoteState, session_id: &str, jump: bool) -> bool {
    // 1) next_pending_conn（DB.lock()，锁内只做 SQL——不放 get_setting）
    // 2) 快照复核：(st.session_source)() 找 session；找不到 → return false（挂起，W2 红·中断）
    //    !jump 且 is_running → return false；jump 则放行
    // 3) content 已在入队时 compose 完毕（Task 6），flush 直发
    // 4) st.injector.locate_and_inject(session.pid, &item.content)
    //    Ok → mark_sent + audit(channel=injector.name(), action=flush|jump, ok)
    //    Err(e) → mark_failed + audit(action=fail, result=failed:e)
    // 5) 两分支都并行 events::audit(action, "sid=… result=…") 日志留痕
}
```

- [ ] **Step 3: 循环接线的单测**：`flush_one` 用 Fake session_source + FakeInjector + 内存 DB 直接驱动（不走 broadcast）——断言：黄态会话**常规路径**不消费、**jump=true 照发**（插队语义回归锁）、Waiting 消费队首、快照无会话不消费（jump 也不发）、注入失败 mark_failed。`spawn_flush_loop` 本身不单测（异步循环，E2E 归 Mac 清单）。
- [ ] **Step 4: 全测 + 门禁** → **Step 5: Commit** `feat(m7-inject): 状态门控队列内核 + watcher 驱动 flush 循环（W2）`

### Task 6: RemoteState 注入缝 + session-send / send-info / queue 三端点

**Files:** Modify `src-tauri/src/remote/server.rs`（RemoteState 加 `pub injector: Arc<dyn Injector>` 字段 + api_router 注册 4 路由 + serve 挂 flush 循环 + 测试模块）；Modify `src-tauri/src/remote/mod.rs`（STATE 装配 `injector: std::sync::Arc::new(crate::inject::engine::RealInjector)`）；Create `src-tauri/src/remote/api.rs` 内新增 handler（同文件追加，模式照 session_messages）。

**Interfaces:** Produces 端点契约（Task 7 移动端消费）：

```
POST /m/api/v1/session-send        body {sessionId, text}          → 200 {"status":"delivered"} | 200 {"status":"queued","itemId":N,"position":M} | 200 {"status":"failed","error":"定位终端失败：…"}（直发注入失败——回执可重试，W4） | 400 {"error":"bad_request"} | 404 {"error":"no_session"} | 403 {"error":"not_injectable","reason":"…","reasonCode":"…"}
GET  /m/api/v1/session-send-info?session_id=                      → 200 {"injectable":bool,"reasonCode?","reason?","channels":["tmux",…],"visibility":"realtime"}
GET  /m/api/v1/session-queue?session_id=                          → 200 {"items":[{"id":N,"content":"…","enqueuedAt":ms,"position":M}]}
POST /m/api/v1/session-queue/jump  body {sessionId, itemId}       → 200 {"status":"delivered"} | 200 {"status":"failed","error"} | 404 {"error":"not_found"}
POST /m/api/v1/session-queue/retract body {sessionId, itemId}     → 200 {"ok":true} | 404 {"error":"not_found"}
```

- [ ] **Step 1: 端点失败测试**（server.rs tests，照既有 `req`/`body_string`/`test state` 模式；FakeInjector 记录调用）

```rust
struct FakeInjector { calls: std::sync::Mutex<Vec<(u32, String)>> }
impl inject::engine::Injector for FakeInjector {
    fn locate_and_inject(&self, pid: u32, text: &str) -> Result<(), String> {
        self.calls.lock().unwrap().push((pid, text.to_string())); Ok(())
    }
    fn locate_and_send_key(&self, _pid: u32, _key: &str) -> Result<(), String> { Ok(()) }
    fn name(&self) -> &'static str { "fake" }
}
// 测试 state：既有测试 RemoteState 构造处追加 injector: Arc::new(FakeInjector…)，
// session_source 注入包含 sess_a(claude,Cli,pid=11,Waiting)/sess_b(claude,Cli,pid=12,Processing)/
// sess_c(workbuddy,Cli,pid=13,Idle)/sess_d(zcode,Cli,pid=14,Waiting) 四个夹具。

#[tokio::test] async fn send_delivers_when_input_ready() {
    // POST session-send {sessionId:"sess_a", text:"你好\n继续"}
    // → 200 delivered；FakeInjector 收到 (11, "[mobile 测试设备] 你好\\n继续")
    // → 审计 recent 首条 action=send result=ok channel=fake；队列无残留
}
#[tokio::test] async fn send_queues_when_running() {
    // sess_b（Processing）→ 200 {"status":"queued","itemId":N,"position":1}
    // → 审计 action=queue；GET session-queue 可见该项
}
#[tokio::test] async fn send_rejects_not_injectable_and_missing() {
    // sess_c → 403 not_injectable reasonCode=blackbox；sess_d → 403 reasonCode=headless_only
    // 不存在的 id → 404 no_session；空 text / >10000 字符 → 400
}
#[tokio::test] async fn queue_jump_and_retract() {
    // 入队两条（sess_b Processing）→ jump 第二条 → delivered（**插队语义：黄态照发**，
    // FakeInjector 收到该条 content）且审计 action=jump
    // → retract 第一条 → ok:true；session-queue 空
}
#[tokio::test] async fn send_reports_inject_failure() {
    // FakeInjector 返回 Err("定位终端失败：…") → sess_a 直发 → 200 {"status":"failed","error":…}
    // → 审计 action=send result=failed:…；队列无残留（W1：定位失败不入队重试）
}
#[tokio::test] async fn send_info_matrix() {
    // sess_a → injectable=true channels=["tmux","iterm2","terminal_app"] visibility="realtime"
    // sess_c → injectable=false reasonCode="blackbox"
}
```

- [ ] **Step 2: 确认失败 → Step 3: 实现**（要点）：
  - `session_send(State, headers: HeaderMap, Json<SessionSendReq>)`（签名照 pair_pin 先例：State + HeaderMap + Json）：缺参/空/超长 → 400；`extract_device(headers)` 无设备理论不可达（gate 已拦）防御 403；设备名经 **`pairing::device_name(conn, id)`（本任务在 pairing.rs 新增小助手：`SELECT name FROM remote_devices WHERE id=?`，形态对齐 device_valid/touch_device，锁内只 SQL）**；session 查找走 `spawn_blocking((st.session_source)())`；route → NotInjectable 403；compose_injection → is_input_ready ? 直发（flush_one(…, jump=false) 投递内核，action=send；Err → 200 failed 回执）: 入队（content 存 compose 产物）+ 审计 queue。
  - `MAX_SEND_CHARS: usize = 10_000` 常量；所有 Json 响应带 `[(CACHE_CONTROL, "no-store")]`。
  - 审计统一经 `inject::audit_write(...)`：write_audit::record（DB）+ `remote::events::audit(action, detail)`（日志留痕并行，M4 T2e 惯例）。
  - 路由注册在 `/file` 之后：`.route("/session-send", post(api::session_send))` 等 4 条；`serve()` 内 `SessionWatcher::start()` 之后追加 `inject::queue::spawn_flush_loop(state.clone());`（Task 5 的循环在此接线合入）。
- [ ] **Step 4: 全门禁** → **Step 5: Commit** `feat(m7-inject): session-send/send-info/queue 三端点（PIN 门禁内 + 审计全覆盖 + 假注入器端点测试）`

### Task 7: 移动端发送 UI（W4）

**Files:** Modify `src/mobile/api.ts`（+5 函数：fetchSendInfo/sessionSend/fetchQueue/queueJump/queueRetract）；Create `src/mobile/MessageComposer.tsx`；Modify `src/mobile/SessionDetail.tsx`（非 preview 分支 messageArea 之后挂 `<MessageComposer session={session} />`）；测试 `tests/mobile/MessageComposer.test.tsx`（审批卡 ApproveCard 属 Task 12，本任务不建）

**Interfaces:** api.ts 新函数（fetch + ApiError 模式，照 pairWithPin 注释风格）：

```ts
export interface SendInfo { injectable: boolean; reasonCode?: string; reason?: string;
  channels: string[]; visibility: "realtime" | "after_refresh" }
export async function fetchSendInfo(sessionId: string): Promise<SendInfo | null>;   // 403 → null
export type SendResult = { status: "delivered" } | { status: "queued"; itemId: number; position: number } | { status: "failed"; error: string };
export async function sessionSend(sessionId: string, text: string): Promise<SendResult>;
export interface QueueItemView { id: number; content: string; enqueuedAt: number; position: number }
export async function fetchQueue(sessionId: string): Promise<QueueItemView[]>;
export async function queueJump(sessionId: string, itemId: number): Promise<{ status: string }>;
export async function queueRetract(sessionId: string, itemId: number): Promise<{ ok: boolean }>;
```

- [ ] **Step 1: 组件失败测试**（照 SessionDetail.test.tsx 的 fetchMock 模式）

```tsx
// 要点用例（中文描述即断言）：
// 1. injectable=true：textarea 输入多行（fireEvent.change "第一行\n第二行"）→ 点发送 →
//    fetch body 为 {sessionId, text}（多行原样上行，归一在服务端）→ 回执「已送达终端」chip 出现
// 2. injectable=false：输入区禁用 + 显示 reason（如「WorkBuddy 黑盒…」）+ data-testid="send-disabled-reason"
// 3. 排队态：send 返回 queued → chip「排队中 第1位」+ [立即发送][撤回] 按钮 →
//    点撤回 → fetchQueue 刷新为空
// 4. 回车不触发发送（textarea 天然换行）；发送按钮 disabled 当 text.trim() 为空
// 5. 发送失败：status="failed"（注入失败回执，如「定位终端失败：…」）→ 失败 chip + 文案
//    + 可重试（重按发送）；网络层 ApiError（403 等）→ 错误文案 + 可重试
```

- [ ] **Step 2: 确认失败 → Step 3: 实现**：MessageComposer（props: session）内部自带 sendInfo 拉取（mount + 3s 轮询仅当有排队项）、发送/插队/撤回、回执 chip 三态（delivered/queued N/failed）；中文文案内联（「已送达终端」「排队中 第 N 位」「立即发送」「撤回」「发送」）；样式对齐 SessionDetail 既有 Tailwind 风格（border-t、px-3、py-2、rounded 按钮）。
- [ ] **Step 4: `npx vitest run tests/mobile/MessageComposer.test.tsx` 过 + pnpm check（tsc）+ 全门禁**
- [ ] **Step 5: Commit** `feat(m7-inject): 移动端会话输入区（多行/发送/排队指示/插队/撤回/禁用态，W4）`

### Task 8: 写审计桌面查看入口（W5）

**Files:** Modify `src-tauri/src/inject/mod.rs`（`#[tauri::command] pub fn inject_list_audit(limit: Option<usize>) -> serde_json::Value`，`DB.lock()` + `write_audit::recent`，默认 100）；Modify `src-tauri/src/lib.rs` invoke_handler 注册；Create `src/components/settings/AuditLogSection.tsx`；Modify 设置页装配处（RemoteSection 同级）；i18n zh/en 各 +6 键（`audit.title/refresh/empty/time/device/session/action/summary`）；`src/tauri-mock.ts` +1 case（返回固定三条）；测试 `tests/settings/AuditLogSection.test.tsx`

- [ ] **Step 1: 失败测试**：渲染（invoke mock 返回三条）→ 列表出现设备名/动作/摘要；空 → 「暂无记录」；刷新按钮触发再 invoke。
- [ ] **Step 2/3/4: 实现 + 过测 + check:i18n（zh/en 成对）**
- [ ] **Step 5: Commit** `feat(m7-inject): 写审计设置页查看入口（W5，只增不改最近 100 条）`

### Task 9: M7 收口——全门禁 + macOS 实测回传清单 + 台账

**Files:** Create `docs/release-notes/m7-m8-macos-live-checklist.md`；台账 progress.md 更新

- [ ] **Step 1: 全六门禁**（数字记台账）。
- [ ] **Step 2: 写回传清单**（Mac 侧执行，逐条含前置/步骤/预期）：
  1. 三通道实测：tmux 会话 / iTerm2 会话 / Terminal.app 会话各开一个真实 claude，手机（或本机浏览器过 PIN）发消息 → 终端出现 `[mobile 设备名]` 行、agent 回复；2. 黄排队→静息 flush（发一个长任务再发消息，观察转绿/红后自动送达）；3. 插队实测（运行中「立即发送」，观察 Claude Code 输入缓冲行为）；4. 撤回实测；5. 锁屏注入（锁定 Mac 后发送，解锁验证送达）；6. 审计页逐条可查；7. WorkBuddy 卡输入框禁用态；8. 一期回归（看板/内容/SSE/通知抽查）。
- [ ] **Step 3: 台账记 M7 完成（代码/单测/门禁全绿，实机项待 Mac）** → **Step 4: Commit** `docs(m7): 收口——门禁数字 + macOS 实测回传清单`

### Task 10: 审批映射表 + 选项解析纯核（W6）

**Files:** Create `src-tauri/src/inject/approve.rs`；Modify `inject/mod.rs`；KV 键 `inject.approve_map`（经 dao::settings，**不在任何锁内调用**）

- [ ] **Step 1: 失败测试**（要点）

```rust
#[test] fn default_mappings_cover_claude_codex() {
    let ms = load_mappings();   // 无 KV 时用默认表
    // spec W6 定死：首批只做批准/拒绝（「不要再问」等扩展键位 Task 13 取证后回填追加）
    let claude = ms.iter().find(|m| m.tool == "claude").unwrap();
    assert_eq!(claude.options.len(), 2);
    assert!(ms.iter().any(|m| m.tool == "codex" && m.options.len() == 2));
}
#[test] fn detect_case_insensitive_marker() {
    let m = ToolMapping { tool: "claude".into(), verified_with: "probe-pending".into(),
        prompt_markers: vec!["do you want".into()], options: vec![] };
    assert!(detect(&m, "Do you want to proceed?")); assert!(!detect(&m, "无关文本"));
}
#[test] fn drift_rules() {
    assert!(is_version_drift("probe-pending", "1.2.3"));  // 未取证恒 drift → UI 提示复核
    assert!(!is_version_drift("1.2.3", "1.2.3"));
    assert!(is_version_drift("1.2.3", "2.0.0"));
    assert!(!is_version_drift("1.2.3", "1.2.4"));         // patch 漂移不告警（保守）
}
#[test] fn option_lookup() { /* option_by_id 命中/未命中 */ }
```

- [ ] **Step 2/3: 实现**——默认表（serde JSON 常量，**verified_with 一律 "probe-pending"，Task 14 实测取证后回填**）：

```rust
/// 默认映射（首批 Claude/Codex，裁决 14 由简到繁；选项键位待实测取证校正——
/// markers 取宽匹配词，Task 14 在临时会话触发真实审批后回填 verified_with 与键位）
const DEFAULT_MAPPINGS_JSON: &str = r#"[
 {"tool":"claude","verified_with":"probe-pending",
  "prompt_markers":["do you want","would you like","allow this","permission"],
  "options":[{"id":"approve","label":"允许","key":"1"},
             {"id":"reject","label":"拒绝","key":"esc"}]},
 {"tool":"codex","verified_with":"probe-pending",
  "prompt_markers":["approve","allow","run this command"],
  "options":[{"id":"approve","label":"允许","key":"y"},
             {"id":"reject","label":"拒绝","key":"n"}]}
]"#;
// 「不要再问」（claude 选项 2 等）不入首批表——Task 13 实测取证确认键位后追加
```

`load_mappings()`：读 KV，损坏/缺失回退默认表（不写回——保持默认表可升级）。`cached_cli_version`：`static CACHE: OnceLock<Mutex<HashMap<String,Option<String>>>>`，首次 spawn `<cli> --version` 取首个版本号 token，失败缓存 None（不重试刷屏）。
- [ ] **Step 4: 门禁** → **Step 5: Commit** `feat(m8-approve): 按键映射表 + 选项解析/版本漂移纯核（W6，默认表待实测取证）`

### Task 11: 审批端点（approve-options / session-approve）

**Files:** Modify `remote/api.rs`（+2 handler）、`server.rs`（+2 路由）、`remote/mod.rs`（STATE 无需新缝——复用 session_source/injector）

**Interfaces:**

```
GET  /m/api/v1/session-approve-options?session_id= → 200 {"available":bool,"options":[{"id","label"}],"verifiedWith":"…","currentVersion":"…|null","drift":bool}
     // available = status==Waiting && 命中 marker（数据同源快照判定）；映射缺失 → available=false
POST /m/api/v1/session-approve  body {sessionId, optionId} → 200 {"status":"key_sent"} | 404 no_session | 409 {"error":"not_waiting"} | 404 {"error":"no_mapping"}（降级提示走普通发送）
```

- [ ] **Step 1: 端点失败测试**（FakeInjector + 夹具 sess_a=Waiting 且 last_message 命中 "Do you want"；sess_e=Processing 命中；sess_d=zcode）：
  - options：sess_a → available=true options 3 项 drift=true（probe-pending）；sess_e → available=false；sess_d → available=false（工具无映射）
  - approve：sess_a optionId="approve" → 200 key_sent，FakeInjector 收到 (pid, "1")，审计 action=approve；optionId="reject" → 审计 action=reject；非法 optionId → 404 no_mapping；非 Waiting → 409
- [ ] **Step 2/3: 实现**（审批注入**不带 [mobile] 前缀**——按键非文本；投递走 `st.injector.locate_and_send_key(pid, key)`，映射经 routing 前置复核可注入；**预留无头分派缝**：投递处留注释位 `// M11：会话路由含无头通道时，此处按 approve option 分派 control_message 而非按键`——选项卡 UI 与端点契约通道无关，届时只扩这一处分派）
- [ ] **Step 4: 门禁** → **Step 5: Commit** `feat(m8-approve): 审批选项/应答端点（Waiting+marker 判定、按键注入、审计、降级路径）`

### Task 12: 移动端审批卡（红卡选项卡 UI）

**Files:** Create `src/mobile/ApproveCard.tsx`（props: session）；Modify `SessionDetail.tsx`（`session.status === "waiting"` 且非 preview 时在 messageArea 上方挂卡）；api.ts +2 函数（`fetchApproveOptions`/`sessionApprove`，契约同端点）；测试 `tests/mobile/ApproveCard.test.tsx`

- [ ] **Step 1: 失败测试**：available=true → 选项按钮（首批映射 = 允许/拒绝两项，W6 定死）渲染；点「允许」→ POST body 正确 → 卡片进入「已发送按键」态；available=false → 不渲染；drift=true → 显示「映射待实测确认，若提示不符请用普通发送」提示条；409/404 错误文案。
- [ ] **Step 2/3: 实现**（红卡视觉：红色边框卡 + 选项按钮横排；中文内联）。
- [ ] **Step 4: vitest + check** → **Step 5: Commit** `feat(m8-approve): 移动端红卡审批选项卡（含漂移提示与降级文案）`

### Task 13: M8 收口——提示形态实测取证 + 门禁 + 回传清单

- [ ] **Step 1: Windows 本机取证（可完成）**：临时项目目录开 claude 与 codex 真实会话，触发一次需批准的操作（如让 claude 运行 `git status` 外的命令），**抄录审批提示原文与可选键位**；据实修订 `DEFAULT_MAPPINGS_JSON` 的 prompt_markers/keys 并回填 `verified_with`（用 `claude --version`/`codex --version` 实测值）；补一条 marker 命中测试用例（用抄录原文）。⚠️ 键位「按下去真实生效」属终端注入，Windows 侧等 Task 15 后可本机验证，macOS 归回传清单。
- [ ] **Step 2: 全门禁** → **Step 3: 回传清单追加**：macOS 红卡一键批准/拒绝实机（claude/codex 各一）；计划模式「批准执行」实机（裁决 13）；降级路径人工核验。
- [ ] **Step 4: Commit** `feat(m8-approve): 实测取证回填映射表（verified_with）+ M8 收口`

### Task 14: Windows ConPTY 通道（M9，前置 = M6 结论）

**Files:** Create `src-tauri/src/inject/windows_console.rs`（`#[cfg(windows)]`）；Modify `inject/engine.rs`（纯函数 `key_to_windows_vk` + `text_to_key_records` 放跨平台区，`RealInjector` 的 windows impl 委托本模块）；Modify `window/win32.rs`（`collect_ancestor_pids` 改 `pub(crate)`）

**前置分支**（读探测报告结论，台账记录采用哪支）：
- **GO**：全量实现；**PARTIAL**：实现 + 宿主限制常量（如 `SUPPORTED_HOSTS: &[ConsoleHost]` 按报告收窄）；**NO-GO**：跳过本任务与 Task 15/16，改走 Task 16-alt 降级登记。

- [ ] **Step 1: 纯核失败测试**（跨平台）

```rust
#[test] fn vk_mapping() {
    assert_eq!(key_to_windows_vk("enter"), Some(0x0D));
    assert_eq!(key_to_windows_vk("esc"), Some(0x1B));
    assert_eq!(key_to_windows_vk("y"), Some(0x59));
    assert_eq!(key_to_windows_vk("1"), Some(0x31));
    assert_eq!(key_to_windows_vk("我们"), None); // 非单键 → 走文本通道
}
#[test] fn text_records_pair_down_up() {
    let recs = text_to_key_records("ab\n"); // 文本通道：每字符 keydown+keyup 对
    assert_eq!(recs.len(), 6);
    assert_eq!(recs[0].ch, 'a' as u16); assert!(recs[0].down); assert!(!recs[1].down);
}
/// 记录规格（纯层自有类型，不引 windows 类型）：
pub struct KeyRecordSpec { pub vk: u16, pub ch: u16, pub down: bool }
```

- [ ] **Step 2: 确认失败 → Step 3: 实现**（cfg(windows) 执行体，对齐 ConIn.ps1 已实证序列）：

```rust
//! AttachConsole 附加写入（M6 探测定案路径；序列对齐探测脚本 ConIn.ps1）：
//! FreeConsole → AttachConsole(target_pid) → GetStdHandle(STD_INPUT_HANDLE) →
//! WriteConsoleInput(文本记录) → WriteConsoleInput(回车)。目标 pid 策略（依 M6 报告）：
//! 优先会话 CLI 进程 pid；附加失败回退祖先链（win32::collect_ancestor_pids 近→远）
//! 中第一个共享目标控制台的宿主 pid。错误码 log::warn 留痕（进回执文案）。
#![cfg(windows)]
use windows::Win32::System::Console::{AttachConsole, FreeConsole, GetStdHandle,
    WriteConsoleInput, INPUT_RECORD, KEY_EVENT, KEY_EVENT_RECORD, STD_INPUT_HANDLE};
// spec → INPUT_RECORD 转换 + run()；locate_and_inject = run(text_records + enter)
// locate_and_send_key = run(单键记录)；发送后 FreeConsole 复位（下次附加前也幂等）
```

- [ ] **Step 4: 纯核测试跨平台过 + cfg(windows) 编译（本机即 Windows，clippy/test 全量）+ 门禁**
- [ ] **Step 5: Commit** `feat(m9-inject): Windows ConPTY 注入通道（AttachConsole+WriteConsoleInput，M6 结论落地）`

### Task 15: Windows 路由接线 + 端点贯通

**Files:** Modify `inject/engine.rs`（windows impl 已在 Task 14 换真——本任务确认 routing `platform = std::env::consts::OS`（"windows"）路径打通）；端点测试补一条 windows 真通道冒烟（`session_send` 对 FakeInjector 已覆盖，真通道属 Task 16 实机）

- [ ] **Step 1: 测试**：routing 测试已覆盖 windows 单通道；补 `session-send` 集成：FakeInjector 换 `RealInjector` 的 windows impl + pid 指向本机测试进程（用探测遗留的 mam-probe 会话或新开 conhost cmd 进程）→ delivered（**真 FFI 一跳**）。
- [ ] **Step 2: 门禁** → **Step 3: Commit** `feat(m9-inject): Windows 通道接入 session-send 全链（routing→engine→审计）`

### Task 16: M9 收口——Windows 实机自测（本机可完成）+ 门禁 + 台账

- [ ] **Step 1: 实机自测（台账逐条记录证据）**：本机开 `pnpm tauri:dev`（远程开启，回环免 PIN——gate 回环豁免既有）；WT 与 conhost（按 M6 结论）各开一个真实 claude 会话；浏览器或 `Invoke-RestMethod` POST `http://127.0.0.1:9420/m/api/v1/session-send`（带已配对 cookie 或本机豁免路径按实测定）→ 终端出现 `[mobile 设备名]` 行、会话文件命中；审批键（Task 13 取证键位）注入生效；审计页可见。**真机蜂窝复验（外网手机发消息）超出执行机能力**——handover 登记为用户项（Windows 实机 + 用户手机），不伪造。
- [ ] **Step 2: 全六门禁 + 台账 M9 完成** → **Step 3: Commit** `docs(m9): Windows 实机自测记录 + M9 收口`

### Task 16-alt: M9 降级登记（仅 M6 = NO-GO 时执行）

**Files:** Modify `inject/routing.rs`（windows 分支返回 `NotInjectable{reason_code:"platform", reason:"Windows 注入探测 NO-GO（见 M6 报告）"}`）。**二期 spec 附录 A 的矩阵更新不属执行机权限**（红线：不改设计文档）——降级申请写进台账与 handover，由 Mac 侧评审后按用户裁决落 spec。

- [ ] **Step 1: routing 降级分支 + 台账记录 → Commit** `chore(m9): NO-GO 降级——Windows 注入登记不可用`

### Task 17: 批次收尾——回归 + handover

- [ ] **Step 1: 全六门禁终跑（数字全记台账）；`git log --oneline origin/feat/phase2-injection..HEAD` 提交清单**。
- [ ] **Step 2: 一期零回归抽查**：看板/会话内容/文件预览/SSE/配对（m5 既有 vitest 全量即回归面）+ cargo test 全量。
- [ ] **Step 3: 写阶段 handover**（台账目录）：结论先行 / 每 task commit 表 / 门禁数字 / 偏离与裁决请求 / macOS 回传清单（M7 8 项 + M8 3 项）/ 风险与下一步。**不 push——等 Mac 侧评审。**

---

## 自审记录（计划侧）

- **Spec 覆盖**：W1（T1/T4）、W2 含插队撤回（T3/T5/T6）、W3（T2）、W4 多行+回执+斜杠放行（T6 天然放行无过滤、T7）、W5（T3/T8）、W6 TUI 路径+降级+漂移提示（T10–13）、W8（T14–16）；红·中断挂起（T5 快照复核）；锁屏/双开等实机项归回传清单（T9/T13/T16）；真机蜂窝复验登记用户项（T16）。F2.3/F2.4 无头不在本计划（M11）。
- **占位符**：无 TBD；「测试即规格」处均给了断言全文或要点集；`find_tmux_pane/run_tmux/flush_one` 内部给了步骤级伪码与语义注释（执行者按既有 window/tmux.rs 形态落码）。
- **类型一致性**：`Injector` 双方法、`RouteOutcome.reason_code` 枚举值、`*_conn` 命名、端点 JSON 键（camelCase wire）与 api.ts 接口逐一对齐；`flush_one(st, sid, jump)` 三参与 T5/T6 消费一致。
- **复审修订（2026-09-18 二轮，对照宪法/总 spec v1.4/批次设计 v2.1 与代码实证）**：①直发注入失败补 `{"status":"failed"}` 回执契约 + 测试 + UI 用例（W4 失败可重试；W1 定位失败不入队）；②`flush_one` 加 `jump` 参——原「is_running 不消费」复核会堵死插队（裁决 12 语义冲突，重要）；③首批映射只 批准/拒绝（spec W6 定死，原默认表多列了「不再问」）；④审计摘要定长 80；⑤serve 接线归并 Task 6（T5/T6 编译纠缠解耦）；⑥设备名新增 `pairing::device_name` 小助手（实证 pairing 无现成函数，避免 api.rs 裸 SQL）；⑦审计写口统一 `inject::audit_write`：DB + `events::audit` 日志并行（M4 T2e 惯例）；⑧无头分派缝留注释位（批次设计 T3）；⑨真机蜂窝复验登记用户项（批次设计 T4 验收含此项，执行机不可达）；⑩16-alt 明确不动 spec（红线）。复用面：get_tty_for_pid/execute_applescript/tmux 输出格式/pair_pin handler 先例/DB.lock+*_conn/broadcast 订阅/spawn_blocking+session_source/windows crate 0.57/win32 祖先链/gate::extract_device/ApiError+fetchMock——全部既有件，零新轮子、零新依赖。
