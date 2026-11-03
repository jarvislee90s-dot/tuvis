//! TPS 存储域层（2026-10-24 计划 Task 2）：commit_batch / save_points / load_recent /
//! purge / tps_state 读写。SQL 全部在 `database::dao::tps`，本层只定事务边界、批次上限
//! 与状态键约定。
//!
//! **原子性契约**（spec §6「事实与 nextCursor 在同事务提交后才推进；读取失败不改变水位」）：
//! `commit_batch` 把「事实 UPSERT + 游标推进 + 来源状态」捏在**同一个事务**里，批内任何
//! 一条语句失败 → 整个事务回滚（rusqlite `Transaction` drop 即回滚）→ 水位不推进、事实
//! 不残留。集成测试用 `ended_at_ms = -1`（epoch 非负 CHECK）的行在批中段注入失败验证。
//!
//! **tick_id 派生**（spec §6 tps_point 主键 = tick_id+series_key）：`tick_id` 取
//! `TpsPoint.point_id`——Task 1 的 `policy::make_point` 已把它按
//! `{run_id}:{sampled_at}:{series_key.id()}` 从点自身字段**确定性**生成（同 run + 同采样
//! 时刻 + 同 series 必得同串），因此同一点重存必得同一 `(tick_id, series_key)` →
//! UPSERT 覆盖而非重复；不同 tick（新采样时刻/新 run）天然各成一行。series_key 列存
//! `SeriesKey::id()`（serde 规范化 JSON），与 tick_id 内嵌的串同源。
use rusqlite::Connection;

use crate::database::dao::tps as dao;
use crate::services::tps::model::{ReadBatch, SourceCursor, SourceState, TpsObservation, TpsPoint};
use crate::services::usage::error::UsageError;

/// 单事务行数上限（spec §6「单批最多200条」）。**选择硬拒绝而非静默切分**：spec 的
/// 200 条/批是「单事务工作量的界」，若切分提交，「事实+游标」的原子性会退化为多事务
/// （前批已提交、后批失败 = 水位推进了但事实没落全），恰恰违反上面那条原子性契约。
/// 超限说明读取层违背了「每宿主最多尾200行」的读取预算，属编程错误 → 报 internal 错
/// 让调用方显式失败，绝不静默截断。
pub const TPS_BATCH_MAX_ROWS: usize = 200;
/// 保留窗：24 小时（spec §2/§6「事实与点只保留最近24小时，按结束时间/采样时间清理」）
pub const TPS_RETENTION_MS: i64 = 24 * 60 * 60 * 1000;

// ==== tps_state 的键约定（游标**只**落 tps_state，绝不借旧 usage_cursor，spec §6）====
//
// **键前缀注册表**（全仓唯一登记处；Task 2 评审追认，新增前缀必须先在此登记——
// tps_state 是自由键值面，无登记的散键会让「状态到底存了什么」不可审计）：
//
// | 前缀               | 载体与所有者                                  | 引入 |
// |--------------------|---------------------------------------------|------|
// | `cursor:<source>`       | 读取水位（本文件 `cursor_state_key`）         | Task 2 |
// | `source_state:<source>` | 来源状态（本文件 `source_state_key`）         | Task 2 |
// | `install:record:<tool>` | 一次统计设置安装记录（`install.rs`，逐键原值/指纹，用户私有） | Task 5 |
// | `install:preview:<id>`  | 待应用预览（`install.rs`，TTL 即焚；每工具至多 1 份待应用） | Task 5 |
// | `install:pending:<tool>`| 待应用预览槽 = 当前 preview_id（`install.rs`，新预览即焚旧预览） | Task 5 |
// | `install:receiver:<tool>`| 本机接收器端口+凭证（`install.rs`）          | Task 5 |
// | `sched:…`               | 调度元数据（runId/租约等；Task 7 预留占位）   | Task 7 |

/// 游标键：`cursor:<source>` → SourceCursor JSON（读取水位）
pub fn cursor_state_key(source: &str) -> String {
    format!("cursor:{source}")
}

/// 来源状态键：`source_state:<source>` → SourceState JSON（含缺口说明 detail）
pub fn source_state_key(source: &str) -> String {
    format!("source_state:{source}")
}

// ==== 写入 ====

/// 原子提交一个来源读取批（spec §6 ReadBatch）：事实 + nextCursor + sourceState 同事务。
/// 空观察批也必须提交——sourceState「始终返回，即使无数据或缺计时」（spec §6），
/// 游标/状态随空批推进是正常路径（该来源本轮确实无新事实）。
pub fn commit_batch(conn: &mut Connection, batch: &ReadBatch) -> Result<(), UsageError> {
    if batch.observations.len() > TPS_BATCH_MAX_ROWS {
        return Err(UsageError::internal(format!(
            "tps: read batch over limit {} > {TPS_BATCH_MAX_ROWS} (read layer broke tail-200 per-host budget)",
            batch.observations.len()
        )));
    }
    let tx = conn
        .transaction()
        .map_err(|e| UsageError::new("usage-db-failed", format!("tps: begin tx: {e}")))?;
    dao::upsert_observations_conn(&tx, &batch.observations)?;
    save_state(
        &tx,
        &cursor_state_key(&batch.next_cursor.source),
        &to_json(&batch.next_cursor)?,
    )?;
    save_state(
        &tx,
        &source_state_key(&batch.source_state.source),
        &to_json(&batch.source_state)?,
    )?;
    tx.commit()
        .map_err(|e| UsageError::new("usage-db-failed", format!("tps: commit tx: {e}")))
}

/// 采样点落库（一个 tick 的全部 series 点一批）：同 (tick_id, series_key) 重存 UPSERT。
/// 与 `commit_batch` 一样受单事务 200 行上限（点批 = 每 series 一行，正常远小于上限；
/// 超限说明 series 爆炸，让调度器分批显式失败好过半事务）。
pub fn save_points(conn: &mut Connection, points: &[TpsPoint]) -> Result<(), UsageError> {
    if points.len() > TPS_BATCH_MAX_ROWS {
        return Err(UsageError::internal(format!(
            "tps: point batch over limit {} > {TPS_BATCH_MAX_ROWS}",
            points.len()
        )));
    }
    let tx = conn
        .transaction()
        .map_err(|e| UsageError::new("usage-db-failed", format!("tps: begin tx: {e}")))?;
    dao::upsert_points_conn(&tx, points)?;
    tx.commit()
        .map_err(|e| UsageError::new("usage-db-failed", format!("tps: commit tx: {e}")))
}

// ==== 读取 ====

/// 读回 `ended_at > since_ms` 的事实（spec 窗口口径按**完成**时刻判定，严格大于）
pub fn load_recent(conn: &Connection, since_ms: i64) -> Result<Vec<TpsObservation>, UsageError> {
    dao::load_observations_since_conn(conn, since_ms)
}

/// 读回 `sampled_at > since_ms` 的采样点（gap 点含在内；曲线查询面 Task 7/8 消费）
pub fn load_points_since(conn: &Connection, since_ms: i64) -> Result<Vec<TpsPoint>, UsageError> {
    dao::load_points_since_conn(conn, since_ms)
}

// ==== 保留清理 ====

/// 24 小时保留清理：删 `ended_at < now-24h` 的事实与 `sampled_at < now-24h` 的点，
/// 返回删除总行数。**只**删 tps 两张表——旧 usage_* 的保留设置一律不碰（spec §2）。
/// 恰在 24h 边界的行保留（「最近 24 小时」含边界）。
pub fn purge(conn: &Connection, now_ms: i64) -> Result<usize, UsageError> {
    dao::purge_before_conn(conn, now_ms - TPS_RETENTION_MS)
}

// ==== tps_state 通用通道（安装记录 / 调度元数据 / 游标 / 来源状态共用）====

/// 保存一个状态键的 JSON 值（UPSERT）。键约定见 [`cursor_state_key`] / [`source_state_key`]；
/// 安装记录（spec §5 逐键原值/指纹）与调度元数据走各自的自由键。
pub fn save_state(
    conn: &Connection,
    key: &str,
    value: &serde_json::Value,
) -> Result<(), UsageError> {
    let text = serde_json::to_string(value).map_err(|e| {
        UsageError::internal(format!("tps: state serialization failed ({key}): {e}"))
    })?;
    dao::save_state_conn(conn, key, &text, crate::services::usage::now_ms())
}

/// 读回一个状态键的 JSON 值；缺键 = None（不猜、不造默认值）
pub fn load_state(conn: &Connection, key: &str) -> Result<Option<serde_json::Value>, UsageError> {
    match dao::load_state_conn(conn, key)? {
        None => Ok(None),
        Some(text) => serde_json::from_str(&text).map(Some).map_err(|e| {
            UsageError::internal(format!("tps: state JSON parse failed ({key}): {e}"))
        }),
    }
}

/// 读回一个来源的水位游标（tps_state 的 `cursor:<source>` 键；未提交过批 = None）
pub fn load_cursor(conn: &Connection, source: &str) -> Result<Option<SourceCursor>, UsageError> {
    match load_state(conn, &cursor_state_key(source))? {
        None => Ok(None),
        Some(v) => serde_json::from_value(v).map(Some).map_err(|e| {
            UsageError::internal(format!("tps: cursor JSON parse failed ({source}): {e}"))
        }),
    }
}

/// 读回一个来源的来源状态（tps_state 的 `source_state:<source>` 键）
pub fn load_source_state(
    conn: &Connection,
    source: &str,
) -> Result<Option<SourceState>, UsageError> {
    match load_state(conn, &source_state_key(source))? {
        None => Ok(None),
        Some(v) => serde_json::from_value(v).map(Some).map_err(|e| {
            UsageError::internal(format!(
                "tps: source state JSON parse failed ({source}): {e}"
            ))
        }),
    }
}

/// serde_json::Value 的统一包装（错误收敛到 UsageError）
fn to_json<T: serde::Serialize>(value: &T) -> Result<serde_json::Value, UsageError> {
    serde_json::to_value(value)
        .map_err(|e| UsageError::internal(format!("tps: state serialization failed: {e}")))
}
