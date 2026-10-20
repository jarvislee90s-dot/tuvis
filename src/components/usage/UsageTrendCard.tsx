// 趋势卡（V2 Task 12）——**始终返回 Card**：空态/不可用/错误只替换内容区，卡头与筛选器常在。
//
// 三条纪律不变：
//  ① 内层直接用 `TrendChartWithTooltip`（不复制几何、不另写 tooltip）；
//  ② `busy` 时**隐藏峰值与环比**（旧数字不是当前筛选口径的测量值）；没有图时优先加载态；
//  ③ 取色全在 `TrendChart` 内，本卡只用语义类，零硬编码色值。
//
// V2 新增：卡头 `filters` 插槽（局部工具/渠道筛选，不联动 A/C）；`unavailableHint` 展示
// `UsageTrend.available=false` 的原因；错误正文与重试也留在卡内（不影响其它分区）。
import type { ReactNode } from "react";
import { Button } from "@/components/ui/button";
import { Card, CardAction, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { TrendChartWithTooltip, type ChartPoint } from "@/components/usage/TrendChart";

const CHART_W = 880;
const CHART_H = 112;

export interface UsageTrendCardProps {
  /** 趋势点（`trendPoints` 产物）；< 2 点画不出线 → 内容区空态 */
  points: ChartPoint[];
  /** 本窗口没量到 token → 内容区空态（不画贴底零线） */
  unmeasured: boolean;
  /** 右峰值文案；`busy` 时不显示 */
  peakLabel: string;
  /** 环比文案（空串 = 不渲染）；`busy` 时不显示 */
  compareLabel: string;
  title: string;
  loadingLabel: string;
  emptyLabel: string;
  busy: boolean;
  /** 卡头筛选插槽（V2 局部筛选器） */
  filters?: ReactNode;
  /** `UsageTrend.available=false` 的原因；有值时内容区显示它 */
  unavailableHint?: string | null;
  /** 查询失败正文（只替换内容区） */
  errorMessage?: string | null;
  retryLabel?: string;
  onRetry?: () => void;
}

export function UsageTrendCard({
  points,
  unmeasured,
  peakLabel,
  compareLabel,
  title,
  loadingLabel,
  emptyLabel,
  busy,
  filters,
  unavailableHint,
  errorMessage,
  retryLabel,
  onRetry,
}: UsageTrendCardProps) {
  const error = errorMessage ?? null;
  const unavailable = unavailableHint ?? null;
  const hasChart = !error && !unavailable && !unmeasured && points.length >= 2;
  const showStats = !busy && hasChart;

  let content: ReactNode;
  if (error) {
    content = (
      <div className="space-y-2">
        <p data-testid="usage-trend-error" className="text-destructive text-sm">
          {error}
        </p>
        {onRetry ? (
          <Button size="sm" variant="outline" onClick={onRetry}>
            {retryLabel}
          </Button>
        ) : null}
      </div>
    );
  } else if (unavailable) {
    content = (
      <p data-testid="usage-trend-unavailable" className="text-muted-foreground text-sm">
        {unavailable}
      </p>
    );
  } else if (unmeasured || points.length < 2) {
    content = busy ? (
      <p data-testid="usage-loading" className="text-muted-foreground text-sm">
        {loadingLabel}
      </p>
    ) : (
      <p data-testid="usage-empty" className="text-muted-foreground text-sm">
        {emptyLabel}
      </p>
    );
  } else {
    content = (
      <TrendChartWithTooltip
        points={points}
        width={CHART_W}
        height={CHART_H}
        idPrefix="usage-trend-card"
      />
    );
  }

  return (
    <Card data-testid="usage-trend-card" className="gap-3 py-4">
      <CardHeader className="gap-1 px-4">
        <CardTitle>{title}</CardTitle>
        <CardAction>
          <div className="flex flex-col items-end gap-1">
            {filters}
            <div className="flex flex-col items-end gap-0.5">
              {busy ? (
                <span className="text-muted-foreground text-xs">{loadingLabel}</span>
              ) : showStats ? (
                <span
                  className="text-muted-foreground text-xs whitespace-nowrap"
                  data-testid="usage-trend-peak"
                >
                  {peakLabel}
                </span>
              ) : null}
              {showStats && compareLabel ? (
                <span
                  className="text-muted-foreground text-xs whitespace-nowrap"
                  data-testid="usage-trend-compare"
                >
                  {compareLabel}
                </span>
              ) : null}
            </div>
          </div>
        </CardAction>
      </CardHeader>
      <CardContent className="px-4">{content}</CardContent>
    </Card>
  );
}
