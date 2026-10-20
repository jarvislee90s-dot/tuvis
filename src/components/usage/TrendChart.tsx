// 趋势图组件（计划② Task 4）——纯 SVG 自绘、零新增依赖（§3 第 29 条）。
// 消费方：Task 7 的 `UsageTrendCard`（`TrendChartWithTooltip` + `trendPoints`，hover 出精确值）、
// Task 12 的分享图趋势段（与 `CHART_PAD` 同一组版式常量，避免两处版式漂移）。
//
// 三条纪律：
//  ① **取色只认两个主题变量** `--usage-accent-a` / `--usage-accent-b`（浅深各一套，声明在
//     `src/index.css`），网格线走 `currentColor` + opacity ⇒ 组件内**零硬编码色值**
//     （§3 第 19/20 条）；分享图导出刻意不消费这些变量（§3 第 21 条），故本文件也不为它让路。
//  ② **几何全在纯函数里**：jsdom 无 canvas 内核、`getBoundingClientRect()` 恒 0 ⇒ 只画
//     `viewBox` 数值坐标、不做布局测量（§3 第 33 条）；「画什么」由 `trendPoints` / `pathOf`
//     这些零 DOM 依赖的纯函数决定，测试直测它们即可。
//  ③ 同一 document 可挂多张图 ⇒ 渐变 id **逐实例唯一**（`idPrefix`，缺省 `useId()`）：
//     `url(#id)` 全部解析到**首个**同 id 实例，撞了就把两张图画成同一个渐变。
import { useId, useState } from "react";
import type { ReactElement } from "react";
import { fmtTokens, fmtPct } from "@/lib/usage/format";
import { getLang, type Lang } from "@/lib/usage/lang";
import type { TrendPoint } from "@/types/usage";

/** 图表点：`trendPoints` 的产物，也是 `TrendChart` 与 tooltip 的唯一入参形状。 */
export interface ChartPoint {
  /** 桶键（`YYYY-MM-DD` 或 `YYYY-MM-DDTHH`），同图内唯一 */
  key: string;
  /** x 轴标签（日桶 `M/D`、小时桶 `HH:00`，由后端给，前端不重算） */
  label: string;
  /** 值口径与 hero 一致（Rust `semantics::hero_of` = 请求输入 + 产出） */
  value: number;
  /** 命中率 0–1；分母为 0（该桶没有请求输入）→ `null`，绝不回填 0（P8：不得显示 0%） */
  hitRate: number | null;
}

/** 非有限 / 负数 / 缺键一律归 0：畸形数据不得把 NaN 或负值写进 path 与圆点坐标 */
const countOf = (v: unknown): number =>
  typeof v === "number" && Number.isFinite(v) && v > 0 ? v : 0;

/** 命中率取后端值并夹到 0–1（分母 0 由调用方转 `null`，本函数不承担「算不出」语义） */
const rateOf = (v: unknown): number => {
  const n = typeof v === "number" && Number.isFinite(v) ? v : 0;
  return Math.min(1, Math.max(0, n));
};

/**
 * 后端趋势桶 → 图表点。逐条口径：
 *  · `value` = **请求输入 + 产出**（与 hero 同一条 `hero_of`）。请求输入取后端给的
 *    `metrics.requestTotal`，**不**用四桶相加反推——Subset 语义下 `cacheWrite` 与 `cacheRead`
 *    可能重叠（Rust `normalize` 里 `request_total` = input 原文），反推会重复计数。
 *  · `hitRate` 分母为 0（后端 `cache_hit_rate` 记 0.0，但那是「算不出」）→ `null`，不得印成 0.0%。
 *  · 空数组 / 非数组（`null` / 缺键 / 畸形条目）→ `[]`，绝不伪造 0 点。
 */
export function trendPoints(trend: TrendPoint[] | null | undefined): ChartPoint[] {
  if (!Array.isArray(trend)) return [];
  const out: ChartPoint[] = [];
  for (const p of trend) {
    if (!p || typeof p !== "object" || !p.buckets || typeof p.buckets !== "object") continue;
    const requestTotal = countOf(p.metrics?.requestTotal);
    out.push({
      key: typeof p.key === "string" ? p.key : "",
      label: typeof p.label === "string" ? p.label : "",
      value: requestTotal + countOf(p.buckets.output),
      hitRate: requestTotal > 0 ? rateOf(p.metrics?.cacheHitRate) : null,
    });
  }
  return out;
}

/**
 * 趋势图版式常量（`viewBox` 数值坐标）：左/右留白、顶部留白，底部两种——
 * 出 x 轴标签用 `bLabel`（给文字留位置），不出标签用 `bPlain`（收窄留白、把曲线画高）。
 * **分享图导出的趋势小图引用同一组**（Task 12 的 `exportLayout.ts:20` import 它并复算投影），
 * 改这里两处一起变——本次 `l/r` 由 6 改 22 正是**一处修两个面**，见下。
 *
 * ⚠️ **`l/r = 6` 会裁掉首末标签**（2026-10-06 实机取证）：x 轴标签以 `textAnchor="middle"`
 * 画在 `x = CHART_PAD.l` 与 `x = width - CHART_PAD.r` 上（本文件 `:236` 的 `<text>`，以及分享图
 * `exportLayout.ts:368` 的同一个画法），标签半宽约 13–14 个视口单位 ⇒ 在 `l=r=6` 时首末标签各有
 * 约 28% 落在视口外。真机表现：屏上最左只剩半个字形 + `:00`，最右 `22:00` 印成 `22:0`；
 * 分享图上同一处标签**溢出白卡内边距**（那里没有被 SVG 视口裁，但压出卡片边界，一样错）。
 * `bLabel` 已是 16、容得下标签的**高度**；`l/r` 管的是**横向**，故它才是病根。
 * 取 22 = 半宽 14 + 余量 8：绘图区横向只窄 32/880 ≈ 3.6%（趋势形状不变），首末标签完整可读。
 * **不得**改用 `textAnchor` 首末特判来打补丁——那会让首末标签与其余标签的锚点语义不一致，
 * 且分享图那条链路也要跟着复制一遍特判（两处版式漂移的老路）。
 */
export const CHART_PAD = { l: 22, r: 22, t: 8, bLabel: 16, bPlain: 6 } as const;

/** `viewBox` 坐标点（数值坐标，不是任何像素测量值） */
type Pt = { x: number; y: number };

/** 第 `i` 点的 x：按点序号等距铺满绘图区。组件与 tooltip 共用，单一出处，避免两处投影漂移。 */
function xOf(i: number, n: number, width: number): number {
  const innerW = Math.max(0, width - CHART_PAD.l - CHART_PAD.r);
  return n <= 1 ? CHART_PAD.l + innerW / 2 : CHART_PAD.l + (innerW * i) / (n - 1);
}

/**
 * viewBox 投影：x 按序号等距，y 按**峰值归一**（最大值贴顶、0 贴底）。
 * 全 0 数据（`max === 0`）时所有点落在底线上——不除零、不出 NaN。
 */
function project(
  points: ChartPoint[],
  width: number,
  height: number,
  showLabels: boolean
): { pts: Pt[]; baseY: number } {
  const n = points.length;
  const bottom = showLabels ? CHART_PAD.bLabel : CHART_PAD.bPlain;
  const innerH = Math.max(0, height - CHART_PAD.t - bottom);
  const max = points.reduce((m, p) => Math.max(m, p.value), 0);
  const pts = points.map((p, i) => ({
    x: xOf(i, n, width),
    y: CHART_PAD.t + innerH * (1 - (max > 0 ? p.value / max : 0)),
  }));
  return { pts, baseY: CHART_PAD.t + innerH };
}

/** 数字进 `d` 属性前收敛到 2 位小数：浮点尾巴会让 path 又长又抖，也让断言不可读 */
const n2 = (v: number): number => Number(v.toFixed(2));

/**
 * 折线 path（纯几何，不碰 DOM）：
 *  · 0 点 → 空串；1 点 → 只有 `M`；2 点 → `M` + 直线 `L`（两点之间没有「平滑」可谈）；
 *  · ≥3 点 → 每段一条三次贝塞尔 `C`，段数恒 = 点数 − 1。
 * 控制点只取两端 x 的中点、y 各自贴端：横向不过冲、纵向不越过相邻点（Catmull-Rom 式的
 * 等距 x 拟合会把峰值削平，趋势图的峰值恰恰是重点）。
 */
export function pathOf(pts: readonly Pt[]): string {
  if (!Array.isArray(pts) || pts.length === 0) return "";
  let d = `M ${n2(pts[0].x)} ${n2(pts[0].y)}`;
  if (pts.length === 1) return d;
  if (pts.length === 2) return `${d} L ${n2(pts[1].x)} ${n2(pts[1].y)}`;
  for (let i = 0; i < pts.length - 1; i++) {
    const a = pts[i];
    const b = pts[i + 1];
    const mx = n2((a.x + b.x) / 2);
    d += ` C ${mx} ${n2(a.y)}, ${mx} ${n2(b.y)}, ${n2(b.x)} ${n2(b.y)}`;
  }
  return d;
}

/**
 * tooltip 文案 = 标签 + **万/亿缩写**（`fmtTokens`，2026-10-19 用户裁决：
 * Token 数值不再显示完整千分位原始值）+ 命中率（1 位小数）。
 * 命中率 `null`（该桶算不出）时**整段省略**，绝不印 `0.0%`。
 * 只含结构化摘要（标签 / 数值 / 比率），不含任何会话正文（隐私白名单，spec P8）。
 */
export function trendTooltipText(p: ChartPoint, locale: Lang = "zh"): string {
  const head = `${p.label} · ${fmtTokens(p.value, locale)}`;
  return typeof p.hitRate === "number" && Number.isFinite(p.hitRate)
    ? `${head} · ${fmtPct(p.hitRate)}`
    : head;
}

/** 出标签的点数上限：点多于它按步长抽稀（首末必显），否则 x 轴文字会叠成一团 */
const MAX_LABELS = 14;

/** 抽稀后的标签下标：`≤ 14` 全出；否则步长 `ceil(n / 14)` 且**末点必显**（跨度读得出来） */
function labelIndexes(n: number): number[] {
  if (n <= MAX_LABELS) return Array.from({ length: n }, (_, i) => i);
  const step = Math.ceil(n / MAX_LABELS);
  const out: number[] = [];
  for (let i = 0; i < n - 1; i += step) out.push(i);
  out.push(n - 1);
  return out;
}

export interface TrendChartProps {
  points: ChartPoint[];
  /** viewBox 宽高（数值坐标，不是渲染尺寸——jsdom 量不到像素，见文件头 §3 第 33 条） */
  width?: number;
  height?: number;
  /** x 轴标签；缺省显示（卡片要读跨度），点多时自动抽稀 */
  showLabels?: boolean;
  /** 渐变 id 前缀；缺省用 `useId()` 保证同 document 内逐实例唯一 */
  idPrefix?: string;
  /** hover 命中层回调：进入某点给 `(point, index)`，移出给 `(null, index)` */
  onHoverPoint?: (point: ChartPoint | null, index: number) => void;
}

/**
 * 纯展示层：只画「有数据」的图——点数 < 2 返回 `null`（空态由卡片渲染，不画假图）。
 * 自带 hover 命中层（每点一个整高透明矩形），tooltip 由 `TrendChartWithTooltip` 装配。
 */
export function TrendChart({
  points,
  width = 320,
  height = 96,
  showLabels = true,
  idPrefix,
  onHoverPoint,
}: TrendChartProps): ReactElement | null {
  const autoId = useId();
  if (points.length < 2) return null;

  const n = points.length;
  const { pts, baseY } = project(points, width, height, showLabels);
  const line = pathOf(pts);
  const area = `${line} L ${n2(pts[n - 1].x)} ${n2(baseY)} L ${n2(pts[0].x)} ${n2(baseY)} Z`;
  // 渐变 id：显式前缀优先；缺省把 useId 的 `«r0»` 洗成字母数字，避免 id 里混入 CSS 选择器元字符
  const gid = `${idPrefix ?? `usage-trend${autoId.replace(/[^a-zA-Z0-9_-]/g, "")}`}-grad`;
  const slot = Math.max(0, (width - CHART_PAD.l - CHART_PAD.r) / (n - 1));
  const plotH = Math.max(0, baseY - CHART_PAD.t);

  return (
    <svg
      viewBox={`0 0 ${width} ${height}`}
      className="text-muted-foreground block h-auto w-full"
      data-testid="usage-trend-svg"
    >
      <defs>
        {/* 面积渐变：上端 accent-a、下端 accent-b（深浅两档各一套取值，见 index.css） */}
        <linearGradient id={gid} x1="0" y1="0" x2="0" y2="1">
          <stop offset="0%" stopColor="var(--usage-accent-a)" stopOpacity={0.35} />
          <stop offset="100%" stopColor="var(--usage-accent-b)" stopOpacity={0.04} />
        </linearGradient>
      </defs>
      {/* 三条参考线（顶 / 中 / 底）：走 currentColor + 低透明度，深浅两档都压得住 */}
      {[0, 0.5, 1].map((r) => (
        <line
          key={r}
          data-testid="usage-trend-grid"
          x1={CHART_PAD.l}
          x2={width - CHART_PAD.r}
          y1={CHART_PAD.t + plotH * r}
          y2={CHART_PAD.t + plotH * r}
          stroke="currentColor"
          strokeOpacity={0.25}
        />
      ))}
      <path data-testid="usage-trend-area" d={area} fill={`url(#${gid})`} stroke="none" />
      <path
        data-testid="usage-trend-line"
        d={line}
        fill="none"
        stroke="var(--usage-accent-a)"
        strokeWidth={1.5}
        strokeLinecap="round"
        strokeLinejoin="round"
      />
      {pts.map((pt, i) => (
        <circle
          key={points[i].key || i}
          data-testid="usage-trend-dot"
          cx={pt.x}
          cy={pt.y}
          r={2.5}
          fill="var(--usage-accent-a)"
        />
      ))}
      {showLabels &&
        labelIndexes(n).map((i) => (
          <text
            key={points[i].key || i}
            data-testid="usage-trend-label"
            data-index={i}
            x={pts[i].x}
            y={height - 4}
            textAnchor="middle"
            fontSize={9}
            fill="currentColor"
          >
            {points[i].label}
          </text>
        ))}
      {/* 命中层：整高透明矩形（`pointerEvents="all"` 让 fill=none 也吃指针）；不靠测量、只靠槽位几何。
          React key 用桶键（畸形条目缺键时回落下标），让换档重挂时节点可复用。 */}
      {pts.map((pt, i) => {
        const left = i === 0 ? CHART_PAD.l : pt.x - slot / 2;
        const right = i === n - 1 ? width - CHART_PAD.r : pt.x + slot / 2;
        return (
          <rect
            key={points[i].key || i}
            data-testid="usage-trend-hit"
            data-index={i}
            x={left}
            y={CHART_PAD.t}
            width={Math.max(0, right - left)}
            height={plotH}
            fill="none"
            pointerEvents="all"
            onMouseEnter={() => onHoverPoint?.(points[i], i)}
            onMouseLeave={() => onHoverPoint?.(null, i)}
          />
        );
      })}
    </svg>
  );
}

export interface TrendChartWithTooltipProps extends Omit<TrendChartProps, "onHoverPoint"> {
  /** 外层容器附加类（卡片用它控边距） */
  className?: string;
}

/**
 * 装配层：`TrendChart` + hover tooltip（规格「数据点 hover 显示精确值」，P8）。
 * tooltip 是**结构化摘要**（标签 + 精确值 + 命中率），不是会话正文（隐私白名单）。
 * 定位只用 `xOf` 的数值比例，不读 `getBoundingClientRect()`（jsdom 恒 0，§3 第 33 条）；
 * 两端夹在 12%–88%，避免贴边时气泡溢出卡面。
 */
export function TrendChartWithTooltip({
  points,
  width = 320,
  height = 96,
  showLabels = true,
  idPrefix,
  className,
}: TrendChartWithTooltipProps): ReactElement | null {
  const [hover, setHover] = useState<number | null>(null);
  if (points.length < 2) return null;

  // 数据是定时刷新的：换档后点数可能变少，而指针没动不会再触发 mouseenter ⇒ hover 下标可能越界。
  // 越界一律按「没悬停」处理（tooltip 收起），不得让 `points[hover]` 取到 undefined。
  const active = hover !== null && hover < points.length ? hover : null;
  const pct = Math.min(88, Math.max(12, (xOf(active ?? 0, points.length, width) / width) * 100));

  return (
    <div className={className ? `relative ${className}` : "relative"}>
      <TrendChart
        points={points}
        width={width}
        height={height}
        showLabels={showLabels}
        idPrefix={idPrefix}
        onHoverPoint={(point, index) => setHover(point ? index : null)}
      />
      {active !== null && (
        <div
          role="tooltip"
          data-testid="usage-trend-tooltip"
          className="bg-popover text-popover-foreground border-border pointer-events-none absolute top-0 z-10 -translate-x-1/2 rounded-md border px-2 py-1 text-xs whitespace-nowrap shadow-md"
          style={{ left: `${pct}%` }}
        >
          {trendTooltipText(points[active], getLang())}
        </div>
      )}
    </div>
  );
}
