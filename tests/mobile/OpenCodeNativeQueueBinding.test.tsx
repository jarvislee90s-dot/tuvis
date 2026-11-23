import { act, cleanup, render, screen, waitFor } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import OpenCodeNativeQueue, { resetQueueControlMemoryForTests } from "@/mobile/OpenCodeNativeQueue";

function outbox(label: "A" | "B", revision: number) {
  return new Response(
    JSON.stringify({
      sessionId: "s",
      revision,
      paused: false,
      items: [
        {
          actionId: label,
          owner: "tuvis",
          state: "queued",
          reasonCode: "OC_BINDING_STALE",
          textSummary: `队列 ${label}`,
          attachmentCount: 0,
        },
      ],
    })
  );
}

afterEach(() => {
  cleanup();
  resetQueueControlMemoryForTests();
  sessionStorage.clear();
  vi.unstubAllGlobals();
});

it.each([
  { description: "首次尚未取得读取绑定", readFailed: false },
  { description: "视图读取失败且无有效 pin", readFailed: true },
])("$description 时不读取或显示旧队列", async ({ readFailed }) => {
  const fetchMock = vi.fn(async () => outbox("A", 10));
  vi.stubGlobal("fetch", fetchMock);

  render(
    <OpenCodeNativeQueue
      sessionId="s"
      bindingToken={null}
      readBindingToken={null}
      readFailed={readFailed}
    />
  );
  await act(async () => {});

  expect(screen.queryByText("队列 A")).not.toBeInTheDocument();
  expect(fetchMock).not.toHaveBeenCalled();
});

it("取得 B 读取绑定后带绑定头请求 B 队列", async () => {
  const fetchMock = vi.fn(async (_input: RequestInfo | URL, _init?: RequestInit) => outbox("B", 1));
  vi.stubGlobal("fetch", fetchMock);
  const view = render(
    <OpenCodeNativeQueue sessionId="s" bindingToken={null} readBindingToken={null} />
  );

  view.rerender(
    <OpenCodeNativeQueue sessionId="s" bindingToken="opaque-b" readBindingToken="opaque-b" />
  );
  expect(await screen.findByText("队列 B")).toBeInTheDocument();
  const boundCalls = fetchMock.mock.calls.filter(
    ([input, init]) =>
      String(input).includes("/opencode/outbox") &&
      new Headers(init?.headers).get("X-OpenCode-Expected-Binding") === "opaque-b"
  );
  expect(boundCalls).toHaveLength(1);
});

it("切换到 B 后 A 的较高 revision 不阻止显示 B", async () => {
  vi.stubGlobal(
    "fetch",
    vi.fn(async (_input: RequestInfo | URL, init?: RequestInit) => {
      const binding = new Headers(init?.headers).get("X-OpenCode-Expected-Binding");
      return binding === "opaque-a" ? outbox("A", 10) : outbox("B", 1);
    })
  );
  const view = render(
    <OpenCodeNativeQueue sessionId="s" bindingToken="opaque-a" readBindingToken="opaque-a" />
  );
  await screen.findByText("队列 A");
  view.rerender(
    <OpenCodeNativeQueue sessionId="s" bindingToken="opaque-b" readBindingToken="opaque-b" />
  );

  expect(await screen.findByText("队列 B")).toBeInTheDocument();
  expect(screen.queryByText("队列 A")).not.toBeInTheDocument();
});

it("A 迟到的队列回执不能覆盖已经显示的 B", async () => {
  let resolveA!: (response: Response) => void;
  const pendingA = new Promise<Response>((resolve) => {
    resolveA = resolve;
  });
  const fetchMock = vi.fn((_input: RequestInfo | URL, init?: RequestInit) => {
    const binding = new Headers(init?.headers).get("X-OpenCode-Expected-Binding");
    return binding === "opaque-a" ? pendingA : Promise.resolve(outbox("B", 1));
  });
  vi.stubGlobal("fetch", fetchMock);
  const view = render(
    <OpenCodeNativeQueue sessionId="s" bindingToken="opaque-a" readBindingToken="opaque-a" />
  );
  await waitFor(() => expect(fetchMock).toHaveBeenCalledTimes(1));
  view.rerender(
    <OpenCodeNativeQueue sessionId="s" bindingToken="opaque-b" readBindingToken="opaque-b" />
  );
  await screen.findByText("队列 B");

  await act(async () => {
    resolveA(outbox("A", 10));
  });
  expect(screen.getByText("队列 B")).toBeInTheDocument();
  expect(screen.queryByText("队列 A")).not.toBeInTheDocument();
});
