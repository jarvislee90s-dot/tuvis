//! S3 迟到结果槽位（设计 §5.3）：RPC 超时/等待者丢失**不得删除全部 pending 关联**——
//! 已可能投递的 mutation 必须保留可关联的迟到结果槽位。
//!
//! 契约（控制会话裁决 #3）：
//! * **绑定**：原 envelope（主键）、连接代际（epoch）、目标 owner clientId、方法名、
//!   actionId——迟到回执只能归属这条原尝试；
//! * **限量**：容量上限内才放行 mutation 写出（`try_reserve`，写前调用）；**容量不足
//!   在投递前明确拒绝**（`false` → 调用方按零投递拒绝，绝不「投递后失去跟踪」）；
//! * **到期清理**：TTL 过期的未成熟槽位在下次访问时惰性清扫（有界资源，不泄漏）；
//! * **安全收尾**：连接退出时未成熟的槽位释放（死连接不会再有回执）；已捕获证据的
//!   槽位保留到被消费或 TTL 到期——证据不随连接死亡丢失；
//! * **同一证据只收敛一次**：`fill` 只在槽位无结论时成功；`take_*` 即消费（取走即删）。
//!
//! 锁纪律：`std::sync::Mutex` 短临界区、闭包内无 await、不嵌套取其他锁——与
//! connection 的 pending 锁（tokio）之间不存在相互等待，无死锁面。
//!
//! **旧路径对照（设计纪律：解释旧行为 + 新正反例替换，不能只删）**：旧实现
//! （connection.rs `request_inner` 超时臂）超时即 `pending.remove()`，迟到响应到达时
//! `handle_response` 查无此条**静默丢弃**——动作永远停在 unknown（「超时移除 pending
//! 以后再也无法识别迟到结果」的盲区，设计 §5.3 点名废除）。新正例 = 槽位持有 +
//! `fill` + 消费归账（`tests` 与集成红绿用例钉死）；旧「超时后丢弃迟到响应」由
//! `fill` 侧反例（无槽位/已消费 → 丢弃不变）保留为显式语义。

use super::types::Delivery;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

/// 迟到槽位默认容量（每适配器全连接共享；容量不足写前拒绝，理由码
/// `late_slot_capacity`）。
pub const DEFAULT_LATE_CAPACITY: usize = 32;

/// 迟到槽位默认 TTL：超时未取得回执的槽位按创建时刻 + TTL 惰性清理。
pub const DEFAULT_LATE_TTL: Duration = Duration::from_secs(10 * 60);

/// 槽位捕获的迟到结论（handle_response 的同构产物；envelope 在注册表键上）。
#[derive(Debug, Clone, PartialEq)]
pub struct LateOutcome {
    pub delivery: Delivery,
    pub result: Option<Value>,
    pub handled_by: Option<String>,
}

/// 可消费的迟到结果（绑定四元组 + 结论 + 派发前 Turn 集合快照）。
#[derive(Debug, Clone, PartialEq)]
pub struct LateResult {
    pub envelope_id: String,
    pub epoch: u64,
    pub target_client_id: Option<String>,
    pub method: String,
    pub action_id: Option<String>,
    pub outcome: LateOutcome,
    /// 终审 F10：**派发前**已存在的 Turn id 集合（预留时随四元组一起存）——
    /// 迟到回执的 stale-turn 判定必须比对这份快照，而非当前镜像：新 Turn 先广播、
    /// 正式 userMessage 延后（P1 实测 +0.5s 常态时序）时，当前镜像已含新 Turn，
    /// 比对当前集合会把有效回执误判成旧 Turn 回显。
    pub pre_dispatch_turns: std::collections::HashSet<String>,
}

#[derive(Debug)]
struct LateSlot {
    epoch: u64,
    target_client_id: Option<String>,
    method: String,
    action_id: Option<String>,
    pre_dispatch_turns: std::collections::HashSet<String>,
    created: tokio::time::Instant,
    outcome: Option<LateOutcome>,
}

/// 迟到结果槽位注册表（每适配器一份，跨连接共享——连接换代不丢证据；
/// epoch 绑定在槽位上供消费方核对归属）。
pub struct LateSlotRegistry {
    slots: Mutex<HashMap<String, LateSlot>>,
    capacity: usize,
    ttl: Duration,
}

impl Default for LateSlotRegistry {
    fn default() -> Self {
        Self::new(DEFAULT_LATE_CAPACITY, DEFAULT_LATE_TTL)
    }
}

impl LateSlotRegistry {
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
            capacity: capacity.max(1),
            ttl,
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// 惰性清扫过期未成熟槽位（成熟证据不按 TTL 丢——等消费方或进程收尾）。
    fn sweep(&self, slots: &mut HashMap<String, LateSlot>) {
        let now = tokio::time::Instant::now();
        slots.retain(|_, s| s.outcome.is_some() || now.duration_since(s.created) < self.ttl);
    }

    /// 写前预留（mutation 专用）：容量不足返回 `false`（调用方**写前明确拒绝**）。
    /// `pre_dispatch_turns` = 派发前已存在的 Turn id 集合（终审 F10：stale-turn
    /// 判定的比对基准快照，预留时与四元组一起冻结）。
    #[allow(clippy::too_many_arguments)]
    pub fn try_reserve(
        &self,
        envelope_id: &str,
        epoch: u64,
        target_client_id: Option<&str>,
        method: &str,
        action_id: Option<&str>,
        pre_dispatch_turns: std::collections::HashSet<String>,
    ) -> bool {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        self.sweep(&mut slots);
        if slots.len() >= self.capacity {
            return false;
        }
        slots.insert(
            envelope_id.to_string(),
            LateSlot {
                epoch,
                target_client_id: target_client_id.map(str::to_string),
                method: method.to_string(),
                action_id: action_id.map(str::to_string),
                pre_dispatch_turns,
                created: tokio::time::Instant::now(),
                outcome: None,
            },
        );
        true
    }

    /// 释放预留：仅在**尚无结论**时移除（已捕获证据的槽位保留给消费方）。
    /// 返回是否确实移除了未成熟槽位。
    pub fn release(&self, envelope_id: &str) -> bool {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        match slots.get(envelope_id) {
            Some(s) if s.outcome.is_none() => {
                slots.remove(envelope_id);
                true
            }
            _ => false,
        }
    }

    /// 捕获迟到结论：槽位在场且**尚无结论**才成功（同一证据只收敛一次）。
    /// 无槽位（未预留/已释放/已消费/已过期）→ `false`，响应照旧丢弃（显式语义）。
    pub fn fill(&self, envelope_id: &str, outcome: LateOutcome) -> bool {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        match slots.get_mut(envelope_id) {
            Some(s) if s.outcome.is_none() => {
                s.outcome = Some(outcome);
                true
            }
            _ => false,
        }
    }

    /// 消费单个槽位的成熟结论（取走即删）。
    pub fn take_result(&self, envelope_id: &str) -> Option<LateResult> {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        // 未成熟（回执未到）→ `None` 且**槽位原样保留**：消费方（收尾任务）按预算
        // 继续轮询；先删再判会让首轮轮询就销毁迟到证据的归属（回归
        // `take_result_keeps_unmatured_slot` 钉死）。
        if slots.get(envelope_id)?.outcome.is_some() {
            let s = slots.remove(envelope_id)?;
            let outcome = s.outcome?;
            return Some(LateResult {
                envelope_id: envelope_id.to_string(),
                epoch: s.epoch,
                target_client_id: s.target_client_id,
                method: s.method,
                action_id: s.action_id,
                outcome,
                pre_dispatch_turns: s.pre_dispatch_turns,
            });
        }
        None
    }

    /// 【Gate B 复审】定向消费：只取**指定 action** 的成熟结论（只读核对的步骤②
    /// 消费口）——异动作的成熟证据原样保留给调度循环对账，核对的 ≤10 秒预算不被
    /// 异动作的回显等待软化（设计 §6.1「核对只针对原目标线程和动作」）。
    /// 未成熟槽位与本动作无关的槽位一律原样保留。
    pub fn take_ready_for(&self, action_id: &str) -> Vec<LateResult> {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        self.sweep(&mut slots);
        let matched: Vec<String> = slots
            .iter()
            .filter(|(_, s)| s.outcome.is_some() && s.action_id.as_deref() == Some(action_id))
            .map(|(k, _)| k.clone())
            .collect();
        let mut ready = Vec::with_capacity(matched.len());
        for env in matched {
            if let Some(s) = slots.remove(&env) {
                if let Some(outcome) = s.outcome {
                    ready.push(LateResult {
                        envelope_id: env,
                        epoch: s.epoch,
                        target_client_id: s.target_client_id,
                        method: s.method,
                        action_id: s.action_id,
                        outcome,
                        pre_dispatch_turns: s.pre_dispatch_turns,
                    });
                }
            }
        }
        ready
    }

    /// 排空全部**成熟**结论（后台对账的兜底消费口；等待者丢失/T 结束后的迟到证据
    /// 由此归账）。未成熟槽位**原样保留**——它们的等待者（收尾任务）仍在轮询。
    pub fn take_ready(&self) -> Vec<LateResult> {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        self.sweep(&mut slots);
        let matured: Vec<String> = slots
            .iter()
            .filter(|(_, s)| s.outcome.is_some())
            .map(|(k, _)| k.clone())
            .collect();
        let mut ready = Vec::with_capacity(matured.len());
        for env in matured {
            if let Some(s) = slots.remove(&env) {
                if let Some(outcome) = s.outcome {
                    ready.push(LateResult {
                        envelope_id: env,
                        epoch: s.epoch,
                        target_client_id: s.target_client_id,
                        method: s.method,
                        action_id: s.action_id,
                        outcome,
                        pre_dispatch_turns: s.pre_dispatch_turns,
                    });
                }
            }
        }
        ready
    }

    /// 当前占用（含已成熟未消费；测试与诊断用）。
    pub fn len(&self) -> usize {
        self.slots.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// 已捕获结论（成熟）的槽位数（测试与诊断用；与 [`Self::len`] 的差 = 未成熟
    /// 预留——迟到回执经连接读循环异步 fill，消费方需等待成熟）。
    pub fn matured_len(&self) -> usize {
        self.slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter(|s| s.outcome.is_some())
            .count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(d: Delivery) -> LateOutcome {
        LateOutcome {
            delivery: d,
            result: None,
            handled_by: None,
        }
    }

    /// 容量不足写前拒绝；释放后可再预留。
    #[test]
    fn capacity_refuses_then_release_allows() {
        let r = LateSlotRegistry::new(2, Duration::from_secs(60));
        assert!(r.try_reserve("e1", 1, Some("owner"), "m", Some("a1"), Default::default()));
        assert!(r.try_reserve("e2", 1, Some("owner"), "m", Some("a2"), Default::default()));
        assert!(
            !r.try_reserve("e3", 1, Some("owner"), "m", Some("a3"), Default::default()),
            "容量满必须写前拒绝"
        );
        assert!(r.release("e1"), "未成熟槽位可释放");
        assert!(r.try_reserve("e3", 1, Some("owner"), "m", Some("a3"), Default::default()));
    }

    /// 同一证据只收敛一次：fill 第二次失败；take 取走即删。
    #[test]
    fn fill_once_and_take_consumes() {
        let r = LateSlotRegistry::new(4, Duration::from_secs(60));
        assert!(r.try_reserve("e1", 3, Some("owner"), "m", Some("a1"), Default::default()));
        assert!(r.fill("e1", outcome(Delivery::Accepted)));
        assert!(
            !r.fill("e1", outcome(Delivery::Rejected)),
            "同一证据只收敛一次"
        );
        let taken = r.take_result("e1").expect("成熟证据可消费");
        assert_eq!(taken.epoch, 3);
        assert_eq!(taken.action_id.as_deref(), Some("a1"));
        assert_eq!(taken.outcome.delivery, Delivery::Accepted);
        assert!(r.take_result("e1").is_none(), "取走即删（只收敛一次）");
        assert!(r.is_empty());
    }

    /// 未成熟槽位经 take_result 查询后必须原样保留（首轮轮询不得销毁迟到证据的
    /// 归属；修回归：曾经 remove 后再 `?`，首轮轮询即丢槽）。
    #[test]
    fn take_result_keeps_unmatured_slot() {
        let r = LateSlotRegistry::new(4, Duration::from_secs(60));
        assert!(r.try_reserve("e1", 1, None, "m", Some("a1"), Default::default()));
        assert!(r.take_result("e1").is_none(), "无结论 = None");
        assert_eq!(r.len(), 1, "槽位必须保留");
        assert!(r.fill("e1", outcome(Delivery::Accepted)));
        assert!(r.take_result("e1").is_some(), "成熟后可消费");
    }

    /// 无槽位的迟到响应被丢弃（显式语义，旧「超时删关联」盲区不复活）。
    #[test]
    fn fill_without_slot_is_dropped() {
        let r = LateSlotRegistry::default();
        assert!(!r.fill("ghost", outcome(Delivery::Accepted)));
    }

    /// release 不丢已捕获证据；TTL 只清未成熟槽位。
    #[tokio::test]
    async fn release_keeps_filled_and_ttl_sweeps_only_pending() {
        let r = LateSlotRegistry::new(4, Duration::from_millis(30));
        assert!(r.try_reserve("kept", 1, None, "m", Some("a"), Default::default()));
        assert!(r.try_reserve("gone", 1, None, "m", Some("b"), Default::default()));
        assert!(r.fill("kept", outcome(Delivery::Accepted)));
        assert!(!r.release("kept"), "已成熟槽位不随 release 丢失");
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(r.take_ready().len(), 1, "成熟证据保留；过期未成熟被清扫");
        assert!(r.is_empty());
    }

    /// 【Gate B 复审】take_ready_for：只取本 action 的成熟结论；异动作成熟证据与
    /// 未成熟槽位原样保留（核对预算不被异动作软化、证据不丢）。
    #[test]
    fn take_ready_for_targets_single_action_and_keeps_others() {
        let r = LateSlotRegistry::new(8, Duration::from_secs(60));
        assert!(r.try_reserve("a", 1, None, "m", Some("act-1"), Default::default()));
        assert!(r.try_reserve("b", 1, None, "m", Some("act-2"), Default::default()));
        assert!(r.try_reserve("c", 1, None, "m", Some("act-1"), Default::default()));
        assert!(r.fill("a", outcome(Delivery::Accepted)));
        assert!(r.fill("b", outcome(Delivery::Accepted)));
        // c 未成熟
        let ready = r.take_ready_for("act-1");
        assert_eq!(ready.len(), 1, "只取 act-1 的成熟槽位");
        assert_eq!(ready[0].envelope_id, "a");
        assert_eq!(r.len(), 2, "异动作成熟 + 本动作未成熟都保留");
        assert!(r.take_result("b").is_some(), "异动作证据留给调度循环");
        assert!(r.take_result("c").is_none(), "未成熟槽位不被定向消费销毁");
    }

    /// take_ready 只出成熟结论。
    #[test]
    fn take_ready_only_matured() {
        let r = LateSlotRegistry::new(4, Duration::from_secs(60));
        assert!(r.try_reserve("a", 1, None, "m", Some("a1"), Default::default()));
        assert!(r.try_reserve("b", 1, None, "m", Some("a2"), Default::default()));
        assert!(r.fill("b", outcome(Delivery::Rejected)));
        let ready = r.take_ready();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].envelope_id, "b");
        assert_eq!(r.len(), 1, "未成熟槽位保留");
    }
}
