// 费用行组织纯层（Task 9；2026-10-20 计划 §4.3 增加平铺模式）——「按渠道 / 按模型」
// 两种查看方式的唯一出处。
//
// 纪律：
//  * 只排序/分组，**不乘价、不产生金额小计**（组织项只有 key/label/rows）；
//  * 排序：tokenUsage 降序，同值 key 升序（稳定、可复现）；
//  * channel 布局：直接按后端 `row.channelKey` 分组（`bp:` / `provider:` / `unassigned` /
//    `ambiguous`）；unassigned/ambiguous 由后端的费用行键携带工具/原始 provider 上下文，
//    但渠道视图仍只有一个组，不再从 `profiles` 字典猜；
//  * flat 布局（按模型视图）：直接返回**单组**（行名由组件层拼「自定义供应商名/模型名」，
//    组的 key/label 不参与渲染）；
//  * unassigned/ambiguous 的 label 是 i18n 键，交给组件 `t(label)`，纯层不硬编码文案；
//  * Top5 只是组件展示层的事——本层返回全部行，不丢数据。
import type { CostRow } from "@/types/usageV2";

/** 卡内视图两态按钮（排序/分组切换语义；2026-10-20 计划冻结口径）。 */
export type CostView = "channel" | "model";

/** 组织层布局：渠道组段 / 平铺单组。按模型视图映射到 flat（`costLayoutOf`）。 */
export type CostLayout = "channel" | "flat";

export interface CostGroup {
  key: string;
  label: string;
  rows: CostRow[];
}

/** 卡内视图 → 组织层布局：按渠道 = 渠道组段；按模型 = 平铺单组。 */
export function costLayoutOf(view: CostView): CostLayout {
  return view === "channel" ? "channel" : "flat";
}

/** 渠道展示名：后端已解析的显示名；未指定/可能混合返回 i18n 键。 */
export function costChannelLabel(row: CostRow): string {
  switch (row.channelKind) {
    case "unassigned":
      return "usage.label.unassignedChannel";
    case "ambiguous":
      return "usage.label.ambiguousChannel";
    default:
      return row.displayProvider ?? "";
  }
}

function sortRows(rows: readonly CostRow[]): CostRow[] {
  return [...rows].sort((a, b) => b.tokenUsage - a.tokenUsage || a.key.localeCompare(b.key));
}

function tokensOf(rows: readonly CostRow[]): number {
  return rows.reduce((sum, row) => sum + row.tokenUsage, 0);
}

/**
 * 组织费用行。返回值**不含任何金额字段**，也不在纯层做乘法。
 * - `channel`：Map 键 = `row.channelKey`；组内/组间均按 Token 降序。
 * - `flat`：直接返回单组（按模型视图；行名含供应商名由组件层承担），Token 降序。
 */
export function organizeCosts(rows: readonly CostRow[], layout: CostLayout): CostGroup[] {
  const sorted = sortRows(rows);

  if (layout === "flat") {
    return [{ key: "flat", label: "", rows: sorted }];
  }

  const groups = new Map<string, CostGroup>();
  for (const row of sorted) {
    const existing = groups.get(row.channelKey);
    if (existing) {
      existing.rows.push(row);
    } else {
      groups.set(row.channelKey, {
        key: row.channelKey,
        label: costChannelLabel(row),
        rows: [row],
      });
    }
  }

  return [...groups.values()].sort(
    (a, b) => tokensOf(b.rows) - tokensOf(a.rows) || a.key.localeCompare(b.key)
  );
}
