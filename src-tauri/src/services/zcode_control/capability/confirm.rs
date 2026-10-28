//! 首次真实动作确认的**纯判据**（spec R4；生产计划 T5/T6）。
//!
//! 本文件零 I/O（无网络 / 无 DB / 无文件 / 无日志）：只承载确认上下文、观察输入、
//! 判定结果与三个纯判据函数。接线（登记 / 观察 / 落账 / 超时）住在 Hub 的
//! `ConfirmationTracker`（hub.rs），本文件只回答「这条观察算不算确认」。
//!
//! 判据（spec R4，控制者裁决口径）：
//! - send = generation 匹配 AND revision 前进 AND last_user_message_id 变化
//!   （新用户消息可见——用 id 变化代表可见，绝不存正文）；
//! - model/thought = generation 匹配 AND revision 前进 AND 新快照
//!   `selection.{provider,model,thought}` 与目标**精确相等**（三字段全等；
//!   wrong model → 不确认，保持候选）；
//! - pending 卡（single/multi/free/approval）= generation 匹配 AND revision 前进
//!   AND 目标 `request_key` 不在观察视图的 pending 列表（只认**目标卡**撤下，
//!   旁边的卡消失绝不代答）；
//! - interrupt = generation 匹配 AND revision 前进 AND 目标守卫 id 不在观察快照的
//!   `activeWorks`（只认**目标守卫**消失，别的回合停止绝不代答）。
//!
//! 边界纪律：观察输入只含 id 形状的指纹、选择三元组、pending 卡的 id 集合与
//! 守卫 id 集合，**绝不**承载正文、题目、选项、答案、命令、JWT、链接。
//!
//! # 自裁偏差（已报备）
//!
//! - `ConfirmationTarget::UserMessage` 携带 `baseline_id`（brief 原文是单元变体）：
//!   「last_user_message_id 变化」需要 baseline 才能比较，而 `ConfirmationContext`
//!   的字段表没有可安放处；登记方（tracker）在提交动作前捕获当下指纹存进目标，
//!   判据函数据此判断「变化」。

use std::time::Instant;

use super::{Capability, CapabilityKey};
use crate::services::zcode_control::types::SessionView;

/// 确认目标（动作提交时登记；观察与之对拍）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConfirmationTarget {
    /// 发送文本：携带**登记时刻**的最后用户消息 id 指纹（id 形状，非正文）。
    UserMessage {
        /// 登记时的指纹；观察侧指纹与它不同 = 有新用户消息可见。
        baseline_id: String,
    },
    /// 模型/思考档位切换：动作携带的目标三元组（provider 来自登记时刻快照）。
    Selection {
        provider: String,
        model: String,
        thought: String,
    },
    /// 问答作答 / 审批应答：目标宿主卡的 requestKey（本实现取 `PendingView`
    /// 的 `interaction_id`；只存 id 形状，绝不存题目/选项/答案）。
    Pending {
        /// 登记时刻动作指向的宿主卡 id；观察侧它不在 pending 列表 = 目标卡撤下。
        request_key: String,
    },
    /// 打断（stop）：目标守卫 id（动作自带的 `expectedForegroundExecutionId`；
    /// 只存 id 形状，绝不存命令/回合内容）。观察侧它不在 control 的 `activeWorks`
    /// = 目标守卫消失。
    Interrupt {
        /// 登记时刻动作指向的守卫 id；观察侧它不在 activeWorks = 目标守卫消失。
        guard_id: String,
    },
}

/// 一次待确认登记（tracker 持有；动作提交前捕获 baseline）。
#[derive(Debug, Clone)]
pub(crate) struct ConfirmationContext {
    pub capability: Capability,
    pub key: CapabilityKey,
    pub session_id: String,
    /// 动作提交时的 Hub 控制代际：观察代际不一致 = 旧代际，结果丢弃不落账。
    pub generation: u64,
    /// 动作提交时的会话 revision 基线（未知 = 0；判据要求观察严格大于它）。
    pub baseline_revision: u64,
    pub target: ConfirmationTarget,
    /// 有界确认窗（生产 5s）：到点未确认 → inconclusive，绝不重发。
    pub deadline: Instant,
}

/// 判定结果（spec R4 三态）。
///
/// 纯判据只产出 `Confirmed` / `Inconclusive`：`Degraded` 由**动作回执显式失败**
/// 产生（Hub tracker 的回执收口路径直接落账，不经过观察判据）；变体按 brief
/// 接口保留完整三态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfirmationOutcome {
    Confirmed,
    Inconclusive,
    // 回执收口路径（settle_receipt）的产出形态；判据函数不产出它（brief 接口
    // 固定三态，条目级保留）。
    #[allow(dead_code)]
    Degraded,
}

/// 纯判据输入（由桥/事件泵/测试构造；不含正文）：send 侧观察。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SendObservation {
    pub generation: u64,
    pub revision: u64,
    pub last_user_message_id: String,
}

/// 纯判据输入（由桥/事件泵/测试构造；不含正文）：选择侧观察。
///
/// `thought` 来自快照 `config.thought`（缺席按空串——与目标相等判定照常执行，
/// 缺席即不可能与显式目标相等，保守不确认）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SelectionObservation {
    pub generation: u64,
    pub revision: u64,
    pub provider: String,
    pub model: String,
    pub thought: String,
}

/// 纯判据输入（由桥/事件泵/测试构造；不含正文）：interrupt 侧观察。
///
/// 这是 T7 接口里 `ControlView` 的**等效 control 快照**（自裁偏差，已报备）：现有
/// `types::ControlView` 是协议 DTO，只承载 `activeWorks` 数组，没有 generation/
/// revision 的安放处，而 interrupt 判据要求「generation 匹配 AND revision 前进
/// AND 守卫消失」三合一；不改协议 DTO（不猜协议），判据输入用本快照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InterruptObservation {
    /// 观察时刻的 Hub 代际（与 ctx.generation 对拍；旧代际一律不确认）。
    pub generation: u64,
    /// 观察侧会话 revision（投影尚无水位 = None：没有任何新证据，等下一帧）。
    pub revision: Option<u64>,
    /// 宿主宣告的 activeWorks 守卫 id 集合（只装 `foregroundExecutionId` 的 id
    /// 形状；control 未宣告 = None：没有「守卫消失」的证据，等下一帧——绝不把
    /// control 未知当成已清空）。
    pub active_works: Option<Vec<String>>,
}

/// 动作提交路径的**会话侧** baseline 观察（bridge 只读快照产出；key 由后端补齐）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfirmationObservation {
    /// 当前会话 revision（投影尚无快照 = None）。
    pub revision: Option<u64>,
    /// 当前选择的 provider（未取到快照 config = None）。
    pub provider: Option<String>,
    /// 会话日志头指纹（`logEpoch:seq`；快照未携带 = None）。
    pub log_fingerprint: Option<String>,
    /// 当前 control 宣告的守卫 id 集合（interrupt 登记面用：初始 control 未宣告
    /// = None → 没有可等待的守卫，不登记；宣告了则只装 `foregroundExecutionId`
    /// 的 id 形状）。
    pub active_works: Option<Vec<String>>,
}

/// 动作提交路径的 baseline（`HubBackend::confirmation_baseline` 产出）。
///
/// `key` 来自后端的宿主证据（capability resolver 同一拼法）；会话侧字段来自桥。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfirmationBaseline {
    pub key: CapabilityKey,
    pub observation: ConfirmationObservation,
}

/// send 判据（spec R4）。
///
/// generation 不匹配时返回 `Inconclusive` 只是纯函数侧的保守兜底：调用方
/// （tracker）负责在判定前把旧代际 pending **丢弃且不落账**（spec：「旧代际确认
/// 结果丢弃」）；本分支保证即使调用方漏过滤也绝不 confirmed。
pub(crate) fn confirm_send(
    ctx: &ConfirmationContext,
    obs: &SendObservation,
) -> ConfirmationOutcome {
    if obs.generation != ctx.generation {
        return ConfirmationOutcome::Inconclusive;
    }
    let ConfirmationTarget::UserMessage { baseline_id } = &ctx.target else {
        // send 上下文配了别的目标类型：登记侧接线错误，保守不确认。
        return ConfirmationOutcome::Inconclusive;
    };
    let revision_advanced = obs.revision > ctx.baseline_revision;
    let message_changed =
        !obs.last_user_message_id.is_empty() && obs.last_user_message_id != *baseline_id;
    if revision_advanced && message_changed {
        ConfirmationOutcome::Confirmed
    } else {
        ConfirmationOutcome::Inconclusive
    }
}

/// model/thought 判据（spec R4）：三项精确相等 + revision 前进。
///
/// generation 不匹配的兜底语义同 [`confirm_send`]。
pub(crate) fn confirm_selection(
    ctx: &ConfirmationContext,
    obs: &SelectionObservation,
) -> ConfirmationOutcome {
    if obs.generation != ctx.generation {
        return ConfirmationOutcome::Inconclusive;
    }
    let ConfirmationTarget::Selection {
        provider,
        model,
        thought,
    } = &ctx.target
    else {
        return ConfirmationOutcome::Inconclusive;
    };
    let revision_advanced = obs.revision > ctx.baseline_revision;
    let selection_matched =
        obs.provider == *provider && obs.model == *model && obs.thought == *thought;
    if revision_advanced && selection_matched {
        ConfirmationOutcome::Confirmed
    } else {
        ConfirmationOutcome::Inconclusive
    }
}

/// pending 卡判据（spec R4；生产计划 T6）：目标 requestKey 撤下 + revision 前进。
///
/// generation 不匹配的兜底语义同 [`confirm_send`]；`after.revision` 缺席（投影尚未
/// 取得水位）同样保守不确认，等下一帧。目标类型不匹配（send/selection 上下文配了
/// 本判据）是登记侧接线错误，保守不确认。
///
/// 判定只读 `after.pending` 的 `interaction_id` 集合：旁边的卡消失绝不构成目标
/// 卡撤下的证据；「只确认回答既有 permission 卡」的边界（spec §4.1）由登记侧保证
/// ——登记只发生在用户对既有卡的动作（见 hub.rs `register_confirmations`）。
pub(crate) fn confirm_pending(
    ctx: &ConfirmationContext,
    after: &SessionView,
) -> ConfirmationOutcome {
    if after.generation != ctx.generation {
        return ConfirmationOutcome::Inconclusive;
    }
    let ConfirmationTarget::Pending { request_key } = &ctx.target else {
        return ConfirmationOutcome::Inconclusive;
    };
    let Some(revision) = after.revision else {
        // 水位未取得（投影尚无快照）：没有任何新证据，等下一帧。
        return ConfirmationOutcome::Inconclusive;
    };
    let revision_advanced = revision > ctx.baseline_revision;
    let target_cleared = !after
        .pending
        .iter()
        .any(|card| card.interaction_id == *request_key);
    if revision_advanced && target_cleared {
        ConfirmationOutcome::Confirmed
    } else {
        ConfirmationOutcome::Inconclusive
    }
}

/// interrupt 判据（spec R4；生产计划 T7）：目标守卫从 activeWorks 消失 + revision 前进。
///
/// generation 不匹配的兜底语义同 [`confirm_send`]；`after.revision` 缺席（投影尚无
/// 水位）与 `after.active_works` 缺席（宿主从未宣告 control）都是「没有任何新证据」
/// ——保守不确认，等下一帧（绝不把 control 未知当成守卫已清空）。
///
/// 判定只读 `after.active_works` 的守卫 id 集合：**别的**守卫消失绝不构成目标守卫
/// 消失的证据（不把其它回合的停止当目标打断成功）；「无守卫不登记」的边界由登记
/// 侧保证——登记只发生在提交时目标守卫已在初始 control 的 activeWorks 里（见
/// hub.rs `register_confirmations`），control 未知时根本不开确认窗。
pub(crate) fn confirm_interrupt(
    ctx: &ConfirmationContext,
    after: &InterruptObservation,
) -> ConfirmationOutcome {
    if after.generation != ctx.generation {
        return ConfirmationOutcome::Inconclusive;
    }
    let ConfirmationTarget::Interrupt { guard_id } = &ctx.target else {
        return ConfirmationOutcome::Inconclusive;
    };
    let Some(revision) = after.revision else {
        // 水位未取得（投影尚无快照）：没有任何新证据，等下一帧。
        return ConfirmationOutcome::Inconclusive;
    };
    let Some(active_works) = after.active_works.as_ref() else {
        // control 未宣告：没有「守卫消失」的证据，等下一帧。
        return ConfirmationOutcome::Inconclusive;
    };
    let revision_advanced = revision > ctx.baseline_revision;
    let guard_cleared = !active_works.iter().any(|id| id == guard_id);
    if revision_advanced && guard_cleared {
        ConfirmationOutcome::Confirmed
    } else {
        ConfirmationOutcome::Inconclusive
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::zcode_control::types::{CapabilityView, PendingKind, PendingView};

    /// brief 指定的测试助手（不是生产接口）：send 上下文，baseline id 固定 `"u0"`
    /// （与 `send_obs` 的用例约定配套：before=u0 → ACK 同形；after=u1 → 变化）。
    fn send_ctx(generation: u64, revision: u64) -> ConfirmationContext {
        ConfirmationContext {
            capability: Capability::Send,
            key: CapabilityKey::new("macos", "9.9.9.9999", 3, "shape", 1),
            session_id: "sess_send".to_owned(),
            generation,
            baseline_revision: revision,
            target: ConfirmationTarget::UserMessage {
                baseline_id: "u0".to_owned(),
            },
            deadline: Instant::now() + std::time::Duration::from_secs(5),
        }
    }

    /// brief 指定的测试助手（不是生产接口）。
    fn send_obs(generation: u64, revision: u64, last_user: &str) -> SendObservation {
        SendObservation {
            generation,
            revision,
            last_user_message_id: last_user.to_owned(),
        }
    }

    /// brief 指定的测试助手（不是生产接口）：选择上下文，provider/thought 固定
    /// （`"synthetic-provider"` / `"high"`），baseline revision 10、generation 7。
    fn selection_ctx(model: &str) -> ConfirmationContext {
        ConfirmationContext {
            capability: Capability::Model,
            key: CapabilityKey::new("macos", "9.9.9.9999", 3, "shape", 1),
            session_id: "sess_selection".to_owned(),
            generation: 7,
            baseline_revision: 10,
            target: ConfirmationTarget::Selection {
                provider: "synthetic-provider".to_owned(),
                model: model.to_owned(),
                thought: "high".to_owned(),
            },
            deadline: Instant::now() + std::time::Duration::from_secs(5),
        }
    }

    /// brief 指定的测试助手（不是生产接口）：generation 7 / revision 11，
    /// provider/thought 与 `selection_ctx` 对齐，只有 model 由参数给定。
    fn selection_obs(model: &str) -> SelectionObservation {
        SelectionObservation {
            generation: 7,
            revision: 11,
            provider: "synthetic-provider".to_owned(),
            model: model.to_owned(),
            thought: "high".to_owned(),
        }
    }

    /// brief Step 1 逐字用例：ACK-only（观察与 baseline 同形）不确认。
    #[test]
    fn ack_alone_does_not_confirm_send() {
        let ctx = send_ctx(7, 10);
        let before = send_obs(7, 10, "u0");
        assert_eq!(
            confirm_send(&ctx, &before),
            ConfirmationOutcome::Inconclusive
        );
    }

    /// brief Step 1 逐字用例：revision 前进 + 新用户消息可见 → confirmed。
    #[test]
    fn new_user_message_and_revision_confirms_send() {
        let ctx = send_ctx(7, 10);
        let after = send_obs(7, 11, "u1");
        assert_eq!(confirm_send(&ctx, &after), ConfirmationOutcome::Confirmed);
    }

    /// brief Step 1 逐字用例：wrong model 快照不确认（保持候选）。
    #[test]
    fn wrong_model_snapshot_is_not_confirmed() {
        let ctx = selection_ctx("GLM-5.3");
        let after = selection_obs("GLM-5.3-Flash");
        assert_eq!(
            confirm_selection(&ctx, &after),
            ConfirmationOutcome::Inconclusive
        );
    }

    /// 补充：revision 前进但消息 id 未变（无新用户消息）→ 不确认
    /// （「id 变化」判据必须真的咬合，不能被 revision 单独满足）。
    #[test]
    fn revision_alone_without_new_message_is_not_confirmed() {
        let ctx = send_ctx(7, 10);
        let after = send_obs(7, 11, "u0");
        assert_eq!(
            confirm_send(&ctx, &after),
            ConfirmationOutcome::Inconclusive
        );
    }

    /// 补充：消息 id 变化但 revision 未前进 → 不确认（两判据缺一不可）。
    #[test]
    fn message_change_alone_without_revision_is_not_confirmed() {
        let ctx = send_ctx(7, 10);
        let after = send_obs(7, 10, "u1");
        assert_eq!(
            confirm_send(&ctx, &after),
            ConfirmationOutcome::Inconclusive
        );
    }

    /// 补充：旧代际观察 → 纯函数侧保守不确认（tracker 另行丢弃不落账）。
    #[test]
    fn stale_generation_send_observation_is_never_confirmed() {
        let ctx = send_ctx(7, 10);
        let stale = send_obs(6, 11, "u1");
        assert_eq!(
            confirm_send(&ctx, &stale),
            ConfirmationOutcome::Inconclusive
        );
    }

    /// 补充：三项精确相等 + revision 前进 → confirmed（选择正控路径）。
    #[test]
    fn exact_selection_snapshot_confirms() {
        let ctx = selection_ctx("GLM-5.3");
        let after = selection_obs("GLM-5.3");
        assert_eq!(
            confirm_selection(&ctx, &after),
            ConfirmationOutcome::Confirmed
        );
    }

    /// 补充：快照与 baseline 相同（ACK-only 形态）→ 不确认。
    #[test]
    fn unchanged_selection_is_not_confirmed() {
        let ctx = selection_ctx("GLM-5.3");
        // revision 10 = baseline：与登记时刻同形，没有任何新证据。
        let mut before = selection_obs("GLM-5.3");
        before.revision = 10;
        assert_eq!(
            confirm_selection(&ctx, &before),
            ConfirmationOutcome::Inconclusive
        );
    }

    /// 补充：thought 不匹配 → 不确认（三字段全等判据逐位咬合）。
    #[test]
    fn wrong_thought_snapshot_is_not_confirmed() {
        let ctx = selection_ctx("GLM-5.3");
        let mut after = selection_obs("GLM-5.3");
        after.thought = "low".to_owned();
        assert_eq!(
            confirm_selection(&ctx, &after),
            ConfirmationOutcome::Inconclusive
        );
    }

    /// 补充：provider 不匹配 → 不确认（spec 精确相等含 provider）。
    #[test]
    fn wrong_provider_snapshot_is_not_confirmed() {
        let ctx = selection_ctx("GLM-5.3");
        let mut after = selection_obs("GLM-5.3");
        after.provider = "other-provider".to_owned();
        assert_eq!(
            confirm_selection(&ctx, &after),
            ConfirmationOutcome::Inconclusive
        );
    }

    /// 补充：旧代际选择观察 → 纯函数侧保守不确认。
    #[test]
    fn stale_generation_selection_observation_is_never_confirmed() {
        let ctx = selection_ctx("GLM-5.3");
        let mut stale = selection_obs("GLM-5.3");
        stale.generation = 6;
        assert_eq!(
            confirm_selection(&ctx, &stale),
            ConfirmationOutcome::Inconclusive
        );
    }

    // -----------------------------------------------------------------------
    // T6：pending 卡式能力（single/multi/free/approval）的纯判据
    // -----------------------------------------------------------------------

    /// brief 指定的测试助手（不是生产接口）：Pending 确认上下文（generation 7，
    /// 与 `session_with` 的固定代际对齐）。
    fn pending_ctx(request_key: &str, revision: u64) -> ConfirmationContext {
        ConfirmationContext {
            capability: Capability::SingleSelect,
            key: CapabilityKey::new("macos", "9.9.9.9999", 3, "shape", 1),
            session_id: "sess_pending".to_owned(),
            generation: 7,
            baseline_revision: revision,
            target: ConfirmationTarget::Pending {
                request_key: request_key.to_owned(),
            },
            deadline: Instant::now() + std::time::Duration::from_secs(5),
        }
    }

    /// brief 指定的测试助手（不是生产接口）：最小会话视图（generation 固定 7）。
    fn session_with(revision: u64, pending: Vec<PendingView>) -> SessionView {
        SessionView {
            session_id: "sess_pending".to_owned(),
            generation: 7,
            revision: Some(revision),
            ready: true,
            pending,
            control: None,
            selection: None,
            capabilities: CapabilityView::default(),
        }
    }

    /// 合成一张最小 pending 卡（判据只看 interaction_id；其余字段对判定不可见，
    /// 也绝不携带题目/选项/答案正文）。
    fn pending(interaction_id: &str) -> PendingView {
        PendingView {
            interaction_id: interaction_id.to_owned(),
            kind: PendingKind::UserInput,
            anchor_row_id: None,
            tool_call_id: None,
            created_at: None,
            summary: None,
            tool_name: None,
            schema_interaction: None,
            sensitive: false,
            auto_resolution: false,
            auto_resolution_state: None,
            questions: Vec::new(),
            options: Vec::new(),
        }
    }

    /// brief Step 1 逐字用例：目标 pending 消失 + revision 前进 → confirmed。
    #[test]
    fn pending_cleared_and_revision_advanced_confirms_answer() {
        let ctx = pending_ctx("req-1", 5);
        let after = session_with(6, vec![]);
        assert_eq!(
            confirm_pending(&ctx, &after),
            ConfirmationOutcome::Confirmed
        );
    }

    /// brief Step 1 逐字用例：同一 pending 仍在（同形观察）→ 不确认。
    #[test]
    fn same_pending_does_not_confirm() {
        let ctx = pending_ctx("req-1", 5);
        let after = session_with(5, vec![pending("req-1")]);
        assert_eq!(
            confirm_pending(&ctx, &after),
            ConfirmationOutcome::Inconclusive
        );
    }

    /// 补充：目标卡仍在场（旁边别的卡撤下了）+ revision 前进 → 不确认
    /// （「只确认目标 requestKey 消失，不把旁边的卡消失当成功」）。
    #[test]
    fn target_still_present_while_sibling_cleared_is_not_confirmed() {
        let ctx = pending_ctx("req-1", 5);
        let after = session_with(6, vec![pending("req-1"), pending("req-9")]);
        assert_eq!(
            confirm_pending(&ctx, &after),
            ConfirmationOutcome::Inconclusive
        );
    }

    /// 补充：目标卡已撤但 revision 未前进（同形观察）→ 不确认（两判据缺一不可）。
    #[test]
    fn cleared_without_revision_advance_is_not_confirmed() {
        let ctx = pending_ctx("req-1", 5);
        let after = session_with(5, vec![]);
        assert_eq!(
            confirm_pending(&ctx, &after),
            ConfirmationOutcome::Inconclusive
        );
    }

    /// 补充：观察证据缺一半（revision 未取得）→ 保守不确认，等下一帧。
    #[test]
    fn missing_revision_is_never_confirmed() {
        let ctx = pending_ctx("req-1", 5);
        let mut after = session_with(6, vec![]);
        after.revision = None;
        assert_eq!(
            confirm_pending(&ctx, &after),
            ConfirmationOutcome::Inconclusive
        );
    }

    /// 补充：旧代际观察 → 纯函数侧保守不确认（tracker 另行丢弃不落账）。
    #[test]
    fn stale_generation_pending_observation_is_never_confirmed() {
        let ctx = pending_ctx("req-1", 5);
        let mut stale = session_with(6, vec![]);
        stale.generation = 6;
        assert_eq!(
            confirm_pending(&ctx, &stale),
            ConfirmationOutcome::Inconclusive
        );
    }

    /// 补充：send 上下文配了 pending 判据（登记侧接线错误）→ 保守不确认。
    #[test]
    fn wrong_target_type_is_never_confirmed() {
        let ctx = send_ctx(7, 5);
        let after = session_with(6, vec![]);
        assert_eq!(
            confirm_pending(&ctx, &after),
            ConfirmationOutcome::Inconclusive
        );
    }

    // -----------------------------------------------------------------------
    // T7：interrupt 的纯判据（目标守卫从 activeWorks 消失 + revision 前进）
    // -----------------------------------------------------------------------

    /// brief 指定的测试助手（不是生产接口）：Interrupt 确认上下文（generation 7，
    /// 与 `control_with` 的固定代际对齐）。
    fn interrupt_ctx(guard_id: &str, revision: u64) -> ConfirmationContext {
        ConfirmationContext {
            capability: Capability::Interrupt,
            key: CapabilityKey::new("macos", "9.9.9.9999", 3, "shape", 1),
            session_id: "sess_interrupt".to_owned(),
            generation: 7,
            baseline_revision: revision,
            target: ConfirmationTarget::Interrupt {
                guard_id: guard_id.to_owned(),
            },
            deadline: Instant::now() + std::time::Duration::from_secs(5),
        }
    }

    /// brief 指定的测试助手（不是生产接口）：等效 control 快照（generation 固定 7；
    /// `active_works` 只装守卫 id 字符串，回合内容不入测试断言）。
    fn control_with(revision: u64, active_works: Vec<String>) -> InterruptObservation {
        InterruptObservation {
            generation: 7,
            revision: Some(revision),
            active_works: Some(active_works),
        }
    }

    /// brief Step 1 逐字用例：目标守卫仍在场（同形观察）→ 不确认。
    #[test]
    fn guard_still_active_is_not_confirmed() {
        let ctx = interrupt_ctx("work-1", 2);
        let after = control_with(2, vec!["work-1".into()]);
        assert_eq!(
            confirm_interrupt(&ctx, &after),
            ConfirmationOutcome::Inconclusive
        );
    }

    /// brief Step 1 逐字用例：目标守卫消失 + revision 前进 → confirmed。
    #[test]
    fn guard_cleared_confirms_interrupt() {
        let ctx = interrupt_ctx("work-1", 2);
        let after = control_with(3, vec![]);
        assert_eq!(
            confirm_interrupt(&ctx, &after),
            ConfirmationOutcome::Confirmed
        );
    }

    /// 补充：守卫已消失但 revision 未前进（同形观察）→ 不确认（两判据缺一不可）。
    #[test]
    fn interrupt_cleared_without_revision_advance_is_not_confirmed() {
        let ctx = interrupt_ctx("work-1", 2);
        let after = control_with(2, vec![]);
        assert_eq!(
            confirm_interrupt(&ctx, &after),
            ConfirmationOutcome::Inconclusive
        );
    }

    /// 补充：目标守卫还在场（别的守卫在场/消失都不影响判定）+ revision 前进
    /// → 不确认（不把其它回合的停止当目标打断成功；「基线里另一个守卫此后消失、
    /// 目标仍在」的完整场景由 hub 侧
    /// `stop_confirmation_keeps_candidate_while_target_guard_still_active` 钉住——
    /// 纯判据看不到登记时刻的 activeWorks 基线，只认「目标守卫是否还在」）。
    #[test]
    fn other_guard_cleared_is_not_confirmed() {
        let ctx = interrupt_ctx("work-1", 2);
        let after = control_with(3, vec!["work-1".into(), "work-9".into()]);
        assert_eq!(
            confirm_interrupt(&ctx, &after),
            ConfirmationOutcome::Inconclusive
        );
    }

    /// 补充：旧代际观察 → 纯函数侧保守不确认（tracker 另行丢弃不落账）。
    #[test]
    fn stale_generation_interrupt_observation_is_never_confirmed() {
        let ctx = interrupt_ctx("work-1", 2);
        let mut stale = control_with(3, vec![]);
        stale.generation = 6;
        assert_eq!(
            confirm_interrupt(&ctx, &stale),
            ConfirmationOutcome::Inconclusive
        );
    }

    /// 补充：revision 未取得（投影尚无水位）→ 没有任何新证据，等下一帧。
    #[test]
    fn interrupt_missing_revision_is_never_confirmed() {
        let ctx = interrupt_ctx("work-1", 2);
        let after = InterruptObservation {
            generation: 7,
            revision: None,
            active_works: Some(vec![]),
        };
        assert_eq!(
            confirm_interrupt(&ctx, &after),
            ConfirmationOutcome::Inconclusive
        );
    }

    /// 补充：control 未宣告（宿主从未给出 activeWorks 数组）→ 没有「守卫消失」
    /// 的证据，保守不确认（绝不把 control 未知当成已清空）。
    #[test]
    fn interrupt_missing_control_is_never_confirmed() {
        let ctx = interrupt_ctx("work-1", 2);
        let after = InterruptObservation {
            generation: 7,
            revision: Some(3),
            active_works: None,
        };
        assert_eq!(
            confirm_interrupt(&ctx, &after),
            ConfirmationOutcome::Inconclusive
        );
    }

    /// 补充：send 上下文配了 interrupt 判据（登记侧接线错误）→ 保守不确认。
    #[test]
    fn interrupt_wrong_target_type_is_never_confirmed() {
        let ctx = send_ctx(7, 2);
        let after = control_with(3, vec![]);
        assert_eq!(
            confirm_interrupt(&ctx, &after),
            ConfirmationOutcome::Inconclusive
        );
    }
}
