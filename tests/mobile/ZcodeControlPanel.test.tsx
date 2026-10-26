// T7.2 Slice B1a：ZCode 统一控制面板（手机）行为锁 —— 本文件先红后绿，组件尚未实现。
//
// 面板只做三件事：如实呈现连接/回执状态、把草稿交给 useZcodeControl.submit（官方
// `/m/api/v1/zcode/actions`）、控制失败时不吞草稿也不影响同页阅读面。
//
// 契约（实现按此提供，测试只走用户可见面）：
// - 根节点 `data-testid="zcode-control-panel"`，`data-connection-state` = ConnectionState
//   （未确认时 `unknown`；已保存链接绝不等于 `paired`）。
// - 草稿 textarea `zcode-draft`；发送按钮 `zcode-send`（不可发送时 disabled）。
// - 就地原因 `zcode-control-reason`（reason=null 时可不渲染），`data-reason-code` =
//   冻结错误码（不是文案），文本是本地化原因。
// - 回执状态 `zcode-action-status`（收到回执后渲染），`data-action-state` =
//   ActionState；`accepted` 文案恰为「已受理」，`unknown` 不得出现「已生效」。
//
// 网络层替身：GET connection / GET session / POST actions 全部为合成 DTO；不读真实
// 账号、DB 或链接。断言「零旧 session-send / 零无头通道」= 旧发送链从未被触碰。

import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import ZcodeControlPanel from "@/mobile/ZcodeControlPanel";
import {
  ZCODE_ACTIONS_PATH,
  ZCODE_CONNECTION_PATH,
  ZCODE_SESSION_PATH,
  type ActionReceipt,
  type ActionState,
  type CapabilityView,
  type ConnectionView,
  type ControlAction,
  type SessionView,
} from "@/lib/api/zcodeControl";
import { pinZhNavigator } from "./pinZhNavigator";

const SID = "session_0000000000000000000000000000000a";
const GEN = 3;

let unpinZh: () => void;

beforeAll(async () => {
  unpinZh = await pinZhNavigator();
});

afterAll(() => {
  unpinZh();
});

const flag = (enabled: boolean) => ({ enabled, reasonCode: enabled ? null : "needs_evidence" });

/** 发送能力全部打开：本文件的禁用原因只允许来自连接/回执面，不能赖到能力矩阵。 */
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

function connection(overrides: Partial<ConnectionView> = {}): ConnectionView {
  return {
    state: "paired",
    generation: GEN,
    reasonCode: null,
    deviceLabel: "Synthetic Mac",
    ...overrides,
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

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

function receipt(actionId: string, state: ActionState, reasonCode: string | null = null) {
  return {
    actionId,
    sessionId: SID,
    generation: GEN,
    state,
    reasonCode,
  } satisfies ActionReceipt;
}

let fetchMock: ReturnType<typeof vi.fn>;

/** 安装确定性网络替身；未登记 URL 直接抛（防「少调/多调一个接口」变成假阴性）。 */
function installFetch(handler: (url: string, init?: RequestInit) => Response | Promise<Response>) {
  fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) =>
    handler(String(input), init)
  );
  vi.stubGlobal("fetch", fetchMock);
}

function calledUrls(): string[] {
  return fetchMock.mock.calls.map(([input]) => String(input));
}

/** 官方动作 POST（唯一允许的写入口）。 */
function postedActions(): { actionId: string; action: ControlAction }[] {
  return fetchMock.mock.calls
    .filter(
      ([input, init]) =>
        String(input) === ZCODE_ACTIONS_PATH && (init as RequestInit | undefined)?.method === "POST"
    )
    .map(
      ([, init]) =>
        JSON.parse(String((init as RequestInit).body)) as {
          actionId: string;
          action: ControlAction;
        }
    );
}

/** 旧发送/无头入口计数：本任务任何路径都不得回退它们。 */
const legacySendCalls = () =>
  calledUrls().filter((url) => url.includes("/session-send") || /headless/i.test(url)).length;

beforeEach(() => {
  // jsdom 无 EventSource；显式置空让 hook 走「GET 首拍权威」路径（本文件只测面板）。
  vi.stubGlobal("EventSource", undefined);
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

describe("ZCode 控制面板：连接态与发送门", () => {
  it("saved_never_implies_connected", async () => {
    // 链接已保存但官方一侧未授权：服务端返回 needs_auth（不是 paired）。
    // 会话投影本身完全可读且 send 能力为 true —— 发送仍必须被连接态挡住。
    installFetch((url) => {
      if (url === ZCODE_CONNECTION_PATH) {
        return json(connection({ state: "needs_auth", reasonCode: "needs_auth" }));
      }
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(sessionView());
      throw new Error(`unexpected fetch: ${url}`);
    });

    render(<ZcodeControlPanel sessionId={SID} />);

    const reason = await screen.findByTestId("zcode-control-reason");
    await waitFor(() => expect(reason.getAttribute("data-reason-code")).toBe("needs_auth"));

    // 已保存链接 ≠ 已连接：不出现任何「已连接/已配对/paired」的断言性文案
    expect(screen.queryByText(/已连接/)).toBeNull();
    expect(screen.queryByText(/已配对/)).toBeNull();
    expect(screen.queryByText(/paired/i)).toBeNull();
    expect(screen.getByTestId("zcode-control-panel").getAttribute("data-connection-state")).toBe(
      "needs_auth"
    );

    // 就地原因可见且是可读文案，不是错误码原文
    expect(reason).toBeVisible();
    expect(reason.textContent?.trim().length ?? 0).toBeGreaterThan(0);
    expect(reason.textContent).toMatch(/授权/);

    // 发送禁用；即便点击也不产生任何动作 POST（禁用是硬门，不是样式）
    const send = screen.getByTestId("zcode-send") as HTMLButtonElement;
    expect(send).toBeDisabled();
    fireEvent.click(send);
    await act(async () => {});
    expect(postedActions()).toHaveLength(0);
  });
});

describe("ZCode 控制面板：发送只走官方动作接口", () => {
  it("send_uses_official_api_not_legacy_or_headless", async () => {
    installFetch((url, init) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(sessionView());
      if (url === ZCODE_ACTIONS_PATH && init?.method === "POST") {
        const body = JSON.parse(String(init.body)) as { actionId: string };
        return json(receipt(body.actionId, "accepted"));
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    render(<ZcodeControlPanel sessionId={SID} />);

    const draft = (await screen.findByTestId("zcode-draft")) as HTMLTextAreaElement;
    const send = screen.getByTestId("zcode-send") as HTMLButtonElement;
    await waitFor(() => expect(send).toBeEnabled());

    fireEvent.change(draft, { target: { value: "待发送草稿" } });
    fireEvent.click(send);

    await waitFor(() => expect(postedActions()).toHaveLength(1));

    // 唯一写入口 = 官方 POST /m/api/v1/zcode/actions，动作形状 = tagged send + 原文
    expect(ZCODE_ACTIONS_PATH).toBe("/m/api/v1/zcode/actions");
    const [posted] = postedActions();
    expect(posted.action).toEqual({
      kind: "send",
      sessionId: SID,
      generation: GEN,
      text: "待发送草稿",
    });
    // 零旧 session-send / 零无头 CLI：本任务不得把官方动作降级为终端按键
    expect(legacySendCalls()).toBe(0);
  });
});

describe("ZCode 控制面板：回执文案不越级", () => {
  it("accepted_and_unknown_keep_distinct_copy", async () => {
    let actionState: ActionState = "accepted";
    let reasonCode: string | null = null;
    installFetch((url, init) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(sessionView());
      if (url === ZCODE_ACTIONS_PATH && init?.method === "POST") {
        const body = JSON.parse(String(init.body)) as { actionId: string };
        return json(receipt(body.actionId, actionState, reasonCode));
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    render(<ZcodeControlPanel sessionId={SID} />);
    const draft = (await screen.findByTestId("zcode-draft")) as HTMLTextAreaElement;
    const send = screen.getByTestId("zcode-send") as HTMLButtonElement;
    await waitFor(() => expect(send).toBeEnabled());

    // accepted 只表示宿主已受理：文案恰好是「已受理」，绝不是「已生效」
    fireEvent.change(draft, { target: { value: "第一条草稿" } });
    fireEvent.click(send);
    const accepted = await screen.findByTestId("zcode-action-status");
    await waitFor(() => expect(accepted.getAttribute("data-action-state")).toBe("accepted"));
    expect(accepted.textContent?.trim()).toBe("已受理");
    expect(accepted.textContent).not.toContain("已生效");

    // unknown 是第三种语义：既不冒称已受理，也不冒称已生效
    actionState = "unknown";
    reasonCode = "unknown";
    fireEvent.change(draft, { target: { value: "第二条草稿" } });
    fireEvent.click(send);
    await waitFor(() =>
      expect(screen.getByTestId("zcode-action-status").getAttribute("data-action-state")).toBe(
        "unknown"
      )
    );
    const unknown = screen.getByTestId("zcode-action-status");
    expect(unknown.textContent?.trim().length ?? 0).toBeGreaterThan(0);
    expect(unknown.textContent).not.toContain("已生效");
    expect(unknown.textContent).not.toContain("已受理");
  });
});

describe("ZCode 控制面板：失败不吞草稿、不拖垮阅读面", () => {
  it("failure_preserves_draft_reading_stays_visible", async () => {
    installFetch((url, init) => {
      if (url === ZCODE_CONNECTION_PATH) return json(connection());
      if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(sessionView());
      if (url === ZCODE_ACTIONS_PATH && init?.method === "POST") {
        const body = JSON.parse(String(init.body)) as { actionId: string };
        return json(receipt(body.actionId, "send_failed", "offline"));
      }
      throw new Error(`unexpected fetch: ${url}`);
    });

    render(
      <>
        <div data-testid="zcode-reading-transcript">合成正文乙</div>
        <ZcodeControlPanel sessionId={SID} />
      </>
    );

    const draft = (await screen.findByTestId("zcode-draft")) as HTMLTextAreaElement;
    const send = screen.getByTestId("zcode-send") as HTMLButtonElement;
    await waitFor(() => expect(send).toBeEnabled());

    fireEvent.change(draft, { target: { value: "待发送草稿" } });
    fireEvent.click(send);

    await waitFor(() =>
      expect(screen.getByTestId("zcode-action-status").getAttribute("data-action-state")).toBe(
        "send_failed"
      )
    );

    // 草稿必须原样保留（失败的是发送，不是用户输入）
    expect(draft).toHaveValue("待发送草稿");

    // 同页阅读面（正文/文件预览走的是既有读取链）不因控制失败消失
    const reading = screen.getByTestId("zcode-reading-transcript");
    expect(reading).toBeVisible();
    expect(reading.textContent).toContain("合成正文乙");

    // 就地原因如实呈现；失败不触发自动重发（等待一个事件循环窗后仍只有一次 POST）
    const reason = screen.getByTestId("zcode-control-reason");
    expect(reason.getAttribute("data-reason-code")).toBe("offline");
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 20));
    });
    expect(postedActions()).toHaveLength(1);
  });
});
