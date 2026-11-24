//! 逐操作有效能力判定（计划 §2.3 固定入口 `capability::evaluate`；Spec §6.2–6.5）。
//!
//! **纯计算无 IO**：`evaluate` 只消费已装载事实（[`OperationContext`]），不读 DB、
//! 不跑包扫描、不建连——可以在投影临界区/SSE 组装路径里反复调用。
//!
//! # 判定链（逐操作独立，八类功能不是八个总开关）
//!
//! ```text
//! implemented ∧ 未认证失效 ∧ target_fresh ∧ 未协议漂移 ∧ 证据门 ∧ 自动适配门
//! ```
//!
//! * **证据门**（[`EvidenceSource`] 四档）：`Shipped` / `ObservedSuccess` /
//!   `ContractMatched` 三档之一，或（仅 [`Operation::Read`]，Mi-2/N-A 修净）
//!   `read_evidence` 这一**只读运行观测**档。持久证据三档都要求**锚定当前包
//!   身份**（整包内容指纹一致）与**当前规则代际**（契约档）；空指纹不锚定任何
//!   持久证据。
//! * **自动适配门**：契约匹配档是「兼容可尝试」——必须 `auto_adapt_enabled`；
//!   shipped/observed 档按各自许可不受开关影响（关闭后**只读收尾**保留）。
//! * **失败分类门**：协议漂移是**按操作持久化的结论**（[`OperationContext::drifted`]），
//!   只撤被漂移的那些操作；认证失效（[`OperationContext::auth_invalid`]）撤回全部；
//!   网络瞬态/业务拒绝（含额度不足）**不写入任何持久结论**，故不降级、不污染
//!   既有证据（验收④）。单槽「最近一次失败」会把漂移结论覆盖丢失，因此上下文
//!   刻意按**集合**承载（P2-1）。
//! * **预算档**：`contract_inconclusive` 里的操作是「暂不可识别」——不误标永久
//!   不支持，也不影响无关操作（验收⑤）。
//!
//! # `verified` 语义（申报）
//!
//! `verified = true` 表示**该操作的证据讲的是眼前这个目标**：
//! * 持久证据（shipped/observed/contract）→ 必须锚定当前**整包内容指纹**
//!   （[`OperationContext::current_package_sha256`]）且契约档规则代际一致；
//! * 只读运行观测（`read_evidence`）→ 是**当前进程/目标**的活体事实，故同样
//!   verified，但**只**对 [`Operation::Read`] 生效（N-A：实现只覆盖 Read，其余
//!   只读目录不共享读链证据），绝不解锁任何写操作。
//!
//! 它**不是**「该 build 已真机实测」——后者由 `Shipped` 档表达。契约匹配 =
//! `verified` 但 `requires_auto_adapt=true`（兼容可尝试）；shipped = `verified`
//! 且不依赖开关；旧包/旧规则证据直接失效（不 verified、不放行）。
//! `requires_auto_adapt` 只标记「确实受活门约束」的操作（契约档写动作），与
//! `evaluate_one` 的实际门保持一一对应——标志不夸大也不遗漏。
//!
//! `revision` 是**事实指纹**（FNV-1a over 稳定逐操作事实），同事实同值：事实不变
//! 的重复求值不得换 revision（SSE 下游据此避免静默期重复发布）。

use std::collections::{BTreeMap, BTreeSet};

use super::error::{codes, DshError};
use super::model::{
    ContractEvidence, EffectiveOperations, EvidenceSource, Operation, OperationStatus,
};

/// 逐操作 reason_code 词表（机器码，snake_case；前端/日志按码取文案）。
///
/// 只增不改：既有语义（`operation_not_implemented` 等）与 Codex 域词表同形但
/// **独立成表**——DSH 的判定事实不同，不复用 Codex 的常量。
pub mod reason {
    /// 本阶段未实现该操作（能力面未接入）。
    pub const NOT_IMPLEMENTED: &str = "operation_not_implemented";
    /// 目标不可归因/连接不在场（旧代际目标立即失效）。
    pub const TARGET_STALE: &str = "operation_target_stale";
    /// 自动适配已关闭：依赖契约匹配的新写动作撤回。
    pub const AUTO_ADAPT_OFF: &str = "operation_auto_adapt_off";
    /// 当前包契约未匹配该操作。
    pub const CONTRACT_UNMATCHED: &str = "operation_contract_unmatched";
    /// 契约证据来自另一个包（包内容指纹变化 → 重新归因）。
    pub const CONTRACT_STALE: &str = "operation_contract_stale";
    /// 契约证据来自另一代规则表（规则换代 → 不复用）。
    pub const RULES_STALE: &str = "operation_rules_stale";
    /// 契约识别失败（包不可读/多义/平台不支持等；失败原因在上下文）。
    pub const CONTRACT_READ_FAILED: &str = "operation_contract_read_failed";
    /// 该操作观测到协议漂移（形状不符）→ 只撤该操作。
    pub const CONTRACT_DRIFT: &str = "operation_contract_drift";
    /// 认证失效：全部操作撤回（重新授权后才能继续）。
    pub const AUTH_INVALID: &str = "operation_auth_invalid";
    /// 预算受限/来源不可读：该操作「暂不可识别」（不是永久不支持）。
    pub const INCONCLUSIVE: &str = "operation_inconclusive";
    /// 没有任何当前证据（未识别/未验证）。
    pub const NO_EVIDENCE: &str = "operation_no_evidence";
}

/// 只读操作（不受自动适配活门约束：关闭适配后仍可只读收尾/读取目录）。
///
/// 这 4 项是**读取类**能力：`Read`（历史/状态）、`SessionSearch`、`SkillsList`、
/// `CommandsList`（N-A 修净：`read_evidence` 读链成功**只**覆盖 `Read`——见
/// [`OperationContext::read_evidence`]；`SessionSearch`/`SkillsList`/`CommandsList`
/// 需要它们各自的契约匹配或本机真实成功记录，不由 `read_evidence` 放行）。
/// `Archive`/`Pin` 等虽然是「整理」语义，但会写宿主状态，**不算**只读。
fn is_read_only(operation: Operation) -> bool {
    matches!(
        operation,
        Operation::Read
            | Operation::SessionSearch
            | Operation::SkillsList
            | Operation::CommandsList
    )
}

/// 失败分类（Spec §6.4「兼容失败分四类」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// 协议漂移：响应/请求形状与契约不符（只撤受影响操作）。
    ProtocolDrift,
    /// 业务拒绝：服务端明确拒绝该次动作（额度不足/队列项不存在/索引未启用）。
    BusinessRejection,
    /// 认证失效：401/403、unpaired、peer 撤销（全部操作撤回）。
    AuthInvalid,
    /// 网络瞬态：断链/超时/背压溢出（不降级、不污染证据）。
    NetworkTransient,
}

/// 原生失败码 → 四档分类（纯词表；未知码保守归入业务拒绝）。
///
/// **为什么不把未知码当协议漂移**：未知业务码（如未来新增的拒绝理由）若被当作
/// 漂移会误撤能力；协议漂移必须由**明确的网关形状错误**证明。
pub fn classify_native_failure(code: &str) -> FailureClass {
    let wire = code.trim();
    let lower = wire.to_ascii_lowercase();
    // 认证失效族（401/403、unpaired、peer 撤销/冲突、绑定换代）。
    const AUTH_CODES: &[&str] = &[
        "unpaired",
        "unauthorized",
        "forbidden",
        "dsh_peer_unauthorized",
        "dsh_peer_revoked",
        "dsh_auth_epoch_stale",
    ];
    if AUTH_CODES.contains(&lower.as_str()) {
        return FailureClass::AuthInvalid;
    }
    // 协议漂移族：网关形状/信封/参数签名错误。
    const DRIFT_PREFIXES: &[&str] = &[
        "gateway/arguments-invalid",
        "gateway/input-invalid",
        "gateway/signature-invalid",
        "gateway/protocol",
        "gateway/result-invalid",
        "gateway/definition-unavailable",
        "gateway/method-unavailable",
        "gateway/invocation-unavailable",
    ];
    if DRIFT_PREFIXES
        .iter()
        .any(|prefix| lower.starts_with(prefix))
    {
        return FailureClass::ProtocolDrift;
    }
    // 网络瞬态族：断链/超时/背压/额度不足（额度是业务拒绝的一种，但明确不降级）。
    const TRANSIENT_CODES: &[&str] = &[
        "gateway/uplink-overflow",
        "gateway/cancelled",
        "dsh_write_timeout",
        "dsh_connection_closed",
        "dsh_connect_failed",
        "dsh_stream_error",
        "quota",
        "insufficient balance",
    ];
    if TRANSIENT_CODES.contains(&lower.as_str()) {
        return FailureClass::NetworkTransient;
    }
    FailureClass::BusinessRejection
}

/// 锚定某一份包身份（整包内容指纹）的逐操作事实表。
///
/// 为什么带指纹：shipped/observed 是**持久证据**，必须证明它讲的是当前这个包。
/// 指纹不符 → 整表不参与判定（不猜、不迁移）。
///
/// **归因键边界申报（P2-4）**：完整归因键（平台、插件协议版本、包内容指纹、
/// 规则版本）由 T09 的持久层承担——本结构只锚**包内容指纹**这一维，平台与插件
/// 协议版本是 store 的主键列/查询条件，不在本结构内重复存储。本任务不扩字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchoredOperations {
    /// 该事实表锚定的**整包内容** SHA-256（来自 `contract::read_report` 的
    /// `evidence.package_sha256`；空串 = 未采到，不锚定任何包）。
    pub package_sha256: String,
    /// 该包下已具备该档证据的操作。
    pub operations: BTreeSet<Operation>,
}

/// 判定上下文：**全部是已装载事实**（临界区只读它，不读 DB、不跑扫描）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OperationContext {
    /// 当前目标可归因且（写入时）连接在场；断开 → 所有操作撤回。
    pub target_fresh: bool,
    /// 自动适配开关生效值（默认开启；关闭 → 契约档新写动作撤回）。
    pub auto_adapt_enabled: bool,
    /// 只读**读链**（历史/状态查询）是否已有成功观测。
    ///
    /// Spec §6.2 第 4 档：只读观测「辅助判定当前上下文；不能单独解锁写能力」。
    /// 实现上它**只**为 [`Operation::Read`] 提供 `ObservedSuccess` 级证据
    /// （读链事实就是 Read 的成功）；其它只读目录（`SessionSearch` / `SkillsList`
    /// / `CommandsList`）需要各自的契约匹配或本机真实成功记录，**不共享**本标志。
    /// 写操作的证据门只认 shipped / observed / contract 三档。
    pub read_evidence: bool,
    /// 本阶段**逐操作**已实现事实（未列出的操作 = 未接入，前端不猜）。
    pub implemented: BTreeSet<Operation>,
    /// 当前包的**整包内容** SHA-256——**必须**来自同一目标最近一次
    /// `contract::read_report` 的 `evidence.package_sha256`。
    ///
    /// **禁止**填 `LocalHost.package_sha256`：那是 identity 的包描述符级快速身份
    /// （Info.plist / app-update.yml 哈希），与内容指纹不同层；误用会让契约证据
    /// 永远 `operation_contract_stale`。最省事的正确路径是
    /// [`OperationContext::anchor_to`]。
    pub current_package_sha256: String,
    /// 当前生效的契约规则代际（必须与证据 `rule_version` 一致才复用）。
    pub contract_rule_version: u32,
    /// 该 build 的逐操作 shipped 证据（精确到操作，不批量推导）。
    pub shipped: Option<AnchoredOperations>,
    /// 本机该操作的**真实成功**证据（持久；暂时失败不抹去）。
    pub observed: Option<AnchoredOperations>,
    /// 当前运行包的契约匹配（`None` = 尚未识别）。
    pub contract: Option<ContractEvidence>,
    /// 预算受限/来源不可读而「暂不可识别」的操作集（不是永久不支持）。
    pub contract_inconclusive: BTreeSet<Operation>,
    /// 契约识别失败原因（机器码 + 诊断；`None` = 未尝试或成功）。
    pub contract_error: Option<String>,
    /// **已被协议漂移撤销**的操作集（持久结论；重新识别契约后才清）。
    ///
    /// 为什么是集合而不是「最近一次失败单槽」（P2-1）：单槽会被另一操作的无关
    /// 失败覆盖，导致已漂移的操作在未重识别时**复活**（fail-open）。这里每个
    /// 操作各自持有一条结论，网络/业务失败不写入本集合。
    pub drifted: BTreeSet<Operation>,
    /// 认证失效（401/403/unpaired/peer 撤销）：撤回全部操作。
    ///
    /// 与漂移分开：认证失效是**目标级**结论，漂移是**操作级**结论。
    pub auth_invalid: bool,
}

impl OperationContext {
    /// 把上下文锚定到一份契约报告的证据（P1-2：让正确路径成为最省事路径）。
    ///
    /// 效果（语义申报，逐条都有测试锁定）：
    /// * **异代报告整体拒绝**（C-3，T09 接线前）：`evidence.rule_version` 与当前
    ///   二进制内 [`CONTRACT_RULE_VERSION`] 不一致 → 返回
    ///   `DSH_CONTRACT_RULES_MISMATCH`，**不碰上下文任何字段**（不锚指纹、不清
    ///   漂移、不改契约表）——旧代报告的 matches 讲的是另一代规则下的结论，把它
    ///   当「实际重读且匹配」清漂移，会让同指纹 shipped 复活已漂移操作（复审实
    ///   证：旧代 28/28 报告 + 同指纹 shipped → Send 复活）。**签名申报**：本方
    ///   法按值接手并返回 `Result<Self, DshError>`；Err 即拒绝，调用方持有的原
    ///   上下文不受影响（试锚请传 clone）。
    /// * `contract_rule_version` = **当前二进制内的 [`CONTRACT_RULE_VERSION`]**，
    ///   **绝不**复制 `evidence.rule_version`——验收③（规则换代后旧证据不复用）
    ///   正是靠「上下文代际 vs 证据代际」的比较实现；若这里复制证据版本，两边
    ///   永远相等，旧证据会被"升级"成当前代际放行（Bl-1；异代报告现已在本入口
    ///   门口被拒，求值侧的代际比对仍保留作第二道锁）。
    /// * `current_package_sha256` = `evidence.package_sha256`（整包内容指纹）；
    ///   但报告指纹为**空**时（预算受限、未读整包）**保留调用方已有的非空值**——
    ///   预算受限不得连带撤掉按许可保留的 shipped/observed 证据（P2-C）。
    /// * `contract` = 该证据；`contract_inconclusive` = 报告的「暂不可识别」并集；
    ///   `contract_error` = `None`。
    /// * **按新结论解除 `drifted`**（轮 3 Major-1 / 轮 4 N1 统一语义）：漂移只在
    ///   该操作被**实际重读且匹配**（`evidence.matches` 命中）时解除——干净报告、
    ///   预算受限、部分受限三种形态**同一规则**。干净报告里「重读但未匹配」的
    ///   操作**没有**可解除漂移的新结论：它可能正是漂移形状本身的证据，把「未
    ///   匹配」当「已重识别」会让 shipped 档把漂移结论洗掉（fail-open：漂移的
    ///   accepted 形状 → 干净报告 27/28 → anchor 后 Send 复活）。`inconclusive`/
    ///   `unresolved` 覆盖的操作自然不在 matches 里，同样保留漂移。需要无条件
    ///   全清的调用方（自担保这次重识别完整可信）走
    ///   [`OperationContext::clear_drift_after_reidentification`]。
    /// * **不动 `auth_invalid`**：认证失效是目标级状态，重识别契约不能证明重新
    ///   授权成功；复位走 [`OperationContext::clear_auth_invalid`]。
    ///
    /// 调用方只需再给出 `implemented` / `shipped` / `observed` / `auto_adapt_enabled`
    /// 等其余事实。
    pub fn anchor_to(self, report: &super::contract::ContractReadReport) -> Result<Self, DshError> {
        // C-3（T09 接线前）：异代报告整体拒绝——锚定前先验代际。
        if report.evidence.rule_version != super::contract::CONTRACT_RULE_VERSION {
            return Err(DshError::new(
                codes::CONTRACT_RULES_MISMATCH,
                format!(
                    "anchor_to: report rule_version {} != current rule table {}",
                    report.evidence.rule_version,
                    super::contract::CONTRACT_RULE_VERSION
                ),
            ));
        }
        let mut this = self;
        if !report.evidence.package_sha256.is_empty() {
            this.current_package_sha256 = report.evidence.package_sha256.clone();
        }
        this.contract_rule_version = super::contract::CONTRACT_RULE_VERSION;
        this.contract_inconclusive = report.undecided();
        this.contract = Some(report.evidence.clone());
        this.contract_error = None;
        // N1（轮 4）：三种报告形态统一——只解除「实际重读且匹配」的操作。
        // 未匹配 ≠ 已重识别：未匹配可能恰恰是漂移本身的形状证据。
        this.drifted
            .retain(|operation| !report.evidence.matches.contains_key(operation));
        Ok(this)
    }

    /// 复位认证失效（目标级状态：重新授权成功 / 宿主重建上下文时由调用方显式清）。
    ///
    /// 与漂移（操作级、由重识别清除）分开：认证失效不可能因契约重识别自动好了。
    pub fn clear_auth_invalid(&mut self) {
        self.auth_invalid = false;
    }

    /// 记录一次原生失败（**store 的唯一写入口**；本方法只更新两份持久结论）。
    ///
    /// 语义（P2-1）：
    /// * `ProtocolDrift` → 把**该操作**加入 [`OperationContext::drifted`]（只撤它）；
    /// * `AuthInvalid` → 置 [`OperationContext::auth_invalid`]（撤全部）；
    /// * `BusinessRejection` / `NetworkTransient` → **不写任何结论**（额度不足、
    ///   队列项不存在、断链等不得降级能力，也不得抹掉既有漂移结论）。
    ///
    /// 返回本次失败被采纳的结论（`None` = 不影响能力）。
    pub fn record_native_failure(
        &mut self,
        operation: Operation,
        native_code: &str,
    ) -> Option<FailureClass> {
        match classify_native_failure(native_code) {
            FailureClass::ProtocolDrift => {
                self.drifted.insert(operation);
                Some(FailureClass::ProtocolDrift)
            }
            FailureClass::AuthInvalid => {
                self.auth_invalid = true;
                Some(FailureClass::AuthInvalid)
            }
            FailureClass::BusinessRejection => None,
            FailureClass::NetworkTransient => None,
        }
    }

    /// 契约**完整**重识别成功后清除**全部**漂移结论（漂移只在重识别后解除，
    /// 不由时间或无关操作解除）。由 T09 store 在写入新证据时调用。
    ///
    /// **分工申报（Nit-2，轮 3）**：本方法是「全清」原语——调用方**自担保**这次
    /// 重识别完整可信（非预算受限）；报告驱动的入口请走
    /// [`OperationContext::anchor_to`]，它按报告的实际结论**选择性**解除（预算
    /// 受限只清实际重读且匹配的操作，见其文档 Major-1）。把本方法用在预算受限
    /// 的「重识别」上会重新引入 fail-open。
    pub fn clear_drift_after_reidentification(&mut self) {
        self.drifted.clear();
    }

    fn anchored_shipped(&self, operation: Operation) -> bool {
        anchored_contains(&self.shipped, &self.current_package_sha256, operation)
    }

    fn anchored_observed(&self, operation: Operation) -> bool {
        anchored_contains(&self.observed, &self.current_package_sha256, operation)
    }

    /// 契约匹配档：证据存在、包指纹一致、规则代际一致。
    fn contract_state(&self, operation: Operation) -> ContractState {
        let Some(contract) = self.contract.as_ref() else {
            return ContractState::None;
        };
        if contract.package_sha256.is_empty()
            || contract.package_sha256 != self.current_package_sha256
        {
            return ContractState::PackageStale;
        }
        if contract.rule_version != self.contract_rule_version {
            return ContractState::RulesStale;
        }
        if contract.matches.contains_key(&operation) {
            ContractState::Matched
        } else {
            ContractState::Unmatched
        }
    }
}

fn anchored_contains(
    table: &Option<AnchoredOperations>,
    current_package_sha256: &str,
    operation: Operation,
) -> bool {
    let Some(table) = table else {
        return false;
    };
    // 空包指纹不锚定任何证据（identity 未采到包内容哈希时的诚实缺省）。
    !current_package_sha256.is_empty()
        && table.package_sha256 == current_package_sha256
        && table.operations.contains(&operation)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContractState {
    None,
    Matched,
    Unmatched,
    PackageStale,
    RulesStale,
}

/// 纯计算求值：每个操作独立判定；任何「没有证据」都落到不允许（不猜）。
pub fn evaluate(context: &OperationContext) -> EffectiveOperations {
    let mut operations = BTreeMap::new();
    for operation in Operation::ALL {
        operations.insert(operation, evaluate_one(operation, context));
    }
    let revision = revision_of(&operations);
    EffectiveOperations {
        revision,
        operations,
    }
}

fn evaluate_one(operation: Operation, context: &OperationContext) -> OperationStatus {
    // 1) 实现事实：未接入的操作显式 false（不因证据存在而伪造）。
    if !context.implemented.contains(&operation) {
        return OperationStatus {
            implemented: false,
            allowed: false,
            verified: false,
            requires_auto_adapt: false,
            evidence: EvidenceSource::None,
            reason_code: Some(reason::NOT_IMPLEMENTED.into()),
        };
    }

    let read_only = is_read_only(operation);
    let shipped = context.anchored_shipped(operation);
    let observed = context.anchored_observed(operation);
    let contract = context.contract_state(operation);
    // 只读运行观测（`read_evidence`）= **读链**（历史/状态查询）的成功事实，只覆盖
    // `Operation::Read`（N-3 / Spec §6.2 第 4 档「辅助判定当前上下文；不能单独解锁
    // 写」）。其它只读目录（Skills/Commands/Search）不共享读链证据：它们要么有
    // 各自的契约匹配，要么有各自的本机真实成功记录。
    let read_observed = operation == Operation::Read && context.read_evidence;
    let evidence = if shipped {
        EvidenceSource::Shipped
    } else if observed || read_observed {
        EvidenceSource::ObservedSuccess
    } else if contract == ContractState::Matched {
        EvidenceSource::ContractMatched
    } else {
        EvidenceSource::None
    };
    // verified = 证据锚定「当前目标」（见模块文档「verified 语义」）。
    let verified = evidence != EvidenceSource::None;
    // requires_auto_adapt 只标「确实受自动适配活门约束」的操作：契约匹配档的
    // **写**动作（N-1：只读契约项不受活门约束，故为 false；shipped/observed 也
    // 不依赖开关）。此标志与下面实际门一一对应。
    let requires_auto_adapt =
        !shipped && !observed && !read_only && contract == ContractState::Matched;

    // 2) 目标/认证门（最先撤回，任何证据都不能越过）。
    if context.auth_invalid {
        return denied(
            evidence,
            verified,
            requires_auto_adapt,
            reason::AUTH_INVALID,
        );
    }
    if !context.target_fresh {
        return denied(
            evidence,
            verified,
            requires_auto_adapt,
            reason::TARGET_STALE,
        );
    }

    // 3) 协议漂移：**按操作**持久结论，只撤被漂移的操作（P2-1）。
    if context.drifted.contains(&operation) {
        return denied(
            evidence,
            verified,
            requires_auto_adapt,
            reason::CONTRACT_DRIFT,
        );
    }

    // 4) 证据门。
    let allowed_by_evidence =
        shipped || observed || read_observed || contract == ContractState::Matched;
    if !allowed_by_evidence {
        // 「暂不可识别」优先于「证据过期/未匹配」：预算受限是更强的陈述
        // （我们根本没读到该来源，而不是读到了并且不匹配）。
        let code = if context.contract_inconclusive.contains(&operation) {
            reason::INCONCLUSIVE
        } else {
            match contract {
                ContractState::PackageStale => reason::CONTRACT_STALE,
                ContractState::RulesStale => reason::RULES_STALE,
                ContractState::None if context.contract_error.is_some() => {
                    reason::CONTRACT_READ_FAILED
                }
                ContractState::None => reason::NO_EVIDENCE,
                // N-6：不 panic；契约表没命中就是「未匹配」。
                ContractState::Unmatched | ContractState::Matched => reason::CONTRACT_UNMATCHED,
            }
        };
        return denied(evidence, verified, requires_auto_adapt, code);
    }

    // 自动适配门：只读操作与 shipped/observed 档不受影响；契约档写动作受开关约束。
    let gated_by_auto_adapt = contract == ContractState::Matched && !shipped && !observed;
    if gated_by_auto_adapt && !read_only && !context.auto_adapt_enabled {
        return denied(
            evidence,
            verified,
            requires_auto_adapt,
            reason::AUTO_ADAPT_OFF,
        );
    }

    OperationStatus {
        implemented: true,
        allowed: true,
        verified,
        requires_auto_adapt,
        evidence,
        reason_code: None,
    }
}

fn denied(
    evidence: EvidenceSource,
    verified: bool,
    requires_auto_adapt: bool,
    code: &'static str,
) -> OperationStatus {
    OperationStatus {
        implemented: true,
        allowed: false,
        verified,
        requires_auto_adapt,
        evidence,
        reason_code: Some(code.to_string()),
    }
}

/// 事实指纹：对**稳定序列化**的逐操作事实做 FNV-1a（同 Codex `revision_of` 的
/// 事实指纹思路：不用计数器，避免「事实没变的重复求值」也换 revision）。
fn revision_of(operations: &BTreeMap<Operation, OperationStatus>) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x100_0000_01b3;
    let mut hash: u64 = FNV_OFFSET;
    let mut mix = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
    };
    for (operation, status) in operations {
        mix(operation.as_str().as_bytes());
        mix(&[status.implemented as u8]);
        mix(&[status.allowed as u8]);
        mix(&[status.verified as u8]);
        mix(&[status.requires_auto_adapt as u8]);
        mix(&[match status.evidence {
            EvidenceSource::None => 0,
            EvidenceSource::Shipped => 1,
            EvidenceSource::ContractMatched => 2,
            EvidenceSource::ObservedSuccess => 3,
        }]);
        if let Some(code) = &status.reason_code {
            mix(code.as_bytes());
        }
    }
    hash
}

impl EffectiveOperations {
    /// 该操作当前是否允许：**implemented ∧ allowed**（未实现/未允许 → false）。
    ///
    /// 本访问器按 T08 收敛在 `capability.rs`（`model.rs` 的既有定义不动）；
    /// 前端/写入口只消费这一结论，不自行拼按钮。
    pub fn allowed(&self, operation: Operation) -> bool {
        self.operations
            .get(&operation)
            .is_some_and(|status| status.implemented && status.allowed)
    }

    /// 该操作的状态（未知 → `None`）。
    pub fn get(&self, operation: Operation) -> Option<&OperationStatus> {
        self.operations.get(&operation)
    }
}
