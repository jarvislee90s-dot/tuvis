//! 接口形状 → 候选能力的纯判定（spec R3/§6.4；生产计划 T1）。
//!
//! 判定纪律：每项能力 = 对应方法存在 **AND** 形状存在；本函数只回答「接口上
//! 这项能力是否具备」，真实目标存在与否由 T4 resolver 叠加（spec §6.4：真实
//! 目标不存在不显示）。零 I/O、零正文字段。

use std::collections::BTreeMap;

use super::state::{Capability, ZcodeInterfaceEvidence};

/// 判定接口形状支持哪些能力（spec §6.4；返回恒含八键的 `BTreeMap`）。
///
/// 每项能力 = 对应方法存在 **AND** 形状存在：
/// - `Send` = `method_send`；
/// - `SingleSelect` = `method_send` AND `has_user_input_pending`；
/// - `MultiSelect` = SingleSelect AND `has_multi_select`；
/// - `FreeText` = SingleSelect AND `has_free_text`；
/// - `Interrupt` = `method_stop` AND `has_active_work`；
/// - `ApprovalReply` = `method_resolve_interaction` AND `has_permission_pending`；
/// - `Model` = `method_selection_get` AND `method_selection_switch` AND
///   `has_model_catalog`；
/// - `ThoughtLevel` = Model AND `has_thought_levels`。
///
/// 返回 `false` 表示「接口上不具备」（缺方法或缺形状），由 T4 统一落
/// unsupported 语义；本函数不做 shipped/账本叠加。
pub(crate) fn interface_capabilities(
    evidence: &ZcodeInterfaceEvidence,
) -> BTreeMap<Capability, bool> {
    let send = evidence.method_send;
    let single_select = send && evidence.has_user_input_pending;
    let model = evidence.method_selection_get
        && evidence.method_selection_switch
        && evidence.has_model_catalog;
    BTreeMap::from([
        (Capability::Send, send),
        (Capability::SingleSelect, single_select),
        (
            Capability::MultiSelect,
            single_select && evidence.has_multi_select,
        ),
        (
            Capability::FreeText,
            single_select && evidence.has_free_text,
        ),
        (
            Capability::Interrupt,
            evidence.method_stop && evidence.has_active_work,
        ),
        (
            Capability::ApprovalReply,
            evidence.method_resolve_interaction && evidence.has_permission_pending,
        ),
        (Capability::Model, model),
        (
            Capability::ThoughtLevel,
            model && evidence.has_thought_levels,
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// brief Step 1 逐字用例：Send 只看方法，MultiSelect 还要形状齐备。
    #[test]
    fn interface_requires_method_and_shape() {
        let mut e = ZcodeInterfaceEvidence::default();
        assert_eq!(
            interface_capabilities(&e).get(&Capability::Send),
            Some(&false)
        );
        e.method_send = true;
        assert_eq!(
            interface_capabilities(&e).get(&Capability::Send),
            Some(&true)
        );
        e.has_user_input_pending = true;
        e.has_multi_select = true;
        assert_eq!(
            interface_capabilities(&e).get(&Capability::MultiSelect),
            Some(&true)
        );
    }

    /// 形状全量给齐 → 八项全开（返回表恒含八键）。
    #[test]
    fn interface_matrix_all_true_when_method_and_shape_present() {
        let full = ZcodeInterfaceEvidence {
            method_send: true,
            method_resolve_interaction: true,
            method_stop: true,
            method_selection_get: true,
            method_selection_switch: true,
            has_user_input_pending: true,
            has_multi_select: true,
            has_free_text: true,
            has_permission_pending: true,
            has_active_work: true,
            has_model_catalog: true,
            has_thought_levels: true,
        };
        let caps = interface_capabilities(&full);
        assert_eq!(caps.len(), 8);
        for capability in [
            Capability::Send,
            Capability::SingleSelect,
            Capability::MultiSelect,
            Capability::FreeText,
            Capability::Interrupt,
            Capability::ApprovalReply,
            Capability::Model,
            Capability::ThoughtLevel,
        ] {
            assert_eq!(
                caps.get(&capability),
                Some(&true),
                "{capability:?} 应为 true"
            );
        }
    }

    /// 只给形状不给方法 → 全关（缺方法返回 unsupported 侧）。
    #[test]
    fn interface_matrix_false_when_method_missing() {
        let shape_only = ZcodeInterfaceEvidence {
            has_user_input_pending: true,
            has_multi_select: true,
            has_free_text: true,
            has_permission_pending: true,
            has_active_work: true,
            has_model_catalog: true,
            has_thought_levels: true,
            ..ZcodeInterfaceEvidence::default()
        };
        let caps = interface_capabilities(&shape_only);
        for capability in [
            Capability::Send,
            Capability::SingleSelect,
            Capability::MultiSelect,
            Capability::FreeText,
            Capability::Interrupt,
            Capability::ApprovalReply,
            Capability::Model,
            Capability::ThoughtLevel,
        ] {
            assert_eq!(
                caps.get(&capability),
                Some(&false),
                "{capability:?} 应为 false"
            );
        }
    }

    /// 方法齐备但缺单项形状 → 只有依赖该项的能力退回 false。
    #[test]
    fn interface_matrix_false_when_single_shape_missing() {
        let missing_shape = ZcodeInterfaceEvidence {
            method_send: true,
            method_resolve_interaction: true,
            method_stop: true,
            method_selection_get: true,
            method_selection_switch: true,
            has_user_input_pending: true,
            has_permission_pending: true,
            has_active_work: true,
            has_model_catalog: true,
            ..ZcodeInterfaceEvidence::default()
        };
        let caps = interface_capabilities(&missing_shape);
        // 缺 multi_select / free_text / thought_levels：仅这三项退回 false。
        assert_eq!(caps.get(&Capability::MultiSelect), Some(&false));
        assert_eq!(caps.get(&Capability::FreeText), Some(&false));
        assert_eq!(caps.get(&Capability::ThoughtLevel), Some(&false));
        // 其余五项不受影响。
        assert_eq!(caps.get(&Capability::Send), Some(&true));
        assert_eq!(caps.get(&Capability::SingleSelect), Some(&true));
        assert_eq!(caps.get(&Capability::Interrupt), Some(&true));
        assert_eq!(caps.get(&Capability::ApprovalReply), Some(&true));
        assert_eq!(caps.get(&Capability::Model), Some(&true));
    }
}
