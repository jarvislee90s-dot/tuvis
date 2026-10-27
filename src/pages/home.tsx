// 四页宿主（2026-10-26 计划 Task 7）：主窗口唯一页面，承载 消息看板 / 工具资源 /
// 模型用量 / 输出速度 四个一级页签（spec §2.1 固定同序、首启落消息看板）。
//
// 宿主职责（切页绝不重复/停止）：
//  * useSessions / useNotification / Toaster / UpdaterDialog / PetStartupGuard /
//    遗留技能迁移 / 托盘菜单初始化——**应用层**，只在宿主挂载一次（spec §2.7）；
//  * useWorkspaceNavigation——跨窗导航目标的事件/peek 消费，同样只挂载一次；
//  * 页面主体**首访才挂载，之后保留**：非活动页隐藏在四页同格堆叠容器里
//    （`visibility:hidden` + `inert`，非 display:none——保盒树 ⇒ 保滚动位置，spec §2.6）；
//    各页体以 active 门控隐藏期间的查询/轮询/采集（用量页全部读 + 兜底 ticker、
//    速度页 dashboard 轮询与 TPS 视图租约），隐藏期间零 IPC（spec §2.7）。
// 旧的「看板/资源」内部页签由四页顶层页签取代（会话摘要 / 工具 chips / 一键清除
// 只属消息看板页）。
import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { WindowFrame } from "@/components/common/window-frame";
import { MainTitleBar } from "@/components/common/main-title-bar";
import { WorkspaceTabs } from "@/components/common/WorkspaceTabs";
import { WorkspacePageHeader } from "@/components/common/WorkspacePageHeader";
import { UpdaterDialog } from "@/components/common/updater-dialog";
import { Toaster } from "@/components/ui/sonner";
import { SessionGrid } from "@/components/sessions/SessionGrid";
import { ExtensionList } from "@/components/resources/ExtensionList";
import { UsageDashboardBody } from "@/components/usage/UsageDashboardBody";
import { SpeedDashboard } from "@/components/speed/SpeedDashboard";
import { CheckCheck, Activity, AlertCircle } from "lucide-react";
import { useQueryClient } from "@tanstack/react-query";
import { useSessions } from "@/hooks/useSessions";
import { useNotification } from "@/hooks/useNotification";
import { useWorkspaceNavigation } from "@/hooks/useWorkspaceNavigation";
import { useSessionStore } from "@/stores/sessionStore";
import { useWorkspaceStore } from "@/stores/workspaceStore";
import { loadVisible, subscribeConfig } from "@/components/pet/petConfig";
import { PetStartupGuard } from "@/components/pet/PetStartupGuard";
import { useAppTranslation } from "@/hooks/use-app-translation";
import { NotificationBell } from "@/components/notifications/NotificationBell";
import { useLegacySkillMigration } from "@/hooks/useLegacySkillMigration";
import { LegacySkillMigrationDialog } from "@/components/resources/LegacySkillMigrationDialog";
import { ToolFilterChips } from "@/components/sessions/ToolFilterChips";
import { AGENT_BADGE } from "@/lib/agentBadge";
import { filterByAgent, loadToolFilter, saveToolFilter, type ToolFilter } from "@/lib/toolFilter";
import type { WorkspaceTab } from "@/types/workspace";

export default function HomePage() {
  useSessions();
  useNotification();
  // 跨窗导航消费（Task 7）：事件 + 订阅后/唤起时 peek，挂载恰一次
  useWorkspaceNavigation();
  // 遗留 codex 技能链接检测（spec §4.3）：mount 即检测，命中 ≥1 条时弹一次性迁移对话框
  const legacyMigration = useLegacySkillMigration();
  const activeTab = useWorkspaceStore((s) => s.activeTab);
  const visitedTabs = useWorkspaceStore((s) => s.visitedTabs);
  const { sessions, totalCount, waitingCount, loading } = useSessionStore();
  const { t, i18n } = useAppTranslation();
  const queryClient = useQueryClient();

  // 按工具筛选（2026-10-11）：单选 chips + localStorage 记忆（重启保持上次选择，
  // 首次默认「全部」）；纯筛选不动排序，计数条保持全量口径（与移动端 Board 一致）
  const [toolFilter, setToolFilterState] = useState<ToolFilter>(() => loadToolFilter());
  const setToolFilter = (filter: ToolFilter) => {
    setToolFilterState(filter);
    saveToolFilter(filter);
  };
  const filteredSessions = filterByAgent(sessions, toolFilter);
  const filterToolLabel = toolFilter === "all" ? undefined : AGENT_BADGE[toolFilter]?.label;

  // 一键清除已完成卡片（绿/空闲）：对每张绿卡复用单卡 X 的同款语义——
  // 未读卡标已读（mark_session_read），其余 dismiss（dismiss_session_card）；
  // 全部完成后立即失效轮询缓存，不等 3 秒轮询自然生效
  const finishedCards = sessions.filter((s) => s.status === "idle" || s.status === "finished");
  const clearFinishedCards = async () => {
    await Promise.allSettled(
      finishedCards.map((s) =>
        s.unread
          ? invoke("mark_session_read", { agentType: s.agentType, sessionId: s.id })
          : invoke("dismiss_session_card", {
              agentType: s.agentType,
              sessionId: s.id,
              status: s.status,
            })
      )
    );
    queryClient.invalidateQueries({ queryKey: ["sessions"] });
  };

  // 桌宠开关状态：与 petConfig 双向同步（标题栏开关/设置页/托盘改动经订阅回流）；
  // 开关本体在 MainTitleBar，此处状态仅供托盘菜单文案
  const [petOn, setPetOn] = useState(() => loadVisible());
  useEffect(() => subscribeConfig(() => setPetOn(loadVisible())), []);

  useEffect(() => {
    const initTrayMenu = async () => {
      try {
        // Task 16 统一重建：基础项 + 预设项一次成型（原 update_tray_menu 会丢预设项）
        await invoke("refresh_tray", {
          presetsLabel: t("tray.presetsLabel"),
          showText: t("tray.show"),
          petText: petOn ? t("tray.petHide") : t("tray.petShow"),
          quitText: t("tray.quit"),
          remoteOnText: t("tray.remote"), // M4 T4：远程开关项标签（勾选态 Rust 侧自查）
          autostartText: t("tray.autostart"), // 2026-10-11：开机自启开关项标签（勾选态 Rust 侧自查）
        });
      } catch (error) {
        console.error("Failed to initialize tray menu:", error);
      }
      // 三终端语言自适应（C-3）：桌面当前语言推给 Rust 内存源（远程端连接时经 /host
      // 下发跟随）。与托盘重建同路触发：启动 i18n 初始化后本 effect 首跑 + 每次语言
      // 切换（t 变化即重跑，覆盖标题栏/设置页/托盘跨窗口的切换回流）；Rust 只存不判
      try {
        await invoke("set_desktop_language", { language: i18n.language });
      } catch (error) {
        console.error("Failed to push desktop language:", error);
      }
    };
    initTrayMenu();

    // M4 T4：远程状态变化（设置页/托盘开关/通道切换）→ 重建托盘刷新勾选态与地址。
    // 监听放本组件：持有 petOn 与真实键名 tray.petHide/petShow（放 useRemoteEvents
    // 会丢桌宠态、重建出错误标签）；勾选态不经 setState，托盘重建即回环刷新
    const unlistenRemoteChanged = listen("remote-changed", () => void initTrayMenu());

    return () => {
      unlistenRemoteChanged.then((fn) => fn());
    };
    // petOn 变化时重跑本 effect，托盘桌宠文案随开关/语言刷新；
    // i18n.language 变化时重跑（语言切换 → 托盘文案 + C-3 语言推送一并刷新）
  }, [t, petOn, i18n.language]);

  // 页面容器（评审 Important 1 修正）：四页**堆叠在同一个 grid 单元格**，非活动页
  // `invisible pointer-events-none` + `aria-hidden` + `inert`。刻意**不用** `hidden`
  // （display:none）——那会拆掉盒树，Chromium/Firefox/Safari 在重新显示时丢弃子树
  // 滚动偏移（spec §2.6「切页不重置」会破）；`visibility:hidden` 保留布局与滚动，
  // 同时把页面从 a11y 树与命中测试里摘掉，`inert` 再挡一层焦点落入。首访前不挂载
  // 主体，避免隐藏页在启动时白白发查询/采集（用量页另有 active 门控双保险）。
  const shown = "col-start-1 row-start-1 flex min-h-0 flex-col gap-3 overflow-hidden";
  const inactive =
    "col-start-1 row-start-1 flex min-h-0 flex-col gap-3 overflow-hidden invisible pointer-events-none";
  const pageClass = (tab: string) => (activeTab === tab ? shown : inactive);
  const pageActive = (tab: string) => activeTab === tab;
  const visited = (tab: WorkspaceTab) => visitedTabs.includes(tab);

  return (
    <WindowFrame
      titleBar={<MainTitleBar />}
      contentClassName="container mx-auto flex flex-1 flex-col gap-3 overflow-hidden p-4"
    >
      <UpdaterDialog />
      <Toaster />

      {/* 四页一级导航（Task 7；Task 8 重做视觉） */}
      <WorkspaceTabs />

      {/* 四页堆叠容器：单行网格（minmax(0,1fr) 撑满剩余高度），四页同格重叠 */}
      <div className="grid min-h-0 flex-1 grid-rows-[minmax(0,1fr)] overflow-hidden">
        {/* ── 消息看板：会话摘要 / 工具 chips / 清除已完成 / 通知铃 / 会话卡 ── */}
        <section
          className={pageClass("messages")}
          data-testid="workspace-page-messages"
          aria-hidden={!pageActive("messages") || undefined}
          inert={!pageActive("messages") || undefined}
        >
          {/* 统一页头（Task 8）：标题 + 今日摘要/操作收进 actions 槽 */}
          <WorkspacePageHeader
            title={t("workspace.page.messages")}
            actions={
              <>
                <div className="flex items-center gap-2">
                  <Activity className="text-muted-foreground h-4 w-4" />
                  <span className="text-sm font-semibold">{totalCount}</span>
                  <span className="text-muted-foreground text-xs">{t("home.sessions")}</span>
                </div>
                {waitingCount > 0 && (
                  <div className="flex items-center gap-2">
                    <AlertCircle className="h-4 w-4 text-red-600 dark:text-red-400" />
                    <span className="text-sm font-semibold text-red-600 dark:text-red-400">
                      {waitingCount}
                    </span>
                    <span className="text-muted-foreground text-xs">{t("home.waiting")}</span>
                  </div>
                )}
                {loading && (
                  <span className="text-muted-foreground text-xs">{t("home.loading")}</span>
                )}
                {/* 消息页专属操作（原内部页签栏右侧迁来）：一键清除已完成 + 通知铃 */}
                {finishedCards.length > 0 && (
                  <button
                    onClick={clearFinishedCards}
                    className="text-muted-foreground hover:bg-muted hover:text-foreground rounded p-1.5 transition-colors"
                    title={t("home.clearFinishedCards")}
                    aria-label={t("home.clearFinishedCards")}
                  >
                    <CheckCheck className="h-4 w-4" />
                  </button>
                )}
                <NotificationBell />
              </>
            }
          />

          <ToolFilterChips value={toolFilter} onChange={setToolFilter} />

          <div className="min-h-0 flex-1 overflow-y-auto">
            <SessionGrid sessions={filteredSessions} filterToolLabel={filterToolLabel} />
          </div>
        </section>

        {/* ── 工具资源 ── */}
        <section
          className={pageClass("resources")}
          data-testid="workspace-page-resources"
          aria-hidden={!pageActive("resources") || undefined}
          inert={!pageActive("resources") || undefined}
        >
          {visited("resources") && (
            <div className="min-h-0 flex-1 overflow-y-auto">
              <ExtensionList />
            </div>
          )}
        </section>

        {/* ── 模型用量：原独立窗口主体原样嵌入（Task 7 提取）；active 门控轮询/采集 ── */}
        <section
          className={pageClass("usage")}
          data-testid="workspace-page-usage"
          aria-hidden={!pageActive("usage") || undefined}
          inert={!pageActive("usage") || undefined}
        >
          {visited("usage") && <UsageDashboardBody active={activeTab === "usage"} />}
        </section>

        {/* ── 输出速度：Task 12 组合页（数据 Task 9 / 图 Task 10 / 采集管理 Task 11）── */}
        <section
          className={pageClass("speed")}
          data-testid="workspace-page-speed"
          aria-hidden={!pageActive("speed") || undefined}
          inert={!pageActive("speed") || undefined}
        >
          {/* 页体常驻挂载（同格堆叠隐藏保状态）；active 门控速度轮询与 TPS 视图租约
              （visibility:hidden 下 IO 判不出页签切换，必须与页签态合取） */}
          {visited("speed") && <SpeedDashboard active={activeTab === "speed"} />}
        </section>
      </div>

      {/* 启动校验弹窗（EP2）：外部宠物素材异常时主窗口确认，宠物窗口先行降级 */}
      <PetStartupGuard />
      {/* 遗留 codex 技能链接迁移对话框（spec §4.3）：仅检测命中时可见 */}
      <LegacySkillMigrationDialog migration={legacyMigration} />
    </WindowFrame>
  );
}
