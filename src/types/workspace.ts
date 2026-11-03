// 四页工作区的公共类型（2026-10-26 计划 Task 7；公共契约「导航」条的 TS 镜像）。
//
// 线格式与 Rust `services/workspace_navigation.rs` 逐字对齐：
//  * `WorkspaceTab` kebab-case：messages | resources | usage | speed；
//  * `WorkspaceNavigation` camelCase：{ revision, tab }。

/** 四个一级页面，固定顺序（消息看板 / 工具资源 / 模型用量 / 输出速度） */
export type WorkspaceTab = "messages" | "resources" | "usage" | "speed";

/** 固定顺序的四页元组（导航条渲染顺序的唯一出处） */
export const WORKSPACE_TABS = ["messages", "resources", "usage", "speed"] as const;

/** 一次跨窗导航目标：revision 单调递增，前端只应用比已应用更新（更大）的目标 */
export interface WorkspaceNavigation {
  revision: number;
  tab: WorkspaceTab;
}
