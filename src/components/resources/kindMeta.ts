// 「按资源」卡片与资源大表弹窗共用的类型元数据与搜索判据。
// 为什么合一（F3 之后）：弹窗内也能改关键词了，卡片段头计数与弹窗 tab 计数/行过滤必须
// 永远同口径——两份副本一旦漂移，会出现「卡片写 3 项、弹窗里只有 2 行」这种没人拦得住的错。
import { Link2, Package, Plug, type LucideIcon } from "lucide-react";

export type ResourceKind = "skill" | "mcp" | "plugin";

/** 三段元数据（数组顺序即展示顺序）：图标 + 计数文案键，卡片段头与弹窗 tab 共用 */
export const KIND_META: { kind: ResourceKind; icon: LucideIcon; countKey: string }[] = [
  { kind: "skill", icon: Package, countKey: "resources.skillsCount" },
  { kind: "mcp", icon: Link2, countKey: "resources.mcpsCount" },
  { kind: "plugin", icon: Plug, countKey: "resources.pluginsCount" },
];

/** 搜索判据：名字或已启用工具名命中即保留（大小写不敏感；空词/纯空白 = 全量） */
export function matchesResourceSearch(
  r: { name: string; enabledTools: string[] },
  search: string
): boolean {
  const q = search.trim().toLowerCase();
  if (!q) return true;
  return [r.name, ...r.enabledTools].some((x) => x.toLowerCase().includes(q));
}
