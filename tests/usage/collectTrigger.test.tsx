// Task 10（扫描总线与自适应档位计划）· 用量可见即采的行为锁。
// F7 整改（2026-10-31）：可见边沿改走**可见性命令**（后端跨 WebView 60s 门闩 + 单飞）；
// 手动路径仍走原采集命令（`force` 语义不变）。
//
// 契约：
//  * `useUsageCollect(active)` 在 active 的 false→true 边沿（含挂载即 active=true）触发一次
//    可见性采集；首次激活必发，60s 内重复可见边沿不再发（前端模块级第一道去抖，跨实例共享）。
//  * 可见性触发**不**调用手动采集命令；手动 `collect()` 仍以 `force=false` 走原命令。
//  * 后端真实采集完成 emit 的 `usage-collected` 到达时失效整个用量域（与 newRecords 无关）。
//
// 测试可控性：模块级时间戳在同一测试文件内跨用例存活，故统一用假 Date（只伪造 `Date`，
// 不伪造定时器——`waitFor` 仍走真实时钟），每个用例把系统时间拨到严格递增的未来 1 小时，
// 上一用例留下的时间戳永远不会挡住本用例的首次可见触发；用例内部的 60s 窗口用
// `vi.setSystemTime` 精确推进。
//
// `@tauri-apps/api/event` 的替身在 `tests/setup.ts`（全局），此处不重复 mock；
// 本文件把用量 API 与 React Query 客户端替掉，只观察采集入参与失效键。
import { act, renderHook, waitFor } from "@testing-library/react";
import { listen } from "@tauri-apps/api/event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

// vi.mock 工厂会被提升到文件顶部；顶层 `const` 在工厂执行时尚未初始化，
// 故用 vi.hoisted 提前拿到同一组 spy（计划代码片段的已知坑，见报告 deviations）。
const { usageCollect, usageCollectVisible, invalidateQueries } = vi.hoisted(() => ({
  usageCollect: vi.fn().mockResolvedValue({}),
  usageCollectVisible: vi.fn().mockResolvedValue({}),
  invalidateQueries: vi.fn(),
}));
vi.mock("@/lib/api/usage", () => ({
  usageCollect,
  usageCollectVisible,
  usageDashboard: vi.fn(),
  usageGetSettings: vi.fn().mockResolvedValue({ enabled: true, collectIntervalMin: 10 }),
  usageRecords: vi.fn(),
}));
vi.mock("@tanstack/react-query", () => ({
  useQueryClient: () => ({ invalidateQueries }),
  useQuery: () => ({ data: { enabled: true, collectIntervalMin: 10 } }),
}));

import { useUsageCollect } from "@/lib/query/queries/usage";

const HOUR = 3_600_000;
// 模块级去抖状态跨用例共享：基准时间整体落在未来且逐用例 +1h（详见文件头）。
let clock = Date.now() + 24 * HOUR;

beforeEach(() => {
  clock += HOUR;
  vi.useFakeTimers({ toFake: ["Date"] });
  vi.setSystemTime(clock);
  usageCollect.mockClear();
  usageCollectVisible.mockClear();
  invalidateQueries.mockClear();
});

afterEach(() => {
  vi.useRealTimers();
});

describe("useUsageCollect", () => {
  it("triggers a visibility collect when the page becomes active", async () => {
    const { rerender } = renderHook(({ active }) => useUsageCollect(active), {
      initialProps: { active: false },
    });

    // 隐藏页不采集：active=false 的挂载帧不得发任何采集。
    expect(usageCollectVisible).not.toHaveBeenCalled();
    expect(usageCollect).not.toHaveBeenCalled();

    rerender({ active: true });

    // 可见边沿 = 可见性采集（后端 60s 门闩裁定是否真扫）；首次激活不受前端去抖约束。
    await waitFor(() => expect(usageCollectVisible).toHaveBeenCalledTimes(1));
    expect(usageCollect).not.toHaveBeenCalled();
  });

  it("debounces repeated activations within 60s", async () => {
    const { rerender } = renderHook(({ active }) => useUsageCollect(active), {
      initialProps: { active: true },
    });
    await waitFor(() => expect(usageCollectVisible).toHaveBeenCalledTimes(1));

    // 60s 内二次可见：去抖命中，不再发。
    rerender({ active: false });
    vi.setSystemTime(clock + 30_000);
    rerender({ active: true });
    expect(usageCollectVisible).toHaveBeenCalledTimes(1);

    // 越过 60s：再次可见恢复触发（是否真扫由后端门闩决定）。
    rerender({ active: false });
    vi.setSystemTime(clock + 61_000);
    rerender({ active: true });
    await waitFor(() => expect(usageCollectVisible).toHaveBeenCalledTimes(2));
  });

  it("shares the 60s debounce across remounts (pet mini-bar hover)", async () => {
    // 挂载即 active=true → 首次可见性采集，写下模块级时间戳。
    const first = renderHook(() => useUsageCollect(true));
    await waitFor(() => expect(usageCollectVisible).toHaveBeenCalledTimes(1));
    first.unmount();

    // 60s 内重挂载（浮窗悬停/托盘菜单反复开关）：新实例沿用模块时间戳，不得再次触发。
    vi.setSystemTime(clock + 30_000);
    const second = renderHook(() => useUsageCollect(true));
    expect(usageCollectVisible).toHaveBeenCalledTimes(1);
    second.unmount();

    // 满 60s：重挂载恢复触发（阈值 `>= 60_000`）。
    vi.setSystemTime(clock + 60_000);
    renderHook(() => useUsageCollect(true));
    await waitFor(() => expect(usageCollectVisible).toHaveBeenCalledTimes(2));
  });

  it("keeps manual collect on the manual command, outside the visibility debounce", async () => {
    const { result } = renderHook(() => useUsageCollect(true));
    await waitFor(() => expect(usageCollectVisible).toHaveBeenCalledTimes(1));

    // 同一 60s 窗口内手动采集：force 默认 false，不走可见性命令、也不受其去抖影响。
    await act(async () => {
      await result.current.collect();
    });
    expect(usageCollect).toHaveBeenCalledTimes(1);
    expect(usageCollect).toHaveBeenLastCalledWith(false);
    expect(usageCollectVisible).toHaveBeenCalledTimes(1);
  });

  it("invalidates the usage domain when the backend emits usage-collected（与 newRecords 无关）", () => {
    renderHook(() => useUsageCollect(false));

    const call = vi
      .mocked(listen)
      .mock.calls.find(([eventName]) => eventName === "usage-collected");
    expect(call).toBeDefined();
    const handler = call![1] as (event: { payload: unknown }) => void;
    // 载荷刻意是「0 条新记录」的完成事件：事件到达即失效，不看 newRecords
    handler({ payload: { collectedAt: 1, success: true, newRecords: 0, sources: [] } });

    expect(invalidateQueries).toHaveBeenCalledWith({ queryKey: ["usage"] });
  });
});
