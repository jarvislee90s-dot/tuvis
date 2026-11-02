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
3. **计划批准**：`plan_approval` 不是普通问答，当前不猜测协议回传。本批只交付只读解释与“打开官方页面”，原生批准在拿到真实脱敏 payload 前标记 `BLOCKED`。
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

| 任务 | 内容                         | 状态    | 子代理        | 复审               |
| ---- | ---------------------------- | ------- | ------------- | ------------------ |
| A    | 队列恢复 UX 与屏障错误       | DONE    | A             | 通过（主会话复核） |
| B    | 供应商三态可用性与确认式切换 | PENDING | B             | 待做               |
| C1   | 计划批准只读解释与证据清单   | PENDING | C             | 待做               |
| C2   | 原生计划批准                 | BLOCKED | —             | 需要真实 payload   |
| D    | 文档、变更账本、全量门禁     | PENDING | 主会话/子代理 | 待做               |

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
- `src/i18n/locales/zh.json`、`en.json`
- 对应测试

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

### 5.2 C2：原生批准（BLOCKED）

- 在拿到真实脱敏 payload 前：
  - 不新增 `PlanApproval` 动作；
  - 不猜测 `resolveInteraction` 形状；
  - 不开放新能力；
  - 不自动批准。
- 拿到 payload 后另开任务：新增显式 `PlanApproval` 动作、能力门、UI 批准/继续修改、失败降级、零重放测试。

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
