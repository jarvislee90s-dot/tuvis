import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterAll, afterEach, beforeAll, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import MessageComposer from "@/mobile/MessageComposer";
import { resetQueueControlMemoryForTests } from "@/mobile/OpenCodeNativeQueue";
import { resetActionAnchorMemoryForTests } from "@/mobile/opencodeActionAnchors";
import type { NativeCommand, PublicNativeView } from "@/mobile/opencodeNativeApi";
import { MockEventSource } from "./eventSourceMock";
import { pinZhNavigator } from "./pinZhNavigator";

const sessionId = "native-notice-session";
const bindingToken = "native-notice-binding";
const t = (key: string) => i18n.t(`mobile.openCodeNative.${key}`);
const lostConnection = () => new TypeError("Failed to fetch");
const allowed = { allowed: true, evidence: "contract", reasonCode: null };

let unpin: () => void;
beforeAll(async () => {
  unpin = await pinZhNavigator();
});
afterAll(() => unpin());
afterEach(() => {
  cleanup();
  MockEventSource.reset();
  resetActionAnchorMemoryForTests();
  resetQueueControlMemoryForTests();
  sessionStorage.clear();
  vi.unstubAllGlobals();
});

function openComposer({
  holdOutbox = false,
  running = false,
  pausedSnapshot = false,
  diagnostics = false,
} = {}) {
  let viewUnavailable = false;
  let rejectOutbox: ((error: Error) => void) | undefined;
  // 2026-11-22：快照暂停态只用于「暂停不再建框」的呈现断言；这里不再有前端可发的
  // 暂停/恢复控制（暂停入口与恢复按钮已删除）。
  const paused = pausedSnapshot;
  const posts: NativeCommand[] = [];
  const view: PublicNativeView = {
    sessionId,
    bindingToken,
    routeVersion: 1,
    revision: 1,
    complete: true,
    route: "opencode_native",
    routeReasonCode: diagnostics ? "OC_TRANSPORT_TIMEOUT" : null,
    capabilities: { send: allowed, stop_session: allowed },
    messages: [],
    pending: [],
    inbox: [],
    runtimeState: running ? "running" : "ready",
    files: {},
  };
  vi.stubGlobal("EventSource", MockEventSource);
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const path = new URL(String(input), "http://localhost").pathname;
      if (path.endsWith("/session-send-info")) {
        return Response.json({
          injectable: false,
          openCodeRouting: {
            policy: "native_only",
            nativeOwned: true,
            legacyAllowed: false,
            connection: "ready",
            reasonCode: null,
          },
        });
      }
      if (path.endsWith("/opencode/view")) {
        if (viewUnavailable) throw lostConnection();
        return Response.json(view);
      }
      if (path.endsWith("/opencode/outbox")) {
        if (holdOutbox) {
          return new Promise<Response>((_resolve, reject) => {
            rejectOutbox = reject;
          });
        }
        return Response.json({ sessionId, revision: paused ? 2 : 1, paused, items: [] });
      }
      if (path.endsWith("/opencode/agents")) return Response.json({ current: null, options: [] });
      if (path.endsWith("/opencode/actions")) {
        if (init?.method !== "POST") return Response.json([]);
        const command = JSON.parse(String(init.body)) as NativeCommand;
        posts.push(command);
        if (command.operation.kind === "stop_session") throw lostConnection();
      }
      throw new Error(`unexpected fetch: ${path} ${init?.method ?? "GET"}`);
    })
  );
  render(<MessageComposer session={{ id: sessionId, agentType: "opencode" }} />);
  return {
    posts,
    loseConnection: async () => {
      await waitFor(() => expect(rejectOutbox).toBeDefined());
      const events = MockEventSource.instances.find((source) =>
        source.url.includes("/opencode/events")
      );
      expect(events).toBeDefined();
      await act(async () => {
        viewUnavailable = true;
        events!.emit("revision", "2");
        rejectOutbox!(lostConnection());
      });
    },
  };
}

async function stopWithLostReceipt() {
  const { posts } = openComposer({ running: true });
  // 停止键现在是输入区那个唯一的图标键（用户口径「只有一个键」）：按 testid 定位，
  // 不再有独立的「停止当前回合」文字按钮。
  const button = await screen.findByTestId("native-stop");
  expect(button).toBeEnabled();
  fireEvent.click(button);
  const result = await screen.findByText(t("stop.unknown"));
  expect(result).toBeVisible();
  expect(posts.map(({ operation }) => operation.kind)).toEqual(["stop_session"]);
}

it("同一连接故障同时使会话与队列读失败时，仅保留一处连接通知", async () => {
  const { loseConnection, posts } = openComposer({ holdOutbox: true });
  await loseConnection();

  expect(await screen.findByText(t("errors.transportFailed"))).toBeVisible();
  expect(screen.getAllByText(t("errors.transportFailed"))).toHaveLength(1);
  expect(screen.getByTestId("native-retry")).toBeEnabled();
  expect(posts).toEqual([]);
  expect(screen.queryAllByText(t("outbox.readFailed"))).toHaveLength(0);
});

it("停止回执丢失时只解释一次停止未知，仍有只读核对入口", async () => {
  await stopWithLostReceipt();
  expect(screen.getAllByText(t("stop.unknown"))).toHaveLength(1);
  const stopRecovery = screen.getByRole("group", { name: /会话停止/ });
  expect(
    within(stopRecovery).getByRole("button", { name: t("unresolvedActions.verify") })
  ).toBeEnabled();
  expect(
    within(stopRecovery).queryAllByText(t("unresolvedActions.unknownDisposition"))
  ).toHaveLength(0);
});

it("只有停止未确认时，不显示上一条发送未知", async () => {
  await stopWithLostReceipt();
  expect(screen.queryByText(t("receipt.recovery"))).not.toBeInTheDocument();
  expect(screen.queryByText(t("receipt.unconfirmed"))).not.toBeInTheDocument();
});

it("native_only 重开停止未知的会话时，不指引退回旧输入区", async () => {
  await stopWithLostReceipt();
  cleanup();

  // 重挂后只剩恢复锚，不依赖即时回执是否负责通知。
  const { posts } = openComposer();
  expect(await screen.findByRole("group", { name: /会话停止/ })).toBeVisible();
  expect(posts).toEqual([]);
  expect(screen.queryByTestId("composer-input")).not.toBeInTheDocument();
  expect(screen.queryByRole("button", { name: t("route.returnLegacy") })).not.toBeInTheDocument();
  expect(screen.queryAllByText(/退回旧输入区/)).toHaveLength(0);
});

it("已暂停时不再有独立暂停框/恢复按钮（暂停只在发送键区提示）", async () => {
  const { posts } = openComposer({ diagnostics: true });
  await screen.findByTestId("native-input");
  // 2026-11-22 用户口径：暂停框、暂停入口、恢复按钮全部删除。
  expect(screen.queryByTestId("native-pause-entry")).toBeNull();
  expect(screen.queryByTestId("outbox-pause")).toBeNull();
  // 暂停框已删除：暂停相关文案键也一并退场（不留残句）
  expect(screen.queryByText(/本地派发已暂停/)).not.toBeInTheDocument();
  expect(screen.queryByText(/恢复本地派发/)).not.toBeInTheDocument();
  expect(posts).toEqual([]);
});

it("暂停快照到达时只在发送键上体现暂停，不建暂停框也不写控制请求", async () => {
  const { posts } = openComposer({ pausedSnapshot: true, diagnostics: true });
  await screen.findByTestId("native-input");
  // 暂停只体现在发送键本身（禁用 + 简短文案），没有独立说明块
  const send = await waitFor(() => {
    const button = screen.getByTestId("native-send");
    expect(button).toHaveAttribute("data-paused", "true");
    return button;
  });
  expect(send).toHaveTextContent(t("composer.pausedShort"));
  expect(send).toBeDisabled();
  expect(screen.queryByTestId("native-paused-note")).toBeNull();
  expect(screen.queryByTestId("native-pause-entry")).toBeNull();
  expect(screen.queryByTestId("outbox-pause")).toBeNull();
  expect(screen.queryByText(/本地派发已暂停/)).not.toBeInTheDocument();
  expect(posts).toEqual([]);
});

it("健康空闲时不显示空队列卡与通知", async () => {
  const { posts } = openComposer();
  await screen.findByTestId("native-input");
  await act(async () => {
    await waitFor(() =>
      expect(fetch).toHaveBeenCalledWith(
        expect.stringContaining("/opencode/outbox"),
        expect.anything()
      )
    );
  });
  expect(screen.queryByTestId("outbox-pause")).not.toBeInTheDocument();
  expect(screen.queryByTestId("native-connection-details")).not.toBeInTheDocument();
  expect(screen.queryByTestId("native-route-notice")).not.toBeInTheDocument();
  expect(screen.queryByTestId("native-outbox")).not.toBeInTheDocument();
  expect(screen.queryAllByRole("status")).toHaveLength(0);
  expect(screen.queryByText(t("errors.transportFailed"))).not.toBeInTheDocument();
  expect(posts).toEqual([]);
});
