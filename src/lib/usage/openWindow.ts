// 用量详情的**唯一**打开入口（计划② Task 6 设立；2026-10-26 计划 Task 7 改语义）。
//
// Task 7 四页同窗后**不再创建** `usage-dashboard` 窗口：本函数保留导出签名
// （title/theme 参数兼容既有调用方：标题栏 / 宠物菜单 / 迷你用量条「详情」），
// 实际行为 = `openWorkspaceTab("usage")`——Rust 侧显示/聚焦既有主窗口并切到
// 「模型用量」页签（revision 排序的导航队列，spec §2.3「不再创建独立窗口」）。
//
// `USAGE_WINDOW_LABEL` / `USAGE_WINDOW_URL` 仅为兼容保留（capability 白名单与
// main.tsx pageMap 仍引用该 label/路由；label 留在 capability 无害——没有任何
// 代码路径再以它建窗）。
import { openWorkspaceTab } from "@/lib/workspaceNavigation";

/** 旧独立窗口 label。与 `src-tauri/capabilities/default.json` 的 `windows` 白名单逐字一致（兼容保留，不再建窗）。 */
export const USAGE_WINDOW_LABEL = "usage-dashboard";

/** 旧窗口 URL（= `src/main.tsx` pageMap 的 `/usage` 兼容路由，现为跳转适配页）。 */
export const USAGE_WINDOW_URL = "/usage";

/**
 * 打开用量详情（旧签名兼容）：`title`/`theme` 已无效果（主窗口的主题/标题由宿主自管），
 * 保留参数让既有调用方零改动。导航失败（主窗缺失等）向上抛——Rust 侧目标已入队，
 * 主窗口下一次 peek 会恢复。
 */
export async function openUsageDashboard(_opts: {
  title: string;
  theme?: "dark" | "light";
}): Promise<void> {
  await openWorkspaceTab("usage");
}
