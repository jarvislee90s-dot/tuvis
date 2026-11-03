# 移动端子 Agent 观察台 · 实施计划（单批：修复 + 清单 + 详情 + 提醒开关）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 修好 claude 双机制（经典 / teammate）的子 agent 完成识别，并在同一地基上交付三个观察特性：文件看板「子 Agent」清单（绿/灰点、派发序、冻结/续跑）、子 agent 实时预览对话框（逐步输出、claude 全量）、「子 Agent 回报提醒」全局开关。

**Architecture:** 判据修复收在 `monitor/subagents/claude.rs` 纯函数层（idle_notification 信号检测 + 名字→档案别名归一，经典机制零改动）；清单 = 既有 `/session-subagents` 端点载荷扩展（status/endTs 终态字段 + 全量名单，四工具 source 各自补终态）；详情 = 新端点 `/session-subagent-messages` 复用 `content.rs` 的 claude 消息映射（`map_claude_lines`）→ 前端复用会话消息渲染器（从 SessionDetail 抽取共享 hook）；提醒开关 = claude_parser 判据层同源打标（`Session.lastMessageSubagentReport`）+ KV 开关 + useNotification 布尔过滤（不匹配文案）。前端把子 agent 列表拉取从 chip 组件上提到 SessionDetail（单一数据源供 chip / 面板卡区 / 详情三处消费）。

**Tech Stack:** Rust（axum / serde / chrono）+ React 19 + TypeScript + vitest + Tailwind CSS v4。

**设计依据（唯一权威，本计划只做任务分解、不改任何判据/口径）：**
`docs/engineering/specs/2026-10-08-mobile-subagent-observatory-design.md`（已裁决单批到底）。
事实底稿：`docs/release-notes/c3-subagent-chips-acceptance-2026-10-08.md`（甲.4 两机制对照表 / 甲.5 修复立项四条 / 乙.3 扩展取证）。
spec 引用记号：§一 修复 / §二 清单 / §三 详情 / §四 提醒开关 / §五 不做 / §六 验收清单。

---

## 一、现状锚点核对表（写计划时已逐一核对，行号为 2026-10-09 快照、实施时以符号检索为准）

| 锚点 | 现状 | 结论 |
|---|---|---|
| `src-tauri/src/monitor/subagents/claude.rs:299` | `classify_parent_line`（resume=SendMessage `input.to` / stop=`<task-id>` 提取，`task_id_in`:290） | teammate 分支在此扩展（stop② = idle_notification） |
| `src-tauri/src/monitor/subagents/claude.rs:380` | `AgentMeta { agent_type, description }`（serde 字段缺失容忍） | 增 `taskKind` / `name` 两可选字段 |
| `src-tauri/src/monitor/subagents/claude.rs:28` | `locate(projects_root, session_id)`（私有，已含 `..`/分隔符逃逸守卫） | 提为 `pub(crate)` 供详情端点复用（T4） |
| `src-tauri/src/monitor/subagents/claude.rs:166/229` | `step()`（运行过滤出卡）/ `rebuild()`（倒序收齐事件） | 两处接别名归一；T2 把运行过滤改为全量出卡 |
| `src-tauri/src/monitor/subagents/claude.rs:353` | `accumulate_agent_line(line, acc, first_ts)` | 增 `last_ts` 捕获（终态锚） |
| `src-tauri/src/monitor/subagents/mod.rs:43` | `SubagentView { id, name, description, spawn_ts, tokens }`（`pub`，camelCase） | 增 `status` / `end_ts`（T2） |
| `src-tauri/src/monitor/subagents/opencode.rs` / `kimi.rs` / `codex.rs` | opencode 90s 过滤 `continue`；kimi 收集时 active_within 门控；codex `!task_complete_seen` 不出卡 | 全部改「出卡 + status 分诊」 |
| `src-tauri/src/remote/api.rs:601` | `session_subagents` handler（400/no-store/spawn_blocking/端点排序） | 载荷契约扩展（T3）+ 新详情 handler（T4） |
| `src-tauri/src/remote/server.rs:468/587/617` | `SubagentSourceFn` 别名 / `RemoteState.subagent_source` / `/session-subagents` 路由 | 新缝 `subagent_message_source` + 新路由（T4） |
| RemoteState 测试字面量 | `subagent_source` 补桩位：server.rs 19 处 + `state_with_subagent_fakes`（:14426）+ remote/mod.rs 生产装配（:385） | 新字段由编译器驱动同款补桩 |
| `src-tauri/src/remote/content.rs:935` | `map_claude_lines`（私有纯函数：user/assistant/thinking/tool_use/tool_result → SessionMessage，attachment 行无 `message.content` 天然跳过） | 详情端点直接复用；新增 `read_claude_subagent_messages_with` |
| `src-tauri/src/remote/content.rs:375/383` | `line_budget(limit)` / `byte_budget(limit)`（512KB×⌈limit/200⌉ 封顶 4MB） | 详情读取同一预算口径 |
| `src-tauri/src/remote/api.rs:808` | `session_messages` Err → 404（细节只进日志） | 详情端点同映射 |
| `src-tauri/src/monitor/claude_parser.rs:125/220` | digest `last_message`（末条有文本消息预览，100 字符截断**前**有原文） | T7 打标挂在截断前的原文上 |
| `src-tauri/src/session/model.rs:70` | `Session`（camelCase Serialize/Deserialize；IPC 与远程端点共用） | 增 `last_message_subagent_report`（T7，横切补桩编译器驱动） |
| `src/mobile/SessionDetail.tsx:93` | `PreviewState = list \| file`（辨识联合） | 增第三形 `subagent`（T5/T6） |
| `src/mobile/SessionDetail.tsx:313/434` | `refreshTick`（手动刷新 + 10s 轮询，hidden 暂停/恢复补刷） | 子 agent 列表拉取挂同一信号（T5） |
| `src/mobile/SessionDetail.tsx:265/278/597/652` | `linkifyMarkdown` / `linkifySegments` / `renderMarkdown` / `renderLinkifiedText`（组件内私有） | 抽取 `src/mobile/message-render.tsx`（T6，纯搬家） |
| `src/mobile/SessionDetail.tsx:1049` | cardDock（key 纪律：approve-/plan-fb-/question-/mode-/subagents-/composer- 互异） | 本批不新增 cardDock 兄弟，纪律不变 |
| `src/mobile/SessionDetail.tsx:1213-1229` | 消息折叠头（toggleable + collapsedLabel + ChevronRight/Down） | SubagentDetail 同款交互（T6） |
| `src/mobile/SubagentChips.tsx:16` | chip 组件（自拉 fetch + 1s tick 收组件内 + `formatElapsed`/`chipTokenText` 导出） | 改纯展示 + 增点击回调 + 导出 `subagentElapsedMs`（T5） |
| `src/mobile/FilePanel.tsx:109/304` | props 驱动纯展示；`panel-content` 为卡区宿主落点 | 增可选 `subagents` / `onOpenSubagent` props（T5） |
| `src/mobile/PreviewModeSwitcher.tsx:9` | `PreviewMode = "split" \| "split-h" \| "fullscreen"` | 详情视图共用（T6） |
| `src/mobile/message-fold.ts` | `isProcessKind` / `collapsedLabel`（活/归档共用折叠文案） | 详情视图直接复用 |
| `src/mobile/api.ts:1480/1490` | `SubagentView` TS 类型 / `fetchSessionSubagents` | 类型扩展（T3）+ 新 `fetchSubagentMessages`（T4） |
| `src/hooks/useNotification.ts:169` | `notifyCompletion`（每拍刷新 `notifications_enabled` → 过滤） | 子 agent 开关过滤插同一位置（T7） |
| `src/pages/settings.tsx:602-627` | 通知区块 + `toggleNotifications`（get_setting/set_setting KV 先例） | 新开关行同款（T7） |
| `src/i18n/locales/zh.json:156` / `en.json:156` | `settings.notifications.*` 键族 | 新键必须 zh/en 同步（`pnpm check:i18n` 门禁） |
| `src/tauri-mock.ts:687` | `get_setting` mock（`notifications_enabled` 等键） | 新键补 mock（T7） |
| `tests/mobile/SubagentChips.test.tsx:47` | fetch 桩：未知路由 throw（**桩必须在 throw 之前**的教训） | 组件改纯展示后整文件重写（T5） |
| `tests/mobile/SessionDetail.test.tsx:212/1191/1937` | 既有桩已回 `/session-subagents → {subagents: []}` | T5 后 SessionDetail 自拉仍走该桩，零补桩；详情端点需另补桩 |
| `tests/notification/notifyOnce.test.ts` | 通知 hook 测试惯例（store 注入 + invoke mock + hoisted mock） | 新过滤用例同款（T7） |

## 二、已拍板决策（spec 定案 + 本计划设计裁决，实施不得重议）

**spec 定案（不可改）：**

1. 完成识别按登记档案的机制标记分派：meta `taskKind=in_process_teammate` → teammate 机制；无标记 → 经典（与既有行为一致）；经典机制零改动（甲.5.4）。
2. teammate 完成信号 = 父 jsonl 行内 `{"type":"idle_notification","from":"<名字>"}`（`teammate_id` 属性辅助）；名字→档案映射以 meta `name` 字段归一，Stop/Resume 双向同映射；Resume = `tool_use SendMessage` 的 `input.to`（名字或长 id 双形态经同一映射）。普通（非 idle）teammate-message 不计为完成信号（甲.5「待议不入本批」）。
3. 清单单一列表按派发时间升序（新增附末尾）；四要素 = 名字/任务摘要/时长/token；绿点活跃走字、灰点冻结原位不重排不折叠；续跑灰→绿从冻结值继续走字（时长从首次派发连续累计不归零）；重启可回溯；无子 agent 卡区不渲染。
4. 详情 = 预览空间第三形态（split/split-h/fullscreen 同文件预览）；活跃打开期间数秒一拍自动刷新、关闭即停；不活跃定格快照；渲染与会话消息同一套；仅 claude，其余工具明确「暂不支持查看详情」（清单照常）。
5. 提醒开关默认开；关闭仅滤「子 agent 回报导致」的提醒；判定与修复项同源（判据解析打标，不靠 lastMessage 文案匹配）；v1 仅 claude。
6. 不做：向子 agent 发消息、不动 `count_active_subagents`（issue #134）、其余工具详情视图与提醒判定。

**本计划设计裁决（含权衡）：**

- **A. 名字归一用「别名表」而非 classifier 内机制分支**：`registered_agents` 后建 `alias_map`（id 恒注册；teammate 按 taskKind 加注 name 别名），`classify_parent_line` 产出的原始键（名字或 id）经 `normalize_event` 归一到档案 id。理由：resume 的 `to` 本来就有名字/长 id 双形态，别名表天然覆盖；经典 stop 信号只带 16-hex id、teammate 信号只带名字，分派在归一时自然发生，classifier 不需要 per-mechanism if-else。代价：归一匹配不到登记集的事件被丢弃（如父会话向 team-lead 发消息——它不是本会话子 agent，丢弃即正确）。
- **B. 终态锚 `endTs` 各工具就近取证，不统一造「完成事件时间戳」**：claude = 子转写末行 timestamp（停写即冻结；`AgentTrack` 增 `last_ts`，不动事件状态机）；opencode = `time_updated`；kimi = wire mtime；codex = `task_complete` 行 timestamp。运行中恒 `null`。理由：前端冻结时长需要一个统一锚（`elapsed = (endTs ?? now) − spawnTs`），而四工具的「完成时刻」证据天然不同源，强造统一事件反而要改三套状态机。代价：opencode/kimi 的 endTs 是「最后活动时刻」而非精确完成时刻（它们本就无完成事件），长静默转灰时冻结在静默起点——与 v1 已申报的 90s 窗口径一致，如实申报。
- **C. v1「超窗/完成后隐藏」改为「转灰冻结」**：spec §二.3 要求不活跃卡片留在原位。opencode/kimi 的 90s 活跃窗语义从「过滤出板」变为「status 分诊」；codex `task_complete_seen` 从「不出卡」变为「status=Idle」。这是载荷语义变化（v1 端点契约冻结条款由本批 spec 明示解除），既有测试断言随改。
- **D. 子 agent 列表拉取上提到 SessionDetail（单一数据源）**：v1 chip 自拉；本批三处消费（chip 过滤 running / FilePanel 卡区全量 / 详情对话框查 status），组件各自拉同一端点会三份状态可漂移。代价：`SubagentChips` 从自拉组件改为纯展示（既有测试重写，换来三处一致 + 详情可判运行态）。
- **E. 详情端点载荷 = session-messages 同形 + `supported` 标志**：`{messages: SessionMessage[], truncated: boolean, supported: boolean}`。source 未装配（opencode/kimi/codex）→ 200 `supported:false` 空表（「暂不支持」是正常态不是错误，不给 404）；claude 读取失败（会话/转写不存在）→ 404（session_messages 同映射）。前端渲染器零适配。
- **F. 渲染复用经「抽取共享 hook」而非复制**：SessionDetail 的 `renderMarkdown`/`renderLinkifiedText`/`renderBody`（含 linkify 两纯函数）抽到 `src/mobile/message-render.tsx` 的 `useMessageRenderers({ openFile, files })`，SessionDetail 与 SubagentDetail 共用——spec §三.5「不另造一套」的落点。代价：一次纯搬家重构（SessionDetail 既有测试守护）。
- **G. codex 清单回溯边界**：codex 子 rollout 的发现沿用既有 24h 窗 + 缓存命中倒排索引（`CODEX_IDX_SCAN`），不为此扩窗——AGENTS.md「无界历史目录扫描限 24h 新鲜窗口（L3）」是采集域预算契约（宪法层），优先于 spec §二.6 的「永久回溯」全称量词；claude（meta 登记源天然有界）/ opencode（SQL 按 parent 过滤有界）/ kimi（单会话 agents/ 目录有界）三家全量回溯无此张力。codex「重启后超窗完成名单缺失」申报为已知边界（§八）。
- **H. 通知打标挂在 digest 的 last_message 原文上（截断前）**：`claude_parser::read_claude_digest` 末尾用 `subagents::claude::is_subagent_report_text`（与判据同一套信号原语：`task_id_in` + `<teammate-message` 标记）打标 → `Session.last_message_subagent_report`。前端只消费布尔（useNotification 永不匹配文案），满足 spec §四.3「同源、不靠文案」。回报场景里「最后一条有文本消息」即回报本身（乙.3.4 实证），主会话自身动作（如自己 SendMessage 的回执复述）不含信号 → 不误伤。
- **I. 任务摘要回落链（2026-10-09 评审 P1-1 补留痕）**：卡片四要素之「它在做的任务」摘要 = meta.description → 子转写**首条 user 原文剥 teammate 壳截断 40 字**（`AgentTrack.first_user` 随增量读捕获，纯缓存物无时间叠加；teammate meta 实测无 description 字段——甲.2）。剥壳是**两语言两份实现**：Rust `strip_teammate_wrapper`（T2，卡片摘要，`claude_source_description_falls_back_to_first_user` 锁）与前端 `extractTaskText`（T6，详情任务原文区，T6 测试锁）——跨 JSON 边界各测各的，不强行共享（共享需端点透传首条 user 原文，载荷扩面不值）。

## 三、模块划分与文件结构（分解决策在此锁定）

```
src-tauri/src/monitor/subagents/
├── mod.rs            # SubagentView 增 status/end_ts；新增 SubagentStatus 枚举
├── claude.rs         # T1：teammate 信号检测 + 别名归一 + is_subagent_report_text；
│                     #   T2：全量出卡（status/endTs）；locate 提 pub(crate)（T4 用）
├── opencode.rs       # T2：全查 + status 分诊 + endTs=time_updated
├── kimi.rs           # T2：全目录收集 + status 分诊 + endTs=wire mtime
└── codex.rs          # T2：全出卡 + status 分诊 + endTs=task_complete 行 ts

src-tauri/src/remote/
├── content.rs        # T4：read_claude_subagent_messages(_with)——locate 复用 + map_claude_lines 复用
├── api.rs            # T3：session_subagents 契约注释更新；T4：session_subagent_messages handler
├── server.rs         # T4：SubagentMessageSourceFn + 字段 + 路由 + 契约测试；T8：观测日志
└── mod.rs            # T4：生产装配（仅 claude）

src-tauri/src/monitor/claude_parser.rs   # T7：digest 打标
src-tauri/src/session/model.rs           # T7：Session 增字段（横切，编译器驱动补桩）

src/mobile/
├── api.ts              # T3：SubagentView 类型扩展；T4：fetchSubagentMessages
├── SubagentChips.tsx   # T5：改纯展示 + 点击回调 + 导出 subagentElapsedMs
├── SubagentList.tsx    # T5：新组件——文件面板「子 Agent」卡区
├── FilePanel.tsx       # T5：增 subagents/onOpenSubagent 可选 props，卡区宿主
├── SessionDetail.tsx   # T5：列表拉取上提 + PreviewState 第三形 + 渲染分支；
│                       #   T6：渲染器改用共享 hook
├── message-render.tsx  # T6：新文件——useMessageRenderers（从 SessionDetail 纯搬家）
└── SubagentDetail.tsx  # T6：新组件——实时预览/定格快照（supported 态 / 自动刷新 / 折叠交互）

src/hooks/useNotification.ts   # T7：开关过滤
src/pages/settings.tsx         # T7：开关行
src/tauri-mock.ts              # T7：get_setting 补键
src/i18n/locales/{zh,en}.json  # T7：settings.notifications.* 新键
src/types/session.ts           # T7：Session TS 类型补字段

tests/mobile/SubagentChips.test.tsx     # T5：重写（纯展示）
tests/mobile/SubagentList.test.tsx      # T5：新
tests/mobile/SubagentDetail.test.tsx    # T6：新
tests/mobile/SessionDetail.test.tsx     # T5/T6：增列表拉取与点击跳转用例
tests/notification/subagentReportFilter.test.ts  # T7：新
```

**为何详情读取放 content.rs 而非 subagents/claude.rs**：subagents 模块是「运行判据域」（状态机/增量缓存），content.rs 是「消息内容读取域」（map_claude_lines / 预算 / MessagesPage 形态都在那里）；详情端点要的是后者，claude.rs 只贡献发现层（locate）。跨模块引用方向 `remote::content → monitor::subagents::claude::locate` 与既有 `remote → monitor` 方向一致。

**为何 SubagentList / SubagentDetail 各自成文件**：卡区与详情是两个独立交互面（面板内嵌区 vs 预览侧栏视图），与 FilePanel/FilePreview 的既有分工同构；都挂在 FilePanel/SessionDetail 里会让两文件超 500/1600 行。

## 四、实施顺序（TDD，8 Task；每 Task 一 commit，门禁全绿）

| Task | 内容（红→绿重点） | 依赖 | commit |
|---|---|---|---|
| T1 | claude 判据修复：teammate 信号检测（idle_notification）+ 名字→档案别名归一 + meta 扩展（taskKind/name）+ 回归夹具（teammate 完成/续跑/双形态/混跑 + 经典零改动） | — | `fix(subagent-observatory): claude teammate 机制完成识别——idle_notification 信号与名字归一` |
| T2 | 全量名单与终态：`SubagentView` 增 `status`/`endTs`；四工具 source 全量出卡（既有「隐藏」断言改「转 Idle」）；claude `last_ts` / codex `complete_ts` 捕获 | T1 | `feat(subagent-observatory): 子 agent 全量名单——status/endTs 终态字段（四工具）` |
| T3 | 清单端点契约扩展：假源/契约矩阵测试补 status/endTs 断言 + 全量语义注释；前端 `api.ts` 类型扩展 | T2 | `feat(subagent-observatory): /session-subagents 契约扩展——全量名单与终态载荷` |
| T4 | 详情端点 `/session-subagent-messages`：content.rs claude 子转写读取（locate 提权 + map_claude_lines 复用）+ RemoteState 新缝 + 生产装配（仅 claude）+ 契约矩阵 + 前端 fetch | T2 | `feat(subagent-observatory): /session-subagent-messages 详情端点（claude 复用消息映射）` |
| T5 | 前端清单：拉取上提 SessionDetail + `SubagentChips` 改纯展示（点击直达）+ `SubagentList` 卡区进 FilePanel + PreviewState 第三形 + 跳转回调 | T3 | `feat(subagent-observatory): 文件面板子 Agent 卡区与 chip 点击直达` |
| T6 | 详情视图：`message-render.tsx` 抽取（SessionDetail 纯搬家）+ `SubagentDetail`（supported 态/自动刷新起停/定格快照/任务原文/折叠交互）+ 渲染分支接线 | T4, T5 | `feat(subagent-observatory): 子 Agent 实时预览对话框——渲染复用与自动刷新` |
| T7 | 通知打标 + 开关：`is_subagent_report_text` 接 claude_parser digest → `Session` 字段（横切补桩）→ TS 类型/mock/i18n/设置行 → useNotification 过滤 + 用例 | T1 | `feat(subagent-observatory): 子 Agent 回报提醒开关与通知打标` |
| T8 | 收口：端点观测日志 + 全量门禁（含 check:i18n）+ 手工验收清单整理（spec §六） | T1–T7 | `docs(subagent-observatory): 收口——观测日志与真机验收清单` |

依赖链：T1 → T2 → {T3, T4}（可并行）→ T5（需 T3）→ T6（需 T4+T5）→ T7（仅依赖 T1 的谓词）→ T8。
每 Task 独立提交、独立验收（spec「实施批次」节）；T7 理论上可在 T2 后任意插入，计划排在 T6 后是为了让「清单/详情」的用户可见面先落地。

## 五、任务详情

### Task 1: claude 判据修复——teammate 机制事件分支

**Files:**
- Modify: `src-tauri/src/monitor/subagents/claude.rs`（meta 扩展 :380、新纯函数、classify stop 分支 :299、step/rebuild 归一 :166/:229、测试）

- [ ] **Step 1: 写失败测试**（claude.rs tests 模块追加；夹具按甲.4 事实构造）

```rust
    // ==== 2026-10-09 观察台 T1：teammate 机制（甲.4 对照表 · 甲.5 修复立项）====

    /// teammate 登记 meta（甲.4 机制 B 实录字段形态）：taskKind 标记 + 编队名 +
    /// name 字段 + spawnDepth:0；agentType 与 name 同值
    fn tm_meta(name: &str) -> String {
        format!(
            r#"{{"taskKind":"in_process_teammate","teamName":"session-tm","name":"{name}","agentType":"{name}","spawnDepth":0}}"#
        )
    }
    /// teammate 完成信号（甲.5 主判据）：user 行 content 含 teammate-message 包裹的
    /// idle_notification——只有名字没有编号（agentId 长串在父转写 0 次）
    fn tm_idle(name: &str) -> String {
        format!(
            r#"{{"type":"user","timestamp":"2026-10-08T15:02:38.000Z","message":{{"role":"user","content":"<teammate-message teammate_id=\"{name}\">{{\"type\":\"idle_notification\",\"from\":\"{name}\"}}</teammate-message>"}}}}"#
        )
    }
    /// teammate 完成报告（非 idle 的 teammate-message）：不构成 Stop（甲.5 待议不入本批）
    fn tm_report(name: &str) -> String {
        format!(
            r#"{{"type":"user","timestamp":"2026-10-08T15:02:26.000Z","message":{{"role":"user","content":"<teammate-message teammate_id=\"{name}\">任务已全部完成，结果 355/113</teammate-message>"}}}}"#
        )
    }
    /// SendMessage 续跑（to=名字形态；长 id 形态复用既有 sendmsg(id)）
    fn sendmsg_to(name: &str) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"2026-10-08T15:08:53.000Z","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"tu2","name":"SendMessage","input":{{"to":"{name}","body":"continue"}}}}]}}}}"#
        )
    }

    /// P0 修复锁（spec §一.4/§六.1）：teammate 完成即消失、续跑即复现——全链路
    #[test]
    fn teammate_source_lifecycle() {
        super::super::reset_cache_for_tests();
        let f = claude_fixture("tm-1");
        // 甲.4：agentId = 名字前缀长串（abg-progress-test-3b76c22694757d64 形态）
        f.add_agent(
            "tm-1",
            "abg-progress-test-3b76c22694757d64",
            &tm_meta("bg-progress-test"),
            &(agent_line("2026-10-08T14:55:56.488Z", 100, 200, 0, 50) + "\n"),
        );
        // ① 出现（spawn 态，名称 = meta.agentType）
        let v = collect_with(&f.root, "tm-1");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "bg-progress-test");
        // ② 完成报告（非 idle）不消失；空闲通告 → 消失（FAIL 修复主体）
        f.append(&f.parent("tm-1"), &(tm_report("bg-progress-test") + "\n"));
        assert_eq!(
            collect_with(&f.root, "tm-1").len(),
            1,
            "非 idle 的 teammate-message 不构成 Stop（甲.5 待议）"
        );
        f.append(&f.parent("tm-1"), &(tm_idle("bg-progress-test") + "\n"));
        assert!(
            collect_with(&f.root, "tm-1").is_empty(),
            "idle_notification → 完成即从活跃区消失"
        );
        // ③ 续跑：to=名字 → 复现（经同一名字→档案映射）
        f.append(&f.parent("tm-1"), &(sendmsg_to("bg-progress-test") + "\n"));
        assert_eq!(collect_with(&f.root, "tm-1").len(), 1, "续跑即复现");
        // ④ 二次完成（甲.2 实录两组 teammate-message/两次 idle）→ 再消失
        f.append(&f.parent("tm-1"), &(tm_idle("bg-progress-test") + "\n"));
        assert!(collect_with(&f.root, "tm-1").is_empty());
        // ⑤ Resume 双形态：to=长 id（agentId 本体）也命中——别名表 id 恒注册
        f.append(
            &f.parent("tm-1"),
            &(sendmsg("abg-progress-test-3b76c22694757d64") + "\n"),
        );
        assert_eq!(collect_with(&f.root, "tm-1").len(), 1, "to=长 id 经同一映射复现");
        super::super::reset_cache_for_tests();
    }

    /// 纯函数层：idle 行分类为 Stop（键=名字）；报告行 None；from 缺失回落 teammate_id（甲.5 辅助）
    #[test]
    fn classify_teammate_idle_line() {
        assert_eq!(
            classify_parent_line(&tm_idle("bg-progress-test")),
            Some(ParentEvent::Stop {
                agent_id: "bg-progress-test".to_string()
            })
        );
        assert_eq!(classify_parent_line(&tm_report("bg-progress-test")), None);
        // 辅助形态：无 from 字段、只有 teammate_id 属性 + idle 标记
        let aux = r#"{"type":"user","message":{"role":"user","content":"<teammate-message teammate_id=\"aux-agent\">{\"type\":\"idle_notification\"}</teammate-message>"}}"#;
        assert_eq!(
            classify_parent_line(aux),
            Some(ParentEvent::Stop { agent_id: "aux-agent".to_string() }),
            "from 缺失 → teammate_id 属性辅助匹配"
        );
    }

    /// 混跑（spec §一.4）：同会话经典 + teammate 各一，各自完成/续跑互不干扰
    #[test]
    fn teammate_and_classic_mixed() {
        super::super::reset_cache_for_tests();
        let f = claude_fixture("tm-2");
        f.add_agent("tm-2", "ae78013828b11ec80", r#"{"agentType":"Plan","toolUseId":"tu1","spawnDepth":1}"#, "");
        f.add_agent("tm-2", "abg-x-ef2d485433592526", &tm_meta("x-agent"), "");
        // 经典按 id 通知 → 消失；teammate 仍在板（名字空间互不污染）
        f.append(&f.parent("tm-2"), &(notify("ae78013828b11ec80") + "\n"));
        let v = collect_with(&f.root, "tm-2");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].id, "abg-x-ef2d485433592526");
        // teammate 按名字 idle → 也消失；经典不复活（id ≠ 名字）
        f.append(&f.parent("tm-2"), &(tm_idle("x-agent") + "\n"));
        assert!(collect_with(&f.root, "tm-2").is_empty());
        super::super::reset_cache_for_tests();
    }

    /// 重启重建（缓存缺失）也能归一：父文件已含 idle → 倒序重建后完成态正确判定
    #[test]
    fn teammate_rebuild_after_restart() {
        super::super::reset_cache_for_tests();
        let f = claude_fixture("tm-3");
        f.add_agent("tm-3", "abg-y-111111111111111", &tm_meta("y-agent"), "");
        f.append(&f.parent("tm-3"), &(tm_idle("y-agent") + "\n"));
        assert!(
            collect_with(&f.root, "tm-3").is_empty(),
            "重建路径同样经归一：名字键命中档案"
        );
        super::super::reset_cache_for_tests();
    }

    /// T7 消费的打标谓词（与判据同一套信号原语）：classic task-notification 与
    /// teammate-message 都算「子 agent 回报」；普通文本不算
    #[test]
    fn is_subagent_report_text_matrix() {
        assert!(is_subagent_report_text(&notify("a88")));
        assert!(is_subagent_report_text(&tm_idle("bg-progress-test")));
        assert!(is_subagent_report_text(&tm_report("bg-progress-test")));
        assert!(!is_subagent_report_text("用户自己发的消息"));
        assert!(!is_subagent_report_text("Another Claude session sent a message: 普通内容"));
    }
```

注意：`is_subagent_report_text` 对「Another Claude session sent a message: 普通内容」判 false——乙.3.4 待滤的 4 条 lastMessage 均带 `<teammate-message` 标记，普通前缀文本不构成信号；若上游未来出现「无标签的跨会话消息」，属上游文案形态变化，本谓据随判据层一起再取证（不靠文案匹配的边界如实申报）。

- [ ] **Step 2: 跑红**：`cd src-tauri && cargo test subagents::claude` → 编译失败（`is_subagent_report_text` 等未定义）。

- [ ] **Step 3: 实现**（claude.rs，全部纯函数）

meta 扩展（:380 `AgentMeta` 整体替换）：

```rust
/// subagents/agent-<id>.meta.json 内容（字段缺失容忍）。
/// **不解析 spawnDepth、不区分深度**（口径明示，评审 P3-5）：spec §3 只收一层
/// 子 agent——claude 的 subagents/ 登记源天然只有一层（深层派生不落此目录），
/// 其余三工具无深度信息可过滤。
/// 2026-10-09 观察台 T1（甲.4）：teammate 机制带 taskKind/name——taskKind 是
/// 机制分派标记（spec §一.1），name 是名字→档案归一的别名源（甲.5）。
#[derive(Deserialize)]
pub(crate) struct AgentMeta {
    #[serde(rename = "agentType")]
    pub(crate) agent_type: Option<String>,
    pub(crate) description: Option<String>,
    #[serde(rename = "taskKind", default)]
    pub(crate) task_kind: Option<String>,
    #[serde(default)]
    pub(crate) name: Option<String>,
}

/// teammate 机制标记值（甲.4：meta.taskKind == "in_process_teammate"）
pub(crate) const TASK_KIND_TEAMMATE: &str = "in_process_teammate";
```

新信号检测函数（放在 `task_id_in` 之后）：

```rust
/// 文本中提取 `<teammate-message teammate_id="名字" …>` 的属性值（甲.4 机制 B）
fn teammate_id_in(text: &str) -> Option<&str> {
    const OPEN: &str = "<teammate-message";
    const ATTR: &str = "teammate_id=\"";
    let tag = text.find(OPEN)?;
    let rel = text[tag..].find(ATTR)?;
    let start = tag + rel + ATTR.len();
    let end = start + text[start..].find('"')?;
    Some(&text[start..end])
}

/// teammate 完成信号（甲.5 主判据）：文本含 `{"type":"idle_notification","from":"<名字>"}`。
/// 提取顺序：idle 标记之后的 `"from":"…"` 值；from 缺失 → teammate_id 属性辅助
/// （甲.5 明示可作辅助匹配）。无 idle 标记 → None（普通 teammate-message 不算）。
pub(crate) fn teammate_idle_from(text: &str) -> Option<&str> {
    const MARK: &str = "\"idle_notification\"";
    const FROM: &str = "\"from\":\"";
    let mark = text.find(MARK)?;
    if let Some(rel) = text[mark..].find(FROM) {
        let start = mark + rel + FROM.len();
        let end = start + text[start..].find('"')?;
        return Some(&text[start..end]);
    }
    teammate_id_in(text)
}

/// lastMessage 是否为「子 agent 回报」（观察台 §四 提醒开关的打标谓词，T7 消费）。
/// 与判据同一套信号原语（task_id_in + teammate-message 标记）——spec §四.3
/// 「判定与修复项同源，不靠文案匹配」的落点：前端只看后端打出的布尔。
pub(crate) fn is_subagent_report_text(text: &str) -> bool {
    task_id_in(text).is_some() || text.contains("<teammate-message")
}
```

`classify_parent_line` 的 stop 分支（:317-322 区域，在 classic task-id 之后追加 teammate 分支；resume 检查保持在前）：

```rust
    // stop：任意消息文本含 task-id 通知块
    let text = message_text(&v);
    if let Some(id) = task_id_in(&text) {
        return Some(ParentEvent::Stop {
            agent_id: id.to_string(),
        });
    }
    // stop②（观察台 T1，甲.5）：teammate 空闲通告——只有名字没有编号，
    // 归一到档案 id 由调用方的别名表完成（键在此保持原始形态）
    if let Some(name) = teammate_idle_from(&text) {
        return Some(ParentEvent::Stop {
            agent_id: name.to_string(),
        });
    }
    None
```

别名归一（新函数，放 `registered_agents` 之后）：

```rust
/// 名字→档案 id 别名表（甲.5「名字→编号归一」，Stop/Resume 双向经同一映射）：
/// - 每个登记 agent 的 id 恒注册（经典机制事件只带 id；teammate 的 to=长 id 同走此键）；
/// - teammate（meta.taskKind 标记，spec §一.1 机制分派）额外注册 meta.name 别名；
/// - 键冲突（两个 teammate 同名）后者跳过——按 id 排序的注册序先到先得（申报边界）。
fn alias_map(reg: &[RegAgent]) -> HashMap<String, String> {
    let mut m: HashMap<String, String> = HashMap::new();
    for a in reg {
        m.entry(a.id.clone()).or_insert_with(|| a.id.clone());
        if a.meta.task_kind.as_deref() == Some(TASK_KIND_TEAMMATE) {
            if let Some(name) = a.meta.name.as_deref() {
                if !name.is_empty() {
                    m.entry(name.to_string()).or_insert_with(|| a.id.clone());
                }
            }
        }
    }
    m
}

/// 事件键归一：classify 产出的原始键（id 或名字）→ 登记档案 id。
/// 匹配不到登记集（如父会话向 team-lead 发消息——非本会话子 agent）→ None（忽略）
fn normalize_event(
    aliases: &HashMap<String, String>,
    ev: ParentEvent,
) -> Option<ParentEvent> {
    let raw = match &ev {
        ParentEvent::Stop { agent_id } | ParentEvent::Resume { agent_id } => agent_id,
    };
    let canonical = aliases.get(raw)?;
    Some(match ev {
        ParentEvent::Stop { .. } => ParentEvent::Stop {
            agent_id: canonical.clone(),
        },
        ParentEvent::Resume { .. } => ParentEvent::Resume {
            agent_id: canonical.clone(),
        },
    })
}
```

`step()` 接归一（:166 起，函数开头建表、事件循环归一）：

```rust
fn step(
    parent_jsonl: &Path,
    subagents_dir: &Path,
    reg: &[RegAgent],
    st: &mut ClaudeCache,
) -> Vec<SubagentView> {
    let aliases = alias_map(reg); // T1：名字→档案归一（经典 id 键恒等映射，行为不变）
    // ① 父文件事件（增量；reset → 清空状态机后按全量行重放）
    match read_increment(parent_jsonl, &mut st.parent) {
        IncrRead::Unchanged => {}
        IncrRead::Lines { lines, reset } => {
            if reset {
                st.last.clear();
            }
            for line in &lines {
                if let Some(ev) = classify_parent_line(line) {
                    if let Some(ev) = normalize_event(&aliases, ev) {
                        apply_event(&mut st.last, &ev);
                    }
                }
            }
        }
    }
    // …②③ 不变（本任务运行过滤语义不动，T2 再改全量出卡）
}
```

`rebuild()` visitor 同款归一（:229 起；`got` 计数用归一后的 id——`wanted` 集本就是档案 id 集）：

```rust
fn rebuild(parent_jsonl: &Path, reg: &[RegAgent], st: &mut ClaudeCache) {
    let wanted: HashSet<&str> = reg.iter().map(|a| a.id.as_str()).collect();
    let aliases = alias_map(reg);
    let mut got: HashSet<String> = HashSet::new();
    let tail_partial = scan_parent_rev(parent_jsonl, |line| {
        if let Some(ev) = classify_parent_line(line) {
            if let Some(ev) = normalize_event(&aliases, ev) {
                let id = match &ev {
                    ParentEvent::Stop { agent_id } | ParentEvent::Resume { agent_id } => {
                        agent_id.clone()
                    }
                };
                apply_event_rev(&mut st.last, &ev);
                got.insert(id);
            }
        }
        wanted.len() == got.len()
    })
    .unwrap_or_default();
    // …游标对齐尾部半行（既有代码不变）
}
```

- [ ] **Step 4: 跑绿**：`cd src-tauri && cargo test subagents` → PASS（**含既有全部用例零改动**——经典机制零改动的回归锁：`claude_source_lifecycle` / `stop_then_sendmessage_is_running_again` / `classify_ignores_irrelevant_lines` 等原样通过）。

- [ ] **Step 5: 门禁 + commit**：

```bash
cd src-tauri && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check
git add src-tauri/src/monitor/subagents/claude.rs
git commit -m "fix(subagent-observatory): claude teammate 机制完成识别——idle_notification 信号与名字归一"
```

### Task 2: 全量名单与终态字段（四工具 source）

**Files:**
- Modify: `src-tauri/src/monitor/subagents/mod.rs`（`SubagentStatus` 枚举 + `SubagentView` 两字段）
- Modify: `src-tauri/src/monitor/subagents/claude.rs`（`AgentTrack.last_ts` + 全量出卡 + 断言更新）
- Modify: `src-tauri/src/monitor/subagents/opencode.rs` / `kimi.rs` / `codex.rs`（status 分诊 + 断言更新）

- [ ] **Step 1: 写失败测试**

mod.rs tests 追加：

```rust
    /// SubagentStatus 序列化为小写单词（端点载荷契约，T3 契约矩阵的单元层锁）
    #[test]
    fn subagent_status_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&SubagentStatus::Running).unwrap(),
            "\"running\""
        );
        assert_eq!(
            serde_json::to_string(&SubagentStatus::Idle).unwrap(),
            "\"idle\""
        );
    }
```

claude.rs tests 追加（并把既有「消失 = is_empty」断言改写为「转 Idle 在板」）：

```rust
    /// T2 任务摘要回落（spec §二.2 四要素）：meta.description 缺失（teammate meta
    /// 实测无该字段）→ 首条 user 文本截断充当「它在做的任务」摘要；有 description
    /// 时 meta 优先
    #[test]
    fn claude_source_description_falls_back_to_first_user() {
        super::super::reset_cache_for_tests();
        let f = claude_fixture("st-2");
        let first_user = r#"{"timestamp":"2026-10-08T14:55:56.488Z","type":"user","message":{"role":"user","content":"<teammate-message teammate_id=\"team-lead\">设计一个两改动的实现方案并给出取舍</teammate-message>"}}"#;
        std::fs::write(
            f.session("st-2").join("subagents").join("agent-at1.jsonl"),
            first_user + "
",
        )
        .unwrap();
        std::fs::write(
            f.session("st-2").join("subagents").join("agent-at1.meta.json"),
            &tm_meta("t1"),
        )
        .unwrap();
        let v = collect_with(&f.root, "st-2");
        assert!(
            v[0].description.as_deref().unwrap_or("").contains("设计一个两改动的实现方案"),
            "无 description 的 teammate：摘要回落首条 user（剥壳截断）"
        );
        // 有 description 的经典 meta：meta 优先（既有 claude_source_lifecycle 已锁）
        super::super::reset_cache_for_tests();
    }

    /// T2 终态锚：idle 的 endTs = 子转写末行 timestamp（停写即冻结）；running 恒 None
    #[test]
    fn claude_source_end_ts_is_transcript_last_ts() {
        super::super::reset_cache_for_tests();
        let f = claude_fixture("st-1");
        f.add_agent(
            "st-1",
            "a1",
            r#"{"agentType":"Plan"}"#,
            &format!(
                "{}\n{}\n",
                agent_line("2026-10-08T07:00:00Z", 1, 0, 0, 1),
                agent_line("2026-10-08T07:05:00Z", 1, 0, 0, 1)
            ),
        );
        // 运行中：全量出卡 + status=Running + endTs=None
        let v = collect_with(&f.root, "st-1");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].status, SubagentStatus::Running);
        assert_eq!(v[0].end_ts, None);
        // 完成 → status=Idle + endTs=末行 ts（07:05，不是首行）
        f.append(&f.parent("st-1"), &(notify("a1") + "\n"));
        let v = collect_with(&f.root, "st-1");
        assert_eq!(v.len(), 1, "全量名单：完成后仍在板（观察台 §二.3）");
        assert_eq!(v[0].status, SubagentStatus::Idle);
        assert_eq!(v[0].end_ts.as_deref(), Some("2026-10-08T07:05:00Z"));
        // 续跑 → 转回 Running、endTs 清空
        f.append(&f.parent("st-1"), &(sendmsg("a1") + "\n"));
        let v = collect_with(&f.root, "st-1");
        assert_eq!(v[0].status, SubagentStatus::Running);
        assert_eq!(v[0].end_ts, None);
        super::super::reset_cache_for_tests();
    }
```

既有用例断言改写清单（**语义变化主体**：v1「完成后不出卡」→「转 Idle 在板」）：

- `claude_source_lifecycle` ②：`assert!(collect_with(...).is_empty())` → `assert_eq!(collect_with(...)[0].status, SubagentStatus::Idle)`；③ 复现后补 `assert_eq!(v[0].status, SubagentStatus::Running)`；④ token 增量断言照旧（idle 后转写追加 → token 变化可断言在 Running 复现之后，原序不变）。
- `claude_source_rebuild_after_restart`：`v.len()==1` → `v.len()==2`（b1 Idle + b2 Running），补 `assert_eq!(v.iter().find(|s| s.id=="b1").unwrap().status, SubagentStatus::Idle)`。
- `claude_source_rebuild_consumes_tail_partial_line`：终态 `is_empty()` → `len()==1 && status==Idle`。
- `claude_source_parent_truncated_rebuilds`：护栏后 `len()==1` 补 `status==Running`；护栏前 `is_empty()` → `len()==1 && status==Idle`。
- T1 的 `teammate_source_lifecycle` / `teammate_and_classic_mixed` / `teammate_rebuild_after_restart`：同款把 `is_empty()`/`len()` 断言改为 status 断言（消失语义移交前端 chip 过滤）。
- opencode `active_boundary_expires_at_90s`（opencode.rs:225，期望恰好 `["c89"]`）：c90 从「被过滤」变「在板 Idle」——期望改为两条 `["c89", "c90"]`（c89 Running / c90 Idle，time_created 升序，评审 P2-2）。
- opencode `stale_child_hidden_v1_and_missing_db_empty`（opencode.rs:248，断言超窗 `is_empty()`）：超窗断言改 `len()==1 && status==Idle && endTs==ms_to_iso(now-200_000)`；测试名「超窗隐藏」语义已过时，**一并改名** `stale_child_marks_idle_v1_and_missing_db_empty`（评审 P2-2；v1 库/库缺失两段空态断言不变）。

opencode.rs / kimi.rs / codex.rs 各自新增/改写（示例为 opencode，kimi/codex 同构改写）：

```rust
    /// T2：全量名单 + status 分诊（v1「超窗 continue 隐藏」→「转 Idle 冻结在板」）
    #[test]
    fn opencode_stale_child_marks_idle_with_end_ts() {
        // …make_db/ins 夹具照既有用例…
        ins(&conn, "c1", Some("m"), Some("a"), 10, 20, 30, 40, now - 100_000, now - 90_000);
        drop(conn);
        let v = collect_with(&db, "m", now);
        assert_eq!(v.len(), 1, "超窗不隐藏：留在名单原位");
        assert_eq!(v[0].status, SubagentStatus::Idle);
        assert_eq!(v[0].end_ts, ms_to_iso(now - 90_000), "endTs = time_updated");
    }
```

kimi：`kimi_source_scans_agents_dir` 末尾的超窗断言 `is_empty()` → `len()==1 && status==Idle && end_ts==mtime ISO`；codex：`codex_source_finds_child_by_parent_thread` 的 task_complete 后 `is_empty()` → `len()==1 && status==Idle && end_ts==Some("2026-10-08T08:05:00Z")`（`complete_line("2026-10-08T08:05:00Z")` 已带该 ts）。

- [ ] **Step 2: 跑红**：`cd src-tauri && cargo test subagents` → 编译失败（`SubagentStatus` 未定义）+ 断言失败。

- [ ] **Step 3: 实现**

mod.rs（`SubagentView` 定义处整体替换 + 新枚举）：

```rust
/// 子 agent 运行状态（观察台 §二.3 清单卡绿/灰点）：running=活跃（时长/token 走字）、
/// idle=不活跃（已完成/已停止——数据冻结原位）
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SubagentStatus {
    Running,
    Idle,
}

/// 端点载荷单条（spec §5.2 + 观察台 §二）。spawnTs=None：首条时间戳尚未落盘
/// （spawn 竞态，下轮自愈，前端不显示时长只显 token）。
/// 2026-10-09 T2：**全量名单**（含终态）——运行过滤移交前端 chip 层；
/// endTs = 终态锚（ISO）：运行中恒 None；不活跃 = 最后已知活动/完成时刻
/// （claude=子转写末行 ts / opencode=time_updated / kimi=wire mtime /
/// codex=task_complete 行 ts）——前端冻结时长锚：elapsed = (endTs ?? now) − spawnTs。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentView {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub spawn_ts: Option<String>,
    pub tokens: TokenUsage,
    pub status: SubagentStatus,
    pub end_ts: Option<String>,
}
```

claude.rs（`AgentTrack` 增字段 + `accumulate_agent_line` 增参 + `step()` ③ 全量出卡）：

```rust
#[derive(Default, Clone)]
struct AgentTrack {
    incr: IncrState,
    tokens: TokenUsage,
    first_ts: Option<String>,
    /// T2：末行 timestamp——idle 终态锚（停写即冻结；resume 后随新行更新但 status
    /// 转 Running → endTs 不再消费它）
    last_ts: Option<String>,
    /// T2（决策 I）：首条 user 原文——meta.description 缺失时的任务摘要回落源
    /// （spec §二.2 四要素：teammate meta 实测无 description 字段，甲.2）
    first_user: Option<String>,
}

/// 剥 `<teammate-message …>内文</teammate-message>` 壳（乙.3.1 teammate 首条
/// user 形态）；非包裹/残缺形态原样返回（不猜）。
/// **两语言两份实现申报（决策 I）**：与前端 SubagentDetail 的 extractTaskText
/// 同构不同语言（卡片摘要在后端算 / 详情任务原文在前端算），跨 JSON 边界
/// 各测各的——共享需端点透传首条 user 原文（载荷扩面），不值。
fn strip_teammate_wrapper(text: &str) -> &str {
    if !text.starts_with("<teammate-message") {
        return text;
    }
    let Some(open) = text.find('>') else {
        return text;
    };
    let Some(close) = text.rfind("</teammate-message>") else {
        return text;
    };
    if close > open {
        text[open + 1..close].trim()
    } else {
        text
    }
}

/// 任务摘要回落（决策 I）：首条 user 剥壳后按字符截断 40 + 省略号；空文本 → None
fn first_user_summary(text: &str) -> Option<String> {
    let stripped = strip_teammate_wrapper(text);
    if stripped.trim().is_empty() {
        return None;
    }
    let chars: Vec<char> = stripped.chars().take(41).collect();
    Some(if chars.len() > 40 {
        format!("{}…", chars[..40].iter().collect::<String>())
    } else {
        chars.into_iter().collect()
    })
}

/// 子 agent jsonl 单行累计（纯函数）：usage 四桶累加 + 首/末 timestamp 捕获 +
/// 首条 user 原文捕获（任务摘要回落源，决策 I）
pub(crate) fn accumulate_agent_line(
    line: &str,
    acc: &mut TokenUsage,
    first_ts: &mut Option<String>,
    last_ts: &mut Option<String>,
    first_user: &mut Option<String>,
) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return;
    };
    if let Some(ts) = v["timestamp"].as_str() {
        if first_ts.is_none() {
            *first_ts = Some(ts.to_string());
        }
        *last_ts = Some(ts.to_string());
    }
    if first_user.is_none() && v["type"].as_str() == Some("user") {
        match &v["message"]["content"] {
            serde_json::Value::String(s) if !s.trim().is_empty() => {
                *first_user = Some(s.clone());
            }
            serde_json::Value::Array(blocks) => {
                let joined: String = blocks
                    .iter()
                    .filter(|b| b["type"].as_str() == Some("text"))
                    .filter_map(|b| b["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("");
                if !joined.trim().is_empty() {
                    *first_user = Some(joined);
                }
            }
            _ => {}
        }
    }
    // …usage 四桶累计（既有代码不变）
}
```

`step()` 的 ② 调用点补第 4、5 参（last_ts / first_user 汇）、reset 分支补 `track.last_ts = None` 与 `track.first_user = None`；③ 整体替换为：

```rust
    // ③ 视图：全量名单（T2）——登记即出卡，status 由事件序状态机分诊
    //（is_running：None/Spawn/Resume → Running，Stop → Idle）
    let mut views: Vec<SubagentView> = reg
        .iter()
        .map(|a| {
            let t = st.agents.get(&a.id);
            let running = is_running(true, st.last.get(&a.id).copied());
            SubagentView {
                id: a.id.clone(),
                name: a
                    .meta
                    .agent_type
                    .clone()
                    .unwrap_or_else(|| format!("agent-{}", a.id)),
                // 任务摘要（决策 I，spec §二.2 四要素）：meta.description 优先；
                // 缺失（teammate meta 实测无该字段）→ 首条 user 剥壳截断 40 字回落
                description: a
                    .meta
                    .description
                    .clone()
                    .or_else(|| t.and_then(|t| t.first_user.as_deref()).and_then(first_user_summary)),
                spawn_ts: t.and_then(|t| t.first_ts.clone()),
                tokens: t.map(|t| t.tokens).unwrap_or_default(),
                status: if running {
                    SubagentStatus::Running
                } else {
                    SubagentStatus::Idle
                },
                end_ts: if running {
                    None
                } else {
                    t.and_then(|t| t.last_ts.clone())
                },
            }
        })
        .collect();
    sort_views(&mut views);
    views
```

`accumulate_usage_and_first_ts` 既有测试：两处调用补 `&mut None` 第 4、5 参（last_ts / first_user 汇），末尾补 `assert_eq!(last.as_deref(), Some("2026-10-08T07:37:00Z"), "末行 ts 持续覆盖")`；`step()` 的 ② 调用点与 reset 分支同步补第 5 参 / `track.first_user = None`。

opencode.rs：删除活跃 `continue`，行构造改为：

```rust
            let active = now_ms.saturating_sub(updated) < (ACTIVE_SECS as i64) * 1000;
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
                status: if active {
                    SubagentStatus::Running
                } else {
                    SubagentStatus::Idle
                },
                end_ts: if active { None } else { ms_to_iso(updated) },
            });
```

kimi.rs：候选收集去掉 `active_within` 门控（保留 mtime 值），视图构造：

```rust
            let active = active_within(*mtime, now);
            views.push(SubagentView {
                id: name.clone(),
                name: name.clone(), // v1：目录名（C1 校准后换真名）
                description: None,
                spawn_ts: track.created.clone(),
                tokens: track.tokens,
                status: if active {
                    SubagentStatus::Running
                } else {
                    SubagentStatus::Idle
                },
                end_ts: if active {
                    None
                } else {
                    systemtime_to_iso(*mtime)
                },
            });
```

kimi.rs 私有小件：

```rust
/// SystemTime → ISO（kimi endTs 用；mtime 无完成事件语义，长静默转灰时锚在静默起点——
/// 与 90s 窗口径一致，如实申报）
fn systemtime_to_iso(t: SystemTime) -> Option<String> {
    t.duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| super::ms_to_iso(d.as_millis() as i64))
}
```

codex.rs：`RolloutAccum` 增 `pub(crate) complete_ts: Option<String>`（Default 同款补 `None`）；`apply_rollout_line` 置位分支捕获行 ts：

```rust
    if v["type"].as_str() == Some("event_msg")
        && v["payload"]["type"].as_str() == Some("task_complete")
    {
        acc.task_complete_seen = true;
        if let Some(ts) = v["timestamp"].as_str() {
            acc.complete_ts = Some(ts.to_string());
        }
    }
```

视图构造去掉 `if !track.acc.task_complete_seen` 门控：

```rust
            let done = track.acc.task_complete_seen;
            views.push(SubagentView {
                id: meta.child_id.clone(),
                name: meta.child_id.chars().take(8).collect(), // 无名源（C3 校准点）
                description: None,
                spawn_ts: meta.spawn_ts.clone(),
                tokens: track.acc.tokens,
                status: if done {
                    SubagentStatus::Idle
                } else {
                    SubagentStatus::Running
                },
                end_ts: if done {
                    track.acc.complete_ts.clone()
                } else {
                    None
                },
            });
```

- [ ] **Step 4: 跑绿**：`cd src-tauri && cargo test subagents` → PASS（含全部改写断言）。

- [ ] **Step 5: 门禁 + commit**：

```bash
cd src-tauri && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check
git add src-tauri/src/monitor/subagents/
git commit -m "feat(subagent-observatory): 子 agent 全量名单——status/endTs 终态字段（四工具）"
```

### Task 3: 清单端点契约扩展 + 前端类型

**Files:**
- Modify: `src-tauri/src/remote/api.rs:590`（`session_subagents` 文档注释更新——运行中 → 全量名单）
- Modify: `src-tauri/src/remote/server.rs:14426`（假源补 status/endTs）+ `:14500`（契约矩阵断言）
- Modify: `src/mobile/api.ts:1480`（TS 类型扩展）

- [ ] **Step 1: 写失败测试**（server.rs `state_with_subagent_fakes` 两条目补字段 + 契约矩阵 ⑤ 段追加断言）

假源条目（late = Running / early = Idle——顺带锁「running 与 idle 都返回」的全量语义）：

```rust
                    crate::monitor::subagents::SubagentView {
                        id: format!("late-{sid}"),
                        name: "Plan".into(),
                        description: Some("设计新建会话两改动实现方案".into()),
                        spawn_ts: Some("2026-10-08T09:00:00.000+00:00".into()),
                        tokens: crate::monitor::subagents::TokenUsage {
                            input: 5124,
                            cache_read: 56448,
                            cache_creation: 0,
                            output: 9312,
                        },
                        status: crate::monitor::subagents::SubagentStatus::Running,
                        end_ts: None,
                    },
                    crate::monitor::subagents::SubagentView {
                        id: format!("early-{sid}"),
                        name: "Explore".into(),
                        description: None,
                        spawn_ts: None, // spawn 竞态：无时长
                        tokens: crate::monitor::subagents::TokenUsage::default(),
                        status: crate::monitor::subagents::SubagentStatus::Idle,
                        end_ts: Some("2026-10-08T08:59:00.000+00:00".into()),
                    },
```

契约矩阵 ⑤ 段（`assert_eq!(arr[1]["spawnTs"], serde_json::Value::Null);` 之后）追加：

```rust
        // ⑤b（观察台 §二）：全量名单——idle 也返回；status 小写单词；endTs camelCase
        assert_eq!(arr[0]["status"], "running", "活跃条目照常返回");
        assert_eq!(arr[0]["endTs"], serde_json::Value::Null);
        assert_eq!(arr[1]["status"], "idle", "终态条目不再被端点过滤（全量名单）");
        assert_eq!(arr[1]["endTs"], "2026-10-08T08:59:00.000+00:00");
```

- [ ] **Step 2: 跑红**：`cd src-tauri && cargo test session_subagents` → 编译失败（假源缺字段）。

- [ ] **Step 3: 实现**：假源补字段即编译通过（载荷结构 T2 已扩）；`api.rs` 的 handler 文档注释更新（只改注释，逻辑零改动——排序/no-store/400 不变）：

```rust
/// GET /m/api/v1/session-subagents?agent_type=&session_id=（spec §5.2 + 观察台 §二）
/// `{subagents: [{id, name, description, spawnTs, tokens{…}, status, endTs}]}`
/// - **全量名单**（2026-10-09 T3）：含终态条目（status=idle）——运行过滤移交前端
///   chip 层（清单卡区消费全量）；按 spawnTs 升序（None 排尾）= 派发序（新派发附末尾）；
/// - 端点是空态唯一权威（会话不存在/未知 agent_type/source 未装配 → []）；
/// - 缺参/空白 → 400；Cache-Control: no-store；IO 全 spawn_blocking（既有不变）。
```

前端 `src/mobile/api.ts:1480` 类型扩展：

```ts
/** GET /session-subagents 载荷单条（与 Rust monitor::subagents::SubagentView 的
 *  camelCase 序列化逐字段对应，勿漂移）。spawnTs=null：首条时间戳未落盘（spawn
 *  竞态，下轮自愈）——前端不显示时长只显 token。
 *  2026-10-09 观察台：**全量名单**（含终态）——status=running → chip 活跃区；
 *  idle → 清单卡灰点冻结（endTs = 冻结锚，elapsed = (endTs ?? now) − spawnTs）。 */
export interface SubagentView {
  id: string;
  name: string;
  description: string | null;
  spawnTs: string | null;
  tokens: { input: number; cacheRead: number; cacheCreation: number; output: number };
  status: "running" | "idle";
  endTs: string | null;
}
```

（`fetchSessionSubagents` 返回 `SubagentView[]` 不变。）

- [ ] **Step 4: 跑绿**：`cd src-tauri && cargo test session_subagents` → PASS；`pnpm vitest run tests/mobile` → 既有前端测试仍绿（既有桩返回 `{subagents: []}`，缺新字段的空列表不受影响）。

- [ ] **Step 5: 门禁 + commit**：

```bash
cd src-tauri && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check
pnpm vitest run tests/mobile && pnpm build:mobile && pnpm format:check && pnpm lint
git add src-tauri/src/remote/api.rs src-tauri/src/remote/server.rs src/mobile/api.ts
git commit -m "feat(subagent-observatory): /session-subagents 契约扩展——全量名单与终态载荷"
```

### Task 4: 详情端点 /session-subagent-messages

**Files:**
- Modify: `src-tauri/src/remote/content.rs`（新增 `read_claude_subagent_messages(_with)`，置于 claude 段 `map_claude_lines` 之后）
- Modify: `src-tauri/src/monitor/subagents/claude.rs:28`（`fn locate` → `pub(crate) fn locate`，一行）
- Modify: `src-tauri/src/remote/server.rs`（`SubagentMessageSourceFn` 类型别名 :468 区 + `RemoteState` 字段 :587 区 + 路由 :617 区 + 20 处测试装配补桩 + 新契约测试）
- Modify: `src-tauri/src/remote/api.rs`（`session_subagent_messages` handler，置于 `session_subagents` 之后）
- Modify: `src-tauri/src/remote/mod.rs:385`（生产装配，仅 claude）
- Modify: `src/mobile/api.ts`（`SubagentMessagesPage` + `fetchSubagentMessages`）

- [ ] **Step 1: 写失败测试**

content.rs tests 追加（tempdir 夹具，复用既有 claude 测试的目录构造惯例）：

```rust
    // ==== 2026-10-09 观察台 T4：claude 子 agent 详情读取 ====

    /// 子转写 → SessionMessage（map_claude_lines 复用：thinking/tool_use/tool_result
    /// 全量映射；attachment 行无 message.content 天然跳过——乙.3.1 噪声过滤免费继承）
    #[test]
    fn claude_subagent_messages_maps_transcript() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".claude/projects/-Users-x-demo/s1/subagents");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("agent-abg-test-1111.jsonl"),
            concat!(
                r#"{"type":"user","timestamp":"2026-10-08T14:55:56.488Z","message":{"role":"user","content":"<teammate-message teammate_id=\"team-lead\">设计新方案</teammate-message>"}}"#, "\n",
                r#"{"type":"attachment","timestamp":"2026-10-08T14:55:57Z","attachment":{"path":"x"}} "#, "\n",
                r#"{"type":"assistant","timestamp":"2026-10-08T14:56:10Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"先看目录"},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}"#, "\n",
                r#"{"type":"user","timestamp":"2026-10-08T14:56:12Z","message":{"role":"user","content":[{"type":"tool_result","content":"file-a\nfile-b"}]}}"#, "\n",
            ),
        )
        .unwrap();
        let pg = read_claude_subagent_messages_with(tmp.path(), "s1", "abg-test-1111", 200)
            .unwrap();
        let kinds: Vec<&str> = pg.messages.iter().map(|m| m.kind.as_str()).collect();
        assert_eq!(kinds, ["user", "thinking", "tool-call", "tool-result"]);
        assert_eq!(pg.messages[0].content, "<teammate-message teammate_id=\"team-lead\">设计新方案</teammate-message>");
        assert_eq!(pg.messages[2].tool_name.as_deref(), Some("Bash"));
        assert!(!pg.truncated);
    }

    /// 会话/转写不存在 → Err（端点映射 404）；subagent_id 穿越形态由端点 400 拦截
    #[test]
    fn claude_subagent_messages_missing_is_err() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(read_claude_subagent_messages_with(tmp.path(), "nope", "x", 200).is_err());
        let dir = tmp.path().join(".claude/projects/-Users-x-demo/s2/subagents");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(
            read_claude_subagent_messages_with(tmp.path(), "s2", "ghost", 200).is_err(),
            "会话在、转写不在 → Err"
        );
    }
```

server.rs 新契约测试（假源装配：复制 `state_with_subagent_fakes` 模式，subagent_message_source 注入 claude 假源）：

```rust
    /// GET /session-subagent-messages 契约矩阵（观察台 §三）：缺参 400 / 穿越 400 /
    /// 未装配工具 supported:false / claude 假源消息形状 / no-store / 无 cookie 403
    #[tokio::test]
    async fn session_subagent_messages_contract_matrix() {
        let mut fakes: std::collections::HashMap<&'static str, Box<SubagentMessageSourceFn>> =
            std::collections::HashMap::new();
        fakes.insert(
            "claude",
            Box::new(|_sid: &str, _sub: &str, _limit: usize| {
                Ok(crate::remote::content::MessagesPage {
                    messages: vec![crate::remote::content::SessionMessage {
                        seq: 0,
                        role: "user".into(),
                        kind: "user".into(),
                        content: "设计新方案".into(),
                        ts: Some(1760000000000),
                        tool_name: None,
                        tool_args: None,
                        collapsed: false,
                    }],
                    truncated: false,
                })
            }),
        );
        // ↓ 复制 test_state() 完整字面量（逐字段同值），仅 subagent_message_source 不同
        let st = state_with_subagent_msg_fakes(fakes); // 组装 helper：同 state_with_subagent_fakes 模式
        let app = crate::remote::server::router(st.clone());
        let cookie = paired_cookie(&st, "sub-det-1");
        // ① 缺 subagent_id → 400
        let r = app.clone().oneshot(req("GET",
            "/m/api/v1/session-subagent-messages?agent_type=claude&session_id=s1",
            Some(&cookie), None)).await.unwrap();
        assert_eq!(r.status(), 400);
        // ② subagent_id 穿越 → 400（该 id 直接拼 subagents/agent-<id>.jsonl 文件名）
        let r = app.clone().oneshot(req("GET",
            "/m/api/v1/session-subagent-messages?agent_type=claude&session_id=s1&subagent_id=..%2F..%2Fsecret",
            Some(&cookie), None)).await.unwrap();
        assert_eq!(r.status(), 400);
        // ③ 未装配工具（opencode）→ 200 supported:false 空表（「暂不支持」是正常态）
        let r = app.clone().oneshot(req("GET",
            "/m/api/v1/session-subagent-messages?agent_type=opencode&session_id=s1&subagent_id=x",
            Some(&cookie), None)).await.unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["supported"], false);
        assert_eq!(v["messages"].as_array().map(Vec::len), Some(0));
        // ④ claude 假源：消息形状 + supported:true + no-store
        let r = app.clone().oneshot(req("GET",
            "/m/api/v1/session-subagent-messages?agent_type=claude&session_id=s1&subagent_id=a1",
            Some(&cookie), None)).await.unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(r.headers().get("cache-control").unwrap(), "no-store");
        let v: serde_json::Value = serde_json::from_str(&body_string(r).await).unwrap();
        assert_eq!(v["supported"], true);
        assert_eq!(v["messages"][0]["kind"], "user");
        assert_eq!(v["messages"][0]["content"], "设计新方案");
        // ⑤ 无 cookie → 403（gate 结构性覆盖）
        let r = app.clone().oneshot(req("GET",
            "/m/api/v1/session-subagent-messages?agent_type=claude&session_id=s1&subagent_id=a1",
            None, None)).await.unwrap();
        assert_eq!(r.status(), 403);
    }
```

（`state_with_subagent_msg_fakes` 的组装 = 复制 `state_with_subagent_fakes` 字面量并把两处 source 都置缺省/假源——仓内既有「每用例组持有字面量」惯例。）

- [ ] **Step 2: 跑红**：`cd src-tauri && cargo test subagent_messages` → 编译失败。

- [ ] **Step 3: 实现**

content.rs（claude 段末尾新增）：

```rust
/// 子 agent 详情（观察台 §三，仅 claude）：会话 subagents/agent-<id>.jsonl 尾窗 →
/// `map_claude_lines` 复用（与会话消息同一映射——乙.3.1 实证两机制子转写格式一致，
/// 单一渲染器通吃）。发现层复用 `subagents::claude::locate`（同一 projects 扫描，
/// 不造第二份路径推导）；预算与 session-messages 同口径（line_budget/byte_budget）。
pub fn read_claude_subagent_messages(
    session_id: &str,
    subagent_id: &str,
    limit: usize,
) -> Result<MessagesPage, String> {
    let home = dirs::home_dir().ok_or_else(|| "无法确定用户主目录".to_string())?;
    read_claude_subagent_messages_with(&home, session_id, subagent_id, limit)
}

/// 注入核（测试 tempdir home）。**limit clamp 在本函数入口**（评审 P1-2）：既有
/// clamp 在 read_session_messages_impl（content.rs:335）的派发核内，本新路径不
/// 经它——不自带的话 limit=0 会产出空消息表；与派发核同口径 [1,1000]。
pub(crate) fn read_claude_subagent_messages_with(
    home: &Path,
    session_id: &str,
    subagent_id: &str,
    limit: usize,
) -> Result<MessagesPage, String> {
    let limit = limit.clamp(1, 1000);
    let projects = home.join(".claude").join("projects");
    let Some((_parent_jsonl, subagents_dir)) =
        crate::monitor::subagents::claude::locate(&projects, session_id)
    else {
        return Err(format!("会话不存在: {session_id}"));
    };
    let path = subagents_dir.join(format!("agent-{subagent_id}.jsonl"));
    if !path.is_file() {
        return Err(format!("子 agent 转写不存在: {subagent_id}"));
    }
    let (lines, truncated) = crate::monitor::jsonl::read_recent_lines_with_budget(
        &path,
        line_budget(limit),
        byte_budget(limit),
    );
    Ok(page(map_claude_lines(&lines), limit, truncated))
}
```

（subagent_id 的穿越防御在端点入口做——locate 只守 session_id；content 层 fail-closed 由 `path.is_file()` 兜底。）

claude.rs:28：`fn locate` → `pub(crate) fn locate`（文档注释补一行「T4 起 remote::content 详情读取复用」）。

server.rs（类型别名 + 字段 + 路由）：

```rust
/// 子 agent 详情源注入缝（观察台 §三）：键 = agent_type；闭包 (session_id,
/// subagent_id, limit) → MessagesPage。生产仅 claude；未装配 → 端点回
/// supported:false（其余工具转写格式未普查，spec §三.6 如实申报——清单照常）。
pub type SubagentMessageSourceFn =
    dyn Fn(&str, &str, usize) -> Result<crate::remote::content::MessagesPage, String>
        + Send + Sync;
```

```rust
// RemoteState 字段（subagent_source 之后）：
/// 子 agent 详情源注入缝（观察台 §三）：见 SubagentMessageSourceFn 文档。
/// 空表 = 一切工具 supported:false（测试缺省）
pub subagent_message_source:
    std::collections::HashMap<&'static str, Box<SubagentMessageSourceFn>>,
```

```rust
        // /session-subagent-messages（2026-10-09 观察台 §三）：子 agent 执行过程
        // 详情（claude 复用消息映射；其余工具 supported=false 如实申报）
        .route("/session-subagent-messages", get(api::session_subagent_messages))
```

api.rs handler（`session_subagents` 之后）：

```rust
/// GET /m/api/v1/session-subagent-messages?agent_type=&session_id=&subagent_id=&limit=
/// （观察台 §三）`{messages: SessionMessage[], truncated: bool, supported: bool}`
/// - 载荷与 /session-messages 同形（SessionMessage 逐字段同构）+ supported 标志
///   ——前端消息渲染器零适配；
/// - source 未装配（opencode/kimi/codex）→ 200 `{messages:[], truncated:false,
///   supported:false}`（「暂不支持查看详情」是正常态不是错误，不给 404）；
/// - claude 读取失败（会话/转写不存在）→ 404（session_messages 同映射：细节只进日志）；
/// - 缺参/空白 → 400；subagent_id 含 `/`、`\`、`..`、`\0` → 400（该 id 直接拼
///   `subagents/agent-<id>.jsonl` 文件名——content.rs 会话守卫同款）；
/// - limit 缺省 200（clamp [1,1000] 在注入核 `read_claude_subagent_messages_with`
///   入口——本端点不经 session-messages 的派发核，评审 P1-2）；no-store；spawn_blocking。
pub async fn session_subagent_messages(
    State(st): State<Arc<RemoteState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Response, StatusCode> {
    let need = |k: &str| {
        params
            .get(k)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let Some(agent) = need("agent_type") else {
        return Err(StatusCode::BAD_REQUEST);
    };
    let Some(sid) = need("session_id") else {
        return Err(StatusCode::BAD_REQUEST);
    };
    let Some(sub) = need("subagent_id") else {
        return Err(StatusCode::BAD_REQUEST);
    };
    if sub.contains(['/', '\\', '\0']) || sub.contains("..") {
        return Err(StatusCode::BAD_REQUEST);
    }
    let limit = params.get("limit").and_then(|s| s.parse().ok()).unwrap_or(200);
    let st = st.clone();
    let result = tokio::task::spawn_blocking(move || {
        match st.subagent_message_source.get(agent.as_str()) {
            Some(f) => f(sid.as_str(), sub.as_str(), limit)
                .map(|pg| (pg, true)), // supported=true（装配即支持）
            None => Ok((crate::remote::content::MessagesPage {
                messages: Vec::new(),
                truncated: false,
            }, false)),
        }
    })
    .await
    .map_err(|e| {
        log::error!("子 agent 详情查询任务异常: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    match result {
        Ok((pg, supported)) => Ok((
            [(axum::http::header::CACHE_CONTROL, "no-store")],
            Json(serde_json::json!({
                "messages": pg.messages,
                "truncated": pg.truncated,
                "supported": supported,
            })),
        )
            .into_response()),
        Err(e) => {
            log::warn!("session-subagent-messages 读取失败（agent={agent}）: {e}");
            Err(StatusCode::NOT_FOUND)
        }
    }
}
```

mod.rs 生产装配（`subagent_source` 之后）：

```rust
        // 2026-10-09 观察台 §三：详情源仅 claude（其余工具 supported=false；
        // 转写格式普查后另批补，spec §五）
        subagent_message_source: [(
            "claude",
            Box::new(crate::remote::content::read_claude_subagent_messages)
                as Box<server::SubagentMessageSourceFn>,
        )]
        .into_iter()
        .collect(),
```

**测试装配补桩（编译器驱动）**：`cargo check --tests` 列出全部缺字段字面量（subagent_source 同款 20 处）——逐一补：

```rust
            subagent_message_source: std::collections::HashMap::new(),
```

前端 api.ts（`fetchSessionSubagents` 之后追加）：

```ts
// ==== 2026-10-09 观察台 §三：子 agent 详情（/session-subagent-messages）====

/** 详情载荷：messages/truncated 与 /session-messages 同形（渲染器零适配）；
 *  supported=false = 该工具暂不支持查看详情（opencode/kimi/codex——spec §三.6，
 *  如实申报不空白页不假数据），前端显示明确提示态 */
export interface SubagentMessagesPage {
  messages: SessionMessage[];
  truncated: boolean;
  supported: boolean;
}

/** 拉取子 agent 执行过程（claude）。404（会话/转写不存在）与非 2xx → ApiError；
 *  supported=false 是 200 正常载荷不是错误 */
export async function fetchSubagentMessages(
  agentType: string,
  sessionId: string,
  subagentId: string,
  limit = 200
): Promise<SubagentMessagesPage> {
  const q = new URLSearchParams({
    agent_type: agentType,
    session_id: sessionId,
    subagent_id: subagentId,
    limit: String(limit),
  });
  let r: Response;
  try {
    r = await fetch(`/m/api/v1/session-subagent-messages?${q}`);
  } catch (e) {
    throw new ApiError(null, `session-subagent-messages 网络异常: ${String(e)}`);
  }
  if (!r.ok) throw new ApiError(r.status, `session-subagent-messages ${r.status}`);
  return (await r.json()) as SubagentMessagesPage;
}
```

- [ ] **Step 4: 跑绿**：`cd src-tauri && cargo test`（新契约矩阵 + content 用例 + 既有回归零破坏）→ PASS；`pnpm vitest run tests/mobile && pnpm build:mobile` → PASS。

- [ ] **Step 5: 门禁 + commit**：

```bash
cd src-tauri && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check
pnpm vitest run tests/mobile && pnpm build:mobile && pnpm format:check && pnpm lint
git add src-tauri/src/remote/ src-tauri/src/monitor/subagents/claude.rs src/mobile/api.ts
git commit -m "feat(subagent-observatory): /session-subagent-messages 详情端点（claude 复用消息映射）"
```

### Task 5: 前端清单卡区 + chip 点击直达（拉取上提 SessionDetail）

**Files:**
- Modify: `src/mobile/SubagentChips.tsx`（改纯展示 + 点击 + `subagentElapsedMs` 导出）
- Create: `src/mobile/SubagentList.tsx`
- Create: `tests/mobile/SubagentList.test.tsx`
- Modify: `src/mobile/FilePanel.tsx`（可选 props + 卡区宿主）
- Modify: `src/mobile/SessionDetail.tsx`（列表拉取上提 + PreviewState 第三形 + 跳转回调 + 两处渲染点接线）
- Rewrite: `tests/mobile/SubagentChips.test.tsx`
- Modify: `tests/mobile/SessionDetail.test.tsx`（新增列表拉取/点击跳转用例）

- [ ] **Step 1: 写失败测试**

`tests/mobile/SubagentList.test.tsx`（新）：

```tsx
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import SubagentList from "@/mobile/SubagentList";
import type { SubagentView } from "@/mobile/api";

// 观察台 §二：单一列表派发序（服务端序即派发序）/ 绿灰点 / 冻结值 /
// 点击回调 / 空（null/[]）整体不渲染。

function sa(
  id: string,
  name: string,
  status: "running" | "idle",
  spawnTs: string | null,
  endTs: string | null,
  description: string | null = "设计新建会话两改动实现方案"
): SubagentView {
  return {
    id, name, status, spawnTs, endTs, description,
    tokens: { input: 5124, cacheRead: 56448, cacheCreation: 0, output: 9312 },
  };
}

describe("SubagentList（文件面板子 Agent 卡区）", () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => { cleanup(); vi.useRealTimers(); });

  it("空（null / []）整体不渲染（§二.7）", () => {
    const { container, rerender } = render(<SubagentList list={null} onOpen={() => {}} />);
    expect(container.querySelector('[data-testid="subagent-list"]')).toBeNull();
    rerender(<SubagentList list={[]} onOpen={() => {}} />);
    expect(container.querySelector('[data-testid="subagent-list"]')).toBeNull();
  });

  it("四要素 + 绿点走字 / 灰点冻结原位（§二.2/§二.3）", () => {
    vi.setSystemTime(new Date("2026-10-08T07:36:45Z"));
    const list = [
      sa("a1", "Plan", "running", "2026-10-08T07:31:07Z", null),       // 5:38 走字
      sa("b1", "Explore", "idle", "2026-10-08T07:00:00Z", "2026-10-08T07:30:00Z"), // 冻结 30:00
    ];
    render(<SubagentList list={list} onOpen={() => {}} />);
    expect(screen.getByTestId("subagent-card-a1").textContent).toContain("Plan");
    expect(screen.getByTestId("subagent-card-a1").textContent).toContain("5:38");
    expect(screen.getByTestId("subagent-card-a1").textContent).toContain("7.09万");
    expect(screen.getByTestId("subagent-dot-a1").className).toContain("bg-emerald-500");
    expect(screen.getByTestId("subagent-card-b1").textContent).toContain("30:00");
    expect(screen.getByTestId("subagent-dot-b1").className).toContain("bg-gray-400");
    // 灰点冻结：时钟前进 60s，b1 不动、a1 继续走
    vi.advanceTimersByTime(60_000);
    expect(screen.getByTestId("subagent-card-b1").textContent).toContain("30:00");
    expect(screen.getByTestId("subagent-card-a1").textContent).toContain("6:38");
  });

  it("列表不重排：服务端序即派发序，续跑灰→绿原位（§二.1/§二.3）", () => {
    vi.setSystemTime(new Date("2026-10-08T07:36:45Z"));
    const list = [
      sa("a1", "Plan", "idle", "2026-10-08T07:00:00Z", "2026-10-08T07:10:00Z"),
      sa("b1", "Explore", "running", "2026-10-08T07:31:00Z", null),
    ];
    const { rerender } = render(<SubagentList list={list} onOpen={() => {}} />);
    const cards = () => screen.getByTestId("subagent-list").querySelectorAll("li");
    expect(cards()[0].querySelector('[data-testid="subagent-card-a1"]')).toBeTruthy();
    // a1 续跑：同位置灰→绿，仍在 b1 之前
    const resumed = [{ ...list[0], status: "running" as const, endTs: null }, list[1]];
    rerender(<SubagentList list={resumed} onOpen={() => {}} />);
    expect(cards()[0].querySelector('[data-testid="subagent-card-a1"]')).toBeTruthy();
    expect(screen.getByTestId("subagent-dot-a1").className).toContain("bg-emerald-500");
    // 时长连续累计不归零（07:00 起，now=07:36:45 → 36:45）
    expect(screen.getByTestId("subagent-card-a1").textContent).toContain("36:45");
  });

  it("点击卡片（绿/灰皆可）→ onOpen(id)（§二.5）", () => {
    vi.setSystemTime(new Date("2026-10-08T07:36:45Z"));
    const onOpen = vi.fn();
    render(
      <SubagentList
        list={[
          sa("a1", "Plan", "running", "2026-10-08T07:31:07Z", null),
          sa("b1", "Explore", "idle", "2026-10-08T07:00:00Z", "2026-10-08T07:30:00Z"),
        ]}
        onOpen={onOpen}
      />
    );
    fireEvent.click(screen.getByTestId("subagent-card-a1"));
    fireEvent.click(screen.getByTestId("subagent-card-b1"));
    expect(onOpen.mock.calls.map((c) => c[0])).toEqual(["a1", "b1"]);
  });
});
```

`tests/mobile/SubagentChips.test.tsx` 重写（纯展示 + 点击直达；拉取行为用例移交 SessionDetail 层）：

```tsx
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import SubagentChips, { chipTokenText, formatElapsed, subagentElapsedMs } from "@/mobile/SubagentChips";
import type { SubagentView } from "@/mobile/api";

// 观察台 T5：chip 改纯展示（拉取上提 SessionDetail）——本文件测
// running 过滤 / 时长自走（tick 不出组件）/ 点击直达 / 空不渲染。

function sa(id: string, status: "running" | "idle", spawnTs: string | null): SubagentView {
  return {
    id, name: `n-${id}`, status, spawnTs, endTs: null, description: "设计新建会话两改动实现方案",
    tokens: { input: 1, cacheRead: 0, cacheCreation: 0, output: 0 },
  };
}

beforeEach(() => vi.useFakeTimers());
afterEach(() => { cleanup(); vi.useRealTimers(); });

describe("SubagentChips（纯展示）", () => {
  it("只渲染 running；idle 不出现在 chip 区（消失语义 = 前端过滤）", () => {
    vi.setSystemTime(new Date("2026-10-08T07:36:45Z"));
    render(
      <SubagentChips
        list={[sa("a1", "running", "2026-10-08T07:31:07Z"), sa("b1", "idle", "2026-10-08T07:00:00Z")]}
        onOpenDetail={() => {}}
      />
    );
    expect(screen.getByTestId("subagent-chip-a1")).toBeTruthy();
    expect(screen.queryByTestId("subagent-chip-b1")).toBeNull();
  });

  it("list=null / 全 idle / 空 → 不渲染", () => {
    const { container, rerender } = render(<SubagentChips list={null} onOpenDetail={() => {}} />);
    expect(container.querySelector('[data-testid="subagent-chips"]')).toBeNull();
    rerender(<SubagentChips list={[]} onOpenDetail={() => {}} />);
    expect(container.querySelector('[data-testid="subagent-chips"]')).toBeNull();
    rerender(<SubagentChips list={[sa("x", "idle", null)]} onOpenDetail={() => {}} />);
    expect(container.querySelector('[data-testid="subagent-chips"]')).toBeNull();
  });

  it("时长自走（tick 不出组件）；点击 chip → onOpenDetail(id)（§二.4 直达）", () => {
    vi.setSystemTime(new Date("2026-10-08T07:36:35Z"));
    const onOpen = vi.fn();
    let parentRenders = 0;
    const Probe = () => {
      parentRenders += 1;
      return <SubagentChips list={[sa("a1", "running", "2026-10-08T07:36:35Z")]} onOpenDetail={onOpen} />;
    };
    render(<Probe />);
    expect(screen.getByTestId("subagent-chip-a1").textContent).toContain("0:00");
    await vi.advanceTimersByTimeAsync(61_000);
    expect(screen.getByTestId("subagent-chip-a1").textContent).toContain("1:01");
    expect(parentRenders).toBe(1, "1s tick 只重渲染本组件");
    fireEvent.click(screen.getByTestId("subagent-chip-a1"));
    expect(onOpen).toHaveBeenCalledWith("a1");
  });
});

describe("subagentElapsedMs（冻结锚共享口径）", () => {
  it("running：now − spawnTs；idle：endTs − spawnTs；spawnTs=null → null", () => {
    const now = Date.parse("2026-10-08T07:36:45Z");
    expect(subagentElapsedMs({ spawnTs: "2026-10-08T07:31:07Z", endTs: null }, now)).toBe(338_000);
    expect(
      subagentElapsedMs({ spawnTs: "2026-10-08T07:00:00Z", endTs: "2026-10-08T07:30:00Z" }, now)
    ).toBe(1_800_000, "冻结在 endTs，不随 now 走");
    expect(subagentElapsedMs({ spawnTs: null, endTs: null }, now)).toBeNull();
  });
  it("formatElapsed / chipTokenText 既有口径不变", () => {
    expect(formatElapsed(338_000)).toBe("5:38");
    expect(chipTokenText({ input: 5124, cacheRead: 56448, cacheCreation: 0, output: 9312 })).toBe("7.09万");
  });
});
```

`tests/mobile/SessionDetail.test.tsx` 追加一组（沿用既有 installFetch 桩——`/session-subagents` 桩位在 throw 之前，路由已存在，改其返回体即可驱动用例；在其上方加一个可配置路由，或将既有三处桩的返回体改为读 `routes.subagents`）：

```tsx
  // 观察台 T5：子 agent 列表由 SessionDetail 拉取（单一数据源）。
  // 既有桩 `/session-subagents → {subagents: []}` 改为读 routes.subagents（默认 []，
  // 既有用例行为不变——缺省空列表卡区/chip 不渲染）。
  if (url.includes("/session-subagents")) {
    return new Response(JSON.stringify({ subagents: routes.subagents ?? [] }), { status: 200 });
  }
```

```tsx
describe("SessionDetail：子 Agent 清单与跳转（观察台 §二）", () => {
  it("FilePanel 打开 → 卡区渲染全量名单（绿灰同列）；点击卡片 → 预览区切 subagent 视图", async () => {
    routes.subagents = [
      { id: "a1", name: "Plan", description: "设计新方案", spawnTs: "2026-10-08T07:31:07Z",
        tokens: { input: 1, cacheRead: 0, cacheCreation: 0, output: 0 },
        status: "running", endTs: null },
      { id: "b1", name: "Explore", description: null, spawnTs: "2026-10-08T07:00:00Z",
        tokens: { input: 1, cacheRead: 0, cacheCreation: 0, output: 0 },
        status: "idle", endTs: "2026-10-08T07:30:00Z" },
    ];
    render(<SessionDetail session={baseSession} onBack={() => {}} />);
    fireEvent.click(await screen.findByTestId("file-panel-button"));
    expect(await screen.findByTestId("subagent-card-a1")).toBeTruthy();
    expect(screen.getByTestId("subagent-dot-b1").className).toContain("bg-gray-400");
    fireEvent.click(screen.getByTestId("subagent-card-a1"));
    // T6 前占位断言：预览区进入 subagent 视图（data-view 由渲染分支写入）
    await vi.waitFor(() =>
      expect(document.querySelector('[data-view="subagent"]')).toBeTruthy()
    );
  });
});
```

- [ ] **Step 2: 跑红**：`pnpm vitest run tests/mobile/SubagentList.test.tsx tests/mobile/SubagentChips.test.tsx` → 模块不存在/props 不符。

- [ ] **Step 3: 实现**

`src/mobile/SubagentChips.tsx` 重写：

```tsx
import { useEffect, useState } from "react";
import { fmtTokens } from "@/lib/usage/format";
import type { SubagentView } from "./api";

/** 运行中子 agent chip 区（v1 自拉 → 观察台 T5 改纯展示：list 由 SessionDetail
 * 单一数据源下发——chip/面板卡区/详情三处一致）。
 * - 只渲染 status==="running"（「完成即消失」= 前端过滤；全量名单在 FilePanel 卡区）；
 * - 时长自走：tick 收在本组件（1s interval）；点击 chip 直达该子 agent 的
 *   实时预览对话框（观察台 §二.4，不经过清单）。 */
export default function SubagentChips({
  list,
  onOpenDetail,
}: {
  list: SubagentView[] | null;
  onOpenDetail: (id: string) => void;
}) {
  // 时长自走 tick（仅本组件重渲染）。now 收在 state 里由 effect/interval 更新——
  // render 期不得调 Date.now()（仓内 eslint react-hooks/purity 红线）；挂载即对齐，
  // 首帧前 now=null 不显时长（一帧，无感）
  const [now, setNow] = useState<number | null>(null);

  useEffect(() => {
    const update = () => setNow(Date.now());
    update();
    const t = setInterval(update, 1000);
    return () => clearInterval(t);
  }, []);

  const running = (list ?? []).filter((s) => s.status === "running");
  if (running.length === 0) return null;
  return (
    <div data-testid="subagent-chips" className="flex flex-wrap items-center gap-x-2 gap-y-1">
      {running.map((s) => {
        const ms = now !== null ? subagentElapsedMs(s, now) : null;
        return (
          <button
            type="button"
            key={s.id}
            data-testid={`subagent-chip-${s.id}`}
            title={s.description ?? undefined}
            onClick={() => onOpenDetail(s.id)}
            className="rounded-full bg-[var(--cb)]/60 px-2 py-0.5 text-[11px] text-[var(--tx)]"
          >
            ◉ {s.name}
            {ms !== null ? ` ${formatElapsed(ms)}` : ""} · {chipTokenText(s.tokens)}
          </button>
        );
      })}
    </div>
  );
}

/** 子 agent 运行时长毫秒（chip 与清单卡共享口径，观察台 §二.3）：
 *  running（endTs=null）→ now − spawnTs 持续走字；idle → endTs − spawnTs 冻结
 *  （续跑转回 running 后自然回到 now 锚——时长从首次派发连续累计不归零）。
 *  spawnTs 缺失/不可解析 → null（只显 token，v1 §8.2 口径） */
export function subagentElapsedMs(
  s: Pick<SubagentView, "spawnTs" | "endTs">,
  now: number
): number | null {
  if (s.spawnTs === null || Number.isNaN(Date.parse(s.spawnTs))) return null;
  const end =
    s.endTs !== null && !Number.isNaN(Date.parse(s.endTs)) ? Date.parse(s.endTs) : now;
  return Math.max(0, end - Date.parse(s.spawnTs));
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

`src/mobile/SubagentList.tsx`（新文件——代码见 Step 1 测试对应的实现）：

```tsx
// 文件面板的「子 Agent」卡区（观察台 §二）：本会话派生过的子 agent 单一列表，
// 服务端序即派发序（spawnTs 升序、新派发附末尾）——本组件不再排序。
// 绿点=活跃（时长/token 走字）、灰点=冻结原位不重排不折叠（§二.3）；
// 点击任一卡片 → 详情（活跃进实时预览 / 不活跃进定格快照——由 SubagentDetail
// 按 status 分诊，本组件只上报 id）。空（null/[]）整体不渲染（§二.7）。
import { useEffect, useState } from "react";
import type { SubagentView } from "./api";
import { chipTokenText, formatElapsed, subagentElapsedMs } from "./SubagentChips";

export default function SubagentList({
  list,
  onOpen,
}: {
  list: SubagentView[] | null;
  onOpen: (id: string) => void;
}) {
  // 相对时钟（1s：活跃卡走字；纯渲染期不调 Date.now——react-hooks/purity）
  const [now, setNow] = useState<number | null>(null);
  useEffect(() => {
    const update = () => setNow(Date.now());
    update();
    const t = setInterval(update, 1000);
    return () => clearInterval(t);
  }, []);

  if (!list || list.length === 0) return null;
  return (
    <section data-testid="subagent-list" aria-label="子 Agent" className="mb-3">
      <p className="mb-1 text-xs font-medium tracking-wide text-[var(--mut)] uppercase">
        子 Agent（{list.length}）
      </p>
      <ul className="space-y-1">
        {list.map((s) => {
          const running = s.status === "running";
          const ms = now !== null ? subagentElapsedMs(s, now) : null;
          return (
            <li key={s.id}>
              <button
                type="button"
                data-testid={`subagent-card-${s.id}`}
                onClick={() => onOpen(s.id)}
                className="flex w-full items-center gap-2 rounded-lg px-2 py-1.5 text-left hover:bg-[var(--cbg)] dark:hover:bg-[var(--btnp)]"
              >
                <span
                  data-testid={`subagent-dot-${s.id}`}
                  className={`h-2 w-2 shrink-0 rounded-full ${
                    running ? "bg-emerald-500" : "bg-gray-400"
                  }`}
                />
                <span className="min-w-0 flex-1 truncate text-sm text-[var(--tx)]">
                  {s.name}
                  {s.description ? (
                    <span className="ml-1 text-xs text-[var(--mut)]" title={s.description}>
                      {s.description}
                    </span>
                  ) : null}
                </span>
                <span className="shrink-0 text-xs text-[var(--mut)]">
                  {ms !== null ? `${formatElapsed(ms)} · ` : ""}
                  {chipTokenText(s.tokens)}
                </span>
              </button>
            </li>
          );
        })}
      </ul>
    </section>
  );
}
```

`src/mobile/FilePanel.tsx`：props 增两项（`entries` 前插入声明 + 解构），`panel-content` 顶部（`{visible.length === 0 …}` 之前）插卡区：

```tsx
import SubagentList from "./SubagentList";
import type { SubagentView } from "./api";
// …
interface FilePanelProps {
  /** 子 Agent 全量名单（观察台 §二）：空/undefined → 卡区整体不渲染 */
  subagents?: SubagentView[] | null;
  /** 卡片点击 → 详情（活跃实时预览 / 不活跃定格快照） */
  onOpenSubagent?: (id: string) => void;
  // …既有 props 不变
}
```

```tsx
      <div
        data-testid="panel-content"
        data-font-scale={fontScale}
        className="min-h-0 flex-1 overflow-y-auto px-3 py-2"
      >
        {/* 子 Agent 卡区（观察台 §二）：派发序单一列表，绿点走字/灰点冻结原位 */}
        <SubagentList list={subagents ?? null} onOpen={onOpenSubagent ?? (() => {})} />
        {/* …既有文件列表不动 */}
```

`src/mobile/SessionDetail.tsx` 五处改动：

(a) `PreviewState` 扩展（:93）：

```ts
/** 预览侧栏状态（M3+ 辨识联合；观察台 §三 增第三形）：
 *  - list：文件面板（聚合列表，用户裁决 4/6）；
 *  - file：单文件预览；backToList 标记来源（从面板进入 → 显示返回按钮，
 *    从消息正文链接进入 → 无返回按钮，行为不变）；
 *  - subagent：子 agent 详情（活跃=实时预览自动刷新 / 不活跃=定格快照）——
 *    backToList 语义与 file 同款（chip 直达 = false，面板卡片进入 = true） */
type PreviewState =
  | { view: "list"; mode: PreviewMode }
  | { view: "file"; path: string; mode: PreviewMode; backToList: boolean }
  | { view: "subagent"; subagentId: string; mode: PreviewMode; backToList: boolean };
```

(b) 列表拉取（state 区 + 文件表拉取 effect 旁新增；`import` 补 `fetchSessionSubagents, type SubagentView`）：

```ts
  // 子 agent 全量名单（观察台 §二）：单一数据源供三处消费——chip（过滤 running）/
  // 文件面板卡区（全量绿灰点）/ 详情对话框（status 查询）。随 refreshTick 重拉
  // （10s 轮询 + 手动刷新免费继承 hidden 暂停/恢复补刷）；**finished 只付首拉一次**
  // （评审 P2-1：清单要历史回溯 §二.6，但 finished 后磁盘数据不再变化——tick 冻结
  // 为 0 不再随 refreshTick 轮询；processing→finished 翻转时 tick N→0 恰好触发
  // 最后一次重拉刷新名单终态，之后停拍；chip 的 finished 挂载门仍在 cardDock）；
  // 失败静默保留上一份（不闪断），首拉失败维持 null（卡区/chip 不渲染）
  const [subagentList, setSubagentList] = useState<SubagentView[] | null>(null);
  const subagentTick = session.status === "finished" ? 0 : refreshTick;
  useEffect(() => {
    let alive = true;
    fetchSessionSubagents(session.agentType, session.id)
      .then((v) => {
        if (alive) setSubagentList(v);
      })
      .catch(() => {
        /* 静默：保留上一份；首拉失败维持 null */
      });
    return () => {
      alive = false;
    };
  }, [session.agentType, session.id, subagentTick]);
```

(c) 跳转回调（`openFileFromList` 旁）：

```ts
  // chip 直达子 agent 详情（观察台 §二.4：不经过清单）：宽屏分屏/窄屏全屏，
  // backToList=false（无返回列表按钮——FilePreview 从正文进入的同款语义）
  const openSubagent = useCallback(
    (id: string) => {
      setPreview({
        view: "subagent",
        subagentId: id,
        mode: isWideViewport() ? "split" : "fullscreen",
        backToList: false,
      });
    },
    [isWideViewport]
  );

  // 面板卡区进入（观察台 §二.5）：沿用当前 mode（往返保持，openFileFromList 同款）
  const openSubagentFromList = useCallback((id: string) => {
    setPreview((p) => ({
      view: "subagent",
      subagentId: id,
      mode: p?.mode ?? "fullscreen",
      backToList: true,
    }));
  }, []);
```

(d) cardDock 的 chip 接线（:1076）：

```tsx
        {session.status !== "finished" && (
          <SubagentChips
            key={`subagents-${session.id}`}
            list={subagentList}
            onOpenDetail={openSubagent}
          />
        )}
```

(e) 两处 `FilePanel` 调用点（split :1379 与 fullscreen :1440）补 props；两处渲染分支（`preview.view === "list" ? … : …`）改为三分——subagent 分支 T6 前先放占位容器（T5 的集成测试只断言 `data-view="subagent"` 容器出现）：

```tsx
              {preview.view === "list" ? (
                <FilePanel
                  subagents={subagentList}
                  onOpenSubagent={openSubagentFromList}
                  /* …既有 props 不变 */
                />
              ) : preview.view === "subagent" ? (
                // T6 填充 SubagentDetail；T5 先落容器（data-view 已在外层写入）
                <div data-testid="subagent-detail-slot" className="h-full" />
              ) : (
                <FilePreview /* …既有 props 不变 */ />
              )}
```

（两处布局分支——split 内联与 fullscreen 浮层——同款三分；外层容器的 `data-view={preview.view}` 已有，subagent 值自动生效。）

- [ ] **Step 4: 跑绿**：`pnpm vitest run tests/mobile` → PASS（含 SessionDetail 既有用例——`routes.subagents` 缺省空列表行为与旧桩一致）。

- [ ] **Step 5: 门禁 + commit**：

```bash
pnpm vitest run tests/mobile && pnpm build:mobile && pnpm format:check && pnpm lint
git add src/mobile/SubagentChips.tsx src/mobile/SubagentList.tsx src/mobile/FilePanel.tsx src/mobile/SessionDetail.tsx tests/mobile/SubagentChips.test.tsx tests/mobile/SubagentList.test.tsx tests/mobile/SessionDetail.test.tsx
git commit -m "feat(subagent-observatory): 文件面板子 Agent 卡区与 chip 点击直达"
```

### Task 6: 详情视图——渲染复用抽取 + 实时预览对话框

**Files:**
- Create: `src/mobile/message-render.tsx`（从 SessionDetail 纯搬家）
- Create: `src/mobile/SubagentDetail.tsx`
- Create: `tests/mobile/SubagentDetail.test.tsx`
- Modify: `src/mobile/SessionDetail.tsx`（渲染器改用 hook；T5 的 subagent-detail-slot 换真组件）
- Modify: `tests/mobile/SessionDetail.test.tsx`（既有桩补 `/session-subagent-messages` 路由）

- [ ] **Step 1: 写失败测试**

`tests/mobile/SubagentDetail.test.tsx`（新）：

```tsx
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import SubagentDetail from "@/mobile/SubagentDetail";
import type { Session } from "@/types/session";

// 观察台 §三：supported 态 / 消息渲染复用（thinking 默认折叠可展开）/ 任务原文 /
// running 自动刷新起停（fake timers）/ 不活跃定格。

const baseSession: Session = {
  id: "s1", agentType: "claude", projectName: "p", projectPath: "p", title: null,
  gitBranch: null, githubUrl: null, status: "processing", lastMessage: null,
  lastMessageRole: null, lastActivityAt: "2026-10-08T07:00:00Z", pid: 1, cpuUsage: 0,
  activeSubagentCount: 0, form: "cli", jumpSupported: false, unread: false,
};

let fetchMock: ReturnType<typeof vi.fn>;
let routes: {
  body?: { messages: unknown[]; truncated?: boolean; supported?: boolean };
  status?: number;
  fail?: boolean;
};

beforeEach(() => {
  routes = {};
  vi.stubGlobal(
    "fetch",
    (fetchMock = vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      if (!url.includes("/session-subagent-messages"))
        throw new Error(`unexpected fetch: ${url}`);
      if (routes.fail) throw new TypeError("network down");
      return new Response(
        JSON.stringify({
          messages: routes.body?.messages ?? [],
          truncated: routes.body?.truncated ?? false,
          supported: routes.body?.supported ?? true,
        }),
        { status: routes.status ?? 200 }
      );
    }))
  );
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

function msg(p: Partial<Record<string, unknown>> & { seq: number; kind: string; content: string }) {
  return {
    role: "assistant", ts: null, toolName: null, toolArgs: null,
    collapsed: p.kind === "thinking", ...p,
  };
}

describe("SubagentDetail（实时预览对话框）", () => {
  it("supported=false → 明确「暂不支持查看详情」态，不渲染消息区（§三.6）", async () => {
    routes.body = { messages: [], supported: false };
    render(
      <SubagentDetail session={baseSession} subagentId="x1" subagentName="Explore"
        running={false} mode="fullscreen" onClose={() => {}} openFile={() => {}} />
    );
    expect(await screen.findByTestId("subagent-detail-unsupported")).toBeTruthy();
    expect(document.querySelector('[data-testid="subagent-messages"]')).toBeNull();
  });

  it("任务原文 + 消息渲染复用（thinking 默认折叠、点击展开）+ 工具行折叠（§三.2/§三.5）", async () => {
    routes.body = {
      messages: [
        msg({ seq: 0, kind: "user", content: "<teammate-message teammate_id=\"team-lead\">设计新方案</teammate-message>" }),
        msg({ seq: 1, kind: "thinking", content: "先看目录" }),
        msg({ seq: 2, kind: "tool-call", content: "", toolName: "Bash", toolArgs: "{\"command\":\"ls\"}" }),
        msg({ seq: 3, kind: "assistant", content: "已完成" }),
      ],
    };
    render(
      <SubagentDetail session={baseSession} subagentId="a1" subagentName="Plan"
        running={true} mode="fullscreen" onClose={() => {}} openFile={() => {}} />
    );
    // 任务原文：首条 user 剥 teammate-message 壳（§三.2「任务原文=首条指令」）
    expect(await screen.findByTestId("subagent-task-text").textContent).toBeTruthy();
    expect(screen.getByTestId("subagent-task-text").textContent).toContain("设计新方案");
    expect(screen.getByTestId("subagent-task-text").textContent).not.toContain("teammate-message");
    // thinking / tool-call 默认折叠（wire collapsed 语义），点击展开
    expect(screen.getByTestId("subagent-msg-1").textContent).toContain("思考过程");
    fireEvent.click(screen.getByTestId("subagent-msg-1-toggle"));
    expect(screen.getByTestId("subagent-msg-1").textContent).toContain("先看目录");
    expect(screen.getByTestId("subagent-msg-2").textContent).toContain("调用 Bash");
    expect(screen.getByTestId("subagent-msg-3").textContent).toContain("已完成");
  });

  it("running：打开期间自动刷新（数秒一拍），输出逐步增长（§三.3）", async () => {
    vi.useFakeTimers();
    routes.body = { messages: [msg({ seq: 0, kind: "assistant", content: "步骤 1" })] };
    render(
      <SubagentDetail session={baseSession} subagentId="a1" subagentName="Plan"
        running={true} mode="fullscreen" onClose={() => {}} openFile={() => {}} />
    );
    expect(await screen.findByTestId("subagent-msg-0")).toBeTruthy();
    routes.body = {
      messages: [
        msg({ seq: 0, kind: "assistant", content: "步骤 1" }),
        msg({ seq: 1, kind: "assistant", content: "步骤 2" }),
      ],
    };
    await vi.advanceTimersByTimeAsync(5_000); // SUBAGENT_DETAIL_REFRESH_MS
    await vi.waitFor(() => expect(fetchMock.mock.calls.length).toBeGreaterThanOrEqual(2));
    await vi.waitFor(() => expect(screen.getByTestId("subagent-msg-1")).toBeTruthy());
  });

  it("running=false：不装轮询（定格快照，§三.4）；关闭即停（卸载清 interval）", async () => {
    vi.useFakeTimers();
    routes.body = { messages: [msg({ seq: 0, kind: "assistant", content: "终稿" })] };
    const { unmount } = render(
      <SubagentDetail session={baseSession} subagentId="a1" subagentName="Plan"
        running={false} mode="fullscreen" onClose={() => {}} openFile={() => {}} />
    );
    expect(await screen.findByTestId("subagent-msg-0")).toBeTruthy();
    await vi.advanceTimersByTimeAsync(30_000);
    expect(fetchMock).toHaveBeenCalledTimes(1, "不活跃 → 不轮询（定格快照）");
    unmount(); // 关闭即停：卸载后再无任何调用（防御断言）
    await vi.advanceTimersByTimeAsync(30_000);
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });
});
```

`tests/mobile/SessionDetail.test.tsx`：既有三处 fetch 桩各补一条路由（**在 throw 之前**）：

```ts
    if (url.includes("/session-subagent-messages")) {
      return new Response(
        JSON.stringify({ messages: [], truncated: false, supported: true }),
        { status: 200 }
      );
    }
```

- [ ] **Step 2: 跑红**：`pnpm vitest run tests/mobile/SubagentDetail.test.tsx` → 模块不存在。

- [ ] **Step 3: 实现**

`src/mobile/message-render.tsx`（新——**纯搬家**：`linkifyMarkdown` / `LinkSegment` / `linkifySegments` / `renderMarkdown` / `renderLinkifiedText` / `renderBody` 六件从 SessionDetail 迁出，收进一个 hook；SessionDetail 同名内部定义删除、改 import——既有 SessionDetail 测试全绿即搬家不改语义的验收）：

```tsx
// 会话消息渲染器（2026-10-09 观察台 T6 从 SessionDetail 纯搬家）：会话详情与
// 子 agent 详情共用同一套视觉与交互（spec §三.5「不另造一套」）。纯搬家零语义
// 变化——renderBody 的 per-kind 分支（markdown/plan 卡/工具参数升格/图片降级）
// 见原 SessionDetail 注释（注释随代码一并迁来）。
import { useCallback } from "react";
import type { ReactNode } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeHighlight from "rehype-highlight";
import type { SessionMessage } from "./api";

/** 已知路径按长度降序（最长优先替换：路径互为前缀时不被短路径截断）。
 *  2026-10-09 T6 随渲染器从 SessionDetail:117 迁入（评审 P3-10：原处删除防
 *  死代码；勿在 hook 内再内联第二份排序） */
export function sortedPaths(files: Set<string>): string[] {
  return [...files].sort((a, b) => b.length - a.length);
}

/** markdown 正文链接化预处理：把出现的已知路径替换为 `#file:` 内链，
 *  再由 components.a 拦截渲染成可点按钮。路径含 markdown 特殊字符（[]()）时
 *  该处替换可能不成链（保持原样文本，M3 接受） */
export function linkifyMarkdown(text: string, files: string[]): string {
  let out = text;
  for (const p of files) {
    if (!p) continue;
    out = out.split(p).join(`[${p}](#file:${encodeURIComponent(p)})`);
  }
  return out;
}

/** 链接化分段元素：文本段或命中已知路径的文件段 */
type LinkSegment = { type: "text" | "file"; value: string };

/** 纯文本分段链接化：按已知路径把正文切成文本段与文件段（thinking / tool-result 用） */
export function linkifySegments(text: string, files: string[]): LinkSegment[] {
  let segments: LinkSegment[] = [{ type: "text", value: text }];
  for (const p of files) {
    if (!p) continue;
    const next: LinkSegment[] = [];
    for (const seg of segments) {
      if (seg.type !== "text" || !seg.value.includes(p)) {
        next.push(seg);
        continue;
      }
      const parts = seg.value.split(p);
      parts.forEach((part, i) => {
        if (part) next.push({ type: "text", value: part });
        if (i < parts.length - 1) next.push({ type: "file", value: p });
      });
    }
    segments = next;
  }
  return segments;
}

/** 消息渲染器集合（SessionDetail 与 SubagentDetail 共用）。
 *  openFile：正文路径点击回调（两页各自的预览入口）；files：已知路径集（链接化）。 */
export function useMessageRenderers({
  openFile,
  files,
}: {
  openFile: (path: string) => void;
  files: Set<string>;
}) {
  // ↓ 以下三个 useCallback 与 per-kind renderBody 的实现 = SessionDetail 原文搬迁
  // （含 #file: 内链拦截 / md-body 排版层 / plan 与 plan-file 卡 / extractPlanBody
  // 工具参数升格——所有分支语义与注释原样保留，此处不重复罗列；实施时整段
  // cut-paste，勿手抄重写）。**extractPlanBody 随 renderBody 一并迁入本文件并
  // export**（评审 P1-3：renderBody 的 tool-call 分支调用它——留在 SessionDetail
  // 会循环 import / 编译失败；已核实仓内无其他消费方，isPlanPending 仍留守
  // SessionDetail——详情页挂载门专属，renderBody 不消费）
  const sorted = sortedPaths(files);
  const renderMarkdown = useCallback(
    (text: string): ReactNode => (
      <ReactMarkdown remarkPlugins={[remarkGfm]} rehypePlugins={[rehypeHighlight]}
        components={{/* #file: 拦截 + 外链 <a>——SessionDetail :597-629 原文 */}}>
        {linkifyMarkdown(text, sorted)}
      </ReactMarkdown>
    ),
    [openFile, sorted]
  );
  const renderLinkifiedText = useCallback(
    (text: string): ReactNode => (
      <>{linkifySegments(text, sorted).map((seg, i) =>
        seg.type === "text" ? (
          <span key={i}>{seg.value}</span>
        ) : (
          <button key={i} type="button" data-testid="file-link"
            className="break-all text-[var(--tx)] underline underline-offset-2"
            onClick={() => openFile(seg.value)}>
            {seg.value}
          </button>
        )
      )}</>
    ),
    [openFile, sorted]
  );
  const renderBody = useCallback(
    (m: SessionMessage): ReactNode => {
      switch (m.kind) {
        // assistant/user/plan/plan-file/tool-call/tool-result/default 七分支
        // ——SessionDetail :652-775 原文整段搬迁（语义零变化）
        default:
          return <div className="text-sm">{m.content}</div>;
      }
    },
    [renderMarkdown, renderLinkifiedText]
  );
  return { renderMarkdown, renderLinkifiedText, renderBody };
}
```

（**实施纪律**：`renderMarkdown` 的 `components` 与 `renderBody` 的七分支必须从 `SessionDetail.tsx:597-775` 整段 cut-paste（含注释），本计划不复述全文；搬家后 SessionDetail 既有测试全绿 = 验收。）

`src/mobile/SubagentDetail.tsx`（新）：

```tsx
// 子 Agent 实时预览对话框（观察台 §三）：预览空间第三形态（与文件预览同区，
// split/split-h/fullscreen 由父排版）。活跃 = 打开期间 5s 一拍自动刷新逐步输出
// （关闭即停；hidden 暂停对齐 SessionDetail 轮询先例）；不活跃 = 定格快照。
// 渲染复用 useMessageRenderers（与会话消息同一套视觉与交互，§三.5）。
// 仅 claude：其余工具 supported=false → 明确「暂不支持查看详情」态。
// 详情内**不重复展示时长/token**（评审 P3-8）：定格快照以清单卡冻结值为准
// （验收口径单点），对话框只承载执行过程正文。
// 页头状态点与 SubagentList 卡片点同款样式（两处 3 行重复，容忍——出现第三处
// 消费再抽 Dot 小组件，评审 P3-9）。
import { useEffect, useRef, useState } from "react";
import { ChevronDown, ChevronRight } from "lucide-react";
import PreviewModeSwitcher, { type PreviewMode } from "./PreviewModeSwitcher";
import { collapsedLabel, isProcessKind } from "./message-fold";
import { useMessageRenderers } from "./message-render";
import { ApiError, fetchSessionFiles, fetchSubagentMessages, type SessionMessage } from "./api";
import type { Session } from "@/types/session";

/** 自动刷新周期（观察台 §三.3「数秒一拍」）：详情页 10s 轮询的一半——
 *  逐步输出的观感优先；仅对话框打开且 running 时产生请求 */
const SUBAGENT_DETAIL_REFRESH_MS = 5_000;

interface SubagentDetailProps {
  session: Session;
  subagentId: string;
  /** 显示名（来自名单快照；null = 名单未拉到 → 显示 id） */
  subagentName: string | null;
  /** 运行态（来自名单快照）：true=实时预览（自动刷新），false=定格快照 */
  running: boolean;
  mode: PreviewMode;
  onModeChange?: (mode: PreviewMode) => void;
  onBack?: () => void;
  fontScale?: number;
  onClose: () => void;
  /** 正文路径点击 → 文件预览（SessionDetail 传既有 openFile——§三.5「同一套交互」：
   *  详情正文里的已知路径可点开，与消息正文行为一致） */
  openFile: (path: string) => void;
}

/** 任务原文剥壳（§三.2）：首条 user 若被 <teammate-message …> 包裹（乙.3.1
 *  teammate 形态），剥标签显内文；经典形态裸文本原样返回 */
export function extractTaskText(content: string): string {
  const open = content.indexOf(">");
  const close = content.lastIndexOf("</teammate-message>");
  if (content.startsWith("<teammate-message") && open >= 0 && close > open) {
    return content.slice(open + 1, close).trim();
  }
  return content;
}

export default function SubagentDetail({
  session, subagentId, subagentName, running, mode, onModeChange, onBack, fontScale = 1, onClose,
  openFile,
}: SubagentDetailProps) {
  const [messages, setMessages] = useState<SessionMessage[] | null>(null);
  const [error, setError] = useState<{ status: number | null } | null>(null);
  // supported=false：该工具暂不支持查看详情（§三.6）
  const [unsupported, setUnsupported] = useState(false);
  // 贴底跟随（P2-B 同款语义）：刷新前采样，贴底才跟随落底
  const areaRef = useRef<HTMLDivElement>(null);
  const followRef = useRef(true);

  useEffect(() => {
    let alive = true;
    const load = () => {
      fetchSubagentMessages(session.agentType, session.id, subagentId)
        .then((pg) => {
          if (!alive) return;
          setUnsupported(!pg.supported);
          setMessages(pg.messages);
          setError(null);
          const el = areaRef.current;
          if (el && followRef.current) el.scrollTop = el.scrollHeight;
        })
        .catch((e: unknown) => {
          if (alive) setError({ status: e instanceof ApiError ? e.status : null });
        });
    };
    load();
    // 自动刷新：仅 running（§三.3）且页面可见（对齐 SessionDetail F6 hidden 暂停）。
    // visibilitychange 暂停逻辑与 SessionDetail F6 是两份独立实现（评审 P3-11：
    // 规模小，容忍——出现第三消费者再抽 useVisibleInterval hook）
    if (!running) return () => { alive = false; };
    const tick = () => {
      const el = areaRef.current;
      followRef.current = el === null || el.scrollHeight - el.scrollTop - el.clientHeight < 120;
      load();
    };
    let timer: ReturnType<typeof setInterval> | null =
      document.visibilityState !== "hidden" ? setInterval(tick, SUBAGENT_DETAIL_REFRESH_MS) : null;
    const onVisibility = () => {
      if (document.visibilityState === "hidden") {
        if (timer !== null) { clearInterval(timer); timer = null; }
      } else if (timer === null) {
        timer = setInterval(tick, SUBAGENT_DETAIL_REFRESH_MS);
      }
    };
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      alive = false;
      if (timer !== null) clearInterval(timer);
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, [session.agentType, session.id, subagentId, running]);

  // 折叠覆盖表（SessionDetail expandedOverride 同款）：seq → 强制展开/折叠；
  // 缺省走 wire collapsed 语义（thinking/tool-call 默认折叠，点击展开——§三.5）
  const [expandMap, setExpandMap] = useState<Map<number, boolean>>(new Map());
  const isCollapsed = (m: SessionMessage): boolean => expandMap.get(m.seq) ?? m.collapsed;
  const toggle = (m: SessionMessage) => {
    setExpandMap((prev) => {
      const next = new Map(prev);
      next.set(m.seq, !isCollapsed(m));
      return next;
    });
  };

  // 文件链接化数据源：本会话文件表（fetchSessionFiles 既有封装；失败静默 → 空集
  // = 不链接化，消息照常渲染——增强能力不阻塞详情）
  const [files, setFiles] = useState<Set<string>>(new Set());
  useEffect(() => {
    let alive = true;
    fetchSessionFiles(session.agentType, session.id, 200)
      .then((pg) => {
        if (alive) setFiles(new Set(pg.files.map((f) => f.path)));
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [session.agentType, session.id]);

  const { renderBody } = useMessageRenderers({ openFile, files });

  const firstUser = messages?.find((m) => m.kind === "user") ?? null;

  return (
    <section
      data-testid="subagent-detail"
      aria-label="子 Agent 详情"
      className="flex h-full min-h-0 flex-col bg-[var(--cbg)]"
    >
      <header className="flex shrink-0 items-center gap-2 border-b border-[var(--cb)] px-3 py-2">
        {onBack && (
          <button type="button" data-testid="subagent-back" aria-label="返回列表"
            onClick={onBack}
            className="shrink-0 rounded-full p-1 text-[var(--mut)] hover:bg-[var(--cb)] dark:hover:bg-[var(--btnp)]">
            <ChevronRight size={16} className="rotate-180" />
          </button>
        )}
        <span data-testid="subagent-dot"
          className={`inline-block h-2 w-2 shrink-0 rounded-full ${
            running ? "bg-emerald-500" : "bg-gray-400"
          }`} />
        <span className="min-w-0 flex-1 truncate text-sm font-medium text-[var(--tx)]">
          {subagentName ?? subagentId}
        </span>
        {running && (
          <span className="shrink-0 text-[10px] text-[var(--mut)]">实时 · 5s</span>
        )}
        {onModeChange && (
          <PreviewModeSwitcher mode={mode} onChange={onModeChange} testIdPrefix="preview-toggle" />
        )}
        <button type="button" data-testid="subagent-close" aria-label="关闭详情"
          onClick={onClose}
          className="shrink-0 rounded-full p-1 text-[var(--mut)] hover:bg-[var(--cb)] dark:hover:bg-[var(--btnp)]">
          <ChevronDown size={16} className="rotate-90" />
        </button>
      </header>

      <div ref={areaRef} data-testid="subagent-messages" data-font-scale={fontScale}
        className="min-h-0 flex-1 overflow-y-auto px-3 py-2">
        {unsupported ? (
          <p data-testid="subagent-detail-unsupported"
            className="py-12 text-center text-sm text-[var(--mut)]">
            该工具暂不支持查看详情（子 Agent 清单照常）
          </p>
        ) : error ? (
          <p data-testid="subagent-detail-error"
            className="py-12 text-center text-sm text-rose-600 dark:text-rose-400">
            {error.status === 404 ? "无法读取该子 agent 内容" : "加载失败，请检查网络后重试"}
          </p>
        ) : messages === null ? (
          <p className="py-12 text-center text-sm text-[var(--mut)]">加载中…</p>
        ) : (
          <>
            {firstUser && (
              <div data-testid="subagent-task" className="mb-2">
                <p className="mb-1 text-[11px] font-medium tracking-wide text-[var(--mut)] uppercase">
                  任务原文
                </p>
                <div data-testid="subagent-task-text"
                  className="rounded-lg border border-[var(--cb)] bg-[var(--bub)] p-2 text-sm text-[var(--tx)]">
                  {extractTaskText(firstUser.content)}
                </div>
              </div>
            )}
            <ul className="space-y-2 pb-4">
              {messages.map((m) => {
                const toggleable = isProcessKind(m.kind);
                const collapsed = isCollapsed(m);
                return (
                  <li key={m.seq} data-testid={`subagent-msg-${m.seq}`} data-kind={m.kind}
                    className="flex flex-col items-start">
                    <div className="w-full rounded-2xl border border-[var(--cb)] bg-[var(--cbg)] px-3 py-2">
                      {toggleable ? (
                        <>
                          <button type="button" data-testid={`subagent-msg-${m.seq}-toggle`}
                            aria-expanded={!collapsed}
                            onClick={() => toggle(m)}
                            className="-mx-1 flex w-[calc(100%+8px)] items-center gap-1 rounded-lg px-1 py-0.5 text-left text-xs text-[var(--mut)]">
                            {collapsed ? <ChevronRight size={13} /> : <ChevronDown size={13} />}
                            <span className="truncate">{collapsedLabel(m)}</span>
                          </button>
                          {!collapsed && <div className="mt-1">{renderBody(m)}</div>}
                        </>
                      ) : (
                        renderBody(m)
                      )}
                    </div>
                  </li>
                );
              })}
            </ul>
          </>
        )}
      </div>
    </section>
  );
}
```

（组件代码为定稿形态：`files` 用既有 `fetchSessionFiles` 取 path 集、`expandMap`/`isCollapsed`/`toggle` 为 SessionDetail 折叠交互的同款实现、`openFile` 为必填 prop 由 SessionDetail 传入既有回调。唯一「引用原文」处是 `useMessageRenderers` 内 `renderBody` 七分支——从 `SessionDetail.tsx:652-775` 整段 cut-paste（含注释），搬家后 SessionDetail 既有测试全绿即验收。）

SessionDetail 接线（T5 的两处 `subagent-detail-slot` 换真组件）：

```tsx
              ) : preview.view === "subagent" ? (
                <SubagentDetail
                  key={`subagent-detail-${session.id}-${preview.subagentId}`}
                  session={session}
                  subagentId={preview.subagentId}
                  subagentName={
                    subagentList?.find((s) => s.id === preview.subagentId)?.name ?? null
                  }
                  running={
                    subagentList?.find((s) => s.id === preview.subagentId)?.status === "running"
                  }
                  mode={preview.mode}
                  onModeChange={changePreviewMode}
                  onBack={preview.backToList ? backToList : undefined}
                  openFile={openFile}
                  fontScale={fontScale}
                  onClose={closePreview}
                />
              ) : (
```

（`running` 查不到名单条目时为 `undefined` → 组件按「不活跃」处理——名单尚未拉到或条目已不在（会话换代会话目录）时保守定格，不空转轮询。）

SessionDetail 渲染器搬家接线：删除 `:265-297`（linkify 两函数）与 `:597-775`（renderMarkdown/renderLinkifiedText/renderBody），import 改：

```tsx
import { useMessageRenderers } from "./message-render";
// …组件内：
  const { renderBody } = useMessageRenderers({ openFile, files });
```

（**搬家迁移清单（评审 P1-3/P3-10，逐项闭环）**：①`extractPlanBody`（SessionDetail:206）**随 renderBody 迁入 message-render.tsx 并 export**——tool-call 分支消费它，留守会循环 import（仓内无其他消费方，已核实）；②`isPlanPending`（SessionDetail:240）**留守 SessionDetail**——详情页挂载门专属；③`sortedPaths`（SessionDetail:117）迁入 message-render.tsx 并 export；④SessionDetail 原处的 `sortedPaths` 定义与 `sortedFiles` useMemo（:593）在渲染器搬走后**无剩余消费 → 删除**（死代码不留）；⑤`renderMarkdown`/`renderLinkifiedText` 若组件内还有其它消费点，一并从 hook 返回值解构。）

- [ ] **Step 4: 跑绿**：`pnpm vitest run tests/mobile` → PASS（SubagentDetail 四用例 + SessionDetail 既有消息渲染用例全绿 = 搬家验收）。

- [ ] **Step 5: 门禁 + commit**：

```bash
pnpm vitest run tests/mobile && pnpm build:mobile && pnpm format:check && pnpm lint
git add src/mobile/message-render.tsx src/mobile/SubagentDetail.tsx src/mobile/SessionDetail.tsx tests/mobile/SubagentDetail.test.tsx tests/mobile/SessionDetail.test.tsx
git commit -m "feat(subagent-observatory): 子 Agent 实时预览对话框——渲染复用与自动刷新"
```

### Task 7: 通知打标 + 设置开关

**Files:**
- Modify: `src-tauri/src/monitor/claude_parser.rs`（digest 打标 + Session 构造）
- Modify: `src-tauri/src/session/model.rs:70`（Session 增字段）
- Modify: 全部构造 `Session` 的位置（编译器驱动补 `last_message_subagent_report: false`；涉及各 `*_parser.rs` 与测试夹具）
- Modify: `src/types/session.ts` / `src/tauri-mock.ts`
- Modify: `src/hooks/useNotification.ts`
- Modify: `src/pages/settings.tsx` + `src/i18n/locales/zh.json` / `en.json`
- Create: `tests/notification/subagentReportFilter.test.ts`

- [ ] **Step 1: 写失败测试**

`tests/notification/subagentReportFilter.test.ts`（照 notifyOnce.test.ts 惯例）：

```ts
// 观察台 §四：关闭「子 Agent 回报提醒」仅滤子 agent 回报触发的通知——
// 打标来自后端（Session.lastMessageSubagentReport 布尔），前端不匹配文案。
import { act, renderHook, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

const { invokeMock, sendNotificationMock } = vi.hoisted(() => ({
  invokeMock: vi.fn(),
  sendNotificationMock: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));
vi.mock("@tauri-apps/plugin-notification", () => ({
  isPermissionGranted: vi.fn(async () => true),
  requestPermission: vi.fn(async () => "granted"),
  sendNotification: sendNotificationMock,
  onAction: vi.fn(async () => () => {}),
  registerActionTypes: vi.fn(async () => {}),
}));
vi.mock("@/lib/audio", () => ({ playCompletionSound: vi.fn() }));
vi.mock("@/lib/notificationHistory", () => ({ addHistory: vi.fn() }));
vi.mock("@/components/pet/petConfig", () => ({
  petSuppressPopup: () => false,
  petSoundTakeover: () => false,
}));

import { useSessionStore } from "@/stores/sessionStore";
import { useNotification, GREEN_STABLE_MS } from "@/hooks/useNotification";

function session(id: string, report: boolean) {
  return {
    id, agentType: "claude", form: "cli" as const, projectName: "Demo", title: "T",
    gitBranch: null, githubUrl: null, status: "idle",
    lastMessage: report ? "Another Claude session sent a message: <teammate-message …" : "ok",
    lastMessageSubagentReport: report,
    lastMessageRole: null, lastActivityAt: new Date().toISOString(),
    pid: 1, cpuUsage: 0, activeSubagentCount: 0, jumpSupported: true, unread: true,
  };
}

// 微任务 flush（评审 P1-4）：用例先 vi.useFakeTimers() 再等初始化——setTimeout
// 已被 fake 接管会永挂（notifyOnce.test.ts 是真计时器场景，写法不可照抄）；
// init effect 的 await 链全是微任务，排空微任务队列即够
async function flushInit() {
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
  });
}

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockImplementation(async (cmd: string, args?: { key?: string }) => {
    if (cmd === "show_notification_window") throw new Error("no popup in jsdom");
    if (cmd === "get_setting" && args?.key === "notify_subagent_report") return "false";
    return null; // notifications_enabled 缺省 → 开
  });
  sendNotificationMock.mockReset();
  useSessionStore.setState({ sessions: [] });
});
afterEach(() => {
  // 兜底恢复真计时器（评审 P1-4）：用例中途断言抛错时尾行手动恢复不会执行，
  // 泄漏的 fake timers 会拖垮后续用例
  vi.useRealTimers();
});

describe("子 Agent 回报提醒开关（观察台 §四）", () => {
  it("开关关闭：回报触发的转绿不弹；主会话自身动作照常（§四.2）", async () => {
    vi.useFakeTimers();
    renderHook(() => useNotification());
    await flushInit();
    // 首见：一条回报标记、一条无标记（都走绿稳定窗）
    act(() => {
      useSessionStore.setState({ sessions: [session("s-report", true), session("s-own", false)] });
    });
    await act(async () => { await vi.advanceTimersByTimeAsync(GREEN_STABLE_MS + 100); });
    // show_notification_window 失败 → 降级系统通知（可观测断言点）：
    // 仅 s-own 到达，s-report 被滤
    const targets = sendNotificationMock.mock.calls.map((c) => JSON.stringify(c[0].extra));
    expect(targets.some((t) => t.includes("s-own"))).toBe(true, "主会话自身动作照常提醒");
    expect(targets.some((t) => t.includes("s-report"))).toBe(false, "子 agent 回报被滤");
  });

  it("开关开（缺省）：回报触发的提醒照常（§四.1 默认开）", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "show_notification_window") throw new Error("no popup in jsdom");
      return null; // notify_subagent_report 缺省 → 开
    });
    vi.useFakeTimers();
    renderHook(() => useNotification());
    await flushInit();
    act(() => {
      useSessionStore.setState({ sessions: [session("s-report", true)] });
    });
    await act(async () => { await vi.advanceTimersByTimeAsync(GREEN_STABLE_MS + 100); });
    expect(sendNotificationMock).toHaveBeenCalledTimes(1);
  });
});
```

Rust 侧打标单测（claude_parser.rs title_tests 旁新组，复用 v1 通知夹具形态）：

```rust
    // ==== 2026-10-09 观察台 T7：lastMessage 子 agent 回报打标（同源谓词）====

    /// teammate 报告 / task-notification → true；普通文本 → false（乙.3.4 实证形态）
    #[test]
    fn digest_marks_subagent_report() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("parent.jsonl");
        std::fs::write(
            &p,
            concat!(
                r#"{"type":"user","timestamp":"2026-10-08T15:02:26Z","message":{"role":"user","content":"Another Claude session sent a message: <teammate-message teammate_id=\"bg\">任务完成</teammate-message>"}}"#,
                "\n",
            ),
        )
        .unwrap();
        let d = read_claude_digest(&p);
        assert!(d.last_is_subagent_report);
        std::fs::write(
            &p,
            r#"{"type":"user","timestamp":"2026-10-08T15:09:43Z","message":{"role":"user","content":"主 agent 自己的回执复述"}}"#,
        )
        .unwrap();
        let d = read_claude_digest(&p);
        assert!(!d.last_is_subagent_report, "主会话自身动作不打标（乙.3.4 23:09:43 案例）");
    }
```

（`read_claude_digest` 为私有 fn 且 `CLAUDE_DIGEST_SCAN` 有进程级缓存——该用例需走既有测试对 digest 缓存的规避惯例：tempdir 路径唯一，摘要缓存键为路径，不冲突。）

- [ ] **Step 2: 跑红**：`pnpm vitest run tests/notification/subagentReportFilter.test.ts` → TS 报 `lastMessageSubagentReport` 不在 Session 类型；`cd src-tauri && cargo test digest_marks_subagent_report` → 字段不存在。

- [ ] **Step 3: 实现**

session/model.rs（`Session` 字段区，`last_message_role` 之后）：

```rust
    /// lastMessage 是否为「子 agent 回报」触发（观察台 §四 提醒开关的判定信号；
    /// claude 判据层同源打标——teammate-message / task-notification，其余工具
    /// 恒 false = 「未区分」如实申报，spec §四.4）
    pub last_message_subagent_report: bool,
```

**横切补桩（编译器驱动）**：`cd src-tauri && cargo check --tests` 列出全部缺字段位置（各 parser 的 Session 构造 + 测试夹具，预估 10–20 处）——逐一补 `last_message_subagent_report: false`；`claude_parser.rs:291` 的构造处补真实值 `last_message_subagent_report: digest.last_is_subagent_report`。

claude_parser.rs（digest 结构体 + 读取函数 + 构造点）：

```rust
struct ClaudeFileDigest {
    // …既有字段…
    /// T7：末条预览消息是否为子 agent 回报（提醒开关打标；谓词与判据同源）
    last_is_subagent_report: bool,
}
```

`read_claude_digest` 内、`last_message` 截断（`let last_message = last_message.map(|m| …)`）**之前**插入：

```rust
    // T7 打标：判原文（截断可能切掉尾部标记）；谓词复用判据层信号原语（同源，
    // spec §四.3——前端只消费布尔，永不匹配文案）
    let last_is_subagent_report = last_message
        .as_deref()
        .map(crate::monitor::subagents::claude::is_subagent_report_text)
        .unwrap_or(false);
```

（`ClaudeFileDigest` 构造与截断后的 `last_message` 顺序注意：先打标、后截断。）

TS：`src/types/session.ts` 的 `Session` 补：

```ts
  /** lastMessage 是否为「子 agent 回报」触发（观察台 §四 提醒开关判定信号；
   *  后端判据层打标，claude 专属、其余工具恒 false = 未区分） */
  lastMessageSubagentReport: boolean;
```

`src/tauri-mock.ts` 的 `get_setting` 补一行（`notifications_enabled` 旁）：

```ts
        if (args?.key === "notify_subagent_report") return Promise.resolve(true);
```

（mock 的 sessions 数据若构造 Session 字面量，同样补 `lastMessageSubagentReport: false`——tsc 驱动。）

`src/hooks/useNotification.ts`——`notifyCompletion` 的 `notifications_enabled` 检查（:177）之后插入：

```ts
        // 子 Agent 回报提醒开关（观察台 §四）：仅滤「子 agent 回报导致」的提醒；
        // 判定信号 = 后端判据层打标（Session.lastMessageSubagentReport 布尔），
        // 与修复项完成识别同源——不在此处匹配 lastMessage 文案（§四.3）
        try {
          const v = await invoke<string | null>("get_setting", {
            key: "notify_subagent_report",
          });
          subagentReportEnabled.current = v !== "false"; // 缺省/读取失败 = 开
        } catch {
          subagentReportEnabled.current = true;
        }
        if (!subagentReportEnabled.current && session.lastMessageSubagentReport) return;
```

（ref 声明：`const subagentReportEnabled = useRef(true);` 放 `notificationsEnabled` 旁——每拍刷新支持运行时切换，与既有开关同模式。）

`src/pages/settings.tsx`——notifications 区块「桌面通知」行（:611-627）之后插入同款行：

```tsx
                <div className="border-t" />
                <div className="flex items-center justify-between py-2.5">
                  <div className="flex-1">
                    <label className={SETTINGS_FIELD}>
                      {t("settings.notifications.subagentReport")}
                    </label>
                    <p className="text-muted-foreground mt-0.5 text-xs">
                      {t("settings.notifications.subagentReportDesc")}
                    </p>
                  </div>
                  <Button
                    variant={subagentReportEnabled ? "default" : "outline"}
                    size="sm"
                    onClick={toggleSubagentReport}
                  >
                    {subagentReportEnabled
                      ? t("settings.notifications.on")
                      : t("settings.notifications.off")}
                  </Button>
                </div>
```

state + toggle（`toggleNotifications` 旁，读值同款）：

```tsx
  const [subagentReportEnabled, setSubagentReportEnabled] = useState(true);
  useEffect(() => {
    void (async () => {
      try {
        const v = await invoke<string | null>("get_setting", { key: "notify_subagent_report" });
        setSubagentReportEnabled(v !== "false");
      } catch {
        /* 缺省开 */
      }
    })();
  }, []);
  const toggleSubagentReport = async () => {
    const newValue = !subagentReportEnabled;
    setSubagentReportEnabled(newValue);
    await invoke("set_setting", { key: "notify_subagent_report", value: String(newValue) });
    toast.success(
      newValue
        ? t("settings.notifications.subagentReportOnToast")
        : t("settings.notifications.subagentReportOffToast")
    );
  };
```

i18n（zh.json `settings.notifications` 键族 + en.json 同步——`pnpm check:i18n` 门禁校验两文件键集一致）：

```json
      "subagentReport": "子 Agent 回报提醒",
      "subagentReportDesc": "子 agent 完成回报唤醒主会话时的提醒（仅 Claude；关闭后主会话自身的提醒不受影响）",
      "subagentReportOnToast": "子 Agent 回报提醒已开启",
      "subagentReportOffToast": "子 Agent 回报提醒已关闭",
```

```json
      "subagentReport": "Sub-agent report alerts",
      "subagentReportDesc": "Alerts when a sub-agent report wakes the main session (Claude only; main-session alerts are unaffected)",
      "subagentReportOnToast": "Sub-agent report alerts enabled",
      "subagentReportOffToast": "Sub-agent report alerts disabled",
```

- [ ] **Step 4: 跑绿**：`pnpm vitest run tests/notification/subagentReportFilter.test.ts` → PASS；`cd src-tauri && cargo test` → PASS（含既有 notifyOnce / greenStableWindow 回归——未打标会话不受影响）。

- [ ] **Step 5: 门禁 + commit**（涉 i18n 键，check:i18n 必须过）：

```bash
cd src-tauri && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check
pnpm test && pnpm build:mobile && pnpm build && pnpm format:check && pnpm lint && pnpm check:i18n
git add src-tauri/src/ src/types/session.ts src/tauri-mock.ts src/hooks/useNotification.ts src/pages/settings.tsx src/i18n/locales/ tests/notification/subagentReportFilter.test.ts
git commit -m "feat(subagent-observatory): 子 Agent 回报提醒开关与通知打标"
```

### Task 8: 收口——观测日志、全量门禁、真机验收清单

**Files:**
- Modify: `src-tauri/src/remote/api.rs`（`session_subagents` handler 观测日志一行）
- Modify: `docs/engineering/plans/2026-10-09-subagent-observatory.md`（勾选状态维护）

- [ ] **Step 1: 观测日志（真机验收辅助，spec §六 需要「完成即消失/续跑即复现」时刻级对账）**

`session_subagents` handler（排序后、响应前）：

```rust
    // 观察台 §六 真机验收辅助：每拍一条 debug（10s 轮询低噪声）——
    // 验收时 `pnpm tauri:dev` 控制台 grep "subagents" 即可对账
    // 「完成即消失（running 数下降）/ 续跑即复现」与「灰点冻结（endTs 不变）」
    let running = subagents
        .iter()
        .filter(|s| s.status == crate::monitor::subagents::SubagentStatus::Running)
        .count();
    log::debug!("subagents {agent}/{sid}: {} running / {} total", running, subagents.len());
```

- [ ] **Step 2: 全量回归**

```bash
cd src-tauri && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check
pnpm test && pnpm build:mobile && pnpm build && pnpm format:check && pnpm lint && pnpm check:i18n
```

- [ ] **Step 3: commit**

```bash
git add src-tauri/src/remote/api.rs docs/engineering/plans/2026-10-09-subagent-observatory.md
git commit -m "docs(subagent-observatory): 收口——观测日志与真机验收清单"
```

## 六、门禁（每 commit 全绿；命令逐段执行防管道吞退出码）

```bash
# Rust（src-tauri/ 下）
cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check
# 前端（仓库根）
pnpm vitest run tests/mobile   # 任务级；T7 起含 tests/notification；收尾全量 pnpm test
pnpm build:mobile              # 含 tsc（移动端入口编译）
pnpm format:check && pnpm lint
pnpm check:i18n                # T7 起新增 i18n 键，必须过
```

红线沿用 v1：测试零网络、零真实用户数据目录（tempdir / 合成字节流 / 内存库 / 假源）；改 `src/mobile/**` 的 commit 先 `pnpm build:mobile` 再 `cargo test`（rust-embed debug 直读依赖构建产物）；改 `src-tauri/src/services/usage/**` 的纪律本批不触及。

## 七、不做（本批范围红线）

- **不向子 agent 发消息/操控**（注入域另立设计，spec §五）。
- **不动 `count_active_subagents`**（issue #134 单独跟踪；桌面看板徽标落差不变）。
- **其余工具详情视图**：opencode/kimi/codex 的 `/session-subagent-messages` 恒 `supported:false`（清单照常）——转写格式普查后另批。
- **其余工具提醒判定**：`lastMessageSubagentReport` 仅 claude 打标，其余工具恒 false（开关对它们无效果 = 「未区分」如实申报，spec §四.4）。
- **普通（非 idle）teammate-message 不计为完成信号**（甲.5 待议，不入本批——测试 `classify_teammate_idle_line`/`teammate_source_lifecycle` ② 已锁）。
- **不做 SSE/事件推送**：清单随 SessionDetail 10s refreshTick、详情 5s 轮询（与既有节奏一致）。
- **不做消息分页/加载更早**：详情对话框单页 200 条（`fetchSubagentMessages` 缺省 limit=200，与详情页默认同标尺）；「加载更早消息」若需要留后续批次。
- **端点契约冻结（本批后）**：`/session-subagents` 载荷（全量 + status/endTs）与 `/session-subagent-messages` 形状不再动；后续工具扩展只补 source 装配。

## 八、已知边界（实施不修，验收对照）

1. **teammate 名字冲突**：两个 teammate 同名时别名表先到先得（按 id 排序注册序）——第二个同名 agent 的完成信号会归到第一个（上游编队名机制下未见此形态，如实申报）。
2. **idle 终态锚精度**：claude = 子转写末行 ts（≈ 完成时刻，差一个落盘拍）；opencode/kimi = 最后活动时刻（无完成事件，长静默转灰时锚在静默起点）；codex = task_complete 行 ts。
3. **opencode/kimi 长静默转灰**：90s 活跃窗语义从「隐藏」变「转灰」——长工具调用期间卡片灰点冻结，恢复活动转绿（比 v1 隐藏更如实，但灰绿可能抖动；接受）。
4. **codex 清单回溯窗口**：24h 新鲜窗 + 缓存命中（AGENTS.md L3 预算契约）——MAM 重启后超窗完成的 codex 子 agent 不在名单（claude/opencode/kimi 全量回溯无此边界；spec §二.6 的全称覆盖以 claude 为准验收）。
5. **详情 only-claude**：其余工具清单正常、点入显示「暂不支持」；不支持态不是错误（200 + supported:false）。
6. **任务原文依赖首条 user 在尾窗内**：超长转写（>4MB）首条被字节窗截掉时任务原文区不渲染（`firstUser=null`），消息流照常——与详情页 truncated 同语义。
7. **spawnTs=null（spawn 竞态）**：卡片只显名字/token（v1 §8.2 口径延续）；灰点冻结值同理无时长。
8. **打标谓词的覆盖面**：`is_subagent_report_text` 认 `<task-id>` 与 `<teammate-message` 两个信号原语；若上游出现「无标签的跨会话消息」形态，随判据层再取证（不猜）。
9. **idle 后迟到写入微调冻结值**（评审 P2-3）：claude idle 后 `step()` ② 仍读子转写增量——极迟落盘的行会微调该灰点卡的 tokens/endTs（数据轻微变动，概率低、幅度小）。**不建议 idle 停累计**（会让 resume 判定复杂化——停拍后恢复需重放窗口）；申报接受。

## 九、真机验收清单（spec §六；需用户配合起 teammate 会话）

> 前置：`pnpm tauri:dev`（mam-worktree）+ 远程接入 + 手机看板；验收对账辅助 = T8 的
> `subagents <agent>/<sid>: N running / M total` debug 日志（控制台 grep "subagents"）。
> teammate 会话由用户在真实 Claude Code 里驱动（spawn / 续跑提示词）；执行方只读观测。

1. **teammate 全生命周期（§六.1）**：起后台子 agent → chip 与清单卡出现（绿点走字）→ 完成后 chip 消失、清单卡原地转灰点冻结（时长停在完成时刻、token 不再变）→ 续跑 → chip 复现、卡片灰转绿继续走字（时长连续不归零）。双 agent 并发时一绿一灰同屏、各自独立更新/冻结、排列按派发序（新的在末尾）。
2. **详情实时预览（§六.2）**：点击活跃 chip 直接进入该子 agent 的实时预览对话框，输出逐步增长可见（5s 一拍 + 贴底跟随）；关闭即停（再开不追帧）；不活跃卡片点开为定格快照（内容与冻结值不再变化）。
3. **经典机制回归（§六.3）**：既有 claude 经典后台 agent 行为不劣化（出现/消失/续跑复现三态 + 详情可看）。
4. **提醒开关（§六.4）**：设置页关闭「子 Agent 回报提醒」→ 子 agent 完成不再弹窗、主会话自身动作照常；重开恢复。
5. **回溯（§二.6）**：重启 MAM dev（缓存清零）→ 重新打开会话详情 → 文件面板「子 Agent」卡区整份名单照常重建（含灰点完成态）。

---

## 自审记录（writing-plans SKILL 自查，实施者可忽略）

- **Spec 覆盖**：§一 修复（机制分派/经典零改动/完成信号/名字归一/双机制用例锁）→ T1（spec §一.5 的「完成即消失、续跑即复现」双机制用例：T1 `teammate_source_lifecycle` + 既有 `claude_source_lifecycle`；混跑 `teammate_and_classic_mixed`）；§二 清单（派发序/四要素/绿灰点冻结/续跑走字/点击直达/卡片点击/回溯/空不渲染）→ T2（终态字段）+ T3（端点）+ T5（卡区 + chip 点击 + `subagentElapsedMs` 冻结锚 + 续跑连续累计用例）；§三 详情（第三形态/自动刷新/定格/内容四件/渲染复用/仅 claude）→ T4（端点 + supported）+ T6（视图 + `extractTaskText` + 5s 轮询起停 + message-render 抽取）；§四 开关（默认开/仅滤回报/同源打标/仅 claude）→ T7（`is_subagent_report_text` 同源谓词 + Session 布尔 + KV + useNotification 过滤 + 用例两方向）；§五 不做 → §七；§六 验收 → §九 + T8 观测日志。
- **占位符扫描**：T6 的 SubagentDetail 组件为定稿实现（files 数据源/折叠覆盖表/openFile prop 均成文）；唯一「引用原文而非复述」的位置是 `useMessageRenderers` 内 `renderBody` 七分支——从 SessionDetail 原文整段 cut-paste（约 120 行含注释，逐字复述只会制造漂移面），已显式标注来源行号与验收（既有测试全绿）。T4 的 `state_with_subagent_msg_fakes` 组装沿仓内「每用例组持有字面量」惯例给出模式与差异行，非 TBD。
- **类型一致性**：`SubagentView`（Rust，+status/endTs）↔ TS `SubagentView`（+"running"|"idle"/endTs）↔ 端点 JSON（camelCase，status 小写单词——T2 `subagent_status_serializes_lowercase` 单元锁 + T3 契约矩阵锁）；`SubagentMessageSourceFn = Fn(&str,&str,usize) -> Result<MessagesPage,String>` 全链一致；`subagentElapsedMs` 的消费方（chips/清单卡）与冻结语义（endTs ?? now）单点定义；`last_message_subagent_report`（Rust snake）↔ `lastMessageSubagentReport`（TS camel）经 serde camelCase 对应。
- **与 v1 契约冻结条款的关系**：spec 本批明示扩展清单载荷（§二）——v1「端点契约冻结」由 spec 解除的部分仅限载荷增字段与全量语义；路径/参数/400/no-store/排序不变（T3 注释更新为证）。
- **spec 疑点（已在计划内处置，供用户复核）**：①codex「重启全量回溯」与 AGENTS.md L3 预算契约冲突 → 决策 G 取 L3 优先并申报边界（§八.4）；②「灰点冻结时长」需要完成时刻锚而四工具证据不同源 → 决策 B 就近取证（§八.2）；③普通 teammate-message 是否计为活动信号 = 甲.5 待议项，本批按「不计」锁用例（§七）。
- **评审修订（2026-10-09 code-reviewer，结论「修订后可开工」）**：P1-1 T2 摘要回落链补齐实现并留痕决策 I（`AgentTrack.first_user` / `accumulate_agent_line` 第 5 参首条 user 捕获 / 视图构造 `description.or(first_user 剥壳截断)` / `strip_teammate_wrapper` 与前端 `extractTaskText` 两语言两份申报）；P1-2 T4 limit clamp 落到注入核入口（`read_claude_subagent_messages_with` 开头 `clamp(1,1000)`——既有 clamp 在 session-messages 派发核内，新路径不经它）；P1-3 T6 搬家依赖闭环（`extractPlanBody` 随 renderBody 迁入 message-render 并 export——tool-call 分支消费它；`isPlanPending` 留守；五项迁移清单含 `sortedPaths` 迁入与原处死代码删除）；P1-4 T7 测试假计时器修复（`flushInit` 改微任务 flush、删未用的 rerender 形参、afterEach 兜底 `vi.useRealTimers()`、删用例尾行手动恢复）；P2-1 finished 会话清单只付首拉（`subagentTick` 冻结为 0，翻转时恰好末拉一次刷新终态）；P2-2 opencode 两条必破用例补全断言改写（`active_boundary_expires_at_90s` / `stale_child_…` 改名 `stale_child_marks_idle_…`）；P2-3 §八.9 申报 idle 后迟到写入微调冻结值（不停累计——resume 判定会复杂化）；P3-8 详情不重复展示时长/token（验收口径单点在卡片冻结值）、P3-9 Dot 两处重复容忍注记、P3-10 `sortedPaths` 迁入删原处（并入 P1-3 清单）、P3-11 visibility 暂停双份容忍注记。
```
