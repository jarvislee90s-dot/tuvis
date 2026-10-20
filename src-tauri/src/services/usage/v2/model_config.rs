//! 模型条目配置与观察条目查询（2026-10-20 计划 §3.1，Task 2）。
//!
//! * 数据源 = `usage_daily` 的全历史聚合（日聚合永久保留 ⇒ 天然覆盖
//!   「含历史出现、当前范围无用量」的条目；与查询范围无关）；
//! * `display_provider` 经 `usage_provider_alias` 解析（复用 `channel.rs` 的别名映射，
//!   **不触 billing attribution**——配置弹窗没有归因上下文）；
//! * 配置按**观测身份**存储在 `usage_model_config`（非空 provider 用
//!   `(provider_raw, model_raw)` 跨工具生效；空 provider 用 `(source_id, model_raw)`），
//!   写入前校验目标身份确在观测集（防幽灵行）；
//! * 写入按 identity 整行读改写：一条命令只改一半字段时**保留另一半**
//!   （§1 决策表「计价身份覆盖」行）；两段都清空时整行删除（回未配置态）；
//! * usage 总开关关闭 ⇒ 空数组（与 `identities_with_conn` 一致），写入随之被
//!   幽灵校验拒绝（观测集为空）。
//!
//! 错误与校验（计划 §2.5，不新增错误码）：校验失败 `usage-settings-invalid`，
//! SQLite 失败 `usage-db-failed`；固定 detail 前缀
//! `model config identity is empty` / `model config display name is empty after trim` /
//! `model config pricing reference is incomplete` / `model config entry not observed`。

use std::collections::{HashMap, HashSet};

use rusqlite::Connection;

use crate::database::connection::DB;
use crate::database::dao::usage_model_config as dao;
use crate::services::usage::model::UsageSourceId;
use crate::services::usage::settings;
use crate::services::usage::v2::channel::{self, ChannelResolver};
use crate::services::usage::v2::model::{
    BillingConfig, ModelDisplayNameRequest, ObservedModelEntry, PricingIdentityRequest,
};
use crate::services::usage::v2::pricing::normalize_model_id;
use crate::services::usage::UsageError;

fn invalid(detail: impl Into<String>) -> UsageError {
    UsageError::new("usage-settings-invalid", detail.into())
}

fn db_failed(context: &str, err: impl std::fmt::Display) -> UsageError {
    UsageError::new("usage-db-failed", format!("{context}: {err}"))
}

fn poisoned() -> UsageError {
    UsageError::new("usage-db-failed", "db poisoned lock")
}

fn parse_source_id(raw: &str) -> Option<UsageSourceId> {
    UsageSourceId::ALL
        .into_iter()
        .find(|source| source.db_id() == raw)
}

/// 观测身份（与 `usage_model_config` 的表达式唯一索引同构）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ModelIdentity {
    /// 非空 provider 原文；空 provider 条目为空串。
    provider_raw: String,
    /// 仅空 provider 条目非空。
    source_id: Option<UsageSourceId>,
    model_raw: String,
}

impl ModelIdentity {
    /// 从请求身份段构造（不含 model）。非空 provider 与 sourceId 同现 / 全空 /
    /// **显式空串 provider**（空 provider 条目在请求侧只用 `null` 表达）⇒ 非法。
    fn from_request_parts(
        provider_raw: Option<&str>,
        source_id: Option<UsageSourceId>,
    ) -> Result<Self, UsageError> {
        match (provider_raw.map(str::trim), source_id) {
            (Some(provider), None) if !provider.is_empty() => Ok(Self {
                provider_raw: provider.to_string(),
                source_id: None,
                model_raw: String::new(),
            }),
            (None, Some(source)) => Ok(Self {
                provider_raw: String::new(),
                source_id: Some(source),
                model_raw: String::new(),
            }),
            _ => Err(invalid("model config identity is empty")),
        }
    }

    fn with_model(mut self, model_raw: &str) -> Self {
        self.model_raw = model_raw.to_string();
        self
    }

    fn from_record(record: &dao::ModelConfigRecord) -> Self {
        Self {
            provider_raw: record.provider_raw.clone(),
            source_id: record.source_id,
            model_raw: record.model_raw.clone(),
        }
    }
}

/// 观测身份集合（防幽灵行的成员判据）。usage 总开关关闭 ⇒ 空集合。
fn observed_identities_with_conn(conn: &Connection) -> Result<HashSet<ModelIdentity>, UsageError> {
    if !settings::load_from_conn(conn).enabled {
        return Ok(HashSet::new());
    }
    let mut stmt = conn
        .prepare(
            "SELECT source_id, provider, model FROM usage_daily
             GROUP BY source_id, provider, model",
        )
        .map_err(|e| db_failed("observed identities select failed", e))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|e| db_failed("observed identities select failed", e))?;
    let mut set = HashSet::new();
    for row in rows {
        let (source_raw, provider, model) =
            row.map_err(|e| db_failed("observed identities row read failed", e))?;
        let Some(source_id) = parse_source_id(&source_raw) else {
            continue;
        };
        set.insert(if provider.is_empty() {
            ModelIdentity {
                provider_raw: String::new(),
                source_id: Some(source_id),
                model_raw: model,
            }
        } else {
            ModelIdentity {
                provider_raw: provider,
                source_id: None,
                model_raw: model,
            }
        });
    }
    Ok(set)
}

/// 观察条目（A 段与 B 段的共同数据源；全历史，来自 usage_daily，与查询范围无关）。
///
/// 排序：`display_provider` 升序（`None` 排最后）→ `model_id` 升序；
/// 空模型（`model_id=""`）照常返回（前端显示「未知模型」）。
pub fn observed_entries_with_conn(
    conn: &Connection,
    now_ms: i64,
) -> Result<Vec<ObservedModelEntry>, UsageError> {
    if !settings::load_from_conn(conn).enabled {
        return Ok(Vec::new());
    }
    let aliases = channel::load_with_conn(conn)?;
    // 不触 billing attribution：空配置的解析器只暴露别名映射。
    let empty_config = BillingConfig {
        revision: 0,
        profiles: Vec::new(),
        rules: Vec::new(),
        pricing_rules: Vec::new(),
    };
    let resolver = ChannelResolver::new(&empty_config, &aliases);
    let configs: HashMap<ModelIdentity, dao::ModelConfigRecord> = dao::load_all_conn(conn)?
        .into_iter()
        .map(|record| (ModelIdentity::from_record(&record), record))
        .collect();

    // 条目 Top10 用最近 7 天（宿主本地日界，含今日）；全历史总量仍保留用于展开排序。
    let cutoff =
        crate::services::usage::range::day_key_of_host(now_ms.saturating_sub(6 * 86_400_000));
    let mut stmt = conn
        .prepare(
            "SELECT source_id, provider, model, SUM(request_total), SUM(output),
                    SUM(CASE WHEN day_key >= ?1 THEN request_total + output ELSE 0 END)
             FROM usage_daily
             GROUP BY source_id, provider, model",
        )
        .map_err(|e| db_failed("observed entries select failed", e))?;
    let rows = stmt
        .query_map([cutoff], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })
        .map_err(|e| db_failed("observed entries select failed", e))?;

    let mut entries = Vec::new();
    for row in rows {
        let (source_raw, provider, model, request_total, output, recent_tokens_7d) =
            row.map_err(|e| db_failed("observed entries row read failed", e))?;
        let Some(source_id) = parse_source_id(&source_raw) else {
            log::warn!(
                "usage model config: skipping observed row with unknown sourceId {source_raw:?}"
            );
            continue;
        };
        let empty_provider = provider.is_empty();
        let identity = ModelIdentity {
            provider_raw: provider.clone(),
            source_id: empty_provider.then_some(source_id),
            model_raw: model.clone(),
        };
        let display_provider = resolver.display_provider_of(
            source_id,
            (!empty_provider).then_some(provider.as_str()),
            &model,
        );
        let (display_model_name, pricing_provider, pricing_model) = configs
            .get(&identity)
            .map(|record| {
                (
                    record.display_name.clone(),
                    record.pricing_provider.clone(),
                    record.pricing_model.clone(),
                )
            })
            .unwrap_or((None, None, None));
        let total_tokens = request_total.saturating_add(output);
        let normalized_model_id = normalize_model_id(&model);
        entries.push(ObservedModelEntry {
            provider_raw: (!empty_provider).then_some(provider),
            source_id: empty_provider.then_some(source_id),
            model_id: model,
            normalized_model_id,
            display_provider,
            display_model_name,
            pricing_provider,
            pricing_model,
            total_tokens,
            recent_tokens_7d,
        });
    }
    entries.sort_by(
        |left, right| match (&left.display_provider, &right.display_provider) {
            (None, None) => left.model_id.cmp(&right.model_id),
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (Some(_), None) => std::cmp::Ordering::Less,
            (Some(a), Some(b)) => a.cmp(b).then_with(|| left.model_id.cmp(&right.model_id)),
        },
    );
    Ok(entries)
}

/// 写入前校验（幽灵身份拒绝）+ 按 identity 整行读改写。
///
/// `patch` 返回 `None` = 清除后两段配置都为空 ⇒ 删除整行（回未配置态）。
fn apply_config_patch(
    conn: &Connection,
    identity: &ModelIdentity,
    patch: impl FnOnce(&mut dao::ModelConfigRecord),
    now_ms: i64,
) -> Result<(), UsageError> {
    if !observed_identities_with_conn(conn)?.contains(identity) {
        return Err(invalid("model config entry not observed"));
    }
    let existing = dao::find_conn(
        conn,
        &identity.provider_raw,
        identity.source_id,
        &identity.model_raw,
    )?;
    let existed = existing.is_some();
    let mut record = existing.unwrap_or(dao::ModelConfigRecord {
        provider_raw: identity.provider_raw.clone(),
        source_id: identity.source_id,
        model_raw: identity.model_raw.clone(),
        display_name: None,
        pricing_provider: None,
        pricing_model: None,
    });
    patch(&mut record);
    if record.display_name.is_none() && record.pricing_provider.is_none() {
        if existed {
            dao::delete_conn(
                conn,
                &identity.provider_raw,
                identity.source_id,
                &identity.model_raw,
            )?;
        }
        return Ok(());
    }
    dao::upsert_conn(conn, &record, now_ms)
}

/// 设置自定义模型显示名；`display_name` 为 null = 清除（回原名）。
pub fn set_display_name_with_conn(
    conn: &Connection,
    request: &ModelDisplayNameRequest,
    now_ms: i64,
) -> Result<Vec<ObservedModelEntry>, UsageError> {
    let identity =
        ModelIdentity::from_request_parts(request.provider_raw.as_deref(), request.source_id)?
            .with_model(&request.model_id);
    let display = match request.display_name.as_deref() {
        Some(name) => {
            let trimmed = name.trim();
            if trimmed.is_empty() {
                return Err(invalid("model config display name is empty after trim"));
            }
            Some(trimmed.to_string())
        }
        None => None,
    };
    apply_config_patch(
        conn,
        &identity,
        |record| record.display_name = display,
        now_ms,
    )?;
    observed_entries_with_conn(conn, now_ms)
}

/// 设置计价身份覆盖；`reference` 为 null = 清除覆盖（回自动匹配）。
pub fn set_pricing_identity_with_conn(
    conn: &Connection,
    request: &PricingIdentityRequest,
    now_ms: i64,
) -> Result<Vec<ObservedModelEntry>, UsageError> {
    let identity =
        ModelIdentity::from_request_parts(request.provider_raw.as_deref(), request.source_id)?
            .with_model(&request.model_id);
    let pricing = match request.reference.as_ref() {
        Some(reference) => {
            let provider = reference.provider_id.trim();
            let model = reference.model_id.trim();
            if provider.is_empty() || model.is_empty() {
                return Err(invalid("model config pricing reference is incomplete"));
            }
            (Some(provider.to_string()), Some(model.to_string()))
        }
        None => (None, None),
    };
    apply_config_patch(
        conn,
        &identity,
        |record| {
            record.pricing_provider = pricing.0;
            record.pricing_model = pricing.1;
        },
        now_ms,
    )?;
    observed_entries_with_conn(conn, now_ms)
}

/// cost_query 的一次性行级配置索引（2026-10-20 计划 §3.3 计价身份覆盖 + §3.5 displayModelName）。
///
/// 费用查询按行身份（`(source, observed_provider, model)`）查 `usage_model_config`；
/// 一次性加载全表避免逐行查库（与 billing/alias 配置同模式）。
pub(crate) struct RowConfigIndex {
    by_identity: HashMap<ModelIdentity, dao::ModelConfigRecord>,
}

impl RowConfigIndex {
    pub(crate) fn load_with_conn(conn: &Connection) -> Result<Self, UsageError> {
        let by_identity = dao::load_all_conn(conn)?
            .into_iter()
            .map(|record| (ModelIdentity::from_record(&record), record))
            .collect();
        Ok(Self { by_identity })
    }

    /// 费用行身份 → 配置身份（非空 provider 跨工具；空 provider 按「工具+模型」）。
    fn key_of(identity: &crate::services::usage::v2::model::IdentityKey) -> ModelIdentity {
        match identity.observed_provider.as_deref() {
            Some(provider) if !provider.is_empty() => ModelIdentity {
                provider_raw: provider.to_string(),
                source_id: None,
                model_raw: identity.model_id.clone(),
            },
            _ => ModelIdentity {
                provider_raw: String::new(),
                source_id: Some(identity.source_id),
                model_raw: identity.model_id.clone(),
            },
        }
    }

    /// 该身份的自定义模型显示名（空串视为未配置）。
    pub(crate) fn display_name(
        &self,
        identity: &crate::services::usage::v2::model::IdentityKey,
    ) -> Option<String> {
        let record = self.by_identity.get(&Self::key_of(identity))?;
        record.display_name.clone().filter(|name| !name.is_empty())
    }

    /// 该身份的计价身份覆盖（成对；单侧缺失视为未覆盖）。
    pub(crate) fn pricing_identity(
        &self,
        identity: &crate::services::usage::v2::model::IdentityKey,
    ) -> Option<(String, String)> {
        let record = self.by_identity.get(&Self::key_of(identity))?;
        Some((
            record.pricing_provider.clone()?,
            record.pricing_model.clone()?,
        ))
    }
}

// ── 生产入口（IPC 用）：只取 DB 锁，业务走 `*_with_conn` ─────────────────────

pub fn observed_entries(now_ms: i64) -> Result<Vec<ObservedModelEntry>, UsageError> {
    let conn = DB.lock().map_err(|_| poisoned())?;
    observed_entries_with_conn(&conn, now_ms)
}

pub fn set_display_name(
    request: &ModelDisplayNameRequest,
    now_ms: i64,
) -> Result<Vec<ObservedModelEntry>, UsageError> {
    let conn = DB.lock().map_err(|_| poisoned())?;
    set_display_name_with_conn(&conn, request, now_ms)
}

pub fn set_pricing_identity(
    request: &PricingIdentityRequest,
    now_ms: i64,
) -> Result<Vec<ObservedModelEntry>, UsageError> {
    let conn = DB.lock().map_err(|_| poisoned())?;
    set_pricing_identity_with_conn(&conn, request, now_ms)
}

// ==== usage 自省锁：以下为排除区（测试代码），勿删勿复制 ====
#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        conn
    }

    /// 观测条目测试的固定“当前时刻”（2026-10-20 12:00 宿主本地时区）。
    fn test_now() -> i64 {
        use chrono::TimeZone;
        chrono::Local
            .with_ymd_and_hms(2026, 10, 20, 12, 0, 0)
            .single()
            .unwrap()
            .timestamp_millis()
    }

    fn add_daily(
        conn: &Connection,
        day: &str,
        source: &str,
        provider: &str,
        model: &str,
        output: i64,
        request_total: i64,
    ) {
        conn.execute(
            "INSERT INTO usage_daily
             (day_key, source_id, project_key, provider, provider_kind, model,
              input_fresh, cache_read, cache_write, output, request_total, requests,
              user_est, updated_at)
             VALUES (?1, ?2, '', ?3, 'measured', ?4, 1, 0, 0, ?5, ?6, 1, NULL, 0)",
            params![day, source, provider, model, output, request_total],
        )
        .unwrap();
    }

    fn add_provider_alias(conn: &Connection, id: &str, provider: &str, display: &str) {
        conn.execute(
            "INSERT INTO usage_provider_alias
             (id, kind, provider_raw, source_id, model_id, display_name, created_at, updated_at)
             VALUES (?1, 'provider', ?2, NULL, NULL, ?3, 0, 0)",
            params![id, provider, display],
        )
        .unwrap();
    }

    fn add_tool_model_alias(
        conn: &Connection,
        id: &str,
        source: UsageSourceId,
        model: &str,
        display: &str,
    ) {
        conn.execute(
            "INSERT INTO usage_provider_alias
             (id, kind, provider_raw, source_id, model_id, display_name, created_at, updated_at)
             VALUES (?1, 'toolModel', NULL, ?2, ?3, ?4, 0, 0)",
            params![id, source.db_id(), model, display],
        )
        .unwrap();
    }

    fn request(
        provider: Option<&str>,
        source: Option<UsageSourceId>,
        model: &str,
        display: Option<&str>,
    ) -> ModelDisplayNameRequest {
        ModelDisplayNameRequest {
            provider_raw: provider.map(str::to_string),
            source_id: source,
            model_id: model.to_string(),
            display_name: display.map(str::to_string),
        }
    }

    /// 观察条目聚合（§3.1）：全历史、空 provider 单列（带 source_id）、空模型照常返回、
    /// 排序 = display_provider 升序（None 最后）→ model_id 升序；totalTokens = requestTotal+output。
    #[test]
    fn observed_entries_aggregate_full_history_and_sort_stably() {
        let conn = mem();
        // 同一 (provider, model) 跨两天聚合；历史 provider（anthropic）无「当前」概念也必须在列。
        add_daily(&conn, "2026-10-16", "codex", "openai", "gpt-5", 2, 3);
        add_daily(&conn, "2026-10-17", "codex", "openai", "gpt-5", 4, 5);
        add_daily(&conn, "2026-10-17", "claude", "anthropic", "claude-x", 6, 7);
        // 空 provider（workbuddy）+ 空模型（未知模型）各一。
        add_daily(&conn, "2026-10-17", "workbuddy", "", "", 1, 2);

        let entries = observed_entries_with_conn(&conn, test_now()).unwrap();
        assert_eq!(entries.len(), 3, "聚合后恰好三个观测条目");
        // 排序：display_provider 升序（anthropic < openai），None 排最后。
        assert_eq!(entries[0].provider_raw.as_deref(), Some("anthropic"));
        assert_eq!(entries[1].provider_raw.as_deref(), Some("openai"));
        assert_eq!(
            entries[2].provider_raw, None,
            "空 provider 条目 providerRaw=null"
        );
        assert_eq!(entries[2].source_id, Some(UsageSourceId::WorkBuddy));
        assert_eq!(entries[2].model_id, "", "空模型照常返回（未知模型）");
        assert!(
            entries[2].display_provider.is_none(),
            "未指派的空 provider 无显示渠道"
        );
        // 聚合：openai/gpt-5 跨两天 requestTotal 3+5=8、output 2+4=6 ⇒ totalTokens 14。
        let openai = &entries[1];
        assert_eq!(openai.model_id, "gpt-5");
        assert_eq!(openai.total_tokens, 14);
        assert_eq!(openai.display_provider.as_deref(), Some("openai"));
        // 未配置条目的三段配置全 null（显式序列化由 serde 契约测试锁）。
        assert_eq!(openai.display_model_name, None);
        assert_eq!(openai.pricing_provider, None);
        assert_eq!(openai.pricing_model, None);
    }

    /// normalizedModelId（2026-10-20 wire 契约）：观察条目必须携带后端机械归一化的
    /// 模型 ID——`GLM-5.3` → `glm-5-3`、无尾缀模型原样 `gpt-5`、空模型仍为空串；
    /// **非空模型也可能被归一化折成空串**（`vendor/` 取尾后为空 ⇒ 前端据此禁用
    /// 覆盖价入口）；序列化后恒为字符串类型（"" 不是 null），前端不重复实现归一化。
    #[test]
    fn observed_entries_fill_normalized_model_id_and_serialize_as_string() {
        let conn = mem();
        add_daily(&conn, "2026-10-17", "codex", "zhipu", "GLM-5.3", 2, 3);
        add_daily(&conn, "2026-10-17", "codex", "openai", "gpt-5", 4, 5);
        add_daily(&conn, "2026-10-17", "workbuddy", "", "", 1, 2);
        // 反例：非空模型（仅 vendor 前缀 + 斜杠）被归一化剥成空串。
        add_daily(&conn, "2026-10-17", "codex", "weird", "vendor/", 1, 1);

        let entries = observed_entries_with_conn(&conn, test_now()).unwrap();
        assert_eq!(entries.len(), 4, "四个观测身份各一条");
        let normalized_of = |model: &str| {
            entries
                .iter()
                .find(|entry| entry.model_id == model)
                .unwrap_or_else(|| panic!("缺少模型 {model:?} 的观测条目"))
                .normalized_model_id
                .clone()
        };
        assert_eq!(
            normalized_of("GLM-5.3"),
            "glm-5-3",
            "大写与点号经机械归一化"
        );
        assert_eq!(normalized_of("gpt-5"), "gpt-5", "无尾缀模型原样保留");
        assert_eq!(normalized_of(""), "", "空模型归一化仍为空串");
        assert_eq!(
            normalized_of("vendor/"),
            "",
            "非空模型归一化也可能折成空串（前端禁用覆盖入口的判据）"
        );

        // wire 面：normalizedModelId 恒为字符串；空模型是 "" 而不是 null。
        let empty_model = entries
            .iter()
            .find(|entry| entry.model_id.is_empty())
            .unwrap();
        let v = serde_json::to_value(empty_model).unwrap();
        assert!(
            v["normalizedModelId"].is_string(),
            "空模型的 normalizedModelId 必须是字符串类型，不得为 null"
        );
        assert_eq!(v["normalizedModelId"], "", "空模型 ⇒ 空串上 wire");

        // 归一化折空的非空模型同样以 "" 上 wire（与空模型同语义、可区分原始 modelId）。
        let vendor_only = entries
            .iter()
            .find(|entry| entry.model_id == "vendor/")
            .unwrap();
        let v = serde_json::to_value(vendor_only).unwrap();
        assert!(
            v["normalizedModelId"].is_string(),
            "vendor/ 的 normalizedModelId 必须是字符串类型，不得为 null"
        );
        assert_eq!(v["normalizedModelId"], "", "vendor/ ⇒ 归一化空串上 wire");
        assert_eq!(v["modelId"], "vendor/", "原始 modelId 保持原文（证据面）");
    }

    /// usage 总开关关闭 ⇒ 空数组（与 identities_with_conn 一致），且写入被幽灵校验拒绝。
    #[test]
    fn disabled_usage_switch_returns_empty_observed_entries() {
        let conn = mem();
        add_daily(&conn, "2026-10-17", "codex", "openai", "gpt-5", 2, 3);
        let raw = serde_json::to_string(&crate::services::usage::model::UsageSettings {
            enabled: false,
            ..Default::default()
        })
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('usage.settings', ?1)",
            params![raw],
        )
        .unwrap();

        assert!(
            observed_entries_with_conn(&conn, test_now())
                .unwrap()
                .is_empty(),
            "总开关关闭 ⇒ 空数组"
        );
        let err = set_display_name_with_conn(
            &conn,
            &request(Some("openai"), None, "gpt-5", Some("X")),
            1_000,
        )
        .unwrap_err();
        assert_eq!(err.detail, "model config entry not observed");
    }

    /// display_provider 别名解析：provider 规则 / toolModel 规则 / 未指派 None；
    /// 同一显示名合并两个原始 provider 时两条目各自在列（身份键不合并）。
    #[test]
    fn display_provider_resolves_alias_rules_without_billing() {
        let conn = mem();
        add_daily(&conn, "2026-10-17", "codex", "openai", "gpt-5", 2, 3);
        add_daily(&conn, "2026-10-17", "claude", "azure-openai", "gpt-5", 4, 5);
        add_daily(&conn, "2026-10-17", "workbuddy", "", "wb-model", 1, 2);
        add_provider_alias(&conn, "a1", "openai", "OpenAI 官方");
        add_provider_alias(&conn, "a2", "azure-openai", "OpenAI 官方");
        add_tool_model_alias(&conn, "t1", UsageSourceId::WorkBuddy, "wb-model", "WB 渠道");

        let entries = observed_entries_with_conn(&conn, test_now()).unwrap();
        assert_eq!(entries.len(), 3, "别名合并显示名，但不合并观测身份条目");
        // 排序按同一显示名升序：三个条目 display_provider 相同 → model_id 升序。
        for entry in &entries {
            match entry.provider_raw.as_deref() {
                Some("openai") | Some("azure-openai") => {
                    assert_eq!(
                        entry.display_provider.as_deref(),
                        Some("OpenAI 官方"),
                        "provider 别名规则生效"
                    );
                }
                None => {
                    assert_eq!(
                        entry.display_provider.as_deref(),
                        Some("WB 渠道"),
                        "toolModel 指派生效"
                    );
                }
                other => panic!("意外条目 {other:?}"),
            }
        }
    }

    /// 显示名 upsert：写入可见、覆盖写、清除（None）回 null 且行删除；
    /// 空白 trim 拒绝（固定 detail）；幽灵身份拒绝；只改显示名时计价身份保留。
    #[test]
    fn display_name_upsert_round_trips_and_validates() {
        let conn = mem();
        add_daily(&conn, "2026-10-17", "codex", "openai", "gpt-5", 2, 3);

        // 幽灵身份（未知模型）拒绝。
        let err = set_display_name_with_conn(
            &conn,
            &request(Some("openai"), None, "ghost-model", Some("X")),
            1_000,
        )
        .unwrap_err();
        assert_eq!(err.code, "usage-settings-invalid");
        assert_eq!(err.detail, "model config entry not observed");

        // 写入 → 覆盖写 → 清除。
        let entries = set_display_name_with_conn(
            &conn,
            &request(Some("openai"), None, "gpt-5", Some(" GPT ")),
            1_000,
        )
        .unwrap();
        let entry = entries
            .iter()
            .find(|entry| entry.provider_raw.as_deref() == Some("openai"))
            .unwrap();
        assert_eq!(
            entry.display_model_name.as_deref(),
            Some("GPT"),
            "trim 后入库"
        );
        let entries = set_display_name_with_conn(
            &conn,
            &request(Some("openai"), None, "gpt-5", Some("GPT-5")),
            1_100,
        )
        .unwrap();
        assert_eq!(
            entries[0].display_model_name.as_deref(),
            Some("GPT-5"),
            "同身份覆盖写"
        );

        // 显示名 + 计价身份并存时，清除显示名不得动计价身份。
        set_pricing_identity_with_conn(
            &conn,
            &PricingIdentityRequest {
                provider_raw: Some("openai".to_string()),
                source_id: None,
                model_id: "gpt-5".to_string(),
                reference: Some(crate::services::usage::v2::model::PricingReference {
                    provider_id: "openai".to_string(),
                    model_id: "gpt-5-2024-11-20".to_string(),
                }),
            },
            1_200,
        )
        .unwrap();
        let entries =
            set_display_name_with_conn(&conn, &request(Some("openai"), None, "gpt-5", None), 1_300)
                .unwrap();
        let entry = entries
            .iter()
            .find(|entry| entry.provider_raw.as_deref() == Some("openai"))
            .unwrap();
        assert_eq!(entry.display_model_name, None, "清除后回 null");
        assert_eq!(
            entry.pricing_provider.as_deref(),
            Some("openai"),
            "另一半配置必须保留"
        );
        assert_eq!(entry.pricing_model.as_deref(), Some("gpt-5-2024-11-20"));

        // 空白显示名（意图为设置）固定 detail 拒绝。
        let err = set_display_name_with_conn(
            &conn,
            &request(Some("openai"), None, "gpt-5", Some("   ")),
            1_400,
        )
        .unwrap_err();
        assert_eq!(err.detail, "model config display name is empty after trim");
    }

    /// 计价身份 upsert：成对写、不完整 reference 拒绝（固定 detail）、清除回 null；
    /// 两段都清空时整行删除（回未配置态，无孤儿行）。
    #[test]
    fn pricing_identity_upsert_round_trips_and_validates() {
        let conn = mem();
        add_daily(&conn, "2026-10-17", "codex", "openai", "gpt-5", 2, 3);
        let identity_request =
            |reference: Option<crate::services::usage::v2::model::PricingReference>| {
                PricingIdentityRequest {
                    provider_raw: Some("openai".to_string()),
                    source_id: None,
                    model_id: "gpt-5".to_string(),
                    reference,
                }
            };

        let entries = set_pricing_identity_with_conn(
            &conn,
            &identity_request(Some(crate::services::usage::v2::model::PricingReference {
                provider_id: "openai".to_string(),
                model_id: "gpt-5-2024-11-20".to_string(),
            })),
            1_000,
        )
        .unwrap();
        let entry = entries
            .iter()
            .find(|entry| entry.provider_raw.as_deref() == Some("openai"))
            .unwrap();
        assert_eq!(entry.pricing_provider.as_deref(), Some("openai"));
        assert_eq!(entry.pricing_model.as_deref(), Some("gpt-5-2024-11-20"));

        for (provider, model) in [("", "m"), ("openai", ""), ("  ", "m")] {
            let err = set_pricing_identity_with_conn(
                &conn,
                &identity_request(Some(crate::services::usage::v2::model::PricingReference {
                    provider_id: provider.to_string(),
                    model_id: model.to_string(),
                })),
                1_100,
            )
            .unwrap_err();
            assert_eq!(err.detail, "model config pricing reference is incomplete");
        }

        // 先给显示名再清计价身份：显示名保留、行不删。
        set_display_name_with_conn(
            &conn,
            &request(Some("openai"), None, "gpt-5", Some("N")),
            1_200,
        )
        .unwrap();
        let entries =
            set_pricing_identity_with_conn(&conn, &identity_request(None), 1_300).unwrap();
        let entry = entries
            .iter()
            .find(|entry| entry.provider_raw.as_deref() == Some("openai"))
            .unwrap();
        assert_eq!(entry.pricing_provider, None, "清除后回 null（回自动匹配）");
        assert_eq!(entry.display_model_name.as_deref(), Some("N"));

        // 两段都空 ⇒ 整行删除：再清显示名后配置表应无行。
        set_display_name_with_conn(&conn, &request(Some("openai"), None, "gpt-5", None), 1_400)
            .unwrap();
        assert!(
            dao::load_all_conn(&conn).unwrap().is_empty(),
            "两段配置都清空时不得留孤儿行"
        );
    }

    /// 身份请求形状：provider 与 sourceId 同现 / 全空 ⇒ `model config identity is empty`。
    #[test]
    fn identity_shape_validation_rejects_mixed_or_empty_keys() {
        let conn = mem();
        add_daily(&conn, "2026-10-17", "codex", "openai", "gpt-5", 2, 3);
        for (provider, source) in [
            (Some("openai"), Some(UsageSourceId::Codex)),
            (Some(""), Some(UsageSourceId::Codex)),
            (Some("   "), None),
            (None, None),
        ] {
            let err = set_display_name_with_conn(
                &conn,
                &request(provider, source, "gpt-5", Some("X")),
                1_000,
            )
            .unwrap_err();
            assert_eq!(err.code, "usage-settings-invalid");
            assert_eq!(err.detail, "model config identity is empty");
        }
    }
}
// ==== usage 自省锁：排除区结束，勿删勿复制 ====
