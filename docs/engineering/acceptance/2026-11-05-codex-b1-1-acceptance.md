# Codex B1.1 实施状态与验收收口

- 日期：2026-11-05。
- 实现提交：`947e699c`、`87d4d3bf`、`7af0946a`；测试观测按[裁决 v2.3](../decisions/2026-11-05-codex-b1-test-checkpoints.md)执行。
- 结论：**B1.1 自动化与独立审查验收通过。** B1.2 和 B1.3 未实施，总体 B1 尚未完成。
- 后续状态：B1.2 已完成收口，见[验收记录](2026-11-05-codex-b1-2-acceptance.md)；B1.3 仍未实施。以上结论保留 B1.1 验收时点。

## 生产入口与状态证据

- `b1_store_post_sql_registry_revocation_rolls_back` 在真实 Store 完成 SQL 准备、取得原 permit 之前，分别完成 bind、invalidate_client、release、reset；四种情况均为 `OwnerFenced`，整批回滚，原行全部字段和 writer epoch 不变，新行不存在。
- `b1_store_db_contention_precedes_gate` 由真实 worker 对同一 DB mutex 的 `try_lock` 得到 `WouldBlock`，随后确认 gate 可取、真实撤销先完成，释放数据库锁后旧写被拒绝且无落库。这证明实际获取发生竞争，**不声称观察到操作系统线程 parked**。
- `b1_store_sqlite_commit_first_blocks_real_owner_revocation` 用隔离 SQLite 读锁与 PENDING 状态验证真实 COMMIT 先胜、撤销等待；真实 COMMIT 失败与 Probe 中止均验证无成功报告和回滚。
- `b1_adapter_owner_fenced_does_not_advance_durable` 分别验证 adapter 的 durable 未推进和 dirty 保留；旧 ACK、新 scope 及空观察的回归亦通过。Store 的错误结果未被单独当成 adapter 内存证据。
- 固定原子 Probe 仅存在于测试构建；普通构建检查通过。独立 `reviewer_senior` 复核检查点位置、公共锁路径、毒锁语义、退出清理和断言，未发现确认的阻塞问题。

## 自动检查

| 检查                                                                                          | 结果                           |
| --------------------------------------------------------------------------------------------- | ------------------------------ |
| `cargo test -j 1 --lib b1_`                                                                   | 63 passed，0 failed，0 ignored |
| 两个精确窗口及 adapter OwnerFenced 定向测试                                                   | 各 1 passed，均实际命中        |
| `cargo test -j 1 --lib remote::pairing`                                                       | 12 passed                      |
| `cargo test -j 1 --lib codex_interactions`                                                    | 23 passed                      |
| `cargo test -j 1 --lib codex_desktop_ipc`                                                     | 351 passed，1 既有 ignored     |
| `cargo test -j 1 --test codex_desktop_ipc`                                                    | 104 passed                     |
| `cargo test -j 1 --test codex_ipc_remote`                                                     | 完整复跑 266 passed            |
| `cargo check -j 1 --lib`、`cargo fmt --all --check`、`cargo clippy -j 1 --lib -- -D warnings` | 均通过                         |

远程集成套件首次运行时 `t2_a_rejected_b_accepted_pause_survives` 等待超时，其余 265 项通过；该用例单独复跑通过，随后整套复跑 266 项通过。本记录不将首次失败抹去，也不据此断言其根因。

## 边界

B1.1 同步撤销可能等待 COMMIT；慢库隔离、后台重试和异步换代属于 B1.3。历史证据与失效核对属于 B1.2。本轮未恢复电脑操控、未做真实会话代答；A 批尚需的人工真机边界仍按原验收清单处理。
