// Task 2（F4/F6 快照更新通知）：前端主路径改为 `scan-bus-snapshot` 事件。
//
// 契约：
//  * 新版本事件直接写入 `["sessions"]` 缓存（不额外 IPC）；
//  * 相同版本重复事件 / 更旧版本一律忽略（多窗口广播不放大刷新）；
//  * 同一窗口挂多个监听实例（main 根组件 + 页面级）也只写一次；
//  * 不同窗口（不同 label）各自写自己的缓存，互不吞事件。
import { act, renderHook, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { SESSIONS_QUERY_KEY } from "@/lib/query/optimisticSessions";
import type { Session, SessionsResponse } from "@/types/session";

type Listener = (e: { payload: unknown }) => void;

const listeners = new Map<string, Listener[]>();
/** 当前模拟 WebView 的 label（两个窗口用不同 label 模拟） */
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

import {
  __resetScanSnapshotVersionsForTest,
  useScanSnapshotSync,
} from "@/lib/query/queries/scanBus";

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

beforeEach(() => {
  listeners.clear();
  currentLabel = "main";
  __resetScanSnapshotVersionsForTest();
});

describe("useScanSnapshotSync", () => {
  it("新版本事件直接写入 sessions 缓存（payload.data，无额外 IPC）", async () => {
    const client = new QueryClient();
    renderHook(() => useScanSnapshotSync(), { wrapper: wrapperFor(client) });
    await waitFor(() => expect(listeners.get("scan-bus-snapshot")?.length).toBe(1));
    emit("scan-bus-snapshot", payload(3, ["a"]));
    expect(client.getQueryData(SESSIONS_QUERY_KEY)).toEqual(response(["a"]));
  });

  it("相同版本重复事件不重复写缓存", async () => {
    const client = new QueryClient();
    renderHook(() => useScanSnapshotSync(), { wrapper: wrapperFor(client) });
    await waitFor(() => expect(listeners.get("scan-bus-snapshot")?.length).toBe(1));
    const spy = vi.spyOn(client, "setQueryData");
    emit("scan-bus-snapshot", payload(3, ["a"]));
    expect(spy).toHaveBeenCalledTimes(1);
    emit("scan-bus-snapshot", payload(3, ["b"]));
    expect(spy).toHaveBeenCalledTimes(1);
    expect(client.getQueryData(SESSIONS_QUERY_KEY)).toEqual(response(["a"]));
  });

  it("更旧版本事件被忽略（缓存不回退）", async () => {
    const client = new QueryClient();
    renderHook(() => useScanSnapshotSync(), { wrapper: wrapperFor(client) });
    await waitFor(() => expect(listeners.get("scan-bus-snapshot")?.length).toBe(1));
    emit("scan-bus-snapshot", payload(5, ["a"]));
    emit("scan-bus-snapshot", payload(4, ["b"]));
    expect(client.getQueryData(SESSIONS_QUERY_KEY)).toEqual(response(["a"]));
    emit("scan-bus-snapshot", payload(6, ["c"]));
    expect(client.getQueryData(SESSIONS_QUERY_KEY)).toEqual(response(["c"]));
  });

  it("同一窗口多个监听实例（主窗根 + 页面级）只写一次", async () => {
    const client = new QueryClient();
    renderHook(() => useScanSnapshotSync(), { wrapper: wrapperFor(client) });
    renderHook(() => useScanSnapshotSync(), { wrapper: wrapperFor(client) });
    await waitFor(() => expect(listeners.get("scan-bus-snapshot")?.length).toBe(2));
    const spy = vi.spyOn(client, "setQueryData");
    emit("scan-bus-snapshot", payload(4, ["a"]));
    expect(spy).toHaveBeenCalledTimes(1);
  });

  it("不同窗口（main / pet）各自应用一次，互不吞事件", async () => {
    const mainClient = new QueryClient();
    const petClient = new QueryClient();
    currentLabel = "main";
    renderHook(() => useScanSnapshotSync(), { wrapper: wrapperFor(mainClient) });
    currentLabel = "pet";
    renderHook(() => useScanSnapshotSync(), { wrapper: wrapperFor(petClient) });
    await waitFor(() => expect(listeners.get("scan-bus-snapshot")?.length).toBe(2));
    emit("scan-bus-snapshot", payload(7, ["x"]));
    expect(mainClient.getQueryData(SESSIONS_QUERY_KEY)).toEqual(response(["x"]));
    expect(petClient.getQueryData(SESSIONS_QUERY_KEY)).toEqual(response(["x"]));
  });

  it("载荷不合法（版本非数字 / data 形状不对）时安全忽略", async () => {
    const client = new QueryClient();
    renderHook(() => useScanSnapshotSync(), { wrapper: wrapperFor(client) });
    await waitFor(() => expect(listeners.get("scan-bus-snapshot")?.length).toBe(1));
    const spy = vi.spyOn(client, "setQueryData");
    emit("scan-bus-snapshot", { version: "3", data: response(["a"]) });
    emit("scan-bus-snapshot", { version: 3, data: { totalCount: 0 } });
    expect(spy).not.toHaveBeenCalled();
    expect(client.getQueryData(SESSIONS_QUERY_KEY)).toBeUndefined();
  });
});
