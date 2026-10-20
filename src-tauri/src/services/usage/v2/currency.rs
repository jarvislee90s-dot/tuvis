//! 费用域的币种解析（2026-10-20 用户裁决）。
//!
//! * 原生来源：models.dev 的 USD 四价 + 源 TOML 的人民币四价（`usage_pricing_native`）；
//! * 显示 USD → 优先 USD 原生价；显示 CNY → 优先人民币原生价；
//!   另一币种缺失时用**当次快照的当日汇率**换算，并逐分量标记 `converted`；
//! * 汇率解析顺序：active 快照的 `usage_pricing_fx` → 最近一次成功（cached）→
//!   设置页备用汇率（settings）→ 内置默认 7.2（default）；后三者均 `fallback = true`。
//!
//! 纪律：本模块只做**展示口径**换算，绝不回写账本、目录快照或覆盖价。

use rusqlite::Connection;

use crate::database::dao::usage_billing as dao;
use crate::services::usage::v2::model::PriceCurrency;
use crate::services::usage::v2::pricing::{parse_rate, rate_string};
use crate::services::usage::UsageError;

/// 内置兜底汇率（仅当快照、缓存、设置页都没有有效值时使用）。
pub const DEFAULT_FX_RATE: &str = "7.2";

/// 当日 USD→CNY 汇率（`rate_scaled` 单位 10^-9 CNY / USD）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FxRate {
    pub rate_scaled: i128,
    pub rate_text: String,
    pub fetched_at: Option<i64>,
    /// `"er-api"` | `"cached"` | `"settings"` | `"default"`。
    pub source: &'static str,
    pub fallback: bool,
}

/// 解析本次查询使用的汇率。
///
/// `fallback_rate` 来自设置页（前端 localStorage），只在没有任何已保存汇率时使用；
/// 非法值忽略，继续落到内置默认。
pub fn resolve_fx_with_conn(
    conn: &Connection,
    snapshot_id: Option<&str>,
    fallback_rate: Option<&str>,
) -> Result<FxRate, UsageError> {
    if let Some(snapshot_id) = snapshot_id {
        if let Some(fx) = dao::load_fx_conn(conn, snapshot_id)? {
            return Ok(FxRate {
                rate_scaled: fx.rate_scaled,
                rate_text: rate_string(fx.rate_scaled),
                fetched_at: Some(fx.fetched_at),
                source: "er-api",
                fallback: false,
            });
        }
    }
    if let Some(fx) = dao::load_latest_fx_conn(conn)? {
        return Ok(FxRate {
            rate_scaled: fx.rate_scaled,
            rate_text: rate_string(fx.rate_scaled),
            fetched_at: Some(fx.fetched_at),
            source: "cached",
            fallback: true,
        });
    }
    if let Some(raw) = fallback_rate {
        if let Some(rate) = parse_rate(raw.trim()).filter(|rate| *rate > 0) {
            return Ok(FxRate {
                rate_scaled: rate,
                rate_text: rate_string(rate),
                fetched_at: None,
                source: "settings",
                fallback: true,
            });
        }
    }
    let rate = parse_rate(DEFAULT_FX_RATE).expect("built-in fx rate is valid");
    Ok(FxRate {
        rate_scaled: rate,
        rate_text: DEFAULT_FX_RATE.to_string(),
        fetched_at: None,
        source: "default",
        fallback: true,
    })
}

/// 原生人民币价索引（一条快照内的 `usage_pricing_native` 行）。
#[derive(Debug, Clone, Default)]
pub struct NativePrices {
    entries: Vec<dao::NativePriceRecord>,
}

impl NativePrices {
    pub fn load_with_conn(
        conn: &Connection,
        snapshot_id: Option<&str>,
    ) -> Result<Self, UsageError> {
        let Some(snapshot_id) = snapshot_id else {
            return Ok(Self::default());
        };
        Ok(Self {
            entries: dao::load_native_prices_conn(conn, snapshot_id)?,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 为某个归一化模型取原生人民币四价。
    ///
    /// 先按参考来源的本站变体（`moonshotai` → `moonshotai-cn` 等）匹配，再退回
    /// 同归一化模型的唯一人民币条目；多个不同来源时**不猜**，返回 None（走汇率换算）。
    pub fn rates_for(
        &self,
        normalized_model_id: &str,
        reference_provider: &str,
    ) -> Option<[Option<i128>; 4]> {
        if normalized_model_id.is_empty() {
            return None;
        }
        for provider in provider_variants(reference_provider) {
            if let Some(entry) = self.entries.iter().find(|entry| {
                entry.normalized_model_id == normalized_model_id && entry.provider_id == *provider
            }) {
                return Some(entry.rates);
            }
        }
        let mut matches = self
            .entries
            .iter()
            .filter(|entry| entry.normalized_model_id == normalized_model_id);
        let first = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        Some(first.rates)
    }
}

/// 参考来源的本站变体顺序（人民币来源优先）。
fn provider_variants(reference_provider: &str) -> &'static [&'static str] {
    match reference_provider {
        "moonshotai" | "moonshotai-cn" => &["moonshotai-cn", "moonshotai"],
        "alibaba" | "alibaba-cn" => &["alibaba-cn", "alibaba"],
        "minimax" | "minimax-cn" => &["minimax-cn", "minimax"],
        "zhipuai" | "zhipuai-coding-plan" => &["zhipuai", "zhipuai-coding-plan"],
        "deepseek" => &["deepseek"],
        _ => &[],
    }
}

/// 一个币种的四价 + 每个分量是否由另一币种换算而来。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CurrencyRates {
    pub rates: [Option<i128>; 4],
    /// 与 `rates` 同序；`true` = 该分量是汇率换算值（展示层加虚线下划线）。
    pub converted: [bool; 4],
}

impl CurrencyRates {
    pub fn any_converted(&self) -> bool {
        self.converted.iter().any(|converted| *converted)
    }

    pub fn has_rates(&self) -> bool {
        self.rates.iter().any(Option::is_some)
    }
}

/// 四价整体换算；`from` 与 `to` 必须不同，否则原样返回。
pub fn convert_rates(
    rates: [Option<i128>; 4],
    fx: &FxRate,
    from: PriceCurrency,
    to: PriceCurrency,
) -> Result<[Option<i128>; 4], UsageError> {
    if from == to {
        return Ok(rates);
    }
    let mut out = [None; 4];
    for (index, rate) in rates.iter().enumerate() {
        let Some(rate) = rate else {
            continue;
        };
        // 单分量换算溢出只让该分量在目标币种下不可用，**不得**把整张费用表打挂
        // （与 estimate 层的按行降级对齐；目录来自网络，单条脏费率是可能输入）。
        let converted = match (from, to) {
            (PriceCurrency::Usd, PriceCurrency::Cny) => usd_to_cny(*rate, fx.rate_scaled),
            (PriceCurrency::Cny, PriceCurrency::Usd) => cny_to_usd(*rate, fx.rate_scaled),
            _ => Ok(*rate),
        };
        if let Ok(value) = converted {
            out[index] = Some(value);
        }
    }
    Ok(out)
}

/// 构造某币种的 `CurrencyRates`：原生币种直接用；另一币种缺失原生价时换算并标记。
pub fn currency_view(
    native: [Option<i128>; 4],
    native_currency: PriceCurrency,
    display: PriceCurrency,
    fx: &FxRate,
) -> Result<CurrencyRates, UsageError> {
    if native_currency == display {
        return Ok(CurrencyRates {
            rates: native,
            converted: [false; 4],
        });
    }
    let converted = convert_rates(native, fx, native_currency, display)?;
    let mut flags = [false; 4];
    for (index, rate) in native.iter().enumerate() {
        flags[index] = rate.is_some() && converted[index].is_some();
    }
    Ok(CurrencyRates {
        rates: converted,
        converted: flags,
    })
}

/// 把覆盖价分量叠加到目录的双币种视图上。
///
/// 覆盖分量按其自身币种落位，另一币种按当日汇率换算并标 `converted`；
/// 未覆盖的分量保留目录的 USD/CNY 原生视图。
pub fn merge_override_views(
    override_rates: [Option<i128>; 4],
    override_currency: PriceCurrency,
    mut usd: CurrencyRates,
    mut cny: CurrencyRates,
    fx: &FxRate,
) -> Result<(CurrencyRates, CurrencyRates), UsageError> {
    for (index, override_rate) in override_rates.iter().enumerate() {
        let Some(rate) = *override_rate else {
            continue;
        };
        match override_currency {
            PriceCurrency::Usd => {
                usd.rates[index] = Some(rate);
                usd.converted[index] = false;
                match usd_to_cny(rate, fx.rate_scaled) {
                    Ok(value) => {
                        cny.rates[index] = Some(value);
                        cny.converted[index] = true;
                    }
                    Err(_) => {
                        // 对侧换算失败：该分量在 CNY 下不可用；不回退目录原值
                        // （目录价是另一份价格，不能冒充覆盖价）。
                        cny.rates[index] = None;
                        cny.converted[index] = false;
                    }
                }
            }
            PriceCurrency::Cny => {
                cny.rates[index] = Some(rate);
                cny.converted[index] = false;
                match cny_to_usd(rate, fx.rate_scaled) {
                    Ok(value) => {
                        usd.rates[index] = Some(value);
                        usd.converted[index] = true;
                    }
                    Err(_) => {
                        // 同上：覆盖分量在 USD 下不可用，不回退目录原值。
                        usd.rates[index] = None;
                        usd.converted[index] = false;
                    }
                }
            }
        }
    }
    Ok((usd, cny))
}

fn rounded_div(numerator: i128, denominator: i128) -> Result<i128, UsageError> {
    if denominator <= 0 {
        return Err(UsageError::internal(
            "fx conversion: non-positive exchange rate".to_string(),
        ));
    }
    let quotient = numerator / denominator;
    let remainder = numerator % denominator;
    Ok(if remainder * 2 >= denominator {
        quotient + 1
    } else {
        quotient
    })
}

/// 10^-9 USD/M → 10^-9 CNY/M（half-up）。
pub fn usd_to_cny(rate: i128, fx_rate_scaled: i128) -> Result<i128, UsageError> {
    let scaled = rate
        .checked_mul(fx_rate_scaled)
        .ok_or_else(|| UsageError::internal("fx conversion: overflow".to_string()))?;
    rounded_div(scaled, 1_000_000_000)
}

/// 10^-9 CNY/M → 10^-9 USD/M（half-up）。
pub fn cny_to_usd(rate: i128, fx_rate_scaled: i128) -> Result<i128, UsageError> {
    let scaled = rate
        .checked_mul(1_000_000_000)
        .ok_or_else(|| UsageError::internal("fx conversion: overflow".to_string()))?;
    rounded_div(scaled, fx_rate_scaled)
}

// ==== usage 自省锁：以下为排除区（测试代码），勿删勿复制 ====
#[cfg(test)]
mod tests {
    use super::*;

    fn fx(rate: &str) -> FxRate {
        FxRate {
            rate_scaled: parse_rate(rate).unwrap(),
            rate_text: rate.to_string(),
            fetched_at: Some(1),
            source: "er-api",
            fallback: false,
        }
    }

    #[test]
    fn usd_to_cny_approximates_official_zhipu_price() {
        // 源 TOML 的 USD 值只保留 5 位小数（8 / 6.693661 ≈ 1.19516），
        // 反算不会逐位等于 8；原生人民币价走 parse_native_cny 的精确路径。
        let rate = parse_rate("1.19516").unwrap();
        let cny = usd_to_cny(rate, parse_rate("6.693661").unwrap()).unwrap();
        assert!((cny - 8_000_000_000).abs() < 10_000, "cny={cny}");
    }

    #[test]
    fn cny_to_usd_round_trips_within_one_scaled_unit() {
        let cny = parse_rate("8").unwrap();
        let usd = cny_to_usd(cny, parse_rate("7.2").unwrap()).unwrap();
        assert_eq!(usd, 1_111_111_111);
    }

    #[test]
    fn usd_to_cny_rounds_half_up_per_component() {
        let fx = parse_rate("7.2").unwrap();
        // 1.111111111 USD × 7.2 = 7.9999999992 → 定点除的商 7.999999999 余 0.2e9 ⇒
        // 不足 .5 向下取整（换算只在恰半时进位，绝不无中生有凑整）。
        let usd = parse_rate("1.111111111").unwrap();
        assert_eq!(usd_to_cny(usd, fx).unwrap(), 7_999_999_999);
        // 恰好 .5 的商向上取整：0.000000005 × 0.1 = 0.0000000005 → 1（half-up，非截断 0）。
        assert_eq!(usd_to_cny(5, parse_rate("0.1").unwrap()).unwrap(), 1);
        // 不足 .5 向下：0.000000005 × 0.1 的两倍以下（0.4）→ 0。
        assert_eq!(usd_to_cny(4, parse_rate("0.1").unwrap()).unwrap(), 0);
    }

    #[test]
    fn cny_to_usd_rounds_half_up_and_overflows_loudly() {
        let fx = parse_rate("7.2").unwrap();
        // ¥8 / 7.2 = 1.111111111...（定点 half-up 到 10^-9 位）。
        assert_eq!(
            cny_to_usd(parse_rate("8").unwrap(), fx).unwrap(),
            1_111_111_111
        );
        // 溢出必须报错（调用方决定降级方式），不得静默截断。
        let huge = i128::MAX / 1_000;
        assert!(cny_to_usd(huge, fx).is_err());
    }

    fn mem_db() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        conn
    }

    /// 换算溢出按分量降级（生产 bug 回归钉：目录费率来自网络，单条脏费率是可能输入，
    /// 不得让整张费用表 Err——与 estimate 层的按行降级对齐）。
    #[test]
    fn conversion_overflow_degrades_only_that_component() {
        let fx = fx("7.2");
        // 1e29 scaled（= 1e20 USD/M）× 7.2e9 溢出 i128：原语仍响亮报错……
        let huge = 100_000_000_000_000_000_000_000_000_000_i128;
        assert!(usd_to_cny(huge, fx.rate_scaled).is_err());
        // ……但 convert_rates 返回 Ok，且只有溢出分量在目标币种下置 None。
        let rates = [
            Some(huge),
            Some(parse_rate("2").unwrap()),
            None,
            Some(parse_rate("10").unwrap()),
        ];
        let out = convert_rates(rates, &fx, PriceCurrency::Usd, PriceCurrency::Cny).unwrap();
        assert_eq!(
            out[0], None,
            "溢出分量在目标币种下不可用（不是 0、不是 Err）"
        );
        assert_eq!(out[1], Some(14_400_000_000), "正常分量照常换算（$2 × 7.2）");
        assert_eq!(out[2], None, "源缺价仍为 None");
        assert_eq!(
            out[3],
            Some(72_000_000_000),
            "正常分量照常换算（$10 × 7.2）"
        );

        // 反方向同理：CNY 覆盖分量 ×1e9 溢出（1e30 scaled = 1e21 CNY/M）。
        assert!(cny_to_usd(
            1_000_000_000_000_000_000_000_000_000_000_i128,
            fx.rate_scaled
        )
        .is_err());

        // merge_override_views：覆盖币种自身原样保留；溢出分量的另一币种置 None 且
        // 不标 converted；正常分量照常换算并标 converted。
        let empty = CurrencyRates {
            rates: [None; 4],
            converted: [false; 4],
        };
        let (usd, cny) = merge_override_views(
            [Some(huge), Some(parse_rate("8").unwrap()), None, None],
            PriceCurrency::Usd,
            empty,
            CurrencyRates {
                rates: [None; 4],
                converted: [false; 4],
            },
            &fx,
        )
        .unwrap();
        assert_eq!(
            usd.rates[0],
            Some(huge),
            "覆盖币种自身的分量不受换算溢出影响"
        );
        assert!(!usd.converted[0]);
        assert_eq!(
            cny.rates[0], None,
            "溢出分量的 CNY 面置 None（不丢覆盖价本身）"
        );
        assert!(
            !cny.converted[0],
            "未产生换算值就不得标 converted（前端不画虚线下划线）"
        );
        assert_eq!(
            cny.rates[1],
            Some(57_600_000_000),
            "正常覆盖分量照常换算（$8 × 7.2）"
        );
        assert!(cny.converted[1]);
    }

    #[test]
    fn fx_resolution_prefers_active_then_cached_then_settings_then_default() {
        let conn = mem_db();
        // ① 无任何已保存汇率、无设置页备用 ⇒ 内置默认（fallback）。
        let by_default = resolve_fx_with_conn(&conn, Some("snap-a"), None).unwrap();
        assert_eq!(
            (by_default.source, by_default.rate_text.as_str()),
            ("default", "7.2")
        );
        assert!(by_default.fallback && by_default.fetched_at.is_none());

        // ② 设置页备用汇率：合法值生效且仍是 fallback；非法值忽略落到默认。
        let by_settings = resolve_fx_with_conn(&conn, Some("snap-a"), Some("7.5")).unwrap();
        assert_eq!(
            (by_settings.source, by_settings.rate_text.as_str()),
            ("settings", "7.5")
        );
        assert!(by_settings.fallback);
        let ignored = resolve_fx_with_conn(&conn, Some("snap-a"), Some("abc")).unwrap();
        assert_eq!(ignored.source, "default", "非法备用汇率必须忽略");
        let negative = resolve_fx_with_conn(&conn, None, Some("-1")).unwrap();
        assert_eq!(negative.source, "default", "非正备用汇率必须忽略");

        // ③ 任意快照的最近一次成功汇率（cached）：快照缺行时回退，仍是 fallback。
        dao::upsert_fx_conn(&conn, "snap-old", 7_100_000_000, 100, "https://old").unwrap();
        let cached = resolve_fx_with_conn(&conn, Some("snap-a"), Some("7.5")).unwrap();
        assert_eq!(
            (cached.source, cached.rate_text.as_str(), cached.fetched_at),
            ("cached", "7.1", Some(100)),
            "settings 备用汇率不得压过已保存的 cached 汇率"
        );
        assert!(cached.fallback);

        // ④ active 快照自带汇率（er-api）：最高优先级，fallback=false。
        dao::upsert_fx_conn(&conn, "snap-a", 7_200_000_000, 200, "https://er-api").unwrap();
        let active = resolve_fx_with_conn(&conn, Some("snap-a"), Some("7.5")).unwrap();
        assert_eq!(
            (active.source, active.rate_text.as_str(), active.fetched_at),
            ("er-api", "7.2", Some(200))
        );
        assert!(!active.fallback, "随 active 快照取到的汇率不是 fallback");

        // ⑤ snapshot_id=None（无快照）时读不到 er-api 档 ⇒ 最近成功（cached）。
        let no_snapshot = resolve_fx_with_conn(&conn, None, Some("7.5")).unwrap();
        assert_eq!(no_snapshot.source, "cached");
    }

    #[test]
    fn provider_variants_put_the_cny_site_first() {
        assert_eq!(
            provider_variants("moonshotai"),
            ["moonshotai-cn", "moonshotai"].as_slice()
        );
        assert_eq!(
            provider_variants("moonshotai-cn"),
            ["moonshotai-cn", "moonshotai"].as_slice()
        );
        assert_eq!(
            provider_variants("alibaba"),
            ["alibaba-cn", "alibaba"].as_slice()
        );
        assert_eq!(
            provider_variants("minimax-cn"),
            ["minimax-cn", "minimax"].as_slice()
        );
        assert_eq!(
            provider_variants("zhipuai-coding-plan"),
            ["zhipuai", "zhipuai-coding-plan"].as_slice()
        );
        assert_eq!(provider_variants("deepseek"), ["deepseek"].as_slice());
        assert!(
            provider_variants("openai").is_empty(),
            "无中国站变体的来源不得猜变体"
        );
    }

    #[test]
    fn native_rates_prefer_reference_variant_and_refuse_ambiguous_sources() {
        let mut conn = mem_db();
        let record = |provider: &str, model: &str, input: i128| dao::NativePriceRecord {
            provider_id: provider.to_string(),
            model_id: model.to_string(),
            normalized_model_id: crate::services::usage::v2::pricing::normalize_model_id(model),
            rates: [Some(input), None, None, None],
            source_url: format!("https://example.test/{provider}/{model}.toml"),
            fetched_at: 1,
        };
        let prices = NativePrices::load_with_conn(&conn, Some("snap")).unwrap();
        assert!(prices.is_empty(), "空快照 ⇒ 空索引");

        // 参考来源的本站变体（moonshotai → moonshotai-cn）优先于国际站。
        dao::replace_native_prices_conn(
            &mut conn,
            "snap",
            &[
                record("moonshotai", "kimi-k3", 1),
                record("moonshotai-cn", "kimi-k3", 2),
            ],
        )
        .unwrap();
        let prices = NativePrices::load_with_conn(&conn, Some("snap")).unwrap();
        assert_eq!(
            prices.rates_for("kimi-k3", "moonshotai").unwrap()[0],
            Some(2),
            "人民币来源（-cn）优先"
        );
        assert_eq!(
            prices.rates_for("kimi-k3", "moonshotai-cn").unwrap()[0],
            Some(2)
        );
        assert_eq!(
            prices.rates_for("kimi-k3", "openai"),
            None,
            "非变体参考来源 + 多条不同来源 ⇒ 不猜，走汇率换算"
        );

        // 单一来源（无变体冲突）⇒ 直接可用。
        dao::replace_native_prices_conn(&mut conn, "snap", &[record("deepseek", "v4", 3)]).unwrap();
        let prices = NativePrices::load_with_conn(&conn, Some("snap")).unwrap();
        assert_eq!(prices.rates_for("v4", "deepseek").unwrap()[0], Some(3));
        assert_eq!(prices.rates_for("missing", "deepseek"), None);
        assert_eq!(prices.rates_for("", "deepseek"), None, "空归一化 ID 不查");
    }

    #[test]
    fn converted_flags_are_per_component() {
        let catalog_usd = CurrencyRates {
            rates: [None, Some(parse_rate("0.28").unwrap()), None, None],
            converted: [false; 4],
        };
        let catalog_cny = CurrencyRates {
            rates: [None, Some(parse_rate("2").unwrap()), None, None],
            converted: [false; 4],
        };
        let (usd, cny) = merge_override_views(
            [Some(parse_rate("20").unwrap()), None, None, None],
            PriceCurrency::Usd,
            catalog_usd,
            catalog_cny,
            &fx("7.2"),
        )
        .unwrap();
        assert_eq!(usd.rates[0], Some(parse_rate("20").unwrap()));
        assert!(!usd.converted[0]);
        assert_eq!(usd.rates[1], Some(parse_rate("0.28").unwrap()));
        assert!(!usd.converted[1]);
        assert_eq!(cny.rates[0], Some(144_000_000_000)); // 20 USD * 7.2
        assert!(cny.converted[0]);
        assert_eq!(cny.rates[1], Some(parse_rate("2").unwrap()));
        assert!(!cny.converted[1]);
    }
}
// ==== usage 自省锁：排除区结束，勿删勿复制 ====
