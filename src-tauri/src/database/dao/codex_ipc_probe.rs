//! Codex Desktop IPC 能力探测账本（#172）：`codex_ipc_probe` 的读写。
//!
//! 目的（设计 §4.6）：同一 `(probe_key, capability)` 的探测结论**复用不重探**，同时
//! 沉淀「Codex build × Tuvis 能力」的匹配数据（后续可据人工证据把某个 build 提升进
//! shipped 表）。
//!
//! 纪律（照抄设计，别在这里放宽）：
//! * 一行 = 一个 `(probe_key, capability)`；**upsert 覆盖**，`attempts` 只累加；
//! * `state` 只写 `probed` / `failed` / `inconclusive`——**`verified` 只来自 shipped 表**，
//!   账本永不写（读侧保留该词，兼容未来人工提升后留下的行）；
//! * 不存正文 / `threadId` / 原始帧（只存 stage / 错误码 / 耗时，对齐审计隐私纪律）；
//! * `inconclusive` 按 `30s → 2min → 10min` 退避（不无限重试）；`failed` 不自动重试，
//!   换 build / `probe_epoch` 变更 / 用户显式重探才再来。
//! * S1（2026-10-23 设计 §3.2）：`state` 承担「**最近一次观测**」，`evidence_at` /
//!   `evidence_stage` 承担「**最后一次成功取证**」——一次成功取证建立可持久复用的
//!   证据，**不因后续超时/断链被失败行覆盖**；「已取证」判据见 [`ProbeRow::has_evidence`]。

use rusqlite::{params, Connection};

/// 探测状态（`codex_ipc_probe.state`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeState {
    /// **只读值**：shipped 表的证据（账本永不写这个值，只可能读到历史/人工提升行）。
    Verified,
    /// 探测通过（读链 P0–P3 已实证）。
    Probed,
    /// 明确失败（协议错误 / 方法不存在）——不自动重试。
    Failed,
    /// 不确定（超时 / 无 owner / 解析失败）——按退避重试。
    Inconclusive,
    /// 未知 / 无行。
    Unknown,
}

impl ProbeState {
    /// 线上/落库词（与设计稿、前端 reasonCode 文案对齐）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Probed => "probed",
            Self::Failed => "failed",
            Self::Inconclusive => "inconclusive",
            Self::Unknown => "unknown",
        }
    }

    /// 解析落库词；未知词按 `Unknown`（前向兼容，不 panic）。
    pub fn parse(raw: &str) -> Self {
        match raw {
            "verified" => Self::Verified,
            "probed" => Self::Probed,
            "failed" => Self::Failed,
            "inconclusive" => Self::Inconclusive,
            _ => Self::Unknown,
        }
    }
}

/// 退避阶梯（毫秒）：30s → 2min → 10min（第 3 次及以后恒 10min）。
pub const RETRY_BACKOFF_MS: [i64; 3] = [30_000, 120_000, 600_000];

/// 第 `attempts` 次失败后的重试间隔（`attempts` 从 1 起；0 按首次处理）。
pub fn retry_backoff_ms(attempts: u32) -> i64 {
    match attempts {
        0 | 1 => RETRY_BACKOFF_MS[0],
        2 => RETRY_BACKOFF_MS[1],
        _ => RETRY_BACKOFF_MS[2],
    }
}

/// 账本一行（读取视图；不含正文/线程信息）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeRow {
    pub probe_key: String,
    pub capability: String,
    pub state: ProbeState,
    /// 最后通过的探测级（P0..P3；失败时可为最后尝试级）。
    pub probe_stage: Option<String>,
    pub error_code: Option<String>,
    /// 证据指纹（只对 `capability='schema'` 的行有意义：官方 schema 探针的
    /// 「文件名+大小」sha256）。让「build × 能力」数据集能追到具体 schema 版本。
    pub fingerprint: Option<String>,
    /// 本 (key, capability) 的累计探测次数（含首次）。
    pub attempts: u32,
    pub first_seen_at: i64,
    pub last_seen_at: i64,
    /// 仅 `inconclusive` 有值：到点前不再探测。
    pub next_retry_at: Option<i64>,
    pub last_duration_ms: Option<u64>,
    /// S1：**最后一次成功取证**的时间（毫秒；NULL = 本实现没写过成功、也没读到旧
    /// 成功行的提升值）。与 `state`（最近观测）分离——超时/断链不改写本列。
    pub evidence_at: Option<i64>,
    /// S1：最后一次成功取证的探测级。
    pub evidence_stage: Option<String>,
}

impl ProbeRow {
    /// 「已取证」判据（S1 单点）：本实现写过的成功证据列，**或**旧数据兼容——
    /// 旧实现只有 `state` 一个字段，`state ∈ probed|verified` 即代表有过成功取证。
    /// 老库 migration 只补列不改行，旧成功行靠这一臂继续可复用。
    pub fn has_evidence(&self) -> bool {
        self.evidence_at.is_some()
            || matches!(self.state, ProbeState::Probed | ProbeState::Verified)
    }
}

/// 读一行（无行 → `None`；`state` 词异常 → `Unknown`）。
pub fn get_conn(
    conn: &Connection,
    probe_key: &str,
    capability: &str,
) -> rusqlite::Result<Option<ProbeRow>> {
    let mut stmt = conn.prepare(
        "SELECT probe_key, capability, state, probe_stage, error_code, fingerprint, attempts,
                first_seen_at, last_seen_at, next_retry_at, last_duration_ms,
                evidence_at, evidence_stage
         FROM codex_ipc_probe WHERE probe_key = ?1 AND capability = ?2",
    )?;
    let mut rows = stmt.query(params![probe_key, capability])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let state_raw: String = row.get(2)?;
    Ok(Some(ProbeRow {
        probe_key: row.get(0)?,
        capability: row.get(1)?,
        state: ProbeState::parse(&state_raw),
        probe_stage: row.get(3)?,
        error_code: row.get(4)?,
        fingerprint: row.get(5)?,
        attempts: row.get::<_, i64>(6)?.max(0) as u32,
        first_seen_at: row.get(7)?,
        last_seen_at: row.get(8)?,
        next_retry_at: row.get(9)?,
        last_duration_ms: row.get::<_, Option<i64>>(10)?.map(|v| v.max(0) as u64),
        evidence_at: row.get(11)?,
        evidence_stage: row.get(12)?,
    }))
}

/// 探测通过：`state = probed`，清退避（`next_retry_at = NULL`）。
///
/// 形参**刻意与 SQL 列序一一对应**（DAO 层的「列即参数」惯例）；补 `fingerprint`
/// 列后超过 clippy 的 7 参上限——这是**刻意**的（避免为省一个参数而把列序打乱、
/// 引入与表结构脱节的位置参数），故显式豁免并在此写明理由。
#[allow(clippy::too_many_arguments)]
pub fn upsert_ok_conn(
    conn: &Connection,
    probe_key: &str,
    capability: &str,
    probe_stage: Option<&str>,
    fingerprint: Option<&str>,
    duration_ms: Option<u64>,
    now_ms: i64,
) -> rusqlite::Result<()> {
    upsert_conn(
        conn,
        probe_key,
        capability,
        ProbeState::Probed,
        probe_stage,
        None,
        fingerprint,
        duration_ms,
        now_ms,
        None,
    )
}

/// 明确失败：`state = failed`，不设退避（不自动重试）。
///
/// 形参与 SQL 列序一一对应（见 [`upsert_ok_conn`] 的说明）；超参上限是**刻意**豁免。
#[allow(clippy::too_many_arguments)]
pub fn upsert_fail_conn(
    conn: &Connection,
    probe_key: &str,
    capability: &str,
    probe_stage: Option<&str>,
    error_code: &str,
    fingerprint: Option<&str>,
    duration_ms: Option<u64>,
    now_ms: i64,
) -> rusqlite::Result<()> {
    upsert_conn(
        conn,
        probe_key,
        capability,
        ProbeState::Failed,
        probe_stage,
        Some(error_code),
        fingerprint,
        duration_ms,
        now_ms,
        None,
    )
}

/// 不确定：`state = inconclusive` + 按**本次（含）累计次数**给退避窗口。
///
/// 形参与 SQL 列序一一对应（见 [`upsert_ok_conn`] 的说明）；超参上限是**刻意**豁免。
#[allow(clippy::too_many_arguments)]
pub fn upsert_inconclusive_conn(
    conn: &Connection,
    probe_key: &str,
    capability: &str,
    probe_stage: Option<&str>,
    error_code: &str,
    fingerprint: Option<&str>,
    duration_ms: Option<u64>,
    now_ms: i64,
) -> rusqlite::Result<()> {
    // 退避按「本次」的累计次数算：首探 → 30s，第二次 → 2min，第三次起 → 10min。
    // 先读再写（同一 probe_key 由 runner 单飞，不存在并发写同一行）。
    let prev_attempts = conn
        .query_row(
            "SELECT attempts FROM codex_ipc_probe WHERE probe_key = ?1 AND capability = ?2",
            params![probe_key, capability],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0)
        .max(0) as u32;
    let next_retry_at = now_ms + retry_backoff_ms(prev_attempts.saturating_add(1));
    upsert_conn(
        conn,
        probe_key,
        capability,
        ProbeState::Inconclusive,
        probe_stage,
        Some(error_code),
        fingerprint,
        duration_ms,
        now_ms,
        Some(next_retry_at),
    )
}

/// 单点 upsert（`ON CONFLICT(probe_key, capability)`）：
/// * `attempts` 累加（`+1`），`first_seen_at` **保留首次**（冲突时不覆盖）；
/// * `last_seen_at` / `state` / `stage` / `error_code` / 耗时按本次覆盖；
/// * `fingerprint`：本次有值才覆盖（失败观测不带指纹时保留既有——schema 行的指纹是
///   成功证据的一部分，不得被后续 Unknown 抹掉）；
/// * **成功证据列**（S1）：成功（`evidence_at = now`）推进；失败观测**原样保留**，
///   且对「旧实现只有 state 字段」的存量成功行按旧值提升（`evidence_at ← 旧行
///   last_seen_at`）——旧成功从此不被失败行抹掉（AGENTS.md 采集域纪律：状态字段
///   语义变化当迁移处理，本 CASE 是写侧的提升点，migration 只补列不改行）。
#[allow(clippy::too_many_arguments)]
fn upsert_conn(
    conn: &Connection,
    probe_key: &str,
    capability: &str,
    state: ProbeState,
    probe_stage: Option<&str>,
    error_code: Option<&str>,
    fingerprint: Option<&str>,
    duration_ms: Option<u64>,
    now_ms: i64,
    next_retry_at: Option<i64>,
) -> rusqlite::Result<()> {
    let state_word = state.as_str();
    // 终审 F1：新行（INSERT 臂）只在**成功**观测时写证据列——首次 failed/
    // inconclusive 的证据列必须为 NULL，否则 `has_evidence()` 把从未成功的指纹
    // 判成已取证，退避到点/重启后的重试被短路（旧实现无条件传 now/stage）。
    // 失败臂写 NULL 与下方 ON CONFLICT 的保留/提升语义一致：成功后的失败仍走
    // UPDATE 保留既有证据（`success_then_failure_keeps_success_evidence`）。
    let (evidence_at, evidence_stage): (Option<i64>, Option<&str>) =
        if matches!(state, ProbeState::Probed | ProbeState::Verified) {
            (Some(now_ms), probe_stage)
        } else {
            (None, None)
        };
    conn.execute(
        "INSERT INTO codex_ipc_probe
            (probe_key, capability, state, probe_stage, error_code, fingerprint, attempts,
             first_seen_at, last_seen_at, next_retry_at, last_duration_ms,
             evidence_at, evidence_stage)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT(probe_key, capability) DO UPDATE SET
            state            = excluded.state,
            probe_stage      = excluded.probe_stage,
            error_code       = excluded.error_code,
            fingerprint      = COALESCE(excluded.fingerprint, codex_ipc_probe.fingerprint),
            attempts         = codex_ipc_probe.attempts + 1,
            last_seen_at     = excluded.last_seen_at,
            next_retry_at    = excluded.next_retry_at,
            last_duration_ms = excluded.last_duration_ms,
            evidence_at = CASE
                WHEN excluded.state IN ('probed', 'verified') THEN excluded.evidence_at
                ELSE COALESCE(
                    codex_ipc_probe.evidence_at,
                    CASE WHEN codex_ipc_probe.state IN ('probed', 'verified')
                         THEN codex_ipc_probe.last_seen_at END
                )
            END,
            evidence_stage = CASE
                WHEN excluded.state IN ('probed', 'verified') THEN excluded.probe_stage
                ELSE COALESCE(
                    codex_ipc_probe.evidence_stage,
                    CASE WHEN codex_ipc_probe.state IN ('probed', 'verified')
                         THEN codex_ipc_probe.probe_stage END
                )
            END",
        params![
            probe_key,
            capability,
            state_word,
            probe_stage,
            error_code,
            fingerprint,
            now_ms,
            next_retry_at,
            duration_ms.map(|d| d as i64),
            evidence_at,
            evidence_stage,
        ],
    )?;
    Ok(())
}

/// S1：**schema 决策词持久证据**写入（`codex_ipc_schema_evidence`，upsert 覆盖）。
///
/// 设计 §3.2：schema 只存一个文件指纹不能重建决策词——须持久保存**经过校验**的
/// decision 枚举，重启后零 CLI 重跑即可恢复审批档。仍在每个 pending 上求交集，
/// 缓存绝不直接批准。
pub fn upsert_schema_evidence_conn(
    conn: &Connection,
    probe_key: &str,
    fingerprint: &str,
    command_decisions: &[String],
    file_decisions: &[String],
    now_ms: i64,
) -> rusqlite::Result<()> {
    let command = serde_json::to_string(command_decisions).unwrap_or_else(|_| "[]".into());
    let file = serde_json::to_string(file_decisions).unwrap_or_else(|_| "[]".into());
    conn.execute(
        "INSERT INTO codex_ipc_schema_evidence
            (probe_key, fingerprint, command_decisions, file_decisions, evidence_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(probe_key) DO UPDATE SET
            fingerprint       = excluded.fingerprint,
            command_decisions = excluded.command_decisions,
            file_decisions    = excluded.file_decisions,
            evidence_at       = excluded.evidence_at",
        params![probe_key, fingerprint, command, file, now_ms],
    )?;
    Ok(())
}

/// S1：schema 决策词持久证据行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaEvidenceRow {
    pub probe_key: String,
    pub fingerprint: String,
    pub command_decisions: Vec<String>,
    pub file_decisions: Vec<String>,
    pub evidence_at: i64,
}

/// S1：读 schema 决策词持久证据（无行 → `None`；JSON 损坏 → `None`——不猜）。
pub fn get_schema_evidence_conn(
    conn: &Connection,
    probe_key: &str,
) -> rusqlite::Result<Option<SchemaEvidenceRow>> {
    let mut stmt = conn.prepare(
        "SELECT probe_key, fingerprint, command_decisions, file_decisions, evidence_at
         FROM codex_ipc_schema_evidence WHERE probe_key = ?1",
    )?;
    let mut rows = stmt.query(params![probe_key])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let parse = |raw: String| -> Vec<String> { serde_json::from_str(&raw).unwrap_or_default() };
    Ok(Some(SchemaEvidenceRow {
        probe_key: row.get(0)?,
        fingerprint: row.get(1)?,
        command_decisions: parse(row.get(2)?),
        file_decisions: parse(row.get(3)?),
        evidence_at: row.get(4)?,
    }))
}

/// T02：**逐操作契约证据**的持久行（`codex_ipc_contract_evidence`）。
///
/// 字段只含结构性规则（方法名／基础版本／请求、响应、语义规则），**不含**会话正文、
/// 命令、文件路径或凭据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredContractEvidence {
    pub probe_key: String,
    pub operation: String,
    pub rule_version: u32,
    pub package_sha256: String,
    pub method: String,
    pub base_version: u32,
    pub request_rule: String,
    pub response_rule: String,
    pub semantic_rule: String,
    /// 首次取证时间（毫秒）。
    pub evidenced_at: i64,
    /// 最近观测时间（毫秒；同一证据被再次确认时更新）。
    pub observed_at: i64,
    /// 取证时包文件大小（NULL = 老行未记录 → 读侧不猜，重新识别）。
    pub package_size: Option<i64>,
    /// 取证时包文件 mtime（毫秒）。
    pub package_mtime_ms: Option<i64>,
}

/// T02：写一条逐操作契约证据（主键 `(probe_key, operation, rule_version,
/// package_sha256)`；同键覆盖——幂等，不累加）。
pub fn upsert_contract_evidence_conn(
    conn: &Connection,
    evidence: &StoredContractEvidence,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO codex_ipc_contract_evidence
            (probe_key, operation, rule_version, package_sha256, method, base_version,
             request_rule, response_rule, semantic_rule, evidenced_at, observed_at,
             package_size, package_mtime_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
         ON CONFLICT(probe_key, operation, rule_version, package_sha256) DO UPDATE SET
            method        = excluded.method,
            base_version  = excluded.base_version,
            request_rule  = excluded.request_rule,
            response_rule = excluded.response_rule,
            semantic_rule = excluded.semantic_rule,
            observed_at   = excluded.observed_at,
            package_size  = excluded.package_size,
            package_mtime_ms = excluded.package_mtime_ms",
        params![
            evidence.probe_key,
            evidence.operation,
            evidence.rule_version as i64,
            evidence.package_sha256,
            evidence.method,
            evidence.base_version as i64,
            evidence.request_rule,
            evidence.response_rule,
            evidence.semantic_rule,
            evidence.evidenced_at,
            evidence.observed_at,
            evidence.package_size,
            evidence.package_mtime_ms,
        ],
    )?;
    Ok(())
}

/// T02：读某 `probe_key` 在**指定规则版本**下的全部逐操作契约证据。
///
/// 规则版本与 probe_key（含 platform/build/cli_id/probe_epoch）共同限定复用面：
/// CLI 漂移或规则收紧后，旧行不会被当成新证据。
pub fn get_contract_evidence_conn(
    conn: &Connection,
    probe_key: &str,
    rule_version: u32,
) -> rusqlite::Result<Vec<StoredContractEvidence>> {
    let mut stmt = conn.prepare(
        "SELECT probe_key, operation, rule_version, package_sha256, method, base_version,
                request_rule, response_rule, semantic_rule, evidenced_at, observed_at,
                package_size, package_mtime_ms
         FROM codex_ipc_contract_evidence
         WHERE probe_key = ?1 AND rule_version = ?2
         ORDER BY operation",
    )?;
    let rows = stmt.query_map(params![probe_key, rule_version as i64], |row| {
        Ok(StoredContractEvidence {
            probe_key: row.get(0)?,
            operation: row.get(1)?,
            rule_version: row.get::<_, i64>(2)? as u32,
            package_sha256: row.get(3)?,
            method: row.get(4)?,
            base_version: row.get::<_, i64>(5)? as u32,
            request_rule: row.get(6)?,
            response_rule: row.get(7)?,
            semantic_rule: row.get(8)?,
            evidenced_at: row.get(9)?,
            observed_at: row.get(10)?,
            package_size: row.get(11)?,
            package_mtime_ms: row.get(12)?,
        })
    })?;
    rows.collect()
}

/// H01：记录一条**按目标**的真实成功证据（效果确认后调用；同键幂等更新）。
pub fn record_operation_success_conn(
    conn: &Connection,
    probe_key: &str,
    package_sha256: &str,
    operation: &str,
    rule_version: u32,
    at_ms: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO codex_operation_success
            (probe_key, package_sha256, operation, rule_version, evidence_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(probe_key, package_sha256, operation, rule_version) DO UPDATE SET
            evidence_at = excluded.evidence_at",
        params![
            probe_key,
            package_sha256,
            operation,
            rule_version as i64,
            at_ms
        ],
    )?;
    Ok(())
}

/// H01：读某目标 + 规则版本下已确认成功的操作词（跨规则版本/跨目标不通用）。
pub fn get_operation_success_conn(
    conn: &Connection,
    probe_key: &str,
    package_sha256: &str,
    rule_version: u32,
) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT operation FROM codex_operation_success
         WHERE probe_key = ?1 AND package_sha256 = ?2 AND rule_version = ?3
         ORDER BY operation",
    )?;
    let rows = stmt
        .query_map(
            params![probe_key, package_sha256, rule_version as i64],
            |r| r.get::<_, String>(0),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        conn
    }

    fn count_rows(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM codex_ipc_probe", [], |r| r.get(0))
            .unwrap()
    }

    const KEY: &str = "windows|26.1013.9999.0|-|1";

    /// upsert 幂等：同一 (key, capability) 反复写只有一行，attempts 累加，
    /// first_seen_at 保留首次。
    #[test]
    fn upsert_is_idempotent_single_row_and_accumulates_attempts() {
        let conn = mem();
        upsert_ok_conn(&conn, KEY, "state", Some("P3"), None, Some(420), 1_000).unwrap();
        let first = get_conn(&conn, KEY, "state").unwrap().unwrap();
        assert_eq!(first.state, ProbeState::Probed);
        assert_eq!(first.attempts, 1);
        assert_eq!(first.first_seen_at, 1_000);
        assert_eq!(first.last_seen_at, 1_000);
        assert_eq!(first.probe_stage.as_deref(), Some("P3"));
        assert_eq!(first.error_code, None);
        assert_eq!(first.fingerprint, None);
        assert_eq!(first.next_retry_at, None);
        assert_eq!(first.last_duration_ms, Some(420));

        upsert_ok_conn(&conn, KEY, "state", Some("P3"), None, Some(380), 2_000).unwrap();
        let second = get_conn(&conn, KEY, "state").unwrap().unwrap();
        assert_eq!(count_rows(&conn), 1, "upsert 不是 append：只能一行");
        assert_eq!(second.attempts, 2, "attempts 累加");
        assert_eq!(second.first_seen_at, 1_000, "first_seen_at 保留首次");
        assert_eq!(second.last_seen_at, 2_000);
        assert_eq!(second.last_duration_ms, Some(380));
    }

    /// 终审 F1：**首次**明确失败不得写成成功证据——新指纹第一次探测即 failed 的行
    /// 证据列必须为空（旧 INSERT 无条件写 evidence_at=now，`has_evidence()` 误报
    /// 已取证并跳过后续探测/重启复用）。
    #[test]
    fn first_failure_writes_no_evidence() {
        let conn = mem();
        upsert_fail_conn(
            &conn,
            KEY,
            "state",
            Some("P1"),
            "method_not_found",
            None,
            Some(120),
            1_000,
        )
        .unwrap();
        let row = get_conn(&conn, KEY, "state").unwrap().unwrap();
        assert_eq!(row.evidence_at, None, "首次失败：证据列必须为空");
        assert_eq!(row.evidence_stage, None);
        assert!(
            !row.has_evidence(),
            "首次失败不得被读侧判为已取证（否则退避到点/重启后跳过重试）"
        );
    }

    /// 终审 F1：**首次**不确定（超时/无 owner）同样不得写成成功证据。
    #[test]
    fn first_inconclusive_writes_no_evidence() {
        let conn = mem();
        upsert_inconclusive_conn(
            &conn,
            KEY,
            "state",
            Some("P1"),
            "no-client-found",
            None,
            None,
            1_000,
        )
        .unwrap();
        let row = get_conn(&conn, KEY, "state").unwrap().unwrap();
        assert_eq!(row.evidence_at, None, "首次不确定：证据列必须为空");
        assert_eq!(row.evidence_stage, None);
        assert!(
            !row.has_evidence(),
            "退避到点后仍须重试（不得被假证据短路）"
        );
    }

    /// 明确失败：state=failed + 错误码；不设退避（failed 不自动重试）。
    #[test]
    fn failed_records_error_without_backoff() {
        let conn = mem();
        upsert_fail_conn(
            &conn,
            KEY,
            "state",
            Some("P1"),
            "method_not_found",
            None,
            Some(120),
            5_000,
        )
        .unwrap();
        let row = get_conn(&conn, KEY, "state").unwrap().unwrap();
        assert_eq!(row.state, ProbeState::Failed);
        assert_eq!(row.error_code.as_deref(), Some("method_not_found"));
        assert_eq!(row.probe_stage.as_deref(), Some("P1"));
        assert_eq!(row.next_retry_at, None, "failed 不自动重试");
        assert_eq!(row.attempts, 1);
    }

    /// 不确定：按「本次含」累计次数退避 30s → 2min → 10min，且不再增长。
    #[test]
    fn inconclusive_backoff_ladder() {
        let conn = mem();
        let now = 10_000_000i64;
        upsert_inconclusive_conn(
            &conn,
            KEY,
            "state",
            Some("P1"),
            "no-client-found",
            None,
            None,
            now,
        )
        .unwrap();
        let first = get_conn(&conn, KEY, "state").unwrap().unwrap();
        assert_eq!(first.state, ProbeState::Inconclusive);
        assert_eq!(first.attempts, 1);
        assert_eq!(first.next_retry_at, Some(now + 30_000));

        upsert_inconclusive_conn(
            &conn,
            KEY,
            "state",
            Some("P1"),
            "no-client-found",
            None,
            None,
            now,
        )
        .unwrap();
        let second = get_conn(&conn, KEY, "state").unwrap().unwrap();
        assert_eq!(second.attempts, 2);
        assert_eq!(second.next_retry_at, Some(now + 120_000));

        for expected_attempts in [3u32, 4] {
            upsert_inconclusive_conn(
                &conn,
                KEY,
                "state",
                Some("P1"),
                "no-client-found",
                None,
                None,
                now,
            )
            .unwrap();
            let row = get_conn(&conn, KEY, "state").unwrap().unwrap();
            assert_eq!(row.attempts, expected_attempts);
            assert_eq!(
                row.next_retry_at,
                Some(now + 600_000),
                "第 3 次起恒 10min（不无限增长）"
            );
        }
    }

    /// 账本永不写 `verified`；`verified` 只可能从 shipped 证据读到（读侧兼容）。
    #[test]
    fn ledger_never_writes_verified_but_reads_it() {
        let conn = mem();
        for state in [
            ProbeState::Probed,
            ProbeState::Failed,
            ProbeState::Inconclusive,
        ] {
            assert_ne!(state, ProbeState::Verified, "写侧集合不含 verified");
        }
        upsert_ok_conn(&conn, KEY, "state", Some("P3"), None, None, 1).unwrap();
        assert_eq!(
            get_conn(&conn, KEY, "state").unwrap().unwrap().state,
            ProbeState::Probed
        );
        // 读侧兼容：库里若有 verified 行（人工提升 / 未来迁移）能正确解析
        conn.execute(
            "UPDATE codex_ipc_probe SET state = 'verified' WHERE probe_key = ?1",
            params![KEY],
        )
        .unwrap();
        assert_eq!(
            get_conn(&conn, KEY, "state").unwrap().unwrap().state,
            ProbeState::Verified
        );
    }

    /// 一个 key 多能力 = 多行；不同 key 互不影响（账本行数按「版本 × 能力」增长）。
    #[test]
    fn rows_are_scoped_per_key_and_capability() {
        let conn = mem();
        upsert_ok_conn(&conn, KEY, "state", Some("P3"), None, None, 1).unwrap();
        upsert_ok_conn(&conn, KEY, "send", Some("P4"), None, None, 1).unwrap();
        upsert_ok_conn(
            &conn,
            "windows|other|-|1",
            "state",
            Some("P3"),
            None,
            None,
            1,
        )
        .unwrap();
        assert_eq!(count_rows(&conn), 3);
        assert_eq!(
            get_conn(&conn, KEY, "send").unwrap().unwrap().state,
            ProbeState::Probed
        );
        assert!(get_conn(&conn, KEY, "command").unwrap().is_none());
    }

    /// 无行 / 未知 state 词的解析契约。
    #[test]
    fn missing_row_and_unknown_state_word() {
        let conn = mem();
        assert!(get_conn(&conn, KEY, "state").unwrap().is_none());
        assert_eq!(ProbeState::parse("whatever"), ProbeState::Unknown);
        assert_eq!(ProbeState::parse("probed"), ProbeState::Probed);
    }

    /// **schema 指纹落库**（#172/T7-R4 补列）：`capability='schema'` 的行带官方
    /// schema 探针的内容指纹；读回一致，且**后写覆盖前值**（upsert 语义）。
    #[test]
    fn schema_fingerprint_round_trips_and_overwrites() {
        let conn = mem();
        upsert_ok_conn(
            &conn,
            KEY,
            "schema",
            Some("P3"),
            Some("sha256-of-names-and-sizes"),
            Some(230),
            1_000,
        )
        .unwrap();
        let row = get_conn(&conn, KEY, "schema").unwrap().unwrap();
        assert_eq!(
            row.fingerprint.as_deref(),
            Some("sha256-of-names-and-sizes"),
            "schema 行必须能追到具体 schema 版本"
        );
        assert_eq!(row.state, ProbeState::Probed);

        upsert_ok_conn(
            &conn,
            KEY,
            "schema",
            Some("P3"),
            Some("sha256-after-upgrade"),
            Some(240),
            2_000,
        )
        .unwrap();
        let row2 = get_conn(&conn, KEY, "schema").unwrap().unwrap();
        assert_eq!(count_rows(&conn), 1, "仍是一行");
        assert_eq!(
            row2.fingerprint.as_deref(),
            Some("sha256-after-upgrade"),
            "后写覆盖前值"
        );
    }

    // ============================================================
    // S1（2026-10-23 设计 §3.2）：成功证据与失败观测分离
    // ============================================================

    /// 成功→超时/失败**不覆盖成功**：`state` 列承担「最近观测」，`evidence_at`/
    /// `evidence_stage` 承担「最后一次成功取证」——失败观测写入后证据列原样保留，
    /// 新成功才推进证据列（设计 §3.2：不能一个可覆盖状态字段承担二者全部语义）。
    #[test]
    fn success_then_failure_keeps_success_evidence() {
        let conn = mem();
        upsert_ok_conn(&conn, KEY, "state", Some("P3"), None, Some(420), 1_000).unwrap();
        let ok = get_conn(&conn, KEY, "state").unwrap().unwrap();
        assert_eq!(ok.evidence_at, Some(1_000), "成功取证落证据列");
        assert_eq!(ok.evidence_stage.as_deref(), Some("P3"));
        assert!(ok.has_evidence());

        // 之后一次超时（inconclusive）：观测覆盖，证据**不**覆盖
        upsert_inconclusive_conn(
            &conn,
            KEY,
            "state",
            Some("P1"),
            "no-client-found",
            None,
            None,
            2_000,
        )
        .unwrap();
        let after_timeout = get_conn(&conn, KEY, "state").unwrap().unwrap();
        assert_eq!(
            after_timeout.state,
            ProbeState::Inconclusive,
            "最近观测如实"
        );
        assert_eq!(
            after_timeout.evidence_at,
            Some(1_000),
            "超时不得覆盖成功证据（设计 §3.2）"
        );
        assert_eq!(after_timeout.evidence_stage.as_deref(), Some("P3"));
        assert!(
            after_timeout.has_evidence(),
            "有成功证据的 build 不因一次超时被判死"
        );

        // 再一次明确失败：证据仍在
        upsert_fail_conn(
            &conn,
            KEY,
            "state",
            Some("P1"),
            "method_not_found",
            None,
            None,
            3_000,
        )
        .unwrap();
        let after_failed = get_conn(&conn, KEY, "state").unwrap().unwrap();
        assert_eq!(after_failed.state, ProbeState::Failed);
        assert_eq!(after_failed.evidence_at, Some(1_000), "失败也不覆盖证据");

        // 更新的成功：证据列推进到新成功
        upsert_ok_conn(&conn, KEY, "state", Some("P2"), None, Some(300), 4_000).unwrap();
        let reprobed = get_conn(&conn, KEY, "state").unwrap().unwrap();
        assert_eq!(reprobed.state, ProbeState::Probed);
        assert_eq!(reprobed.evidence_at, Some(4_000), "新成功推进证据列");
        assert_eq!(reprobed.evidence_stage.as_deref(), Some("P2"));
    }

    /// **老数据回归**（AGENTS.md 采集域纪律：状态字段语义变化当迁移处理）：
    /// 旧实现只有 `state` 一个字段——旧 `probed` 行 = 有过成功取证（migration 只补列
    /// 不改行）。读侧按旧值识别为已取证；下一次**失败观测**写入时按旧值把证据列
    /// 提升（`evidence_at ← last_seen_at`），旧成功从此不再被失败行抹掉。旧 `failed`
    /// 行没有成功语义，证据列保持 NULL。
    #[test]
    fn legacy_rows_read_as_evidence_and_lifted_on_next_failure() {
        let conn = mem();
        // 手工造「旧实现」行（无证据列值——旧库 migration 后的形态）
        conn.execute(
            "INSERT INTO codex_ipc_probe
                (probe_key, capability, state, probe_stage, attempts, first_seen_at, last_seen_at)
             VALUES (?1, 'state', 'probed', 'P3', 2, 500, 900)",
            params![KEY],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO codex_ipc_probe
                (probe_key, capability, state, probe_stage, error_code, attempts, first_seen_at, last_seen_at)
             VALUES (?1, 'send', 'failed', NULL, 'not_sent_before_write', 1, 600, 800)",
            params![KEY],
        )
        .unwrap();

        // 读侧：旧 probed 行 = 已取证（旧数据兼容臂）
        let legacy_probed = get_conn(&conn, KEY, "state").unwrap().unwrap();
        assert!(
            legacy_probed.has_evidence(),
            "旧 probed 行按旧值识别为已取证（migration 不改行）"
        );
        assert_eq!(legacy_probed.evidence_at, None, "证据列尚无本实现写过的值");
        let legacy_failed = get_conn(&conn, KEY, "send").unwrap().unwrap();
        assert!(!legacy_failed.has_evidence(), "旧 failed 行没有成功语义");

        // 写侧：失败观测落账时把旧成功提升进证据列（evidence_at ← 旧行 last_seen_at）
        upsert_inconclusive_conn(
            &conn,
            KEY,
            "state",
            Some("P0"),
            "initialize_timeout",
            None,
            None,
            2_000,
        )
        .unwrap();
        let lifted = get_conn(&conn, KEY, "state").unwrap().unwrap();
        assert_eq!(lifted.state, ProbeState::Inconclusive);
        assert_eq!(
            lifted.evidence_at,
            Some(900),
            "旧成功按 last_seen_at 提升，不被本条失败观测抹掉"
        );
        // 旧 failed 行再写失败：证据仍为 NULL（本来就没有）
        upsert_fail_conn(
            &conn,
            KEY,
            "send",
            None,
            "not_sent_before_write",
            None,
            None,
            2_100,
        )
        .unwrap();
        let still_none = get_conn(&conn, KEY, "send").unwrap().unwrap();
        assert_eq!(still_none.evidence_at, None, "旧失败行不得凭空长出证据");
    }

    /// schema **决策词**持久证据：写→读一致（词序保持）；upsert 覆盖；无行 → None。
    #[test]
    fn schema_words_round_trip_upsert_and_missing() {
        let conn = mem();
        assert!(get_schema_evidence_conn(&conn, KEY).unwrap().is_none());
        upsert_schema_evidence_conn(
            &conn,
            KEY,
            "fp-1",
            &["accept".into(), "acceptForSession".into(), "cancel".into()],
            &["accept".into(), "cancel".into()],
            1_000,
        )
        .unwrap();
        let row = get_schema_evidence_conn(&conn, KEY).unwrap().unwrap();
        assert_eq!(row.fingerprint, "fp-1");
        assert_eq!(
            row.command_decisions,
            vec!["accept", "acceptForSession", "cancel"],
            "决策词按 schema 顺序持久保存（指纹重建不了词，设计 §3.2）"
        );
        assert_eq!(row.file_decisions, vec!["accept", "cancel"]);
        assert_eq!(row.evidence_at, 1_000);

        // 升级后重取：覆盖（仍是同一行）
        upsert_schema_evidence_conn(
            &conn,
            KEY,
            "fp-2",
            &["accept".into(), "cancel".into(), "decline".into()],
            &["accept".into(), "decline".into()],
            2_000,
        )
        .unwrap();
        let updated = get_schema_evidence_conn(&conn, KEY).unwrap().unwrap();
        assert_eq!(updated.fingerprint, "fp-2", "后写覆盖前值");
        assert_eq!(updated.command_decisions.len(), 3);
        assert_eq!(updated.evidence_at, 2_000);
        // 换 key 互不影响
        upsert_schema_evidence_conn(&conn, "other|k|-|1", "fp-3", &[], &[], 3).unwrap();
        assert_eq!(
            get_schema_evidence_conn(&conn, KEY)
                .unwrap()
                .unwrap()
                .fingerprint,
            "fp-2"
        );
    }

    /// T02：契约证据按 `(probe_key, operation, rule_version, package_sha256)` 持久；
    /// 同键重复写幂等覆盖，不同规则版本 / 不同 CLI 指纹互不串用。
    #[test]
    fn f08p_t02_cli_or_rule_version_change_isolated() {
        let conn = mem();
        let row = |probe_key: &str, operation: &str, rule: u32, sha: &str| StoredContractEvidence {
            probe_key: probe_key.into(),
            operation: operation.into(),
            rule_version: rule,
            package_sha256: sha.into(),
            method: "thread-follower-submit-user-input".into(),
            base_version: 1,
            request_rule: "conversationId+requestId+response".into(),
            response_rule: "ok".into(),
            semantic_rule: "blocking user-input v1".into(),
            evidenced_at: 1_000,
            observed_at: 1_000,
            package_size: None,
            package_mtime_ms: None,
        };
        let key_a = "macos|26.1021.31345|0.164.0-alpha.11.1|1";
        let key_cli_drift = "macos|26.1021.31345|0.164.0-alpha.12|1";
        upsert_contract_evidence_conn(&conn, &row(key_a, "blocking_input", 1, "sha-a")).unwrap();

        // 同键覆盖：observed_at 前进，不产生第二行。
        let mut updated = row(key_a, "blocking_input", 1, "sha-a");
        updated.observed_at = 2_000;
        upsert_contract_evidence_conn(&conn, &updated).unwrap();
        let rows = get_contract_evidence_conn(&conn, key_a, 1).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].observed_at, 2_000);
        assert_eq!(rows[0].evidenced_at, 1_000, "首次取证时间不被覆盖");

        // CLI 漂移 → 不同 probe_key → 旧证据不复用。
        assert!(get_contract_evidence_conn(&conn, key_cli_drift, 1)
            .unwrap()
            .is_empty());

        // 规则版本收紧 → 旧行不复用。
        assert!(get_contract_evidence_conn(&conn, key_a, 2)
            .unwrap()
            .is_empty());

        // 不同包内容指纹并存（同规则同键），读回两行（调用方按当前包指纹选）。
        upsert_contract_evidence_conn(&conn, &row(key_a, "blocking_input", 1, "sha-b")).unwrap();
        assert_eq!(
            get_contract_evidence_conn(&conn, key_a, 1).unwrap().len(),
            2
        );
    }
}
