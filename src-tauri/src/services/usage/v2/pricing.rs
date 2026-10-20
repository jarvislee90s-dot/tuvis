//! 公开价格目录的严格解析与快照刷新（Task 7/8）。
//!
//! * 键 = **provider 外键 + model 外键**，绝不使用 `model.id` / canonical 别名回退；
//! * `cost` 缺省允许（rates 全 `None`），存在但结构非法 ⇒ 整个目录失败；
//! * 已知费率必须是 JSON Number，经 `Number::to_string()` 后按十进制文本解析，
//!   禁止先转 f64；负号 / 指数 / >9 位小数 / 溢出 ⇒ 目录失败；
//! * 未知 `cost` 键或无法确认纯文本模态 ⇒ `conditional=true`，但保留已解析费率，
//!   下游不得据此宣称完整估价；
//! * 快照只落在 tuvis.db；刷新失败保留旧 active、只记 `last_error`。网络下载期间**不持 DB 锁**。

use std::collections::BTreeMap;
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::Connection;
use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::database::connection::DB;
use crate::database::dao::usage_billing as dao;
use crate::services::usage::v2::model::{
    PriceOverrideRequest, PriceOverrideView, PricingAutoMatch, PricingCatalogEntry,
    PricingCatalogSearch, PricingQuotePreview, PricingReference, PricingStatus,
};
use crate::services::usage::UsageError;

const RATE_KEYS: [&str; 4] = ["input", "cache_read", "cache_write", "output"];

/// 固定价格目录地址（无 query/body/授权头）。
pub const PRICING_SOURCE_URL: &str = "https://models.dev/api.json";
/// 响应体上限：20 MiB（models.dev 当前约 5.8MB，留足增长余量）。
const MAX_PRICING_BYTES: usize = 20 * 1024 * 1024;
/// 连接超时：只覆盖建连，不限制整体下载。
const PRICING_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// 整体超时：models.dev/api.json 实测 5,816,090 字节 / 24.05s，
/// 旧的 20s 总超时在真实网络下必然失败（手工测试回归）。
const PRICING_TOTAL_TIMEOUT: Duration = Duration::from_secs(120);
const REFRESH_LOOP_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// 目录搜索的 limit 默认值与硬上限（IPC 层可传任意 u32，服务层负责 clamp）。
pub const CATALOG_SEARCH_DEFAULT_LIMIT: usize = 50;
pub const CATALOG_SEARCH_MAX_LIMIT: usize = 200;

/// 精选**直营官方** provider（2026-10-20 用户裁决：直营官方优先于聚合/托管来源）。
/// 含中国站官方变体（`-cn`）；订阅计划（如 `zhipuai-coding-plan`）保留，零价表示套餐内，
/// 由去重链与按量价竞争。
pub const CURATED_OFFICIAL_PROVIDERS: [&str; 14] = [
    "openai",
    "anthropic",
    "google",
    "deepseek",
    "zhipuai",
    "zhipuai-coding-plan",
    "moonshotai",
    "moonshotai-cn",
    "alibaba",
    "alibaba-cn",
    "minimax-cn",
    "xai",
    "mistral",
    "meta",
];
/// 托管/聚合来源（Ollama Cloud）：直营官方没有候选时才落到这里，仍优先于 OpenRouter。
pub const CURATED_HOSTED_PROVIDERS: [&str; 1] = ["ollama-cloud"];
/// 精选兜底来源（直营官方/托管都没有候选时自动匹配才落到这里）。
pub const CURATED_OPENROUTER_PROVIDER: &str = "openrouter";

/// 精选分类：直营官方 → `"official"`，Ollama Cloud → `"hosted"`，
/// OpenRouter → `"openrouter"`，其余 → `None`。
pub fn curated_kind_of(provider_id: &str) -> Option<&'static str> {
    if CURATED_OFFICIAL_PROVIDERS.contains(&provider_id) {
        Some("official")
    } else if CURATED_HOSTED_PROVIDERS.contains(&provider_id) {
        Some("hosted")
    } else if provider_id == CURATED_OPENROUTER_PROVIDER {
        Some("openrouter")
    } else {
        None
    }
}

/// 该 provider 是否精选（直营官方（含中国站变体） ∪ OpenRouter）；原文精确比较、大小写敏感。
pub fn is_curated(provider_id: &str) -> bool {
    curated_kind_of(provider_id).is_some()
}

/// 自动匹配 / 目录搜索的作用域（2026-10-20 计划 §3.2）。
///
/// * `Curated`：只在直营官方（含中国站变体）+ Ollama Cloud + OpenRouter 的解析期索引内匹配（自动匹配**恒定**此作用域）；
/// * `All`：全量目录索引（「显示全部 N 个来源」的目录搜索对照口径）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchScope {
    Curated,
    All,
}

impl MatchScope {
    /// IPC 字符串 → 作用域；非法值固定 detail `pricing catalog scope is invalid`。
    pub fn parse(raw: &str) -> Result<Self, UsageError> {
        match raw {
            "curated" => Ok(Self::Curated),
            "all" => Ok(Self::All),
            _ => Err(UsageError::new(
                "usage-settings-invalid",
                "pricing catalog scope is invalid",
            )),
        }
    }

    fn index<'a>(&self, catalog: &'a PriceCatalog) -> &'a BTreeMap<String, NormalizedQuote> {
        match self {
            Self::Curated => &catalog.normalized_curated,
            Self::All => &catalog.normalized,
        }
    }
}

/// UI 状态用：是否有刷新在飞。
pub static REFRESHING: AtomicBool = AtomicBool::new(false);

/// 全局单飞：`refresh_with_conn` / `refresh` 串行化（第二个调用拿到锁后重读状态）。
static REFRESH_LOCK: Mutex<()> = Mutex::new(());

/// 进程级目录缓存：只保留最新 active 快照的解析结果。
///
/// 快照 id 含内容哈希，内容不可变 ⇒ 按 id 缓存是安全的。费用查询、绑定校验都走
/// `load_active_catalog_with_conn`，命中时只读快照元数据，不读 5.5MB payload，
/// 也不重复解析 9000+ 模型。
static CATALOG_CACHE: Mutex<CatalogCache> = Mutex::new(CatalogCache::new());

struct CatalogCache {
    entry: Option<(String, Arc<PriceCatalog>)>,
}

impl CatalogCache {
    const fn new() -> Self {
        Self { entry: None }
    }

    fn get(&self, snapshot_id: &str) -> Option<Arc<PriceCatalog>> {
        self.entry
            .as_ref()
            .filter(|(id, _)| id == snapshot_id)
            .map(|(_, catalog)| Arc::clone(catalog))
    }

    fn put(&mut self, snapshot_id: &str, catalog: Arc<PriceCatalog>) {
        self.entry = Some((snapshot_id.to_string(), catalog));
    }
}

fn cached_catalog(snapshot_id: &str) -> Option<Arc<PriceCatalog>> {
    CATALOG_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(snapshot_id)
}

fn cache_catalog(snapshot_id: &str, catalog: Arc<PriceCatalog>) {
    CATALOG_CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .put(snapshot_id, catalog);
}

fn lock_flight() -> std::sync::MutexGuard<'static, ()> {
    REFRESH_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct RefreshingFlag;

impl RefreshingFlag {
    fn enter() -> Self {
        REFRESHING.store(true, Ordering::SeqCst);
        Self
    }
}

impl Drop for RefreshingFlag {
    fn drop(&mut self) {
        REFRESHING.store(false, Ordering::SeqCst);
    }
}

fn db_failed(context: &str, error: impl std::fmt::Display) -> UsageError {
    UsageError::internal(format!("{context}: {error}"))
}

fn snapshot_id(payload: &[u8], fetched_at: i64) -> String {
    let digest = Sha256::digest(payload);
    let mut hash = String::with_capacity(64);
    for byte in digest {
        hash.push_str(&format!("{byte:02x}"));
    }
    format!("{hash}:{fetched_at}")
}

pub fn status_with_conn(conn: &Connection, now_ms: i64) -> Result<PricingStatus, UsageError> {
    dao::pricing_status_conn(conn, now_ms)
}

/// 生产入口：短锁读取定价状态（命令层不取锁）。
pub fn status(now_ms: i64) -> Result<PricingStatus, UsageError> {
    let conn = DB
        .lock()
        .map_err(|_| UsageError::new("usage-db-failed", "db poisoned lock"))?;
    status_with_conn(&conn, now_ms)
}

/// 当前 active 快照 + 解析后的目录（Task 10 复用）。无 active ⇒ None。
///
/// 命中进程级缓存时只查询快照元数据；未命中才读取 payload 并解析。缓存按 snapshot id
/// 失效（id 含内容哈希，内容不可变），刷新成功后会把已解析结果预热进缓存。
pub fn load_active_catalog_with_conn(
    conn: &Connection,
) -> Result<Option<(String, i64, Arc<PriceCatalog>)>, UsageError> {
    let Some((id, fetched_at)) = dao::load_active_snapshot_meta_conn(conn)? else {
        return Ok(None);
    };
    if let Some(catalog) = cached_catalog(&id) {
        return Ok(Some((id, fetched_at, catalog)));
    }
    // 元数据与完整快照之间可能发生 active 切换：以完整快照行为准。
    let Some((id, _url, fetched_at, payload)) = dao::load_active_snapshot_conn(conn)? else {
        return Ok(None);
    };
    let catalog = Arc::new(parse_catalog(payload.as_bytes())?);
    cache_catalog(&id, Arc::clone(&catalog));
    Ok(Some((id, fetched_at, catalog)))
}

pub fn set_enabled_with_conn(conn: &Connection, enabled: bool) -> Result<(), UsageError> {
    dao::set_pricing_enabled_conn(conn, enabled)
}

/// 在 active 目录里搜索 `(provider, model)`（大小写不敏感子串）。
///
/// * 无 active 快照 ⇒ 空数组（不是错误；前端另用 `usage_pricing_get` 区分“无快照”）；
/// * `query` trim 后为空 ⇒ 返回全部（按 provider/model 字典序，`quotes` 本身是 BTreeMap）；
/// * `limit` clamp 到 `1..=200`（调用方负责默认值）；
/// * `has_rates` = 至少一个费率存在，`conditional` 沿用目录解析结果。
pub fn catalog_entries_with_conn(
    conn: &Connection,
    query: Option<&str>,
    limit: usize,
) -> Result<Vec<PricingCatalogEntry>, UsageError> {
    let limit = limit.clamp(1, CATALOG_SEARCH_MAX_LIMIT);
    let Some((_, _, catalog)) = load_active_catalog_with_conn(conn)? else {
        return Ok(Vec::new());
    };
    let needle = query.unwrap_or("").trim().to_lowercase();
    let mut entries = Vec::new();
    for quote in catalog.quotes.values() {
        if !needle.is_empty() {
            let provider = quote.reference.provider_id.to_lowercase();
            let model = quote.reference.model_id.to_lowercase();
            if !provider.contains(&needle) && !model.contains(&needle) {
                continue;
            }
        }
        entries.push(PricingCatalogEntry {
            provider_id: quote.reference.provider_id.clone(),
            model_id: quote.reference.model_id.clone(),
            release_date: quote.release_date.clone(),
            has_rates: quote.rates.iter().any(Option::is_some),
            conditional: quote.conditional,
            // 全量口径（高级覆盖参考价选择器在用）不做精选分类；T3 的作用域搜索
            // （`catalog_search_scoped_with_conn`）才会填 official / hosted / openrouter。
            curated_kind: None,
        });
        if entries.len() >= limit {
            break;
        }
    }
    Ok(entries)
}

/// 作用域目录搜索（2026-10-20 计划 §3.2）：配置弹窗 B 段 row3 的选择器入口。
///
/// * `curated`：只返回直营官方（含中国站变体）+ Ollama Cloud + OpenRouter 的条目（`curatedKind` 相应填写）；
///   `all`：现全量行为（非精选条目 `curatedKind = null`）；
/// * `provider_count` 恒为**全量目录** provider 数（「显示全部 N 个来源」的 N）；
/// * 无 active 快照 ⇒ 空列表 + `provider_count = 0`（不是错误）；
/// * 子串过滤 / 排序 / limit clamp 与 [`catalog_entries_with_conn`] 同规则。
pub fn catalog_search_scoped_with_conn(
    conn: &Connection,
    query: Option<&str>,
    limit: usize,
    scope: MatchScope,
) -> Result<PricingCatalogSearch, UsageError> {
    let limit = limit.clamp(1, CATALOG_SEARCH_MAX_LIMIT);
    let Some((_, _, catalog)) = load_active_catalog_with_conn(conn)? else {
        return Ok(PricingCatalogSearch {
            entries: Vec::new(),
            provider_count: 0,
        });
    };
    let needle = query.unwrap_or("").trim().to_lowercase();
    let mut entries = Vec::new();
    for quote in catalog.quotes.values() {
        if !is_curated(&quote.reference.provider_id) && scope == MatchScope::Curated {
            continue;
        }
        if !needle.is_empty() {
            let provider = quote.reference.provider_id.to_lowercase();
            let model = quote.reference.model_id.to_lowercase();
            if !provider.contains(&needle) && !model.contains(&needle) {
                continue;
            }
        }
        entries.push(PricingCatalogEntry {
            provider_id: quote.reference.provider_id.clone(),
            model_id: quote.reference.model_id.clone(),
            release_date: quote.release_date.clone(),
            has_rates: quote.rates.iter().any(Option::is_some),
            conditional: quote.conditional,
            curated_kind: curated_kind_of(&quote.reference.provider_id).map(str::to_string),
        });
        if entries.len() >= limit {
            break;
        }
    }
    Ok(PricingCatalogSearch {
        entries,
        provider_count: catalog.provider_count as i64,
    })
}

/// 目录条目价格预览（B 段 row3 悬停价格表）：精确键查 `quotes`，无条目 / 无快照 ⇒ `None`。
pub fn quote_preview_with_conn(
    conn: &Connection,
    provider_id: &str,
    model_id: &str,
    fallback_rate: Option<&str>,
) -> Result<Option<PricingQuotePreview>, UsageError> {
    let Some((snapshot_id, fetched_at, catalog)) = load_active_catalog_with_conn(conn)? else {
        return Ok(None);
    };
    let Some(quote) = catalog
        .quotes
        .get(&(provider_id.to_string(), model_id.to_string()))
    else {
        return Ok(None);
    };
    let native = crate::services::usage::v2::currency::NativePrices::load_with_conn(
        conn,
        Some(&snapshot_id),
    )?;
    let fx = crate::services::usage::v2::currency::resolve_fx_with_conn(
        conn,
        Some(&snapshot_id),
        fallback_rate,
    )?;
    Ok(Some(preview_of(
        quote.reference.clone(),
        quote.rates,
        quote.conditional,
        quote.release_date.clone(),
        &quote.normalized_model_id,
        &native,
        &fx,
        Some(fetched_at),
    )?))
}

/// 自动匹配预览（**固定精选作用域**：直营官方优先、Ollama Cloud、OpenRouter 兜底）。
pub fn auto_match_with_conn(
    conn: &Connection,
    model_id: &str,
    fallback_rate: Option<&str>,
) -> Result<PricingAutoMatch, UsageError> {
    let active = load_active_catalog_with_conn(conn)?;
    let empty_index: BTreeMap<String, NormalizedQuote> = BTreeMap::new();
    let (catalog, snapshot_id, fetched_at): (
        Option<std::sync::Arc<PriceCatalog>>,
        Option<String>,
        Option<i64>,
    ) = match active {
        Some((id, fetched_at, catalog)) => (Some(catalog), Some(id), Some(fetched_at)),
        None => (None, None, None),
    };
    // 无 active 快照：与「目录为空」同语义（kind=none），不报错。
    let matched = match &catalog {
        Some(catalog) => match_model_scoped(catalog, model_id, MatchScope::Curated),
        None => match_in_index(&empty_index, model_id),
    };
    let kind = match matched.kind {
        ModelMatchKind::Exact => "exact",
        ModelMatchKind::Prefix => "prefix",
        ModelMatchKind::None => "none",
    };
    let native = crate::services::usage::v2::currency::NativePrices::load_with_conn(
        conn,
        snapshot_id.as_deref(),
    )?;
    let fx = crate::services::usage::v2::currency::resolve_fx_with_conn(
        conn,
        snapshot_id.as_deref(),
        fallback_rate,
    )?;
    let preview = match matched.quote {
        Some(quote) => Some(preview_of(
            quote.reference.clone(),
            quote.rates,
            quote.conditional,
            quote.release_date.clone(),
            &quote.normalized_model_id,
            &native,
            &fx,
            fetched_at,
        )?),
        None => None,
    };
    Ok(PricingAutoMatch {
        kind: kind.to_string(),
        reference: matched.quote.map(|quote| quote.reference.clone()),
        quote: preview,
    })
}

/// 组装预览：USD 用目录原生；CNY 优先原生人民币价，否则按当日汇率换算并标 converted。
#[allow(clippy::too_many_arguments)]
fn preview_of(
    reference: PricingReference,
    rates: [Option<i128>; 4],
    conditional: bool,
    release_date: Option<String>,
    normalized_model_id: &str,
    native: &crate::services::usage::v2::currency::NativePrices,
    fx: &crate::services::usage::v2::currency::FxRate,
    snapshot_date: Option<i64>,
) -> Result<PricingQuotePreview, UsageError> {
    let rate = |value: Option<i128>| value.map(rate_string);
    let native_cny = native.rates_for(normalized_model_id, &reference.provider_id);
    let (cny_rates, cny_converted) = match native_cny {
        Some(rates) => (rates, false),
        None => (
            crate::services::usage::v2::currency::convert_rates(
                rates,
                fx,
                crate::services::usage::v2::model::PriceCurrency::Usd,
                crate::services::usage::v2::model::PriceCurrency::Cny,
            )?,
            // 没有任何 USD 费率时没有可换算之物，不宣称 converted。
            rates.iter().any(Option::is_some),
        ),
    };
    Ok(PricingQuotePreview {
        reference,
        input: rate(rates[0]),
        cache_read: rate(rates[1]),
        cache_write: rate(rates[2]),
        output: rate(rates[3]),
        input_cny: rate(cny_rates[0]),
        cache_read_cny: rate(cny_rates[1]),
        cache_write_cny: rate(cny_rates[2]),
        output_cny: rate(cny_rates[3]),
        cny_converted,
        release_date,
        conditional,
        snapshot_date,
        fx_rate: Some(fx.rate_text.clone()),
        fx_fetched_at: fx.fetched_at,
        fx_fallback: fx.fallback,
    })
}

/// 刷新内核的宿主抽象：把「状态读写在哪个连接上」与「结果是否通知 UI」注入进来，
/// 让测试内核（调用方持连接、不 emit）与生产入口（DB 全局锁分段 + emit）共享同一条
/// 状态检查→fetch→parse→UTF-8→activate→缓存预热→状态回读 的顺序与条件。
trait RefreshHost {
    /// 读取当前定价状态（进入单飞后的第一次检查）。
    fn read_status(&self, now_ms: i64) -> Result<PricingStatus, UsageError>;
    /// 只记错误、不改 active 与 fetchedAt，然后回读状态（fetch/parse/UTF-8 失败共用）。
    fn record_error_and_status(
        &mut self,
        error: &UsageError,
        now_ms: i64,
    ) -> Result<PricingStatus, UsageError>;
    /// 原子写入新快照并切换 active（含快照 GC）。
    fn activate(&mut self, id: &str, payload: &str, now_ms: i64) -> Result<(), UsageError>;
    /// 激活后的增强步骤（原生人民币价 + 当日汇率）。
    ///
    /// 默认 no-op：测试内核不访问网络；生产宿主 best-effort 拉取并落库，
    /// 失败只记日志，不影响已激活的 USD 快照。必须在 `final_status`/emit 之前完成，
    /// 这样 UI 第一次回读就能看到汇率与人民币价。
    fn enrich(
        &mut self,
        _id: &str,
        _catalog: &PriceCatalog,
        _now_ms: i64,
    ) -> Result<(), UsageError> {
        Ok(())
    }
    /// 成功路径的最终状态回读。
    fn final_status(&self, now_ms: i64) -> Result<PricingStatus, UsageError>;
    /// 刷新结束（成功或失败）通知 UI；测试内核注入 no-op。
    fn emit_updated(&self);
}

/// 共享刷新内核（单飞）：顺序与条件固定为
/// 状态检查 → fetch（RefreshingFlag 在飞）→ parse → UTF-8 → activate → 缓存预热 → 状态回读。
/// fetch / parse 失败只写 `last_error`，保留旧 active 与 fetchedAt，返回回读的状态。
fn run_refresh(
    host: &mut impl RefreshHost,
    force: bool,
    now_ms: i64,
    fetch: impl FnOnce() -> Result<Vec<u8>, UsageError>,
) -> Result<PricingStatus, UsageError> {
    let _flight = lock_flight();
    let status = host.read_status(now_ms)?;
    if !status.enabled || (!force && !status.stale) {
        return Ok(status);
    }

    let fetched = {
        let _refreshing = RefreshingFlag::enter();
        fetch()
    };
    let bytes = match fetched {
        Ok(bytes) => bytes,
        Err(error) => {
            let status = host.record_error_and_status(&error, now_ms)?;
            host.emit_updated();
            return Ok(status);
        }
    };
    let catalog = match parse_catalog(&bytes) {
        Ok(catalog) => Arc::new(catalog),
        Err(error) => {
            let status = host.record_error_and_status(&error, now_ms)?;
            host.emit_updated();
            return Ok(status);
        }
    };
    let Ok(payload) = std::str::from_utf8(&bytes) else {
        let error = UsageError::internal("pricing catalog: payload is not UTF-8".to_string());
        let status = host.record_error_and_status(&error, now_ms)?;
        host.emit_updated();
        return Ok(status);
    };
    let id = snapshot_id(&bytes, now_ms);
    host.activate(&id, payload, now_ms)?;
    cache_catalog(&id, catalog.clone());
    if let Err(error) = host.enrich(&id, &catalog, now_ms) {
        // 增强步骤失败不推翻已激活的 USD 快照；下次刷新/手动同步会重试。
        log::warn!("pricing enrich failed: {error:?}");
    }
    let status = host.final_status(now_ms)?;
    host.emit_updated();
    Ok(status)
}

/// 测试宿主：调用方持有连接（不取全局 DB 锁），不 emit UI 事件。
struct ConnHost<'a> {
    conn: &'a mut Connection,
}

impl RefreshHost for ConnHost<'_> {
    fn read_status(&self, now_ms: i64) -> Result<PricingStatus, UsageError> {
        dao::pricing_status_conn(self.conn, now_ms)
    }

    fn record_error_and_status(
        &mut self,
        error: &UsageError,
        now_ms: i64,
    ) -> Result<PricingStatus, UsageError> {
        dao::mark_pricing_error_conn(self.conn, &error.detail)?;
        dao::pricing_status_conn(self.conn, now_ms)
    }

    fn activate(&mut self, id: &str, payload: &str, now_ms: i64) -> Result<(), UsageError> {
        dao::activate_snapshot_conn(&mut *self.conn, id, PRICING_SOURCE_URL, now_ms, payload)
    }

    fn final_status(&self, now_ms: i64) -> Result<PricingStatus, UsageError> {
        dao::pricing_status_conn(self.conn, now_ms)
    }

    fn emit_updated(&self) {}
}

/// 可测试刷新内核：单飞锁 + 注入 fetch，不访问网络、不 emit。
///
/// * 未启用 ⇒ 不请求；
/// * `force=false` 且 active 新鲜、无 `last_error` ⇒ 不请求；
/// * fetch / parse 失败 ⇒ 只写 `last_error`，保留旧 active 与 fetchedAt，返回状态。
pub fn refresh_with_conn(
    conn: &mut Connection,
    now_ms: i64,
    force: bool,
    fetch: impl FnOnce() -> Result<Vec<u8>, UsageError>,
) -> Result<PricingStatus, UsageError> {
    run_refresh(&mut ConnHost { conn }, force, now_ms, fetch)
}

fn download_catalog() -> Result<Vec<u8>, UsageError> {
    let client = reqwest::blocking::Client::builder()
        // connect_timeout 只限制建连；总超时必须容得下 5.8MB 慢速下载。
        .connect_timeout(PRICING_CONNECT_TIMEOUT)
        .timeout(PRICING_TOTAL_TIMEOUT)
        .build()
        .map_err(|e| db_failed("pricing download: client build failed", e))?;
    let mut response = client
        .get(PRICING_SOURCE_URL)
        .send()
        .map_err(|e| db_failed("pricing download failed", e))?;
    let status = response.status();
    if !status.is_success() {
        return Err(db_failed(
            "pricing download failed",
            format!("HTTP {status}"),
        ));
    }
    let mut payload = Vec::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = response
            .read(&mut buffer)
            .map_err(|e| db_failed("pricing download read failed", e))?;
        if read == 0 {
            break;
        }
        payload.extend_from_slice(&buffer[..read]);
        ensure_payload_within_limit(payload.len())?;
    }
    Ok(payload)
}

/// 响应体上限判据与文案同处一处：改上限必须同步改错误文案（测试逐字锁住）。
fn ensure_payload_within_limit(length: usize) -> Result<(), UsageError> {
    if length > MAX_PRICING_BYTES {
        return Err(UsageError::internal(
            "pricing download: response exceeds 20 MiB".to_string(),
        ));
    }
    Ok(())
}

fn record_error_and_status(error: &UsageError, now_ms: i64) -> Result<PricingStatus, UsageError> {
    let conn = DB
        .lock()
        .map_err(|_| UsageError::new("usage-db-failed", "db poisoned lock"))?;
    dao::mark_pricing_error_conn(&conn, &error.detail)?;
    dao::pricing_status_conn(&conn, now_ms)
}

fn emit_pricing_updated() {
    crate::remote::events::emit_ui("usage-pricing-updated", ());
}

/// 生产宿主：DB 全局锁**分段**持有（下载/解析期间不持锁），刷新结束 emit UI 事件。
struct ProductionHost;

impl RefreshHost for ProductionHost {
    fn read_status(&self, now_ms: i64) -> Result<PricingStatus, UsageError> {
        let conn = DB
            .lock()
            .map_err(|_| UsageError::new("usage-db-failed", "db poisoned lock"))?;
        dao::pricing_status_conn(&conn, now_ms)
    }

    fn record_error_and_status(
        &mut self,
        error: &UsageError,
        now_ms: i64,
    ) -> Result<PricingStatus, UsageError> {
        record_error_and_status(error, now_ms)
    }

    fn activate(&mut self, id: &str, payload: &str, now_ms: i64) -> Result<(), UsageError> {
        let mut conn = DB
            .lock()
            .map_err(|_| UsageError::new("usage-db-failed", "db poisoned lock"))?;
        dao::activate_snapshot_conn(&mut conn, id, PRICING_SOURCE_URL, now_ms, payload)
    }

    fn enrich(&mut self, id: &str, catalog: &PriceCatalog, now_ms: i64) -> Result<(), UsageError> {
        // 当日汇率：下载不持 DB 锁；落库失败只记日志（USD 快照已激活）。
        match crate::services::usage::v2::native_price::download_fx(now_ms) {
            Ok(fx) => {
                let conn = DB
                    .lock()
                    .map_err(|_| UsageError::new("usage-db-failed", "db poisoned lock"))?;
                if let Err(error) =
                    dao::upsert_fx_conn(&conn, id, fx.rate_scaled, fx.fetched_at, &fx.source_url)
                {
                    log::warn!("pricing fx store failed: {error:?}");
                }
            }
            Err(error) => log::warn!("pricing fx unavailable: {error:?}"),
        }

        // 原生人民币价：先在短锁内选目标，再无锁下载，最后短锁整体替换。
        let targets = {
            let conn = DB
                .lock()
                .map_err(|_| UsageError::new("usage-db-failed", "db poisoned lock"))?;
            crate::services::usage::v2::native_price::native_targets_with_conn(
                &conn, catalog, now_ms,
            )?
        };
        let records = match crate::services::usage::v2::native_price::download_native_prices(
            &targets, now_ms,
        ) {
            Ok(records) => records,
            Err(error) => {
                log::warn!("pricing native download failed: {error:?}");
                return Ok(());
            }
        };
        let mut conn = DB
            .lock()
            .map_err(|_| UsageError::new("usage-db-failed", "db poisoned lock"))?;
        if let Err(error) = dao::replace_native_prices_conn(&mut conn, id, &records) {
            log::warn!("pricing native store failed: {error:?}");
        }
        Ok(())
    }

    fn final_status(&self, now_ms: i64) -> Result<PricingStatus, UsageError> {
        let conn = DB
            .lock()
            .map_err(|_| UsageError::new("usage-db-failed", "db poisoned lock"))?;
        dao::pricing_status_conn(&conn, now_ms)
    }

    fn emit_updated(&self) {
        emit_pricing_updated();
    }
}

/// 生产刷新入口：单飞；短锁读状态 → 无锁下载/解析 → 短锁切换 active。
pub fn refresh(force: bool, now_ms: i64) -> Result<PricingStatus, UsageError> {
    run_refresh(&mut ProductionHost, force, now_ms, download_catalog)
}

/// 写开关；开启时后台立即触发一次刷新（首次启用），关闭后不自动请求。
pub fn set_enabled(enabled: bool) -> Result<PricingStatus, UsageError> {
    let now_ms = crate::services::usage::now_ms();
    {
        let conn = DB
            .lock()
            .map_err(|_| UsageError::new("usage-db-failed", "db poisoned lock"))?;
        set_enabled_with_conn(&conn, enabled)?;
    }
    if enabled {
        std::thread::spawn(|| {
            if let Err(error) = refresh(false, crate::services::usage::now_ms()) {
                log::warn!("pricing refresh after enable failed: {error:?}");
            }
        });
    }
    let conn = DB
        .lock()
        .map_err(|_| UsageError::new("usage-db-failed", "db poisoned lock"))?;
    status_with_conn(&conn, now_ms)
}

/// 后台循环：每 5 分钟只检查 `enabled && stale`，满足才刷新；启动时未启用不请求。
pub fn spawn_pricing_refresh_loop() {
    std::thread::spawn(|| loop {
        std::thread::sleep(REFRESH_LOOP_INTERVAL);
        let now_ms = crate::services::usage::now_ms();
        let should_refresh = DB
            .lock()
            .ok()
            .and_then(|conn| dao::pricing_status_conn(&conn, now_ms).ok())
            .is_some_and(|status| status.enabled && status.stale);
        if should_refresh {
            if let Err(error) = refresh(false, now_ms) {
                log::warn!("pricing refresh loop failed: {error:?}");
            }
        }
    });
}

#[derive(Debug, Clone)]
pub struct PriceQuote {
    pub reference: PricingReference,
    /// 顺序固定 `[input, cache_read, cache_write, output]`，单位 10^-9 USD / 百万 Token。
    /// `None` = 缺价（**不是 0 价**）。
    pub rates: [Option<i128>; 4],
    /// 存在无法确认的计价条件（未知 cost 键 / 非纯文本模态）。
    pub conditional: bool,
    /// models.dev 的 `release_date` 原文（`YYYY` / `YYYY-MM` / `YYYY-MM-DD`，可能缺失）。
    pub release_date: Option<String>,
    /// 机械归一化后的模型 ID（空模型为空串，永不进入 `normalized` 索引）。
    pub normalized_model_id: String,
}

/// 同一归一化模型 ID 去重后的唯一参考报价。
///
/// 选中的条目即使无价 / 条件价也保留为“来源”，不得用其他 provider 的报价补洞。
#[derive(Debug, Clone)]
pub struct NormalizedQuote {
    pub reference: PricingReference,
    pub rates: [Option<i128>; 4],
    pub conditional: bool,
    pub release_date: Option<String>,
    pub normalized_model_id: String,
}

#[derive(Debug, Clone)]
pub struct PriceCatalog {
    /// 精确键 `(provider, model)`，供用户 `PricingRule` 引用；不做任何别名回退。
    pub quotes: BTreeMap<(String, String), PriceQuote>,
    /// 归一化模型 ID → 去重后的唯一参考报价（同一归一化模型全渠道同价；全量口径）。
    pub normalized: BTreeMap<String, NormalizedQuote>,
    /// **精选索引**（2026-10-20 计划 §3.2）：解析期按「直营官方（含中国站变体）内按既有去重链选一 →
    /// 官方无候选则 OpenRouter 内同规则选一 → 两者皆无不进索引」独立去重。
    /// 运行期过滤会在「最新优先去重已选中非精选条目」时失效，因此必须在解析期建好。
    pub normalized_curated: BTreeMap<String, NormalizedQuote>,
    /// 全量目录 provider 数（models.dev 顶层对象键数；「显示全部 N 个来源」的 N，
    /// 当前实测 ≈232）。与搜索作用域无关，恒为全量口径。
    pub provider_count: usize,
}

/// `match_model` 的命中方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelMatchKind {
    Exact,
    Prefix,
    None,
}

/// 模型匹配结果。
#[derive(Debug, Clone)]
pub struct ModelMatch<'a> {
    pub kind: ModelMatchKind,
    pub quote: Option<&'a NormalizedQuote>,
    pub normalized_query: String,
    /// 前缀候选多个且费率不一致（下游行级 reason = `pricingPrefixAmbiguous`）。
    pub prefix_ambiguous: bool,
}

/// 覆盖价应用后的有效费率。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffectiveRates {
    /// 顺序固定 `[input, cache_read, cache_write, output]`。
    pub rates: [Option<i128>; 4],
    /// 至少一个分量来自用户覆盖价。
    pub overridden: bool,
}

/// 模型 ID 机械归一化（计划 §4.1）。
///
/// 规则顺序：trim + 小写 → 取最后一个 `/` 之后 → `.` / `_` → `-` 并折叠连续 `-`
/// → 循环剥尾（日期 / `preview-MM-DD` / 版本 / 噪声 / 推理档位）。
/// **不剥** `-max` / `-xhigh` / `-ultra` / `-chat` / `-instruct` / `-base`。
/// 空串原样返回；由调用方保证空模型永不参与目录匹配。
pub fn normalize_model_id(raw: &str) -> String {
    let lowered = raw.trim().to_lowercase();
    if lowered.is_empty() {
        return String::new();
    }
    let without_vendor = match lowered.rsplit_once('/') {
        Some((_, tail)) => tail,
        None => lowered.as_str(),
    };
    let mut value = collapse_separators(without_vendor);
    while let Some(stripped) = strip_one_tail(&value) {
        let next = collapse_separators(stripped);
        if next == value {
            break;
        }
        value = next;
        if value.is_empty() {
            break;
        }
    }
    value
}

fn collapse_separators(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        let ch = match ch {
            '.' | '_' => '-',
            other => other,
        };
        if ch == '-' {
            if !out.ends_with('-') {
                out.push('-');
            }
        } else {
            out.push(ch);
        }
    }
    out.trim_matches('-').to_string()
}

fn strip_one_tail(value: &str) -> Option<&str> {
    strip_date_tail(value)
        .or_else(|| strip_preview_date_tail(value))
        .or_else(|| strip_version_tail(value))
        .or_else(|| strip_known_tail(value))
}

fn all_ascii_digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

/// 剥日期尾：`-YYYY-MM-DD`、`-YYYYMMDD`、`-YYYY-MM`。
fn strip_date_tail(value: &str) -> Option<&str> {
    for (index, _) in value.match_indices('-').rev() {
        let suffix = &value[index + 1..];
        if suffix.len() == 8 && all_ascii_digits(suffix) {
            return Some(&value[..index]);
        }
        let parts: Vec<&str> = suffix.split('-').collect();
        match parts.as_slice() {
            [year, month]
                if year.len() == 4
                    && month.len() == 2
                    && all_ascii_digits(year)
                    && all_ascii_digits(month) =>
            {
                return Some(&value[..index]);
            }
            [year, month, day]
                if year.len() == 4
                    && month.len() == 2
                    && day.len() == 2
                    && all_ascii_digits(year)
                    && all_ascii_digits(month)
                    && all_ascii_digits(day) =>
            {
                return Some(&value[..index]);
            }
            _ => {}
        }
    }
    None
}

/// 剥 `-preview-MM-DD` 尾。
fn strip_preview_date_tail(value: &str) -> Option<&str> {
    for (index, _) in value.match_indices('-').rev() {
        let suffix = &value[index + 1..];
        let Some(rest) = suffix.strip_prefix("preview-") else {
            continue;
        };
        let parts: Vec<&str> = rest.split('-').collect();
        if let [month, day] = parts.as_slice() {
            if month.len() == 2
                && day.len() == 2
                && all_ascii_digits(month)
                && all_ascii_digits(day)
            {
                return Some(&value[..index]);
            }
        }
    }
    None
}

/// 剥版本尾：`-vN` 及 `-vN(-M)*`。
fn strip_version_tail(value: &str) -> Option<&str> {
    for (index, _) in value.match_indices('-').rev() {
        let suffix = &value[index + 1..];
        let mut parts = suffix.split('-');
        let Some(first) = parts.next() else {
            continue;
        };
        let Some(digits) = first.strip_prefix('v') else {
            continue;
        };
        if !all_ascii_digits(digits) {
            continue;
        }
        if parts.all(all_ascii_digits) {
            return Some(&value[..index]);
        }
    }
    None
}

/// 噪声 / 推理档位尾。长形态必须排在短形态之前（`-non-thinking` 先于 `-thinking`，
/// `-reasoning-effort-high` 先于 `-effort-high` / `-high`）。
const KNOWN_TAILS: [&str; 20] = [
    "-reasoning-effort-minimal",
    "-reasoning-effort-low",
    "-reasoning-effort-medium",
    "-reasoning-effort-high",
    "-effort-minimal",
    "-effort-low",
    "-effort-medium",
    "-effort-high",
    "-non-thinking",
    "-nothinking",
    "-experimental",
    "-thinking",
    "-reasoning",
    "-preview",
    "-latest",
    "-minimal",
    "-medium",
    "-high",
    "-low",
    "-exp",
];

fn strip_known_tail(value: &str) -> Option<&str> {
    KNOWN_TAILS.iter().find_map(|tail| value.strip_suffix(tail))
}

/// `release_date` → `(year, month, day)`；缺失 / 非法一律 `(0, 0, 0)`。
pub(crate) fn parse_release_date(date: Option<&str>) -> (i64, i64, i64) {
    let Some(date) = date else {
        return (0, 0, 0);
    };
    let number = |part: &str, width: usize| -> Option<i64> {
        if part.len() == width && all_ascii_digits(part) {
            part.parse().ok()
        } else {
            None
        }
    };
    let parts: Vec<&str> = date.split('-').collect();
    match parts.as_slice() {
        [year] => number(year, 4).map_or((0, 0, 0), |year| (year, 0, 0)),
        [year, month] => match (number(year, 4), number(month, 2)) {
            (Some(year), Some(month)) if (1..=12).contains(&month) => (year, month, 0),
            _ => (0, 0, 0),
        },
        [year, month, day] => match (number(year, 4), number(month, 2), number(day, 2)) {
            (Some(year), Some(month), Some(day))
                if (1..=12).contains(&month) && (1..=31).contains(&day) =>
            {
                (year, month, day)
            }
            _ => (0, 0, 0),
        },
        _ => (0, 0, 0),
    }
}

fn normalized_candidate(quote: &PriceQuote) -> NormalizedQuote {
    NormalizedQuote {
        reference: quote.reference.clone(),
        rates: quote.rates,
        conditional: quote.conditional,
        release_date: quote.release_date.clone(),
        normalized_model_id: quote.normalized_model_id.clone(),
    }
}

/// 同一归一化 ID 的去重优先级（计划 §4.2）：
/// release_date 降序 → 有至少一项费率优先 → 非 conditional 优先 → provider/model 字典序升序。
fn candidate_is_better(candidate: &NormalizedQuote, current: &NormalizedQuote) -> bool {
    let candidate_release = parse_release_date(candidate.release_date.as_deref());
    let current_release = parse_release_date(current.release_date.as_deref());
    if candidate_release != current_release {
        return candidate_release > current_release;
    }
    let candidate_priced = candidate.rates.iter().any(Option::is_some);
    let current_priced = current.rates.iter().any(Option::is_some);
    if candidate_priced != current_priced {
        return candidate_priced;
    }
    if candidate.conditional != current.conditional {
        return !candidate.conditional;
    }
    (
        candidate.reference.provider_id.as_str(),
        candidate.reference.model_id.as_str(),
    ) < (
        current.reference.provider_id.as_str(),
        current.reference.model_id.as_str(),
    )
}

/// 前缀回退：剩余后缀必须全部是 `-` 分隔的已知噪声词，或整体是日期形态。
///
/// 注意：**裸纯数字不是噪声**（`gpt-4.1` 不得回退到 `gpt-4`，
/// `claude-2.1` 不得回退到 `claude-2`）；只接受 `v<digits>`、日期形态、
/// 已知噪声词与推理档位。
fn is_noise_suffix(suffix: &str) -> bool {
    let Some(rest) = suffix.strip_prefix('-') else {
        return false;
    };
    !rest.is_empty() && (is_date_noise(rest) || rest.split('-').all(is_noise_token))
}

/// 日期形态噪声：`YYYY`、`YYYY-MM`、`YYYY-MM-DD`、`YYYYMMDD`。
///
/// 月/日范围与 [`parse_release_date`] 对齐（月 `1..=12`、日 `1..=31`），
/// 否则 `2024-13` / `2024-99` 这类版本片段会被误当成日期噪声而触发前缀回退。
fn is_date_noise(value: &str) -> bool {
    if value.len() == 8 && all_ascii_digits(value) {
        return two_digit_between(&value[4..6], 1, 12) && two_digit_between(&value[6..8], 1, 31);
    }
    let parts: Vec<&str> = value.split('-').collect();
    let Some(year) = parts.first() else {
        return false;
    };
    if year.len() != 4 || !all_ascii_digits(year) {
        return false;
    }
    match parts.as_slice() {
        [_] => true,
        [_, month] => two_digit_between(month, 1, 12),
        [_, month, day] => two_digit_between(month, 1, 12) && two_digit_between(day, 1, 31),
        _ => false,
    }
}

/// 两位 ASCII 数字且落在闭区间内（与 `parse_release_date` 的月/日校验同口径）。
fn two_digit_between(part: &str, min: u32, max: u32) -> bool {
    part.len() == 2
        && all_ascii_digits(part)
        && part
            .parse::<u32>()
            .is_ok_and(|value| (min..=max).contains(&value))
}

fn is_noise_token(token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    if matches!(
        token,
        "preview"
            | "latest"
            | "exp"
            | "experimental"
            | "beta"
            | "alpha"
            | "thinking"
            | "reasoning"
            | "high"
            | "medium"
            | "low"
            | "minimal"
    ) {
        return true;
    }
    // 只接受 `v<digits>`；裸数字（如 `-2`、`-4-1`）不是可忽略后缀。
    token.strip_prefix('v').is_some_and(all_ascii_digits)
}

/// 自动匹配（2026-10-20 计划 §3.2）：归一化精确 → 安全前缀回退（唯一 / 全部同价取最长）；
/// 否则未命中。**前缀候选只在所选作用域索引内找**，不做反向匹配、不按相似度、不跨模型族替代。
pub fn match_model_scoped<'a>(
    catalog: &'a PriceCatalog,
    raw_model_id: &str,
    scope: MatchScope,
) -> ModelMatch<'a> {
    match_in_index(scope.index(catalog), raw_model_id)
}

fn match_in_index<'a>(
    index: &'a BTreeMap<String, NormalizedQuote>,
    raw_model_id: &str,
) -> ModelMatch<'a> {
    let normalized_query = normalize_model_id(raw_model_id);
    if normalized_query.is_empty() {
        return ModelMatch {
            kind: ModelMatchKind::None,
            quote: None,
            normalized_query,
            prefix_ambiguous: false,
        };
    }
    if let Some(quote) = index.get(&normalized_query) {
        return ModelMatch {
            kind: ModelMatchKind::Exact,
            quote: Some(quote),
            normalized_query,
            prefix_ambiguous: false,
        };
    }

    let mut candidates: Vec<&NormalizedQuote> = Vec::new();
    for (key, quote) in index {
        if key.len() < 4 || !normalized_query.starts_with(key.as_str()) {
            continue;
        }
        let suffix = &normalized_query[key.len()..];
        if !suffix.is_empty() && is_noise_suffix(suffix) {
            candidates.push(quote);
        }
    }

    match candidates.as_slice() {
        [] => ModelMatch {
            kind: ModelMatchKind::None,
            quote: None,
            normalized_query,
            prefix_ambiguous: false,
        },
        [quote] => ModelMatch {
            kind: ModelMatchKind::Prefix,
            quote: Some(quote),
            normalized_query,
            prefix_ambiguous: false,
        },
        _ => {
            let first_rates = candidates[0].rates;
            // 同价判定必须含 conditional：费率不同 → 无法安全选择，保持未命中。
            if candidates.iter().any(|quote| quote.rates != first_rates) {
                return ModelMatch {
                    kind: ModelMatchKind::None,
                    quote: None,
                    normalized_query,
                    prefix_ambiguous: true,
                };
            }
            // 同费率优先非条件价，再在同条件下取最长（字典序打破平局）。
            let preferred: Vec<&NormalizedQuote> =
                if candidates.iter().any(|quote| !quote.conditional) {
                    candidates
                        .iter()
                        .copied()
                        .filter(|quote| !quote.conditional)
                        .collect()
                } else {
                    candidates.clone()
                };
            let longest = preferred
                .iter()
                .max_by(|left, right| {
                    left.normalized_model_id
                        .len()
                        .cmp(&right.normalized_model_id.len())
                        .then_with(|| right.normalized_model_id.cmp(&left.normalized_model_id))
                })
                .expect("候选非空");
            ModelMatch {
                kind: ModelMatchKind::Prefix,
                quote: Some(longest),
                normalized_query,
                prefix_ambiguous: false,
            }
        }
    }
}

/// 应用手动覆盖价（计划 §4.4；2026-10-20 放宽条件价）：
/// 分量 Some → 覆盖值；否则使用目录报价的**基础文本价**（`conditional` 不再丢弃报价，
/// 由调用方在行内标注「按基础文本价估算」）。
pub fn apply_override(
    quote: Option<&NormalizedQuote>,
    overrides: Option<&[Option<i128>; 4]>,
) -> EffectiveRates {
    let base = quote.map(|quote| quote.rates);
    let mut rates = [None; 4];
    let mut overridden = false;
    for (index, slot) in rates.iter_mut().enumerate() {
        if let Some(value) = overrides.and_then(|overrides| overrides[index]) {
            *slot = Some(value);
            overridden = true;
        } else if let Some(base) = base {
            *slot = base[index];
        }
    }
    EffectiveRates { rates, overridden }
}

/// 10^-9 USD / 百万 Token 的 scaled 值 → 十进制字符串（无科学计数法）。
pub fn rate_string(scaled: i128) -> String {
    let sign = if scaled < 0 { "-" } else { "" };
    let absolute = scaled.unsigned_abs();
    let whole = absolute / 1_000_000_000;
    let fraction = absolute % 1_000_000_000;
    if fraction == 0 {
        return format!("{sign}{whole}");
    }
    let mut decimal = format!("{fraction:09}");
    while decimal.ends_with('0') {
        decimal.pop();
    }
    format!("{sign}{whole}.{decimal}")
}

fn invalid(detail: impl Into<String>) -> UsageError {
    UsageError::internal(detail.into())
}

/// 十进制文本 → 10^-9 USD / 百万 Token。
///
/// 接受 `"2"` / `"0.2"` / `"2.500000000"`；拒绝负号、指数、空整数、非数字、
/// 超过 9 位小数及任何 `i128` 溢出。
pub fn parse_rate(s: &str) -> Option<i128> {
    if s.starts_with('-') || s.contains(['e', 'E']) {
        return None;
    }
    let (whole, frac) = s.split_once('.').unwrap_or((s, ""));
    if frac.len() > 9
        || whole.is_empty()
        || !whole.chars().all(|c| c.is_ascii_digit())
        || !frac.chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let scale = 10_i128.pow((9 - frac.len()) as u32);
    whole
        .parse::<i128>()
        .ok()?
        .checked_mul(1_000_000_000)?
        .checked_add(if frac.is_empty() {
            0
        } else {
            frac.parse::<i128>().ok()?.checked_mul(scale)?
        })
}

/// 目录费率容错解析（仅用于 `parse_catalog`；用户输入仍走严格的 [`parse_rate`]）。
///
/// models.dev 偶发 f64 序列化噪声（如 `0.30000000000000004`）：超过 9 位小数时
/// **截断**到 9 位（保守取低），其余非法形态（负数/指数/非数字/空整数）仍返回 None。
/// 单条坏费率由调用方降级为“该分量未估价”，不再拒绝整个目录。
fn parse_catalog_rate(text: &str) -> Option<i128> {
    parse_rate(text).or_else(|| {
        let (whole, frac) = text.split_once('.')?;
        if frac.len() <= 9 || !frac.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        parse_rate(&format!("{whole}.{}", &frac[..9]))
    })
}

fn modalities_are_text_only(value: Option<&Value>) -> bool {
    let Some(object) = value.and_then(Value::as_object) else {
        return false;
    };
    // 只接受**恰好** input/output 两个键；任何未知键（audio 等）都意味着无法确认文本基准价。
    if object.len() != 2 || !object.contains_key("input") || !object.contains_key("output") {
        return false;
    }
    let is_single_text = |key: &str| {
        object
            .get(key)
            .and_then(Value::as_array)
            .is_some_and(|items| items.len() == 1 && items[0].as_str() == Some("text"))
    };
    is_single_text("input") && is_single_text("output")
}

/// 定价目录的最小反序列化骨架。
///
/// `cost` 用 `RawValue` 捕获费率**原始十进制文本**：`arbitrary_precision` 是 crate 级
/// feature，会改变全应用 `Value::Number` 的解析与内存行为；`raw_value` 只影响本目录解析。
#[derive(Deserialize)]
struct RawProvider {
    models: BTreeMap<String, RawModel>,
}

#[derive(Deserialize)]
struct RawModel {
    /// models.dev 实际字段；`YYYY` / `YYYY-MM` / `YYYY-MM-DD` 均可能出现。
    #[serde(default)]
    release_date: Option<String>,
    #[serde(default)]
    cost: RawCostField,
    #[serde(default)]
    modalities: Option<Value>,
}

/// 区分「`cost` 字段缺失」与「`cost` 为 null / 非 object」：前者允许，后者必须整目录失败。
#[derive(Default)]
struct RawCostField(Option<BTreeMap<String, Box<RawValue>>>);

impl<'de> Deserialize<'de> for RawCostField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let cost = BTreeMap::<String, Box<RawValue>>::deserialize(deserializer)?;
        Ok(Self(Some(cost)))
    }
}

fn is_json_number_literal(raw: &str) -> bool {
    raw.starts_with(|c: char| c == '-' || c.is_ascii_digit())
}

pub fn parse_catalog(bytes: &[u8]) -> Result<PriceCatalog, UsageError> {
    let providers: BTreeMap<String, RawProvider> = serde_json::from_slice(bytes)
        .map_err(|error| invalid(format!("pricing catalog: invalid JSON: {error}")))?;
    if providers.is_empty() {
        return Err(invalid(
            "pricing catalog: top-level value must be a non-empty object",
        ));
    }
    let provider_count = providers.len();

    let mut quotes = BTreeMap::new();
    for (provider_id, provider) in &providers {
        for (model_id, model) in &provider.models {
            let mut rates = [None; 4];
            let mut conditional = false;
            if let Some(cost) = &model.cost.0 {
                // 未知 cost 键 = 可能存在条件价（阶梯 / 音频 / reasoning 等）。
                if cost.keys().any(|key| !RATE_KEYS.contains(&key.as_str())) {
                    conditional = true;
                }
                for (index, key) in RATE_KEYS.iter().enumerate() {
                    let Some(raw) = cost.get(*key) else {
                        continue;
                    };
                    let text = raw.get();
                    if !is_json_number_literal(text) {
                        // 单条上游脏数据不得让整个目录不可用：该分量降级为未估价。
                        log::warn!(
                            "pricing catalog: {provider_id:?}/{model_id:?} cost.{key} is not a JSON number; treated as unpriced"
                        );
                        continue;
                    }
                    // 只信十进制文本：直接解析 RawValue，绝不经过 f64。
                    let Some(rate) = parse_catalog_rate(text) else {
                        log::warn!(
                            "pricing catalog: {provider_id:?}/{model_id:?} cost.{key} is not a valid rate; treated as unpriced"
                        );
                        continue;
                    };
                    rates[index] = Some(rate);
                }
            }

            if !modalities_are_text_only(model.modalities.as_ref()) {
                conditional = true;
            }

            quotes.insert(
                (provider_id.clone(), model_id.clone()),
                PriceQuote {
                    reference: PricingReference {
                        provider_id: provider_id.clone(),
                        model_id: model_id.clone(),
                    },
                    rates,
                    conditional,
                    release_date: model.release_date.clone(),
                    normalized_model_id: normalize_model_id(model_id),
                },
            );
        }
    }

    // 归一化索引：同一归一化 ID 只保留一条“来源”报价，选不中也不拿别人的价格补洞。
    let mut normalized: BTreeMap<String, NormalizedQuote> = BTreeMap::new();
    // 精选索引（2026-10-20 用户裁决）：直营官方 / Ollama Cloud / OpenRouter **各自**先按
    // 同一条去重链收敛，再按「直营官方 > 托管 > OpenRouter」合成——非精选来源
    // （哪怕 release_date 最新）不参与。这样智谱行不会再取到 Ollama Cloud 的价。
    let mut curated_official: BTreeMap<String, NormalizedQuote> = BTreeMap::new();
    let mut curated_hosted: BTreeMap<String, NormalizedQuote> = BTreeMap::new();
    let mut curated_openrouter: BTreeMap<String, NormalizedQuote> = BTreeMap::new();
    for quote in quotes.values() {
        if quote.normalized_model_id.is_empty() {
            continue;
        }
        let candidate = normalized_candidate(quote);
        match normalized.entry(candidate.normalized_model_id.clone()) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(candidate.clone());
            }
            std::collections::btree_map::Entry::Occupied(mut slot) => {
                if candidate_is_better(&candidate, slot.get()) {
                    slot.insert(candidate.clone());
                }
            }
        }
        let curated_slot = match curated_kind_of(&quote.reference.provider_id) {
            Some("official") => Some(&mut curated_official),
            Some("hosted") => Some(&mut curated_hosted),
            Some("openrouter") => Some(&mut curated_openrouter),
            _ => None,
        };
        if let Some(index) = curated_slot {
            match index.entry(candidate.normalized_model_id.clone()) {
                std::collections::btree_map::Entry::Vacant(slot) => {
                    slot.insert(candidate);
                }
                std::collections::btree_map::Entry::Occupied(mut slot) => {
                    if candidate_is_better(&candidate, slot.get()) {
                        slot.insert(candidate);
                    }
                }
            }
        }
    }
    let mut normalized_curated = curated_official;
    for (key, quote) in curated_hosted {
        normalized_curated.entry(key).or_insert(quote);
    }
    for (key, quote) in curated_openrouter {
        normalized_curated.entry(key).or_insert(quote);
    }

    Ok(PriceCatalog {
        quotes,
        normalized,
        normalized_curated,
        provider_count,
    })
}

// ── 手动覆盖价（计划 §4.4）──────────────────────────────────────────────────
// 覆盖表按归一化模型 ID 全局生效；刷新流程只写快照/状态表，绝不触碰本表。

fn override_invalid(detail: impl Into<String>) -> UsageError {
    UsageError::new("usage-settings-invalid", detail.into())
}

fn parse_override_component(
    component: &str,
    value: Option<&String>,
) -> Result<Option<i128>, UsageError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let rate = parse_rate(value)
        .filter(|rate| i64::try_from(*rate).is_ok())
        .ok_or_else(|| override_invalid(format!("price override rate is invalid: {component}")))?;
    Ok(Some(rate))
}

fn request_rates(request: &PriceOverrideRequest) -> Result<[Option<i128>; 4], UsageError> {
    let rates = [
        parse_override_component("input", request.input.as_ref())?,
        parse_override_component("cacheRead", request.cache_read.as_ref())?,
        parse_override_component("cacheWrite", request.cache_write.as_ref())?,
        parse_override_component("output", request.output.as_ref())?,
    ];
    if rates.iter().all(Option::is_none) {
        return Err(override_invalid(
            "price override must set at least one rate",
        ));
    }
    Ok(rates)
}

fn override_view(
    record: &dao::PriceOverrideRecord,
    catalog: Option<&(String, i64, Arc<PriceCatalog>)>,
) -> PriceOverrideView {
    let mut source_provider = None;
    let mut source_model = None;
    let mut release_date = None;
    let mut snapshot_date = None;
    if let Some((_, fetched_at, catalog)) = catalog {
        if let Some(quote) = catalog.normalized.get(&record.normalized_model_id) {
            source_provider = Some(quote.reference.provider_id.clone());
            source_model = Some(quote.reference.model_id.clone());
            release_date = quote.release_date.clone();
            snapshot_date = Some(*fetched_at);
        }
    }
    PriceOverrideView {
        normalized_model_id: record.normalized_model_id.clone(),
        currency: record.currency,
        input: record.rates[0].map(rate_string),
        cache_read: record.rates[1].map(rate_string),
        cache_write: record.rates[2].map(rate_string),
        output: record.rates[3].map(rate_string),
        updated_at: record.updated_at,
        source_provider,
        source_model,
        release_date,
        snapshot_date,
    }
}

/// 查询层使用的有效覆盖价：四价 + 计价币种。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveOverride {
    pub rates: [Option<i128>; 4],
    pub currency: crate::services::usage::v2::model::PriceCurrency,
}

/// 读取全部手动覆盖价的费率表（查询层一次性加载，避免逐行查库）。
///
/// 无 active 目录也可用；覆盖表与目录快照互不影响。返回顺序按归一化模型 ID 升序。
pub fn load_effective_overrides_with_conn(
    conn: &Connection,
) -> Result<BTreeMap<String, EffectiveOverride>, UsageError> {
    Ok(dao::load_overrides_conn(conn)?
        .into_iter()
        .map(|record| {
            (
                record.normalized_model_id,
                EffectiveOverride {
                    rates: record.rates,
                    currency: record.currency,
                },
            )
        })
        .collect())
}

/// 读取全部手动覆盖价。无 active 目录时来源字段为 null，但覆盖值仍可用。
pub fn overrides_with_conn(
    conn: &Connection,
    _now_ms: i64,
) -> Result<Vec<PriceOverrideView>, UsageError> {
    let records = dao::load_overrides_conn(conn)?;
    if records.is_empty() {
        return Ok(Vec::new());
    }
    let catalog = load_active_catalog_with_conn(conn)?;
    Ok(records
        .iter()
        .map(|record| override_view(record, catalog.as_ref()))
        .collect())
}

/// 写入（或更新）一条覆盖价并返回完整列表。
///
/// `normalized_model_id` 入库前先归一化；至少一项费率；scaled 必须落在 `i64` 且非负。
pub fn set_override_with_conn(
    conn: &mut Connection,
    request: &PriceOverrideRequest,
    now_ms: i64,
) -> Result<Vec<PriceOverrideView>, UsageError> {
    let normalized_model_id = normalize_model_id(&request.normalized_model_id);
    if normalized_model_id.is_empty() {
        return Err(override_invalid(
            "price override normalizedModelId is empty",
        ));
    }
    let rates = request_rates(request)?;
    dao::upsert_override_conn(conn, &normalized_model_id, request.currency, &rates, now_ms)?;
    overrides_with_conn(conn, now_ms)
}

/// 删除覆盖价（不存在为幂等空操作）并返回完整列表。
pub fn delete_override_with_conn(
    conn: &Connection,
    normalized_model_id: &str,
) -> Result<Vec<PriceOverrideView>, UsageError> {
    let normalized_model_id = normalize_model_id(normalized_model_id);
    if !normalized_model_id.is_empty() {
        dao::delete_override_conn(conn, &normalized_model_id)?;
    }
    overrides_with_conn(conn, 0)
}

// ── 生产入口（IPC 用）：只取 DB 锁，业务全部走 `*_with_conn` ──────────────────

fn db_lock_failed() -> UsageError {
    UsageError::new("usage-db-failed", "db poisoned lock")
}

/// 生产入口：短锁搜索 active 目录（命令层不取锁）。
pub fn catalog_entries(
    query: Option<&str>,
    limit: Option<u32>,
) -> Result<Vec<PricingCatalogEntry>, UsageError> {
    let conn = DB.lock().map_err(|_| db_lock_failed())?;
    let limit = limit
        .map(|value| value as usize)
        .unwrap_or(CATALOG_SEARCH_DEFAULT_LIMIT);
    catalog_entries_with_conn(&conn, query, limit)
}

/// 生产入口：短锁作用域目录搜索（命令层不取锁）。
pub fn catalog_search_scoped(
    query: Option<&str>,
    limit: Option<u32>,
    scope: MatchScope,
) -> Result<PricingCatalogSearch, UsageError> {
    let conn = DB.lock().map_err(|_| db_lock_failed())?;
    let limit = limit
        .map(|value| value as usize)
        .unwrap_or(CATALOG_SEARCH_DEFAULT_LIMIT);
    catalog_search_scoped_with_conn(&conn, query, limit, scope)
}

/// 生产入口：短锁价格预览（命令层不取锁）。
pub fn quote_preview(
    provider_id: &str,
    model_id: &str,
    fallback_rate: Option<&str>,
) -> Result<Option<PricingQuotePreview>, UsageError> {
    let conn = DB.lock().map_err(|_| db_lock_failed())?;
    quote_preview_with_conn(&conn, provider_id, model_id, fallback_rate)
}

/// 生产入口：短锁自动匹配（命令层不取锁；恒精选作用域）。
pub fn auto_match(
    model_id: &str,
    fallback_rate: Option<&str>,
) -> Result<PricingAutoMatch, UsageError> {
    let conn = DB.lock().map_err(|_| db_lock_failed())?;
    auto_match_with_conn(&conn, model_id, fallback_rate)
}

pub fn overrides(now_ms: i64) -> Result<Vec<PriceOverrideView>, UsageError> {
    let conn = DB.lock().map_err(|_| db_lock_failed())?;
    overrides_with_conn(&conn, now_ms)
}

pub fn set_override(
    request: &PriceOverrideRequest,
    now_ms: i64,
) -> Result<Vec<PriceOverrideView>, UsageError> {
    let mut conn = DB.lock().map_err(|_| db_lock_failed())?;
    set_override_with_conn(&mut conn, request, now_ms)
}

pub fn delete_override(normalized_model_id: &str) -> Result<Vec<PriceOverrideView>, UsageError> {
    let conn = DB.lock().map_err(|_| db_lock_failed())?;
    delete_override_with_conn(&conn, normalized_model_id)
}

// ==== usage 自省锁：以下为排除区（测试代码），勿删勿复制 ====
#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::cell::Cell;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::Arc;
    use std::thread;

    fn valid_bytes() -> Vec<u8> {
        VALID.as_bytes().to_vec()
    }

    fn mem_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        conn
    }

    fn file_db() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pricing.db");
        let conn = Connection::open(&path).unwrap();
        crate::database::schema::init(&conn);
        (dir, path)
    }

    fn seed(conn: &mut Connection, now_ms: i64) -> PricingStatus {
        set_enabled_with_conn(conn, true).unwrap();
        refresh_with_conn(conn, now_ms, false, || Ok(valid_bytes())).unwrap()
    }

    const VALID: &str = include_str!("../../../../../tests/fixtures/usage-v2/pricing/valid.json");
    const MISSING: &str =
        include_str!("../../../../../tests/fixtures/usage-v2/pricing/missing.json");
    const INVALID: &str =
        include_str!("../../../../../tests/fixtures/usage-v2/pricing/invalid.json");
    const CONDITIONAL: &str =
        include_str!("../../../../../tests/fixtures/usage-v2/pricing/conditional.json");

    fn parse(text: &str) -> Result<PriceCatalog, UsageError> {
        parse_catalog(text.as_bytes())
    }

    fn quote<'a>(catalog: &'a PriceCatalog, provider: &str, model: &str) -> &'a PriceQuote {
        catalog
            .quotes
            .get(&(provider.to_string(), model.to_string()))
            .unwrap_or_else(|| panic!("缺少精确键 {provider:?}/{model:?}"))
    }

    fn catalog_with_cost(cost_json: &str) -> Result<PriceCatalog, UsageError> {
        let raw = format!(
            r#"{{"p":{{"id":"p","models":{{"m":{{"id":"m","cost":{cost_json},
            "modalities":{{"input":["text"],"output":["text"]}}}}}}}}}}"#
        );
        parse(&raw)
    }

    #[test]
    fn valid_fixture_yields_exact_scaled_rates() {
        let catalog = parse(VALID).unwrap();
        let quote = quote(&catalog, "p", "m");
        assert_eq!(
            quote.rates,
            [
                Some(2_000_000_000),
                Some(200_000_000),
                Some(2_500_000_000),
                Some(10_000_000_000),
            ],
            "顺序必须是 [input, cache_read, cache_write, output]，单位 10^-9 USD/M"
        );
        assert!(!quote.conditional);
        assert_eq!(quote.reference.provider_id, "p");
        assert_eq!(quote.reference.model_id, "m");
    }

    #[test]
    fn missing_cost_keeps_quote_with_no_rates() {
        let catalog = parse(MISSING).unwrap();
        let quote = quote(&catalog, "p", "m");
        assert_eq!(quote.rates, [None; 4]);
        assert!(!quote.conditional, "缺价本身不是条件价");
    }

    #[test]
    fn invalid_rate_degrades_one_entry_without_failing_the_catalog() {
        let catalog = parse(INVALID).expect("单条坏费率不得让整个目录失败");
        let quote = quote(&catalog, "p", "m");
        assert_eq!(quote.rates[0], None, "非法 input 费率降级为未估价");
        assert_eq!(quote.rates[3], Some(10_000_000_000), "其余合法费率必须保留");
    }

    #[test]
    fn float_precision_noise_is_truncated_to_nine_decimals() {
        let catalog = catalog_with_cost(r#"{"input":0.30000000000000004}"#).unwrap();
        let quote = quote(&catalog, "p", "m");
        assert_eq!(
            quote.rates[0],
            Some(300_000_000),
            "f64 序列化噪声截断到 9 位后应可计价（0.3 USD/M）"
        );
    }

    #[test]
    fn non_numeric_rate_is_unpriced_not_a_catalog_error() {
        let catalog = catalog_with_cost(r#"{"input":"oops","output":10}"#).unwrap();
        let quote = quote(&catalog, "p", "m");
        assert_eq!(quote.rates[0], None);
        assert_eq!(quote.rates[3], Some(10_000_000_000));
    }

    #[test]
    fn unknown_cost_key_marks_conditional_but_keeps_rates() {
        let catalog = parse(CONDITIONAL).unwrap();
        let quote = quote(&catalog, "p", "m");
        assert!(quote.conditional);
        assert_eq!(
            quote.rates,
            [
                Some(2_000_000_000),
                Some(200_000_000),
                Some(2_500_000_000),
                Some(10_000_000_000),
            ]
        );
    }

    #[test]
    fn explicit_zero_rate_is_price_not_missing() {
        let catalog = catalog_with_cost(r#"{"input":0}"#).unwrap();
        let quote = quote(&catalog, "p", "m");
        assert_eq!(quote.rates[0], Some(0), "显式 0 价必须保留为 Some(0)");
        assert_eq!(&quote.rates[1..], &[None, None, None]);
        assert!(!quote.conditional);
    }

    #[test]
    fn parse_rate_rejects_every_non_decimal_text_form() {
        assert_eq!(parse_rate("2"), Some(2_000_000_000));
        assert_eq!(parse_rate("0.2"), Some(200_000_000));
        assert_eq!(parse_rate("2.5"), Some(2_500_000_000));
        assert_eq!(parse_rate("0"), Some(0));
        for bad in [
            "-1",           // 负号
            "1e3",          // 指数
            "1E3",          // 大写指数
            "",             // 空
            ".5",           // 空整数
            "1.",           // 零位小数可接受？——只接受纯数字 fraction，空 fraction 合法；见下
            "abc",          // 非数字
            "1.2.3",        // 多小数点
            "1.1234567891", // >9 位小数
            "999999999999999999999999999999999999999999999999",
        ] {
            if bad == "1." {
                // `split_once('.')` 后 fraction 为空串 ⇒ 合法（等价 1）。
                assert_eq!(parse_rate(bad), Some(1_000_000_000));
            } else {
                assert_eq!(parse_rate(bad), None, "`{bad}` 必须被拒绝");
            }
        }
    }

    #[test]
    fn directory_degrades_invalid_rate_shapes_without_failing() {
        // 单条坏费率只让该分量未估价，不得让整个目录失败。
        for cost in [
            r#"{"input":-1}"#,
            r#"{"input":1e100}"#,
            r#"{"input":"2"}"#,
            r#"{"input":999999999999999999999999999999999999999999}"#,
        ] {
            let catalog = catalog_with_cost(cost)
                .unwrap_or_else(|error| panic!("cost={cost} 不得让目录失败: {error:?}"));
            assert_eq!(
                quote(&catalog, "p", "m").rates[0],
                None,
                "cost={cost} 应降级为未估价"
            );
        }
        // >9 位小数按保守截断保留（覆盖上游 f64 序列化噪声）。
        for (cost, expected) in [
            (r#"{"input":1.1234567891}"#, 1_123_456_789),
            (r#"{"input":1.0000000000}"#, 1_000_000_000),
            (r#"{"input":1.0000000000000001}"#, 1_000_000_000),
        ] {
            let catalog = catalog_with_cost(cost).unwrap();
            assert_eq!(
                quote(&catalog, "p", "m").rates[0],
                Some(expected),
                "cost={cost} 应截断到 9 位小数"
            );
        }
        assert!(
            catalog_with_cost(r#"[]"#).is_err(),
            "cost 非 object 仍必须失败"
        );
    }

    #[test]
    fn different_providers_with_the_same_model_are_two_quotes() {
        let raw = r#"{
            "p1":{"models":{"m":{"cost":{"input":1},"modalities":{"input":["text"],"output":["text"]}}}},
            "p2":{"models":{"m":{"cost":{"input":2},"modalities":{"input":["text"],"output":["text"]}}}}
        }"#;
        let catalog = parse(raw).unwrap();
        assert_eq!(catalog.quotes.len(), 2);
        assert_eq!(quote(&catalog, "p1", "m").rates[0], Some(1_000_000_000));
        assert_eq!(quote(&catalog, "p2", "m").rates[0], Some(2_000_000_000));
    }

    #[test]
    fn lookup_is_exact_and_never_guesses_aliases() {
        let raw = r#"{"p":{"models":{"m":{
            "id":"canonical-alias",
            "cost":{"input":2},
            "modalities":{"input":["text"],"output":["text"]}
        }}}}"#;
        let catalog = parse(raw).unwrap();
        assert!(catalog
            .quotes
            .contains_key(&("p".to_string(), "m".to_string())));
        assert!(
            !catalog
                .quotes
                .contains_key(&("p".to_string(), "canonical-alias".to_string())),
            "不得用 model.id 或别名回退"
        );
        assert!(!catalog
            .quotes
            .contains_key(&("alias".to_string(), "m".to_string())));
    }

    #[test]
    fn non_text_or_unknown_modalities_are_conditional() {
        for modalities in [
            Value::Null,
            serde_json::json!([]),
            serde_json::json!({"input":["text"]}),
            serde_json::json!({"input":["text"],"output":["image"]}),
            serde_json::json!({"input":["text","image"],"output":["text"]}),
            serde_json::json!({"input":["text"],"output":["text"],"audio":["audio"]}),
            serde_json::json!({"input":["text"],"output":["text"],"unexpected":true}),
        ] {
            let raw = serde_json::json!({
                "p":{"models":{"m":{
                    "cost":{"input":2},
                    "modalities": modalities
                }}}
            });
            let catalog = parse(&raw.to_string()).unwrap();
            assert!(
                quote(&catalog, "p", "m").conditional,
                "modalities={modalities} 必须标记 conditional"
            );
        }
    }

    #[test]
    fn empty_or_non_object_top_level_is_rejected() {
        for raw in [
            b"".as_slice(),
            b"[]".as_slice(),
            b"{}".as_slice(),
            b"null".as_slice(),
        ] {
            let error = parse_catalog(raw).unwrap_err();
            assert_eq!(error.code, "usage-internal");
        }
    }

    #[test]
    fn provider_without_models_object_is_rejected() {
        for raw in [
            r#"{"p":{"id":"p"}}"#,
            r#"{"p":{"models":[]}}"#,
            r#"{"p":[]}"#,
        ] {
            assert!(parse(raw).is_err(), "raw={raw} 必须整体失败");
        }
    }

    // ── 目录缓存 ──────────────────────────────────────────────────────────

    #[test]
    fn catalog_cache_only_reuses_matching_snapshot() {
        let mut cache = CatalogCache::new();
        let first = Arc::new(parse(VALID).unwrap());
        cache.put("snapshot-a", Arc::clone(&first));
        assert!(
            Arc::ptr_eq(
                &first,
                &cache.get("snapshot-a").expect("同 id 必须命中缓存")
            ),
            "同一 snapshot 必须复用同一份解析结果"
        );
        assert!(cache.get("snapshot-b").is_none(), "不同 id 不得串用");

        let second = Arc::new(parse(MISSING).unwrap());
        cache.put("snapshot-b", Arc::clone(&second));
        assert!(cache.get("snapshot-a").is_none(), "只保留最新快照");
        assert!(Arc::ptr_eq(
            &second,
            &cache.get("snapshot-b").expect("新快照必须命中缓存")
        ));
    }

    // ── Task 8：快照 / 刷新 ────────────────────────────────────────────────

    #[test]
    fn disabled_refresh_never_fetches() {
        let mut conn = mem_db();
        let calls = Cell::new(0);
        let status = refresh_with_conn(&mut conn, 1_000, false, || {
            calls.set(calls.get() + 1);
            Ok(valid_bytes())
        })
        .unwrap();
        assert_eq!(calls.get(), 0, "未启用不得请求");
        assert!(!status.enabled);
        assert_eq!(status.snapshot_id, None);
        assert_eq!(status.fetched_at, None);
        assert!(status.stale);
    }

    #[test]
    fn enabling_without_snapshot_fetches_once_and_activates() {
        let mut conn = mem_db();
        set_enabled_with_conn(&conn, true).unwrap();
        let calls = Cell::new(0);
        let status = refresh_with_conn(&mut conn, 1_000, false, || {
            calls.set(calls.get() + 1);
            Ok(valid_bytes())
        })
        .unwrap();
        assert_eq!(calls.get(), 1);
        assert!(status.enabled);
        assert_eq!(status.fetched_at, Some(1_000));
        assert!(!status.stale);

        let (id, url, fetched_at, payload) =
            dao::load_active_snapshot_conn(&conn).unwrap().unwrap();
        assert_eq!(Some(id), status.snapshot_id);
        assert_eq!(url, PRICING_SOURCE_URL);
        assert_eq!(fetched_at, 1_000);
        assert_eq!(payload.as_bytes(), VALID.as_bytes());
    }

    #[test]
    fn fresh_snapshot_is_reused_until_24h_then_refetched() {
        const HOUR: i64 = 3_600_000;
        let mut conn = mem_db();
        let seeded = seed(&mut conn, 1_000);
        assert_eq!(seeded.fetched_at, Some(1_000));

        let calls = Cell::new(0);
        let at_23h = refresh_with_conn(&mut conn, 1_000 + 23 * HOUR, false, || {
            calls.set(calls.get() + 1);
            Ok(valid_bytes())
        })
        .unwrap();
        assert_eq!(calls.get(), 0, "23h 内不得重复请求");
        assert_eq!(at_23h.fetched_at, Some(1_000));

        let at_24h = refresh_with_conn(&mut conn, 1_000 + 24 * HOUR, false, || {
            calls.set(calls.get() + 1);
            Ok(valid_bytes())
        })
        .unwrap();
        assert_eq!(calls.get(), 1, "24h 到期必须重新请求");
        assert_eq!(at_24h.fetched_at, Some(1_000 + 24 * HOUR));
    }

    #[test]
    fn concurrent_refreshes_share_a_single_fetch() {
        let (dir, path) = file_db();
        {
            let conn = Connection::open(&path).unwrap();
            set_enabled_with_conn(&conn, true).unwrap();
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for force in [false, false] {
            let path = path.clone();
            let calls = Arc::clone(&calls);
            handles.push(thread::spawn(move || {
                let mut conn = Connection::open(&path).unwrap();
                refresh_with_conn(&mut conn, 5_000, force, || {
                    calls.fetch_add(1, AtomicOrdering::SeqCst);
                    thread::sleep(Duration::from_millis(80));
                    Ok(valid_bytes())
                })
                .unwrap()
            }));
        }
        for handle in handles {
            let status = handle.join().unwrap();
            assert!(status.snapshot_id.is_some());
            assert_eq!(status.fetched_at, Some(5_000));
        }
        assert_eq!(
            calls.load(AtomicOrdering::SeqCst),
            1,
            "单飞：只允许一次 fetch"
        );
        drop(dir);
    }

    #[test]
    fn fetch_or_parse_failure_keeps_old_snapshot_and_records_error() {
        let mut conn = mem_db();
        let seeded = seed(&mut conn, 1_000);
        let original_id = seeded.snapshot_id.clone();

        let fetch_failed = refresh_with_conn(&mut conn, 2_000, true, || {
            Err(UsageError::internal("network down".to_string()))
        })
        .unwrap();
        assert_eq!(fetch_failed.snapshot_id, original_id);
        assert_eq!(fetch_failed.fetched_at, Some(1_000), "失败不得更新时间");
        assert!(fetch_failed.stale);
        assert!(fetch_failed
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("network down")));

        let parse_failed =
            refresh_with_conn(&mut conn, 3_000, true, || Ok(b"not json".to_vec())).unwrap();
        assert_eq!(parse_failed.snapshot_id, original_id);
        assert_eq!(parse_failed.fetched_at, Some(1_000));
        assert!(parse_failed
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("invalid JSON")));
    }

    #[test]
    fn snapshot_survives_reconnect_and_is_readable() {
        let (dir, path) = file_db();
        {
            let mut conn = Connection::open(&path).unwrap();
            seed(&mut conn, 1_000);
        }
        let conn = Connection::open(&path).unwrap();
        let status = status_with_conn(&conn, 1_000 + 1_000).unwrap();
        assert!(status.enabled);
        assert_eq!(status.fetched_at, Some(1_000));
        assert!(!status.stale);
        let (_, _, _, payload) = dao::load_active_snapshot_conn(&conn).unwrap().unwrap();
        assert_eq!(payload.as_bytes(), VALID.as_bytes());
        drop(dir);
    }

    #[test]
    fn disabled_switch_stops_refreshing_but_keeps_last_snapshot() {
        let mut conn = mem_db();
        let seeded = seed(&mut conn, 1_000);
        set_enabled_with_conn(&conn, false).unwrap();
        let calls = Cell::new(0);
        let status = refresh_with_conn(&mut conn, 2_000, true, || {
            calls.set(calls.get() + 1);
            Ok(valid_bytes())
        })
        .unwrap();
        assert_eq!(calls.get(), 0, "关闭后即使 force 也不得请求");
        assert!(!status.enabled);
        assert_eq!(status.snapshot_id, seeded.snapshot_id);
    }

    #[test]
    fn re_enable_preserves_active_and_error_until_a_successful_refresh() {
        let mut conn = mem_db();
        let seeded = seed(&mut conn, 1_000);
        let active_id = seeded.snapshot_id.clone();

        refresh_with_conn(&mut conn, 1_100, true, || {
            Err(UsageError::internal("boom".to_string()))
        })
        .unwrap();
        set_enabled_with_conn(&conn, false).unwrap();
        set_enabled_with_conn(&conn, true).unwrap();

        let preserved = status_with_conn(&conn, 1_150).unwrap();
        assert_eq!(preserved.snapshot_id, active_id, "重复启用必须保留 active");
        assert!(preserved.last_error.is_some(), "重复启用必须保留 lastError");

        let refreshed = refresh_with_conn(&mut conn, 1_200, true, || Ok(valid_bytes())).unwrap();
        assert_eq!(refreshed.last_error, None, "成功刷新必须清 lastError");
        assert_eq!(refreshed.fetched_at, Some(1_200));
        assert_ne!(refreshed.snapshot_id, active_id);
    }

    #[test]
    fn snapshot_id_is_lowercase_sha256_plus_fetched_at() {
        let mut conn = mem_db();
        let first = seed(&mut conn, 1_000);
        let id = first.snapshot_id.unwrap();
        let (hash, fetched) = id.split_once(':').expect("id 必须是 hash:time");
        assert_eq!(fetched, "1000");
        assert_eq!(hash.len(), 64, "sha256 小写 hex 固定 64 字符");
        assert!(hash
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));

        let second = refresh_with_conn(&mut conn, 1_001, true, || Ok(valid_bytes())).unwrap();
        let id2 = second.snapshot_id.unwrap();
        let (hash2, fetched2) = id2.split_once(':').unwrap();
        assert_eq!(hash2, hash, "同 payload 哈希部分相同");
        assert_eq!(fetched2, "1001");
        assert_ne!(id2, id, "同 payload 新时间必须得到新 ID");
    }

    #[test]
    fn missing_snapshot_status_has_null_fetched_at_and_is_stale() {
        let conn = mem_db();
        let status = status_with_conn(&conn, 12_345).unwrap();
        assert!(!status.enabled);
        assert_eq!(status.snapshot_id, None);
        assert_eq!(status.fetched_at, None);
        assert!(status.stale);
        assert_eq!(status.last_error, None);
    }

    // ── Task 3：归一化 / 目录去重 / 前缀回退 / 手动覆盖 ────────────────────

    const NORMALIZE: &str =
        include_str!("../../../../../tests/fixtures/usage-v2/pricing/normalize.json");
    const DUPLICATES: &str =
        include_str!("../../../../../tests/fixtures/usage-v2/pricing/duplicates.json");
    const PREFIX: &str = include_str!("../../../../../tests/fixtures/usage-v2/pricing/prefix.json");

    #[derive(serde::Deserialize)]
    struct NormalizeCase {
        raw: String,
        normalized: String,
    }

    #[test]
    fn normalize_model_id_matches_every_locked_example() {
        let cases: Vec<NormalizeCase> = serde_json::from_str(NORMALIZE).unwrap();
        assert!(!cases.is_empty());
        for case in cases {
            assert_eq!(
                normalize_model_id(&case.raw),
                case.normalized,
                "raw={:?}",
                case.raw
            );
        }
    }

    #[test]
    fn normalize_model_id_never_strips_family_or_modality_suffixes() {
        for (raw, expected) in [
            ("gpt-4", "gpt-4"),
            ("qwen3-max", "qwen3-max"),
            ("claude-sonnet-4-5-max", "claude-sonnet-4-5-max"),
            ("model-chat", "model-chat"),
            ("model-instruct", "model-instruct"),
            ("model-base", "model-base"),
            ("model-xhigh", "model-xhigh"),
            ("model-ultra", "model-ultra"),
        ] {
            assert_eq!(normalize_model_id(raw), expected, "raw={raw}");
        }
    }

    #[test]
    fn release_date_parses_year_month_and_day_shapes() {
        assert_eq!(parse_release_date(Some("2024-08-06")), (2024, 8, 6));
        assert_eq!(parse_release_date(Some("2024-08")), (2024, 8, 0));
        assert_eq!(parse_release_date(Some("2024")), (2024, 0, 0));
        for invalid in [
            None,
            Some(""),
            Some("not-a-date"),
            Some("2024-13"),
            Some("2024-08-32"),
            Some("20240806"),
        ] {
            assert_eq!(parse_release_date(invalid), (0, 0, 0), "{invalid:?}");
        }
    }

    #[test]
    fn catalog_dedupes_same_normalized_model_by_release_date_quality_and_provider() {
        let catalog = parse(DUPLICATES).unwrap();

        let dated = catalog.normalized.get("dup-date").unwrap();
        assert_eq!(
            dated.reference.provider_id, "provider-new",
            "release_date 最新优先"
        );
        assert_eq!(dated.reference.model_id, "dup-date-2024-11-20");
        assert_eq!(dated.rates[0], Some(2_000_000_000));
        assert_eq!(dated.release_date.as_deref(), Some("2024-11-20"));
        assert_eq!(dated.normalized_model_id, "dup-date");

        let priced = catalog.normalized.get("dup-price").unwrap();
        assert_eq!(
            priced.reference.provider_id, "provider-new",
            "同日期有价优先"
        );
        assert_eq!(priced.rates[0], Some(3_000_000_000));

        let conditional = catalog.normalized.get("dup-cond").unwrap();
        assert_eq!(
            conditional.reference.provider_id, "provider-new",
            "同日期非条件价优先"
        );
        assert!(!conditional.conditional);

        let tie = catalog.normalized.get("dup-tie").unwrap();
        assert_eq!(
            tie.reference.provider_id, "provider-a",
            "完全相同时 provider 字典序"
        );

        let free = catalog.normalized.get("dup-free").unwrap();
        assert_eq!(free.reference.provider_id, "provider-new");
        assert_eq!(
            free.rates, [None; 4],
            "最新条目无价时保留来源且不用旧价补洞"
        );

        assert_eq!(
            catalog
                .normalized
                .get("dup-month")
                .unwrap()
                .reference
                .provider_id,
            "provider-new",
            "YYYY-MM 按日期降序"
        );
        assert_eq!(
            catalog
                .normalized
                .get("dup-year")
                .unwrap()
                .reference
                .provider_id,
            "provider-new",
            "YYYY 按日期降序"
        );
        assert_eq!(
            catalog
                .normalized
                .get("dup-invalid-date")
                .unwrap()
                .reference
                .provider_id,
            "provider-new",
            "非法日期视为 (0,0,0)"
        );

        assert!(
            catalog.quotes.contains_key(&(
                "provider-old".to_string(),
                "dup-date-2024-08-06".to_string()
            )),
            "精确键仍保留，供用户 PricingRule 引用"
        );
    }

    #[test]
    fn empty_normalized_ids_never_enter_catalog_index() {
        let raw = r#"{"p":{"models":{"":{"cost":{"input":1},
            "modalities":{"input":["text"],"output":["text"]}}}}}"#;
        let catalog = parse(raw).unwrap();
        assert!(catalog
            .quotes
            .contains_key(&("p".to_string(), String::new())));
        assert!(!catalog.normalized.contains_key(""));
    }

    /// 既有自动匹配用例（2026-10-20 Task 3 改写）：口径 = **All 全量索引**，逐字保留
    /// 原有保守性断言；Curated 作用域的行为由下方精选用例对照锁定。
    #[test]
    fn match_model_prefers_exact_then_safe_prefix_fallback() {
        let catalog = parse(PREFIX).unwrap();

        let empty = match_model_scoped(&catalog, "", MatchScope::All);
        assert_eq!(empty.kind, ModelMatchKind::None);
        assert!(empty.quote.is_none());

        let exact = match_model_scoped(&catalog, "openai/GPT-4o-2024-08-06", MatchScope::All);
        assert_eq!(exact.kind, ModelMatchKind::Exact);
        assert_eq!(exact.normalized_query, "gpt-4o");
        assert_eq!(exact.quote.unwrap().reference.model_id, "gpt-4o");

        let beta = match_model_scoped(&catalog, "beta-model-beta", MatchScope::All);
        assert_eq!(beta.kind, ModelMatchKind::Prefix);
        assert_eq!(beta.quote.unwrap().reference.model_id, "beta-model");

        let date_noise = match_model_scoped(&catalog, "gpt-4o-2024", MatchScope::All);
        assert_eq!(date_noise.kind, ModelMatchKind::Prefix);

        // 同费率多候选：同 conditional 取最长。
        let longest = match_model_scoped(&catalog, "long-a-alpha-alpha", MatchScope::All);
        assert_eq!(longest.kind, ModelMatchKind::Prefix);
        assert_eq!(
            longest.quote.unwrap().reference.model_id,
            "long-a-alpha",
            "同价同 conditional 取最长"
        );

        // 同费率但 conditional 不同：优先非条件价（即使它更短）。
        let conditional = match_model_scoped(&catalog, "cond-a-alpha-alpha", MatchScope::All);
        assert_eq!(conditional.kind, ModelMatchKind::Prefix);
        assert_eq!(
            conditional.quote.unwrap().reference.model_id,
            "cond-a",
            "同费率候选必须优先非条件价"
        );
    }

    #[test]
    fn noise_suffix_accepts_versions_and_dates_but_not_bare_numbers() {
        for noise in [
            "-v1",
            "-2024",
            "-2024-08",
            "-2024-08-06",
            "-20240806",
            "-preview",
        ] {
            assert!(is_noise_suffix(noise), "{noise} 必须仍可回退");
        }
        for number in ["-2", "-1", "-4-1", "-2024-0", "-20-4"] {
            assert!(!is_noise_suffix(number), "{number} 不得作为可忽略后缀");
        }
    }

    #[test]
    fn date_noise_rejects_out_of_range_month_and_day() {
        // 与 `parse_release_date` 同口径：月 1..=12、日 1..=31。
        for noise in ["-2024-01", "-2024-12", "-2024-01-01", "-2024-12-31"] {
            assert!(is_noise_suffix(noise), "{noise} 是合法日期形态");
        }
        for not_date in [
            "-2024-00",
            "-2024-13",
            "-2024-99",
            "-2024-01-00",
            "-2024-01-32",
            "-2024-99-01",
            "-20241301",
            "-20240132",
        ] {
            assert!(
                !is_noise_suffix(not_date),
                "{not_date} 月/日越界，不得当作日期噪声回退"
            );
        }
    }

    #[test]
    fn match_model_never_prefix_falls_back_over_bare_numbers() {
        let catalog = parse(PREFIX).unwrap();

        for raw in ["gpt-4.1", "claude-2.1", "claude-3.5-sonnet-2"] {
            let got = match_model_scoped(&catalog, raw, MatchScope::All);
            assert_eq!(
                got.kind,
                ModelMatchKind::None,
                "{raw} 不得按裸数字后缀回退到更短的模型名"
            );
            assert!(!got.prefix_ambiguous, "{raw} 不存在同价候选，不应判歧义");
        }

        // 旧的「数字后缀同价取最长」路径已关闭。
        let bare_numeric = match_model_scoped(&catalog, "gemini-2-2", MatchScope::All);
        assert_eq!(bare_numeric.kind, ModelMatchKind::None);
    }

    #[test]
    fn match_model_never_guesses_ambiguous_or_dangerous_prefixes() {
        let catalog = parse(PREFIX).unwrap();

        let ambiguous = match_model_scoped(&catalog, "qwen-alpha-alpha", MatchScope::All);
        assert_eq!(ambiguous.kind, ModelMatchKind::None);
        assert!(ambiguous.prefix_ambiguous);
        assert!(ambiguous.quote.is_none());

        let dangerous = match_model_scoped(&catalog, "codex-ultra", MatchScope::All);
        assert_eq!(dangerous.kind, ModelMatchKind::None);
        assert!(!dangerous.prefix_ambiguous);

        let short = match_model_scoped(&catalog, "abc-2", MatchScope::All);
        assert_eq!(short.kind, ModelMatchKind::None);

        let reverse = match_model_scoped(&catalog, "gpt", MatchScope::All);
        assert_eq!(
            reverse.kind,
            ModelMatchKind::None,
            "不做目录键以 query 为前缀的反向匹配"
        );
    }

    fn override_request(
        normalized_model_id: &str,
        input: Option<&str>,
        cache_read: Option<&str>,
        cache_write: Option<&str>,
        output: Option<&str>,
    ) -> PriceOverrideRequest {
        PriceOverrideRequest {
            normalized_model_id: normalized_model_id.to_string(),
            currency: crate::services::usage::v2::model::PriceCurrency::Usd,
            input: input.map(str::to_string),
            cache_read: cache_read.map(str::to_string),
            cache_write: cache_write.map(str::to_string),
            output: output.map(str::to_string),
        }
    }

    #[test]
    fn override_roundtrip_without_catalog_and_delete_is_idempotent() {
        let mut conn = mem_db();
        let request = override_request(
            "OpenAI/GPT-4o-2024-08-06",
            Some("1.5"),
            Some("0.25"),
            None,
            Some("6"),
        );
        let views = set_override_with_conn(&mut conn, &request, 1_000).unwrap();
        assert_eq!(views.len(), 1);
        let view = &views[0];
        assert_eq!(view.normalized_model_id, "gpt-4o");
        assert_eq!(view.input.as_deref(), Some("1.5"));
        assert_eq!(view.cache_read.as_deref(), Some("0.25"));
        assert_eq!(view.cache_write, None);
        assert_eq!(view.output.as_deref(), Some("6"));
        assert_eq!(view.updated_at, 1_000);
        assert_eq!(view.source_provider, None, "无目录时来源为 null");
        assert_eq!(view.source_model, None);
        assert_eq!(view.release_date, None);
        assert_eq!(view.snapshot_date, None);

        assert_eq!(overrides_with_conn(&conn, 2_000).unwrap(), views);

        assert!(
            delete_override_with_conn(&conn, "gpt-4o")
                .unwrap()
                .is_empty(),
            "删除后返回空列表"
        );
        assert!(
            delete_override_with_conn(&conn, "openai/gpt-4o-2024-08-06")
                .unwrap()
                .is_empty(),
            "删除不存在的 id 必须幂等"
        );
        assert!(overrides_with_conn(&conn, 3_000).unwrap().is_empty());
    }

    #[test]
    fn override_validation_rejects_empty_id_bad_rates_and_overflow() {
        let mut conn = mem_db();

        let error = set_override_with_conn(
            &mut conn,
            &override_request("", Some("1"), None, None, None),
            1,
        )
        .unwrap_err();
        assert_eq!(error.code, "usage-settings-invalid");
        assert_eq!(error.detail, "price override normalizedModelId is empty");

        let error = set_override_with_conn(
            &mut conn,
            &override_request("gpt-4o", None, None, None, None),
            1,
        )
        .unwrap_err();
        assert_eq!(error.detail, "price override must set at least one rate");

        for (request, component) in [
            (
                override_request("gpt-4o", Some("-1"), None, None, None),
                "input",
            ),
            (
                override_request("gpt-4o", None, Some("1e3"), None, None),
                "cacheRead",
            ),
            (
                override_request("gpt-4o", None, None, Some("1.1234567891"), None),
                "cacheWrite",
            ),
            (
                override_request("gpt-4o", None, None, None, Some("99999999999")),
                "output",
            ),
        ] {
            let error = set_override_with_conn(&mut conn, &request, 1).unwrap_err();
            assert_eq!(error.code, "usage-settings-invalid");
            assert_eq!(
                error.detail,
                format!("price override rate is invalid: {component}")
            );
        }
        assert!(overrides_with_conn(&conn, 2).unwrap().is_empty());
    }

    #[test]
    fn override_view_resolves_catalog_source_and_snapshot_date() {
        let mut conn = mem_db();
        set_enabled_with_conn(&conn, true).unwrap();
        refresh_with_conn(&mut conn, 1_000, false, || {
            Ok(DUPLICATES.as_bytes().to_vec())
        })
        .unwrap();

        let views = set_override_with_conn(
            &mut conn,
            &override_request("dup-date", Some("1.25"), None, None, None),
            2_000,
        )
        .unwrap();
        let view = &views[0];
        assert_eq!(view.source_provider.as_deref(), Some("provider-new"));
        assert_eq!(view.source_model.as_deref(), Some("dup-date-2024-11-20"));
        assert_eq!(view.release_date.as_deref(), Some("2024-11-20"));
        assert_eq!(view.snapshot_date, Some(1_000));
        assert_eq!(view.input.as_deref(), Some("1.25"));
    }

    #[test]
    fn refresh_never_overwrites_manual_overrides() {
        let mut conn = mem_db();
        set_override_with_conn(
            &mut conn,
            &override_request("gpt-4o", Some("1.5"), None, None, None),
            500,
        )
        .unwrap();
        let before = overrides_with_conn(&conn, 600).unwrap();

        seed(&mut conn, 1_000);
        refresh_with_conn(&mut conn, 2_000, true, || {
            Ok(DUPLICATES.as_bytes().to_vec())
        })
        .unwrap();

        let after = overrides_with_conn(&conn, 3_000).unwrap();
        assert_eq!(before, after, "刷新只写快照/状态，绝不触碰覆盖表");
    }

    fn normalized_quote(rates: [Option<i128>; 4], conditional: bool) -> NormalizedQuote {
        NormalizedQuote {
            reference: PricingReference {
                provider_id: "p".to_string(),
                model_id: "m".to_string(),
            },
            rates,
            conditional,
            release_date: None,
            normalized_model_id: "m".to_string(),
        }
    }

    #[test]
    fn manual_override_wins_per_component_and_unlocks_conditional_rates() {
        let conditional =
            normalized_quote([Some(1_000_000_000), None, Some(3_000_000_000), None], true);
        let effective = apply_override(
            Some(&conditional),
            Some(&[None, Some(2_000_000_000), None, Some(4_000_000_000)]),
        );
        assert_eq!(
            effective.rates,
            [
                Some(1_000_000_000),
                Some(2_000_000_000),
                Some(3_000_000_000),
                Some(4_000_000_000)
            ],
            "条件价未覆盖的分量沿用基础文本价（2026-10-20 放宽）"
        );
        assert!(effective.overridden);

        let plain = normalized_quote(
            [Some(1_000_000_000), None, Some(3_000_000_000), None],
            false,
        );
        let partial = apply_override(Some(&plain), Some(&[None, Some(2_000_000_000), None, None]));
        assert_eq!(
            partial.rates,
            [
                Some(1_000_000_000),
                Some(2_000_000_000),
                Some(3_000_000_000),
                None
            ]
        );
        assert!(partial.overridden);

        let catalog_only = apply_override(Some(&plain), None);
        assert_eq!(catalog_only.rates, plain.rates);
        assert!(!catalog_only.overridden);

        let conditional_only = apply_override(Some(&conditional), None);
        assert_eq!(
            conditional_only.rates, conditional.rates,
            "条件价无覆盖：按基础文本价参与计价（估算口径由费用查询 conditionalBaseRate 标注）"
        );
        assert!(!conditional_only.overridden);

        let override_only = apply_override(None, Some(&[Some(5), None, None, None]));
        assert_eq!(override_only.rates, [Some(5), None, None, None]);
        assert!(override_only.overridden);
    }

    #[test]
    fn rate_string_round_trips_scaled_rates_without_scientific_notation() {
        for (scaled, text) in [
            (0_i128, "0"),
            (200_000_000, "0.2"),
            (2_000_000_000, "2"),
            (2_500_000_000, "2.5"),
            (1_234_567_890, "1.23456789"),
        ] {
            assert_eq!(rate_string(scaled), text);
        }
    }

    // ── 手工测试回归：下载超时 / 目录搜索 ──────────────────────────────────

    /// models.dev/api.json 实测 5.8MB / 24.05s：旧的 20s 总超时必然失败。
    /// 超时参数从常量来，避免再被写回一个「看起来够用」的小值。
    #[test]
    fn download_timeouts_allow_slow_catalog_and_keep_size_guard() {
        assert_eq!(PRICING_CONNECT_TIMEOUT, Duration::from_secs(10));
        assert_eq!(PRICING_TOTAL_TIMEOUT, Duration::from_secs(120));
        assert!(
            PRICING_TOTAL_TIMEOUT >= Duration::from_secs(30),
            "总超时必须覆盖实测 24.05s 的下载（含抖动余量）"
        );
        assert_eq!(MAX_PRICING_BYTES, 20 * 1024 * 1024, "20MiB 上限不得被放宽");

        // 上限判据与错误文案一起锁死：改上限必须同时改文案。
        assert!(ensure_payload_within_limit(MAX_PRICING_BYTES).is_ok());
        let error = ensure_payload_within_limit(MAX_PRICING_BYTES + 1).unwrap_err();
        assert_eq!(error.code, "usage-internal");
        assert_eq!(error.detail, "pricing download: response exceeds 20 MiB");
    }

    const SEARCH_FIXTURE: &str = r#"{
        "ollama-cloud": {"models": {
            "deepseek-v4.1-flash": {
                "release_date": "2025-09-01",
                "cost": {"input": 1},
                "modalities": {"input": ["text"], "output": ["text"]}
            },
            "kimi-k2": {
                "release_date": "2025-08-01",
                "modalities": {"input": ["text"], "output": ["text"]}
            }
        }},
        "zhipuai-coding-plan": {"models": {
            "glm-5.3": {
                "release_date": "2025-10-01",
                "cost": {"input": 2, "context_over_200k": {"input": 4}},
                "modalities": {"input": ["text"], "output": ["text"]}
            }
        }}
    }"#;

    fn seed_search_catalog(conn: &mut Connection) {
        set_enabled_with_conn(conn, true).unwrap();
        refresh_with_conn(conn, 1_000, false, || {
            Ok(SEARCH_FIXTURE.as_bytes().to_vec())
        })
        .unwrap();
    }

    fn keys(entries: &[PricingCatalogEntry]) -> Vec<(String, String)> {
        entries
            .iter()
            .map(|entry| (entry.provider_id.clone(), entry.model_id.clone()))
            .collect()
    }

    #[test]
    fn catalog_search_is_case_insensitive_on_provider_and_model() {
        let mut conn = mem_db();
        seed_search_catalog(&mut conn);

        for query in ["ollama", "OLLAMA", "Ollama-Cloud"] {
            let entries = catalog_entries_with_conn(&conn, Some(query), 50).unwrap();
            assert_eq!(
                keys(&entries),
                vec![
                    (
                        "ollama-cloud".to_string(),
                        "deepseek-v4.1-flash".to_string()
                    ),
                    ("ollama-cloud".to_string(), "kimi-k2".to_string()),
                ],
                "query={query} 必须命中 provider 的全部模型"
            );
        }
        for query in ["DEEPSEEK", "deepseek-v4.1", "V4.1-FLASH"] {
            let entries = catalog_entries_with_conn(&conn, Some(query), 50).unwrap();
            assert_eq!(
                keys(&entries),
                vec![(
                    "ollama-cloud".to_string(),
                    "deepseek-v4.1-flash".to_string()
                )],
                "query={query} 必须大小写不敏感地命中模型"
            );
        }
        for query in ["GLM-5.3", "zhipuai"] {
            let entries = catalog_entries_with_conn(&conn, Some(query), 50).unwrap();
            assert_eq!(
                keys(&entries),
                vec![("zhipuai-coding-plan".to_string(), "glm-5.3".to_string())],
                "query={query} 必须命中 zhipuai 目录"
            );
        }
        assert!(catalog_entries_with_conn(&conn, Some("not-present"), 50)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn catalog_search_empty_query_is_sorted_and_limited() {
        let mut conn = mem_db();
        seed_search_catalog(&mut conn);

        // query 缺省 / 空白 / 空串等价；按 provider/model 字典序。
        for query in [None, Some(""), Some("   ")] {
            let entries = catalog_entries_with_conn(&conn, query, 50).unwrap();
            assert_eq!(
                keys(&entries),
                vec![
                    (
                        "ollama-cloud".to_string(),
                        "deepseek-v4.1-flash".to_string()
                    ),
                    ("ollama-cloud".to_string(), "kimi-k2".to_string()),
                    ("zhipuai-coding-plan".to_string(), "glm-5.3".to_string()),
                ],
                "query={query:?} 必须返回全部条目且稳定排序"
            );
        }

        // limit clamp：0 → 1，超大 → 上限 200（本夹具只有 3 条）。
        assert_eq!(
            catalog_entries_with_conn(&conn, None, 0).unwrap().len(),
            1,
            "limit=0 必须 clamp 到 1"
        );
        assert_eq!(catalog_entries_with_conn(&conn, None, 2).unwrap().len(), 2);
        assert_eq!(
            catalog_entries_with_conn(&conn, None, 999).unwrap().len(),
            3
        );
    }

    #[test]
    fn catalog_search_exposes_rates_conditional_and_release_date() {
        let mut conn = mem_db();
        seed_search_catalog(&mut conn);

        let entries = catalog_entries_with_conn(&conn, Some("deepseek"), 50).unwrap();
        let entry = &entries[0];
        assert_eq!(entry.provider_id, "ollama-cloud");
        assert_eq!(entry.model_id, "deepseek-v4.1-flash");
        assert_eq!(entry.release_date.as_deref(), Some("2025-09-01"));
        assert!(entry.has_rates);
        assert!(!entry.conditional);

        let free = catalog_entries_with_conn(&conn, Some("kimi"), 50).unwrap();
        assert!(!free[0].has_rates, "缺价条目 hasRates=false");
        assert_eq!(free[0].release_date.as_deref(), Some("2025-08-01"));

        let conditional = catalog_entries_with_conn(&conn, Some("glm"), 50).unwrap();
        assert!(conditional[0].has_rates);
        assert!(
            conditional[0].conditional,
            "未知 cost 键必须保持 conditional=true"
        );
    }

    #[test]
    fn catalog_search_without_snapshot_returns_empty() {
        let conn = mem_db();
        assert!(catalog_entries_with_conn(&conn, None, 50)
            .unwrap()
            .is_empty());
        assert!(
            catalog_entries_with_conn(&conn, Some("deepseek"), 50)
                .unwrap()
                .is_empty(),
            "无快照时搜索也不能报错或编造结果"
        );
    }

    // ── 2026-10-20 计划 Task 3：精选来源（直营官方（含中国站变体）+ Ollama Cloud + OpenRouter）────────────

    const CURATED: &str =
        include_str!("../../../../../tests/fixtures/usage-v2/pricing/curated.json");

    #[test]
    fn curated_constants_match_the_frozen_official_list() {
        assert_eq!(CURATED_OFFICIAL_PROVIDERS.len(), 14);
        for provider in [
            "openai",
            "anthropic",
            "google",
            "deepseek",
            "zhipuai",
            "zhipuai-coding-plan",
            "moonshotai",
            "moonshotai-cn",
            "alibaba",
            "alibaba-cn",
            "minimax-cn",
            "xai",
            "mistral",
            "meta",
        ] {
            assert!(
                CURATED_OFFICIAL_PROVIDERS.contains(&provider),
                "官方集合必须包含 {provider}"
            );
            assert!(is_curated(provider), "{provider} 必须是精选来源");
            assert_eq!(
                curated_kind_of(provider),
                Some("official"),
                "{provider} 的精选分类必须是 official"
            );
        }
        assert_eq!(CURATED_HOSTED_PROVIDERS, ["ollama-cloud"]);
        assert!(is_curated("ollama-cloud"), "Ollama Cloud 是托管来源");
        assert_eq!(
            curated_kind_of("ollama-cloud"),
            Some("hosted"),
            "ollama-cloud 从官方移入托管档（直营官方优先于它）"
        );
        assert!(is_curated("openrouter"), "OpenRouter 是精选兜底来源");
        assert_eq!(curated_kind_of("openrouter"), Some("openrouter"));
        assert_eq!(curated_kind_of("aggregator-x"), None, "非精选来源不得混入");
        assert!(!is_curated("openai-pro"), "前缀命中不算精选（精确匹配）");
        // 大小写敏感：目录 provider id 全小写，原文精确比较。
        assert!(!is_curated("OpenAI"));
    }

    #[test]
    fn curated_index_prefers_official_then_openrouter_and_never_other_sources() {
        let catalog = parse(CURATED).unwrap();
        assert_eq!(
            catalog.provider_count, 4,
            "provider_count = 全量目录 provider 数"
        );

        // 同一归一化 ID 官方/OpenRouter/非精选并存：官方内部按既有去重链选一
        //（google 的 release_date 比 openai 新 ⇒ google 胜），OpenRouter 与
        // 非精选（更新日期最新）都不得翻盘。
        let curated = catalog.normalized_curated.get("gpt-curation").unwrap();
        assert_eq!(curated.reference.provider_id, "google");
        assert_eq!(curated.reference.model_id, "gpt-curation-2025-02-01");
        assert_eq!(curated.rates[0], Some(3_000_000_000));

        // 官方无候选 ⇒ OpenRouter 兜底。
        let or_only = catalog.normalized_curated.get("or-only").unwrap();
        assert_eq!(or_only.reference.provider_id, "openrouter");

        // 两者皆无 ⇒ 不进精选索引（全量索引不受影响）。
        assert!(
            !catalog.normalized_curated.contains_key("agg-only"),
            "非精选来源不得进入精选索引"
        );
        assert_eq!(
            catalog
                .normalized
                .get("agg-only")
                .unwrap()
                .reference
                .provider_id,
            "aggregator-x",
            "全量索引仍按全量去重链选中非精选来源"
        );
    }

    #[test]
    fn match_model_scoped_runs_in_the_chosen_index_only() {
        let catalog = parse(CURATED).unwrap();

        // Curated：非精选条目不命中（同归一化 ID 有官方候选时走官方）。
        let curated_hit = match_model_scoped(&catalog, "gpt-curation", MatchScope::Curated);
        assert_eq!(curated_hit.kind, ModelMatchKind::Exact);
        assert_eq!(
            curated_hit.quote.unwrap().reference.provider_id,
            "google",
            "精选自动匹配官方优先"
        );

        let curated_miss = match_model_scoped(&catalog, "agg-only", MatchScope::Curated);
        assert_eq!(
            curated_miss.kind,
            ModelMatchKind::None,
            "非精选来源在 Curated 作用域必须不命中"
        );
        assert!(curated_miss.quote.is_none());

        // All：全量口径与旧行为一致（agg-only 命中 aggregator-x）。
        let all_hit = match_model_scoped(&catalog, "agg-only", MatchScope::All);
        assert_eq!(all_hit.kind, ModelMatchKind::Exact);
        assert_eq!(all_hit.quote.unwrap().reference.provider_id, "aggregator-x");
    }

    // ── 2026-10-20 用户裁决：精选三档优先级（直营官方 > ollama-cloud > OpenRouter）────

    const CURATED_HOSTED: &str =
        include_str!("../../../../../tests/fixtures/usage-v2/pricing/curated-hosted.json");

    #[test]
    fn curated_index_ranks_official_above_hosted_above_openrouter() {
        let catalog = parse(CURATED_HOSTED).unwrap();
        assert_eq!(catalog.provider_count, 4);

        // glm-5.3 四家并存（zhipuai 官方 / ollama-cloud 托管 / openrouter / aggregator-x，
        // release_date 依次更新）：精选索引必须取 zhipuai——智谱行不再取到 Ollama Cloud 的价。
        let curated = catalog.normalized_curated.get("glm-5-3").unwrap();
        assert_eq!(curated.reference.provider_id, "zhipuai");
        assert_eq!(curated.reference.model_id, "glm-5.3");
        assert_eq!(curated.rates[0], Some(1_000_000_000));
        assert_eq!(curated.rates[3], Some(8_000_000_000));

        // 仅 hosted（无直营官方）⇒ ollama-cloud 胜 OpenRouter（release_date 更新也不翻盘）。
        let hosted = catalog
            .normalized_curated
            .get("deepseek-v4-1-flash")
            .unwrap();
        assert_eq!(hosted.reference.provider_id, "ollama-cloud");
        assert_eq!(hosted.reference.model_id, "deepseek-v4.1-flash");

        // 全量索引不受三档影响：仍按去重链选 release_date 最新的 aggregator-x。
        let all = catalog.normalized.get("glm-5-3").unwrap();
        assert_eq!(all.reference.provider_id, "aggregator-x");

        // 自动匹配（恒为精选作用域）同样取直营官方。
        let matched = match_model_scoped(&catalog, "GLM-5.3", MatchScope::Curated);
        assert_eq!(matched.kind, ModelMatchKind::Exact);
        assert_eq!(
            matched.quote.unwrap().reference.provider_id,
            "zhipuai",
            "自动匹配 glm-5.3 必须取 zhipuai 而不是 ollama-cloud"
        );
        let hosted_match = match_model_scoped(&catalog, "deepseek-v4.1-flash", MatchScope::Curated);
        assert_eq!(hosted_match.kind, ModelMatchKind::Exact);
        assert_eq!(
            hosted_match.quote.unwrap().reference.provider_id,
            "ollama-cloud",
            "仅 hosted 的模型落到 ollama-cloud（优先于 OpenRouter）"
        );
    }

    /// 前缀回退在**所选作用域索引内**生效：候选来自同一索引，不得跨作用域拼候选。
    #[test]
    fn prefix_fallback_stays_inside_the_selected_scope() {
        // 官方前缀候选：精选索引内回退成功。
        let official = r#"{
            "openai": {"models": {
                "gpt-prefix-x": {"id": "gpt-prefix-x", "cost": {"input": 1},
                    "modalities": {"input": ["text"], "output": ["text"]}}
            }},
            "aggregator-y": {"models": {
                "gpt-prefix-y": {"id": "gpt-prefix-y", "cost": {"input": 2},
                    "modalities": {"input": ["text"], "output": ["text"]}}
            }}
        }"#;
        let catalog = parse(official).unwrap();
        let curated = match_model_scoped(&catalog, "gpt-prefix-x-beta", MatchScope::Curated);
        assert_eq!(curated.kind, ModelMatchKind::Prefix);
        assert_eq!(curated.quote.unwrap().reference.model_id, "gpt-prefix-x");

        let miss = match_model_scoped(&catalog, "gpt-prefix-y-beta", MatchScope::Curated);
        assert_eq!(
            miss.kind,
            ModelMatchKind::None,
            "前缀候选只在精选索引内找：非精选候选不得回退"
        );
        let all = match_model_scoped(&catalog, "gpt-prefix-y-beta", MatchScope::All);
        assert_eq!(all.kind, ModelMatchKind::Prefix, "All 口径照常回退");

        // 歧义判定也在作用域内：两个官方同族异价候选（"qwen" 与 "qwen-alpha" 都能
        // 以噪声后缀命中 "qwen-alpha-alpha"）在精选索引内判歧义。
        let ambiguous = r#"{
            "openai": {"models": {
                "qwen": {"id": "qwen", "cost": {"input": 1},
                    "modalities": {"input": ["text"], "output": ["text"]}},
                "qwen-alpha": {"id": "qwen-alpha", "cost": {"input": 2},
                    "modalities": {"input": ["text"], "output": ["text"]}}
            }}
        }"#;
        let catalog = parse(ambiguous).unwrap();
        let scoped = match_model_scoped(&catalog, "qwen-alpha-alpha", MatchScope::Curated);
        assert_eq!(scoped.kind, ModelMatchKind::None);
        assert!(scoped.prefix_ambiguous, "作用域内的多候选异价歧义保持不变");
    }

    fn seed_curated_catalog(conn: &mut Connection) {
        set_enabled_with_conn(conn, true).unwrap();
        refresh_with_conn(conn, 1_000, false, || Ok(CURATED.as_bytes().to_vec())).unwrap();
    }

    #[test]
    fn catalog_search_scoped_filters_entries_and_keeps_total_provider_count() {
        let mut conn = mem_db();
        seed_curated_catalog(&mut conn);

        // curated：只回直营官方（含中国站变体）+ Ollama Cloud + OpenRouter 的**全部条目**（搜索不走归一化去重）；
        // 条目带 curatedKind。
        let curated =
            catalog_search_scoped_with_conn(&conn, None, 50, MatchScope::Curated).unwrap();
        assert_eq!(
            curated.provider_count, 4,
            "providerCount 恒为全量目录 provider 数"
        );
        let kinds: Vec<(String, String, String)> = curated
            .entries
            .iter()
            .map(|entry| {
                (
                    entry.provider_id.clone(),
                    entry.model_id.clone(),
                    entry.curated_kind.clone().unwrap_or_default(),
                )
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                (
                    "google".to_string(),
                    "gpt-curation-2025-02-01".to_string(),
                    "official".to_string()
                ),
                (
                    "openai".to_string(),
                    "gpt-curation".to_string(),
                    "official".to_string()
                ),
                (
                    "openrouter".to_string(),
                    "gpt-curation".to_string(),
                    "openrouter".to_string()
                ),
                (
                    "openrouter".to_string(),
                    "or-only".to_string(),
                    "openrouter".to_string()
                ),
            ],
            "curated 作用域只含官方 + OpenRouter 条目（aggregator-x 被过滤）"
        );

        // all：全量条目，非精选 curatedKind=null，providerCount 不变。
        let all = catalog_search_scoped_with_conn(&conn, None, 50, MatchScope::All).unwrap();
        assert_eq!(all.provider_count, 4);
        let agg = all
            .entries
            .iter()
            .find(|entry| entry.provider_id == "aggregator-x")
            .expect("all 作用域必须含非精选条目");
        assert_eq!(agg.curated_kind, None);
        let or_entry = all
            .entries
            .iter()
            .find(|entry| entry.provider_id == "openrouter")
            .unwrap();
        assert_eq!(or_entry.curated_kind.as_deref(), Some("openrouter"));

        // 子串过滤与 limit clamp 与全量搜索同规则。
        let filtered =
            catalog_search_scoped_with_conn(&conn, Some("OR-ONLY"), 50, MatchScope::Curated)
                .unwrap();
        assert_eq!(filtered.entries.len(), 1, "大小写不敏感子串过滤");
        assert_eq!(
            catalog_search_scoped_with_conn(&conn, None, 1, MatchScope::Curated)
                .unwrap()
                .entries
                .len(),
            1,
            "limit=1 生效"
        );

        // 作用域非法：固定 detail。
        let err = MatchScope::parse("everything").unwrap_err();
        assert_eq!(err.code, "usage-settings-invalid");
        assert_eq!(err.detail, "pricing catalog scope is invalid");
    }

    #[test]
    fn catalog_search_scoped_without_snapshot_returns_empty_with_zero_count() {
        let conn = mem_db();
        let search = catalog_search_scoped_with_conn(&conn, None, 50, MatchScope::Curated).unwrap();
        assert!(search.entries.is_empty());
        assert_eq!(search.provider_count, 0, "无快照时 N=0（不是编造 232）");
    }

    #[test]
    fn quote_preview_returns_exact_rates_and_snapshot_date() {
        let mut conn = mem_db();
        seed_curated_catalog(&mut conn);

        let quote = quote_preview_with_conn(&conn, "openai", "gpt-curation", None)
            .unwrap()
            .expect("精确键必须命中");
        assert_eq!(quote.reference.provider_id, "openai");
        assert_eq!(quote.reference.model_id, "gpt-curation");
        assert_eq!(quote.input.as_deref(), Some("1"));
        assert_eq!(quote.cache_read, None);
        assert_eq!(quote.cache_write, None);
        assert_eq!(quote.output, None);
        assert_eq!(quote.release_date.as_deref(), Some("2025-01-01"));
        assert!(!quote.conditional);
        assert_eq!(quote.snapshot_date, Some(1_000));

        // 条件价条目（modalities 非纯文本）保留 conditional 标记。
        let conditional = quote_preview_with_conn(&conn, "openai", "missing-model", None).unwrap();
        assert_eq!(conditional, None, "目录无该精确键 ⇒ null（不是错误）");
    }

    #[test]
    fn quote_preview_without_snapshot_is_none() {
        let conn = mem_db();
        assert_eq!(
            quote_preview_with_conn(&conn, "openai", "m", None).unwrap(),
            None
        );
    }

    #[test]
    fn auto_match_is_fixed_to_curated_scope() {
        let mut conn = mem_db();
        seed_curated_catalog(&mut conn);

        let hit = auto_match_with_conn(&conn, "gpt-curation", None).unwrap();
        assert_eq!(hit.kind, "exact");
        let reference = hit.reference.expect("命中必须有 reference");
        assert_eq!(reference.provider_id, "google", "自动匹配固定精选作用域");
        let quote = hit.quote.expect("命中必须有价格预览");
        assert_eq!(quote.snapshot_date, Some(1_000));

        let prefix = auto_match_with_conn(&conn, "or-only-beta", None).unwrap();
        assert_eq!(prefix.kind, "prefix");
        assert_eq!(prefix.reference.unwrap().provider_id, "openrouter");

        let miss = auto_match_with_conn(&conn, "agg-only", None).unwrap();
        assert_eq!(miss.kind, "none");
        assert!(miss.reference.is_none() && miss.quote.is_none());
    }

    // ── 2026-10-20：原生人民币价 / 汇率随快照 GC 的生命周期 ─────────────────────

    fn native_record(snapshot_native_model: &str) -> dao::NativePriceRecord {
        dao::NativePriceRecord {
            provider_id: "zhipuai".to_string(),
            model_id: snapshot_native_model.to_string(),
            normalized_model_id: normalize_model_id(snapshot_native_model),
            rates: [
                Some(8_000_000_000),
                Some(2_000_000_000),
                Some(0),
                Some(28_000_000_000),
            ],
            source_url: "https://example.test/zhipuai/glm.toml".to_string(),
            fetched_at: 1,
        }
    }

    #[test]
    fn snapshot_gc_keeps_native_and_fx_of_active_plus_latest_history() {
        const URL: &str = "https://models.dev/api.json";
        let mut conn = mem_db();
        set_enabled_with_conn(&conn, true).unwrap();

        // 第 1 次激活：active=snap-1，种 native + fx。
        dao::activate_snapshot_conn(&mut conn, "snap-1", URL, 1_000, "{}").unwrap();
        dao::replace_native_prices_conn(&mut conn, "snap-1", &[native_record("glm-5.3")]).unwrap();
        dao::upsert_fx_conn(&conn, "snap-1", 7_000_000_000, 1_000, URL).unwrap();

        // 第 2 次激活：active=snap-2，历史保留 = snap-1 ⇒ 谁都不能删。
        dao::activate_snapshot_conn(&mut conn, "snap-2", URL, 2_000, "{}").unwrap();
        dao::replace_native_prices_conn(&mut conn, "snap-2", &[native_record("glm-5.3")]).unwrap();
        dao::upsert_fx_conn(&conn, "snap-2", 7_100_000_000, 2_000, URL).unwrap();
        let native_of = |conn: &Connection, id: &str| -> i64 {
            conn.query_row(
                "SELECT COUNT(*) FROM usage_pricing_native WHERE snapshot_id = ?1",
                [id],
                |row| row.get(0),
            )
            .unwrap()
        };
        let fx_of = |conn: &Connection, id: &str| -> i64 {
            conn.query_row(
                "SELECT COUNT(*) FROM usage_pricing_fx WHERE snapshot_id = ?1",
                [id],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(native_of(&conn, "snap-1"), 1);
        assert_eq!(fx_of(&conn, "snap-1"), 1);

        // 第 3 次激活：active=snap-3 ⇒ GC 必须连带删掉 snap-1（比最近历史 snap-2 更老）
        // 的 native/fx 行；active 与最近历史两档保留。
        dao::activate_snapshot_conn(&mut conn, "snap-3", URL, 3_000, "{}").unwrap();
        dao::replace_native_prices_conn(&mut conn, "snap-3", &[native_record("glm-5.3")]).unwrap();
        dao::upsert_fx_conn(&conn, "snap-3", 7_200_000_000, 3_000, URL).unwrap();
        for survivor in ["snap-3", "snap-2"] {
            assert_eq!(
                native_of(&conn, survivor),
                1,
                "{survivor} 的 native 行必须存活"
            );
            assert_eq!(fx_of(&conn, survivor), 1, "{survivor} 的 fx 行必须存活");
        }
        assert_eq!(
            native_of(&conn, "snap-1") + fx_of(&conn, "snap-1"),
            0,
            "比最近历史更老的快照，native/fx 行必须随 GC 删除"
        );
        // 快照本体同样是两档：active + 最近历史。
        let snapshots: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage_pricing_snapshot", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(snapshots, 2);
    }
}
// ==== usage 自省锁：排除区结束，勿删勿复制 ====
