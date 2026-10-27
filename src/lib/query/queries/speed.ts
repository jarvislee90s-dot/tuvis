// V2 速度查询数据层（Task 9）——键前缀恒 `["tps", "v2"]`（沿用 TPS 域前缀 + v2 代际位），
// **绝不**与 Token/费用域共享任何键或区间状态（两套档位语义不同：15m/1h/5h/24h vs 旧 TPS 视图档）。
//
// 可见性纪律（与 tps.ts 同款，Task 7 门控口径）：本查询**只读已采集事实**，绝不触发
// 源扫描 / 模型请求 / 采集刷新——真实数据刷新由后端采集节奏（可见 5 秒有界读取）承担，
// 前端仅按可见节奏重读同一事实快照。命令字符串只存在于 lib/api 层（本文件不出现），
// 也不得引用旧用量轮询静态锁扫及的任何采集命令字面量。
//
// active 门控（Task 9 契约，`opts` 必填）：`active: true` ⇒ 拉取 + 可见节奏 5 秒轮询；
// `active: false`（页签隐藏）⇒ `enabled: false`——不发首查、不轮询、聚焦（focus
// refetch）也不发；翻回 active 即恢复（enabled false→true 重发过期查询）。
import { useQuery, type UseQueryResult } from "@tanstack/react-query";
import { speedDashboard } from "@/lib/api/speed";
import { seriesKeyId } from "@/lib/speed/view";
import { TPS_KEY } from "@/lib/query/queries/tps";
import type { SpeedDashboardQuery, SpeedDashboardSnapshot } from "@/types/speed";

/** V2 速度域 queryKey 前缀（TPS 域 + v2 代际位；失效旧 TPS 前缀可同时覆盖本域）。 */
export const SPEED_KEY = [...TPS_KEY, "v2"] as const;

/** 可见时 dashboard 轮询间隔（Spec §7「新页面可见时5秒刷新同一事实快照」）。 */
export const SPEED_DASHBOARD_INTERVAL_MS = 5_000;

/**
 * dashboard 查询键（稳定编码）：selection 经 `seriesKeyId` 规范化（channel 显式 null 化），
 * 其余字段显式 null 化后原样进键——六个字段（range/selection/metricMode/toolId/search/sort）
 * 任一变化都产生不同键（缓存隔离），同查询跨渲染键逐字稳定。
 */
export function speedDashboardKey(query: SpeedDashboardQuery) {
  return [
    ...SPEED_KEY,
    "dashboard",
    {
      range: query.range,
      selection: query.selection ? seriesKeyId(query.selection) : null,
      metricMode: query.metricMode,
      toolId: query.toolId ?? null,
      search: query.search,
      sort: query.sort,
    },
  ] as const;
}

/**
 * V2 速度 dashboard 查询（只读）。
 * - `opts.active === true`：拉取 + 每 5 秒轮询；
 * - `opts.active === false`：`enabled: false`（零请求：无首查、无轮询、无聚焦重查；数据保留）。
 * **`opts` 必填（类型层强制）**：调用方必须显式表态 active 与否——可选+默认 true 的失败
 * 模式是「调用方漏传 ⇒ 隐藏页签仍在 5 秒轮询」（与 tps.ts 的必填 `visible` 同款纪律）。
 * 本查询路径**不得**出现任何采集/刷新调用——刷新是采集器与用户动作（既有单飞限频）的事。
 */
export function useSpeedDashboardQuery(
  query: SpeedDashboardQuery,
  opts: { active: boolean }
): UseQueryResult<SpeedDashboardSnapshot> {
  const active = opts.active;
  return useQuery<SpeedDashboardSnapshot>({
    queryKey: speedDashboardKey(query),
    queryFn: () => speedDashboard(query),
    refetchInterval: active ? SPEED_DASHBOARD_INTERVAL_MS : false,
    enabled: active,
  });
}
