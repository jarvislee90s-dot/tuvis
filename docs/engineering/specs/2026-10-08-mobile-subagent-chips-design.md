# 移动端子 agent 运行 chip（方案 A：磁盘/库直读，四工具）设计

日期：2026-10-08 ｜ 状态：已评审修订 + 四工具实机取证并入（architect 评审 2026-10-08）｜
范围：远程看板（`/m`）会话详情

## 1. 背景与问题

各 CLI 的终端底部状态区会显示后台子 agent（如 Claude 的 `● main / ○ Plan … · 5m38s ·
↓6.1k tokens`），但远程端（手机看板）看不见——用户出门在外时无法知道「主会话下还有没有
agent 在跑、跑了多久、吃了多少 token」。需求：在会话详情底部模式栏（`ModeBar`）右侧显示
**运行中**子 agent 的名称、运行时长、token 消耗。

## 2. 数据源证据（2026-10-08 四工具本机实机取证，不猜）

### 2.1 Claude（判据权威，架构基准）

子 agent 落盘（`~/.claude/projects/<项目>/` 下；**父 JSONL 在项目层平铺，与会话目录
同级**——实施计划写作时核实并修正本表）：

| 落点 | 内容 |
|---|---|
| `<会话uuid>.jsonl`（项目层平铺，父转写） | 子 agent 每次停止追加 `<task-notification><task-id>agentId</task-id>…`；续跑 = `tool_use SendMessage{"to":"agentId"}` |
| `<会话uuid>/subagents/agent-<agentId>.meta.json` | `{"agentType":"Plan","description":"…","toolUseId":"…","spawnDepth":1,…}` |
| `<会话uuid>/subagents/agent-<agentId>.jsonl` | 子 agent 完整转写：每条消息带 `timestamp` + `usage` 四桶 |

**排雷**：① 通知 ≠ 终态——agent 停止后可被 SendMessage 续跑，同 task-id 多次通知
（实盘：agent `a588fb48…` 3 条通知 + 1 条续跑，通知块 note 原文自认）；② 启动即有
tool_result（`Async agent launched…`），不能以「无 tool_result」判运行中。
**运行中判据 = 事件序状态机**（§4）。时长口径与平台一致（Claude 通知块 `duration_ms`
即自首次 spawn 起算）。

### 2.2 OpenCode（SQLite，数据模型最优）

`~/.local/share/opencode/opencode.db` 的 `session`/`session_v2` 表：**`parent_id` 非空即子
agent 会话**（实查 3 条：`explore`/`general` 两类），自带 `agent` 名列、token 五桶列
（`tokens_input/output/reasoning/cache_read/cache_write`）、`time_created/updated`（v2 另有
`time_idle`/`idle_outcome`）。本机样本里 `time_idle` 对已完成的旧子会话也是 NULL——**运行中
判据不能依赖 time_idle**，用 `time_updated` 活跃度（§4.2）。SQLite 类工具豁免 L2/L3 预算。

### 2.3 Kimi（wire 落盘，判据待实机校准两点）

[官方文档](https://www.kimi.com/code/docs/kimi-code-cli/customization/agents.html)确认有
subagent（Agent 工具可后台 + TaskList/TaskOutput/TaskStop）。本机实盘：会话目录
`agents/<agent>/wire.jsonl`——`main/` 是主 agent，另一会话实存 `agent-0…agent-6` 七个子
agent 转写；wire 首行 `{"type":"metadata","created_at":…}`，事件含 `usage.record`
（**usage 四桶 camelCase**：`inputOther/output/inputCacheRead/inputCacheCreation`）、
`tool.call/tool.result/step.end`。**待实机校准两点**：① 目录名是编号（`agent-N`），真实
任务名在父 wire 的派生事件里（本机样本是 agent_swarm 场景，内建 Agent 工具的派生事件形态
未取样）；② 内建 Agent 工具的**完成标记**未取样。

### 2.4 Codex（rollout 双线程，实机探测定案）

codex-cli 0.160.0 `codex features list`：`multi_agent` stable 默认开但 `codex exec` 下**不
暴露工具**；**`--enable multi_agent_v2` 后 collaboration 工具组全部出现**
（spawn_agent / send_message / list_agents / wait_agent / followup_task / interrupt_agent）。
实机派生一个子 agent 后磁盘落点：

- **子 agent 有独立 rollout 文件**，session_meta 带 **`parent_thread_id`**（普通会话无此
  字段）——父子关联零歧义；
- 子 rollout 内有 `token_usage_record`（`total_token_usage{input_tokens, cached_input_tokens,
  cache_write_input_tokens, output_tokens, reasoning_output_tokens, total_tokens}` 累计口径）
  与 `inter_agent_communication_metadata` 事件；
- **终止标记 = 子 rollout 出现 `task_complete` 事件**（实机样本末行即它）；
- 如实申报：上游 `multi_agent_v2` 半熟——实测任务正文经消息传递会丢（官方通道的已知痛点），
  且功能需 feature 门控；判据本身（parent_thread_id / task_complete）不受影响。

## 3. 范围与口径

- **四工具并入 v1**：claude（判据权威）+ opencode + kimi + codex；其余工具空列表。
- **只显示运行中**；多 agent 并发多枚 chip；会话非活跃整区不渲染。
- kimi 只认「wire 存在且活跃」的 agent 目录；codex 只认带 `parent_thread_id` 的 rollout。
- **token 口径如实申报**：各工具取「该子 agent 自身落盘的 usage 累计」，与各家终端 footer
  的计数（内部口径不透明）可能不等——不假装逐字一致。

## 4. 运行中判据（每工具一章；claude 为权威基准）

### 4.1 Claude：事件序状态机

| 事件 | 来源 | 效果 |
|---|---|---|
| 登记 spawn | `subagents/agent-<id>.meta.json` 存在 | last = running |
| 停止 stop | 父 JSONL 追加 `<task-id>该id</task-id>` 通知 | last = stopped |
| 续跑 resume | 父 JSONL 出现 `tool_use SendMessage{"to":"该id"}` | last = running |

```
运行中 = 已登记 ∧ last ∈ {spawn, resume}
```

不做 mtime 兜底（v1 裁决：长静默工具调用会误伤；残留随会话中断整区消失，§8 申报）。

### 4.2 OpenCode：活跃度判据

```
运行中 = session_v2.parent_id 非空 ∧ time_updated 距 now < ACTIVE_SECS（90s：
         详情轮询 10s 的 9 倍冗余，容忍子 agent 长静默工具调用；常量可调）
```

「停止」即活跃度自然过期，无需显式终止事件（SQLite 无通知流）。**待实机校准**：token 列
是否随跑随更（若结束才写，chip 的 token 在运行期显示 0——如实显示，不编数）。
**桶位映射**（对齐 §5.2 四桶载荷）：`tokens_input→input`、`tokens_cache_read→cacheRead`、
`tokens_cache_write→cacheCreation`、`tokens_output→output`；`tokens_reasoning` **不计入
展示合计**（与 codex 同款申报，见 §8.5）。

### 4.3 Kimi：活跃度判据 + 两点实机校准

```
子 agent = agents/<dir>/wire.jsonl 存在 ∧ dir != "main"
运行中   = wire.jsonl mtime 距 now < ACTIVE_SECS（同 4.2 取值）
```

待实机校准（校准后收严为事件判据，替换 mtime）：内建 Agent 工具的完成标记；真实任务名的
派生事件形态。v1 名称显示目录名（`agent-3`），实机校准后换真名。

### 4.4 Codex：线程对判据

```
子 agent = rollout session_meta 带 parent_thread_id
运行中   = 该 rollout 无 task_complete 事件
```

**桶位映射**（codex 的 `input_tokens` **内含** cached，直接求和会重复计数）：
`input = input_tokens − cached_input_tokens`、`cacheRead = cached_input_tokens`、
`cacheCreation = cache_write_input_tokens`、`output = output_tokens`；
`reasoning_output_tokens` 不计入展示合计（申报见 §8.5）。
增量读时维护 `task_complete_seen` 布尔（出现即置位，永不复位）——判据查它而非回扫文件。
**门控申报**：`multi_agent_v2` 默认关——未开启的会话天然无子 agent rollout，判据自洽空态。

## 5. 后端设计

### 5.1 扫描与缓存（L2 预算纪律 + 评审收紧）

进程内全局缓存表：`static OnceLock<Mutex<HashMap<(agent_type, session_id), Cache>>>`，
条目硬上限（照抄 `session_scan.rs` 的 `EVICT_HARD_CAP` 模式，超限整段清空）。**仅
claude / kimi / codex 入缓存**（文件增量语义）；opencode 查询即最新，无缓存条目。

- **claude**：缓存与判据按 §4.1 状态机——父 offset 增量抽通知/SendMessage 两类事件 +
  每 agent jsonl offset 增量累计四桶 + 首条 timestamp；重建走**倒序分块扫描**（登记集
  收齐即停；扫到头仍有未命中 = spawn 态）；`size < offset` 护栏作废重建。
- **kimi / codex**：同款「offset 增量 + 半行缓冲 + stat 门控」骨架复用（抽公共增量读
  helper）；kimi 增量累计 `usage.record` 四桶；codex 读最新 `total_token_usage` + 维护
  `task_complete_seen` 布尔（§4.4）。
- **codex 子 rollout 的发现——倒排索引**：端点按 session_id 查询，需「parent_thread_id →
  rollout 路径」倒排索引（sessions 目录 249+ 文件，不可能每轮全扫首行）。索引只读文件
  首行 session_meta，stat 门控增量维护；扫描窗口对齐既有 codex_parser 的 24h 新鲜窗口
  （L3 纪律：窗口外仅缓存命中参与）。
- **kimi 会话目录解析**：复用 `kimi_parser` 的 `parse_session_index` / 会话目录解析把
  session_id 映射到 `sessions/<workdir>/session_<uuid>/` 目录，不另写路径推导。
- **opencode**：单条 SQL（parent_id 非空的活跃子会话 + token 列），SQLite 豁免预算纪律，
  无缓存（查询即最新）。
- **为何不用 SessionFileScan 摘要缓存**：摘要缓存对「持续增长的转写累计 token」意味着每轮
  全文件重解析；offset 增量是 O(delta)——正当偏离，防后人「归一」。缓存物全是文件内容
  纯函数，无时间叠加；运行时长服务端下发 `spawnTs`、客户端自走。

### 5.2 端点

```
GET /m/api/v1/session-subagents?agent_type=<claude|opencode|kimi|codex>&session_id=…
```

```json
{ "subagents": [
  { "id": "a86f…", "name": "Plan",
    "description": "设计新建会话两改动实现方案",
    "spawnTs": "2026-10-08T07:36:35.508Z",
    "tokens": { "input": 5124, "cacheRead": 56448, "cacheCreation": 0, "output": 9312 } }
] }
```

- 只含运行中条目（各工具判据见 §4），按 `spawnTs` 升序；空列表 = 无运行中；
  **端点是空态唯一权威**，前端不另特判。
- 会话不存在 / 非活跃 → `subagents: []`；未知 `agent_type` → `[]`；缺参/空白参 → 400；
  响应带 `Cache-Control: no-store`；文件 IO 全部 `spawn_blocking`。
- 鉴权：挂 `api_router` 即被内层 gate 结构性覆盖，零额外接线。
- **每工具一个 source 注入缝**（评审定的模式）：`subagent_source: HashMap<agent_type,
  Box<dyn Fn(&str, &str) -> Vec<SubagentView>>>` 挂 RemoteState（claude 缝按 §5.1 缓存
  语义实现；opencode 缝直查 DB；测试全部假源零接触真实磁盘）。
- **待实机校准的两处**（kimi 完成标记、opencode token 随跑性）不影响载荷形状——校准只改
  各自 source 实现，端点契约冻结。

## 6. 前端设计

- **新组件 `src/mobile/SubagentChips.tsx`**：会话详情底部状态区、`ModeBar` 的**兄弟节点**
  （生命周期解耦——mode 视图失败/unsupported 时 chip 仍要活）。布局：同一行 flex-wrap。
- **chip 形态**：`◉ Plan 5:38 · 6.1k`（名称 + 运行时长 + token 合计），多 agent 多枚。
  - 时长客户端自走：tick 状态收在 SubagentChips 组件内部（不拖整页每秒重渲染），
    下轮拉取用服务端数据校正；
  - token 用既有 `fmtTokens` 口径对四桶求和格式化；`description` 进 `title` 悬停。
- **刷新**：随 SessionDetail 既有 `DETAIL_REFRESH_MS` 轮询（refreshTick 驱动，免费继承
  hidden 暂停/恢复补刷）独立拉取。
- **降级**：拉取失败**保留上一份成功数据**（不闪断、无错误 UI；首次成功前失败则不渲染），
  成功载荷（含空列表）即覆盖——空态权威仍在端点。**key = `subagents-${session.id}`**。

## 7. 测试

- **Rust（判据与解析，全部纯函数化）**：
  - claude：事件序状态机（含**通知后 SendMessage 复现 running** 的 P0 回归锁）、增量解析
    半行缓冲、`size<offset` 护栏、倒序重建两分支、stat 门控读计数断言；
  - opencode：活跃度过期即停、parent_id 过滤、时间边界（ACTIVE_SECS 边界值）；
  - kimi：mtime 活跃判据、main 目录排除、usage.record 四桶累计；
  - codex：`parent_thread_id` 识别、`task_complete` 终止、`total_token_usage` 直读；
  - 端点：假源注入、空态/未知工具/缺参 400/no-store 头。
- **前端（vitest）**：chip 渲染 / 多枚并排 / 空列表不渲染 / 拉取失败不渲染 / 时长自走
  （fake timers，tick 不出组件）。
- **手工验收**：claude 起后台 agent + SendMessage 续跑（chip 消失→复现）；opencode 起
  explore 子 agent；kimi 起内建 Agent 后台任务（顺带取证完成标记，回填 §4.3）；codex 开
  `multi_agent_v2` 起子 agent。

## 8. 已知边界（如实申报）

1. token 数字与各家终端 footer 可能不等（口径不同）——chip 不声称「与终端一致」。
2. **claude**：TaskStop 手动停止是否落通知未验证（残留 chip 的可能入口，随会话中断消失）；
   spawn 瞬间竞态（meta 已写、jsonl 未落 → 无时长只显 token，下轮自愈）；resume 跨代
   （新 uuid 目录）不做跨代匹配。
3. **opencode**：token 列若非随跑随更，运行期 token 显示真实值（可能为 0）不编数；
   `time_idle` 语义未用于判据（样本显示已完成会话亦为 NULL）。**长静默子 agent 会被
   暂时隐藏**（`time_updated` 超 ACTIVE_SECS 即不显示，与 kimi 的 mtime 判据同口径：
   不可证活跃即不显示）——长跑测试套件期间 chip 会消失一段时间。
4. **kimi**：编号名（`agent-N`）非真名，派生事件形态与内建完成标记待实机校准后收严；
   mtime 活跃判据在长静默工具调用期间会暂时隐藏 chip（如实口径：不可证活跃即不显示）。
5. **codex**：功能需 `--enable multi_agent_v2`（0.160.0 默认关）；上游任务正文传递有 bug
   （不影响检测）；`reasoning_output_tokens` 单列不并入合计（与 input 重叠口径未验证，
   total_tokens 慎用）。
6. **看板徽标落差**：`monitor/jsonl.rs` 的 `count_active_subagents` 只扫 claude 旧布局
   平铺层，新布局下恒 0——与本端点同源不同层，v1 不动，后续批次用同源数据修复或删除。
7. 仅四工具；其余空态。MAM 重启缓存清零，首次请求重建，判据前后一致。
