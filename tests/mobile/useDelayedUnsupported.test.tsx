import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, renderHook } from "@testing-library/react";
import useDelayedUnsupported, { UNSUPPORTED_NOTICE_MS } from "@/mobile/useDelayedUnsupported";

/**
 * Task 7 风险用例（fake timers，零墙钟 sleep）：
 * - 不足窗口恢复 → 不显示；
 * - 持续错误达到窗口 → 显示；
 * - 恢复立即隐藏；
 * - 切会话/卸载不受旧 timer 影响（不串台、不 setState-after-unmount）。
 */
describe("useDelayedUnsupported", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("不足窗口恢复不显示（瞬态 Unsupported 不闪烁）", () => {
    const { result, rerender } = renderHook(
      ({ active }: { active: boolean }) => useDelayedUnsupported("s1", active),
      { initialProps: { active: true } }
    );
    act(() => {
      vi.advanceTimersByTime(UNSUPPORTED_NOTICE_MS - 1);
    });
    expect(result.current).toBe(false);
    rerender({ active: false });
    act(() => {
      vi.advanceTimersByTime(UNSUPPORTED_NOTICE_MS * 2);
    });
    expect(result.current).toBe(false);
  });

  it("持续错误达到窗口才显示；恢复立即隐藏", () => {
    const { result, rerender } = renderHook(
      ({ active }: { active: boolean }) => useDelayedUnsupported("s1", active),
      { initialProps: { active: true } }
    );
    act(() => {
      vi.advanceTimersByTime(UNSUPPORTED_NOTICE_MS);
    });
    expect(result.current).toBe(true);
    rerender({ active: false });
    expect(result.current).toBe(false);
  });

  it("切会话：旧 timer 不污染新会话", () => {
    const { result, rerender } = renderHook(
      ({ key, active }: { key: string; active: boolean }) => useDelayedUnsupported(key, active),
      { initialProps: { key: "s1", active: true } }
    );
    act(() => {
      vi.advanceTimersByTime(UNSUPPORTED_NOTICE_MS - 1);
    });
    rerender({ key: "s2", active: true });
    act(() => {
      vi.advanceTimersByTime(1);
    });
    expect(result.current).toBe(false);
    act(() => {
      vi.advanceTimersByTime(UNSUPPORTED_NOTICE_MS);
    });
    expect(result.current).toBe(true);
  });

  it("卸载后旧 timer 不再回写（无 setState-after-unmount）", () => {
    const error = vi.spyOn(console, "error").mockImplementation(() => {});
    const { unmount } = renderHook(() => useDelayedUnsupported("s1", true));
    unmount();
    act(() => {
      vi.advanceTimersByTime(UNSUPPORTED_NOTICE_MS * 2);
    });
    expect(error).not.toHaveBeenCalled();
    error.mockRestore();
  });

  it("active=false 恒不显示（enabled=false / 会话关闭）", () => {
    const { result } = renderHook(() => useDelayedUnsupported("s1", false));
    act(() => {
      vi.advanceTimersByTime(UNSUPPORTED_NOTICE_MS * 2);
    });
    expect(result.current).toBe(false);
  });
});
