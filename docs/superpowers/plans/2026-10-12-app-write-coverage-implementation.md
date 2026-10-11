# APP 交互面分套与写通道全覆盖 · 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development（执行方式见文首「执行方式」节）。Steps use checkbox (`- [ ]`) syntax for tracking.

**状态：执行中**（2026-10-12 制定，同日按独立审阅 12 条修订为 **v2**——见文末「修订记录」；同日 **Task 0 探测批完成并回填**——见「⚠️ Task 0 探测回填」节，gate 全闭环，P0 起进入逐任务执行；所有行锚点已在当前 `feat/h1-h12-headless` 工作树上逐条核实）

**Goal:** 交付设计文档（2026-10-11 v2）全部范围：① 探测批（T0.1–T0.5，未验项先证后码）；② P0 修复止血批（zcode 探针 / codex APP 止血门 / WB 指纹）；③ P1 codex 轨（`codex_app.rs` 协议客户端 + api.rs 四臂 + ProbeCache 探测 + 前端双套）；④ P1 dsh 轨（`dsh_acp.rs` + routing `DshAcp` + api.rs ⑤b 臂）；⑤ 全量回归与 M18–M28 验收。

**分支：** `feat/h1-h12-headless`（延续在用工作分支；每 Task 一 commit，沿用仓库惯例 `feat(scope): 中文描述` / `fix(scope): 中文描述`）。

**上位文档与直接输入：**

| 文档 | 角色 |
|---|---|
| `docs/MASTER-PLAN.md` | 宪法——**本计划零宪法改动诉求；任何人不得修改它** |
| `docs/superpowers/specs/2026-10-11-app-interaction-split-and-write-coverage-design.md` | 本批设计权威（v2，裁决 28–34 已裁）；任务书「设计 §x」均指此文 |
| `research/refs/phase2-消息注入/2026-10-11-windows-app-write-channel-probe.md` | 探测报告（证据台账，活体/文档/未验三级标注）——**不入库，留本地** |
| `docs/superpowers/specs/2026-09-27-phase2-closure-app-injection-design.md` | H 系总设计（裁决 1–27；28–34 为本批增量，Task 1 回填其裁决表） |
| `docs/superpowers/plans/2026-10-04-h1-h12-implementation.md` | 前批计划（结构与纪律参照；其「遗留与跟进登记」 L-01~L-36 继续有效） |

---

## 执行方式（subagent-driven-development）

- **每任务 = 一个实现子代理**：控制者把该任务书**全文**（含前置依赖与锚点）贴给实现子代理；任务书自包含，子代理不需要读设计文档全文即可动工（设计 §节引用是溯源线索，不是必读前置）。
- **每任务完成后双审**：spec 审（对设计文档口径，派 code-reviewer 承担）+ 质量审（对测试清单与门禁）。审出问题由主 agent 返工后再进下一任务。
- **探测任务（T0.x）不派实现子代理写 src 代码**——探测允许写临时脚本（`/tmp` 或 `scripts/probe-*`，**探测脚本不进 src 实现**，产物只有探测报告 + 计划回填）。
- **任务间默认串行推进**（按编号序）；唯一允许讨论并行的是「文件域完全不相交」的任务对，但见下条红线。

**api.rs 串行红线（硬约束）**：P1 的 codex 轨与 dsh 轨**都触 `src-tauri/src/remote/api.rs`**（设计 §7 说「文件域不重叠」是指**核心新文件**——`codex_app.rs` vs `dsh_acp.rs`；api.rs 是共享触点）。计划已把 api.rs 触点任务串行排序：**Task 2（duration 透传三调用点）→ Task 3（止血门）→ Task 4b〔条件性：仅当 WB 回执需新字段时触 `wb_headless_dispatch` 调用点〕→ Task 6a（codex 模式族臂）→ Task 6b（问答补投 + 中止臂，与 6a 同串行位、两连号 commit）→ Task 7（探测前置调用）→ Task 10（dsh ⑤b 臂）**，之间不得换序。**禁止两个实现子代理并行改 api.rs。** 同理，Task 5 与 Task 9 都要在 `src-tauri/src/inject/headless/mod.rs` 加一行 `pub mod`——也按编号序串行，后到者以工作树现状为准。

---

## 纪律条款（每任务书都默认继承，实现子代理逐条遵守）

1. **TDD 先测后码**：每个实现任务先写失败测试（用例名即任务书测试清单），确认红，再实现到绿。禁止先写实现后补测试。
2. **git add 逐文件点名**：每个 commit 只加任务书「Commit」步列明的文件；**严禁 `-u` / `-A` / `.`**。
3. **不动 `docs/MASTER-PLAN.md`**：发现设计/实现与宪法冲突 → 停下，报用户裁决，不得自行处置。
4. **`research/` 不入库**：探测报告、探测脚本产物、schema 导出件全部留本地（`research/` 本就不入库；探测临时脚本放 `/tmp` 或 `scripts/probe-*` 且**不 commit 进 src 实现**——若放 `scripts/` 则单独 commit 并在 message 标明 `probe`）。
5. **门禁命令**（全绿才允许 commit）：
   - Rust：`cd src-tauri && cargo check && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check`
   - 前端：`pnpm lint && pnpm build && pnpm test`（`pnpm test` = `vitest run`；也可 `npx vitest run`）
   - **注意（易踩）**：跑 Rust lib 测试前须先 `pnpm run build:mobile` 产出 `dist-mobile/mobile.html`——`remote::server` 的 `static_serving_matrix` 用 rust-embed 嵌入该目录，产物缺失必挂。
6. **已知 main 环境债不算新账**（**本机 2026-10 实跑观测，无文档出处**——白名单随门禁基线漂移需人工复核；本批门禁若遇即归因登记、不阻塞）：connection 隔离×2、symlink/junction 1314 权限失败族、tailscale 探针、`cmd_shim` 载型 flake。**回归判定基线 = 与任务前同套件同环境对比**，不是「零失败」。
7. **探测 gate 纪律**：凡任务书「前置依赖」栏标注「依赖 T0.x 结论」的子项，**未拿到探测结论不得动该子项代码**；gate 结论以探测报告回填段为准，不以口头印象为准。
8. **审计诚实**：所有新动作臂的审计行记**实际走向**；失败与不支持如实透出，不谎报成功（前批 M15 红线延续）。
9. **commit message 风格**：沿用仓库惯例 `feat(scope): 中文描述` / `fix(scope): 中文描述` / `docs(spec): 中文描述`；一任务一 commit。

---

## 探测 gate 总览与降级路径（Task 0 结论 → 各任务的分支）

> 设计 §10 风险 3/6 + 探测报告 §5 未验清单的**可执行化**。每个 gate 的结论决定下游任务的形态；结论回填本计划对应任务书（追加「⚠️ 实施记录」块，**不改写任务书原文字面**——沿用前批纪律）。

| Gate | 探什么 | 决定什么 | 未过时的降级路径（可执行分支） |
|---|---|---|---|
| **T0.1** | codex daemon 控制插座通道（AF_UNIX 直连 + initialize 成帧 + 对 APP 活跃线程 resume/interrupt 可写性；主探工具 = **一次性 Rust 探针**〔std `UnixStream`，先阳性对照〕）**＋降级主腿**（跨实例 resume 磁盘既有非活跃线程 + turn——未验 #3） | **P1 codex 轨写路径架构**：活跃线程写动作（interrupt / 档位写）走 daemon 通道，还是走「关闭态 resume 主腿 + 打开态置灰」；主腿本身的可用性 | 失败 → Task 5 删去 daemon 通道子项（留缝与降级旗标）；**interrupt 臂仅 daemon 通道可用时提供，不可用时如实置灰**（关闭态在飞回合来自 exec resume 路、归既有 H4 kill——协议 interrupt 在关闭态无作用对象，不在关闭态假装提供协议中止）；档位写臂改为仅关闭态（resume 载体）可用 + 打开态如实「暂不可写，请关闭 APP 后重试或在 APP 内操作」（前提 = 降级主腿〔T0.1 Step 5〕验证通过；**两腿全败 → 档位切换整臂降级只读**，见 T0.2 行）；Task 8 灰置面相应扩大；M21/M22 验收判据按两腿实况改写 |
| **T0.2** | codex 档位载体三选一的行为级生效（turn/start.approvalPolicy / thread/resume.approvalPolicy / config/value/write） | 设计 §3.4 档位切换的**主载体**（M21 判据） | 三载体全不可行为级生效（ improbable）→ 档位切换整臂降级为只读（GET 下发 current，POST 回 501 如实「档位协议切换在本版本不可用」）；部分可用 → 选行为级生效且干扰最小者为主、其余为兜底链 |
| **T0.3** | dsh spawn 机制（node.exe + asar 载入 / --expose-internals 参数面 / dsh-subagent-acp 启动器，拿到确定 argv） | **Task 9 `dsh_acp.rs` 的 spawn 实现** | 全部候选起不来 → **停下报用户裁决**（dsh 轨整体推迟，不硬写 spawn 猜测）；部分候选可用 → 取「无需桌面宿主在场、stdout 纯协议」的最小 argv |
| **T0.4** | dsh 行为级全链（initialize → list → resume → prompt → cancel + request_permission 触发面） | **M25 验收判据与 cancel 接线**（session/cancel vs $/cancel_request） | cancel 不可用 → cancel 如实报「暂无撤回原语」（不谎报已取消），M25 判据相应删节并登记；request_permission 不触发 → 透传展示面照做（空路径），M25 该项标「未取证」 |
| **T0.5** | codex `thread/items/list` / `thread/turns/list` 读回形状（GET /session-mode APP 读回选法与字段映射） | **Task 5 读回族的方法选择**与 Task 6a GET 臂的 `current` 映射 | 形状不含档位信息（读不出 approvalPolicy）→ GET APP 臂 `current=null` 保守降级（「模式未知，请人工核对 APP」——设计 §3.3 既有降级语义），POST 回执核对同样降级为「未生效」三态的第三态 |

**已完成、无需重探**（探测报告已闭环，重探即浪费真实回合/token）：WB connect 判据（32462 活体 + 免鉴权 connectionId）、WB new 绑定语义、zcode 探针时延 15–19s、codex steer/interrupt 活体（ephemeral 线程 1.5s）、`thread/read` 跨实例只读。

---

## ⚠️ Task 0 探测回填（2026-10-12 二轮实测，gate 全闭环；证据 = `research/refs/phase2-消息注入/2026-10-12-probe-task0-codex.md` 与 `…-dsh.md` 两附录，留本地）

> 下述结论为**定案**，各任务书原文不改写，冲突处**以本节为准**（回填纪律沿用前批）。

- **T0.1（写路径架构 gate）**：daemon 控制插座协议破案 = **HTTP Upgrade → RFC 6455 WebSocket（客户端帧 mask）→ text 帧 JSONL JSON-RPC**（initialize 应答 1ms；**std 无 Windows AF_UNIX，生产实现须 socket2 0.6.5** `Domain::UNIX`+`Type::STREAM`）；`codex app-server proxy` **判死**（MITM 实证纯字节转发、无 WS 握手——一轮零输出负例根因闭环，Step 4b 触发条件消灭）。**决定性负结论：daemon 路对 APP 持有线程同样 -32600 拒（锁 = codex-home 跨进程级、无 takeover 参数）——「写 APP 活跃会话」在 0.162.1 无任何通道**。降级主腿两路全通（非活跃线程：裸 spawn resume 0.38s + turn + rollout 落盘；daemon WS resume 306ms + turn + interrupt 31ms → interrupted）。锁语义补充：硬杀写者后锁 ≤65s 过期；旧线程 pin 模型失效需 model 覆写。
  → **落点**：Task 5 **删去 daemon 通道子项**（主路 = 裸 spawn 短命实例，零 WS 依赖；daemon WS 通道收窄登记 P2 遗留〔本批-7〕；proxy 不再是任何备选）；interrupt 臂范围收窄见 Task 6b 回填。
- **T0.2（档位载体 gate）**：主载体 = **① `turn/start.approvalPolicy`**（行为级翻转实证：never → 无审批面直接执行/sandbox 拒；on-request → +13.5s 出 `item/commandExecution/requestApproval`，帧全形状归档）；兜底 = **② `thread/resume.approvalPolicy`**（rollout `turn_context.approval_policy` 逐回合精确跟随【持久层】+ untrusted 腿行为级）；③ `config/value/write` **末位弃用**（untrusted 档被 0.162.1 拒写 "no longer supported" + 全局副作用）。**判据语义修正：on-request 审批面是 agent 裁量，M21 必须用 untrusted/never 对照；触发输入须「OS 可写 + sandbox 写集外」（OS ACL 拒绝不触发审批面）**。
  → **落点**：Task 5 档位写主载体① + 兜底②（③ 留注释说明弃用因）；Task 6a POST 臂同载体；M21 判据按 untrusted/never 对照改写。
- **T0.5（读回 gate）**：`thread/read`/`thread/items/list`/`thread/turns/list` 数据在 **`result.data`**（非 turns/items 键）、游标为不透明 JSON 串、items 带 `{turnId,item,startedAtMs,completedAtMs}` 包装；**RPC 读回族全族不暴露 approvalPolicy**。
  → **落点**：GET /session-mode codex 臂 `current` 定案 = **rollout 尾部 `turn_context.approval_policy`**（MAM monitor 已解析 rollout，追加字段即可；null → 默认档展示）；全局默认可用 `config/read`。Task 5 读回族仅保留 `thread/read`（卡面/元数据用），档位读回**不走 RPC**。
- **T0.3（dsh spawn gate）**：候选 A 定案 = **`<dsh runtime node.exe> <asar解包目录>\dsh\node_modules\@deepseek-ai\dsh\lib\bin.js --profile acp`**（env：`DSH_HOME` 透传 + `DSH_TELEMETRY_DISABLED=1`；cwd 任意；前置 = 一次 `@electron/asar extract`；`acp` profile 是 shipped 模板、CLI 首启自动落盘）。候选 B 静态否决（Electron 单实例锁）、候选 C 不随 desktop asar 分发。**实现陷阱实证：`--patch` 对 acp config 是整对象替换，只写一键会抹掉 provider/model**。
  → **落点**：Task 9 spawn 按此 argv 钉值（测试 `spawn_argv_matches_probed_definition` 可写）。
- **T0.4（dsh 行为级 gate，部分受账户余额阻断）**：活体通过 = initialize（含动态 image 位）/ session/list（keyset-cursor 分页 + cwd 筛选）/ resume（跨进程单写者锁原文 "already owned by an active write handle"）/ new（必填 `mcpServers:[]`）/ set_config_option（值为 JSON 字符串形态）/ close（释放写锁）。**余额阻断三项**（deepseek 双路由 `Insufficient Balance`，5 prompt 全 API 侧拒、token 消耗 0）：update 成功流样本、在飞 cancel、request_permission 触发——已按源码级形状登记（update 六变体 / prompt 完成判据 `{stopReason}` / cancel 为 notification / permission 帧 allow-once/reject-once）。
  → **落点**：Task 9 按「活体帧 + 源码形状」实现，M25 真机兜底（用户充值后）；cancel 接线按 notification 形态实现、M25 标「行为级待验」。

**真实回合消耗申报（Task 0 合计 21 次）**：codex 组 16（T0.1×2 / T0.2×14〔其中 2 次因探测脚本替换 bug 打错目标，如实申报〕/ T0.5×0）+ dsh 组 5（全部被余额拒、token=0）。环境还原：config.toml sha 与原始一致（备份留 Temp）、marker 全清、无孤儿进程、scratch 会话已删；用户真会话仅 dsh「Meetings」留 1 条失败回合的用户消息（provider 拒单所致，未动其他数据）。

---

## 锚点速查表（2026-10-12 已在工作树逐条核实；子代理据此定位，勿再全文件搜索）

| 锚点 | 位置 | 说明 |
|---|---|---|
| `session_send` | `src-tauri/src/remote/api.rs:2486` | 发送端点；H3 门 ④ / 路由 ⑤ / 无头分派 ⑤b（`api.rs:2560-2600`） |
| `session_send_info` | `api.rs:2912` | send-info 能力位端点（channels[]） |
| `session_question`（GET 载荷） | `api.rs:6513` | 问答卡数据源（**GET 类，本批不动其 CLI 逻辑**） |
| `session_question_answer` | `api.rs:6920` | 问答作答入口（Task 3 止血门 ③ / Task 6b 补投臂） |
| `dispatch_question_action` | `api.rs:8089` | 问答动作分派（Task 6b 补投组装挂点） |
| `session_headless_cancel` | `api.rs:3756`（路由 `server.rs:655-656` `/session-headless-cancel`） | 无头中止端点（Task 6b interrupt 臂挂点） |
| `session_mode` | `api.rs:9778`（设计文档写 9571 起系该端点区块，函数本体 9778） | GET 当前档（Task 6a `surface`/`degraded` 字段 + APP current 读回） |
| `session_mode_menu` | `api.rs:10116` | POST 菜单键序（Task 3 止血门 ②；**GET 孪生 `session_mode_menu_read` `api.rs:10647` 不动**） |
| `session_mode_switch` | `api.rs:10748` | POST 切档（Task 3 止血门 ① / Task 6a 协议臂翻转点） |
| `zcode_headless_dispatch` / `codex_headless_dispatch` / `wb_headless_dispatch` / `cli_headless_dispatch` | `api.rs:1328` / `2065` / `2333` / `3179` | 四个既有无头分派器（Task 6b 补投复用 codex；Task 4b 条件性触 wb 调用点；Task 10 加 dsh 第五个） |
| `version_gate_receipt` 调用点 | `api.rs:1408` / `1787` / `2155`（本体 `gate.rs:174`） | duration 透传的三个调用点（Task 2 ④） |
| `PROBE_TIMEOUT_MS` / `PROBE_PROMPT` | `gate.rs:21`（`15_000`）/ `:27`（`"hi"`） | Task 2 A 项改 45s |
| gate.rs Timeout 分支 | `gate.rs:392` 起（`receipt.stage == Some(Stage::Timeout)` → 直接 fail 的分支） | Task 2 B 项改造点 |
| `ProbeCache` / `probe_cached` / `probe` | `gate.rs:292` / `:354` / `:374` | Task 7 复用的缓存纪律 |
| `TITLE_FINGERPRINT` / `health_ok` / 双确认调用 | `wb_acp.rs:487` / `:512` / `:529` | Task 4 修复点 |
| `AppPresence` / `presence_of` / `production_presence` | `inject/headless/codex.rs:125` / `:297` / `:1128`（**仓内 4 个同名 codex.rs，本表锚点一律带目录**） | Task 5/6a/6b 分派判据复用（H8 单点） |
| `resolve_thread`（rollout UUID 映射） | `inject/headless/codex.rs:263-289` | Task 5 读回定点 by id 复用 |
| `HeadlessKind` 枚举 / `wire_name` / `dsh_headless_pending` 分支 | `routing.rs:28` / `:49` / `:15-26`（文档注释）与 `:208`（分支体）；钉值测试 `:605` | Task 10 改造点 |
| `Session.form` | `session/model.rs:94`（`pub form: ProcessForm`） | 前端分套主判据（已存在，零新增字段） |
| `HEADLESS_STAGE_TRIAGE` | `src/mobile/SessionDetail.tsx:131-152`（`version_gate` 文案 `:139`） | Task 2 ⑤ 文案改点 |
| `sessionMode` 类型/请求 | `src/mobile/api.ts:1296` 起（GET `:1345`、switch `:1359`） | Task 8 类型扩展（surface） |
| 设置页工具勾选行 | `src/pages/settings.tsx:934-955`（`r.enabled` + `toggleTool`） | Task 3 裁决 34a 文案落点；用例文件 `tests/settings/toolManagement.test.tsx` |
| stage 跨语言夹具 | `tests/fixtures/headless_stages.json` | Task 2 文案改动不得动键集 |
| server.rs 路由表 | `server.rs:685-697`（session-question / session-mode 族注册处） | Task 6a/6b/10 如需新端点在此注册（预期不需要新端点） |

---

## Task 0：探测批（T0.1–T0.5）——**最高优先，先于一切代码任务**

**性质**：探测任务不写 src 代码、不 commit 实现文件；允许写临时脚本（`/tmp` 或 `scripts/probe-*`）。产出两件：① 探测报告新增章节（`research/refs/phase2-消息注入/2026-10-11-windows-app-write-channel-probe.md` 追加「§6 二轮探测（2026-10-12）」或独立附录文件，**留本地不入库**）；② 本计划对应任务书的「⚠️ 探测回填」块 + 设计文档对应 § 的结论回填（设计文档已入库，回填随 Task 1 或对应实现任务 commit）。

**通用探测纪律**（每个 T0.x 都遵守）：
- 证据三级标注（【活体】/【文档】/【未验】），结论不超证据；
- 涉及真实回合的探测（T0.2 / T0.4）**控制次数**（每形态 1–2 次），成本申报（参照 zcode 单回合 ~20k tokens）；
- 优先用 ephemeral 线程 / 测试会话 / 非活跃旧会话，**不污染用户活跃会话**；无法避免时先征用户同意；
- 探测脚本里 hardcode 的本机路径（socket 路径、node.exe 路径等）注明「本机探测态，非产品常量」。

### T0.1 codex daemon 控制插座通道探测（含降级主腿）【gate：P1 codex 轨写路径架构】

**方法学前提（2026-10-12 本机实测，探测工具面判定）**：① pwsh 在本机缺席；② Windows PowerShell 5.1 无 `UnixDomainSocketEndPoint`（实测报「找不到类型」——.NET Framework 无该类型，**PowerShell/.NET 直连路整体不可用**）；③ Node 直连 sock 得 EACCES/ENOENT（路径形态坑——MSYS/Git-Bash 路径改写与 NT 命名空间形态未钉）。**主路 = 一次性 Rust 探针**：std `UnixStream`（Rust 1.70+ 支持 Windows AF_UNIX，与生产侧 socket2 同栈）——`/tmp` 下独立小 cargo crate，**不进 src、不 commit**（纪律条款 4）；Node 降为备选且须先钉路径形态（Step 1b）。

- [ ] **Step 0: 阳性对照（必做，防假阴性）**——Rust 探针先自建 AF_UNIX socket（探针内 `UnixListener::bind` 临时路径 + 自 connect）走通一次完整收发（send JSON 行 → recv 回显）——**探针自身收发逻辑被证明正确后，才有资格判 daemon socket 的死活**；阳性对照本身的失败（如 bind 权限）单独记录，不得与「daemon socket 死」混判。
- [ ] **Step 1: 直连探测**——`codex app-server daemon version` 复核 daemon 在场（拿 socketPath）；Rust 探针对 `~/.codex/app-server-control/app-server-control.sock` connect——记录成败、errno/`io::Error` 原文（**EACCES / ENOENT / ECONNREFUSED 含义各不相同：ENOENT 可能是路径形态问题而非 socket 不在**，与 Step 0 阳性对照结果对照归因）。
- [ ] **Step 1b:（备选）Node 直连**——仅当 Rust 工具链受阻才用 Node `net.connect({path})`；**先钉路径形态**：Windows AF_UNIX 绑定路径是文件系统路径（`C:\Users\...` 形态），防 MSYS 路径改写（设 MSYS_NO_PATHCONV=1，或改试 Windows 扩展长度路径前缀〔四个字符：反斜杠x2+问号+反斜杠〕等形态），EACCES/ENOENT 分别归因记录后再判。
- [ ] **Step 2: initialize 握手成帧**——向插座发 `initialize` 请求，确定成帧方言：JSONL（`\n` 分帧）还是长度前缀（`Content-Length:` 或二进制前缀）；发送体形状参照裸 app-server 的 `{"id":1,"method":"initialize","params":{...}}`（探测报告 §1.1）；记录响应首帧原文（脱敏 userAgent 等）。**若首形态无响应，试第二形态（长度前缀）后再判死**。
- [ ] **Step 3: 活跃线程可写性**——对 APP 正打开的真实线程（rollout UUID 经 `~/.codex/sessions/` 最新文件取，或请用户指定一个可弃会话）：① `thread/resume {threadId}` 是否仍被 `-32600 already has an active writer` 拒（走 daemon 通道时单写者锁是否放行——**这是 daemon 路的决定性问题**）；② 若 resume 放行，追一发 `turn/start`（极小输入 "PROBE"）确认回合受理。**安全边界**：选可弃会话；探测后如产生回合，在 APP 里确认无需保留。
- [ ] **Step 4: interrupt 可写性**——若 Step 3 放行：起一个长回合（"count to 50 slowly"）→ `turn/interrupt {threadId, turnId}` → 计时到 `turn/completed`（对照 ephemeral 活体 1.5s 量级）。
- [ ] **Step 4b: proxy 复试（判死前的最后一条路）**——Rust 直连受阻（connect 失败 / 成帧不通）时，在 **PowerShell 原生环境**（powershell.exe 直接运行 `codex app-server proxy`，stdin/stdout 经原生管道，**不经 Git-Bash/MSYS 管道**）复测 proxy 转发——Git-Bash 下的零输出负例（探测报告 §1.1）可能是管道层伪影，**Git-Bash 负例不能永久封路**；复测仍零输出才允许判「proxy 不可用」并把证据等级回填报告。
- [ ] **Step 5: 降级主腿验证（跨实例 resume 磁盘既有非活跃线程 + turn）**——探测报告 §5 未验 #3，**T0.1 失败降级路径的主腿，必须独立验证（daemon 路成败与否都做）**。夹具纪律：**新 spawn 裸实例 × 磁盘既有非活跃线程**（旧 rollout UUID——APP 当前未打开的会话，从 `~/.codex/sessions/` 挑非当日文件）；**禁止**用「同实例 ephemeral 线程再 resume」充数（验不到跨实例单写者锁对磁盘线程的语义）。动作：`thread/resume {threadId}` → 放行则追 `turn/start`（极小输入 "PROBE"）→ 确认回合受理与产出落盘（rollout 文件追加）。结论三态：放行且回合产出 / resume 被拒（原因原文）/ resume 放行但 turn 被拒。
- [ ] **Step 6: 回填**——探测报告附录 + 本计划 Task 5/6a/6b/8 追加「⚠️ T0.1 回填」块（**daemon 通道结论 × 降级主腿结论的组合矩阵**：daemon 可写 / daemon 不可写但主腿可写〔走降级〕/ 两腿全败〔interrupt 置灰 + 档位写只读降级〕）。

**产出 gate**：Task 5「daemon 通道」子项与降级旗标、Task 6a 档位写臂两态分支、Task 6b interrupt 臂形态（仅 daemon 可用时提供）、Task 8 灰置面、M21/M22 判据形态。

### T0.2 codex 档位载体三选一（行为级生效实测）【gate：设计 §3.4 载体定案】

- [ ] **Step 1: 载体① turn/start.approvalPolicy**——裸 app-server + `thread/start {ephemeral:true}`；两回合对照：回合 A `approvalPolicy:"never"` + 触发工具调用的输入（如 "run `ls` via shell"），回合 B `approvalPolicy:"on-request"` + 同输入。观察差异面：B 是否产生 `item/commandExecution/requestApproval` ServerRequest、A 是否直接执行（或被 sandboxPolicy 拒——两者都可判「行为随档位变化」）。**判据 = 审批请求面的有无随 approvalPolicy 翻转**（参数被接受不算数——那已是文档证据）。
- [ ] **Step 2: 载体② thread/resume.approvalPolicy**——对**磁盘既有非活跃线程**（APP 关闭的旧会话——与 T0.1 Step 5 同一夹具纪律：新 spawn 裸实例 × 磁盘 rollout UUID，**禁止**用「同实例 ephemeral 线程再 resume」充数）：`thread/resume {threadId, approvalPolicy:"on-request"}` → 起回合 → 观察审批面。记录 resume 响应是否回显档位。
- [ ] **Step 3: 载体③ config/value/write**——读当前 config.toml 相关键 → `config/value/write` 写 approval 档 → 起回合观察 → **恢复原值**（全局副作用，必须回滚；回滚动作记录在报告里）。
- [ ] **Step 4: 回填**——按「行为级生效 + 干扰最小」排序定主载体与兜底链，回填 Task 5 档位写子项与 Task 6a POST 臂、M21 判据。

**产出 gate**：Task 5 档位写实现、Task 6a POST /session-mode APP 臂、M21。

### T0.3 dsh spawn 机制定案【gate：`dsh_acp.rs` spawn 实现】

- [ ] **Step 1: 候选盘点**——确认 `~/.dsh/dsh-runtimes/dsh-primary-runtime/dependencies/node/bin/node.exe` 在场；盘点 asar 内 `@deepseek-ai/dsh-acp` 的入口文件路径（asar 可用 `npx asar list` 或直接把 asar 当目录尝试 require——Electron 的 asar 支持裸 node 不认，候选 = asar 解包到临时目录 / `--expose-internals` 直通 / dsh-subagent-acp 启动器）。
- [ ] **Step 2: 逐候选试起**——目标 = 命令行起一个 stdout 纯协议流量的 stdio JSON-RPC 服务器：候选 A `node.exe <解包路径>/node_modules/@deepseek-ai/dsh-acp/<entry>`；候选 B `DeepSeek Harness.exe --expose-internals <参数面>`（从运行中进程命令行抄参数，`wmic process` / PowerShell `Get-CimInstance Win32_Process` 取完整 argv）；候选 C dsh-subagent-acp 启动器。判定：stdin 发 `initialize`（ACP v1 形状）→ stdout 收到 JSON-RPC 响应帧且无混杂日志（stderr 分流确认）。
- [ ] **Step 3: 配置面**——`provider`/`model` 从哪来：读 `~/.dsh` 既有配置（profiles/desktop 与 dsh-acp README 的会话配置项对照）；确认是否需要先落一个 `~/.dsh/profiles/acp` profile（本机现无——探测报告 §4.2），落盘内容记录。
- [ ] **Step 4: 回填**——确定 argv（含 env、cwd 前提）写进探测报告；Task 9 spawn 子项回填。

**产出 gate**：Task 9 spawn 实现（失败 → 停下报用户裁决，见降级路径表）。

### T0.4 dsh 行为级全链【gate：M25 判据与 cancel 接线】

- [ ] **Step 1: initialize → session/list**——对 T0.3 起出的服务器发 `initialize`（记录 agentCapabilities 回显）→ `session/list`（分页参数 + `cwd` 筛选参数实测），选一个**非活跃**旧会话为目标（或 `session/new` 新建测试会话）。
- [ ] **Step 2: resume → prompt**——`session/resume`（目标非活跃会话）→ `session/prompt`（无害输入 "Reply with exactly: DSH-PROBE-OK"）→ 记录 `session/update` 通知流形态（assistant 消息 / thought / 工具生命周期各来一帧样本）与完成判据。
- [ ] **Step 3: cancel**——再起一个长回合（"count to 50 slowly"）→ 在飞时 `session/cancel`（失败则 `$/cancel_request`）→ 记录哪个原语可用、回执形状、取消后 session/update 形态。
- [ ] **Step 4: request_permission 触发面**——发一条会触发权限请求的输入（如要求写工作区外文件/执行命令，按 README「一次性允许/拒绝」语义）→ 记录服务端→客户端请求帧形状（方法名、内容字段）——**只观察不代答**，等它超时或如实拒绝。
- [ ] **Step 5: 回填**——Task 9 各子项 + M25 判据回填。

**产出 gate**：Task 9 全部协议子项、Task 10 接线、M25。

### T0.5 codex 读回形状（GET /session-mode APP 读回用哪个、字段映射）【gate：Task 5 读回族方法选择】

- [ ] **Step 1: 裸实例 + 真实 UUID 只读实测**——spawn 裸 `codex app-server` → initialize → 对真实 rollout UUID（用户在场指定一个可读会话，或探测自建 ephemeral 线程）分别调 `thread/read` / `thread/items/list` / `thread/turns/list`，记录三者的响应 JSON 形状样本（字段名、分页参数、嵌套层级，脱敏正文）。
- [ ] **Step 2: 档位可推导性**——从读回里找 approvalPolicy / 档位面：`thread/read` 的线程元数据是否含档位？`thread/turns/list` 的 turn 记录是否带 `approvalPolicy`（T0.2 载体①回合级设定的回读面）？**结论 = GET /session-mode APP 臂的 `current` 用哪个方法、哪个字段映射**。
- [ ] **Step 3: 回填**——Task 5 读回族方法选择 + Task 6a GET 臂映射 + （若档位读不出）Task 8 `current=null` 降级面的触发条件。

**产出 gate**：Task 5 读回实现、Task 6a GET 臂、Task 8 ModeBarApp 降级态判据。

---

## Task 1：文档入库（设计文档 commit + 上位 spec 裁决表回填 28–34）

**目标**：设计文档与裁决增量进入版本库（当前 `git status` 显示设计文档 untracked）；探测报告留在 `research/` 不入库。

**前置依赖**：无探测 gate（纯文档）。建议在 Task 0 探测结论回填后一次 commit（探测回填直接写进设计文档对应 § 的「v2 探测回填」补段；若探测批拖长，可先 commit 设计文档 v2 现状、回填随实现任务走——二选一由控制者定，**不得把探测报告本体入库**）。

**文件域**：
- `docs/superpowers/specs/2026-10-11-app-interaction-split-and-write-coverage-design.md`（入库 + 探测回填）
- `docs/superpowers/specs/2026-09-27-phase2-closure-app-injection-design.md`（裁决表追加 28–34 行：议题 + 裁决结果一行摘要 + 指回本设计 §8——**只加行不改旧行**）

**实现要点**：
- 上位 spec 裁决表延续编号从 28 起（其 §2 现编至 27）；每行备注列指回本设计 §8 对应行，避免两处正文重复（单一真相源 = 本设计 §8）。
- 设计文档入库前快速校对一遍：§7 P0 表「version_gate 分诊文案（两侧同步）」的含义落地口径见 Task 2 任务书（「两侧」= 移动端 `HEADLESS_STAGE_TRIAGE` 文案一处 + `headless_stages.json` 键集锁——**键集不动、只改值**）；§3.2 的 `api.rs:9571` 锚点按锚点速查表更正口径为「区块 9571 起、函数本体 9778」。此类校对属编辑性修正，不算设计变更。

**测试清单**：无（纯文档）。

**验收门**：`git log` 显示设计文档与上位 spec 两个文件在一个 commit；`git status` 里无 `research/` 文件。

**Commit**：
```bash
git add docs/superpowers/specs/2026-10-11-app-interaction-split-and-write-coverage-design.md docs/superpowers/specs/2026-09-27-phase2-closure-app-injection-design.md
git commit -m "docs(spec): APP 交互面分套与写通道全覆盖设计入库 + 裁决 28-34 回填上位 spec"
```

---

## Task 2（P0）：zcode 探针修复——B 超时先看 stdout + A 阈值 45s + L0 快面 + duration 透传 + 分诊文案

**目标**：根因②修复（设计 §4，裁决 29c = A+B 叠加、裁决 33a = L0 采纳）：探针不再对 15–19s 的真实最小回合恒超时拒发；审计耗时不再是误导性的 `· 0ms`；移动端分诊文案不再误导归因「CLI flag 面已变」。

**前置依赖**：无探测 gate（zcode 时延 15–19s 已活体闭环，**无需重探**——探测报告 §2）。

**文件域**：
- `src-tauri/src/inject/headless/gate.rs`：`PROBE_TIMEOUT_MS`（:21）、Timeout 分支（:392 起）、`version_gate_receipt`（:174）、`probe`（:374）及 tests 模块
- `src-tauri/src/inject/headless/runner.rs`：**条件性**——实现要点 1 已预设「若 kill 路径丢弃已捕获缓冲需在 runner 修（缓冲保留语义、不动 kill 本身）」；测试清单的 `runner_kill_preserves_captured_stdout_buffer` 落点在此。确认无丢弃则本文件零改动
- `src-tauri/src/remote/api.rs`：三个 `version_gate_receipt` 调用点（:1408 / :1787 / :2155）——**仅传参变化，不触其他任务的任务域**
- `src/mobile/SessionDetail.tsx`：`HEADLESS_STAGE_TRIAGE.version_gate`（:139）
- `tests/mobile/SessionDetail.test.tsx`：文案锁钉

**实现要点**（设计 §4.1–§4.3）：
1. **B 主修（证据优先于时序）**：`gate.rs:392` 的 Timeout 分支改为——超时被杀后**先 join 已捕获 stdout**（`r.captured_stdout().join("\n")`）→ `parse_json_skipping_prefix` 命中 JSON → **Pass**（进程真跑完了回合、只是超阈值被杀，stdout 是完成证据）；仅零输出才 Fail。**实现注意**：先确认 `RunnerCfg` kill 路径不丢弃已捕获缓冲（`runner.rs` 的 stdout 捕获在 kill 后仍可读——用测试钉死，若 kill 路径确有丢弃需在 runner 修，改动面控制在缓冲保留语义、不动 kill 本身）。
2. **A 兜底（阈值 + 文案）**：`PROBE_TIMEOUT_MS` `15_000` → `45_000`（实测 15–19s ×2 余量）；超时零输出的 fail hint 改为「探针回合超时（本机实测最小回合 15–19s，阈值已放宽至 45s）——反复超时请检查网络/模型服务」，替换「工具可能挂死或缺 provider config」误导归因。hint 里的「45s」从常量格式化生成，不写死字符串（改阈值时文案跟着走）。
3. **duration 透传**：`ProbeVerdict` 携带实测墙钟（probe 起止计时），`version_gate_receipt` 用它 `with_duration_ms`——api.rs 三个调用点透传后审计行出真实耗时（现状 `· 0ms` 是回执默认值）。
4. **L0 快面（裁决 33a）**：探针两级化——L0（零模型成本）= exe/cjs/provider-config 在场性 stat + `--help` 能起能印（实测 0.9s）；L1（真回合 `"hi"`）= **L0 过且无成功缓存才跑**。红线：L0 串联闸门**不替代** L1（`--help` 漏判静默形态，Task 8 实证教训）——L0 过≠通过，只是放行去跑 L1；L0 败 → 直接 fail（结构性坏，不烧 token），fail hint 区分「可执行不在场」与「provider config 缺失」。
5. **移动端文案**：`HEADLESS_STAGE_TRIAGE.version_gate` 改「版本门控未通过（探针回合未出有效回执；详见原因行）」；**`headless_stages.json` 键集不动**（跨语言锁钉的是键集不是值——「两侧同步」的真义：Rust 侧 stage 枚举与移动端分诊表的键逐项相等）。
6. 缓存 TTL（成功长缓存 / 失败 5min）不回归（既有行为）。

**测试清单**（gate.rs tests + 移动端）：
- `timeout_with_captured_stdout_json_passes`（B 主断言：超时被杀但 stdout 有 JSON → Pass）
- `timeout_with_zero_output_fails_with_new_hint`（零输出 → Fail + 新文案含「45s」与「网络/模型服务」、不含「挂死」）
- `probe_timeout_is_45s`（常量钉值）
- `verdict_carries_real_wallclock_duration`（duration = 实测墙钟而非 0）
- `l0_stat_failure_blocks_l1_without_burning_a_turn`（exe 缺席 → L1 不跑）
- `l0_pass_does_not_shortcircuit_l1`（L0 过 + 无成功缓存 → L1 仍跑）
- `cached_success_skips_probe`（成功缓存 TTL 不回归）
- `runner_kill_preserves_captured_stdout_buffer`（kill 后缓冲可读——B 的前提）
- `tests/mobile/SessionDetail.test.tsx`：`version_gate` 分诊文案新字面锁钉 + 旧字面「CLI flag 面已变」不再出现断言

**验收门**：M19（真机：勾选工具 + H3 开 → 对 zcode 会话发送 → 探针过 → 回执含 assistant 摘要 + tokens；审计行真实耗时）。回归 M3/M16（zcode 发送既有链路零行为变化——L0/L1 与 B 改造只影响探针路径）。

**Commit**：
```bash
git add src-tauri/src/inject/headless/gate.rs src-tauri/src/inject/headless/runner.rs src-tauri/src/remote/api.rs src/mobile/SessionDetail.tsx tests/mobile/SessionDetail.test.tsx
# runner.rs 为条件性文件：确认 kill 缓冲无丢弃则从上一行 add 列表去掉
git commit -m "fix(zcode): 探针修复——超时先看stdout(B)+阈值45s(A)+L0快面+duration透传+分诊文案去误导"
```

---

## Task 3（P0）：codex APP 止血门（三入口 409）+ 设置页工具勾选文案（裁决 34a）【**api.rs 触点 · 串行位 2（前有 Task 2 传参面）**】

**目标**：根因①止血（设计 §7 P0）：codex APP 会话的模式/权限/问答动作在协议臂落地前明确拒绝（不再把键序打进不存在的控制台、报 `AttachConsole 0x80070006`）；设置页补「取消勾选的工具有看板与远程能力」提示（裁决 34a）。

**前置依赖**：无探测 gate（止血门是纯拒绝分支；T0.1/T0.2 只影响 Task 6a/6b 把它翻转为真臂——**翻转分两步：模式两入口归 6a、问答入口归 6b**）。

**文件域**：
- `src-tauri/src/remote/api.rs`：三个入口头部——`session_mode_switch`（:10748）、`session_mode_menu`（:10116）、`session_question_answer`（:6920）。**GET 类端点不动**（`session_mode` :9778、`session_mode_menu_read` :10647、`session_question` :6513 保持现状——读侧正常是设计 §3.1 原则 3）。
- `src/pages/settings.tsx`：工具勾选行（:934-955 区）旁补提示文案
- `src/i18n/locales/zh.json` / `en.json`：新文案双语键
- 测试：`src-tauri/src/remote/server.rs` tests（端点级）+ `tests/settings/toolManagement.test.tsx`

**实现要点**（设计 §1 根因① / §7 P0 表 / §5.4）：
1. **判据单点**：`form == App && tool == "codex"`——与 `session_send` 路由口径、Task 6a/6b 翻转点**同判据**（建议提一个小 helper 如 `codex_app_form(&session) -> bool` 供三入口与 Task 6a/6b 复用，放 api.rs 内部即可）。
2. **拒绝形态**：`409 Conflict` + `{"error":"app_form_pending","reason":"APP 会话该操作走协议通道，开发中——请在 APP 内操作"}`（reason 走既有错误载荷形态）。
3. **审计如实**：三入口的 409 拒绝各落一行审计（action 沿用既有词 + result=refused/app_form_pending，摘要含 session_id）——「开发中」状态对用户可见、对账本可查。
4. **CLI 直通**：`form == Cli` 的 codex 会话三入口行为零变化（既有键序 + 屏读闭环不动——设计 §3.1 原则 1「CLI 版固化」）。
5. **设置页文案（裁决 34a）**：勾选框旁一句「取消勾选的工具有看板与远程能力」（zh）/"Unchecked tools keep board & remote capabilities"（en，措辞可润色但语义不变）；`no_session` 载荷**不附 hint**（不为边缘态扩查询面——设计 §5.4 原文）。
6. **移动端 version_gate 文案两侧同步**：已并入 Task 2（本任务不动移动端）。

**测试清单**：
- server.rs：`mode_switch_rejects_codex_app_form_with_409`（:10748 入口 → 409 `app_form_pending`）
- server.rs：`mode_menu_rejects_codex_app_form_with_409`（:10116 入口）
- server.rs：`question_answer_rejects_codex_app_form_with_409`（:6920 入口）
- server.rs：`codex_cli_mode_and_question_pass_through_unchanged`（CLI 形态三入口直通——固化断言）
- server.rs：`codex_app_rejections_are_audited`（审计行在场）
- server.rs：`get_mode_and_menu_read_not_gated_for_app_form`（GET 类不受门影响——读侧正常）
- `tests/settings/toolManagement.test.tsx`：勾选区提示文案渲染断言（zh 至少）

**验收门**：M20（真机：codex APP 会话切档/作答 → 明确「开发中」拒绝，不再现控制台错误/AttachConsole 报错）；回归 CLI codex 模式/问答直通（上述直通用例 + 手工抽测）。

**Commit**：
```bash
git add src-tauri/src/remote/api.rs src-tauri/src/remote/server.rs src/pages/settings.tsx src/i18n/locales/zh.json src/i18n/locales/en.json tests/settings/toolManagement.test.tsx
git commit -m "fix(remote): codex APP 止血门——三入口 409 app_form_pending + 审计 + 设置页工具勾选文案"
```

---

## Task 4（P0）：WB 指纹修复——双确认第二腿改 connect 活体探测

**目标**：根因③代码侧修复（设计 §5.1）：Windows 内嵌 WB 的 `/health` 被鉴权挡（AUTH_REQUIRED）导致端口指纹双确认恒败、端点永远发现不了——即使工具勾选、WB 内开关已开也拒发。第二腿改为更强的「是 WB 且可用」证据。

**前置依赖**：无探测 gate（connect 免鉴权活体已闭环于 32462——探测报告 §3.1，**无需重探**；M26 修前基线已有人工复现：2026-10-12 00:13 拒发、审计 16:13:27Z）。

**文件域**：
- `src-tauri/src/inject/headless/wb_acp.rs`：`TITLE_FINGERPRINT`（:487，第一腿，**维持不动**）、`health_ok`（:512）、双确认调用处（:529 附近）及 tests 模块

**实现要点**（设计 §5.1）：
1. **第二腿改 `POST /api/v1/acp/connect` 活体探测**：免鉴权、即刻返回 `{connectionId, sessionToken}` = 比 health 更强的「是 WB 且可用」证据；**探测后连接即弃**（不占连接位——若协议有关闭/断开面则调之，无则直接断 TCP，行为记录在注释）。
2. **`{"status":"UP"}` 兼容分支保留**（Mac 旧版走 health 路——标题指纹命中 + health UP 仍算双确认通过；判定顺序建议：connect 成功 → 过；connect 失败/不可达但 health UP → 过（兼容）；两者皆败 → 双确认失败）。
3. **AUTH_REQUIRED 的 health 不再误杀**：health 返回 AUTH_REQUIRED 形态时若 connect 活体过 → 过（这正是本修的目标形态——加专门用例钉死）。
4. **引导文案语义恢复**：「请开启远程控制」只在 connect 与 health **都**失败（端点真没开）时出现；发现失败与 WB 内开关状态解耦（探测报告 §3.1 定案①）。
5. **投递臂不在本任务——归 Task 4b**（new+prompt 单臂与靶向标注，设计 §5.2；本任务只修发现面。M27/M28 的代码判据落点 = Task 4b，真机验收随 Task 11 手工清单执行）。

**测试清单**（wb_acp.rs tests，MockHttp 缝既有）：
- `connect_liveness_passes_double_confirm`（标题指纹 + connect 200 connectionId → 端点发现命中）
- `legacy_health_up_branch_kept`（connect 不可达 + health `{"status":"UP"}` → 仍过——Mac 兼容）
- `auth_required_health_not_misjudged`（health 回 AUTH_REQUIRED + connect 过 → 过——本修主断言）
- `both_legs_fail_means_refused_with_enable_hint`（双败 → refused + 「请开启远程控制」引导在场）
- `connect_probe_discards_connection`（探测连接即弃——审计/注释性断言或行为断言）

**验收门**：M26（修后真机：同 00:13 操作 → 端点发现命中（connect 活体判据）→ 进入投递链（Task 4b 合入后由投递臂接管；未合入时后继如实报未接线状态也可——发现面修复本身 PASS）；WB 内开关关 → 才报「请开启远程控制」）；M18（用户操作：勾选 WorkBuddy → ≤3s 出卡、取消 → 无卡）。

**Commit**：
```bash
git add src-tauri/src/inject/headless/wb_acp.rs
git commit -m "fix(workbuddy): 端点指纹修复——双确认第二腿改 connect 活体探测（/health AUTH_REQUIRED 误杀根修）"
```

---

## Task 4b（P0.5）：WB 投递臂靶向标注 + Windows new+prompt 单臂——M27/M28 代码落点【wb_acp.rs 触点 · 排 Task 4 后串行；条件性触 api.rs（须在 Task 6a 之前的串行窗口完成）】

> 审阅追补（P1-5）：M27/M28 判据要求「sessionId 透出 + 与卡面不一致明示」，但原 Task 5–10 无一触 `wb_acp.rs` 投递臂——本任务补齐归属。

**目标**：设计 §5.2 投递语义落地：① Windows 形态投递臂 = `session/new` + `session/prompt` **单臂**（`session/load` 恒 writer_occupied，形态分叉由端点发现形态决定，Mac 心跳路双臂代码不动）；② 回执/审计靶向标注（「已投递至 WorkBuddy 当前会话（sessionId=…）」；与卡面会话不一致 → 明示）。

**前置依赖**：Task 4 完成（端点发现修复 = 投递臂前置）；无探测 gate（WB connect 判据、new 绑定语义、writer_occupied 三项均活体闭环——探测报告 §3，无需重探）。

**文件域**：
- `src-tauri/src/inject/headless/wb_acp.rs`：投递臂（new+prompt 组装处）+ 回执组装 + tests
- （**条件性**）`src-tauri/src/remote/api.rs`：`wb_headless_dispatch`（:2333）回执载荷透出标注——若复用既有 `Receipt` 字段（reason/note 类）则**零 api.rs 改动**；确需新字段才小改，且**必须排在 Task 6a 之前的串行窗口完成**（api.rs 串行位 2.5，不与任何 api.rs 任务并行）
- （**条件性**）`src/mobile/SessionDetail.tsx` 回执卡：sessionId 标注渲染——走既有 reason 字段则零前端改动；走新字段则最小渲染补丁
- 测试：wb_acp.rs tests（+ 条件性 server.rs 端点级）

**实现要点**（设计 §5.2）：
1. **Windows 单臂形态**：由 Task 4 的端点发现形态（connect 活体判据）判定走 new+prompt 臂——`session/new` 绑定 APP 当前活跃会话（实测语义，无视 cwd 参数）；Mac 心跳路（load+prompt 双臂）既有代码**不动**（版本分叉由发现形态决定，不写 tool/version 判断）。
2. **靶向标注（诚实面）**：回执带实际投递 sessionId +「已投递至 WorkBuddy 当前会话」标注；sessionId ≠ 卡面会话 id → 明示「消息已进入 APP 当前会话，与所选会话不同」；审计行同样带 sessionId（可追溯——设计 §10 风险 5）。
3. **writer_occupied 如实回执**（M28 面）：load 路在 Windows 的恒拒形态回执如实（既有行为核对，不谎报成功）。
4. **误投先例转约束**：探测中 WB-PROBE-OK 误投用户会话的先例（设计 §5.2 末段）落为强制项——**没有 sessionId 的成功回执不允许出现**。

**测试清单**（wb_acp.rs tests）：
- `windows_arm_uses_new_prompt_single_arm`（形态分派）
- `receipt_annotates_delivered_session_id`（回执 sessionId 透出）
- `mismatch_with_card_session_is_explicit`（不一致明示文案）
- `audit_line_carries_delivered_session_id`（审计可追溯）
- `writer_occupied_honest_receipt_kept`（M28 面——既有行为不回归）
- （条件性）server.rs：`wb_send_receipt_exposes_targeting_annotation`（仅当走新字段）

**验收门**：单测绿；真机 M27/M28 归 Task 11（判据落点 = 本任务回执标注面）。

**Commit**：
```bash
git add src-tauri/src/inject/headless/wb_acp.rs
# 条件性追加（仅当走新字段）：src-tauri/src/remote/api.rs src-tauri/src/remote/server.rs src/mobile/SessionDetail.tsx
git commit -m "feat(workbuddy): 投递臂靶向标注——Windows new+prompt 单臂 + sessionId 透出 + 不一致明示（M27/M28 落点）"
```

---

## Task 5（P1 codex 轨）：`codex_app.rs` 协议客户端

**目标**：设计 §3.5 的协议面客户端：裸方言解析、spawn initialize、读回族、档位写、interrupt、daemon 通道——全部以**执行缝注入**实现（零真实 daemon 依赖的可测结构），真机行为由 M21/M22 验收。

**前置依赖（探测 gate，硬性）**：
- **依赖 T0.1 结论**：daemon 通道子项的形态（直连成帧方言 / 放弃走降级）。T0.1 未回填 → 本任务只做非 daemon 子项，daemon 子项挂起。
- **依赖 T0.2 结论**：档位写主载体（三选一定案）。未回填 → 档位写子项挂起。
- **依赖 T0.5 结论**：读回族方法选择（thread/read vs items/list vs turns/list）与档位读回字段映射。未回填 → 读回子项挂起。

**文件域**：
- Create: `src-tauri/src/inject/headless/codex_app.rs`（客户端主体 + tests）
- Modify: `src-tauri/src/inject/headless/mod.rs`（`pub mod codex_app;` 一行——**注意与 Task 9 的 mod.rs 触点串行**）

**实现要点**（设计 §3.5 / §3.4 / 探测报告 §1）：
1. **裸方言解析**：响应帧**无 `jsonrpc` 字段**（探测报告 §1.1 活体定案）——解析器按 `id`/`result`/`method` 识别，不按 2.0 严格校验；通知旁流（`configWarning` / `remoteControl/status`）与响应帧分道（按有无 `id`）。
2. **生命周期 = per-action 短命会话**（裁决 32a）：每动作 spawn `codex app-server`（裸）→ initialize → 调用 → 读回 → 退出。常驻池不做（三期 F3.3 边界）；唯一持连 carve-out 预留给 P2 审批应答（类型上留扩展缝，注释注明）。
3. **读路径（权威源）**：按 T0.5 结论实现读回族（默认形状 = `thread/read` 定点 by rollout UUID——裸实例 `thread/list` 恒 0 线程不枚举磁盘线程；UUID 映射复用 `inject/headless/codex.rs:263-289` `resolve_thread`〔仓内 4 个同名 codex.rs，引用带目录〕）。跨实例只读已活体实证（APP 持写者不挡读）。
4. **写路径**：
   - 非活跃线程：`thread/resume {threadId, approvalPolicy?}`（按 T0.2 若载体②生效）+ `turn/start`——与 H8 exec resume 等价面；
   - **活跃线程（APP 持写者）**：按 T0.1 结论——daemon 控制插座通道（`~/.codex/app-server-control/app-server-control.sock`，成帧按探测定案）可写则走之；不可写 → 模块级降级旗标（如 `daemon_available()` 探测函数），调用方据此走降级路径（**不假装可写**）。
   - `turn/interrupt {threadId, turnId}`（ephemeral 活体 1.5s 已证；对 APP 活跃线程经 daemon 通道 = T0.1 Step 4 结论）。**降级语义（T0.1 回填后钉死）**：interrupt 仅 daemon 通道可用时提供——不可用时如实置灰（关闭态在飞回合归 exec resume 路 H4 kill，协议 interrupt 在关闭态无作用对象）。
5. **档位写**（按 T0.2 主载体实现，其余载体留兜底链注释）：回执 = 协议读回三态（命中 / 不符 / 未生效）。
6. **执行缝注入**：spawn/通道交互抽象为 trait 或闭包缝（参照 `wb_acp.rs` 的 MockHttp / `zcode.rs` 的 TurnDeps 先例）——tests 全部纯核跑，零真实进程。
7. **版本面**：CLI↔APP 双坐标（本机 0.162.1 同版）只登记不硬编码；方法在场性探测归 Task 7（本任务不做缓存）。

**测试清单**（codex_app.rs tests，全部经缝 mock）：
- `bare_dialect_frame_parses_without_jsonrpc_field`（裸方言主断言）
- `notification_stream_separated_from_responses`（无 id 旁流分道）
- `initialize_failure_surfaces_channel_error`
- `method_absent_treated_as_unavailable`（-32601 形态 → unavailable 而非 panic/误判成功）
- `readback_three_states_hit_mismatch_ineffective`（命中/不符/未生效）
- `interrupt_receipt_shapes`（interrupt 回执 + turnId 校验）
- `daemon_channel_handshake_failure_sets_degraded_flag`（daemon 失败 → 降级旗标置位、非 daemon 路不受影响）
- `per_action_session_spawns_and_exits`（短命生命周期：每动作一个实例缝调用序列断言）
- `thread_uuid_resolution_reuses_rollout_mapping`（复用 `resolve_thread`——引用断言或同构断言）
- `approval_policy_write_uses_probed_carrier`（按 T0.2 回填后的载体钉值——回填前该用例写不出，挂起合规）

**验收门**：单测绿即过本任务门（协议面真机行为归 M21/M22，经 Task 6a/6b 接线后在 Task 11 手工清单执行）。

**Commit**：
```bash
git add src-tauri/src/inject/headless/codex_app.rs src-tauri/src/inject/headless/mod.rs
git commit -m "feat(codex-app): app-server 协议客户端——裸方言/短命会话/读回族/档位写/interrupt/daemon 通道（T0.1/2/5 定案）"
```

> **⚠️ T0.1/T0.2/T0.5 回填（2026-10-12，定案见「⚠️ Task 0 探测回填」节，冲突以该节为准）**：① **daemon 通道子项删除**——daemon 对 APP 持有线程同样拒写，主路 = 裸 spawn 短命实例（零 WS 依赖）；`daemon_channel_*` 测试与 socket2 依赖一并去除，模块级降级旗标改为「APP 持有线程写动作不可用」的静态语义（`active_thread_write_unavailable`）。② 档位写主载体 = `turn/start.approvalPolicy`（附极小输入）、兜底 `thread/resume.approvalPolicy`、③ 弃用留注释。③ 读回族 = 仅 `thread/read`（`result.data` 形状）；档位读回**不走 RPC**（GET 臂走 rollout 字段，Task 6a 落）。④ 测试 `approval_policy_write_uses_probed_carrier` 钉载体①；`daemon_channel_handshake_failure_sets_degraded_flag` 改为 `active_thread_write_unavailable_flag_surfaces_degraded`。

---

## Task 6a（P1 codex 轨）：api.rs 模式族臂——switch/menu 翻转 + GET surface + H3 门 + 审计词【**api.rs 触点 · 串行位 3（Task 3 之后、Task 6b/7/10 之前）**】

> 原单一 Task 6 按审阅（P2-11）拆分为 6a〔模式族〕+ 6b〔问答补投与中止〕——**同串行位、两连号 commit**（6a 先 6b 后，6b 紧随不复核 6a 已落面）。

**目标**：设计 §3.4 的模式族后端臂：把 Task 3 的 409 止血门（switch/menu 两入口）翻转为真协议臂；GET /session-mode 下发 `surface` 与 `degraded`；APP 臂与 `session_send` 同 H3 门（helper 落本任务，6b 复用）；审计词 `codex_appserver`。

**前置依赖**：
- Task 5 完成（客户端缝可用）。
- **依赖 T0.1 结论**：档位写的活跃态分支形态（daemon 可写 → 两态全开；不可写 → 仅关闭态 resume 载体 + 打开态如实拒——前提 = T0.1 Step 5 降级主腿验证通过；两腿全败 → 档位切换整臂降级只读）。
- **依赖 T0.2 结论**：POST /session-mode APP 臂主载体。
- **依赖 T0.5 结论**：GET 臂 `current` 读回映射。
- **api.rs 串行**：Task 3 之后、Task 6b 之前；执行期间无其他子代理触 api.rs。

**文件域**：
- `src-tauri/src/remote/api.rs`：
  - `session_mode_switch`（:10748）——Task 3 的 409 分支翻转为 APP 协议臂（档位写）
  - `session_mode_menu`（:10116）——APP 形态处理（菜单键序对 APP 不适用：回明确「APP 会话无菜单通道」错误或并入协议臂按载荷 kind 分派——**按现状载荷语义定，不新增菜单能力**）
  - `session_mode`（:9778）——GET 臂：`current` 改协议读回 + 载荷加 `surface:"app"|"terminal"` 与 **`degraded: Option<String>`**（协议可用性探测失败原因串，None=正常——**字段名与 Task 7/8 两侧钉死同名**，防集成期契约漂移）
  - H3 门接线（switch/menu 写路径；`codex_app_form` + `headless_blocked` 组合的 helper 供 6b 复用）
  - 审计词：`codex_appserver`（channel 列记实际走向；**不动 `HeadlessKind` 路由枚举**——设计 §3.4 审计口径）
- （**条件性**）`src-tauri/src/inject/headless/codex_app.rs`：接线时若缝签名 / 降级旗标查询函数需适配，允许小改（不改客户端本体语义）
- `src-tauri/src/remote/server.rs`：tests（端点级用例）

**实现要点**（设计 §3.4 表格模式两行 + §3.6）：
1. **分派判据复用 H8 单点**：`codex::presence_of(production_presence(form, pid))` + Task 3 的 `codex_app_form` helper。
2. **POST /session-mode APP 臂**：调 Task 5 档位写（主载体按 T0.2）；回执带**协议读回核对三态**——不符 → 如实报 + 前端不采信本地记忆重拉 GET（回执载荷带核对结果字段，前端消费在 Task 8）。
3. **GET /session-mode APP 臂**：`current` 从协议读回（方法/字段按 T0.5）；读不到 → `current=null`（保守降级「模式未知，请人工核对 APP」）；**载荷扩 `surface: "terminal" | "app"`——后端单点下发，防前端复制分派逻辑成第二真相源**（设计 §3.2）；CLI 会话载荷加 `surface:"terminal"`；两形态 `degraded` 均下发（terminal 形态恒 None）。
4. **H3 同门**：switch/menu 写路径与 `session_send` 同门——`headless_disabled` 403 同码同文案，判据复用 `headless_blocked` 单点。CLI 侧模式**不受 H3**（现状不变）——行为变化点，M24 显式回归。GET /session-mode 不进门（读不是动作——补裁 24a 同理）。
5. **审计**：动作行 action 沿用既有词、channel 列按实际走向记 `codex_appserver`（协议路）。

**测试清单**（server.rs tests）：
- `codex_app_mode_switch_routes_to_protocol_arm`（409 翻转：POST → 协议臂回执三态载荷）
- `codex_app_mode_switch_degraded_when_daemon_unavailable`（T0.1 失败分支：按回填结论选断言形态——含两腿全败时的只读降级分支）
- `get_session_mode_app_surface_and_readback_current`（`surface:"app"` + `degraded` 字段在场 + current 来自读回；读不到 → null）
- `get_session_mode_cli_surface_terminal`（CLI 载荷 `surface:"terminal"` + `degraded:null`）
- `mode_switch_audit_word_is_codex_appserver`
- `h3_gate_blocks_app_switch_menu_but_not_get`（开关关 → switch/menu 403、GET 200；CLI codex switch 不受门）
- `cli_codex_mode_unchanged`（Task 3 直通用例保持绿——固化断言）

**验收门**：单测绿；真机 M21/M24（模式族面）归 Task 11 手工清单。

**Commit**：
```bash
git add src-tauri/src/remote/api.rs src-tauri/src/remote/server.rs
# 条件性追加（仅缝适配时）：src-tauri/src/inject/headless/codex_app.rs
git commit -m "feat(remote): codex APP 模式族臂——协议切档/GET surface+degraded/H3 同门 + 审计词 codex_appserver（6a）"
```

> **⚠️ T0.1/T0.2/T0.5 回填（定案见「⚠️ Task 0 探测回填」节）**：① 档位写臂**仅非活跃（APP 关闭）线程可用**——活跃线程被跨进程写者锁拒（两路实测），打开态如实回「会话正被 APP 使用，请关闭 APP 后重试或在 APP 内操作」；`codex_app_mode_switch_degraded_when_daemon_unavailable` 用例改写为 `mode_switch_active_thread_honest_refusal`。② POST 臂载体 = turn/start.approvalPolicy（主）+ resume.approvalPolicy（兜底）。③ GET 臂 `current` = rollout 尾部 `turn_context.approval_policy`（monitor 追加字段透出；null → 默认档）；`degraded: Option<String>` 字段名不变。

---

## Task 6b（P1 codex 轨）：api.rs 问答补投 + 中止 interrupt 臂【**api.rs 触点 · 串行位 3 续（紧随 Task 6a，两连号 commit）**】

**目标**：设计 §3.4 的问答与中止臂：问答降级补投（裁决 28b，经 H8 queue 通道）；无头中止端点 APP 形态 = 协议 interrupt（**仅 daemon 通道可用时提供**）+ queue 滞留如实 + exec resume 路维持 H4 kill。

**前置依赖**：
- Task 6a 完成（H3 helper、`surface`/`degraded` 载荷已落——本任务复用不复写）。
- **依赖 T0.1 结论**：interrupt 臂形态——**仅 daemon 通道可用时提供；不可用时如实置灰**（关闭态在飞回合来自 exec resume 路、归既有 H4 kill——协议 interrupt 在关闭态无作用对象，不在关闭态假装提供协议中止）。
- 问答补投无新探测 gate（走 H8 queue 通道，已交付）。
- **api.rs 串行**：紧随 Task 6a；执行期间无其他子代理触 api.rs。

**文件域**：
- `src-tauri/src/remote/api.rs`：
  - `session_question_answer`（:6920）+ `dispatch_question_action`（:8089）——补投臂（Task 3 的 409 分支在此入口翻转为补投）
  - `session_headless_cancel`（:3756）——interrupt 臂挂点（在飞协议回合 → interrupt〔仅 daemon 可用〕；queue 滞留 → 如实「已入队未消费，暂无撤回原语」；exec resume 路 → 既有 H4 kill 不动）
- （**条件性**）`src-tauri/src/inject/headless/codex_app.rs`（同 6a 条款）
- `src-tauri/src/remote/server.rs`：tests

**实现要点**（设计 §3.4 问答/中止两行）：
1. **问答补投（裁决 28b）**：作答文本模板化（单选 =「选择第 N 项：<文本>」；多选逐项；自由作答 = 原文补投）经 **H8 queue 通道**投递——复用 `codex_headless_dispatch`（:2065）+ 既有串行锁；回执 `screen=null` + 引导文案（「已补投为消息，请回 APP 核对题面」——**永不声称已作答**）；审计 `action=question, channel=headless_codex_queue`（实际走向）。
2. **H3 同门复用 6a helper**：question/cancel 写路径与 `session_send` 同门；CLI 侧问答不受 H3（现状不变）——M24 回归面。
3. **interrupt 臂按 T0.1 结论（P2-12 措辞钉死）**：daemon 通道可用 → 协议 `turn/interrupt`；不可用 → 如实置灰（回执「活跃回合暂无协议中止通道」），**关闭态在飞回合归 exec resume 路的既有 H4 kill，不在关闭态假装提供协议中止**。
4. **queue 滞留项撤回**：无原语——如实报（设计 §3.4 中止行原文）。

**测试清单**（server.rs tests）：
- `question_answer_app_routes_to_queue_repost_with_null_screen`（补投 + `screen:null` + 模板三型：单选/多选/自由）
- `question_answer_audit_word_is_headless_codex_queue`
- `h3_gate_blocks_question_and_cancel`（开关关 → question/cancel 403；CLI codex question 不受门）
- `headless_cancel_app_interrupt_vs_queue_stranded`（在飞 → interrupt 缝回执；queue 滞留 → 如实文案）
- `headless_cancel_app_interrupt_grayed_when_daemon_unavailable`（T0.1 失败分支：interrupt 置灰如实回执——**不在关闭态假装提供协议中止**）
- `cli_codex_question_unchanged`（固化断言）

**验收门**：单测绿；真机 M22/M23/M24（问答/中止面）归 Task 11 手工清单。

**Commit**：
```bash
git add src-tauri/src/remote/api.rs src-tauri/src/remote/server.rs
# 条件性追加（仅缝适配时）：src-tauri/src/inject/headless/codex_app.rs
git commit -m "feat(remote): codex APP 问答补投 + 中止 interrupt 臂——queue 补投/screen=null/滞留如实 + H3 同门（6b）"
```

> **⚠️ T0.1 回填（定案见「⚠️ Task 0 探测回填」节）**：interrupt 臂范围收窄为 **MAM 自持连接的在飞协议回合**（如档位切换 turn/start 所起回合，daemon WS 实测 interrupt 31ms；裸 spawn 自持实例同理）——**APP 发起的在飞回合与 queue 滞留项一律如实置灰/文案**（「活跃回合由 APP 持有写者，暂无协议中止通道」/「已入队未消费，暂无撤回原语」）；`headless_cancel_app_interrupt_grayed_when_daemon_unavailable` 用例语义相应改写（置灰条件 = 非 MAM 自持回合，而非 daemon 缺席）。

---

## Task 7（P1 codex 轨）：`codex_appserver` 可用性探测进 ProbeCache【**api.rs 触点 · 串行位 4（Task 6a/6b 之后、Task 10 之前）**】

**目标**：设计 §3.5 版本门：协议面动作前置可用性探测（spawn + initialize + 方法在场）进既有 `ProbeCache` 缓存纪律——防止 codex 版本漂移时每动作都付一次失败 spawn。

**前置依赖**：Task 5 完成（客户端缝——探测复用其 spawn/initialize 缝）；T0.5 结论（方法在场性判据用哪些方法名）。

**文件域**：
- `src-tauri/src/inject/headless/gate.rs`：`ProbeSpec` 扩 `CodexAppServer` 变体（或平行 spec 结构，按 gate.rs 现状取最小侵入形态）+ `probe_cached` 复用
- `src-tauri/src/inject/headless/codex_app.rs`：探测实现（spawn 裸实例 + initialize + 关键方法在场性——`thread/read` / `turn/start` / `turn/interrupt` / T0.2 载体方法；schema 317 文件仅素材，**运行时以探测为准**）
- `src-tauri/src/remote/api.rs`：Task 6a/6b 各协议臂入口前置探测调用（缓存未过 → 跳过协议尝试直接降级回执，不裸失败）
- 测试：gate.rs / codex_app.rs tests

**实现要点**（设计 §3.5 版本门行 + §10 风险 2）：
1. **缓存纪律同款**：成功长缓存 / 失败 TTL（沿用 zcode 探针的 5min 失败 TTL 口径）；键含 CLI mtime 坐标（版本漂移自动失效）。
2. **探测内容 = 最小代价**：spawn + initialize + 方法在场性（schema 层面 `-32601 method not found` 判缺席）——**不起真实回合**（零模型成本；行为级面归 M21）。
3. **降级联动**：探测 fail → Task 6a/6b 协议臂入口给降级档（GET 载荷 `surface:"app"` 且 **`degraded: Option<String>`** 置为失败原因串——**字段名与 Task 6a 钉死同名**，本任务只负责赋值来源，None=正常；POST 回执如实「协议面不可用：〈原因〉」）——前端 Task 8 的 ModeBarApp 降级态数据源。

**测试清单**：
- `codex_appserver_probe_passes_on_initialize_and_methods_present`
- `codex_appserver_probe_fails_when_method_absent`（-32601 → unavailable + 原因）
- `codex_appserver_probe_cached_success_skips_respawn`
- `codex_appserver_probe_failure_ttl_5min`（失败缓存过期重探）
- `api_arm_returns_degraded_receipt_when_probe_fails`（端点级：探测 fail → 降级回执不裸失败）

**验收门**：单测绿；真机：卸载/改名 codex 二进制后 APP 臂给降级回执（可选手工项，归 Task 11 附带）。

**Commit**：
```bash
git add src-tauri/src/inject/headless/gate.rs src-tauri/src/inject/headless/codex_app.rs src-tauri/src/remote/api.rs src-tauri/src/remote/server.rs
git commit -m "feat(codex-app): 可用性探测进 ProbeCache——spawn+initialize+方法在场性，失败降级不裸败"
```

---

## Task 8（P1 codex 轨）：前端双套——ModeBar / QuestionCard 分置 + surface 选套 + APP 词汇表

**目标**：设计 §3.3 / §3.7 的前端面：CLI 版固化改名、APP 版新建、`surface` 选套、APP 词汇表（不出现「终端/屏读/按键」）、灰置分层（裁决 31b）。

**前置依赖**：Task 6a/6b 完成（`surface`/`degraded` 载荷与回执三态字段已下发）。无新探测 gate（纯前端消费 Task 6a/6b 契约）。

**文件域**：
- `src/mobile/ModeBar.tsx` → **改名** `src/mobile/ModeBarCli.tsx`（逻辑零变化，仅文件名与内部注释措辞「CLI 形态专属」）
- Create: `src/mobile/ModeBarApp.tsx`（当前档来自协议读回；读不到 →「模式未知，请人工核对 APP」；点切档 → POST → 回执核对不符 → hint + 自动重拉 GET，**不拿回执改本地状态**；探测降级 → 只读 chips + 原因）
- `src/mobile/QuestionCard.tsx` → **改名** `src/mobile/QuestionCardCli.tsx`；Create: `src/mobile/QuestionCardApp.tsx`（题目渲染照旧；作答 = 每题「补投作答」入口：单选 =「选择第 N 项：<文本>」、自由作答 = 原文补投；回执如实「已补投为消息，请回 APP 核对题面」+ `screen=null`——永不声称已作答、不显示已翻转）
- `src/mobile/SessionDetail.tsx`：cardDock 挂载点按 GET 载荷 `surface` 选套（terminal → Cli 套、app → App 套；载荷缺 surface 的旧后端兼容 → 按 `form` 兜底，注释注明）
- `src/mobile/api.ts`：`sessionMode` 载荷类型扩 `surface` 与 `degraded`（字段名与 Task 6a/7 钉死一致）、switch 回执扩核对三态字段
- `src/mobile/ApproveCard.tsx` / `PlanFeedbackBar.tsx`：APP 形态显式排除（防「请人工核对终端」文案漏出——设计 §3.3 树图第三行；PlanFeedbackBar 的 Claude 品牌参数化一并做，设计 §3.7(a)）
- `src/mobile/MessageComposer.tsx`：**不改发送逻辑**（已通道化）——只分词汇表与卡片在场探针（CLI 卡片探针在 APP 形态隐藏：jump/edit/retract 的 TUI 队列 UI 按通道隐藏）
- 测试：`tests/mobile/ModeBarApp.test.tsx`（新）、`tests/mobile/QuestionCardApp.test.tsx`（新）、既有 `tests/mobile/ModeBar.test.tsx` / `QuestionCard.test.tsx` **原样平移**改名为 `tests/mobile/ModeBarCli.test.tsx` / `tests/mobile/QuestionCardCli.test.tsx`（零行为变化机器证明——设计 §10 风险 12）、`tests/mobile/SessionDetail.test.tsx` 追加选套用例

**实现要点**（设计 §3.3 树图 + §3.7 (a)(b)(c)）：
1. **固化纪律**：改名 commit 里 ModeBar/QuestionCard 的既有用例**一字不改平移**（import 路径除外）——平移后全绿 = 固化零行为变化的机器证明。
2. **APP 词汇表**（约 30 条 CLI 专属文案的替换面，设计 §3.7(a) 清单）：统一「APP/会话」措辞，禁「终端/屏读/按键」字样——用例级断言（对 App 套渲染输出的字符串扫描）。
3. **灰置分层（裁决 31b）**：工具级无机制 → 不渲染（既有 `unsupported`）；形态级暂缺（steer/queue 滞留撤回等）→ 灰置 + 一句原因。
4. **降级链**：`surface=app` 但协议探测未过（Task 7 降级档——前端消费 GET 载荷的 `degraded` 字段）→ ModeBarApp 只读 chips + 原因；QuestionCardApp 读侧不依赖协议面不受影响。
5. **不新建 APP 版 MessageComposer**（设计 §3.7(b) 定案）：共用现件 + 词汇表 + 隐藏 CLI 卡片探针；codex queue 消费翻转信号（回执卡「排队中」→「APP 已消费」）为**前后端合解的契约缺口**——后端信号面若本批排不下，前端预留状态位并如实显示「排队中（消费确认建设中）」，**登记到 Task 11 遗留**（不谎报已消费）。
6. **视觉面**：不引入按工具/按形态的皮肤分叉（设计 §3.7(c)）；品牌檐色既有机制不动。

**测试清单**（用例名级）：
- `mode_bar_app_renders_readback_current`（协议读回档位渲染）
- `mode_bar_app_unknown_current_shows_manual_check_hint`
- `mode_bar_app_switch_mismatch_triggers_refetch_not_local_state`
- `mode_bar_app_degraded_renders_readonly_chips_with_reason`
- `question_card_app_repost_single_choice_template`
- `question_card_app_repost_free_text_template`
- `question_card_app_receipt_never_claims_answered_and_screen_null`
- `app_suite_copy_has_no_terminal_or_screen_read_words`（词汇表断言——扫描渲染输出禁词）
- `session_detail_selects_cli_suite_on_terminal_surface`
- `session_detail_selects_app_suite_on_app_surface`
- `session_detail_surface_fallback_to_form_when_absent`（兼容）
- 既有 ModeBar/QuestionCard 用例平移后全绿（不改断言）
- `approve_and_plan_feedback_hidden_for_app_form`

**验收门**：单测绿 + `pnpm build` 过；真机 M20 翻转后的 UI 面（M21/M23 的前端判据）归 Task 11。

**Commit**：
```bash
git add src/mobile/ModeBarCli.tsx src/mobile/ModeBarApp.tsx src/mobile/QuestionCardCli.tsx src/mobile/QuestionCardApp.tsx src/mobile/SessionDetail.tsx src/mobile/api.ts src/mobile/ApproveCard.tsx src/mobile/PlanFeedbackBar.tsx src/mobile/MessageComposer.tsx tests/mobile/ModeBarApp.test.tsx tests/mobile/QuestionCardApp.test.tsx tests/mobile/ModeBarCli.test.tsx tests/mobile/QuestionCardCli.test.tsx tests/mobile/SessionDetail.test.tsx
git commit -m "feat(mobile): ModeBar/QuestionCard 双套分置——CLI 固化改名 + App 新建 + surface 选套 + APP 词汇表 + 灰置分层"
```
（注：若 ModeBar/QuestionCard 旧文件用 `git mv` 保留历史，`git add` 列表相应成对出现旧删新增——同样逐文件点名。）

> **⚠️ T0.1/T0.2/T0.5 回填（定案见「⚠️ Task 0 探测回填」节）**：① ModeBarApp 打开态切档 → 灰置 +「会话正被 APP 使用，请关闭 APP 后重试或在 APP 内操作」（关闭态可切）；② `mode_bar_app_switch_mismatch_triggers_refetch_not_local_state` 保留；③ `current=null` 的触发面 = rollout 无 `turn_context.approval_policy` 记录（旧会话）——显示「模式未知，请人工核对 APP」；④ 词汇表断言不变。

---

## Task 9（P1 dsh 轨）：`dsh_acp.rs` dsh-acp stdio 客户端

**目标**：设计 §6 的 dsh 写通道本体：spawn（按 T0.3）、三态靶向（list→resume / new）、prompt、cancel（按 T0.4）、request_permission 透传展示不代答。

**前置依赖（探测 gate，硬性）**：
- **依赖 T0.3 结论**：spawn argv 定案。未回填 → 本任务不得动工（降级路径：全部候选失败 → 停下报用户裁决）。
- **依赖 T0.4 结论**：cancel 原语选择与 request_permission 帧形状。未回填 → cancel/permission 子项挂起，其余可先行。

**文件域**：
- Create: `src-tauri/src/inject/headless/dsh_acp.rs`（客户端 + tests）
- Modify: `src-tauri/src/inject/headless/mod.rs`（`pub mod dsh_acp;`——**在 Task 5 之后，mod.rs 触点串行**）

**实现要点**（设计 §6.1 / 探测报告 §4）：
1. **spawn**（按 T0.3 回填 argv）：stdio JSON-RPC，stdout 只承载协议流量（stderr 分流）；provider/model 会话配置读 `~/.dsh` 既有配置（T0.3 Step 3 结论）；profile 落盘策略按探测结论（若需 `~/.dsh/profiles/acp`，写盘动作要幂等且不碰 desktop/headless/web 三 profile）。
2. **生命周期**：per-action 短命（裁决 32a 同款——每动作 spawn → initialize → 调用 → 退出）；stdio 桥参照 `inject/headless/cli_three.rs` 的双向帧处理先例（仓内同名文件多，引用带目录）。
3. **三态靶向**（session/list 按 cwd 筛选 → 分派）：
   - ① 桌面未打开（非活跃）→ `session/resume` + `session/prompt` **精确靶向**；
   - ② 桌面打开中（活跃）→ 单写者互斥如实拒：「会话正被桌面使用，请关闭后再投」（**不做 queue 臆测**——README 未提 queue，设计 §6.1 红线）;
   - ③ 无既有会话 → `session/new`（MAM 无头新建语义同族）。
4. **prompt 回执**：收 `session/update` 通知流（assistant 消息 / thought / 工具生命周期 / 配置变化 / 上下文用量——T0.4 Step 2 样本为解析真值）；归一为既有 `Receipt`（末条 assistant 摘要 + 时长）。
5. **cancel**（按 T0.4）：`session/cancel`（或 `$/cancel_request`）——不可用则如实「暂无撤回原语」不谎报。
6. **request_permission 透传**：收到即上屏「dsh 请求权限：〈内容〉」——**首批只展示不代答**（代答列 P2）；透传面挂到既有问答/审批载荷机制（`session_question` 载荷族或审批卡通道——按 T0.4 帧形状选最小接入面，接入点在 Task 10 接线时定）。
7. **范围边界（不做清单，设计 §6.1）**：mode（dsh-acp 刻意无）、fork/delete/load（刻意无）、19387 web HTTP、webhook——客户端不实现这些方法，路由层如实 unsupported。
8. **执行缝注入**：spawn 缝 mock，tests 纯核。

**测试清单**（dsh_acp.rs tests）：
- `spawn_argv_matches_probed_definition`（T0.3 回填后钉值——回填前挂起）
- `three_state_dispatch_inactive_resumes_active_refuses_absent_news`（三态分派主断言）
- `active_session_refusal_copy_names_desktop_writer`（如实拒文案）
- `session_list_filters_by_cwd`
- `prompt_receipt_from_session_update_stream`（通知流归一——T0.4 样本形状）
- `cancel_receipt_or_honest_unavailable`（按 T0.4 结论二选一形态）
- `request_permission_passthrough_display_only_never_autoanswers`（透传不代答主断言）
- `initialize_failure_surfaces_channel_error`
- `per_action_session_spawns_and_exits`

**验收门**：单测绿；真机 M25 归 Task 11。

**Commit**：
```bash
git add src-tauri/src/inject/headless/dsh_acp.rs src-tauri/src/inject/headless/mod.rs
git commit -m "feat(dsh): H13 dsh-acp stdio 客户端——三态靶向 resume/new/prompt/cancel/权限透传（T0.3/4 定案）"
```

> **⚠️ T0.3/T0.4 回填（定案见「⚠️ Task 0 探测回填」节）**：① spawn argv 钉值 = `<dsh runtime node.exe> <asar解包目录>\dsh\node_modules\@deepseek-ai\dsh\lib\bin.js --profile acp`（env `DSH_HOME` 透传 + `DSH_TELEMETRY_DISABLED=1`；asar 解包步骤的实现策略〔构建期 vs 首启〕由实现者按最小侵入定并在代码注释登记）；**`--patch` 是 acp config 整对象替换——绝不只写单键**。② 活体帧面（initialize/list 分页+cwd 筛选/resume 单写者锁原文 "already owned by an active write handle"/new 必填 `mcpServers:[]`/set_config_option 值为 JSON 字符串/close 释放锁）按探测样本解析；③ update 流 / 在飞 cancel / request_permission 按源码形状实现（update 六变体 / 完成判据 `{stopReason}` / cancel notification / permission allow-once-reject-once），M25 真机兜底（用户账户充值后）——`prompt_receipt_from_session_update_stream` 与 `cancel_receipt_or_honest_unavailable` 的断言形状以源码形状登记为准。

---

## Task 10（P1 dsh 轨）：routing `DshAcp` + api.rs ⑤b 臂 + send-info【**api.rs 触点 · 串行位 5（必须在 Task 6a/6b/7 之后，P1 双轨合流点）**】

**目标**：设计 §6.1 路由接线：dsh 进无头通道族，替换 `dsh_headless_pending` 刻意缺席分支；H3 自然接管 dsh；send-info 能力位下发。

**前置依赖**：
- Task 9 完成；Task 6a/6b 已合入（**api.rs 串行红线**——本任务不得与 Task 6a/6b 或任何其他任务并行触 api.rs）。
- T0.3/T0.4 已回填（Task 9 的前置顺延）。

**文件域**：
- `src-tauri/src/inject/routing.rs`：`HeadlessKind` 加 `DshAcp` 变体（:28 枚举）、`wire_name` 加词（:49——钉值测试 `:605` 同步）、`dsh_headless_pending` 分支替换（:208 分支体 + `:15-26` 模块文档注释更新）、无头绑定集（H3 管辖集）扩 dsh（:292 附近注释与判定）
- `src-tauri/src/remote/api.rs`：⑤b 分派臂（`api.rs:2560-2600` 加 `K::DshAcp` 臂 → 新 `dsh_acp_headless_dispatch`）、`session_send_info`（:2912）channels 加 `headless_dsh_acp`
- `src-tauri/src/remote/server.rs`：tests
- `tests/fixtures/headless_stages.json`：**不动**（stage 枚举无新增——dsh 复用既有 stage 族）

**实现要点**（设计 §6.1 路由接线行）：
1. **枚举无兜底纪律**：⑤b 匹配「无 `_` 兜底臂」（api.rs 既有注释）——加 `DshAcp` 臂后编译器继续守护全覆盖。
2. **`dsh_headless_pending` 分支删除翻转断言**：既有用例「dsh 必须如实报 dsh_headless_pending」（routing.rs :413-422 附近）翻转为「dsh APP 形态/进程不在 → `Headless(DshAcp)`」。
3. **H3 自然接管 dsh**（行为变化，M24 一部分）：开关关 → dsh 发送 403 `headless_disabled` 置灰标因；绑定集判定更新（routing.rs :292 注释「H3 今天不管 dsh」随之废止）。
4. **send-info**：channels 下发 `headless_dsh_acp`（移动端 composer 通道化消费——零前端改动即生效；若 WB/其他 channel 的下发形态有先例照抄）。
5. **request_permission 透传接线**：按 Task 9 的接入面选择落到对应载荷端点（最小面：挂 `session_question` 载荷族 `available` 机制或审批卡通道——**不新建端点优先**，确需新端点在 server.rs :685-697 注册并补端点级用例）。

**测试清单**：
- routing.rs：`dsh_routes_to_headless_dsh_acp`（翻转主断言——原 `dsh_headless_pending` 用例改写）
- routing.rs：`dsh_wire_name_pinned_headless_dsh_acp`（wire_name 钉值表加行）
- routing.rs：`h3_bound_set_includes_dsh`（绑定集判定）
- routing.rs：`dsh_cli_form_with_live_process_keeps_terminal_route`（CLI 形态 + 活进程 = 在产终端注入保留——既有行为不回归）
- server.rs：`session_send_dsh_app_dispatches_to_dsh_acp_arm`
- server.rs：`session_send_dsh_h3_disabled_returns_403`（接管行为变化）
- server.rs：`send_info_lists_headless_dsh_acp_channel`
- server.rs：`dsh_active_session_send_returns_honest_refusal`（活跃会话如实拒——经端点层）

**验收门**：单测绿；真机 M24（H3 回归：关 → dsh 置灰）+ M25 归 Task 11。

**Commit**：
```bash
git add src-tauri/src/inject/routing.rs src-tauri/src/remote/api.rs src-tauri/src/remote/server.rs
git commit -m "feat(routing): dsh H13 入无头路由——DshAcp 变体 + ⑤b 分派 + send-info 能力位（H3 自然接管）"
```

---

## Task 11：全量回归 + M 系手工验收清单（唯一编号总表）

**目标**：全门禁总跑 + 真机验收 + 文档回填收尾。

**前置依赖**：Task 1–10 全部完成（**含 Task 4b、Task 6a/6b**；探测 gate 全部闭环）。

**Step 1: 全门禁总跑**——
- `pnpm run build:mobile`（**先跑**——产 `dist-mobile/mobile.html`，否则 static_serving_matrix 挂）
- `cd src-tauri && cargo check && cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check`
- `pnpm lint && pnpm build && pnpm test`
- 既有 `#[ignore]` 实机套件不回归（`headless_e2e` 4 ignored / `m9r_e2e` 7 ignored / `monitor::hooks` 6 ignored——列出即可不跑）
- **环境债归因**（不算新账，纪律条款 6）：connection 隔离×2、symlink/junction 1314 族、tailscale 探针、`cmd_shim` flake——遇失败先归因到这四类再判定。

**Step 2: M 系手工验收唯一编号总表**（用户执行；设计 §5.3 与 §9.2 合并去重后的**唯一权威表**——执行时只看这张，两处原文有出入以本表为准）：

| # | 批 | 用例 | 通过判据 | 前置 |
|---|---|---|---|---|
| M18 | P0 | WB 勾选恢复上板（双向） | 勾选 → ≤3s 出卡；取消 → 无卡 | 用户操作 |
| M19 | P0 | zcode 发送恢复 | 探针过 → 回执含 assistant 摘要 + tokens；**审计行真实耗时**（非 0ms） | Task 2 |
| M20 | P0 | codex APP 止血门 | 切档/作答 → 明确「开发中」拒绝（不再现控制台错误）；P1 后该判据翻转为协议臂回执（M20 在 Task 6a/6b 合入后**复验一次**翻转形态） | Task 3 → Task 6a/6b |
| M21 | P1 | codex APP 档位协议切换 | **行为级**：档位改变后工具调用审批行为随之变化（never vs on-request 审批请求面翻转）+ 回执核对三态；**打开态/关闭态两态都验**（T0.1 结论决定打开态可写还是如实拒——按回填判据执行；两腿全败则验只读降级形态） | Task 5/6a + T0.1/T0.2 |
| M22 | P1 | codex APP 中止 | 在飞回合 interrupt 生效（参照活体 1.5s 量级；**仅 daemon 通道可用时提供——不可用时验置灰如实回执**，关闭态在飞回合归 exec resume 路 H4 kill）；queue 滞留项如实「已入队未消费」；两态同 M21 | Task 6b + T0.1 |
| M23 | P1 | codex APP 问答补投 | 补投入队 → APP 消费 → 引导文案 + `screen=null`（**永不显示已作答/已翻转**） | Task 6b/8 |
| M24 | P1 | H3 门扩展回归 | 关：APP 四动作 403 + dsh 发送置灰标因；**CLI codex 模式/问答不受影响**；开：全部恢复 | Task 6a/6b/10 |
| M25 | P1 | dsh ACP 全链 | 非活跃会话 resume+prompt 精确命中（dsh APP 内可见）；活跃会话如实拒；cancel 首验（按 T0.4 结论）；request_permission 展示不代答 | Task 9/10 + T0.3/T0.4 |
| M26 | P0 | WB 端点发现修复 | **修前基线（2026-10-12 00:13 用户复现，本表自含证据链）**：WB 内开关已开、设置页工具已勾选，远程发送仍拒发——回执「端点未启用（channel_unavailable）……端口指纹路：WB 宿主在场，候选 4 个端口双确认 0 命中」，审计 `16:13:27Z result=failed(refused)`（探测报告 §3.1 互证：端点实际在 32462 监听，双确认 0 命中根因 = /health 被 AUTH_REQUIRED 挡）。**修后判据**：同操作 → 端点发现命中（connect 活体）→ 进入投递链；WB 内开关关 → 才报「请开启远程控制」 | Task 4 |
| M27 | P1 | WB 当前会话投递（= 前批 M8 正式执行） | 消息落 APP 当前会话并执行（APP 内可见）；回执含末条 assistant 摘要 + 「已投递至当前会话」标注（**判据落点 = Task 4b 回执标注面**）；sessionId 与卡面不一致时明示 | Task 4 + Task 4b |
| M28 | P1 | WB 他选/已结束会话投递 | 如实回执（writer_occupied / 靶向不一致明示——**判据落点 = Task 4b**）——不谎报成功 | Task 4 + Task 4b |

**P0 批先行验收**（Task 4 完成后即可执行 M18/M19/M20/M26，不必等 P1）；**P1 批在 Task 10 后执行 M21–M25/M27/M28**。

**前批 M 系回归批**（设计 §7 P0 验收门原文 + P1 dsh 路由改动的读侧回归）：M3（zcode 发消息回执卡）/ M6（H8 codex APP queue 发消息）/ M16（zcode resume 回执源）/ **M12（dsh 读侧正文——Task 10 动了 dsh 路由，读侧回归必做：dsh APP 内发一条消息 → 看板卡正文/预览随之更新，照前批 Task 15 表 M12 行执行）**——照前批计划 Task 15 表执行；外加 CLI codex 模式/问答直通手工抽测（Task 3/6a 的直通用例是机器面，此处是目视面）。

**Step 3: spec/计划回填**——设计文档探测回填定稿（T0.1–T0.5 结论已是正文）+ 本计划各任务「实施记录」块 + 前批遗留表交叉更新（L-02〔WB 端点未启用〕与 L-09〔WB cancel〕状态随 M26/M27 结论更新；L-25〔WB 路由乐观 injectable〕不在本批——**登记不修**）。

**Step 4: Commit**：
```bash
git add docs/superpowers/specs/2026-10-11-app-interaction-split-and-write-coverage-design.md docs/superpowers/plans/2026-10-12-app-write-coverage-implementation.md docs/superpowers/specs/2026-09-27-phase2-closure-app-injection-design.md
git commit -m "docs(spec): APP 写通道全覆盖批验收回填 + M18-M28 结论 + 实施记录"
```

---

## 遗留预登记（本批已知不做，防验收时误判）

| # | 项 | 出处 |
|---|---|---|
| 本批-1 | codex steer 插队（在飞检测 + 入口）——协议已活体，**无探测前置**，列 P2 | 设计 §7 P2 |
| 本批-2 | codex 审批协议应答（ServerRequest 三族持连——裁决 32 唯一 carve-out）+ HeadlessApprovalCard channel 门扩表 | 设计 §7 P2 |
| 本批-3 | dsh request_permission 代答按钮（首批只展示） | 设计 §6.1 |
| 本批-4 | WB session/cancel 接线 + 模型选择展示（availableModels 面已实测可读） | 设计 §7 P2；前批 L-09 |
| 本批-5 | codex queue 消费翻转信号（回执卡「排队中→已消费」）——若 Task 8 未消化后端信号面，前端状态位如实「消费确认建设中」 | 设计 §3.7(b) 契约缺口 |
| 本批-6 | zcode `--mode` 档位原语（无此面，裁决 14）；WB mode（Windows 探测证伪）；dsh mode（刻意无）——三家均如实 ⬜ | 设计 §2 结论 2 |

---

## 自审记录（Self-Review，制定时点）

1. **硬约束对照**：① 探测前置——Task 0 立于最前，gate 全部落任务书「前置依赖」栏显式标注（Task 5/6a/6b/8/9/10/11 各处「依赖 T0.x 结论」；T0.1 自身含降级主腿 Step 5——v2 增补）；② 双轨串行——api.rs 触点 Task 2 → 3 → 4b〔条件〕→ 6a → 6b → 7 → 10 排死序 + 头部红线「禁止两个实现子代理并行改 api.rs」（mod.rs 触点 Task 5/9 同款串行提醒）；③ 每任务自包含——目标/文件域（含行锚点）/实现要点（设计 §引用）/测试清单（用例名级）/验收门（M 系）/前置依赖/commit 七件齐；④ 纪律条款入头部九条；⑤ 任务分解 Task 0–11（含 4b、6a/6b——v2 拆分）覆盖设计 §7 全表（P0 四任务 + P1 双轨 + 收尾）；⑥ 头部含状态/分支/引用/执行方式/降级路径表（T0.1 失败 → interrupt 置灰 / 档位写走降级主腿的分支展开为可执行路径——v2 措辞修正）。
2. **锚点核实**：锚点速查表全部条目于 2026-10-12 在工作树逐条 grep/sed 核实（api.rs 七端点、gate.rs 四处、wb_acp.rs 三处、routing.rs 五处、codex.rs 三处、mobile 五处、settings 一处、fixtures 一处、server.rs 路由两处）。
3. **降级路径完整性**：五个 gate 各有「未过时」分支且落到具体任务的具体子项（T0.3 全败是唯一「停下报用户」项——spawn 无法臆写；T0.1 的降级主腿已升为独立探测步骤〔Step 5〕——daemon 路成败与否都验证，两腿全败有显式只读降级出口）。
4. **设计文档问题**（制定中发现，详见交付报告）：§3.2 `api.rs:9571` 锚点偏差（函数本体 9778）；§7 P0「两侧同步」语义需澄清（已按「文案一处 + 键集锁」落 Task 2/3）；§3.4「无头中止端点」未点名（已核为 `/session-headless-cancel` → `session_headless_cancel` api.rs:3756）；§7 codex 轨 gate.rs 探测任务与 Task 5 的顺序未明示（本计划定为 Task 7 后置）；裁决 34a 文案落点的双语要求未写明（本计划补 i18n zh/en）。
5. **测试成本红线**：探测批真实回合次数已限定（T0.1 降级主腿极小回合 ×1 + daemon 路 interrupt 长回合 ×1、T0.2 每载体 1–2 回合、T0.4 一条无害 prompt + 一条长回合 + 一条权限触发——合计约 7–9 次真实回合，全部申报）；单测全部纯核/缝 mock，零真实进程。
6. **v2 修订（2026-10-12 独立审阅 12 条，同日落实——逐条落点见下节「修订记录」）**：探测工具面按本机实测重定（Rust 探针主路 + 阳性对照先行）；未验 #3 升为 T0.1 Step 5；`degraded: Option<String>` 三任务钉名；M12/M26/M27/M28 判据自含；Task 6 拆 6a/6b；新增 Task 4b；interrupt 降级措辞按「关闭态无作用对象」改正。

---

## 修订记录（v2 · 2026-10-12 独立审阅 12 条，同日落实）

| # | 审阅条 | 落点（节） |
|---|---|---|
| P0-1 | T0.1 探测方法重写：PowerShell/.NET 主路被本机实测否决（pwsh 缺席 / PS 5.1 无 `UnixDomainSocketEndPoint` / Node 直连 EACCES·ENOENT 路径坑）→ 主路 = 一次性 Rust 探针（std `UnixStream`，1.70+ 支持 Windows，与生产 socket2 同栈；/tmp 独立 crate 不进 src）+ **阳性对照先行**（自建 AF_UNIX socket 走通收发再判 daemon socket 死活，防假阴性）；Node 降备选且先钉路径形态 | T0.1「方法学前提」+ Step 0/1/1b |
| P1-2 | 未验 #3（跨实例 resume 磁盘既有非活跃线程 + turn）补进 Task 0 = **T0.1 Step 5（降级主腿，daemon 路成败与否都做）**；夹具钉死「新 spawn 裸实例 × 磁盘 rollout UUID」，禁止「同实例 ephemeral 再 resume」充数；T0.2 Step 2 同措辞删除 | T0.1 Step 5 / T0.2 Step 2 / gate 表 T0.1 行 |
| P1-3 | GET 载荷降级字段**钉死字段名 `degraded: Option<String>`**（None=正常），Task 6a 与 Task 7 两侧同名下发、Task 8 消费——防并行/集成期契约漂移 | Task 6a 文件域与测试 / Task 7 要点 3 / Task 8 要点 4 与 api.ts |
| P1-4 | Task 11 前批 M 系回归批补 **M12（dsh 读侧正文）**——Task 10 动了 dsh 路由，读侧回归必做 | Task 11 Step 2 回归批行 |
| P1-5 | WB 投递臂靶向标注无任务归属 → **新增 Task 4b**（wb_acp.rs 回执标注 + Windows new+prompt 单臂，排 Task 4 后；条件性触 api.rs 须在 6a 前串行窗口）；M27/M28 判据落点写明 | Task 4b 全节 / Task 4 要点 5 / M27/M28 行前置 |
| P2-6 | Task 2 文件域与 commit 列表补 `runner.rs`（条件性——kill 缓冲修复的预设落点） | Task 2 文件域 / Commit 注 |
| P2-7 | 环境债白名单溯源改「**本机 2026-10 实跑观测（无文档出处）**」，删「与前批 L-28~L-33 同源」（该出处实核无此四项） | 纪律条款 6 |
| P2-8 | M26 行补修前基线全文（开关开、4 候选 0 命中、审计 16:13:27Z + 根因链）——权威表自含证据链 | Task 11 M26 行 |
| P2-9 | T0.1 补 proxy 复试条款：Rust 直连受阻时，在 **PowerShell 原生环境**（非 Git-Bash 管道）复测 `codex app-server proxy` 后才许判「proxy 不可用」——Git-Bash 负例不能永久封路 | T0.1 Step 4b |
| P2-10 | 锚点表 codex.rs 补目录 `inject/headless/codex.rs`（仓内 4 个同名）；Task 5 内联引用同步 | 锚点表两行 / Task 5 要点 3 |
| P2-11 | Task 6 拆 **6a（模式族：switch/menu 翻转 + GET surface/degraded + H3 门 + 审计词）/ 6b（问答补投 + cancel 臂）**，同串行位两连号 commit；Task 7 标题补「api.rs 触点 · 串行位 4」；6a/6b 文件域补 codex_app.rs（条件性）；Task 9 的 cli_three.rs 补全路径 | 6a/6b 全节 / Task 7 标题 / Task 9 要点 2 / 执行方式串行清单 |
| P2-12 | gate 表 T0.1 行 interrupt 措辞改正：**「仅 daemon 通道可用时提供；不可用时如实置灰」**——关闭态在飞回合来自 exec resume 路（归 H4 kill），协议 interrupt 关闭态无作用对象，不写「关闭态可用」 | gate 表 T0.1 行 / Task 5 要点 4 / Task 6b 要点 3 与测试 / M22 行 |
