// Task 12（2026-10-26 计划）· 速度页面组合、筛选与应用驻留（vitest + jsdom）。
// 判据来源：task-12-brief Step1（workspace_speed_full_flow）+ spec §5.3/§8：
//  * 组合装配：整页**恰好一张** speed-chart；默认区间汇总四格；列表恒客户端排序
//    （2026-10-27 实机反馈 #5：name 升序起步，= tool→model→channel 稳定序）+ 速度列 +
//    有依据的会话数（不称窗口数）；点行切换图表与四格；
//  * 搜索/工具筛选走服务端（invoke 入参断言）；排序三项（组合名/速度/会话数）× 升降序
//    只影响客户端渲染——查询载荷恒 sort:"tool"（服务端稳定基序）；
//  * 两模式全界面同步：切模式一次性更新列表速度列、排序载荷、主数字名称/单位、图表
//    纵轴单位、有效 n——全部由 speed-mode 的 data-mode 根驱动；Codex 临时 fallback
//    显著提示 + 流式按钮禁用 + 偏好不被改写（切回原生源即恢复 stream）；
//  * 悬停/固定生命周期：hover 切四格、移开恢复汇总；点击固定快照（refetch 换数据不漂移）；
//    Esc / 返回区间汇总 / 切范围 / 切组合解除；**滚动窗口**（asOf/rangeFrom/rangeTo 每
//    5 秒前移、同档位）不得冲掉固定与悬停（clearSignalKey 裁决）；
//  * 应用驻留：页签切走（宿主保持挂载、active=false）⇒ 速度查询停（≥60 秒零新请求）+
//    TPS 视图租约释放（最后一条心跳 visible=false）；切回恢复且本地状态（范围/模式/
//    选中组合）全部保留——运行中切页状态不重置（spec §2.6）；
//  * 范围独立：切用量区间零 tps_speed_dashboard 请求；
//  * 来源面板：采集管理开合 + snapshot.sourceStates 注入；面板自身不加轮询；
//  * 空态：搜索无匹配 ≠ 无任何组合；全空桶保持选中出 — 四格与空图（轴仍在）；
//    本地选中被筛掉时跟随服务端 selectedSeries 回退。
// 纪律：真实 hook 路径——invoke 走 tests/setup.ts 接线的 tauriInvokeMock；不 mock
// react-query；asOf/范围标签一律来自服务端快照（视图不猜 now）。
import { useState } from "react";
import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, afterEach, describe, expect, it, vi } from "vitest";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";

// jsdom 未实现 matchMedia（theme 相关首帧判定需要；与其余 workspace/usage 测试同模式）
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
import { SpeedDashboard } from "@/components/speed/SpeedDashboard";
import { SPEED_KEY } from "@/lib/query/queries/speed";
import { mockSpeedDashboard, resetTpsMockState, emptySpeedMetrics } from "@/lib/usage/mockFixtures";
import { setMockSpeedDashboard, tauriInvokeMock } from "../msw/tauriMocks";
import { useUsageDashboardQuery } from "@/lib/query/queries/usage";
import type { SpeedDashboardQuery, SeriesKey } from "@/types/speed";

// ==== 基建 ====

function makeClient() {
  return new QueryClient({ defaultOptions: { queries: { retry: false } } });
}

/** 挂 SpeedDashboard（active 由测试驱动——模拟 Task 7 宿主「保持挂载、active 门控」）。 */
function renderSpeed() {
  const client = makeClient();
  let setActiveExternal: ((v: boolean) => void) | null = null;
  function Host() {
    const [active, setActive] = useState(true);
    setActiveExternal = setActive;
    return (
      <QueryClientProvider client={client}>
        <SpeedDashboard active={active} />
      </QueryClientProvider>
    );
  }
  const utils = render(<Host />);
  return {
    ...utils,
    client,
    setActive: (v: boolean) =>
      act(() => {
        setActiveExternal?.(v);
      }),
  };
}

function callsOf(cmd: string): number {
  return tauriInvokeMock.mock.calls.filter(([c]) => c === cmd).length;
}

/** 最近一次 tps_speed_dashboard 的 query 入参。 */
function lastSpeedQuery(): SpeedDashboardQuery {
  const call = tauriInvokeMock.mock.calls.filter(([c]) => c === "tps_speed_dashboard").at(-1) as
    [string, { query?: SpeedDashboardQuery }] | undefined;
  expect(call).toBeDefined();
  return call?.[1]?.query as SpeedDashboardQuery;
}

/** 最后一次 tps_view_heartbeat 的入参（租约续/放的真值源；workspace 测试同款）。 */
function lastHeartbeat(): { visible?: boolean } | undefined {
  const call = tauriInvokeMock.mock.calls.filter(([c]) => c === "tps_view_heartbeat").at(-1) as
    [string, { visible?: boolean }] | undefined;
  return call?.[1];
}

/** 详情标识可选框的**选中项**文本（tool · model [· channel]；选项即全量组合名）。 */
function detailTitle(): string {
  const sel = screen.getByTestId("speed-detail-title") as HTMLSelectElement;
  return sel.options[sel.selectedIndex]?.textContent ?? "";
}

/** 宽布局组合列表（窄布局 select 之外的那份行表）；空列表返回 []（不抛错）。 */
function listRows(): HTMLElement[] {
  const list = screen.queryByTestId("speed-series-list");
  return list ? within(list).queryAllByTestId("speed-series-row") : [];
}

function rowTools(): string[] {
  return listRows().map((r) => r.getAttribute("data-tool") ?? "");
}

beforeEach(async () => {
  localStorage.clear();
  await i18n.changeLanguage("zh");
  tauriInvokeMock.mockClear();
  setMockSpeedDashboard(null);
  resetTpsMockState();
});

afterEach(() => {
  vi.useRealTimers();
});

// ==== 1. 装配：单图、四格、服务端顺序、选行切换 ====

describe("组合装配", () => {
  it("workspace_speed_full_flow：恰好一张图；默认区间汇总四格；列表服务端顺序直出", async () => {
    renderSpeed();
    await screen.findByTestId("speed-metrics");

    // brief 判据逐字：整页只有一张图
    expect(screen.getAllByTestId("speed-chart")).toHaveLength(1);

    // 默认区间汇总：上下文行 + 四格齐备（当前模式速度/首Token/累计用时/工具累计用时）
    expect(screen.getByTestId("speed-metrics-context").textContent).toContain("区间汇总");
    for (const id of [
      "speed-tile-rate",
      "speed-tile-first-token",
      "speed-tile-model",
      "speed-tile-tool",
    ]) {
      expect(screen.getByTestId(id)).toBeInTheDocument();
    }

    // 列表 = 客户端 name 升序（= tool→model→channel 稳定序，与查询载荷 sort:"tool" 的
    // 服务端基序同序）：claude→codex→dsh→kimi→opencode→workbuddy→zcode
    expect(rowTools()).toEqual([
      "claude",
      "codex",
      "dsh",
      "kimi",
      "opencode",
      "workbuddy",
      "zcode",
    ]);

    // 速度列（stream 默认）+ 有依据的会话数（三会话同 series 汇总 = 3，不称窗口数）
    const zcodeRow = listRows().find((r) => r.getAttribute("data-tool") === "zcode")!;
    expect(zcodeRow.textContent).toContain("86.4");
    expect(zcodeRow.textContent).toContain("3");
    expect(screen.getByTestId("speed-list-header").textContent).toContain("有依据的会话数");

    // 点行切换：图表与四格跟随所选 series（服务端为它出桶）
    fireEvent.click(listRows().find((r) => r.getAttribute("data-tool") === "dsh")!);
    await waitFor(() =>
      expect(lastSpeedQuery().selection).toEqual({
        tool: "dsh",
        model: "doubao-seed-1.6",
        channel: null,
        method: "client-native",
      } satisfies SeriesKey)
    );
    await waitFor(() => expect(detailTitle()).toContain("doubao-seed-1.6")); // 详情标题切换
    await waitFor(() => expect(screen.getByTestId("speed-rate-value").textContent).toBe("41.7"));
  });
});

// ==== 2. 搜索 / 工具筛选 / 排序（客户端排序；查询载荷恒 sort:"tool"）====

describe("筛选与排序", () => {
  it("工具筛选走服务端 toolId；排序三项只影响客户端渲染，查询载荷恒 sort:'tool'", async () => {
    renderSpeed();
    await screen.findByTestId("speed-metrics");

    fireEvent.change(screen.getByTestId("speed-tool-filter"), { target: { value: "zcode" } });
    await waitFor(() => expect(lastSpeedQuery().toolId).toBe("zcode"));
    await waitFor(() => expect(rowTools()).toEqual(["zcode"]));

    fireEvent.change(screen.getByTestId("speed-tool-filter"), { target: { value: "" } });
    await waitFor(() => expect(lastSpeedQuery().toolId).toBeNull());

    // 表头即排序控件：三个可点表头，默认组合名升序（速度/会话数表头 aria-sort=none）
    const headerBtn = (key: "name" | "rate" | "sessions") =>
      screen.getByTestId(`speed-sort-header-${key}`) as HTMLButtonElement;
    expect(headerBtn("name").getAttribute("aria-sort")).toBe("ascending");
    expect(headerBtn("rate").getAttribute("aria-sort")).toBe("none");
    expect(headerBtn("sessions").getAttribute("aria-sort")).toBe("none");

    // 点「速度」表头：切键归位默认降序，客户端按 stream 降序（92.0 → … → null 末位）
    fireEvent.click(headerBtn("rate"));
    await waitFor(() =>
      expect(rowTools()).toEqual([
        "opencode",
        "zcode",
        "kimi",
        "claude",
        "workbuddy",
        "dsh",
        "codex",
      ])
    );
    expect(lastSpeedQuery().sort).toBe("tool"); // 服务端基序不受 UI 排序影响
    expect(headerBtn("rate").getAttribute("aria-sort")).toBe("descending");

    // 同键再点：翻升序（低→高），null 仍末位
    fireEvent.click(headerBtn("rate"));
    await waitFor(() =>
      expect(rowTools()).toEqual([
        "dsh",
        "workbuddy",
        "claude",
        "kimi",
        "zcode",
        "opencode",
        "codex",
      ])
    );
    expect(headerBtn("rate").getAttribute("aria-sort")).toBe("ascending");

    // 点「会话数」表头：切键归位默认降序（多→少）
    fireEvent.click(headerBtn("sessions"));
    await waitFor(() => expect(headerBtn("sessions").getAttribute("aria-sort")).toBe("descending"));
    expect(rowTools()).toHaveLength(7);
    expect(lastSpeedQuery().sort).toBe("tool"); // 全程不变
    // 点「组合名」表头：切键归位升序（稳定序）
    fireEvent.click(headerBtn("name"));
    await waitFor(() =>
      expect(rowTools()).toEqual([
        "claude",
        "codex",
        "dsh",
        "kimi",
        "opencode",
        "workbuddy",
        "zcode",
      ])
    );
    expect(headerBtn("name").getAttribute("aria-sort")).toBe("ascending");
  });

  it("窄宽布局同一份行表 DOM：无下拉/搜索/排序控件、行名 block+truncate 且悬浮全名走 aria-label、左列轨加宽、order 双轨", async () => {
    renderSpeed();
    await screen.findByTestId("speed-metrics");

    // 三轮反馈：下拉/搜索/排序控件全部退场（排序走表头），列表容器常在且未 hidden
    expect(screen.queryByTestId("speed-series-select")).toBeNull();
    expect(screen.queryByTestId("speed-search")).toBeNull();
    expect(screen.queryByTestId("speed-sort")).toBeNull();
    expect(screen.queryByTestId("speed-sort-direction")).toBeNull();
    const list = screen.getByTestId("speed-series-list") as HTMLElement;
    expect(list.className).not.toContain("hidden");
    expect(list.className).toContain("max-h-56");
    expect(list.className).toContain("lg:sticky");
    expect(list.className).toContain("lg:top-0");
    expect(list.className).toContain("lg:max-h-[calc(100vh-190px)]");
    expect(list.className).not.toContain("lg:max-h-none");
    // 整页只滚一处：根容器滚动、详情 section 无内层滚动
    expect(screen.getByTestId("speed-dashboard").className).toContain("overflow-y-auto");
    const detailSec = screen.getByTestId("speed-detail").closest("section") as HTMLElement;
    expect(detailSec.className).not.toContain("overflow-y-auto");

    // 二轮实机反馈 ③：窄窗详情在上（order-1）、列表在下（order-2）；宽窗恢复左列表/右详情
    // （order 类在列表 pane（speed-list-pane）与详情 section 上；DOM 顺序不动）
    const pane = screen.getByTestId("speed-list-pane") as HTMLElement;
    expect(pane.className).toContain("order-2");
    expect(pane.className).toContain("lg:order-1");
    const detailSection = screen.getByTestId("speed-detail").closest("section") as HTMLElement;
    expect(detailSection.className).toContain("order-1");
    expect(detailSection.className).toContain("lg:order-2");

    // 行名单元格 title = 完整组合名（悬浮可读全名）
    const zcodeRow = listRows().find((r) => r.getAttribute("data-tool") === "zcode")!;
    // 名单元：内层 block + truncate（inline 盒上 truncate 不生效的回归锁）；全名走 aria-label
    const nameCell = zcodeRow.querySelector("span.group.relative")!;
    expect(nameCell.getAttribute("aria-label")).toContain("ZCode · glm"); // toolLabel · model 全文
    const nameText = nameCell.firstElementChild!;
    expect(nameText.className).toContain("block");
    expect(nameText.className).toContain("truncate");

    // 宽窗左列轨：minmax(280px,340px)（列表 pane 的父级 grid）
    const gridEl = screen.getByTestId("speed-list-pane").parentElement as HTMLElement;
    expect(gridEl.className).toContain("lg:grid-cols-[minmax(280px,340px)_minmax(0,1fr)]");
  });

  it("口径说明三行 footnote：渲染在图例下方（DOM 顺序）、含三行口径；speed-mode-hint 段落已删", async () => {
    renderSpeed();
    const footnote = await screen.findByTestId("speed-chart-footnote");
    const text = footnote.textContent ?? "";
    // 三行文案（i18n speed.modeHint.line1/2/3）
    expect(text).toContain("首个有效输出到生成完成");
    expect(text).toContain("不含首 Token 等待");
    expect(text).toContain("首 Token 等待 + 生成");
    expect(text).toContain("差额即等待");
    expect(text).toContain("Codex 仅有响应口径");
    // 三行各占一行 <p>
    expect(footnote.querySelectorAll("p")).toHaveLength(3);
    // DOM 顺序：footnote 在图例（speed-chart-legend）之后（mock 桶中部含空桶 ⇒ 图例在场）
    const legend = screen.getByTestId("speed-chart-legend");
    expect(
      legend.compareDocumentPosition(footnote) & Node.DOCUMENT_POSITION_FOLLOWING
    ).toBeTruthy();
    // 旧 speed-mode-hint 段落已删除（口径说明整体迁入图注）
    expect(screen.queryByTestId("speed-mode-hint")).toBeNull();
    // 模式按钮 title（口径定义 tooltip）保留
    expect(screen.getByTestId("speed-mode-stream").getAttribute("title") ?? "").toContain(
      "不含首 Token 等待"
    );
    expect(screen.getByTestId("speed-mode-response").getAttribute("title") ?? "").toContain(
      "总秒数"
    );
  });
});

// ==== 3. 双模式全界面同步 + Codex 临时 fallback ====

describe("口径切换与 fallback", () => {
  it("切 response：列表速度列 / 排序载荷 / 主数字单位 / 图轴单位 / 有效 n 一次同步", async () => {
    renderSpeed();
    await screen.findByTestId("speed-metrics");

    fireEvent.click(screen.getByTestId("speed-mode-response"));
    // brief 判据逐字：data-mode 根同步
    expect(screen.getByTestId("speed-mode")).toHaveAttribute("data-mode", "response");

    await waitFor(() => expect(lastSpeedQuery().metricMode).toBe("response"));
    // 列表速度列：zcode 行 stream 86.4 → response 62.1
    await waitFor(() => {
      const row = listRows().find((r) => r.getAttribute("data-tool") === "zcode");
      expect(row?.textContent ?? "").toContain("62.1");
    });
    // 主数字/单位/n：默认选中 = 服务端排序第一（claude），其 response 值 58.8 · n=7
    expect(screen.getByTestId("speed-rate-value").textContent).toBe("58.8");
    expect(screen.getByTestId("speed-rate-unit")).toHaveTextContent("Token/s"); // 单位
    expect(screen.getByTestId("speed-chart").getAttribute("aria-label")).toContain("Token/s");
    expect(screen.getByTestId("speed-metrics-context").textContent).toContain("n=7"); // 有效 n
  });

  it("Codex 临时 fallback + 速度排序：列表恒按显示口径客户端重排，切回原生源恢复 stream 序", async () => {
    // 列表恒客户端排序（2026-10-27 实机反馈 #5）：fallback（effective ≠ preferred）时
    // 排序口径自动跟随显示口径，不再有「服务端按偏好口径排、列表显示回退口径」的错排。
    renderSpeed();
    await screen.findByTestId("speed-metrics");

    // 速度排序（stream 显示口径）降序：92.0 → 86.4 → 74.9 → 71.2 → 58.3 → 41.7 → codex(null 末位)
    fireEvent.click(screen.getByTestId("speed-sort-header-rate"));
    await waitFor(() =>
      expect(rowTools()).toEqual([
        "opencode",
        "zcode",
        "kimi",
        "claude",
        "workbuddy",
        "dsh",
        "codex",
      ])
    );

    // 选中 Codex ⇒ 显示口径临时回退 response：列表以显示口径客户端重排，
    // 与速度列的 response 值降序逐行一致（62.1 → 61.3 → 58.8 → 55.6 → 47.9 → 41.3 → 33.2；
    // Codex 不再因 stream=null 被钉在末位，而按其 response 值 41.3 落位）
    fireEvent.click(listRows().find((r) => r.getAttribute("data-tool") === "codex")!);
    await waitFor(() =>
      expect(screen.getByTestId("speed-mode")).toHaveAttribute("data-mode", "response")
    );
    await waitFor(() =>
      expect(rowTools()).toEqual([
        "zcode",
        "opencode",
        "claude",
        "kimi",
        "workbuddy",
        "codex",
        "dsh",
      ])
    );

    // 切回支持流式的原生源 ⇒ effective = preferred，恢复 stream 降序原序
    fireEvent.click(listRows().find((r) => r.getAttribute("data-tool") === "zcode")!);
    await waitFor(() =>
      expect(screen.getByTestId("speed-mode")).toHaveAttribute("data-mode", "stream")
    );
    await waitFor(() =>
      expect(rowTools()).toEqual([
        "opencode",
        "zcode",
        "kimi",
        "claude",
        "workbuddy",
        "dsh",
        "codex",
      ])
    );
  });

  it("Codex 临时 fallback：显著提示 + 流式按钮禁用 + 单图 + 偏好不被改写", async () => {
    renderSpeed();
    await screen.findByTestId("speed-metrics");

    fireEvent.click(listRows().find((r) => r.getAttribute("data-tool") === "codex")!);
    await waitFor(() => expect(detailTitle()).toContain("gpt-5-codex"));

    // brief 判据逐字（fallback 全界面同步）
    expect(screen.getByTestId("speed-mode")).toHaveAttribute("data-mode", "response");
    expect(screen.getAllByTestId("speed-chart")).toHaveLength(1);

    // 显著一级提示 + 流式按钮禁用（带解释）
    expect(screen.getByTestId("speed-fallback-notice").textContent).toContain(
      "仅支持响应吞吐（含等待）"
    );
    const streamBtn = screen.getByTestId("speed-mode-stream");
    expect(streamBtn).toBeDisabled();
    expect(streamBtn.getAttribute("title") ?? "").toContain("流式生成窗");

    // 偏好不被改写：切回支持流式的原生源 ⇒ 自动恢复 stream
    fireEvent.click(listRows().find((r) => r.getAttribute("data-tool") === "zcode")!);
    await waitFor(() => expect(detailTitle()).toContain("glm-5.3-flash"));
    expect(screen.getByTestId("speed-mode")).toHaveAttribute("data-mode", "stream");
    expect(screen.queryByTestId("speed-fallback-notice")).toBeNull();
  });
});

// ==== 4. 悬停 / 固定生命周期（含滚动窗口裁决）====

describe("悬停与固定", () => {
  it("hover 切四格、移开恢复区间汇总", async () => {
    renderSpeed();
    await screen.findByTestId("speed-chart");
    const hits = screen.getAllByTestId("speed-chart-hit");
    fireEvent.mouseEnter(hits[0]!);
    expect(screen.getByTestId("speed-rate-value").textContent).toBe("78.0"); // mock 首桶 stream
    fireEvent.mouseLeave(screen.getByTestId("speed-chart-svg"));
    expect(screen.getByTestId("speed-metrics-context").textContent).toContain("区间汇总");
  });

  it("固定=快照：refetch 换数据不漂移；Esc / 返回区间汇总 / 切范围 / 切组合解除", async () => {
    const { client } = renderSpeed();
    await screen.findByTestId("speed-chart");
    let doubled = false;
    setMockSpeedDashboard((query) => {
      const snap = mockSpeedDashboard(query);
      if (!doubled) return snap;
      return {
        ...snap,
        buckets: snap.buckets.map((b) => ({
          ...b,
          metrics: { ...b.metrics, stream: { ...b.metrics.stream, value: 200.5 } },
        })),
      };
    });

    fireEvent.click(screen.getAllByTestId("speed-chart-hit")[3]!);
    expect(screen.getByTestId("speed-rate-value").textContent).toBe("88.2"); // 78+3×3.4
    expect(screen.getByTestId("speed-clear-pin")).toBeInTheDocument();

    // refetch 换数据：固定读数（快照）不变
    doubled = true;
    await act(async () => {
      await client.invalidateQueries({ queryKey: SPEED_KEY });
    });
    expect(screen.getByTestId("speed-rate-value").textContent).toBe("88.2");
    expect(screen.getByTestId("speed-clear-pin")).toBeInTheDocument();

    // Esc 解除
    fireEvent.keyDown(screen.getByTestId("speed-chart"), { key: "Escape" });
    expect(screen.queryByTestId("speed-clear-pin")).toBeNull();

    // 再固定 → 「返回区间汇总」解除
    fireEvent.click(screen.getAllByTestId("speed-chart-hit")[3]!);
    fireEvent.click(screen.getByTestId("speed-clear-pin"));
    expect(screen.queryByTestId("speed-clear-pin")).toBeNull();

    // 再固定 → 切范围解除（等新数据到位后断言，固定态确实被清而非被 loading 掩盖）
    fireEvent.click(screen.getAllByTestId("speed-chart-hit")[3]!);
    fireEvent.click(screen.getByTestId("speed-range-15m"));
    await waitFor(() => expect(lastSpeedQuery().range).toBe("15m"));
    await screen.findByTestId("speed-chart");
    expect(screen.queryByTestId("speed-clear-pin")).toBeNull();
    expect(screen.getByTestId("speed-metrics-context").textContent).toContain("区间汇总");

    // 再固定 → 切组合解除
    fireEvent.click(screen.getAllByTestId("speed-chart-hit")[3]!);
    expect(screen.getByTestId("speed-clear-pin")).toBeInTheDocument();
    fireEvent.click(listRows().find((r) => r.getAttribute("data-tool") === "kimi")!);
    await waitFor(() => expect(detailTitle()).toContain("kimi-k2"));
    await screen.findByTestId("speed-chart");
    expect(screen.queryByTestId("speed-clear-pin")).toBeNull();
  });

  it("滚动窗口（asOf/rangeFrom/rangeTo 前移、同档位）：固定与悬停都不丢（clearSignalKey 裁决）", async () => {
    const { client } = renderSpeed();
    await screen.findByTestId("speed-chart");
    let shift = 0;
    setMockSpeedDashboard((query) => mockSpeedDashboard(query, Date.now() + shift));

    fireEvent.mouseEnter(screen.getAllByTestId("speed-chart-hit")[0]!);
    expect(screen.getByTestId("speed-chart-guide")).toBeInTheDocument();
    fireEvent.click(screen.getAllByTestId("speed-chart-hit")[3]!);
    expect(screen.getByTestId("speed-rate-value").textContent).toBe("88.2");

    // 模拟 5 秒轮询：窗口整体前移（同 1h 档位，clearSignalKey 不变）
    shift += 5_000;
    await act(async () => {
      await client.invalidateQueries({ queryKey: SPEED_KEY });
    });
    // 固定读数与悬停竖线都还在
    expect(screen.getByTestId("speed-rate-value").textContent).toBe("88.2");
    expect(screen.getByTestId("speed-clear-pin")).toBeInTheDocument();
    expect(screen.getByTestId("speed-chart-guide")).toBeInTheDocument();
  });
});

// ==== 5. 应用驻留：active=false 停轮询 + 租约释放；切回恢复且状态保留 ====

describe("应用驻留", () => {
  it("切走：速度查询停（≥60 秒零新请求）+ 心跳 visible=false；切回恢复且状态保留", async () => {
    const { setActive } = renderSpeed();
    await screen.findByTestId("speed-metrics");
    await waitFor(() => expect(callsOf("tps_speed_dashboard")).toBeGreaterThan(0));
    await waitFor(() => expect(lastHeartbeat()?.visible).toBe(true));

    // 改动本地状态（范围/选中组合/模式），随后切走（页面保持挂载——Task 7 宿主语义）
    fireEvent.click(screen.getByTestId("speed-range-15m"));
    await waitFor(() => expect(lastSpeedQuery().range).toBe("15m"));
    await screen.findByTestId("speed-chart"); // 15m 快照到位
    fireEvent.click(listRows().find((r) => r.getAttribute("data-tool") === "dsh")!);
    await waitFor(() => expect(lastSpeedQuery().selection?.tool).toBe("dsh"));

    await setActive(false);
    await act(async () => {
      await Promise.resolve();
    });
    // 页签切走即释放租约（最后一条心跳 visible=false），而不是带着 90s TTL 干等
    expect(lastHeartbeat()?.visible).toBe(false); // brief 判据逐字：切离速度页

    vi.useFakeTimers({ now: new Date(Date.now() + 61_000), shouldAdvanceTime: true });
    const speedAtHide = callsOf("tps_speed_dashboard");
    expect(speedAtHide).toBeGreaterThan(0);
    await act(async () => {
      window.dispatchEvent(new Event("visibilitychange"));
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(callsOf("tps_speed_dashboard")).toBe(speedAtHide); // 隐藏页零新请求
    vi.useRealTimers();

    // 切回：轮询与租约恢复，本地状态全部保留
    await setActive(true);
    await waitFor(() => expect(callsOf("tps_speed_dashboard")).toBeGreaterThan(speedAtHide));
    await waitFor(() => expect(lastHeartbeat()?.visible).toBe(true));
    expect(screen.getByTestId("speed-range-15m")).toHaveAttribute("aria-pressed", "true");
    expect(detailTitle()).toContain("doubao-seed-1.6"); // 选中组合保留
  });
});

// ==== 6. 范围独立：切用量区间零速度请求 ====

describe("用量范围独立", () => {
  it("切用量 range 只发 usage 域请求，零新增 tps_speed_dashboard", async () => {
    const { client } = renderSpeed();
    await screen.findByTestId("speed-metrics");
    const speedAtStart = callsOf("tps_speed_dashboard");
    expect(speedAtStart).toBeGreaterThan(0);

    function UsageRangeHarness() {
      const [preset, setPreset] = useState<"today" | "last7d">("today");
      useUsageDashboardQuery({ preset }, "tool");
      return (
        <button data-testid="usage-range-switch" onClick={() => setPreset("last7d")}>
          switch
        </button>
      );
    }
    render(<UsageRangeHarness />, {
      wrapper: ({ children }: { children: ReactNode }) => (
        <QueryClientProvider client={client}>{children}</QueryClientProvider>
      ),
    });
    await waitFor(() => expect(callsOf("usage_dashboard")).toBeGreaterThan(0));
    fireEvent.click(screen.getByTestId("usage-range-switch"));
    await waitFor(() => expect(callsOf("usage_dashboard")).toBeGreaterThan(1));
    expect(callsOf("tps_speed_dashboard")).toBe(speedAtStart); // 速度域纹丝不动
  });
});

// ==== 7. 来源面板 ====

describe("采集管理", () => {
  it("开合面板并注入 snapshot.sourceStates；面板自身不加轮询", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    renderSpeed();
    await screen.findByTestId("speed-metrics");
    const speedAtOpen = callsOf("tps_speed_dashboard");

    fireEvent.click(screen.getByTestId("speed-sources-open"));
    await screen.findByTestId("speed-sources-panel");
    // 来源一级来自 dashboard 快照（七源）
    for (const tool of ["codex", "zcode", "claude"]) {
      expect(screen.getByTestId(`speed-source-${tool}`)).toBeInTheDocument();
    }
    // 面板自身零取数：排空微任务后速度请求数不增（页面自己的 5 秒节奏不算面板的）
    await act(async () => {
      await Promise.resolve();
    });
    expect(callsOf("tps_speed_dashboard")).toBe(speedAtOpen);

    // 关闭（Radix Presence 关闭动画排空后面板卸载——speed-sources 测试同款手法）
    fireEvent.click(document.querySelector('[data-slot="dialog-close"]')!);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(1_000);
    });
    expect(screen.queryByTestId("speed-sources-panel")).toBeNull();
  });
});

// ==== 8. 空态 ====

describe("空态", () => {
  it("全空桶：保持选中、— 四格、空图（轴仍在）", async () => {
    setMockSpeedDashboard((query) => {
      const snap = mockSpeedDashboard(query);
      const target = snap.series[2]!; // dsh
      return {
        ...snap,
        // 桶全空 ⇒ 区间汇总同源为空（后端同一聚合：n=0 ⇒ value=null，不冒充 0 速度）
        series: snap.series.map((s) => (s === target ? { ...s, metrics: emptySpeedMetrics() } : s)),
        selectedSeries: target.seriesKey,
        buckets: snap.buckets.map((b) => ({ ...b, metrics: emptySpeedMetrics() })),
      };
    });
    renderSpeed();
    await screen.findByTestId("speed-chart");
    expect(screen.getByTestId("speed-rate-value").textContent).toBe("—");
    expect(screen.getAllByTestId("speed-chart-ylabel").length).toBeGreaterThanOrEqual(2); // 轴仍在
    expect(screen.queryAllByTestId("speed-chart-line-segment")).toHaveLength(0);
    // 选中保持：服务端 selectedSeries 指向的组合仍是详情对象
    expect(detailTitle()).toContain("doubao-seed-1.6");
  });

  it("本地选中被筛掉：跟随服务端 selectedSeries 回退并同步本地 selection", async () => {
    renderSpeed();
    await screen.findByTestId("speed-metrics");
    fireEvent.click(listRows().find((r) => r.getAttribute("data-tool") === "zcode")!);
    await waitFor(() => expect(lastSpeedQuery().selection?.tool).toBe("zcode"));

    // 工具筛选把 zcode 筛掉 ⇒ 服务端回退选中（排序后第一项），本地 selection 跟随
    fireEvent.change(screen.getByTestId("speed-tool-filter"), { target: { value: "kimi" } });
    await waitFor(() => expect(lastSpeedQuery().selection?.tool).toBe("kimi"));
    await waitFor(() => expect(detailTitle()).toContain("kimi-k2"));
  });
});
