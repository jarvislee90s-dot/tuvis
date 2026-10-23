import { listen } from "@tauri-apps/api/event";
import { useEffect } from "react";
import { useTranslation } from "react-i18next";

export function useAppTranslation() {
  const { t, i18n } = useTranslation();

  useEffect(() => {
    const unlistenLanguageChanged = listen<{ language: string }>("language-changed", (event) => {
      i18n.changeLanguage(event.payload.language);
    });

    return () => {
      unlistenLanguageChanged.then((fn) => fn());
    };
  }, [i18n]);

  // i18n 一并暴露：消费方（HomePage 托盘 effect）需要读 i18n.language 把当前语言
  // 推给 Rust（三终端语言自适应 C-3）；i18n.language 恒为规范 "zh"/"en"（C-1）
  return { t, i18n };
}
