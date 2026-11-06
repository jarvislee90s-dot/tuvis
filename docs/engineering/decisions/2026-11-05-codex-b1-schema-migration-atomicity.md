# Codex B1 新表与版本列的原子迁移裁决

- 日期：2026-11-05。
- 范围：B1.1 的 T2；[设计 v2.1](../specs/2026-11-05-codex-异步答题持久事实可靠性设计.md)与[实施计划 v2.1](../plans/2026-11-05-codex-异步答题持久事实可靠性实施计划.md)。
- 性质：用户要求规划会话处理实施冲突后的局部方案裁决；不改变项目宪法，不批准新功能范围。

## 1. 冲突与复现

生产先由 `connection::DB` 调用 `schema::init`，随后 `database::init` 在另一连接运行 `migration::migrate`。测试 `DeviceStore::memory` 也按 schema→migrate 执行。

v2 同时要求把 writer 表加入基础 schema，并要求它与迁移中的两列一起回滚。若旧 hints 表已存在，schema 的 `CREATE TABLE IF NOT EXISTS` 不会补其列，却会提前提交新 writer 表；后续迁移失败无法回滚这个先前已提交的表。

本轮在内存 SQLite 中加载当前真实 schema SQL，复现了“writer 表预建后迁移失败，表残留但两列回滚”。这是 v2 规划步骤的事务边界冲突，不应通过放松验收或让执行者改设计掩盖。

## 2. 定案：B1 增量结构归迁移独占

1. `schema::init` 继续创建基础旧六列 `codex_interaction_hints`，不提前创建 `codex_hint_writer_state`、其初始行或两列。
2. `migration::migrate` 调用独立 helper `migrate_b1_hint_schema`，在一个局部事务中：检查当前形状→创建 writer 表→初始化单行→补两列→一次 commit。
3. 新库沿用相同 schema→migrate 顺序；最终数据库结构一样。无需重排全库启动，无需将其他迁移合并进事务。
4. 迁移失败恢复本事务前态。之前不存在的新对象应消失；之前已经存在的表/列/epoch 原样保留。不得为清残留执行补偿 DROP、清空表或把 epoch 重置为 0。
5. helper 用 `CREATE IF NOT EXISTS`、`INSERT OR IGNORE` 和逐列形状检查支持重入；检查与变更同事务进行。基础 hints 表缺失时跳过，不产生孤立 writer 表。
6. `begin_epoch` 必须验证完整 B1 结构和合法单行状态，再调用事务分配器。结构不全只读降级/返回不可用，不运行无版本写入，不把建表职责挪进 DAO。
7. B1 DAO 测试使用迁移后的连接；仅基础 schema 的连接不代表 B1 已 ready。其他功能的 schema-only 测试无需全仓改造。

“原子”限定为本次 B1 事务的增量，而不是回滚已经提交的基础 schema 或清理事务前遗留对象。

## 3. 执行要求与验收

| 初始状态                          | 注入失败后的要求                 | 成功后的要求                       |
| --------------------------------- | -------------------------------- | ---------------------------------- |
| 空库→基础 schema，B1 对象均无     | 新表/初始行/两列均无；基础表仍在 | 表、单行、两列完整                 |
| 旧六列 hints，含 legacy answered  | 旧行及结构不变；不留新 B1 对象   | 旧行保留，版本 NULL                |
| writer 表已存在，epoch=7          | 保留表与 7；回滚本次新增列       | 结构补齐，仍为 7；begin 才增至 8   |
| 一个版本列已存在                  | 保留旧列，回滚本次新对象         | 只补缺失结构，不删改旧数据         |
| 基础 hints 表不存在的局部迁移夹具 | 不创建 writer                    | 不宣称 B1 ready；writer 分配被拒绝 |

故障注入至少三处：writer 表/行创建后、首个缺失列添加后、commit 前。测试对比前后 sqlite_master、PRAGMA table_info、writer 行与 legacy 行，不只检查“最后一个列不存在”。幂等成功测试再运行迁移一次。

规划会话的内存 SQL 验证已通过以下反例与修订方向检查：

```text
OLD_PLAN failure: writer remains, both columns absent
RULING no_preexisting_writer: failure restores exact prior state; repeated success keeps epoch=0, legacy unchanged
RULING preexisting_writer_epoch_7: failure restores exact prior state; repeated success keeps epoch=7, legacy unchanged
RULING partial_epoch_column: failure restores exact prior state; repeated success keeps epoch=0, legacy unchanged
```

这些是事务方案验证，不是产品迁移已经实现的证据。执行者仍需让 T2 的真实 Rust 迁移测试先失败、修后通过，并覆盖生产初始化顺序。继续当前 B1.1 任务，不扩展到 B1.2/B1.3。
