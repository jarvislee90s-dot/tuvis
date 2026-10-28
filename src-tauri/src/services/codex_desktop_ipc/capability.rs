//! 逐操作能力（F01）：操作键、契约目标与契约证据类型。
//!
//! 设计依据：`docs/engineering/specs/2026-10-26-Codex-F01-F08阶段需求与设计.md`
//! §3.1／§3.2 与执行计划 T01／T02。
//!
//! **本模块的边界**：这里只定义"操作"与"证据"的**类型**（T01），以及读取契约证据的
//! 注入点 [`DesktopContractReader`]；逐操作策略计算（`OperationPolicy`）、持久证据与
//! `EffectiveOperations` 在 T02 落地。T01 不产生任何可操作结论，也不发写帧。
//!
//! 纪律（spec §3.2）：
//! * 每个操作**独立**判定；方法名存在、相邻 build、其他平台成功都不足以放行；
//! * 契约证据只含结构性规则（方法名／版本／参数与响应形状／语义规则），
//!   **不存会话正文、命令、路径或凭据**；
//! * 契约证据的复用键 = 完整探测指纹（`probe_key`）+ 包内容指纹，连接代际
//!   （`connection_epoch`）只做在途隔离，不参与跨重启复用。

use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use serde::{Deserialize, Serialize};

use super::probe::fingerprint::ProbeKey;
use super::types::ProtocolError;

/// 契约规则版本（与 P0–P3 只读探测的 [`super::probe::fingerprint::PROBE_EPOCH`] **分开**）。
///
/// 规则一变（新增/收紧结构匹配、改变语义要求）就 **+1**：同一条包内容在不同规则版本
/// 下的结论不能互相当成证据复用（`codex_ipc_contract_evidence` 主键含规则版本）。
pub const CONTRACT_RULE_VERSION: u32 = 1;

/// 逐操作键（spec §3.1：消息、命令审批、文件审批、阻塞答题、异步空闲答题、
/// 请求级取消、回合打断、权限与模式分别判断）。
///
/// 序列化用 snake_case（持久化／HTTP 同一词形，前端只在 DTO 里转 camelCase 字段名）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexOperation {
    /// 普通消息（`thread-follower-start-turn` v2）。
    Send,
    /// 命令审批决策（`thread-follower-command-approval-decision`）。
    CommandApproval,
    /// 文件变更审批决策（`thread-follower-file-approval-decision`）。
    FileChangeApproval,
    /// 原生阻塞问答（`thread-follower-submit-user-input`）。
    BlockingInput,
    /// 异步 Question 的空闲正式答题（start-turn 承载已核验信封）。
    AsyncAnswerIdle,
    /// 定向回合打断（`thread-follower-interrupt-turn` v4 + expectedTurnId）。
    Interrupt,
    /// 单请求取消（本阶段无已核验专用 RPC → 恒不放行）。
    RequestCancel,
    /// 独立权限请求批准（F09—F11，本阶段不接入）。
    PermissionsApproval,
    /// 读模型设置（本阶段不接入）。
    ModeRead,
    /// 写模型设置（本阶段不接入）。
    ModeSet,
    /// 读线程权限（本阶段不接入）。
    PermissionsRead,
    /// 写线程权限（本阶段不接入）。
    PermissionsSet,
}

impl CodexOperation {
    /// 全部操作键（稳定顺序 = 声明顺序，供详情页遍历与测试穷举）。
    pub const ALL: [Self; 12] = [
        Self::Send,
        Self::CommandApproval,
        Self::FileChangeApproval,
        Self::BlockingInput,
        Self::AsyncAnswerIdle,
        Self::Interrupt,
        Self::RequestCancel,
        Self::PermissionsApproval,
        Self::ModeRead,
        Self::ModeSet,
        Self::PermissionsRead,
        Self::PermissionsSet,
    ];

    /// 稳定字符串词（持久化／HTTP／诊断共用；**不要**在别处另抄一份）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Send => "send",
            Self::CommandApproval => "command_approval",
            Self::FileChangeApproval => "file_change_approval",
            Self::BlockingInput => "blocking_input",
            Self::AsyncAnswerIdle => "async_answer_idle",
            Self::Interrupt => "interrupt",
            Self::RequestCancel => "request_cancel",
            Self::PermissionsApproval => "permissions_approval",
            Self::ModeRead => "mode_read",
            Self::ModeSet => "mode_set",
            Self::PermissionsRead => "permissions_read",
            Self::PermissionsSet => "permissions_set",
        }
    }

    /// 字符串词 → 操作键（未知词 → `None`：不猜、不新增）。
    pub fn parse(word: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|op| op.as_str() == word)
    }

    /// 本阶段**已实现**的操作（未接入项必须在策略里显式返回"未接入"，
    /// 不能因为没出现在契约证据里就让前端猜）。
    ///
    /// 范围来源：spec §1.3（F09—F11 的 mode/permissions 不新增写能力）。
    pub fn is_implemented_in_phase(self) -> bool {
        !matches!(
            self,
            Self::RequestCancel
                | Self::PermissionsApproval
                | Self::ModeRead
                | Self::ModeSet
                | Self::PermissionsRead
                | Self::PermissionsSet
        )
    }
}

/// 契约识别目标：**只**来自官方运行身份（进程 bundle / MSIX 包 + 随包 CLI），
/// 绝不接受 HTTP 输入（spec §3.2 准入第 1 条：可归因的当前目标）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractTarget {
    /// 完整探测指纹（`platform|build|cli_id|probe_epoch`）。
    pub probe_key: ProbeKey,
    /// 连接代际（在途隔离；旧代际结果不得安装到新目标）。
    pub connection_epoch: u64,
    /// 官方运行包内的 `app.asar` 绝对路径（只读）。
    pub package_path: PathBuf,
    /// 随包 CLI 可执行文件（只用于身份/版本归因，本任务不执行它）。
    pub cli_path: PathBuf,
}

impl ContractTarget {
    /// 账本/证据复用键（与 `ProbeKey::to_key` 同源）。
    pub fn target_key(&self) -> String {
        self.probe_key.to_key()
    }
}

/// 单操作的契约匹配结论（结构性规则，**不含会话正文**）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractMatch {
    /// IPC 方法名（如 `thread-follower-submit-user-input`）。
    pub method: String,
    /// 已核验契约分支的基础版本（方法版本表里的值）。
    pub base_version: u32,
    /// 请求形状规则（如 `conversationId+requestId+response`）。
    pub request_rule: String,
    /// 响应形状规则（如 `ok`）。
    pub response_rule: String,
    /// 语义规则（如 `interrupt v4 requires expectedTurnId`）。
    pub semantic_rule: String,
}

/// 一次官方运行包的契约证据（T01 产物；T02 据此算逐操作策略并持久化）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopContractEvidence {
    /// 探测指纹（复用键的第一段）。
    pub probe_key: String,
    /// 包内容 SHA-256（同一 build 的包内容变了 → 证据不再同一份）。
    pub package_sha256: String,
    /// 契约规则版本（[`CONTRACT_RULE_VERSION`]）。
    pub rule_version: u32,
    /// 逐操作匹配结论；**未出现的操作 = 当前包未匹配**（不是"支持"）。
    pub matches: BTreeMap<CodexOperation, ContractMatch>,
    /// 取证时包文件大小/ mtime（R17：重启复用前核对**当前包内容**；None = 未记录）。
    pub package_size: Option<u64>,
    pub package_mtime_ms: Option<i64>,
}

impl DesktopContractEvidence {
    /// 该操作是否有当前包的契约匹配。
    pub fn matched(&self, operation: CodexOperation) -> bool {
        self.matches.contains_key(&operation)
    }
}

/// 契约读取器（T02 注入 adapter；生产实现是 [`ProductionContractReader`]，
/// 测试用受控 reader，**不真实扫描／连接本机 App**）。
pub trait DesktopContractReader: Send + Sync {
    fn read(
        &self,
        target: ContractTarget,
    ) -> Pin<Box<dyn Future<Output = Result<DesktopContractEvidence, ProtocolError>> + Send>>;
}

/// 生产实现：把阻塞只读的包解析放进 `spawn_blocking`，不阻塞 async 运行时。
#[derive(Debug, Default, Clone, Copy)]
pub struct ProductionContractReader;

impl DesktopContractReader for ProductionContractReader {
    fn read(
        &self,
        target: ContractTarget,
    ) -> Pin<Box<dyn Future<Output = Result<DesktopContractEvidence, ProtocolError>> + Send>> {
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                super::probe::desktop_contract::read_desktop_contract(&target)
            })
            .await
            .map_err(|e| ProtocolError::InvalidState(format!("contract reader task failed: {e}")))?
        })
    }
}

// ============================================================
// T02：逐操作证据与统一有效策略
// ============================================================

/// 证据来源（spec §3.1 的三档可解释来源；`None` = 无证据）。
///
/// 序列化 snake_case（与 `CodexOperation` 同风格；HTTP DTO 由 remote 层转 camelCase）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSource {
    /// 无任何当前证据（未识别／未验证）。
    None,
    /// 该 build 的精确 shipped 证据（历史真机验证档；逐能力，不批量制造）。
    Shipped,
    /// 当前运行包的结构契约匹配（本阶段新路径：README／ASAR 有限结构匹配）。
    ContractMatched,
    /// 本机对该操作的**真实成功**记录（持久成功证据）。
    ObservedSuccess,
    /// 该 build 的官方 schema 已取证决策词（未知 build 的审批词来源）。
    SchemaMatched,
}

/// 单操作的统一状态（**后端唯一事实**；前端与 HTTP 写入口都只消费它）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationStatus {
    /// 本阶段是否实现了该操作（未接入项显式 false，前端不猜）。
    pub implemented: bool,
    /// 当前是否允许发起该操作（仍会在写出边界再次复核）。
    pub allowed: bool,
    /// 是否依赖「自动适配」活门（关闭即撤回）；shipped 精确证据不依赖。
    pub requires_auto_adapt: bool,
    /// 证据来源。
    pub evidence: EvidenceSource,
    /// 当前不允许的机器码（`reason_code` 词表；允许时为 `None`）。
    pub reason_code: Option<String>,
}

/// 逐操作有效集合（并入 `ThreadProjection.effectiveOperations`）。
///
/// `policy_revision` 是**事实指纹**（同一组事实恒等）：SSE 下游据此判断"策略是否
/// 真的变了"，避免静默期重复发布（T02 验收「静默期间零新增投影」）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectiveOperations {
    pub policy_revision: u64,
    pub operations: BTreeMap<CodexOperation, OperationStatus>,
}

impl Default for EffectiveOperations {
    fn default() -> Self {
        Self::unknown()
    }
}

impl EffectiveOperations {
    /// 未知集合：全部操作 = 未接入／不允许（老载荷缺字段的保守缺省）。
    ///
    /// **语义**：缺字段不等于"全部可用"，也不等于"永久不支持"——它只是"这台
    /// 客户端还没有这份事实"。
    pub fn unknown() -> Self {
        let operations = CodexOperation::ALL
            .into_iter()
            .map(|operation| {
                (
                    operation,
                    OperationStatus {
                        implemented: false,
                        allowed: false,
                        requires_auto_adapt: false,
                        evidence: EvidenceSource::None,
                        reason_code: Some(super::reason_code::OPERATION_UNKNOWN_POLICY.into()),
                    },
                )
            })
            .collect();
        Self {
            policy_revision: 0,
            operations,
        }
    }

    /// 该操作当前是否允许（未知操作 → false）。
    pub fn allows(&self, operation: CodexOperation) -> bool {
        self.operations
            .get(&operation)
            .is_some_and(|status| status.implemented && status.allowed)
    }

    /// 该操作的状态（未知 → `None`）。
    pub fn get(&self, operation: CodexOperation) -> Option<&OperationStatus> {
        self.operations.get(&operation)
    }

    /// 事实指纹：对**稳定序列化**的逐操作事实做 FNV-1a。
    ///
    /// 为什么不用计数器：计数器会让"事实没变"的重复求值也换 revision，造成 SSE
    /// 自激（T02「静默期间零新增投影」）。同一组事实 → 同一 revision。
    pub fn revision_of(operations: &BTreeMap<CodexOperation, OperationStatus>) -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a offset basis
        for (operation, status) in operations {
            for byte in operation.as_str().bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x100_0000_01b3);
            }
            hash ^= u64::from(status.implemented);
            hash = hash.wrapping_mul(0x100_0000_01b3);
            hash ^= u64::from(status.allowed);
            hash = hash.wrapping_mul(0x100_0000_01b3);
            hash ^= u64::from(status.requires_auto_adapt);
            hash = hash.wrapping_mul(0x100_0000_01b3);
            hash ^= match status.evidence {
                EvidenceSource::None => 0,
                EvidenceSource::Shipped => 1,
                EvidenceSource::ContractMatched => 2,
                EvidenceSource::ObservedSuccess => 3,
                EvidenceSource::SchemaMatched => 4,
            };
            hash = hash.wrapping_mul(0x100_0000_01b3);
            if let Some(code) = &status.reason_code {
                for byte in code.bytes() {
                    hash ^= u64::from(byte);
                    hash = hash.wrapping_mul(0x100_0000_01b3);
                }
            }
        }
        hash
    }
}

/// 策略求值上下文：**全部是已装载的事实**（投影临界区只读它，不读 DB、不跑包扫描）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OperationContext {
    /// 当前目标可归因且连接在场（断开 → 所有非只读动作撤回）。
    pub target_fresh: bool,
    /// 自动适配开关生效值（关闭 → 未 shipped 的适配动作立即撤回）。
    pub auto_adapt_enabled: bool,
    /// 线程运行态（`idle` / `active` / `failed` / `unknown`；镜像事实）。
    pub runtime_status: String,
    /// 当前是否有原生阻塞请求在场（含 pending + idle 的错配形态）。
    pub blocking_pending: bool,
    /// 队列是否处于暂停保护。
    pub queue_paused: bool,
    /// 读链/只读探测是否已有成功证据（**不得**解锁任何写操作）。
    pub read_evidence: bool,
    /// 该 build 的**逐能力** shipped 证据（精确到操作，不批量推导）。
    pub shipped: BTreeMap<CodexOperation, bool>,
    /// 本机该操作的**真实成功**证据（持久；暂时失败不抹去）。
    pub observed: BTreeMap<CodexOperation, bool>,
    /// 官方 schema 对该操作决策词的取证（仅命令/文件审批；未知 build 的合法词来源）。
    pub schema: BTreeMap<CodexOperation, bool>,
    /// 当前运行包的契约匹配（逐操作；空 = 未匹配）。
    pub contract: BTreeMap<CodexOperation, ContractMatch>,
    /// 契约识别失败的原因码（`None` = 未尝试或成功）。
    pub contract_error: Option<String>,
}

impl OperationContext {
    fn shipped_for(&self, operation: CodexOperation) -> bool {
        self.shipped.get(&operation).copied().unwrap_or(false)
    }
    fn observed_for(&self, operation: CodexOperation) -> bool {
        self.observed.get(&operation).copied().unwrap_or(false)
    }
    fn schema_for(&self, operation: CodexOperation) -> bool {
        self.schema.get(&operation).copied().unwrap_or(false)
    }
    fn contract_for(&self, operation: CodexOperation) -> Option<&ContractMatch> {
        self.contract.get(&operation)
    }
    fn runtime_is(&self, expected: &str) -> bool {
        self.runtime_status.eq_ignore_ascii_case(expected)
    }
}

/// 逐操作策略（**无 IO 纯计算**，可在投影临界区内调用）。
#[derive(Debug, Default, Clone, Copy)]
pub struct OperationPolicy;

impl OperationPolicy {
    /// 求值：每个操作独立判定；任何"没有证据"都落到不允许（不猜）。
    pub fn evaluate(&self, context: &OperationContext) -> EffectiveOperations {
        let mut operations = BTreeMap::new();
        for operation in CodexOperation::ALL {
            operations.insert(operation, self.evaluate_one(operation, context));
        }
        let policy_revision = EffectiveOperations::revision_of(&operations);
        EffectiveOperations {
            policy_revision,
            operations,
        }
    }

    fn evaluate_one(
        &self,
        operation: CodexOperation,
        context: &OperationContext,
    ) -> OperationStatus {
        use super::reason_code as codes;

        if !operation.is_implemented_in_phase() {
            return OperationStatus {
                implemented: false,
                allowed: false,
                requires_auto_adapt: false,
                evidence: EvidenceSource::None,
                reason_code: Some(codes::OPERATION_NOT_IMPLEMENTED.into()),
            };
        }

        let shipped = context.shipped_for(operation);
        let observed = context.observed_for(operation);
        let contract = context.contract_for(operation).is_some();
        let schema = context.schema_for(operation);
        let evidence = if shipped {
            EvidenceSource::Shipped
        } else if observed {
            EvidenceSource::ObservedSuccess
        } else if contract {
            EvidenceSource::ContractMatched
        } else if schema {
            EvidenceSource::SchemaMatched
        } else {
            EvidenceSource::None
        };

        // 普通消息沿用既有发送门：不依赖新契约识别（T02 验收）。
        if operation == CodexOperation::Send {
            return OperationStatus {
                implemented: true,
                allowed: context.target_fresh,
                requires_auto_adapt: false,
                evidence,
                reason_code: (!context.target_fresh)
                    .then(|| codes::OPERATION_TARGET_STALE.to_string()),
            };
        }

        // 适配活门：只有**精确 shipped 证据**不受自动适配开关影响；
        // 契约匹配必须活门开启；本机真实成功证据保留（已发动作仍可只读核对，
        // 但发起新动作仍要求活门——成功证据不等于当前可写）。
        let requires_auto_adapt = !shipped;
        // H01：observed 也必须受自动适配活门约束（真实成功 != 当前可写；关闭适配后
        // 新动作零帧），只有**精确 shipped** 不受开关影响。
        let gate_open = shipped || (context.auto_adapt_enabled && (observed || contract || schema));
        if !gate_open {
            let reason = if (contract || observed || schema) && !context.auto_adapt_enabled {
                codes::OPERATION_AUTO_ADAPT_OFF
            } else if context.contract_error.is_some() {
                codes::OPERATION_CONTRACT_UNMATCHED
            } else if evidence == EvidenceSource::None {
                codes::OPERATION_NO_EVIDENCE
            } else {
                codes::OPERATION_CONTRACT_UNMATCHED
            };
            return OperationStatus {
                implemented: true,
                allowed: false,
                requires_auto_adapt,
                evidence,
                reason_code: Some(reason.into()),
            };
        }

        // 目标/状态门。
        let blocked_reason = if !context.target_fresh {
            Some(codes::OPERATION_TARGET_STALE)
        } else if !context.auto_adapt_enabled && !shipped && !observed {
            Some(codes::OPERATION_AUTO_ADAPT_OFF)
        } else {
            match operation {
                // 原生阻塞问答：运行中/等待态都可正式响应（不被 F03 空闲规则误伤）。
                CodexOperation::BlockingInput => None,
                // 审批：只约束"该请求仍在"，不约束运行态。
                CodexOperation::CommandApproval | CodexOperation::FileChangeApproval => None,
                // 定向打断：**不以运行态作为门**（spec §10）——镜像运行态可能滞后于
                // 桌面真实回合；真正的安全阀是 `expectedTurnId`（缺省零指令）＋
                // Desktop 对不匹配回合的静默 no-op。证据门在上方已判。
                CodexOperation::Interrupt => None,
                // 异步正式答题：只在安全空闲提交；暂停需用户显式选择（T09/T07）。
                CodexOperation::AsyncAnswerIdle => {
                    if context.blocking_pending {
                        Some(codes::OPERATION_BLOCKING_PENDING)
                    } else if context.queue_paused {
                        Some(codes::OPERATION_QUEUE_PAUSED)
                    } else if context.runtime_is("idle") {
                        None
                    } else if context.runtime_status.is_empty() || context.runtime_is("unknown") {
                        Some(codes::OPERATION_RUNTIME_UNKNOWN)
                    } else {
                        Some(codes::OPERATION_RUNTIME_ACTIVE)
                    }
                }
                _ => Some(codes::OPERATION_NOT_IMPLEMENTED),
            }
        };

        OperationStatus {
            implemented: true,
            allowed: blocked_reason.is_none(),
            requires_auto_adapt,
            evidence,
            reason_code: blocked_reason.map(str::to_string),
        }
    }
}

/// T02：契约证据的**持久存储**（生产 = `codex_ipc_contract_evidence` 表的 DAO；
/// 测试 = 内存实现）。
///
/// 为什么是 trait：adapter 是协议/策略层，不得直接摸 DB（与 `ProbeLedger` 同款
/// 纪律）；持久面的可替换点就在这里。
pub trait ContractEvidenceStore: Send + Sync {
    /// 读某 `probe_key` + 规则版本的逐操作证据行；无行 → `None`（读失败同样
    /// `None`——读不到就重识别，不猜）。
    fn load(
        &self,
        probe_key: &str,
        rule_version: u32,
    ) -> Option<Vec<crate::database::dao::codex_ipc_probe::StoredContractEvidence>>;
    /// 落账一份**当前目标**的契约证据（幂等 upsert）。
    fn store(&self, evidence: &DesktopContractEvidence, at_ms: i64);
}

/// T02：本机**真实成功**证据的来源（按操作逐项；生产由 remote 层从动作账本汇总，
/// 测试注入固定表）。
///
/// 语义：`true` = 本机对该操作有过真实成功（跨重启保留）；**暂时失败不得抹去**。
/// H01：**完整目标身份**（probe key + 包内容 SHA；规则版本在查询时限定）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedTarget {
    pub probe_key: String,
    pub package_sha256: String,
}

pub trait ObservedOperationsSource: Send + Sync {
    /// 该完整目标下已确认成功的操作；`None`/未知目标 → 空表。
    fn observed(&self, target: Option<&ObservedTarget>) -> BTreeMap<CodexOperation, bool>;
}

impl DesktopContractEvidence {
    /// 从持久行重建证据：**同一 probe_key 下只允许一个包内容指纹**；多指纹
    /// （同 build 被替换过包）→ `None`（不猜哪份是当前的，重新识别）。
    ///
    /// 未知操作词 / 版本不符的行被忽略（前向兼容：新操作不因老行出现而放行）。
    pub fn from_rows(
        rows: &[crate::database::dao::codex_ipc_probe::StoredContractEvidence],
        expected_rule_version: u32,
    ) -> Option<Self> {
        let probe_key = rows.first()?.probe_key.clone();
        let mut shas: Vec<&str> = rows.iter().map(|row| row.package_sha256.as_str()).collect();
        shas.sort_unstable();
        shas.dedup();
        if shas.len() != 1 {
            return None;
        }
        let mut matches = BTreeMap::new();
        for row in rows {
            if row.rule_version != expected_rule_version {
                continue;
            }
            let Some(operation) = CodexOperation::parse(&row.operation) else {
                continue;
            };
            matches.insert(
                operation,
                ContractMatch {
                    method: row.method.clone(),
                    base_version: row.base_version,
                    request_rule: row.request_rule.clone(),
                    response_rule: row.response_rule.clone(),
                    semantic_rule: row.semantic_rule.clone(),
                },
            );
        }
        Some(Self {
            probe_key,
            package_sha256: shas[0].to_string(),
            rule_version: expected_rule_version,
            matches,
            package_size: rows
                .first()
                .and_then(|row| row.package_size)
                .map(|v| v as u64),
            package_mtime_ms: rows.first().and_then(|row| row.package_mtime_ms),
        })
    }

    /// R17：重启复用前的**当前包内容核对**。
    ///
    /// 持久行记录取证时的 `(size, mtime)`；当前包任一不同（或老行未记录）→ 不安装，
    /// 重新识别。仅比 SHA 更便宜（不重扫 640MB 包），又能在包被替换时失效。
    pub fn pinned_to_current_package(self, current: Option<(u64, i64)>) -> Option<Self> {
        let (size, mtime) = current?;
        if self.package_size == Some(size) && self.package_mtime_ms == Some(mtime) {
            Some(self)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::codex_desktop_ipc::reason_code as codes;

    fn match_for(operation: CodexOperation) -> ContractMatch {
        ContractMatch {
            method: match operation {
                CodexOperation::Send | CodexOperation::AsyncAnswerIdle => {
                    "thread-follower-start-turn".into()
                }
                CodexOperation::Interrupt => "thread-follower-interrupt-turn".into(),
                _ => "thread-follower-submit-user-input".into(),
            },
            base_version: 1,
            request_rule: "conversationId".into(),
            response_rule: "ok".into(),
            semantic_rule: "rule".into(),
        }
    }

    fn fresh_contract() -> BTreeMap<CodexOperation, ContractMatch> {
        CodexOperation::ALL
            .into_iter()
            .filter(|op| op.is_implemented_in_phase())
            .map(|op| (op, match_for(op)))
            .collect()
    }

    fn idle_context() -> OperationContext {
        OperationContext {
            target_fresh: true,
            auto_adapt_enabled: true,
            runtime_status: "idle".into(),
            blocking_pending: false,
            queue_paused: false,
            read_evidence: true,
            shipped: BTreeMap::new(),
            observed: BTreeMap::new(),
            schema: BTreeMap::new(),
            contract: fresh_contract(),
            contract_error: None,
        }
    }

    /// 只读证据不得解锁任何写操作（读链成功 ≠ 可写）。
    #[test]
    fn f08p_t02_readonly_evidence_does_not_unlock_write() {
        let mut context = idle_context();
        context.contract.clear();
        context.read_evidence = true;
        context.shipped.insert(CodexOperation::Send, true);
        let ops = OperationPolicy.evaluate(&context);
        assert!(ops.allows(CodexOperation::Send), "普通消息沿用既有发送门");
        for op in [
            CodexOperation::CommandApproval,
            CodexOperation::FileChangeApproval,
            CodexOperation::BlockingInput,
            CodexOperation::AsyncAnswerIdle,
            CodexOperation::Interrupt,
        ] {
            assert!(!ops.allows(op), "{op:?} 不得被只读证据解锁");
            assert_eq!(
                ops.get(op).unwrap().evidence,
                EvidenceSource::None,
                "{op:?} 无写证据"
            );
        }
    }

    /// 命令审批的成功证据不得外溢成文件审批证据（逐能力独立）。
    #[test]
    fn f08p_t02_command_success_does_not_evidence_file() {
        let mut context = idle_context();
        context.contract.clear();
        context
            .observed
            .insert(CodexOperation::CommandApproval, true);
        let ops = OperationPolicy.evaluate(&context);
        assert!(ops.allows(CodexOperation::CommandApproval));
        assert_eq!(
            ops.get(CodexOperation::CommandApproval).unwrap().evidence,
            EvidenceSource::ObservedSuccess
        );
        assert!(!ops.allows(CodexOperation::FileChangeApproval));
        assert_eq!(
            ops.get(CodexOperation::FileChangeApproval)
                .unwrap()
                .reason_code
                .as_deref(),
            Some(codes::OPERATION_NO_EVIDENCE)
        );
    }

    /// 普通消息不依赖新契约识别（否则每次 Desktop 升级都会先断发消息）。
    #[test]
    fn f08p_t02_send_remains_attemptable_without_new_contract() {
        let mut context = idle_context();
        context.contract.clear();
        context.contract_error = Some("package_unreadable".into());
        context.auto_adapt_enabled = false;
        context.shipped.insert(CodexOperation::Send, true);
        let ops = OperationPolicy.evaluate(&context);
        assert!(ops.allows(CodexOperation::Send));
        assert_eq!(
            ops.get(CodexOperation::Send).unwrap().evidence,
            EvidenceSource::Shipped
        );
        assert!(!ops.get(CodexOperation::Send).unwrap().requires_auto_adapt);
        // 断链时撤回（状态过期不允许派发）。
        context.target_fresh = false;
        let offline = OperationPolicy.evaluate(&context);
        assert!(!offline.allows(CodexOperation::Send));
        assert_eq!(
            offline
                .get(CodexOperation::Send)
                .unwrap()
                .reason_code
                .as_deref(),
            Some(codes::OPERATION_TARGET_STALE)
        );
    }

    /// 关闭自动适配：契约匹配动作立即撤回；精确 shipped 证据保持不动。
    #[test]
    fn f08p_t02_auto_adapt_off_retracts_contract_but_keeps_shipped() {
        let mut context = idle_context();
        context.auto_adapt_enabled = false;
        context
            .shipped
            .insert(CodexOperation::FileChangeApproval, true);
        let ops = OperationPolicy.evaluate(&context);
        assert_eq!(
            ops.get(CodexOperation::AsyncAnswerIdle)
                .unwrap()
                .reason_code
                .as_deref(),
            Some(codes::OPERATION_AUTO_ADAPT_OFF)
        );
        assert!(!ops.allows(CodexOperation::AsyncAnswerIdle));
        assert!(
            ops.allows(CodexOperation::FileChangeApproval),
            "shipped 不受活门影响"
        );
        assert!(
            !ops.get(CodexOperation::FileChangeApproval)
                .unwrap()
                .requires_auto_adapt
        );
        assert!(
            ops.get(CodexOperation::AsyncAnswerIdle)
                .unwrap()
                .requires_auto_adapt
        );
    }

    /// 异步正式答题只在安全空闲；运行中/阻塞在场/暂停分别给出不同原因码。
    #[test]
    fn f08p_t02_async_idle_state_gates() {
        let mut context = idle_context();
        assert!(OperationPolicy
            .evaluate(&context)
            .allows(CodexOperation::AsyncAnswerIdle));

        context.runtime_status = "active".into();
        let active = OperationPolicy.evaluate(&context);
        assert!(!active.allows(CodexOperation::AsyncAnswerIdle));
        assert_eq!(
            active
                .get(CodexOperation::AsyncAnswerIdle)
                .unwrap()
                .reason_code
                .as_deref(),
            Some(codes::OPERATION_RUNTIME_ACTIVE)
        );
        // 阻塞请求在场优先处理该请求（pending + idle 也不例外）。
        context.runtime_status = "idle".into();
        context.blocking_pending = true;
        let pending = OperationPolicy.evaluate(&context);
        assert!(!pending.allows(CodexOperation::AsyncAnswerIdle));
        assert_eq!(
            pending
                .get(CodexOperation::AsyncAnswerIdle)
                .unwrap()
                .reason_code
                .as_deref(),
            Some(codes::OPERATION_BLOCKING_PENDING)
        );
        // 原生阻塞问答不受空闲规则误伤。
        assert!(pending.allows(CodexOperation::BlockingInput));

        context.blocking_pending = false;
        context.queue_paused = true;
        let paused = OperationPolicy.evaluate(&context);
        assert!(!paused.allows(CodexOperation::AsyncAnswerIdle));
        assert_eq!(
            paused
                .get(CodexOperation::AsyncAnswerIdle)
                .unwrap()
                .reason_code
                .as_deref(),
            Some(codes::OPERATION_QUEUE_PAUSED)
        );
    }

    /// 定向打断由**证据**定门，不以运行态为准（镜像是滞后事实；安全阀是
    /// `expectedTurnId`）。无证据 → 拒绝；有证据 → 允许（运行态如何都一样）。
    #[test]
    fn f08p_t02_interrupt_gated_by_evidence_not_runtime() {
        let mut context = idle_context();
        context.contract.insert(
            CodexOperation::Interrupt,
            match_for(CodexOperation::Interrupt),
        );
        for runtime in ["idle", "active", "unknown", ""] {
            context.runtime_status = runtime.into();
            assert!(
                OperationPolicy
                    .evaluate(&context)
                    .allows(CodexOperation::Interrupt),
                "runtime={runtime} 不得成为打断门（阀值才是安全门）"
            );
        }
        // 无任何证据 → 拒绝（关闭适配也不得凭空放行）。
        context.contract.clear();
        context.auto_adapt_enabled = false;
        let denied = OperationPolicy.evaluate(&context);
        assert!(!denied.allows(CodexOperation::Interrupt));
        assert_eq!(
            denied
                .get(CodexOperation::Interrupt)
                .unwrap()
                .reason_code
                .as_deref(),
            Some(codes::OPERATION_NO_EVIDENCE)
        );
    }

    /// 未接入操作显式 false（前端不猜）；未知集合不给任何授权。
    #[test]
    fn f08p_t02_unimplemented_and_unknown_do_not_grant() {
        let ops = OperationPolicy.evaluate(&idle_context());
        for op in [
            CodexOperation::RequestCancel,
            CodexOperation::PermissionsApproval,
            CodexOperation::ModeRead,
            CodexOperation::ModeSet,
            CodexOperation::PermissionsRead,
            CodexOperation::PermissionsSet,
        ] {
            let status = ops.get(op).unwrap();
            assert!(!status.implemented, "{op:?}");
            assert!(!status.allowed, "{op:?}");
            assert_eq!(
                status.reason_code.as_deref(),
                Some(codes::OPERATION_NOT_IMPLEMENTED)
            );
        }
        let unknown = EffectiveOperations::unknown();
        for op in CodexOperation::ALL {
            assert!(!unknown.allows(op), "{op:?}");
            assert_eq!(
                unknown.get(op).unwrap().reason_code.as_deref(),
                Some(codes::OPERATION_UNKNOWN_POLICY)
            );
        }
    }

    /// 事实不变 → 同一 policyRevision（静默期不产生新投影的自激源头）；
    /// 事实一变 → revision 变。
    #[test]
    fn f08p_t02_policy_revision_is_stable_until_facts_change() {
        let context = idle_context();
        let a = OperationPolicy.evaluate(&context);
        let b = OperationPolicy.evaluate(&context);
        assert_eq!(a.policy_revision, b.policy_revision);
        assert_ne!(a.policy_revision, 0);

        let mut changed = context.clone();
        changed.auto_adapt_enabled = false;
        let c = OperationPolicy.evaluate(&changed);
        assert_ne!(a.policy_revision, c.policy_revision);
    }
}

#[cfg(test)]
mod h01_policy_tests {
    use super::*;
    use crate::services::codex_desktop_ipc::reason_code as codes;

    fn base() -> OperationContext {
        OperationContext {
            target_fresh: true,
            auto_adapt_enabled: false,
            runtime_status: "idle".into(),
            blocking_pending: false,
            queue_paused: false,
            read_evidence: true,
            shipped: BTreeMap::new(),
            observed: BTreeMap::new(),
            schema: BTreeMap::new(),
            contract: BTreeMap::new(),
            contract_error: None,
        }
    }

    /// H01：真实成功证据**不再**豁免自动适配活门（关闭后新动作零写）。
    #[test]
    fn h01_observed_does_not_bypass_auto_adapt_off() {
        let mut ctx = base();
        ctx.observed.insert(CodexOperation::BlockingInput, true);
        ctx.observed.insert(CodexOperation::AsyncAnswerIdle, true);
        let ops = OperationPolicy.evaluate(&ctx);
        for operation in [
            CodexOperation::BlockingInput,
            CodexOperation::AsyncAnswerIdle,
        ] {
            let status = ops.get(operation).expect("status");
            assert!(
                !status.allowed,
                "关闭适配后 observed 不得放行 {operation:?}"
            );
            assert_eq!(
                status.reason_code.as_deref(),
                Some(codes::OPERATION_AUTO_ADAPT_OFF)
            );
        }
    }

    /// 精确 shipped 证据仍不受开关影响（既有语义保留）。
    #[test]
    fn h01_shipped_survives_auto_adapt_off() {
        let mut ctx = base();
        ctx.shipped.insert(CodexOperation::BlockingInput, true);
        let ops = OperationPolicy.evaluate(&ctx);
        let status = ops.get(CodexOperation::BlockingInput).expect("status");
        assert!(status.allowed);
        assert_eq!(status.evidence, EvidenceSource::Shipped);
    }

    /// 没有 shipped/contract、只有 observed 且开关打开 → 允许（真实成功是有效证据）。
    #[test]
    fn h01_observed_allows_when_adapt_open() {
        let mut ctx = base();
        ctx.auto_adapt_enabled = true;
        ctx.observed.insert(CodexOperation::AsyncAnswerIdle, true);
        let ops = OperationPolicy.evaluate(&ctx);
        let status = ops.get(CodexOperation::AsyncAnswerIdle).expect("status");
        assert!(status.allowed);
        assert_eq!(status.evidence, EvidenceSource::ObservedSuccess);
    }
}
