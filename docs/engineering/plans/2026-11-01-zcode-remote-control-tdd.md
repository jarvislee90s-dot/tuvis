# ZCode R01–R10 实施计划与执行索引

> **For agentic workers:** REQUIRED SUB-SKILL: 使用 superpowers:subagent-driven-development（沿用用户执行方式）或 superpowers:executing-plans，逐任务执行。每项用复选框跟踪；任务完成经独立 review 再推进，最后做整批设计覆盖审查。

**Goal:** 交付 Tuvis 持久待发队列、单条插队、完整编辑撤回、跨供应商选择、R10 模式切换及凭据复用工程入口。

**Architecture:** 保留现有 Auth/Keychain/Hub/Bridge/CommandSender；新增 ZCode 本地业务队列，官方队列保持独立观察。复用 Codex main 的业务规则，不复制其 IPC transport；控制能力继续使用新鲜状态、逐能力门控和持久幂等。

**Tech Stack:** Tauri 2、Rust/tokio/rusqlite、React 19、TypeScript、Vitest；不新增依赖。

**Spec:** [需求矩阵 R01–R10](../specs/2026-11-01-zcode-remote-requirements-matrix.md)、[设计 §1–9](../specs/2026-11-01-zcode-local-queue-and-controls-design.md)；`docs/MASTER-PLAN.md` 为最高依据。

## Global Constraints

- 工作区 `/Users/jarvis/Documents/tuvis-worktree2`，分支 `codex/zcode-unified-remote`。起点是 `4c4050ec` **加当前已审的 W6 未提交修正**，不是干净 SHA；G0 先冻结，不覆盖现场、不从 main 重开。
- 参考 `origin/main@5c4c7b49b567554f72a6992f2e174f58f8d583ab` 仅用 `git show` 读取。禁止盲 merge/cherry-pick 整批 Codex 改动。
- 本机协议参考 build `3.16.2.8769`；不得升级 ZCode/依赖或修改 `docs/MASTER-PLAN.md`、`AGENTS.md`。
- 编码阶段只用合成数据、内存库、临时目录、假宿主/回环；不读真实保险箱、不 OAuth、不真实发送/停止/审批/切模式、不启动新 app-server。
- 忙态新消息只存 Tuvis；普通发送不自动停止；明确插队只派选中的一条，其他保持暂停。结果未知不自动重发。
- 正文、答案、附件路径、JWT、token、真实链接不得进入日志、审计、Git 或证据。队列正文属于本机业务数据，单独表存储，不能塞进能力/动作审计表。
- 源消息只组装一次；字段/签名不跨任务漂移；旧响应不覆盖新草稿，LAN 环境 ID 生成复用已有 `newActionId()`。
- 所有新信封 clientId 仍使用现有 `PROTOCOL_CLIENT_ID`，与握手同源；手机 deviceId 只作 Tuvis 业务归属/审计，不能拿来替代协议身份。
- 官方 QueueEdit/QueueNow/QueueResume 的 W0 停止门保持；本地撤回/编辑与 R10 不借用这三个能力槽。
- **R10 纳入本批**。具体模式若确有阻断，必须给出源文件/字段/handler/失败断言与支持范围，不能再次只写“可选，NO-GO”。
- 相关中文/英文文案及手册同步。不自动 push、merge、tag、发版；任务按文件白名单提交，不 `git add -A`、不 amend。

## Review Focus

1. ACK 后运行帧尚未到达的空窗：不提前放下一条；见 Q2/Q3 的 `accepted_before_execution_start_keeps_barrier`。
2. 再编辑成功但响应丢失/页面重载：全文与附件可按原请求重取；见 Q1/Q6 的 `edit_response_loss_recovers_same_snapshot`。
3. 停止命令可能写出后 HTTP 取消/重启：同请求不再停一次，不抢发消息；见 Q4/Q7 的 `cancelled_prioritize_does_not_repeat_stop`。
4. 本地与官方队列同时存在，宿主实际 admission 与意图不同：不双发、不默认清空；见 Q3/Q7 的 `unexpected_official_queue_admission_never_resends`。
5. provider/模式在点击后被桌面改变，或 schema 缺字段：CAS/目标确认不猜成功；见 C1/M1 的拒绝/换代/planEnabled 用例。

## 1. 先读顺序与分包

执行 Agent 先读本索引、矩阵、设计、R10 静态证据，再读子计划；不能只读上一次执行总结。

| 顺序         | 任务与文件                                                       | 产物                                                                 |
| ------------ | ---------------------------------------------------------------- | -------------------------------------------------------------------- |
| G0           | 本文 §2                                                          | 冻结继承现场、核对源码与协议证据                                     |
| Q1–Q7        | [待发队列 TDD](./2026-11-01-zcode-local-queue-tdd.md)            | admission、持久队列、FIFO、单条插队、API/SSE、手机交互、真实路由假链 |
| C1–C2、M1–M2 | [供应商与模式 TDD](./2026-11-01-zcode-provider-mode-tdd.md)      | 两级选择与 R10 四模式；M1 依赖 Q2 的新鲜状态投影，C1 不依赖队列 UI   |
| K1–K2        | [凭据与稳定身份 TDD](./2026-11-01-zcode-credential-trust-tdd.md) | 可选稳定签名构建入口、原生凭据回归、真实验收清单                     |
| V0           | 本文 §4–5                                                        | 全量门禁、整批 review、执行总结和未验清单                            |

先完成队列可用闭环再做 C/M；K 可独立推进。不要多个子代理同时修改 `types.rs`、`hub.rs`、`remote/zcode.rs` 或同一个大测试文件。每任务独立交付：实现者 → 新上下文 reviewer → 修正 → 提交；reviewer 不再嵌套派 reviewer。

## 2. G0：冻结基线与证据（只读任务，不伪造 RED）

- [ ] 记录 `git status --short --branch`、`git rev-parse HEAD` 与 `git diff --stat`。核对 W6 四项修正和上轮评审结论；把继承改动与本轮任务分开留账，不能混称本轮实现。发现额外现场修改保留，不擅自归入提交。
- [ ] 为已审继承修正建立独立基线提交或等价可核对快照，记录精确 SHA/文件清单；新任务提交仅包含本任务。当前两份设计和本索引也须进入可交接版本，不能只依赖工作区未跟踪文件。
- [ ] 复跑继承定向：`cargo test --locked --lib services::zcode_control:: --quiet`（src-tauri 下）；根目录跑既有 ZcodeControlPanel/ZcodeQueuePanel/ZcodePendingCard/zcodeControlHook 套件。记录真实结果，不写死历史 641/153 数字为本次结果。
- [ ] 按 [R10 静态证据](../reviews/2026-11-01-zcode-mode-static-evidence.md) 核对 schema、handler、四标签映射、baseRevision、config.mode/planEnabled 序列化及 setMode 的规范化；核对 model registry 的 provider 可用性字段、执行关联字段在订阅中的承载位置。记录 build/文件 SHA-256 与定位，不贴整个厂商 bundle。
- [ ] 创建 `tests/fixtures/zcode-official-bridge/control-extensions.json`：仅合成 ID、目录/模式/关联字段形状、已证映射和来源标识。不得从真实会话手抄正文；缺承载证据记具体字段未证，不猜新 RPC。
- [ ] 在 `docs/superpowers/reviews/2026-11-01-zcode-control-execution-summary.md` 建任务账本。G0 结论应为已核对的 `GO_STATIC / PARTIAL / BLOCKED_BY_PROTOCOL`，与 LIVE_PASS 区分。当前材料对 R10 支持 GO_STATIC；若实际包已改变，只收口受影响工作包，报告差异。

## 3. TDD 与任务验收纪律

每个编码任务严格执行：写失败行为测试 → 跑到预期 RED → 最小实现 → 同命令 GREEN → 独立 review → 提交。下列子计划的测试片段只规定行为，不是可盲贴的完整测试文件；夹具按该文件既有组织补齐。

- 已存在且通过的 R01/R08 回归不伪称 RED；只读取证、文档步骤也不造红灯。新编码仍必须有真正新增的失败断言。
- 一个测试应能拒绝原缺陷/错误接线，不能只检查 DTO/按钮存在。变异验证如需做，使用外部副本，不撤回/重置共享工作树。
- 每任务回报：BASE/HEAD、改动函数与接口、RED 命令与关键失败、GREEN 命令与计数、独立 reviewer 结论、偏离设计的理由、实机状态。
- 真实宿主验收不要求用户辅助编码。先完成可自主的代码/假链/门禁；未实机不能标 PASS，也不能因此只交空按钮。

## 4. V0：整批门禁与 R01–R10 对账

- [ ] 根目录执行 `pnpm test`、`pnpm check`、`pnpm build:mobile`。src-tauri 下执行：

```bash
cargo check --locked
cargo test --locked --features marker-helper
cargo test --locked --tests --features marker-helper,zcode-bridge-evidence
cargo clippy --all-targets --features marker-helper -- -D warnings
cargo clippy --all-targets --features marker-helper,zcode-bridge-evidence -- -D warnings
cargo fmt --check
```

- [ ] `git diff --check`、受保护文件零 diff、依赖增量零；无真实秘密/正文进入 Git、日志或证据，业务队列表按设计单独保存必要材料。记录普通/evidence 两套构建，不能用 feature 假体绿替代普通构建接线。
- [ ] `CHANGELOG.md/CHANGELOG.en.md` 在未发布区按实际交付同步，不改版本或打 tag；手册只说明已接通行为和明确限制。README/能力矩阵不得提前宣传实机未通过功能为全面可用。
- [ ] 新 reviewer 从本索引和最终真实 diff 独立检查下表。不得只读执行者总结或沿用上轮“可合并”。

| 需求     | 任务           | 不得遗漏的验收                                                |
| -------- | -------------- | ------------------------------------------------------------- |
| R01      | K2、Q3、Q7     | 重启授权复用，主动断开暂停，忘记/401；不自动 OAuth            |
| R02      | K1、K2         | 同构建/签名升级/开发重编译分开；有入口不等于钥匙串实机已过    |
| R03/R04  | Q1、Q5、Q6、Q7 | 忙态入队立即列表，零上游；队位/原文、重载不丢                 |
| R05      | Q4、Q7         | 定向停原回合、终态后仅发所选项、其余留队、无重复停止          |
| R06      | Q1、Q6、Q7     | 撤回 CAS、原文附件、冲突草稿、断网恢复编辑快照                |
| R07      | Q3、Q4、Q6     | 停止先暂停、显式恢复 FIFO、无旧 paused 死门                   |
| R08/R09  | C1、C2         | provider 目录/指纹/CAS/确认/缓存与联动 UI 一起闭合            |
| R10      | G0、M1、M2     | 正确四标签、Mode 独立能力、扩权确认、planEnabled、ACK≠生效    |
| 继承功能 | Q7、V0         | U1–U7、八项原能力、正文/附件/问答/审批、单写槽与目标/代际守卫 |

## 5. 最终回报与交回本聊天 review

报告固定为 `docs/superpowers/reviews/2026-11-01-zcode-control-execution-summary.md`。必须包含：

1. 实际 BASE/最终代码 HEAD/文档 HEAD、干净或剩余修改、提交与任务映射；不只给 commit 标题。
2. R01–R10 覆盖表：要求 → 最终文件:行/函数 → 自动证据 → 实机证据或 NOT RUN → 已完成/部分/受阻。R10 必须有单独结论，不能在最后改回可选。
3. 逐任务 RED/GREEN/独立 review 索引、普通与 evidence 门禁命令及实际结果；flake 记用例名和复跑，不以负载猜测代替记录。
4. 明确的设计偏离、外部签名条件、仍关闭的具体能力。未知结果、旧官方队列、设备归属和编辑快照至少逐项检查一次。
5. 真实验收清单：忙态 B/C、指定 C、停止未知、重启、附件回填、两 provider、四模式配置、模式与实际审批效果、三次 Keychain 冷启动；均保留独立状态。

交回本聊天时附报告绝对路径、可复核 diff 范围、门禁摘要及所有未闭合项。Agent 不自动合并/推送；“代码可合并”和“完整产品需求/真实宿主通过”必须分开裁决。
