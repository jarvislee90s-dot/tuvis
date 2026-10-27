//! TPS 三表 DAO（2026-10-24 计划 Task 2 / spec §6）：事实/采样点/状态的 SQL 层。
//! 全部 `*_conn` 形态（调用方持连接/事务传入，本层不自取全局锁、不开事务）——与
//! dao/usage.rs 同款：事务边界由 services/tps/store.rs 捏（「事实 + nextCursor +
//! sourceState 同一事务」的原子性在那边落实），本层只做单条语句的映射与上行错误。
//!
//! 纪律：
//! * **枚举库串与 serde 逐字一致**（本文件 `mod tests` 锁）；读侧遇未知串 = 库被手改/损坏
//!   → 报错不猜（绝不静默兜底成某个枚举值——那是「把失败伪装成合法值」）。
//! * 可空列一律 `Option` 直读直写（`row.get::<_, Option<i64>>`），「不可得」必须是
//!   NULL 而不是 0（spec §3「失败缺计时为缺失，不填 0」）。
use rusqlite::{params, types::Type, Connection};

use crate::services::tps::model::{
    Outcome, OutputScope, PointStatus, PointTrigger, Purpose, SeriesKey, TimingMethod,
    TpsObservation, TpsPoint, TpsQuality,
};
use crate::services::usage::error::UsageError;

/// 枚举 ↔ 库串映射。库串必须与 serde 序列化串逐字一致（kebab/lowercase/camelCase，
/// 见各枚举的 `#[serde(rename_all)]`），模块测试锁齐；`from_db` 遇未知串返回错误。
trait DbStr: Sized {
    fn to_db(&self) -> &'static str;
    fn from_db(s: &str) -> Result<Self, UsageError>;
}

impl DbStr for TimingMethod {
    fn to_db(&self) -> &'static str {
        self.as_str()
    }
    fn from_db(s: &str) -> Result<Self, UsageError> {
        match s {
            "stream-observed" => Ok(TimingMethod::StreamObserved),
            "client-native" => Ok(TimingMethod::ClientNative),
            "native-event-approx" => Ok(TimingMethod::NativeEventApprox),
            "log-boundary-approx" => Ok(TimingMethod::LogBoundaryApprox),
            other => Err(UsageError::internal(format!(
                "tps: unknown timing-method db string {other:?}"
            ))),
        }
    }
}

macro_rules! db_str_enum {
    ($imp:ident, $($var:ident => $lit:literal),+ $(,)?) => {
        impl DbStr for $imp {
            fn to_db(&self) -> &'static str {
                match self {
                    $($imp::$var => $lit,)+
                }
            }
            fn from_db(s: &str) -> Result<Self, UsageError> {
                match s {
                    $($lit => Ok($imp::$var),)+
                    other => Err(UsageError::internal(format!(
                        "tps: unknown {} db string {other:?}", stringify!($imp)
                    ))),
                }
            }
        }
    };
}

db_str_enum!(Outcome, Success => "success", Failed => "failed", Cancelled => "cancelled");
db_str_enum!(Purpose, MainTask => "mainTask", AssistTitle => "assistTitle", AutoSummary => "autoSummary");
db_str_enum!(
    OutputScope,
    ExclusiveOutput => "exclusiveOutput",
    IncludesReasoning => "includesReasoning",
    ProviderReported => "providerReported",
    Unknown => "unknown"
);
db_str_enum!(TpsQuality, Normal => "normal", SmallSample => "smallSample");
db_str_enum!(PointStatus, Value => "value", Gap => "gap");
db_str_enum!(PointTrigger, Startup => "startup", Scheduled => "scheduled", Manual => "manual");

/// 读侧枚举列解码：`from_db` 失败 → rusqlite 转换失败（列号随行报，不静默丢行）
fn enum_col<T: DbStr>(idx: usize, s: String) -> rusqlite::Result<T> {
    T::from_db(&s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            idx,
            Type::Text,
            format!("{} (col {idx})", e.detail).into(),
        )
    })
}

// ==== 写入 ====

/// 事实 UPSERT（spec §6「事实UPSERT」）：同主键后写整行覆盖（重读取更新鲜的值），
/// **不是**旧账本那种增量累加——TPS 事实是「一次调用的观察快照」，重放同键 = 同一次调用
/// 的重读，累加会翻倍（spec §7「同调用重复读取不翻倍」）。返回成功写入的行数。
pub fn upsert_observations_conn(
    conn: &Connection,
    rows: &[TpsObservation],
) -> Result<usize, UsageError> {
    const SQL: &str = "INSERT INTO tps_observation
             (source, host_instance, stable_call_key, started_at_ms, first_token_at_ms,
              ended_at_ms, native_duration_ms, generation_ms, response_elapsed_ms,
              output_tokens, output_scope, tool_ms, model, channel, session_key, turn_key,
              purpose, outcome, method, failure_reason)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)
             ON CONFLICT(source, host_instance, stable_call_key) DO UPDATE SET
              started_at_ms       = excluded.started_at_ms,
              first_token_at_ms   = excluded.first_token_at_ms,
              ended_at_ms         = excluded.ended_at_ms,
              native_duration_ms  = excluded.native_duration_ms,
              generation_ms       = excluded.generation_ms,
              response_elapsed_ms = excluded.response_elapsed_ms,
              output_tokens       = excluded.output_tokens,
              output_scope        = excluded.output_scope,
              tool_ms             = excluded.tool_ms,
              model               = excluded.model,
              channel             = excluded.channel,
              session_key         = excluded.session_key,
              turn_key            = excluded.turn_key,
              purpose             = excluded.purpose,
              outcome             = excluded.outcome,
              method              = excluded.method,
              failure_reason      = excluded.failure_reason";
    let mut written = 0usize;
    for r in rows {
        conn.execute(
            SQL,
            params![
                r.source,
                r.host_instance,
                r.stable_call_key,
                r.started_at_ms,
                r.first_token_at_ms,
                r.ended_at_ms,
                r.native_duration_ms,
                r.generation_ms,
                r.response_elapsed_ms,
                r.output_tokens,
                r.output_scope.to_db(),
                r.tool_ms,
                r.model,
                r.channel,
                r.session_key,
                r.turn_key,
                r.purpose.to_db(),
                r.outcome.to_db(),
                r.method.to_db(),
                r.failure_reason,
            ],
        )
        .map_err(|e| {
            UsageError::new(
                "usage-db-failed",
                format!(
                    "tps: observation upsert failed ({}/{}/{}): {e}",
                    r.source, r.host_instance, r.stable_call_key
                ),
            )
        })?;
        written += 1;
    }
    Ok(written)
}

/// 采样点 UPSERT：主键 (tick_id, series_key)。`series_key` 列存 [`SeriesKey::id`]
/// （serde 规范化 JSON，跨进程/跨重启逐字稳定）；`sampled_keys` 存 JSON 字符串数组。
/// 同点重存（调度器重放同一 tick / manual 重刷）→ 整行覆盖，不重复（spec §6「点引用
/// 稳定键不重复计数」的存储面）。返回成功写入的行数。
pub fn upsert_points_conn(conn: &Connection, points: &[TpsPoint]) -> Result<usize, UsageError> {
    const SQL: &str = "INSERT INTO tps_point
             (tick_id, series_key, sampled_at, window_ms, run_id, trigger_kind, status,
              method, output_scope, quality, n, sum_output_tokens, sum_generation_ms,
              ttft_sum_ms, ttft_count, model_ms, tool_ms, avg_first_token_ms, tps,
              candidate_count, timed_model_count, timed_tool_count, failed_count,
              cancelled_count, missing_timing_count, latest_ended_at, sampled_keys,
              cadence_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,
                     ?20,?21,?22,?23,?24,?25,?26,?27,?28)
             ON CONFLICT(tick_id, series_key) DO UPDATE SET
              sampled_at           = excluded.sampled_at,
              window_ms            = excluded.window_ms,
              run_id               = excluded.run_id,
              trigger_kind         = excluded.trigger_kind,
              status               = excluded.status,
              method               = excluded.method,
              output_scope         = excluded.output_scope,
              quality              = excluded.quality,
              n                    = excluded.n,
              sum_output_tokens    = excluded.sum_output_tokens,
              sum_generation_ms    = excluded.sum_generation_ms,
              ttft_sum_ms          = excluded.ttft_sum_ms,
              ttft_count           = excluded.ttft_count,
              model_ms             = excluded.model_ms,
              tool_ms              = excluded.tool_ms,
              avg_first_token_ms   = excluded.avg_first_token_ms,
              tps                  = excluded.tps,
              candidate_count      = excluded.candidate_count,
              timed_model_count    = excluded.timed_model_count,
              timed_tool_count     = excluded.timed_tool_count,
              failed_count         = excluded.failed_count,
              cancelled_count      = excluded.cancelled_count,
              missing_timing_count = excluded.missing_timing_count,
              latest_ended_at      = excluded.latest_ended_at,
              sampled_keys         = excluded.sampled_keys,
              cadence_ms           = excluded.cadence_ms";
    let mut written = 0usize;
    for p in points {
        let series = p.series_key.id();
        let keys = serde_json::to_string(&p.sampled_keys).map_err(|e| {
            UsageError::internal(format!("tps: sampled_keys serialization failed: {e}"))
        })?;
        conn.execute(
            SQL,
            params![
                p.point_id,
                series,
                p.sampled_at,
                p.window_ms,
                p.run_id,
                p.trigger.to_db(),
                p.status.to_db(),
                p.method.to_db(),
                p.output_scope.to_db(),
                p.quality.to_db(),
                p.sample_count,
                p.sum_output_tokens,
                p.sum_generation_ms,
                p.ttft_sum_ms,
                p.ttft_sample_count,
                p.model_ms,
                p.tool_ms,
                p.avg_first_token_ms,
                p.tps,
                p.candidate_count,
                p.timed_model_count,
                p.timed_tool_count,
                p.failed_count,
                p.cancelled_count,
                p.missing_timing_count,
                p.latest_ended_at,
                keys,
                p.cadence_ms,
            ],
        )
        .map_err(|e| {
            UsageError::new(
                "usage-db-failed",
                format!("tps: point upsert failed (tick={}): {e}", p.point_id),
            )
        })?;
        written += 1;
    }
    Ok(written)
}

/// 状态 UPSERT：key → JSON 文本（value 已由 store 层序列化）。安装记录/调度元数据与
/// 游标/来源状态共用本通道（spec §6：tps_state = key+JSON）。
pub fn save_state_conn(
    conn: &Connection,
    key: &str,
    value_json: &str,
    now_ms: i64,
) -> Result<(), UsageError> {
    conn.execute(
        "INSERT INTO tps_state (key, value_json, updated_at) VALUES (?1,?2,?3)
         ON CONFLICT(key) DO UPDATE SET
          value_json = excluded.value_json,
          updated_at = excluded.updated_at",
        params![key, value_json, now_ms],
    )
    .map_err(|e| {
        UsageError::new(
            "usage-db-failed",
            format!("tps: state write failed ({key}): {e}"),
        )
    })?;
    Ok(())
}

// ==== 读取 ====

/// 读回 `ended_at > since_ms` 的事实（按结束时刻升序，稳定键破平局）。TPS 消费面
/// （窗口聚合/快照）只在近 24 小时内取数，本查询恒有 ended 索引可走。
pub fn load_observations_since_conn(
    conn: &Connection,
    since_ms: i64,
) -> Result<Vec<TpsObservation>, UsageError> {
    let mut stmt = conn
        .prepare(
            "SELECT source, host_instance, stable_call_key, started_at_ms, first_token_at_ms,
                    ended_at_ms, native_duration_ms, generation_ms, response_elapsed_ms,
                    output_tokens, output_scope, tool_ms, model, channel, session_key,
                    turn_key, purpose, outcome, method, failure_reason
             FROM tps_observation WHERE ended_at_ms > ?1
             ORDER BY ended_at_ms, source, host_instance, stable_call_key",
        )
        .map_err(|e| {
            UsageError::new(
                "usage-db-failed",
                format!("tps: observation query failed: {e}"),
            )
        })?;
    let rows = stmt
        .query_map([since_ms], |row| {
            Ok(TpsObservation {
                source: row.get(0)?,
                host_instance: row.get(1)?,
                stable_call_key: row.get(2)?,
                started_at_ms: row.get(3)?,
                first_token_at_ms: row.get(4)?,
                ended_at_ms: row.get(5)?,
                native_duration_ms: row.get(6)?,
                generation_ms: row.get(7)?,
                response_elapsed_ms: row.get(8)?,
                output_tokens: row.get(9)?,
                output_scope: enum_col(10, row.get(10)?)?,
                tool_ms: row.get(11)?,
                model: row.get(12)?,
                channel: row.get(13)?,
                session_key: row.get(14)?,
                turn_key: row.get(15)?,
                purpose: enum_col(16, row.get(16)?)?,
                outcome: enum_col(17, row.get(17)?)?,
                method: enum_col(18, row.get(18)?)?,
                failure_reason: row.get(19)?,
            })
        })
        .map_err(|e| {
            UsageError::new(
                "usage-db-failed",
                format!("tps: observation query failed: {e}"),
            )
        })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(|e| {
        UsageError::new(
            "usage-db-failed",
            format!("tps: observation row read failed: {e}"),
        )
    })
}

/// 读回 `sampled_at > since_ms` 的采样点（按采样时刻升序，tick 破平局）。
/// `age_ms` 是派生值不落库：按「当时钟 = sampled_at」重算（与 policy 层建点时同口径），
/// 保证 policy 产物整点往返逐字段一致。
pub fn load_points_since_conn(
    conn: &Connection,
    since_ms: i64,
) -> Result<Vec<TpsPoint>, UsageError> {
    let mut stmt = conn
        .prepare(
            "SELECT tick_id, series_key, sampled_at, window_ms, run_id, trigger_kind, status,
                    method, output_scope, quality, n, sum_output_tokens, sum_generation_ms,
                    ttft_sum_ms, ttft_count, model_ms, tool_ms, avg_first_token_ms, tps,
                    candidate_count, timed_model_count, timed_tool_count, failed_count,
                    cancelled_count, missing_timing_count, latest_ended_at, sampled_keys,
                    cadence_ms
             FROM tps_point WHERE sampled_at > ?1
             ORDER BY sampled_at, tick_id",
        )
        .map_err(|e| UsageError::new("usage-db-failed", format!("tps: point query failed: {e}")))?;
    let rows = stmt
        .query_map([since_ms], |row| {
            let sampled_at: i64 = row.get(2)?;
            let latest_ended_at: Option<i64> = row.get(25)?;
            let keys_text: String = row.get(26)?;
            let series_text: String = row.get(1)?;
            // series_key / sampled_keys 的 JSON 解析失败 = 库被手改或写坏 → 报错不猜
            let series_key: SeriesKey = serde_json::from_str(&series_text).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    1,
                    Type::Text,
                    format!("tps: series_key JSON parse failed: {e}").into(),
                )
            })?;
            let sampled_keys: Vec<String> = serde_json::from_str(&keys_text).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    26,
                    Type::Text,
                    format!("tps: sampled_keys JSON parse failed: {e}").into(),
                )
            })?;
            Ok(TpsPoint {
                model_ms: row.get(15)?,
                tool_ms: row.get(16)?,
                avg_first_token_ms: row.get(17)?,
                tps: row.get(18)?,
                sample_count: row.get(10)?,
                ttft_sample_count: row.get(14)?,
                candidate_count: row.get(19)?,
                timed_model_count: row.get(20)?,
                timed_tool_count: row.get(21)?,
                failed_count: row.get(22)?,
                cancelled_count: row.get(23)?,
                missing_timing_count: row.get(24)?,
                latest_ended_at,
                age_ms: latest_ended_at.map(|e| (sampled_at - e).max(0)),
                method: enum_col(7, row.get(7)?)?,
                output_scope: enum_col(8, row.get(8)?)?,
                quality: enum_col(9, row.get(9)?)?,
                sum_output_tokens: row.get(11)?,
                sum_generation_ms: row.get(12)?,
                ttft_sum_ms: row.get(13)?,
                point_id: row.get(0)?,
                series_key,
                sampled_at,
                window_ms: row.get(3)?,
                run_id: row.get(4)?,
                trigger: enum_col(5, row.get(5)?)?,
                status: enum_col(6, row.get(6)?)?,
                cadence_ms: row.get(27)?,
                sampled_keys,
            })
        })
        .map_err(|e| UsageError::new("usage-db-failed", format!("tps: point query failed: {e}")))?;
    rows.collect::<Result<Vec<_>, _>>().map_err(|e| {
        UsageError::new(
            "usage-db-failed",
            format!("tps: point row read failed: {e}"),
        )
    })
}

/// 读回一个状态键的 JSON 文本；缺键 = None（不猜）
pub fn load_state_conn(conn: &Connection, key: &str) -> Result<Option<String>, UsageError> {
    match conn.query_row(
        "SELECT value_json FROM tps_state WHERE key = ?1",
        [key],
        |row| row.get::<_, String>(0),
    ) {
        Ok(v) => Ok(Some(v)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(UsageError::new(
            "usage-db-failed",
            format!("tps: state read failed ({key}): {e}"),
        )),
    }
}

// ==== 保留清理 ====

/// 24 小时保留清理：按事实 `ended_at_ms` / 点 `sampled_at` 删**严格早于** cutoff 的行
/// （恰在边界的行保留——「最近 24 小时」含边界）。**只**删 tps 两张表，绝不触碰旧
/// usage_*（spec §2「不变更旧用量保留设置」）。返回删除总行数。
/// 两表各自单语句原子；第二条失败时返回 Err（第一条已删的行由下一轮清理周期自然收尾）。
pub fn purge_before_conn(conn: &Connection, cutoff_ms: i64) -> Result<usize, UsageError> {
    let obs = conn
        .execute(
            "DELETE FROM tps_observation WHERE ended_at_ms < ?1",
            [cutoff_ms],
        )
        .map_err(|e| {
            UsageError::new(
                "usage-db-failed",
                format!("tps: observation purge failed: {e}"),
            )
        })?;
    let pts = conn
        .execute("DELETE FROM tps_point WHERE sampled_at < ?1", [cutoff_ms])
        .map_err(|e| UsageError::new("usage-db-failed", format!("tps: point purge failed: {e}")))?;
    Ok(obs + pts)
}

// ==== 以下为本文件单元测试（列映射/枚举库串/状态通道的贴身锁；批量行为在集成测试）====
#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::tps::model::{Outcome, Purpose};

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        conn
    }

    /// 库串必须与 serde 序列化串逐字一致（policy.rs 的 serde 形态锁在这里的镜像半边：
    /// 两边各自锁住自己的形态，改任何一侧的另一侧立刻红）
    #[test]
    fn db_enum_strings_match_serde_forms() {
        macro_rules! check {
            ($ty:ty, [$($v:expr),+ $(,)?]) => {
                $(
                    let v: $ty = $v;
                    let serde_form = serde_json::to_string(&v).unwrap();
                    assert_eq!(
                        v.to_db(),
                        serde_form.trim_matches('"'),
                        "{:?} 的库串必须与 serde 形态逐字一致",
                        v
                    );
                    assert_eq!(
                        <$ty as DbStr>::from_db(v.to_db()).unwrap(),
                        v,
                        "库串必须能原样读回"
                    );
                )+
            };
        }
        check!(
            TimingMethod,
            [
                TimingMethod::StreamObserved,
                TimingMethod::ClientNative,
                TimingMethod::NativeEventApprox,
                TimingMethod::LogBoundaryApprox
            ]
        );
        check!(
            Outcome,
            [Outcome::Success, Outcome::Failed, Outcome::Cancelled]
        );
        check!(
            Purpose,
            [
                Purpose::MainTask,
                Purpose::AssistTitle,
                Purpose::AutoSummary
            ]
        );
        check!(
            OutputScope,
            [
                OutputScope::ExclusiveOutput,
                OutputScope::IncludesReasoning,
                OutputScope::ProviderReported,
                OutputScope::Unknown
            ]
        );
        check!(TpsQuality, [TpsQuality::Normal, TpsQuality::SmallSample]);
        check!(PointStatus, [PointStatus::Value, PointStatus::Gap]);
        check!(
            PointTrigger,
            [
                PointTrigger::Startup,
                PointTrigger::Scheduled,
                PointTrigger::Manual
            ]
        );
    }

    /// 未知库串 = 库被手改/损坏 → 报错不猜（不静默兜底成某个枚举值）
    #[test]
    fn unknown_db_enum_string_is_an_error_not_a_guess() {
        assert!(TimingMethod::from_db("weird").is_err());
        assert!(PointStatus::from_db("").is_err());
        assert!(Outcome::from_db("success ").is_err(), "尾随空格也不放过");
    }

    /// 状态通道：同键覆写、缺键 None、updated_at 随写更新
    #[test]
    fn state_upsert_overwrites_and_missing_key_is_none() {
        let conn = mem();
        save_state_conn(&conn, "k", "v1", 1).unwrap();
        save_state_conn(&conn, "k", "v2", 2).unwrap();
        assert_eq!(load_state_conn(&conn, "k").unwrap().as_deref(), Some("v2"));
        let (v, at): (String, i64) = conn
            .query_row(
                "SELECT value_json, updated_at FROM tps_state WHERE key='k'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((v.as_str(), at), ("v2", 2));
        assert_eq!(load_state_conn(&conn, "missing").unwrap(), None);
    }

    /// 保留清理：两表各删各的，返回合并行数；恰在边界（cutoff 本身）的行保留
    #[test]
    fn purge_counts_both_tables_and_keeps_boundary_rows() {
        let conn = mem();
        // 两条事实：ended=100（恰在 cutoff → 保留）与 ended=99（严格早于 → 删）
        let mk_obs = |key: &str, ended: i64| crate::services::tps::model::TpsObservation {
            source: "kimi".into(),
            host_instance: "h".into(),
            stable_call_key: key.into(),
            started_at_ms: 0,
            first_token_at_ms: None,
            ended_at_ms: ended,
            native_duration_ms: None,
            generation_ms: None,
            response_elapsed_ms: None,
            output_tokens: None,
            output_scope: OutputScope::Unknown,
            tool_ms: None,
            model: "m".into(),
            channel: None,
            session_key: None,
            turn_key: None,
            purpose: Purpose::MainTask,
            outcome: Outcome::Failed,
            method: TimingMethod::ClientNative,
            failure_reason: None,
        };
        let o1 = mk_obs("c1", 100);
        let o2 = mk_obs("c2", 99);
        upsert_observations_conn(&conn, &[o1, o2]).unwrap();
        let p = crate::services::tps::model::TpsPoint {
            model_ms: None,
            tool_ms: None,
            avg_first_token_ms: None,
            tps: None,
            sample_count: 0,
            ttft_sample_count: 0,
            candidate_count: 0,
            timed_model_count: 0,
            timed_tool_count: 0,
            failed_count: 0,
            cancelled_count: 0,
            missing_timing_count: 0,
            latest_ended_at: None,
            age_ms: None,
            method: TimingMethod::ClientNative,
            output_scope: OutputScope::Unknown,
            quality: TpsQuality::Normal,
            sum_output_tokens: 0,
            sum_generation_ms: 0,
            ttft_sum_ms: None,
            point_id: "r:100:[...]".into(),
            series_key: SeriesKey {
                tool: "kimi".into(),
                model: "m".into(),
                channel: None,
                method: TimingMethod::ClientNative,
            },
            sampled_at: 100,
            window_ms: crate::services::tps::policy::TPS_WINDOW_MS,
            run_id: "r".into(),
            trigger: PointTrigger::Scheduled,
            status: PointStatus::Gap,
            cadence_ms: Some(600_000),
            sampled_keys: vec![],
        };
        upsert_points_conn(&conn, std::slice::from_ref(&p)).unwrap();
        // cutoff=100：ended=99 的事实删除；ended=100 的事实与 sampled_at=100 的点**恰在
        // 边界 → 保留**（「最近24小时」含边界）
        assert_eq!(purge_before_conn(&conn, 100).unwrap(), 1);
        assert_eq!(load_observations_since_conn(&conn, 0).unwrap().len(), 1);
        assert_eq!(load_points_since_conn(&conn, 0).unwrap().len(), 1);
        assert_eq!(purge_before_conn(&conn, 101).unwrap(), 2, "两表各删一条");
        assert_eq!(load_observations_since_conn(&conn, 0).unwrap().len(), 0);
        assert_eq!(load_points_since_conn(&conn, 0).unwrap().len(), 0);
    }
}
