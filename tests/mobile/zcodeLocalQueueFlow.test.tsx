import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeAll, afterAll, expect, it, vi } from "vitest";
import ZcodeControlPanel from "@/mobile/ZcodeControlPanel";
import {
  ZCODE_ACTIONS_PATH,
  ZCODE_CONNECTION_PATH,
  ZCODE_LOCAL_QUEUE_PATH,
  ZCODE_SESSION_PATH,
  type CapabilityView,
  type ConnectionView,
  type EditResult,
  type LocalQueueView,
  type SessionView,
} from "@/lib/api/zcodeControl";
import { pinZhNavigator } from "./pinZhNavigator";

const SID = "session_0000000000000000000000000000000a";
const GEN = 3;
let unpinZh: () => void;
beforeAll(async () => {
  unpinZh = await pinZhNavigator();
});
afterAll(() => unpinZh());
afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

const flag = (enabled: boolean) => ({ enabled, reasonCode: enabled ? null : "needs_evidence" });
const caps = (): CapabilityView => ({
  send: flag(true),
  singleSelect: flag(true),
  multiSelect: flag(true),
  freeText: flag(true),
  interrupt: flag(true),
  approvalReply: flag(true),
  model: flag(true),
  thoughtLevel: flag(true),
  queueEdit: flag(false),
  queueNow: flag(false),
  queueResume: flag(false),
});
const connection = (): ConnectionView => ({
  state: "paired",
  generation: GEN,
  reasonCode: null,
  deviceLabel: "Synthetic",
});
const session = (): SessionView => ({
  sessionId: SID,
  generation: GEN,
  revision: 1,
  ready: true,
  pending: [],
  control: { activeWorks: [{ kind: "primaryTurn", foregroundExecutionId: "E1" }] },
  selection: null,
  capabilities: caps(),
  queue: {
    known: true,
    paused: false,
    items: [],
    availability: { known: true, queueEditAllowed: false, sendQueuedNowAllowed: false },
  },
});
const emptyQueue = (): LocalQueueView => ({
  sessionId: SID,
  version: 1,
  autoDrain: true,
  pauseReason: null,
  items: [],
  recoverableEdits: [],
});
const queued = (): LocalQueueView => ({
  sessionId: SID,
  version: 2,
  autoDrain: true,
  pauseReason: null,
  items: [
    {
      itemId: "item-1",
      itemVersion: 1,
      state: "queued",
      position: 1,
      textPreview: "忙时消息",
      hasAttachments: false,
    },
  ],
  recoverableEdits: [],
});

function json(body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status: 200,
    headers: { "content-type": "application/json" },
  });
}

it("enqueue_during_running_is_visible_before_host_finishes", async () => {
  let queue = emptyQueue();
  const calls: { url: string; body?: string }[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      calls.push({ url, body: init?.body === undefined ? undefined : String(init.body) });
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(session());
      if (url.startsWith(`${ZCODE_LOCAL_QUEUE_PATH}?`)) return json(queue);
      if (url === `${ZCODE_LOCAL_QUEUE_PATH}/enqueue`) {
        queue = queued();
        return json({ itemId: "item-1", itemVersion: 1, queueVersion: 2, position: 1 });
      }
      if (
        url === `${ZCODE_LOCAL_QUEUE_PATH}/resume` ||
        url === `${ZCODE_LOCAL_QUEUE_PATH}/retract`
      ) {
        return json(queue);
      }
      if (url === ZCODE_ACTIONS_PATH)
        throw new Error("busy send must not use the direct action path");
      throw new Error(`unexpected fetch: ${url}`);
    })
  );

  render(<ZcodeControlPanel sessionId={SID} />);
  await act(async () => {
    await new Promise((resolve) => setTimeout(resolve, 50));
  });
  await waitFor(() => expect(screen.getByTestId("zcode-send")).toBeEnabled());
  fireEvent.change(screen.getByTestId("zcode-draft"), { target: { value: "忙时消息" } });
  await act(async () => {
    screen.getByTestId("zcode-send").click();
  });

  await waitFor(() => expect(screen.getByTestId("zcode-local-item")).toBeVisible());
  const enqueue = calls.find((call) => call.url === `${ZCODE_LOCAL_QUEUE_PATH}/enqueue`);
  expect(enqueue).toBeDefined();
  expect(JSON.parse(enqueue!.body!)).toMatchObject({
    sessionId: SID,
    draft: { text: "忙时消息", attachments: [] },
  });
  expect(calls.filter((call) => call.url === ZCODE_ACTIONS_PATH)).toHaveLength(0);
});

const queuedItem = (overrides: Partial<LocalQueueView["items"][number]> = {}) => ({
  itemId: "item-1",
  itemVersion: 1,
  state: "queued" as const,
  position: 1,
  textPreview: "待发原文",
  hasAttachments: false,
  ...overrides,
});

function queueWith(overrides: Partial<LocalQueueView> = {}): LocalQueueView {
  return {
    sessionId: SID,
    version: 2,
    autoDrain: true,
    pauseReason: null,
    items: [queuedItem()],
    recoverableEdits: [],
    ...overrides,
  };
}

function installQueueServer(options: {
  initial?: LocalQueueView;
  session?: SessionView;
  onEdit?: () => EditResult;
}) {
  let queue = options.initial ?? queueWith();
  const calls: { url: string; body?: string }[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      calls.push({ url, body: init?.body === undefined ? undefined : String(init.body) });
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(options.session ?? session());
      if (url.startsWith(`${ZCODE_LOCAL_QUEUE_PATH}?`)) return json(queue);
      if (url === `${ZCODE_LOCAL_QUEUE_PATH}/prioritize`) {
        return json({ requestId: "priority", state: "completed", receipt: null });
      }
      if (url === `${ZCODE_LOCAL_QUEUE_PATH}/edit`) {
        return json(
          options.onEdit?.() ?? {
            requestId: "edit-1",
            itemId: "item-1",
            draft: { text: "待发原文", attachments: [] },
            expiresAtMs: Date.now() + 60_000,
          }
        );
      }
      if (url === `${ZCODE_LOCAL_QUEUE_PATH}/retract`) {
        queue = queueWith({ items: [], version: 3 });
        return json(queue);
      }
      if (url === `${ZCODE_LOCAL_QUEUE_PATH}/resume`) {
        queue = queueWith({ autoDrain: true, pauseReason: null, version: 4 });
        return json(queue);
      }
      if (url === `${ZCODE_LOCAL_QUEUE_PATH}/edit/finish`) return json({ ok: true });
      throw new Error(`unexpected fetch: ${url}`);
    })
  );
  return { calls, getQueue: () => queue };
}

it("local_queue_prioritize_and_retract_only_use_local_api", async () => {
  const { calls } = installQueueServer({});
  render(<ZcodeControlPanel sessionId={SID} />);
  await waitFor(() => expect(screen.getByTestId("zcode-local-item")).toBeVisible());
  const row = screen.getByTestId("zcode-local-item");
  fireEvent.click(within(row).getByRole("button", { name: "插队" }));
  expect(screen.getByTestId("zcode-priority-dialog")).toBeVisible();
  fireEvent.click(screen.getByTestId("zcode-priority-confirm"));
  await waitFor(() =>
    expect(
      calls.filter((call) => call.url === `${ZCODE_LOCAL_QUEUE_PATH}/prioritize`)
    ).toHaveLength(1)
  );
  const priority = calls.find((call) => call.url === `${ZCODE_LOCAL_QUEUE_PATH}/prioritize`);
  expect(JSON.parse(priority!.body!)).toMatchObject({
    itemId: "item-1",
    expectedVersion: 1,
    confirmStop: true,
    expectedExecutionId: "E1",
  });
  expect(calls.filter((call) => call.url === ZCODE_ACTIONS_PATH)).toHaveLength(0);

  fireEvent.click(
    within(screen.getByTestId("zcode-local-item")).getByRole("button", { name: "撤回" })
  );
  await waitFor(() =>
    expect(calls.filter((call) => call.url === `${ZCODE_LOCAL_QUEUE_PATH}/retract`)).toHaveLength(1)
  );
  expect(calls.filter((call) => call.url === ZCODE_ACTIONS_PATH)).toHaveLength(0);
});

it("edit_snapshot_never_overwrites_a_continued_draft_without_choice", async () => {
  const { calls } = installQueueServer({});
  render(<ZcodeControlPanel sessionId={SID} />);
  await waitFor(() => expect(screen.getByTestId("zcode-local-item")).toBeVisible());
  const row = screen.getByTestId("zcode-local-item");
  const draft = screen.getByTestId("zcode-draft") as HTMLTextAreaElement;
  fireEvent.change(draft, { target: { value: "续写内容" } });
  fireEvent.click(within(row).getByRole("button", { name: "再编辑" }));
  await waitFor(() => expect(screen.getByTestId("zcode-edit-conflict-dialog")).toBeVisible());
  expect(draft).toHaveValue("续写内容");
  fireEvent.click(screen.getByTestId("zcode-edit-conflict-cancel"));
  await waitFor(() =>
    expect(
      calls.filter((call) => call.url === `${ZCODE_LOCAL_QUEUE_PATH}/edit/finish`)
    ).toHaveLength(1)
  );
  expect(draft).toHaveValue("续写内容");
});

it("resume_uses_current_local_queue_version", async () => {
  const { calls } = installQueueServer({
    initial: queueWith({ autoDrain: false, pauseReason: "manual_stop", version: 7 }),
  });
  render(<ZcodeControlPanel sessionId={SID} />);
  await waitFor(() => expect(screen.getByTestId("zcode-local-queue-resume")).toBeVisible());
  fireEvent.click(screen.getByTestId("zcode-local-queue-resume"));
  await waitFor(() =>
    expect(calls.filter((call) => call.url === `${ZCODE_LOCAL_QUEUE_PATH}/resume`)).toHaveLength(1)
  );
  const resume = calls.find((call) => call.url === `${ZCODE_LOCAL_QUEUE_PATH}/resume`);
  expect(JSON.parse(resume!.body!)).toMatchObject({
    sessionId: SID,
    expectedQueueVersion: 7,
  });
});

it("continued_draft_survives_enqueue_ack", async () => {
  let release!: (response: Response) => void;
  const gate = new Promise<Response>((resolve) => {
    release = resolve;
  });
  let queue = emptyQueue();
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, _init?: RequestInit) => {
      const url = String(input);
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(session());
      if (url.startsWith(`${ZCODE_LOCAL_QUEUE_PATH}?`)) return json(queue);
      if (url === `${ZCODE_LOCAL_QUEUE_PATH}/enqueue`) {
        await gate;
        queue = queued();
        return json({ itemId: "item-1", itemVersion: 1, queueVersion: 2, position: 1 });
      }
      throw new Error(`unexpected fetch: ${url}`);
    })
  );

  render(<ZcodeControlPanel sessionId={SID} />);
  await waitFor(() => expect(screen.getByTestId("zcode-send")).toBeEnabled());
  const draft = screen.getByTestId("zcode-draft") as HTMLTextAreaElement;
  fireEvent.change(draft, { target: { value: "第一次内容" } });
  fireEvent.click(screen.getByTestId("zcode-send"));
  await waitFor(() => expect(draft).toHaveValue("第一次内容"));
  fireEvent.change(draft, { target: { value: "回执期间续写" } });
  await act(async () => {
    release(json({ itemId: "item-1", itemVersion: 1, queueVersion: 2, position: 1 }));
    await Promise.resolve();
  });
  await waitFor(() => expect(screen.getByTestId("zcode-local-item")).toBeVisible());
  expect(draft).toHaveValue("回执期间续写");
});

it("unknown_enqueue_retry_reuses_request_id", async () => {
  let attempts = 0;
  let queue = emptyQueue();
  const bodies: string[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(session());
      if (url.startsWith(`${ZCODE_LOCAL_QUEUE_PATH}?`)) return json(queue);
      if (url === `${ZCODE_LOCAL_QUEUE_PATH}/enqueue`) {
        attempts += 1;
        bodies.push(String(init?.body));
        if (attempts === 1) throw new TypeError("synthetic network down");
        queue = queued();
        return json({ itemId: "item-1", itemVersion: 1, queueVersion: 2, position: 1 });
      }
      throw new Error(`unexpected fetch: ${url}`);
    })
  );

  render(<ZcodeControlPanel sessionId={SID} />);
  await waitFor(() => expect(screen.getByTestId("zcode-send")).toBeEnabled());
  const draft = screen.getByTestId("zcode-draft") as HTMLTextAreaElement;
  fireEvent.change(draft, { target: { value: "重试同一条" } });
  fireEvent.click(screen.getByTestId("zcode-send"));
  await waitFor(() => expect(attempts).toBe(1));
  expect(draft).toHaveValue("重试同一条");
  fireEvent.click(screen.getByTestId("zcode-send"));
  await waitFor(() => expect(attempts).toBe(2));
  const first = JSON.parse(bodies[0]) as { requestId: string };
  const second = JSON.parse(bodies[1]) as { requestId: string };
  expect(second.requestId).toBe(first.requestId);
});
