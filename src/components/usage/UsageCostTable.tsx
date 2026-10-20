// D 卡模型卡片（2026-10-20 计划 §4.3；同日用户裁决：每条模型一张卡片、明细包在卡片内、默认展开）。
//
// 纪律：
//  * 卡片主行五列 = 显示供应商/自定义模型名 | 估价 | 总 Token | 缓存命中率 | 计费详情；
//    列宽 `minmax(0,1fr) auto auto auto auto`，容器 `overflow-x-hidden`，长名 `truncate` + title
//    ——任何内容宽度下都不出现横向滚动；
//  * 列头/明细列头一律 `text-foreground font-medium`（用户要求的黑色实体字）；
//  * 估价列按显示币种取后端算好的 USD/CNY：unpriced/null → 「—（未估价）」（缺价绝不显示 0.00）；
//    partial → 「已估价部分 X」+ 该币种覆盖率；经汇率换算的金额加虚线下划线；
//  * 展开详情 = `UsageCostDetailGrid` 四行三列小表 + 小字区（归一化模型 + 快照获取时间一行、
//    不准确原因 `·` 一行、pricingBasis 文案、用户覆盖价徽标）；明细**在卡片内部**；
//  * 按渠道 = 渠道组段（组头 = 显示名 / i18n 键，**无金额小计**，行名不带渠道前缀，
//    unassigned/ambiguous 保留工具 / 原始证据上下文）；平铺（按模型）= 行名含供应商名；
//  * 契约保留：无全局费用总额、无渠道金额小计、`usage-cost-*` 前缀 testid 沿用。
import { useState } from "react";
import { ChevronDown, ChevronRight } from "lucide-react";
import { Button } from "@/components/ui/button";
import { ConvertedPrice } from "@/components/usage/ConvertedPrice";
import { UsageCostDetailGrid } from "@/components/usage/UsageCostDetailGrid";
import { EM_DASH, fmtDateTime, fmtPct, fmtTokens, usageAgentLabel } from "@/lib/usage/format";
import { getLang } from "@/lib/usage/lang";
import { formatNativeMoney, useDisplayCurrency } from "@/lib/usage/displayCurrency";
import type { TFn } from "@/lib/usage/range";
import type { CostRow } from "@/types/usageV2";

export interface CostTableItem {
  row: CostRow;
  /** 渠道显示名（已知渠道名，或由调用方翻译好的 unassigned/ambiguous） */
  channelName: string;
}

export interface UsageCostTableGroup {
  key: string;
  label: string;
  items: CostTableItem[];
}

export interface UsageCostTableProps {
  t: TFn;
  groups: UsageCostTableGroup[];
  /** 平铺（按模型视图）：不渲染渠道组段，行名含供应商名 */
  flat?: boolean;
}

/** 五列主栅格：名称（可截断）| 估价 | 总 Token | 缓存命中率 | 计费详情。 */
const TABLE_GRID_COLS = "grid grid-cols-[minmax(0,1fr)_auto_auto_auto_auto] items-baseline gap-x-3";

/**
 * 原始 provider 证据面：优先使用后端返回的完整去重列表（多个原始 provider 全部展示），
 * 旧形状/测试夹具缺失该字段时回退单值，不把单个 provider 冒充完整证据。
 */
function observedEvidenceOf(row: CostRow): string | null {
  const list = row.observedProviders ?? [];
  if (list.length > 0) return list.join(", ");
  return row.identity.observedProvider;
}

/** 未指定/可能混合行：行首追加工具上下文；已知渠道只显示显示名。 */
function displayNameOf(row: CostRow, channelName: string): string {
  const tools = [...new Set(row.sourceIds)].map(usageAgentLabel).join(", ");
  if (row.channelKind === "unassigned") {
    return tools ? `${channelName} · ${tools}` : channelName;
  }
  if (row.channelKind === "ambiguous") {
    const context = observedEvidenceOf(row) ?? tools;
    return context ? `${channelName} · ${context}` : channelName;
  }
  return channelName;
}

/**
 * 渠道组段内行名的上下文部分（不含渠道名）：
 * 已知渠道返回 null（渠道由组头给出）；unassigned/ambiguous 返回工具或原始证据。
 */
function contextOnlyOf(row: CostRow): string | null {
  const tools = [...new Set(row.sourceIds)].map(usageAgentLabel).join(", ");
  if (row.channelKind === "unassigned") {
    return tools || null;
  }
  if (row.channelKind === "ambiguous") {
    return observedEvidenceOf(row) ?? (tools || null);
  }
  return null;
}

function reasonText(reason: string, t: TFn): string {
  const key = `usage.cost.reason.${reason}`;
  const text = t(key);
  return text === key ? reason : text;
}

interface UsageCostCardRowProps {
  row: CostRow;
  channelName: string;
  t: TFn;
  /** 平铺（按模型）行名含渠道名；渠道组段内行名只留模型 + 上下文 */
  showChannelName: boolean;
}

function UsageCostCardRow({ row, channelName, t, showChannelName }: UsageCostCardRowProps) {
  const lang = getLang();
  const { currency } = useDisplayCurrency();
  const [open, setOpen] = useState(false);

  const status = currency === "cny" ? row.statusCny : row.status;
  const coverageRate = currency === "cny" ? row.coverageRateCny : row.coverageRate;
  const converted = currency === "cny" ? row.cnyConverted : row.usdConverted;
  const amount = formatNativeMoney(
    currency === "cny" ? row.estimatedCny : row.estimatedUsd,
    currency
  );
  const amountMain =
    status === "unpriced" || amount === null
      ? t("usage.cost.unpriced")
      : status === "partial"
        ? t("usage.cost.partialAmount", { amount })
        : amount;

  const rawModelId = row.identity.modelId;
  // 自定义模型名（后端按行成员一致配置解析）优先；空模型显示「未知模型」
  const modelText =
    row.displayModelName ?? (rawModelId === "" ? t("usage.label.unknownModel") : rawModelId);
  const displayName = displayNameOf(row, channelName);
  const contextOnly = contextOnlyOf(row);
  const rowName = showChannelName
    ? `${displayName} / ${modelText}`
    : contextOnly
      ? `${contextOnly} / ${modelText}`
      : modelText;

  // title 保留完整原始 provider 证据与完整原始模型（缩写/自定义名都不丢证据）；
  // ambiguous 的 provider 已进首上下文，不再重复；组内混入空 provider 行时显式提示。
  const evidence = observedEvidenceOf(row);
  const unlabeled =
    row.hasEmptyProvider && (row.observedProviders?.length ?? 0) > 0
      ? t("usage.cost.unlabeledSource")
      : null;
  const rawIdentity = [row.channelKind === "ambiguous" ? null : evidence, unlabeled, rawModelId]
    .filter((part): part is string => Boolean(part))
    .join(" / ");
  const fullName = rawIdentity ? `${displayName} / ${rawIdentity}` : displayName;

  const detailsId = `usage-cost-details-${row.key}`;

  return (
    <div
      className="border-border rounded-md border p-2.5"
      data-testid={`usage-cost-card-${row.key}`}
    >
      <div className="contents" data-testid={`usage-cost-row-${row.key}`}>
        <div className={TABLE_GRID_COLS}>
          <span
            data-testid={`usage-cost-name-${row.key}`}
            className="min-w-0 truncate font-medium"
            title={fullName}
          >
            {rowName}
          </span>
          <span data-testid={`usage-cost-amount-${row.key}`} className="text-right tabular-nums">
            <ConvertedPrice
              converted={converted && amountMain !== t("usage.cost.unpriced")}
              hint={t("usage.cost.convertedPriceHint")}
            >
              <span className="whitespace-nowrap">{amountMain}</span>
            </ConvertedPrice>
            {status === "partial" && coverageRate !== null ? (
              <span
                data-testid={`usage-cost-coverage-${row.key}`}
                className="text-muted-foreground block text-xs whitespace-nowrap"
              >
                {t("usage.cost.coverage")} {fmtPct(coverageRate)}
              </span>
            ) : null}
          </span>
          <span
            data-testid={`usage-cost-token-${row.key}`}
            className="text-muted-foreground text-right text-xs whitespace-nowrap tabular-nums"
          >
            {fmtTokens(row.tokenUsage, lang)}
          </span>
          <span
            data-testid={`usage-cost-hit-${row.key}`}
            className="text-muted-foreground text-right text-xs whitespace-nowrap tabular-nums"
          >
            {row.cacheHitRate === null ? EM_DASH : fmtPct(row.cacheHitRate)}
          </span>
          <span className="text-right">
            <Button
              size="xs"
              variant="ghost"
              className="text-muted-foreground hover:text-foreground h-6 px-1"
              aria-expanded={open}
              aria-controls={detailsId}
              data-testid={`usage-cost-table-detail-toggle-${row.key}`}
              onClick={() => setOpen((v) => !v)}
            >
              {open ? (
                <ChevronDown className="h-3.5 w-3.5" aria-hidden="true" />
              ) : (
                <ChevronRight className="h-3.5 w-3.5" aria-hidden="true" />
              )}
              {t("usage.cost.details")}
            </Button>
          </span>
        </div>
        {/* 展开详情：常驻 DOM、收起时 hidden（与旧原生 details 的 DOM 契约一致，
            下游测试/无障碍可在收起态定位区域；展开态由按钮 aria-expanded 表达） */}
        <div
          id={detailsId}
          data-testid={detailsId}
          hidden={!open}
          className="mt-2 space-y-1.5 rounded-md border p-2 text-xs"
        >
          <UsageCostDetailGrid row={row} t={t} />
          <p
            data-testid={`usage-cost-table-footnote-${row.key}`}
            className="text-muted-foreground text-[11px]"
          >
            {t("usage.cost.normalizedModel")}{" "}
            <span className="font-mono">{row.normalizedModelId ?? EM_DASH}</span>
            {" · "}
            {t("usage.cost.priceUpdated")}{" "}
            {row.priceUpdatedAt === null ? EM_DASH : fmtDateTime(row.priceUpdatedAt)}
          </p>
          {row.reasons.length > 0 ? (
            <p
              data-testid={`usage-cost-table-reasons-${row.key}`}
              className="text-muted-foreground text-[11px]"
            >
              {t("usage.costTable.reasonsLine")}：
              {row.reasons.map((reason) => reasonText(reason, t)).join(" · ")}
            </p>
          ) : null}
          <p className="text-muted-foreground text-[11px]">
            {row.pricingBasis === "userOverride"
              ? t("usage.cost.userOverride")
              : row.pricingBasis === "baseCurrent"
                ? t("usage.cost.baseCurrent")
                : t("usage.cost.insufficient")}
          </p>
          {row.priceOverride ? (
            <span
              data-testid={`usage-cost-override-${row.key}`}
              className="inline-block rounded border px-1 text-[11px]"
            >
              {t("usage.cost.priceOverride")}
            </span>
          ) : null}
        </div>
      </div>
    </div>
  );
}

export function UsageCostTable({ t, groups, flat = false }: UsageCostTableProps) {
  return (
    <div data-testid="usage-cost-table-scroll" className="space-y-2 overflow-x-hidden">
      <div
        id="usage-cost-rows"
        data-testid={flat ? "usage-cost-models" : "usage-cost-groups"}
        className="space-y-3"
      >
        {/* 列头五列（顺序即口径；实体字，2026-10-20 用户裁决） */}
        <div
          data-testid="usage-cost-table-header"
          className={`${TABLE_GRID_COLS} text-foreground border-b px-2.5 pb-1 text-xs font-medium`}
        >
          <span data-testid="usage-cost-table-col-name">{t("usage.costTable.colName")}</span>
          <span data-testid="usage-cost-table-col-amount" className="text-right">
            {t("usage.costTable.colAmount")}
          </span>
          <span data-testid="usage-cost-table-col-token" className="text-right">
            {t("usage.costTable.colToken")}
          </span>
          <span data-testid="usage-cost-table-col-hit" className="text-right">
            {t("usage.costTable.colHitRate")}
          </span>
          <span data-testid="usage-cost-table-col-detail" className="text-right">
            {t("usage.costTable.colDetail")}
          </span>
        </div>

        {flat
          ? groups.flatMap((group) =>
              group.items.map((item) => (
                <UsageCostCardRow
                  key={item.row.key}
                  row={item.row}
                  channelName={item.channelName}
                  t={t}
                  showChannelName
                />
              ))
            )
          : groups.map((group) => (
              <section
                key={group.key}
                data-testid={`usage-cost-group-${group.key}`}
                className="space-y-2"
              >
                {/* 组头只有显示名 / i18n 键；**无金额小计** */}
                <h3 className="text-muted-foreground px-1 text-xs font-medium">{group.label}</h3>
                <div className="space-y-2">
                  {group.items.map((item) => (
                    <UsageCostCardRow
                      key={item.row.key}
                      row={item.row}
                      channelName={item.channelName}
                      t={t}
                      showChannelName={false}
                    />
                  ))}
                </div>
              </section>
            ))}
      </div>
    </div>
  );
}
