# TPS 短时抽样 实施报告（终稿 · 含独立复核修复轮）

> 2026-10-24 实施、2026-10-25 完成两轮独立复核修复。分支 `codex/tps-sampling`，当前 HEAD
> `b3ac6792`。本文件由 Task 9 撰写技术部分、SDD controller 补齐「实施过程」（§7）与
> 「独立复核修复」（§8，两轮）后定稿；最终 Ready 判定权在原设计会话复审。
> 依据：`docs/superpowers/specs/2026-10-24-tps-sampling-design.md`（§4 来源表 / §7 验收收口）、
> `docs/superpowers/plans/2026-10-24-tps-sampling-plan.md`。R12 真机验收边界全程遵守：
> 未发送任何真实模型请求、未动用户真实工具配置、未花费任何模型额度。

## 1. 范围与口径摘要

- 新增独立 `services/tps` 域与「近期TPS」区块：近2m/10m/1h/today 采样曲线（默认1h）、
  最新有效完成调用、当前回合（原生 turn/trace 身份，缺键 unknown-turn）与来源状态。
  与 Token/费用域完全解耦（queryKey 前缀 `["tps"]`；Token 区间切换不重发 TPS 查询，反之亦然）。
- 口径（Spec §2/§3 固定）：每点取该点前 120 秒内**完成**的有效调用，每 series ≤20 条；
  点内 TPS = 1000×Σ输出Token ÷ Σ生成毫秒；成功 + 输出>0 + 生成窗>0 + 同调用计数才入速；
  失败/取消/无 F 不进速度（保留候选面与分类计数）；空窗 gap 不画 0；>10 分钟不当当前值；
  事实与点只保留 24 小时；后台 600 秒/可见 60 秒出点、5 秒有界增量读取；租约 90 秒；
  重开总授权换新 runId 断线。三计时方法（stream-observed / client-native /
  native-event-approx）分族显示、不合并曲线。
- 旧 usage Token 账本、水位、采集间隔、费用计算零改动（验收断言四表行数逐表不变）；
  总授权复用 `usage.enabled`，不设第二开关；关闭报 disabled、不删历史。

## 2. 平台/宿主/版本/路由真实支持矩阵

状态四档：`supported`（有正例）/ `needs-live-validation`（路径已实现、待真实宿主样本）/
`insufficient-evidence`（证据不足，不产出）/ `not-provided`（本版明确不提供）。

| 来源      | 平台 / 宿主                                                                 | 状态                                                                 | 路径 / 计时方法                                                                                                                            | 证据指针                                                                                                                                                       |
| --------- | --------------------------------------------------------------------------- | -------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| ZCode     | macOS / ZCode CLI（SQLite `~/.zcode/cli/db/db.sqlite`，列集按真机脱敏证据） | supported                                                            | 原生 model_usage 首点→completed；client-native                                                                                             | 形态：evidence/2026-10-24-model-speed/sqlite-sources.md；离线回放 `tps_native_primary_test`（28 用例）+ 验收链 `tps_acceptance_test`                           |
| DSH       | macOS / Windows 跨平台落盘路径                                              | supported                                                            | 紧凑 dt 流首非空→结算；同消息 outputTokens；重试不可对齐剔除速度；stream-observed                                                          | 形态：dsh-kimi.md + dsh-output-scope-addendum.md；离线回放 `tps_native_primary_test` + 验收链                                                                  |
| Kimi      | macOS / Kimi Code wire.jsonl                                                | supported（含申报限制）                                              | step.end 同条 usage + llmFirstTokenLatencyMs/llmStreamDurationMs；client-native；**SDK 内部重试不可见**（来源限制已声明，不谎称零重试）    | dsh-kimi.md；`tps_native_secondary_test`（23 用例）+ 验收链                                                                                                    |
| WorkBuddy | macOS / 5.8.3 正常路线                                                      | supported                                                            | qua.end 同请求 completion_tokens/(total_cost_ms−ttft_ms)；成功 finish + model/request 身份核对                                             | probes/2026-10-24-model-speed-supplement-macos §4.4/§11.2；`tps_native_secondary_test`                                                                         |
| WorkBuddy | Windows / sdk 会话日志                                                      | insufficient-evidence                                                | 仅 `hasUsage:true` 无数值 usage；**不产出 TPS**，不借旧 CLI 数据凑值                                                                       | supplement-windows §7.5；`tps_native_secondary_test` 缺口态用例                                                                                                |
| Claude    | macOS / OTLP JSON → 本机接收器（127.0.0.1:47181–47190）                     | needs-live-validation（实现完备，待一次点击 + 自然重启后的真实遥测） | 官方 llm_request span、attempt=1、success；first_content_ms 首点，**native-event-approx（原生近似标记）**；生成窗 = duration−first_content | 夹具按 otel-boundaries-addendum 脱敏形态；`tps_claude_ingest_test`（10）+ 接收器实时 HTTP 往返 `tps_sampling_test`（bind :0，绝不占用 47181–47190）            |
| OpenCode  | macOS / opencode v2.0.22                                                    | supported                                                            | 观察插件（AsyncIterable `for await` 消费）→ facts.jsonl → Rust 读取器；stream-observed；无原生回合键（unknown-turn 降级）                  | **R8 隔离活体正例**（tempdir HOME、零用户凭证、内置免费模型一条 start→delta→end，task-6-report.md §5）；`tps_opencode_test`（14）+ 插件单测 9 + 验收链逐字回放 |
| OpenCode  | 2.1.x 宿主（`session.next.*` 词表）                                         | needs-live-validation                                                | 别名兼容 + `properties` 信封防御回退；subscription 返回形态未经实机复核                                                                    | SDK 1.17.13 类型定义（残留安装，仅作别名依据）；sourceState.detail 已申报                                                                                      |
| Codex     | macOS / Windows                                                             | **not-provided**                                                     | metrics 管道已通但无同调用可靠生成窗（token_usage_record 无计时字段；turn duration−turn TTFT 是回合墙钟）；**不造观察值、不暗改公式**      | file-sources.md §5 + supplement-assessment「Codex回合公式」；`tps_native_secondary_test` 缺口态 + 验收链 sourceStates 断言                                     |

「不是七家全支持」：上表即真实边界——supported 5 个路由（ZCode/DSH/Kimi/WorkBuddy-Mac/
OpenCode-2.0.22），needs-live-validation 2 个（Claude 遥测面、OpenCode 2.1.x），
insufficient-evidence 1 个（WorkBuddy-Windows），not-provided 1 个（Codex）。
OpenCode 的 supported 依据是**隔离活体正例**，不是「插件 loaded」；Codex 从未以
回合 TTFT 或 item 窗口凑过 TPS。

## 3. 测量与验收结果（gate 实录）

Task 9 新增两套整链验收（`src-tauri/tests/tps_acceptance_test.rs` 5 用例、
`tests/tps/tps-acceptance.test.tsx` 6 用例），覆盖：原生来源回放→事实→调度拍→点→
查询快照/曲线全链；取消/无 F 不进速度；单源失败不阻塞其它源；旧 usage 四表快照不变；
24h 清理；总授权关闭报 disabled 且历史完整；Codex 缺口态在场；OpenCode Task 6 捕获
记录逐字回放出有效观察（TPS=600=1000×3/5）；前端 Token↔TPS 双向解耦、稀疏点不插值、
gap 诚实说明、过期降级、停用态、来源状态诚实（全文无「服务器基准」）、心跳随可见性起停。

**RED**（Step1 首跑实录）：Rust 侧 `5 tests … 3 failed`（失败均为本任务测试夹具的时间
偏移算术：OpenCode 捕获偏移 −2080/−2197 误写、zcode 候选事实数 3 误记 4）——**无生产
接线缺口**，Step2 无需改动任何 `src/**` / `src-tauri/src/**` 代码；前端首跑 `1 failed`
（夹具把 opencode 标成 active 导致安装按钮不出现，改回 unconfigured）。修正仅落在两个
新测试文件内。

**GREEN + 门禁实录**（2026-10-24 本机，全部真实退出码）：

| 门禁（brief Step3 逐字命令）                                                                                                                                                                                                                                                          | 结果                                                                                                                 |
| ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------- |
| `pnpm check`                                                                                                                                                                                                                                                                          | exit 0（format + lint + build 全过；「chunks larger than 500 kB」为既有提示性告警，非失败）                          |
| `pnpm test -- tests/tps`                                                                                                                                                                                                                                                              | exit 0；实跑全量 vitest：**Test Files 185 passed (185)，Tests 2209 passed / 5 skipped (2214)**——本轮无基线失败需标注 |
| `cargo test --manifest-path src-tauri/Cargo.toml --lib services::tps`                                                                                                                                                                                                                 | **20 passed; 0 failed**（2984 filtered out）                                                                         |
| `cargo test … --test tps_store_test --test tps_native_primary_test --test tps_native_secondary_test --test tps_claude_ingest_test --test tps_sampling_test --test tps_acceptance_test --test usage_two_round_incremental_test --test usage_ledger_test --test real_db_isolation_test` | 全绿：16 + 28 + 23 + 10 + 25 + **5（新验收）** + 6 + 9 + 1 = **123 passed; 0 failed**                                |
| `cargo fmt --manifest-path src-tauri/Cargo.toml --all -- --check`                                                                                                                                                                                                                     | exit 0                                                                                                               |
| `cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings`                                                                                                                                                                                                      | exit 0（本任务新测试文件两处 `useless_vec` 当场修掉，未豁免）                                                        |
| `node scripts/check-error-codes.mjs`（附加）                                                                                                                                                                                                                                          | exit 0（310 码 / errors 段 311 键）                                                                                  |
| `pnpm check:i18n`（附加，未动 locale）                                                                                                                                                                                                                                                | exit 0（2504 键对齐）                                                                                                |
| `cargo test --test tps_opencode_test`（附加）                                                                                                                                                                                                                                         | 14 passed; 0 failed                                                                                                  |

假时钟纪律：调度/清理/租约全部时间注入（`Sampler` 纯状态机 + `produce_point(at_ms)`），
未真等 1 小时；测试库全部 tempdir 用例私有库 + 影子 home；端口探测只用 bind :0。

## 4. 未完成/未通过事项（如实申报）

**R12 未执行的真机请求（本轮不冒认通过）**：DSH/ZCode/Kimi/WorkBuddy/Claude 的
「一次正常请求」入口确认**本轮未执行（需用户在正常使用中自然产生样本）**——未通过
用户凭证发送任何模型请求、未在 `~/.claude` / `~/.config/opencode` 等真实工具配置安装
任何东西。Spec §7 明示「原生来源用已取得真实结构样本做离线回放和影子存储验收，无需
用户重跑七工具」；本报告按该口径验收，不额外宣称真机在线验证。

**Windows 未运行清单（留待最终平台验收）**：全部 Windows 检查未运行——注入/屏读相关
面、WorkBuddy Windows 正常路线联调、Windows 用户私有 ACL（安装记录；当前 Unix 0600
已实现，Windows ACL 为 cfg+TODO）、接收器/统计键在 Windows 下的路径与权限、两平台
「Token/TPS 解耦、短窗出点/空窗、成功/取消、窗口显隐开销、一次配置确认及自然重启」的
Windows 侧复验。**不得据本报告声称跨平台通过。**

**真机对齐待办（独立复核修复轮后更新）**：

1. ~~WorkBuddy `qua.end` 行前缀拼写~~ **已解决**（R4，2026-10-25）：解析器支持真机
   `[M/D/YYYY, h:mm:ss AM|PM.mmm]` 前缀（本地时区、DST 取较早），真实前缀形态夹具
   重放至观察→120s 点整链，RFC3339 保留兼容；
2. ~~Kimi 行级 `time` 字段 JSON 数值类型~~ **已解决**（独立复核确认 12,328 条现有记录
   全为整数，无需重测；R5 同时修正了顶层 `llm.request` 消费——真机模型归属与可见重试
   排除自此生效）；
3. Claude `success` 属性拼写漂移——独立复核确认本机五条 `llm_request` 均为 `success=true`
   且安装包实际写该属性，当前无需重发请求验证；接收器对漂移不静默缩样本（诊断在位），
   保留为观察项；
4. OpenCode 2.1.x 宿主别名路径与 subscription 返回形态的实机复核（2.0.22 已闭环）；
5. OpenCode usage 口径：`session.step.ended` 自带值若被宿主放成 message 级累计，多步调用
   Σ 会高估——需用户自然使用回填复核（sourceState.detail 已申报）。

以上 3–5 不影响离线验收，上真机前核对；本报告不据此宣称真机在线验证。

无「未通过」测试：本轮门禁全绿；以上均为**边界申报**而非失败项。

## 5. 平台限制（已知且如实上板）

- Codex：本版不提供 TPS（缺口标记 `insufficient-generation-timing`，来源状态单列，
  前端文案「本版不提供TPS（缺同调用生成窗）」）；既有 Token/费用能力不受影响。
- WorkBuddy Windows：insufficient-evidence，不产出 TPS，不借旧 CLI 数据凑值。
- Kimi：SDK 内部重试不可观测（来源限制声明，不谎称零重试）。
- Claude：native-event-approx = **原生近似**（first_content 首点），tooltip 带「原生近似」
  标记，不称首非空 delta；需一次统计配置安装 + 自然重启（Tuvis 不替用户重启）。
- OpenCode：无原生回合键（unknown-turn 降级）；purpose 恒主任务（辅助标题/compaction
  不在观察词表）；tool_ms 恒空；插件分发为纯 JS 的 .ts（宿主不装载 .mjs，隔离实测）。
- 单选题形态的高亮层不可见等屏读类边界与本域无关；本域不上报任何未测得的速度。

## 6. 文档落点

- **用户手册**：`docs/user-manual/zh/11-usage.md` 新增「近期 TPS（短时抽样）」小节
  （抽样非全量 / 不是服务器基准 / 只含上报思考 / 计时方法分族 / 空窗与更新时间 /
  一次配置与自然重启 / Codex 缺口 / 当前边界）；`docs/user-manual/en/11-usage.md`
  同位镜像「Recent TPS (short-window sampling)」，结构逐条对齐。
- **CHANGELOG**：`CHANGELOG.md` 与 `CHANGELOG.en.md` 各在 `## [Unreleased]` 下新增
  `### Added` 条目（两份各自单语、条目同序同内容面），含升级须知式说明：新独立区块、
  一次点击安装统计配置/观察插件、自然重启生效（不自动重启）、卸载只处理自有项。
- _*README*.md 与 docs/capabilities.json 未动_*（R14）：按项目纪律留待发版批处理
  同步（发版时：README 功能概览 + 能力矩阵 + capabilities.json 三处随发版更新）。
- 未触碰 `docs/MASTER-PLAN.md`、未发布、未打 tag、未合并。

## 7. 实施过程（controller 补齐）

> SDD（superpowers:subagent-driven-development）过程账本全文在
> `.superpowers/sdd/2026-10-24-tps-sampling-plan/progress.md`（git-ignored 工作区）；本节为其归档要点。

### 7.1 分支与最终 HEAD

- 分支 `codex/tps-sampling`；merge-base = `35698a33`（origin/main 2026-10-24，PR #181 合并点，
  即「实施前同步主分支」的落点）。其上先 cherry-pick 设计会话的 7 个文档提交
  （`f492e20b..817f4f2f`），实现在其上推进。
- **最终 HEAD：`ef4a77bf`**（`fix(tps): final-review polish — busy timeout, lock-file mode, dead assertion`）。
- 主分支同步方式备注：本机 `/Users/jarvis/Documents/tuvis` 克隆对象库损坏（旧历史缺 tree），
  常规 fetch 报 unresolved deltas；经健康克隆（tuvis-worktree2，代理同步远端）打自包含
  pack 导入并 `update-ref` 推进 main/origin/main 至 `35698a33`，`ls-remote` 核对一致（裁决 R5）。
  全分支 diff 仅在 merge-base..HEAD 内遍历，未受损坏影响。

### 7.2 逐任务提交与评审轮次（9 任务 + 3 修复波，16 个代码提交）

| 任务                         | 提交                                                                                       | 评审结论                                                        |
| ---------------------------- | ------------------------------------------------------------------------------------------ | --------------------------------------------------------------- |
| T1 值对象/点计算             | `48ae4c40`（+clippy 微修 `f37959e1`）                                                      | ✅ 0 Important（10 项 spec 未钉处自裁经评审逐条判定为忠实读法） |
| T2 三表/24h 保留             | `8194173c`                                                                                 | ✅ 0 Important                                                  |
| T3 DSH/ZCode 读取            | `dbba9e95` + 修复 `04e63656`（zcode 水位不越过新鲜 running 行）                            | ✅ 1 轮修复后闭环                                               |
| T4 Kimi/WorkBuddy/Codex 缺口 | `85b775fb`                                                                                 | ✅（真实 Windows 验证推迟已申报）                               |
| T5 Claude 遥测/一次配置      | `e58c614f` + 修复 `e81042a3`（写入加固/严格 success 门控）+ `f1ad10cd`（非字符串形状冲突） | ✅ 2 轮修复后闭环                                               |
| T6 OpenCode 观察插件         | `6d84a6ec`                                                                                 | ✅（隔离活体正例 → supported；未触用户配置）                    |
| T7 调度/查询/租约/IPC        | `9b15377b` + 修复 `0b5a10c6`（接收器非阻塞/字符边界截断）                                  | ✅ 1 轮修复后闭环                                               |
| 修复波 A（跨任务）           | `3bd0fae6`（errcode detail 迁移，`check-error-codes` 85→0）                                | ✅ 纯文本迁移经逐 hunk 核验                                     |
| T8 前端独立区块              | `bac8973f` + 修复 `db6fd20f`（跨窗口唯一 view_id）                                         | ✅ 1 轮修复后闭环                                               |
| T9 验收/双语文档/报告        | `3bece242`                                                                                 | ✅（生产代码零改动）                                            |
| 整分支终审                   | —                                                                                          | **Ready = Yes**：0 Critical / 0 Important / 7 Minor             |
| 修复波 B（终审三小项）       | `ef4a77bf`                                                                                 | ✅ 全部 ADDRESSED、无新破坏                                     |

过程纪律：每任务独立实现者子代理 + 独立评审子代理 + 修复回环（共 6 轮修复）+ 账本记录；
修复一律不进 controller 之手；「✅」指 spec 合规且无未处置的 Critical/Important 发现。

### 7.3 终审对账本延后项的分拣

过程账本累计 ~40 条 `minor (deferred)`。终审逐条分拣：**0 条升为必须修**；
6 条 MOOT（被后续任务显式消化：占位 sourceStates 覆写、build_point_with_trigger 导出、
tps_state 键前缀注册表、refresh 禁用原因、合成 unconfigured 态、脱敏标记归一等）；
其余 PARK 为打磨或推测性前提，已并入 §4「真机对齐待办」与下述下一批候选。

**下一批候选（终审 PARK，不阻断交付）**：detail/diagnostics 机器前缀的 i18n 映射
（中英混排直达 UI 的观感问题）；`fc≥d` 仍设 first_token_at 的守卫；kimi/workbuddy/opencode
三处非饱和减法改 `checked_sub`；`runtime_home()` 解析失败返回 Err；T9 验收加固七条
（空集真空通过、tps_point 侧清理断言等）；T8 打磨清单；T7 busy 消耗限频锚点等。

### 7.4 关键裁决（R1–R14，全量见账本）

- R1 采集域安全：用户在用的 `pnpm tauri:dev` 监视 tuvis-worktree2 源根，不在本树——不杀应用；
  全部测试临时目录/影子 home，禁触 `~/.tuvis` 真实账本。代价：依赖测试隔离纪律兜底。
- R2/R5 对象库损坏与主分支同步：见 §7.1；禁全库 git 考古。
- R3 本 harness 子代理无模型选择参数：九任务全部由会话模型承担（技能的模型分级不可用）。
- R4 分支名遵循计划 Global Constraints「分支codex/」→ `codex/tps-sampling`。
- R6 修正 dispatch 误指：`docs/superpowers/research/2026-10-24-model-speed/*.rs` 不存在
  （目录本空，早期 ls 误读）；复用目标 = `src-tauri/src/services/usage/collectors/*.rs`。
- R7 评审 Minor「缺 success 属性默认成功」升级进 T5 修复（spec §4「齐全」字面要求）。
- R8 OpenCode 活体验证只允许隔离环境（tempdir HOME、零用户凭证）——结果在边界内取得
  真实 start→delta→end 正例，未动用户配置、未花用户额度。
- R9 错误码优先复用；R10 允许计划外新文件 `services/tps/receiver.rs` 承载回环接收器；
  R11 errcode 批量修复波（棘轮实际会统计 `format!` diagnostics 串——修正了「门禁忽略
  diagnostics」的预设）。
- R12 真机请求不授权（见 §4 申报）；R13 报告分工（本节即 controller 部分）；
  R14 发版同步项（README/capabilities.json）留待发版批处理。

### 7.5 终态门禁（ef4a77bf 复核）

修复波 B 后：`tps_claude_ingest_test` 28 + `tps_sampling_test` 25 + `tps_native_secondary_test`
16 = 69/69 绿；`cargo test --lib` 2966 绿；`cargo fmt --check`、`cargo clippy --all-targets
-- -D warnings`、`node scripts/check-error-codes.mjs`（exit 0）全绿。其余门禁终态见 §3 表
（`pnpm check` exit 0、全量 vitest 2209 passed / 5 skipped、9 套 cargo 集成 123 passed）。

## 8. 独立复核与修复（2026-10-25，两轮）

### 8.1 第一轮：R1–R11

原设计会话独立复核（`35698a33..095a11f4`）判定 **Ready = No**，提出 R1–R11（报告：
`docs/superpowers/reports/2026-10-25-tps-sampling-independent-review.md`）。controller 逐条
技术核实（11/11 坐实）后按指定顺序修复：R1/R2/R3 → R4/R5/R7 → R6/R8/R9/R10/R11，
每条一提交、反例测试先行（TDD RED→GREEN），不依赖自造夹具全绿。修复期间子代理通道
故障（API key 过期），批次 1–3 由 controller 在主会话按 executing-plans 降级路径亲自实施；
scoped re-review 由恢复后的子代理完成。

| R                | 提交       | 反例/判别测试                                                                                                                                                       | 结果                                   |
| ---------------- | ---------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------- |
| R1 Windows 编译  | `06959141` | `cargo check --target x86_64-pc-windows-gnu --lib`：修复前 E0425×2，修复后 Finished                                                                                 | ADDRESSED                              |
| R2 所有权保留    | `38b5c767` | `reinstall_preserves_first_takeover_ownership_for_uninstall`、`reinstall_keeps_first_replaced_user_plugin_for_uninstall`（旧代码重装回执记自有值/丢 replaced 事实） | ADDRESSED                              |
| R3 增量读+锁     | `af8b1620` | `read_tail` 单测（6 MiB 前缀只读追加字节、上限截断）、`source_io_runs_without_holding_the_db_lock`、kimi 10 万坏行前缀零重解                                        | 首轮修复；第二轮复审判需收尾 → 见 §8.2 |
| R4 WB 真实前缀   | `aa7f1c06` | `wb_real_timestamp_prefix_flows_to_point_and_rfc3339_stays_compatible`（真实前缀→观察→120s 点；坏前缀诊断；RFC3339 兼容锁）                                         | ADDRESSED                              |
| R5 Kimi 顶层请求 | `4682f3c7` | 夹具改真实顶层形态后 `kimi_retry_ambiguous_requests_strip_speed`、`kimi_nested_step_timing…` 红→绿                                                                  | ADDRESSED                              |
| R6 回合联合身份  | `9a8f017b` | `current_turn_groups_by_session_and_host_not_bare_turn_number`（旧代码 sample_count=4）                                                                             | ADDRESSED                              |
| R7 ZCode 近期尾  | `4befb588` | `zcode_cold_start_and_backlog_recover_recent_tail_first`（三场景：冷启动/积压/新鲜 running 保护+收尾补读）                                                          | 首轮修复；第二轮复审判需收尾 → 见 §8.2 |
| R8 DSH profile   | `dc656e1d` | `dsh_missing_fields_yield_diagnostics_not_zero` 重写：无 profile 完整消息=有效（ProviderReported）、单时刻突发仍排除                                                | ADDRESSED                              |
| R9 TTFT 分母     | `78fa63e6` | `ttft_denominator_is_independent_of_tps_validity`（TPS n=1 / TTFT n=2 / avg 500）                                                                                   | ADDRESSED                              |
| R10 持久安装事实 | `a996a3a5` | vitest：active/read-failed 已装来源首挂载即可卸载、installed=false 才显示安装；query 单测（opencode await-restart、token 组装）                                     | 首轮修复；第二轮复审判需收尾 → 见 §8.2 |
| R11 计划节奏断线 | `4de933fa` | vitest 8 分段用例（两评审例、合法后台、抖动、manual、gap、模式切换、runId、旧行 null）+ 调度器节奏测试 + 迁移补列测试                                               | 首轮修复；第二轮复审判需收尾 → 见 §8.2 |

**第一轮 controller 侧 scoped re-review**（`095a11f4..4de933fa`）曾判 R1–R11 全部
ADDRESSED；原设计会话随后的**修复轮复审**（`095a11f4..a2079f35`）确认 R1/R2/R4/R5/R6/R8/R9
关闭，但判定 R3/R7/R10/R11 尚需收尾（均 P2）——第一轮 re-review 的「11/11」结论**不成立**，
以设计会话复审为准。

### 8.2 第二轮：R3'/R7'/R10'/R11' 收尾 + 修复 diff 自身引入项

复审指出（报告 `docs/superpowers/reports/2026-10-25-tps-sampling-fix-rereview.md`）：

| 项   | 复审发现                                                                                                  | 收尾提交   | 反例/判别测试                                                                                                                                                                                                               |
| ---- | --------------------------------------------------------------------------------------------------------- | ---------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| R3'  | 超大行/帧回退 `read_tail(usize::MAX)` 取消预算；`decode_all` 解码量无预算；24 MiB 无换行行每轮重读整余量  | `e52a2be5` | `dsh_oversized_plain_line_is_skipped_in_bounded_chunks_and_recovers`（持久 `oversized_upto` 扫描态、每轮 ≤8 MiB）、`dsh_zstd_frame_beyond_decode_budget_is_skipped_not_decoded_wholesale`（Decoder+take 8 MiB，炸弹帧越过） |
| R7'  | 保护行在陈旧区开头把起点拉回 0（升序 LIMIT 200 读不到近期行）；LIMIT 64 探针被僵尸填满可漏新鲜 running 行 | `8425bc58` | `zcode_recent_completion_behind_stale_history_is_recovered_same_round`（独立 DESC LIMIT 100 补采 + 尾读 ≤200−补采）、`zcode_truncated_probe_cannot_cross_fresh_running_row`（SQL MIN 带新鲜度，无截断）                     |
| R10' | 安装按钮绑 unconfigured；卸载后保留 24h 历史时 active/no-sample/read-failed + installed=false 无任何入口  | `9c95d1bd` | 三状态 + installed=false 皆有安装入口；完整 UI 流程 已装→卸载（历史保留）→安装→重装→卸载入口回来；动作后 invalidate `["tps"]`                                                                                               |
| R11' | manual 点遮蔽缺失计划锚点（0→manual@50s→120s 判单段）                                                     | `a1985e70` | 锚点链（scheduled/startup）独立成段，manual 为浮点只附着不桥接；可见/后台两反例 + 原八用例                                                                                                                                  |

第二轮 scoped re-review 判四项 ADDRESSED，同时指出修复 diff 自身引入 NB1（Major：骑界普通行被
误判超大行静默丢弃）、NB2（Major：zstd 压缩体 ≥8 MiB 的帧无有界续扫）、NB3/NB4（Minor），
随即收尾：`e72168b4`（起扫须整块无换行 `offset == base`，骑界普通行回归测试）、`d80d2976`
（zstd 帧扫描态：帧尾不可知时水位停帧起点、`oversized_upto` 作扫描位，只扫魔数不解码；
仍在写的帧等待）、`416764a1`（收缩守卫覆盖 `size < oversized_upto`）、`1f916e93`（浮点附着
前跑同一 gap/缺计划点检查，否则自成一段）。NB re-review 判四项 ADDRESSED，再点名两 Minor
并已收尾：`cf70fbd0`（zstd 魔数扫描步进保留 3 字节重叠）、`b3ac6792`（浮点前缘插入同样检查）。

**已申报的固有残留**（不作缺陷，已在代码/报告如实标注）：压缩体内偶合 `28 B5 2F FD` 的魔数
误判（与既有 `frame_offsets` 同风险等级）；跨读取窗的巨帧内容无法跨轮解码、按越过处理并留
诊断；R7' 反例一「同轮含最新尾行」与「补采 + 尾读 ≤ 200」在尾区恰 200 行时算术不相容——
测试断言意图（保护行同轮补采、近期行不被陈旧行挤占、前台 5 秒节奏内下一轮即补齐）。

**第二轮终态门禁（时点 HEAD `b3ac6792`）**：`pnpm check` exit 0；`cargo test --lib` 2972 passed；
七套 TPS 集成 137 passed；`cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`
exit 0；`cargo check --target x86_64-pc-windows-gnu --lib` Finished（Windows **编译**通过；
真机运行/ACL 仍未执行）；`node scripts/check-error-codes.mjs` exit 0；vitest tests/tps +
tests/lib 91 passed；`pnpm build` 通过。

**另登记（复审范围外、原有 P2，单独跟踪，本轮未修）**：`install.rs` 部分卸载冲突后再卸载把
已恢复键当用户改动、安装记录无法了结（复审隔离复现于
`/Users/jarvis/Documents/tps-review-install-afc3k5a2/repro.rs`）。

### 8.3 第三轮复审（merge-review 四项）与收尾

设计会话 merge review（`docs/superpowers/reports/2026-10-25-tps-sampling-merge-review.md`）
判定 9 项关闭、R10/R11/B1/B2 通过，另留四个 P2 阻塞；四项修复（基线 `fdf6daeb`）：

| 项                 | 提交                    | 反例/判别测试                                                                                                                                                                                                                                                                                                         |
| ------------------ | ----------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1 DSH 整批解码预算 | `d2e041d9`              | `dsh_batch_decode_budget_bounds_whole_batch_not_per_frame`（3×4 MiB 帧：恰 2 帧本轮、第 3 帧下轮恢复）                                                                                                                                                                                                                |
| 2 DSH 跨界小帧等待 | `19cacde7`              | `dsh_straddling_small_frame_is_pending_then_parsed_not_skipped`（PendingFrame + 读界倍增 8→16→32→64 MiB）；炸弹对照与 64 MiB 降级诊断用例                                                                                                                                                                             |
| 3 ZCode 后台反例   | `93a47776` + `e048a6b3` | `zcode_tail_read_takes_newest_rows_first_surviving_background_cadence`（尾读 DESC 最新优先，+600s 推进断言）；**新鲜 running 头部阻塞场景**由双字段游标 `{watermark, opportunistic_until}` 机会采样收口（`zcode_opportunistic_sample_beyond_fresh_running_head`：502 同轮采到、水位不越 rowid 1、收尾后补采 rowid 1） |
| 4 部分卸载收敛     | `99e7477f` + `05cb382b` | `uninstall_retry_recognizes_already_restored_keys_and_converges`（已恢复臂 Value 逐字比对，重试收敛、记录清除）+ 非字符串数值恢复值收敛用例 + OpenCode 同构用例                                                                                                                                                       |

第三轮 scoped re-review（两段）：`fdf6daeb..99e7477f` 判 1/2/4 ADDRESSED、3 部分解决（第二
条款未实现 + Minor：非字符串恢复值不收敛）；`99e7477f..05cb382b` 判两项均 ADDRESSED、无新
Critical/Important 破坏（独立推演机会采样全链路、四个适配测试水位断言保留、预算上界与
游标向后兼容核对）。已申报固有残留：机会采样窗含僵尸行（安全阀口径越过）、窗与补采区重叠
由稳定键去重兜底、非字符串现值非恢复值仍落冲突臂（形状改动=用户后改）。

**第三轮终态门禁（HEAD `05cb382b`）**：`pnpm check` exit 0；`cargo test --lib` 2972 passed；
七套 TPS 集成 146 passed；`cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`
exit 0；`cargo check --target x86_64-pc-windows-gnu --lib` Finished（Windows **编译**通过；
真机运行/ACL 仍未执行）；`node scripts/check-error-codes.mjs` exit 0；vitest tests/tps +
tests/lib 91 passed；`pnpm build` 通过。

### 8.4 第四轮复审（第四轮 review 两项）与收尾

第四轮复审（`docs/superpowers/reports/2026-10-25-tps-sampling-fourth-review.md`）确认
merge-review 四项中「普通压缩小帧跨界、部分卸载重试」关闭，另留两项 P2；修复（基线
`9b973fe2`）：

| 项                          | 提交       | 反例/判别测试                                                                                                                                                                                                                    |
| --------------------------- | ---------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| F1 DSH 解码预算按会话重置   | `c27e32aa` | `dsh_decode_budget_is_shared_across_sessions_within_one_pass`（4 会话各 ~3 MiB 解码：单次调用只消费前两帧、其余 deferred + 诊断，下轮继续）；非整齐余量 7 MiB×2（第 1 帧完整、第 2 帧余量不足 → 检查点保留非炸弹，下轮整帧恢复） |
| F2 ZCode 机会游标越过运行行 | `cae70010` | 三行反例（1/2 running、3 completed）+600s：rowid 2 原地完成后同轮成为有效样本且水位不越 rowid 1；交错完成、相同 completed_at（(t,rowid) 复合游标）、旧游标升级（`opportunistic_until` 字段被忽略不重释）用例                     |

第四轮 scoped re-review：F1/F2 均 **ADDRESSED**、无新 Critical/Important（预算一次建立逐层
传递 grep 核实；三结局区分逐字对齐复审要求；复合游标判别力与旧游标不重释经独立推演；
`tps_native_primary_test` 45 项亲跑全绿）。两条不阻断 Minor（zcode 机会谓词注释漂移、deferred
单复数）已随手收在 `8e086199`。已申报取舍：真炸弹在非首位帧位次晚一轮分类（正常帧绝不误判
丢弃优先）；写入端时钟偏移可能使晚完成行错过机会窗（主水位幂等扫回）。

**第四轮终态门禁（时点 HEAD `cae70010`）**：`pnpm check` exit 0；`cargo test --lib` 2972
passed；七套 TPS 集成 152 passed；`cargo fmt --check`、`cargo clippy --all-targets --
-D warnings` exit 0；`cargo check --target x86_64-pc-windows-gnu --lib` Finished（Windows
**编译**通过；真机运行/ACL 仍未执行）；`node scripts/check-error-codes.mjs` exit 0；vitest
tests/tps + tests/lib 91 passed；`pnpm build` 通过。

**Ready 结论**：四轮复审的全部指名问题（R1–R11 及其修复引入项、merge-review 四项、第四轮
两项）均已修复并经 controller 侧 scoped re-review；**最终 Ready 判定权在原设计会话复审**
（复审通过前不合并、不推送、不打 tag）。
