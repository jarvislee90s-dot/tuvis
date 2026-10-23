// tests/mobile/language-decision.test.ts — 移动端语言决策模块单元测试
// （三终端语言自适应 C-4/C-5/C-6/C-7/C-9，需求书 M1/M2/M7 + L1/L2 + 6.4 场景 1/4）。
//
// 覆盖映射（验收绑定清单）：
//  1. 6.4-1 连接跟随（C-6 第②级）：无手动 + 桌面 zh → zh；桌面切 en 后**下次连接** → en
//  2. 6.4-4 / L1 配对页（C-6 第③级）：zh-TW 浏览器 → zh；en 浏览器 → en（不依赖 /host）
//  3. 手动优先读路径（C-6 第①级）：账本预置 {language:"zh"} + 桌面 en → zh
//  4. 桌面语言不可得（null / 缺席 / 网络失败）→ 浏览器归流（第③级负向触发）
//  5. C-7 首帧闸：决策未定不放行、无中间翻面；超时 fail-open；手动缓存直通
//  6. C-9 / M7：生效语言 → html lang（含切换入口调用后）
//  7. C-6 四级优先序表驱动：manual > desktop > browser > en
//
// 纪律：全局约束 2（自动跟随结果绝不落「记住」存储）与 L2（重配清账收敛）各有专测。
// 模块持有会话级单例态（pinnedManual），逐用例 vi.resetModules() + 动态 import 重置
// （与 tests/i18n/desktop-language-coldstart.test.ts 同款手法）。
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { toast } from "sonner";
import { MANUAL_LANGUAGE_KEY, writeManualLanguage } from "@/i18n/language-prefs";

// 切换入口的失败暴露面走 sonner toast（App 根部 Toaster 承载）；本文件不渲染组件，
// 直接替身 toast 断言调用，免依赖挂载面
vi.mock("sonner", () => ({ toast: { error: vi.fn(), success: vi.fn() } }));

/** 覆盖 jsdom 的手机浏览器语言（navigator.languages / language 按候选首项对齐）。 */
function setBrowserLanguage(tags: string[]) {
  Object.defineProperty(window.navigator, "languages", {
    value: Object.freeze([...tags]),
    configurable: true,
  });
  Object.defineProperty(window.navigator, "language", {
    value: tags[0] ?? "en",
    configurable: true,
  });
}

/** /host 响应体（C-4 两字段 + host 必备键；fetchHost 只做 JSON 透传）。 */
function hostResponse(fields: { desktopLanguage?: unknown; manualLanguage?: unknown }): Response {
  return new Response(
    JSON.stringify({
      host: { name: "M", platform: "macos", version: "0.0.0", bootId: "b" },
      enabledTools: [],
      installedTools: [],
      ...fields,
    }),
    { status: 200 }
  );
}

type LanguageModule = typeof import("@/mobile/language");
type I18nModule = typeof import("@/i18n");

/** 模拟一次页面冷启动：重置模块注册表（清 pinnedManual 单例态 + i18n 重新初始化）。 */
async function coldStart(): Promise<{ lang: LanguageModule; i18n: I18nModule["default"] }> {
  vi.resetModules();
  const [langMod, i18nMod] = await Promise.all([import("@/mobile/language"), import("@/i18n")]);
  const instance = i18nMod.default;
  if (!instance.isInitialized) {
    await new Promise<void>((resolve) => {
      instance.on("initialized", () => resolve());
    });
  }
  return { lang: langMod, i18n: instance };
}

/** 推进微任务队列（闸挂起态断言前把 promise 链跑到位）。 */
const flushMicrotasks = () => new Promise<void>((resolve) => setTimeout(resolve, 0));

beforeEach(() => {
  localStorage.clear();
  document.documentElement.lang = "";
  vi.mocked(toast.error).mockClear();
});

afterEach(() => {
  cleanupNavigator();
  localStorage.clear();
  document.documentElement.lang = "";
  vi.unstubAllGlobals();
  vi.resetModules();
  vi.useRealTimers();
});

function cleanupNavigator() {
  Reflect.deleteProperty(window.navigator, "languages");
  Reflect.deleteProperty(window.navigator, "language");
}

// ==== 验收 7：C-6 决策链四级优先序（纯函数表驱动） ====
describe("resolveMobileLanguage C-6 优先序（manual > desktop > browser > en）", () => {
  it.each([
    // [说明, manual, desktop, browserCandidates, 期望]
    ["①手动 zh 压过 ②桌面 en 与 ③浏览器 zh", "zh", "en", ["zh"], "zh"],
    ["①手动 en 压过 ②桌面 zh 与 ③浏览器 en", "en", "zh", ["en"], "en"],
    ["②桌面 zh 压过 ③浏览器 en", null, "zh", ["en"], "zh"],
    ["②桌面 en 压过 ③浏览器 zh-TW", null, "en", ["zh-TW"], "en"],
    ["③浏览器 zh-TW 归流中文（无 ①②）", null, null, ["zh-TW"], "zh"],
    ["③浏览器 zh-HK 归流中文", null, null, ["ja", "zh-HK"], "zh"],
    ["③浏览器 en 归流英文", null, null, ["en-US"], "en"],
    ["③浏览器候选不可解析（ja）→ ④en 兜底", null, null, ["ja"], "en"],
    ["③浏览器候选为空 → ④en 兜底", null, null, [], "en"],
  ] as const)("%s", async (_desc, manual, desktop, browser, expected) => {
    const { lang } = await coldStart();
    expect(lang.resolveMobileLanguage({ manual, desktop, browserCandidates: browser })).toBe(
      expected
    );
  });
});

// ==== 验收 1：6.4-1 连接跟随（M1，C-6 第②级） ====
describe("applyConnectLanguage 连接跟随（6.4-1）", () => {
  it("无手动记录 + 桌面 zh 连接 → zh；桌面切 en 后**下次连接** → en（无需实时推送）", async () => {
    setBrowserLanguage(["en"]); // 浏览器语言与桌面相反，证明生效值来自桌面
    const { lang, i18n } = await coldStart();
    lang.initMobileLanguage(); // 启动：浏览器归流 en

    // 第一次连接：桌面 zh + 无手动记录 → 跟随桌面 → 中文
    expect(lang.applyConnectLanguage({ desktopLanguage: "zh", manualLanguage: null })).toBe("zh");
    expect(i18n.language).toBe("zh");
    expect(document.documentElement.lang).toBe("zh");

    // 桌面切到英文（无实时推送），手机下次连接（重新拉 /host）→ 英文
    expect(lang.applyConnectLanguage({ desktopLanguage: "en", manualLanguage: null })).toBe("en");
    expect(i18n.language).toBe("en");
    expect(document.documentElement.lang).toBe("en");
  });

  it("全局约束 2：连接跟随属自动判定，绝不写「记住」存储（本地手动键保持为空）", async () => {
    setBrowserLanguage(["en"]);
    const { lang } = await coldStart();
    lang.applyConnectLanguage({ desktopLanguage: "zh", manualLanguage: null });
    expect(localStorage.getItem(MANUAL_LANGUAGE_KEY)).toBeNull();
  });
});

// ==== 验收 2：6.4-4 / L1 配对页（C-6 第③级） ====
describe("initMobileLanguage 配对页启动归流（6.4-4 / L1）", () => {
  it.each([
    [["zh-TW"], "zh"],
    [["zh-HK"], "zh"],
    [["zh-CN"], "zh"],
    [["en"], "en"],
    [["en-US"], "en"],
    [["ja"], "en"],
  ])("未配对手机浏览器 %j → 配对页生效语言 %s（不依赖 /host）", async (tags, expected) => {
    setBrowserLanguage(tags);
    const fetchSpy = vi.fn();
    vi.stubGlobal("fetch", fetchSpy);
    const { lang, i18n } = await coldStart();
    expect(lang.initMobileLanguage()).toBe(expected);
    expect(i18n.language).toBe(expected);
    expect(document.documentElement.lang).toBe(expected);
    // 启动判定纯本地（缓存 + 浏览器候选），不发任何请求
    expect(fetchSpy).not.toHaveBeenCalled();
  });
});

// ==== 验收 3：手动优先读路径（C-6 第①级） ====
describe("手动记录优先读路径（C-6 第①级）", () => {
  it("账本预置 {language:zh} + 桌面 en → 手机 zh；桌面切换不再覆盖（§1.5 独立计算）", async () => {
    setBrowserLanguage(["en"]);
    const { lang, i18n } = await coldStart();
    // 记录预置语义 = 任务 2 写接口 POST /device-language {language:"zh"} 落账后，
    // /host 连接下发 manualLanguage:"zh"（端点到端到端的写→读闭环在
    // language-connect.test.tsx 以状态化账本夹具覆盖）
    expect(lang.applyConnectLanguage({ desktopLanguage: "en", manualLanguage: "zh" })).toBe("zh");
    expect(i18n.language).toBe("zh");
    expect(document.documentElement.lang).toBe("zh");

    // 桌面切换语言不作用于有手动记录的设备（§1.5 独立计算）
    expect(lang.applyConnectLanguage({ desktopLanguage: "zh", manualLanguage: "zh" })).toBe("zh");
  });
});

// ==== 验收 4：桌面语言不可得 → 浏览器归流（第③级负向触发） ====
describe("桌面语言不可得的负向触发（M2）", () => {
  it.each([
    ["desktopLanguage 显式 null", { desktopLanguage: null, manualLanguage: null }],
    ["desktopLanguage 缺席（旧版后端）", { manualLanguage: null }],
    [
      "desktopLanguage 非 zh/en 值（控制者裁决按不可得）",
      { desktopLanguage: "fr", manualLanguage: null },
    ],
    ["整个载荷为 null（403 未配对）", null],
  ] as const)("%s → 浏览器归流生效", async (_desc, payload) => {
    setBrowserLanguage(["zh-TW"]);
    const { lang } = await coldStart();
    expect(lang.applyConnectLanguage(payload)).toBe("zh");
    expect(document.documentElement.lang).toBe("zh");
  });

  it("/host 网络失败 → 首帧闸按浏览器语言 fail-open（不拒绝、不无限等待）", async () => {
    setBrowserLanguage(["zh-TW"]);
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => Promise.reject(new TypeError("network down")))
    );
    const { lang, i18n } = await coldStart();
    lang.initMobileLanguage();
    await expect(lang.resolveFirstLanguage({ timeoutMs: 500 })).resolves.toBe("zh");
    expect(i18n.language).toBe("zh");
  });
});

// ==== 验收 5：C-7 首帧闸 ====
describe("resolveFirstLanguage 首帧闸（C-7）", () => {
  it("决策未定期间闸不放行、无语言变更；放行即终值（启动路径无 A→B 可见翻面）", async () => {
    setBrowserLanguage(["zh-TW"]); // 启动临时值 zh，决策终值 en
    let resolveHost!: (r: Response) => void;
    vi.stubGlobal(
      "fetch",
      vi.fn(
        (_url: string) =>
          new Promise<Response>((resolve) => {
            resolveHost = resolve;
          })
      )
    );
    const { lang, i18n } = await coldStart();
    lang.initMobileLanguage(); // 启动：浏览器归流 zh（未渲染）
    const changes: string[] = [];
    i18n.on("languageChanged", (l) => changes.push(l));

    let settled: string | null = null;
    const gate = lang.resolveFirstLanguage({ timeoutMs: 5000 }).then((l) => {
      settled = l;
      return l;
    });
    await flushMicrotasks();
    // 闸未放行：main.tsx 在此之前不 render（不存在「英文先渲染」的窗口）
    expect(settled).toBeNull();
    expect(changes).toEqual([]); // 决策未定，零语言变更

    resolveHost(hostResponse({ desktopLanguage: "en", manualLanguage: null }));
    await gate;
    expect(settled).toBe("en");
    expect(changes).toEqual(["en"]); // 唯一一次变更发生在放行前——首帧即终值
    expect(i18n.language).toBe("en");
    expect(document.documentElement.lang).toBe("en");
  });

  it("超时 fail-open：/host 迟迟不回按浏览器语言放行（M2），迟到结果仍照常应用", async () => {
    vi.useFakeTimers();
    setBrowserLanguage(["zh-TW"]);
    let resolveHost!: (r: Response) => void;
    vi.stubGlobal(
      "fetch",
      vi.fn(
        (_url: string) =>
          new Promise<Response>((resolve) => {
            resolveHost = resolve;
          })
      )
    );
    const { lang, i18n } = await coldStart();
    lang.initMobileLanguage();
    const gate = lang.resolveFirstLanguage({ timeoutMs: 1000 });
    await vi.advanceTimersByTimeAsync(1000);
    await expect(gate).resolves.toBe("zh"); // 超时 → 浏览器语言放行

    // 迟到的 /host 结果不丢弃：照常应用（fail-open 的既定代价，与 Board 重评估同口径）
    resolveHost(hostResponse({ desktopLanguage: "en", manualLanguage: null }));
    await vi.advanceTimersByTimeAsync(0);
    expect(i18n.language).toBe("en");
    expect(document.documentElement.lang).toBe("en");
  });

  it("本地手动缓存（第①级启动已定案）→ 闸直通放行，不发 /host 请求（重连零等待）", async () => {
    setBrowserLanguage(["zh-TW"]); // 浏览器语言与手动缓存相反，证明生效值来自缓存
    localStorage.setItem(MANUAL_LANGUAGE_KEY, "en");
    const fetchSpy = vi.fn();
    vi.stubGlobal("fetch", fetchSpy);
    const { lang, i18n } = await coldStart();
    lang.initMobileLanguage();
    expect(i18n.language).toBe("en"); // i18n 冷启动（任务 1）已按同一缓存初始化
    await expect(lang.resolveFirstLanguage()).resolves.toBe("en");
    expect(fetchSpy).not.toHaveBeenCalled();
  });
});

// ==== 验收 6：C-9 / M7 html lang + 切换生效入口 ====
describe("switchMobileLanguage 切换生效入口（任务 4 消费面）", () => {
  it("立即生效（i18n + html lang）+ 钉住 + 本地缓存 + 桌面记账请求体逐字对齐 C-5", async () => {
    setBrowserLanguage(["zh-TW"]);
    const fetchSpy = vi.fn(
      async (_url: string, init?: RequestInit) =>
        new Response(JSON.stringify({ manual: true, language: init ? "en" : "zh" }), {
          status: 200,
        })
    );
    vi.stubGlobal("fetch", fetchSpy);
    const { lang, i18n } = await coldStart();
    lang.initMobileLanguage(); // 启动 zh

    await lang.switchMobileLanguage("en");

    expect(i18n.language).toBe("en"); // ① 立即生效
    expect(document.documentElement.lang).toBe("en"); // C-9 / M7
    expect(localStorage.getItem(MANUAL_LANGUAGE_KEY)).toBe("en"); // ② 钉住（仅手动值落盘）
    // ③ 桌面记账：POST /m/api/v1/device-language，载荷 {"language":"en"}（C-5 契约）
    const calls = fetchSpy.mock.calls.filter(([u]) => u === "/m/api/v1/device-language");
    expect(calls).toHaveLength(1);
    expect(calls[0]?.[1]?.method).toBe("POST");
    expect(JSON.parse(String(calls[0]?.[1]?.body))).toEqual({ language: "en" });
  });

  it("记账失败不静默假成功：经 formatInvokeError 双语体系 toast 暴露（g5），本会话切换不回滚", async () => {
    setBrowserLanguage(["zh-TW"]);
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => {
        throw new TypeError("network down");
      })
    );
    const { lang, i18n } = await coldStart();
    lang.initMobileLanguage();

    await lang.switchMobileLanguage("en"); // 不抛出（失败经 toast 呈现）

    expect(toast.error).toHaveBeenCalledTimes(1);
    const shown = vi.mocked(toast.error).mock.calls[0]?.[0];
    expect(typeof shown).toBe("string");
    // 原始码串不得裸露；呈现值 = formatInvokeError 产物（当前语言的双语模板，
    // 切换后 toast 以新语言呈现——与用户正在看的界面一致）
    expect(String(shown)).not.toContain("MOBILE_NETWORK_FAIL");
    expect(String(shown)).toBe(i18n.t("errors.mobileNetworkFail", { param: "device-language" }));
    // 本会话生效面不回滚：语言已切、钉住与缓存已写（账本由下次连接按真值收敛）
    expect(i18n.language).toBe("en");
    expect(localStorage.getItem(MANUAL_LANGUAGE_KEY)).toBe("en");
    expect(document.documentElement.lang).toBe("en");
  });
});

// ==== L2 / 缓存收敛 ====
describe("重配清账收敛（L2）与缓存镜像", () => {
  it("本地手动缓存 + 账本显式无记录（重新配对清账）→ 连接回到跟随，缓存一并清除", async () => {
    setBrowserLanguage(["en"]);
    writeManualLanguage("en"); // 预置本地手动缓存（旧设备的手动选择）
    const { lang, i18n } = await coldStart();
    lang.initMobileLanguage();
    expect(i18n.language).toBe("en"); // 启动按第①级缓存

    // 重配后账本已清（manualLanguage = null）→ 跟随桌面 zh，缓存清除
    expect(lang.applyConnectLanguage({ desktopLanguage: "zh", manualLanguage: null })).toBe("zh");
    expect(i18n.language).toBe("zh");
    expect(localStorage.getItem(MANUAL_LANGUAGE_KEY)).toBeNull();

    // 会话内再连接维持跟随（钉住已随清账解除，不再回翻 en）
    expect(lang.applyConnectLanguage({ desktopLanguage: "zh", manualLanguage: null })).toBe("zh");
  });

  it("账本有记录 → 本地缓存镜像之（下次冷启动第①级直通）；字段缺席（旧后端）不动缓存", async () => {
    setBrowserLanguage(["en"]);
    const { lang } = await coldStart();
    // 账本下发 en → 缓存镜像 en
    lang.applyConnectLanguage({ desktopLanguage: "zh", manualLanguage: "en" });
    expect(localStorage.getItem(MANUAL_LANGUAGE_KEY)).toBe("en");

    // 旧版后端两字段缺席：不知情不妄动（缓存保留，桌面语言按不可得 → 浏览器归流）
    lang.applyConnectLanguage({});
    expect(localStorage.getItem(MANUAL_LANGUAGE_KEY)).toBe("en");
    expect(document.documentElement.lang).toBe("en"); // 浏览器 en 归流，与缓存一致
  });
});

// ==== I1 回归（评审 fix round 1）：过期连接响应守卫（连接探测代数） ====
describe("过期连接响应守卫（I1：切换发生在 /host 在途期间）", () => {
  /** /host 挂起 + 记账端点立即成功的分流 stub（模拟慢隧道在途连接）。 */
  function stubHeldHost(ledgered: (r: (r: Response) => void) => void) {
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string): Promise<Response> => {
        if (url === "/m/api/v1/device-language") {
          return new Response(JSON.stringify({ manual: true, language: "en" }), { status: 200 });
        }
        return new Promise<Response>((resolve) => {
          ledgered(resolve);
        });
      })
    );
  }

  it("迟到的旧账（切换前串行化，manualLanguage null）不清钉住/缓存；新鲜连接照常清（守卫不过度保护）", async () => {
    setBrowserLanguage(["zh-TW"]); // 启动 zh
    const { lang, i18n } = await coldStart();
    lang.initMobileLanguage();
    let resolveHost!: (r: Response) => void;
    stubHeldHost((resolve) => (resolveHost = resolve));

    const inFlight = lang.fetchConnectPayload(); // 发出于切换前 → 代 0
    await lang.switchMobileLanguage("en"); // 钉住 en + 缓存 en + 记账 + 代数 → 1
    expect(i18n.language).toBe("en");

    // 放行迟到旧账（慢隧道在途窗口：响应先于切换串行化，账本 null + 桌面 zh）
    resolveHost(hostResponse({ desktopLanguage: "zh", manualLanguage: null }));
    const { generation, payload } = await inFlight;
    expect(lang.applyConnectLanguage(payload, generation)).toBe("en"); // 不回翻
    expect(localStorage.getItem(MANUAL_LANGUAGE_KEY)).toBe("en"); // 缓存幸存
    expect(document.documentElement.lang).toBe("en");

    // 对照（守卫只针对过期账面）：**新鲜**连接（切换后发出）账本 null → L2 清账语义照常
    expect(lang.applyConnectLanguage({ desktopLanguage: "zh", manualLanguage: null })).toBe("zh");
    expect(localStorage.getItem(MANUAL_LANGUAGE_KEY)).toBeNull();
    expect(document.documentElement.lang).toBe("zh");
  });

  it("过期账面的非 null 旧记录不镜像进缓存（不冲掉刚写的手动值）", async () => {
    setBrowserLanguage(["zh-TW"]);
    const { lang } = await coldStart();
    lang.initMobileLanguage(); // 启动 zh
    let resolveHost!: (r: Response) => void;
    stubHeldHost((resolve) => (resolveHost = resolve));

    const inFlight = lang.fetchConnectPayload(); // 代 0（账面旧值 zh）
    await lang.switchMobileLanguage("en"); // 切 en：缓存写 en，代数 → 1

    // 迟到旧账带非 null 旧记录 zh：钉住 en 让决策仍取 en，缓存也不得被旧账冲回 zh
    resolveHost(hostResponse({ desktopLanguage: "zh", manualLanguage: "zh" }));
    const { generation, payload } = await inFlight;
    expect(lang.applyConnectLanguage(payload, generation)).toBe("en");
    expect(localStorage.getItem(MANUAL_LANGUAGE_KEY)).toBe("en");
    expect(document.documentElement.lang).toBe("en");
  });
});
