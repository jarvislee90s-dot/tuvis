# Codex B1.2 多次失效与旧答复重放裁决

- 日期：2026-11-05。
- 范围：现有 B1.2 T6—T7 收口，不重做 B1.1，不提前进入 B1.3/B2/C。
- 依据：[设计 v2.4](../specs/2026-11-05-codex-异步答题持久事实可靠性设计.md)、[实施计划](../plans/2026-11-05-codex-异步答题持久事实可靠性实施计划.md)、[进度审查](../reviews/2026-11-05-codex-b1-2-progress-review.md)。
- 状态：规划会话定案。原 reviewer_senior 独立复核确认单条撤销记录不足，以及同 patch 替换的另一个遗漏；以下明确最低保守范围，产品修复仍由执行 agent 完成。
- 后续状态：B1.2 已按本裁决完成自动化与独立审查，结果和限制见[验收记录](../acceptance/2026-11-05-codex-b1-2-acceptance.md)。

## 1. 已由真实 Rust 路径复现的两个 P2

| 路径                                                 | 当前结果 | 应有结果                   |
| ---------------------------------------------------- | -------- | -------------------------- |
| Q0/W1 有效→Q1 撤销 W1→W2 有效→Q2 撤销 W2→原样恢复 W1 | Answered | Unknown，W1 已撤销         |
| 单个 patch 同时 Q0→Q1、W1→W2，再恢复 W1              | Answered | Unknown，替换时也应退役 W1 |

主会话临时增加独立集成测试，调用当前生产 crate 的 `ThreadMirror.apply/project`，运行 `cargo test -j 1 --test b1_replay_review_probe`：两条均真实断言失败，left=Answered / right=Unknown。不是编译错误，也不是模型替代。

临时测试源在结束后已删除；mirror.rs、async_question.rs、migration.rs 前后 SHA-256 未变。永久回归须由执行 agent 加入现有 Rust suite，不把本记录当成修复通过。

当前根因：`invalidated_answers` 对每题只保存一个 witness，新 witness 不同就 remove；失效条件还遗漏了“新旧答复不同、当前题仍有答复”的同时替换。

## 2. 定案：累积的负证据，按候选核验

1. 按同一 ThreadKey、官方 questionKey 保留单调累积的已撤销答复来源，不再以一条最近记录代表全部历史。
2. 已证实的题目依赖变化、旧答复来源替换、明确的历史删除/改写发生时，收集受影响的全部旧 witness。即使同一个 patch 同时出现新答复，也必须退役旧来源。
3. 仅在整批 patch/snapshot 成功采用后合并撤销记录；非法操作、预算拒绝等未生效变化不能污染已接受历史。
4. 新合法 W2/W3 可以恢复 Answered，但不能删除 W1/W2 的旧撤销记录。投影按当前候选是否命中判定，不能用“本题曾有任何撤销”永久封死所有新答案。
5. 当前候选命中撤销记录，或当前范围已 quarantine → Unknown、actionable=false；没有候选时仍走既有完整性/失效规则，不新增直接回 Pending 的旁路。
6. 同步 adapter 捕获与持久化：被 guard 拒绝的候选不能以 `valid` 写回 hints，也不能靠旧成功 ACK 恢复旧状态；继续使用 B1.1 的观察版本和提交许可。

### 2.1 答复来源身份

使用稳定的答复来源身份识别旧条目，不以答案文本单独匹配。至少绑定 ThreadKey、questionKey、answer_item_id；完整 witness 的版本、answer_turn_id 与 digest 用于内容版本核对和审计。

已经退役的同一 answer_item_id，原样恢复、改内容或移动位置均不能仅因 digest/turn 不同就被视为新来源。真正的新唯一 userMessage.id 且通过现有合法性校验，可作为新答复候选；如果某上游版本确有复用 ID 的新答复机制，须有独立协议证据，不能猜测放行。

一条正式消息可回答多题，撤销按 questionKey 隔离，不能因一题失效把同消息中其他题一起污染。writer epoch、revision、owner generation、题干相似度均不能代替答复来源身份。

### 2.2 未验证与确定退役分开

单纯缺页、未加载、视野不足或不能区分裁剪/删除时，只能撤回当前有效状态为 Unknown/unverified，不能凭分析器没返回 witness 就补造“已证实语义删除”的永久退役。当前重新直接核验、依赖一致且未命中确定退役来源时，仍可恢复有效；这不等于遗忘已经证实的 W1/W2 退役。

ledger 可保存无正文的已观察题目依赖摘要及当前验证状态，用于在状态恢复时比较；这些也计入预算。无法证明依赖连续时继续 Unknown，不把数据缺失当相等。不能只用 `previous_witnesses - current_witnesses` 作为所有永久退役的判据。

## 3. 保存范围与对象生命周期

**本批保证：当前应用运行已经观察并撤销的同线程证据，不因 snapshot、owner 换代、镜像 clear/删除重建、远程停止恢复而被遗忘。**

因此负证据不能只放在 ThreadMirror 对象内。由应用运行级共享 adapter 状态持有独立 `Arc<AnswerValidityLedger>`（可用同职责名称），ThreadMirror 取得相同引用。生产 `shared_production_adapter` 的 OnceLock 可提供该寿命；新建镜像应重新挂回同一 ledger，不复用 owner 临时身份作为清理依据。

- owner/permit scope 的撤销只改变写权限，不能清除历史负证据。
- `clear_state`、`apply_snapshot`、镜像释放不 clear 该 ledger；新 snapshot 的当前候选仍受已知负证据约束。
- snapshot/owner 换代本身不等于所有旧答复来源都已退役；内容依赖一致的直接候选仍可重新核验。只有实际依赖变化/删除/替换等已知失效才增加撤销，不能仅因收到 snapshot 就永久拉黑全部旧答案。
- 旧镜像与新 snapshot 对比应在覆盖旧状态前收集失效 delta，采用成功后才合并。
- 同一进程内确需重装 adapter，必须传递同一运行 ledger；不能通过新建对象绕过已知失效。
- 独立纯函数测试可建自己的 ledger；生命周期测试必须明确共享，不能用全新隔离 ledger 假装重建已保留信息。

### 3.1 跨进程重启的边界

本批不新增持久化撤销日志，继续原 T6 五列。它们只保存最近 witness，无法还原所有 W1/W2 的撤销历史；集合总 hash 也不能恢复缺失成员。

按既有设计 §4.1，冷启动没有当前有效证明时 Unknown；重新直接读到合法候选可重新取证，但**不承诺拒绝所有上次进程曾撤销、此次重新出现的 witness**。不把本运行的“已知撤销不遗忘”夸成跨重启完整防重放。

若要更强的跨进程保证，必须独立设计持久撤销信息或持久 quarantine 并新增迁移/清理策略，不能由执行者悄悄添加。B2 的主动补读也不能自动恢复未存储的全部撤销信息。

## 4. 有界保存与容量耗尽

不使用无界集合，也不通过 TTL/LRU 删除已知撤销后默认放行。

首版配置可注入测试：每题最多 256 个退役答复来源，最多 4096 个已跟踪题键，总元数据预算 4 MiB。这些是待验收的保守预算，不是已经测量出的容量保证。键、来源、digest、集合/标记的内存估算都计入，不能只计 payload 字节。

- 题目预算耗尽：原题 entry 置 quarantine，可释放详细集合回收空间，但该题本运行持续 Unknown，不能通过清镜像或出现新 W3 自动解除。
- 全局预算/题键数耗尽：关闭未跟踪范围的新追踪准入，不为每个新线程/题目另建无限 quarantine 标记；未跟踪范围 Unknown。已跟踪范围按自身有效/quarantine 状态继续，无法保存新增撤销时将该已跟踪题置 quarantine。
- 追踪关闭标记和已有 quarantine 不因释放空间、reconnect 或 stop/start 自动归零；否则丢失的负证据又会被当成“未命中”。
- 题目级耗尽不污染其他正常题/线程；全局耗尽也不禁用其他 CLI 或原生审批等无关能力。诊断仅记录类别和预算，不含题干/答案。

ledger 的更新/查询是短内存操作，不持提交 gate 或 DB 锁调用它；不把它塞进 COMMIT 回调，不引入新循环等待。

## 5. 必须纳入永久回归的情形

| 用例建议名                                                 | 预期                                                         |
| ---------------------------------------------------------- | ------------------------------------------------------------ |
| `b1_retired_w1_cannot_revive_after_w2_cycle`               | 用户原始多轮失效序列最终 Unknown                             |
| `b1_same_patch_replacement_retires_old_witness`            | 同批改题换答也记录 W1 撤销，恢复 W1 不放行                   |
| `b1_new_w3_does_not_erase_old_revocations`                 | 预算内新 W3 可 Answered，再恢复 W1/W2 仍 Unknown             |
| `b1_retired_answer_id_cannot_rebind_by_edit_or_move`       | 已退役 ID 改内容/移动位置不能靠新 digest 洗成新来源          |
| `b1_rejected_patch_does_not_retire_witness`                | 整批变更未采用，不写入撤销 delta                             |
| `b1_revocations_survive_snapshot_owner_and_mirror_rebuild` | 同运行共享 ledger，几种对象/连接重建均不遗忘                 |
| `b1_revocations_survive_remote_stop_start`                 | 远程停开仍共享应用运行 ledger                                |
| `b1_revocation_budget_quarantines_without_eviction_bypass` | 超额 Unknown；其他正常题不受题级超额污染                     |
| `b1_global_revocation_budget_bounds_untracked_keys`        | 关闭未跟踪准入，不生成无限标记、不因重新分配放行             |
| `b1_cold_start_reproof_scope_is_explicit`                  | 当前合法直接证据与仅旧 unverified 行分别处理，记录跨进程限制 |
| `b1_replayed_witness_is_not_persisted_valid`               | 经过镜像、adapter、Store 仍为 Unknown/unverified，不写 valid |

其中两条已由主会话真实 Rust 失败验证；其他是修复方案必须补的边界，不冒称均已复现。新失效版本/迟到 ACK 的既有回归继续保留。

另补 `b1_unverified_window_does_not_invent_retirement`：明确缺页/未加载只变 Unknown；恢复一致当前证据可重新确认，已证实的旧来源退役仍保持。该测试是方案边界，不冒称本轮已验证失败。

## 6. 接续顺序

先把上述两个真实失败案例放入永久测试并确认红测；实现共享、受预算约束的负证据与完整退役规则；定向绿测后运行当前源码对应的全部 B1、数据层、IPC 单元、两组 fake 集成、fmt/Clippy/普通构建。

修后由独立 reviewer_senior 检查负证据保存范围、容量退化、同 patch 顺序、权限与持久化接缝，再更新 B1.2 验收并提交。修复前或更早版本通过的 79/266 项不能当作最终实现验收。

B1.3 T8—T10 的读内存、后台写入/重试、容量分批、异步换代与停止交接仍全部独立；本次裁决不关闭它们。
