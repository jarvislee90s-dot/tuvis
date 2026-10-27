// Task 6（计划②）· 大看板入口（3 用例）；Task 7（2026-10-26 计划）改判：四页同窗后
// 「打开用量详情」= 主窗导航，**任何入口都不再创建 usage-dashboard 窗口**（spec §2.3）。
//
//  * 用例 1 是**静态锁**（jsdom 测不出、实机只有「点了没反应」这一种征兆）：
//    `/usage` 兼容路由仍在 pageMap（Task 7 起为跳转适配页）；`usage-dashboard` label
//    留在 capability 白名单（兼容保留——没有代码路径再以它建窗）；宠物窗口保留
//    `core:webview:allow-create-webview-window`（其余子窗创建仍依赖它）。
//  * 用例 2 断言标题栏图表键 → `workspace_request_navigation(tab="usage")` 且
//    **零** createWindow——旧独立窗口常量（1040×720 等）就此退役。
//  * 用例 3 只验菜单项本身的两种行为（关菜单 + 开看板各一次）与它在第一个分隔线**之前**的位置；
//    FoxbellPet 的主题接线由既有 foxbell-*.test.tsx 的渲染用例兜底。
import { readFileSync } from "node:fs";
import path from "node:path";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";

const { createWindowMock } = vi.hoisted(() => ({ createWindowMock: vi.fn(async () => {}) }));
// 窗口层替身保留：用例 2 的判据从「带什么参数建窗」反转为「**不**建窗」
vi.mock("@/lib/window", () => ({ createWindow: createWindowMock }));

// WindowFrame/TitleBar 依赖 Tauri 窗口 API，jsdom 下全 mock（与 petSettings/toolManagement 同模式）
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

import i18n from "@/i18n";
import { MainTitleBar } from "@/components/common/main-title-bar";
import { PetMenu } from "@/components/pet/PetMenu";
import { tauriInvokeMock } from "../msw/tauriMocks";

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

describe("usage-window（计划② Task 6：大看板窗口与两个入口）", () => {
  beforeEach(() => {
    localStorage.clear();
    createWindowMock.mockClear();
  });

  it("1. 静态锁：/usage 已注册 + 看板进 capability 白名单 + 宠物窗口可开窗", () => {
    const root = process.cwd();
    const main = readFileSync(path.join(root, "src/main.tsx"), "utf8");
    // 漏注册 ⇒ 静默回落首页（计划 Task 6 步骤 8）
    expect(main).toContain('"/usage": UsageDashboardPage');

    const caps = ["default", "pet"].map((name) => ({
      name,
      json: JSON.parse(
        readFileSync(path.join(root, `src-tauri/capabilities/${name}.json`), "utf8")
      ) as { windows?: string[]; permissions?: string[] },
    }));
    const windows = caps.flatMap((c) => c.json.windows ?? []);
    expect(windows).toContain("usage-dashboard");

    // 宠物窗口的开窗权限：缺它则「标题栏 / 菜单」两个入口都静默失败（关键坑第 2 条）
    const pet = caps.find((c) => c.name === "pet");
    expect(pet?.json.permissions ?? []).toContain("core:webview:allow-create-webview-window");
  });

  it("2. 标题栏入口：图表按钮在设置按钮之前，点击 → 主窗导航且不建窗（Task 7）", async () => {
    localStorage.setItem("tauri-ui-theme", "dark");
    render(<MainTitleBar />);

    const openBtn = screen.getByRole("button", { name: i18n.t("usage.title") });
    const settingsBtn = screen.getByRole("button", { name: i18n.t("settings.button") });
    // 步骤 9：插在设置按钮**之前**
    expect(
      openBtn.compareDocumentPosition(settingsBtn) & Node.DOCUMENT_POSITION_FOLLOWING
    ).toBeTruthy();

    fireEvent.click(openBtn);
    // Task 7：唯一入口 = workspace_request_navigation（Rust 唤醒主窗 + 切模型用量页签）
    await waitFor(() => {
      const navCalls = tauriInvokeMock.mock.calls.filter(
        ([cmd]) => cmd === "workspace_request_navigation"
      );
      expect(navCalls).toHaveLength(1);
      expect((navCalls[0] as [string, Record<string, unknown>])[1]).toEqual({ tab: "usage" });
    });
    // 旧独立窗口彻底退役：零建窗
    expect(createWindowMock).not.toHaveBeenCalled();
  });

  it("3. 宠物菜单项：点「📊 用量看板」先关菜单再开看板（各一次）", () => {
    const onClose = vi.fn();
    const onOpenDashboard = vi.fn();
    render(
      <PetMenu
        onClose={onClose}
        onPreview={() => {}}
        onHide={() => {}}
        voiceCapable
        subtitleCapable
        onOpenDashboard={onOpenDashboard}
      />
    );

    const item = screen.getByTestId("pet-menu-dashboard");
    expect(item.textContent).toBe(i18n.t("usage.menu.dashboard"));

    // 步骤 10：插在主菜单**第一个分隔线之前**（第一个分隔线的下一条是「📏 大小」行）
    const sizeRow = screen.getByText(i18n.t("pet.menu.size"));
    expect(item.compareDocumentPosition(sizeRow) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();

    fireEvent.click(item);
    expect(onClose).toHaveBeenCalledTimes(1);
    expect(onOpenDashboard).toHaveBeenCalledTimes(1);
  });
});
