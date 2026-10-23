// tests/mobile/language-connect.test.tsx — 移动端语言决策「连接时重评估」集成测试
// （三终端语言自适应：Board 挂载拉 /host 消费 C-4 两字段 → 决策链重评估）。
//
// 与 language-decision.test.ts（决策模块纯单元）分工：本文件锁的是**接线**——
// Board 的 /host 挂载钩子真的把 desktopLanguage / manualLanguage 喂给了决策模块：
//  - 6.4-1（M1）：无手动记录 + 桌面 zh 连接 → zh；桌面切 en 后**下次连接**（重新挂载
//    拉 /host）→ en，无需实时推送；
//  - 手动优先读路径（C-6 第①级）：经任务 2 写接口（POST /device-language）向**状态化
//    账本**预置 {language:"zh"}，桌面 en 连接 → 手机 zh（写 → 账 → 连接读全链路）；
//  - /host 403（设备失效）不触发重评估：启动的浏览器归流语言继续生效（M2 fail-open）。
//
// 模块单例态（pinnedManual / i18n 语言）逐用例 vi.resetModules() + 动态 import 重置；
// fetch 全量 stub（盖过 setup.ts 的 msw，tests/mobile 惯例）；SSE 走 MockEventSource。
import { act, cleanup, render } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { MockEventSource } from "./eventSourceMock";
import { MANUAL_LANGUAGE_KEY } from "@/i18n/language-prefs";
import type { SessionsResponse } from "@/types/session";

/** 覆盖 jsdom 的手机浏览器语言。 */
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

function okSessions(totalCount = 0): SessionsResponse {
  return { sessions: [], totalCount, waitingCount: 0 };
}

/** 安装脚本化 EventSource：连接建立即送达首帧快照（模拟服务端首帧，Board.test 同款）。 */
function installSse(snapshot: SessionsResponse) {
  class ScriptedEventSource extends MockEventSource {
    constructor(url: string) {
      super(url);
      setTimeout(() => this.emit("snapshot", snapshot), 0);
    }
  }
  vi.stubGlobal("EventSource", ScriptedEventSource);
}

/** 状态化账本 + /host 夹具：POST /device-language 落账（任务 2 写接口语义），
 *  GET /host 按账返回 manualLanguage、按夹具返回 desktopLanguage。 */
function stubStatefulFetch(desktopLanguage: unknown) {
  let ledger: "zh" | "en" | null = null;
  return {
    /** 仅供断言读取当前账面 */
    peek: () => ledger,
    stub: vi.fn(async (url: string, init?: RequestInit) => {
      if (url === "/m/api/v1/device-language") {
        ledger = (JSON.parse(String(init?.body)) as { language: "zh" | "en" }).language;
        return new Response(JSON.stringify({ manual: true, language: ledger }), { status: 200 });
      }
      if (url === "/m/api/v1/host") {
        return new Response(
          JSON.stringify({
            host: { name: "M", platform: "macos", version: "0.0.0", bootId: "b" },
            enabledTools: [],
            installedTools: [],
            desktopLanguage,
            manualLanguage: ledger,
          }),
          { status: 200 }
        );
      }
      // /sessions 与其余端点：空看板成功载荷
      return new Response(JSON.stringify(okSessions(0)), { status: 200 });
    }),
  };
}

/** 全 403 stub：设备失效面（host / sessions 均拒）。 */
function stubAllForbidden() {
  return vi.fn(async () => new Response("", { status: 403 }));
}

type Deps = Awaited<ReturnType<typeof setup>>;

/** 模拟一次页面冷启动：重置模块注册表后重新取 Board / api / language / i18n 同源模块。 */
async function setup() {
  vi.resetModules();
  const [api, lang, i18nMod, boardMod] = await Promise.all([
    import("@/mobile/api"),
    import("@/mobile/language"),
    import("@/i18n"),
    import("@/mobile/Board"),
  ]);
  const instance = i18nMod.default;
  if (!instance.isInitialized) {
    await new Promise<void>((resolve) => {
      instance.on("initialized", () => resolve());
    });
  }
  return { api, lang, Board: boardMod.default, i18n: instance };
}

function renderBoard(deps: Deps, callbacks: { onPaired?: () => void; onUnpaired?: () => void }) {
  return render(
    <deps.Board
      onPaired={callbacks.onPaired ?? vi.fn()}
      onUnpaired={callbacks.onUnpaired ?? vi.fn()}
      onOpenHistory={vi.fn()}
    />
  );
}

/** fake timers 下推进定时器并让微任务落地（Board.test 同款）。 */
async function advance(ms: number) {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(ms);
  });
}

beforeEach(() => {
  vi.useFakeTimers();
  MockEventSource.reset();
  localStorage.clear();
  document.documentElement.lang = "";
});

afterEach(() => {
  cleanup();
  vi.useRealTimers();
  vi.unstubAllGlobals();
  Reflect.deleteProperty(window.navigator, "languages");
  Reflect.deleteProperty(window.navigator, "language");
  localStorage.clear();
  document.documentElement.lang = "";
  vi.resetModules();
});

describe("Board 连接时语言重评估（C-4 消费接线）", () => {
  it("6.4-1：无手动记录 + 桌面 zh 连接 → zh；桌面切 en 后下次连接（重新挂载）→ en", async () => {
    setBrowserLanguage(["en"]); // 浏览器与桌面相反，证明生效值来自桌面
    installSse(okSessions(0));
    const deps = await setup();

    // 第一次连接：桌面 zh + 账本无记录 → 跟随桌面 → 中文
    vi.stubGlobal("fetch", stubStatefulFetch("zh").stub);
    deps.lang.initMobileLanguage(); // 启动：浏览器归流 en（main.tsx 同款调用）
    renderBoard(deps, {});
    await advance(0);
    expect(deps.i18n.language).toBe("zh");
    expect(document.documentElement.lang).toBe("zh");
    // 自动跟随不落「记住」存储（全局约束 2）
    expect(localStorage.getItem(MANUAL_LANGUAGE_KEY)).toBeNull();
    cleanup();

    // 桌面切到英文（无实时推送）；手机下次连接 = 页面重开 / 刷新（Board 重新挂载，
    // 模块态 / i18n 随冷启动重置）
    const deps2 = await setup();
    vi.stubGlobal("fetch", stubStatefulFetch("en").stub);
    installSse(okSessions(0));
    deps2.lang.initMobileLanguage();
    renderBoard(deps2, {});
    await advance(0);
    expect(deps2.i18n.language).toBe("en");
    expect(document.documentElement.lang).toBe("en");
  });

  it("手动优先读路径：经写接口预置 {language:zh}，桌面 en 连接 → 手机 zh（C-6 第①级）", async () => {
    setBrowserLanguage(["en"]);
    const deps = await setup();
    const ledgered = stubStatefulFetch("en");
    vi.stubGlobal("fetch", ledgered.stub);
    installSse(okSessions(0));
    deps.lang.initMobileLanguage(); // 启动：浏览器 en

    // 任务 2 写接口预置手动记录（真实客户端函数 → 状态化账本落账）
    await deps.api.pushDeviceLanguage("zh");
    expect(ledgered.peek()).toBe("zh");

    // 连接：/host 下发 manualLanguage:"zh"（读账）+ desktopLanguage:"en" → 第①级胜
    renderBoard(deps, {});
    await advance(0);
    expect(deps.i18n.language).toBe("zh");
    expect(document.documentElement.lang).toBe("zh");
    // 手动记录随下发镜像到本地缓存（仅手动值落盘），下次冷启动第①级直通
    expect(localStorage.getItem(MANUAL_LANGUAGE_KEY)).toBe("zh");
  });

  it("/host 403（设备失效）不触发重评估：启动的浏览器归流语言继续生效（M2）", async () => {
    setBrowserLanguage(["zh-TW"]);
    vi.stubGlobal("fetch", stubAllForbidden());
    const deps = await setup();
    deps.lang.initMobileLanguage(); // 启动：浏览器归流 zh

    installSse(okSessions(0));
    const onUnpaired = vi.fn();
    renderBoard(deps, { onUnpaired });
    await advance(0);
    // Board 的设备失效判定走 fetchSessions（tick：30s 低频对账 / 降级轮询），推进到对账拍
    await advance(30_000);

    expect(onUnpaired).toHaveBeenCalled(); // 设备失效回路照常（既有语义不回归）
    expect(deps.i18n.language).toBe("zh"); // 语言未被重评估动过
    expect(document.documentElement.lang).toBe("zh");
  });

  it("I1 回归：切换发生在 /host 在途期间 → 迟到旧账（null）不清钉住/缓存；新鲜连接照常清账跟随", async () => {
    setBrowserLanguage(["zh-TW"]); // 启动 zh
    const deps = await setup();
    // /host 响应持有闸：Board 挂载发出连接（代 0）后挂起，切换后再放行——
    // 响应先于切换串行化 = 过期账面（慢隧道在途窗口的评审序列）
    let releaseHost!: (payload: unknown) => void;
    const heldHost = new Promise((resolve) => {
      releaseHost = resolve;
    });
    const staleLedgerView = {
      host: { name: "M", platform: "macos", version: "0.0.0", bootId: "b" },
      enabledTools: [],
      installedTools: [],
      desktopLanguage: "zh",
      manualLanguage: null, // 切换前的账面：无手动记录
    };
    vi.stubGlobal(
      "fetch",
      vi.fn(async (url: string): Promise<Response> => {
        if (url === "/m/api/v1/host") {
          return heldHost.then((p) => new Response(JSON.stringify(p), { status: 200 }));
        }
        if (url === "/m/api/v1/device-language") {
          return new Response(JSON.stringify({ manual: true, language: "en" }), { status: 200 });
        }
        return new Response(JSON.stringify(okSessions(0)), { status: 200 });
      })
    );
    installSse(okSessions(0));
    deps.lang.initMobileLanguage(); // 启动：浏览器归流 zh（无手动缓存）
    renderBoard(deps, {});
    await advance(0); // Board 发出 /host（代 0）并挂起

    // 用户在连接在途期间手动切换 en（钉住 + 缓存 + 记账 + 代数自增）
    await deps.lang.switchMobileLanguage("en");
    expect(deps.i18n.language).toBe("en");

    // 放行迟到旧账 → 重评估整体忽略：不回翻桌面 zh、缓存幸存
    await act(async () => {
      releaseHost(staleLedgerView);
    });
    await advance(0);
    expect(deps.i18n.language).toBe("en");
    expect(document.documentElement.lang).toBe("en");
    expect(localStorage.getItem(MANUAL_LANGUAGE_KEY)).toBe("en");
    cleanup();

    // 新鲜连接（页面重开 = 重新冷启动 + Board 重挂载）：账本仍 null → L2 清账 + 跟随桌面
    // （守卫只免过期账面，不妨碍真值收敛）
    const deps2 = await setup();
    vi.stubGlobal("fetch", stubStatefulFetch("zh").stub);
    installSse(okSessions(0));
    deps2.lang.initMobileLanguage(); // 本地缓存 en → 启动第①级 en
    renderBoard(deps2, {});
    await advance(0);
    expect(deps2.i18n.language).toBe("zh"); // 新鲜 null 清账 → 跟随桌面 zh
    expect(document.documentElement.lang).toBe("zh");
    expect(localStorage.getItem(MANUAL_LANGUAGE_KEY)).toBeNull(); // 缓存已随清账清除
  });
});
