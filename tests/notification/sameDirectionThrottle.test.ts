// tests/notification/sameDirectionThrottle.test.ts — F2a 同方向跃迁节流（终审发现 C）：
// 同会话同一 from→to 颜色对 60s 内不重复通知（黄→绿与绿→黄两个边都算，记录按方向分存
// ——交替抖动也会被压住）；红/waiting 不节流（等待提醒不延迟，既有语义）。
// 判定点 = 最终通知点（绿转换在稳定窗之后），与既有 5s 同色去重叠加
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
import { SAME_DIRECTION_NOTIFY_MS, isSameDirectionThrottled } from "@/lib/notification-throttle";

const mkSession = (id: string, status: string) => ({
  id,
  agentType: "claude",
  form: "cli" as const,
  projectName: "Demo",
  title: "T",
  gitBranch: null,
  githubUrl: null,
  status,
  lastMessage: "ok",
  lastMessageRole: null,
  lastActivityAt: new Date().toISOString(),
  pid: 1,
  cpuUsage: 0,
  activeSubagentCount: 0,
  jumpSupported: true,
  unread: false,
  flapFromSubagentActivity: false,
});

// 系统通知降级路径的 body 以状态文案开头（STATUS_LABELS：idle=空闲 / waiting=等待操作）
const noticesByBody = (prefix: string) =>
  sendNotificationMock.mock.calls.filter(
    ([n]) => typeof n?.body === "string" && n.body.startsWith(prefix)
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

describe("同方向节流纯判定（isSameDirectionThrottled）", () => {
  const dirMap = new Map([["yellow>green", 1000]]);

  it("同方向 60s 内 → 节流；跨过 60s → 放行", () => {
    expect(isSameDirectionThrottled(dirMap, "yellow", "green", 1000 + 1000)).toBe(true);
    expect(
      isSameDirectionThrottled(dirMap, "yellow", "green", 1000 + SAME_DIRECTION_NOTIFY_MS)
    ).toBe(false);
  });

  it("反方向（不同键）→ 不节流（交替边各自记账）", () => {
    expect(isSameDirectionThrottled(dirMap, "green", "yellow", 1500)).toBe(false);
  });

  it("红/waiting 两个方向都豁免；无记账 / 首见未知前色 → 不节流", () => {
    expect(isSameDirectionThrottled(dirMap, "yellow", "red", 1500)).toBe(false);
    expect(isSameDirectionThrottled(dirMap, "red", "green", 1500)).toBe(false);
    expect(isSameDirectionThrottled(undefined, "yellow", "green", 1500)).toBe(false);
    expect(isSameDirectionThrottled(dirMap, "", "green", 1500)).toBe(false);
  });
});

describe("60s 同方向跃迁节流（黄→绿边，经稳定窗）", () => {
  it("60s 内两次黄→绿只弹一次绿；跨窗后再翻绿可再弹", async () => {
    renderHook(() => useNotification());
    await drain();

    setSessions([mkSession("s1", "processing")]);
    await drain();

    // 第一段黄→绿：满窗播一次
    setSessions([mkSession("s1", "idle")]);
    await advance(GREEN_STABLE_MS);
    expect(noticesByBody("空闲")).toBe(1);

    // 翻回黄再翻绿（teammate 抖动形态）：第二段绿在同方向 60s 窗内 → 节流不弹
    setSessions([mkSession("s1", "processing")]);
    await drain();
    setSessions([mkSession("s1", "idle")]);
    await advance(GREEN_STABLE_MS + 100);
    expect(noticesByBody("空闲")).toBe(1, "同方向 60s 内的第二次黄→绿必须被节流");

    // 跨过节流窗后再翻绿（黄→绿）：正常再弹
    await advance(SAME_DIRECTION_NOTIFY_MS);
    setSessions([mkSession("s1", "processing")]);
    await drain();
    setSessions([mkSession("s1", "idle")]);
    await advance(GREEN_STABLE_MS + 100);
    expect(noticesByBody("空闲")).toBe(2);
  });

  it("反方向不节流：黄→绿后紧接绿→黄，黄即时弹（绿→黄边不受绿方向记账影响）", async () => {
    renderHook(() => useNotification());
    await drain();

    setSessions([mkSession("s1", "processing")]);
    await drain();
    setSessions([mkSession("s1", "idle")]);
    await advance(GREEN_STABLE_MS);
    expect(noticesByBody("空闲")).toBe(1);

    // 绿→黄：与上次方向相反 → 不节流，即时弹黄
    setSessions([mkSession("s1", "processing")]);
    await drain();
    expect(noticesByBody("运行中")).toBe(1);
  });

  it("红/waiting 豁免：60s 内两次绿→等待都弹（等待提醒不延迟）", async () => {
    renderHook(() => useNotification());
    await drain();

    // 先过一次常规黄→绿（稳定窗），建立绿基线（首见绿不通知）
    setSessions([mkSession("s1", "processing")]);
    await drain();
    setSessions([mkSession("s1", "idle")]);
    await advance(GREEN_STABLE_MS);
    expect(noticesByBody("空闲")).toBe(1);

    // 绿→等待：红边不节流不进稳定窗，即时弹
    setSessions([mkSession("s1", "waiting")]);
    await drain();
    expect(noticesByBody("等待操作")).toBe(1);

    // 回绿再入等待（同方向绿→等待 60s 内重复）：红边豁免，第二次照常弹
    setSessions([mkSession("s1", "idle")]);
    await advance(GREEN_STABLE_MS);
    setSessions([mkSession("s1", "waiting")]);
    await drain();
    expect(noticesByBody("等待操作")).toBe(2, "waiting 提醒永不节流（既有语义）");
  });
});
