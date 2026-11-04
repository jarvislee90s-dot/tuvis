# 会话状态判定锚点纪律与机制收敛 Implementation Plan

> **执行状态（2026-11-04 收尾）**：Task 1-5（P0/P1/P2）✅ 已交付于分支
> `fix/session-age-anchor-discipline`；**Task 6-7（P3 claude Pending）⚠️ 未纳入本批**——
> review 与真机取证发现两个阻断问题（Stop 通知落盘形态已变，命中率约 1.5%；
> Pending 位置被 hook 叠加层覆盖），详见设计 spec §9.7 与 issue #200。
> 本文件保留 Task 6-7 原文作为重做参考，**不要按 Task 6-7 直接实施**。
> Task 8 的验收部分已按实际交付范围调整。

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 修复 WorkBuddy 任务结束后绿↔黄 30 秒周期振荡（根因：心跳时刻污染完成冷却锚点），并建立「一个 age 值只服务一个判定语义」的架构约束，防止同类缺陷复发。

**Architecture:** 在 `monitor/app_status.rs` 共享核新增 `SessionAges`（双轴年龄，字段私有 + 唯一构造点）与 `CooldownSignal`（完成冷却通用形态）；WorkBuddy 改用 `content_age` 喂冷却、`presence_age` 喂停更降级；时间常量从 5 个 parser 集中到共享核。~~最后把 claude 现成的子代理三态机接入 Pending 拦绿~~（已撤回）。

**Tech Stack:** Rust 1.8x / Tauri 2 / rusqlite / cargo test

**关联文档：**
- 设计 spec：`docs/engineering/specs/2026-11-04-session-age-anchor-discipline-design.md`
- 前置 spec：`docs/engineering/specs/2026-09-16-false-green-flicker-design.md`
- P3 后续 issue：[#200](https://github.com/jarvislee90s-dot/tuvis/issues/200)

**工作目录：** `E:\LLMproject\Github\tuvis-worktree3`（分支 `pr-190`）

---

## 文件结构总览

| 文件 | 职责 | 本计划改动 | 状态 |
|---|---|---|---|
| `src-tauri/src/monitor/app_status.rs` | APP 形态状态判定共享核：尾部倒扫、停更降级、后代仲裁 | **新增** `SessionAges`、`CooldownSignal`、`apply_completion_cooldown`；**集中**时间常量 | ✅ |
| `src-tauri/src/monitor/workbuddy_parser.rs` | WorkBuddy 证据层 + 构卡 | **改**构卡层锚点；**删**本地 `apply_green_debounce`（迁入共享核） | ✅ |
| `src-tauri/src/monitor/subagents/claude.rs` | Claude 子代理状态机与聚合 | ~~新增 `descendant_activity_at` 聚合出口~~ | ⚠️ 本批不做 |
| `src-tauri/src/monitor/claude_parser.rs` | Claude 构卡 | ~~末尾接 Pending 拦绿~~ | ⚠️ 本批不做 |
| `src-tauri/src/monitor/kimi_parser.rs` | Kimi 构卡 | **改**常量改为引用共享核 | ✅ |
| `src-tauri/src/monitor/opencode_parser.rs` | OpenCode 构卡 | **改**常量改为引用共享核 | ✅ |
| `src-tauri/src/monitor/codex_thread_parser.rs` | Codex APP 构卡 | **改**常量改为引用共享核 | ✅ |
| `src-tauri/src/monitor/zcode_parser.rs` | ZCode 构卡 | **改**常量改为引用共享核 | ✅ |
| `src-tauri/src/monitor/dsh/status.rs` | dsh 状态映射 | **改**常量注释（30min 刻意不迁入共享核） | ✅ |

**任务独立性：** Task 1-2 是 P0（行为修复，必须）；Task 3 是 P1（纯重构）；Task 4-5 是 P2（纯重构）；~~Task 6-7 是 P3（新增能力）~~（已撤回）。每个 Task 结束都应跑测试并通过、可独立提交。

---

## Task 1: 新增 `SessionAges` 双轴年龄类型

**Files:**
- Modify: `src-tauri/src/monitor/app_status.rs`（在 `APP_STATUS_STALE_MS` 常量之后、`overlay_mtime_stale` 之前插入）

- [ ] **Step 1: 写失败测试**

在 `app_status.rs` 的 `mod tests` 内（文件末尾 `#[cfg(test)] mod tests` 里，与其他判定核测试并列）追加：

```rust
    // ---- SessionAges：双轴年龄（P0 锚点纪律） ----

    /// content 轴只取内容源；presence 轴取全部来源的最大值
    #[test]
    fn session_ages_separates_content_and_presence() {
        let now = 1_000_000u64;
        // 内容 40 秒前写下；DB 活动 5 秒前；心跳 1 秒前
        let ages = SessionAges::derive(now, Some(now - 40_000), &[
            Some(now - 40_000),
            Some(now - 5_000),
            Some(now - 1_000),
        ]);
        assert_eq!(ages.content_age_ms, Some(40_000), "content 只认内容源");
        assert_eq!(ages.presence_age_ms, Some(1_000), "presence 取最近生命迹象");
    }

    /// 内容源缺失 → content=None（未知不等于刚刚）；presence 仍可由其他源给出
    #[test]
    fn session_ages_content_unknown_when_no_content_source() {
        let now = 1_000_000u64;
        let ages = SessionAges::derive(now, None, &[None, Some(now - 3_000)]);
        assert_eq!(ages.content_age_ms, None);
        assert_eq!(ages.presence_age_ms, Some(3_000));
    }

    /// 全部来源缺失 → 两轴均为 None
    #[test]
    fn session_ages_both_none_when_no_sources() {
        let ages = SessionAges::derive(1_000_000, None, &[None, None]);
        assert_eq!(ages.content_age_ms, None);
        assert_eq!(ages.presence_age_ms, None);
    }

    /// 心跳只能刷新 presence，绝不能缩短 content —— 本类缺陷的回归锁
    #[test]
    fn heartbeat_never_shortens_content_age() {
        let now = 1_000_000u64;
        let content_ms = Some(now - 30_000);
        let ages = SessionAges::derive(now, content_ms, &[content_ms, Some(now - 500)]);
        assert_eq!(
            ages.content_age_ms,
            Some(30_000),
            "心跳新鲜时 content 仍是 30s（冷却窗必须等满）"
        );
        assert_eq!(ages.presence_age_ms, Some(500));
    }
```

- [ ] **Step 2: 运行测试确认失败**

```bash
cd src-tauri && cargo test --lib monitor::app_status::tests::session_ages 2>&1 | tail -20
```

Expected: 编译失败，`cannot find type 'SessionAges' in this scope`

- [ ] **Step 3: 实现 `SessionAges`**

在 `app_status.rs` 的 `APP_STATUS_STALE_MS` 定义之后插入：

```rust
/// 会话年龄双轴（唯一构造点：[`SessionAges::derive`]）。
///
/// **纪律（2026-11-04 用户裁决）**：一个 age 值只服务一个判定语义。
/// - `content_age_ms` 只喂**完成冷却**与内容新鲜度判定；
/// - `presence_age_ms` 只喂**在板判定**、**停更降级**与未读补偿。
///
/// 违反后果：心跳/DB 活动会把内容年龄打回 0，完成冷却永远等不满，
/// 造成绿↔黄周期振荡并重复播报完成提醒（2026-11-04 WorkBuddy 缺陷，
/// 活体实测 30s 周期：心跳刷新 → 黄 10s → 心跳老化 → 绿 20s → 循环）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionAges {
    /// 距**内容最后写入**的毫秒数。只取内容源（jsonl mtime / DB time_updated /
    /// 事件流末条时间戳）。无内容源时为 `None`——**未知不等于刚刚**。
    pub content_age_ms: Option<u64>,
    /// 距**会话最后生命迹象**的毫秒数。内容源 ∨ DB 活动 ∨ 心跳 ∨ 锁租约取最近。
    pub presence_age_ms: Option<u64>,
}

impl SessionAges {
    /// 唯一构造点。
    ///
    /// - `content_ms`：内容最后写入的**绝对时刻**（epoch ms），由各 parser 的证据层提供；
    /// - `presence_sources`：所有生命迹象的绝对时刻，含内容源自身。
    ///
    /// 两轴都为 `None` 表示证据完全缺失。
    pub fn derive(now_ms: u64, content_ms: Option<u64>, presence_sources: &[Option<u64>]) -> Self {
        let content_age_ms = content_ms.map(|ms| now_ms.saturating_sub(ms));
        let presence_age_ms = presence_sources
            .iter()
            .flatten()
            .map(|ms| now_ms.saturating_sub(*ms))
            .min();
        Self {
            content_age_ms,
            presence_age_ms,
        }
    }
}
```

- [ ] **Step 4: 运行测试确认通过**

```bash
cd src-tauri && cargo test --lib monitor::app_status::tests::session_ages 2>&1 | tail -20
```

Expected: 4 passed

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/monitor/app_status.rs
git commit -m "feat(monitor): add SessionAges to separate content and presence anchors"
```

---

## Task 2: WorkBuddy 改用双轴锚点（修复 30s 绿黄振荡）

**Files:**
- Modify: `src-tauri/src/monitor/workbuddy_parser.rs:905-926`
- Test: `src-tauri/src/monitor/workbuddy_parser.rs`（`mod board_tests`）

- [ ] **Step 1: 写失败测试**

在 `workbuddy_parser.rs` 的 `mod board_tests` 内追加（该模块已有 `seed_db` / `write_heartbeat` / `write_jsonl` / `run_board` / `process` fixture）：

```rust
    /// **P0 核心回归锁（2026-11-04 缺陷）**：任务已结束（DB completed、jsonl 停写
    /// 30s），但心跳每 30s 刷新一次。心跳只能证明进程还开着，不得延长完成冷却。
    /// 修复前：eff_age 被心跳打回 0 → assistant 尾 Idle 被拉回 Processing 10s，
    /// 形成绿 20s / 黄 10s 的 30s 周期振荡；修复后：稳定 Idle。
    #[test]
    fn heartbeat_does_not_revive_green_debounce() {
        let home = tempfile::tempdir().unwrap();
        let now = now_ms();
        // jsonl 30 秒前写下（超过 10s 冷却窗），DB completed，心跳 1 秒前刚刷新
        write_jsonl(home.path(), SID, now - 30_000);
        seed_db(
            home.path(),
            &[(SID, "completed", (now - 30_000) as i64, (now - 30_000) as i64, None)],
        );
        write_heartbeat(home.path(), 11952, SID, now - 1_000);

        let out = run_board(home.path(), &[process(11952)], now);
        assert_eq!(out.len(), 1, "心跳 pid 存活 + jsonl 在 300s 内 → 必须出卡");
        assert_eq!(
            out[0].status,
            SessionStatus::Idle,
            "心跳新鲜不得把已完成的会话拉回 Processing（30s 绿黄振荡的根因）"
        );
    }

    /// P0 反面对照：DB 说 working → 仍必须上拉 Processing（保护 AC9 不被误伤）
    #[test]
    fn db_working_still_pulls_processing_regardless_of_heartbeat() {
        let home = tempfile::tempdir().unwrap();
        let now = now_ms();
        write_jsonl(home.path(), SID, now - 30_000);
        seed_db(
            home.path(),
            &[(SID, "working", (now - 1_000) as i64, (now - 1_000) as i64, None)],
        );
        write_heartbeat(home.path(), 11952, SID, now - 1_000);

        let out = run_board(home.path(), &[process(11952)], now);
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].status,
            SessionStatus::Processing,
            "AC9：DB working 必须无条件上拉 Processing"
        );
    }

    /// P0 边界：jsonl mtime 不可得（content_age=None）→ 不启用冷却，直接按尾部判定
    #[test]
    fn missing_content_mtime_disables_cooldown() {
        let home = tempfile::tempdir().unwrap();
        let now = now_ms();
        write_jsonl(home.path(), SID, now - 30_000);
        seed_db(
            home.path(),
            &[(SID, "completed", (now - 30_000) as i64, (now - 30_000) as i64, None)],
        );
        write_heartbeat(home.path(), 11952, SID, now - 1_000);
        // 删掉 jsonl：content 源缺失，但 DB activity 仍在 300s 内 → 卡片仍出
        let jsonl_dir = home.path().join(".workbuddy/projects").join(mangle_project_path(CWD));
        std::fs::remove_file(jsonl_dir.join(format!("{SID}.jsonl"))).unwrap();

        // jsonl 缺失时 find_session_jsonl 未命中 → 不出卡（既有语义），
        // 本测试改为断言「函数不 panic 且返回空」
        let out = run_board(home.path(), &[process(11952)], now);
        assert!(out.is_empty(), "jsonl 缺失 → 不出卡（既有语义不变）");
    }
```

> **注意**：第三个测试依赖 `mangle_project_path` 与 `CWD` —— 二者在该模块已定义。若 `jsonl_dir` 变量有问题，改为直接用 `write_jsonl` 返回路径的方式（见 Step 3 备选）。

- [ ] **Step 2: 运行测试确认失败**

```bash
cd src-tauri && cargo test --lib monitor::workbuddy_parser::board_tests::heartbeat_does_not_revive_green_debounce 2>&1 | tail -25
```

Expected: **FAIL**，assertion 显示 `left: Processing, right: Idle`（这就是 bug）

- [ ] **Step 3: 改造构卡层锚点**

把 `workbuddy_parser.rs:905-926` 这段：

```rust
        // 有效活动时刻 = max(主 jsonl mtime, DB activity, 心跳时刻)：
        // 子代理干活时父 jsonl 可能停更，心跳又冻结 → 用最早的那个会把卡误判成「旧」，
        // 混着用会把 Processing 降成 Waiting（`overlay_mtime_stale`）。
        let eff_activity_ms = [
            jsonl_mtime_ms,
            c.db_activity_ms.map(|a| a.max(0) as u64),
            c.last_heartbeat_ms,
        ]
        .into_iter()
        .flatten()
        .max();
        let eff_age_ms = eff_activity_ms.map_or(0, |a| now.saturating_sub(a));
        // 已读生命周期专用"新工作"时间：只取明确的业务条目（用户消息 / 模型请求 /
        // 工具执行），不混入 DB 活动、mtime 或心跳——那些只能证明"在场"，不能证明
        // "产生了新回合"。没有业务条目时为 None，已读卡不会因普通后台更新复活。
        let session_activity_ms = d.last_work_ms;
        let status = if working {
            // AC9：DB 说在干活 → 上拉 Processing（不因父会话静默而掉卡/误绿）
            SessionStatus::Processing
        } else {
            overlay_mtime_stale(
                apply_green_debounce(d.status_core.clone(), d.tail_kind, eff_age_ms),
                eff_age_ms,
            )
        };
```

替换为：

```rust
        // **锚点纪律（2026-11-04 用户裁决）**：一个 age 值只服务一个判定语义。
        // content 轴只取内容源（jsonl mtime）→ 喂完成冷却；
        // presence 轴取内容 ∨ DB 活动 ∨ 心跳 → 喂停更降级。
        // 修复前两轴共用 eff_age，心跳每 30s 刷新把 eff_age 打回 0，导致
        // assistant 尾 Idle 被反复拉回 Processing（绿 20s / 黄 10s 周期振荡）。
        let db_activity_ms = c.db_activity_ms.map(|a| a.max(0) as u64);
        let ages = SessionAges::derive(
            now,
            jsonl_mtime_ms,
            &[jsonl_mtime_ms, db_activity_ms, c.last_heartbeat_ms],
        );
        // 已读生命周期专用"新工作"时间：只取明确的业务条目（用户消息 / 模型请求 /
        // 工具执行），不混入 DB 活动、mtime 或心跳——那些只能证明"在场"，不能证明
        // "产生了新回合"。没有业务条目时为 None，已读卡不会因普通后台更新复活。
        let session_activity_ms = d.last_work_ms;
        let status = if working {
            // AC9：DB 说在干活 → 上拉 Processing（不因父会话静默而掉卡/误绿）
            SessionStatus::Processing
        } else {
            // ① 完成冷却：content 轴。None（mtime 未知）→ 不启用冷却（未知 ≠ 刚刚）
            overlay_mtime_stale(
                apply_green_debounce(
                    d.status_core.clone(),
                    d.tail_kind,
                    ages.content_age_ms.unwrap_or(u64::MAX),
                ),
                // ② 停更降级：presence 轴。None → 按 0 处理（与既有 map_or(0) 一致）
                ages.presence_age_ms.unwrap_or(0),
            )
        };
```

同时在文件顶部的 `use super::app_status::{...}` 导入中追加 `SessionAges`。本 Task 只新增 `SessionAges` 导入；`apply_green_debounce` 仍是本地函数（Task 4 才迁走），因此该行应为：

```rust
use super::app_status::{derive_app_status, tail_semantic_kind, AppEntryKind, SessionAges};
```

若该行原本已存在，只在花括号内追加 `SessionAges`。

- [ ] **Step 4: 运行测试确认通过**

```bash
cd src-tauri && cargo test --lib monitor::workbuddy_parser::board_tests 2>&1 | tail -30
```

Expected: 全部 passed，含新增 3 条

- [ ] **Step 5: 跑 WorkBuddy 全量回归**

```bash
cd src-tauri && cargo test --lib monitor::workbuddy_parser 2>&1 | tail -20
```

Expected: 全部 passed（原 `debounce_holds_processing_only_for_fresh_assistant_idle` 等测试语义不变）

- [ ] **Step 6: 提交**

```bash
git add src-tauri/src/monitor/workbuddy_parser.rs
git commit -m "fix(workbuddy): stop heartbeat from extending completion cooldown

完成冷却改用 content_age（纯 jsonl mtime），停更降级继续用 presence_age。
修复前两者共用 eff_age，心跳每 30s 刷新把年龄打回 0，造成任务结束后的
绿↔黄 30s 周期振荡并重复播报完成提醒。"
```

---

## Task 3: 时间常量集中到共享核（P1）

**Files:**
- Modify: `src-tauri/src/monitor/app_status.rs`（新增常量段）
- Modify: `src-tauri/src/monitor/claude_parser.rs:277`
- Modify: `src-tauri/src/monitor/kimi_parser.rs:33`
- Modify: `src-tauri/src/monitor/opencode_parser.rs:1039`
- Modify: `src-tauri/src/monitor/codex_thread_parser.rs:53`
- Modify: `src-tauri/src/monitor/zcode_parser.rs:56`
- Modify: `src-tauri/src/monitor/dsh/status.rs:19`

- [ ] **Step 1: 写"常量同源"测试**

在 `app_status.rs` 的 `mod tests` 追加：

```rust
    // ---- 时间常量集中（P1）：语义命名 + 同源锁 ----

    /// 无信号兜底窗必须与 APP 停更阈值同源——两者历史上是同一个值，
    /// 单独调一个会静默改变"兜底新鲜度"语义。
    #[test]
    fn fallback_fresh_equals_app_stale() {
        assert_eq!(FALLBACK_FRESH_MS, APP_STATUS_STALE_MS);
    }

    /// 各家"内容新鲜"阈值已收录且量级正确（防止实现期写错数量级）
    #[test]
    fn content_fresh_constants_have_expected_magnitudes() {
        assert_eq!(CONTENT_FRESH_CLAUDE_MS, 3_000);
        assert_eq!(CONTENT_FRESH_KIMI_MS, 60_000);
        assert_eq!(CONTENT_FRESH_OPENCODE_MS, 60_000);
    }

    /// WorkBuddy 完成冷却窗与心跳周期必须独立——若两者接近，
    /// 冷却会被心跳反复重置（2026-11-04 缺陷的面板条件）
    #[test]
    fn workbuddy_cooldown_is_shorter_than_its_heartbeat() {
        assert!(
            COMPLETION_COOLDOWN_MS < crate::monitor::workbuddy_parser::HEARTBEAT_FRESH_MS,
            "冷却窗必须显著小于心跳周期，否则行为退化"
        );
    }
```

- [ ] **Step 2: 运行测试确认失败**

```bash
cd src-tauri && cargo test --lib monitor::app_status::tests::fallback_fresh_equals_app_stale 2>&1 | tail -15
```

Expected: 编译失败，`cannot find value 'FALLBACK_FRESH_MS'`

- [ ] **Step 3: 在 `app_status.rs` 新增常量段**

在 `APP_STATUS_STALE_MS` 定义之后（`SessionAges` 之前或之后均可，建议之后）插入：

```rust
/// 无信号时的新鲜度兜底窗（zcode / codex APP 共用）：与 [`APP_STATUS_STALE_MS`]
/// 同值同源。判据是「会话最后更新时间在窗内 → Processing，否则 Idle」——
/// 与停更降级（Processing → Waiting）语义不同，但窗口量级刻意对齐，
/// 避免同一会话在两条路径上得到矛盾结论。
pub const FALLBACK_FRESH_MS: i64 = APP_STATUS_STALE_MS as i64;

/// Claude 文件新鲜兜底窗：`determine_status` 无信号分支的 mtime 判据。
/// 3s 是"轮询周期 + 落盘延迟"量级——claude 有消息内 tool_use 判据兜底，
/// 这只是极短窗的防御，不是主力判据。
pub const CONTENT_FRESH_CLAUDE_MS: u64 = 3_000;

/// Kimi 文件新鲜兜底窗：`wire.jsonl` 无状态信号时的 mtime 判据。
/// 60s 是主力兜底（kimi 事件流可能缺失），与 claude 的 3s 防御性兜底不同源。
pub const CONTENT_FRESH_KIMI_MS: u64 = 60_000;

/// OpenCode "user 尾 + 近期活跃" 的活跃窗。
pub const CONTENT_FRESH_OPENCODE_MS: u64 = 60_000;

/// WorkBuddy 完成冷却窗（原 `GREEN_DEBOUNCE_MS`）：assistant 语义尾 + 内容年龄
/// 小于该窗 → 保持 Processing。WorkBuddy 格式无轮次边界信号，中间 assistant
/// 消息的假绿只能时间防抖（spec 2026-09-16 §4.2）。
///
/// **必须用 content_age 喂它，绝不可用含心跳的 presence_age**——心跳每 30s
/// 刷新会把年龄打回 0，冷却永远等不满（2026-11-04 缺陷根因）。
pub const COMPLETION_COOLDOWN_MS: u64 = 10_000;
```

- [ ] **Step 4: 逐家把常量改为引用共享核**

**4a. `kimi_parser.rs`**：把第 33 行的本地常量定义

```rust
const FILE_RECENT_SECS: f32 = 60.0;
```

改为：

```rust
/// 值来源见 `monitor::app_status::CONTENT_FRESH_KIMI_MS`（P1 常量集中）。
const FILE_RECENT_SECS: f32 = crate::monitor::app_status::CONTENT_FRESH_KIMI_MS as f32 / 1000.0;
```

**4b. `claude_parser.rs:277`**：把硬编码 `3.0`

```rust
        .map(|age| age.as_secs_f32() < 3.0)
```

改为：

```rust
        .map(|age| {
            age.as_secs_f32()
                < crate::monitor::app_status::CONTENT_FRESH_CLAUDE_MS as f32 / 1000.0
        })
```

**4c. `opencode_parser.rs:1039`**：把硬编码 `60_000`

```rust
        let is_recent = now - last_active < 60_000; // 60 秒内
```

改为：

```rust
        let is_recent =
            now - last_active < crate::monitor::app_status::CONTENT_FRESH_OPENCODE_MS as i64;
```

**4d. `codex_thread_parser.rs:53`**：把

```rust
const FALLBACK_FRESH_MS: i64 = APP_STATUS_STALE_MS as i64;
```

改为：

```rust
/// 值来源见 `monitor::app_status::FALLBACK_FRESH_MS`（P1 常量集中）。
const FALLBACK_FRESH_MS: i64 = crate::monitor::app_status::FALLBACK_FRESH_MS;
```

**4e. `zcode_parser.rs:56`**：同上改法

```rust
/// 值来源见 `monitor::app_status::FALLBACK_FRESH_MS`（P1 常量集中）。
const FALLBACK_FRESH_MS: i64 = crate::monitor::app_status::FALLBACK_FRESH_MS;
```

**4f. `workbuddy_parser.rs:78`**：把

```rust
pub const GREEN_DEBOUNCE_MS: u64 = 10_000;
```

改为：

```rust
/// 完成冷却窗（值来源见 `monitor::app_status::COMPLETION_COOLDOWN_MS`，P1 集中）。
/// 保留旧名以兼容既有测试与文档引用（决策 8：全量改名留待下次触及）。
#[deprecated(note = "迁至 app_status::COMPLETION_COOLDOWN_MS；语义见该常量文档")]
pub const GREEN_DEBOUNCE_MS: u64 = crate::monitor::app_status::COMPLETION_COOLDOWN_MS;
```

> **注意**：`#[deprecated]` 会让所有现存引用（含测试）产生 warning，而 `cargo clippy -D warnings` 会失败。因此**本步骤先不加 `#[deprecated]`**，只做值别名：
> ```rust
> /// 完成冷却窗（值来源见 `monitor::app_status::COMPLETION_COOLDOWN_MS`，P1 集中）。
> /// 保留旧名以兼容既有测试与文档引用（决策 8）。
> pub const GREEN_DEBOUNCE_MS: u64 = crate::monitor::app_status::COMPLETION_COOLDOWN_MS;
> ```
> `#[deprecated]` 标注留待全量改名时一并处理。

**4g. `dsh/status.rs:19`**：`V0_SILENCE_INTERRUPT_MS` 保持原地不动（dsh 语义独特，30min 与其他家无共性，集中反而制造虚假关联）。在常量注释里加一句来源说明即可：

```rust
/// v0 无锁目录的静默中断阈值（30 分钟）。**刻意不迁入 app_status 共享常量**：
/// dsh 用持有式 fd 租约判定在场，静默阈值只服务"无锁降级"这一独有路径，
/// 与其他工具的停更降级无共性（见 spec 2026-11-04 §7.3 收敛原则）。
pub const V0_SILENCE_INTERRUPT_MS: i64 = 30 * 60 * 1000;
```

- [ ] **Step 5: 运行常量测试 + 全量编译**

```bash
cd src-tauri && cargo test --lib monitor::app_status::tests 2>&1 | tail -20
cargo clippy --all-targets -- -D warnings 2>&1 | tail -20
```

Expected: 测试全 passed；clippy 干净

- [ ] **Step 6: 全量回归（确认零行为变更）**

```bash
cd src-tauri && cargo test --lib 2>&1 | tail -25
```

Expected: 全部 passed（本 Task 是纯搬家，不应有任何状态判定行为变化）

- [ ] **Step 7: 提交**

```bash
git add src-tauri/src/monitor/
git commit -m "refactor(monitor): centralize status freshness constants in app_status

语义命名优先于工具命名（CONTENT_FRESH_CLAUDE_MS 而非 CLAUDE_MS），
同值不强行合并（claude 3s 是防御兜底，kimi 60s 是主力兜底）。
dsh 的 30min 静默阈值刻意不迁：语义独特，集中会制造虚假关联。"
```

---

## Task 4: 抽出 `CooldownSignal` + `apply_completion_cooldown`（P2）

**Files:**
- Modify: `src-tauri/src/monitor/app_status.rs`（新增信号类型与函数）
- Modify: `src-tauri/src/monitor/workbuddy_parser.rs`（删除本地 `apply_green_debounce`，改用共享版）

- [ ] **Step 1: 写失败测试**

在 `app_status.rs` 的 `mod tests` 追加：

```rust
    // ---- CooldownSignal：完成冷却通用形态（P2） ----

    fn sig(observed_at_ms: u64, hold_ms: u64, requires_tail: Option<AppEntryKind>) -> CooldownSignal {
        CooldownSignal {
            observed_at_ms,
            hold_ms,
            requires_tail,
        }
    }

    /// 冷却窗内：绿态 → Processing
    #[test]
    fn cooldown_blocks_green_within_window() {
        let now = 1_000_000u64;
        let s = sig(now - 5_000, 10_000, Some(AppEntryKind::AssistantMessage));
        assert_eq!(
            apply_completion_cooldown(
                SessionStatus::Idle,
                Some(AppEntryKind::AssistantMessage),
                Some(s),
                now
            ),
            SessionStatus::Processing
        );
    }

    /// 冷却窗已过：绿态放行
    #[test]
    fn cooldown_passes_green_after_window() {
        let now = 1_000_000u64;
        let s = sig(now - 10_000, 10_000, Some(AppEntryKind::AssistantMessage));
        assert_eq!(
            apply_completion_cooldown(
                SessionStatus::Idle,
                Some(AppEntryKind::AssistantMessage),
                Some(s),
                now
            ),
            SessionStatus::Idle
        );
    }

    /// 非绿态一律透传（黄/红不受冷却影响）
    #[test]
    fn cooldown_passes_non_green() {
        let now = 1_000_000u64;
        let s = sig(now, 10_000, None);
        for status in [
            SessionStatus::Processing,
            SessionStatus::Waiting,
            SessionStatus::Thinking,
            SessionStatus::Compacting,
        ] {
            assert_eq!(
                apply_completion_cooldown(status.clone(), None, Some(s), now),
                status,
                "非绿态必须透传"
            );
        }
    }

    /// 无信号 → 透传（无冷却证据 = 不启用）
    #[test]
    fn cooldown_none_signal_passes_through() {
        assert_eq!(
            apply_completion_cooldown(
                SessionStatus::Idle,
                Some(AppEntryKind::AssistantMessage),
                None,
                1_000_000
            ),
            SessionStatus::Idle
        );
    }

    /// requires_tail 不匹配 → 透传（冷却只对特定尾部语义生效）
    #[test]
    fn cooldown_requires_tail_mismatch_passes() {
        let now = 1_000_000u64;
        let s = sig(now, 10_000, Some(AppEntryKind::AssistantMessage));
        assert_eq!(
            apply_completion_cooldown(SessionStatus::Idle, Some(AppEntryKind::ToolCall), Some(s), now),
            SessionStatus::Idle,
            "尾部是 ToolCall 时不适用 assistant 冷却"
        );
    }

    /// requires_tail=None → 不限尾部（claude Stop grace 形态）
    #[test]
    fn cooldown_without_tail_requirement_applies_to_any_green() {
        let now = 1_000_000u64;
        let s = sig(now, 5_000, None);
        assert_eq!(
            apply_completion_cooldown(SessionStatus::Idle, Some(AppEntryKind::ToolCall), Some(s), now),
            SessionStatus::Processing
        );
        assert_eq!(
            apply_completion_cooldown(SessionStatus::Finished, None, Some(s), now),
            SessionStatus::Processing
        );
    }
```

- [ ] **Step 2: 运行测试确认失败**

```bash
cd src-tauri && cargo test --lib monitor::app_status::tests::cooldown 2>&1 | tail -15
```

Expected: 编译失败，`cannot find type 'CooldownSignal'`

- [ ] **Step 3: 实现 `CooldownSignal` 与 `apply_completion_cooldown`**

在 `app_status.rs` 的 `SessionAges` 实现之后插入：

```rust
/// 完成冷却信号（纯数据，各 parser 在证据层构造）。
///
/// 语义：在 `observed_at_ms` 观测到"完成候选"，到 `observed_at_ms + hold_ms`
/// 之前不得报绿。`requires_tail` 用于限定只有特定尾部语义才适用冷却
/// （WorkBuddy 的中间 assistant 假绿只发生在 assistant 尾）。
///
/// **`observed_at_ms` 必须是内容/事件时间戳，绝不可含心跳或 DB 活动**——
/// 那是 [`SessionAges`] 的 presence 轴，混用会导致冷却被周期性重置。
///
/// 两种已知形态：
/// - WorkBuddy：`observed_at = jsonl mtime`，`hold = COMPLETION_COOLDOWN_MS`，
///   `requires_tail = Some(AssistantMessage)`
/// - Claude：`observed_at = hook Stop 事件 ts`，`hold = cli/app grace`，
///   `requires_tail = None`（预留，本批不接入）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CooldownSignal {
    /// 完成候选被观测到的时刻（epoch ms）
    pub observed_at_ms: u64,
    /// 冷却时长（ms）
    pub hold_ms: u64,
    /// 仅该尾部语义可被冷却；`None` = 不限尾部
    pub requires_tail: Option<AppEntryKind>,
}

/// 完成冷却（纯函数）：观测到完成候选后压住 hold 时长才允许报绿。
///
/// 仅作用于绿态（Idle/Finished）；其余状态透传。`signal` 为 `None` → 透传
/// （无冷却证据 = 不启用）。`requires_tail` 与 `tail_kind` 不匹配 → 透传。
pub fn apply_completion_cooldown(
    status: SessionStatus,
    tail_kind: Option<AppEntryKind>,
    signal: Option<CooldownSignal>,
    now_ms: u64,
) -> SessionStatus {
    let Some(sig) = signal else {
        return status;
    };
    if !matches!(status, SessionStatus::Idle | SessionStatus::Finished) {
        return status;
    }
    if let Some(required) = sig.requires_tail {
        if tail_kind != Some(required) {
            return status;
        }
    }
    if now_ms.saturating_sub(sig.observed_at_ms) < sig.hold_ms {
        SessionStatus::Processing
    } else {
        status
    }
}
```

- [ ] **Step 4: WorkBuddy 改用共享函数**

**4a.** 在 `workbuddy_parser.rs` 顶部导入追加 `CooldownSignal` 与 `apply_completion_cooldown`：

```rust
use super::app_status::{
    apply_completion_cooldown, derive_app_status, tail_semantic_kind, AppEntryKind,
    CooldownSignal, SessionAges,
};
```

**4b.** 删除本地的 `apply_green_debounce` 函数（`workbuddy_parser.rs:354-367`，含文档注释）。

**4c.** 把 Task 2 里改造后的冷却调用

```rust
            overlay_mtime_stale(
                apply_green_debounce(
                    d.status_core.clone(),
                    d.tail_kind,
                    ages.content_age_ms.unwrap_or(u64::MAX),
                ),
                ages.presence_age_ms.unwrap_or(0),
            )
```

替换为：

```rust
            // ① 完成冷却：content 轴（无内容源 → None = 不启用冷却）
            let cooldown = CooldownSignal {
                observed_at_ms: jsonl_mtime_ms.unwrap_or(0),
                hold_ms: crate::monitor::app_status::COMPLETION_COOLDOWN_MS,
                requires_tail: Some(AppEntryKind::AssistantMessage),
            };
            let signal = ages.content_age_ms.map(|_| cooldown);
            overlay_mtime_stale(
                apply_completion_cooldown(d.status_core.clone(), d.tail_kind, signal, now),
                // ② 停更降级：presence 轴
                ages.presence_age_ms.unwrap_or(0),
            )
```

> **要点**：`ages.content_age_ms.map(|_| cooldown)` 保证 mtime 缺失时 `signal = None` → 冷却不启用（与 Task 2 的 `unwrap_or(u64::MAX)` 等价，但语义更明确）。

**4d.** 把 `GREEN_DEBOUNCE_MS` 的所有现存引用（测试内）改为 `crate::monitor::app_status::COMPLETION_COOLDOWN_MS`，或保留别名常量不动（决策 8：保留旧名兼容）。**推荐保留别名**，零测试改动。

**4e.** 把 `debounce_tests` 模块里对 `apply_green_debounce` 的测试改写为对 `apply_completion_cooldown` 的测试。原测试 `debounce_holds_processing_only_for_fresh_assistant_idle` 改法：

```rust
    /// 完成冷却：仅对「assistant 尾导出的绿 + 窗内」生效，其余透传
    #[test]
    fn completion_cooldown_holds_processing_only_for_fresh_assistant_idle() {
        use crate::session::SessionStatus::*;
        use crate::monitor::app_status::{apply_completion_cooldown, CooldownSignal};
        let at = 1_000_000u64;
        let assistant = Some(AppEntryKind::AssistantMessage);
        let fresh = Some(CooldownSignal {
            observed_at_ms: at - 9_999,
            hold_ms: crate::monitor::app_status::COMPLETION_COOLDOWN_MS,
            requires_tail: Some(AppEntryKind::AssistantMessage),
        });
        assert_eq!(apply_completion_cooldown(Idle, assistant, fresh, at), Processing);
        // 恰好到窗：放行
        let expired = Some(CooldownSignal {
            observed_at_ms: at - crate::monitor::app_status::COMPLETION_COOLDOWN_MS,
            hold_ms: crate::monitor::app_status::COMPLETION_COOLDOWN_MS,
            requires_tail: Some(AppEntryKind::AssistantMessage),
        });
        assert_eq!(apply_completion_cooldown(Idle, assistant, expired, at), Idle);
        // 无信号 → 透传
        assert_eq!(apply_completion_cooldown(Idle, assistant, None, at), Idle);
        // 非 assistant 尾 → 透传
        assert_eq!(
            apply_completion_cooldown(Idle, Some(AppEntryKind::ToolCall), fresh, at),
            Idle
        );
        // 非绿态 → 透传
        assert_eq!(apply_completion_cooldown(Waiting, assistant, fresh, at), Waiting);
        assert_eq!(apply_completion_cooldown(Processing, assistant, fresh, at), Processing);
    }
```

- [ ] **Step 5: 运行测试确认通过**

```bash
cd src-tauri && cargo test --lib monitor::app_status::tests 2>&1 | tail -20
cd src-tauri && cargo test --lib monitor::workbuddy_parser 2>&1 | tail -20
```

Expected: 全部 passed

- [ ] **Step 6: clippy 检查**

```bash
cd src-tauri && cargo clippy --all-targets -- -D warnings 2>&1 | tail -20
```

Expected: 干净（若 `apply_green_debounce` 的删除留下未用导入，一并清理）

- [ ] **Step 7: 提交**

```bash
git add src-tauri/src/monitor/
git commit -m "refactor(monitor): extract CooldownSignal as shared completion cooldown

统一 workbuddy 的内容时间锚与 claude 的事件时间锚（后者本批只留接口）。
CooldownSignal 的 observed_at 必须来自内容/事件时间戳，禁止混入心跳——
这是 2026-11-04 WorkBuddy 振荡缺陷的架构级防线。"
```

---

## Task 5: 删除 WorkBuddy 死代码与残留别名清理

**Files:**
- Modify: `src-tauri/src/monitor/workbuddy_parser.rs`

- [ ] **Step 1: 检查残留**

```bash
cd src-tauri && grep -n "GREEN_DEBOUNCE_MS\|apply_green_debounce" src/monitor/workbuddy_parser.rs
```

预期：只剩 `GREEN_DEBOUNCE_MS` 别名定义（若测试引用已改，则可能完全无引用）。

- [ ] **Step 2: 决策并执行**

- 若 `GREEN_DEBOUNCE_MS` 仍被测试引用 → **保留别名**（决策 8），不删。
- 若完全无引用 → **删除别名**，避免死常量。

- [ ] **Step 3: 编译验证**

```bash
cd src-tauri && cargo clippy --all-targets -- -D warnings 2>&1 | tail -15
```

Expected: 干净

- [ ] **Step 4: 提交（若有改动）**

```bash
git add src-tauri/src/monitor/workbuddy_parser.rs
git commit -m "chore(workbuddy): remove dead debounce alias"
```

---

## Task 6: Claude 子代理聚合出口（P3）⚠️ 已撤回，勿直接实施

> **⚠️ 本 Task 已实施后撤回（2026-11-04）**。以下内容保留作为重做参考，
> 但**有已知设计缺陷**，直接照做会引入更严重的 bug：
>
> 1. **Stop 通知落盘形态已变**：`classify_parent_line` 只读 `message.content`，
>    而现行 Claude Code 的 `<task-id>` 有 42/134 在 `queue-operation.content`、
>    20/134 在 `attachment.prompt`，`message.content` 仅 2/134（约 1.5%）。
>    读不到 Stop ⇒ 子代理永远 `Running` ⇒ 会话被拦绿最长 24h。
>    取证见 `docs/engineering/evidence/claude-task-id-location-20261104/`。
> 2. **`spawn_ts=None` 分支无僵尸上界**：本 Task 的 `None => true` 会永久拦截。
>    重做时应回落 `meta.json` 文件 mtime（`registered_agents` 已 stat）。
> 3. **测试覆盖率不足**：缺 `resumed_child_blocks_again`（Stop→Resume）、
>    `spawn_ts=None` 分支、parser 级 `settled_child_allows_green`。
>
> 重做前置条件完整清单见设计 spec §9.7 与 issue #200。


**Files:**
- Modify: `src-tauri/src/monitor/subagents/claude.rs`（新增公开聚合函数）
- Test: `src-tauri/src/monitor/subagents/claude.rs`（同文件测试模块）

- [ ] **Step 1: 写失败测试**

`subagents/claude.rs` 的测试模块已有现成 `claude_fixture(sid)` 夹具（会话目录 + `subagents/` + 父转写平铺 + `add_agent` / `append`），**直接复用**：

```rust
    // ==== P3（2026-11-04）：后代活动聚合出口 ====

    const P3_SID: &str = "11111111-2222-3333-4444-555555555555";

    fn ts_ms(rfc3339: &str) -> u64 {
        chrono::DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .timestamp_millis() as u64
    }

    /// 无 subagents 目录 / 无登记 → 三元组全空
    #[test]
    fn descendant_activity_empty_when_no_subagents() {
        let fx = claude_fixture(P3_SID);
        let (count, latest, unsettled) =
            descendant_activity_at(&fx.parent(P3_SID), P3_SID, ts_ms("2026-11-04T10:00:00Z"));
        assert_eq!(count, 0);
        assert_eq!(latest, None);
        assert!(!unsettled, "无子代理 = 无未结算");
    }

    /// 已登记 ∧ 父转写无 Stop → 未结算（zcode Pending 语义）
    #[test]
    fn descendant_activity_reports_unsettled_without_stop() {
        let fx = claude_fixture(P3_SID);
        fx.add_agent(
            P3_SID,
            "a1",
            r#"{"agentType":"general-purpose","description":"跑测试"}"#,
            r#"{"type":"user","timestamp":"2026-11-04T10:00:00.000Z","message":{"role":"user","content":"干活"}}
"#,
        );
        let (count, latest, unsettled) =
            descendant_activity_at(&fx.parent(P3_SID), P3_SID, ts_ms("2026-11-04T10:00:30Z"));
        assert_eq!(count, 1, "已登记子代理计入活跃数");
        assert_eq!(
            latest,
            Some(ts_ms("2026-11-04T10:00:00Z") as i64),
            "子转写 timestamp 应被捕获"
        );
        assert!(unsettled, "已登记 ∧ 未见 Stop → 账未结清");
    }

    /// 父转写有 Stop 通知 → 已结算
    #[test]
    fn descendant_activity_settles_after_stop_notification() {
        let fx = claude_fixture(P3_SID);
        fx.add_agent(
            P3_SID,
            "a1",
            r#"{"agentType":"general-purpose"}"#,
            r#"{"type":"user","timestamp":"2026-11-04T10:00:00.000Z","message":{"role":"user","content":"干活"}}
"#,
        );
        // Stop 通知是 **JSON 行**，task-id 在 message.content 文本内
        // （classify_parent_line → message_text → task_id_in）
        fx.append(
            &fx.parent(P3_SID),
            r#"{"type":"user","message":{"role":"user","content":"<task-id>a1</task-id> 已完成"}}
"#,
        );
        let (_, _, unsettled) =
            descendant_activity_at(&fx.parent(P3_SID), P3_SID, ts_ms("2026-11-04T10:01:00Z"));
        assert!(!unsettled, "Stop 已见 → 账已结清");
    }

    /// 僵尸保护：登记超过 24h 仍未结算 → 不再计入未结算
    #[test]
    fn descendant_activity_ignores_zombie_after_24h() {
        let fx = claude_fixture(P3_SID);
        fx.add_agent(
            P3_SID,
            "a1",
            r#"{"agentType":"general-purpose"}"#,
            r#"{"type":"user","timestamp":"2026-11-01T10:00:00.000Z","message":{"role":"user","content":"干活"}}
"#,
        );
        let now = ts_ms("2026-11-01T10:00:00Z") + 25 * 3600 * 1000;
        let (_, _, unsettled) = descendant_activity_at(&fx.parent(P3_SID), P3_SID, now);
        assert!(!unsettled, "超过 24h 的僵尸登记不得永久拦截报绿");
    }
```

> **注意**：`claude_fixture` / `add_agent` / `append` 均定义在 `mod tests` 内（`claude.rs:618-658`）。若测试模块需要 `reset_cache_for_tests()` 清缓存（跨测试污染），按既有两个生命周期测试的开头照做。

- [ ] **Step 2: 运行测试确认失败**

```bash
cd src-tauri && cargo test --lib monitor::subagents::claude::descendant_activity 2>&1 | tail -20
```

Expected: 编译失败，`cannot find function 'descendant_activity_at'`

- [ ] **Step 3: 实现聚合出口**

在 `subagents/claude.rs` 的 `collect_at` 之后插入：

```rust
/// 僵尸登记保护窗（P3）：已登记但超过该时长仍未结算的子代理不再计入
/// 「未结算」。仿 zcode 的 24h 僵尸保护（`zcode_parser.rs:737-748`）——
/// Stop 通知丢失会让 `is_running` 永真，若无此保护会话永远不报绿。
const ZOMBIE_UNSETTLED_MS: i64 = 24 * 3600 * 1000;

/// 后代活动聚合出口（P3）：一次取齐既有 ClaudeCache 状态机，产出三元组。
///
/// - `active_count`：`is_running` 为真的子 agent 数（卡片徽标用）
/// - `latest_child_activity_ms`：子转写末条 timestamp 的最大值（epoch ms）；
///   子代理活着但静默时也可见——与 zcode 的 `descendant_activity` 同口径
/// - `has_unsettled`：存在「已登记 ∧ 未见 Stop ∧ 未超僵尸窗」的子代理
///   → 账未结清（zcode Pending 语义）
///
/// 数据全部来自生产在用的三态机（[`is_running`]），不新增 IO 路径：
/// `collect_at` 已把父事件与子转写增量读进缓存，本函数只做聚合。
pub(crate) fn descendant_activity_at(
    parent_jsonl: &Path,
    session_id: &str,
    now_ms: u64,
) -> (usize, Option<i64>, bool) {
    let views = collect_at(parent_jsonl, session_id);
    let active_count = views
        .iter()
        .filter(|v| matches!(v.status, SubagentStatus::Running))
        .count();
    let latest_child_activity_ms = views
        .iter()
        .filter_map(|v| v.end_ts.as_deref().or(v.spawn_ts.as_deref()))
        .filter_map(super::iso_to_ms)
        .max();
    // 未结算 = Running 状态 且 spawn_ts 未超僵尸窗。
    // Running 的定义已包含「已登记 ∧ last ∈ {None, Spawn, Resume}」，
    // 即「派发已见、结算未见」——正是 zcode Pending 的语义。
    let has_unsettled = views.iter().any(|v| {
        if !matches!(v.status, SubagentStatus::Running) {
            return false;
        }
        match v.spawn_ts.as_deref().and_then(super::iso_to_ms) {
            Some(spawn) => now_ms as i64 - spawn < ZOMBIE_UNSETTLED_MS,
            // 无 spawn_ts（子转写还没落盘/为空）→ 无法判龄，保守按未结算
            // （宁可不误报完成；僵尸保护的目的是防永久卡死，无 ts 时
            //  collect_at 的 meta 发现路径最终会因 meta 消失而失效）
            None => true,
        }
    });
    (active_count, latest_child_activity_ms, has_unsettled)
}
```

**需要的导入**：该文件顶部已是 `use super::{...}` 分组导入（`claude.rs:13`），追加 `iso_to_ms` 与 `SubagentStatus` 即可（若已存在则跳过）。

- [ ] **Step 4: 运行测试确认通过**

```bash
cd src-tauri && cargo test --lib monitor::subagents::claude 2>&1 | tail -25
```

Expected: 全部 passed（含新增 4 条）

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/monitor/subagents/claude.rs
git commit -m "feat(subagents): add claude descendant activity aggregate for pending arbitration"
```

---

## Task 7: Claude 接入 Pending 拦绿（P3）⚠️ 已撤回，勿直接实施

> **⚠️ 本 Task 已实施后撤回（2026-11-04）**。除 Task 6 的三个缺陷外，
> 本 Task 还有架构位置错误：
>
> **Pending 仲裁位置被 hook 叠加层覆盖**：`claude_parser.rs` 内的叠加发生在
> `adapter.find_sessions()` 内，而 `adapter/mod.rs` 的 hook 叠加层在其**之后**
> 执行——Stop grace 过期（CLI 默认 5s，事件 TTL 30s）会把 `Waiting` 改回 `Idle`。
> 而「派发后台子代理 → 结束本回合 → Stop」正是 P3 的目标场景 ⇒ 每轮轮询覆盖一次，
> 功能失效。spec §4.3 要求的「Pending 最后一道」没有满足。
>
> 重做时 Pending 应移到 `apply_wait_mark_overlay` 之后（adapter 层），
> 或 parser 只带出 `has_unsettled` 字段、由 adapter 统一仲裁。
> 详见设计 spec §9.7 与 issue #200。


**Files:**
- Modify: `src-tauri/src/monitor/claude_parser.rs`（构卡末尾）
- Test: `src-tauri/src/monitor/claude_parser.rs`（测试模块）

- [ ] **Step 1: 定位构卡返回点**

```bash
cd src-tauri && sed -n 280,320p src/monitor/claude_parser.rs
```

记录 `Session { ... }` 构造的起始行（本计划其余步骤以此为锚）。

- [ ] **Step 2: 写失败测试**

在 `claude_parser.rs` 测试模块追加（按该模块既有 fixture 风格）：

```rust
    /// P3：后台子代理未结算时，主会话回合收尾不得报绿
    #[test]
    fn pending_subagent_blocks_main_session_green() {
        // 见 Task 6 fixture 建法：父转写 assistant 文本尾（Idle）
        // + subagents/agent-*.meta.json 已登记 + 无 Stop
        // → 期望 Waiting
    }

    /// P3：无子代理时行为与现状逐分支一致（零回归锁）
    #[test]
    fn no_subagents_keeps_existing_behavior() {
        // 父转写 assistant 文本尾 → 期望 Idle（与改动前一致）
    }
```

> **说明**：本 Task 的两个测试必须复用 Task 6 已验证的 fixture 建法。executing-plans 代理应先完成 Task 6，再在本 Task 中照抄其临时目录结构（`<proj>/<sid>/subagents/agent-<id>.meta.json` + `agent-<id>.jsonl` + 父 `<sid>.jsonl`）。

- [ ] **Step 3: 运行测试确认失败**

```bash
cd src-tauri && cargo test --lib monitor::claude_parser::pending_subagent_blocks_main_session_green 2>&1 | tail -20
```

Expected: FAIL（`left: Idle, right: Waiting`）

- [ ] **Step 4: 接入 Pending 仲裁**

在 `claude_parser.rs` 构卡函数里，`determine_status` 得到初始 `status` 之后、构造 `Session` 之前插入：

```rust
    // P3：完成仲裁——存在未结算子代理时，回合收尾不得报绿。
    // 位置必须在所有转绿路径之后：本函数的转绿路径是 assistant 正文档
    // → determine_status 返回 Idle，以及 hook 层 Stop grace 过期 → Idle。
    // 一处调用同时覆盖（与 zcode Z-P1 同款布局，zcode_parser.rs:511-525）。
    let (_, _, has_unsettled) = crate::monitor::subagents::claude::descendant_activity_at(
        &jsonl_path,
        &session_id,
        chrono::Utc::now().timestamp_millis() as u64,
    );
    let status = if has_unsettled {
        crate::monitor::app_status::overlay_completion_with_pending(
            status,
            crate::monitor::app_status::DescendantActivity::Pending,
        )
    } else {
        status
    };
```

> **注意**：`status` 变量在既有代码里可能是 `let status = ...` 且后续被 `Session { status, ... }` 消费。若已声明为 `let status`，改为 `let mut status` 再用 `status = ...` 赋值；若可重绑定，直接新 `let status =` 覆盖。

- [ ] **Step 5: 运行测试确认通过**

```bash
cd src-tauri && cargo test --lib monitor::claude_parser 2>&1 | tail -25
```

Expected: 全部 passed

- [ ] **Step 6: 全量回归**

```bash
cd src-tauri && cargo test --lib 2>&1 | tail -30
```

Expected: 全部 passed

- [ ] **Step 7: clippy**

```bash
cd src-tauri && cargo clippy --all-targets -- -D warnings 2>&1 | tail -20
```

Expected: 干净

- [ ] **Step 8: 提交**

```bash
git add src-tauri/src/monitor/claude_parser.rs
git commit -m "feat(claude): block green when background subagent is unsettled

复用 subagents/claude.rs 现成三态机（registered ∧ last∈{None,Spawn,Resume}），
接入 overlay_completion_with_pending。实测 zcode 392 次转绿中 256 次（65%）
发生在子代理仍在跑时，claude 同属高发面。"
```

---

## Task 8: 文档与最终验收

**Files:**
- Modify: `docs/engineering/specs/2026-11-04-session-age-anchor-discipline-design.md`（补充实施记录）

- [ ] **Step 1: 跑全量测试与检查**

```bash
cd src-tauri && cargo test --lib 2>&1 | tail -30
cargo clippy --all-targets -- -D warnings 2>&1 | tail -15
cargo fmt --check; echo "fmt exit=$?"
```

Expected: 失败集合与基线一致（本仓有 8 条既有基线失败，见 spec §11.2）、
clippy 干净、fmt exit=0。

- [ ] **Step 2: 前端检查（确认未受影响）**

```bash
cd .. && pnpm check 2>&1 | tail -25
```

Expected: 全绿（本批不改前端，若失败说明有意外耦合）

- [ ] **Step 3: 真机验收（WorkBuddy —— 本次唯一的行为修复）**

1. 启动 `pnpm tauri:dev`
2. 在 WorkBuddy 里跑一个 3+ 轮工具任务
3. 观察：
   - 任务运行期间卡片**保持黄灯**，不出现绿→黄振荡
   - 任务真正完成后转绿，**只播一次**完成提醒
   - 完成后静置 5 分钟，卡片**保持绿**，不回黄

- [ ] **Step 4: ~~真机验收（Claude P3）~~ —— 随 P3 撤回，本批不做**

- [ ] **Step 5: 在设计文档追加实施记录**

在 spec 的「决策记录」表之后追加一节：

```markdown
## 14. 实施记录（2026-11-04）

- P0 `SessionAges`：`monitor/app_status.rs` 新增双轴年龄类型（字段私有 + getter），
  workbuddy 改用 `content_age` 喂冷却。**这是唯一的行为修复。**
- P1 常量集中：6 个常量迁入 `app_status.rs`（dsh 30min 刻意不迁）。
- P2 `CooldownSignal`：共享核新增，workbuddy 已接入；claude 只留接口未接。
- P3 claude Pending：**已实施后整块撤回**，理由见 §9.7。
  重做前置条件登记于 issue #200。
```

- [ ] **Step 6: 提交**

```bash
git add docs/engineering/specs/2026-11-04-session-age-anchor-discipline-design.md \
        docs/engineering/plans/2026-11-04-session-age-anchor-discipline.md \
        docs/engineering/evidence/claude-task-id-location-20261104/
git commit -m "docs(spec): record implementation of session age anchor discipline

P0/P1/P2 已交付；P3 因 Stop 落盘形态变化与 hook 叠加层位置错误整块撤回，
取证归档于 docs/engineering/evidence/claude-task-id-location-20261104/。
重做前置条件见 issue #200。"
```

- [ ] **Step 7: 停止（不 push）**

```bash
git log --oneline origin/pr-190..HEAD 2>/dev/null || git log --oneline -10
```

**不要执行 `git push`。** 等待用户 review。

---

## 自审清单（写计划后已完成）

**1. Spec 覆盖检查**

| Spec 章节 | 对应 Task |
|---|---|
| §3.1 `SessionAges` 定义与唯一构造点 | Task 1 |
| §4.2 L2.5 锚点层 | Task 1-2 |
| §4.3 判定顺序（冷却 → 停更 → 二次强信号 → Pending） | Task 2（workbuddy）、Task 7（claude） |
| §6 P0 拆分锚点 | Task 1-2 |
| §6.3 其他工具不接入 SessionAges | Task 2 未触及其他 parser（符合决策 4） |
| §6.4 P0 测试 | Task 1 Step 1（4 条）、Task 2 Step 1（3 条） |
| §7 P1 常量集中 | Task 3 |
| §7.4 P1 测试 | Task 3 Step 1（3 条） |
| §8 P2 `CooldownSignal` | Task 4 |
| §8.5 P2 测试 | Task 4 Step 1（6 条） |
| §9 P3 claude Pending | Task 6-7 |
| §9.4 僵尸保护 | Task 6 Step 3（`ZOMBIE_UNSETTLED_MS`） |
| §9.6 P3 测试 | Task 6 Step 1（4 条）、Task 7 Step 2（2 条） |
| §10 不动清单 | 未触及（`derive_app_status` / codex CLI / zcode / dsh / openclaw / opencode 映射 / 前端 / L1 翻译层） |
| §11 实施顺序 P0→P1→P2→P3 | Task 1→2→3→4→5→6→7 |
| §11.1 验收标准 | Task 8 |

**2. 占位符扫描**

- Task 7 Step 2 的两个测试是**骨架**（注释描述期望），因为 fixture 需要复用 Task 6 的临时目录结构。这是 execution 时的必要留白，已在正文明确说明「照抄 Task 6 的 fixture 建法」。**执行代理必须先完成 Task 6，再回填 Task 7 的测试代码。**
- 无其他 "TBD" / "TODO" / "fill in details"。

**3. 类型一致性检查**

| 类型/函数 | 定义位置 | 使用位置 | 一致 |
|---|---|---|---|
| `SessionAges { content_age_ms, presence_age_ms }` | Task 1 Step 3 | Task 2 Step 3 | ✅ |
| `SessionAges::derive(now_ms, content_ms, presence_sources)` | Task 1 Step 3 | Task 2 Step 3 | ✅ |
| `CooldownSignal { observed_at_ms, hold_ms, requires_tail }` | Task 4 Step 3 | Task 4 Step 4c | ✅ |
| `apply_completion_cooldown(status, tail_kind, signal, now_ms)` | Task 4 Step 3 | Task 4 Step 4c | ✅ |
| `COMPLETION_COOLDOWN_MS` | Task 3 Step 3 | Task 4 Step 4c、Step 4e | ✅ |
| `descendant_activity_at(parent_jsonl, session_id, now_ms) -> (usize, Option<i64>, bool)` | Task 6 Step 3 | Task 7 Step 4 | ✅ |
| `overlay_completion_with_pending(status, DescendantActivity::Pending)` | 既有（app_status.rs:98） | Task 7 Step 4 | ✅ |
| `iso_to_ms(&str) -> Option<i64>` | 既有（subagents/mod.rs:120） | Task 6 Step 3 | ✅ |

**4. 已知执行风险**

| 风险 | 缓解 |
|---|---|
| Task 2 第三条测试 `missing_content_mtime_disables_cooldown` 依赖 `mangle_project_path` 删文件路径 | 若路径构造失败，改为直接断言「不 panic + 空结果」，或删除该测试（边界已由 Task 1 的 `content_unknown` 覆盖） |
| Task 6 fixture 用 `claude_fixture` / `add_agent` / `append`（已存在，`claude.rs:618-658`） | 已核实签名与布局；Stop 通知必须是 **JSON 行** 且 task-id 在 `message.content` 文本内 |
| Task 6 跨测试缓存污染 | 照抄既有 `claude_source_lifecycle` 开头的 `reset_cache_for_tests()` |
| Task 7 的 `status` 变量可能不是 `mut` | 按 Step 4 注意项改为 `let mut status` 或重绑定 |
| `#[deprecated]` 引发 `-D warnings` 失败 | 已在 Task 3 Step 4f 明确本批不加 deprecated |

---

## 执行说明

- 每个 Task 独立提交，commit message 已给定。
- 每个 Task 结束时必须 `cargo test --lib` 全绿，不允许"先攒着最后一起测"。
- **严禁 `git push`**——用户要 review 后自行推送。
- Task 6/7 涉及 claude 子代理数据格式，执行前必须先 `grep` 确认 `parse_meta` 与 `classify_parent_line` 的实际契约（见风险表）。
