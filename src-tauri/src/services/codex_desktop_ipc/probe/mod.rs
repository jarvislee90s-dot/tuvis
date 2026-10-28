//! #172 只读探测（P0–P3）与能力账本的模块门面。
//!
//! 设计依据（2026-10-21 定稿 v4）：`profile_for` 从「准入白名单」降级为「证据缓存」——
//! 版本号只做归因与缓存键，能力一律来自证据或探测。本模块承载：
//!
//! * [`fingerprint`]：探测键（`platform|build|cli_id|epoch`）与平台指纹；
//! * `ledger` / `runner` / `schema_probe` 随后续任务落地（账本、读链执行器、官方
//!   schema 探针）。
//!
//! 纪律：本模块**不发写帧**，也不解释「读不通」为「协议不存在」（见设计 §4.5）。

pub mod desktop_contract;
pub mod fingerprint;
pub mod runner;
pub mod schema_probe;

use crate::database::dao::codex_ipc_probe::ProbeState;

/// 账本 `capability` 列的能力词（读链探测做 `state`；send/command 等随后续任务）。
pub const CAP_STATE: &str = "state";

/// 账本 `capability` 列的 **schema 探针**能力词（#172/T7-R4）。
///
/// 与 `state` 并列的一行：同一 `(probe_key, capability)` 主键下，
/// `state` 记「读链能不能用」，`schema` 记「官方 schema 能不能给出审批决策词」。
/// 结论词沿用账本既有三态：`probed`（取到 decision 枚举）/ `inconclusive`
/// （取不到，带错误码）——**不引入新状态词**（`capability` 是字符串列，无需改表结构）。
pub const CAP_SCHEMA: &str = "schema";

/// S1：账本 `capability` 列的**发送**能力词（兼容发送受理结论的落点；词与
/// `codex_ipc_probe` 既有测试里的 "send" 一致，单一出处在此，remote 层只做别名）。
pub const CAP_SEND: &str = "send";

/// 探测账本接口（#172）：由 remote 层用 `DeviceStore` 实现注入 adapter；测试可注入
/// 内存实现。语义 = `codex_ipc_probe` 表的读写（见 `database::dao::codex_ipc_probe`）。
///
/// 为什么用 trait 而不是让 adapter 直接摸 DB：adapter 是纯协议层（测试里全部走
/// `ScriptedPeer` + 内存库），账本的可替换点是「能力证据来源」，不是协议本身。
pub trait ProbeLedger: Send + Sync {
    /// 读某 `(probe_key, capability)` 的结论；无行 → `None`。
    fn get(&self, probe_key: &str, capability: &str) -> Option<ProbeLedgerEntry>;
    /// 落账（upsert；`attempts` 累加、`next_retry_at` 按退避算）。
    ///
    /// `fingerprint`：**证据指纹**——只对 `capability = "schema"` 的行有意义（官方
    /// schema 探针的「文件名+大小」sha256）；其余能力传 `None`（无指纹可给）。
    fn record(
        &self,
        probe_key: &str,
        capability: &str,
        outcome: &runner::ProbeOutcome,
        fingerprint: Option<&str>,
    );
    /// S1：读持久 **schema 决策词证据**（重启恢复零 CLI 重跑的来源；无 → `None`）。
    /// 缺省 `None`：内存账本/老实现不必实现（生产 `StoreProbeLedger` 落
    /// `codex_ipc_schema_evidence` 表）。
    fn load_schema_evidence(&self, probe_key: &str) -> Option<PersistedSchemaEvidence> {
        let _ = probe_key;
        None
    }
    /// S1：写持久 **schema 决策词证据**（成功取证时与账本行一并落）。缺省空操作。
    fn store_schema_evidence(
        &self,
        probe_key: &str,
        evidence: &schema_probe::SchemaEvidence,
        at_ms: i64,
    ) {
        let _ = (probe_key, evidence, at_ms);
    }
}

/// 账本结论的精简视图（adapter 只关心「能否复用」与「退避到点没」）。
///
/// #172/T9a 追加两个**只读**字段（探测事实要用）：`stage`（最后探测级 P0..P3）
/// 与 `error_code`（账本里的错误码）——不改既有字段的语义。
///
/// S1（2026-10-23 设计 §3.2）追加三个**只读**字段（成功证据与失败观测分离）：
/// * `evidenced`：该 (key, capability) 是否有过**成功取证**（证据列或旧数据兼容的
///   `state∈probed|verified`）——跨重启复用看它，不看 `state`；
/// * `evidenced_at` / `last_seen_at`：成功证据时间戳与最近观测时间戳（毫秒），
///   DTO（适配状态）用它们表达「已取证」与「最近失败观测」两条线。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeLedgerEntry {
    pub state: ProbeState,
    pub next_retry_at: Option<i64>,
    /// 最后探测级（`P0`..`P3`）；旧行/无行 → `None`。
    pub stage: Option<String>,
    /// 账本里的错误码（`failed` / `inconclusive` 行有值）；`probed` 行 → `None`。
    pub error_code: Option<String>,
    /// 证据指纹（只对 `capability = "schema"` 的行有值：官方 schema 探针的
    /// 内容指纹）；其余能力 → `None`。
    pub fingerprint: Option<String>,
    /// S1：有过成功取证（不随后续失败观测消失）。
    pub evidenced: bool,
    /// S1：最后一次成功取证时间（毫秒；无 → `None`）。
    pub evidenced_at: Option<i64>,
    /// S1：最近一次观测时间（毫秒；无 → `None`）。
    pub last_seen_at: Option<i64>,
}

/// S1：持久 **schema 决策词证据**（`codex_ipc_schema_evidence` 一行的精简视图）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedSchemaEvidence {
    pub fingerprint: String,
    pub command_decisions: Vec<String>,
    pub file_decisions: Vec<String>,
    pub evidenced_at: i64,
}
