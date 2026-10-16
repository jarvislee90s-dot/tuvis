// 大看板页（计划② Task 6 步骤 7 + Task 7 步骤 5 + Task 8 步骤 6 + Task 9 步骤 5）：范围条 + 页签壳
// （看板 / 记录，共用同一 range）+ 看板三态 + 首屏概要 + 趋势卡 + 工作小结 + 分布卡。窗口与两个入口
// 见 `src/lib/usage/openWindow.ts` / `main-title-bar.tsx` / `PetMenu.tsx`；Task 11 在本页范围条同行
// 右侧插入导出行。
//
// 数据流（§3 第 15/16/17 条）：
//  * 打开即 `useUsageCollect()` 按需采集**一次**（`force=false`；最小间隔与并发单飞由后端保证，
//    前端不做二次节流）——采集成功后一次失效 `["usage"]` 前缀，读查询随之重取；
//  * 三条读命令只读账本、**绝不触发扫描**；环比**不另发第二个查询**（差值由 `UsageSummary` 前端算）；
//  * 设置走 `useUsageSettingsQuery`（**唯一**设置入口；直接调 `usageGetSettings` 会逃出 `["usage"]` 失效家族）。
//
// 三态分支（spec P8）：`isPending → 加载` / `isError → 错误（可重试，message 走 usageErrMsg）` /
// `isDashboardEmpty → 空态` / 否则 board。切档时 `custom` 走 `clampCustomRange` 并回写 `clamped`，
// 其余档清 `clamped`。
//
// 分组维度（Task 7 步骤 5）：`groupBy` 由常量改为**可切换 state**（默认「按工具」），四维切换只改
// 查询入参（`useUsageDashboardQuery(range, groupBy)` 的键含它）——分布卡是受控组件，切换回调进这里。
//
// 页签壳（Task 9 步骤 5）：`tab` 默认 `"board"`；**两页签共用同一 `range` 与同一次打开窗口**——
// 记录页签只换渲染分支（`UsageRecordsPanel` 自带三态，**不复用** board 的 `body`），range / 设置 /
// 采集仍是本页这一份；而**筛选状态与卡片维度由本页持有**（`useRecordsFilters`，2026-10-06 用户裁决
// 「CSV 要接收记录页筛选」之后，屏幕与导出必须同一出处）。
import { useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { TitleBar } from "@/components/common/title-bar";
import { WindowFrame } from "@/components/common/window-frame";
import { Button } from "@/components/ui/button";
import { trendPoints } from "@/components/usage/TrendChart";
import { UsageDistributionCard } from "@/components/usage/UsageDistributionCard";
import { UsageExportActions } from "@/components/usage/UsageExportActions";
import { UsageRecordsPanel } from "@/components/usage/UsageRecordsPanel";
import { UsageEmpty, UsageError, UsageLoading } from "@/components/usage/UsageStates";
import { UsageRangeBar } from "@/components/usage/UsageRangeBar";
import { UsageSummary } from "@/components/usage/UsageSummary";
import { UsageTrendCard } from "@/components/usage/UsageTrendCard";
import { UsageWorkSummary } from "@/components/usage/UsageWorkSummary";
import { usageErrMsg } from "@/components/usage/usageErrors";
import { fmtTokens } from "@/lib/usage/format";
import { getLang } from "@/lib/usage/lang";
import {
  useUsageCollect,
  useUsageDashboardQuery,
  useUsageSettingsQuery,
} from "@/lib/query/queries/usage";
import {
  clampCustomRange,
  compareSuffix,
  defaultCustomRange,
  DEFAULT_PRESET,
  isDashboardEmpty,
  prevOf,
  rangeOf,
  tokenUnmeasured,
} from "@/lib/usage/range";
import { useRecordsFilters } from "@/lib/usage/useRecordsFilters";
import type { UsageGroupBy, UsageRangePreset } from "@/types/usage";

/** 窗口内的两个页签（spec P3「两者同一窗口、两个页签」）；数组顺序即页签条顺序 */
type UsageTab = "board" | "records";

export default function UsageDashboardPage() {
  const { t } = useTranslation();
  // 格式化语言（issue #160）：趋势卡峰值的数值单位（万/亿 vs K/M/B）随界面语言走。
  const lang = getLang();
  const [preset, setPreset] = useState<UsageRangePreset>(DEFAULT_PRESET);
  const [custom, setCustom] = useState(() => defaultCustomRange(new Date()));
  const [clamped, setClamped] = useState(false);
  /** 分布维度：默认「按工具」（spec P1 第 5 条的四维之一） */
  const [groupBy, setGroupBy] = useState<UsageGroupBy>("tool");
  /** 当前页签：默认看板；记录页签共用同一个 `range`（同一次打开窗口） */
  const [tab, setTab] = useState<UsageTab>("board");

  const range = rangeOf(preset, custom);
  useUsageCollect();
  const settings = useUsageSettingsQuery();
  const dashboard = useUsageDashboardQuery(range, groupBy);
  /**
   * 记录页筛选（工具 chips + 子代理开关）与**卡片维度**的**唯一出处**。
   *
   * **为什么由本页持有**（2026-10-06 用户裁决：「CSV 需要接收记录页的筛选条件……那最后导出的 CSV
   * 应该也是这么一个口径」）：导出与屏幕若各持一份状态必然漂移，且漂移在界面上**看不出来**。
   * 故屏幕（`UsageRecordsPanel`）与导出（`UsageExportActions`）读的是同一份；三层日档守卫
   * （判据 / 复位 / filters 双保险）跟着状态一起搬进 `useRecordsFilters`，**没有**被落下。
   */
  const rec = useRecordsFilters(range);

  /** 切档：预设四档只发 `preset`（窗口边界由后端裁定）；切到非自定义档即清「已截断」提示 */
  const handleSelectPreset = (next: UsageRangePreset) => {
    setPreset(next);
    if (next !== "custom") setClamped(false);
  };

  /**
   * 自定义日期编辑：先 `clampCustomRange`（31 天含首尾）再回写——输入框显示的必须是**实际查询**的区间；
   * `clamped` 只在真被上限截断时为真，常驻到用户下次编辑日期（状态归本页持有）。
   */
  const handleCustomChange = (next: { from: string; to: string }) => {
    const fixed = clampCustomRange(next.from, next.to);
    setCustom({ from: fixed.from, to: fixed.to });
    setClamped(fixed.clamped);
  };

  const dash = dashboard.data;
  let body: ReactNode;
  if (dashboard.isPending) {
    body = <UsageLoading label={t("common.loading")} />;
  } else if (dashboard.isError) {
    // 用量命令 reject 结构化 `{ code, detail }` → 一律 usageErrMsg（String(e) 会印 [object Object]）
    body = (
      <UsageError
        title={t("usage.error.title")}
        message={usageErrMsg(dashboard.error, t)}
        retryLabel={t("common.retry")}
        onRetry={() => void dashboard.refetch()}
      />
    );
  } else if (!dash || isDashboardEmpty(dash)) {
    // 空态：显示「暂无数据」而不是 0；`collectedAt === 0` 的哨兵文案在 board 的 asOf / 页脚
    body = <UsageEmpty label={t("usage.empty")} />;
  } else {
    // 趋势点归一在 Task 4 的 `trendPoints` 里（值口径 = 请求输入 + 产出，与 hero 同源，不从四桶反推）；
    // 峰值文案由本页生成（万/亿缩写），任务 7 的卡片在 `busy` 时自己不显示它（不拿上一档的数字糊弄）
    const points = trendPoints(dash.trend);
    // **没量到 token** 的窗口（2026-10-06 用户裁决）：趋势的**峰值**与**区间合计环比**都是 token 量
    // ⇒ 交给卡片**整卡空态**（`unmeasured` prop）——全零趋势会被画成一条贴底零线，那是假图；
    // 页面**不再**用「峰值 —」去打补丁（整卡不渲染，那个怪值就不该存在）。
    const unmeasured = tokenUnmeasured(dash);
    // 趋势区的**文字**环比（spec P8：「指标网格各项**与趋势区**都显示对比值」）。
    // 口径 = 本区间 hero（= Σ 趋势点值 = `requestTotal + output`）环比上一等长区间；
    // 不画上一周期的折线——契约只带上一周期的聚合值，没有序列（画线要改冻结契约）。
    // ⚠️ **必须带主语**（2026-10-06 评审）：这一行在卡面上紧贴在「峰值 {{v}}」正下方，而它比的是
    // **整窗合计**、不是峰值。不加主语就会读成「峰值 11.28万，上一周期 175.18万，还涨 16.3%」——
    // 数字全对、归属全错。故前缀 `usage.card.compareSubject`（「区间合计」）。
    // `compare === null` → `prevHero === null` → `compareSuffix` 返回空串 ⇒ 整段不渲染（不出 0% / NaN）。
    const prevHero = (() => {
      const p = prevOf(dash.compare);
      return p ? p.metrics.requestTotal + p.buckets.output : null;
    })();
    const trendCompare = compareSuffix(dash.hero, prevHero, t);
    const trendCompareLabel =
      trendCompare === "" ? "" : `${t("usage.card.compareSubject")} ${trendCompare}`;
    body = (
      <>
        <UsageSummary
          dash={dash}
          retentionDays={settings.data?.detailRetentionDays ?? null}
          t={t}
        />
        <UsageTrendCard
          points={points}
          // 没量到 token ⇒ **整卡空态**（不给峰值、也不画零线）：判据只在卡片自己那一处生效，
          // 页面**不再**用 `peakLabel = —` 去打补丁（整卡不渲染，「峰值 —」这个怪值就不该存在）。
          unmeasured={unmeasured}
          peakLabel={t("usage.card.peak", {
            v: fmtTokens(Math.max(0, ...points.map((p) => p.value)), lang),
          })}
          compareLabel={trendCompareLabel}
          title={t("usage.card.trend")}
          loadingLabel={t("common.loading")}
          emptyLabel={t("usage.empty")}
          busy={dashboard.isFetching}
        />
        {/* Task 8 的工作小结区：趋势卡之后、分布卡之前（D15–D19，逐指标空态） */}
        <UsageWorkSummary work={dash.workSummary} availability={dash.availability} t={t} />
        <UsageDistributionCard
          rows={dash.rows}
          groupBy={groupBy}
          onGroupByChange={setGroupBy}
          t={t}
        />
      </>
    );
  }

  return (
    <WindowFrame
      titleBar={<TitleBar title={t("usage.title")} showMinimize={false} />}
      contentClassName="flex-1 overflow-y-auto"
    >
      <div className="space-y-4 p-5">
        {/* 范围条 + 导出 + 页签条 = **吸顶头**（2026-10-06 排版修复）：看板窗 1040×720 而内容约
            1700px ⇒ 范围条一滚就没了（真机截图顶部只剩页签条，五档范围与导出全在视口外）。
            `-mx-5 px-5` 抵消外层 `p-5` 让底色铺满整宽；`bg-background/95` + `backdrop-blur`
            保证滚过的内容不透出来；取色一律主题 token（零硬编码色值）。 */}
        <div className="bg-background/95 sticky top-0 z-10 -mx-5 space-y-4 border-b px-5 pt-5 pb-3 backdrop-blur">
          {/* 范围条与导出行**同行**：左侧五档 + 自定义区间，右侧导出动作条（Task 11 步骤 6）。
            导出口径 = **当前页签那一份**（2026-10-06 用户裁决：CSV 要接收记录页的筛选条件）：
            * 记录页签 → 记录页的 `filters`（工具 chips + 子代理模式，经三层日档守卫）与**卡片维度**
              `cardDim` 作 `groupBy` —— 屏幕上按什么分组，导出就按什么分组；记录页的卡片维度控件
              就在同一屏，用户看得见；
            * 看板页签 → `filters: {}` + 看板的分布维度 `groupBy`（看板没有筛选控件，故不筛）。
            `filters` 由 `useRecordsFilters` 产出（**已经是**第三层守卫的产物：日档不含
            `subagentMode`），所以这里**不会**撞上「日档 + parentsOnly」的 `usage-filter-unavailable`。 */}
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
              groupBy={tab === "records" ? rec.cardDim : groupBy}
              t={t}
            />
          </div>

          {/* 页签条（范围条下方；Task 11 的导出行落在范围条同行右侧，与这里不争位） */}
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
          body
        ) : (
          // 记录页：面板自带三态；筛选状态与卡片维度由本页注入（与导出行**同一份**，见上）
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
