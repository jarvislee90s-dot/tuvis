// tests/i18n/language-toggle.test.tsx — 手动切换入口的回归测试（需求书 6.3-4 / D2 / C-2）
// 覆盖：点击切换 → 手动标记落盘（唯一持久化写点）+ i18next languageChanged 事件触发
// （home.tsx 依赖 react-i18next 重渲染重建托盘的机制）+ refresh_tray 直接重建 +
// language-changed 跨窗口事件广播 + 连续切换最后一次生效 + C-2 不写旧版缓存键。
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { emit } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import { LanguageToggle } from "@/components/common/language-toggle";
// tests/setup.ts 未初始化 i18n，显式引入并固定中文（文案按 zh 断言，与 presetList 同法）
import i18n from "@/i18n";
import { LEGACY_LNG_CACHE_KEY, MANUAL_LANGUAGE_KEY } from "@/i18n/language-prefs";

beforeEach(async () => {
  localStorage.clear();
  vi.clearAllMocks();
  await i18n.changeLanguage("zh"); // 固定起点为中文（changeLanguage 只改内存，不落盘）
});

describe("手动切换入口（标题栏 + 设置页共用 LanguageToggle）", () => {
  it("点击切换：写手动标记 + 内存语言翻转 + languageChanged 事件触发", async () => {
    const languageChangedSpy = vi.fn();
    i18n.on("languageChanged", languageChangedSpy);

    render(<LanguageToggle />);
    fireEvent.click(screen.getByRole("button", { name: "切换语言" }));

    await waitFor(() => {
      expect(i18n.language).toBe("en");
    });
    // home.tsx 托盘重建链的第一环：react-i18next 因 languageChanged 重渲染（t 身份变化）
    expect(languageChangedSpy).toHaveBeenCalledWith("en");
    // C-2：手动标记 + 值落盘；且不产生旧版自动缓存键
    expect(localStorage.getItem(MANUAL_LANGUAGE_KEY)).toBe("en");
    expect(localStorage.getItem(LEGACY_LNG_CACHE_KEY)).toBeNull();

    i18n.off("languageChanged", languageChangedSpy);
  });

  it("点击切换：托盘即时重建（refresh_tray）并广播 language-changed 跨窗口事件（6.3-4 回归）", async () => {
    render(<LanguageToggle />);
    fireEvent.click(screen.getByRole("button", { name: "切换语言" }));

    await waitFor(() => {
      expect(invoke).toHaveBeenCalledWith(
        "refresh_tray",
        expect.objectContaining({ showText: "Show Window", presetsLabel: "Presets" })
      );
    });
    await waitFor(() => {
      expect(emit).toHaveBeenCalledWith("language-changed", { language: "en" });
    });
  });

  it("中 → 英 → 中 连续切换：手动标记以最后一次写入为准（U14）", async () => {
    render(<LanguageToggle />);
    const button = screen.getByRole("button", { name: "切换语言" });

    fireEvent.click(button); // zh → en（等内存语言翻转完成再点，避免两次异步切换交错）
    await waitFor(() => expect(i18n.language).toBe("en"));
    expect(localStorage.getItem(MANUAL_LANGUAGE_KEY)).toBe("en");

    fireEvent.click(button); // en → zh
    await waitFor(() => expect(i18n.language).toBe("zh"));
    expect(localStorage.getItem(MANUAL_LANGUAGE_KEY)).toBe("zh");
    expect(localStorage.getItem(LEGACY_LNG_CACHE_KEY)).toBeNull();
  });

  it("changeLanguage 不落盘：直接 changeLanguage（无手动入口参与）不写任何语言键（C-2）", async () => {
    // 例如 use-app-translation 收到跨窗口事件后的路径：本窗口未点切换，
    // 持久化已由源窗口的手动写入完成，本路径不得再产生写入
    await i18n.changeLanguage("en");
    expect(i18n.language).toBe("en");
    expect(localStorage.getItem(MANUAL_LANGUAGE_KEY)).toBeNull();
    expect(localStorage.getItem(LEGACY_LNG_CACHE_KEY)).toBeNull();
  });
});
