//! 能力枚举、状态枚举、账本记录与接口形状证据（spec R2/R3/R6/§6.1；生产计划 T1）。
//!
//! 本文件是这些类型的**唯一定义处**（控制者裁决）：T3 会从 bridge 侧复用
//! `ZcodeInterfaceEvidence` 并补 observe 采集，T2 账本复用 `CapabilityRecord`；
//! 不在此之外的文件重复定义。
//!
//! 边界纪律：`ZcodeInterfaceEvidence` 只记录方法存在性与 pending / control /
//! selection 的**形状布尔**，绝不承载正文、题目、选项、答案、JWT、链接。

/// 八项远程能力（与 `types::CapabilityView` 八字段一一对应；spec R2/§4.1）。
///
/// 声明序即 `Ord` 序（仅用于 `BTreeMap` 键），**不是**解析优先级——优先级归
/// T4 resolver（confirmed > interface_matched > inconclusive > unsupported）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Capability {
    /// 发送文本（send）。
    Send,
    /// 问答卡单选（selectOption 单选）。
    SingleSelect,
    /// 问答卡多选（selectOption 多选）。
    MultiSelect,
    /// 问答卡自由作答（submitInput）。
    FreeText,
    /// 打断运行回合（stop）。
    Interrupt,
    /// 审批应答（resolveInteraction 的 permission 卡）。
    ApprovalReply,
    /// 模型切换（selection.switch）。
    Model,
    /// 思考档位切换（selection.switch 的 thought 维度）。
    ThoughtLevel,
}

impl Capability {
    /// 固定字符串标识（spec R6 账本 `capability` 列与 resolver 的稳定键；
    /// 八项取值冻结，不得随版本漂移）。
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Capability::Send => "send",
            Capability::SingleSelect => "single_select",
            Capability::MultiSelect => "multi_select",
            Capability::FreeText => "free_text",
            Capability::Interrupt => "interrupt",
            Capability::ApprovalReply => "approval_reply",
            Capability::Model => "model",
            Capability::ThoughtLevel => "thought_level",
        }
    }
}

/// 能力状态（spec R2 的五态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum CapabilityState {
    /// 合同/实测明确不可用（不显示动作入口）。
    Unsupported,
    /// 接口形状匹配的候选（首次真实使用确认后才升级，ACK 不算）。
    InterfaceMatched,
    /// 已取得实机确认（shipped 表或首次真实动作确认）。
    Confirmed,
    /// 实机确认明确失败（降级；key 变化前保持，不随重试自动回升）。
    Degraded,
    /// 超时/结果未知（保持候选，不升级、不重发）。
    Inconclusive,
}

/// 接口形状证据（spec R3）：握手协议、hello/schema、方法存在性与投影形状的
/// **存在性布尔**集合。
///
/// 唯一定义处（控制者裁决）：T3 会从 bridge 侧 `use` 本类型并补 observe 采集
/// 方法；字段语义——
/// - `method_*`：send / resolveInteraction / stop / selection.get /
///   selection.switch 方法在接口上是否存在；
/// - `has_*`：pending（user_input / multiSelect / freeText / permission）、
///   control.activeWorks、selection 目录/档位的形状是否出现。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct ZcodeInterfaceEvidence {
    pub method_send: bool,
    pub method_resolve_interaction: bool,
    pub method_stop: bool,
    pub method_selection_get: bool,
    pub method_selection_switch: bool,
    pub has_user_input_pending: bool,
    pub has_multi_select: bool,
    pub has_free_text: bool,
    pub has_permission_pending: bool,
    pub has_active_work: bool,
    pub has_model_catalog: bool,
    pub has_thought_levels: bool,
}

/// 一条能力证据记录（T2 账本行 `zcode_capability_evidence` 的内存形态；spec R6）。
///
/// `reason_code` 用冻结错误码字符串（degraded 时必带）；`evidence` 本任务先给
/// `Option<String>` 占位，形状由 T2 落库时定——无论怎么定都**不得**存正文/
/// 凭据/链接。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CapabilityRecord {
    pub capability: Capability,
    pub state: CapabilityState,
    pub reason_code: Option<String>,
    pub evidence: Option<String>,
}

impl CapabilityRecord {
    /// 首次真实动作确认成功（spec R4）。
    pub(crate) fn confirmed(capability: Capability) -> Self {
        Self {
            capability,
            state: CapabilityState::Confirmed,
            reason_code: None,
            evidence: None,
        }
    }

    /// 实机确认明确失败（spec R4/R6：降级并携带冻结原因码）。
    pub(crate) fn degraded(capability: Capability, reason: String) -> Self {
        Self {
            capability,
            state: CapabilityState::Degraded,
            reason_code: Some(reason),
            evidence: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::zcode_control::types::CapabilityView;

    /// 八项固定字符串（spec R6 账本 `capability` 列的稳定键），且与
    /// `types::CapabilityView` 八字段一一对应（解构即编译期核对字段存在）。
    #[test]
    fn capability_strings_match_capability_view_fields() {
        let view = CapabilityView::default();
        let CapabilityView {
            send,
            single_select,
            multi_select,
            free_text,
            interrupt,
            approval_reply,
            model,
            thought_level,
        } = view;
        drop((
            send,
            single_select,
            multi_select,
            free_text,
            interrupt,
            approval_reply,
            model,
            thought_level,
        ));

        let strings: Vec<&str> = [
            Capability::Send,
            Capability::SingleSelect,
            Capability::MultiSelect,
            Capability::FreeText,
            Capability::Interrupt,
            Capability::ApprovalReply,
            Capability::Model,
            Capability::ThoughtLevel,
        ]
        .iter()
        .map(|capability| capability.as_str())
        .collect();
        assert_eq!(
            strings,
            vec![
                "send",
                "single_select",
                "multi_select",
                "free_text",
                "interrupt",
                "approval_reply",
                "model",
                "thought_level",
            ]
        );
    }

    /// 形状证据默认全 false，且全部字段都是 bool 形状位（边界纪律的实证）。
    #[test]
    fn interface_evidence_default_is_all_false() {
        let e = ZcodeInterfaceEvidence::default();
        let all = [
            e.method_send,
            e.method_resolve_interaction,
            e.method_stop,
            e.method_selection_get,
            e.method_selection_switch,
            e.has_user_input_pending,
            e.has_multi_select,
            e.has_free_text,
            e.has_permission_pending,
            e.has_active_work,
            e.has_model_catalog,
            e.has_thought_levels,
        ];
        assert!(all.iter().all(|v| !v));
    }

    /// T2 复用的构造器：confirmed 无原因码，degraded 必带原因。
    #[test]
    fn record_constructors_fix_state_and_reason() {
        let confirmed = CapabilityRecord::confirmed(Capability::Model);
        assert_eq!(confirmed.capability, Capability::Model);
        assert_eq!(confirmed.state, CapabilityState::Confirmed);
        assert_eq!(confirmed.reason_code, None);
        assert_eq!(confirmed.evidence, None);

        let degraded = CapabilityRecord::degraded(Capability::Send, "action_failed".to_owned());
        assert_eq!(degraded.capability, Capability::Send);
        assert_eq!(degraded.state, CapabilityState::Degraded);
        assert_eq!(degraded.reason_code.as_deref(), Some("action_failed"));
    }

    /// spec R2 的五态两两可分（`Inconclusive` 由 T2 首次确认流程产出，
    /// 这里显式构造锁住它的存在与语义位）。
    #[test]
    fn capability_states_are_pairwise_distinct() {
        let states = [
            CapabilityState::Unsupported,
            CapabilityState::InterfaceMatched,
            CapabilityState::Confirmed,
            CapabilityState::Degraded,
            CapabilityState::Inconclusive,
        ];
        for (i, a) in states.iter().enumerate() {
            for (j, b) in states.iter().enumerate() {
                assert_eq!(a == b, i == j, "{a:?} vs {b:?}");
            }
        }
    }
}
