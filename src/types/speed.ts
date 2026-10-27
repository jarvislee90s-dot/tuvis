// V2 双指标速度公共类型（2026-10-26 计划 Task 9）——Task 6 Rust DTO 的 IPC camelCase 逐字镜像。
// Rust 真值源：src-tauri/src/services/tps/v2/model.rs（serde `rename_all = "camelCase"`，
// 枚举 `stream|response` / `15m|1h|5h|24h` / `tool|model|rate` / 来源状态 kebab-case）；
// **改动必须两端同步**。
//
// Option 纪律与 tps.ts 相同：Rust 侧 `?` 字段不加 skip_serializing_if——缺值 = 显式 `null`，
// TS 侧一律 `X | null`，消费方以 `=== null` 判「不可得」，**不得把 null 当 0**
// （「未知不填 0」：n=0 ⇒ value=null，充分统计的零合计不冒充 0 速度）。
import type { TpsOutputScope, TpsSeriesKey, TpsTimingMethod } from "@/types/tps";

/** 系列 key（契约 SeriesKey = tool + 原始 model + channel + method 四元组；复用既有 tps 镜像）。 */
export type SeriesKey = TpsSeriesKey;
/** 计时方法（复用既有镜像，含 V2 的 log-boundary-approx）。 */
export type TimingMethod = TpsTimingMethod;
/** 输出与思考的包含关系（复用既有镜像；scope_mixed=true 时折算 unknown，不猜单一口径）。 */
export type OutputScope = TpsOutputScope;

/** 指标模式：stream = 流式输出速度；response = 响应吞吐（含等待）。 */
export type SpeedMetricMode = "stream" | "response";

/** 区间档位（桶宽 1m/5m/10m/1h 与 Spec §7 档位表逐字）。 */
export type SpeedRange = "15m" | "1h" | "5h" | "24h";

/** 排序：rate 按当前模式值降序（null 排末）。 */
export type SpeedSort = "tool" | "model" | "rate";

/** 单指标速率（契约 SpeedRate 逐字段）：value=null 当 n=0；分子/分母是原始充分统计。 */
export interface SpeedRate {
  /** 1000×Σ输出/Σ毫秒；无有效样本为 null */
  value: number | null;
  outputTokens: number;
  durationMs: number;
  sampleCount: number;
}

/** 聚合指标（契约 SpeedMetrics 逐字段，15 个）。两个速度与 TTFT 各自独立有效集与 n；
 *  分类计数（candidate/failed/cancelled/missingTiming）不是任何速度的 n（Spec §5.3）。 */
export interface SpeedMetrics {
  stream: SpeedRate;
  response: SpeedRate;
  /** 平均首点等待（分母 = ttftSampleCount，独立）；无可对齐样本为 null */
  avgFirstTokenMs: number | null;
  ttftSampleCount: number;
  /** 可靠模型用时累计（含可计时失败/取消）；无任何可靠用时为 null */
  modelMs: number | null;
  /** 可靠工具用时累计（工具归属不唯一为 null，不填 0） */
  toolMs: number | null;
  candidateCount: number;
  failedCount: number;
  cancelledCount: number;
  /** 双窗皆缺失的候选数（缺计时为缺失，不填 0） */
  missingTimingCount: number;
  /** 有依据的会话数（DISTINCT session_key，null 键不计入） */
  sessionCount: number;
  /** 最近 stream 有效样本完成时刻（epoch ms） */
  latestStreamEndedAt: number | null;
  /** 最近 response 有效样本完成时刻（epoch ms） */
  latestResponseEndedAt: number | null;
  outputScope: OutputScope;
  /** 混合 scope 出现 ⇒ true 且 outputScope 折算 Unknown */
  scopeMixed: boolean;
}

/** 时间桶（契约 SpeedBucket 逐字段）：完成时刻归属互不重叠的 (start,end]；
 *  部分桶仅表示范围边缘未满，不表示已证明采集覆盖。 */
export interface SpeedBucket {
  startMs: number;
  endMs: number;
  isPartialBucket: boolean;
  metrics: SpeedMetrics;
}

/** 单 series 区间汇总（契约 SpeedSeriesSummary 逐字段）。 */
export interface SpeedSeriesSummary {
  seriesKey: SeriesKey;
  metrics: SpeedMetrics;
}

/** 来源双指标能力（契约 SpeedCapabilities 逐字段）；「支持」= 具备计算路径，不是数据保证
 *  ——未安装的来源也照报结构能力。reason 为 ASCII 机器 token（翻译一律经
 *  `view.ts` 的 `translateSpeedReason`——它是键面含 `:` 的 `speed.reason.*` 的唯一合法出口；
 *  未知 token 原样进技术详情）。 */
export interface SpeedCapabilities {
  stream: boolean;
  response: boolean;
  /** stream 缺失原因（如 Codex `stream-gap:insufficient-generation-timing`）；具备时为 null */
  streamReason: string | null;
  responseReason: string | null;
}

/** reason / streamReason 的已知稳定 token（与 Rust `v2/query.rs` 当前 token 集逐字对齐；
 *  Rust 新增 token 时与此处及 locales 的 `speed.reason.*` 三方同步补）。 */
export type SpeedReasonToken =
  | "stream-gap:insufficient-generation-timing"
  | "not-installed"
  | "never-read"
  | "read-failed"
  | "parser-errors"
  | "await-restart";

/** 来源接入状态（契约五值，kebab-case）。V2 没有 no-sample/insufficient-timing——
 *  「无区间记录是数据空态，不自动等于采集故障」；Codex = Ready + stream=false（能力位承载）。 */
export type SpeedSourceStatus =
  "ready" | "unconfigured" | "await-restart" | "needs-check" | "unsupported";

/** 来源视图（契约 SpeedSourceView 逐字段）：每注册来源一条；主界面只渲染名称/状态/能力，
 *  `detail` 仅供详情面板（主界面不直接渲染）。 */
export interface SpeedSourceView {
  source: string;
  status: SpeedSourceStatus;
  /** 安装事实（安装回执在册与否，与样本无关；无安装机制的来源恒 true） */
  installed: boolean;
  capabilities: SpeedCapabilities;
  /** 该源最近一次事实完成时刻（区间内；无事实为 null） */
  lastRecordAt: number | null;
  /** 状态成因的稳定机器 token（not-installed / never-read / read-failed / parser-errors /
   *  await-restart）；Ready 为 null。前端 i18n 映射。 */
  reason: string | null;
  /** 既有诊断原文（SourceState.detail 原样透传；主界面不渲染） */
  detail: string | null;
}

/** dashboard 查询（契约 SpeedDashboardQuery 逐字段）；search 为字面子串（空串 = 不过滤）。 */
export interface SpeedDashboardQuery {
  range: SpeedRange;
  selection: SeriesKey | null;
  metricMode: SpeedMetricMode;
  toolId: string | null;
  search: string;
  sort: SpeedSort;
}

/** dashboard 快照（契约 SpeedDashboardSnapshot 逐字段）：列表 / 总览 / 曲线同一事实快照；
 *  总授权关闭 ⇒ disabled=true 且 series/buckets 空、选择 null（历史不删；来源状态仍解析）。 */
export interface SpeedDashboardSnapshot {
  asOf: number;
  rangeFrom: number;
  rangeTo: number;
  disabled: boolean;
  sourceStates: SpeedSourceView[];
  series: SpeedSeriesSummary[];
  selectedSeries: SeriesKey | null;
  buckets: SpeedBucket[];
}
