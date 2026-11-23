import { act, cleanup, renderHook, waitFor } from "@testing-library/react";
import { useState } from "react";
import { afterEach, expect, it, vi } from "vitest";
import useOpenCodeNative from "@/mobile/useOpenCodeNative";
import { MockEventSource } from "./eventSourceMock";

afterEach(() => {
  cleanup();
  MockEventSource.reset();
  vi.unstubAllGlobals();
});

// 首次选择返回 A；另一页面切换到 B 后，旧页面必须继续发送 A 的绑定。
// 普通读取使用 X-OpenCode-Expected-Binding；EventSource 使用查询参数。
function installTwoTargets() {
  let switched = false;
  const fetchMock = vi.fn(async (_input: RequestInfo | URL, init?: RequestInit) => {
    const expected = new Headers(init?.headers).get("X-OpenCode-Expected-Binding");
    if (switched && expected === "opaque-a") {
      return new Response(JSON.stringify({ error: "OC_BINDING_STALE" }), { status: 409 });
    }
    return new Response(
      JSON.stringify({
        sessionId: "sess-1",
        bindingToken: switched ? "opaque-b" : "opaque-a",
        revision: switched ? 2 : 1,
        messages: [],
        pending: [],
        inbox: [],
        route: "opencode_native",
        capabilities: {},
        complete: true,
      }),
      { status: 200 }
    );
  });
  vi.stubGlobal("fetch", fetchMock);
  vi.stubGlobal("EventSource", MockEventSource);
  return {
    fetchMock,
    switchToB: () => {
      switched = true;
    },
  };
}

it("旧绑定失效时无绑定只读恢复一次并保留草稿与锚", async () => {
  const { fetchMock, switchToB } = installTwoTargets();
  const { result } = renderHook(() => {
    const [draft] = useState("未发送草稿");
    const [anchor] = useState("question-anchor");
    return { native: useOpenCodeNative("sess-1", true), draft, anchor };
  });
  await waitFor(() => expect(result.current.native.view?.bindingToken).toBe("opaque-a"));
  switchToB();
  act(() => result.current.native.refresh());
  await waitFor(() => expect(result.current.native.view?.bindingToken).toBe("opaque-b"));
  expect(
    fetchMock.mock.calls.map(([, init]) =>
      new Headers(init?.headers).get("X-OpenCode-Expected-Binding")
    )
  ).toEqual([null, "opaque-a", null]);
  expect(result.current.native.readFailed).toBe(false);
  expect(result.current.draft).toBe("未发送草稿");
  expect(result.current.anchor).toBe("question-anchor");
  await act(async () => {
    await Promise.resolve();
  });
  expect(fetchMock).toHaveBeenCalledTimes(3);
});

it("自动恢复的无绑定读取也失效时停止重试", async () => {
  const fetchMock = vi.fn(
    async (_input: RequestInfo | URL, _init?: RequestInit) =>
      new Response(JSON.stringify({ error: "OC_BINDING_STALE" }), { status: 409 })
  );
  vi.stubGlobal("fetch", fetchMock);
  vi.stubGlobal("EventSource", MockEventSource);
  const { result } = renderHook(() => useOpenCodeNative("sess-1", true, undefined, "opaque-a"));
  await waitFor(() => expect(result.current.readFailed).toBe(true));
  await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(2));
  expect(
    fetchMock.mock.calls.map(([, init]) =>
      new Headers((init as RequestInit | undefined)?.headers).get("X-OpenCode-Expected-Binding")
    )
  ).toEqual(["opaque-a", null]);
  await act(async () => {
    await Promise.resolve();
  });
  expect(fetchMock).toHaveBeenCalledTimes(2);
});

it("订阅事件时使用已选择的 A 绑定", async () => {
  installTwoTargets();
  const { result } = renderHook(() => useOpenCodeNative("sess-1", true));
  await waitFor(() => expect(result.current.view?.bindingToken).toBe("opaque-a"));
  await waitFor(() => {
    const streams = MockEventSource.instances.filter((stream) => !stream.closed);
    expect(streams).toHaveLength(1);
    expect(
      new URL(streams[0].url, "http://localhost").searchParams.get("expected_binding_token")
    ).toBe("opaque-a");
  });
});
