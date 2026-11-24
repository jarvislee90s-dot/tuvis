// tests/notification/greenStableWindow.test.ts — 绿灯稳定窗（GREEN_STABLE_MS）：
// 转绿不立即播报，持续 3s 仍绿才播；窗内翻回非绿取消绿播报；真实完成只多等 3 秒。
// 注意：非绿转换（绿→黄、黄→红）保持既有即时播报行为，断言用 body 前缀区分颜色。
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
import { SAME_DIRECTION_NOTIFY_MS } from "@/lib/notification-throttle";

const mkSession = (id: string, status: string) => ({
  id,
  agentType: "claude",
  form: "cli" as const,
  projectName: "Demo",
  title: "T",
  gitBranch: null,
  github_url: null,
  status,
  lastMessage: "ok",
  lastMessageRole: null,
  lastActivityAt: new Date().toISOString(),
  pid: 123,
  cpuUsage: 0,
  activeSubagentCount: 0,
  jumpSupported: true,
  unread: false,
});

// 系统通知降级路径的 body 以状态文案开头（STATUS_LABELS：idle=空闲 / processing=运行中）
const greenNotices = () =>
  sendNotificationMock.mock.calls.filter(
    ([n]) => typeof n?.body === "string" && n.body.startsWith("空闲")
  ).length;

// 排空微任务：setState 触发的异步播报链（invoke→catch→降级 sendNotification）需要若干轮
const drain = async () => {
  for (let i = 0; i < 10; i++) {
    await act(async () => {
      await Promise.resolve();
    });
  }
};

const setSessions = (sessions: ReturnType<typeof mkSession>[]) => {
  act(() => {
    useSessionStore.setState({ sessions });
  });
};

const advance = async (ms: number) => {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(ms);
  });
};

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockImplementation(async (cmd: string) => {
    // 浮窗失败 → 强制走系统通知降级路径（可观测断言点）
    if (cmd === "show_notification_window") throw new Error("no popup in jsdom");
    return null;
  });
  sendNotificationMock.mockReset();
  useSessionStore.setState({ sessions: [] });
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
});

describe("绿灯稳定窗（GREEN_STABLE_MS）", () => {
  it("DSH 同 ID 不同实例的稳定窗各自触发并播报正确卡片", async () => {
    renderHook(() => useNotification());
    await drain();
    const mk = (instance: string, status: string) => ({
      ...mkSession("same", status),
      agentType: "dsh",
      dshInstanceId: instance,
      projectName: instance,
    });
    setSessions([mk("A", "processing"), mk("B", "processing")]);
    await drain();
    setSessions([mk("A", "idle"), mk("B", "idle")]);
    await advance(GREEN_STABLE_MS);
    await drain();
    const notices = sendNotificationMock.mock.calls
      .map(([n]) => n)
      .filter((n) => String(n.body).startsWith("空闲"));
    expect(notices).toHaveLength(2);
    expect(notices.map((n) => n.title.split(" — ").at(-1)).sort()).toEqual(["A", "B"]);
  });
  it("转绿不立即播；持续满窗才播一次绿", async () => {
    renderHook(() => useNotification());
    await drain();

    setSessions([mkSession("s1", "processing")]);
    await drain();
    expect(greenNotices()).toBe(0);

    // 翻绿：立即不播
    setSessions([mkSession("s1", "idle")]);
    await drain();
    expect(greenNotices()).toBe(0);

    // 满窗后播一次绿
    await advance(GREEN_STABLE_MS);
    expect(greenNotices()).toBe(1);
  });

  it("窗内翻回黄则取消绿播报（瞬态绿误报形态）", async () => {
    renderHook(() => useNotification());
    await drain();

    setSessions([mkSession("s1", "processing")]);
    await drain();

    // 翻绿后 1s 内翻回黄——绿通知被取消；黄通知为既有即时行为，不计入绿断言
    setSessions([mkSession("s1", "idle")]);
    await advance(1000);
    setSessions([mkSession("s1", "processing")]);
    await drain();
    await advance(GREEN_STABLE_MS + 1000);
    await drain();
    expect(greenNotices()).toBe(0);
  });

  it("绿持续跨轮询只播一次", async () => {
    renderHook(() => useNotification());
    await drain();

    setSessions([mkSession("s1", "processing")]);
    await drain();
    setSessions([mkSession("s1", "idle")]);
    await advance(GREEN_STABLE_MS);
    expect(greenNotices()).toBe(1);

    // 后续轮询（新数组、仍绿）→ 不重播
    setSessions([mkSession("s1", "idle")]);
    await advance(GREEN_STABLE_MS * 3);
    expect(greenNotices()).toBe(1);
  });

  it("绿→黄→绿：黄即时播；跨过同方向节流窗后第二段绿重新过窗再播", async () => {
    renderHook(() => useNotification());
    await drain();

    setSessions([mkSession("s1", "processing")]);
    await drain();

    // 第一段绿：满窗播
    setSessions([mkSession("s1", "idle")]);
    await advance(GREEN_STABLE_MS);
    expect(greenNotices()).toBe(1);

    // 翻黄：非绿转换即时播（黄通知），无稳定窗
    setSessions([mkSession("s1", "processing")]);
    await drain();
    expect(sendNotificationMock.mock.calls.length).toBeGreaterThanOrEqual(2);
    expect(greenNotices()).toBe(1);

    // 跨过 F2a 同方向节流窗（60s，终审发现 C）：黄→绿边的节流记账过期
    await advance(SAME_DIRECTION_NOTIFY_MS);

    // 再翻绿：重新过窗，满窗后再播
    setSessions([mkSession("s1", "idle")]);
    await advance(GREEN_STABLE_MS);
    expect(greenNotices()).toBe(2);
  });
});
