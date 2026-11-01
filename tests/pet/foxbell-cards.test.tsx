// tests/pet/foxbell-cards.test.tsx — 卡片渲染 + 点击跳转 ack（msw/invoke mock 走 tests/msw）
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const { mockQueryState, eventListeners } = vi.hoisted(() => ({
  mockQueryState: { data: null as unknown },
  eventListeners: new Map<string, Array<(event: { payload: unknown }) => void>>(),
}));

vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({
    show: vi.fn(),
    hide: vi.fn(),
    setAlwaysOnTop: vi.fn(),
    setIgnoreCursorEvents: vi.fn(),
    setPosition: vi.fn(),
    setSize: vi.fn(),
    outerPosition: vi.fn(),
    outerSize: vi.fn(),
    scaleFactor: vi.fn(),
    currentMonitor: vi.fn(),
  }),
}));
vi.mock("@tauri-apps/api/event", () => ({
  listen: async (name: string, callback: (event: { payload: unknown }) => void) => {
    const group = eventListeners.get(name) ?? [];
    group.push(callback);
    eventListeners.set(name, group);
    return () =>
      eventListeners.set(
        name,
        (eventListeners.get(name) ?? []).filter((fn) => fn !== callback)
      );
  },
}));

const sessionsData = {
  sessions: [
    {
      id: "s1",
      agentType: "claude",
      projectName: "项目A",
      projectPath: "/a",
      title: "标题A",
      gitBranch: null,
      githubUrl: null,
      status: "waiting",
      lastMessage: "等你确认",
      lastMessageRole: null,
      lastActivityAt: "",
      pid: 42,
      cpuUsage: 0,
      activeSubagentCount: 0,
      form: "cli",
      jumpSupported: true,
    },
  ],
  totalCount: 1,
  waitingCount: 1,
};
vi.mock("@/lib/query/queries/sessions", () => ({
  useSessionsQuery: () => ({ data: mockQueryState.data }),
}));

import { invoke } from "@tauri-apps/api/core";
import {
  __resetSessionMutationWaitersForTest,
  __resetScanSnapshotVersionsForTest,
  applySessionsSnapshot,
  clearPendingSessions,
} from "@/lib/query/optimisticSessions";
import { queryClient } from "@/lib/query/queryClient";
import { FoxbellPet } from "@/components/pet/FoxbellPet";

mockQueryState.data = sessionsData;

beforeEach(() => {
  queryClient.clear();
  __resetScanSnapshotVersionsForTest();
  __resetSessionMutationWaitersForTest();
  clearPendingSessions([{ id: "s1", agentType: "claude" }]);
  mockQueryState.data = sessionsData;
  eventListeners.clear();
});

afterEach(() => {
  __resetSessionMutationWaitersForTest();
  clearPendingSessions([{ id: "s1", agentType: "claude" }]);
  queryClient.clear();
  vi.restoreAllMocks();
});

function emitSessionRead(agentType: string, sessionId: string) {
  for (const listener of eventListeners.get("session-read") ?? []) {
    listener({ payload: { agentType, sessionId } });
  }
}

describe("FoxbellPet 卡片", () => {
  it("waiting 会话渲染红卡；点击调用 focus_session（spec C1）", async () => {
    render(<FoxbellPet />);
    const card = await screen.findByTestId("pet-card-s1");
    // 题头与看板一致：工具名 + 项目 + 会话名（问题 3）
    expect(card.textContent).toContain("Claude");
    expect(card.textContent).toContain("项目A");
    expect(card.textContent).toContain("标题A");
    expect(card.textContent).toContain("等待操作");
    fireEvent.click(card);
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith(
        "focus_session",
        expect.objectContaining({ pid: 42, sessionId: "s1" })
      )
    );
  });

  it("X 使用 pending 票根，旧 query 快照不得重建宠物卡", async () => {
    vi.mocked(invoke).mockImplementation((cmd: string) => {
      if (cmd === "dismiss_session_card") {
        return Promise.resolve({ applied: false, version: 1, epoch: 0 });
      }
      return Promise.resolve(undefined);
    });
    const { rerender } = render(<FoxbellPet />);
    const card = await screen.findByTestId("pet-card-s1");
    fireEvent.click(within(card).getByText("✕"));
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("dismiss_session_card", expect.anything())
    );
    await waitFor(() => expect(screen.queryByTestId("pet-card-s1")).toBeNull());

    // 模拟旧轮询回写：状态计算前 applyPendingSessions 仍必须过滤目标。
    mockQueryState.data = {
      ...sessionsData,
      sessions: [...sessionsData.sessions],
    };
    rerender(<FoxbellPet />);
    await act(async () => {});
    expect(screen.queryByTestId("pet-card-s1")).toBeNull();
  });

  it("未读绿宠物卡 X 走 mark_session_read 并阻止旧未读快照复活", async () => {
    const session = {
      ...sessionsData.sessions[0],
      id: "green-1",
      agentType: "codex",
      status: "finished",
      form: "app",
      unread: true,
    };
    const source = { sessions: [session], totalCount: 1, waitingCount: 0 };
    mockQueryState.data = source;
    vi.mocked(invoke).mockImplementation((cmd: string) => {
      if (cmd === "mark_session_read") {
        return Promise.resolve({ applied: false, version: 1, epoch: 0 });
      }
      return Promise.resolve(undefined);
    });
    const { rerender } = render(<FoxbellPet />);
    const card = await screen.findByTestId("pet-card-green-1");
    fireEvent.click(within(card).getByText("✕"));
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("mark_session_read", expect.anything())
    );
    await waitFor(() => expect(screen.queryByTestId("pet-card-green-1")).toBeNull());

    mockQueryState.data = { ...source, sessions: [{ ...session }] };
    rerender(<FoxbellPet />);
    await act(async () => {});
    expect(screen.queryByTestId("pet-card-green-1")).toBeNull();
  });

  it("宠物绿卡已读命令失败后，权威未读快照恢复卡片", async () => {
    const session = {
      ...sessionsData.sessions[0],
      id: "green-failure",
      agentType: "codex",
      status: "finished",
      form: "app",
      unread: true,
    };
    const source = { sessions: [session], totalCount: 1, waitingCount: 0 };
    mockQueryState.data = source;
    let rejectInvalidation!: () => void;
    let invalidationStarted!: () => void;
    const invalidationRelease = new Promise<void>((resolve) => {
      rejectInvalidation = resolve;
    });
    const invalidation = new Promise<void>((resolve) => {
      invalidationStarted = resolve;
    });
    let rejectRead!: (error: Error) => void;
    const readRequest = new Promise<never>((_, reject) => {
      rejectRead = reject;
    });
    vi.spyOn(queryClient, "invalidateQueries").mockImplementation(async () => {
      // 模拟 active useSessionsQuery 在 pending 仍在时完成 authority refetch。
      mockQueryState.data = { ...source, sessions: [{ ...session }] };
      await act(async () => rerender(<FoxbellPet />));
      invalidationStarted();
      await invalidationRelease;
    });
    vi.mocked(invoke).mockImplementation((cmd: string) => {
      if (cmd === "mark_session_read") return readRequest;
      return Promise.resolve(undefined);
    });
    const { rerender } = render(<FoxbellPet />);
    const card = await screen.findByTestId("pet-card-green-failure");
    fireEvent.click(within(card).getByText("✕"));
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("mark_session_read", expect.anything())
    );
    await waitFor(() => expect(screen.queryByTestId("pet-card-green-failure")).toBeNull());
    await act(async () => {
      rejectRead(new Error("read failed"));
      await readRequest.catch(() => {});
    });
    await invalidation;
    expect(screen.queryByTestId("pet-card-green-failure")).toBeNull();
    rejectInvalidation();
    await waitFor(() => expect(screen.getByTestId("pet-card-green-failure")).toBeTruthy());

    // 卡片回滚可见后，内部 sessionIndexRef 也必须恢复，才能再次关闭。
    const restoredCard = screen.getByTestId("pet-card-green-failure");
    fireEvent.click(within(restoredCard).getByText("✕"));
    await waitFor(() =>
      expect(
        vi.mocked(invoke).mock.calls.filter(([cmd]) => cmd === "mark_session_read")
      ).toHaveLength(2)
    );
    await waitFor(() => expect(screen.getByTestId("pet-card-green-failure")).toBeTruthy());

    // 第二次失败回滚后仍可通过同一 sessionIndexRef 跳转。
    fireEvent.click(screen.getByTestId("pet-card-green-failure"));
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith(
        "focus_session",
        expect.objectContaining({ sessionId: "green-failure" })
      )
    );
  });

  it("失败时较新权威快照已删除会话，不得复活宠物卡", async () => {
    let rejectDismiss!: (error: Error) => void;
    const request = new Promise<never>((_, reject) => {
      rejectDismiss = reject;
    });
    vi.mocked(invoke).mockImplementation((cmd: string) =>
      cmd === "dismiss_session_card" ? request : Promise.resolve(undefined)
    );
    const { rerender } = render(<FoxbellPet />);
    fireEvent.click(within(await screen.findByTestId("pet-card-s1")).getByText("✕"));
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("dismiss_session_card", expect.anything())
    );
    const empty = { sessions: [], totalCount: 0, waitingCount: 0 };
    __resetScanSnapshotVersionsForTest();
    expect(applySessionsSnapshot(queryClient, 10, empty)).toBe(true);
    mockQueryState.data = empty;
    rerender(<FoxbellPet />);
    await act(async () => {
      rejectDismiss(new Error("failed"));
      await request.catch(() => {});
    });
    await waitFor(() => expect(screen.queryByTestId("pet-card-s1")).toBeNull());
  });

  it("dismiss 失败时，别处的 session-read 事件不能替代本次 dismiss 回滚", async () => {
    let rejectDismiss!: (error: Error) => void;
    const dismissRequest = new Promise<never>((_, reject) => {
      rejectDismiss = reject;
    });
    vi.mocked(invoke).mockImplementation((cmd: string) => {
      if (cmd === "dismiss_session_card") return dismissRequest;
      return Promise.resolve(undefined);
    });
    render(<FoxbellPet />);
    const card = await screen.findByTestId("pet-card-s1");
    expect(eventListeners.get("session-read")?.length).toBeGreaterThan(0);
    fireEvent.click(within(card).getByText("✕"));
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("dismiss_session_card", expect.anything())
    );
    await waitFor(() => expect(screen.queryByTestId("pet-card-s1")).toBeNull());

    act(() => emitSessionRead("claude", "s1")); // 另一处标已读；当前 X 仍是 dismiss 操作
    await act(async () => {
      rejectDismiss(new Error("dismiss failed"));
      await dismissRequest.catch(() => {});
      await new Promise((resolve) => setTimeout(resolve, 0));
    });
    expect(screen.getByTestId("pet-card-s1")).toBeTruthy();
  });

  it("歧义候选浮层：点外/Esc 关闭且不 ack，点内不关闭（spec §11）", async () => {
    vi.mocked(invoke).mockImplementation((cmd: string) => {
      if (cmd === "focus_session") {
        return Promise.resolve({
          type: "ambiguous",
          windows: [
            { hwnd: 1, title: "win-a", process: "iTerm2" },
            { hwnd: 2, title: "win-b", process: "iTerm2" },
          ],
        });
      }
      return Promise.resolve(undefined);
    });
    render(<FoxbellPet />);
    fireEvent.click(await screen.findByTestId("pet-card-s1"));
    const overlay = await screen.findByTestId("pet-jump-candidates", undefined, { timeout: 10000 });
    await act(async () => {}); // 排空被动效应：确保浮层关闭监听已注册再触发事件（FIX-11）
    // 点浮层内部：不关闭
    fireEvent.pointerDown(overlay.firstChild as HTMLElement);
    expect(screen.getByTestId("pet-jump-candidates")).toBeTruthy();
    // 点外：关闭（不 ack，卡片保留）
    fireEvent.pointerDown(document.body);
    await waitFor(() => expect(screen.queryByTestId("pet-jump-candidates")).toBeNull(), {
      timeout: 10000,
    });
    expect(screen.getByTestId("pet-card-s1")).toBeTruthy();
    // 再次触发歧义 → Esc 关闭
    fireEvent.click(screen.getByTestId("pet-card-s1"));
    await screen.findByTestId("pet-jump-candidates", undefined, { timeout: 10000 });
    await act(async () => {}); // 排空被动效应（Esc 关闭段，FIX-11）
    fireEvent.keyDown(window, { key: "Escape" });
    await waitFor(() => expect(screen.queryByTestId("pet-jump-candidates")).toBeNull(), {
      timeout: 10000,
    });
    expect(screen.getByTestId("pet-card-s1")).toBeTruthy();
    vi.mocked(invoke).mockClear();
  });
});
