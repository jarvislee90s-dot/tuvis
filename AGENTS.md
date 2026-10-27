# 项目指令

## 工作区与分支（2026-10-22 用户裁决）

- 默认直接在 `/Users/jarvis/Documents/tuvis` 内建立功能分支、开发与验证。
- 如需隔离 worktree，只能建在 `/Users/jarvis/Documents/` 下；不得使用 `~/.codex/worktrees/` 等其他目录。

## 项目宪法（最高设计依据）

`docs/MASTER-PLAN.md`（兔维斯 远程接入与操控平台 · 总需求说明书）是本项目**唯一最高权威设计文档**，定义三期总纲（一期只读接入 / 二期消息注入 / 三期全控与任务流转）、共用底座、技术选型与横切原则。任何设计文档、spec、实现、任务列表与其冲突时，**一律以它为准**。

- **未经用户在场明确同意，任何人（含任何 AI agent）不得修改 `docs/MASTER-PLAN.md`**
- 发现实现或下层文档与宪法冲突时：停下，向用户报告冲突点，由用户裁决"改宪法"还是"改实现"，不得自行处置
- 调研资料与三期参考库索引：`research/README.md`（持续跟踪外部依赖版本）
- 依赖升级与月度批处理流程：`docs/DEPENDENCY-UPDATES.md`（dependabot 节奏、`Tauri version parity` 门禁、红灯处置表、ignore 命令表）

## 语言规范
- 本项目所有设计文档、规格说明、计划文档、任务列表均使用中文撰写
- 代码标识符使用英文，代码注释尽量使用中文
- Git commit message 可使用英文

## 发版文档纪律（2026-10-16 用户裁决）

**每次发布正式版前，两轮检查缺一不可；未过关不得打 tag。**

### ① 变更覆盖检查（diff 驱动，先做）

对上一版 tag 执行 `git log vX.Y.Z..HEAD` / `git diff vX.Y.Z..HEAD`，梳理本版**全部变更**
（逐 commit 归类，先交用户确认无遗漏），再逐项核对三份发版文档是否覆盖：

- `CHANGELOG.md` 本版小节：全部变更入库（工程质量类可合并叙述，但不漏项）
- `docs/release-notes/vX.Y.Z.md`：用户可感知变更全覆盖；行为变更（默认值变化、设置迁移、
  快捷键/入口变化等）必须写进「升级须知」
- `README.zh-CN.md`（中文主笔；英文侧 `README.md` 同步）：功能概览与受影响的功能章节反映最新行为（新功能补入概览、行为变更就地更新）
- `docs/capabilities.json`（官网兼容矩阵数据源）：较上一版有能力矩阵变化时，更新矩阵内容并刷新其 `updated` 日期字段（官网「更新于」直接显示此值）

### ② 中英对照检查

覆盖全部双语资产；本轮带中文改动的条目，英文侧必须同步刷新（经用户确认后可由代理翻译）：

| 双语资产 | 中文侧 | 英文侧 | 说明 |
| --- | --- | --- | --- |
| 前端页面文案 | `src/i18n/locales/zh.json` | `src/i18n/locales/en.json` | `pnpm check:i18n` 只保**键集**一致，键下的英文值漂移不报，须逐键比对 |
| 项目 README | `README.zh-CN.md` | `README.md` | 无门禁，人工/代理核对；`README.md` 为英文侧（默认展示），中文主笔在 `README.zh-CN.md` |
| 用户手册（官网 `/docs` 挂载） | `docs/user-manual/zh/`（13 章） | `docs/user-manual/en/` | 无门禁；en 曾为骨架，至少核对本版有变动的章节 |
| 更新日志（官网 `/changelog` 挂载） | `CHANGELOG.md`（纯中文） | `CHANGELOG.en.md`（纯英文） | 2026-10-17 拆分（此前「同文件内嵌 `EN:` 行」契约作废）：两份各自单语、版本/日期/分区逐条对齐，未译条目只写中文版 |

检查方法：对上一版 tag 执行 `git diff vX.Y.Z..HEAD -- <上述路径>`，列出本轮全部中文改动逐条核对。
配套的发版同步义务（单语，不涉对照）：`docs/capabilities.json` 与 README「多终端能力矩阵」
**两处随发版同步更新**（官网拉取前者渲染兼容矩阵）；`docs/release-notes/vX.Y.Z.md`
为纯中文产物，不要求英文对照（更新日志已入上表双语对照面）。

## 文档分工纪律（2026-10-23 用户裁决）

说明性文档三分为「门面 / 手册 / 账本」，职责互斥；允许同一功能在两处各有一句话，但细节只允许落在一处：

| 文档 | 唯一职责 |
| --- | --- |
| `README.md` / `README.zh-CN.md` | 门面：是什么/为什么/30 秒上手/去哪学。只允许：首屏（logo+一句话+badge+全景主图）、功能卡片（图标+一句话+链接）、Quick Start、能力矩阵表（随 `docs/capabilities.json` 发版同步）、精简 Roadmap（≤5 条）、社区区、法务小节。 |
| `docs/user-manual/{zh,en}/` | 手册：唯一 how-to 权威。全部操作细节、向导分步、注入能力现状申报、配置说明。zh 改动必须同步 en（遵守发版文档纪律的中英对照要求）。 |
| `CHANGELOG*.md` + `docs/release-notes/` | 账本：唯一变更权威。逐版本变更与发版说明；README 不复述版本细节。 |

执行纪律：

- README 内禁止「发版级细节段落」（向导分步、逐工具能力现状、配置字段说明等）；新增功能说明先判断归属：门面一句话 → README；操作细节 → 手册；变更 → 账本。
- README 中英两版结构对等（章节骨架一致），语言切换行置于首屏。
- 能力矩阵表只在 README 保留表本体；表后长文一律进手册对应章节。
- README 社区区链接只允许稳定 URL（QQ 官方加群链接 / Discord 永久邀请 / Discussions），二维码图片入 `docs/images/`；**禁止直挂会过期的群码截图**（QQ 群码约 7 天过期）；换群只换图片文件与链接文本，群号文字兜底常驻。

## 技术栈
- Tauri 2 + Rust + React 19 + TypeScript + shadcn/ui + Tailwind CSS v4

## 构建与开发命令

```bash
pnpm install            # 安装前端依赖
pnpm tauri:dev          # 启动开发模式（Rust + Vite HMR）
pnpm build              # TypeScript 编译 + Vite 打包
pnpm check              # 完整检查：format + lint + build
pnpm format             # Prettier 自动格式化
pnpm format:check       # Prettier 只读检查
pnpm lint               # ESLint 检查
pnpm lint:fix           # ESLint 自动修复

# Rust 专用检查（在 src-tauri/ 目录下）
cd src-tauri && cargo check    # 编译检查（快）
cd src-tauri && cargo test     # 运行 Rust 单元测试
cd src-tauri && cargo clippy   # Rust 代码 lint
```

## 采集域开发纪律（2026-10-07 事故后新增）

**改 `src-tauri/src/services/usage/**` 之前，先停 `pnpm tauri:dev`**（或把 `~/.dsh` / `TUVIS_HOME` 指到影子目录再动）。

- **为什么**：`pnpm tauri:dev` 的文件监视器会在保存后重建并重启应用，而用量采集**会写真实账本**
  （`~/.tuvis/tuvis.db`）。2026-10-07 的 dsh 时间归属改造中，应用在源文件改到一半时跑了三轮真实采集，
  真机 dsh 账本被灌到约 4×（22.50B vs 事件流真值 5.67B），只能靠「删 dsh 行与游标 + 全量重采」
  重建（用户 18:38 裁决）。事故全文（根因 / 回退点路径 / 重建后验收 / 已知差异）见
  `docs/release-notes/2026-10-07-dsh-ledger-rebuild-incident.md`。
- **同类风险（同一事故的直接根因）**：**采集游标字段的语义变更必须当成迁移处理** ——
  旧版本写下的 `0` 常常表示「不适用」，新代码若把它读成「水位 = 0 / 从头开始」，
  会把每个会话的**全部历史**重复入账。新增或改写水位字段时，必须给一个「本实现写过」的标记位
  （如 dsh 的 `last_cumulative` = 已记录水位 R），并为「老游标」补一条回归用例。
- 次选加固（本轮未做）：给采集加一道「源码树脏则不落库」闸（仅 debug 构建）。

## 项目结构

```
# 项目根目录
├── src/                    # 前端源码（React + TypeScript）
│   ├── pages/             #   页面路由：首页、设置、关于
│   ├── components/        #   UI 组件（会话卡片、资源管理、预设组等）
│   ├── hooks/             #   React Hooks（会话轮询、通知、更新器）
│   ├── stores/            #   Zustand 状态管理
│   ├── lib/               #   工具函数（音频、快捷键、截图）
│   ├── i18n/              #   国际化（中文 / English）
│   └── types/             #   TypeScript 类型定义
├── src-tauri/             # Tauri Rust 后端
│   └── src/
│       ├── adapter/       #   Agent 适配器（Claude / Codex / OpenCode / OpenClaw / Kimi Code）
│       ├── monitor/       #   进程扫描、会话解析、状态判定
│       ├── services/      #   按功能域拆分的业务服务（skill/resource/mcp/preset/plugin/manifest）
│       ├── linker/        #   三层符号链接映射（SSOT → Tool → SubAgent）
│       ├── commands/      #   按模块拆分的 Tauri IPC 命令
│       ├── database/      #   SQLite 数据层（schema/migration/dao）
│       ├── window/        #   终端聚焦（iTerm2 / Terminal.app / tmux）
│       ├── plugins/       #   系统托盘
│       └── session/       #   会话模型与状态枚举
├── docs/                  # 开发文档
├── scripts/               # 构建/发布脚本
├── research/              # 早期调研文档（本地参考，不入库）
├── package.json           # 项目配置
├── vite.config.ts         # Vite 构建配置
├── tsconfig.json          # TypeScript 配置
├── README.md              # 项目说明（英文，默认展示）
└── README.zh-CN.md        # 项目说明（中文主笔）
```

## 卡面状态权威源：屏读为准（2026-10-03 用户裁决）

兔维斯 远程卡面（问答卡的勾选态、自由作答内容、当前所在题）的**状态权威源 = 终端屏读**，不是本地发送记忆。远程注入是「尽力而为」：网络/通道/用户在终端手动操作/应用重启，都会让两边漂移——本地记忆**只作乐观显示**，最终必须以屏读核对为准。

- **交互后延迟核对**：每次注入动作（点选项/打字/切题）的回执必须携带**动作后的屏读快照**（编排内置闭环核验，延迟 0.x 秒等 TUI 重绘）——核对不符时卡面回退到屏读状态（例如选项未翻转就不显示已选中），让用户重试；
- **GET 初始化纠偏**：拉取问答载荷时附带终端当前屏的快照（题干对位到载荷题 + 勾选态 + 自由作答内容；Review 在场 → 直接进确认卡）——覆盖「兔维斯 重启后重开页面」「终端手动作答后回来」等漂移场景，**不做时刻轮询**（GET 时机 = 挂载/状态跃迁，交互回执时机 = 每次动作后）；
- **快照丢失保守降级**：屏读不可用（非 Windows/进程退出/attach 失败）→ 快照为 null，前端维持本地状态（不假清不假同步）；
- **回执即快照的传播面**：toggle 回执 `checked`（既有）/ 多选自由作答回执 `text`+`checked` / 切题回执 `screen{heading,checked,freeText}` / GET `screen`——四路同一权威源；**中止回执不带快照但前端中止后自动重拉一次 GET**（键可能在轮询窗尽后才被终端消费）。
- **已知边界（如实申报）**：单选题的选中项在终端屏上**无勾选框标记**（高亮是属性层，字符读法不可见）→ mqSelected 仍为本地记忆，快照权威源不覆盖该面；快照对位失败（题干匹配不到载荷题/多义匹配/勾选数不符）→ **放弃同步维持现状**（不猜纪律）；**终端手动输入且已切离该题的自由作答内容**不在任何快照窗口（快照只覆盖终端当前停着的题）→ 确认卡如实显示「未作答」（真值在终端 Review 页；Review 回显解析留待后续批次）。

**项目专属技能（`.agents/skills/`，随库分发）**：`win-console-inject-probe` —— Windows 终端注入与屏读探测工作流（注入侧：P1 输入模式分族 + 确认子集 P0–P7 快路径 + 全量方法论 + M6R 验证脚本套件；屏读侧：四闸门证据纪律 + 只读探针 + 屏读形态矩阵，见 `references/screen-read-matrix.md`）。**凡涉及以下任务必须主动调用该技能**（读其 SKILL.md 按流程执行，勿从零自研探测方法）：①新 CLI 工具接入前的 Windows 注入与屏读规格探测；②注入故障诊断（写不进/被吞/打完字不提交/停摆）；③工具大版本升级后的注入与屏读规格复验；④**屏读解析与实机不符的诊断**（读得到屏但解析不出 / 中止文案与屏面状态对不上 / 卡片与终端脱钩）——先活体 dump 取证再改判据，禁止按档案手抄夹具。产出 = 单工具注入规格定案表 + 屏读形态矩阵（四闸门：样本门槛 / 证据自带失败 / 账本吸收文案漂移 / 屏读体检表），归档 `research/refs/phase2-消息注入/`。

## 架构概览

**Tauri 2 桌面应用**，Rust 后端 + React 19 前端，通过 Tauri IPC (`invoke`) 通信。

### 后端（Rust）— `src-tauri/src/`

- **`adapter/`** — 工具适配器，实现 `AgentAdapter` trait。每个适配器负责发现进程、解析会话、读写配置、管理 skill 目录。
- **`monitor/`** — 进程扫描、JSONL 会话解析、Hook 事件读取、状态判定。
- **`services/`** — 按功能域拆分：`skill/`（安装/启用/禁用）、`resource/`（扫描/导入/补链）、`mcp/`（JSON/TOML/JSONC）、`preset/`、`plugin/`、`manifest/`。
- **`linker/`** — 符号链接/交接点管理，三层映射：
  - **Layer 1**: SSOT 全局仓库 `~/.tuvis/skills/`
  - **Layer 2**: 工具级激活目录 `~/.tuvis/active/<tool>/skills/`
  - **Layer 3**: 子 Agent 级激活目录 `~/.tuvis/active/<tool>/<sub-agent>/skills/`
- **`database/`** — SQLite 数据层（`~/.tuvis/tuvis.db`，schema/migration/dao）。
- **`commands/`** — 所有 `#[tauri::command]` IPC 处理器，按模块拆分。
- **`window/`** — 通过 AppleScript 聚焦终端（iTerm2/Terminal.app）和 tmux。
- **`plugins/system_tray.rs`** — 系统托盘（状态指示 + 预设菜单）。

### 前端（React/TypeScript）— `src/`

- **`pages/`** — 路由页面：首页、设置、关于。
- **`components/`** — UI 组件：会话卡片、资源管理双视图、预设组、MCP 面板等。
- **`stores/sessionStore.ts`** — Zustand 会话状态存储。
- **`hooks/`** — `useSessions`（轮询）、`useNotification`、`useUpdater`。
- **`lib/`** — 音频（Web Audio）、快捷键、截图工具。
- **`i18n/`** — i18next，支持中文和英文。
- **`tauri-mock.ts`** — 浏览器/Playwright 渲染时的 Tauri API Mock。

### IPC 数据流

```
前端 (invoke) → commands/ → services/linker/adapter → database/ (SQLite)
                                      ↓
                                文件系统（符号链接、配置文件）
```

### 三层 Skill 映射

```
Layer 1 (SSOT):    ~/.tuvis/skills/brainstorming/SKILL.md
                         ↓ 符号链接
Layer 2 (Tool):    ~/.tuvis/active/claude/skills/brainstorming → Layer 1
                         ↓ 符号链接
Layer 3 (SubAgent):~/.tuvis/active/claude/sub-agent-1/skills/brainstorming → Layer 2
```

Layer 3 指向 Layer 2（而非 Layer 1），因此工具级禁用会自动断开所有子 Agent 链接。

### Agent Adapter 模式

每个工具实现 `AgentAdapter` trait：
- `find_processes()` — 通过 `sysinfo` 扫描运行进程
- `find_sessions()` — 解析工具特定的会话文件
- `mcp_format()` / `mcp_config_path()` — MCP 配置格式
- `skill_dirs()` — 工具读取 skill 的目录
- `hook_supported()` — 是否支持状态 Hook

添加新工具：实现 `AgentAdapter`，在 `adapter/mod.rs` 的 `get_all_adapters()` 中注册。

**会话扫描预算契约（新工具必读，详见 `src-tauri/src/monitor/session_scan.rs` 模块文档）**——前端每 3 秒轮询 `get_all_sessions`，解析器必须遵守三层预算，否则历史会话堆积后会拖垮 CPU（实机教训：codex 2GB 会话库导致主线程 100%）：

1. **零进程零解析（L1）**：编排层已统一强制（`get_all_sessions` 对空进程列表不调 `find_sessions`）并有防回归测试（`adapter::session_scan_contract_tests`）；实现侧仍应自带 `processes.is_empty()` 早退作纵深防御；
2. **文件解析必须经 `monitor::session_scan::SessionFileScan`（L2）**：`(mtime, size)` 内容摘要缓存；解析函数必须拆成「纯内容产物（进缓存）+ 时间叠加（每次现算）」——`recently_modified` / mtime 停更叠加等依赖当前时间的部分**不能进缓存**；
3. **无界历史目录扫描限 24h 新鲜窗口（L3）**：窗口外文件仅缓存命中时参与匹配，活动进程在窗口内匹配不到才回退全量解析（`fill_uncached`，结果进缓存只付一次代价）；进程界定的有界扫描（目录名 / 索引 / 心跳直达）只需 L1+L2，**不要**窗口化（会丢空闲超窗的活跃卡）。
4. **预过滤反模式禁令（2026-09-14 用户裁决，全工具统一适用）**：窗口判定必须发生在**打开/解压文件之前**——先 `stat` 代际文件 mtime，超窗且无缓存命中且无活动进程回退需求 → **不解析**（文件直接不读）。禁止"先全量解析、后按窗口过滤"的反模式（M1 dsh 冷启动即此反型，评审 Important 抓获：77 会话 155MB 全量解码后才丢弃超窗项）。新增/修改解析器时，扫描顺序固定为：枚举 → stat 预过滤 → （缓存命中 | fill_uncached 回退）→ 出卡。

   dsh 豁免情形（2026-09-14 用户裁决）：dsh web 宿主无法映射进程→会话，无从实现 fill_uncached 回退——冷启动后超窗的「开回合」会话（红·中断 / 红·等待批准）不上板，接受为已知限制（详见 M1 计划「合并前评审追记」），与 I1 强杀可达性裁决一并跟进。

SQLite 类工具（查询即过滤，如 opencode / zcode）豁免第 2、3 条，仅受 L1 约束。

## 数据目录

应用数据存储在 `~/.tuvis/`：

| 路径 | 用途 |
|------|------|
| `~/.tuvis/tuvis.db` | SQLite 数据库 |
| `~/.tuvis/skills/` | 全局 Skill 仓库 |
| `~/.tuvis/mcp/` | 全局 MCP 服务器配置 |
| `~/.tuvis/plugins/` | 全局 Plugin 仓库 |
| `~/.tuvis/hooks/status-hook.sh` | 共享 Hook 脚本 |
| `~/.tuvis/stash/` | 预设独占模式的原生技能暂存区（应用时移入、恢复时回移，stash_journal 记账） |
| `~/.tuvis/events/` | Hook 事件文件（自动清理，30 秒 TTL） |

## 贡献者行为准则

### 我们的承诺

为了营造一个开放和友好的环境，我们作为贡献者和维护者承诺：无论年龄、体型、残疾、种族、性别特征、性别认同和表达、经验水平、教育程度、社会经济地位、国籍、个人外貌、种族、宗教或性取向如何，参与我们项目的每个人都将受到尊重。

### 我们的标准

有助于创造积极环境的行为包括：

- 使用友好和包容的语言
- 尊重不同的观点和经验
- 优雅地接受建设性批评
- 关注对社区最有利的事情
- 对其他社区成员表示同理心

不可接受的行为包括：

- 使用性化语言或图像，以及不受欢迎的性关注或性挑逗
- 挑衅、侮辱/贬损性评论，以及个人或政治攻击
- 公开或私下的骚扰
- 未经明确许可发布他人的私人信息，如物理或电子地址
- 其他在专业环境中被合理认为不恰当的行为

### 我们的责任

项目维护者有责任明确可接受行为的标准，并针对任何不可接受行为采取适当和公平的纠正措施。

项目维护者有权利和责任删除、编辑或拒绝不符合本行为准则的评论、提交、代码、wiki 编辑、issue 和其他贡献，或暂时或永久禁止任何贡献者的其他行为被认为不适当、威胁、冒犯或有害。

### 适用范围

本行为准则适用于所有项目空间，也适用于个人在公共空间代表项目或其社区时。

### 执行

可以通过 GitHub Issue 向项目团队报告辱骂、骚扰或其他不可接受的行为。所有投诉都将被审查和调查，并将根据情况作出必要和适当的回应。

### 来源

本行为准则改编自 [Contributor Covenant](https://www.contributor-covenant.org)，版本 1.4。
