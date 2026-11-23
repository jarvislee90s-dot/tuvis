// OpenCode 原生交互设置卡（2026-11-23 精简批：一个总开关 + 一行状态 + 至多三个动作）。
//
// **用户裁决的形态**：
// - 卡面与相邻 Codex APP / ZCode 卡同款整行折叠：默认收起时**只有一行**（图标、标题、状态、
//   箭头），卡头整行就是唯一展开入口；总开关、卸载、安装/修复、兼容控件、详情与重试全部
//   收在展开内容里，卡头内不嵌套任何交互元素。
// - 总开关「OpenCode 远程控制」= 观察插件 + 写入桥接**两个插件都装上**；开启即自动安装
//   两个插件，关闭即自动卸载两个插件。总开关只是把两个既有命令组合起来，**不新增安装器**：
//   观察插件仍走 `opencode_tui_observer_install/uninstall`，桥接仍走
//   `opencode_bridge_install/uninstall`。
// - 组合动作**可恢复的一致状态**：任一步失败 → 状态行如实显示失败原因（不谎报已就绪），
//   保留「安装/修复」入口（重跑幂等安装）；**不回滚**已成功的另一步（避免来回装卸）。
// - 状态压成一行：正常态只显示一句结论；异常态（冲突 / 中断 / 读取失败）才显示可读原因。
// - 动作最多三个：安装/修复（正常态隐藏）、卸载、总开关。其余（观察插件状态、桥接状态与
//   PID、影响说明、每会话写入权限说明、刷新状态）全部收进**默认收起**的展开区。
// - 「允许旧终端交互」本轮语义不动，只移到分区末尾，避免与总开关并列成两个开关的观感。
//
// **安全语义（不得退化）**：
// - 冲突态（observer === "conflict"）保留显式恢复动作：「安装/修复」= 重试安装（续写未完成
//   提交），「卸载」= 完成卸载（清理中断残留）；外来文件不会被覆盖。
// - 桥接「已安装但服务未加载」不冒充已就绪（状态为「等待加载」）。
// - 读取失败按「未知」处理：不清空已显示的状态、不猜成未安装。
import { useCallback, useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { ChevronDown, ChevronRight } from "lucide-react";
import { ToolIcon } from "@/components/common/ToolIcon";
import { Button } from "@/components/ui/button";
import { Switch } from "@/components/ui/switch";
import { useAppTranslation } from "@/hooks/use-app-translation";
import { formatInvokeError } from "@/lib/invokeError";
import { cn } from "@/lib/utils";
import {
  getOpenCodeBridgeStatus,
  getOpenCodeInteractionPolicyStatus,
  getOpenCodeObserverStatus,
  installOpenCodeObserver,
  uninstallOpenCodeObserver,
  type OpenCodeObserverStatus,
  installOpenCodeBridge,
  reloadOpenCodeBridge,
  setOpenCodeInteractionPolicy,
  uninstallOpenCodeBridge,
  type OpenCodeBridgeView,
  type OpenCodeInteractionPolicy,
} from "@/lib/api/remote";
import { SETTINGS_CARD_TITLE, SETTINGS_FIELD, SETTINGS_HINT } from "./typography";

/** 详情面板 DOM id 单点（aria-controls 与面板 id 同源）。 */
const PANEL_ID = "opencode-native-details-panel";

/** 总状态（单行状态的判定源；`data-state` 是测试与样式的可观测面）。 */
type NativeState =
  "loading" | "off" | "partial" | "pending" | "ready" | "outdated" | "conflict" | "failed";

export function OpenCodeNativeSection() {
  const { t } = useAppTranslation();
  const [policy, setPolicy] = useState<OpenCodeInteractionPolicy>("native_only");
  const [legacyConfigurable, setLegacyConfigurable] = useState(false);
  const [ready, setReady] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState(false);
  const [reload, setReload] = useState(0);
  const [bridge, setBridge] = useState<OpenCodeBridgeView | null>(null);
  const [bridgeQueryError, setBridgeQueryError] = useState(false);
  const [bridgeActionError, setBridgeActionError] = useState<string | null>(null);
  const [observer, setObserver] = useState<OpenCodeObserverStatus | null>(null);
  const [observerQueryError, setObserverQueryError] = useState(false);
  const [observerActionError, setObserverActionError] = useState<string | null>(null);
  const [observerChanged, setObserverChanged] = useState(false);
  const [busy, setBusy] = useState(false);
  const [expanded, setExpanded] = useState(false);

  /** 错误 → 可读原因（Tauri 拒绝多为字符串码，Error 实例取 message；不猜成功）。 */
  const describe = (e: unknown) => (e instanceof Error ? e.message : formatInvokeError(e, t));

  const refreshObserver = useCallback(async () => {
    try {
      setObserver(await getOpenCodeObserverStatus());
      setObserverQueryError(false);
    } catch {
      // 读取失败按「未知」处理：保留最后已知状态，只标记读取失败。
      setObserverQueryError(true);
    }
  }, []);

  const refreshBridge = useCallback(async () => {
    try {
      setBridge(await getOpenCodeBridgeStatus());
      setBridgeQueryError(false);
    } catch {
      setBridgeQueryError(true);
    }
  }, []);

  /** 详情区的「刷新状态」：只读重查两个插件，不改安装、不清失败原因。 */
  const refreshPlugins = useCallback(async () => {
    await refreshObserver();
    await refreshBridge();
  }, [refreshObserver, refreshBridge]);

  useEffect(() => {
    let alive = true;
    let latest = 0;
    const refresh = async () => {
      const request = ++latest;
      try {
        const value = await getOpenCodeInteractionPolicyStatus();
        if (alive && request === latest) {
          setPolicy(value.policy);
          setLegacyConfigurable(value.legacyConfigurable === true);
          setReady(true);
          setError(false);
        }
      } catch {
        if (alive && request === latest) setError(true);
      }
    };
    void refresh();
    const stop = listen("opencode-interaction-policy-changed", () => void refresh());
    return () => {
      alive = false;
      void stop.then((unlisten) => unlisten());
    };
  }, [reload]);

  useEffect(() => {
    void refreshBridge();
    void refreshObserver();
  }, [refreshBridge, refreshObserver]);

  const changeLegacy = async (checked: boolean) => {
    if (!legacyConfigurable) return;
    setSaving(true);
    setError(false);
    try {
      const next = checked ? "legacy_compatible" : "native_only";
      await setOpenCodeInteractionPolicy(next);
      setReady(false);
      setReload((n) => n + 1);
    } catch {
      setError(true);
    } finally {
      setSaving(false);
    }
  };

  const updateObserver = async (
    operation: () => Promise<OpenCodeObserverStatus>,
    changedStatus: OpenCodeObserverStatus
  ) => {
    try {
      const result = await operation();
      setObserver(result);
      setObserverQueryError(false);
      setObserverActionError(null);
      setObserverChanged(result === changedStatus);
    } catch (e) {
      setObserverActionError(describe(e));
      setObserverChanged(false);
      await refreshObserver();
    }
  };

  const updateBridge = async (operation: () => Promise<OpenCodeBridgeView>) => {
    try {
      setBridge(await operation());
      setBridgeQueryError(false);
      setBridgeActionError(null);
    } catch (e) {
      setBridgeActionError(describe(e));
      await refreshBridge();
    }
  };

  /** 总开关 = 两个插件命令的组合；一步失败不回滚另一步（不做来回装卸）。 */
  const runComposite = async (target: "on" | "off") => {
    if (busy) return;
    setBusy(true);
    setObserverActionError(null);
    setBridgeActionError(null);
    try {
      if (target === "on") {
        await updateObserver(installOpenCodeObserver, "current");
        await updateBridge(installOpenCodeBridge);
      } else {
        await updateObserver(uninstallOpenCodeObserver, "absent");
        await updateBridge(uninstallOpenCodeBridge);
      }
    } finally {
      setBusy(false);
    }
  };

  /** 「安装/修复」：冲突态只续写观察插件（外来文件仍不覆盖）；其余补装缺的一侧。 */
  const repair = async () => {
    if (busy) return;
    setBusy(true);
    setObserverActionError(null);
    setBridgeActionError(null);
    try {
      if (conflict) {
        await updateObserver(installOpenCodeObserver, "current");
        return;
      }
      const needObserver = observer !== "current";
      const needBridge = !installed || !needObserver;
      if (needObserver) await updateObserver(installOpenCodeObserver, "current");
      if (needBridge) await updateBridge(installOpenCodeBridge);
    } finally {
      setBusy(false);
    }
  };

  /** 「卸载」：冲突态只完成观察插件的中断卸载；其余按总开关语义两个都卸。 */
  const remove = async () => {
    if (busy) return;
    if (conflict) {
      setBusy(true);
      setObserverActionError(null);
      try {
        await updateObserver(uninstallOpenCodeObserver, "absent");
      } finally {
        setBusy(false);
      }
      return;
    }
    await runComposite("off");
  };

  const reloadBridge = async () => {
    if (busy) return;
    setBusy(true);
    setBridgeActionError(null);
    try {
      await updateBridge(reloadOpenCodeBridge);
    } finally {
      setBusy(false);
    }
  };

  const installed =
    bridge?.install.pluginInstalled === true && bridge.install.configRegistered === true;
  const connected = bridge?.bridge?.attested === true;
  const observerInstalled = observer === "current" || observer === "outdated";
  const loading =
    (observer === null && !observerQueryError) || (bridge === null && !bridgeQueryError);
  const actionError = observerActionError ?? bridgeActionError;
  /** 连接失败（桥接在、但连接被断开）：如实算异常态并给出后端原因。 */
  const connectionError = installed && !connected ? (bridge?.bridge?.lastError ?? null) : null;

  // 单行状态判定（优先级：操作失败 > 读取失败 > 冲突 > 装载进度）。
  const state: NativeState = actionError
    ? "failed"
    : observerQueryError || bridgeQueryError
      ? "failed"
      : loading
        ? "loading"
        : observer === "conflict"
          ? "conflict"
          : observer === "outdated"
            ? "outdated"
            : !observerInstalled && !installed
              ? "off"
              : observerInstalled && installed
                ? connected
                  ? "ready"
                  : connectionError !== null
                    ? "failed"
                    : "pending"
                : "partial";

  const statusText = (() => {
    switch (state) {
      case "loading":
        return t("settings.openCodeNative.statusLoading");
      case "ready":
        return t("settings.openCodeNative.statusReady");
      case "off":
        return t("settings.openCodeNative.statusOff");
      case "partial":
        return t("settings.openCodeNative.statusPartial");
      case "pending":
        return t("settings.openCodeNative.statusPending");
      case "outdated":
        return t("settings.openCodeNative.statusOutdated");
      case "conflict":
        return t("settings.openCodeNative.statusConflict");
      case "failed":
        if (actionError !== null) {
          return t("settings.openCodeNative.statusFailed", { reason: actionError });
        }
        if (connectionError !== null) {
          return t("settings.openCodeNative.statusConnectionFailed", {
            reason: connectionError,
          });
        }
        return t("settings.openCodeNative.statusReadFailed");
    }
  })();

  const bridgeStatusText = connected
    ? t("settings.openCodeNative.bridge.connected", {
        pid: bridge?.bridge?.peerPid ?? "?",
      })
    : installed
      ? t("settings.openCodeNative.bridge.installed")
      : t("settings.openCodeNative.bridge.notInstalled");

  const conflict = observer === "conflict";
  // 安装/修复只在异常态出现（正常态与「等待加载」不需要）；「未开启」由总开关开启即自动安装。
  const showRepair =
    state === "partial" || state === "outdated" || state === "conflict" || state === "failed";
  const showRemove = observerInstalled || installed || conflict;
  // 状态未知（读取失败）时不允许写盘动作：不拿「未知」当「未安装」。
  const unknown = observer === null || bridge === null;

  return (
    <section
      data-tool-card="opencode"
      className="mt-2 overflow-hidden rounded-xl border"
      aria-busy={busy}
    >
      {/* 卡头 = 唯一展开入口（与 Codex APP / ZCode 卡同款整行折叠语义）：
          折叠态只有 图标 + 标题 + 一行状态 + 箭头，不留任何交互控件。
          h3 包住 button（disclosure 模式）：标题语义与整行点击兼得。 */}
      <div className="bg-muted/40">
        <h3 className={`${SETTINGS_CARD_TITLE} flex items-center`}>
          <button
            type="button"
            data-testid="opencode-native-details-toggle"
            aria-expanded={expanded}
            aria-controls={PANEL_ID}
            onClick={() => setExpanded((v) => !v)}
            className="hover:bg-muted flex w-full items-center gap-2 px-4 py-2.5 text-left"
          >
            <ToolIcon toolId="opencode" size={18} />
            <span>{t("settings.openCodeNative.masterTitle")}</span>
            <span
              data-testid="opencode-native-status"
              data-state={state}
              role={state === "failed" ? "alert" : undefined}
              className={cn(
                "ml-auto min-w-0 truncate font-normal",
                state === "failed"
                  ? cn(SETTINGS_HINT, "text-rose-600 dark:text-rose-400")
                  : SETTINGS_HINT
              )}
            >
              {statusText}
            </span>
            {expanded ? (
              <ChevronDown className="text-muted-foreground flex-none" size={16} aria-hidden />
            ) : (
              <ChevronRight className="text-muted-foreground flex-none" size={16} aria-hidden />
            )}
          </button>
        </h3>
      </div>

      {/* 展开内容：总开关、安装/修复、卸载、兼容设置与详情全部在内层，卡头不嵌套交互元素。 */}
      {expanded && (
        <div
          id={PANEL_ID}
          data-testid="opencode-native-details"
          role="region"
          aria-label={t("settings.openCodeNative.title")}
          className="space-y-3 border-t p-4"
        >
          <div className="flex items-center justify-between gap-3">
            <span className={SETTINGS_FIELD}>{t("settings.openCodeNative.masterTitle")}</span>
            <Switch
              data-testid="opencode-native-switch"
              aria-label={t("settings.openCodeNative.masterTitle")}
              checked={observerInstalled && installed}
              // 状态未知（读取失败 / 首拍未回）时禁用总开关：不拿「未知」当「未安装」去写盘。
              disabled={busy || loading || unknown}
              onCheckedChange={(checked) => void runComposite(checked ? "on" : "off")}
            />
          </div>

          {(showRepair || showRemove) && (
            <div className="flex flex-wrap items-center gap-2">
              {showRepair && (
                <Button
                  size="sm"
                  data-testid="opencode-native-repair"
                  disabled={busy || unknown}
                  onClick={() => void repair()}
                >
                  {conflict
                    ? t("settings.openCodeNative.observer.retryInstall")
                    : t("settings.openCodeNative.repair")}
                </Button>
              )}
              {showRemove && (
                <Button
                  size="sm"
                  variant="outline"
                  data-testid="opencode-native-remove"
                  disabled={busy || unknown}
                  onClick={() => void remove()}
                >
                  {conflict
                    ? t("settings.openCodeNative.observer.finishUninstall")
                    : t("settings.openCodeNative.remove")}
                </Button>
              )}
            </div>
          )}

          {/* 详情区：插件独立状态、桥接 PID、影响说明、写入权限、刷新。 */}
          <div className="space-y-2 border-t pt-3">
            <div className="space-y-1" data-testid="opencode-observer">
              <h4 className={SETTINGS_FIELD}>{t("settings.openCodeNative.observer.title")}</h4>
              <p className={SETTINGS_HINT}>{t("settings.openCodeNative.observer.description")}</p>
              {observer !== null && (
                <p data-testid="opencode-observer-status" className={SETTINGS_FIELD}>
                  {t(`settings.openCodeNative.observer.${observer}`)}
                </p>
              )}
              {observerChanged && (
                <p data-testid="opencode-observer-restart" className={SETTINGS_HINT}>
                  {t("settings.openCodeNative.observer.restart")}
                </p>
              )}
              {(observerActionError !== null || observerQueryError) && (
                <p role="alert" data-testid="opencode-observer-error" className={SETTINGS_HINT}>
                  {observerActionError ?? t("settings.openCodeNative.observer.error")}
                </p>
              )}
            </div>

            <div className="space-y-1 border-t pt-2" data-testid="opencode-bridge">
              <h4 className={SETTINGS_FIELD}>{t("settings.openCodeNative.bridge.title")}</h4>
              <p className={SETTINGS_HINT}>{t("settings.openCodeNative.bridge.description")}</p>
              <p className={SETTINGS_HINT}>{t("settings.openCodeNative.bridge.impact")}</p>
              <p data-testid="opencode-bridge-status" className={SETTINGS_FIELD}>
                {bridgeStatusText}
              </p>
              <p className={SETTINGS_HINT}>
                {t("settings.openCodeNative.bridge.sessionWriteStatus")}
              </p>
              {bridge?.reloadError !== null && bridge?.reloadError !== undefined && (
                <p
                  data-testid="opencode-bridge-reload-warning"
                  className={cn(SETTINGS_HINT, "text-amber-700")}
                >
                  {t("settings.openCodeNative.bridge.reloadPending")}
                </p>
              )}
              {installed && (
                <Button
                  size="sm"
                  variant="outline"
                  data-testid="opencode-bridge-reload"
                  disabled={busy}
                  onClick={() => void reloadBridge()}
                >
                  {t("settings.openCodeNative.bridge.reload")}
                </Button>
              )}
              {(bridgeActionError !== null || bridgeQueryError) && (
                <p role="alert" data-testid="opencode-bridge-error" className={SETTINGS_HINT}>
                  {bridgeActionError ?? t("settings.openCodeNative.bridge.error")}
                </p>
              )}
            </div>

            <Button
              size="sm"
              variant="outline"
              data-testid="opencode-native-recheck"
              disabled={busy}
              onClick={() => void refreshPlugins()}
            >
              {t("settings.openCodeNative.bridge.recheck")}
            </Button>
          </div>

          {/* 未退役的平台沿用现有显式兼容设置；平台未确认时不显示入口。 */}
          {legacyConfigurable && (
            <div className="space-y-1 border-t pt-3">
              <div className="flex items-center justify-between gap-3">
                <span className={SETTINGS_FIELD}>{t("settings.openCodeNative.legacy")}</span>
                <Switch
                  data-testid="opencode-legacy-switch"
                  aria-label={t("settings.openCodeNative.legacy")}
                  checked={policy === "legacy_compatible"}
                  disabled={!ready || saving}
                  onCheckedChange={(checked) => void changeLegacy(checked)}
                />
              </div>
              <p className={SETTINGS_HINT}>{t("settings.openCodeNative.description")}</p>
            </div>
          )}
          {error && (
            <div>
              <p role="alert">{t("settings.openCodeNative.error")}</p>
              <button
                type="button"
                data-testid="opencode-policy-retry"
                className="text-sm underline"
                disabled={saving}
                onClick={() => setReload((n) => n + 1)}
              >
                {t("common.retry")}
              </button>
            </div>
          )}
        </div>
      )}
    </section>
  );
}
