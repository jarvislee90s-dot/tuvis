//! 独立正文估算事实的 SQL 操作。
use crate::services::{tps::estimate_store::EstimateFact, usage::error::UsageError};
use rusqlite::{params, Connection};

pub(crate) fn insert(conn: &Connection, fact: &EstimateFact) -> Result<bool, UsageError> {
    let tokens = i64::try_from(fact.reference_tokens)
        .map_err(|_| UsageError::internal("reference token overflow".into()))?;
    let duration = i64::try_from(fact.duration_ns)
        .map_err(|_| UsageError::internal("duration overflow".into()))?;
    let rows=conn.execute("INSERT INTO tps_body_estimate (host_key,thread_key,turn_key,item_key,reference_profile_id,model,channel,model_source,reference_tokens,duration_ns,ended_at_ms,run_id,source_build,short_window,small_sample) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15) ON CONFLICT(host_key,thread_key,turn_key,item_key,reference_profile_id) DO NOTHING",params![fact.host_key,fact.thread_key,fact.turn_key,fact.item_key,fact.reference_profile_id,fact.model,fact.channel,fact.model_source,tokens,duration,fact.ended_at_ms,fact.run_id,fact.source_build,fact.short_window,fact.small_sample]).map_err(|e|UsageError::new("usage-db-failed",format!("estimate insert failed: {e}")))?;
    Ok(rows == 1)
}

/// 沿用速度事实的结束时刻保留边界。
pub fn purge_before(conn: &Connection, cutoff_ms: i64) -> Result<usize, UsageError> {
    conn.execute(
        "DELETE FROM tps_body_estimate WHERE ended_at_ms < ?1",
        [cutoff_ms],
    )
    .map_err(|e| UsageError::new("usage-db-failed", format!("estimate purge failed: {e}")))
}

use crate::database::dao::speed::SeriesSumsRow;
use crate::services::tps::{
    model::{OutputScope, SeriesKey, TimingMethod},
    v2::sums::SpeedSums,
};

fn estimate_error(error: rusqlite::Error) -> UsageError {
    UsageError::new("usage-db-failed", format!("estimate aggregate: {error}"))
}

fn read_estimate_sums(row: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<SpeedSums> {
    Ok(SpeedSums {
        estimate_output_tokens: row.get(offset)?,
        estimate_duration_ns: row.get(offset + 1)?,
        estimate_sample_count: row.get(offset + 2)?,
        session_count: row.get(offset + 3)?,
        short_window_count: row.get(offset + 4)?,
        small_sample_count: row.get(offset + 5)?,
        candidate_count: row.get(offset + 2)?,
        output_scope: Some(OutputScope::VisibleBody),
        ..SpeedSums::default()
    })
}

/// 与原生事实完全分离，只读估算表，并按参考配置分组。
pub fn series_sums_conn(
    conn: &Connection,
    from: i64,
    to: i64,
    tool: Option<&str>,
    search: Option<&str>,
) -> Result<Vec<SeriesSumsRow>, UsageError> {
    if tool.is_some_and(|t| t != "codex") {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare("SELECT COALESCE(NULLIF(model,''),'unknown'), channel, reference_profile_id, SUM(reference_tokens), SUM(duration_ns), COUNT(*), COUNT(DISTINCT thread_key), SUM(short_window), SUM(small_sample) FROM tps_body_estimate WHERE ended_at_ms > ?1 AND ended_at_ms <= ?2 AND (?3 IS NULL OR instr(lower('codex ' || COALESCE(model,'unknown') || ' ' || COALESCE(channel,'')),lower(?3)) > 0) GROUP BY 1,2,3 ORDER BY 1,2,3").map_err(estimate_error)?;
    let rows = stmt
        .query_map(params![from, to, search], |row| {
            Ok(SeriesSumsRow {
                key: SeriesKey {
                    tool: "codex".into(),
                    model: row.get(0)?,
                    channel: row.get(1)?,
                    method: TimingMethod::IpcBodyReference,
                    reference_profile_id: Some(row.get(2)?),
                },
                sums: read_estimate_sums(row, 3)?,
            })
        })
        .map_err(estimate_error)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(estimate_error)
}

/// 用最后一次正文增长的墙钟时刻归桶，不将完成通知或工具等待算进窗口。
pub fn bucket_sums_conn(
    conn: &Connection,
    from: i64,
    to: i64,
    anchor: i64,
    width: i64,
    key: &SeriesKey,
) -> Result<Vec<(i64, SpeedSums)>, UsageError> {
    if key.tool != "codex" || key.method != TimingMethod::IpcBodyReference {
        return Ok(Vec::new());
    }
    let Some(profile) = &key.reference_profile_id else {
        return Ok(Vec::new());
    };
    let mut stmt = conn.prepare("SELECT (ended_at_ms - ?3 - 1) / ?4 AS bucket, SUM(reference_tokens), SUM(duration_ns), COUNT(*), COUNT(DISTINCT thread_key), SUM(short_window), SUM(small_sample) FROM tps_body_estimate WHERE ended_at_ms > ?1 AND ended_at_ms <= ?2 AND COALESCE(NULLIF(model,''),'unknown') = ?5 AND channel IS ?6 AND reference_profile_id = ?7 GROUP BY bucket ORDER BY bucket").map_err(estimate_error)?;
    let rows = stmt
        .query_map(
            params![from, to, anchor, width, key.model, key.channel, profile],
            |row| Ok((row.get(0)?, read_estimate_sums(row, 1)?)),
        )
        .map_err(estimate_error)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(estimate_error)
}
