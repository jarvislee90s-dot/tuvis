// 金额显示（Task 15）——USD 字符串 → 展示字符串。**不做浮点转换**。
//
// 规则（Plan 逐字）：
//  * `null` → `null`（未估价，由组件显示「未估价」而不是 `$0.00`）；
//  * 只接受 `^\d+(\.\d+)?$`；非法/负数/科学计数/千分位输入一律 `null`；
//  * 整数部分加千分位；小数去掉尾零后**至少**保留两位（`0` → `$0.00`，`0.5` → `$0.50`）；
//  * 小额完整保留（`0.000529` → `$0.000529`），大数不转科学计数法；
//  * 完整金额由组件放进 title/详情，不在这里截断。
export function formatUsd(value: string | null): string | null {
  if (value === null) return null;
  if (!/^\d+(\.\d+)?$/.test(value)) return null;
  const [whole, raw = ""] = value.split(".");
  const fraction = raw.replace(/0+$/, "");
  const grouped = whole.replace(/\B(?=(\d{3})+(?!\d))/g, ",");
  return `$${grouped}.${fraction.padEnd(2, "0")}`;
}
