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
  mode: flag(true),
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
    model: "m",
    thought: "low",
    models: [{ modelId: "m", thoughtLevels: ["low"] }],
    mode: { rawMode: "build", planEnabled: false, effectiveMode: "build" },
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

describe("ZcodeModeControl", () => {
  it("full_access_cancel_sends_nothing_and_confirm_sends_one_mode_action", async () => {
    const posts = install();
    render(<ZcodeControlPanel sessionId={SID} />);
    await waitFor(() => expect(screen.getByTestId("zcode-mode-select")).toBeVisible());
    const modeSelect = screen.getByTestId("zcode-mode-select");
    const apply = screen.getByTestId("zcode-mode-apply") as HTMLButtonElement;
    fireEvent.change(modeSelect, { target: { value: "yolo" } });
    await waitFor(() => expect(apply).not.toBeDisabled());
    fireEvent.click(apply);
    expect(await screen.findByTestId("zcode-mode-confirm-dialog")).toBeVisible();
    fireEvent.click(screen.getByTestId("zcode-mode-cancel"));
    expect(posts).toHaveLength(0);

    fireEvent.change(modeSelect, { target: { value: "yolo" } });
    await waitFor(() => expect(apply).not.toBeDisabled());
    fireEvent.click(apply);
    fireEvent.click(screen.getByTestId("zcode-mode-confirm"));
    await waitFor(() => expect(posts).toHaveLength(1));
    expect(posts[0].action).toMatchObject({
      kind: "mode",
      sessionId: SID,
      generation: GEN,
      mode: "yolo",
      confirmFullAccess: true,
    });
  });

  it("mode_ack_does_not_optimistically_change_confirmed_label", async () => {
    install();
    render(<ZcodeControlPanel sessionId={SID} />);
    await waitFor(() => expect(screen.getByTestId("zcode-mode-current")).toBeVisible());
    const label = screen.getByTestId("zcode-mode-current");
    expect(label.textContent).toBe("变更前确认");
    fireEvent.change(screen.getByTestId("zcode-mode-select"), { target: { value: "plan" } });
    fireEvent.click(screen.getByTestId("zcode-mode-apply"));
    await waitFor(() => expect(screen.getByTestId("zcode-action-status")).toBeVisible());
    // 回执 accepted ≠ 配置已生效：确认标签仍取新鲜 mode 投影。
    expect(label.textContent).toBe("变更前确认");
    expect(label.getAttribute("data-mode")).toBe("build");
  });
});
