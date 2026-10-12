// 关于页（2026-10-12 改版，CC Switch 风格）：
// - 版本卡：appName + 当前版本 + 更新动作。on mount 自动检查一次更新（复用
//   check_for_github_update，不轮询；自动路径失败静默——用户没点按钮就不弹红条，
//   仅手动点「检查更新」失败才 toast）——有新版本 → 右侧醒目主色「更新到 vX.Y.Z」
//   主按钮（点击显式打开本窗口 UpdaterDialog 走既有安装流程）+「检测到新版本」
//   状态行；无新版本 → 维持「检查更新」次按钮 + upToDate 行内提示
// - 快捷行：官方仓库 / 更新日志 / 近期更新 三枚外链按钮
// - Star 横幅：amber 横条；关闭态写 localStorage 且键含当前 app 版本号
//   （升级到新版本后横幅重新出现）
// - 技术栈显示区保留不动
import { useEffect, useRef, useState } from "react";
import { getVersion } from "@tauri-apps/api/app";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { Toaster } from "sonner";
import { ArrowUpCircle, ExternalLink, History, RefreshCw, Sparkles, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { TitleBar } from "@/components/common/title-bar";
import { WindowFrame } from "@/components/common/window-frame";
import { useAppTranslation } from "@/hooks/use-app-translation";
import { openUrl } from "@tauri-apps/plugin-opener";
import { cancelDestroyWindow, destroyWindow } from "@/lib/window";
import { useManualUpdateCheck, UpdaterDialog } from "@/components/common/updater-dialog";
import { useUpdaterStore } from "@/stores/updaterStore";
import packageJson from "../../package.json";

const GITHUB_REPO_URL = "https://github.com/jarvislee90s-dot/tuvis";
const GITHUB_RELEASES_URL = `${GITHUB_REPO_URL}/releases`;
const GITHUB_LATEST_URL = `${GITHUB_REPO_URL}/releases/latest`;

/** Star 横幅关闭态键：含当前版本号 → 升级后横幅重新出现（产品设计语义） */
const starBannerKey = (version: string) => `tuvis.star-banner.dismissed.${version}`;

const techVersions = {
  tauri: packageJson.dependencies["@tauri-apps/api"].replace(/^\^/, "v"),
  react: packageJson.dependencies.react.replace(/^\^/, "v"),
  typescript: packageJson.devDependencies.typescript.replace(/^~/, "v"),
};

export default function AboutPage() {
  const [appVersion, setAppVersion] = useState("");
  const { t } = useAppTranslation();
  const { checkUpdate, checkUpdateSilent, checking, showNoUpdate } = useManualUpdateCheck();
  // 有无新版本以 store 检查结果为权威（mount 自动检查与手动检查共用一条管道）
  const result = useUpdaterStore((s) => s.result);
  const openDialog = useUpdaterStore((s) => s.openDialog);
  const update = result?.status === "available" ? result.update : null;

  useEffect(() => {
    void getVersion().then(setAppVersion);
  }, []);

  // on mount 自动检查一次（ref 门栓兜 dev StrictMode 双挂载；不轮询）。
  // 走静默变体：用户没点任何按钮，断网时不该凭空弹红条（update 契约见 updater.rs
  // check_for_github_update 尾注）——「已是最新」行内提示照常点亮。
  // 弹窗不自动开（UpdaterDialog autoOpen=false）——检查结果只点亮主按钮，点击才开
  const autoCheckRef = useRef(false);
  useEffect(() => {
    if (autoCheckRef.current) return;
    autoCheckRef.current = true;
    void checkUpdateSilent();
  }, [checkUpdateSilent]);

  // Star 横幅关闭态：null = 版本未就绪（先不渲染，避免「闪现后又被藏起」）；
  // 版本就绪后读一次 localStorage 判定
  const [starDismissed, setStarDismissed] = useState<boolean | null>(null);
  useEffect(() => {
    if (!appVersion) return;
    let dismissed = false;
    try {
      dismissed = localStorage.getItem(starBannerKey(appVersion)) !== null;
    } catch {
      // 存储不可用（隐私模式等）→ 按未关闭处理，横幅照常显示
    }
    setStarDismissed(dismissed);
  }, [appVersion]);

  const dismissStarBanner = () => {
    setStarDismissed(true);
    if (!appVersion) return;
    try {
      localStorage.setItem(starBannerKey(appVersion), "1");
    } catch {
      // 写不进则只在本会话内隐藏
    }
  };

  useEffect(() => {
    const appWindow = getCurrentWebviewWindow();

    const unlistenClose = appWindow.onCloseRequested(async (event) => {
      event.preventDefault();
      console.log("About window close requested, will destroy in 5 seconds");
      await destroyWindow(appWindow.label, 5000);
    });

    const unlistenFocusChanged = appWindow.onFocusChanged(({ payload: focused }) => {
      if (focused) {
        cancelDestroyWindow(appWindow.label);
      }
    });

    return () => {
      unlistenClose.then((fn) => fn());
      unlistenFocusChanged.then((fn) => fn());
    };
  }, []);

  return (
    <WindowFrame
      titleBar={<TitleBar title={t("about.title")} showMinimize={false} showMaximize={false} />}
      contentClassName="flex flex-1 items-center justify-center overflow-hidden"
    >
      {/* 手动检查/主按钮出可用更新时在本窗口弹升级弹窗（store 为窗口内实例）；
          autoOpen=false：mount 自动检查只点亮主按钮，弹窗由主按钮显式打开；
          Toaster 必须本窗口自挂——检查失败/忽略成功的 toast 否则丢进虚空（评审 I4） */}
      <UpdaterDialog manualCheck autoOpen={false} />
      <Toaster />
      <div className="w-full max-w-sm space-y-4">
        {/* 版本卡 */}
        <div className="space-y-3 rounded-lg border p-4">
          <div className="flex items-center justify-between gap-2">
            <div className="min-w-0">
              <h2 className="text-2xl font-bold">{t("about.appName")}</h2>
              <p className="text-muted-foreground text-sm">
                {t("about.version")} {appVersion}
              </p>
            </div>
            {update ? (
              <Button size="sm" onClick={openDialog} className="shrink-0">
                <ArrowUpCircle className="mr-1 h-4 w-4" />
                {t("about.updateTo", { version: update.version })}
              </Button>
            ) : (
              <Button
                size="sm"
                variant="outline"
                onClick={checkUpdate}
                disabled={checking}
                className="shrink-0"
              >
                <RefreshCw className={`mr-1 h-4 w-4 ${checking ? "animate-spin" : ""}`} />
                {checking ? t("updater.checking") : t("updater.checkForUpdates")}
              </Button>
            )}
          </div>

          {update && (
            <p className="text-primary text-sm font-medium">
              {t("about.newVersionDetected", { version: update.version })}
            </p>
          )}
          {!update && showNoUpdate && (
            <p className="text-muted-foreground text-center text-sm">{t("updater.upToDate")}</p>
          )}

          {/* 技术栈显示区（保留不动） */}
          <div className="space-y-2 border-t pt-3 text-sm">
            <div className="flex justify-between">
              <span className="text-muted-foreground">Tauri</span>
              <span className="font-medium">{techVersions.tauri}</span>
            </div>
            <div className="flex justify-between">
              <span className="text-muted-foreground">React</span>
              <span className="font-medium">{techVersions.react}</span>
            </div>
            <div className="flex justify-between">
              <span className="text-muted-foreground">TypeScript</span>
              <span className="font-medium">{techVersions.typescript}</span>
            </div>
          </div>
        </div>

        {/* 快捷行 */}
        <div className="grid grid-cols-3 gap-2">
          <Button
            variant="outline"
            size="sm"
            className="text-xs"
            onClick={() => void openUrl(GITHUB_REPO_URL)}
          >
            <ExternalLink className="mr-1 h-3.5 w-3.5" />
            {t("about.officialRepo")}
          </Button>
          <Button
            variant="outline"
            size="sm"
            className="text-xs"
            onClick={() => void openUrl(GITHUB_RELEASES_URL)}
          >
            <History className="mr-1 h-3.5 w-3.5" />
            {t("about.changelog")}
          </Button>
          <Button
            variant="outline"
            size="sm"
            className="text-xs"
            onClick={() => void openUrl(GITHUB_LATEST_URL)}
          >
            <Sparkles className="mr-1 h-3.5 w-3.5" />
            {t("about.recentUpdates")}
          </Button>
        </div>

        {/* Star 横幅 */}
        {starDismissed === false && (
          <div className="flex items-center gap-2 rounded-lg border border-amber-500/40 bg-amber-500/10 p-3">
            <span aria-hidden className="text-base leading-none">
              ⭐
            </span>
            <p className="flex-1 text-xs leading-relaxed">{t("about.starMessage")}</p>
            <Button
              size="sm"
              className="shrink-0 bg-amber-500 text-xs text-white hover:bg-amber-500/90"
              onClick={() => void openUrl(GITHUB_REPO_URL)}
            >
              {t("about.starAction")}
            </Button>
            <button
              type="button"
              aria-label={t("about.starDismiss")}
              onClick={dismissStarBanner}
              className="text-muted-foreground hover:text-foreground shrink-0 transition-colors"
            >
              <X className="h-3.5 w-3.5" />
            </button>
          </div>
        )}
      </div>
    </WindowFrame>
  );
}
