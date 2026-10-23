// tests/mobile/language-toggle.test.tsx — 移动端语言切换按钮（M3 / M4）集成测试。
//
// 与 language-connect.test.tsx（Task 3，连接时重评估接线）分工：本文件锁的是
// **Board 页头按钮**——点击经 switchMobileLanguage 生效入口（立即生效 + 钉住手动
// + 桌面按设备记账 C-5）的端到端行为与独立账本语义（§1.5 / U14）：
//  - 按钮渲染在换肤图标旁（U4）、与同排图标按钮同构、二值无三态（U6）；
//  - 点击即翻语言（i18n + html lang，免刷新）且记账载荷逐字 {\"language\":\"en\"}；
//  - 6.4-2：手动后多次重连 / 换网络（新 /host 拉取）仍手动值（账本记录优先）；
//  - 6.4-3：重新配对清账 → 下次连接回到跟随桌面（L2）；
//  - 6.4-7：已有 zh 手动记录 + 桌面 en → 连接后仍 zh（桌面切换不影响有记录设备）；
//  - 6.4-8：手机手动切换不改桌面语言源（记账是唯一副作用，desktopLanguage 不动）；
//  - 记账失败 → formatInvokeError 体系 toast 如实暴露，乐观显示不回滚（设计既定）。
// （无记录设备的「连接跟随桌面」半边由 Task 3 language-connect.test.tsx 场景 1 锁定，
//  此处不重复。）
//
// 夹具沿用 Task 3 惯例：fetch 全量 stub（盖过 setup.ts 的 msw）+ 状态化账本
// （POST /device-language 落账、GET /host 按账读回——cookie 身份不随网络变化）；
// SSE 走 MockEventSource；模块单例态逐用例 vi.resetModules() + 动态 import 重置。
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { MockEventSource } from "./eventSourceMock";
import { MANUAL_LANGUAGE_KEY } from "@/i18n/language-prefs";
import type { SessionsResponse } from "@/types/session";

// sonner 局部替身：只换 toast.error 为间谍（language.ts 记账失败经它如实提示）；
// 其余导出（Toaster / toast.success 等）保真，防同图其他模块引用缺失。
const { toastError } = vi.hoisted(() => ({ toastError: vi.fn() }));
vi.mock("sonner", async (importOriginal) => {
  const actual = await importOriginal<typeof import("sonner")>();
  return { ...actual, toast: { ...actual.toast, error: toastError } };
});

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

/** 安装脚本化 EventSource：连接建立即送达首帧快照（Board.test 同款）。 */
function installSse(snapshot: SessionsResponse) {
  class ScriptedEventSource extends MockEventSource {
    constructor(url: string) {
      super(url);
      setTimeout(() => this.emit("snapshot", snapshot), 0);
    }
  }
  vi.stubGlobal("EventSource", ScriptedEventSource);
}

/** 状态化账本夹具（cookie 身份语义）：POST /device-language 落账（记录原始载荷），
 *  GET /host 按账返回 manualLanguage、按构造参数返回 desktopLanguage。
 *  `writeStatus` 可注入记账失败（500 → ApiError → formatInvokeError toast 路径）。 */
function stubLedgerFixture(desktopLanguage: unknown, opts?: { writeStatus?: number }) {
  let ledger: "zh" | "en" | null = null;
  const writes: string[] = [];
  const calls: string[] = [];
  let hostFetches = 0;
  return {
    /** 仅供断言读取当前账面 */
    peek: () => ledger,
    /** 记账请求的原始 body 列表（逐字载荷断言用） */
    writes,
    /** 全部请求的 `METHOD url` 列表（「记账是唯一副作用」断言用） */
    calls,
    hostFetchCount: () => hostFetches,
    /** 模拟任务 2 的「配对完成即清该设备语言记录」（Rust pairing persist_and_cookie 路径） */
    clearLedger: () => {
      ledger = null;
    },
    stub: vi.fn(async (url: string, init?: RequestInit) => {
      calls.push(`${init?.method ?? "GET"} ${url}`);
      if (url === "/m/api/v1/device-language") {
        writes.push(String(init?.body));
        if (opts?.writeStatus) {
          return new Response(JSON.stringify({ error: "internal" }), {
            status: opts.writeStatus,
          });
        }
        ledger = (JSON.parse(String(init?.body)) as { language: "zh" | "en" }).language;
        return new Response(JSON.stringify({ manual: true, language: ledger }), { status: 200 });
      }
      if (url === "/m/api/v1/host") {
        hostFetches += 1;
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

function renderBoard(deps: Deps) {
  return render(<deps.Board onPaired={vi.fn()} onUnpaired={vi.fn()} onOpenHistory={vi.fn()} />);
}

/** fake timers 下推进定时器并让微任务落地（Board.test 同款）。 */
async function advance(ms: number) {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(ms);
  });
}

/** 点击语言按钮并等切换生效入口（含记账 fetch）落地。 */
async function clickLanguageToggle() {
  await act(async () => {
    fireEvent.click(screen.getByTestId("language-toggle"));
  });
}

beforeEach(() => {
  vi.useFakeTimers();
  MockEventSource.reset();
  toastError.mockClear();
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

describe("Board 语言切换按钮（M3 / M4）", () => {
  it("渲染于换肤图标旁且同构；点击 zh→en→zh 即时生效免刷新；aria-label 随态翻转（U4/U6/g4）", async () => {
    setBrowserLanguage(["zh-CN"]); // 启动中文（浏览器归流）
    installSse(okSessions(0));
    const deps = await setup();
    vi.stubGlobal("fetch", stubLedgerFixture("zh").stub);
    deps.lang.initMobileLanguage();
    const { container } = renderBoard(deps);
    await advance(0);

    // 位置：主题按钮的紧邻兄弟（U4「换肤图标旁边」）；主题按钮按 testid 定位
    // （M5 文案双语化后其可达名走 mobile.board.themeToLight 键，不作为定位依据）
    const themeBtn = screen.getByTestId("theme-toggle"); // 默认 dark（matchMedia 缺失 → dark）
    const langBtn = screen.getByTestId("language-toggle");
    expect(themeBtn.nextElementSibling).toBe(langBtn);
    expect(langBtn.parentElement).toBe(themeBtn.parentElement);

    // 同构：与主题/声音按钮同一壳类（图标按钮族）
    const shell = ["rounded-lg", "border", "border-[var(--cb)]", "bg-[var(--cbg)]", "p-1.5"];
    for (const cls of shell) {
      expect(langBtn.className).toContain(cls);
      expect(themeBtn.className).toContain(cls);
    }

    // 初始态：当前 zh（短码可见）+ 可达名描述动作（zh 键值，g4 等价描述）
    expect(langBtn.textContent).toContain("ZH");
    expect(langBtn.getAttribute("aria-label")).toBe("切换界面语言为英文");
    expect(langBtn.getAttribute("title")).toBe("切换界面语言为英文");

    // 免刷新的原地切换：记录页头标题节点，切换后仍存活（导航/重载会换掉整棵树）
    const heading = container.querySelector("h1");
    expect(heading).not.toBeNull();

    await clickLanguageToggle(); // zh → en
    expect(deps.i18n.language).toBe("en");
    expect(document.documentElement.lang).toBe("en");
    expect(langBtn.textContent).toContain("EN");
    // 可达名随态翻转为 en 键值（英文等价描述，非逐字直译）
    expect(langBtn.getAttribute("aria-label")).toBe("Switch interface language to Chinese");
    expect(heading?.isConnected).toBe(true); // 原地切换，无导航重载

    await clickLanguageToggle(); // en → zh（二值往返，无第三态残留）
    expect(deps.i18n.language).toBe("zh");
    expect(document.documentElement.lang).toBe("zh");
    expect(langBtn.textContent).toContain("ZH");
    expect(langBtn.getAttribute("aria-label")).toBe("切换界面语言为英文");
    expect(heading?.isConnected).toBe(true);
  });

  it('点击记账载荷逐字 {"language":"en"}；会话钉住在迟到 /host 旧账与新鲜重连下均保持', async () => {
    setBrowserLanguage(["zh-TW"]); // 启动中文，与手动值可区分
    const fixture = stubLedgerFixture("zh");
    vi.stubGlobal("fetch", fixture.stub);
    const deps = await setup();
    deps.lang.initMobileLanguage();

    // /host 响应持有闸：挂载发出连接（代 0）后挂起，切换后再放行（慢隧道在途窗口）
    let releaseHost!: (payload: unknown) => void;
    const heldHost = new Promise((resolve) => {
      releaseHost = resolve;
    });
    const rawStub = fixture.stub.getMockImplementation()!;
    fixture.stub.mockImplementation(async (url: string, init?: RequestInit) => {
      if (url === "/m/api/v1/host") {
        return heldHost.then(
          () =>
            new Response(
              JSON.stringify({
                host: { name: "M", platform: "macos", version: "0.0.0", bootId: "b" },
                enabledTools: [],
                installedTools: [],
                desktopLanguage: "zh",
                manualLanguage: null, // 切换前串行化的旧账面
              }),
              { status: 200 }
            )
        );
      }
      return rawStub(url, init);
    });
    installSse(okSessions(0));
    renderBoard(deps);
    await advance(0); // Board 发出 /host（代 0）并挂起

    await clickLanguageToggle(); // 切 en：立即生效 + 钉住 + 记账（代数自增）
    expect(deps.i18n.language).toBe("en");
    expect(fixture.writes).toEqual(['{"language":"en"}']); // C-5 载荷逐字
    expect(fixture.peek()).toBe("en");

    // 迟到旧账（manualLanguage:null，先于切换串行化）→ 过期账面整体忽略，钉住保持
    await act(async () => {
      releaseHost(undefined);
    });
    await advance(0);
    expect(deps.i18n.language).toBe("en");
    expect(document.documentElement.lang).toBe("en");

    // 恢复夹具常规实现：其后 /host 返回账本真值视图（manualLanguage=en）
    fixture.stub.mockImplementation(rawStub);
    cleanup();

    // 新鲜重连（同会话内 Board 重挂载；不冷启动以保留会话钉住）：按账读回 en → 仍 en
    const hostFetchesBeforeRemount = fixture.hostFetchCount();
    installSse(okSessions(0));
    renderBoard(deps);
    await advance(0);
    expect(fixture.hostFetchCount()).toBeGreaterThan(hostFetchesBeforeRemount); // 确实发生了新的 /host 拉取
    expect(deps.i18n.language).toBe("en");
    expect(document.documentElement.lang).toBe("en");
  });

  it("6.4-2：手动 en 后多次重连 / 换网络（新 /host）与冷启动均保持 en；桌面语言源始终 zh", async () => {
    setBrowserLanguage(["zh-TW"]);
    const fixture = stubLedgerFixture("zh");
    vi.stubGlobal("fetch", fixture.stub);
    installSse(okSessions(0));
    const deps = await setup();
    deps.lang.initMobileLanguage();
    renderBoard(deps);
    await advance(0);

    await clickLanguageToggle(); // 手动 en（记账落账）
    expect(deps.i18n.language).toBe("en");
    cleanup();

    // 重连 1（Board 重挂载 → 新 /host 拉取，账本记录优先于桌面 zh）
    installSse(okSessions(0));
    renderBoard(deps);
    await advance(0);
    expect(deps.i18n.language).toBe("en");
    cleanup();

    // 重连 2（「换网络 / 换 IP」= 再一次全新 /host；夹具按 cookie 身份保留账）
    installSse(okSessions(0));
    renderBoard(deps);
    await advance(0);
    expect(deps.i18n.language).toBe("en");
    expect(fixture.hostFetchCount()).toBeGreaterThanOrEqual(3); // 连接 + 两次重连
    cleanup();

    // 冷启动（页面重开）：手机本地手动缓存第①级直通 en，连接读账 en 确认
    const deps2 = await setup();
    vi.stubGlobal("fetch", fixture.stub);
    installSse(okSessions(0));
    expect(deps2.lang.initMobileLanguage()).toBe("en"); // 缓存直通（仅手动值落盘的收益面）
    renderBoard(deps2);
    await advance(0);
    expect(deps2.i18n.language).toBe("en");
    expect(document.documentElement.lang).toBe("en");
    // 桌面语言源全程未被远程切换触碰（6.4-8 的账面半边）
    const hostPayload = (await (await fetch("/m/api/v1/host")).json()) as {
      desktopLanguage: unknown;
      manualLanguage: unknown;
    };
    expect(hostPayload.desktopLanguage).toBe("zh");
    expect(hostPayload.manualLanguage).toBe("en");
  });

  it("6.4-3：重新配对清账后，下次连接清本地手动缓存并回到跟随桌面语言（L2）", async () => {
    setBrowserLanguage(["en"]); // 浏览器 en：清账后须跟桌面 zh 而非浏览器，判据更锐
    const fixture = stubLedgerFixture("zh");
    vi.stubGlobal("fetch", fixture.stub);
    installSse(okSessions(0));
    const deps = await setup();
    deps.lang.initMobileLanguage();
    renderBoard(deps);
    await advance(0);

    await clickLanguageToggle(); // 手动 en
    expect(deps.i18n.language).toBe("en");
    expect(fixture.peek()).toBe("en");
    cleanup();

    // 重新配对：任务 2 在 pairing 完成路径清该设备语言记录（此处模拟其账面效果）
    fixture.clearLedger();
    expect(fixture.peek()).toBeNull();

    // 冷启动：本地缓存仍在 → 启动第①级 en（启动加速器，连接时对账纠正）
    const deps2 = await setup();
    vi.stubGlobal("fetch", fixture.stub);
    installSse(okSessions(0));
    expect(deps2.lang.initMobileLanguage()).toBe("en");
    renderBoard(deps2);
    await advance(0); // 连接：/host manualLanguage=null → 清钉住 + 清缓存 → 跟随桌面 zh
    expect(deps2.i18n.language).toBe("zh");
    expect(document.documentElement.lang).toBe("zh");
    expect(localStorage.getItem(MANUAL_LANGUAGE_KEY)).toBeNull(); // 收敛不残留
    expect(screen.getByTestId("language-toggle").textContent).toContain("ZH");
  });

  it("6.4-7：已有 zh 手动记录 + 桌面 en → 连接后仍 zh；按钮可再覆盖为 en（独立账本）", async () => {
    setBrowserLanguage(["en"]);
    const fixture = stubLedgerFixture("en");
    vi.stubGlobal("fetch", fixture.stub);
    const deps = await setup();
    deps.lang.initMobileLanguage(); // 启动：浏览器 en

    // 经任务 2 写接口预置 zh 手动记录（真实客户端 → 状态化账本，Task 3 同款预置法）
    await deps.api.pushDeviceLanguage("zh");
    expect(fixture.peek()).toBe("zh");

    installSse(okSessions(0));
    renderBoard(deps);
    await advance(0); // 连接：manual zh > desktop en → 仍 zh（桌面切换不影响有记录设备）
    expect(deps.i18n.language).toBe("zh");
    expect(document.documentElement.lang).toBe("zh");
    expect(screen.getByTestId("language-toggle").textContent).toContain("ZH");
    expect(screen.getByTestId("language-toggle").getAttribute("aria-label")).toBe(
      "切换界面语言为英文"
    );

    // 同一设备再点按钮 → 覆盖为 en（最后一次手动为准），账本随之更新
    await clickLanguageToggle();
    expect(deps.i18n.language).toBe("en");
    expect(fixture.peek()).toBe("en");
    expect(fixture.writes).toEqual(['{"language":"zh"}', '{"language":"en"}']);
  });

  it("6.4-8：手机手动切换不改桌面语言源——记账是唯一副作用，desktopLanguage 原值不动", async () => {
    setBrowserLanguage(["zh-TW"]);
    const fixture = stubLedgerFixture("zh");
    vi.stubGlobal("fetch", fixture.stub);
    installSse(okSessions(0));
    const deps = await setup();
    deps.lang.initMobileLanguage();
    renderBoard(deps);
    await advance(0);

    await clickLanguageToggle(); // 手动 en
    expect(deps.i18n.language).toBe("en");

    // 唯一写副作用 = 一次 device-language POST；无任何「改桌面语言」类调用
    const postCalls = fixture.calls.filter((c) => c.startsWith("POST"));
    expect(postCalls).toEqual(["POST /m/api/v1/device-language"]);
    expect(fixture.writes).toEqual(['{"language":"en"}']);

    // 桌面语言源独立：之后的 /host 仍下发原 desktopLanguage=zh，manualLanguage=en
    const hostPayload = (await (await fetch("/m/api/v1/host")).json()) as {
      desktopLanguage: unknown;
      manualLanguage: unknown;
    };
    expect(hostPayload.desktopLanguage).toBe("zh");
    expect(hostPayload.manualLanguage).toBe("en");
  });

  it("记账失败（500）→ formatInvokeError 体系 toast 如实暴露；乐观显示不回滚、不崩溃", async () => {
    setBrowserLanguage(["zh-TW"]);
    const fixture = stubLedgerFixture("zh", { writeStatus: 500 });
    vi.stubGlobal("fetch", fixture.stub);
    installSse(okSessions(0));
    const deps = await setup();
    deps.lang.initMobileLanguage();
    renderBoard(deps);
    await advance(0);

    await clickLanguageToggle(); // 切换立即生效；记账 500 → toast（不假成功、不回滚）
    expect(toastError).toHaveBeenCalledTimes(1);
    expect(typeof toastError.mock.calls[0][0]).toBe("string");
    expect(String(toastError.mock.calls[0][0])).toContain("500"); // ApiError 原文透出（未登记码回退）

    // 设计既定的乐观语义：本会话内选择保持（钉住 + 本地缓存），账本下次连接收敛
    expect(deps.i18n.language).toBe("en");
    expect(document.documentElement.lang).toBe("en");
    expect(screen.getByTestId("language-toggle").textContent).toContain("EN");
    expect(localStorage.getItem(MANUAL_LANGUAGE_KEY)).toBe("en");
    expect(fixture.peek()).toBeNull(); // 夹具未落账（如实失败）
  });
});
