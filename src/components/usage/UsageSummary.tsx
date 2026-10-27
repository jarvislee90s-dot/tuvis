// 首屏概要（V2 Task 12）：hero + 7 行固定顺序指标网格 + 页脚。
//
// 口径：
//  * hero = `fmtTokens(dash.hero, lang)`（万/亿缩写；2026-10-19 用户覆盖旧完整千分位约定）；
//  * 四项 token 行顺序 = 新增输入 / 缓存命中 / 未缓存命中 / 输出；cacheMiss 恒为
//    `requestTotal - cacheRead`（本期与上期同公式，见 `summaryV2.ts`）；
//  * 辅助两项 = 命中率 / 请求次数；asOf 保留；
//  * **已删除**「用户输入(估)」行/变量/GridKey；
//  * **已删除**「缓存写入（解释）」整块（2026-10-20 计划需求 7；缓存写仅在费用明细有数据时显示）；
//  * 没量到 token 时：四项 token 派生行 + 命中率出 `—`，计数与 asOf 保留真值；
//  * 数字恒 `tabular-nums` + `whitespace-nowrap`，不折行；Token 数值不再挂完整千分位 tooltip。
import { Card, CardContent } from "@/components/ui/card";
import { EM_DASH, fmtInt, fmtPct, fmtTokens } from "@/lib/usage/format";
import { getLang } from "@/lib/usage/lang";
import {
  asOfText,
  compareSuffix,
  hitCompareSuffix,
  prevOf,
  tokenUnmeasured,
  type TFn,
} from "@/lib/usage/range";
import { summaryNumbers } from "@/lib/usage/summaryV2";
import type { UsageDashboard } from "@/types/usage";

/** 7 行固定顺序（键同时是 i18n / testid 后缀） */
type GridKey =
  "requestInput" | "cacheRead" | "cacheMiss" | "output" | "hitRate" | "requests" | "asOf";

interface GridRow {
  key: GridKey;
  value: string;
  exact: string;
  compare: string;
  compareTitle: string;
}

export function UsageSummary({ dash, t }: { dash: UsageDashboard; t: TFn }) {
  const lang = getLang();
  const prev = prevOf(dash.compare);
  const unmeasured = tokenUnmeasured(dash);
  const cur = summaryNumbers(dash.totals, dash.totalsBuckets);
  const prevNumbers = prev ? summaryNumbers(prev.metrics, prev.buckets) : null;

  const blankRow = (key: GridKey): GridRow => ({
    key,
    value: EM_DASH,
    exact: "",
    compare: "",
    compareTitle: "",
  });

  const tokenRow = (key: GridKey, value: number, prevValue: number | null | undefined): GridRow =>
    unmeasured
      ? blankRow(key)
      : {
          key,
          value: fmtTokens(value, lang),
          exact: "",
          compare: prevValue == null ? "" : compareSuffix(value, prevValue, t),
          compareTitle:
            prevValue == null ? "" : t("usage.compareHint", { prev: fmtTokens(prevValue, lang) }),
        };

  const collected = asOfText(dash.collectedAt, t);
  const rows: GridRow[] = [
    tokenRow("requestInput", cur.requestInput, prevNumbers?.requestInput),
    tokenRow("cacheRead", cur.cacheRead, prevNumbers?.cacheRead),
    // 上期 cacheMiss 与本期同公式（requestTotal - cacheRead）
    tokenRow("cacheMiss", cur.cacheMiss, prevNumbers?.cacheMiss),
    tokenRow("output", cur.output, prevNumbers?.output),
    unmeasured
      ? blankRow("hitRate")
      : {
          key: "hitRate",
          value: fmtPct(cur.hitRate),
          exact: "",
          compare: hitCompareSuffix(cur.hitRate, dash.compare, t),
          compareTitle: dash.compare
            ? t("usage.compareHint", { prev: fmtPct(dash.compare.prevMetrics.cacheHitRate) })
            : "",
        },
    {
      key: "requests",
      value: fmtInt(cur.requests),
      exact: "",
      compare: compareSuffix(cur.requests, prevNumbers?.requests ?? null, t),
      compareTitle:
        prevNumbers == null ? "" : t("usage.compareHint", { prev: fmtInt(prevNumbers.requests) }),
    },
    {
      key: "asOf",
      value: collected,
      exact: "",
      compare: "",
      compareTitle: "",
    },
  ];

  return (
    <Card data-testid="usage-summary-card" className="gap-3 py-4">
      <CardContent className="space-y-2 px-4">
        <div>
          <div className="text-3xl font-semibold tabular-nums" data-testid="usage-hero">
            {unmeasured ? EM_DASH : fmtTokens(dash.hero, lang)}
          </div>
          <p className="text-muted-foreground mt-1 text-xs">{t("usage.hero.sub")}</p>
        </div>

        <div className="grid gap-x-8 gap-y-1 sm:grid-cols-2 lg:grid-cols-3">
          {rows.map((row) => (
            <div
              key={row.key}
              data-testid={`usage-grid-${row.key}`}
              className="grid grid-cols-[minmax(0,1fr)_auto_4.75rem] items-baseline gap-x-2 text-sm"
            >
              <span className="text-muted-foreground truncate">{t(`usage.grid.${row.key}`)}</span>
              <span
                className="text-right font-medium whitespace-nowrap tabular-nums"
                title={row.exact || undefined}
              >
                {row.value}
              </span>
              <span
                className="text-muted-foreground text-right text-xs whitespace-nowrap"
                title={row.compareTitle || undefined}
              >
                {row.compare}
              </span>
            </div>
          ))}
        </div>
      </CardContent>
    </Card>
  );
}
