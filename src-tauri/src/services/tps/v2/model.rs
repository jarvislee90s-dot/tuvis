//! V2 双指标 DTO 层（2026-10-26 桌面速度 V2 计划「公共类型与接口契约」逐字镜像）。
//!
//! 命名纪律：本层一律 Rust snake_case；IPC camelCase 镜像由 serde `rename_all` 承担，
//! 序列化串与契约逐字（`stream|response` / `15m|1h|5h|24h` / `tool|model|rate` /
//! 来源状态 kebab-case）。Option 语义与 tps::model 相同：`?` 字段**不加**
//! `skip_serializing_if`——缺值时必须序列化出显式 `null`（契约「所有可空字段显式 null」）。
//!
//! 「未知不填 0」纪律由 `Option` 与 `derive_metrics` 共同承载：充分统计的零合计
//! **绝不冒充 0 速度**（n=0 ⇒ value=null，Spec §7「空桶不推算速度」）。
//!
//! 本层只放类型与枚举；聚合充分统计在 sums.rs（纯 `derive_metrics`），
//! 区间/桶/来源状态的只读查询在 query.rs。

use serde::{Deserialize, Serialize};

use crate::services::tps::model::{OutputScope, SeriesKey};

// ==== 模式 / 档位 / 排序（契约逐字三枚举）====

/// 指标模式（契约 `SpeedMetricMode={Stream,Response}` → `stream|response`）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpeedMetricMode {
    /// 流式输出速度（1000×Σ流式有效输出 / Σ生成毫秒）
    Stream,
    /// 响应吞吐·含等待（1000×Σ响应有效输出 / Σ响应毫秒）
    Response,
}

/// 区间档位（契约 `SpeedRange={Last15m,Last1h,Last5h,Last24h}` → `15m|1h|5h|24h`；
/// 桶宽 1m/5m/10m/1h 与 Spec §7 档位表逐字）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpeedRange {
    #[serde(rename = "15m")]
    Last15m,
    #[serde(rename = "1h")]
    Last1h,
    #[serde(rename = "5h")]
    Last5h,
    #[serde(rename = "24h")]
    Last24h,
}

impl SpeedRange {
    /// 滚动窗长 ms（`range_from = now − window`）
    pub fn window_ms(self) -> i64 {
        match self {
            SpeedRange::Last15m => 15 * 60_000,
            SpeedRange::Last1h => 3_600_000,
            SpeedRange::Last5h => 5 * 3_600_000,
            SpeedRange::Last24h => 24 * 3_600_000,
        }
    }

    /// 每桶时长 ms（Spec §7 档位表逐字：1m/5m/10m/1h）
    pub fn bucket_width_ms(self) -> i64 {
        match self {
            SpeedRange::Last15m => 60_000,
            SpeedRange::Last1h => 5 * 60_000,
            SpeedRange::Last5h => 10 * 60_000,
            SpeedRange::Last24h => 3_600_000,
        }
    }
}

/// 排序（契约 `SpeedSort={Tool,Model,Rate}` → `tool|model|rate`；rate 按当前模式值降序）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpeedSort {
    Tool,
    Model,
    Rate,
}

// ==== 聚合指标 ====

/// 单指标速率（契约 `SpeedRate` 逐字段）：`value = null` 当 n=0——零合计不冒充 0 速度；
/// 分子/分母是原始充分统计（供详情核对），n 为该口径有效样本数。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeedRate {
    /// 1000×Σ输出/Σ毫秒；无有效样本为 null
    pub value: Option<f64>,
    pub output_tokens: i64,
    pub duration_ms: i64,
    pub sample_count: i64,
}

/// 聚合指标（契约 `SpeedMetrics` 逐字段）：两个速度与 TTFT 各自独立有效集与 n；
/// 分类计数（candidate/failed/cancelled/missingTiming）不是任何速度的 n（Spec §5.3）。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeedMetrics {
    pub stream: SpeedRate,
    pub response: SpeedRate,
    /// 平均首点等待（分母 = ttft_sample_count，独立）；无可对齐样本为 null
    pub avg_first_token_ms: Option<f64>,
    pub ttft_sample_count: i64,
    /// 可靠模型用时累计（含可计时失败/取消）；无任何可靠用时为 null
    pub model_ms: Option<i64>,
    /// 可靠工具用时累计（工具归属不唯一为 null，不填 0）
    pub tool_ms: Option<i64>,
    pub candidate_count: i64,
    pub failed_count: i64,
    pub cancelled_count: i64,
    /// 双窗皆缺失的候选数（缺计时为缺失，不填 0）
    pub missing_timing_count: i64,
    /// 有依据的会话数（COUNT(DISTINCT session_key)，null 键不计入）
    pub session_count: i64,
    /// 最近 stream 有效样本完成时刻（epoch ms）
    pub latest_stream_ended_at: Option<i64>,
    /// 最近 response 有效样本完成时刻（epoch ms）
    pub latest_response_ended_at: Option<i64>,
    pub output_scope: OutputScope,
    /// 混合 scope 出现 ⇒ true 且 output_scope 折算 Unknown（不猜单一口径）
    pub scope_mixed: bool,
}

/// 时间桶（契约 `SpeedBucket` 逐字段）：完成时刻归属互不重叠的 (start,end]；
/// 部分桶仅表示范围边缘未满，不表示已证明采集覆盖（Spec §7）。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeedBucket {
    pub start_ms: i64,
    pub end_ms: i64,
    pub is_partial_bucket: bool,
    pub metrics: SpeedMetrics,
}

/// 单 series 区间汇总（契约 `SpeedSeriesSummary` 逐字段）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeedSeriesSummary {
    pub series_key: SeriesKey,
    pub metrics: SpeedMetrics,
}

// ==== 来源能力与状态 ====

/// 来源双指标能力（契约 `SpeedCapabilities` 逐字段）；「支持」= 具备计算路径，
/// 不是数据保证（Spec §5.2）——未安装的来源也照报结构能力。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeedCapabilities {
    pub stream: bool,
    pub response: bool,
    /// stream 缺失原因（如 Codex 流式缺口）；具备时为 null
    pub stream_reason: Option<String>,
    pub response_reason: Option<String>,
}

/// 来源接入状态（契约 `SpeedSourceStatus` 五值，kebab-case）。
/// 与 V1 [`crate::services::tps::query::TpsSourceStatus`] 的分野：V2 没有 no-sample/
/// insufficient-timing——「无区间记录是数据空态，不自动等于采集故障」（Spec §8 尾段）；
/// Codex 旧流式缺口前缀**不再**降级整体状态（它 Ready，stream=false 由能力位承载）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SpeedSourceStatus {
    /// 已接入且健康（有无数据均算）
    Ready,
    /// 未安装 / 从未产出读取批
    Unconfigured,
    /// 已安装统计键但尚无事实（「下次启动生效」等待态）
    AwaitRestart,
    /// 待检查：最近读取失败（ok=false）或解析错误（detail 以 `parser-errors=` 起头）
    NeedsCheck,
    /// 结构性不可能（当前静态能力表无此来源；保留枚举臂备将来）
    Unsupported,
}

/// 来源视图（契约 `SpeedSourceView` 逐字段）：每注册来源一条；主界面只渲染
/// 名称/状态/能力，`detail` 仅供详情面板（前端职责，后端只如实携带）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeedSourceView {
    pub source: String,
    pub status: SpeedSourceStatus,
    /// 安装事实（安装回执在册与否，与样本无关；无安装机制的来源恒 true）
    pub installed: bool,
    pub capabilities: SpeedCapabilities,
    /// 该源最近一次事实完成时刻（区间内；无事实为 null）
    pub last_record_at: Option<i64>,
    /// 状态成因的稳定机器 token（`not-installed` / `never-read` / `read-failed` /
    /// `parser-errors` / `await-restart`）；Ready 为 null。前端 i18n 映射。
    pub reason: Option<String>,
    /// 既有诊断原文（SourceState.detail 原样透传；主界面不渲染）
    pub detail: Option<String>,
}

// ==== 查询入口 ====

/// dashboard 查询（契约 `SpeedDashboardQuery` 逐字段）；`search` 为字面子串
/// （后端转义 LIKE 通配符），空串 = 不过滤。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeedDashboardQuery {
    pub range: SpeedRange,
    pub selection: Option<SeriesKey>,
    pub metric_mode: SpeedMetricMode,
    pub tool_id: Option<String>,
    pub search: String,
    pub sort: SpeedSort,
}

/// dashboard 快照（契约 `SpeedDashboardSnapshot` 逐字段）：列表 / 总览 / 曲线
/// 同一事实快照；总授权关闭 ⇒ disabled=true 且 series/buckets 空、选择 None
/// （历史不删；来源状态仍解析——安装事实不依赖样本）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeedDashboardSnapshot {
    pub as_of: i64,
    pub range_from: i64,
    pub range_to: i64,
    pub disabled: bool,
    pub source_states: Vec<SpeedSourceView>,
    pub series: Vec<SpeedSeriesSummary>,
    pub selected_series: Option<SeriesKey>,
    pub buckets: Vec<SpeedBucket>,
}

// ==== 桶几何 ====

/// 桶几何（内部产物；公共 [`SpeedBucket`] 携带 metrics 后即契约面）。
/// span 为半开区间 (start, end]，裁剪后的首/末桶 carry `is_partial_bucket = true`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BucketSpan {
    pub start_ms: i64,
    pub end_ms: i64,
    pub is_partial_bucket: bool,
}

// ==== 以下为本文件单元测试（serde 串形态逐字锁）====
#[cfg(test)]
mod tests {
    use super::*;

    /// 契约枚举的序列化串逐字（`stream|response` / `15m…24h` / `tool|model|rate` /
    /// kebab-case 状态）——IPC 镜像的 Rust 半边，前端形态锁在集成测试
    #[test]
    fn enum_wire_forms_are_verbatim() {
        assert_eq!(
            serde_json::to_string(&SpeedMetricMode::Stream).unwrap(),
            "\"stream\""
        );
        assert_eq!(
            serde_json::to_string(&SpeedMetricMode::Response).unwrap(),
            "\"response\""
        );
        assert_eq!(
            serde_json::to_string(&SpeedRange::Last15m).unwrap(),
            "\"15m\""
        );
        assert_eq!(
            serde_json::to_string(&SpeedRange::Last1h).unwrap(),
            "\"1h\""
        );
        assert_eq!(
            serde_json::to_string(&SpeedRange::Last5h).unwrap(),
            "\"5h\""
        );
        assert_eq!(
            serde_json::to_string(&SpeedRange::Last24h).unwrap(),
            "\"24h\""
        );
        assert_eq!(serde_json::to_string(&SpeedSort::Tool).unwrap(), "\"tool\"");
        assert_eq!(
            serde_json::to_string(&SpeedSort::Model).unwrap(),
            "\"model\""
        );
        assert_eq!(serde_json::to_string(&SpeedSort::Rate).unwrap(), "\"rate\"");
        for (v, s) in [
            (SpeedSourceStatus::Ready, "ready"),
            (SpeedSourceStatus::Unconfigured, "unconfigured"),
            (SpeedSourceStatus::AwaitRestart, "await-restart"),
            (SpeedSourceStatus::NeedsCheck, "needs-check"),
            (SpeedSourceStatus::Unsupported, "unsupported"),
        ] {
            assert_eq!(serde_json::to_string(&v).unwrap(), format!("\"{s}\""));
            let back: SpeedSourceStatus = serde_json::from_str(&format!("\"{s}\"")).unwrap();
            assert_eq!(back, v);
        }
    }

    /// 档位表（Spec §7）逐字：窗长与桶宽
    #[test]
    fn range_window_and_bucket_widths_are_verbatim() {
        assert_eq!(SpeedRange::Last15m.window_ms(), 15 * 60_000);
        assert_eq!(SpeedRange::Last15m.bucket_width_ms(), 60_000);
        assert_eq!(SpeedRange::Last1h.window_ms(), 3_600_000);
        assert_eq!(SpeedRange::Last1h.bucket_width_ms(), 5 * 60_000);
        assert_eq!(SpeedRange::Last5h.window_ms(), 5 * 3_600_000);
        assert_eq!(SpeedRange::Last5h.bucket_width_ms(), 10 * 60_000);
        assert_eq!(SpeedRange::Last24h.window_ms(), 24 * 3_600_000);
        assert_eq!(SpeedRange::Last24h.bucket_width_ms(), 3_600_000);
    }
}
