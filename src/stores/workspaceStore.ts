// 四页工作区的页签状态（2026-10-26 计划 Task 7）。
//
// 为什么用 Zustand 而不是 home.tsx 的 useState：窗口显示/隐藏（关窗即 hide）**
// 不能**重置当前页面（spec §2.1），store 与应用同生命周期即可满足——本 store 刻意
// 不做持久化（重启回到默认消息看板，不恢复上次的页签）。
//
// revision 裁决（公共契约）：`applyNavigation` 只应用比 `appliedRevision` 更新
// （严格更大）的目标——事件与 peek 乱序到达、过期回放都不能把页面往回挪。
// 本窗页签点击也走同一请求链路（openWorkspaceTab → Rust 队列 → 事件回流），
// 因此「较早的跨窗目标覆盖较晚的本窗选择」在结构上不可能发生。
import { create } from "zustand";
import type { WorkspaceNavigation, WorkspaceTab } from "@/types/workspace";

interface WorkspaceStore {
  /** 当前一级页面；首启恒为消息看板 */
  activeTab: WorkspaceTab;
  /** 本次运行中访问过的页面（按首访顺序）：宿主按它决定页面主体是否已挂载 */
  visitedTabs: WorkspaceTab[];
  /** 已应用的最新导航 revision；0 = 尚未应用任何导航 */
  appliedRevision: number;
  /** 应用一条导航目标（revision 单调；旧目标忽略） */
  applyNavigation: (nav: WorkspaceNavigation) => void;
}

export const useWorkspaceStore = create<WorkspaceStore>((set) => ({
  activeTab: "messages",
  visitedTabs: ["messages"],
  appliedRevision: 0,
  applyNavigation: (nav) =>
    set((state) => {
      // 乱序/过期目标（事件重放、peek 与事件竞争）：不回退
      if (nav.revision <= state.appliedRevision) return state;
      return {
        activeTab: nav.tab,
        visitedTabs: state.visitedTabs.includes(nav.tab)
          ? state.visitedTabs
          : [...state.visitedTabs, nav.tab],
        appliedRevision: nav.revision,
      };
    }),
}));
