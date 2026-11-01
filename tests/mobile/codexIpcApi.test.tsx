import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  CODEX_SSE_DEGRADE_AFTER,
  CODEX_SSE_RECONNECT_BASE_MS,
  CODEX_THREAD_EVENTS_PATH,
  CODEX_THREAD_MESSAGES_PATH,
  CODEX_THREAD_PATH,
  answerCodexAsync,
  fetchCodexAction,
  fetchCodexAdaptStatus,
  updateCodexAdaptSettings,
  fetchCodexMessagesPage,
  fetchCodexQueue,
  fetchCodexThread,
  newActionId,
  reconcileCodexQueuedReceipt,
  respondCodexPending,
  retractCodexAction,
  editCodexQueueMessage,
  subscribeCodexThread,
  type CapabilitySet,
  type CodexActionView,
  type CodexQueuedReceipt,
  type CodexQueueView,
  type MessagesPage,
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
    interrupt: false,
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
    contentState: "ready",
    messages: [
      {
        id: "m-1",
        role: "user",
        kind: "user_input",
        text: "你好",
        ts: 1000,
        turnId: null,
        artifacts: [],
        turnImageCount: 0,
        omittedArtifactCount: 0,
      },
    ],
    truncated: false,
    // 服务端新载荷恒序列化 historyToken（旧载荷缺省由归一化补空串）
    historyToken: "",
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
  /** Task 5：同代只读历史页（未设时按 end 兜底） */
  messages?: MessagesPage;
  messagesStatus?: number;
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
    // Task 5：页端点排在投影端点之前（/codex/thread/messages ⊃ /codex/thread）
    if (url.includes(CODEX_THREAD_MESSAGES_PATH)) {
      if (routes.messagesStatus) return new Response("no", { status: routes.messagesStatus });
      return new Response(
        JSON.stringify(
          routes.messages ?? {
            messages: [],
            historyToken: "",
            revision: 7,
            nextBefore: null,
            status: "end",
            reasonCode: null,
          }
        ),
        { status: 200 }
      );
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

  it("fetch_codex_thread_malformed_content_state_throws：缺 contentState / 枚举外取值 → 残缺载荷报错（不渲染假界面）", async () => {
    installFetch();
    // 旧后端/残缺载荷：服务端恒序列化 contentState——缺字段即残缺（2026-10-19 复审）
    routes.thread = { ...projection() } as unknown as ThreadProjection;
    delete (routes.thread as Record<string, unknown>).contentState;
    await expect(fetchCodexThread("s1")).rejects.toBeInstanceOf(ApiError);

    // 枚举外取值（loading/ready/partial/unsupported 四态之外）同样拒绝
    routes.thread = projection({
      contentState: "ok" as unknown as ThreadProjection["contentState"],
    });
    await expect(fetchCodexThread("s1")).rejects.toThrow("malformed projection payload");

    // 四态之内照常透传（守卫只做形状校验，不做语义改写）
    for (const cs of ["loading", "ready", "partial", "unsupported"] as const) {
      routes.thread = projection({ contentState: cs });
      await expect(fetchCodexThread("s1")).resolves.toMatchObject({ contentState: cs });
    }
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

  it("残缺帧跳过：合法 JSON 但缺 contentState 的帧不进 onState（与 GET 同形状守卫），后续好帧照常透传", () => {
    installFetch();
    const onState = vi.fn();
    const onUnavailable = vi.fn();
    const stop = subscribeCodexThread("s1", onState, onUnavailable);
    const es = MockEventSource.latest();
    // 合法 JSON 但残缺（缺 contentState——2026-10-19 复审 Minor：SSE 面同受形状
    // 守卫覆盖，残缺 DTO 不直入 onState 渲染假界面）
    es.emit(
      "snapshot",
      JSON.stringify({ availability: "controllable", messages: [], pending: [] })
    );
    // contentState 枚举外取值同样按残缺跳过
    es.emit("state", JSON.stringify({ ...projection(), contentState: "bogus" }));
    // 非 JSON 帧（代理注入页）照旧跳过
    es.emit("state", "<html>proxy page</html>");
    expect(onState).not.toHaveBeenCalled();
    // 好帧照常透传，流未降级（坏帧跳过语义不变）
    es.emit("state", projection({ revision: 9 }));
    expect(onState).toHaveBeenCalledTimes(1);
    expect(onState.mock.calls[0][0].revision).toBe(9);
    expect(onUnavailable).not.toHaveBeenCalled();
    stop();
  });

  it(`残缺帧不清零失败计数：${CODEX_SSE_DEGRADE_AFTER - 1} 次失败后投坏帧再失败一次即达降级阈值`, async () => {
    installFetch();
    const onUnavailable = vi.fn();
    const stop = subscribeCodexThread("s1", vi.fn(), onUnavailable);
    MockEventSource.latest().fail(); // 第 1 次
    await vi.advanceTimersByTimeAsync(CODEX_SSE_RECONNECT_BASE_MS);
    MockEventSource.latest().fail(); // 第 2 次
    await vi.advanceTimersByTimeAsync(CODEX_SSE_RECONNECT_BASE_MS * 2);
    // 残缺帧：字节到了但解析不出完整投影——不清零（能收字节 ≠ 链路健康）
    MockEventSource.latest().emit("snapshot", '{"availability":"controllable"}');
    MockEventSource.latest().fail(); // 第 3 次 → 达阈值
    expect(onUnavailable).toHaveBeenCalledTimes(1);
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

it("旧消息载荷补齐附件默认值", async () => {
  installFetch();
  // 旧后端载荷：没有 turnId / artifacts / 计数（模拟升级前的投影）。
  routes.thread = projection({
    messages: [{ id: "m-1", role: "user", kind: "user_input", text: "你好", ts: 1000 }],
  });
  const p = await fetchCodexThread("s");
  expect(p?.messages[0]).toMatchObject({
    turnId: null,
    artifacts: [],
    turnImageCount: 0,
    omittedArtifactCount: 0,
  });
});

it("新附件字段按 Rust DTO 原样透出（camelCase，不重排字段）", async () => {
  installFetch();
  const artifact = {
    artifactId: "t-1/artifact/0",
    kind: "image",
    source: "image_generation",
    path: "/project/generated.png",
    url: null,
    fileKey: "/project/generated.png",
    ordinal: 1,
    imageIndexInTurn: 1,
    summary: "Synthetic image description",
    sourceStatus: "synthetic-complete",
    changeKind: null,
    unavailableReason: null,
  } as const;
  routes.thread = projection({
    messages: [
      {
        id: "m-gen",
        role: "system",
        kind: "imageGeneration",
        text: "",
        ts: null,
        turnId: "t-1",
        artifacts: [{ ...artifact }],
        turnImageCount: 3,
        omittedArtifactCount: 2,
      },
    ],
  });
  const p = await fetchCodexThread("s");
  expect(p?.messages[0]).toMatchObject({
    turnId: "t-1",
    artifacts: [artifact],
    turnImageCount: 3,
    omittedArtifactCount: 2,
  });
});

// ==== Task 5：线程 historyToken + 同代只读消息页 ====

function messagesPage(over: Partial<MessagesPage> = {}): MessagesPage {
  return {
    messages: [],
    historyToken: "",
    revision: 7,
    nextBefore: null,
    status: "end",
    reasonCode: null,
    ...over,
  };
}

/** 旧投影消息（无 Task 3/4 新字段），用于旧载荷降级用例 */
function legacyMessage(id: string) {
  return { id, role: "user", kind: "user_input", text: id, ts: 1000 };
}

describe("codexIpcApi：T03 支持状态/能力身份/活动回合 增量字段", () => {
  it("t03_support_capability_identity_active_turn_parsed：新字段逐字段透出", async () => {
    installFetch();
    routes.thread = projection({
      activeTurnId: "turn-9",
      effectiveOperations: {
        policyRevision: 7,
        capabilityIdentity: "cap-id-1",
        operations: {
          async_answer_idle: {
            implemented: true,
            allowed: false,
            requiresAutoAdapt: true,
            evidence: "contract_matched",
            support: "supported",
            reasonCode: "operation_runtime_active",
          },
        },
      },
    } as unknown as ThreadProjection);
    const p = await fetchCodexThread("s1");
    expect(p?.activeTurnId).toBe("turn-9");
    expect(p?.effectiveOperations?.capabilityIdentity).toBe("cap-id-1");
    const status = p?.effectiveOperations?.operations.async_answer_idle;
    expect(status?.support).toBe("supported");
    // 资格字段原样保留：support 只是并列输出，不改 allowed/reasonCode。
    expect(status?.allowed).toBe(false);
    expect(status?.reasonCode).toBe("operation_runtime_active");
  });

  it("t03_legacy_payload_support_derived_display_only：旧载荷缺新字段不报错；support 按 evidence 推导（仅展示）；缺字段不产生新写资格", async () => {
    installFetch();
    routes.thread = projection({
      effectiveOperations: {
        policyRevision: 7,
        operations: {
          async_answer_idle: {
            implemented: true,
            allowed: false,
            requiresAutoAdapt: true,
            evidence: "contract_matched",
            reasonCode: "operation_runtime_active",
          },
          blocking_input: {
            implemented: true,
            allowed: false,
            requiresAutoAdapt: true,
            evidence: "none",
            reasonCode: "operation_no_evidence",
          },
        },
      },
    } as unknown as ThreadProjection);
    const p = await fetchCodexThread("s1");
    // 旧载荷无 activeTurnId / capabilityIdentity → undefined（不崩、不猜）。
    expect(p?.activeTurnId).toBeUndefined();
    expect(p?.effectiveOperations?.capabilityIdentity).toBeUndefined();
    // support 缺省 → 按 evidence 推导（展示值）：有证据即 supported。
    expect(p?.effectiveOperations?.operations.async_answer_idle?.support).toBe("supported");
    expect(p?.effectiveOperations?.operations.blocking_input?.support).toBe("identifying");
    // 推导只改展示：allowed=false 原样保留，不得因"推导出 supported"而放行。
    expect(p?.effectiveOperations?.operations.async_answer_idle?.allowed).toBe(false);
  });

  it("r1_support_inherited_parsed：展示沿用标记透出（仅 true；缺省/脏值 → undefined）", async () => {
    installFetch();
    routes.thread = projection({
      effectiveOperations: {
        policyRevision: 8,
        capabilityIdentity: "cap-1",
        operations: {
          async_answer_idle: {
            implemented: true,
            allowed: false,
            requiresAutoAdapt: true,
            evidence: "none",
            support: "supported",
            supportInherited: true,
            reasonCode: "operation_target_stale",
          },
          blocking_input: {
            implemented: true,
            allowed: false,
            requiresAutoAdapt: true,
            evidence: "none",
            support: "identifying",
            supportInherited: "yes",
            reasonCode: "operation_no_evidence",
          },
        },
      },
    } as unknown as ThreadProjection);
    const p = await fetchCodexThread("s1");
    expect(p?.effectiveOperations?.operations.async_answer_idle?.supportInherited).toBe(true);
    // 脏值不透出（严格 true 才认）。
    expect(p?.effectiveOperations?.operations.blocking_input?.supportInherited).toBeUndefined();
    // 展示标记不授权：allowed 原样 false。
    expect(p?.effectiveOperations?.operations.async_answer_idle?.allowed).toBe(false);
  });

  it("c03_support_identity_verified_parsed：包身份已核对标记透出（仅 true；缺省/脏值 → undefined=待核对）", async () => {
    installFetch();
    routes.thread = projection({
      effectiveOperations: {
        policyRevision: 9,
        capabilityIdentity: "cap-1",
        operations: {
          async_answer_idle: {
            implemented: true,
            allowed: false,
            requiresAutoAdapt: true,
            evidence: "none",
            support: "supported",
            supportInherited: true,
            supportIdentityVerified: true,
            reasonCode: "operation_target_stale",
          },
          blocking_input: {
            implemented: true,
            allowed: false,
            requiresAutoAdapt: true,
            evidence: "none",
            support: "supported",
            supportInherited: true,
            supportIdentityVerified: "yes",
            reasonCode: "operation_no_evidence",
          },
        },
      },
    } as unknown as ThreadProjection);
    const p = await fetchCodexThread("s1");
    expect(p?.effectiveOperations?.operations.async_answer_idle?.supportIdentityVerified).toBe(
      true
    );
    // 脏值不透出（严格 true 才认）→ 前端按「待核对」展示。
    expect(
      p?.effectiveOperations?.operations.blocking_input?.supportIdentityVerified
    ).toBeUndefined();
    expect(p?.effectiveOperations?.operations.blocking_input?.allowed).toBe(false);
  });

  it("t03_dirty_support_and_turn_id_degrade：脏 support / 脏 activeTurnId 不透传", async () => {
    installFetch();
    routes.thread = projection({
      activeTurnId: 42,
      effectiveOperations: {
        policyRevision: 7,
        capabilityIdentity: 99,
        operations: {
          send: {
            implemented: true,
            allowed: true,
            requiresAutoAdapt: false,
            evidence: "shipped",
            support: "supported_troll",
            reasonCode: null,
          },
        },
      },
    } as unknown as ThreadProjection);
    const p = await fetchCodexThread("s1");
    expect(p?.activeTurnId).toBeUndefined();
    expect(p?.effectiveOperations?.capabilityIdentity).toBeUndefined();
    // 枚举外 support 词 → 按 evidence 推导兜底（shipped → supported）。
    expect(p?.effectiveOperations?.operations.send?.support).toBe("supported");
    expect(p?.effectiveOperations?.operations.send?.allowed).toBe(true);
  });
});

describe("codexIpcApi：projectionVersion 增量版本束（T02）", () => {
  it("projection_version_parsed_when_present：版本束逐字段透出（camelCase，与 Rust DTO 逐字段对齐）", async () => {
    installFetch();
    routes.thread = projection({
      projectionVersion: {
        instanceId: "boot-1",
        ownerEpoch: 2,
        ownerGeneration: 3,
        revision: 50,
        localSeq: 7,
      },
    });
    const p = await fetchCodexThread("s1");
    expect(p?.projectionVersion).toEqual({
      instanceId: "boot-1",
      ownerEpoch: 2,
      ownerGeneration: 3,
      revision: 50,
      localSeq: 7,
    });
  });

  it("projection_version_absent_or_dirty_degrades：旧载荷缺字段不报错（undefined）；脏形状不透传（缺版本不获新写资格）", async () => {
    installFetch();
    // 旧后端：无该字段 → undefined，不报错（按旧载荷退化）
    routes.thread = projection();
    expect((await fetchCodexThread("s1"))?.projectionVersion).toBeUndefined();
    // 脏形状（子字段缺失/错型）：归一化丢弃，不得把半截版本束当真值
    routes.thread = projection({
      projectionVersion: {
        instanceId: 7,
        ownerEpoch: "x",
      } as unknown as ThreadProjection["projectionVersion"],
    });
    expect((await fetchCodexThread("s1"))?.projectionVersion).toBeUndefined();
    routes.thread = projection({
      projectionVersion: {
        instanceId: "boot-2",
        ownerEpoch: 1,
        // ownerGeneration 缺失 → 脏
        revision: null,
        localSeq: 0,
      } as unknown as ThreadProjection["projectionVersion"],
    });
    expect((await fetchCodexThread("s1"))?.projectionVersion).toBeUndefined();
    // revision=null 合法（无 Desktop revision 的事件按 localSeq 竞争）
    routes.thread = projection({
      projectionVersion: {
        instanceId: "boot-2",
        ownerEpoch: 1,
        ownerGeneration: 1,
        revision: null,
        localSeq: 0,
      },
    });
    expect((await fetchCodexThread("s1"))?.projectionVersion).toEqual({
      instanceId: "boot-2",
      ownerEpoch: 1,
      ownerGeneration: 1,
      revision: null,
      localSeq: 0,
    });
  });

  it("sse_frames_share_projection_version_normalization：SSE 首帧/增量帧同一归一化", () => {
    MockEventSource.reset();
    vi.useFakeTimers();
    vi.stubGlobal("EventSource", MockEventSource);
    installFetch();
    const onState = vi.fn();
    const onUnavailable = vi.fn();
    const stop = subscribeCodexThread("s1", onState, onUnavailable);
    MockEventSource.latest().emit(
      "snapshot",
      JSON.stringify({
        ...projection(),
        projectionVersion: {
          instanceId: "boot-1",
          ownerEpoch: 1,
          ownerGeneration: 1,
          revision: 12,
          localSeq: 30,
        },
      })
    );
    expect(onState).toHaveBeenCalledTimes(1);
    expect(onState.mock.calls[0][0].projectionVersion).toEqual({
      instanceId: "boot-1",
      ownerEpoch: 1,
      ownerGeneration: 1,
      revision: 12,
      localSeq: 30,
    });
    // 脏版本束的帧照常落地（版本束归一化为 undefined），不炸流
    MockEventSource.latest().emit(
      "state",
      JSON.stringify({ ...projection(), projectionVersion: { instanceId: null } })
    );
    expect(onState).toHaveBeenCalledTimes(2);
    expect(onState.mock.calls[1][0].projectionVersion).toBeUndefined();
    stop();
  });
});

describe("codexIpcApi：historyToken 归一化（Task 5）", () => {
  it("thread_history_token_legacy_defaults_empty：旧载荷缺 historyToken → 空串（不得据此分页）", async () => {
    installFetch();
    routes.thread = projection({ messages: [legacyMessage("m-1")] });
    // 模拟升级前载荷：连 historyToken 键都没有
    delete (routes.thread as Record<string, unknown>).historyToken;
    const p = await fetchCodexThread("s1");
    expect(p?.historyToken).toBe("");
    // 消息兼容字段同样补齐（首帧/GET/轮询同一归一化入口）
    expect(p?.messages[0]).toMatchObject({
      turnId: null,
      artifacts: [],
      turnImageCount: 0,
      omittedArtifactCount: 0,
    });
  });

  it("thread_history_token_passthrough：新载荷原样透出", async () => {
    installFetch();
    routes.thread = projection({ historyToken: "v1-tag-1-2-3-deadbeef" });
    const p = await fetchCodexThread("s1");
    expect(p?.historyToken).toBe("v1-tag-1-2-3-deadbeef");
  });

  it("sse_frames_share_history_token_normalization：首帧/增量帧同一归一化（缺省空串、新值透传）", () => {
    MockEventSource.reset();
    vi.useFakeTimers();
    vi.stubGlobal("EventSource", MockEventSource);
    installFetch();
    const onState = vi.fn();
    const stop = subscribeCodexThread("s1", onState, vi.fn());
    const es = MockEventSource.latest();
    // 旧帧：JSON 里没有 historyToken
    es.emit("snapshot", JSON.stringify({ ...projection(), historyToken: undefined }));
    expect(onState.mock.calls[0][0].historyToken).toBe("");
    es.emit("state", projection({ historyToken: "v1-new" }));
    expect(onState.mock.calls[1][0].historyToken).toBe("v1-new");
    expect(onState).toHaveBeenCalledTimes(2);
    stop();
  });
});

describe("codexIpcApi：fetchCodexMessagesPage（同代只读历史页）", () => {
  it("messages_page_query_contract：session_id/history_token/before_id/limit 与 AbortSignal 原样上送", async () => {
    installFetch();
    const ctrl = new AbortController();
    routes.messages = messagesPage({
      status: "ready",
      messages: [legacyMessage("m-1")],
      historyToken: "v1-next",
      nextBefore: "m-1",
    });
    const p = await fetchCodexMessagesPage({
      sessionId: "s1",
      historyToken: "v1-curr",
      beforeId: "m-2",
      limit: 20,
      signal: ctrl.signal,
    });
    expect(p.status).toBe("ready");
    expect(p.historyToken).toBe("v1-next");
    expect(p.nextBefore).toBe("m-1");
    const call = fetchMock.mock.calls.find(([u]) => String(u).includes(CODEX_THREAD_MESSAGES_PATH));
    expect(call).toBeTruthy();
    const u = new URL(String(call![0]), "http://localhost");
    expect(u.pathname).toBe(CODEX_THREAD_MESSAGES_PATH);
    expect(u.searchParams.get("session_id")).toBe("s1");
    expect(u.searchParams.get("history_token")).toBe("v1-curr");
    expect(u.searchParams.get("before_id")).toBe("m-2");
    expect(u.searchParams.get("limit")).toBe("20");
    expect((call![1] as RequestInit).signal).toBe(ctrl.signal);

    // before_id / limit 缺省时不上送（服务端按「末端锚点 / 默认 50」处理）
    await fetchCodexMessagesPage({ sessionId: "s1", historyToken: "v1-curr" });
    const u2 = new URL(String(fetchMock.mock.calls[1][0]), "http://localhost");
    expect(u2.searchParams.has("before_id")).toBe(false);
    expect(u2.searchParams.has("limit")).toBe(false);
  });

  it("messages_page_normalizes_legacy_messages：页内旧消息补默认值，缺省字段安全降级", async () => {
    installFetch();
    routes.messages = {
      messages: [legacyMessage("m-1")],
      historyToken: "v1-next",
      revision: 3,
      nextBefore: "m-1",
      status: "ready",
      reasonCode: null,
    } as unknown as MessagesPage;
    const p = await fetchCodexMessagesPage({ sessionId: "s1", historyToken: "v1-curr" });
    expect(p.messages[0]).toMatchObject({
      id: "m-1",
      turnId: null,
      artifacts: [],
      turnImageCount: 0,
      omittedArtifactCount: 0,
    });

    // 旧/残缺页载荷缺 historyToken / revision / nextBefore / reasonCode → 安全默认
    routes.messages = { messages: [], status: "end" } as unknown as MessagesPage;
    await expect(
      fetchCodexMessagesPage({ sessionId: "s1", historyToken: "v1-curr" })
    ).resolves.toEqual({
      messages: [],
      historyToken: "",
      revision: null,
      nextBefore: null,
      status: "end",
      reasonCode: null,
    });
  });

  it("messages_page_non_ready_has_no_messages：非正常状态不得带出半页消息（不伪装完整空页）", async () => {
    installFetch();
    routes.messages = messagesPage({
      status: "cursor_invalid",
      reasonCode: "history_rewritten",
      messages: [legacyMessage("m-1")],
    });
    const p = await fetchCodexMessagesPage({ sessionId: "s1", historyToken: "v1-curr" });
    expect(p.status).toBe("cursor_invalid");
    expect(p.reasonCode).toBe("history_rewritten");
    expect(p.messages).toEqual([]);
    expect(p.nextBefore).toBeNull();
  });

  it("messages_page_malformed_payload_rejected：缺 status / 枚举外 status / 缺 messages → ApiError", async () => {
    installFetch();
    for (const bad of [{ messages: [] }, { messages: [], status: "ok" }, { status: "ready" }]) {
      routes.messages = bad as unknown as MessagesPage;
      await expect(
        fetchCodexMessagesPage({ sessionId: "s1", historyToken: "v1-curr" })
      ).rejects.toThrow("malformed messages page payload");
    }
  });

  it("messages_page_http_error_throws：400（非法 limit/缺 token）等非 2xx 抛 ApiError", async () => {
    installFetch();
    routes.messagesStatus = 400;
    const err = await fetchCodexMessagesPage({
      sessionId: "s1",
      historyToken: "v1-curr",
      limit: 51,
    }).catch((e: unknown) => e);
    expect(err).toBeInstanceOf(ApiError);
    expect((err as ApiError).status).toBe(400);
  });
});

// ==== reconcileCodexQueuedReceipt（2026-10-22 B1：codexQueued 回执收敛）====

describe("reconcileCodexQueuedReceipt", () => {
  const prev: CodexQueuedReceipt = { kind: "codexQueued", position: 2, actionId: "act-1" };
  const view = (actions: CodexQueueView["actions"]): CodexQueueView => ({
    actions,
    blockedReasonCode: "native_turn_pending",
    canInterrupt: true,
  });
  const queued = (actionId: string, position: number | null): CodexActionView => ({
    actionId,
    state: "queued",
    position,
    receipt: null,
  });

  it("action_gone_converges_to_null：动作已离队（派发/撤回/他端清空）→ null，chip 收敛清除", () => {
    expect(reconcileCodexQueuedReceipt(prev, view([]))).toBeNull();
    expect(reconcileCodexQueuedReceipt(prev, view([queued("other", 1)]))).toBeNull();
  });

  it("position_follows_and_stable：位次变化 → 跟随；位次不变 / null → 原引用（不触发无谓重渲）", () => {
    expect(reconcileCodexQueuedReceipt(prev, view([queued("act-1", 1)]))).toEqual({
      kind: "codexQueued",
      position: 1,
      actionId: "act-1",
    });
    const same = view([queued("act-1", 2)]);
    expect(reconcileCodexQueuedReceipt(prev, same)).toBe(prev);
    const nullPos = view([queued("act-1", null)]);
    expect(reconcileCodexQueuedReceipt(prev, nullPos)).toBe(prev);
  });
});

// ============================================================
// T08：移动端能力详情/开关/正式答题的**不可信载荷**处理
// ============================================================

describe("codexIpcApi：T08 能力详情与异步正式答题", () => {
  function stubAdapt(payloads: {
    thread?: unknown;
    status?: unknown;
    settings?: unknown;
    answer?: unknown;
  }) {
    fetchMock = vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      const body = (value: unknown) =>
        new Response(JSON.stringify(value), {
          status: 200,
          headers: { "content-type": "application/json" },
        });
      if (url.includes("/codex/adapt-status")) return body(payloads.status);
      if (url.includes("/codex/adapt-settings")) return body(payloads.settings);
      if (url.includes("/codex/async-answer")) return body(payloads.answer);
      if (url.includes("/codex/thread")) return body(payloads.thread);
      return new Response("no", { status: 404 });
    });
    vi.stubGlobal("fetch", fetchMock);
  }

  it("T08 missing operations is unknown, not permission granted", async () => {
    stubAdapt({
      thread: {
        threadId: "th-1",
        revision: 1,
        stale: false,
        availability: "controllable",
        capabilities: caps(),
        runtimeStatus: "idle",
        contentState: "ready",
        messages: [],
        truncated: false,
        historyToken: "",
        pending: [],
      },
    });
    const parsed = await fetchCodexThread("s1");
    // 缺字段 = 未知；绝不能从 availability/capabilities 推导"全部可用"。
    expect(parsed?.effectiveOperations).toBeUndefined();
    expect(parsed?.asyncQuestions).toBeUndefined();
  });

  it("T08 unknown operation keys are ignored and dirty statuses are not grants", async () => {
    stubAdapt({
      thread: {
        ...projection(),
        effectiveOperations: {
          policyRevision: 7,
          operations: {
            blocking_input: {
              implemented: true,
              allowed: true,
              requiresAutoAdapt: false,
              evidence: "contract_matched",
              reasonCode: null,
            },
            brand_new_operation: {
              implemented: true,
              allowed: true,
              requiresAutoAdapt: false,
              evidence: "shipped",
              reasonCode: null,
            },
            send: { implemented: "yes" },
          },
        },
        asyncQuestions: [
          {
            questionKey: '["request_user_input_async","call-a",0]',
            contextToken: "ctx:1",
            sourceTurnId: "t1",
            sourceItemId: "call-a",
            questionIndex: 0,
            title: "选一个",
            options: ["甲"],
            state: "pending",
            actionable: true,
            disabledReasonCode: null,
          },
          { questionKey: "broken" },
        ],
      },
    });
    const parsed = await fetchCodexThread("s1");
    expect(parsed?.effectiveOperations?.operations.blocking_input?.allowed).toBe(true);
    expect(
      (parsed?.effectiveOperations?.operations as Record<string, unknown>).brand_new_operation
    ).toBeUndefined();
    expect(parsed?.effectiveOperations?.operations.send).toBeUndefined();
    expect(parsed?.asyncQuestions).toHaveLength(1);
    expect(parsed?.asyncQuestions?.[0].state).toBe("pending");
  });

  it("T08 async answer body is strict (no raw/owner/method)", async () => {
    stubAdapt({ answer: receipt("accepted") });
    const result = await answerCodexAsync({
      sessionId: "s1",
      actionId: "act-1",
      questionKey: '["request_user_input_async","call-a",0]',
      contextToken: "ctx:1",
      answer: "甲",
    });
    expect(result.delivery).toBe("accepted");
    const call = fetchMock.mock.calls.find(([u]) => String(u).includes("/codex/async-answer"));
    const body = JSON.parse(String((call![1] as RequestInit).body)) as Record<string, unknown>;
    expect(Object.keys(body).sort()).toEqual([
      "actionId",
      "answer",
      "contextToken",
      "questionKey",
      "sessionId",
    ]);
  });

  it("T08 adapt details keep operations optional and never invent them", async () => {
    stubAdapt({
      status: {
        sessionId: "s1",
        systemVersion: "macos 27",
        tuvisVersion: "0.6.1",
        desktopBuild: "26.1021.31345",
        autoAdaptEnabled: true,
        hintSeen: false,
        lastFailure: null,
      },
      settings: {
        sessionId: "s1",
        systemVersion: "macos 27",
        tuvisVersion: "0.6.1",
        desktopBuild: "26.1021.31345",
        autoAdaptEnabled: false,
        hintSeen: true,
        effectiveOperations: {
          policyRevision: 9,
          operations: {
            async_answer_idle: {
              implemented: true,
              allowed: false,
              requiresAutoAdapt: true,
              evidence: "contract_matched",
              reasonCode: "operation_runtime_active",
            },
          },
        },
      },
    });
    const details = await fetchCodexAdaptStatus("s1");
    expect(details.effectiveOperations).toBeUndefined();
    const updated = await updateCodexAdaptSettings({
      sessionId: "s1",
      action: "set_enabled",
      enabled: false,
    });
    expect(updated.autoAdaptEnabled).toBe(false);
    expect(updated.effectiveOperations?.operations.async_answer_idle?.allowed).toBe(false);
  });
});

it("queue edit posts session and action, and never returns text on a failed CAS", async () => {
  const fetcher = vi
    .fn()
    .mockResolvedValueOnce(
      new Response(JSON.stringify({ actionId: "a1", text: "original" }), { status: 200 })
    )
    .mockResolvedValueOnce(
      new Response(JSON.stringify({ error: "already_dispatched" }), { status: 409 })
    );
  vi.stubGlobal("fetch", fetcher);
  await expect(editCodexQueueMessage("s1", "a1")).resolves.toEqual({
    actionId: "a1",
    text: "original",
  });
  expect(fetcher).toHaveBeenCalledWith(
    "/m/api/v1/codex/queue/edit",
    expect.objectContaining({
      method: "POST",
      body: JSON.stringify({ sessionId: "s1", actionId: "a1" }),
    })
  );
  await expect(editCodexQueueMessage("s1", "a1")).rejects.toMatchObject({ status: 409 });
});
