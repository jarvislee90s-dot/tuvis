// TPS 短时抽样 IPC 唯一入口（Task 8）——七命令包装（Spec §6 逐字）。
// 全部**只读或显式用户动作**：snapshot/points 查询绝不触发采集；refresh 是用户点击的
// manual 读取；install 族是一次配置流；heartbeat 是视图租约（30 秒一拍）。
import { invoke } from "@tauri-apps/api/core";
import type {
  TpsHeartbeatResult,
  TpsInstallChoice,
  TpsInstallPreview,
  TpsInstallReceipt,
  TpsPointsResult,
  TpsRefreshResult,
  TpsSelection,
  TpsSnapshotResult,
  TpsViewRange,
} from "@/types/tps";

/** 快照（近 24h 事实的最近调用/当前回合 + 真实来源状态；总授权关闭 → disabled）。 */
export async function tpsSnapshot(selection: TpsSelection): Promise<TpsSnapshotResult> {
  return await invoke<TpsSnapshotResult>("tps_snapshot", { selection });
}

/** 曲线（view_range 尾查；runId/trigger/status 供前端断线；查询不触发采集）。 */
export async function tpsPoints(
  selection: TpsSelection,
  viewRange: TpsViewRange
): Promise<TpsPointsResult> {
  return await invoke<TpsPointsResult>("tps_points", { selection, viewRange });
}

/** 手动刷新（立即读取 + manual 点；最小间隔 5 秒，限频时 refreshed=false + reason）。 */
export async function tpsRefresh(selection: TpsSelection): Promise<TpsRefreshResult> {
  return await invoke<TpsRefreshResult>("tps_refresh", { selection });
}

/** 视图心跳（窗口与 TPS 区块实际可见才传 visible=true；30 秒一拍，租约 90 秒）。 */
export async function tpsViewHeartbeat(
  viewId: string,
  visible: boolean
): Promise<TpsHeartbeatResult> {
  return await invoke<TpsHeartbeatResult>("tps_view_heartbeat", { viewId, visible });
}

/** 安装预览（只读设置文件 + 端口真实探测；敏感统计 header 只给脱敏值/指纹）。 */
export async function tpsInstallPreview(tool: string): Promise<TpsInstallPreview> {
  return await invoke<TpsInstallPreview>("tps_install_preview", { tool });
}

/** 应用预览（指纹漂移拒绝；错误信封带 reason：conflict / stale-preview / …）。 */
export async function tpsInstallApply(
  previewId: string,
  choice: TpsInstallChoice
): Promise<TpsInstallReceipt> {
  return await invoke<TpsInstallReceipt>("tps_install_apply", { previewId, choice });
}

/** 卸载自有统计项（仍匹配安装值才处置；用户后改项保留并报冲突）。 */
export async function tpsUninstall(tool: string): Promise<void> {
  return await invoke<void>("tps_uninstall", { tool });
}
