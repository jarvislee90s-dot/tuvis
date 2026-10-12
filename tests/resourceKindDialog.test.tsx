// tests/resourceKindDialog.test.tsx — 资源大表弹窗（资源管理页 UI 改版 A2/A3/A4）
// 覆盖：类型预选（initialKind）/ tab 再切 + tab 计数与行数一致 / 卡片 search 过滤弹窗列表 /
// 竖排「全部启用」双行按钮渲染（全部/启用、全启用态 全部/禁用）与点击触发
// handleToggleAll 的 invoke 序列（enable 方向逐工具直发；disable 方向先查类型再删链）/
// 弹窗关闭（X → onOpenChange(false)）
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { ResourceKindDialog } from "@/components/resources/ResourceKindDialog";
import type { SsotResource } from "@/types/extension";

const { invokeMock } = vi.hoisted(() => ({ invokeMock: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));

// tests/setup.ts 未初始化 i18n，显式引入并固定中文（文案按 zh 断言）
import i18n from "@/i18n";

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

const res = (o: Partial<SsotResource>): SsotResource => ({
  name: "x",
  kind: "skill",
  enabledTools: [],
  ...o,
});

/** alpha 全启用（claude+codex）→ 竖排钮态「全部/禁用」；beta 全未启用 → 「全部/启用」 */
const RESOURCES = {
  skills: [
    res({ name: "alpha", enabledTools: ["claude", "codex"] }),
    res({ name: "beta", enabledTools: [] }),
  ],
  mcp: [res({ name: "mcp-x", kind: "mcp", enabledTools: ["claude"] })],
  plugins: [res({ name: "plug-x", kind: "plugin", enabledTools: ["codex"] })],
};

const TOOLS = [
  {
    id: "claude",
    label: "Claude Code",
    skillToggleSupported: true,
    mcpSupported: true,
    pluginSupported: true,
  },
  {
    id: "codex",
    label: "Codex CLI",
    skillToggleSupported: true,
    mcpSupported: true,
    pluginSupported: true,
  },
];

function renderDialog(props?: {
  initialKind?: "skill" | "mcp" | "plugin";
  search?: string;
  onOpenChange?: ReturnType<typeof vi.fn>;
}) {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  render(
    <QueryClientProvider client={queryClient}>
      <ResourceKindDialog
        open
        onOpenChange={props?.onOpenChange ?? vi.fn()}
        initialKind={props?.initialKind ?? "skill"}
        search={props?.search ?? ""}
      />
    </QueryClientProvider>
  );
}

/** 行定位：名字 span → 左列格 → 父级即行 div（行内含竖排钮与工具格） */
const rowOf = (name: string) => screen.getByText(name).closest("div")!.parentElement!;

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockImplementation(async (cmd: string) => {
    if (cmd === "list_enabled_tools") return TOOLS;
    if (cmd === "list_ssot_resources") return RESOURCES;
    if (cmd === "list_resource_bindings") return [];
    if (cmd === "list_tool_residents") return [];
    if (cmd === "check_skill_target_type") return "symlink";
    return [];
  });
});

describe("ResourceKindDialog（大表弹窗）", () => {
  it("类型预选：initialKind=mcp → 直接渲染 MCP 表（mcp-x 在、skill 行不在）", async () => {
    renderDialog({ initialKind: "mcp" });
    expect(await screen.findByText("mcp-x")).toBeInTheDocument();
    expect(screen.queryByText("alpha")).not.toBeInTheDocument();
    expect(screen.queryByText("beta")).not.toBeInTheDocument();
  });

  it("tab 再切：点 Skill tab → skill 表渲染且 tab 计数与行数一致", async () => {
    renderDialog({ initialKind: "mcp" });
    expect(await screen.findByText("mcp-x")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Skill (2)" }));
    expect(await screen.findByText("alpha")).toBeInTheDocument();
    expect(screen.getByText("beta")).toBeInTheDocument();
    expect(screen.queryByText("mcp-x")).not.toBeInTheDocument();
  });

  it("搜索过滤（卡片 search 提升传入）：alpha 命中 → tab 计数 Skill (1)、beta 不渲染", async () => {
    renderDialog({ search: "alpha" });
    expect(await screen.findByText("alpha")).toBeInTheDocument();
    expect(screen.queryByText("beta")).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Skill (1)" })).toBeInTheDocument();
  });

  it("竖排全部启用钮（A4）：未全启用行 = 全部/启用 两行文本；点击 → 逐工具 enable 直发序列", async () => {
    renderDialog();
    await screen.findByText("beta");
    const row = rowOf("beta");
    const btn = within(row).getByRole("button", { name: "全部启用" });
    // 竖排双行文本：上行「全部」+ 下行「启用」
    expect(within(btn).getByText("全部")).toBeInTheDocument();
    expect(within(btn).getByText("启用")).toBeInTheDocument();
    fireEvent.click(btn);
    // beta 全未启用 → claude/codex 各一次 enable（camelCase 参数契约）
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("enable_skill_for_tool_cmd", {
        skillName: "beta",
        toolId: "claude",
      })
    );
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("enable_skill_for_tool_cmd", {
        skillName: "beta",
        toolId: "codex",
      })
    );
  });

  it("全启用态翻转：alpha 全启用 → 全部/禁用；点击 → 先查类型再逐工具删链", async () => {
    renderDialog();
    await screen.findByText("alpha");
    const row = rowOf("alpha");
    const btn = within(row).getByRole("button", { name: "全部禁用" });
    expect(within(btn).getByText("全部")).toBeInTheDocument();
    expect(within(btn).getByText("禁用")).toBeInTheDocument();
    fireEvent.click(btn);
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("disable_skill_for_tool", {
        toolId: "claude",
        skillName: "alpha",
      })
    );
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("disable_skill_for_tool", {
        toolId: "codex",
        skillName: "alpha",
      })
    );
    // disable 方向先过类型检查（symlink → 走删链而非跳过）
    expect(invokeMock).toHaveBeenCalledWith("check_skill_target_type", {
      toolId: "claude",
      skillName: "alpha",
    });
  });

  it("关闭：点 X → onOpenChange(false)", async () => {
    const onOpenChange = vi.fn();
    renderDialog({ onOpenChange });
    expect(await screen.findByText("alpha")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Close" }));
    await waitFor(() => expect(onOpenChange).toHaveBeenCalledWith(false));
  });
});
