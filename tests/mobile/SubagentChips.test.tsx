import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import SubagentChips, {
  chipTokenText,
  formatElapsed,
  subagentElapsedMs,
} from "@/mobile/SubagentChips";
import type { SubagentView } from "@/mobile/api";
import i18n from "@/i18n";

// 观察台 T5：chip 改纯展示（拉取上提 SessionDetail）——本文件测
// running 过滤 / 时长自走（tick 不出组件）/ 点击直达 / 空不渲染。

// `7.09万` 断言隐含 zh 前提（issue #160）：chipTokenText 缺省读 i18n 单例，
// jsdom 默认 en-US 会出 `70.88K`——显式钉 zh，断言行零改动。
beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

function sa(id: string, status: "running" | "idle", spawnTs: string | null): SubagentView {
  return {
    id,
    name: `n-${id}`,
    status,
    spawnTs,
    endTs: null,
    description: "设计新建会话两改动实现方案",
    tokens: { input: 1, cacheRead: 0, cacheCreation: 0, output: 0 },
  };
}

beforeEach(() => vi.useFakeTimers());
afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

describe("SubagentChips（纯展示）", () => {
  it("只渲染 running；idle 不出现在 chip 区（消失语义 = 前端过滤）", () => {
    vi.setSystemTime(new Date("2026-10-08T07:36:45Z"));
    render(
      <SubagentChips
        list={[
          sa("a1", "running", "2026-10-08T07:31:07Z"),
          sa("b1", "idle", "2026-10-08T07:00:00Z"),
        ]}
        onOpenDetail={() => {}}
      />
    );
    expect(screen.getByTestId("subagent-chip-a1")).toBeTruthy();
    expect(screen.queryByTestId("subagent-chip-b1")).toBeNull();
  });

  it("list=null / 全 idle / 空 → 不渲染", () => {
    const { container, rerender } = render(<SubagentChips list={null} onOpenDetail={() => {}} />);
    expect(container.querySelector('[data-testid="subagent-chips"]')).toBeNull();
    rerender(<SubagentChips list={[]} onOpenDetail={() => {}} />);
    expect(container.querySelector('[data-testid="subagent-chips"]')).toBeNull();
    rerender(<SubagentChips list={[sa("x", "idle", null)]} onOpenDetail={() => {}} />);
    expect(container.querySelector('[data-testid="subagent-chips"]')).toBeNull();
  });

  it("时长自走（tick 不出组件）；点击 chip → onOpenDetail(id)（§二.4 直达）", async () => {
    vi.setSystemTime(new Date("2026-10-08T07:36:35Z"));
    const onOpen = vi.fn();
    let parentRenders = 0;
    const Probe = () => {
      parentRenders += 1;
      return (
        <SubagentChips list={[sa("a1", "running", "2026-10-08T07:36:35Z")]} onOpenDetail={onOpen} />
      );
    };
    render(<Probe />);
    expect(screen.getByTestId("subagent-chip-a1").textContent).toContain("0:00");
    await vi.advanceTimersByTimeAsync(61_000);
    expect(screen.getByTestId("subagent-chip-a1").textContent).toContain("1:01");
    expect(parentRenders).toBe(1, "1s tick 只重渲染本组件");
    fireEvent.click(screen.getByTestId("subagent-chip-a1"));
    expect(onOpen).toHaveBeenCalledWith("a1");
  });
});

describe("subagentElapsedMs（冻结锚共享口径）", () => {
  it("running：now − spawnTs；idle：endTs − spawnTs；spawnTs=null → null", () => {
    const now = Date.parse("2026-10-08T07:36:45Z");
    expect(subagentElapsedMs({ spawnTs: "2026-10-08T07:31:07Z", endTs: null }, now)).toBe(338_000);
    expect(
      subagentElapsedMs({ spawnTs: "2026-10-08T07:00:00Z", endTs: "2026-10-08T07:30:00Z" }, now)
    ).toBe(1_800_000, "冻结在 endTs，不随 now 走");
    expect(subagentElapsedMs({ spawnTs: null, endTs: null }, now)).toBeNull();
  });
  it("formatElapsed / chipTokenText 既有口径不变", () => {
    expect(formatElapsed(338_000)).toBe("5:38");
    expect(chipTokenText({ input: 5124, cacheRead: 56448, cacheCreation: 0, output: 9312 })).toBe(
      "7.09万"
    );
  });
});
