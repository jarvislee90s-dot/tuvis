# ZCode 手机远控：队列下一轮独立复审报告

> 本报告 §0–10 记录 2026-10-31 首轮复审快照；W6 修正后的当前复核见 §11（2026-11-01）。旧结论保留作历史证据，不代表当前工作树状态。

> 日期：2026-10-31
> 仓库：`/Users/jarvis/Documents/tuvis-worktree2` · 分支 `codex/zcode-unified-remote`
> **审查基线（BASE）**：`f81f062e7d5493f947c6d3dbe0dc5729c95ffb77`（施工单冻结基线）
> **审查 HEAD**：`f509b79e0699be8de4fbd2928960460de441be56`
> 审查区间：`f81f062e..HEAD`（11 个提交，78 文件，+14543 / −367）
> 权威合同：`docs/superpowers/plans/2026-10-31-zcode-next-round-engineering-plan-v2.md`（下称「施工单」）
> 上游意见：`docs/superpowers/reviews/2026-10-31-zcode-next-round-review-and-change-proposals.md`
> W0 证据：`docs/superpowers/reviews/2026-10-31-zcode-queue-debug-evidence.md`
>
> **复审者身份**：独立复审者，未参与本轮实现。所有结论以**代码与本轮实跑结果**为准，
> 不采信任何「已完成」自述。本轮**未修改任何产品代码**（唯一临时改动：一份仅用于
> 探针的 e2e 测试，已删除还原；变异实验全部改坏→跑→还原，HEAD 与工作区最终干净）。

---

## 0. 结论摘要

### 0.1 一句话结论

**不建议合并到 `main`。** 本轮在**服务端协议层、命令层、能力层、投影层**的工程质量是高的，
「引导」被诚实关闭、F1/F2/F3/F4/F5 的语义实现基本正确、11 项变异实验全部被现有测试捕获；
但存在 **1 个 P0 阻断缺陷**：**四个队列写动作（撤回／修改／插队／恢复运行）与 F3 的
`heldQueue` 在 HTTP 解析层被白名单拒绝（400 `invalid_action`）**，即「按钮在、后端必拒」——
这正是施工单 §0「本轮禁止」与审查要求重点防的**假实现**。另有 **1 个 P1 回归**：
F2 严格化后未同步更新合成宿主夹具，导致 `cargo test --tests` **两条 e2e 用例在 HEAD 确定性失败**
（基线 `f81f062e` 全绿）。

### 0.2 逐条四档统计表

| # | 合同条目 | 档位 | 一句话 |
|---|---|---|---|
| 1 | P0-1 队列行布局（序号+20字素+引导/修改/撤回） | **符合** | 一行一列顺序正确；序号缺失显「—」不假造；预览按字素截断、换行折叠，且**不用于任何提交载荷** |
| 2 | P0-2 「引导」恒禁用且不产生 POST | **符合** | 按钮 `disabled` 硬编码、**结构上无 `onGuide` prop**；`queue_now` 能力槽恒 `None`/`needs_evidence`；文案诚实（"暂未验证"） |
| 3 | P0-3 「修改」事务语义 | **部分符合** | 先删后回填完整原文、冲突三选、默认不覆盖、永不自动发送、附件项禁用均符合；但**删除因果证据只到「项消失」**，未达施工单 §4.3 要求的「确认撤回了未消费原项」 |
| 4 | P0-4 「撤回」语义 | **部分符合** | 不回填、不发送、ACK 只作受理、以新鲜队列为准均符合；但**同样依赖「项消失」**，且**走不通**（见 P0-1） |
| 5 | P0-5 插队显式目标 ID | **符合** | `beforeQueueItemId` 服务端**一律拒绝 null**；菜单只给「移到最前/上移/下移」，**未声称实现「移到末尾」** |
| 6 | P0-6 「恢复运行」 | **符合** | `paused===true` **常驻**且 **items 为空也显示**；只发 `setAutoDrain(true)`，不重发、不重启被打断回合；不自动恢复 |
| 7 | P1-7 F1 视图单调性 | **符合** | §3.1 语义表五分支齐全，重点格「更旧 + recovery + 降级态 → recover」正确；前后端双向守卫 |
| 8 | P1-8 F2 发送 ACK 校验 | **部分符合** | 四元组校验与 `unknown` 保留草稿**正确**；但 **`delivery` 被丢弃**（UI 靠启发式旁证），且**引入 1 个 e2e 回归**（见 P1-2） |
| 9 | P1-9 F3 `choice` 处置 | **部分符合** | 三选、默认不清空、CAS 逐项比对、服务端拒绝 stale 均符合；但 **`heldQueue` 在 HTTP 层被拒**（见 P0-1），且失配时**客户端不自动刷新重选** |
| 10 | P2-10 F4 不可答子型只读 | **符合** | 服务端 `answerable` 与应答闸**共用同一纯判定**；前端缺字段时回退旧口径（自洽，见 §4.4 说明） |
| 11 | P2-11 F5 截断安全 | **符合** | 截断真值由服务端下发；截断审批**撤掉「允许一次」**、保留「拒绝」与官方页面；`detail` 不落盘 |
| 12 | 回归 U1–U7 | **符合** | 附件「＋」旁问号、单行工具栏、单一主按钮（有字发送/空输入运行中才打断）、问答卡、审批卡、队列面板均保留 |
| 13 | 安全/隐私 | **符合** | 队列正文不进 `action_params`/审计/日志/DB；审计摘要只含类别与计数；`heldQueue` 的 `disposition`+IDs 进指纹 |
| 14 | 范围纪律 | **符合** | O1/O2/O3 **未实现**（无假 UI）；`docs/MASTER-PLAN.md`、`AGENTS.md` **零改动** |
| 15 | 文案与手册 | **符合** | 中英手册**结构逐条对等**且与实现一致；i18n 中英同步（2636 键）、无遗留旧文案 |

**统计：符合 11 · 部分符合 4 · 不符合 0 · 无法判定（需实机）0**

> 「无法判定（需实机）」计 0 的原因：本报告的 15 条合同**全部可在代码层判定**；实机相关性
> 单列在 §3「合成／未实机清单」，不与合同档位混算。

### 0.3 P0/P1 发现清单（摘要）

| ID | 级别 | 标题 | 状态 |
|---|---|---|---|
| **P0-1** | **P0 阻断** | 四个队列写动作 + F3 `heldQueue` 在 HTTP 解析层被 400 拒绝 → **所有队列按钮走不通** | **已实证**（临时探针 e2e） |
| **P1-1** | P1 | 「修改」的删除因果证据只到「项消失」，未达施工单 §4.3 的保守要求（可能误回填已消费内容） | 已核实 |
| **P1-2** | P1 回归 | F2 严格化未同步更新合成宿主夹具 → `cargo test --tests` 两条 e2e **确定性失败**（基线全绿） | **已实证**（基线对照 + 变异实验） |
| **P1-3** | P1 | F3 CAS 失配时客户端不刷新队列、不强制重选，仅落到通用 `invalid_action` 文案 | 已核实 |
| **P1-4** | P1 | F2 的 `delivery` 校验后被丢弃；前端改用「内容包含 + 条数增加」启发式旁证，存在误判为「已排队」的可能 | 已核实（W0 文档已自陈，本轮部分收口） |

---

## 1. P0 交付判据逐条复核（施工单 §0）

### 1.1 ✅ 符合 — 队列每项一行（序号 + 前 20 字素… + 引导/修改/撤回）

**证据**
- 行结构：`src/mobile/ZcodeQueuePanel.tsx:293-332` — `<div className="flex min-w-0 items-center gap-1">` 内依次为序号按钮（`:297`）→ 预览 `<span>`（`:313`）→ 引导（`:320`）→ 修改（`:330`）→ 撤回（`:331`）。
- 序号缺失不假造：`src/mobile/ZcodeQueuePanel.tsx:76-78` — `positionLabel` 在 `item.position === null` 时返回 `"—"`，绝不按数组下标造号。
- 预览按**字素**截断：`src/mobile/queuePreview.ts:56-62` — `Intl.Segmenter`（`granularity: "grapheme"`）优先，缺失时回退 code point；`\s+` 折叠为单空格（`:58`）；超限加 `…`。
- **预览不进任何提交载荷**：`src/mobile/ZcodeControlPanel.tsx:873` — `const original = item.text;`（**完整原文**），载荷侧 `queueDelete`/`queueReorder` 只带 `queueItemId`/`beforeQueueItemId`（`:756-761`、`:934-940`）。
- 文案与「20 字」口径一致：`src/i18n/locales/zh.json:2324-2326`。

**复核用例**（我自己跑过并做过变异）
- `tests/mobile/ZcodeQueuePanel.test.tsx`：`exactly_20_graphemes_is_not_truncated`、`21_graphemes_is_truncated_with_ellipsis`、`emoji_zwj_family_counts_as_one_grapheme`、`combining_marks_stay_attached`、`newlines_and_runs_of_whitespace_collapse_to_one_space`、`missing_position_shows_dash_and_never_invents_a_number`、`three_items_render_one_visual_row_each_with_guide_edit_delete_in_order`、`preview_keeps_20_graphemes_and_full_text_lives_in_title`。

**是否有假实现**：否。布局与截断是纯展示层，且有 DOM 断言。

---

### 1.2 ✅ 符合 — 「引导」当前状态：必须禁用且不产生 POST

**证据（逐条追到后端派发链）**
1. **UI 结构性不可用**：`src/mobile/ZcodeQueuePanel.tsx:320-329` — 引导按钮 `disabled` **硬编码**（非条件表达式），且 **props 里根本没有 `onGuide`**（`ZcodeQueuePanelProps` 定义于 `:80-105`，无该字段）。全仓 `rg "onGuide" src/` 只命中 `ZcodeQueuePanel.tsx:20` 的一句注释。
2. **服务端能力槽恒关**：`src-tauri/src/services/zcode_control/capability/key.rs:128` — `Capability::QueueNow => None`（不进候选层）；`capability/resolver.rs` 的 `evidence_ok` 对队列写取 `shipped` 是否为 `Confirmed|InterfaceMatched`，`QueueNow` 的 `shipped` 恒 `None` → 恒落 `closed(codes::NEEDS_EVIDENCE)`。
3. **前端 API 类型保留但无人调用**：`src/lib/api/zcodeControl.ts:399` 有 `queueNow` 类型；`rg "queueNow" src/mobile/` 只命中能力矩阵字段与两处「绝不接 queueNow」的注释，**无任何提交点**。
4. **文案诚实**：`src/i18n/locales/zh.json:2328` —「当前 ZCode 版本暂未验证引导已排队内容」；`:2353` 的 `capabilityQueueNowClosed` 亦不含任何「可用」暗示。

**复核用例**：`tests/mobile/ZcodeQueuePanel.test.tsx:321` `guide_is_always_disabled_with_a_readable_reason_and_fires_nothing`；`src-tauri/.../capability/resolver.rs:548` `queue_now_stays_needs_evidence`。

**是否有假实现**：**否 —— 这是本轮做得最好的一项**。按钮在，但**结构上**不可能产生 POST；能力槽**恒** `needs_evidence`；文案不误导。

---

### 1.3 ⚠️ 部分符合 — 「修改」事务语义

**符合的部分**
| 施工单要求 | 证据 |
|---|---|
| 先删后回填**完整原文** | `src/mobile/ZcodeControlPanel.tsx:747-813`（`deleteAndRestore`）：先 `queueDelete`（`:756`），再回填 |
| 回填用**完整原文**（非 preview） | `:873` `const original = item.text;` |
| `unknown` **不回填**、不重发 | `:762-767` — `receipt.state === "unknown"` → `setQueueNotice({state:"uncertain"})` + `refresh()`，**直接 return** |
| 草稿已有内容先问**合并/替换/取消** | `:878-881` — `existing.length > 0` → `setDraftConflict(...)` 并 **return（先问后删）** |
| **默认不覆盖** | `:892-900` `resolveDraftConflict("cancel")` 分支；`conflictCancelRef` 默认聚焦（`:218-221`） |
| **永不自动发送** | 回填只调 `updateDraft(next)`（`:737-741`），**无任何 submit 调用** |
| 附件项**禁用或提示** | `src/mobile/ZcodeQueuePanel.tsx:152` `itemEditReason` → `queueEditAttachmentsReason`；面板层 `:872` 二次拦截 `item.hasAttachments` |
| 等待期间草稿变了**再问一次** | `:797-805` → `draftRef.current !== args.existing` → 再次 `setDraftConflict` |

**不符合的部分（P1-1）：删除因果证据不足**

施工单 §4.3「删除特别提醒」原文：

> 对「修改→回填」必须比普通撤回更保守：**只有有证据确认该动作确实撤回了未消费的原项**才能自动回填；
> 如果 ACK 仅说明命令受理、而 item 消失可能是自然消费，则落到「结果不确定」提示，**不自动回填以免用户重复发送**。

施工单 §5.3 亦要求 `verifyQueueItemRemovedByDelete` 返回 `uncertain`（而非 `deleted`）当「只知道 item 不在队列」。

**实际实现**：`src/mobile/ZcodeControlPanel.tsx:713-734` 的 `confirmRemoved` 判据**只有一条** ——
新鲜队列里 `!view.queue.items.some((item) => item.inputId === itemId)` 即判 `"removed"`（`:729`）。
`:778` 拿到 `"removed"` 后**直接回填**（`:806`）。

**风险复现路径**：宿主在 delete 前后并发消费该项（`dispatch.state` 从 `queued` → `drained`）→
新鲜队列里该项同样「不在」→ 判定为「已撤回」→ **把一条已被执行的正文回填进输入框** →
用户以为「没发出去」，改一改再发 → **重复执行**。

**诚实度评价**：W0 文档 §4.5 只申报了「前后端 `dispatch.state` 白名单不一致」，
**未申报本条**（回填因果证据不足）。这是**文档与代码之间的漏报**。

**复核用例**：`tests/mobile/ZcodeControlPanel.test.tsx` 覆盖了 unknown 不回填、冲突三选、附件禁用；
但**没有任何用例**断言「消费态项不得回填」——即这条风险**无测试覆盖**。

**是否有假实现**：**部分**。按钮确实能发起删除，但**「修改」的语义承诺（"确认撤回后回填"）在宿主并发消费时会被违反**，且**无测试锁定**。

---

### 1.4 ⚠️ 部分符合 — 「撤回」语义

**符合的部分**
- **不回填不发送**：`src/mobile/ZcodeControlPanel.tsx:816-861` `handleQueueDelete` — 只有 `queueDelete` 动作，**无任何 `updateDraft`/submit**。
- **ACK 只表示受理**：`src-tauri/.../command.rs:512` `classify_queue_ack` — `accepted`/`duplicate` 只算受理；注释明写「**不据此宣布删除成功**」。
- **完成与否以新鲜队列为准**：`:850` `confirmRemoved` → `:851` 只有 `"removed"` 才 `setQueueNotice({state:"deleted"})`。
- **不乐观更新**：权威队列只来自服务端（`ZcodeQueuePanel.tsx:3-6` 文件头纪律）。
- 失败/未知均**不自动重发**：`:834-853` 全部分支只 `refresh()`。

**不符合的部分**：**走不通**（P0-1）——HTTP 层 400 拒绝 `queueDelete`。
另外，与 1.3 同源，「撤回」的成功判据同样只到「项消失」，施工单 §4.3 的 ACK 闭环要求
「只宣告观测到的结果」——这一点**做到了**（文案是「已撤回」而非「已执行完成」），
但**因果归因**同样无法区分「我删的」与「它被消费的」。

**是否有假实现**：**是（P0-1）**——按钮在、后端命令层完整、但 HTTP 解析层必拒。

---

### 1.5 ✅ 符合 — 插队：显式目标 ID，菜单只提供移到最前/上移/下移

**证据**
- **服务端拒绝 null**：`src-tauri/.../command.rs:651-654` —
  `let Some(before) = before_queue_item_id.as_deref() else { return Err(invalid_action()); };`
  且 `:655-660` 校验 `before != queue_item_id` 且 `before` 必须存在于新鲜队列。
- **绝不按位置定位**：`:661-665` 载荷为 `{"queueItemId": ..., "beforeQueueItemId": ...}`，两者都是**官方 ID**。
- **菜单只有三项**：`src/mobile/ZcodeQueuePanel.tsx:342-363` — `first`/`up`/`down`，**无「末尾」**。
- **前端不传 null**：`:280-284` `move(before)` 在 `before === null` 时直接 `return`（不发）。
- **「下移」用后继项的前驱表达**（`:277-279` 注释 + `items[index + 2]`），末项无后继 → `downId === null` → 禁用（`:361`）。
- **不声称实现「移到末尾」**：手册 `docs/user-manual/zh/08-mobile-remote.md:186` 只写「移到最前 / 上移 / 下移」；W0 文档 §6.3 明确申报「末项没有『下移』」及其理由。

**复核用例**：`src-tauri/.../command.rs` `queue_reorder_targets_ids_not_positions`；
`tests/mobile/ZcodeQueuePanel.test.tsx:457` `move_menu_uses_explicit_target_ids_and_never_null`。

**变异实验**：把 `command.rs:651` 的 `else` 分支改成「允许 null 并构造载荷」→
`queue_reorder_targets_ids_not_positions` **变红**（`unwrap_err()` 收到 `Ok`）→ 该测试**有效**。

**是否有假实现**：**是（P0-1，路径层）**——语义层完全正确，但 HTTP 层 400。

---

### 1.6 ✅ 符合 — 「恢复运行」

**证据**
- **`paused=true` 时常驻**：条件为 `queue.paused === true`（`ZcodeQueuePanel.tsx:233`），**不在 `expanded` 条件内**（展开门在 `:268`）。
- **items 为空也显示**：`:125` 只在 `known !== true` 时返回 `null`；`:130` 的空队列早退分支要求 `state === "empty"`，而 `queueState`（`:53-60`）**先判 `paused === true`** → 暂停态永远不进空队列早退。
- **只恢复队列自动处理**：服务端 `src-tauri/.../command.rs:673-679` — 只构造 `{"autoDrain": true}`，**无任何队列项派发**；`:678-679` 注释明写「只解除暂停：不重启被打断的回合、不重发任何队列项」。
- **不得表述为重启被打断的回合**：`src/i18n/locales/zh.json:2338` —「队列自动处理已恢复」（**不是**「回合已恢复」）；`:2336-2337` 恢复态文案亦不含此类暗示；手册 `zh:187` 明写「**不会重启被打断的那个回合**，也不保证下一条已经在跑」。
- **不自动恢复**：全仓无任何自动调用 `onResume`/`queueResume` 的路径（`rg "queueResume" src/mobile/` 仅命中 props 与 handler 定义）。
- **只有新鲜队列 `paused===false` 才说「已恢复」**：`ZcodeControlPanel.tsx:995-1001` — `waitFreshQueue(view.generation, (fresh) => fresh.paused === false)`，超时 → `uncertain`。

**复核用例**：`tests/mobile/ZcodeQueuePanel.test.tsx:487` `paused_with_empty_items_still_offers_resume`、
`:503` `resume_is_disabled_without_capability_or_write_gate`、`:518` `resume_done_appears_only_after_a_fresh_unpaused_queue`；
`src-tauri/.../command.rs` `queue_resume_only_changes_auto_drain`。

**变异实验**：删掉 `command.rs` 中 `paused != Some(true)` 的前置 → `queue_resume_only_changes_auto_drain`
与 `queue_action_rejected_when_availability_disallows` **同时变红**。

**是否有假实现**：**是（P0-1，路径层）**——语义层完全正确，但 HTTP 层 400。

---

## 2. P1 可靠性复核（F1/F2/F3）

### 2.1 ✅ 符合 — F1：同代视图倒退

**前端（施工单 §3.1 语义表）**
- 纯函数 `resolveSessionViewLanding`：`src/mobile/useZcodeControl.ts:187-207`，五分支 `replace|degrade|recover|caps|drop`。
- **重点格「更旧 + recovery + 降级态」**：`:203-207` — `origin === "recovery"` 且 `current.ready === false` → 返回 `"recover"`；`:330-338` `mergeRecoveredView` → 内容留 `current`、`ready = true`、能力取 `incoming`（**允许由关变开**）。
- **内容不回退**：`:340-347` `mergeRecoveredView` 只改 `ready`/`capabilities`，**不碰** `pending/queue/control/selection`；`mergeDegradedView`（`:293-300`）同理；`mergeRecoveredCaps`（`:305-313`）只收窄能力。
- **`ready=false` 安全降级仍生效**：`:204` — `if (!incoming.ready) return "degrade"`（**在 revision 比较之前**），且 `degradedCapabilities()`（`:237-252`）把 11 项能力全关并带 `needs_evidence`。
- **跨会话/换代**：`:191-194` — 跨会话 → `replace`；换代更大 → `replace`；更小 → `drop`。
- **恢复 GET 能恢复能力**：`:601-613` `readSessions(recovery)` → `applyRead(..., "recovery")`；`:531-534` 只在 `outcome.value.ready` 时用 `origin`，否则用 `"safety-degrade"`。

**服务端（Hub 侧）**
- `src-tauri/.../hub.rs:1522-1540` `should_replace_published` — **同 session_id + 同 generation 才比较**（`:1527-1529` 跨会话直接放行），`incoming.ready=false` **独立放行**（`:1530-1532` 安全先行）。
- `:1553-1580` `merge_stale_degrade` — 较旧降级只并 `ready=false` + 全关能力，内容留 `current`。
- `:1594-1620` `merge_degraded_current` — 降级态收到**不更新**快照（含 `revision <=` 与 `None`）时只并 `ready`/能力。
- `:1625-1662` `publish_session` — 拒绝时**静默丢弃本帧**、不广播倒退事件；`hub.rs:3339-3345` GET 路径被拒时回既有视图（**HTTP GET 也不回退**）。

**复核用例**：`tests/mobile/zcodeSessionRevision.test.ts` 18 例（含 `sse_rev12_then_get_rev11_is_rejected`、
`degraded_current_view_accepts_recovery_even_with_lower_revision`、`stale_safety_degrade_lands_as_degrade_only`、
`degraded_stale_recovery_lands_as_recover`）；`tests/mobile/zcodeControlHook.test.tsx`
（`old_get_cannot_rollback_newer_sse_queue`、`stale_degrade_keeps_newer_queue_content`）。

**变异实验**：把 `useZcodeControl.ts:202` 之后强行 `return "replace"`（绕过 revision 单调）→
`zcodeSessionRevision.test.ts` + `zcodeControlHook.test.tsx` **共 12 条变红**。

**是否有假实现**：否。语义表与施工单 §3.1 一致。

---

### 2.2 ⚠️ 部分符合 — F2：发送 ACK 必须校验 `result.type=inputAccepted`

**符合的部分**
- **发送专用分类器**：`src-tauri/.../command.rs:475-499` `classify_send_ack` — 独立于通用 `classify_ack`（`:425-431`）。
- **四元组齐全才受理**：`status ∈ {accepted,duplicate}` ∧ `result.type == "inputAccepted"` ∧
  `result.inputId == sent_command_id` ∧ `result.delivery ∈ {startNow,queue,guide}`（`:483-497`），否则 `unknown` + `codes::UNKNOWN`。
- **`inputId` 对应本次出站命令**：`command.rs:1960` 生成 `command_id`，`:1979` 传入 `classify_send_ack(&ack, &command_id)`。
- **草稿/附件在 unknown 时不清除**：`src/mobile/useZcodeControl.ts:830` — 仅 `action.kind === "send" && receipt.state === "accepted"` 才 `setDraftCleared(true)`；附件清理在 `ZcodeControlPanel.tsx:243-247`，**由 `draftCleared` 驱动**，unknown 时不触发。
- **不自动重试可能已受理的发送**：`useZcodeControl.ts:817-840` `submit` 无重试；catch 分支只 `setReason` + rethrow（`:835-837`），**绝不重发**。
- **通用 ACK 仍只用于非发送**：`command.rs:1909` 的 `classify_ack` 是 stop/resolve 路径。

**不符合的部分**
- **P1-4：`delivery` 校验后被丢弃**。`command.rs:1975` — `let (state, reason_code, _delivery) = ...`（下划线明示忽略）；
  `ActionReceipt`（`types.rs`）**无 `delivery` 字段**。本轮 `f509b79e` 改由前端启发式旁证
  （`src/mobile/zcode-send-delivery.ts:38-62` `judgeSendDelivery`），但该启发式有两处可疑判据：
  `:53-56` 用 `item.text.includes(needle)`（子串包含）判「本次发送」，`:58-60` 用「条数增加」兜底 ——
  **并发场景下别的客户端入队也会被判为「已排队」**。W0 文档 §3.2 末尾与 §4.5-2 已如实申报此落地差距
  （本轮**部分**收口，但未解决误判可能）。
- **P1-2：引入 e2e 回归**（见 §3.2 严重发现）。

**复核用例**：`src-tauri/.../command.rs` `send_ack_requires_full_input_accepted`（`:5969`）、
`send_ack_never_promotes_failure_states`、`send_accepted_without_input_accepted_is_unknown`（`:6025`）；
`tests/mobile/ZcodeControlPanel.test.tsx` `accepted_and_unknown_keep_distinct_copy`、
`newly_typed_text_survives_late_accepted`、`failure_preserves_draft`。

**变异实验**：把 `command.rs:482-485` 的 `same_id` 改成 `true` →
`send_ack_requires_full_input_accepted` **变红**（错 `inputId` 的 ACK 被误判为 Accepted）。

**是否有假实现**：否（F2 校验侧本身是真实现）；但 **UI 提示侧是启发式**，且**存在未修的 e2e 回归**。

---

### 2.3 ⚠️ 部分符合 — F3：`choice` 模式必须让用户选择

**符合的部分**
- **`inputRouting` 投影**：`src-tauri/.../projection.rs:462-476` `parse_input_routing`（最小值域 `mode` + `reasonCode`，非对象/空串 → `None`）。
- **服务端 CAS 逐项相等**：`command.rs:382-425` `resolve_held_queue` — 比对新鲜官方队列的**完整 ID 列表与顺序**（`:414-419`），不等 → `invalid_action`，**不静默清空、不重试**。
- **choice + 非空队列必须携带处置**：`:396-403` — 未带 `held_queue` 且处于 choice+非空 → `invalid_action`（零帧）。
- **载荷展平为官方两个字段**：`command.rs:1947-1949` — `heldQueueDisposition` + `expectedHeldQueueItemIds`。
- **`held_queue` 进幂等指纹**：`src-tauri/.../action.rs:182-195` — 同 actionId 从「保留」改「清空」是不同动作。
- **UI 三选、默认不清空**：`ZcodeControlPanel.tsx:622-625`（choice+非空 → `setHeldQueuePrompt(true)` 并 return）、
  `:630-640` `confirmHeldQueue`（CAS 期望取当前手机可见队列的**完整 ID**）、`:251-253`（默认焦点在「保留旧队列」）。
- **不改写 `requestedDelivery`**：`command.rs:1944-1946` — `requestedDelivery="queue"` 只由 `running_primary_turn` 决定，**与 choice 处置无关**。
- **U3 裁决未被破坏**：`src/mobile/zcode-primary-action.ts:25-28` — 有内容 → `send`；无内容且运行中 → `stop`。

**不符合的部分**
- **P0-1**：`heldQueue` 在 HTTP 层被 400 拒绝 → **F3 的选择永远送不到宿主**。
- **P1-3：CAS 失配时客户端不刷新、不强制重选**。服务端失配回 `invalid_action`（400）。
  客户端 `useZcodeControl.ts:829` `setReason(errorReason(error))` → 只显示通用文案
  「该操作在当前状态下无效，请刷新后重试」（`zh.json:2268`），**不自动 `refresh()` 队列、也不重新弹出选择器**。
  施工单 §3.3 要求「不匹配返回 `stale`/明确冻结码，**重新加载并要求用户再决定**」——「要求用户再决定」未落地。
  （`heldQueuePrompt` 在 `:602` `submitSend` 开头即 `setHeldQueuePrompt(false)`，失败后不再自动复开。）

**复核用例**：`src-tauri/.../command.rs` `choice_send_includes_expected_held_queue_ids`（`:5783`）、
`choice_send_cas_mismatch_is_stale_and_not_retried`、`choice_send_order_mismatch_is_rejected`、
`choice_with_empty_queue_sends_normally`、`held_queue_without_fresh_view_is_not_ready`；
`tests/mobile/ZcodeHeldQueueChoice.test.tsx` 4 例。

**变异实验**：删掉 `ZcodeControlPanel.tsx:622-625` 的 choice 弹层 → `ZcodeHeldQueueChoice.test.tsx`
**3 条变红**。

**是否有假实现**：**是（P0-1）**——后端 CAS 逻辑是真实现，但**整个 F3 通路在 HTTP 层断开**。

---

## 3. P2 复核（F4/F5）

### 3.1 ✅ 符合 — F4：不可答子型只读

**证据**
- **服务端权威判定**：`src-tauri/.../pending.rs:264-270` `is_answerable_user_input` 与 `:273-300` `answerable_parts`
  （`kind=userInput` ∧ `toolName=AskUserQuestion` ∧ 合法 `schema.interaction`）。
- **与应答闸共用同一纯判定**：`pending.rs:306-308` — `build_user_answer` 前置复用 `is_answerable_user_input`（**不另建一套**）。
- **投影下发 `answerable`**：`pending.rs:540` 计算 → `:567` 赋值；`types.rs:830` `pub answerable: bool`。
- **前端只读渲染**：`src/mobile/ZcodePendingCard.tsx:283` `readOnlyInput = isUserInput && !isPendingAnswerable(pending)`；
  `:425`（只读原因）、`:619`（只读态**不渲染**选项/提交/输入框）。
- **不产生提交**：`:425`/`:433`/`:442`/`:451`/`:472`/`:483` 的作答面全部以 `!readOnlyInput` 为前提。

**缺字段时的兼容回退（自洽性说明 —— 施工单 §5 附带的「answerable 缺字段时的兼容回退是否与 F4 冲突」）**
`src/mobile/ZcodePendingCard.tsx:165-179` `isPendingAnswerable`：
- `answerable === false` → 不可答（**以服务端为准，F4 目标不被破坏**）；
- `answerable === true` → 可答；
- **`answerable === undefined`（字段缺失）** → 回退旧口径（`userInput ∧ AskUserQuestion ∧ 合法 schemaInteraction`）。

**结论：自洽，不与 F4 冲突。** 理由：(a) 服务端**总是**下发该字段（`pending.rs:567` 无条件赋值），
回退分支只在**旧服务端 / 增量 merge 未带字段**时触发；(b) 回退口径与 Rust 判定**同形**
（`kind/toolName/schemaInteraction` 三项与 `answerable_parts` 一致），不是「更宽松的近似」；
(c) 显式 `false` 永远优先，**F4 的安全目标（显式不可答即只读）不被削弱**。
`f509b79e` 的提交说明亦如实记录这是修复 `sse_new_question_card_is_answerable_without_refresh` 的回归。

**复核用例**：`src-tauri/.../pending.rs` `plan_approval_is_not_a_question_auto_approval`（`:1355`）、
`MCP elicitation is not answerable`（`:1429`）、`only AskUserQuestion may be answered`（`:1442`）；
`tests/mobile/ZcodePendingCard.test.tsx:821` `unsupported_pending_subtype_is_readonly`、`:871` 支持子型仍可答。

**变异实验**：把回退改成「任何 `userInput` 都可答」→ `unsupported_pending_subtype_is_readonly` **变红**。

**是否有假实现**：否。

---

### 3.2 ✅ 符合 — F5：长 Bash 审批详情被截断

**证据**
- **截断事实由服务端下发**：`types.rs:836` `pub detail_truncated: bool`；计算在 `types.rs:876-921`
  `project_pending_detail_bounded`（`:907` 长度判定、`:913` 有界截断）；`pending.rs:568` 赋值。
- **前端不按长度自行推断**：`src/mobile/ZcodePendingCard.tsx:285` `const detailTruncated = isPermission && pending.detailTruncated;`
- **截断审批不出现可盲点的「允许」**：`:600` `disabled={answerDisabled || (detailTruncated && option.kind === "allowOnce")}`；
  `:603` 点击处理器同样短路；`:461` 就地说明「内容未完全展示…」。
- **保留「拒绝」与「打开官方页面」**：`zh.json` 的 `pendingDetailTruncated` 文案 + `:986` 断言 `deny` 仍 `toBeEnabled()`。
- **`detail` 不落盘**：`types.rs:818` 注释 + `pending.rs` 投影链**不写日志/证据账本**；`rg "log::" command.rs` 仅 1 处
  （`:1933` 落账失败，**不含正文**）。

**复核用例**：`src-tauri/.../types.rs` `overlong_detail_is_truncated_with_ellipsis`（`:1536`）、
`:1592`/`:1601` 截断真值；`tests/mobile/ZcodePendingCard.test.tsx:889` `truncated_permission_does_not_offer_blind_allow`、
`:772` `detail_is_collapsed_read_only_and_never_submitted`。

**变异实验**：去掉 `detailTruncated` 对 `allowOnce` 的禁用 → `truncated_permission_does_not_offer_blind_allow` **变红**。

**是否有假实现**：否。

---

## 4. 约束与回归复核

### 4.1 ✅ 符合 — 回归 U1–U7

| U 项 | 证据 |
|---|---|
| U1 附件「＋」旁问号 | `ZcodeControlPanel.tsx:55` `ATTACH_HINT_ID`、`:1480-1483` 按钮、`:1542-1545` 说明区（只切换说明） |
| U2 单行工具栏（官方页面/模型/思考强度） | `:1341` `data-testid="zcode-control-toolbar"`，`:1351` 官方页面按钮；容器 `flex-wrap` + 允许折行不横向溢出 |
| U3 单一主按钮（有字发送、空输入运行中才打断） | `src/mobile/zcode-primary-action.ts:25-28` 决策表；`ZcodeControlPanel.tsx:486-492` `deriveZcodePrimaryAction`；`:612-627` `handleSend`（空正文绝不 POST） |
| U4 真实问答卡可答 | `ZcodePendingCard.tsx` 作答面（`:629`/`:653`），`readOnlyInput` 为假时可答 |
| U5 审批卡可答 | `ZcodePendingCard.tsx:288-291` 选项白名单（`allowOnce`/`deny`），`:600` 禁用逻辑 |
| U6 队列面板 | `ZcodeQueuePanel.tsx` 全文 |
| U7 队列可见 | `ZcodeControlPanel.tsx:1216-1229` 挂载 `<ZcodeQueuePanel>` |

**注**：U3 裁决（「有文字就发送、空输入且运行中才打断」）**未被本轮破坏** ——
`handleSend`（`:612`）先判 `primaryAction !== "send"` 即返回，运行中 + 空输入走 `handleInterrupt`（`:642`）。

### 4.2 ✅ 符合 — 安全/隐私

| 项 | 证据 |
|---|---|
| 队列正文不进 `action_params` | `action.rs:231-246` — 四个队列变体只取 `queueItemId`/`beforeQueueItemId`；`:247` `QueueResume` 取空对象。`action.rs:1343` 有断言「队列动作不得携带正文」 |
| 队列正文不进审计摘要 | `action.rs:266-274` — 只写 `items=1` / `items=1 moved=true`，**连项 ID 都不写**（注释：ID 足以反查排队内容） |
| 答案/正文不进审计 | `action.rs:251-253` — `send` 只写 `chars=N`；`answers` 只写计数 |
| 不进日志 | `command.rs:1933` 唯一日志（落账失败，无正文） |
| 不进 DB | `action.rs` 落库列为 `payload_hash`（**指纹**，非参数原文）+ `summary`（`action.rs:428-441`） |
| JWT/URL/Keychain 秘密 | 本轮未新增任何承载面；`queueItemId` 是官方队列标识，非秘密 |
| `heldQueue` 的处置与 IDs 进指纹 | `action.rs:187-193` — `disposition` + `expectedQueueItemIds` 进 `action_params`（施工单 §3.3 明确要求） |

**审计摘要只含类别与必要 ID**：符合（实为「类别 + 计数」，比「必要 ID」更保守）。

### 4.3 ✅ 符合 — 范围纪律

- **O1/O2/O3 未实现**：`rg "ContextRing|contextUsage|ProviderChoice" src/ src-tauri/src/services/zcode_control/` → 无命中；
  `src/lib/api/zcodeControl.ts` 的 `SelectionView` **无 `providers`** 字段 → **没有假 UI**。
- **`docs/MASTER-PLAN.md`、`AGENTS.md` 零改动**：`git diff --stat f81f062e..HEAD -- docs/MASTER-PLAN.md AGENTS.md` → **空输出**。
- **未伪造队列**：`ZcodeQueuePanel.tsx:3-6` 明写「不建自建队列」；权威队列只来自服务端投影。
- **未把 `sendQueuedNow` 冒充 `guide`**：`capability/key.rs:105-106,128`。

### 4.4 ✅ 符合 — 文案与手册

- **中英结构逐条对等**：`docs/user-manual/zh/08-mobile-remote.md:183-188` 与
  `docs/user-manual/en/08-mobile-remote.md:179-184` — **同为 1 个父条目 + 5 个子条目**，顺序一致
  （撤回/Remove、修改/Edit、插队/Jump the queue、恢复运行/Resume、引导/Guide）。
- **与实现一致**：手册描述的「移到最前/上移/下移」「引导恒灰」「恢复只恢复自动处理」
  均与代码/文案逐条对得上（无「末尾」、无「引导可用」）。
- **i18n 中英同步**：`pnpm run check:i18n` → 「i18n 键对齐通过（2636 键）」。
- **无遗留旧文案**：`rg -i "queue" zh.json en.json | rg "只读|read-only"` → 无命中（旧「官方队列只读」表述已被替换）。

---

## 5. 严重发现（P0/P1）—— 含复现步骤

### P0-1（阻断）四个队列写动作 + F3 `heldQueue` 在 HTTP 解析层被 400 拒绝 → **所有队列按钮走不通**

**严重度**：P0（阻断合并）——施工单 §0 的**产品交付判据**（用户能在手机上「看到每项一行 + 撤回/修改/插队/恢复运行」）**在真实链路上 100% 不成立**。

**根因**
`src-tauri/src/remote/zcode.rs:374-413` 的 `ACTION_KEYS` 白名单**在本轮 11 个提交中完全未被修改**
（`git diff --stat f81f062e..HEAD -- src-tauri/src/remote/zcode.rs` → **空输出**；`git log --oneline -3 -- src-tauri/src/remote/zcode.rs` 最后一次改动是 `3b70a916`，早于本轮）。
该表只列了 6 个旧 kind（`send`/`snooze`/`answer`/`permission`/`stop`/`selection`）。
`parse_action_request`（`:483-510`）用它做**严格键集校验**：
- `:497-500` `ACTION_KEYS.iter().find(|(name, _)| *name == kind)` → 未知 kind → `None` → 整个请求被拒；
- `:501-503` `action.keys().all(|key| allowed.contains(key))` → `send` 带 `heldQueue` 也被拒。

于是：
- `queueDelete` / `queueReorder` / `queueNow` / `queueResume` → **400 `invalid_action`**（`action_error` → `:445-447`）。
- `send` + `heldQueue` → **400 `invalid_action`**（`heldQueue` 不在 `send` 的允许键 `["kind","sessionId","generation","text"]` 内）。

**为什么没被发现**：本轮新增的 Rust 测试全部是 **`CommandSender` 直调**（`command.rs` 内部单测），
**没有任何一条**走 HTTP 解析层。`rg "queueDelete|queueReorder|queueResume|queueNow|heldQueue" src-tauri/tests/` → **零命中**；
`src-tauri/src/services/zcode_control/wire_e2e.rs` 同样零命中。移动端测试则整体 mock 掉 `fetch`，
只验证「发出了什么形状的请求体」，不验证「服务端接不接」。

**复现步骤（可复现，已实证）**
1. 在 `src-tauri/tests/zcode_control_e2e.rs` 追加一个临时用例，复用既有夹具
   （`seed` → `paired_hub(all_policy())` → `router(state)` → `post_action`），依次 POST：
   `{"kind":"queueDelete",...,"queueItemId":"q1"}`、`queueReorder`、`queueNow`、`queueResume`，
   以及 `{"kind":"send",...,"text":"hello","heldQueue":{"disposition":"keepQueueAndSend","expectedQueueItemIds":["q1"]}}`。
2. 实测输出（本轮真实跑出）：
   ```
   REVIEWER_PROBE queueDelete  => status=400 Bad Request body={"error":"invalid_action"}
   REVIEWER_PROBE queueReorder => status=400 Bad Request body={"error":"invalid_action"}
   REVIEWER_PROBE queueNow     => status=400 Bad Request body={"error":"invalid_action"}
   REVIEWER_PROBE queueResume  => status=400 Bad Request body={"error":"invalid_action"}
   REVIEWER_PROBE send+heldQueue => status=400 Bad Request body={"error":"invalid_action"}
   ```
3. 探针用例已删除还原（`git status --short` 空、`git diff --stat` 空）。

**影响**
- 手机上「撤回」「修改」「插队」「恢复运行」四个按钮 **点了必失败**（400 → 冻结原因码）。
  UI 会把它显示成「该操作在当前状态下无效，请刷新后重试」——**误导性文案**（真实原因是本仓白名单缺项，不是用户状态问题）。
- F3 的「保留/清空」选择 **永远送不到宿主**；`choice` 模式下发送会直接 400。
- 施工单 §1 的「手机协议」层与 §1 的必改清单**未同步**（施工单 §1 表格明确列出
  `src/lib/api/zcodeControl.ts::{ControlAction,...}` 与 Rust DTO 必须一致，但**没列** `remote/zcode.rs::ACTION_KEYS` ——
  这本身是施工单的疏漏，实现者应当顺链发现）。

**修复建议（不在本次复审中实施）**
1. `src-tauri/src/remote/zcode.rs::ACTION_KEYS` 增补 4 个队列 kind 与 `send` 的 `heldQueue` 键：
   ```rust
   ("queueDelete",  &["kind", "sessionId", "generation", "queueItemId"]),
   ("queueReorder", &["kind", "sessionId", "generation", "queueItemId", "beforeQueueItemId"]),
   ("queueNow",     &["kind", "sessionId", "generation", "queueItemId"]),
   ("queueResume",  &["kind", "sessionId", "generation"]),
   // "send" 增补 "heldQueue"
   ```
2. **必须新增至少一条走 HTTP 层的 e2e**：四个队列动作 + `send`+`heldQueue` 各 POST 一次，
   断言 **200 + 恰一帧上游命令 + 载荷形状**（这是本轮缺失的关键门禁）。
3. 移动端 `postZcodeAction` 的 400 `invalid_action` 文案应区分「契约不匹配（版本不兼容）」与「状态无效」，
   否则用户会被误导。

---

### P1-1 「修改」的删除因果证据只到「项消失」，未达施工单 §4.3 保守要求

**严重度**：P1（数据正确性 / 潜在重复执行）

**证据**：`src/mobile/ZcodeControlPanel.tsx:713-734` `confirmRemoved` 的判据**唯一**：
`:729` `if (!view.queue.items.some((item) => item.inputId === itemId)) return "removed";`
`:778-806` 拿到 `"removed"` 后**直接回填**。

**与合同的差距**（施工单 §4.3 / §5.3 / §13.3）：
> 「**只有有证据确认该动作确实撤回了未消费的原项**才能自动回填；如果 ACK 仅说明命令受理、
> 而 item 消失可能是自然消费，则落到『结果不确定』提示，**不自动回填以免用户重复发送**。」

施工单 §13.3 反模式 3 明确写：「不能以 `item 不在队列` 单一事实证明删除动作因果成功，若缺宿主明确证据必须标『不确定』并人工确认。」

**复现步骤（可复现，合成宿主）**
1. 构造队列项 `X`（`dispatch.state = "queued"`），点「修改」。
2. 在 delete 发出与 `confirmRemoved` 拿到新鲜队列之间，让宿主**消费** `X`（`dispatch.state → drained`，项从队列消失）。
3. `confirmRemoved` 返回 `"removed"`（项确实不在队列了）→ **回填 X 的完整原文**。
4. 用户看到「已撤回 + 原文回到输入框」，实际 X **可能已被执行** → 用户再发 → **重复执行**。

**根因与修复建议**：本仓**没有**「该项是我删的」这一因果信号（宿主未提供）。
按施工单 §4.3 的兜底口径，应改为：
- 「修改」路径在**没有因果证据**时**不自动回填**，落到 `uncertain`；
- 已有的「复制原文」人工兜底（`ZcodeControlPanel.tsx:1011-1024` `copyOriginal` + `:1253-1272` UI）**正是施工单
  §4.3 指定的「明确人工确认」安全退化**——只需把「修改」的回填条件收紧到有因果证据即可复用现成兜底。
- 至少**新增一条测试**锁定「消费态项不得回填」。

**文档漏报**：W0 文档 §4.5 只申报了「前后端 `dispatch.state` 白名单不一致」，
**未申报本条**。

---

### P1-2（回归）F2 严格化未同步更新合成宿主夹具 → `cargo test --tests` 两条 e2e **确定性失败**

**严重度**：P1（回归；本轮自述「门禁全绿」与实跑不符）

**证据**
- `src-tauri/tests/common/zcode.rs:133` — 合成宿主的默认 ACK 是 `json!({ "status": "accepted" })`，
  **不含** `result.type="inputAccepted"` / `result.inputId` / `result.delivery`。
- F2 上线后，`command.rs:475-499` `classify_send_ack` 对该 ACK 判 `unknown`
  → e2e 里 `first["state"]` 从 `"accepted"` 变 `"unknown"`。
- 失败用例（本轮实跑，确定性失败，重跑 3 次均失败）：
  - `src-tauri/tests/zcode_control_e2e.rs:166` `two_devices_same_action_single_dispatch_across_disconnect`
    （`left: String("unknown") / right: "accepted"`）
  - `src-tauri/tests/zcode_control_e2e.rs:964` `sse_lag_resyncs_before_next_write`（同上）
- **基线对照**（本轮实跑）：`/tmp/tuvis-baseline`（`git archive f81f062e` 解包）同命令 → **8 passed / 0 failed**。
  → **确属本轮引入的回归**，非既有失败。
- **因果确认（变异实验）**：临时把 `command.rs` 的 `classify_send_ack` 末段恢复为「一律回退 `classify_ack`」
  （即 F2 前的宽松口径）→ 两条用例**立即转绿（8 passed）** → 回归根因锁定在 F2 严格化。
  改坏→跑→已还原。

**为什么本轮自述漏过**：W0 文档 §1.3 与 `f509b79e` 提交说明都只报 `cargo test --lib zcode_control::`
（**`--lib` 不含 `tests/` 集成测试**）。施工单 §10.1 本就要求
`cargo test --locked --tests --features marker-helper,zcode-bridge-evidence`。

**修复建议**：给合成宿主夹具补一个符合 F2 的发送 ACK（`status=accepted` + `result{type,inputId,delivery}`），
或让夹具对 `sendText` 回 `inputAccepted`；随后两条 e2e 恢复绿。
**注意**：不得为了让它变绿而放宽 `classify_send_ack` —— F2 的四元组校验本身是正确的。

---

### P1-3 F3 CAS 失配时客户端不刷新队列、不强制重选

**严重度**：P1（交互正确性；施工单 §3.3 明列「重新加载并要求用户再决定」）

**证据**
- 服务端失配正确：`command.rs:419-422` — 完整 ID 列表/顺序不等 → `invalid_action`（**不静默清空、不重试**）。
- 客户端未接住：`useZcodeControl.ts:829-836` — catch 分支只 `setReason(errorReason(error))` + rethrow；
  **无 `refresh()`**、**无重新弹出选择器**。
- `ZcodeControlPanel.tsx:602` `submitSend` 开头即 `setHeldQueuePrompt(false)` → 失败后选择器**不再复开**。
- 用户可见的只有通用文案「该操作在当前状态下无效，请刷新后重试」（`zh.json:2268`）——
  **没有告诉用户「队列已变，请重新选择」**。

**修复建议**：捕获 `invalid_action`（且处于 choice 上下文）时 → `refresh()` + `setHeldQueuePrompt(true)` 重新征询，
并给一条专属文案（区别于一般状态无效）。

---

### P1-4 F2 的 `delivery` 校验后被丢弃；前端启发式旁证可能误判「已排队」

**严重度**：P1（本轮**部分**收口；W0 文档 §3.2 末尾 / §4.5-2 已如实申报，属**已知未完成项**）

**证据**
- `command.rs:1975` — `let (state, reason_code, _delivery) = ...`（有意丢弃）；`ActionReceipt` 无 `delivery` 字段。
- 本轮 `f509b79e` 新增 `src/mobile/zcode-send-delivery.ts:38-62` `judgeSendDelivery`：
  - `:53-56` 信号一：`item.text.includes(needle) && !baselineIds.has(item.inputId)` —— **子串包含**；
  - `:58-60` 信号二：`fresh.items.length > baseline.items.length` —— **只看条数**。
- **信号二的风险**：并发场景下**别的客户端**入队也会让条数增加 → 误判「已排队」。
- **信号一的风险**：`includes` 是子串匹配，两次发送相同正文时可能匹配到**另一条**历史项。

**与施工单的关系**：施工单 §3.2「本轮最小方案」明确允许**不扩 `ActionReceipt`**，但要求
「UI 以新鲜 `queue.items` 呈现入队事实；没有最新队列时只说『宿主已受理，等待状态同步』，**不写『正在执行』**」。
当前实现的措辞（「已排队」/「已受理」）**没有**谎称「正在执行」，方向正确；但**信号二**已经越过了
「以新鲜队列呈现入队事实」的边界（它呈现的是「条数变了」，不是「本次发送入队了」）。

**修复建议**：删掉信号二（条数兜底），只保留「本次发送内容对应的新队列项」这一条；
或按施工单 §3.2 的增强方案把 `delivery` 透传到回执（需版本化迁移，本轮不做亦可，但要**去掉条数兜底**）。

---

## 6. 合成 / 未实机清单（**不得与实机验证混淆**）

### 6.1 本复审的证据等级

| 类别 | 内容 |
|---|---|
| **本轮实机（本复审者亲跑）** | 全部 7 条门禁命令；`cargo test --tests` 全量；基线对照（`/tmp/tuvis-baseline`）；临时 HTTP 探针 e2e；**11 项变异实验**；工作区洁净性核对 |
| **合成宿主（本仓单测/e2e 假体）** | 所有 `src-tauri/src/services/zcode_control/**` 单测、`src-tauri/tests/zcode_control_e2e.rs`、`tests/mobile/**` vitest、38 张截图（`docs/superpowers/reviews/2026-10-31-zcode-queue-screenshots/`）——**均基于 fake bridge / fake session view / mock fetch，不代表官方 ZCode 行为** |
| **静态取证（非实机）** | R0 安装包分析 `docs/superpowers/reviews/2026-10-29-zcode-r0-wire-evidence.md`（`class = PACKAGE`，零实连零帧捕获）——只证明「代码里存在该命令名/字段名」 |

### 6.2 未实机清单（沿用并核对 W0 文档 §5，仍 **NOT RUN（需实机）**）

W0 文档申报 12 项，**本复审逐条核对源码后确认：无一项在本轮被"偷偷"变成已验证**。
以下为复核后仍需实机取证的项（与 W0 文档编号一致）：

| # | 待验证项 | 复核结论 |
|---|---|---|
| 1 | 暂停后恢复的真实 ACK 形状、`autoDrain=true` 后是否自动拉起下一条 | 仍 NOT RUN。代码只发 `{"autoDrain":true}`，不重发、不谎称 |
| 2 | `reorderQueueItem` 的 `beforeQueueItemId` 真实语义；`null` 是否=移末尾 | 仍 NOT RUN。**代码一律拒绝 null**（保守正确），但因此**末项与次末项无法下移** |
| 3 | 带 `attachments[]` 队列项的 `deleteQueueItem` 是否被宿主接受 | 仍 NOT RUN。代码只投影 `hasAttachments` 布尔，**不下发附件对象** |
| 4 | `choice` 模式下 `heldQueueDisposition` + `expectedHeldQueueItemIds` 的真实接受/拒绝与原因码 | 仍 NOT RUN。**且因 P0-1，该载荷当前根本发不出去** |
| 5 | `sendQueuedNow` 是否等价于「引导」 | 仍 NOT RUN。**「引导」恒禁用**（本轮做得对） |
| 6 | 2000 字截断上限在真实 Bash 审批中的长度分布 | 仍 NOT RUN。上限沿用旧值，本轮只加截断事实 + 安全降级 |
| 7 | `state.availability` 的真实字段路径/形状；`setAutoDrain` 是否真无 availability 字段 | 仍 NOT RUN。代码对缺字段**一律不放行**（保守） |
| 8 | 队列四命令 ACK 里 `result` 的实际内容 | 仍 NOT RUN。`classify_queue_ack` **只读顶层 status**（不读 result） |
| 9 | `pauseReason:"manual"` 是否有真实写入点 | 仍 NOT RUN。代码对非枚举值一律 `None` |
| 10 | `interrupted → paused` 的真实跃迁条件 | 仍 NOT RUN |
| 11 | 真实会话里四命令是否真被官方接受 | 仍 NOT RUN。**且因 P0-1，手机侧连请求都发不出去** |
| 12 | `dispatch.state` 各值的真实可写性（`admitted`/`reserved` 能否撤回） | 仍 NOT RUN。`f509b79e` 把后端白名单**放宽**到与前端一致的 `{queued,admitted,reserved}` —— 见 §7 附加观察 |

**本轮新发现、未被 W0 文档申报的实机依赖**：无新增（除 P0-1/P1-1 属代码层问题，不需实机即可判定）。

### 6.3 明确**不**阻塞主线但**未实现**的可选项

| 项 | 状态 | 证据 |
|---|---|---|
| O1 供应商→模型二级选择 | **未实现**（正确） | `selection.rs` 本轮只改了一行测试夹具（`input_routing: None`）；`zcodeControl.ts` 的 `SelectionView` 无 `providers` |
| O2 上下文占用圆环 | **未实现**（正确） | 无 `ContextRing.tsx`、无 `contextUsage` 字段 |
| O3 Keychain 弹窗定位 | **未实现**（正确） | `credential_store.rs` 本轮零改动 |

三者均**未以假 UI 形式偷跑**，符合施工单 §10「没有官方引导语义证据时…列入未完成；不能用『暂时按钮可点』迎合测试」。

---

## 7. 门禁实测结果（本复审者亲跑，**不采信自述**）

| 门禁（用户指定） | 命令 | 结果 | 判定 |
|---|---|---|---|
| 移动端单测 | `pnpm exec vitest run tests/mobile` | **1175 passed / 2 failed**（57 文件中 2 个失败） | ⚠️ **疑似并发负载** — 单独重跑 `language-connect.test.tsx` + `App.test.tsx`（`--pool=threads --no-file-parallelism`）→ **6 passed / 0 failed**（详见下） |
| Rust zcode_control 单测 | `cd src-tauri && cargo test --locked --lib services::zcode_control::` | **640 passed / 0 failed** | ✅ 通过 |
| 类型检查 | `pnpm exec tsc --noEmit` | **exit 0**，无输出 | ✅ 通过 |
| i18n 对齐 | `pnpm run check:i18n` | 「i18n 键对齐通过（2636 键）」 | ✅ 通过 |
| Prettier | `pnpm exec prettier --check "src/**/*.{ts,tsx,json}" "tests/**/*.{ts,tsx}"` | 「All matched files use Prettier code style!」 | ✅ 通过 |
| 错误码对账 | `node scripts/check-error-codes.mjs` | 「错误码对账通过：328 码 / errors 段 329 键（legacy 5）」 | ✅ 通过 |
| 空白检查 | `git diff --check` | **exit 0**，无输出 | ✅ 通过 |

**「疑似并发负载」的具体说明（用户要求注明）**
- 首跑 `pnpm exec vitest run tests/mobile` 的两个失败**都不是业务断言失败**：
  1. `tests/mobile/language-connect.test.tsx:138` — `Test timed out in 5000ms`；
  2. `tests/mobile/App.test.tsx` — 未进用例即报 `[vitest-pool-runner]: Timeout waiting for worker to respond`
     （**worker 启动超时**，非用例失败）。
- 单独重跑这两个文件（`--pool=threads --no-file-parallelism`）→ **全部通过**。
- W0 文档 §1.3 已记载同一现象（首轮 `--pool=forks` worker 启动超时，改 `--pool=threads` 后全通过），
  属**本机 vitest worker 启动环境问题**，与被测代码无关。
- ✅ **结论：门禁本身通过；这 2 条失败判定为「疑似并发负载」，不计入业务失败。**

**额外跑的门禁（用户要求可选的完整 `pnpm test` + 本复审者追加的 Rust 集成测试）**

| 门禁 | 命令 | 结果 | 判定 |
|---|---|---|---|
| 完整前端测试 | `pnpm test` | **200 文件 / 2502 passed / 5 skipped**，0 failed（1016s） | ✅ 通过 |
| Rust 集成测试 | `cd src-tauri && cargo test --locked --tests` | **多数二进制通过**，但 `zcode_control_e2e` **2 failed**（详见 P1-2） | ❌ **失败（本轮回归）** |
| Rust 集成（remote） | `cargo test --locked --test zcode_control_remote` | **27 passed / 0 failed**（与基线一致） | ✅ 通过 |
| 基线对照 | `git archive f81f062e` → `/tmp/tuvis-baseline` → `cargo test --locked --test zcode_control_e2e` | **8 passed / 0 failed** | 证明 P1-2 属本轮回归 |

**门禁自述与实测的差异（须记录）**
- `f509b79e` 提交说明称「`cargo test services::zcode_control::`（640 passed）…全绿」——
  该命令只覆盖 `--lib`，**不含 `tests/` 集成测试**，因而漏掉了 P1-2。
- W0 文档 §1.3 的验证表同样只有 `--lib` 与 vitest，**没有 `--tests`**。

---

## 8. 变异实验记录（改坏一次看是否真的会红）

方法：临时改坏实现 → 跑相关测试 → 观测 → `cp` 还原（未提交任何改动；最终 `git status --short` 为空、
`git diff --stat` 为空）。全部 **11 项**实验。

| # | 用例名 | 改坏什么 | 是否变红 | 结论 |
|---|---|---|---|---|
| 1 | `send_ack_requires_full_input_accepted` | `command.rs:482-485`：`same_id` 改为 `true`（忽略 `inputId`） | ✅ **红**（`left: (Accepted, None, Some(Queue)) / right: (Unknown, ...)`） | 测试**有效**；F2 的 `inputId` 校验被真实锁定 |
| 2 | `queue_reorder_targets_ids_not_positions` | `command.rs:651`：允许 `beforeQueueItemId = null` 并构造载荷 | ✅ **红**（`unwrap_err()` 收到 `Ok`） | 测试**有效**；「拒绝 null」被真实锁定 |
| 3 | `queue_delete_accepts_admitted_and_reserved_states` + `queue_actions_refuse_unknown_or_unready_state` | `command.rs:601-604`：`writable = true`（跳过 `dispatch.state` 白名单） | ✅ **红 ×2**（均收到 `Ok` 而非 `Err`） | 测试**有效**；`dispatch.state` 闸被真实锁定 |
| 4 | `queue_resume_only_changes_auto_drain` + `queue_action_rejected_when_availability_disallows` | `command.rs:674-676`：删掉 `paused != Some(true)` 前置 | ✅ **红 ×2** | 测试**有效**；「恢复只允许 paused 态」被真实锁定 |
| 5 | `guide_is_always_disabled_with_a_readable_reason_and_fires_nothing` | `ZcodeQueuePanel.tsx:324`：引导按钮 `disabled` → `disabled={false}` | ✅ **红**（`expect(element).toBeDisabled()` 失败） | 测试**有效**；「引导恒禁用」被真实锁定 |
| 6 | `zcodeSessionRevision.test.ts`（18 例）+ `zcodeControlHook.test.tsx` | `useZcodeControl.ts:203`：强行 `return "replace"`（绕过 revision 单调） | ✅ **红 ×12** | 测试**有效**；F1 的五分支语义表被真实锁定 |
| 7 | `ZcodeHeldQueueChoice.test.tsx`（4 例） | `ZcodeControlPanel.tsx:622-625`：删除 choice 弹层分支 | ✅ **红 ×3** | 测试**有效**；F3 的「必须先问」被真实锁定 |
| 8 | `ZcodeControlPanel.test.tsx`（64 例） | `ZcodeControlPanel.tsx:762-767`：`unknown` 分支改为**也回填草稿** | ✅ **红**（找不到 `zcode-queue-notice`） | 测试**有效**；「unknown 不回填」被真实锁定 |
| 9 | `truncated_permission_does_not_offer_blind_allow` | `ZcodePendingCard.tsx:600,603`：去掉 `detailTruncated` 对 `allowOnce` 的禁用 | ✅ **红**（`allow` 未 disabled） | 测试**有效**；F5 安全降级被真实锁定 |
| 10 | `unsupported_pending_subtype_is_readonly` | `ZcodePendingCard.tsx:166`：`answerable` 缺失时**任何** `userInput` 都可答 | ✅ **红**（找不到 `zcode-pending-readonly-reason`） | 测试**有效**；F4 只读被真实锁定 |
| 11 | `two_devices_same_action_single_dispatch_across_disconnect` + `sse_lag_resyncs_before_next_write` | `command.rs:497-499`：`classify_send_ack` 末段回退通用 `classify_ack`（F2 前口径） | ✅ **红 ×2 → 转绿**（反向验证） | 证明 **P1-2 回归的根因确在 F2 严格化**；同时说明这两条 e2e 只覆盖 `status`，**不覆盖 `result` 四元组** |

**实验总结论**
- **11/11 实验全部被现有测试捕获**（第 11 项为反向验证）。
- **测试质量总体良好**：F1/F2/F3/F4/F5 与队列写闸的关键不变量都有真实断言，非恒真断言。
- **唯一盲区**（与 P0-1、P1-1、P1-3 同源）：
  1. **HTTP 解析层（`remote/zcode.rs::ACTION_KEYS`）零覆盖** —— 变异 #11 反证了 e2e 只到
     `CommandSender`/`HubBackend` 边界，**不覆盖 wire 白名单**；
  2. **「修改」的因果证据（消费态不回填）零覆盖**；
  3. **F3 CAS 失配后的客户端重选行为零覆盖**。

---

## 9. 附加观察（不构成 P0/P1，但建议记录）

1. **`dispatch.state` 白名单被放宽到 `{queued, admitted, reserved}`（`f509b79e`）**。
   `src-tauri/.../types.rs:1146` `WRITABLE_DISPATCH_STATES = ["queued", "admitted", "reserved"]`。
   该改动**方向正确**（修掉前后端不一致，且既有测试 `queue_actions_refuse_unknown_or_unready_state`
   原把 `reserved` 列为「必须拒」，被改为用 `promoting` 覆盖——**属修改既有测试以适配新行为**，
   已在提交说明中如实记录，不是隐瞒）。
   **但**：`reserved`（已被预留）项能否安全撤回属 W0 文档 §5 #12 的 **NOT RUN** 事项，
   本轮在**没有实机证据**的情况下把服务端从「只允许 queued」放宽到「允许 reserved」——
   属**风险方向的选择**（fail-open 一侧）。建议在下一轮实机联调中优先取证，
   若 `reserved` 不可撤回则须收紧回 `{queued}`（并同步前端）。

2. **`zcode-send-delivery.ts` 的启发式** 见 P1-4（建议去掉「条数增加」兜底）。

3. **`docs/user-manual` 两侧结构对等性**：逐条核对通过（父条目 1 + 子条目 5，顺序一致）。
   唯一措辞差异：zh 用「恢复运行」，en 用「Resume queue」——**语义等价**，不构成不对等。

4. **38 张截图**（`docs/superpowers/reviews/2026-10-31-zcode-queue-screenshots/`）是**合成宿主**视觉证据，
   其 README §1 自述夹具「直接 import 仓库里的 `src/mobile/ZcodeControlPanel.tsx`」——
   可作为**排版**证据（行内三按钮、20 字省略、零横向溢出、恢复入口可见），
   但**不能**作为「动作可用」的证据（P0-1 恰恰证明动作不可用）。

---

## 10. 合并建议

### ❌ **不建议合并到 `main`**

**理由**
1. **P0-1 是阻断级**：施工单 §0 的**产品交付判据**（用户在手机上能真正「撤回/修改/插队/恢复运行」，
   以及 F3 的 `choice` 处置）在真实 HTTP 链路上 **100% 不成立**。
   按钮在、命令层完整、能力层开闸、测试全绿 —— 这是本轮**最典型的假实现**，
   也正是审查要求「逐个按钮追到后端派发链」所要防的那一类。
   交付它等于交付一批「点了必失败且文案误导」的按钮。
2. **P1-2 是回归**：`cargo test --tests` 两条 e2e 确定性失败（基线全绿），
   且本轮所有自述均未覆盖 `--tests` 集成层。合并会把「自述全绿但实跑有红」的状态带进主干。
3. **P1-1** 会让「修改」在宿主并发消费时**把已执行的正文回填进输入框**，存在**重复执行**风险；
   施工单 §4.3/§13.3 明文禁止这种做法。

### ✅ 合并前**必须**完成（建议的最小闭环）
1. 修 `src-tauri/src/remote/zcode.rs::ACTION_KEYS`（P0-1），并**新增走 HTTP 层的 e2e**
   （四个队列动作 + `send`+`heldQueue`，断言 200 + 恰一帧 + 载荷形状）。
2. 修合成宿主夹具的发送 ACK（P1-2），恢复 `cargo test --tests` 全绿，并**把 `--tests` 纳入 W6 门禁记录**。
3. 收紧「修改」的回填条件（P1-1）：无因果证据 → 不自动回填，走已有的「复制原文」人工兜底；补测试。
4. 顺带（低成本、高收益）：F3 CAS 失配 → 刷新 + 重新征询（P1-3）；去掉 `judgeSendDelivery` 的「条数增加」兜底（P1-4）。

### 📌 关于「不阻塞」的部分
- 「引导」恒关闭、F1/F4/F5、U1–U7 回归、安全/隐私、范围纪律、文案手册 —— **均已达标**，
  修完上述 3–4 项后即可交付，不需返工。
- W0 文档 §5 的 **12 项 NOT RUN** 与 O1/O2/O3 的 **NO-GO** 状态**已如实申报且未偷跑**，
  符合施工单 §10「不得将未运行的真实宿主标为已通过」——**这一条本轮做得对**。

---

## 附录 A：本次复审的命令清单（全部本复审者亲跑）

```bash
# 审查区间
git log --oneline f81f062e..HEAD
git diff --stat f81f062e..HEAD
git diff --stat f81f062e..HEAD -- src-tauri/src/remote/zcode.rs   # 空 → P0-1 根因
git diff --stat f81f062e..HEAD -- docs/MASTER-PLAN.md AGENTS.md   # 空 → 范围纪律

# 门禁
pnpm exec vitest run tests/mobile
pnpm exec vitest run tests/mobile/language-connect.test.tsx tests/mobile/App.test.tsx --pool=threads --no-file-parallelism
cd src-tauri && cargo test --locked --lib services::zcode_control::      # 640 passed
cd src-tauri && cargo test --locked --tests                              # 2 failed（P1-2）
cd src-tauri && cargo test --locked --test zcode_control_remote          # 27 passed
pnpm exec tsc --noEmit
pnpm run check:i18n
pnpm exec prettier --check "src/**/*.{ts,tsx,json}" "tests/**/*.{ts,tsx}"
node scripts/check-error-codes.mjs
git diff --check
pnpm test                                                                 # 200 files / 2502 passed

# 基线对照
git archive f81f062e | tar -x -C /tmp/tuvis-baseline
cd /tmp/tuvis-baseline/src-tauri && cargo test --locked --test zcode_control_e2e   # 8 passed（基线绿）

# HTTP 探针（临时用例，已删除还原）
# → queueDelete/queueReorder/queueNow/queueResume/send+heldQueue 全部 400 invalid_action

# 变异实验 ×11（全部改坏→跑→还原）
```

## 附录 B：关键证据文件:行索引

| 主题 | 位置 |
|---|---|
| **P0-1 根因** | `src-tauri/src/remote/zcode.rs:374-413`（`ACTION_KEYS`，本轮零改动）、`:483-510`（`parse_action_request`）、`:497-503`（键集校验）、`:445-447`（400 `invalid_action`） |
| 队列写命令构造 | `src-tauri/src/services/zcode_control/command.rs:547-561`（信封）、`:622-670`（`queue_write_payload`）、`:1622-1673`（`execute_queue_write`）、`:1664-1679`（`fresh_queue_view`） |
| 队列 ACK 分类 | `command.rs:512-517`（`classify_queue_ack`） |
| F2 发送 ACK | `command.rs:475-499`（`classify_send_ack`）、`:1975`（`delivery` 丢弃）、`:1960`/`:1979`（commandId 绑定） |
| F3 CAS | `command.rs:382-425`（`resolve_held_queue`）、`:1947-1949`（载荷展平）、`action.rs:182-195`（指纹含 heldQueue） |
| 能力闸 | `capability/key.rs:105-106,128`（`QueueNow => None`）、`capability/resolver.rs:86-100,117-121`（队列 evidence_ok）、`capability/interface.rs:22-28`（队列恒 false） |
| F1 前端 | `src/mobile/useZcodeControl.ts:187-207`（语义表）、`:284-338`（三个 merge）、`:466-497`（`applySessionView`）、`:505-575`（`applyRead`） |
| F1 服务端 | `src-tauri/src/services/zcode_control/hub.rs:1522-1540`（`should_replace_published`）、`:1553-1580`（`merge_stale_degrade`）、`:1594-1620`（`merge_degraded_current`）、`:1625-1662`（`publish_session`）、`:3339-3345`（GET 不回退） |
| 队列 UI | `src/mobile/ZcodeQueuePanel.tsx:45-50`（白名单）、`:71-78`（`dispatchWritable`/`positionLabel`）、`:80-105`（props，**无 onGuide**）、`:233-248`（恢复常驻）、`:271-391`（行与菜单）、`:320-329`（引导恒禁用） |
| 队列事务 | `src/mobile/ZcodeControlPanel.tsx:685-705`（`waitFreshQueue`）、`:713-734`（`confirmRemoved`）、`:747-813`（`deleteAndRestore`）、`:816-861`（撤回）、`:867-889`（修改）、`:892-921`（冲突三选）、`:924-958`（插队）、`:965-1009`（恢复） |
| F4/F5 | `src-tauri/.../pending.rs:264-300`（纯判定）、`:306-308`（闸复用）、`:540,567-568`（投影）；`types.rs:830,836,842,876-921`；`src/mobile/ZcodePendingCard.tsx:165-179`（回退）、`:283-291`、`:425-472`、`:600-603` |
| 安全 | `src-tauri/.../action.rs:182-246`（`action_params`）、`:249-275`（`audit_summary`）、`:428-441`（落库列） |
## 11. W6 follow-up：修正后独立复核与最终门禁（2026-11-01）

### 11.1 复核范围与结论

复核基线仍为 `f81f062e7d5493f947c6d3dbe0dc5729c95ffb77`。W6 修正处于未提交工作树，最新提交仍为 `4c4050ec868e390a42bd7cd54fb338eca8f5ad0b`；复核同时检查了 `git diff 4c4050ec` 和三个新增的未跟踪设计文件。

**代码层结论：上一轮 HTTP P0-1 已修复；P1-1、P1-2、P1-3 均已按安全合同修正；P1-4 的同前缀误归因也已收口。** 复核发现的 P1-4 反例是“本手机发送 `a`，宿主实际 `startNow`，另一端同期排入新 ID 的 `another task`”：若按 `startsWith(sentText)`，仍会误显示“已排队”。修正后删掉整段队列投递推测，未关联的发送只显示 `accepted/已受理`，官方队列列表独立显示队列事实。

**产品层结论仍是部分符合**：宿主没有提供已验证的删除因果信号，因此“修改”安全降级为撤回请求 + 项目不在队列时提供人工复制；原始“确认未消费后自动回填”未实现。F2 的 `delivery` 已在服务端严格校验，但仍未透传到回执/UI。真实 ZCode 写入行为未验证，不能把合成测试视为实机验收。

### 11.2 逐项复核

| 要求                                                     | 当前代码/修改点                                                                                                                                                                                                                                                | 自动证据                                                                                                                                                                                                                                                                                | 实机证据                    | 剩余风险                                                                            | 判断                                       |
| -------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------- | ----------------------------------------------------------------------------------- | ------------------------------------------ |
| HTTP 接受四种队列动作与 `send.heldQueue`，拒绝契约外字段 | [zcode.rs](/Users/jarvis/Documents/tuvis-worktree2/src-tauri/src/remote/zcode.rs:374) `ACTION_KEYS`、`:441` `HELD_QUEUE_KEYS`、`:507` `parse_action_request`                                                                                                   | [zcode_control_e2e.rs](/Users/jarvis/Documents/tuvis-worktree2/src-tauri/tests/zcode_control_e2e.rs:278) 通过生产 router→Hub→CommandSender，5 个动作每项恰一帧并核对 payload；嵌套多余字段 400 且零新帧。feature suite 内 `queue_actions_and_held_queue_send_cross_http_once_each` 通过 | NOT RUN                     | 合成能力配置可开 `queueNow`，不代表生产已开放；生产“引导”仍关                       | **代码符合**                               |
| 「修改」不得把队列项消失误当撤回成功                     | [ZcodeControlPanel.tsx](/Users/jarvis/Documents/tuvis-worktree2/src/mobile/ZcodeControlPanel.tsx:665) `waitForQueueItemAbsent`、`:692` `deleteAndOfferOriginal`、`:809` `handleQueueEdit`                                                                      | `edit_does_not_restore_when_item_disappears_without_causal_delete_evidence`、`edit_preserves_an_existing_draft_and_offers_original_for_manual_copy`、`draft_written_during_wait_is_never_overwritten`                                                                                   | NOT RUN                     | 草稿不改、不自动发送；用户须核对终端后手动复制。原始自动回填需求未达成              | **安全降级符合合同分支；原始功能部分符合** |
| F2 发送 ACK 绑定当前命令 ID 与 `inputAccepted` 四元组    | [command.rs](/Users/jarvis/Documents/tuvis-worktree2/src-tauri/src/services/zcode_control/command.rs:475) `classify_send_ack`；[zcode.rs tests](/Users/jarvis/Documents/tuvis-worktree2/src-tauri/tests/common/zcode.rs:573) 默认合成 ACK 使用当前 `commandId` | `send_ack_requires_full_input_accepted` 等单测；完整 Rust feature suites 通过                                                                                                                                                                                                           | NOT RUN                     | `delivery` 仍在 `dispatch_send` 解构后丢弃，见 `command.rs:1978`；UI 只显示“已受理” | **受理判断符合；即时 delivery 展示未完成** |
| F3 CAS 失配后刷新并重新征询                              | [ZcodeControlPanel.tsx](/Users/jarvis/Documents/tuvis-worktree2/src/mobile/ZcodeControlPanel.tsx:522) `submitSend` 捕获 `invalid_action`、拉取同代新鲜队列并比较完整有序 IDs                                                                                   | `cas_mismatch_refreshes_queue_and_requires_a_new_choice` 证明失败不自动二次 POST，用户重新选择才用新 IDs                                                                                                                                                                                | NOT RUN                     | 官方在不同执行阶段的真实 CAS 错误码及并发效果未抓包                                 | **本地路径符合；实机部分未知**             |
| F1 revision 单调与降级恢复                               | `useZcodeControl.ts` / `hub.rs`，本轮未改                                                                                                                                                                                                                      | 既有 revision/recovery 用例；Rust 与前端完整 suites 通过                                                                                                                                                                                                                                | NOT RUN                     | 真实网络乱序仍需实机                                                                | **代码符合**                               |
| F4/F5 pending 类型和截断审批安全                         | `pending.rs`、`ZcodePendingCard.tsx`，本轮未改                                                                                                                                                                                                                 | 既有卡片回归纳入完整 `pnpm test`                                                                                                                                                                                                                                                        | NOT RUN                     | 审批正文真实长度和宿主子型仍需实机                                                  | **代码符合**                               |
| U1–U7 与中英手册                                         | [zh 手册](/Users/jarvis/Documents/tuvis-worktree2/docs/user-manual/zh/08-mobile-remote.md:183)、[en 手册](/Users/jarvis/Documents/tuvis-worktree2/docs/user-manual/en/08-mobile-remote.md:179) 更新了安全降级表述                                              | `pnpm test` 200 files / 2503 passed / 5 skipped；`check:i18n` 2629 键通过                                                                                                                                                                                                               | 手机截图/官方端闭环 NOT RUN | “修改”已如实写为人工复制                                                            | **代码层符合，实机验收未完成**             |
| O1/O2/O3 设计交付                                        | 新增 `2026-10-31-zcode-provider-model-selection-design.md`、`2026-10-31-zcode-context-usage-indicator-design.md`、`2026-10-31-zcode-keychain-prompt-reduction-design.md`                                                                                       | 文档格式通过；三份均明确 NO-GO/证据门                                                                                                                                                                                                                                                   | NOT RUN                     | 三个可选功能均未实现，不阻塞主线代码检查                                            | **设计交付符合，功能未实现**               |

### 11.3 自动门禁实跑结果

| 门禁                                                                         | 结果                                                                                                                                                |
| ---------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------- |
| `pnpm check`                                                                 | 退出 0；format、lint、i18n、错误码、TS/Vite build 均完成。ESLint 有 1 条 `uploadCtrlsRef.current` cleanup 警告、0 error；构建有 chunk size advisory |
| `pnpm build:mobile`                                                          | 退出 0；mobile bundle 有 >500 kB chunk advisory                                                                                                     |
| `pnpm test`                                                                  | 200 files / 2503 passed / 5 skipped；jsdom 打印若干未实现 `HTMLMediaElement` 方法提示，退出 0                                                       |
| 施工单定向 mobile set                                                        | 145 passed；新增 CAS/发送用例在全量测试中通过                                                                                                       |
| `cargo check --locked`、`cargo fmt --check`                                  | 均退出 0                                                                                                                                            |
| `cargo test --locked --lib services::zcode_control::`                        | 640 passed / 0 failed                                                                                                                               |
| `cargo test --locked --features marker-helper`                               | 退出 0；3613 passed / 38 ignored                                                                                                                    |
| `cargo test --locked --tests --features marker-helper,zcode-bridge-evidence` | 退出 0；3632 passed / 38 ignored。包含 `zcode_control_e2e` 9 passed，含新 HTTP 闭环                                                                 |
| 两组 Clippy（施工单 §10.1）                                                  | `marker-helper` 与 `marker-helper,zcode-bridge-evidence` 均退出 0，`-D warnings`                                                                    |
| `git diff --check`                                                           | 退出 0；`docs/MASTER-PLAN.md` 与 `AGENTS.md` 无修改                                                                                                 |

### 11.4 Minor（按本轮执行约定暂缓）

1. `tests/common/zcode.rs:573` 用默认 ACK JSON 相等性判断是否启用动态 `inputAccepted`。显式 `set_ack({"status":"accepted"})` 会被当成默认值并动态修正，无法用该接口构造这一种裸 accepted 负例。生产 `classify_send_ack` 仍按严格四元组校验，当前不影响产品路径；后续可把 fake 改成显式默认/override 模式。
2. 中英手册 `zh/08-mobile-remote.md:184`、`en/08-mobile-remote.md:180` 把“当前接入尚未取得可用于确认删除因果的回执”写成“官方没有提供”。这句话的确定性超过现有协议证据；应在后续小修中收窄措辞。

### 11.5 未实机项与最终评估

施工单 §5 的 **12 项 `NOT RUN（需实机）`** 均继续保留；O1/O2/O3 继续 **NO-GO**。特别是：队列命令真实 ACK/result、恢复后是否消费下一项、队列项 `reserved` 的可写性、“引导”是否存在等价命令、发送 `delivery` 是否与队列 ID 可关联，均未在当前设备上验证。

**评估：W6 自动门禁和代码修正通过；尚不能判定完整产品需求或真实宿主验收通过。** 不建议把“全部功能已完成”或“实机闭环通过”作为合并/发布结论。当前安全退化与限制应随代码一并保留。

## 12. 第二轮独立复审修正与复验（2026-11-01）

### 12.1 复审结论

收到的后续复审覆盖 `f81f062e → 4c4050ec` 与当前未提交 W6 修正，指出 3 处代码缺口和 1 处 W0 停止门冲突。逐项复现后完成根因修正；对应回归在修复前失败、修复后通过。施工单的生产门控优先于静态命令表：真实宿主取证未闭合前，普通构建不开放队列写能力；`reserved` 项不派发。

### 12.2 缺口与修正

| 复审发现 | 根因与修正 | 回归证据 |
|---|---|---|
| 同 revision 恢复快照未恢复失效时被清空的暂停队列 | `degrade_view` 安全清空队列后，`merge_degraded_current` 旧逻辑只恢复 ready/能力。现在同会话、同代、同 revision 且权威恢复快照声明 `queue.known=true` 时，同时恢复队列及暂停/可用性字段；旧 revision 仍不能覆盖现有内容 | `same_revision_recovery_restores_cleared_queue_and_pause_state`：修复前红、修复后绿 |
| 未完成 W0 取证却从静态表开放 `queueEdit/queueResume`，且允许 `reserved` | 普通构建的 `QueueEdit/QueueResume` 状态固定为 `None`；只有受编译门保护的 debug evidence/probe 构建可进入候选。Rust 写状态缩为 `queued/admitted`，前端同样禁用 `reserved` 和未知状态 | `queue_edit_and_resume_stay_closed_outside_w0_evidence_build`；feature 构建候选测试；reserved 零帧拒绝与禁用测试 |
| 队列按钮只检查官方 availability | UI 现要求 Tuvis `capabilities.queueEdit.enabled` 和官方 `availability.queueEditAllowed` 同时为真；关门时按钮禁用并显示原因 | `queue_edit_actions_require_both_tuvis_capability_and_host_availability`：修复前红、修复后绿 |
| 审批 POST 绕过队列单写槽 | 审批/问答入口统一通过面板 `runAction`；共享在途状态传给队列面板，审批写请求未结束时队列写入口禁用 | `permission_and_queue_writes_share_the_same_in_flight_slot`：修复前红、修复后绿 |

### 12.3 最新复验与边界

最新工作树的门禁如下：

| 门禁 | 本轮结果 |
|---|---|
| `cargo check --locked`、`cargo fmt --check` | 均退出 0 |
| `cargo test --locked --lib services::zcode_control::` | **641 passed / 0 failed** |
| `cargo test --locked --features marker-helper` | 全套退出 0；库单测 **3614 passed / 38 ignored** |
| `cargo test --locked --tests --features marker-helper,zcode-bridge-evidence` | 全套退出 0；累计 **4150 passed / 0 failed / 46 ignored**；`zcode_control_e2e` **9/9** |
| 两套 Clippy（普通与 evidence feature，`-D warnings`） | 均退出 0 |
| `pnpm check` | 退出 0；格式、lint、i18n（2630 键）、错误码和 TS/Vite build 均完成；ESLint 有 1 条 ref cleanup warning、0 error，Vite 有大 chunk advisory |
| `pnpm build:mobile` | 退出 0；有 configLoader `__dirname` 未来兼容提示及大 chunk advisory |
| `pnpm test` | **200 files / 2506 passed / 5 skipped**；jsdom 打印 `HTMLMediaElement` 未实现提示但退出 0 |
| 施工单定向移动端用例 | **148 passed**；四条新复审回归均包含在定向/全量结果中 |
| `git diff --check`；受保护文件核对 | 退出 0；`docs/MASTER-PLAN.md` 与 `AGENTS.md` 未修改 |

真实 ZCode 宿主的 12 项检查仍为 `NOT RUN（需实机）`；`queueNow/引导` 仍关闭；撤回后无删除因果证据时仍只提供人工复制，原始自动回填需求部分完成。上述修正关闭的是代码缺口与施工门控冲突，不构成实机验收。
