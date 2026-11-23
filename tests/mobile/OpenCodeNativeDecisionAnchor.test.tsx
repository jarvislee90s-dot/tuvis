import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import OpenCodeNativePanel from "@/mobile/OpenCodeNativePanel";
import { readActionAnchors, resetActionAnchorMemoryForTests } from "@/mobile/opencodeActionAnchors";
import { MockEventSource } from "./eventSourceMock";

const sessionId = "decision-anchor-session";
const permission = {
  kind: "permission",
  id: "shared-request",
  title: "审批请求",
  decisions: ["once", "reject"],
  context_token: "context",
};
const response = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status });
let posts: Array<{ actionId: string; bindingToken: string }>;
let verifies: string[];
let verifyState: string;
let releaseVerifies: Array<(response: Response) => void>;
let holdVerify: boolean;
let stale: boolean;

function installServer() {
  vi.stubGlobal("EventSource", MockEventSource);
  vi.stubGlobal(
    "confirm",
    vi.fn(() => true)
  );
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url.includes("/opencode/actions/") && url.endsWith("/verify")) {
        const id = url.split("/").at(-2)!;
        verifies.push(id);
        if (holdVerify)
          return new Promise<Response>((resolve) => {
            releaseVerifies.push(resolve);
          });
        return response({ actionId: id, state: verifyState, reasonCode: "binding_stale" });
      }
      if (url.endsWith("/opencode/actions") && init?.method === "POST") {
        const body = JSON.parse(String(init.body)) as { actionId: string; bindingToken: string };
        posts.push(body);
        return response({ actionId: body.actionId, state: "unconfirmed" });
      }
      if (url.includes("/opencode/route/preview?")) {
        return response({
          sessionId,
          candidate: {
            sessionId,
            directory: "/project",
            serviceId: "service",
            pid: 1,
            startedAtMs: 1,
            bindingToken: "binding-b",
          },
          expiresAtMs: 9999999999999,
          previewToken: "preview-token",
        });
      }
      if (url.endsWith("/opencode/route/native")) {
        expect(JSON.parse(String(init?.body)).candidateToken).toBe("preview-token");
        return response({
          sessionId,
          bindingToken: "binding-b",
          routeVersion: 2,
          route: "opencode_native",
        });
      }
      if (url.includes("/opencode/view")) {
        const expected = new Headers(init?.headers).get("X-OpenCode-Expected-Binding");
        if (stale && expected === "binding-a") return response({ error: "OC_BINDING_STALE" }, 409);
        const bindingToken =
          expected === "binding-b" || (stale && !expected) ? "binding-b" : "binding-a";
        return response({
          sessionId,
          bindingToken,
          routeVersion: bindingToken === "binding-a" ? 1 : 2,
          revision: bindingToken === "binding-a" ? 1 : 2,
          complete: true,
          capabilities: {
            send: { allowed: true, evidence: "prompt", reasonCode: null },
            permission_once: { allowed: true, evidence: "once", reasonCode: null },
            permission_reject: { allowed: true, evidence: "reject", reasonCode: null },
          },
          messages: [],
          pending: [permission],
          inbox: [],
          runtimeState: "ready",
          files: {},
          route: "opencode_native",
          routeReasonCode: null,
        });
      }
      if (url.includes("/opencode/agents")) return response({ current: null, options: [] });
      if (url.includes("/opencode/outbox"))
        return response({ sessionId, revision: 0, paused: false, items: [] });
      if (url.includes("/opencode/actions/"))
        return response({ actionId: "unknown", state: "unconfirmed" });
      throw new Error(`Unexpected request: ${url}`);
    })
  );
}

beforeEach(() => {
  sessionStorage.clear();
  resetActionAnchorMemoryForTests();
  MockEventSource.reset();
  posts = [];
  verifies = [];
  verifyState = "confirmed";
  releaseVerifies = [];
  holdVerify = false;
  stale = false;
  installServer();
});
afterEach(() => {
  cleanup();
  resetActionAnchorMemoryForTests();
  sessionStorage.clear();
  MockEventSource.reset();
  vi.unstubAllGlobals();
});

async function submitUnknown() {
  fireEvent.click((await screen.findAllByTestId("native-decision"))[0]);
  await waitFor(() =>
    expect(screen.getByTestId("native-decision-receipt")).toHaveAttribute("data-kind", "unknown")
  );
  const anchor = await screen.findByTestId("native-action-anchor");
  expect(anchor.dataset.actionId).toBe(posts[0].actionId);
  return anchor;
}

describe("审批公共核对同步", () => {
  it.each(["not_sent", "not_accepted"])("公共核对得到 %s 后审批行恢复可操作", async (state) => {
    verifyState = state;
    render(<OpenCodeNativePanel sessionId={sessionId} />);
    const anchor = await submitUnknown();
    fireEvent.click(within(anchor).getByTestId("native-action-verify"));
    await waitFor(() => expect(screen.queryByTestId("native-action-anchor")).toBeNull());
    await waitFor(() =>
      expect(screen.getByTestId("native-decision-receipt")).toHaveAttribute("data-kind", "rejected")
    );
    expect(screen.getByTestId("native-decision-receipt-code")).toHaveTextContent("binding_stale");
    expect(screen.getAllByTestId("native-decision")[0]).toBeEnabled();
    expect(screen.queryByTestId("native-decision-verify")).toBeNull();
    expect(readActionAnchors(sessionId)).toHaveLength(0);
    expect(verifies).toEqual([posts[0].actionId]);
    expect(posts).toHaveLength(1);
  });

  it.each([
    ["confirmed", "submitted"],
    ["settled", "settled"],
  ])("公共核对得到 %s 后审批行显示 %s", async (state, kind) => {
    verifyState = state;
    render(<OpenCodeNativePanel sessionId={sessionId} />);
    const anchor = await submitUnknown();
    fireEvent.click(within(anchor).getByTestId("native-action-verify"));
    await waitFor(() => expect(screen.queryByTestId("native-action-anchor")).toBeNull());
    await waitFor(() =>
      expect(screen.getByTestId("native-decision-receipt")).toHaveAttribute("data-kind", kind)
    );
    expect(screen.queryByTestId("native-decision-verify")).toBeNull();
    expect(screen.queryByTestId("native-decision")).toBeNull();
    expect(verifies).toEqual([posts[0].actionId]);
    expect(posts).toHaveLength(1);
  });

  it.each(["公共", "审批行"])(
    "%s 核对迟到的未确认结果不覆盖另一入口的确定结果",
    async (lateEntry) => {
      holdVerify = true;
      render(<OpenCodeNativePanel sessionId={sessionId} />);
      const anchor = await submitUnknown();
      const actionId = posts[0].actionId;
      const publicButton = within(anchor).getByTestId("native-action-verify");
      const decisionButton = screen.getByTestId("native-decision-verify");
      fireEvent.click(lateEntry === "公共" ? publicButton : decisionButton);
      fireEvent.click(lateEntry === "公共" ? decisionButton : publicButton);
      await waitFor(() => expect(verifies).toEqual([actionId, actionId]));
      await act(async () => {
        releaseVerifies[1](response({ actionId, state: "confirmed" }));
      });
      await waitFor(() =>
        expect(screen.getByTestId("native-decision-receipt")).toHaveAttribute(
          "data-kind",
          "submitted"
        )
      );
      await act(async () => {
        releaseVerifies[0](response({ actionId, state: "unconfirmed" }));
      });
      expect(screen.getByTestId("native-decision-receipt")).toHaveAttribute(
        "data-kind",
        "submitted"
      );
      expect(readActionAnchors(sessionId)).toHaveLength(0);
      expect(posts).toHaveLength(1);
    }
  );

  it.each(["公共", "审批行"])(
    "%s 核对迟到的未确认结果不覆盖同一审批的新动作",
    async (lateEntry) => {
      holdVerify = true;
      render(<OpenCodeNativePanel sessionId={sessionId} />);
      const anchor = await submitUnknown();
      const oldActionId = posts[0].actionId;
      const publicButton = within(anchor).getByTestId("native-action-verify");
      const decisionButton = screen.getByTestId("native-decision-verify");
      fireEvent.click(lateEntry === "公共" ? publicButton : decisionButton);
      fireEvent.click(lateEntry === "公共" ? decisionButton : publicButton);
      await waitFor(() => expect(verifies).toEqual([oldActionId, oldActionId]));
      await act(async () => {
        releaseVerifies[1](
          response({ actionId: oldActionId, state: "not_sent", reasonCode: "binding_stale" })
        );
      });
      await waitFor(() => expect(screen.getAllByTestId("native-decision")[0]).toBeEnabled());
      fireEvent.click(screen.getAllByTestId("native-decision")[0]);
      await waitFor(() => expect(posts).toHaveLength(2));
      expect(posts[1].actionId).not.toBe(oldActionId);
      await waitFor(() =>
        expect(screen.getByTestId("native-decision-receipt")).toHaveAttribute(
          "data-kind",
          "unknown"
        )
      );
      await act(async () => {
        releaseVerifies[0](response({ actionId: oldActionId, state: "unconfirmed" }));
      });
      expect(screen.getByTestId("native-decision-receipt")).toHaveAttribute("data-kind", "unknown");
      expect(readActionAnchors(sessionId).map((item) => item.actionId)).toEqual([
        posts[1].actionId,
      ]);
      fireEvent.click(screen.getByTestId("native-decision-verify"));
      await waitFor(() => expect(verifies).toHaveLength(3));
      expect(verifies[2]).toBe(posts[1].actionId);
    }
  );

  it("审批行核对得到 settled 后显示定案", async () => {
    verifyState = "settled";
    render(<OpenCodeNativePanel sessionId={sessionId} />);
    await submitUnknown();
    fireEvent.click(screen.getByTestId("native-decision-verify"));
    await waitFor(() =>
      expect(screen.getByTestId("native-decision-receipt")).toHaveAttribute("data-kind", "settled")
    );
    expect(readActionAnchors(sessionId)).toHaveLength(0);
    expect(verifies).toEqual([posts[0].actionId]);
  });

  it("切换绑定后旧公共核对回执不改变新目标的同 ID 审批", async () => {
    holdVerify = true;
    render(<OpenCodeNativePanel sessionId={sessionId} />);
    const anchor = await submitUnknown();
    fireEvent.click(within(anchor).getByTestId("native-action-verify"));
    await waitFor(() => expect(verifies).toHaveLength(1));
    await waitFor(() => expect(MockEventSource.instances.length).toBeGreaterThan(0));
    stale = true;
    act(() => MockEventSource.latest().emit("revision", "2"));
    await waitFor(() => expect(MockEventSource.latest().url).toContain("binding-b"));
    expect(screen.queryByTestId("native-reselect")).toBeNull();
    await waitFor(() => expect(screen.getAllByTestId("native-decision")[0]).toBeEnabled());
    fireEvent.click(screen.getAllByTestId("native-decision")[0]);
    await waitFor(() => expect(posts).toHaveLength(2));
    expect(posts[1].bindingToken).toBe("binding-b");
    await waitFor(() =>
      expect(screen.getByTestId("native-decision-receipt")).toHaveAttribute("data-kind", "unknown")
    );
    expect(releaseVerifies).toHaveLength(1);
    await act(async () => {
      releaseVerifies[0](response({ actionId: posts[0].actionId, state: "confirmed" }));
    });
    expect(screen.getByTestId("native-decision-receipt")).toHaveAttribute("data-kind", "unknown");
    expect(readActionAnchors(sessionId).some((item) => item.actionId === posts[1].actionId)).toBe(
      true
    );
    expect(posts).toHaveLength(2);
  });
});
