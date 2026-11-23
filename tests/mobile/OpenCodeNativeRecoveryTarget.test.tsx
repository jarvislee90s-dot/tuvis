import {
  act,
  cleanup,
  fireEvent,
  render,
  renderHook,
  screen,
  waitFor,
} from "@testing-library/react";
import { useState } from "react";
import { afterEach, expect, it, vi } from "vitest";
import OpenCodeNativePanel from "@/mobile/OpenCodeNativePanel";
import useOpenCodeNative from "@/mobile/useOpenCodeNative";
import i18n from "@/i18n";
import { MockEventSource } from "./eventSourceMock";

afterEach(() => {
  cleanup();
  MockEventSource.reset();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

const original = {
  sessionId: "sess-1",
  bindingToken: "opaque-a",
  directory: "/project/a",
  serviceId: "service-a",
  servicePid: 100,
  revision: 1,
  messages: [
    { seq: 0, role: "assistant", kind: "assistant", content: "Original view", collapsed: false },
  ],
  pending: [],
  inbox: [],
  route: "opencode_native",
  capabilities: {},
  complete: true,
};

function installRecovery(next: typeof original, code = "OC_BINDING_STALE") {
  let recovering = false;
  const reads: Array<string | null> = [];
  const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    const binding = new Headers(init?.headers).get("X-OpenCode-Expected-Binding");
    if (url.includes("/opencode/view")) {
      reads.push(binding);
      if (recovering && binding === "opaque-a") {
        return new Response(JSON.stringify({ error: code }), { status: 409 });
      }
      return new Response(JSON.stringify(recovering ? next : original), { status: 200 });
    }
    if (url.includes("/opencode/outbox")) {
      return new Response(
        JSON.stringify({ sessionId: "sess-1", revision: 0, paused: false, items: [] })
      );
    }
    if (url.includes("/opencode/agents")) {
      return new Response(JSON.stringify({ current: null, options: [] }));
    }
    if (url.includes("/channel"))
      return new Response(JSON.stringify({ via: "lan", limited: false }));
    throw new Error(`unexpected fetch: ${url}`);
  });
  vi.stubGlobal("fetch", fetchMock);
  vi.stubGlobal("EventSource", MockEventSource);
  return {
    reads,
    recover: () => {
      recovering = true;
    },
  };
}

const restarted = { ...original, bindingToken: "opaque-b", servicePid: 200, revision: 2 };

it.each([
  ["directory", { ...restarted, directory: "/project/b" }],
  ["service", { ...restarted, serviceId: "service-b" }],
])("rejects recovery when the %s changes and retains the old view", async (_field, next) => {
  const state = installRecovery(next);
  const onSnapshot = vi.fn();
  const { result } = renderHook(() => useOpenCodeNative("sess-1", true, onSnapshot));
  await waitFor(() => expect(result.current.view?.bindingToken).toBe("opaque-a"));
  state.recover();
  act(() => result.current.refresh());
  await waitFor(() => expect(result.current.error?.data?.error).toBe("OC_TARGET_CHANGED"));
  expect(result.current.readFailed).toBe(true);
  expect(result.current.view).toMatchObject(original);
  expect(onSnapshot).toHaveBeenCalledTimes(1);
  expect(state.reads).toEqual([null, "opaque-a", null]);
  expect(MockEventSource.instances.filter((stream) => !stream.closed)).toHaveLength(0);
  await act(async () => {
    await Promise.resolve();
  });
  expect(state.reads).toHaveLength(3);
});

it("rejects the different target on every repeated recovery read", async () => {
  const state = installRecovery({ ...restarted, directory: "/project/b" });
  const onSnapshot = vi.fn();
  const { result } = renderHook(() => useOpenCodeNative("sess-1", true, onSnapshot));
  await waitFor(() => expect(result.current.view?.bindingToken).toBe("opaque-a"));
  state.recover();
  act(() => result.current.refresh());
  await waitFor(() => expect(result.current.error?.data?.error).toBe("OC_TARGET_CHANGED"));

  for (let attempt = 0; attempt < 3; attempt++) {
    await act(async () => {
      result.current.refresh();
    });
    await waitFor(() => expect(state.reads).toHaveLength(4 + attempt));
    expect(state.reads.at(-1)).toBeNull();
    expect(result.current.view).toMatchObject(original);
    expect(result.current.error?.data?.error).toBe("OC_TARGET_CHANGED");
    expect(result.current.readFailed).toBe(true);
    expect(onSnapshot).toHaveBeenCalledTimes(1);
  }
});

it("keeps explicit reselection visible when recovery reaches a different target", async () => {
  const state = installRecovery({ ...restarted, directory: "/project/b", serviceId: "service-b" });
  render(<OpenCodeNativePanel sessionId="sess-1" />);
  await waitFor(() => expect(MockEventSource.instances).toHaveLength(1));
  state.recover();
  act(() => MockEventSource.latest().emit("revision", "2"));
  expect(await screen.findByTestId("native-reselect")).toBeVisible();
  expect(screen.getByTestId("native-load-error")).toBeVisible();
  fireEvent.click(screen.getByTestId("native-messages-toggle"));
  expect(screen.getByText("Original view")).toBeVisible();
  expect(state.reads).toEqual([null, "opaque-a", null]);
  for (let attempt = 0; attempt < 3; attempt++) {
    await act(async () => {
      fireEvent.click(screen.getByTestId("native-retry"));
    });
    await waitFor(() => expect(state.reads).toHaveLength(4 + attempt));
    expect(state.reads.at(-1)).toBeNull();
    expect(screen.getByTestId("native-load-error")).toBeVisible();
    expect(screen.getByText("Original view")).toBeVisible();
  }
});

// H6（HF）：换代目标漂移（服务重启换登记 id / 目录漂移）后，恢复核对失败的
// 错误提示必须走**可操作**的本地化文案（指向「重新选择当前目标」），不得落
// 通用句、不得上屏机器串——三段链第一段（错误提示）的码面钉约。
it("H6：换代目标漂移的错误提示走可操作文案（不落通用句、不露机器串）", async () => {
  const state = installRecovery({ ...restarted, directory: "/project/b" });
  render(<OpenCodeNativePanel sessionId="sess-1" />);
  await waitFor(() => expect(MockEventSource.instances).toHaveLength(1));
  state.recover();
  act(() => MockEventSource.latest().emit("revision", "2"));
  const banner = await screen.findByTestId("native-load-error");
  expect(banner.textContent).toContain(i18n.t("mobile.openCodeNative.errors.targetChanged"));
  expect(banner.textContent).not.toBe(i18n.t("mobile.openCodeNative.errors.generic"));
  expect(banner.textContent).not.toContain("OC_TARGET_CHANGED");
  // 上游英文 reason 原文不得原样上屏（本地化英文句子共享措辞不算原文直出）
  expect(banner.textContent?.trim()).not.toBe("native target changed during recovery");
  // 可操作动作：错误面重选入口在场（恢复不成功时仍有路可走）
  expect(screen.getByTestId("native-reselect")).toBeVisible();
});

it("accepts a restarted service in the same directory while preserving draft and anchor", async () => {
  const state = installRecovery(restarted);
  const { result } = renderHook(() => {
    const [draft] = useState("未发送草稿");
    const [anchor] = useState("question-anchor");
    return { native: useOpenCodeNative("sess-1", true), draft, anchor };
  });
  await waitFor(() => expect(result.current.native.view?.bindingToken).toBe("opaque-a"));
  state.recover();
  act(() => result.current.native.refresh());
  await waitFor(() => expect(result.current.native.view?.bindingToken).toBe("opaque-b"));
  expect(result.current.native.view).toMatchObject(restarted);
  expect(result.current.native.readFailed).toBe(false);
  expect(result.current.draft).toBe("未发送草稿");
  expect(result.current.anchor).toBe("question-anchor");
  expect(state.reads).toEqual([null, "opaque-a", null]);
  await waitFor(() => expect(MockEventSource.latest().url).toContain("opaque-b"));
});

it("keeps accepting same-target reads after recovering a restarted service", async () => {
  const state = installRecovery(restarted);
  const { result } = renderHook(() => useOpenCodeNative("sess-1", true));
  await waitFor(() => expect(result.current.view?.bindingToken).toBe("opaque-a"));
  state.recover();
  act(() => result.current.refresh());
  await waitFor(() => expect(result.current.view?.bindingToken).toBe("opaque-b"));

  for (let attempt = 0; attempt < 3; attempt++) {
    await act(async () => {
      result.current.refresh();
    });
    await waitFor(() => expect(state.reads).toHaveLength(4 + attempt));
    expect(state.reads.at(-1)).toBe("opaque-b");
    expect(result.current.view).toMatchObject(restarted);
    expect(result.current.error).toBeNull();
  }
});

it.each([
  ["same directory", true],
  ["different directory", false],
])("checks subsequent recovery against the explicitly selected target: %s", async (_name, same) => {
  const selected = {
    ...restarted,
    routeVersion: 2,
    directory: "/project/b",
    serviceId: "service-b",
    messages: [{ ...original.messages[0], content: "Selected view" }],
  };
  const afterRestart = {
    ...selected,
    bindingToken: "opaque-c",
    directory: same ? selected.directory : "/project/c",
    servicePid: 300,
    revision: 3,
    messages: [{ ...original.messages[0], content: "Restarted view" }],
  };
  const windowIdentity = { pid: selected.servicePid, startedAtMs: 1, boot: null };
  let requestId: string;
  const receipt = () => ({
    sessionId: selected.sessionId,
    route: selected.route,
    requestId,
    state: "committed",
    current: true,
    bindingToken: selected.bindingToken,
    routeVersion: selected.routeVersion,
    window: windowIdentity,
  });
  let phase: "initial" | "changed" | "selected" | "restarted" = "initial";
  const reads: Array<string | null> = [];
  const events: string[] = [];
  vi.stubGlobal("EventSource", MockEventSource);
  vi.spyOn(window, "confirm").mockImplementation(() => {
    events.push("confirm");
    return true;
  });
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url.includes("/opencode/route/preview?")) {
        events.push("preview");
        return new Response(
          JSON.stringify({
            sessionId: "sess-1",
            candidate: {
              sessionId: "sess-1",
              directory: selected.directory,
              serviceId: selected.serviceId,
              ...windowIdentity,
              bindingToken: selected.bindingToken,
            },
            previewToken: "preview-token",
            expiresAtMs: 9999999999999,
          })
        );
      }
      if (url.endsWith("/opencode/route/native")) {
        events.push("route");
        const request = JSON.parse(String(init?.body));
        expect(request.candidateToken).toBe("preview-token");
        expect(request.requestId).toEqual(expect.stringMatching(/\S/));
        requestId = request.requestId;
        phase = "selected";
        return new Response(JSON.stringify(receipt()));
      }
      if (url.includes("/opencode/route/native/status?")) {
        events.push("route status");
        const query = new URL(url, "http://localhost").searchParams;
        expect(init?.method ?? "GET").toBe("GET");
        expect(query.get("sessionId")).toBe(selected.sessionId);
        expect(query.get("requestId")).toBe(requestId);
        return new Response(JSON.stringify(receipt()));
      }
      if (url.includes("/opencode/view")) {
        const binding = new Headers(init?.headers).get("X-OpenCode-Expected-Binding");
        reads.push(binding);
        if (
          (phase === "changed" && binding === "opaque-a") ||
          (phase === "restarted" && binding === "opaque-b")
        ) {
          return new Response(JSON.stringify({ error: "OC_BINDING_STALE" }), { status: 409 });
        }
        if (phase === "selected") events.push("selected view");
        return new Response(
          JSON.stringify(
            phase === "initial" ? original : phase === "restarted" ? afterRestart : selected
          )
        );
      }
      if (url.includes("/opencode/outbox"))
        return new Response(
          JSON.stringify({ sessionId: "sess-1", revision: 0, paused: false, items: [] })
        );
      if (url.includes("/opencode/agents"))
        return new Response(JSON.stringify({ current: null, options: [] }));
      if (url.includes("/channel"))
        return new Response(JSON.stringify({ via: "lan", limited: false }));
      throw new Error(`unexpected fetch: ${url}`);
    })
  );
  render(<OpenCodeNativePanel sessionId="sess-1" />);
  await waitFor(() => expect(MockEventSource.instances).toHaveLength(1));
  fireEvent.click(screen.getByTestId("native-messages-toggle"));
  expect(screen.getByText("Original view")).toBeVisible();
  phase = "changed";
  act(() => MockEventSource.latest().emit("revision", "2"));
  fireEvent.click(await screen.findByTestId("native-reselect"));
  expect(await screen.findByText("Selected view")).toBeVisible();
  expect(events).toEqual(["preview", "confirm", "route", "route status", "selected view"]);
  expect(reads).toEqual([null, "opaque-a", null, "opaque-b"]);
  await waitFor(() => expect(MockEventSource.latest().url).toContain("opaque-b"));

  phase = "restarted";
  act(() => MockEventSource.latest().emit("revision", "3"));
  await waitFor(() => expect(reads.slice(-2)).toEqual(["opaque-b", null]));
  if (same) {
    expect(await screen.findByText("Restarted view")).toBeVisible();
    expect(screen.queryByTestId("native-load-error")).toBeNull();
    await waitFor(() => expect(MockEventSource.latest().url).toContain("opaque-c"));
  } else {
    expect(await screen.findByTestId("native-load-error")).toBeVisible();
    expect(screen.getByText("Selected view")).toBeVisible();
    expect(screen.queryByText("Restarted view")).toBeNull();
  }
});

it("recovers once without a binding when the old runtime is unverified", async () => {
  const state = installRecovery(restarted, "OC_RUNTIME_UNVERIFIED");
  const { result } = renderHook(() => useOpenCodeNative("sess-1", true));
  await waitFor(() => expect(result.current.view?.bindingToken).toBe("opaque-a"));
  state.recover();
  act(() => result.current.refresh());
  await waitFor(() => expect(result.current.view?.bindingToken).toBe("opaque-b"));
  expect(result.current.error).toBeNull();
  expect(state.reads).toEqual([null, "opaque-a", null]);
  await act(async () => {
    await Promise.resolve();
  });
  expect(state.reads).toHaveLength(3);
});

it("accepts an unbound first read and records its target for later recovery checks", async () => {
  const first = { ...restarted, directory: "/project/b", serviceId: "service-b" };
  const reads: Array<string | null> = [];
  vi.stubGlobal("EventSource", MockEventSource);
  vi.stubGlobal(
    "fetch",
    vi.fn(async (_input: RequestInfo | URL, init?: RequestInit) => {
      const binding = new Headers(init?.headers).get("X-OpenCode-Expected-Binding");
      reads.push(binding);
      if (binding === "opaque-b") {
        return new Response(JSON.stringify({ error: "OC_BINDING_STALE" }), { status: 409 });
      }
      const response = reads.length === 1 ? first : { ...original, bindingToken: "opaque-c" };
      return new Response(JSON.stringify(response), { status: 200 });
    })
  );
  const { result } = renderHook(() => useOpenCodeNative("sess-1", true));
  await waitFor(() => expect(result.current.view?.bindingToken).toBe("opaque-b"));
  expect(result.current.view).toMatchObject(first);
  expect(result.current.error).toBeNull();
  await waitFor(() => expect(MockEventSource.latest().url).toContain("opaque-b"));
  act(() => result.current.refresh());
  await waitFor(() => expect(result.current.error?.data?.error).toBe("OC_TARGET_CHANGED"));
  expect(result.current.view).toMatchObject(first);
  expect(reads).toEqual([null, "opaque-b", null]);
});
