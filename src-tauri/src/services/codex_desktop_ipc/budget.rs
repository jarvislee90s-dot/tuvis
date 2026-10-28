//! S3 统一预算（设计 §5）：前台编排 30 秒、后台主动收尾 120 秒。
//!
//! * 预算是**绝对截止**（[`Deadline`]）：子步骤（连接、探测、owner/审批复核、提交
//!   回执、必要确认）只能消耗剩余预算，**不能逐层重新获得 30 秒**——结构上由
//!   「同一个绝对时刻向下传递」保证，`child()` 只做继承不做重置。
//! * 两端单调时钟、不假设同步（设计 §5.1）：前台到期不证明后端已停，可能已写出时
//!   上层必须呈现「待确认」，绝不记 NotSent（超时 ≠ 未送达）。
//!
//! **Clock 注入点（受控时间纪律，登记见总计划 S3 条目）**：预算读取 tokio 单调时间
//! 驱动（`tokio::time::Instant`）；测试等价物 = 仓库既有注参模式
//! （`set_reconcile_period_ms` / `set_decision_confirm_ms` 同款）——hub `set_front_budget_ms`
//! / router `set_wrap_up_ms` / adapter `set_conn_tuning` 把预算常量收窄到测试量级，
//! 红绿测试一律有界等待（`tokio::time::timeout`），禁止真实长 sleep 镜像生产时值。

use std::future::Future;
use std::time::Duration;

/// 前台编排预算（毫秒）：自服务端接收发送请求起，连接＋探测＋复核＋回执＋确认全部
/// 在此窗口内给前台一个明确结果（含「待确认」）。
pub const FRONT_BUDGET_MS: u64 = 30_000;

/// 后台主动收尾预算（毫秒）：自**实际派发编排开始**起算（排队驻留不占），到点即
/// 有界收尾——保留持久动作，不自动重发、不永久占用 RPC/锁/轮询（设计 §5.3）。
pub const WRAP_UP_BUDGET_MS: u64 = 120_000;

/// 收尾窗口内等迟到回执的轮询间隔（毫秒）：有界、短周期；槽位由 `LateSlotRegistry`
/// 持有，轮询只查不占锁。
pub const LATE_POLL_INTERVAL_MS: u64 = 100;

/// S4 单次只读核对预算（毫秒，设计 §6.1「首版单次用户核对窗口上限 10 秒，可提前
/// 返回」）：核对内部子步骤（ensure 新鲜快照、迟到收敛）只能消耗剩余预算；到期
/// 如实按当前已得证据返回（「没找到」≠「没发送」）。
pub const VERIFY_BUDGET_MS: u64 = 10_000;

/// 预算到期信号（`Deadline::within` 的 Err 臂）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetElapsed;

/// 绝对截止时刻。`None` = 不设预算（后台非编排调用、既有路径）。
///
/// 复制即共享语义（Copy）：向下传递保持同一绝对时刻——「子步骤只能用剩余预算」
/// 由类型形状结构化成立，调用方无法意外重置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deadline(Option<tokio::time::Instant>);

impl Deadline {
    /// 自现在起 `ms` 毫秒的绝对截止。
    pub fn after_ms(ms: u64) -> Self {
        Self(Some(
            tokio::time::Instant::now() + Duration::from_millis(ms),
        ))
    }

    /// 不设预算（既有路径：flush 接力等待者、测试直调等）。
    pub fn none() -> Self {
        Self(None)
    }

    /// 剩余预算；`None` = 无预算（不限时）。
    pub fn remaining(&self) -> Option<Duration> {
        self.0
            .map(|d| d.saturating_duration_since(tokio::time::Instant::now()))
    }

    /// 预算是否已到期（无预算恒 false）。
    pub fn is_expired(&self) -> bool {
        self.0.is_some_and(|d| tokio::time::Instant::now() >= d)
    }

    /// 底层绝对时刻（交给连接/RPC 等待层用；`None` = 不设预算）。
    pub fn instant(&self) -> Option<tokio::time::Instant> {
        self.0
    }

    /// 子步骤继承同一绝对截止（**不重置**——「串联不重置预算」的结构化保证）。
    pub fn child(&self) -> Self {
        *self
    }

    /// 在预算内等待 `fut`：到期返回 [`BudgetElapsed`]，**不取消已 spawn 的任务**——
    /// 调用方负责「到期后动作仍由派发任务有界收尾」（等待者与动作解耦，设计 §5.3）。
    pub async fn within<F: Future>(&self, fut: F) -> Result<F::Output, BudgetElapsed> {
        match self.0 {
            Some(d) => tokio::time::timeout_at(d, fut)
                .await
                .map_err(|_| BudgetElapsed),
            None => Ok(fut.await),
        }
    }

    /// 预算内等待的上限时长（无预算返回 `None`——调用方自行决定缺省上限）。
    pub fn cap(&self, fallback: Duration) -> Duration {
        self.remaining().map_or(fallback, |r| r.min(fallback))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 子步骤继承同一绝对截止：child 不重置（重置 = 剩余回到 50ms 量级）。
    #[tokio::test]
    async fn child_shares_absolute_deadline() {
        let d = Deadline::after_ms(50);
        tokio::time::sleep(Duration::from_millis(20)).await;
        let c = d.child();
        let rem = d.remaining().unwrap();
        assert!(rem <= Duration::from_millis(31));
        assert!(
            c.remaining().unwrap() <= rem,
            "child 与父同一绝对截止（单调钟两次读数只允许微小漂移）"
        );
        assert!(
            c.remaining().unwrap() < Duration::from_millis(40),
            "child 不得重新计时（重置会回到 ~50ms）"
        );
    }

    /// within：到期返回 BudgetElapsed；无预算不限时。
    #[tokio::test]
    async fn within_bounds_wait() {
        let d = Deadline::after_ms(30);
        let r = d.within(tokio::time::sleep(Duration::from_secs(5))).await;
        assert_eq!(r, Err(BudgetElapsed));
        assert!(Deadline::none().within(async { 1u8 }).await == Ok(1));
    }
}
