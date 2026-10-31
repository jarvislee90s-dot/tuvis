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
/** T05：POST /codex/turn/interrupt 请求体捕获（共享控制器的冻结目标断言用） */
let interruptBodies: string[];
/** T05：interrupt 挂起放行器（stopping 窗断言用） */
let releaseInterrupt: ((r: Response) => void) | null = null;

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
  interruptBodies = [];
  releaseInterrupt = null;
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
      interruptBodies.push(String(init?.body ?? ""));
      if (interruptResponse === "hang") {
        return new Promise<Response>((resolve) => {
          releaseInterrupt = resolve;
        });
      }
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
    // C04：面板默认折叠——行级断言先展开
    fireEvent.click(await screen.findByTestId("codex-queue-toggle"));
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
    // C04：面板折叠摘要出现，展开后行可见
    fireEvent.click(await screen.findByTestId("codex-queue-toggle"));
    const row = await screen.findByTestId("codex-queue-row-act-q");
    expect(row).toBeTruthy();

    fireEvent.click(await screen.findByTestId("codex-queue-retract-act-q"));
    await waitFor(() => expect(retractBodies.length).toBe(1));
    expect(JSON.parse(retractBodies[0])).toEqual({ actionId: "act-q" });
  });

  it("R02：空暂停下发送入队后立即重拉 send-info，暂停展示收敛到队列区摘要", async () => {
    installFetch();
    render(<SessionDetail session={makeSession({ status: "idle" })} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    emit(es, "snapshot", projection({ revision: 5 }));
    const input = await screen.findByTestId("composer-input");
    // 发送前：线程没有旧队列，composer 与队列区都不出暂停件。
    expect(screen.queryByTestId("send-queue-paused")).toBeNull();
    // 发送回执 queued 后，服务端仍有持久暂停；暂停解释只由队列区摘要承担。
    queueResponse = {
      actions: [{ actionId: "act-q", state: "queued", position: 1, receipt: null }],
      blockedReasonCode: null,
      queuePaused: true,
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
    // C04：composer 不再渲染暂停二选一（解释/恢复归队列区）
    expect(screen.queryByTestId("send-queue-paused")).toBeNull();
    // 队列区摘要如实显示「已暂停」
    await waitFor(() =>
      expect(screen.getByTestId("codex-queue-summary").getAttribute("data-paused")).toBe("true")
    );
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
    // C04：面板默认折叠——行级断言先展开
    fireEvent.click(await screen.findByTestId("codex-queue-toggle"));
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
    // C04：面板默认折叠——操作断言先展开
    fireEvent.click(await screen.findByTestId("codex-queue-toggle"));
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
    // C04：面板默认折叠——行级断言先展开
    fireEvent.click(await screen.findByTestId("codex-queue-toggle"));
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
  it("R6-1 同页：先 queuePaused=false 挂载 → composer 停止受理（真实操作）→ 暂停展示收敛到队列区摘要", async () => {
    installFetch();
    queueResponse = {
      actions: [{ actionId: "a1", state: "queued", position: 1, receipt: null }],
      blockedReasonCode: "native_turn_pending",
      canInterrupt: true,
    };
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    emit(
      es,
      "snapshot",
      projection({
        runtimeStatus: "active",
        activeTurnId: "turn-A",
        effectiveOperations: interruptAllowedOps(),
      })
    );
    // 挂载时（打断前）：无暂停件——queuePaused=false 的真实初值（复审明文：
    // 不可渲染前预置 true；打断是「真实操作」，服务端暂停在其受理后才成立）
    await screen.findByTestId("composer-input");
    expect(screen.queryByTestId("send-queue-paused")).toBeNull();
    // 打断受理后服务端置持久暂停（send-info 与队列视图随之翻转——真实操作后的
    // 服务端真值；队列视图的 queuePaused 是面板摘要的数据源）
    sendInfoResponse = {
      injectable: true,
      channels: ["headless_codex_queue"],
      visibility: "realtime",
      queuePaused: true,
    };
    queueResponse.queuePaused = true;
    fireEvent.click(await screen.findByTestId("codex-queue-toggle"));
    expect(screen.queryByTestId("codex-queue-interrupt")).toBeNull();
    fireEvent.click(await screen.findByTestId("composer-stop"));
    await waitFor(() => expect(interruptBodies).toHaveLength(1));
    await waitFor(() =>
      expect(screen.getByTestId("codex-queue-summary").getAttribute("data-paused")).toBe("true")
    );
    expect(screen.queryByTestId("send-queue-paused")).toBeNull();
    expect(screen.queryByTestId("queue-bypass-arm")).toBeNull();
  });

  it("R6-2 跨设备：设备 B 打断暂停同线程 → 设备 A 经 SSE 收到终态 revision 投影帧（生产下游帧）→ 暂停展示收敛到队列区摘要", async () => {
    installFetch();
    queueResponse = { actions: [], blockedReasonCode: null };
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    emit(es, "snapshot", projection({ revision: 5 }));
    // 挂载时（B 打断前）：无暂停件（queuePaused=false 初值）
    await screen.findByTestId("composer-input");
    expect(screen.queryByTestId("send-queue-paused")).toBeNull();
    // 设备 B 打断并暂停同线程：服务端状态两处变化——①队列视图 queuePaused=true
    // ②send-info queuePaused=true（同一持久暂停的两个消费面）
    queueResponse = {
      actions: [{ actionId: "a1", state: "queued", position: 1, receipt: null }],
      blockedReasonCode: "native_turn_pending",
      canInterrupt: false,
      queuePaused: true,
    };
    sendInfoResponse = {
      injectable: true,
      channels: ["headless_codex_queue"],
      visibility: "realtime",
      queuePaused: true,
    };
    // 设备 A 收到生产 SSE 帧（终态 revision 推进——队列刷新与 send-info 重拉的
    // 既有触发源；ThreadProjection 全量帧 = r6_toggle 同一形状，无手工字段）
    emit(es, "state", projection({ revision: 6 }));
    // C04：暂停展示收敛到队列区摘要（composer 无先发出口）
    await waitFor(() =>
      expect(screen.getByTestId("codex-queue-summary").getAttribute("data-paused")).toBe("true")
    );
    expect(screen.queryByTestId("queue-bypass-arm")).toBeNull();
    expect(screen.queryByTestId("send-queue-paused")).toBeNull();
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
    // C04：composer 不再渲染暂停件；**空队列**面板自隐（验收明文）——暂停事实的
    // 可观察面 = composer send-info 重拉（暂停下有新入队时的摘要展示见 R02）。
    queueResponse = { actions: [], blockedReasonCode: null, queuePaused: true };
    expect(screen.queryByTestId("queue-resume")).toBeNull();
    expect(screen.queryByTestId("queue-bypass-arm")).toBeNull();
    // 重拉确实发生（composer send-info 调用数前进，而非别的机制碰巧出条；去抖
    // 有界等待）
    await waitFor(
      () => {
        const infoCallsAfter = fetchMock!.mock.calls.filter(([u]) =>
          String(u).includes("/session-send-info")
        ).length;
        expect(infoCallsAfter).toBeGreaterThan(infoCallsBefore);
      },
      { timeout: 2000 }
    );
  });

  it("T4-2 resume 反向帧：queuePaused=true 挂载 → SSE 帧只翻 queuePaused=false → composer 恢复正常且无暂停件", async () => {
    installFetch();
    queueResponse = { actions: [], blockedReasonCode: null, queuePaused: true };
    sendInfoResponse = {
      injectable: true,
      channels: ["headless_codex_queue"],
      visibility: "realtime",
      queuePaused: true,
    };
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    emit(es, "snapshot", projection({ revision: 5, queuePaused: true }));
    // 暂停中：空队列面板自隐，composer 无暂停件（C04）
    await act(async () => {});
    expect(screen.queryByTestId("send-queue-paused")).toBeNull();
    expect(screen.queryByTestId("codex-queue-panel")).toBeNull();
    // 设备 B 显式恢复（跨设备起源）→ T4 resume republish 反向帧：只翻 queuePaused
    sendInfoResponse = {
      injectable: true,
      channels: ["headless_codex_queue"],
      visibility: "realtime",
    };
    queueResponse = { actions: [], blockedReasonCode: null, queuePaused: false };
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
    // T04：自由填写默认收起——先点「其他答案」展开再写稿
    fireEvent.click(within(dock).getByTestId("codex-async-free-toggle"));
    const area = (await within(dock).findByRole("textbox")) as HTMLTextAreaElement;
    fireEvent.change(area, { target: { value: "草稿保留" } });
    // 第二组到来：只展开一张卡，其余组收进唯一入口；第一组仍挂载，草稿不丢。
    emit(
      es,
      "snapshot",
      projection({
        revision: 6,
        asyncQuestions: [asyncQuestion("q1"), asyncQuestion("q2")],
      })
    );
    await waitFor(() => expect(screen.getAllByTestId("codex-async-question")).toHaveLength(2));
    expect(screen.getByTestId("codex-other-groups")).toBeTruthy();
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
    // T04：一张展开 + 其余组收进唯一「另有 N 组」入口；组卡片仍挂载但 hidden，草稿不丢。
    await waitFor(() => expect(screen.getAllByTestId("codex-other-groups")).toHaveLength(1));
    expect(screen.getByTestId("codex-other-groups")).toHaveTextContent("1");
    const oldPanel = screen.getByTestId("codex-group-panel-t1|old");
    const newPanel = screen.getByTestId("codex-group-panel-t1|new");
    expect(oldPanel.hidden).toBe(false);
    expect(newPanel.hidden).toBe(true);
    expect(within(oldPanel).getByText("old-0")).toBeTruthy();
    expect(screen.queryByTestId("codex-group-tab-0")).toBeNull();
    expect(screen.queryByTestId("codex-async-history")).toBeNull();
    // 展开入口 → 点组条目切换
    fireEvent.click(screen.getByTestId("codex-other-groups"));
    fireEvent.click(screen.getByTestId("codex-group-item"));
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
    expect(await screen.findByTestId("codex-async-question")).toBeTruthy();
    emit(es, "snapshot", projection({ revision: 7, asyncQuestions: [aq(0, "unknown")] }));
    await waitFor(() => expect(screen.queryByTestId("codex-async-question")).toBeNull());
  });
});

// ============================================================
// T04：紧凑题卡与唯一「另有 N 组」入口（spec §3）
// - 多个有效组只展开一张卡，其余收进唯一 codex-other-groups 入口；
//   已答组不计数；展开列表可切任意组；
// - 切组／收起／展开全程零提交 POST；隐藏组保持挂载（草稿与在途不丢）；
// - 停靠区上限 35dvh；字号档位经 SessionDetail 状态到达题卡内容。
// ============================================================
describe("T04 唯一其他组入口", () => {
  beforeEach(() => installFetch());
  afterEach(() => {
    cleanup();
    window.localStorage.clear();
    window.sessionStorage.clear();
    vi.unstubAllGlobals();
  });

  function gq(item: string, index: number, state = "pending") {
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

  it("t04_seven_pending_groups_yield_exactly_one_other_groups_entry_counting_six", async () => {
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    const questions = [1, 2, 3, 4, 5, 6, 7].map((n) => gq(`g${n}`, 0));
    emit(es, "snapshot", projection({ asyncQuestions: questions }));
    await waitFor(() => expect(screen.getAllByTestId("codex-async-question")).toHaveLength(7));
    // 唯一入口 + 另有 6 组
    expect(screen.getAllByTestId("codex-other-groups")).toHaveLength(1);
    expect(screen.getByTestId("codex-other-groups")).toHaveTextContent("6");
    // 停靠区上限 35dvh（spec §3.6：常态最多 35dvh）
    expect(screen.getByTestId("card-dock").className).toContain("max-h-[35dvh]");
    // 一组整体已答 → 不计数（6 → 5）
    const withAnswered = questions.map((q, i) =>
      i === 6 ? { ...q, state: "answered", actionable: false } : q
    );
    emit(es, "snapshot", projection({ revision: 7, asyncQuestions: withAnswered }));
    await waitFor(() => expect(screen.getByTestId("codex-other-groups")).toHaveTextContent("5"));
    // 展开列表可切任意组；切组／展开／收起全程零提交 POST
    fireEvent.click(screen.getByTestId("codex-other-groups"));
    const items = screen.getAllByTestId("codex-group-item");
    expect(items).toHaveLength(5);
    fireEvent.click(items.find((el) => el.getAttribute("data-group-key") === "t1|g4")!);
    await waitFor(() => expect(screen.getByTestId("codex-group-panel-t1|g4").hidden).toBe(false));
    expect(screen.getByTestId("codex-group-panel-t1|g1").hidden).toBe(true);
    const answerPosts = fetchMock!.mock.calls.filter(([u]) =>
      String(u).includes("/async-answer-group")
    );
    expect(answerPosts).toHaveLength(0);
  });

  it("t04_switch_collapse_expand_keeps_draft_and_inflight_with_zero_new_posts", async () => {
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    emit(es, "snapshot", projection({ asyncQuestions: [gq("g1", 0), gq("g2", 0), gq("g3", 0)] }));
    await waitFor(() => expect(screen.getAllByTestId("codex-async-question")).toHaveLength(3));
    const panelOf = (key: string) => screen.getByTestId(`codex-group-panel-t1|${key}`);
    // g1 展开自由填写并写稿
    const panel1 = panelOf("g1");
    fireEvent.click(within(panel1).getByTestId("codex-async-free-toggle"));
    fireEvent.change(within(panel1).getByTestId("codex-async-free"), {
      target: { value: "草稿壹" },
    });
    // 切到 g3
    fireEvent.click(screen.getByTestId("codex-other-groups"));
    fireEvent.click(
      screen
        .getAllByTestId("codex-group-item")
        .find((el) => el.getAttribute("data-group-key") === "t1|g3")!
    );
    await waitFor(() => expect(panelOf("g3").hidden).toBe(false));
    expect(panelOf("g1").hidden).toBe(true);
    // g3 作答并提交：fetch 挂起（在途）
    const submitCalls: string[] = [];
    let resolveSubmit: ((r: Response) => void) | null = null;
    const base = fetchMock!;
    vi.stubGlobal("fetch", (input: RequestInfo | URL, init?: RequestInit) => {
      if (String(input).includes("/async-answer-group")) {
        submitCalls.push(String(input));
        return new Promise<Response>((resolve) => {
          resolveSubmit = resolve;
        });
      }
      return base(input, init);
    });
    const panel3 = panelOf("g3");
    fireEvent.click(within(panel3).getByRole("button", { name: "甲" }));
    fireEvent.click(within(panel3).getByTestId("codex-async-submit"));
    await waitFor(() => expect(submitCalls).toHaveLength(1));
    // 在途期间切走再切回（收起/展开入口各一次）
    fireEvent.click(screen.getByTestId("codex-other-groups"));
    fireEvent.click(
      screen
        .getAllByTestId("codex-group-item")
        .find((el) => el.getAttribute("data-group-key") === "t1|g1")!
    );
    await waitFor(() => expect(panelOf("g1").hidden).toBe(false));
    fireEvent.click(screen.getByTestId("codex-other-groups"));
    fireEvent.click(
      screen
        .getAllByTestId("codex-group-item")
        .find((el) => el.getAttribute("data-group-key") === "t1|g3")!
    );
    await waitFor(() => expect(panelOf("g3").hidden).toBe(false));
    // 在途回执落地：epoch/token 守卫未因切组丢失 → submitted
    await act(async () => {
      resolveSubmit?.(
        new Response(
          JSON.stringify({
            actionId: "act-x",
            delivery: "accepted",
            route: "desktop_ipc",
            turnId: "turn-x",
            envelopeId: "env-x",
            reasonCode: null,
          }),
          { status: 200 }
        )
      );
      await Promise.resolve();
    });
    await waitFor(() =>
      expect(
        within(panelOf("g3")).getByTestId("codex-async-question").getAttribute("data-phase")
      ).toBe("submitted")
    );
    expect(submitCalls).toHaveLength(1);
    // 回到 g1：草稿仍在
    fireEvent.click(screen.getByTestId("codex-other-groups"));
    fireEvent.click(
      screen
        .getAllByTestId("codex-group-item")
        .find((el) => el.getAttribute("data-group-key") === "t1|g1")!
    );
    await waitFor(() => expect(panelOf("g1").hidden).toBe(false));
    expect((within(panelOf("g1")).getByTestId("codex-async-free") as HTMLInputElement).value).toBe(
      "草稿壹"
    );
  });

  it("t04_font_scale_reaches_async_question_content_via_session_state", async () => {
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    emit(es, "snapshot", projection({ asyncQuestions: [gq("g1", 0)] }));
    const card = await screen.findByTestId("codex-async-question");
    // 题干/选项内容在字号档位作用域内（默认 100%）
    expect(card.querySelector("[data-font-scale]")).not.toBeNull();
    fireEvent.change(screen.getByTestId("font-scale-select"), { target: { value: "1.25" } });
    expect(card.querySelector("[data-font-scale]")?.getAttribute("data-font-scale")).toBe("1.25");
  });
});

// ============================================================
// T05：composer 主按钮停止 + 页面共享停止控制器（SessionDetail 单例接线）
// ============================================================

/** interrupt.allowed 的有效操作集合（T02/T03 真实载荷形状） */
function interruptAllowedOps() {
  return {
    policyRevision: 1,
    operations: {
      interrupt: {
        implemented: true,
        allowed: true,
        requiresAutoAdapt: false,
        evidence: "observed_success",
        support: "supported",
        reasonCode: null,
      },
    },
  };
}

/** send-info 拉取次数（刷新通知断言用） */
function sendInfoCalls(): number {
  return (
    fetchMock?.mock.calls.filter(([u]) => String(u).includes("/session-send-info")).length ?? 0
  );
}

describe("SessionDetail × Codex IPC：T05 composer 停止与共享控制器", () => {
  it("T05-A：运行中+空稿+明确目标 → 主按钮=停止；载荷带点击时刻冻结 turnId；停止只由 composer 提供；落地后队列/send-info 刷新", async () => {
    installFetch();
    queueResponse = {
      actions: [{ actionId: "a1", state: "queued", position: 1, receipt: null }],
      blockedReasonCode: "native_turn_pending",
      canInterrupt: true,
    };
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    emit(
      es,
      "snapshot",
      projection({
        runtimeStatus: "active",
        activeTurnId: "turn-A",
        effectiveOperations: interruptAllowedOps(),
      })
    );
    // 主按钮 = 停止（enabled）；仅渲染绝不发停止
    const stop = await screen.findByTestId("composer-stop", undefined, { timeout: 3000 });
    expect(stop).toBeEnabled();
    expect(interruptBodies).toEqual([]);
    // C04：面板默认折叠——展开后共享闸的渲染面才可观察（打断臂随 stopping 禁用）
    fireEvent.click(await screen.findByTestId("codex-queue-toggle"));
    // 打断挂起：stopping 期间面板打断臂同闸禁用（同一共享单例）
    interruptResponse = "hang";
    const queueCallsBefore = queueCalls.length;
    const sendInfoBefore = sendInfoCalls();
    fireEvent.click(stop);
    await waitFor(() =>
      expect(interruptBodies).toEqual(['{"sessionId":"sess-codex","turnId":"turn-A"}'])
    );
    // 共享闸：队列面板的打断入口此刻不可再点（多按钮不各发一次）
    expect(screen.queryByTestId("codex-queue-interrupt")).toBeNull();
    // 双击不发第二次（按钮禁用 + 控制器闸双防线）
    fireEvent.click(screen.getByTestId("composer-stop"));
    await act(async () => {});
    expect(interruptBodies).toHaveLength(1);
    // 放行：interrupted → 刷新通知（队列重拉 + send-info 重拉——停止落地后
    // 屏障释放/持久暂停都可能已变化）
    await act(async () => {
      releaseInterrupt?.(
        json({ ok: true, uncertain: false, interruptedTurnId: "turn-A", expectedTurnId: "turn-A" })
      );
      await Promise.resolve();
    });
    await waitFor(() => expect(queueCalls.length).toBeGreaterThan(queueCallsBefore), {
      timeout: 3000,
    });
    await waitFor(() => expect(sendInfoCalls()).toBeGreaterThan(sendInfoBefore), { timeout: 3000 });
    // 回合被打断的事实以投影为准：镜像转 idle → composer 主按钮回到发送
    emit(
      es,
      "state",
      projection({
        runtimeStatus: "idle",
        activeTurnId: null,
        effectiveOperations: interruptAllowedOps(),
      })
    );
    expect(await screen.findByTestId("composer-send", undefined, { timeout: 3000 })).toBeTruthy();
    expect(screen.queryByTestId("composer-stop")).toBeNull();
  });

  it("T05-B：空队列+仅运行中 → 不出现长队列说明（面板自隐）；停止入口在 composer 主按钮可达", async () => {
    installFetch();
    queueResponse = { actions: [], blockedReasonCode: "native_turn_active", canInterrupt: true };
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    emit(
      es,
      "snapshot",
      projection({
        runtimeStatus: "active",
        activeTurnId: "turn-A",
        effectiveOperations: interruptAllowedOps(),
      })
    );
    // 没有排队消息、没有真实未知投递屏障：不因正在运行就展开整段队列提醒
    await screen.findByTestId("composer-stop", undefined, { timeout: 3000 });
    expect(screen.queryByTestId("codex-queue-panel")).toBeNull();
    expect(screen.queryByTestId("codex-queue-blocked")).toBeNull();
    expect(screen.queryByTestId("codex-queue-interrupt")).toBeNull();
    // 停止入口在 composer（可执行）
    expect(screen.getByTestId("composer-stop")).toBeEnabled();
  });

  it("T05-C：运行中+能力不足 → 主按钮保持发送、就近短状态（不提供可执行的停止）", async () => {
    installFetch();
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    emit(
      es,
      "snapshot",
      projection({
        runtimeStatus: "active",
        activeTurnId: null, // 目标不明
      })
    );
    await screen.findByTestId("composer-stop-unavailable", undefined, { timeout: 3000 });
    expect(screen.queryByTestId("composer-stop")).toBeNull();
    // 空稿 + 空闲发送规则：发送禁用（不可执行停止 ≠ 可发送）
    expect(screen.getByTestId("composer-send")).toBeDisabled();
    expect(interruptBodies).toEqual([]);
  });

  it("T05-D：入队反馈——发送 codex_queued → 「排队第1位」chip + 队列行（展开撤回入口）；未暂停不常驻插队提示", async () => {
    installFetch();
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    emit(es, "snapshot", projection({ revision: 5 }));
    const input = await screen.findByTestId("composer-input");
    // 尚无排队消息：无暂停条、无「本条先发送」（插队）提示
    expect(screen.queryByTestId("send-queue-paused")).toBeNull();
    expect(screen.queryByTestId("queue-bypass-arm")).toBeNull();
    queueResponse = {
      actions: [{ actionId: "act-q", state: "queued", position: 1, receipt: null }],
      blockedReasonCode: null,
    };
    fireEvent.change(input, { target: { value: "排队一条" } });
    fireEvent.click(screen.getByTestId("composer-send"));
    // 真实 codex_queued 回执 → 紧凑「排队第 N 位」chip
    const chip = await screen.findByTestId("send-receipt-codex-queued", undefined, {
      timeout: 3000,
    });
    // 紧凑「排队第 N 位」（zh/en 各有措辞，断言位次数字）
    expect(chip.textContent).toMatch(/第1位|position 1/);
    // 展开入口：队列面板折叠摘要（计数更新不自动展开）→ 展开（撤回入口）
    fireEvent.click(await screen.findByTestId("codex-queue-toggle", undefined, { timeout: 3000 }));
    expect(
      await screen.findByTestId("codex-queue-row-act-q", undefined, { timeout: 3000 })
    ).toBeTruthy();
    expect(screen.getByTestId("codex-queue-retract-act-q")).toBeTruthy();
    // 未暂停：插队提示不常驻
    expect(screen.queryByTestId("queue-bypass-arm")).toBeNull();
  });

  it("T05-E：旧队列暂停时恢复入口及时出现（沿用 queuePaused 语义，不改后端）", async () => {
    installFetch();
    queueResponse = {
      actions: [{ actionId: "act-q", state: "queued", position: 1, receipt: null }],
      blockedReasonCode: null,
      queuePaused: true,
    };
    sendInfoResponse = {
      injectable: true,
      channels: ["headless_codex_queue"],
      visibility: "realtime",
      queuePaused: true,
    };
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    emit(es, "snapshot", projection({ revision: 5 }));
    // C04：composer 无暂停件；恢复入口在队列区——摘要及时显示已暂停，展开后
    // 唯一恢复按钮在场
    expect(
      await screen.findByTestId("codex-queue-summary", undefined, { timeout: 3000 })
    ).toBeTruthy();
    expect(screen.getByTestId("codex-queue-summary").getAttribute("data-paused")).toBe("true");
    fireEvent.click(screen.getByTestId("codex-queue-toggle"));
    expect(
      await screen.findByTestId("codex-queue-resume", undefined, { timeout: 3000 })
    ).toBeTruthy();
  });
});

describe("R07：详情同源 + 真实重连展示", () => {
  function displayOps(allowed: boolean, support: "supported" | "identifying") {
    return {
      policyRevision: allowed ? 11 : 12,
      capabilityIdentity: "cap-1",
      operations: {
        async_answer_idle: {
          implemented: true,
          allowed,
          requiresAutoAdapt: true,
          evidence: support === "supported" ? "contract_matched" : "none",
          support,
          reasonCode: support === "supported" ? null : "operation_no_evidence",
        },
      },
    };
  }

  it("R07-1：详情能力表消费页面同版事实，随投影变化（不读独立 GET 旧快照）", async () => {
    installFetch();
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    emit(
      es,
      "snapshot",
      projection({
        revision: 5,
        stale: false,
        effectiveOperations: displayOps(true, "supported"),
      })
    );
    // 工具栏已按页面同版渲染。
    expect(await screen.findByTestId("codex-ops-chip-async_answer_idle")).toBeTruthy();
    // 打开详情：能力表与页面同版一致（allowed=true）。
    fireEvent.click(await screen.findByTestId("codex-ops-details"));
    expect(await screen.findByTestId("codex-adapt-op-async_answer_idle")).toHaveAttribute(
      "data-allowed",
      "true"
    );
    // 页面换一版（allowed 翻转 + 原因变化）→ 详情跟着变（独立 GET 不会重拉，
    // 只能来自页面事实）。
    emit(
      es,
      "state",
      projection({
        revision: 6,
        stale: false,
        effectiveOperations: {
          policyRevision: 12,
          capabilityIdentity: "cap-1",
          operations: {
            async_answer_idle: {
              implemented: true,
              allowed: false,
              requiresAutoAdapt: true,
              evidence: "contract_matched",
              support: "supported",
              reasonCode: "operation_runtime_active",
            },
          },
        },
      })
    );
    await waitFor(() =>
      expect(screen.getByTestId("codex-adapt-op-async_answer_idle")).toHaveAttribute(
        "data-allowed",
        "false"
      )
    );
    // 原因也随之更新（同一份页面事实的另一个字段）。
    expect(screen.getByTestId("codex-adapt-op-async_answer_idle")).toHaveTextContent(
      /Turn is running|回合运行中/
    );
  });

  it("R07-2：同身份真实重连（stale）沿用上次支持展示并标「连接恢复中」", async () => {
    installFetch();
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    emit(
      es,
      "snapshot",
      projection({
        revision: 5,
        stale: false,
        effectiveOperations: displayOps(true, "supported"),
      })
    );
    expect(await screen.findByTestId("codex-ops-chip-async_answer_idle")).toBeTruthy();
    expect(screen.queryByTestId("codex-ops-reconnecting")).toBeNull();

    // 真实重连：stale + 证据槽换代失配 → 投影 support 回 identifying、identity 不变。
    emit(
      es,
      "state",
      projection({
        revision: 6,
        stale: true,
        effectiveOperations: displayOps(false, "identifying"),
      })
    );
    // 芯片仍在（沿用上次支持），并如实标「连接恢复中」。
    await waitFor(() => expect(screen.getByTestId("codex-ops-reconnecting")).toBeTruthy());
    expect(screen.getByTestId("codex-ops-chip-async_answer_idle")).toBeTruthy();
    expect(screen.queryByTestId("codex-ops-unsupported-async_answer_idle")).toBeNull();

    // 证据装回（stale 解除、实时 supported）→ 恢复实时展示，标记消失。
    emit(
      es,
      "state",
      projection({
        revision: 7,
        stale: false,
        effectiveOperations: displayOps(true, "supported"),
      })
    );
    await waitFor(() => expect(screen.queryByTestId("codex-ops-reconnecting")).toBeNull());
    expect(screen.getByTestId("codex-ops-chip-async_answer_idle")).toBeTruthy();
  });

  it("R07-4：后端 supportInherited（生产真实路径）→ 显示「连接恢复中」", async () => {
    installFetch();
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    // 后端已把 support 替换成上次结论并置 supportInherited（不依赖前端缓存）。
    emit(
      es,
      "snapshot",
      projection({
        revision: 5,
        stale: true,
        effectiveOperations: {
          policyRevision: 21,
          capabilityIdentity: "cap-1",
          operations: {
            async_answer_idle: {
              implemented: true,
              allowed: false,
              requiresAutoAdapt: true,
              evidence: "none",
              support: "supported",
              supportInherited: true,
              reasonCode: "operation_no_evidence",
            },
          },
        },
      })
    );
    await waitFor(() => expect(screen.getByTestId("codex-ops-reconnecting")).toBeTruthy());
    expect(screen.getByTestId("codex-ops-chip-async_answer_idle")).toBeTruthy();

    // 证据装回：标记清除 → 提示消失。
    emit(
      es,
      "state",
      projection({
        revision: 6,
        stale: false,
        effectiveOperations: {
          policyRevision: 22,
          capabilityIdentity: "cap-1",
          operations: {
            async_answer_idle: {
              implemented: true,
              allowed: true,
              requiresAutoAdapt: true,
              evidence: "contract_matched",
              support: "supported",
              reasonCode: null,
            },
          },
        },
      })
    );
    await waitFor(() => expect(screen.queryByTestId("codex-ops-reconnecting")).toBeNull());
  });

  it("R07-3：identity 变化（A→B）不沿用旧目标展示", async () => {
    installFetch();
    render(<SessionDetail session={makeSession()} onBack={() => {}} />);
    const es = await waitFor(() => MockEventSource.latest());
    emit(
      es,
      "snapshot",
      projection({
        revision: 5,
        stale: false,
        effectiveOperations: displayOps(true, "supported"),
      })
    );
    expect(await screen.findByTestId("codex-ops-chip-async_answer_idle")).toBeTruthy();
    // A→B：identity 变 + 识别中 → 不继承旧目标的已支持。
    emit(
      es,
      "state",
      projection({
        revision: 6,
        stale: true,
        effectiveOperations: {
          policyRevision: 13,
          capabilityIdentity: "cap-2",
          operations: {
            async_answer_idle: {
              implemented: true,
              allowed: false,
              requiresAutoAdapt: true,
              evidence: "none",
              support: "identifying",
              reasonCode: "operation_no_evidence",
            },
          },
        },
      })
    );
    // 不继承旧目标：芯片消失，识别中状态只在详情展示，且不谎报
    // 「连接恢复中」（身份变了，没有可沿用的展示）。
    await waitFor(() =>
      expect(screen.queryByTestId("codex-ops-chip-async_answer_idle")).toBeNull()
    );
    expect(screen.queryByTestId("codex-ops-chip-async_answer_idle")).toBeNull();
    expect(screen.queryByTestId("codex-ops-reconnecting")).toBeNull();
    // 详情也如实显示"识别中"（不是旧目标的已支持）。
    fireEvent.click(screen.getByTestId("codex-ops-details"));
    expect(await screen.findByTestId("codex-adapt-op-async_answer_idle")).toHaveAttribute(
      "data-support",
      "identifying"
    );
  });
});
