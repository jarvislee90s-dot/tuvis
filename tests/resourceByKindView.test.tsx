// tests/resourceByKindView.test.tsx — 「按资源」视图（资源管理页 UI 改版 A1/A2）真视图闭环
// 覆盖 F3（宽弹窗修复后评审 Important）：搜索框进弹窗后，卡片与弹窗必须同源——
// ① 卡片输入关键词 → 打开资源大表弹窗：弹窗内搜索框 value 与卡片一致，行与 tab 计数
//    都是过滤后结果（不是只测 ResourceKindDialog 的合成 Host——那绕过了 ResourceByKindView
//    的提升状态与 props 接线，是本轮补的短板）
// ② 弹窗内改词 → 清空 → 关闭 → 重开：关键词不丢、过滤结果一致、不复活旧词
// ③ 弹窗内改词后不清空直接关窗 → 重开：关键词仍是该词（关闭 ≠ 清空，M3「关闭即清空」
//    式回归会红）
// mock 面（弹窗挂载即查）：资源 list_ssot_resources / 工具 list_enabled_tools /
// 专属绑定 list_resource_bindings / 常驻锁 list_tool_residents
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { ResourceByKindView } from "@/components/resources/ResourceByKindView";
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

/** alpha 全启用（claude+codex）/ beta 未启用：搜 alpha 或 beta 各命中 1 行 → 计数 1 */
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

const SEARCH_PLACEHOLDER = "搜索 skill...";

async function renderView() {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  render(
    <QueryClientProvider client={queryClient}>
      <ResourceByKindView />
    </QueryClientProvider>
  );
  // 卡片就绪（资源落地 + 段头计数按全量渲染）
  await screen.findByRole("button", { name: "Skill (2)" });
}

/** 卡片根（h3「兔维斯 仓库」的父级；弹窗标题也是同一句文案，故按 h3 限定）；
 *  弹窗打开后卡片内容被模态 aria-hidden，角色查询抓不到 → 卡片面断言一律
 *  within(cardRoot()) 走文本查询 */
const cardRoot = () => screen.getByText("兔维斯 仓库", { selector: "h3" }).closest("div")!;
const cardBox = () => within(cardRoot()).getByPlaceholderText(SEARCH_PLACEHOLDER);
const dialogRoot = () => screen.getByRole("dialog");
const dialogBox = () => within(dialogRoot()).getByPlaceholderText(SEARCH_PLACEHOLDER);

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockImplementation(async (cmd: string) => {
    if (cmd === "list_enabled_tools") return TOOLS;
    if (cmd === "list_ssot_resources") return RESOURCES;
    if (cmd === "list_resource_bindings") return [];
    if (cmd === "list_tool_residents") return [];
    return [];
  });
});

describe("ResourceByKindView 真视图（F3 搜索闭环：卡片 ↔ 弹窗同源）", () => {
  it("① 卡片输入关键词 → 打开弹窗：弹窗内搜索框 value 同源，行与 tab 计数均为过滤后结果", async () => {
    await renderView();

    // 卡片输入：段头计数实时收敛（Skill 2 → 1）
    fireEvent.change(cardBox(), { target: { value: "alpha" } });
    expect(within(cardRoot()).getByText("Skill (1)")).toBeInTheDocument();

    // 点段头入口开弹窗（预选 skill）
    fireEvent.click(screen.getByRole("button", { name: "Skill (1)" }));

    await screen.findByRole("dialog");
    // 弹窗内搜索框 value = 卡片同源（同一份提升状态，不是各持一份）
    expect(dialogBox()).toHaveValue("alpha");
    expect(cardBox()).toHaveValue("alpha");
    // 行 = 过滤后：alpha 在、beta 被过滤掉
    expect(within(dialogRoot()).getByText("alpha")).toBeInTheDocument();
    expect(within(dialogRoot()).queryByText("beta")).not.toBeInTheDocument();
    // tab 计数 = 过滤后（弹窗内 tab 与卡片段头同口径）
    expect(within(dialogRoot()).getByRole("button", { name: "Skill (1)" })).toBeInTheDocument();
  });

  it("② 弹窗内改词 → 清空 → 关闭重开：关键词不丢、结果一致、不复活旧词", async () => {
    await renderView();

    // 卡片先给一个初始词，再开弹窗
    fireEvent.change(cardBox(), { target: { value: "alpha" } });
    fireEvent.click(screen.getByRole("button", { name: "Skill (1)" }));
    await screen.findByRole("dialog");

    // 弹窗内直接改词（F3 的核心诉求）：alpha → beta，弹窗行/tab 与卡片段头同步收敛
    fireEvent.change(dialogBox(), { target: { value: "beta" } });
    expect(dialogBox()).toHaveValue("beta");
    expect(cardBox()).toHaveValue("beta");
    expect(within(dialogRoot()).getByText("beta")).toBeInTheDocument();
    expect(within(dialogRoot()).queryByText("alpha")).not.toBeInTheDocument();
    expect(within(dialogRoot()).getByRole("button", { name: "Skill (1)" })).toBeInTheDocument();
    expect(within(cardRoot()).getByText("Skill (1)")).toBeInTheDocument();

    // 清空即恢复全量：两行都在、计数回 2
    fireEvent.change(dialogBox(), { target: { value: "" } });
    expect(within(dialogRoot()).getByText("alpha")).toBeInTheDocument();
    expect(within(dialogRoot()).getByText("beta")).toBeInTheDocument();
    expect(within(dialogRoot()).getByRole("button", { name: "Skill (2)" })).toBeInTheDocument();

    // 关闭（X）→ 重开：关键词保持已清空，不复活 alpha
    fireEvent.click(within(dialogRoot()).getByRole("button", { name: "Close" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(cardBox()).toHaveValue("");

    fireEvent.click(screen.getByRole("button", { name: "Skill (2)" }));
    await screen.findByRole("dialog");
    expect(dialogBox()).toHaveValue("");
    expect(within(dialogRoot()).getByText("alpha")).toBeInTheDocument();
    expect(within(dialogRoot()).getByText("beta")).toBeInTheDocument();
    expect(within(dialogRoot()).getByRole("button", { name: "Skill (2)" })).toBeInTheDocument();
  });

  it("③ 弹窗内输入后直接关窗：关键词不丢（关闭 ≠ 清空），重开仍是 beta、alpha 被过滤", async () => {
    await renderView();

    // 卡片无词 → 全量开弹窗
    fireEvent.click(screen.getByRole("button", { name: "Skill (2)" }));
    await screen.findByRole("dialog");

    // 弹窗内输入 beta，不清空直接点 X 关闭（关窗是关窗、不是清词）
    fireEvent.change(dialogBox(), { target: { value: "beta" } });
    expect(within(dialogRoot()).getByText("beta")).toBeInTheDocument();
    fireEvent.click(within(dialogRoot()).getByRole("button", { name: "Close" }));

    // 等弹窗消失：卡片框保留 beta（提升状态没被关闭动作重置）
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(cardBox()).toHaveValue("beta");

    // 重开：弹窗框仍为 beta，alpha 依然被过滤、tab 计数 Skill (1)
    fireEvent.click(screen.getByRole("button", { name: "Skill (1)" }));
    await screen.findByRole("dialog");
    expect(dialogBox()).toHaveValue("beta");
    expect(within(dialogRoot()).getByText("beta")).toBeInTheDocument();
    expect(within(dialogRoot()).queryByText("alpha")).not.toBeInTheDocument();
    expect(within(dialogRoot()).getByRole("button", { name: "Skill (1)" })).toBeInTheDocument();
  });
});
