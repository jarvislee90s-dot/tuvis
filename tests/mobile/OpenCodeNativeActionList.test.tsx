import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import OpenCodeNativePanel from "@/mobile/OpenCodeNativePanel";
import { ACTION_ANCHOR_CHANGED_EVENT, readActionAnchors } from "@/mobile/opencodeActionAnchors";
import { MockEventSource } from "./eventSourceMock";

const sessionA = "action-list-session-a";
const sessionB = "action-list-session-b";
const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status });
const acceptedSend = {
  actionId: "server-only-send",
  operation: "send",
  state: "acknowledged",
  nativeId: "msg_server_only_send",
};

const deadSend = { ...acceptedSend, runtimeDead: true };

function mockActionLists() {
  const requests: Array<{ sessionId: string; respond: (response: Response) => void }> = [];
  const verifyRequests: Array<{ respond: (response: Response) => void }> = [];
  vi.stubGlobal("EventSource", MockEventSource as unknown as typeof EventSource);
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = new URL(String(input), "http://localhost");
      const sessionId = url.searchParams.get("sessionId") ?? sessionA;
      if (
        init?.method === "POST" &&
        url.pathname.endsWith(`/opencode/actions/${acceptedSend.actionId}/verify`)
      ) {
        return new Promise<Response>((respond) => verifyRequests.push({ respond }));
      }
      if (init?.method && init.method !== "GET") throw new Error(`Unexpected write: ${url}`);
      if (url.pathname.endsWith("/opencode/actions")) {
        return new Promise<Response>((respond) => requests.push({ sessionId, respond }));
      }
      if (url.pathname.endsWith("/opencode/view")) {
        return json({
          sessionId,
          bindingToken: `binding-${sessionId}`,
          revision: 1,
          complete: true,
          capabilities: {},
          messages: [],
          pending: [],
          inbox: [],
          runtimeState: "ready",
          files: {},
          route: "opencode_native",
        });
      }
      if (url.pathname.endsWith("/opencode/agents")) return json({ current: null, options: [] });
      if (url.pathname.endsWith("/opencode/outbox")) {
        return json({ sessionId, revision: 0, paused: false, items: [] });
      }
      if (url.pathname.endsWith("/channel")) return json({ via: "lan", limited: false });
      throw new Error(`Unexpected request: ${url}`);
    })
  );
  return { requests, verifyRequests };
}

function refreshActionList(sessionId: string) {
  act(() => {
    window.dispatchEvent(new CustomEvent(ACTION_ANCHOR_CHANGED_EVENT, { detail: { sessionId } }));
  });
}

beforeEach(() => {
  sessionStorage.clear();
  MockEventSource.reset();
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("OpenCode 原生动作清单刷新", () => {
  it.each([
    { state: "dispatching", nativeId: acceptedSend.nativeId },
    { state: "queued", nativeId: acceptedSend.nativeId },
    { state: "unconfirmed", nativeId: acceptedSend.nativeId },
    { state: "not_sent", nativeId: acceptedSend.nativeId },
    { state: "acknowledged", nativeId: undefined },
    { state: "acknowledged", nativeId: "" },
  ])(
    "服务端发现的 send 在 state=$state、nativeId=$nativeId 时不可当作已证受理归档",
    async (evidence) => {
      const { requests } = mockActionLists();
      render(<OpenCodeNativePanel sessionId={sessionA} />);
      const incompleteSend = { ...acceptedSend, ...evidence, actionId: "incomplete-send" };
      await act(async () =>
        requests[0].respond(
          json([incompleteSend, { ...acceptedSend, actionId: "live-send" }, deadSend])
        )
      );

      const rows = within(screen.getByTestId("native-unresolved-actions")).getAllByTestId(
        "native-action-anchor"
      );
      expect(rows.map((row) => row.dataset.actionId)).toEqual([
        incompleteSend.actionId,
        deadSend.actionId,
      ]);
      expect(rows[0]).toHaveAttribute("data-kind", "send");
      expect(within(rows[0]).getByTestId("native-action-verify")).toBeEnabled();
      expect(within(rows[0]).queryByTestId("native-action-confirm-unknown")).toBeNull();
      expect(within(rows[1]).getByTestId("native-action-confirm-unknown")).toBeEnabled();
    }
  );

  it("成功返回空清单后移除仅由服务端发现的动作行", async () => {
    const { requests } = mockActionLists();
    render(<OpenCodeNativePanel sessionId={sessionA} />);
    expect(requests.map((request) => request.sessionId)).toEqual([sessionA]);
    await act(async () => requests[0].respond(json([deadSend])));
    expect(screen.getByTestId("native-action-anchor").dataset.actionId).toBe(acceptedSend.actionId);
    expect(readActionAnchors(sessionA)).toEqual([]);

    refreshActionList(sessionA);
    expect(requests.map((request) => request.sessionId)).toEqual([sessionA, sessionA]);
    await act(async () => requests[1].respond(json([])));

    expect(screen.queryByTestId("native-action-anchor")).toBeNull();
  });

  it("成功刷新缺少死亡证据时收回归档按钮并隐藏已受理动作行", async () => {
    const { requests } = mockActionLists();
    render(<OpenCodeNativePanel sessionId={sessionA} />);
    await act(async () => requests[0].respond(json([deadSend])));
    const initialRow = screen.getByTestId("native-action-anchor");
    expect(within(initialRow).getByTestId("native-action-confirm-unknown")).toBeTruthy();
    expect(readActionAnchors(sessionA)).toEqual([]);

    refreshActionList(sessionA);
    expect(requests.map((request) => request.sessionId)).toEqual([sessionA, sessionA]);
    await act(async () => requests[1].respond(json([acceptedSend])));

    expect(screen.queryByTestId("native-unresolved-actions")).toBeNull();
  });

  it("较早发起的手动核对晚于清单返回时不能恢复已收回的归档按钮", async () => {
    const { requests, verifyRequests } = mockActionLists();
    render(<OpenCodeNativePanel sessionId={sessionA} />);
    await act(async () => requests[0].respond(json([deadSend])));
    const row = within(screen.getByTestId("native-action-anchor"));
    expect(row.getByTestId("native-action-confirm-unknown")).toBeTruthy();

    fireEvent.click(row.getByTestId("native-action-verify"));
    expect(verifyRequests).toHaveLength(1);
    refreshActionList(sessionA);
    expect(requests).toHaveLength(2);
    await act(async () => requests[1].respond(json([acceptedSend])));
    expect(screen.queryByTestId("native-action-anchor")).toBeNull();

    await act(async () => verifyRequests[0].respond(json(deadSend)));

    expect(screen.queryByTestId("native-action-anchor")).toBeNull();
    expect(verifyRequests).toHaveLength(1);
  });

  it("清单请求失败时保留最近一次成功的动作行并提示失败", async () => {
    const { requests } = mockActionLists();
    render(<OpenCodeNativePanel sessionId={sessionA} />);
    await act(async () => requests[0].respond(json([deadSend])));
    expect(screen.queryByTestId("native-action-list-error")).toBeNull();

    refreshActionList(sessionA);
    expect(requests.map((request) => request.sessionId)).toEqual([sessionA, sessionA]);
    await act(async () => requests[1].respond(json({ error: "unavailable" }, 503)));

    expect(screen.getByTestId("native-action-anchor").dataset.actionId).toBe(acceptedSend.actionId);
    const error = screen.getByTestId("native-action-list-error");
    expect(error.getAttribute("role")).toBe("status");
    expect(error.textContent?.trim()).toBeTruthy();
  });

  it("切换至 B 后，A 的迟到清单不会替换 B 的动作行", async () => {
    const { requests } = mockActionLists();
    const panel = render(<OpenCodeNativePanel sessionId={sessionA} />);
    panel.rerender(<OpenCodeNativePanel sessionId={sessionB} />);
    expect(requests.map((request) => request.sessionId)).toEqual([sessionA, sessionB]);

    const actionB = { ...deadSend, actionId: "server-only-send-b" };
    await act(async () => requests[1].respond(json([actionB])));
    await act(async () => requests[0].respond(json([deadSend])));

    expect(
      screen.getAllByTestId("native-action-anchor").map((row) => row.dataset.actionId)
    ).toEqual([actionB.actionId]);
  });
});
