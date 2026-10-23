// 桌面端语言偏好（需求书 C-2 持久化语义）——「自动跟随 / 手动覆盖」二值语义的唯一出处：
// 1. 未手动选择过 → **不持久化任何语言值**，每次冷启动按系统语言重新归流检测
//    （系统语言变化后，应用下次启动即跟随新语言，U8 / D1）；
// 2. 手动选择 → 写「手动标记 + 值」（键存在即手动标记，键值即所选语言），
//    再次切换覆盖前值（最后一次写入生效，U14 / D2）；此后系统语言变化不再覆盖；
// 3. 旧版键 `i18nextLng`（i18next LanguageDetector 的默认 localStorage 缓存键）无法区分
//    「自动检测结果」与「手动选择」，按裁决一律视为**非手动**：启动时清除、此后永不读写
//    （已接受代价：升级前手动选过语言的用户会回落到系统语言一次）。

/** 手动语言记录键（marker + value 一体：键存在 = 手动选择过；值 = "zh" | "en"）。 */
export const MANUAL_LANGUAGE_KEY = "tuvis.language.manual";

/** 旧版 i18next LanguageDetector 默认缓存键（历史遗留，只清不读）。 */
export const LEGACY_LNG_CACHE_KEY = "i18nextLng";

/** 界面语言二值桶（C-1：与 i18n `resources` key 同名同义，值集二值）。 */
export type UiLanguage = "zh" | "en";

function isUiLanguage(value: string | null): value is UiLanguage {
  return value === "zh" || value === "en";
}

/**
 * 语言归流（D4），与旧版 i18next detector + 资源解析语义逐条对齐：
 * 按候选顺序扫描，**首个可解析候选定胜负**——主子标签为 zh（zh / zh-CN / zh-TW / zh-HK /
 * zh-SG / zh-MO / zh-Hant-* …）→ 中文；主子标签为 en → 英文；其余语言（ja / de …）
 * 在二值资源下不可解析 → 跳过继续；扫描完无可解析候选 → 英文兜底（fallbackLng 同口径）。
 * 候选顺序即用户偏好优先级（navigator.languages 逐项在前、navigator.language 兜底），
 * 不得让低优先级候选覆盖高优先级（例：["en-US","zh-HK"] 是英文优先 → 英文）。
 */
export function detectSystemLanguage(
  candidates: readonly (string | undefined | null)[] | undefined
): UiLanguage {
  for (const tag of candidates ?? []) {
    if (!tag) continue;
    const primary = tag.toLowerCase().split("-")[0];
    if (primary === "zh") return "zh";
    if (primary === "en") return "en";
    // 其余主子标签（ja / de / fr …）不可解析：跳过，继续看下一候选
  }
  return "en";
}

/**
 * 浏览器语言候选（桌面冷启动与移动端决策链共用，保证「候选顺序 = 用户偏好优先级」
 * 两端口径同源）：navigator.languages 逐项在前、navigator.language 兜底；
 * 无 navigator 环境（SSR / 测试注入前）→ 空数组（归流自然落到 en 兜底）。
 */
export function navigatorLanguageCandidates(): (string | undefined)[] {
  if (typeof navigator === "undefined") return [];
  return [
    ...(Array.isArray(navigator.languages) ? navigator.languages : []),
    ...(navigator.language ? [navigator.language] : []),
  ];
}

/**
 * 读手动记录：键存在且值合法 → 所选语言；键缺失或脏值 → null（视同从未手动选择）。
 * 存储不可用（异常环境）同样降级为「从未手动选择」，绝不阻塞启动。
 */
export function readManualLanguage(storage?: Pick<Storage, "getItem">): UiLanguage | null {
  try {
    const raw = (storage ?? localStorage).getItem(MANUAL_LANGUAGE_KEY);
    return isUiLanguage(raw) ? raw : null;
  } catch (_error) {
    return null;
  }
}

/**
 * 写手动记录（手动切换入口唯一写点；每次切换调用，最后一次写入生效）。
 * 存储写入失败（异常环境）不抛出——语言仍会在内存中切换，只是这次重启后回到自动跟随。
 */
export function writeManualLanguage(lang: UiLanguage, storage?: Pick<Storage, "setItem">): void {
  try {
    (storage ?? localStorage).setItem(MANUAL_LANGUAGE_KEY, lang);
  } catch (_error) {
    // 存储不可用：放弃持久化，保持本次会话内语言已切换
  }
}

/**
 * 清除手动记录（远程端重配收敛用，三终端语言自适应 L2）：账本明确「该设备无手动记录」
 * 时调用，保证本地下一次冷启动不再把已失效的手动值当第①级。键本就不存在时无害。
 * 存储不可用（异常环境）不抛出——清不掉只是多一次回落，不阻塞调用方。
 */
export function clearManualLanguage(storage?: Pick<Storage, "removeItem">): void {
  try {
    (storage ?? localStorage).removeItem(MANUAL_LANGUAGE_KEY);
  } catch (_error) {
    // 存储不可用：忽略（与 writeManualLanguage 的降级口径一致）
  }
}

/** 清除旧版自动缓存键（视为非手动，启动时一次性清理，防历史脏数据残留）。 */
export function clearLegacyLanguageCache(storage?: Pick<Storage, "removeItem">): void {
  try {
    (storage ?? localStorage).removeItem(LEGACY_LNG_CACHE_KEY);
  } catch (_error) {
    // 存储不可用：忽略
  }
}

/**
 * 冷启动初始语言（唯一判定入口，D1 + D2）：
 * 有合法手动记录 → 手动值（手动优先级最高）；否则清掉旧版缓存键后按系统语言归流检测。
 * C-2 不变量：无手动记录时本模块不写任何语言值进存储（只删旧键，不写新键）。
 */
export function resolveInitialLanguage(
  systemCandidates: readonly (string | undefined | null)[] | undefined,
  storage?: Pick<Storage, "getItem" | "removeItem">
): UiLanguage {
  // storage 缺省时透传 undefined，由子函数在各自 try 内取 localStorage（本函数不直接触碰，
  // 存储访问异常在子函数内降级，维持「绝不阻塞启动」承诺）
  const manual = readManualLanguage(storage);
  if (manual) return manual;
  clearLegacyLanguageCache(storage);
  return detectSystemLanguage(systemCandidates);
}
