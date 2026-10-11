import { Languages } from "lucide-react";
import { useTranslation } from "react-i18next";
import { emit } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import { loadVisible } from "@/components/pet/petConfig";

export function LanguageToggle() {
  const { i18n, t } = useTranslation();

  const toggleLanguage = async () => {
    const newLang = i18n.language === "zh" ? "en" : "zh";
    await i18n.changeLanguage(newLang);

    // Update tray menu with new language（Task 16 统一重建：基础项 + 预设项一次成型，
    // 预设项不再因语言切换丢失）
    try {
      await invoke("refresh_tray", {
        presetsLabel: t("tray.presetsLabel", { lng: newLang }),
        showText: t("tray.show", { lng: newLang }),
        petText: loadVisible()
          ? t("tray.petHide", { lng: newLang })
          : t("tray.petShow", { lng: newLang }),
        // M4 T4：远程开关项标签随语言重建（勾选态 Rust 侧自查）；
        // main 侧 preset-v2 补的 quitText 一并传入（合并：两侧参数并集）
        remoteOnText: t("tray.remote", { lng: newLang }),
        // 2026-10-11：开机自启开关项标签随语言重建（勾选态 Rust 侧自查）
        autostartText: t("tray.autostart", { lng: newLang }),
        quitText: t("tray.quit", { lng: newLang }),
      });
    } catch (error) {
      console.error("Failed to update tray menu:", error);
    }

    // Emit event to sync language across windows
    await emit("language-changed", { language: newLang });
  };

  return (
    <button
      onClick={toggleLanguage}
      className="title-bar-btn mr-1"
      aria-label={t("language.toggle")}
      title={i18n.language === "zh" ? "Switch to English" : "Switch to 中文"}
      tabIndex={-1}
    >
      <Languages className="h-4 w-4" />
    </button>
  );
}
