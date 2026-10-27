// TPS 指标面板（Task 8）——「最新有效完成调用 / 最近一轮」共用的展示块。
//
// 口径纪律（Spec §1.3/§3/§6）：
//  * 四项详情 = 样本模型用时 / 样本工具调用用时 / 样本平均首token / 样本TPS；
//    未知时长为空（—），**绝不**借用 Token 区间的累计值冒充样本值；
//  * 六个分类计数（候选/计时模型/计时工具/失败/取消/缺计时）是**独立事实**，不统称 TPS n；
//  * TTFT 分母独立（ttftSampleCount），与 TPS 的 sampleCount 分开标注；
//  * method 标注进面板；native-event-approx 加「原生近似」徽标；
//    全部都是端侧采样口径，不称服务器基准。
import type { ReactElement } from "react";
import { EM_DASH, fmtDateTime } from "@/lib/usage/format";
import {
  formatDurationMs,
  formatTpsValue,
  isApproxMethod,
  TPS_METHOD_KEYS,
} from "@/lib/usage/tpsSeries";
import type { TFn } from "@/lib/usage/range";
import type { TpsMetrics } from "@/types/tps";

const OUTPUT_SCOPE_KEYS: Record<TpsMetrics["outputScope"], string> = {
  exclusiveOutput: "usage.tps.outputScope.exclusiveOutput",
  includesReasoning: "usage.tps.outputScope.includesReasoning",
  providerReported: "usage.tps.outputScope.providerReported",
  unknown: "usage.tps.outputScope.unknown",
};

function detailRow(testId: string, label: string, value: ReactElement | string): ReactElement {
  return (
    <div className="flex items-baseline justify-between gap-2" data-testid={testId}>
      <span className="text-muted-foreground">{label}</span>
      <span className="font-medium">{value}</span>
    </div>
  );
}

export interface TpsMetricsPanelProps {
  /** 面板标题（最新有效完成调用 / 最近一轮）。 */
  title: string;
  /** 展示的样本指标；null = 无当前值（staleText 出文案，不出 0）。 */
  metrics: TpsMetrics | null;
  /** metrics 为 null 时的占位文案（暂无近期样本 / 暂无有效样本…）。 */
  staleText: string | null;
  /** TPS 值的测试 id（tps-current-tps / tps-turn-tps）。 */
  valueTestId: string;
  /** 无当前值占位的测试 id（tps-stale / tps-turn-stale）。 */
  staleTestId: string;
  /** 详情四项的测试 id 前缀（tps-detail / tps-turn-detail）。 */
  detailPrefix: string;
  t: TFn;
}

export function TpsMetricsPanel({
  title,
  metrics,
  staleText,
  valueTestId,
  staleTestId,
  detailPrefix,
  t,
}: TpsMetricsPanelProps): ReactElement {
  return (
    <div className="space-y-1.5" data-testid={`${detailPrefix}-panel`}>
      <div className="flex flex-wrap items-baseline justify-between gap-2">
        <span className="text-muted-foreground text-xs">{title}</span>
        {metrics !== null ? (
          <span className="flex items-baseline gap-1.5">
            <span className="text-2xl font-semibold" data-testid={valueTestId}>
              {formatTpsValue(metrics.tps)}
            </span>
            <span className="text-muted-foreground text-xs">token/s</span>
          </span>
        ) : (
          <span className="text-sm" data-testid={staleTestId}>
            {staleText ?? EM_DASH}
          </span>
        )}
      </div>

      {metrics !== null ? (
        <>
          <div className="text-muted-foreground flex flex-wrap items-center gap-x-2 gap-y-0.5 text-xs">
            <span>
              {t(TPS_METHOD_KEYS[metrics.method])}
              {isApproxMethod(metrics.method) ? ` · ${t("usage.tps.method.approxBadge")}` : ""}
            </span>
            <span>·</span>
            <span>{t(OUTPUT_SCOPE_KEYS[metrics.outputScope])}</span>
            {metrics.latestEndedAt !== null ? (
              <>
                <span>·</span>
                <span>
                  {t("usage.tps.latestEnded")} {fmtDateTime(metrics.latestEndedAt)}
                </span>
              </>
            ) : null}
          </div>
          <div className="grid grid-cols-2 gap-x-4 gap-y-0.5 text-xs">
            {detailRow(
              `${detailPrefix}-modelMs`,
              t("usage.tps.detail.modelMs"),
              formatDurationMs(metrics.modelMs)
            )}
            {detailRow(
              `${detailPrefix}-toolMs`,
              t("usage.tps.detail.toolMs"),
              formatDurationMs(metrics.toolMs)
            )}
            {detailRow(
              `${detailPrefix}-avgFirstToken`,
              t("usage.tps.detail.avgFirstToken"),
              metrics.avgFirstTokenMs === null
                ? EM_DASH
                : `${formatDurationMs(metrics.avgFirstTokenMs)} (n=${metrics.ttftSampleCount})`
            )}
            {detailRow(
              `${detailPrefix}-tps`,
              t("usage.tps.detail.tps"),
              formatTpsValue(metrics.tps)
            )}
          </div>
          <p className="text-muted-foreground text-[11px]" data-testid={`${detailPrefix}-counters`}>
            {t("usage.tps.counters", {
              candidate: metrics.candidateCount,
              timedModel: metrics.timedModelCount,
              timedTool: metrics.timedToolCount,
              failed: metrics.failedCount,
              cancelled: metrics.cancelledCount,
              missingTiming: metrics.missingTimingCount,
            })}
          </p>
        </>
      ) : null}
    </div>
  );
}
