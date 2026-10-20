// Codex 详情页接线的组件级测试（2026-10-20 复审 P2）：
// - 旧 session.status=idle/finished 不得隐藏 Codex 能力、pending、屏障与队列；
// - 队列刷新由**本地信号**驱动（投影 revision 变化 / 发送回执），不要求 Desktop 出新帧；
// - revision=null 的兼容模式同样成立；换会话不串台。
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import SessionDetail from "@/mobile/SessionDetail";
import type { Session } from "@/types/session";
import { MockEventSource } from "./eventSourceMock";

function makeSession(over: Partial<Session> = {}): Session {
  return {
    id: "sess-codex",
    agentType: "codex",
    projectName: "proj",
    projectPath: "/tmp/proj",
    title: "codex 会话",
    gitBranch: null,
    githubUrl: null,
    status: "idle",
    lastMessage: null,
    lastMessageRole: null,
    lastActivityAt: "2026-10-20T00:00:00Z",
    pid: 1,
    cpuUsage: 0,
    activeSubagentCount: 0,
    form: "app",
    jumpSupported: false,
    unread: false,
    ...over,
  };
}

function projection(over: Record<string, unknown> = {}) {
  return {
    threadId: "thread-1",
    revision: 5,
    stale: false,
    availability: "controllable",
    capabilities: {
      send: true,
      state: true,
      command: true,
      permissions: false,
      blockingInput: true,
    },
    runtimeStatus: "idle",
    contentState: "ready",
    messages: [],
    truncated: false,
    pending: [],
    ...over,
  };
}

let queueResponse: unknown;
let queueCalls: string[];
let retractBodies: string[];
let sendCalls: string[];

function json(body: unknown, status = 200) {
  return new Response(JSON.stringify(body), { status });
}

function installFetch() {
  queueResponse = { actions: [], blockedReasonCode: null };
  queueCalls = [];
  retractBodies = [];
  sendCalls = [];
  const fn = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    if (url.includes("/codex/queue/retract")) {
      retractBodies.push(String(init?.body ?? ""));
      return json({ actionId: "act-q", state: "cancelled", position: null, receipt: null });
    }
    if (url.includes("/codex/queue")) {
      queueCalls.push(url);
      return json(queueResponse);
    }
    if (url.includes("/codex/thread/events")) return new Response("", { status: 200 });
    if (url.includes("/codex/thread")) return json(projection());
    // 长路径先判：/session-send-info ⊃ /session-send
    if (url.includes("/session-send-info")) {
      return json({ injectable: true, channels: ["headless_codex_queue"], visibility: "realtime" });
    }
    if (url.includes("/session-send")) {
      sendCalls.push(String(init?.body ?? ""));
      return json({ status: "codex_queued", actionId: "act-q", position: 1 });
    }
    if (url.includes("/session-messages")) return json({ messages: [], truncated: false });
    if (url.includes("/session-files")) return json({ files: [], truncated: false });
    if (url.includes("/session-queue")) return json({ items: [] });
    if (url.includes("/session-approve-options")) {
      return json({
        available: false,
        options: [],
        verifiedWith: "t",
        currentVersion: null,
        drift: false,
      });
    }
    if (url.includes("/session-question")) return json({ available: false, questions: [] });
    if (url.includes("/session-subagents")) return json({ subagents: [] });
    if (url.includes("/session-open")) return json({ status: "opening" });
    return json({});
  });
  vi.stubGlobal("fetch", fn);
  return fn;
}

function emit(es: MockEventSource, type: string, body: unknown) {
  act(() => es.emit(type, body));
}

beforeEach(() => {
  MockEventSource.reset();
  vi.stubGlobal("EventSource", MockEventSource as unknown as typeof EventSource);
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("SessionDetail × Codex IPC：恢复入口与队列刷新", () => {
  it("A：legacy idle + Unknown 屏障 → 队列与「核对后继续」可见可操作", async () => {
    installFetch();
    queueResponse = {
      actions: [{ actionId: "a1", state: "unknown", position: null, receipt: null }],
      blockedReasonCode: "native_delivery_unknown",
      blockedActionIds: ["a1"],
    };
    render(<SessionDetail session={makeSession({ status: "idle" })} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    emit(es, "snapshot", projection());
    expect(await screen.findByTestId("codex-queue-panel")).toBeTruthy();
    const btn = await screen.findByTestId("codex-queue-force-continue");
    expect(btn).toBeTruthy();
    // 能力说明同样不被旧状态隐藏
    expect(screen.getByTestId("codex-availability-banner")).toBeTruthy();
  });

  it("B：idle 状态仍显示能力说明；有原生 pending 时不被旧状态隐藏", async () => {
    installFetch();
    render(<SessionDetail session={makeSession({ status: "finished" })} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    emit(
      es,
      "snapshot",
      projection({
        pending: [
          {
            requestKey: "req-1",
            contextToken: "tok-1",
            kind: "command_approval",
            actionable: true,
            disabledReasonCode: null,
            command: "npm test",
            cwd: "/tmp/proj",
            reason: null,
            permissionSummary: null,
            questions: [],
          },
        ],
      })
    );
    expect(await screen.findByTestId("codex-availability-banner")).toBeTruthy();
    expect(await screen.findByTestId("codex-pending-command-text")).toHaveTextContent("npm test");
  });

  it("C：revision 固定 + 首发队列为空 → 发送 queued 后条目出现，撤回打对 actionId", async () => {
    installFetch();
    render(<SessionDetail session={makeSession({ status: "idle" })} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    emit(es, "snapshot", projection({ revision: 5 }));
    // 首拉为空（面板自隐）
    await waitFor(() => expect(queueCalls.length).toBeGreaterThan(0));
    expect(screen.queryByTestId("codex-queue-panel")).toBeNull();

    // 发送回执 queued：本地信号刷新队列（Desktop revision 未变）
    queueResponse = {
      actions: [{ actionId: "act-q", state: "queued", position: 1, receipt: null }],
      blockedReasonCode: null,
    };
    const input = await screen.findByTestId("composer-input");
    fireEvent.change(input, { target: { value: "排队一条" } });
    const sendBtn = screen.getByTestId("composer-send") as HTMLButtonElement;
    expect(sendBtn.disabled).toBe(false);
    fireEvent.click(sendBtn);
    await waitFor(() => expect(sendCalls.length).toBe(1), { timeout: 3000 });
    expect(
      await screen.findByTestId("send-receipt-codex-queued", undefined, { timeout: 3000 })
    ).toBeTruthy();
    const row = await screen.findByTestId("codex-queue-row-act-q");
    expect(row).toBeTruthy();

    fireEvent.click(await screen.findByTestId("codex-queue-retract-act-q"));
    await waitFor(() => expect(retractBodies.length).toBe(1));
    expect(JSON.parse(retractBodies[0])).toEqual({ actionId: "act-q" });
  });

  it("D：revision=null 的兼容模式同样能显示队列与撤回入口", async () => {
    installFetch();
    queueResponse = {
      actions: [{ actionId: "compat-a", state: "queued", position: 1, receipt: null }],
      blockedReasonCode: "compat_outstanding",
      blockedActionIds: [],
    };
    render(<SessionDetail session={makeSession({ status: "idle" })} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    emit(es, "snapshot", projection({ revision: null, stale: true, availability: "compat_only" }));
    expect(await screen.findByTestId("codex-queue-row-compat-a")).toBeTruthy();
    expect(screen.getByTestId("codex-queue-retract-compat-a")).toBeTruthy();
    expect(screen.getByTestId("codex-stale-note")).toBeTruthy();
  });

  it("E：切会话后旧队列/旧投影不串入新会话", async () => {
    installFetch();
    queueResponse = {
      actions: [{ actionId: "old-a", state: "queued", position: 1, receipt: null }],
      blockedReasonCode: null,
    };
    const { rerender } = render(
      <SessionDetail session={makeSession({ id: "sess-a" })} onBack={() => {}} />
    );
    const esA = await waitFor(() => MockEventSource.latest());
    emit(esA, "snapshot", projection());
    expect(await screen.findByTestId("codex-queue-row-old-a")).toBeTruthy();

    queueResponse = { actions: [], blockedReasonCode: null };
    rerender(<SessionDetail session={makeSession({ id: "sess-b" })} onBack={() => {}} />);
    await waitFor(() => expect(queueCalls.some((u) => u.includes("sess-b"))).toBe(true));
    expect(screen.queryByTestId("codex-queue-row-old-a")).toBeNull();
  });
});
