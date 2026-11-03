# 实现说明 — 监控解析层解耦 + Kimi Code 全链路接入

日期：2026-09-01 ｜ 分支：task3/finch

## 0. 交付物与验证结果总览

| 项 | 命令 | 结果 |
|---|---|---|
| macOS 编译 | `cd src-tauri && cargo check` | ✅ 通过（基线为 E0599 编译失败） |
| Rust lint | `cargo clippy --all-targets -- -D warnings` | ✅ 0 warning |
| Rust 测试 | `cargo test`（连跑 3 次） | ✅ lib 85 + dao 4 + linker 7，全绿且稳定 |
| 前端 lint | `pnpm lint` | ✅ 0 error（2 个 warning 为 ExtensionList.tsx 既有） |
| 前端格式 | `pnpm format:check` | ✅ |
| 前端构建 | `pnpm build` | ✅ |
| 前端测试 | `pnpm test` | ✅ 3 文件 11 测试 |
| i18n 键对齐 | `pnpm check:i18n` | ✅ 272 键 |

Linux 影响：macOS 修复所用 `macos-private-api` feature 在 tauri 内部由
`cfg(target_os = "macos")` 门控，非 macOS 为空操作；CI（ubuntu-latest）的
`cargo check/clippy/test` 路径不受影响。本机为 macOS（Darwin 25.6.0），
Linux 侧仅能以"feature 为空操作 + CI 同命令集"论证，未实机验证。

提交序列（语义化小步）：

```
b10dbb4 test(kimi): MCP write/remove roundtrip through the registry
7fbe08d feat(kimi): frontend integration — badge, icon, tool lists
2080bb8 feat(kimi): backend integration — adapter, session parser, process discovery
e24e9df refactor(adapter): central tool registry, dispatch service layer through it
d7d9d1c refactor(monitor): split monolithic parser.rs into per-tool parsers + shared infra
a34c3ae fix(macos): enable macOS-private-api for transparent windows + platform-aware cwd test
```

---

## 1. 前置门：macOS 编译修复

**现象**：基线 `cargo check` 在 macOS 失败——
`WebviewWindowBuilder::transparent()`（src-tauri/src/commands/notification.rs:97，
通知浮窗）在 Tauri v2/macOS 下由 `macos-private-api` feature 门控。
CI 只跑 Linux（`.github/workflows/ci.yml` backend job = ubuntu-latest），故长期未暴露。

**方案选择**（三选一并记录理由）：

1. ~~条件编译跳过 `.transparent(true)`~~ —— 通知浮窗丢失透明圆角，macOS 视觉行为回退，否决。
2. ~~仅 tauri.conf.json 加 `macOSPrivateApi: true`~~ —— tauri-build 报
   "features does not match the allowlist... or add the `macos-private-api` feature"，
   二者必须成对，否决。
3. **成对启用**（采用）：`tauri.conf.json` `app.macOSPrivateApi: true` +
   Cargo.toml `tauri` features 加 `macos-private-api`。主窗 `transparent: true`
   （tauri.conf.json 既有）同样依赖此 feature，一处修复两处受益。

**顺带修复的既有 macOS 测试失败**（基线 `cargo test` 即红，非本次引入）：
`cwd_equivalent_tests::separator_direction_and_trailing_are_equivalent` 无条件断言
Windows 专有的大小写不敏感行为（`cwd_equivalent("e:/llmproject/x", "E:\\LLMproject\\x\\")`），
在 macOS/Linux 必挂。按同文件兄弟用例 `case_rules_follow_platform` 的既有模式，
将该断言用 `cfg!(windows)` 平台化（Windows 行为不变）。

---

## 2. 监控解析层解耦

### 2.1 问题

`monitor/parser.rs`（约 37KB）同时混杂：Claude Code 与 Codex CLI 两套 JSONL
协议解析、GitHub URL 缓存、路径编解码、cwd 归一化、子 agent 计数等公共设施。
新工具接入被迫改这个热点文件，Claude/Codex 解析逻辑互相牵连。

### 2.2 拆分方案

`monitor/` 下按职责单一原则拆为 9 个模块（`parser.rs` 删除）：

| 模块 | 职责 | 使用方 |
|---|---|---|
| `cwd.rs` | cwd 归一化/等价比较（跨工具共享） | 全部 4+1 解析器 |
| `path_codec.rs` | Claude projects 目录名编解码（Claude 专用） | claude_parser |
| `git.rs` | GitHub URL 查询 + 进程内缓存 | claude/codex/kimi |
| `project.rs` | 项目名提取、cwd 形态校验 | claude/codex/kimi/jsonl |
| `jsonl.rs` | JSONL 尾部读取、cwd 提取、子 agent 计数、文件枚举 | claude/codex/kimi |
| `claude_parser.rs` | Claude message.role+content[] 协议 | — |
| `codex_parser.rs` | Codex type+payload 协议（rollout 文件） | — |
| `kimi_parser.rs` | Kimi session_index + wire 事件流（本次新增） | — |
| `opencode_parser.rs` / `openclaw_parser.rs` | 既有，仅改 import 指向 `cwd.rs` | — |

**测试零改动**：`path_tests`/`git_url_tests`/`normalize_cwd_tests`/
`cwd_equivalent_tests` 四组既有测试**逐字节随代码搬入对应模块**（断言一字未改），
`cargo test --lib` 71 → 71 全绿（其中 1 个平台断言按 §1 平台化）。

**一处行为保持的去重**：Claude/Codex 解析器尾部读取逻辑（512KB seek + 跳截断行 +
取末 500 行）原本逐字重复两份，提取为 `jsonl::read_recent_lines(path, max_lines)`
（按文件序返回，调用方 `.iter().rev()` 即得原"最新在前"遍历序）。语义等价性：
空文件/缺失文件/不足 500 行/超大文件四种边界与原实现逐一对照一致；
解析函数无直接单测（既有覆盖为活机扫描 `test_get_all_sessions`），
风险由等价性分析 + 活机扫描（见 §5）兜底。

### 2.3 中央工具登记表（adapter registry）

解耦的另一半：服务层原有 **8 处** `match tool_id { "claude" => …, "codex" => …,
"opencode" => …, "openclaw" => …, _ => … }`（mcp/skill/plugin×4/preset×2/
resource×3/detector），每加一个工具要逐个加 arm。收敛为 `adapter/mod.rs` 的
唯一登记处：

```rust
pub const TOOL_IDS: &[&str] = &["claude", "codex", "opencode", "openclaw", "kimi"];
pub fn adapter_by_id(tool_id: &str) -> Option<Box<dyn AgentAdapter>> { /* 唯一 match */ }
pub fn all_adapters() -> Vec<Box<dyn AgentAdapter>> { TOOL_IDS … filter_map(adapter_by_id) }
pub fn all_adapters_with_ids() -> Vec<(&'static str, Box<dyn AgentAdapter>)> { … }
```

8 处分发改走 registry（映射关系、迭代顺序逐点对照不变）；
`services/resource` 的 skill/plugin 扫描源改为由 adapter 派生
（值与原硬编码逐工具一致，新工具登记后自动纳入）。

**顺带修复 2 个既有 flaky 集成测试**（基线 `cargo test` 间歇红，已用
`git stash` 在未改代码的 d7d9d1c 上复现）：`linker_test.rs` 三个用例在
`Once` 共享的临时 HOME 里共用 `demo-skill` 夹具名/目录，并行执行下互相竞速
（"已存在同名资源"/内容被覆盖）。各自改用独立夹具名，纯测试改动，
生产代码零改动，修复后连跑 5 次全绿。

### 2.4 新旧行为对照证据

- 既有测试：71 个 lib 测试（含 4 组解析路径测试）断言零改动，全绿。
- 活机扫描：`test_get_all_sessions`（真实扫描本机）改动前后均为
  `Total: 0, Waiting: 0`（本机无 agent 进程运行），无回归。
- 顺序保持：`all_adapters()` 按 `TOOL_IDS` 序（= 原 vec 序），
  `get_all_sessions` 的会话聚合顺序不变。
- 前端：`pnpm build` + 11 测试全绿。

---

## 3. Kimi Code 接入决策记录

技术特征先经官方文档（moonshotai.github.io/kimi-code）核实，再与本机实机
`~/.kimi-code` 交叉验证（目录结构、session_index 字段名、wire 事件类型普查）。

### 3.1 数据布局（文档 + 实机一致）

```
$KIMI_CODE_HOME（默认 ~/.kimi-code，env 可重定向；早期版本 ~/.kimi 作回退）
├── config.toml              # TOML（providers/models/hooks…）
├── mcp.json                 # JSON，mcpServers 段（MCP 配置在这，不在 config.toml）
├── skills/                  # 用户级 skill 目录
├── plugins/managed/<id>/    # 插件（kimi.plugin.json 清单型）
├── session_index.jsonl      # 每行 {sessionId, sessionDir, workDir}
└── sessions/<workDirKey>/<sessionId>/
    ├── state.json           # {title, isCustomTitle, createdAt, updatedAt}
    └── agents/main/wire.jsonl   # 事件流（type 判别，time=epoch 毫秒）
```

### 3.2 会话识别方式

- **定位**：以 `session_index.jsonl` 的 `workDir` 与进程 cwd 归一化匹配
  （`monitor::cwd` 共享件）。`workDirKey = wd_<slug>_<sha256[:12]>` 不可逆算，
  索引是唯一稳定映射；索引缺失/为空 → 返回空（优雅降级，不误报）。
  `sessionDir` 实测为绝对路径，解析器同时兼容相对 sessions/ 与相对数据根。
- **一进程一卡**：同 workDir 多会话时取 wire.jsonl mtime 最新者
  （与 Codex Phase 1 同策略）；无有效 cwd 的进程回退最新未匹配会话（Phase 2）。
- **进程发现**：`find_processes_by_names(&["kimi"], …)`。Web Worker 子进程
  `kimi-code-worker` basename 不同名不会误匹配，且父链过滤会剔除同工具子进程。
- **标题**：优先 `state.json.title`（用户自定义/会话摘要，实测优于其他工具的
  8 位 id 前缀），回退 id 前 8 位保持卡片一致性。
- **跳转**：Kimi 为终端 TUI（CLI 形态），macOS 走既有 TTY 链路
  （iterm/Terminal.app/tmux，不依赖 agent_type），`jump_supported=true`；
  Windows 侧补 `TOOL_CLAIM_KEYWORDS`("kimi") 与 `running_projects` 面板反推。

### 3.3 状态判定规则（扫 wire.jsonl 尾部，取最新一条有效信号）

| wire 事件 | 状态 | 语义 |
|---|---|---|
| `context.append_loop_event` tool.call / step.begin | Processing（黄） | 工具执行中 / LLM 步进中 |
| `content.part`（part.type=text） | Processing（黄） | 回复流式输出中 |
| `content.part`（part.type=think） | Thinking（黄） | 模型思考中 |
| `step.end`（finishReason=tool_use） | Processing（黄） | 本步以工具调用结束，后续还有步骤 |
| `step.end`（end_turn）/ tool.result | （继续前扫） | 轮次边界/工具刚返回，非终态 |
| `turn.prompt` / `turn.steer` | Thinking（黄） | 用户刚提交输入 |
| `llm.request` / `permission.record_approval_result` / `goal.create`/`goal.update` | Processing（黄） | LLM 请求在飞 / 权限放行后续跑 / 目标模式轮次 |
| `usage.record` | Waiting（红） | 一轮结束（每轮最后一个事件），等待用户输入 |
| `turn.cancel` | Waiting（红） | 用户打断，回到输入态 |
| `context.append_message`（role=user） | Thinking（黄） | 用户消息 |
| `context.append_message`（role=assistant 纯文本） | Waiting（红） | 纯文本回复，轮次结束 |
| `context.append_message`（role=assistant 带 toolCalls） | Processing（黄） | 工具将执行 |
| `full_compaction.begin`（未见 complete/cancel） | Compacting（黄） | 压缩进行中 |
| 无任何信号 | 文件 60s 内有改动 → Processing，否则 Waiting | 与既有工具 file-age 兜底同语义 |

规则依据：本机 wire.jsonl 事件类型普查（`context.append_loop_event` 9176 条、
`usage.record` 1832、`llm.request` 621、`context.append_message` 361、
`turn.prompt` 190 等）+ 单会话样本结构（step.begin/content.part/step.end/
tool.call/tool.result/usage.record 的时序）。实测样本尾部
`step.end(end_turn) → usage.record` 即"轮次结束"形态，判 Waiting 正确。

### 3.4 MCP 下发格式（与任务书的偏差及依据）

任务书假设"按 TOML 格式下发到 ~/.kimi-code"，**官方文档核实结果不同**：
MCP 服务器声明在 `~/.kimi-code/mcp.json`（JSON，`mcpServers` 段，与 Claude 同构），
`config.toml` 仅放 `[mcp]` 超时等全局项。故 KimiAdapter：
`mcp_format() → Json`、`mcp_config_path() → ~/.kimi-code/mcp.json`，
复用既有 JSON writer（`mcpServers.<name>.{command,args,env}`），
零新增写入路径。决策依据：customization/mcp 官方文档原文
"MCP server declarations are configured in ~/.kimi-code/mcp.json … not in config.toml"。

### 3.5 其他决策

- **Hook**：`hook_supported() = false`。Kimi 支持 `[[hooks]]`（config.toml，
  PascalCase 事件，stdin JSON 含 hook_event_name/session_id/cwd），但现有注册器
  只写 Claude 风格 JSON 配置；TOML `[[hooks]]` 注册器为后续扩展。状态判定由
  wire.jsonl 尾部解析承担（与 opencode/openclaw 同档），不影响验收三要素。
- **Skill**：`skill_dirs() = [$KIMI_CODE_HOME/skills]`（官方用户级 skill 目录，
  SKILL.md frontmatter 与既有扫描器兼容）；`skill_dir_for_tool("kimi")` 经
  `kimi_home()` 保持 KIMI_CODE_HOME 与 adapter 同源。
- **Plugin**：`plugin_dirs() = [plugins/managed]`（官方安装位置），
  `plugin_config_paths() = []`（manifest 目录型，非配置段型）。
- **无新增运行时依赖**：Rust 侧仅用既有 serde/toml_edit/chrono/once_cell；
  前端零新依赖（图标为内联 SVG）。

### 3.6 优雅降级（无 Kimi 环境的机器）

- `~/.kimi-code` 不存在 → 进程发现空匹配、`detect_tools` 显示不可用、
  资源页显示空目录，均不报错（单测 `no_kimi_home_means_no_sessions` 等覆盖）。
- 索引/ wire 损坏行跳过（`malformed_index_lines_are_skipped`）。
- 前端 `ToolIcon` 对未知 toolId 回退 claude 图标；`AGENT_BADGE` 已含 kimi，
  SessionCard/通知浮窗/历史面板不会因缺 badge 崩溃。

---

## 4. "新增第六个工具"改动面（解耦可证明）

后端（以新增 Kimi 为实例）：

| 文件 | 改动 |
|---|---|
| `src-tauri/src/adapter/xxx.rs` | **新增** adapter 文件（~70 行） |
| `src-tauri/src/adapter/mod.rs` | 3 行：`pub mod xxx;` + `TOOL_IDS` + `adapter_by_id` 各 1 行 |
| `src-tauri/src/session/model.rs` | 1 行：`AgentType::Xxx` |
| `src-tauri/src/monitor/xxx_parser.rs` | **新增** 解析器（含单测） |
| `src-tauri/src/monitor/mod.rs` | 1 行：`pub mod xxx_parser;` |
| `src-tauri/src/monitor/process.rs` | ~3 行：`find_xxx_processes` |
| `src-tauri/src/commands/session.rs` | 1 行：Windows `running_projects` arm |
| `src-tauri/src/window/win32.rs` | 1 行：`TOOL_CLAIM_KEYWORDS`（Windows 跳转认领） |

**不需要动**：其他工具的 parser（claude/codex/opencode/openclaw）、
services 全部 match 站点（registry 分发）、linker、database。

前端：`constants.ts`、`types/session.ts`、`ToolIcon.tsx`、`BrandIcons.tsx`、
`agentBadge.tsx`、`audio.ts`、`settings.tsx`、4 个资源/预设视图、
`tauri-mock.ts`（各 1 行）+ 可选 badge 测试。

对比解耦前：加 Kimi 需改 parser.rs 热点 + 8 处服务层 match + 上述全部，
且 parser.rs 改动会牵连 Claude/Codex 解析。

---

## 5. Kimi 手动验收记录

### 5.1 已自动验证（本机，macOS）

**① 解析全链路（真实数据 + 伪造进程）**——临时集成测试（验证后已删除），
读取本机真实 `~/.kimi-code/session_index.jsonl`，伪造同 cwd 进程：

```
workDir: /Users/jarvis
parsed 1 session(s)
  id=session_9671e285-… title=Some("最新版本的 openclaw 我要怎么改它的模型？在哪个文件里改？")
  status=Waiting project=jarvis last_activity=2026-07-14T18:14:03.863Z jump=true role=Some("assistant")
test live_kimi_data_parses_end_to_end ... ok
```

真实 state.json 标题、wire 时间戳（epoch 毫秒 → ISO）、Waiting 状态
（尾部为 usage.record）均正确。

**② MCP 映射全链路**——`mcp_write_and_remove_roundtrip` 单测：
`write_mcp("kimi","demo",…)` → `~/.kimi-code/mcp.json` 出现
`mcpServers.demo.{command,args}`；`remove_mcp` 后清除。✅

**③ 状态判定**——14 个 kimi 单测覆盖：turn.prompt→Thinking、
usage.record→Waiting、tool.call/step.begin→Processing、think part→Thinking、
step.end(tool_use)→Processing、turn.cancel→Waiting、full_compaction.begin→
Compacting、空 wire 兜底、cwd 不匹配不出卡、损坏索引容忍。✅

**④ 既有工具无回归**——85 lib 测试全绿（71 既有断言零改动）+
活机扫描 `Total: 0` 与基线一致。

### 5.2 建议的人工验收命令（交互环境）

```bash
# ① 看板状态色：启动 kimi 会话后
pnpm tauri:dev    # 看板应出现 Kimi Code 卡片：
                  # 提问后→黄(Thinking/Processing)，回答完→红(Waiting)
# ② 点击跳转：点击卡片 → 聚焦对应终端（TTY 链路）
# ③ MCP 映射：资源页 → MCP → 选一个服务器 → 启用给 Kimi Code
cat ~/.kimi-code/mcp.json   # 应出现 mcpServers.<name>
```

交互截图：未在本环境采集（无显示会话交互），以 5.1 的命令输出为准。

---

## 6. 已知限制 / 后续工作

1. **Kimi hook 注册**：TOML `[[hooks]]` 注册器未实现（`hook_supported=false`），
   状态实时性依赖 3s 轮询 + wire 尾部解析；需要时在 `monitor/hooks.rs` 增加
   TOML writer 并置 true（stdin 载荷字段已确认兼容现有 status-hook.sh）。
2. **权限等待态**：Kimi 等待用户批准工具调用时，wire 尾部通常是触发审批的
   tool.call/step → 显示 Processing（黄）。Claude 对 AskUserQuestion 有专门
   Waiting 映射；Kimi 的权限请求事件类型未在文档中公开，未做特判。
3. **子 agent 计数**：Kimi swarm 子 agent 未计数（`active_subagent_count=0`）。
4. **Linux 实机**：仅 macOS 实机验证；Linux 论证依据为 feature 空操作 +
   CI 同命令集，未实机跑过。

---

# 实现说明 — ZCode 全链路接入（第七工具）

日期：2026-09-08 ｜ 分支：feat/zcode-integration ｜ 探测基线：ZCode v3.11.x（任务书双平台实测事实直接采信）

## Z0. 交付物与验证结果总览

| 项 | 命令 | 结果 |
|---|---|---|
| Rust 格式 | `cd src-tauri && cargo fmt --check` | ✅ 0 diff |
| Rust lint | `cargo clippy --all-targets -- -D warnings` | ✅ 0 warning（含基线红的一处顺带修复，见 Z1.8 表末行） |
| Rust 测试 | `cargo test` | ✅ lib 379（基线 312 + 新增 67，含后代 fork 过滤与 MCP 删除对称性两个加固用例）+ dao 4 + linker 7，全绿；既有断言零改动 |
| Windows 交叉 | `cargo check --target x86_64-pc-windows-gnu` | ✅ Finished（本机工具链） |
| 前端检查 | `pnpm check`（format:check + lint + check:i18n + build） | ✅ i18n 497 键对齐 |
| 前端测试 | `pnpm test` | ✅ 46 文件 228 测试（基线 44 文件 224 测试 + 新增 2 文件 4 用例，既有测试文件零改动） |
| 数据安全 | — | 本轮**新增**的 67 lib + 4 前端用例全部 tempdir fixture，零真实路径访问（Z4.2-9 附 grep 证据）；既有基线活机扫描测试 `test_get_all_sessions`（先于本轮存在、本文件历史内容保持原样不改）在 ZCode 运行时按产品路径**只读**打开真实 `~/.zcode` 并写 兔维斯 自身 `~/.tuvis/tuvis.db`——该 DB 写入为基线既有行为（六工具同机制），缓解手段（宿主门控 + `TUVIS_HOME` 重定向，已实测）见 Z4.2-9 |

提交序列（整合分支按功能模块重组为语义化提交，实际序列以 git log 为准）：

```
feat(monitor): D5 后代活跃度仲裁共享核
feat(zcode): 监控链路——宿主判定 / 双库发现 / 状态推导
feat(zcode): 跳转与未读——工作区深链两级链
feat(resources): MCP 段导航读写 + 声明式子树 + 在案债清理
feat(ui): 前端注册面（徽标/图标/清单/mock/词条）
docs(zcode): 实现说明与 GUI 验收清单
```

## Z1. 架构决策与理由

### Z1.1 会话真相源 = 数据库，进程侧只回答「应用开没开」（任务事实 2）

ZCode 进程数量与任务数无关（app-server 池化、子代理与主会话共享进程），任何
「进程 ↔ 会话」绑定都不成立。因此：

- `find_processes` 只做**宿主判定**：枚举进程表，exe basename 严格等于
  `zcode`/`zcode.exe` 且命令行非「`--type=`（Electron 辅助）/ 含 `zcode.cjs`
  （会话运行时）」→ 单个 App 形态 AgentProcess。macOS 上 `ZCode Helper` /
  `zcode-cli` / `zcode-host-local-1` / `zcode-node-repl-mcp` basename 均不
  严格等于 zcode，天然排除；Windows 全部可执行体同名 `ZCode.exe`，**命令行是
  唯一判据**（主进程 = 裸命令行）。cmd 读不到（提权进程）按 exe 放行——漏判
  宿主会清空全部卡片，代价高于误判（与 workbuddy sidecar 排除的防御方向一致）。
- `find_sessions` 完全由数据库聚合（`get_zcode_sessions` → `build_sessions`），
  卡片 pid/cpu 挂宿主进程。宿主是全部卡片的总开关与清理触发器：宿主不在场 →
  活跃卡为空；未读池由既有 `dead_tools_from_pool` → `clear_tool` 管线清理
  （host.rs 加了 zcode 分支，Windows 命令行门在 `tool_host_alive_in` 生效）。
- Windows 关窗驻留托盘、进程仍活 → 宿主判定以进程存活为准，与窗口可见性解耦
  （实测事实直接采信，无需特判）。

### Z1.2 每会话一卡（D1）与 24h 窗口

`session` 表 `task_type='interactive'` + `time_updated >= now-24h` +
`time_updated` 倒序 LIMIT 100（有界防御）。过滤链：

1. `task_type` 只收 interactive（subagent_child / selection_side_chat / fork 过滤）；
2. `parent_id` 非空双保险过滤（私有格式升级可能改 task_type 取值，parent_id 语义更稳定）；
3. 会话 id 严格校验 `sess_` + 8-4-4-4-12 hex UUID（复用 workbuddy 的
   `is_strict_uuid_form`；子代理 id `sess_subagent_agent_<uuid>` 不满足该形态，
   天然被拒）——不合规一律跳过该会话，不 panic、不影响其他会话/工具；
4. tasks 索引 `archived`/`deleted` 真值过滤（INTEGER/TEXT/BOOL 多形态防御读取；
   索引缺行视为未归档未删除——索引是加速源不是真相源）；
5. 单行类型不符（time_updated 非整型等）→ 行级跳过。

`unread_at`/`last_unread_at`（ZCode 侧栏自己的未读标记）不使用——与 兔维斯 未读池
语义不同步（任务事实 1）。

### Z1.3 状态推导：消息流尾部按 sequence 倒扫（任务事实 4 的翻译）

复用共享判定核 `monitor::app_status`（D5 反向约束：ZCode 特有逻辑不进共享层，
ZCode 只写一个「格式翻译适配器」`flatten_entries`/`part_entry_kind`）：

| ZCode 形态 | AppEntryKind | 状态 |
|---|---|---|
| message role=user + semantics.kind=user_prompt（或 kind 缺失） | UserMessage | Thinking（用户刚发话） |
| message role=user + kind=todo_reminder / timeline_event | **整条跳过** | —（记账消息，继续倒扫） |
| part type=tool | ToolCall | Processing（调用已发出、结果未回） |
| part type=step-start | TurnStart | Processing |
| part type=step-finish + reason=tool-calls | ToolCall | Processing（本步以工具调用收尾、后续还有动作） |
| part type=step-finish（无 reason） | TurnEnd | Idle（回合收尾） |
| part type=text | AssistantMessage | Idle（助手正文收尾） |
| part type=reasoning / timeline / file / compaction / 未知 | Other | —（跳过） |

**懒落库处理**：回合收尾的 step-finish 在流关闭时才写入，时间戳不可靠、顺序可靠
（永远排在所属消息尾部）——消息与 parts 一律按 `sequence` 列排序（列缺失降级
`time_created`/`rowid`，两列皆无 → 该会话跳过），**绝不按时间戳**；测试
锁定于两个**判别性 fixture**（自检轮强化，反例构造 + 变异证明）：
① `lazy_step_finish_ordered_by_sequence_not_timestamp`——双消息 time_created 序
与 sequence 序相反（懒补写的上一轮消息时间戳最新、用户续问消息 sequence 在最后），
断言 Thinking；把实现临时改为「按 time_created 排序」后该用例实测 FAILED
（`left: Idle, right: Thinking`），错误实现无法通过；
② `parts_ordered_by_sequence_not_physical_row_order`——parts 物理行序（rowid）
与 sequence 序相反插入，断言 Idle；把实现临时改为「按 rowid 排序」后实测 FAILED
（`left: Processing, right: Idle`）。两处变异均已还原，还原后全绿。
成批落库（响应完成时一次写入、思考中无中间写入）意味着尾部即最新已完成批次，
倒扫第一条有语义条目定状态与既有工具口径一致。

**停更与仲裁**：`session.time_updated`（活动期间实时刷新）为停更时钟，走共享层
`overlay_stale_with_descendants`（见 Z1.4）。无语义条目兜底：time_updated 新鲜
（<300s）→ Processing，停更 → Waiting（与 Codex/WorkBuddy 兜底同档）。

**task_status 只作提示不作主源**（任务事实 3：仅 Windows 可靠，macOS 旧任务续跑
不翻回 running）：只有 `error` 参与判定（→ Finished 转绿，双平台采信）；
`running`/`completed` 一律不覆盖尾部推导（macOS 恒旧值也不出错），测试双向锁定
（`stale_completed_hint_does_not_override_running_tail` /
`running_hint_does_not_override_idle_tail`）。
**error 同样不得压制其后的新活动**（自检轮修复）：macOS 上 error 是恒旧值，
用户续聊后若仍强制 Finished 就把活的会话钉死成绿卡。规则：`tasks.updated_at`
为 error 记录写入时刻，`session.time_updated`（实时刷新）严格越过它 = error
之后又有新活动 → 提示已被取代，以尾部推导为准；相等/更早/字段缺失（无过时
证据）→ error 照常生效（失败完成宁可提醒）。纯函数 `error_hint_superseded`
锁定边界，fixture 测试 `error_hint_does_not_suppress_newer_activity`（error
1 分钟前 + 用户刚续聊 → Thinking 非 Finished）锁定端到端。

### Z1.4 D5 通用化：后代活跃度仲裁修在共享层

共享核新增 `DescendantActivity { Active, Stale, Absent }` 与
`overlay_stale_with_descendants(status, age, descendants)`：Processing 且停更
≥300s 时，后代活跃 → 保持 Processing（主会话静默 = 健康等待子代理），后代停更/
无后代 → Waiting（疑似卡住）。**既有工具零变化的证明**：`overlay_mtime_stale`
改为委托 `Absent` 分支，回归测试 `absent_descendants_matches_legacy_overlay_exactly`
对 6 状态 × 6 年龄边界逐点断言新旧函数输出一致（改前改后语义一致的测试证据）。
ZCode 侧数据源：`session.parent_id` 分组聚合（COUNT + MAX(time_updated)），
纯函数 `descendant_activity` 判定，四类情形（有后代活跃 / 后代也停更 / 无后代 /
非运行态）各有用例。子代理会话自身不出卡（task_type + parent_id 双过滤），
活跃子代理计数进 `active_subagent_count`（30s 活跃口径与 Claude 对齐）。
后代聚合在 task_type 列存在时**仅统计 `subagent_child`**：fork 等显式他类的
子会话可能带相同 parent_id 且持续写入，但不是子代理，不得豁免主会话的真卡死
（`active_fork_child_does_not_exempt_real_stall` 锁定）；列缺失或行值 NULL/空串
时不过滤（私有格式演进防御，宁可多豁免不可误报卡死）。

### Z1.5 提醒与未读（D2）：复用 W4 管线 + P1-3 门通用化

- 转绿（Idle/Finished，含 error→Finished）→ 既有 `sync_unread_sessions`
  迁移触发插行，跨 兔维斯 重启保留（DB 持久）；已读信号三类不变（跳转成功且前台
  验证通过 / 手动 X / 24h 过期）；宿主退出 → `dead_tools_from_pool` 清池 +
  `filter_host_dead_cards` 清活跃卡（host.rs zcode 分支供判）。
- ZCode 卡片是**数据驱动持久绿卡**（time_updated 24h 窗口内完成态持续出卡，
  与 Codex rollout 聚合卡同族；区别于 WorkBuddy 进程驱动卡）——P1-3 的
  「池行存在 ⇒ 在板呈未读态；已读/过期删行 ⇒ 剔除绿卡」门从 `matches!(Codex)`
  特判泛化为 `green_card_is_data_driven`（Codex | ZCode），判定核心
  `codex_green_card_should_drop` 与既有测试零改动，新增门测试锁定
  WorkBuddy/CLI 工具不在门内（既有行为零变化）。
- ZCode 无「心跳消失竞态」：会话行持久存在，完成转绿必然被轮询观测，无需
  WorkBuddy 式补偿管线。

### Z1.6 跳转：直接聚焦唯一窗口（深链已移出）

初版曾接入工作区深链（`zcode://workspace/open?path=<URL编码路径>`，会话级深链
不存在的前提下取工作区级精度上限）。**2026-09-09 Windows 实机验收推翻该设计，
深链整体移出跳转链**，依据（ZCode 应用日志 36 条 `[deep-link]` 事件 + 会话库
+ 窗口枚举取证）：

- **每次派发无条件弹信任确认**（"打开外部 ZCode 链接？…项目设置可能影响 agent
  runtime"），当前前台已开着的工作区也不例外（13:12:07 两条「用户取消」日志），
  `~/.zcode` 全量状态无信任白名单可配；
- **落点语义 = 打开工作区 + 全新会话 composer**（路由后 `conversation store
  connect generation:1`，十几次跳转零新会话行——非定位已有会话，用户须重新
  起会话），跳转目标落空；
- **每次派发拉起一个 ZCode.exe 转发进程**（日志 13:12–13:30 十余个新 pid 各打
  一条「注册协议成功」后退出），跳转重且慢；
- **窗口形态实机不符**：单窗口多标签（`windowId:1` 恒定），标题恒为 `"ZCode"`
  不含工作区名——原「多窗口按工作区名消歧」命题不存在，聚焦唯一窗口零歧义。

现行跳转链（无深链）：

- Windows：`focus_session` → `resolve_and_focus` 的 **pid 单窗口路径**直接聚焦
  （卡片携带宿主 pid，ZCode 宿主恰只有一个可见窗口 → 零消歧锁定）；失败落
  `reactivate_tool_app`（谓词与 `TOOL_CLAIM_KEYWORDS` 均含 zcode，按标题
  "ZCode" 认领唯一窗口）。ZCode 关闭 ⇒ 宿主不存活 ⇒ 无卡片 ⇒ 无跳转需求。
- macOS：无深链路径，直接走 bundle 激活（`bundle_matches_agent` 含 zcode.app），
  与其他 App 形态工具同口径。
- 保留项：`open_url` 的 **ShellExecuteW 首选派发**（cmd `%` 展开破坏编码 URL 的
  修复，对 WorkBuddy/Codex 深链同样成立）；`zcode_has_no_session_level_url`
  回归锁改为「zcode 无任何深链」。随深链移除：`jump_url` / `workspace_url` /
  `percent_encode` / `workspace_path_for_session` 及其测试。

### Z1.7 资源管理（D3：Skill + MCP，插件不做）

- **Skill（Plan A）**：`skill_dir_for_tool("zcode") = ~/.zcode/skills`（官方
  文档声明的用户级目录；实测尚不存在，`enable_skill_for_tool` 既有
  `create_dir_all` 首启建目录）。启用 = SSOT→Layer2→工具目录建链，禁用 = 删链，
  SSOT 真身保留——linker 三层设施工具无关，零改动复用。不确定性与备选方案见 Z4.1。
- **MCP**：`~/.zcode/cli/config.json`（与 plugins 等顶层键共存）仅 `mcp.servers`
  子树读-改-写。实现为 trait 通用能力 `mcp_json_section() -> &[&str]`（默认
  `["mcpServers"]`，ZCode 声明 `["mcp","servers"]`，OpenCode 显式声明 `["mcp"]`
  与既有 jsonc 写入段同源）——**共享层无 ZCode 特判**。写入导航缺失层逐层补建、
  中间层被非对象值占用 → 拒绝写入（不静默覆盖用户数据）；解析失败 → Err 且
  不落盘；键序与未知键保留（serde_json preserve_order）；条目形态
  `{"command","args","env"}` 与既有一致。`enable:false`（ZCode 自有停用标记，
  无字段 = 启用）：SSOT 扫描不把停用条目计入该工具启用列，`read_mcp_servers`
  原样透传条目（含 enable 字段）如实展示为停用；兔维斯 写入不携带 enable 字段
  （= 启用），不破坏 ZCode 语义。删除路径与写入**对称**：段路径任一层被非对象
  值占用 → 报错且不落盘（不静默吞损坏形态）；段或条目不存在 → 幂等成功且
  **不重写文件**（避免无谓 pretty 化扰动用户主配置的键序与格式，
  `remove_rejects_corrupt_section_and_skips_noop_rewrite` 锁定三种形态）。
- 插件：marketplace 结构，范围外（`plugin_dirs`/`plugin_config_paths` 沿用默认空）。

### Z1.8 已知债清理（随接入一并处理）

| 债 | 位置 | 修法 |
|---|---|---|
| MCP 读取链路硬编码 claude/codex/opencode（openclaw/kimi/workbuddy 今天就落「未知工具」） | `commands/mcp.rs::read_mcp_servers` | 改走 `adapter_by_id` 注册表 + `mcp_json_section` 段导航（历史顶层键探测链保留为兜底） |
| 资源导入溯源硬编码四工具（kimi/workbuddy 来源历史行回溯恒 None → 跳过补链） | `services/resource/mod.rs::detect_source_tool` | 改遍历 `TOOL_IDS` |
| SSOT 扫描 / MCP 导入 / 预设冲突检查的段探测链不识别嵌套子树 | `commands/resource.rs`、`services/preset/mod.rs` | 同走 `mcp_json_section` 导航 |
| 前端 `SUPPORTED_TOOLS` 缺 workbuddy（徽标遍历测试覆盖面缺口） | `src/config/constants.ts` | 补 workbuddy + zcode，与后端 TOOL_IDS 对齐 |
| 浏览器 mock `detect_tools` 缺 workbuddy 行 | `src/tauri-mock.ts` | 补齐（+ zcode 行） |
| `cargo clippy --all-targets` 基线红（windows-only 测试模块的 unused import，CI Linux / 本机 macOS 均触发） | `linker/detector.rs::alias_dir_tests` | import 随用例同门控 `#[cfg(windows)]`，断言零改动 |

自查其余注册链路（前端 mock、测试遍历、i18n）：工具列正规来源是后端
`list_enabled_tools` 下发（W5 已重构），资源双视图/设置页声音区/工具开关均无
硬编码；i18n 无按工具名词条（显示名来自后端 label / agentBadge），无需新增键
（check:i18n 497 键对齐通过）；`running_projects_from_processes`（Windows 面板
反推）为 CLI 终端工具专用清单，WorkBuddy/ZCode（APP 形态、无终端窗口）本就不
参与，维持现状（与 workbuddy 轮口径一致）。

## Z2. 新旧行为对照

| 面 | 改前 | 改后 | 零回归证据 |
|---|---|---|---|
| 停更降级（共享核） | `overlay_mtime_stale`：Processing+停更≥300s → Waiting | 委托 `overlay_stale_with_descendants(…, Absent)`，语义逐点相等 | `absent_descendants_matches_legacy_overlay_exactly`（6 状态 × 6 边界）+ 既有 4 个 overlay 测试断言零改动全绿 |
| P1-3 绿卡门 | `matches!(s.agent_type, Codex)` 特判 | `green_card_is_data_driven(&AgentType)`（Codex|ZCode） | `data_driven_persistent_green_card_tools`（WorkBuddy/CLI 全不在门内）+ 既有 `codex_green_read_tests` 零改动 |
| MCP JSON 写/删 | 硬编码顶层 `mcpServers` | 按 adapter 声明键路径导航（默认仍 `mcpServers`） | `default_section_roundtrip_matches_legacy_shape`（旧形态往返等价）+ kimi 轮 `mcp_write_and_remove_roundtrip` 同型测试全绿 |
| MCP 读取命令 | 三工具硬编码，其余报「未知工具」 | 注册表分发（六→七工具全可达） | 既有测试零改动；新增 `zcode_adapter_declares_nested_section` + `read_mcp_servers_registry_tests`（2 个） |
| 宿主判定 | workbuddy/codex 两分支 | + zcode 分支（exe 严格名 + Windows 命令行门） | 既有 host 测试零改动 + 新增 7 个 zcode 宿主判定用例（host.rs 4 个 + zcode_parser 3 个） |
| 深链 | session_url 两工具 + UUID 门 | zcode 不接入（初版工作区深链 2026-09-09 实机验收后移除，见 Z1.6；ShellExecuteW 首选派发保留） | `zcode_has_no_session_level_url` + 既有 P1-1 注入测试零改动 |
| 前端 | 六工具 | 七工具（badge/icon/types/mock） | 既有 224 测试零改动全绿（既有测试文件零 diff，zcode 用例独立新文件 tests/toolIconZcode.test.tsx + tests/agentBadgeZcode.test.tsx） |

## Z3. 测试证据对照表（行为 → 用例 → 结果）

运行命令与结果（本机 macOS，2026-09-08）：

```
$ cd src-tauri && cargo test --lib
test result: ok. 377 passed; 0 failed; 0 ignored
$ cargo test            # 集成：dao 4 + linker 7 全绿
$ cargo clippy -- -D warnings                 # 0 warning
$ cargo clippy --all-targets -- -D warnings   # 0 warning（基线红一处已修，见 Z1.8）
$ cargo fmt --check                           # 0 diff
$ cargo check --target x86_64-pc-windows-gnu  # Finished
$ pnpm check                                  # format+lint+i18n(497 键)+build ✅
$ pnpm test                                   # 46 files, 228 tests ✅
```

| 行为（任务目标） | 用例（模块::名） | 结果 |
|---|---|---|
| 每会话一卡（不按项目压缩） | zcode_parser::one_card_per_session | ✅ |
| 24h 窗口外不出卡 | zcode_parser::outside_24h_window_no_card | ✅ |
| archived/deleted 不出卡（索引缺行不影响） | zcode_parser::archived_or_deleted_tasks_no_card | ✅ |
| fork/selection_side_chat/subagent_child 过滤 | zcode_parser::only_interactive_sessions_become_cards | ✅ |
| parent_id 双保险过滤 | zcode_parser::subagent_row_with_parent_id_is_filtered_even_if_type_mutated | ✅ |
| 会话 id 不合规跳过、字段缺失/类型不符行级跳过、不 panic 不影响他卡 | zcode_parser::session_id_validation / malformed_session_id_skipped_without_affecting_others | ✅ |
| 库缺失整体降级为空 | zcode_parser::missing_databases_degrade_to_empty | ✅ |
| 用户刚发话 → Thinking | zcode_parser::user_just_sent_message_is_thinking | ✅ |
| 助手正文收尾 → Idle | zcode_parser::assistant_text_tail_is_idle | ✅ |
| 工具调用挂起 → Processing | zcode_parser::pending_tool_call_tail_is_processing | ✅ |
| step-finish(reason=tool-calls) → Processing | zcode_parser::step_finish_with_tool_calls_reason_is_processing | ✅ |
| 记账消息（todo_reminder/timeline_event）跳过继续倒扫 | zcode_parser::bookkeeping_messages_are_skipped_and_scan_continues | ✅ |
| 懒落库：时间戳序与 sequence 序相反（按 sequence 倒扫，变异证明见 Z1.3） | zcode_parser::lazy_step_finish_ordered_by_sequence_not_timestamp | ✅ |
| 成批落库：parts 物理行序与 sequence 序相反（按 sequence 排序，变异证明见 Z1.3） | zcode_parser::parts_ordered_by_sequence_not_physical_row_order | ✅ |
| 子代理长任务全程运行中（主静默 17min + 子活跃） | zcode_parser::subagent_long_task_keeps_main_processing | ✅ |
| 停更 + 后代也停更 → 疑似卡住 | zcode_parser::stale_with_stale_descendants_is_suspected_stuck | ✅ |
| 停更 + 无后代 → 疑似卡住 | zcode_parser::stale_without_descendants_is_suspected_stuck | ✅ |
| 非运行态不受停更/后代影响 | zcode_parser::non_running_tail_ignores_descendant_and_staleness | ✅ |
| 仲裁四类纯判定（含时钟回拨防御） | zcode_parser::descendant_activity_pure_judgment | ✅ |
| task_status=error 按完成转绿 | zcode_parser::task_status_error_turns_green_as_finished | ✅ |
| error 之后用户续聊：提示不得压制尾部推导（macOS 恒旧值） | zcode_parser::error_hint_does_not_suppress_newer_activity / error_hint_superseded_pure_judgment | ✅ |
| task_status 恒旧值不出错（completed/running 不覆盖尾部） | zcode_parser::stale_completed_hint_does_not_override_running_tail / running_hint_does_not_override_idle_tail | ✅ |
| 标题降级链（索引→会话→首条用户消息 60 截断） | zcode_parser::title_degradation_chain | ✅ |
| 卡片字段口径对齐（项目名/路径/摘要/角色/RFC3339/pid 挂宿主/unread 由池管线标） | zcode_parser::card_fields_align_with_other_tools / long_last_message_is_truncated | ✅ |
| Windows 反斜杠 + 大写盘符路径行 | zcode_parser::windows_backslash_uppercase_drive_paths | ✅ |
| 宿主判定（macOS 主进程/非宿主枚举、Windows 命令行门、提权 cmd 缺失防御） | zcode_parser::exe_basename_matching / windows_cmdline_host_gate / host_process_combination + host::zcode_*（4 个） | ✅ |
| D5 空后代语义 = 现状（逐边界） | app_status::absent_descendants_matches_legacy_overlay_exactly | ✅ |
| D5 后代活跃保持运行中 / 后代停更降级 / 非运行态透传 / 阈值内透传 | app_status::active_descendants_keep_processing_when_stale / stale_descendants_downgrade_to_waiting / non_processing_statuses_ignore_descendants / fresh_processing_ignores_descendants | ✅ |
| ~~深链携带项目原生路径（Windows 整段编码）~~ | 随深链移除（2026-09-09 实机验收，见 Z1.6） | 🗑 |
| ~~深链输出无 cmd 元字符（注入面闭合）~~ | 随深链移除（ShellExecuteW 首选派发保留，惠及 WorkBuddy/Codex） | 🗑 |
| zcode 无任何深链（路由不存在的不构造） | deep_link::zcode_has_no_session_level_url | ✅ |
| 跳转 = 聚焦唯一窗口（深链移出后 pid 单窗口路径 + reactivate 兜底） | deep_link::zcode_has_no_session_level_url + 既有 P1-2/P2-1/P2-2 链路测试零改动；原生路径随会话行携带：zcode_parser::windows_backslash_uppercase_drive_paths | ✅ |
| 前台验证通过才标已读（Windows） | 既有 verify_foreground_tool 状态机测试零改动（zcode 走同一链路，白名单加行） | ✅ |
| 已读只消该会话卡、同项目其他未读保留 | 既有 unread DAO delete_single_and_clear_tool + adapter 层 unread_card_tests 零改动（工具无关管线） | ✅ |
| 绿卡在板呈未读态 + 已读剔除（Codex 同款、WorkBuddy 不受影响） | adapter::green_card_gate_tests::data_driven_persistent_green_card_tools / zcode_tool_id_serde_roundtrip + 既有 codex_green_read_tests 零改动 | ✅ |
| macOS bundle 兜底匹配 zcode.app | app_activation::bundle_matches_agent_zcode_rules（独立新用例，既有 bundle_matches_agent_rules 零改动） | ✅ |
| skill 目录 = 官方声明 ~/.zcode/skills（Plan A） | adapter::skill_dir_tests::zcode_skill_dir_uses_official_user_level_directory | ✅ |
| skill 启用=建链 / 禁用=删链 / SSOT 保留 | 既有 linker_test + tool_settings restore 矩阵零改动（linker 工具无关，zcode 经注册表自动纳入） | ✅ |
| MCP 读-改-写只动 mcp.servers 子树、未知键与原键序保留 | mcp::json_section_tests::write_into_nested_section_preserves_unknown_keys_and_order | ✅ |
| 缺失层补建 | mcp::write_creates_missing_intermediate_layers | ✅ |
| 解析失败只报错不落盘 | mcp::parse_failure_errors_without_touching_file | ✅ |
| 中间层被非对象占用拒绝写入 | mcp::non_object_intermediate_layer_is_rejected | ✅ |
| 删除只删目标条目 | mcp::remove_only_target_entry | ✅ |
| 默认段（既有 Json 工具）新旧等价 | mcp::default_section_roundtrip_matches_legacy_shape | ✅ |
| zcode adapter 段声明/格式/路径 | mcp::zcode_adapter_declares_nested_section | ✅ |
| enable:false 如实展示为停用（仅布尔 false 算停用，类型漂移防御） | mcp::entry_disabled_tests::enable_flag_recognized_defensively（扫描侧消费该纯函数；read 原样透传由 GUI 验收项 8 覆盖） | ✅ |
| MCP 读取链路注册表分发（债修复：三工具硬编码 → 七工具可达） | commands::mcp::read_mcp_servers_registry_tests（2 用例，零文件系统访问） | ✅ |
| 资源导入溯源遍历 TOOL_IDS（债修复：kimi/workbuddy/zcode 不再回溯恒 None） | services::resource::detect_source_tool_tests::registry_covers_late_registered_tools | ✅ |
| 前端图标不回退 Claude | tests/toolIconZcode.test.tsx::"ToolIcon zcode"（2 用例，独立新文件） | ✅ |
| 前端徽标显示名/配色/图标渲染（显式断言） | tests/agentBadgeZcode.test.tsx::"agentBadge zcode"（2 用例，独立新文件） | ✅ |
| 前端徽标七工具遍历 | tests/agentBadge.test.tsx（SUPPORTED_TOOLS 补齐后遍历含 workbuddy+zcode，既有断言零改动） | ✅ |

既有测试零改动核对（自检轮规范化后口径）：
- **专属测试文件**：`git diff a77bb21..HEAD -- tests/ src-tauri/tests/` 中
  `src-tauri/tests/`（dao_test/linker_test/support）零 diff；`tests/` 下既有
  文件全部零 diff（自检轮把 zcode 图标用例从 tests/toolIcon.test.tsx 迁出为
  独立新文件 tests/toolIconZcode.test.tsx + tests/agentBadgeZcode.test.tsx），唯一变化是**新增文件**。
- **源码内联测试模块**：app_status 原有 12 例、host 原有 8 例、deep_link 原有
  P1-1/P1-2 例、adapter unread/sort/dedup 系列、app_activation 原有 6 例、
  commands/session 原有例、win32 原有例、resource/preset/linker 原有例——
  全部断言逐字节未动，本轮改动仅为**新增测试函数/新增测试模块**（自检轮把
  曾追加进既有函数 bundle_matches_agent_rules 的 zcode 断言迁出为独立新函数
  bundle_matches_agent_zcode_rules）。
- 唯一既有测试模块的非断言改动：`linker/detector.rs::alias_dir_tests` 的
  `use super::*;` 加 `#[cfg(windows)]` 门控（该模块两个用例本就全部
  `#[cfg(windows)]`）。佐证：基线文件在同机 `cargo clippy --all-targets
  -- -D warnings` 实测报 `error: unused import: super::* -->
  src/linker/detector.rs:732:9`（非 Windows 平台基线即红），修复后 0 warning。
- `tests/agentBadge.test.tsx` 的遍历断言因 SUPPORTED_TOOLS 补齐而覆盖面扩大
  （断言文本未动，文件零 diff）。

## Z4. 已知限制与开放问题

### Z4.1 `~/.zcode/skills/` 是否被 ZCode 真实读取未经实测（任务目标 7）

- **采用 Plan A**：按官方文档声明的用户级目录实现（skill_dirs =
  `~/.zcode/skills`，首启建目录）。理由：唯一有文档依据的每工具独立目录；
  兔维斯 的启用/禁用/还原语义（Layer2 建链删链）在该目录下完整成立。
- **不确定性**：双平台实测该目录尚不存在（首装时创建），ZCode 是否真实读取
  未经确认；本轮按任务约束**未在本机在线实测**。
- **备选方案（不采用）及其处理思路**：跨工具共享目录 `~/.agents/skills/` 已
  实测确认 ZCode 会读取，但该目录 Codex 等工具同读——在其中做「ZCode 专属
  启用/禁用」会同时开关其他工具，与 兔维斯「每工具独立激活」模型直接冲突（禁用
  ZCode 的链接会断掉 Codex 的技能）。若后续实测证伪 Plan A（ZCode 不读
  `~/.zcode/skills`），处理思路：① 首选向 ZCode 官方确认配置项（是否存在
  skill 目录设置或 env 重定向）；② 若只能走共享目录，则把 ZCode 的 skill
  语义降级为「只读展示 + 文档提示」（不做建链删链，避免跨工具副作用），并在
  工具管理页对 ZCode 的 skill 开关给出明确 tooltip；③ 不引入
  「共享目录 + 工具专属子目录」 hack（无文档依据，猜测形态易被升级打破）。
- 若 Plan A 被证伪，监控/跳转/未读/MCP 链路不受影响（互不依赖）。

### Z4.2 其他限制

1. **macOS 深链不标已读**（P1-2 既有口径）：macOS 无前台验证设施，
   `open` 退出码 0 不能证实路由成功——zcode 工作区深链成功也不回标已读，
   bundle/枚举兜底激活照旧标已读。与 Codex/WorkBuddy 行为完全一致（D2 口径）。
2. **跳转精度上限 = 项目工作区**：会话级深链不存在（实测枚举），点击卡片
   只能把 ZCode 切到目标项目工作区，不能定位到具体会话。
3. **message/part 表排序列假设**：按 `sequence` 排序，列缺失降级
   `time_created`；两者皆无（极端 schema 漂移）时 `load_tail_messages` 返回
   None → **整卡跳过**（不出卡、不出错卡，比无语义兜底更保守——Z4.2-3 修正：
   此前文档误写为「按无语义条目兜底仍出卡」，与实现不符；现行为即目标行为）。
4. **task_status 仅 error 参与判定**：Windows 上 running/completed 本可加速，
   但为保证 macOS（恒旧值）双平台同一套语义，一律以尾部推导为主源——加速
   收益（至多一轮轮询间隔）不值得平台分叉。
5. **ZCode 侧栏未读标记不使用**：`unread_at`/`last_unread_at` 与 兔维斯 未读池
   语义不同步（用户在 ZCode 内查看不清 兔维斯 卡，反之亦然）——任务事实直接采信。
6. **子代理仲裁窗口 = 300s**：后代 `time_updated` 距 now <300s 记活跃。子代理
   单次模型请求实测最长 204s、无超 300s 样本，窗口覆盖；若 ZCode 未来出现
   超长单请求，会短暂误判疑似卡住（下轮自愈）。
7. **进程池化下 cpu_usage 语义弱化**：卡片 CPU 挂宿主进程（全部会话共享），
   仅作展示，不参与状态判定（与 Codex APP 聚合卡口径一致）。
8. **歧义假设记录**（禁止等待人工指导，自行合理假设）：① message.data 的
   `semantics.kind` 缺失时按真实用户消息处理（已知记账 kind 已
   显式排除，未知 kind 保守按用户消息 → Thinking，宁黄勿绿）；② tasks 索引
   缺行视为未归档未删除（索引是加速源）；③ part 的 text 字段仅 type=text 消费
   （reasoning 的 text 不作摘要，与「思考过程不入摘要」的既有口径一致）。
9. **测试期数据安全边界（如实披露）**：本轮新增的全部测试仅用 tempdir fixture
   （zcode_parser 测试经 `ZcodeRoots` 注入、MCP 测试经 tempdir 路径、
   skill_dir/detect_source_tool 测试为纯路径字符串比较，零文件系统访问）。
   唯一的真实目录触达来自**既有基线活机扫描测试** `adapter::tests::
   test_get_all_sessions`（Kimi 轮起的仓库惯例，本文件历史内容保持原样不改）：
   它执行产品路径 `get_all_sessions()`，当本机 ZCode 主进程在跑时，
   zcode adapter 会以 `SQLITE_OPEN_READ_ONLY` 打开真实 `~/.zcode` 两库（与
   出货应用每轮轮询完全相同的行为，绝不写入），并把状态缓存/未读池写入 兔维斯
   自身 `~/.tuvis/tuvis.db`（该 DB 写入是基线对全部工具一致的既有行为，非本轮
   引入）。两项已实测的缓解：① **宿主门控**——ZCode 未运行时 `get_zcode_sessions`
   在打开任何数据库之前即返回空（关闭 ZCode 后跑 `cargo test` 则零 `~/.zcode`
   触达）；② **TUVIS_HOME 重定向**——debug 构建尊重 `TUVIS_HOME`
   （connection.rs::app_data_home），`TUVIS_HOME=$(mktemp -d) cargo test` 可使
   `~/.tuvis` 零写入（本轮取证已按此口径执行）。全量跑测试建议采用
   `TUVIS_HOME=$(mktemp -d) cargo test` 或关闭 ZCode 后运行。

## Z5. GUI 手动验收清单（macOS 为主，Windows 项单列）

前置：`pnpm tauri:dev` 启动 兔维斯；本机安装 ZCode 且有历史任务（日常使用环境）。
**注意：验收会读取真实 `~/.zcode/`（只读）与写入 `~/.tuvis/`（兔维斯 自身数据目录，
产品行为）——与开发/测试阶段的「严禁读写」约束不同，属正常运行时语义。**

1. **工具管理**：设置 → 工具管理出现 ZCode 行（蓝紫渐变 Z 图标 + 「已安装」
   badge，`~/.zcode` 有原生内容即判定）；开关切换 → 保存 → 确认弹窗 →
   取消勾选后看板/资源页彻底隐藏 ZCode，重新勾选恢复。
2. **会话出卡**：ZCode 开着且有 24h 内活动任务 → 首页每会话一张卡（标题 =
   ZCode 侧栏同款任务标题；项目名 = 工作区目录名；徽标 ZCode）。归档/删除的
   任务不出卡；超过 24h 无活动的任务不出卡。
3. **状态灯**：在 ZCode 里发一条消息 → 兔维斯 卡变黄（Thinking）；agent 调工具
   期间保持黄（Processing）；回复正文完成 → 转绿（Idle）并弹完成提醒（浮窗/
   系统通知/声音/桌宠气泡按既有通知面策略）。
4. **子代理仲裁**：触发一个长时子代理任务（如「用子代理分析整个仓库」）→
   主会话卡全程保持黄（运行中），即使超过 5 分钟主会话零写入；ZCode 内子代理
   结束、主会话正文收尾 → 转绿。
5. **疑似卡住**：任务运行中强杀 ZCode 的网络（或构造停更）→ 停更超 5 分钟且
   无活跃子代理 → 卡变红（Waiting）。（可选，构造成本高可跳过）
6. **error 转绿**：构造一个失败任务（如无效 API key 触发 error）→ ZCode 侧栏
   显示失败后，兔维斯 卡转绿（Finished）进未读池并弹提醒。
7. **跳转与已读**：点击绿色未读卡 → ZCode 被激活且**同一主窗口内切换到该项目
   工作区**（不新开窗口、其他后台任务不受影响）。已读口径按平台区分（与
   WorkBuddy/Codex 一致的 P1-2 分层防御）：**Windows** 深链派发后前台验证通过
   即标已读（仅该会话，同项目其他未读卡保留）；**macOS** 无前台验证设施，
   深链成功也不自动标已读——通过手动点卡上 X 关闭或等 24h 过期消除，兜底
   激活路径同理（见 Z4.2.1）。
8. **MCP 面板**：资源页 → MCP → 对 ZCode 启用一个服务器 →
   `~/.zcode/cli/config.json` 的 `mcp.servers.<name>` 出现
   `{command,args,env}`，文件其他键（plugins 等）与原键序不变；禁用 → 该条目
   移除、其余不动。手动在某条目加 `"enable": false` → 重新扫描后该服务器在
   ZCode 列不显示为启用（如实展示为停用）。把 config.json 改成非法 JSON →
   MCP 写入报错且文件原样保留（不被写坏）。
9. **Skill 分发**：资源页 → Skill → 对 ZCode 启用 → `~/.zcode/skills/<name>`
   出现符号链接（指向 `~/.tuvis/active/zcode/<name>`），SSOT 真身保留；禁用 →
   链接删除、SSOT 仍在。（ZCode 是否真实加载该目录见 Z4.1 开放问题）
10. **宿主清理**：兔维斯 运行中完全退出 ZCode（Cmd+Q）→ 下一轮扫描（≤30s）
    ZCode 全部卡片（含未读）消失；重开 ZCode → 24h 内会话卡恢复。
11. **跨重启保留**：ZCode 开着、兔维斯 有绿色未读卡 → 重启 兔维斯 → 未读卡仍在
    （宿主存活前提）；若重启前 ZCode 已关 → 重启后残留未读卡被清。
12. **Windows 专项**（Windows 实机）：① 任务栏关窗驻留托盘 → 卡片照常（宿主
    以进程存活判定）；② 点击卡片 → `zcode://workspace/open?path=E%3A%5C…`
    深链派发 + 前台验证通过 → 切到对应工作区且标已读；scheme 未注册的机器 →
    自动落 ZCode.exe 窗口聚焦兜底；③ 深链路径为反斜杠 + 大写盘符整段编码
    （日志可见 URL 形态）；④ Electron 辅助进程/会话运行时不被误判宿主
    （强杀主进程后卡片清理，即便辅助进程残存）。

---

# 实现说明 — codex 私有 Skill 目录化 + `.agents` 只读共享化

日期：2026-09-09 ｜ 分支：feat/codex-private-skill-dir ｜ 证据基线：官方文档 + codex 0.149.1 实机取证（详见 spec §2）

## X0. 决策总览

| 决策 | 内容 |
|---|---|
| codex 激活目标切换 | `skill_dir_for_tool("codex")`：`~/.agents/skills` → **`~/.codex/skills`**（私有目录）。`.agents` 是 Agent Skills 开放标准的跨工具共享目录（codex 与 zcode 都读，spec F1/F3），把它当 codex 专属激活目标会让「仅对 codex 启停」泄露给所有遵循该标准的工具 |
| `.agents` 降级为只读通用导入源 | `auto_import_extensions` 扫描源显式追加 `~/.agents/skills`，来源标签 **`agents-shared`**：手装技能照常入库，但 `source_tool=None`（不归属工具、不补链）；兔维斯 从此永不写该目录（唯一例外是迁移对话框对「兔维斯 自建链接」的处置） |
| 遗留链接一次性迁移对话框 | 识别谓词 = `.agents/skills` 下 target 规范化后位于 `~/.tuvis/active/codex/` 的 兔维斯 自建链接（手装真目录/外部链接/断链不命中）；启动后台检测，命中即弹二选一——「迁移到 .codex/skills」（在 `.codex/skills` 重建链接、删旧链，DB assignments 与 Layer 2 完全不动 = 启停无损；同名冲突跳过并报告）或「保留为共享」（`.agents` 侧 target 改指 Layer 1 `~/.tuvis/skills/`，脱钩 codex 启停、继续服务未适配工具）；两种结局谓词均不再命中，对话框自熄灭，无需持久化开关 |
| SSOT 删除保护 | 删除 `~/.tuvis/skills/<name>` 前若 `.agents/skills/<name>` 存在指向它的直链，删除请求返回需确认提示（防止「保留为共享」后误删 SSOT 导致依赖共享目录的未适配工具失效） |

## X1. 证据引用（spec F1–F6 摘引）

依据 spec `docs/engineering/specs/2026-09-09-codex-private-skill-dir-design.md` §2（2026-09-09 取证）：

- **F1** codex 读 `~/.agents/skills`——官方文档（开放标准用户级路径）；
- **F2** codex 也读私有 `~/.codex/skills`——codex 0.149.1 实机取证：全局状态留有
  `~/.codex/skills` 下技能的真实调用记录、官方捆绑系统技能落在 `.codex/skills/.system`、
  社区文档与 GitHub Discussion #9682 确认该目录用户技能可与系统技能并列、符号链接受支持；
- **F3** zcode 双读 `~/.zcode/skills`（优先）+ `~/.agents/skills`，同名 first-wins——ZCode 官方配置指南；
- **F4** 后端对 codex skill 目录的硬编码仅 `skill_dir_for_tool` 一处，写入/生效检测/导入扫描均派生自注册表——单点切换可行，linker 零硬编码；
- **F5** 启用状态真源 = DB assignments + Layer 2，工具侧目录只是投影——迁移可不动启停状态；
- **F6** codex adapter 的 `skill_dirs()` fallback 本就是 `base_dir().join("skills")` = `~/.codex/skills`。

残留风险（spec §2 / §8 V1）：`.codex/skills` 中符号链接被 codex 跟随目前只有 F2 的
文档背书 + codex 已在 `.agents/skills` 跟随 兔维斯 链接的旁证，发布前需按 spec §8 实机验证。

## X2. 方案选型

**方案 A（采用）**：codex → `~/.codex/skills` + `.agents` 只读导入源 + 遗留链接一次性迁移对话框。
方案 B（否决）：把 `.agents` 注册为可启用的伪工具目标——伪工具需贯穿 monitor/MCP/预设/徽标处处特判，
当前无实锤的未适配工具需求，YAGNI（留作后续立项）。方案 C（否决）：目录不动，用 codex
`[[skills.config]]` / zcode 配置禁用覆盖做精准化——破坏「链接存在 = 已启用」单一真源，复杂度最高。

## X3. 提交序列

```
b0beffa feat(adapter): switch codex skill registry to private ~/.codex/skills
d5f4c36 feat(resources): treat ~/.agents/skills as read-only shared import source
bccdfe9 feat(migration): legacy codex link detection and migrate/keep for .agents
7589153 feat(ui): one-time legacy codex skill link migration dialog
8d2c7e9 feat(resources): confirm guard when deleting SSOT skill referenced by .agents
```

既有测试更新三处断言：两处按全局约束的单元断言（§7.1 `codex_skill_dir_uses_real_cli_directory`、
§7.2 `registry_covers_late_registered_tools` 的 `.agents` 行），另有一处被迫同步的集成测试断言
（`src-tauri/tests/linker_test.rs::test_enable_skill_for_tool_creates_codex_harness_link`，
注册表切换的设计内后果：原断言 codex harness 链接落在 `~/.agents/skills`，注册表切至
`~/.codex/skills` 后按设计必然失败，已最小更新为 `.codex/skills` 断言并加锁 `.agents`
手装源保持非链接真目录）；其余既有测试与 fixture 零改动；新增测试全部 tempdir fixture，
零真实家目录访问。
