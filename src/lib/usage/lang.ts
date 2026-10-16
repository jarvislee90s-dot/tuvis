// 语言判定（issue #160 P1-1 泛化，原「评语池语言判定」计划② Task 12 步骤 3）——
// 界面语言 → 二值桶（zh/en）的**唯一**出处。消费方 = `quotePool.ts`（评语池）与
// 用量格式化层（locale 参数化）；导出层不引 React，故这里只依赖 i18n 单例本身
// （`src/i18n/index.ts`，`fallbackLng: "en"`）。
//
// 为什么要有这一层：评语池是**文案资产**（不进 i18n 键表，见 `quotePool.ts` 文件头），
// 它必须自己知道当前说什么语言；`i18n.language` 形如 `zh` / `zh-CN` / `en-US`，与两个桶对不齐。
import i18n from "@/i18n";

/** 语言二值桶（与 `i18n` 的 `resources` key 同名同义）。评语池与用量格式化共用，本文件是唯一出处。 */
export type Lang = "zh" | "en";

/**
 * 界面语言字符串 → 二值桶（纯函数，issue #160）：`zh*` 一律归 `zh`，其余一律归 `en`
 * （与 `fallbackLng: "en"` 同口径）。**方向警示**：不要写成 `startsWith("en") ? "en" : "zh"`——
 * 首次运行无 localStorage 时第三语言（ja/de…）会掉进 zh 桶，英文界面出万/亿混排。
 */
export function langOf(language: string | null | undefined): Lang {
  return (language ?? "").toLowerCase().startsWith("zh") ? "zh" : "en";
}

/**
 * 当前界面语言 → 桶语言：`zh*` 一律归 `zh`（`zh-CN` / `zh-TW` 都是中文），其余一律 `en`
 * （与 `fallbackLng: "en"` 同口径，绝不因第三种语言而掉进「没有池」的空档）。
 * 无参便捷形式，读 i18n 单例；既有消费方零改动。
 */
export function getLang(): Lang {
  return langOf(i18n.language);
}
