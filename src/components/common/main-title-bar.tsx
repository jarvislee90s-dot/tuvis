import { Moon, Sun, Info, Settings, ChartLine, Download } from "lucide-react";
import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { useTheme } from "@/components/common/theme-provider";
import { createWindow } from "@/lib/window";
import { openUsageDashboard } from "@/lib/usage/openWindow";
import { TitleBar } from "@/components/common/title-bar";
import { LanguageToggle } from "@/components/common/language-toggle";
import { useTranslation } from "react-i18next";
import { loadVisible, saveVisible, subscribeConfig } from "@/components/pet/petConfig";
import { useAvailableUpdate, useUpdaterStore } from "@/stores/updaterStore";
import packageJson from "../../../package.json";

export function MainTitleBar() {
  const { theme, setTheme } = useTheme();
  const { t } = useTranslation();
  // 升级徽标：检查结果 available 才渲染（无更新/检查失败不出现）；被「忽略本版本」
  // 静音时仍常显——忽略只压弹窗，不藏徽标。点击打开升级弹窗（再点「立即升级」）
  const availableUpdate = useAvailableUpdate();
  const openUpdaterDialog = useUpdaterStore((s) => s.openDialog);
  // 桌宠开关状态：与 petConfig 双向同步（设置页/宠物菜单/托盘改动经订阅回流）
  const [petOn, setPetOn] = useState(() => loadVisible());
  useEffect(
    () =>
      subscribeConfig(() => {
        setPetOn(loadVisible());
      }),
    []
  );

  const handleTogglePet = async () => {
    const next = !petOn;
    saveVisible(next);
    setPetOn(next);
    try {
      await invoke("set_pet_visible", { visible: next });
    } catch (e) {
      console.error("set_pet_visible failed:", e);
    }
  };

  const handleToggleTheme = () => {
    setTheme(theme === "dark" ? "light" : "dark");
  };

  // 子窗口创建时按当前主题传 Tauri theme 选项（原生边框/标题栏与全局主题一致）；
  // system 模式不传（Tauri 默认跟随系统）
  const tauriTheme = theme === "system" ? undefined : theme;

  const handleOpenAbout = async () => {
    await createWindow("about", {
      title: t("about.title"),
      url: "/about",
      width: 500,
      height: 400,
      resizable: false,
      maximizable: false,
      minimizable: false,
      decorations: false,
      transparent: true,
      shadow: false,
      alwaysOnTop: true,
      parent: "main",
      theme: tauriTheme,
    });
  };

  const handleOpenSettings = async () => {
    await createWindow("settings", {
      title: t("settings.title"),
      url: "/settings",
      // M5 A6（线稿 v5 定稿）：远程接入四卡 + 唯一展开详情区需要更宽的设置窗口
      width: 880,
      height: 640,
      resizable: true,
      maximizable: true,
      minimizable: false,
      decorations: false,
      transparent: true,
      shadow: false,
      parent: "main",
      theme: tauriTheme,
    });
  };

  return (
    <TitleBar
      title={`${t("app.title")} v${packageJson.version}`}
      rightActions={
        <>
          {/* 用量看板入口（计划② Task 6 步骤 9）：图表键 → 经 openUsageDashboard 请求
              主窗导航至「模型用量」页签（V2 四页同窗后不再创建独立 usage-dashboard 窗口）。
              title/theme 形参为旧签名兼容保留，已无效果 */}
          <button
            onClick={() => void openUsageDashboard({ title: t("usage.title"), theme: tauriTheme })}
            className="title-bar-btn mr-1"
            aria-label={t("usage.title")}
            title={t("usage.title")}
            tabIndex={-1}
          >
            <ChartLine className="h-4 w-4" />
          </button>

          <button
            onClick={handleTogglePet}
            className="title-bar-btn mr-1 text-base leading-none"
            aria-label={t("home.petToggle")}
            title={t("home.petToggle")}
            tabIndex={-1}
          >
            <span className={petOn ? "" : "opacity-45 grayscale"}>🦊</span>
          </button>

          <button
            onClick={handleToggleTheme}
            className="title-bar-btn mr-0.5"
            aria-label={t("theme.toggle")}
            tabIndex={-1}
          >
            {theme === "dark" ? <Sun className="h-4 w-4" /> : <Moon className="h-4 w-4" />}
          </button>
          <LanguageToggle />

          {/* 升级徽标（prerelease 渠道）：琥珀色下载图标 + 呼吸圆点 */}
          {availableUpdate && (
            <button
              onClick={openUpdaterDialog}
              className="title-bar-btn relative mr-1 text-amber-500 hover:bg-amber-500/15 hover:text-amber-500"
              aria-label={t("updater.badgeTooltip", { version: availableUpdate.version })}
              title={t("updater.badgeTooltip", { version: availableUpdate.version })}
              tabIndex={-1}
            >
              <Download className="h-4 w-4" />
              <span className="ring-background absolute top-0.5 right-0.5 h-1.5 w-1.5 animate-pulse rounded-full bg-amber-500 ring-2" />
            </button>
          )}

          <button
            onClick={handleOpenSettings}
            className="title-bar-btn mr-1"
            aria-label={t("settings.button")}
            tabIndex={-1}
          >
            <Settings className="h-4 w-4" />
          </button>

          <button
            onClick={handleOpenAbout}
            className="title-bar-btn mr-1"
            aria-label={t("about.button")}
            tabIndex={-1}
          >
            <Info className="h-4 w-4" />
          </button>
        </>
      }
    />
  );
}
