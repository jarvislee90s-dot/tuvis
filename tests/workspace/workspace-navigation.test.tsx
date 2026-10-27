// Task 7（2026-10-26 计划）· 四页宿主、用量主体提取与可靠导航（vitest + jsdom）。
// 判据来源：brief Step1 + 公共契约「导航」条 + spec §2（导航、窗口与状态）。
//
// 覆盖：
//  * 监听就绪前到达的跨窗目标 ⇒ hook 订阅后 peek 恢复并应用；
//  * 请求2后过期 ACK(1) 不清队、store 不回退；乱序事件/peek 不把 store 往回挪；
//  * openUsageDashboard() 走主窗导航（workspace_request_navigation），绝不创建
//    usage-dashboard 窗口（createdWindowLabels 断言）；
//  * 四页反复切换：用量主体保持挂载、区间状态不重置；useSessions/useNotification
//    只在宿主挂载一次（切页不重复）。
//
// 纪律：真实 hook 路径——`listen` 替身把 `tuvis-workspace-navigation` 监听登记进
// tests/msw/tauriMocks.ts 的 `workspaceNavMock.listeners`，invoke 走 setup.ts 接线的
// `tauriInvokeMock`（mock 队列先存后 emit），前端 store/hook 不被替身短路。
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
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
// 其余事件照旧登记本地表（home 的 remote-changed 等），返回解除函数。
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

// 窗口层替身（WindowFrame / 导航 hook 的焦点监听）
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

// 开窗层替身：记录「测试期间创建过的窗口 label」（openWindow.ts 不应再创建任何窗口）
vi.mock("@/lib/window", () => ({ createWindow: createWindowMock }));

// 会话轮询/通知在宿主恰好一次：替身里用 mount-only effect 计数（hook 函数每次渲染
// 都会执行，「调用次数」不等于「挂载次数」；effect([]) 每次**实例挂载**恰跑一次——
// 若四页各自挂载监控，切页次数就会累加）
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
// 桌宠启动校验（EP2）与本文件无关：stub 掉，避免 pet_scan 深链路
vi.mock("@/components/pet/PetStartupGuard", () => ({ PetStartupGuard: () => null }));

import i18n from "@/i18n";
import HomePage from "@/pages/home";
import { openUsageDashboard, USAGE_WINDOW_LABEL } from "@/lib/usage/openWindow";
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

/** 点顶栏页签（走真实链路：WorkspaceTabs → openWorkspaceTab → invoke → mock 队列 → 事件 → hook） */
async function switchTab(tab: WorkspaceTab) {
  fireEvent.click(screen.getByRole("button", { name: i18n.t(`workspace.tab.${tab}`) }));
  await waitFor(() => expect(useWorkspaceStore.getState().activeTab).toBe(tab));
}

/** 手工派发一条 tuvis-workspace-navigation 事件（模拟 Rust emit 到已订阅的 hook） */
function dispatchNavigation(payload: { revision: number; tab: string }) {
  const listeners = workspaceNavMock.listeners;
  expect(listeners.size).toBeGreaterThan(0);
  for (const listener of listeners) listener({ payload });
}

/** 单条命令的调用次数（tauriInvokeMock 的历史记录） */
function callsOf(cmd: string): number {
  return tauriInvokeMock.mock.calls.filter(([c]) => c === cmd).length;
}

/** 全部 usage 域 IPC 读的调用次数（评审 Important 8 的口径：cmd 以 usage_ 开头） */
function usageIpcCalls(): number {
  return tauriInvokeMock.mock.calls.filter(([c]) => c.startsWith("usage_")).length;
}

/** 全部 tps 域 IPC 的调用次数（快照/点/心跳；评审修复轮 2 的口径：cmd 以 tps_ 开头） */
function tpsIpcCalls(): number {
  return tauriInvokeMock.mock.calls.filter(([c]) => c.startsWith("tps_")).length;
}

/** 最后一次 tps_view_heartbeat 的入参（租约续/放的真值源） */
function lastHeartbeat(): { visible?: boolean } | undefined {
  const call = tauriInvokeMock.mock.calls.filter(([c]) => c === "tps_view_heartbeat").at(-1) as
    [string, { visible?: boolean }] | undefined;
  return call?.[1];
}

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

beforeEach(() => {
  localStorage.clear();
  localStorage.setItem("tauri-ui-theme", "light");
  createdWindowLabels.length = 0;
  createWindowMock.mockClear();
  resetWorkspaceNavMock();
  otherEventListeners.clear();
  hookMounts.sessions = 0;
  hookMounts.notification = 0;
  // zustand 是模块级单例：用例间复位到「首启落消息看板」初值
  useWorkspaceStore.setState({
    activeTab: "messages",
    visitedTabs: ["messages"],
    appliedRevision: 0,
  });
});

describe("workspace-navigation（Task 7：四页宿主与可靠导航）", () => {
  it("1. 监听就绪前的跨窗目标：订阅后 peek 恢复、应用并 ACK 清队", async () => {
    // 主窗 hook 尚未挂载（比如还在启动），跨窗目标先到
    const nav = (await tauriInvokeMock("workspace_request_navigation", {
      tab: "usage",
    })) as { revision: number; tab: string };
    expect(nav).toEqual({ revision: 1, tab: "usage" });
    expect(workspaceNavMock.pending).toEqual({ revision: 1, tab: "usage" });

    renderHost();

    // 订阅 + peek 恢复：store 落到 usage，ACK 命中清队
    await waitFor(() => expect(useWorkspaceStore.getState().activeTab).toBe("usage"));
    await waitFor(() => expect(workspaceNavMock.pending).toBeNull());
    expect(useWorkspaceStore.getState().appliedRevision).toBe(1);
    expect(useWorkspaceStore.getState().visitedTabs).toContain("usage");
  });

  it("2. 请求2后过期 ACK(1) 不清队；store 按最新 revision 收敛且不回退", async () => {
    renderHost();

    // 两次请求（如宠物窗口连点两下）：mock 队列替换 pending，事件回放给 hook
    await tauriInvokeMock("workspace_request_navigation", { tab: "usage" });
    await waitFor(() => expect(useWorkspaceStore.getState().activeTab).toBe("usage"));
    await tauriInvokeMock("workspace_request_navigation", { tab: "speed" });
    await waitFor(() => expect(useWorkspaceStore.getState().activeTab).toBe("speed"));

    // 过期 ACK(1)：队列里已是 2 ⇒ false 且不动队
    expect(await tauriInvokeMock("workspace_ack_navigation", { revision: 1 })).toBe(false);
    expect(await tauriInvokeMock("workspace_peek_navigation")).toBeNull(); // 最新已被 hook ACK
    expect(useWorkspaceStore.getState().activeTab).toBe("speed");
    expect(useWorkspaceStore.getState().appliedRevision).toBe(2);
  });

  it("3. 乱序事件/peek 不把 store 往回挪（revision 单调裁决）", async () => {
    renderHost();

    dispatchNavigation({ revision: 5, tab: "speed" });
    await waitFor(() => expect(useWorkspaceStore.getState().activeTab).toBe("speed"));
    // 迟到的旧目标（revision 3 < 5）：忽略，不回退到 messages
    dispatchNavigation({ revision: 3, tab: "messages" });
    expect(useWorkspaceStore.getState().activeTab).toBe("speed");
    expect(useWorkspaceStore.getState().appliedRevision).toBe(5);
    // 乱序 ACK 同样只动命中的那一份
    expect(await tauriInvokeMock("workspace_ack_navigation", { revision: 3 })).toBe(false);
  });

  it("4. openUsageDashboard：走主窗导航，绝不创建 usage-dashboard 窗口", async () => {
    renderHost();

    await openUsageDashboard({ title: i18n.t("usage.title") });

    expect(createdWindowLabels).not.toContain(USAGE_WINDOW_LABEL);
    expect(createWindowMock).not.toHaveBeenCalled();
    const calls = tauriInvokeMock.mock.calls.filter(
      ([cmd]) => cmd === "workspace_request_navigation"
    );
    expect(calls.length).toBeGreaterThan(0);
    expect(calls.at(-1)?.[1]).toEqual({ tab: "usage" });
    await waitFor(() => expect(useWorkspaceStore.getState().activeTab).toBe("usage"));
  });

  it("5. 四页反复切换：用量主体保持挂载、区间状态不重置", async () => {
    renderHost();

    // 首次访问用量页：主体挂载出 hero
    await switchTab("usage");
    expect(await screen.findByTestId("usage-hero")).toBeInTheDocument();
    // 改动区间（默认 last7d → today）
    const todayBtn = screen.getByTestId("usage-range-today");
    expect(todayBtn).toHaveAttribute("aria-pressed", "false");
    fireEvent.click(todayBtn);
    await waitFor(() => expect(todayBtn).toHaveAttribute("aria-pressed", "true"));

    // 切走再切回：主体仍挂载（不重挂）、区间还是 today
    await switchTab("resources");
    expect(screen.getByTestId("usage-range-today")).toBeInTheDocument(); // 仅隐藏，未卸载
    await switchTab("usage");
    expect(screen.getByTestId("usage-range-today")).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByTestId("usage-hero")).toBeInTheDocument();

    // 速度页自 Task 12 起为 SpeedDashboard 组合页（过渡期 TPS 卡已被替换）；其余三页各就各位
    await switchTab("speed");
    expect(await screen.findByTestId("speed-dashboard")).toBeInTheDocument();
    expect(screen.getByTestId("speed-range-5h")).toHaveAttribute("aria-pressed", "true"); // 默认 5h（2026-10-27 实机反馈 #4b）
    await switchTab("messages");
    expect(screen.getByTestId("workspace-page-messages")).toBeInTheDocument();
  });

  it("6. useSessions / useNotification 在宿主恰好挂载一次（切页不重复）", async () => {
    renderHost();
    await switchTab("usage");
    await switchTab("resources");
    await switchTab("speed");
    await switchTab("messages");
    expect(hookMounts.sessions).toBe(1);
    expect(hookMounts.notification).toBe(1);
  });

  it("7. 非活动页是 visibility/inert 语义而非 display:none（评审 Important 1：保滚动）", async () => {
    renderHost();
    await switchTab("usage");
    await switchTab("resources");

    // 非活动页：不用 `hidden`（display:none 会拆盒树、丢滚动偏移），
    // 而是 `invisible`（visibility:hidden，盒子树保留 ⇒ 滚动位置存活）+ a11y/焦点隔离
    const usage = screen.getByTestId("workspace-page-usage");
    expect(usage.classList.contains("hidden")).toBe(false);
    expect(usage.classList.contains("invisible")).toBe(true);
    expect(usage.classList.contains("pointer-events-none")).toBe(true);
    expect(usage.getAttribute("aria-hidden")).toBe("true");
    expect(usage.hasAttribute("inert")).toBe(true);

    // 活动页：无任何隐藏语义（可见、可聚焦、进 a11y 树）
    const resources = screen.getByTestId("workspace-page-resources");
    expect(resources.classList.contains("invisible")).toBe(false);
    expect(resources.getAttribute("aria-hidden")).toBeNull();
    expect(resources.hasAttribute("inert")).toBe(false);

    // 四页堆叠在同一个 grid 单元格（同格重叠 ⇒ 非活动页不占额外布局高度）
    for (const id of [
      "workspace-page-messages",
      "workspace-page-resources",
      "workspace-page-usage",
      "workspace-page-speed",
    ]) {
      const section = screen.getByTestId(id);
      expect(section.classList.contains("col-start-1")).toBe(true);
      expect(section.classList.contains("row-start-1")).toBe(true);
    }
  });

  it("8. 用量页隐藏时窗口聚焦零 usage IPC；可见时恢复（评审 Important 2：focus refetch）", async () => {
    renderHost();

    // 访问用量页并等首屏数据齐（dashboard / collect / costs 都已发过）
    await switchTab("usage");
    expect(await screen.findByTestId("usage-hero")).toBeInTheDocument();
    await waitFor(() => expect(callsOf("usage_collect")).toBeGreaterThan(0));
    await waitFor(() => expect(callsOf("usage_costs")).toBeGreaterThan(0));

    // 切走（用量页隐藏 ⇒ 全部用量查询 enabled:false）并排空在飞微任务
    await switchTab("messages");
    await act(async () => {
      await Promise.resolve();
    });
    // 把时钟拨过各查询自带的 staleTime: 30_000（hooks 逐查询钉死，客户端默认改不动）——
    // 让「数据已过期」从而 focus refetch 真正上路，用例才在Exercise 重查路径而非 30 秒新鲜窗
    vi.useFakeTimers({ now: new Date(Date.now() + 31_000), shouldAdvanceTime: true });
    const countAtHide = usageIpcCalls();
    expect(countAtHide).toBeGreaterThan(0);

    // 模拟主窗口重新聚焦（react-query focusManager 监听 visibilitychange；
    // jsdom 的 document.visibilityState 恒为 visible ⇒ 该事件即「聚焦」）
    await act(async () => {
      window.dispatchEvent(new Event("visibilitychange"));
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(usageIpcCalls()).toBe(countAtHide); // 隐藏页：数据已过期，focus 也不产生任何 usage 读

    // 可见时恢复：切回用量页（enabled false→true 即重发过期查询）+ 聚焦，usage 读重新发生
    await switchTab("usage");
    vi.useRealTimers();
    await act(async () => {
      window.dispatchEvent(new Event("visibilitychange"));
      await Promise.resolve();
    });
    await waitFor(() => expect(usageIpcCalls()).toBeGreaterThan(countAtHide));
  });

  it("9. 隐藏的速度页/记录子页：零 tps_* 且心跳已释放、零 billing 重查（评审修复轮 2）", async () => {
    renderHost();

    // 访问速度页：SpeedDashboard（Task 12）挂载即首查 + 续租
    await switchTab("speed");
    await waitFor(() => expect(callsOf("tps_speed_dashboard")).toBeGreaterThan(0));
    await waitFor(() => expect(callsOf("tps_view_heartbeat")).toBeGreaterThan(0));
    // 访问用量页的记录子页签：BillingProfilesManager 恒挂载即查 billing
    await switchTab("usage");
    fireEvent.click(await screen.findByTestId("usage-tab-records"));
    await waitFor(() => expect(callsOf("usage_billing_get")).toBeGreaterThan(0));

    // 同时隐藏速度页与用量页，拨钟过 snapshot 5s / points 60s / billing staleTime 30s
    await switchTab("messages");
    await act(async () => {
      await Promise.resolve();
    });
    vi.useFakeTimers({ now: new Date(Date.now() + 61_000), shouldAdvanceTime: true });
    const tpsAtHide = tpsIpcCalls();
    const billingAtHide = callsOf("usage_billing_get");
    const tpsSnapshotsAtHide = callsOf("tps_speed_dashboard");

    // 聚焦（visibilitychange ⇒ react-query focusManager）：隐藏页零 tps_*、零 billing
    await act(async () => {
      window.dispatchEvent(new Event("visibilitychange"));
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(tpsIpcCalls()).toBe(tpsAtHide);
    expect(callsOf("tps_speed_dashboard")).toBe(tpsSnapshotsAtHide);
    expect(callsOf("usage_billing_get")).toBe(billingAtHide);
    // 页签切走即释放租约（最后一条心跳 visible=false），而不是带着 90s TTL 干等
    expect(lastHeartbeat()?.visible).toBe(false);

    // 切回速度页：轮询与续租恢复
    await switchTab("speed");
    vi.useRealTimers();
    await waitFor(() => expect(tpsIpcCalls()).toBeGreaterThan(tpsAtHide));
    await waitFor(() => expect(lastHeartbeat()?.visible).toBe(true));
  });
});
