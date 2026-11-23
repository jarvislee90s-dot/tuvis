//! H5：原生写动作链**统一端到端预算**与**五段分段计时**。
//!
//! ## 统一预算（`ACTION_E2E_BUDGET_MS`）
//!
//! 覆盖服务端动作链接收（`NativeHub::execute_pinned` 起点，即 HTTP 体解析后的
//! 动作链入口）→ 正式回执（账本终态落定、响应返回）全链。语义（用户裁决逐条）：
//!
//! * **派发前超时** ⇒ 尽快落「确定未发送」可核对结论：受控码
//!   [`OC_DISPATCH_BUDGET_EXHAUSTED`] + `not_sent` 墓碑（碑文
//!   `abandon_reasons::BUDGET_EXHAUSTED`，零写出可核对）；**此后不得迟到取得派发权**
//!   ——claim（授权写出的唯一入口）事务内预算复核，超预算后到达的派发拒绝授权写出。
//! * **派发后丢回执** ⇒ 既有 Unknown 语义（只按原 action ID 只读核对），预算不裁剪
//!   已授权写出的结果面。
//! * 预算拒绝是**测量面事件**：计入 [`ActionTiming::budget_refused`]，供
//!   Ruling H2 的瓶颈定位与预算标定复核。
//!
//! 预算检查点（六处）：① `execute_pinned` 入口（许可等待后）→ ② reserve 前写权链
//! 入口 → ③ dispatch 写权链/复核入口 → ④ **claim 事务内**（权威闸，语义硬保证）—
//! ①-④ 全部在**取得派发权之前**（not_sent 墓碑既有语义）；⑤ **写权后复核**（I2：
//! claim UPDATE 成功后、提交前，同一绝对截止再核——外部写锁对峙/落盘等待把链拖过
//! 截止时事务内回滚，零提交零发送）；⑥ **发包前复核**（I2 fix1：claim 提交完成后、
//! write_native 开始前——提交后跨截止 ⇒ 不发原生请求，账本落 Unknown 族
//! Unconfirmed（碑文 OC_DISPATCH_BUDGET_EXHAUSTED，只按原 action ID 核对）——claim
//! 提交后账本边界按设计不可判定（状态机禁止 dispatching 回退 not_sent），绝不冒充
//! 确定）。
//!
//! ## 五段分段计时（Ruling H2 测量先行）
//!
//! 每笔动作链尝试落一行 `opencode_native_action_timings`（结构化可查询）+
//! 一行 `OC_ACTION_TIMING` 结构化日志。五段口径：
//!
//! | 段 | 字段 | 覆盖 |
//! |---|---|---|
//! | 排队 | `queue_ms` | 动作许可等待 + echo 门单飞等待（`acquire_flight`） |
//! | 归属核对 | `ownership_ms` | 快照构建（R0 链）、三互证前置、复核、echo challenge 本体 |
//! | 数据库等待 | `database_ms` | 账本全部 `store().with`（reserve/claim/finish 等的锁等待+SQL） |
//! | 原生派发 | `dispatch_ms` | `write_native`（POST /interrupt 等） |
//! | 回读 | `readback_ms` | settle 的结果定性面（stop=应答判定；其余=新鲜投影回读） |
//!
//! 五段是**分段观测**而非全链划分（段间存在未计入的间隙，五段之和 ≤ total_ms）。
//!
//! ## I2（2026-11-18）：落账异步化与写权后复核
//!
//! * 计时落账（行写库）**移出回执等待路径**：`finish` 只同步产出结构化日志行
//!   （恒输出 = 丢失窗口兜底），行落库经 `NativeHub::persist_action_timing_row`
//!   异步进行——回执不被计时写库同步阻塞（外部锁对峙窗的病态拉伸不再传导到
//!   回执时延）。
//! * 预算检查点新增 ⑤「写权后复核」（在 actions 的 claim 事务内，写权在握后、
//!   提交前）与 ⑥「发包前复核」（fix1：claim 提交完成后、原生请求开始前）——与
//!   ①-④ 构成三层：④ 取写权前拒（墓碑）、⑤ 提交前回滚（零提交）、⑥ 提交后
//!   兜底零发包（Unknown 族，只按原 action ID 核对）。

/// 统一端到端预算（毫秒）：服务端动作链接收 → 正式回执的硬上限。
///
/// 标定依据（GFix 实测 + 本轮五场景复测，见 task-HE-report）：无争用全链 2–8s；
/// 3 发并发 6.2–7.5s；单发外部 DB 锁 40s 对峙 39.6s。45s = 覆盖全部既有实测合法路径
/// + 余量，同时把风暴/长锁窗的病态拉伸裁成「预算内确定结论」。
pub(crate) const ACTION_E2E_BUDGET_MS: i64 = 45_000;

/// 预算耗尽的受控拒绝码：**只在取得派发权之前产生**（零写出证明）。
/// 常量登记在 [`super::types::codes`]（受控码唯一出处）。
pub use super::types::codes::OC_DISPATCH_BUDGET_EXHAUSTED;

/// 预算耗尽的固定文案（不携带任何环境细节）。
pub(crate) const BUDGET_EXHAUSTED_MESSAGE: &str =
    "action budget exhausted before dispatch authority";

/// 预算耗尽的 `NativeError`。
pub(crate) fn budget_exhausted_error() -> super::types::NativeError {
    super::types::NativeError::new(OC_DISPATCH_BUDGET_EXHAUSTED, BUDGET_EXHAUSTED_MESSAGE)
}

/// 计时段（五段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Segment {
    /// 排队（动作许可等待 + 单飞等待）。
    Queue,
    /// 归属核对（快照/三互证/复核/echo challenge）。
    Ownership,
    /// 数据库等待（账本读写）。
    Database,
    /// 原生派发（写通道 POST）。
    Dispatch,
    /// 回读（结果定性面）。
    Readback,
}

/// 作用域守卫：Drop 时把经过时长累加进所属段。
pub(crate) struct TimingSpan<'a> {
    timing: &'a mut ActionTiming,
    segment: Segment,
    started: std::time::Instant,
}

impl Drop for TimingSpan<'_> {
    fn drop(&mut self) {
        let elapsed = self.started.elapsed().as_millis() as i64;
        self.timing.add(elapsed, self.segment);
    }
}

/// 每笔动作链尝试的分段计时记录器。
///
/// 创建于 `execute_pinned`（服务端动作链接收点）；经 `dispatch`/`settle` 传递；
/// `finish` 时落账（`opencode_native_action_timings` 一行 + 结构化日志一行）。
/// 落账失败只记日志——测量绝不影响动作链结果。
pub(crate) struct ActionTiming {
    action_id: String,
    session_id: String,
    operation: &'static str,
    started: std::time::Instant,
    queue_ms: i64,
    ownership_ms: i64,
    database_ms: i64,
    dispatch_ms: i64,
    readback_ms: i64,
    budget_refused: bool,
    finished: bool,
}

impl ActionTiming {
    pub(crate) fn start(
        action_id: &str,
        session_id: &str,
        operation: &'static str,
    ) -> InstantMarker {
        InstantMarker {
            action_id: action_id.to_string(),
            session_id: session_id.to_string(),
            operation,
            started: std::time::Instant::now(),
        }
    }

    fn add(&mut self, ms: i64, segment: Segment) {
        let slot = match segment {
            Segment::Queue => &mut self.queue_ms,
            Segment::Ownership => &mut self.ownership_ms,
            Segment::Database => &mut self.database_ms,
            Segment::Dispatch => &mut self.dispatch_ms,
            Segment::Readback => &mut self.readback_ms,
        };
        *slot = slot.saturating_add(ms);
    }

    /// 排队段直加（动作许可等待无法用作用域守卫表达：许可守卫需持到链尾）。
    pub(crate) fn add_queue_ms(&mut self, ms: i64) {
        self.add(ms, Segment::Queue);
    }

    /// 任意段直加（闭包内使用记录器时无法用作用域守卫——闭包借用 `&mut self`，
    /// 外层 span 会双重借用；调用方自测起止差分后直加）。
    pub(crate) fn add_ms(&mut self, ms: i64, segment: Segment) {
        self.add(ms.max(0), segment);
    }

    /// echo 门单飞等待直加（门内 `acquire_flight` 累计值的差分）。
    pub(crate) fn add_flight_wait_ms(&mut self, ms: i64) {
        self.add(ms.max(0), Segment::Queue);
    }

    /// 开一个作用域计时段。
    pub(crate) fn span(&mut self, segment: Segment) -> TimingSpan<'_> {
        TimingSpan {
            timing: self,
            segment,
            started: std::time::Instant::now(),
        }
    }

    /// 当前是否已超统一预算（活读 hub 的预算值——测试可注入小预算做确定性截断）。
    pub(crate) fn exceeded(&self, hub: &super::NativeHub) -> bool {
        self.elapsed_ms() >= hub.action_budget_ms()
    }

    /// 标记「派发前预算拒绝」（落账观测位）。
    pub(crate) fn mark_budget_refused(&mut self) {
        self.budget_refused = true;
    }

    pub(crate) fn elapsed_ms(&self) -> i64 {
        self.started.elapsed().as_millis() as i64
    }

    /// 落账：`opencode_native_action_timings` 一行（**异步**，见
    /// `NativeHub::persist_action_timing_row`）+ `OC_ACTION_TIMING` 结构化日志
    ///（**同步**恒输出——异步落账的丢失窗口兜底）。重复调用幂等（只落一次）。
    /// 计时数据完整性：异步行的丢失窗口如实申报 = spawn 到落库之间（进程崩溃等），
    /// 日志行恒在；同 action_id 主键覆盖语义不变。
    pub(crate) fn finish(&mut self, hub: &super::NativeHub) {
        if self.finished {
            return;
        }
        self.finished = true;
        let total_ms = self.elapsed_ms();
        let row = crate::database::dao::opencode_action_timings::ActionTimingRow {
            action_id: self.action_id.clone(),
            session_id: self.session_id.clone(),
            operation: self.operation.to_string(),
            queue_ms: self.queue_ms,
            ownership_ms: self.ownership_ms,
            database_ms: self.database_ms,
            dispatch_ms: self.dispatch_ms,
            readback_ms: self.readback_ms,
            total_ms,
            budget_refused: self.budget_refused,
            created_at: chrono::Utc::now().timestamp_millis(),
        };
        // 结构化日志恒同步输出（兜底在先，落账在后）。
        log::info!(
            "OC_ACTION_TIMING {{\"action_id\":\"{}\",\"session\":\"{}\",\"operation\":\"{}\",\
             \"queue_ms\":{},\"ownership_ms\":{},\"database_ms\":{},\"dispatch_ms\":{},\
             \"readback_ms\":{},\"total_ms\":{},\"budget_refused\":{}}}",
            self.action_id,
            self.session_id,
            self.operation,
            self.queue_ms,
            self.ownership_ms,
            self.database_ms,
            self.dispatch_ms,
            self.readback_ms,
            total_ms,
            self.budget_refused,
        );
        hub.persist_action_timing_row(row);
    }
}

/// I2 仅测试装配：**计时落账闸**——用于证明「回执等待路径不被计时写库同步阻塞」
/// （回执照常返回而计时落账仍停在闸内）。装配侧见
/// `NativeHub::arm_timing_persist_hold_for_tests`；单次消费（`armed` swap），
/// 放行靠 `release` 原子位。类型不门控（生产构建的异步落账任务恒收到 None 直通），
/// 装配字段/入口按测试门控。
pub struct TimingPersistHold {
    armed: std::sync::atomic::AtomicBool,
    /// 落账点已到达（消费方停在闸内，或——异步形态——任务已 spawn 到闸前）。
    pub(crate) started: tokio::sync::Notify,
    /// 放行位。
    pub(crate) release: std::sync::atomic::AtomicBool,
}

impl TimingPersistHold {
    #[cfg(any(test, feature = "opencode-coordinator-tests"))]
    pub(crate) fn new() -> Self {
        Self {
            armed: std::sync::atomic::AtomicBool::new(true),
            started: tokio::sync::Notify::new(),
            release: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

/// 计时落账闸的消费：单次有效；到达即通知 `started`（Notify 允许先发后订），
/// 然后协作式轮询 `release`（异步形态必须 yield 让运行时调度其他任务）。
/// 生产路径恒 None 直通。
pub(crate) async fn pass_timing_persist_hold(hold: Option<std::sync::Arc<TimingPersistHold>>) {
    use std::sync::atomic::Ordering;
    let Some(hold) = hold else { return };
    if !hold.armed.swap(false, Ordering::SeqCst) {
        return;
    }
    hold.started.notify_one();
    while !hold.release.load(Ordering::SeqCst) {
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
}

/// `ActionTiming::start` 的产物：先攒身份，`begin` 才真正建记录器。
///
/// 为什么分两步：`execute_pinned` 在动作许可等待**之前**就要开始计时（排队段覆盖
/// 许可等待），而许可守卫持有跨越整条链——记录器在许可取得后由 `begin` 交给链，
/// 起点时刻保持在接收点。
pub(crate) struct InstantMarker {
    action_id: String,
    session_id: String,
    operation: &'static str,
    started: std::time::Instant,
}

impl InstantMarker {
    pub(crate) fn begin(self) -> ActionTiming {
        ActionTiming {
            action_id: self.action_id,
            session_id: self.session_id,
            operation: self.operation,
            started: self.started,
            queue_ms: 0,
            ownership_ms: 0,
            database_ms: 0,
            dispatch_ms: 0,
            readback_ms: 0,
            budget_refused: false,
            finished: false,
        }
    }
}
