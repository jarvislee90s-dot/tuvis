//! 计费配置的持久化 DAO（Task 3）。
//!
//! 全部 `*_conn` 形态：调用方持连接；本层不取全局锁、不做业务校验。
//! `usage_billing_state` 只保留当前版本（singleton=1），`usage_billing_revision` 保留
//! 每次提交的历史快照——撤销也走「追加新版本」，绝不删除历史。
//!
//! 错误约定：所有 SQLite / JSON 读写失败统一收敛为 `usage-db-failed`，不得吞错；
//! 版本冲突属于业务错误，返回 `usage-settings-invalid`（detail 逐字固定）。

use rusqlite::{params, Connection, OptionalExtension};

use crate::services::usage::v2::model::{
    BillingConfig, BillingRevision, PriceCurrency, PricingStatus, TimePriceTimezone,
    TimePriceWindow,
};
use crate::services::usage::UsageError;

fn db_failed(context: &str, err: impl std::fmt::Display) -> UsageError {
    UsageError::new("usage-db-failed", format!("{context}: {err}"))
}

fn empty_config() -> BillingConfig {
    BillingConfig {
        revision: 0,
        profiles: Vec::new(),
        rules: Vec::new(),
        pricing_rules: Vec::new(),
    }
}

fn parse_config(json: &str) -> Result<BillingConfig, UsageError> {
    serde_json::from_str(json).map_err(|e| db_failed("billing config decode failed", e))
}

/// 当前 revision；无 state 行 = 0。SQLite 错误必须上抛。
fn current_revision(conn: &Connection) -> Result<i64, UsageError> {
    conn.query_row(
        "SELECT revision FROM usage_billing_state WHERE singleton = 1",
        [],
        |row| row.get::<_, i64>(0),
    )
    .optional()
    .map_err(|e| db_failed("billing state read failed", e))
    .map(|revision| revision.unwrap_or(0))
}

/// 读取当前有效配置；无 state 行返回 revision=0 的空配置。
pub fn load_config_conn(conn: &Connection) -> Result<BillingConfig, UsageError> {
    let row: Option<(i64, String)> = conn
        .query_row(
            "SELECT revision, config_json FROM usage_billing_state WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|e| db_failed("billing state read failed", e))?;
    match row {
        None => Ok(empty_config()),
        Some((revision, json)) => {
            let mut config = parse_config(&json)?;
            // 以列值为权威，防止 JSON 内部 revision 与 state 列漂移。
            config.revision = revision;
            Ok(config)
        }
    }
}

/// 在**短事务**内比较 expected revision、追加历史快照并更新当前 state。
///
/// `config.revision` 会被改写为 `current + 1` 后序列化；事务提交前任何失败都回滚。
pub fn commit_config_conn(
    conn: &mut Connection,
    config: &BillingConfig,
    expected_revision: i64,
    now_ms: i64,
) -> Result<BillingRevision, UsageError> {
    let tx = conn
        .transaction()
        .map_err(|e| db_failed("billing transaction begin failed", e))?;

    let current = current_revision(&tx)?;
    if current != expected_revision {
        return Err(UsageError::new(
            "usage-settings-invalid",
            "billing revision conflict",
        ));
    }

    let new_revision = current
        .checked_add(1)
        .ok_or_else(|| db_failed("billing revision overflow", current))?;

    let mut stored = config.clone();
    stored.revision = new_revision;
    let json =
        serde_json::to_string(&stored).map_err(|e| db_failed("billing config encode failed", e))?;

    // 先写历史快照，再换当前 state；同一事务内失败整体回滚。
    tx.execute(
        "INSERT INTO usage_billing_revision (revision, committed_at, config_json)
         VALUES (?1, ?2, ?3)",
        params![new_revision, now_ms, &json],
    )
    .map_err(|e| db_failed("billing revision insert failed", e))?;
    tx.execute(
        "INSERT INTO usage_billing_state (singleton, revision, config_json)
         VALUES (1, ?1, ?2)
         ON CONFLICT(singleton) DO UPDATE SET
             revision = excluded.revision,
             config_json = excluded.config_json",
        params![new_revision, &json],
    )
    .map_err(|e| db_failed("billing state upsert failed", e))?;

    tx.commit()
        .map_err(|e| db_failed("billing transaction commit failed", e))?;
    Ok(BillingRevision {
        revision: new_revision,
        committed_at: now_ms,
    })
}

/// 读取指定历史 revision 的快照；不存在返回 `None`。
/// revision 0 表示「空配置」，没有历史行，因此也返回 `None`（调用方按空配置处理）。
pub fn load_revision_conn(
    conn: &Connection,
    revision: i64,
) -> Result<Option<BillingConfig>, UsageError> {
    if revision <= 0 {
        return Ok(None);
    }
    let row: Option<(i64, String)> = conn
        .query_row(
            "SELECT revision, config_json FROM usage_billing_revision WHERE revision = ?1",
            params![revision],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|e| db_failed("billing revision read failed", e))?;
    match row {
        None => Ok(None),
        Some((stored_revision, json)) => {
            let mut config = parse_config(&json)?;
            config.revision = stored_revision;
            Ok(Some(config))
        }
    }
}

// ── 价格快照 DAO（Task 8）────────────────────────────────────────────────────
// 全部返回 `Result`，不吞 SQL；快照只存在于 tuvis.db，不使用临时价格文件。

const PRICING_TTL_MS: i64 = 24 * 60 * 60 * 1000;

/// 当前定价状态。`refreshing` 来自 pricing 模块的进程级 AtomicBool（刷新在飞时为 true）。
/// 无 state 行 ⇒ 从未启用：enabled=false、snapshotId/fetchedAt=null、stale=true。
pub fn pricing_status_conn(conn: &Connection, now_ms: i64) -> Result<PricingStatus, UsageError> {
    let state: Option<(i64, Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT enabled, active_snapshot_id, last_error
             FROM usage_pricing_state WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(|e| db_failed("pricing state read failed", e))?;
    let refreshing =
        crate::services::usage::v2::pricing::REFRESHING.load(std::sync::atomic::Ordering::Relaxed);

    let Some((enabled, active_snapshot_id, last_error)) = state else {
        return Ok(PricingStatus {
            enabled: false,
            snapshot_id: None,
            fetched_at: None,
            stale: true,
            refreshing,
            last_error: None,
            fx_rate: None,
            fx_fetched_at: None,
            fx_source: None,
            fx_fallback: false,
        });
    };

    let fetched_at: Option<i64> = match active_snapshot_id.as_deref() {
        Some(id) => conn
            .query_row(
                "SELECT fetched_at FROM usage_pricing_snapshot WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| db_failed("pricing snapshot read failed", e))?,
        None => None,
    };
    let stale = active_snapshot_id.is_none()
        || fetched_at.is_none()
        || now_ms.saturating_sub(fetched_at.unwrap_or(now_ms)) >= PRICING_TTL_MS
        || last_error.is_some();

    // 当日汇率：优先 active 快照；缺失时沿用最近一次成功（标记 fallback）。
    let mut fx: Option<(i64, i64)> = None;
    let mut fx_is_active = false;
    if let Some(id) = active_snapshot_id.as_deref() {
        fx = conn
            .query_row(
                "SELECT rate_scaled, fetched_at
                 FROM usage_pricing_fx WHERE snapshot_id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|e| db_failed("pricing fx read failed", e))?;
        fx_is_active = fx.is_some();
        if fx.is_none() {
            fx = conn
                .query_row(
                    "SELECT rate_scaled, fetched_at
                     FROM usage_pricing_fx ORDER BY fetched_at DESC LIMIT 1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(|e| db_failed("pricing fx fallback read failed", e))?;
        }
    }
    let (fx_rate, fx_fetched_at, fx_source, fx_fallback) = match fx {
        Some((rate, at)) => (
            Some(crate::services::usage::v2::pricing::rate_string(
                i128::from(rate),
            )),
            Some(at),
            Some(if fx_is_active { "er-api" } else { "cached" }.to_string()),
            !fx_is_active,
        ),
        None => (None, None, None, false),
    };

    Ok(PricingStatus {
        enabled: enabled != 0,
        snapshot_id: active_snapshot_id,
        fetched_at,
        stale,
        refreshing,
        last_error,
        fx_rate,
        fx_fetched_at,
        fx_source,
        fx_fallback,
    })
}

/// 开关定价。只更新 enabled，保留既有 active_snapshot_id / last_error。
pub fn set_pricing_enabled_conn(conn: &Connection, enabled: bool) -> Result<(), UsageError> {
    conn.execute(
        "INSERT INTO usage_pricing_state (singleton, enabled, active_snapshot_id, last_error)
         VALUES (1, ?1, NULL, NULL)
         ON CONFLICT(singleton) DO UPDATE SET enabled = excluded.enabled",
        params![if enabled { 1 } else { 0 }],
    )
    .map_err(|e| db_failed("pricing enabled write failed", e))?;
    Ok(())
}

/// 写入新快照并原子切换 active；清空 last_error。事务失败时旧 active 与旧快照都不变。
///
/// 同一事务内做 GC：只保留「当前 active + 最近 1 个历史快照」（按 fetched_at 降序、
/// id 打破平局），更旧的行删除——快照 id = sha256:fetched_at，每次刷新成功都会落一行
/// ~5.5MB payload，active 切走后旧行是永久死数据。离线回退兼容：全部读取路径
/// （`load_active_snapshot_meta_conn` / `load_active_snapshot_conn`）都经 active 指针
/// JOIN，active 行永不删除；保留的 1 个历史快照内容完整，可供回退兜底。
pub fn activate_snapshot_conn(
    conn: &mut Connection,
    id: &str,
    url: &str,
    fetched_at: i64,
    payload: &str,
) -> Result<(), UsageError> {
    let tx = conn
        .transaction()
        .map_err(|e| db_failed("pricing snapshot transaction begin failed", e))?;
    tx.execute(
        "INSERT INTO usage_pricing_snapshot (id, source_url, fetched_at, payload_json)
         VALUES (?1, ?2, ?3, ?4)",
        params![id, url, fetched_at, payload],
    )
    .map_err(|e| db_failed("pricing snapshot insert failed", e))?;
    tx.execute(
        "INSERT INTO usage_pricing_state (singleton, enabled, active_snapshot_id, last_error)
         VALUES (1, 0, ?1, NULL)
         ON CONFLICT(singleton) DO UPDATE SET
             active_snapshot_id = excluded.active_snapshot_id,
             last_error = NULL",
        params![id],
    )
    .map_err(|e| db_failed("pricing active switch failed", e))?;
    // GC：先物化要保留的最近历史快照 id（避免对同一张表边查边删），再删除其余。
    // 没有历史快照时 keep 退化为 active 自身，DELETE 只清孤行（理论不可达，防御性保留）。
    let keep_id: Option<String> = tx
        .query_row(
            "SELECT id FROM usage_pricing_snapshot WHERE id <> ?1
             ORDER BY fetched_at DESC, id DESC LIMIT 1",
            params![id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| db_failed("pricing snapshot gc select failed", e))?;
    let keep = keep_id.unwrap_or_else(|| id.to_string());
    tx.execute(
        "DELETE FROM usage_pricing_snapshot WHERE id <> ?1 AND id <> ?2",
        params![id, keep],
    )
    .map_err(|e| db_failed("pricing snapshot gc delete failed", e))?;
    // 人民币价 / 当日汇率与快照同生命周期：只保留 active + 最近 1 个历史快照的行。
    tx.execute(
        "DELETE FROM usage_pricing_native WHERE snapshot_id <> ?1 AND snapshot_id <> ?2",
        params![id, keep],
    )
    .map_err(|e| db_failed("pricing native gc delete failed", e))?;
    tx.execute(
        "DELETE FROM usage_pricing_fx WHERE snapshot_id <> ?1 AND snapshot_id <> ?2",
        params![id, keep],
    )
    .map_err(|e| db_failed("pricing fx gc delete failed", e))?;
    tx.commit()
        .map_err(|e| db_failed("pricing snapshot transaction commit failed", e))?;
    Ok(())
}

/// 只记错误，不改变 active 快照与 fetchedAt。
pub fn mark_pricing_error_conn(conn: &Connection, error: &str) -> Result<(), UsageError> {
    conn.execute(
        "INSERT INTO usage_pricing_state (singleton, enabled, active_snapshot_id, last_error)
         VALUES (1, 0, NULL, ?1)
         ON CONFLICT(singleton) DO UPDATE SET last_error = excluded.last_error",
        params![error],
    )
    .map_err(|e| db_failed("pricing error write failed", e))?;
    Ok(())
}

/// 当前 active 快照的元数据（不含 payload）。无 active 或行缺失 ⇒ None。
///
/// 定价目录缓存命中时只需读这一行，避免每次查询都把 5.5MB payload 拉出 SQLite。
pub fn load_active_snapshot_meta_conn(
    conn: &Connection,
) -> Result<Option<(String, i64)>, UsageError> {
    conn.query_row(
        "SELECT s.id, s.fetched_at
         FROM usage_pricing_state st
         JOIN usage_pricing_snapshot s ON s.id = st.active_snapshot_id
         WHERE st.singleton = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
    .map_err(|e| db_failed("pricing active snapshot meta read failed", e))
}

/// 当前 active 快照（Task 10 复用）。无 active 或行缺失 ⇒ None。
pub fn load_active_snapshot_conn(
    conn: &Connection,
) -> Result<Option<(String, String, i64, String)>, UsageError> {
    conn.query_row(
        "SELECT s.id, s.source_url, s.fetched_at, s.payload_json
         FROM usage_pricing_state st
         JOIN usage_pricing_snapshot s ON s.id = st.active_snapshot_id
         WHERE st.singleton = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )
    .optional()
    .map_err(|e| db_failed("pricing active snapshot read failed", e))
}

// ── 手动覆盖价 DAO（V2 Task 3；2026-10-20 增加币种）──────────────────────────
// 按归一化模型 ID 全局生效；`*_scaled` 与 pricing::parse_rate 同口径
// （10^-9 `currency` / 百万 Token），四价逐项可空，至少一项非空。刷新流程绝不调用本组写函数。

/// 分时规则中的低谷价格（与 `rates` 同序）与时段配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimePriceRuleRecord {
    pub timezone: TimePriceTimezone,
    pub windows: Vec<TimePriceWindow>,
    /// 顺序固定 `[input, cache_read, cache_write, output]`。
    pub offpeak_rates: [Option<i128>; 4],
}

/// 一条手动覆盖价（内存侧用 `i128`，落库时校验并转换为 `i64`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriceOverrideRecord {
    pub normalized_model_id: String,
    pub currency: PriceCurrency,
    /// 顺序固定 `[input, cache_read, cache_write, output]`；分时规则下为高峰价。
    pub rates: [Option<i128>; 4],
    /// `None` = 固定价；`Some` = 分时价。
    pub time_rule: Option<TimePriceRuleRecord>,
    pub updated_at: i64,
}

const OVERRIDE_COMPONENTS: [&str; 4] = ["input", "cacheRead", "cacheWrite", "output"];

fn override_scaled(value: Option<i128>, component: &str) -> Result<Option<i64>, UsageError> {
    match value {
        None => Ok(None),
        Some(value) => i64::try_from(value)
            .ok()
            .filter(|scaled| *scaled >= 0)
            .map(Some)
            .ok_or_else(|| {
                UsageError::new(
                    "usage-settings-invalid",
                    format!("price override rate is invalid: {component}"),
                )
            }),
    }
}

/// 读取全部覆盖价，按归一化模型 ID 升序。
pub fn load_overrides_conn(conn: &Connection) -> Result<Vec<PriceOverrideRecord>, UsageError> {
    let mut stmt = conn
        .prepare(
            "SELECT normalized_model_id, currency, input_scaled, cache_read_scaled,
                    cache_write_scaled, output_scaled,
                    time_enabled, time_timezone, time_windows,
                    offpeak_input_scaled, offpeak_cache_read_scaled,
                    offpeak_cache_write_scaled, offpeak_output_scaled,
                    updated_at
             FROM usage_price_override
             ORDER BY normalized_model_id",
        )
        .map_err(|e| db_failed("price override read failed", e))?;
    let rows = stmt
        .query_map([], |row| {
            let rate = |index: usize| -> rusqlite::Result<Option<i128>> {
                Ok(row.get::<_, Option<i64>>(index)?.map(i128::from))
            };
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                [rate(2)?, rate(3)?, rate(4)?, rate(5)?],
                row.get::<_, i64>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, Option<String>>(8)?,
                [rate(9)?, rate(10)?, rate(11)?, rate(12)?],
                row.get::<_, i64>(13)?,
            ))
        })
        .map_err(|e| db_failed("price override read failed", e))?;
    let mut records = Vec::new();
    for row in rows {
        let (normalized_model_id, currency, rates, enabled, timezone, windows, offpeak, updated_at) =
            row.map_err(|e| db_failed("price override read failed", e))?;
        let time_rule = if enabled != 0 {
            match windows
                .as_deref()
                .ok_or("time_windows is null")
                .and_then(|raw| {
                    serde_json::from_str::<Vec<TimePriceWindow>>(raw).map_err(|_| "invalid JSON")
                }) {
                Ok(windows) => Some(TimePriceRuleRecord {
                    timezone: TimePriceTimezone::parse(&timezone).unwrap_or_default(),
                    windows,
                    offpeak_rates: offpeak,
                }),
                Err(reason) => {
                    // 手工改库等异常不得拖垮整张覆盖表：降级为固定价并留日志。
                    log::warn!(
                        "usage v2: price override {normalized_model_id} has invalid time rule ({reason}); falling back to fixed rates"
                    );
                    None
                }
            }
        } else {
            None
        };
        records.push(PriceOverrideRecord {
            normalized_model_id,
            // CHECK(currency IN ('USD','CNY')) 下不可达；纵深防御：脏值按 USD 读。
            currency: PriceCurrency::parse(&currency).unwrap_or_default(),
            rates,
            time_rule,
            updated_at,
        });
    }
    Ok(records)
}

/// 插入或整体替换一条覆盖价（参数化；未给出的分量写 NULL）。
///
/// `time_rule` 为空 = 固定价（清空分时列）；非空 = 分时价，`rates` 为高峰价。
pub fn upsert_override_conn(
    conn: &Connection,
    normalized_model_id: &str,
    currency: PriceCurrency,
    rates: &[Option<i128>; 4],
    time_rule: Option<&TimePriceRuleRecord>,
    now_ms: i64,
) -> Result<(), UsageError> {
    let scaled = [
        override_scaled(rates[0], OVERRIDE_COMPONENTS[0])?,
        override_scaled(rates[1], OVERRIDE_COMPONENTS[1])?,
        override_scaled(rates[2], OVERRIDE_COMPONENTS[2])?,
        override_scaled(rates[3], OVERRIDE_COMPONENTS[3])?,
    ];
    if scaled.iter().all(Option::is_none) {
        return Err(UsageError::new(
            "usage-settings-invalid",
            "price override must set at least one rate",
        ));
    }
    let (time_enabled, timezone, windows, offpeak) = match time_rule {
        Some(rule) => {
            let offpeak = [
                override_scaled(rule.offpeak_rates[0], OVERRIDE_COMPONENTS[0])?,
                override_scaled(rule.offpeak_rates[1], OVERRIDE_COMPONENTS[1])?,
                override_scaled(rule.offpeak_rates[2], OVERRIDE_COMPONENTS[2])?,
                override_scaled(rule.offpeak_rates[3], OVERRIDE_COMPONENTS[3])?,
            ];
            if offpeak.iter().all(Option::is_none) {
                return Err(UsageError::new(
                    "usage-settings-invalid",
                    "time price rule must set at least one offpeak rate",
                ));
            }
            let windows = serde_json::to_string(&rule.windows)
                .map_err(|e| db_failed("price override time windows encode failed", e))?;
            (1_i64, rule.timezone.as_str(), Some(windows), offpeak)
        }
        None => (
            0_i64,
            TimePriceTimezone::default().as_str(),
            None,
            [None; 4],
        ),
    };
    conn.execute(
        "INSERT INTO usage_price_override
             (normalized_model_id, currency, input_scaled, cache_read_scaled,
              cache_write_scaled, output_scaled,
              time_enabled, time_timezone, time_windows,
              offpeak_input_scaled, offpeak_cache_read_scaled,
              offpeak_cache_write_scaled, offpeak_output_scaled,
              updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
         ON CONFLICT(normalized_model_id) DO UPDATE SET
             currency = excluded.currency,
             input_scaled = excluded.input_scaled,
             cache_read_scaled = excluded.cache_read_scaled,
             cache_write_scaled = excluded.cache_write_scaled,
             output_scaled = excluded.output_scaled,
             time_enabled = excluded.time_enabled,
             time_timezone = excluded.time_timezone,
             time_windows = excluded.time_windows,
             offpeak_input_scaled = excluded.offpeak_input_scaled,
             offpeak_cache_read_scaled = excluded.offpeak_cache_read_scaled,
             offpeak_cache_write_scaled = excluded.offpeak_cache_write_scaled,
             offpeak_output_scaled = excluded.offpeak_output_scaled,
             updated_at = excluded.updated_at",
        params![
            normalized_model_id,
            currency.as_str(),
            scaled[0],
            scaled[1],
            scaled[2],
            scaled[3],
            time_enabled,
            timezone,
            windows,
            offpeak[0],
            offpeak[1],
            offpeak[2],
            offpeak[3],
            now_ms
        ],
    )
    .map_err(|e| db_failed("price override upsert failed", e))?;
    Ok(())
}

/// 删除一条覆盖价；不存在为幂等空操作。
pub fn delete_override_conn(
    conn: &Connection,
    normalized_model_id: &str,
) -> Result<(), UsageError> {
    conn.execute(
        "DELETE FROM usage_price_override WHERE normalized_model_id = ?1",
        params![normalized_model_id],
    )
    .map_err(|e| db_failed("price override delete failed", e))?;
    Ok(())
}

// ── 原生人民币价 / 当日汇率 DAO（2026-10-20）────────────────────────────────

/// 一条原生人民币价（`*_scaled` 单位 10^-9 CNY / 百万 Token）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativePriceRecord {
    pub provider_id: String,
    pub model_id: String,
    pub normalized_model_id: String,
    pub rates: [Option<i128>; 4],
    pub source_url: String,
    pub fetched_at: i64,
}

/// 一条当日汇率（`rate_scaled` 单位 10^-9 CNY / USD）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FxRecord {
    pub rate_scaled: i128,
    pub fetched_at: i64,
    pub source_url: String,
}

/// 整体替换某快照的原生人民币价（刷新只写本表，不触碰覆盖价）。
pub fn replace_native_prices_conn(
    conn: &mut Connection,
    snapshot_id: &str,
    entries: &[NativePriceRecord],
) -> Result<(), UsageError> {
    let tx = conn
        .transaction()
        .map_err(|e| db_failed("pricing native transaction begin failed", e))?;
    tx.execute(
        "DELETE FROM usage_pricing_native WHERE snapshot_id = ?1",
        params![snapshot_id],
    )
    .map_err(|e| db_failed("pricing native clear failed", e))?;
    for entry in entries {
        let scaled = [
            override_scaled(entry.rates[0], "input")?,
            override_scaled(entry.rates[1], "cacheRead")?,
            override_scaled(entry.rates[2], "cacheWrite")?,
            override_scaled(entry.rates[3], "output")?,
        ];
        if scaled.iter().all(Option::is_none) {
            continue;
        }
        tx.execute(
            "INSERT INTO usage_pricing_native
                 (snapshot_id, provider_id, model_id, normalized_model_id, currency,
                  input_scaled, cache_read_scaled, cache_write_scaled, output_scaled,
                  source_url, fetched_at)
             VALUES (?1, ?2, ?3, ?4, 'CNY', ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                snapshot_id,
                entry.provider_id,
                entry.model_id,
                entry.normalized_model_id,
                scaled[0],
                scaled[1],
                scaled[2],
                scaled[3],
                entry.source_url,
                entry.fetched_at
            ],
        )
        .map_err(|e| db_failed("pricing native insert failed", e))?;
    }
    tx.commit()
        .map_err(|e| db_failed("pricing native transaction commit failed", e))?;
    Ok(())
}

/// 读取某快照的全部原生人民币价。
pub fn load_native_prices_conn(
    conn: &Connection,
    snapshot_id: &str,
) -> Result<Vec<NativePriceRecord>, UsageError> {
    let mut stmt = conn
        .prepare(
            "SELECT provider_id, model_id, normalized_model_id,
                    input_scaled, cache_read_scaled, cache_write_scaled, output_scaled,
                    source_url, fetched_at
             FROM usage_pricing_native
             WHERE snapshot_id = ?1
             ORDER BY provider_id, model_id",
        )
        .map_err(|e| db_failed("pricing native read failed", e))?;
    let rows = stmt
        .query_map(params![snapshot_id], |row| {
            let rate = |index: usize| -> rusqlite::Result<Option<i128>> {
                Ok(row.get::<_, Option<i64>>(index)?.map(i128::from))
            };
            Ok(NativePriceRecord {
                provider_id: row.get(0)?,
                model_id: row.get(1)?,
                normalized_model_id: row.get(2)?,
                rates: [rate(3)?, rate(4)?, rate(5)?, rate(6)?],
                source_url: row.get(7)?,
                fetched_at: row.get(8)?,
            })
        })
        .map_err(|e| db_failed("pricing native read failed", e))?;
    let mut records = Vec::new();
    for row in rows {
        records.push(row.map_err(|e| db_failed("pricing native read failed", e))?);
    }
    Ok(records)
}

/// 写入某快照的当日汇率（同快照重复写入为幂等替换）。
pub fn upsert_fx_conn(
    conn: &Connection,
    snapshot_id: &str,
    rate_scaled: i128,
    fetched_at: i64,
    source_url: &str,
) -> Result<(), UsageError> {
    let rate = override_scaled(Some(rate_scaled), "fx")?
        .ok_or_else(|| db_failed("pricing fx write failed", "missing rate"))?;
    conn.execute(
        "INSERT INTO usage_pricing_fx (snapshot_id, rate_scaled, fetched_at, source_url)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(snapshot_id) DO UPDATE SET
             rate_scaled = excluded.rate_scaled,
             fetched_at = excluded.fetched_at,
             source_url = excluded.source_url",
        params![snapshot_id, rate, fetched_at, source_url],
    )
    .map_err(|e| db_failed("pricing fx write failed", e))?;
    Ok(())
}

/// 读取某快照的当日汇率。
pub fn load_fx_conn(conn: &Connection, snapshot_id: &str) -> Result<Option<FxRecord>, UsageError> {
    conn.query_row(
        "SELECT rate_scaled, fetched_at, source_url
         FROM usage_pricing_fx WHERE snapshot_id = ?1",
        params![snapshot_id],
        |row| {
            Ok(FxRecord {
                rate_scaled: i128::from(row.get::<_, i64>(0)?),
                fetched_at: row.get(1)?,
                source_url: row.get(2)?,
            })
        },
    )
    .optional()
    .map_err(|e| db_failed("pricing fx read failed", e))
}

/// 最近一次成功取得的汇率（active 快照缺失时回退）。
pub fn load_latest_fx_conn(conn: &Connection) -> Result<Option<FxRecord>, UsageError> {
    conn.query_row(
        "SELECT rate_scaled, fetched_at, source_url
         FROM usage_pricing_fx ORDER BY fetched_at DESC LIMIT 1",
        [],
        |row| {
            Ok(FxRecord {
                rate_scaled: i128::from(row.get::<_, i64>(0)?),
                fetched_at: row.get(1)?,
                source_url: row.get(2)?,
            })
        },
    )
    .optional()
    .map_err(|e| db_failed("pricing fx latest read failed", e))
}
