// V2 速度图表纯数学层（2026-10-26 计划 Task 10）——「画什么」的唯一出处，**零 React、零 DOM**：
// jsdom 量不到像素、`getBoundingClientRect()` 恒 0（TrendChart 同款纪律），组件只消费本层
// 的数值投影与几何分段；测试直测本层即可。
//
// 四条纪律：
//  ① **x 按完整 [from,to] 铺满**（Spec §8.4「横轴按完整范围/宽度独立排布，不用挤在样本
//     位置的首/中/末三个时刻」）：桶下标 → 绘图区等距 x，与样本在桶内的落点无关；
//  ② **y 从 0 起 + 易读刻度**（Spec §8.4）：max 收敛到 4 步的整齐上界（1/2/2.5/5×10^k）；
//  ③ **实测线只在有值相邻桶间连续**（Spec §8.5）：`segmentSeries` 把序列切成有值连续段；
//     缺口内的虚线连接与阴影由组件画，但**有没有缺口、缺口在哪**由本层判定；
//  ④ **固定 = 快照不是引用**（brief 契约）：`pinSnapshot` 深拷贝 metrics——5 秒轮询
//     refetch 产生新对象也绝不改变已固定的读数；绝不固定数组下标。
import type { TFunction } from "i18next";
import { rateForMode } from "@/lib/speed/view";
import type { SpeedBucket, SpeedMetricMode, SpeedMetrics } from "@/types/speed";

/** 「不可得」占位（value/modelMs/toolMs/avgFirstTokenMs 为 null）。速度域自持一份，
 *  语义与 usage 域 `format.ts` 的同名常量一致（未知不填 0，显示 —）。 */
export const EM_DASH = "—";

/** 固定读数（契约逐字）：`seriesKey: unknown`（身份由父层注入，图表不解释），
 *  `metricsSnapshot` 是点击时刻的**深拷贝**——不是数组下标、不是活引用。 */
export interface PinnedBucket {
  seriesKey: unknown;
  mode: SpeedMetricMode;
  startMs: number;
  endMs: number;
  metricsSnapshot: SpeedMetrics;
}

/** 桶在当前模式下有没有速度值（value=null 的桶不参与实测线，也不推算速度）。 */
export function bucketHasValue(bucket: SpeedBucket, mode: SpeedMetricMode): boolean {
  return rateForMode(bucket.metrics, mode).value !== null;
}

/** 有值连续段（双闭下标区间 [from,to]）；两段之间即缺口（Spec §8.5「实测线断开」）。 */
export interface SeriesSegment {
  from: number;
  to: number;
}

/** 把桶序列按「当前模式有值」切成连续段：无值桶是断点。全空 → []；全有值 → 单段。 */
export function segmentSeries(buckets: SpeedBucket[], mode: SpeedMetricMode): SeriesSegment[] {
  const out: SeriesSegment[] = [];
  let start = -1;
  for (let i = 0; i < buckets.length; i++) {
    if (bucketHasValue(buckets[i]!, mode)) {
      if (start < 0) start = i;
    } else if (start >= 0) {
      out.push({ from: start, to: i - 1 });
      start = -1;
    }
  }
  if (start >= 0) out.push({ from: start, to: buckets.length - 1 });
  return out;
}

/**
 * y 轴刻度：从 0 起、恒 4 步 5 条刻度线，max 收敛到「整齐上界」。
 * 步长候选 1/2/2.5/5×10^k 中取能盖住 maxValue 的最小者（86.4 → 步长 25 → max 100）。
 * 全空/畸形（≤0 或非有限）→ max=1 的名义坐标（仍然从 0 起、可画轴，不除零、不出 NaN）。
 */
export function niceScale(maxValue: number): { max: number; ticks: number[] } {
  if (!Number.isFinite(maxValue) || maxValue <= 0) {
    return { max: 1, ticks: [0, 0.25, 0.5, 0.75, 1] };
  }
  const raw = maxValue / 4;
  const mag = 10 ** Math.floor(Math.log10(raw));
  const norm = raw / mag;
  const step = (norm <= 1 ? 1 : norm <= 2 ? 2 : norm <= 2.5 ? 2.5 : norm <= 5 ? 5 : 10) * mag;
  const max = step * 4;
  return { max, ticks: [0, step, step * 2, step * 3, max] };
}

/** 刻度步长候选（分钟）：覆盖 15m–24h 四档都能落进 4–7 个标签（Spec §8.4）。 */
const MINUTE = 60_000;
const TICK_STEPS_MS = [1, 2, 5, 10, 15, 30, 60, 120, 180, 240, 360, 720, 1440].map(
  (m) => m * MINUTE
);

/**
 * x 轴时间刻度（纯时间几何，与桶无关——「按完整范围/宽度独立排布」）：
 * 在候选步长里选「刻度数 ∈ [4,7] 且最接近 5」的一档；刻度对齐到**本地墙钟**的步长整倍数
 * （对齐计算在「本地时差归零」的坐标里做，避免 epoch 整倍数落在本地 02:00 这类怪刻度）。
 * 极小/畸形范围退化为范围 4 等分（恒 5 个，仍在 4–7 内）。
 */
export function timeTicks(from: number, to: number): number[] {
  const span = Math.max(1, to - from);
  let best: { step: number; count: number } | null = null;
  for (const step of TICK_STEPS_MS) {
    const count = Math.floor(span / step) + 1;
    if (count >= 4 && count <= 7 && (!best || Math.abs(count - 5) < Math.abs(best.count - 5))) {
      best = { step, count };
    }
  }
  if (!best) {
    return [0, 1, 2, 3, 4].map((f) => from + (span * f) / 4);
  }
  // 本地墙钟对齐：把时差移出后再取整（getTimezoneOffset = 本地→UTC 的分钟数）
  const offsetMs = new Date(from).getTimezoneOffset() * MINUTE;
  const first = Math.ceil((from + offsetMs) / best.step) * best.step - offsetMs;
  const out: number[] = [];
  for (let t = first; t <= to; t += best.step) out.push(t);
  return out.length > 0 ? out : [from];
}

/** metrics 深拷贝（两层足够：metrics 与其两个 rate 都是扁平对象）——固定快照的载体。 */
export function cloneMetrics(m: SpeedMetrics): SpeedMetrics {
  return { ...m, stream: { ...m.stream }, response: { ...m.response } };
}

/** 点击固定：携快照（`cloneMetrics`），不携下标、不携活引用（brief 契约逐字）。 */
export function pinSnapshot(
  bucket: SpeedBucket,
  seriesKey: unknown,
  mode: SpeedMetricMode
): PinnedBucket {
  return {
    seriesKey,
    mode,
    startMs: bucket.startMs,
    endMs: bucket.endMs,
    metricsSnapshot: cloneMetrics(bucket.metrics),
  };
}

// ==== 显示格式化（速度域自持；与 usage 域互不依赖）====

const pad2 = (v: number): string => String(v).padStart(2, "0");

/** 本地时钟 HH:mm（x 轴标签与 tooltip 短跨度用）。 */
export function formatClock(ms: number): string {
  const d = new Date(ms);
  return `${pad2(d.getHours())}:${pad2(d.getMinutes())}`;
}

/** 本地「月-日 HH:mm」：tooltip 在跨日范围/跨日桶时带日期（brief：24h 跨日期桶显示日期）。 */
export function formatClockWithDate(ms: number): string {
  const d = new Date(ms);
  return `${d.getMonth() + 1}-${d.getDate()} ${formatClock(ms)}`;
}

/** 两时刻之间是否跨本地日期（决定 tooltip 是否显示日期）。 */
export function spansLocalDate(startMs: number, endMs: number): boolean {
  return new Date(startMs).toDateString() !== new Date(endMs).toDateString();
}

/** 速度值显示（沿既有 TPS 口径）：≥100 取整，否则 1 位小数；null/NaN 一律「—」，
 *  绝不显示 0 或插值（空桶不推算速度，Spec §8.3）。 */
export function formatRateValue(v: number | null | undefined): string {
  if (typeof v !== "number" || !Number.isFinite(v)) return EM_DASH;
  return v >= 100 ? String(Math.round(v)) : v.toFixed(1);
}

/** 轴刻度值显示（只用于 y 轴/网格标签）：整数不带小数（0 → "0"、25 → "25"），
 *  非 1 位小数；「从 0 开始」的基线因此印作 "0" 而不是 "0.0"。 */
export function formatAxisValue(v: number): string {
  if (!Number.isFinite(v)) return EM_DASH;
  return Number.isInteger(v) ? String(v) : v.toFixed(1);
}

/** 首 Token 平均等待：恒「X.XX s」两位小数（brief 指定）；null 如实「—」，不伪造秒值。 */
export function formatFirstTokenMs(ms: number | null | undefined): string {
  if (typeof ms !== "number" || !Number.isFinite(ms)) return EM_DASH;
  return `${(ms / 1_000).toFixed(2)} s`;
}

/** 时长显示（毫秒/秒/分钟三档，1 位小数；单位词走 i18n，t 由调用方注入——纯计算）。
 *  四格「累计用时」与 tooltip「桶宽」共用同一口径。 */
export function formatDurationI18n(ms: number | null | undefined, t: TFunction): string {
  if (typeof ms !== "number" || !Number.isFinite(ms)) return EM_DASH;
  if (ms >= 60_000) return t("speed.duration.min", { n: (ms / 60_000).toFixed(1) });
  if (ms >= 1_000) return t("speed.duration.s", { n: (ms / 1_000).toFixed(1) });
  return t("speed.duration.ms", { n: Math.round(ms) });
}

/** 当前模式的单位文案（stream=TPS / response=Token/s；文案在 locales 的 speed.unit.*）。 */
export function unitForMode(mode: SpeedMetricMode, t: TFunction): string {
  return mode === "stream" ? t("speed.unit.tps") : t("speed.unit.tokenPerSec");
}
