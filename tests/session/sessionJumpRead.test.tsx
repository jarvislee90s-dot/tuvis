import { act, renderHook, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { PropsWithChildren } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

type Listener = (event: { payload: unknown }) => void;
const { invokeMock, listeners } = vi.hoisted(() => ({
  invokeMock: vi.fn(),
  listeners: new Map<string, Listener[]>(),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args) }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: async (name: string, callback: Listener) => {
    const group = listeners.get(name) ?? [];
    group.push(callback);
    listeners.set(name, group);
    return () =>
      listeners.set(
        name,
        (listeners.get(name) ?? []).filter((item) => item !== callback)
      );
  },
}));

import i18n from "@/i18n";
import {
  __resetScanSnapshotVersionsForTest,
  __resetSessionMutationWaitersForTest,
  applyPendingSessions,
  SESSIONS_QUERY_KEY,
} from "@/lib/query/optimisticSessions";
import { useSessionJump } from "@/hooks/useSessionJump";
import type { Session, SessionsResponse } from "@/types/session";

void i18n;

const mk = (over: Partial<Session>): Session => ({
  id: "s1",
  agentType: "claude",
  projectName: "Demo",
  projectPath: "/demo",
  title: "Session",
  gitBranch: null,
  githubUrl: null,
  status: "idle",
  lastMessage: null,
  lastMessageRole: null,
  lastMessageSubagentReport: false,
  flapFromSubagentActivity: false,
  lastActivityAt: "",
  pid: 42,
  cpuUsage: 0,
  activeSubagentCount: 0,
  form: "app",
  jumpSupported: true,
  unread: true,
  ...over,
});

const seed = (sessions: Session[]): SessionsResponse => ({
  sessions,
  totalCount: sessions.length,
  waitingCount: sessions.filter((session) => session.status === "waiting").length,
});

const emitSnapshot = async (epoch: number, version: number, sessions: Session[]) => {
  await act(async () => {
    for (const listener of listeners.get("scan-bus-snapshot") ?? []) {
      listener({ payload: { epoch, version, data: seed(sessions) } });
    }
  });
};

function wrapperFor(client: QueryClient) {
  return ({ children }: PropsWithChildren) => (
    <QueryClientProvider client={client}>{children}</QueryClientProvider>
  );
}

beforeEach(() => {
  listeners.clear();
  invokeMock.mockReset();
  __resetScanSnapshotVersionsForTest();
  __resetSessionMutationWaitersForTest();
});

afterEach(() => {
  __resetSessionMutationWaitersForTest();
});

describe("useSessionJump 的显式已读结算", () => {
  it("直达成功后立即清 unread，旧 epoch 不收口，越过 readGateEpoch 才收口", async () => {
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const unread = mk({ agentType: "codex", status: "finished", unread: true });
    client.setQueryData(SESSIONS_QUERY_KEY, seed([unread]));
    invokeMock.mockResolvedValue({ type: "focused", via: "deep-link", readGateEpoch: 5 });
    const { result } = renderHook(() => useSessionJump(), { wrapper: wrapperFor(client) });

    await act(async () => {
      await result.current.focus({
        pid: unread.pid,
        id: unread.id,
        agentType: unread.agentType,
        projectName: unread.projectName,
        unread: true,
        form: "app",
      });
    });

    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(invokeMock).toHaveBeenCalledWith("focus_session", expect.anything());
    expect(client.getQueryData<SessionsResponse>(SESSIONS_QUERY_KEY)?.sessions[0].unread).toBe(
      false
    );
    expect(client.getQueryData<SessionsResponse>(SESSIONS_QUERY_KEY)?.sessions).toHaveLength(1);
    await waitFor(() => expect(listeners.get("scan-bus-snapshot")?.length).toBe(1));

    await emitSnapshot(5, 2, [unread]);
    expect(applyPendingSessions([unread])[0].unread).toBe(false);

    await emitSnapshot(6, 3, [mk({ ...unread, unread: false })]);
    await waitFor(() => expect(applyPendingSessions([unread])[0].unread).toBe(true));
  });

  it("歧义分支只在 focusHwnd 成功后走显式已读", async () => {
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const unread = mk({ id: "ambiguous", unread: true });
    client.setQueryData(SESSIONS_QUERY_KEY, seed([unread]));
    invokeMock.mockImplementation(async (command: string) => {
      if (command === "focus_session") {
        return { type: "ambiguous", windows: [{ hwnd: 77, title: "Demo", process: "Terminal" }] };
      }
      if (command === "focus_hwnd") return undefined;
      if (command === "mark_session_read") return { applied: false, version: 2, epoch: 10 };
      return undefined;
    });
    const { result } = renderHook(() => useSessionJump(), { wrapper: wrapperFor(client) });

    await act(async () => {
      await result.current.focus({
        pid: unread.pid,
        id: unread.id,
        agentType: unread.agentType,
        projectName: unread.projectName,
        unread: true,
      });
    });
    expect(client.getQueryData<SessionsResponse>(SESSIONS_QUERY_KEY)?.sessions[0].unread).toBe(
      true
    );
    expect(invokeMock).not.toHaveBeenCalledWith("mark_session_read", expect.anything());

    await act(async () => {
      await result.current.focusHwnd(77);
    });
    expect(invokeMock).toHaveBeenCalledWith("mark_session_read", {
      agentType: unread.agentType,
      sessionId: unread.id,
    });
    expect(client.getQueryData<SessionsResponse>(SESSIONS_QUERY_KEY)?.sessions[0].unread).toBe(
      false
    );
  });

  it("歧义窗口聚焦失败时不发起已读", async () => {
    const client = new QueryClient();
    const unread = mk({ id: "failed", unread: true });
    client.setQueryData(SESSIONS_QUERY_KEY, seed([unread]));
    invokeMock.mockImplementation(async (command: string) => {
      if (command === "focus_session") {
        return { type: "ambiguous", windows: [{ hwnd: 77, title: "Demo", process: "Terminal" }] };
      }
      if (command === "focus_hwnd") throw new Error("focus failed");
      return undefined;
    });
    const { result } = renderHook(() => useSessionJump(), { wrapper: wrapperFor(client) });
    await act(async () => {
      await result.current.focus({
        pid: unread.pid,
        id: unread.id,
        agentType: unread.agentType,
        projectName: unread.projectName,
        unread: true,
      });
    });
    await expect(result.current.focusHwnd(77)).rejects.toThrow("focus failed");
    expect(invokeMock).not.toHaveBeenCalledWith("mark_session_read", expect.anything());
    expect(client.getQueryData<SessionsResponse>(SESSIONS_QUERY_KEY)?.sessions[0].unread).toBe(
      true
    );
  });

  it("没有 readGateEpoch 的 macOS deep-link 不触发已读回退", async () => {
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const unread = mk({ id: "unverified-link", unread: true });
    client.setQueryData(SESSIONS_QUERY_KEY, seed([unread]));
    invokeMock.mockResolvedValue({ type: "focused", via: "deep-link" });
    const { result } = renderHook(() => useSessionJump(), { wrapper: wrapperFor(client) });

    await act(async () => {
      await result.current.focus({
        pid: unread.pid,
        id: unread.id,
        agentType: unread.agentType,
        projectName: unread.projectName,
        unread: true,
        form: "app",
      });
    });

    expect(invokeMock).toHaveBeenCalledTimes(1);
    expect(invokeMock).toHaveBeenCalledWith("focus_session", expect.anything());
    expect(invokeMock).not.toHaveBeenCalledWith("mark_session_read", expect.anything());
    expect(client.getQueryData<SessionsResponse>(SESSIONS_QUERY_KEY)?.sessions[0].unread).toBe(
      true
    );
  });

  it("通知窗口没有会话缓存时，歧义聚焦成功仍持久标已读", async () => {
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    invokeMock.mockImplementation(async (command: string) => {
      if (command === "focus_session") {
        return { type: "ambiguous", windows: [{ hwnd: 88, title: "Demo", process: "Terminal" }] };
      }
      return undefined;
    });
    const { result } = renderHook(() => useSessionJump(), { wrapper: wrapperFor(client) });

    await act(async () => {
      await result.current.focus({
        pid: 42,
        id: "popup-unread",
        agentType: "claude",
        projectName: "Demo",
        unread: true,
      });
    });
    await act(async () => {
      await result.current.focusHwnd(88);
    });

    expect(invokeMock).toHaveBeenCalledWith("mark_session_read", {
      agentType: "claude",
      sessionId: "popup-unread",
    });
  });
});
