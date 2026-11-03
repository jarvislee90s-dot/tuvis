// 方向判定单测（2026-10-27 横竖屏自适应默认布局，判据见 src/mobile/viewport.ts）
//
// 忠实度要求：fake matchMedia **按 query 缓存同一实例**，且每个实例有自己的监听集合、
// 只认 `type === "change"`。若所有实例共用一个全局集合或忽略事件类型，即使实现把事件
// 名写错、或订阅时用了别的查询串，用例照样会绿（假绿）。
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { LANDSCAPE_QUERY, readViewportKind, subscribeViewportKind } from "@/mobile/viewport";

type Listener = () => void;

interface FakeMql {
  media: string;
  matches: boolean;
  addEventListener?: (type: string, cb: Listener) => void;
  removeEventListener?: (type: string, cb: Listener) => void;
  addListener?: (cb: Listener) => void;
  removeListener?: (cb: Listener) => void;
}

interface InstallOptions {
  /** MQL 的事件 API 形态：现代两件套 / iOS 13 legacy 两件套 / 都没有（老 shim） */
  api?: "modern" | "legacy" | "none";
}

/** 装一个按 query 缓存实例的 matchMedia fake；`set` 只在值真的变化时对**该实例**广播 */
function installMatchMedia(initial: "portrait" | "landscape", opts: InstallOptions = {}) {
  const state = { landscape: initial === "landscape" };
  const instances: Array<{ query: string; mql: FakeMql; listeners: Set<Listener> }> = [];
  // 只有宽高比查询按方向作答；其余查询（如 min-width）恒 false——本模块不消费
  const matchesFor = (q: string) => (q.includes("aspect-ratio") ? state.landscape : false);

  const make = (q: string): FakeMql => {
    const listeners = new Set<Listener>();
    const mql: FakeMql = { media: q, matches: matchesFor(q) };
    const api = opts.api ?? "modern";
    if (api === "modern") {
      mql.addEventListener = (type, cb) => {
        if (type === "change") listeners.add(cb); // 写错事件名就不该收得到
      };
      mql.removeEventListener = (_type, cb) => listeners.delete(cb);
    } else if (api === "legacy") {
      mql.addListener = (cb) => listeners.add(cb);
      mql.removeListener = (cb) => listeners.delete(cb);
    }
    instances.push({ query: q, mql, listeners });
    return mql;
  };

  const matchMedia = vi.fn((q: string) => {
    const hit = instances.find((it) => it.query === q)?.mql ?? make(q);
    hit.matches = matchesFor(q); // 每次读取都对齐当前方向（等价浏览器实时求值）
    return hit;
  });

  Object.defineProperty(window, "matchMedia", {
    writable: true,
    configurable: true,
    value: matchMedia,
  });

  return {
    matchMedia,
    /** 当前注册的监听器总数（跨实例） */
    listenerCount: () => instances.reduce((n, it) => n + it.listeners.size, 0),
    /** 切方向并对**值真的变了**的实例广播 change（浏览器语义） */
    set(kind: "portrait" | "landscape") {
      state.landscape = kind === "landscape";
      for (const it of instances) {
        const next = matchesFor(it.query);
        if (next === it.mql.matches) continue;
        it.mql.matches = next;
        for (const cb of [...it.listeners]) cb();
      }
    },
    /** 不改值、只再广播一次（模拟 change 与 orientationchange 双通道重复唤醒） */
    notify() {
      for (const it of instances) for (const cb of [...it.listeners]) cb();
    },
  };
}

beforeEach(() => {
  // 与 SessionDetail.test 同款隔离：jsdom 原生没有 matchMedia，显式置 undefined
  Object.defineProperty(window, "matchMedia", {
    writable: true,
    configurable: true,
    value: undefined,
  });
});

afterEach(() => {
  vi.restoreAllMocks();
  Object.defineProperty(window, "matchMedia", {
    writable: true,
    configurable: true,
    value: undefined,
  });
});

describe("readViewportKind：宽 > 高 × 1.2 才算横屏", () => {
  it("matchMedia 缺失（jsdom / 旧 WebView）→ 竖屏", () => {
    expect(readViewportKind()).toBe("portrait");
  });

  it("matchMedia 抛错 → 竖屏（不把异常抛给调用方）", () => {
    Object.defineProperty(window, "matchMedia", {
      writable: true,
      configurable: true,
      value: () => {
        throw new TypeError("matchMedia is not a function");
      },
    });
    expect(readViewportKind()).toBe("portrait");
  });

  it("命中宽高比查询 → 横屏；未命中 → 竖屏", () => {
    const ctl = installMatchMedia("landscape");
    expect(readViewportKind()).toBe("landscape");
    ctl.set("portrait");
    expect(readViewportKind()).toBe("portrait");
  });

  it("判据查询串是 6/5（= 1.2），不是屏幕宽度阈值", () => {
    const ctl = installMatchMedia("portrait");
    readViewportKind();
    expect(LANDSCAPE_QUERY).toBe("(min-aspect-ratio: 6/5)");
    expect(ctl.matchMedia).toHaveBeenCalledWith("(min-aspect-ratio: 6/5)");
  });
});

describe("subscribeViewportKind：旋转监听与退订", () => {
  it("订阅走的是同一个 6/5 查询串，change 事件转发给回调", () => {
    const ctl = installMatchMedia("portrait");
    const onChange = vi.fn();
    const off = subscribeViewportKind(onChange);

    // 订阅与读取必须同源：否则「读用 A、订阅用 B」的实现会漏掉旋转
    expect(ctl.matchMedia).toHaveBeenCalledWith(LANDSCAPE_QUERY);

    ctl.set("landscape");
    expect(onChange).toHaveBeenCalledTimes(1);
    expect(readViewportKind()).toBe("landscape");

    off();
    expect(ctl.listenerCount()).toBe(0);
    ctl.set("portrait");
    expect(onChange).toHaveBeenCalledTimes(1); // 退订后不再回调
  });

  it("值没变时不广播 change（浏览器语义，避免无谓重置）", () => {
    const ctl = installMatchMedia("landscape");
    const onChange = vi.fn();
    const off = subscribeViewportKind(onChange);

    ctl.set("landscape"); // 同值
    expect(onChange).not.toHaveBeenCalled();

    off();
  });

  it("legacy addListener / removeListener（iOS 13 及更早）同样可用", () => {
    const ctl = installMatchMedia("portrait", { api: "legacy" });
    const onChange = vi.fn();
    const off = subscribeViewportKind(onChange);

    ctl.set("landscape");
    expect(onChange).toHaveBeenCalledTimes(1);

    off();
    expect(ctl.listenerCount()).toBe(0);
  });

  it("MQL 两套事件 API 都没有（老 shim）→ 静默不订阅，不抛错", () => {
    const ctl = installMatchMedia("portrait", { api: "none" });
    const onChange = vi.fn();
    const off = subscribeViewportKind(onChange);

    expect(() => ctl.set("landscape")).not.toThrow();
    expect(onChange).not.toHaveBeenCalled(); // 没有订阅通道，等价「方向变化收不到」
    expect(() => off()).not.toThrow();
  });

  it("orientationchange 兜底：MQL 漏发时仍能唤醒重读（延后一帧，避开旧布局）", async () => {
    // api=none ⇒ 只有 window 的 orientationchange 一条通道
    installMatchMedia("portrait", { api: "none" });
    const onChange = vi.fn();
    const off = subscribeViewportKind(onChange);

    window.dispatchEvent(new Event("orientationchange"));
    // 兜底是「延后一帧再唤醒」——不能在派发瞬间同步回调（那时布局可能还没更新）
    expect(onChange).not.toHaveBeenCalled();
    await vi.waitFor(() => expect(onChange).toHaveBeenCalledTimes(1));

    off();
    window.dispatchEvent(new Event("orientationchange"));
    await new Promise((r) => setTimeout(r, 20));
    expect(onChange).toHaveBeenCalledTimes(1);
  });

  it("matchMedia 抛错时订阅不崩（只剩兜底通道）", async () => {
    Object.defineProperty(window, "matchMedia", {
      writable: true,
      configurable: true,
      value: () => {
        throw new Error("boom");
      },
    });
    const onChange = vi.fn();
    const off = subscribeViewportKind(onChange);
    expect(() => window.dispatchEvent(new Event("orientationchange"))).not.toThrow();
    await vi.waitFor(() => expect(onChange).toHaveBeenCalledTimes(1));
    off();
  });
});
