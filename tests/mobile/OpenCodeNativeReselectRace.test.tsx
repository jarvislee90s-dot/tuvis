import { act, cleanup, renderHook, waitFor } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import useOpenCodeNative from "@/mobile/useOpenCodeNative";
import { MockEventSource } from "./eventSourceMock";

const view = (bindingToken: string, revision = 1) =>
  new Response(
    JSON.stringify({
      sessionId: "sess-1",
      bindingToken,
      revision,
      messages: [],
      pending: [],
      inbox: [],
      route: "opencode_native",
      capabilities: {},
      complete: true,
    })
  );

afterEach(() => {
  cleanup();
  MockEventSource.reset();
  vi.unstubAllGlobals();
});

it("普通刷新不能中止在途显式重选", async () => {
  let completeSelection!: (response: Response) => void;
  const selection = new Promise<Response>((resolve) => {
    completeSelection = resolve;
  });
  let count = 0;
  const requests: Array<{ expected: string | null; signal?: AbortSignal | null }> = [];
  vi.stubGlobal("EventSource", MockEventSource);
  vi.stubGlobal(
    "fetch",
    vi.fn((_input: RequestInfo | URL, init?: RequestInit) => {
      requests.push({
        expected: new Headers(init?.headers).get("X-OpenCode-Expected-Binding"),
        signal: init?.signal,
      });
      count += 1;
      if (count === 1) return Promise.resolve(view("binding-a"));
      if (count === 2) return selection;
      return Promise.resolve(view("binding-b", 3));
    })
  );
  const { result } = renderHook(() => useOpenCodeNative("sess-1", true));
  await waitFor(() => expect(result.current.view?.bindingToken).toBe("binding-a"));
  act(() => {
    void result.current.reselect("binding-b");
  });
  await waitFor(() => expect(requests).toHaveLength(2));
  expect.soft(requests[1].expected).toBe("binding-b");
  act(() => result.current.refresh());
  expect(requests).toHaveLength(2);
  await act(async () => {
    completeSelection(view("binding-b", 2));
    await selection;
  });
  await waitFor(() => expect(result.current.view?.bindingToken).toBe("binding-b"));
  expect(requests[1].signal?.aborted).toBe(false);
  await waitFor(() => expect(requests).toHaveLength(3));
  expect(requests[2].expected).toBe("binding-b");
});

it("SSE 事件不能覆盖在途显式重选", async () => {
  let completeSelection!: (response: Response) => void;
  const selection = new Promise<Response>((resolve) => {
    completeSelection = resolve;
  });
  const requests: Array<string | null> = [];
  vi.stubGlobal("EventSource", MockEventSource);
  vi.stubGlobal(
    "fetch",
    vi.fn((_input: RequestInfo | URL, init?: RequestInit) => {
      requests.push(new Headers(init?.headers).get("X-OpenCode-Expected-Binding"));
      if (requests.length === 1) return Promise.resolve(view("binding-a"));
      if (requests.length === 2) return selection;
      return Promise.resolve(view(requests.at(-1) ?? "binding-c", 4));
    })
  );
  const { result } = renderHook(() => useOpenCodeNative("sess-1", true));
  await waitFor(() => expect(result.current.view?.bindingToken).toBe("binding-a"));
  await waitFor(() => expect(MockEventSource.instances).toHaveLength(1));
  act(() => {
    void result.current.reselect("binding-b");
  });
  expect.soft(requests[1]).toBe("binding-b");
  act(() => MockEventSource.latest().emit("revision", 3));
  await act(async () => {
    completeSelection(view("binding-b", 2));
    await selection;
  });
  await waitFor(() => expect(result.current.view?.bindingToken).toBe("binding-b"));
  await waitFor(() => expect(requests.length).toBeGreaterThan(2));
  expect(requests.slice(2)).toEqual(["binding-b"]);
});

it("重选在途时迟到的 stale 读取不触发无绑定恢复，待刷新使用新绑定", async () => {
  let rejectOldRead!: (response: Response) => void;
  let completeSelection!: (response: Response) => void;
  const oldRead = new Promise<Response>((resolve) => {
    rejectOldRead = resolve;
  });
  const selection = new Promise<Response>((resolve) => {
    completeSelection = resolve;
  });
  const requests: Array<string | null> = [];
  vi.stubGlobal("EventSource", MockEventSource);
  vi.stubGlobal(
    "fetch",
    vi.fn((_input: RequestInfo | URL, init?: RequestInit) => {
      requests.push(new Headers(init?.headers).get("X-OpenCode-Expected-Binding"));
      if (requests.length === 1) return Promise.resolve(view("binding-a"));
      if (requests.length === 2) return oldRead;
      if (requests.length === 3) return selection;
      return Promise.resolve(view("binding-b", 3));
    })
  );
  const { result } = renderHook(() => useOpenCodeNative("sess-1", true));
  await waitFor(() => expect(result.current.view?.bindingToken).toBe("binding-a"));
  act(() => result.current.refresh());
  expect(requests).toEqual([null, "binding-a"]);
  act(() => {
    void result.current.reselect("binding-b");
  });
  expect(requests).toEqual([null, "binding-a", "binding-b"]);
  await act(async () => {
    rejectOldRead(new Response(JSON.stringify({ error: "OC_BINDING_STALE" }), { status: 409 }));
    await oldRead;
  });
  expect(requests).toEqual([null, "binding-a", "binding-b"]);
  act(() => result.current.refresh());
  expect(requests).toHaveLength(3);
  await act(async () => {
    completeSelection(view("binding-b", 2));
    await selection;
  });
  await waitFor(() => expect(requests).toEqual([null, "binding-a", "binding-b", "binding-b"]));
  await waitFor(() => expect(result.current.view?.revision).toBe(3));
  expect(result.current.error).toBeNull();
});

it("显式选择 B 的读取返回 C 时保留 A 并报告绑定失效", async () => {
  const requests: Array<string | null> = [];
  vi.stubGlobal("EventSource", MockEventSource);
  vi.stubGlobal(
    "fetch",
    vi.fn((_input: RequestInfo | URL, init?: RequestInit) => {
      requests.push(new Headers(init?.headers).get("X-OpenCode-Expected-Binding"));
      return Promise.resolve(
        view(requests.length === 1 ? "binding-a" : "binding-c", requests.length)
      );
    })
  );
  const { result } = renderHook(() => useOpenCodeNative("sess-1", true));
  await waitFor(() => expect(result.current.view?.bindingToken).toBe("binding-a"));
  act(() => {
    void result.current.reselect("binding-b");
  });
  await waitFor(() => expect(requests).toHaveLength(2));
  expect.soft(requests[1]).toBe("binding-b");
  await waitFor(() => expect(result.current.readFailed).toBe(true));
  expect(result.current.view?.bindingToken).toBe("binding-a");
});
