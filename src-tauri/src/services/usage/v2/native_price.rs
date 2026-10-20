//! 原生人民币价与当日汇率（2026-10-20 用户裁决）。
//!
//! models.dev 的 `api.json` 只给 USD；中国厂商的官方人民币价写在其源 TOML 的注释里
//! （`# List price: CNY 8/2/28` / `# CNY per 1M tokens: …` / `# input ¥…`）。
//! 刷新价格快照时，只对**账本确实观测到、且能被精选目录匹配到人民币来源**的模型
//! 拉取对应 TOML，解析出明确写出的四价；解析失败/没有人民币数值的条目不落库，
//! 费用查询会按当日汇率换算并标 `converted`。
//!
//! 纪律：
//! * 本模块**只写** `usage_pricing_native` / `usage_pricing_fx` 两张随快照生命周期的表，
//!   绝不触碰账本、覆盖价或 billing 配置；
//! * 网络失败是**可降级的**：USD 快照已经激活，人民币价/汇率缺失只影响 CNY 显示，
//!   不得让整次刷新失败；
//! * 只接受注释里明确写出的数值，不按相似度猜价、不用源注释里的历史汇率反推人民币价。

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rusqlite::Connection;
use serde::Deserialize;
use serde_json::value::RawValue;

use crate::database::dao::usage_billing::{FxRecord, NativePriceRecord};
use crate::services::usage::range::day_key_of_host;
use crate::services::usage::v2::pricing::{normalize_model_id, parse_rate, PriceCatalog};
use crate::services::usage::UsageError;

/// 明确以人民币定价、且源 TOML 里可能写出人民币数值的精选来源。
///
/// 注意：这里刻意**不包含** `moonshotai` / `alibaba`（国际站以 USD 定价）与
/// `zhipuai-coding-plan`（套餐零价）；它们若需要人民币显示，会走当日汇率换算。
pub const CNY_SOURCE_PROVIDERS: [&str; 5] = [
    "zhipuai",
    "moonshotai-cn",
    "alibaba-cn",
    "minimax-cn",
    "deepseek",
];

/// 单次刷新最多拉取的 TOML 数（按最近 7 天用量降序取前 N，控制网络/时延）。
const MAX_NATIVE_TARGETS: usize = 32;
/// 单个 TOML 请求上限（源文件都是几 KB）。
const MAX_TOML_BYTES: usize = 1024 * 1024;
const TOML_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const TOML_TOTAL_TIMEOUT: Duration = Duration::from_secs(10);
/// 并发拉取 TOML 的线程数。
const TOML_WORKERS: usize = 4;
/// 整批 TOML 的总预算（到点不再发起新请求；已在飞的请求受单请求超时约束）。
const TOML_BATCH_BUDGET: Duration = Duration::from_secs(45);

pub const FX_SOURCE_URL: &str = "https://open.er-api.com/v6/latest/USD";
const MAX_FX_BYTES: usize = 256 * 1024;
const FX_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const FX_TOTAL_TIMEOUT: Duration = Duration::from_secs(15);

fn db_failed(context: &str, error: impl std::fmt::Display) -> UsageError {
    UsageError::new("usage-db-failed", format!("{context}: {error}"))
}

fn source_url(provider_id: &str, model_id: &str) -> String {
    format!(
        "https://raw.githubusercontent.com/sst/models.dev/dev/providers/{provider_id}/models/{model_id}.toml"
    )
}

fn fallback_source_url(provider_id: &str, model_id: &str) -> String {
    format!(
        "https://cdn.jsdelivr.net/gh/sst/models.dev@dev/providers/{provider_id}/models/{model_id}.toml"
    )
}

fn http_client() -> Result<reqwest::blocking::Client, UsageError> {
    reqwest::blocking::Client::builder()
        .connect_timeout(TOML_CONNECT_TIMEOUT)
        .timeout(TOML_TOTAL_TIMEOUT)
        .build()
        .map_err(|e| db_failed("native price client build failed", e))
}

fn read_limited(
    response: &mut reqwest::blocking::Response,
    max: usize,
    what: &str,
) -> Result<Vec<u8>, UsageError> {
    let mut payload = Vec::new();
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let read = response
            .read(&mut buffer)
            .map_err(|e| db_failed(&format!("{what} read failed"), e))?;
        if read == 0 {
            break;
        }
        payload.extend_from_slice(&buffer[..read]);
        if payload.len() > max {
            return Err(UsageError::internal(format!("{what} exceeds size limit")));
        }
    }
    Ok(payload)
}

fn fetch_url(
    client: &reqwest::blocking::Client,
    url: &str,
    max: usize,
) -> Result<String, UsageError> {
    let mut response = client
        .get(url)
        .send()
        .map_err(|e| db_failed("native price download failed", e))?;
    let status = response.status();
    if !status.is_success() {
        return Err(UsageError::internal(format!(
            "native price download failed: HTTP {status}"
        )));
    }
    let bytes = read_limited(&mut response, max, "native price download")?;
    String::from_utf8(bytes)
        .map_err(|_| UsageError::internal("native price download is not UTF-8".to_string()))
}

fn download_toml(
    client: &reqwest::blocking::Client,
    provider_id: &str,
    model_id: &str,
) -> Result<(String, String), UsageError> {
    let primary = source_url(provider_id, model_id);
    match fetch_url(client, &primary, MAX_TOML_BYTES) {
        Ok(text) => Ok((text, primary)),
        Err(primary_error) => {
            let fallback = fallback_source_url(provider_id, model_id);
            match fetch_url(client, &fallback, MAX_TOML_BYTES) {
                Ok(text) => Ok((text, fallback)),
                Err(fallback_error) => Err(UsageError::internal(format!(
                    "native price download failed ({}): {}; fallback: {}",
                    primary, primary_error.detail, fallback_error.detail
                ))),
            }
        }
    }
}

/// 从 TOML 注释解析明确写出的人民币四价。
///
/// 支持三种已观测格式（大小写敏感按源文件原样）：
/// 1. `# List price: CNY 8/2/28 (input/cache read/output per MTok)`
///    —— 3 个数依次 input/cache read/output；缓存写按源文件 `cache_write = 0` 视为免费；
/// 2. `# CNY per 1M tokens: 2.1 in / 8.4 out / 0.42 cache read / 2.625 cache write`
///    —— 按 `in` / `out` / `cache read` / `cache write` 标记取数；
/// 3. `# input ¥20.00, cache hit ¥2.00, cache write ¥20.00 (5m) / ¥40.00 (1h), output ¥100.00`
///    —— 按 `input ¥` / `cache hit ¥` / `cache write ¥` / `output ¥` 取第一个数。
///
/// 返回顺序固定 `[input, cache_read, cache_write, output]`；至少一项非空。
pub fn parse_native_cny(toml: &str) -> Option<[Option<i128>; 4]> {
    for raw_line in toml.lines() {
        let line = raw_line.trim_start();
        if !line.starts_with('#') {
            continue;
        }
        let line = line.trim_start_matches('#').trim();
        if let Some(rates) = parse_list_price_line(line) {
            return Some(rates);
        }
        if let Some(rates) = parse_cny_per_million_line(line) {
            return Some(rates);
        }
        if let Some(rates) = parse_yen_field_line(line) {
            return Some(rates);
        }
    }
    None
}

fn parse_list_price_line(line: &str) -> Option<[Option<i128>; 4]> {
    let lower = line.to_lowercase();
    let marker = "list price: cny";
    let start = lower.find(marker)? + marker.len();
    let rest = line.get(start..)?.trim_start();
    let token = rest
        .split(|ch: char| ch.is_whitespace() || ch == '(' || ch == '（')
        .next()?
        .trim()
        .trim_end_matches([',', ';', '，', '；', '。']);
    let parts: Vec<&str> = token.split('/').collect();
    if parts.len() != 3 {
        return None;
    }
    let input = parse_rate(parts[0].trim())?;
    let cache_read = parse_rate(parts[1].trim())?;
    let output = parse_rate(parts[2].trim())?;
    // 该格式只列三项；源文件对应的 `cache_write` 为 0（缓存写入免费）。
    Some([Some(input), Some(cache_read), Some(0), Some(output)])
}

fn parse_cny_per_million_line(line: &str) -> Option<[Option<i128>; 4]> {
    let lower = line.to_lowercase();
    if !lower.starts_with("cny per 1m tokens") {
        return None;
    }
    let input = number_before(line, " in");
    let output = number_before(line, " out");
    let cache_read = number_before(line, " cache read");
    let cache_write = number_before(line, " cache write");
    if input.is_none() && output.is_none() && cache_read.is_none() && cache_write.is_none() {
        return None;
    }
    Some([input, cache_read, cache_write, output])
}

fn parse_yen_field_line(line: &str) -> Option<[Option<i128>; 4]> {
    let lower = line.to_lowercase();
    if !lower.contains("input ¥") || !lower.contains("output ¥") {
        return None;
    }
    let input = number_after(line, "input ¥");
    let cache_read = number_after(line, "cache hit ¥");
    let cache_write = number_after(line, "cache write ¥");
    let output = number_after(line, "output ¥");
    Some([input, cache_read, cache_write, output])
}

fn number_after(text: &str, marker: &str) -> Option<i128> {
    let lower = text.to_lowercase();
    let start = lower.find(&marker.to_lowercase())? + marker.len();
    let rest = text.get(start..)?;
    let token: String = rest
        .chars()
        .skip_while(|ch| ch.is_whitespace())
        .take_while(|ch| ch.is_ascii_digit() || *ch == '.')
        .collect();
    parse_rate(token.trim_matches('.'))
}

fn number_before(text: &str, marker: &str) -> Option<i128> {
    let lower = text.to_lowercase();
    let end = lower.find(&marker.to_lowercase())?;
    let head = text.get(..end)?;
    let token: String = head
        .chars()
        .rev()
        .skip_while(|ch| ch.is_whitespace())
        .take_while(|ch| ch.is_ascii_digit() || *ch == '.')
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    parse_rate(token.trim_matches('.'))
}

/// 观测到的归一化模型 → 最近 7 天用量（用于排序与截断）。
fn observed_models_with_conn(conn: &Connection, now_ms: i64) -> Result<Vec<String>, UsageError> {
    let cutoff = day_key_of_host(now_ms.saturating_sub(6 * 86_400_000));
    let mut stmt = conn
        .prepare(
            "SELECT model,
                    SUM(CASE WHEN day_key >= ?1 THEN request_total + output ELSE 0 END) AS recent
             FROM usage_daily
             GROUP BY model
             ORDER BY recent DESC, model ASC",
        )
        .map_err(|e| db_failed("native price observed select failed", e))?;
    let rows = stmt
        .query_map([cutoff], |row| row.get::<_, String>(0))
        .map_err(|e| db_failed("native price observed select failed", e))?;
    let mut models = Vec::new();
    let mut seen = BTreeSet::new();
    for row in rows {
        let model = row.map_err(|e| db_failed("native price observed read failed", e))?;
        let normalized = normalize_model_id(&model);
        if normalized.is_empty() || !seen.insert(normalized.clone()) {
            continue;
        }
        models.push(normalized);
    }
    // 手动计价身份覆盖也可能指向 CNY 来源；即使该模型当前没有账本行也补上。
    let overrides = crate::services::usage::v2::pricing::load_effective_overrides_with_conn(conn)?;
    for normalized in overrides.keys() {
        if !normalized.is_empty() && seen.insert(normalized.clone()) {
            models.push(normalized.clone());
        }
    }
    Ok(models)
}

/// 选取需要拉取人民币价的 `(provider, model)`：目录里存在、来源属于 CNY_SOURCE_PROVIDERS、
/// 且归一化模型在观测集合内。按观测顺序（近 7 天用量降序）截断到 32 个。
pub fn native_targets(catalog: &PriceCatalog, observed: &[String]) -> Vec<(String, String)> {
    let wanted: BTreeMap<&str, usize> = observed
        .iter()
        .enumerate()
        .map(|(index, model)| (model.as_str(), index))
        .collect();
    let mut targets: Vec<(usize, String, String)> = Vec::new();
    for quote in catalog.quotes.values() {
        if !CNY_SOURCE_PROVIDERS.contains(&quote.reference.provider_id.as_str()) {
            continue;
        }
        let Some(rank) = wanted.get(quote.normalized_model_id.as_str()) else {
            continue;
        };
        targets.push((
            *rank,
            quote.reference.provider_id.clone(),
            quote.reference.model_id.clone(),
        ));
    }
    targets.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.1.cmp(&right.1))
            .then_with(|| left.2.cmp(&right.2))
    });
    targets.dedup_by(|left, right| left.1 == right.1 && left.2 == right.2);
    targets
        .into_iter()
        .take(MAX_NATIVE_TARGETS)
        .map(|(_, provider, model)| (provider, model))
        .collect()
}

/// 选取某快照要拉取的目标（需要连接读观测模型；**不持锁下载**）。
pub fn native_targets_with_conn(
    conn: &Connection,
    catalog: &PriceCatalog,
    now_ms: i64,
) -> Result<Vec<(String, String)>, UsageError> {
    let observed = observed_models_with_conn(conn, now_ms)?;
    Ok(native_targets(catalog, &observed))
}

/// 并发拉取目标 TOML 并解析（best-effort；无数据库访问，可在持锁外调用）。
pub fn download_native_prices(
    targets: &[(String, String)],
    now_ms: i64,
) -> Result<Vec<NativePriceRecord>, UsageError> {
    if targets.is_empty() {
        return Ok(Vec::new());
    }
    let client = Arc::new(http_client()?);
    let queue = Arc::new(Mutex::new(
        targets
            .iter()
            .cloned()
            .collect::<std::collections::VecDeque<_>>(),
    ));
    let results = Arc::new(Mutex::new(Vec::<NativePriceRecord>::new()));
    let deadline = Instant::now() + TOML_BATCH_BUDGET;

    std::thread::scope(|scope| {
        for _ in 0..TOML_WORKERS {
            let client = Arc::clone(&client);
            let queue = Arc::clone(&queue);
            let results = Arc::clone(&results);
            scope.spawn(move || loop {
                if Instant::now() >= deadline {
                    break;
                }
                let next = {
                    let mut queue = queue.lock().unwrap_or_else(|e| e.into_inner());
                    queue.pop_front()
                };
                let Some((provider_id, model_id)) = next else {
                    break;
                };
                let fetched = download_toml(&client, &provider_id, &model_id);
                match fetched {
                    Ok((text, url)) => {
                        let Some(rates) = parse_native_cny(&text) else {
                            continue;
                        };
                        let normalized_model_id = normalize_model_id(&model_id);
                        if normalized_model_id.is_empty() {
                            continue;
                        }
                        results
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .push(NativePriceRecord {
                                provider_id,
                                model_id,
                                normalized_model_id,
                                rates,
                                source_url: url,
                                fetched_at: now_ms,
                            });
                    }
                    Err(error) => {
                        log::warn!(
                            "pricing native: {provider_id}/{model_id} unavailable: {error:?}"
                        );
                    }
                }
            });
        }
    });

    Ok(Arc::try_unwrap(results)
        .map(|mutex| mutex.into_inner().unwrap_or_else(|e| e.into_inner()))
        .unwrap_or_else(|shared| shared.lock().unwrap_or_else(|e| e.into_inner()).clone()))
}

#[derive(Deserialize)]
struct FxResponse {
    rates: BTreeMap<String, Box<RawValue>>,
}

pub fn download_fx(now_ms: i64) -> Result<FxRecord, UsageError> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(FX_CONNECT_TIMEOUT)
        .timeout(FX_TOTAL_TIMEOUT)
        .build()
        .map_err(|e| db_failed("pricing fx client build failed", e))?;
    let mut response = client
        .get(FX_SOURCE_URL)
        .send()
        .map_err(|e| db_failed("pricing fx download failed", e))?;
    let status = response.status();
    if !status.is_success() {
        return Err(UsageError::internal(format!(
            "pricing fx download failed: HTTP {status}"
        )));
    }
    let bytes = read_limited(&mut response, MAX_FX_BYTES, "pricing fx download")?;
    let parsed: FxResponse =
        serde_json::from_slice(&bytes).map_err(|e| db_failed("pricing fx parse failed", e))?;
    let raw = parsed
        .rates
        .get("CNY")
        .ok_or_else(|| UsageError::internal("pricing fx response missing CNY".to_string()))?;
    let rate = parse_rate(raw.get()).ok_or_else(|| {
        UsageError::internal("pricing fx response has invalid CNY rate".to_string())
    })?;
    if rate <= 0 {
        return Err(UsageError::internal(
            "pricing fx response has non-positive CNY rate".to_string(),
        ));
    }
    Ok(FxRecord {
        rate_scaled: rate,
        fetched_at: now_ms,
        source_url: FX_SOURCE_URL.to_string(),
    })
}

/// 刷新后拉取并落库当日汇率（best-effort；失败由费用层回退 cached/default）。
// ==== usage 自省锁：以下为排除区（测试代码），勿删勿复制 ====
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_zhipu_list_price_comment() {
        let toml = "# List price: CNY 8/2/28 (input/cache read/output per MTok)\n";
        assert_eq!(
            parse_native_cny(toml),
            Some([
                Some(8_000_000_000),
                Some(2_000_000_000),
                Some(0),
                Some(28_000_000_000)
            ])
        );
    }

    #[test]
    fn parses_moonshot_cny_per_million_comment() {
        let toml = "# CNY per 1M tokens: 2.1 in / 8.4 out / 0.42 cache read / 2.625 cache write\n";
        assert_eq!(
            parse_native_cny(toml),
            Some([
                Some(2_100_000_000),
                Some(420_000_000),
                Some(2_625_000_000),
                Some(8_400_000_000)
            ])
        );
    }

    #[test]
    fn parses_yen_field_comment() {
        let toml = "# (accessed 2026-10-19): input ¥20.00, cache hit ¥2.00, cache write ¥20.00 (5m) / ¥40.00 (1h), output ¥100.00.\n";
        assert_eq!(
            parse_native_cny(toml),
            Some([
                Some(20_000_000_000),
                Some(2_000_000_000),
                Some(20_000_000_000),
                Some(100_000_000_000)
            ])
        );
    }

    #[test]
    fn ignores_rate_only_and_non_comment_lines() {
        let toml = "# CNY→USD rate: 6.693661, 2026-10-19\n[cost.text.input]\nprice = 1.19516\n";
        assert_eq!(parse_native_cny(toml), None);
    }

    #[test]
    fn native_targets_are_ordered_by_observed_rank_and_limited_to_cny_sources() {
        use crate::services::usage::v2::model::PricingReference;
        use crate::services::usage::v2::pricing::{PriceCatalog, PriceQuote};
        use std::collections::BTreeMap;

        let quote = |provider: &str, model: &str| PriceQuote {
            reference: PricingReference {
                provider_id: provider.to_string(),
                model_id: model.to_string(),
            },
            rates: [Some(1), None, None, None],
            conditional: false,
            release_date: None,
            normalized_model_id: normalize_model_id(model),
        };
        let mut quotes = BTreeMap::new();
        quotes.insert(
            ("zhipuai".to_string(), "glm-5.3".to_string()),
            quote("zhipuai", "glm-5.3"),
        );
        quotes.insert(
            ("ollama-cloud".to_string(), "glm-5.3".to_string()),
            quote("ollama-cloud", "glm-5.3"),
        );
        quotes.insert(
            ("moonshotai-cn".to_string(), "kimi-k3".to_string()),
            quote("moonshotai-cn", "kimi-k3"),
        );
        let catalog = PriceCatalog {
            quotes,
            normalized: BTreeMap::new(),
            normalized_curated: BTreeMap::new(),
            provider_count: 3,
        };
        let targets = native_targets(
            &catalog,
            &[normalize_model_id("kimi-k3"), normalize_model_id("glm-5.3")],
        );
        assert_eq!(
            targets,
            vec![
                ("moonshotai-cn".to_string(), "kimi-k3".to_string()),
                ("zhipuai".to_string(), "glm-5.3".to_string())
            ]
        );
    }
}
// ==== usage 自省锁：排除区结束，勿删勿复制 ====
