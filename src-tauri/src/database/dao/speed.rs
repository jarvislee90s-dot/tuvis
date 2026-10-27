//! V2 速度聚合只读 DAO（2026-10-26 桌面速度 V2 计划 Task 6）。
//!
//! 职责：把 `tps_observation` 的一窗事实折算成充分统计（[`SpeedSums`]），**只读**——
//! 事务边界、采集、游标一律不沾（查询面不触发源扫描或模型请求，Spec §2/§7）；
//! DAO 只返回预聚合统计，不向前端传原始事实。
//!
//! ## 有效性 SQL 与 Task 1 Rust 谓词的一致性
//!
//! 全部查询共用**同一条** CTE（[`validity_cte_sql`]）：谓词用 CASE 列展开
//! （`stream_ok` / `response_ms` / `resp_ok` / `model_ms_val` / `ttft_ok`），与
//! `services/tps/model.rs` 的 `is_tps_valid` / `response_sample_ms` / `is_response_valid`
//! / `ttft_ms` 逐分支镜像——**绝不**用三条互相分歧的 WHERE 各算各的。矩阵契约测试
//! （tests/tps_speed_dashboard_test.rs `sql_validity_matches_rust_predicates_matrix`）
//! 用同一 CTE 文本逐记录断言 SQL 分类 == Rust 谓词。
//!
//! 关键保真点（与 Rust 逐字对齐，改任一侧必须同步另一侧）：
//! * 显式响应窗分支：`response_elapsed_ms IS NOT NULL` 即进入（无论好坏），内层
//!   `v > 0 且 (generation 缺 或 v ≥ generation)`——生成窗缺失放行（Codex 日志近似），
//!   与 Rust `is_none_or` 同构；不一致（非正/短于生成窗）⇒ NULL，**绝不**落到旧兼容分支；
//! * 旧记录兼容分支：要求旧流式有效性本就成立（重试/失败/取消不救回）且
//!   `native > 0 且 generation 在场且 native ≥ generation`——与 Rust
//!   `is_some_and(|g| d >= g)` 同构（generation 缺失 = 拒绝，不是放行）；
//! * 主任务过滤（`purpose = 'mainTask'`）在 WHERE，全部候选**保留在行集**；三个口径
//!   与 TTFT/用时池都在 CASE 里各算各的——无速度但有可靠等待/用时的记录照常贡献
//!   那些池（brief Step 3 逐字）；
//! * TTFT：成功 且 首点在场 且 `first_token − started ≥ 0`（与 Rust `ttft_ms` 的
//!   `>= 0` 一致——恰等开始判真）；
//! * 可靠模型用时：`native > 0` 优先取 native，否则取响应样本时长表达式——两路互斥
//!   **绝不双重计数**；双窗皆缺失 ⇒ NULL（missing_timing 计数）。
//!
//! ## 桶索引
//!
//! 桶几何（对齐/裁剪）在 query.rs 的 Rust 侧算好；SQL 只做等价的整除分桶
//! `(ended_at_ms − anchor) / width`——anchor 是**未裁剪**的对齐锚点（query.rs 与
//! `bucket_spans_with_offset` 同源），使桶序号与 span 下标一一对应。

use rusqlite::{params, types::Type, Connection};

use crate::services::tps::model::{OutputScope, SeriesKey, TimingMethod};
use crate::services::tps::v2::sums::SpeedSums;
use crate::services::usage::error::UsageError;

// ==== 有效性表达式（单源常量，多处拼接不得手抄）====

/// 响应样本时长 ms（`TpsObservation::response_sample_ms` 的 SQL 镜像，见模块头）。
/// 表达式**不带尾部分号**、可直接作为标量嵌入；两分支次序不可换（显式值不一致时
/// 必须保持 NULL，不能被兼容路径修复）。
const RESPONSE_SAMPLE_EXPR: &str = "CASE \
        WHEN response_elapsed_ms IS NOT NULL THEN \
            CASE WHEN response_elapsed_ms > 0 \
                  AND (generation_ms IS NULL OR response_elapsed_ms >= generation_ms) \
                 THEN response_elapsed_ms END \
        WHEN (outcome = 'success' AND COALESCE(output_tokens, 0) > 0 AND COALESCE(generation_ms, 0) > 0) THEN \
            CASE WHEN native_duration_ms IS NOT NULL AND native_duration_ms > 0 \
                  AND generation_ms IS NOT NULL AND native_duration_ms >= generation_ms \
                 THEN native_duration_ms END \
    END";

/// 有效性 CTE（全部 V2 查询的唯一有效性口径）。`?1` = from（完成时刻严格大于），
/// `?2` = to（完成时刻包含）——半开完成窗 `(from, to]`；`extra_where` 由调用方追加
/// （工具/搜索/series 过滤，占位符从 ?3 起编号）。矩阵契约测试直接消费本函数文本。
pub fn validity_cte_sql(extra_where: &str) -> String {
    format!(
        "SELECT stable_call_key, source, model, channel, method, session_key, output_scope, \
                outcome, started_at_ms, first_token_at_ms, ended_at_ms, generation_ms, \
                output_tokens, tool_ms, \
            (outcome = 'success' AND COALESCE(output_tokens, 0) > 0 AND COALESCE(generation_ms, 0) > 0) AS stream_ok, \
            {RESPONSE_SAMPLE_EXPR} AS response_ms, \
            (outcome = 'success' AND COALESCE(output_tokens, 0) > 0 AND ({RESPONSE_SAMPLE_EXPR}) IS NOT NULL) AS resp_ok, \
            (CASE WHEN COALESCE(native_duration_ms, 0) > 0 THEN native_duration_ms \
                  ELSE ({RESPONSE_SAMPLE_EXPR}) END) AS model_ms_val, \
            (outcome = 'success' AND first_token_at_ms IS NOT NULL \
                AND first_token_at_ms - started_at_ms >= 0) AS ttft_ok \
         FROM tps_observation \
         WHERE purpose = '{purpose}' AND ended_at_ms > ?1 AND ended_at_ms <= ?2{extra}",
        purpose = "mainTask", // Purpose::MainTask 的库串（dao/tps.rs DbStr；模块测试锁）
        extra = extra_where,
    )
}

/// series 过滤占位符编号（工具从 ?3、搜索从下一个起）；搜索 = tool 与 model 的
/// **字面**子串（`%`/`_`/`\` 先转义再 LIKE，ESCAPE '\\'）。
fn series_filters_sql(tool_filter: bool, search_filter: bool) -> String {
    let mut n = 2;
    let mut sql = String::new();
    if tool_filter {
        n += 1;
        sql.push_str(&format!(" AND source = ?{n}"));
    }
    if search_filter {
        n += 1;
        sql.push_str(&format!(
            " AND (source LIKE ?{n} ESCAPE '\\' OR model LIKE ?{n} ESCAPE '\\')"
        ));
    }
    sql
}

/// LIKE 通配符转义（字面子串语义：`%`/`_`/`\` 前加 `\`；与 SQL 的 ESCAPE '\\' 配对）
pub(crate) fn escape_like(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if matches!(ch, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// series 过滤常量（桶查询专用，占位符 ?5–?8 固定：锚点/宽度占 ?3/?4）
const BUCKET_SERIES_FILTER: &str =
    " AND source = ?5 AND model = ?6 AND channel IS ?7 AND method = ?8";

/// series 聚合 SQL（pub 供 EXPLAIN QUERY PLAN 契约测试；生产路径 [`series_sums_conn`]）
pub fn series_query_sql(tool_filter: bool, search_filter: bool) -> String {
    format!(
        "WITH cand AS ({}) \
         SELECT source, model, channel, method, {} \
         FROM cand GROUP BY source, model, channel, method \
         ORDER BY source, model, channel, method",
        validity_cte_sql(&series_filters_sql(tool_filter, search_filter)),
        AGG_COLUMNS
    )
}

/// 桶聚合 SQL（pub 供 EXPLAIN 契约测试；`?3` = 未裁剪对齐锚点，`?4` = 桶宽；
/// series 过滤 ?5–?8 固定）。生产路径 [`bucket_sums_conn`]。
pub fn bucket_query_sql() -> String {
    format!(
        "WITH cand AS ({}) \
         SELECT ((ended_at_ms - ?3) / ?4) AS bucket_idx, {} \
         FROM cand GROUP BY bucket_idx ORDER BY bucket_idx",
        validity_cte_sql(BUCKET_SERIES_FILTER),
        AGG_COLUMNS
    )
}

/// 聚合列（充分统计；列序与 [`read_sums`] 的读取序严格一致——改任一侧必须同步）
const AGG_COLUMNS: &str = "COUNT(*) AS candidate_count, \
    COALESCE(SUM(outcome = 'failed'), 0) AS failed_count, \
    COALESCE(SUM(outcome = 'cancelled'), 0) AS cancelled_count, \
    COALESCE(SUM(model_ms_val IS NULL), 0) AS missing_timing_count, \
    COUNT(DISTINCT session_key) AS session_count, \
    COALESCE(SUM(CASE WHEN stream_ok THEN COALESCE(output_tokens, 0) ELSE 0 END), 0) AS stream_out, \
    COALESCE(SUM(CASE WHEN stream_ok THEN COALESCE(generation_ms, 0) ELSE 0 END), 0) AS stream_ms, \
    COALESCE(SUM(stream_ok), 0) AS stream_n, \
    MAX(CASE WHEN stream_ok THEN ended_at_ms END) AS latest_stream_ended_at, \
    COALESCE(SUM(CASE WHEN resp_ok THEN COALESCE(output_tokens, 0) ELSE 0 END), 0) AS resp_out, \
    COALESCE(SUM(CASE WHEN resp_ok THEN response_ms ELSE 0 END), 0) AS resp_ms, \
    COALESCE(SUM(resp_ok), 0) AS resp_n, \
    MAX(CASE WHEN resp_ok THEN ended_at_ms END) AS latest_response_ended_at, \
    COALESCE(SUM(CASE WHEN ttft_ok THEN first_token_at_ms - started_at_ms ELSE 0 END), 0) AS ttft_sum, \
    COALESCE(SUM(ttft_ok), 0) AS ttft_n, \
    COALESCE(SUM(model_ms_val), 0) AS model_ms_sum, \
    COALESCE(SUM(model_ms_val IS NOT NULL), 0) AS model_ms_n, \
    COALESCE(SUM(CASE WHEN COALESCE(tool_ms, 0) > 0 THEN tool_ms ELSE 0 END), 0) AS tool_ms_sum, \
    COALESCE(SUM(COALESCE(tool_ms, 0) > 0), 0) AS tool_ms_n, \
    COUNT(DISTINCT output_scope) AS scope_kinds, \
    MIN(output_scope) AS scope_one";

// ==== 枚举库串解码（库串与 serde 逐字一致；未知串 = 库损坏 → 报错不猜，dao/tps.rs 同款）====

fn method_from_db(s: &str) -> Result<TimingMethod, UsageError> {
    match s {
        "stream-observed" => Ok(TimingMethod::StreamObserved),
        "client-native" => Ok(TimingMethod::ClientNative),
        "native-event-approx" => Ok(TimingMethod::NativeEventApprox),
        "log-boundary-approx" => Ok(TimingMethod::LogBoundaryApprox),
        other => Err(UsageError::internal(format!(
            "speed: unknown timing-method db string {other:?}"
        ))),
    }
}

fn scope_from_db(s: &str) -> Result<OutputScope, UsageError> {
    match s {
        "exclusiveOutput" => Ok(OutputScope::ExclusiveOutput),
        "includesReasoning" => Ok(OutputScope::IncludesReasoning),
        "providerReported" => Ok(OutputScope::ProviderReported),
        "unknown" => Ok(OutputScope::Unknown),
        other => Err(UsageError::internal(format!(
            "speed: unknown output-scope db string {other:?}"
        ))),
    }
}

/// 读侧枚举列解码：失败 → rusqlite 转换失败（列号随行报，dao/tps.rs::enum_col 同款）
fn enum_col<T>(
    idx: usize,
    s: String,
    parse: fn(&str) -> Result<T, UsageError>,
) -> rusqlite::Result<T> {
    parse(&s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            idx,
            Type::Text,
            format!("{} (col {idx})", e.detail).into(),
        )
    })
}

// ==== 充分统计读取 ====

/// series 行（键 + 充分统计；query.rs 用 `derive_metrics` 纯派生成契约指标面）
#[derive(Debug, Clone, PartialEq)]
pub struct SeriesSumsRow {
    pub key: SeriesKey,
    pub sums: SpeedSums,
}

/// 从聚合行读充分统计（`first` = bucket_idx 之外的首列偏移：桶查询 +1）
fn read_sums(row: &rusqlite::Row<'_>, first: usize) -> rusqlite::Result<SpeedSums> {
    let scope_kinds: i64 = row.get(first + 19)?;
    let scope_one: Option<String> = row.get(first + 20)?;
    let output_scope = match scope_kinds {
        0 => None, // 组内无行（防御；GROUP BY 组恒有 ≥1 行）
        1 => Some(enum_col(
            first + 20,
            scope_one.unwrap_or_default(),
            scope_from_db,
        )?),
        _ => None, // 混合 scope：派生面折算 Unknown 且 scope_mixed=true
    };
    Ok(SpeedSums {
        candidate_count: row.get(first)?,
        failed_count: row.get(first + 1)?,
        cancelled_count: row.get(first + 2)?,
        missing_timing_count: row.get(first + 3)?,
        session_count: row.get(first + 4)?,
        stream_output_tokens: row.get(first + 5)?,
        stream_duration_ms: row.get(first + 6)?,
        stream_sample_count: row.get(first + 7)?,
        latest_stream_ended_at: row.get(first + 8)?,
        response_output_tokens: row.get(first + 9)?,
        response_duration_ms: row.get(first + 10)?,
        response_sample_count: row.get(first + 11)?,
        latest_response_ended_at: row.get(first + 12)?,
        ttft_sum_ms: row.get(first + 13)?,
        ttft_sample_count: row.get(first + 14)?,
        model_ms_sum: row.get(first + 15)?,
        model_ms_samples: row.get(first + 16)?,
        tool_ms_sum: row.get(first + 17)?,
        tool_ms_samples: row.get(first + 18)?,
        output_scope,
        scope_mixed: scope_kinds > 1,
    })
}

/// 聚合失败的统一错误信封：`usage-db-failed` + `speed: {stage}: {底层错误}`；
/// `stage` 是 ASCII 机器段位 token（`series-aggregate` / `bucket-aggregate` /
/// `source-stats` 及其 `-row` 行读取变体），便于日志与 detail 检索（错误码棘轮：
/// 运行时串不写中文，人读说明在本注释）。
fn db_failed(stage: &str, e: rusqlite::Error) -> UsageError {
    UsageError::new("usage-db-failed", format!("speed: {stage}: {e}"))
}

/// 区间 series 聚合（只读）：完成时刻 ∈ (from, to] 的主任务候选按
/// (source, model, channel, method) 分组的充分统计。`tool_id` 过滤 `SeriesKey.tool`；
/// `search` 为 tool/model 的字面子串（内部转义 LIKE 通配符；空串/None 不过滤）。
pub fn series_sums_conn(
    conn: &Connection,
    from_ms: i64,
    to_ms: i64,
    tool_id: Option<&str>,
    search: Option<&str>,
) -> Result<Vec<SeriesSumsRow>, UsageError> {
    let tool_filter = tool_id.is_some();
    let search_filter = search.is_some();
    let sql = series_query_sql(tool_filter, search_filter);
    let search_pattern = search.map(|s| format!("%{}%", escape_like(s)));
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| db_failed("series-aggregate", e))?;
    // 拥有型绑定盒（规避借用生命周期），再取 trait 对象切片传参
    let mut bind: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(from_ms), Box::new(to_ms)];
    if let Some(t) = tool_id {
        bind.push(Box::new(t.to_string()));
    }
    if let Some(p) = &search_pattern {
        bind.push(Box::new(p.clone()));
    }
    let refs: Vec<&dyn rusqlite::ToSql> = bind.iter().map(|b| b.as_ref()).collect();
    let rows = stmt
        .query_map(refs.as_slice(), |row| {
            Ok(SeriesSumsRow {
                key: SeriesKey {
                    tool: row.get(0)?,
                    model: row.get(1)?,
                    channel: row.get(2)?,
                    method: enum_col(3, row.get(3)?, method_from_db)?,
                },
                sums: read_sums(row, 4)?,
            })
        })
        .map_err(|e| db_failed("series-aggregate", e))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| db_failed("series-aggregate-row", e))
}

/// 选中 series 的桶聚合（只读）：完成时刻 ∈ (from, to] 且 series 匹配的候选按
/// `(ended − anchor) / width` 分桶的充分统计。`anchor` 必须是**未裁剪**的墙钟对齐
/// 锚点（query.rs 与 bucket_spans 同源计算），返回 (桶序号, 充分统计)；序号与
/// `bucket_spans` 下标一一对应，范围外的序号不会出现。
pub fn bucket_sums_conn(
    conn: &Connection,
    from_ms: i64,
    to_ms: i64,
    anchor_ms: i64,
    bucket_ms: i64,
    key: &SeriesKey,
) -> Result<Vec<(i64, SpeedSums)>, UsageError> {
    let method_db = key.method.as_str();
    let sql = bucket_query_sql();
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| db_failed("bucket-aggregate", e))?;
    let rows = stmt
        .query_map(
            params![
                from_ms,
                to_ms,
                anchor_ms,
                bucket_ms,
                key.tool,
                key.model,
                key.channel,
                method_db
            ],
            |row| {
                let idx: i64 = row.get(0)?;
                Ok((idx, read_sums(row, 1)?))
            },
        )
        .map_err(|e| db_failed("bucket-aggregate", e))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| db_failed("bucket-aggregate-row", e))
}

/// 每源事实统计（全部 purpose——`last_record_at` 是「该源最近报过记录」的中性事实，
/// 不限主任务）：source → (区间内事实总数, 最近完成时刻)。供来源状态分类
/// （AwaitRestart 需零事实判定）与 last_record_at 使用。
pub fn source_record_stats_conn(
    conn: &Connection,
    from_ms: i64,
    to_ms: i64,
) -> Result<std::collections::BTreeMap<String, (i64, Option<i64>)>, UsageError> {
    let mut stmt = conn
        .prepare(
            "SELECT source, COUNT(*), MAX(ended_at_ms) FROM tps_observation
             WHERE ended_at_ms > ?1 AND ended_at_ms <= ?2 GROUP BY source",
        )
        .map_err(|e| db_failed("source-stats", e))?;
    let rows = stmt
        .query_map(params![from_ms, to_ms], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<i64>>(2)?,
            ))
        })
        .map_err(|e| db_failed("source-stats", e))?;
    let mut out = std::collections::BTreeMap::new();
    for r in rows {
        let (source, count, last) = r.map_err(|e| db_failed("source-stats-row", e))?;
        out.insert(source, (count, last));
    }
    Ok(out)
}

// ==== 以下为本文件单元测试（枚举库串/转义的贴身锁；口径矩阵在集成测试）====
#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::tps::model::{Outcome, Purpose};

    /// 枚举库串必须与 serde 序列化串逐字一致（dao/tps.rs DbStr 的镜像半边）
    #[test]
    fn db_enum_strings_match_serde_forms() {
        for m in [
            TimingMethod::StreamObserved,
            TimingMethod::ClientNative,
            TimingMethod::NativeEventApprox,
            TimingMethod::LogBoundaryApprox,
        ] {
            assert_eq!(
                method_from_db(serde_json::to_string(&m).unwrap().trim_matches('"')).unwrap(),
                m
            );
        }
        for s in [
            OutputScope::ExclusiveOutput,
            OutputScope::IncludesReasoning,
            OutputScope::ProviderReported,
            OutputScope::Unknown,
        ] {
            assert_eq!(
                scope_from_db(serde_json::to_string(&s).unwrap().trim_matches('"')).unwrap(),
                s
            );
        }
        assert!(method_from_db("weird").is_err());
        assert!(scope_from_db("success").is_err());
    }

    /// 主任务库串与 Purpose serde 形态一致（validity_cte_sql 硬编码半边的锁）
    #[test]
    fn main_task_db_string_matches_serde() {
        assert_eq!(
            serde_json::to_string(&Purpose::MainTask)
                .unwrap()
                .trim_matches('"'),
            "mainTask"
        );
        assert!(validity_cte_sql("").contains("purpose = 'mainTask'"));
    }

    /// LIKE 转义：`%`/`_`/`\` 逐一前置 `\`，其余原样
    #[test]
    fn escape_like_quotes_wildcards() {
        assert_eq!(escape_like("abc"), "abc");
        assert_eq!(escape_like("a%b_c\\d"), "a\\%b\\_c\\\\d");
        assert_eq!(escape_like(""), "");
    }

    /// 过滤占位符编号连续（?3 起），搜索子句引用同一占位符两次
    #[test]
    fn filter_placeholders_are_sequential() {
        assert_eq!(series_filters_sql(false, false), "");
        assert_eq!(series_filters_sql(true, false), " AND source = ?3");
        assert_eq!(
            series_filters_sql(false, true),
            " AND (source LIKE ?3 ESCAPE '\\' OR model LIKE ?3 ESCAPE '\\')"
        );
        assert_eq!(
            series_filters_sql(true, true),
            " AND source = ?3 AND (source LIKE ?4 ESCAPE '\\' OR model LIKE ?4 ESCAPE '\\')"
        );
    }

    /// Outcome 库串与聚合 SQL 字面量一致（failed/cancelled 计数列的锁）
    #[test]
    fn outcome_db_strings_match_serde() {
        assert_eq!(
            serde_json::to_string(&Outcome::Failed)
                .unwrap()
                .trim_matches('"'),
            "failed"
        );
        assert_eq!(
            serde_json::to_string(&Outcome::Cancelled)
                .unwrap()
                .trim_matches('"'),
            "cancelled"
        );
        assert!(AGG_COLUMNS.contains("outcome = 'failed'"));
        assert!(AGG_COLUMNS.contains("outcome = 'cancelled'"));
    }
}
