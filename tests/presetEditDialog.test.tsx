// tests/presetEditDialog.test.tsx — 预设编辑弹窗（Task 7）
// 覆盖：空名保存不触发 create_preset；填名+勾项+工具私有类型 → create_preset 以含 meta
// 的 camelCase args 调用；工具私有模式不适配（exclusiveTools 不含绑定工具）资源置灰 +
// 原生技能分组（isNative && sourceTool === boundTool）；final review Finding 2 回归锁：
// 工具私有勾选原生项后切「通用」，原生项不随保存泄漏进通用预设（spec §3.3 通用 = 仅 兔维斯 资源）
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { PresetEditDialog } from "@/components/presets/PresetEditDialog";
import type { ExtensionWithAssignments } from "@/types/extension";
import type { ResourceBinding } from "@/types/extension";

const { invokeMock } = vi.hoisted(() => ({ invokeMock: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));

// tests/setup.ts 未初始化 i18n，显式引入并固定中文（文案按 zh 断言）
import i18n from "@/i18n";

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

const ext = (overrides: Partial<ExtensionWithAssignments>): ExtensionWithAssignments => ({
  id: "x",
  kind: "skill",
  name: "ext",
  description: null,
  sourcePath: "/tmp/x",
  sourceTool: null,
  suite: null,
  tags: null,
  isNative: false,
  assignments: [],
  ...overrides,
});

const EXTENSIONS: ExtensionWithAssignments[] = [
  ext({ id: "s1", kind: "skill", name: "skill-alpha" }),
  ext({ id: "m1", kind: "mcp", name: "mcp-exclusive" }),
  // 原生（未纳管）技能：sourceTool = codex → 工具私有（绑定 codex）时出现在「原生技能」组
  ext({ id: "n1", kind: "skill", name: "native-codex-skill", isNative: true, sourceTool: "codex" }),
];

/** mcp-exclusive 专属绑定：允许工具仅 claude（不含 codex → 绑定 codex 时不适配） */
const BINDINGS: ResourceBinding[] = [
  {
    extensionId: "m1",
    exclusiveTools: "claude",
    reason: "依赖 claude 专属环境",
    updatedAt: "2026-09-15T00:00:00Z",
  },
];

function renderDialog(onClose = vi.fn(), extensions: ExtensionWithAssignments[] = EXTENSIONS) {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  render(
    <QueryClientProvider client={queryClient}>
      <PresetEditDialog open preset={null} presetExtensions={extensions} onClose={onClose} />
    </QueryClientProvider>
  );
  return onClose;
}

/** 勾选/取消资源复选项（资源名 → label 内的 radix checkbox） */
function checkItem(name: string, next = true) {
  const label = screen.getByText(name).closest("label")!;
  const checkbox = within(label).getByRole("checkbox");
  if (
    (checkbox as HTMLInputElement).getAttribute("data-state") === (next ? "checked" : "unchecked")
  )
    return; // 已是目标态
  fireEvent.click(checkbox);
}

/** 切到工具私有类型并选好绑定工具 */
function selectToolScope(toolId = "codex") {
  fireEvent.click(screen.getByLabelText("工具私有"));
  fireEvent.change(screen.getByRole("combobox"), { target: { value: toolId } });
}

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockImplementation(async (cmd: string) => {
    if (cmd === "list_enabled_tools")
      return [
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
    if (cmd === "list_resource_bindings") return BINDINGS;
    if (cmd === "create_preset") return "new-preset-id";
    return [];
  });
});

describe("PresetEditDialog（新建/编辑共用）", () => {
  it("空名点保存 → create_preset 不被 invoke（保存按钮禁用兜底 + 校验守卫零调用）", async () => {
    renderDialog();
    // 等弹窗内容就绪（套件列表数据落地）
    expect(await screen.findByText("skill-alpha")).toBeInTheDocument();
    const save = screen.getByRole("button", { name: "保存" });
    // 名称空 → 保存按钮禁用（canSave 守卫）
    expect(save).toBeDisabled();
    fireEvent.click(save); // disabled 点击不触发 onClick
    // 程序化兜底路径也不该 invoke（handleSave 空名 toast 守卫同样不触发 create）
    expect(invokeMock).not.toHaveBeenCalledWith("create_preset", expect.anything());
  });

  it("填名+勾项+工具私有+绑定 codex → create_preset 以含 meta 的 camelCase args 调用", async () => {
    const onClose = renderDialog();
    expect(await screen.findByText("skill-alpha")).toBeInTheDocument();
    fireEvent.change(screen.getByPlaceholderText("预设组名称（如：前端开发）"), {
      target: { value: "前端组合" },
    });
    fireEvent.change(screen.getByPlaceholderText("描述这个预设组的用途（可选）"), {
      target: { value: "备忘" },
    });
    checkItem("skill-alpha");
    selectToolScope("codex");
    fireEvent.click(screen.getByRole("button", { name: "保存" }));
    // 后端契约：参数 camelCase（Tauri 自动映射 snake_case），items 为 [extensionId, kind] 二元组
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("create_preset", {
        name: "前端组合",
        items: [["s1", "skill"]],
        description: "备忘",
        scope: "tool",
        boundTool: "codex",
      })
    );
    // 保存成功 → invalidate 后回调关闭
    await waitFor(() => expect(onClose).toHaveBeenCalled());
  });

  it("工具私有模式不适配置灰：exclusiveTools 不含绑定工具的 兔维斯 资源 disabled；原生技能组按 sourceTool 归属出现", async () => {
    renderDialog();
    expect(await screen.findByText("skill-alpha")).toBeInTheDocument();
    selectToolScope("codex");
    // mcp-exclusive：允许工具仅 claude ≠ 绑定 codex → checkbox disabled
    const mismatchedLabel = screen.getByText("mcp-exclusive").closest("label")!;
    expect(within(mismatchedLabel).getByRole("checkbox")).toBeDisabled();
    // 不适配原因 title（resources.binding.* 组合文案）
    expect(mismatchedLabel).toHaveAttribute(
      "title",
      "专属原因: 依赖 claude 专属环境 · 允许工具: claude"
    );
    // 适配项不受影响
    const okLabel = screen.getByText("skill-alpha").closest("label")!;
    expect(within(okLabel).getByRole("checkbox")).not.toBeDisabled();
    // 原生技能组：isNative && sourceTool === boundTool 的项出现且可勾选
    expect(screen.getByText("原生技能")).toBeInTheDocument();
    const nativeLabel = screen.getByText("native-codex-skill").closest("label")!;
    expect(within(nativeLabel).getByRole("checkbox")).not.toBeDisabled();
  });

  it("切通用剪除原生项（Finding 2 回归锁）：tool-scope 勾选原生技能 → 切通用 → create items 不含原生项", async () => {
    const onClose = renderDialog();
    expect(await screen.findByText("skill-alpha")).toBeInTheDocument();
    selectToolScope("codex");
    checkItem("skill-alpha"); // 兔维斯 资源：切通用后应保留
    checkItem("native-codex-skill"); // 原生技能：仅工具私有合法
    expect(screen.getByText("原生技能")).toBeInTheDocument();
    // 切到通用：原生组整体消失（不可见即不可再勾选，spec §3.3 通用 = 仅 兔维斯 资源）
    fireEvent.click(screen.getByLabelText("通用"));
    expect(screen.queryByText("原生技能")).not.toBeInTheDocument();
    expect(screen.queryByText("native-codex-skill")).not.toBeInTheDocument();
    fireEvent.change(screen.getByPlaceholderText("预设组名称（如：前端开发）"), {
      target: { value: "通用组合" },
    });
    fireEvent.click(screen.getByRole("button", { name: "保存" }));
    // 锁：create args 的 items 既不含原生 n1，也保留 兔维斯 的 s1
    await waitFor(() =>
      expect(invokeMock).toHaveBeenCalledWith("create_preset", {
        name: "通用组合",
        items: [["s1", "skill"]],
        description: undefined,
        scope: "universal",
        boundTool: undefined,
      })
    );
    await waitFor(() => expect(onClose).toHaveBeenCalled());
  });
});

describe("PresetEditDialog 两列选择区（UI 改版 B6/B7）", () => {
  it("右列空态 → 勾选左列进右列 → 点右列对钩移出且左列复选回弹", async () => {
    renderDialog();
    expect(await screen.findByText("skill-alpha")).toBeInTheDocument();
    // 初始空态文案
    expect(screen.getByText("尚未选择资源")).toBeInTheDocument();
    checkItem("skill-alpha");
    // 右列（已选资源）出现勾选项
    const rightCol = screen.getByText("已选资源").closest("div[class*='border']")!;
    expect(within(rightCol).getByText("skill-alpha")).toBeInTheDocument();
    // 点对钩 = 取消选中：右列移出、回到空态、左列复选框回弹
    fireEvent.click(within(rightCol).getByRole("button", { name: "移除该项: skill-alpha" }));
    expect(within(rightCol).queryByText("skill-alpha")).not.toBeInTheDocument();
    expect(screen.getByText("尚未选择资源")).toBeInTheDocument();
    const label = screen.getByText("skill-alpha").closest("label")!;
    expect(within(label).getByRole("checkbox")).toHaveAttribute("data-state", "unchecked");
  });

  it("左列搜索过滤：输入 alpha → mcp 组隐藏、skill 项保留", async () => {
    renderDialog();
    expect(await screen.findByText("skill-alpha")).toBeInTheDocument();
    fireEvent.change(screen.getByPlaceholderText("按名称筛选…"), {
      target: { value: "alpha" },
    });
    expect(screen.queryByText("mcp-exclusive")).not.toBeInTheDocument();
    expect(screen.getByText("skill-alpha")).toBeInTheDocument();
  });

  it("左列排序切换：none→asc→desc 三态循环生效", async () => {
    const EXT2: ExtensionWithAssignments[] = [
      ext({ id: "s1", kind: "skill", name: "skill-zeta" }),
      ext({ id: "s2", kind: "skill", name: "skill-alpha" }),
    ];
    renderDialog(vi.fn(), EXT2);
    expect(await screen.findByText("skill-alpha")).toBeInTheDocument();
    const leftCol = screen.getByText("可选资源").closest("div[class*='border']")!;
    const names = () =>
      within(leftCol)
        .getAllByText(/skill-(alpha|zeta)/)
        .map((el) => el.textContent);
    expect(names()).toEqual(["skill-zeta", "skill-alpha"]); // 默认扫描序
    fireEvent.click(screen.getByRole("button", { name: "按名字排序" })); // → asc
    expect(names()).toEqual(["skill-alpha", "skill-zeta"]);
    fireEvent.click(screen.getByRole("button", { name: "按名字排序" })); // → desc
    expect(names()).toEqual(["skill-zeta", "skill-alpha"]);
  });

  it("保存/取消各仅一枚（B7：移入描述框下方，不再有底部 footer 操作钮）", async () => {
    renderDialog();
    expect(await screen.findByText("skill-alpha")).toBeInTheDocument();
    expect(screen.getAllByRole("button", { name: "保存" }).length).toBe(1);
    expect(screen.getAllByRole("button", { name: "取消" }).length).toBe(1);
  });
});
