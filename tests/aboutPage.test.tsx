// tests/aboutPage.test.tsx — 关于页改版（2026-10-12，CC Switch 风格）
// 覆盖：
// T1 on mount 自动检查一次（不轮询）：有新版本 → 主色「更新到 vX.Y.Z」按钮 +
//    「检测到新版本」状态行；次按钮退场；弹窗不自动开（由主按钮显式打开）；检查恰好一次
// T1b 点击主按钮 → 打开既有 UpdaterDialog 走既有安装流程（manualCheck + autoOpen=false）
// T2 无新版本 → 维持「检查更新」次按钮 + upToDate 行内提示
// T2b mount 自动检查失败 → 静默不 toast（契约：网络失败静默）；弹窗不开、主按钮不出
// T2c 手动点「检查更新」失败 → toast 带具体原因（Rust message 不吞）
// T3 Star 横幅：默认可见；「去点 Star」openUrl 仓库；X 关闭写 localStorage
//    （键含当前版本号，升级后重新出现）；重挂载不再出现（关闭记忆生效）
// T3b localStorage 已有当前版本关闭态 → 横幅不出现
// T4 快捷行三枚按钮：官方仓库 / 更新日志 / 近期更新 → 对应 GitHub 地址
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { invoke } from "@tauri-apps/api/core";
import { toast } from "sonner";
import AboutPage from "@/pages/about";
import { useUpdaterStore } from "@/stores/updaterStore";
import type { UpdateCheckResult } from "@/lib/updater";

const { getVersionMock, openUrlMock } = vi.hoisted(() => ({
  getVersionMock: vi.fn(async () => "0.4.1"),
  openUrlMock: vi.fn(async () => undefined),
}));

vi.mock("@tauri-apps/api/app", () => ({ getVersion: getVersionMock }));
vi.mock("@tauri-apps/plugin-opener", () => ({ openUrl: openUrlMock }));
// toast 断言走 mock（真 sonner 无 Toaster 挂载时无法断言）；Toaster 本体替换为空
vi.mock("sonner", () => ({
  toast: { error: vi.fn(), info: vi.fn(), success: vi.fn() },
  Toaster: () => null,
}));
// WindowFrame / About 页直接牵连 webviewWindow API（jsdom 无 __TAURI_INTERNALS__）→ stub 窗口面
vi.mock("@tauri-apps/api/webviewWindow", () => ({
  getCurrentWebviewWindow: () => ({
    label: "about",
    isMaximized: async () => false,
    onResized: async () => () => {},
    onCloseRequested: async () => () => {},
    onFocusChanged: async () => () => {},
  }),
}));
vi.mock("@/components/common/title-bar", () => ({
  TitleBar: () => <div data-testid="titlebar" />,
}));
vi.mock("@/lib/window", () => ({
  destroyWindow: vi.fn(async () => undefined),
  cancelDestroyWindow: vi.fn(async () => undefined),
}));

// tests/setup.ts 未初始化 i18n，显式引入并固定中文（文案按 zh 断言）
import i18n from "@/i18n";

const AVAILABLE: Extract<UpdateCheckResult, { status: "available" }> = {
  status: "available",
  currentVersion: "0.4.1",
  update: {
    version: "0.5.0-beta.1",
    prerelease: true,
    notes: "## v0.5.0-beta.1 更新\n\n- **手机远程操控**：发消息与排队\n",
    htmlUrl: "https://github.com/jarvislee90s-dot/tuvis/releases/tag/v0.5.0-beta.1",
    publishedAt: "2026-09-23T15:18:19Z",
    tag: "v0.5.0-beta.1",
    latestJsonUrl:
      "https://github.com/jarvislee90s-dot/tuvis/releases/download/v0.5.0-beta.1/latest.json",
  },
};

/** 每用例可变的 mock 形态（模块级可变对象，beforeEach 必须复位防泄漏） */
const mode = { upToDate: false, checkError: null as string | null };

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

beforeEach(() => {
  mode.upToDate = false;
  mode.checkError = null;
  localStorage.clear();
  vi.mocked(toast.error).mockClear();
  openUrlMock.mockClear();
  useUpdaterStore.setState({
    result: null,
    checking: false,
    downloading: false,
    progress: null,
    dialogOpen: false,
    skippedVersion: null,
    skippedLoaded: false,
    autoCheckDone: false,
  });
  vi.mocked(invoke).mockImplementation((cmd: string) => {
    if (cmd === "check_for_github_update") {
      if (mode.checkError) {
        return Promise.resolve({ status: "error", message: mode.checkError });
      }
      return Promise.resolve(
        mode.upToDate
          ? {
              status: "up-to-date",
              currentVersion: "0.5.0-beta.1",
              latestVersion: "0.5.0-beta.1",
            }
          : AVAILABLE
      );
    }
    return Promise.resolve(undefined);
  });
});

const checkCalls = () =>
  vi.mocked(invoke).mock.calls.filter(([cmd]) => cmd === "check_for_github_update").length;

describe("关于页改版（自动检测 + 醒目更新入口 + 快捷行 + Star 横幅）", () => {
  it("T1 on mount 自动检查一次：有新版本 → 主色按钮 + 状态行；次按钮退场；弹窗不自动开", async () => {
    render(<AboutPage />);

    const primary = await screen.findByRole("button", { name: "更新到 v0.5.0-beta.1" });
    expect(primary).toBeInTheDocument();
    // 醒目主色（primary 实心），非 outline 次按钮
    expect(primary.className).toContain("bg-primary");
    expect(screen.getByText("检测到新版本：0.5.0-beta.1")).toBeInTheDocument();
    // 无新版本态的次按钮退场
    expect(screen.queryByRole("button", { name: "检查更新" })).not.toBeInTheDocument();
    // 自动检查恰好一次（ref 门栓兜 dev StrictMode 双挂载），不轮询
    expect(checkCalls()).toBe(1);
    // 弹窗不自动开——由主按钮显式打开
    expect(useUpdaterStore.getState().dialogOpen).toBe(false);
  });

  it("T1b 点击主按钮 → 打开既有 UpdaterDialog（安装流程按钮语义不变）", async () => {
    render(<AboutPage />);
    fireEvent.click(await screen.findByRole("button", { name: "更新到 v0.5.0-beta.1" }));

    expect(await screen.findByText("发现新版本")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "立即升级" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "忽略本版本" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "稍后" })).toBeInTheDocument();
  });

  it("T2 无新版本 → 维持「检查更新」次按钮 + upToDate 行内提示", async () => {
    mode.upToDate = true;
    render(<AboutPage />);

    expect(await screen.findByText("您正在使用最新版本！")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "检查更新" })).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /更新到 v/ })).not.toBeInTheDocument();
    expect(screen.queryByText(/检测到新版本/)).not.toBeInTheDocument();
  });

  it("T2b mount 自动检查失败 → 静默不 toast；弹窗不开、主按钮不出", async () => {
    mode.checkError = "GitHub API 请求失败: connection refused";
    render(<AboutPage />);

    // 自动路径（用户没点任何按钮）：结果落 store 即视为已跑完，全程不得有 toast
    await waitFor(() => expect(useUpdaterStore.getState().result?.status).toBe("error"));
    expect(checkCalls()).toBe(1);
    expect(toast.error).not.toHaveBeenCalled();
    expect(useUpdaterStore.getState().dialogOpen).toBe(false);
    expect(screen.queryByRole("button", { name: /更新到 v/ })).not.toBeInTheDocument();
    // 失败不占住按钮：checking 已回落，用户仍可自己再点一次
    expect(screen.getByRole("button", { name: "检查更新" })).not.toBeDisabled();
  });

  it("T2c 点「检查更新」失败 → toast 带具体原因（手动路径才提示）", async () => {
    mode.checkError = "GitHub API 请求失败: connection refused";
    render(<AboutPage />);

    // mount 那次自动检查静默落地后，手动再点一次
    await waitFor(() => expect(useUpdaterStore.getState().result?.status).toBe("error"));
    expect(toast.error).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole("button", { name: "检查更新" }));
    await waitFor(() => {
      expect(toast.error).toHaveBeenCalledWith(
        "检查更新失败：GitHub API 请求失败: connection refused"
      );
    });
    expect(checkCalls()).toBe(2);
  });

  it("T3 Star 横幅：默认可见；去点 Star openUrl 仓库；X 关闭写版本化 localStorage 且重挂载不再现", async () => {
    const { unmount } = render(<AboutPage />);
    expect(
      await screen.findByText("如果 兔维斯 帮到了你，请在 GitHub 点个 Star，这对我们意义重大")
    ).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: "去点 Star" }));
    expect(openUrlMock).toHaveBeenCalledWith("https://github.com/jarvislee90s-dot/tuvis");

    // X 关闭：横幅消失 + 关闭态写入 localStorage，键含当前 app 版本号（升级后重新出现）
    fireEvent.click(screen.getByRole("button", { name: "关闭横幅" }));
    await waitFor(() => expect(screen.queryByText(/点个 Star/)).not.toBeInTheDocument());
    expect(localStorage.getItem("tuvis.star-banner.dismissed.0.4.1")).toBe("1");

    // 重挂载：关闭记忆生效，横幅不再出现（等 mount 自动检查落地 = 版本必已就绪）
    unmount();
    render(<AboutPage />);
    await screen.findByRole("button", { name: "更新到 v0.5.0-beta.1" });
    expect(screen.queryByText(/点个 Star/)).not.toBeInTheDocument();
  });

  it("T3b localStorage 已有当前版本关闭态 → 横幅不出现", async () => {
    localStorage.setItem("tuvis.star-banner.dismissed.0.4.1", "1");
    render(<AboutPage />);

    await screen.findByRole("button", { name: "更新到 v0.5.0-beta.1" });
    expect(screen.queryByText(/点个 Star/)).not.toBeInTheDocument();
  });

  it("T4 快捷行三枚按钮：官方仓库 / 更新日志 / 近期更新 → 对应 GitHub 地址", async () => {
    render(<AboutPage />);

    expect(await screen.findByRole("button", { name: "官方仓库" })).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "官方仓库" }));
    fireEvent.click(screen.getByRole("button", { name: "更新日志" }));
    fireEvent.click(screen.getByRole("button", { name: "近期更新" }));
    expect(openUrlMock).toHaveBeenNthCalledWith(1, "https://github.com/jarvislee90s-dot/tuvis");
    expect(openUrlMock).toHaveBeenNthCalledWith(
      2,
      "https://github.com/jarvislee90s-dot/tuvis/releases"
    );
    expect(openUrlMock).toHaveBeenNthCalledWith(
      3,
      "https://github.com/jarvislee90s-dot/tuvis/releases/latest"
    );
  });
});
