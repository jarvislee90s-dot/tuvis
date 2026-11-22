//! hints 写入调度。A3.1/A3.3（T9）：调度器、退避阶梯与失败分类进入生产——
//! 单 worker 消费观察版本化任务，提交离开发布线程（adapter 装配）。
use super::types::ThreadKey;

/// A3.3：暂态失败退避阶梯毫秒数（1/4/16s，封顶 60s）。
pub(crate) fn backoff_delay_ms(attempts: u32) -> u64 {
    match attempts {
        0 => 1_000,
        1 => 4_000,
        2 => 16_000,
        _ => 60_000,
    }
}

/// A3.1：真实写失败分类（监督接口的生产结果面）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteFailureClass {
    /// 暂态（存储忙/版本冲突/任务中断）——按退避阶梯重试。
    Temporary,
    /// 终态（数据非法/存储缺失）——保持 dirty，条件改变（新观察）后恢复。
    Terminal,
    /// 被栅栏（owner 撤销/换代）——保持 dirty，新代观察后恢复。
    Fenced,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Enqueue {
    Accepted,
    Coalesced,
    DeferredStale,
    Closed,
}
#[derive(Debug, PartialEq, Eq)]
#[cfg(test)]
enum CommitOutcome {
    Applied { more: bool },
    Retry,
}
pub(crate) struct Scheduler {
    max_threads: usize,
    queue: std::collections::VecDeque<ThreadKey>,
    in_flight: Option<ThreadKey>,
    closed: bool,
    changed_in_flight: bool,
    progressed: std::collections::HashSet<ThreadKey>,
    rescan: std::collections::HashSet<ThreadKey>,
    retry: std::collections::HashMap<ThreadKey, (u32, u64)>,
}
impl Scheduler {
    pub(crate) fn new(max_threads: usize) -> Self {
        Self {
            max_threads,
            queue: Default::default(),
            in_flight: None,
            closed: false,
            changed_in_flight: false,
            progressed: Default::default(),
            rescan: Default::default(),
            retry: Default::default(),
        }
    }
    pub(crate) fn enqueue(&mut self, key: ThreadKey) -> Enqueue {
        if self.closed {
            return Enqueue::Closed;
        }
        if self.queue.contains(&key) {
            if self.progressed.contains(&key) {
                self.rescan.insert(key);
            }
            return Enqueue::Coalesced;
        }
        if self.in_flight.as_ref() == Some(&key) {
            self.changed_in_flight = true;
            return Enqueue::Coalesced;
        }
        if self.queue.len() + usize::from(self.in_flight.is_some()) >= self.max_threads {
            return Enqueue::DeferredStale;
        }
        self.queue.push_back(key);
        Enqueue::Accepted
    }
    pub(crate) fn next(&mut self) -> Option<ThreadKey> {
        if self.in_flight.is_some() {
            return None;
        }
        let next = self.queue.pop_front()?;
        self.in_flight = Some(next.clone());
        Some(next)
    }
    /// A3.1：真实失败登记（退避阶梯 + 到期时刻），供 worker 的异步循环查询。
    pub(crate) fn note_failure(&mut self, key: &ThreadKey, now: u64) -> u64 {
        let (attempts, _) = self.retry.get(key).copied().unwrap_or((0, 0));
        let due = now.saturating_add(backoff_delay_ms(attempts));
        self.retry
            .insert(key.clone(), (attempts.saturating_add(1), due));
        due
    }
    /// A3.1：线程当前退避到期时刻（无退避记录 → None）。生产 worker 用它判定
    /// attempt 与定时唤醒（终审 Minor 2）。
    pub(crate) fn retry_due(&self, key: &ThreadKey) -> Option<u64> {
        self.retry.get(key).map(|(_, due)| *due)
    }
    /// A3.1：成功/条件恢复后清除退避记录。
    pub(crate) fn clear_retry(&mut self, key: &ThreadKey) {
        self.retry.remove(key);
    }
    #[cfg(test)]
    fn run_once<F: FnMut(&ThreadKey) -> CommitOutcome>(&mut self, now: u64, mut transaction: F) {
        for _ in 0..self.queue.len() {
            let Some(key) = self.next() else {
                return;
            };
            let (attempts, due) = self.retry.get(&key).copied().unwrap_or((0, 0));
            if now < due {
                self.complete(&key, true);
                continue;
            }
            if let CommitOutcome::Applied { more } = transaction(&key) {
                self.retry.remove(&key);
                self.complete(&key, more);
            } else {
                let delay = match attempts {
                    0 => 1_000,
                    1 => 4_000,
                    2 => 16_000,
                    _ => 60_000,
                };
                self.retry.insert(
                    key.clone(),
                    (attempts.saturating_add(1), now.saturating_add(delay)),
                );
                self.complete(&key, true);
            }
            return;
        }
    }
    pub(crate) fn complete(&mut self, key: &ThreadKey, still_dirty: bool) {
        if self.in_flight.as_ref() != Some(key) {
            return;
        }
        let key = self.in_flight.take().unwrap();
        if self.changed_in_flight {
            self.rescan.insert(key.clone());
        }
        if still_dirty {
            self.progressed.insert(key.clone());
            self.queue.push_back(key);
        } else {
            self.progressed.remove(&key);
            if self.rescan.remove(&key) {
                self.queue.push_back(key);
            }
        }
        self.changed_in_flight = false;
    }
}
/// A4/T10：阻塞任务的终值（普通事务 = Unit；带结果事务 = 生产监督面的真实返回）。
/// A6 终审归属：监督槽与其等待面是**测试侧合同装置**（监督语义的生产实现
/// 走 adapter 单写 worker 的句柄跟踪 + 失败分类 + log 监督面；见 a4-design）。
#[cfg(test)]
#[derive(Debug)]
enum BlockingTaskOutcome {
    Unit,
    Result(Result<(), String>),
}

/// A4/T10：阻塞任务**进程级监督槽**——drop 监督者不释放句柄；同一运行的两个
/// 监督者经同一槽仲裁「旧阻塞事务在飞时替代 start 必须被拒」（连接占用守卫；
/// `b1_dropped_supervisor_...` / `b1_replacement_writer_waits_...` 合同）。
#[cfg(test)]
fn blocking_task_slot(
) -> &'static std::sync::Mutex<Option<tokio::task::JoinHandle<BlockingTaskOutcome>>> {
    static SLOT: std::sync::OnceLock<
        std::sync::Mutex<Option<tokio::task::JoinHandle<BlockingTaskOutcome>>>,
    > = std::sync::OnceLock::new();
    SLOT.get_or_init(|| std::sync::Mutex::new(None))
}

#[cfg(test)]
struct BlockingSupervisor;

#[cfg(test)]
impl BlockingSupervisor {
    fn new() -> Self {
        Self
    }
    fn start<F: FnOnce() + Send + 'static>(&mut self, transaction: F) -> bool {
        let mut slot = blocking_task_slot()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if slot.as_ref().is_some_and(|handle| !handle.is_finished()) {
            return false;
        }
        *slot = Some(tokio::task::spawn_blocking(move || {
            transaction();
            BlockingTaskOutcome::Unit
        }));
        true
    }
    fn start_result<F: FnOnce() -> Result<(), String> + Send + 'static>(
        &mut self,
        transaction: F,
    ) -> bool {
        let mut slot = blocking_task_slot()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if slot.as_ref().is_some_and(|handle| !handle.is_finished()) {
            return false;
        }
        *slot = Some(tokio::task::spawn_blocking(move || {
            BlockingTaskOutcome::Result(transaction())
        }));
        true
    }
    /// A6 终审（clippy 严格门 + 正确性）：等待循环**不持槽锁跨 await**、
    /// **不依赖时钟**——短锁检查 `is_finished`，未完成则 `yield_now` 让位；
    /// 完成后**短锁取出句柄**、锁外 `.await`（此时已就绪，立即返回）。
    /// 返回 `None` = 槽本就为空；`Some(handle)` = 已取出（槽位已清）。
    /// 被取消的等待者停在 yield 点、什么都不取——所有权合同
    ///（`b1_canceled_finish_keeps_blocking_handle`）保持。
    async fn wait_slot_finished(&self) -> Option<tokio::task::JoinHandle<BlockingTaskOutcome>> {
        loop {
            {
                let mut slot = blocking_task_slot()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                match slot.as_mut() {
                    None => return None,
                    Some(handle) if handle.is_finished() => return slot.take(),
                    Some(_) => {}
                }
            }
            tokio::task::yield_now().await;
        }
    }
    async fn stop(&mut self, wait: std::time::Duration) -> bool {
        // 超时上界经 tokio 时钟（start_paused 测试下 advance 可驱动）；
        // Err = 超时不取句柄——任务仍被监督槽持有（所有权合同）。
        match tokio::time::timeout(
            wait.min(std::time::Duration::from_secs(3)),
            self.wait_slot_finished(),
        )
        .await
        {
            Ok(handle) => {
                if let Some(handle) = handle {
                    let _ = handle.await;
                }
                true
            }
            Err(_) => false,
        }
    }
    async fn finish(&mut self) {
        // A6 终审：句柄在**完成**后才出槽（见 wait_slot_finished）。
        if let Some(handle) = self.wait_slot_finished().await {
            let _ = handle.await;
        }
    }
    /// A4/T10：生产监督结果面——真实事务失败到达调用方（不再被测试缝吞掉）。
    async fn finish_result(&mut self) -> Option<Result<(), String>> {
        let outcome = self.wait_slot_finished().await?.await;
        match outcome {
            Ok(BlockingTaskOutcome::Result(result)) => Some(result),
            Ok(BlockingTaskOutcome::Unit) => Some(Ok(())),
            Err(join_error) => Some(Err(format!("blocking task join failed: {join_error}"))),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::super::hint_state::HintState;
    use super::super::types::OwnerLease;
    use super::*;
    fn key(id: &str) -> ThreadKey {
        ThreadKey {
            host_id: "local".into(),
            thread_id: id.into(),
        }
    }
    #[test]
    fn b1_writer_thread_round_robin_is_fair() {
        let mut s = Scheduler::new(2);
        assert_eq!(s.enqueue(key("a")), Enqueue::Accepted);
        assert_eq!(s.enqueue(key("b")), Enqueue::Accepted);
        let a = s.next().unwrap();
        assert_eq!(a, key("a"));
        s.complete(&a, true);
        assert_eq!(s.next(), Some(key("b")));
    }
    #[test]
    fn b1_idle_thread_retries_failed_transaction_on_clock() {
        let mut s = Scheduler::new(1);
        s.enqueue(key("a"));
        let mut calls = 0;
        s.run_once(0, |_| {
            calls += 1;
            CommitOutcome::Retry
        });
        assert_eq!(calls, 1);
        s.run_once(999, |_| {
            calls += 1;
            CommitOutcome::Applied { more: false }
        });
        assert_eq!(calls, 1);
        s.run_once(1000, |_| {
            calls += 1;
            CommitOutcome::Applied { more: false }
        });
        assert_eq!(calls, 2);
    }
    #[test]
    fn b1_successful_first_batch_keeps_remaining_keys_scheduled() {
        let mut s = Scheduler::new(1);
        s.enqueue(key("a"));
        let mut remaining = 257usize;
        let mut writes = 0;
        s.run_once(0, |_| {
            remaining -= remaining.min(256);
            writes += 1;
            CommitOutcome::Applied {
                more: remaining > 0,
            }
        });
        s.run_once(0, |_| {
            remaining -= remaining.min(256);
            writes += 1;
            CommitOutcome::Applied {
                more: remaining > 0,
            }
        });
        assert_eq!(remaining, 0);
        assert_eq!(writes, 2);
    }
    #[tokio::test(start_paused = true)]
    async fn b1_supervisor_retains_blocking_transaction_after_stop_timeout() {
        use std::time::Duration;
        let mut supervisor = BlockingSupervisor::new();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let started = supervisor.start(move || {
            let _ = started_tx.send(());
            let _ = release_rx.recv();
        });
        started_rx.await.unwrap();
        let clock = tokio::spawn(async {
            tokio::task::yield_now().await;
            tokio::time::advance(Duration::from_secs(3)).await;
        });
        let drained = supervisor.stop(Duration::from_secs(3)).await;
        clock.await.unwrap();
        let premature_replacement = supervisor.start(|| {});
        release_tx.send(()).unwrap();
        supervisor.finish().await;
        let replacement = supervisor.start(|| {});
        supervisor.finish().await;
        assert!(started);
        assert!(!drained);
        assert!(
            !premature_replacement,
            "replacement must wait for the old blocking task"
        );
        assert!(replacement);
    }
    #[tokio::test]
    async fn b1_canceled_finish_keeps_blocking_handle() {
        let mut supervisor = BlockingSupervisor::new();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        supervisor.start(move || {
            let _ = started_tx.send(());
            let _ = release_rx.recv();
            let _ = finished_tx.send(());
        });
        started_rx.await.unwrap();
        {
            let finish = supervisor.finish();
            tokio::pin!(finish);
            tokio::select! { biased; _ = &mut finish => {}, _ = tokio::task::yield_now() => {} }
        }
        let replacement = supervisor.start(|| {});
        release_tx.send(()).unwrap();
        finished_rx.await.unwrap();
        supervisor.finish().await;
        assert!(
            !replacement,
            "canceling the waiter must retain the blocking handle"
        );
    }
    #[tokio::test]
    async fn b1_dropped_supervisor_cannot_allow_live_connection_replacement() {
        use std::sync::{Arc, Mutex};
        let conn = Arc::new(Mutex::new(rusqlite::Connection::open_in_memory().unwrap()));
        conn.lock()
            .unwrap()
            .execute_batch("CREATE TABLE t(v INTEGER)")
            .unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        {
            let mut old = BlockingSupervisor::new();
            let db = conn.clone();
            old.start(move || {
                let c = db.lock().unwrap();
                c.execute_batch("BEGIN IMMEDIATE; INSERT INTO t VALUES(1)")
                    .unwrap();
                let _ = started_tx.send(());
                let _ = release_rx.recv();
                c.execute_batch("COMMIT").unwrap();
                let _ = done_tx.send(());
            });
            started_rx.await.unwrap();
        }
        let mut replacement = BlockingSupervisor::new();
        let db2 = conn.clone();
        let accepted = replacement.start(move || {
            db2.lock()
                .unwrap()
                .execute_batch("INSERT INTO t VALUES(2)")
                .unwrap();
        });
        release_tx.send(()).unwrap();
        done_rx.await.unwrap();
        replacement.finish().await;
        assert!(!accepted, "a second supervisor must not acquire the live connection before the old transaction finishes");
    }
    #[tokio::test]
    async fn b1_supervisor_reports_real_sqlite_transaction_failure() {
        let mut supervisor = BlockingSupervisor::new();
        supervisor.start_result(|| { let c = rusqlite::Connection::open_in_memory().map_err(|e| e.to_string())?; c.execute_batch("CREATE TABLE t(v INTEGER UNIQUE); INSERT INTO t VALUES(1); INSERT INTO t VALUES(1)").map_err(|e| e.to_string()) });
        let result = supervisor.finish_result().await;
        assert!(
            matches!(result, Some(Err(_))),
            "the actual transaction failure must reach the supervisor"
        );
    }
    #[test]
    fn b1_inflight_update_requires_rescan_after_remaining_pages() {
        use super::super::async_question::InteractionHint;
        use super::super::hint_state::{HintState, WriteOutcome};
        use super::super::types::OwnerLease;
        let thread = key("a");
        let lease = OwnerLease {
            key: thread.clone(),
            client_id: "owner".into(),
            epoch: 1,
            generation: 1,
        };
        let hint = |item: &str, fact: &str| InteractionHint {
            thread_id: "a".into(),
            source_turn_id: "turn".into(),
            source_item_id: item.into(),
            question_index: 0,
            last_fact: fact.into(),
            witness: None,
            evidence_state: None,
        };
        let mut state = HintState::new(1).unwrap();
        let obs = state
            .observe(
                thread.clone(),
                lease.clone(),
                1,
                vec![hint("a", "answered"), hint("b", "answered")],
            )
            .unwrap();
        let mut scheduler = Scheduler::new(1);
        assert_eq!(scheduler.enqueue(thread.clone()), Enqueue::Accepted);
        assert_eq!(scheduler.next(), Some(thread.clone()));
        let (first, cursor) = state.prepare_page(&obs, None, 1);
        let newer = state
            .observe(thread.clone(), lease, 1, vec![hint("a", "unknown")])
            .unwrap();
        assert_eq!(scheduler.enqueue(thread.clone()), Enqueue::Coalesced);
        state.apply_report(&first, &[WriteOutcome::Applied(first.hints[0].clone())]);
        scheduler.complete(&thread, true);
        assert_eq!(scheduler.next(), Some(thread.clone()));
        let (last, _) = state.prepare_page(&newer, cursor.as_ref(), 1);
        assert_eq!(last.hints[0].hint.source_item_id, "b");
        state.apply_report(&last, &[WriteOutcome::Applied(last.hints[0].clone())]);
        scheduler.complete(&thread, false);
        let rescheduled = scheduler.next();
        assert_eq!(
            rescheduled,
            Some(thread.clone()),
            "newer A must be scheduled after B completes"
        );
        let (rescan, _) = state.prepare_page(&newer, None, 1);
        assert_eq!(rescan.hints[0].hint.last_fact, "unknown");
    }

    // ==================================================================
    // A3（T9：单 worker、有界分批与退避）五个具名用例。
    // ==================================================================

    fn hint_for(thread: &str, fact: &str) -> super::super::async_question::InteractionHint {
        super::super::async_question::InteractionHint {
            thread_id: thread.into(),
            source_turn_id: "turn".into(),
            source_item_id: "a".into(),
            question_index: 0,
            last_fact: fact.into(),
            witness: None,
            evidence_state: None,
        }
    }

    fn owner_for(thread: &str, epoch: u64) -> OwnerLease {
        OwnerLease {
            key: key(thread),
            client_id: "owner".into(),
            epoch,
            generation: 1,
        }
    }

    /// T9 合同 1：单线程 257 个提示键（重复 enqueue 同键合并入同一 dirty 集）
    /// 按 256/批分页——两批恰好清空，`Applied { more }` 驱动重排，批间无新观察。
    #[test]
    fn b1_overflow_rebuild_makes_progress() {
        let mut s = Scheduler::new(1);
        for batch in 0..257 {
            assert_eq!(
                s.enqueue(key("overflow")),
                if batch == 0 {
                    Enqueue::Accepted
                } else {
                    Enqueue::Coalesced
                }
            );
        }
        let mut remaining = 257usize;
        let mut writes = 0;
        s.run_once(0, |_| {
            remaining -= remaining.min(256);
            writes += 1;
            CommitOutcome::Applied {
                more: remaining > 0,
            }
        });
        s.run_once(0, |_| {
            remaining -= remaining.min(256);
            writes += 1;
            CommitOutcome::Applied {
                more: remaining > 0,
            }
        });
        assert_eq!(remaining, 0, "两次 256/批后 257 个提示键必须全部清空");
        assert_eq!(writes, 2, "恰好两批（256+1），不许多批也不许少批");
        assert!(
            s.next().is_none(),
            "第二批 more=false 后调度器必须排空（无遗留重排）"
        );
    }

    /// T9 合同 2：分页字节预算——单批 `used` 不得超过预算且计入元数据
    /// （结构体、字符串内容与用于下一批的键副本，见 prepare_page_inner 的
    /// size 计算注释）。`used` 本身不外显，以「预算恰容纳 N 条」的可观察
    /// 页计数证明：来源条目 id 定长 1000 字节（键副本 ×2 计 2000），预算取
    /// 6 × (size_of::<VersionedHint>() + 其余元数据 + 2000)，若字符串内容或
    /// 键副本少计，页内条数必然 > 6。
    #[test]
    fn b1_pending_bytes_are_bounded() {
        use super::super::hint_state::PageStatus;
        let mut state = HintState::new(1).unwrap();
        let hints = (0..257)
            .map(|i| {
                let mut h = hint_for("t", "unknown");
                h.source_item_id = format!("{:<1000}", format!("k{i:03}-"));
                h
            })
            .collect();
        let obs = state
            .observe(key("t"), owner_for("t", 1), 1, hints)
            .unwrap();
        // 大批次 + 4 MiB 预算：limit 为主约束，预算未超（页 = 256 条、More）。
        let (page, cursor, status) =
            state.prepare_page_bounded(&obs, None, None, 256, 4 * 1024 * 1024);
        assert_eq!(page.hints.len(), 256);
        assert_eq!(status, PageStatus::More);
        assert!(cursor.is_some(), "大批次必须留有续批游标");
        // 元数据计入合同：预算恰好容纳 6 条（含键副本 ×2 的字符串元数据）。
        let per_hint = std::mem::size_of::<super::super::hint_state::VersionedHint>()
            + "t".len() * 2
            + "turn".len() * 2
            + 1000 * 2
            + "unknown".len();
        let budget = 6 * per_hint;
        let (bounded, bounded_cursor, bounded_status) =
            state.prepare_page_bounded(&obs, None, None, 256, budget);
        assert_eq!(
            bounded.hints.len(),
            6,
            "单批 used 不得超预算且必须计入字符串元数据与键副本：每条 \
             {per_hint} 字节（id 1000×2 + 结构体 + 其余元数据），6 条后预算耗尽；\
             若元数据少计，页内条数会 > 6"
        );
        assert_eq!(bounded_status, PageStatus::More, "其余 251 条仍在 dirty 集");
        assert!(bounded_cursor.is_some(), "字节预算分页必须推进游标");
        // 续批同样受预算约束（游标分页逐批推进，不一次性倾泻）。
        let (next_page, _, next_status) =
            state.prepare_page_bounded(&obs, None, bounded_cursor.as_ref(), 256, budget);
        assert_eq!(next_page.hints.len(), 6);
        assert_eq!(next_status, PageStatus::More);
    }

    /// T9 合同 3：退避时钟 1/4/16/60s 完整阶梯——无新观察时按闸门重试且推进；
    /// 每次 due 前被闸门挡住（事务不调用），第四次失败起封顶 60s；时钟到达后
    /// 同一线程无需新观察即恢复（Applied 排空）。
    #[test]
    fn b1_idle_thread_retries_without_new_observation() {
        let mut s = Scheduler::new(1);
        s.enqueue(key("backoff"));
        let mut calls = 0;
        let mut fail = |s: &mut Scheduler, now: u64, expect_call: bool, label: &str| {
            let before = calls;
            s.run_once(now, |_| {
                calls += 1;
                CommitOutcome::Retry
            });
            assert_eq!(
                calls - before,
                usize::from(expect_call),
                "{label}: now={now} 事务调用次数不符"
            );
        };
        fail(&mut s, 0, true, "首次失败");
        fail(&mut s, 999, false, "1s 闸门前被挡");
        fail(&mut s, 1000, true, "1s 闸门重试（第 2 次失败 → 4s）");
        fail(&mut s, 4999, false, "4s 闸门前被挡");
        fail(&mut s, 5000, true, "4s 闸门重试（第 3 次失败 → 16s）");
        fail(&mut s, 20999, false, "16s 闸门前被挡");
        fail(
            &mut s,
            21000,
            true,
            "16s 闸门重试（第 4 次失败 → 封顶 60s）",
        );
        fail(&mut s, 80999, false, "60s 封顶闸门前被挡");
        // 无新观察：时钟到达后同一线程恢复（Applied { more: false } 排空）。
        s.run_once(81000, |_| {
            calls += 1;
            CommitOutcome::Applied { more: false }
        });
        assert_eq!(calls, 5, "4 次失败 + 1 次恢复，闸门恰好按 1/4/16/60s 生效");
        assert!(s.next().is_none(), "恢复后无新观察不得再排程（dirty 已清）");
    }

    /// T9 合同 4：双线程多批次公平——两线程各带多批 dirty 时严格轮转，
    /// 第二线程在第一线程完结前持续取得进展；各自完结后排空。
    #[test]
    fn b1_thread_round_robin_is_fair() {
        let mut s = Scheduler::new(2);
        assert_eq!(s.enqueue(key("a")), Enqueue::Accepted);
        assert_eq!(s.enqueue(key("b")), Enqueue::Accepted);
        let mut remaining_a = 3usize;
        let mut remaining_b = 2usize;
        let mut order: Vec<String> = Vec::new();
        {
            let mut run_batch = |scheduler: &mut Scheduler| {
                let mut served = String::new();
                scheduler.run_once(0, |key| {
                    served = key.thread_id.clone();
                    let more = if served == "a" {
                        remaining_a -= 1;
                        remaining_a > 0
                    } else {
                        remaining_b -= 1;
                        remaining_b > 0
                    };
                    order.push(served.clone());
                    CommitOutcome::Applied { more }
                });
            };
            for _ in 0..5 {
                run_batch(&mut s);
            }
        }
        assert_eq!(
            order,
            ["a", "b", "a", "b", "a"],
            "双线程必须严格轮转：第二线程在第一线程完结前取得进展"
        );
        assert!(s.next().is_none(), "两线程 dirty 清空后调度器排空");
    }

    /// T9 合同 5：满队列下最新否定不丢——已排队键的新失效观察走 Coalesced
    /// （容量闸门只拦新键），其后续批次按最新观察写出 unknown；新键在队列
    /// 满载时 DeferredStale（调用方保留 dirty、变化点重试），腾出槽位后被
    /// 接受并写出最新事实。
    #[test]
    fn b1_latest_invalidation_survives_full_queue() {
        use super::super::hint_state::PageStatus;
        let thread_a = key("full-a");
        let thread_b = key("full-b");
        let thread_c = key("full-c");
        let mut state = HintState::new(1).unwrap();
        let _obs_a = state
            .observe(
                thread_a.clone(),
                owner_for("full-a", 1),
                1,
                vec![hint_for("full-a", "answered"), {
                    let mut h = hint_for("full-a", "answered");
                    h.source_item_id = "b".into();
                    h
                }],
            )
            .unwrap();
        let obs_b = state
            .observe(
                thread_b.clone(),
                owner_for("full-b", 1),
                1,
                vec![hint_for("full-b", "answered")],
            )
            .unwrap();
        let mut scheduler = Scheduler::new(2);
        assert_eq!(scheduler.enqueue(thread_a.clone()), Enqueue::Accepted);
        assert_eq!(scheduler.enqueue(thread_b.clone()), Enqueue::Accepted);
        // 队列满载：已排队键的新失效观察 → Coalesced（最新否定进入状态）。
        let obs_a_latest = state
            .observe(
                thread_a.clone(),
                owner_for("full-a", 1),
                1,
                vec![{
                    let mut h = hint_for("full-a", "unknown");
                    h.source_item_id = "a".into();
                    h
                }],
            )
            .unwrap();
        assert_eq!(
            scheduler.enqueue(thread_a.clone()),
            Enqueue::Coalesced,
            "已排队键不受容量闸门影响（最新否定不得被 DeferredStale 丢弃）"
        );
        // 新键在满队列下 DeferredStale；其最新否定由调用方保留（变化点重试模型）。
        let obs_c = state
            .observe(
                thread_c.clone(),
                owner_for("full-c", 1),
                1,
                vec![hint_for("full-c", "unknown")],
            )
            .unwrap();
        assert_eq!(scheduler.enqueue(thread_c.clone()), Enqueue::DeferredStale);
        assert_eq!(scheduler.enqueue(thread_c.clone()), Enqueue::DeferredStale);
        // 批次 1：t1 按最新观察写出（unknown 覆盖 answered，b 随批携带）。
        scheduler.next();
        let (page_a, _, status_a) =
            state.prepare_page_bounded(&obs_a_latest, None, None, 256, 4096);
        assert_eq!(status_a, PageStatus::Complete);
        assert_eq!(
            page_a.hints[0].hint.last_fact, "unknown",
            "Coalesced 后的批次必须携带最新否定"
        );
        assert_eq!(page_a.hints[0].hint.source_item_id, "a");
        scheduler.complete(&thread_a, false);
        // 批次 2：t2 照常写出。
        scheduler.next();
        let (page_b, _, _) = state.prepare_page_bounded(&obs_b, None, None, 256, 4096);
        assert_eq!(page_b.hints[0].hint.last_fact, "answered");
        scheduler.complete(&thread_b, false);
        // 槽位腾出：t3 被接受并写出最新否定。
        assert_eq!(
            scheduler.enqueue(thread_c.clone()),
            Enqueue::Accepted,
            "腾出槽位后 DeferredStale 的线程必须被接受（不丢）"
        );
        scheduler.next();
        let (page_c, _, _) = state.prepare_page_bounded(&obs_c, None, None, 256, 4096);
        assert_eq!(page_c.hints[0].hint.last_fact, "unknown");
        scheduler.complete(&thread_c, false);
        assert!(scheduler.next().is_none());
    }

    // ==================================================================
    // A4（T10：停止监督与活连接串行交接）。
    // ==================================================================

    /// T10 合同：stop 3s 上界超时后，在飞 blocking 任务句柄仍被监督者持有
    ///（超时不换所有权、不得声称 blocking 已终止）；任务自然完成后才可替换，
    /// 且副作用完整发生（不被 abort/回滚）。
    #[tokio::test(start_paused = true)]
    async fn b1_writer_stop_timeout_preserves_task_ownership() {
        use std::time::Duration;
        let mut supervisor = BlockingSupervisor::new();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        assert!(supervisor.start(move || {
            let _ = started_tx.send(());
            let _ = release_rx.recv();
            let _ = done_tx.send(());
        }));
        started_rx.await.unwrap();
        let clock = tokio::spawn(async {
            tokio::task::yield_now().await;
            tokio::time::advance(Duration::from_secs(3)).await;
        });
        let drained = supervisor.stop(Duration::from_secs(3)).await;
        clock.await.unwrap();
        assert!(!drained, "3s 上界超时不得声称 blocking 已终止");
        assert!(
            !supervisor.start(|| {}),
            "超时后 start 必须拒绝替代（句柄所有权仍在监督者）"
        );
        // 二次 stop 超时：同一句柄仍被持有（不以 timeout 换所有权）。
        let clock2 = tokio::spawn(async {
            tokio::task::yield_now().await;
            tokio::time::advance(Duration::from_secs(3)).await;
        });
        let drained_second = supervisor.stop(Duration::from_secs(3)).await;
        clock2.await.unwrap();
        assert!(!drained_second, "二次超时同样不得声称终止");
        assert!(!supervisor.start(|| {}));
        // finish 等待原始任务完成后才可替换；任务副作用完整发生。
        release_tx.send(()).unwrap();
        supervisor.finish().await;
        assert!(
            done_rx.await.is_ok(),
            "超时后的任务必须完整跑完（不得被 abort）"
        );
        assert!(supervisor.start(|| {}), "finish 后才可替换");
    }

    /// T10 合同：旧阻塞事务在飞（持有真实 SQLite 连接的 BEGIN IMMEDIATE）时，
    /// 替代 writer 的启动必须被拒（连接占用守卫）；旧事务完成后才可接管。
    /// 与 `b1_dropped_supervisor_cannot_allow_live_connection_replacement` 同根，
    /// 本用例独立钉「替换方等待」语义（旧监督者保持存活、不 drop）。
    #[tokio::test]
    async fn b1_replacement_writer_waits_for_old_transaction() {
        use std::sync::{Arc, Mutex};
        let conn = Arc::new(Mutex::new(rusqlite::Connection::open_in_memory().unwrap()));
        conn.lock()
            .unwrap()
            .execute_batch("CREATE TABLE t(v INTEGER)")
            .unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let mut old = BlockingSupervisor::new();
        let db = conn.clone();
        old.start(move || {
            let c = db.lock().unwrap();
            c.execute_batch("BEGIN IMMEDIATE; INSERT INTO t VALUES(1)")
                .unwrap();
            let _ = started_tx.send(());
            let _ = release_rx.recv();
            c.execute_batch("COMMIT").unwrap();
        });
        started_rx.await.unwrap();
        // 替代 writer（新监督者、同连接）：旧事务在飞时必须等待/被拒。
        let mut replacement = BlockingSupervisor::new();
        let db2 = conn.clone();
        let accepted = replacement.start(move || {
            db2.lock()
                .unwrap()
                .execute_batch("INSERT INTO t VALUES(2)")
                .unwrap();
        });
        assert!(
            !accepted,
            "旧事务在飞时替代 writer 必须等待/被拒（连接占用守卫）"
        );
        release_tx.send(()).unwrap();
        old.finish().await;
        // 旧事务完成后才可接管。
        let db3 = conn.clone();
        assert!(
            replacement.start(move || {
                db3.lock()
                    .unwrap()
                    .execute_batch("INSERT INTO t VALUES(3)")
                    .unwrap();
            }),
            "旧事务完成后替代 writer 才可接管"
        );
        replacement.finish().await;
        let rows: i64 = conn
            .lock()
            .unwrap()
            .query_row("SELECT count(*) FROM t", [], |row| row.get(0))
            .unwrap();
        // A4/T10：tx2 的启动被连接占用守卫拒绝（本用例钉住的合同）——被拒写入
        // 从未落库；行数 = 旧事务(1) + 接管后(3)。
        assert_eq!(
            rows, 2,
            "接替后写入按序落库：被拒的替代写入从未执行（合同）"
        );
    }
}
