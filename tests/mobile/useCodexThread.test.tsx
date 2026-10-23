import { act, cleanup, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import useCodexThread, {
  CODEX_THREAD_POLL_MS,
  type CodexHistoryLoadResult,
} from "@/mobile/useCodexThread";
import { CODEX_HISTORY_MAX_BYTES, CODEX_HISTORY_MAX_PAGES } from "@/mobile/codex-history";
import type {
  CapabilitySet,
  MessagesPage,
  ProjectedMessage,
  ThreadProjection,
} from "@/mobile/codexIpcApi";
import { MockEventSource } from "./eventSourceMock";

// Task 9：useCodexThread 行为锁（计划 Step 1「SSE 首帧/增量、3 次失败降级轮询、
// 卸载关流、session 切换不串台」逐条落地）。SSE 用 MockEventSource 手动驱动；
// fetch 全量 stub（GET /m/api/v1/codex/thread 与 Task 5 的 /codex/thread/messages）。
//
// Task 5：追加「实时投影 + 有限历史页合并」锁——state.messages 保持实时尾窗，
// loadedMessages = 尾窗 + 已合并更早页；token 换代/线程重建/禁用订阅/切会话清空
// 页、在途请求与游标；旧响应与旧 token 不得落地。

function caps(over: Partial<CapabilitySet> = {}): CapabilitySet {
  return {
    send: true,
    state: true,
    command: true,
    permissions: true,
    blockingInput: true,
    interrupt: false,
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
    contentState: "ready",
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
  /** Task 5：历史页（按 before_id 分派，缺省 end） */
  page?: MessagesPage;
  pageByBefore?: (before: string | null) => MessagesPage | undefined;
  pageStatus?: number;
  pageHang?: boolean;
  pageNetworkFail?: boolean;
}

let routes: Routes;
let fetchMock: ReturnType<typeof vi.fn>;
let releaseThread: ((p: ThreadProjection) => void) | null = null;
let pendingPage: {
  url: string;
  signal: AbortSignal | undefined;
  release: (p: MessagesPage) => void;
} | null = null;

beforeEach(() => {
  routes = {};
  releaseThread = null;
  pendingPage = null;
  MockEventSource.reset();
  vi.useFakeTimers();
  vi.stubGlobal("EventSource", MockEventSource);
});
afterEach(() => {
  cleanup();
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

/** Task 3/5 消息默认值（测试夹具；服务端新载荷恒带这些字段） */
function msg(id: string, over: Partial<ProjectedMessage> = {}): ProjectedMessage {
  return {
    id,
    role: "user",
    kind: "user_input",
    text: id,
    ts: null,
    turnId: null,
    artifacts: [],
    turnImageCount: 0,
    omittedArtifactCount: 0,
    ...over,
  };
}

/** 旧投影消息（无 Task 3/4 新字段） */
function legacyMessage(id: string) {
  return { id, role: "user", kind: "user_input", text: id, ts: 1000 };
}

function endPage(): MessagesPage {
  return {
    messages: [],
    historyToken: "",
    revision: 7,
    nextBefore: null,
    status: "end",
    reasonCode: null,
  };
}

function page(over: Partial<MessagesPage> = {}): MessagesPage {
  return { ...endPage(), status: "ready", ...over };
}

function installFetch() {
  fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    // 判序：/codex/thread/messages ⊃ /codex/thread
    if (url.includes("/codex/thread/messages")) {
      if (routes.pageHang) {
        // 真实 fetch 会因 AbortSignal 拒绝：挂起页请求必须同样可中止，否则测试只
        // 观察得到落地后的代际校验，覆盖不到「在途请求被 abort」这条主路径。
        return new Promise<Response>((resolve, reject) => {
          const signal = init?.signal ?? undefined;
          if (signal?.aborted) {
            reject(new DOMException("The operation was aborted.", "AbortError"));
            return;
          }
          signal?.addEventListener(
            "abort",
            () => reject(new DOMException("The operation was aborted.", "AbortError")),
            { once: true }
          );
          pendingPage = {
            url,
            signal,
            release: (p) => resolve(new Response(JSON.stringify(p), { status: 200 })),
          };
        });
      }
      if (routes.pageNetworkFail) throw new TypeError("messages page network down");
      if (routes.pageStatus) return new Response("no", { status: routes.pageStatus });
      const before = new URL(url, "http://localhost").searchParams.get("before_id");
      const p = routes.pageByBefore?.(before) ?? routes.page ?? endPage();
      return new Response(JSON.stringify(p), { status: 200 });
    }
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

/** GET /codex/thread 调用次数（轮询节奏断言用；不含 /codex/thread/messages） */
function threadGetCalls(): number {
  return fetchMock.mock.calls.filter(([u]) => String(u).includes("/codex/thread?")).length;
}

/** 历史页请求 URL 列表（Task 5） */
function pageCalls(): string[] {
  return fetchMock.mock.calls
    .map(([u]) => String(u))
    .filter((u) => u.includes("/codex/thread/messages"));
}

function pageParam(call: string, name: string): string | null {
  return new URL(call, "http://localhost").searchParams.get(name);
}

function ids(messages: readonly ProjectedMessage[]): string[] {
  return messages.map((m) => m.id);
}

/** R2 接缝夹具：连续数字 id 的尾窗（如 `windowMessages(100, 500)` = m100..m599）。 */
function windowMessages(start: number, count: number): ProjectedMessage[] {
  return Array.from({ length: count }, (_, i) => msg(`m${start + i}`));
}

/** R2：合并视图必须是连续 id（无空洞、无重复、顺序不变）。 */
function expectContiguousMessages(
  messages: readonly ProjectedMessage[],
  start: number,
  count: number
) {
  const nums = messages.map((m) => Number(m.id.slice(1)));
  expect(nums).toHaveLength(count);
  expect(nums).toEqual(Array.from({ length: count }, (_, i) => start + i));
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

// ==== Task 5：实时投影 + 有限历史页合并 ====

describe("useCodexThread：历史分页合并与失效清理（Task 5）", () => {
  it("loadEarlier_merges_page_keeps_tail_window：更早页并入 loadedMessages，state.messages 仍是实时尾窗", async () => {
    installFetch();
    const { result } = renderHook(() => useCodexThread("s1", true));
    const es = MockEventSource.latest();
    es.emit("snapshot", projection({ historyToken: "v1-tok1", messages: [msg("m3")] }));
    await act(async () => {});
    expect(result.current.loadedMessages).toEqual(result.current.state?.messages);

    routes.page = page({
      messages: [msg("m1"), msg("m2")],
      nextBefore: "m1",
      historyToken: "v1-page2",
    });
    let loaded: CodexHistoryLoadResult | undefined;
    await act(async () => {
      loaded = await result.current.loadEarlier();
    });
    expect(loaded?.status).toBe("ready");
    expect(loaded?.page?.nextBefore).toBe("m1");
    expect(ids(result.current.loadedMessages)).toEqual(["m1", "m2", "m3"]);
    expect(ids(result.current.state!.messages)).toEqual(["m3"]); // 尾窗语义不变
    expect(result.current.historyPages).toHaveLength(1);
    expect(pageCalls()).toHaveLength(1);
    expect(pageParam(pageCalls()[0], "session_id")).toBe("s1");
    expect(pageParam(pageCalls()[0], "history_token")).toBe("v1-tok1");
    expect(pageParam(pageCalls()[0], "before_id")).toBe("m3");
    expect(pageParam(pageCalls()[0], "limit")).toBe("50");

    // 第二页沿游标链：上一页的 nextBefore + historyToken
    routes.page = page({ messages: [msg("m0")], nextBefore: "m0", historyToken: "" });
    await act(async () => {
      loaded = await result.current.loadEarlier();
    });
    expect(ids(result.current.loadedMessages)).toEqual(["m0", "m1", "m2", "m3"]);
    expect(pageParam(pageCalls()[1], "history_token")).toBe("v1-page2");
    expect(pageParam(pageCalls()[1], "before_id")).toBe("m1");
    expect(result.current.historyPages.map((p) => p.beforeId)).toEqual(["m3", "m1"]);

    // 上一页已给不出下一页 token（服务端「不可继续」）→ 后续 loadEarlier 直接到头、零请求
    await act(async () => {
      loaded = await result.current.loadEarlier();
    });
    expect(loaded?.status).toBe("end");
    expect(pageCalls()).toHaveLength(2);
  });

  it("sse_same_token_keeps_pages：同代际 SSE 帧不冲掉有效更早页，尾窗追加无接缝重复", async () => {
    installFetch();
    const { result } = renderHook(() => useCodexThread("s1", true));
    const es = MockEventSource.latest();
    es.emit("snapshot", projection({ historyToken: "v1-tok1", messages: [msg("m3")] }));
    await act(async () => {});
    routes.page = page({
      messages: [msg("m1"), msg("m2")],
      nextBefore: "m1",
      historyToken: "v1-page2",
    });
    await act(async () => {
      await result.current.loadEarlier();
    });
    expect(result.current.historyPages).toHaveLength(1);

    // 同 token 的新帧（尾部追加）：更早页保留，合并无重复
    es.emit(
      "state",
      projection({ historyToken: "v1-tok1", revision: 8, messages: [msg("m3"), msg("m4")] })
    );
    await act(async () => {});
    expect(ids(result.current.loadedMessages)).toEqual(["m1", "m2", "m3", "m4"]);
    expect(ids(result.current.state!.messages)).toEqual(["m3", "m4"]);
    expect(result.current.historyPages).toHaveLength(1);

    // 重连首帧重发同一尾窗：不产生重复
    es.emit("snapshot", projection({ historyToken: "v1-tok1", messages: [msg("m3"), msg("m4")] }));
    await act(async () => {});
    expect(ids(result.current.loadedMessages)).toEqual(["m1", "m2", "m3", "m4"]);
  });

  it("token_change_clears_pages_and_drops_late_page：token 换代清空页/在途/游标，旧页迟到响应不落地", async () => {
    installFetch();
    const { result } = renderHook(() => useCodexThread("s1", true));
    const es = MockEventSource.latest();
    es.emit("snapshot", projection({ historyToken: "v1-tok1", messages: [msg("m3")] }));
    await act(async () => {});

    routes.pageHang = true;
    let pending!: Promise<CodexHistoryLoadResult>;
    await act(async () => {
      pending = result.current.loadEarlier();
      await Promise.resolve();
    });
    expect(pageCalls()).toHaveLength(1);

    // 历史代际换代（线程重建/尾窗前移）：清空缓存与游标
    es.emit("state", projection({ historyToken: "v1-tok2", messages: [msg("m4")] }));
    await act(async () => {});
    expect(result.current.historyPages).toHaveLength(0);
    expect(ids(result.current.loadedMessages)).toEqual(["m4"]);
    // 真实 abort 路径：在途页请求已收到 AbortSignal（不是靠响应落地后的代际校验）
    expect(pendingPage!.signal?.aborted).toBe(true);

    // 旧 token 的页此刻才返回 → 不得回填
    let late: CodexHistoryLoadResult | undefined;
    await act(async () => {
      pendingPage!.release(
        page({ messages: [msg("m1"), msg("m2")], nextBefore: "m1", historyToken: "v1-page2" })
      );
      late = await pending;
    });
    expect(late?.status).toBe("unavailable");
    expect(late?.reasonCode).toBe("stale_response");
    expect(late?.page).toBeNull();
    expect(result.current.historyPages).toHaveLength(0);
    expect(ids(result.current.loadedMessages)).toEqual(["m4"]);

    // 下一次定位使用当前尾窗与当前 token（旧游标链已清）
    routes.pageHang = false;
    routes.page = page({ messages: [msg("m3")], nextBefore: "m3", historyToken: "v1-page2b" });
    await act(async () => {
      await result.current.loadEarlier();
    });
    expect(pageParam(pageCalls().at(-1)!, "history_token")).toBe("v1-tok2");
    expect(pageParam(pageCalls().at(-1)!, "before_id")).toBe("m4");
  });

  it("history_rewrite_clears_cache_and_reanchors：cursor_invalid 清页并结束旧游标链，重试从当前尾窗锚定", async () => {
    installFetch();
    const { result } = renderHook(() => useCodexThread("s1", true));
    const es = MockEventSource.latest();
    es.emit("snapshot", projection({ historyToken: "v1-tok1", messages: [msg("m3")] }));
    await act(async () => {});
    routes.page = page({
      messages: [msg("m1"), msg("m2")],
      nextBefore: "m1",
      historyToken: "v1-page2",
    });
    await act(async () => {
      await result.current.loadEarlier();
    });
    expect(result.current.historyPages).toHaveLength(1);

    // 已分页区域被改写：服务端 cursor_invalid/history_rewritten
    routes.page = page({
      status: "cursor_invalid",
      reasonCode: "history_rewritten",
      messages: [],
      nextBefore: null,
      historyToken: "",
    });
    let res: CodexHistoryLoadResult | undefined;
    await act(async () => {
      res = await result.current.loadEarlier();
    });
    expect(res?.status).toBe("cursor_invalid");
    expect(res?.reasonCode).toBe("history_rewritten");
    expect(res?.page).toBeNull();
    expect(result.current.historyPages).toHaveLength(0);
    expect(ids(result.current.loadedMessages)).toEqual(["m3"]);

    // 重试重新锚定当前尾窗（不沿用失效游标）
    routes.page = page({ messages: [msg("m0")], nextBefore: "m0", historyToken: "v1-page3" });
    await act(async () => {
      await result.current.loadEarlier();
    });
    expect(pageParam(pageCalls().at(-1)!, "before_id")).toBe("m3");
    expect(pageParam(pageCalls().at(-1)!, "history_token")).toBe("v1-tok1");
  });

  it("explicit_pages_keep_cache_order：连续从显式游标取页后缓存仍按新→旧排列（T1）", async () => {
    installFetch();
    const { result } = renderHook(() => useCodexThread("s1", true));
    MockEventSource.latest().emit(
      "snapshot",
      projection({ historyToken: "v1-tok1", messages: [msg("m100")] })
    );
    await act(async () => {});

    // 先取 m50—m99（游标 m100），再取 m0—m49（游标 m50）：第二页更旧，必须排在后面，
    // 否则共享选择器先看到 m0—m49 就判缺口，两页一起出局、池窗口只剩尾窗。
    routes.pageByBefore = (before) =>
      before === "m100"
        ? page({
            messages: [msg("m50"), msg("m99")],
            nextBefore: "m50",
            historyToken: "v1-p1",
          })
        : page({
            messages: [msg("m0"), msg("m49")],
            nextBefore: "m0",
            historyToken: "v1-p2",
          });
    await act(async () => {
      await result.current.loadEarlier("m100");
    });
    await act(async () => {
      await result.current.loadEarlier("m50");
    });

    expect(result.current.historyPages.map((entry) => entry.beforeId)).toEqual(["m100", "m50"]);
  });

  it("token_change_beats_retention：已有保留缓冲时，删除换来的新 token 会清空它（跨层）", async () => {
    installFetch();
    const { result } = renderHook(() => useCodexThread("s1", true));
    const es = MockEventSource.latest();
    es.emit(
      "snapshot",
      projection({
        historyToken: "v2-tokA",
        messages: [msg("m0"), msg("m1"), msg("m2"), msg("m3")],
      })
    );
    await act(async () => {});
    // 先让缓冲非空：同 token 尾窗前移，m0 滚出尾窗进入保留缓冲
    es.emit(
      "state",
      projection({
        historyToken: "v2-tokA",
        messages: [msg("m1"), msg("m2"), msg("m3"), msg("m4")],
      })
    );
    await act(async () => {});
    expect(ids(result.current.retained)).toEqual(["m0"]);

    // 最新 Turn 内删除 m2：服务端内容代推进 → 新 token。客户端必须先失效再显示
    // 权威投影，不能靠单调性检查去猜——m2 既不在新尾窗、也不该留在缓冲里。
    es.emit(
      "state",
      projection({
        historyToken: "v2-tokB",
        messages: [msg("m1"), msg("m3"), msg("m4")],
      })
    );
    await act(async () => {});

    expect(ids(result.current.retained)).toEqual([]);
    expect(ids(result.current.loadedMessages)).toEqual(["m1", "m3", "m4"]);
    expect(ids(result.current.loadedMessages)).not.toContain("m2");
  });

  it("cursor_invalid_clears_retained：已知失效证据必须连同保留缓冲一起作废", async () => {
    installFetch();
    const { result } = renderHook(() => useCodexThread("s1", true));
    const es = MockEventSource.latest();
    es.emit("snapshot", projection({ historyToken: "v1-tok1", messages: [msg("m0"), msg("m1")] }));
    await act(async () => {});
    // 同 token 尾窗前移一条：m0 滚出尾窗进入保留缓冲，仍在列表里显示
    es.emit("state", projection({ historyToken: "v1-tok1", messages: [msg("m1"), msg("m2")] }));
    await act(async () => {});
    expect(ids(result.current.retained)).toEqual(["m0"]);
    expect(ids(result.current.loadedMessages)).toEqual(["m0", "m1", "m2"]);

    routes.page = page({
      status: "cursor_invalid",
      reasonCode: "history_rewritten",
      messages: [],
      nextBefore: null,
      historyToken: "",
    });
    await act(async () => {
      await result.current.loadEarlier();
    });

    // 复核 F5：游标失效已经证明历史不可信——不能等「下一帧 SSE 换 token」才处理，
    // 否则 SSE 断开/轮询窗口内旧消息仍冒充当前权威内容。
    expect(ids(result.current.retained)).toEqual([]);
    expect(ids(result.current.loadedMessages)).toEqual(["m1", "m2"]);
  });

  it("session_switch_clears_pages_and_drops_late_page：切会话清页与在途，旧会话页响应不得回填", async () => {
    installFetch();
    const { result, rerender } = renderHook(
      ({ sid }: { sid: string }) => useCodexThread(sid, true),
      { initialProps: { sid: "s1" } }
    );
    MockEventSource.latest().emit(
      "snapshot",
      projection({ historyToken: "v1-tok1", messages: [msg("m3")] })
    );
    await act(async () => {});
    routes.pageHang = true;
    let pending!: Promise<CodexHistoryLoadResult>;
    await act(async () => {
      pending = result.current.loadEarlier();
      await Promise.resolve();
    });
    expect(pageCalls()).toHaveLength(1);

    rerender({ sid: "s2" });
    await act(async () => {});
    expect(result.current.historyPages).toHaveLength(0);
    expect(result.current.state).toBeNull();
    expect(result.current.loadedMessages).toEqual([]);

    // s1 的迟到页响应：不得写入 s2
    let late: CodexHistoryLoadResult | undefined;
    await act(async () => {
      pendingPage!.release(
        page({ messages: [msg("m1"), msg("m2")], nextBefore: "m1", historyToken: "v1-page2" })
      );
      late = await pending;
    });
    expect(late?.status).toBe("unavailable");
    expect(late?.reasonCode).toBe("stale_response");
    expect(result.current.historyPages).toHaveLength(0);
    expect(ids(result.current.loadedMessages)).toEqual([]);

    // s2 自己的代际/尾窗独立
    const s2Es = MockEventSource.latest();
    s2Es.emit(
      "snapshot",
      projection({ threadId: "th-2", historyToken: "v2-tok1", messages: [msg("x1")] })
    );
    await act(async () => {});
    expect(ids(result.current.loadedMessages)).toEqual(["x1"]);
    routes.pageHang = false;
    routes.page = page({ messages: [msg("x0")], nextBefore: "x0", historyToken: "v2-page2" });
    await act(async () => {
      await result.current.loadEarlier();
    });
    expect(pageParam(pageCalls().at(-1)!, "history_token")).toBe("v2-tok1");
    expect(pageParam(pageCalls().at(-1)!, "before_id")).toBe("x1");
  });

  it("disabled_subscription_clears_pages_and_blocks_loading：禁用订阅清页/游标，loadEarlier 不再发请求", async () => {
    installFetch();
    const { result, rerender } = renderHook(
      ({ enabled }: { enabled: boolean }) => useCodexThread("s1", enabled),
      { initialProps: { enabled: true } }
    );
    MockEventSource.latest().emit(
      "snapshot",
      projection({ historyToken: "v1-tok1", messages: [msg("m3")] })
    );
    await act(async () => {});
    routes.page = page({ messages: [msg("m1")], nextBefore: "m1", historyToken: "v1-page2" });
    await act(async () => {
      await result.current.loadEarlier();
    });
    expect(result.current.historyPages).toHaveLength(1);

    // 在途页请求 + 禁用订阅：清空页/游标并作废在途请求
    routes.pageHang = true;
    let pending!: Promise<CodexHistoryLoadResult>;
    await act(async () => {
      pending = result.current.loadEarlier();
      await Promise.resolve();
    });
    expect(pageCalls()).toHaveLength(2);

    rerender({ enabled: false });
    await act(async () => {});
    expect(result.current.transport).toBe("off");
    expect(result.current.historyPages).toHaveLength(0);
    // 现有 state 语义保留（最后尾窗视图不清空——详情页继续可用）
    expect(ids(result.current.state!.messages)).toEqual(["m3"]);

    let late: CodexHistoryLoadResult | undefined;
    await act(async () => {
      pendingPage!.release(
        page({ messages: [msg("m1")], nextBefore: "m1", historyToken: "v1-page2" })
      );
      late = await pending;
    });
    expect(late?.status).toBe("unavailable");
    expect(late?.reasonCode).toBe("stale_response");
    expect(result.current.historyPages).toHaveLength(0);

    const res = await result.current.loadEarlier();
    expect(res.status).toBe("unavailable");
    expect(res.reasonCode).toBe("subscription_disabled");
    expect(pageCalls()).toHaveLength(2);
  });

  it("cache_caps_four_pages_and_reanchors_without_gap：第五页按预算丢弃，显示无空洞，下次重新锚定尾窗", async () => {
    installFetch();
    const { result } = renderHook(() => useCodexThread("s1", true));
    MockEventSource.latest().emit(
      "snapshot",
      projection({ historyToken: "v1-tok1", messages: [msg("m6")] })
    );
    await act(async () => {});
    const chain: Record<string, string> = { m6: "m5", m5: "m4", m4: "m3", m3: "m2", m2: "m1" };
    routes.pageByBefore = (before) => {
      const id = before === null ? undefined : chain[before];
      return id
        ? page({ messages: [msg(id)], nextBefore: id, historyToken: `v1-${id}` })
        : endPage();
    };
    let fifth: CodexHistoryLoadResult | undefined;
    for (let i = 0; i < 5; i += 1) {
      await act(async () => {
        fifth = await result.current.loadEarlier();
      });
    }
    // 被展示缓存预算丢弃的页仍交给调用方（Task 7 可逐页搜索本次结果）
    expect(fifth?.status).toBe("ready");
    expect(ids(fifth?.page?.messages ?? [])).toEqual(["m1"]);
    expect(result.current.historyPages).toHaveLength(CODEX_HISTORY_MAX_PAGES);
    expect(result.current.historyPages.map((p) => p.beforeId)).toEqual(["m6", "m5", "m4", "m3"]);
    expect(result.current.historyPages.every((p) => p.messages.length <= 50)).toBe(true);
    const totalBytes = result.current.historyPages.reduce((sum, p) => sum + p.bytes, 0);
    expect(totalBytes).toBeLessThanOrEqual(CODEX_HISTORY_MAX_BYTES);
    // 第五页（m1）被预算丢弃，但已合并视图连续（m2..m6）——不是半页空洞
    expect(ids(result.current.loadedMessages)).toEqual(["m2", "m3", "m4", "m5", "m6"]);

    // 下一次定位重新锚定当前尾窗（不跳过被丢弃页造成 m1/m2 空洞）
    await act(async () => {
      await result.current.loadEarlier();
    });
    expect(pageParam(pageCalls().at(-1)!, "before_id")).toBe("m6");
    expect(pageParam(pageCalls().at(-1)!, "history_token")).toBe("v1-tok1");
  });

  it("legacy_payload_without_history_token_degrades：旧帧无 token 只显示尾窗，loadEarlier 不发请求", async () => {
    installFetch();
    const { result } = renderHook(() => useCodexThread("s1", true));
    MockEventSource.latest().emit("snapshot", projection({ messages: [legacyMessage("m3")] }));
    await act(async () => {});
    expect(result.current.state?.historyToken).toBe("");
    expect(result.current.loadedMessages[0]).toMatchObject({
      turnId: null,
      artifacts: [],
      turnImageCount: 0,
      omittedArtifactCount: 0,
    });

    const res = await result.current.loadEarlier();
    expect(res.status).toBe("unavailable");
    expect(res.reasonCode).toBe("history_token_missing");
    expect(pageCalls()).toHaveLength(0);
  });

  it("refresh_and_poll_share_token_invalidation：手动刷新与降级轮询拿到新代际同样清空旧页", async () => {
    installFetch();
    const { result } = renderHook(() => useCodexThread("s1", true));
    const es = MockEventSource.latest();
    es.emit("snapshot", projection({ historyToken: "v1-tok1", messages: [msg("m3")] }));
    await act(async () => {});
    routes.page = page({ messages: [msg("m1")], nextBefore: "m1", historyToken: "v1-page2" });
    await act(async () => {
      await result.current.loadEarlier();
    });
    expect(result.current.historyPages).toHaveLength(1);

    // 手动刷新拿到新代际：清旧页
    routes.thread = projection({ historyToken: "v1-tok2", messages: [msg("m4")] });
    await act(async () => {
      await result.current.refresh();
    });
    expect(result.current.historyPages).toHaveLength(0);
    expect(ids(result.current.loadedMessages)).toEqual(["m4"]);

    // 新代际下再加载一页
    routes.page = page({ messages: [msg("m0")], nextBefore: "m0", historyToken: "v1-page3" });
    await act(async () => {
      await result.current.loadEarlier();
    });
    expect(result.current.historyPages).toHaveLength(1);
    expect(pageParam(pageCalls().at(-1)!, "history_token")).toBe("v1-tok2");

    // SSE 三连失败 → 3s GET 轮询；轮询拿到下一代 token 同样清页
    routes.thread = projection({ historyToken: "v1-tok3", messages: [msg("m5")] });
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
    expect(result.current.transport).toBe("poll");
    expect(result.current.historyPages).toHaveLength(0);
    expect(ids(result.current.loadedMessages)).toEqual(["m5"]);
  });

  it("refresh_late_response_with_old_token_dropped：手动刷新在途时已有新帧落地 → 旧响应不覆盖新状态", async () => {
    installFetch();
    const { result } = renderHook(() => useCodexThread("s1", true));
    const es = MockEventSource.latest();
    es.emit("snapshot", projection({ historyToken: "v1-tok1", messages: [msg("m3")] }));
    await act(async () => {});

    routes.threadHang = true;
    let pending!: Promise<void>;
    await act(async () => {
      pending = result.current.refresh();
      await Promise.resolve();
    });
    // 刷新在途期间 SSE 推进到新代际
    es.emit("state", projection({ historyToken: "v1-tok2", messages: [msg("m4")] }));
    await act(async () => {});
    await act(async () => {
      releaseThread!(projection({ historyToken: "v1-tok1", messages: [msg("m3")] }));
      await pending;
    });
    // 旧 token 的手动刷新响应作废：不覆盖、也不清掉当前代际缓存（此处缓存为空）
    expect(result.current.state?.historyToken).toBe("v1-tok2");
    expect(ids(result.current.loadedMessages)).toEqual(["m4"]);
  });

  it("page_over_limit_is_rejected_without_touching_cache_or_chain：超 50 条页是协议违约，不裁剪、不消耗游标", async () => {
    installFetch();
    const { result } = renderHook(() => useCodexThread("s1", true));
    MockEventSource.latest().emit(
      "snapshot",
      projection({ historyToken: "v1-tok1", messages: [msg("m3")] })
    );
    await act(async () => {});
    routes.page = page({
      messages: [msg("m1"), msg("m2")],
      nextBefore: "m1",
      historyToken: "v1-page2",
    });
    await act(async () => {
      await result.current.loadEarlier();
    });
    expect(result.current.historyPages).toHaveLength(1);
    const before = ids(result.current.loadedMessages);

    // 服务端契约保证 ≤50 条；51 条是协议违约——不裁剪成「最新 50 条」，不推进游标链
    const overLimit = Array.from({ length: 51 }, (_, i) => msg(`x${i + 1}`));
    routes.page = page({ messages: overLimit, nextBefore: "x1", historyToken: "v1-violation" });
    let res: CodexHistoryLoadResult | undefined;
    await act(async () => {
      res = await result.current.loadEarlier();
    });
    expect(res?.status).toBe("unavailable");
    expect(res?.reasonCode).toBe("page_over_limit");
    expect(res?.page).toBeNull();
    // 既有缓存与合并视图原样保留（不裁剪出假页，也不丢已有页）
    expect(result.current.historyPages.map((p) => p.beforeId)).toEqual(["m3"]);
    expect(ids(result.current.loadedMessages)).toEqual(before);

    // 游标链未被消耗：下一次仍从原游标/原 token 继续，而不是跳过被裁剪的旧消息
    routes.page = page({ messages: [msg("m0")], nextBefore: "m0", historyToken: "v1-page3" });
    await act(async () => {
      await result.current.loadEarlier();
    });
    expect(pageParam(pageCalls().at(-1)!, "before_id")).toBe("m1");
    expect(pageParam(pageCalls().at(-1)!, "history_token")).toBe("v1-page2");
    expect(result.current.historyPages).toHaveLength(2);
  });

  it("page_network_failure_keeps_request_failed：真实网络失败仍是加载失败（区别于代际失效）", async () => {
    installFetch();
    const { result } = renderHook(() => useCodexThread("s1", true));
    MockEventSource.latest().emit(
      "snapshot",
      projection({ historyToken: "v1-tok1", messages: [msg("m3")] })
    );
    await act(async () => {});
    routes.pageNetworkFail = true;
    let res: CodexHistoryLoadResult | undefined;
    await act(async () => {
      res = await result.current.loadEarlier();
    });
    expect(res?.status).toBe("unavailable");
    expect(res?.reasonCode).toBe("request_failed");
    expect(result.current.historyPages).toHaveLength(0);
  });
});

// ==== R2 客户端接缝：尾窗前移时滚出窗口的消息不得消失 ====

describe("useCodexThread：R2 尾窗接缝保留（同 token 尾窗前移）", () => {
  it("window_rollover_keeps_dropped_message_with_page：有缓存页时滚出窗口的消息仍在合并视图，连续无洞无重", async () => {
    installFetch();
    const { result } = renderHook(() => useCodexThread("s1", true));
    const es = MockEventSource.latest();
    // 饱和尾窗：500 条 m100..m599
    es.emit(
      "snapshot",
      projection({ historyToken: "v1-tok1", messages: windowMessages(100, 500) })
    );
    await act(async () => {});
    expect(result.current.state?.messages).toHaveLength(500);

    // 更早一页 m50..m99，游标锚在尾窗最早一条 m100
    routes.page = page({
      messages: windowMessages(50, 50),
      nextBefore: "m50",
      historyToken: "v1-page2",
    });
    await act(async () => {
      await result.current.loadEarlier();
    });
    expect(result.current.historyPages).toHaveLength(1);
    expect(pageParam(pageCalls()[0], "before_id")).toBe("m100");

    // 同一 token，尾窗前移一条：m100 滚出窗口，既不在新尾窗也不在缓存页
    es.emit(
      "state",
      projection({ historyToken: "v1-tok1", revision: 8, messages: windowMessages(101, 500) })
    );
    await act(async () => {});

    // 稳定 token 不清页；滚出的 m100 仍可见，合并视图 m50..m600 连续
    expect(result.current.historyPages).toHaveLength(1);
    expect(ids(result.current.loadedMessages)).toContain("m100");
    expectContiguousMessages(result.current.loadedMessages, 50, 551);
    // state.messages 仍是实时尾窗（语义不变）
    expect(ids(result.current.state!.messages)).toEqual(ids(windowMessages(101, 500)));
  });

  it("window_rollover_keeps_dropped_message_without_page：没有缓存页时滚出的消息仍保留在新尾窗之前", async () => {
    installFetch();
    const { result } = renderHook(() => useCodexThread("s1", true));
    const es = MockEventSource.latest();
    es.emit(
      "snapshot",
      projection({ historyToken: "v1-tok1", messages: windowMessages(100, 500) })
    );
    await act(async () => {});
    expect(result.current.historyPages).toHaveLength(0);

    es.emit(
      "state",
      projection({ historyToken: "v1-tok1", revision: 8, messages: windowMessages(101, 500) })
    );
    await act(async () => {});

    // 接缝只有一条 m100：新尾窗之前恰好保留它，视图 m100..m600 连续
    expectContiguousMessages(result.current.loadedMessages, 100, 501);
    expect(result.current.loadedMessages[0].id).toBe("m100");
    expect(result.current.loadedMessages[1].id).toBe("m101");
    expect(result.current.state!.messages[0].id).toBe("m101");
  });

  it("token_change_clears_retained：token 换代清空保留缓冲，loadedMessages 只剩新尾窗", async () => {
    installFetch();
    const { result } = renderHook(() => useCodexThread("s1", true));
    const es = MockEventSource.latest();
    es.emit(
      "snapshot",
      projection({ historyToken: "v1-tok1", messages: windowMessages(100, 500) })
    );
    await act(async () => {});
    es.emit(
      "state",
      projection({ historyToken: "v1-tok1", revision: 8, messages: windowMessages(101, 500) })
    );
    await act(async () => {});
    expect(ids(result.current.loadedMessages)).toContain("m100");

    // 历史换代（重建/改写）：保留缓冲与页缓存一并失效
    es.emit(
      "state",
      projection({ historyToken: "v1-tok2", revision: 9, messages: windowMessages(200, 500) })
    );
    await act(async () => {});
    expect(result.current.historyPages).toHaveLength(0);
    expect(ids(result.current.loadedMessages)).toEqual(ids(windowMessages(200, 500)));
    expect(ids(result.current.loadedMessages)).not.toContain("m100");
  });

  it("session_switch_clears_retained：切会话清空保留缓冲，旧会话滚出的消息不得串台", async () => {
    installFetch();
    const { result, rerender } = renderHook(
      ({ sid }: { sid: string }) => useCodexThread(sid, true),
      { initialProps: { sid: "s1" } }
    );
    const s1Es = MockEventSource.latest();
    s1Es.emit(
      "snapshot",
      projection({ historyToken: "v1-tok1", messages: windowMessages(100, 500) })
    );
    await act(async () => {});
    s1Es.emit(
      "state",
      projection({ historyToken: "v1-tok1", revision: 8, messages: windowMessages(101, 500) })
    );
    await act(async () => {});
    expect(ids(result.current.loadedMessages)).toContain("m100");

    // 切会话：旧投影/页/保留缓冲一律清空
    rerender({ sid: "s2" });
    await act(async () => {});
    expect(result.current.loadedMessages).toEqual([]);
    expect(result.current.historyPages).toHaveLength(0);

    // s2 自己的尾窗独立；旧会话的 m100 不得因保留缓冲跨会话出现
    MockEventSource.latest().emit(
      "snapshot",
      projection({ threadId: "th-2", historyToken: "v2-tok1", messages: [msg("x1")] })
    );
    await act(async () => {});
    expect(ids(result.current.loadedMessages)).toEqual(["x1"]);
  });
});
