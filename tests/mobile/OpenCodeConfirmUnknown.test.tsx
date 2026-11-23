// patch3 D2 / O2 / H09-O2 的呈现面回归：
// - D2：死代际 acknowledged/unconfirmed 行的「确认终态未知并解锁重选」出口——
//   发送槽与「未确认动作」行提供出口；确认后发送解锁或移除未确认行；
//   后端 fail-closed 拒绝（活代际 409 OC_ACTION_STATE）时如实挂出拒绝短注、不解锁不清锚。
// - O2：读面 409（OC_RUNTIME_UNVERIFIED）必须经 nativeErrorKey 本地化映射，英文原文不上屏。
// - H09 parked O2/O3：停止回执槽在核对收敛 not_sent 后同步「已核对：未发出」短注。
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import OpenCodeNativePanel from "@/mobile/OpenCodeNativePanel";
import {
  addActionAnchor,
  removeActionAnchor,
  rememberActionBinding,
} from "@/mobile/opencodeActionAnchors";
import i18n from "@/i18n";
import { MockEventSource } from "./eventSourceMock";
import { pinZhNavigator } from "./pinZhNavigator";

let unpin: () => void;
beforeAll(async () => {
  unpin = await pinZhNavigator();
});
afterAll(() => unpin());

const session = "ses-confirm";
const binding = "bind-confirm";
const response = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status });

/** 可编程服务桩：各路由的回应可被用例逐个覆写。 */
type Routes = {
  view?: unknown;
  confirmUnknown?: { status: number; body: unknown };
  actionRead?: unknown;
  actionList?: () => unknown;
  verify?: unknown;
};
function installServer(routes: Routes, posts: string[]) {
  vi.stubGlobal("EventSource", MockEventSource as unknown as typeof EventSource);
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (init?.method === "POST" && url.endsWith("/confirm-unknown")) {
        posts.push(url);
        const refusal = routes.confirmUnknown ?? {
          status: 200,
          body: { state: "unknown_confirmed" },
        };
        return response(refusal.body, refusal.status);
      }
      if (init?.method === "POST" && url.endsWith("/verify")) {
        return response(
          routes.verify ?? { actionId: "x", state: "not_sent", reasonCode: "binding_unverified" }
        );
      }
      if (init?.method === "POST") {
        const body = JSON.parse(String(init.body)) as Record<string, unknown>;
        posts.push(url);
        return response({ actionId: body.actionId, state: "unconfirmed" });
      }
      if (url.includes("/opencode/view"))
        return response(
          routes.view ?? {
            sessionId: session,
            bindingToken: binding,
            revision: 1,
            complete: true,
            capabilities: {
              send: { allowed: true, evidence: "prompt", reasonCode: null },
              stop_session: { allowed: true, evidence: "interrupt", reasonCode: null },
            },
            messages: [],
            pending: [],
            inbox: [],
            runtimeState: "ready",
            files: {},
            route: "opencode_native",
            routeReasonCode: null,
          }
        );
      if (url.includes("/opencode/agents"))
        return response({ current: "build", options: [{ id: "build", name: "Build" }] });
      if (url.includes("/opencode/outbox"))
        return response({ sessionId: session, revision: 0, paused: false, items: [] });
      if (url.includes("/opencode/actions?")) return response(routes.actionList?.() ?? []);
      if (url.includes("/opencode/actions/"))
        return response(routes.actionRead ?? { actionId: "x", state: "unconfirmed" });
      throw new Error(`Unexpected request: ${url}`);
    })
  );
}

beforeEach(() => {
  sessionStorage.clear();
  MockEventSource.reset();
  vi.spyOn(window, "confirm").mockReturnValue(true);
});
afterEach(() => {
  cleanup();
  removeActionAnchor(session, "act_row");
  sessionStorage.clear();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

async function renderPanelWithUnknownReceipt() {
  const posts: string[] = [];
  installServer({}, posts);
  render(<OpenCodeNativePanel sessionId={session} />);
  await act(async () => {
    await Promise.resolve();
  });
  const input = (await screen.findByTestId("native-input")) as HTMLTextAreaElement;
  fireEvent.change(input, { target: { value: "确认未知探针" } });
  fireEvent.click(screen.getByTestId("native-send"));
  await screen.findByTestId("native-receipt-unconfirmed");
  return posts;
}

describe("确认终态未知（D2）——发送槽", () => {
  it("确认成功：清锚解锁、回执槽转「已确认未知」、恰一次确认 POST", async () => {
    const posts = await renderPanelWithUnknownReceipt();
    expect(posts.filter((p) => p.endsWith("/confirm-unknown"))).toHaveLength(0);
    fireEvent.click(screen.getByTestId("native-confirm-unknown"));
    await screen.findByTestId("native-receipt-unknown-confirmed");
    expect(i18n.t("mobile.openCodeNative.receipt.unknownConfirmed")).toBe(
      screen.getByTestId("native-receipt-unknown-confirmed").textContent
    );
    // 发送锁解除（锚清空），确认 POST 恰一次
    expect(screen.getByTestId("native-send")).not.toBeDisabled();
    expect(screen.queryByTestId("native-receipt-unconfirmed")).toBeNull();
    const confirmPosts = posts.filter((p) => p.endsWith("/confirm-unknown"));
    expect(confirmPosts).toHaveLength(1);
  });

  it("后端拒绝（活代际 409 OC_ACTION_STATE）：拒绝短注如实上屏，锚与未决槽保持", async () => {
    const posts: string[] = [];
    installServer(
      {
        confirmUnknown: {
          status: 409,
          body: {
            error: "OC_ACTION_STATE",
            reasonCode: "OC_ACTION_STATE",
            reason: "该动作所属的服务代际仍在线：请先用「只读核对」收敛结果",
          },
        },
      },
      posts
    );
    render(<OpenCodeNativePanel sessionId={session} />);
    await act(async () => {
      await Promise.resolve();
    });
    const input = (await screen.findByTestId("native-input")) as HTMLTextAreaElement;
    fireEvent.change(input, { target: { value: "拒绝面探针" } });
    fireEvent.click(screen.getByTestId("native-send"));
    await screen.findByTestId("native-receipt-unconfirmed");
    fireEvent.click(screen.getByTestId("native-confirm-unknown"));
    const note = await screen.findByTestId("native-confirm-unknown-error");
    expect(note.textContent).toContain("服务代际仍在线");
    expect(screen.getByTestId("native-receipt-unconfirmed")).toBeTruthy();
    expect(screen.getByTestId("native-send")).toBeDisabled();
  });
});

describe("确认终态未知（D2）——「未确认动作」行", () => {
  it("行上确认成功：锚行删除（账本归档面由队列如实呈现）", async () => {
    const posts: string[] = [];
    installServer({}, posts);
    rememberActionBinding("act_row", binding);
    addActionAnchor(session, { actionId: "act_row", kind: "stop", targetId: session });
    render(<OpenCodeNativePanel sessionId={session} />);
    const row = await screen.findByTestId("native-action-anchor");
    fireEvent.click(within(row).getByTestId("native-action-confirm-unknown"));
    await waitFor(() => expect(screen.queryByTestId("native-action-anchor")).toBeNull());
    expect(posts.filter((p) => p.endsWith("/confirm-unknown"))).toHaveLength(1);
  });
});

it("死代际已受理发送确认归档后移除未确认行与核对入口", async () => {
  const posts: string[] = [];
  const accepted = {
    actionId: "native",
    operation: "send",
    nativeId: "msg-native",
    runtimeDead: true,
  };
  installServer(
    {
      actionList: () => [
        { ...accepted, state: posts.length ? "unknown_confirmed" : "acknowledged" },
      ],
      confirmUnknown: { status: 200, body: { ...accepted, state: "unknown_confirmed" } },
    },
    posts
  );
  render(<OpenCodeNativePanel sessionId={session} />);
  const row = await screen.findByTestId("native-action-anchor");
  expect(row).toHaveAttribute("data-kind", "send");
  expect(within(row).getByTestId("native-action-verify")).toBeEnabled();
  fireEvent.click(within(row).getByTestId("native-action-confirm-unknown"));
  await waitFor(() => expect(screen.queryByTestId("native-action-anchor")).toBeNull());
  expect(screen.queryByTestId("native-action-verify")).toBeNull();
  expect(posts).toHaveLength(1);
  expect(posts[0]).toMatch(/\/native\/confirm-unknown$/);
});

describe("读面 409 本地化（O2）", () => {
  it("view 漂移 409 的上游英文 reason 原文不上屏，走 nativeErrorKey 中文短语", async () => {
    vi.stubGlobal("EventSource", MockEventSource as unknown as typeof EventSource);
    vi.stubGlobal(
      "fetch",
      vi.fn(async () =>
        response(
          {
            error: "OC_RUNTIME_UNVERIFIED",
            reasonCode: "OC_RUNTIME_UNVERIFIED",
            reason: "the fixed native service process is unavailable",
          },
          409
        )
      )
    );
    render(<OpenCodeNativePanel sessionId={session} />);
    const banner = await screen.findByTestId("native-load-error");
    expect(banner.textContent).toContain(i18n.t("mobile.openCodeNative.errors.runtimeUnverified"));
    expect(banner.textContent).not.toContain("the fixed native service");
    expect(banner.textContent).not.toContain("OC_RUNTIME_UNVERIFIED");
  });
});

describe("停止回执槽随核对同步（H09 parked O2/O3）", () => {
  it("停止失败后核对收敛 not_sent：回执槽更新为「已核对：未发出」短注", async () => {
    const posts: string[] = [];
    vi.stubGlobal("EventSource", MockEventSource as unknown as typeof EventSource);
    // 停止 → 409 OC_TRANSPORT_TIMEOUT（受控拒绝 → 停止槽「停止失败」+ 锚行未知）
    vi.stubGlobal(
      "fetch",
      vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
        const url = String(input);
        if (init?.method === "POST" && url.endsWith("/opencode/actions")) {
          posts.push(url);
          return response(
            {
              error: "OC_TRANSPORT_TIMEOUT",
              reasonCode: "OC_TRANSPORT_TIMEOUT",
              reason: "native transport timed out",
            },
            409
          );
        }
        if (init?.method === "POST" && url.endsWith("/verify")) {
          return response({ actionId: "x", state: "not_sent", reasonCode: "binding_unverified" });
        }
        if (url.includes("/opencode/view"))
          return response({
            sessionId: session,
            bindingToken: binding,
            revision: 1,
            complete: true,
            capabilities: {
              send: { allowed: true, evidence: "prompt", reasonCode: null },
              stop_session: { allowed: true, evidence: "interrupt", reasonCode: null },
            },
            messages: [],
            pending: [],
            inbox: [],
            runtimeState: "running",
            files: {},
            route: "opencode_native",
            routeReasonCode: null,
          });
        if (url.includes("/opencode/agents"))
          return response({ current: "build", options: [{ id: "build", name: "Build" }] });
        if (url.includes("/opencode/outbox"))
          return response({ sessionId: session, revision: 0, paused: false, items: [] });
        if (url.includes("/opencode/actions?")) return response(routes.actionList?.() ?? []);
        if (url.includes("/opencode/actions/"))
          return response({ actionId: "x", state: "unconfirmed" });
        throw new Error(`Unexpected request: ${url}`);
      })
    );
    render(<OpenCodeNativePanel sessionId={session} />);
    fireEvent.click(await screen.findByTestId("native-stop"));
    await screen.findByTestId("native-stop-result");
    expect(screen.getByTestId("native-stop-result").textContent).toContain("停止失败");
    // 锚行核对收敛 not_sent → 停止槽同步「已核对：未发出」
    fireEvent.click(screen.getByTestId("native-action-verify"));
    await screen.findByText(i18n.t("mobile.openCodeNative.stop.verifiedNotSent"));
    expect(screen.getByTestId("native-stop-result").textContent).toContain("已核对：未发出");
  });
});
