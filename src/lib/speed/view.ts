// V2 速度显示状态纯函数（Task 9）——「口径切换不破坏用户选择」「未知不填 0」的承载层。
// 全部为纯函数：无 IPC、无副作用、不改入参；Tasks 10–12（图表 / 来源面板 / dashboard）逐字消费。
// 唯一的 i18n 触点是 `translateSpeedReason`（t 由调用方注入，仍是纯计算）——
// 它是键面含 `:` 的 `speed.reason.*` 的**唯一合法出口**，下游不得裸调 `t()`。
import type { TFunction } from "i18next";
import { usageAgentLabel } from "@/lib/usage/format";
import type {
  SeriesKey,
  SpeedCapabilities,
  SpeedMetricMode,
  SpeedMetrics,
  SpeedRate,
  SpeedSeriesSummary,
  SpeedSort,
} from "@/types/speed";

/** 小样本阈值：沿既有 TPS 小输出口径（样本输出合计 < 50 标小样本，policy.rs 同值）。 */
const SMALL_SAMPLE_OUTPUT_TOKENS = 50;

/**
 * 实际生效的指标模式（Spec §5.3「口径切换」）：偏好 stream 但来源缺流式能力 ⇒ 临时
 * 显示 response（Codex 场景）；其余情形一律原样返回偏好。
 * **只读能力位，与样本无关**——区间 n=0 不构成切换理由（缺能力不伪装数值，有能力不因
 * 空窗擅自换单位）；本函数是纯计算，用户的偏好由调用方持有、这里绝不改写。
 */
export function effectiveMetric(
  preferred: SpeedMetricMode,
  capabilities: SpeedCapabilities
): SpeedMetricMode {
  if (preferred === "stream" && !capabilities.stream) return "response";
  return preferred;
}

/** 当前模式的速率（含 null：无有效样本原样透出，不伪装 0 速度）。 */
export function rateForMode(metrics: SpeedMetrics, mode: SpeedMetricMode): SpeedRate {
  return mode === "stream" ? metrics.stream : metrics.response;
}

/**
 * 稳定 series 键串：`{tool,model,channel,method}` 四元组规范化 JSON（channel 显式 null 化，
 * 兼容 wire 缺键 / undefined / null 三态）。用途：queryKey 成员、React key、悬停对位——
 * 同一 series 跨快照、跨重启逐字稳定。
 */
export function seriesKeyId(key: SeriesKey): string {
  return JSON.stringify({
    tool: key.tool,
    model: key.model,
    channel: key.channel ?? null,
    method: key.method,
  });
}

/** 不变式比较（code-unit 字典序；不依赖 locale，测试与运行环境无 ICU 漂移）。 */
function cmp(x: string, y: string): number {
  if (x < y) return -1;
  if (x > y) return 1;
  return 0;
}

/** channel 比较：null（无渠道概念）排前，其余字典序。 */
function cmpChannel(a: string | null, b: string | null): number {
  if (a === b) return 0;
  if (a === null) return -1;
  if (b === null) return 1;
  return cmp(a, b);
}

/**
 * 默认稳定序：tool → model → channel（与后端 `sort_series` 的 fallback_cmp 三元组一致，
 * 不含 method——后端比较器没有这一层）。同为稳定排序（ES2019+ `Array.prototype.sort`），
 * 服务端 SQL `ORDER BY …, method` 给出的同键相对次序在比较器并列时原样保留。
 * 比较引擎备注（scoped 评审 ④）：前端按 UTF-16 码元、后端按字节序，仅对纯 ASCII
 * 标识符等价；现实 tool/model/channel 键均为 ASCII，若未来出现非 ASCII 键，两引擎
 * 并列序可能不同（已知近似，注释即契约）。
 */
/**
 * 组合的显示名（工具别名 → 模型 [· 渠道]）：列表行与详情可选框共用同一拼法，
 * 避免两处各拼一份后漂移（第四轮实机反馈 ②）。
 */
export function seriesRowLabel(key: SeriesKey): string {
  const toolLabel = usageAgentLabel(key.tool);
  return key.channel
    ? `${toolLabel} · ${key.model} · ${key.channel}`
    : `${toolLabel} · ${key.model}`;
}

function stableSeriesCompare(a: SeriesKey, b: SeriesKey): number {
  return cmp(a.tool, b.tool) || cmp(a.model, b.model) || cmpChannel(a.channel, b.channel);
}

/** 列表客户端排序键（2026-10-27 实机反馈 #5）：组合名 / 当前模式速度 / 有依据的会话数。 */
export type SpeedListSortKey = "name" | "rate" | "sessions";

/** 排序方向。 */
export type SortDirection = "asc" | "desc";

/**
 * 各排序键的默认方向（2026-10-27 用户裁决）：组合名升序，速度与会话数降序。
 * 切换排序键时由调用方取此默认，方向切换按钮再行翻转。
 */
export function defaultDirectionFor(key: SpeedListSortKey): SortDirection {
  return key === "name" ? "asc" : "desc";
}

/** 按方向翻转一个已算好的比较结果（asc 原样，desc 取反）。 */
function flip(result: number, direction: SortDirection): number {
  return direction === "desc" ? -result : result;
}

/**
 * 列表排序（Spec §8.1；2026-10-27 实机反馈 #5 扩展）。
 *  * `name` = tool→model→channel 稳定序（与后端 `sort_series` fallback_cmp 三元组一致），
 *    方向翻转**整个比较器结果**（desc 时 null 渠道同样落到同工具组的后面）；
 *  * `rate` = 当前模式的速度值，**无数值（null）一律排末（与方向无关）**、同值并列
 *    落回稳定序；asc 低→高、desc 高→低；
 *  * `sessions` = `metrics.sessionCount`，并列落回稳定序；
 *  * `tool` / `model` = 旧 wire 键（查询载荷的服务端稳定基序仍用 `tool`；mockFixtures
 *    与既有测试沿用）——恒为升序稳定序，`direction` 不生效；
 *  * `direction` 缺省 = `defaultDirectionFor(key)`（`rate` 由此保持旧的降序语义，
 *    旧三参调用方行为逐字不变）。**不改入参**——始终返回新数组（React 渲染路径可安全直用）。
 */
export function sortSeries(
  series: SpeedSeriesSummary[],
  sort: SpeedSort | SpeedListSortKey,
  mode: SpeedMetricMode,
  direction?: SortDirection
): SpeedSeriesSummary[] {
  const sorted = [...series];
  const dir: SortDirection = direction ?? (sort === "name" ? "asc" : "desc");
  const stable = (x: SpeedSeriesSummary, y: SpeedSeriesSummary): number =>
    stableSeriesCompare(x.seriesKey, y.seriesKey);
  if (sort === "rate") {
    sorted.sort((x, y) => {
      const xv = rateForMode(x.metrics, mode).value;
      const yv = rateForMode(y.metrics, mode).value;
      if (xv === null && yv === null) return stable(x, y);
      if (xv === null) return 1; // 无数值一律排末（与方向无关）
      if (yv === null) return -1;
      return flip(xv - yv, dir) || stable(x, y); // 升序比较结果按方向翻转
    });
    return sorted;
  }
  if (sort === "sessions") {
    sorted.sort(
      (x, y) => flip(x.metrics.sessionCount - y.metrics.sessionCount, dir) || stable(x, y)
    );
    return sorted;
  }
  if (sort === "name") {
    sorted.sort((x, y) => flip(stable(x, y), dir));
    return sorted;
  }
  // 旧 wire 键：tool / model 恒为升序稳定序（方向不生效）
  sorted.sort((x, y) =>
    sort === "model"
      ? cmp(x.seriesKey.model, y.seriesKey.model) || stableSeriesCompare(x.seriesKey, y.seriesKey)
      : stableSeriesCompare(x.seriesKey, y.seriesKey)
  );
  return sorted;
}

/**
 * 小样本判定（沿既有 TPS 口径）：有样本且输出合计 < 50 才标小样本。
 * n=0 不是「小样本」而是「无值」（value=null，四格显示 —，不标小样本角标）。
 */
export function isSmallSample(rate: SpeedRate): boolean {
  return rate.sampleCount > 0 && rate.outputTokens < SMALL_SAMPLE_OUTPUT_TOKENS;
}

/**
 * reason / streamReason 的 i18n 出口（Tasks 10–12 渲染二者的**唯一合法入口**）。
 * 键面 = token 原文（locales 的 `speed.reason.*`），而 Codex token
 * `stream-gap:insufficient-generation-timing` 含冒号——i18next 默认把 `:` 解析为
 * 命名空间分隔符，裸调 `t("speed.reason." + token)` 会**静默**回退显示原始键串。
 * 本函数统一传 `nsSeparator: false`（键结构仍是 `speed.reason.<token>` 三层，故
 * keySeparator 保持默认 `.`）；查键未命中（未知 token / 键缺失）时**如实返回原文**，
 * 与「未知 token 原样进技术详情」的降级纪律一致。
 */
export function translateSpeedReason(t: TFunction, token: string): string {
  const key = `speed.reason.${token}`;
  const text: unknown = t(key, { nsSeparator: false });
  return typeof text === "string" && text !== key ? text : token;
}
