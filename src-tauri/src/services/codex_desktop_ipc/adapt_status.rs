//! 适配状态 DTO（S1 登记边界，总计划「接口登记（实施记录）」的权威形状）。
//!
//! **为什么单独一个文件**：S2（可发送判据）/ S5（界面三态徽标）/ R1（诊断摘要）都
//! 只消费这里的类型，不另造第二套；装配逻辑（读 adapter 运行态）在
//! `adapter::DesktopIpcAdapter::adapt_status`。
//!
//! **核心纪律（S0 §3.1 用户裁决）**：DTO 必须同时表达两件不得混同的事——
//! * [`AttemptableSet`]「**可尝试**」：当前档位能力位（兼容档 send/state=true =
//!   允许按协议守卫尝试），随连接断开回落 closed；
//! * [`EvidencedSet`]「**已取证**」：持久成功证据（shipped 表 / 只读探测账本 /
//!   schema 决策词证据），不随断链、超时回落。
//!
//! 破坏性能力门（打断、审批）与证据呈现（徽标三态）不得把两者混同：S5 徽标看
//! evidenced，发送按钮看 attemptable ∧ 设置开关；本 DTO 结构上把两维度分开。

use serde::Serialize;

use super::types::ProbeStatus;

/// 「可尝试」维度：当前档位能力位（**不等于已取证**）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct AttemptableSet {
    /// 远程发送是否允许尝试（自动适配关 → 未知 build 恒 false；已验证 build 不受影响）。
    pub send: bool,
    /// 只读跟随/读取是否允许尝试（关闭自动适配不回撤只读——已发动作仍可收尾核对）。
    pub state: bool,
}

/// 「已取证」维度：持久成功证据（不随断链/超时/重启回落）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct EvidencedSet {
    /// 读链有成功证据（shipped 表 / 账本 `state` 行证据）。
    pub state: bool,
    /// 写（发送受理）有成功证据（账本 `send` 行证据；未知 build 只有真受理过才有）。
    pub send: bool,
    /// 审批决策词有 schema 证据（未知 build；决策词持久保存，跨重启复用）。
    pub schema: bool,
}

/// schema 决策词证据摘要（已取证时随身携带；S5 详情页 / R1 诊断消费）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SchemaEvidenceDto {
    /// 官方 schema 内容指纹（文件名+大小的 sha256；归因用）。
    pub fingerprint: String,
    /// command 审批已取证的决策词（schema 顺序）。
    pub command_decisions: Vec<String>,
    /// file 审批已取证的决策词。
    pub file_decisions: Vec<String>,
    /// 取证时间（毫秒）。
    pub evidenced_at: i64,
}

/// 最近失败观测（S1：与成功证据分离的一条线；暂时故障不判死）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FailureObservation {
    /// 账本能力词（`state` / `send`）。
    pub capability: String,
    /// 观测词：`failed`（明确失败）| `inconclusive`（不确定，可退避重试）。
    pub state: String,
    /// 最后尝试的探测级（`P0`..`P3`；发送观测无级次 → `null`）。
    pub stage: Option<String>,
    /// 机器错误码（词表见 `reason_code`）。
    pub error_code: Option<String>,
    /// 观测时间（毫秒）。
    pub observed_at: Option<i64>,
    /// 退避到点（仅 `inconclusive`；到点前不自动重试）。
    pub next_retry_at: Option<i64>,
}

/// T08：**移动端适配详情**（F08）——系统/Tuvis/Desktop 版本、逐能力状态、证据来源、
/// 最近失败与首次提示。**不含**包路径、原始源码或任何正文（诊断最小面）。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexAdaptDetails {
    /// 本机 Desktop 会话 id（授权校验后的回显）。
    pub session_id: String,
    /// 系统版本（OS + 版本；诊断用，不含机器名）。
    pub system_version: String,
    /// Tuvis 版本。
    pub tuvis_version: String,
    /// Desktop build（读不出 → `null`）。
    pub desktop_build: Option<String>,
    /// 自动适配开关生效值。
    pub auto_adapt_enabled: bool,
    /// 首次提示是否已展示（与桌面设置同源；不随 build 重置）。
    pub hint_seen: bool,
    /// 逐操作有效集合（与卡片/写入口同一事实）。
    pub effective_operations: super::capability::EffectiveOperations,
    /// 最近失败观测（与成功证据分离）。
    pub last_failure: Option<FailureObservation>,
}

/// 适配状态 DTO（S1 权威形状；S2/S5/R1 只消费它）。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexAdaptStatus {
    /// 自动适配设置**生效值**（缺省开；仅显式 true/false 尊重——旧「兼容发送（实验）」
    /// 开关的缺值/false 不作为本设置的缺省来源）。
    pub auto_adapt_enabled: bool,
    /// 用户是否显式写过该设置（`null` = 从未写过 → 缺省开；一旦写过即尊重）。
    pub auto_adapt_explicit: Option<bool>,
    /// 平台（归一化小写）。
    pub platform: String,
    /// Desktop build（读不出/多义 → `null`）。
    pub desktop_build: Option<String>,
    /// 完整探测指纹 `platform|build|cli_id|probe_epoch`（跨重启复用键；尚未归因 →
    /// `null`。连接 epoch **不在**键里——它只在途隔离，不是复用键）。
    pub probe_key: Option<String>,
    /// 当前连接代际（在途结果隔离键；每次重连递增，**不用于**跨重启复用）。
    pub connection_epoch: u64,
    pub attemptable: AttemptableSet,
    pub evidenced: EvidencedSet,
    /// schema 决策词证据（已取证时非空）。
    pub schema_evidence: Option<SchemaEvidenceDto>,
    /// 最近失败观测（与成功证据分离；`null` = 无失败观测）。
    pub last_failure: Option<FailureObservation>,
    /// 探测只读事实（`probing`/`probed`/`inconclusive`/`failed`/`idle`；既有语义原样）。
    pub probe_status: ProbeStatus,
}
