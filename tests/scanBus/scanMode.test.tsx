// Task 9（扫描总线与自适应档位）：前端档位跟随 + 可见性心跳。
//
// 判据来源：计划 Task 9 步骤 1/3（`useScanMode` 默认 idle、跟随 `scan-bus-mode` 事件；
// 心跳立即发一次 + 事件驱动 + 卸载发 false）。
// 收口点（Required adaptations）：
//  * 心跳必须带 `source`（主窗 "main"、宠物 "pet"），后端按源记账，单源退出不得清掉另一源；
//  * 事件监听必须随卸载解除（不泄漏）；初始化/事件载荷不合法时保持 idle（浏览器 mock 返回
//    undefined 时 `MODE_INTERVAL_MS[undefined]` 会变 NaN，必须挡在类型守卫里）。
import { act, renderHook, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { describe, expect, it, vi, beforeEach } from "vitest";

const invoke = vi.fn();
const listeners: Array<(e: { payload: { mode: string } }) => void> = [];
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...a: unknown[]) => invoke(...a) }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: async (_n: string, cb: (e: { payload: { mode: string } }) => void) => {
    listeners.push(cb);
    return () => {};
  },
}));

import { useScanMode, MODE_INTERVAL_MS, useScanBusHeartbeat } from "@/lib/query/queries/scanBus";

/** Fix 2 后 `useScanMode` 依赖 `useQueryClient`（事件到达即 invalidate sessions）⇒ 需 Provider */
function createWrapper() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={client}>{children}</QueryClientProvider>
  );
}

describe("useScanMode", () => {
  beforeEach(() => {
    invoke.mockReset();
    listeners.length = 0;
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
    await waitFor(() => expect(listeners).toHaveLength(1));
    act(() => listeners[0]({ payload: { mode: "active" } }));
    await waitFor(() => expect(result.current).toBe(MODE_INTERVAL_MS.active));
    act(() => listeners[0]({ payload: { mode: "away" } }));
    await waitFor(() => expect(result.current).toBe(MODE_INTERVAL_MS.away));
  });

  it("heartbeat reports visibility", async () => {
    invoke.mockResolvedValue(undefined);
    renderHook(() => useScanBusHeartbeat());
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith(
        "scan_bus_watch",
        expect.objectContaining({ watching: expect.any(Boolean) })
      )
    );
    // 主窗心跳必须自报 source，后端按源记账（宠物窗口另报 "pet"）
    expect(invoke).toHaveBeenCalledWith(
      "scan_bus_watch",
      expect.objectContaining({ source: "main" })
    );
  });

  it("heartbeat 卸载时报告 watching:false（同一 source）", async () => {
    invoke.mockResolvedValue(undefined);
    const { unmount } = renderHook(() => useScanBusHeartbeat());
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith(
        "scan_bus_watch",
        expect.objectContaining({ source: "main" })
      )
    );
    invoke.mockClear();
    unmount();
    expect(invoke).toHaveBeenCalledWith("scan_bus_watch", {
      watching: false,
      source: "main",
    });
  });

  it("enabled=false（宠物/通知窗口）不发心跳", async () => {
    invoke.mockResolvedValue(undefined);
    renderHook(() => useScanBusHeartbeat(false));
    await Promise.resolve();
    expect(invoke).not.toHaveBeenCalledWith("scan_bus_watch", expect.anything());
  });
});
