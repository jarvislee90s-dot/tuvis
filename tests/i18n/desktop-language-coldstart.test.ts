// tests/i18n/desktop-language-coldstart.test.ts — 桌面端冷启动语言判定场景测试（需求书 6.3）
// 用 vi.resetModules() + 动态 import 模拟「每次重启」：同一 i18n 模块在不同
// localStorage 状态 + 不同系统语言（navigator）组合下重新求值，断言初始界面语言与存储终态。
// 场景映射：6.3-1 首启归流 / 6.3-2 系统语言变化跟随（U8）/ 6.3-3 手动优先 /
// 6.3-5 最后一次手动为准（U14）/ C-2 无手动不落盘 / 旧版 i18nextLng 迁移。
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  LEGACY_LNG_CACHE_KEY,
  MANUAL_LANGUAGE_KEY,
  writeManualLanguage,
} from "@/i18n/language-prefs";

/** 覆盖 jsdom 的系统语言（navigator.languages / navigator.language 均按候选首项对齐）。 */
function setSystemLanguage(tags: string[]) {
  Object.defineProperty(window.navigator, "languages", {
    value: Object.freeze([...tags]),
    configurable: true,
  });
  Object.defineProperty(window.navigator, "language", {
    value: tags[0] ?? "en",
    configurable: true,
  });
}

/** 模拟一次应用冷启动：重置模块注册表后重新求值 @/i18n，等初始化完成返回 i18n 实例。 */
async function coldStart() {
  vi.resetModules();
  const mod = await import("@/i18n");
  const instance = mod.default;
  if (!instance.isInitialized) {
    await new Promise<void>((resolve) => {
      instance.on("initialized", () => resolve());
    });
  }
  return instance;
}

beforeEach(() => {
  localStorage.clear();
});

afterEach(() => {
  // 删除实例自身属性，恢复 jsdom 原型上的只读 getter，避免泄漏到后续用例
  Reflect.deleteProperty(window.navigator, "languages");
  Reflect.deleteProperty(window.navigator, "language");
  localStorage.clear();
  vi.resetModules();
});

describe("6.3-1 首次启动按系统语言归流", () => {
  it.each([[["zh-TW"]], [["zh-HK"]], [["zh"]], [["zh-CN"]], [["zh", "en"]]])(
    "系统语言 %j + 无手动记录 → 简体中文界面",
    async (tags) => {
      setSystemLanguage(tags);
      const i18n = await coldStart();
      expect(i18n.language).toBe("zh");
      expect(i18n.t("tray.show")).toBe("显示窗口");
    }
  );

  it.each([[["ja"]], [["de"]], [["en-US"]], [["fr"]], [["en", "ja"]], [["en-US", "zh-HK"]]])(
    "系统语言 %j + 无手动记录 → 英文界面",
    async (tags) => {
      setSystemLanguage(tags);
      const i18n = await coldStart();
      expect(i18n.language).toBe("en");
      expect(i18n.t("tray.show")).toBe("Show Window");
    }
  );
});

describe("6.3-2 未手动选择时跟随系统语言变化（U8 核心语义）", () => {
  it("日文系统首启为英文 → 改成繁中系统后重启跟随为中文", async () => {
    setSystemLanguage(["ja"]);
    const first = await coldStart();
    expect(first.language).toBe("en");

    // 用户改了系统语言，下次冷启动必须反映新语言（因为首启结果未被持久化）
    setSystemLanguage(["zh-TW"]);
    const second = await coldStart();
    expect(second.language).toBe("zh");
    expect(second.t("tray.show")).toBe("显示窗口");
  });

  it("繁中系统首启为中文 → 改成英文系统后重启跟随为英文", async () => {
    setSystemLanguage(["zh-HK"]);
    const first = await coldStart();
    expect(first.language).toBe("zh");

    setSystemLanguage(["en-US"]);
    const second = await coldStart();
    expect(second.language).toBe("en");
    expect(second.t("tray.show")).toBe("Show Window");
  });
});

describe("6.3-3 手动选择后系统语言变化不覆盖", () => {
  it("手动记录中文 + 系统英文 → 重启仍中文", async () => {
    writeManualLanguage("zh");
    setSystemLanguage(["en-US"]);
    const i18n = await coldStart();
    expect(i18n.language).toBe("zh");
    expect(i18n.t("tray.show")).toBe("显示窗口");
    // 手动记录不被判定过程改写
    expect(localStorage.getItem(MANUAL_LANGUAGE_KEY)).toBe("zh");
  });

  it("手动记录英文 + 系统繁中 → 重启仍英文", async () => {
    writeManualLanguage("en");
    setSystemLanguage(["zh-TW"]);
    const i18n = await coldStart();
    expect(i18n.language).toBe("en");
    expect(i18n.t("tray.show")).toBe("Show Window");
  });
});

describe("6.3-5 多次手动切换以最后一次为准（U14 / D2）", () => {
  it("中 → 英 → 中，重启后中文", async () => {
    writeManualLanguage("zh");
    writeManualLanguage("en");
    writeManualLanguage("zh");
    setSystemLanguage(["ja"]);
    const i18n = await coldStart();
    expect(i18n.language).toBe("zh");
  });

  it("中 → 英，重启后英文", async () => {
    writeManualLanguage("zh");
    writeManualLanguage("en");
    setSystemLanguage(["zh-CN"]);
    const i18n = await coldStart();
    expect(i18n.language).toBe("en");
    expect(i18n.t("tray.show")).toBe("Show Window");
  });
});

describe("C-2 不变量：无手动记录 → 不持久化任何语言值", () => {
  it("干净存储冷启动后，存储中既无旧版缓存键也无手动标记", async () => {
    setSystemLanguage(["zh-TW"]);
    await coldStart();
    expect(localStorage.getItem(LEGACY_LNG_CACHE_KEY)).toBeNull();
    expect(localStorage.getItem(MANUAL_LANGUAGE_KEY)).toBeNull();
    expect(localStorage.length).toBe(0);
  });

  it("旧版自动缓存键（i18nextLng）被视为非手动：不生效且启动即清除（迁移语义）", async () => {
    localStorage.setItem(LEGACY_LNG_CACHE_KEY, "zh-TW");
    setSystemLanguage(["en-US"]);
    const i18n = await coldStart();
    expect(i18n.language).toBe("en"); // 旧值不当作手动记录
    expect(localStorage.getItem(LEGACY_LNG_CACHE_KEY)).toBeNull(); // 已清理
  });
});
