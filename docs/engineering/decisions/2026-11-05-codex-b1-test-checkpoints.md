# Codex B1.1 生产存储窗口的测试观测裁决

- 日期：2026-11-05。
- 范围：B1.1 最后两项生产入口证据；不改变 [owner 提交栅栏协议](2026-11-05-codex-b1-owner-commit-fence.md)，不提前进入 B1.2/B1.3。
- 状态：规划会话定案；原 reviewer_senior 独立复核认为该狭窄测试例外可接受，未降低原 owner/COMMIT 验收条件。

## 1. 裁决时的完成程度

当前生产 Store 已使用原 permit，先准备全部 SQL，再取 gate，并持 guard 执行真实 COMMIT。现有测试证明提交先胜、事务/authority 组合的撤销先胜和一部分 DB 竞争行为。

仍未闭合：

1. 真实 `StoreInteractionHints::write_batch` 已经从 `prepare_versioned_hints_conn` 返回、尚未进入 `permit.lock()`，此时完成四种 Registry 撤销；必须直接证明该生产入口回滚。
2. 真实 worker 已在同一 DB mutex 获取路径上发生竞争。原 start 信号和测试主线程自己的 try_lock 只能证明启动/锁被持有，不能证明 worker 已尝试获取。

裁决时 B1.1 保持“实现和已列自动检查已通过，验收待补证”。随后两项窗口已补证并完成 B1.1 验收，详见[验收记录](../acceptance/2026-11-05-codex-b1-1-acceptance.md)；总体 B1 仍未完成。

## 2. 对原锁纪律的狭窄测试例外

只允许：**仅 `#[cfg(test)]`、实例级固定 Probe，在指定检查点执行纯原子观测与控制。** 不允许把它扩展为可注入闭包、消息发送、业务通知、其他应用锁、权限修改或提交旁路。非测试构建完全移除，不用 `debug_assertions` 或可运行时打开的生产 feature 代替。

区分三种情况：

| 行为                                                                                             | 裁决                                        |
| ------------------------------------------------------------------------------------------------ | ------------------------------------------- |
| 既有 DeviceStore 闭包封装本次受控 DB 操作                                                        | 属于存储实现本体，不等于可注入外部业务回调  |
| 固定测试点写入/读取 Probe 原子字段，必要时 yield_now，等待测试主控释放                           | 仅测试构建允许；不重入业务、SQLite 或应用锁 |
| 持 DB/gate 调用户提供的 Fn、发送 channel/watch/oneshot、取其他业务 Mutex、改权限后假装原实现通过 | 禁止；测试模式也不能借此绕开生产锁协议      |

这是测试设施的有限例外，不是修改生产的锁顺序。测试控制线程可以使用通道回收线程，但被测持锁路径的 Probe 自身只能操作固定原子状态。

## 3. Probe 与安全退出

在 `services/codex_desktop_ipc/hint_test_probe.rs` 新建 cfg(test) 模块；Store 仅测试构建持有可选的实例级 Arc。每个测试/Store 使用独立 Probe，不用进程全局或会串并行用例的动态钩子。

固定检查点：`AfterSqlPreparedBeforePermit`、`DbContended`。Probe 至少记录 reached 和 command（wait/resume/abort）；Reached 用 Release 写入，主控用 Acquire 观察；主控释放用 Release，worker 用 Acquire 继续。

示意内核如下，使用实际 Rust 类型即可；它不执行传入代码，也不拿应用锁：

```rust
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TestPoint { AfterSqlPreparedBeforePermit, DbContended }

pub struct TestProbe {
    point: TestPoint,
    reached: AtomicBool,
    command: AtomicU8,
    timeout: Duration,
}

impl TestProbe {
    pub fn new(point: TestPoint) -> Self {
        Self { point, reached: AtomicBool::new(false),
            command: AtomicU8::new(0), timeout: Duration::from_secs(10) }
    }
    pub fn hit(&self, point: TestPoint) -> Result<(), &'static str> {
        if point != self.point { return Ok(()); }
        self.reached.store(true, Ordering::Release);
        let deadline = Instant::now() + self.timeout;
        loop {
            match self.command.load(Ordering::Acquire) {
                1 => return Ok(()),
                2 => return Err("test_probe_aborted"),
                _ => {}
            }
            if Instant::now() >= deadline {
                self.abort();
                return Err("test_probe_timeout");
            }
            std::thread::yield_now();
        }
    }
    pub fn reached(&self) -> bool { self.reached.load(Ordering::Acquire) }
    pub fn resume(&self) -> bool {
        self.command.compare_exchange(0, 1, Ordering::Release, Ordering::Acquire).is_ok()
    }
    pub fn abort(&self) { self.command.store(2, Ordering::Release); }
}
```

实例接口保持 crate 内测试可见；不为普通集成测试增加公开生产 API。示例使用 10 秒挂死保护，可在测试中统一配置；时限不是时序证据，也不是随机 sleep。

必须有 RAII 清理：控制线程失败或超时后原子 abort 所有未释放 Probe，释放 DB holder，再 join worker/撤销线程；先释放全部资源再等待线程。worker 在检查点超时/abort 必须退出，SQL 已准备时返回测试失败/存储错误并让事务回滚，不能绕过 permit 继续提交。

数据库竞争检查点发生在 worker 尚未取得 DB/gate 的阶段；测试超时可以使该用例明确失败，但不能改变 Global 毒锁恢复和 Owned 原有错误语义。

## 4. 生产 Store 撤销先胜的精确测试

仅在真实 Store 方法中下列顺序插入固定测试点，不能另写一份复制的 write_batch：

```text
真实 Store.write_batch
  → DeviceStore 公共获取路径取得连接
  → prepare_versioned_hints_conn 成功返回（SQL 全部完成）
  → #[cfg(test)] AfterSqlPreparedBeforePermit
  → 原 permit.lock
  → 原 batch.commit
```

对 bind/invalidate_client/release/reset 分别测试：

1. 真实 Registry 产生 lease/authority/permit，真实 Store 装配 DeviceStore::Owned 的内存/临时 SQLite；记录 writer epoch 和原行。
2. worker 只调用真实 Store.write_batch；主控等待该实例 Probe.Reached，确认 gate 尚未持有。
3. 在独立撤销线程执行真实 Registry 操作，主控有界等待其完成。主控不持 DB/gate，不写任何新 hint，不旋转 writer epoch。
4. 正常 resume worker，使其继续原 permit 校验；断言精确为 OwnerFenced，原批次全部回滚，DB 行和 writer epoch 与原值一致，不能只断言任意 Err。
5. 保留原 commit-first/真实提交失败测试，证明两个合法胜负顺序都成立。

用例建议命名 `b1_store_post_sql_registry_revocation_rolls_back`，可以在一个测试中参数化四种真实入口。Reached 必须来自 Store 内的指定位置，不能由测试 worker 在调用 write_batch 前自己设置。

Store 不持有 adapter 的 durable 状态，不能仅凭 Store 错误就声称 durable 未推进。关联现有 adapter 的错误早退/dirty 保留测试；若未覆盖 OwnerFenced，再补 `b1_adapter_owner_fenced_does_not_advance_durable`，无需把每个 Store 窗口重复搭建成完整 SSE 场景。

## 5. 数据库锁竞争的可观测标准

标准 Mutex 没有公开 API 能确认另一个线程已在操作系统内部 parked。把“即将调用 lock”或主控的 try_lock 失败写成这一结论不成立；也不为本批引入平台线程诊断或替换生产锁实现。

[Rust 标准库的 try_lock 文档](https://doc.rust-lang.org/std/sync/struct.Mutex.html#method.try_lock)明确其非阻塞语义：因为 mutex 已锁而无法取得时返回 WouldBlock。因此本项观测的是 worker 对实际资源发生的获取竞争，不是内部调度器状态。

**合并验收标准：实际 worker 在生产公共获取路径，对同一个 DB mutex 获取失败并得到 WouldBlock；该时刻尚未持提交 gate；真实 Registry 撤销可先完成；DB 释放后旧写被拒绝。** 这明确验证了“等 DB 时不占 gate”，不宣称内核排队状态。

实施要求：

- 生产和测试使用同一连接锁获取核心。允许仅 cfg(test) 的 Probe 参数及一次实际 try_lock；WouldBlock 才标 DbContended，然后继续同一 mutex 的正常获取路径。
- try_lock 成功应复用取得的 guard；不得先释放再重锁。Global 的毒锁恢复、Owned 的原有错误策略保持不变。
- 不准在 Store 外额外试一把相似锁，或更换假连接/复制锁代码当作生产路径证据。
- 用独立 DB holder 线程持有实际 mutex，并以纯原子控制释放；主控本身不持 DB 锁执行 Registry 撤销，避免测试装置制造反向锁序。
- DbContended 由 worker 的真实 WouldBlock 产生；主控确认 gate 可取并立即释放，再由独立撤销线程完成失效，最后释放 DB holder 和 worker Probe。
- 断言旧写 OwnerFenced、无落库；新增 `b1_store_db_contention_precedes_gate`。旧“start 信号后未完成”的测试保留为较弱证据或替换，不能继续标成严格排队证明。

## 6. 接续任务和验收门

1. 只修改 cfg(test) 测试设施、必要的共同锁获取 helper 和目标测试；不改 owner/COMMIT 权限协议，不加列/表、不做 B1.2/B1.3。
2. 定向跑两个新测试及原提交先胜、提交失败、过期 ACK 和 adapter durable 分支；确认四种撤销实际执行。
3. 按 T5 重跑受改动影响的套件、fmt/Clippy；普通 `cargo check --lib` 验证非测试构建，确保 Probe 未泄漏。测试-only 改动不是新生产缺陷，不人为添加绕过权限开关来制造“红测”。
4. 独立 reviewer_senior 复核检查点位置、cfg 边界、公共获取路径、超时清理与断言力度。
5. 补最终验收记录、按白名单提交；当前未跟踪的 hint_commit.rs 与 hint_commit_sqlite_tests.rs 必须包含在产品提交，不能只提交 tracked diff。
6. 上述通过才能宣布 B1.1 验收完成。随后才交接 B1.2 T6—T7；是否继续实施遵循后续授权，不自动越界。

本裁决本身提供补证方式；执行后的测试结果与限制以[验收记录](../acceptance/2026-11-05-codex-b1-1-acceptance.md)为准。
