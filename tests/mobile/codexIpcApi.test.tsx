import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  CODEX_SSE_DEGRADE_AFTER,
  CODEX_SSE_RECONNECT_BASE_MS,
  CODEX_THREAD_EVENTS_PATH,
  CODEX_THREAD_PATH,
  fetchCodexAction,
  fetchCodexQueue,
  fetchCodexThread,
  newActionId,
  respondCodexPending,
  retractCodexAction,
  subscribeCodexThread,
  type CapabilitySet,
  type ThreadProjection,
} from "@/mobile/codexIpcApi";
import { sessionSend, type SendResult } from "@/mobile/api";
import { ApiError } from "@/mobile/api";
import { MockEventSource } from "./eventSourceMock";

// Task 9：Codex Desktop IPC 客户端（src/mobile/codexIpcApi.ts）契约锁。
// DTO 与 Rust `services/codex_desktop_ipc/types.rs` serde 输出**逐字段相同**
// （camelCase 字段；枚举值 snake_case）；requestKey 永远是 string（`n:<十进制>` /
// `s:<原串>`——浏览器侧 2^53 以上整数只有字符串形态无损）。
// fetch 全量 stub（盖过 setup.ts 的 msw）；SSE 用 tests/mobile/eventSourceMock。

function caps(over: Partial<CapabilitySet> = {}): CapabilitySet {
  return {
    send: true,
    state: true,
    command: true,
    permissions: true,
    blockingInput: true,
    ...over,
  };
}

function projection(over: Partial<ThreadProjection> = {}): ThreadProjection {
  return {
    threadId: "th-1",
    revision: 7,
    stale: false,
    availability: "controllable",
    capabilities: caps(),
    runtimeStatus: "idle",
    messages: [{ id: "m-1", role: "user", kind: "user_input", text: "你好", ts: 1000 }],
    truncated: false,
    pending: [],
    ...over,
  };
}

function receipt(delivery: string) {
  return {
    actionId: "act-1",
    delivery,
    route: "desktop_ipc",
    turnId: "turn-1",
    envelopeId: "env-1",
    reasonCode: null,
  };
}

interface Routes {
  thread?: ThreadProjection;
  threadStatus?: number;
  action?: Record<string, unknown>;
  actionStatus?: number;
  respond?: Record<string, unknown>;
  respondStatus?: number;
  respondBody?: Record<string, unknown>;
  queue?: Record<string, unknown>;
  queueStatus?: number;
  retract?: Record<string, unknown>;
  retractStatus?: number;
  send?: Record<string, unknown>;
  sendStatus?: number;
}

let routes: Routes;
let fetchMock: ReturnType<typeof vi.fn>;

beforeEach(() => {
  routes = {};
});
afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

function installFetch() {
  fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    // 判序：长路径在前（/codex/thread/events ⊃ /codex/thread；/codex/queue/retract ⊃ /codex/queue）
    if (url.includes("/codex/thread/events")) {
      return new Response("data: {}\n\n", {
        status: 200,
        headers: { "content-type": "text/event-stream" },
      });
    }
    if (url.includes(CODEX_THREAD_PATH)) {
      if (routes.threadStatus) return new Response("no", { status: routes.threadStatus });
      return new Response(JSON.stringify(routes.thread ?? projection()), { status: 200 });
    }
    if (url.includes("/codex/pending-response")) {
      if (routes.respondStatus) {
        return new Response(JSON.stringify(routes.respondBody ?? { error: "internal" }), {
          status: routes.respondStatus,
        });
      }
      return new Response(JSON.stringify(routes.respond ?? receipt("accepted")), { status: 200 });
    }
    if (url.includes("/codex/queue/retract")) {
      if (routes.retractStatus) {
        return new Response(JSON.stringify({ error: "action_conflict" }), {
          status: routes.retractStatus,
        });
      }
      return new Response(
        JSON.stringify(routes.retract ?? { actionId: "a1", state: "cancelled" }),
        {
          status: 200,
        }
      );
    }
    if (url.includes("/codex/queue")) {
      if (routes.queueStatus) return new Response("no", { status: routes.queueStatus });
      return new Response(
        JSON.stringify(routes.queue ?? { actions: [], blockedReasonCode: null }),
        {
          status: 200,
        }
      );
    }
    if (url.includes("/codex/action")) {
      if (routes.actionStatus) return new Response("no", { status: routes.actionStatus });
      return new Response(JSON.stringify(routes.action ?? { actionId: "a1", state: "queued" }), {
        status: 200,
      });
    }
    if (url.includes("/session-send")) {
      if (routes.sendStatus) return new Response("no", { status: routes.sendStatus });
      return new Response(JSON.stringify(routes.send ?? { status: "delivered" }), { status: 200 });
    }
    throw new Error(`unexpected fetch: ${url} ${init?.method ?? ""}`);
  });
  vi.stubGlobal("fetch", fetchMock);
}

/** 最近一次 POST /codex/pending-response 的请求体 */
function lastRespondBody(): Record<string, unknown> {
  const call = fetchMock.mock.calls.find(([u]) => String(u).includes("/codex/pending-response"));
  expect(call).toBeTruthy();
  return JSON.parse(String((call![1] as RequestInit).body)) as Record<string, unknown>;
}

describe("codexIpcApi：Thread 投影拉取", () => {
  it("fetch_codex_thread_get：GET /m/api/v1/codex/thread?session_id=，载荷逐字段透传", async () => {
    installFetch();
    routes.thread = projection({ availability: "send_only", revision: 9 });
    const p = await fetchCodexThread("s1");
    expect(p).toEqual(routes.thread);
    expect(String(fetchMock.mock.calls[0][0])).toBe(`${CODEX_THREAD_PATH}?session_id=s1`);
  });

  it("fetch_codex_thread_403_null：设备失效同 fetchSendInfo 语义返回 null（不抛）", async () => {
    installFetch();
    routes.threadStatus = 403;
    await expect(fetchCodexThread("s1")).resolves.toBeNull();
  });

  it("fetch_codex_thread_404_throws：其余非 2xx 抛 ApiError（status 携带）", async () => {
    installFetch();
    routes.threadStatus = 404;
    const err = await fetchCodexThread("s1").catch((e: unknown) => e);
    expect(err).toBeInstanceOf(ApiError);
    expect((err as ApiError).status).toBe(404);
  });
});

describe("codexIpcApi：pending-response 契约（S3/S4/S5 红线）", () => {
  it("respond_codex_pending_big_int_request_key_lossless：大整数 key 以字符串无损上行", async () => {
    installFetch();
    routes.respond = receipt("accepted");
    const r = await respondCodexPending({
      sessionId: "s1",
      actionId: "act-1",
      requestKey: "n:9007199254740993",
      contextToken: "tok-1",
      action: "approve",
    });
    expect(r.delivery).toBe("accepted");
    const body = lastRespondBody();
    // 2^53 以上整数：requestKey 永远是 string，无损往返（计划 Review Focus 3）
    expect(body.requestKey).toBe("n:9007199254740993");
    expect(typeof body.requestKey).toBe("string");
  });

  it("respond_codex_pending_never_sends_owner_fields：绝不携带 ownerClientId/targetClientId/raw method", async () => {
    installFetch();
    await respondCodexPending({
      sessionId: "s1",
      actionId: "act-1",
      requestKey: "s:req-9",
      contextToken: "tok-1",
      action: "answer",
      answers: { q1: ["A"] },
    });
    const body = lastRespondBody();
    expect(body).not.toHaveProperty("ownerClientId");
    expect(body).not.toHaveProperty("targetClientId");
    expect(body.method).toBeUndefined();
    // 白名单键集合：多一个键都不行（后端 deny_unknown_fields 会 400）
    expect(Object.keys(body).sort()).toEqual(
      ["action", "actionId", "answers", "contextToken", "requestKey", "sessionId"].sort()
    );
    expect(body.action).toBe("answer");
    expect(body.answers).toEqual({ q1: ["A"] });
  });

  it("respond_codex_pending_unknown_delivery_is_200_receipt：unknown_delivery 是明确回执不是网络错误（S5）", async () => {
    installFetch();
    routes.respond = receipt("unknown_delivery");
    const r = await respondCodexPending({
      sessionId: "s1",
      actionId: "act-1",
      requestKey: "n:12",
      contextToken: "tok-1",
      action: "reject",
    });
    expect(r.delivery).toBe("unknown_delivery");
  });

  it("respond_codex_pending_409_stale_context：过期上下文抛 ApiError(409)", async () => {
    installFetch();
    routes.respondStatus = 409;
    routes.respondBody = { error: "stale_context" };
    const err = await respondCodexPending({
      sessionId: "s1",
      actionId: "act-1",
      requestKey: "n:12",
      contextToken: "tok-old",
      action: "approve",
    }).catch((e: unknown) => e);
    expect(err).toBeInstanceOf(ApiError);
    expect((err as ApiError).status).toBe(409);
  });
});

describe("codexIpcApi：动作查询 / 队列查看与撤回", () => {
  it("fetch_codex_action：GET ?action_id=；404（他人/不存在）抛 ApiError", async () => {
    installFetch();
    routes.action = {
      actionId: "a1",
      state: "unknown",
      position: null,
      receipt: receipt("unknown_delivery"),
    };
    const v = await fetchCodexAction("a1");
    expect(v).toEqual(routes.action);
    expect(String(fetchMock.mock.calls[0][0])).toContain("/codex/action?action_id=a1");

    routes.actionStatus = 404;
    const err = await fetchCodexAction("a2").catch((e: unknown) => e);
    expect((err as ApiError).status).toBe(404);
  });

  it("fetch_codex_queue：GET ?session_id=；retract 200 → CodexActionView", async () => {
    installFetch();
    routes.queue = { actions: [], blockedReasonCode: "compat_outstanding" };
    const q = await fetchCodexQueue("s1");
    expect(q).toEqual(routes.queue);
    routes.retract = { actionId: "a1", state: "cancelled", position: null, receipt: null };
    const v = await retractCodexAction("a1");
    expect(v.state).toBe("cancelled");
    const call = fetchMock.mock.calls.find(([u]) => String(u).includes("/codex/queue/retract"));
    expect(JSON.parse(String((call![1] as RequestInit).body))).toEqual({ actionId: "a1" });
  });

  it("retract_409_keeps_entry_semantics：已派发 409 抛 ApiError；404 抛 ApiError", async () => {
    installFetch();
    routes.retractStatus = 409;
    const e409 = await retractCodexAction("a1").catch((e: unknown) => e);
    expect((e409 as ApiError).status).toBe(409);
    routes.retractStatus = 404;
    const e404 = await retractCodexAction("a1").catch((e: unknown) => e);
    expect((e404 as ApiError).status).toBe(404);
  });
});

describe("codexIpcApi：sessionSend 的 Codex APP actionId 契约", () => {
  it("session_send_with_action_id：传入 actionId → body 携带；desktop_ipc / codex_queued 回执透传", async () => {
    installFetch();
    routes.send = {
      status: "desktop_ipc",
      receipt: receipt("accepted"),
    } as unknown as SendResult;
    const r1 = await sessionSend("s1", "hi", undefined, "act-9");
    expect(r1).toEqual(routes.send);
    let call = fetchMock.mock.calls.find(([u]) => /\/session-send$/.test(String(u)));
    expect(JSON.parse(String((call![1] as RequestInit).body))).toEqual({
      sessionId: "s1",
      text: "hi",
      actionId: "act-9",
    });

    routes.send = {
      status: "codex_queued",
      actionId: "act-9",
      position: 2,
    } as unknown as SendResult;
    const r2 = await sessionSend("s1", "hi", undefined, "act-9");
    expect(r2).toEqual(routes.send);
    call = fetchMock.mock.calls.filter(([u]) => /\/session-send$/.test(String(u)))[1];
    expect(JSON.parse(String((call![1] as RequestInit).body)).actionId).toBe("act-9");
  });

  it("session_send_without_action_id：CLI 旧合同零漂移——body 不含 actionId 键", async () => {
    installFetch();
    routes.send = { status: "delivered" };
    await sessionSend("s1", "hi");
    const call = fetchMock.mock.calls.find(([u]) => /\/session-send$/.test(String(u)));
    expect(JSON.parse(String((call![1] as RequestInit).body))).toEqual({
      sessionId: "s1",
      text: "hi",
    });
  });

  it("session_send_headless_fallback_receipt：fallback 旧 headless 封套附 actionId 可解析", async () => {
    installFetch();
    routes.send = {
      status: "headless",
      channel: "headless_codex_queue",
      actionId: "act-9",
      receipt: { status: "queued", sessionId: "s1", durationMs: 5 },
    } as unknown as SendResult;
    const r = await sessionSend("s1", "hi", undefined, "act-9");
    if (r.status !== "headless") throw new Error("expected headless envelope");
    expect(r.actionId).toBe("act-9");
    expect(r.receipt.status).toBe("queued");
  });
});

describe("codexIpcApi：actionId 生成", () => {
  it("new_action_id_uuid_v4_distinct：UUID 形态且连续生成不相等", () => {
    const a = newActionId();
    const b = newActionId();
    expect(a).toMatch(/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
    expect(b).toMatch(/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
    expect(a).not.toBe(b);
  });
});

describe("codexIpcApi：subscribeCodexThread（SSE 订阅）", () => {
  beforeEach(() => {
    MockEventSource.reset();
    vi.useFakeTimers();
    vi.stubGlobal("EventSource", MockEventSource);
  });

  it("首帧 snapshot 与后续 state 均转 onState；URL 带 session_id", () => {
    installFetch();
    const onState = vi.fn();
    const onUnavailable = vi.fn();
    const stop = subscribeCodexThread("s1", onState, onUnavailable);
    const es = MockEventSource.latest();
    expect(es.url).toBe(`${CODEX_THREAD_EVENTS_PATH}?session_id=s1`);
    es.emit("snapshot", projection({ revision: 1 }));
    es.emit("state", projection({ revision: 2 }));
    expect(onState).toHaveBeenCalledTimes(2);
    expect(onState.mock.calls[1][0].revision).toBe(2);
    expect(onUnavailable).not.toHaveBeenCalled();
    stop();
  });

  it(`连续 ${CODEX_SSE_DEGRADE_AFTER} 次失败降级：onUnavailable 一次且不再建连；成功帧清零计数`, async () => {
    installFetch();
    const onUnavailable = vi.fn();
    const stop = subscribeCodexThread("s1", vi.fn(), onUnavailable);
    MockEventSource.latest().fail(); // 第 1 次
    await vi.advanceTimersByTimeAsync(CODEX_SSE_RECONNECT_BASE_MS);
    MockEventSource.latest().fail(); // 第 2 次
    await vi.advanceTimersByTimeAsync(CODEX_SSE_RECONNECT_BASE_MS * 2);
    // 成功帧清零失败计数（网络抖动不累积）
    MockEventSource.latest().emit("snapshot", projection({ revision: 3 }));
    MockEventSource.latest().fail(); // 又是第 1 次
    await vi.advanceTimersByTimeAsync(CODEX_SSE_RECONNECT_BASE_MS);
    MockEventSource.latest().fail(); // 第 2 次
    await vi.advanceTimersByTimeAsync(CODEX_SSE_RECONNECT_BASE_MS * 2);
    MockEventSource.latest().fail(); // 第 3 次 → 达阈值
    expect(onUnavailable).toHaveBeenCalledTimes(1);
    const created = MockEventSource.instances.length;
    await vi.advanceTimersByTimeAsync(CODEX_SSE_RECONNECT_BASE_MS * 20);
    expect(MockEventSource.instances).toHaveLength(created); // 降级是终态（轮询由 hook 接管）
    expect(() => stop()).not.toThrow();
  });

  it("停止函数关流：close 后投帧不再回调", () => {
    installFetch();
    const onState = vi.fn();
    const stop = subscribeCodexThread("s1", onState, vi.fn());
    const es = MockEventSource.latest();
    stop();
    expect(es.closed).toBe(true);
    es.emit("snapshot", projection());
    expect(onState).not.toHaveBeenCalled();
  });
});
