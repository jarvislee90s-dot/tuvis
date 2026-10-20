// V2 分布纯层（Task 12）——C 卡 Top5 与排序/占比的唯一出处。
//
// 与旧 `distribution.ts::distributionRows` 同口径：过滤正行、按 hero 降序、share 为 0–1 分数
// （下限 `DIST_MIN_BAR_PCT / 100`）。**Top5 独立**：旧固定分享的 `DIST_MAX_ROWS=10` 保持不变，
// 本文件只服务看板 C 卡。
import { DIST_MIN_BAR_PCT, distValue } from "@/lib/usage/distribution";
import type { UsageSourceId } from "@/types/usage";
import type { AttributionKind, ChannelKind, DistributionRow } from "@/types/usageV2";

/** C 卡默认可见行数；「显示更多」展开剩余行，与固定分享的 10 行上限互不影响。 */
export const DIST_TOP_ROWS = 5;

export interface DistributionV2Row {
  key: string;
  label: string;
  /** hero 口径：`requestTotal + output` */
  value: number;
  /** 0–1 分数，已夹到 `DIST_MIN_BAR_PCT / 100` 下限与 1 上限 */
  share: number;
  /** 0–1 分数；不可得为 `null`（组件出 `—`，绝不填 0） */
  hitRate: number | null;
  billingProfileId: string | null;
  attributionKind: AttributionKind;
  sourceIds: readonly UsageSourceId[];
  /** 渠道稳定键；tool/project 维度为 null。 */
  channelKind: ChannelKind | null;
  /** 展示用渠道名（别名映射后的显示名）。 */
  displayProvider: string | null;
  /** 账本原始 provider（证据面，仅供 title/tooltip）。 */
  observedProvider: string | null;
  /** 该分组内全部去重后的非空原始 provider（title/tooltip 的完整证据面）。 */
  observedProviders: readonly string[];
  /** 该分组是否混入未标注来源（原始 provider 为空）的行。 */
  hasEmptyProvider: boolean;
  /** 原始模型 ID（空串 = 未知模型）；tool/project/渠道维度为 null。 */
  modelId: string | null;
}

function hitRateOf(row: DistributionRow): number | null {
  const v = row.metrics?.cacheHitRate;
  if (typeof v !== "number" || !Number.isFinite(v)) return null;
  return Math.min(1, Math.max(0, v));
}

/**
 * 全部正值行（降序）。`null` / `undefined` / 空数组 → `[]`；零值、负值、非有限值直接丢弃
 * （零值不成行，空态由调用方渲染「暂无数据」）。
 */
export function distributionV2Rows(
  rows: readonly DistributionRow[] | null | undefined
): DistributionV2Row[] {
  const candidates = (rows ?? [])
    .map((row) => ({
      key: row.key,
      label: row.label,
      value: distValue(row),
      hitRate: hitRateOf(row),
      billingProfileId: row.billingProfileId,
      attributionKind: row.attributionKind,
      sourceIds: row.sourceIds,
      channelKind: row.channelKind ?? null,
      displayProvider: row.displayProvider ?? null,
      observedProvider: row.observedProvider ?? null,
      observedProviders: row.observedProviders ?? [],
      hasEmptyProvider: row.hasEmptyProvider ?? false,
      modelId: row.modelId ?? null,
    }))
    .filter((row) => row.value > 0)
    .sort((a, b) => b.value - a.value);

  const max = candidates[0]?.value ?? 0;
  return candidates.map((row) => ({
    ...row,
    share:
      max > 0
        ? Math.min(1, Math.max(DIST_MIN_BAR_PCT / 100, row.value / max))
        : DIST_MIN_BAR_PCT / 100,
  }));
}

/** 折叠视图：未展开时前 `DIST_TOP_ROWS` 行 + 被折叠行数；展开时全量。 */
export function distributionV2View(
  all: readonly DistributionV2Row[],
  expanded: boolean
): { rows: DistributionV2Row[]; moreCount: number } {
  if (expanded) return { rows: [...all], moreCount: 0 };
  return {
    rows: all.slice(0, DIST_TOP_ROWS),
    moreCount: Math.max(0, all.length - DIST_TOP_ROWS),
  };
}
