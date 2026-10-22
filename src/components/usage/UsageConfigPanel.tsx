// 用量配置正文：模型条目 + 高级覆盖（挂在 `UsageConfigDialog` 内）。
//
//  * 不再有 BillingProfilesManager / 参考价规则；旧版入口只在「记录」页保留；
//  * 高级区只保留「四价覆盖」，默认收起，展开才挂载 PriceOverrideManager；
//  * 模型条目的「覆盖此模型价格」会展开高级区并把模型名预填到覆盖表单；
//  * 观察条目 / 覆盖价查询只在本面板挂载时发起（弹窗关闭即卸载 → 查询停发）。
import { useEffect, useRef, useState } from "react";
import { Button } from "@/components/ui/button";
import {
  PriceOverrideManager,
  type PriceOverridePrefill,
} from "@/components/usage/PriceOverrideManager";
import { UsageConfigModelSection } from "@/components/usage/UsageConfigModelSection";
import { UsageDisclosure } from "@/components/usage/UsageDisclosure";
import { UsageInfoTip } from "@/components/usage/UsageInfoTip";
import { usageErrMsg } from "@/components/usage/usageErrors";
import {
  useUsageObservedEntriesQuery,
  useUsagePriceOverridesQuery,
} from "@/lib/query/queries/usageV2";
import type { TFn } from "@/lib/usage/range";

export interface UsageConfigPanelProps {
  t: TFn;
  instanceId: string;
}

// 注意：正文内部 testid（usage-config-model-* / price-override-*）不带 instanceId 前缀，
// 同一棵树里只允许展开一个配置面板；生产路由/窗口互斥，测试也遵守该前提。

export function UsageConfigPanel({ t, instanceId }: UsageConfigPanelProps) {
  const entriesQuery = useUsageObservedEntriesQuery();
  const overridesQuery = useUsagePriceOverridesQuery();
  const [advancedOpen, setAdvancedOpen] = useState(false);
  const [prefill, setPrefill] = useState<PriceOverridePrefill | null>(null);
  // 滚动触发与 prefill 生命周期解耦：子组件消费 prefill 后父级会 setPrefill(null)，
  // 若 effect 依赖 prefill 会在同一批重渲染中被 cleanup 取消 rAF（主路径滚动失效）。
  const [scrollNonce, setScrollNonce] = useState(0);
  const nonceRef = useRef(0);
  const overrideRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!advancedOpen || scrollNonce === 0) return;
    // 等高级区与覆盖管理器挂载后再滚动；jsdom 没有 scrollIntoView 时安全跳过。
    const frame = requestAnimationFrame(() => {
      overrideRef.current?.scrollIntoView?.({ block: "start" });
    });
    return () => cancelAnimationFrame(frame);
  }, [advancedOpen, scrollNonce]);

  const handleOverride = (modelId: string, normalizedModelId: string) => {
    const nonce = ++nonceRef.current;
    setAdvancedOpen(true);
    setPrefill({ modelId, normalizedModelId, nonce });
    setScrollNonce(nonce);
  };

  return (
    <div data-testid={`usage-config-panel-${instanceId}`} className="space-y-4 text-sm">
      {entriesQuery.isPending ? (
        <p className="text-muted-foreground text-sm">{t("common.loading")}</p>
      ) : entriesQuery.isError ? (
        <div className="space-y-2" data-testid="usage-config-load-error" role="alert">
          <p className="text-destructive text-sm">{usageErrMsg(entriesQuery.error, t)}</p>
          <Button size="sm" variant="outline" onClick={() => void entriesQuery.refetch()}>
            {t("common.retry")}
          </Button>
        </div>
      ) : (
        <UsageConfigModelSection
          t={t}
          entries={entriesQuery.data ?? []}
          overrides={overridesQuery.data ?? []}
          onOverride={handleOverride}
        />
      )}

      <UsageDisclosure
        id={`usage-config-advanced-${instanceId}`}
        testId={`usage-config-advanced-${instanceId}`}
        title={t("usage.config.advanced")}
        summary={t("usage.config.advancedHint")}
        info={
          <UsageInfoTip
            label={t("usage.config.help.advancedLabel")}
            testId={`usage-config-advanced-info-${instanceId}`}
          >
            {t("usage.config.help.advanced")}
          </UsageInfoTip>
        }
        open={advancedOpen}
        onOpenChange={setAdvancedOpen}
      >
        <div ref={overrideRef} className="space-y-3">
          <PriceOverrideManager t={t} prefill={prefill} onPrefillHandled={() => setPrefill(null)} />
        </div>
      </UsageDisclosure>
    </div>
  );
}
