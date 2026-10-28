//! ZCode 能力解析**纯核**（spec §6.1/§6.2；生产计划 T1）。
//!
//! 本子树零 I/O（无网络 / 无 DB / 无文件），只承载类型与纯函数：
//! - `key`：能力 key 五元组与已知 build 的 shipped 表查询；
//! - `state`：八项能力枚举、状态枚举、账本记录与接口形状证据（唯一定义处）；
//! - `interface`：接口形状 → 候选能力的纯判定（spec §6.4）。
//!
//! 边界纪律：只记录存在性与形状，**绝不**存正文、题目、选项、答案、JWT、链接。
//! T2 账本 / T3 形状采集 / T4 resolver 都建立在本核之上（T1 交付时尚无消费方，
//! 故沿 `bridge.rs` 惯例仅在非测试构建放宽 dead_code / unused_imports——测试
//! 构建必须全部用到）。
#![cfg_attr(not(test), allow(dead_code))]

pub(crate) mod interface;
pub(crate) mod key;
pub(crate) mod state;

// 便捷再导出（T2-T4 的统一入口面）；T1 阶段生产构建暂无消费方，仅非测试构建
// 放宽 unused_imports（测试构建由下方冒烟用例消费，不得放宽）。
#[cfg_attr(not(test), allow(unused_imports))]
pub(crate) use interface::interface_capabilities;
#[cfg_attr(not(test), allow(unused_imports))]
pub(crate) use key::{shipped_state, CapabilityKey};
#[cfg_attr(not(test), allow(unused_imports))]
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
