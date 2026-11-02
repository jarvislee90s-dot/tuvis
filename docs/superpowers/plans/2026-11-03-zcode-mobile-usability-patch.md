# ZCode 手机远控可用性修补文档

日期：2026-11-03
状态：执行中（由主会话维护勾选与证据）
基线：`ba96d63e`
范围：手机端队列恢复、供应商可用性、计划批准只读/原生证据边界

> 本文是施工文档，不是验收通过记录。真实 ZCode 宿主、真实凭据、真实发送/停止/审批/权限均不在本批执行范围内。

## 0. 结论与安全边界

本轮只解决“用户看得到但无法操作 / 操作成功但没有效果 / 操作语义不清楚”的代码与 UX 问题：

1. **队列**：重启恢复后存在 `unconfirmed` 屏障。核对前“继续队列/插队”不得静默成功；核对后允许一键继续，但绝不重发旧 unknown 项。
2. **供应商**：显式 `available=false` 继续禁用；缺字段视为“可用性未知”，必须用户二次确认后才允许一次切换尝试，失败如实回显。
3. **计划批准**：`plan_approval` 不是普通问答；C2 已按官方投影的精确 `approve` 选项实现原生批准（独立 `planApproval` 动作），真实宿主批准验收仍待完成。
4. 不使用真实凭据、不连接真实宿主、不操作真实权限/发送/停止；测试只使用假宿主、回环、临时数据。

## 1. 根因摘要

### 1.1 队列

- 启动恢复 `recover()` 会把 `accepted + startNow + 无终态` 的发送项标成 restart unconfirmed。
- 该会话被置 `auto_drain=0, pause_reason='unconfirmed'`，`has_delivery_barrier` 为真。
- `resume` 当前只置 `auto_drain=1`，不清除 barrier；`prioritize` 在无前台回合且 barrier 存在时提前返回。
- 结果：UI 的“继续队列/插队”可能返回成功，但 `claim_next` 永远返回 `None`，消息不派发。
- “核对重启/待核对结果”是上述 restart unconfirmed 的人工确认入口；确认本身不重发旧消息，确认后还需一次 resume。

### 1.2 供应商

- 后端已支持 `Selection.provider`、目标 provider 目录、一次完整三元组和 CAS。
- 解析器当前对非当前 provider 缺 `available` 时默认 `false`，UI option 直接 `disabled`。
- 因此“能看见但不能选”不是下拉框 bug，而是可用性证据门；需要显式 known/unknown 语义，而不是把目录成员假装成可用。

### 1.3 计划批准

- `kind=userInput + toolName=ExitPlanMode + schema.interaction=plan_approval` 被投影成 `answerable=false`。
- 现有 `Answer` 路径只允许 `AskUserQuestion`；`Permission` 路径只处理 `kind=permission` 的审批选项。
- 当前 UI 只读并提示“打开官方页面处理”，这是安全边界，不是普通问答漏接。
- 原生批准必须先拿到真实脱敏 interaction payload；无证据不得猜测 `resolveInteraction` 的 answer 形状。

## 2. 任务状态表

| 任务 | 内容                         | 状态         | 子代理        | 复审               |
| ---- | ---------------------------- | ------------ | ------------- | ------------------ |
| A    | 队列恢复 UX 与屏障错误       | DONE         | A             | 通过（主会话复核） |
| B    | 供应商三态可用性与确认式切换 | PENDING      | B             | 待做               |
| C1   | 计划批准只读解释与证据清单   | PENDING      | C             | 待做               |
| C2   | 原生计划批准                 | DONE（代码） | 主会话        | 真实批准验收待跑   |
| D    | 文档、变更账本、全量门禁     | PENDING      | 主会话/子代理 | 待做               |

## 3. Task A：队列恢复 UX 与屏障错误

### 3.1 目标

- 未核对时 `resume` / `prioritize` 返回明确错误，不再“成功但零派发”。
- “确认并继续队列”一次完成人工核对 + resume（仅当用户显式确认）。
- 保留“旧 unknown 不重发”安全边界。

### 3.2 改动白名单

- `src-tauri/src/services/zcode_control/local_queue/store.rs`
- `src-tauri/src/services/zcode_control/local_queue/service.rs`
- `src-tauri/src/services/zcode_control/types.rs`（错误码）
- `src-tauri/src/remote/zcode.rs`（HTTP 映射）
- `src/lib/api/zcodeControl.ts`
- `src/mobile/ZcodeControlPanel.tsx`
- `src/mobile/ZcodeLocalQueuePanel.tsx`
- `src/mobile/ZcodePendingCard.tsx`（仅 `ZCODE_REASON_KEYS` 新码映射，错误码词表要求）
- `src/i18n/locales/zh.json`、`en.json`
- 对应 Rust/移动端测试

### 3.3 实现要求

1. 新增错误码 `queue_review_required`（HTTP 409），加入所有错误码映射和文案。
2. `store.resume`：存在未核对 barrier 时返回该错误；不得改动 `version` / `auto_drain`。
3. `store.prioritize` 入口（或 service 层对应检查）：无前台回合且 barrier 存在时返回同一错误；不得产生 stop/send。
4. `acknowledge_unconfirmed` / 其他 review 动作要保留既有语义：只清屏障，不自动补发旧消息。
5. UI：
   - 将“核对重启”改为“确认并继续队列”（或等价清晰文案）。
   - 确认成功后刷新队列，若仍暂停则用最新 `queueVersion` 自动调用 resume。
   - 未核对 barrier 存在时，“继续队列”“插队”禁用并显示原因；不得只灰掉无说明。
   - 队列为空时仍可确认，但不得伪造 resume 成功。
6. 不改动 unknown 重发语义、不改动已审查的 stop/priority CAS 规则。

### 3.4 RED / GREEN 用例

必须先写失败回归，至少覆盖：

- `resume_before_unconfirmed_review_is_rejected_without_send`
  - 构造重启恢复：A accepted/startNow 未终态，B queued。
  - 断言 resume 返回 `queue_review_required`，队列版本不变，零发送。
- `prioritize_before_unconfirmed_review_is_rejected_without_stop_or_send`
  - 断言拒绝、零 stop、零 send。
- `review_then_continue_sends_only_new_item_once`
  - 核对 A，确认并继续；只发 B 一次，A 不重发；重复请求幂等。
- 移动端：
  - 未核对时继续/插队给出明确文案；
  - 确认按钮成功只触发一次 ack + resume；
  - resume 失败时保留已确认状态并显示原因。

### 3.5 验收

- 定向测试通过；
- `services::zcode_control::local_queue::` 与相关移动端测试通过；
- 不出现“成功但队列未动”的假成功；
- 旧 unknown 零重发。

## 4. Task B：供应商三态可用性与确认式切换

### 4.1 目标

- 显式 `available=false`：禁用。
- 显式 `available=true`：直接可选。
- 缺字段：`availabilityKnown=false`，可选中但必须二次确认后才允许一次完整三元组尝试。
- 任何失败如实回显，不乐观改当前 provider/model。

### 4.2 改动白名单

- `src-tauri/src/services/zcode_control/selection.rs`
- `src-tauri/src/services/zcode_control/types.rs`
- `src-tauri/src/services/zcode_control/command.rs`
- `src-tauri/src/services/zcode_control/action.rs`
- `src-tauri/src/services/zcode_control/hub.rs`（仅测试字面量机械修复）
- `src-tauri/src/remote/zcode.rs`
- `src-tauri/src/tests`/`tests/` 下相关测试
- `src/lib/api/zcodeControl.ts`
- `src/mobile/ZcodeControlPanel.tsx`
- `src/mobile/zcode-selection-state.ts`
- i18n locale

### 4.3 实现要求

1. `ProviderChoice` 增加可用性未知标识（建议 `availabilityKnown: bool`，serde default 兼容旧载荷）。
   - 显式 `available` true/false → known；
   - 缺 `available` → `available=false, availabilityKnown=false`。
2. `ControlAction::Selection` 增加 `confirmUnknownProvider`（serde default false），加入 HTTP ACTION_KEYS。
3. `build_selection_command_for_provider` 增加显式参数或等价保护：
   - 显式 unavailable → 拒绝；
   - unknown + 未确认 → 拒绝；
   - unknown + 已确认 → 允许，仍只发一次完整 provider/model/thought。
4. 动作指纹包含 provider；`confirmUnknownProvider` 是否纳入指纹需在实现与文档中定论（建议纳入，避免同 actionId 不同安全意图重放）。
5. UI：
   - 显式 unavailable 保持禁用并显示原因；
   - unknown provider 可被选择，但选择模型后弹二次确认；
   - 确认前零 selection POST；
   - 确认后恰一次完整三元组；宿主拒绝只显示错误，不改当前选择；
   - 供应商单独变化不写 provider-only 中间态。
6. 不把目录成员资格直接当成可用证据；不改变模型/档位归属校验。

### 4.4 RED / GREEN 用例

- `unknown_provider_requires_explicit_confirmation`
  - 缺字段 B：不带确认拒绝，带确认允许一次。
- `explicit_unavailable_provider_stays_disabled`
  - 显式 false 仍拒绝。
- `provider_switch_ui_confirms_before_post`
  - 选未知供应商不 POST；确认后只发一次完整三元组。
- `same_model_two_providers_keep_their_own_levels` 回归不破。
- `provider_enters_action_fingerprint` 回归不破；确认标志的指纹行为按实现补充。

### 4.5 验收

- 定向测试通过；
- 真实目录/真实切换保持 NOT RUN；
- 不声称“所有灰项都能切”，只承诺“显式确认后可尝试，宿主错误可见”。

### 4.5.1 实施备注（B 复审确认）

- 缺 `available` 的**当前 provider** 也按 unknown 处理：解析仍 `available=true`，但提交
  必须带 `confirmUnknownProvider`；因此模型/档位切换也会先确认一次。这是把“可用性未知”
  与“当前 provider 可继续使用”分开，不把未知冒充已知。
- 旧便捷入口 `build_selection_command` 固定 `allow_unknown=false`；生产命令走
  `build_selection_command_for_provider` + action 标志。该旧入口仓库内仅测试调用。
- `hub.rs` 的 `ProviderChoice`/`ControlAction` 字面量补字段属于结构变更的机械编译修复，
  不改变 Hub 行为。
- UI 用未知 provider 的档位切换同样走确认框；取消清 pending provider/model，回到服务端
  当前组合。

## 5. Task C：计划批准

### 5.1 C1：只读解释与证据清单（本批执行）

**改动白名单**

- `src-tauri/src/services/zcode_control/pending.rs`
- `src-tauri/src/services/zcode_control/types.rs`
- `src/mobile/ZcodePendingCard.tsx`
- `src/mobile/ZcodeControlPanel.tsx`（tab 文案）
- `src/lib/api/zcodeControl.ts`（TS 镜像）
- `src/i18n/locales/zh.json`、`en.json`
- 对应测试
- `hub.rs` / `command.rs` / `capability/confirm.rs` / `tests/zcode_control_e2e.rs` 仅因新字段的机械夹具补 `plan_approval:false`

**实现**

1. `PendingView` 增加 `planApproval`（或等价只读标识），由 `kind=userInput + toolName=ExitPlanMode + schema.interaction=plan_approval` 置位。
2. 移动端计划卡：
   - 明确说明这是计划批准，不是普通问答；
   - 显示“当前不能远程作答，需在官方页面处理”；
   - 保留“打开官方页面”；
   - 不显示可提交选项/提交按钮；
   - 零 `resolveInteraction`、零 `snooze`、零 `Answer` 帧。
3. 文档补充原生方案所需证据：
   - 真实脱敏 interaction payload（字段、questions/options、schema）；
   - 批准/拒绝/继续修改对应的 optionId 或 content；
   - `resolveInteraction` 的 answer 变体与成功/失败可观察终态；
   - 能力门与 actionId/CAS 设计。
4. 不得实现“打断 + 发批准提示词 + 切模式”的伪批准。

**5.1.1 实施备注（C1，待主会话复核）**

- 服务端：`PendingView` 新增 `plan_approval: bool`（serde camelCase `planApproval`，
  `#[serde(default)]`）；`pending::parse_pending` 只在
  `kind=userInput ∧ toolName=ExitPlanMode ∧ schema.interaction=plan_approval` 三条同时
  成立时置 `true`，`answerable` 保持 `false`。新增纯判定 `is_plan_approval` /
  `plan_approval_parts`（与投影同源），**不**新增 `ControlAction`、不构造
  `resolveInteraction`、不产生任何计划批准帧。相似形状（缺 schema、工具名不符、
  MCP elicitation、`kind=permission`）不置位，走通用只读文案。
- 移动端：`planApproval=true` 时卡片标题与多卡 tab 显示「计划批准」
  （zh/en），只读原因改用专用文案「这是计划批准，当前不能远程提交；请在官方页面
  批准/继续修改」，保留「打开官方页面」；选项/翻页/提交/确认页/输入框均不渲染，
  且不触发 snooze（零 POST）。`answerable=false` 的其他未知子型仍用
  `pendingReadonlyReason` 通用文案（回归已锁）。
- 白名单补充（经主会话批准）：`src/lib/api/zcodeControl.ts`（`planApproval?: boolean`，
  旧载荷缺字段按 false）、`src/mobile/ZcodeControlPanel.tsx`（tab 文案），以及仅因
  `PendingView` 新字段需要的 `cfg(test)` / e2e 字面量机械补 `plan_approval: false`
  （`hub.rs` 3 处、`command.rs` 2 处、`capability/confirm.rs` 1 处、
  `tests/zcode_control_e2e.rs` 3 处）。未改 Task A/B 代码。
- RED/GREEN：
  - RED：投影 wire 断言 `planApproval` 为 `Null`（期望 `true`）；计划卡无
    `data-plan-approval`（`null`）；tab 文案为「问答 1/2」（期望「计划批准 1/2」）。
  - GREEN：新增 Rust 用例 `plan_approval_projection_sets_flag_and_stays_unanswerable`
    / `ask_user_question_projection_is_not_plan_approval` /
    `other_user_input_plan_shapes_are_not_misflagged`；新增移动端用例
    `plan_approval_card_uses_dedicated_readonly_copy_with_zero_post` /
    `unsupported_readonly_subtype_keeps_generic_copy` /
    `plan_approval_tab_uses_dedicated_label_and_card_stays_readonly`。
  - 定向结果：`services::zcode_control::` 866 通过；`zcode_control_e2e` 9 通过；
    `ZcodePendingCard` / `ZcodeControlPanel` / `ZcodeControlFlow` 共 97 通过。
  - 全量结果：`cargo test --locked --features marker-helper --lib` 3845 通过 / 38 ignored；
    `pnpm test` 207 个文件、2548 通过 / 5 skipped；`cargo clippy --all-targets
--features marker-helper -- -D warnings`、`cargo fmt --check`、`pnpm check`
    （format/lint/i18n/build）、`git diff --check` 均通过。
- 边界：全部为合成 DTO/假夹具；未连接真实宿主、未读真实凭据、未发送/停止/审批；
  C2 仍 `BLOCKED`。

### 5.2 C2：原生批准（历史 BLOCKED 评估，已被 §11 取代）

- 在拿到真实脱敏 payload 前：
  - 不新增 `PlanApproval` 动作；
  - 不猜测 `resolveInteraction` 形状；
  - 不开放新能力；
  - 不自动批准。
- 拿到 payload 后另开任务：新增显式 `PlanApproval` 动作、能力门、UI 批准/继续修改、失败降级、零重放测试。

**C2 证据清单（当前全部缺失，禁止用推测填空）**

- [ ] 真实脱敏 interaction payload：顶层 `kind` / `interactionId` / `anchorRowId` /
      `autoResolution`，以及 `payload.kind` / `toolName` / `schema.interaction` /
      `questions[]`（`question` / `header` / `options[{value,label,description?,preview?}]` /
      `multiSelect`）/ `freeText` / `options[]` / `detail` 的实际字段集与取值域。
- [ ] 批准 / 拒绝 / 继续修改在宿主上的语义载体：分别对应哪个 `optionId`、哪个
      `content` 字段或哪种 answer 变体（含「继续修改」是否进入同一 interaction）。
- [ ] `resolveInteraction` 的完整 answer 形状：`action` 的实际取值（**待取证**，
      不得假设与现有 Answer 的 `accept` 同形）、`content` 结构与必填字段；
      成功/失败的可观察终态（revision 推进、卡撤除、宿主错误码/回执 state）。
- [ ] 能力门与 actionId/CAS 设计：是否新增计划批准能力旗标、动作指纹是否纳入
      plan 身份、并发/重放/代际校验规则。
- [ ] 失败降级与零重放：宿主拒绝、超时、断线、重连后的卡状态与重试语义。

现状：C1 只交付只读解释；以上证据均为零实连、零真实 payload、零真实批准，C2 保持
`BLOCKED`，不得以「打断 + 发批准提示词 + 切模式」等伪批准替代。

### 5.3 C1 RED / GREEN

- 计划卡只读：无选项、无提交、零 POST；
- `AskUserQuestion` 可答性回归不变；
- `plan_approval` 不进入普通 Answer 路径；
- 文案明确指引官方页面。

### 5.4 C1 验收

- C1 测试通过；
- C2 在文档中明确 `BLOCKED` 及原因；
- 不产生任何计划批准帧。

## 6. Task D：文档、账本与门禁

### 6.1 文档

- `docs/user-manual/zh/08-mobile-remote.md`
- `docs/user-manual/en/08-mobile-remote.md`
- `CHANGELOG.md`
- `CHANGELOG.en.md`
- `docs/superpowers/reviews/2026-11-01-zcode-control-execution-summary.md`

内容：

- 队列“待核对/确认并继续”的语义与顺序；
- 供应商 known/unknown/不可用三态与二次确认；
- 计划批准只读边界和官方页面入口；
- 真实宿主仍未验。

### 6.2 门禁

在 `src-tauri`：

```bash
cargo check --locked
cargo fmt --check
cargo test --locked --features marker-helper
cargo test --locked --tests --features marker-helper,zcode-bridge-evidence
cargo test --locked --lib --features marker-helper,zcode-bridge-evidence
cargo clippy --all-targets --features marker-helper -- -D warnings
cargo clippy --all-targets --features marker-helper,zcode-bridge-evidence -- -D warnings
```

在仓库根：

```bash
pnpm test
pnpm check
pnpm build:mobile
git diff --check
```

### 6.3 最终报告

- 每个任务：状态、diff、测试、RED/GREEN、未完成项；
- C2 `BLOCKED` 及所需 payload；
- 真实宿主/真实凭据/真实模式/真实 Keychain NOT RUN；
- 不 push/merge/tag。

## 7. 主会话 Review Checklist

- [ ] A：未核对时 resume/priority 不再静默成功；核对后只发新消息；旧 unknown 零重发。
- [ ] B：显式 false 禁用；unknown 需确认；零 provider-only、一次完整三元组；错误可见。
- [ ] C1：计划卡只读、零计划批准帧；C2 明确 BLOCKED。
- [ ] D：中英手册/账本/报告与实际行为一致。
- [ ] 所有任务只用假宿主/回环/临时数据。
- [ ] 只改白名单文件；无 `git add -A`；无 commit/push/merge/tag（主会话 review 后按任务提交）。
- [ ] 定向测试 + 全量门禁通过，或失败项如实记录。

## 8. 子代理执行记录

| 子代理 | 任务            | 开始       | 完成 | 测试                                         | Review 结论                          |
| ------ | --------------- | ---------- | ---- | -------------------------------------------- | ------------------------------------ |
| A      | Task A          | 2026-11-03 | 完成 | 159 local-queue + 92 mobile + 4 HTTP mapping | 通过；普通 running accepted 回归已补 |
| B      | Task B          | —          | —    | —                                            | —                                    |
| C      | C1 + C2 证据    | —          | —    | —                                            | —                                    |
| 主会话 | D + 全量 review | —          | —    | —                                            | —                                    |

## 9. 已知风险 / 不承诺

- 真实 ZCode 宿主是否接受计划批准原生回传：未验，C2 BLOCKED。
- 真实 provider 的 `available`/账号状态：未验；unknown 尝试可能被宿主拒绝。
- 真实重启/队列屏障：本批用假 store/service 复现；真实进程重启未验。
- 附件删除窗口、真实网络超时、移动真机 UI 未验。

## 10. 最终结果（2026-11-03）

- Task A：`d91389c2 fix(zcode): require review before queue resume`
  - 159 local-queue tests；普通 running accepted 的插队回归通过；未核对 resume/priority 不再静默成功。
- Task B：`6fa1b8b4 fix(zcode): confirm switching to providers with unknown availability`
  - 三态 `available`/`availabilityKnown`；显式 false 禁用，未知需二次确认；一次完整三元组。
- Task C1：`d26b1083 fix(zcode): explain plan approval as read-only`
  - `planApproval` 只读标识；专用“计划批准”标题/原因；零批准帧。
- Task C2：`85c04164 feat(zcode): resolve plan approval natively` 已实现（独立 `planApproval` 动作、精确 `approve` 原值、单选项能力闸）；真实宿主批准验收仍待跑，不得仅凭 fake 宣称 P0 完成。
- P1 跨 workspace switch：未实现；协议 transport/role 仍需 Phase 0 真实取证，不得以手动切窗口冒充修复。
- Task D：中英手册、CHANGELOG 已更新；执行报告已追加。
- 追补：会话级核对修复（160 local-queue tests）见 Task A 备注；C2 P0 原生批准代码见 §11；最终门禁：`pnpm test` 2549 passed / 5 skipped；`pnpm check`、`pnpm build:mobile` 通过；
  Rust 普通 4375 passed；evidence integration 4398 passed；evidence lib 3864 passed / 38 ignored；
  两套 clippy、`cargo fmt --check`、`cargo check`、`git diff --check` 通过。
- 真实 ZCode 宿主、真实 provider 切换、真实计划批准、真实发送/停止/审批/权限：**NOT RUN**。
- 未 push/merge/tag。C2 需要在获得真实 payload 后另开任务。

## 11. C2 P0 原生计划批准实施（2026-11-03 追补）

官方投影已确认：`ExitPlanMode/plan_approval` 是 `userInput`，唯一问题为
“Review this implementation plan.”，唯一 option 原值为 `approve`；官方不是发普通消息，
而是对既有 interaction 做一次 resolve。

实现：

- `PendingView.plan_approval` 保持 C1 标识；`answerable` 仍 false。
- 新增 `pending::build_plan_approval_answer`：只接受唯一 question + 唯一 `approve`
  option，返回 `{optionId:"approve"}`；多题/多选项/非 approve 一律拒绝。
- 新增 `ControlAction::PlanApproval`（`kind=planApproval`, `decision=approve`）；
  action 指纹含 interactionId/decision；HTTP 白名单；Hub 能力闸用
  `single_select`（不借 permission 的 `approval_reply`）。
- `CommandSender::execute_plan_approval` 复用 pending 授权、写前预留、autoResolution
  snooze、恰一帧 resolve、宿主撤卡后 confirmed、unknown 不重发。
- 移动端计划卡显示专用“批准并开始实施”按钮；点击前零 POST，点击后恰一次
  `planApproval`；`single_select` 关闭或卡形状不精确时只读并显示原因；普通
  `answerable=false` 子型仍走通用只读。
- 测试：pending 纯映射 4、command 2、移动端 26（含计划卡批准/能力关闭）、
  zcode_control lib 870、全量 Rust 4375、移动端 2549 通过。
- **真实宿主批准验收仍 NOT RUN**：需要重建 Tuvis，在真实 `plan_approval` 卡上点一次
  “批准并开始实施”，确认宿主撤卡且退出 plan mode；失败不得声称 P0 完成。
- 当前 Travel 的 bridge 在 10:19 后已能被宿主 attach（日志显示 Travel bridge active），
  但运行中的 Tuvis 仍是旧构建；重建后可用同一张 pending 计划验收。
