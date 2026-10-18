# 子代理完成信号误报修复 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 消除「主会话仍在等子代理/后台任务，MAM 却判绿并播提示音、发通知、插未读卡」这一类误报，并把「工具注入的合成消息被当成用户提问/末条消息展示」一并清掉。

**Architecture:** 修复分两个正交缺陷类，落在共享判定核与各 parser 两层：

- **P1 假绿（状态误判）**：给共享层 `monitor::app_status` 增加「未结算后台工作」的第四种后代态，把「回合收尾判绿」纳入仲裁；各工具提供「后台工作在跑吗」的数据源。
- **P2 脏预览（合成消息）**：在共享层新增注入条目的统一识别口径，各 parser 的**跳过白名单从"枚举已知记账 kind"改成"识别合成标记"**（dsh 的 `source.kind == "user"` 白名单是唯一正确范式，其余工具向其看齐）。

**Tech Stack:** Rust（Tauri 2 侧）、rusqlite、serde_json、chrono；前端无需改动（绿→提示音的链路本身正确，无需动 `useNotification.ts`）。

**Spec:** 本方案由 2026-09-14 的实机调查直接产出，原始证据见本文档「附录 A · 实机探测证据」。相关上游设计：`docs/MASTER-PLAN.md` D14（未读语义对齐全工具口径）、`docs/superpowers/plans/2026-09-14-m1-dsh-monitoring-adapter.md`（dsh 接入基线）。

## Global Constraints

- **只读红线**：MAM 不写任何工具的数据目录一个字节。所有新逻辑只读既有数据源（SQLite / JSONL / metadata.json）。
- **`docs/MASTER-PLAN.md` 是最高权威**，不得修改。本方案与它冲突时停下报告，由用户裁决。
- 中文注释；英文标识符；commit 遵循 conventional commits（仓库有 commitlint）。
- **扫描预算契约**（`src-tauri/src/monitor/session_scan.rs` 模块文档 + AGENTS.md）是硬约束：前端每 3 秒轮询，新增文件 IO 必须走 `SessionFileScan`（L2 缓存），无界历史目录扫描受 L3 24h 窗口约束，`stat` 预过滤必须在打开文件之前。**本方案新增的所有数据源读取都必须说明其预算归属。**
- **防御性要求**：任一数据源缺失/表缺失/字段类型不符/JSON 损坏 → 降级为"无信号"（= 修复前的行为），绝不 panic。（先例：`zcode_parser.rs` 模块文档头）
- 每个 Task 结束必须 `cd src-tauri && cargo test` 全绿 + `cargo clippy --all-targets -- -D warnings` 零告警 + `cargo fmt` 后 commit。测试一律用 tempdir fixture / 内存 SQLite，**严禁触碰真实 `~/.zcode`、`~/.claude`、`~/.codex`、`~/.dsh`**。
- 涉及"实机探测"的 Task，探测脚本输出一律落 `research/` 下的报告文件（该目录不入库）。

---

## 背景与缺陷定义

### 实机确认的缺陷清单

调查基线：`~/.zcode/cli/db/db.sqlite`（451 会话 / 24232 message / 94078 part）、`~/.claude/projects`（59 主 transcript + 23 子代理文件）、`~/.codex/sessions`（383 rollout）、`~/.dsh/sessions`（84 日志）。

| 编号 | 缺陷 | 证据强度 | 实机数据 |
|---|---|---|---|
| **Z-P1** | ZCode 主会话派发**后台**子代理后结束自己的回合（`step-finish(stop)`）→ 判绿，而子代理仍在跑 | **实机确认** | 392 次转绿中 **256 次（65%）** 在转绿后 60s 内仍有子代理写库；`sess_5a57eda4` 有 3 次连续假绿（21:04:58 / 21:14:10 / 21:14:35），最后一个子代理 21:46:41 才结束 |
| **Z-P2** | ZCode 合成通知消息（`semantics.kind` = `background_notification` / `subagent_notification` / `system_reminder`）被译成 `UserMessage`（假黄），并把 `<task-notification>...` 原文当末条消息上卡/进通知 | **实机确认** | 合成 user 消息 1051 条（含 `todo_reminder` 815）；**当前白名单只跳过 `todo_reminder`/`timeline_event`**，其余全部漏过。用户通知历史里有 3 条 `<task-notification>` 原文 |
| **C-P1** | Claude 主会话尾部 assistant 文本 → 判绿，而 `subagents/*.jsonl` 仍在写 | **实机确认** | 3 个父会话、24 个窗口；最长 15.7 分钟窗口内 372 次子代理写入 |
| **C-P2** | Claude 主 transcript 的 `origin.kind=task-notification` / `isMeta` / `<local-command-stdout>` 被当成用户消息 | **实机确认** | 59 个会话中 **29 个**当前尾部即合成条目（24 × `"## Context Usage..."`、5 × stdout），这些会话永远不转绿 |
| **X-P1** | Codex CLI 尾部 user 条目为注入消息（`<turn_aborted>` / `<environment_context>`）→ 假黄 + 脏预览；`role=developer` 也进预览 | **实机确认** | 383 rollout 中 **37 个**假黄、**45 个**脏预览、5 个 developer 角色上卡 |
| **X-P1'** | Codex CLI 把**子代理 rollout**（`source.subagent`，67 个）当会话卡候选，按 mtime 可与父抢占 | 代码路径确认，用户可见性待探测 | `is_rollout_file` 无 source 过滤 |
| **K-P2** | Kimi `origin.kind=injection` / `background_task` 被当用户消息 | **实机确认** | 195 个模拟时刻假黄 + 脏预览；`<notification id="task:...">` 与 ZCode 同类 |
| **D-P1** | dsh 父会话 `turn/end completed` → 判绿，子代理仍在跑 | **实机确认（最严重）** | `session-946b624e`：绿灯持续 **777 秒**，期间 3 个后台子代理在跑；`parent_session` 字段已解析但**从未使用** |
| **W-P2** | WorkBuddy `<system-reminder data-role="user-context">`（最大 14322 字符）被当用户消息，且 `extract_message_text` **无长度截断** | **实机确认** | 2 个文件、1-4 条注入消息在尾窗内 |
| **O-P2** | OpenCode `## New Messages` 合成消息被当用户消息 | **实机确认（窗口短）** | 5 条，91ms 后即被 assistant 覆盖 |
| **Dsh-P2** | — | **无缺陷** | `preview.rs:49` 的 `source.kind == "user"` 白名单是唯一正确实现，**作为修复范式** |

### 三个必须澄清的技术事实（决定方案结构）

**事实 1：只跳过通知消息不足以修复假绿。**
用户初始设想（"把子代理返回的写入日志跳过"）只解决 P2 的一半。真实数据回放证明：`sess_5a57eda4` 在 21:04:58 判绿时，那条绿**完全不来自通知消息**，而是父会话自己的 `step-finish(reason=stop)`。跳过通知后回放，21:04:58 仍判绿。**两个缺陷必须分开修。**

**事实 2：ZCode 自己写了权威的子代理状态文件。**
`~/.zcode/cli/agents/<parent_sess>/agent_<uuid>/metadata.json` 含 `status: running|completed|failed` + `completedAt`。383 个文件中 `status=running` 恰好 1 个（就是当时真在跑的那个）。**这是比"子会话是否在写库"更权威的信号，但代价是文件 IO**——需与 DB 侧信号权衡（见 Task 3）。

**事实 3：ZCode 的"后台派发"有结构化标记，且派发↔结算 100% 配对。**
派发侧：`part.data.state.input.run_in_background == true`（Agent 21 条 + Bash 90 条 = 111 条，`output` 字段含 `agentId:` / `ID: exec_`，**111/111 可提取 workId**）。结算侧：`background_notification.metadata.originMeta.workId`。**闭环率 99%（110/111），唯一未结算是 28882 分钟前的僵尸派发**——该信号可直接用于"后台工作在跑吗"。

---

## 文件结构

| 文件 | 职责 | 改动 |
|---|---|---|
| `src-tauri/src/monitor/app_status.rs` | 共享判定核：新增第四种后代态 + 仲裁分支 | P1 核心 |
| `src-tauri/src/monitor/zcode_parser.rs` | ZCode：注入识别 + 未结算后台工作数据源 | Z-P1/Z-P2 |
| `src-tauri/src/monitor/claude_parser.rs` | Claude：注入识别 + 子代理活跃度 + digest 扩字段 | C-P1/C-P2 |
| `src-tauri/src/monitor/jsonl.rs` | Claude 子代理文件的活跃度探测（复用既有 `is_subagent_file`） | C-P1 数据源 |
| `src-tauri/src/monitor/codex_parser.rs` | Codex CLI：注入识别 + 子代理 rollout 排除 | X-P1/X-P1' |
| `src-tauri/src/monitor/codex_thread_parser.rs` | Codex APP：绿态仲裁接入 | P1 接入 |
| `src-tauri/src/monitor/kimi_parser.rs` | Kimi：注入识别 | K-P2 |
| `src-tauri/src/monitor/dsh/mod.rs`、`dsh/status.rs` | dsh：父子关联 + 绿态仲裁 | D-P1 |
| `src-tauri/src/monitor/workbuddy_parser.rs` | WorkBuddy：注入识别 + 截断 | W-P2 |
| `src-tauri/src/monitor/opencode_parser.rs` | OpenCode：注入识别 | O-P2 |
| `src-tauri/src/session/model.rs` | `JsonlMessage` 增补字段（`isMeta`/`origin`/`isSidechain`） | P2 数据模型 |
| `docs/superpowers/specs/2026-09-15-subagent-completion-signal-design.md` | 设计规格（本方案的 spec 伴随物，Task 1 产出） | 新增 |
| `research/subagent-signal-probe-2026-09-15.md` | 实机探测报告（Task 2 产出，不入库） | 新增 |

### 关键接口（先定义，后续 Task 遵循）

```rust
// monitor/app_status.rs —— P1 新枚举成员（Task 1 定义）
pub enum DescendantActivity {
    Active,
    Stale,
    Absent,
    /// 存在**已派发但未结算**的后台工作（子代理/后台 bash/后台任务）。
    /// 与 Active 的区别：Active 依赖"最近有写入"（会被长思考误判 Stale），
    /// Pending 依赖"派发事件已见、结算事件未见"，语义是"账没结清"。
    /// 判定优先级：Pending > Active > Stale > Absent
    Pending,
}

// monitor/app_status.rs —— P1 新仲裁函数（Task 1）
/// 绿态仲裁（补充 overlay_stale_with_descendants 未覆盖的分支）：
/// Idle/Finished + Pending ⇒ Waiting（"任务未完结，别急着报完成"）
/// 其余组合透传。既有工具传 Absent 时行为零变化。
pub fn overlay_completion_with_pending(
    status: SessionStatus,
    descendants: DescendantActivity,
) -> SessionStatus;

// monitor/app_status.rs —— P2 注入识别（Task 1）
/// 归一化条目来源（各 parser 的格式翻译层产出）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryOrigin {
    /// 真人输入
    Human,
    /// 工具注入的合成条目（通知/提醒/上下文/摘要）
    Synthetic,
    /// 无法判定 —— 保守按 Human 处理（防御方向：宁可假黄，不可漏判真人）
    Unknown,
}
```

---

## 阶段划分

| 阶段 | 内容 | 可独立合并 |
|---|---|---|
| **A（Task 1-4）** | ZCode 两个缺陷 + 共享层能力 | ✅ 用户当前痛点，独立价值 |
| **B（Task 5-7）** | Claude + Codex（CLI/APP） | ✅ |
| **C（Task 8-9）** | dsh + Kimi | ✅ |
| **D（Task 10-11）** | WorkBuddy + OpenCode + 跨工具回归 | ✅ |

每阶段结束可合并；后续阶段依赖 Task 1 的共享层接口。

---

### Task 1: 共享层——第四种后代态 + 注入识别口径

**Files:**
- Modify: `src-tauri/src/monitor/app_status.rs`（`DescendantActivity` 枚举 + 新函数 + 测试）
- Create: `docs/superpowers/specs/2026-09-15-subagent-completion-signal-design.md`

**Interfaces:**
- Produces: `DescendantActivity::Pending`；`overlay_completion_with_pending(status, descendants) -> SessionStatus`；`EntryOrigin` 枚举；`pub fn entry_origin_from_flags(is_synthetic: bool, ui_hidden: bool) -> EntryOrigin`（各 parser 的共享判据）。

- [ ] **Step 1: 写失败测试**

在 `src-tauri/src/monitor/app_status.rs` 的 `mod tests` 内追加：

```rust
    // ---- P1：绿态仲裁（2026-09-15 子代理完成信号修复）----

    /// 核心回归锁：回合收尾判绿 + 后台工作未结算 ⇒ 不判绿
    #[test]
    fn pending_descendants_block_green() {
        assert_eq!(
            overlay_completion_with_pending(SessionStatus::Idle, DescendantActivity::Pending),
            SessionStatus::Waiting,
            "后台工作未结算时不得报完成（Z-P1/C-P1/D-P1 根因）"
        );
        assert_eq!(
            overlay_completion_with_pending(SessionStatus::Finished, DescendantActivity::Pending),
            SessionStatus::Waiting,
            "error 提示转绿同样受仲裁约束"
        );
    }

    /// 无未结算工作 → 绿态透传（既有行为零变化）
    #[test]
    fn non_pending_keeps_green() {
        for status in [SessionStatus::Idle, SessionStatus::Finished] {
            for desc in [
                DescendantActivity::Active,
                DescendantActivity::Stale,
                DescendantActivity::Absent,
            ] {
                assert_eq!(
                    overlay_completion_with_pending(status.clone(), desc),
                    status,
                    "只有 Pending 拦截绿态；Active/Stale/Absent 不得改变既有语义"
                );
            }
        }
    }

    /// 非绿态不受影响（黄/红语义不变）
    #[test]
    fn non_green_statuses_ignore_pending() {
        for status in [
            SessionStatus::Processing,
            SessionStatus::Thinking,
            SessionStatus::Waiting,
            SessionStatus::Compacting,
        ] {
            assert_eq!(
                overlay_completion_with_pending(status.clone(), DescendantActivity::Pending),
                status
            );
        }
    }

    /// 组合序锁：先停更仲裁、后完成仲裁，两级互不吞没
    #[test]
    fn overlays_compose_in_order() {
        // 停更的 Processing + 后代活跃 → 保持 Processing；再经完成仲裁仍是 Processing
        let s1 = overlay_stale_with_descendants(
            SessionStatus::Processing,
            APP_STATUS_STALE_MS * 2,
            DescendantActivity::Active,
        );
        assert_eq!(s1, SessionStatus::Processing);
        assert_eq!(
            overlay_completion_with_pending(s1, DescendantActivity::Pending),
            SessionStatus::Processing
        );
        // 停更的 Processing + 无后代 → Waiting；完成仲裁不动 Waiting
        let s2 = overlay_stale_with_descendants(
            SessionStatus::Processing,
            APP_STATUS_STALE_MS * 2,
            DescendantActivity::Absent,
        );
        assert_eq!(s2, SessionStatus::Waiting);
    }

    // ---- P2：注入识别口径 ----

    #[test]
    fn synthetic_flags_map_to_synthetic_origin() {
        assert_eq!(entry_origin_from_flags(true, false), EntryOrigin::Synthetic);
        assert_eq!(entry_origin_from_flags(false, true), EntryOrigin::Synthetic);
        assert_eq!(entry_origin_from_flags(true, true), EntryOrigin::Synthetic);
    }

    #[test]
    fn clean_flags_map_to_human_origin() {
        assert_eq!(entry_origin_from_flags(false, false), EntryOrigin::Human);
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd src-tauri && cargo test monitor::app_status::tests -- --nocapture 2>&1 | tail -20
```
Expected: 编译失败，`cannot find function overlay_completion_with_pending`。

- [ ] **Step 3: 最小实现**

在 `src-tauri/src/monitor/app_status.rs` 中：

1. 给 `DescendantActivity` 追加成员（**放在枚举末尾并加注释说明优先级**）：

```rust
    /// 无后代会话（既有工具的空后代语义 = 现状行为不变）
    Absent,
    /// 存在**已派发但未结算**的后台工作（子代理 / 后台 bash / 后台任务）：
    /// 与 Active 的本质区别——Active 依赖"最近有写入"，长思考期会被误判 Stale；
    /// Pending 依赖"派发事件已见、结算事件未见"，语义是"账没结清"，与静默无关。
    /// 判定优先级（各 parser 的映射函数负责）：Pending > Active > Stale > Absent
    Pending,
```

2. 新增仲裁函数（紧随 `overlay_stale_with_descendants` 之后）：

```rust
/// 完成态仲裁（2026-09-15 子代理完成信号修复）：
/// 回合收尾推导出的绿（Idle/Finished）在**存在未结算后台工作**时不成立——
/// 真实场景：主会话派发后台子代理后结束本回合（ZCode step-finish(stop) /
/// Claude assistant 尾部 / dsh turn/end completed），子代理仍在跑，MAM 却报完成
/// 并播提示音、发通知、插未读卡。实测 ZCode 392 次转绿中 256 次（65%）属此类。
///
/// 非 Pending 一律透传：Active/Stale/Absent 保持既有语义（既有工具零变化，
/// 见 non_pending_keeps_green 回归锁）。黄/红态不受影响——Pending 只拦"报完成"。
pub fn overlay_completion_with_pending(
    status: SessionStatus,
    descendants: DescendantActivity,
) -> SessionStatus {
    match (status, descendants) {
        (SessionStatus::Idle | SessionStatus::Finished, DescendantActivity::Pending) => {
            SessionStatus::Waiting
        }
        (other, _) => other,
    }
}
```

3. 追加注入识别口径：

```rust
/// 归一化条目来源（各 parser 格式翻译层产出，P2 修复的共享判据）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryOrigin {
    /// 真人输入
    Human,
    /// 工具注入的合成条目（子代理/后台任务通知、系统提醒、上下文注入、摘要）
    Synthetic,
    /// 无法判定
    Unknown,
}

/// 合成标记 → 条目来源（各 parser 共享的判据）：
/// 工具侧显式的 synthetic 标记或"UI 不可见"标记任一命中即视为注入。
/// 两标记实测对真实用户输入零误伤（ZCode：1051 条合成全部命中，629 条
/// user_prompt 全部不命中）。
///
/// 为何不用 kind 枚举白名单：kind 取值随工具版本改名（ZCode 已有
/// background_notification / subagent_notification / system_reminder /
/// todo_reminder / compact_summary 五种），枚举法必然随版本失效——这正是
/// 本轮缺陷的成因（现有白名单只列了两种）。**语义标记优先于枚举。**
pub fn entry_origin_from_flags(is_synthetic: bool, ui_hidden: bool) -> EntryOrigin {
    if is_synthetic || ui_hidden {
        EntryOrigin::Synthetic
    } else {
        EntryOrigin::Human
    }
}
```

4. 创建 spec 文档 `docs/superpowers/specs/2026-09-15-subagent-completion-signal-design.md`，内容为本文档「背景与缺陷定义」+「三个必须澄清的技术事实」+ 上述接口定义，并注明证据出处（附录 A）。

- [ ] **Step 4: 跑测试确认通过**

```bash
cd src-tauri && cargo test monitor::app_status::tests 2>&1 | tail -20
```
Expected: 全部 PASS（含既有 18 个测试）。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/monitor/app_status.rs docs/superpowers/specs/2026-09-15-subagent-completion-signal-design.md
git commit -m "feat(monitor): 共享层——未结算后台工作仲裁 + 注入条目识别口径"
```

---

### Task 2: 实机探测——补齐"需实机采样确认"项

**Files:**
- Create: `research/subagent-signal-probe-2026-09-15.md`（不入库，`.gitignore` 已覆盖 `research/`）
- Create: `scripts/probe-subagent-signals.sh`（可选，探测脚本留档）

**为何先探测**：审计报告有 5 项标"需实机采样确认"（OpenClaw 假绿率、Codex 子代理抢占、Kimi v1.5+swarm、workbuddy/opencode 注入落尾率、dsh 父子关联可用性）。**Task 3 起依赖这些结论确定判据优先级**，故先探测。

**Interfaces:**
- Produces: 探测报告，每项给出「判定 + 证据 + 对方案的影响」。报告结论若与本方案冲突，**停下报告用户**，不自行改方案。

- [ ] **Step 1: 探测 OpenClaw 假绿率**

OpenClaw 用 `cpu_usage > 5.0` 判状态（`openclaw_parser.rs:143-148`），CPU 空闲的后台等待会被判 Idle。采集方法：在 openclaw 运行时，以 3 秒间隔采样其 CPU 与 `~/.openclaw/agents/*/sessions/*.jsonl` 的 mtime，标出"CPU ≤5% 但会话文件仍在写"的时段。

```bash
# 需先启用 openclaw（agent_tools.enabled=0，当前未实测）
# 采样 10 分钟，记录 CSV：timestamp,cpu,session_mtime
```

Expected: 报告给出「假绿窗口占比」。若 >10%，Task 11 增加 OpenClaw 专项任务；否则记为已知限制。

- [ ] **Step 2: 探测 Codex 子代理 rollout 的用户可见性**

```bash
python3 - <<'PY'
import json, glob, os
files=glob.glob(os.path.expanduser("~/.codex/sessions/**/rollout-*.jsonl"), recursive=True)
sub=[]
for f in files:
    try: v=json.loads(open(f, errors='replace').readline())
    except Exception: continue
    src=(v.get('payload') or {}).get('source')
    if isinstance(src, dict) and 'subagent' in src: sub.append(f)
print(f"子代理 rollout: {len(sub)}/{len(files)}")
# 对每个子代理 rollout：其 cwd 与 mtime，检查是否可能与父 rollout 竞争同一 CLI 进程
for f in sub[:10]:
    st=os.stat(f); print(f"  {os.path.basename(f)[:40]} mtime={st.st_mtime:.0f}")
PY
```
Expected: 输出子代理 rollout 的 cwd/mtime 分布。若存在"同 cwd 且 mtime 更新"的父子对 → X-P1' 成立，Task 6 必须加 source 过滤。

- [ ] **Step 3: 探测 Kimi v1.5 + swarm 组合**

```bash
python3 - <<'PY'
import json, glob, os, collections
mains=glob.glob(os.path.expanduser("~/.kimi-code/sessions/*/*/agents/main/wire.jsonl"))
for m in mains:
    agents=glob.glob(os.path.join(os.path.dirname(m), "agent-*"))
    ver=None
    for line in open(m, errors='replace'):
        try: v=json.loads(line)
        except Exception: continue
        if 'version' in v: ver=v['version']; break
    if agents: print(f"v={ver} swarm={len(agents)} {os.path.basename(os.path.dirname(os.path.dirname(m)))[:40]}")
PY
```
Expected: 找出「v1.5 且带 agent-* 目录」的会话。存在 → K-P1 风险成立，Task 9 需补子代理活跃度；不存在 → 记已知限制。

- [ ] **Step 4: 探测 WorkBuddy / OpenCode 注入落尾率**

复刻各自 parser 的尾部推导，统计"注入消息为末条"的真实时长占比（WorkBuddy 用 `~/.workbuddy/projects/*/*.jsonl`，OpenCode 用 `~/.local/share/opencode/opencode.db`）。Expected: 给出占比数字，决定 P2 修复的紧迫度排序。

- [ ] **Step 5: 探测 dsh 父子关联可用性**

```bash
python3 - <<'PY'
import glob, os, json, zstandard as zstd   # 若 zstd 不可用则 pip install zstandard
# 找 origin=subagent 且 delegation_depth>0 的日志，确认 header.parent_session 是否指向父会话 id
PY
```
Expected: 确认 `parent_session` 是否可靠填充。**若不可靠**，Task 8 改用「主会话内 `subagent-settled` 事件计数」作为 Pending 数据源（该事件实测存在：`session-946b624e` seq 21383）。

- [ ] **Step 6: 写报告并 Commit（脚本部分）**

报告格式：每项一节，含「探测方法 / 原始数据 / 判定 / 对方案的影响」。

```bash
git add scripts/probe-subagent-signals.sh
git commit -m "chore(probe): 子代理信号实机探测脚本（探测报告见 research/，不入库）"
```

---

### Task 3: ZCode——注入识别（Z-P2）

**Files:**
- Modify: `src-tauri/src/monitor/zcode_parser.rs:624-760`（`flatten_entries` / `last_message_summary` / `first_user_text`）
- Test: 同文件 `mod tests`

**Interfaces:**
- Consumes: Task 1 的 `EntryOrigin` / `entry_origin_from_flags`
- Produces: `fn message_origin(v: &serde_json::Value) -> EntryOrigin`（ZCode 侧格式翻译）

**判据（实机验证）**：
- `synthetic == true` 或 `semantics.uiVisibility == "hidden"` → Synthetic
- 实测：1051 条合成 user 消息全部命中，629 条真实 `user_prompt` 零误伤
- 保留既有 `todo_reminder` / `timeline_event` kind 判断作为**纵深防御**（双保险，标记缺失时兜底）

- [ ] **Step 1: 写失败测试**

```rust
    /// Z-P2 回归锁：合成通知消息不参与状态判定、不上预览（2026-09-15）
    #[test]
    fn synthetic_notifications_are_skipped() {
        // 通知消息（synthetic=true，带 <task-notification> 正文）
        let notif = r#"{"role":"user","synthetic":true,
            "semantics":{"kind":"background_notification","uiVisibility":"hidden"},
            "metadata":{"originMeta":{"backgroundSource":"subagent","workId":"agent_x"}}}"#.to_string();
        // 真实用户输入
        let human = r#"{"role":"user","semantics":{"kind":"user_prompt"},"synthetic":false}"#.to_string();
        let msgs = vec![
            MessageRow { id: "m1".into(), data: msg_data("assistant", "") },
            MessageRow { id: "m2".into(), data: notif },
            MessageRow { id: "m3".into(), data: human },
        ];
        let parts = HashMap::new();
        let entries = flatten_entries(&msgs, &parts);
        // 通知不产生 UserMessage：只应有 m1(assistant) 与 m3(user) 两条
        assert_eq!(entries.len(), 2, "合成通知不得进入判定条目");
        assert_eq!(entries[1], AppEntryKind::UserMessage, "真实用户输入仍判 UserMessage");

        // 末条消息：跳过后应取真实用户输入，而非 <task-notification> 正文
        let mut parts2 = HashMap::new();
        parts2.insert("m2".into(), vec![PartRow { data: r#"{"type":"text","text":"<task-notification>\n<task-id>agent_x</task-id>"}"#.into() }]);
        parts2.insert("m3".into(), vec![PartRow { data: r#"{"type":"text","text":"继续"}"#.into() }]);
        let (last, role) = last_message_summary(&msgs, &parts2);
        assert_eq!(last.as_deref(), Some("继续"), "不得把通知原文当末条展示");
        assert_eq!(role.as_deref(), Some("user"));
    }

    /// 纵深防御：标记缺失但 kind 已知 → 仍跳过
    #[test]
    fn known_kind_skipped_without_synthetic_flag() {
        let old = r#"{"role":"user","semantics":{"kind":"todo_reminder"}}"#.to_string();
        let msgs = vec![MessageRow { id: "m1".into(), data: old }];
        assert!(flatten_entries(&msgs, &HashMap::new()).is_empty());
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd src-tauri && cargo test monitor::zcode_parser::tests::synthetic_notifications_are_skipped 2>&1 | tail -20
```
Expected: FAIL — `assertion left == right failed, left: 3, right: 2`。

- [ ] **Step 3: 实现**

在 `zcode_parser.rs` 追加（放在 `flatten_entries` 之前）：

```rust
/// 消息来源判定（Z-P2）：工具侧显式合成标记优先，kind 枚举作纵深防御。
/// 实测（2026-09-14，451 会话）：synthetic=1 或 uiVisibility=hidden 恰好覆盖
/// 全部 1051 条注入消息（todo_reminder 815 / background_notification 157 /
/// system_reminder 72 / subagent_notification 7），629 条 user_prompt 零误伤。
/// 保留 kind 判断：私有格式升级可能丢标记，双保险方向与既有白名单一致
fn message_origin(v: &serde_json::Value) -> EntryOrigin {
    let synthetic = v.get("synthetic").and_then(|b| b.as_bool()).unwrap_or(false);
    let ui_hidden = v
        .pointer("/semantics/uiVisibility")
        .and_then(|s| s.as_str())
        == Some("hidden");
    if entry_origin_from_flags(synthetic, ui_hidden) == EntryOrigin::Synthetic {
        return EntryOrigin::Synthetic;
    }
    let kind = v.pointer("/semantics/kind").and_then(|k| k.as_str()).unwrap_or_default();
    // 已知记账 kind 兜底（标记缺失场景）；新增 kind 时在此追加
    if matches!(kind, "todo_reminder" | "timeline_event" | "system_reminder") {
        return EntryOrigin::Synthetic;
    }
    EntryOrigin::Human
}
```

然后修改三处：

1. `flatten_entries` 的 user 分支：把 kind 白名单判断换成 `message_origin(&v)`：

```rust
            ("user", _) => {
                // Z-P2：合成注入（通知/提醒/上下文）不参与状态判定——
                // 判成 UserMessage 会让"子代理返回"看起来像"用户提问"（假黄），
                // 且其正文会污染预览（用户通知历史实测出现 3 条 <task-notification> 原文）
                if message_origin(&v) == EntryOrigin::Synthetic {
                    continue;
                }
                entries.push(AppEntryKind::UserMessage);
            }
```

2. `last_message_summary`：把 `matches!((role, kind), ("user","todo_reminder") | ("user","timeline_event"))` 整段替换为：

```rust
        if role == "user" && message_origin(&v) == EntryOrigin::Synthetic {
            continue;
        }
```

3. `first_user_text`：同样替换：

```rust
        if role != "user" || message_origin(&v) == EntryOrigin::Synthetic {
            continue;
        }
```

4. 文件头 `use` 增补：`use super::app_status::{..., EntryOrigin, entry_origin_from_flags};`

5. 更新模块文档头（第 19-20 行附近）的记账消息说明，改为「按合成标记识别，kind 枚举作兜底」。

- [ ] **Step 4: 跑测试确认通过**

```bash
cd src-tauri && cargo test monitor::zcode_parser 2>&1 | tail -25
```
Expected: 全绿（含既有 20+ 测试）。特别注意 `subagent_long_task_keeps_main_processing` 等既有测试不得回归。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/monitor/zcode_parser.rs
git commit -m "fix(zcode): 合成通知消息不参与状态判定与预览（Z-P2）"
```

---

### Task 4: ZCode——未结算后台工作仲裁（Z-P1，本方案核心）

**Files:**
- Modify: `src-tauri/src/monitor/zcode_parser.rs`（新增 `load_pending_background` + 接入 `build_one_session`）
- Test: 同文件 `mod tests`

**Interfaces:**
- Consumes: Task 1 的 `DescendantActivity::Pending` / `overlay_completion_with_pending`
- Produces: `fn load_pending_background(conn: &Connection, tail: &[MessageRow], parts: &HashMap<String, Vec<PartRow>>) -> HashSet<String>`（返回"有未结算后台工作的父会话 id 集合"）

**数据源（实机验证，闭环率 99%）**：

| 侧 | 位置 | 实测量 |
|---|---|---|
| 派发 | `part.data.state.input.run_in_background == true`，`output` 含 `agentId: agent_<uuid>` 或 `ID: exec_<uuid>` | 111 条，workId **111/111 可提取** |
| 结算 | `message.data.metadata.originMeta.workId`（`kind` = `background_notification`） | 155 条 |
| 闭环 | 派发 − 结算 | 110/111 已闭环；唯一残留是 28882 分钟前的僵尸派发 |

**关键设计（扫描预算）**：**复用已加载的尾部数据**（`load_tail_messages` 的 200 条 + `load_parts_for_messages` 的 parts），**零新增查询**。风险：派发早于尾窗时不可见（实测 `sess_35bb3c94` 最老派发距尾 253 条，会漏）。缓解见 Step 3 第 3 点的**超龄保护**与 Step 4 的验证。

- [ ] **Step 1: 写失败测试**

```rust
    /// Z-P1 回归锁：回合收尾 + 未结算后台工作 ⇒ 不判绿（2026-09-15）
    #[test]
    fn pending_background_blocks_green() {
        // 与 subagent_long_task_keeps_main_processing 同构，但主会话**已收尾**
        let tmp = tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        std::fs::create_dir_all(roots.cli_db.parent().unwrap()).unwrap();
        let conn = Connection::open(&roots.cli_db).unwrap();
        setup_schema(&conn);

        let past = now_ms() - 60_000;
        insert_session(&conn, SID_A, None, "interactive", "父会话", past);
        // 派发：后台 Agent（part 立即 completed，output 含 agentId）
        insert_message(&conn, "m1", SID_A, 1, past, r#"{"role":"assistant"}"#);
        insert_part(&conn, "p1", "m1", 0, past, r#"{"type":"tool","tool":"Agent",
            "state":{"status":"completed","input":{"run_in_background":true},
                     "output":"Async agent launched successfully.\nagentId: agent_beef0000-1111-2222-3333-444455556666 (internal ID)"}}"#);
        // 回合收尾 → 推导 Idle（绿）
        insert_message(&conn, "m2", SID_A, 2, past, r#"{"role":"assistant"}"#);
        insert_part(&conn, "p2", "m2", 0, past, r#"{"type":"step-finish","reason":"stop"}"#);
        // 注意：无 background_notification ⇒ 未结算

        let host = fake_host();
        let sessions = build_sessions(&roots, &host, now_ms());
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions[0].status,
            SessionStatus::Waiting,
            "后台子代理未返回，父会话不得报完成（Z-P1 根因）"
        );
    }

    /// 结算后恢复绿（回归锁：不误伤正常完成）
    #[test]
    fn settled_background_allows_green() {
        // 同上，但追加 background_notification 结算
        // ...（fixture 同构，末尾追加 insert_message m3：
        //   {"role":"user","synthetic":true,"semantics":{"kind":"background_notification"},
        //    "metadata":{"originMeta":{"workId":"agent_beef0000-1111-2222-3333-444455556666"}}}
        //   以及后续 assistant 收尾）
        // 断言 status == Idle
    }

    /// 超龄保护：派发在尾窗外、且无结算证据 → 不永久劫持（防僵尸派发）
    #[test]
    fn stale_pending_does_not_hijack_forever() {
        // 派发 part 的时间戳设为 25 小时前（超过 CARD_WINDOW_MS）
        // 断言：仍判 Idle（超龄派发不参与 Pending 判定）
    }

    /// 前台派发不受影响（既有语义不变）
    #[test]
    fn foreground_dispatch_unaffected() {
        // run_in_background 缺失 → 不产生 Pending → 既有 Processing 行为不变
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd src-tauri && cargo test monitor::zcode_parser::tests::pending_background_blocks_green 2>&1 | tail -20
```
Expected: FAIL — `left: Idle, right: Waiting`。

- [ ] **Step 3: 实现**

1. 新增函数：

```rust
/// 未结算后台工作探测（Z-P1）：从**已加载的尾部数据**提取"派发已见、结算未见"
/// 的后台工作 id 集合，返回父会话 id → 是否仍有未结算工作。
///
/// 判据（实机验证，2026-09-14）：派发 = part.state.input.run_in_background == true
/// 且 output 含 workId（agentId/exec ID）；结算 = background_notification 的
/// metadata.originMeta.workId 出现在尾部。111 条派发 / 155 条结算，闭环率 99%。
///
/// 扫描预算：**零新增查询**——只读 load_tail_messages 已取的 200 条消息与
/// 其 parts。代价 = O(尾窗条目数) 的 JSON 解析。
///
/// 超龄保护：派发时刻早于 CARD_WINDOW_MS（24h）的视为僵尸（实测存在 1 例
/// 28882 分钟前的未结算派发），不参与判定——防止"某次异常中断的派发"
/// 永久劫持该会话的状态。
fn load_pending_background(
    tail: &[MessageRow],
    parts: &HashMap<String, Vec<PartRow>>,
    now: i64,
) -> bool {
    let mut dispatched: HashSet<String> = HashSet::new();
    let mut settled: HashSet<String> = HashSet::new();
    for msg in tail {
        // 结算侧
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&msg.data) {
            if let Some(w) = v
                .pointer("/metadata/originMeta/workId")
                .and_then(|s| s.as_str())
            {
                settled.insert(w.to_string());
            }
        }
        // 派发侧
        for part in parts.get(&msg.id).map(|v| v.as_slice()).unwrap_or_default() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&part.data) else {
                continue;
            };
            if v.get("type").and_then(|t| t.as_str()) != Some("tool") {
                continue;
            }
            if v.pointer("/state/input/run_in_background").and_then(|b| b.as_bool()) != Some(true) {
                continue;
            }
            // 超龄保护：派发时刻早于 24h 卡片窗的视为僵尸（实测 1 例 28882 分钟前的
            // 未结算派发），不参与判定——防止异常中断的派发永久劫持会话状态。
            // 时间戳来自 part 的 state.time.start（毫秒）；缺失/unparseable 按新鲜处理
            // （防御方向 = 保持 Pending，宁可晚一轮报绿，不可漏报未完成）
            let started = v
                .pointer("/state/time/start")
                .and_then(|t| t.as_i64())
                .unwrap_or(now);
            if now.saturating_sub(started) > CARD_WINDOW_MS {
                continue;
            }
            let out = v.pointer("/state/output").and_then(|s| s.as_str()).unwrap_or("");
            let Some(w) = extract_work_id(out) else { continue };
            dispatched.insert(w);
        }
    }
    // 未结算 = 派发集合 − 结算集合（差额非空）
    dispatched.iter().any(|w| !settled.contains(w))
}

/// 从派发 output 提取 workId：`agentId: agent_<uuid>` 或 `ID: exec_<uuid>`
/// （111/111 实测可提取；格式变化时返回 None → 该派发不参与判定，降级为修复前行为）
fn extract_work_id(output: &str) -> Option<String> {
    for line in output.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("agentId: ") {
            let id = rest.split_whitespace().next().unwrap_or("");
            // 形态校验复用既有严格 UUID 判定（agent_ 后为 36 字符 UUID）
            if let Some(uuid) = id.strip_prefix("agent_") {
                if super::workbuddy_parser::is_strict_uuid_form(uuid) {
                    return Some(id.to_string());
                }
            }
        } else if let Some(rest) = line.split("ID: ").nth(1) {
            let id = rest.split_whitespace().next().unwrap_or("");
            if let Some(uuid) = id.strip_prefix("exec_") {
                if super::workbuddy_parser::is_strict_uuid_form(uuid) {
                    return Some(id.to_string());
                }
            }
        }
    }
    None
}
```

> **实现注意（给执行者）**：`state.time.start` 位于 part 的 `state.time` 子对象（实测形态 `{"start":1789049908662,"end":1789049908924}`）。**为 `extract_work_id` 单独写单元测试**（Agent 形态 / Bash 形态 / 畸形串返回 None 三种用例）。

2. 修改 `build_one_session`：在 error 提示分支**之后**接入完成仲裁（位置在 `task_status == "error"` 覆盖之后，确保 error 转绿同样受约束）：

```rust
    if task.task_status.as_deref() == Some("error")
        && !error_hint_superseded(task.updated_at, row.time_updated)
    {
        status = SessionStatus::Finished;
    }

    // Z-P1：完成仲裁——存在未结算后台工作时，回合收尾不得报绿。
    // 位置必须在 error 覆盖之后（否则 error 转绿会绕过）；此即本会话唯一的
    // 完成仲裁调用点，不存在第二条转绿路径。实测 65% 的转绿发生在子代理仍在跑时
    // （提示音/通知/未读卡均由此误触发）
    if load_pending_background(&messages, &parts, now) {
        status = overlay_completion_with_pending(status, DescendantActivity::Pending);
    }
```

> **给执行者**：`build_one_session` 当前的转绿路径只有两条——尾部推导（`derive_app_status` → TurnEnd → Idle）与 error 覆盖。上面这一处调用同时覆盖两者。**请通读该函数确认无第三条路径**（如 fallback 分支的 `Idle`）；若有，一并纳入。
>
> 注意 `load_pending_background` 只在 `status` 已是绿态时才可能改变结果（非绿态由 `overlay_completion_with_pending` 内部透传），故**无需为性能加条件前置**——实测该函数对 200 条尾窗条目的 JSON 解析在微秒级。

3. 更新模块文档头，把「子代理执行期间主会话零写入」那段扩写为「+ 后台派发与结算的配对判据」。

- [ ] **Step 4: 跑测试确认通过 + 实机验证**

```bash
cd src-tauri && cargo test monitor::zcode_parser 2>&1 | tail -25
```

实机验证（**关键**，用真实数据库跑一次判绿率对比）：

```bash
# 修复前后对比：对 sess_5a57eda4 在 21:04-21:47 的转绿次数
# 修复前 = 3 次假绿 + 1 次真绿；修复后应只剩 1 次（21:47:49）
cd src-tauri && cargo test -- --ignored zcode_live_replay 2>/dev/null || true
```

> 若既有代码无 live replay 测试基础设施，则在 Task 2 的探测脚本中追加一个 Python 复刻脚本（复刻修复后的判定），对 `sess_5a57eda4` / `sess_2db854ff` / `sess_6e7b9e58` 三个会话输出"转绿时刻表"，人工核对假绿消失且真绿保留。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/monitor/zcode_parser.rs
git commit -m "fix(zcode): 未结算后台工作阻止误判完成（Z-P1，65% 转绿误报根因）"
```

---

### Task 5: Claude——注入识别与子代理活跃度（C-P1 / C-P2）

**Files:**
- Modify: `src-tauri/src/session/model.rs:104-117`（`JsonlMessage` 增补字段）
- Modify: `src-tauri/src/monitor/claude_parser.rs`（digest 扩字段 + 跳过逻辑 + 活跃度接入）
- Modify: `src-tauri/src/monitor/jsonl.rs`（子代理活跃度探测函数）
- Test: `claude_parser.rs` / `jsonl.rs` 的 `mod tests`

**Interfaces:**
- Consumes: Task 1 的 `DescendantActivity` / `overlay_completion_with_pending` / `EntryOrigin`
- Produces: `jsonl::count_active_subagents_in(project_dir, parent_session_id, now) -> usize`（**修正现有 `count_active_subagents` 的目录范围 bug**——它当前扫平铺目录，而真实布局是 `<sessionId>/subagents/`，实测返回恒 0）

**判据（实机验证）**：
- C-P2：`isMeta == true` 或 `origin.kind == "task-notification"` 或内容以 `<local-command-stdout>` / `<command-name>` 开头 → Synthetic。实测 376 条 isMeta + 18 条 task-notification。
- C-P1：子代理活跃度用 `subagents/*.jsonl` 的 mtime（30s 窗口，与既有 `SUBAGENT_ACTIVE_WINDOW_MS` 口径一致）

- [ ] **Step 1: 写失败测试**

```rust
    /// C-P2 回归锁：合成条目不上状态、不上预览（2026-09-15）
    #[test]
    fn claude_synthetic_entries_skipped() {
        // isMeta 的 Context Usage 消息作为末条 → 不得判 Thinking
        let lines = vec![
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"完成"}]}}"#,
            r#"{"type":"user","isMeta":true,"message":{"role":"user","content":[{"type":"text","text":"## Context Usage\n..."}]}}"#,
        ];
        // 断言 digest 的 last_role 仍为 assistant（跳过 isMeta 后向前找）
        // ...
    }

    /// C-P1 回归锁：子代理活跃时父会话不判绿（2026-09-15）
    #[test]
    fn claude_active_subagent_blocks_green() {
        // fixture：父 transcript 尾部 assistant + subagents/ 下 1 个 30s 内写入的文件
        // 断言 status == Waiting（而非 Idle）
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd src-tauri && cargo test monitor::claude_parser::tests::claude_synthetic_entries_skipped 2>&1 | tail -15
```

- [ ] **Step 3: 实现**

1. `model.rs` 的 `JsonlMessage` 增补（保持 `Option` 以兼容旧数据）：

```rust
    #[serde(rename = "isMeta")]
    pub is_meta: Option<bool>,
    /// 条目来源（claude 写入：`{"kind":"task-notification"}` 等）
    pub origin: Option<serde_json::Value>,
    #[serde(rename = "isSidechain")]
    pub is_sidechain: Option<bool>,
```

2. `claude_parser.rs` 的 `ClaudeFileDigest` 增补 `last_origin_synthetic: bool`，在 `read_claude_digest` 的倒扫循环里计算：

```rust
            // C-P2：合成条目判定（isMeta / task-notification / 本地命令输出）
            let is_meta = msg.is_meta == Some(true);
            let is_task_notif = msg
                .origin
                .as_ref()
                .and_then(|o| o.get("kind"))
                .and_then(|k| k.as_str())
                == Some("task-notification");
            let text_head = /* 内容首 40 字符（复用既有 content 提取） */;
            let is_cmd_out = text_head.starts_with("<local-command-stdout>")
                || text_head.starts_with("<command-name>");
            let synthetic = is_meta || is_task_notif || is_cmd_out;
            if synthetic {
                continue; // 跳过，继续倒扫（既有 todo_reminder 同款语义）
            }
```

> **注意**：该 `continue` 必须放在 `if !found_status { ... }` **之前**，否则已找到的状态会被覆盖。执行者请核对控制流（现有代码在 `is_compacting` 分支已有类似结构）。

3. `claude_parser.rs` 的状态推导处（`claude_parser.rs:266-275` 的 `determine_status(...)` 调用）接入 Pending 仲裁：

```rust
    let status = if digest.is_compacting {
        SessionStatus::Compacting
    } else {
        determine_status(
            digest.last_msg_type.as_deref(),
            digest.last_has_tool_use,
            digest.last_has_tool_result,
            digest.last_is_local,
            digest.last_is_interrupted,
            digest.last_is_user_input,
            file_recently_modified,
        )
    };
    // C-P1：子代理活跃时不得报完成（实测最长 15.7 分钟窗口、372 次子代理写入；
    // 原 count_active_subagents 因目录范围错误恒返回 0，数据源长期缺失）
    let status = {
        let active = jsonl::count_active_subagents_in(project_dir, &session_id, now);
        if active > 0 {
            overlay_completion_with_pending(status, DescendantActivity::Pending)
        } else {
            status
        }
    };
```

> **给执行者**：`project_dir` 与 `now` 需从该函数上下文取得（`project_dir` 已在同作用域，`now` 用 `SystemTime::now()`）；`session_id` 已在上方 `digest.session_id.clone()?` 提取。

4. `jsonl.rs` 修正并新增：

```rust
/// 活跃子代理计数（修正版，2026-09-15）：claude 的真实布局是
/// `<project>/<sessionId>/subagents/agent-*.jsonl`（实测 23 个文件、4 层目录），
/// 而原 count_active_subagents 只扫平铺目录 → 实测恒返回 0（issue：C-P1 数据源缺失）。
/// 新实现按 sessionId 直达子代理目录，避免全目录遍历。
/// 扫描预算：单目录 read_dir + stat（<20 项），无文件打开，不计 L2/L3
pub(crate) fn count_active_subagents_in(
    project_dir: &Path,
    parent_session_id: &str,
    now: SystemTime,
) -> usize {
    let sub_dir = project_dir.join(parent_session_id).join("subagents");
    let Ok(entries) = fs::read_dir(&sub_dir) else { return 0 };
    entries
        .flatten()
        .filter(|e| is_subagent_file(&e.path()))
        .filter(|e| {
            e.metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|m| now.duration_since(m).ok())
                .map(|d| d < Duration::from_secs(30))
                .unwrap_or(false)
        })
        .count()
}
```

> **保留**原 `count_active_subagents`（平铺形态，可能在其他调用点或旧版本布局下有效），新增函数作为主路径；若确认无其他调用点则删除原函数（执行者核实后决定，在 commit message 中说明）。

- [ ] **Step 4: 跑测试确认通过**

```bash
cd src-tauri && cargo test monitor::claude_parser monitor::jsonl 2>&1 | tail -25
```

实机核对：找出"29 个尾部为合成条目的会话"，修复后这些会话应能正常转绿（不再是永久黄）。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/session/model.rs src-tauri/src/monitor/claude_parser.rs src-tauri/src/monitor/jsonl.rs
git commit -m "fix(claude): 合成条目跳过 + 子代理活跃度阻止误判完成（C-P1/C-P2）"
```

---

### Task 6: Codex CLI——注入识别与子代理 rollout 排除（X-P1 / X-P1'）

**Files:**
- Modify: `src-tauri/src/monitor/codex_parser.rs`（`codex_entry_kind` + `read_codex_digest` + `is_rollout_file`）
- Test: 同文件 `mod tests`

**Interfaces:**
- Consumes: Task 1 的 `EntryOrigin` / `overlay_completion_with_pending`
- Produces: `fn codex_is_injected_user(text: &str) -> bool`

**判据（实机验证）**：实测 383 rollout 中 37 个假黄、45 个脏预览。注入前缀族：`<turn_aborted>`, `<environment_context>`, `<recommended_plugins>`, `# AGENTS.md instructions for`, `<codex_internal_context`, `<skill>`, `<subagent_notification>`。

**设计取舍**：Codex 无 `synthetic` 标记，只能按**内容前缀**识别（白名单）。这是次优解——新增注入类型会漏。缓解：
1. 前缀表集中在一处并加注释「新增注入类型时在此追加」；
2. **同时**过滤 `role == "developer"`（系统角色，实测 5 条误上卡）——这是结构化判据，更稳；
3. 若 Task 2 探测确认 `session_meta` 带 `origin` 类字段，优先升级为结构化判据。

- [ ] **Step 1: 写失败测试**

```rust
    /// X-P1 回归锁：注入 user 条目不上状态、不上预览（2026-09-15）
    #[test]
    fn codex_injected_user_entries_skipped() {
        assert!(codex_is_injected_user("<turn_aborted>\nUser interrupted"));
        assert!(codex_is_injected_user("<environment_context>\n<cwd>/x</cwd>"));
        assert!(codex_is_injected_user("# AGENTS.md instructions for /repo"));
        assert!(codex_is_injected_user("<subagent_notification>\n<task-id>x</task-id>"));
        assert!(!codex_is_injected_user("帮我看一下这个 bug"));
        assert!(!codex_is_injected_user("继续"));
    }

    /// developer 角色不上预览（结构化判据）
    #[test]
    fn codex_developer_role_not_in_preview() {
        // fixture rollout 尾部为 developer 消息 → last_message 不得取其内容
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd src-tauri && cargo test monitor::codex_parser::tests::codex_injected_user_entries_skipped 2>&1 | tail -15
```

- [ ] **Step 3: 实现**

1. 追加判定函数：

```rust
/// Codex 注入 user 条目识别（X-P1）：内容前缀白名单。
/// 实机语料（383 rollout）中 37 条注入条目导致假黄、45 条污染预览。
///
/// **已知局限**：Codex 不写结构化来源标记，只能按内容识别；工具新增注入类型
/// 会漏判（表现为假黄 + 脏预览，不 panic、不影响正确性）。新增类型时在此追加，
/// 并在 Task 2 的探测报告中记录版本。
fn codex_is_injected_user(text: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "<turn_aborted>",
        "<environment_context>",
        "<recommended_plugins>",
        "<codex_internal_context",
        "<subagent_notification>",
        "<skill>",
        "# AGENTS.md instructions for",
    ];
    let t = text.trim_start();
    PREFIXES.iter().any(|p| t.starts_with(p))
}
```

2. 在 `read_codex_digest` 的 response_item 分支（约 `:332-365`）：跳过注入项与 developer 角色，且**不进 `last_message`**。

3. `is_rollout_file` / 会话匹配（`:23-28`、`:220-263`）排除子代理 rollout：

```rust
/// 子代理 rollout 排除（X-P1'）：session_meta.source 含 subagent 键的 rollout
/// 是子代理执行记录（实测 67/383），不得作为会话卡候选（会与父 rollout 按
/// mtime 抢占同一 CLI 进程的卡片）。首次读取时判定并缓存——session_meta
/// 只在文件首行，代价 O(1) 读一行
fn is_subagent_rollout(path: &Path) -> bool { /* 读首行，解析 source.subagent */ }
```

> **接入位置**：`phase1_match` 的候选筛选处过滤。若 Task 2 探测显示父子不竞争，仍应过滤（语义正确性），但可降优先级。

- [ ] **Step 4: 跑测试确认通过**

```bash
cd src-tauri && cargo test monitor::codex_parser 2>&1 | tail -25
```

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/monitor/codex_parser.rs
git commit -m "fix(codex): 注入条目跳过 + 子代理 rollout 排除（X-P1/X-P1'）"
```

---

### Task 7: Codex APP——绿态仲裁接入

**Files:**
- Modify: `src-tauri/src/monitor/codex_thread_parser.rs:309-345`
- Test: 同文件 `mod tests`

**现状**：`codex_thread_parser` **已有** descendants 机制（`load_descendant_activity` + `overlay_stale_with_descendants`），但只覆盖两个分支：无投影 fallback、Processing 停更仲裁。**尾部 `task_complete` 判 Idle 时子代理 Active 完全不阻止转绿**（`app_status.rs:64-70` 的非 Processing 透传）。

- [ ] **Step 1: 写失败测试**

```rust
    /// P1 回归锁：尾部 task_complete 判绿 + 子线程活跃 ⇒ 不判绿（2026-09-15）
    #[test]
    fn green_blocked_by_active_child_thread() {
        // fixture：父线程尾部 task_complete（→ Idle）+ thread_spawn_edges 指向
        // 一个 30s 内更新的子线程
        // 断言 status == Waiting
    }

    /// 子线程停更 → 正常转绿（不误伤）
    #[test]
    fn green_allowed_when_child_stale() {
        // 子线程 updated_at 超过窗口 → 断言 status == Idle
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd src-tauri && cargo test monitor::codex_thread_parser::tests::green_blocked_by_active_child_thread 2>&1 | tail -15
```

- [ ] **Step 3: 实现**

在 `build_one` 的 `overlay_stale_with_descendants(...)` 调用（`:341`）之后追加：

```rust
    // P1：完成仲裁——子线程活跃时不报完成（既有 overlay 只覆盖 Processing 分支）
    if matches!(activity, DescendantActivity::Active) {
        status = overlay_completion_with_pending(status, DescendantActivity::Pending);
    }
```

> **语义说明**：此处把 `Active` 映射为 `Pending`（Codex APP 无"派发/结算"配对数据，只能用线程活跃度近似）。该映射**仅在本文件成立**——因为 Codex 的子线程活跃窗口（30s）已由 `thread_spawn_edges` 精确界定，不存在 ZCode 那种"长思考被判 Stale"的问题。执行者在注释中写明该差异。
>
> 另：`thread_spawn_edges.status`（`open`/`closed`）未被读取，若能读到 `open` 边缘则更权威——Task 2 探测确认字段可用性后决定是否升级。

- [ ] **Step 4: 跑测试确认通过**

```bash
cd src-tauri && cargo test monitor::codex_thread_parser 2>&1 | tail -25
```

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/monitor/codex_thread_parser.rs
git commit -m "fix(codex-app): 子线程活跃阻止误判完成（P1）"
```

---

### Task 8: dsh——父子关联与绿态仲裁（D-P1，实机证据最严重）

**Files:**
- Modify: `src-tauri/src/monitor/dsh/mod.rs`（扫描阶段收集父子关联 + 接入仲裁）
- Modify: `src-tauri/src/monitor/dsh/status.rs`（如需在 `TurnFacts` 中补字段）
- Modify: `src-tauri/src/monitor/dsh/log.rs`（如需暴露 header 时间戳）
- Test: 各文件 `mod tests`

**证据**：`session-946b624e` 的 `turn/end completed` 落在 3 个活跃子代理窗口内，**绿灯持续 777 秒**；另一例 ~524 秒。`DshHeader.parent_session` 已解析但**从未被使用**（全仓库 grep 确认）。

**设计**：两阶段扫描——先收集全部 digest（含 `parent_session` 与 `log_mtime`），再对父会话做仲裁。**注意 L3 窗口**：子代理日志可能超窗，但父会话的仲裁只需"子代理是否活跃"（30s 窗口），超窗子代理必然不活跃 → 无需 `fill_uncached` 回退（与 AGENTS.md 的 dsh 豁免裁决一致）。

- [ ] **Step 1: 写失败测试**

```rust
    /// D-P1 回归锁：父会话 turn/end completed + 子代理日志活跃 ⇒ 不判绿（2026-09-15）
    #[test]
    fn dsh_child_activity_blocks_green() {
        // fixture：父会话 turn/start + turn/end(completed)（→ Finished）
        //        + 子代理日志（origin=subagent, parent_session=父id, mtime=now-10s）
        // 断言父会话 status == Waiting（而非 Finished）
    }

    /// 子代理停更 → 正常转绿
    #[test]
    fn dsh_green_allowed_when_child_stale() { /* 子代理 mtime = now-600s → Finished */ }

    /// parent_session 缺失（旧格式）→ 行为不变
    #[test]
    fn dsh_missing_parent_session_is_noop() { /* 无关联 → 不产生 Pending */ }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd src-tauri && cargo test monitor::dsh::tests::dsh_child_activity_blocks_green 2>&1 | tail -15
```

- [ ] **Step 3: 实现**

1. `mod.rs` 扫描阶段：在子代理 `continue`（`:242-244`）**之前**，把 `(parent_session_id, log_mtime_ms)` 计入一个 `HashMap<String, i64>`（父 id → 子代理最新写入 ms）。**当前 `is_subagent` 分支是直接 `continue` 丢弃**，改为记录后再丢弃。

2. 父会话构建时仲裁：

```rust
    // D-P1：完成仲裁——子代理日志活跃时父会话不得报完成
    // （实测 session-946b624e 绿灯持续 777s，期间 3 个后台子代理在跑；
    //  DshHeader.parent_session 一直解析但从未使用）
    if child_latest_ms
        .get(&header.id)
        .map(|&t| now_ms.saturating_sub(t) < DSH_SUBAGENT_ACTIVE_MS)
        .unwrap_or(false)
    {
        status = overlay_completion_with_pending(status, DescendantActivity::Pending);
    }
```

> `DSH_SUBAGENT_ACTIVE_MS` = 30_000（与既有口径一致）。**扫描预算**：`log_mtime_ms` 已在 digest 中算好（`:76-82`），父子关联用内存 HashMap 聚合，**零新增文件 IO**。

3. **兜底方案**（若 Task 2 探测显示 `parent_session` 不可靠）：改用主会话内 `subagent-settled` 事件——派发计数 vs settled 计数（`session-946b624e` seq 21383 有该事件）。这需要 `status::scan_facts` 增补计数字段。

- [ ] **Step 4: 跑测试确认通过**

```bash
cd src-tauri && cargo test monitor::dsh 2>&1 | tail -25
```

实机核对：`session-946b624e` / `session-3364bbda` 两例的绿灯时长应从 777s / 524s 降为 0（或仅剩子代理全部结束后的真绿）。

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/monitor/dsh/
git commit -m "fix(dsh): 父会话完成信号被子代理活跃阻止（D-P1，实机 777s 误报）"
```

---

### Task 9: Kimi——注入识别（K-P2）

**Files:**
- Modify: `src-tauri/src/monitor/kimi_parser.rs`（`KimiWireEntry` 增补 `origin` 字段 + `` 跳过）
- Test: 同文件 `mod tests`

**判据（实机验证）**：`origin.kind` ∈ {`injection`, `skill_activation`, `background_task`, `system_trigger`} → Synthetic。实测 195 个时刻假黄。`KimiWireEntry`（`:129-140`）当前**不反序列化 `origin`**，字段完全不可见。

- [ ] **Step 1: 写失败测试**

```rust
    /// K-P2 回归锁：注入条目不上状态、不上预览（2026-09-15）
    #[test]
    fn kimi_injected_entries_skipped() {
        // origin.kind=injection 且内容 <system-reminder>，role=user
        // origin.kind=background_task 且内容 <notification id="task:...">
        // 断言二者均不产生 Thinking、不上 last_message
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd src-tauri && cargo test monitor::kimi_parser::tests::kimi_injected_entries_skipped 2>&1 | tail -15
```

- [ ] **Step 3: 实现**

1. `KimiWireEntry` / `KimiMessage` 增补 `origin: Option<serde_json::Value>`。
2. `` 与 `entry_status` 中，user 条目若 `origin.kind` ∈ 上述四值 → 跳过。
3. **保留** 既有 `usage.record` 的处理（`:464-465` 有意不映射，属已知旧版本限制，本次不动）。

- [ ] **Step 4: 跑测试确认通过**

```bash
cd src-tauri && cargo test monitor::kimi_parser 2>&1 | tail -25
```

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/monitor/kimi_parser.rs
git commit -m "fix(kimi): 注入条目跳过（K-P2，195 时刻假黄）"
```

---

### Task 10: WorkBuddy + OpenCode——注入识别与截断（W-P2 / O-P2）

**Files:**
- Modify: `src-tauri/src/monitor/workbuddy_parser.rs`（`workbuddy_entry_kind` + `extract_message_text`）
- Modify: `src-tauri/src/monitor/opencode_parser.rs`（`get_last_message_info` 前置过滤）
- Test: 各文件 `mod tests`

**判据（实机验证）**：
- WorkBuddy：内容以 `<system-reminder` 开头 → Synthetic；**且** `extract_message_text` 加 100 字符截断（与 Codex/ZCode 口径统一——实测注入消息最大 14322 字符，无截断会撑爆卡片布局）
- OpenCode：user 条目内容以 `## New Messages` 开头 → Synthetic

- [ ] **Step 1: 写失败测试**

```rust
    /// W-P2 回归锁：system-reminder 注入跳过 + 预览截断（2026-09-15）
    #[test]
    fn workbuddy_injection_skipped_and_truncated() {
        assert!(workbuddy_is_injected(r#"<system-reminder data-role="user-context">…"#));
        assert!(!workbuddy_is_injected("帮我改一下这个文件"));
        // 截断：14322 字符输入 → 输出 ≤ 103 字符（100 + "..."）
    }

    /// O-P2 回归锁：跨 agent 合成消息跳过（2026-09-15）
    #[test]
    fn opencode_cross_agent_message_skipped() { /* "## New Messages" → Synthetic */ }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd src-tauri && cargo test monitor::workbuddy_parser::tests::workbuddy_injection_skipped 2>&1 | tail -15
```

- [ ] **Step 3: 实现**

1. WorkBuddy：`workbuddy_entry_kind` 的 user 分支加注入判定；`extract_message_text` 加 100 字符截断（复用 `zcode_parser` 的 `MESSAGE_TRUNC` 常量口径，定义本地常量并注释「与 Codex/ZCode 100 字符口径一致」）。
2. OpenCode：`get_last_message_info` 取最新消息后，若判定为合成则**继续向前找**（而非直接返回）。
3. `compensate_vanished_heartbeats_in`（`:473-563`）走同一份 `extract_message_text` → 自动受益，核对即可。

- [ ] **Step 4: 跑测试确认通过**

```bash
cd src-tauri && cargo test monitor::workbuddy_parser monitor::opencode_parser 2>&1 | tail -25
```

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/monitor/workbuddy_parser.rs src-tauri/src/monitor/opencode_parser.rs
git commit -m "fix(workbuddy,opencode): 注入条目跳过与预览截断（W-P2/O-P2）"
```

---

### Task 11: 跨工具回归与端到端验证

**Files:**
- Modify: `src-tauri/src/adapter/mod.rs`（如需要，加跨工具防回归测试）
- Test: 全量 `cargo test`

- [ ] **Step 1: 写跨工具防回归测试**

在 `src-tauri/src/monitor/app_status.rs` 或 `adapter/mod.rs` 追加：

```rust
    /// 全工具共享语义锁：非 Pending 后代态下，绿态判定与修复前逐字节一致
    /// （防止 P1 修复意外改变既有工具行为）
    #[test]
    fn p1_fix_is_opt_in_per_tool() {
        // 对 8 个工具的形态值遍历，凡未接入 Pending 的（传 Absent）
        // 断言 overlay_completion_with_pending 不改变状态
    }
```

- [ ] **Step 2: 全量测试 + clippy + fmt**

```bash
cd src-tauri && cargo test 2>&1 | tail -20
cd src-tauri && cargo clippy --all-targets -- -D warnings 2>&1 | tail -10
cd src-tauri && cargo fmt --check
cd .. && pnpm check   # 前端门禁（本方案不动前端，应零变化）
```
Expected: 全绿，零告警，格式 CLEAN。

- [ ] **Step 3: 端到端实机验收**

按以下清单逐项核对（**这是本方案唯一能证明修复有效的手段**）：

| 验收项 | 方法 | 通过标准 |
|---|---|---|
| Z-P1 假绿消失 | 用修复后的解析器复刻脚本对 `sess_5a57eda4` 回放 21:04–21:47 | 转绿次数从 4 次降为 **1 次**（仅 21:47:49 真绿） |
| Z-P1 不误伤 | 对 `sess_6e7b9e58`（2 子代理，无假绿）回放 | 转绿时机不变 |
| Z-P2 脏预览消失 | 检查通知历史新增条目 | 不再出现 `<task-notification>` 原文 |
| C-P1 | 对 `75ef873a` / `00cf35ac` / `e2a68ed6` 回放 | 子代理窗口内不再判绿 |
| C-P2 | 统计 29 个"永久黄"会话 | 修复后可正常转绿 |
| D-P1 | `session-946b624e` / `session-3364bbda` | 绿灯时长 777s → ≤30s |
| 提示音 | 实机跑一次"派 2 个子代理"的任务 | 仅全部返回后响一次 |
| 未读卡 | 同上，观察看板 | 子代理在跑时父卡保持黄，全部返回后转绿并入未读池 |

- [ ] **Step 4: 更新文档**

1. `src-tauri/src/monitor/app_status.rs` 模块头补「P1 完成仲裁」说明。
2. AGENTS.md 的「会话扫描预算契约」段补一句：新增数据源读取必须声明预算归属（本方案已在各 Task 注明，此处补通用要求）。
3. 在 `docs/superpowers/plans/2026-09-15-subagent-completion-signal-fix.md` 末尾追加「执行结果」节，记录实际验收数据。

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "test(monitor): 跨工具回归锁 + 端到端验收记录"
```

---

## 附录 A · 实机探测证据（2026-09-14）

调查基线：ZCode `~/.zcode/cli/db/db.sqlite`、Claude `~/.claude/projects`、Codex `~/.codex/sessions`、dsh `~/.dsh/sessions`、MAM `~/.mam/mam.db` + WebKit localStorage。

### A.1 ZCode 假绿量化

```
全库 interactive 父会话（有子代理）：30 个
累计转绿（Idle）次数：392
其中「转绿后 60s 内仍有子代理在写库」：256  (65%)

按「后台通知后收尾」口径：
后台通知后收尾的回合：97 次
其中收尾时刻之后仍有子代理在写库：42 次  (43%)
```

### A.2 `sess_5a57eda4` 逐时刻回放（3 个并行评审子代理）

```
21:04:10  派发 3 个后台 Agent（run_in_background=true，part 立即 completed）
21:04:58  step-finish(stop) ⇒ Idle(绿)  ⚠️ 3 个子代理全在跑
21:13:54  子代理 3c51aafe 最后写入
21:13:55  background_notification（"评审前端与计划对齐 completed"）
21:14:10  step-finish(stop) ⇒ Idle(绿)  ⚠️ 2 个子代理仍在跑
21:14:24  子代理 21d86fa7 返回
21:14:35  step-finish(stop) ⇒ Idle(绿)  ⚠️ 1 个子代理仍在跑
21:46:41  子代理 c9144d7b（最长任务）最后写入
21:47:49  step-finish(stop) ⇒ Idle(绿)  ✅ 真绿
```

MAM 通知历史对应条目（`~/Library/WebKit/com.jarvis.multiagents-manager/.../localstorage.sqlite3`）：

```
09-14 21:14:15 | zcode | idle       | sess_5a57eda4 | 评审进度：3 路中 1 路已完成
09-14 21:14:25 | zcode | processing | sess_5a57eda4 | <task-notification> <task-id>agent_21d86fa7...
09-14 22:01:32 | zcode | processing | sess_5a57eda4 | <task-notification> <task-id>exec_34fdc7f2...
```

### A.3 判据选型验证（ZCode）

```
合成（synthetic=1）user 消息构成：
  todo_reminder            815   ← 已在既有白名单
  background_notification  157   ← 漏（子代理/后台 bash 返回）
  system_reminder           72   ← 漏
  subagent_notification      7   ← 漏
  合计                    1051

判据命中/误伤对比：
  A. synthetic==1                      命中 1046，误伤 user_prompt 0
  B. uiVisibility=="hidden"            命中 1060，误伤 user_prompt 0
  C. kind != "user_prompt"             命中 1060，误伤 user_prompt 0
  （A∪B 即 Task 3 采用口径：覆盖全部注入，零误伤）
```

### A.4 派发↔结算配对（ZCode）

```
run_in_background=true 派发：111 条（Agent 21 + Bash 90）
  workId 可提取：111/111（output 含 "agentId: agent_<uuid>" 或 "ID: exec_<uuid>"）
结算（background_notification.originMeta.workId）：155 条
闭环：110/111 已结算（99%）
唯一未结算：exec_98103e55...（派发于 28882 分钟前，僵尸）→ 由超龄保护处理
通知来源分布：{bash: 121, subagent: 35}
```

### A.5 ZCode 权威信号（备选数据源，Task 3/4 未采用的原因）

`~/.zcode/cli/agents/<parent>/agent_<uuid>/metadata.json`：

```
status 取值分布（383 文件）：completed 372 / failed 10 / running 1
status=running 时 completedAt=None，且 mtime 随运行更新
交叉验证：3 个 agent 的 completedAt 与 DB 侧 session.time_updated 差 ≤1s
```

**未采用原因**：需逐 agent 目录 stat（383 目录 → 每轮 IO 放大），且与 DB 信号（run_in_background 配对）信息等价，而后者**零新增查询**。保留为未来备选：若 DB 侧格式变化导致配对失效，可回退到该文件信号（需按 `SUBAGENT_ACTIVE_WINDOW_MS` 加 L2 缓存）。

### A.6 Claude 证据

```
主 transcript 文件数：59
  isMeta: 376
  origin.kind=human: 129
  origin.kind=task-notification: 18
子代理文件：23 个，布局 <project>/<sessionId>/subagents/agent-*.jsonl（4 层）
父会话尾部为合成条目（永久黄）：29/59（24 × "## Context Usage..." isMeta，5 × <local-command-stdout>）
假绿窗口：3 个会话、24 个窗口，最长 15.7 分钟 / 372 次子代理写入
```

### A.7 Codex / dsh / 其他

```
Codex CLI：383 rollout
  注入 user 条目 → 假黄 37 个、脏预览 45 个
  role=developer 上预览：5 个
  子代理 rollout（source.subagent）：67 个
Codex APP（thread_history_1.sqlite）：59 userMessage 中未发现合成内容（需探测确认）
dsh：84 日志 / 约 280k 事件
  session-946b624e：turn/end completed 后绿灯 777s（3 个子代理在跑）
  session-3364bbda：绿灯 ~524s
  user/message source.kind 分布：user 345 / plugin 165 / skill-catalog 71 /
    agent-instructions 68 / subagent-settled 27 / goal 13 / skill-invocation 12 /
    subagent-report 5 / coordinator 3
  preview.rs:49 的 source.kind=="user" 白名单 = 唯一正确范式
Kimi：35 main wire
  origin.kind: injection 143 / skill_activation 35 / background_task 12 / system_trigger 5
  195 个模拟时刻假黄 + 脏预览
WorkBuddy：注入 <system-reminder data-role="user-context">，最大 14322 字符
OpenCode：5 条 "## New Messages" 合成消息（窗口 91ms）
```

### A.8 未完成的探测项（Task 2 负责）

1. OpenClaw 假绿率（`cpu_usage > 5.0` 判据在后台等待时是否误判）
2. Codex 子代理 rollout 是否实际与父竞争同一进程卡
3. Kimi「v1.5 + swarm」组合是否存在假绿（当前语料 0 例）
4. WorkBuddy / OpenCode 注入消息落尾的真实时长占比
5. dsh `parent_session` 字段的填充可靠性

---

## 附录 B · 修复范围速查

| 工具 | P1 假绿 | P2 脏预览 | 本方案 Task |
|---|---|---|---|
| ZCode | ✅ 65% 转绿误报 | ✅ 1051 条注入 | Task 3、4 |
| Claude | ✅ 24 窗口 | ✅ 29/59 会话永久黄 | Task 5 |
| Codex CLI | 待探测（子代理抢占） | ✅ 37+45 条 | Task 6 |
| Codex APP | ⚠️ 结构缺口（未实测） | 当前样本干净 | Task 7 |
| dsh | ✅ **777s 实机证据** | ✅ 无缺陷（范式来源） | Task 8 |
| Kimi | 待探测（v1.5+swarm） | ✅ 195 时刻 | Task 9 |
| WorkBuddy | 无子代理概念 | ✅ 14322 字符注入 | Task 10 |
| OpenCode | 无子代理概念 | ✅ 5 条（窗口短） | Task 10 |
| OpenClaw | 待探测（CPU 判据） | ⚪ 不读 transcript | Task 2 探测 |

---

## 2026-10-18 适配追记

> 背景：本计划制定于 2026-09-15（基准为当时 main）。2026-10-18 用户裁定按本计划推进 #61（tracking 批次，见 `docs/superpowers/specs/2026-10-18-tracking-batches-design.md` §4），实施前对齐 origin/main（d14aa8c）现状，落四条适配 + 一条推迟裁定 + 阶段顺序确认。**本追记不改动上方计划原文**；与原文冲突处以本追记为准（均为「计划写 X / main 现状是 Y」的对齐，不改变任何验收语义）。

### 适配① Task 4 测试脚手架——按 main 真实签名对齐

计划中的 zcode 测试脚手架名与 main 现状（`src-tauri/src/monitor/zcode_parser.rs` tests 模块）不符，实现时按下表替换：

| 计划写法 | main 现状（行号基于 d14aa8c） |
|---|---|
| `setup_schema` | 不存在；用 `build_cli_db(path: &Path) -> Connection`（zcode_parser.rs:784） |
| `insert_session` 6 形参 | 实为 **7 形参**（:821：`conn, id, parent_id: Option<&str>, dir, task_type, title: Option<&str>, time_updated: i64`） |
| `insert_part` 带时间参 | 实为 **5 形参、无时间参**（:854：`conn, id, message_id, seq, data`）；超龄场景把时间写进 part JSON 的 `state.time.start`（按计划原意图落 part data，不改夹具签名） |

测试基数修正：`app_status.rs` 现有 **26** 个 `#[test]`（计划写 18）、`zcode_parser.rs` 现有 **35** 个（计划写 20+）。计划 Step 若按「新增后总数」断言，按实际数修正基线。

### 适配② Task 10（opencode）——按双 schema 重设计

`src-tauri/src/monitor/opencode_parser.rs` 已从计划制定时的 523 行增长到 2495 行，单文件承载 v1/v2 双 schema：

- `schema_is_v2`（:87）双 schema 分派是判据单点；消息读取 v1 走 `message`+`part`、v2 走 `session_message` 内联 `content[]`；
- `get_last_message_info`（:430）仅覆盖 v1 路径；v2 路径是 `get_last_message_info_v2`（:544）；
- 计划制定后新增的 TailSignal / 问答卡层：`get_session_tail_part`（:503）、`tail_signal_v2`（:728）、`pending_question_part`（:876）、`determine_opencode_status`（:992）；
- 计划引用的「## New Messages」判据在 main 全文件**零命中**——v2 库形态必须在实现时以夹具定案，**不得照抄 1.x 判据**。

**Task 10 需按双 schema 重写**：注入识别（O-P2）与截断判据的取证、夹具、断言均分 v1 / v2 两条路径分别落（v1 夹具照 `get_last_message_info` 既有测试形态，v2 照 `get_last_message_info_v2` / `tail_signal_v2` 既有测试形态）。

### 适配③ 各 parser Task 的 remote/content.rs 同步点（映射表）

`src-tauri/src/remote/content.rs`（4103 行）内有与各 parser 判据同口径的**复制品**。凡本计划改动下列 parser 判据（message_origin / 注入识别口径）的 Task，**必须同批同步 content.rs 对应点**，否则详情页与看板口径漂移：

| Task | parser 改动 | content.rs 同步点（d14aa8c 行号） |
|---|---|---|
| Task 3（zcode 注入识别） | `zcode_parser::flatten_entries` kind 白名单 | :536-550（user + todo_reminder/timeline_event 两 kind 跳过复制品，与 flatten_entries 同口径） |
| Task 5（claude isMeta / 注入识别） | `claude_parser` | :950-958（isMeta 行跳过） |
| Task 6（codex developer / 记账跳过） | `codex_parser` | :1210-1239（developer / 记账行跳过） |
| Task 8（dsh 注入过滤） | dsh 模块 | :805-821、:922（source.kind=="user" 过滤，agent-instructions / plugin / skill-catalog 注入） |
| Task 9（kimi skill 注入） | `kimi_parser` | :1527-1571（is_kimi_skill_injection：`<skill-loaded` 等标记跳过） |

每个 Task 的「Files」清单执行时追加对应 content.rs 同步点 + 同口径测试。

### 适配④ 并入项现状——teammate 完成信号已独立落地

teammate 完成信号（甲.5 主判据）已随移动端子代理观察台独立落地：`src-tauri/src/monitor/subagents/claude.rs:383 teammate_idle_from`（`idle_notification` 标记 + `from` 提取，teammate_id 属性辅助）。该实现与本计划 Task 5 的注入识别 / 子代理活跃度链路**互补不冲突**：Task 5 执行时**不再重做** teammate 完成信号，只做计划内的 isMeta / 注入识别部分。

### Task 2 推迟裁定（2026-10-18 用户裁定）

Task 2（实机探测 5 项：WorkBuddy/OpenCode 注入落尾占比、dsh `parent_session` 可靠性、Codex 子代理抢占、Kimi v1.5+swarm、OpenClaw CPU 判据）**本轮推迟**。各 Task 判据沿用计划内既有实测证据（附录 A）。纪律：实现中若发现判据与 main 现状失配（形态漂移 / 字段改名 / 布局变更），**停下报告**，由用户裁决补探或改判据——不猜、不抄旧档。

**补探清单追加（2026-10-18 阶段 A 评审 Nit2/Nit3）**：① `subagent_notification`（审计 7 条）是否携带 `metadata.originMeta.workId` 且语义确为「完成」——结算侧判据现无 kind 门，若其 workId 语义非完成会提前核销（假绿回归）；② 派发 part 的 `state.time.start` 是否随版本系统性缺失——`unwrap_or(now)` 缺失按新鲜处理，会让 24h 僵尸保护静默失效（未结算派发可长期劫持状态直至核销或滑出卡窗）。

### 其他现状对齐

- `codex_parser.rs` 已部分预占 Task 6（`<turn_aborted>` 判 TurnEnd、developer→Other 已落地）；剩余未做：environment_context 等 6 个前缀白名单 + 子代理 rollout 排除。
- `app_status.rs` 已被 PR #70 加过 `tail_semantic_kind` / `turn_window_open`；Task 1 给 `DescendantActivity` 加 `Pending` 成员时，不得破坏既有 26 个测试。

### 阶段执行顺序确认

**A（Task 1-4）→ B（Task 5-7）→ C（Task 8-9）→ D（Task 10-11）**，每阶段独立 PR、可独立合并；后续阶段依赖 Task 1 共享层接口。与 2026-10-18 tracking 三批（PR-1/2/3）存在 `monitor/` 与 `remote/content.rs` 弱文件交集，建议三批先合、阶段 A 随后 rebase 起步。
