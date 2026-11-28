//! AX 通道的**持久动作账本**：把 `codex_desktop_ax_sender::ActionJournal` 映射到
//! 既有 `ActionStore`（`codex_actions` 表）。
//!
//! # 为什么必须是它，而不是 `InMemoryJournal`
//!
//! 进程重启后内存账本消失。若 AX 动作已经「可能按下但未确认」而账本丢了，
//! 重启后同 `action_id` 再来一次就会被当作全新动作 → **可能重复发送**。
//! 计划 R2 bullet 2 明写「将每请求 `InMemoryJournal` 替换为有原子登记和预副作用
//! 进度的持久账本」。
//!
//! # 语义映射（刻意保守）
//!
//! | `ActionPhase` | `ActionState` | 说明 |
//! | --- | --- | --- |
//! | `Registered` | `Queued` | 已登记，尚未触达编辑区 |
//! | `AboutToFill` | `Reserved` | 即将写编辑器——此后不可再声称"零副作用" |
//! | `Filled` / `AboutToSubmit` / `SubmitAttempted` | `Reserved` | 仍在写路径内，未终态 |
//! | `Verified` | `Accepted` | 只读核对确认送达 |
//! | （任何拒绝且**可能已提交**） | `Unknown` | 结果未知，只能只读核对 |
//! | （任何拒绝且**明确零副作用**） | `Rejected` | 明确未送达 |
//!
//! **注意**：`Reserved → Unknown` 由既有启动恢复逻辑自动完成（进程消失 = 投递
//! 结果未知），这正是我们要的语义。
//!
//! # 不复刻策略
//!
//! 本模块只做"把 AX 的阶段写进账本/读出来"，**不**自行判断能不能发——
//! 目标核证、空稿判据、预算都在状态机里。也不新造账号或请求语义：
//! 设备 ID 来自真实鉴权，`kind` 复用 `ActionKind::Send`，`route` 用
//! `ControlRoute::DesktopAx`。

use std::sync::Arc;

use crate::services::codex_control::queue::StoreError;
use crate::services::codex_control::{ActionContext, ActionKind, ActionStore};
use crate::services::codex_desktop_ax_sender::{
    ActionJournal, ActionPhase, JournalError, TargetKey,
};
use crate::services::codex_desktop_ipc::types::{ActionState, ControlRoute, Delivery, ThreadKey};

/// 把 `ActionPhase` 映射到账本状态。
///
/// 依据"副作用已发生到哪一步"而不是"流程走到哪一步"：
/// 只要可能碰过编辑器就不能停在 `Queued`（那等于宣称零副作用）。
#[cfg_attr(not(test), allow(dead_code))]
fn phase_to_state(phase: ActionPhase) -> ActionState {
    match phase {
        ActionPhase::Registered => ActionState::Queued,
        // AboutToFill 之后、Verified 之前：都在写路径内。
        ActionPhase::AboutToFill
        | ActionPhase::Filled
        | ActionPhase::AboutToSubmit
        | ActionPhase::SubmitAttempted => ActionState::Reserved,
        ActionPhase::Verified => ActionState::Accepted,
    }
}

/// 账本状态 → 状态机阶段（重启后恢复用）。
///
/// `Reserved` 恢复成 `AboutToFill`（最保守的可写阶段）：重启时它可能是
/// "已填入未提交"，恢复成更晚的阶段会误报"可能已提交"；恢复成更早的
/// `Registered` 则会误报"零副作用"。`AboutToFill` 恰好表达"可能已动编辑器、
/// 但不确定是否提交"。
fn state_to_phase(state: ActionState) -> Option<ActionPhase> {
    match state {
        // 未开始写：可安全重来（但路由层仍会按 action_id 去重）。
        ActionState::Queued => Some(ActionPhase::Registered),
        // 写路径内或结果未知：**可能已提交**，绝不能恢复成 `AboutToFill`
        // （那个阶段的 `may_have_submitted()` 是 false，会把"可能已提交"降级成
        // "明确未提交"——复审 Important-1）。
        //
        // `Reserved` 只在"已填字但未按"时可能成立，但我们没有持久化那个区分；
        // 无精确证据时必须选**更保守**的一侧：`AboutToSubmit`。
        ActionState::Reserved | ActionState::Unknown => Some(ActionPhase::AboutToSubmit),
        ActionState::Accepted => Some(ActionPhase::Verified),
        // 明确未送达：零副作用。
        ActionState::Rejected => Some(ActionPhase::Registered),
        // 已撤回：不复活。
        ActionState::Cancelled => None,
    }
}

/// 生产用持久账本。持有 `ActionStore` 的 Arc，与既有 IPC 路径共用同一张表。
pub struct SqliteActionJournal {
    store: Arc<ActionStore>,
    /// 真实鉴权设备（**不是** action_id）。
    device_id: String,
    device_name: String,
    host_id: String,
    /// 本次动作的线程（决定账本 `thread_id` 列）。
    thread_id: String,
    /// 恢复模式：从账本读阶段而不是写。
    recovered: std::collections::BTreeMap<String, ActionPhase>,
}

impl SqliteActionJournal {
    /// 新建。`device_id` / `device_name` 必须来自真实设备鉴权。
    pub fn new(
        store: Arc<ActionStore>,
        device_id: impl Into<String>,
        device_name: impl Into<String>,
        host_id: impl Into<String>,
        thread_id: impl Into<String>,
    ) -> Self {
        Self {
            store,
            device_id: device_id.into(),
            device_name: device_name.into(),
            host_id: host_id.into(),
            thread_id: thread_id.into(),
            recovered: std::collections::BTreeMap::new(),
        }
    }

    fn key(&self) -> ThreadKey {
        ThreadKey {
            host_id: self.host_id.clone(),
            thread_id: self.thread_id.clone(),
        }
    }

    /// 读既有行的阶段（供路由层判断"同 ID 是否已存在、到哪一步"）。
    ///
    /// 这是 `action_id` 幂等键的读侧：`send_once` 在 `register` 失败时用它
    /// 决定回执档位。
    pub fn phase_from_ledger(&self, action_id: &str) -> Option<ActionPhase> {
        if let Some(p) = self.recovered.get(action_id) {
            return Some(*p);
        }
        let (_, _, _, snap) = self.store.action_identity(action_id)?;
        state_to_phase(snap.state)
    }

    /// 该动作是否已在账本里（任何状态，含墓碑）。
    pub fn exists(&self, action_id: &str) -> bool {
        self.store.action_identity(action_id).is_some()
    }

    /// 账本里该动作的**设备与线程**（用于拒绝跨设备/跨线程复用同 ID）。
    pub fn identity_of(&self, action_id: &str) -> Option<(String, String)> {
        self.store
            .action_identity(action_id)
            .map(|(device, thread, _, _)| (device, thread))
    }

    /// 该动作的载荷摘要（用于拒绝"同 ID 换载荷"）。
    pub fn payload_hash_of(&self, action_id: &str) -> Option<String> {
        self.store
            .action_identity(action_id)
            .map(|(_, _, hash, _)| hash)
    }

    /// 单事务冻结真实设备、原线程、载荷和通道，必须早于 AX 导航或编辑。
    pub(crate) fn reserve_request(&self, action_id: &str, text: &str) -> Result<(), JournalError> {
        let payload_hash = crate::services::codex_desktop_window::sha256_prefix12(text);
        let ctx = ActionContext {
            action_id: action_id.to_string(),
            device_id: self.device_id.clone(),
            device_name: self.device_name.clone(),
            kind: ActionKind::Send,
            key: self.key(),
        };
        match self.store.register_reserved(
            &ctx,
            ActionKind::Send,
            &payload_hash,
            ControlRoute::DesktopAx,
        ) {
            Ok(Some(_)) => Ok(()),
            Ok(None) | Err(StoreError::Conflict) => Err(JournalError::Duplicate),
            Err(StoreError::Database(_)) => Err(JournalError::Storage),
        }
    }

    /// 把明确未提交落成 `Rejected`，把可能已提交落成 `Unknown`。
    ///
    /// 由调用方按 `SendReport.outcome` 选择，**不**在本模块猜。
    pub fn settle(&self, action_id: &str, delivery: Delivery, reason: Option<&str>) -> bool {
        let receipt = crate::services::codex_desktop_ipc::types::ActionReceipt {
            action_id: action_id.to_string(),
            delivery,
            route: ControlRoute::DesktopAx,
            turn_id: None,
            envelope_id: None,
            reason_code: reason.map(str::to_string),
        };
        self.store.finish(&receipt).is_ok()
    }
}

impl ActionJournal for SqliteActionJournal {
    fn register(
        &mut self,
        action_id: &str,
        key: &TargetKey,
        text: &str,
    ) -> Result<(), JournalError> {
        // 登记前先核**目标一致性**：账本的 `thread_id` 来自构造时的线程，
        // 若与本次 `TargetKey` 的线程不一致，说明调用方把不同会话混进了同一个
        // 账本实例——那会让跨会话去重失效。直接拒，不猜。
        if key.thread_id != self.thread_id {
            return Err(JournalError::Storage);
        }

        // **同 ID 已存在** → 重复动作。绝不覆盖既有阶段（那会把"可能已提交"
        // 洗成"刚登记"）。
        if self.exists(action_id) {
            if let Some(phase) = self.phase_from_ledger(action_id) {
                self.recovered.insert(action_id.to_string(), phase);
            }
            return Err(JournalError::Duplicate);
        }

        self.reserve_request(action_id, text)
    }

    fn advance(&mut self, action_id: &str, phase: ActionPhase) -> Result<(), JournalError> {
        // `Verified` 是唯一会落到 `Accepted` 的阶段；其余都在 reserved 内，
        // 账本层不需要额外写（`reserved` 已经是"写路径内"）。
        //
        // 但**必须**让 `Registered → AboutToFill` 之外的推进可被观察到：
        // 用 `recovered` 记录最新阶段，供同进程内的重放读回。
        if phase == ActionPhase::Verified {
            let receipt = crate::services::codex_desktop_ipc::types::ActionReceipt {
                action_id: action_id.to_string(),
                delivery: Delivery::Accepted,
                route: ControlRoute::DesktopAx,
                turn_id: None,
                envelope_id: None,
                reason_code: None,
            };
            // **先落库，成功后才写内存**（复审 Minor-1）：反过来的话，
            // `finish` 失败时 `phase()` 会读到虚假的 `Verified`。
            return match self.store.finish(&receipt) {
                Ok(()) => {
                    self.recovered.insert(action_id.to_string(), phase);
                    Ok(())
                }
                Err(StoreError::Conflict) => Err(JournalError::Duplicate),
                Err(StoreError::Database(_)) => Err(JournalError::Storage),
            };
        }
        self.recovered.insert(action_id.to_string(), phase);
        // 未到终态的推进：账本里仍是 `reserved`（正确——仍在写路径内）。
        Ok(())
    }

    fn phase(&self, action_id: &str) -> Option<ActionPhase> {
        self.recovered
            .get(action_id)
            .copied()
            .or_else(|| self.phase_from_ledger(action_id))
    }

    fn binding_of(&self, action_id: &str) -> Option<&TargetKey> {
        // 持久账本不存完整 `TargetKey`（那会引入一轮 schema 迁移）。
        // 重放判定的关键字段由 `route_of` + `identical payload` 覆盖：
        // 同 ID 换载荷会在 `register` 前被 `payload_hash` 比对拦下（见调用方）。
        let _ = action_id;
        None
    }

    fn text_of(&self, action_id: &str) -> Option<String> {
        // 账本**不存正文**（纪律）。返回 None 表示"无法比对载荷"，
        // 调用方据此保守处理（见 `send_once` 的 Duplicate 分支）。
        let _ = action_id;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_mapping_never_claims_zero_side_effect_after_fill() {
        // 只要进入写路径，就不能停在 NearestZero（`Queued`）。
        for p in [
            ActionPhase::AboutToFill,
            ActionPhase::Filled,
            ActionPhase::AboutToSubmit,
            ActionPhase::SubmitAttempted,
        ] {
            assert_eq!(
                phase_to_state(p),
                ActionState::Reserved,
                "{p:?} 仍在写路径内，不得落回 Queued"
            );
        }
        assert_eq!(phase_to_state(ActionPhase::Registered), ActionState::Queued);
        assert_eq!(phase_to_state(ActionPhase::Verified), ActionState::Accepted);
    }

    #[test]
    fn recovered_unknown_is_conservative() {
        // 重启后 `Unknown` 必须表达"**可能已提交**"，不能是"零副作用"，
        // 也不能是 `AboutToFill`（那个阶段 may_have_submitted=false，
        // 会把"可能已提交"降级成"明确未提交"）。
        assert_eq!(
            state_to_phase(ActionState::Unknown),
            Some(ActionPhase::AboutToSubmit)
        );
        // 已撤回不复活。
        assert_eq!(state_to_phase(ActionState::Cancelled), None);
    }
}
