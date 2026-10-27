// Task 10（2026-10-26 计划）· 双指标图表与区间四格（vitest + jsdom）。
// 判据来源：task-10-brief Step1（bucket_hover_pin_and_gap）+ Spec §5.3/§8.3–8.5：
//  * 两模式单位（stream=TPS / response=Token/s）与 n 来自 rateForMode，切模式即换；
//  * hover 桶：四格联动 + 细竖线 + 真实 DOM tooltip（实际起止/桶宽/n）；空速度桶不推算
//    速度（无值、无插值），但可靠 TTFT/用时不整组清空（其他格独立显示，Spec §5.3）；
//  * 移开恢复区间汇总；click 固定**快照**（metricsSnapshot 与点击时刻深相等、非活引用），
//    refetch 换桶数据不得改变固定读数；Esc / 切模式解除固定（Spec §8.3）；
//  * 键盘可操作：focus 后 ArrowLeft/Right 移动选桶、Escape 清除，不依赖 pointer；
//  * gap：实测线只在有值相邻桶间连续（分段）；灰虚线独立图层 + pointer-events:none，
//    不参与悬停；无插值 tooltip（无有效样本）；阴影层；图例「虚线仅连接，无样本」；
//    gapStyle="band" 严格断开（无连接线）；
//  * 几何：x 按完整 [from,to] 铺满（首末桶贴边），y 从 0 起 + 易读刻度；按宽度 4–7 标签；
//    24h 跨日桶 tooltip 带日期；单点也出坐标轴；全部无值也出坐标轴；
//  * Codex（log-boundary-approx）：用时格标签「响应累计用时」+ 一级「日志近似」短标记，
//    TTFT 如实「—」不伪造。
// 纪律：夹具为手搭 SpeedBucket[]（含 Codex 仅响应序列与空桶段）；i18n 全部走 speed.*。
import { useState } from "react";
import { fireEvent, render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import { SpeedChart, SPEED_CHART_PAD, type PinnedBucket } from "@/components/speed/SpeedChart";
import { SpeedMetrics, type SpeedMetricsContext } from "@/components/speed/SpeedMetrics";
import { SpeedSeriesList } from "@/components/speed/SpeedSeriesList";
import { niceScale, pinSnapshot, segmentSeries, timeTicks } from "@/lib/speed/chart";
import type {
  SeriesKey,
  SpeedBucket,
  SpeedMetricMode,
  SpeedMetrics as SpeedMetricsData,
  SpeedRate,
  SpeedSeriesSummary,
} from "@/types/speed";

// ==== 夹具 ====

/** 桶宽 5 分钟（1h 档，Spec §7 档位表）；起点取本地 2026-10-26 14:00（同日，无跨日）。 */
const WIDTH_MS = 5 * 60_000;
const FROM = new Date(2026, 9, 26, 14, 0, 0).getTime();
const TO = FROM + 12 * WIDTH_MS;

function rate(
  value: number | null,
  outputTokens: number,
  durationMs: number,
  sampleCount: number
): SpeedRate {
  return { value, outputTokens, durationMs, sampleCount };
}

function bucketMetrics(overrides: Partial<SpeedMetricsData> = {}): SpeedMetricsData {
  return {
    stream: rate(86.4, 3_888, 45_000, 12),
    response: rate(62.1, 3_888, 62_600, 10),
    avgFirstTokenMs: 1_234,
    ttftSampleCount: 9,
    modelMs: 45_200,
    toolMs: null,
    candidateCount: 15,
    failedCount: 2,
    cancelledCount: 1,
    missingTimingCount: 0,
    sessionCount: 4,
    latestStreamEndedAt: null,
    latestResponseEndedAt: null,
    outputScope: "providerReported",
    scopeMixed: false,
    ...overrides,
  };
}

/** 区间汇总读数（与任何单桶值都不同，供「移开恢复」断言）。 */
const RANGE_SUMMARY = bucketMetrics({
  stream: rate(84.2, 25_260, 300_000, 30),
  response: rate(61.7, 25_260, 409_400, 26),
  avgFirstTokenMs: 1_234,
  ttftSampleCount: 22,
  modelMs: 300_000,
});

/** 12 只桶；`streamNullAt` 指定的桶 stream 无值（n=0 ⇒ null，不伪装 0）。
 *  桶 5 刻意「空速度但有可靠 TTFT/用时」——Spec §5.3「没有当前模式速度但其他指标
 *  可靠时，其他格仍独立显示」。stream 值 70+i：桶 0=70.0、桶 2=72.0、桶 3=73.0。 */
function makeBuckets(streamNullAt: number[] = []): SpeedBucket[] {
  const nulls = new Set(streamNullAt);
  return Array.from({ length: 12 }, (_, i) => {
    const emptySpeed = nulls.has(i);
    return {
      startMs: FROM + i * WIDTH_MS,
      endMs: FROM + (i + 1) * WIDTH_MS,
      isPartialBucket: false,
      metrics: bucketMetrics({
        stream: emptySpeed ? rate(null, 0, 0, 0) : rate(70 + i, (70 + i) * 150, 15_000, 3),
        response: rate(50 + i * 0.5, 5_200, 104_000, 4),
        avgFirstTokenMs: emptySpeed && i !== 5 ? null : 1_200 + i * 10,
        ttftSampleCount: emptySpeed && i !== 5 ? 0 : 3,
        modelMs: i === 5 ? 28_000 : 24_000,
        sessionCount: 2,
      }),
    };
  });
}

/** 24h 档（桶宽 1 小时 × 24 只，本地 2026-10-26 00:00 → 10-27 00:00，跨日）。 */
const FROM24 = new Date(2026, 9, 26, 0, 0, 0).getTime();
const WIDTH24 = 3_600_000;
const TO24 = FROM24 + 24 * WIDTH24;
function makeBuckets24(): SpeedBucket[] {
  return Array.from({ length: 24 }, (_, i) => ({
    startMs: FROM24 + i * WIDTH24,
    endMs: FROM24 + (i + 1) * WIDTH24,
    isPartialBucket: false,
    metrics: bucketMetrics({ stream: rate(60 + i, 9_000, 150_000, 6) }),
  }));
}

// ==== 测试挂件：图表 + 四格按 dashboard 的方式接线（固定 > 悬停 > 区间汇总）====

function Harness(props: {
  buckets: SpeedBucket[];
  mode: SpeedMetricMode;
  from?: number;
  to?: number;
  gapStyle?: "dash" | "band";
  seriesMethod?: string;
}) {
  const [hovered, setHovered] = useState<SpeedBucket | null>(null);
  const [pinned, setPinned] = useState<PinnedBucket | null>(null);
  const display = pinned ? pinned.metricsSnapshot : hovered ? hovered.metrics : RANGE_SUMMARY;
  const context: SpeedMetricsContext = pinned
    ? {
        kind: "bucket",
        startMs: pinned.startMs,
        endMs: pinned.endMs,
        bucketWidthMs: pinned.endMs - pinned.startMs,
        pinned: true,
      }
    : hovered
      ? {
          kind: "bucket",
          startMs: hovered.startMs,
          endMs: hovered.endMs,
          bucketWidthMs: hovered.endMs - hovered.startMs,
        }
      : { kind: "range-summary" };
  return (
    <div>
      <SpeedChart
        buckets={props.buckets}
        mode={props.mode}
        from={props.from ?? FROM}
        to={props.to ?? TO}
        onHover={setHovered}
        onPin={setPinned}
        pinned={pinned}
        gapStyle={props.gapStyle}
        seriesKey={{
          tool: "zcode",
          model: "glm-5.3-flash",
          channel: null,
          method: "stream-observed",
        }}
      />
      <SpeedMetrics
        metrics={display}
        mode={props.mode}
        series={{
          tool: "zcode",
          model: "glm-5.3-flash",
          channel: null,
          method: props.seriesMethod,
        }}
        context={context}
        onClearPin={() => setPinned(null)}
      />
    </div>
  );
}

function hits(): HTMLElement[] {
  return screen.getAllByTestId("speed-chart-hit");
}

function rateValueText(): string {
  return screen.getByTestId("speed-rate-value").textContent ?? "";
}

beforeEach(async () => {
  await i18n.changeLanguage("zh");
});

// ==== 1. 两模式：单位与 n ====

describe("两模式单位与汇总读数", () => {
  it("stream 模式四格显示 TPS 与区间汇总（mode 名 + n）", () => {
    render(<Harness buckets={makeBuckets()} mode="stream" />);
    expect(screen.getByTestId("speed-rate-unit")).toHaveTextContent("TPS");
    expect(rateValueText()).toBe("84.2"); // rateForMode(RANGE_SUMMARY, "stream").value
    const ctx = screen.getByTestId("speed-metrics-context").textContent ?? "";
    expect(ctx).toContain("区间汇总");
    expect(ctx).toContain("流式输出速度");
    expect(ctx).toContain("n=30");
    expect(screen.getByTestId("speed-metrics")).toHaveAttribute("data-mode", "stream");
  });

  it("response 模式显示 Token/s（brief 判据逐字）", () => {
    render(<Harness buckets={makeBuckets()} mode="response" />);
    expect(screen.getByTestId("speed-rate-unit")).toHaveTextContent("Token/s");
    expect(rateValueText()).toBe("61.7");
    expect(screen.getByTestId("speed-metrics")).toHaveAttribute("data-mode", "response");
  });

  // 窄窗形态锁（Spec §2.8：窄窗排名表 + 指标 2×2）——jsdom 无法做真实响应式布局，
  // 这里做类级锁定（class-level lock）。指标网格与列表两侧的断点必须同在 lg:——任一侧
  // 漂移到 md:（800px 主窗 = 列表仍限高、四格已横排的混合形态）都必须报红。
  it("指标网格 2×2 ↔ lg 四列；列表两布局同一份 DOM（<lg 限高、≥lg 铺满），不漂到 md:", () => {
    const seriesKey: SeriesKey = {
      tool: "zcode",
      model: "glm-5.3-flash",
      channel: null,
      method: "stream-observed",
    };
    const series: SpeedSeriesSummary[] = [{ seriesKey, metrics: bucketMetrics() }];
    render(
      <div>
        <Harness buckets={makeBuckets()} mode="stream" />
        <SpeedSeriesList
          series={series}
          mode="stream"
          selectedKey={seriesKey}
          onSelect={vi.fn()}
          toolId={null}
          toolOptions={["zcode"]}
          onToolFilterChange={vi.fn()}
          sortKey="name"
          sortDirection="asc"
          onSortKeyChange={vi.fn()}
          onToggleSortDirection={vi.fn()}
          // 二轮实机反馈 ③：父层经 className 注入 order 类（窄窗详情在上、宽窗恢复左列表）
          className="order-2 lg:order-1"
        />
      </div>
    );
    // 指标网格：2×2 基线 + lg 四列
    const grid = screen.getByTestId("speed-metrics").firstElementChild as HTMLElement;
    expect(grid.className).toContain("grid-cols-2");
    expect(grid.className).toContain("lg:grid-cols-4");
    expect(grid.className).not.toMatch(/\bmd:grid-cols-4\b/);
    // 列表容器：两布局同一份 DOM——不 hidden，<lg 限高滚动（max-h-56），≥lg 吸顶自滚
    const list = screen.getByTestId("speed-series-list") as HTMLElement;
    expect(list.className).not.toContain("hidden");
    expect(list.className).toContain("max-h-56");
    expect(list.className).toContain("lg:sticky");
    expect(list.className).toContain("lg:top-0");
    expect(list.className).toContain("lg:max-h-[calc(100vh-190px)]");
    expect(list.className).not.toMatch(/\bmd:max-h\b/);
    // 二轮实机反馈 ③：order 类经 className 透传到列表 pane（speed-list-pane；窄窗 order-2
    // 在详情下、宽窗 lg:order-1 回左列）——row 表本身不带 order，pane 才是栅格直接子元素
    const pane = screen.getByTestId("speed-list-pane") as HTMLElement;
    expect(pane.className).toContain("order-2");
    expect(pane.className).toContain("lg:order-1");
    // 窄窗下拉选择器已删除（2026-10-27 实机反馈 #4a）：不再有 speed-series-select
    expect(screen.queryByTestId("speed-series-select")).toBeNull();
  });
});

// ==== 2. hover：四格联动 + 竖线 + tooltip；空速度桶不清其他格 ====

describe("hover 桶联动", () => {
  it("hover 有值桶：四格更新为该桶读数，竖线 + tooltip 出实际起止/桶宽/n", () => {
    render(<Harness buckets={makeBuckets()} mode="stream" />);
    fireEvent.mouseEnter(hits()[2]);
    expect(rateValueText()).toBe("72.0"); // 桶 2 的 stream 值 70+2
    expect(screen.getByTestId("speed-metrics-context").textContent).toContain("n=3");
    expect(screen.getByTestId("speed-chart-guide")).toBeInTheDocument();
    const tip = screen.getByTestId("speed-chart-tooltip").textContent ?? "";
    expect(tip).toContain("14:10"); // 桶 2 起点（本地 14:00 + 2×5min）
    expect(tip).toContain("14:15"); // 终点
    expect(tip).toContain("5.0 分钟"); // 桶宽
    expect(tip).toContain("n=3");
    expect(tip).toContain("72.0"); // 当前模式值 + 单位
    expect(tip).toContain("TPS");
  });

  it("空速度桶：速度格 — 且 tooltip 无值无插值（无有效样本），可靠 TTFT/用时不被清空", () => {
    render(<Harness buckets={makeBuckets([5, 6])} mode="stream" />);
    fireEvent.mouseEnter(hits()[5]);
    expect(rateValueText()).toBe("—");
    // 其他格独立显示（Spec §5.3）：桶 5 的 TTFT=1250ms、modelMs=28s 仍在
    expect(screen.getByTestId("speed-first-token")).toHaveTextContent("1.25 s");
    expect(screen.getByTestId("speed-model-duration")).toHaveTextContent("28.0 秒");
    expect(screen.getByTestId("speed-chart-tooltip").textContent).toContain("无有效样本");
    expect(screen.getByTestId("speed-chart-tooltip").textContent).not.toContain("TPS");
  });

  it("移开恢复区间汇总", () => {
    render(<Harness buckets={makeBuckets()} mode="stream" />);
    fireEvent.mouseEnter(hits()[2]);
    expect(rateValueText()).toBe("72.0");
    fireEvent.mouseLeave(screen.getByTestId("speed-chart-svg"));
    expect(rateValueText()).toBe("84.2");
    expect(screen.queryByTestId("speed-chart-tooltip")).toBeNull();
    expect(screen.queryByTestId("speed-chart-guide")).toBeNull();
  });
});

// ==== 3. click 固定快照；refetch 不漂移；Esc / 切模式解除 ====

describe("click 固定与解除", () => {
  it("固定对象=点击时刻快照（深相等、非活引用），不固定数组下标", () => {
    const buckets = makeBuckets();
    const onPin = vi.fn();
    render(
      <SpeedChart
        buckets={buckets}
        mode="stream"
        from={FROM}
        to={TO}
        onPin={onPin}
        seriesKey={null}
      />
    );
    fireEvent.click(hits()[3]);
    const pinned = onPin.mock.calls.at(-1)![0] as PinnedBucket;
    expect(pinned.metricsSnapshot).toEqual(buckets[3].metrics); // brief 判据逐字
    expect(pinned.metricsSnapshot).not.toBe(buckets[3].metrics); // 是拷贝不是活引用
    expect(pinned.startMs).toBe(buckets[3].startMs);
    expect(pinned.endMs).toBe(buckets[3].endMs);
    expect(pinned.mode).toBe("stream");
    expect("index" in pinned).toBe(false); // 不固定数组 index
  });

  it("refetch 换桶数据后固定读数不变（快照），四格仍显示点击时刻值", () => {
    const before = makeBuckets();
    const { rerender } = render(<Harness buckets={before} mode="stream" />);
    fireEvent.click(hits()[3]);
    expect(rateValueText()).toBe("73.0"); // 桶 3
    // 模拟 5 秒轮询 refetch：数值全部翻倍（新对象）
    const after = before.map((b) => ({
      ...b,
      metrics: bucketMetrics({
        ...b.metrics,
        stream: rate((70 + 3) * 2, 43_800, 30_000, 6),
        response: rate(120, 10_400, 86_000, 8),
      }),
    }));
    rerender(<Harness buckets={after} mode="stream" />);
    expect(rateValueText()).toBe("73.0"); // 固定快照不随 refetch 漂移
    expect(screen.getByTestId("speed-clear-pin")).toHaveTextContent("返回区间汇总");
  });

  it("Escape 解除：四格回到区间汇总（onPin(null) 被触发）", () => {
    const buckets = makeBuckets();
    const onPin = vi.fn();
    const spyChart = render(
      <SpeedChart
        buckets={buckets}
        mode="stream"
        from={FROM}
        to={TO}
        onPin={onPin}
        seriesKey={null}
      />
    );
    fireEvent.click(hits()[3]);
    expect(onPin.mock.calls.at(-1)![0]).not.toBeNull();
    fireEvent.keyDown(screen.getByTestId("speed-chart"), { key: "Escape" });
    expect(onPin.mock.calls.at(-1)![0]).toBeNull();
    spyChart.unmount();

    const harness = render(<Harness buckets={buckets} mode="stream" />);
    fireEvent.click(hits()[3]);
    fireEvent.keyDown(screen.getByTestId("speed-chart"), { key: "Escape" });
    expect(rateValueText()).toBe("84.2");
    harness.unmount();
  });

  it("切模式解除固定（Spec §8.3：切换范围/组合/模式解除）", () => {
    const buckets = makeBuckets();
    const onPin = vi.fn();
    const { rerender } = render(
      <SpeedChart
        buckets={buckets}
        mode="stream"
        from={FROM}
        to={TO}
        onPin={onPin}
        seriesKey={null}
      />
    );
    fireEvent.click(hits()[3]);
    expect(onPin.mock.calls.at(-1)![0]).not.toBeNull();
    rerender(
      <SpeedChart
        buckets={buckets}
        mode="response"
        from={FROM}
        to={TO}
        onPin={onPin}
        seriesKey={null}
      />
    );
    expect(onPin.mock.calls.at(-1)![0]).toBeNull();
  });

  // Task 12 裁决（加成 prop）：clearSignalKey 提供时，解除判定只看该键——滚动窗口的
  // from/to 每 5 秒随 asOf 前移不算「切换」，固定不丢；键变化才解除。缺省保持旧行为
  // （上面两条用例即旧行为）。
  it("clearSignalKey：from/to 滚动（同键）不解除固定；键变化才解除", () => {
    const buckets = makeBuckets();
    const onPin = vi.fn();
    const { rerender } = render(
      <SpeedChart
        buckets={buckets}
        mode="stream"
        from={FROM}
        to={TO}
        onPin={onPin}
        clearSignalKey="stream:1h:zcode"
        seriesKey={null}
      />
    );
    fireEvent.click(hits()[3]);
    expect(onPin.mock.calls.at(-1)![0]).not.toBeNull();

    // 滚动窗口：from/to 前移、模式与键都不变 ⇒ 固定保留（不触发 onPin(null)）
    rerender(
      <SpeedChart
        buckets={buckets}
        mode="stream"
        from={FROM + 5_000}
        to={TO + 5_000}
        onPin={onPin}
        clearSignalKey="stream:1h:zcode"
        seriesKey={null}
      />
    );
    expect(onPin.mock.calls.at(-1)![0]).not.toBeNull();

    // 键变化（切组合/范围/模式）⇒ 解除
    rerender(
      <SpeedChart
        buckets={buckets}
        mode="stream"
        from={FROM + 5_000}
        to={TO + 5_000}
        onPin={onPin}
        clearSignalKey="stream:15m:kimi"
        seriesKey={null}
      />
    );
    expect(onPin.mock.calls.at(-1)![0]).toBeNull();
  });

  it("固定空桶：速度格 —、其他格保留快照值；点「返回区间汇总」解除", () => {
    render(<Harness buckets={makeBuckets([5, 6])} mode="stream" />);
    fireEvent.click(hits()[5]);
    expect(rateValueText()).toBe("—");
    expect(screen.getByTestId("speed-first-token")).toHaveTextContent("1.25 s");
    fireEvent.click(screen.getByTestId("speed-clear-pin"));
    expect(rateValueText()).toBe("84.2");
  });
});

// ==== 4. 键盘可操作（不依赖 pointer）====

describe("键盘操作", () => {
  it("focus 后 ArrowRight/Left 移动选桶，Enter 固定，Escape 清除", () => {
    render(<Harness buckets={makeBuckets()} mode="stream" />);
    const wrapper = screen.getByTestId("speed-chart");
    wrapper.focus();
    fireEvent.keyDown(wrapper, { key: "ArrowRight" });
    expect(rateValueText()).toBe("70.0"); // 桶 0
    expect(screen.getByTestId("speed-chart-tooltip")).toBeInTheDocument();
    fireEvent.keyDown(wrapper, { key: "ArrowRight" });
    expect(rateValueText()).toBe("71.0"); // 桶 1
    fireEvent.keyDown(wrapper, { key: "ArrowLeft" });
    expect(rateValueText()).toBe("70.0"); // 退回桶 0
    fireEvent.keyDown(wrapper, { key: "Enter" });
    expect(screen.getByTestId("speed-clear-pin")).toHaveTextContent("返回区间汇总");
    fireEvent.keyDown(wrapper, { key: "Escape" });
    expect(rateValueText()).toBe("84.2");
    expect(screen.queryByTestId("speed-chart-tooltip")).toBeNull();
  });

  it("图表 aria-label 携带当前模式与单位；越界方向键夹在首末桶", () => {
    render(<Harness buckets={makeBuckets()} mode="stream" />);
    const wrapper = screen.getByTestId("speed-chart");
    expect(wrapper.getAttribute("aria-label")).toContain("TPS");
    wrapper.focus();
    fireEvent.keyDown(wrapper, { key: "ArrowLeft" }); // 从未选状态向左 → 夹在桶 0
    expect(rateValueText()).toBe("70.0");
    for (let i = 0; i < 20; i++) fireEvent.keyDown(wrapper, { key: "ArrowRight" });
    expect(rateValueText()).toBe("81.0"); // 夹在末桶（70+11）
  });
});

// ==== 5. gap：实测线分段、灰虚线独立不可命中、阴影、图例、band 严格断开 ====

describe("缺口渲染与交互", () => {
  it("实测线只在有值相邻桶间连续：null 段把线拆成多段", () => {
    render(<Harness buckets={makeBuckets([5, 6])} mode="stream" />);
    const segs = screen.getAllByTestId("speed-chart-line-segment");
    expect(segs).toHaveLength(2); // [0..4] 与 [7..11]
    expect(segmentSeries(makeBuckets([5, 6]), "stream")).toEqual([
      { from: 0, to: 4 },
      { from: 7, to: 11 },
    ]);
    expect(segmentSeries(makeBuckets(), "stream")).toEqual([{ from: 0, to: 11 }]);
  });

  it("灰虚线连接层独立存在且 pointer-events:none；悬停 gap 桶无值无插值；阴影层与图例在场", () => {
    render(<Harness buckets={makeBuckets([5, 6])} mode="stream" />);
    const connector = screen.getByTestId("speed-chart-gap-connector");
    expect(connector.style.pointerEvents).toBe("none");
    expect(screen.getByTestId("speed-chart-gap-band")).toBeInTheDocument();
    expect(screen.getByTestId("speed-chart-legend").textContent).toContain("虚线仅连接，无样本");

    fireEvent.mouseEnter(hits()[5]);
    const tip = screen.getByTestId("speed-chart-tooltip").textContent ?? "";
    expect(tip).toContain("无有效样本");
    expect(tip).not.toContain("TPS"); // 不给插值数字
  });

  it("gapStyle=band 严格断开：无连接线，阴影层仍在", () => {
    render(<Harness buckets={makeBuckets([5, 6])} mode="stream" gapStyle="band" />);
    expect(screen.queryByTestId("speed-chart-gap-connector")).toBeNull();
    expect(screen.getByTestId("speed-chart-gap-band")).toBeInTheDocument();
  });

  it("无缺口时不渲染连接层与图例", () => {
    render(<Harness buckets={makeBuckets()} mode="stream" />);
    expect(screen.queryByTestId("speed-chart-gap-connector")).toBeNull();
    expect(screen.queryByTestId("speed-chart-legend")).toBeNull();
  });
});

// ==== 6. 几何：x 铺满全范围、y 从 0 起、4–7 标签、单点/全空也有坐标轴 ====

describe("坐标几何", () => {
  it("首末数据点贴绘图区左右边缘（x 按完整 [from,to] 铺满，不挤在样本时刻）", () => {
    render(<Harness buckets={makeBuckets([5, 6])} mode="stream" />);
    const dots = screen.getAllByTestId("speed-chart-dot");
    const xs = dots.map((d) => Number(d.getAttribute("cx")));
    expect(xs[0]).toBe(SPEED_CHART_PAD.l); // 首桶（有值）贴左缘
    expect(xs[xs.length - 1]).toBe(880 - SPEED_CHART_PAD.r); // 末桶（有值）贴右缘
    const labels = screen.getAllByTestId("speed-chart-xlabel");
    expect(Number(labels[0].getAttribute("x"))).toBe(SPEED_CHART_PAD.l); // x 轴刻度也从左缘起
  });

  it("按宽度出 4–7 个 x 标签（1h 与 24h 档都成立；y 从 0 起）", () => {
    const first = render(<Harness buckets={makeBuckets()} mode="stream" />);
    expect(screen.getAllByTestId("speed-chart-xlabel").length).toBeGreaterThanOrEqual(4);
    expect(screen.getAllByTestId("speed-chart-xlabel").length).toBeLessThanOrEqual(7);
    const yLabels = screen.getAllByTestId("speed-chart-ylabel").map((el) => el.textContent);
    expect(yLabels).toContain("0"); // 纵轴从 0 开始
    first.unmount();
    render(<Harness buckets={makeBuckets24()} mode="stream" from={FROM24} to={TO24} />);
    expect(screen.getAllByTestId("speed-chart-xlabel").length).toBeGreaterThanOrEqual(4);
    expect(screen.getAllByTestId("speed-chart-xlabel").length).toBeLessThanOrEqual(7);
    // 纯函数核对四档全在 4–7
    expect(timeTicks(FROM, TO).length).toBeGreaterThanOrEqual(4);
    expect(timeTicks(FROM24, TO24).length).toBeLessThanOrEqual(7);
  });

  it("24h 跨日桶 tooltip 显示日期", () => {
    render(<Harness buckets={makeBuckets24()} mode="stream" from={FROM24} to={TO24} />);
    fireEvent.mouseEnter(screen.getAllByTestId("speed-chart-hit")[3]);
    const tip = screen.getByTestId("speed-chart-tooltip").textContent ?? "";
    expect(tip).toContain("10-26"); // 本地月-日（跨日范围全量带日期）
  });

  it("单点：仍有坐标轴与数据点", () => {
    const single = [makeBuckets()[0]];
    render(<Harness buckets={single} mode="stream" from={FROM} to={FROM + WIDTH_MS} />);
    expect(screen.getAllByTestId("speed-chart-dot")).toHaveLength(1);
    expect(screen.getAllByTestId("speed-chart-ylabel").length).toBeGreaterThanOrEqual(2);
    expect(screen.getAllByTestId("speed-chart-xlabel").length).toBeGreaterThanOrEqual(1);
  });

  it("全部无值：不出实测线，坐标轴仍在，悬停显示无有效样本", () => {
    const allNull = makeBuckets([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]);
    render(<Harness buckets={allNull} mode="stream" />);
    expect(screen.queryAllByTestId("speed-chart-line-segment")).toHaveLength(0);
    expect(screen.getAllByTestId("speed-chart-ylabel").length).toBeGreaterThanOrEqual(2);
    fireEvent.mouseEnter(hits()[0]);
    expect(screen.getByTestId("speed-chart-tooltip").textContent).toContain("无有效样本");
  });
});

// ==== 7. 纯函数：刻度 / 固定快照 ====

describe("chart.ts 纯函数", () => {
  it("niceScale 从 0 起且 max 为易读上界", () => {
    const s = niceScale(86.4);
    expect(s.ticks[0]).toBe(0);
    expect(s.max).toBeGreaterThanOrEqual(86.4);
    expect(s.max).toBe(100);
    expect(s.ticks).toEqual([0, 25, 50, 75, 100]);
    expect(niceScale(0).max).toBeGreaterThan(0); // 全空也有可用坐标
  });

  it("pinSnapshot 拷贝指标（原对象后变不影响快照）", () => {
    const buckets = makeBuckets();
    const bucket = buckets[4];
    const snap = pinSnapshot(bucket, { tool: "zcode" }, "stream");
    expect(snap.metricsSnapshot).toEqual(bucket.metrics);
    expect(snap.metricsSnapshot).not.toBe(bucket.metrics);
    expect(snap.metricsSnapshot.stream).not.toBe(bucket.metrics.stream);
    // 原地改原对象（防御性场景）：快照不动
    bucket.metrics.stream.value = 999;
    expect(snap.metricsSnapshot.stream.value).toBe(74);
  });
});

// ==== 8. Codex：响应累计用时 + 日志近似标记 + TTFT 如实 — ====

describe("Codex 计时口径四格", () => {
  const codexMetrics = bucketMetrics({
    stream: rate(null, 0, 0, 0),
    response: rate(41.3, 1_200, 29_000, 5),
    avgFirstTokenMs: null,
    ttftSampleCount: 0,
    modelMs: 29_000,
  });

  it("method=log-boundary-approx：标签「响应累计用时」+ 一级「日志近似」标记 + TTFT —（不伪造）", () => {
    render(
      <SpeedMetrics
        metrics={codexMetrics}
        mode="response"
        series={{
          tool: "codex",
          model: "gpt-5-codex",
          channel: null,
          method: "log-boundary-approx",
        }}
        context={{ kind: "range-summary" }}
      />
    );
    expect(screen.getByText("响应累计用时")).toBeInTheDocument();
    expect(screen.queryByText(/^累计用时$/)).toBeNull(); // 不是六源标签
    expect(screen.getByTestId("speed-approx-mark")).toHaveTextContent("日志近似");
    expect(screen.getByTestId("speed-first-token")).toHaveTextContent("—"); // brief 判据逐字
    expect(screen.getByTestId("speed-model-duration")).toHaveTextContent("29.0 秒");
    expect(screen.getByTestId("speed-rate-unit")).toHaveTextContent("Token/s");
  });

  it("原生六源（无近似方法）：标签保持「累计用时」，无日志近似标记", () => {
    render(
      <SpeedMetrics
        metrics={codexMetrics}
        mode="stream"
        series={{ tool: "zcode", model: "glm-5.3-flash", channel: null }}
        context={{ kind: "range-summary" }}
      />
    );
    expect(screen.getByText(/^累计用时$/)).toBeInTheDocument();
    expect(screen.queryByText("响应累计用时")).toBeNull();
    expect(screen.queryByTestId("speed-approx-mark")).toBeNull();
  });

  it("值为 null 的格子一律 —：速度、TTFT、工具用时（诚实空态）", () => {
    render(
      <SpeedMetrics
        metrics={bucketMetrics({
          stream: rate(null, 0, 0, 0),
          avgFirstTokenMs: null,
          modelMs: null,
          toolMs: null,
        })}
        mode="stream"
        context={{ kind: "range-summary" }}
      />
    );
    expect(rateValueText()).toBe("—");
    expect(screen.getByTestId("speed-first-token").textContent).toBe("—");
    expect(screen.getByTestId("speed-model-duration").textContent).toBe("—");
    expect(screen.getByTestId("speed-tool-duration").textContent).toBe("—");
  });
});
