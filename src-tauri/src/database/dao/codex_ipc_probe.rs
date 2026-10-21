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
    /// 本 (key, capability) 的累计探测次数（含首次）。
    pub attempts: u32,
    pub first_seen_at: i64,
    pub last_seen_at: i64,
    /// 仅 `inconclusive` 有值：到点前不再探测。
    pub next_retry_at: Option<i64>,
    pub last_duration_ms: Option<u64>,
}

/// 读一行（无行 → `None`；`state` 词异常 → `Unknown`）。
pub fn get_conn(
    conn: &Connection,
    probe_key: &str,
    capability: &str,
) -> rusqlite::Result<Option<ProbeRow>> {
    let mut stmt = conn.prepare(
        "SELECT probe_key, capability, state, probe_stage, error_code, attempts,
                first_seen_at, last_seen_at, next_retry_at, last_duration_ms
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
        attempts: row.get::<_, i64>(5)?.max(0) as u32,
        first_seen_at: row.get(6)?,
        last_seen_at: row.get(7)?,
        next_retry_at: row.get(8)?,
        last_duration_ms: row.get::<_, Option<i64>>(9)?.map(|v| v.max(0) as u64),
    }))
}

/// 探测通过：`state = probed`，清退避（`next_retry_at = NULL`）。
pub fn upsert_ok_conn(
    conn: &Connection,
    probe_key: &str,
    capability: &str,
    probe_stage: Option<&str>,
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
        duration_ms,
        now_ms,
        None,
    )
}

/// 明确失败：`state = failed`，不设退避（不自动重试）。
pub fn upsert_fail_conn(
    conn: &Connection,
    probe_key: &str,
    capability: &str,
    probe_stage: Option<&str>,
    error_code: &str,
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
        duration_ms,
        now_ms,
        None,
    )
}

/// 不确定：`state = inconclusive` + 按**本次（含）累计次数**给退避窗口。
pub fn upsert_inconclusive_conn(
    conn: &Connection,
    probe_key: &str,
    capability: &str,
    probe_stage: Option<&str>,
    error_code: &str,
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
        duration_ms,
        now_ms,
        Some(next_retry_at),
    )
}

/// 单点 upsert（`ON CONFLICT(probe_key, capability)`）：
/// * `attempts` 累加（`+1`），`first_seen_at` **保留首次**（冲突时不覆盖）；
/// * `last_seen_at` / `state` / `stage` / `error_code` / 退避 / 耗时按本次覆盖。
#[allow(clippy::too_many_arguments)]
fn upsert_conn(
    conn: &Connection,
    probe_key: &str,
    capability: &str,
    state: ProbeState,
    probe_stage: Option<&str>,
    error_code: Option<&str>,
    duration_ms: Option<u64>,
    now_ms: i64,
    next_retry_at: Option<i64>,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO codex_ipc_probe
            (probe_key, capability, state, probe_stage, error_code, attempts,
             first_seen_at, last_seen_at, next_retry_at, last_duration_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?6, ?7, ?8)
         ON CONFLICT(probe_key, capability) DO UPDATE SET
            state            = excluded.state,
            probe_stage      = excluded.probe_stage,
            error_code       = excluded.error_code,
            attempts         = codex_ipc_probe.attempts + 1,
            last_seen_at     = excluded.last_seen_at,
            next_retry_at    = excluded.next_retry_at,
            last_duration_ms = excluded.last_duration_ms",
        params![
            probe_key,
            capability,
            state.as_str(),
            probe_stage,
            error_code,
            now_ms,
            next_retry_at,
            duration_ms.map(|d| d as i64)
        ],
    )?;
    Ok(())
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
        upsert_ok_conn(&conn, KEY, "state", Some("P3"), Some(420), 1_000).unwrap();
        let first = get_conn(&conn, KEY, "state").unwrap().unwrap();
        assert_eq!(first.state, ProbeState::Probed);
        assert_eq!(first.attempts, 1);
        assert_eq!(first.first_seen_at, 1_000);
        assert_eq!(first.last_seen_at, 1_000);
        assert_eq!(first.probe_stage.as_deref(), Some("P3"));
        assert_eq!(first.error_code, None);
        assert_eq!(first.next_retry_at, None);
        assert_eq!(first.last_duration_ms, Some(420));

        upsert_ok_conn(&conn, KEY, "state", Some("P3"), Some(380), 2_000).unwrap();
        let second = get_conn(&conn, KEY, "state").unwrap().unwrap();
        assert_eq!(count_rows(&conn), 1, "upsert 不是 append：只能一行");
        assert_eq!(second.attempts, 2, "attempts 累加");
        assert_eq!(second.first_seen_at, 1_000, "first_seen_at 保留首次");
        assert_eq!(second.last_seen_at, 2_000);
        assert_eq!(second.last_duration_ms, Some(380));
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
        upsert_ok_conn(&conn, KEY, "state", Some("P3"), None, 1).unwrap();
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
        upsert_ok_conn(&conn, KEY, "state", Some("P3"), None, 1).unwrap();
        upsert_ok_conn(&conn, KEY, "send", Some("P4"), None, 1).unwrap();
        upsert_ok_conn(&conn, "windows|other|-|1", "state", Some("P3"), None, 1).unwrap();
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
}
