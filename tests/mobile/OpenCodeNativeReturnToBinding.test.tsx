import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import OpenCodeNativePanel from "@/mobile/OpenCodeNativePanel";
import { addActionAnchor, readActionAnchors } from "@/mobile/opencodeActionAnchors";
import type { PublicNativeView } from "@/mobile/opencodeNativeApi";
import { MockEventSource } from "./eventSourceMock";

type Target = "A" | "B";
type Kind = "answer" | "permission" | "stop";
const sessionId = "sess-return";
const form = {
  kind: "form" as const,
  id: "form-same",
  title: "Question",
  context_token: "ctx-same",
  state: "unknown",
  fields: [
    {
      key: "q",
      title: "Choose",
      required: true,
      kind: { kind: "text" as const, custom: false, options: [{ value: "yes", label: "Yes" }] },
    },
  ],
};
const permission = {
  kind: "permission" as const,
  id: "perm-same",
  title: "Permission",
  decisions: ["once", "reject"],
  context_token: "ctx-same",
};
function view(
  target: Target,
  revision: number,
  runtimeState: PublicNativeView["runtimeState"]
): PublicNativeView {
  return {
    sessionId,
    bindingToken: `bind_${target}`,
    routeVersion: revision,
    revision,
    complete: true,
    capabilities: {
      send: { allowed: true, evidence: "contract", reasonCode: null },
      answer_group: { allowed: true, evidence: "contract", reasonCode: null },
      permission_once: { allowed: true, evidence: "contract", reasonCode: null },
      permission_reject: { allowed: true, evidence: "contract", reasonCode: null },
      stop_session: { allowed: true, evidence: "contract", reasonCode: null },
    },
    messages: [
      {
        seq: 0,
        role: "assistant",
        kind: "assistant",
        content: `Target ${target}`,
        collapsed: false,
      },
    ],
    pending: [form, permission],
    inbox: [],
    runtimeState,
    files: {},
    route: "opencode_native",
    routeReasonCode: null,
  };
}
const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status });

class Upload {
  upload = { onprogress: null };
  onload: ((event: ProgressEvent) => void) | null = null;
  status = 200;
  responseText = JSON.stringify({ path: "/tmp/attachment.txt", size: 1 });
  open() {}
  setRequestHeader() {}
  send() {
    this.onload?.(new ProgressEvent("load"));
  }
}

beforeEach(() => {
  sessionStorage.clear();
  MockEventSource.reset();
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

async function setup(action?: Kind) {
  let active: Target = "A";
  let scanned: Target = "A";
  let revision = 1;
  let switches = 0;
  const windowIdentity = { pid: 1, startedAtMs: 1, boot: null };
  let receipt: ReturnType<typeof committedReceipt> | undefined;
  const committedReceipt = (requestId: string) => ({
    sessionId,
    route: "opencode_native",
    requestId,
    state: "committed",
    current: true,
    bindingToken: `bind_${active}`,
    routeVersion: revision,
    window: windowIdentity,
  });
  const reads: Array<string | null> = [];
  const actions: Array<{ actionId: string; bindingToken: string; operation: { kind: string } }> =
    [];
  const releases: Array<(response: Response) => void> = [];
  vi.stubGlobal("EventSource", MockEventSource as unknown as typeof EventSource);
  vi.stubGlobal("XMLHttpRequest", Upload as unknown as typeof XMLHttpRequest);
  vi.stubGlobal(
    "fetch",
    vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      const binding = new Headers(init?.headers).get("X-OpenCode-Expected-Binding");
      if (url.includes("/opencode/route/preview?")) {
        return Promise.resolve(
          json({
            sessionId,
            candidate: {
              sessionId,
              directory: "/project",
              serviceId: "service",
              ...windowIdentity,
              bindingToken: `bind_${scanned}`,
            },
            previewToken: "preview-token",
            expiresAtMs: 9999999999999,
          })
        );
      }
      if (url.endsWith("/opencode/route/native")) {
        const request = JSON.parse(String(init?.body));
        expect(request.candidateToken).toBe("preview-token");
        expect(request.requestId).toEqual(expect.stringMatching(/\S/));
        active = scanned;
        receipt = committedReceipt(request.requestId);
        return Promise.resolve(json(receipt));
      }
      if (url.includes("/opencode/route/native/status?")) {
        const query = new URL(url, "http://localhost").searchParams;
        expect(init?.method ?? "GET").toBe("GET");
        expect(query.get("sessionId")).toBe(sessionId);
        expect(query.get("requestId")).toBe(receipt?.requestId);
        return Promise.resolve(json(receipt));
      }
      if (url.endsWith("/opencode/actions") && init?.method === "POST") {
        actions.push(JSON.parse(String(init.body)));
        return new Promise<Response>((resolve) => releases.push(resolve));
      }
      if (url.includes("/opencode/actions/"))
        return Promise.resolve(json({ error: "OC_ACTION_NOT_FOUND" }, 404));
      if (url.includes("/opencode/view")) {
        reads.push(binding);
        if (binding && (binding !== `bind_${active}` || active !== scanned))
          return Promise.resolve(json({ error: "OC_BINDING_STALE" }, 409));
        if (!binding) active = scanned;
        return Promise.resolve(
          json(view(active, revision, action === "stop" ? "running" : "ready"))
        );
      }
      if (url.includes("/opencode/agents"))
        return Promise.resolve(json({ current: null, options: [] }));
      if (url.includes("/opencode/forms/"))
        return Promise.resolve(json({ error: "not_found" }, 404));
      if (url.includes("/opencode/outbox"))
        return Promise.resolve(json({ sessionId, revision: 0, paused: false, items: [] }));
      if (url.includes("/channel")) return Promise.resolve(json({ via: "lan", limited: false }));
      throw new Error(`unexpected fetch: ${url}`);
    })
  );
  vi.spyOn(window, "confirm").mockReturnValue(true);
  render(<OpenCodeNativePanel sessionId={sessionId} />);
  await screen.findByTestId("opencode-question-card");
  await waitFor(() => expect(MockEventSource.instances).toHaveLength(1));
  fireEvent.click(screen.getByTestId("native-messages-toggle"));
  expect(screen.getByText("Target A")).toBeVisible();
  async function switchTo(target: Target) {
    scanned = target;
    revision++;
    const before = reads.length;
    act(() => MockEventSource.latest().emit("revision", String(revision)));
    if (switches++ === 0) {
      await screen.findByText(`Target ${target}`);
      expect(reads.slice(before)).toEqual([target === "B" ? "bind_A" : "bind_B", null]);
      expect(screen.queryByTestId("native-reselect")).toBeNull();
    } else {
      fireEvent.click(await screen.findByTestId("native-reselect"));
      await screen.findByText(`Target ${target}`);
    }
    await waitFor(() => expect(MockEventSource.latest().url).toContain(`bind_${target}`));
  }
  return { actions, releases, switchTo };
}

function tryAction(kind: Kind) {
  if (kind === "answer") {
    const option = screen.queryByTestId("opencode-q-option");
    if (option && option.dataset.selected !== "true") fireEvent.click(option);
    const submit = screen.queryByTestId("opencode-q-submit");
    if (submit) fireEvent.click(submit);
  } else if (kind === "permission") {
    const button = screen.queryAllByTestId("native-decision")[0];
    if (button) fireEvent.click(button);
  } else {
    const button = screen.queryByTestId("native-stop");
    if (button) fireEvent.click(button);
  }
}

describe("returning to an OpenCode binding", () => {
  it.each(["answer", "permission", "stop"] as const)(
    "does not resubmit unconfirmed %s after A → B → A",
    async (kind) => {
      const state = await setup(kind);
      if (kind === "stop") expect(screen.getByTestId("native-stop")).toBeEnabled();
      tryAction(kind);
      await waitFor(() => expect(state.actions).toHaveLength(1));
      const first = state.actions[0];
      expect(first.bindingToken).toBe("bind_A");
      await act(async () => {
        state.releases[0](json({ actionId: first.actionId, state: "unconfirmed" }));
      });
      await waitFor(() =>
        expect(readActionAnchors(sessionId).map((a) => a.actionId)).toContain(first.actionId)
      );
      await state.switchTo("B");
      await state.switchTo("A");
      tryAction(kind);
      await act(async () => {
        await Promise.resolve();
      });
      expect(state.actions).toHaveLength(1);
    }
  );

  it.each(["answer", "permission", "stop"] as const)(
    "does not resubmit %s when a restored anchor has unknown binding",
    async (kind) => {
      addActionAnchor(sessionId, {
        actionId: "restored-unknown",
        kind: kind === "permission" ? "decide" : kind,
        targetId: kind === "answer" ? form.id : kind === "permission" ? permission.id : sessionId,
      });
      const state = await setup(kind);
      expect(readActionAnchors(sessionId).map((a) => a.actionId)).toContain("restored-unknown");
      tryAction(kind);
      await act(async () => {
        await Promise.resolve();
      });
      expect(state.actions).toHaveLength(0);
    }
  );

  it.each(["queued", "confirmed"] as const)(
    "does not consume the current draft attachment when old A send returns %s after A → B → A",
    async (outcome) => {
      const state = await setup();
      fireEvent.change(screen.getByTestId("native-file-input"), {
        target: { files: [new File(["a"], "old.txt")] },
      });
      await waitFor(() =>
        expect(
          screen.getByText("old.txt").closest('[data-testid="native-attachment"]')
        ).toHaveAttribute("data-status", "ready")
      );
      fireEvent.change(screen.getByTestId("native-input"), { target: { value: "old draft" } });
      fireEvent.click(screen.getByTestId("native-send"));
      await waitFor(() => expect(state.actions).toHaveLength(1));
      await state.switchTo("B");
      await state.switchTo("A");
      fireEvent.change(screen.getByTestId("native-input"), { target: { value: "current draft" } });
      expect(screen.getByText("old.txt")).toBeInTheDocument();
      await act(async () => {
        state.releases[0](json({ actionId: state.actions[0].actionId, state: outcome }));
      });
      expect(screen.getByTestId("native-input")).toHaveValue("current draft");
      expect(screen.getByText("old.txt")).toBeInTheDocument();
      expect(state.actions).toHaveLength(1);
    }
  );
});
