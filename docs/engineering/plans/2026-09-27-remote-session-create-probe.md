# 远程新建会话 · Phase P 探测计划（Windows 主 + Mac 量力）

> 日期：2026-09-27 · 上位设计：`docs/engineering/specs/2026-09-27-remote-session-create-design.md`
> 任务性质：**纯探测，零代码修改**。为「远程新建会话」功能的编码批次（Phase C）提供五类定案：① 四家裸命令表；② 弹窗锚点文案 + 处置键序；③ 冷启动/物化时延；④ 首句物化落点；⑤ macOS 通道档位。
> 执行方：两台机器各自独立执行——**Windows 段（P0–P5）**在本机（项目仓库所在机）执行；**Mac 段（M1–M4）**在另一台 Mac 上拉取本文件执行。两段互不依赖，可并行。
> 交付：Windows 段证据全部落盘（主线直接读取）；Mac 段以**最终消息全文返回执行报告**（模板见 §Mac-M4）。

## 0. 总纪律（两段共同，违反则该实验作废重做）

1. **只碰本次探测自建的会话**：所有测试会话一律跑在带 run-id 的全新临时目录里（Windows `%TEMP%\mam-create-<run-id>-<tool>` / Mac `/tmp/mam-create-<run-id>-<tool>`），绝不触碰用户自己的终端、会话、项目目录。
2. **日志追加式带 run-id，永不覆盖**；每个实验一节，记时间戳与原始输出。
3. **结束清场**：taskkill（Windows）/ pkill（Mac）杀掉本次探测起的全部进程；终端窗口关闭；临时目录与证据**保留**（不删）。
4. **不修任何代码、不 commit、不 push、不动分支**；发现疑似产品缺陷只登记进报告，不处置。
5. **结论不得超过证据**：每个定案值都要能指到一条日志/截图/屏读采样；测不了的格子写「不可构造 + 尝试过什么」，不硬凑。
6. Windows 段**必须先读项目技能** `.agents/skills/win-console-inject-probe/SKILL.md` 并按其纪律执行（八条铁律）；屏读/注入脚本缺失时按 `scripts/README.md` 的接口重写，纪律不变。Mac 段参考 `references/mac-verification.md`。
7. 环境变量隔离手法（构造「首装态」用）：`CLAUDE_CONFIG_DIR`（claude）/ `CODEX_HOME`（codex）/ `KIMI_CODE_HOME`（kimi）/ `OPENCODE_CONFIG`（opencode，具体名以 `--help` 实测为准）——指向探测临时目录即得干净首装环境；**用后即还原，绝不污染真实配置**。
8. 本探测**独立于 MAM 应用**（不需要 MAM 运行；若在跑互不干扰，出现探测会话卡片属正常，忽略）。

---

# Windows 段（P0–P5）

> 证据基座：`%USERPROFILE%\mam-probe-m6r\evidence\create-probe\<run-id>\`（run-id = `yyyymmdd-HHmmss`）。下设 `run.log`（总台账）与各段子目录。截图用技能 `scripts/probe-shot-win.ps1`（conhost 归属 cmd 进程主窗口；WT 标签页用 `probe-shot-wintitle.ps1`）。

## P0 · 环境基线（10 分钟）

记录：OS build、PowerShell 版本、Windows Terminal 版本、四家 `--version`、`where <tool>` 解析路径。与 `.agents/skills/win-console-inject-probe/references/known-families.md` 的族表指纹对照，记录漂移。

## P1 · 裸命令与宿主矩阵（30 分钟）

**问题**：新建会话的终端命令是什么形态、双宿主下是否都成立。

- 每家工具（claude / codex / kimi / opencode）× 双宿主（conhost / WT）：
  - conhost：`Start-Process conhost.exe -ArgumentList 'cmd.exe','/k','<tool>' -WorkingDirectory '<临时目录>'`；
  - WT：`wt -d <临时目录> cmd /k <tool>`；
- 进程定位（起窗后 45s 内轮询）：conhost 拓扑从 conhost pid 走父子树找 TUI（cmd → claude.exe/node/bun）；WT 拓扑按「cwd 含 run-id 目录名」扫进程表（sysinfo / Get-CimInstance 均可）。
- **判据**：TUI 进程出现且存活 ≥10s = 命令形态成立。
- **产物**：命令表草稿（工具 → 命令串、宿主、PATH 解析路径、TUI 进程名）。
- 每格记录：起窗 → TUI 进程出现的时延。

## P2 · 屏读文本层 + 冷启动时间轴（60 分钟）

**P2a 屏读脚本就绪（前置交付物）**：扩展技能脚本或新建 `ConOut.ps1`——`AttachConsole(<target pid>)` → `CreateFileW("CONOUT$")` → `GetConsoleScreenBufferInfo` → `ReadConsoleOutputW`（读末 40 行 × 缓冲宽）→ 文本化（剥离颜色属性）→ stdout。**一次性子进程执行 + 结束 FreeConsole 复位**（技能铁律）。先对一个普通 cmd 窗口自测（回显已知文本比对），再上 TUI。
- conhost 与 WT 宿主**各验一次可达性**（attach 目标 = TUI 进程 pid，不是宿主进程）。

**P2b 冷启动时间轴**：每家一个全新临时目录（P0 环境隔离不启用——本段测「用户视角的首启」）→ P1 定案形态起窗 → **每 2s 屏读采样一次，持续至 90s 或连续 5 次采样无变化**。记录：
- 首帧时刻（首次读到非空屏）；弹窗文本出现/消失时刻；**空闲输入框出现时刻与其锚文案逐字抄录**（如 claude 底部快捷键提示行）；
- **信任类弹窗挡住 idle 时**：先按 P3a 流程处置一次再继续采样（该轮弹窗文案的正式取证与键序定案仍归 P3a，本段只推进时间轴）；
- 关键节点截图（首帧 / 弹窗在屏 / idle）。
- claude 双宿主必测；其余工具 conhost 一轮 + WT 量力。

## P3 · 弹窗矩阵（60–90 分钟）

**P3a 信任框（核心）**：每家在**全新目录**首启（claude / codex 预期必现；kimi / opencode 以实测为准）：
1. 屏读取证：弹窗标题、正文、**选项逐字**（含序号/快捷键标记）、默认高亮项（截图辅助判断）；
2. 发处置键序（技能 `ConIn.ps1` 键域：enter / down+enter / 数字键——**按观察到的默认项决定**，默认已是「信任」则 enter，否则先移到信任项）；
3. 处置后 5s 再屏读：确认进入 idle（对照 P2b 锚文案）；
4. 产物：锚文案逐字 + 键序 + 处置前后屏读样本 + 截图。
- **用环境变量隔离构造「真正首装态」复核一轮**（claude/codex 至少）：隔离 HOME 类变量再首启，观察与「全新目录」路径的差异（onboarding/登录提示是否额外出现）。

**P3b 更新提示**：构造版本落差（尝试：降级安装/缓存版本号文件/离线首启等，各家机制不同）。**不可构造即登记**（写明尝试过什么、卡在哪），以官方 changelog/文档佐证形态。
**P3c 引导流 / onboarding / 登录态**：P3a 的环境隔离轮已覆盖大部分；未覆盖的（如主题选择、API key 引导）以隔离环境实测，不可安全构造则登记。

## P4 · 首句物化（45 分钟）

对 P3a 处置完成、已 idle 的会话，用 `ConIn.ps1` 注入探针 `hi` 并提交回车（回车形态按族规格：claude/kimi/opencode A 族 VK 回车；codex B 族 VK 回车——键域见技能）：
- 注入前记时；随后**每 2s 轮询四家会话存储落点**（≤120s）：
  - claude：`~/.claude/projects/<目录名转写>/` 新 `.jsonl`
  - codex：`~/.codex/sessions/<Y/M/D>/rollout-*.jsonl`
  - kimi：`~/.kimi-code/sessions/`（或 KIMI_CODE_HOME 指向下）新目录
  - opencode：`~/.local/share/opencode/opencode.db` ——**拷贝 db+wal+shm 副本后查询**（技能纪律：SQLite 活库不可直查），比对新增行
- 记录：落点路径、首条用户消息**完整原文**（含 TUI 修剪形态）、注入→落盘时延、模型回复到达时延（可选）。

## P5 · 汇总

在证据目录写 `spec-draft.md`：四家 × {命令形态, 宿主可达性, 信任框锚文案+键序, 更新/引导登记, idle 锚文案, 冷启动时延, 物化时延, 落点} 定案表草稿 + 屏读脚本副本。**最终消息返回简报**：每段 PASS/FAIL/不可达一行 + 证据路径 + 定案表要点。

---

# Mac 段（M1–M4，量力而为）

> 环境：独立 clone 本仓库到 `/tmp/mam-create-probe/repo`（分支见下），证据 `/tmp/mam-create-probe/<run-id>/`。工具版本记录 `--version`。**全程不 commit、不 push、不动用户真实配置与会话。**

## M1 · 屏读可达性判定（决定 Mac 通道档位，30–40 分钟）

三路分别尝试对**探测自建 TUI 会话**读出屏上文本：
1. **tmux**：会话跑在 tmux 里 → `tmux capture-pane -p -t <target>`——预期可达（resume 已用）；
2. **iTerm2**：AppleScript 读 session 文本（`tell application "iTerm2"` 的 session `text` / `contents` 类属性，以实际字典为准）——待实证；
3. **Terminal.app**：AppleScript 尝试读 tab 文本——预期不可达，如实记录。

每路产出：可达/不可达 + 读到的文本样本（TUI 画面能否完整读出，含底框/弹窗区）。
**结论三档**：任一路可达 → Mac 可走锚点式（记录是哪路）；仅 tmux 可达 → 锚点式但建议新建会话固定跑 tmux（记录该建议）；全不可达 → Mac 剧本式退化。

## M2 · 裸命令起窗效果（20 分钟）

Terminal.app：`do script "cd '<临时目录>' && <tool>"` 裸命令起窗（resume 同机制）+ 效果回查（2–3s 内按 tty/进程存在确认，复用 resume 修复批思路）。claude 必测，其余量力。iTerm2 `create window with command` 已知恒死窗（既有结论，引用不复测）。若 M1 建议固定 tmux：补测「开窗 → tmux 新会话 → 跑工具」形态一轮。

## M3 · 时间轴 + 信任框（M1 可达时，30 分钟）

claude（+量力 codex）：全新临时目录首启 → 每 2s 读屏采样 → 信任框锚文案逐字 + 处置键序实证（AppleScript ` keystroke` 需窗口前台——记录升窗方案与体验代价）→ idle 锚文案 + 时延。M1 不可达则本段跳过（剧本式参数引用 Windows P2 结论 + 保守延时）。

## M4 · 汇总报告（**最终消息全文返回**，同时落 `/tmp/mam-create-probe/<run-id>/report.md`）

结构：① 环境基线（OS/各工具版本）；② M1–M3 逐项结论表；③ 关键锚文案逐字；④ 时延数据；⑤ **Mac 通道档位建议**（锚点式-哪路 / 剧本式 / 本批不做）+ 理由；⑥ 测试痕迹与清理说明；⑦ 遗留问题。

---

## 产物与归档对照表

| 产物 | Windows | Mac |
|---|---|---|
| 证据/日志 | `%USERPROFILE%\mam-probe-m6r\evidence\create-probe\<run-id>\` | `/tmp/mam-create-probe/<run-id>/` |
| 定案表草稿 | 同上 `spec-draft.md`（主线读取后整理入库） | 报告内嵌 |
| 屏读脚本副本 | 证据目录 | 报告附关键脚本片段 |
| 最终回传 | 简报（证据在盘，主线直读） | **报告全文**（主线读不到 Mac 磁盘） |
