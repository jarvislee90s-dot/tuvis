# ZCode 能力对齐与 build 兼容 · 执行报告

日期：2026-10-28（初版）/ 2026-10-29（外部评审修复轮更新）。执行方式：subagent-driven-development（每任务独立实现者 + 任务级审查 + 修复循环 + 最终全分支审查）；外部评审修复轮按 systematic-debugging（先逐条核实根因，再写失败测试，再最小修复）。
Spec：`docs/superpowers/specs/2026-10-28-zcode-capability-parity-spec.md`；Plan：`docs/superpowers/plans/2026-10-28-zcode-capability-parity-implementation.md`。
分支：`codex/zcode-unified-remote`（从 `0c111c06` 开工）。未 push、未 merge、未 tag、未改 `docs/MASTER-PLAN.md`。

## 结论（2026-10-29 修订）

**代码完成，等待复审。** 初版报告（commit `370d93a4`）写「无 Critical/Important、可合并」——**该结论不成立**：外部评审在 `a148dad9` 上查出 2 处 P0（model/thought 生产自锁；pending 首次确认可被本地消费 + 任意 revision 帧假确认）与 4 处 P1，初版的绿色门禁把错误行为写成了预期。本轮已逐条核实根因并修复（见 §八），plan §4 九条门禁在修复后 HEAD 重跑全绿。合并与否由外部评审复审裁决，本报告不再自行宣称「可合并」。

初版报告的另一处错误：§六写最终 HEAD `a148dad9`，实际初版报告自身提交于 `370d93a4`（代码在 `a148dad9`、报告在 `370d93a4`）。本轮修正见 §六。

## 一、逐任务状态与 commit

| 任务 | 状态 | commit | 审查 |
|---|---|---|---|
| T1 Capability 纯核 | ✅ | `fc2afe0e` `feat(zcode): add capability core and shipped profiles` | spec ✅ / approved |
| T2 Capability DB 账本 | ✅ | `d8119e5b` `feat(zcode): add capability evidence ledger` | spec ✅ / approved |
| T3 Bridge 接口形状采集 | ✅ | `e4aaff95` `feat(zcode): collect interface evidence from bridge` | spec ✅ / approved |
| T4 Resolver + 统一下发/执行闸 | ✅ | `dc646274` `feat(zcode): resolve capabilities from build evidence` | spec ✅ / approved |
| T5 首次确认 send/model/thought | ✅（1 轮修复） | `d46f02dc` + `98dde7d5` `fix(zcode): scope confirmation settlement and generation guard` | 首轮 3 项 Important → 全部 ADDRESSED |
| T6 首次确认 pending 卡 | ✅ | `c741b7dd` `feat(zcode): confirm pending answers and approval on first use` | spec ✅ / approved |
| T7 首次确认 interrupt | ✅ | `91ae2348` `feat(zcode): confirm interrupt on first real stop` | spec ✅ / approved |
| T8 手机断开并抢回 | ✅ | `e29773a6` `feat(zcode): let mobile take back control from official page` | spec ✅ / approved |
| 清理（门禁前置） | ✅ | `191bcc31` `style(zcode): fix field_reassign_with_default in test code` | 机械 style，纳入最终 review |
| T9 UI+e2e+文档+门禁 | ✅（1 轮修复） | `61eee354` + `6529971d` `test(zcode): cover pending first answer on resolver e2e` | 首轮 1 项 Important（e2e 缺 pending 腿）→ 补腿后 ADDRESSED |
| 最终审查修复（M1） | ✅ | `a148dad9` `fix(zcode): map unsupported reason code to dedicated copy` | ADDRESSED / 放行 |

## 二、每任务 RED/GREEN 证据（摘要）

每任务均为先写失败测试、跑出 RED、再最小实现转 GREEN；实现者完整报告存 `.superpowers/sdd/2026-10-28-zcode-capability-parity-implementation/task-N-report.md`（N=1..9）。

| 任务 | 定向测试命令 | RED 证据 | GREEN 证据 |
|---|---|---|---|
| T1 | `cargo test --lib services::zcode_control::capability` | 97 个符号不存在编译错误 | 13 passed |
| T2 | `cargo test --lib zcode_capability` | 13 编译错误 | DAO 8/8 + storage key 1/1；回归 database 109 + capability 33 |
| T3 | `cargo test --lib services::zcode_control::bridge` | 16 编译错误 | bridge 62/62；全量 lib 3456 passed |
| T4 | `cargo test --lib services::zcode_control::capability` / `::hub` | E0425 resolver 不存在（51 errors） | capability 21 + hub 69（既有 68 零回归）；全量 3464 |
| T5 | confirm 纯判据 + hub 接线 | 511 编译错误 | confirm 11 + hub 7；全量 3482；修复后 3487（+5 修复测试，变异验证） |
| T6 | `cargo test --lib services::zcode_control::hub` | pending target 未实现（行为 FAIL） | +15 用例；全量 3502 |
| T7 | 同上 | 16 编译错 + 3 行为 FAIL | confirm 26 + hub 96；全量 3517 |
| T8 | hub 两条 + handler 集成 + vitest | E0599 / handler 403 / 前端 3 failed | cargo 全量 3520；vitest 31 passed；check:i18n/lint 过 |
| T9 | `cargo test --lib services::zcode_control::wire_e2e` + vitest | e2e 行为 RED（revision 停 1 + 账本无 confirmed）；UI 合规测试如实申报 GREEN on arrival（T1-T8 已交付合规 UI，未为凑 RED 破坏组件） | wire_e2e 14→15（补 pending 腿，行为级 RED→GREEN）；zcode_control 551；vitest 32/32 |

## 三、全量门禁（plan §4，控制器独立复跑 @ a148dad9）

| 门禁 | 结果 |
|---|---|
| `cargo test --lib services::zcode_control::` | ✅ 551 passed / 0 failed |
| `cargo test --features marker-helper` | ✅ 36 个测试目标全部 ok，0 failed（lib 3522） |
| `cargo test --tests --features marker-helper,zcode-bridge-evidence` | ✅ 0 failed |
| `cargo clippy --all-targets --features marker-helper -- -D warnings` | ✅ exit 0 |
| `cargo clippy --all-targets --features marker-helper,zcode-bridge-evidence -- -D warnings` | ✅ exit 0 |
| `cargo fmt --check` | ✅ |
| `pnpm test` | ✅ 194 文件 / 2372 passed、5 skipped / 0 failed |
| `pnpm check` | ✅ exit 0 |
| `pnpm build:mobile` | ✅ exit 0 |

## 四、计划偏差（已裁决，全部记录在案）

1. **T8 测试助手适配**：plan 测试草稿引用的 `fake_hub()`/`fake.jwt`/`fake.auth_state` 不存在——按现有 `ZcodeHub::with_parts` + `mark_authorized_for_test()` + FakeBackend 最小适配，语义保持（`disconnect_calls==2` = 既有 quiesce 两段语义，非新增断开，已独立读证）。
2. **schema_fingerprint 占位 "v4"、probe_epoch 恒 0**（T4 自裁，已入 spec §12）：spec R2「schema 变化即重算」暂不成立；接受理由 = build/协议承担主要失效面 + confirmed 仍受同代际形状与门 + 首次失败写 degraded 自愈。**接真实指纹时须同步更新专测**。
3. **send「新用户消息可见」代理判据**：投影无消息 id，用会话日志头指纹 `logEpoch:seq`（T5，已入 spec §12）。风险面：5s 窗内日志头因非用户消息原因前进可能假 confirmed（低-中，需 revision 同时前进）；若宿主只发 revision 不带 seq 则恒 inconclusive（安全方向）。**实机取证项**。
4. **确认窗 5s vs 回执 10s 时序**（T5-T7，已入 spec §12）：宿主 5-10s 区间的撤卡/停止两路都无处确认 → Inconclusive 保持候选，方向保守，下次真实使用重开确认窗自愈。
5. **手机抢回「确认」落地为常驻后果说明 + 单击即发**（非二次模态，T8）：brief 前端契约「点击 → 恰好 1 次 POST」字面排除模态；按钮仅冲突态出现 + 单飞 + JWT 保留构成等价防误触。**若用户要求更硬防误触需改 brief 用例后加模态**。
6. **计划外收口**：①`pnpm check` 在基线 `191bcc31` 即红（T2 的错误码棘轮 0→5 + 4 个 locale 缺键）——T9 内修复（`EVIDENCE_TOKEN_INVALID` 码 + locale 补 5 键，行为面零变化）；②clippy `--all-targets` 9 条 `field_reassign_with_default`（T3/T4 引入）——`191bcc31` 清理；③最终审查 M1：`unsupported` 码不在前端 20 码词表（已知 build 多选/自由文本卡落误导性「请稍后重试」）——`a148dad9` 补码表+专文案+测试。
7. **T2 交付时未跑 `pnpm check`**（其错误码/locale 缺键漏到 T9 才暴露）——流程教训：后端任务也应跑前端门禁中会扫描 Rust 错误码棘轮的 `pnpm check`。
8. **T3 夹带一处任务外注释改笔**（「当成→当就绪」，逐字节核实仅删一字，无害）。

## 五、未跑真机项（不伪造，全部登记于验收记录）

以下各项**未在真机验证**（不访问真实凭据/真实官方账号）：

1. 实机未知 build 验证（真实官方 build 升级后的接口候选 + 首次确认全流程）。
2. send 指纹推进形态取证：①send 后首帧是否携带 `seq`（防恒不确认）；②非消息类 patch 是否携带 `seq`（防假 confirmed）。
3. 手机 takeover 真机端到端（真实官方页踢 conflict → 手机抢回）。
4. 保险箱迁移项（验收记录 §四登记的第 4 项）。

`docs/superpowers/reviews/2026-10-28-zcode-capability-parity-acceptance.md`（T9 交付）为验收记录正本；本报告为其执行面补充。

## 六、最终状态

- 最终 HEAD：`a148dad93cf65bdd651ae26874c06e35dfd6f978`（`codex/zcode-unified-remote`）。
- 工作树：干净（`git status` 零输出；spec/plan/执行报告随 T9 及后续 commit 入库）。
- 分支 commits：`0c111c06..a148dad9` 共 14 个（13 个计划内 + 1 个最终审查修复）。
- 未 push、未 merge、未 tag（遵执行纪律，合并由用户决定）。

## 七、审查缓办项（最终审查分诊：无合并前必修项）

必修：无。可延（下次触碰相应文件时顺手）：validate_evidence_token Err 回显截断/哈希化；isOther 双源 FreeText 候选宽一档；pending/interrupt 判据词随路径先到而异的统一；takeover 提示陈旧展示；takeover_lock 并发回归用例；interrupt Rejected→degraded 专项用例。不修（有裁决理由）：key.rs Hash 去重用例、短凭据字符层不可分、observe_hello 直赋值语义、catch_unwind stderr 噪音、sweep e2e 理论 flake 窗、needs_config 无用例等——完整分诊见 `.superpowers/sdd/2026-10-28-zcode-capability-parity-implementation/progress.md`。
