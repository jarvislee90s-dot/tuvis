// 观察台 §四：关闭「子 Agent 回报提醒」仅滤子 agent 回报触发的通知——
// 打标来自后端（Session.lastMessageSubagentReport 布尔），前端不匹配文案。
import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const { invokeMock, sendNotificationMock } = vi.hoisted(() => ({
  invokeMock: vi.fn(),
  sendNotificationMock: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));
vi.mock("@tauri-apps/plugin-notification", () => ({
  isPermissionGranted: vi.fn(async () => true),
  requestPermission: vi.fn(async () => "granted"),
  sendNotification: sendNotificationMock,
  onAction: vi.fn(async () => () => {}),
  registerActionTypes: vi.fn(async () => {}),
}));
vi.mock("@/lib/audio", () => ({ playCompletionSound: vi.fn() }));
vi.mock("@/lib/notificationHistory", () => ({ addHistory: vi.fn() }));
vi.mock("@/components/pet/petConfig", () => ({
  petSuppressPopup: () => false,
  petSoundTakeover: () => false,
}));

import { useSessionStore } from "@/stores/sessionStore";
import { useNotification, GREEN_STABLE_MS } from "@/hooks/useNotification";

function session(id: string, report: boolean) {
  return {
    id,
    agentType: "claude",
    form: "cli" as const,
    projectName: "Demo",
    title: "T",
    gitBranch: null,
    githubUrl: null,
    status: "idle",
    lastMessage: report ? "Another Claude session sent a message: <teammate-message …" : "ok",
    lastMessageSubagentReport: report,
    lastMessageRole: null,
    lastActivityAt: new Date().toISOString(),
    pid: 1,
    cpuUsage: 0,
    activeSubagentCount: 0,
    jumpSupported: true,
    unread: true,
  };
}

// 微任务 flush（评审 P1-4）：用例先 vi.useFakeTimers() 再等初始化——setTimeout
// 已被 fake 接管会永挂（notifyOnce.test.ts 是真计时器场景，写法不可照抄）；
// init effect 的 await 链全是微任务，排空微任务队列即够
async function flushInit() {
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
  });
}

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockImplementation(async (cmd: string, args?: { key?: string }) => {
    if (cmd === "show_notification_window") throw new Error("no popup in jsdom");
    if (cmd === "get_setting" && args?.key === "notify_subagent_report") return "false";
    return null; // notifications_enabled 缺省 → 开
  });
  sendNotificationMock.mockReset();
  useSessionStore.setState({ sessions: [] });
});
afterEach(() => {
  // 兜底恢复真计时器（评审 P1-4）：用例中途断言抛错时尾行手动恢复不会执行，
  // 泄漏的 fake timers 会拖垮后续用例
  vi.useRealTimers();
});

describe("子 Agent 回报提醒开关（观察台 §四）", () => {
  it("开关关闭：回报触发的转绿不弹；主会话自身动作照常（§四.2）", async () => {
    vi.useFakeTimers();
    renderHook(() => useNotification());
    await flushInit();
    // 首见：一条回报标记、一条无标记（都走绿稳定窗）
    act(() => {
      useSessionStore.setState({ sessions: [session("s-report", true), session("s-own", false)] });
    });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(GREEN_STABLE_MS + 100);
    });
    // show_notification_window 失败 → 降级系统通知（可观测断言点）：
    // 仅 s-own 到达，s-report 被滤
    const targets = sendNotificationMock.mock.calls.map((c) => JSON.stringify(c[0].extra));
    expect(targets.some((t) => t.includes("s-own"))).toBe(true, "主会话自身动作照常提醒");
    expect(targets.some((t) => t.includes("s-report"))).toBe(false, "子 agent 回报被滤");
  });

  it("开关开（缺省）：回报触发的提醒照常（§四.1 默认开）", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "show_notification_window") throw new Error("no popup in jsdom");
      return null; // notify_subagent_report 缺省 → 开
    });
    vi.useFakeTimers();
    renderHook(() => useNotification());
    await flushInit();
    act(() => {
      useSessionStore.setState({ sessions: [session("s-report", true)] });
    });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(GREEN_STABLE_MS + 100);
    });
    expect(sendNotificationMock).toHaveBeenCalledTimes(1);
  });
});
