# 整改方案：Windows 实机问题修复（P0）与 P1/P2 处置

- 日期：2026-09-04
- 分支：`feat/workbuddy-app-jump-tool-toggle`（PR #30）
- 输入：spec `2026-09-03-workbuddy-app-jump-tool-toggle-design.md`（2026-09-04 修订版，见其 §11）；Windows 实机测试证据（附录 A）
- 状态：设计稿，待评审。本文档不含代码实现。

## 0. 问题总览

| 级别 | 问题 | 一句话根因 |
|------|------|-----------|
| P0-1 | Windows 无会话卡：进程发现空集 | 会话宿主进程 = `WorkBuddy.exe` 自身，无 `codebuddy` 进程，`["codebuddy"]` 名单匹配恒空 |
| P0-2 | JSONL 永不命中：mangle 编码错 | Windows 实际编码为盘符小写+去冒号，实现保留冒号与大小写 |
| P0-3 | prewarm 幽灵会话风险：UUID 判定过松 | `prewarm-wb-pool-*` 恰为 36 字符 4 连字符，骗过长度+连字符计数判定（当前被 P0-2 意外掩蔽） |
| P1-1 | Windows 深度链接死代码 | spec §7 将深度链接划为 macOS 专属，代码忠实照做 |
| P1-2 | 深度链接误报成功→误标已读 | spawn 成功 ≠ handler 存在/路由成功，异步无回执交互被当同步有回执建模 |
| P1-3 | W5 还原 Windows 未测 + 文件目标复制语义 | 测试 `#[cfg(all(test, unix))]` 锚定 Unix；linker 文件目标降级 copy 无告知 |
| P1-4 | workbuddy.db 无 busy_timeout | 新代码未复刻同仓既有模式（opencode 已有），测试用静止 DB 无法复现锁竞争 |

### 0.1 决策记录（2026-09-04，需求方）

| 项 | 决策 |
|----|------|
| P0-1/2/3 | 按本方案实施（顺序 P0-3 → P0-1 → P0-2） |
| P1-1 | **待 P1-2 探测结果决策**：深度链接可达成（判据 C1~C5 见 P1-2 节）→ 本 PR 内直接启用；否则 → 本 PR 只做死代码清理 + 独立 follow-up PR。**探测结果须先反馈需求方** |
| P1-2 | **A + B 组合**（派发前 handler 校验 + 派发后前台验证），实施前先本机探测验证两项机制可行性 |
| P1-3 | 测试面：**平台无关化（A）+ Windows 专项补充（B）**；语义面（hardlink/明示 copy）本轮不动，待后续单独决策 |
| P1-4 | **B 消根**（共享 `open_readonly_with_timeout`） |
| P2 | 全部按 §3 清单执行，无异议 |

---

## 1. P0 整改设计

### P0-1 会话进程发现改为「心跳目录驱动」

**问题**：`process.rs:205-207` `find_workbuddy_processes` 用 `["codebuddy"]` 匹配进程名；Windows 上 CLI 宿主由 `WorkBuddy.exe` 以 Node 模式重执行自身承担，进程表中不存在名为 codebuddy 的进程 → 发现恒空 → 心跳过滤、卡片、未读、补偿全链路不启动。

**设计**：

1. 在 `workbuddy_parser.rs` 新增发现入口（替代 `find_workbuddy_processes`）：
   - `read_dir(~/.workbuddy/sessions/)`，对每个 `<PID>.json` 防御性解析心跳；
   - 过滤：严格 UUID（P0-3）+ 新鲜（< 90s）+ `kind != "prewarm"`；
   - 以 pid 回查 sysinfo 进程表：存在 → 构装 `AgentProcess { pid, cpu_usage, cwd: hb.cwd, exe, form: App }`；不存在 → 跳过（消失场景由 W4 `compensate_vanished_heartbeats` 经 `LAST_SEEN_SESSIONS` 处理，语义不变）；
   - MAM 自身进程排除不再需要（MAM 不写心跳）；子代理过滤天然绕过（无进程名匹配，心跳 sessionId 唯一标识会话）。
2. 删除 `find_processes_by_names(["codebuddy"], ...)` 的 WorkBuddy 调用点与 `process_names()` 中的该值（保留 trait 方法本身供其他工具用）；macOS 一并切到心跳驱动（心跳文件双平台同构，已验证）。
3. `get_workbuddy_sessions` 主体逻辑不动：仍以收到的 `AgentProcess` 列表为输入，逐 pid 读心跳——注意发现入口已读过一次心跳，第二次读用于会话构装；可让发现入口返回 `(AgentProcess, Heartbeat)` 缓存避免双读（实现期取舍，I/O 量小可先不优化）。

**涉及文件**：`monitor/workbuddy_parser.rs`、`monitor/process.rs`（删旧入口）、`adapter/mod.rs`（发现调用点）、`adapter/workbuddy.rs`（`process_names()` 置空或删除）。

**测试**：fixture 驱动（真实样本，附录 A）——
- `interactive-12032`（serve）/ `prewarm-wb-pool-1788496419201-bb1050`（池）→ 不产进程；
- `ecbf3d35-76e9-42df-b71d-89409ec156ea` 新鲜 → 产出；同 pid 心跳过期 → 不产出；
- 心跳新鲜但 pid 不在进程表 → 不产出（且不动 `LAST_SEEN_SESSIONS`）；
- 心跳目录不存在/不可读 → 空集不 panic。

**验收（Windows 实机）**：WorkBuddy 里发起真实任务 → 30s 内看板出现 WorkBuddy 卡片；卡片状态随任务黄/红/绿；关闭 WorkBuddy → 卡片清理；`~/.workbuddy/sessions` 里的 serve/prewarm 心跳不产生卡片。

### P0-2 mangle 双平台规则 + 目录扫描兜底

**问题**：`workbuddy_parser.rs:51-54` 仅做「去首 `/` + `/`、`\`→`-`」，对 `C:\Users\...` 产出 `C:-Users-...`；实际目录为 `c-Users-...`（盘符小写、无冒号）→ `jsonl.exists()` 恒 false → 卡片被 `continue` 跳过，且 `LAST_SEEN_SESSIONS` 在 push 之后才写入 → W4 补偿链连带死亡。测试 `:388-392` 用合成数据锁定了错误形态。

**设计**：

1. `mangle_project_path` 重写：
   - Windows 盘符形态：`<字母>:<分隔符>rest` → 小写字母 + `-` + rest 分隔符替换（即 `C:\Users` → `c-Users`）；
   - POSIX：维持现状（去首 `/`，`/`→`-`）；
   - UNC（`\\...`）与其他未实测形态：不猜规则，交给兜底。
2. 新增共享查找函数 `find_session_jsonl(home, cwd, session_id) -> Option<PathBuf>`：
   - 先试 `mangle(cwd)/<sessionId>.jsonl`；
   - 未命中 → 扫描 `~/.workbuddy/projects/*/` 查找 `<sessionId>.jsonl`（把 `compensate_vanished_heartbeats_in:283-290` 的内联扫描抽为该函数，两处共用）；
   - 仍无 → `None`（调用方跳过该会话）。
3. `get_workbuddy_sessions` 改用 `find_session_jsonl`。

**涉及文件**：`monitor/workbuddy_parser.rs`（mangle、find_session_jsonl、主路径、补偿路径复用）。

**测试**：以真实目录名为 fixture（附录 A）——
- `C:\Users\bunny\WorkBuddy\2026-08-06-15-57-15` → `c-Users-bunny-WorkBuddy-2026-08-06-15-57-15`；
- `E:\LLMproject\0807` → `e-LLMproject-0807`；
- 修正原 `C:-Users-...` 错误断言；
- POSIX 回归：`/Users/jarvis/proj` → `Users-jarvis-proj` 不变；
- 兜底：mangle 未命中但 `projects/其他目录/<sessionId>.jsonl` 存在 → 命中；全无 → None。

**验收（Windows 实机）**：对附录 A 中既有会话（如 `672509f4-3cff-495e-923f-4a689bded9bf`，目录 `e-LLMproject-0807`）手动构造新鲜心跳 → 卡片出现且标题/最后消息来自真实 JSONL 与 workbuddy.db。

### P0-3 严格 UUID 判定 + kind 防御

**问题**：`workbuddy_parser.rs:42-44` 的 `len==36 && 连字符==4` 判定被 `prewarm-wb-pool-1788496419201-bb1050`（36/4）骗过。当前因 P0-2 导致 JSONL 落空而意外无害；修复 P0-2 后立即暴露为幽灵会话。

**设计**：

1. `heartbeat_session_id_is_uuid` 改为分段 hex 校验：8-4-4-4-12 五段、每段 ASCII hex（大小写均可）。纯字节循环实现，不引入 regex 依赖。
2. `Heartbeat` 结构体增加 `kind: Option<String>`（serde 缺省 None）；过滤条件追加 `kind.as_deref() != Some("prewarm")`（字段缺失视为通过，防御私有格式演进）。
3. 两个条件是独立防线：严格 UUID 为主，kind 为双保险。

**涉及文件**：`monitor/workbuddy_parser.rs`。

**测试**：接受 `ecbf3d35-76e9-42df-b71d-89409ec156ea` 与大写 hex UUID；拒绝 `prewarm-wb-pool-1788496419201-bb1050`、`interactive-12032`、`g8hh...` 类非 hex 8-4-4-4-12 形态；kind=Some("prewarm") 拒绝、kind=None 通过。

### 实施顺序与门禁

顺序：**P0-3 → P0-1 → P0-2**（先收紧判别，再换发现入口，最后修路径；三者无相互阻塞，也可并行）。P1-4 随同批提交（同模块顺手）。P1-3 测试面与 P2 各项可随后分批；P1-2 的 A+B 机制与 P1-1 走向按 §0.1 决策记录执行（探测结论回填 P1-2 节后启动）。

门禁：`cd src-tauri && cargo test && cargo clippy --all-targets -- -D warnings`（Windows 实机 + macOS）；`pnpm check`。Windows 实机按上述三项验收清单手测（这台机器可直接执行）。

---

## 2. P1 根因分析（定性）与修改选项

### P1-1 Windows 深度链接是死代码（`workbuddy://` 实际已注册可用）

**根因（定性）**：设计先行于事实。spec §7 在 Windows 注册表事实未知前，保守地把深度链接划为 macOS 专属路径；实现忠实执行了 spec，`deep_link.rs:13-20` 的 Windows 分支因此成为不可达代码。这是「设计分层保守 + 代码不越 spec」共同作用的产物，不是实现走样——修正入口在 spec（已完成修订），不在代码本身。

**选项**：
- **A. 本 PR 内只做清理**：删除或 `#[cfg]` 隔离 Windows 不可达分支 + 注释指向升级项；Windows 深度链接维持近祖聚焦现状。改动最小、零风险；代价是放弃一次已证明可行的直达能力。
- **B. 本 PR 内直接启用**：`focus_session` Windows 分支在 `resolve_and_focus` 前对 App 形态卡先试 `open_url`（handler 校验通过才派发），失败无缝落回现有链路。收益是会话级直达；风险是依赖 P1-2 的校验/验证机制先行，否则会引入误标已读的新路径。
- **C. 独立 follow-up PR**：本 PR 按 A 清理，深度链接启用连同 P1-2 的「前台验证」一起在后续小 PR 做（spec §9 已写入升级项语义）。

**推荐**：A（本 PR）+ C（后续）。B 的前提（P1-2 完成）不具备，不应塞进本已很大的 PR。

**【决策 2026-09-04】**：走向取决于 P1-2 探测结果——若深度链接本机实测可达成（判据见 P1-2 探测计划 C1~C5），选 B（本 PR 内直接启用，随 P1-2 的 A+B 机制一起交付）；否则选 A（清理）+ C（follow-up）。探测结果反馈需求方后再定。

### P1-2 深度链接「spawn 即成功」→ 误标已读（codex:// 可能无 handler）

**根因（定性）**：URL 协议派发是**异步、无回执**的 OS 交互——`cmd /C start` / macOS `open` 的成功只证明「命令被系统接收」，不证明「存在 handler 且完成了路由」；而 `mark_read_on_jump` 挂在派发返回上，把不可观测信号当成了确认信号。叠加本机实测的「协议标记存在但 handler 缺失」注册形态，形成「派发成功→卡片已读→实际什么都没发生」的静默错误链。属于把弱信号（spawn 退出码）误用为强信号（用户已看到会话）的建模错误。

**选项**：
- **A. 派发前同步校验 handler 存在性**：Windows 读注册表 `HKCR\\<scheme>\shell\open\command`（含 HKCU 回退）；macOS 用 `LSCopyDefaultHandlerForURLScheme`。无 handler → 跳过深度链接直接走保底聚焦。改动小、同步、确定性；局限是「handler 存在但路由错误」（如 codex threadId 不同源）仍探测不到。
- **B. 派发后前台验证**：open 后 1.5~2s 轮询前台窗口（Windows `GetForegroundWindow` 归属进程；macOS `NSWorkspace.frontmostApplication`），目标 APP 未到前台 → 自动执行保底聚焦且**不标已读**。覆盖路由失败类；代价是时序代码与用户可感知的短暂等待。
- **C. 解耦标已读**：深度链接路径一律不自动标已读，改由前台验证（B）成功或用户第二次点击补标。UX 最保守，实现最简单，但正常直达也要用户多点一次。

**推荐**：**A + B 组合**（A 是廉价前置门槛，B 是确认机制），C 仅作 A/B 均不可行时的兜底。spec §9 已按此修订。

**【决策 2026-09-04】**：采用 A + B 组合；实施前先在本机执行探测，验证：
- **C1** handler 可同步检测（注册表 `HKCR/HKCU\<scheme>\shell\open\command`）；
- **C2** 派发后 WorkBuddy 前台化（App 级激活达成）；
- **C3** 派发后导航到**具体会话**（session 级直达达成，截图/UIA 确认）；
- **C4** 前台变化可编程检测（`GetForegroundWindow` 轮询，B 机制可行性）；
- **C5** 无效 sessionId 的行为差异（界定 B 机制「只能验证 App 级前台、无法区分会话级路由成败」的边界）。

判定：**C2 + C3 + C4 成立 → 深度链接「能达成」**，P1-1 走本 PR 直接启用；否则走清理 + follow-up。探测结论回填本节。

**【探测结论 2026-09-04（Windows 实机，v2.115.0）】——五项判据全部成立**：
- **C1 ✓** 注册表检测有效且能区分：`workbuddy` 在 HKCR/HKCU 均有 handler（`"D:\...\WorkBuddy.exe" "%1"`），`codex` 两处均无。
- **C2 ✓** App 前台化：后台状态（前台为 msedge）派发后 ~1.2s WorkBuddy 到前台；最小化状态恢复 < 100ms。
- **C3 ✓** 会话级直达（OCR 三步闭环实证）：有效 id `672509f4` → 窗口显示对应会话「安装 weather 技能到当前项目」及其正文；无效 id → 会话视图消失、回落首页（无错误弹窗）；再次有效 id → 会话视图恢复。导航与会话 id 一一对应。
- **C4 ✓** 前台变化可编程检测：`GetForegroundWindow` 轮询捕获完整转换序列（+203ms Weixin 瞬时闪现 → +1235ms WorkBuddy 稳定前台）。
- **C5 ✓** 边界确认：无效 id 同样激活 App——**前台验证只能确认 App 级激活、不能区分路由成败**。对 WorkBuddy 无风险（MAM 只派发心跳/db 中的真实 sessionId，路由有效性由构造保证）；Codex 的 threadId 同源性风险仍在（P2-11）。

附带发现：① 协议派发瞬间可能有第三方窗口闪现（实测 +203ms 时 Weixin 短暂置前）——B 机制轮询须以窗口期终点前台为准，不能见到非目标窗口即判失败；② Electron/Chromium 不暴露 UIA 文本树（仅根窗口可读）——B 机制不能依赖 UIA 读内容，只能用前台窗口归属判定。

**判定：C2+C3+C4 成立 → 深度链接「能达成」→ 按决策规则 P1-1 走「本 PR 内直接启用」（待需求方确认后实施）。**

### P1-3 W5 还原在 Windows 未测试 + 文件目标复制语义破坏 SSOT

**根因（定性）**：两个独立缺陷叠加。
（i）**测试面**：`tool_settings.rs:288` `#[cfg(all(test, unix))]` 把旗舰特性测试锚定在开发机的 symlink 语义上——测试与实现同构于同一平台假设，Windows junction 路径从未被执行；Windows 侧只有交叉编译检查（编译通过 ≠ 语义正确）。
（ii）**语义面**：linker 在 Windows 用 junction 替代目录链接是「免权限」下的正确选择，但**文件**目标直接降级为 `fs::copy` 是权限约束下的权宜解——它静默放弃了 SSOT 单源语义（工具侧编辑不回流 `~/.mam`），且「取消勾选还原」会用陈旧 SSOT 覆盖用户在工具侧的副本修改，全程无告知。

**修复（测试面）选项**：
- **A. 平台无关化**：`restore_tests` 改为经 `create_link` 抽象创建目标（Windows 下自动走 junction），断言还原结果，双平台同跑；删除 `#[cfg(all(test, unix))]`。
- **B. Windows 专项补充**：另加 `#[cfg(windows)]` 测试：junction 的 Valid/Dangling 健康判定、跨 junction SSOT 的 `copy_dir_recursive` 暂存、rename 落位、SSOT 缺失时 Skipped 报告。

**推荐 A + B**（A 保证双平台基线，B 覆盖 junction 特有行为）。

**修复（语义面）选项**：
- **A. 同卷 hardlink**（`fs::hard_link`）：免权限、内容单源（同文件两个目录项），`~/.mam` 与各工具目录几乎总在同一用户卷；跨卷/失败回退 copy 并在结果中标注「副本」。
- **B. symlink 优先**：尝试 `symlink_file`（Developer Mode / 管理员下可用），失败回退 hardlink/copy，记录实际形态。
- **C. 维持 copy + 明示**：不动机制，但在启用/还原两端向用户明示「Windows 文件目标为副本，编辑不回流 SSOT」。

**推荐 A 为主、C 无论如何都做（告知义务）**；B 作为后续增强。注意 hardlink 语义与还原路径兼容（hardlink 是真实文件，`restore_mam_link` 的 remove+copy 流程不受影响）。

**【决策 2026-09-04】**：测试面 A + B 均实施；语义面（hardlink / copy 明示）本轮不动，待后续单独决策。

### P1-4 workbuddy.db 连接无 busy_timeout

**根因（定性）**：跨模块经验没有沉淀。`opencode_parser.rs:58` 已有 `busy_timeout(1000)` 的成熟模式，但它是解析器内的局部代码而非共享 helper——新写 workbuddy 解析器时无从继承；测试全部使用静止 DB，锁竞争路径不可复现；三轮评审也只比对 spec 不比对相邻实现。属于「知识以代码形式存在但不可发现」的组织性缺陷。

**选项**：
- **A. 最小补丁**：workbuddy 两处连接各加 `busy_timeout(1000)`，与 opencode 对齐（约 3 行）。
- **B. 消根**：抽 `database::open_readonly_with_timeout(path)`（或 sqlite 工具模块）共享 helper，opencode/workbuddy 共用，杜绝后续解析器再犯。

**推荐 B**（改动仍很小，顺手消根）；A 作回退。

**【决策 2026-09-04】**：选 B（消根）。

---

## 3. P2 修改意见（清单式，不展开设计）【需求方已确认：全部按清单执行】

1. **custom_title 优先**：标题查询改 `SELECT COALESCE(NULLIF(custom_title,''), title)`（spec §4 已同步该要求）。
2. **关窗口拦截**：`settings.tsx:143-151` 的 `beforeunload` 改 Tauri 2 `getCurrentWindow().onCloseRequested`（`preventDefault` 后走三选弹窗），兑现 spec §6「关窗口」拦截；现有操作日志 follow-up 同款。
3. **LAST_SEEN_SESSIONS 全局清空**：值改为 `(tool_id, session_id)` 或按工具分桶；停用工具只清本工具条目（为未来第二个心跳驱动工具消除已自认的脚枪）。
4. **宠物跳转上下文对齐**：`FoxbellPet` 跳转与看板统一走 `useSessionJump`，补 `via=app-fallback` 的 M3 提示，避免两套跳转参数漂移。
5. **rebuildFailed toast i18n**：`settings.tsx:190` 硬编码英文补 `settings.tools.rebuildFailed` key（中/英）。
6. **MSIX 硬编码**：`process.rs:62` `windowsapps/openai.codex_` 至少去版本耦合并加注释；更稳妥是 `windowsapps/` + basename 双条件。
7. **db `status` 列交叉校验（增强，可选）**：JSONL 尾部 Processing 但 db `status='completed'` 且 `updated_at` 陈旧 → 降级 Idle，弥补 mtime 不可知场景。
8. **`tools=[]` 闪白**：enabled_tools 查询未就绪时用骨架/上一次缓存，避免「全部停用」瞬时误渲染。
9. **`as keyof` 强转**：声音配置工具键类型改由后端下发工具列表的联合类型派生。
10. **WorkBuddy 品牌图标**：替换占位素材（PR 待办已列）。
11. **codex threadId 同源性 GUI 实测**：失败则 codex 深度链接分支回退 None（PR 待办已列；P1-2 落地后误标已读风险已消，此项只剩直达率问题）。

---

## 4. 实机验证回归与新发现（2026-09-04 晚，整改实施后）

实机验证结果：**WorkBuddy 检测/跳转完全符合验收**（会话级直达、关闭按键、窗口键入识别关卡均通过）。新发现并处置：

| # | 发现 | 根因 | 处置 |
|---|------|------|------|
| N1 | **Codex APP 会话不出卡（大问题）** | 原 PR 仅按 macOS 拓扑设计：Windows 上 Codex 桌面端为 MSIX（`WindowsApps\OpenAI.Codex_*\app\ChatGPT.exe`），会话运行时为独立 codex.exe（AppData bin），**宿主不叫 codex** → `find_codex_processes(["codex","Codex"])` 永远发现不了 App 形态宿主 → `codex_host_process` 恒 None → W4 聚合不执行 | 已修：`codex_process_names()` 平台分支，Windows 追加 `chatgpt`（Electron 子进程经通用子代理过滤后仅主进程存活，classify_form 判 App）；探针实机验证：发现宿主 pid 3936 + 2 张 deepseek-harness 会话卡（19:03/19:04 rollout）。**非整改批次回归，是原 PR 的 Windows 设计缺口**（与 P0-1 同类） |
| N2 | 设置页增减工具后某列表需切页才刷新（小问题） | 待定位：`applyChanges` 已 `loadToolSettings()` + `invalidateQueries()`，后端 `list_enabled_tools` 无缓存——机制上应实时刷新；需用户指认具体是哪个列表（声音覆盖行/预设/资源分布） | 待用户复现指认后修 |
| N3 | 「deepseek 窗口被瞬间识别成 claude」闪现 | 探针时点 claude 发现为 0 进程；推测为用户在 deepseek-harness 窗口短暂运行过 claude（短命进程 → 卡片闪现即消失，属正常行为）。状态缓存同期有一条真实 Kimi 会话记录 | 待用户确认当时是否运行过 claude；无代码层异常证据 |
| N4 | review F1 | 14 文件 LF→CRLF 翻转进入实施 commit（~3400 行噪声） | 已修：`chore` commit 转回 LF，PR 净 diff 恢复 +1513/-162 |

另：M1（关窗守卫决议后重发 close）、M5（foxbell 测试超时 3s→10s）本轮一并修复。

## 5. 附录 A：Windows 实机证据摘要（2026-09-04）- **环境**：Windows 11，WorkBuddy v2.115.0，安装于 `D:\Program Files\WorkBuddy`（NSIS，非 MSIX），运行中（主进程 pid 2420，窗口标题 `WorkBuddy`）。
- **进程**：进程表仅 `WorkBuddy.exe`（11 个），无任何 codebuddy 进程；`cli/bin/codebuddy` 为 Node 脚本（`#!/usr/bin/env node`）；主日志（2026-08-06~09-04 多条）：`[Sidecar] Creating session __workbuddy_cli_host__-1-221d7536 — D:\Program Files\WorkBuddy\WorkBuddy.exe`。
- **心跳**（`~/.workbuddy/sessions/`，14 份）：serve 进程 `sessionId:"interactive-12032"`（cwd 在 `Temp\workbuddy-host-cli\...`）；prewarm 进程 `sessionId:"prewarm-wb-pool-1788496419201-bb1050"`（36 字符/4 连字符，`kind:"prewarm"`，心跳新鲜）；真实任务 `sessionId:"ecbf3d35-76e9-42df-b71d-89409ec156ea"`（cwd 为真实工作区，`kind:"interactive"`）。
- **项目目录**（`~/.workbuddy/projects/`）：`c-Users-bunny-WorkBuddy-2026-08-06-15-57-15`、`e-LLMproject-0807` 等 → mangle 规则 = 盘符小写 + 去冒号 + 分隔符→`-`；JSONL 内 `cwd` 字段亦为小写盘符形态。
- **JSONL**：行类型分布 `message`(role user/assistant)/`function_call`/`function_call_result`/`reasoning`，与解析器映射一致。
- **workbuddy.db**：`sessions(id, cwd, title, custom_title, status, deleted_at, ...)`，PR 查询列全部存在；运行中 readonly 打开成功（WAL sidecar 正常）；样本行 title 为用户首条消息截断、custom_title 为 NULL。
- **注册表**：`HKCR\workbuddy\shell\open\command = "D:\Program Files\WorkBuddy\WorkBuddy.exe" "%1"`（HKCU 同）；`HKCR\codex` 仅有 `URL Protocol` 标记、**无 shell\open\command**。
- **MCP**：`cli/dist/codebuddy.js` 引用 `.workbuddy/mcp.json`（`~/.workbuddy/mcp.json` 当前不存在，MAM 首次写入时创建即可）。

## 6. 收尾轮处置记录（2026-09-04 晚，macOS 侧）

| # | 处置 | 结果 |
|---|------|------|
| N2 | 用户指认：**资源管理分布**列表需切页才刷新。systematic-debugging 定位根因：设置是独立 WebviewWindow，`applyChanges` 的 `invalidateQueries()` 只作用于设置窗口自己的 QueryClient，主窗口缓存不感知；"切页才刷新"是视图 remount 时 stale refetch 的侥幸路径 | **已修**（TDD）：后端 `update_tool_settings` 成功后 `app.emit("tools-changed")`；主/设置窗口 `setupToolsChangedListener`（`src/lib/query/toolsChangedSync.ts`）监听并全量失效本窗口缓存。 vitest `toolsChangedSync.test.tsx` 锁定「事件→refetch」行为 |
| N3 | 用户确认**未**运行过 claude。代码审计：claude 出卡需三重条件同时满足（进程 basename 精确=`claude` 的存活进程 + 可读 cwd + `~/.claude/projects` 真实会话文件），三处匹配均为精确比对，无跨工具误标通路 → 卡片闪现必然有真实 claude 进程短暂存活。结合事发项目名 deepseek-harness（测试 harness 可能自行拉起 claude 子进程），最可能是 harness 拉起而非用户手动运行 | 挂起待 Windows 机取证（PowerShell：`Get-WinEvent -FilterHashtable @{LogName='Security';Id=4688} -MaxEvents 2000 | Where-Object {$_.Message -match 'claude.exe'}` 或 sysmon；亦可在 `~/.claude/projects/` 看事发时段新增会话文件）。若复现：MAM 探针 + 任务管理器同时盯 |
| macOS 回归 | 门禁：cargo test 206/0、clippy 0、vitest 72/72、pnpm check ✓。临时探针（examples/detect_probe.rs，用后已删）验证：Codex APP 进程发现+每会话一卡聚合正常；无 handler scheme 经 `open` output() **正确快速失败**（退出码 1）；WorkBuddy 心跳驱动在 mac 上等价（serve `interactive-*` 与过期心跳均正确过滤，processes=0 而宿主 alive=true） | 通过 |
| P2-11 | codex threadId↔rollout UUID 同源性 GUI 实测：派发 `codex://threads/01a067b5…` 用户目击**直达 vision-relay 会话**；派发 `codex://threads/01a067b4…` 截图证实**直达 Personal_Infro 会话**（会话内容与 rollout 元数据一致）。两个不同 sessionId 均一对一导航 | **同源性确认，codex 深链保留**，P2-11 关闭 |
| M6 | 登记：Windows 文件目标（`create_link` 降级 `fs::copy`）disable 还原后工具侧遗留副本、且 `NotApplicable` 不进 skipped 报告；语义面改造（hardlink 方案）按 §0.1 决策**不本轮实施**，列后续单独决策 | 已登记本节，待需求方决策 |
| P2-7 | 评估为可选增强，维持不实施（JSONL 尾部 + mtime 阈值已覆盖实测场景，db status 交叉校验仅在"mtime 不可知"的假想场景有增益，避免过度设计） | 关闭 |

遗留（非阻塞）：P2-10 WorkBuddy 品牌图标素材（待设计资源）；行尾治本（`.gitattributes` + renormalize）按需求方要求独立 PR 单独决策。

## 7. 分支收尾第二轮（2026-09-05，Windows 实机）：卡片行为统一 + X 关闭 + codex 深链修复

背景：P2-10 品牌图标已补齐（a0cbcc1）后，实机复测发现三处行为不一致/bug，本轮统一处置。

| # | 问题 | 根因 | 处置 |
|---|------|------|------|
| F-A | vitest 出现 Unhandled Errors（`win.close is not a function`） | settings.tsx 关窗守卫三选决议回调里 `void win.close()` 在 mock 环境（jsdom 无 close 方法）未捕获异常 | **已修**（T0）：包 try/catch 静默（放行标记已置位，真实 Tauri 环境重发 close 正常放行） |
| B-1 | 看板点掉未读卡后，宠物头顶卡晚 ~60s 才消 | 双根因叠加：① petStatus 对 done+unread 绿卡有 `VANISH_TTL_MS=60s` 消失保留；② 宠物窗口感知不到看板侧的已读（跨窗口无同步通道） | **已修**（T1）：①后端 `mark_session_read` / `focus_session::mark_read_on_jump` 删行后广播 `session-read` 事件（复用 N2 tools-changed 模式）；②前端 `sessionReadSync.ts` 监听并回调 FoxbellPet 对状态机执行 ackDone 等价置位（卡片立即消隐）；③**删除宠物本地 60s TTL**——会话从 payload 消失即清卡，与看板一致（绿未读的持续可见由后端 unread pool 保证，App 类会话本来就在 payload 里）；事件差分（语音/姿态 prev 对比）不受影响。**卡片生命周期统一到后端事实源** |
| B-2 | App 类活跃卡（黄/红）没有 X 按钮 | W4 的 X 仅覆盖未读卡（mark_session_read 语义），活跃卡无「暂离不提示」通道 | **已修**（T2）：monitor 层新增进程内 `SESSION_DISMISALS`（key=(tool_id, session_id, status)），`get_all_sessions` 组装末尾按 key 过滤；新 IPC `dismiss_session_card` 写入（不碰 unread_sessions 表，已读逻辑不变）；状态变化后 key 不匹配自然重现；MAM 重启清空（内存语义）。前端 SessionCard 的 X 扩展到全部 App 形态卡（unread 卡维持 mark_session_read，活跃卡走 dismiss）；宠物头顶卡同步加 X |
| B-3 | codex 深链被 A 门误拦，永远降级 App 级聚焦 | `scheme_handler_exists` 判据为「shell\open\command 非空」，而 ChatGPT MSIX 的协议关联走 AppModel，注册表仅有 `URL Protocol` 标记、无 command——**判据假阴性**（实测派发 `codex://threads/<uuid>` 三次均成功前台化且 OCR 证实导航到具体会话） | **已修**（T3）：判据放宽为「command 非空 **或 `URL Protocol` 值存在**」；macOS output() 退出码判据不动；派发后既有 B 前台验证（2s 双连击）继续兜底——真无 handler 导致未前台化时落回近祖聚焦、不误标已读（分层防御） |

测试教训（B-1 用例）：mock `useSessionsQuery` 若每次渲染返回新 data 对象，FoxbellPet 的 `[data]` effect 会 setCards → 无限重渲染 → vitest 进程挂死；mock 必须返回稳定引用（预建两帧常量，测试内切换）。

本轮门禁（Windows 实机）：cargo test 219+ / clippy 0 警告 / pnpm check ✓ / vitest ≥74（含 session-read、dismiss、handler 判据新增用例）。
