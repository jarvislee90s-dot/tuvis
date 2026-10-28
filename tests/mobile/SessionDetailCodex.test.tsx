// Codex 详情页接线的组件级测试（2026-10-20 复审 P2）：
// - 旧 session.status=idle/finished 不得隐藏 Codex 能力、pending、屏障与队列；
// - 队列刷新由**本地信号**驱动（投影 revision 变化 / 发送回执），不要求 Desktop 出新帧；
// - revision=null 的兼容模式同样成立；换会话不串台。
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
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
let fetchMock: ReturnType<typeof vi.fn> | null = null;
let retractBodies: string[];
let sendCalls: string[];
let nextSend: unknown;
let afterSendQueue: unknown;
let compatSent = false;
/** F8：send-info 响应（可变——打断受理/设置翻转后服务端状态变化的注入点） */
let sendInfoResponse: unknown;
let interruptResponse: unknown;

function json(body: unknown, status = 200) {
  return new Response(JSON.stringify(body), { status });
}

function installFetch() {
  queueResponse = { actions: [], blockedReasonCode: null };
  queueCalls = [];
  retractBodies = [];
  sendCalls = [];
  nextSend = null;
  afterSendQueue = null;
  compatSent = false;
  sendInfoResponse = {
    injectable: true,
    channels: ["headless_codex_queue"],
    visibility: "realtime",
  };
  interruptResponse = null;
  const fn = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    if (url.includes("/codex/queue/retract")) {
      retractBodies.push(String(init?.body ?? ""));
      return json({ actionId: "act-q", state: "cancelled", position: null, receipt: null });
    }
    if (url.includes("/codex/queue")) {
      queueCalls.push(url);
      return json(compatSent && afterSendQueue ? afterSendQueue : queueResponse);
    }
    if (url.includes("/codex/turn/interrupt")) {
      return json(
        interruptResponse ?? {
          ok: true,
          uncertain: false,
          interruptedTurnId: "t-9",
          expectedTurnId: "t-9",
        }
      );
    }
    if (url.includes("/codex/thread/events")) return new Response("", { status: 200 });
    if (url.includes("/codex/thread")) return json(projection());
    // 长路径先判：/session-send-info ⊃ /session-send
    if (url.includes("/session-send-info")) {
      return json(sendInfoResponse);
    }
    if (url.includes("/session-send")) {
      sendCalls.push(String(init?.body ?? ""));
      if (nextSend) {
        compatSent = true;
        return json(nextSend);
      }
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
  fetchMock = fn;
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
    // R16：App 会话不再挂"总能力徽标"（逐操作工具栏承担解释）；队列入口仍在。
    expect(screen.queryByTestId("codex-availability-banner")).toBeNull();
    expect(screen.getByTestId("codex-ops-row")).toBeTruthy();
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
    // R16：App 会话无旧总徽标；原生 pending 卡照常出现。
    expect(screen.queryByTestId("codex-availability-banner")).toBeNull();
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

  it("R02：空暂停下发送入队后立即重拉 send-info 并显示暂停恢复提示", async () => {
    installFetch();
    render(<SessionDetail session={makeSession({ status: "idle" })} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    emit(es, "snapshot", projection({ revision: 5 }));
    const input = await screen.findByTestId("composer-input");
    // 发送前：线程没有旧队列，暂停条自隐。
    expect(screen.queryByTestId("send-queue-paused")).toBeNull();
    // 发送回执 queued 后，服务端仍有持久暂停；必须立即显示“恢复后发送”。
    queueResponse = {
      actions: [{ actionId: "act-q", state: "queued", position: 1, receipt: null }],
      blockedReasonCode: null,
    };
    sendInfoResponse = {
      injectable: true,
      channels: ["headless_codex_queue"],
      visibility: "realtime",
      queuePaused: true,
    };
    fireEvent.change(input, { target: { value: "排队一条" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    await screen.findByTestId("send-receipt-codex-queued", undefined, { timeout: 3000 });
    await waitFor(() => expect(screen.getByTestId("send-queue-paused")).toBeTruthy());
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

  it("F：revision=null 兼容超时 → 不离开页面也能看到屏障与恢复入口", async () => {
    installFetch();
    nextSend = {
      status: "headless",
      receipt: { status: "timeout", reason: "compat 回合超时（结果未知）" },
    };
    afterSendQueue = {
      actions: [{ actionId: "act-to", state: "unknown", position: null, receipt: null }],
      blockedReasonCode: "native_delivery_unknown",
      blockedActionIds: ["act-to"],
    };
    render(<SessionDetail session={makeSession({ status: "idle" })} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    emit(es, "snapshot", projection({ revision: null, stale: true, availability: "compat_only" }));
    const input = await screen.findByTestId("composer-input");
    fireEvent.change(input, { target: { value: "兼容超时消息" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    expect(await screen.findByTestId("send-receipt-headless")).toBeTruthy();
    // 没有任何新投影帧：队列刷新由 headless 回执独立触发
    expect(await screen.findByTestId("codex-queue-panel")).toBeTruthy();
    expect(await screen.findByTestId("codex-queue-force-continue")).toBeTruthy();
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

// ============================================================
// 复审 R6（F8 前端半边）：后端真实状态变化驱动手机刷新权威 send-info。
// 纪律（复审明文）：先以 queuePaused=false 挂载，再通过真实操作转 true——
// 不可渲染前预置；availability 帧用后端 r6_toggle 生产 republish 的真实形状
//（SSE snapshot/state 帧 = ThreadProjection 全量，availability 随 caps 翻转），
// 不得手工构造生产不存在的帧。同页与其他设备起源均覆盖：
// - 同页：队列面板打断受理（现有 onQueueStateChanged 链）；
// - 跨设备（本测重点）：设备 B 打断暂停同线程 → 设备 A 经 SSE 收到终态 revision
//   的投影帧（生产下游帧）→ composer 重拉 send-info → 「本条先发送」出现。
// ============================================================
describe("SessionDetail × Codex IPC：R6 后端真实状态变化驱动 send-info 刷新", () => {
  it("R6-1 同页：先 queuePaused=false 挂载 → 队列面板打断受理（真实操作）→ 二选一出现", async () => {
    installFetch();
    queueResponse = {
      actions: [{ actionId: "a1", state: "queued", position: 1, receipt: null }],
      blockedReasonCode: "native_turn_pending",
      canInterrupt: true,
    };
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    emit(es, "snapshot", projection());
    // 挂载时（打断前）：无暂停条——queuePaused=false 的真实初值（复审明文：
    // 不可渲染前预置 true；打断是「真实操作」，服务端暂停在其受理后才成立）
    await screen.findByTestId("composer-input");
    expect(screen.queryByTestId("send-queue-paused")).toBeNull();
    // 打断受理后服务端置持久暂停（send-info 随之翻转——真实操作后的服务端真值）
    sendInfoResponse = {
      injectable: true,
      channels: ["headless_codex_queue"],
      visibility: "realtime",
      queuePaused: true,
    };
    // 打断两步确认（破坏性动作的前端防线）→ 受理
    fireEvent.click(await screen.findByTestId("codex-queue-interrupt"));
    fireEvent.click(await screen.findByTestId("codex-queue-interrupt-yes"));
    await screen.findByTestId("codex-queue-note");
    // R6：composer 同页感知暂停（无需重进）——二选一出现
    expect(await screen.findByTestId("send-queue-paused")).toBeTruthy();
    // 显式先发新意见：arm 一次性越队 → 发送请求带 bypassPausedQueue
    fireEvent.click(screen.getByTestId("queue-bypass-arm"));
    expect(screen.getByTestId("queue-bypass-armed")).toBeTruthy();
    const input = screen.getByTestId("composer-input") as HTMLTextAreaElement;
    fireEvent.change(input, { target: { value: "打断期间的新意见" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    await waitFor(() => expect(sendCalls.length).toBeGreaterThan(0));
    const body = JSON.parse(sendCalls[sendCalls.length - 1]) as Record<string, unknown>;
    expect(body.bypassPausedQueue).toBe(true);
    expect(body.text).toBe("打断期间的新意见");
  });

  it("R6-2 跨设备：设备 B 打断暂停同线程 → 设备 A 经 SSE 收到终态 revision 投影帧（生产下游帧）→ composer 重拉 send-info → 先发出口出现", async () => {
    installFetch();
    queueResponse = { actions: [], blockedReasonCode: null };
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    emit(es, "snapshot", projection({ revision: 5 }));
    // 挂载时（B 打断前）：无暂停条（queuePaused=false 初值）
    await screen.findByTestId("composer-input");
    expect(screen.queryByTestId("send-queue-paused")).toBeNull();
    // 设备 B 打断并暂停同线程：服务端状态两处变化——①队列视图 queuePaused=true
    // ②send-info queuePaused=true（同一持久暂停的两个消费面）
    queueResponse = {
      actions: [{ actionId: "a1", state: "queued", position: 1, receipt: null }],
      blockedReasonCode: "native_turn_pending",
      canInterrupt: false,
    };
    sendInfoResponse = {
      injectable: true,
      channels: ["headless_codex_queue"],
      visibility: "realtime",
      queuePaused: true,
    };
    // 设备 A 收到生产 SSE 帧（终态 revision 推进——队列刷新的既有触发源；
    // ThreadProjection 全量帧 = r6_toggle 同一形状，无手工字段）
    emit(es, "state", projection({ revision: 6 }));
    // R6：队列面板刷新（既有链）+ composer send-info 刷新（本验收）→ 先发出口
    expect(await screen.findByTestId("send-queue-paused")).toBeTruthy();
    // 显式先发新意见可达（旧队列继续暂停由服务端保证）
    fireEvent.click(screen.getByTestId("queue-bypass-arm"));
    expect(screen.getByTestId("queue-bypass-armed")).toBeTruthy();
  });

  it("R6-3 关闭/重开：重发布帧只翻 availability（revision 不变——r6_toggle 同款帧）→ composer 触发 send-info 重拉", async () => {
    installFetch();
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    // 初帧：controllable（打开态；生产首快照形状）
    emit(es, "snapshot", projection({ revision: 5, availability: "controllable" }));
    const input = (await screen.findByTestId("composer-input")) as HTMLTextAreaElement;
    expect(input.disabled).toBe(false);
    // 设置页关闭自动适配 → 后端 48b371d8 effective_projection_caps 生产 republish：
    // **只翻 availability**（controllable → read_only；revision/pending/runtime 与
    // 前帧逐字相同——重发布不推进 revision），send-info 只能靠此帧驱动重拉
    sendInfoResponse = {
      injectable: false,
      reasonCode: "capability_unverified",
      reason: "该版本暂不能远程发送",
    };
    emit(es, "state", projection({ revision: 5, availability: "read_only" }));
    // composer 重拉 send-info（静态门拒绝）→ 输入区置灰 + 原因码
    await waitFor(() => {
      expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).disabled).toBe(true);
    });
    expect(screen.getByTestId("send-disabled-reason").textContent).toContain(
      "capability_unverified"
    );
    // 重开 → availability 翻回（生产 republish 第二帧，revision 仍不变）→ 解禁
    sendInfoResponse = {
      injectable: true,
      channels: ["headless_codex_queue"],
      visibility: "realtime",
    };
    emit(es, "state", projection({ revision: 5, availability: "controllable" }));
    await waitFor(() => {
      expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).disabled).toBe(false);
    });
  });

  it("R6-3 对照：同帧形状但 availability 不翻（纯 revision 推进之外的重复帧）→ 不触发 send-info 重拉（签名不误触发）", async () => {
    installFetch();
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    emit(es, "snapshot", projection({ revision: 5, availability: "controllable" }));
    await screen.findByTestId("composer-input");
    // 置一个「若被重拉就会置灰」的响应，但本帧 availability 原样（controllable）
    // ——签名无变化，重拉不得发生（否则同 availability 的重复帧/乱序帧会抖动输入区）
    sendInfoResponse = {
      injectable: false,
      reasonCode: "capability_unverified",
      reason: "该版本暂不能远程发送",
    };
    const infoCallsBefore = fetchMock.mock.calls.filter(([u]) =>
      String(u).includes("/session-send-info")
    ).length;
    emit(es, "state", projection({ revision: 5, availability: "controllable" }));
    await act(async () => {
      await new Promise((r) => setTimeout(r, 450)); // 越过去抖窗（250ms）+ 余量
    });
    const infoCallsAfter = fetchMock.mock.calls.filter(([u]) =>
      String(u).includes("/session-send-info")
    ).length;
    expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).disabled).toBe(false);
    expect(screen.queryByTestId("send-disabled-reason")).toBeNull();
    // send-info 调用数不变——签名未变化就不得重拉（挂载 1 次）
    expect(infoCallsAfter).toBe(infoCallsBefore);
  });
});

// ============================================================
// 复审 R-T4 前端半边：队列暂停有自己的可靠传播——T4 后端把 queuePaused 纳入
// ThreadProjection，并在每个暂停变化点（前置落行/撤回/resume/settle）重发布该
// 线程投影（local_seq 决胜穿透，Desktop 零新帧也必达 SSE 下游）。
// 前端义务：帧签名纳入 queuePaused。真实 republish 帧 = **只翻 queuePaused**，
// revision/availability/runtimeStatus/pending 与前帧逐字不变（重发布不推进
// revision）——旧签名（availability|runtimeStatus|pending|revision）检测不到任何
// 变化 → 不重拉 send-info → 设备 A 永远看不到暂停（复审原文的缺陷场景：
// B 打断 RPC 超时 UnknownDelivery、连接健康、Desktop 无新状态帧）。
// 纪律同 R6：先以 queuePaused=false 挂载（不可预置 true）；帧 = 生产 ThreadProjection
// 全量形状；零轮询零 Tauri channel。
// ============================================================
describe("SessionDetail × Codex IPC：R-T4 投影 queuePaused 驱动 send-info 刷新", () => {
  it("T4-1 跨设备暂停：queuePaused=false 挂载 → SSE 帧只翻 queuePaused=true（其余字段不变）→ composer 重拉 send-info → 恢复/先发入口出现", async () => {
    installFetch();
    queueResponse = { actions: [], blockedReasonCode: null };
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    // 生产首快照形状：T4 后端恒序列化 queuePaused，首快照如实 false
    emit(es, "snapshot", projection({ revision: 5, queuePaused: false }));
    await screen.findByTestId("composer-input");
    // 挂载时（B 打断前）：无暂停条
    expect(screen.queryByTestId("send-queue-paused")).toBeNull();
    const infoCallsBefore = fetchMock!.mock.calls.filter(([u]) =>
      String(u).includes("/session-send-info")
    ).length;
    // 设备 B 打断暂停同线程（RPC 超时 UnknownDelivery、连接健康、Desktop 零新帧）
    // → T4 republish 帧：**只翻 queuePaused**，revision/availability/runtime/pending
    // 与前帧逐字不变——这正是复审明文所禁的手工双变化帧的反面（生产真实形状）
    sendInfoResponse = {
      injectable: true,
      channels: ["headless_codex_queue"],
      visibility: "realtime",
      queuePaused: true,
    };
    emit(es, "state", projection({ revision: 5, queuePaused: true }));
    // 帧签名含 queuePaused → 去抖后 composer 重拉 send-info → 二选一出现
    expect(
      await screen.findByTestId("send-queue-paused", undefined, { timeout: 2000 })
    ).toBeTruthy();
    expect(screen.getByTestId("queue-resume")).toBeTruthy();
    expect(screen.getByTestId("queue-bypass-arm")).toBeTruthy();
    // 重拉确实发生（composer send-info 调用数前进，而非别的机制碰巧出条）
    const infoCallsAfter = fetchMock!.mock.calls.filter(([u]) =>
      String(u).includes("/session-send-info")
    ).length;
    expect(infoCallsAfter).toBeGreaterThan(infoCallsBefore);
  });

  it("T4-2 resume 反向帧：queuePaused=true 挂载 → SSE 帧只翻 queuePaused=false → composer 重拉 send-info → 二选一消失", async () => {
    installFetch();
    queueResponse = { actions: [], blockedReasonCode: null };
    sendInfoResponse = {
      injectable: true,
      channels: ["headless_codex_queue"],
      visibility: "realtime",
      queuePaused: true,
    };
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    emit(es, "snapshot", projection({ revision: 5, queuePaused: true }));
    // 暂停中：二选一在场
    expect(await screen.findByTestId("send-queue-paused")).toBeTruthy();
    // 设备 B 显式恢复（跨设备起源）→ T4 resume republish 反向帧：只翻 queuePaused
    sendInfoResponse = {
      injectable: true,
      channels: ["headless_codex_queue"],
      visibility: "realtime",
    };
    emit(es, "state", projection({ revision: 5, queuePaused: false }));
    await waitFor(() => expect(screen.queryByTestId("send-queue-paused")).toBeNull(), {
      timeout: 2000,
    });
    // 恢复后输入区不被误置灰
    expect((screen.getByTestId("composer-input") as HTMLTextAreaElement).disabled).toBe(false);
  });
});

// ============================================================
// H12：来源随形态 + 待处理收纳（去重后一行入口）
// ============================================================

describe("H12 来源与收纳", () => {
  beforeEach(() => {
    installFetch();
    MockEventSource.reset?.();
  });
  afterEach(() => {
    cleanup();
    vi.unstubAllGlobals();
  });

  it("h12_cli_form_never_subscribes_desktop_projection", async () => {
    const calls: string[] = [];
    const base = fetchMock!;
    vi.stubGlobal("fetch", (input: RequestInfo | URL, init?: RequestInit) => {
      calls.push(String(input));
      return base(input, init);
    });
    render(<SessionDetail session={makeSession({ form: "cli" })} onBack={() => {}} />);
    await act(async () => {
      await Promise.resolve();
    });
    expect(calls.some((url) => url.includes("/codex/thread"))).toBe(false);
    expect(screen.queryByTestId("codex-ops-row")).toBeNull();
  });

  it("h12_multiple_pending_collapse_into_one_row_and_expand", async () => {
    installFetch();
    const pendingFixture = [
      {
        requestKey: "n:1",
        contextToken: "tok-a",
        kind: "command_approval",
        actionable: true,
        allowedDecisions: ["accept", "decline"],
        command: "pnpm test",
        cwd: "/w",
      },
      {
        requestKey: "n:2",
        contextToken: "tok-b",
        kind: "file_change_approval",
        actionable: false,
        disabledReasonCode: "operation_contract_unmatched",
        allowedDecisions: [],
      },
      // 同一 requestKey 重复（投影重复帧）→ 去重只算一条
      {
        requestKey: "n:1",
        contextToken: "tok-a",
        kind: "command_approval",
        actionable: true,
        allowedDecisions: ["accept", "decline"],
        command: "pnpm test",
        cwd: "/w",
      },
    ];
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    emit(es, "snapshot", projection({ pending: pendingFixture }));
    // 新裁决：不再收纳折叠，去重后的两张有效 pending 卡直接停在底部。
    const dock = await screen.findByTestId("codex-dock-cards");
    expect(screen.queryByTestId("codex-inbox-row")).toBeNull();
    expect(within(dock).getAllByTestId("codex-pending-card")).toHaveLength(2);
    expect(dock.querySelector("[hidden]")).toBeNull();
  });
});

// ============================================================
// H12（第三轮）：收纳只隐藏不卸载——草稿与动作锚保留
// ============================================================

describe("H12 收纳保留状态", () => {
  beforeEach(() => {
    installFetch();
  });
  afterEach(() => {
    cleanup();
    vi.unstubAllGlobals();
  });

  function asyncQuestion(key: string) {
    return {
      questionKey: key,
      contextToken: `${key}-ctx`,
      sourceTurnId: "t1",
      sourceItemId: key,
      questionIndex: 0,
      title: "选一个",
      options: ["甲"],
      state: "pending",
      actionable: true,
      disabledReasonCode: null,
    };
  }

  it("h12_second_question_arrival_keeps_first_draft", async () => {
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    emit(es, "snapshot", projection({ asyncQuestions: [asyncQuestion("q1")] }));
    const dock = await screen.findByTestId("codex-dock-cards");
    const area = (await within(dock).findByRole("textbox")) as HTMLTextAreaElement;
    fireEvent.change(area, { target: { value: "草稿保留" } });
    // 第二组到来：两张卡都停靠，第一组仍挂载，草稿不丢。
    emit(
      es,
      "snapshot",
      projection({
        revision: 6,
        asyncQuestions: [asyncQuestion("q1"), asyncQuestion("q2")],
      })
    );
    await waitFor(() => expect(screen.getAllByTestId("codex-async-question")).toHaveLength(2));
    const areas = within(screen.getByTestId("codex-dock-cards")).getAllByRole(
      "textbox"
    ) as HTMLTextAreaElement[];
    expect(areas.some((el) => el.value === "草稿保留")).toBe(true);
    expect(screen.queryByTestId("codex-inbox-row")).toBeNull();
  });
});

// ============================================================
// B04/B03：同组多题单卡 + 历史待核对收纳
// ============================================================

describe("B04/B03 分组与历史", () => {
  beforeEach(() => {
    installFetch();
  });
  afterEach(() => {
    cleanup();
    window.localStorage.clear();
    vi.unstubAllGlobals();
  });

  function q(
    key: string,
    index: number,
    state = "pending",
    item = "call-g"
  ): Record<string, unknown> {
    return {
      questionKey: `["request_user_input_async","${item}",${index}]`,
      contextToken: `${key}-ctx`,
      sourceTurnId: "t1",
      sourceItemId: item,
      questionIndex: index,
      title: `问题${index + 1}`,
      options: ["甲"],
      state,
      actionable: state === "pending",
      disabledReasonCode: null,
    };
  }

  it("b04_five_questions_show_one_card_with_progress", async () => {
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    emit(es, "snapshot", projection({ asyncQuestions: [0, 1, 2, 3, 4].map((i) => q(`q${i}`, i)) }));
    const dock = screen.getByTestId("codex-dock-cards");
    const cards = within(dock).getAllByTestId("codex-async-question");
    expect(cards).toHaveLength(1);
    expect(cards[0].getAttribute("data-progress")).toBe("1/5");
    expect(screen.queryByText("问题2")).toBeNull();
  });

  it("b03_answered_and_unknown_are_not_rendered_without_local_action", async () => {
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    emit(
      es,
      "snapshot",
      projection({
        asyncQuestions: [
          q("q0", 0, "answered"),
          q("q1", 1, "unknown", "call-old"),
          q("q2", 2, "pending", "call-cur"),
        ],
      })
    );
    const cards = await screen.findAllByTestId("codex-async-question");
    // 只有 pending 组；已答组与无本地动作的 unknown 组都不出现。
    expect(cards).toHaveLength(1);
    expect(cards[0].getAttribute("data-progress")).toBe("1/1");
    expect(screen.queryByTestId("codex-async-history")).toBeNull();
    expect(screen.queryByTestId("codex-history-toggle")).toBeNull();
    expect(screen.queryByTestId("codex-inbox-row")).toBeNull();
  });
});

describe("G02 有效组直接堆叠", () => {
  beforeEach(() => installFetch());
  afterEach(() => {
    cleanup();
    window.localStorage.clear();
    vi.unstubAllGlobals();
  });

  function gq(item: string, index: number, state: string) {
    return {
      questionKey: `["request_user_input_async","${item}",${index}]`,
      contextToken: `${item}-${index}-ctx`,
      sourceTurnId: "t1",
      sourceItemId: item,
      questionIndex: index,
      title: `${item}-${index}`,
      options: ["甲"],
      state,
      actionable: state === "pending",
      disabledReasonCode: null,
    };
  }

  it("g02_all_live_groups_show_without_tabs_or_history", async () => {
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    emit(
      es,
      "snapshot",
      projection({
        asyncQuestions: [
          gq("old", 0, "pending"),
          gq("old", 1, "pending"),
          gq("new", 0, "pending"),
          gq("new", 1, "pending"),
        ],
      })
    );
    // 一张展开 + 另一组一行；其余组卡片仍挂载但 hidden，草稿不丢。
    await waitFor(() => expect(screen.getAllByTestId("codex-group-row")).toHaveLength(1));
    const oldPanel = screen.getByTestId("codex-group-panel-t1|old");
    const newPanel = screen.getByTestId("codex-group-panel-t1|new");
    expect(oldPanel.hidden).toBe(false);
    expect(newPanel.hidden).toBe(true);
    expect(within(oldPanel).getByText("old-0")).toBeTruthy();
    expect(screen.queryByTestId("codex-group-tab-0")).toBeNull();
    expect(screen.queryByTestId("codex-async-history")).toBeNull();
    fireEvent.click(screen.getByTestId("codex-group-row"));
    await waitFor(() => expect(newPanel.hidden).toBe(false));
    expect(oldPanel.hidden).toBe(true);
    expect(within(newPanel).getByText("new-0")).toBeTruthy();
  });

  it("g02_unknown_without_local_action_is_not_rendered", async () => {
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    emit(
      es,
      "snapshot",
      projection({
        asyncQuestions: [
          gq("old", 0, "unknown"),
          gq("old", 1, "unknown"),
          gq("new", 0, "pending"),
          gq("new", 1, "pending"),
        ],
      })
    );
    await waitFor(() => expect(screen.getByText("new-0")).toBeTruthy());
    expect(screen.queryByText("old-0")).toBeNull();
    expect(screen.queryByTestId("codex-async-history")).toBeNull();
  });
});

describe("未知题目的停靠口径（2026-10-28 裁决）", () => {
  beforeEach(() => installFetch());
  afterEach(() => {
    cleanup();
    window.localStorage.clear();
    window.sessionStorage.clear();
    vi.unstubAllGlobals();
  });

  function aq(index: number, state: string, token = "t1") {
    return {
      questionKey: `["request_user_input_async","call-x",${index}]`,
      contextToken: `${token}-${index}`,
      sourceTurnId: "t1",
      sourceItemId: "call-x",
      questionIndex: index,
      title: `题目${index + 1}`,
      options: ["甲", "乙"],
      state,
      actionable: state === "pending",
      disabledReasonCode: null,
    };
  }

  it("unknown_without_local_action_is_hidden_not_history", async () => {
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    emit(es, "snapshot", projection({ asyncQuestions: [aq(0, "unknown"), aq(1, "unknown")] }));
    await waitFor(() => expect(screen.getByTestId("codex-dock-cards")).toBeTruthy());
    expect(screen.queryByTestId("codex-async-question")).toBeNull();
    expect(screen.queryByTestId("codex-async-history")).toBeNull();
    expect(screen.queryByTestId("codex-history-toggle")).toBeNull();
  });

  it("answered_with_local_anchor_is_auto_closed_and_anchor_cleared", async () => {
    window.sessionStorage.setItem(`codex:group:${makeSession().id}:t1:call-x`, "act-anchor");
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    emit(es, "snapshot", projection({ asyncQuestions: [aq(0, "answered"), aq(1, "answered")] }));
    await waitFor(() => expect(screen.queryByTestId("codex-async-question")).toBeNull());
    expect(window.sessionStorage.getItem(`codex:group:${makeSession().id}:t1:call-x`)).toBeNull();
    expect(screen.queryByTestId("codex-async-history")).toBeNull();
  });

  it("unknown_with_local_action_stays_for_readonly_verify", async () => {
    window.sessionStorage.setItem(`codex:group:${makeSession().id}:t1:call-x`, "act-anchor");
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    emit(es, "snapshot", projection({ asyncQuestions: [aq(0, "unknown")] }));
    const card = await screen.findByTestId("codex-async-question");
    expect(card.getAttribute("data-phase")).toBe("unknown");
    expect(screen.getByTestId("codex-async-verify")).toBeTruthy();
    expect(screen.queryByTestId("codex-async-fallback")).toBeNull();
  });

  it("pending_to_unknown_without_action_removes_card", async () => {
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    emit(es, "snapshot", projection({ asyncQuestions: [aq(0, "pending")] }));
    expect(await screen.findByTestId("codex-async-free")).toBeTruthy();
    emit(es, "snapshot", projection({ revision: 7, asyncQuestions: [aq(0, "unknown")] }));
    await waitFor(() => expect(screen.queryByTestId("codex-async-question")).toBeNull());
  });
});
