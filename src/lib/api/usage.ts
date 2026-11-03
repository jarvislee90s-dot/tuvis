// 用量域 IPC 唯一入口（契约 §3 的 8 条命令 + 既有 reveal_dir；参数名逐字对齐契约，
// JS 侧一律传 camelCase 键，Tauri 自动映射到 Rust 的 snake_case 入参）。
//
// **只有 `usageCollect` 触发扫描**（契约 §3 要点 1 / 计划② §3 第 15 条）：
// `usage_dashboard` / `usage_records` / `usage_export_csv` 只读本地账本，绝不触发全量扫描；
// 且 `usageCollect` 不得进 3 秒会话轮询路径（有测试断言）。
//
// 错误形状（契约 §1）：6 条用量命令 reject 结构化 `{ code, detail }` → 渲染一律走
// `usageErrMsg(e, t)`；两条导出命令 reject 的是自由文本 `String` → 透传，**不得**走 `usageErrMsg`。
import { invoke } from "@tauri-apps/api/core";
import type {
  UsageCollectResult,
  UsageDashboard,
  UsageFilters,
  UsageGroupBy,
  UsageRange,
  UsageRecords,
  UsageRecordsGroupBy,
  UsageSettings,
  UsageSettingsPatch,
} from "@/types/usage";

/**
 * 触发一次采集（手动路径，**总是执行**）。`force=true` 绕过最小间隔窗口；`force=false` 且距上次
 * 采集小于 `collectIntervalMin` 时后端直接返回上次结果（不扫描）。并发调用由后端单飞复用同一次
 * 结果——前端不做二次节流。
 */
export async function usageCollect(force: boolean): Promise<UsageCollectResult> {
  return await invoke<UsageCollectResult>("usage_collect", { force });
}

/**
 * **可见性触发采集**（F7）：设置分区 / 大看板 / 浮窗可见时调用。
 *
 * 与手动路径的区别：后端有一把**跨所有 WebView** 的 60s 门闩——窗口内直接返回缓存结果
 * （零扫描），同时到达的调用合并为一次重型采集；全失败的扫描不写回窗口（下一次可立即重试）。
 * 无入参（不需要 `force`：可见性路径的「总是尝试」由门闩判定，而不是无条件重扫）。
 */
export async function usageCollectVisible(): Promise<UsageCollectResult> {
  return await invoke<UsageCollectResult>("usage_collect_visible");
}

/** 大看板数据（含 hero / 趋势 / 分组行 / 工作小结 / 可得性 / 环比）；只读账本，不扫描 */
export async function usageDashboard(
  range: UsageRange,
  groupBy: UsageGroupBy
): Promise<UsageDashboard> {
  return await invoke<UsageDashboard>("usage_dashboard", { range, groupBy });
}

/**
 * 记录页数据（每工具 / 每项目一张卡）。`groupBy` 收窄为 `UsageRecordsGroupBy`（只有 tool | project，
 * 卡片维度）：它决定**卡片**维度，卡内行恒为「供应商 / 模型」（D6/D7）；传 provider/model 会被
 * 后端判 `usage-groupby-invalid`，故类型层直接排除。
 */
export async function usageRecords(
  range: UsageRange,
  groupBy: UsageRecordsGroupBy,
  filters: UsageFilters
): Promise<UsageRecords> {
  return await invoke<UsageRecords>("usage_records", { range, groupBy, filters });
}

/** CSV 文本（不含 BOM：`.csv` 的 BOM 由后端落盘层 `with_bom_if_csv` 加，前端不得重复加）；只读账本 */
export async function usageExportCsv(
  range: UsageRange,
  groupBy: UsageGroupBy,
  filters: UsageFilters
): Promise<string> {
  return await invoke<string>("usage_export_csv", { range, groupBy, filters });
}

export async function usageGetSettings(): Promise<UsageSettings> {
  return await invoke<UsageSettings>("usage_get_settings");
}

/** 返回**合并后的完整 8 字段**设置（不是 patch 回显） */
export async function usageSetSettings(patch: UsageSettingsPatch): Promise<UsageSettings> {
  return await invoke<UsageSettings>("usage_set_settings", { patch });
}

/** 文本落盘（CSV 用）→ 返回落盘**绝对路径**；失败 reject 自由文本 */
export async function exportSaveText(name: string, content: string): Promise<string> {
  return await invoke<string>("export_save_text", { name, content });
}

/** 二进制落盘（分享图 PNG 用，base64 输入）→ 返回落盘**绝对路径**；失败 reject 自由文本 */
export async function exportSaveBytes(name: string, base64: string): Promise<string> {
  return await invoke<string>("export_save_bytes", { name, base64 });
}

/** 落盘后在文件管理器里定位该文件（复用既有命令，白名单只认 ~/.tuvis 与 ~/.agents）——传**文件路径** */
export async function revealDir(path: string): Promise<void> {
  return await invoke<void>("reveal_dir", { path });
}
