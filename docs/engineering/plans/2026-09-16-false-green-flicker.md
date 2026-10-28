# 假绿/假红信号治理（Codex CLI / WorkBuddy / OpenCode）实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 消灭三类错误状态信号——Codex CLI 回合内中间消息瞬态假绿、WorkBuddy 同款假绿（时间防抖）、OpenCode 输入瞬间假红 + 运行全程红 + 单步 >60s 假绿（会话尾部部件信号）。

**Architecture:** 全部改动收敛在 `src-tauri/src/monitor/` 状态判定层：`app_status.rs` 新增两个共享纯函数（尾部语义条目 / 回合开闭），三个解析器各自接线消费；OpenCode 新增「会话末条 part」SQL 查询与信号映射纯函数。不改 `derive_app_status` 契约、不改前端、不动 L2 缓存契约（新增字段均为纯内容产物或时间叠加现算）。

**Tech Stack:** Rust（rusqlite、serde_json）、cargo test。

**Spec:** `docs/engineering/specs/2026-09-16-false-green-flicker-design.md`（本计划从 spec 出发；执行者须同时读 spec，尤其 §4 三节设计与 §8 决策记录）

## Global Constraints

- 语言：代码注释用中文；commit message 可英文。（AGENTS.md）
- 不动清单（spec §5）：Codex APP 路线、ZCode（`zcode_parser.rs`）、Claude/Kimi/dsh/OpenClaw、共享核 `derive_app_status` 契约、前端（petStatus/FoxbellPet/未读池）。
- L2 缓存契约：摘要缓存产物只由文件内容决定；时间叠加必须每次现算（monitor::session_scan 模块文档）。
- 常量：`GREEN_DEBOUNCE_MS = 10_000`（WorkBuddy 完成防抖）；不得与 `FALLBACK_FRESH_MS`（=300s 无信号兜底窗）混用或改名对齐（spec §4.2 命名防混）。
- OpenCode `step-finish` reason 语义（spec §4.3）：`"stop"`→TurnDone；`"tool-calls"`/`"length"`/其余→Running（宁黄不假绿）；不共享 ZCode 映射（spec §8 决策 7）。
- 验收基线：`cd src-tauri && cargo test` 全绿 + `cargo clippy -- -D warnings` 干净。

## File Structure

| 文件 | 职责 | 本轮动作 |
|---|---|---|
| `src-tauri/src/monitor/app_status.rs` | 判定核与共享仲裁件 | 新增 `tail_semantic_kind`、`turn_window_open` 纯函数 + 单测 |
| `src-tauri/src/monitor/codex_parser.rs` | Codex CLI rollout 解析 | `session_from_digest` 接入回合守卫 + 夹具测试 |
| `src-tauri/src/monitor/workbuddy_parser.rs` | WorkBuddy 解析 | `GREEN_DEBOUNCE_MS` + `derive_status_with_tail` + 摘要新字段 + 防抖叠加 + 单测 |
| `src-tauri/src/monitor/opencode_parser.rs` | OpenCode 解析 | `TailSignal` + `tail_part_signal` + 尾部件查询 + `determine_opencode_status` 加参 + 单测 |

---

### Task 1: app_status.rs 共享判定件（tail_semantic_kind + turn_window_open）

**Files:**
- Modify: `src-tauri/src/monitor/app_status.rs`（在 `derive_app_status`（:79）之后新增两个 pub fn；测试加进既有 `mod tests`）

**Interfaces:**
- Consumes: `AppEntryKind`（同文件既有枚举）。
- Produces: `pub fn tail_semantic_kind(entries: &[AppEntryKind]) -> Option<AppEntryKind>`；`pub fn turn_window_open(entries: &[AppEntryKind]) -> Option<bool>`。Task 2、Task 3 依赖这两个签名。

- [x] **Step 1: 写失败测试**（追加到 `app_status.rs` 既有 `mod tests` 内）

```rust
    // ---- 假绿治理共享件（spec 2026-09-16 §4.1/§4.2）----

    #[test]
    fn tail_semantic_kind_returns_last_non_other() {
        let entries = [
            AppEntryKind::UserMessage,
            AppEntryKind::Other,
            AppEntryKind::ToolCall,
            AppEntryKind::Other,
        ];
        assert_eq!(tail_semantic_kind(&entries), Some(AppEntryKind::ToolCall));
        // 全记账条目 → None（derive 核同样返回 None，兜底留给各工具）
        assert_eq!(tail_semantic_kind(&[AppEntryKind::Other]), None);
        assert_eq!(tail_semantic_kind(&[]), None);
    }

    #[test]
    fn turn_window_open_detects_pair_order() {
        use AppEntryKind::*;
        // 开：最后一个 TurnStart 晚于最后一个 TurnEnd
        assert_eq!(
            turn_window_open(&[TurnEnd, AssistantMessage, TurnStart, UserMessage, AssistantMessage]),
            Some(true)
        );
        // 闭：最后一个 TurnEnd 更晚
        assert_eq!(turn_window_open(&[TurnStart, ToolCall, TurnEnd, AssistantMessage]), Some(false));
        // 只有 TurnStart（回合刚开始，未见 TurnEnd）→ 开
        assert_eq!(turn_window_open(&[UserMessage, TurnStart]), Some(true));
        // 只有 TurnEnd（上回合闭合痕迹，起点不可见）→ 不可证开
        assert_eq!(turn_window_open(&[TurnEnd, AssistantMessage]), Some(false));
        // 窗口内无任何边界事件 → None（调用方回退现状，不仲裁）
        assert_eq!(turn_window_open(&[UserMessage, AssistantMessage]), None);
        assert_eq!(turn_window_open(&[]), None);
    }
```

- [x] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test --lib app_status::tests::tail_semantic_kind_returns_last_non_other app_status::tests::turn_window_open_detects_pair_order`
Expected: 编译失败（`cannot find function`）。

- [x] **Step 3: 最小实现**（放在 `derive_app_status` 函数之后）

```rust
/// 尾部第一条有语义条目（= derive_app_status 的判定依据条目；全记账 → None）。
/// 假绿治理共享件（spec 2026-09-16）：WorkBuddy 完成防抖（§4.2）与 Codex 回合守卫（§4.1）
/// 共用——识别「Idle 是否由 assistant 尾导出」
pub fn tail_semantic_kind(entries: &[AppEntryKind]) -> Option<AppEntryKind> {
    entries
        .iter()
        .rev()
        .find(|k| !matches!(k, AppEntryKind::Other))
        .copied()
}

/// 回合开闭判定（spec 2026-09-16 §4.1 方案 A）：最后一个 TurnStart 晚于最后一个
/// TurnEnd → Some(true)；窗口内无任何边界事件 → None（调用方回退尾扫语义，不仲裁）。
/// dsh `TurnFacts::has_open_turn` 的 AppEntryKind 语义镜像（其类型绑定 DshEvent，不可直接复用）
pub fn turn_window_open(entries: &[AppEntryKind]) -> Option<bool> {
    let mut last_start: Option<usize> = None;
    let mut last_end: Option<usize> = None;
    for (i, k) in entries.iter().enumerate() {
        match k {
            AppEntryKind::TurnStart => last_start = Some(i),
            AppEntryKind::TurnEnd => last_end = Some(i),
            _ => {}
        }
    }
    match (last_start, last_end) {
        (Some(s), Some(e)) => Some(s > e),
        (Some(_), None) => Some(true),
        (None, Some(_)) => Some(false),
        (None, None) => None,
    }
}
```

- [x] **Step 4: 跑测试确认通过**

Run: `cd src-tauri && cargo test --lib app_status::tests`
Expected: 全部 PASS（含既有测试零回归）。

- [x] **Step 5: 提交**

```bash
git add src-tauri/src/monitor/app_status.rs
git commit -m "feat(monitor): shared tail_semantic_kind + turn_window_open arbitration helpers"
```

---

### Task 2: Codex CLI 回合守卫（方案 A 接线）

**Files:**
- Modify: `src-tauri/src/monitor/codex_parser.rs`（import 行 :4；`session_from_digest` :404 起的状态判定段；测试加进既有 `mod app_status_fixture_tests`）

**Interfaces:**
- Consumes: Task 1 的 `tail_semantic_kind` / `turn_window_open`；既有夹具构造器 `task_started` / `task_complete` / `assistant_msg` / `user_msg` / `round_one` / `round_two_running` / `round_two_complete` / `write_rollout`（同模块测试内已有）。
- Produces: 无新接口；行为变更——`session_from_digest` 在「Idle + assistant 尾 + 回合开」时改判 Processing。

- [x] **Step 1: 写失败测试**（追加到 `codex_parser.rs` 的 `mod app_status_fixture_tests`；该模块已有 `use super::*` 与 `SessionStatus`）

```rust
    /// 假绿治理 §4.1 方案 A 主回归：回合仍开（本轮 task_started 已写、task_complete 未写）时，
    /// 尾部中间 assistant 消息不得判 Idle——实测中间消息与下一轮 function_call 落盘间隔
    /// 0~3s，3s 轮询落窗即假绿触发完成语音（spec §1.2 实证序列）
    #[test]
    fn open_turn_interim_assistant_tail_is_processing() {
        let tmp = tempfile::tempdir().unwrap();
        // round_one（含 task_started + task_complete）结束后，第二轮 task_started 已落盘、
        // 中间 assistant 消息在尾、下一轮 function_call 尚未写
        let mut lines = round_one();
        lines.push(task_started("2026-09-06T05:41:19.000Z", 9));
        lines.push(user_msg("2026-09-06T05:41:25.003Z", 10, "同步到本地"));
        lines.push(assistant_msg("2026-09-06T05:41:28.121Z", 11, "开始同步"));
        let f = write_rollout(tmp.path(), &lines);
        let session = parse_codex_jsonl(&f, ProcessForm::App).unwrap();
        assert_eq!(
            session.status,
            SessionStatus::Processing,
            "回合开的中间消息不是完成信号（假绿源）"
        );
    }

    /// 方案 A 边界锁 1：窗口内无任何开闭对（既有夹具形态，超长回合/老文件）→ 不仲裁，
    /// assistant 尾保持 Idle（用户裁决：回退现状，spec §8 决策 2/5）。
    /// 既有测试 assistant_message_tail_is_idle 已锁定该行为，此处补「有旧闭对但无新开对」变体：
    /// 最后一个 TurnEnd 晚于（可见范围内）任何 TurnStart → 不可证开 → 不仲裁
    #[test]
    fn closed_turn_assistant_tail_stays_idle() {
        let tmp = tempfile::tempdir().unwrap();
        // round_two_running 无 task_started，其 assistant 追加尾位于 round_one 的
        // task_complete 之后：窗口内 TurnEnd 晚于 TurnStart → Some(false) → 不改判
        let mut lines = round_one();
        lines.extend(round_two_running());
        lines.push(assistant_msg("2026-09-06T05:41:43.138Z", 16, "同步完成"));
        let f = write_rollout(tmp.path(), &lines);
        let session = parse_codex_jsonl(&f, ProcessForm::App).unwrap();
        assert_eq!(session.status, SessionStatus::Idle);
    }
```

- [x] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test --lib codex_parser::app_status_fixture_tests::open_turn_interim_assistant_tail_is_processing`
Expected: FAIL——实际得到 `Processing != Idle` 断言失败（守卫未实现，中间消息被判 Idle）。
（`closed_turn_assistant_tail_stays_idle` 此时应已 PASS，作为行为不变锁。）

- [x] **Step 3: 实现守卫**

3a. 扩 import（`codex_parser.rs:4`）：

```rust
use super::app_status::{
    derive_app_status, overlay_mtime_stale, tail_semantic_kind, turn_window_open, AppEntryKind,
};
```

3b. `session_from_digest` 内，`let status = match derive_app_status(&digest.kinds) { ... };` 的 match 表达式改为 `let mut status = ...`（None 兜底分支不变），并在该 match 之后、`overlay_mtime_stale` 调用之前插入：

```rust
    // 回合守卫（spec 假绿治理 §4.1 方案 A）：回合仍开（最后一个 TurnStart 晚于最后一个
    // TurnEnd）时，尾部的 assistant 消息是回合内中间消息而非完成信号——改判 Processing，
    // 拦截「中间消息短暂占据尾部」的瞬态假绿；窗口内无边界事件 → 不仲裁，回退尾扫语义
    if status == SessionStatus::Idle
        && tail_semantic_kind(&digest.kinds) == Some(AppEntryKind::AssistantMessage)
        && turn_window_open(&digest.kinds) == Some(true)
    {
        status = SessionStatus::Processing;
    }
```

- [x] **Step 4: 跑模块全部测试确认通过（重点回归 issue #6 既有判定组）**

Run: `cd src-tauri && cargo test --lib codex_parser`
Expected: 全部 PASS——新增 2 个 + 既有 `function_call_tail_is_processing` / `task_complete_tail_is_idle` / `assistant_message_tail_is_idle` / `two_rounds_same_file_second_round_running_then_idle` / `processing_stale_downgrades_to_idle` 等零回归（方案 A 下 `assistant_message_tail_is_idle` 夹具窗口内无新开对，语义不变）。

- [x] **Step 5: 提交**

```bash
git add src-tauri/src/monitor/codex_parser.rs
git commit -m "fix(codex): open-turn guard stops interim-message false green on rollout path"
```

---

### Task 3: WorkBuddy 完成防抖（GREEN_DEBOUNCE_MS = 10s）

**Files:**
- Modify: `src-tauri/src/monitor/workbuddy_parser.rs`（import :5；`WorkBuddyTailDigest` :23-34；`derive_status_from_tail` :211 起重构；构卡层叠加点 :393 后；新增 `mod debounce_tests`）

**Interfaces:**
- Consumes: Task 1 的 `tail_semantic_kind`；既有 `workbuddy_entry_kind` / `derive_app_status` / `overlay_mtime_stale`。
- Produces: `pub const GREEN_DEBOUNCE_MS: u64`（=10_000）；`fn derive_status_with_tail(lines: &[String]) -> (SessionStatus, Option<AppEntryKind>)`（模块内）；`fn apply_green_debounce(status, tail_kind, mtime_age_ms) -> SessionStatus`（模块内，纯函数可测）。`pub fn derive_status_from_tail` 对外签名不变。

- [x] **Step 1: 写失败测试**（新增测试模块，放在 `derive_status_from_tail` 附近或文件既有测试区）

```rust
#[cfg(test)]
mod debounce_tests {
    use super::*;

    /// §4.2 判定表：仅「Idle 且尾部语义条目为 AssistantMessage 且 mtime 年龄 < 10s」拉回
    /// Processing；窗口边界值 10_000ms 恰好放行（< 判定）；其余状态一概透传
    #[test]
    fn debounce_holds_processing_only_for_fresh_assistant_idle() {
        use crate::session::SessionStatus::*;
        // 防抖窗内：Idle ← assistant 尾 → Processing（拦截中间消息瞬态假绿）
        assert_eq!(
            apply_green_debounce(Idle, Some(AppEntryKind::AssistantMessage), 9_999),
            Processing
        );
        assert_eq!(
            apply_green_debounce(Idle, Some(AppEntryKind::AssistantMessage), 0),
            Processing
        );
        // 恰好到窗：放行转绿（真完成绿灯/语音延迟 10s 到达，用户已接纳）
        assert_eq!(
            apply_green_debounce(Idle, Some(AppEntryKind::AssistantMessage), GREEN_DEBOUNCE_MS),
            Idle
        );
        // 非 assistant 尾导出的 Idle（tail 为 None，如仅记账条目兜底前的形态）→ 不防抖
        assert_eq!(apply_green_debounce(Idle, None, 0), Idle);
        // 其他状态透传：Waiting 兜底、Processing（含 function_call 尾）、Thinking（user 尾）不受影响
        assert_eq!(apply_green_debounce(Waiting, Some(AppEntryKind::AssistantMessage), 0), Waiting);
        assert_eq!(
            apply_green_debounce(Processing, Some(AppEntryKind::AssistantMessage), 0),
            Processing
        );
        assert_eq!(apply_green_debounce(Processing, Some(AppEntryKind::ToolCall), 0), Processing);
        assert_eq!(apply_green_debounce(Thinking, Some(AppEntryKind::UserMessage), 0), Thinking);
    }

    /// 尾部语义条目随摘要产出：assistant 文本尾 → (Idle, AssistantMessage)；
    /// function_call 尾 → (Processing, ToolCall)
    #[test]
    fn derive_status_with_tail_exposes_tail_kind() {
        let assistant_tail = vec![
            r#"{"type":"message","role":"user","content":"跑一下"}"#.to_string(),
            r#"{"type":"message","role":"assistant","content":"我先看一下"}"#.to_string(),
        ];
        assert_eq!(
            derive_status_with_tail(&assistant_tail),
            (crate::session::SessionStatus::Idle, Some(AppEntryKind::AssistantMessage))
        );
        let tool_tail = vec![
            r#"{"type":"message","role":"user","content":"跑一下"}"#.to_string(),
            r#"{"type":"function_call","name":"shell"}"#.to_string(),
        ];
        assert_eq!(
            derive_status_with_tail(&tool_tail),
            (crate::session::SessionStatus::Processing, Some(AppEntryKind::ToolCall))
        );
    }
}
```

- [x] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test --lib workbuddy_parser::debounce_tests`
Expected: 编译失败（`cannot find function apply_green_debounce / derive_status_with_tail / GREEN_DEBOUNCE_MS`）。

- [x] **Step 3: 实现**

3a. 扩 import（`workbuddy_parser.rs:5`，现为 `use super::app_status::{derive_app_status, AppEntryKind};`）：

```rust
use super::app_status::{derive_app_status, tail_semantic_kind, AppEntryKind};
```

3b. 常量（放在 `HEARTBEAT_FRESH_MS`（:37）附近）：

```rust
/// 完成防抖窗（spec 假绿治理 §4.2）：assistant 语义尾 + JSONL mtime 年龄 < 该窗 → 拉回
/// Processing（WorkBuddy 格式无轮次信号，中间消息假绿只能时间防抖），≥ 该窗才转绿。
/// 注意与 FALLBACK_FRESH_MS（无信号兜底新鲜窗，300s）语义不同，独立常量不得混用
pub const GREEN_DEBOUNCE_MS: u64 = 10_000;
```

3c. `derive_status_from_tail`（:211）重构为薄壳 + 元组版：

```rust
/// 尾部状态 + 尾部语义条目（摘要缓存消费；tail_kind 供完成防抖判定「Idle 是否由
/// assistant 尾导出」，纯内容产物可随 L2 缓存）
fn derive_status_with_tail(lines: &[String]) -> (SessionStatus, Option<AppEntryKind>) {
    let mut kinds: Vec<AppEntryKind> = Vec::new();
    for line in lines {
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(v) if v.get("type").is_some() => kinds.push(workbuddy_entry_kind(&v)),
            _ => continue,
        }
    }
    (
        derive_app_status(&kinds).unwrap_or(SessionStatus::Waiting),
        tail_semantic_kind(&kinds),
    )
}

/// 完成防抖纯判定（§4.2，可测）：仅作用于「assistant 尾导出的 Idle + 新鲜」，
/// 其余状态透传；真完成的转绿由调用方在 mtime 年龄 ≥ 窗口后自然到达
fn apply_green_debounce(
    status: SessionStatus,
    tail_kind: Option<AppEntryKind>,
    mtime_age_ms: u64,
) -> SessionStatus {
    if status == SessionStatus::Idle
        && tail_kind == Some(AppEntryKind::AssistantMessage)
        && mtime_age_ms < GREEN_DEBOUNCE_MS
    {
        SessionStatus::Processing
    } else {
        status
    }
}
```

（原 `derive_status_from_tail` 的函数体即 `derive_status_with_tail` 的循环部分，保留原 doc 注释、签名与语义不变：）

```rust
pub fn derive_status_from_tail(lines: &[String]) -> SessionStatus {
    derive_status_with_tail(lines).0
}
```

3d. 摘要结构体（:23-34）加字段并填充：

```rust
struct WorkBuddyTailDigest {
    status_core: SessionStatus,
    /// 尾部语义条目（纯内容产物，随摘要缓存；完成防抖判定依据）
    tail_kind: Option<AppEntryKind>,
    last_message: Option<String>,
}

fn read_workbuddy_tail_digest(jsonl: &Path) -> WorkBuddyTailDigest {
    let lines = read_recent_lines(jsonl, 500);
    let (status_core, tail_kind) = derive_status_with_tail(&lines);
    WorkBuddyTailDigest {
        status_core,
        tail_kind,
        last_message: lines.iter().rev().find_map(|l| extract_message_text(l)),
    }
}
```

3e. 构卡层叠加点（:393 `let status = overlay_mtime_stale(...)` 改为；顺序按 spec §4.2——防抖在内层、`overlay_mtime_stale` 在外层。两叠加作用域互斥（防抖只动 Idle、overlay 只动 Processing），顺序无行为差异，取 spec 字面顺序）：

```rust
        let status = overlay_mtime_stale(
            apply_green_debounce(d.status_core.clone(), d.tail_kind, mtime_age_ms),
            mtime_age_ms,
        );
```

- [x] **Step 4: 跑模块全部测试确认通过**

Run: `cd src-tauri && cargo test --lib workbuddy_parser`
Expected: 全部 PASS（既有 `derive_status_from_tail` 系列测试经薄壳零回归；`unread` 池测试（:530 附近用 `derive_status_from_tail`）不受影响）。

- [x] **Step 5: 提交**

```bash
git add src-tauri/src/monitor/workbuddy_parser.rs
git commit -m "fix(workbuddy): 10s green debounce on assistant tail kills transient false green"
```

---

### Task 4: OpenCode 会话尾部部件信号（修三症状）

**Files:**
- Modify: `src-tauri/src/monitor/opencode_parser.rs`（`build_session_from_row` :209-226；`determine_opencode_status` :355 加参；既有 `mod status_tests` 适配；新增查询与映射 + `mod tail_signal_tests`）

**Interfaces:**
- Consumes: 既有 `MessageData`（:14）；`SessionStatus`。
- Produces（全部模块内私有）: `enum TailSignal { TurnDone, Running, Fallback }`；`fn tail_part_signal(part: &serde_json::Value, message_role: Option<&str>, message_has_step: bool) -> TailSignal`；`struct SessionTailPart { message_id: String, message_role: Option<String>, part: serde_json::Value }`；`fn get_session_tail_part(conn: &Connection, session_id: &str) -> Option<SessionTailPart>`；`fn message_has_step_part(conn: &Connection, message_id: &str) -> bool`；`determine_opencode_status` 新末参 `tail: TailSignal`。

- [x] **Step 1: 写失败测试**（新增测试模块；既有 `status_tests` 两测在 Step 4 一并加参适配）

```rust
#[cfg(test)]
mod tail_signal_tests {
    use super::*;

    fn part(ptype: &str, reason: Option<&str>) -> serde_json::Value {
        let mut v = serde_json::json!({ "type": ptype });
        if let Some(r) = reason {
            v["reason"] = serde_json::json!(r);
        }
        v
    }

    /// §4.3 判定表（词汇表活体取证 2026-09-16，opencode v1.18.22）
    #[test]
    fn tail_part_signal_rules() {
        // step-finish(stop) → 回合结束，走既有启发式（收尾红→绿语义保留）
        assert_eq!(tail_part_signal(&part("step-finish", Some("stop")), Some("assistant"), false), TailSignal::TurnDone);
        // step-finish(tool-calls / length) → 后续还有动作（length 宁黄不假绿，spec §8 决策 7）
        assert_eq!(tail_part_signal(&part("step-finish", Some("tool-calls")), Some("assistant"), false), TailSignal::Running);
        assert_eq!(tail_part_signal(&part("step-finish", Some("length")), Some("assistant"), false), TailSignal::Running);
        // 步骤进行中部件
        assert_eq!(tail_part_signal(&part("step-start", None), Some("assistant"), false), TailSignal::Running);
        assert_eq!(tail_part_signal(&part("reasoning", None), Some("assistant"), false), TailSignal::Running);
        assert_eq!(tail_part_signal(&part("tool", None), Some("assistant"), false), TailSignal::Running);
        // 用户消息的部件（任意类型）= 输入刚提交（含 assistant 占位行空窗）→ Running（修输入瞬间假红）
        assert_eq!(tail_part_signal(&part("text", None), Some("user"), false), TailSignal::Running);
        // assistant text/patch：新格式（消息含 step 部件）流式窗口 → Running；
        // 老格式 team-mode（无 step 部件）→ Fallback 回退启发式（老会话零回归）
        assert_eq!(tail_part_signal(&part("text", None), Some("assistant"), true), TailSignal::Running);
        assert_eq!(tail_part_signal(&part("text", None), Some("assistant"), false), TailSignal::Fallback);
        assert_eq!(tail_part_signal(&part("patch", None), Some("assistant"), false), TailSignal::Fallback);
        // 未知类型 / 无 role 信息 → Fallback
        assert_eq!(tail_part_signal(&part("file", None), Some("assistant"), false), TailSignal::Fallback);
        assert_eq!(tail_part_signal(&part("text", None), None, false), TailSignal::Fallback);
    }

    /// Running 强信号短路既有启发式：即便 last_role=assistant 且新鲜（旧逻辑判 Waiting 红），
    /// 也返回 Processing（修「输入瞬间绿→红假语音」与「运行全程红」）
    #[test]
    fn running_signal_short_circuits_to_processing() {
        let now = chrono::Utc::now().timestamp_millis();
        assert_eq!(
            determine_opencode_status(0.0, Some("assistant"), now, now, TailSignal::Running),
            crate::session::SessionStatus::Processing
        );
    }

    /// TurnDone / Fallback 走既有语义（assistant+新鲜 → Waiting；超窗 → Idle）
    #[test]
    fn turn_done_and_fallback_keep_legacy_behavior() {
        let now = chrono::Utc::now().timestamp_millis();
        for tail in [TailSignal::TurnDone, TailSignal::Fallback] {
            assert_eq!(
                determine_opencode_status(0.0, Some("assistant"), now, now, tail),
                crate::session::SessionStatus::Waiting
            );
            let old = now - 61_000;
            assert_eq!(
                determine_opencode_status(0.0, Some("assistant"), old, old, tail),
                crate::session::SessionStatus::Idle
            );
        }
    }

    /// 尾部件查询：跨消息取会话末条 part + 所属 role（占位行空窗场景——末条 part 属 user 消息）
    #[test]
    fn session_tail_part_crosses_messages() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
             CREATE TABLE part (id INTEGER PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, data TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message VALUES ('m1','s1',100,'{\"role\":\"user\"}')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message VALUES ('m2','s1',200,'{\"role\":\"assistant\"}')",
            [],
        )
        .unwrap();
        // 占位行 m2 无部件；末条 part 属 m1（user）
        conn.execute(
            "INSERT INTO part VALUES (1,'m1','s1',101,'{\"type\":\"text\",\"text\":\"列一下目录\"}')",
            [],
        )
        .unwrap();
        let tail = get_session_tail_part(&conn, "s1").unwrap();
        assert_eq!(tail.message_role.as_deref(), Some("user"));
        assert_eq!(tail.part.get("type").and_then(|t| t.as_str()), Some("text"));
        // m2 无 step 部件
        assert!(!message_has_step_part(&conn, "m2"));
        // step 部件探测
        conn.execute(
            "INSERT INTO part VALUES (2,'m2','s1',300,'{\"type\":\"step-finish\",\"reason\":\"stop\"}')",
            [],
        )
        .unwrap();
        assert!(message_has_step_part(&conn, "m2"));
        // 末条 part 现属 m2 → role assistant
        let tail2 = get_session_tail_part(&conn, "s1").unwrap();
        assert_eq!(tail2.message_role.as_deref(), Some("assistant"));
    }
}
```

- [x] **Step 2: 跑测试确认失败**

Run: `cd src-tauri && cargo test --lib opencode_parser::tail_signal_tests`
Expected: 编译失败（`TailSignal` / 新函数 / `determine_opencode_status` 参数个数不匹配）。

- [x] **Step 3: 实现**

3a. `TailSignal` 与 `tail_part_signal`（放在 `determine_opencode_status` 之前）：

```rust
/// 会话尾部部件信号（spec 假绿治理 §4.3，2026-09-16）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TailSignal {
    /// step-finish(reason=stop)：回合结束 → 走既有 last_role+60s 启发式
    ///（60s 窗内 Waiting 红 → Idle 绿；用户验证该收尾转换为正常语义，保留）
    TurnDone,
    /// 步骤进行中 / 用户输入刚提交 / step-finish(reason≠stop) → Processing 黄
    Running,
    /// 无部件或老格式无 step 信号（team-mode text/patch）→ 回退既有启发式
    Fallback,
}

/// 尾部部件 → 信号（纯函数）。词汇表活体取证 2026-09-16（opencode v1.18.22）：
/// step-start / reasoning / tool / step-finish(reason=tool-calls|stop)。
/// 注意 reason=length 归 Running（宁黄不假绿），与 ZCode part_entry_kind 的
/// length→TurnEnd 语义相反，故不共享其映射（spec §8 决策 7）
fn tail_part_signal(
    part: &serde_json::Value,
    message_role: Option<&str>,
    message_has_step: bool,
) -> TailSignal {
    // 用户消息的部件（任意类型）= 输入刚提交——含 assistant 占位行空窗
    //（opencode 按回车后 ~130ms 即写空 assistant 行，末条 part 仍属 user 消息）
    if message_role == Some("user") {
        return TailSignal::Running;
    }
    let ptype = part.get("type").and_then(|t| t.as_str()).unwrap_or_default();
    match ptype {
        "step-finish" => {
            if part.get("reason").and_then(|r| r.as_str()) == Some("stop") {
                TailSignal::TurnDone
            } else {
                TailSignal::Running // tool-calls / length 等：后续还有动作
            }
        }
        "step-start" | "reasoning" | "tool" => TailSignal::Running,
        // assistant 的 text/patch：消息含 step 部件 → 新格式流式窗口进行中；
        // 无 step 部件 → team-mode 老格式无信号，回退启发式（老会话零回归）
        "text" | "patch" => {
            if message_has_step {
                TailSignal::Running
            } else {
                TailSignal::Fallback
            }
        }
        _ => TailSignal::Fallback,
    }
}
```

3b. 查询函数（放在 `get_message_text` 之后）：

```rust
/// 会话末条部件（跨消息，按落盘时间倒序取 1）+ 所属消息 role（spec §4.3）。
/// part 表自带 session_id 列，无需绕消息表过滤；role 经 LEFT JOIN message 取
struct SessionTailPart {
    message_id: String,
    message_role: Option<String>,
    part: serde_json::Value,
}

fn get_session_tail_part(conn: &Connection, session_id: &str) -> Option<SessionTailPart> {
    let row = conn
        .query_row(
            "SELECT p.message_id, p.data, m.data FROM part p \
             LEFT JOIN message m ON m.id = p.message_id \
             WHERE p.session_id = ?1 ORDER BY p.time_created DESC LIMIT 1",
            [session_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .ok()?;
    let (message_id, part_data, message_data) = row;
    let part = serde_json::from_str(&part_data).ok()?;
    let message_role = message_data.and_then(|d| {
        serde_json::from_str::<MessageData>(&d)
            .ok()
            .and_then(|m| m.role)
    });
    Some(SessionTailPart {
        message_id,
        message_role,
        part,
    })
}

/// 消息是否含 step 部件（新格式标识；仅尾部为 text/patch 时按需调用）。
/// 沿模块既有模式取原始 data 在 Rust 侧解析（不依赖 SQLite JSON1 扩展）
fn message_has_step_part(conn: &Connection, message_id: &str) -> bool {
    let Ok(mut stmt) = conn.prepare("SELECT data FROM part WHERE message_id = ?1") else {
        return false;
    };
    let Ok(rows) = stmt.query_map([message_id], |r| r.get::<_, String>(0)) else {
        return false;
    };
    for row in rows.flatten() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&row) else {
            continue;
        };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("step-start") | Some("step-finish") => return true,
            _ => {}
        }
    }
    false
}
```

3c. `determine_opencode_status` 加末参并在函数体最前短路：

```rust
fn determine_opencode_status(
    cpu: f32,
    last_role: Option<&str>,
    last_msg_time: i64,
    session_updated: i64,
    tail: TailSignal,
) -> SessionStatus {
    // 尾部部件强信号（spec 假绿治理 §4.3）：步骤进行中/用户输入 → 黄灯，短路既有
    // 启发式（修「输入瞬间绿→红假语音」「运行全程红」「单步>60s 假绿」三症状）；
    // TurnDone 与 Fallback 走既有语义
    if tail == TailSignal::Running {
        return SessionStatus::Processing;
    }
    // ……以下原函数体不变……
```

3d. `build_session_from_row`（:218-226）取信号并传参：

```rust
    let (last_role, last_message) = get_last_message_info(conn, session_id);
    let last_msg_time = get_last_message_time(conn, session_id);
    // 会话尾部部件信号（§4.3）：末条 part + 所属 role；仅 text/patch 尾按需查 step 部件
    let tail = get_session_tail_part(conn, session_id)
        .map(|t| {
            let ptype = t.part.get("type").and_then(|x| x.as_str()).unwrap_or_default();
            let has_step =
                (ptype == "text" || ptype == "patch") && message_has_step_part(conn, &t.message_id);
            tail_part_signal(&t.part, t.message_role.as_deref(), has_step)
        })
        .unwrap_or(TailSignal::Fallback);

    let status = determine_opencode_status(
        process.cpu_usage,
        last_role.as_deref(),
        last_msg_time,
        time_updated,
        tail,
    );
```

- [x] **Step 4: 适配既有测试并跑全模块**

既有 `mod status_tests` 两个测试的 `determine_opencode_status(...)` 调用补第 5 参 `TailSignal::Fallback`（行为不变语义）：

```rust
        assert_eq!(
            determine_opencode_status(50.0, Some("assistant"), old, old, TailSignal::Fallback),
            crate::session::SessionStatus::Idle
        );
```
```rust
        assert_eq!(
            determine_opencode_status(50.0, Some("user"), now, now, TailSignal::Fallback),
            crate::session::SessionStatus::Processing
        );
```

Run: `cd src-tauri && cargo test --lib opencode_parser`
Expected: 全部 PASS（新增 4 个 + 既有 2 个适配后零回归）。

- [x] **Step 5: 提交**

```bash
git add src-tauri/src/monitor/opencode_parser.rs
git commit -m "fix(opencode): session-tail part signal fixes false red at input, all-run red, and >60s false green"
```

---

### Task 5: 全量回归 + 端到端验收（E2E）

**Files:**
- 无新增代码（验证任务；若 clippy/fmt 产生修复，单独提交）

**Interfaces:**
- Consumes: Task 1-4 的全部产出；用户配合操作三个工具（Codex CLI / WorkBuddy / OpenCode）。

#### 5A. 自动化回归

- [x] **Step 1: Rust 全量测试**

Run: `cd src-tauri && cargo test`
Expected: 全部 PASS（重点确认 issue #6 的 codex `app_status_fixture_tests` 组、WorkBuddy 未读池测试、OpenCode `status_tests` 无回归）。

- [x] **Step 2: clippy 与格式**

Run: `cd src-tauri && cargo clippy -- -D warnings && cargo fmt --check`
Expected: 干净；若有 fmt 差异运行 `cargo fmt` 后单独提交 `style: rustfmt`。

- [x] **Step 3: 改动面确认**

Run: `git diff --stat origin/main -- src/ package.json` → 空输出（不动前端，spec §5）；
`git diff --stat origin/main -- src-tauri/src/monitor/` → 仅 4 个文件（app_status / codex_parser / workbuddy_parser / opencode_parser），不动清单成员零触碰。

#### 5B. 端到端验收（用户配合，三工具实操）

**环境准备（Step 4）：**

1. 退出日常运行的 MAM 实例（它不含本修复；双实例会抢托盘/宠物窗造成观察混淆）。
2. 从 worktree 启动带修复的 MAM：`cd /Users/jarvis/Documents/mam-worktree3 && pnpm install && pnpm tauri:dev`。
3. 宠物挂载且 done/approval 语音开启；MAM 看板可见三个工具的会话卡。
4. 用户开三个窗口待命：Codex CLI（任意项目目录）、WorkBuddy、OpenCode。
5. 执行者（agent）全程每 ~5s 采样工具侧数据文件，与用户观察的灯色/语音对齐时间线：
   - Codex：`tail` 对应 rollout 文件的 type/payload.type/role 序列；
   - OpenCode：`sqlite3 -readonly ~/.local/share/opencode/opencode.db` 查末条 part 与 message；
   - WorkBuddy：`tail` 对应 `~/.workbuddy/projects/*/<sessionId>.jsonl`。

- [ ] **Step 5: Codex CLI 场景——多轮任务零假绿**

用户在 Codex CLI 发指令（必然 3+ 轮工具调用）：

> 查看当前 git log 最近 5 条提交，逐条总结每次改了什么

PASS 判据（全部满足）：
- a. 任务全程（含每轮工具调用间隙）卡片黄灯，宠物零语音；
- b. 任务真正结束后恰好一次绿灯 + 一次 done 语音；
- c. 执行者证据：rollout 尾部序列中存在「`msg/assistant` 后紧跟下一轮 `function_call`」的中间消息形态（即假绿触发条件在本轮真实成立过），且期间卡片未转绿。

- [ ] **Step 6: WorkBuddy 场景——防抖延迟转绿**

用户在 WorkBuddy 发指令（1~2 轮工具调用）：

> 列出当前目录的文件，并统计一共有多少个

PASS 判据：
- a. 运行全程黄灯，轮次间隙无假绿、无语音；
- b. 任务完成后绿灯与 done 语音延迟约 10s 到达（允许 10~13s，含 3s 轮询与音频起播抖动）——用户体感确认「完成语音晚了一拍但只响一次」。

- [ ] **Step 7: OpenCode 场景——三症状逐一验证**

用户在 OpenCode 依次发两条指令：

指令 1（验症状①输入瞬间 + 症状②全程黄）：

> 用 ls 列出当前目录的文件，并告诉我一共有几个

PASS 判据：
- a. 按下回车的瞬间卡片转**黄**（不红、无 approval 语音）；
- b. 任务运行期间全程黄，无红「等待操作」。

指令 2（验症状③单步 >60s 不假绿 + 正常收尾）：

> 运行 sleep 70，然后告诉我退出码
>（若 bash 工具超时限制不足 70s，改发：通读目录里最大的一个源文件并总结其结构）

PASS 判据：
- c. 单步执行的 70s 期间卡片保持黄，不闪绿、无 done 语音；
- d. 结束后红（≤60s，既有语义）→ 绿 + 恰好一次 done 语音。

- [ ] **Step 8: E2E 证据归档与失败协议**

- 每场景记录：指令文本、发送时刻、灯色迁移时间线（用户口述 + 执行者数据采样对齐）、语音次数。
- 任一判据 FAIL：执行者立即采样当时的 rollout/DB 尾部序列留证，按 systematic-debugging 流程定位（预期常见点：轮询时序、宠物语音冷却干扰观察、Codex 会话卡与 APP 卡混淆——注意观察的是 CLI 形态卡）；修复后该场景重跑。

- [ ] **Step 9: 汇报**

输出五列判定表：场景 × 判据 × 预期 × 实际 × PASS/FAIL；全部 PASS 后本计划收口，剩余按 finishing-a-development-branch 流程处理分支合流。

---

## Self-Review 记录

- **Spec 覆盖**：§4.1 方案 A → Task 1+2；§4.2 → Task 1+3；§4.3 → Task 4（三症状 + 老格式回退 + length 语义 + tool 部件在尾即 Running）；§5 不动清单 → 各任务 Modify 清单 + Task 5 Step 3 验证；§6 测试与验收 → 各任务测试步 + Task 5；§8 决策 7（不共享 ZCode 映射）→ Task 4 注释与代码。无遗漏。
- **占位符扫描**：无 TBD/TODO/「类似 Task N」；所有代码步含完整代码。
- **类型一致性**：`tail_semantic_kind(&[AppEntryKind]) -> Option<AppEntryKind>`、`turn_window_open(&[AppEntryKind]) -> Option<bool>`（Task 1 定义，Task 2/3 消费）；`TailSignal` 三变体与 `tail_part_signal` 签名在 Task 4 内自洽；`determine_opencode_status` 五参签名在 Step 3c/3d/Step 4 间一致。
- **行号锚点**：Task 3 的 `:211`、Task 4 的 `:209-226`/`:355` 均为 main 分支实测行号；执行时如有漂移以函数名定位。
- **二次复查（2026-09-16，用户三轮审阅）**：① Task 3 叠加顺序改为与 spec §4.2 字面一致（防抖内层、overlay 外层；行为等价，两者作用域互斥）；② Task 3 测试补 spec 测试清单的两条透传断言（function_call 尾 Processing / user 尾 Thinking）；③ Task 5 扩为完整 E2E（5A 自动化回归 + 5B 三工具实操协议、PASS 判据、证据采样、失败协议）；④ spec 回写两处计划期精确化（`turn_window_open` 的仅 TurnEnd 可见 → Some(false)；tool 部件在尾即 Running 覆盖增强设想）。宪法与关联文档核对：MASTER-PLAN 一期 R1.1/R1.2（状态准确性与边沿提醒）正向受益；二期 R2.3（黄状态排队不打断运行会话）正是假绿治理保护的语义；ZCode spec（2026-09-08）对尾扫核的叙述属其自身格式（有 step-finish 守卫），本轮不动 ZCode，无冲突。
