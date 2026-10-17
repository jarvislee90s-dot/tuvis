import { act, cleanup, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import useCodexThread, { CODEX_THREAD_POLL_MS } from "@/mobile/useCodexThread";
import type { CapabilitySet, ThreadProjection } from "@/mobile/codexIpcApi";
import { MockEventSource } from "./eventSourceMock";

// Task 9：useCodexThread 行为锁（计划 Step 1「SSE 首帧/增量、3 次失败降级轮询、
// 卸载关流、session 切换不串台」逐条落地）。SSE 用 MockEventSource 手动驱动；
// fetch 全量 stub（GET /m/api/v1/codex/thread）。

function caps(over: Partial<CapabilitySet> = {}): CapabilitySet {
  return {
    send: true,
    state: true,
    command: true,
    permissions: true,
    blockingInput: true,
    ...over,
  };
}

function projection(over: Partial<ThreadProjection> = {}): ThreadProjection {
  return {
    threadId: "th-1",
    revision: 7,
    stale: false,
    availability: "controllable",
    capabilities: caps(),
    runtimeStatus: "idle",
    messages: [],
    truncated: false,
    pending: [],
    ...over,
  };
}

interface Routes {
  thread?: ThreadProjection;
  threadStatus?: number;
  threadHang?: boolean;
  threadNetworkFail?: boolean;
}

let routes: Routes;
let fetchMock: ReturnType<typeof vi.fn>;
let releaseThread: ((p: ThreadProjection) => void) | null = null;

beforeEach(() => {
  routes = {};
  releaseThread = null;
  MockEventSource.reset();
  vi.useFakeTimers();
  vi.stubGlobal("EventSource", MockEventSource);
});
afterEach(() => {
  cleanup();
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

function installFetch() {
  fetchMock = vi.fn(async (input: RequestInfo | URL) => {
    const url = String(input);
    if (url.includes("/codex/thread")) {
      if (routes.threadHang) {
        return new Promise<Response>((resolve) => {
          releaseThread = (p: ThreadProjection) =>
            resolve(new Response(JSON.stringify(p), { status: 200 }));
        });
      }
      if (routes.threadNetworkFail) throw new TypeError("network down");
      if (routes.threadStatus) return new Response("no", { status: routes.threadStatus });
      return new Response(JSON.stringify(routes.thread ?? projection()), { status: 200 });
    }
    throw new Error(`unexpected fetch: ${url}`);
  });
  vi.stubGlobal("fetch", fetchMock);
}

/** GET /codex/thread 调用次数（轮询节奏断言用） */
function threadGetCalls(): number {
  return fetchMock.mock.calls.filter(([u]) => String(u).includes("/codex/thread")).length;
}

/** 冲刷挂起的微任务（fetch → json → setState 链） */
async function flushMicro() {
  for (let i = 0; i < 6; i += 1) {
    await act(async () => {
      await Promise.resolve();
    });
  }
}

describe("useCodexThread：SSE 订阅（Task 9）", () => {
  it("sse_first_frame_then_delta：snapshot 首帧与 state 增量同 DTO 落 state；transport=sse", async () => {
    installFetch();
    const { result } = renderHook(() => useCodexThread("s1", true));
    expect(result.current.transport).toBe("sse");
    const es = MockEventSource.latest();
    expect(es.url).toContain("/m/api/v1/codex/thread/events?session_id=s1");
    expect(result.current.state).toBeNull();
    es.emit("snapshot", projection({ revision: 1 }));
    await act(async () => {});
    expect(result.current.state?.revision).toBe(1);
    es.emit("state", projection({ revision: 2, runtimeStatus: "active" }));
    await act(async () => {});
    expect(result.current.state?.revision).toBe(2);
    expect(result.current.state?.runtimeStatus).toBe("active");
    expect(result.current.transport).toBe("sse");
  });

  it("disabled_off：enabled=false 不建流不拉取（transport=off）", async () => {
    installFetch();
    const { result } = renderHook(() => useCodexThread("s1", false));
    await flushMicro();
    expect(result.current.transport).toBe("off");
    expect(result.current.state).toBeNull();
    expect(MockEventSource.instances).toHaveLength(0);
    expect(threadGetCalls()).toBe(0);
  });

  it("refresh_pulls_get：refresh() 走 GET 并更新 state，不改变 transport", async () => {
    installFetch();
    routes.thread = projection({ revision: 11 });
    const { result } = renderHook(() => useCodexThread("s1", true));
    await act(async () => {
      await result.current.refresh();
    });
    expect(result.current.state?.revision).toBe(11);
    expect(result.current.transport).toBe("sse");
    expect(threadGetCalls()).toBe(1);
  });
});

describe("useCodexThread：3 次失败降级轮询（transport=poll）", () => {
  it("degrade_after_three_failures：SSE 连续 3 次失败 → 3s GET 轮询接管并更新 state", async () => {
    installFetch();
    routes.thread = projection({ revision: 20 });
    const { result } = renderHook(() => useCodexThread("s1", true));
    MockEventSource.latest().fail(); // 第 1 次 → 1s 退避
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1000);
    });
    MockEventSource.latest().fail(); // 第 2 次 → 2s 退避
    await act(async () => {
      await vi.advanceTimersByTimeAsync(2000);
    });
    MockEventSource.latest().fail(); // 第 3 次 → 达阈值降级
    await flushMicro();
    expect(result.current.transport).toBe("poll");
    // 降级即拉一次 + 3s 节奏轮询；GET 结果落 state（SSE 挂了也能看到投影）
    expect(threadGetCalls()).toBe(1);
    expect(result.current.state?.revision).toBe(20);
    routes.thread = projection({ revision: 21 });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(CODEX_THREAD_POLL_MS);
    });
    expect(threadGetCalls()).toBe(2);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(CODEX_THREAD_POLL_MS);
    });
    expect(threadGetCalls()).toBe(3);
    expect(result.current.state?.revision).toBe(21);
    // 降级后不再建 SSE 连接
    expect(MockEventSource.instances).toHaveLength(3);
  });

  it("unmount_closes_stream_and_stops_poll：卸载关流 + 停轮询 + 在途 fetch 作废", async () => {
    installFetch();
    routes.threadHang = true;
    const { result, unmount } = renderHook(() => useCodexThread("s1", true));
    // 降级 → 立即轮询一次（挂起）
    MockEventSource.latest().fail();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1000);
    });
    MockEventSource.latest().fail();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(2000);
    });
    MockEventSource.latest().fail();
    await flushMicro();
    expect(threadGetCalls()).toBe(1);
    unmount();
    expect(MockEventSource.instances.every((e) => e.closed)).toBe(true);
    // 卸载后：定时器清零、挂起响应放行也不落 state
    const released = releaseThread!;
    routes.threadHang = false;
    await act(async () => {
      await vi.advanceTimersByTimeAsync(CODEX_THREAD_POLL_MS * 5);
    });
    expect(threadGetCalls()).toBe(1); // 无新增轮询
    await act(async () => {
      released(projection({ revision: 99 }));
    });
    expect(result.current.state).toBeNull(); // 在途响应不得写入已卸载的 hook
  });

  it("poll_failure_keeps_last_view：轮询单次失败保留最后视图（stale 语义的第一道防线）", async () => {
    installFetch();
    routes.thread = projection({ revision: 30 });
    const { result } = renderHook(() => useCodexThread("s1", true));
    for (let i = 0; i < 3; i += 1) {
      MockEventSource.latest().fail();
      // 线性退避 1s/2s（第 3 次直接降级，无退避）
      if (i < 2) {
        await act(async () => {
          await vi.advanceTimersByTimeAsync(1000 * (i + 1));
        });
      }
    }
    await flushMicro();
    expect(result.current.state?.revision).toBe(30);
    routes.threadNetworkFail = true;
    await act(async () => {
      await vi.advanceTimersByTimeAsync(CODEX_THREAD_POLL_MS);
    });
    expect(result.current.state?.revision).toBe(30); // 失败不清空
  });
});

describe("useCodexThread：session 切换防护（不串台）", () => {
  it("session_switch_discards_stale：切会话清旧 state；旧会话在途轮询响应与旧 ES 帧都不得写入新会话", async () => {
    installFetch();
    routes.threadHang = true; // s1 的轮询挂起（降级后发出）
    const { result, rerender } = renderHook(
      ({ sid }: { sid: string }) => useCodexThread(sid, true),
      { initialProps: { sid: "s1" } }
    );
    // s1：SSE 三连失败降级 → 立即 GET（挂起中）
    MockEventSource.latest().fail();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1000);
    });
    MockEventSource.latest().fail();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(2000);
    });
    MockEventSource.latest().fail();
    await flushMicro();
    expect(threadGetCalls()).toBe(1);
    const s1Es = MockEventSource.latest();

    // 切到 s2：state 立即清空（旧投影不得跨会话显示）
    routes.threadHang = false;
    routes.thread = projection({ threadId: "th-2", revision: 1 });
    rerender({ sid: "s2" });
    await flushMicro();
    expect(result.current.state).toBeNull();
    expect(s1Es.closed).toBe(true); // 旧流已关

    // s1 的挂起轮询响应此刻才到 → 不得盖到 s2（过时响应不盖新 session）
    const release = releaseThread;
    releaseThread = null;
    await act(async () => {
      release?.(projection({ threadId: "th-1", revision: 42 }));
    });
    expect(result.current.state).toBeNull();

    // s2 自己的 SSE 快照正常落 state
    const s2Es = MockEventSource.latest();
    expect(s2Es).not.toBe(s1Es);
    s2Es.emit("snapshot", projection({ threadId: "th-2", revision: 5 }));
    await act(async () => {});
    expect(result.current.state?.threadId).toBe("th-2");
    expect(result.current.state?.revision).toBe(5);
  });
});

describe("useCodexThread：403 设备失效", () => {
  it("poll_403_keeps_quiet：轮询遇 403（fetchCodexThread 返回 null）不清空已有视图", async () => {
    installFetch();
    routes.thread = projection({ revision: 50 });
    const { result } = renderHook(() => useCodexThread("s1", true));
    await act(async () => {
      await result.current.refresh();
    });
    expect(result.current.state?.revision).toBe(50);
    MockEventSource.latest().fail();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1000);
    });
    MockEventSource.latest().fail();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(2000);
    });
    MockEventSource.latest().fail();
    routes.threadStatus = 403;
    await flushMicro();
    expect(result.current.transport).toBe("poll"); // 降级即接管（与上方用例同一时点）
    await act(async () => {
      await vi.advanceTimersByTimeAsync(CODEX_THREAD_POLL_MS);
    });
    expect(threadGetCalls()).toBeGreaterThanOrEqual(2); // 轮询在走（403 被吞掉）
    expect(result.current.state?.revision).toBe(50); // null 不覆盖最后视图
  });
});
