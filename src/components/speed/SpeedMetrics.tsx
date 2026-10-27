// V2 速度四格（2026-10-26 计划 Task 10，Spec §5.3）——当前模式速度 / 首 Token 平均等待 /
// 累计用时 / 工具累计用时，加一条上下文行（区间汇总 或 桶起止/宽/n）。
//
// 诚实纪律（逐条对 Spec）：
//  · 「未知不填 0」：任何 null（速度无样本 / TTFT 不可得 / 用时无可靠值）一律显示「—」，
//    绝不伪造秒值或 0；
//  · Codex（method=log-boundary-approx）的用时格标签是「响应累计用时」并带一级「日志近似」
//    短标记；六源显示「累计用时」（可靠模型用时）——按 series 的 method 判定，不按工具名猜；
//  · 工具用时当前来源普遍无可靠归属，保持「—」（不能拿全工具工作活动时长冒充）；
//  · 「没有当前模式速度但其他指标可靠时，其他格仍独立显示」——各格独立判空，不整组清空；
//  · 上下文行区分读数来源：区间汇总（模式名+n）或桶（起止+宽+n，空桶加「无有效样本」）；
//    固定时给「返回区间汇总」按钮（Spec §8.3「返回汇总解除固定」）。
import type { ReactElement } from "react";
import { useTranslation } from "react-i18next";
import {
  EM_DASH,
  formatClock,
  formatClockWithDate,
  formatDurationI18n,
  formatFirstTokenMs,
  formatRateValue,
  spansLocalDate,
  unitForMode,
} from "@/lib/speed/chart";
import { rateForMode } from "@/lib/speed/view";
import type { SpeedMetricMode, SpeedMetrics as SpeedMetricsData } from "@/types/speed";

/** series 身份（SeriesKey 的结构子集 + 可选 method）：method 仅用于计时口径标注
 *  （log-boundary-approx ⇒ Codex 式「响应累计用时」+ 日志近似标记）。 */
export interface SpeedMetricsSeries {
  tool: string;
  model: string;
  channel: string | null;
  method?: string;
}

/** 读数上下文：range-summary = 选中区间汇总；bucket = 某个时间桶（pinned=true 时为固定快照）。 */
export type SpeedMetricsContext =
  | { kind: "range-summary" }
  | {
      kind: "bucket";
      startMs?: number;
      endMs?: number;
      bucketWidthMs?: number;
      pinned?: boolean;
    };

export interface SpeedMetricsProps {
  metrics: SpeedMetricsData;
  mode: SpeedMetricMode;
  series?: SpeedMetricsSeries;
  context: SpeedMetricsContext;
  /** 「返回区间汇总」按钮回调（仅固定态出现按钮） */
  onClearPin?: () => void;
}

/** 单格：标签 + 值（值已含格式化与 — 兜底）；suffix 紧贴值（如单位），badge 进标签行。 */
function Tile(props: {
  testId: string;
  label: string;
  value: string;
  valueTestId: string;
  extraLabel?: ReactElement;
  suffix?: ReactElement;
  /** 悬停解释（第四轮实机反馈：工具格「—」的口径说明；可选） */
  title?: string;
}): ReactElement {
  return (
    <div
      data-testid={props.testId}
      title={props.title}
      className="border-border/60 rounded-lg border p-3"
    >
      <div className="text-muted-foreground text-xs leading-snug">
        {props.label}
        {props.extraLabel}
      </div>
      <div className="mt-1 flex items-baseline gap-1">
        <span data-testid={props.valueTestId} className="text-xl font-semibold">
          {props.value}
        </span>
        {props.suffix}
      </div>
    </div>
  );
}

export function SpeedMetrics({
  metrics,
  mode,
  series,
  context,
  onClearPin,
}: SpeedMetricsProps): ReactElement {
  const { t } = useTranslation();
  const rate = rateForMode(metrics, mode);
  const unit = unitForMode(mode, t);
  const isLogApprox = series?.method === "log-boundary-approx";

  // 上下文行（读数来源）：
  let contextText: string;
  if (context.kind === "range-summary") {
    contextText = t("speed.context.range", {
      metric: t(`speed.metric.${mode}`),
      n: rate.sampleCount,
    });
  } else {
    const { startMs, endMs } = context;
    const hasSpan = typeof startMs === "number" && typeof endMs === "number";
    const withDate = hasSpan && spansLocalDate(startMs!, endMs!);
    const span = hasSpan
      ? `${withDate ? formatClockWithDate(startMs!) : formatClock(startMs!)} – ${
          withDate ? formatClockWithDate(endMs!) : formatClock(endMs!)
        }`
      : EM_DASH;
    const widthMs = context.bucketWidthMs ?? (hasSpan ? endMs! - startMs! : null);
    contextText = t("speed.context.bucket", {
      span,
      width: formatDurationI18n(widthMs, t),
      n: rate.sampleCount,
    });
  }
  const noValue = rate.value === null;

  return (
    <div data-testid="speed-metrics" data-mode={mode}>
      {/* 指标 2×2 ↔ 4 列的断点必须与组合列表的收展一致（均 lg，Spec §2.8「窄窗收列表 +
          指标 2×2」）：若用 md，800px 主窗（≥768 < 1024）会出现列表已收、四格还横排的混合形态 */}
      <div className="grid grid-cols-2 gap-2 lg:grid-cols-4">
        <Tile
          testId="speed-tile-rate"
          label={t("speed.tile.currentRate")}
          value={formatRateValue(rate.value)}
          valueTestId="speed-rate-value"
          suffix={
            <span data-testid="speed-rate-unit" className="text-muted-foreground text-[11px]">
              {unit}
            </span>
          }
        />
        <Tile
          testId="speed-tile-first-token"
          label={t("speed.tile.avgFirstToken")}
          value={formatFirstTokenMs(metrics.avgFirstTokenMs)}
          valueTestId="speed-first-token"
        />
        <Tile
          testId="speed-tile-model"
          label={isLogApprox ? t("speed.tile.responseMs") : t("speed.tile.modelMs")}
          value={formatDurationI18n(metrics.modelMs, t)}
          valueTestId="speed-model-duration"
          extraLabel={
            isLogApprox ? (
              <span
                data-testid="speed-approx-mark"
                className="border-border/60 text-muted-foreground ml-1 rounded border px-1 text-[10px]"
              >
                {t("speed.tile.approxMark")}
              </span>
            ) : undefined
          }
        />
        <Tile
          testId="speed-tile-tool"
          label={t("speed.tile.toolMs")}
          value={formatDurationI18n(metrics.toolMs, t)}
          valueTestId="speed-tool-duration"
          title={t("speed.tile.toolMsHint")}
        />
      </div>
      <div className="text-muted-foreground mt-2 flex flex-wrap items-center gap-2 text-xs">
        <span data-testid="speed-metrics-context">
          {contextText}
          {context.kind === "bucket" && noValue ? ` · ${t("speed.chart.noSample")}` : ""}
        </span>
        {context.kind === "bucket" && context.pinned && onClearPin ? (
          <button
            type="button"
            data-testid="speed-clear-pin"
            onClick={onClearPin}
            className="border-border hover:bg-accent rounded-md border px-2 py-0.5 text-xs"
          >
            {t("speed.context.backToSummary")}
          </button>
        ) : null}
      </div>
    </div>
  );
}

export default SpeedMetrics;
