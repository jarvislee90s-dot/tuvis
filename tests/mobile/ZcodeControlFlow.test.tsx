// T8.1 Slice C：手机侧生产流（浏览器 mock）——本文件只锁三条客户端契约：
//
// 1. **两台独立设备**各自渲染一次面板，各自生成 actionId（不是同一 UUID 的两块
//    面板），每台设备的动作只 POST 一次；SSE 断线只触发只读纠偏（GET），绝不重放
//    动作。真实 POST 次数直接取自 fetch mock 的调用记录（不设「账本自计数」）。
//    跨设备同 actionId 的幂等/冲突是**服务端**契约，由 Rust e2e 覆盖（本文件不
//    伪造服务端去重）。
// 2. 初始 control 不可用时，不渲染打断入口、任何点击零动作 POST。
// 3. SSE lag 只重拉 GET 纠偏；投影未就绪期间写面关闭（零 POST），恢复后才放行。
//
// 网络层替身：全部合成 DTO；未登记 URL 直接抛（防「少调/多调一个接口」变成假阴性）。
// 浏览器 mock 只扮演「某台已配对设备看到的服务端」；每台设备独立一份 mock/账本，
// 不读真实账号 / DB / 网络。

import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
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

function sessionView(overrides: Partial<SessionView> = {}): SessionView {
  return {
    sessionId: SID,
    generation: GEN,
    revision: 12,
    ready: true,
    pending: [],
    control: { activeWorks: [] },
    selection: null,
    capabilities: capabilities(),
    ...overrides,
  };
}

/** 残缺 DTO：整个 control 键缺失（守卫只要求 sessionId/generation/ready）。 */
function sessionWithoutControl(): SessionView {
  const view = sessionView();
  delete (view as Partial<SessionView>).control;
  return view;
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

function calledUrls(mock: ReturnType<typeof vi.fn> = fetchMock): string[] {
  return mock.mock.calls.map(([input]) => String(input));
}

interface ActionPostBody {
  actionId: string;
  action: ControlAction;
}

/** 一台设备看到的服务端：回执按 actionId 幂等（真实服务端契约的最小投影）。 */
interface DeviceServer {
  device: string;
  receipts: Map<string, ActionReceipt>;
  posts: ActionPostBody[];
}

function deviceServer(device: string): DeviceServer {
  return { device, receipts: new Map(), posts: [] };
}

/** 安装某台设备的服务端；返回该设备的 fetch mock（真实调用计数只从它取）。 */
function installDeviceFetch(server: DeviceServer): ReturnType<typeof vi.fn> {
  installFetch((url, init) => {
    if (url === ZCODE_CONNECTION_PATH) return json(connection());
    if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(sessionView());
    if (url.includes("/official-control")) return json({ configState: "missing", url: null });
    if (url === ZCODE_ACTIONS_PATH && init?.method === "POST") {
      const body = JSON.parse(String(init.body)) as ActionPostBody;
      server.posts.push(body);
      let receipt = server.receipts.get(body.actionId);
      if (receipt === undefined) {
        receipt = {
          actionId: body.actionId,
          sessionId: SID,
          generation: GEN,
          state: "accepted",
          reasonCode: null,
        } satisfies ActionReceipt;
        server.receipts.set(body.actionId, receipt);
      }
      return json(receipt);
    }
    throw new Error(`unexpected fetch: ${url}`);
  });
  return fetchMock;
}

/** 已发生的动作 POST 请求体（直接数 fetch 调用，不用测试内计数器）。 */
function postBodies(mock: ReturnType<typeof vi.fn> = fetchMock): ActionPostBody[] {
  return mock.mock.calls
    .filter(
      ([input, init]) =>
        String(input) === ZCODE_ACTIONS_PATH && (init as RequestInit | undefined)?.method === "POST"
    )
    .map(([, init]) => JSON.parse(String((init as RequestInit).body)) as ActionPostBody);
}

/** 旧发送 / 无头入口计数：ZCode 会话下任何路径都不得回退它们。 */
const legacySendCalls = (mock: ReturnType<typeof vi.fn> = fetchMock) =>
  calledUrls(mock).filter((url) => url.includes("/session-send") || /headless/i.test(url)).length;

/** 动作 id 必须是 UUID v4 形状（真实客户端每次提交新生成）。 */
const UUID_V4 = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;

beforeEach(() => {
  MockEventSource.reset();
  // 默认无 SSE：本文件只测 GET 首拍、断流与控制流断开三条路径（断开用例内再装替身）。
  vi.stubGlobal("EventSource", undefined);
  window.localStorage.clear();
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  MockEventSource.reset();
});

describe("ZCode 生产流：两台设备的动作各自单发且断线不重放", () => {
  it("two_devices_distinct_action_ids_single_post_each_across_disconnect", async () => {
    vi.stubGlobal("EventSource", MockEventSource);

    // 两台设备 = 两次独立挂载 + 两份独立服务端账本；actionId 由真实客户端生成，
    // 绝不 mock randomUUID（真实客户端不会让两台设备复用同一 id）。
    const observed: Array<{ device: string; actionId: string; posts: number }> = [];
    const servers: DeviceServer[] = [];

    for (const device of ["device-a", "device-b"]) {
      MockEventSource.reset();
      const server = deviceServer(device);
      servers.push(server);
      const mock = installDeviceFetch(server);

      const view = render(<ZcodeControlPanel sessionId={SID} />);
      const send = (await screen.findByTestId("zcode-send")) as HTMLButtonElement;
      await waitFor(() => expect(send).toBeEnabled());

      fireEvent.change(screen.getByTestId("zcode-draft"), { target: { value: SEND_TEXT } });
      fireEvent.click(send);
      await waitFor(() => expect(postBodies(mock)).toHaveLength(1));
      await waitFor(() =>
        expect(screen.getByTestId("zcode-action-status").getAttribute("data-action-state")).toBe(
          "accepted"
        )
      );

      // 真实 POST 次数（fetch 调用）恰一；服务端账本只收到这一条动作。
      const posts = postBodies(mock);
      expect(posts, `${device}: exactly one real POST`).toHaveLength(1);
      expect(posts[0].actionId).toMatch(UUID_V4);
      expect(server.posts).toHaveLength(1);
      expect(server.receipts.size).toBe(1);
      expect(server.receipts.has(posts[0].actionId)).toBe(true);
      expect(legacySendCalls(mock)).toBe(0);

      // 两次提交之间控制流断开：只允许只读纠偏（GET），不得重放动作。
      expect(MockEventSource.instances, `${device}: one SSE stream`).toHaveLength(1);
      const connectionGets = () =>
        calledUrls(mock).filter((url) => url === ZCODE_CONNECTION_PATH).length;
      const getsBeforeDisconnect = connectionGets();
      await act(async () => {
        MockEventSource.latest().fail();
        await Promise.resolve();
      });
      await waitFor(() => expect(connectionGets()).toBe(getsBeforeDisconnect + 1));
      expect(postBodies(mock), `${device}: disconnect must not replay`).toHaveLength(1);
      expect(screen.getByTestId("zcode-action-status").getAttribute("data-action-state")).toBe(
        "accepted"
      );

      observed.push({ device, actionId: posts[0].actionId, posts: posts.length });
      view.unmount();
    }

    // 两台设备各恰一次真实 POST；actionId 不同（跨设备复用同一 id 会触发服务端
    // action_payload_conflict，客户端绝不能制造该场景）。
    expect(observed.map((entry) => entry.posts)).toEqual([1, 1]);
    expect(observed[0].actionId).not.toBe(observed[1].actionId);

    // 设备隔离：每台设备的服务端账本只见过自己的 actionId。
    expect(servers[0].receipts.has(observed[0].actionId)).toBe(true);
    expect(servers[0].receipts.has(observed[1].actionId)).toBe(false);
    expect(servers[1].receipts.has(observed[1].actionId)).toBe(true);
    expect(servers[1].receipts.has(observed[0].actionId)).toBe(false);
  });
});

describe("ZCode 生产流：控制面失败只关写面", () => {
  it("control_failure_keeps_writes_gated", async () => {
    // 控制面两种失败态各跑一遍：连接如实申报，发送硬门零动作 POST。
    // 真实阅读链（正文 / 文件预览 / 附件表）由 Rust e2e 覆盖；本用例不伪造阅读面。
    for (const state of ["needs_auth", "offline"] as const) {
      installFetch((url) => {
        if (url === ZCODE_CONNECTION_PATH) return json(connection(state));
        if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(sessionView());
        if (url.includes("/official-control")) return json({ configState: "missing", url: null });
        throw new Error(`unexpected fetch: ${url}`);
      });

      const view = render(<ZcodeControlPanel sessionId={SID} />);

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
      expect(postBodies()).toHaveLength(0);

      expect(legacySendCalls()).toBe(0);
      view.unmount();
    }
  });
});

describe("ZCode 生产流：初始 control 缺失时零 stop 触发", () => {
  it("no_initial_control_timeout_zero_stop_and_no_trigger", async () => {
    // 初始 control 的三种不可用形态：显式 null / 整个键缺失 / 会话读取失败。
    // 三者都不得被猜成打断目标：不渲染可点的打断入口，任何点击零动作 POST。
    const shapes: Array<{ label: string; session: SessionView | "failed" }> = [
      { label: "control-null", session: sessionView({ control: null }) },
      { label: "control-missing", session: sessionWithoutControl() },
      { label: "session-read-failed", session: "failed" },
    ];

    for (const shape of shapes) {
      installFetch((url) => {
        if (url === ZCODE_CONNECTION_PATH) return json(connection());
        if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) {
          if (shape.session === "failed") return Promise.reject(new Error("read failed"));
          return json(shape.session);
        }
        if (url.includes("/official-control")) return json({ configState: "missing", url: null });
        throw new Error(`unexpected fetch: ${url}`);
      });

      const view = render(<ZcodeControlPanel sessionId={SID} />);
      const panel = await screen.findByTestId("zcode-control-panel");
      await waitFor(() => expect(panel.getAttribute("data-connection-state")).toBe("paired"));

      if (shape.session === "failed") {
        // 读取失败按 offline 如实申报（不装成「控制可用」）。
        await waitFor(() =>
          expect(screen.getByTestId("zcode-control-reason").getAttribute("data-reason-code")).toBe(
            "offline"
          )
        );
      } else {
        await waitFor(() => expect(screen.getByTestId("zcode-send")).toBeEnabled());
      }

      // 没有确认过的前台执行 = 没有打断入口（绝不随取/猜测 stop 目标）。
      expect(
        screen.queryByTestId("zcode-interrupt"),
        `${shape.label}: interrupt must not be rendered`
      ).toBeNull();

      // 发送面同样零写：空草稿点击不产生动作。
      fireEvent.click(screen.getByTestId("zcode-send"));
      await act(async () => {});

      const actions = postBodies();
      expect(actions, `${shape.label}: zero action POSTs`).toHaveLength(0);
      expect(actions.filter(({ action }) => action.kind === "stop")).toHaveLength(0);
      // stop 绝不夹带 sendText：本场景连 send 动作都不允许出现。
      expect(actions.filter(({ action }) => action.kind === "send")).toHaveLength(0);
      expect(legacySendCalls()).toBe(0);
      view.unmount();
    }
  });
});

describe("ZCode 生产流：SSE lag 先纠偏 GET 再放写", () => {
  it("sse_lag_resyncs_before_next_write", async () => {
    vi.stubGlobal("EventSource", MockEventSource);

    // 宿主投影：lag 前 ready=true 且有唯一 primaryTurn；lag 后同一目标仍在，
    // 但投影退回 ready=false（relay 掉线 / stale_session）——写面必须随之关闭。
    let ready = true;
    let revision = 12;
    let writesReleased = false;
    const control = { activeWorks: [{ kind: "primaryTurn", foregroundExecutionId: "E1" }] };
    installFetch((url, init) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) {
        return json(sessionView({ ready, revision, control }));
      }
      if (url.includes("/official-control")) return json({ configState: "missing", url: null });
      if (url === ZCODE_ACTIONS_PATH && init?.method === "POST") {
        if (!writesReleased) {
          throw new Error("write must not be released before resync GET completes");
        }
        const body = JSON.parse(String(init.body)) as {
          actionId: string;
          action: ControlAction;
        };
        return json({
          actionId: body.actionId,
          sessionId: SID,
          generation: GEN,
          state: "accepted",
          reasonCode: null,
        } satisfies ActionReceipt);
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    const view = render(<ZcodeControlPanel sessionId={SID} />);
    const initialInterrupt = (await screen.findByTestId("zcode-interrupt")) as HTMLButtonElement;
    await waitFor(() => expect(initialInterrupt).toBeEnabled());

    const connectionGets = () => calledUrls().filter((url) => url === ZCODE_CONNECTION_PATH).length;
    const sessionGets = () =>
      calledUrls().filter((url) => url.startsWith(`${ZCODE_SESSION_PATH}?`)).length;
    const connectionGetsBefore = connectionGets();
    const sessionGetsBefore = sessionGets();

    // 事件流 lag：中间事件已丢，宿主已退回未就绪，手机侧尚未读到。
    ready = false;
    revision = 13;
    await act(async () => {
      MockEventSource.latest().emit("resync", { reason: "lag" });
      await Promise.resolve();
    });

    // 只读纠偏：连接 + 会话各重拉一次 GET（lag 不得被当成写信号）。
    await waitFor(() => expect(connectionGets()).toBe(connectionGetsBefore + 1));
    await waitFor(() => expect(sessionGets()).toBe(sessionGetsBefore + 1));
    // GET 期间与完成之前：零动作 POST（本 mock 对 POST 直接抛错）。
    expect(postBodies()).toHaveLength(0);

    // 纠偏落地 ready=false：打断入口仍在（目标仍存在）但禁用并申报 not_ready。
    const interrupt = (await screen.findByTestId("zcode-interrupt")) as HTMLButtonElement;
    await waitFor(() => expect(interrupt).toBeDisabled());
    expect(screen.getByTestId("zcode-interrupt-reason").getAttribute("data-reason-code")).toBe(
      "not_ready"
    );

    // ready=false 期间：点击打断 / 发送零动作 POST（写面硬门先于任何写）。
    fireEvent.click(interrupt);
    fireEvent.change(screen.getByTestId("zcode-draft"), { target: { value: "lag 后草稿" } });
    fireEvent.click(screen.getByTestId("zcode-send"));
    await act(async () => {
      await Promise.resolve();
    });

    expect(postBodies()).toHaveLength(0);
    expect(postBodies().filter(({ action }) => action.kind === "stop")).toHaveLength(0);
    expect(postBodies().filter(({ action }) => action.kind === "send")).toHaveLength(0);
    expect(legacySendCalls()).toBe(0);
    // 草稿不被吞：写面关闭不是清空用户输入。
    expect(screen.getByTestId("zcode-draft")).toHaveValue("lag 后草稿");

    // 恢复：resync GET 的 ready=true 落地后才放行写（客户端不会在旧 ready 上写）。
    ready = true;
    revision = 14;
    writesReleased = true;
    await act(async () => {
      MockEventSource.latest().emit("resync", { reason: "lag" });
      await Promise.resolve();
    });
    await waitFor(() => expect(screen.getByTestId("zcode-send")).toBeEnabled());
    fireEvent.click(screen.getByTestId("zcode-send"));
    await waitFor(() => expect(postBodies()).toHaveLength(1));
    expect(postBodies()[0].action.kind).toBe("send");
    expect(legacySendCalls()).toBe(0);
    view.unmount();
  });
});
