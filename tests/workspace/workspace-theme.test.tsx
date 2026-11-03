// Task 8（2026-10-26 计划）· 已定深浅主题与轻玻璃（vitest + jsdom）。
// 判据来源：brief Step1 + spec §3（色彩方案定稿：浅色纯白底蓝紫强调 / 深色雾银石墨；
// 轻玻璃仅导航与浮层；选中与焦点不只靠颜色区别）。
//
// 覆盖：
//  1. 主导航 a11y 契约：role=navigation + 可访问名「主导航」；活动页签
//     aria-current="page" + 非颜色指示（data-active 属性 + 强调点 + 字重）；玻璃材质类在场；
//  2. 主题 SSOT 跨窗同步（既有机制回归门）：主窗 setTheme("dark") → 写 DB
//     （set_theme）+ 应用 html 类；模拟 Rust 广播 tuvis-theme-changed 到 main/settings
//     两个窗口（theme-provider 独立模块实例 = 独立 WebView 的真实形态）→ 两窗收敛 dark；
//  3. 减少透明度 / 不支持 backdrop-filter：玻璃面带 data-glass-fallback 显式实心回退，
//     内容照常渲染、页签可点、标题在场；
//  4. WorkspacePageHeader：title/description/actions 三槽；四页逐一切换，
//     活动页恰有一个 h1（非活动页被 aria-hidden 摘出 a11y 树，不会重复命中）。
//
// 纪律：本文件只测行为契约，不做 CSS 值镜像——色层由截图清单
// （docs/superpowers/evidence/2026-10-26-desktop-workspace-v2/theme/）与对比度表验证。
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";

// jsdom 无 matchMedia（theme-provider / MainTitleBar / 玻璃降级探测都需要）。
// 用可变开关模拟「系统要求减少透明度」，用例内按需翻转。
const mmState = vi.hoisted(() => ({ reducedTransparency: false }));
window.matchMedia = ((query: string) => ({
  matches: mmState.reducedTransparency && query.includes("prefers-reduced-transparency"),
  media: query,
  onchange: null,
  addListener: vi.fn(),
  removeListener: vi.fn(),
  addEventListener: vi.fn(),
  removeEventListener: vi.fn(),
  dispatchEvent: vi.fn(),
})) as unknown as typeof window.matchMedia;

// —— 双窗口模拟：每个 label 对应一个独立 WebView 的 theme-provider 模块实例 ——
// listen 替身按「注册时刻的活动窗口」给 handler 打标签；投递 tuvis-theme-changed 时
// 同步记录到该窗口的条目（= 该窗口已应用的主题）。invoke 走 setup.ts 接线的
// tauriInvokeMock（get_setting 缺省 null、set_theme 落 default undefined）。
const themeState = vi.hoisted(() => ({
  activeMockWindow: "main" as string,
  windows: [
    { label: "main", theme: "light" },
    { label: "settings", theme: "light" },
  ] as Array<{ label: string; theme: string }>,
  themeListeners: [] as Array<(e: { payload: { theme?: string } }) => void>,
}));

vi.mock("@tauri-apps/api/event", async () => {
  const { workspaceNavMock } = await import("../msw/tauriMocks");
  return {
    listen: vi.fn(async (event: string, handler: (e: { payload: unknown }) => void) => {
      if (event === "tuvis-workspace-navigation") {
        workspaceNavMock.listeners.add(handler);
        return () => {
          workspaceNavMock.listeners.delete(handler);
        };
      }
      // 主题等其余事件：按注册时刻的活动窗口打标签收集（Rust 广播的投递对象）
      const label = themeState.activeMockWindow;
      themeState.themeListeners.push((e: { payload: { theme?: string } }) => {
        const win = themeState.windows.find((w) => w.label === label);
        if (event === "tuvis-theme-changed" && win && typeof e?.payload?.theme === "string") {
          win.theme = e.payload.theme;
        }
        return handler(e as { payload: unknown });
      });
      return () => {};
    }),
    once: vi.fn(async () => () => {}),
    emit: vi.fn(async () => {}),
  };
});

// 窗口层替身（WindowFrame / TitleBar / 导航 hook 的焦点监听）
vi.mock("@tauri-apps/api/webviewWindow", () => ({
  getCurrentWebviewWindow: () => ({
    label: "main",
    isMaximized: vi.fn(async () => false),
    onResized: vi.fn(async () => () => {}),
    minimize: vi.fn(),
    toggleMaximize: vi.fn(),
    close: vi.fn(),
  }),
}));
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({
    label: "main",
    onFocusChanged: vi.fn(async () => () => {}),
    show: vi.fn(async () => {}),
  }),
}));

// 会话轮询/通知挂载计数无关本文件：替身掉，避免真实轮询
vi.mock("@/hooks/useSessions", async () => {
  const { useEffect } = await import("react");
  return {
    useSessions: vi.fn(() => {
      useEffect(() => {}, []);
      return undefined;
    }),
  };
});
vi.mock("@/hooks/useNotification", async () => {
  const { useEffect } = await import("react");
  return {
    useNotification: vi.fn(() => {
      useEffect(() => {}, []);
      return undefined;
    }),
  };
});
// 桌宠启动校验（EP2）与本文件无关
vi.mock("@/components/pet/PetStartupGuard", () => ({ PetStartupGuard: () => null }));

import i18n from "@/i18n";
import HomePage from "@/pages/home";
import { useWorkspaceStore } from "@/stores/workspaceStore";
import { useTheme } from "@/components/common/theme-provider";
import WorkspacePageHeader from "@/components/common/WorkspacePageHeader";
import { resetWorkspaceNavMock } from "../msw/tauriMocks";
import type { WorkspaceTab } from "@/types/workspace";

function renderHost() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const wrapper = ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={client}>{children}</QueryClientProvider>
  );
  return render(<HomePage />, { wrapper });
}

async function switchTab(tab: WorkspaceTab) {
  fireEvent.click(screen.getByRole("button", { name: i18n.t(`workspace.tab.${tab}`) }));
  await waitFor(() => expect(useWorkspaceStore.getState().activeTab).toBe(tab));
}

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

beforeEach(() => {
  mmState.reducedTransparency = false;
  localStorage.clear();
  localStorage.setItem("tauri-ui-theme", "light");
  document.documentElement.classList.remove("light", "dark");
  document.documentElement.classList.add("light");
  themeState.windows.forEach((w) => {
    w.theme = "light";
  });
  resetWorkspaceNavMock();
  // zustand 模块级单例：复位到「首启落消息看板」初值
  useWorkspaceStore.setState({
    activeTab: "messages",
    visitedTabs: ["messages"],
    appliedRevision: 0,
  });
});

describe("workspace-theme（Task 8：已定主题与轻玻璃契约）", () => {
  it("1. 主导航：可访问名「主导航」、玻璃材质类在场；活动页签 aria-current + 非颜色指示", () => {
    renderHost();

    const nav = screen.getByRole("navigation", { name: "主导航" });
    expect(nav).toBeVisible();
    // 材质契约：导航是轻玻璃面（内容面板永不带此类）
    expect(nav).toHaveClass("workspace-glass");

    const active = within(nav).getByRole("button", { name: i18n.t("workspace.tab.messages") });
    expect(active).toHaveAttribute("aria-current", "page");
    // 非颜色指示：data-active 状态属性（配合字重/强调点，选中不只靠颜色）
    expect(active).toHaveAttribute("data-active", "true");

    const inactive = within(nav).getByRole("button", { name: i18n.t("workspace.tab.speed") });
    expect(inactive.getAttribute("aria-current")).toBeNull();
    expect(inactive).toHaveAttribute("data-active", "false");
  });

  it("2. 主题 SSOT：主窗切 dark 写 DB 并广播，main/settings 两窗经既有同步收敛 dark", async () => {
    themeState.activeMockWindow = "main";
    function MainHarness() {
      const { theme, setTheme } = useTheme();
      return (
        <button data-testid="to-dark" onClick={() => setTheme("dark")}>
          <span data-testid="main-theme">{theme}</span>
        </button>
      );
    }
    render(<MainHarness />);

    fireEvent.click(screen.getByTestId("to-dark"));
    // 本窗立即生效 + 写 DB 事实源（Rust set_theme 广播的源头）
    expect(screen.getByTestId("main-theme").textContent).toBe("dark");
    expect(document.documentElement.classList.contains("dark")).toBe(true);
    expect(localStorage.getItem("tauri-ui-theme")).toBe("dark");

    // 第二个窗口（settings）：独立 WebView = 独立模块实例
    vi.resetModules();
    themeState.activeMockWindow = "settings";
    await import("@/components/common/theme-provider");

    // 模拟 Rust set_theme 向所有窗口广播 tuvis-theme-changed
    await act(async () => {
      for (const listener of themeState.themeListeners) {
        listener({ payload: { theme: "dark" } });
      }
    });

    expect(themeState.windows.map((w) => w.label)).toEqual(
      expect.arrayContaining(["main", "settings"])
    );
    expect(themeState.windows.every((w) => w.theme === "dark")).toBe(true);
    expect(localStorage.getItem("tauri-ui-theme")).toBe("dark");
  });

  it("3a. 系统要求减少透明度：玻璃面声明实心回退且仍可读可操作", async () => {
    mmState.reducedTransparency = true;
    renderHost();

    const nav = screen.getByRole("navigation", { name: "主导航" });
    expect(nav).toHaveClass("workspace-glass");
    expect(nav).toHaveAttribute("data-glass-fallback", "reduced-transparency");

    // 内容照常渲染且可操作：点「输出速度」页签能切页、页面标题在场
    await switchTab("speed");
    expect(
      screen.getByRole("heading", { level: 1, name: i18n.t("workspace.page.speed") })
    ).toBeInTheDocument();
    // Task 12 起速度页体 = SpeedDashboard 组合页（过渡期 TpsCard 已被替换）
    expect(await screen.findByTestId("speed-dashboard")).toBeInTheDocument();
  });

  it("3b. 内核不支持 backdrop-filter：玻璃面同样声明实心回退", async () => {
    const cssOwner = window as unknown as { CSS?: { supports?: unknown } };
    const originalSupports = cssOwner.CSS?.supports;
    Object.defineProperty(cssOwner, "CSS", {
      value: { supports: () => false },
      configurable: true,
      writable: true,
    });
    try {
      renderHost();
      const nav = screen.getByRole("navigation", { name: "主导航" });
      expect(nav).toHaveAttribute("data-glass-fallback", "no-backdrop-filter");
      // 文本在场：页签与页面标题照常渲染
      expect(
        within(nav).getByRole("button", { name: i18n.t("workspace.tab.messages") })
      ).toBeInTheDocument();
      expect(
        screen.getByRole("heading", { level: 1, name: i18n.t("workspace.page.messages") })
      ).toBeInTheDocument();
    } finally {
      Object.defineProperty(cssOwner, "CSS", {
        value: originalSupports !== undefined ? { supports: originalSupports } : undefined,
        configurable: true,
        writable: true,
      });
    }
  });

  it("4. WorkspacePageHeader：title/description/actions 三槽齐备", () => {
    render(
      <WorkspacePageHeader
        title="页面标题"
        description="次级说明"
        actions={<button type="button">动作</button>}
      />
    );
    expect(screen.getByRole("heading", { level: 1, name: "页面标题" })).toBeInTheDocument();
    expect(screen.getByText("次级说明")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "动作" })).toBeInTheDocument();
  });

  it("5. 四页各挂统一页头：活动页恰有一个 h1 且文案正确", async () => {
    renderHost();

    const tabs: Array<WorkspaceTab> = ["messages", "resources", "usage", "speed"];
    for (const tab of tabs) {
      await switchTab(tab);
      // 等该页主体就位（首访挂载）
      await screen.findByTestId(`workspace-page-${tab}`);
      // 非活动页已被 aria-hidden 摘出 a11y 树：可见页头恰一个
      const h1s = screen.getAllByRole("heading", { level: 1 });
      expect(h1s, `页签 ${tab} 应恰有一个 h1`).toHaveLength(1);
      expect(h1s[0]).toHaveTextContent(i18n.t(`workspace.page.${tab}`));
    }
  });

  it("6. 实机反馈轮 #3b：页头防挤压类锁——标题块 shrink-0、h1 nowrap、动作槽 flex-wrap + min-w-0", async () => {
    // jsdom 不做布局，断言不了「标题不再被挤成竖排 CJK」——以类锁为机制：
    // 标题块 shrink-0 + h1 whitespace-nowrap（标题永不收缩/逐字换行），
    // 动作槽 flex-wrap + min-w-0（超宽动作换行而不是挤标题）。
    // 用真实用量页（动作槽最重的页面）验证：h1 文案 = 页标题，且类锁在真挂载点上。
    renderHost();
    await switchTab("usage");
    await screen.findByTestId("workspace-page-usage-body");

    const h1 = screen.getByRole("heading", { level: 1 });
    expect(h1).toHaveTextContent(i18n.t("workspace.page.usage"));
    // 标题块：h1 的直接父块 shrink-0
    expect(h1.parentElement).toHaveClass("shrink-0");
    expect(h1).toHaveClass("whitespace-nowrap");

    // 动作槽：header 的最后一个子块（与标题块互为 justify-between 两侧）
    const header = h1.closest("header")!;
    const actionsWrapper = header.lastElementChild!;
    expect(actionsWrapper).not.toBe(h1.parentElement);
    expect(actionsWrapper).toHaveClass("flex-wrap");
    expect(actionsWrapper).toHaveClass("min-w-0");
    expect(actionsWrapper).toHaveClass("justify-end");
  });
});
