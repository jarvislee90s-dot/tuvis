import { dshSession, json } from "./dshSessionFixture";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import MessageComposer from "@/mobile/MessageComposer";
import SessionDetail from "@/mobile/SessionDetail";
import type { SendInfo } from "@/mobile/api";
import { pinZhNavigator } from "./pinZhNavigator";

// 只替换 HTTP；真实详情页、composer 与 API 转换共同消费同一实例的预检结果。
const session = dshSession({
  id: "dsh-preflight-session",
  projectName: "preflight-project",
  projectPath: "/tmp/preflight-project",
  title: "手机发送预检",
  status: "idle",
  lastMessageSubagentReport: false,
  flapFromSubagentActivity: false,
  lastActivityAt: "2026-11-06T00:00:00Z",
  activeSubagentCount: 0,
  form: "app",
  dshInstanceId: "dsh-preflight-instance",
});

const available: SendInfo = {
  injectable: true,
  channels: ["dsh_desktop"],
  visibility: "after_refresh",
};
const hostStopped: SendInfo = {
  injectable: false,
  reasonCode: "DSH_HOST_NOT_RUNNING",
  reason: "DeepSeek Harness 桌面端未在运行",
};
const ownershipDenied: SendInfo = {
  injectable: false,
  reasonCode: "DSH_SESSION_OWNERSHIP_UNPROVEN",
  reason: "无法证明该会话由所选实例提供，已拒绝写入（请重新选择实例）",
};
const draft = "保留这条尚未发送的消息";

let currentInfo: SendInfo;
let preflightResponses: Array<Promise<Response>>;
let refuseOwnership: boolean;
let fetchMock: ReturnType<typeof vi.fn>;
let unpinZh: () => void;

function deferredResponse() {
  let resolve!: (response: Response) => void;
  const promise = new Promise<Response>((release) => {
    resolve = release;
  });
  return { promise, resolve };
}

function callsTo(pathname: string) {
  return fetchMock.mock.calls.filter(
    ([input]) => new URL(String(input), "http://localhost").pathname === pathname
  );
}

function preflightCalls() {
  return callsTo("/m/api/v1/session-send-info");
}

function expectInstanceScopedPreflight() {
  for (const [input] of preflightCalls()) {
    const query = new URL(String(input), "http://localhost").searchParams;
    expect(query.get("session_id")).toBe("dsh-preflight-session");
    expect(query.get("instanceId")).toBe("dsh-preflight-instance");
  }
}

beforeAll(async () => {
  unpinZh = await pinZhNavigator();
});

afterAll(() => unpinZh());

beforeEach(() => {
  sessionStorage.clear();
  currentInfo = available;
  preflightResponses = [];
  refuseOwnership = false;
  // 固定手机竖屏，避免视口默认值导致布局切换重挂 composer，掩盖刷新接线缺失。
  vi.stubGlobal("innerWidth", 390);
  vi.stubGlobal("innerHeight", 844);
  vi.stubGlobal("matchMedia", undefined);
  fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const { pathname } = new URL(String(input), "http://localhost");
    switch (pathname) {
      case "/m/api/v1/session-send-info":
        return preflightResponses.shift() ?? json(currentInfo);
      case "/m/api/v1/session-send": {
        if (refuseOwnership) {
          // 挂载时可发送，发送时归属证据失效；下一次 GET 才能拿到新的禁用结论。
          currentInfo = ownershipDenied;
          return json({ error: "not_injectable", ...ownershipDenied }, 403);
        }
        const { actionId } = JSON.parse(String(init?.body)) as { actionId: string };
        return json({
          status: "dsh",
          receipt: {
            actionId,
            state: "succeeded",
            nativeId: "preflight-native",
            reasonCode: null,
          },
        });
      }
      case "/m/api/v1/session-messages":
        return json({ messages: [], truncated: false });
      case "/m/api/v1/session-files":
        return json({ files: [], truncated: false });
      case "/m/api/v1/session-subagents":
        return json({ subagents: [] });
      case "/m/api/v1/session-queue":
        return json({ items: [] });
      case "/m/api/v1/session-approve-options":
        return json({
          available: false,
          options: [],
          verifiedWith: "test",
          currentVersion: null,
          drift: false,
        });
      case "/m/api/v1/session-question":
        return json({ available: false, questions: [] });
      case "/m/api/v1/session-mode":
        return json({ switchKind: "unsupported" });
      case "/m/api/v1/channel":
        return json({ via: "lan", limited: false, est_mbps_down: 0, est_mbps_up: 0 });
      case "/m/api/v1/host":
        return json({
          host: { name: "preflight-host", platform: "darwin", version: "0", bootId: "preflight" },
          enabledTools: ["dsh"],
          installedTools: ["dsh"],
        });
      default:
        throw new Error(`unexpected fetch: ${String(input)}`);
    }
  });
  vi.stubGlobal("fetch", fetchMock);
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

async function rejectDraft() {
  const input = await screen.findByTestId("composer-input");
  fireEvent.change(input, { target: { value: draft } });
  expect(screen.getByTestId("composer-send")).toBeEnabled();
  refuseOwnership = true;
  fireEvent.click(screen.getByTestId("composer-send"));
  // T3：失败回执按 reasonCode 呈现 errors.* 词表的可操作文案（原 raw 码断言
  // 随映射上线路同步更新——语义不变：失败原因可见且可操作）。
  expect(await screen.findByTestId("send-receipt-failed")).toHaveTextContent(
    "无法证明该会话由所选实例提供"
  );
  await act(async () => {});
  expect(screen.queryByText("已进入 DSH 会话")).not.toBeInTheDocument();
  expect(screen.getByTestId("composer-input")).toHaveValue(draft);
  const sends = callsTo("/m/api/v1/session-send");
  expect(sends).toHaveLength(1);
  expect(JSON.parse(String((sends[0][1] as RequestInit).body))).toEqual({
    sessionId: "dsh-preflight-session",
    instanceId: "dsh-preflight-instance",
    text: draft,
    actionId: expect.any(String),
  });
}

describe("DSH 手机发送预检：同页宿主状态与归属变化", () => {
  // T3：发送预检四轴之一——当代握手/发送凭据就绪。后端给出独立原因码
  // （DSH_SEND_RECONNECTING / DSH_SEND_NOT_AUTHORIZED），输入区必须按码呈现
  // 可操作文案（「正在恢复连接」/「请在电脑上检查所选实例」），不得折叠成
  // 一句通用「不支持远程注入」。
  it.each([
    {
      name: "凭据恢复中显示「正在恢复连接」",
      info: { injectable: false, reasonCode: "DSH_SEND_RECONNECTING" } as SendInfo,
      expected: "正在恢复连接",
    },
    {
      name: "凭据未授权显示「请在电脑上检查所选实例」",
      info: { injectable: false, reasonCode: "DSH_SEND_NOT_AUTHORIZED" } as SendInfo,
      expected: "请在电脑上检查所选实例",
    },
  ])("$name", async ({ info, expected }) => {
    currentInfo = info;
    render(<MessageComposer session={session} refreshSendInfoSignal={0} />);
    const reason = await screen.findByTestId("send-disabled-reason");
    expect(reason).toHaveTextContent(expected);
    // 机器码原样保留在码位（诊断面），可读文案在前。
    expect(reason).toHaveTextContent(info.reasonCode ?? "");
  });

  it("发送遇握手恢复错误时，待核实回执显示按码映射的说明", async () => {
    render(<MessageComposer session={session} refreshSendInfoSignal={0} />);
    const input = await screen.findByTestId("composer-input");
    fireEvent.change(input, { target: { value: draft } });
    preflightResponses = [];
    fetchMock.mockImplementation(async (inputArg: RequestInfo | URL) => {
      const { pathname } = new URL(String(inputArg), "http://localhost");
      if (pathname === "/m/api/v1/session-send") {
        return json({ error: "not_injectable", reasonCode: "DSH_SEND_RECONNECTING" }, 503);
      }
      return json(currentInfo);
    });
    fireEvent.click(screen.getByTestId("composer-send"));
    expect(await screen.findByTestId("send-receipt-dsh")).toHaveTextContent("正在恢复连接");
    expect(screen.queryByTestId("send-receipt-failed")).toBeNull();
    expect(screen.queryByText("已进入 DSH 会话")).not.toBeInTheDocument();
    // 503 不断定投递结果，草稿保留。
    expect(screen.getByTestId("composer-input")).toHaveValue(draft);
  });

  it("保持 key 同次切换实例与刷新信号时，冗余失败不得废弃新实例成功初始化", async () => {
    currentInfo = hostStopped;
    const view = render(
      <MessageComposer
        key="same-composer"
        session={session}
        refreshSendInfoSignal={0}
        draftText={draft}
      />
    );
    expect(await screen.findByTestId("composer-input")).toBeDisabled();
    expect(screen.getByTestId("send-disabled-reason")).toHaveTextContent("DSH_HOST_NOT_RUNNING");
    expect(preflightCalls()).toHaveLength(1);
    expectInstanceScopedPreflight();

    const firstResponse = deferredResponse();
    const redundantResponse = deferredResponse();
    preflightResponses = [firstResponse.promise, redundantResponse.promise];
    const beforeSwitch = preflightCalls().length;
    // key 和原生 sessionId 保持不变；同一次 rerender 更新实例与 signal，复用组件。
    view.rerender(
      <MessageComposer
        key="same-composer"
        session={{ ...session, dshInstanceId: "dsh-preflight-instance-B" }}
        refreshSendInfoSignal={1}
        draftText={draft}
      />
    );
    const newTargetCalls = preflightCalls().slice(beforeSwitch);
    expect(newTargetCalls.length).toBeGreaterThan(0);
    for (const [input] of newTargetCalls) {
      const query = new URL(String(input), "http://localhost").searchParams;
      expect(query.get("session_id")).toBe("dsh-preflight-session");
      expect(query.get("instanceId")).toBe("dsh-preflight-instance-B");
    }
    await act(async () => {
      firstResponse.resolve(json(available));
    });
    await act(async () => {
      redundantResponse.resolve(json({ error: "unavailable" }, 500));
    });

    // 不锁定重复请求数量：合并为一次初始化请求的修复也应通过。
    const input = await screen.findByTestId("composer-input");
    expect(input).toBeEnabled();
    expect(input).toHaveValue(draft);
    expect(screen.queryByTestId("send-disabled-reason")).toBeNull();
    expect(screen.getByTestId("composer-send")).toBeEnabled();
    expect(callsTo("/m/api/v1/session-send")).toHaveLength(0);
    fireEvent.click(screen.getByTestId("composer-send"));
    expect(await screen.findByTestId("send-receipt-dsh")).toHaveAttribute(
      "data-state",
      "succeeded"
    );
    const sends = callsTo("/m/api/v1/session-send");
    expect(sends).toHaveLength(1);
    expect(JSON.parse(String((sends[0][1] as RequestInit).body))).toEqual({
      sessionId: "dsh-preflight-session",
      instanceId: "dsh-preflight-instance-B",
      text: draft,
      actionId: expect.any(String),
    });
  });

  it("携带非零刷新信号重挂 composer 时，冗余请求失败不得丢弃成功首拉或隐藏输入区", async () => {
    const view = render(
      <MessageComposer
        key="before-layout-change"
        session={session}
        refreshSendInfoSignal={0}
        draftText={draft}
      />
    );
    expect(await screen.findByTestId("composer-input")).toBeEnabled();
    view.rerender(
      <MessageComposer
        key="before-layout-change"
        session={session}
        refreshSendInfoSignal={1}
        draftText={draft}
      />
    );
    await act(async () => {});
    expect(preflightCalls()).toHaveLength(2);
    expect(screen.getByTestId("composer-send")).toBeEnabled();

    const firstResponse = deferredResponse();
    const redundantResponse = deferredResponse();
    preflightResponses = [firstResponse.promise, redundantResponse.promise];
    const beforeRemount = preflightCalls().length;
    // 用 key 真正重挂组件，模拟详情布局切换；父级已非零的刷新信号原样保留。
    view.rerender(
      <MessageComposer
        key="after-layout-change"
        session={session}
        refreshSendInfoSignal={1}
        draftText={draft}
      />
    );
    expect(preflightCalls().length).toBeGreaterThan(beforeRemount);
    await act(async () => {
      firstResponse.resolve(json(available));
    });
    await act(async () => {
      redundantResponse.resolve(json({ error: "unavailable" }, 500));
    });

    // 只约束最终可用性：修复时合并重复请求也应通过，不强制保留冗余请求。
    const input = await screen.findByTestId("composer-input");
    expect(input).toBeEnabled();
    expect(input).toHaveValue(draft);
    expect(screen.getByTestId("composer-send")).toBeEnabled();
    expect(screen.queryByTestId("send-disabled-reason")).toBeNull();
    expectInstanceScopedPreflight();
    expect(callsTo("/m/api/v1/session-send")).toHaveLength(0);
  });

  it("首拉预检失败后，手动刷新成功恢复输入区并允许发送", async () => {
    preflightResponses = [Promise.resolve(json({ error: "unavailable" }, 500))];
    await act(async () => {
      render(<SessionDetail session={session} onBack={() => {}} />);
    });
    expect(preflightCalls()).toHaveLength(1);
    expect(screen.queryByTestId("message-composer")).toBeNull();
    expect(screen.getByTestId("detail-refresh")).toBeEnabled();

    fireEvent.click(screen.getByTestId("detail-refresh"));

    await waitFor(() => expect(preflightCalls()).toHaveLength(2));
    const input = await screen.findByTestId("composer-input");
    expect(input).toBeEnabled();
    fireEvent.change(input, { target: { value: draft } });
    expect(screen.getByTestId("composer-send")).toBeEnabled();
    expect(screen.queryByTestId("send-disabled-reason")).toBeNull();
    expectInstanceScopedPreflight();
    expect(callsTo("/m/api/v1/session-send")).toHaveLength(0);
  });

  it.each([
    { name: "旧首拉拒绝不得覆盖刷新后的可发送状态", initial: hostStopped, refreshed: available },
    { name: "旧首拉可发送不得覆盖刷新后的拒绝状态", initial: available, refreshed: hostStopped },
  ])("首拉延迟且刷新结果先返回：$name", async ({ initial, refreshed }) => {
    const firstResponse = deferredResponse();
    const refreshResponse = deferredResponse();
    preflightResponses = [firstResponse.promise, refreshResponse.promise];
    render(<SessionDetail session={session} onBack={() => {}} />);
    expect(preflightCalls()).toHaveLength(1);
    expect(screen.queryByTestId("message-composer")).toBeNull();

    fireEvent.click(screen.getByTestId("detail-refresh"));
    await waitFor(() => expect(preflightCalls()).toHaveLength(2));
    // 分别使用 async act 放行并排空响应处理，固定“新结果先落地、旧结果后落地”。
    await act(async () => {
      refreshResponse.resolve(json(refreshed));
    });
    await act(async () => {
      firstResponse.resolve(json(initial));
    });

    // 等两个请求都结束后再断言最终状态，使此用例独立暴露旧结果覆盖问题，
    // 不被刷新未恢复 infoReady 的另一缺陷提前截断。
    const input = await screen.findByTestId("composer-input");
    expect(input).toHaveProperty("disabled", !refreshed.injectable);
    if (refreshed.injectable) {
      expect(screen.queryByTestId("send-disabled-reason")).toBeNull();
      fireEvent.change(input, { target: { value: draft } });
      expect(screen.getByTestId("composer-send")).toBeEnabled();
    } else {
      expect(screen.getByTestId("send-disabled-reason")).toHaveTextContent("DSH_HOST_NOT_RUNNING");
      expect(screen.getByTestId("composer-send")).toBeDisabled();
    }
    expect(preflightCalls()).toHaveLength(2);
    expectInstanceScopedPreflight();
    expect(callsTo("/m/api/v1/session-send")).toHaveLength(0);
  });

  it.each([
    { name: "宿主恢复后启用输入", initial: hostStopped, next: available },
    { name: "宿主停止后禁用输入", initial: available, next: hostStopped },
  ])("composer 收到外部刷新信号时$name", async ({ initial, next }) => {
    currentInfo = initial;
    const view = render(<MessageComposer session={session} refreshSendInfoSignal={0} />);
    const input = await screen.findByTestId("composer-input");
    expect(input).toHaveProperty("disabled", !initial.injectable);
    expect(preflightCalls()).toHaveLength(1);

    currentInfo = next;
    view.rerender(<MessageComposer session={session} refreshSendInfoSignal={1} />);

    await waitFor(() => expect(input).toHaveProperty("disabled", !next.injectable));
    expect(preflightCalls()).toHaveLength(2);
    expect(screen.getByTestId("composer-input")).toBe(input);
    expectInstanceScopedPreflight();
  });

  it("同一详情页宿主由不可发送恢复后，页头刷新重查预检并启用发送", async () => {
    currentInfo = hostStopped;
    render(<SessionDetail session={session} onBack={() => {}} />);
    const input = await screen.findByTestId("composer-input");
    expect(input).toBeDisabled();
    expect(screen.getByTestId("send-disabled-reason")).toHaveTextContent("DSH_HOST_NOT_RUNNING");
    expect(preflightCalls()).toHaveLength(1);

    currentInfo = available;
    fireEvent.click(screen.getByTestId("detail-refresh"));

    await waitFor(() => expect(preflightCalls()).toHaveLength(2));
    await waitFor(() => expect(screen.getByTestId("composer-input")).toBeEnabled());
    expect(screen.getByTestId("composer-input")).toBe(input);
    expect(screen.queryByTestId("send-disabled-reason")).toBeNull();
    fireEvent.change(input, { target: { value: draft } });
    expect(screen.getByTestId("composer-send")).toBeEnabled();
    expectInstanceScopedPreflight();
    expect(callsTo("/m/api/v1/session-send")).toHaveLength(0);
  });

  it("同一详情页宿主由可发送变为停止后，页头刷新重查预检并保留禁用草稿", async () => {
    render(<SessionDetail session={session} onBack={() => {}} />);
    const input = await screen.findByTestId("composer-input");
    fireEvent.change(input, { target: { value: draft } });
    expect(screen.getByTestId("composer-send")).toBeEnabled();
    expect(preflightCalls()).toHaveLength(1);

    currentInfo = hostStopped;
    fireEvent.click(screen.getByTestId("detail-refresh"));

    await waitFor(() => expect(preflightCalls()).toHaveLength(2));
    await waitFor(() => expect(screen.getByTestId("composer-input")).toBeDisabled());
    expect(screen.getByTestId("composer-input")).toBe(input);
    expect(input).toHaveValue(draft);
    expect(screen.getByTestId("composer-send")).toBeDisabled();
    expect(screen.getByTestId("send-disabled-reason")).toHaveTextContent("DSH_HOST_NOT_RUNNING");
    expectInstanceScopedPreflight();
    fireEvent.click(screen.getByTestId("composer-send"));
    expect(callsTo("/m/api/v1/session-send")).toHaveLength(0);
  });

  it("发送遭归属拒绝后主动重查预检，禁用发送并保留草稿", async () => {
    render(<MessageComposer session={session} />);
    await rejectDraft();

    await waitFor(() => expect(preflightCalls()).toHaveLength(2));
    await waitFor(() => expect(screen.getByTestId("composer-input")).toBeDisabled());
    expect(screen.getByTestId("composer-input")).toHaveValue(draft);
    expect(screen.getByTestId("composer-send")).toBeDisabled();
    expect(screen.getByTestId("send-disabled-reason")).toHaveTextContent(
      "DSH_SESSION_OWNERSHIP_UNPROVEN"
    );
    expectInstanceScopedPreflight();
    expect(callsTo("/m/api/v1/session-send")).toHaveLength(1);
  });

  it("发送遭归属拒绝后，页头刷新仍重新查询同一实例的预检且不重发消息", async () => {
    render(<SessionDetail session={session} onBack={() => {}} />);
    await rejectDraft();
    const before = preflightCalls().length;

    fireEvent.click(screen.getByTestId("detail-refresh"));

    await waitFor(() => expect(preflightCalls().length).toBeGreaterThan(before));
    await waitFor(() => expect(screen.getByTestId("composer-input")).toBeDisabled());
    expect(screen.getByTestId("composer-input")).toHaveValue(draft);
    expect(screen.getByTestId("composer-send")).toBeDisabled();
    expect(screen.getByTestId("send-disabled-reason")).toHaveTextContent(
      "DSH_SESSION_OWNERSHIP_UNPROVEN"
    );
    expectInstanceScopedPreflight();
    expect(callsTo("/m/api/v1/session-send")).toHaveLength(1);
  });
});
