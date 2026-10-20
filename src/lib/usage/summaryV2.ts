// V2 Token 总览数值派生（Task 12）——A 卡与上期环比的**唯一口径**。
//
// 与旧 `distribution.ts::distValue` 同源：值口径恒为 `requestTotal + output`（hero 口径），
// 不从四桶反推（Subset 语义下可能重叠）。`cacheMiss = requestTotal - cacheRead` 用同一公式
// 计算本期与上期，避免两套口径。
import type { UsageBuckets, UsageMetrics } from "@/types/usage";

export interface SummaryNumbers {
  total: number;
  requestInput: number;
  cacheRead: number;
  cacheMiss: number;
  output: number;
  hitRate: number;
  requests: number;
}

/** A 卡派生数值（不修改入参；上期值也用它，保证 cacheMiss 同公式） */
export function summaryNumbers(m: UsageMetrics, b: UsageBuckets): SummaryNumbers {
  return {
    total: m.requestTotal + b.output,
    requestInput: m.requestTotal,
    cacheRead: b.cacheRead,
    cacheMiss: m.requestTotal - b.cacheRead,
    output: b.output,
    hitRate: m.cacheHitRate,
    requests: m.requests,
  };
}
