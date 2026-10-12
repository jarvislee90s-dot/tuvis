// tests/resourceUiI18n.test.ts — 资源管理页 UI 改版 i18n 键在场锁（zh/en 双语）
// 覆盖：resources.*（竖排全部启用钮 / 来源徽章三态）与 presets.*（两列选择区 / 预览）
// 新键在两份语言包中同时存在且非空
import { describe, expect, it } from "vitest";
import zh from "@/i18n/locales/zh.json";
import en from "@/i18n/locales/en.json";

const RESOURCE_KEYS = [
  "allPrefix",
  "allOnAction",
  "allOffAction",
  "sourceNative",
  "sourceRepo",
  "sourceAgents",
  "sourceAgentsHint",
] as const;

const PRESET_KEYS = [
  "availableResources",
  "selectedResources",
  "noSelectionHint",
  "previewTitle",
  "previewPreset",
  "editPreset",
  "removeSelection",
] as const;

describe("资源管理页 UI 改版 i18n 新键（zh/en 双语在场）", () => {
  it.each(RESOURCE_KEYS)("resources.%s 在 zh/en 均存在且非空", (key) => {
    expect(Boolean((zh.resources as Record<string, string>)[key])).toBe(true);
    expect(Boolean((en.resources as Record<string, string>)[key])).toBe(true);
  });

  it.each(PRESET_KEYS)("presets.%s 在 zh/en 均存在且非空", (key) => {
    expect(Boolean((zh.presets as Record<string, string>)[key])).toBe(true);
    expect(Boolean((en.presets as Record<string, string>)[key])).toBe(true);
  });
});
