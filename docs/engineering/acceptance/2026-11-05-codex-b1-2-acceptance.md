# Codex B1.2 历史答复证据验收

- 日期：2026-11-05。
- 实现提交：`4dec2316`；执行依据为[设计 v2.4](../specs/2026-11-05-codex-异步答题持久事实可靠性设计.md)、[实施计划 T6—T7](../plans/2026-11-05-codex-异步答题持久事实可靠性实施计划.md)和[旧证据重放裁决](../decisions/2026-11-05-codex-b1-witness-replay.md)。
- 结论：**B1.2 自动化及独立审查验收通过**。B1.3 未实施，总体 B1 未完成。

## 行为与边界

- 迁移新增五个可空证明字段；旧行不自动获得强证据。直接 witness 校验原始 canonical 引用和身份，持久化仅保存身份、版本、摘要和状态，不保存题干、选项、答案或信封正文。
- 运行期共享有界账本累积退役来源，按线程、题目和答复条目核验；同 patch 换答、同 ID 改写或移动不能复活旧来源，真正的新来源不删除已有退役记录。题级超额隔离，全局耗尽关闭未跟踪范围准入。
- 缺页和未加载只撤回有效状态，不虚构永久退役；明确依赖改写或完整历史删除会失效。B1.1 的版本、原 permit 提交栅栏和旧 ACK 保护保留。
- 账本只覆盖当前应用运行期。冷启动可重新验证当前直接证据，但不承诺识别上次进程全部退役来源；本批未新增持久撤销日志或 `prefix_digest`。
- 同进程串行重装可显式传递同一账本；回归测试验证无连接旧实例先停用后的接续，不宣称活连接观察者已排空或并行 adapter 安全。
- 独立 `reviewer_senior` 复核最新实现，未发现新的确定性正确性 P0—P2。移除逐删除项克隆后，按题重复遍历历史仍有未实测的性能风险；不能据此宣称大历史性能或 B1.3 广播隔离已验收。

## 当前源码自动检查

| 检查                                                                                          | 结果                       |
| --------------------------------------------------------------------------------------------- | -------------------------- |
| `cargo test -j 1 --lib b1_`                                                                   | 119 passed                 |
| `cargo test -j 1 --lib codex_interactions`                                                    | 24 passed                  |
| `cargo test -j 1 --lib answer_validity`                                                       | 7 passed                   |
| `cargo test -j 1 --lib codex_desktop_ipc`                                                     | 411 passed，1 既有 ignored |
| `cargo test -j 1 --test codex_desktop_ipc`                                                    | 104 passed                 |
| `cargo test -j 1 --test codex_ipc_remote`                                                     | 完整复跑 266 passed        |
| `cargo check -j 1 --lib`、`cargo fmt --all --check`、`cargo clippy -j 1 --lib -- -D warnings` | 均通过                     |

远程集成套件本轮运行中 `t2_a_rejected_b_accepted_pause_survives` 和 `stop_reconnect_and_late_worker_preserve_other_consumers` 分别曾等待超时，单独复跑和后续整套复跑通过；串行重装接线后的首次整套再次出现后者超时，单项和整套复跑通过。原因均未确认，不宣称已根治；B1.1 [验收记录](2026-11-05-codex-b1-1-acceptance.md)中的既有超时记录保留。

未恢复电脑操控、未代答真实会话或操作真实账本。A 批既有人工真机边界仍待验；B1.3 的广播隔离、自动重试及异步换代尚未实施。
