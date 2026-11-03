// 四页一级导航条（2026-10-26 计划 Task 7 导航链路 + Task 8 视觉定稿）。
// 顺序固定 = `WORKSPACE_TABS`（消息看板 / 工具资源 / 模型用量 / 输出速度）。
//
// 视觉（spec §3）：导航是**轻玻璃**面（.workspace-glass，内容面板禁用）；
// 活动页签 = aria-current="page" + data-active 状态属性 + 强调点 + 字重——
// 选中不只靠颜色区别（spec §3.1）；支持 reduced-transparency / 无 blur 时实心回退。
//
// 点击**也走 openWorkspaceTab**（不是直接 set store）：本窗选择与跨窗请求同一条
// revision 排序链路——较早的跨窗目标不可能覆盖较晚的本窗点击，store 只认最新 revision。
import { useAppTranslation } from "@/hooks/use-app-translation";
import { useGlassFallback } from "@/hooks/use-glass-fallback";
import { openWorkspaceTab } from "@/lib/workspaceNavigation";
import { useWorkspaceStore } from "@/stores/workspaceStore";
import { WORKSPACE_TABS, type WorkspaceTab } from "@/types/workspace";
import { cn } from "@/lib/utils";

export function WorkspaceTabs() {
  const { t } = useAppTranslation();
  const activeTab = useWorkspaceStore((s) => s.activeTab);
  // 玻璃降级原因（减少透明度 / 不支持 blur 时挂 data-glass-fallback ⇒ CSS 退实心）
  const glassFallback = useGlassFallback();

  const labels: Record<WorkspaceTab, string> = {
    messages: t("workspace.tab.messages"),
    resources: t("workspace.tab.resources"),
    usage: t("workspace.tab.usage"),
    speed: t("workspace.tab.speed"),
  };

  return (
    <nav
      aria-label={t("workspace.nav.label")}
      data-glass-fallback={glassFallback === "glass" ? undefined : glassFallback}
      className="workspace-glass flex items-center gap-1 rounded-lg border p-1"
    >
      {WORKSPACE_TABS.map((tab) => {
        const active = activeTab === tab;
        return (
          <button
            key={tab}
            type="button"
            data-testid={`workspace-tab-${tab}`}
            aria-current={active ? "page" : undefined}
            data-active={active ? "true" : "false"}
            onClick={() => void openWorkspaceTab(tab)}
            className={cn(
              "focus-visible:ring-ring/50 flex items-center gap-1.5 rounded-md px-3 py-1 text-sm transition-colors outline-none focus-visible:ring-2",
              active
                ? "bg-selected text-selected-foreground font-semibold"
                : "text-muted-foreground hover:bg-accent/50 hover:text-foreground font-medium"
            )}
          >
            {/* 非颜色选中指示：强调点仅活动页签着色（未选中占位透明，布局稳定） */}
            <span
              aria-hidden="true"
              className={cn("size-1.5 rounded-full", active ? "bg-primary" : "bg-transparent")}
            />
            {labels[tab]}
          </button>
        );
      })}
    </nav>
  );
}
