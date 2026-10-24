# ZCode（智谱）APP 形态工具接入设计 —— 监控提醒侧 + 资源管理侧

> **2026-10-24 近期设计补充（待用户确认）：** 见 [ZCode 读取恢复、工具设置卡与统一远控界面设计 v3](2026-10-24-zcode-unified-remote-design.md)。当前拟恢复新旧 ID 读取、增加工具配置卡并研究官方协议接入统一控制 UI；本文件的历史实测仍保留，宿主／状态／未读／资源管理规则继续有效。会话 ID 合法域按新稿双形态更新；不把本补充当成已实施或已批准的执行计划。

- 日期：2026-09-08
- 状态：待用户审阅
- 前置事实源：
  - macOS 本机实测（ZCode v3.11.2，`/Applications/ZCode.app`，本机运行中的真实会话数据库）
  - Windows 探测报告：`research/zcode-windows-probe-2026-09-07.md`（Windows v3.11.2.6792）
  - 前序设计：`docs/superpowers/specs/2026-09-03-workbuddy-app-jump-tool-toggle-design.md`（APP 类通用规则的来源）

---

## 1. 背景与目标

MAM（MultiAgents-Manager）是 Tauri 2 桌面应用，统一监控多个 AI 编程工具：首页为每个进行中的会话显示卡片（状态灯 + 标题 + 项目名），任务完成弹未读提醒（绿卡 + 系统通知 + 声音 + 桌宠气泡），点击卡片跳回对应工具；资源管理页对 skill / MCP / 插件做全局仓库（SSOT）入库与按工具分发。

WorkBuddy 一轮（2026-09-03 spec）已把「APP 形态工具」的全部通用机制泛化为按会话形态（`ProcessForm::App`）门控的共享基建：未读卡、已读信号、跳转链、宿主清理、工具开关。**本次新增 ZCode，两条链路：**

```
消息提醒侧：会话发现 → 状态推导 → 转绿未读 → 跳转回 ZCode
资源管理侧：skill 分发链接 + MCP 配置读写
```

范围外（明确不做）：ZCode 插件管理（marketplace 结构，另行立项）、hooks 注入（Windows 落点存疑，见 §10）、`AGENTS.md` 指令文件管理（MAM 不纳管任何工具的指令文件）、CLI 形态（ZCode 无独立 CLI 进程形态，双平台实测确认）。

### 用户已确认的决策

| # | 决策 | 结论 |
|---|------|------|
| D1 | 卡片粒度 | 数据库聚合式：**每会话一卡**（不按项目压缩、不按进程绑定） |
| D2 | 跳转后标已读 | **跳转成功（前台验证通过）即标已读**（方案 A），与 WorkBuddy/Codex 一致 |
| D3 | 资源管理范围 | **Skill + MCP**；Plugin 不做（方案 A） |
| D4 | hooks | v1 **不做**，纯轮询（Windows hooks 落点存疑，且 WorkBuddy 前例为纯轮询） |
| D5 | 子代理等待降级修复 | **写成通用函数**（共享核「后代活跃度仲裁」），ZCode 提供提取器，WorkBuddy/Codex 返回空列表（行为不变） |

---

## 2. 调研结论事实表

所有结论均标注证据来源。ZCode 的本地数据全部是**无文档私有格式**，解析必须防御（字段缺失/格式不符 → 跳过该会话，不 panic、不影响其他工具）。

### 2.1 进程模型

| 平台 | 事实 | 来源 |
|------|------|------|
| macOS | Electron 主进程 `ZCode`（bundle id `dev.zcode.app`）+ 各类 `ZCode Helper`；宿主 `zcode-host-local-1`；会话运行时 `zcode-cli`（**cwd = 项目目录，命令行无任何参数**）；MCP 边车 `zcode-node-repl-mcp` | 本机 ps 实测 |
| Windows | **全部可执行体都是 `ZCode.exe`**。主进程 = 裸命令行（父进程 explorer）；Electron 辅助进程带 `--type=`；会话运行时 = `"…ZCode.exe" "…\resources\glm\zcode.cjs" app-server --stdio --surface desktop`；插件宿主带 `__zcode-plugin-host` | Windows 报告 A1 |
| Windows | **进程数量与任务数无关**：5 个 app-server 常驻池化（40s 子代理任务期间 6 次采样恒为 5/16），子代理与主会话共享进程（子代理在 session 表有独立行，`parent_id` 指向主会话） | Windows 报告 A2 |
| 双平台 | 无心跳文件机制（区别于 WorkBuddy 的 `~/.workbuddy/sessions/<PID>.json`） | 双平台目录实测 |

**结论：进程侧只做「宿主判定」（应用开没开），不做会话级发现——会话唯一真相源是数据库。**

### 2.2 数据文件布局（`~/.zcode/`，双平台一致）

| 文件 | 内容 | 关键点 |
|------|------|--------|
| `v2/tasks-index.sqlite` → `tasks` 表 | 任务级索引 | `task_id`（= 会话 id，`sess_<uuid>` 形态）、`title`（ZCode 侧栏同款任务标题）、`task_status`、`workspace_path`、`unread_at`/`last_unread_at`（ZCode 自带未读追踪，**MAM 不使用**，见 §5）、`archived`、`deleted`、`pinned`、`updated_at`（毫秒） |
| `cli/db/db.sqlite` → `session` / `message` / `part` 表 | 会话正文与消息流 | `session.id` / `session.parent_id` / `session.directory`（= 项目绝对路径）/ `session.task_type` / `session.time_updated`（毫秒，**活动期间实时刷新**）/ `session.title`（标题降级源）；`message.data` 内含 `role` 与 `semantics.kind`；`part.data` 内含 `type` |
| `cli/rollout/model-io-sess_<uuid>.jsonl` | 每次模型请求一行（含 `durationMs`） | 默认精简保留（`modelIoFullRetentionEnabled: false`，Windows 仅存 3 个文件），**不可作为稳定数据源** |
| `cli/config.json` | ZCode 主配置（MCP + 插件启用态） | 顶层键实测：`mcp`（含 `servers`）、`plugins`（含 `enabledPlugins`）；官方文档另载 `hooks` 键（7 个 PascalCase 事件），但 Windows 实机无此键 |
| `~/.zcode/skills/` | 用户级 skill 目录（官方文档） | **双平台均尚不存在**（首装时创建），「ZCode 是否真读取」待实测（§6、§10） |
| `~/.agents/skills/` | 跨工具共享 skill 目录 | ZCode 确认读取（本机 ZCode 会话实测在用）；MAM 备选方案（Plan B） |

两个 SQLite 库均为 WAL 模式，**ZCode 运行中以 `file:…?mode=ro` 只读打开双平台实测可行**（Windows 用 Python stdlib、macOS 用 sqlite3 CLI 验证）。

### 2.3 关键字段取值（实测枚举）

| 字段 | 取值 | 备注 |
|------|------|------|
| `session.task_type` | `interactive` / `subagent_child` / `selection_side_chat` / `fork` | `fork` 仅 Windows 实测出现；MAM 只收 `interactive` |
| `tasks.task_status` | `running` / `completed` / `error` | **Windows** 任务启动即写 `running`（含子代理等待期全程保持），完成写 `completed`，失败 `error`；**macOS 旧任务续跑不翻回 running**（实测：正在运行的任务显示 `completed`）→ 只能当加速提示，不能当状态主源 |
| `message.data.role` | `user` / `assistant` | 另有 `semantics.kind`：`user_prompt` / `assistant_response` / `todo_reminder` / `timeline_event` |
| `part.data.type` | `tool`(11082) / `step-start` / `step-finish`(9630) / `text`(7170) / `reasoning`(6572) / `timeline` / `file` / `compaction` | 括号为本机全库计数；`step-finish` 可带 `reason:"tool-calls"`（表示本步以工具调用收尾、后续还有动作） |
| `session.directory` / `tasks.workspace_path` | macOS 正斜杠；Windows **反斜杠 + 大写盘符**（`E:\LLMproject\…`） | 比对/展示前需归一化 |
| 会话 id | 当前需兼容 sess_＋严格 UUID 与 session_＋32 ASCII hex 两种主会话形态 | 2026-10-24 双平台证据修订；子代理前缀 sess_subagent_agent_ 不通过主会话 ID 门，非空 parent 继续排除 |

### 2.4 消息落库节律（状态推导的依据，全部实测）

1. **单次模型请求的 parts 成批落库**：`step-start`/`reasoning`/`text` 同秒写入（响应完成时一次写入），思考过程中无中间写入。
2. **`step-finish` 懒落库**：回合的收尾 `step-finish` 在流关闭（通常是用户发下一条消息）时才写入——其**时间戳不可靠，但顺序可靠**（永远排在所属消息尾部）。尾部推导必须按 `sequence` 倒扫，不得按时间戳。
3. **子代理执行期间主会话零写入**：实测 17 分钟子代理窗口内主会话 0 条 message/part；尾部停留在「工具调用已发出、结果未回」状态。
4. **子代理自身持续落库**：实测 17 分钟子代理会话共写 51 条消息（22:51–23:05 每 3 分钟段均有活动）——「后代活跃度仲裁」（§4）的数据前提成立。
5. **单请求时长上限**：本机全部 66 次模型请求，最长 204 秒，无一次超过 300 秒——纯思考触发 300 秒误降级在实测中从未发生（残余风险见 §10）。

### 2.5 深链与安装形态

| 项 | 事实 | 来源 |
|----|------|------|
| scheme | `zcode://` 已注册（macOS Info.plist `CFBundleURLSchemes`；Windows 注册表 `HKCU\Software\Classes\zcode` 有 `URL Protocol` 值，`shell\open\command` = `"…\ZCode.exe" "%1"`） | 双平台实测 |
| 路由全集 | app.asar（307MB）全量正则枚举，**仅 3 条**：`zcode://oauth/callback`、`zcode://payment/callback`、`zcode://workspace/open`；无动态拼接模板字符串残留 | 双平台各测一次，一致 |
| 工作区深链 | `zcode://workspace/open?path=<URL编码的项目路径>`：Windows GUI 实测**有效**——同一主窗口内切换到目标项目工作区（不新开窗口），后台任务不受影响 | Windows 报告 C3b |
| 会话级路由 | **不存在**（双平台确认）：`task/open?id=`、`session/<id>` 无任何反应；`workspace/open?task=` 的 task 参数被忽略（缺 path 时回退最近工作区）——区分实验证伪 | Windows 报告 C4 |
| 安装形态 | Windows 常规 NSIS 式安装（`D:\Program Files\ZCode\`，自定义盘符），**非 MSIX**；exe 完整路径可从注册表 `shell\open\command` 动态解析 | Windows 报告 D1/C1 |
| 关窗行为 | `closeToTrayOnWindows: true`（配置证据）+ 托盘图标资源存在；GUI 行为未实测。宿主判定以**进程存活**为准，与窗口可见性解耦 → 关窗驻留托盘时卡片正确保留 | Windows 报告 A3 |

---

## 3. 监控侧：发现与卡片模型

**定性**：回答「该显示哪些 ZCode 会话、每张卡对应什么」。ZCode 无法像 CLI 工具那样扫进程发现会话（§2.1：进程与任务数无关），也无法像 WorkBuddy 那样数心跳文件（不存在心跳），唯一可靠的会话清单在本地数据库里。因此发现机制是「**进程只判活，会话靠数据库**」，与 Codex APP 的「宿主在场 + 会话文件聚合」同构，只是数据源从文件换成数据库查询。

**功能内容**：

1. **宿主判定**——输入：全量进程快照（MAM 每轮扫描已持有）；输出：ZCode 主进程（0 或 1 个）。它是 ZCode 全部卡片的总开关：宿主不在场不出活跃卡；宿主退出触发该工具全部卡片（含未读卡）清理。
2. **会话枚举**——输入：两个 SQLite 库（只读连接，每轮打开即关，复用现有 `open_readonly_with_timeout` 共享 helper）；输出：符合条件的会话清单。每次轮询（30 秒周期）执行。
3. **卡片生成**——每个枚举到的会话生成一张卡：标题（`tasks.title` → 降级 `session.title` → 降级首条用户消息截断）、项目名（复用 `monitor::project::project_name_from_path`）、`github_url`（复用 `monitor::git::get_github_url`，与 WorkBuddy/Codex 卡片字段对齐）、`last_message`（尾部正文条目截断，未读卡展示用）、`pid = 宿主 pid`、`form = App`（挂接 APP 类全部通用规则的键）。

**关键函数与参数**：

- 宿主判定规则（`is_host_process` 新增 `"zcode"` 臂）：
  - macOS：可执行路径含 `ZCode.app/Contents/MacOS` 且进程名恰为 `ZCode`（排除 `ZCode Helper`、`zcode-cli`、`zcode-host-local-1`、`zcode-node-repl-mcp`——它们不是宿主）。
  - Windows：进程名为 `ZCode.exe` 且命令行**不含** `--type=`（Electron 辅助）**且不含** `zcode.cjs`（app-server 池与插件宿主）——三类进程共用同一 exe 名，必须查命令行。
- 会话枚举 SQL 语义（实际实现可合并查询）：
  ```
  主源（db.sqlite.session）：
    WHERE task_type = 'interactive'
      AND time_updated > (now - 24h)          -- 聚合窗口，与 Codex APP 一致
  辅源（tasks-index.tasks）：按 task_id 对齐，取 title；archived/deleted 在此过滤
  ```
  - `deleted=0` 且 `archived=0`（尊重用户在 ZCode 内的删除/归档操作）。
  - 24 小时窗口：超窗的老会话不再出卡；若它此前已转绿，未读卡按通用规则自行过期（24h 兜底），时序自洽。
- 路径归一化：Windows 反斜杠 + 大写盘符 ↔ POSIX 正斜杠，比对前统一（项目名提取、跳转 path 构造共用）。
- 会话 id 防御（2026-10-24 新稿待确认）：允许 sess_＋严格 UUID 或 session_＋32 ASCII hex；两者均保留原始 ID，坏形态跳过。仍需满足 interactive、空 parent、窗口和归档等独立准入门；详细契约见本轮补充设计 §5。旧实现仅接受第一种是本轮漏卡根因。

---

## 4. 监控侧：状态推导

**定性**：回答「这张卡此刻是运行中、空闲还是疑似卡住」。ZCode 没有跨平台可靠的现成状态字段（`task_status` 仅 Windows 可用，§2.3），状态靠**读消息流尾部**推导：一个对话回合若还在工具执行中，尾部必然停在「工具调用已发出」；若回合结束，尾部必然出现助手正文。这套「尾部倒扫」判定已是 APP 类共享核（`monitor/app_status.rs::derive_app_status`），WorkBuddy/Codex 在用，ZCode 只需写一个「格式翻译」函数。

**功能内容**：

1. **尾部推导（主源）**——输入：该会话消息流的语义条目序列（旧 → 新）；输出：状态。翻译映射（ZCode 消息 → 共享核 `AppEntryKind`）：

   | ZCode 尾部形态 | AppEntryKind | 状态 |
   |---|---|---|
   | `role=user` 的最后语义条目 | UserMessage | Thinking（刚发话） |
   | `part.type=text`（助手正文收尾） | AssistantMessage | Idle（回合完成） |
   | `part.type=tool`（工具调用挂起，无收尾正文） | ToolCall | Processing（运行中） |
   | `reasoning` / `step-start` / `timeline` / 记账类 | Other | 跳过，继续倒扫 |

   - 倒扫按 `sequence`，**不按时间戳**（`step-finish` 懒落库，时间戳不可靠，§2.4-2）；`step-finish` 本身归入记账类跳过（其后的 `text`/`tool` 条目定状态）。
   - 语义过滤：`semantics.kind = todo_reminder / timeline_event` 的 user 行按记账条目跳过——实测 ZCode 会在回合中途注入 todo 提醒（user 角色），若按用户消息处理会误判 Thinking。
2. **`task_status` 加速提示**——`tasks.task_status='running'` 时直接判 Processing（省一次尾部查询）；`='error'` 按**完成**处理（转绿，提醒用户去看失败结果）；其余情况不影响尾部推导。仅 Windows 受益，macOS 该字段恒旧值也不出错（加速提示单向生效）。
3. **停更降级 + 后代活跃度仲裁（D5，本次新增的通用函数）**——输入：会话自身状态、自身 `time_updated` 年龄、后代（子代理）会话最近活动年龄（可选）；行为：
   - 状态为 Processing 且自身停更 ≥ **300 秒**（共享阈值 `APP_STATUS_STALE_MS`）时：
     - 若存在后代会在 **300 秒内有活动 → 保持 Processing**（子代理在跑，主会话健康等待）；
     - 否则降级 **Waiting（疑似卡住）**。
   - 其余状态不受影响（Idle 是明确完成信号，永不拉回）。
   - **落点：写成 `app_status.rs` 的纯函数**（如 `overlay_stale_with_descendants(status, own_age_ms, descendant_age_ms: Option<u64>)`），不感知任何工具细节；各工具实现「子会话提取器」：
     - ZCode：一句 SQL——`SELECT MAX(time_updated) FROM session WHERE parent_id = ?`；
     - WorkBuddy / Codex：返回空（行为与今天完全一致）。已实测 WorkBuddy 的 `parentId` 是**会话内消息 DAG**（126 个值全部指向同文件消息 id，与 session id 交集为 0），不构成会话级父子——留接口位，待其数据出现即插即用。

**关键参数**：轮询周期 30 秒（全局一致）；停更阈值 300 秒；后代活跃窗口 300 秒；子代理等待场景实证——主会话 17 分钟零写入但子代理持续落库（§2.4-3/4），仲裁后全程保持运行中。

---

## 5. 提醒与跳转

**定性**：任务从运行中翻为空闲（或 error）的瞬间，该会话进入 MAM 的**未读池**——一张持久绿卡 + 系统通知 + 声音 + 桌宠气泡，跨 MAM 重启保留，直到出现已读信号或 24 小时过期。此整套机制是 WorkBuddy 轮泛化的 APP 类通用规则（`unread_sessions` 表、`sync_unread_sessions` 管线、前端绿卡/X 按钮、桌宠气泡），**按 `form=App` 自动生效**。本节定义 ZCode 特有的跳转、已读行为，以及一处必须随 Codex 同款扩展的在板未读态（第 6 条，未读链路唯一的新代码）。

**功能内容**：

1. **跳转两级链**（ZCode 无会话级深链，跳转精度上限 = 直达项目工作区，§2.5）：
   - **第一级 工作区深链**：构造 `zcode://workspace/open?path=<percent-encode(项目原生路径)>`（macOS 正斜杠 / Windows 反斜杠大写盘符，整段编码）。派发前校验协议处理器：macOS 看 `open <url>` 非零退出码；Windows 查 `HKCU\Software\Classes\zcode` 的 `shell\open\command` 有值或存在 `URL Protocol` 标记（WorkBuddy 整改沉淀的 MSIX 假阴性防御判据）。未注册 → 直接走第二级。
   - **第二级 APP 级激活兜底**：macOS 激活 `ZCode.app`（bundle `dev.zcode.app`，挂入现有 bundle 匹配表）；Windows 将 `ZCode.exe` 主窗口拉前台（窗口认领关键字 `zcode`；激活目标 exe 路径可从注册表 `shell\open\command` 动态解析，不硬编码安装盘符）。
2. **前台验证**：深链派发后 2 秒内轮询前台窗口归属，前台进程属于 ZCode 才算成功；失败落兜底且**不标已读**（派发成功 ≠ 用户看到，P1-2 分层防御沿用）。
3. **标已读（D2）**：两级链中**任一级成功（前台验证通过）即标已读**（深链直达工作区或兜底激活拉前台，与 WorkBuddy/Codex 的「点卡跳转 / 兜底激活成功均算」口径一致）——绿卡消失、写墓碑防复插、广播 `session-read` 事件（桌宠等辅助窗口同步）；同项目其他未读卡保留。兜底信号：手动 X 关闭、24 小时过期。
4. **宿主清理**：ZCode 主进程退出 → 该工具活跃卡 + 未读池全清（复用 `tool_host_alive_in` → `clear_tool`）。Windows 关窗驻留托盘时进程仍活，卡片正确保留。
5. **不使用 ZCode 自带未读**：`tasks.unread_at` 是 ZCode 侧栏自己的未读标记，语义与 MAM 未读池不同步（用户在 ZCode 内查看不清 MAM 的卡，反之亦然）——按通用规则显式忽略，与 WorkBuddy 轮「不做 APP 内切换检测」的决策一致。
6. **在板未读态（Codex 分支扩展，未读链路唯一的新代码）**：ZCode 聚合卡与 Codex 同为「完成后仍持久在板」（24h 窗口内、宿主存活即显示）。空闲卡在板期间必须呈现未读态（绿徽标 + 未读卡排后）——现行管线中该行为由 **Codex 专属分支**承担（`sync_unread_sessions` 内 `if tool == "codex" { s.unread = true; }`，代码注释明示「聚合卡即该会话的未读卡形态」），ZCode 接入 = 该分支扩展为 `matches!(tool, "codex" | "zcode")`。WorkBuddy 不需要此分支（其活跃卡随进程退出离板，由未读池接管渲染——spec §5 双形态语义）。

**关键参数（实现后修正，PR #46 review M6）**：spec 原定扩展 `session_url` 签名为「会话 id + 项目路径」双参数；实现期间（2026-09-09 实机验收）发现 `zcode://workspace/open` 有无条件信任弹窗、落点为新会话 composer、每次派发拉起转发进程三项副作用，**ZCode 深链整体移出跳转链**——跳转改为 pid 单窗口路径直接聚焦唯一窗口（ZCode 单窗口多标签），失败由 `reactivate_tool_app` 兜底（认领关键字 `zcode`）。`session_url` 维持原签名未扩展，也未引入 `jump_url`/`workspace_url`（曾短暂引入后随深链移除一并删除）。本节为设计演进记录，行为以 `deep_link.rs` 与 `session.rs` 注释为准。会话 ID 按 §3 的双形态合法域校验，原生跳转不因此改成官方 session 深链。

---

## 6. 资源管理侧

**定性**：MAM 资源管理页的两条分发链。**skill**：全局仓库 `~/.mam/skills/<名>` 是唯一真身（SSOT），每个工具的激活目录里放指向真身的符号链接——启用 = 建链，禁用 = 删链（工具看不见该 skill，真身保留）。**MCP**：每个工具有自己的配置文件，MAM 往里增删服务器条目（名字 + `command` + `args` + `env` 四件套）。ZCode 两条链分别接入如下。

**功能内容**：

1. **Skill 分发**：激活目录（Layer 2）定为 `~/.zcode/skills/<名>`（官方文档声明的用户级目录，双平台同路径）。实现 = `skill_dirs()` 返回 `primary_skill_dir("zcode")`，与 `skill_dir_for_tool` 注册臂保持一致（WorkBuddy 同款约定）。子代理级（Layer 3）不做：ZCode 子代理经插件系统配置，无用户级子代理 skill 目录。
   - **首验钩子（实现阶段第一步）**：安装一枚真实 skill 到 `~/.zcode/skills/`，实测 ZCode 能否发现。若不识别 → Plan B：改挂 `~/.agents/skills/`（ZCode 确认读取），但该目录是跨工具共享（其他工具同读），与 MAM 每工具独立激活的模型冲突，需重新设计启用/禁用语义——触发时回到用户决策。
2. **MCP 读写**：落点 `~/.zcode/cli/config.json` → `mcp.servers` 子树。该文件是 ZCode **主配置**（与 `provider`、`plugins` 等键共存，Windows 实测样本含 playwright/context7/firecrawl 三服务器），写入策略：
   - 整文件读入 → 仅修改 `mcp.servers` 子树 → **保序写回**（JSON 对象维持原键序，未知键原样保留——serde_json `Map` 保序特性，WorkBuddy `mcp.json` 保序写入同款要求）；
   - 新增 `McpFormat` 变体（如 `ZcodeConfigJson`）：读 = 展开 `mcp.servers` 条目入面板；写 = 增/删/改只动该子树；
   - 解析失败只报错不落盘（绝不写坏用户主配置）；
   - ZCode 可选字段 `enable: false`（停用标记，无字段 = 启用）：读取照实展示为停用，MAM 写入时不主动添加该字段；
   - 与 ZCode 设置界面的写入交替发生，双方均为「读-改-写」，冲突窗口为一瞬间，可接受（风险条目见 §10）；
   - **面板读取链路（遗留硬编码，随本次接入顺带修复）**：MCP 面板的读取命令 `commands/mcp.rs::read_mcp_servers` 当前硬编码 claude/codex/opencode 三工具（`_ => Err("未知工具")`），**WorkBuddy 的 MCP 读取今天就已缺失**。ZCode 接入时将该函数改为 `adapter_by_id` 统一分发，一并修复 WorkBuddy。
3. **Plugin / Hooks**：`plugin_dirs()` / `plugin_config_paths()` 返回空、`hook_supported()` = false（与 WorkBuddy 同款空实现；ZCode 插件是 marketplace + 版本化缓存 + `enabledPlugins` 三方结构，另行立项）。

**关键参数**：ZCode MCP 条目形态（Windows 实测）：`"playwright": {"command": "…", "args": […], "env": {}}`——与 MAM 现有 MCP 模型同构。工具开关（W5）自动收纳：`TOOL_IDS` 注册后，设置页开关自动出现、默认启用；取消勾选的清理语义自动套用（skill 链接还原为真实文件、MCP 条目移除，SSOT 保留）。

---

## 7. 注册点清单（改动面）

WorkBuddy/Kimi 两轮已模板化，ZCode 按模板执行。**数据库零 schema 变更**（`agent_tools` 启动时按 `TOOL_IDS` 幂等种子行，`unread_sessions` 等以字符串 tool_id 为键）。

**后端（约 14 处）**：`AgentType` 枚举加 `ZCode`（小写 = `"zcode"`）；`adapter/mod.rs` 的 `TOOL_IDS` / `adapter_by_id` / `skill_dir_for_tool` / `sync_unread_sessions` 的 Codex 在板未读分支扩展（§5-6）；新建 `adapter/zcode.rs` + `monitor/zcode_parser.rs`（含 `monitor/mod.rs` 模块声明）；`monitor/host.rs::is_host_process` 加臂；`window/deep_link.rs::session_url` 加臂（含签名扩展）+ 测试；`window/app_activation.rs::bundle_matches_agent` 加臂；`window/win32.rs` 的 `TOOL_CLAIM_KEYWORDS` 窗口认领关键字 + `reactivate_tool_app` 白名单（`matches!(t, "workbuddy" | "codex")` 加 `"zcode"`）；`commands/session.rs:80` Windows 深链白名单；`services/mcp` 新 `McpFormat` 变体。

**遗留硬编码顺带修复（三处均现缺 workbuddy，随 zcode 接入一并补）**：
1. `commands/mcp.rs::read_mcp_servers` — 硬编码 claude/codex/opencode → 改 `adapter_by_id` 分发（§6）；
2. `services/resource/mod.rs::detect_source_tool` — 硬编码 `["claude","codex","opencode","openclaw"]` → 补 `workbuddy` + `zcode`（资源导入溯源用）；
3. `src/config/constants.ts::SUPPORTED_TOOLS` — 现为五工具缺 `workbuddy` → 补 `workbuddy` + `zcode`（测试遍历断言用）。

**前端（约 5 处）**：`types/session.ts` AgentType 联合；`agentBadge.tsx` 徽标（label「ZCode」）；`ToolIcon.tsx` 官方图标（本机 `icon.icns` 取样重绘，同 WorkBuddy 轮做法）；i18n 无 per-tool 必需词条（工具名走后端，`zh.json` 的 `emptyHint` 文案可选更新）；`tauri-mock.ts` mock 行 + 测试用例。另：README / README.en / CHANGELOG 工具表更新。

---

## 8. 跨平台要求

- 双平台门禁：macOS `cargo test/clippy` + `pnpm check`；Windows 侧沿 WorkBuddy 轮经验做 `x86_64-pc-windows-gnu` 交叉编译验证（注意上轮 E0460/E0463 编译竞态的应对）。
- 平台差异全部落在 adapter/parser 内部（宿主判定规则、路径归一化、深链编码），不得外溢到共享层。
- Windows 未实测项（§2.5 关窗行为、深链冷启动、后台拉前台）不阻塞设计：宿主判定以进程存活为准已解耦窗口可见性；深链冷启动由前台验证失败落兜底覆盖。

## 9. 测试策略

- **fixture 用真实数据**（上轮 P0-2 教训：合成样本会把错误形态锁进测试）：从本机双库导出脱敏样本——含子代理窗口（主会话静默 + 子会话活跃）、error 完成态、Windows 反斜杠路径行、`fork`/`selection_side_chat` 过滤行。
- **状态推导表格用例**：§4 映射表逐行 + 边界（全 Other 条目、空会话、`step-finish` 懒落库顺序）。
- **后代活跃度仲裁纯函数单测**：有后代活跃/后代停更/无后代（`None`）/非 Processing 状态四类 + **回归用例「空提取器 = 现行行为」**（WorkBuddy/Codex 不变的保证）。
- **GUI 手动清单（双平台）**：跑任务 → 卡片出现与状态流转；子代理长任务 → 全程保持运行中；完成 → 转绿 + 通知 + 点卡跳转工作区 + 标已读；退出 ZCode → 卡片全清；关窗驻留托盘（Windows）→ 卡片保留；装 skill → ZCode 发现；MCP 面板增删 → ZCode 设置界面同步；强制长思考回合（>5 分钟）边界观察。

## 10. 风险与已知限制

| 风险 | 应对 |
|------|------|
| 双库为无文档私有格式，ZCode 升级可能改表 | 防御性解析（缺列/类型不符 → 跳过该会话并降级，绝不 panic、不影响其他工具）；字段访问集中在 parser 单文件便于跟进 |
| `config.json` 与 ZCode 设置界面共写 | 保序读-改-写 + 解析失败不落盘；极端并发丢一次编辑可由用户在 ZCode 界面重做，风险自留 |
| `~/.zcode/skills` 读取未实测 | 实现阶段首验；Plan B（`~/.agents/skills`）已备案，触发即回用户决策 |
| 单次思考 >300 秒且零中间落库（实测未发生，理论上存在） | 短暂显示 Waiting，响应落库后 30 秒内恢复，不漏转绿；实现阶段加长思考实测用例 |
| macOS `task_status` 恒旧值 | 设计已规避（加速提示单向生效，主源是尾部推导） |
| Windows 未测项（关窗/冷启动深链/后台拉前台） | 进程存活判定解耦窗口；前台验证失败自动落兜底；列入 GUI 清单待实测 |
| 深链 `workspace/open` 为未文档化路由（ZCode 自用入口） | 派发前 handler 校验 + 前台验证 + 兜底三层防御；版本升级若失效自动降级为 APP 级激活 |

## 11. Commit 划分

执行阶段按 TDD 细分，最终按功能块合并（沿用 WorkBuddy 轮流程）：

1. `feat(monitor)`: zcode adapter + 解析器（DB 聚合发现 / 宿主判定 / 状态推导格式翻译 / 后代活跃度仲裁通用函数 + 提取器）
2. `feat(jump)`: 工作区深链（`session_url` 签名扩展）+ 两级跳转全链注册
3. `feat(resources)`: skill 链接 + `McpFormat` 新变体（config.json 保序读写）
4. `feat(ui)`: 徽标 / 官方图标 / 词条 + 文档

## 12. 修订记录

- 2026-09-08 初版：macOS 本机实测 + Windows 探测报告（`research/zcode-windows-probe-2026-09-07.md`）定稿；用户确认 D1–D5；子代理/长思考两个边界问题以实测数据闭环（§2.4、§4）。
- 2026-09-08 二审（对照当前 main 实现逐项对账）：① 修正 §5「未读零新代码」表述——Codex 在板未读分支（`sync_unread_sessions` 的 `if tool == "codex"`）须扩展至 zcode（§5-6），ZCode 聚合卡与 Codex 同为持久在板形态；② 补齐三处遗留硬编码匹配点（`read_mcp_servers` / `detect_source_tool` / `SUPPORTED_TOOLS`，均现缺 workbuddy，随本次顺带修复，§7）；③ 补标复用函数 `project_name_from_path` / `get_github_url`（§3）；④ 全部接口名与现行实现对账无误（`is_host_process(exe_lower, tool_id)`、`session_url(agent_type, session_id)`、`bundle_matches_agent`、`TOOL_CLAIM_KEYWORDS`、`reactivate_tool_app` 白名单、`McpFormat::{Json,Toml,Jsonc}`、`open_readonly_with_timeout`、`is_strict_uuid_form`、`primary_skill_dir` 均核实存在且签名一致）。
