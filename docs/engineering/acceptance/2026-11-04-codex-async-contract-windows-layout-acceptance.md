# Codex 异步答题契约 Windows 布局兼容 · 实施与验收记录

- 日期：2026-11-04
- 关联 issue/PR：本批 PR（`fix/codex-async-contract-windows`）
- 前置文档：
  - `docs/engineering/acceptance/2026-10-28-v0.6.3-Codex异步答题识别修复验收.md`
  - `docs/engineering/evidence/codex-desktop-ipc/2026-10-28-codex-app-26.1023.51421-async-envelope-reprobe.md`
  - `docs/engineering/specs/2026-10-26-Codex-F01-F08阶段需求与设计.md`

## 1. 问题现象

用户通过 Tuvis 移动端（远程连接）查看 Codex 桌面会话时，异步答题卡显示：

> 「当前 Desktop 版本未匹配到异步答题契约，暂不能从手机提交；答案已保留，可到桌面端作答或查看适配详情」

即 `reason_code = operation_contract_unmatched`，卡片无法提交，且**桌面端作答后卡片不会消失**。

## 2. 根因（真机取证）

### 2.1 不是规则失配，是探测根本没跑到

`capability.rs` 里同一个 `CONTRACT_UNMATCHED` 文案有**两个来源分支**：

```rust
} else if context.contract_error.is_some() {
    codes::OPERATION_CONTRACT_UNMATCHED      // ← 实际走这条
} else if evidence == EvidenceSource::None {
    if context.contract_identified { OPERATION_CONTRACT_UNMATCHED }
    else { OPERATION_NO_EVIDENCE }
}
```

真机日志与账本证据：

```
mam.log: codex-ipc contract: key=windows|26.1029.7803.0|0.166.0-alpha.6|1
         识别失败（contract_package_unavailable）
tuvis.db: codex_ipc_contract_evidence 表 0 行   ← 该机器从未成功识别过任何 build
```

**断点**：`desktop_contract.rs` 的 `cli_path_in_bundle` 硬编码旧路径
`app\resources\codex-cli\bin\codex.exe`；而 Codex ≥ 26.1027 已把随包 CLI 提到
`app\resources\codex.exe`。

实测（`C:\Program Files\WindowsApps\OpenAI.Codex_26.1029.7803.0_x64__2p2nqsd0c76g0`）：

| 路径                       | 状态               |
| -------------------------- | ------------------ |
| `app\resources\codex-cli\` | **不存在**         |
| `app\resources\codex.exe`  | 37 MB ✅           |
| `app\resources\codex`      | 319 MB（单文件版） |

### 2.2 版本号双体系（易误导）

| 显示位置                             | 值                                                                   |
| ------------------------------------ | -------------------------------------------------------------------- |
| 界面「版本」                         | `26.1029.61003`（来自 `app\resources\owl-app.ini` 的 `AppVersion=`） |
| **MSIX `AppxManifest.xml` Identity** | **`26.1029.7803.0`** ← `profile_for()` 走这个                        |

排查时必须以 MSIX 版本为准。

### 2.3 规则层还有两处词法钉死（修好路径后才会暴露）

用 Node 只读解析 `app.asar`（720 MB），**先用仓库 20261028 夹具做阳性对照**确认解析器可信，再在真机包上比对：

| 条件         | v2 要求                                                  | 真机 26.1029.7803.0 实际                                                     |
| ------------ | -------------------------------------------------------- | ---------------------------------------------------------------------------- |
| 信封解包切片 | 字面 `slice(34,-35)`                                     | `slice(Sc.length,-Sd.length)`（标记绑定推导）                                |
| 题目标识合成 | `function F(P1,P2){return JSON.stringify(...)}` + 调用点 | 内联进 map 箭头 `(e.questions??[]).map((t,r)=>JSON.stringify([...,e.id,r]))` |

**能力本身完整**：信封、守卫、schema、合成、carrier 全链在场，与 26.1023 语义等价。
排除「OWL 定制版砍掉功能」与「压缩器改名」两种假设。

## 3. 修复内容

### 3.1 CLI 路径双布局

`desktop_contract.rs::cli_path_in_bundle` 改为新布局优先、旧布局兜底：

```rust
let resources = bundle.join("app").join("resources");
let modern = resources.join("codex.exe");                                  // ≥26.1027
let legacy = resources.join("codex-cli").join("bin").join("codex.exe");    // ≤26.1016
```

保持「只 `is_file()` 判定、不执行、不读内容」的既有契约。

### 3.2 契约规则 v3

- **切片**：接受数字字面量 **或** 从已核验的标记绑定取 `.length`
  （后者天然免疫标记长度变化）。
- **合成**：接受命名函数+调用点 **或** 内联进 `.map` 箭头；
  内联形态**复用同一套参数关系判据**（第三元素 = map index 形参、
  第二元素 = 同 map 源条目的 `.id`、map 源 = `(<条目>.questions??[])`）。
- `requires` 里的 `slice(34,-35)` 字面门槛改为 `.slice(`（形态由 structural 规则核验）。
- `CONTRACT_RULE_VERSION` **2 → 3**：v2 证据行对该 build 落账为「不匹配」，必须作废重识别。

### 3.3 移动端手动收纳

组件 `AsyncQuestionGroupCard` 早有 `onArchive` 按钮（「在 Tuvis 中收纳本组」），
但 `SessionDetail` **从未传该 prop**。本批接上：

- 收纳写 `sessionStorage` 标记 → `inbox` 过滤隐藏该组，**不向桌面发送任何动作**。
- 已收纳组单独回传，dock 顶部提供「恢复显示」入口。
- 收纳键含 `sourceTurnId/sourceItemId`：桌面若有新回合，题目换代即生成新 key，
  卡自然重现——不会被一次收纳永久吞掉。

## 4. 测试

### 自动化

| 用例                                                     | 断言                                       |
| -------------------------------------------------------- | ------------------------------------------ |
| `cli_path_accepts_both_windows_layouts`                  | 新布局命中 / 旧布局命中 / 两者皆无 → None  |
| `async_contract_accepts_derived_slice_lengths`           | `slice(Sc.length,-Sd.length)` 形态匹配成功 |
| `async_contract_accepts_inline_question_id_synthesis`    | 内联形态匹配成功                           |
| `async_contract_rejects_inline_with_index_derived_item`  | **阴性锁**：内联但第二元素取 index → 拒绝  |
| `contract_rule_version_bumped_for_structural_async_rule` | 当前版本 = 4；旧 v3 证据需重新识别         |

`cargo test --lib codex_desktop_ipc`：**298 passed, 0 failed**。

## 5. 真机验收（待执行）

前置：重启 Tuvis（使新规则生效，并让契约证据按当前 v4 重探）。

### 5.1 契约识别恢复

| 项                                                   | 期望                                                  |
| ---------------------------------------------------- | ----------------------------------------------------- |
| `mam.log` 中 `codex-ipc contract` 行                 | 不再出现 `contract_package_unavailable`               |
| `~/.tuvis/tuvis.db` 的 `codex_ipc_contract_evidence` | 出现 `windows\|26.1029.7803.0\|0.166.0-alpha.6\|1` 行 |
| 移动端卡片                                           | 不再显示「未匹配契约」，按钮可提交                    |

### 5.2 「正式提交 → Desktop 原卡撤下」闭环

> 该路径在 2026-10-28 验收中标注为**未执行（待验）**，本批必须补做。

在**隔离会话**中验证四件事同时成立：

1. 手机提交回执 `Accepted`
2. Desktop 原问答卡撤下
3. 会话里出现 `<send_user_message_question_reply>` 信封正文
4. 模型按答案继续

任一项不成立记失败。

### 5.3 收纳与恢复

| 步骤                      | 期望                                       |
| ------------------------- | ------------------------------------------ |
| 点「在 Tuvis 中收纳本组」 | 卡片立即隐藏，dock 顶部出现「已收纳 1 组」 |
| 点「恢复显示」            | 卡片重新出现，草稿仍在                     |
| 桌面该会话产生新回合      | 旧收纳不影响新题（新 key）                 |

## 6. 已知限制

- **compat 档不因此解锁写入**：`profile_for` 白名单未新增该 build，
  写能力仍走契约证据门控（`auto_adapt_enabled && contract`）。
  若要 shipped 档位，需补完整只读探测取证后单独登记。
- **macOS 未验证**：本批形态变化取证仅限 Windows 26.1029.7803.0；
  macOS 包若出现同类内联/推导形态，v3 规则同样适用（形态判据平台无关），
  但未实测。

## 6.1 过期答题卡自动纠正（L1 + L2，2026-11-05 追加）

### 问题

桌面端已作答后，移动端仍挂出待作答卡。手动收纳是唯一出口（用户兜底）。

### 为什么不按「对话推进程度」自动隐藏

用户曾提议「卡片出新题后，若正文已多出很多内容就绕过该卡」。**该方案不可行**：

- `request_user_input_async` 是**非阻塞**语义：模型提问后继续干活，用户可很久不答。
- 仓库有钉死测试 `f08p_t03_historical_unanswered_survives_completed`
  （`async_question.rs:885-915`）：turn-1 带题且 completed + turn-2 无关消息
  → 仍 `Pending` + `actionable`。
- spec `2026-10-27:49` 明文禁止「回合完成自动等同异步题已答」。
- 真机台账 `async-question-ledger.sanitized.json:30-38`：官方客户端对一道题
  **挂 5 小时、跨多轮对话**才被作答。
- 反直觉点：`coverage == Complete` 且 `answered_map` 为空时，「后面有很多 turn」
  恰恰**证明用户聊了很多但真没答**——隐藏卡就是隐藏真待答。

### 实际断点（两处）

1. **`answered_map` 的 `known` 集合只含窗口内可见题**（原 `async_question.rs:441-444`
   的成员检查）：题干条目被 Desktop 尾窗淘汰后，作答信封因键不匹配被静默丢弃
   → 永远派生不出 `Answered` → hint 永久滞留 `pending`。
2. **persist 只有一个调用点**（`adapter.rs:4730`，在 `projection()` 尾部）：
   正常状态帧推送路径（`handle_state_changed`）不落库 → 桌面答题后视图实时清了，
   但 DB hint 滞后；页面未开该会话时无限期。

### 修复

**L1 — 信封配对覆盖窗口外的题**（`async_question.rs`）

- `known` = 可见题 ∪ 本线程 hint 派生 key（`hint_derived_keys`，
  由 `source_item_id + question_index` 无损重建 `questionItemId`）。
- `questions.is_empty()` 早退分支与主循环后的占位合并，均对命中信封的占位
  改判 `Answered`。
- **必守**：仍可见的题继续走 `coverage_since`（Complete → `Pending`），
  绝不降级；未命中信封的 hint-only 题保持 `Unknown`（前端不算 `live`，
  不渲染成待作答卡）——**不猜已答**。

**L2 — SSE 路径也写穿 hint**（`adapter.rs`）

- 正常 `handle_state_changed` 路径投影前在镜像锁外加载历史提示；发布前调用
  `persist_interaction_hints` 写穿（fact 未变不写）。初始化与写穿由异步闸串行，
  读取失败不标记已加载。首次广播可据同线程正式信封核销窗口外旧题。

### 新增测试（`async_question.rs`）

| 用例                                                             | 断言                                          |
| ---------------------------------------------------------------- | --------------------------------------------- |
| `hint_derived_key_matches_envelope_after_question_scrolled_out`  | 题干不在窗口 + 信封在 → `Answered`            |
| `hint_only_question_without_envelope_yields_unknown_not_pending` | 无信封 → `Unknown`（非 `Pending`），不可提交  |
| `visible_question_still_pending_when_coverage_complete`          | **回归锁**：可见题仍 `Pending` + `actionable` |
| `visible_question_unaffected_by_other_hints`                     | 多题并存时旧题收口不影响新题                  |

历史结果：`cargo test --lib codex_desktop_ipc` 曾为 302 passed。本批当前结果见下方补充。

### 剩余边界（L1 无法覆盖，手动收纳仍是出口）

- **桌面用非官方入口作答**（未生成 `<send_user_message_question_reply>` 信封）
  → Tuvis 无法确认已答，按纪律不猜。
- **IPC 漏帧且镜像从不重取**（`ensure_thread` 对新鲜镜像短路）→ 信封根本没进镜像。
- **信封与题干都滑出窗口** → 无证据可配对。

### 2026-11-05 PR #202 A 批补充

- A1：首次状态投影前加载历史提示，并串行化加载和写穿；假连接首次广播验证旧题正式核销、新题保留及提示落库。
- A2：手动收纳保留组组件与当前页面草稿、选项和动作锚；组换代及线程切换各自隔离。
- A3：拒绝回调形参遮蔽题目来源的内联表达式，契约规则升为 v4，旧证据重新识别。
- 自动测试：`cargo test --lib codex_desktop_ipc` 312 passed / 1 ignored；
  `cargo test --test codex_desktop_ipc` 104 passed；前端两套件 67 passed。
  远程集成套件最终复跑 **266 passed**。此前两项 SSE 测试在本批及同机基线均失败：测试误以为缓存命中后必有两次重发布，实际只发生一次，竞态钩子未触发；调整为拦截首次重发布后，两项定向及整套通过，禁用后不回退断言保持不变。另一次全套运行的 `stop_reconnect_and_late_worker_preserve_other_consumers` 超时，单独复跑及最终全套均通过，仍按时序不稳定记录。
- `pnpm check` 通过（ESLint 5 条警告），`pnpm build:mobile` 通过；`cargo clippy --lib -- -D warnings` 通过。
- 隔离 Desktop / 手机真机验收：未执行；本记录的自动化结果不能替代正式答题与原卡收敛证据。
- 慢库隔离、写入失败重试、历史补读及语义识别仍属于手册 B/C 后续独立批次。

### 隔离环境验收（2026-11-05）

Windows 隔离验收构建使用单独的 Tuvis 数据目录和 WebView 配置，并在验收副本中禁用 Hook 注册、Skill 补链、用量采集等无关启动副作用；因此该构建不是未经修改的发布包。Codex Desktop 26.1029.7803.0 的当前包通过只读契约识别；隔离数据库中 `async_answer_idle` 证据规则版本为 4，包 SHA-256 前缀为 `77ed4ddc5b38`。移动页面适配详情显示该能力契约匹配，但当时存在原生阻塞提问，故不可提交异步答题。

隔离验收随后以本机浏览器模拟移动端，在专用 Test 聊天生成一题异步选择题：选项收纳并恢复后保持选中；一次提交后移动卡消失，原会话存在同题标识的正式答复，模型按答案继续，隔离 hints 行更新为 `answered`。随后在 Desktop 专用聊天直接核对，原题已收起；本次浏览器代替手机，且使用屏蔽启动副作用的验收构建，不能表述为发布包及实体手机全面验收通过。其余场景的后续结果和限制见下。

隔离版重启后重新进入同一测试聊天，已回答题未重新显示为待答卡，聊天中仍可见正式答复及模型回复。此检查未构造旧题滑出历史窗口并与新题并存的情况，不替代该场景的完整验收。

同一专用聊天随后生成一组两题异步问题；发题回合完成后仍显示待答。第一题输入自由文本后收纳、恢复，原稿保留。用户在 Desktop 原生提交整组后，已打开的移动页面无需手动刷新即移除待答卡；原会话中的正式答复覆盖两个精确题目身份，隔离 hints 的两条记录均为 `answered`。这验证了该版本的 Desktop 先答和自由稿恢复；两道题属于同一组，不作为独立新组换代证据。

新组换代检查：先在移动页面选中旧题并收纳；随后用户关闭 Desktop 旧题并在新回合生成另一道异步题。移动页面仍有旧组收纳入口，新题独立可见且无旧选项。隔离库中两题提示均为 `pending`，未因回合结束或关闭原卡伪造已答；本次未在 Desktop 保持旧原卡打开，不能据此声称验证了该变体。

经用户授权，在专用聊天追加两轮不回答测试题的短回合后，Desktop 仍显示该未答原题；但隔离版重启后将该聊天列为历史，移动端未建立实时连接，不能据此认定移动端仍显示待答。旧题滑出 Desktop 实际历史窗口的场景未构造，仍待验；不能把消息列表的截断等同于事实历史窗口截断。

### 未完成实机场景的自动证据（2026-11-05）

| 场景                 | 本轮自动检查                                                                                                                                           | 边界                                          |
| -------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------- |
| 窗口外旧题与新题并存 | `pr202_first_snapshot_reconciles_old_answer`：1 passed；假连接首帧核销旧题、保留新题并写入提示                                                         | 模拟历史窗口，不是 Desktop 真机窗口截断       |
| 未答跨回合及缺失证据 | `async_question::tests`：13 passed，含完成回合后未答保留与历史缺口 Unknown；`hint_only_question_without_envelope_yields_unknown_not_pending`：1 passed | 自动投影证据，不能替代断链／缺页真机验收      |
| 窗口外题目配对       | `hint_derived_key_matches_envelope_after_question_scrolled_out`：1 passed                                                                              | 函数级模拟，不证明真实 Desktop 的历史窗口形态 |

按用户要求，后续不再进行电脑操控；未完成的实体手机和真实窗口边界保留待验。

### 2026-11-06 双端正式答题复验

- Windows Desktop MSIX 26.1103.3517.0 的安装包通过 v5 只读契约识别，六项操作均匹配。新版由类型与长度守卫后直接遍历同一条目的题目，题目标识继续使用原条目 ID 和题序。
- 普通助手条目的 `questions: null` 等同无题。非空非法结构继续保守处理。
- 用户在当前开发构建清理旧卡后测试两道新题，分别在 Desktop 原生卡和远程卡正式提交，另一端均撤下对应题。
- 运行中可保留草稿，空闲后正式提交。运行中提交单独跟踪 [#203](https://github.com/jarvislee90s-dot/tuvis/issues/203)。直接跳过或关闭后的当前出口为 Tuvis 本地收纳。
