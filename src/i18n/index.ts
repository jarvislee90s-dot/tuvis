import i18n from "i18next";
import { initReactI18next } from "react-i18next";
import en from "./locales/en.json";
import zh from "./locales/zh.json";
import { navigatorLanguageCandidates, resolveInitialLanguage } from "./language-prefs";

// 冷启动语言判定（需求书 D1 / D2 / D4）：
// - 未手动选择 → 按系统语言归流检测（zh* 系 → zh，其余 → en），且**不持久化任何语言值**，
//   系统语言变化后下次启动即跟随新语言（U8 核心语义）；
// - 手动选择过 → 以最后一次手动切换为准（读取 language-prefs 的手动标记存储，
//   写入点在 language-toggle 的手动切换入口）。
//
// 不再使用 i18next-browser-languagedetector：其 localStorage 缓存（caches: ["localStorage"]）
// 会把首次自动检测结果持久化成 i18nextLng，是「系统语言变化后不再跟随」（D1 差距）的根源；
// navigator 候选经 language-prefs 统一构造与归流（与原 detector + i18next 资源解析语义逐条
// 对齐，详见 language-prefs.detectSystemLanguage / navigatorLanguageCandidates），初始化永远
// 显式传 lng，changeLanguage 只改内存、不落盘（持久化唯一写点 = writeManualLanguage）。
const navigatorCandidates = navigatorLanguageCandidates();

i18n.use(initReactI18next).init({
  resources: {
    en: { translation: en },
    zh: { translation: zh },
  },
  lng: resolveInitialLanguage(navigatorCandidates),
  fallbackLng: "en",
  interpolation: {
    escapeValue: false,
  },
});

export default i18n;
