// tests/settings/toolManagement.test.tsx — review F7①②：工具管理批量保存 happy path
// 与未保存离开「放弃更改」路径（mock invoke，断言 IPC 入参与确认弹窗分类文案）
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

const { invokeMock } = vi.hoisted(() => ({ invokeMock: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));
// P2-2：settings 页改用 getCurrentWindow().onCloseRequested 拦截窗口关闭（代替 beforeunload）。
// jsdom 无 __TAURI_INTERNALS__，此处 mock window API 供测试捕获 close 回调
const { closeRequestedHandler } = vi.hoisted(() => ({
  closeRequestedHandler: vi.fn(),
}));
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({
    onCloseRequested: vi.fn(async (cb: (e: { preventDefault: () => void }) => void) => {
      // 记录回调供测试触发（mockImplementation 不产生调用记录，这里显式调用包装 fn）
      closeRequestedHandler(cb);
      return () => {};
    }),
  }),
}));
// WindowFrame/TitleBar 依赖 Tauri 窗口 API，jsdom 下全 mock（与 petSettings 同模式）
vi.mock("@tauri-apps/api/webviewWindow", () => ({
  getCurrentWebviewWindow: () => ({
    isMaximized: vi.fn(async () => false),
    onResized: vi.fn(async () => () => {}),
    minimize: vi.fn(),
    toggleMaximize: vi.fn(),
    close: vi.fn(),
  }),
}));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async () => () => {}),
  emit: vi.fn(async () => {}),
}));

// tests/setup.ts 未初始化 i18n，显式引入并按默认英文断言
import i18n from "@/i18n";
import SettingsPage from "@/pages/settings";

void i18n;

const rows = [
  { toolId: "opencode", name: "OpenCode", enabled: true, installed: true, managed: true },
  { toolId: "codex", name: "Codex CLI", enabled: true, installed: true, managed: false },
];

beforeEach(() => {
  window.localStorage.clear();
  invokeMock.mockReset();
  invokeMock.mockImplementation(async (cmd: string) => {
    if (cmd === "get_tool_settings") return structuredClone(rows);
    if (cmd === "update_tool_settings") {
      // 按 issue #36-4 的 ToolSettingsResult 真实形状返回（旧 mock 的 skipped 字段
      // 缺 skippedKept/skippedLost，会让 applyChanges 读 .length 抛错中断后续流程）
      return {
        restored: [],
        restoredMcps: [],
        rebuildFailed: [],
        skippedKept: [],
        skippedLost: [],
      };
    }
    if (cmd === "list_enabled_tools") return rows.map((r) => ({ id: r.toolId, label: r.name }));
    return [];
  });
});

// 语言是 i18n 单例状态：zh 用例结束必须还原 en，避免污染同文件既有英文断言
afterEach(async () => {
  await i18n.changeLanguage("en");
});

const renderPage = () =>
  render(
    <QueryClientProvider client={new QueryClient()}>
      <SettingsPage />
    </QueryClientProvider>
  );

// jsdom 未实现 matchMedia（ThemeProviders 需要），与 petSettings 同款 shim
window.matchMedia = ((query: string) => ({
  matches: false,
  media: query,
  onchange: null,
  addListener: vi.fn(),
  removeListener: vi.fn(),
  addEventListener: vi.fn(),
  removeEventListener: vi.fn(),
  dispatchEvent: vi.fn(),
})) as unknown as typeof window.matchMedia;

async function gotoToolsAndToggleOpenCode() {
  renderPage();
  // 2026-10-06 两级导航：侧栏点击目标由「分区名」变为「块」（块内多卡并置），用 testid 更稳
  fireEvent.click(await screen.findByTestId("settings-nav-tools"));
  const opencode = await screen.findByText("OpenCode");
  const row = opencode.closest("div[class*='justify-between']") as HTMLElement;
  fireEvent.click(row.querySelector('[role="switch"]') as HTMLElement);
  await screen.findByText("Save Settings");
}

describe("工具管理（review F7①②）", () => {
  it("F7① toggle→确认→apply：弹窗分类提示（还原/回溯）与 update_tool_settings 入参", async () => {
    await gotoToolsAndToggleOpenCode();
    // 打开保存确认弹窗
    fireEvent.click(await screen.findByText("Save Settings"));
    expect(await screen.findByText("Apply Changes")).toBeInTheDocument();
    // OpenCode（managed=true、关闭）→ 还原/回溯分类文案
    expect(
      screen.getByText(/Restore\/rollback: skill\/plugin links restored to real files/)
    ).toBeInTheDocument();
    // 确认应用 → IPC 入参断言
    fireEvent.click(screen.getByText("Apply"));
    await waitFor(() => {
      expect(invokeMock).toHaveBeenCalledWith("update_tool_settings", {
        changes: [{ toolId: "opencode", enabled: false }],
      });
    });
    // 终审 Minor #5：保存成功后同步托盘（工具启停可能改动预设激活态，需重建托盘菜单）
    await waitFor(() => {
      expect(invokeMock).toHaveBeenCalledWith("refresh_tray", { presetsLabel: "Presets" });
    });
  });

  it("F7② 未保存切分区 → 放弃更改：不持久化且开关回到已保存状态", async () => {
    await gotoToolsAndToggleOpenCode();
    // 切**块**触发三选拦截（2026-10-06 两级导航：导航单位由分区变为块，工具卡住在「工具与体检」块）
    fireEvent.click(screen.getByTestId("settings-nav-appearance"));
    expect(await screen.findByText("Unsaved Changes")).toBeInTheDocument();
    // 放弃更改 → 不落盘
    fireEvent.click(screen.getByText("Discard Changes"));
    await waitFor(() => expect(screen.queryByText("Unsaved Changes")).not.toBeInTheDocument());
    expect(invokeMock).not.toHaveBeenCalledWith("update_tool_settings", expect.anything());
    // 回到工具分区：开关为已保存状态（checked）、dirty 已重置（无保存按钮）
    // 2026-10-06 两级导航：侧栏点击目标由「分区名」变为「块」（块内多卡并置），用 testid 更稳
  fireEvent.click(await screen.findByTestId("settings-nav-tools"));
    const opencode = await screen.findByText("OpenCode");
    const row = opencode.closest("div[class*='justify-between']") as HTMLElement;
    expect(row.querySelector('[role="switch"]')).toHaveAttribute("data-state", "checked");
    expect(screen.queryByText("Save Settings")).not.toBeInTheDocument();
  });

  it("P2-2 脏标记时关窗口 → onCloseRequested 拦截并弹三选", async () => {
    await gotoToolsAndToggleOpenCode();
    // 模拟窗口关闭请求（onCloseRequested 回调注册是异步的，等它挂上）
    await waitFor(() => expect(closeRequestedHandler).toHaveBeenCalled());
    const preventDefault = vi.fn();
    closeRequestedHandler.mock.calls[0][0]({ preventDefault });
    expect(preventDefault).toHaveBeenCalled();
    // 三选弹窗出现（保存 / 放弃更改 / 继续编辑）
    expect(await screen.findByText("Unsaved Changes")).toBeInTheDocument();
    expect(screen.getByText("Keep Editing")).toBeInTheDocument();
    // 选「放弃更改」→ 关闭拦截解除、弹窗消失、不落盘
    fireEvent.click(screen.getByText("Discard Changes"));
    await waitFor(() => expect(screen.queryByText("Unsaved Changes")).not.toBeInTheDocument());
    expect(invokeMock).not.toHaveBeenCalledWith("update_tool_settings", expect.anything());
  });

  // Task 3（裁决 34a）：设置页工具勾选区旁补「取消勾选的工具……」提示——
  // 后端对 APP 形态写动作以 409 app_form_pending 止血拒绝，本行是入口侧引导文案
  it("裁决 34a：工具勾选区旁渲染看板与远程能力提示（zh）", async () => {
    await i18n.changeLanguage("zh");
    renderPage();
    fireEvent.click(await screen.findByTestId("settings-nav-tools"));
    // 勾选行列表在场 + 提示文案渲染（zh 键 = settings.tools.boardRemoteNote）
    expect(await screen.findByText("Codex CLI")).toBeInTheDocument();
    expect(
      await screen.findByText("取消勾选的工具不会出现在看板，也无法远程发送")
    ).toBeInTheDocument();
  });
});
