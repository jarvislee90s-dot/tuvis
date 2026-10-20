//! 可计价基础的保守分类与金额估算（Task 6/9）。
//!
//! `safe_components` **只判断哪些桶能安全计价**；`estimate_components` 在 safe 与价格
//! 同时可得时按 10^-15 USD 定点计算金额与覆盖率。`trusted` 不是“有价格”，
//! 而是源与数据形态的静态证据许可：七源标准 normalize 路径、非负规范桶、output 已含
//! reasoning，且上游价是可适用的文本基准价。未来新增源默认不可信。
//!
//! ## 聚合层只恢复互斥数量
//!
//! `safe_components` 只能从聚合桶恢复**互斥**的普通输入/缓存读/缓存写/输出数量；
//! 它不证明缓存 TTL、上下文阶梯或任何 provider 条件（Task 7 负责价格条件降级）。
//!
//! 对已证明的 normalize 路径（尤其是 Subset 语义），有：
//!
//! ```text
//! sum(fresh + read + write) - requestTotal == Subset 写重叠总量
//! ```
//!
//! 在非负增量前提下，该差值等于 0 即无重叠（普通输入可计价）；大于 0 则普通输入与缓存写
//! 都可能已含重叠，二者**都留空**，只保留已证明的 cache_read / output。该推导**不得**
//! 推广到未知历史数据形态——**尤其 dsh backfill / 日档**（日聚合无法区分逐行语义）——
//! 这些形态通过 `basis_trusted` 降级，而不是对所有日档一刀切。
//!
//! dsh 日档的降级**只针对输入/缓存桶**：`output`（已含 reasoning）的数量与缓存语义无关，
//! 也不参与 `requestTotal`（请求输入）的去重推导，因此由 `safe_components_output_only`
//! 单独计价；输入/缓存读/缓存写仍保持未估价，整行状态落为 `partial`，覆盖率分母仍是
//! 规范处理 Token（`requestTotal + output`）。不得据此放宽 input/cache 的可信性。

use crate::services::usage::model::{UsageBuckets, UsageSourceId};
use crate::services::usage::range::RangeGranularity;
use crate::services::usage::v2::model::CostStatus;
use crate::services::usage::UsageError;

/// 单个组件的内部估算（金额单位 10^-15 USD，**不得**在行合并前转字符串/四舍五入）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComponentEstimate {
    pub tokens: i64,
    pub rate_scaled: i128,
    pub amount_scaled: i128,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Estimate {
    /// 顺序固定 `[input, cache_read, cache_write, output]`；`None` = 该部分不可计价。
    pub components: [Option<ComponentEstimate>; 4],
    pub amount_scaled: Option<i128>,
    pub priced_tokens: i64,
    pub unpriced_tokens: i64,
    pub coverage_rate: Option<f64>,
    pub status: CostStatus,
}

pub fn estimate_components(
    buckets: &UsageBuckets,
    request_total: i64,
    safe: [Option<i64>; 4],
    rates: [Option<i128>; 4],
) -> Result<Estimate, UsageError> {
    let internal = |detail: &str| UsageError::internal(detail.to_string());
    // 规范总量（hero 口径）：requestTotal + output。金额/覆盖率的分母都用它。
    let total = request_total
        .checked_add(buckets.output)
        .ok_or_else(|| internal("cost estimate: total overflow"))?;

    let mut components: [Option<ComponentEstimate>; 4] = [None; 4];
    let mut priced_tokens: i64 = 0;
    let mut amount_scaled: i128 = 0;
    for index in 0..4 {
        let (Some(tokens), Some(rate_scaled)) = (safe[index], rates[index]) else {
            continue; // 不可安全计价或缺价：不产生组件，也不计 priced token。
        };
        if tokens < 0 || rate_scaled < 0 {
            return Err(internal("cost estimate: negative tokens or rate"));
        }
        let component_amount = i128::from(tokens)
            .checked_mul(rate_scaled)
            .ok_or_else(|| internal("cost estimate: component amount overflow"))?;
        // 单位换算（rate 1e-9 USD/M → 1e-15 USD/M）也必须可表示，避免下游格式化时溢出。
        rate_scaled
            .checked_mul(1_000_000)
            .ok_or_else(|| internal("cost estimate: unit amount overflow"))?;
        amount_scaled = amount_scaled
            .checked_add(component_amount)
            .ok_or_else(|| internal("cost estimate: amount accumulation overflow"))?;
        priced_tokens = priced_tokens
            .checked_add(tokens)
            .ok_or_else(|| internal("cost estimate: priced token overflow"))?;
        components[index] = Some(ComponentEstimate {
            tokens,
            rate_scaled,
            amount_scaled: component_amount,
        });
    }

    let unpriced_tokens = total
        .checked_sub(priced_tokens)
        .ok_or_else(|| internal("cost estimate: unpriced token overflow"))?;
    if unpriced_tokens < 0 {
        return Err(internal(
            "cost estimate: priced tokens exceed the canonical total",
        ));
    }
    let coverage_rate = if total > 0 {
        Some(priced_tokens as f64 / total as f64)
    } else {
        None
    };
    // 只有至少一个**正 token** 的可计价项才输出金额；total=0 或无可计价正量 ⇒ None。
    let amount = (priced_tokens > 0).then_some(amount_scaled);
    let status = if total > 0 && priced_tokens == total {
        CostStatus::Priced
    } else if priced_tokens > 0 && priced_tokens < total {
        CostStatus::Partial
    } else {
        CostStatus::Unpriced
    };

    Ok(Estimate {
        components,
        amount_scaled: amount,
        priced_tokens,
        unpriced_tokens,
        coverage_rate,
        status,
    })
}

/// 金额（10^-15 USD）→ 十进制字符串：完整保留精度、去尾零、整数不带小数点。
pub fn usd_string(value: i128) -> String {
    let whole = value / 1_000_000_000_000_000;
    let fraction = value % 1_000_000_000_000_000;
    let tail = format!("{fraction:015}");
    let tail = tail.trim_end_matches('0');
    if tail.is_empty() {
        whole.to_string()
    } else {
        format!("{whole}.{tail}")
    }
}

/// 金额（10^-15 CNY）→ 十进制字符串；定点/去尾零规则与 [`usd_string`] 完全一致，
/// 仅用于语义区分（避免把人民币金额误读成 USD）。
pub fn cny_string(value: i128) -> String {
    usd_string(value)
}

/// 返回 `[input, cache_read, cache_write, output]` 的可安全计价 Token 数。
/// `None` = 该部分不可计价（**不是 0 价**）。
pub fn safe_components(b: &UsageBuckets, request_total: i64, trusted: bool) -> [Option<i64>; 4] {
    if !trusted
        || request_total < 0
        || b.input_fresh < 0
        || b.cache_read < 0
        || b.cache_write < 0
        || b.output < 0
    {
        return [None; 4];
    }
    let Some(base) = b.input_fresh.checked_add(b.cache_read) else {
        return [None; 4];
    };
    let Some(with_write) = base.checked_add(b.cache_write) else {
        return [None; 4];
    };
    if request_total == with_write {
        [
            Some(b.input_fresh),
            Some(b.cache_read),
            Some(b.cache_write),
            Some(b.output),
        ]
    } else if request_total >= base && request_total < with_write {
        // 存在写重叠：普通输入与缓存写均不可证明互斥，只保留 read/output。
        [None, Some(b.cache_read), None, Some(b.output)]
    } else {
        [None; 4]
    }
}

/// dsh 日档专用：只把 **output** 列为可安全计价，其余三桶保持 `None`。
///
/// 日聚合丢失逐请求缓存语义（spec §4.3），无法证明普通输入 / 缓存读 / 缓存写互斥；
/// 但 output（已含 reasoning）的数量与缓存语义无关，也不参与 `requestTotal`（请求输入）
/// 的去重推导，因此缺少语义证据时仍可单独计价。覆盖率分母由 `estimate_components`
/// 按 `requestTotal + output` 计算，状态自然落为 `partial`。
///
/// `output < 0` ⇒ 整行不可计价。`requestTotal + output` 的算术溢出不在这里判断
/// （本函数没有算术），由 `estimate_components` 的 checked 计算兜底并降级整行。
pub fn safe_components_output_only(b: &UsageBuckets) -> [Option<i64>; 4] {
    if b.output < 0 {
        return [None; 4];
    }
    [None, None, None, Some(b.output)]
}

/// 该源 × 档位是否具备保守计价的数据形态证据。
///
/// * Hour：七源都走标准 normalize 路径、记录级明细可区分语义 ⇒ 全部可信；
/// * Day：`dsh` 的 backfill 行只有日键、日聚合无法区分逐行语义 ⇒ 输入/缓存桶不可信；
///   其余六源可信。dsh 日档的 output 与缓存语义无关，由 `safe_components_output_only`
///   单独计价（不是整行未估价）。
///
/// 白名单必须来自本任务的证据；未来新增源默认 `false`（不进入 `matches!`）。
pub fn basis_trusted(source: UsageSourceId, granularity: RangeGranularity) -> bool {
    match granularity {
        RangeGranularity::Hour => matches!(
            source,
            UsageSourceId::Claude
                | UsageSourceId::Codex
                | UsageSourceId::Kimi
                | UsageSourceId::OpenCode
                | UsageSourceId::WorkBuddy
                | UsageSourceId::ZCode
                | UsageSourceId::Dsh
        ),
        RangeGranularity::Day => matches!(
            source,
            UsageSourceId::Claude
                | UsageSourceId::Codex
                | UsageSourceId::Kimi
                | UsageSourceId::OpenCode
                | UsageSourceId::WorkBuddy
                | UsageSourceId::ZCode
        ),
    }
}

// ==== usage 自省锁：以下为排除区（测试代码），勿删勿复制 ====
#[cfg(test)]
mod tests {
    use super::*;

    fn buckets(fresh: i64, read: i64, write: i64, output: i64) -> UsageBuckets {
        UsageBuckets {
            input_fresh: fresh,
            cache_read: read,
            cache_write: write,
            output,
        }
    }

    #[test]
    fn overlapping_cache_write_cannot_be_charged_twice() {
        let b = buckets(80, 20, 10, 30);
        assert_eq!(
            safe_components(&b, 100, true),
            [None, Some(20), None, Some(30)]
        );
    }

    #[test]
    fn disjoint_input_without_write_is_recoverable() {
        let b = buckets(80, 20, 0, 30);
        assert_eq!(
            safe_components(&b, 100, true),
            [Some(80), Some(20), Some(0), Some(30)]
        );
    }

    #[test]
    fn untrusted_source_yields_no_safe_component() {
        let b = buckets(80, 20, 0, 30);
        assert_eq!(safe_components(&b, 100, false), [None; 4]);
    }

    #[test]
    fn inconsistent_totals_and_negative_buckets_are_all_unpriced() {
        let b = buckets(80, 20, 0, 30);
        for request_total in [-1, 99, 101] {
            assert_eq!(
                safe_components(&b, request_total, true),
                [None; 4],
                "requestTotal={request_total} 与桶不一致时必须整行不可计价"
            );
        }
        for b in [
            buckets(-1, 20, 0, 30),
            buckets(80, -1, 0, 30),
            buckets(80, 20, -1, 30),
            buckets(80, 20, 0, -1),
        ] {
            assert_eq!(safe_components(&b, 100, true), [None; 4]);
        }
    }

    #[test]
    fn output_is_emitted_exactly_once_in_fixed_order() {
        // output 已在上游合并 reasoning：本函数不得再次追加。
        let b = buckets(10, 0, 0, 30);
        let safe = safe_components(&b, 10, true);
        assert_eq!(safe, [Some(10), Some(0), Some(0), Some(30)]);
        assert_eq!(safe[3], Some(b.output));
    }

    #[test]
    fn zero_usage_is_proven_zero_not_missing_price() {
        let zero = buckets(0, 0, 0, 0);
        assert_eq!(
            safe_components(&zero, 0, true),
            [Some(0), Some(0), Some(0), Some(0)],
            "空总量/真实 0 与“缺价”不同：前者是已证明的 0 Token"
        );
    }

    #[test]
    fn positive_write_without_overlap_is_fully_priced() {
        // requestTotal == fresh + read + write ⇒ 普通输入与缓存写**不重叠**，四个组件都可计价。
        // 这条专门打红「只要 cache_write > 0 就判重叠」的过度保守实现。
        let b = buckets(80, 10, 10, 30);
        assert_eq!(
            safe_components(&b, 100, true),
            [Some(80), Some(10), Some(10), Some(30)]
        );
    }

    #[test]
    fn checked_add_overflow_is_all_unpriced() {
        let base_overflow = UsageBuckets {
            input_fresh: i64::MAX,
            cache_read: 1,
            cache_write: 0,
            output: 0,
        };
        assert_eq!(safe_components(&base_overflow, 0, true), [None; 4]);

        let write_overflow = UsageBuckets {
            input_fresh: 1,
            cache_read: 0,
            cache_write: i64::MAX,
            output: 0,
        };
        assert_eq!(safe_components(&write_overflow, 0, true), [None; 4]);
    }

    #[test]
    fn subset_overlap_equals_the_nonnegative_derivation() {
        // 第 4 步推导：Subset 有写 [80,20,10,30], requestTotal=100
        // sum(fresh+read+write)-requestTotal = 110-100 = 10 = 写重叠总量。
        let overlap = buckets(80, 20, 10, 30);
        let sum = overlap.input_fresh + overlap.cache_read + overlap.cache_write;
        assert_eq!(sum - 100, overlap.cache_write);
        assert_eq!(safe_components(&overlap, 100, true)[2], None);

        // Subset 无写：增量 0，普通输入可安全计价。
        let disjoint = buckets(80, 20, 0, 30);
        let sum = disjoint.input_fresh + disjoint.cache_read + disjoint.cache_write;
        assert_eq!(sum - 100, 0);
        assert_eq!(
            safe_components(&disjoint, 100, true),
            [Some(80), Some(20), Some(0), Some(30)]
        );
    }

    #[test]
    fn basis_trusted_is_hour_true_for_all_sources_and_day_false_only_for_dsh() {
        for source in UsageSourceId::ALL {
            assert!(
                basis_trusted(source, RangeGranularity::Hour),
                "{source:?} 小时档必须可信（标准 normalize 路径）"
            );
        }
        for source in UsageSourceId::ALL {
            let expected = source != UsageSourceId::Dsh;
            assert_eq!(
                basis_trusted(source, RangeGranularity::Day),
                expected,
                "{source:?} 日档可信性不符（dsh 的 backfill 行只有日键）"
            );
        }
    }

    // ── Task 9：金额 / 覆盖率 / 状态 ────────────────────────────────────────

    fn all_rates() -> [Option<i128>; 4] {
        [
            Some(2_000_000_000),
            Some(200_000_000),
            Some(2_500_000_000),
            Some(10_000_000_000),
        ]
    }

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-12,
            "coverage 不符：actual={actual}, expected={expected}"
        );
    }

    #[test]
    fn plan_main_case_amount_and_full_coverage() {
        let b = buckets(100, 20, 10, 30);
        let estimate = estimate_components(
            &b,
            130,
            [Some(100), Some(20), Some(10), Some(30)],
            all_rates(),
        )
        .unwrap();
        assert_eq!(estimate.amount_scaled, Some(529_000_000_000));
        assert_eq!(usd_string(estimate.amount_scaled.unwrap()), "0.000529");
        assert_eq!(estimate.priced_tokens, 160);
        assert_eq!(estimate.unpriced_tokens, 0);
        assert_eq!(estimate.coverage_rate, Some(1.0));
        assert_eq!(estimate.status, CostStatus::Priced);
        assert!(estimate.components.iter().all(Option::is_some));
        assert_eq!(estimate.components[0].unwrap().tokens, 100);
        assert_eq!(estimate.components[3].unwrap().tokens, 30);
    }

    #[test]
    fn missing_cache_write_rate_is_partial() {
        let b = buckets(100, 20, 10, 30);
        let mut rates = all_rates();
        rates[2] = None;
        let estimate =
            estimate_components(&b, 130, [Some(100), Some(20), Some(10), Some(30)], rates).unwrap();
        assert_eq!(estimate.priced_tokens, 150);
        assert_eq!(estimate.unpriced_tokens, 10);
        assert_close(estimate.coverage_rate.unwrap(), 150.0 / 160.0);
        assert_eq!(estimate.status, CostStatus::Partial);
        assert!(estimate.components[2].is_none());
        assert_eq!(estimate.amount_scaled, Some(504_000_000_000));
        assert_eq!(usd_string(estimate.amount_scaled.unwrap()), "0.000504");
    }

    #[test]
    fn subset_only_read_and_output_are_priced() {
        let b = buckets(80, 20, 10, 30);
        let estimate =
            estimate_components(&b, 100, [None, Some(20), None, Some(30)], all_rates()).unwrap();
        assert_eq!(estimate.priced_tokens, 50);
        assert_eq!(estimate.unpriced_tokens, 80, "130 - 50");
        assert_close(estimate.coverage_rate.unwrap(), 50.0 / 130.0);
        assert_eq!(estimate.status, CostStatus::Partial);
        assert!(estimate.components[0].is_none());
        assert!(estimate.components[2].is_none());
        assert_eq!(estimate.components[1].unwrap().tokens, 20);
        assert_eq!(estimate.components[3].unwrap().tokens, 30);
        assert_eq!(estimate.amount_scaled, Some(304_000_000_000));

        // 覆盖率分母是规范 total（requestTotal + output），不是 requestTotal。
        assert_close(estimate.coverage_rate.unwrap() * 130.0, 50.0);
    }

    #[test]
    fn explicit_zero_rates_are_priced_zero_but_missing_rates_are_unpriced() {
        let b = buckets(100, 20, 10, 30);
        let safe = [Some(100), Some(20), Some(10), Some(30)];

        let free = estimate_components(&b, 130, safe, [Some(0); 4]).unwrap();
        assert_eq!(free.amount_scaled, Some(0));
        assert_eq!(usd_string(0), "0");
        assert_eq!(free.priced_tokens, 160);
        assert_eq!(free.unpriced_tokens, 0);
        assert_eq!(free.status, CostStatus::Priced);
        assert!(free.components.iter().all(|part| {
            part.is_some_and(|component| {
                component.rate_scaled == 0
                    && component.amount_scaled == 0
                    && usd_string(component.amount_scaled) == "0"
            })
        }));

        let missing = estimate_components(&b, 130, safe, [None; 4]).unwrap();
        assert_eq!(missing.amount_scaled, None);
        assert_eq!(missing.priced_tokens, 0);
        assert_eq!(missing.unpriced_tokens, 160);
        assert_eq!(missing.coverage_rate, Some(0.0));
        assert_eq!(missing.status, CostStatus::Unpriced);
        assert!(missing.components.iter().all(Option::is_none));
    }

    #[test]
    fn zero_total_has_null_coverage_and_no_amount() {
        let b = buckets(0, 0, 0, 0);
        let estimate = estimate_components(&b, 0, [Some(0); 4], [Some(2_000_000_000); 4]).unwrap();
        assert_eq!(estimate.amount_scaled, None);
        assert_eq!(estimate.priced_tokens, 0);
        assert_eq!(estimate.unpriced_tokens, 0);
        assert_eq!(estimate.coverage_rate, None, "total=0 不得显示假 100%");
        assert_eq!(estimate.status, CostStatus::Unpriced);
    }

    #[test]
    fn overflow_in_multiplication_addition_or_total_is_internal() {
        // tokens * rate 溢出
        let mul_overflow = estimate_components(
            &buckets(1, 0, 0, 0),
            i64::MAX,
            [Some(i64::MAX), Some(0), Some(0), Some(0)],
            [Some(10_i128.pow(32)), Some(0), Some(0), Some(0)],
        );
        assert_eq!(mul_overflow.unwrap_err().code, "usage-internal");

        // 两项 amount_scaled 相加溢出（单项与 unit 换算都不溢出）
        let add_overflow = estimate_components(
            &buckets(1_000_000, 0, 0, 1_000_000),
            1_000_000,
            [Some(1_000_000), Some(0), Some(0), Some(1_000_000)],
            [
                Some(10_i128.pow(32)),
                Some(0),
                Some(0),
                Some(10_i128.pow(32)),
            ],
        );
        assert_eq!(add_overflow.unwrap_err().code, "usage-internal");

        // requestTotal + output 溢出
        let total_overflow = estimate_components(
            &buckets(0, 0, 0, 1),
            i64::MAX,
            [Some(0), Some(0), Some(0), Some(1)],
            all_rates(),
        );
        assert_eq!(total_overflow.unwrap_err().code, "usage-internal");
    }

    #[test]
    fn usd_string_keeps_full_precision_without_scientific_notation() {
        assert_eq!(usd_string(0), "0");
        assert_eq!(usd_string(529_000_000_000), "0.000529");
        assert_eq!(usd_string(1_000_000_000_000_000), "1");
        assert_eq!(usd_string(1_500_000_000_000_000), "1.5");
        assert_eq!(usd_string(1_230_000_000_000_000), "1.23");
        assert_eq!(usd_string(1_000_000_000_000_001), "1.000000000000001");
        assert_eq!(
            usd_string(123_456_789_000_000_000_000_000),
            "123456789",
            "大数不得使用科学计数法"
        );
        assert!(!usd_string(1_000_000_000_000_001).contains('e'));
    }

    #[test]
    fn output_component_carries_reasoning_exactly_once() {
        // 上游已经把 reasoning 合并进 output；本层不得再新增 reasoning 组件。
        let b = buckets(10, 0, 0, 30);
        let estimate =
            estimate_components(&b, 10, [Some(10), Some(0), Some(0), Some(30)], all_rates())
                .unwrap();
        assert_eq!(estimate.components.len(), 4);
        assert_eq!(estimate.components[3].unwrap().tokens, 30);
        let token_sum: i64 = estimate
            .components
            .iter()
            .flatten()
            .map(|component| component.tokens)
            .sum();
        assert_eq!(token_sum, 40, "四组件和 = fresh+read+write+output");
    }

    // ── dsh 日档：只计价 output（2026-10-19 修复）────────────────────────────

    #[test]
    fn output_only_prices_exactly_the_output_bucket() {
        let b = buckets(80, 20, 10, 30);
        assert_eq!(
            safe_components_output_only(&b),
            [None, None, None, Some(30)],
            "日聚合 dsh 行只给与缓存语义无关的 output，输入/缓存读/缓存写必须留空"
        );
    }

    #[test]
    fn output_only_treats_negative_output_as_entirely_unpriced() {
        assert_eq!(
            safe_components_output_only(&buckets(80, 20, 10, -1)),
            [None; 4]
        );
    }

    #[test]
    fn output_only_estimate_is_partial_with_output_coverage() {
        let b = buckets(100, 20, 10, 30);
        let estimate =
            estimate_components(&b, 130, safe_components_output_only(&b), all_rates()).unwrap();
        assert_eq!(estimate.components[0], None);
        assert_eq!(estimate.components[1], None);
        assert_eq!(estimate.components[2], None);
        assert_eq!(estimate.components[3].map(|part| part.tokens), Some(30));
        assert_eq!(estimate.priced_tokens, 30);
        assert_eq!(
            estimate.unpriced_tokens, 130,
            "分母仍是 requestTotal+output"
        );
        assert_close(estimate.coverage_rate.unwrap(), 30.0 / 160.0);
        assert_eq!(estimate.status, CostStatus::Partial);
        assert_eq!(estimate.amount_scaled, Some(300_000_000_000));
        assert_eq!(usd_string(estimate.amount_scaled.unwrap()), "0.0003");
    }

    #[test]
    fn output_only_with_zero_output_prices_nothing() {
        let b = buckets(10, 0, 0, 0);
        let estimate =
            estimate_components(&b, 10, safe_components_output_only(&b), all_rates()).unwrap();
        assert_eq!(estimate.status, CostStatus::Unpriced);
        assert_eq!(estimate.amount_scaled, None);
        assert_eq!(estimate.priced_tokens, 0);
        assert_eq!(estimate.unpriced_tokens, 10);
    }

    #[test]
    fn component_strings_use_the_documented_units() {
        let estimate = estimate_components(
            &buckets(100, 20, 10, 30),
            130,
            [Some(100), Some(20), Some(10), Some(30)],
            all_rates(),
        )
        .unwrap();
        let units: Vec<String> = estimate
            .components
            .iter()
            .map(|component| {
                let component = component.unwrap();
                usd_string(component.rate_scaled * 1_000_000)
            })
            .collect();
        assert_eq!(units, vec!["2", "0.2", "2.5", "10"]);
    }
}
// ==== usage 自省锁：排除区结束，勿删勿复制 ====
