// 定价目录状态面板（Task 17 前端）。Props `{t}`。
//
// 只用查询 + 事件驱动：手动刷新期间显示进行中，完成/失败/卸载即停止；**不引入任何轮询**。
// 外部请求前明确边界：只下载 models.dev 公开价目表，不上传本地用量。
import { useState, type ReactNode } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { Button } from "@/components/ui/button";
import { Switch } from "@/components/ui/switch";
import { usageErrMsg } from "@/components/usage/usageErrors";
import { usagePricingRefresh, usagePricingSetEnabled } from "@/lib/api/usageV2";
import { EM_DASH, fmtDateTime } from "@/lib/usage/format";
import { useUsagePricingEvents, useUsagePricingQuery } from "@/lib/query/queries/usageV2";
import type { TFn } from "@/lib/usage/range";
import type { PricingStatus } from "@/types/usageV2";

export interface PricingStatusPanelProps {
  t: TFn;
}

export function PricingStatusPanel({ t }: PricingStatusPanelProps) {
  const queryClient = useQueryClient();
  const pricing = useUsagePricingQuery();
  // 刷新完成/失败后由事件失效 pricing（与费用查询），不轮询
  useUsagePricingEvents();
  const [busy, setBusy] = useState(false);
  const [actionError, setActionError] = useState<unknown>(null);

  const applyStatus = (status: PricingStatus) => {
    queryClient.setQueryData(["usage", "v2", "pricing"], status);
  };

  const setEnabled = async (enabled: boolean) => {
    setBusy(true);
    setActionError(null);
    try {
      applyStatus(await usagePricingSetEnabled(enabled));
      await queryClient.invalidateQueries({ queryKey: ["usage", "v2", "costs"] });
    } catch (e) {
      setActionError(e);
    } finally {
      setBusy(false);
    }
  };

  const refresh = async () => {
    setBusy(true);
    setActionError(null);
    try {
      applyStatus(await usagePricingRefresh());
      await queryClient.invalidateQueries({ queryKey: ["usage", "v2", "costs"] });
    } catch (e) {
      setActionError(e);
    } finally {
      setBusy(false);
    }
  };

  // 状态区三态只替换自身；覆盖价管理不依赖定价目录，任何状态下都保持可达。
  let statusBody: ReactNode;
  if (pricing.isPending) {
    statusBody = <p className="text-muted-foreground text-sm">{t("common.loading")}</p>;
  } else if (pricing.isError) {
    statusBody = (
      <div className="space-y-2" role="alert">
        <p className="text-destructive text-sm">{usageErrMsg(pricing.error, t)}</p>
        <Button size="sm" variant="outline" onClick={() => void pricing.refetch()}>
          {t("common.retry")}
        </Button>
      </div>
    );
  } else {
    const status = pricing.data;
    const error = actionError ? usageErrMsg(actionError, t) : (status?.lastError ?? null);
    // 去重：status.refreshing 与本地 busy 只出一条「正在刷新…」
    const refreshing = busy || status?.refreshing === true;

    statusBody = (
      <>
        <div className="flex flex-wrap items-center gap-3">
          <Switch
            aria-label={t("usage.pricing.toggle")}
            data-testid="usage-pricing-toggle"
            checked={status?.enabled ?? false}
            disabled={busy}
            onCheckedChange={(v) => void setEnabled(v)}
          />
          <span className="text-muted-foreground text-xs">
            {status?.enabled ? t("usage.pricing.enabled") : t("usage.pricing.disabled")}
          </span>
          <span className="text-muted-foreground text-xs">
            {t("usage.pricing.source", { source: "models.dev" })}
          </span>
        </div>

        <div className="text-muted-foreground flex flex-wrap gap-x-4 gap-y-1 text-xs">
          {/* 长 snapshot ID 不再展示：只保留快照获取时间与数据源（models.dev） */}
          <span data-testid="usage-pricing-fetched-at">
            {t("usage.pricing.fetchedAt")}:{" "}
            {status?.fetchedAt == null ? EM_DASH : fmtDateTime(status.fetchedAt)}
          </span>
          {status?.stale ? (
            <span data-testid="usage-pricing-stale" className="text-muted-foreground">
              {t("usage.pricing.stale")}
            </span>
          ) : null}
          {refreshing ? (
            <span data-testid="usage-pricing-refreshing">{t("usage.pricing.refreshing")}</span>
          ) : null}
        </div>

        {error ? (
          <div data-testid="usage-pricing-error" className="space-y-1" role="alert">
            <p className="text-destructive text-xs">{t("usage.pricing.lastError", { error })}</p>
            {status?.fetchedAt != null ? (
              <p className="text-muted-foreground text-xs" data-testid="usage-pricing-last-valid">
                {t("usage.pricing.lastValid", { time: fmtDateTime(status.fetchedAt) })}
              </p>
            ) : null}
          </div>
        ) : null}

        {!status?.enabled ? (
          <div className="space-y-2">
            <p className="text-muted-foreground text-xs">{t("usage.pricing.disabledHint")}</p>
            <p className="text-muted-foreground text-xs">{t("usage.pricing.privacy")}</p>
            <Button
              size="sm"
              data-testid="usage-pricing-enable"
              disabled={busy}
              onClick={() => void setEnabled(true)}
            >
              {t("usage.pricing.enable")}
            </Button>
          </div>
        ) : (
          <div className="flex flex-wrap items-center gap-2">
            <Button
              size="sm"
              variant="outline"
              data-testid="usage-pricing-refresh"
              disabled={busy}
              onClick={() => void refresh()}
            >
              {t("usage.pricing.refresh")}
            </Button>
            {status.snapshotId === null ? (
              <span className="text-muted-foreground text-xs">{t("usage.pricing.noSnapshot")}</span>
            ) : null}
          </div>
        )}
      </>
    );
  }

  return <div className="space-y-2 text-sm">{statusBody}</div>;
}
