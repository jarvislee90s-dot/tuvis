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
