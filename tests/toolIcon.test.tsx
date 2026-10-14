// tests/toolIcon.test.tsx — ToolIcon 薄委托层（2026-10-14 徽标统一批重写）
// 源码侧：ToolIcon 不再自绘 SVG，改为 AGENT_BADGE 查表委托（canonical 图标
// 聚齐 BrandIcons.tsx）；旧断言的 --tool-* CSS 变量体系与旧色值（#0FBF8F /
// #6445A2 等）已随统一批退役，本文件改为锁：①委托链路（查表渲染出 svg）
// ②canonical 图标的关键色值 ③unknown 回退 claude。
// 形状级断言不在此锁（图标重绘属设计自由），专项几何锁见 toolIconZcode.test.tsx。
import { render } from "@testing-library/react";
import { describe, it, expect } from "vitest";
import { ToolIcon } from "@/components/common/ToolIcon";
import { AGENT_BADGE } from "@/lib/agentBadge";

describe("ToolIcon 委托渲染（AGENT_BADGE 查表）", () => {
  it("renders an svg through the badge table (opencode, size box applied)", () => {
    const { container } = render(<ToolIcon toolId="opencode" size={16} />);
    expect(container.querySelector("svg")).toBeTruthy();
  });

  it("dsh renders the canonical DeepSeek blue #4D6BFE", () => {
    const { container } = render(<ToolIcon toolId="dsh" />);
    expect(container.innerHTML).toContain("#4D6BFE");
  });

  it("zcode renders the canonical charcoal #2D2D2D", () => {
    const { container } = render(<ToolIcon toolId="zcode" />);
    expect(container.innerHTML).toContain("#2D2D2D");
  });

  it("openclaw renders the official mascot gradient end #991b1b", () => {
    const { container } = render(<ToolIcon toolId="openclaw" />);
    expect(container.innerHTML).toContain("#991b1b");
  });

  it("workbuddy renders the official app icon bitmap (img, extracted from WorkBuddy.exe)", () => {
    const { container } = render(<ToolIcon toolId="workbuddy" />);
    const img = container.querySelector("img");
    expect(img).toBeTruthy();
    expect(img?.getAttribute("src")).toContain("workbuddy-icon");
  });

  it("opencode renders the canonical lower-core grey #5A5858", () => {
    const { container } = render(<ToolIcon toolId="opencode" />);
    expect(container.innerHTML).toContain("#5A5858");
  });
});

describe("ToolIcon unknown 回退", () => {
  it("falls back to the claude icon for unknown tools (no crash, still an svg)", () => {
    const { container } = render(<ToolIcon toolId="future-tool" />);
    expect(container.querySelector("svg")).toBeTruthy();
  });
});

describe("ToolIcon 委托一致性", () => {
  it("toolId=claude 与直接渲染 AGENT_BADGE.claude.Icon 均产出 svg（委托链路锁，形状不锁）", () => {
    const viaToolIcon = render(<ToolIcon toolId="claude" />);
    expect(viaToolIcon.container.querySelector("svg")).toBeTruthy();
    const direct = render(<AGENT_BADGE.claude.Icon className="h-4 w-4" />);
    expect(direct.container.querySelector("svg")).toBeTruthy();
  });
});
