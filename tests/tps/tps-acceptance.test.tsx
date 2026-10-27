// Task 9 · TPS 前端整链收口验收（2026-10-24 TPS 短时抽样计划，brief Step1）。
//
// 与 Task 8 套件（tests/tps/tps-card.test.tsx）的分工：那边是组件级逐行为锁；
// 本文件是**整链验收**——在 mocked IPC 上把「Token↔TPS 解耦 → 稀疏点/空窗 →
// 过期 → 停用 → 来源状态诚实 → 心跳租约」一次走通，并加验收独有的诚实面扫描：
// 渲染产物任何位置不得出现「服务器基准」类措辞（Spec §3「全部都是端侧体验……
// 不称服务器纯解码基准」）。
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import type { ReactNode } from "react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

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
import { UsageDashboardBody } from "@/components/usage/UsageDashboardBody";
import { TpsCard } from "@/components/usage/TpsCard";
import { tauriInvokeMock } from "../msw/tauriMocks";
import type {
  TpsMetrics,
  TpsPoint,
  TpsSelection,
  TpsSeriesKey,
  TpsSeriesSnapshot,
  TpsSnapshotResult,
  TpsSourceState,
  TpsViewRange,
} from "@/types/tps";

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

afterEach(() => {
  cleanup();
});

type InvokeCall = [string, Record<string, unknown> | undefined];

function callsOf(cmd: string): InvokeCall[] {
  return tauriInvokeMock.mock.calls.filter(([c]) => c === cmd) as InvokeCall[];
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

function newClient() {
  return new QueryClient({ defaultOptions: { queries: { retry: false } } });
}

function wrapper(client: QueryClient) {
  return ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={client}>{children}</QueryClientProvider>
  );
}

function renderPage() {
  // Task 7（2026-10-26 计划）：TPS 卡移出用量主体、常驻四页宿主的「输出速度」页；
  // Token/TPS 解耦判据要求两者**同时挂载**（真实宿主里两页体首访后都在场）
  render(
    <>
      <UsageDashboardBody active />
      <TpsCard selection={{}} />
    </>,
    { wrapper: wrapper(newClient()) }
  );
}

function renderCard(selection: TpsSelection = {}) {
  render(<TpsCard selection={selection} />, { wrapper: wrapper(newClient()) });
}

// ==== 夹具 ====

const MIN = 60_000;
const SERIES: TpsSeriesKey = {
  tool: "zcode",
  model: "glm-5.3-flash",
  channel: null,
  method: "stream-observed",
};

function metrics(over: Partial<TpsMetrics> = {}): TpsMetrics {
  return {
    modelMs: 45_200,
    toolMs: null,
    avgFirstTokenMs: 1_230,
    tps: 86.4,
    sampleCount: 12,
    ttftSampleCount: 9,
    candidateCount: 15,
    timedModelCount: 14,
    timedToolCount: 0,
    failedCount: 2,
    cancelledCount: 1,
    missingTimingCount: 0,
    latestEndedAt: null,
    ageMs: null,
    method: "stream-observed",
    outputScope: "providerReported",
    quality: "normal",
    ...over,
  };
}

function point(sampledAt: number, over: Partial<TpsPoint> = {}): TpsPoint {
  return {
    ...metrics(),
    sumOutputTokens: 3_888,
    sumGenerationMs: 45_000,
    ttftSumMs: 11_070,
    pointId: `p-${sampledAt}`,
    seriesKey: SERIES,
    sampledAt,
    windowMs: 120_000,
    runId: "run-acc",
    trigger: "scheduled",
    status: "value",
    cadenceMs: 60_000,
    sampledKeys: ["k1", "k2"],
    ...over,
  };
}

function seriesSnapshot(over: Partial<TpsSeriesSnapshot> = {}): TpsSeriesSnapshot {
  return {
    seriesKey: SERIES,
    lastCall: metrics({ latestEndedAt: Date.now() - 40_000, ageMs: 40_000 }),
    currentTurn: metrics({ tps: 72.5, sampleCount: 4, ttftSampleCount: 3 }),
    latestEndedAt: Date.now() - 40_000,
    ageMs: 40_000,
    nullReason: null,
    ...over,
  };
}

function sourceState(source: string, detail: string, ok = true): TpsSourceState {
  return { source, cursor: { source, value: "c" }, ok, detail };
}

function snapshotResult(
  series: TpsSeriesSnapshot[],
  sourceStates: TpsSourceState[] = []
): TpsSnapshotResult {
  return { disabled: false, snapshot: { series, sourceStates } };
}

function pointsResult(points: TpsPoint[], now: number): Record<string, unknown> {
  return { disabled: false, knownSeries: [SERIES], points, rangeSinceMs: now - 60 * MIN };
}

/** 验收 happy 路由：单 series 快照 + 近 1 小时 6 个实测点 + 1 个 gap（x 轴 = 实际采样时间） */
function overrideHappyTps() {
  const now = Date.now();
  return override({
    tps_snapshot: () =>
      Promise.resolve(
        snapshotResult(
          [seriesSnapshot()],
          [
            sourceState("zcode", "tps-status=active"),
            sourceState("dsh", "tps-status=active"),
            sourceState("kimi", "tps-status=active"),
            sourceState("opencode", "tps-status=unconfigured"),
            sourceState(
              "codex",
              "tps-status=insufficient-timing; insufficient-generation-timing: no reliable per-call generation window"
            ),
            sourceState("workbuddy", "tps-status=no-sample"),
            sourceState("claude", "tps-status=unconfigured"),
          ]
        )
      ),
    tps_points: (_cmd: string, args?: unknown) => {
      const viewRange = (args as { viewRange?: TpsViewRange } | undefined)?.viewRange ?? "1h";
      if (viewRange === "2m" || viewRange === "10m") {
        return Promise.resolve(pointsResult([point(now - 40_000)], now));
      }
      const p = (minAgo: number, over: Partial<TpsPoint> = {}) =>
        point(now - minAgo * MIN, { pointId: `p-${minAgo}`, ...over });
      return Promise.resolve(
        pointsResult(
          [
            p(55),
            p(44),
            p(33, {
              status: "gap",
              tps: null,
              sampleCount: 0,
              sumOutputTokens: 0,
              sumGenerationMs: 0,
              ttftSumMs: null,
              sampledKeys: [],
            }),
            p(22),
            p(11),
            p(6),
            p(1),
          ],
          now
        )
      );
    },
  });
}

// ==== 用例 ====

describe("TPS 前端整链验收（Task 9）", () => {
  let restore: (() => void) | null = null;
  afterEach(() => {
    restore?.();
    restore = null;
  });

  it("Token today→last30d 不重发 TPS 查询；TPS 1h→2m 不重发费用查询（双向解耦）", async () => {
    restore = overrideHappyTps();
    renderPage();
    await screen.findByTestId("tps-card");
    await waitFor(() => expect(callsOf("tps_points").length).toBeGreaterThan(0));

    const lastPreset = () => {
      const calls = callsOf("usage_costs");
      return (
        (calls[calls.length - 1]?.[1] as { range?: { preset?: string } } | undefined)?.range
          ?.preset ?? null
      );
    };
    const lastTpsRange = () => {
      const calls = callsOf("tps_points");
      return (
        (calls[calls.length - 1]?.[1] as { viewRange?: string } | undefined)?.viewRange ?? null
      );
    };

    fireEvent.click(screen.getByTestId("usage-range-today"));
    await waitFor(() => expect(lastPreset()).toBe("today"));
    const tpsCallsAtToday = callsOf("tps_points").length;
    fireEvent.click(screen.getByTestId("usage-range-last30d"));
    await waitFor(() => expect(lastPreset()).toBe("last30d"));
    // 切 Token 区间不得重发 TPS points 查询
    expect(callsOf("tps_points").length).toBe(tpsCallsAtToday);

    const costCalls = callsOf("usage_costs").length;
    fireEvent.click(screen.getByTestId("tps-range-2m"));
    await waitFor(() => expect(lastTpsRange()).toBe("2m"));
    // 切 TPS 区间不得重发费用查询
    expect(callsOf("usage_costs").length).toBe(costCalls);
    expect(screen.getByTestId("usage-cost-body")).toBeTruthy();
  });

  it("6 个稀疏实测点不插值补线；gap 不画 0 且有诚实说明", async () => {
    restore = overrideHappyTps();
    renderCard();
    await screen.findByTestId("tps-card");
    // 7 点 = 6 value + 1 gap：数据点恰 6 个（不插值补到任意密度），gap 不产生 0 值点
    const dots = await screen.findAllByTestId("tps-point-dot");
    expect(dots).toHaveLength(6);
    expect(screen.queryByTestId("tps-point-gap")).toBeNull();
    expect(screen.getByTestId("tps-gap-note")).toHaveTextContent("1");
    // 空窗措辞必须诚实（不画 0）
    expect(screen.getByTestId("tps-gap-note").textContent).toContain("不画 0");
  });

  it("最新读数超 10 分钟：当前值退位为「暂无近期样本」，n/方法/年龄仍可见", async () => {
    const now = Date.now();
    restore = override({
      tps_snapshot: () =>
        Promise.resolve(
          snapshotResult([
            seriesSnapshot({
              lastCall: null,
              currentTurn: null,
              latestEndedAt: now - 15 * MIN,
              ageMs: 15 * MIN,
              nullReason: "stale",
            }),
          ])
        ),
      tps_points: () =>
        Promise.resolve(
          pointsResult(
            [point(now - 15 * MIN, { sampleCount: 5, method: "client-native", tps: 42 })],
            now
          )
        ),
    });
    renderCard();
    await screen.findByTestId("tps-card");
    expect(await screen.findByTestId("tps-stale")).toHaveTextContent("暂无近期样本");
    expect(screen.queryByTestId("tps-current-tps")).toBeNull();
    expect(screen.getByTestId("tps-last-n")).toHaveTextContent("5");
    expect(screen.getByTestId("tps-method")).toHaveTextContent("客户端原生");
    expect(screen.getByTestId("tps-age")).toHaveTextContent("15 分钟前");
  });

  it("总授权关闭：按 snapshot.disabled 渲染停用态，历史语义不变", async () => {
    restore = override({
      tps_snapshot: () => Promise.resolve({ disabled: true, snapshot: null }),
      tps_points: () =>
        Promise.resolve({ disabled: true, knownSeries: [], points: [], rangeSinceMs: 0 }),
    });
    renderCard();
    await screen.findByTestId("tps-card");
    expect(await screen.findByTestId("tps-disabled")).toBeTruthy();
    expect(screen.queryByTestId("tps-current-tps")).toBeNull();
    expect(screen.queryByTestId("tps-chart")).toBeNull();
  });

  it("来源状态诚实：Codex 缺口单列不产 TPS、安装引导在位、全文无「服务器基准」", async () => {
    restore = overrideHappyTps();
    renderCard();
    await screen.findByTestId("tps-card");
    // Codex：如实缺口说明，不给任何 Codex TPS 值
    const codex = await screen.findByTestId("tps-source-codex");
    expect(codex.textContent).toContain("本版不提供TPS");
    // Claude / OpenCode：一次点击的安装入口在位（原生只读来源没有安装按钮）
    expect(screen.getByTestId("tps-install-claude")).toBeTruthy();
    expect(screen.getByTestId("tps-install-opencode")).toBeTruthy();
    expect(screen.queryByTestId("tps-install-zcode")).toBeNull();
    // 副标题声明「不发送测速请求」
    expect(screen.getByTestId("tps-card").textContent).toContain("不发送测速请求");
    // 验收独有扫面：渲染产物任何位置不得出现「服务器基准」类措辞
    expect(document.body.textContent).not.toContain("服务器基准");
    expect(document.body.textContent).not.toContain("服务器纯解码");
  });

  it("心跳随可见性起停：挂载即续租、隐藏立即释放并停轮询、卸载兜底再释放", async () => {
    vi.useFakeTimers();
    try {
      restore = overrideHappyTps();
      const { unmount } = render(<TpsCard selection={{}} />, { wrapper: wrapper(newClient()) });
      const hb = () => callsOf("tps_view_heartbeat");
      const snapCount = () => callsOf("tps_snapshot").length;

      await act(async () => {
        await vi.advanceTimersByTimeAsync(0);
      });
      expect(hb().length).toBeGreaterThan(0);
      expect(hb()[0]![1]).toMatchObject({ visible: true });
      const c0 = snapCount();
      await act(async () => {
        await vi.advanceTimersByTimeAsync(5_100);
      });
      expect(snapCount()).toBeGreaterThan(c0);

      // 隐藏：立即 visible=false + 前端轮询停止
      const s1 = snapCount();
      Object.defineProperty(document, "visibilityState", {
        configurable: true,
        get: () => "hidden",
      });
      await act(async () => {
        document.dispatchEvent(new Event("visibilitychange"));
        await vi.advanceTimersByTimeAsync(0);
      });
      expect(hb()[hb().length - 1]![1]).toMatchObject({ visible: false });
      const h1 = hb().length;
      await act(async () => {
        await vi.advanceTimersByTimeAsync(20_000);
      });
      expect(snapCount()).toBe(s1);
      expect(hb().length).toBe(h1);

      // 卸载：兜底再释放一次
      unmount();
      expect(hb().length).toBeGreaterThan(h1);
      expect(hb()[hb().length - 1]![1]).toMatchObject({ visible: false });
    } finally {
      delete (document as unknown as { visibilityState?: string }).visibilityState;
      vi.useRealTimers();
    }
  });
});
