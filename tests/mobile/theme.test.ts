// tests/mobile/theme.test.ts — P8f 日/夜双皮肤纯逻辑层（Task 4）
//
// 契约（brief 逐字 + 控制者裁决）：
// - getInitialTheme 优先级：localStorage 已存值 > matchMedia 系统偏好 > 默认 dark；
// - toggleTheme：翻转 → 写 localStorage → 切 documentElement 的 dark 类 → 返回新值；
// - applyInitialTheme：把「读初始主题 → 应用 documentElement 类」拆出供 main.tsx 调用；
// - localStorage / matchMedia 访问均须 try/catch（隐私模式 Safari 抛错、jsdom 无 matchMedia）。
//
// 注意：模块级不自动应用主题（applyInitialTheme 显式调用），因此测试可在导入后
// 自由构造 DOM 前置条件，不受导入副作用污染。
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { applyInitialTheme, getInitialTheme, toggleTheme } from "@/mobile/theme";

// jsdom 未实现 matchMedia：按需安装 shim（参考 tests/pet/petSettings.test.tsx:11 模式），
// 返回 matches 可控的实现，供「系统偏好」分支断言
function installMatchMedia(matches: boolean, opts?: { throwOnAccess?: boolean }) {
  if (opts?.throwOnAccess) {
    Object.defineProperty(window, "matchMedia", {
      configurable: true,
      writable: true,
      value: () => {
        throw new Error("matchMedia unavailable");
      },
    });
    return;
  }
  window.matchMedia = ((query: string) => ({
    matches,
    media: query,
    onchange: null,
    addListener: vi.fn(),
    removeListener: vi.fn(),
    addEventListener: vi.fn(),
    removeEventListener: vi.fn(),
    dispatchEvent: vi.fn(),
  })) as unknown as typeof window.matchMedia;
}

beforeEach(() => {
  localStorage.clear();
  document.documentElement.classList.remove("dark", "light");
  installMatchMedia(false); // 默认：系统非浅色偏好
});

afterEach(() => {
  document.documentElement.classList.remove("dark", "light");
});

describe("mobile theme：getInitialTheme 优先级", () => {
  it("localStorage saved 优先于 matchMedia：saved=light + 系统非浅色 → light", () => {
    installMatchMedia(false);
    localStorage.setItem("tuvis-theme", "light");
    expect(getInitialTheme()).toBe("light");
  });

  it("localStorage saved 优先于 matchMedia：saved=dark + 系统浅色 → dark", () => {
    installMatchMedia(true);
    localStorage.setItem("tuvis-theme", "dark");
    expect(getInitialTheme()).toBe("dark");
  });

  it("无 saved：跟随系统——浅色偏好 → light", () => {
    installMatchMedia(true);
    expect(getInitialTheme()).toBe("light");
  });

  it("无 saved：跟随系统——非浅色偏好 → dark", () => {
    installMatchMedia(false);
    expect(getInitialTheme()).toBe("dark");
  });

  it("无 saved 且 matchMedia 不存在（旧 WebView）：默认 dark 不抛错", () => {
    // 删除 shim：theme.ts 必须容错（try/catch），回退 dark
    // @ts-expect-error 测试刻意移除 jsdom 缺失的 API
    delete window.matchMedia;
    expect(getInitialTheme()).toBe("dark");
  });

  it("saved 非法值（既非 light 也非 dark）：按无 saved 处理", () => {
    installMatchMedia(true);
    localStorage.setItem("tuvis-theme", "blue");
    expect(getInitialTheme()).toBe("light");
  });

  it("localStorage 读取抛错（隐私模式 Safari）：回退 matchMedia 分支不抛错", () => {
    installMatchMedia(true);
    const spy = vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new Error("SecurityError");
    });
    expect(getInitialTheme()).toBe("light");
    spy.mockRestore();
  });
});

describe("mobile theme：toggleTheme 翻转 + 持久化 + 类切换", () => {
  it("当前 dark → 翻转为 light：写 tuvis-theme=light 且移除 documentElement.dark 类", () => {
    document.documentElement.classList.add("dark");
    expect(toggleTheme()).toBe("light");
    expect(localStorage.getItem("tuvis-theme")).toBe("light");
    expect(document.documentElement.classList.contains("dark")).toBe(false);
  });

  it("当前 light → 翻转为 dark：写 tuvis-theme=dark 且添加 documentElement.dark 类", () => {
    installMatchMedia(true); // 系统浅色且无 saved ⇒ 当前 light
    expect(toggleTheme()).toBe("dark");
    expect(localStorage.getItem("tuvis-theme")).toBe("dark");
    expect(document.documentElement.classList.contains("dark")).toBe(true);
  });

  it("连续两次 toggle 回到原主题（幂等翻转）", () => {
    document.documentElement.classList.add("dark");
    toggleTheme();
    expect(toggleTheme()).toBe("dark");
    expect(document.documentElement.classList.contains("dark")).toBe(true);
  });

  it("localStorage 写入抛错（隐私模式）：类切换仍生效，不抛错", () => {
    const spy = vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("QuotaExceededError");
    });
    document.documentElement.classList.add("dark");
    expect(toggleTheme()).toBe("light");
    expect(document.documentElement.classList.contains("dark")).toBe(false);
    spy.mockRestore();
  });

  it("localStorage 写入持续抛错：连续 toggle 仍能往返，不单向锁死", () => {
    // 终审 Important 3：写失败被吞后 getInitialTheme() 恒回落系统偏好（无 saved），
    // 旧实现连续点击恒产同一 next（dark→light 后永远切不回）。修复后 next 从
    // documentElement 当前生效态推导，写失败也自愈可往返。
    // spy 在 finally 里恢复：红态断言失败时不得让 mock 泄漏污染后续用例
    const spy = vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("QuotaExceededError");
    });
    try {
      document.documentElement.classList.add("dark");
      expect(toggleTheme()).toBe("light");
      expect(document.documentElement.classList.contains("dark")).toBe(false);
      expect(toggleTheme()).toBe("dark");
      expect(document.documentElement.classList.contains("dark")).toBe(true);
      expect(toggleTheme()).toBe("light");
      expect(document.documentElement.classList.contains("dark")).toBe(false);
    } finally {
      spy.mockRestore();
    }
  });
});

describe("mobile theme：applyInitialTheme 初始应用", () => {
  it("初始 dark：加 documentElement.dark 类", () => {
    localStorage.setItem("tuvis-theme", "dark");
    applyInitialTheme();
    expect(document.documentElement.classList.contains("dark")).toBe(true);
  });

  it("初始 light：移除 documentElement.dark 类", () => {
    installMatchMedia(true);
    applyInitialTheme();
    expect(document.documentElement.classList.contains("dark")).toBe(false);
  });

  it("重复调用幂等（切换后再调用按当前存储值收敛）", () => {
    localStorage.setItem("tuvis-theme", "dark");
    applyInitialTheme();
    toggleTheme(); // → light
    applyInitialTheme();
    expect(document.documentElement.classList.contains("dark")).toBe(false);
  });
});

// mobile.html 防闪白内联脚本 + mobile.css dark 变体声明：
// 这两处是「手工同步」的字符串（不在 TS 类型系统与测试运行时内），一旦漂移会造成
// 首帧闪白（脚本）或手动切换成死功能（CSS），按控制者裁决必须锁一致性。
// 内联脚本以真实文本提取后执行（非重写副本）——锁的是实际出货内容。
describe("mobile theme：mobile.html 内联脚本与 mobile.css 策略一致性", () => {
  const html = readFileSync(resolve(__dirname, "../../mobile.html"), "utf8");
  const css = readFileSync(resolve(__dirname, "../../src/mobile/mobile.css"), "utf8");

  // 提取 <head> 内的内联脚本正文（构建产物同款：vite 不改写内联脚本内容）
  function inlineScript(): string {
    const m = html.match(/<script>([\s\S]*?)<\/script>/);
    if (!m) throw new Error("mobile.html 未找到内联防闪白脚本");
    return m[1];
  }

  // 在 jsdom 全局上执行真实内联脚本（document / localStorage / matchMedia 均为全局）
  function runInlineScript() {
    new Function(inlineScript())();
  }

  it("mobile.css 显式声明 dark 变体为类策略（Tailwind v4 默认 media 查询，不声明则手动切换死功能）", () => {
    expect(css).toContain("@custom-variant dark (&:where(.dark, .dark *));");
  });

  it("内联脚本与 theme.ts 默认分支逐例同源（saved 优先 > 系统偏好 > 默认 dark）", () => {
    // 用例限定在「应用自身可写入的值」（light / dark / 无值）：两处写路径
    // （theme.ts toggleTheme 与本脚本）只会产生这三种状态。
    // 已知且可接受的边界差异（brief 明文要求内联脚本逐字落，不得改动）：
    // localStorage 被外部写入非法值（如 "blue"）时，内联脚本走 else 落到 dark
    // （`!t` 对非空字符串为 false），theme.ts 则把非法值当未设置、回落系统偏好。
    // 影响 = 首帧暗一帧后由 main.tsx 的 applyInitialTheme 立即校正，且应用自身
    // 产生不出该状态；已在 task-4-report.md 记录为已知边界。
    const cases: Array<{ saved: string | null; prefersLight: boolean }> = [
      { saved: "light", prefersLight: false },
      { saved: "dark", prefersLight: true },
      { saved: null, prefersLight: true },
      { saved: null, prefersLight: false },
    ];
    for (const { saved, prefersLight } of cases) {
      // 内联脚本（首帧防闪白）结果
      localStorage.clear();
      document.documentElement.classList.remove("dark");
      if (saved !== null) localStorage.setItem("tuvis-theme", saved);
      installMatchMedia(prefersLight);
      runInlineScript();
      const inlineDark = document.documentElement.classList.contains("dark");
      // theme.ts（权威应用点）结果
      localStorage.clear();
      document.documentElement.classList.remove("dark");
      if (saved !== null) localStorage.setItem("tuvis-theme", saved);
      installMatchMedia(prefersLight);
      applyInitialTheme();
      const themeTsDark = document.documentElement.classList.contains("dark");
      expect(
        { saved, prefersLight, inlineDark },
        `内联脚本与 theme.ts 对 ${JSON.stringify({ saved, prefersLight })} 的判定应一致`
      ).toEqual({ saved, prefersLight, inlineDark: themeTsDark });
    }
  });

  it("内联脚本 catch 分支：localStorage 抛错（隐私模式）时回退 dark", () => {
    document.documentElement.classList.remove("dark");
    const spy = vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new Error("SecurityError");
    });
    runInlineScript();
    expect(document.documentElement.classList.contains("dark")).toBe(true);
    spy.mockRestore();
  });

  it("body 底色双态：消费语义 Token --pg（2026-10-05 改版，不再硬编码 slate 黑底）", () => {
    expect(html).toContain('class="bg-[var(--pg)]"');
  });
});
