// 展开详情小表（2026-10-20 计划 §4.3；同日用户裁决补列头/虚线换算标识）。
//
// 纪律：
//  * 行 = 新增输入 / 缓存命中 / 输出 / 合计；`components.cacheWrite` 非 null 时在缓存命中
//    与输出之间插入「缓存写」行（固定占位会让明细显得多出一个不存在项）；
//  * 列 = 项目 / Token 用量 / 金额 / 单价（列头为 `text-foreground font-medium` 实体字）；
//  * 金额/单价按显示币种取后端已算好的 USD 或 CNY 值；经汇率换算的分量加虚线下划线；
//  * **合计行 = tokenUsage + 行金额，单价单元格留空**——它是行内分项合计，不是渠道/全局合计；
//  * 无数据的 component 三列显示 `—`（缺价 ≠ 0，绝不印 0.00）；
//  * 币种切换经 `useDisplayCurrency` 订阅（同窗口自定义事件 / 跨窗口 storage 事件），切换即重算。
import { ConvertedPrice } from "@/components/usage/ConvertedPrice";
import { EM_DASH, fmtTokens } from "@/lib/usage/format";
import { getLang } from "@/lib/usage/lang";
import {
  formatNativeMoney,
  formatNativeUnitPrice,
  useDisplayCurrency,
} from "@/lib/usage/displayCurrency";
import type { TFn } from "@/lib/usage/range";
import type { CostComponent, CostRow } from "@/types/usageV2";

export interface UsageCostDetailGridProps {
  row: CostRow;
  t: TFn;
}

const COMPONENT_KEYS = ["input", "cacheRead", "cacheWrite", "output"] as const;
type ComponentKey = (typeof COMPONENT_KEYS)[number];

/** 四列栅格：行名（component/合计）+ Token 用量 + 金额 + 单价。 */
const DETAIL_GRID_COLS =
  "grid grid-cols-[minmax(0,1.1fr)_minmax(0,1fr)_minmax(0,1fr)_minmax(0,1.3fr)] gap-x-2";

/** 明细行序：新增输入 → 缓存命中 →（缓存写，有数据才插入）→ 输出 → 合计。 */
function detailRowKeys(row: CostRow): ComponentKey[] {
  return COMPONENT_KEYS.filter((key) => key !== "cacheWrite" || row.components.cacheWrite !== null);
}

export function UsageCostDetailGrid({ row, t }: UsageCostDetailGridProps) {
  const lang = getLang();
  const { currency } = useDisplayCurrency();
  const hint = t("usage.cost.convertedPriceHint");

  const componentRow = (key: ComponentKey) => {
    const component: CostComponent | null = row.components[key];
    const amount =
      component === null
        ? null
        : formatNativeMoney(
            currency === "cny" ? component.estimatedCny : component.estimatedUsd,
            currency
          );
    const unitPrice =
      component === null
        ? null
        : formatNativeUnitPrice(
            currency === "cny" ? component.unitCnyPerMillion : component.unitUsdPerMillion,
            currency
          );
    const converted =
      component !== null && (currency === "cny" ? component.cnyConverted : component.usdConverted);
    return (
      <div
        key={key}
        data-testid={`usage-cost-table-detail-row-${key}`}
        className={DETAIL_GRID_COLS}
      >
        <span>{t(`usage.cost.component.${key}`)}</span>
        <span className="text-right tabular-nums">
          {component === null ? EM_DASH : fmtTokens(component.tokens, lang)}
        </span>
        <span className="text-right tabular-nums">
          <ConvertedPrice converted={converted} hint={hint}>
            {amount ?? EM_DASH}
          </ConvertedPrice>
        </span>
        <span className="text-right tabular-nums">
          {unitPrice === null ? (
            EM_DASH
          ) : (
            <ConvertedPrice converted={converted} hint={hint}>
              {t("usage.cost.unitPrice", {
                value: unitPrice,
                currency: currency.toUpperCase(),
              })}
            </ConvertedPrice>
          )}
        </span>
      </div>
    );
  };

  const totalAmount = formatNativeMoney(
    currency === "cny" ? row.estimatedCny : row.estimatedUsd,
    currency
  );
  const totalConverted = currency === "cny" ? row.cnyConverted : row.usdConverted;

  return (
    <div data-testid="usage-cost-table-detail-grid" className="text-muted-foreground grid gap-y-1">
      {/* 列头：项目 / Token 用量 / 金额 / 单价（2026-10-20 用户要求补全并改实体字） */}
      <div
        className={`${DETAIL_GRID_COLS} text-foreground border-b pb-1 font-medium`}
        data-testid="usage-cost-table-detail-header"
      >
        <span>{t("usage.costTable.colItem")}</span>
        <span className="text-right">{t("usage.costTable.tokenAmount")}</span>
        <span className="text-right">{t("usage.costTable.colAmountShort")}</span>
        <span className="text-right">{t("usage.costTable.colUnitPrice")}</span>
      </div>
      {detailRowKeys(row).map(componentRow)}
      {/* 合计行 = 本行 Token 与金额合计；单价留空；不是渠道小计 / 全局合计 */}
      <div
        data-testid="usage-cost-table-detail-row-total"
        className={`${DETAIL_GRID_COLS} border-t pt-1 font-medium`}
      >
        <span>{t("usage.costTable.total")}</span>
        <span className="text-right tabular-nums">{fmtTokens(row.tokenUsage, lang)}</span>
        <span className="text-right tabular-nums">
          <ConvertedPrice converted={totalConverted} hint={hint}>
            {totalAmount ?? EM_DASH}
          </ConvertedPrice>
        </span>
        <span className="text-right tabular-nums">{t("usage.costTable.unitPriceBlank")}</span>
      </div>
    </div>
  );
}
