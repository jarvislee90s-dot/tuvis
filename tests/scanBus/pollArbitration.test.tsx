// 复核 R-3（F4 消费方改造）：兜底轮询与快照事件必须共用同一道版本仲裁。
//
// 症状：查询先取回旧快照 N，事件把 N+1 写进缓存，随后查询完成又把缓存覆盖回 N。
// 事件侧的版本水位（`snapshotVersionByWindow`）拦不住这次回写——它只约束事件，
// 而 React Query 对「查询结果落地」没有版本概念。
//
// 三条写缓存路径（快照事件 / 变更结算 / 兜底轮询）都必须走同一个闸门：
//  * 更旧的快照一律不落缓存；
//  * 在途轮询期间已有更新事件写入 → 丢弃这份可能更旧的查询结果；
//  * 没有事件插队的正常轮询照常回写（兜底通道不能被闸死）。
import { act, renderHook, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";

import {
  SESSIONS_QUERY_KEY,
  __resetScanSnapshotVersionsForTest,
  __resetSessionMutationWaitersForTest,
  applyOptimisticSessionMutation,
  applyPendingSessions,
  clearPendingSessions,
  lastAppliedSnapshotVersion,
  settleSessionMutation,
} from "@/lib/query/optimisticSessions";
import type { SessionsSnapshotRead } from "@/lib/api/session";
import type { Session, SessionsResponse } from "@/types/session";

type Listener = (e: { payload: unknown }) => void;

const listeners = new Map<string, Listener[]>();
let currentLabel = "main";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(async () => undefined) }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: async (name: string, cb: Listener) => {
    const arr = listeners.get(name) ?? [];
    arr.push(cb);
    listeners.set(name, arr);
    return () => {
      listeners.set(
        name,
        (listeners.get(name) ?? []).filter((f) => f !== cb)
      );
    };
  },
}));
vi.mock("@tauri-apps/api/webviewWindow", () => ({
  getCurrentWebviewWindow: () => ({ label: currentLabel }),
}));

// 兜底轮询的数据源：由用例掌控 resolve 时机，用来构造「查询在途时事件先到」的交错
const getSessionsSnapshot = vi.fn();
vi.mock("@/lib/api/session", () => ({
  getSessionsSnapshot: (...a: unknown[]) => getSessionsSnapshot(...a),
}));

import { useScanSnapshotSync } from "@/lib/query/queries/scanBus";
import { useSessionsQuery } from "@/lib/query/queries/sessions";

const mkSession = (id: string): Session => ({
  id,
  agentType: "claude",
  projectName: "项目A",
  projectPath: "/a",
  title: null,
  gitBranch: null,
  githubUrl: null,
  status: "idle",
  lastMessage: null,
  lastMessageRole: null,
  lastMessageSubagentReport: false,
  flapFromSubagentActivity: false,
  lastActivityAt: "",
  pid: 1,
  cpuUsage: 0,
  activeSubagentCount: 0,
  form: "cli",
  jumpSupported: true,
  unread: false,
});

const response = (ids: string[]): SessionsResponse => ({
  sessions: ids.map(mkSession),
  totalCount: ids.length,
  waitingCount: 0,
});

const snapshotRead = (version: number, ids: string[], epoch = version): SessionsSnapshotRead => ({
  version,
  epoch,
  mode: "idle",
  degraded: false,
  data: response(ids),
});

const payload = (version: number, ids: string[]) => ({
  version,
  epoch: version,
  reason: "event",
  scannedAtMs: 1_700_000_000_000 + version,
  mode: "active",
  degraded: false,
  data: response(ids),
});

function wrapperFor(client: QueryClient) {
  return ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={client}>{children}</QueryClientProvider>
  );
}

function emit(name: string, value: unknown) {
  act(() => {
    for (const cb of listeners.get(name) ?? []) cb({ payload: value });
  });
}

/** 外部可控的 deferred，用来把「查询在途」与「事件到达」卡在任意相对顺序 */
function deferred<T>() {
  let resolve!: (v: T) => void;
  const promise = new Promise<T>((r) => {
    resolve = r;
  });
  return { promise, resolve };
}

beforeEach(() => {
  listeners.clear();
  currentLabel = "main";
  getSessionsSnapshot.mockReset();
  clearPendingSessions([
    { id: "hidden", agentType: "codex" },
    { id: "read", agentType: "claude" },
  ]);
  __resetScanSnapshotVersionsForTest();
  __resetSessionMutationWaitersForTest();
});

describe("兜底轮询与快照事件的版本仲裁", () => {
  it("在途查询取回旧快照时，不得覆盖事件已写入的新快照", async () => {
    const call = deferred<SessionsSnapshotRead>();
    getSessionsSnapshot.mockReturnValue(call.promise);

    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    renderHook(
      () => {
        useScanSnapshotSync();
        return useSessionsQuery();
      },
      { wrapper: wrapperFor(client) }
    );
    await waitFor(() => expect(getSessionsSnapshot).toHaveBeenCalled());

    // 1) 查询已在途（它读到的是旧快照 N）；2) 事件 N+1 先把缓存写好
    emit("scan-bus-snapshot", payload(6, ["new"]));
    expect(client.getQueryData(SESSIONS_QUERY_KEY)).toEqual(response(["new"]));

    // 查询的 queryFn 已读完旧 watermark 后，先排队它的 resolve continuation，再排队事件；
    // 事件会在 React Query 真正 setData 前到达，暴露 queryFn 预检查之后的异步窗口。
    call.resolve(snapshotRead(5, ["stale"]));
    queueMicrotask(() => emit("scan-bus-snapshot", payload(7, ["newest"])));
    await waitFor(() => expect(client.isFetching(SESSIONS_QUERY_KEY)).toBeFalsy());
    expect(client.getQueryData(SESSIONS_QUERY_KEY)).toEqual(response(["newest"]));
  });

  it("没有事件插队时，兜底轮询照常回写（闸门不得闸死兜底通道）", async () => {
    getSessionsSnapshot.mockResolvedValue(snapshotRead(1, ["polled"]));
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    renderHook(
      () => {
        useScanSnapshotSync();
        return useSessionsQuery();
      },
      { wrapper: wrapperFor(client) }
    );

    await waitFor(() =>
      expect(client.getQueryData(SESSIONS_QUERY_KEY)).toEqual(response(["polled"]))
    );
    expect(lastAppliedSnapshotVersion("main")).toBe(1);
  });

  it("新轮询提交后迟到的旧事件不得回退缓存", async () => {
    getSessionsSnapshot.mockResolvedValue(snapshotRead(9, ["poll-newer"], 12));
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    renderHook(
      () => {
        useScanSnapshotSync();
        return useSessionsQuery();
      },
      { wrapper: wrapperFor(client) }
    );

    await waitFor(() =>
      expect(client.getQueryData(SESSIONS_QUERY_KEY)).toEqual(response(["poll-newer"]))
    );
    expect(lastAppliedSnapshotVersion("main")).toBe(9);
    const polled = client.getQueryData(SESSIONS_QUERY_KEY);
    emit("scan-bus-snapshot", payload(8, ["event-older"]));
    expect(client.getQueryData(SESSIONS_QUERY_KEY)).toEqual(polled);
  });

  it("事件先写入、随后事件重复广播同一版本不写缓存；更新版本才写", async () => {
    getSessionsSnapshot.mockReturnValue(new Promise<SessionsSnapshotRead>(() => {}));
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    renderHook(
      () => {
        useScanSnapshotSync();
        return useSessionsQuery();
      },
      { wrapper: wrapperFor(client) }
    );
    // useScanSnapshotSync 与 useScanMode 各挂一个 scan-bus-snapshot 监听
    await waitFor(() => expect((listeners.get("scan-bus-snapshot")?.length ?? 0) >= 1).toBe(true));

    const spy = vi.spyOn(client, "setQueryData");
    emit("scan-bus-snapshot", payload(3, ["a"]));
    emit("scan-bus-snapshot", payload(2, ["b"]));
    expect(spy).toHaveBeenCalledTimes(1);
    emit("scan-bus-snapshot", payload(4, ["c"]));
    expect(spy).toHaveBeenCalledTimes(2);
    expect(client.getQueryData(SESSIONS_QUERY_KEY)).toEqual(response(["c"]));
  });

  it("轮询取回与水位同源的更新数据后，事件仍能按版本继续推进（不互相卡死）", async () => {
    getSessionsSnapshot.mockResolvedValue(snapshotRead(1, ["p1"]));
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    renderHook(
      () => {
        useScanSnapshotSync();
        return useSessionsQuery();
      },
      { wrapper: wrapperFor(client) }
    );
    await waitFor(() => expect(client.getQueryData(SESSIONS_QUERY_KEY)).toEqual(response(["p1"])));

    emit("scan-bus-snapshot", payload(9, ["e9"]));
    expect(client.getQueryData(SESSIONS_QUERY_KEY)).toEqual(response(["e9"]));

    // 事件之后再次轮询：无插队 → 正常回写
    getSessionsSnapshot.mockResolvedValue(snapshotRead(10, ["p2"], 11));
    await act(async () => {
      await client.refetchQueries({ queryKey: SESSIONS_QUERY_KEY });
    });
    expect(client.getQueryData(SESSIONS_QUERY_KEY)).toEqual(response(["p2"]));
    expect(lastAppliedSnapshotVersion("main")).toBe(10);
  });

  it("事件丢失时由轮询提交的较新 epoch 收敛 mutation waiter", async () => {
    getSessionsSnapshot
      .mockResolvedValueOnce(snapshotRead(1, ["hide"], 1))
      .mockResolvedValue(snapshotRead(2, [], 6));
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    renderHook(
      () => {
        useScanSnapshotSync();
        return useSessionsQuery();
      },
      { wrapper: wrapperFor(client) }
    );
    await waitFor(() =>
      expect(client.getQueryData(SESSIONS_QUERY_KEY)).toEqual(response(["hide"]))
    );

    const hidden = client.getQueryData<SessionsResponse>(SESSIONS_QUERY_KEY)!.sessions[0];
    const ticket = applyOptimisticSessionMutation(client, [hidden])!;
    const invalidate = vi.spyOn(client, "invalidateQueries");
    await settleSessionMutation(client, ticket, { applied: false, version: 1, epoch: 5 });
    await waitFor(() => expect(listeners.get("scan-bus-snapshot")?.length).toBeGreaterThan(0));

    // 没有 scan-bus-snapshot 事件；只有权威 poll 的 version/epoch 进入最终提交闸。
    await act(async () => {
      await client.refetchQueries({ queryKey: SESSIONS_QUERY_KEY });
    });
    await waitFor(() => expect(invalidate).toHaveBeenCalledTimes(1));
    expect(client.getQueryData(SESSIONS_QUERY_KEY)).toEqual(response([]));
    expect(applyPendingSessions([hidden])).toEqual([hidden]);
  });

  it("失败结算恢复同版本权威快照，并保留其它有效 pending", async () => {
    const hidden = {
      ...mkSession("hidden"),
      agentType: "codex" as const,
      form: "app" as const,
      unread: true,
    };
    const stillPending = { ...mkSession("read"), unread: true };
    const keep = mkSession("keep");
    const authoritative: SessionsResponse = {
      sessions: [hidden, stillPending, keep],
      totalCount: 3,
      waitingCount: 0,
    };
    const sameVersion = {
      version: 1,
      epoch: 1,
      mode: "idle" as const,
      degraded: false,
      data: authoritative,
    };
    getSessionsSnapshot.mockResolvedValue(sameVersion);
    clearPendingSessions([
      { id: hidden.id, agentType: hidden.agentType },
      { id: stillPending.id, agentType: stillPending.agentType },
    ]);

    const client = new QueryClient({
      defaultOptions: { queries: { retry: false, refetchInterval: false } },
    });
    renderHook(() => useSessionsQuery(), { wrapper: wrapperFor(client) });
    await waitFor(() => expect(client.getQueryData(SESSIONS_QUERY_KEY)).toEqual(authoritative));

    const failedTicket = applyOptimisticSessionMutation(client, [hidden])!;
    const otherTicket = applyOptimisticSessionMutation(client, [stillPending])!;
    expect(
      client.getQueryData<SessionsResponse>(SESSIONS_QUERY_KEY)?.sessions.map((s) => s.id)
    ).toEqual(["read", "keep"]);

    // 真实 useSessionsQuery 的 structuralSharing 收到同版本回取时保留 oldData，
    // 因此失败结算必须从已登记的 authority 数据源恢复，而不能假设 invalidate 会覆盖缓存。
    await settleSessionMutation(client, failedTicket, null);

    const restored = client.getQueryData<SessionsResponse>(SESSIONS_QUERY_KEY)!;
    expect(restored.sessions.map((s) => s.id)).toEqual(["hidden", "read", "keep"]);
    expect(restored.sessions.find((s) => s.id === "read")?.unread).toBe(false);
    expect(restored.totalCount).toBe(3);
    expect(otherTicket.targets).toEqual([{ id: "read", agentType: "claude" }]);
    clearPendingSessions([{ id: stillPending.id, agentType: stillPending.agentType }]);
  });

  it("applied=false 超时结算恢复当前权威快照并保留其它 pending", async () => {
    const hidden = {
      ...mkSession("hidden"),
      agentType: "codex" as const,
      form: "app" as const,
      unread: true,
    };
    const stillPending = { ...mkSession("read"), unread: true };
    const keep = mkSession("keep");
    const authoritative: SessionsResponse = {
      sessions: [hidden, stillPending, keep],
      totalCount: 3,
      waitingCount: 0,
    };
    getSessionsSnapshot.mockResolvedValue({
      version: 1,
      epoch: 1,
      mode: "idle",
      degraded: false,
      data: authoritative,
    });
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const hook = renderHook(() => useSessionsQuery(), { wrapper: wrapperFor(client) });
    await waitFor(() => expect(client.getQueryData(SESSIONS_QUERY_KEY)).toEqual(authoritative));

    const timedOutTicket = applyOptimisticSessionMutation(client, [hidden])!;
    applyOptimisticSessionMutation(client, [stillPending]);
    await settleSessionMutation(
      client,
      timedOutTicket,
      { applied: false, version: 1, epoch: 10 },
      { timeoutMs: 20 }
    );
    await waitFor(() => expect(listeners.get("scan-bus-snapshot")?.length).toBeGreaterThan(0));
    await new Promise((resolve) => setTimeout(resolve, 40));
    await waitFor(() => expect(client.isFetching(SESSIONS_QUERY_KEY)).toBe(0));

    const restored = client.getQueryData<SessionsResponse>(SESSIONS_QUERY_KEY)!;
    expect(restored.sessions.map((s) => s.id)).toEqual(["hidden", "read", "keep"]);
    expect(restored.sessions.find((s) => s.id === "read")?.unread).toBe(false);
    expect(restored.totalCount).toBe(3);
    hook.unmount();
  });
});
