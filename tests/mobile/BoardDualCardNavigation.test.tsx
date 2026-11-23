// M5（2026-11-16）：同目录双卡 → 详情按 sessionID 区分回归（复审返工项）。
//
// 背景：N7 冻结轮 O-N7-6（docs/engineering/acceptance/2026-11-16-n7-freeze-formal-h16.md §6）
// 记录「同目录双 TUI（实名 A + mutant B）期，看板两张卡的点击均落到同一详情（后建会话），
// 单 TUI 后消失」。复审要求补组件回归：同目录两卡（不同 sessionID）点击各自进入按
// sessionID 区分的详情、各自投递目标正确（detail 绑定各自会话）。先跑现状——若组件层
// 现状正确（N7 记录为无头环境 AXPress 工件），本测试锁行为即闭环；若现状缺陷则按纪律
// 只记录不修。
//
// 口径：桌面看板与手机看板共用 Board 组件语义（卡片点击 = onOpenSession(session)，
// App.setSelected 单一路由状态），故以 App 级组件测试表达；N7 观察面（无头宿主 AXPress
// 按坐标命中）不在 jsdom 可达范围，如实标注。断言面 = 导航 + 绑定 + 投递目标三处：
// - 导航：点击各卡进入详情、返回后可点另一卡（双卡均可达，不固化第一张）；
// - 绑定：详情数据读 /session-messages 与原生只读投影 /opencode/view 均按所点卡片的
//   sessionID 发起（不串到同目录另一会话）；
// - 投递目标：原生事件流 SSE（/opencode/events?session_id=…）按所点卡片会话建连。
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import App from "@/mobile/App";
import { MockEventSource } from "./eventSourceMock";
import type { Session } from "@/types/session";
import { pinZhNavigator } from "./pinZhNavigator";

let unpinZh: () => void;

beforeAll(async () => {
  unpinZh = await pinZhNavigator();
});

afterAll(() => {
  unpinZh();
});

// N7 O-N7-6 场景还原：同一目录（同 projectName/projectPath）双 OpenCode 会话，
// sessionID 不同（实名 A 先建 / mutant B 后建）。
const SESSION_A = "oc-real-aaa";
const SESSION_B = "oc-mutant-bbb";
const TITLE_A = "实名A 会话";
const TITLE_B = "mutantB 会话";

function dualCardSession(id: string, title: string, pid: number): Session {
  return {
    id,
    agentType: "opencode",
    // 同目录：项目名与路径刻意相同——缺陷面正是「按目录而非 sessionID 落详情」
    projectName: "同目录项目",
    projectPath: "/tmp/same-directory",
    title,
    gitBranch: null,
    githubUrl: null,
    status: "processing",
    lastMessage: null,
    lastMessageRole: null,
    lastActivityAt: "2026-11-16T00:00:00Z",
    pid,
    cpuUsage: 0,
    activeSubagentCount: 0,
    form: "cli",
    jumpSupported: false,
    unread: false,
  };
}

/** 原生只读投影载荷（PublicNativeView 最小可用形状），按请求的 session 回声键控 */
function nativeView(sessionId: string) {
  return {
    sessionId,
    bindingToken: `bind-${sessionId}`,
    routeVersion: 1,
    revision: 1,
    complete: true,
    capabilities: { send: { allowed: true, evidence: "contract", reasonCode: null } },
    messages: [
      {
        seq: 0,
        role: "assistant",
        kind: "assistant",
        content: `${sessionId} 的原生历史`,
        collapsed: false,
      },
    ],
    pending: [],
    inbox: [],
    runtimeState: "ready",
    files: {},
    route: "opencode_native",
    routeReasonCode: null,
  };
}

interface Captures {
  /** /opencode/view 请求携带的 session_id（原生只读投影目标） */
  viewReads: string[];
  /** /session-messages 请求携带的 session_id（详情数据目标） */
  messageReads: string[];
  /** 原生事件流 SSE URL（投递/回执通道目标） */
  eventUrls: string[];
}

/** 按 URL 分路的 fetch 桩：详情 + 原生面板挂载的最小端点集合
 *  （OpenCodeNativeGatePresentation.test.tsx installShellFetch 同形），并记录会话目标 */
function installShellFetch(): Captures {
  const captures: Captures = { viewReads: [], messageReads: [], eventUrls: [] };
  const sessionIdOf = (url: string) =>
    new URLSearchParams(new URL(url, "http://localhost").search).get("session_id");
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL) => {
      const url = String(input);
      // 前缀序：/session-* 系列先判，避免被 /sessions 轮询分支吞掉
      if (url.includes("/session-messages")) {
        const sid = sessionIdOf(url);
        if (sid) captures.messageReads.push(sid);
        return Response.json({ messages: [], truncated: false });
      }
      if (url.includes("/session-files")) return Response.json({ files: [], truncated: false });
      if (url.includes("/session-subagents")) return Response.json({ subagents: [] });
      if (url.includes("/session-send-info"))
        return Response.json({
          injectable: false,
          reasonCode: "opencode_native",
          interactionRoute: "opencode_native",
          interactionRouteReason: "已归原生",
        });
      if (url.includes("/opencode/view")) {
        const sid = sessionIdOf(url) ?? "";
        captures.viewReads.push(sid);
        return Response.json(nativeView(sid));
      }
      if (url.includes("/opencode/agents")) return Response.json({ current: null, options: [] });
      if (url.includes("/opencode/outbox"))
        return Response.json({
          sessionId: sessionIdOf(url),
          revision: 1,
          paused: false,
          items: [],
        });
      if (url.includes("/host"))
        return Response.json({
          host: { name: "n", platform: "darwin", version: "0", bootId: "b" },
          enabledTools: ["opencode"],
          installedTools: ["claude", "codex", "kimi", "opencode"],
        });
      if (url.includes("/channel")) return Response.json({ via: "lan", limited: false });
      if (url.includes("/sessions")) {
        // 看板轮询兜底（SSE 为主通道，快照经脚本化 EventSource 投递）
        return Response.json({
          sessions: [
            dualCardSession(SESSION_A, TITLE_A, 101),
            dualCardSession(SESSION_B, TITLE_B, 102),
          ],
          totalCount: 2,
          waitingCount: 0,
        });
      }
      return Response.json({});
    })
  );
  return captures;
}

/** 脚本化 EventSource：看板事件流（/m/api/v1/events）首帧投快照；原生事件流只记录不投帧 */
function installScriptedSse(captures: Captures) {
  class ScriptedEventSource extends MockEventSource {
    constructor(url: string) {
      super(url);
      if (url.includes("/opencode/events")) {
        captures.eventUrls.push(url);
        return; // 原生事件流不投看板帧（事件类型不同，投了也无监听者）
      }
      setTimeout(
        () =>
          this.emit("snapshot", {
            sessions: [
              dualCardSession(SESSION_A, TITLE_A, 101),
              dualCardSession(SESSION_B, TITLE_B, 102),
            ],
            totalCount: 2,
            waitingCount: 0,
          }),
        0
      );
    }
  }
  vi.stubGlobal("EventSource", ScriptedEventSource);
}

/** 打开看板并等待双卡上板（N7 场景：同目录双卡同屏） */
async function renderBoardWithDualCards(captures: Captures) {
  installScriptedSse(captures);
  render(<App />);
  expect(await screen.findByText(TITLE_A)).toBeTruthy();
  expect(await screen.findByText(TITLE_B)).toBeTruthy();
}

/** 从看板点击指定卡并等待原生详情挂载（native-input = 原生面板按该会话建连完成） */
async function openCardAndAwaitNative(title: string) {
  fireEvent.click(screen.getByText(title).closest("li") as HTMLLIElement);
  await screen.findByTestId("native-input");
  expect(screen.getByTestId("detail-back")).toBeTruthy();
}

describe("同目录双卡 → 详情按 sessionID 区分（M5 复审回归 / N7 O-N7-6）", () => {
  beforeEach(() => {
    sessionStorage.clear();
    MockEventSource.reset();
  });
  afterEach(() => {
    cleanup();
    vi.unstubAllGlobals();
  });

  it("先点实名 A 再点 mutant B：两次详情的数据/投影/事件流目标各按所点卡片的 sessionID", async () => {
    const captures = installShellFetch();
    await renderBoardWithDualCards(captures);

    // 卡 A → 详情绑定 A：数据读、原生投影、原生事件流全部指向 oc-real-aaa
    await openCardAndAwaitNative(TITLE_A);
    await waitFor(() => expect(captures.viewReads).toContain(SESSION_A));
    expect(captures.messageReads).toContain(SESSION_A);
    expect(captures.viewReads).not.toContain(SESSION_B);
    expect(captures.messageReads).not.toContain(SESSION_B);
    expect(captures.eventUrls.some((u) => u.includes(`session_id=${SESSION_A}`))).toBe(true);

    // 返回看板 → 卡 B → 详情换绑 B：目标翻转为 oc-mutant-bbb，A 的目标不再新增
    fireEvent.click(screen.getByTestId("detail-back"));
    expect(await screen.findByText(TITLE_A)).toBeTruthy();
    expect(screen.getByText(TITLE_B)).toBeTruthy();
    const aViewsBefore = captures.viewReads.filter((s) => s === SESSION_A).length;
    await openCardAndAwaitNative(TITLE_B);
    await waitFor(() => expect(captures.viewReads).toContain(SESSION_B));
    expect(captures.messageReads).toContain(SESSION_B);
    expect(captures.viewReads.filter((s) => s === SESSION_A).length).toBe(aViewsBefore);
    expect(captures.eventUrls.some((u) => u.includes(`session_id=${SESSION_B}`))).toBe(true);
  });

  it("反向顺序（先 B 后 A）同判据：首点不固化，双卡各自可达且目标正确", async () => {
    const captures = installShellFetch();
    await renderBoardWithDualCards(captures);

    await openCardAndAwaitNative(TITLE_B);
    await waitFor(() => expect(captures.viewReads).toContain(SESSION_B));
    expect(captures.viewReads).not.toContain(SESSION_A);

    fireEvent.click(screen.getByTestId("detail-back"));
    expect(await screen.findByText(TITLE_A)).toBeTruthy();
    await openCardAndAwaitNative(TITLE_A);
    await waitFor(() => expect(captures.viewReads).toContain(SESSION_A));
    expect(captures.messageReads).toContain(SESSION_A);
    expect(captures.viewReads).not.toContain("oc-other");
  });
});
