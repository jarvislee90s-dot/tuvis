# 设计规格：WorkBuddy 适配 + APP 跳转与已读机制 + 工具勾选管理

- 日期：2026-09-03（2026-09-04 按 Windows 实机测试修订，见 §11 修订记录）
- 分支：`feat/workbuddy-app-jump-tool-toggle`
- 状态：已与需求方逐项对齐（本文档为对齐结果的书面化）
- 实现顺序：W1 → W2 → W3 → W4 → W5（小→大依赖序，单分支分阶段 commit）

## 1. 背景与目标

本次迭代包含三个 feature 与一个 bug 修复，相互关系：

1. **WorkBuddy 适配器**：新增支持腾讯 WorkBuddy（Electron APP 形态的 AI 办公工作台，内置 codebuddy CLI 运行时）。
2. **工具勾选管理**：设置页新增「工具管理」，用户可勾选启用的工具；未勾选工具在会话监控、通知、资源管理中彻底隐藏，解决看板「按资源分布」横向拥挤问题。
3. **APP 类跳转 + 已读机制**：跳转优先 session 级直达（深度链接 `codex://`/`workbuddy://`）、APP 级激活保底，macOS 补齐激活路径；APP 类工具的看板卡片改为「活跃卡 + 持久未读卡」双驱动，绿色未读卡保留直到用户查看。
4. **通知面统一 + 宠物气泡 bug 修复**：宠物开启时气泡成为唯一通知面；修复气泡点击跳转失败后永远点不掉的 bug。

### 调研结论（设计依据）

| 事实 | 来源 |
|------|------|
| WorkBuddy 是 Electron APP（macOS `/Applications/WorkBuddy.app`，VSCode 同源架构），agent 运行时为内嵌 `cli/bin/codebuddy`。<br>**Windows 实测（2026-09-04，v2.115.0）**：独立目录安装（NSIS 形态，如 `D:\Program Files\WorkBuddy`，非 MSIX），宿主 `WorkBuddy.exe`；`cli/bin/codebuddy` 为 Node 脚本（无 .exe），由 `WorkBuddy.exe` 以 Electron/Node 模式重新执行自身来运行（日志证据 `[Sidecar] Creating session … — …\WorkBuddy.exe`）——**Windows 上不存在名为 codebuddy 的进程，会话宿主进程名与 APP 主进程同为 WorkBuddy.exe** | 本机进程观测 + Windows 实测（日志/进程表/文件头） |
| `~/.workbuddy/sessions/<PID>.json` 心跳文件写明 `pid ↔ sessionId ↔ cwd ↔ lastHeartbeat`，是进程-会话关联的权威来源 | 本机文件 |
| 会话历史在 `~/.workbuddy/projects/<路径编码>/<sessionId>.jsonl`，OpenAI 风格 `type/role/content` 格式（实测行类型 `message`/`function_call`/`function_call_result`/`reasoning`，角色 user/assistant，与解析器状态映射一致）。<br>**路径编码实测**：POSIX 为去首 `/` 后 `/`→`-`；Windows 为**盘符小写 + 去冒号 + 分隔符→`-`**——`C:\Users\bunny\WorkBuddy\2026-08-06-15-57-15` → `c-Users-bunny-WorkBuddy-2026-08-06-15-57-15`，`E:\LLMproject\0807` → `e-LLMproject-0807`（编码以 WorkBuddy 自身写盘形态为准，心跳 cwd 中的大写盘符在编码时统一转小写） | 本机文件 + Windows 实测目录 |
| 会话标题在 `~/.workbuddy/workbuddy.db`（SQLite）`sessions.title` | 本机数据库 |
| `~/.workbuddy/mcp.json` 为标准 `mcpServers` JSON（与 Claude 同构）；skills 在 `~/.workbuddy/skills/<name>/SKILL.md` | 本机文件 |
| WorkBuddy 无 hook 机制，状态提取只能轮询（与 Codex 实际以轮询 JSONL 为主一致） | 文档+配置核查 |
| 干扰进程两类，Windows 实测（v2.115.0，14 份心跳样本）**均写心跳文件**：<br>① `--serve`/CLI 宿主服务：sessionId 为 `interactive-<pid>`（`kind:"interactive"`，cwd 在临时 `workbuddy-host-cli` 目录）；<br>② prewarm 池：**心跳持续新鲜**（非原文假设的「空闲时无心跳/心跳过期」），sessionId 形如 `prewarm-wb-pool-<13位ms>-<6位hex>`——**恰为 36 字符 4 连字符，仅凭「长度 36 + 连字符 4」判定 UUID 会被骗过**——心跳含 `kind:"prewarm"`、`meta.status:"idle"`；<br>真实任务会话：sessionId 为标准 UUID（8-4-4-4-12 **hex** 字符集）、`kind:"interactive"`、cwd 为真实工作区。<br>→ 过滤必须用**严格 UUID 形态**（hex 字符集校验），建议叠加 `kind != "prewarm"` 防御 | 本机进程+文件 + Windows 实测心跳样本 |
| Codex APP 原生宠物可跳转的原因：宠物是 APP 自身窗口（同进程激活无限制）。外部进程的等价能力：Windows 用 `SetForegroundWindow`（MAM `win32.rs` 已实现且覆盖 App 类）；macOS 用 AppleScript `activate application`（本次新增） | 代码+原理分析 |
| ChatGPT.app 注册了 `codex://` URL scheme；WorkBuddy.app 注册了 `workbuddy://` Deep Links scheme → 深度链接为 session 级直达的**第一顺位**（路由格式实现期探测，APP 级激活保底）。<br>**Windows 注册表实测（2026-09-04）**：`workbuddy://` 在 HKCR/HKCU 均有 handler（`"…\WorkBuddy.exe" "%1"`）——Windows 具备深度链接可用条件（本次按 §7 仍走近祖聚焦，深度链接列为 Windows 升级项）；`codex://` 在未安装 ChatGPT 桌面版的机器**仅有协议标记、无 handler**——派发前必须校验 handler 存在性，「spawn 成功」不代表路由成功 | Info.plist 核查 + Windows 注册表实测 |
| 「用户键入但未发送」在 Codex/WorkBuddy 均无法可靠检测（实验证伪：Codex global-state 无归因变化；WorkBuddy localStorage 为 675KB gzip React 状态块） | 双端对照实验 |
| Codex++（进程名 `CodexPlusPlus`）不纳入监控（需求方明确决定） | 需求对齐 |

## 2. W1：通知面统一与宠物气泡修复

### 问题

1. 宠物气泡点击跳转失败时（macOS Codex APP 等场景）气泡永远点不掉（`FoxbellPet.tsx` 的 `jump()` 失败静默保留卡片）。
2. `petSuppressPopup()` 现条件为 `visible && alwaysOnTop`，语义不清晰；且宠物开启时系统通知可能仍出现，通知面不统一。

### 设计

- **气泡点击行为链**：点击宠物头顶卡片 → 调用 `focus_session`（尽力跳转：Windows 走 win32，macOS 走 W2 新增的 APP 激活）→ **无论成败立即清除气泡**。跳转成功且为 APP 类会话时联动标记已读（W4）。
- **通知面策略**：
  - `petSuppressPopup()` 与 `petSoundTakeover()` 的判定简化为仅 `loadVisible()`（去掉 `alwaysOnTop` 条件）。
  - 宠物可见：右下角浮窗与系统通知（含浮窗失败降级路径）**全部静默**，气泡是唯一通知面。
  - 宠物隐藏：维持现状（右下角浮窗为主，浮窗创建失败时降级系统通知）；点击跳转链路同上修复。

### 涉及文件

- `src/components/pet/petConfig.ts`（两函数简化）
- `src/hooks/useNotification.ts`（降级路径纳入压制条件）
- `src/components/pet/FoxbellPet.tsx`（jump 失败也清除气泡）

## 3. W2：APP 跳转（session 级直达优先 + APP 级保底）

### 设计

- `src-tauri/src/window/` 新增 macOS APP 激活模块，跳转优先级链：
  1. **第一顺位（session 级直达）**：该工具注册了 URL scheme（ChatGPT.app → `codex://`；WorkBuddy.app → `workbuddy://`，均已在 Info.plist 核实）。实施计划将「探测会话路由参数格式」列为前置任务；探明后 `open "<scheme>:...<sessionId>"` 直达具体会话。
  2. **保底（APP 级激活）**：从目标进程可执行路径提取 `.app` bundle 根（路径中定位 `.app/` 段），`osascript -e 'activate application "<bundle 路径>"'`。
  - 路由格式探测成功前，以 APP 级激活交付，不阻塞主流程。
- **pid 失效兜底**：未读卡对应的会话进程可能已退出（WorkBuddy prewarm 干完活回池、Codex 会话结束）。`focus_session` 在 pid 不存在时按工具降级：定位该工具任一 App 形态进程 → 激活其宿主（macOS bundle / Windows 近祖窗口），保证未读卡点击始终可跳。
- `commands/session.rs` 的 `focus_session` macOS 分支：`ProcessForm::App`（或 TTY 获取失败且路径含 `.app`）→ 走 APP 激活。
- `session/model.rs` 的 `jump_supported_for`：macOS 对 App 形态返回 `true`。
- 前端 `SessionCard` 无需改动逻辑（`jumpSupported` 变 true 后 toast 拦截自然消失）。
- **Windows 小幅增强**：`win32.rs` 的 App 类近祖聚焦（`codebuddy.exe → WorkBuddy.exe`）保持不变；**新增 pid 失效兜底的 Windows 分支**——pid 不存在时枚举该工具 App 形态进程的可见窗口聚焦（现有 `resolve_and_focus` 以 pid 为起点沿祖先链，pid 死则链断，无法直接复用）。

### 限制（已与需求方确认接受）

- 外部进程默认只能把 APP 带到前台；**session 级直达是第一顺位目标**，依赖深度链接路由格式可探明——探测成功前以 APP 级激活保底交付。
- Codex++ 不纳入。

## 4. W3：WorkBuddy 适配器

### 定位

新增 `src-tauri/src/adapter/workbuddy.rs`，实现 `AgentAdapter` trait；`TOOL_IDS` 增加 `"workbuddy"`，`AgentType::WorkBuddy`，显示名「WorkBuddy」。与 Codex 的差异全部来源于两者私有文件机制不同（框架、状态推导、三层链接、MCP 写入、通知、看板渲染全部复用；新代码 = 路径定义 + 心跳过滤 + 一个新的 JSONL 解析器）。

### 进程与会话规则

- **会话进程发现以心跳目录为权威来源**（2026-09-04 Windows 实测修订）：枚举 `~/.workbuddy/sessions/*.json` 并逐个解析，符合下述过滤规则的 pid 即会话进程；再以 pid 回查进程表补充 CPU/exe 构装 `AgentProcess`（进程表查无此 pid → 跳过，消失场景由 W4 补偿处理）。**不使用进程名匹配**——Windows 上会话宿主与 APP 主进程同名（均为 `WorkBuddy.exe`，Electron 以自身作 Node 运行 `cli/bin/codebuddy` 脚本，无 `codebuddy` 进程），且「父进程同名」会被通用子代理过滤误杀；macOS 的 `codebuddy` 进程名匹配一并移除，双平台统一心跳驱动（macOS 心跳文件同样存在）。
- **过滤规则**（排除干扰进程，依据心跳文件 `~/.workbuddy/sessions/<PID>.json`；两类干扰进程实测均写心跳）：
  - 心跳文件存在，且 `sessionId` 为**严格 UUID**（8-4-4-4-12 分段且每段均为 hex 字符集——prewarm 池的 `prewarm-wb-pool-<13位ms>-<6位hex>` 恰为 36 字符 4 连字符，仅凭「长度 36 + 连字符 4」判定会被骗过），且心跳 `kind` 非 `prewarm`（防御性双保险，字段缺失视为通过），且 `lastHeartbeat` 距今 < 90s（= 30s 轮询周期 × 3，实现校准值；原文 30s 为初值）→ 活跃会话进程。
  - 其余（无心跳 / 心跳过期 / `interactive-*` 服务 / prewarm 池）→ 不产生会话卡。
  - 注：独立安装的腾讯 CodeBuddy CLI 在 `~/.workbuddy` 下无心跳文件，天然排除，不会误认。
- `find_sessions()`：心跳 → `sessionId` + `cwd` → 定位 `~/.workbuddy/projects/<mangle(cwd)>/<sessionId>.jsonl`。mangle 规则（2026-09-04 双平台实测）：
  - POSIX：去除首 `/`，`/` 替换为 `-`（macOS 原有形态，不变）；
  - Windows：**盘符小写 + 去冒号**，再 `/`、`\` 统一替换 `-`——`C:\Users\bunny\WorkBuddy\2026-08-06-15-57-15` → `c-Users-bunny-WorkBuddy-2026-08-06-15-57-15`，`E:\LLMproject\0807` → `e-LLMproject-0807`；
  - **容错兜底（硬性）**：mangle 路径未命中时，扫描 `~/.workbuddy/projects/*/` 查找 `<sessionId>.jsonl`（与 W4 心跳消失补偿共用同一实现）；仍无 → 跳过该会话。UNC 等未实测形态由兜底覆盖。
- **状态推导**（解析 JSONL 尾部，复用 `monitor/status.rs` 的 `determine_status` 语义）：
  - 最后有效条目为 `user` message → Thinking（黄）；
  - `function_call` / `function_call_result` → Processing（红）；
  - `assistant` message 纯文本 → Idle（绿）；
  - 叠加 mtime 阈值（App 形态 300s，与 Codex APP 一致）。
- **会话标题**：只读打开 `workbuddy.db`（URI `mode=ro`，连接设 `busy_timeout` 防应用写入高峰期查询失败，与 opencode 解析器对齐）读 `sessions.title`（用户自定义名 `custom_title` 非空时优先）；读失败降级为会话首条 user 消息截断。
- `base_dir()` = `~/.workbuddy`；`mcp_format()` = Json，`mcp_config_path()` = `~/.workbuddy/mcp.json`；`skill_dirs()` = `[~/.workbuddy/skills]`；`plugin_dirs()` / `plugin_config_paths()` = 空（插件不纳入）；`hook_supported()` = false；`subagent_dir()` = None。

### 资源接入

- Skills：`~/.workbuddy/skills` 接入三层符号链接（完全复用 linker）。
- MCP：`mcp.json` 走现有 JSON 写入路径（与 Claude 同构）。
- 插件：**不纳入**（市场化版本化管理，手动写入有被 APP 覆盖/损坏风险）。
- 项目级 skills（`<项目>/.workbuddy/skills/`）：**不纳入**（与现有 5 工具保持一致）。

### 前端接入

`ResourceByKindView` / `ResourceByToolView` / `settings.tsx` 三处工具列表、`agentBadge`、i18n（中/英）、工具图标、`detect_all_tools`（`~/.workbuddy` 存在即已安装）、按工具声音配置，全部补齐 workbuddy。注意：W5 会把三处硬编码列表改为后端下发，接入方式以 W5 为准（W3 阶段先按现状加，W5 统一重构）。

### 防御性要求

心跳 / JSONL / DB 均为**未文档化私有格式**：任一文件缺失或解析失败 → 跳过该会话或降级显示，绝不 panic、不影响其他工具的监控。

## 5. W4：已读机制（仅 APP 类工具）

### 范围（需求方明确划分）

- **CLI 类**（Claude / Codex CLI / OpenCode / OpenClaw / Kimi）：维持现有纯进程体系，**完全不参与**已读机制。
- **APP 类**（Codex APP / WorkBuddy）：单走「活跃卡 + 持久未读卡」分支。

### 数据模型

新表 `unread_sessions`：

| 列 | 说明 |
|----|------|
| `tool_id` + `session_id` | 联合唯一 |
| `project_name` / `title` / `last_message` | 卡片展示快照 |
| `turned_green_at` | 转绿时间 |
| `expires_at` | `turned_green_at + 24h` 兜底过期 |

记录的删除为**单轨物理删除**：已读 / 手动关闭 / 变黄（会话重新运行，活跃卡由进程监控接手渲染，删备忘防同会话双卡）/ 过期 / 宿主进程退出 / 工具取消勾选，全部直接删行，无软删标记。

### 生命周期

- 会话状态进入绿色 → upsert 未读记录（跨 MAM 重启保留，前提是宿主 APP 进程仍存活，见「宿主进程生命周期」）。
- **WorkBuddy 转绿竞态补偿**：任务完成到 prewarm 回池（心跳文件删除）可能只隔几秒；若 MAM 两次扫描之间全部完成，「转绿」从未被观测 → 未读漏插、提醒静默丢失。处理：**心跳消失事件本身触发补偿**——按该进程上一轮记录的 sessionId 读其 JSONL 尾部，终态为完成 → 补插未读记录（卡片转绿继续显示）；终态为运行中被杀 → 不插。（Codex 无此问题：rollout 文件完成后持续存在。）
- 状态重新变黄/红 → 删除未读记录（活跃卡本身可见，属状态迁移而非「重置机制」；再次转绿重新插入）。
- **已读信号**（仅以下三类；「未发送键入」已实验证伪，不采用；「APP 内切换会话检测」因 Codex 状态库被锁、WorkBuddy 靠 675KB 压缩状态块，均太脆弱，不做）：
  1. 通过 MAM 跳转：点击该卡（看板/宠物/浮窗）跳转成功 → **仅标记被点击的那张卡已读**，同工具其他未读卡保留（pid 失效时走 W2 兜底激活宿主，同样算跳转成功。自洽保证：未读卡存在 ⇒ 宿主进程必存活 ⇒ 兜底必有跳转目标）；
  2. 手动关闭：卡片 X 按钮 → 已读；
  3. 兜底过期：24h 自动清理。
  跨窗口同步（2026-09-05 补）：跳转/关闭已读发生后，后端广播 `session-read` 事件至宠物等辅助窗口，宠物对本地状态机做等价已读置位——**卡片行为与看板一致**（后端为单一事实源，宠物不保留本地消失 TTL）。

### APP 类会话卡片通用规则（每会话一卡）

**这是所有 APP 类工具的通用机制，不是某个工具的特例**（Codex APP、WorkBuddy 及未来新增的 APP 类工具一律遵循）：

1. **卡片数量以筛选结果为准**，不设「每 APP 只保留一张卡」的限制；每个会话（session）独立成卡，互不顶掉。
2. **卡片以「活着」为显示前提**（存续机制）：活跃卡需宿主进程存活；转绿进入未读池持久保留；被已读信号清除；宿主进程退出则全部清理（见下节）。

落地到各工具：

- **Codex APP**（解析器改造）：现状是 APP 形态每进程只取最新一张卡（cwd 无效时回退最近文件）。改为：近期（24h 内 mtime 有更新）的 rollout 中，**未被 CLI 进程认领**（cwd 匹配未占用）的 → **按文件名中的会话 UUID 聚合**（Codex 每轮对话写新 rollout，同一会话对应多个文件，取该会话最新文件的状态）→ 每会话一张卡，归属 Codex（APP）；活跃（黄/红）由 JSONL 尾部推导；CLI 认领规则不变（Phase 1 cwd 精确匹配优先），确保 CLI/APP 会话不重复出卡。
- **WorkBuddy**：心跳文件天然按会话分卡（W3 过滤规则的输出即每会话一卡），无需额外改造。

### 宿主进程生命周期（卡片清理规则）

未读卡的持久保留以**宿主 APP 进程存活**为前提。两种场景统一为一条规则——**「该工具无任何 App 形态进程存活 → 该工具全部卡片（含未读，无论已读与否、何种状态）立即清理」**：

1. **MAM 运行中进程被关**：用户直接关闭 Codex/WorkBuddy APP → MAM 下一轮监控扫描发现后清理该工具全部卡片。
2. **MAM 重启残留检查**：MAM 关闭期间 APP 也被关闭 → MAM 重启后首轮扫描按同规则清理 DB 中残留的未读卡。

判定口径：**宿主进程 = 该工具 `.app` 包内的非会话运行时进程**——WorkBuddy 看 Electron 主进程/Helper（路径在 `WorkBuddy.app` 内且可执行名不含 `codebuddy`）；Codex 看 `ChatGPT.app` 主进程。会话进程（codebuddy prewarm/serve、Codex 框架进程）**不参与宿主判定**，防止 APP 崩溃/强杀后残留的孤儿 codebuddy 进程导致误判「宿主还活着」、卡片不清理。判定使用未过滤心跳的原始进程扫描结果。

### 前端

- 看板：活跃卡（现状渲染）+ 未读卡（复用 SessionCard 样式，带未读徽标 + X 按钮），未读卡排后。
- 宠物头顶卡片与看板同一数据源，已读联动消失。
- 新增 IPC：`list_unread_sessions` / `mark_session_read`；`get_all_sessions` 返回值不变，前端 query 合并。
  - 实现期调整（计划架构）：未读卡由后端 `sync_unread_sessions` 直接合并进 `get_all_sessions` 返回值（`Session.unread` 标记），看板/宠物/通知管线零改造；不设独立 `list_unread_sessions` IPC，仅保留 `mark_session_read`。

## 6. W5：工具勾选管理

### 数据与命令

- 启用 `agent_tools` 表预留的 `enabled` 列；首次迁移为全部工具写入 `enabled=1`（老用户升级零感知，默认全启用）。
- 新 IPC：`get_tool_settings`（返回 `tool_id/name/enabled/installed/managed`——`managed` 表示该工具是否存在 MAM 管理内容（skill/插件链接或 MCP 条目），保存确认弹窗据此决定是否对该工具提示「还原/回溯」）、`update_tool_settings`（批量保存，保存时执行清理/重建）。
- DAO 补齐 `agent_tools` 读写。

### 取消勾选的清理语义（保存时执行，需求方确认）

对该工具：
1. skill 与文件型插件的符号链接 → `remove_link` 后**从 SSOT 复制真实内容还原**（新增 `restore_from_ssot`；SSOT 缺失的项跳过并在结果中报告）。
2. MAM 管理的 MCP 条目（依 DB assignment 记录）→ 从工具配置文件移除（JSON/TOML/JSONC 各走现有 remove 路径）。
3. **SSOT 仓库与 DB 分配关系全部保留**；重新勾选时按原分配重建链接与 MCP 写回。
4. 未读卡（`unread_sessions` 该工具记录）一并清除；活跃会话卡立即消失、监控停止。

### 生效范围（彻底隐藏）

未勾选工具：会话扫描跳过（`get_all_sessions` 过滤 adapter）、通知静音、「按资源分布」表格列移除、skill/MCP/插件管理界面不出现、enable/disable 类命令对其返回明确错误。

### 前端

- 设置页新增「工具管理」分区：**行式开关列表**（每行 = 图标 + 名称 + 安装状态 badge「已安装/未检测到」+ 左右滑动开关按钮）。
- **交互与保存机制**：
  1. 点击开关即切换 UI 状态（本地暂存），**不立即生效**——开关涉及文件改动（还原/重建），必须批量提交。
  2. 点击「保存设置」按钮统一应用全部变更（文件复制还原、链接重建、MCP 写入/移除、监控启停）。
  3. **保存确认弹窗**：列出本次开关变更清单并分类提示——(a) **新纳入监控**（新开启的工具，影响较小）；(b) **还原/回溯**（关闭且该工具由 MAM 管理（`managed=true`）→ skill/插件链接将被还原为真实文件、MCP 条目从工具配置移除）。用户在弹窗中确认后才执行应用。
  4. **未保存离开拦截**：存在未保存的开关变更时离开设置页（切分区/关窗口）→ 弹窗三选：**保存**（走确认弹窗流程）/ **放弃更改** / **继续编辑**。
- 三处硬编码 TOOLS 列表（byKind/byTool 视图、settings 声音区）改为后端 `enabled_tools` 下发（react-query）。

## 7. 跨平台要求（硬性）

- DB 层（`agent_tools` / `unread_sessions`）天然跨平台。
- APP 激活：macOS 用深度链接（第一顺位）/ AppleScript（保底）；Windows 近祖聚焦沿用 + pid 失效兜底。Windows 近祖链实测形态（2026-09-04）为 `WorkBuddy.exe（CLI 宿主，无窗口）→ WorkBuddy.exe（主进程，持窗口）`——首祖即主进程，聚焦可达，无需按原设想的 `codebuddy.exe → WorkBuddy.exe` 链验证。`workbuddy://` 在 Windows 注册表已注册 handler，Windows 深度链接列为后续升级项（若启用，派发前必须校验 handler，见 §9）。
- WorkBuddy 进程匹配（2026-09-04 实测修订）：Windows 安装形态为独立目录 NSIS（如 `D:\Program Files\WorkBuddy`，未观测到 MSIX）；会话进程发现不依赖进程名，双平台统一心跳驱动（见 §4）；宿主存活判定按 exe basename 前缀 `workbuddy` 且路径不含 `codebuddy`（实测匹配 `D:\Program Files\WorkBuddy\WorkBuddy.exe`）。
- 心跳/JSONL/DB 路径两端一致（`~/.workbuddy/`），无平台分支；Windows 下 `~` 展开沿用现有 `home_dir` 机制。
- 所有条件编译改动在两平台 `cargo check` + `pnpm check` 通过。

## 8. 测试策略

- **Rust 单测**（fixture 驱动）：workbuddy 心跳过滤（UUID 判定/新鲜度/interactive 排除/独立 CodeBuddy CLI 排除）、JSONL 尾部状态推导、cwd mangle、`restore_from_ssot`、`agent_tools` DAO、unread 生命周期（转绿 upsert / 心跳消失竞态补偿 / 变黄删除 / 过期清理 / 跳转已读仅单卡（含 pid 失效兜底）/ 宿主进程退出全部清理（含孤儿 codebuddy 不误判））、Codex APP 每会话一卡的认领与按 sessionId 聚合规则、`focus_session` 的 pid 失效按工具降级。
- **全量门禁**：`cd src-tauri && cargo test && cargo clippy`；`pnpm check`。
- **手动验证清单**（双平台，按工作项）：气泡点击即清除；宠物开 → 浮窗/系统通知静默；macOS 点 Codex APP/WorkBuddy 卡跳转（深度链接直达会话优先，APP 前台保底）；WorkBuddy 会话卡状态与实际任务一致；转绿卡片跨 MAM 重启保留、点击跳转后消失、同工具其他未读卡保留；MAM 运行中关闭 APP → 该工具卡片全部清理；MAM 重启后残留卡片按宿主进程存在性清理；工具管理页开关切换不立即生效、保存弹确认弹窗（变更清单 + 还原提示）、未保存离开弹窗三选、确认后文件还原为真实文件；重新勾选按原分配重建。

## 9. 风险与已知限制

| 风险/限制 | 应对 |
|-----------|------|
| W5 批量保存「重建部分失败 → **自动回滚本次产物 + DB 保持未启用**（重新勾选即幂等重试）」为 2026-09-05 新增语义（§6 原文未规定，PR #41 实现） | 回滚复用停用清理（`disable_tool_cleanup`），产物状态与正常停用完全一致（Layer2 junction 残留行为相同，重新勾选幂等重建）；配套 `enable_allowed` 失败增量门控 + 批量保存重入互斥（`W5_APPLY_IN_PROGRESS`）与 JoinError 结构化错误码（`W5_APPLY_TASK_FAILED`） |
| WorkBuddy 私有格式无文档，版本升级可能破坏 | 防御性解析 + 降级显示；fixture 测试锁格式假设 |
| 深度链接路由格式未知 | 第一顺位优先探测；探不明则以 APP 级激活保底交付。实测结论（2026-09-04）：已探明并接线 —— WorkBuddy `workbuddy://chat/<sessionId>`（app.asar 源码证据）、Codex `codex://threads/<threadId>`（asar 模板证据）；threadId 与 rollout UUID 同源性待 GUI 实测确认，直达失败则回退 None 走 APP 级保底。**Windows 补充实测（同日）**：`workbuddy://` 已注册 handler；`codex://` 在未装 ChatGPT 桌面版的机器仅有协议标记、无 handler——「spawn 成功」不代表路由成功，派发前应校验 handler 存在性（Windows 查注册表 `\<scheme>\shell\open\command`，macOS 用 `LSCopyDefaultHandlerForURLScheme`），校验失败走保底且**不得标已读**。**Windows 判据修订（2026-09-05）**：ChatGPT 为 MSIX 安装时协议关联走 AppModel，注册表仅有 `URL Protocol` 标记、无 `shell\open\command`——仅查 command 产生**假阴性**（实测派发 `codex://threads/<uuid>` 三次均成功前台化且 OCR 证实会话级导航），判据放宽为「command 非空 **或 `URL Protocol` 值存在**」；假阳性风险由派发后 B 前台验证兜底（未前台化 → 落回保底聚焦、不标已读），分层防御 |
| 外部进程无法 APP 内部导航到具体会话 | 已确认接受（APP 前台级别） |
| Codex++ 不被监控 | 明确不纳入本次范围 |
| ~~Windows 上 WorkBuddy 安装形态未实测~~（已实测） | 2026-09-04 实测结论已回写本文档：NSIS 独立目录 + `WorkBuddy.exe`，会话宿主与主进程同名、无 `codebuddy` 进程 → §4 发现机制改心跳驱动；mangle 与 prewarm 心跳两处原调研结论有误，已更正。由此产生的代码偏差由整改方案处理：`docs/engineering/plans/2026-09-04-windows-p0-remediation.md` |
| 取消勾选时 SSOT 缺失导致还原不完整 | 跳过并在保存结果中逐项报告，不中断 |

## 10. Commit 划分（单分支分阶段）

1. `fix(pet): 气泡点击即清除 + 宠物开启时通知面统一`
2. `feat(jump): APP 跳转（session 级深度链接优先 + APP 级保底 + pid 失效兜底）`
3. `feat(adapter): WorkBuddy 适配器（会话监控 + Skills/MCP 资源接入）`
4. `feat(sessions): APP 类已读机制（持久未读卡 + 每会话一卡）`
5. `feat(settings): 工具勾选管理（批量保存 + 确认弹窗 + 全量还原 + 后端下发工具列表）`

## 11. 修订记录

- **2026-09-04（Windows 实机测试修订）**：测试环境 WorkBuddy v2.115.0 / Windows 11 / `D:\Program Files\WorkBuddy`（数据目录 `~/.workbuddy` 含 14 份心跳、4 个项目目录、多会话 workbuddy.db）。按实测更正四项调研结论，并联动修订 §4/§7/§9：
  1. Windows 会话宿主进程 = `WorkBuddy.exe` 自身（Electron 以 Node 模式重执行自身运行 `cli/bin/codebuddy` 脚本，无 `codebuddy` 进程）→ §4 进程发现改为**心跳目录驱动**，弃用进程名匹配（macOS 同步）；
  2. prewarm 池**心跳持续新鲜**且 sessionId `prewarm-wb-pool-*` 恰为 36 字符 4 连字符 → §4 过滤规则改为**严格 UUID（hex 字符集）+ `kind` 防御**；心跳新鲜度阈值由 30s（初值）校准为 **90s**；
  3. Windows mangle 实测为**盘符小写 + 去冒号** → §4 更正规则并新增**目录扫描容错兜底**（硬性要求）；
  4. `workbuddy://` 在 Windows 注册表已注册 handler（列为 Windows 深度链接升级项）；`codex://` 可能仅有协议标记无 handler → §9 增补「派发前校验 handler、失败走保底且不标已读」。
  - 本轮修订揭示的既有代码偏差（进程名匹配、宽松 UUID 判定、错误 mangle 及其锁定测试、DB 连接无 busy_timeout）由整改方案处理：`docs/engineering/plans/2026-09-04-windows-p0-remediation.md`。
