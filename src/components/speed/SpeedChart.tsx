// V2 速度桶状趋势图（2026-10-26 计划 Task 10，Spec §8.3–8.5）——纯 SVG 自绘、零新增依赖。
// 消费方：Task 12 的 SpeedDashboard（组合页把本组件与 SpeedMetrics 同帧装配）。
//
// 交互契约（Spec §8.3 逐字）：
//  · 默认区间汇总；悬停桶 = 细竖线 + 真实 DOM tooltip（实际起止/桶宽/n）+ 四格联动
//    （联动由父层经 onHover 装配）；**空桶不推算速度**——tooltip 无值无插值，只给「无有效样本」；
//  · 移开恢复汇总；点击固定该次读数**快照**（`pinSnapshot` 深拷贝，不固定数组下标/活引用，
//    refetch 换数据不漂移）；再次点击同一桶 / Esc / 切范围·组合·模式解除固定（onPin(null)）；
//  · 键盘可操作：外层可聚焦，ArrowLeft/Right 移动选桶，Enter/Space 等价点击固定，
//    Escape 清除悬停与固定——全部不依赖 pointer，不依赖原生 title；
//  · 缺口（Spec §8.5）：实测折线只在有值相邻桶间连续（`segmentSeries` 分段）；缺口区画
//    阴影层；灰虚线连接画在**独立图层**且 `pointer-events:none`，不参与悬停命中与 tooltip；
//    短图例「虚线仅连接，无样本」；`gapStyle="band"` 为严格断开（只阴影、无虚线）。
// 几何纪律见 `@/lib/speed/chart.ts` 文件头（x 铺满 [from,to]、y 从 0 起、4–7 标签）。
import { useCallback, useEffect, useId, useRef, useState } from "react";
import type { KeyboardEvent as ReactKeyboardEvent, ReactElement, ReactNode } from "react";
import { useTranslation } from "react-i18next";
import {
  formatAxisValue,
  formatClock,
  formatClockWithDate,
  formatDurationI18n,
  formatRateValue,
  niceScale,
  pinSnapshot,
  segmentSeries,
  spansLocalDate,
  timeTicks,
  unitForMode,
} from "@/lib/speed/chart";
import type { PinnedBucket } from "@/lib/speed/chart";
import { rateForMode } from "@/lib/speed/view";
import type { SpeedBucket, SpeedMetricMode } from "@/types/speed";

export type { PinnedBucket } from "@/lib/speed/chart";

/** viewBox 版式常量（数值坐标，非像素测量）：左右留白容得下轴标签，底部给 x 标签留位。 */
export const SPEED_CHART_PAD = { l: 40, r: 16, t: 14, b: 26 } as const;
const DEFAULT_WIDTH = 880;
const DEFAULT_HEIGHT = 240;

export interface SpeedChartProps {
  buckets: SpeedBucket[];
  mode: SpeedMetricMode;
  /** 完整范围（epoch ms）——x 轴按它铺满，与桶是否覆盖到边缘无关 */
  from: number;
  to: number;
  /** 悬停回执：进入某桶给该桶，移出/清除给 null（父层据此切四格） */
  onHover?: (b: SpeedBucket | null) => void;
  /** 固定回执：点击给快照，解除给 null（父层据此把快照交给 SpeedMetrics） */
  onPin?: (p: PinnedBucket | null) => void;
  /** 受控固定态（dashboard 持有「返回区间汇总」的解除权）；缺省时组件内部自持 */
  pinned?: PinnedBucket | null;
  /** 缺口样式：dash=灰虚线仅连接（默认，Task 12 接用户开关）/ band=严格断开只阴影 */
  gapStyle?: "dash" | "band";
  /**
   * 固定/悬停自动解除的哨兵键（可选加成，2026-10-26 Task 12 裁决）。提供时，「切范围/
   * 组合/模式解除固定」的判定**只看该键**——滚动窗口的 from/to 每 5 秒随 asOf 前移不算
   * 「切换」，固定与悬停不丢；缺省保持旧行为（观察 mode/from/to 原值），既有调用方零改动。
   */
  clearSignalKey?: string;
  /** 固定对象携带的 series 身份（原样进 PinnedBucket.seriesKey；图表不解释） */
  seriesKey?: unknown;
  /**
   * 口径脚注（可选，二轮实机反馈 ④，2026-10-27）：渲染在图例（`speed-chart-legend`）之后、
   * 卡底之前——口径说明从详情区的独立段落（speed-mode-hint）迁入图注，紧贴图例读。
   * 纯插槽：本组件不解释内容、不做 i18n。
   */
  footnote?: ReactNode;
  width?: number;
  height?: number;
}

/** path 坐标收敛到 2 位小数：浮点尾巴让 d 又长又抖（TrendChart 同款）。 */
const n2 = (v: number): number => Number(v.toFixed(2));

/** 折线 path：1 点只有 M（单点也落点），其余 M+L 直连（桶是离散聚合，不做平滑）。 */
function polylinePath(pts: readonly { x: number; y: number }[]): string {
  if (pts.length === 0) return "";
  let d = `M ${n2(pts[0]!.x)} ${n2(pts[0]!.y)}`;
  for (let i = 1; i < pts.length; i++) d += ` L ${n2(pts[i]!.x)} ${n2(pts[i]!.y)}`;
  return d;
}

const clamp = (v: number, lo: number, hi: number): number => Math.min(hi, Math.max(lo, v));

export function SpeedChart({
  buckets,
  mode,
  from,
  to,
  onHover,
  onPin,
  pinned,
  gapStyle = "dash",
  clearSignalKey,
  seriesKey,
  footnote,
  width = DEFAULT_WIDTH,
  height = DEFAULT_HEIGHT,
}: SpeedChartProps): ReactElement {
  const { t } = useTranslation();
  const hatchId = `speed-gap-hatch${useId().replace(/[^a-zA-Z0-9_-]/g, "")}`;
  const [innerPinned, setInnerPinned] = useState<PinnedBucket | null>(null);
  const [hoverIdx, setHoverIdx] = useState<number | null>(null);
  const controlled = pinned !== undefined;
  const activePinned = controlled ? (pinned ?? null) : innerPinned;

  const n = buckets.length;
  // refetch 可能让桶数变少：悬停下标越界一律按「未悬停」处理（TrendChart 同款防越界）
  const hover = hoverIdx !== null && hoverIdx < n ? hoverIdx : null;

  const applyPin = useCallback(
    (next: PinnedBucket | null) => {
      if (!controlled) setInnerPinned(next);
      onPin?.(next);
    },
    [controlled, onPin]
  );

  const applyHover = useCallback(
    (idx: number | null) => {
      setHoverIdx(idx);
      onHover?.(idx !== null && idx < n ? (buckets[idx] ?? null) : null);
    },
    [buckets, n, onHover]
  );

  // 切范围/组合/模式解除固定与悬停（Spec §8.3）；首挂载与无关重渲不触发（prev 哨兵）。
  // clearSignalKey（Task 12 裁决）：提供时解除判定只看该键——滚动窗口 from/to 随 asOf
  // 每 5 秒前移不算切换；缺省退回 mode/from/to 原值哨兵（既有调用方行为不变）。
  const navKey = clearSignalKey ?? `${mode}:${from}:${to}`;
  const prevNav = useRef(navKey);
  useEffect(() => {
    const prev = prevNav.current;
    prevNav.current = navKey;
    if (prev === navKey) return;
    setHoverIdx(null);
    if (!controlled) setInnerPinned(null);
    onHover?.(null);
    onPin?.(null);
  }, [navKey, controlled, onHover, onPin]);

  const onKeyDown = (e: ReactKeyboardEvent<HTMLDivElement>): void => {
    if (n === 0) return;
    if (e.key === "ArrowRight" || e.key === "ArrowLeft") {
      e.preventDefault();
      const delta = e.key === "ArrowRight" ? 1 : -1;
      const base = hover ?? (delta > 0 ? -1 : 0);
      applyHover(clamp(base + delta, 0, n - 1));
    } else if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      if (hover === null) return;
      const target = buckets[hover]!;
      if (activePinned && activePinned.startMs === target.startMs) {
        applyPin(null); // 再按一次 = 解除（与点击同一桶清除同语义）
      } else {
        applyPin(pinSnapshot(target, seriesKey, mode));
      }
    } else if (e.key === "Escape") {
      e.preventDefault();
      setHoverIdx(null);
      onHover?.(null);
      applyPin(null);
    }
  };

  // ==== 几何（全部数值坐标；jsdom 无测量，见 chart.ts 文件头）====
  const innerW = Math.max(0, width - SPEED_CHART_PAD.l - SPEED_CHART_PAD.r);
  const innerH = Math.max(0, height - SPEED_CHART_PAD.t - SPEED_CHART_PAD.b);
  const baseY = SPEED_CHART_PAD.t + innerH;
  // x 按桶下标在完整范围内等距铺满（单桶居中）；首末桶永远贴绘图区两缘
  const xOf = (i: number): number =>
    n <= 1 ? SPEED_CHART_PAD.l + innerW / 2 : SPEED_CHART_PAD.l + (innerW * i) / (n - 1);

  const unit = unitForMode(mode, t);
  const metricName = t(`speed.metric.${mode}`);
  const values: number[] = [];
  for (const b of buckets) {
    const v = rateForMode(b.metrics, mode).value;
    if (v !== null) values.push(v);
  }
  const scale = niceScale(values.length > 0 ? Math.max(...values) : 0);
  const yOf = (v: number): number => SPEED_CHART_PAD.t + innerH * (1 - clamp(v / scale.max, 0, 1));

  const segments = segmentSeries(buckets, mode);
  const segPaths = segments.map((seg) => {
    const pts: { x: number; y: number }[] = [];
    for (let i = seg.from; i <= seg.to; i++) {
      const v = rateForMode(buckets[i]!.metrics, mode).value;
      if (v !== null) pts.push({ x: xOf(i), y: yOf(v) });
    }
    return polylinePath(pts);
  });

  // 缺口几何：前段末点 → 后段首点（虚线连接端点）+ 阴影带（段端 x 之间的整高区）
  const gaps: { x0: number; x1: number; y0: number; y1: number }[] = [];
  for (let s = 1; s < segments.length; s++) {
    const prevEnd = segments[s - 1]!.to;
    const nextStart = segments[s]!.from;
    const v0 = rateForMode(buckets[prevEnd]!.metrics, mode).value;
    const v1 = rateForMode(buckets[nextStart]!.metrics, mode).value;
    if (v0 === null || v1 === null) continue; // 防御：段端必有值，不成立就不画
    gaps.push({ x0: xOf(prevEnd), x1: xOf(nextStart), y0: yOf(v0), y1: yOf(v1) });
  }

  const ticks = timeTicks(from, to);
  const spanFx = (ms: number): number =>
    SPEED_CHART_PAD.l + innerW * clamp((ms - from) / Math.max(1, to - from), 0, 1);
  const rangeSpansDates = spansLocalDate(from, to);

  const hoverBucket = hover !== null ? (buckets[hover] ?? null) : null;
  const pinIdx = activePinned ? buckets.findIndex((b) => b.startMs === activePinned.startMs) : -1;

  const tooltipPct = hover !== null ? clamp((xOf(hover) / width) * 100, 12, 88) : 0;

  return (
    <div
      data-testid="speed-chart"
      role="application"
      tabIndex={0}
      aria-label={t("speed.chart.ariaLabel", { metric: metricName, unit })}
      className="relative rounded-lg focus-visible:outline-2 focus-visible:outline-offset-2"
      onKeyDown={onKeyDown}
    >
      <svg
        data-testid="speed-chart-svg"
        viewBox={`0 0 ${width} ${height}`}
        className="text-muted-foreground block h-auto w-full"
        onMouseLeave={() => {
          setHoverIdx(null);
          onHover?.(null);
        }}
      >
        <defs>
          <pattern
            id={hatchId}
            width={8}
            height={8}
            patternUnits="userSpaceOnUse"
            patternTransform="rotate(45)"
          >
            <line
              x1={0}
              y1={0}
              x2={0}
              y2={8}
              stroke="currentColor"
              strokeOpacity={0.18}
              strokeWidth={4}
            />
          </pattern>
        </defs>

        {/* y 网格 + 刻度值（从 0 起的易读刻度） */}
        {scale.ticks.map((tick) => (
          <g key={`y${tick}`}>
            <line
              x1={SPEED_CHART_PAD.l}
              x2={width - SPEED_CHART_PAD.r}
              y1={n2(yOf(tick))}
              y2={n2(yOf(tick))}
              stroke="currentColor"
              strokeOpacity={0.18}
            />
            <text
              data-testid="speed-chart-ylabel"
              x={SPEED_CHART_PAD.l - 6}
              y={n2(yOf(tick) + 3)}
              textAnchor="end"
              fontSize={10}
              fill="currentColor"
            >
              {formatAxisValue(tick)}
            </text>
          </g>
        ))}
        {/* 坐标轴（单点/全空也画） */}
        <line
          x1={SPEED_CHART_PAD.l}
          x2={SPEED_CHART_PAD.l}
          y1={SPEED_CHART_PAD.t}
          y2={baseY}
          stroke="currentColor"
          strokeOpacity={0.35}
        />
        <line
          x1={SPEED_CHART_PAD.l}
          x2={width - SPEED_CHART_PAD.r}
          y1={baseY}
          y2={baseY}
          stroke="currentColor"
          strokeOpacity={0.35}
        />

        {/* 缺口阴影层（Spec §8.5「缺口用阴影」；在实测线之下） */}
        {gaps.length > 0 && (
          <g data-testid="speed-chart-gap-band">
            {gaps.map((g, gi) => (
              <rect
                key={gi}
                x={n2(Math.min(g.x0, g.x1))}
                y={SPEED_CHART_PAD.t}
                width={n2(Math.abs(g.x1 - g.x0))}
                height={n2(innerH)}
                fill={`url(#${hatchId})`}
              />
            ))}
          </g>
        )}

        {/* 实测折线（只在有值相邻桶间连续；每段一条 path） */}
        <g data-testid="speed-chart-line">
          {segPaths.map((d, si) => (
            <path
              key={si}
              data-testid="speed-chart-line-segment"
              data-segment={si}
              d={d}
              fill="none"
              stroke="var(--usage-accent-a)"
              strokeWidth={1.8}
              strokeLinecap="round"
              strokeLinejoin="round"
            />
          ))}
        </g>

        {/* 灰虚线连接：独立图层 + pointer-events none，不参与悬停命中（Spec §8.5） */}
        {gapStyle === "dash" && gaps.length > 0 && (
          <g data-testid="speed-chart-gap-connector" style={{ pointerEvents: "none" }}>
            {gaps.map((g, gi) => (
              <path
                key={gi}
                d={`M ${n2(g.x0)} ${n2(g.y0)} L ${n2(g.x1)} ${n2(g.y1)}`}
                fill="none"
                stroke="currentColor"
                strokeOpacity={0.45}
                strokeWidth={1}
                strokeDasharray="4 4"
              />
            ))}
          </g>
        )}

        {/* 数据点 */}
        {buckets.map((b, i) => {
          const v = rateForMode(b.metrics, mode).value;
          if (v === null) return null;
          return (
            <circle
              key={b.startMs}
              data-testid="speed-chart-dot"
              cx={n2(xOf(i))}
              cy={n2(yOf(v))}
              r={2.5}
              fill="var(--usage-accent-a)"
            />
          );
        })}

        {/* 固定标记（粗竖线）与悬停细竖线（Spec §8.3「悬停桶显示细竖线」） */}
        {pinIdx >= 0 && (
          <line
            data-testid="speed-chart-pin"
            x1={n2(xOf(pinIdx))}
            x2={n2(xOf(pinIdx))}
            y1={SPEED_CHART_PAD.t}
            y2={baseY}
            stroke="var(--usage-accent-a)"
            strokeWidth={2}
            style={{ pointerEvents: "none" }}
          />
        )}
        {hover !== null && (
          <line
            data-testid="speed-chart-guide"
            x1={n2(xOf(hover))}
            x2={n2(xOf(hover))}
            y1={SPEED_CHART_PAD.t}
            y2={baseY}
            stroke="currentColor"
            strokeOpacity={0.55}
            strokeWidth={1}
            style={{ pointerEvents: "none" }}
          />
        )}

        {/* x 轴时间标签：按宽度 4–7 个（timeTicks），与桶位置解耦 */}
        {ticks.map((tick) => (
          <text
            key={tick}
            data-testid="speed-chart-xlabel"
            x={n2(spanFx(tick))}
            y={height - 6}
            textAnchor="middle"
            fontSize={10}
            fill="currentColor"
          >
            {formatClock(tick)}
          </text>
        ))}

        {/* 命中层：每桶一个整高透明矩形（fill=none 也吃指针）；悬停/点击的唯一入口 */}
        {buckets.map((b, i) => {
          const cx = xOf(i);
          const prevCx = i === 0 ? cx : xOf(i - 1);
          const nextCx = i === n - 1 ? cx : xOf(i + 1);
          const left = i === 0 ? SPEED_CHART_PAD.l : (prevCx + cx) / 2;
          const right = i === n - 1 ? width - SPEED_CHART_PAD.r : (cx + nextCx) / 2;
          return (
            <rect
              key={b.startMs}
              data-testid="speed-chart-hit"
              data-index={i}
              x={n2(left)}
              y={SPEED_CHART_PAD.t}
              width={n2(Math.max(0, right - left))}
              height={n2(innerH)}
              fill="none"
              pointerEvents="all"
              onMouseEnter={() => applyHover(i)}
              onMouseLeave={() => applyHover(null)}
              onClick={() => {
                const current = activePinned;
                if (current && current.startMs === b.startMs) {
                  applyPin(null); // 点击已固定桶 = 解除
                } else {
                  applyPin(pinSnapshot(b, seriesKey, mode));
                }
              }}
            />
          );
        })}
      </svg>

      {/* tooltip：真实 DOM（role=tooltip），不是原生 title；空桶只给「无有效样本」无值无插值 */}
      {hoverBucket && (
        <div
          role="tooltip"
          data-testid="speed-chart-tooltip"
          className="bg-popover text-popover-foreground border-border pointer-events-none absolute top-1 z-10 -translate-x-1/2 rounded-md border px-2 py-1 text-xs whitespace-nowrap shadow-md"
          style={{ left: `${tooltipPct}%` }}
        >
          {(() => {
            const withDate =
              spansLocalDate(hoverBucket.startMs, hoverBucket.endMs) || rangeSpansDates;
            const startText = withDate
              ? formatClockWithDate(hoverBucket.startMs)
              : formatClock(hoverBucket.startMs);
            const endText = withDate
              ? formatClockWithDate(hoverBucket.endMs)
              : formatClock(hoverBucket.endMs);
            const rate = rateForMode(hoverBucket.metrics, mode);
            const valueText =
              rate.value !== null
                ? `${formatRateValue(rate.value)} ${unit}`
                : t("speed.chart.noSample");
            return (
              <>
                <div className="font-medium">
                  {startText} – {endText}
                </div>
                <div>
                  {t("speed.chart.tooltipWidth", {
                    width: formatDurationI18n(hoverBucket.endMs - hoverBucket.startMs, t),
                  })}
                </div>
                <div>{valueText}</div>
                <div className="text-muted-foreground">
                  {t("speed.chart.tooltipN", { n: rate.sampleCount })}
                </div>
              </>
            );
          })()}
        </div>
      )}

      {/* 图例（仅有缺口时）：虚线仅连接，无样本（Spec §8.5）；band 样式说明阴影 */}
      {gaps.length > 0 && (
        <div
          data-testid="speed-chart-legend"
          className="text-muted-foreground mt-1 flex items-center gap-1.5 text-[11px]"
        >
          {gapStyle === "dash" ? (
            <svg width="22" height="8" aria-hidden="true" className="shrink-0">
              <line
                x1={0}
                y1={4}
                x2={22}
                y2={4}
                stroke="currentColor"
                strokeOpacity={0.6}
                strokeDasharray="4 3"
              />
            </svg>
          ) : (
            <span
              aria-hidden="true"
              className="border-border/60 inline-block h-2.5 w-5 shrink-0 rounded-sm border"
              style={{
                backgroundImage:
                  "repeating-linear-gradient(45deg, currentColor 0 2px, transparent 2px 5px)",
                opacity: 0.35,
              }}
            />
          )}
          <span>
            {gapStyle === "dash"
              ? t("speed.chart.legend.gapConnector")
              : t("speed.chart.legend.gapBand")}
          </span>
        </div>
      )}

      {/* 口径脚注（二轮实机反馈 ④）：渲染在图例之后、卡底之前（speed-chart-footnote） */}
      {footnote ? (
        <div
          data-testid="speed-chart-footnote"
          className="text-muted-foreground mt-1 space-y-0.5 text-[11px]"
        >
          {footnote}
        </div>
      ) : null}
    </div>
  );
}

export default SpeedChart;
