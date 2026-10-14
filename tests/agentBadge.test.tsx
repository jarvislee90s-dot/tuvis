import { render } from "@testing-library/react";
import { describe, it, expect } from "vitest";
import { AGENT_BADGE, getAgentLabel } from "@/lib/agentBadge";
import { ToolIcon } from "@/components/common/ToolIcon";
import { SUPPORTED_TOOLS } from "@/config/constants";
import { TOOL_LABELS } from "@/mobile/board-logic";

describe("getAgentLabel", () => {
  it("maps every supported tool to a display name", () => {
    expect(getAgentLabel("kimi")).toBe("Kimi Code");
    expect(getAgentLabel("claude")).toBe("Claude");
    expect(getAgentLabel("opencode")).toBe("OpenCode");
    expect(getAgentLabel("openclaw")).toBe("OpenClaw");
  });

  it("keeps codex app/cli distinction", () => {
    expect(getAgentLabel("codex", "app")).toBe("Codex APP");
    expect(getAgentLabel("codex", "cli")).toBe("Codex CLI");
    expect(getAgentLabel("codex")).toBe("Codex CLI");
  });

  it("falls back to the raw agent type for unknown tools", () => {
    expect(getAgentLabel("future-tool")).toBe("future-tool");
  });
});

describe("AGENT_BADGE", () => {
  it("has a badge entry for every supported tool", () => {
    for (const tool of SUPPORTED_TOOLS) {
      const badge = AGENT_BADGE[tool];
      expect(badge, `missing badge for ${tool}`).toBeDefined();
      expect(badge.label).toBeTruthy();
      expect(badge.className).toContain("border-");
      // 2026-10-11 看板筛选 chips 依赖 iconClass（中性 chip 底上的图标色）：
      // 穷尽锁与 label/className 同点，新工具接入漏列直接红
      expect(badge.iconClass).toContain("text-");
      // 2026-10-14 统一批：brandHex 必填裸色（远程外观预览檐色等消费），锁 #RRGGBB 形态
      expect(badge.brandHex).toMatch(/^#[0-9A-Fa-f]{6}$/);
    }
  });

  // 2026-10-14 统一批：openclaw 已退出 SUPPORTED_TOOLS（不支持消息提醒，不入
  // 看板筛选），但其会话卡 / 通知徽标仍由 AGENT_BADGE 提供——徽标存续与猩红
  // tint 在此显式锁死，防「删清单时误删徽章」
  it("keeps the openclaw badge (red tint) despite its exit from SUPPORTED_TOOLS", () => {
    expect(AGENT_BADGE.openclaw).toBeDefined();
    expect(AGENT_BADGE.openclaw.label).toBe("OpenClaw");
    expect(AGENT_BADGE.openclaw.className).toContain("border-red-500/30");
  });

  // 渲染冒烟穷尽锁：键集派生自 Object.keys(TOOL_LABELS)（Record<AgentType,…>
  // 编译穷尽，第 9 工具接入时键集自动扩张，硬编码清单会静默逃逸覆盖），并互锁
  // AGENT_BADGE 键集与之全等（漏挂徽章/孤儿徽章双向红）。含已退出筛选清单的
  // openclaw——canonical 图标聚齐 BrandIcons 后，任何一图标实现崩溃（如缺
  // import / 组件抛错）在此直接红
  it("renders a visible icon (svg or official bitmap img) for every badge (smoke; key set derived from the exhaustive AgentType record)", () => {
    const keys = Object.keys(TOOL_LABELS);
    expect(Object.keys(AGENT_BADGE).sort()).toEqual([...keys].sort());
    for (const key of keys) {
      const badge = AGENT_BADGE[key];
      expect(badge, `missing badge for ${key}`).toBeDefined();
      const { container } = render(<badge.Icon className="h-4 w-4" />);
      expect(
        container.querySelector("svg, img"),
        `no icon rendered for ${key}`,
      ).toBeTruthy();
    }
  });

  it("kimi badge renders its icon without crashing", () => {
    const { container } = render(<AGENT_BADGE.kimi.Icon className="h-4 w-4" />);
    expect(container.querySelector("svg")).toBeTruthy();
  });
});

describe("ToolIcon", () => {
  it("renders the kimi tool icon", () => {
    const { container } = render(<ToolIcon toolId="kimi" />);
    expect(container.querySelector("svg")).toBeTruthy();
  });

  it("falls back to the claude icon for unknown tools", () => {
    const { container } = render(<ToolIcon toolId="unknown-tool" />);
    expect(container.querySelector("svg")).toBeTruthy();
  });
});
