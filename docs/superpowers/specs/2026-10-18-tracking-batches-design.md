# 2026-10-18 tracking 批次整合设计（#150 技术债 / #151 功能修复 / #152 新特性 · 已裁决项）

> 状态：设计定案（待实现）｜基准：origin/main = d14aa8c｜行号均基于该基准。
> 本文档只做设计，不含完整实现代码；关键形态以函数签名 / 插入点行号给出。

---

## 0. 引言

### 0.1 范围与裁决基准

三个 GitHub tracking issues（#150 / #151 / #152），用户裁定本轮「**除实机检测项外全部设计修复**」。本文档覆盖四个已裁决项 + #61 大项适配追记：

| 批次 | 项 | 一句话裁决 |
|---|---|---|
| 第 1 批（一个 PR） | #66 SubAgentPresetActions 死 UI | 保留入口 + 空态提示 |
| 第 1 批 | #134 看板父卡徽标恒 0 | 数据源改用与子代理面板同源的 `monitor::subagents` 重算 |
| 第 1 批 | 子代理回报提醒开关全工具化 | 现有 subagentReport 开关（仅 Claude）扩展到 claude/opencode/kimi/codex 四家 |
| 第 2 批（一个 PR） | #80 codex 问答取消钮 | 放开 Esc 中断（破坏性语义），按钮保留改文案「中断回合（破坏性）」，中英双语 |
| 第 3 批（一个 PR） | #69 文件池 codex 首推 + 降级结论落档 | codex 新增 event_msg/local_images 抽取源；claude/opencode 不入池结论落档 |
| 独立分支、按阶段 PR | #61 子代理完成信号误报修复 | 按既有 TDD 计划推进，先落适配追记（见 plans 文件末尾） |

### 0.2 排除项（本轮不做）

- #74（macOS 红卡实机验收）、#78（kimi 多选探测）；
- #80 的**实机验证步骤**（Esc 在 codex 问答面板的实际语义留给用户后续复核，见 §2.6）；
- #115 / #114（blocked on PR #117）；
- #61 计划的 Task 2 实机探测（推迟，各 Task 判据沿用计划内既有证据，见 plans 追记）；
- #69 中 workbuddy / openclaw / dsh 的文件池调研（issue 内保持待办）。

### 0.3 全局纪律（每批通用）

- 门禁：`scripts/ci-backend-local.sh`（CI backend 7 步同口径）、`scripts/check-i18n.mjs`（zh/en 双侧齐键）、`pnpm check`。
- TDD：每项先写测试；验收判据已定死的直接照写。
- UI 文案一律 i18n 键，不硬编码；注释 / 文档 / commit 中文，标识符英文。
- 会话扫描预算契约（L1/L2/L3，AGENTS.md）：凡触轮询热路径的改动逐项做预算评估（见各节）。
- 本四项不新增任何错误码（无新 Err 路径），不涉及 `coded!` / errcode 机制。

---

## 1. 第 1 批（PR-1）：#66 + #134 + 提醒开关全工具化

### 1.1 #66 SubAgentPresetActions 死 UI → 保留入口 + 空态提示

#### 现状与根因

`src/components/presets/PresetList.tsx` 组件 `SubAgentPresetActions`（:441-543）自锁死循环：

- `const [subAgents, setSubAgents] = useState<string[]>([])` 初始空；
- `:495 if (subAgents.length === 0) return null` 位于展开按钮（:497-500，`onClick` 里才调 `loadSubAgents`）**之前**；
- 结果：从未加载 → 永远空 → 永不渲染 → 永不加载。功能整体死亡。

渲染入口 `:421-434`：`scope=tool && boundTool` 挂单个实例，否则按 `enabledTools` 每工具一个实例——入口本身健在，死的是组件内部。

后端 `detect_subagents` 链路：`src/lib/api/settings.ts:11-12` → `commands/settings.rs:32-34` → `services/mod.rs:83-103`。口径：未知工具 / `subagent_dir()=None` / 目录不存在 / `read_dir` 失败 → 均返回 `Vec::new()`（空数组，**非错误**）；目录存在则全收 `file_stem`（无过滤）。**空数组无法区分「无子代理」与「工具不支持」**——kimi（swarm 会话内，`subagent_dir()=None`）、workbuddy / zcode / dsh（默认 None）与「装了 claude 但还没 spawn 过子代理」呈现完全相同。

#### 改法定案

**① 后端契约扩展（区分「不支持」与「无」）**

```rust
// src-tauri/src/services/mod.rs（替换 :81-103 既有 detect_subagents 实现体）
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentDetection {
    pub supported: bool,      // adapter.subagent_dir().is_some()
    pub agents: Vec<String>,  // 既有口径不变：目录存在则全收 file_stem
}
pub fn detect_subagents(tool_id: &str) -> SubagentDetection
```

- `commands/settings.rs:32-34` 命令签名随改；`src/lib/api/settings.ts:11-12` 返回类型改 `{ supported: boolean; agents: string[] }`。
- 判定口径定案表（ruling 要求的「复核」结论）：

| 场景 | supported | agents | 前端呈现 |
|---|---|---|---|
| 未知工具 id | false | [] | 不支持文案 |
| `subagent_dir()=None`（kimi / workbuddy / zcode / dsh） | false | [] | 不支持文案（复用既有键 `presets.presetSubagentUnsupported`，zh.json:1395） |
| 目录不存在（支持但从未 spawn） | true | [] | 空态文案（新键 `presets.presetSubagentEmpty`） |
| `read_dir` 失败 | true | [] | 空态文案（如实：目录在但读不了，与「无」同呈现） |
| 目录存在有子代理 | true | [...] | 既有列表行为不变 |

- **权衡**：改 IPC 契约 vs 前端硬编码支持工具名单。选前者——前端名单是后端知识复制，`subagent_dir()` 口径一变即漂移；本命令唯一消费方就是该组件（全仓 grep 确认），单体桌面应用前后端同构建同发，无兼容负担。ruling 明确要求「复核目录不存在返回空数组时的表现」，契约可区分正是复核的产出。

**② 前端组件复活（`src/components/presets/PresetList.tsx`）**

- 状态改三态：`useState<string[] | null>(null)`（`null` = 未加载）+ 既有 `expanded`；
- 删除 `:495 if (subAgents.length === 0) return null` —— 入口**恒渲染**折叠态文字按钮（形态沿用本组件自身 expanded 按钮 + 既有键 `presets.expandSubAgents` / `collapseSubAgents`，zh.json:728-729；无 radix Collapsible，不引入）；
- 展开时 `loadSubAgents()`；结果分诊：`supported=false` → `presets.presetSubagentUnsupported`；`agents.length===0` → `presets.presetSubagentEmpty`；非空 → 既有 per-agent 应用/停用按钮行为不变（:505-543）；
- **整段包一层 `onClick={(e) => e.stopPropagation()}` 容器**（照 :394-396 开关区先例）——修复后入口恒在，必须防误触整卡点击（onEdit 编辑弹窗）；这是修复引入的新风险点，不是可选项；
- `catch` 分支：`console.error` + 置空列表显示空态文案（后端永不 Err，仅 IPC 层故障可达；如实呈现为空态）。

**③ i18n 新键（zh/en 双侧，过 check-i18n 门禁）**

- `presets.presetSubagentEmpty`：zh「未检测到子 Agent」/ en「No sub-agents detected」；
- `presets.presetSubagentUnsupported` 既有键复用，不新增。

**④ tauri-mock.ts**

- `src/tauri-mock.ts` 现无 `detect_subagents` 处理器（组件死 UI 时从不需要）。修复后浏览器 / Playwright 渲染会真实 invoke → 新增 mock 处理器返回固定 `{ supported: true, agents: [] }`。

#### 涉及文件

| 文件 | 改动 |
|---|---|
| `src-tauri/src/services/mod.rs`（:81-103） | 返回类型改 `SubagentDetection` |
| `src-tauri/src/commands/settings.rs`（:32-34） | 命令签名随改 |
| `src/lib/api/settings.ts`（:11-12） | 返回类型 |
| `src/components/presets/PresetList.tsx`（:441-543） | 三态 + 空态分诊 + stopPropagation |
| `src/i18n/locales/zh.json` / `en.json` | 新键 `presets.presetSubagentEmpty` |
| `src/tauri-mock.ts` | 新增 `detect_subagents` mock |

#### 测试要点（tests/presetList.test.tsx，现有 8 用例零覆盖本组件）

1. 无子代理工具：折叠入口渲染（死 UI 回归锁——现版本此断言必失败，TDD 红灯）；
2. 展开后显示 `presetSubagentEmpty` 文案；
3. `supported=false` 显示 `presetSubagentUnsupported` 文案；
4. 有子代理：展开列表 + 应用/停用按钮行为不变（既有链路回归锁）；
5. mock invoke 返回新契约形态（契约变更同步锁）。

#### 验收映射

- 「无子 Agent 的工具显示折叠态入口、展开显示『未检测到子 Agent』空态」→ 测试 1/2；
- 「有子 Agent 工具行为不变」→ 测试 4；
- 「复核 detect_subagents 判定口径」→ 判定口径定案表（上）+ `services` 单测按表逐行断言。

---

### 1.2 #134 看板父卡徽标恒 0 → monitor::subagents 同源重算

#### 现状与根因

- 赋值点 `src-tauri/src/monitor/claude_parser.rs:93-94`（轮询热路径，每轮每 claude 会话一次）调 `monitor/jsonl.rs:86-118 count_active_subagents`：只 `read_dir` **项目目录一层**找 `agent-*.jsonl`（旧布局），新布局子代理在 `<项目>/<sid>/subagents/` 下，扫不到 → 新布局会话恒 0。
- 徽标链：`Session.active_subagent_count`（model.rs:92；前端 `types/session.ts:32 activeSubagentCount`）→ `SessionCard.tsx:219-223` 仅 >0 渲染。
- **第二消费方**：`adapter/mod.rs:174-183` flap 打标谓词 `flap_from_subagent_activity = active_subagent_count>0 ∧ 最新 hook ∈ PostToolUse 族 ∧ <30s TTL`（既有测试 :1053/:1083/:1101 直接构造字段值，不依赖扫描）。
- chips 同源模块 `monitor/subagents/`：claude 判据 = meta.json 登记 + 父转写 Stop/Resume 事件状态机（**无 mtime 兜底**），全量名单（running+idle）出 `SubagentView{id,name,description,spawn_ts,tokens,status,end_ts}`；会话级 offset 增量缓存（registry 键 `("claude", session_id)`，`SUBAGENT_CACHE_CAP=1024`，`reset_cache_for_tests` 供测试隔离）。chips 前端只渲染 running。

#### 改法定案

**① 新增桌面热路径入口（免 locate 遍历）**

`monitor/subagents/claude.rs` 新增：

```rust
/// 桌面看板热路径入口：父转写路径已知（claude_parser 手里就是它），
/// 免 locate 的 projects 根遍历；布局推导与 locate 同一规则，等价性测试锁定。
/// 缓存键仍为 ("claude", session_id) —— 与 remote chips 端点共用同一增量缓存条目。
pub(crate) fn collect_at(parent_jsonl: &Path, session_id: &str) -> Vec<SubagentView>
```

- 实现：`project_dir = parent_jsonl.parent()`；`subagents_dir = project_dir.join(session_id).join("subagents")`；`!is_dir` → 空_vec 早退；命中则把 `collect_with`（:187 起）的 `update_entry` 闭包体提取为共享私有核复用（`locate` 仅做发现，判据 / 缓存 / 状态机零复制）。
- **权衡**：直接调 `collect_with(projects_root, sid)` 也可行（locate = read_dir projects 根 + 每项目一次 `is_dir` stat），但 `collect_at` 一次 stat 即止（无子代理会话——绝大多数——严格 cheaper），且 parser 手里已有精确路径，遍历是纯浪费。风险 = 布局规则第二处书写 → 用「`collect_at` 推导与 `locate(parent.parent(), sid)` 等价」测试钉死。

**② 赋值点换源**

`claude_parser.rs:93-94` 改为：

```rust
session.active_subagent_count = subagents::claude::collect_at(f, &session.id)
    .iter()
    .filter(|v| v.status == SubagentStatus::Running)
    .count() as u32; // 字段类型以 model.rs 既有定义为准
```

- **计数口径 = status==Running 过滤数**，与 chips 前端 running 过滤同口径——验收「与 chips 面板计数一致」的机制保证（同源同缓存同过滤）。

**③ 删除死代码**

`monitor/jsonl.rs:86-118 count_active_subagents` 移除（唯一调用方已换源，全仓 grep 确认）；`is_subagent_file` 保留（`get_recent_jsonl_files` 仍在用）。

**④ 范围界定（不扩面）**

- 只改 claude 赋值点。codex APP 线 `codex_thread_parser.rs:435` 既有 `descendants` 计数（sqlite 口径，:427/:435，测试 :884）**不动**；opencode / kimi / codex CLI 从未赋值（恒 0，不显示徽标）维持。全工具徽标化如有需求另立 issue。

#### 预算契约评估（热路径）

- 旧实现每轮：read_dir 项目目录 + 每个候选 `agent-*.jsonl` stat + open + 读 5 行找 session_id；
- 新实现：无子代理会话 = 1 次 `is_dir` stat 即止；有子代理会话 = subagents 目录 readdir（meta 登记）+ 父/子转写 **offset 增量读**（O(delta)，跨轮缓存，缓存物全为文件内容纯函数，无时间叠加——模块头既有纪律）；
- L1 不受影响（赋值点在进程匹配循环内，零进程不达）；不新增无界目录扫描（subagents 为会话界定目录）。**结论：严格优于旧实现。**

#### 旧布局口径（定案 + 申报）

`monitor::subagents` claude 的 locate 只认新布局（missing layout → 空，既有测试 `claude_source_missing_layout_is_empty`）。换源后**旧布局会话徽标从「旧实现偶发 >0」变为恒 0 不显示**。

定案口径：「旧布局回归不变」= **现有自动化测试与既有行为面不回归**（渲染条件、无子代理不显示、flap 谓词本身不动）。旧布局会话徽标归零视为该布局被 claude 废弃后的可接受收窄——旧布局下旧实现本身也几乎恒 0（30s mtime 窗极窄）。**如有真冲突**（用户仍在用旧布局 claude 且依赖徽标）：备选方案 = 给 `monitor::subagents` claude 源补旧布局扫描（扩面，chips 面板行为连带变化）——**留用户裁决，本轮不做**。已列入 §5 风险申报。

#### 连带效应（如实申报，不改谓词）

修复后新布局会话 `active_subagent_count` 恢复真实值 → `adapter/mod.rs:180` flap 谓词从「恒假（死门）」恢复为可触发 → F2b 静默开关（`silence_subagent_activity_flap`）恢复设计行为。这是修数的必然结果，不是扩面；既有三测试（:1053/:1083/:1101）构造字段值直接断言，不受影响。

#### 测试要点

1. `subagents/claude.rs`：`collect_at` 与 `locate` 等价性（同布局两路同出）；旧布局（无 subagents 目录）`collect_at` 空；
2. `claude_parser`：新布局夹具 + Running 子代理 → `active_subagent_count > 0` 且 == running 数；全 Idle → 0；
3. 回归：`adapter` 既有三测试照跑；`jsonl.rs` 移除函数后全量 `cargo test` 无悬挂引用；
4. （一致性）chips 端点与看板同缓存条目——同一夹具下 remote `collect` 与 `collect_at` 计数一致。

#### 验收映射

- 「新布局会话有活跃子代理时徽标 >0 且与 chips 面板计数一致」→ 测试 2/4（同源同缓存机制保证）；
- 「旧布局回归不变」→ 口径见上（测试 1 锁定新源对旧布局的既定空态）；
- 「无子代理时徽标不显示」→ Running=0 → 字段 0 → 渲染条件（:219-223）不满足。

---

### 1.3 子代理回报提醒开关全工具化（claude / opencode / kimi / codex）

#### 现状链路

- 开关：`src/pages/settings.tsx:253/:530-539`（Button 非 Switch，KV `notify_subagent_report`，"true"=提醒）→ 后端 KV（dao/settings.rs）。
- 消费：`src/hooks/useNotification.ts:205-220` 每次通知现读 KV（缺省=开）→ `if (!subagentReportEnabled.current && session.lastMessageSubagentReport) return;`（gate:subagentReportOff，整条静默含 addHistory）。**gate 本就按 Session 字段工作，与工具无关。**
- 打标：`Session.last_message_subagent_report`（model.rs:84）仅 claude——`claude_parser.rs:127` digest 字段 `last_is_subagent_report` → `:303` 落位；谓词 `subagents/claude.rs:399-401 is_subagent_report_text`（`task_id_in ∨ contains("<teammate-message")`）。其余三家硬编码 false：`codex_parser.rs:694`、`kimi_parser.rs:384`、`opencode_parser.rs:406`。

#### 设计难点定案：分层判据（claude 保留文本谓词，三家时间关联）

三家协议无 claude 同款文本信号原语。定案：**claude 原样不动；codex / kimi / opencode 用「回报时间关联」泛化打标**。

- **claude 不统一换掉的理由**：文本谓词是精确证据（「回报消息恰为末条」），已被测试钉住；换时间关联会改变 claude 的 ON 行为——扩面 + 回归风险，且 ruling 要求「与现 claude 行为同构」，同构的主体正是 claude 现机制。

**① 共享判据与常量（`monitor/subagents/mod.rs` 新增，纯函数 + now 不注入）**

```rust
/// 回报关联系数窗：与 ACTIVE_SECS=90 同值、独立常量（两语义可分别调参）
pub(crate) const SUBAGENT_REPORT_WINDOW_MS: i64 = 90_000;
/// 时钟/秒级取整容差
pub(crate) const SUBAGENT_REPORT_SKEW_MS: i64 = 2_000;
/// 锚 ≤ last+skew ∧ last−anchor ≤ window（回报发生在父末条之前不久）
pub(crate) fn report_anchor_near_last(anchor_ms: i64, last_msg_ms: i64) -> bool
/// rfc3339 → 毫秒（ms_to_iso 的逆；解析失败 None → 该锚放弃，保守不打标）
pub(crate) fn iso_to_ms(ts: &str) -> Option<i64>
```

**② 每工具的「锚」定义**

| 工具 | 锚（anchor_ms） | 获取 | 父侧 last_msg_ms |
|---|---|---|---|
| codex | 子 rollout `task_complete` 行 ts（**真实完成事件**） | `subagents::codex::collect(session_id)` 的 Idle 视图 `end_ts`（task_complete 即置 Idle，无 90s 滞后）→ `iso_to_ms` | `digest.last_timestamp`（ISO 串，`iso_to_ms`） |
| kimi | `agents/<dir>/wire.jsonl` mtime（最后写时刻 ≈ 回报写完） | 新 `subagents::kimi::wire_activity_anchors(session_id) -> Vec<i64>`（复用 `parse_session_index` / `resolve_session_dir` 定位 + readdir stat；stat-only 不进 incr 缓存） | `d.last_ts`（既有毫秒，构造处 :363-369 已在域） |
| opencode | 子会话 `time_updated` | 新 `subagents::opencode::child_updated_anchors(session_id) -> Vec<i64>`（同 `collect_with` 的 SQL 只取 time_updated 列） | `time_updated`（既有毫秒，:368-372 `last_activity_at = ms_to_iso(time_updated)` 同源） |

- **为什么不用 `SubagentView.end_ts` 直接判**：opencode / kimi 的 Idle 分诊有 90s 活动窗滞后——「刚完成 10s」的子代理在视图里还是 Running（end_ts=None），恰是打标最需要的时刻；故取原始活动毫秒锚，不经视图分诊。codex 无此滞后（task_complete 即 Idle 且 end_ts=complete_ts），走视图即可。
- **为什么比 last_msg 而不比「当前时刻」**：digest-now 随轮询推进，打标会在数秒后翻转为 false（flappy）；last_msg 锚使打标是会话内容的稳定函数（与 L2「纯内容产物」纪律同构）。
- **N=90s 的取值理由**：与模块既有 `ACTIVE_SECS=90` 同值（其论证「详情轮询 10s × 9 冗余，容忍长静默工具调用」复用于回报流式耗时），独立常量防两语义耦合漂移。

**③ 打标落位与预算闸**

三家现在硬编码 `false` 的构造点（`codex_parser.rs:694` / `kimi_parser.rs:384` / `opencode_parser.rs:406`）改为：

```
仅当 status == Completed ∧ last_message 非空 才收集锚并判定（预算闸）
```

- 预算：codex 走 `CODEX_IDX_SCAN`（(mtime,size) 门控 + 负缓存 + offset 增量）；kimi = agents 目录 readdir + stat；opencode = SQLite 查询即最新（AGENTS.md 豁免条款），一次 readonly 连接 + parent_id 等值查询。三家成本均被「仅绿会话」闸住，看板绿会话数受进程配对天然有界。
- claude 文本谓词零成本，不加闸照旧。

**④ 开关机制零改动**

KV 键、settings.tsx 按钮、`useNotification` gate 全不动。三家打标一旦落位，开关对四工具即同构生效：OFF 时仅命中打标的转绿被静默（整条含 addHistory），主会话自身完成（未打标）照常提醒——与 claude 完全同构的机制保证。

**⑤ i18n（值改，键不动）**

`settings.notifications.subagentReportDesc`（zh/en.json:183）：
- zh：「子 agent 完成回报唤醒主会话时的提醒（Claude/Codex/Kimi/OpenCode；关闭后主会话自身的提醒不受影响）」
- en："Alerts when a sub-agent report wakes the main session (Claude/Codex/Kimi/OpenCode; main-session alerts are unaffected)"

（同节其余 4 键 :182-189 不动。F2b flap 开关是另一道门，不在本轮。）

#### 口径差异（如实申报，§5 重申）

claude=「回报消息恰为末条」精确文本证据；三家=「子代理完成/最后活动发生在父末条前 90s 窗内」近似。误报面（OFF 方向）：子代理恰在父转绿前后活跃过但并非回报成因 → 该次完成提醒被静默；漏报面（ON 方向）：回报流式超 90s 才落到父末条 → 打不中、照常提醒。取舍：**宁漏报不误报**（漏报只是多一条可关的提醒，误报是吞掉用户要等的完成信号）。

#### 测试要点

1. `mod.rs`：`report_anchor_near_last` 边界（差 89_999 命中 / 90_001 不命中 / 锚晚于 last+skew 拒 / 锚==last 命中）；`iso_to_ms` 合法 / 非法形态；
2. `codex_parser`：夹具绿会话 + 子 rollout `task_complete` ts 距父末条 5s → 打标 true；10 分钟前 → false；无子代理 → false；非绿会话不触发锚收集；
3. `kimi_parser`：wire mtime 设定（夹具 set mtime）距 `d.last_ts` 窗内 → true；窗外 → false；
4. `opencode_parser`：sqlite 夹具（先例 `subagents/opencode.rs` tests 的 `make_db`）子 `time_updated` 窗内 / 窗外；
5. TS：`tests/notification/subagentReportFilter.test.ts` 增 codex/kimi/opencode 三家 payload——flag=true 且开关关 → 静默；flag=false 且开关关 → 主会话照常提醒（「关闭后主会话自身的完成提醒不受影响」的回归锁）。

#### 验收映射

- 「开关对四工具的子代理完成回报提醒均生效」→ 测试 2/3/4 + 5 前半；
- 「关闭后主会话自身的完成提醒不受影响（与现 claude 行为同构）」→ 测试 5 后半（gate 机制与 claude 同一条代码路径）。

---

## 2. 第 2 批（PR-2）：#80 codex 问答取消钮放开 Esc 中断

### 2.1 现状与两道门（含摸底口径修正）

- 门①：`inject/question.rs:5717 action_supported` 的兜底臂（:5825-5837）对单键动作委托 `answer_key_sequence_for`，Err 文案不含「越界 / 超键域 / 缺少选项序号」→ `ActionRefusal::ToolUnverified`（:5683-5695）→ `remote/api.rs:4788-4801` 映射 409 `tool_readonly`。
- 门②：`answer_key_sequence_for` SingleDigitSubmit 档 `:403 AnswerAction::Cancel => Err("Q_UNVERIFIED_CODEX: codex 取消未实测，不出键")`。
- **摸底口径修正（本轮通读 :5717-5837 全臂证实）**：`action_supported` 对 Cancel **无独立拒绝臂**，纯委托键序档——`:403` 改 `Ok` 后门①自然放行。任务简报称「只改 :403 仍被门拦」，以通读结论为准；为对冲口径分歧，交付仍按「两处」落实：代码改 `:403`，**门级新增回归锁** `action_supported("codex", Cancel, …).is_ok()` 断言，防将来门被单独收紧时 codex cancel 再被拦。
- 现注释已过时：「取消未实测」实为 2026-09-21 已实测（Esc=中断整个回合，语义破坏性不出手）。底料：`question.rs:275-283` 摘录 0.155.1 实测 + `docs/superpowers/specs/2026-10-08-codex-160-question-屏读底料.md` §2.3。

### 2.2 改法定案

**① 键序放开（`inject/question.rs:403`）**

```rust
AnswerAction::Cancel => Ok(vec!["esc".to_string()]),
```

（`:400` opencode 臂先行拦截，本臂仅 codex 可达。）注释同步：2026-10-18 裁决放开（#80）——Esc = 中断整个回合（破坏性：`function_call_output` 为 "aborted by user…" + `turn_aborted` 事件，非 claude K3 的 is_error 拒答回执）；前端按钮文案如实标注。

**② 注释三处同步（不做即留谎言注释）**

- profile 文档表（~:295-305「cancel 不代按」块）→「cancel=esc 中断回合（破坏性，2026-10-18 放开）」；
- `question_key_profile` codex 臂注释（~:338-341「cancel 因 Esc=中断回合而拒」）；
- 同构先例锚点照写：claude Cancel=esc（:249，K3 is_error 拒答回执）、opencode Cancel=esc（:400，E6 dismiss 落账）——三家语义差异（拒答回执 / dismiss / 整回合中断）在注释里保持可辨。

**③ 回执 / 审计 / 落账：零新建**

- 回执：200 `{"status":"key_sent"}` + `endpoint_audit(action="answer", summary="cancel", result="ok")`（claude 先例 `remote/server.rs:8023-8052`）；
- esc 中断落账已存在：`<turn_aborted>` 墓碑（`codex_parser.rs:150`）+ event_msg `turn_aborted` usage 计数（`usage/collectors/codex.rs` event_msg 臂）。

**④ 前端文案（`src/mobile/QuestionCard.tsx`）**

- `:1492-1497`（单题，`data-testid="question-cancel"`）与 `:1212-1217`（多题确认，`data-testid="question-confirm-cancel"`）两处硬编码「取消回答」改 `i18n.t("mobile.question.cancelInterrupt")`；
- mobile 已接入 `@/i18n`（本文件 `:64 import i18n from "@/i18n"` 既有先例），新键落 `mobile` 命名空间（先例 `mobile.composer.sendFailedRetry`，zh.json:1505 起）：
  - zh：「中断回合（破坏性）」／ en："Interrupt turn (destructive)"
- **只改这两个按钮**：组件内其余硬编码中文（「提交勾选」「返回上一题修改」等）与 `questionAnswerErrorCopy`（`mobile/api.ts:1015-1028`，硬编码中文映射）为既有独立债，不扩面（§5 申报）。
- 错误映射不动：本次不新增错误码；codex CLI cancel 不再出现 `tool_readonly`。
- codex APP 形态 409 止血门（`api.rs:4726 app_form_pending`）与本裁决无关，**不动**。

### 2.3 实机验证标注（裁决明示推迟）

Esc 在 codex 问答面板的最终语义（是否如 2026-09-21 实测为整回合中断）由**用户后续实机复核**；本设计的「破坏性」文案即按已实测语义如实前置，复核若推翻语义则回滚本项（单点 `:403` + 两键文案，回滚面小）。

### 2.4 测试要点

1. `question.rs` `codex_profile_digit_select_only`（:6305-6311）cancel 断言改 `vec!["esc"]`（现断言 is_err，TDD 先红后绿）；
2. 新增 `action_supported("codex", AnswerAction::Cancel, None, &single()).is_ok()`（门级回归锁）；
3. opencode / kimi cancel 行为不回归（既有 :6249 / :6281 / :6341 照跑）；
4. `remote/server.rs` 新黑盒：codex cancel → 200 key_sent + 审计 `action="answer"` / `summary="cancel"`（照 :8023-8052 claude 先例形态）；
5. `tests/mobile/QuestionCard.test.tsx`：两按钮新文案断言（i18n 键生效，中英两 locale）。

### 2.5 验收映射

- 「放开 Esc 中断」→ 测试 1/2/4；
- 「按钮保留并改文案『中断回合（破坏性）』，中英双语」→ 测试 5 + check-i18n 门禁（zh/en 双侧新键）；
- 「文案与行为一致」→ 键名 `cancelInterrupt` 与 `:403` esc 序列同一 PR 落地，注释互引。

---

## 3. 第 3 批（PR-3）：#69 codex 文件池首推 + 降级结论落档

### 3.1 现状管线

`remote/files.rs extract_file_paths`（:513）→ `extract_file_paths_with_env`（:546-617）：
`read_session_messages_impl`（content.rs:322，八工具 match 分派 :340-353）→ `EntryMerger.absorb_messages`（tool-call 走 PATH_KEYS 递归 / user 走 extract_inline_markup_paths）→ **zcode 特例第二抽取源**（:575-591 调 `read_zcode_attachment_refs_with` + `resolve_zcode_artifact`，merge `FileOrigin::User`）→ `merge_artifact_fallback`（:593；**调用顺序契约**：必须在 absorb 与第二源之后，:906-908）→ `filter_artifacts_by_existence`。整体 Err → `(Vec::new(), false)`（增强能力不阻塞）。

zcode 先例形态：`content.rs:624-712 read_zcode_attachment_refs_with(home, session_id, limit) -> Vec<(i64 窗口序号, String 路径, Option<i64> ts)>`，同 limit 同 seq 刻度，任何失败空 Vec。

codex 读取双路线：rollout JSONL 优先（`read_codex_rollout_with` :1157-1207，字节窗 `read_recent_lines_with_budget`），thread_history sqlite 兜底（APP 线）。`map_codex_lines` 只处理 response_item，**event_msg / session_meta 明确跳过**（:1216-1218，测试 :3244-3252 钉住）。`local_images` 全树零引用。event_msg 消费先例：`monitor/codex_parser.rs:210/:555` 与 `usage/collectors/codex.rs:297`。

issue #69 实测数据：2290 行 event_msg 含 local_images、非空 184 行、绝对路径 210 个、承载车道 2289/2290 在 event_msg（rollout JSONL）。

### 3.2 改法定案

**① 新抽取源函数（`remote/content.rs`，照 zcode 先例署名）**

```rust
/// Codex 用户图片附件第二抽取源（#69）：event_msg 行 local_images 字段。
/// 形态照 read_zcode_attachment_refs_with：同 limit 同窗口刻度，任何失败空 Vec。
pub(crate) fn read_codex_local_images_refs_with(
    home: &Path,
    session_id: &str,
    limit: usize,
) -> Vec<(i64 /*窗口行序*/, String /*绝对路径*/, Option<i64> /*ts 毫秒*/)>
```

- **rollout 定位**：从 `read_codex_rollout_with`（:1157-1207）提取共享 helper `locate_codex_rollout(sessions_dir, session_id) -> Option<PathBuf>`（文件名快路径 + 首行 `payload.id` 校验兜底；**未排序取首个含 id**——与既有提取链同口径）。`read_codex_rollout_with` 改调该 helper（行为不变，既有测试锁）。同 id 多份 rollout 的「提取侧首个 vs monitor 侧 mtime 最新」分叉是 `files.rs:597-605` ⚠ 已登记既有问题，**新源沿用提取侧口径、不扩面修**（落档申报）。
- **窗口预算**：复用 `read_recent_lines_with_budget(&f, line_budget(limit), byte_budget(limit))`——与 `read_codex_messages_with` 同刻度（条数窗 / 字节窗 512KB×⌈limit/200⌉ 封顶 4MB 机制照抄）。
- **行判据**：`type=="event_msg"` 且 local_images 非空；local_images 取 `payload.local_images`（主判据——与 event_msg 既有消费先例 `subagents/codex.rs apply_rollout_line` 的 payload 层同构）+ 顶层 `local_images` 兜底探一层（形态漂移容忍，命中任一即收）。条目形态：字符串路径 或 对象 `path` 字段；其余跳过。
- **路径卫生**（函数内前置，不依赖下游）：URI 判别（冒号前缀 >1 字符跳过，同 `files.rs:983-987` 判别——Windows 盘符单字符不误伤）；`path.exists()` 不存在跳过（照 zcode 先例「实体已清理 → 降级跳过，不可预览条目无展示价值」）。
- **seq 刻度**：窗口内行序（单调，与消息条目序同向）。与 zcode「同刻度」差异如实说明：codex event_msg 行不在 `pg.messages` 条目集内（被 map_codex_lines 过滤），取行序作单调代理；merger 只消费大小关系，排序语义一致。
- **不改 `map_codex_lines`**：本函数是独立第二抽取，:3244-3252 event_msg 跳过测试原样照跑。

**② 插入点（`remote/files.rs`）**

zcode 特例块（:575-591）之后、`merge_artifact_fallback`（:593）之前——顺序契约（:906-908）不破：

```rust
} else if agent_type == "codex" {
    for (seq, path, ts) in
        super::content::read_codex_local_images_refs_with(home, session_id, limit)
    {
        merger.merge(&path, seq, ts, FileOrigin::User);
    }
}
```

**③ FileOrigin 定案：User 池（零前端改动）**

- 语义合身：local_images = 用户带入会话的图片（粘贴 / 拖入），天然 user 来源；
- 新池方案需四处同步（`files.rs` enum+rank / `mobile/api.ts` FileOriginFilter / `FilePanel.tsx` originOptions+originLabel），无任何用户可见收益；
- rank 既有序 `session_artifact < user < tool_read < tool_write`：User=1，artifact 兜底命中已存在条目时跳过，不会覆盖。

**④ 降级结论落档（两份新档，`docs/superpowers/specs/`）**

**档一 `2026-10-18-codex-local-images-file-pool-底料.md`**（骨架）：

```
# codex local_images 文件池抽取底料（#69）
> 引言块：来源（issue #69 用户实测）、结论一句话（event_msg/local_images 进 User 池，
> 限 rollout JSONL 车道，字节窗与消息读取同刻度）
## 一、实测数据引用
  2290 行 event_msg 含 local_images / 非空 184 / 绝对路径 210 / 车道 2289 rollout + 1 thread_history
## 二、定案总表
  抽取源 / 判据层级（payload 主 + 顶层兜底）/ 池=User / 窗口预算（line_budget+byte_budget 同刻度）/
  URI 判别 / 存在性过滤 / 同 id rollout 口径（提取侧首个，沿既有 ⚠ 登记）/ seq 刻度（行序单调代理）
## 三、证据状态申报
  实测数据引自 issue #69（不伪造原始底料）；local_images 字段层级（payload vs 顶层）
  以实现夹具 + 用户实机样本定案后回填本节；thread_history 单例车道不接（量级 1/2290，投入产出否决）
## 四、实现锚点
  read_codex_local_images_refs_with 签名 / locate_codex_rollout 提取 / files.rs 插入点 / 关联测试清单
```

**档二 `2026-10-18-claude-opencode-不入池结论.md`**（骨架）：

```
# claude / opencode 附件不入文件池结论（#69 降级落档）
> 引言块：裁决来源（#69 本轮裁决）、结论一句话（两家无路径形态附件可提取，不入池）
## 一、claude：base64 内嵌
  附件以 base64 内嵌于消息体，无独立文件路径可提取 → 不入池（证据形态说明）
## 二、opencode：part 无 file 类型
  part 表无 file 类型 → 不入池（引 src-tauri/src/remote/attachment_fixtures.rs:1-11 既有 B1 调研注释为证）
## 三、重开条件
  任一工具未来引入带路径的附件形态（文件落盘 + 消息引用路径）时重评
## 四、待办边界
  workbuddy / openclaw / dsh 调研不在本轮（issue 内保持待办）
```

（体例先例：`2026-10-08-codex-160-question-屏读底料.md`（头部引言块 + 定案总表 + 证据状态申报「不伪造原始底料」）与 `2026-10-12-codex-app-form-止血门底料与摘门条件.md`。`research/` 在 .gitignore 不可入库，故入 `docs/superpowers/specs/`。）

### 3.3 测试要点

1. `content.rs`：`locate_codex_rollout` 提取后 `read_codex_rollout_with` 行为不变锁；新函数——合成 rollout 夹具（tempdir，`rollout-<ts>-<uuid>.jsonl`，session_meta 首行 + response_item 行 + event_msg 行含 `payload.local_images`）命中；顶层 local_images 兜底命中；URI 条目跳过；不存在路径跳过；字节窗截断（超窗旧行不读）；
2. `files.rs`（mod tests 现有 57 个）：端到端 `extract_file_paths_with_env("codex")` → User 池条目（zcode 金夹具链 :2842 / kimi :2814 先例——codex 合成 rollout 夹具照 `map_codex_lines` 测试形态）；同一路径同时是 artifact 候选 → 单条 User（顺序契约 + fallback 不覆盖锁）；同 id 双 rollout 首个命中锁；:3244-3252 event_msg 跳过测试照跑（`map_codex_lines` 零改动回归锁）；
3. i18n / 前端零改动（User 池决定）——`FilePanel` 既有测试照跑。

### 3.4 验收映射

- codex 会话文件池出现用户图片（User 池，来自 event_msg/local_images）→ 测试 1/2；
- claude / opencode 行为不变 + 结论落档 → 档二 + 两家既有 files 测试照跑；
- zcode 既有链路回归不变 → 测试 2 末项。

---

## 4. #61 大项：适配追记（不重写计划）

计划 `docs/superpowers/plans/2026-09-15-subagent-completion-signal-fix.md`（1482 行，11 Task，阶段 A=1-4 / B=5-7 / C=8-9 / D=10-11，每阶段独立 PR）按原样推进；本轮在文件**末尾追加**「## 2026-10-18 适配追记」，内容四条适配 + Task 2 推迟裁定 + 阶段顺序确认：

1. **适配① Task 4 测试脚手架对齐 main 现状**：`setup_schema` 不存在（用 `zcode_parser.rs:784 build_cli_db`）；`insert_session` 实为 7 形参（:821，计划写 6）；`insert_part` 实为 5 形参无时间参（:854，超龄场景写进 part JSON 的 `state.time.start`）；`app_status.rs` 现有 26 测试（计划写 18）、`zcode_parser` 35（计划写 20+）。
2. **适配② Task 10 双 schema 重设计**：opencode.rs 523→2495 行；`schema_is_v2`（:87）双 schema；`get_last_message_info` 仅 v1（:430），v2 走 `get_last_message_info_v2`（:544）；新增 TailSignal / 问答卡层（:503/:728/:876/:992）；「## New Messages」判据在 main 全文件零命中——v2 库形态须实现时以夹具定案，不得照抄 1.x 判据。
3. **适配③ content.rs 同步点映射表**：parser 改 message_origin / 注入口径后，`remote/content.rs` 的同构复制品必须同批同步（zcode :536-550 / claude isMeta :950-958 / dsh :805-821+:922 / codex :1210-1239 / kimi :1527-1571），逐 Task 列映射表。
4. **适配④ 并入项现状**：teammate 完成信号已在 `monitor/subagents/claude.rs:383 teammate_idle_from` 独立落地（甲.5），与 Task 5 互补不冲突，Task 5 不再重做该部分。
5. **Task 2 推迟裁定**：实机探测 5 项推迟，各 Task 判据沿用计划内既有实测证据；实现中发现判据失配 → 停下报告，不猜不抄旧档。
6. **阶段顺序确认**：A→B→C→D，每阶段独立 PR。

（追记全文见该 plans 文件末尾；本节不重复。）

---

## 5. PR / 分支划分与实施顺序

| PR | 分支 | 内容（commit 按项拆） |
|---|---|---|
| PR-1 | `fix/tracking-batch-1-subagent-ui` | ① `fix(preset): 子 Agent 级操作入口复活并区分空态与不支持态——#66`；② `fix(board): 父卡子代理徽标改用 monitor::subagents 同源重算——#134`；③ `fix(notify): 子代理回报提醒开关扩展至 codex/kimi/opencode——#151` |
| PR-2 | `fix/80-codex-cancel-esc` | `fix(inject): codex 问答取消放开 Esc 中断并标注破坏性文案——#80` |
| PR-3 | `feat/69-codex-local-images-file-pool` | ① `feat(files): codex 文件池新增 event_msg local_images 用户图片抽取源——#69`；② `docs(specs): codex local_images 抽取底料与 claude/opencode 不入池结论落档——#69` |
| #61 | `fix/61-subagent-completion-signal`（阶段分支） | 阶段 A→D 各自 PR；追记随阶段 A 首 PR 或独立 docs commit：`docs(plan): 子代理完成信号修复计划追加 2026-10-18 适配追记——#61` |

**顺序建议**：PR-2（最小、纯后端单点 + 两键文案）→ PR-1 → PR-3；#61 阶段 A 与三批有 `monitor/` / `remote/content.rs` 弱文件交集（PR-1 改 claude_parser 赋值点与 subagents 模块、PR-3 改 content.rs 抽取层、阶段 A 改 app_status / zcode_parser / content.rs 复制品），**建议三批先合、#61 阶段 A 随后 rebase 起步**，避免同文件并行冲突。三批之间无依赖，可并行开发。

每 PR 门禁：`scripts/ci-backend-local.sh` + `pnpm check` +（涉 i18n）`node scripts/check-i18n.mjs`；Rust 侧 `cargo test` 目标模块 + `cargo clippy`。

---

## 6. 风险与如实申报区

1. **#134 旧布局口径**（§1.2 定案）：换源后旧布局会话徽标恒 0 不显示；口径 = 测试与行为面不回归，布局收窄可接受。如用户仍在用旧布局 claude 且依赖徽标 → 备选（monitor::subagents 补旧布局扫描）留用户裁决，本轮不做。
2. **#134 连带效应**：flap 谓词（adapter/mod.rs:180）从死门恢复可触发，F2b 静默开关恢复设计行为——修数的必然结果，已申报，不改谓词。
3. **提醒开关口径差异**：claude 精确文本证据 vs 三家 90s 时间关联近似；误报面（子代理恰活跃但非回报成因的转绿被静默）与漏报面（回报超 90s 落末条打不中）如实存在；取舍宁漏报不误报。settings 文案不写技术细节，差异落本 spec 与测试。
4. **#80 摸底口径修正**：action_supported 对 Cancel 是委托非独立拒绝点（通读证实），代码改动单点 :403；门级回归锁对冲。Esc 实机语义验证推迟——若复核推翻「整回合中断」语义则回滚（单点 + 两键，回滚面小）。QuestionCard 其余硬编码中文与 questionAnswerErrorCopy 中文映射为既有债，不扩面。破坏性操作的误触风险由文案前置警示缓解（裁决指定）。
5. **#69 local_images 字段层级待实机定案**：payload 层为主判据（与既有 event_msg 消费先例同构）+ 顶层兜底双探；夹具先行，用户实机样本复核后回填底料档（不伪造原始底料）。同 id 多 rollout 分叉为 files.rs ⚠ 既有登记，新源沿用提取侧口径不修。thread_history 单例车道不接（1/2290）。
6. **#61 Task 2 推迟风险**：判据沿用计划内既有证据，若实现中发现失配 → 停下报告纪律（追记已写入）。
7. **#66 契约变更**：`detect_subagents` 返回结构体，前后端 + tauri-mock 同 PR 同步（单体应用同构建，无兼容负担）；入口恒渲染增加卡片视觉密度（ruling 预期行为）。

---

## 7. 验收标准总表（供 test-engineer 设计测试）

| 项 | 验收判据（做完的标准） | 对应测试 |
|---|---|---|
| #66 | 无子代理工具显示折叠入口，展开显示「未检测到子 Agent」；不支持工具显示「不支持」文案；有子代理行为不变 | presetList.test.tsx 5 用例（§1.1） |
| #134 | 新布局会话有活跃子代理 → 徽标 >0 且与 chips running 计数一致；无子代理 → 徽标不显示；既有测试 / flap 测试不回归 | §1.2 测试 1-4 |
| 提醒开关 | 四工具打标测试 + 开关关时「回报转绿静默 / 主会话照常」双态；claude 既有谓词测试照跑 | §1.3 测试 1-5 |
| #80 | codex cancel 出 esc（键序级 + 门级 + server 黑盒审计）；两按钮文案中英双语；opencode/kimi cancel 不回归 | §2.4 测试 1-5 |
| #69 | codex 文件池 User 池出现 local_images 路径；URI / 不存在路径不入池；zcode / claude / opencode 既有测试不回归；两份落档存在且无占位 | §3.3 测试 1-3 + 档案评审 |
| #61 追记 | plans 文件末尾追记六要素齐备，不改计划原文其他内容 | git diff 仅追加 |
