// TPS 采样曲线（Task 8）——纯 SVG 自绘、零新增依赖（沿用 TrendChart 的三条纪律：
// 取色只认主题变量 / 几何全在纯函数与 viewBox 数值坐标 / 渐变 id 逐实例唯一）。
//
// 与 Token 趋势图的**刻意差异**（Spec §1.2/§2）：
//  * x 轴是**实际采样时间**（sampled_at 在时间域上的线性投影），不是等距桶；
//  * 断线由 `tpsSegments` 决定（runId / gap / 缺计划点），gap 点**不产生任何坐标点**——
//    既不画 0，也不把断口两侧连起来；
//  * 不做 Token 域的字段（无 hitRate / 无 requestTotal），不向 Token 图伪造任何字段。
import { useId } from "react";
import type { ReactElement } from "react";
import { CHART_PAD } from "@/components/usage/TrendChart";
import { formatTimeLabel, formatTpsValue, tpsSegments } from "@/lib/usage/tpsSeries";
import type { TpsPoint } from "@/types/tps";

export interface TpsChartProps {
  /** 同一 series 内的实测点（可多段）；函数内部再排序与断线，调用方无需预处理。 */
  points: TpsPoint[];
  /** x 域（查询范围）；点越界时夹入。 */
  sinceMs: number;
  untilMs: number;
  /** viewBox 宽高（数值坐标，不是渲染尺寸）。 */
  width?: number;
  height?: number;
  /** 每个实测点的原生 tooltip 文案（组件层组装 i18n）。 */
  formatTooltip?: (p: TpsPoint) => string;
}

const n2 = (v: number): number => Number(v.toFixed(2));

/** TPS 采样曲线：点数 < 1 返回 null（空态由卡片渲染，不画假图）。 */
export function TpsChart({
  points,
  sinceMs,
  untilMs,
  width = 320,
  height = 96,
  formatTooltip,
}: TpsChartProps): ReactElement | null {
  const autoId = useId();
  const segments = tpsSegments(points);
  if (segments.length === 0) return null;

  const values = segments.flat();
  const max = values.reduce((m, p) => Math.max(m, p.tps ?? 0), 0);
  const bottom = CHART_PAD.bLabel;
  const innerH = Math.max(0, height - CHART_PAD.t - bottom);
  const innerW = Math.max(0, width - CHART_PAD.l - CHART_PAD.r);
  const span = Math.max(1, untilMs - sinceMs);
  const xOf = (t: number) => CHART_PAD.l + innerW * Math.min(1, Math.max(0, (t - sinceMs) / span));
  const yOf = (v: number) =>
    CHART_PAD.t + innerH * (1 - (max > 0 && Number.isFinite(v) ? v / max : 0));

  // 渐变 id 逐实例唯一（同 document 多图不串）
  const gid = `tps-chart${autoId.replace(/[^a-zA-Z0-9_-]/g, "")}-grad`;
  const baseY = CHART_PAD.t + innerH;

  // x 标签取首/中/末三个实测点时刻（点多也不叠：固定三个锚点）
  const labelTimes = [
    values[0]!.sampledAt,
    values[Math.floor((values.length - 1) / 2)]!.sampledAt,
    values[values.length - 1]!.sampledAt,
  ];

  return (
    <svg
      viewBox={`0 0 ${width} ${height}`}
      className="text-muted-foreground block h-auto w-full"
      data-testid="tps-chart"
    >
      <defs>
        <linearGradient id={gid} x1="0" y1="0" x2="0" y2="1">
          <stop offset="0%" stopColor="var(--usage-accent-a)" stopOpacity={0.35} />
          <stop offset="100%" stopColor="var(--usage-accent-b)" stopOpacity={0.04} />
        </linearGradient>
      </defs>
      {[0, 0.5, 1].map((r) => (
        <line
          key={r}
          x1={CHART_PAD.l}
          x2={width - CHART_PAD.r}
          y1={CHART_PAD.t + innerH * r}
          y2={CHART_PAD.t + innerH * r}
          stroke="currentColor"
          strokeOpacity={0.25}
        />
      ))}
      {/* 断线逐段画：段内相邻实测点直线相连（不插值、不跨断口） */}
      {segments.map((seg, si) => {
        if (seg.length === 0) return null;
        const d = seg
          .map((p, i) => `${i === 0 ? "M" : "L"} ${n2(xOf(p.sampledAt))} ${n2(yOf(p.tps ?? 0))}`)
          .join(" ");
        const first = seg[0]!;
        const last = seg[seg.length - 1]!;
        const area =
          seg.length > 1
            ? `${d} L ${n2(xOf(last.sampledAt))} ${n2(baseY)} L ${n2(xOf(first.sampledAt))} ${n2(baseY)} Z`
            : "";
        return (
          <g key={`seg-${si}`}>
            {area ? <path d={area} fill={`url(#${gid})`} stroke="none" /> : null}
            <path
              d={d}
              fill="none"
              stroke="var(--usage-accent-a)"
              strokeWidth={1.5}
              strokeLinecap="round"
              strokeLinejoin="round"
            />
          </g>
        );
      })}
      {/* 实测点：每个 value 点一个圆点（gap 点零渲染——不画 0） */}
      {values.map((p) => (
        <circle
          key={p.pointId}
          data-testid="tps-point-dot"
          data-point-id={p.pointId}
          data-tps={formatTpsValue(p.tps)}
          cx={n2(xOf(p.sampledAt))}
          cy={n2(yOf(p.tps ?? 0))}
          r={2.5}
          fill="var(--usage-accent-a)"
        >
          {formatTooltip ? <title>{formatTooltip(p)}</title> : null}
        </circle>
      ))}
      {labelTimes.map((t, i) => (
        <text
          key={`label-${i}`}
          x={n2(xOf(t))}
          y={height - 4}
          textAnchor="middle"
          fontSize={9}
          fill="currentColor"
        >
          {formatTimeLabel(t)}
        </text>
      ))}
    </svg>
  );
}
