// 大看板页（V2 Task 12）：A 总览 / B 趋势 / C 分布 / E 工作活动。
//
// 编排原则：
//  * A/E 用旧 `useUsageDashboardQuery`（固定 tool 口径，仅供总览与工作活动）；
//  * B 用 `useUsageTrendQuery(range, trendSelection)`（局部筛选不联动 A/C）；
//  * C 用 `useUsageDistributionQuery(range, distributionGroup)`（默认按计费渠道）；
//  * 渠道筛选选项用 `useUsageChannelOptionsQuery(range)`（显示渠道，非未指定/可能混合）；
//  * 每个分区独立三态：某个查询失败只替换该卡体并可重试，不互相拖垮。
//
// 折叠状态与筛选状态**都在页面顶层**（不在 board 条件子树里）：切范围/页签不重置；
// 重开窗口恢复默认（C 展开、E 收起、D 收起）。
import { useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { TitleBar } from "@/components/common/title-bar";
import { WindowFrame } from "@/components/common/window-frame";
import { Button } from "@/components/ui/button";
import { trendPoints } from "@/components/usage/TrendChart";
import { UsageCostCard } from "@/components/usage/UsageCostCard";
import { UsageDistributionBody } from "@/components/usage/UsageDistributionCard";
import { UsageExportActions } from "@/components/usage/UsageExportActions";
import { UsageFold } from "@/components/usage/UsageFold";
import { UsageRecordsPanel } from "@/components/usage/UsageRecordsPanel";
import { UsageEmpty, UsageError, UsageLoading } from "@/components/usage/UsageStates";
import { UsageRangeBar } from "@/components/usage/UsageRangeBar";
import { UsageSummary } from "@/components/usage/UsageSummary";
import { UsageTrendCard } from "@/components/usage/UsageTrendCard";
import { UsageTrendFilters } from "@/components/usage/UsageTrendFilters";
import { UsageWorkSummaryBody, workHeaderCounts } from "@/components/usage/UsageWorkSummary";
import { usageErrMsg } from "@/components/usage/usageErrors";
import { EM_DASH, fmtInt, fmtTokens } from "@/lib/usage/format";
import { getLang } from "@/lib/usage/lang";
import {
  useUsageCollect,
  useUsageDashboardQuery,
  useUsageSettingsQuery,
} from "@/lib/query/queries/usage";
import {
  useUsageChannelOptionsQuery,
  useUsageDistributionQuery,
  useUsageTrendQuery,
} from "@/lib/query/queries/usageV2";
import {
  clampCustomRange,
  compareSuffix,
  defaultCustomRange,
  DEFAULT_PRESET,
  isDashboardEmpty,
  rangeOf,
  type TFn,
} from "@/lib/usage/range";
import { useRecordsFilters } from "@/lib/usage/useRecordsFilters";
import type { UsageGroupBy, UsageRangePreset } from "@/types/usage";
import type { DistributionGroup, TrendSelection } from "@/types/usageV2";

type UsageTab = "board" | "records";

/**
 * 把 `UsageTrend.reason` 收敛成可展示文案：
 *  * i18n 键（`usage.*`）→ `t(key)`；
 *  * 错误码（`usage-*`）→ `usageErrMsg` 的本地化模板（不直接印码名）；
 *  * 其他人话原样。
 */
function trendUnavailableText(reason: string | null, t: TFn): string {
  if (!reason) return t("usage.trend.unavailable");
  if (reason.startsWith("usage.")) return t(reason);
  if (reason.startsWith("usage-")) {
    return (
      usageErrMsg({ code: reason, detail: "" }, t)
        .replace(/[：:]\s*$/, "")
        .trim() || t("usage.trend.unavailable")
    );
  }
  return reason;
}

interface Folds {
  distribution: boolean;
  cost: boolean;
  work: boolean;
}

/** 无 Card 的分区错误体（供折叠卡内容区使用，避免卡中卡） */
function SectionError({
  message,
  retryLabel,
  onRetry,
}: {
  message: string;
  retryLabel: string;
  onRetry(): void;
}) {
  return (
    <div className="space-y-2" role="alert">
      <p className="text-destructive text-sm">{message}</p>
      <Button size="sm" variant="outline" onClick={onRetry}>
        {retryLabel}
      </Button>
    </div>
  );
}

export default function UsageDashboardPage() {
  const { t } = useTranslation();
  const lang = getLang();
  const [preset, setPreset] = useState<UsageRangePreset>(DEFAULT_PRESET);
  const [custom, setCustom] = useState(() => defaultCustomRange(new Date()));
  const [clamped, setClamped] = useState(false);
  const [tab, setTab] = useState<UsageTab>("board");

  // —— 顶层交互状态（切范围 / 页签不重置）——
  // API 等效费用（D 卡）默认展开（2026-10-20 用户裁决）。
  const [folds, setFolds] = useState<Folds>({
    distribution: true,
    cost: true,
    work: false,
  });
  const [distributionGroup, setDistributionGroup] = useState<DistributionGroup>("billingProfile");
  const [distributionExpanded, setDistributionExpanded] = useState(false);
  const [trendSelection, setTrendSelection] = useState<TrendSelection>({ scope: "all" });

  const range = rangeOf(preset, custom);
  useUsageCollect();
  const settings = useUsageSettingsQuery();

  // A/E 只看全局口径；旧 groupBy 不再驱动 C，固定 "tool" 也供固定分享/Token CSV 使用。
  const boardGroupBy: UsageGroupBy = "tool";
  const dashboard = useUsageDashboardQuery(range, boardGroupBy);
  // B 的渠道选项 = 显示渠道（后端已排除 unassigned/ambiguous 与归档渠道）
  const channelOptions = useUsageChannelOptionsQuery(range);
  // 渠道模式但选项未就绪（selectedId 为空）时不发空键 trend 请求；加载/错误由筛选器就地展示。
  const trendEnabled = !(
    trendSelection.scope === "billingProfile" &&
    (trendSelection.selectedId === "" || channelOptions.isPending)
  );
  const trendQuery = useUsageTrendQuery(range, trendSelection, { enabled: trendEnabled });
  const distributionQuery = useUsageDistributionQuery(range, distributionGroup);
  const rec = useRecordsFilters(range);

  const setFold = (key: keyof Folds) => (open: boolean) => setFolds((v) => ({ ...v, [key]: open }));

  const handleSelectPreset = (next: UsageRangePreset) => {
    setPreset(next);
    if (next !== "custom") setClamped(false);
  };

  const handleCustomChange = (next: { from: string; to: string }) => {
    const fixed = clampCustomRange(next.from, next.to);
    setCustom({ from: fixed.from, to: fixed.to });
    setClamped(fixed.clamped);
  };

  const dash = dashboard.data;
  const trend = trendQuery.data;
  const dist = distributionQuery.data;

  // —— A 总览（固定展开，不可折叠）——
  let summary: ReactNode;
  if (dashboard.isPending) {
    summary = <UsageLoading label={t("common.loading")} />;
  } else if (dashboard.isError) {
    summary = (
      <UsageError
        title={t("usage.error.title")}
        message={usageErrMsg(dashboard.error, t)}
        retryLabel={t("common.retry")}
        onRetry={() => void dashboard.refetch()}
      />
    );
  } else if (!dash || isDashboardEmpty(dash)) {
    summary = <UsageEmpty label={t("usage.empty")} />;
  } else {
    summary = (
      <UsageSummary dash={dash} retentionDays={settings.data?.detailRetentionDays ?? null} t={t} />
    );
  }

  // —— B 趋势（固定展开）——
  const points = trendPoints(trend?.trend);
  const trendUnmeasured =
    !trend ||
    trend.trend.every((p) => {
      const b = p.buckets;
      return !(b.inputFresh + b.cacheRead + b.cacheWrite + b.output > 0);
    });
  const prevHero =
    trend?.compare == null
      ? null
      : trend.compare.prevMetrics.requestTotal + trend.compare.prevBuckets.output;
  const trendCompareSuffix =
    trend && !trendQuery.isFetching ? compareSuffix(trend.hero, prevHero, t) : "";
  const compareLabel =
    trendCompareSuffix === "" ? "" : `${t("usage.card.compareSubject")} ${trendCompareSuffix}`;
  const peakLabel = t("usage.card.peak", {
    v: fmtTokens(Math.max(0, ...points.map((p) => p.value)), lang),
  });
  const trendUnavailable = trend && !trend.available ? trendUnavailableText(trend.reason, t) : null;

  // —— E 卡头摘要（会话数 / 工具调用次数；不可得 => —）——
  const workCounts = workHeaderCounts(dash?.workSummary, dash?.availability);
  const workFoldSummary = t("usage.work.foldSummary", {
    sessions: workCounts.sessions === null ? EM_DASH : fmtInt(workCounts.sessions),
    toolCalls: workCounts.toolCalls === null ? EM_DASH : fmtInt(workCounts.toolCalls),
  });

  // —— 分区正文 ——
  let workContent: ReactNode;
  if (dashboard.isPending) {
    workContent = <p className="text-muted-foreground text-sm">{t("common.loading")}</p>;
  } else if (dashboard.isError) {
    workContent = (
      <SectionError
        message={usageErrMsg(dashboard.error, t)}
        retryLabel={t("common.retry")}
        onRetry={() => void dashboard.refetch()}
      />
    );
  } else {
    workContent = (
      <UsageWorkSummaryBody work={dash?.workSummary} availability={dash?.availability} t={t} />
    );
  }

  let distributionContent: ReactNode;
  if (distributionQuery.isPending) {
    distributionContent = <p className="text-muted-foreground text-sm">{t("common.loading")}</p>;
  } else if (distributionQuery.isError) {
    distributionContent = (
      <SectionError
        message={usageErrMsg(distributionQuery.error, t)}
        retryLabel={t("common.retry")}
        onRetry={() => void distributionQuery.refetch()}
      />
    );
  } else {
    distributionContent = (
      <UsageDistributionBody
        rows={dist?.rows ?? []}
        groupBy={distributionGroup}
        onGroupByChange={setDistributionGroup}
        expanded={distributionExpanded}
        onExpandedChange={setDistributionExpanded}
        t={t}
      />
    );
  }

  const board = (
    <>
      {summary}

      <UsageTrendCard
        points={points}
        unmeasured={trendUnmeasured}
        peakLabel={peakLabel}
        compareLabel={compareLabel}
        title={t("usage.card.trend")}
        loadingLabel={t("common.loading")}
        emptyLabel={t("usage.empty")}
        busy={
          trendQuery.isFetching ||
          (trendSelection.scope === "billingProfile" && channelOptions.isPending)
        }
        filters={
          <UsageTrendFilters
            selection={trendSelection}
            onChange={setTrendSelection}
            channels={channelOptions.data ?? []}
            channelsPending={channelOptions.isPending}
            channelsError={channelOptions.isError ? usageErrMsg(channelOptions.error, t) : null}
            onRetryChannels={() => void channelOptions.refetch()}
            unavailable={trend?.available === false}
            t={t}
          />
        }
        unavailableHint={trendUnavailable}
        errorMessage={trendQuery.isError ? usageErrMsg(trendQuery.error, t) : null}
        retryLabel={t("common.retry")}
        onRetry={() => void trendQuery.refetch()}
      />

      {/* C 用量分布：可折叠，默认展开；Top5 展开状态独立 */}
      <UsageFold
        id="distribution"
        title={t("usage.card.distribution")}
        open={folds.distribution}
        onOpenChange={setFold("distribution")}
      >
        {distributionContent}
      </UsageFold>

      {/* D API 等效费用：位于 C 后、E 前；折叠状态与 C/E 一起放在页面顶层 */}
      <UsageCostCard range={range} open={folds.cost} onOpenChange={setFold("cost")} t={t} />

      {/* E 工作活动：默认折叠；卡头摘要常在，高级统计在展开后的第二个 details */}
      <UsageFold
        id="work"
        title={t("usage.work.title")}
        open={folds.work}
        onOpenChange={setFold("work")}
        summary={workFoldSummary}
      >
        {workContent}
      </UsageFold>
    </>
  );

  return (
    <WindowFrame
      titleBar={<TitleBar title={t("usage.title")} showMinimize={false} />}
      contentClassName="flex-1 overflow-y-auto"
    >
      <div className="space-y-3 p-4">
        <div className="bg-background/95 sticky top-0 z-10 -mx-4 space-y-3 border-b px-4 pt-4 pb-2 backdrop-blur">
          <div className="flex flex-wrap items-start justify-between gap-2">
            <UsageRangeBar
              preset={preset}
              custom={custom}
              clamped={clamped}
              t={t}
              onSelectPreset={handleSelectPreset}
              onCustomChange={handleCustomChange}
            />
            <UsageExportActions
              dash={dashboard.data ?? null}
              range={range}
              tab={tab}
              filters={tab === "records" ? rec.filters : {}}
              // 固定分享 / Token CSV 仍按工具口径；记录页用卡片维度。
              groupBy={tab === "records" ? rec.cardDim : boardGroupBy}
              t={t}
            />
          </div>

          <div className="flex items-center gap-1.5">
            {(["board", "records"] as const).map((key) => (
              <Button
                key={key}
                size="sm"
                variant={key === tab ? "default" : "outline"}
                aria-pressed={key === tab}
                data-testid={`usage-tab-${key}`}
                onClick={() => setTab(key)}
              >
                {t(`usage.tab.${key}`)}
              </Button>
            ))}
          </div>
        </div>

        {tab === "board" ? (
          board
        ) : (
          <UsageRecordsPanel
            range={range}
            retentionDays={settings.data?.detailRetentionDays ?? null}
            records={rec}
            t={t}
          />
        )}
      </div>
    </WindowFrame>
  );
}
