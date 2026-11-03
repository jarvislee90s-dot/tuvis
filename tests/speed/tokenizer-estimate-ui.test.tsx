import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import { mockSpeedDashboard, resetTpsMockState } from "@/lib/usage/mockFixtures";
import { SpeedDashboard } from "@/components/speed/SpeedDashboard";
import { setMockSpeedDashboard, tauriInvokeMock } from "../msw/tauriMocks";

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
function renderDashboard() {
  return render(
    <QueryClientProvider
      client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}
    >
      <SpeedDashboard active />
    </QueryClientProvider>
  );
}
beforeEach(async () => {
  await i18n.changeLanguage("zh");
  tauriInvokeMock.mockClear();
  setMockSpeedDashboard(null);
  resetTpsMockState();
});
describe("tokenizer estimate dashboard", () => {
  it("keeps all three modes accessible when no series has ever been recorded", async () => {
    setMockSpeedDashboard((query) => ({
      ...mockSpeedDashboard(query),
      series: [],
      selectedSeries: null,
      buckets: [],
    }));
    renderDashboard();
    const button = await screen.findByTestId("speed-mode-tokenizer-estimate");
    fireEvent.click(button);
    expect(screen.getByTestId("speed-mode").getAttribute("data-mode")).toBe("tokenizer-estimate");
  });
  it("offers a third mode without showing native values when estimates are empty", async () => {
    renderDashboard();
    await screen.findByTestId("speed-mode-stream");
    fireEvent.click(screen.getByTestId("speed-mode-tokenizer-estimate"));
    await waitFor(() =>
      expect(screen.getByTestId("speed-mode").getAttribute("data-mode")).toBe("tokenizer-estimate")
    );
    expect(screen.queryByTestId("speed-rate-value")?.textContent ?? "—").toBe("—");
    expect(screen.getByText(/分词器估算只统计可见正文/)).toBeTruthy();
  });
  it("switches the entire dashboard to the matching estimate series", async () => {
    setMockSpeedDashboard((query) => {
      const base = mockSpeedDashboard({ ...query!, metricMode: "stream", selection: null });
      if (query?.metricMode !== "tokenizer-estimate") return base;
      const native = base.series.find(
        (item) =>
          item.seriesKey.tool === base.selectedSeries?.tool &&
          item.seriesKey.model === base.selectedSeries?.model
      )!;
      const key = {
        ...native.seriesKey,
        method: "ipc-body-reference" as const,
        referenceProfileId: "fixture-profile",
      };
      const metrics = {
        ...native.metrics,
        tokenizerEstimate: { value: 40, outputTokens: 400, durationMs: 10000, sampleCount: 2 },
        bodyObservedMs: 10000,
        avgFirstTokenMs: 999,
        toolMs: 999,
      };
      return {
        ...base,
        series: [{ seriesKey: key, metrics }],
        selectedSeries: query.selection ?? key,
        buckets: base.buckets.map((bucket) => ({ ...bucket, metrics })),
      };
    });
    renderDashboard();
    await screen.findByTestId("speed-mode-stream");
    fireEvent.click(screen.getByTestId("speed-mode-tokenizer-estimate"));
    await waitFor(() => expect(screen.getByTestId("speed-rate-value").textContent).toBe("40.0"));
    expect(screen.getByTestId("speed-rate-unit").textContent).toContain("参考 Token/s");
    expect(screen.getByTestId("speed-first-token").textContent).toBe("—");
    expect(screen.getByTestId("speed-tool-duration").textContent).toBe("—");
    expect(screen.getByTestId("speed-model-duration").textContent).toContain("10.0");
    expect(screen.getByTestId("speed-metrics-context").textContent).toContain("n=2");
  });
  it("ranks only one reference profile and permits selecting another", async () => {
    setMockSpeedDashboard((query) => {
      const base = mockSpeedDashboard({ ...query!, metricMode: "stream", selection: null });
      if (query?.metricMode !== "tokenizer-estimate") return base;
      const native = base.series.find(
        (item) =>
          item.seriesKey.tool === base.selectedSeries?.tool &&
          item.seriesKey.model === base.selectedSeries?.model
      )!;
      const keyA = {
        ...native.seriesKey,
        method: "ipc-body-reference" as const,
        referenceProfileId: "profile-A",
      };
      const keyB = { ...keyA, referenceProfileId: "profile-B" };
      const metrics = {
        ...native.metrics,
        tokenizerEstimate: { value: 40, outputTokens: 400, durationMs: 10000, sampleCount: 2 },
        bodyObservedMs: 10000,
      };
      return {
        ...base,
        series: [
          { seriesKey: keyA, metrics },
          {
            seriesKey: keyB,
            metrics: { ...metrics, tokenizerEstimate: { ...metrics.tokenizerEstimate, value: 80 } },
          },
        ],
        selectedSeries: query.selection ?? keyA,
        buckets: [],
      };
    });
    renderDashboard();
    await screen.findByTestId("speed-mode-stream");
    fireEvent.click(screen.getByTestId("speed-mode-tokenizer-estimate"));
    const selector = await screen.findByTestId("speed-profile-filter");
    fireEvent.change(selector, { target: { value: "profile-A" } });
    await waitFor(() => expect(screen.getAllByTestId("speed-series-row")).toHaveLength(1));
    expect(screen.getByTestId("speed-rate-value").textContent).toBe("40.0");
    fireEvent.change(screen.getByTestId("speed-profile-filter"), {
      target: { value: "profile-B" },
    });
    await waitFor(() => expect(screen.getByTestId("speed-rate-value").textContent).toBe("80.0"));
    expect(screen.getAllByTestId("speed-series-row")).toHaveLength(1);
  });
  it("ignores a late old-mode response and clears a pinned bucket", async () => {
    const original = tauriInvokeMock.getMockImplementation()!;
    let release!: (value: ReturnType<typeof mockSpeedDashboard>) => void;
    const delayed = new Promise<ReturnType<typeof mockSpeedDashboard>>((resolve) => {
      release = resolve;
    });
    setMockSpeedDashboard((query) => {
      const base = mockSpeedDashboard({ ...query!, metricMode: "stream", selection: null });
      if (query?.metricMode !== "tokenizer-estimate") return base;
      const native = base.series.find(
        (item) =>
          item.seriesKey.tool === base.selectedSeries?.tool &&
          item.seriesKey.model === base.selectedSeries?.model
      )!;
      const keyA = {
        ...native.seriesKey,
        method: "ipc-body-reference" as const,
        referenceProfileId: "profile-A",
      };
      const keyB = { ...keyA, model: "other-model" };
      const metrics = {
        ...native.metrics,
        tokenizerEstimate: { value: 40, outputTokens: 400, durationMs: 10000, sampleCount: 2 },
        bodyObservedMs: 10000,
      };
      return {
        ...base,
        series: [
          { seriesKey: keyA, metrics },
          {
            seriesKey: keyB,
            metrics: { ...metrics, tokenizerEstimate: { ...metrics.tokenizerEstimate, value: 80 } },
          },
        ],
        selectedSeries: query.selection ?? keyA,
        buckets: [],
      };
    });
    tauriInvokeMock.mockImplementation((cmd, args) => {
      if (
        cmd === "tps_speed_dashboard" &&
        (args as { query?: { metricMode?: string } })?.query?.metricMode === "response"
      )
        return delayed;
      return original(cmd, args);
    });
    try {
      renderDashboard();
      await screen.findByTestId("speed-chart");
      fireEvent.click(screen.getAllByTestId("speed-chart-hit")[0]!);
      expect(screen.getByTestId("speed-clear-pin")).toBeTruthy();
      fireEvent.click(screen.getByTestId("speed-mode-response"));
      expect(screen.queryByTestId("speed-clear-pin")).toBeNull();
      fireEvent.click(screen.getByTestId("speed-mode-tokenizer-estimate"));
      await waitFor(() =>
        expect(screen.getByTestId("speed-mode").getAttribute("data-mode")).toBe(
          "tokenizer-estimate"
        )
      );
      await waitFor(() => expect(screen.getByTestId("speed-rate-value").textContent).toBe("40.0"));
      fireEvent.click(
        screen
          .getAllByTestId("speed-series-row")
          .find((row) => row.textContent?.includes("other-model"))!
      );
      await waitFor(() => expect(screen.getByTestId("speed-rate-value").textContent).toBe("80.0"));
      await act(async () => {
        release(mockSpeedDashboard());
        await delayed;
      });
      await waitFor(() =>
        expect(screen.getByTestId("speed-mode").getAttribute("data-mode")).toBe(
          "tokenizer-estimate"
        )
      );
      expect(screen.getByTestId("speed-rate-value").textContent).toBe("80.0");
      expect(screen.getByTestId("speed-detail-title").textContent).toContain("other-model");
    } finally {
      tauriInvokeMock.mockImplementation(original);
    }
  });
  it("exposes an independent estimate stop control and reference details", async () => {
    renderDashboard();
    await screen.findByTestId("speed-sources-open");
    fireEvent.click(screen.getByTestId("speed-sources-open"));
    fireEvent.click(await screen.findByTestId("speed-details-toggle-codex"));
    expect(screen.getByText(/o200k_base/)).toBeTruthy();
    fireEvent.click(screen.getByTestId("speed-estimate-toggle"));
    await waitFor(() =>
      expect(tauriInvokeMock).toHaveBeenCalledWith("tps_tokenizer_estimate_set_enabled", {
        enabled: false,
      })
    );
  });
  it("shows all three formula notes in English as well", async () => {
    await i18n.changeLanguage("en");
    renderDashboard();
    await screen.findByTestId("speed-mode-stream");
    expect(
      screen.getByText(/Tokenizer estimate = reference tokens in the visible text/)
    ).toBeTruthy();
    expect(screen.getByText(/Estimation error has not been calibrated/)).toBeTruthy();
  });
});
