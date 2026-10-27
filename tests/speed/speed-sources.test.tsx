// Task 11（2026-10-26 计划）· 二级采集管理面板 SpeedSourcesPanel（vitest + jsdom）。
// 判据来源：task-11-brief Step1（source_actions_follow_install_fact）+ Spec §8.6/§8.7
// （来源一级只显示名称、接入状态、指标能力与动作；已安装事实决定卸载入口；接入引导 =
// 预览 → 原生信任说明（写哪个原生设置文件）→ 应用 → 下次启动生效；详情才显示技术诊断；
// 无区间记录是数据空态，不自动等于采集故障）。
//
// 覆盖：
//  1. 状态矩阵：ready/unconfigured/await-restart/needs-check/unsupported 五态标签；
//     Codex「已接入 · 仅响应」——响应能力可用、流缺口走 translateSpeedReason 的短原因
//     （不裸印机器 token）、无接入按钮（brief 断言逐字）；
//  2. 卸载/重装跟随 installed 事实：卸载后行保留历史记录时间并给出重装（接入）入口
//     （brief 断言逐字）；空数据（lastRecordAt=null）无错误样式；
//  3. 接入流：预览（含脱敏 chip）→ 取消不动任何事实 → 应用 → 回执 + 下次启动生效提示；
//  4. 错误路径：apply 拒绝 ⇒ 内联可重试错误、已加载来源不清空；stale/conflict 走既有文案；
//     回执敏感字段保持脱敏（不回显原文指纹）；
//  5. needs-check 详情：一级只给短原因，raw detail 只在展开的 details 区；其他来源不受影响；
//  6. 设置集成：面板 open 才取 sourceStates 且**单次取数**（不开 5 秒轮询、不申请视图租约），
//     手动刷新恰好再取一次，关闭后零请求，重开再取一次（面板自身无数据 pin）。
//
// 纪律：真实 hook 路径——invoke 走 tests/setup.ts 接线的 tauriInvokeMock；安装/卸载算法只经
// 共享的 TpsInstallFlow（不复制预览/应用/脱敏逻辑）；不 mock react-query。
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";

// jsdom 未实现 matchMedia（与其余 usage/settings 测试同模式）
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

vi.mock("@tauri-apps/api/webviewWindow", () => ({
  getCurrentWebviewWindow: () => ({
    isMaximized: vi.fn(async () => false),
    onResized: vi.fn(async () => () => {}),
    minimize: vi.fn(),
    toggleMaximize: vi.fn(),
    close: vi.fn(),
  }),
}));

import i18n from "@/i18n";
import { SpeedSourcesPanel } from "@/components/speed/SpeedSourcesPanel";
import { UsageSection } from "@/components/settings/UsageSection";
import { mockSpeedDashboard, resetTpsMockState } from "@/lib/usage/mockFixtures";
import { tauriInvokeMock } from "../msw/tauriMocks";
import type { SpeedDashboardSnapshot, SpeedSourceView } from "@/types/speed";

// ==== 基建 ====

const BASE_NOW = Date.parse("2026-10-26T12:00:00+08:00");

function makeClient() {
  return new QueryClient({ defaultOptions: { queries: { retry: false } } });
}

function wrap(children: ReactNode, client = makeClient()) {
  return <QueryClientProvider client={client}>{children}</QueryClientProvider>;
}

type InvokeCall = [string, Record<string, unknown> | undefined];

function callsOf(cmd: string): InvokeCall[] {
  return tauriInvokeMock.mock.calls.filter(([c]) => c === cmd) as InvokeCall[];
}

function tpsCalls(): number {
  return tauriInvokeMock.mock.calls.filter(([c]) => c.startsWith("tps_")).length;
}

type Impl = (cmd: string, args?: unknown) => Promise<unknown>;

/** 在既有 mock 路由上覆写指定命令；返回恢复函数 */
function override(overrides: Record<string, Impl>) {
  const original = tauriInvokeMock.getMockImplementation() as
    ((cmd: string, args?: unknown) => Promise<unknown>) | undefined;
  tauriInvokeMock.mockImplementation((cmd: string, args?: unknown) =>
    overrides[cmd] ? overrides[cmd](cmd, args) : (original?.(cmd, args) ?? Promise.resolve())
  );
  return () => {
    if (original) tauriInvokeMock.mockImplementation(original);
  };
}

/** 单来源视图构造（缺省 = 双能力、已装、40 秒前有记录的 ready 态）。 */
function srcView(patch: Partial<SpeedSourceView> & { source: string }): SpeedSourceView {
  return {
    status: "ready",
    installed: true,
    capabilities: { stream: true, response: true, streamReason: null, responseReason: null },
    lastRecordAt: BASE_NOW - 40_000,
    reason: null,
    detail: null,
    ...patch,
  };
}

const CODEX_STREAM_GAP = "stream-gap:insufficient-generation-timing";

/** 状态矩阵夹具：五态各一 + Codex 仅响应（与 mockSpeedSourceStates 的折算矩阵同构）。 */
function matrixSources(): SpeedSourceView[] {
  return [
    srcView({ source: "zcode", status: "ready", lastRecordAt: BASE_NOW - 40_000 }),
    srcView({
      source: "claude",
      status: "unconfigured",
      installed: false,
      lastRecordAt: null,
      reason: "not-installed",
    }),
    srcView({
      source: "opencode",
      status: "await-restart",
      lastRecordAt: null,
      reason: "await-restart",
    }),
    srcView({
      source: "kimi",
      status: "needs-check",
      reason: "parser-errors",
      detail: "parser-errors=3; unparseable assistant blocks in batch",
    }),
    srcView({
      source: "codex",
      status: "ready",
      lastRecordAt: BASE_NOW - 70_000,
      capabilities: {
        stream: false,
        response: true,
        streamReason: CODEX_STREAM_GAP,
        responseReason: null,
      },
    }),
  ];
}

function renderPanel(sources: SpeedSourceView[], onOpenChange: (open: boolean) => void = () => {}) {
  return render(wrap(<SpeedSourcesPanel open onOpenChange={onOpenChange} sources={sources} />));
}

beforeEach(async () => {
  tauriInvokeMock.mockClear();
  resetTpsMockState();
  await i18n.changeLanguage("zh");
});

// ==== 1. 状态矩阵 + Codex 仅响应（brief Step1 判据主体）====

describe("source_actions_follow_install_fact · 状态矩阵", () => {
  it("五态标签按真实状态显示；Codex=已接入·仅响应，缺口走短原因，无接入按钮", () => {
    renderPanel([
      ...matrixSources(),
      // unsupported 态（原生只读来源的结构形态之一，补齐五态标签覆盖）
      srcView({
        source: "openclaw",
        status: "unsupported",
        capabilities: { stream: false, response: false, streamReason: null, responseReason: null },
      }),
    ]);

    expect(screen.getByTestId("speed-status-zcode")).toHaveTextContent("已接入");
    expect(screen.getByTestId("speed-status-claude")).toHaveTextContent("待配置");
    expect(screen.getByTestId("speed-status-opencode")).toHaveTextContent("待重启");
    expect(screen.getByTestId("speed-status-kimi")).toHaveTextContent("待检查");
    expect(screen.getByTestId("speed-status-openclaw")).toHaveTextContent("不支持");

    // Codex：整体状态不被降级为不支持——「已接入 · 仅响应」
    expect(screen.getByTestId("speed-status-codex")).toHaveTextContent("已接入");
    expect(screen.getByTestId("speed-status-codex")).toHaveTextContent("仅响应");
    // 响应能力可用照常展示；流缺口给 translateSpeedReason 的短原因（不是机器 token）
    expect(screen.getByTestId("speed-cap-response-codex")).toHaveTextContent("响应");
    expect(screen.getByTestId("speed-cap-stream-codex")).toHaveTextContent(
      "缺少同调用流式生成窗（响应口径为日志近似）"
    );
    expect(screen.queryByText(CODEX_STREAM_GAP)).not.toBeInTheDocument();
    expect(document.body.textContent).not.toContain(CODEX_STREAM_GAP);
    // 一级行不堆 Windows/诊断长文（raw detail 只进 details）
    expect(screen.queryByText(/parser-errors=/)).not.toBeInTheDocument();
    // Codex 无接入按钮（brief 断言逐字）
    expect(screen.queryByTestId("speed-install-codex")).not.toBeInTheDocument();
    // 未安装事实给出接入入口；已装的接入族来源给出卸载入口（installed 事实决定）
    expect(screen.getByTestId("speed-install-claude")).toBeInTheDocument();
    expect(screen.queryByTestId("speed-uninstall-claude")).not.toBeInTheDocument(); // 未安装无卸载
    expect(screen.getByTestId("speed-uninstall-opencode")).toBeInTheDocument();
    // 原生只读来源（无安装机制）不给无意义的卸载入口
    expect(screen.queryByTestId("speed-uninstall-kimi")).not.toBeInTheDocument();
    expect(screen.queryByTestId("speed-uninstall-zcode")).not.toBeInTheDocument();
  });

  it("卸载跟随 installed 事实：卸载后行保留历史并出现重装（接入）入口", async () => {
    const installed = srcView({ source: "claude", lastRecordAt: BASE_NOW - 5 * 60_000 });
    const { rerender } = renderPanel([installed]);
    expect(screen.getByTestId("speed-uninstall-claude")).toBeVisible();

    fireEvent.click(screen.getByTestId("speed-uninstall-claude"));
    await waitFor(() => expect(callsOf("tps_uninstall")).toHaveLength(1));
    // 卸载成功提示（共享流的原有文案）+ ["tps"] 前缀失效由共享流承担
    await screen.findByRole("status");

    // 宿主（查询）按新安装事实重算：卸载后 installed=false，但历史（lastRecordAt）仍在
    const uninstalled = srcView({
      source: "claude",
      installed: false,
      status: "unconfigured",
      reason: "not-installed",
      lastRecordAt: BASE_NOW - 5 * 60_000, // 卸载后保留的 24h 历史
    });
    rerender(wrap(<SpeedSourcesPanel open onOpenChange={() => {}} sources={[uninstalled]} />));
    expect(screen.getByTestId("speed-install-claude")).toBeVisible(); // 卸载后仍有历史
    // 历史记录时间照常渲染（不是空态「—」），且行不带错误样式（数据空态≠采集故障）
    const record = screen.getByTestId("speed-last-record-claude");
    expect(record).toHaveTextContent("最近记录");
    expect(record.textContent).not.toContain("—");
    expect(screen.getByTestId("speed-source-claude").className).not.toContain("text-red-500");
  });

  it("空数据 ≠ 故障：ready 且 lastRecordAt=null 的行无错误样式、记录位给中性占位", () => {
    renderPanel([srcView({ source: "zcode", lastRecordAt: null })]);
    expect(screen.getByTestId("speed-status-zcode")).toHaveTextContent("已接入");
    expect(screen.getByTestId("speed-last-record-zcode")).toHaveTextContent("—");
    expect(screen.getByTestId("speed-source-zcode").className).not.toContain("text-red-500");
  });
});

// ==== 2. 接入流（复用既有 preview→apply 流；取消不动任何事实）====

describe("接入流", () => {
  it("预览（原生设置路径+脱敏）→ 取消不动事实 → 应用 → 回执与下次启动生效", async () => {
    renderPanel(matrixSources());

    fireEvent.click(screen.getByTestId("speed-install-claude"));
    const preview = await screen.findByTestId("tps-preview");
    // 原生信任说明：写的是哪个原生设置文件，逐字可见
    expect(preview).toHaveTextContent("/home/mock/.claude/settings.json");
    // 敏感统计 header 在预览里只给脱敏 chip
    expect(within(preview).getAllByTestId("tps-masked-chip").length).toBeGreaterThanOrEqual(1);
    expect(callsOf("tps_install_preview")).toHaveLength(1);

    // 取消：不落任何配置事实
    fireEvent.click(within(preview).getByText("取消"));
    expect(screen.queryByTestId("tps-preview")).not.toBeInTheDocument();
    expect(callsOf("tps_install_apply")).toHaveLength(0);

    // 再走一次：应用 → 回执 + 待重启说明
    fireEvent.click(screen.getByTestId("speed-install-claude"));
    fireEvent.click(await screen.findByTestId("tps-apply"));
    const receipt = await screen.findByTestId("tps-receipt");
    expect(callsOf("tps_install_apply")).toHaveLength(1);
    expect(callsOf("tps_install_apply")[0][1]?.previewId).toBe("mock-preview-claude");
    expect(within(receipt).getByTestId("tps-restart-hint")).toHaveTextContent("下次启动生效");
    expect(receipt).toHaveTextContent("接收器端口 47181");
  });
});

// ==== 3. 错误路径（可重试、不清数据、既有冲突文案、脱敏保留）====

describe("错误路径", () => {
  it("apply 拒绝 ⇒ 内联可重试错误，已加载来源不清空", async () => {
    const restore = override({
      tps_install_apply: () =>
        Promise.reject({
          code: "usage-settings-invalid",
          detail: "settings changed since preview",
          reason: "stale-preview",
        }),
    });
    renderPanel(matrixSources());

    fireEvent.click(screen.getByTestId("speed-install-claude"));
    fireEvent.click(await screen.findByTestId("tps-apply"));
    // 既有 stale-preview 文案（共享流的 reason 表）
    expect(await screen.findByRole("status")).toHaveTextContent("预览后设置已变化，请重新预览");
    // 已加载的来源列表不被清空，接入按钮恢复可点（可重试）
    expect(screen.getByTestId("speed-source-zcode")).toBeInTheDocument();
    expect(screen.getByTestId("speed-source-codex")).toBeInTheDocument();
    expect(screen.getByTestId("speed-install-claude")).not.toBeDisabled();
    restore();
  });

  it("冲突 reason 走既有文案；回执敏感字段保持脱敏（不回显原文指纹）", async () => {
    const restore = override({
      tps_install_apply: () =>
        Promise.reject({
          code: "usage-settings-invalid",
          detail: "conflict",
          reason: "conflict",
        }),
    });
    const first = renderPanel(matrixSources());
    fireEvent.click(screen.getByTestId("speed-install-claude"));
    fireEvent.click(await screen.findByTestId("tps-apply"));
    expect(await screen.findByRole("status")).toHaveTextContent(
      "已有相同信号目标，默认保留；可在预览中选择转交"
    );
    first.unmount();
    restore();

    // 成功路径的回执：敏感 header 仍是脱敏 chip，原文/指纹不出现在 DOM
    renderPanel(matrixSources());
    fireEvent.click(screen.getByTestId("speed-install-claude"));
    fireEvent.click(await screen.findByTestId("tps-apply"));
    const receipt = await screen.findByTestId("tps-receipt");
    expect(within(receipt).getAllByTestId("tps-masked-chip").length).toBeGreaterThanOrEqual(1);
    expect(screen.queryByText(/ab12cd34ef56ab78/)).not.toBeInTheDocument();
    expect(screen.queryByText(/1122334455667788/)).not.toBeInTheDocument();
  });
});

// ==== 4. 详情区（技术说明只进 details）====

describe("详情区", () => {
  it("needs-check 一级只给短原因；raw 诊断只在展开的 details 区；其他来源不受影响", async () => {
    renderPanel(matrixSources());

    // 展开前：一级只有短原因（有记录解析失败），机器前缀的原文不出现
    expect(screen.getByText("有记录解析失败")).toBeInTheDocument();
    expect(screen.queryByText(/parser-errors=3/)).not.toBeInTheDocument();

    fireEvent.click(screen.getByTestId("speed-details-toggle-kimi"));
    const details = await screen.findByTestId("speed-details-kimi");
    expect(details).toHaveTextContent("parser-errors=3; unparseable assistant blocks in batch");

    // 其他来源不受影响：zcode 仍是已接入、且没有意外展开的详情区
    expect(screen.getByTestId("speed-status-zcode")).toHaveTextContent("已接入");
    expect(screen.queryByTestId("speed-details-zcode")).not.toBeInTheDocument();
  });
});

// ==== 5. 设置集成（open 门控、单次取数、无泄漏轮询）====

describe("设置集成", () => {
  it("面板 open 才取 sourceStates 且单次取数；手动刷新恰再取一次；关闭零请求；重开再取", async () => {
    vi.useFakeTimers();
    render(wrap(<UsageSection />));
    // 设置表单就绪（usage 域查询，与 tps 域无关）
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    expect(tpsCalls()).toBe(0); // 面板未开：零 tps_* 请求（不申请视图租约、不取快照）

    // 打开面板：恰好一次 dashboard 取数，sourceStates 渲染成行
    fireEvent.click(screen.getByTestId("usage-sources-open"));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(100);
    });
    expect(screen.getByTestId("speed-sources-panel")).toBeTruthy();
    expect(screen.getByTestId("speed-source-zcode")).toBeTruthy();
    expect(callsOf("tps_speed_dashboard")).toHaveLength(1);
    expect(tpsCalls()).toBe(1);

    // 面板开着不动：12 秒零新增（无 5 秒轮询、无心跳）
    await act(async () => {
      await vi.advanceTimersByTimeAsync(12_000);
    });
    expect(tpsCalls()).toBe(1);

    // 手动刷新：恰好再取一次
    fireEvent.click(screen.getByTestId("speed-sources-refresh"));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(100);
    });
    expect(callsOf("tps_speed_dashboard")).toHaveLength(2);

    // 关闭：零请求；面板卸载
    fireEvent.click(document.querySelector('[data-slot="dialog-close"]')!);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1_000); // 排空 Radix Presence 关闭动画
    });
    expect(screen.queryByTestId("speed-sources-panel")).toBeNull();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(12_000);
    });
    expect(tpsCalls()).toBe(2);

    // 重开：面板自身无数据 pin，重新取一次
    fireEvent.click(screen.getByTestId("usage-sources-open"));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(100);
    });
    expect(callsOf("tps_speed_dashboard")).toHaveLength(3);
  });

  it("取数失败 ⇒ 内联错误而非「暂无来源」，错误也单次即停；刷新恰重试一次并可恢复", async () => {
    vi.useFakeTimers();
    let fail = true;
    const restore = override({
      tps_speed_dashboard: () =>
        fail
          ? Promise.reject({ code: "usage-settings-invalid", detail: "tps dashboard failed" })
          : Promise.resolve(mockSpeedDashboard()),
    });
    render(wrap(<UsageSection />));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });

    // 打开面板：一次取数即失败
    fireEvent.click(screen.getByTestId("usage-sources-open"));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(100);
    });
    expect(callsOf("tps_speed_dashboard")).toHaveLength(1);
    // 故障不冒充空态：内联错误可见，中性空态文案不在场（Spec §8 尾的反面）
    expect(screen.getByTestId("speed-sources-error")).toHaveTextContent(
      "来源状态取数失败，请点「刷新」重试。"
    );
    expect(screen.queryByText("暂无来源")).not.toBeInTheDocument();
    expect(screen.queryByTestId("speed-sources-loading")).not.toBeInTheDocument();

    // 错误也单次即停：12 秒零新增（失败分支不得退化成 5 秒轮询）
    await act(async () => {
      await vi.advanceTimersByTimeAsync(12_000);
    });
    expect(callsOf("tps_speed_dashboard")).toHaveLength(1);
    expect(tpsCalls()).toBe(1);

    // 「刷新」= 手动重试：恰好一次，成功后错误消失、列表恢复（仍无自动轮询）
    fail = false;
    fireEvent.click(screen.getByTestId("speed-sources-refresh"));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(100);
    });
    expect(callsOf("tps_speed_dashboard")).toHaveLength(2);
    expect(screen.queryByTestId("speed-sources-error")).not.toBeInTheDocument();
    expect(screen.getByTestId("speed-source-zcode")).toBeTruthy();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(12_000);
    });
    expect(callsOf("tps_speed_dashboard")).toHaveLength(2);
    restore();
  });

  it("首次加载渲染占位而非「暂无来源」；加载完成切换为来源列表", async () => {
    vi.useFakeTimers();
    let resolveFetch!: (v: SpeedDashboardSnapshot) => void;
    const restore = override({
      tps_speed_dashboard: () =>
        new Promise<SpeedDashboardSnapshot>((resolve) => {
          resolveFetch = resolve;
        }),
    });
    render(wrap(<UsageSection />));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(0);
    });
    fireEvent.click(screen.getByTestId("usage-sources-open"));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(50);
    });
    // 首次加载中：占位在场，空态文案不在场
    expect(screen.getByTestId("speed-sources-loading")).toHaveTextContent("加载中…");
    expect(screen.queryByText("暂无来源")).not.toBeInTheDocument();
    expect(screen.queryByTestId("speed-source-zcode")).not.toBeInTheDocument();

    await act(async () => {
      resolveFetch(mockSpeedDashboard());
      await vi.advanceTimersByTimeAsync(50);
    });
    expect(screen.getByTestId("speed-source-zcode")).toBeTruthy();
    expect(screen.queryByTestId("speed-sources-loading")).not.toBeInTheDocument();
    restore();
  });
});
