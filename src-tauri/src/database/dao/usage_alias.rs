//! Provider 别名的持久化 DAO（用量看板 V2 Task 2）。
//!
//! 全部 `*_conn` 形态：调用方持连接；本层不取全局锁、不做业务校验。
//! 表结构由 `schema.rs` 的 `usage_provider_alias` 承载：
//! * `kind = provider`：`provider_raw` 为唯一键（跨工具生效）；
//! * `kind = toolModel`：`(source_id, model_id)` 为唯一键（`model_id` 可为空串）。
//!
//! 错误约定：所有 SQLite 读写/解码失败统一收敛为 `usage-db-failed`，不得吞错；
//! 唯一键冲突的**固定 detail** 由服务层在事务内先查后报（本层只提供查询与写入原语）。

use rusqlite::{params, Connection, OptionalExtension};

use crate::services::usage::model::UsageSourceId;
use crate::services::usage::v2::model::{ProviderAliasKind, ProviderAliasRule};
use crate::services::usage::UsageError;

fn db_failed(context: &str, err: impl std::fmt::Display) -> UsageError {
    UsageError::new("usage-db-failed", format!("{context}: {err}"))
}

fn parse_source_id(raw: &str) -> Option<UsageSourceId> {
    UsageSourceId::ALL
        .into_iter()
        .find(|source| source.db_id() == raw)
}

/// 把一行数据库记录解码为规则；形状异常（绕过 CHECK 的脏数据）必须响亮失败。
fn decode_alias(
    id: String,
    kind_raw: String,
    provider_raw: Option<String>,
    source_raw: Option<String>,
    model_id: Option<String>,
    display_name: String,
) -> Result<ProviderAliasRule, UsageError> {
    match kind_raw.as_str() {
        "provider" => {
            if provider_raw.as_deref().is_none_or(|raw| raw.is_empty())
                || source_raw.is_some()
                || model_id.is_some()
            {
                return Err(db_failed(
                    "provider alias decode failed",
                    format!("provider row {id:?} has an invalid shape"),
                ));
            }
            Ok(ProviderAliasRule {
                id,
                kind: ProviderAliasKind::Provider,
                provider: provider_raw,
                source_id: None,
                model_id: None,
                display_name,
            })
        }
        "toolModel" => {
            if provider_raw.is_some() || source_raw.is_none() || model_id.is_none() {
                return Err(db_failed(
                    "provider alias decode failed",
                    format!("toolModel row {id:?} has an invalid shape"),
                ));
            }
            let raw_source = source_raw.unwrap_or_default();
            let source_id = parse_source_id(&raw_source).ok_or_else(|| {
                db_failed(
                    "provider alias decode failed",
                    format!("toolModel row {id:?} has unknown sourceId {raw_source:?}"),
                )
            })?;
            Ok(ProviderAliasRule {
                id,
                kind: ProviderAliasKind::ToolModel,
                provider: None,
                source_id: Some(source_id),
                model_id,
                display_name,
            })
        }
        other => Err(db_failed(
            "provider alias decode failed",
            format!("row {id:?} has unknown kind {other:?}"),
        )),
    }
}

/// 读取全部别名规则（按创建时间、id 稳定排序）。
pub fn load_aliases_conn(conn: &Connection) -> Result<Vec<ProviderAliasRule>, UsageError> {
    let mut stmt = conn
        .prepare(
            "SELECT id, kind, provider_raw, source_id, model_id, display_name
             FROM usage_provider_alias
             ORDER BY created_at, id",
        )
        .map_err(|e| db_failed("provider alias select failed", e))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
            ))
        })
        .map_err(|e| db_failed("provider alias select failed", e))?;

    let mut aliases = Vec::new();
    for row in rows {
        let (id, kind, provider, source, model, display_name) =
            row.map_err(|e| db_failed("provider alias row read failed", e))?;
        aliases.push(decode_alias(
            id,
            kind,
            provider,
            source,
            model,
            display_name,
        )?);
    }
    Ok(aliases)
}

/// 查询目标键是否已被**另一条**规则占用；命中时返回固定 detail 中的键文本。
///
/// 调用方必须先在事务内调用本函数，再调用 [`write_alias_conn`]——两者在同一连接/事务上
/// 保证「查重 + 写入」原子性。`rule` 必须已通过服务层校验（字段形状合法）。
pub fn conflicting_alias_key_conn(
    conn: &Connection,
    rule: &ProviderAliasRule,
) -> Result<Option<String>, UsageError> {
    match rule.kind {
        ProviderAliasKind::Provider => {
            let provider = rule.provider.as_deref().ok_or_else(|| {
                db_failed(
                    "provider alias conflict check failed",
                    "provider rule is missing provider",
                )
            })?;
            let found: Option<String> = conn
                .query_row(
                    "SELECT id FROM usage_provider_alias
                     WHERE kind = 'provider' AND provider_raw = ?1 AND id <> ?2
                     LIMIT 1",
                    params![provider, rule.id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|e| db_failed("provider alias conflict query failed", e))?;
            Ok(found.map(|_| format!("provider:{provider}")))
        }
        ProviderAliasKind::ToolModel => {
            let source = rule.source_id.ok_or_else(|| {
                db_failed(
                    "provider alias conflict check failed",
                    "toolModel rule is missing sourceId",
                )
            })?;
            let model = rule.model_id.as_deref().ok_or_else(|| {
                db_failed(
                    "provider alias conflict check failed",
                    "toolModel rule is missing modelId",
                )
            })?;
            let found: Option<String> = conn
                .query_row(
                    "SELECT id FROM usage_provider_alias
                     WHERE kind = 'toolModel' AND source_id = ?1 AND model_id = ?2
                       AND id <> ?3
                     LIMIT 1",
                    params![source.db_id(), model, rule.id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|e| db_failed("provider alias conflict query failed", e))?;
            Ok(found.map(|_| format!("toolModel:{}:{model}", source.db_id())))
        }
    }
}

/// 按 id 插入或更新（`created_at` 保留、`updated_at` 刷新）。
pub fn write_alias_conn(
    conn: &Connection,
    rule: &ProviderAliasRule,
    now_ms: i64,
) -> Result<(), UsageError> {
    let (kind, provider_raw, source_id, model_id) = match rule.kind {
        ProviderAliasKind::Provider => ("provider", rule.provider.as_deref(), None, None),
        ProviderAliasKind::ToolModel => (
            "toolModel",
            None,
            rule.source_id.map(|source| source.db_id()),
            rule.model_id.as_deref(),
        ),
    };
    conn.execute(
        "INSERT INTO usage_provider_alias
         (id, kind, provider_raw, source_id, model_id, display_name, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
         ON CONFLICT(id) DO UPDATE SET
             kind = excluded.kind,
             provider_raw = excluded.provider_raw,
             source_id = excluded.source_id,
             model_id = excluded.model_id,
             display_name = excluded.display_name,
             updated_at = excluded.updated_at",
        params![
            rule.id,
            kind,
            provider_raw,
            source_id,
            model_id,
            rule.display_name,
            now_ms
        ],
    )
    .map_err(|e| db_failed("provider alias upsert failed", e))?;
    Ok(())
}

/// 按 id 删除；不存在视为幂等空操作。
pub fn delete_alias_conn(conn: &Connection, id: &str) -> Result<(), UsageError> {
    conn.execute(
        "DELETE FROM usage_provider_alias WHERE id = ?1",
        params![id],
    )
    .map_err(|e| db_failed("provider alias delete failed", e))?;
    Ok(())
}
