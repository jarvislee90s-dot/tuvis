// T7.2 Slice B1d：ZCode 会话页集成（手机）——统一控制面板接管发送区，阅读链不受控制面影响。
//
// 锁三件事：
// 1. ZCode 会话的发送区渲染 `ZcodeControlPanel`，**不再挂载**旧终端 composer；
//    发送只 POST 官方 `/m/api/v1/zcode/actions`，零 `/session-send`、零无头通道；
// 2. 控制面失败（needs_auth / offline）时正文与文件预览照常可见（阅读不依赖控制成功）；
// 3. 非 ZCode 会话（claude/codex）逐字沿用旧 composer 路径（零回归）。
//
// 网络层替身：全部合成 DTO，未登记 URL 直接抛（防「少调/多调一个接口」变成假阴性）。
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import SessionDetail from "@/mobile/SessionDetail";
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
import type { Session } from "@/types/session";
import type { SessionFileEntry, SessionMessage } from "@/mobile/api";
import { pinZhNavigator } from "./pinZhNavigator";

let unpinZh: () => void;
beforeAll(async () => {
  unpinZh = await pinZhNavigator();
});
afterAll(() => {
  unpinZh();
});

const ZCODE_SID = "session_0000000000000000000000000000000b";
const GEN = 5;

function makeSession(overrides: Partial<Session> = {}): Session {
  return {
    id: ZCODE_SID,
    agentType: "zcode",
    projectName: "proj",
    projectPath: "/tmp/proj",
    title: "合成会话",
    gitBranch: null,
    githubUrl: null,
    // idle = 总结态：审批/问答旧卡自隐，本文件只锁发送区与阅读面
    status: "idle",
    lastMessage: null,
    lastMessageRole: null,
    lastMessageSubagentReport: false,
    flapFromSubagentActivity: false,
    lastActivityAt: "2026-10-24T00:00:00Z",
    pid: 1,
    cpuUsage: 0,
    activeSubagentCount: 0,
    form: "app",
    jumpSupported: false,
    unread: true,
    ...overrides,
  };
}

function msg(
  overrides: Partial<SessionMessage> & Pick<SessionMessage, "seq" | "kind" | "content">
): SessionMessage {
  return {
    role: overrides.kind === "user" ? "user" : "assistant",
    ts: 1000,
    collapsed: overrides.kind === "thinking" || overrides.kind === "tool-call",
    ...overrides,
  };
}

const fileEntry: SessionFileEntry = {
  path: "/tmp/proj/synthetic.rs",
  lastSeq: 1,
  lastTs: 1000,
  hits: 1,
  modified: true,
};

const flag = (enabled: boolean) => ({ enabled, reasonCode: enabled ? null : "needs_evidence" });

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
    sessionId: ZCODE_SID,
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

/** 任一合成会话的读取链 + 控制面分路；未登记 URL 直接抛。 */
function installFetch(options: { connectionState?: ConnectionState; injectLegacy?: boolean }) {
  const connectionState = options.connectionState ?? "paired";
  const calls: Array<{ url: string; method: string }> = [];
  const fetchMock = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    const method = init?.method ?? "GET";
    calls.push({ url, method });
    // 官方统一控制（ZCode 唯一写入口）
    if (url === ZCODE_CONNECTION_PATH) return json(connection(connectionState));
    if (url.startsWith(`${ZCODE_SESSION_PATH}?`)) return json(sessionView());
    if (url === ZCODE_ACTIONS_PATH && method === "POST") {
      const body = JSON.parse(String(init?.body)) as { actionId: string };
      const receipt: ActionReceipt = {
        actionId: body.actionId,
        sessionId: ZCODE_SID,
        generation: GEN,
        state: "accepted",
        reasonCode: null,
      };
      return json(receipt);
    }
    // 阅读链（正文 / 文件 / 子 agent / 模式 / 书签 bootId）
    if (url.includes("/session-messages")) {
      return json({
        messages: [
          msg({ seq: 0, kind: "user", content: "合成正文甲" }),
          msg({ seq: 1, kind: "assistant", content: "合成正文乙" }),
        ],
        truncated: false,
      });
    }
    if (url.includes("/session-files")) {
      return json({ files: [fileEntry], truncated: false });
    }
    if (url.includes("/session-subagents")) return json({ subagents: [] });
    if (url.includes("/session-mode")) {
      return json({
        tool: "zcode",
        current: null,
        currentLabel: null,
        readback: false,
        switchKind: "unsupported",
      });
    }
    if (url.includes("/host")) {
      return json({
        host: { name: "n", platform: "windows", version: "0", bootId: "boot-test" },
        enabledTools: [],
        installedTools: [],
      });
    }
    // 旧发送链：ZCode 会话下必须零调用（只为「非 ZCode 回归」用例放行）
    if (url.includes("/session-send-info")) {
      if (!options.injectLegacy) throw new Error(`unexpected fetch: ${url}`);
      return json({ injectable: true, channels: ["tmux"], visibility: "realtime" });
    }
    if (url.includes("/session-send")) {
      if (!options.injectLegacy) throw new Error(`unexpected fetch: ${url}`);
      return json({ status: "delivered" });
    }
    if (url.includes("/session-queue")) return json({ items: [] });
    if (url.includes("/channel")) {
      return json({ via: "lan", limited: false, est_mbps_down: 0, est_mbps_up: 0 });
    }
    if (url.includes("/session-approve-options")) {
      return json({
        available: false,
        options: [],
        verifiedWith: "test",
        currentVersion: null,
        drift: false,
      });
    }
    if (url.includes("/session-question")) return json({ available: false, questions: [] });
    throw new Error(`unexpected fetch: ${url}`);
  });
  vi.stubGlobal("fetch", fetchMock);
  return calls;
}

const postedActions = (calls: Array<{ url: string; method: string }>) =>
  calls.filter((c) => c.url === ZCODE_ACTIONS_PATH && c.method === "POST");
/** 旧发送/无头入口：ZCode 会话下必须为 0。 */
const legacySendCalls = (calls: Array<{ url: string; method: string }>) =>
  calls.filter((c) => c.url.includes("/session-send") || /headless/i.test(c.url)).length;

beforeEach(() => {
  // jsdom 无 EventSource：hook 走「GET 首拍权威」路径（面板测试同款）。
  vi.stubGlobal("EventSource", undefined);
  window.localStorage.clear();
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("ZCode 会话集成：发送区走统一控制面板", () => {
  it("zcode_composer_uses_control_panel_not_legacy_send", async () => {
    const calls = installFetch({ connectionState: "paired" });
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);

    // 发送区 = 统一控制面板；旧终端 composer 整块不再挂载
    const panel = await screen.findByTestId("zcode-control-panel");
    expect(panel.getAttribute("data-connection-state")).toBe("paired");
    expect(screen.queryByTestId("message-composer")).toBeNull();
    expect(screen.queryByTestId("composer-input")).toBeNull();
    expect(screen.queryByTestId("composer-send")).toBeNull();

    // 阅读面照常可见（控制面与阅读面解耦）
    expect(await screen.findByText("合成正文乙")).toBeVisible();

    // 发送：唯一写入口是官方动作 POST
    const draft = screen.getByTestId("zcode-draft") as HTMLTextAreaElement;
    const send = screen.getByTestId("zcode-send") as HTMLButtonElement;
    await waitFor(() => expect(send).toBeEnabled());
    fireEvent.change(draft, { target: { value: "待发送草稿" } });
    fireEvent.click(send);

    await waitFor(() => expect(postedActions(calls)).toHaveLength(1));
    const actionCall = vi
      .mocked(fetch)
      .mock.calls.find(
        ([input, init]) => String(input) === ZCODE_ACTIONS_PATH && init?.method === "POST"
      );
    const posted = JSON.parse(String((actionCall?.[1] as RequestInit).body)) as {
      actionId: string;
      action: ControlAction;
    };
    expect(posted.action).toEqual({
      kind: "send",
      sessionId: ZCODE_SID,
      generation: GEN,
      text: "待发送草稿",
    });

    // 零旧发送 / 零无头：连 send-info 资格探测都没有发生
    expect(legacySendCalls(calls)).toBe(0);
    expect(calls.some((c) => c.url.includes("/session-send-info"))).toBe(false);
  });
});

describe("ZCode 会话集成：控制失败不影响阅读", () => {
  it.each<ConnectionState>(["needs_auth", "offline"])(
    "zcode_reading_survives_control_failure（%s）",
    async (state) => {
      const calls = installFetch({ connectionState: state });
      render(<SessionDetail session={makeSession()} onBack={() => {}} />);

      // 控制面如实申报失败态：就地原因可见（本地化文案，不是错误码原文）
      const panel = await screen.findByTestId("zcode-control-panel");
      await waitFor(() => expect(panel.getAttribute("data-connection-state")).toBe(state));
      const reason = await screen.findByTestId("zcode-control-reason");
      expect(reason.getAttribute("data-reason-code")).toBe(state);
      expect(reason.textContent?.trim().length ?? 0).toBeGreaterThan(0);

      // 阅读链（正文 + 文件）照旧：不整页报错、不把「未联机」当「无消息」
      expect(await screen.findByText("合成正文乙")).toBeVisible();
      expect(screen.getByTestId("msg-1")).toBeVisible();
      expect(screen.queryByTestId("detail-error")).toBeNull();
      fireEvent.click(await screen.findByTestId("file-panel-button"));
      const fileRow = await screen.findByTestId("file-row-0-open");
      expect(fileRow.textContent).toContain("synthetic.rs");

      // 失败态下发送是硬门：点击不产生任何动作 POST
      const draft = screen.getByTestId("zcode-draft") as HTMLTextAreaElement;
      fireEvent.change(draft, { target: { value: "失败态草稿" } });
      const send = screen.getByTestId("zcode-send") as HTMLButtonElement;
      expect(send).toBeDisabled();
      fireEvent.click(send);
      await act(async () => {});
      expect(draft).toHaveValue("失败态草稿");
      expect(postedActions(calls)).toHaveLength(0);
      expect(legacySendCalls(calls)).toBe(0);
    }
  );
});

describe("ZCode 会话集成：非 ZCode 会话路径不变", () => {
  it.each(["claude", "codex"] as const)(
    "non_zcode_composer_path_is_unchanged（%s）",
    async (agentType) => {
      const calls = installFetch({ injectLegacy: true });
      const session = makeSession({
        id: "sess-legacy",
        agentType,
        form: "cli",
        status: "processing",
      });
      render(<SessionDetail session={session} onBack={() => {}} />);

      // 旧 composer 照旧挂载；统一控制面板不出现，官方接口零调用
      expect(await screen.findByTestId("composer-input")).toBeInTheDocument();
      expect(screen.queryByTestId("zcode-control-panel")).toBeNull();
      expect(calls.some((c) => c.url.startsWith("/m/api/v1/zcode/"))).toBe(false);

      // 发送仍走既有 /session-send
      fireEvent.change(screen.getByTestId("composer-input"), {
        target: { value: "旧链路正文" },
      });
      fireEvent.click(screen.getByTestId("composer-send"));
      await waitFor(() =>
        expect(
          calls.filter((c) => c.url.includes("/session-send") && c.method === "POST")
        ).toHaveLength(1)
      );
      expect(postedActions(calls)).toHaveLength(0);
    }
  );
});
