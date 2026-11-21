//! 执行器：把「幂等回放 → 预检 → 注入前重读 → 计划 → 原子 claim → 共享窗口复核 →
//! 最多一次 Invoke 尝试或零 Invoke 结论 → 有界回读 → 持久 finish」编排成单条路径。
//!
//! 守卫顺序（任一失败立即返回，其后步骤一律不执行）：
//! 1. 仓储幂等回放（零 UI，且**优先于租约时效与并发许可**：重复请求无论租约是否
//!    已过期都返回原结果）：`Finished` 且完整 command 相等 → 直接返回原 receipt；
//!    `Claimed` 且完整 command 相等 → 不重放 Invoke，落 `Unresolved` 回执；
//!    两种状态下 payload 不同 → `ActionConflict`；`lookup` 错误原样传播。
//! 2. 并发闸：调用方必须持有绑定目标线程（`proof.group.thread_id`）的答题同线程
//!    dispatch 许可；生产接入现有 router 线程锁，此处不另造锁。
//! 3. 绑定/租约预检（无 UI，判据与 `plan_close` 一致）：lease 非空且组身份完整相等；
//!    entry 时刻未过租约精确边界（`now >= expires_at` 视为已过期）。
//! 4. 来源预检（无 UI）：来源完整且成员恰一题。
//! 5. 注入前重读：`UiAutomation::snapshot` 取实测快照（读失败 → `NativeUnavailable`）。
//! 6. `plan_close` 定计划（纯函数，无副作用）：`Refuse` 原样拒绝（零 claim 零
//!    Invoke）；其余收敛为「待执行目标」或「零 Invoke 的已核对未呈现结论」。
//! 7. claim 前窗口复核 + 原子消费租约：重新读 now（过期 → `LeaseExpired`）与 permit
//!    （失效 → `ConcurrentAction`），均为零副作用的可重试形态错误（尚未 claim）；
//!    `consume_and_claim` 成功表示「签发租约校验 + 单次消费 + claim 完整 payload」
//!    已在持久层原子提交，此后才允许任何原生操作。
//! 8. claim 后窗口复核（claim/持久事务可能耗时；NotPresent 与 Invoke 两分支共享）：
//!    now 过期或 permit 失效都不得继续任何原生操作，但已消费的 claim 不可重放 →
//!    统一持久化 `Unresolved` 回执（非可重试错误）。
//! 9. 效果执行（**最多一次 Invoke 尝试**；拒绝/claim 后失效为零次，外部副作用是否
//!    发生不可证实，不承诺 exactly-once）：只调用 planner 实测产出的目标；失败记
//!    `Unresolved` 回执，不重试、租约不退回。
//! 10. 有界回读：预算内每次 `clock.tick()` 后重读；「完整快照 + scope 逐字段一致 +
//!     卡列表为空」才 `ClosedVerified`；读错 / scope 变 / 不完整立即保守 `Unresolved`
//!     （不继续消耗预算，防止把其他同题 runtime 替换或未知变化误判为已关闭）；
//!     仍在场则消耗预算重读，预算尽 `Unresolved`。
//! 11. `finish` 持久回执：失败如实传播（如 `StoreUnavailable`），不伪成功。
//!
//! claim 提交之后的所有路径都收敛为持久化回执：租约绝不退回，Invoke 绝不自动重试，
//! 也不再返回 `LeaseExpired` / `LeaseConsumed` 等可重试形态的错误。
//!
//! 接口限制（后续 API/Task1 注意）：
//! - `SkipReceipt` 只携带内部效果三元组 `{display, native_effect, reason}`；`action_id`
//!   与 group 身份保存在 `ActionState` 存档的完整 command 里，HTTP/API 层回执必须由
//!   外层补齐 actionId / group 回显，不得因内部简化丢失身份。
//! - 本核心是同步接口，无法取消在途的 `UiAutomation` 调用，也不保证其墙钟上界；
//!   真实 UIA 的外层超时 / 取消 / 迟到操作守卫归 Task1（届时 claim 已消费的语义
//!   保持不变：迟到完成也只能落 `Unresolved` 回执或如实传播 finish 错误）。

use super::planner::plan_close;
use super::types::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SkipCommand {
    pub(crate) action_id: String,
    pub(crate) selector: GroupSelector,
    pub(crate) proof: ThreadBindingProof,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ActionState {
    Claimed(SkipCommand),
    Finished(SkipCommand, SkipReceipt),
}

pub(crate) trait Clock {
    fn now_ms(&self) -> u64;
    /// 可注入的有界等待；生产实现负责等待，测试不得真实 sleep。
    fn tick(&mut self);
}

/// 由调用方在已持有答题同线程 dispatch 锁时提供；实现不得另外造锁冒充许可。
pub(crate) trait DispatchPermit {
    fn permits(&self, thread_id: &str) -> bool;
}

pub(crate) trait ActionRepository {
    fn lookup(&mut self, action_id: &str) -> Result<Option<ActionState>, SkipReason>;
    /// 必须在一个持久事务中校验并消费单次租约、claim 带完整 payload 的 action；
    /// 返回成功表示提交已落盘，之后才允许 Invoke。租约绝不因 Invoke 失败退回。
    fn consume_and_claim(&mut self, command: &SkipCommand, now_ms: u64) -> Result<(), SkipReason>;
    fn finish(&mut self, action_id: &str, receipt: &SkipReceipt) -> Result<(), SkipReason>;
}

/// 执行入口：只做编排，不做任何 UI / 持久化的具体实现（守卫顺序见模块文档）。
pub(crate) fn execute(
    command: &SkipCommand,
    permit: &dyn DispatchPermit,
    clock: &mut dyn Clock,
    ui: &mut dyn UiAutomation,
    repository: &mut dyn ActionRepository,
    readback_budget: usize,
) -> Result<SkipReceipt, SkipReason> {
    // 1. 仓储幂等回放：已存在同 id 的 action 时不再触碰 UI（优先于租约/许可检查）。
    match repository.lookup(&command.action_id)? {
        Some(ActionState::Finished(prior, receipt)) => {
            return if prior == *command {
                Ok(receipt)
            } else {
                Err(SkipReason::ActionConflict)
            };
        }
        Some(ActionState::Claimed(prior)) => {
            if prior != *command {
                return Err(SkipReason::ActionConflict);
            }
            // 重启后撞见已 claim 的同 payload action：Invoke 是否发生过不可证实，
            // 不重放 Invoke，把 Unresolved 结论按仓储合同持久化（幂等可重入）。
            return persist(repository, &command.action_id, unresolved_receipt());
        }
        None => {}
    }
    // 2. 并发闸：必须持有绑定目标线程的答题同线程 dispatch 许可。
    if !permit.permits(&command.proof.group.thread_id) {
        return Err(SkipReason::ConcurrentAction);
    }
    // 3. 绑定/租约预检：与 plan_close 同判据，提前到任何 UI 探测之前。
    if command.proof.lease_id.is_empty() || command.proof.group != command.selector.group {
        return Err(SkipReason::BindingRequired);
    }
    let entry_now_ms = clock.now_ms();
    if entry_now_ms >= command.proof.expires_at_ms {
        return Err(SkipReason::LeaseExpired);
    }
    // 4. 来源预检：来源未收敛 / 非单题组，不发起任何 UI 探测。
    if !command.selector.source_complete {
        return Err(SkipReason::SourceIncomplete);
    }
    if command.selector.members.len() != 1 {
        return Err(SkipReason::UnsupportedGroup);
    }
    // 5. 注入前重读：以实测快照为准，不信任 caller 转述的现场。
    let snapshot = match ui.snapshot(&command.proof.scope) {
        Ok(snapshot) => snapshot,
        Err(_) => return Err(SkipReason::NativeUnavailable),
    };
    // 6. 计划（纯函数，无副作用）：Refuse 原样拒绝；其余收敛为「待执行目标」
    //    或「零 Invoke 的已核对未呈现结论」，使 claim 与窗口复核两分支共享。
    let planned_target =
        match plan_close(&command.selector, &command.proof, &snapshot, entry_now_ms) {
            ClosePlan::Invoke(target) => Some(target),
            ClosePlan::NotPresentVerified => None,
            ClosePlan::Refuse(reason) => return Err(reason),
        };
    // 7. claim 前窗口复核 + 原子消费租约：成功即代表 claim 已持久提交。
    claim(command, permit, clock, repository)?;
    // 8. claim 后窗口复核（两分支共享）：claim/持久事务可能耗时，重新读时钟与许可；
    //    任一失效都不得继续任何原生操作，但已消费的 claim 不可重放 → 落 Unresolved 回执。
    if clock.now_ms() >= command.proof.expires_at_ms
        || !permit.permits(&command.proof.group.thread_id)
    {
        return persist(repository, &command.action_id, unresolved_receipt());
    }
    // 9. 效果执行：最多一次 Invoke 尝试（拒绝/claim 后失效为零次，外部副作用是否
    //    发生不可证实）；NotPresentVerified 为零 Invoke 结论，直接落回执。
    let receipt = match planned_target {
        None => not_present_receipt(),
        Some(target) => {
            if ui.invoke(&target).is_err() {
                unresolved_receipt()
            } else {
                // 10. 有界回读：预算内证明目标交互容器已从完整连续 scope 中消失。
                verify_closed(clock, ui, &target, readback_budget)
            }
        }
    };
    // 11. 回执持久化：失败如实传播，不伪成功。
    persist(repository, &command.action_id, receipt)
}

/// claim 前重读时钟与许可防 TOCTOU（过期/失许为零副作用的可重试形态错误）；
/// `consume_and_claim` 成功表示单次消费 + claim 已落盘。
fn claim(
    command: &SkipCommand,
    permit: &dyn DispatchPermit,
    clock: &mut dyn Clock,
    repository: &mut dyn ActionRepository,
) -> Result<(), SkipReason> {
    let now_ms = clock.now_ms();
    if now_ms >= command.proof.expires_at_ms {
        return Err(SkipReason::LeaseExpired);
    }
    if !permit.permits(&command.proof.group.thread_id) {
        return Err(SkipReason::ConcurrentAction);
    }
    repository.consume_and_claim(command, now_ms)
}

/// claim 之后唯一的出口：回执必须持久化，失败如实传播（如 `StoreUnavailable`），
/// 不伪成功、不重试、不把租约退回。
fn persist(
    repository: &mut dyn ActionRepository,
    action_id: &str,
    receipt: SkipReceipt,
) -> Result<SkipReceipt, SkipReason> {
    repository.finish(action_id, &receipt)?;
    Ok(receipt)
}

/// Invoke 后结论不可证实的统一回执：展示侧跳过，原生效未证实。
fn unresolved_receipt() -> SkipReceipt {
    SkipReceipt {
        display: DisplayDecision::Skipped,
        native_effect: NativeEffect::Unresolved,
        reason: SkipReason::InvokedUnconfirmed,
    }
}

/// plan_close 证实「完整同 scope 已核对未呈现」的回执：零 Invoke，展示侧跳过。
fn not_present_receipt() -> SkipReceipt {
    SkipReceipt {
        display: DisplayDecision::Skipped,
        native_effect: NativeEffect::NotPresentVerified,
        reason: SkipReason::None,
    }
}

/// 回读证实目标交互容器已消失的回执。
fn closed_receipt() -> SkipReceipt {
    SkipReceipt {
        display: DisplayDecision::Skipped,
        native_effect: NativeEffect::ClosedVerified,
        reason: SkipReason::None,
    }
}

/// 有界回读：只认「完整快照 + scope（含后端 scope_epoch）逐字段一致 + 卡列表为空」
/// 为 `ClosedVerified`。核心不自证在场卡是否为同题替换（不做指纹比对）——替换/未知卡
/// 只表现为非空卡列表，一律不当作已关闭；同题替换的同一性证据由后端 scope/epoch
/// 完整性承担。读错 / scope 漂移 / 不完整立即保守 `Unresolved`（不继续重读）；
/// 仍在场则消耗预算重读，预算尽 `Unresolved`。
fn verify_closed(
    clock: &mut dyn Clock,
    ui: &mut dyn UiAutomation,
    target: &InvokeTarget,
    readback_budget: usize,
) -> SkipReceipt {
    for _ in 0..readback_budget {
        // 注入的有界等待：invoke 后与重读间隔让 TUI 重绘；测试实现只推进虚拟时钟。
        clock.tick();
        match ui.snapshot(&target.scope) {
            Err(_) => break,
            Ok(snapshot) => {
                if snapshot.scope != target.scope {
                    // scope 代际/窗口/进程漂移：消失不可信，立即保守 Unresolved。
                    break;
                }
                if !snapshot.complete {
                    // 不完整快照既不能证实也不能证伪消失，立即保守 Unresolved。
                    break;
                }
                if snapshot.cards.is_empty() {
                    // 完整连续同 scope 且卡列表为空：目标交互容器已消失。
                    return closed_receipt();
                }
                // 仍在场（含其他同题 runtime 替换/未知卡）：核心不自证替换同一性
                //（同一性证据由后端 scope/epoch 承担），非空卡列表一律不当作已关闭，
                // 消耗预算重读。
            }
        }
    }
    unresolved_receipt()
}
