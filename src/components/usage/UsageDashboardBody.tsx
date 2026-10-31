// 模型用量页的主体（V2 Task 12 → 2026-10-26 计划 Task 7 提取为可嵌入组件）：
// A 总览 / B 趋势 / C 分布 / D API 等效费用 / E 工作活动 + 记录页签。
//
// 从旧 `usage-dashboard.tsx` 独立窗口页原样提取，**只减不加**：
//  * 不含 WindowFrame / TitleBar（四页同窗后窗口 chrome 属于主窗宿主，绝不嵌套）；
//  * TPS 区块随 Task 7 移出（输出速度页在 Task 12 前复用既有 TpsCard 过渡）；
//  * 其余分区顺序 / 筛选 / 折叠 / 价格渠道管理 / 导出 / 记录页与旧页逐字一致（spec §4）。
//
// active 门控（Task 7；评审 Important 2 收口）：挂载在四页宿主后**常驻不卸载**——宿主以
// 同格堆叠 + `visibility:hidden`（非 display:none）隐藏非活动页，盒子树保留故**滚动位置
// 不丢**（真机滚动保留的实机验证归 Task 13），React 状态（范围/筛选/折叠）自然保留。
// `active=false`（页面隐藏）时**全部**用量读查询停发——含主体直连（settings/dashboard/
// channelOptions/distribution/trend）、子组件透传（D 卡 costs/pricing、记录面板
// records/identities/aliases）、`useUsageCollect` 的可见即采——因为
// queryClient 未禁 `refetchOnWindowFocus`，只有 `enabled:false` 能挡住隐藏页重聚焦时的
// 隐性重查。重新可见（active 的 false→true 边沿）时强制采集一次（模块级 60s 去抖，见
// `lib/query/queries/usage.ts`）；兜底采集由后端 ticker 负责，与本页活跃状态无关。
// 默认 `active=true`，旧独立窗口用法行为不变。
//
// 编排原则（不变）：
//  * A/E 用旧 `useUsageDashboardQuery`（固定 tool 口径，仅供总览与工作活动）；
//  * B 用 `useUsageTrendQuery(range, trendSelection)`（局部筛选不联动 A/C）；
//  * C 用 `useUsageDistributionQuery(range, distributionGroup)`（默认按计费渠道）；
//  * 渠道筛选选项用 `useUsageChannelOptionsQuery(range)`（显示渠道，非未指定/可能混合）；
//  * 每个分区独立三态：某个查询失败只替换该卡体并可重试，不互相拖垮。
//
// 折叠状态与筛选状态**都在页面顶层**（不在 board 条件子树里）：切范围/页签不重置。
import { useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { Button } from "@/components/ui/button";
import { WorkspacePageHeader } from "@/components/common/WorkspacePageHeader";
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

export function UsageDashboardBody({ active = true }: { active?: boolean }) {
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
  // 采集触发吃 active 门控：页面激活的 false→true 边沿强制采集一次（模块级 60s 去抖），
  // 页面隐藏期间零采集；兜底采集由后端 ticker 负责，与页面活跃状态无关
  // （Task 7/8/10；旧独立窗口恒 active=true 行为不变）
  useUsageCollect(active);
  const settings = useUsageSettingsQuery({ enabled: active });

  // A/E 只看全局口径；旧 groupBy 不再驱动 C，固定 "tool" 也供固定分享/Token CSV 使用。
  const boardGroupBy: UsageGroupBy = "tool";
  const dashboard = useUsageDashboardQuery(range, boardGroupBy, { enabled: active });
  // B 的渠道选项 = 显示渠道（后端已排除 unassigned/ambiguous 与归档渠道）
  const channelOptions = useUsageChannelOptionsQuery(range, { enabled: active });
  // 渠道模式但选项未就绪（selectedId 为空）时不发空键 trend 请求；加载/错误由筛选器就地展示。
  const trendEnabled =
    active &&
    !(
      trendSelection.scope === "billingProfile" &&
      (trendSelection.selectedId === "" || channelOptions.isPending)
    );
  const trendQuery = useUsageTrendQuery(range, trendSelection, { enabled: trendEnabled });
  const distributionQuery = useUsageDistributionQuery(range, distributionGroup, {
    enabled: active,
  });
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
    summary = <UsageSummary dash={dash} t={t} />;
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
      <UsageCostCard
        range={range}
        open={folds.cost}
        onOpenChange={setFold("cost")}
        t={t}
        active={active}
      />

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
    <div className="flex-1 overflow-y-auto" data-testid="workspace-page-usage-body">
      <div className="space-y-3 p-4">
        {/* 页头区（二轮实机反馈 ②，2026-10-27）：**不再吸顶**——表头随内容滚动
            （sticky 移除后导出动作会随滚动离开视口，用户裁决接受）；保留通栏负边距、
            实心面与下边框（深浅色切换无闪面）。
            工具栏并行：看板/记录页签对在左、时间范围条在右，两行合并为一行
            justify-between；items-start 顶对齐——自定义日期展开时范围条向下生长
            （presets + 日期 + 截断提示三行竖排），页签对保持与 presets 行平齐。 */}
        <div className="bg-background -mx-4 space-y-3 border-b px-4 pt-4 pb-2">
          <WorkspacePageHeader
            title={t("workspace.page.usage")}
            actions={
              <UsageExportActions
                dash={dashboard.data ?? null}
                range={range}
                tab={tab}
                filters={tab === "records" ? rec.filters : {}}
                // 固定分享 / Token CSV 仍按工具口径；记录页用卡片维度。
                groupBy={tab === "records" ? rec.cardDim : boardGroupBy}
                t={t}
              />
            }
          />

          <div className="flex flex-wrap items-start justify-between gap-2">
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
            <UsageRangeBar
              preset={preset}
              custom={custom}
              clamped={clamped}
              t={t}
              onSelectPreset={handleSelectPreset}
              onCustomChange={handleCustomChange}
            />
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
            active={active}
          />
        )}
      </div>
    </div>
  );
}
