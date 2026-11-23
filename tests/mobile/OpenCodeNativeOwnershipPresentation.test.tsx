import { cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import OpenCodeNativePanel from "@/mobile/OpenCodeNativePanel";
import { addActionAnchor, resetActionAnchorMemoryForTests } from "@/mobile/opencodeActionAnchors";
import { MockEventSource } from "./eventSourceMock";
import { pinZhNavigator } from "./pinZhNavigator";

const sessionId = "ownership-presentation";
const ownership = "OC_INSTANCE_OWNERSHIP_UNVERIFIED";
let unpin: () => void;
let mode: "ownership" | "supported" | "unsupported";

beforeAll(async () => {
  unpin = await pinZhNavigator();
});
afterAll(() => unpin());
beforeEach(() => {
  mode = "ownership";
  sessionStorage.clear();
  resetActionAnchorMemoryForTests();
  MockEventSource.reset();
  vi.stubGlobal("EventSource", MockEventSource);
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      const json = (value: unknown) => new Response(JSON.stringify(value), { status: 200 });
      if (url.includes("/opencode/view")) {
        const rejected = mode === "ownership";
        const capability = (allowed: boolean, reasonCode: string | null = null) => ({
          allowed,
          evidence: "contract",
          reasonCode,
        });
        return json({
          sessionId,
          bindingToken: "bind-1",
          revision: 1,
          complete: true,
          route: "opencode_native",
          routeReasonCode: rejected ? ownership : null,
          bindingRequired: rejected,
          desktopBound: true,
          capabilities: {
            send: capability(!rejected, rejected ? ownership : null),
            answer_group: capability(
              !rejected && mode !== "unsupported",
              rejected ? ownership : mode === "unsupported" ? "OC_CAPABILITY_MISSING" : null
            ),
            permission_once: capability(!rejected),
            permission_reject: capability(!rejected),
          },
          messages: [],
          inbox: [],
          runtimeState: "ready",
          files: {},
          pending: [
            {
              kind: "permission",
              id: "perm-1",
              title: "审批请求",
              decisions: ["once", "reject"],
              context_token: "ctx",
            },
            {
              kind: "form",
              id: "form-1",
              title: "问题表单",
              context_token: "ctx",
              state: "unknown",
              fields: [
                {
                  key: "q0",
                  title: "选择环境",
                  required: true,
                  kind: { kind: "text", custom: false, options: [{ value: "yes", label: "是" }] },
                },
              ],
            },
          ],
        });
      }
      if (url.includes("/opencode/outbox"))
        return json({ sessionId, revision: 1, paused: false, items: [] });
      if (url.includes("/opencode/agents")) return json({ current: null, options: [] });
      if (url.includes("/opencode/actions/"))
        return json({ actionId: "pending-decision", state: "unconfirmed" });
      throw new Error(`Unexpected request: ${url}`);
    })
  );
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  sessionStorage.clear();
  resetActionAnchorMemoryForTests();
});

async function openPanel() {
  render(<OpenCodeNativePanel sessionId={sessionId} />);
  await screen.findByTestId("native-input");
  await screen.findByTestId("native-pending-permission");
}

describe("OpenCode 归属与能力的展示", () => {
  it("归属未证只在会话级说明，题卡和审批不重复解释", async () => {
    await openPanel();
    expect(screen.getByTestId("native-binding-gate")).toHaveTextContent(
      /无法.{0,12}(验证|确认).{0,12}同一实例/
    );
    expect(screen.getByTestId("opencode-question-card")).toBeInTheDocument();
    expect.soft(screen.queryByTestId("native-pending-form-binding")).not.toBeInTheDocument();
    expect.soft(screen.queryByTestId("native-pending-permission-binding")).not.toBeInTheDocument();
    expect.soft(screen.queryByTestId("native-send-blocked")).not.toBeInTheDocument();
  });

  it("展开连接详情后不重复会话级归属说明或将其写成能力受限", async () => {
    await openPanel();
    fireEvent.click(screen.getByTestId("native-connection-details-toggle"));
    const details = screen.getByTestId("native-connection-details");
    expect(
      within(details).queryByText(/尚无法确认此会话属于哪个 OpenCode 实例/)
    ).not.toBeInTheDocument();
    expect(details).not.toHaveTextContent(ownership);
    expect(screen.getByTestId("native-binding-gate")).toBeInTheDocument();
  });

  it("审批存在未决锚时显示待核对状态和核对入口，不误称能力受限", async () => {
    mode = "supported";
    addActionAnchor(sessionId, {
      actionId: "pending-decision",
      kind: "decide",
      targetId: "perm-1",
    });
    await openPanel();
    const row = screen.getByTestId("native-pending-permission");
    expect(screen.getByTestId("native-unresolved-actions")).toHaveTextContent(/待核对|未确认|未知/);
    expect(screen.getByTestId("native-action-verify")).toBeEnabled();
    expect(within(row).queryByTestId("native-decision")).not.toBeInTheDocument();
    expect(within(row).queryByTestId("native-pending-permission-readonly")).not.toBeInTheDocument();
  });

  it("真正未放行答题能力时仍说明该操作受限，而非归属未证", async () => {
    mode = "unsupported";
    await openPanel();
    const form = screen.getByTestId("native-pending-form");
    expect(within(form).getByTestId("native-pending-form-blocked")).toHaveTextContent(/能力未放行/);
    expect(form).not.toHaveTextContent(/无法.{0,12}(验证|确认).{0,12}同一实例/);
    expect(screen.queryByTestId("native-binding-gate")).not.toBeInTheDocument();
  });
});
