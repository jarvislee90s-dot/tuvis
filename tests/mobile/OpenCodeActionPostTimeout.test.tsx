// D1：POST 超时仍是结果未知，不能重发；主动中止则保持 AbortError。
// 发送与恢复本地派发共享服务端动作预算，不能在通用 10s 截断点丢掉确定回执。
import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterAll, afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  enterOpenCodeNative,
  isAbortError,
  OC_RESPONSE_INVALID_MARKER,
  nativeErrorCode,
  submitOpenCodeAction,
  type NativeCommand,
} from "@/mobile/opencodeNativeApi";
import OpenCodeNativePanel from "@/mobile/OpenCodeNativePanel";
import OpenCodeNativeQueue, { resetQueueControlMemoryForTests } from "@/mobile/OpenCodeNativeQueue";
import { resetActionAnchorMemoryForTests } from "@/mobile/opencodeActionAnchors";
import { ApiError } from "@/mobile/api";
import i18n from "@/i18n";
import { MockEventSource } from "./eventSourceMock";
import { pinZhNavigator } from "./pinZhNavigator";

let unpin: () => void;
beforeAll(async () => {
  unpin = await pinZhNavigator();
});
afterAll(() => unpin());

const command = (actionId: string): NativeCommand => ({
  actionId,
  sessionId: "ses_timeout",
  bindingToken: "bind_timeout",
  operation: {
    kind: "send",
    text: "超时探针",
    attachmentPaths: [],
    delivery: "queue",
    clientMessageId: `msg_${actionId}`,
  },
});

/** 挂起的 fetch：仅在 init.signal 中止时以 AbortError 拒绝（模拟连接池饿死的悬空请求）。 */
function hangUntilAborted() {
  const calls = vi.fn();
  vi.stubGlobal(
    "fetch",
    vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
      calls(String(input));
      return new Promise<Response>((_resolve, reject) => {
        init?.signal?.addEventListener("abort", () =>
          reject(new DOMException("The operation was aborted.", "AbortError"))
        );
      });
    })
  );
  return calls;
}

beforeEach(() => {
  sessionStorage.clear();
  MockEventSource.reset();
  vi.useFakeTimers();
});
afterEach(() => {
  cleanup();
  resetActionAnchorMemoryForTests();
  resetQueueControlMemoryForTests();
  sessionStorage.clear();
  vi.useRealTimers();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("写 POST 客户端超时（D1）", () => {
  it("submitOpenCodeAction 挂起 → 超时落可辨未知态（非普通网络错误），且无第二次请求", async () => {
    const calls = hangUntilAborted();
    const pending = submitOpenCodeAction(command("act_timeout_1"), 10_000);
    const assertion = expect(pending).rejects.toMatchObject({
      status: null,
      data: { reasonCode: OC_RESPONSE_INVALID_MARKER },
    });
    await vi.advanceTimersByTimeAsync(10_000);
    await assertion;
    const err = await pending.catch((e) => e);
    expect(isAbortError(err)).toBe(false);
    expect(nativeErrorCode(err)).toBe("OC_RESPONSE_INVALID");
    expect(String(err.message)).not.toContain("MOBILE_NETWORK_FAIL");
    expect(calls).toHaveBeenCalledTimes(1);
  });

  it("普通网络错误（连接拒绝）不携带未知态标记 —— 两形态可辨", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => {
        throw new TypeError("Failed to fetch");
      })
    );
    const err = await submitOpenCodeAction(command("act_timeout_2")).catch((e) => e);
    expect(err).toBeInstanceOf(ApiError);
    expect(err.status).toBeNull();
    expect(nativeErrorCode(err)).toBeNull();
    expect(String(err.message)).toContain("MOBILE_NETWORK_FAIL");
  });

  it("enterOpenCodeNative 挂起 → 超时落「结果未确认」标记", async () => {
    hangUntilAborted();
    const pending = enterOpenCodeNative("ses_timeout", "bind_timeout", 3);
    const assertion = expect(pending).rejects.toMatchObject({
      status: null,
      data: { reasonCode: OC_RESPONSE_INVALID_MARKER },
    });
    await vi.advanceTimersByTimeAsync(10_000);
    await assertion;
  });

  it("调用方主动中止仍按 AbortError 原样抛出（不折成未知态）", async () => {
    hangUntilAborted();
    const controller = new AbortController();
    const pending = enterOpenCodeNative("ses_timeout", "bind_timeout", 3, null, controller.signal);
    const assertion = expect(pending).rejects.toSatisfy(isAbortError);
    await act(async () => {
      controller.abort();
      await Promise.resolve();
    });
    await assertion;
  });
});

const session = "ses_timeout";
const binding = "bind_timeout";
const sendAnchorKey = `tuvis.opencodeNative.action.${session}`;
const resumeAnchorKey = `tuvis.opencodeNative.outboxControl.${session}`;
// 服务端 budget.rs 的统一动作预算；不从待测的客户端超时常量推导回执时机。
const serverBudgetMs = 45_000;
const legacyCutoffMs = 10_000;
const budgetRefusal = "OC_DISPATCH_BUDGET_EXHAUSTED";
const draft = "超时探针";
const response = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status });
const advance = (ms: number) => act(() => vi.advanceTimersByTimeAsync(ms));

function installActionServer(postReply: "refusal" | "lost" | "body-stalled", paused = false) {
  const posts: NativeCommand[] = [];
  const readVerifications: string[] = [];
  const streamedResponses: Response[] = [];
  const abortedPosts = vi.fn();
  vi.stubGlobal("EventSource", MockEventSource as unknown as typeof EventSource);
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = String(input);
      if (url.endsWith("/opencode/actions") && init?.method === "POST") {
        const body = JSON.parse(String(init.body)) as NativeCommand;
        posts.push(body);
        if (postReply === "body-stalled") {
          const stream = new ReadableStream<Uint8Array>({
            start(controller) {
              controller.enqueue(new TextEncoder().encode('{"state":'));
              // 头已到达；模拟 fetch 的 signal 中止仍会打断未消费完的响应流。
              init.signal?.addEventListener(
                "abort",
                () => {
                  abortedPosts(body.actionId);
                  controller.error(new DOMException("aborted", "AbortError"));
                },
                { once: true }
              );
            },
          });
          const headersReceived = new Response(stream, {
            status: 200,
            headers: { "content-type": "application/json" },
          });
          streamedResponses.push(headersReceived);
          return headersReceived;
        }
        return new Promise<Response>((resolve, reject) => {
          // 真正丢失回执：服务端不返回任何内容，由客户端最终退出等待。
          const refusalTimer =
            postReply === "refusal"
              ? setTimeout(
                  () => resolve(response({ error: budgetRefusal, reasonCode: budgetRefusal }, 409)),
                  serverBudgetMs
                )
              : undefined;
          init.signal?.addEventListener(
            "abort",
            () => {
              clearTimeout(refusalTimer);
              abortedPosts(body.actionId);
              reject(new DOMException("aborted", "AbortError"));
            },
            { once: true }
          );
        });
      }
      if (url.includes("/opencode/view"))
        return response({
          sessionId: session,
          bindingToken: binding,
          revision: 1,
          complete: true,
          capabilities: { send: { allowed: true, evidence: "prompt", reasonCode: null } },
          messages: [],
          pending: [],
          inbox: [],
          runtimeState: "ready",
          files: {},
          route: "opencode_native",
          routeReasonCode: null,
        });
      if (url.includes("/opencode/agents"))
        return response({ current: "build", options: [{ id: "build", name: "Build" }] });
      if (url.includes("/opencode/outbox"))
        return response({
          sessionId: session,
          revision: 1,
          paused,
          // 已暂停时本地待发项仍列出（可撤回）；未暂停时空队列（暂停/恢复按钮已删除）
          items: paused
            ? [
                {
                  actionId: "local",
                  owner: "tuvis",
                  state: "queued",
                  textSummary: "本地待发",
                  attachmentCount: 0,
                },
              ]
            : [],
        });
      const actionRead = url.match(/\/opencode\/actions\/([^/]+)(\/verify)?$/);
      if (actionRead) {
        const [, actionId, verify] = actionRead;
        // /verify 使用 POST 但语义仅为核对，不是重新派发动作。
        if (verify && init?.method === "POST") readVerifications.push(actionId);
        return response({ actionId, state: "unconfirmed" });
      }
      throw new Error(`Unexpected request: ${url}`);
    })
  );
  return { posts, readVerifications, abortedPosts, streamedResponses };
}

async function startSend() {
  render(<OpenCodeNativePanel sessionId={session} />);
  await advance(0);
  fireEvent.change(screen.getByTestId("native-input"), { target: { value: draft } });
  fireEvent.click(screen.getByTestId("native-send"));
}

/** 本地撤回（local_only 控制动作）：暂停/恢复按钮已删除（2026-11-22 用户口径），
 *  队列里仍可达的本地写控件就是「撤回本地待发」。 */
async function startRetract() {
  render(<OpenCodeNativeQueue sessionId={session} bindingToken={binding} />);
  await advance(0);
  fireEvent.click(screen.getByTestId("outbox-retract-local"));
}

describe("动作预算内的迟到拒绝仍是确定未派发", () => {
  it("发送在 10s 后继续等确定拒绝，收到后清锚、保留草稿并解锁", async () => {
    const { posts, abortedPosts } = installActionServer("refusal");
    await startSend();
    expect(posts).toHaveLength(1);
    expect(posts[0].operation).toMatchObject({ kind: "send", text: draft });
    expect(sessionStorage.getItem(sendAnchorKey)).toBe(posts[0].actionId);

    await advance(legacyCutoffMs);
    expect.soft(abortedPosts).not.toHaveBeenCalled();

    await advance(serverBudgetMs - legacyCutoffMs);
    expect
      .soft({
        rejected: screen.queryByTestId("native-receipt-rejected") !== null,
        unconfirmed: screen.queryByTestId("native-receipt-unconfirmed") !== null,
        anchor: sessionStorage.getItem(sendAnchorKey),
        locked: (screen.getByTestId("native-send") as HTMLButtonElement).disabled,
      })
      .toEqual({ rejected: true, unconfirmed: false, anchor: null, locked: false });
    expect(screen.getByTestId("native-input")).toHaveValue(draft);
    expect(posts).toHaveLength(1);
  });

  it("撤回本地待发丢回执：保留控件锚、显示未确认，且绝不重投", async () => {
    const { posts, abortedPosts } = installActionServer("lost", true);
    await startRetract();
    expect(posts).toHaveLength(1);
    expect(posts[0].operation).toEqual({
      kind: "retract_outbox",
      targetActionId: "local",
    });
    // 通用写超时（10s）后落未知态：控件锚保留、只给只读核对，零第二次写
    await advance(legacyCutoffMs);
    expect.soft(abortedPosts).toHaveBeenCalledExactlyOnceWith(posts[0].actionId);
    const unknown = screen.getByTestId("outbox-unknown");
    expect(unknown).toBeTruthy();
    expect(sessionStorage.getItem(resumeAnchorKey)).toBe(posts[0].actionId);
    expect(posts).toHaveLength(1);
  });
});

const actionCases = [
  { label: "发送", start: startSend, key: sendAnchorKey, paused: false, button: "native-send" },
];

function queryVerify(paused: boolean) {
  if (!paused) return screen.queryByTestId("native-verify");
  const unknown = screen.queryByTestId("outbox-unknown");
  return unknown
    ? within(unknown).queryByRole("button", { name: i18n.t("mobile.openCodeNative.outbox.verify") })
    : null;
}

it.each(actionCases)(
  "$label 收到响应头但 JSON 流挂起时，50s 转未知并保留原 ID",
  async ({ start, key, paused, button }) => {
    const { posts, abortedPosts, streamedResponses } = installActionServer("body-stalled", paused);
    await start();
    expect(posts).toHaveLength(1);
    const { actionId } = posts[0];

    await advance(49_999);
    expect(streamedResponses).toHaveLength(1);
    expect(streamedResponses[0].bodyUsed).toBe(true);
    expect(abortedPosts).not.toHaveBeenCalled();
    const pendingVerify = queryVerify(paused);
    if (pendingVerify) expect(pendingVerify).toBeDisabled();
    expect(sessionStorage.getItem(key)).toBe(actionId);

    await advance(1);
    expect.soft(abortedPosts).toHaveBeenCalledExactlyOnceWith(actionId);
    expect.soft(queryVerify(paused)).toBeEnabled();
    expect(sessionStorage.getItem(key)).toBe(actionId);
    expect(screen.getByTestId(button)).toBeDisabled();
    expect(posts).toHaveLength(1);
  }
);

describe("真丢回执只按原动作 ID 核对，不重发", () => {
  it.each(actionCases)(
    "$label 超时后仍保留原 ID 和写锁",
    async ({ start, key, paused, button }) => {
      const { posts, readVerifications } = installActionServer("lost", paused);
      await start();
      expect(posts).toHaveLength(1);
      const { actionId } = posts[0];
      // 推过服务端预算和客户端余量，不绑定到某个客户端实现常量。
      await advance(serverBudgetMs + 15_000);

      const verify = paused
        ? within(screen.getByTestId("outbox-unknown")).getByRole("button", {
            name: i18n.t("mobile.openCodeNative.outbox.verify"),
          })
        : screen.getByTestId("native-verify");
      expect(sessionStorage.getItem(key)).toBe(actionId);
      expect(screen.getByTestId(button)).toBeDisabled();
      expect(verify).toBeEnabled();
      fireEvent.click(verify);
      await advance(0);

      expect(readVerifications).toEqual([actionId]);
      expect(sessionStorage.getItem(key)).toBe(actionId);
      expect(posts).toHaveLength(1);
    }
  );
});
