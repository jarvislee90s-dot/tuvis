//! #172/T9a：对外可见的机器码词表（snake_case）+ 用户可展示文案（invoke 码形态）。
//!
//! 两套词表的关系（**单一出处**，别在别处再写一遍）：
//! * **snake_case 机器码**（本模块的 `*_CODE` 常量）：远程 API 的 `reasonCode`
//!   取值——回执 `reasonCode`（投递/审批）与探测事实 `probeStatus.reasonCode`
//!   都用它。前端按码取文案（`errors` 段）。
//! * **大写 invoke 码**（本模块 `*_MESSAGE` 常量的前缀）：兔维斯既有的 invoke
//!   错误码体系（约定全文见 `crate::errcode`）——locale 键 = `errors.` +
//!   camelCase(大写码)（`DESKTOP_BUILD_UNVERIFIED` → `errors.desktopBuildUnverified`）；
//!   `scripts/check-error-codes.mjs` 与 locale **双向对账**（Rust 有码 → locale 必须
//!   有键；locale 有键 → Rust 必须有码）。
//!
//! 纪律：
//! * 词表**只增不改**：既有 `capability_unverified` / `capability_unavailable` 的
//!   语义一字不动（见 `remote::codex_ipc`），本表只登记**语义确实不同**的新码；
//! * 动态码只有一个（`probe_failed:<stage>`）：前缀用常量拼，stage 取值域限
//!   [`PROBE_STAGES`]（P0..P3），**不散落**；
//! * 本模块**只读**：不查账本、不建连、不写任何状态——「事实」由调用方取，
//!   本模块只负责「码 ↔ 文案 ↔ 只读事实」的映射（纯函数）。

use crate::database::dao::codex_ipc_probe::ProbeState;
use crate::services::codex_desktop_ipc::types::{ProbeStatus, ProbeStatusState};

// ============================================================
// 机器码（snake_case；远程 API reasonCode 的取值）
// ============================================================

/// 该 Desktop build **未经真机验证**（尚未探测通过）。
///
/// 落点：探测只读事实 `probeStatus.reasonCode`——该 build 在账本里**没有任何**
/// 探测记录时（`state = idle`）。语义 ≠ `capability_unverified`（那是「方法已知、
/// 能力未验证」的**写**侧拒发码），这里是**读**侧的「这台 build 还没验过」。
pub const DESKTOP_BUILD_UNVERIFIED: &str = "desktop_build_unverified";

/// 探测在 P0..P3 某级**明确失败**的动态码前缀（完整码 = `probe_failed:<stage>`）。
///
/// 为什么要动态：失败级（P0 连接 / P1 owner / P2 following / P3 结构解析）决定
/// 用户该做什么——P0/P1 多半是「桌面端没起/没人持有」，P3 才是「协议形态变了」。
pub const PROBE_FAILED_PREFIX: &str = "probe_failed";

/// 探测中**怀疑协议漂移**（桌面端返回的结构与预期不一致）。
///
/// 落点：探测只读事实 `probeStatus.reasonCode`（账本行 `inconclusive` 且错误码
/// 就是它时）；词源在 `probe/runner.rs`（P2 静默 → 补发 status-requested 仍静默）。
pub const PROTOCOL_DRIFT_SUSPECTED: &str = "protocol_drift_suspected";

/// 探测**不确定**（有 attempt、无结论、非判死）——按退避重试，绝不判死。
///
/// 落点：探测只读事实 `probeStatus.reasonCode`（账本行 `inconclusive` 且错误码
/// 不是漂移疑似时）。
pub const PROBE_INCONCLUSIVE: &str = "probe_inconclusive";

/// 投递结果**未知**（已发出但结果未知）——**禁止**自动重试、禁止自动 fallback。
///
/// 落点：回执 `reasonCode`（兼容发送超时/断链；`remote::codex_ipc` 的
/// `COMPAT_SEND_DELIVERY_UNKNOWN` 复用本常量）。
pub const DELIVERY_UNKNOWN: &str = "delivery_unknown";

/// 审批决策**已受理但 5s 内未见生效**（`decision_confirm_ms` 窗口超时）。
///
/// 落点：回执 `reasonCode`（`remote::codex_ipc` 的 `confirm_decision_gone`）。
pub const DECISION_UNCONFIRMED: &str = "decision_unconfirmed";

/// 逐操作策略：未知集合（老载荷缺 `effectiveOperations`）——"还不知道"，不是永久不支持。
pub const OPERATION_UNKNOWN_POLICY: &str = "operation_unknown_policy";

/// 逐操作策略：本阶段未实现该操作（F09—F11／无专用 RPC）。
pub const OPERATION_NOT_IMPLEMENTED: &str = "operation_not_implemented";

/// 逐操作策略：当前目标不可归因／连接不在场（状态过期，动作撤回）。
pub const OPERATION_TARGET_STALE: &str = "operation_target_stale";

/// 逐操作策略：自动适配已关闭，未 shipped 的适配动作撤回。
pub const OPERATION_AUTO_ADAPT_OFF: &str = "operation_auto_adapt_off";

/// 逐操作策略：当前运行包的契约未匹配（或契约识别失败）。
pub const OPERATION_CONTRACT_UNMATCHED: &str = "operation_contract_unmatched";

/// 逐操作策略：没有任何当前证据（未识别／未验证）。
pub const OPERATION_NO_EVIDENCE: &str = "operation_no_evidence";

/// 逐操作策略：线程正在运行（异步正式答题需空闲）。
pub const OPERATION_RUNTIME_ACTIVE: &str = "operation_runtime_active";

/// 逐操作策略：运行态未知（断链/状态过期），不猜。
pub const OPERATION_RUNTIME_UNKNOWN: &str = "operation_runtime_unknown";

/// 逐操作策略：另有原生阻塞请求在场（先处理该请求）。
pub const OPERATION_BLOCKING_PENDING: &str = "operation_blocking_pending";

/// 逐操作策略：队列处于暂停保护（需用户显式恢复或本条先发）。
pub const OPERATION_QUEUE_PAUSED: &str = "operation_queue_paused";

/// 异步题的相关答复范围（题目 → 当前）不完整/不可读：保持 Unknown，不清卡。
pub const ASYNC_HISTORY_INCOMPLETE: &str = "async_answer_history_incomplete";

/// 预留码：没有可打断的活动回合。**实际语义（2026-10-29 修正过时表述）**：
/// 逐操作策略对 Interrupt **不以「有无活动回合」作门**（门 = 证据/适配/目标在场；
/// 安全阀是 `expectedTurnId`）——本码当前不出自策略词表；端点侧「无可打断回合」
/// 以 HTTP 409 `no_expected_turn` 与审计词 `no_active_turn` 表达（`codex_ipc.rs`）。
pub const OPERATION_NO_ACTIVE_TURN: &str = "operation_no_active_turn";

/// 合法探测级（动态码 `probe_failed:<stage>` 的 stage 取值域）。
///
/// 账本 `probe_stage` 列只可能出现这四个词；其它词（历史脏数据 / 未来扩展）
/// **不猜**——不进动态码。
pub const PROBE_STAGES: [&str; 4] = ["P0", "P1", "P2", "P3"];

// ============================================================
// 用户可展示文案（invoke 码形态：`CODE: 中文原文`）
// ============================================================
//
// 中文原文的写法固定为「**发生了什么 + 用户该怎么做**」：前端按
// `errors.<camelCase(CODE)>` 取本地化文案（英文见 `src/i18n/locales/en.json`），
// 中文原文保留在码后供日志与「未知码原样回退」使用。

/// `errors.desktopBuildUnverified` 的中文原文。
pub const DESKTOP_BUILD_UNVERIFIED_MESSAGE: &str = "DESKTOP_BUILD_UNVERIFIED: 该 Codex 桌面端版本尚未经真机验证，探测通过前不能远程操控；请在电脑端打开该会话触发探测，或升级到已验证版本";

/// `errors.probeFailed` 的中文原文（探测级另见 `probeStatus.stage`）。
pub const PROBE_FAILED_MESSAGE: &str = "PROBE_FAILED: 只读探测明确失败，该桌面端版本的协议形态与兔维斯不兼容；请到桌面端处理，或升级/回退 Codex 客户端";

/// `errors.protocolDriftSuspected` 的中文原文。
pub const PROTOCOL_DRIFT_SUSPECTED_MESSAGE: &str = "PROTOCOL_DRIFT_SUSPECTED: 探测疑似遇到协议漂移，桌面端返回的结构与兔维斯预期不一致；请重启桌面端后重试，若持续出现请升级客户端";

/// `errors.probeInconclusive` 的中文原文。
pub const PROBE_INCONCLUSIVE_MESSAGE: &str = "PROBE_INCONCLUSIVE: 只读探测暂无线索（桌面端可能未启动、无人持有或正忙），兔维斯稍后会自动重试；期间请到桌面端查看该会话";

/// `errors.deliveryUnknown` 的中文原文。
pub const DELIVERY_UNKNOWN_MESSAGE: &str = "DELIVERY_UNKNOWN: 投递结果未知，这条消息可能已送达也可能没有；请到桌面端核对后再决定是否重发，兔维斯不会自动重发";

/// `errors.asyncAnswerHistoryIncomplete` 的中文原文。
pub const ASYNC_HISTORY_INCOMPLETE_MESSAGE: &str = "ASYNC_ANSWER_HISTORY_INCOMPLETE: 该题到当前之间的答复范围尚未完整加载，无法确认是否已作答；答案会保留，请刷新或稍后重试，兔维斯不会自动代答";
/// `errors.operationUnknownPolicy` 的中文原文。
pub const OPERATION_UNKNOWN_POLICY_MESSAGE: &str = "OPERATION_UNKNOWN_POLICY: 当前客户端还没有该会话的能力事实（投影缺字段）；请刷新后重试，兔维斯不会据此猜测可用能力";
/// `errors.operationNotImplemented` 的中文原文。
pub const OPERATION_NOT_IMPLEMENTED_MESSAGE: &str =
    "OPERATION_NOT_IMPLEMENTED: 该操作本阶段尚未接入（没有已核验的协议出口）；请到 Desktop 端处理";
/// `errors.operationTargetStale` 的中文原文。
pub const OPERATION_TARGET_STALE_MESSAGE: &str =
    "OPERATION_TARGET_STALE: 连接或目标状态已过期，操作已撤回；请刷新状态后重试";
/// `errors.operationAutoAdaptOff` 的中文原文。
pub const OPERATION_AUTO_ADAPT_OFF_MESSAGE: &str =
    "OPERATION_AUTO_ADAPT_OFF: 自动适配已关闭，新版本的适配操作已撤回；可在适配详情中重新开启";
/// `errors.operationContractUnmatched` 的中文原文。
pub const OPERATION_CONTRACT_UNMATCHED_MESSAGE: &str = "OPERATION_CONTRACT_UNMATCHED: 当前 Desktop 运行包的契约与已核验分支不匹配，该操作暂不可用；可到桌面端处理或查看适配详情";
/// `errors.operationNoEvidence` 的中文原文。
pub const OPERATION_NO_EVIDENCE_MESSAGE: &str =
    "OPERATION_NO_EVIDENCE: 该操作在当前 Desktop 上还没有证据（未识别或未验证），暂不可用";
/// `errors.operationRuntimeActive` 的中文原文。
pub const OPERATION_RUNTIME_ACTIVE_MESSAGE: &str =
    "OPERATION_RUNTIME_ACTIVE: 当前回合正在运行；答案可先保存，空闲后再由你点击提交";
/// `errors.operationRuntimeUnknown` 的中文原文。
pub const OPERATION_RUNTIME_UNKNOWN_MESSAGE: &str =
    "OPERATION_RUNTIME_UNKNOWN: 运行状态暂时无法确认；请刷新状态后再试";
/// `errors.operationBlockingPending` 的中文原文。
pub const OPERATION_BLOCKING_PENDING_MESSAGE: &str =
    "OPERATION_BLOCKING_PENDING: 另有一张原生请求正在等待处理；请先处理该请求";
/// `errors.operationQueuePaused` 的中文原文。
pub const OPERATION_QUEUE_PAUSED_MESSAGE: &str =
    "OPERATION_QUEUE_PAUSED: 队列处于暂停保护；请明确恢复队列，或选择仅本条先发";
/// `errors.operationNoActiveTurn` 的中文原文。
pub const OPERATION_NO_ACTIVE_TURN_MESSAGE: &str =
    "OPERATION_NO_ACTIVE_TURN: 当前没有可打断的活动回合";

/// `errors.decisionUnconfirmed` 的中文原文。
pub const DECISION_UNCONFIRMED_MESSAGE: &str = "DECISION_UNCONFIRMED: 审批决策已被受理但尚未确认生效，请到桌面端核对该请求是否已消失；必要时可终止本轮";

// ============================================================
// 映射（纯函数）
// ============================================================

/// 动态码 `probe_failed:<stage>`（stage 必须是 P0..P3 之一）。
pub fn probe_failed_code(stage: &str) -> String {
    format!("{PROBE_FAILED_PREFIX}:{stage}")
}

/// 机器码 → 用户可展示文案（**单一出处**）。
///
/// 动态码 `probe_failed:<stage>` 归到 `PROBE_FAILED`（探测级另见
/// `probeStatus.stage`）；其余按精确码取。未知码 → `None`（**不猜**，调用方
/// 原样展示机器码即可）。
pub fn message_of(machine_code: &str) -> Option<&'static str> {
    if machine_code == PROBE_FAILED_PREFIX
        || machine_code.starts_with(&format!("{PROBE_FAILED_PREFIX}:"))
    {
        return Some(PROBE_FAILED_MESSAGE);
    }
    match machine_code {
        DESKTOP_BUILD_UNVERIFIED => Some(DESKTOP_BUILD_UNVERIFIED_MESSAGE),
        PROTOCOL_DRIFT_SUSPECTED => Some(PROTOCOL_DRIFT_SUSPECTED_MESSAGE),
        PROBE_INCONCLUSIVE => Some(PROBE_INCONCLUSIVE_MESSAGE),
        DELIVERY_UNKNOWN => Some(DELIVERY_UNKNOWN_MESSAGE),
        DECISION_UNCONFIRMED => Some(DECISION_UNCONFIRMED_MESSAGE),
        ASYNC_HISTORY_INCOMPLETE => Some(ASYNC_HISTORY_INCOMPLETE_MESSAGE),
        OPERATION_UNKNOWN_POLICY => Some(OPERATION_UNKNOWN_POLICY_MESSAGE),
        OPERATION_NOT_IMPLEMENTED => Some(OPERATION_NOT_IMPLEMENTED_MESSAGE),
        OPERATION_TARGET_STALE => Some(OPERATION_TARGET_STALE_MESSAGE),
        OPERATION_AUTO_ADAPT_OFF => Some(OPERATION_AUTO_ADAPT_OFF_MESSAGE),
        OPERATION_CONTRACT_UNMATCHED => Some(OPERATION_CONTRACT_UNMATCHED_MESSAGE),
        OPERATION_NO_EVIDENCE => Some(OPERATION_NO_EVIDENCE_MESSAGE),
        OPERATION_RUNTIME_ACTIVE => Some(OPERATION_RUNTIME_ACTIVE_MESSAGE),
        OPERATION_RUNTIME_UNKNOWN => Some(OPERATION_RUNTIME_UNKNOWN_MESSAGE),
        OPERATION_BLOCKING_PENDING => Some(OPERATION_BLOCKING_PENDING_MESSAGE),
        OPERATION_QUEUE_PAUSED => Some(OPERATION_QUEUE_PAUSED_MESSAGE),
        OPERATION_NO_ACTIVE_TURN => Some(OPERATION_NO_ACTIVE_TURN_MESSAGE),
        _ => None,
    }
}

/// 账本结论 → 探测只读事实（**纯函数**：不读账本、不连 Desktop、不写任何状态）。
///
/// 口径：
/// * `probed`（含只读值 `verified`）→ `probed`，无失败码；
/// * `failed` → `failed`，码取 `probe_failed:<stage>`（stage 合法时），
///   stage 取不到时退回账本里的错误码；
/// * `inconclusive` → `inconclusive`，错误码是漂移疑似就用它，否则用
///   `probe_inconclusive`（**不判死**：这类结论按退避重试）；
/// * `unknown`（无行 / 词异常）→ `idle` + **无码**（取不到，不猜）。
pub fn probe_status_from(
    state: ProbeState,
    stage: Option<&str>,
    error_code: Option<&str>,
) -> ProbeStatus {
    // stage 只认 P0..P3（历史脏数据 / 未来扩展不进动态码）。
    let stage = stage
        .map(str::trim)
        .filter(|s| PROBE_STAGES.contains(s))
        .map(str::to_string);
    match state {
        ProbeState::Probed | ProbeState::Verified => ProbeStatus {
            state: ProbeStatusState::Probed,
            stage,
            reason_code: None,
        },
        ProbeState::Failed => ProbeStatus {
            state: ProbeStatusState::Failed,
            stage: stage.clone(),
            reason_code: stage
                .as_deref()
                .map(probe_failed_code)
                .or_else(|| error_code.map(str::to_string)),
        },
        ProbeState::Inconclusive => ProbeStatus {
            state: ProbeStatusState::Inconclusive,
            stage,
            reason_code: Some(if error_code == Some(PROTOCOL_DRIFT_SUSPECTED) {
                PROTOCOL_DRIFT_SUSPECTED.to_string()
            } else {
                PROBE_INCONCLUSIVE.to_string()
            }),
        },
        ProbeState::Unknown => ProbeStatus::idle(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 六个码都在词表里，且各有一句可展示文案（不漏码）。
    #[test]
    fn every_machine_code_has_a_display_message() {
        for code in [
            DESKTOP_BUILD_UNVERIFIED,
            PROTOCOL_DRIFT_SUSPECTED,
            PROBE_INCONCLUSIVE,
            DELIVERY_UNKNOWN,
            DECISION_UNCONFIRMED,
        ] {
            let msg = message_of(code).unwrap_or_else(|| panic!("{code} 缺文案"));
            assert!(
                msg.starts_with(&code.to_uppercase()),
                "{code} 的文案必须以大写码开头（invoke 码形态）：{msg}"
            );
        }
        // 动态码归到 PROBE_FAILED
        assert_eq!(
            message_of(&probe_failed_code("P2")),
            Some(PROBE_FAILED_MESSAGE)
        );
        assert_eq!(message_of("probe_failed"), Some(PROBE_FAILED_MESSAGE));
        // 未知码不猜
        assert_eq!(message_of("capability_unverified"), None);
    }

    /// 动态码只由常量前缀拼出，stage 取值域限 P0..P3。
    #[test]
    fn probe_failed_code_is_built_from_the_constant_prefix() {
        assert_eq!(probe_failed_code("P0"), "probe_failed:P0");
        assert_eq!(probe_failed_code("P3"), "probe_failed:P3");
        assert!(probe_failed_code("P3").starts_with(PROBE_FAILED_PREFIX));
    }

    /// 账本三态 → 只读事实的映射（纯函数）。
    #[test]
    fn ledger_states_map_to_probe_status() {
        let probed = probe_status_from(ProbeState::Probed, Some("P3"), None);
        assert_eq!(probed.state, ProbeStatusState::Probed);
        assert_eq!(probed.stage.as_deref(), Some("P3"));
        assert_eq!(probed.reason_code, None);

        let failed = probe_status_from(ProbeState::Failed, Some("P2"), Some("method_not_found"));
        assert_eq!(failed.state, ProbeStatusState::Failed);
        assert_eq!(failed.reason_code.as_deref(), Some("probe_failed:P2"));

        // stage 取不到 → 退回账本里的错误码
        let failed_no_stage = probe_status_from(ProbeState::Failed, None, Some("method_not_found"));
        assert_eq!(
            failed_no_stage.reason_code.as_deref(),
            Some("method_not_found")
        );

        let inconclusive = probe_status_from(
            ProbeState::Inconclusive,
            Some("P2"),
            Some("no-client-found"),
        );
        assert_eq!(inconclusive.state, ProbeStatusState::Inconclusive);
        assert_eq!(
            inconclusive.reason_code.as_deref(),
            Some("probe_inconclusive"),
            "非漂移疑似的不确定 → 通用不确定码（不判死）"
        );

        let drift = probe_status_from(
            ProbeState::Inconclusive,
            Some("P2"),
            Some(PROTOCOL_DRIFT_SUSPECTED),
        );
        assert_eq!(
            drift.reason_code.as_deref(),
            Some("protocol_drift_suspected")
        );

        // 未知词 → idle + 无码（不猜）
        let unknown = probe_status_from(ProbeState::Unknown, None, None);
        assert_eq!(unknown.state, ProbeStatusState::Idle);
        assert_eq!(unknown.reason_code, None);

        // 非法 stage 不进动态码（历史脏数据不猜）
        let dirty = probe_status_from(ProbeState::Failed, Some("P9"), None);
        assert_eq!(dirty.stage, None);
        assert_eq!(dirty.reason_code, None);
    }
}
