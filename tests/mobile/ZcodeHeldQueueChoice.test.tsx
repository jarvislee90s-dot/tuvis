// F3：`inputRouting.mode="choice"` + 非空队列时的旧队列处置选择。
//
// 锁四件事：
// 1. 发送前弹轻量选择（保留/清空/取消），默认焦点在「保留」；
// 2. 取消不发、零 POST，草稿保留；
// 3. 确认后**一次** send（新 actionId + 期望 ID 列表），保留/清空语义如实上行；
// 4. 清空是破坏性选择，绝不默认。
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import ZcodeControlPanel from "@/mobile/ZcodeControlPanel";
import {
  ZCODE_ACTIONS_PATH,
  ZCODE_CONNECTION_PATH,
  ZCODE_SESSION_PATH,
  type ActionReceipt,
  type CapabilityView,
  type ConnectionView,
  type ControlAction,
  type SessionView,
} from "@/lib/api/zcodeControl";

const SID = "session_0000000000000000000000000000000e";
const GEN = 7;

const flag = (enabled: boolean) => ({ enabled, reasonCode: enabled ? null : "needs_evidence" });

function capabilities(): CapabilityView {
  return {
    send: flag(true),
    singleSelect: flag(true),
    multiSelect: flag(true),
    freeText: flag(true),
    interrupt: flag(true),
    approvalReply: flag(true),
    model: flag(true),
    thoughtLevel: flag(true),
  };
}

function connection(): ConnectionView {
  return { state: "paired", generation: GEN, reasonCode: null, deviceLabel: "Synthetic Mac" };
}

function sessionView(overrides: Partial<SessionView> = {}): SessionView {
  return {
    sessionId: SID,
    generation: GEN,
    revision: 12,
    ready: true,
    pending: [],
    control: { activeWorks: [] },
    selection: null,
    capabilities: capabilities(),
    queue: {
      known: true,
      paused: false,
      items: [
        { inputId: "queue_a", text: "first queued", position: 0 },
        { inputId: "queue_b", text: "second queued", position: 1 },
      ],
    },
    inputRouting: { mode: "choice", reasonCode: "heldQueueDispositionRequired" },
    ...overrides,
  };
}

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

interface PostBody {
  actionId: string;
  action: ControlAction;
}

let posts: PostBody[];

function installFetch(view: SessionView = sessionView()) {
  posts = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(view);
      if (url.includes("/official-control")) return json({ configState: "missing", url: null });
      if (url === ZCODE_ACTIONS_PATH && init?.method === "POST") {
        const body = JSON.parse(String(init.body)) as PostBody;
        posts.push(body);
        return json({
          actionId: body.actionId,
          sessionId: SID,
          generation: GEN,
          state: "accepted",
          reasonCode: null,
        } satisfies ActionReceipt);
      }
      throw new Error(`unexpected fetch: ${url}`);
    })
  );
}

async function typeDraft(text: string) {
  const textarea = await screen.findByTestId("zcode-draft");
  fireEvent.change(textarea, { target: { value: text } });
  return textarea as HTMLTextAreaElement;
}

const UUID_V4 = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;

beforeEach(() => {
  vi.stubGlobal("EventSource", undefined);
  window.localStorage.clear();
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

describe("ZcodeControlPanel：choice 模式旧队列处置", () => {
  it("choice_prompts_before_send_and_defaults_to_keep", async () => {
    installFetch();
    render(<ZcodeControlPanel sessionId={SID} />);
    await typeDraft("新的正文");

    await waitFor(() => expect(screen.getByTestId("zcode-send")).not.toBeDisabled());
    fireEvent.click(screen.getByTestId("zcode-send"));

    // 先弹选择，绝不直接发；默认焦点在「保留」。
    const dialog = await screen.findByTestId("zcode-held-queue-dialog");
    expect(dialog).toBeInTheDocument();
    expect(posts).toHaveLength(0);
    expect(document.activeElement).toBe(screen.getByTestId("zcode-held-queue-keep"));

    fireEvent.click(screen.getByTestId("zcode-held-queue-keep"));
    await waitFor(() => expect(posts).toHaveLength(1));
    expect(posts[0].actionId).toMatch(UUID_V4);
    expect(posts[0].action).toMatchObject({
      kind: "send",
      sessionId: SID,
      generation: GEN,
      heldQueue: {
        disposition: "keepQueueAndSend",
        expectedQueueItemIds: ["queue_a", "queue_b"],
      },
    });
    // 只发一次（没有第二个 sendText 试错）。
    expect(posts).toHaveLength(1);
  });

  it("choice_clear_sends_clear_disposition_exactly_once", async () => {
    installFetch();
    render(<ZcodeControlPanel sessionId={SID} />);
    await typeDraft("新的正文");
    await waitFor(() => expect(screen.getByTestId("zcode-send")).not.toBeDisabled());
    fireEvent.click(screen.getByTestId("zcode-send"));

    fireEvent.click(await screen.findByTestId("zcode-held-queue-clear"));
    await waitFor(() => expect(posts).toHaveLength(1));
    expect(posts[0].action).toMatchObject({
      kind: "send",
      heldQueue: {
        disposition: "clearQueueAndSend",
        expectedQueueItemIds: ["queue_a", "queue_b"],
      },
    });
    expect(posts).toHaveLength(1);
  });

  it("choice_cancel_sends_nothing_and_keeps_draft", async () => {
    installFetch();
    render(<ZcodeControlPanel sessionId={SID} />);
    const textarea = await typeDraft("保留中的草稿");
    await waitFor(() => expect(screen.getByTestId("zcode-send")).not.toBeDisabled());
    fireEvent.click(screen.getByTestId("zcode-send"));

    fireEvent.click(await screen.findByTestId("zcode-held-queue-cancel"));
    await act(async () => {
      await Promise.resolve();
    });

    expect(screen.queryByTestId("zcode-held-queue-dialog")).toBeNull();
    expect(posts).toHaveLength(0);
    expect(textarea.value).toBe("保留中的草稿");
  });

  it("choice_with_empty_queue_sends_without_prompt_or_held_queue", async () => {
    installFetch(
      sessionView({
        queue: { known: true, paused: false, items: [] },
        inputRouting: { mode: "choice", reasonCode: null },
      })
    );
    render(<ZcodeControlPanel sessionId={SID} />);
    await typeDraft("空队列直接发");
    await waitFor(() => expect(screen.getByTestId("zcode-send")).not.toBeDisabled());
    fireEvent.click(screen.getByTestId("zcode-send"));

    async function noDialogYet() {
      expect(screen.queryByTestId("zcode-held-queue-dialog")).toBeNull();
    }
    await noDialogYet();
    await waitFor(() => expect(posts).toHaveLength(1));
    expect(posts[0].action).toMatchObject({ kind: "send" });
    expect((posts[0].action as { heldQueue?: unknown }).heldQueue).toBeUndefined();
  });
});
