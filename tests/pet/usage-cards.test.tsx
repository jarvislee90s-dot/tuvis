// Task 7（计划②）· 看板两张卡的行为锁：趋势卡 `UsageTrendCard` + 四维分组分布卡
// `UsageDistributionCard`（8 用例）。
// 判据来源：spec P1 第 4/5 条（折线 + 渐变面积 + 右上角峰值 + 数据点 hover 精确值；四维切换 +
// 逐行「名称 + 数值 + 占比条」、下限 4%、上限 10 行超出折「等 N」）、P8（缩写值 hover 给精确值、
// 空态不显示 0）、计划 §3 第 5/10/11/13/14/19/20/22/29 条与 Task 7 步骤 3/4/5。
//
// 纪律：
//  * 语言固定 zh（同 `usage-page.test.tsx`）：`src/i18n/index.ts` 的 detector 在 jsdom 下读
//    navigator(en-US) ⇒ 默认英文；本文件断言的中文文案（「峰值 11.58万」/「等 2」/「未知」）只有 zh 有。
//  * 两张卡都是**受控展示组件**：不查询、不取数（`peakLabel` / `rows` / `groupBy` 全由 props 给）。
//    趋势卡的峰值由**页面**按 `usage.card.peak` 生成（步骤 5：`fmtTokens(max(0, …p.value))`），
//    本文件按同一公式造 label，验的是「万/亿缩写真的进卡面」这一环。
//  * 行夹具按**真机口径**构造：大看板的 tool 维度 `label` = **工具 id**（`query.rs::group_of`：
//    `UsageGroupBy::Tool => (source_id, source_id)`），展示名由 `usageAgentLabel(label)` 解析。
import { fireEvent, render, screen } from "@testing-library/react";
import { beforeAll, describe, expect, it, vi } from "vitest";
import i18n from "@/i18n";
import { UsageTrendCard } from "@/components/usage/UsageTrendCard";
import { UsageDistributionCard } from "@/components/usage/UsageDistributionCard";
import { trendPoints } from "@/components/usage/TrendChart";
import type { ChartPoint } from "@/components/usage/TrendChart";
import { DIST_MIN_BAR_PCT } from "@/lib/usage/distribution";
import { fmtTokens } from "@/lib/usage/format";
import { compareSuffix } from "@/lib/usage/range";
import type { TrendPoint, UsageRow } from "@/types/usage";
import type { DistributionRow } from "@/types/usageV2";

/** i18next 的 `t` 直接作 `TFn` 传入（与 `usage-page.test.tsx` 同法） */
const tf = i18n.t.bind(i18n);

/** 造一个趋势点（`trendPoints` 的入参形状）；`hitRate` 由后端 `cacheHitRate` 与分母决定 */
function tp(
  key: string,
  label: string,
  requestTotal: number,
  output: number,
  cacheHitRate = 0
): TrendPoint {
  return {
    key,
    label,
    buckets: { inputFresh: requestTotal, cacheRead: 0, cacheWrite: 0, output },
    metrics: { requestTotal, cacheHitRate, userEst: null, requests: 1 },
  };
}

/** 造一行 `UsageRow`：值口径与 hero 一致（`requestTotal + output`），`label` 是当前维度的 key / 原文 */
function row(key: string, label: string, requestTotal: number, output = 0): UsageRow {
  return {
    key,
    label,
    buckets: { inputFresh: requestTotal, cacheRead: 0, cacheWrite: 0, output },
    metrics: { requestTotal, cacheHitRate: 0, userEst: null, requests: 1 },
    sourceKind: "measured",
    isSubagent: null,
  };
}

/** V2 分布行：在旧行形状上补齐身份字段 */
function distRow(
  key: string,
  label: string,
  requestTotal: number,
  output = 0,
  extra: Partial<DistributionRow> = {}
): DistributionRow {
  return {
    ...row(key, label, requestTotal, output),
    sourceIds: ["claude"],
    billingProfileId: null,
    attributionKind: "unknown",
    ...extra,
  };
}

/** 页面口径的峰值文案（步骤 5）：`fmtTokens(max(0, …p.value))` ⇒ 万/亿缩写，不是千分位 */
function peakOf(points: ChartPoint[]): string {
  return tf("usage.card.peak", { v: fmtTokens(Math.max(0, ...points.map((p) => p.value))) });
}

/** 占比条宽度（%）→ 数值：`style.width` 的书写形态（`4.0%` / `25.6%`）不该被断言绑死 */
function barPctOf(key: string): number {
  const bar = screen.getByTestId(`usage-dist-bar-${key}`).firstElementChild as HTMLElement;
  return Number.parseFloat(bar.style.width);
}

/** 三个点、峰值在中间那个（115,765 → 11.58万）；命中率给 0.5 供 tooltip 断精确值 */
const PTS: ChartPoint[] = trendPoints([
  tp("2026-10-01", "10/1", 40_000, 0),
  tp("2026-10-02", "10/2", 115_765, 0, 0.5),
  tp("2026-10-03", "10/3", 20_000, 0),
]);

describe("usage-cards（计划② Task 7：趋势卡 + 四维分布卡）", () => {
  beforeAll(async () => {
    await i18n.changeLanguage("zh");
  });

  it("1. 趋势卡：纯 SVG 折线 + 右上角峰值（万/亿缩写）+ 趋势区文字环比 + 数据点 hover 出精确值", () => {
    const view = render(
      <UsageTrendCard
        points={PTS}
        // 没量到 token 的窗口级判据恒 false：本用例验的是**正常有数据**的卡（判据的用例见 page 8 / range 26-27）
        unmeasured={false}
        peakLabel={peakOf(PTS)}
        // 趋势区环比（spec P8：「指标网格各项**与趋势区**都显示对比值」）：口径 = 区间 hero 环比
        // 上一等长区间。这里给 1,954,268 ← 1,802,000 ⇒ 差 +152,268 ⇒ +8.4%，上一周期 180.20万
        compareLabel={compareSuffix(1_954_268, 1_802_000, tf)}
        title="用量趋势"
        loadingLabel="加载中…"
        emptyLabel="暂无数据"
        busy={false}
      />
    );

    // 纯 SVG 自绘（零新增依赖）：一条折线 + 每点一个圆
    expect(screen.getByTestId("usage-trend-svg")).toBeTruthy();
    expect(screen.getByTestId("usage-trend-line")).toBeTruthy();
    expect(screen.getAllByTestId("usage-trend-dot")).toHaveLength(PTS.length);
    expect(screen.getByText("用量趋势")).toBeTruthy();

    // 右上角峰值：115,765 → 11.58万（缩写形态，**带单位恒 2 位小数**；千分位 115,765 不得出现在峰值位）
    const peak = screen.getByTestId("usage-trend-peak");
    expect(peak).toHaveTextContent("峰值 11.58万");
    expect(peak.textContent).not.toContain(",");

    // 趋势区**文字**环比（spec P8 明文要求；不画上一周期的折线——契约没有上一周期的序列）。
    // 2026-10-06 用户裁决「同比冗余字太多」后：**屏上只剩差值**，上一周期的值（180.20万）
    // 不再进这句文案——它归调用方挂进 `title`（`usage.compareHint`，见 `usage-page.test.tsx`）。
    // 峰值与环比都在右槽，但**是两个节点** ⇒ 峰值那行的 textContent 不含环比（上一条断言的隐含前提）。
    expect(screen.getByTestId("usage-trend-compare")).toHaveTextContent("（+8.4%）");
    expect(peak.textContent).not.toContain("上一周期");

    // 内层直接用 Task 4 的 TrendChartWithTooltip：hover 也走万/亿缩写（不再印完整千分位）
    fireEvent.mouseEnter(screen.getAllByTestId("usage-trend-hit")[1]);
    expect(screen.getByTestId("usage-trend-tooltip")).toHaveTextContent("10/2 · 11.58万 · 50.0%");

    view.unmount();
  });

  it("2. 趋势卡：加载中优先出加载态；点数 < 2 出空态（不画假图、不出峰值）", () => {
    const props = {
      peakLabel: peakOf(PTS),
      title: "用量趋势",
      loadingLabel: "加载中…",
      emptyLabel: "暂无数据",
      // 空串 = `compare === null` / 上一周期无可比数据 ⇒ 环比整段不渲染（P8：只显示当前值，不出 0%）
      compareLabel: "",
      // 「没量到 token」的**窗口级**判据：本用例验的是**点数 < 2** 那条分支，故恒 false
      // （那条判据的用例在 `usage-page.test.tsx` 用例 8 与 `usage-range.test.ts` 用例 26/27）
      unmeasured: false,
    };

    // 无点：空态（usage.empty 文案），不画图、不出峰值
    const none = render(<UsageTrendCard {...props} points={[]} busy={false} />);
    expect(screen.getByTestId("usage-empty")).toHaveTextContent("暂无数据");
    expect(screen.queryByTestId("usage-trend-svg")).toBeNull();
    // 环比空串 = `compare === null` / 上一周期无可比 ⇒ **整段不渲染**（spec P8：只显示当前值，
    // 不得出 0% / NaN）。这里是该分支唯一的锁（`props.compareLabel` 就是空串）。
    expect(screen.queryByTestId("usage-trend-compare")).toBeNull();
    expect(screen.queryByTestId("usage-trend-peak")).toBeNull();
    none.unmount();

    // 单点同样画不出线（Task 4 对 <2 点返回 null）⇒ 仍是空态，不伪造一条线
    const one = render(<UsageTrendCard {...props} points={[PTS[0]]} busy={false} />);
    expect(screen.getByTestId("usage-empty")).toHaveTextContent("暂无数据");
    expect(screen.queryByTestId("usage-trend-svg")).toBeNull();
    expect(screen.queryByTestId("usage-trend-peak")).toBeNull();
    one.unmount();

    // 取数中：「正在取数」不是「没有数据」⇒ 加载态优先于空态
    const busyEmpty = render(<UsageTrendCard {...props} points={[]} busy />);
    expect(screen.getByTestId("usage-loading")).toHaveTextContent("加载中…");
    expect(screen.queryByTestId("usage-empty")).toBeNull();
    busyEmpty.unmount();

    // 有图但正在刷新：图留在原地，峰值**不出**（峰值是上一档的数字，印出来就是撒谎）
    const busyChart = render(<UsageTrendCard {...props} points={PTS} busy />);
    expect(screen.getByTestId("usage-trend-svg")).toBeTruthy();
    expect(screen.queryByTestId("usage-trend-peak")).toBeNull();
    expect(screen.getByText("加载中…")).toBeTruthy();
    busyChart.unmount();
  });

  it("2b. 非 busy 的空态 / 错误 / 不可用：卡头不出现「加载中」；busy 隐藏环比", () => {
    const loadingLabel = "加载中…";
    const base = {
      peakLabel: peakOf(PTS),
      compareLabel: "区间合计 （+8.4%）",
      title: "用量趋势",
      loadingLabel,
      emptyLabel: "暂无数据",
      unmeasured: false,
    };

    const empty = render(<UsageTrendCard {...base} points={[]} busy={false} />);
    expect(screen.queryByText(loadingLabel)).toBeNull();
    expect(screen.getByTestId("usage-empty")).toBeTruthy();
    empty.unmount();

    const error = render(
      <UsageTrendCard
        {...base}
        points={[]}
        busy={false}
        errorMessage="boom"
        retryLabel="重试"
        onRetry={() => {}}
      />
    );
    expect(screen.queryByText(loadingLabel)).toBeNull();
    expect(screen.getByTestId("usage-trend-error")).toHaveTextContent("boom");
    expect(screen.queryByTestId("usage-trend-peak")).toBeNull();
    error.unmount();

    const unavailable = render(
      <UsageTrendCard {...base} points={[]} busy={false} unavailableHint="why" />
    );
    expect(screen.queryByText(loadingLabel)).toBeNull();
    expect(screen.getByTestId("usage-trend-unavailable")).toHaveTextContent("why");
    unavailable.unmount();

    // busy 时峰值与环比都不显示（即使有图、compareLabel 非空）
    const busy = render(<UsageTrendCard {...base} points={PTS} busy />);
    expect(screen.queryByTestId("usage-trend-peak")).toBeNull();
    expect(screen.queryByTestId("usage-trend-compare")).toBeNull();
    expect(screen.getByText(loadingLabel)).toBeTruthy();
    busy.unmount();
  });

  it("3. 分布卡：四维切换按钮（aria-pressed 反映当前维度）点击回调出分组键", () => {
    const onGroupByChange = vi.fn();
    const rows = [row("claude", "claude", 1_000)];
    render(
      <UsageDistributionCard
        rows={rows as unknown as DistributionRow[]}
        groupBy="tool"
        onGroupByChange={onGroupByChange}
        expanded={false}
        onExpandedChange={vi.fn()}
        t={tf}
      />
    );

    // V2 四维：渠道 / 工具 / 项目 / 模型（provider 不再是按钮）
    expect(screen.getByTestId("usage-dist-group-billingProfile")).toHaveTextContent("按渠道");
    expect(screen.getByTestId("usage-dist-group-tool")).toHaveTextContent("按工具");
    expect(screen.getByTestId("usage-dist-group-project")).toHaveTextContent("按项目");
    expect(screen.getByTestId("usage-dist-group-model")).toHaveTextContent("按模型");
    expect(screen.queryByTestId("usage-dist-group-provider")).toBeNull();
    for (const dim of ["billingProfile", "tool", "project", "model"]) {
      expect(screen.getByTestId(`usage-dist-group-${dim}`)).toHaveAttribute(
        "aria-pressed",
        String(dim === "tool")
      );
    }

    fireEvent.click(screen.getByRole("button", { name: "按模型" }));
    expect(onGroupByChange).toHaveBeenCalledTimes(1);
    expect(onGroupByChange).toHaveBeenCalledWith("model");
  });

  it("4. 分布卡：计费渠道维度；未指定渠道走 t(label)，渠道维度不追加工具后缀", () => {
    const rows: DistributionRow[] = [
      distRow("p0", "Team API", 900, 0, {
        billingProfileId: "profile-a",
        attributionKind: "userAssigned",
      }),
      distRow("p1", "usage.label.unassignedChannel", 100, 0, { sourceIds: ["codex"] }),
    ];
    const view = render(
      <UsageDistributionCard
        rows={rows}
        groupBy="billingProfile"
        onGroupByChange={vi.fn()}
        expanded={false}
        onExpandedChange={vi.fn()}
        t={tf}
      />
    );

    expect(screen.getByTestId("usage-dist-p0")).toHaveTextContent("Team API");

    // Task 9（2026-10-19）：渠道维度 = 显示渠道，未指定渠道全局一行，工具不再是渠道后缀；
    // 工具上下文只在模型维度（`未指定渠道 · <工具>`）与费用行出现。
    const unknown = screen.getByTestId("usage-dist-p1");
    expect(unknown.textContent).toBe("未指定渠道");
    expect(unknown.textContent).not.toContain("usage.label.unassignedChannel");

    view.unmount();
  });

  it("5. 分布卡：逐行 = 名称 + 数值（缩写）+ 占比条（4% 下限），hover 给精确值", () => {
    // tool 维度：label 是工具 **id**（真机口径）⇒ 名字必须由 usageAgentLabel 解析出展示名
    const rows = [
      row("claude", "claude", 1_954_268),
      row("codex", "codex", 500_000),
      row("zcode", "zcode", 100_000),
      row("workbuddy", "workbuddy", 10), // 长尾：占比远低于 4% ⇒ 走下限
    ];
    const view = render(
      <UsageDistributionCard
        rows={rows as unknown as DistributionRow[]}
        groupBy="tool"
        onGroupByChange={vi.fn()}
        expanded={false}
        onExpandedChange={vi.fn()}
        t={tf}
      />
    );

    // 名称（AGENT_BADGE 展示名，不是 id）落在 `usage-dist-<key>`；数值落在同一行的 `usage-val-<key>`
    // （2026-10-06 排版修复后名称与数值是**两个格子**：整张列表共用一个 grid，名称列按最长名称取宽，
    // 数值列右对齐 ⇒ 两者不再是同一个元素的孩子。testid 刻意不带 `dist-` 前缀，免得落进
    // 下面「恰 10 行」那条 `/^usage-dist-k/` 的匹配面把行数数成 20）。
    const top = screen.getByTestId("usage-dist-claude");
    expect(top).toHaveTextContent("Claude");
    expect(screen.getByTestId("usage-val-claude")).toHaveTextContent("195.43万");
    expect(screen.getByTestId("usage-dist-codex")).toHaveTextContent("Codex");
    expect(screen.getByTestId("usage-val-codex")).toHaveTextContent("50.00万");

    // 数值格只显示万/亿缩写，不再挂完整千分位原始值 tooltip（新显示口径）
    expect(screen.getByTestId("usage-val-claude")).not.toHaveAttribute("title");
    expect(screen.getByTestId("usage-val-codex")).not.toHaveAttribute("title");

    // 占比条：按该组最大值归一；下限 DIST_MIN_BAR_PCT（4%）防短条不可见
    expect(DIST_MIN_BAR_PCT).toBe(4);
    expect(barPctOf("claude")).toBe(100);
    expect(barPctOf("codex")).toBeCloseTo(25.6, 1);
    expect(barPctOf("zcode")).toBeCloseTo(5.1, 1);
    expect(barPctOf("workbuddy")).toBe(DIST_MIN_BAR_PCT); // 10 / 1,954,268 → 落 4% 下限
    // 取色只认主题变量（§3 第 19/20 条）：底轨 --usage-track、填充 --usage-accent-a，零硬编码色值
    expect(screen.getByTestId("usage-dist-bar-workbuddy").className).toContain(
      "var(--usage-track)"
    );
    const fill = screen.getByTestId("usage-dist-bar-workbuddy").firstElementChild as HTMLElement;
    expect(fill.className).toContain("var(--usage-accent-a)");

    view.unmount();
  });

  it("6. 分布卡：V2 默认 Top5 + 显示更多（7 行全出）+ 收起；零值行出空态、绝不显示 0", () => {
    const many = Array.from({ length: 12 }, (_, i) =>
      distRow(`k${i}`, `项目${i}`, 1_000 - i * 10, 0, { billingProfileId: null })
    );
    const onExpandedChange = vi.fn();
    const fold = render(
      <UsageDistributionCard
        rows={many}
        groupBy="project"
        onGroupByChange={vi.fn()}
        expanded={false}
        onExpandedChange={onExpandedChange}
        t={tf}
      />
    );

    // V2 看板默认只显示 Top5；按钮给出剩余行数（不修改固定分享的 DIST_MAX_ROWS=10）
    expect(screen.getAllByTestId(/^usage-dist-k/)).toHaveLength(5);
    expect(screen.getByTestId("usage-dist-expand")).toHaveTextContent("显示更多（7）");
    expect(screen.queryByTestId("usage-dist-k5")).toBeNull();
    fireEvent.click(screen.getByTestId("usage-dist-expand"));
    expect(onExpandedChange).toHaveBeenCalledWith(true);
    fold.unmount();

    // 展开态：12 行 + 收起
    const expanded = render(
      <UsageDistributionCard
        rows={many}
        groupBy="project"
        onGroupByChange={vi.fn()}
        expanded
        onExpandedChange={onExpandedChange}
        t={tf}
      />
    );
    expect(screen.getAllByTestId(/^usage-dist-k/)).toHaveLength(12);
    expect(screen.getByTestId("usage-dist-expand")).toHaveTextContent("收起");
    expanded.unmount();

    // 零值行被丢弃 ⇒ 空态（不是一行 0）
    const zero = render(
      <UsageDistributionCard
        rows={[distRow("z", "零值项目", 0)]}
        groupBy="project"
        onGroupByChange={vi.fn()}
        expanded={false}
        onExpandedChange={vi.fn()}
        t={tf}
      />
    );
    expect(screen.getByTestId("usage-empty")).toHaveTextContent("暂无数据");
    expect(screen.queryByTestId("usage-dist-z")).toBeNull();
    expect(zero.container.textContent).not.toMatch(/\d/);
    zero.unmount();
  });

  it("7. 分布卡：模型名原文直出（不短化），title 给全名", () => {
    const rows = [
      distRow("m1", "claude-sonnet-4-5", 800, 0, { billingProfileId: "profile-a" }),
      distRow("m2", "gpt-5", 200, 0, { billingProfileId: "profile-a" }),
    ];
    const view = render(
      <UsageDistributionCard
        rows={rows}
        groupBy="model"
        onGroupByChange={vi.fn()}
        expanded={false}
        onExpandedChange={vi.fn()}
        t={tf}
      />
    );

    const long = screen.getByTestId("usage-dist-m1");
    expect(long).toHaveTextContent("claude-sonnet-4-5");
    expect(long).not.toHaveTextContent("…");
    expect(long).toHaveAttribute("title", "claude-sonnet-4-5");

    const brief = screen.getByTestId("usage-dist-m2");
    expect(brief).toHaveTextContent("gpt-5");

    view.unmount();
  });

  it("8. 分布卡空态：卡壳与四维按钮照常在位，空标签**恰好一个**（卡头在空态也保留）", () => {
    const onGroupByChange = vi.fn();
    const view = render(
      <UsageDistributionCard
        rows={[]}
        groupBy="project"
        onGroupByChange={onGroupByChange}
        expanded={false}
        onExpandedChange={vi.fn()}
        t={tf}
      />
    );

    expect(screen.getByText("分组分布")).toBeTruthy();
    for (const dim of ["billingProfile", "tool", "project", "model"]) {
      const btn = screen.getByTestId(`usage-dist-group-${dim}`);
      expect(btn).toHaveAttribute("aria-pressed", String(dim === "project"));
    }
    fireEvent.click(screen.getByTestId("usage-dist-group-model"));
    expect(onGroupByChange).toHaveBeenCalledWith("model");

    const empties = screen.getAllByTestId("usage-empty");
    expect(empties).toHaveLength(1);
    expect(empties[0]).toHaveTextContent("暂无数据");
    expect(empties[0].closest("[data-slot='card-content']")?.textContent).not.toMatch(/\d/);
    expect(
      view.container.querySelectorAll(
        "[data-testid^='usage-dist-']:not([data-testid^='usage-dist-group-']):not([data-testid^='usage-dist-bar-'])"
      )
    ).toHaveLength(0);
    expect(screen.queryByTestId("usage-dist-expand")).toBeNull();

    view.unmount();
  });
});
