import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterAll, afterEach, beforeAll, describe, expect, it, vi } from "vitest";
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
  revision: 4,
  ready: true,
  pending: [],
  control: { activeWorks: [] },
  selection: {
    provider: "a",
    model: "a-model",
    thought: "low",
    models: [{ modelId: "a-model", thoughtLevels: ["low", "high"] }],
    providers: [
      {
        providerId: "a",
        label: "Provider A",
        available: true,
        models: [{ modelId: "a-model", thoughtLevels: ["low", "high"] }],
      },
      {
        providerId: "b",
        label: "Provider B",
        available: true,
        models: [{ modelId: "b-model", thoughtLevels: ["low"] }],
      },
    ],
  },
  capabilities: caps(),
});
const json = (body: unknown) =>
  new Response(JSON.stringify(body), {
    status: 200,
    headers: { "content-type": "application/json" },
  });

function install() {
  const posts: { actionId: string; action: ControlAction }[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(session());
      if (url.startsWith("/m/api/v1/zcode/local-queue?"))
        return json({
          sessionId: SID,
          version: 0,
          autoDrain: true,
          pauseReason: null,
          items: [],
          recoverableEdits: [],
        });
      if (url === ZCODE_ACTIONS_PATH && init?.method === "POST") {
        const body = JSON.parse(String(init.body)) as { actionId: string; action: ControlAction };
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
  return posts;
}

describe("ZcodeProviderSelection", () => {
  it("provider_change_alone_emits_zero_post_then_model_sends_one_full_triple", async () => {
    const posts = install();
    render(<ZcodeControlPanel sessionId={SID} />);
    await waitFor(() => expect(screen.getByTestId("zcode-provider-select")).toBeVisible());
    fireEvent.change(screen.getByTestId("zcode-provider-select"), { target: { value: "b" } });
    await waitFor(() => expect(posts).toHaveLength(0));
    const model = screen.getByTestId("zcode-model-select") as HTMLSelectElement;
    expect(Array.from(model.options).map((option) => option.value)).toEqual(["b-model"]);
    fireEvent.change(model, { target: { value: "b-model" } });
    await waitFor(() => expect(posts).toHaveLength(1));
    expect(posts[0].action).toMatchObject({
      kind: "selection",
      provider: "b",
      model: "b-model",
      thought: "low",
    });
  });
});
