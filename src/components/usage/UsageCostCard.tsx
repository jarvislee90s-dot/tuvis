// API 等效费用卡（Task 16；2026-10-20 计划 Task 7 表格重构后改壳；同日改为默认展开 + 卡片）。
// 卡头常驻标题/口径说明/同步价格/管理配置/折叠按钮。
//
// 纪律：
//  * **没有全局费用总额或渠道小计**：组织项只含 rows，本卡不渲染任何合计；
//  * 展开不自动启用定价：无快照时只显示「启用」入口，用户显式点击才 `setEnabled(true)`；
//  * 视图切换 = 「按渠道 / 按模型」排序/分组两态（默认按渠道；按模型走平铺单组）；
//    Top5 默认、「显示更多」只影响展示，不丢行；
//  * 表头币种快捷切换（USD/CNY，`useDisplayCurrency`）；账本/CSV 仍 USD，只换显示层；
//  * 查询错误只替换本卡体并可重试，不影响 A/B/C/E；
//  * stale 快照显示日期与降级标记；行级金额与明细规则全在 `UsageCostTable`；
//  * 「管理配置」打开 `UsageConfigDialog`（设置页与 D 卡共用；弹窗内上半模型条目、
//    下半可折叠高级覆盖，只留四价覆盖；旧版渠道绑定/参考价规则入口在记录页）。
import { useState, type ReactNode } from "react";
import { RefreshCw } from "lucide-react";
import { Button } from "@/components/ui/button";
import { UsageConfigDialog } from "@/components/usage/UsageConfigDialog";
import { UsageCostTable, type UsageCostTableGroup } from "@/components/usage/UsageCostTable";
import { UsageFold } from "@/components/usage/UsageFold";
import { usageErrMsg } from "@/components/usage/usageErrors";
import { usagePricingRefresh, usagePricingSetEnabled } from "@/lib/api/usageV2";
import { costChannelLabel, costLayoutOf, organizeCosts, type CostView } from "@/lib/usage/costRows";
import { EM_DASH, fmtDateTime } from "@/lib/usage/format";
import { useDisplayCurrency } from "@/lib/usage/displayCurrency";
import {
  useUsageCostsQuery,
  useUsagePricingEvents,
  useUsagePricingQuery,
} from "@/lib/query/queries/usageV2";
import type { TFn } from "@/lib/usage/range";
import type { UsageRange } from "@/types/usage";

const COST_TOP_ROWS = 5;

export interface UsageCostCardProps {
  range: UsageRange;
  open: boolean;
  onOpenChange(open: boolean): void;
  t: TFn;
  /**
   * active 门控（评审 Important 2）：卡片在四页宿主里常驻挂载，页面隐藏期间传 false——
   * costs / pricing 两个读查询停发（含 focus refetch）。默认 true，旧调用者行为不变。
   */
  active?: boolean;
}

function i18nLabel(label: string, t: TFn): string {
  // unknown / ambiguous 标签是 i18n 键；profile 名原样显示（i18next 对缺失键也会回原串）
  return label.startsWith("usage.") ? t(label) : label;
}

export function UsageCostCard({ range, open, onOpenChange, t, active = true }: UsageCostCardProps) {
  const { currency, rate, setCurrency } = useDisplayCurrency();
  const costs = useUsageCostsQuery(range, rate, { enabled: active });
  const pricing = useUsagePricingQuery({ enabled: active });
  // 价格刷新完成/失败后失效 pricing 与 costs（不轮询）
  useUsagePricingEvents();

  const [view, setView] = useState<CostView>("channel");
  const [expanded, setExpanded] = useState(false);
  const [configOpen, setConfigOpen] = useState(false);
  const [enabling, setEnabling] = useState(false);
  const [enableError, setEnableError] = useState<unknown>(null);
  const [syncing, setSyncing] = useState(false);
  const [syncError, setSyncError] = useState<unknown>(null);
  const [syncMessage, setSyncMessage] = useState<string | null>(null);

  const status = pricing.data ?? costs.data?.pricing ?? null;
  // 当日汇率：费用查询结果优先（含 cached/default/settings 回退），其次定价状态。
  const fxRate = costs.data?.fxRate ?? pricing.data?.fxRate ?? rate;
  const fxFallback =
    currency === "cny" && (costs.data?.fxFallback ?? pricing.data?.fxFallback ?? true);
  const layout = costLayoutOf(view);
  const groups = organizeCosts(costs.data?.rows ?? [], layout);
  // Top5 按**全卡行** tokenUsage 全局降序（同值 key 升序）取，再按 group 展示。
  // organizeCosts 只保证组内降序、组间按组 Token 和——flatMap 后必须再全局排序，
  // 否则 Top5 会退化成「组内 Top5」（把高 Token 的他组行挤出去）。
  const allItems = (
    layout === "flat"
      ? (groups[0]?.rows ?? []).map((row) => ({
          row,
          channelName: i18nLabel(costChannelLabel(row), t),
          groupKey: "flat",
        }))
      : groups.flatMap((group) => {
          const channelName = i18nLabel(group.label, t);
          return group.rows.map((row) => ({ row, channelName, groupKey: group.key }));
        })
  ).sort((a, b) => b.row.tokenUsage - a.row.tokenUsage || a.row.key.localeCompare(b.row.key));
  const visibleItems = expanded ? allItems : allItems.slice(0, COST_TOP_ROWS);
  const hiddenCount = allItems.length - visibleItems.length;
  const visibleKeys = new Set(visibleItems.map((item) => item.row.key));
  const tableGroups: UsageCostTableGroup[] = groups
    .map((group) => ({
      key: group.key,
      label: i18nLabel(group.label, t),
      items: visibleItems.filter(
        (item) => item.groupKey === group.key && visibleKeys.has(item.row.key)
      ),
    }))
    .filter((group) => group.items.length > 0);

  const handleEnablePricing = async () => {
    setEnabling(true);
    setEnableError(null);
    try {
      await usagePricingSetEnabled(true);
      await Promise.all([pricing.refetch(), costs.refetch()]);
    } catch (e) {
      setEnableError(e);
    } finally {
      setEnabling(false);
    }
  };

  /** 手动强制同步价格（2026-10-20 用户裁决）：联网刷新 models.dev + 人民币来源 + 当日汇率。 */
  const handleSyncPricing = async () => {
    setSyncing(true);
    setSyncError(null);
    setSyncMessage(null);
    try {
      const refreshed = await usagePricingRefresh();
      await Promise.all([pricing.refetch(), costs.refetch()]);
      if (refreshed.lastError) {
        setSyncError({ code: "usage-internal", detail: refreshed.lastError });
        return;
      }
      setSyncMessage(
        refreshed.fetchedAt === null
          ? t("usage.cost.syncDoneNoSnapshot")
          : t("usage.cost.syncDone", { time: fmtDateTime(refreshed.fetchedAt) })
      );
    } catch (e) {
      setSyncError(e);
    } finally {
      setSyncing(false);
    }
  };

  let body: ReactNode;
  if (costs.isPending) {
    body = (
      <p data-testid="usage-cost-loading" className="text-muted-foreground text-sm">
        {t("common.loading")}
      </p>
    );
  } else if (costs.isError) {
    body = (
      <div data-testid="usage-cost-error" role="alert" className="space-y-2">
        <p className="text-destructive text-sm">{usageErrMsg(costs.error, t)}</p>
        <Button size="sm" variant="outline" onClick={() => void costs.refetch()}>
          {t("common.retry")}
        </Button>
      </div>
    );
  } else {
    body = (
      <div data-testid="usage-cost-body" className="space-y-2">
        {status?.stale && status.snapshotId !== null ? (
          <p data-testid="usage-cost-stale" className="text-muted-foreground text-xs">
            {t("usage.cost.stale", {
              time: status.fetchedAt === null ? EM_DASH : fmtDateTime(status.fetchedAt),
            })}
          </p>
        ) : null}

        {status?.refreshing ? (
          <p data-testid="usage-cost-refreshing" className="text-muted-foreground text-xs">
            {t("usage.cost.refreshing")}
          </p>
        ) : null}

        {syncMessage ? (
          <p
            data-testid="usage-cost-sync-status"
            role="status"
            className="text-muted-foreground text-xs"
          >
            {syncMessage}
          </p>
        ) : null}
        {syncError ? (
          <p data-testid="usage-cost-sync-error" role="alert" className="text-destructive text-xs">
            {usageErrMsg(syncError, t)}
          </p>
        ) : null}

        {status !== null && status.snapshotId === null ? (
          <div
            data-testid="usage-cost-no-pricing"
            className="flex flex-wrap items-center gap-2 text-xs"
          >
            <span className="text-muted-foreground">
              {status.enabled ? t("usage.cost.noSnapshotYet") : t("usage.cost.noSnapshot")}
            </span>
            {!status.enabled ? (
              <Button
                size="sm"
                variant="outline"
                data-testid="usage-cost-enable-pricing"
                disabled={enabling}
                onClick={() => void handleEnablePricing()}
              >
                {t("usage.cost.enablePricing")}
              </Button>
            ) : null}
            {enableError ? (
              <span className="text-destructive">{usageErrMsg(enableError, t)}</span>
            ) : null}
          </div>
        ) : null}

        <div className="flex flex-wrap items-center gap-3">
          <div className="flex flex-wrap items-center gap-1.5">
            {(["channel", "model"] as const).map((key) => (
              <Button
                key={key}
                size="sm"
                variant={key === view ? "default" : "outline"}
                aria-pressed={key === view}
                data-testid={`usage-cost-view-${key}`}
                onClick={() => setView(key)}
              >
                {t(`usage.cost.view.${key}`)}
              </Button>
            ))}
          </div>
          {/* 币种快捷切换：只换显示层；设置页汇率输入与 USD 导出保留不动 */}
          <div className="flex flex-wrap items-center gap-1.5">
            <span className="text-muted-foreground text-xs">
              {t("usage.costTable.currencyToggle")}
            </span>
            {(["usd", "cny"] as const).map((key) => (
              <Button
                key={key}
                size="sm"
                variant={currency === key ? "default" : "outline"}
                aria-pressed={currency === key}
                data-testid={`usage-cost-table-currency-${key}`}
                onClick={() => setCurrency(key)}
              >
                {key.toUpperCase()}
              </Button>
            ))}
          </div>
        </div>

        {currency === "cny" ? (
          <p
            data-testid="usage-cost-converted-legend"
            className="text-muted-foreground text-[11px]"
          >
            {t("usage.cost.convertedLegend")}
          </p>
        ) : null}

        {visibleItems.length === 0 ? (
          <p data-testid="usage-cost-empty" className="text-muted-foreground text-sm">
            {t("usage.cost.empty")}
          </p>
        ) : (
          <UsageCostTable t={t} groups={tableGroups} flat={layout === "flat"} />
        )}

        {hiddenCount > 0 || expanded ? (
          <Button
            size="sm"
            variant="ghost"
            data-testid="usage-cost-more"
            aria-expanded={expanded}
            aria-controls="usage-cost-rows"
            onClick={() => setExpanded((v) => !v)}
          >
            {expanded ? t("usage.cost.less") : t("usage.cost.more", { n: hiddenCount })}
          </Button>
        ) : null}

        <p className="text-muted-foreground text-[11px]">{t("usage.cost.tokenBasis")}</p>
      </div>
    );
  }

  return (
    <>
      <UsageFold
        id="cost"
        title={t("usage.cost.title")}
        open={open}
        onOpenChange={onOpenChange}
        summary={
          <>
            <span data-testid="usage-cost-disclaimer">{t("usage.cost.disclaimer")}</span>
            <span data-testid="usage-cost-currency">
              {" · "}
              {currency === "cny"
                ? t("usage.cost.currencyHintCnyDaily", { rate: fxRate })
                : t("usage.cost.currencyHintUsd")}
              {fxFallback ? ` · ${t("usage.cost.currencyFallback")}` : null}
            </span>
          </>
        }
        actions={
          <>
            {status?.enabled ? (
              <Button
                size="sm"
                variant="outline"
                data-testid="usage-cost-sync"
                disabled={syncing || status.refreshing}
                onClick={() => void handleSyncPricing()}
              >
                <RefreshCw
                  className={
                    syncing || status.refreshing ? "h-3.5 w-3.5 animate-spin" : "h-3.5 w-3.5"
                  }
                  aria-hidden="true"
                />
                {t("usage.cost.sync")}
              </Button>
            ) : null}
            <Button
              size="sm"
              variant="outline"
              data-testid="usage-cost-manage"
              aria-haspopup="dialog"
              onClick={() => setConfigOpen(true)}
            >
              {t("usage.cost.manage")}
            </Button>
          </>
        }
      >
        {body}
      </UsageFold>

      {/* 管理配置 → 与设置页共用的配置弹窗（模型条目 + 可折叠高级覆盖）。
          弹窗关闭时不挂载内容，observed entries / 覆盖价查询随之停发。 */}
      <UsageConfigDialog t={t} instanceId="cost" open={configOpen} onOpenChange={setConfigOpen} />
    </>
  );
}
