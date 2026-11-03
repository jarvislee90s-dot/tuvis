# 跳转按需注入与 hook 通道修复设计（issue #43 第三轮探测落地方案）

- 日期：2026-09-12
- 状态：草案（待评审）
- 关联：issue #43（marker 通道与三轮实机验收）、issue #58（codex hook 机制调研与插件化暂缓决策）、PR #56（分支 `feat/marker-and-issue-batch`）
- 前置事实：本文所有「实证」结论均来自 2026-09-11/12 三轮 Windows 实机验收（详见 #43 评论），本文不再重复论证

## 1. 背景与目标

### 1.1 背景

MAM 的卡片跳转在 Windows 上依靠五层消歧判定链（marker 精确匹配 → 标题匹配 → 排除制推理 → UIA 正文 → 选择器）。三轮实机验收确立了以下事实：

1. **跳转正确性契约已达成**：「锁对或弹选择器、绝不静默锁错」在 codex 单开/双开、claude 双开、kimi 三形态下全部验证通过（PR #56）；
2. **marker 注入基础设施可用**：helper 双路线（B=官方控制台通道改标签 / A=外部直改窗口标题）均实证 GO，但 hook 触发链路对现有工具全部不通——claude TUI 活动期 3 秒内冲掉 marker、codex 根本不读 hooks.json（#58）；
3. **claude 空闲态是注入窗口**：claude 仅在回合边界与动画帧重写标题，空闲期零重写（15 秒采样全程存活）；需要跳转的目标会话恰恰以空闲态为主；
4. **外部按 PID 附加可行**：PowerShell `AttachConsole(pid)` 对 claude 的 ConPTY 附加成功，证明 helper 不依赖「自家父链」也能到达目标会话的终端；
5. **claude hook 事件通道事实上中断**：claude 把 hook 进程脱管（$PPID 恒为 1），全部事件互相覆盖在 `1.json`，消费端按 PID 取事件永远取不到；
6. **SessionStart 报错根因**：claude 在 Windows 经 `powershell -Command "<用户command>"` 包装执行钩子，MAM 注册命令中的引号破坏外层引号配对（报错位置实证落在结尾引号处）。

### 1.2 目标

| # | 目标 | 对应改动 |
|---|---|---|
| G1 | claude 多开跳转从「选择器手选」升级为「自动锁对」 | 改动一、二 |
| G2 | codex 同项目双开跳转从「选择器手选」升级为「自动锁对」 | 改动一、二 |
| G3 | 消除 claude SessionStart 启动报错 | 改动四 |
| G4 | 复活 claude hook 状态通道（Stop 宽限等逻辑恢复生效），消除多会话事件互覆 | 改动五 |
| G5 | 简化：下线被取代的 hook 周期注入机制 | 改动三 |

### 1.3 非目标

- **codex hook 接入**：插件化已决策暂缓（#58，与插件自由插拔理念冲突）；本文不依赖 codex hook；
- **kimi / opencode 注入**：两者的标题匹配键已实测够用（kimi 双开免手选），注入只会在其标题上留下无收益的后缀；
- **活动中会话的 marker 存活性**：活动中的会话不是跳转目标，不做任何对抗 TUI 重写的努力；
- **跨窗口多标签精确定位**：维持既有能力边界（锁定到窗口，标签切换由用户完成）。

## 2. 名词与口径

- **marker**：`MAM:<session_id 剥连字符前 12 位>`，注入到终端标题的会话身份标记；口径三处互引（commands/session.rs 匹配侧 / helper / 文档），本文不改变该口径。
- **按需注入**：跳转动作发起的瞬间，对目标会话的终端注入一次 marker，随即执行既有判定链。与已退役的「hook 周期注入」（低频事件反复重贴）相区别。
- **空闲态**：会话未处于回合活动期（无 spinner 动画）。仅空闲态保证 marker 存活。

## 3. 改动设计

### 3.1 改动一：helper 收敛为「按 PID 附加」唯一形态

**现状**：mam-marker 只有「父链自走」一种定位方式——从自身进程沿父链上溯找终端。该方式只为 hook 周期注入服务（helper 被钩子脚本 spawn，顺自家父链找终端）。随改动三退役 hook 周期注入，父链自走**失去全部生产调用方**，予以删除——helper 收敛为唯一调用形态：

```
mam-marker --pid <目标进程pid> <session_id>
```

内部仍为 B→A 双路线融合执行（两轴正交：定位轴只剩 --pid 一种；写入轴的 B/A 失败面不重叠，必须共存）：

- **路线 B（官方控制台通道，tab 级）**：`AttachConsole(目标pid)` 附加到目标会话所在控制台 → 读现标题 → 追加 marker → `SetConsoleTitleW` 写回。覆盖**多窗口**场景（多开时各会话各占一个 WT 窗口）；
- **路线 A（外部直改，窗口级）**：通过进程快照解析**目标 pid** 的父链，找到第一个拥有可见顶层窗口的非黑名单祖先；恰好单窗口时 `SetWindowTextW` 追加 marker，多窗口放弃（由路线 B 的 tab 级标题覆盖）。即时生效、不依赖终端配合，为 B 的「无回执、可能被终端配置忽略」提供保险；
- B 的成功与否无法读回（控制台侧无回执），故双路线**总是都执行**，各自记日志，任一成功即退出码 0。

**判定依据**：第三轮以 PowerShell `AttachConsole(pid)` 复刻路线 B 语义，外部附加 claude ConPTY 成功且 marker 立即可见；父链解析逻辑（ToolHelp 快照）仓库内已有同款实现可复用。

### 3.2 改动二：跳转前按需注入（focus_session Windows 链路）

**触发条件**（三者同时满足才注入，任一不满足直接跳过、零开销）：

1. 会话为 CLI 形态；
2. 工具 ∈ {claude, codex}（没有可靠静态标题键的两个工具；kimi/opencode 的标题键已实测够用，不注入以保持其标题干净）；
3. helper 已安装（`~/.mam/bin/mam-marker.exe` 存在）。

**时序**（定性）：

```
点击卡片
  → spawn helper --pid <会话pid> <session_id>（异步，不阻塞等待其退出）
  → 有界等待：轮询候选窗口标题是否出现 marker，上限约 500ms
      （B 实证 idle 存活 ≥15s，500ms 上限远小于存活期；等待的是 ConPTY 渲染到窗口标题的延迟）
  → 既有五层判定链原样执行，marker 层（①）对含 marker 的窗口精确命中
```

**失败语义（全部静默回落，零回归）**：

| 失败点 | 行为 |
|---|---|
| helper 不存在（未升级/被杀软删/手动卸载） | 跳过注入，判定链照旧 |
| AttachConsole 失败（会话活动中/控制台已消失/权限） | helper 退出码非 0，MAM 不感知细节，等待期自然超时后照旧 |
| 等待期内 marker 未出现在任何候选窗口标题 | 超时放行，落入既有层（标题键/UIA/选择器） |
| 目标会话活动中，marker 注入后被 TUI 冲掉 | 同上，落回既有层；活动会话本就不是跳转目标 |

**残留语义**：跳转成功后，目标窗口标题残留 ` — MAM:xxx` 后缀。claude 在下一回合活动时自然重写冲掉（自愈）；codex 标题静态、后缀持续可见——定性决策为**接受残留**（换取双开自动锁；如未来反馈介意，可在锁定成功后追加一次「去标记」调用，本期不做）。

**门控**：按需注入**不受** `MAM_MARKER` 环境变量门控——它是用户主动点击触发的低频动作，且全部失败路径零回归；配合改动三（hook 周期注入退役），该门控随之消失。

### 3.3 改动三：hook 周期注入退役

**现状**：hook 脚本在低频事件（Stop/PostToolUse/SessionEnd/UserPromptSubmit）里调用 helper 重贴 marker，意图是「保持 marker 新鲜」。三轮验收证伪了该思路的前提——claude 活动期重贴必被冲、codex hook 根本不触发；且按需注入只在「需要的那一刻」注入，完全取代之。

**设计**：

- `HOOK_SCRIPT` 删除 marker 注入块，脚本回归**纯事件记录**（读 stdin → 写事件文件，见 3.5）；
- `MAM_MARKER` 环境变量门控及 `hooks.rs` 内相关注释一并移除；
- helper 随之收敛为 `--pid` 唯一形态（见 3.1），父链自走与 `ATTACH_PARENT_PROCESS` 特例删除；
- marker 口径的「三处互引」缩减为「两处」（session.rs 匹配侧 / helper），注释同步。

**收益**：少一个常驻写入面（不再向用户终端标题做周期性写入）；消除「注入了但没用的」死机制与「MAM_MARKER=1 才生效」的隐藏开关；helper 从双定位模式收敛为单一形态，无生产死代码。

### 3.4 改动四：SessionStart 引号绕开

**根因**：claude Windows 侧以 `powershell -NonInteractive -ExecutionPolicy Bypass -Command "<用户command>"` 包装执行钩子命令；MAM 注册的 `bash "C:\…\status-hook.sh"` 中的内层双引号破坏外层 `-Command` 引号配对（TUI 报错「行:1 字符: 48」实证落在结尾引号位置）。同一命令在非 TUI 通道执行正常。

**设计**：`register_hooks_for_tool` 生成命令时，**路径不含空格则不加引号**：`bash C:\Users\<u>\.mam\hooks\status-hook.sh`。核验逻辑（`expected_cmd`）与去重逻辑（`command_str`）同源推导，自动跟随。

**已知边界**：路径含空格（如用户名带空格）时维持加引号注册——该场景下 SessionStart 报错可能复现，属已知残留，注册处注释声明；后续如有用户反馈再评估 8.3 短路径等方案（本期不做）。

**待实机确认**：TUI 侧报错未能离线完全复现（非 TUI 通道三路验证均 exit=0），修复后需在 TUI 实测 SessionStart 无报错。

### 3.5 改动五：事件文件键 session_id 化

**现状**：hook 脚本以 `$PPID.json` 命名事件文件，意图是「hook 由 CLI 进程 spawn，PPID 即 CLI 进程号」。实证 claude 将 hook 进程脱管（4/4 事件 PPID=1；跨 MSYS/原生进程边界 $PPID 是虚拟值），多会话事件全部覆盖在 `1.json`；消费端 `get_all_sessions` 按 `session.pid` 取事件永远取不到——**claude 的 hook 状态通道（Stop 宽限保持黄灯等）今天事实上中断**。

**设计**：

- **脚本侧**：事件文件名改为 `$SESSION_ID.json`（session_id 本就从 stdin 解析）。写入前对 session_id 做字符白名单校验（字母/数字/连字符），不合法则**丢弃该事件**（防路径注入；合法 UUID 形态下永不触发）；
- **结构侧**：事件 JSON payload 本就携带 session_id 字段；Rust `HookEvent` 结构体无需新增字段——映射键来自**文件名**，消费逻辑只使用 event/ts 两个既有字段；
- **消费侧**：`get_all_sessions` 的 `hook_events.get(&session.pid)` 改为 `hook_events.get(&session.id)`，`read_hook_events` 返回类型从 `HashMap<u32, HookEvent>`（PPID 键）改为 `HashMap<String, HookEvent>`（session_id 键）；会话宽限表（grace map）维持按 pid 键（循环局部有会话上下文，不跨请求）；
- **兼容**：读取侧仅保留 30 秒内的新鲜事件（TTL 既有语义），旧 PPID 命名文件自然过期，**无迁移逻辑**；
- **多事件语义**：同会话同事件覆盖写=保留最新状态，语义正确；不属于任何已知会话的孤儿文件 30 秒后自然清理。

**效果**：claude 的 Stop 宽限（防误报红灯）等依赖 hook 事件的逻辑恢复对 claude 生效；codex 无 hook（#58），事件面天然为空，无需特判。

## 4. 数据流（定性）

**跳转链（改动后）**：

```
点卡片 → focus_session
  → [claude/codex 且 helper 在场] 注入：helper --pid <pid> <sid> → 有界等待 marker 上窗
  → resolve_and_focus 五层判定链（marker ① → 标题键 ①′ → 排除推理 ② → UIA ③④ → 选择器 ⑤）
  → 失败路径逐层回落，最终兜底选择器；任一层锁定即聚焦
```

**事件链（改动后）**：

```
claude 钩子触发 → status-hook.sh（纯事件记录）
  → 写 ~/.mam/events/<session_id>.json（白名单校验，非法丢弃）
→ MAM 每轮扫描：过滤 30s 外 → session_id 键映射 → get_all_sessions 按 session.id 消费
  （Stop 宽限保持黄灯等既有逻辑恢复对 claude 生效）
```

## 5. 错误处理汇总

| 场景 | 行为 | 用户感知 |
|---|---|---|
| helper 缺失/损坏 | 跳过注入 | 无（跳转行为同今日） |
| 目标会话活动中，注入被冲 | 等待超时，落既有层 | 无（活动会话非跳转目标） |
| AttachConsole 失败 | 同上 | 无 |
| session_id 含非法字符（理论不触发） | 丢弃该事件 | 无 |
| 钩子路径含空格 | 引号注册维持现状 | SessionStart 报错可能复现（已知残留，注释声明） |
| 注入成功但锁定失败 | 落选择器 | 多一步手选（与今日一致） |

## 6. 测试与验收

### 6.1 mac 本机可测

- helper：`--pid` 模式的 marker 构造/标题追加纯函数单测（沿用既有口径测试）；
- 事件读取：session_id 键的解析、白名单校验、TTL 过滤单测（tempdir fixture）；
- 注册命令：无空格路径去引号 / 含空格路径保持引号的分支单测；
- 全量回归：cargo test + windows-gnu 交叉编译门禁 + 前端套件。

### 6.2 Windows 实机验收清单（第四轮）

- [ ] claude 双开（不同项目）点卡片**自动锁对**（不再弹选择器）；会话空闲态前置条件
- [ ] codex 同项目双开点卡片自动锁对（从选择器升级）；单开不回归
- [ ] kimi / opencode 跳转零回归（无注入、标题无后缀、行为同第二轮）
- [ ] claude TUI 启动无 SessionStart 报错（去引号注册生效）
- [ ] `~/.mam/events/` 按 session_id 分文件、30 秒自动清理；claude Stop 后黄灯宽限逻辑生效（多会话不互覆）
- [ ] 无 helper 环境（删除 `~/.mam/bin/mam-marker.exe`）全链零回归
- [ ] 判定口径沿用：「锁对或弹选择器、绝不静默锁错」

## 7. 里程碑与关联

- 实现于分支 `feat/marker-and-issue-batch`（PR #56 保持 draft 直至此轮验收通过）；
- 第四轮 Windows 验收通过后：关闭 #43、PR #56 转正；
- codex hook 插件化不在本文范围（#58 已记录决策与未来选项）。
