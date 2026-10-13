// tests/toolFilterChips.test.tsx — 桌面看板工具筛选 chips（2026-10-11）
// 覆盖：「全部」+ 固定 8 工具齐渲染（与用户截图定案一致）、单选 aria-pressed
// 翻转、点击回调携带筛选值。图标/文案与徽章同源的穷尽锁在 agentBadge.test.tsx
//（iconClass 列）与 tests/toolFilter.test.ts（过滤语义）。
import { fireEvent, render, screen } from "@testing-library/react";
import { beforeAll, describe, expect, it, vi } from "vitest";
import { ToolFilterChips } from "@/components/sessions/ToolFilterChips";
import i18n from "@/i18n";

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

describe("ToolFilterChips", () => {
  it("「全部」+ 8 工具齐渲染（溢出交 flex-wrap 换行，不做折叠省略）", () => {
    render(<ToolFilterChips value="all" onChange={vi.fn()} />);
    expect(screen.getByTestId("tool-chips").children).toHaveLength(9);
    expect(screen.getByTestId("tool-chip-all")).toHaveTextContent("全部");
    expect(screen.getByTestId("tool-chip-all")).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByTestId("tool-chip-kimi")).toHaveTextContent("Kimi Code");
    expect(screen.getByTestId("tool-chip-dsh")).toHaveTextContent("DSH");
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
