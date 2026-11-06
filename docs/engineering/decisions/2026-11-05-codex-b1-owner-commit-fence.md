# Codex B1 owner 换代与事务提交的原子边界

- 日期：2026-11-05。
- 范围：B1.1 T3/T4；同时补足既有 mirror scope 校验。B1.3 的广播非阻塞另行验收。
- 依据：[设计 v2.2](../specs/2026-11-05-codex-异步答题持久事实可靠性设计.md)、[实施计划 v2.2](../plans/2026-11-05-codex-异步答题持久事实可靠性实施计划.md)。主会话定案，原 reviewer_senior 已独立复核拟定锁序与语义。

## 1. 反例与定案

旧侧载通过写前复核→挂起→owner 换代→旧任务提交。writer epoch 仍相同，DB 又无更新 hint，CAS 不能拒绝。再查一次 watch/AtomicBool 或在检查后放锁，仍然存在 TOCTOU。

**定案：每个 lease 一把可撤销提交栅栏；最终校验与真实 SQLite COMMIT 共享同一个 guard，owner 撤销也走该栅栏。** 不新增持久表/列，不因单线程 owner 换代旋转全局 writer epoch，也不伪造一条新 hint 来充当 fence。

## 2. authority 与 scope

每个有效 OwnerLease 新建 `Arc<HintLeaseAuthority>`，绑定完整 lease 身份，内部同步互斥状态至少为：

```text
owner_valid: bool
current_mirror_scope: Option<唯一 scope id>
```

- 新 lease 使用新 authority，旧 authority 一旦 owner_valid=false 永不恢复。
- 镜像 snapshot/clear/stale/恢复条件撤销或旋转 scope；scope id 在 authority 内递增、不复用，不能只用可能重复的 build_generation。溢出或毒锁保守拒绝提交。
- 原纯内存 `HintObservation` 保留已实现字段。adapter 同次观察另捕获私有 `HintCommitPermit`，包装为 `AuthorizedHintObservation`；permit 持有原 authority 与预期 scope。
- 不在 persist 时按 thread id 重新配“当前 permit”给旧观察，不序列化或持久化 permit；它不是历史连续性证明。
- 旧 scope 的迟到清理只有在 expected scope 仍匹配时才能撤销；复核原 lease/镜像身份后才能操作镜像，不能清掉新 scope 或新镜像。

## 3. 提交协议

1. 完成排队、取得 DB 锁、开启事务、准备全部 SQL 和逐项 report；等待 DB 时不提前持 gate。
2. 生产 Store 从原 permit 取得单一 gate guard，核验完整 owner 身份、owner_valid 和预期 scope。
3. 失效则不 COMMIT，回滚并返回 `Fenced`（区分 owner/scope）；不推进 durable，不按 Storage 暂时失败反复重试旧上下文。新 scope 真实观察重新入队。
4. 有效则跨**真实 COMMIT**持有同一个 guard，直到提交成功或失败退出。只有 COMMIT 成功才产出成功 report，提交失败不冒称 Applied。
5. 释放 gate 和 DB 锁后再做 HintState、通知、日志与缓存更新。过期 ACK 可以忽略，不能清新 dirty、恢复旧权限或认领新 scope 的成功。

DAO 保持数据职责：提供内部“准备事务+report”入口，返回持有事务的 prepared batch，由生产 Store 在 permit guard 内调用其真实 commit。不能用 before_commit 返回 bool、放锁后再 commit。无 permit 的便捷提交只用于隔离 DAO 测试；生产 hints 写入口必须传入原 permit。

一个批次只含同一 authority/scope，不跨线程混批同时获取多把 gate。

## 4. 两种合法顺序

| gate 内先完成的边界           | 必须得到的结果                                                           |
| ----------------------------- | ------------------------------------------------------------------------ |
| revoke 先使权限失效           | 旧事务最终校验失败，回滚，即使 writer epoch 不变、DB 没有新 hint         |
| COMMIT 先取得 gate 并校验成功 | revoke 等待提交退出；旧事务可以提交，随后撤销完成，新 lease 才可有效发布 |

线性化按 gate 内实际 COMMIT/撤销排序，不按任务开始、失效事件到达或外部预检查时刻排序。不能追回已成功提交的事务，也不能把提交已成功但 ACK 迟到说成“数据库零写入”。本保证针对本地处理的状态，不声称已获知尚未收到的上游 owner 变化。

`OwnerRegistry::bind` 替换旧绑定、`invalidate_client`、`release`、`reset` 均在对外完成撤销/安装新 lease 前经旧 gate 永久失效，不能只发送 watch=false。镜像变更同样经该 gate 条件撤销/旋转自己的 scope，未就绪时为 None，安装成功后才产生新 scope。

## 5. 锁序

| 路径            | 允许顺序                                                             |
| --------------- | -------------------------------------------------------------------- |
| worker          | DB → lease gate → COMMIT/事务收尾；不再取得其他应用锁                |
| owner 撤销      | owners → lease gate 更新权限 → 释放 gate → 后续处理                  |
| 镜像 scope 变化 | mirrors → lease gate 更新权限 → 释放 gate → 之后才做可能读 DB 的投影 |

禁止 worker 持 DB/gate 再取 owners、mirrors、HintState；禁止撤销路径持 gate 查 DB、投影、通知、回调或 await。COMMIT 内回调也不能取得这些应用锁。保留的 Registry 写前预检查只能位于 DB/gate 外。

v2.3 的[测试观测裁决](2026-11-05-codex-b1-test-checkpoints.md)仅允许 cfg(test) 实例级固定纯原子检查点，非测试构建移除；不允许注入外部业务回调、发送消息或追加应用锁，生产锁协议不变。它用于补齐实际 Store 的精确窗口证据。

**B1.1 限制：** 同步 revoke 可能等待持 gate 的 COMMIT，等待没有固定短时上界；持 owners/mirrors 等待也可能牵连其他线程。不能因为最终只改 bool 就称其短锁，不宣称已完成 B1.3 慢库隔离。

## 6. B1.3 必须补的异步 transition

1. 该线程进入 TransitionPending/Revoking，关闭新动作与新观察准入；pending 不等于 revoke 已完成。
2. 释放应用锁，后台等待 gate 完成撤销；forwarder 不 await 该等待，继续处理其他线程。
3. 以 transition ticket 和原身份复核后，才完成失效或安装新 lease。pending 期间该线程增量不当完整镜像应用，随后复用既有 owner 恢复流程取得 fresh snapshot。
4. 覆盖并发 reset/rebind/stop，迟到转换不能撤销或发布更新的 lease。

不能用“去掉持久化 await”代替本项。B1.3 慢库测试须覆盖 COMMIT 持 gate 时收到 owner 失效帧。

## 7. 必须补充的受控测试

| 用例                                                            | 时序与断言                                                                                                               |
| --------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------ |
| `b1_owner_revocation_after_precheck_blocks_commit`              | SQL 已准备、尚未取 gate；分别 bind/invalidate_client/release/reset，不写新 hint；epoch 不变也须 Fenced/回滚/durable 不进 |
| `b1_commit_before_revocation_is_ordered`                        | worker 持 gate 停在真实 COMMIT 前；revoke 不能先完成或发布新有效 lease；提交退出后再撤销                                 |
| `b1_queued_db_write_does_not_hold_commit_gate`                  | worker 等 DB 锁时 revoke 能完成；取得 DB 后旧任务仍被拒                                                                  |
| `b1_mirror_scope_change_blocks_commit`                          | 参数化 snapshot/clear/stale、同 build_generation 恢复；旧 permit 不复活                                                  |
| `b1_old_scope_cleanup_does_not_revoke_new_scope`                | 新 scope 安装后到达旧清理，不能影响新 scope/镜像                                                                         |
| `b1_revoked_scope_ack_cannot_clear_new_dirty`                   | 旧事务成功后扣住 report，换代并观察新事实，再放 report；不清新 dirty、不恢复旧权限                                       |
| `b1_commit_error_releases_authority_gate`                       | COMMIT 实际失败时无成功 report/durable；事务回滚，随后 revoke 不死锁                                                     |
| `b1_revocation_during_slow_commit_does_not_stall_other_threads` | B1.3 专项：阻塞持 gate 的 COMMIT，发送失效帧及其他线程超过 256 条广播；其余线程仍消费且无 lag                            |

通道/屏障用于安排先后与确认 COMMIT 是否进入；超时仅作挂死保护。旧“捕获后、写前复核前换代”用例不能代替上述窗口。

“提交先胜”的撤销须在独立阻塞线程/可独立推进的任务中启动；测试控制线程确认其正在等待后，再释放 COMMIT。不能让测试控制线程同步调用会阻塞的 bind，随后又要求同一线程发信号释放 COMMIT，从而制造测试自身死锁。COMMIT 失败用例必须命中实际提交阶段的错误，例如受控 deferred constraint，不能以 SQL 准备阶段报错冒充。

## 8. 本轮验证边界

规划会话用标准库锁、受控线程、内存 SQLite 得到：

```text
UNGATED: revoke completed before commit, stale answered still persisted
REVOKE_FIRST: SQL prepared; final gate rejects; DB remains pending
COMMIT_FIRST: order=commit,revoke; late ACK cannot reactivate revoked scope
DB_QUEUE: revoke completes while worker waits for DB; stale write rolls back
SCOPE_ROTATION: same owner plus new scope rejects the old permit
```

规划阶段的模型验证不代表 Rust 产品实现已修复。后续 B1.1 的生产路径测试和独立高级复审结果见[验收记录](../acceptance/2026-11-05-codex-b1-1-acceptance.md)；B1.3 异步 transition 尚未实施。
