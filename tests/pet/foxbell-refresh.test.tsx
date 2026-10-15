// tests/pet/foxbell-refresh.test.tsx — 激活宠物刷新"后到者胜"闸门 + 丢弃结果回收（FIX-6）
// 并发两次 pet-active-changed：旧解析后到不得回滚显示、不得 dispose 新宠物的活跃快照；
// 被丢弃的解析（过期或卸载）其 blob 快照必须回收。
import { render, screen, act } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({
    show: vi.fn(),
    hide: vi.fn(),
    setAlwaysOnTop: vi.fn(),
    setIgnoreCursorEvents: vi.fn(),
    setPosition: vi.fn(),
    setSize: vi.fn(),
    outerPosition: vi.fn(),
    outerSize: vi.fn(),
    scaleFactor: vi.fn(),
    currentMonitor: vi.fn(),
  }),
}));
// listen 由测试手动控制触发；emit 补齐导出避免 mock 代理缺属性
const listenHandlers: Array<(e: { event: string; payload: unknown }) => void> = [];
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (_ev: string, cb: (e: { event: string; payload: unknown }) => void) => {
    listenHandlers.push(cb);
    return () => {
      const i = listenHandlers.indexOf(cb);
      if (i >= 0) listenHandlers.splice(i, 1);
    };
  }),
  emit: vi.fn(async () => {}),
}));

// resolveActivePet 返回手动控制的 deferred：测试依次 resolve，制造"旧结果后到"
let deferredChain: Array<
  (v: Awaited<ReturnType<typeof import("@/components/pet/petRuntime").resolveActivePet>>) => void
> = [];
vi.mock("@/components/pet/petRuntime", async (importOriginal) => {
  const orig = await importOriginal<typeof import("@/components/pet/petRuntime")>();
  return {
    ...orig,
    resolveActivePet: vi.fn(
      () =>
        new Promise((resolve) => {
          deferredChain.push(resolve);
        })
    ),
  };
});

// useSessionsQuery 需要 QueryClientProvider：本测试只关心 refresh 闸门，直接 mock 掉（与 foxbell-events 同款）
const data: unknown = undefined;
vi.mock("@/lib/query/queries/sessions", () => ({ useSessionsQuery: () => ({ data }) }));

import { FoxbellPet } from "@/components/pet/FoxbellPet";

const mkPet = (id: string) => ({
  id,
  displayName: id,
  spritesheetUrl: `/x/${id}.webp`,
  rows: 11 as const,
  hasVoice: false,
  hasSubtitle: false,
  voices: [],
  resolveVoiceUrl: () => "",
  dispose: vi.fn(),
});

const fire = async () => {
  // 连续两次 pet-active-changed：每次事件各触发一次 refresh（并发两次解析）
  for (const h of [...listenHandlers]) {
    await h({ event: "pet-active-changed", payload: {} });
    await h({ event: "pet-active-changed", payload: {} });
  }
};

describe("FoxbellPet 刷新后到者胜（FIX-6）", () => {
  afterEach(() => {
    deferredChain = [];
    vi.clearAllMocks();
  });

  it("并发两次刷新：旧解析后到不回滚、被丢弃结果回收、新结果存活", async () => {
    render(<FoxbellPet />);
    // 启动 refresh 产生第 1 个挂起 promise
    await act(async () => {});
    expect(deferredChain.length).toBe(1);

    // 连续两次事件 → 第 2、3 个挂起 promise
    await act(async () => {
      await fire();
    });
    expect(deferredChain.length).toBe(3);

    const [p1, p2, p3] = deferredChain;
    const first = mkPet("first");
    const second = mkPet("second");
    const third = mkPet("third-unused");

    // 先 resolve 第 3 次（最新 gen）刷新 → 生效
    await act(async () => {
      p3(second);
    });
    // 后 resolve 第 1 次启动刷新（旧结果后到，必须被丢弃并回收）
    await act(async () => {
      p1(first);
    });
    // 再后 resolve 第 2 次（也是过期结果，同样被丢弃回收）
    await act(async () => {
      p2(third);
    });

    // ① 最终显示是第二个结果（后到者胜：旧结果不得回滚）
    const sprite = screen.getByTestId("pet-sprite") as HTMLElement;
    expect(sprite.style.backgroundImage).toContain("/x/second.webp");

    // ② 第一个结果（过期被丢弃）的 dispose 被调用
    expect(first.dispose).toHaveBeenCalledTimes(1);

    // ③ 第二个结果（当前活跃）的 dispose 未被调用
    expect(second.dispose).not.toHaveBeenCalled();

    // ④ 每个被丢弃的快照都回收（FIX-12：第三个过期结果同样恰好一次）
    expect(third.dispose).toHaveBeenCalledTimes(1);

    void p1;
    void p2;
    void p3;
  });
});
