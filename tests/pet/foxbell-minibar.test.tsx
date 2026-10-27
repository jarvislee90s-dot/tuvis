// Task 13（计划②）· 宠物浮窗接线（`FoxbellPet` / `PetMenu`）6 用例。
// 判据来源：spec D12（两个触发方式共用一个容器 + 唯一可点元素「详情 »」）、D13（悬停 0.5s / 移开 2s（原 200ms，2026-10-07 放宽）
// 宽限 / 拖拽隐藏 + 松手 0.5s 恢复 / ESC 逐层）、D20（浮窗与卡片**求和**进窗口高度）、
// spec §7.2（触发清单含「悬停浮窗」）、计划 §3 第 15/16/41/42 条与 Task 13 步骤 5/6。
//
// 纪律：
//  * jsdom 里 `getBoundingClientRect()` **恒 0**（计划 §3 第 33 条）⇒ 高度只能断 `syncSize` 的**入参**
//    （它由纯函数 `windowHeightSum` 推出），**禁止**「渲染高度 = Npx」这类断言。
//  * 真实数据走 `tests/setup.ts` 接进 `@tauri-apps/api/core` 的单例（① 的 mock case → ② 同源夹具），
//    不另起 `vi.mock("@tauri-apps/api/core")`（那会绕开夹具）。
//  * 窗口层与开窗层 mock 法同 `usage-window.test.tsx`：断言「带什么参数」，不验 Tauri 内部行为。
import { act, fireEvent, render, screen } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";

const { createWindowMock, setSizeMock } = vi.hoisted(() => ({
  createWindowMock: vi.fn(async () => {}),
  setSizeMock: vi.fn(async () => {}),
}));
// 「详情 »」→ `openUsageDashboard`：Task 7 四页同窗后判据反转为「**不**建窗、走主窗导航」
vi.mock("@/lib/window", () => ({ createWindow: createWindowMock }));
// 宠物窗口 API（与 foxbell-interactions.test.tsx 同模式），setSize 单独提出来断言高度入参
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({
    show: vi.fn(),
    hide: vi.fn(),
    setAlwaysOnTop: vi.fn(),
    setIgnoreCursorEvents: vi.fn(),
    setPosition: vi.fn(),
    setSize: setSizeMock,
    outerPosition: vi.fn(async () => ({ x: 0, y: 0 })),
    outerSize: vi.fn(async () => ({ width: 680, height: 520 })),
    scaleFactor: vi.fn(async () => 1),
  }),
  currentMonitor: vi.fn(async () => ({
    workArea: { position: { x: 0, y: 0 }, size: { width: 1440, height: 900 } },
    scaleFactor: 1,
  })),
}));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async () => () => {}),
  emit: vi.fn(async () => {}),
}));
// 会话查询 mock 掉：本文件只验浮窗接线，卡片区不参与（jsdom 里卡片高恒 0）
vi.mock("@/lib/query/queries/sessions", () => ({ useSessionsQuery: () => ({ data: undefined }) }));

// 空 manifest：本文件不验语音，空清单让 VoicePlayer.pick 走空组静默跳过（spec E5），
// 避免 jsdom 反复打印「HTMLMediaElement's load() 未实现」的噪音
const fetchMock = vi.fn(async () => ({ json: async () => [] }));
vi.stubGlobal("fetch", fetchMock);

import i18n from "@/i18n";
import { FoxbellPet } from "@/components/pet/FoxbellPet";
import {
  MINI_BAR_ROWS,
  MINI_GRACE_MS,
  MINI_HOVER_MS,
  MINI_RESTORE_MS,
  miniBarHeight,
  windowHeightSum,
} from "@/lib/usage/miniBar";
import { tauriInvokeMock } from "../msw/tauriMocks";

/** 渲染桌宠（浮窗的三条查询需要 QueryClientProvider；宠物窗口真机由 main.tsx 提供同一个） */
function renderPet() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const wrapper = ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={client}>{children}</QueryClientProvider>
  );
  return render(<FoxbellPet />, { wrapper });
}

/** 推进假时钟并把在途微任务排空（查询 / 采集 / 尺寸防抖都要 flush 才落地） */
async function tick(ms: number) {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(ms);
  });
}

/** 悬停精灵直到浮窗出现（含查询落地的那一拍） */
async function hoverUntilShown(): Promise<HTMLElement> {
  fireEvent.pointerEnter(screen.getByTestId("pet-sprite"));
  await tick(MINI_HOVER_MS);
  await tick(0);
  return screen.getByTestId("usage-mini");
}

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

describe("foxbell-minibar（计划② Task 13 步骤 5/6：浮窗接线）", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    localStorage.clear();
    localStorage.setItem("mam-pet-visible", "1");
    vi.stubGlobal("fetch", fetchMock);
  });
  afterEach(() => vi.useRealTimers());

  it("1. 悬停 500ms 出浮窗（5 行）；移开先留 200ms 宽限、到点才消失", async () => {
    renderPet();
    await tick(20); // 排空挂载期几何读取
    expect(screen.queryByTestId("usage-mini")).toBeNull();

    fireEvent.pointerEnter(screen.getByTestId("pet-sprite"));
    await tick(MINI_HOVER_MS - 1);
    expect(screen.queryByTestId("usage-mini")).toBeNull(); // 未到 500ms 不出
    await tick(1);
    await tick(0);
    const box = screen.getByTestId("usage-mini");
    expect(box.dataset.mode).toBe("hover"); // 悬停进入的是 hover 模式
    expect(screen.getAllByTestId(/^usage-mini-row-/)).toHaveLength(MINI_BAR_ROWS);

    fireEvent.pointerLeave(screen.getByTestId("pet-sprite"));
    await tick(MINI_GRACE_MS - 1);
    expect(screen.queryByTestId("usage-mini")).not.toBeNull(); // 宽限期内还在（够鼠标移到浮窗上）
    await tick(1);
    expect(screen.queryByTestId("usage-mini")).toBeNull();
  });

  it("2. 宽限桥接：指针进入浮窗即取消宽限；点「详情 »」→ 打开看板窗口并关闭浮窗", async () => {
    renderPet();
    await tick(20);
    await hoverUntilShown();

    fireEvent.pointerLeave(screen.getByTestId("pet-sprite"));
    fireEvent.pointerEnter(screen.getByTestId("pet-mini-wrap")); // 真人鼠标穿过间隙进浮窗
    await tick(MINI_GRACE_MS + 50);
    expect(screen.queryByTestId("usage-mini")).not.toBeNull(); // 宽限已取消，浮窗不会被卸载

    fireEvent.click(screen.getByTestId("usage-mini-row-detail"));
    await tick(0);
    // Task 7：唯一入口 = workspace_request_navigation（唤起主窗 + 切模型用量页签），零建窗
    const navCalls = tauriInvokeMock.mock.calls.filter(
      ([cmd]) => cmd === "workspace_request_navigation"
    );
    expect(navCalls).toHaveLength(1);
    expect((navCalls[0] as [string, Record<string, unknown>])[1]).toEqual({ tab: "usage" });
    expect(createWindowMock).not.toHaveBeenCalled();
    expect(screen.queryByTestId("usage-mini")).toBeNull(); // 钻取即关浮窗（spec P2 第 5 条）
  });

  it("3. 拖拽期间浮窗隐藏，松手 500ms 后才恢复（恢复的仍是 hover 模式）", async () => {
    renderPet();
    await tick(20);
    await hoverUntilShown();

    const sprite = screen.getByTestId("pet-sprite");
    fireEvent.pointerDown(sprite, {
      pointerId: 1,
      button: 0,
      clientX: 100,
      clientY: 300,
      screenX: 500,
      screenY: 700,
    });
    expect(screen.queryByTestId("usage-mini")).toBeNull(); // 按下即隐藏
    await tick(2000);
    expect(screen.queryByTestId("usage-mini")).toBeNull(); // 拖拽期间不回来

    fireEvent.pointerUp(sprite, { pointerId: 1, clientX: 100, clientY: 300 });
    await tick(MINI_RESTORE_MS - 1);
    expect(screen.queryByTestId("usage-mini")).toBeNull(); // 松手未满 500ms
    await tick(1);
    expect(screen.getByTestId("usage-mini").dataset.mode).toBe("hover");
  });

  it("4. 右键菜单「🏷 今日用量」→ manual 模式共用同一容器；菜单开着时浮窗由渲染守卫隐藏", async () => {
    renderPet();
    await tick(20);
    const sprite = screen.getByTestId("pet-sprite");

    fireEvent.contextMenu(sprite, { clientX: 120, clientY: 200 });
    expect(screen.getByTestId("pet-menu")).toBeTruthy();
    expect(screen.queryByTestId("usage-mini")).toBeNull(); // 菜单优先：浮窗不渲染（故 syncSize 传 miniH=0）

    const item = screen.getByTestId("pet-menu-mini-usage");
    expect(item.textContent).toBe(i18n.t("usage.menu.usage"));
    fireEvent.click(item);
    expect(screen.queryByTestId("pet-menu")).toBeNull(); // 菜单项自身先关菜单
    const box = screen.getByTestId("usage-mini");
    expect(box.dataset.mode).toBe("manual"); // 回调只置模式，容器与 hover 是同一个
    expect(screen.getAllByTestId(/^usage-mini-row-/)).toHaveLength(MINI_BAR_ROWS);

    // 再开菜单：manual 浮窗同样被守卫压住（层序：菜单在上）
    fireEvent.contextMenu(sprite, { clientX: 120, clientY: 200 });
    expect(screen.getByTestId("pet-menu")).toBeTruthy();
    expect(screen.queryByTestId("usage-mini")).toBeNull();
  });

  it("5. ESC 逐层：菜单开着时浮窗的键盘监听不注册 ⇒ 一次 ESC 只关菜单，再一次才关浮窗", async () => {
    renderPet();
    await tick(20);
    const sprite = screen.getByTestId("pet-sprite");

    // 先唤回 manual 浮窗
    fireEvent.contextMenu(sprite, { clientX: 120, clientY: 200 });
    fireEvent.click(screen.getByTestId("pet-menu-mini-usage"));
    expect(screen.getByTestId("usage-mini").dataset.mode).toBe("manual");

    // 再开菜单：此时是「菜单 + manual 浮窗」两层（浮窗被守卫隐藏）
    fireEvent.contextMenu(sprite, { clientX: 120, clientY: 200 });
    expect(screen.queryByTestId("usage-mini")).toBeNull();

    fireEvent.keyDown(window, { key: "Escape" });
    expect(screen.queryByTestId("pet-menu")).toBeNull(); // 第一层：菜单
    // 浮窗那一层**没被一起关掉**——这正是「菜单开着时不注册浮窗键盘监听」的可见后果
    expect(screen.getByTestId("usage-mini").dataset.mode).toBe("manual");

    fireEvent.keyDown(window, { key: "Escape" });
    expect(screen.queryByTestId("usage-mini")).toBeNull(); // 第二层：浮窗
  });

  it("6. 数据与几何：档位取设置（today）、分组恒 tool、悬停即按需采集一次；窗口高度 = 卡片 + 浮窗求和", async () => {
    renderPet();
    await tick(20);
    await hoverUntilShown();
    await tick(20); // 查询 / 采集落地

    // 行③/④ 就是同源夹具那两份数据（设置默认档「当日」+ 按工具分组）
    expect(screen.getByTestId("usage-mini-row-tools").textContent).toContain("Claude 124.66万");
    expect(screen.getByTestId("usage-mini-row-session").textContent).toContain("演示会话");

    const calls = (cmd: string) => tauriInvokeMock.mock.calls.filter(([c]) => c === cmd);
    const dash = calls("usage_dashboard")[0]?.[1] as { range: unknown; groupBy: unknown };
    expect(dash.range).toEqual({ preset: "today" }); // 设置项 miniBarRange（预设档只发 preset）
    expect(dash.groupBy).toBe("tool"); // 第 3 行是分工具汇总

    // 悬停浮窗是 spec §7.2 的采集触发点之一：挂载采一次、force 恒 false（最小间隔与单飞由后端保证）
    const collect = calls("usage_collect");
    expect(collect).toHaveLength(1);
    expect(collect[0][1]).toEqual({ force: false });
    // 读查询**绝不**触发扫描：本次会话里没有别的 collect（三个读命令只读账本，§3 第 15 条）
    expect(calls("usage_dashboard").length).toBe(1);

    // 窗口几何：浮窗与卡片竖直堆叠 ⇒ **求和**（D20 核心修订）；jsdom 卡片高恒 0
    await tick(60); // syncSize 防抖 50ms
    const size = setSizeMock.mock.calls.at(-1)?.[0] as { width: number; height: number };
    expect(size.width).toBe(340); // 宽度 px(340) 不动
    const expected = windowHeightSum({
      scale: 1,
      cardsH: 0,
      menuH: 0,
      candidatesH: 0,
      miniH: miniBarHeight(1),
    });
    expect(size.height).toBe(expected);
    expect(size.height).toBe(268 + 6 + 103); // = 377：旧口径（max）会是 268；漏掉 6px 间距会是 371
  });

  it("7. 松手恢复要对账指针：500ms 内指针离开精灵 ⇒ 浮窗不得恢复，高度也不得把浮窗算进去（修复轮 1）", async () => {
    renderPet();
    await tick(20);
    await hoverUntilShown();

    const sprite = screen.getByTestId("pet-sprite");
    // 单击（按下 + 松开）：进入「隐藏 ⇒ 500ms 后恢复 hover」的窗口
    fireEvent.pointerDown(sprite, { pointerId: 1, button: 0, clientX: 100, clientY: 300 });
    expect(screen.queryByTestId("usage-mini")).toBeNull();
    fireEvent.pointerUp(sprite, { pointerId: 1, clientX: 100, clientY: 300 });
    await tick(100);
    // 500ms 未到，指针移到宠物之外（真实路径 = 精灵 pointerleave；此时 miniMode 已是 null，
    // 宽限那一条分支不会生效，唯一能拦住恢复的就是「离开即作废 + 到点再对账」）
    fireEvent.pointerLeave(sprite);
    await tick(MINI_RESTORE_MS + 200);
    expect(screen.queryByTestId("usage-mini")).toBeNull(); // 不得冒出一个没有关闭通道的常驻 hover 浮窗

    // 窗口高度也必须停在「无浮窗」那一档（jsdom 卡片高恒 0 ⇒ base 268），不得把浮窗高度算进去
    await tick(60);
    const size = setSizeMock.mock.calls.at(-1)?.[0] as { width: number; height: number };
    expect(size.width).toBe(340);
    expect(size.height).toBe(
      windowHeightSum({ scale: 1, cardsH: 0, menuH: 0, candidatesH: 0, miniH: 0 })
    );
    expect(size.height).toBe(268);
  });
});
