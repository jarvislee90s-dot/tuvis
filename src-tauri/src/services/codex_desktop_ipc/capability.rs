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
///
/// 版本史：
/// * 1 — F01/T01 初版有限字面规则；
/// * 2 — 2026-10-28 异步答题信封改为引用关系核验（压缩器改局部名不失配；同日复审收口：
///   守卫／解析同接收者同作用域、题目标识形参↔数组位置↔map index、字符串专有组合子
///   证人、恒等 transform——收紧发生在 v2 落账前，不另 +1）。v1 下对 Desktop
///   26.1023.51421 落账的证据行**不含** AsyncAnswerIdle，必须作废重识别。
pub const CONTRACT_RULE_VERSION: u32 = 2;

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

/// T03：版本能力**支持状态**（稳定展示面）——与「当前允许执行资格」（`allowed`，
/// 实时门）**分离**的两个并列输出。
///
/// 语义（spec：修复计划 T03）：`support` 回答"这个 Desktop 版本有没有这项能力"，
/// 只随**识别/证据事实**变化；运行中、暂停、短暂断链等**当前资格**条件只改
/// `allowed`/`reason_code`，**不得**把"已支持"闪成"该版本不可用"。
/// `support` **不参与** `allowed` 计算（`evaluate_one` 的资格逻辑一字不动）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupportState {
    /// 有 shipped/observed/contract/schema 任一证据：该版本已支持（稳定展示，
    /// 不随运行态/连接状态翻转）。
    Supported,
    /// 无任何证据且契约识别未完成（识别中/目标未知）。
    Identifying,
    /// 契约识别已完成（包已读、规则已跑）但该操作无匹配：该版本未匹配此项。
    Unmatched,
    /// 本阶段未接入该操作。
    NotImplemented,
}

impl SupportState {
    /// 线上/日志词（与 serde 值一致；单一来源，别处不另抄）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::Identifying => "identifying",
            Self::Unmatched => "unmatched",
            Self::NotImplemented => "not_implemented",
        }
    }
}

/// T03：能力身份摘要（**无路径**）：`SHA-256("probe_key|package_sha256|rule_version")`
/// 前 16 hex。用于前端区分"这份支持结论属于哪个目标/包/规则版本"——同目标短断链
/// 不变；包内容、规则版本或目标任一变化即失效（旧 identity 的展示信息不装进新
/// 目标的授权/证据）。**绝不包含包路径等本机文件系统信息。**
pub fn capability_identity_of(probe_key: &str, package_sha256: &str, rule_version: u32) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(probe_key.as_bytes());
    hasher.update(b"|");
    hasher.update(package_sha256.as_bytes());
    hasher.update(b"|");
    hasher.update(rule_version.to_string().as_bytes());
    let digest = hasher.finalize();
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
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
    /// T03：版本能力支持状态（稳定展示；与 `allowed` 并列，**不参与**资格计算）。
    pub support: SupportState,
    /// R07：本次 `support` 是否**沿用了上次识别结论的展示事实**（真实重连换代窗内、
    /// 实时证据尚未装回）。`true` = 前端应如实标「连接恢复中」。**纯展示**：
    /// 不参与 `allowed`/资格判定。
    #[serde(default)]
    pub support_inherited: bool,
    /// C03：`support_inherited` 为 true 时的**包身份核对态**——`true` = 本代际已对
    /// 当前包完成**内容 SHA 核验**且与上次识别一致（展示身份可当作已确认）；
    /// `false`/缺省 = 仅 (len, mtime) 快筛通过，当前包身份待核对（前端必须以
    /// 「待核对」文案与已确认能力明确区分）。**纯展示**，不参与资格判定。
    #[serde(default)]
    pub support_identity_verified: bool,
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
    /// T03：能力身份摘要（[`capability_identity_of`]；无路径）。由 adapter 在装配
    /// 投影处填充（策略层不持目标身份）——`None` = 无目标/未识别。仅用于前端区分
    /// 展示目标（短断链保留上次支持展示的前提 = identity 不变），**不参与授权**。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability_identity: Option<String>,
}

impl Default for EffectiveOperations {
    fn default() -> Self {
        Self::unknown()
    }
}

/// R07：一次已完成识别的**展示记录**（同一完整目标的上次支持结论）。
///
/// 用途：真实重连（连接代际更换、证据槽因此失配）期间沿用上次识别的 support 展示
/// 与能力身份，避免"已支持"在毫秒级重连里闪成"识别中"；证据重新装回即回到实时
/// 事实。**纯展示**：不参与 `allowed`/`evidence`，不产生写资格。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplaySupport {
    /// 记录时的**连接代际**。只有当前代际与它不同（= 真实重连换代过）才允许
    /// 沿用：同代际下槽为空是"未识别"，不得拿旧记录冒充。
    pub epoch: u64,
    /// 记录时的目标身份（沿用判据第一段）。
    pub platform: String,
    /// 记录时的 Desktop build（沿用判据第一段）。
    pub build: String,
    /// 记录时的探测键（双方都解析出值时比对；不同 = 新目标，不沿用）。
    pub probe_key: Option<String>,
    /// R17 同款**包内容指纹** `(len, mtime_ms)`。同 build 换包（适配关闭时包内容
    /// 永不复查）必须靠它拒绝沿用——stat 变化或读不到即视为新目标。
    ///
    /// C03：语义收严为**取证时**绑定并核验的那次 stat（证据自带 `package_size`/
    /// `package_mtime_ms`，取证函数已保证读前读后一致），不是安装时对盘上文件的
    /// 另一次 stat——消除「读 A、装前换 B、A 结论配 B 指纹」的窗口。
    pub package_fingerprint: Option<(u64, i64)>,
    /// C03：**取证时**的包内容 SHA-256（= 证据的 `package_sha256`）。重连沿用前
    /// 对当前包重算内容哈希并与它核对——stat 快筛可被同 len/保留 mtime 的替换
    /// 绕过，只有内容哈希能证明「还是被识别的那份包」。
    pub package_sha256: String,
    /// 该次识别的能力身份摘要（[`capability_identity_of`]）。
    pub identity: String,
    /// 逐操作支持结论——**上次展示的快照**（安装时用同一 `OperationPolicy` 求值，
    /// 因此天然含 shipped/observed/contract/schema 全部证据来源；不另行推导）。
    pub support: BTreeMap<CodexOperation, SupportState>,
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
                        support: SupportState::NotImplemented,
                        support_inherited: false,
                        support_identity_verified: false,
                        reason_code: Some(super::reason_code::OPERATION_UNKNOWN_POLICY.into()),
                    },
                )
            })
            .collect();
        Self {
            policy_revision: 0,
            operations,
            capability_identity: None,
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
            // T03：support 是序列化事实的一部分——进事实指纹（恒等派生自已有事实，
            // 不引入额外抖动）。
            hash ^= match status.support {
                SupportState::Supported => 0,
                SupportState::Identifying => 1,
                SupportState::Unmatched => 2,
                SupportState::NotImplemented => 3,
            };
            hash = hash.wrapping_mul(0x100_0000_01b3);
            // R07：support_inherited 是**序列化事实**的一部分（前端据此显示
            // 「连接恢复中」）——进事实指纹，否则"沿用↔实时"切换的投影不会重发布。
            hash ^= u64::from(status.support_inherited);
            hash = hash.wrapping_mul(0x100_0000_01b3);
            // C03：support_identity_verified 同理——「待核对 ↔ 已核对」改变前端
            // 标记文案，是序列化展示事实；不进指纹则核验落账后的升级不会重发布。
            hash ^= u64::from(status.support_identity_verified);
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
    /// 当前目标的契约识别**已完成**（包已读、规则已跑；`contract` 里缺的操作 =
    /// 规则未匹配，而不是"还没识别"）。`false` = 未尝试／读取失败／目标已换代。
    pub contract_identified: bool,
    /// 契约识别失败的原因码（`None` = 未尝试或成功）。
    pub contract_error: Option<String>,
    /// R07：**仅展示**的「同一完整目标上次的支持结论」（逐操作）——真实重连
    /// （连接代际更换）期间沿用上次已识别结论的 support 展示；识别重新完成即回到
    /// 实时事实。**不参与** `allowed`/`evidence`/`contract` 计算，不产生任何写资格
    /// （`contract` 仍为空、`contract_identified` 仍为 `false`——识别事实不伪装）。
    pub display_support: BTreeMap<CodexOperation, SupportState>,
    /// C03：`display_support` 非空时，本代际是否已对当前包完成**内容 SHA 核验**
    /// 且与记录一致。`true` = 展示身份可当作已确认（`supportIdentityVerified` 置位）；
    /// `false` = 仅 stat 快筛通过（「待核对」态——支持可如实沿用，身份不发布）。
    /// **纯展示**，不参与授权。
    pub display_identity_verified: bool,
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
    /// R07：该操作「同身份上次支持结论」的**展示**事实（不参与授权）。
    fn display_support_for(&self, operation: CodexOperation) -> Option<SupportState> {
        self.display_support.get(&operation).copied()
    }
    fn runtime_is(&self, expected: &str) -> bool {
        self.runtime_status.eq_ignore_ascii_case(expected)
    }
}

/// T03：支持态推导（唯一计算源 = [`OperationPolicy::evaluate_one`] 经本函数）。
/// `!implemented` → 未接入；有 shipped/observed/contract/schema 任一证据 → 已支持；
/// 无证据且契约识别已完成 → 未匹配；无证据且未识别 → 识别中。
///
/// R07：**无证据且未识别**时，若有同一完整目标上次识别结论的展示事实（真实重连
/// 换代导致证据槽失配）→ 沿用它（`Unmatched` 也沿用，如实"该版本未匹配"）；
/// 有实时证据/识别完成时实时事实优先。**只做并列输出，不参与 `allowed` 计算。**
///
/// 返回 `(支持态, 是否沿用了展示事实)` —— 后者进 `OperationStatus.support_inherited`，
/// 让前端在生产路径上显示「连接恢复中」（纯展示，不参与资格）。
fn support_of(
    implemented: bool,
    evidence: EvidenceSource,
    contract_identified: bool,
    display: Option<SupportState>,
) -> (SupportState, bool) {
    if !implemented {
        return (SupportState::NotImplemented, false);
    }
    if evidence != EvidenceSource::None {
        return (SupportState::Supported, false);
    }
    if contract_identified {
        return (SupportState::Unmatched, false);
    }
    match display {
        // 展示事实只表达「上次已识别到的结论」，不能凭空造出支持。
        Some(carried @ (SupportState::Supported | SupportState::Unmatched)) => (carried, true),
        _ => (SupportState::Identifying, false),
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
            // 身份由 adapter 装配投影处填充（策略层不持目标身份）。
            capability_identity: None,
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
                support: SupportState::NotImplemented,
                support_inherited: false,
                support_identity_verified: false,
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
            let (support, support_inherited) = support_of(
                true,
                evidence,
                context.contract_identified,
                context.display_support_for(operation),
            );
            return OperationStatus {
                implemented: true,
                allowed: context.target_fresh,
                requires_auto_adapt: false,
                evidence,
                support,
                support_inherited,
                support_identity_verified: support_inherited && context.display_identity_verified,
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
        // R07：**展示**事实只用于选「原因文案」（解释"为什么现在不能"），不参与
        // 上面的 `gate_open`/`allowed`。同身份重连 + 适配关闭时，上次已支持 → 原因
        // 如实报「适配已关闭」，而不是误报「无证据（等识别）」。
        let display_supported = matches!(
            context.display_support_for(operation),
            Some(SupportState::Supported)
        );
        if !gate_open {
            let reason = if (contract || observed || schema || display_supported)
                && !context.auto_adapt_enabled
            {
                codes::OPERATION_AUTO_ADAPT_OFF
            } else if context.contract_error.is_some() {
                codes::OPERATION_CONTRACT_UNMATCHED
            } else if evidence == EvidenceSource::None {
                // 包已读、规则已跑而本项仍无匹配 = "规则未匹配"；只有还没识别过
                // （未尝试／目标换代）才是"无证据"。两者给用户的动作不同：前者是
                // 版本契约问题（到桌面端处理/等适配），后者是等识别完成。
                if context.contract_identified {
                    codes::OPERATION_CONTRACT_UNMATCHED
                } else {
                    codes::OPERATION_NO_EVIDENCE
                }
            } else {
                codes::OPERATION_CONTRACT_UNMATCHED
            };
            let (support, support_inherited) = support_of(
                true,
                evidence,
                context.contract_identified,
                context.display_support_for(operation),
            );
            return OperationStatus {
                implemented: true,
                allowed: false,
                requires_auto_adapt,
                evidence,
                support,
                support_inherited,
                support_identity_verified: support_inherited && context.display_identity_verified,
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

        let (support, support_inherited) = support_of(
            true,
            evidence,
            context.contract_identified,
            context.display_support_for(operation),
        );
        OperationStatus {
            implemented: true,
            allowed: blocked_reason.is_none(),
            requires_auto_adapt,
            evidence,
            support,
            support_inherited,
            support_identity_verified: support_inherited && context.display_identity_verified,
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
            contract_identified: true,
            contract_error: None,
            display_support: Default::default(),
            display_identity_verified: false,
        }
    }

    /// 只读证据不得解锁任何写操作（读链成功 ≠ 可写）。
    #[test]
    fn f08p_t02_readonly_evidence_does_not_unlock_write() {
        let mut context = idle_context();
        context.contract.clear();
        context.contract_identified = false;
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
        // 场景 = 完全没有契约事实（未识别），不是"已识别但未匹配"。
        context.contract.clear();
        context.contract_identified = false;
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
        // 无任何证据（未识别）→ 拒绝（关闭适配也不得凭空放行）。
        context.contract.clear();
        context.contract_identified = false;
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

    /// 2026-10-28：异步答题规则改为结构化识别（重命名容忍）。规则一变就 +1——
    /// 已缓存的 v1 证据行对 26.1023.51421 **不含** AsyncAnswerIdle，若按旧版本号
    /// 复用，修了规则也不生效。
    #[test]
    fn contract_rule_version_bumped_for_structural_async_rule() {
        assert_eq!(CONTRACT_RULE_VERSION, 2);
    }

    /// 已识别（包已读、规则已跑）但本操作无匹配 → `operation_contract_unmatched`
    /// （"规则未匹配"），不再与"尚未识别"的 `operation_no_evidence` 混用；
    /// 识别失败（包不可读等）仍是契约未匹配码（既有行为）。
    #[test]
    fn identified_but_unmatched_operation_reports_contract_unmatched() {
        let mut context = idle_context();
        context.contract.remove(&CodexOperation::AsyncAnswerIdle);
        context.contract_identified = true;
        let ops = OperationPolicy.evaluate(&context);
        let status = ops.get(CodexOperation::AsyncAnswerIdle).unwrap();
        assert!(!status.allowed);
        assert_eq!(status.evidence, EvidenceSource::None);
        assert_eq!(
            status.reason_code.as_deref(),
            Some(codes::OPERATION_CONTRACT_UNMATCHED),
            "已识别但本项未匹配 = 规则未匹配，不是无证据"
        );
        // 对照：其它已匹配操作不受影响。
        assert!(ops.allows(CodexOperation::BlockingInput));

        // 对照：尚未识别（未尝试、无失败码）→ 无证据码。
        let mut pending = idle_context();
        pending.contract.clear();
        pending.contract_identified = false;
        let ops = OperationPolicy.evaluate(&pending);
        assert_eq!(
            ops.get(CodexOperation::AsyncAnswerIdle)
                .unwrap()
                .reason_code
                .as_deref(),
            Some(codes::OPERATION_NO_EVIDENCE)
        );

        // 对照：识别失败（包不可读等）→ 契约未匹配码。
        let mut failed = idle_context();
        failed.contract.clear();
        failed.contract_identified = false;
        failed.contract_error = Some("contract_package_unavailable".into());
        let ops = OperationPolicy.evaluate(&failed);
        assert_eq!(
            ops.get(CodexOperation::AsyncAnswerIdle)
                .unwrap()
                .reason_code
                .as_deref(),
            Some(codes::OPERATION_CONTRACT_UNMATCHED)
        );
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
            contract_identified: false,
            contract_error: None,
            display_support: Default::default(),
            display_identity_verified: false,
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

#[cfg(test)]
mod t03_support_identity_tests {
    //! T03：「版本能力支持状态」（稳定展示）与「当前允许执行资格」（实时门）分离。
    //! 核心断言：同目标已匹配时，运行中/暂停/短暂断链只改 allowed/reason，
    //! **support 恒 supported**；身份摘要区分目标/包/规则版本且不含路径。

    use super::*;
    use crate::services::codex_desktop_ipc::reason_code as codes;

    fn matched_idle_context() -> OperationContext {
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
            contract: (CodexOperation::ALL)
                .into_iter()
                .filter(|op| op.is_implemented_in_phase())
                .map(|op| {
                    (
                        op,
                        ContractMatch {
                            method: "thread-follower-start-turn".into(),
                            base_version: 2,
                            request_rule: "conversationId".into(),
                            response_rule: "ok".into(),
                            semantic_rule: "rule".into(),
                        },
                    )
                })
                .collect(),
            contract_identified: true,
            contract_error: None,
            display_support: Default::default(),
            display_identity_verified: false,
        }
    }

    /// T03 核心反例：同目标已匹配（contract 全量、auto_adapt on、idle）→
    /// idle.allowed=true 且 support=supported；运行中 allowed=false 但
    /// **support 仍 supported**（"已支持"不被当前资格条件闪成"该版本不可用"）。
    #[test]
    fn t03_support_stays_supported_while_eligibility_flips() {
        let context = matched_idle_context();
        let idle = OperationPolicy.evaluate(&context);
        let idle_status = idle.get(CodexOperation::AsyncAnswerIdle).unwrap();
        assert!(idle_status.allowed, "前置：空闲可答");
        assert_eq!(idle_status.support, SupportState::Supported);

        // 运行中：只改 allowed/reason。
        let mut active_ctx = context.clone();
        active_ctx.runtime_status = "active".into();
        let active = OperationPolicy.evaluate(&active_ctx);
        let status = active.get(CodexOperation::AsyncAnswerIdle).unwrap();
        assert!(!status.allowed);
        assert_eq!(
            status.reason_code.as_deref(),
            Some(codes::OPERATION_RUNTIME_ACTIVE)
        );
        assert_eq!(
            status.support,
            SupportState::Supported,
            "运行中不得把已支持闪成未支持"
        );

        // 原生阻塞在场：同上。
        let mut pending_ctx = context.clone();
        pending_ctx.blocking_pending = true;
        let status = OperationPolicy
            .evaluate(&pending_ctx)
            .get(CodexOperation::AsyncAnswerIdle)
            .unwrap()
            .clone();
        assert!(!status.allowed);
        assert_eq!(status.support, SupportState::Supported);

        // 队列暂停：同上。
        let mut paused_ctx = context.clone();
        paused_ctx.queue_paused = true;
        let status = OperationPolicy
            .evaluate(&paused_ctx)
            .get(CodexOperation::AsyncAnswerIdle)
            .unwrap()
            .clone();
        assert!(!status.allowed);
        assert_eq!(status.support, SupportState::Supported);

        // 短暂断链（target_fresh=false）：同上（展示面稳定，资格撤回）。
        let mut stale_ctx = context.clone();
        stale_ctx.target_fresh = false;
        let stale = OperationPolicy.evaluate(&stale_ctx);
        let status = stale.get(CodexOperation::AsyncAnswerIdle).unwrap();
        assert!(!status.allowed);
        assert_eq!(
            status.reason_code.as_deref(),
            Some(codes::OPERATION_TARGET_STALE)
        );
        assert_eq!(status.support, SupportState::Supported);
        // 断链对 Send 同样只撤资格不改支持。
        let send = stale.get(CodexOperation::Send).unwrap();
        assert!(!send.allowed);
        assert_eq!(send.support, SupportState::Supported);
    }

    /// T03：支持态四档推导（唯一计算源 = OperationPolicy）。
    /// 未实现 → not_implemented；无证据且未识别 → identifying；已识别但无匹配 →
    /// unmatched；shipped/observed/contract/schema 任一 → supported。
    #[test]
    fn t03_support_states_follow_identification_stages() {
        // 未接入操作 → not_implemented（不因"没有证据"误报未匹配）。
        let ops = OperationPolicy.evaluate(&matched_idle_context());
        assert_eq!(
            ops.get(CodexOperation::RequestCancel).unwrap().support,
            SupportState::NotImplemented
        );
        // 未知集合（老载荷缺字段缺省）全部 not_implemented 支持（implemented=false）。
        let unknown = EffectiveOperations::unknown();
        assert_eq!(
            unknown.get(CodexOperation::Send).unwrap().support,
            SupportState::NotImplemented
        );

        // 无任何证据且未识别 → identifying。
        let mut ctx = matched_idle_context();
        ctx.contract.clear();
        ctx.contract_identified = false;
        let ops = OperationPolicy.evaluate(&ctx);
        assert_eq!(
            ops.get(CodexOperation::AsyncAnswerIdle).unwrap().support,
            SupportState::Identifying
        );

        // 已识别（包已读、规则已跑）但本项无匹配 → unmatched（如实"该版本未匹配"）。
        let mut ctx = matched_idle_context();
        ctx.contract.remove(&CodexOperation::AsyncAnswerIdle);
        let ops = OperationPolicy.evaluate(&ctx);
        let status = ops.get(CodexOperation::AsyncAnswerIdle).unwrap();
        assert_eq!(status.support, SupportState::Unmatched);
        assert!(!status.allowed, "推导 support 不得改资格：无证据仍不允许");

        // shipped / observed / schema 任一证据 → supported（含适配关闭）。
        for mut ctx in [
            {
                let mut c = matched_idle_context();
                c.contract.clear();
                c.contract_identified = true;
                c.shipped.insert(CodexOperation::AsyncAnswerIdle, true);
                c
            },
            {
                let mut c = matched_idle_context();
                c.contract.clear();
                c.contract_identified = true;
                c.observed.insert(CodexOperation::AsyncAnswerIdle, true);
                c
            },
            {
                let mut c = matched_idle_context();
                c.contract.clear();
                c.contract_identified = true;
                c.schema.insert(CodexOperation::CommandApproval, true);
                c
            },
        ] {
            ctx.auto_adapt_enabled = false; // 关适配也不改变"有没有证据"这一事实
            let op = if ctx.schema.is_empty() {
                CodexOperation::AsyncAnswerIdle
            } else {
                CodexOperation::CommandApproval
            };
            let ops = OperationPolicy.evaluate(&ctx);
            assert_eq!(
                ops.get(op).unwrap().support,
                SupportState::Supported,
                "有证据即 supported（{op:?}）"
            );
        }
    }

    /// T03：适配关闭 → support 仍 supported，allowed=false + auto_adapt_off
    /// （写入口零帧仍由既有资格门保证——support 只是并列输出）。
    #[test]
    fn t03_auto_adapt_off_keeps_support_but_retracts_eligibility() {
        let mut ctx = matched_idle_context();
        ctx.auto_adapt_enabled = false;
        let ops = OperationPolicy.evaluate(&ctx);
        let status = ops.get(CodexOperation::AsyncAnswerIdle).unwrap();
        assert_eq!(status.support, SupportState::Supported);
        assert!(!status.allowed);
        assert_eq!(
            status.reason_code.as_deref(),
            Some(codes::OPERATION_AUTO_ADAPT_OFF)
        );
    }

    /// R07：**展示事实不授权**——同一上下文加/不加 display_support，
    /// `allowed`/`evidence`/`reason_code` 逐操作完全相同；只有 `support` 变。
    /// 同时锁住展示事实的四档边界：只沿用 supported/unmatched，不伪造未接入。
    #[test]
    fn r07_display_support_never_grants_and_only_touches_support() {
        // 无证据、未识别（真实重连窗内的策略上下文）。
        let mut bare = matched_idle_context();
        bare.contract.clear();
        bare.contract_identified = false;
        let without = OperationPolicy.evaluate(&bare);

        let mut displayed = bare.clone();
        displayed
            .display_support
            .insert(CodexOperation::AsyncAnswerIdle, SupportState::Supported);
        displayed
            .display_support
            .insert(CodexOperation::BlockingInput, SupportState::Unmatched);
        displayed
            .display_support
            .insert(CodexOperation::RequestCancel, SupportState::Supported);
        let with = OperationPolicy.evaluate(&displayed);

        for operation in CodexOperation::ALL {
            let a = without.get(operation).expect("status");
            let b = with.get(operation).expect("status");
            assert_eq!(
                a.allowed, b.allowed,
                "展示事实不得改 allowed（{operation:?}）"
            );
            assert_eq!(
                a.evidence, b.evidence,
                "展示事实不得冒充证据（{operation:?}）"
            );
            assert_eq!(
                a.reason_code, b.reason_code,
                "展示事实不得改原因码（{operation:?}）"
            );
        }
        // 只影响 support：上次已支持/未匹配如实沿用。
        assert_eq!(
            with.get(CodexOperation::AsyncAnswerIdle).unwrap().support,
            SupportState::Supported
        );
        assert_eq!(
            with.get(CodexOperation::BlockingInput).unwrap().support,
            SupportState::Unmatched
        );
        // 未接入操作不因展示事实变成"已支持"。
        assert_eq!(
            with.get(CodexOperation::RequestCancel).unwrap().support,
            SupportState::NotImplemented
        );
        // 未提供展示事实的操作仍是识别中。
        assert_eq!(
            with.get(CodexOperation::Interrupt).unwrap().support,
            SupportState::Identifying
        );
        // 实时事实优先：识别已完成时展示事实不覆盖 unmatched/无证据结论。
        let mut identified = matched_idle_context();
        identified.contract.remove(&CodexOperation::AsyncAnswerIdle);
        identified
            .display_support
            .insert(CodexOperation::AsyncAnswerIdle, SupportState::Supported);
        assert_eq!(
            OperationPolicy
                .evaluate(&identified)
                .get(CodexOperation::AsyncAnswerIdle)
                .unwrap()
                .support,
            SupportState::Unmatched,
            "识别完成后实时事实优先于展示事实"
        );
    }

    /// R07 修复轮 1（偏差①接受条件②）：**适配关闭**下的反向行——有/无展示事实，
    /// `allowed` 相同（`reason_code` 允许不同，那是"原因文案"的既定差异）；
    /// `evidence` 相同。把「原因码是唯一允许差异」写死。
    #[test]
    fn r07_auto_adapt_off_display_only_may_change_reason_text() {
        let mut bare = matched_idle_context();
        bare.contract.clear();
        bare.contract_identified = false;
        bare.auto_adapt_enabled = false;
        let without = OperationPolicy.evaluate(&bare);

        let mut displayed = bare.clone();
        displayed
            .display_support
            .insert(CodexOperation::AsyncAnswerIdle, SupportState::Supported);
        displayed
            .display_support
            .insert(CodexOperation::CommandApproval, SupportState::Supported);
        let with = OperationPolicy.evaluate(&displayed);

        for operation in CodexOperation::ALL {
            let a = without.get(operation).expect("status");
            let b = with.get(operation).expect("status");
            assert_eq!(
                a.allowed, b.allowed,
                "适配关闭：展示事实不得改 allowed（{operation:?}）"
            );
            assert_eq!(
                a.evidence, b.evidence,
                "适配关闭：展示事实不得冒充证据（{operation:?}）"
            );
            assert_eq!(
                a.requires_auto_adapt, b.requires_auto_adapt,
                "适配关闭：展示事实不得改活门依赖（{operation:?}）"
            );
        }
        // 原因码是**唯一**允许的差异：从「等识别」纠正为「适配已关闭」。
        assert_eq!(
            without
                .get(CodexOperation::AsyncAnswerIdle)
                .unwrap()
                .reason_code
                .as_deref(),
            Some(codes::OPERATION_NO_EVIDENCE)
        );
        assert_eq!(
            with.get(CodexOperation::AsyncAnswerIdle)
                .unwrap()
                .reason_code
                .as_deref(),
            Some(codes::OPERATION_AUTO_ADAPT_OFF)
        );
        // 支持展示照旧沿用，但资格仍为 false。
        assert_eq!(
            with.get(CodexOperation::AsyncAnswerIdle).unwrap().support,
            SupportState::Supported
        );
        assert!(!with.get(CodexOperation::AsyncAnswerIdle).unwrap().allowed);
    }

    /// T03：identity——不同包 SHA / 规则版本 / 目标互不相同；同输入恒等；
    /// 摘要不含路径成分。
    #[test]
    fn t03_capability_identity_discriminates_targets_and_versions() {
        let base = capability_identity_of("macos|build-a|cli-a|1", &"a".repeat(64), 2);
        // 同输入恒等
        assert_eq!(
            base,
            capability_identity_of("macos|build-a|cli-a|1", &"a".repeat(64), 2)
        );
        // 包 SHA 变 → 失效
        assert_ne!(
            base,
            capability_identity_of("macos|build-a|cli-a|1", &"b".repeat(64), 2)
        );
        // 规则版本变 → 失效
        assert_ne!(
            base,
            capability_identity_of("macos|build-a|cli-a|1", &"a".repeat(64), 3)
        );
        // 目标变（同 SHA 也不行——A 的支持不是 B 的支持）
        assert_ne!(
            base,
            capability_identity_of("macos|build-b|cli-b|1", &"a".repeat(64), 2)
        );
        // 无路径：固定长度 hex 摘要，不含分隔符/盘符/扩展名等路径痕迹
        assert_eq!(base.len(), 16);
        assert!(base.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(!base.contains('/') && !base.contains('\\') && !base.contains('.'));
    }

    /// T03：serde 形状——support snake_case 小写；capabilityIdentity 为 None 时
    /// 整个字段省略（旧载荷兼容面），Some 时 camelCase 键名。
    #[test]
    fn t03_support_and_identity_serialize_camel_snake() {
        let mut context = matched_idle_context();
        context.contract.remove(&CodexOperation::AsyncAnswerIdle);
        let ops = OperationPolicy.evaluate(&context);
        let raw = serde_json::to_value(&ops).unwrap();
        assert_eq!(
            raw["operations"]["async_answer_idle"]["support"],
            serde_json::json!("unmatched")
        );
        assert_eq!(
            raw["operations"]["blocking_input"]["support"],
            serde_json::json!("supported")
        );
        // evaluate 不填身份 → 字段整个省略
        assert!(raw.get("capabilityIdentity").is_none());
        // adapter 填充后（此处直接模拟）随载荷下发
        let mut ops = ops;
        ops.capability_identity = Some(capability_identity_of("k", "s", 2));
        let raw = serde_json::to_value(&ops).unwrap();
        assert!(raw["capabilityIdentity"].is_string());
        // 注：support 是 Rust 侧**必备**字段（新服务端恒输出）；「旧载荷缺 support」
        // 的兼容在前端归一化层做（按 evidence 推导展示值，见 codexIpcApi.ts），
        // Rust 侧不提供无上下文的缺省（缺上下文的猜测才是错误方向）。
    }
}
