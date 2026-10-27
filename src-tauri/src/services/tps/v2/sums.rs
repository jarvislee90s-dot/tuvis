//! V2 聚合充分统计与纯派生（计划 Task 6：`derive_metrics(sums:&SpeedSums)->SpeedMetrics`）。
//!
//! 职责边界：DAO（database/dao/speed.rs）在 SQL 里把一窗事实折算成 [`SpeedSums`]
//! 充分统计（SUM/COUNT 一次扫描，不向前端传原始事实）；本层把充分统计**纯函数**地
//! 派生成契约指标面 [`SpeedMetrics`]——无 IO、无时钟、无隐藏状态（同输入必同输出）。
//!
//! 「零合计不冒充 0 速度」（Spec §7「空桶不推算速度」/ 契约同名条款）在此收口：
//! `n == 0 ⇒ value = null`；TTFT / 模型用时 / 工具用时同理（各自分母为 0 ⇒ null）。
//!
//! 有效性口径**只**在 SQL 侧（与 Task 1 Rust 谓词的矩阵契约测试锁在
//! tests/tps_speed_dashboard_test.rs）；本层不再判有效性，只汇总已分类的量。

use crate::services::tps::model::OutputScope;
use crate::services::tps::v2::model::{SpeedMetrics, SpeedRate};

/// 一窗（区间或单桶）事实的充分统计。全部字段都是可加的 SUM/COUNT 产物；
/// `output_scope` 聚合规则：组内无行 → None；恰一种 scope → Some(该 scope)；
/// 混合 → None 且 `scope_mixed = true`（派生面折算 Unknown，不猜单一口径）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SpeedSums {
    // ---- stream 口径（成功 + 正输出 + 正生成窗）----
    pub stream_output_tokens: i64,
    pub stream_duration_ms: i64,
    pub stream_sample_count: i64,
    // ---- response 口径（成功 + 正输出 + 响应样本时长可靠；独立有效集）----
    pub response_output_tokens: i64,
    pub response_duration_ms: i64,
    pub response_sample_count: i64,
    // ---- TTFT 独立池（成功且首点可对齐；无输出也计入，Spec §5.3）----
    pub ttft_sum_ms: i64,
    pub ttft_sample_count: i64,
    // ---- 可靠模型用时池（不分成败：native>0 直接取（无 outcome/输出门槛）；
    //      否则显式响应窗有效才取（>0 且不短于生成窗）；双缺 = missing_timing，不填 0）----
    pub model_ms_sum: i64,
    pub model_ms_samples: i64,
    // ---- 可靠工具用时池（tool_ms > 0 才计入；现来源普遍为空）----
    pub tool_ms_sum: i64,
    pub tool_ms_samples: i64,
    // ---- 候选与分类计数（不是任何速度的 n，Spec §5.3）----
    pub candidate_count: i64,
    pub failed_count: i64,
    pub cancelled_count: i64,
    /// 双窗皆缺失的候选数
    pub missing_timing_count: i64,
    /// COUNT(DISTINCT session_key)；null 会话键不计入（也不是错误）
    pub session_count: i64,
    pub latest_stream_ended_at: Option<i64>,
    pub latest_response_ended_at: Option<i64>,
    pub output_scope: Option<OutputScope>,
    pub scope_mixed: bool,
}

/// 单速率派生：`value = 1000×Σ输出/Σ毫秒`，n=0 ⇒ null（不冒充 0 速度）。
/// 有效样本的分母恒正（有效性要求时长 > 0），不会除零。
fn derive_rate(output_tokens: i64, duration_ms: i64, sample_count: i64) -> SpeedRate {
    SpeedRate {
        value: if sample_count == 0 {
            None
        } else {
            Some(1000.0 * output_tokens as f64 / duration_ms as f64)
        },
        output_tokens,
        duration_ms,
        sample_count,
    }
}

/// 纯派生（契约入口）：[`SpeedSums`] → [`SpeedMetrics`]。无 IO、无时钟、无隐藏状态。
pub fn derive_metrics(sums: &SpeedSums) -> SpeedMetrics {
    SpeedMetrics {
        stream: derive_rate(
            sums.stream_output_tokens,
            sums.stream_duration_ms,
            sums.stream_sample_count,
        ),
        response: derive_rate(
            sums.response_output_tokens,
            sums.response_duration_ms,
            sums.response_sample_count,
        ),
        avg_first_token_ms: if sums.ttft_sample_count == 0 {
            None
        } else {
            Some(sums.ttft_sum_ms as f64 / sums.ttft_sample_count as f64)
        },
        ttft_sample_count: sums.ttft_sample_count,
        model_ms: (sums.model_ms_samples > 0).then_some(sums.model_ms_sum),
        tool_ms: (sums.tool_ms_samples > 0).then_some(sums.tool_ms_sum),
        candidate_count: sums.candidate_count,
        failed_count: sums.failed_count,
        cancelled_count: sums.cancelled_count,
        missing_timing_count: sums.missing_timing_count,
        session_count: sums.session_count,
        latest_stream_ended_at: sums.latest_stream_ended_at,
        latest_response_ended_at: sums.latest_response_ended_at,
        output_scope: sums.output_scope.unwrap_or(OutputScope::Unknown),
        scope_mixed: sums.scope_mixed,
    }
}

// ==== 以下为本文件单元测试（brief 算术与纯性的贴身锁；SQL 口径在集成矩阵测试）====
#[cfg(test)]
mod tests {
    use super::*;

    /// brief 算术逐字：500/5s + 50/1s ⇒ 550/6、(2,3)；TTFT 独立 n
    #[test]
    fn brief_arithmetic_reproduces() {
        let sums = SpeedSums {
            stream_output_tokens: 550,
            stream_duration_ms: 6_000,
            stream_sample_count: 2,
            ttft_sum_ms: 950,
            ttft_sample_count: 3,
            ..SpeedSums::default()
        };
        let m = derive_metrics(&sums);
        assert!((m.stream.value.unwrap() - 550.0 / 6.0).abs() < 1e-9);
        assert_eq!((m.stream.sample_count, m.ttft_sample_count), (2, 3));
        assert_eq!(m.response.value, None, "响应口径独立：n=0 ⇒ null");
        assert!((m.avg_first_token_ms.unwrap() - 950.0 / 3.0).abs() < 1e-9);
    }

    /// 零合计不冒充 0 速度；混合 scope 折算 Unknown + scope_mixed
    #[test]
    fn zero_sums_never_masquerade_as_speed_and_scope_mixes() {
        let m = derive_metrics(&SpeedSums::default());
        assert_eq!(m.stream.value, None);
        assert_eq!(m.stream.output_tokens, 0);
        assert_eq!(m.model_ms, None);
        assert_eq!(m.tool_ms, None);
        assert_eq!(m.output_scope, OutputScope::Unknown);
        assert!(!m.scope_mixed);

        let mixed = derive_metrics(&SpeedSums {
            scope_mixed: true,
            candidate_count: 2,
            ..SpeedSums::default()
        });
        assert_eq!(mixed.output_scope, OutputScope::Unknown);
        assert!(mixed.scope_mixed);
    }

    /// 纯函数：同输入同输出、入参不被改动
    #[test]
    fn derive_metrics_has_no_hidden_state() {
        let sums = SpeedSums {
            stream_output_tokens: 100,
            stream_duration_ms: 1_000,
            stream_sample_count: 1,
            session_count: 1,
            latest_stream_ended_at: Some(42),
            output_scope: Some(OutputScope::ExclusiveOutput),
            ..SpeedSums::default()
        };
        let snapshot = sums;
        assert_eq!(derive_metrics(&sums), derive_metrics(&sums));
        assert_eq!(sums, snapshot, "入参不可被改动");
    }
}
