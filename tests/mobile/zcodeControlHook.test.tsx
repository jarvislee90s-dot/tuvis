// T7.2 Slice A2a/A2b：ZCode 统一控制状态 hook 行为锁（全合成夹具）。
//
// 锁四件事：
// 1. 连接态唯一来自 GET /zcode/connection——已保存链接/可读会话绝不等于已连接，
//    客户端在确认前只显示「未知」；
// 2. accepted 与 unknown 的状态/原因/草稿语义严格分离，unknown 绝不自动重发；
// 3. 每次提交生成一个 actionId（crypto.randomUUID）：请求与回执同一个，
//    重试是新提交 → 新 actionId；
// 4. send_failed / rejected / 未知 / 网络失败一律保留草稿；只有 send 的 accepted
//    允许清草稿。
import { act, cleanup, renderHook, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import useZcodeControl from "@/mobile/useZcodeControl";
import {
  ZCODE_ACTIONS_PATH,
  ZCODE_CONNECTION_PATH,
  ZCODE_SESSION_PATH,
  ZcodeControlApiError,
  zcodeEventsUrl,
  type ActionReceipt,
  type CapabilityView,
  type ConnectionView,
  type ControlAction,
  type PendingView,
  type QueueView,
  type SessionView,
} from "@/lib/api/zcodeControl";
import { MockEventSource } from "./eventSourceMock";

const SID = "session_0000000000000000000000000000000a";

const flag = (enabled: boolean) => ({ enabled, reasonCode: enabled ? null : "needs_evidence" });

const capabilities = (): CapabilityView => ({
  send: flag(true),
  singleSelect: flag(true),
  multiSelect: flag(false),
  freeText: flag(false),
  interrupt: flag(true),
  approvalReply: flag(false),
  model: flag(true),
  thoughtLevel: flag(true),
});

/** Hub 降级后事件沿用的全关能力矩阵（reasonCode = needs_evidence）。 */
const closedCapabilities = (): CapabilityView => ({
  send: flag(false),
  singleSelect: flag(false),
  multiSelect: flag(false),
  freeText: flag(false),
  interrupt: flag(false),
  approvalReply: flag(false),
  model: flag(false),
  thoughtLevel: flag(false),
});

function connection(overrides: Partial<ConnectionView> = {}): ConnectionView {
  return {
    state: "paired",
    generation: 3,
    reasonCode: null,
    deviceLabel: "Synthetic Mac",
    ...overrides,
  };
}

function sessionView(overrides: Partial<SessionView> = {}): SessionView {
  return {
    sessionId: SID,
    generation: 3,
    revision: 12,
    ready: true,
    pending: [],
    control: { activeWorks: [] },
    selection: null,
    capabilities: capabilities(),
    ...overrides,
  };
}

const sendAction: ControlAction = {
  kind: "send",
  sessionId: SID,
  generation: 3,
  text: "待发送草稿",
};

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

let fetchMock: ReturnType<typeof vi.fn>;

function installFetch(handler: (url: string, init?: RequestInit) => Response | Promise<Response>) {
  fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) =>
    handler(String(input), init)
  );
  vi.stubGlobal("fetch", fetchMock);
}

function calledUrls(): string[] {
  return fetchMock.mock.calls.map(([input]) => String(input));
}

function postedActions(): { actionId: string; action: ControlAction }[] {
  return fetchMock.mock.calls
    .filter(([input]) => String(input) === ZCODE_ACTIONS_PATH)
    .map(
      ([, init]) =>
        JSON.parse(String((init as RequestInit).body)) as {
          actionId: string;
          action: ControlAction;
        }
    );
}

async function flush() {
  for (let i = 0; i < 8; i += 1) {
    await act(async () => {
      await Promise.resolve();
    });
  }
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((res) => {
    resolve = res;
  });
  return { promise, resolve };
}

beforeEach(() => {
  MockEventSource.reset();
  vi.stubGlobal("EventSource", MockEventSource);
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  MockEventSource.reset();
});

describe("useZcodeControl：连接态唯一来自服务端 GET", () => {
  it("saved_never_implies_connected", async () => {
    const gate = deferred<Response>();
    installFetch((url) => {
      if (url === ZCODE_CONNECTION_PATH) return gate.promise;
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(sessionView());
      throw new Error(`unexpected fetch: ${url}`);
    });

    const { result } = renderHook(() => useZcodeControl(SID));

    // 首拍：连接未知。可读会话（阅读链）不代表已连接；hook 不虚构 paired。
    expect(result.current.connection).toBeNull();
    expect(result.current.loading).toBe(true);

    await flush();
    // 会话投影已读到（阅读不受控制态拖累），连接仍保持未知
    expect(result.current.session?.sessionId).toBe(SID);
    expect(result.current.connection).toBeNull();

    // 服务端明确「已保存链接但未授权」：state=needs_auth，绝不是 paired
    gate.resolve(json(connection({ state: "needs_auth", reasonCode: null })));
    await flush();

    expect(result.current.connection?.state).toBe("needs_auth");
    expect(result.current.connection?.state).not.toBe("paired");
    expect(result.current.reason).toBe("needs_auth");
    expect(result.current.loading).toBe(false);
    // 保存链接不是客户端输入：加载阶段零写；SSE 只订阅本会话事件流
    expect(postedActions()).toHaveLength(0);
    expect(MockEventSource.latest().url).toBe(zcodeEventsUrl(SID));
  });
});

describe("useZcodeControl：回执语义", () => {
  it("accepted_and_unknown_keep_distinct_copy", async () => {
    let state: ActionReceipt["state"] = "accepted";
    installFetch((url, init) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(sessionView());
      if (url === ZCODE_ACTIONS_PATH && init?.method === "POST") {
        const body = JSON.parse(String(init.body)) as { actionId: string };
        return json({
          actionId: body.actionId,
          sessionId: SID,
          generation: 3,
          state,
          reasonCode: state === "unknown" ? "unknown" : null,
        } satisfies ActionReceipt);
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    const { result } = renderHook(() => useZcodeControl(SID));
    await flush();

    let accepted!: ActionReceipt;
    await act(async () => {
      accepted = await result.current.submit(sendAction);
    });
    // accepted = 宿主已受理（≠ 已生效）；本次草稿可清，原因面干净
    expect(accepted.state).toBe("accepted");
    expect(result.current.draftCleared).toBe(true);
    expect(result.current.reason).toBeNull();

    // 非 send 动作与 composer 草稿无关：accepted 也不改动草稿标记
    await act(async () => {
      await result.current.submit({
        kind: "snooze",
        sessionId: SID,
        generation: 3,
        interactionId: "int-1",
      });
    });
    expect(result.current.draftCleared).toBe(true);

    state = "unknown";
    let unknown!: ActionReceipt;
    await act(async () => {
      unknown = await result.current.submit(sendAction);
    });
    // unknown 与 accepted 是两种文案/两种语义：不得冒称已受理/已生效，草稿保留
    expect(unknown.state).toBe("unknown");
    expect(unknown.state).not.toBe(accepted.state);
    expect(result.current.draftCleared).toBe(false);
    expect(result.current.reason).toBe("unknown");
    // unknown 绝不自动重试：三次提交恰好三次 POST（没有第四次）
    expect(postedActions()).toHaveLength(3);
  });

  it("action_id_is_stable_per_submission_and_new_on_retry", async () => {
    const ids = ["11111111-1111-4111-8111-111111111111", "22222222-2222-4222-8222-222222222222"];
    const uuid = vi.spyOn(globalThis.crypto, "randomUUID");
    for (const id of ids) uuid.mockReturnValueOnce(id);

    installFetch((url, init) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(sessionView());
      if (url === ZCODE_ACTIONS_PATH && init?.method === "POST") {
        const body = JSON.parse(String(init.body)) as { actionId: string };
        return json({
          actionId: body.actionId,
          sessionId: SID,
          generation: 3,
          state: "accepted",
          reasonCode: null,
        } satisfies ActionReceipt);
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    const { result } = renderHook(() => useZcodeControl(SID));
    await flush();

    let first!: ActionReceipt;
    await act(async () => {
      first = await result.current.submit(sendAction);
    });
    let second!: ActionReceipt;
    await act(async () => {
      second = await result.current.submit(sendAction);
    });

    const bodies = postedActions();
    expect(bodies).toHaveLength(2);
    // 每次提交一个 ID：请求体与回执同一个（本次提交内稳定）
    expect(bodies[0].actionId).toBe(ids[0]);
    expect(first.actionId).toBe(ids[0]);
    expect(bodies[1].actionId).toBe(ids[1]);
    expect(second.actionId).toBe(ids[1]);
    // 重试 = 新提交 → 新 actionId（复用旧 ID 会被服务端按同一次动作去重）
    expect(bodies[1].actionId).not.toBe(bodies[0].actionId);
    expect(uuid).toHaveBeenCalledTimes(2);
    // 只走官方 actions 端点：零旧 session-send / 零无头入口
    const urls = calledUrls();
    expect(urls.every((url) => url.startsWith("/m/api/v1/zcode/"))).toBe(true);
    expect(urls.some((url) => /session-send|headless/.test(url))).toBe(false);
  });

  it("action_id_works_without_secure_context", async () => {
    // 手机页由远程端以明文 TCP 伺服（http://<LAN-IP>:<port>/m）：非安全上下文里
    // `crypto.randomUUID` 不存在，直调会让发送/作答/审批/打断全部静默失败。
    // 这里模拟该环境（保留 getRandomValues，去掉 randomUUID）。
    const realCrypto = globalThis.crypto;
    vi.stubGlobal("crypto", {
      getRandomValues: realCrypto.getRandomValues.bind(realCrypto),
    });

    installFetch((url, init) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(sessionView());
      if (url === ZCODE_ACTIONS_PATH && init?.method === "POST") {
        const body = JSON.parse(String(init.body)) as { actionId: string };
        return json({
          actionId: body.actionId,
          sessionId: SID,
          generation: 3,
          state: "accepted",
          reasonCode: null,
        } satisfies ActionReceipt);
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    const { result } = renderHook(() => useZcodeControl(SID));
    await flush();

    let settled!: ActionReceipt;
    await act(async () => {
      settled = await result.current.submit(sendAction);
    });

    const bodies = postedActions();
    expect(bodies).toHaveLength(1); // 缺 randomUUID 仍发出一次动作
    expect(bodies[0].actionId.length).toBeGreaterThan(0);
    // 仍是 v4 形状（version=4 / variant=10），宿主可直接当作 action id
    expect(bodies[0].actionId).toMatch(
      /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/
    );
    expect(settled.actionId).toBe(bodies[0].actionId);
  });

  it("stale_get_cannot_overwrite_newer_connection_state", async () => {
    const connectionGate = deferred<Response>();
    installFetch((url) => {
      // 同一个延迟响应可能被多次 GET 消费（同代配对跃迁会补一次纠偏读）：
      // 每次给一个 clone，避免第二次读同 body 报「body unusable」被误判成网络错误。
      if (url === ZCODE_CONNECTION_PATH) {
        return connectionGate.promise.then((response) => response.clone());
      }
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(sessionView());
      throw new Error(`unexpected fetch: ${url}`);
    });

    const { result } = renderHook(() => useZcodeControl(SID));
    await flush();

    // GET 仍在途时，SSE 已把连接推进到 paired（gen 4）
    const es = MockEventSource.latest();
    await act(async () => {
      es.emit("connection", {
        seq: 1,
        generation: 4,
        sessionId: SID,
        kind: "connection",
        data: connection({ generation: 4 }),
      });
    });
    expect(result.current.connection?.generation).toBe(4);
    expect(result.current.connection?.state).toBe("paired");

    // 旧 GET（gen 3, needs_auth）迟到：连接视图不被覆盖，原因面也不得被改写
    await act(async () => {
      connectionGate.resolve(json(connection({ state: "needs_auth", generation: 3 })));
      await Promise.resolve();
    });
    await flush();

    expect(result.current.connection?.state).toBe("paired");
    expect(result.current.reason).not.toBe("needs_auth");
    expect(result.current.reason).toBeNull();
  });

  it("failure_preserves_draft", async () => {
    let response: "send_failed" | "rejected" | "network" = "send_failed";
    const gate = deferred<Response>();
    installFetch((url, init) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(sessionView());
      if (url === ZCODE_ACTIONS_PATH && init?.method === "POST") {
        if (response === "network") throw new TypeError("synthetic network down");
        if (response === "send_failed") return gate.promise;
        const body = JSON.parse(String(init.body)) as { actionId: string };
        return json({
          actionId: body.actionId,
          sessionId: SID,
          generation: 3,
          state: "rejected",
          reasonCode: "invalid_answer",
        } satisfies ActionReceipt);
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    const { result } = renderHook(() => useZcodeControl(SID));
    await flush();

    let pending!: Promise<ActionReceipt>;
    await act(async () => {
      pending = result.current.submit(sendAction);
      await Promise.resolve();
    });
    expect(result.current.submitting).toBe(true);

    await act(async () => {
      gate.resolve(
        json({
          actionId: "11111111-1111-4111-8111-111111111111",
          sessionId: SID,
          generation: 3,
          state: "send_failed",
          reasonCode: "offline",
        } satisfies ActionReceipt)
      );
      await pending;
    });
    expect(result.current.submitting).toBe(false);
    expect(result.current.draftCleared).toBe(false);
    expect(result.current.reason).toBe("offline");
    expect(postedActions()).toHaveLength(1);
    await flush();
    expect(postedActions()).toHaveLength(1); // send_failed 不自动重发

    response = "rejected";
    await act(async () => {
      await result.current.submit(sendAction);
    });
    expect(result.current.draftCleared).toBe(false);
    expect(result.current.reason).toBe("invalid_answer");
    expect(postedActions()).toHaveLength(2);

    response = "network";
    await act(async () => {
      const error = await result.current.submit(sendAction).catch((e: unknown) => e);
      expect(error).toBeInstanceOf(ZcodeControlApiError);
      expect((error as ZcodeControlApiError).code).toBeNull();
    });
    expect(result.current.submitting).toBe(false);
    expect(result.current.draftCleared).toBe(false);
    expect(result.current.reason).toBe("offline"); // 网络层失败按 offline 呈现
    expect(postedActions()).toHaveLength(3);
  });
});

describe("useZcodeControl：SSE 纠偏（不重放动作）", () => {
  it("sse_lag_triggers_get_resync", async () => {
    let revision = 12;
    installFetch((url) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(sessionView({ revision }));
      throw new Error(`unexpected fetch: ${url}`);
    });

    const { result } = renderHook(() => useZcodeControl(SID));
    await flush();
    expect(result.current.session?.revision).toBe(12);
    const connectionGets = () => calledUrls().filter((url) => url === ZCODE_CONNECTION_PATH).length;
    const sessionGets = () =>
      calledUrls().filter((url) => url.startsWith(`${ZCODE_SESSION_PATH}?`)).length;
    expect(connectionGets()).toBe(1);
    expect(sessionGets()).toBe(1);

    revision = 13;
    await act(async () => {
      MockEventSource.latest().emit("resync", { reason: "lag" });
      await Promise.resolve();
    });
    await flush();

    // lag = 中间事件已丢：只重 GET 连接 + 会话纠偏，绝不重放动作
    expect(connectionGets()).toBe(2);
    expect(sessionGets()).toBe(2);
    expect(result.current.session?.revision).toBe(13);
    expect(postedActions()).toHaveLength(0);
  });

  it("sse_unsubscribes_on_unmount_and_session_change", async () => {
    const SID2 = "sess_0000000000000000000000000000000b";
    installFetch((url) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(sessionView());
      throw new Error(`unexpected fetch: ${url}`);
    });

    const { rerender, unmount } = renderHook(({ sid }: { sid: string }) => useZcodeControl(sid), {
      initialProps: { sid: SID },
    });
    await flush();
    expect(MockEventSource.instances).toHaveLength(1);
    const first = MockEventSource.latest();
    expect(first.url).toBe(zcodeEventsUrl(SID));

    rerender({ sid: SID2 });
    await flush();
    // 换会话：旧流立即关闭，新流只订阅新会话（不共享旧事件面）
    expect(first.closed).toBe(true);
    expect(MockEventSource.instances).toHaveLength(2);
    const second = MockEventSource.latest();
    expect(second.url).toBe(zcodeEventsUrl(SID2));
    expect(second.closed).toBe(false);

    unmount();
    expect(second.closed).toBe(true);
  });

  it("sse_reconnect_uses_bounded_backoff", async () => {
    vi.useFakeTimers();
    try {
      installFetch((url) => {
        if (url === ZCODE_CONNECTION_PATH) return json(connection());
        if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(sessionView());
        throw new Error(`unexpected fetch: ${url}`);
      });

      const { rerender, unmount } = renderHook(({ sid }: { sid: string }) => useZcodeControl(sid), {
        initialProps: { sid: SID },
      });
      await flush();
      expect(MockEventSource.instances).toHaveLength(1);

      const advance = async (ms: number) => {
        await act(async () => {
          vi.advanceTimersByTime(ms);
        });
        await flush();
      };
      /** 断流一次，断言「恰好 delay 毫秒后才新建一条连接」 */
      const expectReconnectAfter = async (delay: number) => {
        const before = MockEventSource.instances.length;
        MockEventSource.latest().fail();
        await flush();
        expect(MockEventSource.latest().closed).toBe(true); // 断流即关流（不叠加连接）
        await advance(delay - 1);
        expect(MockEventSource.instances).toHaveLength(before);
        await advance(1);
        expect(MockEventSource.instances).toHaveLength(before + 1);
      };

      // 1s → 2s → 4s → 8s → 16s → 30s（封顶）→ 30s（封顶后维持上限）
      await expectReconnectAfter(1000);
      for (const delay of [2000, 4000, 8000, 16000, 30000, 30000]) {
        await expectReconnectAfter(delay);
      }

      // 恢复（任一帧落地）→ 退避复位：下一次断流仍只等 1s
      await act(async () => {
        MockEventSource.latest().emit("snapshot", { connection: connection() });
      });
      await flush();
      await expectReconnectAfter(1000);

      // 切会话：换新流、旧定时器作废（不再为旧会话建连）
      const sessionChangeCount = MockEventSource.instances.length;
      rerender({ sid: "sess_0000000000000000000000000000000b" });
      await flush();
      expect(MockEventSource.instances).toHaveLength(sessionChangeCount + 1);
      await advance(120_000);
      expect(MockEventSource.instances).toHaveLength(sessionChangeCount + 1);

      // 卸载：不留定时器（长时间推进也不再建连）
      const unmountCount = MockEventSource.instances.length;
      unmount();
      await advance(120_000);
      expect(MockEventSource.instances).toHaveLength(unmountCount);
    } finally {
      vi.useRealTimers();
    }
  });

  it("sse_resync_triggers_one_get", async () => {
    vi.useFakeTimers();
    try {
      installFetch((url) => {
        if (url === ZCODE_CONNECTION_PATH) return json(connection());
        if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(sessionView());
        throw new Error(`unexpected fetch: ${url}`);
      });

      const { unmount } = renderHook(() => useZcodeControl(SID));
      await flush();
      const connectionGets = () =>
        calledUrls().filter((url) => url === ZCODE_CONNECTION_PATH).length;
      const sessionGets = () =>
        calledUrls().filter((url) => url.startsWith(`${ZCODE_SESSION_PATH}?`)).length;
      expect(connectionGets()).toBe(1);
      expect(sessionGets()).toBe(1);

      // 一次断流 = 一次纠偏 GET（连接 + 会话各一发），不是每秒刷新的风暴
      MockEventSource.latest().fail();
      await flush();
      expect(connectionGets()).toBe(2);
      expect(sessionGets()).toBe(2);

      // 退避等待期内零 GET（未重连就不纠偏）
      await act(async () => {
        vi.advanceTimersByTime(999);
      });
      await flush();
      expect(connectionGets()).toBe(2);
      expect(sessionGets()).toBe(2);
      expect(MockEventSource.instances).toHaveLength(1);

      // 重连建流本身不额外发 GET
      await act(async () => {
        vi.advanceTimersByTime(1);
      });
      await flush();
      expect(MockEventSource.instances).toHaveLength(2);
      expect(connectionGets()).toBe(2);
      expect(sessionGets()).toBe(2);

      // 连续断流（离线 / 403 拒绝）7 秒：退避把重连压到 t=1s / 3s / 7s，GET 不得按秒刷新。
      // 旧实现每秒重连一次 → 这里会打出 8 发 connection GET（刷新风暴）。
      for (let i = 0; i < 6; i += 1) {
        MockEventSource.latest().fail(); // 已关闭的实例是 no-op（同真实 EventSource）
        await flush();
        await act(async () => {
          vi.advanceTimersByTime(1000);
        });
        await flush();
      }
      expect(MockEventSource.instances).toHaveLength(4); // 首连 + 1s/3s/7s 各重连一次
      expect(connectionGets()).toBe(4); // 首拍 + 三次断流纠偏，各自恰好一发
      expect(sessionGets()).toBe(4);

      // lag 帧（resync）= 恰好一次 GET 纠偏
      const getsBeforeResync = connectionGets();
      await act(async () => {
        MockEventSource.latest().emit("resync", { reason: "lag" });
      });
      await flush();
      expect(connectionGets()).toBe(getsBeforeResync + 1);
      expect(sessionGets()).toBe(getsBeforeResync + 1);
      // 纠偏只读：绝不重放动作
      expect(postedActions()).toHaveLength(0);

      unmount();
    } finally {
      vi.useRealTimers();
    }
  });

  it("sse_stale_session_event_is_ignored", async () => {
    installFetch((url) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection({ generation: 3 }));
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) {
        return json(sessionView({ generation: 3, revision: 12 }));
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    const { result } = renderHook(() => useZcodeControl(SID));
    await flush();
    const es = MockEventSource.latest();

    // 旧代际帧：同会话但 generation=2，不得覆盖当前 generation=3 的投影
    await act(async () => {
      es.emit("session", {
        seq: 1,
        generation: 2,
        sessionId: SID,
        kind: "session",
        data: sessionView({ generation: 2, revision: 99 }),
      });
    });
    expect(result.current.session?.generation).toBe(3);
    expect(result.current.session?.revision).toBe(12);

    // 别的会话的帧：即使代际更高也不得污染本会话
    await act(async () => {
      es.emit("session", {
        seq: 2,
        generation: 9,
        sessionId: "sess_0000000000000000000000000000000c",
        kind: "session",
        data: sessionView({
          sessionId: "sess_0000000000000000000000000000000c",
          generation: 9,
          revision: 77,
        }),
      });
    });
    expect(result.current.session?.sessionId).toBe(SID);
    expect(result.current.session?.revision).toBe(12);

    // 当前代际的正常帧照常落地（不是一刀切丢弃）
    await act(async () => {
      es.emit("session", {
        seq: 3,
        generation: 3,
        sessionId: SID,
        kind: "session",
        data: sessionView({ generation: 3, revision: 14 }),
      });
    });
    expect(result.current.session?.revision).toBe(14);
    expect(postedActions()).toHaveLength(0);
  });

  it("stale_session_frame_refreshes_once_per_generation", async () => {
    // Data 溢出后 Hub 把每个已跟踪会话降级为 ready=false 并广播 Session 帧：
    // 手机必须**自己重拉一次 GET** 重新绑定/现读（否则页面永远停在失效投影），
    // 但同一代次的重复降级帧绝不能再触发 GET（防刷新循环）。
    let serverReady = true;
    let revision = 12;
    installFetch((url) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) {
        return json(
          sessionView({
            ready: serverReady,
            revision,
            capabilities: serverReady ? capabilities() : closedCapabilities(),
          })
        );
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    const { result } = renderHook(() => useZcodeControl(SID));
    await flush();
    const sessionGets = () =>
      calledUrls().filter((url) => url.startsWith(`${ZCODE_SESSION_PATH}?`)).length;
    expect(result.current.session?.ready).toBe(true);
    expect(sessionGets()).toBe(1);

    const emitSession = async (overrides: Partial<SessionView>) => {
      await act(async () => {
        MockEventSource.latest().emit("session", {
          seq: 1,
          generation: 3,
          sessionId: SID,
          kind: "session",
          data: sessionView(overrides),
        });
        await Promise.resolve();
      });
      await flush();
    };

    // 同代 ready=false（lag 降级）：恰好补一发会话 GET 自愈。
    serverReady = false;
    revision = 30;
    await emitSession({ ready: false, revision: 30, capabilities: closedCapabilities() });
    expect(result.current.session?.ready).toBe(false);
    expect(sessionGets()).toBe(2);

    // 同一代次重复降级（多帧/别的设备触发）：不得再发 GET（无刷新风暴）。
    await emitSession({ ready: false, revision: 30, capabilities: closedCapabilities() });
    await emitSession({ ready: false, revision: 30, capabilities: closedCapabilities() });
    expect(sessionGets()).toBe(2);

    // 恢复快照（Hub 事件沿用降级后的全关能力）：必须**自动**补一次权威 GET，
    // 不手动 refresh——GET 落地 ready=true 才算恢复，守卫由 GET 清空。
    serverReady = true;
    revision = 31;
    await emitSession({ ready: true, revision: 31, capabilities: closedCapabilities() });
    expect(result.current.session?.ready).toBe(true);
    expect(sessionGets()).toBe(3);
    expect(result.current.session?.capabilities.send.enabled).toBe(true);

    // 恢复后同代次再次失效：守卫已由 GET 落地重置 → 仍会自愈一次（GET 4）。
    serverReady = false;
    revision = 32;
    await emitSession({ ready: false, revision: 32, capabilities: closedCapabilities() });
    expect(sessionGets()).toBe(4);

    // 本次 GET 仍返回失效投影：守卫保持，同代重复 false 不再刷新。
    await emitSession({ ready: false, revision: 32, capabilities: closedCapabilities() });
    expect(sessionGets()).toBe(4);
  });

  it("recovery_after_not_ready_get_auto_refetches_capabilities", async () => {
    // P2-4：坏帧降级 → 自动 GET 撞上 not_ready（重订阅 ACK 早于快照）→ 随后有效快照
    // ready=true 但能力沿用降级后的全关。手机必须**自己**补一次权威 GET（不手动
    // refresh）才能拿回 send；否则发送按钮会一直灰着。
    let sessionGet: "ready" | "not_ready" | "stale" = "ready";
    let revision = 12;
    installFetch((url) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) {
        if (sessionGet === "not_ready") return json({ error: "not_ready" }, 503);
        if (sessionGet === "stale") {
          return json(sessionView({ revision, ready: false, capabilities: closedCapabilities() }));
        }
        return json(sessionView({ revision, ready: true, capabilities: capabilities() }));
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    const { result } = renderHook(() => useZcodeControl(SID));
    await flush();
    const sessionGets = () =>
      calledUrls().filter((url) => url.startsWith(`${ZCODE_SESSION_PATH}?`)).length;
    const emitSession = async (overrides: Partial<SessionView>) => {
      await act(async () => {
        MockEventSource.latest().emit("session", {
          seq: 1,
          generation: 3,
          sessionId: SID,
          kind: "session",
          data: sessionView(overrides),
        });
        await Promise.resolve();
      });
      await flush();
    };

    // ① 初始：send 可用（GET #1 现算开闸矩阵）。
    expect(sessionGets()).toBe(1);
    expect(result.current.session?.capabilities.send.enabled).toBe(true);

    // ② 坏帧降级：自动 GET #2 撞上 not_ready（会话保持上一份落地的投影 = ready=false）。
    sessionGet = "not_ready";
    revision = 13;
    await emitSession({ ready: false, revision: 13, capabilities: closedCapabilities() });
    expect(sessionGets()).toBe(2);
    expect(result.current.session?.ready).toBe(false);
    expect(result.current.reason).toBe("not_ready");

    // ③ 有效快照 ready=true（能力沿用降级后的全关）：必须自动补 GET #3 拿回权威能力。
    sessionGet = "ready";
    revision = 14;
    await emitSession({ ready: true, revision: 14, capabilities: closedCapabilities() });
    expect(sessionGets()).toBe(3);
    expect(result.current.session?.ready).toBe(true);
    expect(result.current.session?.capabilities.send.enabled).toBe(true);
    // 面板发送门 = paired + reason=null + send.enabled：恢复 GET 必须把 #2 的
    // not_ready 原因面也清掉，否则按钮仍会被 reason 门禁用。
    expect(result.current.reason).toBeNull();
    expect(postedActions()).toHaveLength(0); // 恢复只读：绝不重放动作

    // ④ 同代重复 ready=true（能力已是权威）：有界，不得再补 GET #4。
    await emitSession({ ready: true, revision: 14, capabilities: capabilities() });
    expect(sessionGets()).toBe(3);
    expect(result.current.session?.capabilities.send.enabled).toBe(true);

    // ⑤ 恢复后再次失效（同代）= 新的失效周期：允许再自愈一次（GET #4）；本次 GET
    //    仍返回失效投影（守卫保持），因此重复降级帧不再加发（有界，无风暴）。
    sessionGet = "stale";
    revision = 15;
    await emitSession({ ready: false, revision: 15, capabilities: closedCapabilities() });
    expect(sessionGets()).toBe(4);
    await emitSession({ ready: false, revision: 15, capabilities: closedCapabilities() });
    expect(sessionGets()).toBe(4);
    expect(postedActions()).toHaveLength(0);
  });

  it("stale_snapshot_recovery_refetches_capabilities", async () => {
    // snapshot 路径同规：ready=false 快照触发失效 GET；随后 ready=true 快照（能力全关）
    // 必须自动补一发权威 GET，重复恢复快照不再加发。
    let serverReady = true;
    let revision = 12;
    installFetch((url) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) {
        return json(
          sessionView({
            revision,
            ready: serverReady,
            capabilities: serverReady ? capabilities() : closedCapabilities(),
          })
        );
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    const { result } = renderHook(() => useZcodeControl(SID));
    await flush();
    const sessionGets = () =>
      calledUrls().filter((url) => url.startsWith(`${ZCODE_SESSION_PATH}?`)).length;
    const emitSnapshot = async (overrides: Partial<SessionView>) => {
      await act(async () => {
        MockEventSource.latest().emit("snapshot", {
          connection: connection(),
          session: sessionView(overrides),
        });
        await Promise.resolve();
      });
      await flush();
    };

    expect(sessionGets()).toBe(1);

    // 降级快照 → 失效 GET #2（仍返回失效投影，守卫保持）。
    serverReady = false;
    revision = 20;
    await emitSnapshot({ ready: false, revision: 20, capabilities: closedCapabilities() });
    expect(sessionGets()).toBe(2);

    // 恢复快照（能力全关）→ 自动补权威 GET #3，send 恢复。
    serverReady = true;
    revision = 21;
    await emitSnapshot({ ready: true, revision: 21, capabilities: closedCapabilities() });
    expect(sessionGets()).toBe(3);
    expect(result.current.session?.capabilities.send.enabled).toBe(true);

    // 重复恢复快照：有界，不再加发 GET #4。
    await emitSnapshot({ ready: true, revision: 21, capabilities: capabilities() });
    expect(sessionGets()).toBe(3);
    expect(postedActions()).toHaveLength(0);
  });

  it("stale_get_without_sse_frame_still_recovers_capabilities", async () => {
    // 页面在失效窗口内打开：首拍 GET **直接**读到 ready=false（没有任何降级 SSE 帧）。
    // 随后有效快照 ready=true 但能力沿用全关 → 仍必须自动补一次权威 GET，否则发送
    // 按钮永远灰着（本用例锁的正是「GET 是唯一失效信号」的开局）。
    installFetch((url) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) {
        const gets = fetchMock.mock.calls.filter(([input]) =>
          String(input).startsWith(`${ZCODE_SESSION_PATH}?`)
        ).length;
        if (gets === 1) {
          return json(
            sessionView({ ready: false, revision: 11, capabilities: closedCapabilities() })
          );
        }
        // 权威现读必须**不比已落地视图旧**（rev13）才能恢复能力：更旧的现读只能把
        // 能力由开改关（F1 `recovery` 语义），否则旧快照会把已开的写面关回去/或把
        // 旧内容带回来。这里锁的是「自动补读能拿回 send」，故给同序号的权威读。
        return json(sessionView({ ready: true, revision: 13, capabilities: capabilities() }));
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    const { result } = renderHook(() => useZcodeControl(SID));
    await flush();
    const sessionGets = () =>
      calledUrls().filter((url) => url.startsWith(`${ZCODE_SESSION_PATH}?`)).length;
    const emitSession = async (overrides: Partial<SessionView>) => {
      await act(async () => {
        MockEventSource.latest().emit("session", {
          seq: 1,
          generation: 3,
          sessionId: SID,
          kind: "session",
          data: sessionView(overrides),
        });
        await Promise.resolve();
      });
      await flush();
    };

    // ① 首拍 GET #1 = ready=false（失效窗口内打开，无 SSE 降级帧）。
    expect(sessionGets()).toBe(1);
    expect(result.current.session?.ready).toBe(false);
    expect(result.current.session?.capabilities.send.enabled).toBe(false);

    // ② 有效快照 ready=true（Hub 事件沿用降级后的全关能力）：必须自动补 GET #2。
    await emitSession({ ready: true, revision: 13, capabilities: closedCapabilities() });
    expect(sessionGets()).toBe(2);
    expect(result.current.session?.ready).toBe(true);
    expect(result.current.session?.capabilities.send.enabled).toBe(true);
    expect(postedActions()).toHaveLength(0);

    // ③ 重复 ready=true：有界，不再加发 GET #3。
    await emitSession({ ready: true, revision: 13, capabilities: capabilities() });
    expect(sessionGets()).toBe(2);
  });

  it("recovery_attempts_are_capped_per_generation", async () => {
    // 恢复 GET 连续失败时不得无限重试：同一会话代次至多 2 次恢复尝试（首次 + 一次
    // 重试）。每次 ready=false 都清恢复标记（不被毒化），但尝试计数封顶后 ready=true
    // 直接 no-op；换代/切会话/GET 落地 ready=true 才重置。
    let sessionGet: "ready" | "not_ready" = "ready";
    installFetch((url) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) {
        if (sessionGet === "not_ready") return json({ error: "not_ready" }, 503);
        return json(sessionView({ revision: 12, ready: true, capabilities: capabilities() }));
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    renderHook(() => useZcodeControl(SID));
    await flush();
    const sessionGets = () =>
      calledUrls().filter((url) => url.startsWith(`${ZCODE_SESSION_PATH}?`)).length;
    const emitSession = async (overrides: Partial<SessionView>) => {
      await act(async () => {
        MockEventSource.latest().emit("session", {
          seq: 1,
          generation: 3,
          sessionId: SID,
          kind: "session",
          data: sessionView(overrides),
        });
        await Promise.resolve();
      });
      await flush();
    };
    expect(sessionGets()).toBe(1);

    // 降级帧 → 失效 GET #2 失败。
    sessionGet = "not_ready";
    await emitSession({ ready: false, revision: 20, capabilities: closedCapabilities() });
    expect(sessionGets()).toBe(2);

    // 恢复尝试 1/2：GET #3 仍失败。
    await emitSession({ ready: true, revision: 20, capabilities: closedCapabilities() });
    expect(sessionGets()).toBe(3);

    // 再次降级只清恢复标记，不额外 GET（同代 stale 守卫仍有效）。
    await emitSession({ ready: false, revision: 20, capabilities: closedCapabilities() });
    expect(sessionGets()).toBe(3);

    // 恢复尝试 2/2：GET #4 仍失败。
    await emitSession({ ready: true, revision: 20, capabilities: closedCapabilities() });
    expect(sessionGets()).toBe(4);

    // 第三次恢复请求触顶：no-op（有界，不循环）。
    await emitSession({ ready: false, revision: 20, capabilities: closedCapabilities() });
    expect(sessionGets()).toBe(4);
    await emitSession({ ready: true, revision: 20, capabilities: closedCapabilities() });
    expect(sessionGets()).toBe(4);
    expect(postedActions()).toHaveLength(0);
  });

  it("stale_snapshot_refreshes_once_and_session_switch_clears_guard", async () => {
    // snapshot（首帧/重连基线）里的 ready=false 与 Session 帧同规：一次性自愈；
    // 换会话必须清空守卫，否则新会话同代次的降级帧会被旧守卫吞掉。
    const SID2 = "sess_0000000000000000000000000000000b";
    // 自愈 GET 仍返回失效投影（守卫保持）——若 GET 已恢复 ready=true，守卫会随之解除，
    // 下一次 ready=false 属于「新的失效」，本来就该再刷一次（由上一个用例覆盖）。
    let serverReady = true;
    installFetch((url) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) {
        const sid = url.includes(SID2) ? SID2 : SID;
        return json(
          sessionView({
            sessionId: sid,
            ready: serverReady,
            capabilities: serverReady ? capabilities() : closedCapabilities(),
          })
        );
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    const { rerender } = renderHook(({ sid }: { sid: string }) => useZcodeControl(sid), {
      initialProps: { sid: SID },
    });
    await flush();
    const sessionGets = () =>
      calledUrls().filter((url) => url.startsWith(`${ZCODE_SESSION_PATH}?`)).length;
    expect(sessionGets()).toBe(1);

    const emitSnapshot = async (sid: string, overrides: Partial<SessionView>) => {
      await act(async () => {
        MockEventSource.latest().emit("snapshot", {
          connection: connection(),
          session: sessionView({ sessionId: sid, ...overrides }),
        });
        await Promise.resolve();
      });
      await flush();
    };

    // 重连基线带同代 ready=false：恰好一发 GET（自愈 GET 仍返回失效投影，守卫保持）。
    serverReady = false;
    await emitSnapshot(SID, { ready: false, revision: 40, capabilities: closedCapabilities() });
    expect(sessionGets()).toBe(2);
    // 同代重复快照：不再发 GET。
    await emitSnapshot(SID, { ready: false, revision: 40, capabilities: closedCapabilities() });
    expect(sessionGets()).toBe(2);

    // 切会话：首拍 GET（ready=true）后守卫清空；新会话**同代次**（gen 3）的降级帧
    // 必须能刷新——若旧会话守卫残留，这里会被吞掉（计数停在 3）。
    serverReady = true;
    rerender({ sid: SID2 });
    await flush();
    expect(sessionGets()).toBe(3);
    serverReady = false;
    await emitSnapshot(SID2, { ready: false, revision: 41, capabilities: closedCapabilities() });
    expect(sessionGets()).toBe(4);
    await emitSnapshot(SID2, { ready: false, revision: 41, capabilities: closedCapabilities() });
    expect(sessionGets()).toBe(4);
    expect(postedActions()).toHaveLength(0);
  });

  it("same_generation_paired_event_refreshes_failed_session", async () => {
    // 手机先开：首拍连接 needs_auth（gen 3），会话 GET 失败 → session=null。
    // 桌面端随后配对成功，Hub 发同代（gen 3）connection 帧：代次没变也不能漏掉
    // 这次「未配对 → 已配对」的跃迁，必须重拉会话，否则手机永远停在空投影。
    let paired = false;
    let failSession = true;
    installFetch((url) => {
      if (url === ZCODE_CONNECTION_PATH) {
        return json(
          connection(
            paired
              ? { state: "paired", generation: 3, reasonCode: null }
              : { state: "needs_auth", generation: 3, reasonCode: "needs_auth" }
          )
        );
      }
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) {
        if (failSession) return json({ error: "offline" }, 503);
        return json(sessionView({ generation: 3, revision: 21 }));
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    const { result } = renderHook(() => useZcodeControl(SID));
    await flush();

    const sessionGets = () =>
      calledUrls().filter((url) => url.startsWith(`${ZCODE_SESSION_PATH}?`)).length;
    expect(result.current.connection?.state).toBe("needs_auth");
    expect(result.current.session).toBeNull();
    expect(sessionGets()).toBe(1);

    // 同代配对跃迁：paired 帧 generation 仍是 3（没有换代）
    paired = true;
    failSession = false;
    await act(async () => {
      MockEventSource.latest().emit("connection", {
        seq: 1,
        generation: 3,
        sessionId: SID,
        kind: "connection",
        data: connection({ generation: 3 }),
      });
      await Promise.resolve();
    });
    await flush();

    expect(result.current.connection?.state).toBe("paired");
    // 必须补一次会话 GET，并把 session 填上（不是只改连接态）
    expect(sessionGets()).toBeGreaterThan(1);
    expect(result.current.session?.sessionId).toBe(SID);
    expect(result.current.session?.generation).toBe(3);
    expect(result.current.session?.revision).toBe(21);

    // 重复同代 paired 帧（已落地 paired）不再重复拉取
    const afterPair = sessionGets();
    await act(async () => {
      MockEventSource.latest().emit("connection", {
        seq: 2,
        generation: 3,
        sessionId: SID,
        kind: "connection",
        data: connection({ generation: 3 }),
      });
      await Promise.resolve();
    });
    await flush();
    expect(sessionGets()).toBe(afterPair);
  });
});

describe("useZcodeControl：F1 同代 revision 单调", () => {
  function queueView(ids: string[]): QueueView {
    return {
      known: true,
      paused: false,
      items: ids.map((id, position) => ({ inputId: id, text: id, position })),
    };
  }

  function pendingView(interactionId: string): PendingView {
    return {
      interactionId,
      kind: "permission",
      anchorRowId: 7,
      toolCallId: `tc_${interactionId}`,
      createdAt: null,
      summary: "允许执行",
      toolName: "Bash",
      schemaInteraction: null,
      sensitive: false,
      autoResolution: false,
      autoResolutionState: null,
      questions: [],
      options: [],
      detail: null,
      answerable: false,
      detailTruncated: false,
    };
  }

  it("old_get_cannot_rollback_newer_sse_queue", async () => {
    // 首拍 GET 仍在途时，SSE 已把同代视图推进到 revision=12（新队列）。
    const gate = deferred<Response>();
    installFetch((url) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return gate.promise;
      throw new Error(`unexpected fetch: ${url}`);
    });

    const { result } = renderHook(() => useZcodeControl(SID));
    await flush();
    expect(result.current.session).toBeNull();

    await act(async () => {
      MockEventSource.latest().emit("session", {
        seq: 1,
        generation: 3,
        sessionId: SID,
        kind: "session",
        data: sessionView({ revision: 12, queue: queueView(["queue_new"]) }),
      });
      await Promise.resolve();
    });
    await flush();
    expect(result.current.session?.revision).toBe(12);

    // 慢 GET 带着 revision=11 与旧队列迟到：不得回退视图。
    await act(async () => {
      gate.resolve(json(sessionView({ revision: 11, queue: queueView(["queue_old"]) })));
      await Promise.resolve();
    });
    await flush();

    expect(result.current.session?.revision).toBe(12);
    expect(result.current.session?.queue?.items.map((item) => item.inputId)).toEqual(["queue_new"]);
  });

  it("sse_safety_degrade_without_revision_still_closes_writes", async () => {
    // 首拍 GET 就绪；随后 SSE 宣布失效（ready=false、revision=null）：revision 守卫
    // 绝不能挡住安全降级，写面必须立即关闭。
    let degraded = false;
    installFetch((url) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) {
        return json(
          degraded
            ? sessionView({ ready: false, revision: 11, capabilities: closedCapabilities() })
            : sessionView()
        );
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    const { result } = renderHook(() => useZcodeControl(SID));
    await flush();
    expect(result.current.session?.ready).toBe(true);
    expect(result.current.session?.capabilities.send.enabled).toBe(true);

    degraded = true;
    await act(async () => {
      MockEventSource.latest().emit("session", {
        seq: 1,
        generation: 3,
        sessionId: SID,
        kind: "session",
        data: sessionView({
          ready: false,
          revision: null,
          capabilities: closedCapabilities(),
        }),
      });
      await Promise.resolve();
    });
    await flush();

    expect(result.current.session?.ready).toBe(false);
    expect(result.current.session?.capabilities.send.enabled).toBe(false);
  });

  const CAPABILITY_KEYS = [
    "send",
    "singleSelect",
    "multiSelect",
    "freeText",
    "interrupt",
    "approvalReply",
    "model",
    "thoughtLevel",
  ] as const;

  function emitSessionFrame(data: SessionView) {
    return act(async () => {
      MockEventSource.latest().emit("session", {
        seq: 1,
        generation: data.generation,
        sessionId: SID,
        kind: "session",
        data,
      });
      await Promise.resolve();
    });
  }

  it("stale_degrade_keeps_newer_queue_content", async () => {
    // 当前 rev12 就绪、队列两条。一个迟到的 rev11 `ready=false`（applyRead 会把它标成
    // safety-degrade）只能关写面：队列/pending 一律留在 rev12，绝不能被旧快照覆盖。
    let serverView = sessionView({
      revision: 12,
      queue: queueView(["queue_a", "queue_b"]),
    });
    installFetch((url) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(serverView);
      throw new Error(`unexpected fetch: ${url}`);
    });

    const { result } = renderHook(() => useZcodeControl(SID));
    await flush();
    const sessionGets = () =>
      calledUrls().filter((url) => url.startsWith(`${ZCODE_SESSION_PATH}?`)).length;
    expect(result.current.session?.revision).toBe(12);
    expect(result.current.session?.queue?.items).toHaveLength(2);
    expect(result.current.session?.capabilities.send.enabled).toBe(true);

    // 迟到的旧快照：rev11、ready=false、空队列、能力全关。
    serverView = sessionView({
      revision: 11,
      ready: false,
      queue: queueView([]),
      capabilities: closedCapabilities(),
    });
    await emitSessionFrame(serverView);
    await flush();
    // 失效自愈 GET 仍返回这份旧快照：同样只能降级、不得回退内容。
    expect(sessionGets()).toBe(2);

    expect(result.current.session?.queue?.items.map((item) => item.inputId)).toEqual([
      "queue_a",
      "queue_b",
    ]);
    expect(result.current.session?.revision).toBe(12);
    expect(result.current.session?.ready).toBe(false);
    // 写面全关（面板 sendAllowed = gateReason + send 能力 + 代际，见 ZcodeControlPanel）。
    for (const key of CAPABILITY_KEYS) {
      expect(result.current.session?.capabilities[key].enabled).toBe(false);
    }
    expect(postedActions()).toHaveLength(0);
  });

  it("recovery_never_restores_older_content", async () => {
    // rev12 就绪（队列两条、能力全关）→ 更旧的降级帧关写面（内容不回退）→ 由它触发的
    // 恢复 GET 返回更旧的 rev10 `ready=true` 且能力全开：内容一律留在较新的 rev12
    // （旧队列不得复活）；但降级态下**允许能力恢复**——能力取 incoming 的权威现算值，
    // 否则降级后发送按钮会永久灰着（本表以「内容不回退」为准，能力单独放行）。
    let serverView = sessionView({
      revision: 12,
      queue: queueView(["queue_new1", "queue_new2"]),
      capabilities: closedCapabilities(),
    });
    installFetch((url) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(serverView);
      throw new Error(`unexpected fetch: ${url}`);
    });

    const { result } = renderHook(() => useZcodeControl(SID));
    await flush();
    const sessionGets = () =>
      calledUrls().filter((url) => url.startsWith(`${ZCODE_SESSION_PATH}?`)).length;
    expect(result.current.session?.revision).toBe(12);
    expect(result.current.session?.queue?.items).toHaveLength(2);

    // 恢复 GET 的答案先换好：rev10（比当前旧）、ready=true、能力全开、旧队列。
    serverView = sessionView({
      revision: 10,
      ready: true,
      queue: queueView(["queue_old"]),
      capabilities: capabilities(),
    });
    await emitSessionFrame(
      sessionView({
        revision: 11,
        ready: false,
        queue: queueView([]),
        capabilities: closedCapabilities(),
      })
    );
    await flush();
    // 恢复现读确实发生过（否则下面的断言会因「根本没读」而空过）。
    expect(sessionGets()).toBe(2);

    expect(result.current.session?.queue?.items.map((item) => item.inputId)).toEqual([
      "queue_new1",
      "queue_new2",
    ]);
    expect(result.current.session?.revision).toBe(12);
    // 降级态下恢复：就绪与权威能力必须回来（内容仍不回退）。
    expect(result.current.session?.ready).toBe(true);
    expect(result.current.session?.capabilities.send.enabled).toBe(true);
    expect(postedActions()).toHaveLength(0);
  });

  it("degraded_recovery_keeps_newer_queue_content", async () => {
    // 同一场景（降级态 + 更旧的 recovery 现读），但这次锁**内容**：就绪与能力可以恢复，
    // 队列/pending/control/selection 一律留在较新视图——旧快照的 `queue_old` 绝不复活。
    let serverView = sessionView({
      revision: 12,
      queue: queueView(["queue_a", "queue_b"]),
      pending: [pendingView("pi_new")],
      control: { activeWorks: [{ kind: "exec", foregroundExecutionId: "exec_new" }] },
      selection: { provider: "bigmodel", model: "glm-5", thought: null, models: [] },
      capabilities: closedCapabilities(),
    });
    installFetch((url) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(serverView);
      throw new Error(`unexpected fetch: ${url}`);
    });

    const { result } = renderHook(() => useZcodeControl(SID));
    await flush();
    const sessionGets = () =>
      calledUrls().filter((url) => url.startsWith(`${ZCODE_SESSION_PATH}?`)).length;
    expect(result.current.session?.revision).toBe(12);

    // 恢复 GET 返回更旧的 rev10（ready=true、能力开、内容全旧）：
    serverView = sessionView({
      revision: 10,
      ready: true,
      queue: queueView(["queue_old"]),
      pending: [pendingView("pi_old")],
      control: { activeWorks: [{ kind: "exec", foregroundExecutionId: "exec_old" }] },
      selection: { provider: "bigmodel", model: "glm-4", thought: null, models: [] },
      capabilities: capabilities(),
    });
    // 更旧的降级帧（rev11）：关写面但不回退内容，并触发恢复现读。
    await emitSessionFrame(
      sessionView({
        revision: 11,
        ready: false,
        queue: queueView([]),
        pending: [],
        control: { activeWorks: [] },
        selection: null,
        capabilities: closedCapabilities(),
      })
    );
    await flush();
    expect(sessionGets()).toBe(2);

    // 能力/就绪恢复……
    expect(result.current.session?.ready).toBe(true);
    expect(result.current.session?.capabilities.send.enabled).toBe(true);
    // ……但内容仍是较新的那两条（以及较新的 pending/control/selection）。
    expect(result.current.session?.queue?.items.map((item) => item.inputId)).toEqual([
      "queue_a",
      "queue_b",
    ]);
    expect(result.current.session?.pending.map((item) => item.interactionId)).toEqual(["pi_new"]);
    expect(
      result.current.session?.control?.activeWorks.map((work) => work.foregroundExecutionId)
    ).toEqual(["exec_new"]);
    expect(result.current.session?.selection?.model).toBe("glm-5");
    expect(result.current.session?.revision).toBe(12);
    expect(postedActions()).toHaveLength(0);
  });

  it("degraded_recovery_with_null_revision_restores_caps", async () => {
    // 当前 rev12 已降级（队列两条、能力全关）→ 恢复现读带回 **revision=null** 的
    // `ready=true`（宿主重启/投影刚恢复时给不出序号）：仍必须恢复就绪与能力，否则
    // 视图永久停在关闭态；内容（队列两条）一律不得被旧读替换。
    let serverView = sessionView({
      revision: 12,
      queue: queueView(["queue_a", "queue_b"]),
    });
    installFetch((url) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(serverView);
      throw new Error(`unexpected fetch: ${url}`);
    });

    const { result } = renderHook(() => useZcodeControl(SID));
    await flush();
    const sessionGets = () =>
      calledUrls().filter((url) => url.startsWith(`${ZCODE_SESSION_PATH}?`)).length;
    expect(result.current.session?.revision).toBe(12);
    expect(result.current.session?.capabilities.send.enabled).toBe(true);

    // 恢复现读的答案：ready=true、能力全开，但序号为空（旧队列也不再被声称）。
    serverView = sessionView({
      revision: null,
      ready: true,
      queue: queueView(["queue_old"]),
      capabilities: capabilities(),
    });
    // 更旧的降级帧（rev11）：只关写面、不回退内容，并触发恢复现读。
    await emitSessionFrame(
      sessionView({
        revision: 11,
        ready: false,
        queue: queueView([]),
        capabilities: closedCapabilities(),
      })
    );
    await flush();
    expect(sessionGets()).toBe(2);

    // 就绪与权威能力必须回来（空序号不得成为挡板）……
    expect(result.current.session?.ready).toBe(true);
    expect(result.current.session?.capabilities.send.enabled).toBe(true);
    // ……内容仍是最新那两条，旧读的 `queue_old` 绝不复活。
    expect(result.current.session?.queue?.items.map((item) => item.inputId)).toEqual([
      "queue_a",
      "queue_b",
    ]);
    expect(result.current.session?.revision).toBe(12);
    expect(postedActions()).toHaveLength(0);
  });

  it("recovery_restores_enabled_caps_when_newer", async () => {
    // 恢复现读**不比当前旧**（rev13 > rev12）时仍走完整落地：能力按新视图生效。
    let serverView = sessionView({
      revision: 12,
      queue: queueView(["queue_a"]),
      capabilities: closedCapabilities(),
    });
    installFetch((url) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(serverView);
      throw new Error(`unexpected fetch: ${url}`);
    });

    const { result } = renderHook(() => useZcodeControl(SID));
    await flush();
    const sessionGets = () =>
      calledUrls().filter((url) => url.startsWith(`${ZCODE_SESSION_PATH}?`)).length;
    expect(result.current.session?.capabilities.send.enabled).toBe(false);

    // 降级帧（同序 rev12）→ 触发恢复 GET；该 GET 返回 rev13（更新）且能力全开。
    serverView = sessionView({ revision: 13, queue: queueView(["queue_b"]) });
    await emitSessionFrame(
      sessionView({
        revision: 12,
        ready: false,
        queue: queueView([]),
        capabilities: closedCapabilities(),
      })
    );
    await flush();
    expect(sessionGets()).toBe(2);

    expect(result.current.session?.ready).toBe(true);
    expect(result.current.session?.revision).toBe(13);
    expect(result.current.session?.capabilities.send.enabled).toBe(true);
    expect(result.current.session?.queue?.items.map((item) => item.inputId)).toEqual(["queue_b"]);
    expect(postedActions()).toHaveLength(0);
  });
});

describe("useZcodeControl：本地待发队列", () => {
  it("local_queue_snapshot_event_merges_by_version", async () => {
    const local = (version: number) => ({
      sessionId: SID,
      version,
      autoDrain: true,
      pauseReason: null,
      items: [
        {
          itemId: "item-1",
          itemVersion: 1,
          state: "queued",
          position: 1,
          textPreview: `第${version}版`,
          hasAttachments: false,
        },
      ],
      recoverableEdits: [],
    });
    installFetch((url) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(sessionView());
      if (url.startsWith("/m/api/v1/zcode/local-queue?")) return json(local(2));
      throw new Error(`unexpected fetch: ${url}`);
    });

    const { result } = renderHook(() => useZcodeControl(SID));
    await waitFor(() => expect(result.current.localQueue?.version).toBe(2));

    const es = MockEventSource.latest();
    await act(async () => {
      es.emit("local_queue", local(3));
    });
    expect(result.current.localQueue?.version).toBe(3);
    expect(result.current.localQueue?.items[0].textPreview).toBe("第3版");

    await act(async () => {
      es.emit("local_queue", local(1));
    });
    expect(result.current.localQueue?.version).toBe(3);
  });
});
