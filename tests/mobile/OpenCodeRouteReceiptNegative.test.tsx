import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import OpenCodeNativePanel from "@/mobile/OpenCodeNativePanel";
import { returnOpenCodeRouteToLegacy, type PublicNativeView } from "@/mobile/opencodeNativeApi";
import { MockEventSource } from "./eventSourceMock";
import { pinZhNavigator } from "./pinZhNavigator";

const sessionId = "sess-route";
const token = "bind_old";
const view: PublicNativeView = {
  sessionId,
  bindingToken: token,
  routeVersion: 4,
  revision: 1,
  complete: true,
  capabilities: { send: { allowed: true, evidence: "contract", reasonCode: null } },
  messages: [{ seq: 0, role: "assistant", kind: "assistant", content: "旧目标", collapsed: false }],
  pending: [],
  inbox: [],
  runtimeState: "ready",
  files: {},
  route: "opencode_native",
  routeReasonCode: null,
};
const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status });
const windowIdentity = { pid: 123, startedAtMs: 123, boot: "boot" };
const committedRoute = {
  sessionId,
  state: "committed",
  current: true,
  window: windowIdentity,
  bindingToken: "bind_new",
  routeVersion: 5,
};
const native = { ...committedRoute, route: "opencode_native" };

function requestIdFrom(confirmation: Record<string, unknown> | undefined) {
  const requestId = confirmation?.requestId;
  expect(typeof requestId).toBe("string");
  expect(requestId).toBeTruthy();
  return requestId as string;
}
const legacy = { sessionId, route: "legacy", bindingToken: token, routeVersion: 5, abandoned: 0 };
const malformed = [
  ["空体", ""],
  ["坏 JSON", "{"],
] as const;
const variants = (base: Record<string, unknown>) =>
  [
    ...malformed,
    ["错误 session", JSON.stringify({ ...base, sessionId: "other" })],
    ["错误 route", JSON.stringify({ ...base, route: "wrong" })],
    ["缺失 token", JSON.stringify({ ...base, bindingToken: undefined })],
    ["空 token", JSON.stringify({ ...base, bindingToken: " " })],
    ["缺失 routeVersion", JSON.stringify({ ...base, routeVersion: undefined })],
    ...[0, -1, 1.5, "5", null].map((routeVersion) => [
      `无效 routeVersion ${String(routeVersion)}`,
      JSON.stringify({ ...base, routeVersion }),
    ]),
  ] as Array<[string, string]>;

let unpin: () => void;
beforeAll(async () => {
  unpin = await pinZhNavigator();
});
afterAll(() => unpin());
beforeEach(() => {
  sessionStorage.clear();
  MockEventSource.reset();
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

async function setup(
  body: string,
  force = false,
  failure?: "native" | "legacy" | "preview" | "native-timeout",
  check?: Promise<Response>,
  receiptStatus?: (requestId: string) => Response | Promise<Response>
) {
  let stale = false;
  const posts: Array<{ path: string; body: Record<string, unknown> }> = [];
  const reads: Array<string | null> = [];
  const onRouteChanged = vi.fn();
  vi.stubGlobal("EventSource", MockEventSource);
  vi.spyOn(window, "confirm").mockReturnValue(true);
  vi.stubGlobal(
    "fetch",
    vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url.includes("/opencode/route/preview?")) {
        if (failure === "preview") return Promise.reject(new TypeError("preview network"));
        return Promise.resolve(
          json({
            sessionId,
            candidate: {
              sessionId,
              directory: "/project",
              serviceId: "service",
              ...windowIdentity,
              bindingToken: committedRoute.bindingToken,
            },
            previewToken: "preview-token",
            expiresAtMs: 9999999999999,
          })
        );
      }
      if (url.includes("/opencode/route/check?")) {
        return check ?? Promise.resolve(json({ sessionId, routeState: "none" }));
      }
      if (url.includes("/opencode/route/native/status?")) {
        expect(init?.method ?? "GET").toBe("GET");
        const requestId = requestIdFrom(posts[0]?.body);
        const query = new URL(url, "http://localhost").searchParams;
        expect(query.get("requestId")).toBe(requestId);
        expect(query.get("sessionId")).toBe(sessionId);
        return Promise.resolve(
          receiptStatus?.(requestId) ?? json({ sessionId, requestId, state: "in_progress" })
        );
      }
      if (url.includes("/opencode/route/")) {
        posts.push({ path: url, body: JSON.parse(String(init?.body)) });
        if (failure === "native-timeout" && url.endsWith("/route/native")) {
          const signal = init?.signal;
          if (!signal) throw new Error("missing timeout signal on confirmation POST");
          return new Promise<Response>((_resolve, reject) => {
            signal.addEventListener("abort", () => reject(signal.reason), { once: true });
          });
        }
        if (failure && url.endsWith(`/route/${failure}`))
          return Promise.reject(new TypeError("route network"));
        if (force && posts.length === 1)
          return Promise.resolve(
            json(
              {
                error: "OC_ROUTE_CHANGED",
                unverifiableActions: 2,
                forceAvailable: true,
                unverifiableActionIds: ["a", "b"],
                routeVersion: 4,
              },
              409
            )
          );
        if (url.endsWith("/route/native") && body.startsWith("{") && body.endsWith("}")) {
          const receipt = JSON.parse(body);
          return Promise.resolve(json({ ...receipt, requestId: posts.at(-1)?.body.requestId }));
        }
        return Promise.resolve(new Response(body, { status: 200 }));
      }
      if (url.includes("/opencode/view")) {
        const expected = new Headers(init?.headers).get("X-OpenCode-Expected-Binding");
        reads.push(expected);
        if ((check || receiptStatus) && expected === committedRoute.bindingToken)
          return Promise.resolve(
            json({
              ...view,
              bindingToken: committedRoute.bindingToken,
              routeVersion: committedRoute.routeVersion,
              messages: [{ ...view.messages[0], content: "新目标" }],
            })
          );
        if (stale || (expected && expected !== token))
          return Promise.resolve(json({ error: "OC_BINDING_STALE" }, 409));
        return Promise.resolve(json(view));
      }
      if (url.includes("/opencode/agents"))
        return Promise.resolve(json({ current: null, options: [] }));
      if (url.includes("/opencode/outbox"))
        return Promise.resolve(json({ sessionId, revision: 0, paused: false, items: [] }));
      if (url.includes("/channel")) return Promise.resolve(json({ via: "lan", limited: false }));
      if (url.includes("/opencode/actions")) {
        // J2：GET /opencode/actions?sessionId= 是**清单发现**（无锚核对入口），
        // 不属于执行投递序列——posts 只记 POST（执行）。
        if ((init?.method ?? "GET") !== "POST") {
          return Promise.resolve(json([]));
        }
        posts.push({ path: url, body: {} });
        return Promise.resolve(json({}));
      }
      throw new Error(`unexpected fetch: ${url}`);
    })
  );
  render(<OpenCodeNativePanel sessionId={sessionId} onRouteChanged={onRouteChanged} />);
  const input = await screen.findByTestId("native-input");
  fireEvent.change(input, { target: { value: "保留草稿" } });
  await waitFor(() => expect(MockEventSource.instances).toHaveLength(1));
  fireEvent.click(screen.getByTestId("native-messages-toggle"));
  expect(screen.getByText("旧目标")).toBeVisible();
  return {
    input,
    posts,
    reads,
    onRouteChanged,
    makeStale: async () => {
      stale = true;
      act(() => MockEventSource.latest().emit("revision", "2"));
      await screen.findByTestId("native-reselect");
    },
  };
}

it("stale 恢复读返回 500 时仍可预览并确认重选目标", async () => {
  const reads: Array<string | null> = [];
  const steps: string[] = [];
  let requestId: string;
  let stale = false;
  vi.stubGlobal("EventSource", MockEventSource);
  const confirm = vi.spyOn(window, "confirm").mockImplementation(() => {
    steps.push("confirm");
    return true;
  });
  vi.stubGlobal(
    "fetch",
    vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url.includes("/opencode/view")) {
        const expected = new Headers(init?.headers).get("X-OpenCode-Expected-Binding");
        reads.push(expected);
        if (expected === committedRoute.bindingToken)
          return Promise.resolve(
            json({
              ...view,
              bindingToken: committedRoute.bindingToken,
              routeVersion: committedRoute.routeVersion,
            })
          );
        if (stale)
          return Promise.resolve(
            expected
              ? json({ error: "OC_BINDING_STALE" }, 409)
              : json({ error: "server error" }, 500)
          );
        return Promise.resolve(json(view));
      }
      if (url.includes("/opencode/route/preview?")) {
        steps.push("preview");
        return Promise.resolve(
          json({
            sessionId,
            candidate: {
              sessionId,
              directory: "/project",
              serviceId: "service",
              ...windowIdentity,
              bindingToken: committedRoute.bindingToken,
            },
            previewToken: "preview-token",
            expiresAtMs: 9999999999999,
          })
        );
      }
      if (url.endsWith("/opencode/route/native")) {
        steps.push("post");
        expect(init?.method).toBe("POST");
        const confirmation = JSON.parse(String(init?.body));
        expect(confirmation.candidateToken).toBe("preview-token");
        requestId = requestIdFrom(confirmation);
        return Promise.resolve(json({ ...native, requestId }));
      }
      if (url.includes("/opencode/route/native/status?")) {
        const query = new URL(url, "http://localhost").searchParams;
        expect(query.get("sessionId")).toBe(sessionId);
        expect(query.get("requestId")).toBe(requestId);
        return Promise.resolve(json({ ...committedRoute, requestId }));
      }
      if (url.includes("/opencode/agents"))
        return Promise.resolve(json({ current: null, options: [] }));
      if (url.includes("/opencode/outbox"))
        return Promise.resolve(json({ sessionId, revision: 0, paused: false, items: [] }));
      if (url.includes("/channel")) return Promise.resolve(json({ via: "lan", limited: false }));
      throw new Error(`unexpected fetch: ${url}`);
    })
  );
  render(<OpenCodeNativePanel sessionId={sessionId} />);
  await screen.findByTestId("native-input");
  await waitFor(() => expect(MockEventSource.instances).toHaveLength(1));
  stale = true;
  act(() => MockEventSource.latest().emit("revision", "2"));
  await waitFor(() => expect(reads).toEqual([null, token, null]));
  expect(await screen.findByTestId("native-load-error")).toBeVisible();
  const reselect = screen.getByTestId("native-reselect");
  expect(reselect).toBeVisible();
  expect(reselect).toBeEnabled();
  fireEvent.click(reselect);
  await waitFor(() => expect(reads).toContain("bind_new"));
  expect(confirm).toHaveBeenCalledOnce();
  expect(steps).toEqual(["preview", "confirm", "post"]);
  await waitFor(() => expect(screen.queryByTestId("native-load-error")).toBeNull());
});

describe("强制交回回执解析", () => {
  it.each([undefined, -1, 1.5, "2", 0, 1, 3])(
    "拒绝 abandoned=%s，与确认的两个动作不符",
    async (abandoned) => {
      const fetchMock = vi.fn().mockResolvedValue(json({ ...legacy, abandoned }));
      vi.stubGlobal("fetch", fetchMock);
      await expect(
        returnOpenCodeRouteToLegacy(sessionId, token, 4, true, ["a", "b"])
      ).rejects.toThrow();
      expect(fetchMock).toHaveBeenCalledTimes(1);
    }
  );
});

describe("丢失回执后的只读核对", () => {
  it("确认 POST 超时后旧路由仍可读时保持待确认，收到本次提交回执后收敛", async () => {
    const deadline = new AbortController();
    vi.spyOn(AbortSignal, "timeout").mockReturnValueOnce(deadline.signal);
    let releaseOldRoute!: (response: Response) => void;
    const oldRoute = new Promise<Response>((resolve) => {
      releaseOldRoute = resolve;
    });
    let committed = false;
    const receiptStatusRead = vi.fn();
    const state = await setup("", false, "native-timeout", oldRoute, (requestId) => {
      receiptStatusRead();
      return json(
        committed
          ? { ...committedRoute, requestId }
          : { sessionId, requestId, state: "in_progress" }
      );
    });
    await state.makeStale();
    fireEvent.click(screen.getByTestId("native-reselect"));
    await waitFor(() => expect(state.posts).toHaveLength(1));
    expect(state.posts[0]?.body.candidateToken).toBe("preview-token");
    await act(async () => {
      deadline.abort(new DOMException("Confirmation timed out", "TimeoutError"));
      releaseOldRoute(
        json({
          sessionId,
          routeState: "active",
          bindingToken: token,
          routeVersion: view.routeVersion,
        })
      );
    });

    const pendingNotice = /重新选择请求的结果未确认/;
    expect(await screen.findByText(pendingNotice)).toBeVisible();
    expect(screen.queryByText("路由尚未提交：本次未写出，可安全重试")).not.toBeInTheDocument();
    expect(screen.getByText("旧目标")).toBeVisible();
    expect(state.input).toHaveValue("保留草稿");
    await waitFor(() => expect(receiptStatusRead).toHaveBeenCalled());
    committed = true;
    expect(await screen.findByText("新目标")).toBeVisible();
    expect(screen.queryByText(pendingNotice)).not.toBeInTheDocument();
    expect(
      vi.mocked(fetch).mock.calls.some(([url]) => String(url).includes("/opencode/route/check?"))
    ).toBe(false);
    expect(state.posts.map((post) => post.path)).toEqual(["/m/api/v1/opencode/route/native"]);
  });

  it("本次请求收到 not_applied 后才显示未提交", async () => {
    const state = await setup("", false, "native", undefined, (requestId) =>
      json({ sessionId, requestId, state: "not_applied" })
    );
    await state.makeStale();
    fireEvent.click(screen.getByTestId("native-reselect"));

    expect(await screen.findByText("路由尚未提交：本次未写出，可安全重试")).toBeVisible();
    expect(screen.queryByText(/重新选择请求的结果未确认/)).not.toBeInTheDocument();
    expect(screen.getByText("旧目标")).toBeVisible();
    expect(state.posts.map((post) => post.path)).toEqual(["/m/api/v1/opencode/route/native"]);
  });

  it.each(["网络失败", "无效回执"] as const)(
    "%s 且候选已生效时读取新视图而不重发",
    async (failure) => {
      let release!: () => void;
      const ready = new Promise<void>((resolve) => {
        release = resolve;
      });
      const receiptRead = vi.fn(async (requestId: string) => {
        await ready;
        return json({ ...committedRoute, requestId });
      });
      const state = await setup(
        "{",
        false,
        failure === "网络失败" ? "native" : undefined,
        undefined,
        receiptRead
      );
      await state.makeStale();
      fireEvent.click(screen.getByTestId("native-reselect"));
      expect(await screen.findByTestId("native-reselect-unconfirmed")).toBeVisible();
      expect(receiptRead).toHaveBeenCalledWith(requestIdFrom(state.posts[0]?.body));
      await act(async () => release());
      expect(await screen.findByText("新目标")).toBeVisible();
      expect(screen.queryByTestId("native-reselect-unconfirmed")).toBeNull();
      expect(state.reads).toContain("bind_new");
      expect(state.posts.map((p) => p.path)).toEqual(["/m/api/v1/opencode/route/native"]);
    }
  );

  it.each([
    ["请求仍在进行", { state: "in_progress" }],
    ["已提交但绑定不匹配", { ...committedRoute, bindingToken: "other" }],
    ["已提交但目标已非当前", { ...committedRoute, current: false }],
  ] as const)("%s 时保留原目标", async (_name, result) => {
    let release!: () => void;
    const ready = new Promise<void>((resolve) => {
      release = resolve;
    });
    const receiptRead = vi.fn(async (requestId: string) => {
      await ready;
      return json({ sessionId, requestId, ...result });
    });
    const state = await setup("{", false, undefined, undefined, receiptRead);
    await state.makeStale();
    fireEvent.click(screen.getByTestId("native-reselect"));
    expect(await screen.findByTestId("native-reselect-unconfirmed")).toBeVisible();
    expect(receiptRead).toHaveBeenCalledWith(requestIdFrom(state.posts[0]?.body));
    await act(async () => release());
    if ("current" in result && result.current === false) {
      expect(screen.getByTestId("native-reselect-superseded")).toBeVisible();
    } else {
      expect(screen.getByTestId("native-reselect-unconfirmed")).toBeVisible();
    }
    expect(state.reads).not.toContain("bind_new");
    expect(state.posts).toHaveLength(1);
  });
});

describe("路由回执不可信时保留原目标", () => {
  it.each(["native", "legacy"] as const)(
    "%s POST 网络失败时提示未确认且不重发",
    async (endpoint) => {
      const state = await setup("", false, endpoint);
      if (endpoint === "native") await state.makeStale();
      fireEvent.click(
        screen.getByTestId(endpoint === "native" ? "native-reselect" : "native-return-legacy")
      );
      const notice =
        endpoint === "native"
          ? await screen.findByRole("status")
          : await screen.findByTestId("native-return-error");
      expect(notice).toHaveTextContent(/未确认/);
      expect(state.posts).toHaveLength(1);
      expect(state.input).toHaveValue("保留草稿");
      expect(state.onRouteChanged).not.toHaveBeenCalled();
    }
  );

  it("预览 GET 网络失败时提示重选失败且不提交", async () => {
    const state = await setup("", false, "preview");
    await state.makeStale();
    fireEvent.click(screen.getByTestId("native-reselect"));
    expect(await screen.findByRole("status")).toHaveTextContent(/无法重新选择目标/);
    expect(state.posts).toHaveLength(0);
    expect(state.onRouteChanged).not.toHaveBeenCalled();
  });
  it.each([
    ...variants(native),
    ["错误 token 不匹配后续只读投影", JSON.stringify({ ...native, bindingToken: "bind_wrong" })],
  ])("native：%s 时保留原目标且不自动切换", async (_name, body) => {
    const state = await setup(body);
    await state.makeStale();
    fireEvent.click(screen.getByTestId("native-reselect"));
    await waitFor(() => expect(screen.getByRole("status")).toBeVisible());
    expect.soft(state.input).toHaveValue("保留草稿");
    expect.soft(screen.getByText("旧目标")).toBeVisible();
    expect.soft(state.posts.map((p) => p.path)).toEqual(["/m/api/v1/opencode/route/native"]);
    expect.soft(state.posts[0]?.body.candidateToken).toBe("preview-token");
    expect.soft(state.onRouteChanged).not.toHaveBeenCalled();
    expect.soft(screen.getByRole("status")).toHaveTextContent(/未确认/);
  });

  it.each([
    ...variants(legacy),
    ["错误 token", JSON.stringify({ ...legacy, bindingToken: "bind_wrong" })],
    ["错误版本", JSON.stringify({ ...legacy, routeVersion: 7 })],
    ["非 force 却作废动作", JSON.stringify({ ...legacy, abandoned: 1 })],
  ])("legacy：%s 的结果未确认且不自动切换", async (_name, body) => {
    const state = await setup(body);
    fireEvent.click(screen.getByTestId("native-return-legacy"));
    const error = await screen.findByTestId("native-return-error");
    expect.soft(state.input).toHaveValue("保留草稿");
    expect.soft(screen.getByText("旧目标")).toBeVisible();
    expect.soft(state.posts.map((p) => p.path)).toEqual(["/m/api/v1/opencode/route/legacy"]);
    expect.soft(state.onRouteChanged).not.toHaveBeenCalled();
    expect.soft(error).toHaveTextContent(/未确认/);
  });

  it.each([undefined, -1, 1.5, "2", 0, 1, 3])(
    "force 交回 abandoned=%s 时结果未确认且不自动切换",
    async (abandoned) => {
      const state = await setup(JSON.stringify({ ...legacy, abandoned }), true);
      fireEvent.click(screen.getByTestId("native-return-legacy"));
      await waitFor(() => expect(state.posts).toHaveLength(1));
      expect(state.posts[0].body).toEqual({ sessionId, bindingToken: token, routeVersion: 4 });
      fireEvent.click(await screen.findByTestId("native-force-confirm"));
      await waitFor(() => expect(state.posts).toHaveLength(2));
      await act(async () => {
        await Promise.resolve();
      });
      expect.soft(state.posts.map((p) => p.body.force ?? false)).toEqual([false, true]);
      expect.soft(state.input).toHaveValue("保留草稿");
      expect.soft(screen.getByText("旧目标")).toBeVisible();
      expect.soft(state.onRouteChanged).not.toHaveBeenCalled();
      expect.soft(screen.queryByTestId("native-return-error")).toHaveTextContent(/未确认/);
    }
  );
});
