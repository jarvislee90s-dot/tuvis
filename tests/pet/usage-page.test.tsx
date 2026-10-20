// Task 6（计划②）· 大看板首屏 `src/pages/usage-dashboard.tsx`（8 用例；第 8 条是 2026-10-06
// 用户裁决补的「没量到 token 的窗口不印 0」，第三轮补了 `用户输入(估)` 恢复 KEEP 的两条分支）。
// 判据来源：spec P1（hero + 7 行固定顺序 + 页脚口径）、P8（hero 完整千分位 / 命中率 1 位小数 /
// 环比对比段 / 空态不显示 0 / 三态）、P6（五档 + 自定义 31 天截断常驻提示）、计划② §3 第 5/8/17/18
// 条与 Task 6 步骤 5/6/7/13。
//
// 纪律：
//  * 语言固定 zh：`src/i18n/index.ts` 的 `fallbackLng: "en"` + detector（jsdom navigator 是 en-US）
//    ⇒ 默认英文；全角「（上一周期」模板只有 zh 有（en 是半角 + 前导空格）。
//  * 日期期望值**按运行时今天做日历加法**推算（本地 `setDate`，不写死日期、不用毫秒常数）；
//    时刻只断 `/\d{2}:\d{2}:\d{2}/` 形态 ⇒ 断言与本机时区/日历无关。
//  * 独立 cache 且 `retry: false`：真实 App 的 queryClient 默认 `retry: 2`（1s + 2s 退避），本文件
//    只验页面三态分支，重试策略不是本任务的产物。
//  * `invoke` 走 tests/setup.ts 接进 `@tauri-apps/api/core` 的那个单例（① 的 mock case → ② 同源夹具），
//    不另起 `vi.mock("@tauri-apps/api/core")`（那会绕开夹具）。
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";

// jsdom 未实现 matchMedia（theme-provider 的首帧主题判定会用到；与 petSettings 同模式）
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
import UsageDashboardPage from "@/pages/usage-dashboard";
import { UsageSummary } from "@/components/usage/UsageSummary";
import { tauriInvokeMock } from "../msw/tauriMocks";
import { EM_DASH, fmtTokens } from "@/lib/usage/format";
import { mockUsageDashboard } from "@/lib/usage/mockFixtures";

type InvokeCall = [string, Record<string, unknown> | undefined];

/** setup.ts 把 invoke 接到了这个 vi.fn 上（每用例后清空调用记录） */
function callsOf(cmd: string): InvokeCall[] {
  return tauriInvokeMock.mock.calls.filter(([c]) => c === cmd) as InvokeCall[];
}

/** 本地日键 `YYYY-MM-DD`（测试自算，不 import 被验实现；不走 toISOString 的 UTC 日） */
function dayKey(d: Date): string {
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}`;
}

/** 日历加法（与实现同口径：`setDate`，不用毫秒推算——夏令时回拨日不是 86_400_000 ms） */
function addDays(d: Date, n: number): Date {
  const next = new Date(d.getFullYear(), d.getMonth(), d.getDate());
  next.setDate(next.getDate() + n);
  return next;
}

function renderPage() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const wrapper = ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={client}>{children}</QueryClientProvider>
  );
  return render(<UsageDashboardPage />, { wrapper });
}

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

describe("usage-page（计划② Task 6：大看板首屏）", () => {
  beforeEach(() => localStorage.clear());

  it("1. hero 万/亿缩写 + 7 行固定顺序 + 命中率环比段 + asOf 时刻形态", async () => {
    renderPage();
    // 夹具四工具求和：ΣrequestTotal 1,954,268 + Σoutput 82,713 = 2,036,981
    // 新显示口径：hero 也用万/亿缩写（用户 2026-10-19 覆盖旧完整千分位约定）
    expect(await screen.findByTestId("usage-hero")).toHaveTextContent("203.70万");
    expect(screen.getByTestId("usage-hero")).not.toHaveTextContent("2,036,981");

    expect(
      screen.getAllByTestId(/^usage-grid-/).map((el) => el.getAttribute("data-testid"))
    ).toEqual([
      "usage-grid-requestInput",
      "usage-grid-cacheRead",
      "usage-grid-cacheMiss",
      "usage-grid-output",
      "usage-grid-hitRate",
      "usage-grid-requests",
      "usage-grid-asOf",
    ]);
    // V2 删除「用户输入(估)」；2026-10-20 Task 8 再删「缓存写入（解释）」details 块
    expect(screen.queryByTestId("usage-grid-userEst")).toBeNull();
    expect(screen.queryByTestId("usage-cache-write")).toBeNull();

    // 常规 token 数值走「万」缩写；不再挂完整千分位原始值 tooltip
    const reqRow = screen.getByTestId("usage-grid-requestInput");
    expect(reqRow).toHaveTextContent("195.43万");
    expect(reqRow.querySelector("[title='1,954,268']")).toBeNull();

    // 未缓存命中输入 = requestTotal - cacheRead（与 hero 同口径的派生行）
    const cacheMiss = screen.getByTestId("usage-grid-cacheMiss");
    expect(cacheMiss).toHaveTextContent(
      fmtTokens(
        mockUsageDashboard({ preset: "last7d" }, "tool").totals.requestTotal -
          mockUsageDashboard({ preset: "last7d" }, "tool").totalsBuckets.cacheRead
      )
    );

    // 命中率行：当前值 1 位小数 + 环比段**只剩差值**（2026-10-06 用户裁决「同比冗余字太多」）
    const hitRow = screen.getByTestId("usage-grid-hitRate");
    expect(hitRow).toHaveTextContent("67.4%");
    expect(hitRow).toHaveTextContent(/（[+±-]\d+\.\d pt）/);
    // 上一周期的值不再进屏上文案，但**没有丢**——挂在环比格的 `title` 里（usage.compareHint）
    expect(hitRow.querySelector("[title*='环比上一等长周期']")).toBeTruthy();

    expect(screen.getByTestId("usage-grid-asOf")).toHaveTextContent(/\d{2}:\d{2}:\d{2}/);

    // **趋势区也必须有环比**（spec P8：「指标网格各项**与趋势区**都显示对比值」）。
    // V2 的 B 来自新 `usage_trend`（hero=140 / 上期 hero=100）⇒ 区间合计 +40.0%。
    expect(screen.getByTestId("usage-trend-compare")).toHaveTextContent("区间合计 （+40.0%）");
  });

  it("2. 默认档 last7d；切「近 30 天」只把 preset 交给后端重查（前端不自行推算窗口）", async () => {
    renderPage();
    await screen.findByTestId("usage-hero");
    expect(callsOf("usage_dashboard")[0][1]).toEqual({
      range: { preset: "last7d" },
      groupBy: "tool",
    });

    fireEvent.click(screen.getByText("近 30 天"));
    await waitFor(() =>
      expect(
        callsOf("usage_dashboard").some(
          (c) => JSON.stringify(c[1]?.range) === JSON.stringify({ preset: "last30d" })
        )
      ).toBe(true)
    );
    expect(screen.getByText("近 30 天")).toHaveAttribute("aria-pressed", "true");

    // 预设四档**只发 preset**：窗口边界（含上一等长周期）由后端裁定（§3 第 17 条 / 契约要点 1）
    for (const call of callsOf("usage_dashboard")) {
      expect(call[1]?.range).not.toHaveProperty("from");
      expect(call[1]?.range).not.toHaveProperty("to");
    }
  });

  it("3. 自定义跨度 >31 天：夹到 31 天 + 常驻提示 + 以整日区间重查", async () => {
    renderPage();
    await screen.findByTestId("usage-hero");
    fireEvent.click(screen.getByText("自定义"));

    const today = new Date();
    const todayKey = dayKey(today);
    const clampedFrom = dayKey(addDays(today, -30)); // 31 天含首尾 ⇒ from = to - 30
    fireEvent.change(screen.getByTestId("usage-range-from"), {
      target: { value: dayKey(addDays(today, -60)) },
    });

    await waitFor(() =>
      expect(
        callsOf("usage_dashboard").some(
          (c) =>
            JSON.stringify(c[1]?.range) ===
            JSON.stringify({ preset: "custom", from: clampedFrom, to: todayKey })
        )
      ).toBe(true)
    );

    // 输入框回写夹取后的值；提示常驻（不是一帧即逝）
    expect(screen.getByTestId("usage-range-from")).toHaveValue(clampedFrom);
    expect(screen.getByTestId("usage-range-clamped")).toHaveTextContent("已截断为最近 31 天");
    // 起 ≤ 止、止 ≤ 今天（步骤 5 的输入边界）
    expect(screen.getByTestId("usage-range-from")).toHaveAttribute("max", todayKey);
    expect(screen.getByTestId("usage-range-to")).toHaveAttribute("min", clampedFrom);
    expect(screen.getByTestId("usage-range-to")).toHaveAttribute("max", todayKey);

    // 新查询已落定、看板已重渲染（换档 = 新查询键 → 先走加载态）⇒ 提示仍在；再点一次「自定义」
    // 也不清（清只发生在切到别的档）
    await waitFor(() => expect(screen.getByTestId("usage-hero")).toBeTruthy());
    fireEvent.click(screen.getByText("自定义"));
    expect(screen.getByTestId("usage-range-clamped")).toBeTruthy();
  });

  it("4. 错误态：可见错误正文（结构化错误经 usageErrMsg）+ 可重试 + 不出 hero", async () => {
    // 夹具 "error" 档让用量命令 reject 结构化 `{code, detail}`——直印 String(e) 会得到 `[object Object]`。
    // 采集 hook 的失败只 console.warn（不抛给 UI），本用例静音以免污染输出。
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    localStorage.setItem("mam-mock-usage", "error");
    renderPage();

    const alert = await screen.findByTestId("usage-error");
    expect(alert).toHaveAttribute("role", "alert");
    expect(alert).toHaveTextContent("用量数据加载失败");
    expect(alert).toHaveTextContent("mam-mock-usage=error");
    expect(alert.textContent).not.toContain("[object Object]");
    expect(screen.queryByTestId("usage-hero")).toBeNull();

    const before = callsOf("usage_dashboard").length;
    // D 卡（API 等效费用）默认展开，错误档下它也有一个「重试」——点击限定在 A 卡错误体内，
    // 本用例只验「旧 dashboard 查询失败可重试」（D 卡重试归 D 查询，见 v2-page 用例 11）
    fireEvent.click(within(alert).getByRole("button", { name: "重试" }));
    await waitFor(() => expect(callsOf("usage_dashboard").length).toBeGreaterThan(before));
    warn.mockRestore();
  });

  it("5. 空态：显示「暂无数据」而不是 0，且不出 hero / 指标网格", async () => {
    localStorage.setItem("mam-mock-usage", "empty");
    renderPage();
    expect(await screen.findByTestId("usage-empty")).toHaveTextContent("暂无数据");
    expect(screen.queryByTestId("usage-hero")).toBeNull();
    expect(screen.queryByTestId("usage-grid-requests")).toBeNull();
  });

  it("8. **没量到 token 的窗口**（有行、requests ≥ 1、四桶全零）→ 四桶派生的行一律 `—`，真值照常（P8）", async () => {
    // 真机形状（用户 2026-10-06 给出的账本实测）：**真空回合**行——四桶全零、但 requests ≥ 1
    // （15 行 / 50 requests，分布在 claude / opencode / zcode）。用户裁决：「只有是真实的数值，
    // 你才能写它的数值。如果没有产生数值，就不显示了」⇒ **四桶派生的位**出 `—`，**绝不印 0**。
    // 与用例 5 的区别：这里 `isDashboardEmpty` 为**假**（有行）⇒ 页面照常出看板，拦 0 的是
    // `tokenUnmeasured`（只看四个 token 桶的和）。两者**不可互替**。
    localStorage.setItem("mam-mock-usage", "unmeasured");
    renderPage();

    // 看板照常在（不是整块「暂无数据」）；hero 是 token 合计 ⇒ 出 `—` 而不是 0
    expect(await screen.findByTestId("usage-hero")).toHaveTextContent(EM_DASH);

    // **四项 token 派生行 + 命中率**一律 `—`，且都不显示环比段
    for (const key of ["requestInput", "cacheRead", "cacheMiss", "output", "hitRate"]) {
      const row = screen.getByTestId(`usage-grid-${key}`);
      expect(row).toHaveTextContent(EM_DASH);
      expect(row.textContent).not.toMatch(/\d/); // 一个数字都不许出现（0 / 0.0% 都不行）
      expect(row.textContent).not.toContain("上一周期");
    }
    // cacheWrite 详情块已随 2026-10-20 计划删除（Task 8）——token 派生量的解释面不再存在
    expect(screen.queryByTestId("usage-cache-write")).toBeNull();

    // **计数类真值照常显示**（请求次数是量出来的真值，不得被置空）
    const requests = mockUsageDashboard({ preset: "last7d" }, "tool").totals.requests;
    expect(requests).toBeGreaterThan(0);
    expect(screen.getByTestId("usage-grid-requests")).toHaveTextContent(String(requests));
    expect(screen.getByTestId("usage-grid-requests")).not.toHaveTextContent(EM_DASH);

    // 页脚/截止时间不受影响：这一档是**采集过但没有 token**，不是「尚未采集」
    expect(screen.getByTestId("usage-grid-asOf")).toHaveTextContent(/\d{2}:\d{2}:\d{2}/);

    // V2：B/C 是独立查询，不再被旧 dashboard 的「四桶全零」拖垮
    await waitFor(() => expect(screen.getByTestId("usage-trend-svg")).toBeTruthy());
    expect(screen.getByTestId("usage-trend-peak")).toBeTruthy();
    expect(screen.getByTestId("usage-dist-expand")).toHaveTextContent("显示更多（2）");
  });

  it("6. 页脚口径常驻 + 环比基准声明一次 + 保留期由设置项驱动 + collectedAt===0 哨兵可见", async () => {
    const page = renderPage();
    await screen.findByTestId("usage-hero");
    const footer = screen.getByTestId("usage-footer");
    expect(footer).toHaveTextContent("纯 token · 含子代理 · 本地聚合");
    // 2026-10-06 排版修复：页脚的「数据新鲜度：<时刻>」删掉了——它与网格的 asOf 行是**同一个
    // `collected`**（真机截图里同一时间戳在一张卡上出现两遍）；腾出的位置放「环比基准」，
    // 因为屏上 8 处环比现在都只印差值，基准必须在卡上声明一次。
    expect(footer).toHaveTextContent("环比基准：上一等长周期");
    expect(footer.textContent).not.toMatch(/\d{2}:\d{2}:\d{2}/);
    // 设置夹具默认 `detailRetentionDays = 90` ⇒ 页面把设置值传进页脚（设置查询与看板并行，
    // 谁先落地不定 ⇒ 这句要等）
    await waitFor(() =>
      expect(screen.getByTestId("usage-footer")).toHaveTextContent("90 天前无明细")
    );
    page.unmount();

    // `collectedAt === 0` 是「尚未采集」哨兵。mock 的 0 哨兵必与空态同现（页面走 UsageEmpty），
    // 故这一支只能直接渲染组件覆盖：**网格 asOf 行**要出哨兵（页脚那处已随 freshness 一并删除），
    // **不得**出 `—`。
    const dash = {
      ...mockUsageDashboard({ preset: "last7d" }, "tool"),
      compare: null,
      collectedAt: 0,
    };
    const t = i18n.t.bind(i18n);
    const one = render(<UsageSummary dash={dash} retentionDays={30} t={t} />);
    expect(screen.getByTestId("usage-grid-asOf")).toHaveTextContent("尚未采集");
    expect(screen.getByTestId("usage-footer")).toHaveTextContent("30 天前无明细");
    expect(screen.getByTestId("usage-footer").textContent).not.toMatch(/NaN|Infinity|0%/);

    // compare === null ⇒ 只显示当前值：不得出现对比段（值照常出）
    const hitRow = screen.getByTestId("usage-grid-hitRate");
    expect(hitRow).toHaveTextContent("67.4%");
    expect(hitRow).not.toHaveTextContent("上一周期");
    expect(hitRow.textContent).not.toMatch(/pt）/);
    one.unmount();

    // `retentionDays` 为空 ⇒ 不出保留期（步骤 6 的页脚三件套）
    render(<UsageSummary dash={dash} retentionDays={null} t={t} />);
    expect(screen.getByTestId("usage-footer")).not.toHaveTextContent("天前无明细");
  });

  it("7. 打开即按需采集恰一次（force=false）；切档重查不触发扫描", async () => {
    renderPage();
    await screen.findByTestId("usage-hero");

    expect(callsOf("usage_collect")).toHaveLength(1);
    expect(callsOf("usage_collect")[0][1]).toEqual({ force: false });
    // 三条读命令只读账本（§3 第 15 条）
    expect(callsOf("usage_dashboard").length).toBeGreaterThan(0);
    expect(callsOf("usage_get_settings").length).toBeGreaterThan(0);

    fireEvent.click(screen.getByText("近 5 小时"));
    await waitFor(() =>
      expect(
        callsOf("usage_dashboard").some(
          (c) => JSON.stringify(c[1]?.range) === JSON.stringify({ preset: "last5h" })
        )
      ).toBe(true)
    );
    expect(callsOf("usage_collect")).toHaveLength(1);
  });
});
