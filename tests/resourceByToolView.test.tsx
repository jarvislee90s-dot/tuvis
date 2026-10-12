// tests/resourceByToolView.test.tsx — 「按工具」视图（资源管理页 UI 改版 C8/C9）
// 覆盖：每工具卡只显实际配置（assignments enabled 过滤：未链接仓库项不上卡）/
// 来源徽章三态（原生 / 全局仓库 / .agents 可用）与优先级（链接 > .agents；原生同名合并）/
// .agents 可用仅 codex 卡面（detect_legacy_agents_links 面 codex 专属）/
// 计数如实（Skills/MCP 段头计数 = 合并后行数）
import { fireEvent, render, screen, within } from "@testing-library/react";
import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { ResourceByToolView } from "@/components/resources/ResourceByToolView";
import { TOOLS } from "./fixtures/resourceView";
import type {
  AssignmentSummary,
  ExtensionWithAssignments,
  NativeExtension,
  ToolResources,
} from "@/types/extension";

const { invokeMock } = vi.hoisted(() => ({ invokeMock: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));

// tests/setup.ts 未初始化 i18n，显式引入并固定中文（文案按 zh 断言）
import i18n from "@/i18n";

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

const assign = (toolId: string, enabled: boolean): AssignmentSummary[] => [
  { agentToolId: toolId, enabled, linkStatus: enabled ? "linked" : "missing" },
];

const g = (
  id: string,
  kind: string,
  name: string,
  assignments: AssignmentSummary[]
): ExtensionWithAssignments => ({
  id,
  kind,
  name,
  description: null,
  sourcePath: "/tmp/x",
  sourceTool: null,
  suite: null,
  tags: null,
  isNative: false,
  assignments,
});

const nativeSkill = (name: string): NativeExtension => ({
  id: `skill-${name}`,
  kind: "skill",
  name,
  description: null,
  sourcePath: `/home/x/.codex/skills/${name}`,
  sourceTool: "codex",
  detectedAt: "2026-10-12T00:00:00Z",
  imported: false,
});

/** codex 卡数据：链接 2（含同名 .agents 命中的 shared 优先级用例）+ 未链接 1（不上卡）+
 *  仅 .agents 可达 1 + 原生 2（含与全局链接同名的 dup 合并用例）+ MCP 链接 1 / 未链接 1 */
const CODEX_RESOURCES: ToolResources = {
  global: [
    g("skill-linked", "skill", "linked-skill", assign("codex", true)),
    g("skill-shared", "skill", "shared-skill", assign("codex", true)),
    g("skill-unlinked", "skill", "unlinked-skill", assign("codex", false)),
    g("skill-legacy", "skill", "legacy-skill", []),
    g("skill-dup", "skill", "dup-skill", assign("codex", true)),
    g("mcp-1", "mcp", "mcp-1", assign("codex", true)),
    g("mcp-2", "mcp", "mcp-2", []),
  ],
  native: [nativeSkill("native-foo"), nativeSkill("dup-skill")],
};

/** claude 卡数据：链接 1 + 仅 .agents 名册命中 1（非 codex → 不上卡） */
const CLAUDE_RESOURCES: ToolResources = {
  global: [
    g("skill-c", "skill", "claude-skill", assign("claude", true)),
    g("skill-legacy", "skill", "legacy-skill", []),
  ],
  native: [],
};

/** detect_legacy_agents_links 名册：shared-skill（已链接，优先级用例）与 legacy-skill 命中 */
const LEGACY_AGENTS = ["legacy-skill", "shared-skill"];

async function renderView() {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  render(
    <QueryClientProvider client={queryClient}>
      <ResourceByToolView />
    </QueryClientProvider>
  );
  // 等两工具卡数据落地（codex 卡先就绪即可驱动后续断言的 findBy）
  await screen.findByText("linked-skill");
  await screen.findByText("claude-skill");
}

/** 行定位：名字 span → 最近 div 父级即行（行右端为徽章/导入钮容器） */
const rowOf = (name: string) => {
  const span = screen.getAllByText(name)[0];
  return span.closest("div[class*='justify-between']")!;
};

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockImplementation(async (cmd: string, args?: { toolId?: string }) => {
    if (cmd === "list_enabled_tools") return TOOLS;
    if (cmd === "list_tool_resources") {
      return args?.toolId === "codex" ? CODEX_RESOURCES : CLAUDE_RESOURCES;
    }
    if (cmd === "detect_legacy_agents_links") return LEGACY_AGENTS;
    if (cmd === "detect_duplicate_skills") return [];
    return [];
  });
});

describe("ResourceByToolView（实际配置 + 来源徽章三态）", () => {
  it("assignments 过滤：已链接项上卡（全局仓库徽章），未链接仓库项不上卡", async () => {
    await renderView();
    // 已链接 → 上卡佩「全局仓库」徽章
    expect(within(rowOf("linked-skill")).getByText("全局仓库")).toBeInTheDocument();
    // 未链接 → 不出现在任何工具卡
    expect(screen.queryByText("unlinked-skill")).not.toBeInTheDocument();
    expect(screen.queryByText("mcp-2")).not.toBeInTheDocument();
  });

  it("来源徽章三态：.agents 可用（仅名册命中且未链接）/ 原生（含导入钮）各就其位", async () => {
    await renderView();
    // 仅 .agents 可达 → 「.agents 可用」徽章（codex 卡）
    expect(within(rowOf("legacy-skill")).getByText(".agents 可用")).toBeInTheDocument();
    // 原生项 → 「原生」徽章 + 保留导入动作
    const nativeRow = rowOf("native-foo");
    expect(within(nativeRow).getByText("原生")).toBeInTheDocument();
    expect(within(nativeRow).getByRole("button", { name: "导入" })).toBeInTheDocument();
  });

  it("优先级与同名合并：链接 > .agents（shared-skill 佩全局仓库）；原生 > 链接（dup-skill 单行佩原生）", async () => {
    await renderView();
    // shared-skill 已链接且在 .agents 名册 → 佩「全局仓库」而非「.agents 可用」
    expect(within(rowOf("shared-skill")).getByText("全局仓库")).toBeInTheDocument();
    // dup-skill 原生与全局链接同名 → 只出一行，佩高优先级「原生」徽章（含导入钮）
    expect(screen.getAllByText("dup-skill").length).toBe(1);
    const dupRow = rowOf("dup-skill");
    expect(within(dupRow).getByText("原生")).toBeInTheDocument();
    expect(within(dupRow).getByRole("button", { name: "导入" })).toBeInTheDocument();
  });

  it("codex 专属面：claude 卡不显示 .agents 可用项（legacy-skill 不上 claude 卡）", async () => {
    await renderView();
    // legacy-skill 只出现在 codex 卡（1 处），claude 卡上不存在
    expect(screen.getAllByText("legacy-skill").length).toBe(1);
    // claude 卡自身：链接项佩全局仓库徽章
    expect(within(rowOf("claude-skill")).getByText("全局仓库")).toBeInTheDocument();
  });

  it("计数如实：codex 卡 Skills (5)（链接2 + .agents1 + 原生2 - 同名合并1）、MCP (1)", async () => {
    await renderView();
    const codexCard = screen
      .getByText("Codex", { selector: "h3" })
      .closest("div[class*='rounded border p-3']")!;
    expect(within(codexCard).getByText("Skills (5)")).toBeInTheDocument();
    expect(within(codexCard).getByText("MCP (1)")).toBeInTheDocument();
    expect(within(codexCard).getByText("mcp-1")).toBeInTheDocument();
  });

  it("搜索过滤作用于合并后行：输入 legacy → 仅命中行保留", async () => {
    await renderView();
    fireEvent.change(screen.getByPlaceholderText("按名称筛选…"), {
      target: { value: "legacy" },
    });
    expect(screen.getByText("legacy-skill")).toBeInTheDocument();
    expect(screen.queryByText("linked-skill")).not.toBeInTheDocument();
    expect(screen.queryByText("native-foo")).not.toBeInTheDocument();
  });
});
