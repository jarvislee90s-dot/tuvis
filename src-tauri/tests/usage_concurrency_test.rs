//! Task 22：**写库不阻塞 3 秒轮询**的并发验收（真并发 + 计时 + 明确阈值 + 前提断言）。
//!
//! GC 2 / 说明书 §7.2 的三条硬约束在这里被锁住：
//! ① 结构性：单事务写入行数 ≤ `LEDGER_BATCH_ROWS`（200）；
//! ② 行为性：写库期间「取同一把全局 `Mutex<Connection>`」的单次等待**上界 < 500 ms**；
//! ③ 结构性：会话轮询热路径（`adapter::get_all_sessions` 兼容入口 → scan_bus 共享快照）**不得**触发用量采集。
//!
//! ## 为什么必须真并发（本任务最容易假绿的地方）
//! 「串行跑一遍写库、再跑一遍查询，两边都返回了」在两个函数都正常时**也是绿的**——
//! 那种用例测的是「什么都没发生时查询不慢」。本文件的写法：
//! * 写库在**独立线程**里持续跑生产入口（`ledger::apply_delta`，分批短事务）；
//! * 查询在**主线程**里同时取锁计时（`database::get_tool_enabled` = 真实读路径，取同一把锁）；
//! * **先断言前提、再做上界断言**：`DB.try_lock()` 返回 `WouldBlock` ⟺ 此刻有人**正持** DB 锁
//!   （Mutex 语义），用它证明「写库侧真的持过锁、两边真的并发起来了」；再用写库侧的
//!   轮数 / 行数 / 批数证明它真的在写——落库失败时锁窗口为空，上界断言会退化成假绿。
//! * `#[ignore]` 的反例（一个大事务写 4,000 行）给出**对照量级**：它才是「不阻塞」判据的反证。
//!
//! ## W-26（锁序铁律，本文件最容易无意构造出的死锁）
//! 持 `DB` 锁时**只能**调 `collect::cached_result()` / `collect_calls()`（二者不碰飞行锁）；
//! **绝不能**调 `collect()` / `collect_with()` / `run_collection()`——在飞的扫描需要 `DB`
//! → ABBA 死锁（`std::sync::Mutex` 不可重入、无超时）。本文件确实有持 `DB` 锁的路径
//! （反例与清理测量），所以文末 `this_file_never_calls_collection_entries` 用自省锁
//! 把这条从「注释承诺」变成**机械锁**。
//!
//! ## 测试隔离（GC 19 / GC 21 + Task 17 / Task 20 的实测教训）
//! * 本文件所有用例都碰进程级全局量（全局 `DB`、env、`COLLECT_CALLS`）→ 每条用例持
//!   `serial()` 串行锁；文末 `every_test_in_this_file_takes_the_serial_lock` 是机械锁。
//! * `support::setup()` **只重定向 HOME / TUVIS_HOME**，不覆盖 `DSH_HOME` / `KIMI_CODE_HOME`；
//!   而 dsh 采集器的根是 `DSH_HOME` 优先、kimi 是 `KIMI_CODE_HOME` → 不摘就会**真的扫真实家目录**
//!   （Task 20 首跑实测：11.9 s + 脏数据；摘掉后 0.03 s）。`serial()` 里一并摘掉。

mod support;

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

use multi_agents_manager_lib::adapter;
use multi_agents_manager_lib::database;
use multi_agents_manager_lib::services::usage::collect;
use multi_agents_manager_lib::services::usage::delta::{
    DetailDelta, DetailKey, SessionCounters, SourceDelta,
};
use multi_agents_manager_lib::services::usage::ledger::{self, LEDGER_BATCH_ROWS};
use multi_agents_manager_lib::services::usage::model::{SourceKind, UsageBuckets, UsageSourceId};
use multi_agents_manager_lib::services::usage::semantics::CacheSemantics;

static SERIAL: Mutex<()> = Mutex::new(());

/// 串行锁 + 环境重定向（Task 17 `dsh.rs` / Task 20 `usage_ipc_test.rs` 先例）。
/// `MutexGuard` 必须绑到具名变量（绑 `_` 会立刻 drop，锁形同虚设——`clippy::let_underscore_lock`）。
///
/// **Minor 3（fix round 1）**：**先取锁、再改 env**。env 是进程级全局量，
/// `remove_var` 必须在临界区**内**——原版先 `remove_var` 后取锁，等于「它要保护的东西全在锁外」，
/// 与 `support::setup()` 的 `set_var` 也可能和别的用例并发。锁内顺序由
/// `every_test_in_this_file_takes_the_serial_lock` 的结构断言钉住（变异回旧顺序 → 真红）。
fn serial() -> MutexGuard<'static, ()> {
    let guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    support::setup();
    // 生产语义里这两个变量是「宿主指定的数据根」；测试里一律摘掉，让所有源只看临时 home
    std::env::remove_var("DSH_HOME");
    std::env::remove_var("KIMI_CODE_HOME");
    guard
}

/// 大批量明细夹具：`n` 条都落在同一小时 + 64 个会话行键上（与任务书给的夹具逐字同形）。
fn big_delta(n: usize) -> SourceDelta {
    let details = (0..n)
        .map(|i| DetailDelta {
            source_id: "claude".into(),
            provider_kind: SourceKind::Inferred,
            key: DetailKey {
                session_id: format!("s{}", i % 64),
                hour_key: "2026-10-03T09".into(),
                day_key: "2026-10-03".into(),
                project_key: "proj".into(),
                model: "m".into(),
                provider: "p".into(),
            },
            buckets: UsageBuckets {
                input_fresh: 1,
                ..Default::default()
            },
            request_total: 1,
            requests: 1,
            user_est: None,
            cache_semantics: CacheSemantics::Exclusive,
            counters: SessionCounters::default(),
            ts_ms: 1,
        })
        .collect();
    SourceDelta {
        details,
        ..Default::default()
    }
}

/// 「保留期外」的旧明细夹具：`n` 条**各自独立**的行键（不是 64 个会话的折叠）
/// → 库里真的落 `n` 行，`purge_detail_before_conn` 的一条 `DELETE` 真的删 `n` 行（W-04 测量用）。
fn old_delta(n: usize) -> SourceDelta {
    let mut d = big_delta(n);
    for (i, row) in d.details.iter_mut().enumerate() {
        row.key.session_id = format!("old{i}");
        row.key.hour_key = "2020-01-01T00".into();
        row.key.day_key = "2020-01-01".into();
    }
    d
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("系统时钟必须晚于 1970")
        .as_millis() as i64
}

/// 持全局 `DB` 锁做一次纯 SQL（**不调任何采集入口**——W-26）
fn with_db<T>(f: impl FnOnce(&rusqlite::Connection) -> T) -> T {
    let conn = database::connection::DB.lock().unwrap();
    f(&conn)
}

fn count_details() -> i64 {
    with_db(|c| {
        c.query_row("SELECT COUNT(*) FROM usage_detail", [], |r| r.get(0))
            .expect("统计 usage_detail 行数失败")
    })
}

/// 库内四桶累计（反例的**库级前提**：证明那 4,000 行真的落盘了，不只是函数返回值好看）
fn sum_input_fresh() -> i64 {
    with_db(|c| {
        c.query_row(
            "SELECT COALESCE(SUM(input_fresh), 0) FROM usage_detail",
            [],
            |r| r.get(0),
        )
        .expect("统计 input_fresh 失败")
    })
}

/// 写库线程的共享统计（跨线程只经原子量/互斥量，不用 `static mut`）
#[derive(Default)]
struct WriterStats {
    rounds_ok: AtomicUsize,
    rounds_err: AtomicUsize,
    rows_written: AtomicUsize,
    batches_last: AtomicUsize,
    max_batch_rows: AtomicUsize,
    max_hold_ms: AtomicU64,
    last_err: Mutex<Option<String>>,
}

struct PollProbe {
    worst_ms: u128,
    worst_us: u128,
    /// `try_lock()` 返回 `WouldBlock` 的次数 = 「采样那一刻确实有人正持 DB 锁」的次数
    locked_by_other: usize,
}

/// 模拟 3 秒轮询：每轮取一次全局 `DB` 锁做一次真实读（`get_tool_enabled`），
/// 并用 `try_lock()` 探针记录「此刻是否有人正持锁」。
///
/// 为什么探针是**必要**的（而不是锦上添花）：`worst_ms < 500` 这条断言只有在
/// 「写库侧真的在持锁」时才有意义；`WouldBlock` 是 Mutex 语义下**唯一**能直接观测到
/// 「别人正持锁」的方式（`try_lock` 成功只说明此刻没人在写）。
///
/// **采样时刻有讲究（第一版就在这里假绿）**：探针**不能**紧贴轮询自己释放锁之后——
/// 那一刻锁刚被轮询放掉、写库线程还没被唤醒，`try_lock` 永远抢得到（首跑实测：
/// 轮询最长等待 19 ms 却 `0/200` 命中）。所以先让出 1ms 给写库线程抢锁，再采样。
fn poll_with_lock_probe(iterations: usize, sleep_ms: u64) -> PollProbe {
    let mut worst_us = 0u128;
    let mut locked_by_other = 0usize;
    for _ in 0..iterations {
        let t = Instant::now();
        // 任意读路径：取同一把全局 DB 锁（真实 API：`database::get_tool_enabled`，
        // `dao/agent_tool.rs:98` 定义、`database/mod.rs:22` 重导出）
        let _ = database::get_tool_enabled("claude");
        worst_us = worst_us.max(t.elapsed().as_micros());
        // 让出窗口的前 1ms：写库线程会在此期间抢到锁并开始下一批
        std::thread::sleep(Duration::from_millis(1));
        if matches!(
            database::connection::DB.try_lock(),
            Err(TryLockError::WouldBlock)
        ) {
            locked_by_other += 1;
        }
        // 剩余让出窗口（保持每轮 ~3ms 的轮询节奏）
        std::thread::sleep(Duration::from_millis(sleep_ms.saturating_sub(1)));
    }
    PollProbe {
        worst_ms: worst_us / 1000,
        worst_us,
        locked_by_other,
    }
}

/// 硬约束①（结构性，确定性）：单事务行数 ≤ `LEDGER_BATCH_ROWS`。
///
/// 同时**打印实测数字**（C4：评估文档 §2 的表格只能由这里产出，不得编造）：
/// `--nocapture` 下会看到 `[measure] 单批最大行数 / 批数 / 单批锁持有最长 ms`。
#[test]
fn ledger_transactions_are_bounded() {
    let _g = serial();
    // **常量本身必须钉住**：本文件所有判据都以 `LEDGER_BATCH_ROWS` / `LEDGER_YIELD_MS` 为基准，
    // 若有人把常量调大，「单事务 ≤ LEDGER_BATCH_ROWS」会**跟着一起变松**（自指式假绿：
    // 把 200 改成 100_000 后本轮所有断言仍全绿）→ 所以先把 GC 2 冻结的这两个值钉死。
    assert_eq!(LEDGER_BATCH_ROWS, 200, "GC 2 冻结的单事务行数上限被人改动");
    assert_eq!(
        ledger::LEDGER_YIELD_MS,
        1,
        "GC 2 冻结的批间让出时长被人改动"
    );

    let delta = big_delta(4_000);
    // 前提：夹具真的产出 4,000 条明细行（否则「批数 ≥ 20」是纸面判据）
    assert_eq!(
        delta.details.len(),
        4_000,
        "前提：夹具必须真的产出 4,000 行"
    );

    let stats = ledger::apply_delta(UsageSourceId::Claude, &delta).expect("落库必须成功");

    // 前提：写入行数真的覆盖 4,000 条明细（+ 日聚合等派生行）
    assert!(
        stats.rows >= 4_000,
        "前提：本轮计划写入行数必须覆盖 4,000 条明细（实际 {}）",
        stats.rows
    );
    assert!(
        stats.max_batch_rows > 0,
        "前提：至少要有一批（否则下面的上界断言空洞）"
    );
    assert!(
        stats.max_batch_rows <= LEDGER_BATCH_ROWS,
        "单事务行数 {} 超过上限 {}（GC 2：禁止一个事务写完一整轮采集）",
        stats.max_batch_rows,
        LEDGER_BATCH_ROWS
    );
    assert!(
        stats.batches >= 4_000 / LEDGER_BATCH_ROWS,
        "批数 {} 少于 4,000/{LEDGER_BATCH_ROWS}（批次被合并了？）",
        stats.batches
    );
    // ↓ C4：真机填表的数据来源（`cargo test … -- --nocapture`）
    println!(
        "[measure] 现状（分批短事务）: 单批 200 行事务耗时上限 = {} ms | 批数 = {} | 最大批行数 = {}",
        stats.max_hold_ms, stats.batches, stats.max_batch_rows
    );
}

/// 硬约束②（行为性）：采集写库期间，模拟 3 秒轮询的取锁请求不被长时间阻塞。
/// 断言的是**单次取锁等待上界**——「一个事务写完整轮」会让它飙到秒级。
/// 阈值取 500ms：单批 200 行 upsert 的正常耗时是毫秒级，500ms 只有极端慢盘才可能触发。
#[test]
fn polling_lock_wait_stays_bounded_while_writing() {
    let _g = serial();

    let stop = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(WriterStats::default());
    let writer_stop = stop.clone();
    let writer_stats = stats.clone();
    let writer = std::thread::spawn(move || {
        while !writer_stop.load(Ordering::SeqCst) {
            // 生产入口：分批短事务 + 批间让出（GC 2）
            match ledger::apply_delta(UsageSourceId::Claude, &big_delta(2_000)) {
                Ok(st) => {
                    writer_stats.rounds_ok.fetch_add(1, Ordering::SeqCst);
                    writer_stats
                        .rows_written
                        .fetch_add(st.rows, Ordering::SeqCst);
                    writer_stats
                        .batches_last
                        .store(st.batches, Ordering::SeqCst);
                    writer_stats
                        .max_batch_rows
                        .fetch_max(st.max_batch_rows, Ordering::SeqCst);
                    writer_stats
                        .max_hold_ms
                        .fetch_max(st.max_hold_ms as u64, Ordering::SeqCst);
                }
                Err(e) => {
                    writer_stats.rounds_err.fetch_add(1, Ordering::SeqCst);
                    *writer_stats.last_err.lock().unwrap() = Some(format!("{e:?}"));
                }
            }
        }
    });

    let probe = poll_with_lock_probe(200, 3);

    // 轮询窗口**结束时**（stop 之前）写库已完成的轮数 → 证明写与查询在时间上真的重叠
    let rounds_during_poll = stats.rounds_ok.load(Ordering::SeqCst);
    stop.store(true, Ordering::SeqCst);
    writer.join().expect("写库线程不得 panic");

    let rounds_err = stats.rounds_err.load(Ordering::SeqCst);
    let rows_written = stats.rows_written.load(Ordering::SeqCst);
    let batches = stats.batches_last.load(Ordering::SeqCst);
    let max_batch_rows = stats.max_batch_rows.load(Ordering::SeqCst);
    let writer_hold_ms = stats.max_hold_ms.load(Ordering::SeqCst);

    // ↓ C4：真机填表的数据来源（`--nocapture`）
    println!(
        "[measure] 现状（分批短事务）: 轮询单次取锁最长等待 = {} ms",
        probe.worst_ms
    );
    println!(
        "[measure-extra] 现状: 轮询窗口内写库完成轮数 = {rounds_during_poll} | 失败轮数 = {rounds_err} | 累计计划行数 = {rows_written} | 每轮批数 = {batches} | 写库侧单批持锁上限 = {writer_hold_ms} ms | try_lock 命中「别人正持锁」= {}/200 | 轮询最长等待 = {} µs",
        probe.locked_by_other, probe.worst_us
    );

    // ---- 前提断言（先证明「并发真的发生了、写库真的在持锁」，再断言上界）----
    assert_eq!(
        rounds_err,
        0,
        "前提：落库路径必须真的成功——失败时锁窗口为空，上界断言会退化成假绿（last_err = {:?}）",
        stats.last_err.lock().unwrap()
    );
    assert!(
        rounds_during_poll >= 3,
        "前提：轮询窗口内写库必须真的跑了多轮（实际 {rounds_during_poll} 轮）"
    );
    assert!(
        rows_written >= 2_000 * rounds_during_poll,
        "前提：写库每轮至少落 2,000 行（实际 {rows_written} 行 / {rounds_during_poll} 轮）"
    );
    assert!(
        batches >= 2_000 / LEDGER_BATCH_ROWS,
        "前提：写库侧每轮真的是分批短事务（实际 {batches} 批）"
    );
    assert_eq!(
        LEDGER_BATCH_ROWS, 200,
        "前提：GC 2 冻结的单事务行数上限被人改动（上面那条批数断言会跟着一起变松）"
    );
    assert!(
        max_batch_rows <= LEDGER_BATCH_ROWS,
        "前提：写库侧单批行数受限（实际 {max_batch_rows}）"
    );
    // **Minor 4（fix round 1）**：`writer_hold_ms` 原来只打印不断言（「采集了却从不断言」）。
    // 它是「写库侧确实持锁了一段时间」的直接证据；恒 0 说明批次根本没占住锁窗口
    // （变异：把写库侧的 `fetch_max` 恒置 0 → 本断言真红）。
    assert!(
        writer_hold_ms > 0,
        "前提：写库侧单批持锁上限必须可测（> 0 ms；实际 {writer_hold_ms}）——恒 0 说明写库没占住锁窗口"
    );
    assert!(
        probe.locked_by_other > 0,
        "前提：轮询窗口内必须观察到「写库正持 DB 锁」（try_lock 全程零 WouldBlock = 两边根本没并发，本用例的计时断言无意义）"
    );
    assert_eq!(
        collect::collect_calls(),
        0,
        "前提：本文件不得触发采集（W-26 / W-22）"
    );

    // ---- 判据（本任务的行为性硬断言）----
    assert!(
        probe.worst_ms < 500,
        "轮询单次取锁最长等待 {}ms —— 写库阻塞了 3 秒轮询（检查是否有人把批次合并成一个大事务）",
        probe.worst_ms
    );
}

/// 硬约束③：**采集不得进入 3 秒会话轮询路径**
/// （会话扫描前后采集轮次计数必须不变；`get_all_sessions` 只碰 monitor，不碰 usage）
#[test]
fn session_poll_never_triggers_collection() {
    let _g = serial();
    // **前提断言（W-22 的教训）**：写成「扫描前后不变」时，只要前面有别的调用把计数推到 1，
    // 断言就退化成恒真（假绿）。本二进制从不采集 → 这里要的是**字面上的 0**。
    assert_eq!(
        collect::collect_calls(),
        0,
        "前提：本测试二进制不得触发过任何采集（否则本用例的判据自毁）"
    );
    let before = collect::collect_calls();
    let resp = adapter::get_all_sessions();
    let after = collect::collect_calls();
    // **Minor 2（fix round 1）：这里原来写作「前提：扫描真的跑完」，是错的。**
    // 空临时 home 下响应必然是 0 会话（本仓 L1「零进程零解析」在编排层就短路，
    // 各 adapter 的 `find_sessions` **根本不会被调用**）→ `0 == 0` 是空洞断言，
    // 不能拿来当「扫描真的跑完」的前提。改成**结构自洽性检查**（不断言「跑完」），
    // 并把「L1 逃逸面」（往某个 adapter 的 `find_sessions` 塞采集入口，行为式断言抓不到）
    // 交给静态锁 `adapter_layer_never_references_collection_entries` 去堵。
    assert_eq!(
        resp.total_count,
        resp.sessions.len(),
        "响应必须结构自洽（totalCount == sessions.len()）——注意：空 home 下这是 0==0，不构成「扫描真的跑完」的证据"
    );
    assert_eq!(after, 0, "get_all_sessions 不得触发用量采集（字面上的 0）");
    assert_eq!(before, after, "get_all_sessions 不得触发用量采集");
}

/// **C4：反例（一个事务写完整轮）的可运行测量**——评估文档 §2 第二行的数据来源。
/// 刻意**忽略**（它会把 3 秒轮询挡到秒级，不适合放进日常门禁）：
/// `cargo test --test usage_concurrency_test -- --ignored --nocapture counterexample`
/// 期望：这行的数字**远大于**上面两行的数字；若两者接近，说明评估结论不成立，必须重测。
#[test]
#[ignore = "反例测量：故意制造秒级锁持有，只在填评估表时手动跑"]
fn counterexample_single_transaction_blocks_polling() {
    let _g = serial();
    let stop = Arc::new(AtomicBool::new(false));
    let rounds = Arc::new(AtomicUsize::new(0));
    let executed_rows = Arc::new(AtomicUsize::new(0));
    let hold_us_max = Arc::new(AtomicU64::new(0));

    // 前提基线：本库可能已有别的用例落的行（`SUM` 是累计列）→ 必须用**差值**判据
    let sum_before = sum_input_fresh();

    let writer_stop = stop.clone();
    let writer_rounds = rounds.clone();
    let writer_rows = executed_rows.clone();
    let writer_hold = hold_us_max.clone();
    let writer = std::thread::spawn(move || {
        // 反例：把 4,000 行**塞进一个事务**（模拟"不分批"的写法；用 `Batch` 之外的路径
        // 直接开事务——这里用 ledger 的公开入口做不到，所以直接操作 DAO + 全局连接）
        while !writer_stop.load(Ordering::SeqCst) {
            let delta = big_delta(4_000);
            let guard = database::connection::DB.lock().unwrap();
            let t = Instant::now();
            let tx = guard.unchecked_transaction().unwrap();
            let written = multi_agents_manager_lib::database::dao::usage::upsert_detail_conn(
                &tx,
                &delta.details,
                1,
            );
            tx.commit().unwrap();
            // 持锁窗口 = 取到锁之后 → commit 之后（不含取锁等待，口径见评估文档）
            let hold_us = t.elapsed().as_micros();
            drop(guard);
            writer_hold.fetch_max(hold_us as u64, Ordering::SeqCst);
            writer_rows.fetch_add(written, Ordering::SeqCst);
            writer_rounds.fetch_add(1, Ordering::SeqCst);
        }
    });

    let probe = poll_with_lock_probe(200, 3);

    let rounds_during_poll = rounds.load(Ordering::SeqCst);
    stop.store(true, Ordering::SeqCst);
    writer.join().expect("写库线程不得 panic");

    let rounds_total = rounds.load(Ordering::SeqCst);
    let executed = executed_rows.load(Ordering::SeqCst);
    let hold_ms_max = (hold_us_max.load(Ordering::SeqCst) / 1000) as u128;
    let sum_delta = sum_input_fresh() - sum_before;

    println!(
        "[measure] 反例（一个事务写 4,000 行）: 轮询单次取锁最长等待 = {} ms",
        probe.worst_ms
    );
    println!(
        "[measure-extra] 反例: 大事务自身持锁上限 = {hold_ms_max} ms（含 commit，不含取锁等待）| 轮数 = {rounds_total}（轮询窗口内 {rounds_during_poll}）| try_lock 命中「别人正持锁」= {}/200 | 单事务执行行数 × 轮数 = {executed} | 库内 SUM(input_fresh) 增量 = {sum_delta}",
        probe.locked_by_other
    );

    // ---- 前提断言（反例必须真的「一个事务写 4,000 行」并且真的落盘）----
    assert!(
        rounds_during_poll >= 1,
        "前提：轮询窗口内反例写库必须至少跑完一轮（实际 {rounds_during_poll}）"
    );
    assert_eq!(
        executed,
        4_000 * rounds_total,
        "前提：每个大事务真的执行了 4,000 条明细 upsert"
    );
    assert_eq!(
        sum_delta,
        4_000 * rounds_total as i64,
        "前提（库级）：4,000 行/轮 真的落进了 usage_detail（SUM(input_fresh) 增量必须逐位相等）"
    );
    assert!(
        probe.locked_by_other > 0,
        "前提：必须观察到反例线程真的持 DB 锁（否则本行数字量不到东西）"
    );
    assert!(hold_ms_max > 0, "前提：大事务自身持锁必须可测（> 0 ms）");
    assert!(probe.worst_ms > 0, "至少要测到一个非零等待（用于填表）");
}

/// **W-04 / W-05（Task 22 被点名的动作）**：`purge_expired` 的**单条无上限 `DELETE`**
/// 也在同一把全局 `DB` 锁下——把它一并纳入锁窗口口径；并用**触发器造真实删失败**，
/// 把 W-05「清理失败 ≡ 无行可清」从散文变成可执行事实。
///
/// `#[ignore]`：它会往共享库写 5,000 行（只在填评估表时手动跑）：
/// `cargo test --test usage_concurrency_test -- --ignored --nocapture purge_expired`
#[test]
#[ignore = "清理路径的锁窗口测量（W-04）+ 静默降级特征化（W-05）：只在填评估表时手动跑"]
fn purge_expired_lock_window_and_silent_degradation() {
    let _g = serial();

    // ---- W-04：把这条 DELETE 的锁窗口量出来（`purge_expired` 自身取全局 DB 锁）----
    let rows_before = seed_old_rows(5_000);
    let t = Instant::now();
    let deleted = ledger::purge_expired(7, now_ms()).expect("清理不得返回 Err");
    let hold_ms = t.elapsed().as_millis();
    assert!(
        deleted >= 5_000,
        "前提：这一次 DELETE 真的删掉了那 5,000 行（实际 {deleted}）"
    );
    assert_eq!(
        count_details(),
        rows_before - deleted as i64,
        "前提：删除行数必须与库内行数变化一致"
    );
    println!(
        "[measure] purge_expired（单条无上限 DELETE，删除 {deleted} 行）: 锁窗口 = {hold_ms} ms"
    );

    // ---- W-05：让 DELETE 真的失败 → `Ok(0)` 与「本来就没有行可清」**不可区分** ----
    // 这是对**现状的特征化断言**（characterization）：修复 W-05（DAO 区分失败/0 行）后，
    // 本段必须一并改写为「返回 Err / 带失败标志」。现在锁住的是「静默降级」这个事实本身。
    let blocker_rows = seed_old_rows(2_000);
    assert!(
        blocker_rows >= 2_000,
        "前提：库里确实还有行可清（否则 Ok(0) 无法与「无行可清」区分）"
    );
    with_db(|c| {
        c.execute_batch(
            "CREATE TRIGGER mam_test_block_purge BEFORE DELETE ON usage_detail \
             BEGIN SELECT RAISE(ABORT, 'blocked by test'); END;",
        )
    })
    .expect("前提：建阻塞触发器必须成功（usage_detail 表存在）");
    let deleted_blocked = ledger::purge_expired(7, now_ms()).expect("W-05 现状：清理失败被吞成 Ok");
    assert_eq!(
        deleted_blocked, 0,
        "W-05 现状特征化：DELETE 被触发器拒绝 → 返回 0（与「没有行可清」完全不可区分）"
    );
    assert_eq!(
        count_details(),
        blocker_rows,
        "前提：其实一行都没删（0 是「失败」，不是「无行可清」）"
    );
    with_db(|c| c.execute_batch("DROP TRIGGER IF EXISTS mam_test_block_purge"))
        .expect("前提：撤触发器必须成功");
    let deleted_after = ledger::purge_expired(7, now_ms()).expect("撤销阻塞后清理必须成功");
    assert!(
        deleted_after >= 2_000,
        "撤销阻塞后同一批行必须被删掉（实际 {deleted_after}）——证明前面那个 0 是「失败被吞」而不是「无行可清」"
    );
    assert_eq!(
        count_details(),
        blocker_rows - deleted_after as i64,
        "前提：删除行数必须与库内行数变化一致"
    );
    println!(
        "[measure-extra] W-05: 撤掉阻塞后同一条 DELETE 删掉 {deleted_after} 行（同一个 0 的两种含义在此分叉）"
    );
}

/// 写入 `n` 行保留期之外的旧明细（每条一个独立行键 → 库里真的 `n` 行），返回写后的库内总行数。
fn seed_old_rows(n: usize) -> i64 {
    let stats =
        ledger::apply_delta(UsageSourceId::Claude, &old_delta(n)).expect("旧行落库必须成功");
    assert!(
        stats.max_batch_rows <= LEDGER_BATCH_ROWS,
        "前提：落库仍走分批短事务（单批 {} 行）",
        stats.max_batch_rows
    );
    let rows = count_details();
    assert!(
        rows >= n as i64,
        "前提：保留期外的旧行必须真的落库（期望 ≥ {n}，实际 {rows}）"
    );
    rows
}

/// **测试隔离自省锁**（Task 17 `dsh.rs` 先例的移植）：本文件每条用例都碰进程级全局量
/// （全局 `DB` / env / `COLLECT_CALLS`），漏一条串行锁就会间歇性互相串味。
/// 去掉任一条 `serial()` → 本用例真红。
#[test]
fn every_test_in_this_file_takes_the_serial_lock() {
    let _g = serial();
    let src = include_str!("usage_concurrency_test.rs");
    let lines: Vec<&str> = src.lines().collect();
    let mut checked = 0;
    for (i, l) in lines.iter().enumerate() {
        if l.trim() != "#[test]" {
            continue;
        }
        let end = (i + 5).min(lines.len());
        // 逐行比对**确切语句**：`window.contains(...)` 对注释掉的同行也为真（Task 20 Minor 7 的假绿）
        let has_lock = lines[i..end]
            .iter()
            .any(|l| l.trim() == "let _g = serial();");
        assert!(
            has_lock,
            "第 {} 行的用例没有取 `let _g = serial();` 串行锁（Task 17 教训：进程级全局量必须串行；注释里的不算）",
            i + 1
        );
        checked += 1;
    }
    assert_eq!(
        checked, 8,
        "本文件应有 8 条用例（含 2 条 #[ignore] 测量与 3 条自省/静态锁）；新增用例必须同步持有 serial()"
    );

    // **Minor 3（fix round 1）的结构锁**：`serial()` 内部必须**先取锁、再改 env**
    // （env 是进程级全局量，`remove_var` 落在临界区外就等于「它要保护的东西在锁外」）。
    // 变异：把 `let guard = SERIAL.lock()` 挪回函数最后一行 → 本断言真红。
    let fn_at = lines
        .iter()
        .position(|l| l.trim_start().starts_with("fn serial()"))
        .expect("前提：本文件必须有 serial() 定义");
    let body = &lines[fn_at..(fn_at + 16).min(lines.len())];
    let lock_at = body
        .iter()
        .position(|l| l.contains("SERIAL.lock()"))
        .expect("前提：serial() 必须取 SERIAL 串行锁");
    let env_at = body
        .iter()
        .position(|l| l.contains("remove_var"))
        .expect("前提：serial() 必须摘掉 DSH_HOME / KIMI_CODE_HOME");
    assert!(
        lock_at < env_at,
        "`serial()` 必须先取 SERIAL 串行锁（相对第 {lock_at} 行）、再改进程级 env（相对第 {env_at} 行）——Minor 3"
    );
}

/// **W-26 自省锁**：本文件存在持 `DB` 锁的路径（反例 / 清理测量 / 探针），因此
/// **绝不能**出现采集入口——持锁时调 `collect()` / `collect_with()` / `run_collection()`
/// 就是在飞扫描（需要 `DB`）与 `DB` 的 ABBA 死锁，而 `std::sync::Mutex` 不可重入、无超时。
/// 禁项全部是本仓**真实 API 名**（不是注释承诺）；字符串用 `concat!` 拼出，
/// 避免自省扫描把自己的字面量数进去（否则恒红）。
#[test]
fn this_file_never_calls_collection_entries() {
    let _g = serial();
    let src: String = include_str!("usage_concurrency_test.rs")
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    // 正向前提：本文件确实有持 DB 锁的路径（否则这条自省锁是纸面的）
    assert!(
        src.contains(concat!("DB", ".lock()")),
        "前提：本文件必须真的存在持 DB 锁的路径（否则 W-26 自省锁没有保护对象）"
    );
    for forbidden in [
        concat!("collect::", "collect("),
        concat!("collect", "_with("),
        concat!("run", "_collection("),
        concat!("spawn_initial", "_collection"),
        concat!("usage", "_collect"),
    ] {
        assert!(
            !src.contains(forbidden),
            "本文件不得出现 `{forbidden}`：持 DB 锁时调采集入口 = ABBA 死锁（W-26），\
             且会毁掉「字面上的 0」前提（W-22）"
        );
    }
}

/// **Minor 2（fix round 1）的补洞半边**：`adapter` 层是 3 秒轮询的**内核**
/// （`adapter/mod.rs:547-555` 逐个调各 adapter 的 `find_sessions`），而本仓 L1「零进程零解析」
/// 是**按源**判定的：某源此刻**没有运行进程** → 编排层直接给空表、**根本不调它的 `find_sessions`**
/// （与 home 里有没有夹具无关）→ **「往该 adapter 的 `find_sessions` 里塞一行采集入口」会逃逸**。
/// 本用例把它变成**静态机械锁**：`src/adapter/**/*.rs` 一律不得引用采集入口。
///
/// 变异实测（fix round 1）：
/// * 往 **`kimi.rs`** 的 `find_sessions` 插一行 `collect::collect(false)`（本机**没有** kimi 进程
///   → L1 在编排层跳过它）→ **本用例真红**，而 `session_poll_never_triggers_collection`
///   **仍是绿的**（1 passed）——逃逸面被当场复现，这就是本用例存在的理由；
/// * 对照：同一个变异放到 **`claude.rs`**（本机**有** claude 进程 → `find_sessions` 真被调用）
///   → 行为式用例能抓到（RED）。
///
/// ⇒ 「行为式断言够不够」取决于**该源此刻有没有运行进程**，所以必须有静态锁兜住全部 adapter。
#[test]
fn adapter_layer_never_references_collection_entries() {
    let _g = serial();
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/adapter");
    assert!(
        dir.is_dir(),
        "前提：{} 必须存在（静态锁依赖仓内路径，缺失即误红）",
        dir.display()
    );
    let mut files: Vec<(String, String)> = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("读取 src/adapter 必须成功") {
        let path = entry.expect("目录项必须可读").path();
        if path.extension().map(|e| e == "rs").unwrap_or(false) {
            let name = path
                .file_name()
                .expect("文件名必须可解析")
                .to_string_lossy()
                .into_owned();
            let text = std::fs::read_to_string(&path).expect("adapter 源文件必须可读");
            files.push((name, text));
        }
    }
    // 正向前提：被检查清单必须非空且含**编排层**（轮询内核的入口所在）
    assert!(
        files.len() >= 9,
        "前提：src/adapter 至少 9 个 .rs（实际 {}）——清单为空/缩水时本锁会静默失效",
        files.len()
    );
    assert!(
        files.iter().any(|(n, _)| n == "mod.rs"),
        "前提：mod.rs（`get_all_sessions` 编排层 = 轮询内核）必须在被检查清单内"
    );
    for (name, text) in &files {
        for forbidden in [
            concat!("services::", "usage::collect"),
            concat!("usage", "_collect"),
            concat!("collect::", "collect("),
            concat!("collect::", "collect", "_with("),
            concat!("run", "_collection("),
            concat!("spawn_initial", "_collection"),
        ] {
            assert!(
                !text.contains(forbidden),
                "src/adapter/{name} 不得引用采集入口 `{forbidden}`：adapter 层是 3 秒轮询内核（GC 3 / W-22）"
            );
        }
    }
}
