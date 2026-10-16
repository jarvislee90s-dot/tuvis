// issue #160 · P1-1：语言谓词唯一出处 lang.ts 的泛化行为锁。
// 方向用例（ja/de）是本步的灵魂：fallbackLng 是 en，第三语言绝不能掉进 zh 桶。
import { describe, expect, it } from "vitest";
import { getLang, langOf } from "@/lib/usage/lang";

describe("lang.ts 泛化（issue #160 P1-1）", () => {
  it("langOf：zh 系一律归 zh（大小写不敏感）", () => {
    expect(langOf("zh")).toBe("zh");
    expect(langOf("zh-CN")).toBe("zh");
    expect(langOf("zh-TW")).toBe("zh");
    expect(langOf("ZH-cn")).toBe("zh");
  });
  it("langOf：en 与第三语言一律归 en（fallbackLng 同口径）——谓词方向的关键用例", () => {
    expect(langOf("en")).toBe("en");
    expect(langOf("en-US")).toBe("en");
    expect(langOf("ja")).toBe("en");
    expect(langOf("de")).toBe("en");
    expect(langOf("")).toBe("en");
  });
  it("langOf：空值容错", () => {
    expect(langOf(undefined)).toBe("en");
    expect(langOf(null)).toBe("en");
  });
  it("getLang：读 i18n 单例，返回两桶之一", () => {
    expect(["zh", "en"]).toContain(getLang());
  });
});
