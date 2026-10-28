//! 首次真实动作确认的**纯判据**（spec R4；生产计划 T5）。
//!
//! 本文件零 I/O（无网络 / 无 DB / 无文件 / 无日志）：只承载确认上下文、观察输入、
//! 判定结果与两个纯判据函数。接线（登记 / 观察 / 落账 / 超时）住在 Hub 的
//! `ConfirmationTracker`（hub.rs），本文件只回答「这条观察算不算确认」。
//!
//! 判据（spec R4，控制者裁决口径）：
//! - send = generation 匹配 AND revision 前进 AND last_user_message_id 变化
//!   （新用户消息可见——用 id 变化代表可见，绝不存正文）；
//! - model/thought = generation 匹配 AND revision 前进 AND 新快照
//!   `selection.{provider,model,thought}` 与目标**精确相等**（三字段全等；
//!   wrong model → 不确认，保持候选）。
//!
//! 边界纪律：观察输入只含 id 形状的指纹与选择三元组，**绝不**承载正文、题目、
//! 选项、答案、JWT、链接。
//!
//! # 自裁偏差（已报备）
//!
//! - `ConfirmationTarget::UserMessage` 携带 `baseline_id`（brief 原文是单元变体）：
//!   「last_user_message_id 变化」需要 baseline 才能比较，而 `ConfirmationContext`
//!   的字段表没有可安放处；登记方（tracker）在提交动作前捕获当下指纹存进目标，
//!   判据函数据此判断「变化」。

use std::time::Instant;

use super::{Capability, CapabilityKey};

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

/// 动作提交路径的**会话侧** baseline 观察（bridge 只读快照产出；key 由后端补齐）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfirmationObservation {
    /// 当前会话 revision（投影尚无快照 = None）。
    pub revision: Option<u64>,
    /// 当前选择的 provider（未取到快照 config = None）。
    pub provider: Option<String>,
    /// 会话日志头指纹（`logEpoch:seq`；快照未携带 = None）。
    pub log_fingerprint: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
