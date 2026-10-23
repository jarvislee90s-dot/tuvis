// tests/i18n/language-prefs.test.ts — 桌面端语言偏好纯函数单元测试（需求书 C-2 / D1 / D2 / D4）
// 覆盖：归流判定（zh* 系 → zh，其余 → en）、手动标记读写与脏值降级、
// 旧版 i18nextLng 缓存键清理、resolveInitialLanguage 优先级与「无手动不落盘」不变量。
import { beforeEach, describe, expect, it } from "vitest";
import {
  LEGACY_LNG_CACHE_KEY,
  MANUAL_LANGUAGE_KEY,
  clearLegacyLanguageCache,
  clearManualLanguage,
  detectSystemLanguage,
  readManualLanguage,
  resolveInitialLanguage,
  writeManualLanguage,
} from "@/i18n/language-prefs";

// 内存版 Storage 桩：隔离于 jsdom localStorage，免受模块级 i18n 初始化副作用影响
function fakeStorage(initial: Record<string, string> = {}) {
  const map = new Map(Object.entries(initial));
  return {
    getItem: (key: string) => (map.has(key) ? (map.get(key) as string) : null),
    setItem: (key: string, value: string) => void map.set(key, value),
    removeItem: (key: string) => void map.delete(key),
  };
}

beforeEach(() => {
  localStorage.clear();
});

describe("detectSystemLanguage 归流（D4，首个可解析候选优先）", () => {
  it.each([
    ["zh"],
    ["zh-CN"],
    ["zh-SG"],
    ["zh-TW"],
    ["zh-HK"],
    ["zh-MO"],
    ["ZH-TW"], // 大小写不敏感
    ["zh-HK", "en"], // 首个可解析候选是 zh 系 → 中文
    ["ja-JP", "zh-TW"], // ja 不可解析跳过 → 次选 zh 系生效
  ])("%j → zh", (...candidates) => {
    expect(detectSystemLanguage(candidates as string[])).toBe("zh");
  });

  it.each([
    ["en"],
    ["en-US"],
    ["ja"],
    ["de"],
    ["fr"],
    ["pt-BR"],
    [],
    ["en-US", "zh-HK"], // 英文优先的偏好序 → 英文（zh 系在后不得覆盖，评审 Important #1）
    ["en", "zh-HK"], // 首个可解析候选是 en → 英文
    ["ja", "en"], // ja 不可解析跳过 → 次选 en 生效
    ["de", "ja"], // 全部不可解析 → en 兜底
  ])("%j → en", (...candidates) => {
    expect(detectSystemLanguage(candidates as string[])).toBe("en");
  });

  it("候选缺失（undefined / null 元素）→ en 兜底", () => {
    expect(detectSystemLanguage(undefined)).toBe("en");
    expect(detectSystemLanguage([undefined, null])).toBe("en");
  });
});

describe("手动标记存储（C-2：marker + value）", () => {
  it("写入后可读出所选语言", () => {
    const storage = fakeStorage();
    writeManualLanguage("en", storage);
    expect(readManualLanguage(storage)).toBe("en");
  });

  it("再次写入覆盖前值（最后一次写入生效，U14）", () => {
    const storage = fakeStorage();
    writeManualLanguage("en", storage);
    writeManualLanguage("zh", storage);
    expect(readManualLanguage(storage)).toBe("zh");
  });

  it("键不存在 → null（从未手动选择）", () => {
    expect(readManualLanguage(fakeStorage())).toBeNull();
  });

  it("脏值降级为 null（值集二值 C-1，视同从未手动选择）", () => {
    const storage = fakeStorage({ [MANUAL_LANGUAGE_KEY]: "fr" });
    expect(readManualLanguage(storage)).toBeNull();
  });

  // 三终端语言自适应 L2：远程端重配清账时清本地镜像（清不存在的键无害）
  it("clearManualLanguage 清除手动记录；键本就不存在时幂等无害", () => {
    const storage = fakeStorage({ [MANUAL_LANGUAGE_KEY]: "en" });
    clearManualLanguage(storage);
    expect(readManualLanguage(storage)).toBeNull();
    expect(() => clearManualLanguage(fakeStorage())).not.toThrow();
  });
});

describe("旧版 i18nextLng 缓存键", () => {
  it("清除旧版缓存键", () => {
    const storage = fakeStorage({ [LEGACY_LNG_CACHE_KEY]: "zh-TW" });
    clearLegacyLanguageCache(storage);
    expect(storage.getItem(LEGACY_LNG_CACHE_KEY)).toBeNull();
  });
});

describe("resolveInitialLanguage（冷启动唯一判定入口）", () => {
  it("有手动记录 → 手动值优先（系统语言不参与判定）", () => {
    const storage = fakeStorage({ [MANUAL_LANGUAGE_KEY]: "zh" });
    expect(resolveInitialLanguage(["en-US"], storage)).toBe("zh");
    expect(resolveInitialLanguage(["ja"], storage)).toBe("zh");
  });

  it("无手动记录 → 按系统语言归流", () => {
    expect(resolveInitialLanguage(["zh-TW"], fakeStorage())).toBe("zh");
    expect(resolveInitialLanguage(["de-DE"], fakeStorage())).toBe("en");
  });

  it("旧版缓存键不能当作手动记录（裁决：一律非手动）——有旧键 + 英文系统 → 英文，且旧键被清", () => {
    const storage = fakeStorage({ [LEGACY_LNG_CACHE_KEY]: "zh" });
    expect(resolveInitialLanguage(["en-US"], storage)).toBe("en");
    expect(storage.getItem(LEGACY_LNG_CACHE_KEY)).toBeNull();
  });

  it("C-2 不变量：无手动记录时判定过程只删旧键、不写任何语言值进存储", () => {
    const storage = fakeStorage({ [LEGACY_LNG_CACHE_KEY]: "zh-TW" });
    resolveInitialLanguage(["zh-TW"], storage);
    expect(storage.getItem(LEGACY_LNG_CACHE_KEY)).toBeNull();
    expect(storage.getItem(MANUAL_LANGUAGE_KEY)).toBeNull();
  });

  it("有手动记录时旧版缓存键同样被无视（手动值生效）", () => {
    const storage = fakeStorage({
      [MANUAL_LANGUAGE_KEY]: "en",
      [LEGACY_LNG_CACHE_KEY]: "zh",
    });
    expect(resolveInitialLanguage(["zh-CN"], storage)).toBe("en");
  });
});
