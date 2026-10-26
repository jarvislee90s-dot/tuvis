// T8.1 Slice C：手机侧生产流（浏览器 mock）——本文件只锁两条链路：
//
// 1. 同 actionId 的两台设备共享同一个动作账本：第二次 POST 命中既有回执，上游只
//    真正派发一次；两次回执逐字段相同；两次提交之间的控制流断开只触发只读纠偏
//    （GET），既不重放动作，也不改变已落袋的回执。
// 2. 控制连接 needs_auth / offline 时，同页阅读面（正文 / 文件预览同辈元素）
//    照常可见，发送硬门零动作 POST。
//
// 网络层替身：全部合成 DTO；未登记 URL 直接抛（防「少调/多调一个接口」变成假阴性）。
// 浏览器 mock 扮演服务端动作账本（按 actionId 去重），不读真实账号 / DB / 网络。

import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import ZcodeControlPanel from "@/mobile/ZcodeControlPanel";
import {
  ZCODE_ACTIONS_PATH,
  ZCODE_CONNECTION_PATH,
  ZCODE_SESSION_PATH,
  type ActionReceipt,
  type CapabilityView,
  type ConnectionState,
  type ConnectionView,
  type ControlAction,
  type SessionView,
} from "@/lib/api/zcodeControl";
import { MockEventSource } from "./eventSourceMock";
import { pinZhNavigator } from "./pinZhNavigator";

const SID = "session_0000000000000000000000000000000c";
const GEN = 7;
/** 两台设备共享的同一次动作 id（合成值；UUID v4 形状）。 */
const FIXED_ACTION_ID = "11111111-1111-4111-8111-111111111111";
const SEND_TEXT = "同一次动作的合成正文";

let unpinZh: () => void;

beforeAll(async () => {
  unpinZh = await pinZhNavigator();
});

afterAll(() => {
  unpinZh();
});

const flag = (enabled: boolean) => ({ enabled, reasonCode: enabled ? null : "needs_evidence" });

/** 发送能力全部打开：本文件的禁用只允许来自连接面。 */
function capabilities(): CapabilityView {
  return {
    send: flag(true),
    singleSelect: flag(true),
    multiSelect: flag(true),
    freeText: flag(true),
    interrupt: flag(true),
    approvalReply: flag(true),
    model: flag(true),
    thoughtLevel: flag(true),
  };
}

function connection(state: ConnectionState = "paired"): ConnectionView {
  return {
    state,
    generation: GEN,
    reasonCode: state === "paired" || state === "connecting" ? null : state,
    deviceLabel: "Synthetic Mac",
  };
}

function sessionView(): SessionView {
  return {
    sessionId: SID,
    generation: GEN,
    revision: 12,
    ready: true,
    pending: [],
    control: { activeWorks: [] },
    selection: null,
    capabilities: capabilities(),
  };
}

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

let fetchMock: ReturnType<typeof vi.fn>;

/** 安装确定性网络替身；未登记 URL 直接抛。 */
function installFetch(handler: (url: string, init?: RequestInit) => Response | Promise<Response>) {
  fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) =>
    handler(String(input), init)
  );
  vi.stubGlobal("fetch", fetchMock);
}

function calledUrls(): string[] {
  return fetchMock.mock.calls.map(([input]) => String(input));
}

interface ActionPostBody {
  actionId: string;
  action: ControlAction;
}

/** 服务端动作账本替身：身份 = actionId；同一 id 重放返回既有回执、零第二发。 */
interface ActionLedger {
  receipts: Map<string, ActionReceipt>;
  posts: ActionPostBody[];
  responses: ActionReceipt[];
  /** 真正离开手机的上游派发次数（与 HTTP 命中次数分离）。 */
  upstreamDispatches: number;
}

function actionLedger(): ActionLedger {
  return { receipts: new Map(), posts: [], responses: [], upstreamDispatches: 0 };
}

function handleActionPost(ledger: ActionLedger, init?: RequestInit): Response {
  const body = JSON.parse(String(init?.body)) as ActionPostBody;
  ledger.posts.push(body);
  let receipt = ledger.receipts.get(body.actionId);
  if (receipt === undefined) {
    ledger.upstreamDispatches += 1;
    receipt = {
      actionId: body.actionId,
      sessionId: SID,
      generation: GEN,
      state: "accepted",
      reasonCode: null,
    } satisfies ActionReceipt;
    ledger.receipts.set(body.actionId, receipt);
  }
  ledger.responses.push(receipt);
  return json(receipt);
}

/** 旧发送 / 无头入口计数：ZCode 会话下任何路径都不得回退它们。 */
const legacySendCalls = () =>
  calledUrls().filter((url) => url.includes("/session-send") || /headless/i.test(url)).length;

beforeEach(() => {
  MockEventSource.reset();
  // 默认无 SSE：本文件只测 GET 首拍与控制流断开两条路径（断开用例内再装替身）。
  vi.stubGlobal("EventSource", undefined);
  window.localStorage.clear();
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  MockEventSource.reset();
});

describe("ZCode 生产流：同 actionId 幂等与断线无关", () => {
  it("two_devices_same_actionId_single_dispatch_across_disconnect", async () => {
    // 两台设备（同配对身份）提交同一次动作：actionId 由账本身份固定。
    vi.spyOn(globalThis.crypto, "randomUUID").mockReturnValue(FIXED_ACTION_ID);
    vi.stubGlobal("EventSource", MockEventSource);

    const ledger = actionLedger();
    installFetch((url, init) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(sessionView());
      if (url.includes("/official-control")) return json({ configState: "missing", url: null });
      if (url === ZCODE_ACTIONS_PATH && init?.method === "POST") {
        return handleActionPost(ledger, init);
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    render(
      <>
        <section data-testid="device-a">
          <ZcodeControlPanel sessionId={SID} />
        </section>
        <section data-testid="device-b">
          <ZcodeControlPanel sessionId={SID} />
        </section>
      </>
    );

    const deviceA = within(screen.getByTestId("device-a"));
    const deviceB = within(screen.getByTestId("device-b"));
    const sendA = (await deviceA.findByTestId("zcode-send")) as HTMLButtonElement;
    const sendB = (await deviceB.findByTestId("zcode-send")) as HTMLButtonElement;
    await waitFor(() => {
      expect(sendA).toBeEnabled();
      expect(sendB).toBeEnabled();
    });

    // 第一台设备：唯一一次真正离机的上游派发。
    fireEvent.change(deviceA.getByTestId("zcode-draft"), { target: { value: SEND_TEXT } });
    fireEvent.change(deviceB.getByTestId("zcode-draft"), { target: { value: SEND_TEXT } });
    fireEvent.click(sendA);
    await waitFor(() => expect(ledger.posts).toHaveLength(1));
    await waitFor(() =>
      expect(deviceA.getByTestId("zcode-action-status").getAttribute("data-action-state")).toBe(
        "accepted"
      )
    );
    expect(ledger.upstreamDispatches).toBe(1);

    // 两次提交之间控制事件流断开：只允许只读纠偏（GET），不得重放动作。
    expect(MockEventSource.instances).toHaveLength(2);
    const connectionGets = () => calledUrls().filter((url) => url === ZCODE_CONNECTION_PATH).length;
    const getsBeforeDisconnect = connectionGets();
    await act(async () => {
      for (const source of MockEventSource.instances) source.fail();
      await Promise.resolve();
    });
    await waitFor(() => expect(connectionGets()).toBe(getsBeforeDisconnect + 2));
    expect(ledger.posts).toHaveLength(1);
    expect(deviceA.getByTestId("zcode-action-status").getAttribute("data-action-state")).toBe(
      "accepted"
    );

    // 第二台设备在断线后提交同一 actionId：命中既有回执，上游零第二发。
    fireEvent.click(sendB);
    await waitFor(() => expect(ledger.posts).toHaveLength(2));
    await waitFor(() =>
      expect(deviceB.getByTestId("zcode-action-status").getAttribute("data-action-state")).toBe(
        "accepted"
      )
    );

    // 两次请求逐字段相同（同 id 同载荷）；回执逐字段相同；上游恰一次派发。
    expect(ledger.posts[1]).toEqual(ledger.posts[0]);
    expect(ledger.posts[1].actionId).toBe(FIXED_ACTION_ID);
    expect(ledger.upstreamDispatches).toBe(1);
    expect(ledger.responses).toHaveLength(2);
    expect(ledger.responses[0]).toEqual(ledger.responses[1]);
    expect(ledger.responses[0].actionId).toBe(FIXED_ACTION_ID);
    expect(deviceB.getByTestId("zcode-action-status").textContent).toBe(
      deviceA.getByTestId("zcode-action-status").textContent
    );
    expect(legacySendCalls()).toBe(0);
  });
});

describe("ZCode 生产流：阅读不依赖控制成功", () => {
  it("full_chain_reads_are_independent_of_control_failure", async () => {
    // 控制面两种失败态各跑一遍：连接如实申报，阅读同辈元素始终可见。
    for (const state of ["needs_auth", "offline"] as const) {
      installFetch((url) => {
        if (url === ZCODE_CONNECTION_PATH) return json(connection(state));
        if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(sessionView());
        if (url.includes("/official-control")) return json({ configState: "missing", url: null });
        throw new Error(`unexpected fetch: ${url}`);
      });

      const view = render(
        <>
          <div data-testid="zcode-reading-transcript">合成正文乙</div>
          <div data-testid="zcode-reading-file-preview">synthetic.rs</div>
          <ZcodeControlPanel sessionId={SID} />
        </>
      );

      const panel = await screen.findByTestId("zcode-control-panel");
      await waitFor(() => expect(panel.getAttribute("data-connection-state")).toBe(state));

      // 控制面如实失败：就地原因可见（本地化文案；码只进 data 属性）。
      const reason = await screen.findByTestId("zcode-control-reason");
      expect(reason.getAttribute("data-reason-code")).toBe(state);
      expect(reason).toBeVisible();
      expect(reason.textContent?.trim().length ?? 0).toBeGreaterThan(0);

      // 发送是硬门：点击零动作 POST，草稿不被吞。
      const draft = screen.getByTestId("zcode-draft") as HTMLTextAreaElement;
      const send = screen.getByTestId("zcode-send") as HTMLButtonElement;
      expect(send).toBeDisabled();
      fireEvent.change(draft, { target: { value: "失败态草稿" } });
      fireEvent.click(send);
      await act(async () => {});
      expect(draft).toHaveValue("失败态草稿");
      expect(ledgerPosts()).toHaveLength(0);

      // 同页阅读面（正文 / 文件预览）不因控制失败消失。
      const transcript = screen.getByTestId("zcode-reading-transcript");
      const preview = screen.getByTestId("zcode-reading-file-preview");
      expect(transcript).toBeVisible();
      expect(transcript.textContent).toContain("合成正文乙");
      expect(preview).toBeVisible();
      expect(preview.textContent).toContain("synthetic.rs");

      expect(legacySendCalls()).toBe(0);
      view.unmount();
    }
  });
});

/** 已发生的动作 POST 数（本文件第二个用例的零写断言）。 */
function ledgerPosts(): unknown[] {
  return fetchMock.mock.calls.filter(
    ([input, init]) =>
      String(input) === ZCODE_ACTIONS_PATH && (init as RequestInit | undefined)?.method === "POST"
  );
}
