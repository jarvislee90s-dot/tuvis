// tests/presetList.test.tsx — 预设组 v2 列表（Task 4 骨架 + Task 6 预设×工具开关）
// 覆盖：双分区渲染（通用 / 工具私有按绑定工具分组）、activePresets 驱动 Switch checked、
// 能力门控（工具不支持的资源类型 → 未激活开关 disabled；已激活保持可操作以走 restore）
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { PresetList } from "@/components/presets/PresetList";
import type { EnabledTool } from "@/lib/query/queries/tools";
import type { ExtensionWithAssignments } from "@/types/extension";

const { invokeMock, toastMock } = vi.hoisted(() => ({
  invokeMock: vi.fn(),
  // 终审 Minor #1：捕获 toast 文案，锁定 restore 失败分支的专用文案（不与 applyFailed 混用）
  toastMock: { success: vi.fn(), error: vi.fn(), warning: vi.fn(), info: vi.fn() },
}));
vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));
vi.mock("sonner", () => ({ toast: toastMock }));

// tests/setup.ts 未初始化 i18n，显式引入并固定中文（文案按 zh 断言）
import i18n from "@/i18n";

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

const TOOL_CLAUDE: EnabledTool = {
  id: "claude",
  label: "Claude Code",
  skillToggleSupported: true,
  mcpSupported: true,
  pluginSupported: true,
};

/** 能力全关的工具（门控用例：任何资源类型都不支持启停） */
const TOOL_DSH: EnabledTool = {
  id: "dsh",
  label: "Dsh",
  skillToggleSupported: false,
  mcpSupported: false,
  pluginSupported: false,
};

const item = (extensionId: string, kind: string, extensionName: string) => ({
  extensionId,
  kind,
  extensionName,
});

const UNIVERSAL_PRESET = {
  id: "p-univ",
  name: "通用组合",
  description: "全工具通用",
  scope: "universal",
  boundTool: null,
  items: [item("s1", "skill", "skill-a")],
};

const CODEX_PRESET = {
  id: "p-codex",
  name: "Codex 专属",
  description: "",
  scope: "tool",
  boundTool: "codex",
  items: [item("s2", "skill", "skill-b")],
};

/** 挂载 PresetList（真实 React Query，invoke 全走 invokeMock），等待列表数据落地 */
async function renderPresetList(opts: {
  enabledTools?: EnabledTool[];
  presets?: unknown[];
  activePresets?: unknown[];
  extensions?: ExtensionWithAssignments[];
}) {
  invokeMock.mockImplementation(async (cmd: string) => {
    if (cmd === "list_enabled_tools") return opts.enabledTools ?? [TOOL_CLAUDE];
    if (cmd === "list_presets") return opts.presets ?? [];
    if (cmd === "list_active_presets") return opts.activePresets ?? [];
    // list_resource_bindings（编辑弹窗 hooks 挂载即拉）与其余命令一律空兜底
    return [];
  });
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  const utils = render(
    <QueryClientProvider client={queryClient}>
      <PresetList extensions={opts.extensions ?? []} />
    </QueryClientProvider>
  );
  // 等数据落地：预设名渲染 = list_presets 就绪；activePresets 查询已发出
  await utils.findByText(
    opts.presets?.[0]
      ? (opts.presets[0] as { name: string }).name
      : "暂无预设组。创建一个将多个 skill/MCP 打包为组合。"
  );
  await waitFor(() => expect(invokeMock).toHaveBeenCalledWith("list_active_presets"));
  return utils;
}

/** 定位「预设卡片 × 工具行」的 Switch：预设名 → 卡片根 div → 工具名 span → 开关包装 span → switch */
function switchFor(presetName: string, toolLabel: string): HTMLElement {
  // 名字 span 所在 flex 行的父节点即卡片根
  const card = screen.getByText(presetName).closest("div")!.parentElement!;
  // 工具名 span 的父节点是「图标 + 名字 + 开关」包装 span
  const toolSpan = within(card).getByText(toolLabel).parentElement!;
  return within(toolSpan).getByRole("switch");
}

beforeEach(() => {
  invokeMock.mockReset();
});

describe("PresetList（v2 双分区 + 预设×工具开关）", () => {
  it("双分区渲染：通用区 + 私有区按绑定工具分组（tool-scope 预设落在绑定工具组头下）", async () => {
    await renderPresetList({
      enabledTools: [
        TOOL_CLAUDE,
        {
          ...TOOL_DSH,
          id: "codex",
          label: "Codex CLI",
          skillToggleSupported: true,
          mcpSupported: true,
          pluginSupported: true,
        },
      ],
      presets: [UNIVERSAL_PRESET, CODEX_PRESET],
    });
    // 标题计数 + 两分区标题（zh 词条）
    expect(screen.getByText("预设组 (2)")).toBeInTheDocument();
    expect(screen.getByText("通用预设")).toBeInTheDocument();
    expect(screen.getByText("工具私有预设")).toBeInTheDocument();
    // 通用预设出现在通用区
    const universalSection = screen.getByText("通用预设").closest("section")!;
    expect(within(universalSection).getByText("通用组合")).toBeInTheDocument();
    // 私有区：组头 = 绑定工具名，预设卡片落在该组容器内
    const privateSection = screen.getByText("工具私有预设").closest("section")!;
    // "Codex" 出现 2 处（组头 div + 开关行标签 span；显示名经 TOOL_LABEL_OVERRIDES 覆盖）（组头 div + 开关行包装 span + 开关行标签 span），取组头 div
    const groupHeader = within(privateSection)
      .getAllByText("Codex")
      .find((el) => el.tagName === "DIV")!;
    const group = groupHeader.parentElement!;
    expect(within(group).getByText("Codex 专属")).toBeInTheDocument();
  });

  it("activePresets 驱动 Switch checked：on 工具 checked、off 工具 unchecked（radix data-state）", async () => {
    await renderPresetList({
      enabledTools: [
        TOOL_CLAUDE,
        {
          ...TOOL_DSH,
          id: "codex",
          label: "Codex CLI",
          skillToggleSupported: true,
          mcpSupported: true,
          pluginSupported: true,
        },
      ],
      presets: [UNIVERSAL_PRESET, CODEX_PRESET],
      activePresets: [
        { toolId: "claude", presetId: "p-univ" },
        { toolId: "codex", presetId: "p-codex" },
      ],
    });
    // 通用预设 × claude = 激活 → checked；× codex = 未激活 → unchecked
    expect(switchFor("通用组合", "Claude Code")).toHaveAttribute("data-state", "checked");
    expect(switchFor("通用组合", "Codex")).toHaveAttribute("data-state", "unchecked");
    // 私有预设 × 绑定工具 = 激活 → checked；非绑定工具不渲染开关（仅 claude/codex 两枚）
    expect(switchFor("Codex 专属", "Codex")).toHaveAttribute("data-state", "checked");
  });

  it("能力门控：dsh 全能力 false → 未激活 mcp 开关 disabled 且 unchecked；已激活开关保持可操作（restore 路径）", async () => {
    const MCP_PRESET = {
      ...UNIVERSAL_PRESET,
      id: "p-mcp",
      name: "MCP 组合",
      items: [item("m1", "mcp", "mcp-x")],
    };
    await renderPresetList({
      enabledTools: [TOOL_CLAUDE, TOOL_DSH],
      presets: [MCP_PRESET],
      // dsh 已激活 p-mcp：门控不挡「关」（a5f4e2d 裁决）
      activePresets: [{ toolId: "dsh", presetId: "p-mcp" }],
    });
    // 未激活组合不存在（p-mcp 仅在 dsh 上激活），故门控开关 = checked + 可操作
    const activeGated = switchFor("MCP 组合", "DSH");
    expect(activeGated).toHaveAttribute("data-state", "checked");
    expect(activeGated).not.toBeDisabled();
    // 支持该类型的 claude 开关不受门控影响（未激活 → unchecked + 可操作）
    expect(switchFor("MCP 组合", "Claude Code")).toHaveAttribute("data-state", "unchecked");
    expect(switchFor("MCP 组合", "Claude Code")).not.toBeDisabled();
  });

  it("能力门控（未激活）：预设含工具不支持的资源类型 → 该工具 Switch disabled 且未激活，title 带不支持原因", async () => {
    const MCP_PRESET = {
      ...UNIVERSAL_PRESET,
      id: "p-mcp",
      name: "MCP 组合",
      items: [item("m1", "mcp", "mcp-x")],
    };
    await renderPresetList({
      enabledTools: [TOOL_CLAUDE, TOOL_DSH],
      presets: [MCP_PRESET],
      activePresets: [],
    });
    // 未激活 + 门控 → disabled 且 unchecked，包装 span title = 「工具名: 暂不支持」（zh）
    const gated = switchFor("MCP 组合", "DSH");
    expect(gated).toBeDisabled();
    expect(gated).toHaveAttribute("data-state", "unchecked");
    expect(screen.getByText("DSH").parentElement).toHaveAttribute("title", "DSH: 暂不支持");
    // 支持该类型的 claude 开关不受影响
    expect(switchFor("MCP 组合", "Claude Code")).not.toBeDisabled();
  });

  it("restore 失败文案（终审 Minor #1）：开关关 → restore_preset 拒绝 → toast 报「恢复默认失败」而非「应用失败」", async () => {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "list_enabled_tools") return [TOOL_CLAUDE];
      if (cmd === "list_presets") return [UNIVERSAL_PRESET];
      if (cmd === "list_active_presets") return [{ toolId: "claude", presetId: "p-univ" }];
      if (cmd === "restore_preset") throw new Error("backend boom");
      return [];
    });
    const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={queryClient}>
        <PresetList extensions={[]} />
      </QueryClientProvider>
    );
    await screen.findByText("通用组合");
    // 已激活开关恒可操作（关 = 恢复默认路径）
    fireEvent.click(switchFor("通用组合", "Claude Code"));
    await waitFor(() => expect(toastMock.error).toHaveBeenCalledTimes(1));
    const msg = toastMock.error.mock.calls[0][0] as string;
    // 新文案 presets.restoreFailed（zh「恢复默认失败」），且不得回退到 applyFailed 的「应用失败」
    expect(msg).toContain("恢复默认失败");
    expect(msg).toContain("backend boom");
    expect(msg).not.toContain("应用失败");
  });
});

describe("PresetCard 👁/✏️（UI 改版 B5）", () => {
  const SOURCES_PRESET = {
    ...UNIVERSAL_PRESET,
    id: "p-src",
    name: "来源组合",
    items: [item("s1", "skill", "skill-a"), item("n1", "skill", "native-skill")],
  };
  const SOURCES_EXTENSIONS: ExtensionWithAssignments[] = [
    {
      id: "s1",
      kind: "skill",
      name: "skill-a",
      description: null,
      sourcePath: "/x",
      sourceTool: null,
      suite: null,
      tags: null,
      isNative: false,
      assignments: [],
    },
    {
      id: "n1",
      kind: "skill",
      name: "native-skill",
      description: null,
      sourcePath: "/y",
      sourceTool: "codex",
      suite: null,
      tags: null,
      isNative: true,
      assignments: [],
    },
  ];

  it("👁 预览：只读弹窗按 kind 分组列名 + 来源徽章（全局仓库/原生）", async () => {
    await renderPresetList({ presets: [SOURCES_PRESET], extensions: SOURCES_EXTENSIONS });
    fireEvent.click(screen.getByRole("button", { name: "预览预设" }));
    const dlg = await screen.findByRole("dialog");
    expect(within(dlg).getByText("Skill (2)")).toBeInTheDocument();
    expect(within(dlg).getByText("skill-a")).toBeInTheDocument();
    expect(within(dlg).getAllByText("全局仓库").length).toBe(1);
    expect(within(dlg).getByText("native-skill")).toBeInTheDocument();
    expect(within(dlg).getAllByText("原生").length).toBe(1);
    // 只读预览不触发编辑弹窗
    expect(screen.queryByText("编辑预设组")).not.toBeInTheDocument();
  });

  it("✏️ 编辑：图标入口打开编辑弹窗（预填），整卡点击仍为同一入口", async () => {
    await renderPresetList({ presets: [SOURCES_PRESET] });
    fireEvent.click(screen.getByRole("button", { name: "编辑预设" }));
    expect(await screen.findByText("编辑预设组")).toBeInTheDocument();
  });

  it("整卡点击行为保持不变：点卡片名打开编辑弹窗（与 ✏️ 同入口）", async () => {
    await renderPresetList({ presets: [SOURCES_PRESET] });
    fireEvent.click(screen.getByText("来源组合"));
    expect(await screen.findByText("编辑预设组")).toBeInTheDocument();
  });
});

// ==== #66 SubAgentPresetActions 复活：折叠入口恒渲染 + supported/agents 分诊 ====
describe("SubAgentPresetActions 入口复活（#66）", () => {
  /** 挂载单工具卡，mock detect_subagents 返回指定载荷（新契约 SubagentDetection） */
  async function renderWithDetection(detection: unknown) {
    invokeMock.mockImplementation(async (cmd: string) => {
      if (cmd === "list_enabled_tools") return [TOOL_CLAUDE];
      if (cmd === "list_presets") return [UNIVERSAL_PRESET];
      if (cmd === "list_active_presets") return [];
      if (cmd === "detect_subagents") return detection;
      return [];
    });
    const queryClient = new QueryClient({
      defaultOptions: { queries: { retry: false } },
    });
    render(
      <QueryClientProvider client={queryClient}>
        <PresetList extensions={[]} />
      </QueryClientProvider>
    );
    await screen.findByText("通用组合");
    await waitFor(() => expect(invokeMock).toHaveBeenCalledWith("list_active_presets"));
  }
  /** 折叠入口按钮（zh 词条 presets.expandSubAgents） */
  function expandButton(): HTMLElement {
    return screen.getByRole("button", { name: "子 Agent ▼" });
  }

  it("无子代理工具（supported+空列表）：折叠入口恒渲染（死 UI 回归锁——旧版 length===0 return null 在按钮之前，此断言必红）+ 展开显示空态文案", async () => {
    await renderWithDetection({ supported: true, agents: [] });
    // 死 UI 回归锁：旧版初始空列表直接 return null，按钮永不渲染
    expect(expandButton()).toBeInTheDocument();
    fireEvent.click(expandButton());
    expect(await screen.findByText("未检测到子 Agent")).toBeInTheDocument();
  });

  it("工具不支持子 Agent（supported:false）：展开走「不支持」分支而非空态/列表", async () => {
    await renderWithDetection({ supported: false, agents: [] });
    expect(expandButton()).toBeInTheDocument();
    fireEvent.click(expandButton());
    // 键路径已修（errors.presetSubagentUnsupported，zh 值在 errors 命名空间）——
    // 纯文案断言，缺键/键路径渲染会直接红
    const hint = await screen.findByText("该类型不支持子 Agent 级分配");
    expect(hint).toBeInTheDocument();
    // 不得误走空态分支（supported=false 优先于 agents 空列表分诊）
    expect(screen.queryByText("未检测到子 Agent")).not.toBeInTheDocument();
  });

  it("有子代理：展开列出 agents + 应用/停用按钮在（既有列表行为回归锁）", async () => {
    await renderWithDetection({ supported: true, agents: ["frontend-dev"] });
    fireEvent.click(expandButton());
    const row = (await screen.findByText("frontend-dev")).closest("div")!;
    // 应用按钮（zh「应用」+ Play 图标）在行内；停用按钮（X 图标）也在
    expect(within(row).getByRole("button", { name: "应用" })).toBeInTheDocument();
    expect(within(row).getAllByRole("button")).toHaveLength(2);
  });

  it("新契约形态同步锁：展开时以 {toolId} 调 detect_subagents，返回结构体（非旧数组契约）驱动分诊", async () => {
    await renderWithDetection({ supported: true, agents: [] });
    fireEvent.click(expandButton());
    await screen.findByText("未检测到子 Agent");
    expect(invokeMock).toHaveBeenCalledWith("detect_subagents", { toolId: "claude" });
  });
});
