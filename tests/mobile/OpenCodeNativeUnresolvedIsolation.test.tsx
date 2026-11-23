import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import OpenCodeNativePanel from "@/mobile/OpenCodeNativePanel";
import {
  addActionAnchor,
  hasUnresolvedAction,
  rememberActionBinding,
  removeActionAnchor,
} from "@/mobile/opencodeActionAnchors";
import { MockEventSource } from "./eventSourceMock";

const session = "isolation-session";
const binding = "binding-1";
const sendKey = `tuvis.opencodeNative.action.${session}`;
const response = (body: unknown) => new Response(JSON.stringify(body), { status: 200 });
let posts: Record<string, unknown>[];
let releases: Array<(value: Response) => void>;
let actionMode: "unknown" | "pending";
let pendingItems: unknown[];

function installServer(runtimeState: "ready" | "running" = "ready") {
  vi.stubGlobal("EventSource", MockEventSource as unknown as typeof EventSource);
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url.endsWith("/opencode/actions") && init?.method === "POST") {
        const body = JSON.parse(String(init.body)) as Record<string, unknown>;
        posts.push(body);
        if (actionMode === "pending")
          return new Promise<Response>((resolve) => releases.push(resolve));
        return response({ actionId: body.actionId, state: "unconfirmed" });
      }
      if (url.includes("/opencode/view"))
        return response({
          sessionId: session,
          bindingToken: binding,
          revision: 1,
          complete: true,
          capabilities: {
            send: { allowed: true, evidence: "prompt", reasonCode: null },
            set_agent: { allowed: true, evidence: "agent", reasonCode: null },
            stop_session: { allowed: true, evidence: "interrupt", reasonCode: null },
            answer_group: { allowed: true, evidence: "answer", reasonCode: null },
            permission_once: { allowed: true, evidence: "once", reasonCode: null },
            permission_reject: { allowed: true, evidence: "reject", reasonCode: null },
          },
          messages: [],
          pending: pendingItems,
          inbox: [],
          runtimeState,
          files: {},
          route: "opencode_native",
          routeReasonCode: null,
        });
      if (url.includes("/opencode/agents"))
        return response({
          current: "build",
          options: [
            { id: "build", name: "Build" },
            { id: "plan", name: "Plan" },
            { id: "other", name: "Other" },
          ],
        });
      if (url.includes("/opencode/outbox"))
        return response({ sessionId: session, revision: 0, paused: false, items: [] });
      if (url.includes("/opencode/actions/"))
        return response({ actionId: "unresolved", state: "unconfirmed" });
      throw new Error(`Unexpected request: ${url}`);
    })
  );
}

beforeEach(() => {
  sessionStorage.clear();
  MockEventSource.reset();
  posts = [];
  releases = [];
  actionMode = "unknown";
  pendingItems = [];
});
afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  const ids = [
    ...posts.map((post) => String(post.actionId)),
    ...["answer", "decide", "stop"].flatMap((kind) => [`write-${kind}`, `read-${kind}`]),
    ...Array.from({ length: 21 }, (_, i) => `overflow-${i}`),
  ];
  for (const id of ids) removeActionAnchor(session, id);
  sessionStorage.clear();
  vi.unstubAllGlobals();
});

describe("同页未决动作隔离", () => {
  it.each(["answer", "decide", "stop"] as const)("storage 写入失败后 %s 仍保持未决", (kind) => {
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new DOMException("quota", "QuotaExceededError");
    });
    const id = `write-${kind}`;
    rememberActionBinding(id, binding);
    addActionAnchor(session, { actionId: id, kind, targetId: "target" });
    expect(hasUnresolvedAction(session, kind, "target", binding)).toBe(true);
  });

  it.each(["answer", "decide", "stop"] as const)("storage 读取失败后 %s 仍保持未决", (kind) => {
    const id = `read-${kind}`;
    rememberActionBinding(id, binding);
    addActionAnchor(session, { actionId: id, kind, targetId: "target" });
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new DOMException("denied", "SecurityError");
    });
    expect(hasUnresolvedAction(session, kind, "target", binding)).toBe(true);
  });

  it.each([
    ["answer", "write"],
    ["decide", "write"],
    ["stop", "write"],
    ["answer", "read"],
    ["decide", "read"],
    ["stop", "read"],
  ] as const)("storage %s 在 %s 失败时重挂面板不能重发", async (kind, failure) => {
    pendingItems =
      kind === "answer"
        ? [
            {
              kind: "form",
              id: "form-ui",
              title: "表单",
              context_token: "ctx",
              state: "unknown",
              fields: [
                {
                  key: "q0",
                  title: "选择",
                  required: true,
                  kind: {
                    kind: "text",
                    custom: false,
                    options: [{ value: "yes", label: "同意" }],
                  },
                },
              ],
            },
          ]
        : kind === "decide"
          ? [
              {
                kind: "permission",
                id: "permission-ui",
                title: "授权",
                decisions: ["once", "reject"],
                context_token: "ctx",
              },
            ]
          : [];
    installServer(kind === "stop" ? "running" : "ready");
    if (failure === "write")
      vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
        throw new DOMException("quota", "QuotaExceededError");
      });
    const first = render(<OpenCodeNativePanel sessionId={session} />);
    const attempt = async () => {
      if (kind === "answer") {
        fireEvent.click((await screen.findAllByTestId("opencode-q-option"))[0]);
        fireEvent.click(screen.getByTestId("opencode-q-submit"));
      } else if (kind === "decide") {
        fireEvent.click((await screen.findAllByTestId("native-decision"))[0]);
      } else {
        fireEvent.click(await screen.findByTestId("native-stop"));
      }
    };
    await attempt();
    await waitFor(() => expect(posts).toHaveLength(1));
    const resultId =
      kind === "answer"
        ? "opencode-q-receipt"
        : kind === "decide"
          ? "native-decision-receipt"
          : "native-stop-result";
    await screen.findByTestId(resultId);
    if (failure === "read")
      vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
        throw new DOMException("denied", "SecurityError");
      });
    first.unmount();
    render(<OpenCodeNativePanel sessionId={session} />);
    if (kind === "decide") {
      const row = await screen.findByTestId("native-action-anchor");
      expect(row.dataset.actionId).toBe(String(posts[0].actionId));
      expect(row.querySelector('[data-testid="native-action-verify"]')).not.toBeNull();
      expect(screen.queryByTestId("native-decision")).toBeNull();
      expect(hasUnresolvedAction(session, "decide", "permission-ui", binding)).toBe(true);
    } else if (kind === "stop") {
      await screen.findByTestId("native-messages-toggle");
      expect(hasUnresolvedAction(session, "stop", session, binding)).toBe(true);
      const stop = screen.queryByTestId("native-stop");
      if (stop) fireEvent.click(stop);
    } else {
      await attempt();
    }
    await act(async () => {
      await Promise.resolve();
    });
    expect(posts).toHaveLength(1);
  });

  it("超过 20 个未决锚后最早动作仍禁止重发", () => {
    for (let i = 0; i < 21; i++) {
      const id = `overflow-${i}`;
      rememberActionBinding(id, binding);
      addActionAnchor(session, { actionId: id, kind: "answer", targetId: `form-${i}` });
    }
    expect(hasUnresolvedAction(session, "answer", "form-0", binding)).toBe(true);
  });

  it("set_agent 结果未知后禁止切换到另一模式", async () => {
    installServer();
    render(<OpenCodeNativePanel sessionId={session} />);
    const select = await screen.findByTestId("native-mode-select");
    fireEvent.change(select, { target: { value: "plan" } });
    fireEvent.click(screen.getByTestId("native-mode-switch"));
    await waitFor(() => expect(posts).toHaveLength(1));
    await screen.findByTestId("native-mode-error");
    fireEvent.change(select, { target: { value: "other" } });
    fireEvent.click(screen.getByTestId("native-mode-switch"));
    await act(async () => {
      await Promise.resolve();
    });
    expect(posts).toHaveLength(1);
  });

  it("set_agent 同步连点只派发一次", async () => {
    installServer();
    actionMode = "pending";
    render(<OpenCodeNativePanel sessionId={session} />);
    const select = await screen.findByTestId("native-mode-select");
    fireEvent.change(select, { target: { value: "plan" } });
    const button = screen.getByTestId("native-mode-switch");
    act(() => {
      button.click();
      button.click();
    });
    expect(posts).toHaveLength(1);
  });

  it("旧面板卸载后的发送回执不能删除新动作锚", async () => {
    installServer();
    actionMode = "pending";
    const oldPanel = render(<OpenCodeNativePanel sessionId={session} />);
    const input = await screen.findByTestId("native-input");
    fireEvent.change(input, { target: { value: "旧消息" } });
    fireEvent.click(screen.getByTestId("native-send"));
    await waitFor(() => expect(posts).toHaveLength(1));
    oldPanel.unmount();
    // 模拟旧锚已被另一个界面显式放弃，随后为同会话创建新动作。
    sessionStorage.removeItem(sendKey);
    render(<OpenCodeNativePanel sessionId={session} />);
    const newInput = await screen.findByTestId("native-input");
    fireEvent.change(newInput, { target: { value: "新消息" } });
    fireEvent.click(screen.getByTestId("native-send"));
    await waitFor(() => expect(posts).toHaveLength(2));
    const newId = String(posts[1].actionId);
    expect(sessionStorage.getItem(sendKey)).toBe(newId);
    await act(async () => {
      releases[0](response({ actionId: posts[0].actionId, state: "confirmed" }));
    });
    expect(sessionStorage.getItem(sendKey)).toBe(newId);
  });
});
