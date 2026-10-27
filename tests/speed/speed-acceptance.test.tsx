// Task 13（2026-10-26 计划）· 整链验收（vitest + jsdom，真实 HomePage + mocked IPC）。
// 判据来源：task-13-brief Step4 + Spec §11「UI切换 / 导航与用量 / 主题」三行：
//  * 四页导航各自保状态：用量区间（30d）与速度区间/选中组合（1h 默认）互不污染、
//    切页往返不重置（spec §2.6）；速度页租约在切离时释放（最后一条心跳 visible=false）、
//    轮询停止，切回恢复（spec §2.7）；
//  * 速度页双模式全流程：切模式一次同步列表/主数字/单位/有效 n；Codex 临时 fallback
//    显著 + 流式按钮禁用，切回原生源恢复偏好（spec §5.3 / §11 UI切换行）；
//  * 采集管理：面板开合 + 接入预览（脱敏）→ 应用 → 回执「下次启动生效」→ tps 域缓存
//    失效重取（spec §8.6/§8.7）；opencode「待重启」态在场；
//  * 主题：深浅两态下四页主体照常渲染可读（行为契约——色层由截图清单验证，
//    workspace-theme.test.tsx 同纪律）；
//  * 隐藏窗返回：隐藏期到达的跨窗目标（事件错过）在唤起时经 peek 补上（spec §2.4）。
// 纪律：真实链路——invoke 走 tests/setup.ts 接线的 tauriInvokeMock；导航事件登记进
// tauriMocks 的 workspaceNavMock.listeners；不 mock react-query；不启动任何真实窗口。
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";

// jsdom 未实现 matchMedia（theme-provider / MainTitleBar 首帧主题判定需要）
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

const { createWindowMock, createdWindowLabels, otherEventListeners, hookMounts } = vi.hoisted(
  () => ({
    createWindowMock: vi.fn(async (label: string) => {
      createdWindowLabels.push(label);
    }),
    createdWindowLabels: [] as string[],
    otherEventListeners: new Map<string, Set<(e: { payload: unknown }) => void>>(),
    hookMounts: { sessions: 0, notification: 0 },
  })
);

// 事件替身：导航事件登记进 mock 队列的 listeners（request 时回放）；
// 其余事件照旧登记本地表，返回解除函数（workspace-navigation.test.tsx 同款）。
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
      let set = otherEventListeners.get(event);
      if (!set) {
        set = new Set();
        otherEventListeners.set(event, set);
      }
      set.add(handler);
      return () => {
        set.delete(handler);
      };
    }),
    once: vi.fn(async () => () => {}),
    emit: vi.fn(async () => {}),
  };
});

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

// 开窗层替身：整链验收里旧详情入口绝不创建独立窗口
vi.mock("@/lib/window", () => ({ createWindow: createWindowMock }));

// 会话轮询/通知在宿主恰好一次（mount 计数法，workspace-navigation.test.tsx 同款）
vi.mock("@/hooks/useSessions", async () => {
  const { useEffect } = await import("react");
  return {
    useSessions: vi.fn(() => {
      useEffect(() => {
        hookMounts.sessions += 1;
      }, []);
      return undefined;
    }),
  };
});
vi.mock("@/hooks/useNotification", async () => {
  const { useEffect } = await import("react");
  return {
    useNotification: vi.fn(() => {
      useEffect(() => {
        hookMounts.notification += 1;
      }, []);
      return undefined;
    }),
  };
});
vi.mock("@/components/pet/PetStartupGuard", () => ({ PetStartupGuard: () => null }));

import i18n from "@/i18n";
import HomePage from "@/pages/home";
import { useWorkspaceStore } from "@/stores/workspaceStore";
import { resetWorkspaceNavMock, tauriInvokeMock, workspaceNavMock } from "../msw/tauriMocks";
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

function callsOf(cmd: string): number {
  return tauriInvokeMock.mock.calls.filter(([c]) => c === cmd).length;
}

/** 某命令的调用实参列表（断言入参用） */
function callArgsOf(cmd: string): Array<Record<string, unknown> | undefined> {
  return tauriInvokeMock.mock.calls
    .filter(([c]) => c === cmd)
    .map(([, args]) => args as Record<string, unknown> | undefined);
}

function lastHeartbeat(): { visible?: boolean } | undefined {
  const call = tauriInvokeMock.mock.calls.filter(([c]) => c === "tps_view_heartbeat").at(-1) as
    [string, { visible?: boolean }] | undefined;
  return call?.[1];
}

/** 宽布局组合列表（speed-series-list）里的行 */
function listRows(): HTMLElement[] {
  const list = screen.getByTestId("speed-series-list");
  return within(list).getAllByTestId("speed-series-row");
}

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

beforeEach(() => {
  localStorage.clear();
  localStorage.setItem("tauri-ui-theme", "light");
  document.documentElement.classList.remove("dark");
  document.documentElement.classList.add("light");
  createdWindowLabels.length = 0;
  createWindowMock.mockClear();
  resetWorkspaceNavMock();
  otherEventListeners.clear();
  hookMounts.sessions = 0;
  hookMounts.notification = 0;
  // zustand 模块级单例：复位到「首启落消息看板」初值
  useWorkspaceStore.setState({
    activeTab: "messages",
    visitedTabs: ["messages"],
    appliedRevision: 0,
  });
});

/** 详情标识可选框的选中项文本（tool · model [· channel]）。 */
function detailTitleText(): string {
  const sel = screen.getByTestId("speed-detail-title") as HTMLSelectElement;
  return sel.options[sel.selectedIndex]?.textContent ?? "";
}

describe("speed-acceptance（Task 13：整链验收）", () => {
  it("1. 四页整页流：用量 30d 与速度 5h/选中组合各自保状态，切页往返不重置", async () => {
    renderHost();

    // 用量页：默认 last7d → 改 last30d
    await switchTab("usage");
    expect(await screen.findByTestId("usage-hero")).toBeInTheDocument();
    fireEvent.click(screen.getByTestId("usage-range-last30d"));
    await waitFor(() =>
      expect(screen.getByTestId("usage-range-last30d")).toHaveAttribute("aria-pressed", "true")
    );

    // 速度页：默认 5h（2026-10-27 实机反馈 #4b）；改 15m 并选中 dsh
    await switchTab("speed");
    await screen.findByTestId("speed-dashboard");
    expect(screen.getByTestId("speed-range-5h")).toHaveAttribute("aria-pressed", "true");
    fireEvent.click(screen.getByTestId("speed-range-15m"));
    await waitFor(() =>
      expect(screen.getByTestId("speed-range-15m")).toHaveAttribute("aria-pressed", "true")
    );
    fireEvent.click(listRows().find((r) => r.getAttribute("data-tool") === "dsh")!);
    await waitFor(() => expect(detailTitleText()).toContain("doubao-seed-1.6"));

    // 切到消息页再回来：速度页 15m + dsh 全保留（主体保持挂载，仅隐藏）
    await switchTab("messages");
    expect(screen.getByTestId("speed-range-15m")).toBeInTheDocument();
    await switchTab("speed");
    expect(screen.getByTestId("speed-range-15m")).toHaveAttribute("aria-pressed", "true");
    expect(detailTitleText()).toContain("doubao-seed-1.6");

    // 用量页往返：last30d 仍按下（两页状态互不污染）
    await switchTab("usage");
    expect(screen.getByTestId("usage-range-last30d")).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByTestId("usage-hero")).toBeInTheDocument();
  });

  it("2. 双模式全流程：Codex fallback 显著且切回原生源恢复；切模式一次同步", async () => {
    renderHost();
    await switchTab("speed");
    await screen.findByTestId("speed-metrics");
    expect(screen.getAllByTestId("speed-chart")).toHaveLength(1);
    expect(screen.getByTestId("speed-mode")).toHaveAttribute("data-mode", "stream");

    // Codex 临时 fallback（默认 stream 偏好下）：显著提示 + 流式按钮禁用 + 整页仍恰好一张图
    fireEvent.click(listRows().find((r) => r.getAttribute("data-tool") === "codex")!);
    await waitFor(() => expect(detailTitleText()).toContain("gpt-5-codex"));
    expect(screen.getByTestId("speed-mode")).toHaveAttribute("data-mode", "response");
    expect(screen.getByTestId("speed-fallback-notice").textContent).toContain(
      "仅支持响应吞吐（含等待）"
    );
    const streamBtn = screen.getByTestId("speed-mode-stream");
    expect(streamBtn).toBeDisabled();
    expect(streamBtn.getAttribute("title") ?? "").toContain("流式生成窗");
    expect(screen.getAllByTestId("speed-chart")).toHaveLength(1);

    // 切回原生源：偏好恢复 stream，提示消失（不在 TPS 标题下混入响应数值）
    fireEvent.click(listRows().find((r) => r.getAttribute("data-tool") === "zcode")!);
    await waitFor(() => expect(detailTitleText()).toContain("glm-5.3-flash"));
    expect(screen.getByTestId("speed-mode")).toHaveAttribute("data-mode", "stream");
    expect(screen.queryByTestId("speed-fallback-notice")).toBeNull();

    // 显式切 response：data-mode 根同步 + 请求载荷跟随 + 单位换 Token/s
    fireEvent.click(screen.getByTestId("speed-mode-response"));
    expect(screen.getByTestId("speed-mode")).toHaveAttribute("data-mode", "response");
    await waitFor(() => {
      const call = tauriInvokeMock.mock.calls
        .filter(([c]) => c === "tps_speed_dashboard")
        .at(-1) as [string, { query?: { metricMode?: string } }] | undefined;
      expect(call?.[1]?.query?.metricMode).toBe("response");
    });
    await waitFor(() => expect(screen.getByTestId("speed-rate-unit")).toHaveTextContent("Token/s"));
  });

  it("3. 采集管理：开面板看七源 → 接入预览（脱敏）→ 应用 → 下次启动生效 + 缓存失效重取", async () => {
    renderHost();
    await switchTab("speed");
    await screen.findByTestId("speed-metrics");
    const dashboardsBefore = callsOf("tps_speed_dashboard");
    expect(dashboardsBefore).toBeGreaterThan(0);

    // 开面板：来源一级来自 dashboard 快照（七源；opencode = 待重启，codex = 已接入·仅响应）
    fireEvent.click(screen.getByTestId("speed-sources-open"));
    await screen.findByTestId("speed-sources-panel");
    for (const tool of ["claude", "codex", "dsh", "kimi", "opencode", "workbuddy", "zcode"]) {
      expect(screen.getByTestId(`speed-source-${tool}`)).toBeInTheDocument();
    }
    expect(screen.getByTestId("speed-status-opencode")).toHaveTextContent("待重启");
    expect(screen.getByTestId("speed-status-claude")).toHaveTextContent("待配置");
    expect(screen.getByTestId("speed-status-codex")).toHaveTextContent("已接入");
    expect(screen.getByTestId("speed-status-codex")).toHaveTextContent("仅响应");

    // 接入预览：脱敏 chip + 原生设置路径；取消不动任何事实
    fireEvent.click(screen.getByTestId("speed-install-claude"));
    const preview = await screen.findByTestId("tps-preview");
    expect(preview).toHaveTextContent("/home/mock/.claude/settings.json");
    expect(within(preview).getAllByTestId("tps-masked-chip").length).toBeGreaterThanOrEqual(1);
    expect(callsOf("tps_install_apply")).toBe(0);
    fireEvent.click(within(preview).getByText("取消"));
    expect(screen.queryByTestId("tps-preview")).not.toBeInTheDocument();

    // 再走一次：应用 → 回执 + 下次启动生效；tps 域缓存失效（dashboard 重取）
    const dashboardsAtApply = callsOf("tps_speed_dashboard");
    fireEvent.click(screen.getByTestId("speed-install-claude"));
    fireEvent.click(await screen.findByTestId("tps-apply"));
    const receipt = await screen.findByTestId("tps-receipt");
    expect(callsOf("tps_install_apply")).toBe(1);
    expect(callArgsOf("tps_install_apply")[0]?.previewId).toBe("mock-preview-claude");
    expect(within(receipt).getByTestId("tps-restart-hint")).toHaveTextContent("下次启动生效");
    // 安装落库即失效 ["tps"] 前缀：速度 dashboard 重取（配置后缓存失效）
    await waitFor(() => expect(callsOf("tps_speed_dashboard")).toBeGreaterThan(dashboardsAtApply));
  });

  it("4. 深浅主题均可读：两态下四页主体照常渲染、导航玻璃契约与 a11y 保持", async () => {
    // 深色：SSOT 初始态写 localStorage + html 类（theme-provider 读取形态）
    localStorage.setItem("tauri-ui-theme", "dark");
    document.documentElement.classList.remove("light");
    document.documentElement.classList.add("dark");
    const first = renderHost();

    const nav = screen.getByRole("navigation", { name: "主导航" });
    expect(nav).toHaveClass("workspace-glass");
    expect(nav).not.toHaveAttribute("data-glass-fallback");
    for (const tab of ["usage", "speed", "resources", "messages"] as WorkspaceTab[]) {
      await switchTab(tab);
      expect(screen.getByTestId(`workspace-page-${tab}`)).toBeInTheDocument();
    }
    // 速度页在深色下完整渲染（可读性行为契约：结构在场、单图、四格）
    await switchTab("speed");
    await screen.findByTestId("speed-metrics");
    for (const id of [
      "speed-tile-rate",
      "speed-tile-first-token",
      "speed-tile-model",
      "speed-tile-tool",
    ]) {
      expect(screen.getByTestId(id)).toBeInTheDocument();
    }
    expect(screen.getAllByTestId("speed-chart")).toHaveLength(1);
    expect(document.documentElement.classList.contains("dark")).toBe(true);

    // 浅色回归：同套契约在 light 下同样成立（先卸第一份宿主，避免双导航/双页签）
    localStorage.setItem("tauri-ui-theme", "light");
    document.documentElement.classList.remove("dark");
    document.documentElement.classList.add("light");
    first.unmount();
    // zustand 复位后重挂一份宿主
    useWorkspaceStore.setState({
      activeTab: "messages",
      visitedTabs: ["messages"],
      appliedRevision: 0,
    });
    renderHost();
    await switchTab("speed");
    await screen.findByTestId("speed-metrics");
    expect(screen.getByTestId("speed-mode")).toHaveAttribute("data-mode", "stream");
    expect(document.documentElement.classList.contains("light")).toBe(true);
  });

  it("5. 用量 30d 与速度 5h 独立：切用量档位零新增 tps_speed_dashboard", async () => {
    renderHost();
    await switchTab("speed");
    await screen.findByTestId("speed-metrics");
    await waitFor(() => expect(callsOf("tps_speed_dashboard")).toBeGreaterThan(0));
    const speedAtStart = callsOf("tps_speed_dashboard");

    // 切到用量页改 30d：用量域请求发生，速度域纹丝不动
    await switchTab("usage");
    await screen.findByTestId("usage-hero");
    const usageBefore = callsOf("usage_dashboard");
    fireEvent.click(screen.getByTestId("usage-range-last30d"));
    await waitFor(() => expect(callsOf("usage_dashboard")).toBeGreaterThan(usageBefore));
    expect(callsOf("tps_speed_dashboard")).toBe(speedAtStart);

    // 速度页档位仍是默认 5h（速度区间与用量范围独立）
    await switchTab("speed");
    expect(screen.getByTestId("speed-range-5h")).toHaveAttribute("aria-pressed", "true");
  });

  it("6. 离开速度页释放租约并停轮询；隐藏窗返回经 peek 恢复目标页", async () => {
    renderHost();
    await switchTab("speed");
    await screen.findByTestId("speed-metrics");
    await waitFor(() => expect(callsOf("tps_speed_dashboard")).toBeGreaterThan(0));
    await waitFor(() => expect(lastHeartbeat()?.visible).toBe(true));

    // 切走：租约释放（最后一条心跳 visible=false）
    await switchTab("messages");
    await act(async () => {
      await Promise.resolve();
    });
    expect(lastHeartbeat()?.visible).toBe(false);

    // 隐藏期（页面隐藏 + 钟拨过 5s 轮询窗）：聚焦零新速度请求
    vi.useFakeTimers({ now: new Date(Date.now() + 61_000), shouldAdvanceTime: true });
    const speedAtHide = callsOf("tps_speed_dashboard");
    expect(speedAtHide).toBeGreaterThan(0);
    await act(async () => {
      window.dispatchEvent(new Event("visibilitychange"));
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(callsOf("tps_speed_dashboard")).toBe(speedAtHide);
    vi.useRealTimers();

    // 隐藏窗返回恢复（两条路径）：
    // ① 切回速度页：轮询与租约恢复
    await switchTab("speed");
    await waitFor(() => expect(callsOf("tps_speed_dashboard")).toBeGreaterThan(speedAtHide));
    await waitFor(() => expect(lastHeartbeat()?.visible).toBe(true));

    // ② 主窗隐藏期到达的跨窗目标：事件无人接（摘掉监听模拟错过）→ 目标滞留队列；
    //    唤起（visibilitychange → visible）后 hook 再 peek，补上错过的目标（spec §2.4）。
    //    派发到 document（hook 监听在 document；bubbles 让 window 侧 focusManager 同收）
    const listeners = [...workspaceNavMock.listeners];
    for (const l of listeners) workspaceNavMock.listeners.delete(l);
    await tauriInvokeMock("workspace_request_navigation", { tab: "usage" });
    for (const l of listeners) workspaceNavMock.listeners.add(l);
    expect(useWorkspaceStore.getState().activeTab).toBe("speed");
    await act(async () => {
      document.dispatchEvent(new Event("visibilitychange", { bubbles: true }));
      await Promise.resolve();
    });
    await waitFor(() => expect(useWorkspaceStore.getState().activeTab).toBe("usage"));
    expect(workspaceNavMock.pending).toBeNull();
    // 应用层监控不因切页/唤起重复挂载
    expect(hookMounts.sessions).toBe(1);
    expect(hookMounts.notification).toBe(1);
  });
});
