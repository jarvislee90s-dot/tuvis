import { render } from "@testing-library/react";
import { describe, it, expect } from "vitest";
import { AGENT_BADGE, getAgentLabel } from "@/lib/agentBadge";

// ZCode 徽标专项——zcode 的存在性 / brandHex / 渲染冒烟覆盖由 agentBadge.test.tsx
// 的 SUPPORTED_TOOLS 遍历与穷尽键集冒烟承担；本文件补 zcode 显式 label / 渲染断言
describe("agentBadge zcode", () => {
  it("maps zcode to its display name", () => {
    expect(getAgentLabel("zcode")).toBe("ZCode");
    // APP 形态不改变 ZCode 显示名（无 CLI/APP 双名之分）
    expect(getAgentLabel("zcode", "app")).toBe("ZCode");
  });

  it("has an indigo badge entry and renders its icon without crashing", () => {
    const badge = AGENT_BADGE.zcode;
    expect(badge).toBeDefined();
    expect(badge.label).toBe("ZCode");
    expect(badge.className).toContain("border-");
    const { container } = render(<badge.Icon className="h-4 w-4" />);
    expect(container.querySelector("svg")).toBeTruthy();
  });
});
