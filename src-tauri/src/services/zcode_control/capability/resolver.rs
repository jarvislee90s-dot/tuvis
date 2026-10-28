//! 能力 resolver（spec R2/§6.4/§6.6；生产计划 T4）：shipped 表 + DB 能力账本 +
//! 接口形状证据 → [`CapabilityView`] 的**唯一权威解析**（UI 下发与执行闸同源消费）。
//!
//! 每项能力独立解析，来源叠加优先级（spec R2）：
//! 1. **账本记录**（最新实机证据；R4 首次真实动作确认写入）：`Confirmed` /
//!    `InterfaceMatched` / `Inconclusive`（仍候选）按候选链放行；`Degraded` 优先于
//!    候选直到 key 变化，并把冻结原因码透传给 UI；`Unsupported` 明确不可用；
//! 2. **shipped 表**（已知 build 的静态知识）：精确命中**优先于接口候选**——shipped
//!    `Unsupported` 的 multi_select / free_text 在已知 build 上不可用；
//! 3. **接口形状候选**（未知 build 的 A 策略）：接口匹配即候选可用。
//!
//! 叠加纪律（spec R3）：候选/确认仍要求**本代际**接口形状在场——缺形状不可用
//! （无 pending 不显示问答/审批、无目录不显示模型、无运行回合不显示打断）；
//! degraded / unsupported 是实机判决，与形状无关，持有到 key 变化。
//!
//! reasonCode（UI 只见可用/不可用）：可用 = `None`；不可用 = `needs_evidence`
//! （缺证据/缺形状）、`unsupported`（静态知识/账本明确不可用）或账本 degraded 的
//! 冻结原因码。异常兜底（spec §6.6）：解析路径任何 panic →
//! [`CapabilityPolicy::production_unverified`] 全关；账本读失败已由实现折成空
//! Vec（退化为 shipped + 接口候选，不阻塞连接）。

use super::{
    interface_capabilities, shipped_state, Capability, CapabilityKey, CapabilityRecord,
    CapabilityState, ZcodeInterfaceEvidence,
};
use crate::database::dao::zcode_capability::CapabilityLedger;
use crate::services::zcode_control::types::{
    codes, CapabilityFlag, CapabilityPolicy, CapabilityView,
};

/// 八项能力的固定清单（声明序无语义；逐项独立解析）。
const ALL_CAPABILITIES: [Capability; 8] = [
    Capability::Send,
    Capability::SingleSelect,
    Capability::MultiSelect,
    Capability::FreeText,
    Capability::Interrupt,
    Capability::ApprovalReply,
    Capability::Model,
    Capability::ThoughtLevel,
];

/// 解析八项能力矩阵（spec §6.4；生产装配点 = `PartsBackend` 的能力现算源）。
///
/// 本函数是纯函数（零 I/O：账本经 [`CapabilityLedger`] 注入），理论上不 panic；
/// 按 T4 验收契约，任何一层（含账本实现）panic 时整体回退
/// [`CapabilityPolicy::production_unverified`] 全关，绝不放大故障面。
pub(crate) fn resolve_capabilities(
    key: &CapabilityKey,
    evidence: &ZcodeInterfaceEvidence,
    ledger: &dyn CapabilityLedger,
) -> CapabilityView {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        resolve_all(key, evidence, ledger)
    }))
    .unwrap_or_else(|_| CapabilityPolicy::production_unverified())
}

/// 无兜底的解析本体（`catch_unwind` 内层；调用方保证 key 与证据已就绪）。
fn resolve_all(
    key: &CapabilityKey,
    evidence: &ZcodeInterfaceEvidence,
    ledger: &dyn CapabilityLedger,
) -> CapabilityView {
    let interface = interface_capabilities(evidence);
    // 账本读失败已由实现折成空 Vec（spec §6.6：账本异常 → shipped + 接口候选）。
    let records = ledger.get(key);
    let mut view = CapabilityView::default();
    for capability in ALL_CAPABILITIES {
        let record = records
            .iter()
            .find(|candidate| candidate.capability == capability);
        let flag = flag_for(
            record,
            shipped_state(key, capability),
            interface.get(&capability) == Some(&true),
        );
        assign(&mut view, capability, flag);
    }
    view
}

/// 单项能力解析（spec R2 优先级 + R3 形状叠加）。
fn flag_for(
    record: Option<&CapabilityRecord>,
    shipped: Option<CapabilityState>,
    interface_ok: bool,
) -> CapabilityFlag {
    if let Some(record) = record {
        return match record.state {
            // 确认/候选/未知结果都按候选链放行，但要求本代际形状在场（缺形状不可用）。
            CapabilityState::Confirmed
            | CapabilityState::InterfaceMatched
            | CapabilityState::Inconclusive => {
                if interface_ok {
                    open()
                } else {
                    closed(codes::NEEDS_EVIDENCE)
                }
            }
            // 实机明确失败：优先于候选直到 key 变化；冻结原因码透传 UI
            // （账本写闸保证它是判据标识形状；缺原因码按缺证据保守关闭）。
            CapabilityState::Degraded => CapabilityFlag {
                enabled: false,
                reason_code: Some(
                    record
                        .reason_code
                        .clone()
                        .unwrap_or_else(|| codes::NEEDS_EVIDENCE.to_owned()),
                ),
            },
            // 账本明确不可用。
            CapabilityState::Unsupported => closed(codes::UNSUPPORTED),
        };
    }
    match shipped {
        // shipped 精确命中优先于接口候选；候选/确认仍要求形状在场。
        Some(CapabilityState::Confirmed) | Some(CapabilityState::InterfaceMatched) => {
            if interface_ok {
                open()
            } else {
                closed(codes::NEEDS_EVIDENCE)
            }
        }
        // shipped 明确不可用（合同 false），接口形状在场也不放行。
        Some(CapabilityState::Unsupported) => closed(codes::UNSUPPORTED),
        // shipped 表只产上面三态（防御分支：不认识的组合按缺证据保守关闭）。
        Some(CapabilityState::Degraded) | Some(CapabilityState::Inconclusive) => {
            closed(codes::NEEDS_EVIDENCE)
        }
        // 未知 build（无账本记录）：接口匹配即候选可用（spec §6.4 A 策略）。
        None => {
            if interface_ok {
                open()
            } else {
                closed(codes::NEEDS_EVIDENCE)
            }
        }
    }
}

/// 可用：无原因码（UI 只见可用/不可用，不区分候选/确认）。
fn open() -> CapabilityFlag {
    CapabilityFlag {
        enabled: true,
        reason_code: None,
    }
}

/// 不可用：冻结原因码（`needs_evidence` / `unsupported`）。
fn closed(reason: &str) -> CapabilityFlag {
    CapabilityFlag {
        enabled: false,
        reason_code: Some(reason.to_owned()),
    }
}

/// 按能力枚举写回矩阵字段（八项一一对应，漏项即编译期 match 报错）。
fn assign(view: &mut CapabilityView, capability: Capability, flag: CapabilityFlag) {
    match capability {
        Capability::Send => view.send = flag,
        Capability::SingleSelect => view.single_select = flag,
        Capability::MultiSelect => view.multi_select = flag,
        Capability::FreeText => view.free_text = flag,
        Capability::Interrupt => view.interrupt = flag,
        Capability::ApprovalReply => view.approval_reply = flag,
        Capability::Model => view.model = flag,
        Capability::ThoughtLevel => view.thought_level = flag,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// brief Step 1 逐字用例：未知 build 只开放接口匹配的候选（send 方法在场 →
    /// send 开；approval_reply 方法缺席 → 关）。
    #[test]
    fn unknown_build_opens_only_interface_matched() {
        let key = CapabilityKey::new("macos", "9.9.9.9999", 3, "shape", 1);
        let mut ev = ZcodeInterfaceEvidence::default();
        ev.method_send = true;
        let view = resolve_capabilities(&key, &ev, &MemoryLedger::default());
        assert!(view.send.enabled);
        assert!(!view.approval_reply.enabled);
    }

    /// brief Step 1 逐字用例：账本 degraded 优先于接口候选（候选即使形状在场也不放行）。
    #[test]
    fn degraded_overrides_interface_candidate() {
        let key = CapabilityKey::new("macos", "9.9.9.9999", 3, "shape", 1);
        let mut ev = ZcodeInterfaceEvidence::default();
        ev.method_send = true;
        let ledger = MemoryLedger::with(key.clone(), Capability::Send, CapabilityState::Degraded);
        let view = resolve_capabilities(&key, &ev, &ledger);
        assert!(!view.send.enabled);
    }

    /// 测试账本（brief 指定的内存助手；仅测试用，不是生产接口）。
    struct MemoryLedger {
        records: Mutex<Vec<(CapabilityKey, CapabilityRecord)>>,
    }

    impl Default for MemoryLedger {
        fn default() -> Self {
            Self {
                records: Mutex::new(Vec::new()),
            }
        }
    }

    impl MemoryLedger {
        /// 注入一条无原因码的记录（brief degraded 用例不带原因码）。
        fn with(key: CapabilityKey, capability: Capability, state: CapabilityState) -> Self {
            let ledger = Self::default();
            ledger.records.lock().unwrap().push((
                key,
                CapabilityRecord {
                    capability,
                    state,
                    reason_code: None,
                    evidence: None,
                },
            ));
            ledger
        }

        /// 注入一条带冻结原因码的 degraded 记录（R4 明确失败形态）。
        fn with_degraded(key: CapabilityKey, capability: Capability, reason: &str) -> Self {
            let ledger = Self::default();
            ledger.records.lock().unwrap().push((
                key,
                CapabilityRecord::degraded(capability, reason.to_owned()),
            ));
            ledger
        }

        /// 指定状态记录的语义化别名（inconclusive 用例复用 `with`）。
        fn with_state(key: CapabilityKey, capability: Capability, state: CapabilityState) -> Self {
            Self::with(key, capability, state)
        }
    }

    impl CapabilityLedger for MemoryLedger {
        fn get(&self, key: &CapabilityKey) -> Vec<CapabilityRecord> {
            self.records
                .lock()
                .unwrap()
                .iter()
                .filter(|(stored_key, _)| stored_key == key)
                .map(|(_, record)| record.clone())
                .collect()
        }

        fn put(&self, _key: &CapabilityKey, _record: &CapabilityRecord) -> Result<(), String> {
            Ok(())
        }
    }

    /// 已知 build 的 key（shipped 表精确命中形态）。
    fn known_build_key() -> CapabilityKey {
        CapabilityKey::new("macos", "3.16.2.8769", 3, "shape", 1)
    }

    /// 已知 build：shipped Confirmed/InterfaceMatched 命中且形状在场 → 可用；
    /// shipped Unsupported 的 multi_select/free_text 即使接口形状在场也不可用
    /// （shipped 精确命中优先于接口候选）；形状缺席的项按 needs_evidence 关闭。
    #[test]
    fn known_build_shipped_beats_interface_candidates() {
        let key = known_build_key();
        let mut ev = ZcodeInterfaceEvidence::default();
        ev.method_send = true;
        ev.has_user_input_pending = true;
        ev.has_multi_select = true;
        ev.has_free_text = true;
        let view = resolve_capabilities(&key, &ev, &MemoryLedger::default());
        // shipped Confirmed + 方法在场 → 可用。
        assert!(view.send.enabled);
        assert_eq!(view.send.reason_code, None);
        // shipped InterfaceMatched + 形状在场 → 候选可用（UI 只见可用）。
        assert!(view.single_select.enabled);
        assert_eq!(view.single_select.reason_code, None);
        // shipped Unsupported：合同 false，接口形状在场也不放行，原因码 unsupported。
        assert!(!view.multi_select.enabled);
        assert_eq!(
            view.multi_select.reason_code.as_deref(),
            Some(codes::UNSUPPORTED)
        );
        assert!(!view.free_text.enabled);
        assert_eq!(
            view.free_text.reason_code.as_deref(),
            Some(codes::UNSUPPORTED)
        );
        // 形状/方法缺席：即使 shipped 有正面知识也按缺证据关闭（缺形状不可用）。
        for (name, flag) in [
            ("interrupt", &view.interrupt),
            ("approval_reply", &view.approval_reply),
            ("model", &view.model),
            ("thought_level", &view.thought_level),
        ] {
            assert!(!flag.enabled, "{name} 缺形状时不得开闸");
            assert_eq!(
                flag.reason_code.as_deref(),
                Some(codes::NEEDS_EVIDENCE),
                "{name} 缺形状的原因码必须是 needs_evidence"
            );
        }
    }

    /// 账本 confirmed 覆盖 shipped：single_select 从候选升级为确认可用，
    /// multi_select 覆盖 shipped Unsupported（R4 首次真实动作确认的升级路径）。
    #[test]
    fn ledger_confirmed_overrides_shipped() {
        let key = known_build_key();
        let mut ev = ZcodeInterfaceEvidence::default();
        ev.method_send = true;
        ev.has_user_input_pending = true;
        ev.has_multi_select = true;
        let ledger = MemoryLedger::default();
        ledger.records.lock().unwrap().push((
            key.clone(),
            CapabilityRecord::confirmed(Capability::SingleSelect),
        ));
        ledger.records.lock().unwrap().push((
            key.clone(),
            CapabilityRecord::confirmed(Capability::MultiSelect),
        ));
        let view = resolve_capabilities(&key, &ev, &ledger);
        assert!(view.single_select.enabled);
        assert!(
            view.multi_select.enabled,
            "confirmed 覆盖 shipped Unsupported"
        );
        assert_eq!(view.multi_select.reason_code, None);
    }

    /// 账本 inconclusive 保持候选（不升级、不关闭；形状在场 → 可用）。
    #[test]
    fn ledger_inconclusive_stays_candidate() {
        let key = known_build_key();
        let mut ev = ZcodeInterfaceEvidence::default();
        ev.method_send = true;
        let ledger =
            MemoryLedger::with_state(key.clone(), Capability::Send, CapabilityState::Inconclusive);
        let view = resolve_capabilities(&key, &ev, &ledger);
        assert!(view.send.enabled, "inconclusive 仍是候选，不得关闭");
        assert_eq!(view.send.reason_code, None);
    }

    /// 账本 degraded 覆盖 shipped Confirmed，并把冻结原因码透传给 UI。
    #[test]
    fn ledger_degraded_overrides_shipped_and_passes_reason() {
        let key = known_build_key();
        let mut ev = ZcodeInterfaceEvidence::default();
        ev.method_send = true;
        let ledger = MemoryLedger::with_degraded(key.clone(), Capability::Send, "action_failed");
        let view = resolve_capabilities(&key, &ev, &ledger);
        assert!(!view.send.enabled);
        assert_eq!(view.send.reason_code.as_deref(), Some("action_failed"));
    }

    /// 解析路径异常（账本实现 panic）→ 整体回退 production_unverified() 全关。
    #[test]
    fn ledger_panic_falls_back_to_production_unverified() {
        let key = known_build_key();
        let mut ev = ZcodeInterfaceEvidence::default();
        ev.method_send = true;
        let view = resolve_capabilities(&key, &ev, &PanicLedger);
        assert_eq!(view, CapabilityPolicy::production_unverified());
    }

    /// 会 panic 的账本替身（验证 resolver 的异常兜底面）。
    struct PanicLedger;

    impl CapabilityLedger for PanicLedger {
        fn get(&self, _key: &CapabilityKey) -> Vec<CapabilityRecord> {
            panic!("账本异常");
        }

        fn put(&self, _key: &CapabilityKey, _record: &CapabilityRecord) -> Result<(), String> {
            Ok(())
        }
    }
}
