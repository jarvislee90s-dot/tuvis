# ZCode R0/R1 复审报告（规格合规 + 代码质量）

- 复审对象：`4cc69771`（R0）、`3c5007b6`（R1），基线 `6cd667e9`
- 复审日期：2026-10-29；方式：只读静态审查 + 反事实测试（临时回退后已还原，`git status` 干净）
- 依据：`docs/superpowers/plans/2026-10-29-zcode-session-ui-repair.md` §Global Constraints/§Review Focus/§1/§4(R0,R1)/§5/§7、`docs/superpowers/reviews/2026-10-29-zcode-r0-wire-evidence.md`

## 1. 结论摘要

**规格层面不通过：阻塞 1 项、重要 5 项、次要 8 项。**（2026-10-29 修复轮：B1/I1/I3/I5/M1/M3/M4/M5/M6/M7/M8 已处理，见文末 §6；I2/I4 保留为后续任务。） 代码层面 R0/R1 的功能实现与门禁均绿（见 §5），但 R0 对现场症状的根因归因与证据文档冲突且未经证实，属计划 §2/§4-R0 明令禁止的“把审批问题归到未证原因”。修正该条与 RED 记录口径后可进入 R2。

## 2. 逐条覆盖表

| 计划要求 | 代码位置 | 测试名 | 判定 |
|---|---|---|---|
| R0-1 安装 build/合同取证（审批、队列、ACK 字段与来源定位） | `reviews/2026-10-29-zcode-r0-wire-evidence.md` §1–§4 | —（文档） | 部分：安装包侧充分且给到 asar 偏移；**公开源码侧完全缺失**（计划要求“分别记载”） |
| R0-2 RED `permission_snapshot_is_retained_with_original_options` | `pending.rs:830` | 同名 | **部分：不是 RED**（见 B1/I1） |
| R0-2 RED `queue_snapshot_and_delta_preserve_input_identity` | `projection.rs:750` | 同名 | 满足（新 API，RED=编译失败；身份/次序/整键替换/旧代际/换代均有断言） |
| R0-2 断言原 interaction/option/input 身份保留、旧代际样本丢弃 | `pending.rs:830,900`；`projection.rs:750` | 上述 + `old_generation_permission_sample_is_dropped` | 满足（旧代际拆到独立测试） |
| R0-2 不得手造 expected code 迁就实现 | `pending.rs:777-826`、`projection.rs:726-748` | 同上 | 满足（夹具字段与证据文档 §2/§3 对齐，无 expected 码） |
| R0-3 最小修正支持已核实形状 | `pending.rs:563,599` | `malformed_sibling_option_does_not_drop_the_card` | 部分：permission 已放宽；`detail`（Bash 正文，证据 §5.2#8）仍未投影；`parse_questions` 同步改但无测试 |
| R0-3 错误只做允许列表分类 | 本批未改（`pending.rs` 冻结闸原样） | 既有 `permission_answer_rejects_*` | 满足（未扩大允许面，`allowAlways/fullAccess` 仍 `forbidden_option`） |
| R0-3 未知形状保留安全禁用与可诊断原因，不隐藏只读正文 | `pending.rs:603-618`；`ZcodeControlPanel.tsx:181-200` | `malformed_sibling_option_*`、`malformed_queue_shape_*` | 部分：卡片级有 `unsupported_protocol`；**选项级静默丢弃无任何诊断** |
| R0-4 记录具体 RED | 无存档文件 | — | 部分：仓库内无 RED 日志；且经反事实验证有一条命名测试不成立 |
| R1-1 `CapabilitySource` 继续复用 | `types.rs:1055`（未改）；`hub.rs:73` 新增 `SessionCapabilitySource` | — | 满足 |
| R1-1 `apply_relay_session` 新增本帧 `CapabilityView` 入参 | `hub.rs:1417-1422` | — | 满足 |
| R1-1 source 捕获与 `session_capabilities` 一致 | `hub.rs:1897-1915`、`2258-2261` | `first_*_without_refresh` | 满足（GET 与 SSE 共用同一闭包，无第二套实现） |
| R1-1 现算在 Hub 状态锁外 | `hub.rs:2039-2042`（锁内只 `clone` Arc）、`3141` | 同上 | 满足；代价见 I3 |
| R1-1 发布前锁内复核 generation/Paired | `hub.rs:1426-1428` | 既有降级测试 | 满足 |
| R1-1 debug evidence 会话/代际限制不被绕过 | `evidence.rs:95-99`（键=session+generation）；`hub.rs:1906` | 证据构建 117 项全绿 | 满足（SSE 只命中同会话同代际许可） |
| R1-2 RED `first_question_after_empty_get_is_answerable_without_refresh` | `hub.rs:6573` | 同名 | 满足（反事实回退后实测失败） |
| R1-2 RED `first_permission_after_empty_get_is_answerable_without_refresh` | `hub.rs:6643` | 同名 | 满足（同上） |
| R1-2 两会话交替、不借用最后 GET 矩阵 | `hub.rs:6706` | `live_frames_compute_...` | 部分：只覆盖一次切换；source 与会话无关，无法证伪“按会话计算”（M4/M5） |
| R1-2 ready=false 必须全关 | `hub.rs:1430-1436`、`6706` 尾段 | 同上 | 满足（断言整份 `CapabilityView::default()`） |
| R1-3 前端 RED：不挂旧 Question/Approve、不调 `/session-question`/CLI answer/旧 `/session-send`；Claude 正常 | `SessionDetail.tsx:526-527,1311,1319` | `ZcodeSessionIntegration.test.tsx:308,323`；`zcodeControlHook.test.tsx:282`（既有） | 满足（`/session-question` 前缀同时覆盖 GET 与 answer POST；`/session-send` 由既有回归覆盖） |
| R1-3 只消费原生 pending；连接失败保留正文与就地原因 | `SessionDetail.tsx:1319`；`ZcodeControlPanel` 未改 | `zcode_reading_survives_control_failure` | 满足 |
| R1-4 保留“完整模型目录经实时帧不丢失”回归 | `wire_e2e.rs:2736` | `session_broadcast_keeps_full_catalog_across_live_delta` | 满足（实测通过） |

**超范围/做多：** R0 落地了 R2 的 `QueueView/QueueItemView` 与投影（`types.rs:852,869`、`projection.rs:299`）。因 R0 命名测试需要该面，判为“可接受的前置”，但 `SessionView`（`types.rs:880-889`）尚未接线，当前无消费方。

## 3. 问题清单

### 阻塞

**B1（规格/归因）** `pending.rs:590-598` 注释 + `4cc69771` 提交信息称“现场『审批没出现』的直接原因是单个坏选项吞整卡”。
证据文档 §6 明确“零实连、零帧捕获”，§5.2#7 指出真实差距是 `allowAlways` 被写闸拒绝，从未记录过“选项缺字段→整卡被丢”的真实帧。计划 §2 要求“R0 必须先核实，不能把审批问题归为未证原因”，§4-R0 停止门要求“无法重现时如实保留未定位项，不虚报关闭”。
影响：现场缺陷可能根本未修，却被记为已定位；后续 R3/R6 会以错误前提收尾。
建议：注释与提交说明改为“未定位/待实机”，或在证据文档补一条脱敏真实 permission 帧证明该路径。

### 重要

**I1（测试真实性）** `pending.rs:830`。实测：仅把 `parse_permission_options` 回退为严格版，该测试仍 `ok`（官方夹具三字段齐全，旧解析器本就不会丢卡）。计划点名的“失败测试”实为特征化测试，提交信息“RED→GREEN 均已记录”不成立。建议把可失败形状（坏兄弟选项）作为该任务的正式 RED，并在报告更正。
**I2（测试真实性）** `bridge.rs:7634`（生产代码未改，仅测试辅助函数重构）与 `tests/mobile/ZcodeControlFlow.test.tsx:353`（`useZcodeControl.ts` 未改）在旧代码上必然通过，不能作为 RED。建议在文档中标为“回归/契约测试”。
**I3（性能/热路径）** `hub.rs:2039-2042` + `capability/resolver.rs:48`：每条 changed 会话每帧同步读一次 SQLite 账本，且 `ready=false` 的丢弃帧也照算。事件泵单任务串行，真实会话高频推送时可能拖慢 relay 消费。建议 `!view.ready` 时跳过，或按 `(capability_key, 账本版本)` 缓存（仍在锁外）。
**I4（产品/安全）** `detail` 未投影，`ZcodeControlPanel.tsx:271` 只渲染 `summary`：审批卡看不到 Bash 命令正文就要点“允许”。证据 §5.2#8 已列出。建议 R3 前补只读、截断、脱敏的 `detail`。
**I5（诊断）** `pending.rs:603-618`：坏选项被静默 `continue`，无计数、无原因码，违反“未知形状保留安全禁用与可诊断原因”。建议记录 skipped 选项数或对含丢弃项的卡附原因。

### 次要

- M1 `hub.rs:67-77`：`static_policy_source` 的原文档注释被新 `type` 抢走，该函数现无注释。
- M2 `pending.rs:520-546`：`parse_questions` 仍“一坏即整卡丢”，与 options 逐项宽容不一致，且无测试。
- M3 `projection.rs:139-160`：`ready=false` 降级路径不清 `queue`，旧队列会挂在降级视图上（R2 接线前需处理）。
- M4/M5 `hub.rs:6706`：source 与 session 无关，测试无法证伪“按会话计算”；两会话仅一次切换，未覆盖“交替读写”。
- M6 证据文档无公开源码对照（计划要求分别记载）。
- M7 证据文档 §5.1 仍写“optionId/label/kind 三者必填 | 一致”，与同提交的逐项宽容实现不符。
- M8 `SessionDetail.tsx:1311`：codex 分支内再判 `!isZcodeSession` 属冗余（该分支下不可能是 zcode）。

## 4. 静态审查无法判定

1. 3.16.2.8769 桌面端真实 permission/queue 帧形状（本轮零实连、零帧捕获）。
2. 现场“图片三审批消失”是否真被本次改动消除（B1 的直接后果）。
3. SSE 热路径每帧读库在真实推送频率下的时延与卡顿。
4. 证据许可（debug 构建）在真机上的发放/消费时序。
5. 手机端 360px 视觉、深浅色与中英文（属 R4，本批未做）。

## 5. 复审执行记录（HEAD `3c5007b6`，工作区干净）

- `cargo test --lib services::zcode_control::` → 577 passed
- `cargo test --lib --features zcode-bridge-evidence services::zcode_control::hub::tests::` → 117 passed
- `cargo clippy --lib --features marker-helper -- -D warnings` / `...,zcode-bridge-evidence` → 均 clean
- `cargo fmt --check` → clean；`git diff --check` 无输出
- `pnpm exec vitest run zcodeControlHook/ZcodeSessionIntegration/ZcodeControlFlow/ZcodeControlPanel` → 63 passed
- 反事实①：回退 `parse_permission_options` 为严格版 → `malformed_sibling_option_does_not_drop_the_card` FAILED，`permission_snapshot_is_retained_with_original_options` 仍 ok
- 反事实②：回退 `apply_relay_session` 为沿用 `last_session` → 两个 `first_*_without_refresh` 均 FAILED（断言“新卡帧必须带本帧现算能力”）
- 两次改动均已 `git checkout --` 还原，`git status --short` 为空

## 6. 修复轮记录（2026-10-29，提交 `fix(zcode): correct r0 attribution and diagnostics`）

| 项 | 处理 |
|---|---|
| B1 | `parse_permission_options` 文档注释改为「未定位项（如实申报）」：明确现场「审批消失」无真实帧证据，本改动只消除代码层面可证伪的丢卡路径，不宣称归因；证据文档 §5.2#13 同步 |
| I1 | 承认 `permission_snapshot_is_retained_with_original_options` 是**特征化测试**（旧解析器本就不丢该形状），正式 RED 是 `malformed_sibling_option_does_not_drop_the_card`；后续报告不再声称前者是 RED |
| I2 | `live_delta_reports_new_pending_card_in_changed_view` 与 `sse_new_question_card_is_answerable_without_refresh` 标为**回归/契约测试**，不作为 RED |
| I3 | `ready=false` 帧跳过能力现算（结果必被清成全关），不为此读库 |
| I5 | 新增 `PendingBook::invalid_option_skipped()` 计数，坏选项不再静默丢弃 |
| M1 | `static_policy_source` 恢复自己的文档注释 |
| M3 | 投影降级（`mark_unready`）同时清空队列快照，旧队列不挂在 `ready=false` 视图上 |
| M4/M5 | 两会话用例改为**交替**（SECOND 开→降级→恢复→FIRST 关→FIRST 开），证明每帧现算、无跨会话/跨帧缓存 |
| M6/M7 | 证据文档补「公开源码侧本轮未取证」与逐项宽容说明 |
| M8 | 移除 codex 分支内冗余的 `!isZcodeSession` |
| I4 | `detail`（Bash 正文）仍只在 `payload.detail`，未投影到手机 → 转 R3 处理（审批卡需展示只读正文） |
