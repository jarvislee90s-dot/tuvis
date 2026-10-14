// tests/toolFilterChips.test.tsx — 桌面看板工具筛选 chips（2026-10-11）
// 覆盖：「全部」+ 固定 7 工具齐渲染、单选 aria-pressed 翻转、点击回调携带
// 筛选值。2026-10-14 统一批：openclaw 退出看板筛选 chips（不支持消息提醒，
// 用户裁决不入 SUPPORTED_TOOLS），总数 9 → 8 且 openclaw chip 不再渲染（防回
// 归锁）。图标/文案与徽章同源的穷尽锁在 agentBadge.test.tsx（iconClass 列）
// 与 tests/toolFilter.test.ts（过滤语义）。
import { fireEvent, render, screen } from "@testing-library/react";
import { beforeAll, describe, expect, it, vi } from "vitest";
import { ToolFilterChips } from "@/components/sessions/ToolFilterChips";
import i18n from "@/i18n";

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

describe("ToolFilterChips", () => {
  it("「全部」+ 7 工具齐渲染（溢出交 flex-wrap 换行，不做折叠省略）", () => {
    render(<ToolFilterChips value="all" onChange={vi.fn()} />);
    expect(screen.getByTestId("tool-chips").children).toHaveLength(8);
    expect(screen.getByTestId("tool-chip-all")).toHaveTextContent("全部");
    expect(screen.getByTestId("tool-chip-all")).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByTestId("tool-chip-kimi")).toHaveTextContent("Kimi Code");
    expect(screen.getByTestId("tool-chip-dsh")).toHaveTextContent("DSH");
  });

  it("openclaw 不出现在看板筛选 chips（2026-10-14 退出 SUPPORTED_TOOLS 的防回归锁）", () => {
    render(<ToolFilterChips value="all" onChange={vi.fn()} />);
    expect(screen.queryByTestId("tool-chip-openclaw")).toBeNull();
  });

  it("选中态：aria-pressed 只在当前值翻转（单选）", () => {
    render(<ToolFilterChips value="kimi" onChange={vi.fn()} />);
    expect(screen.getByTestId("tool-chip-all")).toHaveAttribute("aria-pressed", "false");
    expect(screen.getByTestId("tool-chip-kimi")).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByTestId("tool-chip-codex")).toHaveAttribute("aria-pressed", "false");
  });

  it("点击触发 onChange（含回到「全部」）", () => {
    const onChange = vi.fn();
    render(<ToolFilterChips value="all" onChange={onChange} />);
    fireEvent.click(screen.getByTestId("tool-chip-kimi"));
    expect(onChange).toHaveBeenLastCalledWith("kimi");
    fireEvent.click(screen.getByTestId("tool-chip-all"));
    expect(onChange).toHaveBeenLastCalledWith("all");
  });
});
