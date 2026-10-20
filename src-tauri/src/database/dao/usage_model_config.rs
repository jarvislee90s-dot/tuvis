//! `usage_model_config` 的持久化 DAO（2026-10-20 计划 §2.1，Task 2）。
//!
//! 全部 `*_conn` 形态：调用方持连接；本层不取全局锁、不做业务校验。
//! 身份键 = 表达式唯一索引 `idx_usage_model_config_identity`；
//! `id` 由服务层按身份**确定性派生**（`channel::join_key`，长度前缀不碰撞），
//! 因此主键冲突与身份冲突同源，upsert 走 `ON CONFLICT(id)` 即可。
//!
//! 错误约定：所有 SQLite 读写/解码失败统一收敛为 `usage-db-failed`，不得吞错。

use rusqlite::{params, Connection, OptionalExtension};

use crate::services::usage::model::UsageSourceId;
use crate::services::usage::v2::channel::join_key;
use crate::services::usage::UsageError;

fn db_failed(context: &str, err: impl std::fmt::Display) -> UsageError {
    UsageError::new("usage-db-failed", format!("{context}: {err}"))
}

fn parse_source_id(raw: &str) -> Option<UsageSourceId> {
    UsageSourceId::ALL
        .into_iter()
        .find(|source| source.db_id() == raw)
}

/// 一行模型条目配置（身份 + 两段可选配置）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelConfigRecord {
    /// 非空 provider 原文；空 provider 条目为空串。
    pub provider_raw: String,
    /// 仅空 provider 条目非空。
    pub source_id: Option<UsageSourceId>,
    pub model_raw: String,
    pub display_name: Option<String>,
    /// 计价身份覆盖（成对；单侧 None 视为未覆盖）。
    pub pricing_provider: Option<String>,
    pub pricing_model: Option<String>,
}

/// 身份 → 确定性主键（长度前缀拼接，不碰撞；与唯一索引同键语义）。
fn record_id(record: &ModelConfigRecord) -> String {
    join_key(&[
        "model-config",
        &record.provider_raw,
        record.source_id.map(|s| s.db_id()).unwrap_or(""),
        &record.model_raw,
    ])
}

/// WHERE 谓词的身份条件（provider + COALESCE 折叠的 source + model），
/// 与表达式唯一索引 `idx_usage_model_config_identity` 同构。
const IDENTITY_WHERE: &str = "provider_raw = ?1
     AND COALESCE(source_id, '') = COALESCE(?2, '')
     AND model_raw = ?3";

const SELECT_COLUMNS: &str =
    "provider_raw, source_id, model_raw, display_name, pricing_provider, pricing_model";

/// 把一行数据库记录解码为配置。未知 sourceId 响亮失败（`usage-db-failed`）；
/// 「非空 provider + 非 NULL source_id」形状已被表级 CHECK 在写入侧排除、读取侧实际不可达，
/// 真出现时按空 provider 条目同形折叠为 `source_id = None`（防御性归一，不算脏数据报错）。
fn decode_record(
    provider_raw: String,
    source_raw: Option<String>,
    model_raw: String,
    display_name: Option<String>,
    pricing_provider: Option<String>,
    pricing_model: Option<String>,
) -> Result<ModelConfigRecord, UsageError> {
    let source_id = match source_raw.as_deref() {
        None => None,
        Some(raw) => {
            if provider_raw.is_empty() {
                let parsed = parse_source_id(raw).ok_or_else(|| {
                    db_failed(
                        "model config decode failed",
                        format!("row has unknown sourceId {raw:?}"),
                    )
                })?;
                Some(parsed)
            } else {
                None
            }
        }
    };
    Ok(ModelConfigRecord {
        provider_raw,
        source_id,
        model_raw,
        display_name,
        pricing_provider,
        pricing_model,
    })
}

type RawRow = (
    String,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// 读取全部配置行（按 model_raw 升序稳定输出；展示排序由服务层决定）。
pub fn load_all_conn(conn: &Connection) -> Result<Vec<ModelConfigRecord>, UsageError> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {SELECT_COLUMNS} FROM usage_model_config ORDER BY model_raw"
        ))
        .map_err(|e| db_failed("model config select failed", e))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })
        .map_err(|e| db_failed("model config select failed", e))?;
    let mut records = Vec::new();
    for row in rows {
        let (provider_raw, source_raw, model_raw, display_name, pricing_provider, pricing_model) =
            row.map_err(|e| db_failed("model config row read failed", e))?;
        records.push(decode_record(
            provider_raw,
            source_raw,
            model_raw,
            display_name,
            pricing_provider,
            pricing_model,
        )?);
    }
    Ok(records)
}

/// 按身份读取单条配置；不存在返回 `None`。
pub fn find_conn(
    conn: &Connection,
    provider_raw: &str,
    source_id: Option<UsageSourceId>,
    model_raw: &str,
) -> Result<Option<ModelConfigRecord>, UsageError> {
    let raw: Option<RawRow> = conn
        .query_row(
            &format!("SELECT {SELECT_COLUMNS} FROM usage_model_config WHERE {IDENTITY_WHERE}"),
            params![provider_raw, source_id.map(|s| s.db_id()), model_raw],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            },
        )
        .optional()
        .map_err(|e| db_failed("model config find failed", e))?;
    raw.map(
        |(provider_raw, source_raw, model_raw, display_name, pricing_provider, pricing_model)| {
            decode_record(
                provider_raw,
                source_raw,
                model_raw,
                display_name,
                pricing_provider,
                pricing_model,
            )
        },
    )
    .transpose()
}

/// 按 id（= 身份派生）插入或更新两段配置；身份列不更新（身份即键）。
pub fn upsert_conn(
    conn: &Connection,
    record: &ModelConfigRecord,
    now_ms: i64,
) -> Result<(), UsageError> {
    conn.execute(
        "INSERT INTO usage_model_config
         (id, provider_raw, source_id, model_raw, display_name, pricing_provider, pricing_model, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(id) DO UPDATE SET
             display_name = excluded.display_name,
             pricing_provider = excluded.pricing_provider,
             pricing_model = excluded.pricing_model,
             updated_at = excluded.updated_at",
        params![
            record_id(record),
            record.provider_raw,
            record.source_id.map(|s| s.db_id()),
            record.model_raw,
            record.display_name,
            record.pricing_provider,
            record.pricing_model,
            now_ms
        ],
    )
    .map_err(|e| db_failed("model config upsert failed", e))?;
    Ok(())
}

/// 按身份删除（不存在为幂等空操作）。
pub fn delete_conn(
    conn: &Connection,
    provider_raw: &str,
    source_id: Option<UsageSourceId>,
    model_raw: &str,
) -> Result<(), UsageError> {
    conn.execute(
        &format!("DELETE FROM usage_model_config WHERE {IDENTITY_WHERE}"),
        params![provider_raw, source_id.map(|s| s.db_id()), model_raw],
    )
    .map_err(|e| db_failed("model config delete failed", e))?;
    Ok(())
}
