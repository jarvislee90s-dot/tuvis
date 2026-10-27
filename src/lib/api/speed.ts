// V2 速度 dashboard IPC 唯一入口（Task 9；契约 `tps_speed_dashboard(query)`，Task 6 落地）。
// **只读**：查询绝不触发源扫描、模型请求或采集——真实刷新走既有采集器节奏
// （可见 5 秒有界读取 / 后台 600 秒，`tps_refresh` 限频单飞是用户动作，不归本查询）。
// 命令字符串只落在本文件（lib/api 层），查询层与组件一律经 `speedDashboard` 调用。
import { invoke } from "@tauri-apps/api/core";
import type { SpeedDashboardQuery, SpeedDashboardSnapshot } from "@/types/speed";

/** V2 速度 dashboard（区间 series 汇总 + 选中 series 桶聚合 + 来源状态，同一事实快照）。 */
export async function speedDashboard(query: SpeedDashboardQuery): Promise<SpeedDashboardSnapshot> {
  return await invoke<SpeedDashboardSnapshot>("tps_speed_dashboard", { query });
}
