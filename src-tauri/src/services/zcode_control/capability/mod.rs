//! ZCode 能力解析**纯核**（spec §6.1/§6.2；生产计划 T1）。
//!
//! 本子树零 I/O（无网络 / 无 DB / 无文件），只承载类型与纯函数：
//! - `key`：能力 key 五元组与已知 build 的 shipped 表查询；
//! - `state`：八项能力枚举、状态枚举、账本记录与接口形状证据（唯一定义处）；
//! - `interface`：接口形状 → 候选能力的纯判定（spec §6.4）；
//! - `confirm`：首次真实动作确认的纯判据与确认类型（spec R4；生产计划 T5）。
//!
//! 边界纪律：只记录存在性与形状，**绝不**存正文、题目、选项、答案、JWT、链接。
//! T2 账本 / T3 形状采集 / T4 resolver / T5 确认流程都建立在本核之上；全部符号
//! 都有生产消费方，模块级 dead_code 放宽已收口。

pub(crate) mod confirm;
pub(crate) mod interface;
pub(crate) mod key;
pub(crate) mod resolver;
pub(crate) mod state;

// 便捷再导出（T2-T5 的统一入口面）；全部有生产消费方
// （resolver / hub / dao / types / bridge），不再放宽 unused_imports。
pub(crate) use confirm::{
    confirm_selection, confirm_send, ConfirmationBaseline, ConfirmationContext,
    ConfirmationObservation, ConfirmationOutcome, ConfirmationTarget, SelectionObservation,
    SendObservation,
};
pub(crate) use interface::interface_capabilities;
pub(crate) use key::{shipped_state, CapabilityKey};
pub(crate) use resolver::resolve_capabilities;
pub(crate) use state::{Capability, CapabilityRecord, CapabilityState, ZcodeInterfaceEvidence};

#[cfg(test)]
mod tests {
    use super::*;

    /// 冒烟：七个入口都能从 mod 再导出解析到，且行为与子模块一致。
    #[test]
    fn reexports_resolve() {
        let key = CapabilityKey::new("macos", "3.16.2.8769", 3, "shape", 1);
        assert_eq!(
            shipped_state(&key, Capability::Send),
            Some(CapabilityState::Confirmed)
        );
        assert!(CapabilityRecord::confirmed(Capability::Send)
            .evidence
            .is_none());
        assert!(!ZcodeInterfaceEvidence::default().method_send);
        assert_eq!(
            interface_capabilities(&ZcodeInterfaceEvidence::default()).len(),
            8
        );
    }
}
