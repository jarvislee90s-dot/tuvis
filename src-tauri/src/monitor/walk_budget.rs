//! FS 遍历预算内核（2026-11-16 真机发现②加固轮；终验六收敛模型：
//! **walk-permit-settle-once**）。
//!
//! 真机证据（`docs/engineering/evidence/2026-11-16-mac-acceptance-run/` 的
//! `vfs-self-deadlock-analysis.md` 与 scan-hang-sample*.txt）：应用内三条并发
//! 文件遍历线（scan_bus 看板扫描 / 只读消息走 / wakeup·动作路径快照）在 macOS
//! 内核同时楔死（readdir/open/getattrlist 各线程分挂 getdirentries 数分钟），
//! 而同机单线程探针瞬时通过——「内核楔死 → 请求无限期挂」。本模块把这类
//! 遍历包进请求级死线：超预算即返回（有界超时降级 + 重试），卡住的线程
//! **detached 保留**（内核操作不可中断，最终会自愈完成，不杀）。
//!
//! ## 模型（删除性重构后的最终形态）
//!
//! Important-2 修复后遍历段已是**纯读**（超时弃产物零副作用）——timeout 与
//! completion 的竞争只剩「许可释放恰好一次」，**不需要 stranded 概念**：
//!
//! 1. `PERMITS`：单 `AtomicUsize` 仅计**准入许可**（admitted 未 settle 数）；
//!    admit = 单字 CAS 容量检查 + 占位（单线性化点，无两读组合窗口）。
//! 2. 每 walk 私有 `settled: AtomicBool`。**timeout 与 completion 谁先到谁
//!    释放**：`if settled.swap(true) == false { permits -= 1 }`——恰好一次，
//!    无双计、无泄漏、无窗口（竞争败者看到 true 跳过，零额外动作）。
//! 3. stranded/WalkCancel/record-publish 全套删除（walk 段纯读使超时早释放
//!    绝对安全——这是 Important-2 修复送我们的礼物）。
//! 4. 楔死线程的资源面独立：`DETACHED_THREADS`（walk 线程存活数，体首 +1 /
//!    体尾 -1）做**纯资源阀门**（超 [`MAX_DETACHED_WALK_THREADS`] Busy，不参与
//!    准入语义），与许可记账彻底分离。
//! 5. 常量收敛：[`MAX_ACTIVE_PERMITS`] = 8；[`MAX_DETACHED_WALK_THREADS`] = 6。
//!
//! ## 交错分析（终验六逐一核对；对应测试见各回归注释）
//!
//! 记号：许可计数 P，每 walk 私有 settled 位 B（初 false），一次成功 admit
//! 使 P +1。
//!
//! * **顺序①（timeout settle 先）**：caller recv 超时 → `B.swap(true)` 得
//!   false → P −1 → 楔死 walk 自愈完成 → completion settle 见 true → 跳过，
//!   零额外动作。P 恰减 1 ✓（`timeout_and_completion_settle_exactly_once`
//!   顺序①断言）。
//! * **顺序②（completion settle 先）**：walk 在预算内完成 → completion
//!   settle 得 false → P −1 → caller recv 随后超时进 Timeout 分支 → settle
//!   见 true → 跳过。P 恰减 1 ✓（同测试顺序②断言）。
//! * **并发 admit**：P 的 CAS 是唯一线性化点；「最后槽位竞争」回归钉住
//!   恰好一个放行（`concurrent_contenders_for_last_slot_admit_exactly_one`）。
//! * **无下溢**：每次 P −1 都对应一次成功 admit 的 +1（settle 恰一次；
//!   spawn 失败走独立回退，无 settle 方）。
//! * **无泄漏**：两个结算方竞争由 `swap` 裁决，败者零动作——不存在
//!   「+1 永久无对手方」的保守泄漏形态（前轮 stranded 记账模型的缺陷面
//!   整体消失）。
//! * **超时早释放安全**：walk 段纯读（Important-2），超时早释放后楔死线程
//!   继续只读、产物随通道丢弃——早释放零副作用。
//!
//! [`WalkError`] 必须可区分于业务错误：上层据此降级，不改硬下界（超时 ≠
//! 通过）。无跨线程锁：实现只用 mpsc + 原子计数，请求热路径不新增锁。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

/// 看板扫描轮预算（45s）：真实环境大库一端正常耗时可观（秒级～几十秒量级），
/// 45s 给足余量；设计上限 60s 封顶（2026-11-16 真机发现②裁决）。楔死轮按超时
/// 降级，不拖死 worker 节拍。
pub const SCAN_WALK_BUDGET: Duration = Duration::from_secs(45);

/// 只读消息走预算（9s）：对齐动作路径快照既有 9s 口径（2026-11-16 裁决）。
/// 超时 → `DSH_READ_TIMEOUT`，/m 的 session-messages 端点映射 HTTP 504
/// （前端「读取超时，点重试」语义）。
pub const READONLY_ROOT_BUDGET: Duration = Duration::from_secs(9);

/// wakeup·事件目录快照预算（9s）：与只读消息走同口径。超时返回 uncertain
/// 快照（本拍按「不可证明」处理，清理不删文件）。
pub const WAKEUP_SNAPSHOT_BUDGET: Duration = Duration::from_secs(9);

/// 准入许可上限（终验六收敛常量）：合法并发峰值 = 详情页挂载三路只读根解析
/// （`src/mobile/SessionDetail.tsx` 消息 :833 / 文件 :898 / 子任务 :920）+
/// 看板扫描 1 + wakeup 快照 1 = **5 路**；取 8 留 3 余量。超限 → 新准入 Busy
/// 快失败——可接受的拒绝行为：各调用方按自己的降级语义处理（504 重试 /
/// uncertain / 保留上一轮快照）。
pub const MAX_ACTIVE_PERMITS: usize = 8;

/// detached 遍历线程资源阀门（终验六收敛常量）：**纯资源面**——楔死线程
/// （超时早释放许可后仍未自愈的 detached 线程）堆积到 6 即 Busy，不参与准入
/// 语义（许可记账与它彻底分离）。重启清零。
pub const MAX_DETACHED_WALK_THREADS: usize = 6;

/// 生产全局许可计数（终验六：单字，仅准入语义）。诊断口 [`active_walks`]。
static PERMITS: once_cell::sync::Lazy<std::sync::Arc<AtomicUsize>> =
    once_cell::sync::Lazy::new(|| std::sync::Arc::new(AtomicUsize::new(0)));

/// 生产全局 detached 遍历线程存活数（纯资源阀门计数）。诊断口
/// [`detached_walk_threads`]。
static DETACHED_THREADS: once_cell::sync::Lazy<std::sync::Arc<AtomicUsize>> =
    once_cell::sync::Lazy::new(|| std::sync::Arc::new(AtomicUsize::new(0)));

/// 当前未结算（admitted 未 settle）的许可数。诊断/巡检可查，不参与判定硬下界。
pub fn active_walks() -> usize {
    PERMITS.load(Ordering::SeqCst)
}

/// 当前存活的 detached 遍历线程数（含预算内在跑者——纯资源观测口径）。
pub fn detached_walk_threads() -> usize {
    DETACHED_THREADS.load(Ordering::SeqCst)
}

/// 每 walk 私有的 settle-once 位：timeout 与 completion 谁先到谁释放许可，
/// 另一方见 true 跳过（恰好一次）。
#[derive(Clone)]
pub struct SettleOnce {
    settled: std::sync::Arc<AtomicBool>,
}

impl SettleOnce {
    fn new() -> Self {
        Self {
            settled: std::sync::Arc::new(AtomicBool::new(false)),
        }
    }

    #[cfg(test)]
    fn with_settled(settled: std::sync::Arc<AtomicBool>) -> Self {
        Self { settled }
    }

    /// 恰好一次释放：返回 true = 本调用是首次结算（调用方执行 P −1）。
    fn release(&self) -> bool {
        !self.settled.swap(true, Ordering::SeqCst)
    }
}

/// 受限遍历的失败形态。与业务错误严格可区分：上层据此降级（超时 ≠ 通过）。
/// （Panicked 载荷不可比较/克隆，故 WalkError 整体不做 PartialEq。）
#[derive(Debug)]
pub enum WalkError {
    /// 预算内未完成。卡住的线程 detached 保留（内核操作不可中断，不杀——
    /// 最终会自愈完成）；其许可已由超时方释放（settle-once）。
    TimedOut(WalkTimedOut),
    /// 准入许可或 detached 线程资源达上限：不 spawn，快失败。调用方跳过本轮
    /// 并告警。
    Busy,
    /// 遍历线程 panic（载荷原样回传，调用方 `resume_unwind` 保持既有 panic 语义）。
    Panicked(Box<dyn std::any::Any + Send>),
    /// 遍历线程启动失败（线程句柄耗尽等系统级失败）。
    Spawn(String),
}

impl std::fmt::Display for WalkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WalkError::TimedOut(t) => write!(f, "walk timed out: {}", t),
            WalkError::Busy => write!(
                f,
                "too many walks (permits cap {MAX_ACTIVE_PERMITS}, detached threads cap {MAX_DETACHED_WALK_THREADS})"
            ),
            WalkError::Panicked(_) => write!(f, "walk panicked"),
            WalkError::Spawn(e) => write!(f, "walk thread spawn failed: {e}"),
        }
    }
}

/// 超时明细：线名（归因）与预算（诊断）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkTimedOut {
    /// 接线面注册名（同时是线程名后缀）。
    pub name: &'static str,
    /// 本次调用的预算。
    pub budget: Duration,
}

impl std::fmt::Display for WalkTimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} did not finish within {:?}", self.name, self.budget)
    }
}

/// 生产入口：全局许可计数 + 资源阀门 + [`MAX_ACTIVE_PERMITS`]/[`MAX_DETACHED_
/// WALK_THREADS`] 的受限遍历。
pub fn run_bounded<T, F>(name: &'static str, budget: Duration, f: F) -> Result<T, WalkError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    run_bounded_scoped(
        &PERMITS,
        &DETACHED_THREADS,
        MAX_ACTIVE_PERMITS,
        MAX_DETACHED_WALK_THREADS,
        name,
        budget,
        f,
    )
}

/// 计数器与上限可注入的内核（生产走 [`run_bounded`]；测试注入独立计数器与
/// 小上限，避免同进程并行测试经全局计数互串）。
///
/// 语义（回归锚点）：
/// * 快走返回值（基线）；
/// * 超预算 → `Err(TimedOut)` 有界返回，线程 detached 保留，**许可已由超时方
///   释放**（settle-once——新准入立即可用），楔死线程自愈完成零二次释放；
/// * 许可满或 detached 线程资源满 → 不 spawn，立即 `Busy`；
/// * panic 以 `Panicked` 载荷回传（调用方决定 resume 语义）。
pub fn run_bounded_scoped<T, F>(
    permits: &std::sync::Arc<AtomicUsize>,
    detached_threads: &std::sync::Arc<AtomicUsize>,
    max_active_permits: usize,
    max_detached_threads: usize,
    name: &'static str,
    budget: Duration,
    f: F,
) -> Result<T, WalkError>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    // 快失败闸（终验六）：许可阀门先行；detached 线程资源阀门在 spawn 前
    // **原子预留**（NEEDS_FIX Important 终验收口：检查与预留同一 CAS 线性化点
    // ——分离式检查在并发下会全部通过预检查后各自 spawn，存活峰值越过阀门）。
    admit_slot(permits, max_active_permits)?;
    // NEEDS_FIX Important 终验收口：detached 预留失败必须回退已占的许可位
    //（防许可泄漏——泄漏会让 active 视图低估、多放行越过容量）。
    if admit_detached_slot(detached_threads, max_detached_threads).is_err() {
        permits.fetch_sub(1, Ordering::SeqCst);
        return Err(WalkError::Busy);
    }
    let settle = SettleOnce::new();
    let settle_for_walk = settle.clone();
    let (tx, rx) = mpsc::channel();
    let permits_for_walk = std::sync::Arc::clone(permits);
    let detached_for_walk = std::sync::Arc::clone(detached_threads);
    let spawned = std::thread::Builder::new()
        // 线程名前缀 `fs-walk:` + 线名：`sample` 抓内核楔死时可直接归因到接线面。
        .name(format!("fs-walk:{name}"))
        .spawn(move || {
            // catch_unwind：panic 载荷原样回传调用方（由调用方决定 resume 语义）；
            // settle-once 保证许可恰好释放一次（本线程 completion 结算 vs caller
            // 超时结算，谁先到谁释放）。detached 槽位在 spawn 前已预留（本线程
            // 结束时释放）。
            let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
            settle_once(&settle_for_walk, &permits_for_walk);
            detached_for_walk.fetch_sub(1, Ordering::SeqCst);
            let _ = tx.send(payload);
        });
    let handle = match spawned {
        Ok(handle) => handle,
        Err(e) => {
            settle_once(&settle, permits);
            detached_threads.fetch_sub(1, Ordering::SeqCst);
            return Err(WalkError::Spawn(e.to_string()));
        }
    };
    match rx.recv_timeout(budget) {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(payload)) => Err(WalkError::Panicked(payload)),
        // 死线到：超时方释放许可（settle-once；楔死线程 detached 保留自愈）。
        Err(mpsc::RecvTimeoutError::Timeout) => {
            settle_once(&settle, permits);
            Err(WalkError::TimedOut(WalkTimedOut { name, budget }))
        }
        // sender 提前丢弃（线程被杀等罕见形态）：join 收尾保证计数已释放。
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            let _ = handle.join();
            Err(WalkError::Spawn("walk thread died before reporting".into()))
        }
    }
}

/// 恰好一次释放许可（终验六核心原语）：`settled.swap(true)` 裁决——首次结算
/// 方执行 P −1，竞争败者看到 true 跳过（零额外动作）。
fn settle_once(settled: &SettleOnce, permits: &AtomicUsize) {
    if settled.release() {
        permits.fetch_sub(1, Ordering::SeqCst);
    }
}

/// 准入占位（单字 CAS，终验六：容量判断与占位同一原子线性化点，无两读组合
/// 窗口）。CAS 失败（他人抢先）→ 重读重判再重试。
fn admit_slot(permits: &AtomicUsize, max_active_permits: usize) -> Result<(), WalkError> {
    loop {
        let cur = permits.load(Ordering::SeqCst);
        if cur >= max_active_permits {
            return Err(WalkError::Busy);
        }
        if permits
            .compare_exchange_weak(cur, cur + 1, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return Ok(());
        }
    }
}

/// detached 线程资源槽位预留（NEEDS_FIX Important 终验收口）：**检查与预留
/// 同一原子线性化点**——CAS 失败（他人抢先占槽）后重读重判再重试。旧实现
/// 「spawn 前单次读计数 + 线程入口才 +1」的分离式检查在并发下会全部通过
/// 预检查并 spawn（存活峰值越过阀门）。
fn admit_detached_slot(
    detached_threads: &AtomicUsize,
    max_detached_threads: usize,
) -> Result<(), WalkError> {
    loop {
        let cur = detached_threads.load(Ordering::SeqCst);
        if cur >= max_detached_threads {
            return Err(WalkError::Busy);
        }
        if detached_threads
            .compare_exchange_weak(cur, cur + 1, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return Ok(());
        }
    }
}

/// 统一测试隔离锁（NEEDS_FIX Important-4）：触碰 walker 全局注入缝的接线面
/// 测试共用同一把锁，杜绝并行测试互抢注入缝。持锁期间设置的覆写须以 RAII
/// 守卫在 Drop 时恢复（防用例 panic 泄漏覆写）。
#[cfg(test)]
pub(crate) fn walk_test_guard() -> std::sync::MutexGuard<'static, ()> {
    static WALK_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    WALK_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Instant;

    /// 独立许可/线程计数器：内核回归不污染生产全局计数（同进程并行测试互不
    /// 串扰）。
    fn scoped() -> (Arc<AtomicUsize>, Arc<AtomicUsize>) {
        (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)))
    }

    fn permits_of(counters: &(Arc<AtomicUsize>, Arc<AtomicUsize>)) -> usize {
        counters.0.load(Ordering::SeqCst)
    }

    fn detached_of(counters: &(Arc<AtomicUsize>, Arc<AtomicUsize>)) -> usize {
        counters.1.load(Ordering::SeqCst)
    }

    /// 轮询等待两计数都归零（后台线程完成后计数释放）。
    fn wait_until_drained(
        counters: &(Arc<AtomicUsize>, Arc<AtomicUsize>),
        deadline: Duration,
    ) -> bool {
        let start = Instant::now();
        while start.elapsed() < deadline {
            if permits_of(counters) == 0 && detached_of(counters) == 0 {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        permits_of(counters) == 0 && detached_of(counters) == 0
    }

    #[test]
    fn run_bounded_returns_value_for_fast_walk() {
        let counters = scoped();
        let value = run_bounded_scoped(
            &counters.0,
            &counters.1,
            4,
            4,
            "unit-fast",
            Duration::from_millis(500),
            || 6u32 * 7,
        );
        assert_eq!(value.unwrap(), 42);
        assert_eq!(permits_of(&counters), 0, "快走完成后许可必须归零");
        assert_eq!(detached_of(&counters), 0, "快走完成后线程计数必须归零");
    }

    #[test]
    fn walk_thread_name_carries_fs_walk_prefix() {
        let counters = scoped();
        let seen = run_bounded_scoped(
            &counters.0,
            &counters.1,
            4,
            4,
            "attribution",
            Duration::from_millis(500),
            || {
                std::thread::current()
                    .name()
                    .unwrap_or_default()
                    .to_string()
            },
        )
        .unwrap();
        assert!(
            seen.starts_with("fs-walk:"),
            "遍历线程名必须带 fs-walk: 前缀（sample 归因），实际 {seen:?}"
        );
        assert!(
            seen.contains("attribution"),
            "线程名须携带线名，实际 {seen:?}"
        );
    }

    /// NEEDS_FIX Important-A 终验回归（settle-once）：超时有界返回且**立即释放
    /// 许可**（新准入立即可用）；楔死线程自愈完成后零二次释放（许可不复活、
    /// 不下溢）。
    #[test]
    fn stuck_thread_releases_permit_on_timeout_then_no_leak() {
        let counters = scoped();
        let started = Instant::now();
        let result = run_bounded_scoped(
            &counters.0,
            &counters.1,
            1,
            4,
            "stuck-walk",
            Duration::from_millis(300),
            || {
                // 楔死模拟：预算 300ms，f 睡 1s（超时返回后 detached 线程继续睡）
                std::thread::sleep(Duration::from_secs(1));
                1u32
            },
        );
        assert!(
            matches!(result, Err(WalkError::TimedOut(_))),
            "超预算必须返回 TimedOut，实际 {result:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        // 超时方已释放许可：新准入**立即可用**（无泄漏、无额度扣留）
        let again = run_bounded_scoped(
            &counters.0,
            &counters.1,
            1,
            4,
            "post-timeout-admit",
            Duration::from_millis(200),
            || 2u32,
        );
        assert_eq!(again.unwrap(), 2, "超时释放后新准入必须立即可用");
        // 楔死线程自愈完成后零二次释放：两计数归零且不再变化
        assert!(
            wait_until_drained(&counters, Duration::from_secs(3)),
            "自愈完成后计数必须归零"
        );
        let drained = (permits_of(&counters), detached_of(&counters));
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            (permits_of(&counters), detached_of(&counters)),
            drained,
            "自愈完成零二次释放（settled 位裁决，无下溢/无泄漏）"
        );
    }

    /// NEEDS_FIX Important-A 终验回归（握手固定两种顺序，零时序假设）：timeout
    /// 与 completion 的结算以任意顺序各执行一次，许可**恰好减 1**、settled 位
    /// 裁决另一方跳过。
    ///
    /// 交错分析（本测试逐条钉住）：
    /// * 顺序①（timeout settle 先）：P −1 → completion settle 见 settled=true
    ///   → 跳过（零额外动作）；
    /// * 顺序②（completion settle 先）：P −1 → timeout settle 见 settled=true
    ///   → 跳过；
    /// * 两序下 P 均恰减 1、无下溢、无泄漏——旧 stranded 记账模型的
    ///   「预增窗口越界」「保守泄漏」面在新模型中不存在。
    #[test]
    fn timeout_and_completion_settle_exactly_once() {
        // 顺序①：timeout 角色先结算
        let counters = scoped();
        counters.0.fetch_add(1, Ordering::SeqCst); // 模拟已 admit（P=1）
        let settled = Arc::new(AtomicBool::new(false));
        settle_once(&SettleOnce::with_settled(Arc::clone(&settled)), &counters.0);
        assert_eq!(permits_of(&counters), 0, "顺序①：首次结算释放许可");
        settle_once(&SettleOnce::with_settled(Arc::clone(&settled)), &counters.0);
        assert_eq!(
            permits_of(&counters),
            0,
            "顺序①：后到方零额外动作（无下溢/无双计）"
        );
        assert!(settled.load(Ordering::SeqCst), "顺序①：settled 位已置");

        // 顺序②：completion 角色先结算（全新实例，角色互换）
        let counters2 = scoped();
        counters2.0.fetch_add(1, Ordering::SeqCst);
        let settled2 = Arc::new(AtomicBool::new(false));
        settle_once(
            &SettleOnce::with_settled(Arc::clone(&settled2)),
            &counters2.0,
        );
        assert_eq!(permits_of(&counters2), 0, "顺序②：首次结算释放许可");
        settle_once(
            &SettleOnce::with_settled(Arc::clone(&settled2)),
            &counters2.0,
        );
        assert_eq!(
            permits_of(&counters2),
            0,
            "顺序②：后到方零额外动作（无下溢/无双计）"
        );
        assert!(settled2.load(Ordering::SeqCst), "顺序②：settled 位已置");
    }

    /// 准入决策边界（终验六单字模型）：许可满 → Busy；未满 → 放行。旧「双计数
    /// 快照组合」交错在单字模型中不可表达（无 stranded 字段可陈旧）。满界用
    /// 真实并发钉住：4 路挂起占满后，第 5 路 Busy，放行后 4 路全部完成。
    #[test]
    fn admission_rejects_at_capacity_admits_below() {
        let counters = scoped();
        let mut holds = Vec::new();
        let mut results = Vec::new();
        for _ in 0..4 {
            let (hold_tx, hold_rx) = std::sync::mpsc::channel::<()>();
            let (result_tx, result_rx) = std::sync::mpsc::channel();
            holds.push(hold_tx);
            let permits_for_thread = Arc::clone(&counters.0);
            let detached_for_thread = Arc::clone(&counters.1);
            std::thread::spawn(move || {
                let r = run_bounded_scoped(
                    &permits_for_thread,
                    &detached_for_thread,
                    4,
                    6,
                    "gate-probe",
                    Duration::from_secs(30),
                    move || {
                        let _ = hold_rx.recv();
                        1u32
                    },
                );
                let _ = result_tx.send(r);
            });
            results.push(result_rx);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while permits_of(&counters) < 4 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(permits_of(&counters), 4, "4 路并发必须全部在跑");
        // 第 5 路：槽满 → Busy（不 spawn）
        let fifth = run_bounded_scoped(
            &counters.0,
            &counters.1,
            4,
            6,
            "fifth-walk",
            Duration::from_millis(200),
            || panic!("槽满的准入绝不允许 spawn"),
        );
        assert!(matches!(fifth, Err(WalkError::Busy)));
        // 放行收尾：全部完成，计数归零
        drop(holds);
        for rx in &results {
            let r = rx.recv_timeout(Duration::from_secs(10)).expect("按时收货");
            assert!(r.is_ok());
        }
        assert!(wait_until_drained(&counters, Duration::from_secs(5)));
    }

    /// 占位下的顺序拒绝（终验 Minor-1 收口）：holder park 占住唯一槽位后，
    /// 竞争者必须逐个 Busy（调度先后不影响结果——被占槽位对一切后来者满），
    /// holder 释放后恰好放行。并发抢空槽的 CAS 场景由
    /// `concurrent_admissions_cannot_overshoot_detached_valve` 的确定性顺序
    /// 放行与 hammer 的并发准入覆盖。
    #[test]
    fn parked_holder_rejects_contenders_then_admits_one() {
        let counters = scoped();
        // holder 先占住唯一的槽
        let (hold_tx, hold_rx) = std::sync::mpsc::channel::<()>();
        let (holder_done_tx, holder_done_rx) = std::sync::mpsc::channel();
        let permits_holder = Arc::clone(&counters.0);
        let detached_holder = Arc::clone(&counters.1);
        std::thread::spawn(move || {
            let r = run_bounded_scoped(
                &permits_holder,
                &detached_holder,
                1,
                6,
                "slot-holder",
                Duration::from_secs(60),
                move || {
                    let _ = hold_rx.recv();
                    1u32
                },
            );
            let _ = holder_done_tx.send(r);
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while permits_of(&counters) < 1 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(permits_of(&counters), 1, "holder 必须先占住唯一槽位");

        // 3 个竞争者：f 各自挂自己的 channel；**逐个释放逐个收集**（终验 Minor-1
        // 方向）——holder 全程保持占位，竞争者对被占槽位必然 Busy（零时序假设），
        // holder 最后释放收其 Ok。
        for contender_index in 0..3 {
            let (contender_hold_tx, contender_hold_rx) = std::sync::mpsc::channel::<()>();
            let (result_tx, result_rx) = std::sync::mpsc::channel();
            let permits_c = Arc::clone(&counters.0);
            let detached_c = Arc::clone(&counters.1);
            std::thread::spawn(move || {
                let r = run_bounded_scoped(
                    &permits_c,
                    &detached_c,
                    1,
                    6,
                    "contender",
                    Duration::from_secs(60),
                    move || {
                        let _ = contender_hold_rx.recv();
                        1u32
                    },
                );
                let _ = result_tx.send(r);
            });
            assert_eq!(
                permits_of(&counters),
                1,
                "竞争者 {contender_index} 尝试前后 holder 都必须占住唯一槽位"
            );
            // 释放该竞争者：唯一槽位仍被 holder 占住 → 必须 Busy
            drop(contender_hold_tx);
            let r = result_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("竞争者按时收货");
            assert!(
                matches!(r, Err(WalkError::Busy)),
                "竞争者 {contender_index} 必须 Busy（holder 占位），实际 {r:?}"
            );
            assert_eq!(
                permits_of(&counters),
                1,
                "Busy 竞争者不得影响 holder 的占位（{contender_index}）"
            );
        }
        // 全部竞争者 Busy 收齐后，释放 holder：恰好 1 Ok
        drop(hold_tx);
        let r = holder_done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("holder 放行后必须按时收货");
        assert!(r.is_ok(), "holder 释放后必须正常放行：{r:?}");
        assert!(wait_until_drained(&counters, Duration::from_secs(5)));
    }

    /// detached 线程资源阀门（终验六模型）：楔死线程（超时释放许可后未自愈的
    /// detached 线程）堆积达阀门上限 → 新准入 Busy（纯资源阀门，不参与准入
    /// 语义）；自愈完成后阀门重新开放。
    #[test]
    fn detached_thread_valve_rejects_when_wedged_threads_pile_up() {
        let counters = scoped();
        let max_detached = 2usize;
        // 两个楔死 walk：进入即发 entered 信号、随后阻塞在显式释放信号上（终验
        // Minor-2 方向——零睡眠窗口依赖：状态探针由握手与预算到期驱动，不靠
        // 睡眠时长维持测试状态）。
        let mut entered_rxs = Vec::new();
        let mut release_txs = Vec::new();
        let mut results = Vec::new();
        let mut joins = Vec::new();
        for i in 0..2 {
            let permits_for_walk = Arc::clone(&counters.0);
            let detached_for_walk = Arc::clone(&counters.1);
            let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let (result_tx, result_rx) = std::sync::mpsc::channel();
            entered_rxs.push(entered_rx);
            release_txs.push(release_tx);
            results.push(result_rx);
            joins.push(std::thread::spawn(move || {
                let r = run_bounded_scoped(
                    &permits_for_walk,
                    &detached_for_walk,
                    8,
                    max_detached,
                    "wedged",
                    Duration::from_millis(200),
                    move || {
                        let _ = entered_tx.send(());
                        let _ = release_rx.recv();
                        i
                    },
                );
                let _ = result_tx.send(r);
            }));
        }
        // ① 双进入确认：walker 各自发 entered（detached 槽位已预留且线程在跑）
        for rx in &entered_rxs {
            rx.recv_timeout(Duration::from_secs(5))
                .expect("walker 必须按时进入");
        }
        // ② 双超时确认：预算 200ms 到点，许可被超时方释放归零（walker 仍阻塞在释放信号上）
        let deadline = Instant::now() + Duration::from_secs(5);
        while permits_of(&counters) > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(permits_of(&counters), 0, "许可已被两个超时方释放");
        assert_eq!(detached_of(&counters), 2, "两个 walker 阻塞占用资源槽");
        // ③ 阀门：资源满 → 新准入 Busy（尽管许可额度充足）
        let r = run_bounded_scoped(
            &counters.0,
            &counters.1,
            8,
            max_detached,
            "valve-probe",
            Duration::from_millis(200),
            || panic!("资源阀门满的准入绝不允许 spawn"),
        );
        assert!(
            matches!(r, Err(WalkError::Busy)),
            "资源阀门必须 Busy，实际 {r:?}"
        );
        // ④ 显式释放两个 walker：结果必须都是超时降级
        for tx in &release_txs {
            let _ = tx.send(());
        }
        for rx in &results {
            let r = rx
                .recv_timeout(Duration::from_secs(5))
                .expect("楔死 walk 释放后必须按时收货");
            assert!(
                matches!(r, Err(WalkError::TimedOut(_))),
                "楔死 walk 必须超时降级，实际 {r:?}"
            );
        }
        for join in joins {
            join.join().expect("楔死 walk 线程必须正常退出");
        }
        // ⑤ 自愈后阀门重新开放
        assert!(wait_until_drained(&counters, Duration::from_secs(4)));
        let recovered = run_bounded_scoped(
            &counters.0,
            &counters.1,
            8,
            max_detached,
            "post-recover",
            Duration::from_millis(200),
            || 3u32,
        );
        assert_eq!(recovered.unwrap(), 3, "自愈后阀门重新开放");
    }

    /// 合法并发容量（终验六场景锁）：详情页三路只读根解析 + scan + wakeup =
    /// 5 路并发必须全部放行。
    #[test]
    fn five_legitimate_concurrent_walks_all_admitted() {
        let counters = scoped();
        let mut holds = Vec::new();
        let mut results = Vec::new();
        for _ in 0..5 {
            let (result_tx, result_rx) = std::sync::mpsc::channel();
            let (hold_tx, hold_rx) = std::sync::mpsc::channel::<()>();
            holds.push(hold_tx);
            let permits_c = Arc::clone(&counters.0);
            let detached_c = Arc::clone(&counters.1);
            std::thread::spawn(move || {
                let r = run_bounded_scoped(
                    &permits_c,
                    &detached_c,
                    8,
                    6,
                    "detail-page-mount",
                    Duration::from_secs(30),
                    move || {
                        let _ = hold_rx.recv();
                        1u32
                    },
                );
                let _ = result_tx.send(r);
            });
            results.push(result_rx);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while permits_of(&counters) < 5 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(permits_of(&counters), 5, "5 路合法并发必须全部在跑");
        drop(holds);
        for rx in &results {
            let r = rx
                .recv_timeout(Duration::from_secs(10))
                .expect("放行后必须按时收货");
            assert!(r.is_ok(), "5 路合法并发不得被 Busy 误拒，实际 {r:?}");
        }
        assert!(wait_until_drained(&counters, Duration::from_secs(5)));
    }

    #[test]
    fn run_bounded_propagates_panicking_walk_payload() {
        let counters = scoped();
        let result = run_bounded_scoped(
            &counters.0,
            &counters.1,
            4,
            4,
            "panicky",
            Duration::from_millis(500),
            || panic!("遍历内 panic"),
        );
        match result {
            Err(WalkError::Panicked(_)) => {}
            other => panic!("panic 必须以 Panicked 形态回传，实际 {other:?}"),
        }
        assert!(wait_until_drained(&counters, Duration::from_secs(2)));
    }

    /// 隔离锁（Important-4 / Minor-1 终验）：排他性以**事件序**证明。「准备
    /// 释放」事件在持锁期间发出——其后的 ContenderAcquired 不受 holder 解锁
    /// 后调度先后影响；锁缺陷时 contender 提前获锁，事件先到即失败。
    #[test]
    fn walk_test_guard_holds_exclusive_access() {
        #[derive(Debug)]
        enum Event {
            HolderLocked,
            HolderReleasing,
            ContenderAcquired,
        }
        let (events_tx, events_rx) = std::sync::mpsc::channel();
        let events_tx_holder = events_tx.clone();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let holder = std::thread::spawn(move || {
            let guard = walk_test_guard();
            let _ = events_tx_holder.send(Event::HolderLocked);
            let _ = release_rx.recv();
            // **仍在持锁中**发出「准备释放」——此事件之后才解锁
            let _ = events_tx_holder.send(Event::HolderReleasing);
            drop(guard);
        });
        match events_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Event::HolderLocked) => {}
            other => panic!("必须先收到 holder 已持锁事件，实际 {other:?}"),
        }
        let contender = std::thread::spawn(move || {
            let guard = walk_test_guard();
            let _ = events_tx.send(Event::ContenderAcquired);
            drop(guard);
        });
        let _ = release_tx.send(());
        match events_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Event::HolderReleasing) => {}
            Ok(Event::ContenderAcquired) => {
                panic!("竞争者在 holder 持锁期间即获锁 = 排他性缺陷")
            }
            other => panic!("事件流中断，实际 {other:?}"),
        }
        match events_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Event::ContenderAcquired) => {}
            other => panic!("释放后竞争者必须获锁，实际 {other:?}"),
        }
        holder.join().expect("holder 不得 panic");
        contender.join().expect("contender 不得 panic");
    }

    /// NEEDS_FIX Important 终验收口回归（确定性）：**detached 槽位的检查与
    /// 预留必须同一原子线性化点**。场景：楔死线程占 5/6 槽（余 1），同时放行
    /// 2 个新请求——旧实现「先读计数后 spawn、线程入口才 +1」的分离式检查下
    /// 两个请求都通过预检查并 spawn（存活峰值 7 > 阀门 6，越界）；新实现
    /// spawn 前原子预留（CAS 满则 Busy），恰好 1 放行 + 1 Busy，存活峰值 ≤ 阀门。
    #[test]
    fn concurrent_admissions_cannot_overshoot_detached_valve() {
        let counters = scoped();
        let max_detached = 6usize;
        // 5 个楔死线程：超时释放许可、walker 进入即发 entered、随后阻塞在显式
        // 释放信号上（终验 Minor 收口——零睡眠窗口依赖：资源槽占用由握手维持，
        // 直到主线程显式放行）
        let mut wedge_joins = Vec::new();
        let mut wedge_entered_rxs = Vec::new();
        let mut wedge_release_txs = Vec::new();
        for i in 0..5 {
            let permits_for_walk = Arc::clone(&counters.0);
            let detached_for_walk = Arc::clone(&counters.1);
            let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            wedge_entered_rxs.push(entered_rx);
            wedge_release_txs.push(release_tx);
            wedge_joins.push(std::thread::spawn(move || {
                let r = run_bounded_scoped(
                    &permits_for_walk,
                    &detached_for_walk,
                    8,
                    max_detached,
                    "wedge",
                    Duration::from_millis(200),
                    move || {
                        let _ = entered_tx.send(());
                        let _ = release_rx.recv();
                        i
                    },
                );
                assert!(matches!(r, Err(WalkError::TimedOut(_))));
            }));
        }
        // 握手①：等 5 个楔死 walker 全部进入（detached = 5）
        for rx in &wedge_entered_rxs {
            rx.recv_timeout(Duration::from_secs(5))
                .expect("楔死 walker 必须按时进入");
        }
        assert_eq!(detached_of(&counters), 5, "5 个楔死线程必须全部入槽");
        // 握手②：200ms 预算超时，许可被超时方释放（余 1 槽在资源面）
        let timeout_deadline = Instant::now() + Duration::from_secs(5);
        while permits_of(&counters) > 0 && Instant::now() < timeout_deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            permits_of(&counters),
            0,
            "许可已被超时方释放（余 1 槽在资源面）"
        );

        // 确定性顺序放行（终验 Minor-1 方向，零调度假设）：先 A 后 B——
        // A 占住余下唯一资源槽（park 挂起），B 的准入看到槽满必然 Busy
        // （先收 B 的 Busy 再放 A），杜绝「胜者先释放→迟到者正常成功」窗口。
        // A 的占位确认 = walker 发 entered（终验 Minor 收口——permits==1 不证明
        // A 已预留 detached 槽并进入；entered 由 walker 本体在槽位预留后发出）。
        let mut holds = Vec::new();
        let mut results = Vec::new();
        let mut starts = Vec::new();
        let mut entered_rxs = Vec::new();
        for _ in 0..2 {
            let (contender_hold_tx, contender_hold_rx) = std::sync::mpsc::channel::<()>();
            let (start_tx, start_rx) = std::sync::mpsc::channel::<()>();
            let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
            let (result_tx, result_rx) = std::sync::mpsc::channel();
            holds.push(contender_hold_tx);
            starts.push(start_tx);
            results.push(result_rx);
            entered_rxs.push(entered_rx);
            let permits_c = Arc::clone(&counters.0);
            let detached_c = Arc::clone(&counters.1);
            std::thread::spawn(move || {
                let _ = start_rx.recv();
                let r = run_bounded_scoped(
                    &permits_c,
                    &detached_c,
                    8,
                    max_detached,
                    "contender",
                    Duration::from_secs(60),
                    move || {
                        let _ = entered_tx.send(());
                        let _ = contender_hold_rx.recv();
                        1u32
                    },
                );
                let _ = result_tx.send(r);
            });
        }
        // A：占住余下唯一资源槽（walker 发 entered = 已预留槽位且已进入遍历）
        starts[0].send(()).expect("A 启动");
        entered_rxs[0]
            .recv_timeout(Duration::from_secs(5))
            .expect("A 的 walker 必须按时进入并占位");
        assert_eq!(permits_of(&counters), 1, "A 必须占住余下唯一资源槽");
        // B：槽被 A 的 park 占住 → 准入 Busy（先收齐 B 的 Busy，A 仍挂起）
        starts[1].send(()).expect("B 启动");
        let r_b = results[1]
            .recv_timeout(Duration::from_secs(10))
            .expect("B 按时收货");
        assert!(
            matches!(r_b, Err(WalkError::Busy)),
            "槽满时 B 必须 Busy（5 楔死 + A 已占满 6/6 detached 槽），实际 {r_b:?}"
        );
        // 释放 A：收其 Ok；B 已 Busy 收齐在前，无迟到成功窗口
        drop(holds);
        let r_a = results[0]
            .recv_timeout(Duration::from_secs(10))
            .expect("A 按时收货");
        assert!(r_a.is_ok(), "A 释放后必须正常放行：{r_a:?}");
        // 收尾：显式放行 5 个楔死 walker（它们的许可早已被超时方释放，此处只
        // 退 detached 槽位）、全部计数归零（存活峰值 ≤ 阀门由采样压力回归覆盖）
        drop(wedge_release_txs);
        for join in wedge_joins {
            join.join().expect("楔死线程不得 panic");
        }
        assert!(wait_until_drained(&counters, Duration::from_secs(5)));
    }

    /// 准入与结算的混合 hammer（终验六模型迁移）：并发线程循环「admit →    /// 准入与结算的混合 hammer（终验六模型迁移）：并发线程循环「admit →
    /// settle（超时/完成由内核 settle-once 裁决）」，Busy 为合法拒绝（不计
    /// 失败）；终值精确回收 + 完整性配对（admitted + busy == 总轮数）。
    #[test]
    fn packed_admission_settlement_hammer_keeps_invariants() {
        let counters = scoped();
        let max_permits = 4usize;
        let max_detached = 6usize;
        const THREADS: usize = 4;
        const ROUNDS: usize = 64;
        let busy_legal = Arc::new(AtomicUsize::new(0));
        let admitted = Arc::new(AtomicUsize::new(0));
        let mut joins = Vec::new();
        for _ in 0..THREADS {
            let permits_for_thread = Arc::clone(&counters.0);
            let detached_for_thread = Arc::clone(&counters.1);
            let busy_for_thread = Arc::clone(&busy_legal);
            let admitted_for_thread = Arc::clone(&admitted);
            joins.push(std::thread::spawn(move || {
                // 线程私有计数器对（主线程的 counters 借用不可跨线程——按值
                // move 进线程体）
                let counters = (
                    Arc::clone(&permits_for_thread),
                    Arc::clone(&detached_for_thread),
                );
                for _round in 0..ROUNDS {
                    match run_bounded_scoped(
                        &counters.0,
                        &counters.1,
                        max_permits,
                        max_detached,
                        "hammer",
                        Duration::from_secs(60),
                        || 1u32,
                    ) {
                        Ok(_) => {
                            admitted_for_thread.fetch_add(1, Ordering::SeqCst);
                        }
                        Err(WalkError::Busy) => {
                            // 合法拒绝：不计失败、不计违规
                            busy_for_thread.fetch_add(1, Ordering::SeqCst);
                        }
                        Err(other) => panic!("hammer 非 Busy 失败：{other:?}"),
                    }
                    // 不变式采样（每轮后）：许可 ≤ 容量（终值断言另含清零）
                    assert!(
                        permits_of(&counters) <= max_permits,
                        "许可越界：{}",
                        permits_of(&counters)
                    );
                }
            }));
        }
        for join in joins {
            join.join().expect("hammer 线程不得 panic");
        }
        assert_eq!(permits_of(&counters), 0, "终值：许可必须清零");
        assert_eq!(
            admitted.load(Ordering::SeqCst) + busy_legal.load(Ordering::SeqCst),
            THREADS * ROUNDS,
            "每轮必须恰好落入「放行」或「合法 Busy」之一"
        );
    }
}
