// Task 4（计划②）· 趋势图组件 `src/components/usage/TrendChart.tsx` 的行为锁（13 条用例）。
// 判据来源：spec P4 第 4 条（折线 + 渐变面积；x 轴粒度随视图；数据点 hover 显示精确值）、
// P8（hero 完整千分位 / 命中率 1 位小数 / 精确值 hover；不得出现 0% 或 NaN）、
// 计划 §3 第 19/20 条（图表取色只认三个主题变量、浅深各一套、零硬编码色值——趋势图用其中
// 两个 accent，`--usage-track` 归占比条）与第 33 条
// （jsdom 无 canvas 内核、`getBoundingClientRect()` 恒 0 ⇒「画什么」全在纯函数里断言，
// **禁止**写「渲染高度 = Npx」这类做不到的用例；本文件只断属性与文本，不量像素）。
//
// 值口径：`trendPoints.value` 与 hero 同源（`semantics::hero_of` = 请求输入 + 产出），
// 请求输入取**后端给的** `metrics.requestTotal`，绝不用四桶相加反推——Subset 语义下
// `cacheWrite` 可能与 `cacheRead` 重叠（`normalize` 里 request_total = input 原文），反推会重复计数。
import { fireEvent, render, screen } from "@testing-library/react";
import { beforeAll, describe, expect, it } from "vitest";
import i18n from "@/i18n";
import {
  CHART_PAD,
  TrendChart,
  TrendChartWithTooltip,
  pathOf,
  trendPoints,
  trendTooltipText,
} from "@/components/usage/TrendChart";
import type { ChartPoint } from "@/components/usage/TrendChart";
import type { TrendPoint } from "@/types/usage";

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

/** 造一个趋势点（buckets 与 metrics 可分别给值，用来钉「值口径」与「命中率分母」）。 */
function tp(
  key: string,
  label: string,
  requestTotal: number,
  output: number,
  over?: { inputFresh?: number; cacheRead?: number; cacheWrite?: number; cacheHitRate?: number }
): TrendPoint {
  return {
    key,
    label,
    buckets: {
      inputFresh: over?.inputFresh ?? requestTotal,
      cacheRead: over?.cacheRead ?? 0,
      cacheWrite: over?.cacheWrite ?? 0,
      output,
    },
    metrics: {
      requestTotal,
      cacheHitRate: over?.cacheHitRate ?? 0,
      userEst: null,
      requests: 1,
    },
  };
}

/** 直接造图点（`TrendChart` 吃的是 `trendPoints` 的产物）。 */
function cp(label: string, value: number, hitRate: number | null = null): ChartPoint {
  return { key: label, label, value, hitRate };
}

const P3: ChartPoint[] = [cp("09:00", 100), cp("10:00", 800), cp("11:00", 400)];

describe("usage-trendchart（计划② Task 4：纯 SVG 趋势图）", () => {
  describe("trendPoints：值口径与命中率", () => {
    it("1. 值口径与 hero 一致（requestTotal + output），不从四桶反推", () => {
      const pts = trendPoints([
        tp("2026-10-03T09", "09:00", 1_400, 20),
        // Subset 形态：四桶相加 = 600 + 500 + 400 + 50 = 1,550，后端口径 = 700 + 50 = 750
        tp("2026-10-03T10", "10:00", 700, 50, { inputFresh: 600, cacheRead: 500, cacheWrite: 400 }),
      ]);
      expect(pts.map((p) => p.value)).toEqual([1_420, 750]);
      expect(pts.map((p) => p.key)).toEqual(["2026-10-03T09", "2026-10-03T10"]);
      expect(pts.map((p) => p.label)).toEqual(["09:00", "10:00"]);
    });

    it("2. 命中率分母为 0 → null（不是 0%）；>0 → 后端 cacheRead/requestTotal", () => {
      const pts = trendPoints([
        tp("a", "09:00", 1_400, 20, { inputFresh: 900, cacheRead: 500, cacheHitRate: 500 / 1_400 }),
        // 空桶：后端 `semantics::cache_hit_rate` 记 0.0，但那是「算不出」，前端必须转 null（P8）
        tp("b", "10:00", 0, 0, { cacheHitRate: 0 }),
      ]);
      expect(pts[0].hitRate).toBeCloseTo(500 / 1_400, 10);
      expect(pts[1].hitRate).toBeNull();
      expect(pts[1].value).toBe(0);
    });

    it("3. 空 / 畸形输入出空数组（不伪造 0 点、不抛）", () => {
      expect(trendPoints([])).toEqual([]);
      expect(trendPoints(null)).toEqual([]);
      expect(trendPoints(undefined)).toEqual([]);
      expect(trendPoints({} as unknown as TrendPoint[])).toEqual([]);
      expect(trendPoints("x" as unknown as TrendPoint[])).toEqual([]);
      // 畸形条目逐个剔除（缺 buckets 的对象、null、字符串都不是点）
      expect(trendPoints([null, undefined, {}, "x"] as unknown as TrendPoint[])).toEqual([]);
    });
  });

  describe("pathOf：纯几何（不碰 DOM）", () => {
    it("4. 空点集出空串；单点只有 M 没有连线", () => {
      expect(pathOf([])).toBe("");
      expect(pathOf([{ x: 0, y: 10 }])).toBe("M 0 10");
    });

    it("5. 两点走 M + 直线 L（不做平滑）", () => {
      expect(
        pathOf([
          { x: 0, y: 10 },
          { x: 10, y: 20 },
        ])
      ).toBe("M 0 10 L 10 20");
    });

    it("6. ≥3 点走平滑 C，段数 = 点数 − 1，全程无 L", () => {
      const d = pathOf([
        { x: 0, y: 10 },
        { x: 10, y: 0 },
        { x: 20, y: 10 },
        { x: 30, y: 0 },
      ]);
      expect(d.startsWith("M 0 10 C ")).toBe(true);
      expect(d.match(/C/g) ?? []).toHaveLength(3);
      expect(d.match(/L/g) ?? []).toHaveLength(0);
    });
  });

  describe("TrendChart：结构、取色与抽稀", () => {
    it("7. 点数 < 2 返回 null（空态由卡片渲染，不画假图）", () => {
      const a = render(<TrendChart points={[]} />);
      expect(a.container).toBeEmptyDOMElement();
      a.unmount();
      const b = render(<TrendChart points={[cp("09:00", 100)]} />);
      expect(b.container).toBeEmptyDOMElement();
      b.unmount();
      const c = render(<TrendChartWithTooltip points={[cp("09:00", 100)]} />);
      expect(c.container).toBeEmptyDOMElement();
    });

    it("8. 结构：三条网格线 + 渐变面积 + 折线 + 每点一个数据点圆", () => {
      const { container } = render(<TrendChart points={P3} />);
      expect(container.querySelectorAll("svg")).toHaveLength(1);
      expect(container.querySelectorAll('[data-testid="usage-trend-grid"]')).toHaveLength(3);
      expect(container.querySelectorAll('[data-testid="usage-trend-area"]')).toHaveLength(1);
      expect(container.querySelectorAll('[data-testid="usage-trend-line"]')).toHaveLength(1);
      expect(container.querySelectorAll('[data-testid="usage-trend-dot"]')).toHaveLength(P3.length);
      // 版式常量（分享图的趋势段引用同一组，避免两处漂移）。
      // `l/r` 2026-10-06 由 6 改 **22**：x 轴首末标签以 `textAnchor="middle"` 画在 `x = l` /
      // `x = width - r` 上，半宽约 13–14 个视口单位 ⇒ `6` 时首末标签各被裁掉约 28%
      // （真机：最左只剩半个字形，最右 `22:00` 印成 `22:0`；分享图同一处溢出白卡边界）。
      expect(CHART_PAD).toEqual({ l: 22, r: 22, t: 8, bLabel: 16, bPlain: 6 });
    });

    it("9. 取色：渐变端点只取两个主题变量，网格线 currentColor + opacity，零硬编码色值", () => {
      const { container } = render(<TrendChart points={P3} idPrefix="t4" />);
      // 注意：jsdom 的 nwsapi 不认 `linearGradient stop` 这种带驼峰标签的组合选择器（恒 0），
      // 故先取渐变元素再在它内部用小写标签名选 stop —— 断言的是真实 DOM 结构，不是选择器口味。
      const grad = container.querySelector("linearGradient");
      const stops = [...(grad?.querySelectorAll("stop") ?? [])];
      expect(stops).toHaveLength(2);
      expect(stops[0].getAttribute("stop-color")).toBe("var(--usage-accent-a)");
      expect(stops[1].getAttribute("stop-color")).toBe("var(--usage-accent-b)");
      // 面积填充引用本实例的渐变；折线走 accent-a
      const gid = grad?.getAttribute("id");
      expect(gid).toBeTruthy();
      expect(
        container.querySelector('[data-testid="usage-trend-area"]')?.getAttribute("fill")
      ).toBe(`url(#${gid})`);
      expect(
        container.querySelector('[data-testid="usage-trend-line"]')?.getAttribute("stroke")
      ).toBe("var(--usage-accent-a)");
      for (const line of container.querySelectorAll('[data-testid="usage-trend-grid"]')) {
        expect(line.getAttribute("stroke")).toBe("currentColor");
        expect(Number(line.getAttribute("stroke-opacity"))).toBeLessThan(1);
      }
      // 「组件内零硬编码色值」：整段 DOM 里不得出现 #hex / rgb() / hsl()
      const html = container.innerHTML;
      expect(html).not.toMatch(/#[0-9a-fA-F]{3,8}\b/);
      expect(html).not.toMatch(/rgba?\(|hsla?\(/);
    });

    it("10. showLabels：>14 点按步长抽稀且首末必显；false 一条不出", () => {
      const many = Array.from({ length: 20 }, (_, i) => cp(`L${i}`, 100 + i));
      const sparse = render(<TrendChart points={many} showLabels idPrefix="lbl" />);
      const labels = [...sparse.container.querySelectorAll('[data-testid="usage-trend-label"]')];
      const idx = labels.map((el) => Number(el.getAttribute("data-index")));
      // 20 点 → 步长 2：0,2,…,18 再加末点 19（首末必显）
      expect(idx).toEqual([0, 2, 4, 6, 8, 10, 12, 14, 16, 18, 19]);
      expect(labels[0].textContent).toBe("L0");
      expect(labels[labels.length - 1].textContent).toBe("L19");
      sparse.unmount();
      // 边界：14 点不抽稀；showLabels={false} 一条不出；缺省 = 显示（卡片要看 x 轴）
      const exact = render(<TrendChart points={many.slice(0, 14)} idPrefix="lbl2" />);
      expect(exact.container.querySelectorAll('[data-testid="usage-trend-label"]')).toHaveLength(
        14
      );
      exact.unmount();
      const off = render(<TrendChart points={many} showLabels={false} idPrefix="lbl3" />);
      expect(off.container.querySelectorAll('[data-testid="usage-trend-label"]')).toHaveLength(0);
    });

    it("11. 同 document 两张图：idPrefix 逐实例去重，渐变 id 不冲突（缺省也自动唯一）", () => {
      const two = render(
        <>
          <TrendChart points={P3} idPrefix="chart-a" />
          <TrendChart points={P3} idPrefix="chart-b" />
        </>
      );
      const grads = [...two.container.querySelectorAll("linearGradient")];
      expect(grads).toHaveLength(2);
      const [ga, gb] = grads.map((g) => g.getAttribute("id"));
      expect(ga).not.toBe(gb);
      expect(ga).toContain("chart-a");
      expect(gb).toContain("chart-b");
      // 面积各自引用自己那份渐变：`url(#id)` 全都解析到首个实例会把两张图画成同一个色
      const fills = [...two.container.querySelectorAll('[data-testid="usage-trend-area"]')].map(
        (a) => a.getAttribute("fill")
      );
      expect(fills).toEqual([`url(#${ga})`, `url(#${gb})`]);
      two.unmount();
      // 不给 idPrefix 也必须逐实例唯一（否则卡片里挂两张图就撞 id）
      const auto = render(
        <>
          <TrendChart points={P3} />
          <TrendChart points={P3} />
        </>
      );
      const autoIds = [...auto.container.querySelectorAll("linearGradient")].map((g) =>
        g.getAttribute("id")
      );
      expect(autoIds).toHaveLength(2);
      expect(autoIds[0]).toBeTruthy();
      expect(autoIds[0]).not.toBe(autoIds[1]);
    });
  });

  describe("Tooltip", () => {
    it("12. hover 命中层出 tooltip（万/亿缩写）、移出或换档后消失", () => {
      const pts = trendPoints([
        tp("a", "09:00", 1_400, 20, { cacheHitRate: 0.5 }),
        tp("b", "10:00", 0, 0, { cacheHitRate: 0 }),
      ]);
      const view = render(<TrendChartWithTooltip points={pts} idPrefix="tip" />);
      const hits = screen.getAllByTestId("usage-trend-hit");
      expect(hits).toHaveLength(2);
      expect(screen.queryByTestId("usage-trend-tooltip")).toBeNull();
      fireEvent.mouseEnter(hits[0]);
      expect(screen.getByTestId("usage-trend-tooltip").textContent).toBe("09:00 · 1420 · 50.0%");
      // 第二个点分母为 0 → 命中率段整段省略（不得印 0.0%）
      fireEvent.mouseEnter(hits[1]);
      expect(screen.getByTestId("usage-trend-tooltip").textContent).toBe("10:00 · 0");
      fireEvent.mouseLeave(hits[1]);
      expect(screen.queryByTestId("usage-trend-tooltip")).toBeNull();
      // 先卸掉这张图：RTL 的 `screen` 查询绑在 document.body 上，同测试里两张图会串到一起
      view.unmount();
      // 数据定时刷新：换档后点数变少而指针没动（不会再触发 mouseenter）→ 越界下标按「没悬停」收起来
      const three = [cp("A", 10), cp("B", 20), cp("C", 30)];
      const short = render(<TrendChartWithTooltip points={three} idPrefix="tip" />);
      fireEvent.mouseEnter(screen.getAllByTestId("usage-trend-hit")[2]);
      expect(screen.getByTestId("usage-trend-tooltip").textContent).toBe("C · 30");
      short.rerender(<TrendChartWithTooltip points={three.slice(0, 2)} idPrefix="tip" />);
      expect(screen.queryByTestId("usage-trend-tooltip")).toBeNull();
      short.unmount();
    });

    it("13. trendTooltipText = 标签 + 万/亿缩写 + 命中率（可省）", () => {
      expect(trendTooltipText(cp("09:00", 2_036_981, 0.9269))).toBe("09:00 · 203.70万 · 92.7%");
      // 万/亿缩写而不是完整千分位；标签与数值两段不可省
      expect(trendTooltipText(cp("X", 12_500, null))).toBe("X · 1.25万");
      expect(trendTooltipText(cp("10:00", 0, null))).toBe("10:00 · 0");
    });
  });
});
