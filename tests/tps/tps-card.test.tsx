// Task 8 · 独立 TPS 区块与曲线（2026-10-24 TPS 短时抽样计划，brief Step1 红测试）。
//
// 被测对象：src/components/usage/TpsCard.tsx（独立「近期TPS」区块）与
// src/lib/query/queries/tps.ts（可见性驱动的轮询 + view_id 心跳租约）。
//
// 覆盖面（brief + 任务书）：
//  1. Token today→30d 不改 TPS 区间；TPS 2m→1h 不触发费用查询（§7 解耦）；
//  2. 6 个稀疏点只画 6 个实测点（不插值补到 60），gap 不画 0（§2 断线纪律）；
//  3. 过期 snapshot（>10 分钟）不显示当前值，n/方法/age 仍可见（§2）；
//  4. 心跳：窗口可见即租约 visible=true（30 秒续约），隐藏/卸载立即 visible=false，
//     隐藏后 snapshot/points 前端轮询停止（§2 租约纪律）；
//  5. IntersectionObserver：区块滚出视口同样释放租约；
//  6. disabled（总授权关闭）按 snapshot.disabled 渲染，不靠 refresh reason；
//  7. Codex 来源按 tps-status= 前缀给「本版不提供TPS」缺口说明，不造假 TPS；
//  8. 安装引导 = 一次点击 + 「下次启动生效」提示；脱敏标记统一渲染为脱敏 chip；
//  9. TPS 性能错误只替换本区块，不遮盖费用卡（§7）。
//
// 三条字面 expect（brief Step1 原文）在用例 1 与用例 2 中逐字出现。
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
  const client = newClient();
  render(
    <>
      <UsageDashboardBody active />
      <TpsCard selection={{}} />
    </>,
    { wrapper: wrapper(client) }
  );
}

function renderCard(selection: TpsSelection = {}) {
  const client = newClient();
  const ui = render(<TpsCard selection={selection} />, { wrapper: wrapper(client) });
  return { client, ...ui };
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
    toolMs: 8_100,
    avgFirstTokenMs: 1_230,
    tps: 86.4,
    sampleCount: 12,
    ttftSampleCount: 9,
    candidateCount: 15,
    timedModelCount: 14,
    timedToolCount: 6,
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
    runId: "run-1",
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

/** 默认 happy 路由：单 series 快照 + 近 1 小时 6 个稀疏点（中间含 1 个 gap） */
function overrideHappyTps() {
  return override({
    tps_snapshot: () => Promise.resolve(snapshotResult([seriesSnapshot()])),
    tps_points: (_cmd: string, args?: unknown) => {
      const viewRange = (args as { viewRange?: TpsViewRange } | undefined)?.viewRange ?? "1h";
      const now = Date.now();
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

describe("TPS 独立区块（Task 8）", () => {
  let restore: (() => void) | null = null;
  afterEach(() => {
    restore?.();
    restore = null;
  });

  it("Token today→30d 不改 TPS 区间；TPS 2m→1h 不触发费用查询（三条字面 expect 之一、之二）", async () => {
    restore = overrideHappyTps();
    renderPage();
    await screen.findByTestId("tps-card");
    await waitFor(() => expect(callsOf("tps_points").length).toBeGreaterThan(0));

    const lastPreset = () => {
      const calls = callsOf("usage_costs");
      const args = calls[calls.length - 1]?.[1] as { range?: { preset?: string } } | undefined;
      return args?.range?.preset ?? null;
    };
    const lastTpsRange = () => {
      const calls = callsOf("tps_points");
      const args = calls[calls.length - 1]?.[1] as { viewRange?: string } | undefined;
      return args?.viewRange ?? null;
    };

    // Token：today → last30d
    fireEvent.click(screen.getByTestId("usage-range-today"));
    await waitFor(() => expect(lastPreset()).toBe("today"));
    const tpsCallsAtToday = callsOf("tps_points").length;
    fireEvent.click(screen.getByTestId("usage-range-last30d"));
    await waitFor(() => expect(lastPreset()).toBe("last30d"));
    await waitFor(() => expect(screen.getByTestId("usage-cost-body")).toBeTruthy());
    // 切 Token 范围不重发 TPS points 查询（queryKey 不含 Token range）
    expect(callsOf("tps_points").length).toBe(tpsCallsAtToday);

    const tokenRange = lastPreset();
    const tpsRange = lastTpsRange();
    expect(tokenRange).toBe("last30d");
    expect(tpsRange).toBe("1h");

    // TPS：1h → 2m → 1h，费用查询纹丝不动
    const costCalls = callsOf("usage_costs").length;
    fireEvent.click(screen.getByTestId("tps-range-2m"));
    await waitFor(() => expect(lastTpsRange()).toBe("2m"));
    fireEvent.click(screen.getByTestId("tps-range-1h"));
    await waitFor(() => expect(lastTpsRange()).toBe("1h"));
    expect(callsOf("usage_costs").length).toBe(costCalls);
    // 性能面改动不遮盖费用卡
    expect(screen.getByTestId("usage-cost-body")).toBeTruthy();
  });

  it("6 个稀疏点只画 6 个实测点，gap 不画 0（三条字面 expect 之三）", async () => {
    restore = overrideHappyTps();
    renderCard();
    await screen.findByTestId("tps-card");
    // 7 个点里 6 个 value + 1 个 gap：实测点恰 6 个，不插值补点、gap 不落 0
    const renderedMeasuredPoints = await screen.findAllByTestId("tps-point-dot");
    expect(renderedMeasuredPoints).toHaveLength(6);
    // gap 点不产生任何数据点（没有 0 值圆点）
    expect(screen.queryByTestId("tps-point-gap")).toBeNull();
    // gap 有诚实说明
    expect(screen.getByTestId("tps-gap-note")).toHaveTextContent("1");
  });

  it("过期 snapshot（>10 分钟）：不显示当前值，n/方法/age 仍可见", async () => {
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
    // 当前值不显示
    expect(await screen.findByTestId("tps-stale")).toHaveTextContent("暂无近期样本");
    expect(screen.queryByTestId("tps-current-tps")).toBeNull();
    // n / 方法 / age 可见（来自范围内最近点与 series 年龄）
    expect(screen.getByTestId("tps-last-n")).toHaveTextContent("5");
    expect(screen.getByTestId("tps-method")).toHaveTextContent("客户端原生");
    expect(screen.getByTestId("tps-age")).toHaveTextContent("15 分钟前");
  });

  it("心跳：可见即 visible=true 且 5 秒轮询；隐藏立即 visible=false 并停轮询；卸载再释放一次", async () => {
    vi.useFakeTimers();
    try {
      restore = overrideHappyTps();
      const { unmount } = renderCard();
      const hb = () => callsOf("tps_view_heartbeat");
      const snapCount = () => callsOf("tps_snapshot").length;

      await act(async () => {
        await vi.advanceTimersByTimeAsync(0);
      });
      // 挂载即续租
      expect(hb().length).toBeGreaterThan(0);
      expect(hb()[0]![1]).toMatchObject({ visible: true });
      // 5 秒一轮 snapshot
      const c0 = snapCount();
      await act(async () => {
        await vi.advanceTimersByTimeAsync(5_100);
      });
      expect(snapCount()).toBeGreaterThan(c0);

      // 隐藏：立即释放 + 停止前端轮询
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
      // 隐藏期间不再续租（30 秒间隔不触发 true）
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

  it("IntersectionObserver：区块滚出视口同样释放租约，滚回续租", async () => {
    class IOStub {
      static latest: IOStub | null = null;
      private cb: IntersectionObserverCallback;
      constructor(cb: IntersectionObserverCallback) {
        this.cb = cb;
        IOStub.latest = this;
      }
      observe() {
        // 真实 IntersectionObserver 在 observe 后总会立即回调一次初始相交态
        this.trigger(true);
      }
      unobserve() {}
      disconnect() {}
      takeRecords(): IntersectionObserverEntry[] {
        return [];
      }
      trigger(isIntersecting: boolean) {
        this.cb(
          [{ isIntersecting } as IntersectionObserverEntry],
          this as unknown as IntersectionObserver
        );
      }
    }
    vi.stubGlobal("IntersectionObserver", IOStub as unknown as typeof IntersectionObserver);
    try {
      restore = overrideHappyTps();
      renderCard();
      await screen.findByTestId("tps-card");
      const hb = () => callsOf("tps_view_heartbeat");
      await waitFor(() => expect(hb().length).toBeGreaterThan(0));
      expect(hb()[hb().length - 1]![1]).toMatchObject({ visible: true });

      act(() => {
        IOStub.latest!.trigger(false);
      });
      await waitFor(() => expect(hb()[hb().length - 1]![1]).toMatchObject({ visible: false }));
      act(() => {
        IOStub.latest!.trigger(true);
      });
      await waitFor(() => expect(hb()[hb().length - 1]![1]).toMatchObject({ visible: true }));
    } finally {
      vi.unstubAllGlobals();
    }
  });

  it("view_id 跨实例（模拟两窗口）互不相同；同实例重渲染不变", async () => {
    restore = overrideHappyTps();
    const viewIds = () =>
      callsOf("tps_view_heartbeat").map(([, a]) => (a as { viewId?: string } | undefined)?.viewId);

    // 实例 1：挂载即续租，当前全部心跳同一 view_id
    const ui1 = render(<TpsCard selection={{}} />, { wrapper: wrapper(newClient()) });
    await waitFor(() => expect(callsOf("tps_view_heartbeat").length).toBeGreaterThan(0));
    const id1 = viewIds()[0]!;
    expect(id1).toBeTruthy();
    expect(viewIds().every((v) => v === id1)).toBe(true);

    // 同实例重渲染：visible 不变不发新心跳；翻一次可见性触发释放后仍是同一 view_id
    ui1.rerender(<TpsCard selection={{}} />);
    Object.defineProperty(document, "visibilityState", {
      configurable: true,
      get: () => "hidden",
    });
    await act(async () => {
      document.dispatchEvent(new Event("visibilitychange"));
    });
    const afterHide = viewIds();
    expect(afterHide.length).toBeGreaterThan(1);
    expect(afterHide[afterHide.length - 1]).toBe(id1);
    // 恢复可见 + 撤销可见性覆写（不泄漏到后续用例）
    delete (document as unknown as { visibilityState?: string }).visibilityState;
    await act(async () => {
      document.dispatchEvent(new Event("visibilitychange"));
    });

    // 实例 2（模拟第二个窗口）：useId 若是 id 主体则两窗同串——随机主体保证互不相同
    const container2 = document.createElement("div");
    document.body.appendChild(container2);
    const ui2 = render(<TpsCard selection={{}} />, {
      wrapper: wrapper(newClient()),
      container: container2,
    });
    await waitFor(() => expect(viewIds().some((v) => v !== undefined && v !== id1)).toBe(true));
    const id2 = viewIds()[viewIds().length - 1]!;
    expect(id2).not.toBe(id1);
    ui2.unmount();
    container2.remove();
  });

  it("总授权关闭：按 snapshot.disabled 渲染停用态；refresh 的 rate-limited reason 不改变该判定", async () => {
    restore = override({
      tps_snapshot: () => Promise.resolve({ disabled: true, snapshot: null }),
      tps_points: () =>
        Promise.resolve({ disabled: true, knownSeries: [], points: [], rangeSinceMs: 0 }),
      tps_refresh: () =>
        Promise.resolve({ refreshed: false, reason: "rate-limited", snapshot: null }),
    });
    renderCard();
    await screen.findByTestId("tps-card");
    expect(await screen.findByTestId("tps-disabled")).toBeTruthy();
    expect(screen.queryByTestId("tps-current-tps")).toBeNull();
    expect(screen.queryByTestId("tps-chart")).toBeNull();

    // 点刷新：refreshed=false + reason 只出提示，不把停用态改写成别的
    fireEvent.click(screen.getByTestId("tps-refresh"));
    await waitFor(() => expect(callsOf("tps_refresh").length).toBeGreaterThan(0));
    await waitFor(() => expect(screen.getByTestId("tps-refresh-note")).toBeTruthy());
    expect(screen.getByTestId("tps-disabled")).toBeTruthy();
  });

  it("Codex 缺口按 tps-status= 前缀如实说明「本版不提供TPS」，不显示 Codex TPS", async () => {
    restore = overrideHappyTps();
    // 在 happy 路由之上仅覆写 snapshot 的来源状态（包装原实现，points 走 happy）
    const original = tauriInvokeMock.getMockImplementation() as Impl;
    tauriInvokeMock.mockImplementation((cmd: string, args?: unknown) => {
      if (cmd === "tps_snapshot") {
        return Promise.resolve(
          snapshotResult(
            [seriesSnapshot()],
            [
              sourceState("zcode", "tps-status=active"),
              sourceState(
                "codex",
                "tps-status=insufficient-timing; insufficient-generation-timing: no reliable per-call generation window"
              ),
              sourceState("claude", "tps-status=unconfigured"),
            ]
          )
        );
      }
      return original(cmd, args);
    });
    renderCard();
    await screen.findByTestId("tps-card");
    const codex = await screen.findByTestId("tps-source-codex");
    expect(codex).toHaveTextContent("本版不提供TPS");
    // claude 未配置 → 给一次点击的安装入口
    expect(screen.getByTestId("tps-install-claude")).toBeTruthy();
  });

  it("安装引导：预览（脱敏 chip）→ 应用 → 「下次启动生效」提示；卸载可点", async () => {
    restore = override({
      tps_snapshot: () =>
        Promise.resolve(
          snapshotResult([seriesSnapshot()], [sourceState("claude", "tps-status=unconfigured")])
        ),
      tps_install_preview: () =>
        Promise.resolve({
          previewId: "prev-1",
          tool: "claude",
          settingsPath: "/home/u/.claude/settings.json",
          expiresAtMs: Date.now() + 600_000,
          entries: [
            {
              key: "env.OTEL_METRICS_EXPORTER",
              oldExists: false,
              oldMasked: null,
              oldFingerprint: null,
              plannedMasked: "otlp",
              conflict: false,
            },
            {
              key: "env.OTEL_EXPORTER_OTLP_HEADERS",
              oldExists: true,
              oldMasked: "[redacted ab12cd34]",
              oldFingerprint: "ab12cd34",
              plannedMasked: "[redacted ef56ab78]",
              conflict: false,
            },
          ],
          conflictKeys: [],
        }),
      tps_install_apply: (_cmd: string, args?: unknown) => {
        expect((args as { previewId?: string }).previewId).toBe("prev-1");
        return Promise.resolve({
          tool: "claude",
          appliedAtMs: Date.now(),
          choice: "append-only",
          entries: [
            {
              key: "env.OTEL_EXPORTER_OTLP_HEADERS",
              oldExists: true,
              oldValue: null,
              installedValue: "[已脱敏]",
              oldFingerprint: "ab12cd34",
            },
          ],
          receiver: { port: 47181 },
        });
      },
      tps_uninstall: () => Promise.resolve(null),
    });
    renderCard();
    await screen.findByTestId("tps-card");
    fireEvent.click(await screen.findByTestId("tps-install-claude"));
    // 预览：敏感值以脱敏 chip 呈现（[redacted …] / [已脱敏] 两种标记同权）
    await screen.findByTestId("tps-preview");
    const chips = screen.getAllByTestId("tps-masked-chip");
    expect(chips.length).toBeGreaterThanOrEqual(2);
    fireEvent.click(screen.getByTestId("tps-apply"));
    // 回执 + 「下次启动生效」提示（不替用户重启 Harness）
    expect(await screen.findByTestId("tps-restart-hint")).toHaveTextContent("下次启动生效");
    expect(screen.getAllByTestId("tps-masked-chip").length).toBeGreaterThanOrEqual(1);
    // 已安装态给卸载入口
    expect(screen.getByTestId("tps-uninstall-claude")).toBeTruthy();
    fireEvent.click(screen.getByTestId("tps-uninstall-claude"));
    await waitFor(() => expect(callsOf("tps_uninstall").length).toBeGreaterThan(0));
  });

  it("TPS 查询失败只替换本区块，费用卡照常", async () => {
    restore = override({
      tps_snapshot: () => Promise.reject({ code: "usage-internal", detail: "tps boom" }),
      tps_points: () => Promise.reject({ code: "usage-internal", detail: "tps boom" }),
    });
    renderPage();
    expect(await screen.findByTestId("tps-error")).toBeTruthy();
    await waitFor(() => expect(screen.getByTestId("usage-cost-body")).toBeTruthy());
  });

  it("R10：卸载入口来自持久安装事实（tps-installed=true），不依赖本组件临时回执或 await-restart", async () => {
    // 已生效（active）与安装后读取失败（read-failed）的来源：首次挂载、无本地回执，仍必须能卸载；
    // 已安装的来源不再显示「安装统计」按钮；未安装（tps-installed=false）才显示安装入口
    restore = override({
      tps_snapshot: () =>
        Promise.resolve(
          snapshotResult(
            [seriesSnapshot()],
            [
              sourceState("claude", "tps-status=active; tps-installed=true"),
              sourceState(
                "opencode",
                "tps-status=read-failed; tps-installed=true; tps-receiver-port-conflict: x",
                false
              ),
              sourceState("zcode", "tps-status=active"),
            ]
          )
        ),
    });
    renderCard();
    await screen.findByTestId("tps-card");
    expect(await screen.findByTestId("tps-uninstall-claude")).toBeTruthy();
    expect(screen.getByTestId("tps-uninstall-opencode")).toBeTruthy();
    expect(screen.queryByTestId("tps-install-claude")).toBeNull();
    expect(screen.queryByTestId("tps-install-opencode")).toBeNull();
    // 非可安装来源无任何安装/卸载入口
    expect(screen.queryByTestId("tps-uninstall-zcode")).toBeNull();
  });

  it("R10：未安装（tps-installed=false）的 unconfigured 来源显示安装入口、无卸载入口", async () => {
    restore = override({
      tps_snapshot: () =>
        Promise.resolve(
          snapshotResult(
            [seriesSnapshot()],
            [sourceState("claude", "tps-status=unconfigured; tps-installed=false")]
          )
        ),
    });
    renderCard();
    await screen.findByTestId("tps-card");
    expect(await screen.findByTestId("tps-install-claude")).toBeTruthy();
    expect(screen.queryByTestId("tps-uninstall-claude")).toBeNull();
  });

  it("R10'：成功卸载后即使保留 24h 历史（active / no-sample / read-failed + installed=false）仍有安装入口", async () => {
    restore = override({
      tps_snapshot: () =>
        Promise.resolve(
          snapshotResult(
            [seriesSnapshot()],
            [
              sourceState("claude", "tps-status=active; tps-installed=false"),
              sourceState("opencode", "tps-status=no-sample; tps-installed=false"),
            ]
          )
        ),
    });
    renderCard();
    await screen.findByTestId("tps-card");
    expect(await screen.findByTestId("tps-install-claude")).toBeTruthy();
    expect(screen.getByTestId("tps-install-opencode")).toBeTruthy();
    expect(screen.queryByTestId("tps-uninstall-claude")).toBeNull();
    expect(screen.queryByTestId("tps-uninstall-opencode")).toBeNull();
    // 历史仍在（未用删历史绕过）
    expect(screen.getByTestId("tps-card")).toHaveTextContent("token/s");
  });

  it("R10'：read-failed + installed=false 同样给安装入口", async () => {
    restore = override({
      tps_snapshot: () =>
        Promise.resolve(
          snapshotResult(
            [seriesSnapshot()],
            [
              sourceState(
                "claude",
                "tps-status=read-failed; tps-installed=false; tps-receiver-port-conflict: x",
                false
              ),
            ]
          )
        ),
    });
    renderCard();
    await screen.findByTestId("tps-card");
    expect(await screen.findByTestId("tps-install-claude")).toBeTruthy();
    expect(screen.queryByTestId("tps-uninstall-claude")).toBeNull();
  });

  it("R10'：完整 UI 流程——已装 active → 卸载（历史保留）→ 安装入口出现 → 重装 → 卸载入口回来", async () => {
    let installed = true;
    restore = override({
      tps_snapshot: () =>
        Promise.resolve(
          snapshotResult(
            [seriesSnapshot()],
            [sourceState("claude", `tps-status=active; tps-installed=${installed}`)]
          )
        ),
      tps_uninstall: () => {
        installed = false;
        return Promise.resolve(null);
      },
      tps_install_preview: () =>
        Promise.resolve({
          previewId: "prev-2",
          tool: "claude",
          settingsPath: "/home/u/.claude/settings.json",
          expiresAtMs: Date.now() + 600_000,
          entries: [
            {
              key: "env.OTEL_METRICS_EXPORTER",
              oldExists: false,
              oldMasked: null,
              oldFingerprint: null,
              plannedMasked: "otlp",
              conflict: false,
            },
          ],
          conflictKeys: [],
        }),
      tps_install_apply: () => {
        installed = true;
        return Promise.resolve({
          tool: "claude",
          appliedAtMs: Date.now(),
          choice: "append-only",
          entries: [
            {
              key: "env.OTEL_METRICS_EXPORTER",
              oldExists: false,
              oldValue: null,
              installedValue: "otlp",
              oldFingerprint: null,
            },
          ],
          receiver: { port: 47181 },
        });
      },
    });
    renderCard();
    await screen.findByTestId("tps-card");
    fireEvent.click(await screen.findByTestId("tps-uninstall-claude"));
    await waitFor(() => expect(callsOf("tps_uninstall").length).toBeGreaterThan(0));
    // 卸载后：历史仍显示，安装入口必须出现（不依赖状态回到 unconfigured）
    expect(await screen.findByTestId("tps-install-claude")).toBeTruthy();
    expect(screen.getByTestId("tps-card")).toHaveTextContent("token/s");
    fireEvent.click(screen.getByTestId("tps-install-claude"));
    await screen.findByTestId("tps-preview");
    fireEvent.click(screen.getByTestId("tps-apply"));
    expect(await screen.findByTestId("tps-restart-hint")).toBeTruthy();
    expect(await screen.findByTestId("tps-uninstall-claude")).toBeTruthy();
    expect(screen.queryByTestId("tps-install-claude")).toBeNull();
  });
});
