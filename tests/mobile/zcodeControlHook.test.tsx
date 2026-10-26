// T7.2 Slice A2a：ZCode 统一控制状态 hook 行为锁（无 SSE；全合成夹具）。
//
// 锁四件事：
// 1. 连接态唯一来自 GET /zcode/connection——已保存链接/可读会话绝不等于已连接，
//    客户端在确认前只显示「未知」；
// 2. accepted 与 unknown 的状态/原因/草稿语义严格分离，unknown 绝不自动重发；
// 3. 每次提交生成一个 actionId（crypto.randomUUID）：请求与回执同一个，
//    重试是新提交 → 新 actionId；
// 4. send_failed / rejected / 未知 / 网络失败一律保留草稿；只有 send 的 accepted
//    允许清草稿。
import { act, cleanup, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import useZcodeControl from "@/mobile/useZcodeControl";
import {
  ZCODE_ACTIONS_PATH,
  ZCODE_CONNECTION_PATH,
  ZCODE_SESSION_PATH,
  ZcodeControlApiError,
  type ActionReceipt,
  type CapabilityView,
  type ConnectionView,
  type ControlAction,
  type SessionView,
} from "@/lib/api/zcodeControl";

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
  vi.stubGlobal("EventSource", vi.fn());
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
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
    // 保存链接不是客户端输入：加载阶段零写、零 SSE（SSE 是下一片）
    expect(postedActions()).toHaveLength(0);
    expect(EventSource).not.toHaveBeenCalled();
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
