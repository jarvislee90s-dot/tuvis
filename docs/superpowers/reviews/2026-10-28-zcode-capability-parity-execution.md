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

## 三、全量门禁（plan §4）

门禁共三轮，**最终轮在最终代码 HEAD `68c99327` 全量复跑，九条全绿**（此前轮次只证明对应历史 HEAD 的状态，不外推）：

| 门禁 | @ a148dad9（初版） | @ 3b70a916（评审修复轮一） | @ 68c99327（最终代码 HEAD） |
|---|---|---|---|
| `cargo test --lib services::zcode_control::` | ✅ 551 | ✅ 563 | ✅ 564 passed / 0 failed |
| `cargo test --features marker-helper` | ✅ 36 目标全 ok | ✅ 36 目标全 ok | ✅ 36 目标全 ok，无 FAILED |
| `cargo test --tests --features marker-helper,zcode-bridge-evidence` | ✅ 0 failed | ✅ 4069 / 0 failed | ✅ 4070 passed / 0 failed |
| `cargo clippy --all-targets --features marker-helper -- -D warnings` | ✅ | ✅ | ✅ exit 0 |
| `cargo clippy --all-targets --features marker-helper,zcode-bridge-evidence -- -D warnings` | ✅ | ✅ | ✅ exit 0 |
| `cargo fmt --check` | ✅ | ✅ | ✅ |
| `pnpm test` | ✅ 2372 passed | ✅ 2372 passed | ✅ 首轮 1 个失败文件（未捕获到文件名，与已知 flaky 群体相容但未确证）；**随后 3 次全量重跑全绿**：194 文件 / 2372 passed、5 skipped / 0 failed |
| `pnpm check` | ✅ | ✅ | ✅ exit 0 |
| `pnpm build:mobile` | ✅ | ✅ | ✅ exit 0 |

**门禁绿 ≠ 语义正确**：初版九条全绿时代码里仍有两处 P0（§八），因为相关测试把错误行为写成了预期。修复轮补的测试均经「失败测试先 RED / 变异验证」证明真实咬合（P0-1、P0-2、P2 各一次变异验证，含复审者独立复做）。

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

## 六、最终状态（2026-10-29 修订）

- 最终代码 HEAD：`68c99327`（二轮评审修复，见 §八「二轮复审」）；初版报告写作 `a148dad9` 是错的——那是初版的代码尾 commit，初版报告自身提交于 `370d93a4`。
- 工作树：干净（`git status` 零输出）。
- 分支 commits：`0c111c06..3b70a916` 共 19 个 = T1-T9 计划内 13 个 + 最终审查修复 2 个（`191bcc31`/`a148dad9`）+ 初版报告 `370d93a4` + 一轮评审修复 5 个（`95a38d83`/`56be58a7`/`4d1be38e`/`404ff524`/`3b70a916`）；其后为报告修订（`581e6346`/`339c00f6`）、一轮复审修复（`646cb2d4`）、二轮评审修复（`68c99327`）与本报告修订。
- 未 push、未 merge、未 tag（遵执行纪律，合并由用户与外部评审决定）。

## 七、审查缓办项（历史分诊存档；初版「无必修项」结论已被外部评审推翻，见 §八）

必修：无（初版口径，已失效）。可延（下次触碰相应文件时顺手）：validate_evidence_token Err 回显截断/哈希化；isOther 双源 FreeText 候选宽一档；pending/interrupt 判据词随路径先到而异的统一；takeover 提示陈旧展示；takeover_lock 并发回归用例；interrupt Rejected→degraded 专项用例。不修（有裁决理由）：key.rs Hash 去重用例、短凭据字符层不可分、observe_hello 直赋值语义、catch_unwind stderr 噪音、sweep e2e 理论 flake 窗、needs_config 无用例等——完整分诊见 `.superpowers/sdd/2026-10-28-zcode-capability-parity-implementation/progress.md`。

## 八、外部评审修复轮（2026-10-29）

外部评审（HEAD `a148dad9`）结论：**不通过，阻塞合并**。逐条核实全部属实，修复如下（每项先写失败测试跑 RED，再最小修复，遵循 systematic-debugging：先核实根因、不改协议、不猜形状）：

| 项 | 根因（核实确认） | 修复 | commit | 证据 |
|---|---|---|---|---|
| P0-1 model/thought 生产自锁 | `has_model_catalog` 唯一写入点是 `SelectionPort::load`（bridge.rs），而 `load` 在 command.rs 能力闸**之后**才被调用；snapshot `config` 是扁平形状（无 models 目录），闸关 → 不读目录 → 永远 enabled=false，已知 build shipped confirmed 同样被锁死（只有 debug 证据许可可绕过，正好解释手工验证能过、生产不行） | 读路径惰性单飞目录读：`BridgeManager::ensure_selection_catalog`（绑定会话 + 投影就绪才读；同 revision 只尝试一次，失败也记位；不经任何能力闸；失败零报错），挂在 `HubBackend::session`（GET /zcode/session 读路径）；目录缓存 `SessionEntry::catalog` 与既有 `load` 共用（写前新鲜目录同步刷新缓存）；目录合并进 SessionView（模型下拉有真实候选） | `4d1be38e` | 桥级 2 用例（闸前采集形状 / 未 bind 零 RPC + 每 revision 恰一次 + 失败记位）；**已知 build resolver e2e** `known_build_opens_model_and_thought_via_catalog_read`：model/thought enabled、无原因码、目录模型回写视图、恰一帧 getView、重复 GET 零新读、零命令帧零注入、shipped 零账本行；**变异验证**：临时禁用目录读 → e2e 以「目录读自锁未解」失败（复现原缺陷）→ 恢复后通过 |
| P0-2 pending 假确认 | 泵判据 `confirm_pending` 读本地投影 `SessionView.pending`；ACK 后 `mark_resolved_local` 已把卡从本地册消费（代码自注「本地消费 ≠ 宿主撤卡」但判据没用它）→ 任意 revision 前进帧即假 confirmed，尤其 approval_reply；证据价值被击穿 | 判据改签名 `confirm_pending(ctx, after, host_pending: Option<&[String]>)`：撤卡证据只认**宿主集合**（`PendingBook::host_pending_ids`，与回执腿 `wait_host_cleared` 同一权威源）；`None` = 桥无证据面 → 保守不确认（绝不把「取不到」当「已撤卡」）；加 `after.ready` 闸；到点判据词（`pending_still_present`）同源改按宿主集合；泵慢路径经 `BridgePump::host_pending_ids` 供给（生产 ProjectionBridge 覆写，fake 默认 None） | `95a38d83` | 纯判据反例 3 条（宿主卡在 + 本地册空 + revision 前进不确认；not-ready 不确认；None 无证据不确认）+ tracker 接线正反 2 条（宿主宣告撤卡 → confirmed；宿主未撤零落账）；既有 6 条泵路径测试改为 FakePump 显式 `set_host_pending_ids` 如实声明宿主状态后仍绿（证明修复后确认为真实撤卡证据驱动）；answer e2e 的泵腿经假体真实撤卡帧驱动 |
| P1-1 shipped Confirmed 也登记首确认 | `needs_confirmation` 只查账本；已知 build 的 send/approval 也开确认窗，一次动作级拒绝（会话忙/参数错/过期卡）→ `settle_receipt` 写账本 degraded，resolver 里账本 degraded 优先于 shipped confirmed → 永久关闭无 UI 恢复 | `needs_confirmation` 前置 `shipped_state` 检查：`Some(Confirmed)` 直接 false——出厂判定不进首次确认 | `56be58a7` | `shipped_confirmed_capability_skips_first_use_confirmation`：needs_confirmation 对已知 build key false / 未知 build key true；send 被受理后观察证据齐全（指纹变化 + revision 前进）仍零落账 |
| P1-2 空 baseline 假确认 | 登记时 `unwrap_or_default()` 把缺席指纹存成空串；判据「非空且 != baseline」→ 任意非空指纹 + revision 前进即 confirmed（非消息 patch 可触发） | 登记面过滤：`log_fingerprint` 缺席或空串 → `continue` 不登记（没有消息基线就不开确认窗，等下一次带指纹基线的真实使用） | `56be58a7` | `send_without_baseline_fingerprint_never_registers_confirmation`：空 baseline 下任意非空指纹 + revision 前进零落账（RED→GREEN） |
| P1-3 schema 指纹占位 | `"v4"` 常量占位；spec R2「schema 变化即重算」不成立；renderer 热更不改版本时失效面不足 | 实现真实指纹：`SchemaShape`（桥内新类型）只记录 hello 应答 / 首个有效 snapshot / 其 `config` 子对象 / 首张 pending 卡的**键名骨架**，FNV-1a 64 哈希为定长 16 位十六进制 id 形状（跨版本稳定，不用 std DefaultHasher；值/正文/题干/JWT 对指纹不可见）；经 `HostEvidence.schema_fingerprint` 进 `capability_key`；占位常量删除 | `404ff524` | `schema_shape_fingerprint_hashes_keys_not_values`（同形状不同值同指纹 / snapshot 顶层键集或 config 子形状变化换指纹 / 定长 hex 不含原文 / 未观测 None / invalidate 清空）+ `host_evidence_carries_schema_fingerprint_after_hello_and_snapshot`（握手前 None → bind 后有指纹 → 换代清空）；e2e 账本断言改 `platform|build|protocol|` 前缀匹配（不再硬编码指纹哈希，前缀仍精确钉住 build+protocol） |
| P1-4 takeover 可取消 future | handler future 内联 await 长流程（持 `takeover_lock` 跨 disconnect+connect 网络 await）；客户端断网/abort 丢弃 future → Hub 可能停在 `Connecting`，而 takeover 对 `Connecting` 幂等返回、auto worker 不接管 → 手机无法自救 | handler 改 `tokio::spawn` 独立任务跑完全程，handler 只等结果；客户端提前离场任务仍收敛（成功 Paired / 失败回冻结码）；JoinError → `unknown` 冻结码不猜终态 | `3b70a916` | 既有 takeover 集成测试 3 条全绿（conflict→paired / 403 / needs_auth 零 OAuth，行为面不变）；机制为任务独立于请求生命周期，语义由结构保证 |

修复轮门禁（控制器独立复跑 @ `3b70a916`，全部绿）：见 §三修复轮列。

评审报告问题两项已修正：①本报告 §结论/§六改写，如实标注初版「可合并」结论不成立；②HEAD 记录以 `3b70a916` 为准并解释初版错误来源。

### 二轮评审与修复（2026-10-29，核查基线 `646cb2d4`）

二轮评审确认一轮六项修复成立，新发现 1 项 **P2**（唯一合并阻塞）+ 3 项非阻塞建议：

- **P2 实时帧覆盖合并目录**：GET 读路径（`view_with_catalog`）返回合并目录，而泵广播面 `on_data_outcome` 的前后 diff 用原始投影 `projection.view()`——手机在会话事件上整包覆盖状态，模型下拉刚由 GET 恢复就被下一条实时帧打回单项，且 ready=true 不触发补拉。**修复**：广播面前后视图统一走 `view_with_catalog`（与读路径同一出口）。commit `68c99327`。**证据**：e2e `session_broadcast_keeps_full_catalog_across_live_delta`（GET 得两模型 → 宿主正常快照换模型推进 revision 2 → Hub 广播 Session 事件必须仍带 [a,b] 且 selection.model=b → 实时帧后 GET 仍 [a,b]）；RED 精确复现（广播载荷 models=[b]）→ GREEN；**变异验证**（广播面还原原始投影 → 测试以同一症状失败）。
- spec §12 的指纹条目已同步为真实形状哈希实现（原 `"v4"` 占位描述作废，历史见 §八一轮 P1-3 行）。
- 门禁表述修正：§三改为三轮分列，最终轮在 `68c99327` 全量复跑（pnpm test 首轮 1 个失败文件未捕获名称、3 次重跑全绿，如实记录不掩盖）。
- takeover 客户端中断回归：维持为测试跟进项（二轮评审确认非阻塞）。

### 复审轮（requesting-code-review，范围 `a148dad9..339c00f6`）

复审结论：**可进入合并评审**。两个 P0 经复审者独立变异验证证实真实关闭（禁用目录读 → 已知 build e2e 逐字复现 P0-1；判据无视宿主集合 → 新反例测试失败）；P1-1/P1-2/P1-3 原项关闭；P1-4 判 PARTIAL（缺陷关闭，但评审要求的「客户端断开用例」未补，复审接受为非阻塞跟进项）。

复审发现修复自身引入 1 个 **Important**：`SchemaShape` 把 `pendingInteractions` 首卡键集纳入骨架，而首帧有无卡/首卡题型随会话状态变化 → 同 build 跨重启指纹漂移 → 账本分裂（confirmed 重开确认窗、degraded 被遗忘；方向保守无安全面，但削弱指纹本意）。**已在复审轮立即修复**：骨架剔除 pending 段，只保留 hello/snapshot/config 三段键集；补回归钉「有卡/无卡、卡题型/payload 形状都不改指纹」。另修复审 Minor：hub.rs 一处重复测试行；本报告 §六/§八三处计数笔误（19 commits、6 条泵路径测试）。

复审登记的跟进项（不阻塞）：①takeover 客户端断开回归用例（gate 住 FakeBackend 中途 abort handler 任务，断言 Hub 最终 Paired）；②`ensure_selection_catalog` 与 `load` 各自合成 config JSON 的重复逻辑可抽取；③目录读每 revision 一次无重试计时（静止会话瞬时失败需等下一帧，有界自愈）。

评审 P2/记录项处置（本轮不改码，逐项回应）：单击即发=产品裁决面（常驻说明文案保留，是否升级二次模态由用户裁决）；并发 takeover 二次冲突会再跑一轮（`takeover_lock` 串行化已保证不叠加连接流；注释已按实际语义核对，无并发用例留在缓办项）；interface 证据宿主级 OR（动作侧按目标再校验，无跨会话越权，接受）；`action_failed` 不在前端码表（下一批 UX 清理带走）；DB 读失败 fail-open（内存 last-known degraded 记为后续改进项）；读侧不复验 reason_code（写侧硬闸已挡源头，读侧复验记为潜在雷）；上传文件 spec 明确排除、未混入（另立项）；真机未跑项维持如实登记（§五），合并前需 unknown build 与 takeover 的真实链路证据。



## 九、第三轮增量修复（2026-10-29，用户确认新范围）

用户确认范围：①修复所有写动作的协议身份；②八项能力尽量按接口开放；③接入通用附件上传（正文引用方式）；并同步 spec/plan 旧「本批不接附件」范围条款（不改 MASTER-PLAN）。执行方式：systematic-debugging（先根因后修复，TDD RED→GREEN）+ 独立复审。基线 `7ea6df25`，最终 HEAD `29b11ad2`。

### 1. 协议身份（commit `72994ffd`）

- **根因（用户已确认）**：clientHello 用 `tuvis-remote`（bridge.rs），命令信封却用手机 device_id（command.rs 4 处 json!/insert + snooze 2 处实参）→ 官方 `fault.command.clientMismatch`（202 Promise Error），旧实现把它一律映射成 `unknown` 吞掉。
- **修复**：`types::PROTOCOL_CLIENT_ID` 单一来源（clientHello + 全部五类信封：send/resolve/stop/selection/snooze）；手机 device_id 仍用于幂等身份（actionId+device+session+generation+kind）、审计（write_audit/zcode_actions.device_id）、回执归属，绝不进协议信封。
- **错误分类**：`call_with_timeout` 对 202 按 fault code 分类——含 `clientMismatch` → 新冻结码 `codes::CLIENT_MISMATCH`（`client_mismatch`），其余保持 unknown；上游原文不透传；`settle_receipt` 对 client_mismatch 回执零落账（同 SendFailed：身份错误不是能力失败、不降级、不重发）。
- **测试**：严格假宿主（记录握手身份 + 逐一核对命令信封，不一致回 202 fault）——RED 复现生产症状（send → unknown）→ GREEN；`client_mismatch_fault_classifies_locally_without_resend_or_degrade`（不透传/不重发/账本 0 行/send 保持 enabled）；审计归属断言（zcode_actions.device_id = 配对手机）；**变异验证**（信封改回 device_id → 严格宿主以 client_mismatch 拒绝）；纠正 7 处把旧错误身份写成预期的既有断言（command.rs 6 + wire_e2e 3 → PROTOCOL_CLIENT_ID）；前端 21 码表 + zh/en 文案。

### 2. 八项能力开放（commits `8e79cf89` + `d34a3f5e` + `29b11ad2`）

- shipped 表（已知 build 3.16.2.8769/protocol 3）：`multi_select`/`free_text` 从 Unsupported → **InterfaceMatched**（候选）。「未实测」≠「明确不支持」：历史证据记录保留在 spec §4.1（不改写为实机通过）；本仓解析（pending 卡 multiSelect/freeText 形状）与编码（build_user_answer 多选/自由作答）均已支持；无卡形状时接口闸仍关闭；首次真实动作确认/降级照常兜底。spec §6.3 同步（复审必修项）。
- **矩阵回归** `eight_capabilities_matrix_across_builds_and_shapes`：已知+未知 build × 全形状八项全部候选开放（reasonCode=None）× 8 个逐一缺席分支关闭（needs_evidence）+ 兄弟能力不受牵连（multi 缺形状不影响 single/free）。混合题注册已有测试保持。
- 不把身份/网络/会话错误误判为能力：身份错误零落账（新测）；网络（offline）/会话（stale_generation）错误维持既有 Err 路径（不降级、保持候选），本轮零改动。

### 3. 通用附件上传（commits `96c94ab1` + `9f660872` + `41c4ebb0`）

- **服务端闸**：`BridgeManager/HupBackend/ZcodeHub::bound_workspace_path`（桥精确绑定工作区）→ 附件 handler 一致性闸 `ensure_project_matches_bound_workspace`：仅对**已绑定** ZCode 会话生效，扫描快照 project_path 与绑定工作区不一致 → 409 `workspace_mismatch`（不透传路径原文）；`same_project_dir` 归一（尾斜杠 + canonicalize 解析 macOS /tmp↔/private/tmp 符号链接，单测钉住）。未绑定/非 ZCode 会话零变化（单测 + e2e 404 步）。
- **回环 e2e** `attachment_uploads_to_bound_workspace_and_flows_into_send_body`：真实 HTTP 上传字节 → 落盘 `.tuvis-attachments/<sid>/hello.txt` 内容一致 → sendText 正文含 `<file path="...">` → `injector.calls()==0`（零终端/无头回落）→ 409 不一致。
- **前端**：共享 `src/mobile/attachments.ts`（PendingAttachment 形状/`attachmentMarkup` 引用约定/`isImageFile`/id）——MessageComposer 改引共享（81 测试零回归）；ZcodeControlPanel 接入：附件钮/文件选择/图片粘贴/上传进度/×取消（XHR abort）/失败 chip、发送闸（uploading 或 failed 存在 → 零 POST + 可见提示，不静默丢附件）、仅附件可发（空文本 → 正文恰为 markup）、accepted 只清本次提交批次（unknown/失败保留草稿+全部附件，不自动重传重发）、切会话 abort 全部在途上传并清空（不串会话）。5 条 vitest 回归。
- **范围边界**：本轮=「上传到项目 + 正文引用」；官方原生附件协议未实现（spec/plan 已同步该边界）。

### 4. 门禁（plan §4，@ 最终代码 HEAD `29b11ad2` 全绿）

| 门禁 | 结果 |
|---|---|
| `cargo test --lib services::zcode_control::` | ✅ 568 passed（其中一次交错运行出现 1 个失败，两次重跑全绿——既有计时敏感 flake 族，如实记录） |
| `cargo test --features marker-helper` | ✅ 36 目标全 ok，无 FAILED |
| `cargo test --tests --features marker-helper,zcode-bridge-evidence` | ✅ 4076 passed / 0 failed |
| clippy 两 feature 构建 `-D warnings` | ✅ exit 0 |
| `cargo fmt --check` | ✅ |
| `pnpm test` | ✅ 195 文件 / 2377 passed、5 skipped / 0 failed（含新增 5 条附件面板回归） |
| `pnpm check`（prettier+lint+i18n+错误码棘轮+build） | ✅ exit 0（首轮 prettier 3 文件未格式化已 `pnpm format` 修复后复跑） |
| `pnpm build:mobile` | ✅ exit 0 |

### 5. 实机待验项（不伪造）

1. 官方宿主实机验证 `fault.command.clientMismatch` 的 202 wire 形态（本轮按 Promise Error + `{code}` 体分类；若官方把 fault 放在别处，分类分支需对齐——现有 unknown 兜底不回归）。
2. 已知 build 实机多选/自由文本卡的端到端（候选开放 → 首答 → 账本 confirmed）；实机真跑后 §4.1 证据记录可升级。
3. 真机附件链路：真实项目目录上传 → ZCode 终端内附件可读（正文引用最终消费方是官方 CLI 的 Read 能力）。
4. 上传端点 409 `workspace_mismatch` 的实机触发面（扫描快照 project_path 与实际绑定工作区漂移的真实场景）。

### 6. 计划偏差

1. 「失败重试」：MessageComposer 现状无重试按钮（失败=移除后重加），ZCode 面板对齐既有口径，未新造重试按钮（「只做必要的共享逻辑提取」边界）。
2. snooze 的协议身份：plan 未列 snooze，本轮按「所有写动作」覆盖（信封走 PROTOCOL_CLIENT_ID）——用户范围「覆盖发送、问答回答、审批、打断、模型/思考强度切换及 snooze」明示包含。
3. 钥匙串反复弹窗（用户会上报的体验问题，非本轮代码范围）：根因 = dev/测试二进制 ad-hoc 签名每次重编译变化，macOS 钥匙串 ACL 按二进制签名记账，「始终允许」无法跨构建生效。已核实**测试链路不碰真实钥匙串**（凭据存储全部注入替身）。根治需用户拍板：①自签证书固定签名（一次配置永久有效，推荐）；②dev 构建环境变量跳过原生钥匙串（ZCode 登录不持久化）；③维持现状每次允许。未经确认未改动。
4. 测试层 SSE 流读取在 current-thread 运行时不可靠（chunk 拆分 + 轮询饿死）——二轮已记录，本轮未复用该路径。
5. 一处提交时序瑕疵：`9f660872` 提交时 5 条附件测试尚未全绿（删陈旧引用后 `41c4ebb0` 转绿）——如实记录，未掩盖。

### 7. 复审与遗留

独立复审（general-purpose 约束镜，范围 `7ea6df25..8bd877df`）：四项 CLOSED，唯一必修（spec §6.3 与 shipped_state 矛盾）已修（`29b11ad2`）。遗留（非阻塞，记录在案）：①`view_with_catalog` 进广播 diff 的每帧目录克隆（会话数小+字节预算约束，可接受；后续可按 revision 预判短路）；②面板失败 chip 显示原始错误码未本地化（与 MessageComposer 既有口径一致）；③`addZcodeFiles` 无名图片的名称兜底是空操作（显示空名，不影响功能）；④takeover 客户端中断回归用例（前轮跟进项，仍开放）。
