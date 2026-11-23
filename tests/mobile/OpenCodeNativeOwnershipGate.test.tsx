import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import OpenCodeNativePanel from "@/mobile/OpenCodeNativePanel";
import { resetActionAnchorMemoryForTests } from "@/mobile/opencodeActionAnchors";
import { MockEventSource } from "./eventSourceMock";
import { pinZhNavigator } from "./pinZhNavigator";

const sessionId = "ownership-gate-test";
const json = (body: unknown) => new Response(JSON.stringify(body), { status: 200 });
let unpinZh: () => void;
let posts: Array<{ operation: { kind: string; paused?: boolean; targetActionId?: string } }>;
let uploads: number;
let paused: boolean;
let reason: string | null;
let runtimeState: "ready" | "running";

class IsolatedXHR {
  upload = { onprogress: null };
  open() {}
  setRequestHeader() {}
  send() {
    uploads += 1;
  }
  abort() {}
}

let emptyOutbox = false;

beforeAll(async () => {
  unpinZh = await pinZhNavigator();
});
afterAll(() => unpinZh());
beforeEach(() => {
  posts = [];
  uploads = 0;
  paused = false;
  emptyOutbox = false;
  reason = "OC_INSTANCE_OWNERSHIP_UNVERIFIED";
  runtimeState = "ready";
  sessionStorage.clear();
  resetActionAnchorMemoryForTests();
  MockEventSource.reset();
  vi.stubGlobal("EventSource", MockEventSource);
  vi.stubGlobal("XMLHttpRequest", IsolatedXHR);
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url.endsWith("/opencode/actions") && init?.method === "POST") {
        const body = JSON.parse(String(init.body));
        posts.push(body);
        return json({ actionId: body.actionId, state: "confirmed" });
      }
      if (url.includes("/opencode/view"))
        return json({
          sessionId,
          bindingToken: "binding-1",
          revision: 1,
          complete: true,
          bindingRequired: true,
          desktopBound: true,
          route: "opencode_native",
          routeReasonCode: reason,
          capabilities: Object.fromEntries(
            [
              "send",
              "stop_session",
              "set_agent",
              "answer_group",
              "permission_once",
              "permission_reject",
            ].map((key) => [key, { allowed: true, evidence: "contract", reasonCode: null }])
          ),
          messages: [],
          inbox: [],
          runtimeState,
          files: {},
          pending: [
            {
              kind: "permission",
              id: "perm-1",
              title: "授权",
              decisions: ["once", "reject"],
              context_token: "ctx",
            },
            {
              kind: "form",
              id: "form-1",
              title: "表单",
              context_token: "ctx",
              state: "unknown",
              fields: [
                {
                  key: "q0",
                  title: "选择",
                  required: true,
                  kind: { kind: "text", custom: false, options: [{ value: "yes", label: "同意" }] },
                },
              ],
            },
          ],
        });
      if (url.includes("/opencode/agents"))
        return json({
          current: "build",
          options: [
            { id: "build", name: "Build" },
            { id: "plan", name: "Plan" },
          ],
        });
      if (url.includes("/opencode/outbox"))
        return json({
          sessionId,
          revision: 3,
          paused,
          items: emptyOutbox
            ? []
            : [
                {
                  actionId: "local-1",
                  owner: "tuvis",
                  state: "queued",
                  textSummary: "待发",
                  attachmentCount: 0,
                },
              ],
        });
      throw new Error(`Unexpected request: ${url}`);
    })
  );
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  sessionStorage.clear();
});

async function openPanel() {
  render(<OpenCodeNativePanel sessionId={sessionId} />);
  await screen.findByTestId("native-input");
}

describe("OpenCode 归属未证的前端隔离", () => {
  it.each([
    ["归属未证", "OC_INSTANCE_OWNERSHIP_UNVERIFIED"],
    ["需要绑定", null],
  ])("%s 即使桌面已绑定也不得上传选中的附件", async (_label, code) => {
    reason = code;
    await openPanel();
    fireEvent.change(screen.getByTestId("native-file-input"), {
      target: { files: [new File(["x"], "private.txt", { type: "text/plain" })] },
    });
    expect(uploads).toBe(0);
  });

  it.each([
    ["归属未证", "OC_INSTANCE_OWNERSHIP_UNVERIFIED"],
    ["需要绑定", null],
  ])("%s 即使桌面已绑定也禁用原生动作", async (_label, code) => {
    reason = code;
    runtimeState = "running";
    await openPanel();
    fireEvent.change(screen.getByTestId("native-input"), { target: { value: "保留的草稿" } });
    const mode = await screen.findByTestId("native-mode-select");
    fireEvent.change(mode, { target: { value: "plan" } });
    expect(screen.getByTestId("native-send")).toBeDisabled();
    expect(screen.queryByTestId("native-stop")).not.toBeInTheDocument();
    expect(screen.getByTestId("native-mode-switch")).toBeDisabled();
    for (const button of screen.queryAllByTestId("native-decision")) expect(button).toBeDisabled();
    for (const button of screen.queryAllByTestId("opencode-q-submit"))
      expect(button).toBeDisabled();
    expect(posts).toHaveLength(0);
  });

  it("归属未证说明无法验证同一实例，不要求处理冲突服务", async () => {
    await openPanel();
    const gate = await screen.findByTestId("native-binding-gate");
    expect(gate.textContent).toMatch(/无法.{0,12}(验证|确认).{0,12}同一实例/);
    expect(gate.textContent).not.toMatch(/(关闭|处理|解决).{0,12}冲突.{0,6}服务/);
  });

  it("归属未证时草稿仍可编辑并保留", async () => {
    await openPanel();
    const input = screen.getByTestId("native-input");
    fireEvent.change(input, { target: { value: "本地草稿" } });
    fireEvent.click(screen.getByTestId("native-send"));
    expect(input).toHaveValue("本地草稿");
    expect(posts).toHaveLength(0);
  });

  it("归属未证时不再有暂停入口：发送键只按能力/门禁用，零暂停写", async () => {
    emptyOutbox = true;
    await openPanel();
    // 2026-11-22 用户口径：暂停不再有独立入口/说明框，面板与队列区都不提供暂停控件。
    expect(screen.queryByTestId("native-pause-entry")).toBeNull();
    expect(screen.queryByTestId("outbox-pause")).toBeNull();
    expect(screen.queryByTestId("native-outbox")).toBeNull();
    // 归属未证 ⇒ 发送键禁用（既有能力门语义不变），点击零 POST
    fireEvent.change(screen.getByTestId("native-input"), { target: { value: "归属未证草稿" } });
    expect(screen.getByTestId("native-send")).toBeDisabled();
    fireEvent.click(screen.getByTestId("native-send"));
    expect(posts).toHaveLength(0);
  });

  it("归属未证时仍可撤回本地待发项", async () => {
    paused = true;
    await openPanel();
    fireEvent.click(await screen.findByTestId("outbox-retract-local-1"));
    await waitFor(() => expect(posts).toHaveLength(1));
    expect(posts[0].operation).toMatchObject({ kind: "retract_outbox", targetActionId: "local-1" });
  });
});
