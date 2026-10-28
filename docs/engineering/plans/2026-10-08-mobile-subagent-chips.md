# 移动端子 agent 运行 chip · 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在远程看板（`/m`）会话详情底部状态区显示**运行中**子 agent 的名称、运行时长、token 消耗（四工具：claude / opencode / kimi / codex）。

**Architecture:** 后端在 `monitor/` 下新建 `subagents/` 模块承载四工具的运行判据（判据/解析/累计全部纯函数化，文件增量读 + 进程内会话级缓存）；端点 `GET /m/api/v1/session-subagents` 挂 `api_router`，经 `RemoteState.subagent_source`（HashMap 注入缝，每工具一条）取数，文件 IO 全 `spawn_blocking`。前端新组件 `SubagentChips.tsx` 作 `ModeBar` 兄弟节点（同一行 flex-wrap），随 SessionDetail 既有 `DETAIL_REFRESH_MS` 轮询拉取，时长 tick 收在组件内。

**Tech Stack:** Rust（axum / serde / rusqlite / chrono）+ React 19 + TypeScript + vitest + Tailwind CSS v4。

**设计依据（唯一权威，本计划只做任务分解、不改任何判据/口径）：**
`docs/engineering/specs/2026-10-08-mobile-subagent-chips-design.md`（已评审修订定稿）。
spec 引用记号：§2 数据源证据 / §4 判据 / §5 后端 / §6 前端 / §7 测试 / §8 已知边界。

---

## 一、现状锚点核对表（写计划时已逐一核对，行号为 2026-10-08 快照、实施时以符号检索为准）

| 锚点 | 现状 | 结论 |
|---|---|---|
| `src-tauri/src/monitor/session_scan.rs:75` | `EVICT_HARD_CAP = 8192`（超限整 namespace 清空） | 新缓存照抄该模式 |
| `src-tauri/src/monitor/session_scan.rs:46` | `SCAN_FRESH_SECS`（24h）+ `fresh_within` | codex 倒排索引窗口对齐它 |
| `src-tauri/src/monitor/jsonl.rs:23` | `read_recent_lines_with_budget`（512KB 尾窗） | 只读先例；新模块**不复用**（见任务 T1 权衡） |
| `src-tauri/src/monitor/jsonl.rs:86` | `count_active_subagents`（旧布局平铺层，新布局恒 0） | **不动**（spec §8.6，T9 挂 issue） |
| `src-tauri/src/monitor/kimi_parser.rs:74` | `resolve_data_root` / `parse_session_index`:263 / `resolve_session_dir`:298 均 `pub(crate)` | kimi 会话目录定位直接复用，零路径推导复制 |
| `src-tauri/src/monitor/codex_parser.rs:26` | `is_rollout_file`（私有）+ `CODEX_SCAN.collect` 递归收集先例 | 需把 `is_rollout_file` 提为 `pub(crate)`（一行） |
| `src-tauri/src/monitor/opencode_parser.rs:87` | `schema_is_v2` 单点判据 | opencode source 复用 |
| `src-tauri/src/monitor/sqlite.rs` | `open_readonly_with_timeout`（共享 helper） | opencode source 复用 |
| `src-tauri/src/remote/server.rs:462` | `RemoteState`（缝字段均 `Box<dyn Fn>` / `Arc<dyn Fn>`） | 新增 `subagent_source` 字段 |
| `src-tauri/src/remote/server.rs:583` | `api_router`（nest + 内层 gate，新端点零鉴权接线） | 路由加一行 |
| `src-tauri/src/remote/api.rs:549` | `session_files` handler（缺参 400 / spawn_blocking / no-store 模板） | 新端点照此写 |
| `src-tauri/src/remote/mod.rs:288` | 生产装配 `static STATE: Lazy<Arc<RemoteState>>` | 四工具 source 在此接线 |
| RemoteState 测试字面量 | server.rs 19 处 + `inject/queue.rs:1104` 1 处（共 20 处测试装配） | 加字段后由编译器驱动逐一补桩 |
| `src/mobile/SessionDetail.tsx:69` | `DETAIL_REFRESH_MS = 10_000`；`:312` refreshTick；`:433-465` 轮询（hidden 暂停/恢复补刷）；`:1048` cardDock；`:1069` ModeBar（key=`mode-${session.id}`） | chip 挂 cardDock、key=`subagents-${session.id}` |
| `src/mobile/ModeBar.tsx:45` | 挂载即拉、失败静默自隐 | chip 同惯例 |
| `src/mobile/api.ts:685` | `fetchApproveOptions`（GET + ApiError 封装模板） | 新 fetch 同款 |
| `src/lib/usage/format.ts:36` | `fmtTokens`（万/亿两档缩写，全仓唯一口径） | 移动端可经 `@/` 别名引入（先例：`Board.tsx` 引 `@/components/common/ToolIcon`） |
| `tests/mobile/SessionDetail.test.tsx:121/1167/1878` | fetch 桩对未知路由 `throw new Error("unexpected fetch")` | **新增拉取必须同步补桩**（T8 步骤 6） |
| `tests/mobile/ModeBar.test.tsx` | vitest + stubGlobal fetch 惯例 | 新组件测试同款 |

## 二、已拍板决策（spec 定案，实施不得重议）

1. 四工具判据：claude 事件序状态机（spawn=meta 存在 / stop=父 JSONL task-id 通知 / resume=父 JSONL SendMessage；**不做 mtime 兜底**）；opencode `session_v2.parent_id` 非空 ∧ `time_updated` 距 now < 90s（**不用 time_idle**）；kimi `agents/<dir>/wire.jsonl` 存在 ∧ dir≠main ∧ mtime 距 now < 90s；codex rollout 带 `parent_thread_id` ∧ 无 `task_complete`（`task_complete_seen` 布尔出现即置位永不复位）。
2. token 口径：各工具取自身落盘 usage 累计，四桶 `{input, cacheRead, cacheCreation, output}`；codex `input = input_tokens − cached_input_tokens`（防重复计数）；opencode `tokens_reasoning` 与 codex `reasoning_output_tokens` 不计入展示合计。
3. 端点 `GET /m/api/v1/session-subagents?agent_type=&session_id=`：只含运行中、按 `spawnTs` 升序、空态唯一权威、缺参/空白 400、未知工具 `[]`、`Cache-Control: no-store`、IO 全 `spawn_blocking`、挂 `api_router` 结构性继承 PIN gate。**端点契约冻结**。
4. 缓存：进程内全局表（claude/kimi/codex 入缓存；opencode 查询即最新无缓存）；条目硬上限照抄 `EVICT_HARD_CAP` 模式；缓存物全是文件内容纯函数、无时间叠加；时长服务端下发 `spawnTs`、客户端自走。
5. 前端：`SubagentChips.tsx` 为 ModeBar 兄弟节点（同一行 flex-wrap、生命周期解耦）；`key = subagents-${session.id}`；时长 tick 收在组件内；token 用 `fmtTokens` 四桶合计；`description` 进 `title`；失败/空态静默。
6. kimi 完成标记与 opencode token 随跑性是**待实机校准项**：校准只改各 source 实现，不阻塞其余任务（见 §十）。

## 三、模块划分与文件结构（分解决策在此锁定）

```
src-tauri/src/monitor/subagents/        # 新模块：四工具子 agent 运行检测（判据域）
├── mod.rs        # SubagentView/TokenUsage 载荷类型、ACTIVE_SECS/active_within、
│                 #   ms_to_iso、IncrState/read_increment（增量读+半行缓冲+stat门控+护栏）、
│                 #   会话级缓存 registry（EVICT_HARD_CAP 模式）
├── claude.rs     # 事件序状态机纯函数 + meta/转写解析 + 倒序分块重建 + source 装配（缓存语义）
├── opencode.rs   # session_v2 单条 SQL + 活跃过滤 + 桶位映射 + source（无缓存）
├── kimi.rs       # 复用 kimi_parser 注册表定位会话目录 + agents/ 扫描 + usage.record 增量累计
└── codex.rs      # parent_thread_id 倒排索引（24h 窗口）+ task_complete_seen + 桶位映射

src-tauri/src/remote/
├── server.rs     # RemoteState 加 subagent_source 字段 + api_router 加路由 + 端点测试
├── api.rs        # session_subagents handler + spawnTs 排序
└── mod.rs        # 生产装配四工具 source

src/mobile/
├── SubagentChips.tsx   # 新组件（含 formatElapsed/chipTokenText 纯函数导出）
├── SessionDetail.tsx   # cardDock 挂载（status-row 包裹 ModeBar + SubagentChips）
└── api.ts              # fetchSessionSubagents + SubagentView 类型

tests/mobile/SubagentChips.test.tsx     # 前端测试
```

**新模块放 `monitor/subagents/` 而非 remote 下的理由（权衡）**：判据读的是各家工具的磁盘布局（claude projects 目录 / kimi 会话注册表 / codex rollout 首行 / opencode DB），与 `monitor/` 各 parser 同域同知识；`remote/` 是传输与端点层（api/content 是 HTTP 形状），把磁盘解析放 remote 会重蹈「端点里长出第二套解析」的口径漂移。且 kimi 的目录定位（`pub(crate)`）与 codex 的 `is_rollout_file`/`fresh_within` 都在 monitor 侧，同 crate 引用零成本。**反方代价**：remote 层经 `crate::monitor::subagents` 跨层引用（与 `message_source → content::read_session_messages` 的既有方向一致，可接受）。

**为何不建单文件 `monitor/subagents.rs`**：四工具判据各自独立演化（claude 状态机 ~400 行、codex 索引 ~300 行），单文件会超 1200 行；仓内先例（`monitor/` 23 个文件、`services/` 按域拆分）支持目录模块。

**为何不复用 `SessionFileScan` 摘要缓存而自建会话级 offset 缓存（spec §5.1 明示的正当偏离）**：摘要缓存按 `(mtime,size)` 全量重解析，对「持续增长的转写累计 token」意味着每轮全文件重读；offset 增量是 O(delta)。缓存键也不同（会话 vs 文件路径）。**防止后人「归一」**：在 `mod.rs` 模块文档写明此偏离理由（照抄 spec §5.1 原文）。

## 四、实施顺序（TDD，9 Task + 3 校准；每 Task 一 commit，门禁全绿）

| Task | 内容（红→绿重点） | 依赖 | commit |
|---|---|---|---|
| T1 | `subagents/mod.rs`：载荷类型 + 增量读（半行缓冲/stat 门控/护栏）+ 会话级缓存 registry | — | `feat(subagent-chips): 子 agent 载荷类型与增量读/缓存骨架` |
| T2 | claude 判据纯函数：行分类（stop/resume）、状态机（含**通知后 SendMessage 复现 running** P0 回归锁）、meta/usage 累计解析 | T1 | `feat(subagent-chips): claude 事件序状态机判据（纯函数）` |
| T3 | claude source 装配：会话目录发现 + 倒序分块重建 + 会话级缓存增量（token 累计/护栏） | T2 | `feat(subagent-chips): claude 子 agent source——增量缓存与倒序重建` |
| T4 | opencode source：session_v2 单查 + 活跃边界（ACTIVE_SECS 边界值）+ 桶位映射 | T1 | `feat(subagent-chips): opencode 子 agent source（SQLite 直查）` |
| T5 | kimi source：注册表定位 + agents/ 扫描（main 排除/mtime 活跃）+ usage.record 四桶累计 | T1 | `feat(subagent-chips): kimi 子 agent source（wire 增量累计）` |
| T6 | codex source：parent_thread_id 倒排索引（24h 窗口）+ task_complete_seen + input−cached 桶位映射 | T1 | `feat(subagent-chips): codex 子 agent source——倒排索引与终态布尔` |
| T7 | 端点 + RemoteState 缝 + 路由 + 生产装配（20 处测试装配补桩）+ 端点测试 | T3–T6 | `feat(subagent-chips): /session-subagents 端点与 source 注入缝` |
| T8 | 前端：api.ts fetch + SubagentChips 组件 + SessionDetail 挂载 + 既有测试补桩 + 新测试 | T7 | `feat(subagent-chips): 移动端子 agent chip 组件与轮询接线` |
| T9 | 文档收口：看板徽标落差挂追踪 issue + 手工验收清单整理 | T8 | `docs(subagent-chips): 收口文档与追踪 issue` |
| C1–C3 | 实机校准（kimi 完成标记 / opencode token 随跑性 / claude 续跑手工验收） | — | **可并行/可延期，不阻塞合入**（§十） |

依赖链：T1 → {T2→T3, T4, T5, T6}（四工具相互独立可并行）→ T7 → T8 → T9。C1–C3 与主线无关。

## 五、任务详情

### Task 1: 共享骨架——载荷类型、增量读、会话级缓存

**Files:**
- Create: `src-tauri/src/monitor/subagents/mod.rs`
- Modify: `src-tauri/src/monitor/mod.rs`（追加 `pub mod subagents;`——字母序在 `pub mod status;` 与 `pub mod workbuddy_parser;` 之间：session_scan < sqlite < status < **subagents** < workbuddy_parser）

- [ ] **Step 1: 写失败测试**（`monitor/mod.rs` 先挂 `pub mod subagents;` 并建空 `subagents/mod.rs`，让测试模块可编译）

```rust
// src-tauri/src/monitor/subagents/mod.rs 底部
#[cfg(test)]
mod tests {
    use super::*;

    /// 增量读：追加完整行 → 返回；追加半行 → 不返回，下轮补齐后返回
    #[test]
    fn read_increment_handles_partial_line() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("wire.jsonl");
        std::fs::write(&p, "{\"a\":1}\n").unwrap();
        let mut st = IncrState::default();
        let r = read_increment(&p, &mut st);
        assert!(matches!(r, IncrRead::Lines { ref lines, reset: false } if lines.len() == 1));
        // 追加半行：不产出
        std::fs::write(&p, "{\"a\":1}\n{\"b\":").unwrap();
        let r = read_increment(&p, &mut st);
        assert!(matches!(r, IncrRead::Lines { ref lines, reset: false } if lines.is_empty()),
            "半行不得提前产出");
        // 补齐换行：产出一行
        std::fs::write(&p, "{\"a\":1}\n{\"b\":2}\n").unwrap();
        let r = read_increment(&p, &mut st);
        assert!(matches!(r, IncrRead::Lines { ref lines, reset: false } if lines == ["{\"b\":2}"]));
    }

    /// stat 门控（spec §7「读计数断言」的单元层落点）：文件未变 → Unchanged（未开文件）
    #[test]
    fn read_increment_stat_gate() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("w.jsonl");
        std::fs::write(&p, "l1\n").unwrap();
        let mut st = IncrState::default();
        let _ = read_increment(&p, &mut st);
        assert!(matches!(read_increment(&p, &mut st), IncrRead::Unchanged),
            "size 未变 → Unchanged（T3/T5/T6 的功能层等价断言：两次 collect 结果一致）");
    }

    /// 护栏（spec §5.1「size < offset 作废重建」）：截断/重写 → reset=true 且全量重读
    #[test]
    fn read_increment_truncation_resets() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("w.jsonl");
        std::fs::write(&p, "aaaa\nbbbb\ncccc\n").unwrap();
        let mut st = IncrState::default();
        let _ = read_increment(&p, &mut st);
        // 重写为更短内容（size < offset）
        std::fs::write(&p, "x\ny\n").unwrap();
        let r = read_increment(&p, &mut st);
        assert!(matches!(r, IncrRead::Lines { reset: true, ref lines } if lines.len() == 2),
            "护栏触发：状态清零后从 0 重读，调用方据此清空累计器");
    }

    /// 会话级缓存：读改写往返 + 硬上限整段清空（EVICT_HARD_CAP 模式）
    #[test]
    fn cache_update_and_hard_cap() {
        reset_cache_for_tests();
        let v = update_entry("t-cap", "s1", |cur| {
            assert!(cur.is_none());
            (Arc::new(7u32) as CacheBox, 1u8)
        });
        assert_eq!(v, 1);
        let v = update_entry("t-cap", "s1", |cur| {
            let old = cur.and_then(|b| b.downcast::<u32>().ok()).map(|a| *a);
            (Arc::new(old.unwrap_or(0) + 1) as CacheBox, old)
        });
        assert_eq!(v, Some(7), "二次进入拿到上次条目");
        for i in 0..=SUBAGENT_CACHE_CAP {
            let _ = update_entry("t-cap", &format!("fill-{i}"), |_| (Arc::new(0u32) as CacheBox, ()));
        }
        // 超限后整段清空 → s1 条目不复存在
        let v = update_entry("t-cap", "s1", |cur| (Arc::new(0u32) as CacheBox, cur.is_none()));
        assert!(v, "条目超上限 → 整段清空（宁可重建也不吃内存）");
        reset_cache_for_tests();
    }

    /// 活跃窗口边界（spec §4.2/§4.3 共用取值）：恰好 90s 视为已过期
    #[test]
    fn active_within_boundary() {
        let now = std::time::SystemTime::now();
        assert!(active_within(now - std::time::Duration::from_secs(89), now));
        assert!(!active_within(now - std::time::Duration::from_secs(ACTIVE_SECS), now),
            "距 now 恰好 ACTIVE_SECS → 已过期（严格小于才活跃）");
    }

    #[test]
    fn token_usage_total_saturating() {
        let t = TokenUsage { input: u64::MAX, cache_read: 1, cache_creation: 0, output: 0 };
        assert_eq!(t.total(), u64::MAX, "四桶合计饱和加法，不 panic");
    }
}
```

- [ ] **Step 2: 跑红**：`cd src-tauri && cargo test subagents` → 期望编译失败（类型/函数未定义）。

- [ ] **Step 3: 最小实现**（`mod.rs` 主体；模块文档必须含「为何偏离 SessionFileScan」段，照抄 spec §5.1）

```rust
//! 移动端子 agent 运行检测（spec：docs/engineering/specs/2026-10-08-mobile-subagent-chips-design.md §5.1）
//!
//! 模块职责：四工具（claude/opencode/kimi/codex）「该会话下有哪些运行中子 agent」
//! 的判据与装配。判据/解析/累计全部纯函数化（注入字节流/路径，测试零真实磁盘）。
//!
//! 预算纪律：opencode 查询即最新（SQLite 豁免）；claude/kimi/codex 走**会话级
//! offset 增量缓存**——**为何不用 SessionFileScan 摘要缓存**：摘要缓存对「持续增长
//! 的转写累计 token」意味着每轮全文件重解析；offset 增量是 O(delta)。缓存物全是
//! 文件内容纯函数，无时间叠加；运行时长服务端下发 spawnTs、客户端自走。
//! 防后人「归一」：不要把本模块的缓存改回 (mtime,size) 全量摘要形态。

use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

/// 四桶 token 载荷（端点 JSON camelCase；各工具映射时已把 reasoning 类桶排除在四桶外）
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TokenUsage {
    pub(crate) input: u64,
    pub(crate) cache_read: u64,
    pub(crate) cache_creation: u64,
    pub(crate) output: u64,
}

impl TokenUsage {
    /// 四桶合计（前端 chip 的显示值；饱和加法防溢出）
    pub(crate) fn total(&self) -> u64 {
        self.input
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_creation)
            .saturating_add(self.output)
    }
}

/// 端点载荷单条（spec §5.2）。spawnTs=None：首条时间戳尚未落盘（spawn 竞态，下轮自愈，
/// 前端不显示时长只显 token——spec §8.2）
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SubagentView {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) description: Option<String>,
    pub(crate) spawn_ts: Option<String>,
    pub(crate) tokens: TokenUsage,
}

/// 活跃窗口（opencode §4.2 / kimi §4.3 共用取值：详情轮询 10s 的 9 倍冗余，
/// 容忍子 agent 长静默工具调用；常量可调）
pub(crate) const ACTIVE_SECS: u64 = 90;

/// 活跃判定（纯函数，now 注入便于测试边界）：严格小于 ACTIVE_SECS 才活跃
pub(crate) fn active_within(
    last_activity: std::time::SystemTime,
    now: std::time::SystemTime,
) -> bool {
    now.duration_since(last_activity)
        .map(|d| d.as_secs() < ACTIVE_SECS)
        .unwrap_or(false)
}

/// 毫秒 → ISO8601 字符串（spawnTs 下发口径；溢出/负值 → None）。
/// **待收口申报（评审 P2-3）**：content.rs 与 kimi_parser.rs 各有一份私有 ms_to_iso，
/// 本模块是第三份——跨文件提取公用 helper 属独立重构批次，不在本计划扩面；
/// 三份实现语义一致（from_timestamp_millis + to_rfc3339），先注明不静默新造。
pub(crate) fn ms_to_iso(ms: i64) -> Option<String> {
    chrono::DateTime::from_timestamp_millis(ms).map(|t| t.to_rfc3339())
}

/// JSON 时间值（int 毫秒 / ISO 字符串双形态）→ ISO 字符串（kimi created_at 用）。
/// 与 kimi_parser::updated_at_to_string 同构（那处不做毫秒→ISO 归一）——同属
/// P2-3 待收口申报：C1 取证 created_at 形态后随 kimi 判据收严一并评估合并。
pub(crate) fn json_time_to_iso(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::Number(n) => n.as_i64().and_then(ms_to_iso),
        serde_json::Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

// ============================================================
// 增量读：offset 只跨过完整行；尾部半行留待下轮；stat 门控；护栏作废重建
// ============================================================

/// 增量读状态（进会话级缓存，随缓存条目存活；Clone 供缓存条目回取后的可变化拷贝）
#[derive(Default, Clone)]
pub(crate) struct IncrState {
    /// 已消费字节数（不含尾部半行——半行下次从 offset 重读再拼）
    offset: u64,
    /// 上次读到的文件大小（stat 门控：不变则不开文件）
    last_size: u64,
    /// 尾部半行缓冲（UTF-8 安全：按字节切、按行转 String）
    partial: Vec<u8>,
}

pub(crate) enum IncrRead {
    /// stat 门控命中：文件未增长，未打开
    Unchanged,
    /// 新完整行（文件序）。reset=true：size<offset 护栏触发，已清零状态并从 0
    /// 重读全文件——调用方必须先清空自己的累计器再消费 lines
    Lines { lines: Vec<String>, reset: bool },
}

/// 增量读（spec §5.1「offset 增量 + 半行缓冲 + stat 门控」骨架，三工具复用）。
/// 文件不存在/不可读 → Lines{lines:[], reset:false}（与既有解析器防御语义一致，
/// 状态保留——下次文件出现按旧 offset 续读）。
pub(crate) fn read_increment(path: &Path, st: &mut IncrState) -> IncrRead {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(meta) = std::fs::metadata(path) else {
        return IncrRead::Lines { lines: Vec::new(), reset: false };
    };
    let size = meta.len();
    if size == st.last_size {
        return IncrRead::Unchanged;
    }
    let mut reset = false;
    if size < st.offset {
        // 护栏：文件被截断/重写（换代会话重写同名文件）——状态作废，从 0 重读
        st.offset = 0;
        st.last_size = 0;
        st.partial.clear();
        reset = true;
    }
    let Ok(mut f) = std::fs::File::open(path) else {
        return IncrRead::Lines { lines: Vec::new(), reset: false };
    };
    if st.offset > 0 && f.seek(SeekFrom::Start(st.offset)).is_err() {
        return IncrRead::Lines { lines: Vec::new(), reset: false };
    }
    let mut chunk = vec![0u8; (size - st.offset) as usize];
    if f.read_exact(&mut chunk).is_err() {
        return IncrRead::Lines { lines: Vec::new(), reset: false };
    }
    // 拼上半行缓冲，按字节切 '\n'；末段（其后无换行）→ 新 partial
    let mut buf = std::mem::take(&mut st.partial);
    buf.extend_from_slice(&chunk);
    let mut lines = Vec::new();
    let mut start = 0usize;
    for (i, &b) in buf.iter().enumerate() {
        if b == b'\n' {
            lines.push(String::from_utf8_lossy(&buf[start..i]).into_owned());
            start = i + 1;
        }
    }
    st.partial = buf[start..].to_vec();
    // 核心不变式：partial 永远是「文件尾部尚未换行收尾的字节」，故
    // offset = size - partial.len()（字节精确；from_utf8_lossy 不参与计数）
    st.offset = size - st.partial.len() as u64;
    st.last_size = size;
    IncrRead::Lines { lines, reset }
}
```

**不变式说明**：JSONL 行以 `
` **右终结**——切段后最后一段（其后无 `
`）即跨写半行；`partial` 恒等于文件尾段字节，`offset = size - partial.len()` 一行等式收口，无需增量字节记账。测试 `read_increment_handles_partial_line` 锁定行为。

```rust
// ============================================================
// 会话级缓存 registry（spec §5.1：进程内全局表；仅 claude / kimi / codex 使用，
// opencode 查询即最新无缓存）。评审 P2-1：定稿实现，不再是「只有测试契约」。
// ============================================================

/// 缓存条目盒（Arc<dyn Any>——存取形态照抄 session_scan.rs 的 FileParseCache）
type CacheBox = std::sync::Arc<dyn std::any::Any + Send + Sync>;

/// 条目硬上限（照抄 session_scan.rs:75 EVICT_HARD_CAP 模式：超限整段清空——
/// 正常活跃会话远达不到；病态增殖下宁可重建也不吃内存）
const SUBAGENT_CACHE_CAP: usize = 1024;

fn registry() -> &'static std::sync::Mutex<std::collections::HashMap<(&'static str, String), CacheBox>> {
    static REGISTRY: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<(&'static str, String), CacheBox>>,
    > = std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// 读改写一条缓存条目：`f` 收到当前条目（None = 缓存缺失/被清/键类型不符），
/// 返回 (新条目, 结果)。downcast 失败按 None 处理——与 FileParseCache
/// 「类型不匹配按未命中覆盖」同语义（同键换类型只发生在实现演化期，防御即可）。
pub(crate) fn update_entry<R>(
    tool: &'static str,
    session_id: &str,
    f: impl FnOnce(Option<CacheBox>) -> (CacheBox, R),
) -> R {
    let mut map = registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if map.len() >= SUBAGENT_CACHE_CAP {
        map.clear(); // EVICT_HARD_CAP 同语义：整段清空
    }
    let cur = map.remove(&(tool, session_id.to_string()));
    let (entry, out) = f(cur);
    map.insert((tool, session_id.to_string()), entry);
    out
}

/// 测试隔离：清空全部条目（MAM 重启语义的测试等价物；仅测试构建）
#[cfg(test)]
pub(crate) fn reset_cache_for_tests() {
    registry().lock().unwrap_or_else(|e| e.into_inner()).clear();
}
```

- [ ] **Step 4: 跑绿**：`cd src-tauri && cargo test subagents` → PASS。
- [ ] **Step 5: 门禁 + commit**：

```bash
cd src-tauri && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check
git add src-tauri/src/monitor/subagents/ src-tauri/src/monitor/mod.rs
git commit -m "feat(subagent-chips): 子 agent 载荷类型与增量读/缓存骨架"
```

### Task 2: claude 判据纯函数（事件序状态机）

**Files:**
- Create: `src-tauri/src/monitor/subagents/claude.rs`
- Modify: `src-tauri/src/monitor/subagents/mod.rs`（追加 `mod claude;`——本任务先不注册到 crate 外）

- [ ] **Step 1: 写失败测试**（`claude.rs` 底部 `#[cfg(test)] mod tests`；全部注入字符串，零磁盘）

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn notify(id: &str) -> String {
        // 父 JSONL 通知行（spec §2.1：子 agent 停止追加 task-notification；实盘同 id 多次）
        format!(r#"{{"type":"user","timestamp":"2026-10-08T07:40:00Z","message":{{"role":"user","content":"<task-notification><task-id>{id}</task-id><summary>Agent stopped</summary></task-notification>"}}}}"#)
    }
    fn sendmsg(id: &str) -> String {
        // 父 JSONL SendMessage 续跑行（assistant tool_use，input.to 指向子 agent id）
        format!(r#"{{"type":"assistant","timestamp":"2026-10-08T07:41:00Z","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"tu1","name":"SendMessage","input":{{"to":"{id}","body":"continue"}}}}]}}}}"#)
    }
    fn agent_line(ts: &str, inp: u64, cr: u64, cc: u64, out: u64) -> String {
        // 子 agent 转写行（每条消息带 timestamp + usage 四桶——spec §2.1）
        format!(r#"{{"timestamp":"{ts}","type":"assistant","message":{{"role":"assistant","usage":{{"input_tokens":{inp},"cache_read_input_tokens":{cr},"cache_creation_input_tokens":{cc},"output_tokens":{out}}}}}}}"#)
    }

    /// P0 回归锁（spec §2.1 排雷① + §7）：通知 ≠ 终态——stop 后 SendMessage 复现 running
    #[test]
    fn stop_then_sendmessage_is_running_again() {
        let mut last: HashMap<String, LastEvent> = HashMap::new();
        for line in [notify("a88"), sendmsg("a88")] {
            if let Some(ev) = classify_parent_line(&line) {
                apply_event(&mut last, &ev);
            }
        }
        assert!(is_running(true, last.get("a88").copied()),
            "stop → resume 后必须回到运行中（实盘 a588fb48 案例）");
        // 反向：只有通知 → 不在跑
        let mut last2: HashMap<String, LastEvent> = HashMap::new();
        if let Some(ev) = classify_parent_line(&notify("a88")) {
            apply_event(&mut last2, &ev);
        }
        assert!(!is_running(true, last2.get("a88").copied()));
        // 同 id 多次通知（实盘 3 条）仍是 Stop
        let mut last3: HashMap<String, LastEvent> = HashMap::new();
        for _ in 0..3 {
            if let Some(ev) = classify_parent_line(&notify("a88")) { apply_event(&mut last3, &ev); }
        }
        assert_eq!(last3.get("a88"), Some(&LastEvent::Stop));
    }

    /// 倒序重建语义（spec §5.1）：新→旧扫描，每 id 取最新事件；无事件 → Spawn
    #[test]
    fn reverse_rebuild_takes_latest_event() {
        let mut seen: HashMap<String, LastEvent> = HashMap::new();
        // 倒序喂：新（resume）→ 旧（stop）
        for line in [sendmsg("b1"), notify("b1")] {
            if let Some(ev) = classify_parent_line(&line) { apply_event_rev(&mut seen, &ev); }
        }
        assert_eq!(seen.get("b1"), Some(&LastEvent::Resume));
        assert!(is_running(true, None), "扫到头无事件 = spawn 态（运行中）");
    }

    /// 行分类：无关行/坏 JSON/别的工具调用 → None；SendMessage 给别的 id 不影响本 id
    #[test]
    fn classify_ignores_irrelevant_lines() {
        assert_eq!(classify_parent_line("not json"), None);
        assert_eq!(classify_parent_line(r#"{"type":"user","message":{"role":"user","content":"hi"}}"#), None);
        let other = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Read","input":{"to":"x"}}]}}"#;
        assert_eq!(classify_parent_line(other), None, "非 SendMessage 工具调用不构成 resume");
    }

    /// usage 四桶累计 + 首条 timestamp 捕获（timestamp 优先于 usage 在场性）
    #[test]
    fn accumulate_usage_and_first_ts() {
        let mut acc = TokenUsage::default();
        let mut first = None;
        accumulate_agent_line(
            r#"{"timestamp":"2026-10-08T07:36:35.508Z","type":"user","message":{"role":"user","content":"go"}}"#,
            &mut acc, &mut first,
        );
        assert_eq!(first.as_deref(), Some("2026-10-08T07:36:35.508Z"), "首条 user 消息也带 timestamp");
        accumulate_agent_line(&agent_line("2026-10-08T07:36:40Z", 1000, 2000, 0, 500), &mut acc, &mut first);
        accumulate_agent_line(&agent_line("2026-10-08T07:37:00Z", 24, 56000, 7, 8812), &mut acc, &mut first);
        assert_eq!(acc, TokenUsage { input: 1024, cache_read: 58000, cache_creation: 7, output: 9312 });
        assert_eq!(first.as_deref(), Some("2026-10-08T07:36:35.508Z"), "首条 timestamp 不被覆盖");
    }

    /// meta 解析（spec §2.1：{"agentType":"Plan","description":…}）+ 文件名提 id
    #[test]
    fn parse_meta_and_id() {
        let m = parse_meta(r#"{"agentType":"Plan","description":"设计新建会话两改动实现方案","toolUseId":"tu9","spawnDepth":1}"#).unwrap();
        assert_eq!(m.agent_type.as_deref(), Some("Plan"));
        assert_eq!(agent_id_from_meta_name("agent-a86f0c2d.meta.json"), Some("a86f0c2d"));
        assert_eq!(agent_id_from_meta_name("plan.md"), None);
        assert!(parse_meta("broken").is_none());
    }
}
```

- [ ] **Step 2: 跑红**：`cd src-tauri && cargo test subagents::claude` → 编译失败。

- [ ] **Step 3: 实现**（纯函数，无 IO）

```rust
//! Claude 子 agent 运行判据（spec §4.1 事件序状态机；判据权威/架构基准）。
//!
//! 落盘布局（spec §2.1 已修正版——评审 P0-1：父转写在**项目层平铺**，不在会话目录内）：
//!   ~/.claude/projects/<项目>/
//!   ├── <会话uuid>.jsonl                          父转写（通知 / SendMessage 事件源）
//!   └── <会话uuid>/subagents/agent-<agentId>.meta.json   登记 spawn（存在即 running 起点）
//!       <会话uuid>/subagents/agent-<agentId>.jsonl       子转写（timestamp + usage 四桶）
//!
//! 排雷（spec §2.1）：①通知 ≠ 终态（stop 后 SendMessage 续跑，同 id 多次通知）；
//! ②启动即有 tool_result，不能以「无 tool_result」判运行中。
//! **不做 mtime 兜底**（v1 裁决：长静默工具调用会误伤；残留随会话中断整区消失）。

use super::{SubagentView, TokenUsage};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;

/// 父 JSONL 行事件
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ParentEvent {
    /// 子 agent 停止通知（文本含 <task-id>该id</task-id>）
    Stop { agent_id: String },
    /// SendMessage 续跑（tool_use name=SendMessage, input.to=该id）
    Resume { agent_id: String },
}

/// 每子 agent 的最近事件（运行中 = 已登记 ∧ last ∈ {spawn, resume}）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LastEvent {
    /// 无任何事件（meta 已登记）——扫到头未命中即此态
    Spawn,
    Stop,
    Resume,
}

/// 提取消息全部文本（content 字符串 / text 块数组两种形态）
fn message_text(v: &serde_json::Value) -> String {
    match &v["message"]["content"] {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b["type"].as_str() == Some("text"))
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// 文本中提取 <task-id>…</task-id>（无 regex 依赖，朴素查找）
fn task_id_in(text: &str) -> Option<&str> {
    const OPEN: &str = "<task-id>";
    const CLOSE: &str = "</task-id>";
    let start = text.find(OPEN)? + OPEN.len();
    let end = start + text[start..].find(CLOSE)?;
    Some(&text[start..end])
}

/// 行分类（纯函数）：坏 JSON / 无关行 → None
pub(crate) fn classify_parent_line(line: &str) -> Option<ParentEvent> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    // resume 优先：assistant 的 SendMessage tool_use（input.to）
    if v["message"]["role"].as_str() == Some("assistant") {
        if let Some(blocks) = v["message"]["content"].as_array() {
            for b in blocks {
                if b["type"].as_str() == Some("tool_use")
                    && b["name"].as_str() == Some("SendMessage")
                {
                    if let Some(to) = b["input"]["to"].as_str() {
                        return Some(ParentEvent::Resume { agent_id: to.to_string() });
                    }
                }
            }
        }
    }
    // stop：任意消息文本含 task-id 通知块
    let text = message_text(&v);
    task_id_in(&text).map(|id| ParentEvent::Stop { agent_id: id.to_string() })
}

/// 增量路径：文件序应用（后到覆盖——last 语义）
pub(crate) fn apply_event(last: &mut HashMap<String, LastEvent>, ev: &ParentEvent) {
    let (id, val) = match ev {
        ParentEvent::Stop { agent_id } => (agent_id, LastEvent::Stop),
        ParentEvent::Resume { agent_id } => (agent_id, LastEvent::Resume),
    };
    last.insert(id.clone(), val);
}

/// 重建路径：新→旧扫描，每 id 首个（=最新）事件胜出
pub(crate) fn apply_event_rev(seen: &mut HashMap<String, LastEvent>, ev: &ParentEvent) {
    let (id, val) = match ev {
        ParentEvent::Stop { agent_id } => (agent_id, LastEvent::Stop),
        ParentEvent::Resume { agent_id } => (agent_id, LastEvent::Resume),
    };
    seen.entry(id.clone()).or_insert(val);
}

/// 运行中判据（spec §4.1）：已登记 ∧ last ∈ {spawn, resume}。
/// registered=false（meta 消失/未登记）一律不在跑。
pub(crate) fn is_running(registered: bool, last: Option<LastEvent>) -> bool {
    registered && matches!(last, None | Some(LastEvent::Spawn) | Some(LastEvent::Resume))
}

/// 子 agent jsonl 单行累计（纯函数）：usage 四桶累加 + 首条 timestamp 捕获
pub(crate) fn accumulate_agent_line(
    line: &str,
    acc: &mut TokenUsage,
    first_ts: &mut Option<String>,
) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { return };
    if first_ts.is_none() {
        if let Some(ts) = v["timestamp"].as_str() {
            *first_ts = Some(ts.to_string());
        }
    }
    let u = &v["message"]["usage"];
    if !u.is_object() {
        return; // 无 usage 的消息（user/tool_result 等）不累计
    }
    acc.input += u["input_tokens"].as_u64().unwrap_or(0);
    acc.cache_read += u["cache_read_input_tokens"].as_u64().unwrap_or(0);
    acc.cache_creation += u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
    acc.output += u["output_tokens"].as_u64().unwrap_or(0);
}

/// subagents/agent-<id>.meta.json 内容（字段缺失容忍）。
/// **不解析 spawnDepth、不区分深度**（口径明示，评审 P3-5）：spec §3 只收一层
/// 子 agent——claude 的 subagents/ 登记源天然只有一层（深层派生不落此目录），
/// 其余三工具无深度信息可过滤。
#[derive(Deserialize)]
pub(crate) struct AgentMeta {
    #[serde(rename = "agentType")]
    pub(crate) agent_type: Option<String>,
    pub(crate) description: Option<String>,
}

pub(crate) fn parse_meta(text: &str) -> Option<AgentMeta> {
    serde_json::from_str(text).ok()
}

/// `agent-<id>.meta.json` → `<id>`（其余文件名 → None）
pub(crate) fn agent_id_from_meta_name(name: &str) -> Option<&str> {
    name.strip_prefix("agent-")?
        .strip_suffix(".meta.json")
        .filter(|id| !id.is_empty())
}
```

- [ ] **Step 4: 跑绿**：`cd src-tauri && cargo test subagents::claude` → PASS（`is_running(true, None)` 为 true 属预期——无事件=spawn 态）。
- [ ] **Step 5: 门禁 + commit**：

```bash
cd src-tauri && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check
git add src-tauri/src/monitor/subagents/
git commit -m "feat(subagent-chips): claude 事件序状态机判据（纯函数）"
```

### Task 3: claude source——增量缓存与倒序重建

**Files:**
- Modify: `src-tauri/src/monitor/subagents/claude.rs`（追加 source 装配 + 倒序分块扫描 + 集成测试）
- Modify: `src-tauri/src/monitor/subagents/mod.rs`（`pub(crate) mod claude;` 提升可见性，供 T7 生产装配）

- [ ] **Step 1: 写失败测试**（tempdir 合成夹具，零真实 `~/.claude`）

```rust
// claude.rs tests 模块追加（沿用 Task 2 的 notify/sendmsg/agent_line 构造器）

/// 夹具（spec §2.1 已修正版布局，评审 P0-1）：父转写在项目层平铺
/// （proj/<sid>.jsonl），subagents/ 在会话目录内（proj/<sid>/subagents/）；
/// 持有 TempDir 随 drop 清理
struct Fixture { td: tempfile::TempDir, root: PathBuf }
fn claude_fixture(sid: &str) -> Fixture {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().to_path_buf();
    let proj = root.join("proj--e--demo");
    std::fs::create_dir_all(proj.join(sid).join("subagents")).unwrap();
    // 父转写：项目层平铺（与 remote/content.rs find_jsonl_in_projects 的发现位置同层）
    std::fs::write(proj.join(format!("{sid}.jsonl")), "").unwrap();
    Fixture { td, root }
}
impl Fixture {
    /// 父转写路径（项目层平铺）
    fn parent(&self, sid: &str) -> PathBuf { self.root.join("proj--e--demo").join(format!("{sid}.jsonl")) }
    /// 会话目录（内含 subagents/）
    fn session(&self, sid: &str) -> PathBuf { self.root.join("proj--e--demo").join(sid) }
    fn add_agent(&self, sid: &str, id: &str, meta: &str, transcript: &str) {
        let d = self.session(sid).join("subagents");
        std::fs::write(d.join(format!("agent-{id}.meta.json")), meta).unwrap();
        std::fs::write(d.join(format!("agent-{id}.jsonl")), transcript).unwrap();
    }
    fn append(&self, p: &Path, s: &str) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(p).unwrap();
        f.write_all(s.as_bytes()).unwrap();
    }
}

/// 全链路：登记 → 出 chip（名/时长/token 四桶）；追加通知 → 消失；追加 SendMessage → 复现（P0 集成层）
#[test]
fn claude_source_lifecycle() {
    super::super::reset_cache_for_tests();
    let f = claude_fixture("sess-1");
    f.add_agent("sess-1", "a86f",
        r#"{"agentType":"Plan","description":"设计新建会话两改动实现方案"}"#,
        &format!("{}\n{}\n",
            agent_line("2026-10-08T07:36:35.508Z", 5124, 56448, 0, 9312),
            agent_line("2026-10-08T07:37:00Z", 100, 0, 0, 200)));
    let v = collect_with(f.root.path(), "sess-1");
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].id, "a86f");
    assert_eq!(v[0].name, "Plan");
    assert_eq!(v[0].description.as_deref(), Some("设计新建会话两改动实现方案"));
    assert_eq!(v[0].spawn_ts.as_deref(), Some("2026-10-08T07:36:35.508Z"));
    assert_eq!(v[0].tokens, TokenUsage { input: 5224, cache_read: 56448, cache_creation: 0, output: 9512 });
    // ② 追加停止通知 → 消失
    f.append(&f.parent("sess-1"), &(notify("a86f") + "\n"));
    assert!(collect_with(f.root.path(), "sess-1").is_empty());
    // ③ 追加 SendMessage → 复现（增量路径，P0）
    f.append(&f.parent("sess-1"), &(sendmsg("a86f") + "\n"));
    assert_eq!(collect_with(f.root.path(), "sess-1").len(), 1);
    // ④ token 增量：转写追加一行，四桶只加新增
    f.append(&f.session("sess-1").join("subagents").join("agent-a86f.jsonl"),
             &(agent_line("2026-10-08T07:38:00Z", 1, 2, 3, 4) + "\n"));
    let v = collect_with(f.root.path(), "sess-1");
    assert_eq!(v[0].tokens, TokenUsage { input: 5225, cache_read: 56450, cache_creation: 3, output: 9516 });
    super::super::reset_cache_for_tests();
}

/// 重启重建（缓存缺失）：倒序分块扫父文件收齐事件；agent 无任何事件 → spawn 态运行中
#[test]
fn claude_source_rebuild_after_restart() {
    super::super::reset_cache_for_tests();
    let f = claude_fixture("sess-2");
    f.add_agent("sess-2", "b1", r#"{"agentType":"Explore"}"#, &agent_line("2026-10-08T07:00:00Z", 1, 0, 0, 1) + "\n");
    f.add_agent("sess-2", "b2", r#"{"agentType":"Plan"}"#, "");
    // b1 有停止通知、b2 无事件（→ spawn 态运行中）
    f.append(&f.parent("sess-2"), &(notify("b1") + "\n"));
    let v = collect_with(f.root.path(), "sess-2");
    assert_eq!(v.len(), 1, "b1 已停止不上板，b2 无事件 = spawn 态在板");
    assert_eq!(v[0].id, "b2");
    super::super::reset_cache_for_tests();
}

/// P1-1 回归锁：重建时刻父文件尾恰为半行通知 → 增量游标不得跳过它——
/// 半行补齐换行后，增量路径必须消费到该 stop 事件（chip 消失）
#[test]
fn claude_source_rebuild_consumes_tail_partial_line() {
    super::super::reset_cache_for_tests();
    let f = claude_fixture("sess-6");
    f.add_agent("sess-6", "d1", r#"{"agentType":"Plan"}"#, "");
    // 完整无关行 + 半行通知（无结尾换行）
    std::fs::write(f.parent("sess-6"), format!("{{}}\n{}", r#"{"filler":1}"#, notify("d1"))).unwrap();
    // 重建：半行不可解析 → d1 无事件 = spawn 态在板
    assert_eq!(collect_with(f.root.path(), "sess-6").len(), 1);
    // 补齐换行：增量路径消费该通知 → 下板
    f.append(f.parent("sess-6"), "\n");
    assert!(collect_with(f.root.path(), "sess-6").is_empty(), "尾半行事件不得被 skip_to 丢弃");
    super::super::reset_cache_for_tests();
}

/// 护栏：父文件重写变短（size<offset）→ 作废重建，不 panic、不丢登记
#[test]
fn claude_source_parent_truncated_rebuilds() {
    super::super::reset_cache_for_tests();
    let f = claude_fixture("sess-3");
    f.add_agent("sess-3", "c1", r#"{"agentType":"Plan"}"", "");
    f.append(&f.parent("sess-3"), &(notify("c1") + "\n"));
    assert!(collect_with(f.root.path(), "sess-3").is_empty());
    std::fs::write(f.parent("sess-3"), "x\n").unwrap(); // 重写变短
    let v = collect_with(f.root.path(), "sess-3");
    assert_eq!(v.len(), 1, "护栏作废后重建：c1 无事件 → spawn 态在板");
    super::super::reset_cache_for_tests();
}

/// 会话目录不存在 / 无 subagents 目录 → 空态（发现层防御，非判据）
#[test]
fn claude_source_missing_layout_is_empty() {
    super::super::reset_cache_for_tests();
    let f = claude_fixture("sess-4");
    assert!(collect_with(f.root.path(), "nope").is_empty());
    std::fs::create_dir_all(f.root.path().join("proj--e--demo").join("flat")).unwrap(); // 无 subagents/ 的目录
    assert!(collect_with(f.root.path(), "flat").is_empty());
    super::super::reset_cache_for_tests();
}

/// spawnTs 升序（None 排尾）
#[test]
fn claude_source_orders_by_spawn_ts() {
    super::super::reset_cache_for_tests();
    let f = claude_fixture("sess-5");
    f.add_agent("sess-5", "late", r#"{"agentType":"A"}"#, &agent_line("2026-10-08T09:00:00Z", 1, 0, 0, 1) + "\n");
    f.add_agent("sess-5", "early", r#"{"agentType":"B"}"#, &agent_line("2026-10-08T07:00:00Z", 1, 0, 0, 1) + "\n");
    let v = collect_with(f.root.path(), "sess-5");
    let ids: Vec<&str> = v.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, ["early", "late"]);
    super::super::reset_cache_for_tests();
}
```

- [ ] **Step 2: 跑红**：`cd src-tauri && cargo test subagents::claude` → `collect_with` 未定义。

- [ ] **Step 3: 实现**（source 装配 + 倒序分块扫描）

```rust
// claude.rs 追加（use 补 super::{read_increment, IncrRead, IncrState, update_entry, CacheBox, ms_to_iso 无需——timestamp 已是 ISO}）

/// 会话目录发现（发现层，非判据；spec §2.1 已修正版——评审 P0-1）：父转写在
/// **项目层平铺**（`projects/<项目>/<sid>.jsonl`，与 remote/content.rs:452-469
/// find_jsonl_in_projects 同一发现模式），登记源 subagents/ 在会话目录内
/// （`projects/<项目>/<sid>/subagents/`）。两处齐备才算命中：无 subagents/ 的
/// 目录（旧平铺布局 / 无子 agent 会话）天然空态。父 jsonl 文件缺失不阻断发现
/// （read_increment 对缺失文件返回空行 → 无事件 = spawn 态，残留口径 §8.2 申报）。
fn locate(projects_root: &Path, session_id: &str) -> Option<(PathBuf, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(projects_root) else { return None };
    for e in entries.flatten() {
        let proj = e.path();
        if !proj.is_dir() {
            continue;
        }
        let subagents = proj.join(session_id).join("subagents");
        if subagents.is_dir() {
            return Some((proj.join(format!("{session_id}.jsonl")), subagents));
        }
    }
    None
}

/// meta 登记（readdir subagents/*.meta.json）
struct RegAgent { id: String, meta: AgentMeta }
fn registered_agents(subagents_dir: &Path) -> Vec<RegAgent> {
    let Ok(entries) = std::fs::read_dir(subagents_dir) else { return Vec::new() };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if let Some(id) = agent_id_from_meta_name(&name) {
            if let Ok(text) = std::fs::read_to_string(e.path()) {
                if let Some(meta) = parse_meta(&text) {
                    out.push(RegAgent { id: id.to_string(), meta });
                }
            }
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id)); // 注册序稳定（spawnTs 同值时的次序确定性）
    out
}

/// 会话级缓存条目（spec §5.1）
struct ClaudeCache {
    parent: IncrState,
    last: HashMap<String, LastEvent>,
    agents: HashMap<String, AgentTrack>,
}
struct AgentTrack { incr: IncrState, tokens: TokenUsage, first_ts: Option<String> }

/// 倒序分块扫描（重建路径，spec §5.1）：从文件尾向头按 256KB 取块，行序「新→旧」。
/// visitor 返回 true = 登记集收齐即停（不再读更早的块）。
/// 块边界语义：JSONL 行以 '\n' **右终结**——[块 + carry] 切段后，最后一段（其后无
/// '\n'）是跨块半行，进 carry 与左邻块拼接；其余段皆完整可访问。
/// **返回值 = 文件尾未终结半行字节**（空 = 以 '\n' 收尾）——carry 恒等于已处理
/// 区域最后一个 '\n' 之后缀，而处理区域总达文件尾，故 carry 非空 ⟺ 文件尾有未终结
/// 行（早停亦然）。调用方据此把增量游标对齐到 size − carry.len()（T1 不变式），该半行的
/// 事件留给增量路径补齐后消费——评审 P1-1：skip_to(size) 会把它永久丢失。
fn scan_parent_rev(path: &Path, mut visit: impl FnMut(&str) -> bool) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    const BLOCK: u64 = 256 * 1024;
    let mut f = std::fs::File::open(path)?;
    let size = f.metadata()?.len();
    let mut pos = size;
    let mut carry: Vec<u8> = Vec::new(); // 右邻块的跨块半行（文件序在块后）
    while pos > 0 {
        let start = pos.saturating_sub(BLOCK);
        f.seek(SeekFrom::Start(start))?;
        let take = (pos - start) as usize;
        let mut buf = vec![0u8; take];
        f.read_exact(&mut buf)?;
        buf.extend_from_slice(&carry);
        carry.clear();
        let mut complete: Vec<(usize, usize)> = Vec::new(); // [a,b) 完整行字节区间
        let mut seg = 0usize;
        for (i, &b) in buf.iter().enumerate() {
            if b == b'\n' {
                complete.push((seg, i));
                seg = i + 1;
            }
        }
        carry = buf[seg..].to_vec();
        for (a, b) in complete.iter().rev() {
            if visit(&String::from_utf8_lossy(&buf[*a..*b])) {
                return Ok(carry); // 登记集收齐即停（carry 已是本块尾半行）
            }
        }
        pos = start;
    }
    Ok(carry)
}
```

**边界测试补充（收齐即停的读计数断言）**：


```rust
#[test]
fn scan_rev_visits_newest_first_and_stops_early() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("parent.jsonl");
    let mut body = String::new();
    for i in 0..100 { body.push_str(&format!("{{\"n\":{i}}}\n")); }
    std::fs::write(&p, body).unwrap();
    let mut seen: Vec<u32> = Vec::new();
    let carry = scan_parent_rev(&p, |line| {
        let n: u32 = serde_json::from_str::<serde_json::Value>(line).unwrap()["n"].as_u64().unwrap() as u32;
        seen.push(n);
        n == 90 // 第 90 行（新→旧第 10 个）命中即停
    }).unwrap();
    assert!(carry.is_empty(), "以换行收尾的文件无尾半行（P1-1 返回值语义）");
    assert_eq!(seen.first(), Some(&99), "新→旧");
    assert_eq!(seen.last(), Some(&90));
    assert_eq!(seen.len(), 10, "收齐即停：不扫到头");
}
```

主装配：

```rust
/// 生产入口（T7 经 RemoteState 缝调用）
pub fn collect(session_id: &str) -> Vec<SubagentView> {
    let Some(root) = dirs::home_dir().map(|h| h.join(".claude").join("projects")) else {
        return Vec::new();
    };
    collect_with(&root, session_id)
}

/// 可注入核心（projects 根注入；测试 tempdir）
pub(crate) fn collect_with(projects_root: &Path, session_id: &str) -> Vec<SubagentView> {
    let Some((parent_jsonl, subagents_dir)) = locate(projects_root, session_id) else {
        return Vec::new();
    };
    let reg = registered_agents(&subagents_dir);
    // 闭包 move parent_jsonl + subagents_dir + reg（发现结果一次取齐，step 不再反推路径）
    update_entry("claude", session_id, move |cur| {
        let fresh = cur.is_none(); // 缓存缺失/被清（MAM 重启 / 硬上限清空）→ 走重建
        let mut st = cur
            .and_then(|b| b.downcast::<ClaudeCache>().ok())
            .map(|a| Arc::try_unwrap(a).unwrap_or_else(|a| (*a).clone()))
            .unwrap_or_default();
        if fresh {
            rebuild(&parent_jsonl, &reg, &mut st); // 倒序扫父文件收齐事件 + 游标对齐尾半行
            // agent 转写无游标 → 全量读累计（rebuild 后 step 的 IncrState 从 0 起，
            // 首轮 read_increment 自然全量消费——无需专门分支）
        }
        let out = step(&parent_jsonl, &subagents_dir, &reg, &mut st);
        (Arc::new(st) as CacheBox, out)
    })
}

fn step(parent_jsonl: &Path, subagents_dir: &Path, reg: &[RegAgent], st: &mut ClaudeCache) -> Vec<SubagentView> {
    // ① 父文件事件（增量；reset → 清空状态机后按全量行重放）
    match read_increment(parent_jsonl, &mut st.parent) {
        IncrRead::Unchanged => {}
        IncrRead::Lines { lines, reset } => {
            if reset { st.last.clear(); }
            for line in &lines {
                if let Some(ev) = classify_parent_line(line) {
                    apply_event(&mut st.last, &ev);
                }
            }
        }
    }
    // ② 每 agent 转写增量累计（reset → 清空累计后按全量行重放）
    for a in reg {
        let track = st.agents.entry(a.id.clone()).or_default();
        // 评审 P0-1：subagents 目录来自 locate() 的发现结果，不从父路径反推
        // （父在项目层平铺，parent() 反推会指到错误目录）
        let path = subagents_dir.join(format!("agent-{}.jsonl", a.id));
        match read_increment(&path, &mut track.incr) {
            IncrRead::Unchanged => {}
            IncrRead::Lines { lines, reset } => {
                if reset { track.tokens = TokenUsage::default(); track.first_ts = None; }
                for line in &lines {
                    accumulate_agent_line(line, &mut track.tokens, &mut track.first_ts);
                }
            }
        }
    }
    // ③ 视图：已登记 ∧ 运行中（spawnTs 升序，None 排尾）
    let mut views: Vec<SubagentView> = reg.iter()
        .filter(|a| is_running(true, st.last.get(&a.id).copied()))
        .map(|a| {
            let t = st.agents.get(&a.id);
            SubagentView {
                id: a.id.clone(),
                name: a.meta.agent_type.clone()
                    .unwrap_or_else(|| format!("agent-{}", a.id)),
                description: a.meta.description.clone(),
                spawn_ts: t.and_then(|t| t.first_ts.clone()),
                tokens: t.map(|t| t.tokens).unwrap_or_default(),
            }
        })
        .collect();
    sort_views(&mut views);
    views
}

/// 首次进缓存（重建路径）：倒序分块扫父文件收齐登记集事件 + agent 转写全量读
fn rebuild(parent_jsonl: &Path, reg: &[RegAgent], st: &mut ClaudeCache) {
    let wanted: std::collections::HashSet<&str> = reg.iter().map(|a| a.id.as_str()).collect();
    let mut got: std::collections::HashSet<String> = std::collections::HashSet::new();
    let tail_partial = scan_parent_rev(parent_jsonl, |line| {
        if let Some(ev) = classify_parent_line(line) {
            let id = match &ev {
                ParentEvent::Stop { agent_id } | ParentEvent::Resume { agent_id } => agent_id.clone(),
            };
            apply_event_rev(&mut st.last, &ev);
            if wanted.contains(id.as_str()) {
                got.insert(id);
            }
        }
        wanted.len() == got.len() // 登记集收齐即停；扫到头仍有未命中 = spawn 态（is_running 对 None 放行）
    })
    .unwrap_or_default(); // 读失败 → 无尾半行可续（下次 collect 由 last_size 兜底语义接管）
    // 增量游标对齐 T1 不变式（评审 P1-1）：partial = 文件尾未终结半行——重建时刻
    // 它的事件未被消费，skip_to(size) 会永久跳过；补齐换行后由增量路径消费
    if let Ok(m) = std::fs::metadata(parent_jsonl) {
        st.parent.resume_at(m.len(), tail_partial);
    }
}
```

**实现提示**：①`update_entry` 闭包拿回的是 `Arc<ClaudeCache>`——为可变借用，给 `ClaudeCache` 派生 `Clone`（成员皆 Clone）并 `Arc::try_unwrap(...).unwrap_or_else(|a| (*a).clone())`；②给 `IncrState` 补 `pub(crate) fn resume_at(&mut self, size: u64, partial: Vec<u8>)`（重建后对齐增量游标：offset = size − partial.len()、last_size = size、partial = 文件尾半行——T1 不变式不被破坏，评审 P1-1）；③`sort_views` 是公共排序（spawnTs 升序、None 排尾、稳定），放 `mod.rs` 供四工具复用：

```rust
// mod.rs 追加（T3 一并交付，T4/T6 复用）
/// 端点输出序（spec §5.2）：spawnTs 升序，None 排尾（无时间不可比），稳定排序
pub(crate) fn sort_views(v: &mut [SubagentView]) {
    v.sort_by(|a, b| match (&a.spawn_ts, &b.spawn_ts) {
        (Some(x), Some(y)) => x.cmp(y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
}
```

- [ ] **Step 4: 跑绿**：`cd src-tauri && cargo test subagents::claude` → PASS（含 P0 集成层 + 重建 + 护栏 + 顺序）。
- [ ] **Step 4b: 布局实机复核（阻塞项——评审 P0-1 并入，不再推迟到 C3）**：`ls ~/.claude/projects/<项目>/`
  抽查一个带子 agent 的会话，确认两处落点与 locate()/夹具一致——父转写 `<会话uuid>.jsonl`
  在**项目层**、`<会话uuid>/subagents/` 在**会话目录内**。不一致即停：勿凭猜测改判据或发现层，
  回报用户裁决（布局是发现层输入，改错方向 = stop/resume 永远读不到 → chip 永不消失）。
- [ ] **Step 5: 门禁 + commit**：

```bash
cd src-tauri && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check
git add src-tauri/src/monitor/subagents/
git commit -m "feat(subagent-chips): claude 子 agent source——增量缓存与倒序重建"
```

### Task 4: opencode source（SQLite 直查，无缓存）

**Files:**
- Create: `src-tauri/src/monitor/subagents/opencode.rs`
- Modify: `src-tauri/src/monitor/subagents/mod.rs`（`mod opencode;`）

- [ ] **Step 1: 写失败测试**（tempdir 建文件库；建表先例 `v2_db` opencode_parser.rs:1512——注意其表**缺** parent_id/agent/token 五桶列，本组夹具自建全列表）

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// 建 v2 库（列齐全版——含 token 五桶与 parent_id；先例：opencode_parser tests）
    fn make_db(p: &Path) -> Connection {
        let conn = Connection::open(p).unwrap();
        conn.execute_batch(
            "CREATE TABLE session_v2 (id TEXT PRIMARY KEY, project_id TEXT, directory TEXT, title TEXT,
                 parent_id TEXT, agent TEXT, tokens_input INTEGER DEFAULT 0, tokens_output INTEGER DEFAULT 0,
                 tokens_reasoning INTEGER DEFAULT 0, tokens_cache_read INTEGER DEFAULT 0,
                 tokens_cache_write INTEGER DEFAULT 0, time_created INTEGER, time_updated INTEGER,
                 time_idle INTEGER, idle_outcome TEXT, version TEXT);",
        ).unwrap();
        conn
    }
    fn ins(conn: &Connection, id: &str, parent: Option<&str>, agent: Option<&str>,
           in_t: i64, out_t: i64, cr: i64, cw: i64, created: i64, updated: i64) {
        conn.execute(
            "INSERT INTO session_v2 (id, parent_id, agent, tokens_input, tokens_output,
                 tokens_cache_read, tokens_cache_write, time_created, time_updated)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            rusqlite::params![id, parent, agent, in_t, out_t, cr, cw, created, updated],
        ).unwrap();
    }

    /// 活跃子会话出板：parent_id 过滤 + 桶位映射（§4.2）+ agent 名 + spawnTs
    #[test]
    fn active_child_maps_and_filters() {
        let td = tempfile::tempdir().unwrap();
        let db = td.path().join("opencode.db");
        let conn = make_db(&db);
        let now = 2_000_000_000_000i64;
        ins(&conn, "main-1", None, None, 0, 0, 0, 0, now - 9_000_000, now);
        ins(&conn, "child-1", Some("main-1"), Some("explore"), 5124, 9312, 56448, 7, now - 60_000, now - 30_000);
        ins(&conn, "child-2", Some("main-1"), Some("general"), 1, 2, 3, 4, now - 9_000_000, now - 30_000);
        ins(&conn, "orphan", None, Some("explore"), 1, 1, 1, 1, now - 1000, now); // parent_id NULL → 非子
        drop(conn);
        let v = collect_with(&db, "main-1", now);
        assert_eq!(v.len(), 2, "parent_id NULL 不算子 agent");
        assert_eq!(v[0].id, "child-1"); // time_created 升序
        assert_eq!(v[0].name, "explore");
        assert_eq!(v[0].tokens, TokenUsage { input: 5124, cache_read: 56448, cache_creation: 7, output: 9312 });
        assert_eq!(v[0].spawn_ts, ms_to_iso(now - 60_000));
        // 换会话查 → 空（parent 过滤）
        assert!(collect_with(&db, "main-2", now).is_empty());
    }

    /// 活跃度边界（spec §4.2 严格小于）：恰好 90s → 过期；89.999s → 活跃。
    /// 「停止」即活跃度自然过期，无需显式终止事件。
    #[test]
    fn active_boundary_expires_at_90s() {
        let td = tempfile::tempdir().unwrap();
        let db = td.path().join("opencode.db");
        let conn = make_db(&db);
        let now = 2_000_000_000_000i64;
        ins(&conn, "c90", Some("m"), Some("a"), 0, 0, 0, 0, now - 100_000, now - 90_000);
        ins(&conn, "c89", Some("m"), Some("a"), 0, 0, 0, 0, now - 100_000, now - 89_999);
        drop(conn);
        let v = collect_with(&db, "m", now);
        assert_eq!(v.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["c89"]);
    }

    /// 长静默子 agent 暂时隐藏（spec §8.3 如实口径）+ v1 库（无 session_v2）空态 + 库缺失空态
    #[test]
    fn stale_child_hidden_v1_and_missing_db_empty() {
        let td = tempfile::tempdir().unwrap();
        let db = td.path().join("opencode.db");
        let conn = make_db(&db);
        let now = 2_000_000_000_000i64;
        ins(&conn, "stale", Some("m"), Some("a"), 0, 0, 0, 0, now - 400_000, now - 200_000);
        drop(conn);
        assert!(collect_with(&db, "m", now).is_empty(), "超窗隐藏");
        // v1：只有 session 表（无 session_v2）
        let v1db = td.path().join("v1.db");
        let c1 = Connection::open(&v1db).unwrap();
        c1.execute_batch("CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT, title TEXT, time_updated INTEGER);").unwrap();
        drop(c1);
        assert!(collect_with(&v1db, "m", now).is_empty(), "1.x 库无 session_v2 → 空态（判据单点 schema_is_v2）");
        assert!(collect_with(&td.path().join("missing.db"), "m", now).is_empty());
    }

    /// agent 名缺失 → 回落 id 前 8 位；tokens_reasoning 不入四桶（§4.2 申报）
    #[test]
    fn name_fallback_and_reasoning_excluded() {
        let td = tempfile::tempdir().unwrap();
        let db = td.path().join("opencode.db");
        let conn = make_db(&db);
        let now = 2_000_000_000_000i64;
        conn.execute(
            "INSERT INTO session_v2 (id, parent_id, agent, tokens_input, tokens_output, tokens_reasoning,
                 tokens_cache_read, tokens_cache_write, time_created, time_updated)
             VALUES ('abcdefgh1234','m',NULL,100,200,9999,50,10,?,?)",
            rusqlite::params![now - 1000, now],
        ).unwrap();
        drop(conn);
        let v = collect_with(&db, "m", now);
        assert_eq!(v[0].name, "abcdefgh");
        assert_eq!(v[0].tokens.total(), 360, "reasoning 9999 不计入（100+50+10+200）");
    }
}
```

- [ ] **Step 2: 跑红**：`cargo test subagents::opencode` → 编译失败。

- [ ] **Step 3: 实现**

```rust
//! OpenCode 子 agent source（spec §4.2：活跃度判据，数据模型最优）。
//! `session_v2.parent_id` 非空即子 agent 会话；运行中 = time_updated 距 now < 90s
//! （「停止」即活跃度自然过期，无显式终止事件；time_idle 不用于判据——已完成会话
//! 亦为 NULL，spec §2.2）。SQLite 查询即最新：**无缓存**（豁免 L2/L3 预算纪律）。
//! 桶位映射：tokens_input→input、cache_read→cacheRead、cache_write→cacheCreation、
//! output→output；tokens_reasoning 不计入展示合计（§4.2）。

use super::{ms_to_iso, sort_views, SubagentView, TokenUsage, ACTIVE_SECS};
use std::path::Path;

/// 生产入口
pub fn collect(session_id: &str) -> Vec<SubagentView> {
    let Some(db) = dirs::home_dir().map(|h| {
        h.join(".local").join("share").join("opencode").join("opencode.db")
    }) else { return Vec::new() };
    collect_with(&db, session_id, chrono::Utc::now().timestamp_millis())
}

/// 可注入核心（db 路径 / now 毫秒）
pub(crate) fn collect_with(db_path: &Path, session_id: &str, now_ms: i64) -> Vec<SubagentView> {
    let Some(conn) = crate::monitor::sqlite::open_readonly_with_timeout(db_path) else {
        return Vec::new();
    };
    // 2.x 判据单点（opencode_parser::schema_is_v2，禁止第二实现）
    if !crate::monitor::opencode_parser::schema_is_v2(&conn) {
        return Vec::new();
    }
    let sql = "SELECT id, agent, tokens_input, tokens_cache_read, tokens_cache_write, \
               tokens_output, time_created, time_updated FROM session_v2 \
               WHERE parent_id IS NOT NULL AND parent_id = ?1 ORDER BY time_created ASC";
    let Ok(mut stmt) = conn.prepare(sql) else { return Vec::new() };
    let rows = stmt.query_map([session_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, i64>(6)?,
            row.get::<_, i64>(7)?,
        ))
    });
    let mut views = Vec::new();
    if let Ok(rows) = rows {
        for r in rows.flatten() {
            let (id, agent, tin, tcr, tcw, tout, created, updated) = r;
            // 活跃过滤（毫秒域严格小于；§4.2）
            if now_ms.saturating_sub(updated) >= (ACTIVE_SECS as i64) * 1000 {
                continue;
            }
            views.push(SubagentView {
                name: agent.unwrap_or_else(|| id.chars().take(8).collect()),
                id,
                description: None,
                spawn_ts: ms_to_iso(created),
                tokens: TokenUsage {
                    input: tin.max(0) as u64,
                    cache_read: tcr.max(0) as u64,
                    cache_creation: tcw.max(0) as u64,
                    output: tout.max(0) as u64,
                },
            });
        }
    }
    sort_views(&mut views);
    views
}
```

- [ ] **Step 4: 跑绿**：`cargo test subagents::opencode` → PASS。
- [ ] **Step 5: 门禁 + commit**：

```bash
cd src-tauri && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check
git add src-tauri/src/monitor/subagents/
git commit -m "feat(subagent-chips): opencode 子 agent source（SQLite 直查）"
```

### Task 5: kimi source（wire 增量累计）

**Files:**
- Create: `src-tauri/src/monitor/subagents/kimi.rs`
- Modify: `src-tauri/src/monitor/kimi_parser.rs`（两行，不改语义：①:63 `KimiDataRoot` 补 `#[derive(Clone)]`——现无任何 derive，测试/装配需按值多次传递；②:96 `kimi_data_root()` 私有 → `pub(crate)`——评审 P2-3：collect() 复用数据根判定单点，不内联第二份 env/home 逻辑）
- Modify: `src-tauri/src/monitor/subagents/mod.rs`（`mod kimi;`）

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::kimi_parser::{resolve_data_root, KimiDataRoot};

    /// 夹具：.kimi-code 根 + session_index.jsonl + sessions/<wd>/<sid>/agents/<dir>/wire.jsonl
    fn kimi_fixture(sid: &str, agents: &[(&str, &str)]) -> (tempfile::TempDir, KimiDataRoot) {
        let td = tempfile::tempdir().unwrap();
        let home = td.path().join("khome");
        let sess = home.join("sessions").join("wd-1").join(sid);
        for (dir, wire) in agents {
            let d = sess.join("agents").join(dir);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("wire.jsonl"), wire).unwrap();
        }
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("session_index.jsonl"),
            format!(r#"{{"sessionId":"{sid}","sessionDir":"sessions/wd-1/{sid}","workDir":"E:/demo"}}"#)).unwrap();
        let root = resolve_data_root(Some(home.to_str().unwrap()), td.path()).unwrap();
        (td, root)
    }
    fn wire(body: &str) -> String {
        format!("{{\"type\":\"metadata\",\"created_at\":1760000000000}}\n{body}")
    }
    fn usage(inp: u64, out: u64, cr: u64, cc: u64) -> String {
        // usage.record 四桶 camelCase（spec §2.3 实盘形态）
        format!("{{\"type\":\"usage.record\",\"time\":1760000001000,\"usage\":{{\"inputOther\":{inp},\"output\":{out},\"inputCacheRead\":{cr},\"inputCacheCreation\":{cc}}}}}\n")
    }

    /// main 排除 + wire 缺失跳过 + mtime 活跃 + usage 累计 + spawnTs（created_at 毫秒）
    #[test]
    fn kimi_source_scans_agents_dir() {
        super::super::reset_cache_for_tests();
        let (_td, root) = kimi_fixture("ks-1", &[
            ("main", &wire(&usage(9, 9, 9, 9))),                    // main 排除（§4.3）
            ("agent-0", &wire(&usage(100, 200, 300, 400))),
            ("agent-1", ""),                                        // 空 wire 也登记（存在即候选）
            ("empty-dir", ""),                                      // 无 wire.jsonl → 跳过（构造后删 wire）
        ]);
        std::fs::remove_file(root.sessions.join("wd-1").join("ks-1").join("agents").join("empty-dir").join("wire.jsonl")).unwrap();
        // now = wire mtime + 10s（活跃）
        let mtime = std::fs::metadata(root.sessions.join("wd-1").join("ks-1").join("agents").join("agent-0").join("wire.jsonl"))
            .unwrap().modified().unwrap();
        let v = collect_with(root.clone(), "ks-1", mtime + std::time::Duration::from_secs(10));
        let ids: Vec<&str> = v.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(ids, ["agent-0", "agent-1"], "main 排除、无 wire 跳过、目录名即名称（§4.3 v1）");
        assert_eq!(v[0].tokens, TokenUsage { input: 100, cache_read: 300, cache_creation: 400, output: 200 });
        assert_eq!(v[0].spawn_ts, ms_to_iso(1_760_000_000_000));
        // mtime 超 90s → 隐藏（§8.4 长静默如实口径）
        let v = collect_with(root.clone(), "ks-1", mtime + std::time::Duration::from_secs(91));
        assert!(v.is_empty());
        super::super::reset_cache_for_tests();
    }

    /// 增量：第二轮只累计追加的 usage.record；重建（缓存清空）结果一致
    #[test]
    fn kimi_source_incremental_and_rebuild() {
        super::super::reset_cache_for_tests();
        let (_td, root) = kimi_fixture("ks-2", &[("agent-0", &wire(&usage(1, 2, 3, 4)))]);
        let wire_path = root.sessions.join("wd-1").join("ks-2").join("agents").join("agent-0").join("wire.jsonl");
        let mtime = std::fs::metadata(&wire_path).unwrap().modified().unwrap();
        let now = || mtime + std::time::Duration::from_secs(5);
        assert_eq!(collect_with(root.clone(), "ks-2", now())[0].tokens.total(), 10);
        // 追加一条 usage.record（mtime 变新）
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&wire_path).unwrap();
        f.write_all(usage(10, 20, 30, 40).as_bytes()).unwrap();
        drop(f);
        let mtime2 = std::fs::metadata(&wire_path).unwrap().modified().unwrap();
        assert_eq!(collect_with(root.clone(), "ks-2", mtime2 + std::time::Duration::from_secs(5))[0].tokens.total(), 110,
            "增量只加新增（1+2+3+4 + 10+20+30+40）");
        // 重启重建
        super::super::reset_cache_for_tests();
        assert_eq!(collect_with(root.clone(), "ks-2", mtime2 + std::time::Duration::from_secs(5))[0].tokens.total(), 110);
        super::super::reset_cache_for_tests();
    }

    /// P1-2：sessionDir 越界（../../ 逃逸 / 任意绝对路径）→ 空态（信任边界与
/// kimi_parser::stat_index_entry 同款——防索引行指向 sessions 根之外）
    #[test]
    fn kimi_source_rejects_out_of_root_session_dir() {
        super::super::reset_cache_for_tests();
        let td = tempfile::tempdir().unwrap();
        let home = td.path().join("khome");
        std::fs::create_dir_all(home.join("sessions")).unwrap();
        // 两行坏索引：相对逃逸 + 绝对路径；均不得出数据
        std::fs::write(home.join("session_index.jsonl"), concat!(
            r#"{"sessionId":"bad1","sessionDir":"../../escape","workDir":"E:/x"}"#, "\n",
            r#"{"sessionId":"bad2","sessionDir":"C:/Windows/Temp","workDir":"E:/x"}"#, "\n",
        )).unwrap();
        let root = resolve_data_root(Some(home.to_str().unwrap()), td.path()).unwrap();
        assert!(collect_with(root.clone(), "bad1", std::time::SystemTime::now()).is_empty());
        assert!(collect_with(root, "bad2", std::time::SystemTime::now()).is_empty());
        super::super::reset_cache_for_tests();
    }

    /// 会话不在索引 → 空态（复用注册表，不另写路径推导）
    #[test]
    fn kimi_source_unknown_session_empty() {
        super::super::reset_cache_for_tests();
        let (_td, root) = kimi_fixture("ks-3", &[("agent-0", &wire(&usage(1,1,1,1)))]);
        assert!(collect_with(root, "nope", std::time::SystemTime::now()).is_empty());
        super::super::reset_cache_for_tests();
    }
}
```

- [ ] **Step 2: 跑红**：`cargo test subagents::kimi` → 编译失败。

- [ ] **Step 3: 实现**

```rust
//! Kimi 子 agent source（spec §4.3：活跃度判据 + 两点待实机校准）。
//! 子 agent = agents/<dir>/wire.jsonl 存在 ∧ dir != "main"；运行中 = mtime 距
//! now < 90s（与 opencode §4.2 同取值）。会话目录定位**复用 kimi_parser 会话注册表**
//! （session_index.jsonl + resolve_session_dir），不另写路径推导。
//! v1 名称显示目录名（agent-N）；实机校准后换真名（§十 C1，校准只改本文件）。

use super::{active_within, json_time_to_iso, read_increment, IncrRead, IncrState, sort_views,
            update_entry, CacheBox, SubagentView, TokenUsage};
use crate::monitor::kimi_parser::{parse_session_index, resolve_data_root, resolve_session_dir, KimiDataRoot};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

/// wire.jsonl 单行累计（纯函数）：usage.record 四桶 + 首行 metadata.created_at
pub(crate) fn accumulate_wire_line(line: &str, acc: &mut TokenUsage, created: &mut Option<String>) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { return };
    match v["type"].as_str() {
        Some("metadata") => {
            if created.is_none() {
                *created = json_time_to_iso(&v["created_at"]);
            }
        }
        Some("usage.record") => {
            // usage 对象在场则取之，否则事件平铺（双形态容忍；C1 校准后收严）
            let u = if v["usage"].is_object() { &v["usage"] } else { &v };
            acc.input += u["inputOther"].as_u64().unwrap_or(0);
            acc.output += u["output"].as_u64().unwrap_or(0);
            acc.cache_read += u["inputCacheRead"].as_u64().unwrap_or(0);
            acc.cache_creation += u["inputCacheCreation"].as_u64().unwrap_or(0);
        }
        _ => {}
    }
}

/// 会话级缓存条目（每 agent 目录一份增量游标）
#[derive(Default, Clone)]
struct KimiCache { agents: HashMap<String, AgentTrack> }
#[derive(Default, Clone)]
struct AgentTrack { incr: IncrState, tokens: TokenUsage, created: Option<String> }

pub fn collect(session_id: &str) -> Vec<SubagentView> {
    // 数据根判定复用 kimi_parser 单点（评审 P2-3：不内联第二份 env/home 逻辑）
    let Some(root) = crate::monitor::kimi_parser::kimi_data_root() else {
        return Vec::new();
    };
    collect_with(root, session_id, SystemTime::now())
}

pub(crate) fn collect_with(root: KimiDataRoot, session_id: &str, now: SystemTime) -> Vec<SubagentView> {
    // ① 注册表定位会话目录（零路径推导复制）
    let Some(dir) = parse_session_index(&root).into_iter()
        .find(|e| e.session_id == session_id)
        .map(|e| resolve_session_dir(&root, &e.session_dir))
    else { return Vec::new() };
    // 信任边界（评审 P1-2，kimi_parser::stat_index_entry 同款，kimi_parser.rs:276-284）：
    // sessionDir 只允许落在 sessions 根之下——越界（含 ../ 逃逸、任意绝对路径）跳过，不误报
    if !dir.starts_with(&root.sessions) {
        return Vec::new();
    }
    // ② agents/ 扫描：dir != main ∧ wire.jsonl 存在 ∧ mtime 活跃
    let agents_dir = dir.join("agents");
    let mut candidates: Vec<(String, PathBuf, SystemTime)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&agents_dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name == "main" || !e.path().is_dir() { continue; }
            let wire = e.path().join("wire.jsonl");
            let Ok(meta) = std::fs::metadata(&wire) else { continue };
            let Ok(mtime) = meta.modified() else { continue };
            if active_within(mtime, now) {
                candidates.push((name, wire, mtime));
            }
        }
    }
    candidates.sort_by(|a, b| a.0.cmp(&b.0)); // 注册序稳定
    // ③ 增量累计（缓存）
    update_entry("kimi", session_id, move |cur| {
        let mut st = cur.and_then(|b| b.downcast::<KimiCache>().ok())
            .map(|a| Arc::try_unwrap(a).unwrap_or_else(|a| (*a).clone()))
            .unwrap_or_default();
        let mut views = Vec::new();
        for (name, wire, _mtime) in &candidates {
            let track = st.agents.entry(name.clone()).or_default();
            match read_increment(wire, &mut track.incr) {
                IncrRead::Unchanged => {}
                IncrRead::Lines { lines, reset } => {
                    if reset { track.tokens = TokenUsage::default(); track.created = None; }
                    for line in &lines {
                        accumulate_wire_line(line, &mut track.tokens, &mut track.created);
                    }
                }
            }
            views.push(SubagentView {
                id: name.clone(),
                name: name.clone(), // v1：目录名（§4.3；C1 校准后换真名）
                description: None,
                spawn_ts: track.created.clone(),
                tokens: track.tokens,
            });
        }
        sort_views(&mut views);
        (Arc::new(st) as CacheBox, views)
    })
}
```

- [ ] **Step 4: 跑绿**：`cargo test subagents::kimi` → PASS。
- [ ] **Step 5: 门禁 + commit**：

```bash
cd src-tauri && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check
git add src-tauri/src/monitor/subagents/ src-tauri/src/monitor/kimi_parser.rs
git commit -m "feat(subagent-chips): kimi 子 agent source（wire 增量累计）"
```

### Task 6: codex source——倒排索引与终态布尔

**Files:**
- Create: `src-tauri/src/monitor/subagents/codex.rs`
- Modify: `src-tauri/src/monitor/codex_parser.rs:26`（`is_rollout_file` 私有 → `pub(crate)`，一行）
- Modify: `src-tauri/src/monitor/subagents/mod.rs`（`mod codex;`）

- [ ] **Step 1: 写失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn session_dir(td: &tempfile::TempDir) -> PathBuf {
        let d = td.path().join("sessions").join("2026").join("10").join("08");
        std::fs::create_dir_all(&d).unwrap();
        d
    }
    /// rollout 首行 session_meta（子形态带 parent_thread_id；普通会话无此字段）
    fn meta_line(id: &str, parent: Option<&str>, ts: &str) -> String {
        let p = match parent {
            Some(pt) => format!(r#","parent_thread_id":"{pt}""#),
            None => String::new(),
        };
        format!(r#"{{"timestamp":"{ts}","type":"session_meta","payload":{{"id":"{id}","cwd":"E:\\demo"{p}}}}}"#)
    }
    fn token_usage_line(inp: u64, cached: u64, cw: u64, out: u64) -> String {
        format!(r#"{{"timestamp":"2026-10-08T08:00:02Z","type":"token_usage_record","payload":{{"total_token_usage":{{"input_tokens":{inp},"cached_input_tokens":{cached},"cache_write_input_tokens":{cw},"output_tokens":{out},"reasoning_output_tokens":777,"total_tokens":99999}}}}}}"#)
    }
    fn complete_line(ts: &str) -> String {
        format!(r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"task_complete","turn_id":"t1"}}}}"#)
    }
    fn write_rollout(dir: &Path, name: &str, lines: &[String]) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, lines.iter().map(|l| format!("{l}\n")).collect::<String>()).unwrap();
        p
    }

    /// 桶位映射（§4.4）：input_tokens 内含 cached，直接求和会重复计数——P0 数字锁
    #[test]
    fn map_codex_usage_excludes_cached_from_input() {
        let t = map_codex_usage(100, 60, 10, 30);
        assert_eq!(t, TokenUsage { input: 40, cache_read: 60, cache_creation: 10, output: 30 });
        assert_eq!(t.total(), 140, "合计不含 reasoning 777（§8.5 申报）");
        assert_eq!(map_codex_usage(50, 60, 0, 0).input, 0, "cached > input 时饱和到 0，不下溢");
    }

    /// task_complete 出现即置位、永不复位；token_usage_record 直读覆盖（累计口径）
    #[test]
    fn rollout_line_applies_complete_and_usage() {
        let mut acc = RolloutAccum::default();
        apply_rollout_line(&token_usage_line(100, 60, 10, 30), &mut acc);
        assert!(!acc.task_complete_seen);
        assert_eq!(acc.tokens, map_codex_usage(100, 60, 10, 30));
        apply_rollout_line(&complete_line("2026-10-08T08:01:00Z"), &mut acc);
        assert!(acc.task_complete_seen);
        apply_rollout_line(&token_usage_line(1, 0, 0, 1), &mut acc);
        assert!(acc.task_complete_seen, "永不复位（§4.4）");
    }

    /// 倒排索引 + 判据：parent_thread_id 识别、无 task_complete 出板、换会话空
    #[test]
    fn codex_source_finds_child_by_parent_thread() {
        let td = tempfile::tempdir().unwrap();
        let d = session_dir(&td);
        write_rollout(&d, "rollout-2026-10-08T08-00-00-parent.jsonl",
            &[meta_line("parent-1", None, "2026-10-08T08:00:00Z")]);
        write_rollout(&d, "rollout-2026-10-08T08-00-01-child-9abc.jsonl", &[
            meta_line("child-9abc", Some("parent-1"), "2026-10-08T08:00:01Z"),
            token_usage_line(100, 60, 10, 30),
        ]);
        let now = SystemTime::now();
        let v = collect_with(&td.path().join("sessions"), "parent-1", now);
        assert_eq!(v.len(), 1, "普通会话（无 parent_thread_id）不入索引");
        assert_eq!(v[0].id, "child-9abc");
        assert_eq!(v[0].name, "child-9a".to_string(), "无名源：id 前 8 位截断（C3 校准点）——样本 ≥8 字符让截断真被测到");
        assert_eq!(v[0].spawn_ts.as_deref(), Some("2026-10-08T08:00:01Z"));
        assert_eq!(v[0].tokens, map_codex_usage(100, 60, 10, 30));
        assert!(collect_with(&td.path().join("sessions"), "parent-2", now).is_empty());
        // 追加 task_complete → 消失（增量：task_complete_seen 置位）
        let child = d.join("rollout-2026-10-08T08-00-01-child-9abc.jsonl");
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&child).unwrap();
        f.write_all((complete_line("2026-10-08T08:05:00Z") + "\n").as_bytes()).unwrap();
        drop(f);
        assert!(collect_with(&td.path().join("sessions"), "parent-1", now).is_empty());
        super::super::reset_cache_for_tests();
    }

    /// 24h 窗口（L3 对齐）：新鲜 rollout 进索引；超窗文件仅缓存命中参与
    #[test]
    fn codex_index_window_allows_cached_stale_entries() {
        let td = tempfile::tempdir().unwrap();
        let d = session_dir(&td);
        let p = write_rollout(&d, "rollout-a.jsonl", &[
            meta_line("c-1", Some("p-1"), "2026-10-08T08:00:01Z"), token_usage_line(1, 0, 0, 1),
        ]);
        let mtime = std::fs::metadata(&p).unwrap().modified().unwrap();
        // now1：新鲜 → 进索引
        assert_eq!(collect_with(&td.path().join("sessions"), "p-1", mtime + Duration::from_secs(10)).len(), 1);
        // now2 = mtime + 25h：文件超窗，但已缓存命中 → 仍参与（窗口外仅缓存命中参与）
        assert_eq!(collect_with(&td.path().join("sessions"), "p-1", mtime + Duration::from_secs(25 * 3600)).len(), 1,
            "判据本身不含时间叠加（task_complete_seen），窗口只管首行摘要的发现（stale 走 peek）");
        super::super::reset_cache_for_tests();
    }
}
```

- [ ] **Step 2: 跑红**：`cargo test subagents::codex` → 编译失败。

- [ ] **Step 3: 实现**

```rust
//! Codex 子 agent source（spec §4.4：线程对判据）。
//! 子 agent = rollout session_meta 带 parent_thread_id（普通会话无此字段——零歧义）；
//! 运行中 = 该 rollout 无 task_complete 事件（task_complete_seen 布尔出现即置位
//! 永不复位，判据查它而非回扫文件）。
//! 发现层 = 倒排索引（parent_thread_id → rollout 路径）：sessions 目录 249+ 文件，
//! 不可能每轮全扫首行。首行摘要（session_meta）走 `SessionFileScan::parse`（T =
//! Option<ChildMeta>）——(mtime,size) 门控与**负缓存**（无 parent_thread_id 的普通
//! rollout 也只读一次首行）免费继承，**不手搓第二套 stat 比对**（评审 P2-4）；
//! 扫描窗口对齐 codex_parser 的 24h 新鲜窗口（L3：fresh 现解析 / stale 仅 peek
//! 缓存命中参与，与 codex_parser 同款分流）。
//! 桶位映射（§4.4）：input = input_tokens − cached_input_tokens（内含防重复计数）、
//! cacheRead = cached_input_tokens、cacheCreation = cache_write_input_tokens、
//! output = output_tokens；reasoning_output_tokens 不计入展示合计。

use super::{read_increment, IncrRead, IncrState, sort_views, update_entry, CacheBox, SubagentView, TokenUsage};
use crate::monitor::codex_parser::is_rollout_file;
use crate::monitor::session_scan::{fresh_within, SessionFileScan};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

/// 索引文件扫描（复用 SessionFileScan 的递归收集 + (mtime,size) 摘要缓存 + 负缓存；
/// 独立 namespace 不与 codex-digest 串扰）
const CODEX_IDX_SCAN: SessionFileScan = SessionFileScan::new("codex-subagent-idx");

/// 桶位映射（纯函数；§4.4 防重复计数锁）
pub(crate) fn map_codex_usage(input: u64, cached: u64, cache_write: u64, output: u64) -> TokenUsage {
    TokenUsage {
        input: input.saturating_sub(cached),
        cache_read: cached,
        cache_creation: cache_write,
        output,
    }
}

/// 子 rollout 行累计（纯函数）：task_complete 置位（永不复位）+ token_usage_record 直读覆盖
pub(crate) struct RolloutAccum {
    pub(crate) task_complete_seen: bool,
    pub(crate) tokens: TokenUsage,
}
impl Default for RolloutAccum { fn default() -> Self { Self { task_complete_seen: false, tokens: TokenUsage::default() } } }

pub(crate) fn apply_rollout_line(line: &str, acc: &mut RolloutAccum) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { return };
    if v["type"].as_str() == Some("event_msg")
        && v["payload"]["type"].as_str() == Some("task_complete")
    {
        acc.task_complete_seen = true;
    }
    if v["type"].as_str() == Some("token_usage_record") {
        let tu = &v["payload"]["total_token_usage"];
        acc.tokens = map_codex_usage(
            tu["input_tokens"].as_u64().unwrap_or(0),
            tu["cached_input_tokens"].as_u64().unwrap_or(0),
            tu["cache_write_input_tokens"].as_u64().unwrap_or(0),
            tu["output_tokens"].as_u64().unwrap_or(0),
        );
    }
}

/// 子 rollout 首行摘要（SessionFileScan 缓存产物；None = 非子 rollout——负缓存：
/// 普通 rollout 首行只读一次，后续轮次 (mtime,size) 未变零重读，评审 P2-4）
#[derive(Clone)]
struct ChildMeta {
    child_id: String,
    parent_thread_id: String,
    spawn_ts: Option<String>,
}

/// 首行 session_meta 解析（SessionFileScan::parse 的解析函数）：子形态（带
/// parent_thread_id）才有意义，其余一律 None（负缓存条目）
fn read_child_meta(path: &Path) -> Option<ChildMeta> {
    let first = crate::monitor::jsonl::read_first_lines(path, 1).first()?.clone();
    let v: serde_json::Value = serde_json::from_str(&first).ok()?;
    if v["type"].as_str() != Some("session_meta") {
        return None;
    }
    let id = v["payload"]["id"].as_str()?.to_string();
    let parent = v["payload"]["parent_thread_id"].as_str()?.to_string();
    let ts = v["timestamp"].as_str().map(str::to_string);
    Some(ChildMeta { child_id: id, parent_thread_id: parent, spawn_ts: ts })
}

/// 索引扫描（L3 分流与 codex_parser.scan_codex_sessions 同款，测试
/// codex_index_window_allows_cached_stale_entries 锁定）：fresh → parse（含负缓存
/// 命中跳过）；stale → peek（仅缓存命中参与，未缓存 = 历史首行不碰）。
/// **不手搓 stat 比对 / retain 收敛**——门控与孤儿清理全在 SessionFileScan 内。
fn scan_child_metas(sessions_dir: &Path, now: SystemTime) -> Vec<(PathBuf, Arc<Option<ChildMeta>>)> {
    CODEX_IDX_SCAN
        .collect(sessions_dir, is_rollout_file)
        .into_iter()
        .map(|(path, mtime)| {
            let meta = if fresh_within(mtime, now) {
                CODEX_IDX_SCAN.parse(&path, read_child_meta)
            } else {
                CODEX_IDX_SCAN
                    .peek::<Option<ChildMeta>>(&path)
                    .unwrap_or_else(|| Arc::new(None))
            };
            (path, meta)
        })
        .collect()
}

/// 会话级缓存条目：每子 rollout 一份增量游标 + 终态布尔
#[derive(Default, Clone)]
struct CodexCache { children: HashMap<String, ChildTrack> }
#[derive(Default, Clone)]
struct ChildTrack { incr: IncrState, acc: RolloutAccum }

pub fn collect(session_id: &str) -> Vec<SubagentView> {
    let Some(dir) = dirs::home_dir().map(|h| h.join(".codex").join("sessions")) else {
        return Vec::new();
    };
    collect_with(&dir, session_id, SystemTime::now())
}

pub(crate) fn collect_with(sessions_dir: &Path, session_id: &str, now: SystemTime) -> Vec<SubagentView> {
    // 倒排查询：全量首行摘要中过滤 parent_thread_id == 本会话（摘要已被
    // SessionFileScan 缓存，这步是内存过滤，零额外首行读）
    let children: Vec<(PathBuf, ChildMeta)> = scan_child_metas(sessions_dir, now)
        .into_iter()
        .filter_map(|(p, m)| m.as_ref().as_ref().cloned().map(|m| (p, m)))
        .filter(|(_, m)| m.parent_thread_id == session_id)
        .collect();
    update_entry("codex", session_id, move |cur| {
        let mut st = cur.and_then(|b| b.downcast::<CodexCache>().ok())
            .map(|a| Arc::try_unwrap(a).unwrap_or_else(|a| (*a).clone()))
            .unwrap_or_default();
        let mut views = Vec::new();
        for (path, meta) in &children {
            let track = st.children.entry(meta.child_id.clone()).or_default();
            match read_increment(path, &mut track.incr) {
                IncrRead::Unchanged => {}
                IncrRead::Lines { lines, reset } => {
                    if reset { track.acc = RolloutAccum::default(); }
                    for line in &lines { apply_rollout_line(line, &mut track.acc); }
                }
            }
            if !track.acc.task_complete_seen {
                views.push(SubagentView {
                    id: meta.child_id.clone(),
                    name: meta.child_id.chars().take(8).collect(), // 无名源（C3 校准点）
                    description: None,
                    spawn_ts: meta.spawn_ts.clone(),
                    tokens: track.acc.tokens,
                });
            }
        }
        sort_views(&mut views);
        (Arc::new(st) as CacheBox, views)
    })
}
```

- [ ] **Step 4: 跑绿**：`cargo test subagents::codex` → PASS。
- [ ] **Step 5: 门禁 + commit**：

```bash
cd src-tauri && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check
git add src-tauri/src/monitor/subagents/ src-tauri/src/monitor/codex_parser.rs
git commit -m "feat(subagent-chips): codex 子 agent source——倒排索引与终态布尔"
```

### Task 7: 端点 + source 注入缝 + 路由 + 生产装配

**Files:**
- Modify: `src-tauri/src/remote/server.rs`（RemoteState 加字段 ~462、api_router 加路由 ~583、类型别名、20 处测试装配补桩、端点测试）
- Modify: `src-tauri/src/remote/api.rs`（`session_subagents` handler，置于 `session_files` 之后 ~590）
- Modify: `src-tauri/src/remote/mod.rs`（生产装配 ~288 补 `subagent_source`）
- Modify: `src-tauri/src/monitor/subagents/mod.rs`（`SubagentView` 需被 remote 引用——已是 `pub(crate)` 可直接用；确认 `mod claude/opencode/kimi/codex` 声明为 `pub(crate) mod`）

- [ ] **Step 1: 写失败测试**（server.rs tests 模块追加；复用既有 `req()` / `body_string`——先例 `ui-config` 用例组）

**装配定案**：`test_state()` 字面量里直接加 `subagent_source: std::collections::HashMap::new()`（缺省空表——未知工具空态用例直接复用 test_state()）；本用例组另建专属构造 `state_with_subagent_fakes()`：**复制 test_state() 字面量、仅替换 subagent_source 为假源**（仓内先例：inject/queue.rs 的 `state()`、server.rs 多个用例组各自持有字面量）。假源（claude 键，返回两条故意倒序的视图——端点必须按 spawnTs 升序输出、None 排尾）：

```rust
fn state_with_subagent_fakes() -> Arc<RemoteState> {
    let mut fakes: std::collections::HashMap<&'static str, Box<SubagentSourceFn>> =
        std::collections::HashMap::new();
    fakes.insert(
        "claude",
        Box::new(|_: &str, sid: &str| {
            vec![
                SubagentView {
                    id: format!("late-{sid}"),
                    name: "Plan".into(),
                    description: Some("设计新建会话两改动实现方案".into()),
                    spawn_ts: Some("2026-10-08T09:00:00.000+00:00".into()),
                    tokens: TokenUsage { input: 5124, cache_read: 56448, cache_creation: 0, output: 9312 },
                },
                SubagentView {
                    id: format!("early-{sid}"),
                    name: "Explore".into(),
                    description: None,
                    spawn_ts: None, // spawn 竞态：无时长
                    tokens: TokenUsage::default(),
                },
            ]
        }),
    );
    // ↓ 复制 test_state() 的完整字面量（逐字段同值），仅此行不同：
    Arc::new(RemoteState {
        // …test_state() 各字段照抄…
        subagent_source: fakes,
        // …其余字段照抄…
    })
}
```

（`SubagentView` / `TokenUsage` 从 `crate::monitor::subagents` 引入。）

测试主体：

```rust
/// 端点契约矩阵（spec §5.2/§7）：缺参 400 / 空白 400 / 未知工具空态 / 假源 camelCase
/// 载荷 / no-store 头 / spawnTs 升序（None 排尾）/ 无 cookie 403（gate 结构性覆盖）
#[tokio::test]
async fn session_subagents_contract_matrix() {
    let st = state_with_subagent_fakes();
    let app = crate::remote::server::router(st.clone());
    // 既有同步 helper（server.rs tests ~975）：fn paired_cookie(state: &Arc<RemoteState>,
    // id: &str) -> String——直插设备表返回 cookie，不走 /pair/pin（零 PIN 依赖）
    let cookie = paired_cookie(&st, "sub-chips-1");
    // ① 缺 agent_type → 400
    let r = app.clone().oneshot(req("GET", "/m/api/v1/session-subagents?session_id=s1", Some(&cookie), None)).await.unwrap();
    assert_eq!(r.status(), 400);
    // ② 缺 session_id → 400
    let r = app.clone().oneshot(req("GET", "/m/api/v1/session-subagents?agent_type=claude", Some(&cookie), None)).await.unwrap();
    assert_eq!(r.status(), 400);
    // ③ 空白参（%20）→ 400
    let r = app.clone().oneshot(req("GET", "/m/api/v1/session-subagents?agent_type=%20&session_id=s1", Some(&cookie), None)).await.unwrap();
    assert_eq!(r.status(), 400);
    // ④ 未知工具 → 200 空态（端点是空态唯一权威）
    let r = app.clone().oneshot(req("GET", "/m/api/v1/session-subagents?agent_type=zzz&session_id=s1", Some(&cookie), None)).await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(body_string(r).await, r#"{"subagents":[]}"#);
    // ⑤ 假源：camelCase 载荷 + spawnTs 升序（None 排尾）+ 四桶 + no-store
    let r = app.clone().oneshot(req("GET", "/m/api/v1/session-subagents?agent_type=claude&session_id=s1", Some(&cookie), None)).await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers().get("cache-control").unwrap(), "no-store", "门禁下私有数据禁中间层缓存");
    let body = body_string(r).await;
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let arr = v["subagents"].as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["id"], "early-s1", "无 spawnTs 排尾");
    assert_eq!(arr[0]["spawnTs"], serde_json::Value::Null);
    assert_eq!(arr[1]["name"], "Plan");
    assert_eq!(arr[1]["tokens"]["cacheRead"], 56448);
    assert_eq!(arr[1]["description"], "设计新建会话两改动实现方案");
    // ⑥ 无 cookie → 403（gate 结构性覆盖）
    let r = app.clone().oneshot(req("GET", "/m/api/v1/session-subagents?agent_type=claude&session_id=s1", None, None)).await.unwrap();
    assert_eq!(r.status(), 403);
}
```

- [ ] **Step 2: 跑红**：`cargo test session_subagents` → 编译失败（字段/路由不存在）。

- [ ] **Step 3: 实现**

`server.rs`（类型别名 + 字段，放在既有缝字段区末尾）：

```rust
/// 子 agent 运行源注入缝（spec §5.2，评审定的「每工具一个 source」模式）：
/// 键 = agent_type；闭包参数 (agent_type, session_id)——与 message_source 的
/// (tool, sid, …) 形态同构（键已是工具名，首参供通用假源复用，实现可忽略）。
/// 生产 = 四工具 monitor::subagents::{claude,opencode,kimi,codex}::collect；
/// 测试注入假源（零真实磁盘/DB）。**端点契约冻结**：kimi/opencode 校准只改
/// 各自 source 实现，不动端点与载荷。
pub type SubagentSourceFn =
    dyn Fn(&str, &str) -> Vec<crate::monitor::subagents::SubagentView> + Send + Sync;
```

```rust
// RemoteState 字段（ui_config_source 之后）：
/// 子 agent 运行源注入缝（spec §5.2）：见 SubagentSourceFn 文档。
/// 空表 = 一切工具空态（测试缺省；未知 agent_type 同形）
pub subagent_source: std::collections::HashMap<&'static str, Box<SubagentSourceFn>>,
```

`api_router` 路由（`/session-files` 之后）：

```rust
// /session-subagents（2026-10-08 子 agent chip，spec §5.2）：会话运行中子 agent
// 视图（四工具 source 注入缝；PIN 门禁内层 gate 结构性覆盖——新端点不需要
// 各自鉴权代码；空态唯一权威）
.route("/session-subagents", get(api::session_subagents))
```

`api.rs` handler（置于 `session_files` 之后；模板 = session_files/session_messages）：

```rust
/// GET /m/api/v1/session-subagents?agent_type=&session_id=（spec §5.2）
/// `{subagents: [{id, name, description, spawnTs, tokens{input,cacheRead,cacheCreation,output}}]}`
/// - 只含运行中条目（判据在各 source，spec §4）；按 spawnTs 升序（None 排尾）；
/// - **端点是空态唯一权威**：前端不另特判；会话不存在/未知 agent_type/source
///   未装配 → `subagents: []`（不给探测面）。「非活跃 → []」**不做端点侧活跃度
///   判定**（不查会话快照——省一次 sysinfo 全量扫描，取舍如实申报）：已结束会话
///   的空态由前端 finished 卸载实现（spec §6 挂载门）；其余状态的裁决交给各
///   source 判据（判据空 = 空列表）；
/// - 缺参（agent_type / session_id）或空白 → 400 BAD_REQUEST；
/// - 响应带 Cache-Control: no-store（门禁下私有数据，sessions 同规）；
/// - source 是文件/DB IO（同步阻塞），`spawn_blocking` 包裹（sessions handler 同先例）。
pub async fn session_subagents(
    State(st): State<Arc<RemoteState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Response, StatusCode> {
    let Some(agent) = params
        .get("agent_type")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        return Err(StatusCode::BAD_REQUEST);
    };
    let Some(sid) = params
        .get("session_id")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        return Err(StatusCode::BAD_REQUEST);
    };
    let st = st.clone();
    let mut subagents = tokio::task::spawn_blocking(move || {
        match st.subagent_source.get(agent.as_str()) {
            Some(f) => f(agent.as_str(), sid.as_str()),
            None => Vec::new(), // 未知工具/未装配：空态唯一权威
        }
    })
    .await
    .map_err(|e| {
        log::error!("子 agent 查询任务异常: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    // 端点统一排序（source 只管判据，序是端点契约的一部分）
    crate::monitor::subagents::sort_views(&mut subagents);
    Ok((
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({ "subagents": subagents })),
    )
        .into_response())
}
```

（`sort_views` 从 `pub(crate)` 提为 `pub(crate)` 已够——同 crate；若 api.rs 引用路径报私有权，确认 `monitor/subagents/mod.rs` 的 `pub(crate) fn sort_views` 即可。）

`mod.rs` 生产装配（`path_source` 之后）：

```rust
// 2026-10-08 子 agent chip（spec §5.2）：四工具 source 直调 monitor::subagents
// 各 collect（判据/缓存在那层；本缝只做接线）。空表键 = 未知工具空态。
subagent_source: [
    ("claude", Box::new(|_a: &str, sid: &str| crate::monitor::subagents::claude::collect(sid)) as Box<server::SubagentSourceFn>),
    ("opencode", Box::new(|_a: &str, sid: &str| crate::monitor::subagents::opencode::collect(sid))),
    ("kimi", Box::new(|_a: &str, sid: &str| crate::monitor::subagents::kimi::collect(sid))),
    ("codex", Box::new(|_a: &str, sid: &str| crate::monitor::subagents::codex::collect(sid))),
].into_iter().collect(),
```

**测试装配补桩（编译器驱动）**：`cargo check --tests` 会列出全部缺失字段的字面量位置（server.rs 19 处 + inject/queue.rs:1104，行号会漂移，以报错为准）——逐一补：

```rust
subagent_source: std::collections::HashMap::new(),
```

（`mod.rs` 顶部补 `use` 或写全路径 `server::SubagentSourceFn`。）

- [ ] **Step 4: 跑绿**：`cd src-tauri && cargo test` 全量（重点 `session_subagents` + 既有端点回归零破坏）。
- [ ] **Step 5: 门禁 + commit**：

```bash
cd src-tauri && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check
git add src-tauri/src/remote/ src-tauri/src/monitor/subagents/
git commit -m "feat(subagent-chips): /session-subagents 端点与 source 注入缝"
```

### Task 8: 前端——api 封装、SubagentChips 组件、SessionDetail 挂载

**Files:**
- Create: `src/mobile/SubagentChips.tsx`
- Create: `tests/mobile/SubagentChips.test.tsx`
- Modify: `src/mobile/api.ts`（文件尾部追加类型与 fetch，~1473 行后）
- Modify: `src/mobile/SessionDetail.tsx:1069`（ModeBar 换 status-row 包裹 + SubagentChips 兄弟；import 补两行）
- Modify: `tests/mobile/SessionDetail.test.tsx`（3 处 fetch 桩补 `/session-subagents` 路由，~121/1167/1878）

- [ ] **Step 1: 写失败测试**（`tests/mobile/SubagentChips.test.tsx`，vitest 惯例照 ModeBar.test.tsx）

```tsx
import { cleanup, render, screen, vi } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import SubagentChips, { chipTokenText, formatElapsed } from "@/mobile/SubagentChips";
import type { Session } from "@/types/session";

// spec §6/§7（前端）：渲染/多枚并排/空列表不渲染/拉取失败不渲染/时长自走（fake timers，
// tick 不出组件）/refreshTick 重拉/finished 不拉。

let fetchMock: ReturnType<typeof vi.fn>;
const baseSession: Session = { /* 按 types/session.ts 必填字段构造最小 Session */
  id: "s1", agentType: "claude", projectName: "p", projectPath: "p", title: null,
  gitBranch: null, githubUrl: null, status: "processing", lastMessage: null,
  lastMessageRole: null, lastActivityAt: "2026-10-08T07:00:00Z", pid: 1, cpuUsage: 0,
  activeSubagentCount: 0, form: "cli", jumpSupported: false, unread: false,
};
function subagent(id: string, name: string, spawnTs: string | null,
                  t: { input: number; cacheRead: number; cacheCreation: number; output: number }) {
  return { id, name, description: "设计新建会话两改动实现方案", spawnTs, tokens: t };
}

/** 桩路由表（用例就地改写；fail=true 模拟网络异常） */
let routes: { body?: ReturnType<typeof subagent>[]; status?: number; fail?: boolean };

beforeEach(() => {
  routes = {};
  vi.stubGlobal("fetch", (fetchMock = vi.fn(async (input: RequestInfo | URL) => {
    const url = String(input);
    if (!url.includes("/session-subagents")) throw new Error(`unexpected fetch: ${url}`);
    if (routes.fail) throw new TypeError("network down");
    return new Response(JSON.stringify({ subagents: routes.body ?? [] }), { status: routes.status ?? 200 });
  })));
});
afterEach(() => { cleanup(); vi.unstubAllGlobals(); vi.useRealTimers(); });

/** 纯函数：时长格式（M:SS；≥1h H:MM:SS）与 token 合计（fmtTokens 口径） */
describe("formatElapsed / chipTokenText", () => {
  it("时长与 token 缩写", () => {
    expect(formatElapsed(0)).toBe("0:00");
    expect(formatElapsed(59_000)).toBe("0:59");
    expect(formatElapsed(338_000)).toBe("5:38");
    expect(formatElapsed(3_753_000)).toBe("1:02:33");
    expect(chipTokenText({ input: 5124, cacheRead: 56448, cacheCreation: 0, output: 9312 })).toBe("7.09万");
    expect(chipTokenText({ input: 0, cacheRead: 0, cacheCreation: 0, output: 0 })).toBe("0");
  });
});

/** 渲染矩阵（fake timers：spawnTs 固定为 10s 前） */
describe("SubagentChips", () => {
  it("渲染名称+时长+token；description 进 title", async () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-10-08T07:36:45Z")); // 与 spawnTs 差 338s = 5:38
    routes.body = [subagent("a86f", "Plan", "2026-10-08T07:31:07Z",
      { input: 5124, cacheRead: 56448, cacheCreation: 0, output: 9312 })];
    render(<SubagentChips session={baseSession} refreshTick={0} />);
    const chip = await screen.findByTestId("subagent-chip-a86f");
    expect(chip.textContent).toContain("Plan");
    expect(chip.textContent).toContain("5:38");
    expect(chip.textContent).toContain("7.09万");
    expect(chip.getAttribute("title")).toBe("设计新建会话两改动实现方案");
  });

  it("spawnTs=null → 只显名称与 token，无时长", async () => {
    routes.body = [subagent("x1", "Explore", null, { input: 100, cacheRead: 0, cacheCreation: 0, output: 0 })];
    render(<SubagentChips session={baseSession} refreshTick={0} />);
    const chip = await screen.findByTestId("subagent-chip-x1");
    expect(chip.textContent).not.toMatch(/\d:\d\d/);
    expect(chip.textContent).toContain("100");
  });

  it("多枚并排", async () => {
    routes.body = [
      subagent("a", "Plan", "2026-10-08T07:30:00Z", { input: 1, cacheRead: 0, cacheCreation: 0, output: 0 }),
      subagent("b", "Explore", "2026-10-08T07:31:00Z", { input: 2, cacheRead: 0, cacheCreation: 0, output: 0 }),
    ];
    render(<SubagentChips session={baseSession} refreshTick={0} />);
    expect(await screen.findByTestId("subagent-chip-a")).toBeTruthy();
    expect(screen.getByTestId("subagent-chip-b")).toBeTruthy();
  });

  it("空列表不渲染；首拉失败不渲染（静默）", async () => {
    routes.body = [];
    const { container, rerender } = render(<SubagentChips session={baseSession} refreshTick={0} />);
    await vi.waitFor(() => expect(fetchMock).toHaveBeenCalled());
    expect(container.querySelector('[data-testid="subagent-chips"]')).toBeNull();
    // 首拉即网络异常 → 维持 null，不渲染、不报错
    fetchMock.mockClear();
    routes.fail = true;
    rerender(<SubagentChips session={baseSession} refreshTick={1} />);
    await vi.waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(1));
    expect(container.querySelector('[data-testid="subagent-chips"]')).toBeNull();
  });

  it("时长自走：tick 不出组件（父零重渲染）", async () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-10-08T07:36:35Z"));
    routes.body = [subagent("a86f", "Plan", "2026-10-08T07:36:35Z",
      { input: 1, cacheRead: 0, cacheCreation: 0, output: 0 })];
    let parentRenders = 0;
    const Probe = () => { parentRenders += 1; return <SubagentChips session={baseSession} refreshTick={0} />; };
    render(<Probe />);
    const chip = await screen.findByTestId("subagent-chip-a86f");
    expect(chip.textContent).toContain("0:00");
    await vi.advanceTimersByTimeAsync(61_000);
    expect(screen.getByTestId("subagent-chip-a86f").textContent).toContain("1:01");
    expect(parentRenders).toBe(1, "1s tick 只重渲染 SubagentChips，不拖整页（spec §6）");
  });

  it("refreshTick bump → 重拉；成功空列表 → 清空", async () => {
    routes.body = [subagent("a", "Plan", "2026-10-08T07:36:35Z", { input: 1, cacheRead: 0, cacheCreation: 0, output: 0 })];
    const { rerender } = render(<SubagentChips session={baseSession} refreshTick={0} />);
    await screen.findByTestId("subagent-chip-a");
    routes.body = [];
    rerender(<SubagentChips session={baseSession} refreshTick={1} />);
    await vi.waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(2));
    await vi.waitFor(() => expect(document.querySelector('[data-testid="subagent-chips"]')).toBeNull());
  });

  it("失败保留上一份（不闪断）；finished 不拉取", async () => {
    routes.body = [subagent("a", "Plan", "2026-10-08T07:36:35Z", { input: 1, cacheRead: 0, cacheCreation: 0, output: 0 })];
    const { rerender } = render(<SubagentChips session={baseSession} refreshTick={0} />);
    await screen.findByTestId("subagent-chip-a");
    routes.fail = true; // 网络失败 → 静默保留上一份
    rerender(<SubagentChips session={baseSession} refreshTick={1} />);
    await vi.waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(2));
    expect(screen.getByTestId("subagent-chip-a")).toBeTruthy();
    // finished：不挂载不拉取
    fetchMock.mockClear();
    rerender(<SubagentChips session={{ ...baseSession, status: "finished" } as Session} refreshTick={2} />);
    expect(fetchMock).toHaveBeenCalledTimes(0);
  });
});
```

- [ ] **Step 2: 跑红**：`pnpm vitest run tests/mobile/SubagentChips.test.tsx` → 模块不存在。

- [ ] **Step 3: 实现**

`src/mobile/api.ts` 追加：

```ts
// ==== 2026-10-08 子 agent 运行 chip（spec 2026-10-08-mobile-subagent-chips §5.2/§6）====

/** GET /session-subagents 载荷单条（与 Rust monitor::subagents::SubagentView 的
 *  camelCase 序列化逐字段对应，勿漂移）。spawnTs=null：首条时间戳未落盘（spawn
 *  竞态，下轮自愈）——前端不显示时长只显 token。 */
export interface SubagentView {
  id: string;
  name: string;
  description: string | null;
  spawnTs: string | null;
  tokens: { input: number; cacheRead: number; cacheCreation: number; output: number };
}

/** 拉取会话的运行中子 agent（空列表 = 无运行中，端点是空态唯一权威——前端不
 *  另特判）。非 2xx / 网络异常 → 抛 ApiError（调用方静默，ModeBar 同惯例） */
export async function fetchSessionSubagents(
  agentType: string,
  sessionId: string,
): Promise<SubagentView[]> {
  const q = new URLSearchParams({ agent_type: agentType, session_id: sessionId });
  let r: Response;
  try {
    r = await fetch(`/m/api/v1/session-subagents?${q}`);
  } catch (e) {
    throw new ApiError(null, `session-subagents 网络异常: ${String(e)}`);
  }
  if (!r.ok) throw new ApiError(r.status, `session-subagents ${r.status}`);
  const data = (await r.json()) as { subagents: SubagentView[] };
  return data.subagents;
}
```

`src/mobile/SubagentChips.tsx`：

```tsx
import { useEffect, useState } from "react";
import { fmtTokens } from "@/lib/usage/format";
import type { Session } from "@/types/session";
import { fetchSessionSubagents, type SubagentView } from "./api";

/** 运行中子 agent chip 区（spec §6）：ModeBar 兄弟节点，同一行 flex-wrap。
 *
 * - 拉取随 SessionDetail 的 refreshTick（DETAIL_REFRESH_MS 轮询——免费继承
 *   hidden 暂停/恢复补刷）独立拉取；空列表 = 端点权威，不渲染；
 * - 时长自走：tick 状态收在本组件（1s interval），不拖整页每秒重渲染；
 *   下轮拉取用服务端 spawnTs 校正；
 * - 失败静默**保留上一份成功数据**（不闪断、不报错——「静默不渲染」指无错误
 *   UI；首拉失败 → null 不渲染）；成功载荷（含空）即覆盖；
 * - 会话 finished 不拉取（挂载门在 SessionDetail；idle 不拦——claude 后台
 *   agent 可在主会话 idle 时仍在跑）。 */
export default function SubagentChips({
  session,
  refreshTick,
}: {
  session: Session;
  refreshTick: number;
}) {
  const [list, setList] = useState<SubagentView[] | null>(null);
  const [, setNowTick] = useState(0); // 时长自走 tick（仅本组件重渲染）

  useEffect(() => {
    if (session.status === "finished") return;
    let alive = true;
    fetchSessionSubagents(session.agentType, session.id)
      .then((v) => {
        if (alive) setList(v);
      })
      .catch(() => {
        /* 静默：保留上一份（不闪断）；首拉失败维持 null 不渲染 */
      });
    return () => {
      alive = false;
    };
  }, [session.agentType, session.id, session.status, refreshTick]);

  useEffect(() => {
    const t = setInterval(() => setNowTick((n) => n + 1), 1000);
    return () => clearInterval(t);
  }, []);

  if (list === null || list.length === 0) return null;
  const now = Date.now();
  return (
    <div data-testid="subagent-chips" className="flex flex-wrap items-center gap-x-2 gap-y-1">
      {list.map((s) => {
        const elapsed =
          s.spawnTs !== null && !Number.isNaN(Date.parse(s.spawnTs))
            ? formatElapsed(now - Date.parse(s.spawnTs))
            : null;
        return (
          <span
            key={s.id}
            data-testid={`subagent-chip-${s.id}`}
            title={s.description ?? undefined}
            className="rounded-full bg-[var(--cb)]/60 px-2 py-0.5 text-[11px] text-[var(--tx)]"
          >
            ◉ {s.name}
            {elapsed !== null ? ` ${elapsed}` : ""} · {chipTokenText(s.tokens)}
          </span>
        );
      })}
    </div>
  );
}

/** 运行时长格式（纯函数）：M:SS；≥1h → H:MM:SS */
export function formatElapsed(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = s % 60;
  const two = (n: number) => String(n).padStart(2, "0");
  return h > 0 ? `${h}:${two(m)}:${two(sec)}` : `${m}:${two(sec)}`;
}

/** chip 的 token 文案：四桶求和走既有 fmtTokens 口径（reasoning 类桶已在后端排除） */
export function chipTokenText(t: SubagentView["tokens"]): string {
  return fmtTokens(t.input + t.cacheRead + t.cacheCreation + t.output);
}
```

`src/mobile/SessionDetail.tsx:1069`（import 区补 `import SubagentChips from "./SubagentChips";`）——ModeBar 一行替换为：

```tsx
      {/* 2026-10-08 子 agent chip：与 ModeBar 同一行（flex-wrap 兄弟），生命周期
          解耦——mode 视图失败/unsupported 时 chip 仍活，反之亦然（spec §6）。
          finished 不挂载（idle 不拦：claude 后台 agent 可在主会话 idle 时仍在跑）；
          其余状态的空态裁决交给端点（空态唯一权威）。 */}
      <div data-testid="status-row" className="flex flex-wrap items-center gap-2">
        <ModeBar key={`mode-${session.id}`} session={session} />
        {session.status !== "finished" && (
          <SubagentChips
            key={`subagents-${session.id}`}
            session={session}
            refreshTick={refreshTick}
          />
        )}
      </div>
```

（钥匙防线延续：`subagents-` 前缀与 `approve-/plan-fb-/question-/mode-/composer-` 互异——cardDock 兄弟 key 无碰撞。）

`tests/mobile/SessionDetail.test.tsx`——3 处 fetch 桩（~121/1167/1878）各补一条路由（**不补会让既有用例 unexpected fetch 报错**）：

```ts
    if (url.includes("/session-subagents")) {
      return new Response(JSON.stringify({ subagents: [] }), { status: 200 });
    }
```

- [ ] **Step 4: 跑绿**：`pnpm vitest run tests/mobile/SubagentChips.test.tsx tests/mobile/SessionDetail.test.tsx` → PASS。
- [ ] **Step 5: 全套门禁 + commit**：

```bash
pnpm vitest run tests/mobile && pnpm build:mobile && pnpm format:check && pnpm lint
git add src/mobile/SubagentChips.tsx src/mobile/api.ts src/mobile/SessionDetail.tsx tests/mobile/SubagentChips.test.tsx tests/mobile/SessionDetail.test.tsx
git commit -m "feat(subagent-chips): 移动端子 agent chip 组件与轮询接线"
```

### Task 9: 文档收口与追踪 issue

**Files:**
- 无代码改动（issue + 本计划勾选状态维护）

- [ ] **Step 1: 全量回归**：

```bash
cd src-tauri && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check
pnpm test && pnpm build:mobile && pnpm build && pnpm format:check && pnpm lint
```

- [ ] **Step 2: 挂追踪 issue**（spec §8.6 看板徽标落差——本批不动 `count_active_subagents`）：

```bash
gh issue create --title "看板徽标 count_active_subagents 旧布局恒 0——后续批次修复或删除" \
  --body "spec 2026-10-08-mobile-subagent-chips §8.6：monitor/jsonl.rs count_active_subagents 只扫 claude 旧布局平铺层，新布局（<会话uuid>/subagents/）下恒 0；与本端点同源不同层。后续批次用 monitor::subagents 同源数据修复看板徽标，或删除该字段与 UI。"
```

- [ ] **Step 3: commit**：

```bash
git add docs/engineering/plans/2026-10-08-mobile-subagent-chips.md
git commit -m "docs(subagent-chips): 收口文档与追踪 issue"
```

## 六、门禁（每 commit 全绿；命令逐段执行防管道吞退出码）

```bash
# Rust（src-tauri/ 下）
cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check
# 前端（仓库根）
pnpm vitest run tests/mobile   # 任务级；收尾全量 pnpm test
pnpm build:mobile              # 含 tsc（移动端入口编译）
pnpm format:check && pnpm lint
```

红线沿用：测试零网络、零真实用户数据目录（tempdir / 合成字节流 / 内存库 / 假源）；`cargo test` 前先 `pnpm build:mobile`（rust-embed debug 直读——仅当改了 `src/mobile/**` 的 commit 需要）；`pnpm check:i18n` 不涉及（移动端文案硬编码中文，无 i18n 键新增）。

## 七、不做（v1 范围红线）

- **不做 mtime 兜底（claude）**：长静默工具调用会误伤；残留 chip 随会话中断整区消失（spec §4.1/§8.2 申报）。
- **不做跨代匹配（claude resume）**：resume 落新 uuid 目录时旧 agent 的累计/状态不迁移（spec §8.2）。
- **不做看板徽标修复**：`count_active_subagents` 留后续批次（T9 挂 issue；spec §8.6）。
- **端点契约冻结**：路径/参数/载荷形状/no-store/排序均不再动；kimi/opencode 校准只改各 source 实现。
- **不动 opencode v1 旧表**：判据单点 `schema_is_v2`，1.x 库空态。
- **不做 codex 终止细分类**：只有 `task_complete` 一个终止信号（interrupt 等不产生它 → 维持运行中，spec §4.4 口径）。
- **chip 不做交互**：不可点、无跳转；仅 `title` 悬停显示 description。
- **不做 kimi 真名/完成标记预实现**：v1 显示目录名 + mtime 判据，C1 校准后收严（spec §4.3）。
- **不做 SSE/事件推送**：chip 数据走 10s 轮询（与详情页既有节奏一致）。

## 八、已知边界（实施不修，验收对照 spec §8）

1. token 数字与各家终端 footer 可能不等（口径不同）——chip 不声称「与终端一致」。
2. claude：TaskStop 手动停止是否落通知未验证（残留 chip 可能，随会话中断消失）；spawn 瞬间竞态（无时长只显 token，下轮自愈）；resume 跨代不匹配。
3. opencode：token 列若非随跑随更则运行期显示 0（如实不编数，C2 观测）；长静默子 agent 暂时隐藏（time_updated 超 90s）。
4. kimi：编号名非真名；mtime 判据在长静默工具调用期间暂时隐藏 chip；完成标记待 C1。
5. codex：需 `--enable multi_agent_v2`（0.160.0 默认关，未开启会话天然空态）；上游任务正文传递 bug 不影响检测；`reasoning_output_tokens` 不入合计。
6. MAM 重启缓存清零，首次请求重建（倒序扫描/全量读各付一次），判据前后一致。

## 九、手工验收清单（用户执行；自动化已覆盖逻辑层，此处验交互观感）

1. **claude（判据权威）**：终端起后台 agent（如 Plan）→ 手机详情页底部模式栏右侧出现 `◉ Plan <时长> · <token>`；agent 停止 → 10s 内 chip 消失；对它 SendMessage 续跑 → chip 复现（P0 场景实机复验，同 C3）。
2. **opencode**：起 explore 子 agent → chip 出现且 token 随跑变化（C2 观测点）；子 agent 长静默 >90s → chip 暂时消失（预期行为非缺陷）。
3. **kimi**：起内建 Agent 后台任务 → chip 显示 `agent-N` 编号名；顺带取证完成标记与真名派生事件（C1）。
4. **codex**：`codex --enable multi_agent_v2` 派生子 agent → chip 出现；子 rollout 出现 task_complete → chip 消失。
5. **横切**：多 agent 并发多枚 chip 同一行换行排列；chip `title` 悬停显示 description；会话结束（finished）整区消失；弱网/断网刷新页 chip 不闪断（保留上一份）、恢复后自动校正。

## 十、实机校准任务（可并行 / 可延期，不阻塞合入）

> 三项均为 spec 标注的「待实机校准」：**校准只改各 source 实现（或 spec 追记判据），端点契约与前端不动**。合入后任何时间执行；结论回填 spec 对应小节（spec 是判据权威，回填走 spec 追记 + source 实现，不写死在代码注释里了事）。

### C1: kimi 内建 Agent 完成标记 + 真名派生事件取证

**操作步骤**：①真机起 kimi 会话，prompt 要求用内建 Agent 工具派生一个后台子任务；②等待任务完成前后各 dump 一次：父 `agents/main/wire.jsonl` 尾部 200 行 + `agents/agent-N/wire.jsonl` 全量（只读探针，勿改任何文件）；③记录：完成时刻父/子 wire 各出现什么事件形态（完成标记候选）、派生事件里是否带真实任务名、`metadata.created_at` 的具体形态（毫秒 int 还是 ISO 字符串）。
**回填判据**：若找到可靠完成标记 → spec §4.3 追记事件判据（替换 mtime）+ 改 `kimi.rs`（`accumulate_wire_line` 增事件分支或判据换事件序）；若派生事件带真名 → `name` 换真名；created_at 形态若与 `json_time_to_iso` 双形态容忍不符 → 收严。**产出归档**：`docs/research/refs/` 下留取证摘录。

### C2: opencode token 随跑性观测

**操作步骤**：①真机 opencode 会话派生 explore 子 agent；②任务运行中每 30s 查一次 `~/.local/share/opencode/opencode.db`：`SELECT id, tokens_input, tokens_output, tokens_cache_read, time_updated FROM session_v2 WHERE parent_id IS NOT NULL;`（sqlite3 只读）；③记录 token 列是否随运行增长、还是结束才一次性写入。
**回填判据**：若随跑随更 → 无代码改动，spec §4.2 申报落定；若结束才写 → chip 运行期 token 显示 0 是如实口径（spec §8.3 已申报），无代码改动。两种结论都只需 spec 追记一句。

### C3: claude SendMessage 续跑手工验收（布局复核已并入 T3 阻塞验收）

**操作步骤**：①按 §九.1 走全流程（起后台 agent → 停止 → SendMessage 续跑），确认 chip
消失→复现（P0 判据的实机终验）。布局复核已并入 T3 Step 4b（阻塞项）——若实机出现与
spec §2.1 已修正版**都不符**的第三种形态（本计划旧版曾把父转写画进会话目录，评审 P0-1
已修正），停下回报用户裁决后再动发现层。

**回填判据**：§九.1 全流程通过 → 判据终验落档；布局异常 → 回报裁决（不在本任务自行改 locate）。

---

## 自审记录（writing-plans SKILL 自查，实施者可忽略）

- **Spec 覆盖**：§4.1→T2/T3；§4.2→T4；§4.3→T5（+C1）；§4.4→T6（+C3 布局复核）；§5.1 缓存→T1/T3/T5/T6、倒排索引→T6、注册表复用→T5、opencode 无缓存→T4；§5.2 端点→T7（含 400/no-store/spawn_blocking/缝/空态权威/排序）；§6 前端→T8（兄弟节点/同一行/tick 收组件/fmtTokens/key/失败空态静默）；§7 测试矩阵逐条落进各 Task 的用例（claude P0/半行/护栏/倒序/stat 门控→T1-T3；opencode 边界→T4；kimi main 排除/累计→T5；codex 识别/终止/直读→T6；端点假源矩阵→T7；前端五用例→T8）；§8 边界→§八；手工验收→§九；校准→§十。
- **占位符扫描**：T1 registry（update_entry / 硬上限 / 测试隔离）与 read_increment、T3 块边界、T6 索引均已写成定稿实现，配套测试锁定；T3「实现提示」为跨函数装配要点（Clone / resume_at / sort_views 放置），非 TBD。（初版自审曾误称 registry 已定稿而实际只有测试契约——评审 P2-1 抓获，本版已补齐。）
- **评审修订（2026-10-08 code-reviewer，结论「修订后可开工」）**：P0-1 claude 布局（父转写项目层平铺——locate/夹具/step 三处一致 + T3 Step 4b 阻塞实机复核 + C3 归咎改写）；P1-1 重建尾半行（scan_parent_rev 返回尾半行 + IncrState::resume_at + 新用例）；P1-2 kimi sessionDir 越界校验 + 逃逸用例；P2-1 registry 定稿实现补齐；P2-2 child id 样本 ≥8 字符防切片 panic；P2-3 kimi_data_root/kimi_home 复用 + ms_to_iso/json_time_to_iso 待收口申报；P2-4 codex 首行摘要走 SessionFileScan 负缓存（删手搓 stat 门控与索引全局态）；P3-1/2/3/4/5 快修（paired_cookie 签名 / mod.rs 插入位 / v2_db 锚点 / 端点无活跃度判定取舍 / spawnDepth 口径明示）。
- **类型一致性**：`SubagentView`/`TokenUsage`（Rust）↔ `SubagentView`（TS，camelCase）↔ 端点 JSON；`SubagentSourceFn = Fn(&str,&str) -> Vec<SubagentView>` 全链一致；`sort_views` 三工具 source 与端点双侧调用（幂等，无害）。
