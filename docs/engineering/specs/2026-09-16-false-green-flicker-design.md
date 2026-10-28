# 设计文档：会话假「完成」信号（假绿）治理 — Codex CLI / WorkBuddy / OpenCode

- 日期：2026-09-16
- 状态：设计定稿（全部决策已拍板，含 OpenCode 活体取证；待出实现计划）
- 工作分支：`fix/status-false-green`（基于 main `2b86907`）
- 关联问题：用户实测报告——Codex CLI 多轮工具调用场景，每轮结束输出中间消息时被误判「任务完成」转绿灯，触发宠物语音

---

## 1. 背景与症状

用户报告：Codex CLI 执行一个由多轮工具调用组成的任务时，只要模型完成一轮工具调用并回复一条消息，MAM 就把该会话判定为「任务完成」（绿灯），触发一次宠物 "done" 语音；随后下一轮工具调用开始，又翻回黄灯。一个多轮任务会触发多次假完成语音。

### 1.1 完整触发链路（已代码级验证）

```
rollout 尾部短暂停在中间 assistant 消息
  → codex_parser.rs 判 Idle（绿灯）                    [误判源]
  → 后端 sync_unread：上一轮非绿 → 本轮绿 = Insert 未读行  [边沿触发]
  → 前端 petStatus.ts：prevColor≠green → green = completion 事件 [边沿触发]
  → FoxbellPet.tsx：newCompletion → playVoice("done")    [语音]
```

三层都是边沿触发，任何一次黄→绿翻转都会全链路生效。

### 1.2 实证数据（2026-09-16 本机 rollout）

`~/.codex/sessions/2026/09/16/rollout-2026-09-16T21-03-06-*.jsonl`，单个 task（`task_started` 13:03:33 → `task_complete` 13:04:34，61 秒）内出现 **5 条中间 assistant 消息，每条后面都还有下一轮 function_call**：

```
11  13:03:36  msg/assistant   ← 中间消息
12  13:03:37  function_call    ← 任务还在继续
...
57  13:04:11  msg/assistant   ← 中间消息
58  13:04:14  function_call    ← 中间隔了整 3 秒
...
66  13:04:34  msg/assistant   ← 真正的收尾消息
69  13:04:34  ev:task_complete
```

中间消息与下一轮 function_call 落盘间隔 0~3 秒。MAM 每 3 秒轮询，落在窗内即产生一次假绿。

## 2. 根因

共享判定核 `derive_app_status`（`src-tauri/src/monitor/app_status.rs:79`）采用**尾部倒扫**模型：取最后一条有语义条目定状态，`AssistantMessage` 在尾即判 Idle。该模型假设「assistant 纯文本 = 回合完成信号」，但 Codex（及下文 WorkBuddy）在**同一回合内**会在工具调用轮次之间写中间文本消息，该假设不成立。判定时完全不感知回合窗口——尽管 `task_started`/`task_complete` 已被 `codex_entry_kind`（`codex_parser.rs:61-64`）解析为 `TurnStart`/`TurnEnd` 放进 kinds 序列，核只把它们当「最后一条」候选，从不作为**开闭对**读取。

历史脉络：判定核诞生于 2026-09-06（commit `e14ab47`，issue #6），当时修的是「第二轮工具执行期恒 Idle」（function_call 条目被整体跳过），尾扫模型解决了永久性误判，但「中间消息短暂占据尾部」的瞬时误判是其固有残留。2026-09-10 APP 路线（commit `9285c56`）迁移 SQLite 时拿到了 Codex 自维护的轮次生命周期表 `thread_turns.status`，加了 `inProgress → Processing` 强信号守卫；CLI rollout 路线未被回移同款守卫。

## 3. 全工具横评与策略

| 工具/路线 | 判定模型 | 中间消息假绿风险 | 本轮策略 |
|---|---|---|---|
| **Codex CLI**（rollout JSONL） | 共享尾扫核 | **实锤**（§1.2 实证） | **加回合守卫**（§4.1） |
| **Codex APP**（app-server SQLite） | 尾扫核 + `inProgress` 强信号 | 已守卫（`codex_thread_parser.rs:338`） | 不动，作为语义参照 |
| **WorkBuddy**（JSONL） | 共享尾扫核，**格式无任何轮次信号** | **本机数据证实同款中间消息模式**（最新会话尾部 `msg/assistant → function_call → …` 多轮交错） | **完成防抖 10s**（§4.2） |
| **ZCode**（SQLite） | 尾扫核 + `step-finish(reason=tool-calls)`→ToolCall 守卫 | 半免疫：步骤间已守卫（`zcode_parser.rs:661` `part_entry_kind`），残余仅部件流式落盘窗口（未证实） | 不动，残余暴露记录于 §7 |
| **Claude**（JSONL） | 消息模型：assistant 带 `tool_use` → Processing（`status.rs:127`） | 免疫（文字+tool_use 同条消息，纯文本即回合结束） | 不动 |
| **Kimi**（wire.jsonl 事件流） | 事件模型：`tool_calls` 判据 + `turn.ended` 边界（`kimi_parser.rs:429`） | 免疫 | 不动 |
| **OpenCode**（SQLite） | `last_role` + 60s 新鲜窗 + CPU（`opencode_parser.rs:355`） | 三症状：输入瞬间绿→红 + 假 approval 语音；运行全程红灯（应为黄）；单步 >60s 运行中假绿 | **加会话尾部部件 step 边界守卫**（§4.3，ZCode 同构，活体取证完成，一并修三症状） |
| **OpenClaw** | 纯 CPU 启发式 | 另一类误报源（API 等待期 CPU 低） | 不动（超出本轮范围，§7 记 follow-up） |
| **dsh**（事件流 + 锁） | 轮次事实 `has_open_turn`（`dsh/status.rs`） | 免疫，正面教材 | 不动，作为 Codex 守卫的参考实现 |

方案总纲两条轨道：**有精确信号的用信号**（Codex CLI 的开闭对、OpenCode 的 tool part），**没信号的用防抖**（WorkBuddy）。

## 4. 修复设计

### 4.1 Codex CLI：回合守卫

**目标**：回合（task）仍在进行中时，中间 assistant 消息不得触发 Idle；只有回合真正结束（`task_complete` 落盘）才转绿。

**实现位置**：`app_status.rs` 新增共享纯函数 `turn_window_open(&[AppEntryKind]) -> Option<bool>`（最后一个 `TurnStart` 晚于最后一个 `TurnEnd` → `Some(true)`；窗口内无任何边界事件 → `None`）+ `codex_parser.rs` 消费，`derive_app_status` 契约不变（核仍是「最后一条说了什么」，回合感知是叠加在核之上的仲裁层，WorkBuddy/ZCode 等无信号消费者不受影响）。复用与参考：dsh 的 `TurnFacts::has_open_turn`（`dsh/status.rs`）语义相同但其类型绑定 `DshEvent` 不可直接复用，新函数按 `AppEntryKind` 镜像其语义；守卫条件中「尾部语义条目是 AssistantMessage」复用 §4.2 的 `tail_semantic_kind`（同文件共享，两节共同消费）。

**已定方案 A（2026-09-16 用户拍板；方案 B 为已否决备选，见本节末尾）**：

- **方案 A（开闭对，dsh 同款，选定）**：扫描 kinds 序列，最后一个 `TurnStart` 晚于最后一个 `TurnEnd` → 回合开（`Some(true)`）；仅 `TurnEnd` 可见无 `TurnStart`（起点滚出窗口）→ 不可证开（`Some(false)`）；窗口内无任何边界事件 → `None` 不仲裁。仅当「derive 结果为 Idle 且尾部语义条目是 `AssistantMessage` 且回合开」时改判 Processing。
  - 超长回合（>500 行尾读窗口）时 `TurnStart` 滚出窗口 → 回合状态不可证开 → 回退现状（用户已拍板：保持现状 + 文档化，见 §8 决策 2）。已知限制：长回合中途仍可能假绿（记入 §7）。
  - 既有测试 `assistant_message_tail_is_idle`（夹具无 task_started/task_complete 开闭对）在方案 A 下语义不变（窗口内无边界事件 → 不仲裁 → Idle 照旧），无适配负担——这是 A 相对 B 的实现简洁性优势。

**不参与仲裁的路径**：`UserMessage` 在尾 → Thinking 原样；`ToolCall`/`TurnStart` 在尾 → Processing 原样；`TurnEnd` 在尾 → Idle 原样。

**测试**（方案 A）：
- 真实样本夹具化（§1.2 序列）：中间消息在尾 → Processing；追加 function_call → Processing；追加收尾消息 + task_complete → Idle。
- 两轮同文件：第一轮完成后第二轮运行中 → Processing（回归 issue #6 主场景）。
- 回合正常完成 → Idle 立即（无防抖延迟）。
- 窗口内无开闭对（既有夹具形态）→ 语义不变（Idle 照旧），锁定 A 的回退行为。

**已否决备选——方案 B（不可证完成即不绿）**：`AssistantMessage` 在尾且位于窗口内最后一个 `TurnEnd` 之后（或窗口内无 `TurnEnd`）→ 保持 Processing。优点：超长回合中途也不假绿、绿灯零延迟；缺点：依赖「每回合必写 `task_complete`」这一协议假设，且反转 `assistant_message_tail_is_idle` 既有语义。用户裁决取 A（行为可预期性优先，超窗场景接受已知限制）。

### 4.2 WorkBuddy：完成防抖（10 秒）

**目标**：WorkBuddy 格式无任何轮次边界信号，无法精确判定；用时间防抖拦截瞬态假绿。

**语义**：`derive_status_from_tail` 结果为 Idle 且尾部语义条目为 `AssistantMessage` 时——文件 mtime 年龄 < `GREEN_DEBOUNCE_MS`（10_000）→ 保持 Processing；≥ 10s → Idle。其余状态（Thinking/Processing）原样。

**实现位置**：`workbuddy_parser.rs` 构卡层（现有 mtime 叠加点 `workbuddy_parser.rs:384-393` 旁），纯时间叠加，不动 L2 缓存契约（内容摘要仍为纯内容产物）。

**接口调整**：防抖需感知「Idle 是否由 assistant 尾导出」。在 `app_status.rs` 新增共享纯函数 `tail_semantic_kind(&[AppEntryKind]) -> Option<AppEntryKind>`（与 §4.1 守卫函数同文件，便于单测），`derive_status_from_tail` 内部改用它定位尾部语义条目；WorkBuddy 构卡层以「derive 结果 + 尾部语义条目」共同决定防抖。`derive_status_from_tail` 对外签名不变，波及面仅限内部实现与新增单测。

**权衡**（用户已接纳）：所有真完成绿灯/语音延迟 10 秒到达；10s 窗口内模型若停顿超 10s 再继续（重推理长停顿），仍可能漏放一次假绿——防抖是启发式，不追求全防。

**命名防混**：`GREEN_DEBOUNCE_MS` 与既有 `FALLBACK_FRESH_MS`（`zcode_parser.rs:48` / `codex_thread_parser.rs:53`，值均为 `APP_STATUS_STALE_MS` = 300s）语义不同——后者是「无信号时按会话新鲜度兜底」的窗口，前者是「完成信号延迟确认」的防抖窗；独立常量，实现期不得互相借用或改名对齐。

**与 300s 停更叠加的顺序**：防抖在前（assistant 尾 + 新鲜 → 拉回 Processing），`overlay_mtime_stale` 在后。真完成场景 10s 后即为 Idle，不会落入 300s 降级路径。

**测试**：assistant 尾 + mtime 9.9s → Processing / 10.1s → Idle；`function_call` 尾不受防抖影响（照常 Processing）；user 尾照常 Thinking；防抖只作用于 assistant 尾导出的 Idle，不影响其他 Idle 来源。

### 4.3 OpenCode：step 边界守卫（ZCode 同构）

**目标**：消除三类症状——① 输入瞬间绿灯直落红灯并触发假 approval 语音（2026-09-16 用户报告）；② 任务运行全程红灯（应为黄灯）；③ 单步超 60s 运行中假绿（假完成语音）。

**现状与根因**：`determine_opencode_status`（`opencode_parser.rs:355`）只看 last_role + 60s 窗（`is_recent` 基于**消息行创建时间**，`opencode_parser.rs:272`）+ CPU，完全不读 parts。根因（活体取证，2026-09-16）：

- opencode 在用户按回车后 **~105-171ms** 即创建**空的 assistant 占位消息行**（部件之后才写入；首轮 step-start 部件约 +4s 才到）。「user 消息在尾 → 黄灯」的正确状态仅存在 ~130ms，3s 轮询必然错过，落点恒为 `last_role=assistant + is_recent` → **Waiting 红灯** → 绿→红边沿触发 `newWaiting` → 假 approval 语音（`petStatus.ts:129` → `FoxbellPet.tsx:333`，10s 冷却）。
- 运行期间最后一条消息永远是 assistant（占位行或 step 消息）→ **全程红灯**；CPU 分支条件 `last_role != "assistant"` 永不满足。
- 单步（一条 step 消息）运行超 60s：`is_recent` 过期 → **运行中假绿** + 假完成语音（step 间隔实测可达 24s+，长工具调用轻松超 60s）。

**活体取证（2026-09-16，opencode v1.18.22，用户配合实测一轮工具任务）**——原「实现期前置取证」任务已消解，证据如下：

- **消息/部件结构**：每个模型 step = 一条独立 assistant 消息，部件序 `step-start → [reasoning | tool | text] → step-finish`；一轮 = 一至多个 step。
- **step-finish.reason 词汇表（本机两值全观测）**：`tool-calls`（本步因要调工具而结束，后续还有动作）/ `stop`（回合结束）。与 ZCode 解析器的映射（`zcode_parser.rs:661` `part_entry_kind`）及 opencode issue 生态（[opencode #14972](https://github.com/anomalyco/opencode/issues/14972) 中 `finish_reason: tool_calls` 标准值）三源交叉一致。
- **tool 部件真实形状**：`{"type":"tool","tool":"bash","callID":"call_…","state":{"status":"completed","input":{"command":"…"}}}`——tool 部件会落盘，本守卫设计不依赖它（增强项）。
- **时序金证**：step1 `step-finish(tool-calls)` 落盘到 step2 `step-start` 落盘间隔 **4.3 秒**——此窗口内尾部即 `step-finish(tool-calls)`，恰是守卫要覆盖的「步骤间空窗」；step2 回答 text 落盘到 `step-finish(stop)` 间隔 5.1 秒（部件流式窗口，守卫同样覆盖）。

**设计**（ZCode 同构，升级为**会话尾部部件序列**判定——占位行场景最后一条消息没有部件，只看末条消息会在回合头 ~4s 漏判）：新增「会话最后一条 part（跨消息，按 time_created）+ 该 part 所属消息的 role + 该消息是否含 step 部件」查询。复用与实现要点：

- 查询沿既有模式新增（`get_last_message_info`（`opencode_parser.rs:280`）/ `get_message_text`（`:310`，内部 `:312` 已有按消息查 parts 的 SQL）为参照）；`part` 表自带 `session_id` 列（`PRAGMA table_info(part)` 实证），末条 part 可直接按会话查询，仅需 join `message` 取 role。
- part JSON（`type`/`reason` 顶层字段）与 ZCode `part_entry_kind`（`zcode_parser.rs:661`）的输入同构，但**不抽共享映射**（2026-09-16 计划期裁决，§8 决策 7）：① ZCode 在不动清单（§5），为其重构共享件违反本轮范围约束；② 两者对 `step-finish(reason="length")` 的判定语义相反——ZCode 映射 TurnEnd（回合结束）、OpenCode 按「宁黄不假绿」归 Running（输出截断后会自动续步），共享映射会互相绑架。OpenCode 侧自实现 ~25 行可测纯函数（raw `type` + `reason` 判定），语义独立性优先于去重。

**前置规则：末条消息失败判定（2026-09-17 用户裁决，优先于尾部部件规则）**——末条消息 `role=assistant` 且 `data.error` 非空 → 该请求已彻底失败/被中止，部件层无痕迹（error 落在 **0 part 的空 assistant 占位行**上，尾部 part 停留在 user 文本 → 若无此规则将永久黄灯）。取证：本机库 10 条真实样本（9× `APIError` 401 Invalid API key + 1× `MessageAbortedError`，形态样本 `ses_f5574c39`）：

- `error.name == "MessageAbortedError"`（用户主动 Esc 中止）→ **Idle 绿**——用户自己终止的已知事实，与提示板无关，不提示（2026-09-17 用户裁决：「我主动终止，这跟提不提示我没有关系」）
- 其余 error（`APIError` 等，含 name 缺失的任意非空 error）→ **Waiting 红**——需要用户介入（修 key / 重试）；绿→红边沿触发语音为**正确报警**（区别于症状①的假报警）
- 明确**不加时间兜底**（user 尾部超时判异常）——error 字段取证可靠，时间兜底会把模型冷启动慢与失败混淆（2026-09-17 用户裁决）

规则按尾部部件映射：

- 尾部 part = `step-finish(reason="stop")` → 回合结束 → **Idle 绿（完成即绿）**（2026-09-17 用户裁决：推翻本节原「收尾红→绿保留」决策——正常完成不需要提示，**红只留给失败与批准**；完成语音随黄→绿边沿即刻触发，不再延迟 60s）
- 尾部 part = `step-finish(reason≠stop)`（`tool-calls`/`length`）→ 后续还有动作 → **Processing 黄**
- 尾部 part = `step-start` / `reasoning` / `tool` → 步骤进行中 → **Processing 黄**
- 尾部 part **属于 user 消息**（任意 part 类型；含占位行空窗：最后一条消息是空 assistant 行、末条 part 仍属于 user 消息）→ 输入刚提交 → **Processing 黄**（修症状①：不再误触 Waiting 红/假语音）
- 尾部 part = assistant 的 `text`/`patch` 且其消息**无任何 step 部件**（team-mode 老格式，无信号）→ 回退现行为（last_role + 60s + CPU 启发式；assistant 尾完成即绿，同上裁决），老会话零回归
- 会话无 part 数据（空库/极旧库）→ 回退现行为

增强说明：尾部出现 `tool` 部件即判 Running（部件在步骤内落盘、`step-finish` 随后 ~0.5s 才写，工具部件在尾恒为步骤进行中）——已覆盖「`state.status` 未完成 → Processing」的原始增强设想且更简，不再单独读取 `state.status`。

该设计同时修复：症状①（占位空窗判黄，绿→红与假 approval 语音消失）、症状②（运行全程黄）、症状③（步骤未完结不判 Idle，单步 >60s 不再闪绿）。完成即绿（2026-09-17 裁决；原「完成后红→绿 60s 延迟」语义废除）。失败卡片的错误摘要（`error.name` + `data.message`）写入卡片消息行前缀（如 `❌ APIError: Invalid API key.`），与完成绿一眼可辨。

**残余已知限制**：外部编排器 team-mode 形态（AionUi/omo "Sisyphus"，本机 `ses_fae18a2f` 等老会话实证）只落 `text`/`patch` 部件、无 step 部件 → 回退启发式，编排器续跑的假绿窗口保留（记入 §7）。进程被强杀/崩溃且未落 `error` 字段的请求无内容信号可判 → 停留黄灯（「宁黄不假绿」方向，接受）。

**测试**（以本节取证样本为夹具）：
- 占位空窗夹具（末条 part=user text、最后消息=空 assistant 行）→ Processing 黄（症状①回归锁）
- `step-finish(tool-calls)` 在尾（4.3s 步间空窗样本）→ Processing 黄
- `step-finish(stop)` 在尾 → Idle 绿（完成即绿；新鲜时间戳也不红——回归锁），完成语音即刻触发
- step 进行中尾部（`step-start`/`reasoning`/`tool`/assistant `text` 无 step-finish，5.1s 流式窗口样本）→ Processing 黄
- 单步 60s+ 无新消息（模拟长工具）→ 不落 Idle（症状③回归锁）
- 无 step 部件的老格式会话（text/patch 尾）→ 现行为逐分支不变（降级零回归）
- user 消息在尾（无占位行的 130ms 窗）→ Processing（现行为保留）
- 末条消息 `APIError`（空占位行 + user 文本尾，`ses_f5574c39` 形态）→ Waiting 红（失败请求回归锁；error 判定优先于尾部 Running 短路——user 尾本身判黄）
- 末条消息 `MessageAbortedError`（同形态）→ Idle 绿（主动中止不提示，优先级同样高于尾部 Running）

## 4.4 Codex 长会话 digest 头尾拼接（2026-09-18 验收期抓获，随本 PR 修复）

**症状**：`codex resume` 长命会话（rollout > 500 行）正在运行却整卡消失（桌面与远程列表皆无），进程被同目录空闲兄弟会话抢占匹配（后者绿卡剔除后项目完全无卡）。活体取证：11.4MB / 2801 行 rollout，`session_id` 在状态缓存中从未出现。

**根因**：`read_codex_digest` 只读尾部 `RECENT_LINES=500` 行，而会话身份（`session_meta` 的 id/cwd）**只在文件第一行**——长会话头部被挤出尾窗 → digest 身份缺失整体作废（None）→ Phase 1 cwd 匹配跳过该文件；`fill_uncached` 回退同样产出 None，无法自救。

**修复**：头尾拼接——身份取文件头首行 `session_meta`，状态取尾窗 500 行，互不依赖；小文件头尾同窗零变化。纯内容函数，(mtime,size) 缓存契约不变（仅缓存未命中时多读一行）。Claude（头 20 行取 cwd）与 WorkBuddy（目录身份）本就免疫，仅 Codex 需修。

**测试**：600+ 行夹具（头部 meta + 超窗记账行 + 尾部工具活动）→ 进程匹配出卡且判 Processing（修复前红：0 卡）。

## 5. 明确不动清单

| 对象 | 理由 |
|---|---|
| Codex APP 路线 | `inProgress` 强信号已覆盖，作为 §4.1 语义参照 |
| ZCode | `step-finish(tool-calls)` 守卫已覆盖步骤间场景；流式落盘窗口未证实，不值得预防性改动 |
| Claude / Kimi | 消息内工具调用判据 + 轮次边界事件，结构免疫 |
| dsh | 轮次事实模型，正面教材 |
| OpenClaw | 纯 CPU 启发式的误报是另一族问题（无消息模型可依），单独立项 |
| 共享核 `derive_app_status` 契约 | 保持「最后一条说了什么」定位；回合感知以叠加仲裁层实现（§4.1），避免波及无信号消费者 |
| 前端（petStatus / FoxbellPet / 未读池） | 边沿触发链路本身正确，源头状态正确后无需改动 |

## 6. 测试与验收

1. 单测：§4.1/§4.2/§4.3 各自的测试清单全部落地（夹具优先取自本机真实数据脱敏）。
2. 回归：`cd src-tauri && cargo test` 全绿；`clippy -D warnings` 干净。重点回归组：`app_status` 既有判定核测试、codex `app_status_fixture_tests`（issue #6 样本）、WorkBuddy `derive_status_from_tail` 系列、OpenCode `status_tests`。
3. 手工验收：Codex CLI 跑一个 3+ 轮工具任务，全程黄灯、只在 `task_complete` 后一次绿灯一次语音；WorkBuddy 完成任务观察绿灯延迟约 10s 出现；OpenCode 提交指令瞬间转黄（无红闪、无假语音）、运行全程黄、结束后红→绿一次完成语音。

## 7. 已知限制与 follow-up

- **WorkBuddy 防抖漏防**：模型中间停顿 >10s 仍会漏放一次假绿（启发式上限）。
- **ZCode 流式落盘窗口**：text part 已写、step-finish 未写的瞬态窗口未证实是否存在假绿；待有实感症状再立项。
- **OpenCode 编排器续跑**：team-mode 外部编排形态（无 step 部件）无库内回合信号，假绿窗口保留。
- **OpenClaw CPU 误报族**：API 等待期 CPU 低可能假绿/假红，与本轮不同根因，单独立项。
- **方案 A 专属限制**：>500 行长回合窗口内无开闭对 → 回退现状可能假绿（用户裁决接受，§8 决策 2/5）。

## 8. 决策记录

| # | 决策点 | 结论 | 日期 |
|---|---|---|---|
| 1 | WorkBuddy 防抖思路是否接纳 | 接纳 | 2026-09-16 |
| 2 | Codex CLI 超窗（>500 行）回退策略 | 保持现状 + 文档化（方案 A 语境） | 2026-09-16 |
| 3 | WorkBuddy 防抖窗口 N | 10 秒（常量可调） | 2026-09-16 |
| 4 | OpenCode 延迟衰减变体是否本轮处理 | 本轮一并修（§4.3） | 2026-09-16 |
| 5 | Codex CLI 守卫方案 A vs B | **方案 A（开闭对，dsh 同款）**；B 记录为已否决备选 | 2026-09-16 |
| 6 | OpenCode 输入瞬间绿→红 + 假 approval 语音（用户追加报告） | 根因坐实（assistant 占位行 ~130ms + last_role 启发式误判），并入 §4.3 会话尾部部件守卫本轮一并修 | 2026-09-16 |
| 7 | OpenCode 部件映射是否抽共享（复查轮追加项的再裁决） | 不共享：ZCode 在不动清单且 `reason=length` 语义分歧（ZCode→TurnEnd / OpenCode→Running 宁黄不假绿），OpenCode 自实现可测纯函数 | 2026-09-16 |
