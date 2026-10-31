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
//! 队列三写（`queue_edit`/`queue_now`/`queue_resume`）的「形状」不是运行时接口
//! 布尔（接口面对队列恒 `false`）。未完成 W0 实机取证前，普通构建一律关闭；只有
//! `zcode-bridge-evidence` debug evidence/probe 构建可按已知 build 的命令条目
//! （R0 §3，见 `key::build_declares_queue_command`）开放 queue_edit/queue_resume 候选。
//! `queue_now` 语义未实测，所有构建恒关闭。
//!
//! reasonCode（UI 只见可用/不可用）：确认档 = `None`；**候选档**（仅队列写）=
//! `interface_matched`（复用账本 state 列的既有冻结词，不是新造错误码）；不可用 =
//! `needs_evidence`（缺证据/缺形状）、`unsupported`（静态知识/账本明确不可用）或
//! 账本 degraded 的冻结原因码。异常兜底（spec §6.6）：解析路径任何 panic →
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

/// 十一项能力的固定清单（声明序无语义；逐项独立解析）。
///
/// 队列三项（`queue_edit`/`queue_now`/`queue_resume`）同样逐项解析。普通构建下均关闭；
/// 仅 `zcode-bridge-evidence` debug evidence/probe 构建可让已知 build 命中命令表的
/// `queue_edit`/`queue_resume` 进入候选档。三者都不得借 `send`/`approval_reply` 的结论开闸。
const ALL_CAPABILITIES: [Capability; 11] = [
    Capability::Send,
    Capability::SingleSelect,
    Capability::MultiSelect,
    Capability::FreeText,
    Capability::Interrupt,
    Capability::ApprovalReply,
    Capability::Model,
    Capability::ThoughtLevel,
    Capability::QueueEdit,
    Capability::QueueNow,
    Capability::QueueResume,
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
        let shipped = shipped_state(key, capability);
        // 队列写：接口形状位恒 `false`（`interface_capabilities` 不认队列），
        // 「形状在场」改由 **build 命令条目**提供——shipped 表命中候选/确认条目
        // 即视为判据在场；否则永远落不到候选档。
        let evidence_ok = if is_queue_write(capability) {
            matches!(
                shipped,
                Some(CapabilityState::Confirmed | CapabilityState::InterfaceMatched)
            )
        } else {
            interface.get(&capability) == Some(&true)
        };
        let flag = flag_for(capability, record, shipped, evidence_ok);
        assign(&mut view, capability, flag);
    }
    view
}

/// 队列三写（撤回/重排、立即执行、恢复）。
fn is_queue_write(capability: Capability) -> bool {
    matches!(
        capability,
        Capability::QueueEdit | Capability::QueueNow | Capability::QueueResume
    )
}

/// 单项能力解析（spec R2 优先级 + R3 形状叠加）。
fn flag_for(
    capability: Capability,
    record: Option<&CapabilityRecord>,
    shipped: Option<CapabilityState>,
    evidence_ok: bool,
) -> CapabilityFlag {
    if let Some(record) = record {
        return match record.state {
            // 确认/候选/未知结果都按候选链放行，但要求本代际形状在场（缺形状不可用）。
            CapabilityState::Confirmed
            | CapabilityState::InterfaceMatched
            | CapabilityState::Inconclusive => {
                if evidence_ok {
                    open_with(capability, record.state)
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
        Some(state @ (CapabilityState::Confirmed | CapabilityState::InterfaceMatched)) => {
            if evidence_ok {
                open_with(capability, state)
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
        // 队列写走不到这里：其 evidence_ok 恒 false（shipped 为 None）。
        None => {
            if evidence_ok {
                open_with(capability, CapabilityState::Confirmed)
            } else {
                closed(codes::NEEDS_EVIDENCE)
            }
        }
    }
}

/// 可用：按证据等级分档——非队列能力只有「可用」一档（无原因码）；队列写在
/// `Confirmed` 之外的候选态带 [`CANDIDATE_REASON`]，让 W4 能区分「候选可点」与
/// 「实机确认」。
fn open_with(capability: Capability, state: CapabilityState) -> CapabilityFlag {
    if is_queue_write(capability) && state != CapabilityState::Confirmed {
        candidate()
    } else {
        open()
    }
}

/// 候选档原因码：复用账本 `state` 列的**既有冻结词** `interface_matched`
/// （`database/dao/zcode_capability.rs` 的 `state_to_str`、`database/schema.rs`
/// 词表、`hub.rs` 的 `state_word` 同词），不是 `codes` 词表里的新错误码
/// ——`codes` 保持冻结、不新增。
const CANDIDATE_REASON: &str = "interface_matched";

/// 可用（确认档）：无原因码（UI 只见可用/不可用，不区分候选/确认）。
fn open() -> CapabilityFlag {
    CapabilityFlag {
        enabled: true,
        reason_code: None,
    }
}

/// 可用（候选档）：`enabled=true` + 候选原因码。
///
/// **候选 ≠ 实机验证**：它只表示静态证据（build 命令条目 / 接口形状）匹配，
/// 写命令是否被宿主接受由运行时新鲜 `availability` 与真实 ACK 回答。
fn candidate() -> CapabilityFlag {
    CapabilityFlag {
        enabled: true,
        reason_code: Some(CANDIDATE_REASON.to_owned()),
    }
}

/// 不可用：冻结原因码（`needs_evidence` / `unsupported`）。
fn closed(reason: &str) -> CapabilityFlag {
    CapabilityFlag {
        enabled: false,
        reason_code: Some(reason.to_owned()),
    }
}

/// 按能力枚举写回矩阵字段（十一项一一对应，漏项即编译期 match 报错）。
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
        Capability::QueueEdit => view.queue_edit = flag,
        Capability::QueueNow => view.queue_now = flag,
        Capability::QueueResume => view.queue_resume = flag,
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
        let ev = ZcodeInterfaceEvidence {
            method_send: true,
            ..Default::default()
        };
        let view = resolve_capabilities(&key, &ev, &MemoryLedger::default());
        assert!(view.send.enabled);
        assert!(!view.approval_reply.enabled);
    }

    /// brief Step 1 逐字用例：账本 degraded 优先于接口候选（候选即使形状在场也不放行）。
    #[test]
    fn degraded_overrides_interface_candidate() {
        let key = CapabilityKey::new("macos", "9.9.9.9999", 3, "shape", 1);
        let ev = ZcodeInterfaceEvidence {
            method_send: true,
            ..Default::default()
        };
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
    /// shipped 与接口对 multi_select/free_text 均按候选（形状在场即可用；三轮 B）
    /// （shipped 精确命中优先于接口候选）；形状缺席的项按 needs_evidence 关闭。
    #[test]
    fn known_build_shipped_beats_interface_candidates() {
        let key = known_build_key();
        let ev = ZcodeInterfaceEvidence {
            method_send: true,
            has_user_input_pending: true,
            has_multi_select: true,
            has_free_text: true,
            ..Default::default()
        };
        let view = resolve_capabilities(&key, &ev, &MemoryLedger::default());
        // shipped Confirmed + 方法在场 → 可用。
        assert!(view.send.enabled);
        assert_eq!(view.send.reason_code, None);
        // shipped InterfaceMatched + 形状在场 → 候选可用（UI 只见可用）。
        assert!(view.single_select.enabled);
        assert_eq!(view.single_select.reason_code, None);
        // 三轮范围裁决：「未实测」≠「明确不支持」——multi/free 与 single 同为
        // shipped InterfaceMatched，形状在场即候选可用（UI 只见可用，原因码空）。
        assert!(view.multi_select.enabled);
        assert_eq!(view.multi_select.reason_code, None);
        assert!(view.free_text.enabled);
        assert_eq!(view.free_text.reason_code, None);
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

    /// 三轮 B 回归矩阵：已知/未知 build × 八项能力——接口形状齐备时八项**全部**
    /// 候选开放（已知 build 的 shipped 项按其证据等级，未实测项按候选）；任一目标
    /// 形状缺席时对应项按真实状态关闭（needs_evidence）。「未实测」与「明确不支持」
    /// 分离：本矩阵不再断言任何 Unsupported（账本 Unsupported 记录才产生它）。
    #[test]
    fn eight_capabilities_matrix_across_builds_and_shapes() {
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
        let unknown = CapabilityKey::new("macos", "9.9.9.9999", 3, "shape", 1);
        let known = known_build_key();
        for (label, key) in [("unknown build", &unknown), ("known build", &known)] {
            let view = resolve_capabilities(key, &full, &MemoryLedger::default());
            for name in [
                "send",
                "single_select",
                "multi_select",
                "free_text",
                "interrupt",
                "approval_reply",
                "model",
                "thought_level",
            ] {
                let flag = match name {
                    "send" => &view.send,
                    "single_select" => &view.single_select,
                    "multi_select" => &view.multi_select,
                    "free_text" => &view.free_text,
                    "interrupt" => &view.interrupt,
                    "approval_reply" => &view.approval_reply,
                    "model" => &view.model,
                    _ => &view.thought_level,
                };
                assert!(
                    flag.enabled,
                    "{label} 全形状齐备时 {name} 必须开放：{:?}",
                    flag.reason_code
                );
                assert_eq!(
                    flag.reason_code, None,
                    "{label} 开放项不得携带原因码（UI 只见可用）：{name}"
                );
            }
        }

        // 目标形状逐一缺席：对应能力关闭（needs_evidence），其余不受牵连。
        type ShapeCase = (&'static str, Box<dyn Fn(&mut ZcodeInterfaceEvidence)>);
        let cases: &[ShapeCase] = &[
            (
                "send 缺方法",
                Box::new(|e: &mut ZcodeInterfaceEvidence| {
                    e.method_send = false;
                }),
            ),
            (
                "single 缺卡",
                Box::new(|e: &mut ZcodeInterfaceEvidence| {
                    e.has_user_input_pending = false;
                }),
            ),
            (
                "multi 缺形状",
                Box::new(|e: &mut ZcodeInterfaceEvidence| {
                    e.has_multi_select = false;
                }),
            ),
            (
                "free 缺形状",
                Box::new(|e: &mut ZcodeInterfaceEvidence| {
                    e.has_free_text = false;
                }),
            ),
            (
                "interrupt 缺回合",
                Box::new(|e: &mut ZcodeInterfaceEvidence| {
                    e.has_active_work = false;
                }),
            ),
            (
                "approval 缺审批卡",
                Box::new(|e: &mut ZcodeInterfaceEvidence| {
                    e.has_permission_pending = false;
                }),
            ),
            (
                "model 缺目录",
                Box::new(|e: &mut ZcodeInterfaceEvidence| {
                    e.has_model_catalog = false;
                }),
            ),
            (
                "thought 缺档位",
                Box::new(|e: &mut ZcodeInterfaceEvidence| {
                    e.has_thought_levels = false;
                }),
            ),
        ];
        for (label, mutate) in cases {
            let mut ev = full;
            mutate(&mut ev);
            let view = resolve_capabilities(&unknown, &ev, &MemoryLedger::default());
            let flag = match *label {
                "send 缺方法" => &view.send,
                "single 缺卡" => &view.single_select,
                "multi 缺形状" => &view.multi_select,
                "free 缺形状" => &view.free_text,
                "interrupt 缺回合" => &view.interrupt,
                "approval 缺审批卡" => &view.approval_reply,
                "model 缺目录" => &view.model,
                _ => &view.thought_level,
            };
            assert!(!flag.enabled, "{label} 时对应能力必须关闭");
            assert_eq!(
                flag.reason_code.as_deref(),
                Some(codes::NEEDS_EVIDENCE),
                "{label} 关闭原因码必须是 needs_evidence"
            );
            // 兄弟能力不受牵连：multi 缺形状不影响 single/free（同卡异题独立判据）。
            if *label == "multi 缺形状" {
                assert!(view.single_select.enabled && view.free_text.enabled);
            }
            if *label == "free 缺形状" {
                assert!(view.single_select.enabled && view.multi_select.enabled);
            }
        }
    }

    /// 命令形状全齐（八项接口都可用）时，命中证据表的 build 上 `queue_edit` /
    /// `queue_resume` 解析为**候选档**：`enabled=true` + 候选原因码。
    ///
    /// 依据：`key.rs` 的 `shipped_state` 对已知 build 的队列条目返回
    /// `InterfaceMatched`（命令名 `deleteQueueItem`/`reorderQueueItem`/`setAutoDrain`
    /// 在 R0 §3 命令表内）。**候选 ≠ 实机验证**——运行时仍由新鲜 `availability`
    /// 二次校验（`command::execute_queue_write`）。
    #[cfg(feature = "zcode-bridge-evidence")]
    #[test]
    fn queue_edit_is_candidate_on_evidenced_build() {
        let key = known_build_key();
        let view = resolve_capabilities(
            &key,
            &ZcodeInterfaceEvidence::default(),
            &MemoryLedger::default(),
        );
        assert!(view.queue_edit.enabled, "命令条目命中 → 撤回/修改候选可点");
        assert_eq!(
            view.queue_edit.reason_code.as_deref(),
            Some(CANDIDATE_REASON),
            "候选档必须带 interface_matched 原因码（区别于实机确认）"
        );
        assert!(
            view.queue_resume.enabled,
            "setAutoDrain 命中 → 恢复候选可点"
        );
        assert_eq!(
            view.queue_resume.reason_code.as_deref(),
            Some(CANDIDATE_REASON)
        );
    }

    #[cfg(not(feature = "zcode-bridge-evidence"))]
    #[test]
    fn queue_edit_and_resume_stay_closed_outside_w0_evidence_build() {
        let view = resolve_capabilities(
            &known_build_key(),
            &ZcodeInterfaceEvidence::default(),
            &MemoryLedger::default(),
        );
        assert!(!view.queue_edit.enabled);
        assert_eq!(
            view.queue_edit.reason_code.as_deref(),
            Some(codes::NEEDS_EVIDENCE)
        );
        assert!(!view.queue_resume.enabled);
        assert_eq!(
            view.queue_resume.reason_code.as_deref(),
            Some(codes::NEEDS_EVIDENCE)
        );
    }

    /// 同一 build、同一命令表下 `queue_now` 仍关闭：`sendQueuedNow` 的「立即执行」
    /// 与「引导当前回合」是否等价没有实机证据 → 不给候选。
    #[test]
    fn queue_now_stays_needs_evidence() {
        let key = known_build_key();
        let full = ZcodeInterfaceEvidence {
            method_send: true,
            has_active_work: true,
            ..Default::default()
        };
        let view = resolve_capabilities(&key, &full, &MemoryLedger::default());
        assert!(view.send.enabled, "前提：send 在方法在场时可用");
        assert!(!view.queue_now.enabled, "queue_now 不得进入候选层");
        assert_eq!(
            view.queue_now.reason_code.as_deref(),
            Some(codes::NEEDS_EVIDENCE),
            "queue_now 关闭原因码必须是 needs_evidence"
        );
    }

    /// 未知 build / 非取证平台：三写能力**全部**关闭（命令条目只认已知 build，
    /// 不因接口形状齐备而放宽）。
    #[test]
    fn queue_candidates_require_evidenced_build() {
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
        for key in [
            CapabilityKey::new("macos", "9.9.9.9999", 3, "shape", 1),
            CapabilityKey::new("linux", "3.16.2.8769", 3, "shape", 1),
            CapabilityKey::new("macos", "3.16.2.8769", 4, "shape", 1),
        ] {
            let view = resolve_capabilities(&key, &full, &MemoryLedger::default());
            for (name, flag) in [
                ("queue_edit", &view.queue_edit),
                ("queue_now", &view.queue_now),
                ("queue_resume", &view.queue_resume),
            ] {
                assert!(!flag.enabled, "{name} 不得在未知 build 上开闸");
                assert_eq!(
                    flag.reason_code.as_deref(),
                    Some(codes::NEEDS_EVIDENCE),
                    "{name} 关闭原因码必须是 needs_evidence"
                );
            }
        }
    }

    /// send 开 ≠ 队列开：已知 build 上队列候选来自命令条目，不来自 `send`；未知
    /// build 上 `send` 即使可用，三写能力仍全关（不借授权）。
    #[test]
    fn queue_write_capabilities_never_borrow_send() {
        let full = ZcodeInterfaceEvidence {
            method_send: true,
            ..Default::default()
        };
        let unknown = CapabilityKey::new("macos", "9.9.9.9999", 3, "shape", 1);
        let view = resolve_capabilities(&unknown, &full, &MemoryLedger::default());
        assert!(view.send.enabled, "前提：send 可用");
        for (name, flag) in [
            ("queue_edit", &view.queue_edit),
            ("queue_now", &view.queue_now),
            ("queue_resume", &view.queue_resume),
        ] {
            assert!(!flag.enabled, "{name} 不得借 send 开闸");
            assert_eq!(
                flag.reason_code.as_deref(),
                Some(codes::NEEDS_EVIDENCE),
                "{name} 关闭原因码必须是 needs_evidence"
            );
        }

        // 已知 build：普通构建下 queueEdit/Resume 仍关闭；静态命令名不替代 W0 实机证据。
        let known = resolve_capabilities(
            &known_build_key(),
            &ZcodeInterfaceEvidence::default(),
            &MemoryLedger::default(),
        );
        assert!(!known.send.enabled, "前提：无方法时 send 不可用");
        #[cfg(feature = "zcode-bridge-evidence")]
        assert!(known.queue_edit.enabled && known.queue_resume.enabled);
        #[cfg(not(feature = "zcode-bridge-evidence"))]
        assert!(!known.queue_edit.enabled && !known.queue_resume.enabled);
        assert!(!known.queue_now.enabled);
    }

    /// 账本 `confirmed`（首次真实动作有界观察的落账结果）把队列三写升到确认档：
    /// `enabled=true` 且**无原因码**（候选档的原因码随之消失）。
    #[cfg(feature = "zcode-bridge-evidence")]
    #[test]
    fn ledger_confirmed_promotes_queue_candidate_to_confirmed() {
        let key = known_build_key();
        let ledger = MemoryLedger::default();
        ledger.records.lock().unwrap().push((
            key.clone(),
            CapabilityRecord::confirmed(Capability::QueueEdit),
        ));
        let view = resolve_capabilities(&key, &ZcodeInterfaceEvidence::default(), &ledger);
        assert!(view.queue_edit.enabled);
        assert_eq!(view.queue_edit.reason_code, None, "确认档无原因码");
    }

    #[cfg(not(feature = "zcode-bridge-evidence"))]
    #[test]
    fn queue_ledger_confirmation_stays_closed_without_w0_evidence_build() {
        let key = known_build_key();
        let ledger = MemoryLedger::default();
        ledger.records.lock().unwrap().push((
            key.clone(),
            CapabilityRecord::confirmed(Capability::QueueEdit),
        ));
        let view = resolve_capabilities(&key, &ZcodeInterfaceEvidence::default(), &ledger);
        assert!(!view.queue_edit.enabled);
        assert_eq!(
            view.queue_edit.reason_code.as_deref(),
            Some(codes::NEEDS_EVIDENCE)
        );
    }

    /// 账本 confirmed 覆盖 shipped：single_select 从候选升级为确认可用；
    /// multi_select 覆盖 shipped 候选（R4 首次真实动作确认的升级路径）。
    #[test]
    fn ledger_confirmed_overrides_shipped() {
        let key = known_build_key();
        let ev = ZcodeInterfaceEvidence {
            method_send: true,
            has_user_input_pending: true,
            has_multi_select: true,
            ..Default::default()
        };
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
        let ev = ZcodeInterfaceEvidence {
            method_send: true,
            ..Default::default()
        };
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
        let ev = ZcodeInterfaceEvidence {
            method_send: true,
            ..Default::default()
        };
        let ledger = MemoryLedger::with_degraded(key.clone(), Capability::Send, "action_failed");
        let view = resolve_capabilities(&key, &ev, &ledger);
        assert!(!view.send.enabled);
        assert_eq!(view.send.reason_code.as_deref(), Some("action_failed"));
    }

    /// 解析路径异常（账本实现 panic）→ 整体回退 production_unverified() 全关。
    #[test]
    fn ledger_panic_falls_back_to_production_unverified() {
        let key = known_build_key();
        let ev = ZcodeInterfaceEvidence {
            method_send: true,
            ..Default::default()
        };
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
