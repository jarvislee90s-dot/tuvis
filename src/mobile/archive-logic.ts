// 历史会话过滤纯函数（spec §7.2）：工具 × 项目双维。与 board-logic.ts 同风格：
// 不改入参。相对时间与状态显示名**复用** board-logic 既有导出
// （formatRelativeTime / toolLabel），本文件不自造。
import i18n from "@/i18n";
import { toolLabel } from "./board-logic";

export type ArchiveToolFilter = string; // "all" 或工具 id

/** 工具 id → 显示名（toolLabel 以 string 索引安全读，未知回退原文；"all" →
 *  mobile.archive.toolAll）。ArchiveBoard chips 与 ArchiveDetail 信息行共用——信息行
 *  不再裸显 agentType 原文（评审 Minor）。M5 Task 7：全量经 i18n 渲染期现取。 */
export function chipLabel(t: string): string {
  if (t === "all") return i18n.t("mobile.archive.toolAll");
  return toolLabel(t);
}

export function filterArchivedByTool<T extends { agentType: string }>(
  rows: T[],
  filter: ArchiveToolFilter
): T[] {
  if (filter === "all") return rows;
  return rows.filter((r) => r.agentType === filter);
}

export function filterArchivedByProject<T extends { projectName: string }>(
  rows: T[],
  project: string
): T[] {
  if (project === "all") return rows;
  return rows.filter((r) => r.projectName === project);
}
