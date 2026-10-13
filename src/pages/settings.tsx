import { SETTINGS_FIELD } from "@/components/settings/typography";
import {
  SETTINGS_BADGE,
  SETTINGS_CARD_TITLE,
  SETTINGS_PAGE_TITLE,
  SETTINGS_SUBTITLE,
} from "@/components/settings/typography";
import { useCallback, useEffect, useRef, useState } from "react";
import RemoteAppearanceSection from "@/components/settings/RemoteAppearanceSection";
import { useQueryClient } from "@tanstack/react-query";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { cn } from "@/lib/utils";
import { useTheme } from "@/components/common/theme-provider";
import { TitleBar } from "@/components/common/title-bar";
import { WindowFrame } from "@/components/common/window-frame";
import { LanguageToggle } from "@/components/common/language-toggle";
// 一级导航只有 6 个块，故**只保留 6 个块图标**（Palette / Bell / Wrench / BarChart3 /
// Smartphone / Database）+ 页内仍在用的功能图标。原先 12 个分区图标里
// `Keyboard` / `Dog` / `HeartPulse` / `RadioTower` / `ScrollText` / `Activity` 已成未使用导入
// （`noUnusedLocals` 会当场报错）——分区不再各自出现在侧栏，就没有「分区图标」这个概念了。
import {
  Moon,
  Sun,
  Monitor,
  Palette,
  Bell,
  Volume2,
  Database,
  Wrench,
  RefreshCw,
  Smartphone,
  BarChart3,
  type LucideIcon,
} from "lucide-react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { Switch } from "@/components/ui/switch";
import {
  loadVisible,
  saveVisible,
  loadConfig,
  saveConfig,
  subscribeConfig,
  PET_SCALES,
  type PetConfig,
} from "@/components/pet/petConfig";
import {
  SOUND_IDS,
  getSoundConfig,
  saveSoundConfig,
  playSound,
  type SoundConfig,
} from "@/lib/audio";
import { PetSwitchDialog } from "@/components/pet/manage/PetSwitchDialog";
import { PetImportDialog } from "@/components/pet/manage/PetImportDialog";
import { PetManageDialog } from "@/components/pet/manage/PetManageDialog";
import { loadActiveName } from "@/components/pet/petRuntime";
import { useEnabledToolsQuery } from "@/lib/query/queries/tools";
import { usePresetHealthQuery } from "@/lib/query/queries/health";
import { RemoteSection } from "@/components/settings/RemoteSection";
import { AuditLogSection } from "@/components/settings/AuditLogSection";
import { SignalHealthSection } from "@/components/settings/SignalHealthSection";
import { DataManagementSection } from "@/components/settings/DataManagementSection";
import { UsageStatusSection } from "@/components/settings/UsageStatusSection";
import { UsageSection } from "@/components/settings/UsageSection";
import { toast } from "sonner";
import { formatInvokeError } from "@/lib/invokeError";
import { ToolIcon } from "@/components/common/ToolIcon";
import { Toaster } from "@/components/ui/sonner";
import { useAppTranslation } from "@/hooks/use-app-translation";

// 一致性体检只读摘要（spec §13 设置页「立即体检」）：各源计数 + 前几条文本 + refetch。
// 与资源页 HealthCheckCard 共用 ["preset-health"] query key（设置窗口独立 WebView，各自取数）
function HealthSummary() {
  const { t } = useAppTranslation();
  const healthQuery = usePresetHealthQuery();
  const drift = healthQuery.data?.drift ?? [];
  const invariants = healthQuery.data?.invariants ?? [];
  const stashPending = healthQuery.data?.stashPending ?? [];
  const hasIssues = drift.length + invariants.length + stashPending.length > 0;
  // 漂移按 L1-L4 分组计数（复用 resources.health.L1-L4 文案，无新 key）
  const kindCounts = (["L1", "L2", "L3", "L4"] as const).map((kind) => ({
    kind,
    n: drift.filter((d) => d.kind === kind).length,
  }));
  // 前几条文本摘要（封顶 6 行防长列表；只读，不提供处置入口——处置在资源页卡片）
  const lines = [
    ...drift.slice(0, 3).map((d) => `${d.toolId} · ${d.extensionId} (${d.kind})`),
    ...invariants.slice(0, 2),
    ...stashPending.slice(0, 1).map((s) => `${s.skillName} → ${s.originalPath}`),
  ];
  return (
    <div className="space-y-3">
      <div className="flex items-center justify-between gap-2">
        <div className="flex min-w-0 flex-wrap items-center gap-1.5 text-xs">
          {hasIssues ? (
            <>
              {kindCounts
                .filter((k) => k.n > 0)
                .map((k) => (
                  <span key={k.kind} className="bg-muted rounded px-1.5 py-0.5">
                    {t(`resources.health.${k.kind}`)}: {k.n}
                  </span>
                ))}
              {invariants.length > 0 && (
                <span className="bg-muted rounded px-1.5 py-0.5">
                  {t("resources.health.invariantBroken")}: {invariants.length}
                </span>
              )}
              {stashPending.length > 0 && (
                <span className="bg-muted rounded px-1.5 py-0.5">
                  {t("resources.health.stashPending")}: {stashPending.length}
                </span>
              )}
            </>
          ) : (
            <span className="text-muted-foreground">{t("resources.health.ok")}</span>
          )}
        </div>
        <Button
          size="sm"
          variant="outline"
          onClick={() => void healthQuery.refetch()}
          disabled={healthQuery.isFetching}
        >
          <RefreshCw className={cn("mr-1 h-3 w-3", healthQuery.isFetching && "animate-spin")} />
          {t("resources.health.runNow")}
        </Button>
      </div>
      {hasIssues && lines.length > 0 && (
        <div className="divide-border divide-y rounded-md border">
          {lines.map((line, i) => (
            <div key={i} className="text-muted-foreground truncate px-3 py-1.5 text-xs">
              {line}
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

type SettingSection =
  | "appearance"
  // 2026-10-06 两级导航：远程端外观（皮肤）从 `appearance` 里拆出来独立成卡——
  // 它本来就是一个完整的折叠配置器（字体/皮肤/圆角/品牌色 + 预览），与「本机外观（主题+语言）」
  // 不是一件事。块 = 「外观与皮肤」，块内两张卡。
  | "skin"
  | "notifications"
  | "pet"
  // 开机自动启动（2026-10-11）：与通知/桌宠同属桌面行为块
  | "autostart"
  | "tools"
  | "health"
  | "signal"
  | "remote"
  | "audit"
  | "data"
  | "usage"
  | "usageStatus";

// 工具管理行（后端 ToolSetting，serde camelCase）
type ToolRow = {
  toolId: string;
  name: string;
  enabled: boolean;
  installed: boolean;
  managed: boolean;
};

/**
 * 一级导航 = **功能大块**（2026-10-06 用户裁决「按大的功能区块做一个切换，点进大块再分小卡片」）。
 * 每个分区（`SettingSection`）**恰好挂在一个块里**——有测试遍历它来防「重构漏挂一节 ⇒ 该分区
 * 在界面上永远看不见」。
 *
 * 分块依据（修的是三条具体缺陷）：
 *  * **同族不再被拆到两端**：`usage`（用量设置）与 `usageStatus`（采集状态/导出）原先隔了 7 项，
 *    现在同属「用量统计」块；`remote`/`audit` 同属「远程接入」块。
 *    （2026-10-13 用户裁决：`signal` 从「远程接入」挪入「工具与体检」——
 *    hook 健康度检查的对象是各 CLI 工具，与工具启停/一致性体检同块更合语义。）
 *  * **只读查看面与手动设置项分开**：审计/体检/信号/数据这些「平时就看一看」的面各自归到
 *    语义相邻的块里，不再与开关项平铺混列。
 *  * **皮肤有自己的位置**：远程端外观整节原本塞在「外观」里，现在与「本机外观」并列为两张卡。
 */
type SettingBlock = "appearance" | "desktop" | "tools" | "usage" | "remote" | "data";

const SETTINGS_BLOCKS: {
  id: SettingBlock;
  labelKey: string;
  descKey: string;
  icon: LucideIcon;
  sections: SettingSection[];
}[] = [
  // 块序（2026-10-11 用户裁决）：桌面（通知/桌宠/自启）→ 外观与皮肤 → 远程接入 →
  // 用量统计 → 工具与体检 → 数据管理
  {
    id: "desktop",
    labelKey: "settings.nav.desktop",
    descKey: "settings.nav.desktopDesc",
    icon: Bell,
    // 开机自动启动（2026-10-11）：软件级常规卡，与宠物卡内的「启动跟随」分卡分层
    sections: ["notifications", "pet", "autostart"],
  },
  {
    id: "appearance",
    labelKey: "settings.nav.appearance",
    descKey: "settings.nav.appearanceDesc",
    icon: Palette,
    sections: ["appearance", "skin"],
  },
  {
    id: "remote",
    labelKey: "settings.nav.remote",
    descKey: "settings.nav.remoteDesc",
    icon: Smartphone,
    sections: ["remote", "audit"],
  },
  {
    id: "usage",
    labelKey: "settings.nav.usage",
    descKey: "settings.nav.usageDesc",
    icon: BarChart3,
    sections: ["usage", "usageStatus"],
  },
  {
    id: "tools",
    labelKey: "settings.nav.tools",
    descKey: "settings.nav.toolsDesc",
    icon: Wrench,
    // signal（信号健康度）2026-10-13 用户裁决挪入：hook 通道自查与工具/体检同块
    sections: ["tools", "health", "signal"],
  },
  {
    id: "data",
    labelKey: "settings.nav.data",
    descKey: "settings.nav.dataDesc",
    icon: Database,
    sections: ["data"],
  },
];

export default function SettingsPage() {
  const [notificationsEnabled, setNotificationsEnabled] = useState(true);
  const [subagentReportEnabled, setSubagentReportEnabled] = useState(true);
  // 开机自动启动（2026-10-11）：真值源 = OS 侧插件状态（注册表/登录项，不落 KV
  // 镜像），加载时实时读；托盘翻转经 autostart-changed 事件回流同步。缺省关
  const [autostartEnabled, setAutostartEnabled] = useState(false);
  // 用户动作过路标（终审 Min-3）：置位后丢弃尚未 resolve 的初读结果——
  // 防挂载初读（毫秒级注册表读）在手速 toggle 之后 resolve 用陈旧值回退 UI
  const autostartTouchedRef = useRef(false);
  // F2b 子 agent 活动打标静默开关（终审发现 C）：默认 true=静默，与消费侧
  // （useNotification / 手机端默认静默）同一缺省语义
  const [silenceSubagentFlap, setSilenceSubagentFlap] = useState(true);
  const [soundConfig, setSoundConfig] = useState<SoundConfig>(() => getSoundConfig());
  // 一级导航状态 = 当前**块**。块内所有卡片同时渲染（用户裁决的形态），故不再有
  // 「当前分区」这一层状态 —— 删掉 `activeSection` 是刻意的：留着一个不参与渲染的旧状态，
  // 会造出「工具卡已脏但守卫不触发」的空档（见 `switchBlock`）。
  const [activeBlock, setActiveBlock] = useState<SettingBlock>("desktop");
  /** 当前块要渲染的 13 个分区集合（JSX 里 13 处守卫都读它；评审 n2 顺手修） */
  /** 当前块的元数据（侧栏与块标题共用同一张表；`??` 只为类型收窄，`activeBlock` 恒在表内） */
  const activeBlockMeta = SETTINGS_BLOCKS.find((b) => b.id === activeBlock) ?? SETTINGS_BLOCKS[0];
  const visibleIds = new Set(
    (SETTINGS_BLOCKS.find((b) => b.id === activeBlock) ?? SETTINGS_BLOCKS[0]).sections
  );
  // 桌宠状态：复用 petConfig（localStorage 单后端），跨窗口改动经 subscribeConfig 回流
  const [petVisible, setPetVisible] = useState(() => loadVisible());
  const [petCfg, setPetCfg] = useState(() => loadConfig());
  const [switchOpen, setSwitchOpen] = useState(false);
  const [importOpen, setImportOpen] = useState(false);
  const [manageOpen, setManageOpen] = useState(false);
  const [activePetName, setActivePetName] = useState(loadActiveName());
  // 工具管理状态（spec W5）：本地草稿 + 脏标记，保存时统一批量应用
  const [toolRows, setToolRows] = useState<ToolRow[]>([]);
  const [toolDirty, setToolDirty] = useState(false);
  const [confirmOpen, setConfirmOpen] = useState(false);
  // 未保存离开拦截：缓存「放弃更改并跳转」的回调
  const [leaveGuard, setLeaveGuard] = useState<null | (() => void)>(null);
  // 加载成功后的 enabled 快照（state 而非 ref：changedRows 渲染期 diff 需响应式读取）
  const [savedToolEnabled, setSavedToolEnabled] = useState<Record<string, boolean>>({});
  // 离开拦截选「保存」时缓存跳转，应用成功后执行
  const pendingJumpRef = useRef<(() => void) | null>(null);
  // 关窗守卫放行标记：三选（保存/放弃）尘埃落定后重发 close，此时不拦截（M1）
  const closeApprovedRef = useRef(false);
  const { t } = useAppTranslation();
  const { theme, setTheme } = useTheme();
  const queryClient = useQueryClient();
  // 启用工具列表（后端下发，勾选状态驱动；声音覆盖行随勾选增减）。
  // P2-8：查询未就绪/失败时用上一次数据或空占位，不渲染「全部停用」的瞬时误态
  const enabledToolsQuery = useEnabledToolsQuery();
  const enabledTools = enabledToolsQuery.data ?? [];

  // 更新音效配置并持久化。
  // ⚠️ 2026-10-07 D4 手术追记：本函数**被我删快捷键的那条正则误吞过**（`handleShowMainWindow`
  // 的正则没在 `}, []);` 处收住，一路吃到下一个 `};`），靠 eslint 的两条
  // `setSoundConfig/saveSoundConfig is never used` 才暴露；此处按 `git show HEAD:` 的原文恢复。
  const updateSound = (patch: Partial<SoundConfig>) => {
    const next = { ...soundConfig, ...patch };
    setSoundConfig(next);
    saveSoundConfig(next);
  };

  useEffect(() => {
    const loadNotificationSetting = async () => {
      try {
        const value = await invoke<string | null>("get_setting", { key: "notifications_enabled" });
        setNotificationsEnabled(value !== "false");
      } catch {
        // 忽略错误
      }
    };
    loadNotificationSetting();
  }, []);

  useEffect(() => {
    void (async () => {
      try {
        const v = await invoke<string | null>("get_setting", { key: "notify_subagent_report" });
        setSubagentReportEnabled(v !== "false");
      } catch {
        /* 缺省开 */
      }
    })();
  }, []);

  useEffect(() => {
    void (async () => {
      try {
        const v = await invoke<string | null>("get_setting", {
          key: "silence_subagent_activity_flap",
        });
        // 缺省/读取失败 = 静默（"true"），与 useNotification 运行时消费侧同口径
        setSilenceSubagentFlap(v !== "false");
      } catch {
        /* 缺省静默 */
      }
    })();
  }, []);

  // 开机自动启动（2026-10-11）：加载实时读 OS 侧真值；托盘翻转事件回流同步
  //（两窗口各自独立 WebView，K/V 不互通，靠后端广播对齐）
  useEffect(() => {
    void (async () => {
      try {
        const v = await invoke<boolean>("get_autostart_enabled");
        if (!autostartTouchedRef.current) setAutostartEnabled(v === true);
      } catch {
        /* 读取失败维持缺省关（不假开） */
      }
    })();
    const unlisten = listen<{ enabled: boolean }>("autostart-changed", (e) => {
      setAutostartEnabled(e.payload.enabled === true);
    });
    return () => {
      void unlisten.then((fn) => fn());
    };
  }, []);

  useEffect(
    () =>
      subscribeConfig(() => {
        setPetVisible(loadVisible());
        setPetCfg(loadConfig());
        setActivePetName(loadActiveName());
      }),
    []
  );

  const loadToolSettings = useCallback(async () => {
    try {
      // 兜底 null：浏览器/Playwright mock 下未注册命令会 resolve null（同 useEnabledToolsQuery
      // 的 `?? []` 防御），防止 toolRows.map / rows.map 崩溃
      const rows = (await invoke<ToolRow[]>("get_tool_settings")) ?? [];
      setToolRows(rows);
      // 记录快照，供 changedRows diff 与保存后复位
      setSavedToolEnabled(Object.fromEntries(rows.map((r) => [r.toolId, r.enabled])));
      setToolDirty(false);
    } catch (e) {
      console.error("get_tool_settings failed:", e);
    }
  }, []);

  // 进入工具分区时拉取勾选状态
  useEffect(() => {
    if (visibleIds.has("tools")) void loadToolSettings();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeBlock, loadToolSettings]);

  // 脏标记时拦截窗口关闭（P2-2）：Tauri 2 onCloseRequested → preventDefault 后走既有
  // 三选弹窗（保存 / 放弃更改 / 继续编辑）。浏览器/jsdom 下 getCurrentWindow 未实现
  // 或 onCloseRequested 不可用 → 防御性跳过（Tauri 外无真实窗口关闭语义）
  useEffect(() => {
    if (!toolDirty) return;
    let unlisten: (() => void) | undefined;
    let disposed = false;
    let win: ReturnType<typeof getCurrentWindow> | undefined;
    try {
      win = getCurrentWindow();
    } catch {
      return; // 非 Tauri 环境（浏览器/Playwright mock）：无窗口关闭事件可拦截
    }
    (async () => {
      const fn = await win.onCloseRequested((event) => {
        // M1：三选决议后重发的 close 不拦截，放行原生关闭
        if (closeApprovedRef.current) {
          closeApprovedRef.current = false;
          return;
        }
        event.preventDefault();
        // 关窗口与切分区共用同一三选守卫（保存缓存跳转 → 应用成功后执行）。
        // 守卫回调在三选决议（保存应用成功 / 放弃更改）后执行：清脏并重发 close，
        // 否则 preventDefault 已吞掉本次关闭，用户需再点一次 X
        setLeaveGuard(() => () => {
          setToolDirty(false);
          closeApprovedRef.current = true;
          // F-A：mock 环境（jsdom）的 window 对象无 close 方法 → 静默；
          // 放行标记已在上方置位，真实 Tauri 环境重发的 close 正常放行
          try {
            void win.close();
          } catch {
            /* mock 环境无 close：静默 */
          }
        });
      });
      if (disposed) fn();
      else unlisten = fn;
    })();
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [toolDirty]);

  // 开关点击仅改本地草稿（不立即生效），保存时批量应用
  const toggleTool = (toolId: string, next: boolean) => {
    setToolRows((rows) => rows.map((r) => (r.toolId === toolId ? { ...r, enabled: next } : r)));
    setToolDirty(true);
  };

  // 与快照 diff 出本次变更行
  const changedRows = toolRows.filter(
    (r) => r.enabled !== (savedToolEnabled[r.toolId] ?? r.enabled)
  );

  // 块切换守卫：工具卡有未保存更改时先弹三选拦截。
  // `visibleIds.has("tools")` = 「当前这一块里正显示着工具卡」——块内多卡并置后这比原来的
  // `activeSection === "tools"` 更准（工具卡可见 ⇒ 脏标记必须拦）。
  const switchBlock = (next: SettingBlock) => {
    if (next === activeBlock) return;
    if (visibleIds.has("tools") && toolDirty) {
      setLeaveGuard(() => () => {
        setToolDirty(false);
        setActiveBlock(next);
      });
      return;
    }
    setActiveBlock(next);
  };

  // 批量应用变更；成功后复位草稿、失效缓存并执行缓存跳转。
  // saving 态禁用确认按钮（review-2 Important 1：异步保存后双击会触发并发保存）
  const [toolSaving, setToolSaving] = useState(false);
  const applyChanges = async () => {
    if (toolSaving) return;
    setToolSaving(true);
    try {
      const result = await invoke<{
        restored: string[];
        restoredMcps: string[];
        rebuildFailed: string[];
        // issue #36-4：跳过项按现场分账——kept=链接保持不变，lost=现场缺失需重建
        skippedKept: string[];
        skippedLost: string[];
      }>("update_tool_settings", {
        changes: changedRows.map((r) => ({ toolId: r.toolId, enabled: r.enabled })),
      });
      toast.success(t("settings.tools.applied"));
      if (result.rebuildFailed.length) {
        toast.warning(
          t("settings.tools.rebuildFailed", { items: result.rebuildFailed.join(", ") })
        );
      }
      // SSOT 缺失/暂存失败的项逐项报告（spec W5 清理语义 1 + §9），不中断整体保存
      if (result.skippedKept.length) {
        toast.warning(
          t("settings.tools.skippedKeptItems", { items: result.skippedKept.join(", ") })
        );
      }
      // 现场已失（还原中断且链接恢复也失败）：更高级别提示，需重新勾选重建
      if (result.skippedLost.length) {
        toast.error(t("settings.tools.skippedLostItems", { items: result.skippedLost.join(", ") }));
      }
      setConfirmOpen(false);
      await loadToolSettings();
      // 全量失效本窗口（设置窗口）的 react-query 缓存；主窗口/看板的缓存由后端广播的
      // tools-changed 事件失效（toolsChangedSync，N2 根因修复）——两者是独立 WebView
      await queryClient.invalidateQueries();
      // 托盘同步（终审 Minor #5）：工具启停可能改动预设激活态，重建托盘菜单；失败静默
      // （只读传参，refresh_tray 内部已有持久化合并，与 PresetList 既有调用同款）
      invoke("refresh_tray", { presetsLabel: t("tray.presetsLabel") }).catch(() => {});
      const jump = pendingJumpRef.current;
      pendingJumpRef.current = null;
      jump?.();
    } catch (e) {
      toast.error(formatInvokeError(e, t));
    } finally {
      setToolSaving(false);
    }
  };

  const toggleNotifications = async () => {
    const newValue = !notificationsEnabled;
    setNotificationsEnabled(newValue);
    await invoke("set_setting", { key: "notifications_enabled", value: String(newValue) });
    toast.success(
      newValue
        ? t("settings.notifications.enabledToast")
        : t("settings.notifications.disabledToast")
    );
  };

  const toggleSubagentReport = async () => {
    const newValue = !subagentReportEnabled;
    setSubagentReportEnabled(newValue);
    await invoke("set_setting", { key: "notify_subagent_report", value: String(newValue) });
    toast.success(
      newValue
        ? t("settings.notifications.subagentReportOnToast")
        : t("settings.notifications.subagentReportOffToast")
    );
  };

  // F2b 静默开关（终审发现 C）：true=静默（value 落库 "true"/"false"，消费侧
  // `v !== "false"` 判静默，语义不倒挂——与 notify_subagent_report 的「true=提醒」相区分）
  const toggleSilenceSubagentFlap = async () => {
    const newValue = !silenceSubagentFlap;
    setSilenceSubagentFlap(newValue);
    await invoke("set_setting", {
      key: "silence_subagent_activity_flap",
      value: String(newValue),
    });
    toast.success(
      newValue
        ? t("settings.notifications.silenceSubagentFlapOnToast")
        : t("settings.notifications.silenceSubagentFlapOffToast")
    );
  };

  // 开机自动启动（2026-10-11）：乐观更新对齐既有开关手感；enable/disable 写
  // 注册表/LaunchAgent 可能被系统策略拦 → 失败回滚乐观态并如实报错
  const toggleAutostart = async (next: boolean) => {
    autostartTouchedRef.current = true;
    setAutostartEnabled(next);
    try {
      await invoke("set_autostart_enabled", { enabled: next });
      toast.success(next ? t("settings.autostart.onToast") : t("settings.autostart.offToast"));
    } catch (e) {
      setAutostartEnabled(!next);
      toast.error(formatInvokeError(e, t));
    }
  };

  // 桌宠显隐：写 localStorage + 同步 Rust 端窗口创建/销毁
  const onPetVisibleChange = async (v: boolean) => {
    saveVisible(v);
    setPetVisible(v);
    try {
      await invoke("set_pet_visible", { visible: v });
    } catch (e) {
      console.error("set_pet_visible failed:", e);
    }
    toast.success(v ? t("settings.pet.enabledToast") : t("settings.pet.disabledToast"));
  };

  // 桌宠配置：仅置顶变化需要同步 Rust 端 always-on-top
  const onPetCfgChange = (patch: Partial<PetConfig>) => {
    saveConfig(patch);
    setPetCfg(loadConfig());
    if (patch.alwaysOnTop !== undefined) {
      invoke("set_pet_always_on_top", { onTop: patch.alwaysOnTop }).catch(() => {});
    }
  };

  return (
    <WindowFrame
      titleBar={<TitleBar title={t("settings.title")} showMaximize={false} />}
      contentClassName="flex flex-1 overflow-hidden"
    >
      <Toaster />
      <aside className="border-border flex w-40 flex-col border-r p-4">
        <nav className="flex-1 space-y-1">
          {SETTINGS_BLOCKS.map((block) => {
            const Icon = block.icon;
            return (
              <button
                key={block.id}
                data-testid={`settings-nav-${block.id}`}
                onClick={() => switchBlock(block.id)}
                className={cn(
                  "flex w-full items-center gap-2 rounded-md px-3 py-2 text-sm transition-colors",
                  activeBlock === block.id
                    ? "bg-accent text-accent-foreground font-medium"
                    : "text-muted-foreground hover:bg-accent/50 hover:text-foreground"
                )}
              >
                <Icon className="h-4 w-4" />
                {t(block.labelKey)}
              </button>
            );
          })}
        </nav>
      </aside>

      <div className="flex-1 overflow-auto">
        <div className="max-w-3xl space-y-4 p-4">
          {/* 一级标题（2026-10-11 两级标题拉开）：左侧 primary 色条 + 大字块标题与说明行，
              与卡内灰底小字头条（SETTINGS_CARD_TITLE 在 bg-muted 条内）形成强对比 */}
          <div className="border-primary border-l-4 pl-3">
            <h2 className={SETTINGS_PAGE_TITLE}>{t(activeBlockMeta.labelKey)}</h2>
            <p className={SETTINGS_SUBTITLE}>{t(activeBlockMeta.descKey)}</p>
          </div>
          {/* 框一 · 桌面端外观（即点即生效，本期维持现状）——卡头条 = 二级卡头；
              一级是页顶色条大标题。2026-10-11：卡外同名 h2 取缔（同名双标题正是
              「两级分不清」的病灶） */}
          {visibleIds.has("appearance") && (
            <div className="overflow-hidden rounded-xl border">
              <div className="bg-muted/40 flex items-baseline gap-2 border-b px-4 py-2.5">
                <h2 className={SETTINGS_CARD_TITLE}>{t("settings.appearance.title")}</h2>
                <span className="text-muted-foreground text-xs">
                  {t("settings.remoteAppearance.desktopHint")}
                </span>
                <span className="ml-auto rounded-full bg-emerald-600/10 px-2 py-0.5 text-[11px] font-medium text-emerald-700 dark:text-emerald-400">
                  {t("settings.remoteAppearance.localOnly")}
                </span>
              </div>
              <div className="px-4 py-1">
                <div className="space-y-0">
                  <div className="flex items-center justify-between py-2.5">
                    <label className={SETTINGS_FIELD}>{t("settings.appearance.theme")}</label>
                    <div className="flex gap-2">
                      <Button
                        variant={theme === "light" ? "default" : "outline"}
                        size="sm"
                        onClick={() => setTheme("light")}
                        className="flex items-center gap-1.5"
                      >
                        <Sun className="h-3.5 w-3.5" />
                        {t("settings.appearance.light")}
                      </Button>
                      <Button
                        variant={theme === "dark" ? "default" : "outline"}
                        size="sm"
                        onClick={() => setTheme("dark")}
                        className="flex items-center gap-1.5"
                      >
                        <Moon className="h-3.5 w-3.5" />
                        {t("settings.appearance.dark")}
                      </Button>
                      <Button
                        variant={theme === "system" ? "default" : "outline"}
                        size="sm"
                        onClick={() => setTheme("system")}
                        className="flex items-center gap-1.5"
                      >
                        <Monitor className="h-3.5 w-3.5" />
                        {t("settings.appearance.system")}
                      </Button>
                    </div>
                  </div>

                  <div className="border-t" />

                  <div className="flex items-center justify-between py-2.5">
                    <label className={SETTINGS_FIELD}>{t("settings.appearance.language")}</label>
                    <LanguageToggle />
                  </div>
                </div>
              </div>
            </div>
          )}

          {/* 远程端外观（皮肤）：独立成卡（2026-10-06 两级导航）——桌面端外观 / 远程端外观
              两卡标题按 2026-10-11 裁决区分（本卡配置器折叠交互已移除，常展开）。 */}
          {visibleIds.has("skin") && <RemoteAppearanceSection />}

          {/* 快捷键分区**整节已移除**（2026-10-07 用户裁决 D4：UI 与功能一起去掉）。
              原来这里是一个 ShortcutInput（全局快捷键，默认 Cmd+Shift+M，用于唤起主窗口）。
              移除后：`lib/shortcut.ts`、`components/common/shortcut-input.tsx` 一并删除，
              设置块表里也不再有 `shortcut` 这一节。 */}

          {visibleIds.has("notifications") && (
            <div className="overflow-hidden rounded-xl border">
              {/* 卡头条（2026-10-11 两级标题拉开）：h2 保留档位常量搬进条内；说明收条右侧 */}
              <div className="bg-muted/40 flex items-baseline justify-between gap-2 border-b px-4 py-2.5">
                <h2 className={SETTINGS_CARD_TITLE}>{t("settings.notifications.heading")}</h2>
                <span className="text-muted-foreground truncate text-xs">
                  {t("settings.notifications.description")}
                </span>
              </div>
              <div className="px-4 py-1">
                <div className="space-y-0">
                  <div className="flex items-center justify-between py-2.5">
                    <div className="flex-1">
                      <label className={SETTINGS_FIELD}>
                        {t("settings.notifications.desktop")}
                      </label>
                      <p className="text-muted-foreground mt-0.5 text-xs">
                        {t("settings.notifications.desktopDesc")}
                      </p>
                    </div>
                    <Button
                      variant={notificationsEnabled ? "default" : "outline"}
                      size="sm"
                      onClick={toggleNotifications}
                    >
                      {notificationsEnabled
                        ? t("settings.notifications.on")
                        : t("settings.notifications.off")}
                    </Button>
                  </div>
                  <div className="border-t" />
                  <div className="flex items-center justify-between py-2.5">
                    <div className="flex-1">
                      <label className={SETTINGS_FIELD}>
                        {t("settings.notifications.subagentReport")}
                      </label>
                      <p className="text-muted-foreground mt-0.5 text-xs">
                        {t("settings.notifications.subagentReportDesc")}
                      </p>
                    </div>
                    <Button
                      variant={subagentReportEnabled ? "default" : "outline"}
                      size="sm"
                      onClick={toggleSubagentReport}
                    >
                      {subagentReportEnabled
                        ? t("settings.notifications.on")
                        : t("settings.notifications.off")}
                    </Button>
                  </div>
                  <div className="border-t" />
                  {/* F2b 进阶开关（终审发现 C）：子 agent 活动引发的状态跃迁默认静默 */}
                  <div className="flex items-center justify-between py-2.5">
                    <div className="flex-1">
                      <label className={SETTINGS_FIELD}>
                        {t("settings.notifications.silenceSubagentFlap")}
                      </label>
                      <p className="text-muted-foreground mt-0.5 text-xs">
                        {t("settings.notifications.silenceSubagentFlapDesc")}
                      </p>
                    </div>
                    <Button
                      variant={silenceSubagentFlap ? "default" : "outline"}
                      size="sm"
                      onClick={toggleSilenceSubagentFlap}
                    >
                      {silenceSubagentFlap
                        ? t("settings.notifications.on")
                        : t("settings.notifications.off")}
                    </Button>
                  </div>
                  <div className="border-t" />
                  <div className="space-y-3 py-2.5">
                    {/* 全局完成音：所有工具默认播放的音效 */}
                    <div className="flex items-center justify-between gap-2">
                      <label className={SETTINGS_FIELD}>
                        {t("settings.notifications.soundGlobalDefault")}
                      </label>
                      <div className="flex items-center gap-1.5">
                        <select
                          className="bg-background h-7 rounded border px-1.5 text-xs"
                          value={soundConfig.default}
                          onChange={(e) => updateSound({ default: e.currentTarget.value })}
                        >
                          {SOUND_IDS.map((id) => (
                            <option key={id} value={id}>
                              {id}
                            </option>
                          ))}
                          <option value="mute">{t("settings.notifications.soundMute")}</option>
                        </select>
                        {soundConfig.default !== "mute" && (
                          <Button
                            variant="outline"
                            size="sm"
                            onClick={() => playSound(soundConfig.default)}
                          >
                            <Volume2 className="mr-1 h-3 w-3" />
                            {t("settings.notifications.soundTest")}
                          </Button>
                        )}
                      </div>
                    </div>
                    {/* 工具专属音：覆盖全局默认，空值=跟随全局 */}
                    <label className={SETTINGS_FIELD}>
                      {t("settings.notifications.soundToolOverride")}
                    </label>
                    {enabledTools.map((tool) => {
                      // 后端工具 id 与 audio.ts 的 SoundConfig.tools 键同源（P2-9：均由
                      // AgentType 联合派生，tool.id 已具类型，无需 as keyof 强转）
                      const key = tool.id;
                      return (
                        <div key={key} className="flex items-center justify-between gap-2">
                          <span className="text-muted-foreground text-xs">{tool.label}</span>
                          <div className="flex items-center gap-1.5">
                            <select
                              className="bg-background h-7 rounded border px-1.5 text-xs"
                              value={soundConfig.tools[key] ?? ""}
                              onChange={(e) =>
                                updateSound({
                                  tools: {
                                    ...soundConfig.tools,
                                    [key]: e.currentTarget.value || undefined,
                                  },
                                })
                              }
                            >
                              <option value="">
                                {t("settings.notifications.soundFollowGlobal")}
                              </option>
                              {SOUND_IDS.map((id) => (
                                <option key={id} value={id}>
                                  {id}
                                </option>
                              ))}
                              <option value="mute">{t("settings.notifications.soundMute")}</option>
                            </select>
                            <Button
                              variant="outline"
                              size="sm"
                              onClick={() => {
                                // 试听：优先该工具当前生效音（未配置则回退全局）
                                const id = soundConfig.tools[key] || soundConfig.default;
                                if (id !== "mute") playSound(id);
                              }}
                            >
                              <Volume2 className="mr-1 h-3 w-3" />
                              {t("settings.notifications.soundTest")}
                            </Button>
                          </div>
                        </div>
                      );
                    })}
                  </div>
                  <div className="border-t" />
                  <div className="flex items-center justify-between py-2.5">
                    <div className="flex-1">
                      <label className={SETTINGS_FIELD}>
                        {t("settings.notifications.floatTest")}
                      </label>
                      <p className="text-muted-foreground mt-0.5 text-xs">
                        {t("settings.notifications.floatTestDesc")}
                      </p>
                    </div>
                    <Button
                      variant="outline"
                      size="sm"
                      onClick={async () => {
                        try {
                          // [Bug A 取证插桩·临时] 第二调用方（设置页浮窗试听按钮，手动触发）：
                          // 带 3 帧栈提示，用于与 useNotification 主路径对账（H1 排除用）
                          console.debug(
                            "[notif] SHOW_WINDOW(settings-test)",
                            new Error().stack?.split("\n").slice(0, 3).join(" | "),
                            Date.now()
                          );
                          await invoke("show_notification_window", {
                            payload: {
                              agentType: "claude",
                              agentLabel: "Claude",
                              projectName: t("settings.notifications.testProject"),
                              statusColor: "yellow",
                              status: "waiting",
                              lastMessage: t("settings.notifications.testMessage"),
                              title: "",
                              pid: 0,
                              sessionId: "test",
                            },
                          });
                        } catch (e) {
                          console.error("float preview failed:", e);
                        }
                      }}
                    >
                      <Bell className="mr-1.5 h-3.5 w-3.5" />
                      {t("settings.notifications.testFloat")}
                    </Button>
                  </div>
                </div>
              </div>
            </div>
          )}

          {visibleIds.has("pet") && (
            <div className="overflow-hidden rounded-xl border">
              {/* 卡头条（2026-10-11 两级标题拉开）：h2 保留档位常量搬进条内；说明收条右侧 */}
              <div className="bg-muted/40 flex items-baseline justify-between gap-2 border-b px-4 py-2.5">
                <h2 className={SETTINGS_CARD_TITLE}>{t("settings.pet.title")}</h2>
                <span className="text-muted-foreground truncate text-xs">
                  {t("settings.pet.desc")}
                </span>
              </div>
              <div className="px-4 py-1">
                <div className="space-y-0">
                  {/* 开启开关：显隐同步 Rust 端创建/销毁宠物窗口 */}
                  <div className="flex items-center justify-between py-2.5">
                    <div className="flex-1">
                      <label className={SETTINGS_FIELD}>{t("settings.pet.enable")}</label>
                    </div>
                    <Switch checked={petVisible} onCheckedChange={onPetVisibleChange} />
                  </div>
                  <div className="border-t" />
                  {/* 启动跟随开关（2026-10-11 用户裁决）：开 = 软件每次启动自动跟出（哪怕
                    上次隐身——显隐开关只管本会话中途）；关 = 启动后先不出，托盘/标题栏
                    随时可开。未设置过 = 默认开（petConfig 缺省位） */}
                  <div className="flex items-center justify-between py-2.5">
                    <div className="flex-1">
                      <label className={SETTINGS_FIELD}>{t("settings.pet.followStartup")}</label>
                      <p className="text-muted-foreground mt-0.5 text-xs">
                        {t("settings.pet.followStartupDesc")}
                      </p>
                    </div>
                    <Switch
                      checked={petCfg.followStartup}
                      onCheckedChange={(v) => onPetCfgChange({ followStartup: v })}
                    />
                  </div>
                  <div className="border-t" />
                  {/* 置顶开关：置顶时抑制主窗口浮窗通知（spec D4） */}
                  <div className="flex items-center justify-between py-2.5">
                    <div className="flex-1">
                      <label className={SETTINGS_FIELD}>{t("settings.pet.alwaysOnTop")}</label>
                    </div>
                    <Switch
                      checked={petCfg.alwaysOnTop}
                      onCheckedChange={(v) => onPetCfgChange({ alwaysOnTop: v })}
                    />
                  </div>
                  <div className="border-t" />
                  {/* 大小三档 */}
                  <div className="flex items-center justify-between py-2.5">
                    <label className={SETTINGS_FIELD}>{t("settings.pet.scale")}</label>
                    <div className="flex gap-1">
                      {PET_SCALES.map((s) => (
                        <button
                          key={s}
                          onClick={() => onPetCfgChange({ scale: s })}
                          className={`rounded px-3 py-1 text-sm transition-colors ${
                            petCfg.scale === s
                              ? "bg-accent text-accent-foreground font-medium"
                              : "text-muted-foreground hover:bg-accent/50"
                          }`}
                        >
                          {s === 0.75
                            ? t("pet.scale.small")
                            : s === 1
                              ? t("pet.scale.medium")
                              : t("pet.scale.large")}
                        </button>
                      ))}
                    </div>
                  </div>
                  <div className="border-t" />
                  {/* 当前宠物 + 三入口（spec §11）：切换在 Task 13，导入在 Task 16，修改在 Task 17 */}
                  <div className="flex items-center justify-between gap-2 py-2.5">
                    <label className={SETTINGS_FIELD}>{t("settings.pet.currentPet")}</label>
                    <span className="text-muted-foreground mr-auto pl-2 text-sm">
                      {activePetName}
                    </span>
                    <div className="flex gap-2">
                      <Button size="sm" variant="outline" onClick={() => setSwitchOpen(true)}>
                        {t("settings.pet.switchPet")}
                      </Button>
                      <Button size="sm" variant="outline" onClick={() => setImportOpen(true)}>
                        {t("settings.pet.importPet")}
                      </Button>
                      <Button size="sm" variant="outline" onClick={() => setManageOpen(true)}>
                        {t("settings.pet.managePet")}
                      </Button>
                    </div>
                  </div>
                </div>
              </div>
            </div>
          )}

          {/* 开机自动启动（2026-10-11）：「通知与桌宠」块的软件级常规卡（与宠物卡内的
              「启动跟随」分卡分层——本开关管整个软件与 OS 自启项，勾选态 = OS 侧实时真值） */}
          {visibleIds.has("autostart") && (
            <div className="overflow-hidden rounded-xl border">
              <div className="bg-muted/40 flex items-baseline justify-between gap-2 border-b px-4 py-2.5">
                <h2 className={SETTINGS_CARD_TITLE}>{t("settings.autostart.heading")}</h2>
                <span className="text-muted-foreground truncate text-xs">
                  {t("settings.autostart.description")}
                </span>
              </div>
              <div className="px-4 py-1">
                <div className="space-y-0">
                  <div className="flex items-center justify-between py-2.5">
                    <div className="flex-1">
                      <label className={SETTINGS_FIELD}>{t("settings.autostart.row")}</label>
                      <p className="text-muted-foreground mt-0.5 text-xs">
                        {t("settings.autostart.rowDesc")}
                      </p>
                    </div>
                    <Switch checked={autostartEnabled} onCheckedChange={toggleAutostart} />
                  </div>
                </div>
              </div>
            </div>
          )}

          {visibleIds.has("tools") && (
            <div className="overflow-hidden rounded-xl border">
              {/* 卡头条（2026-10-11 两级标题拉开）：h2 保留档位常量搬进条内；说明收条右侧 */}
              <div className="bg-muted/40 flex items-baseline justify-between gap-2 border-b px-4 py-2.5">
                <h2 className={SETTINGS_CARD_TITLE}>{t("settings.tools.title")}</h2>
                <span className="text-muted-foreground truncate text-xs">
                  {t("settings.tools.hint")}
                </span>
              </div>
              <div className="px-1 py-1">
                {/* 行式开关列表：名称 + 安装状态 badge + Switch */}
                <div className="divide-border divide-y">
                  {toolRows.map((r) => (
                    <div key={r.toolId} className="flex items-center justify-between px-3 py-2">
                      <div className="flex items-center gap-2">
                        {/* issue #36-6：行首补图标（spec §6「图标 + 名称 + badge + 开关」） */}
                        <ToolIcon toolId={r.toolId} size={16} />
                        <span className={SETTINGS_FIELD}>{r.name}</span>
                        <span
                          className={cn(
                            `rounded px-1.5 py-0.5 ${SETTINGS_BADGE}`,
                            r.installed
                              ? "bg-emerald-500/10 text-emerald-500"
                              : "bg-muted text-muted-foreground"
                          )}
                        >
                          {r.installed
                            ? t("settings.tools.installed")
                            : t("settings.tools.notInstalled")}
                        </span>
                      </div>
                      <Switch
                        checked={r.enabled}
                        onCheckedChange={(v) => toggleTool(r.toolId, v)}
                      />
                    </div>
                  ))}
                </div>
                {/* 裁决 34a：未启用工具的错误引导收敛在设置页文案级——勾选行旁明示
                    取消勾选对看板与远程能力的影响；APP 形态会话的写动作后端以
                    409 app_form_pending 拒绝（Task 3 止血门），本行是其入口侧引导 */}
                <p className={SETTINGS_SUBTITLE}>{t("settings.tools.boardRemoteNote")}</p>
                {toolDirty && (
                  <Button onClick={() => setConfirmOpen(true)}>{t("settings.tools.save")}</Button>
                )}
              </div>
            </div>
          )}

          {/* 一致性体检（spec §13）：同 query 数据只读摘要（处置入口在资源页卡片） */}
          {visibleIds.has("health") && (
            <div className="overflow-hidden rounded-xl border">
              <div className="bg-muted/40 flex items-baseline gap-2 border-b px-4 py-2.5">
                <h2 className={SETTINGS_CARD_TITLE}>{t("resources.health.title")}</h2>
              </div>
              <div className="p-3">
                <HealthSummary />
              </div>
            </div>
          )}
          {/* T5：信号健康度（hook 通道自查 + codex 信任门引导）。2026-10-13 用户裁决
              从「远程接入」块挪入本块——健康度检查对象是各 CLI 工具，与体检同块 */}
          {visibleIds.has("signal") && <SignalHealthSection />}
          {visibleIds.has("remote") && <RemoteSection />}
          {/* M7 W5：注入审计桌面查看入口（与 RemoteSection 同级独立分区） */}
          {visibleIds.has("audit") && <AuditLogSection />}
          {/* 2026-09-20：数据管理首版（移动端附件占用列出/清理，C5） */}
          {visibleIds.has("data") && <DataManagementSection />}
          {/* 计划② Task 14：用量统计设置分组（8 项设置；① 的采集验收面在下面的 usageStatus 分支） */}
          {visibleIds.has("usage") && <UsageSection />}
          {/* 计划① Task 24：用量采集状态（最小可见验收面；② 上线后可保留为调试入口或删除） */}
          {visibleIds.has("usageStatus") && <UsageStatusSection />}
        </div>
      </div>
      <PetSwitchDialog open={switchOpen} onOpenChange={setSwitchOpen} />
      <PetImportDialog open={importOpen} onOpenChange={setImportOpen} />
      <PetManageDialog open={manageOpen} onOpenChange={setManageOpen} />

      {/* 保存确认弹窗：列出变更行并按变更方向提示影响 */}
      <Dialog
        open={confirmOpen}
        onOpenChange={(v) => {
          setConfirmOpen(v);
          // 取消确认时丢弃缓存的跳转，留在本页
          if (!v) pendingJumpRef.current = null;
        }}
      >
        <DialogContent className="max-w-md">
          <DialogHeader>
            <DialogTitle>{t("settings.tools.confirmTitle")}</DialogTitle>
            <DialogDescription>{t("settings.tools.confirmDesc")}</DialogDescription>
          </DialogHeader>
          <div className="space-y-2">
            {changedRows.map((r) => (
              <div key={r.toolId} className="flex items-start justify-between gap-3 text-sm">
                <span className="font-medium">{r.name}</span>
                <span
                  className={cn(
                    "text-right text-xs",
                    r.enabled
                      ? "text-emerald-500"
                      : r.managed
                        ? "text-amber-500"
                        : "text-muted-foreground"
                  )}
                >
                  {r.enabled
                    ? t("settings.tools.enableItem")
                    : r.managed
                      ? t("settings.tools.restoreItem")
                      : t("settings.tools.disableItem")}
                </span>
              </div>
            ))}
          </div>
          <DialogFooter>
            <Button variant="outline" onClick={() => setConfirmOpen(false)}>
              {t("settings.tools.cancel")}
            </Button>
            <Button onClick={() => void applyChanges()} disabled={toolSaving}>
              {t("settings.tools.confirm")}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* 未保存离开拦截：保存 / 放弃更改 / 继续编辑 三选 */}
      <Dialog open={leaveGuard !== null} onOpenChange={(v) => !v && setLeaveGuard(null)}>
        <DialogContent className="max-w-md">
          <DialogHeader>
            <DialogTitle>{t("settings.tools.unsavedTitle")}</DialogTitle>
            <DialogDescription>{t("settings.tools.unsavedDesc")}</DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="ghost" onClick={() => setLeaveGuard(null)}>
              {t("settings.tools.keepEditing")}
            </Button>
            <Button
              variant="outline"
              onClick={() => {
                // 放弃更改：缓存回调内含「清脏 + 跳转」
                const jump = leaveGuard;
                setLeaveGuard(null);
                jump?.();
              }}
            >
              {t("settings.tools.discard")}
            </Button>
            <Button
              onClick={() => {
                // 保存：记下跳转，确认应用成功后再执行
                pendingJumpRef.current = leaveGuard;
                setLeaveGuard(null);
                setConfirmOpen(true);
              }}
            >
              {t("settings.tools.save")}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </WindowFrame>
  );
}
