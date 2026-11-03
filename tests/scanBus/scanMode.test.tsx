// Task 9 / Task 2（扫描总线与自适应档位）：前端档位跟随 + 可见性心跳。
//
// 判据来源：计划 Task 9 步骤 1/3（`useScanMode` 默认 idle、跟随 `scan-bus-mode` 事件；
// 心跳立即发一次 + 事件驱动 + 卸载发 false）与 Task 2（快照事件也携带档位；
// 心跳上报窗口 label 而不是写死的 source）。
//
// 收口点：
//  * 心跳必须带窗口 label（`getCurrentWebviewWindow().label`：主窗 "main"、宠物 "pet"），
//    后端按窗口独立记账；
//  * 事件监听必须随卸载解除（不泄漏）；初始化/事件载荷不合法时保持 idle（浏览器 mock 返回
//    undefined 时 `MODE_INTERVAL_MS[undefined]` 会变 NaN，必须挡在类型守卫里）。
import { act, renderHook, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { describe, expect, it, vi, beforeEach } from "vitest";

type Listener = (e: { payload: unknown }) => void;

const invoke = vi.fn();
/** 事件名 → 回调列表（`scan-bus-mode` 与 `scan-bus-snapshot` 分开收集） */
const listeners = new Map<string, Listener[]>();
/** 当前模拟 WebView 的 label（Task 2：心跳按窗口区分） */
let currentLabel = "main";
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...a: unknown[]) => invoke(...a) }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: async (name: string, cb: Listener) => {
    const arr = listeners.get(name) ?? [];
    arr.push(cb);
    listeners.set(name, arr);
    return () => {};
  },
}));
vi.mock("@tauri-apps/api/webviewWindow", () => ({
  getCurrentWebviewWindow: () => ({ label: currentLabel }),
}));

import { MODE_INTERVAL_MS, useScanBusHeartbeat, useScanMode } from "@/lib/query/queries/scanBus";

/** `useScanMode` 不再依赖 queryClient；保留 Provider 以贴近真实挂载环境 */
function createWrapper() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={client}>{children}</QueryClientProvider>
  );
}

function emit(name: string, payload: unknown) {
  act(() => {
    for (const cb of listeners.get(name) ?? []) cb({ payload });
  });
}

describe("useScanMode", () => {
  beforeEach(() => {
    invoke.mockReset();
    listeners.clear();
    currentLabel = "main";
    invoke.mockResolvedValue({ mode: "idle", version: 1 });
  });

  it("档位间隔表与后端一致：active 2s / idle 10s / away 30s", () => {
    expect(MODE_INTERVAL_MS.active).toBe(2_000);
    expect(MODE_INTERVAL_MS.idle).toBe(10_000);
    expect(MODE_INTERVAL_MS.away).toBe(30_000);
  });

  it("follows backend mode events", async () => {
    const { result } = renderHook(() => useScanMode(), { wrapper: createWrapper() });
    // 初始化：先读一次 scan_bus_status
    await waitFor(() => expect(invoke).toHaveBeenCalledWith("scan_bus_status"));
    await waitFor(() => expect(result.current).toBe(MODE_INTERVAL_MS.idle));
    // 监听注册是异步的（listen 返回 Promise），先等它落地再推事件，避免竞态
    await waitFor(() => expect(listeners.get("scan-bus-mode")?.length).toBe(1));
    emit("scan-bus-mode", { mode: "active" });
    await waitFor(() => expect(result.current).toBe(MODE_INTERVAL_MS.active));
    emit("scan-bus-mode", { mode: "away" });
    await waitFor(() => expect(result.current).toBe(MODE_INTERVAL_MS.away));
  });

  it("优先跟随快照事件携带的 mode（Task 2 主路径）", async () => {
    const { result } = renderHook(() => useScanMode(), { wrapper: createWrapper() });
    await waitFor(() => expect(listeners.get("scan-bus-snapshot")?.length).toBe(1));
    emit("scan-bus-snapshot", { version: 2, epoch: 2, mode: "away" });
    await waitFor(() => expect(result.current).toBe(MODE_INTERVAL_MS.away));
  });

  it("heartbeat 上报窗口 label 与可见性", async () => {
    invoke.mockResolvedValue(undefined);
    renderHook(() => useScanBusHeartbeat());
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("scan_bus_watch", {
        window: "main",
        watching: expect.any(Boolean),
      })
    );
  });

  it("heartbeat 用当前 WebView label 区分窗口（宠物窗口报 pet）", async () => {
    invoke.mockResolvedValue(undefined);
    currentLabel = "pet";
    renderHook(() => useScanBusHeartbeat());
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("scan_bus_watch", {
        window: "pet",
        watching: expect.any(Boolean),
      })
    );
  });

  it("heartbeat 卸载时报告 watching:false（同一窗口 label）", async () => {
    invoke.mockResolvedValue(undefined);
    const { unmount } = renderHook(() => useScanBusHeartbeat());
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith(
        "scan_bus_watch",
        expect.objectContaining({ window: "main" })
      )
    );
    invoke.mockClear();
    unmount();
    expect(invoke).toHaveBeenCalledWith("scan_bus_watch", {
      window: "main",
      watching: false,
    });
  });

  it("enabled=false（通知/宠物窗口的通用心跳）不发心跳", async () => {
    invoke.mockResolvedValue(undefined);
    renderHook(() => useScanBusHeartbeat(false));
    await Promise.resolve();
    expect(invoke).not.toHaveBeenCalledWith("scan_bus_watch", expect.anything());
  });
});
