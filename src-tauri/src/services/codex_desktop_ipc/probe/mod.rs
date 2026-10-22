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
}

/// 账本结论的精简视图（adapter 只关心「能否复用」与「退避到点没」）。
///
/// #172/T9a 追加两个**只读**字段（探测事实要用）：`stage`（最后探测级 P0..P3）
/// 与 `error_code`（账本里的错误码）——不改既有字段的语义。
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
}
