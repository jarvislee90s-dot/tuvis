// Task 12（计划②）· 分享图布局纯层 `src/lib/usage/exportLayout.ts`（8 用例）。
// 判据来源：brief 步骤 4 的版式表（逐字）。本文件直测**步骤列表**（不碰 canvas）：
//  * 段序 = 品牌 → 范围 → hero → 指标 → 趋势 → 分组 → 工具 → 评语 → 页脚，锚点 y **严格递增**；
//  * 「用量趋势」全图**恰好一次**（hero 段小标题误用 trendTitle 会连着出现两次）；
//  * `bar.w` 恒 = `CW`(608)、`share` 夹到 `[0.04, 1]`（执行层才乘，写成「填充宽」会让前景条被平方）；
//  * 立绘/气泡/让位/总高四条公式逐字核对；`sheet = null` → 无 sprite 步骤但照常出图；
//  * 趋势点 < 2 → 空态占位；≥2 → 投影与 `TrendChart` 的峰值归一同式（直接复用 `CHART_PAD`）。
//
// 纪律：本用例是**值导入**（`layoutExport` 直接调用做对照）+ 类型一起 import；测量函数是纯函数
// （1 字符 10px），不依赖 jsdom 的任何布局 API（§3 第 33/34 条：jsdom 量不到像素）。
import { describe, expect, it } from "vitest";
import { CHART_PAD } from "@/components/usage/TrendChart";
import {
  BUBBLE_BG,
  CARD,
  CW,
  EXPORT_MIN_BAR_PCT,
  EXPORT_MIN_H,
  EXPORT_PET_H,
  EXPORT_W,
  M,
  PAD,
  PAGE,
  layoutExport,
  type ExportInput,
  type ExportLayout,
  type ExportStep,
  type MeasureText,
} from "@/lib/usage/exportLayout";

/** 假测量：1 字符 = 10px（可算、可复现；真机走 canvas `measureText`） */
const measure: MeasureText = (text) => text.length * 10;

/** 版式钉死值：任何一个变了都要先回改 brief，不是「顺手调一下」 */
const LAYOUT_CONSTANTS = {
  EXPORT_W: 720,
  M: 24,
  PAD: 32,
  CW: 608,
  EXPORT_PET_H: 360,
  EXPORT_MIN_H: 1560,
};

/** 固定浅色版的三个钉死取值（分享图不读 CSS 变量；气泡底色不是「忘了走 token」） */
const LAYOUT_COLORS = { PAGE: "#f5f6f8", CARD: "#ffffff", BUBBLE_BG: "#fffdf9" };

/** 基准输入：逐条对齐 brief 的断言锚点文案（近 7 天 · 09/27 – 10/03 / 近 7 天 Token 合计 / 产出 /
 *  用量趋势 / 分组分布 / 工具调用 / 纯 token · 含子代理 · 本地聚合） */
function baseInput(over: Partial<ExportInput> = {}): ExportInput {
  return {
    brand: "兔维斯 · 用量看板",
    rangeLabel: "近 7 天 · 09/27 – 10/03",
    heroLabel: "近 7 天 Token 合计",
    hero: "2,036,981",
    heroSub: "数据截止时间: 2026-10-03 12:00:00",
    metrics: [
      ["用户输入(估) · 含子代理", "~1,000"],
      ["产出", "82,713"],
      ["请求输入(全文累计)", "1,954,268"],
      ["缓存命中", "1,200,000"],
      ["命中率", "91.2%"],
      ["请求次数", "320"],
    ],
    trendTitle: "用量趋势",
    emptyLabel: "暂无数据",
    points: [
      { label: "09/27", value: 100 },
      { label: "09/28", value: 250 },
      { label: "10/03", value: 400 },
    ],
    groupTitle: "分组分布",
    groups: [
      { name: "Claude", value: "1,246,567", share: 1 },
      { name: "Codex", value: "460,789", share: 0.37 },
      { name: "空档", value: "0", share: 0 },
    ],
    // 分组区「等 N」文案（2026-10-06 X2 修复新增字段）：`null` = 一行没少、不出这行
    groupsMore: null,
    toolsTitle: "工具调用",
    tools2x2: [
      ["调用总数", "1,234"],
      ["平均耗时", "2.4 秒"],
      ["次数第一", "Bash 120"],
      ["耗时第一", "Edit 31.2 秒"],
    ],
    quote: "缓存吃得干干净净（命中率 91.2%），省钱小能手！",
    footer: "纯 token · 含子代理 · 本地聚合",
    pose: "waving",
    poseColIndex: 0,
    sheet: { rows: 11, geometry: { frameW: 192, frameH: 208, matchesConstants: true } },
    ...over,
  };
}

const stepsOf = (
  l: ExportLayout,
  kind: ExportStep["kind"]
): Extract<ExportStep, { kind: typeof kind }>[] =>
  l.steps.filter((s): s is Extract<ExportStep, { kind: typeof kind }> => s.kind === kind);

const textsOf = (l: ExportLayout): string[] => stepsOf(l, "text").map((s) => s.text);

/** 步骤是否越出画布（x / y 不越左、上边界；带尺寸的步骤不越右、下边界） */
function inBounds(step: ExportStep, height: number): boolean {
  const fitsBox = (x: number, y: number, w: number, h: number): boolean =>
    x >= 0 && y >= 0 && x + w <= EXPORT_W && y + h <= height;
  switch (step.kind) {
    case "rect":
    case "bar":
    case "sprite":
      return fitsBox(step.x, step.y, step.w, step.h);
    case "trend":
      return (
        fitsBox(step.x, step.y, step.w, step.h) &&
        step.pts.every(
          (p) => p.x >= step.x && p.x <= step.x + step.w && p.y >= step.y && p.y <= step.y + step.h
        )
      );
    case "line":
      return step.x1 >= 0 && step.x2 <= EXPORT_W && step.y1 >= 0 && step.y2 <= height;
    case "text":
      return step.x >= 0 && step.x <= EXPORT_W && step.y >= 0 && step.y <= height;
  }
}

describe("exportLayout（计划② Task 12：分享图版式纯层）", () => {
  it("1. 钉死常量 + 段序：9 个锚点 y 严格递增，总高 = max(下限, ceil(内容底 + 34))，任何步骤不出界", () => {
    expect({
      EXPORT_W,
      M,
      PAD,
      CW,
      EXPORT_PET_H,
      EXPORT_MIN_H,
    }).toEqual(LAYOUT_CONSTANTS);
    expect(EXPORT_MIN_BAR_PCT).toBe(0.04);
    expect({ PAGE, CARD, BUBBLE_BG }).toEqual(LAYOUT_COLORS);

    const l = layoutExport(baseInput(), measure);
    expect(l.anchors.map((a) => a.id)).toEqual([
      "brand",
      "range",
      "hero",
      "metrics",
      "trend",
      "groups",
      "tools",
      "quote",
      "footer",
    ]);
    const ys = l.anchors.map((a) => a.y);
    for (let i = 1; i < ys.length; i++)
      expect(ys[i], `锚点 ${i} 的 y 未递增`).toBeGreaterThan(ys[i - 1]);

    expect(l.height).toBeGreaterThanOrEqual(EXPORT_MIN_H);
    expect(l.height).toBe(Math.max(EXPORT_MIN_H, Math.ceil(l.contentBottom + 34)));
    // 两张底：页背景铺满整幅、白卡留 M 边距
    const rects = stepsOf(l, "rect");
    expect(rects[0]).toMatchObject({ x: 0, y: 0, w: EXPORT_W, h: l.height });
    expect(rects[1]).toMatchObject({ x: M, y: M, w: EXPORT_W - M * 2, h: l.height - M * 2 });

    const out = l.steps.filter((s) => !inBounds(s, l.height));
    expect(out, `越界步骤：${JSON.stringify(out)}`).toEqual([]);
  });

  it("2. 锚点文案逐字 + 段序 + 「用量趋势」全图恰好 1 次（hero 小标题不得误用 trendTitle）", () => {
    const l = layoutExport(baseInput(), measure);
    const texts = textsOf(l);
    for (const pinned of [
      "近 7 天 · 09/27 – 10/03",
      "近 7 天 Token 合计",
      "产出",
      "用量趋势",
      "分组分布",
      "工具调用",
      "纯 token · 含子代理 · 本地聚合",
    ]) {
      expect(texts, `缺锚点文案「${pinned}」`).toContain(pinned);
    }
    // 顺序即段序（下标严格递增）
    const order = [
      "近 7 天 · 09/27 – 10/03",
      "近 7 天 Token 合计",
      "产出",
      "用量趋势",
      "分组分布",
      "工具调用",
      "纯 token · 含子代理 · 本地聚合",
    ].map((t) => texts.indexOf(t));
    for (let i = 1; i < order.length; i++)
      expect(order[i], "锚点文案次序不对").toBeGreaterThan(order[i - 1]);
    // 全图恰好一次：hero 段小标题用 trendTitle 会让这里变 2
    expect(texts.filter((t) => t === "用量趋势")).toHaveLength(1);
  });

  it("3. bar 口径：轨道宽恒 = CW(608)、share 夹到 [0.04, 1]（执行层才乘 w×share）", () => {
    const bars = stepsOf(layoutExport(baseInput(), measure), "bar");
    expect(bars).toHaveLength(3);
    for (const bar of bars) expect(bar.w).toBe(CW);
    // share = 0 → 夹到下限 0.04（写成「填充宽」时这里会是 608 × 0.04，轨道就没了）
    expect(bars[2].share).toBeCloseTo(EXPORT_MIN_BAR_PCT, 10);
    expect(bars[0].share).toBe(1);
    expect(bars[1].share).toBeCloseTo(0.37, 10);
    // 越界输入（>1 / NaN）不得画出超宽条
    const wild = stepsOf(
      layoutExport(
        baseInput({
          groups: [
            { name: "a", value: "1", share: 3 },
            { name: "b", value: "1", share: Number.NaN },
          ],
        }),
        measure
      ),
      "bar"
    );
    expect(wild[0].share).toBe(1);
    expect(wild[1].share).toBeCloseTo(EXPORT_MIN_BAR_PCT, 10);
    // ≤6 行：多给的行不画
    const many = stepsOf(
      layoutExport(
        baseInput({
          groups: Array.from({ length: 9 }, (_, i) => ({ name: `g${i}`, value: "1", share: 1 })),
        }),
        measure
      ),
      "bar"
    );
    expect(many).toHaveLength(6);
  });

  it("分组区「等 N」：给了文案必须画出来，且整段高度跟着长（X2：旧实现静默丢行）", () => {
    // 没给（null）⇒ 一行都不多画
    expect(textsOf(layoutExport(baseInput(), measure))).not.toContain("等 6");
    // 给了 ⇒ 逐字画出来，且**内容底随之下移**（否则它会盖住下面的「工具 2×2」）
    const withMore = layoutExport(baseInput({ groupsMore: "等 6" }), measure);
    expect(textsOf(withMore)).toContain("等 6");
    expect(withMore.contentBottom).toBeGreaterThan(
      layoutExport(baseInput(), measure).contentBottom
    );
    // 与「工具 2×2」段标题的先后：等 N 在分组段内、必须在工具段之前
    const texts = textsOf(withMore);
    expect(texts.indexOf("等 6")).toBeLessThan(texts.indexOf("工具调用"));
  });

  it("4. 立绘：petW = round(360×frameW/frameH)、petX = W-PAD-petW、cut 框 = (col×frameW, row×frameH, frameW, frameH)、h 恒 360", () => {
    const l = layoutExport(baseInput(), measure);
    const [sprite] = stepsOf(l, "sprite");
    const petW = Math.round((EXPORT_PET_H * 192) / 208); // 332
    expect(petW).toBe(332);
    expect(sprite).toEqual({
      kind: "sprite",
      x: EXPORT_W - PAD - petW, // 356
      y: l.anchors.find((a) => a.id === "quote")?.y,
      w: petW,
      h: EXPORT_PET_H,
      sx: 0, // 代表帧列恒 0（安全默认）
      sy: 3 * 208, // waving = 第 3 行
      sw: 192,
      sh: 208,
    });

    // look 走第 9 行；代表帧列超出该行动画帧数时夹到最后一帧（waving 4 帧 → col 3）
    const [look] = stepsOf(layoutExport(baseInput({ pose: "look" }), measure), "sprite");
    expect(look.sy).toBe(9 * 208);
    const [clamped] = stepsOf(layoutExport(baseInput({ poseColIndex: 9 }), measure), "sprite");
    expect(clamped.sx).toBe(3 * 192);
    // 9 行图集没有 look 行 → 回落第 0 行（空白格是硬伤）
    const [fallback] = stepsOf(
      layoutExport(
        baseInput({
          pose: "look",
          sheet: { rows: 9, geometry: { frameW: 192, frameH: 208, matchesConstants: true } },
        }),
        measure
      ),
      "sprite"
    );
    expect(fallback.sy).toBe(0);
  });

  it("5. 气泡几何三式 + 按测量宽度换行（气泡右缘永远 ≤ petX - 18）", () => {
    const l = layoutExport(baseInput(), measure);
    const petX = EXPORT_W - PAD - 332; // 356
    const [bubble] = stepsOf(l, "rect").filter((r) => r.color === "#fffdf9");
    const [sprite] = stepsOf(l, "sprite");
    const quoteTextW = baseInput().quote.length * 10;
    // 长文案 + 立绘占位 → 取三项里的最小值（这里是「文本 + 44」那一项）
    const expectW = Math.min(340, Math.max(150, quoteTextW + 44), Math.max(150, petX - 18 - PAD));
    expect(bubble.w).toBe(expectW);
    expect(bubble.w).toBe(quoteTextW + 44);
    expect(bubble.x).toBe(Math.max(PAD, petX - 18 - bubble.w));
    expect(bubble.x + bubble.w).toBeLessThanOrEqual(petX - 18);
    expect(bubble.y).toBe(sprite.y + EXPORT_PET_H * 0.1);

    // 换行（长文案）：气泡内每行都装得下；行距 27、左内边距 20、整体不越过气泡内宽
    const longQuote = "这是一条很长的评语，用来验证换行。".repeat(8);
    const longL = layoutExport(baseInput({ quote: longQuote }), measure);
    const [longBubble] = stepsOf(longL, "rect").filter((r) => r.color === "#fffdf9");
    const lines = stepsOf(longL, "text").filter(
      (t) => t.font.startsWith("500 19px") && t.x === longBubble.x + 20
    );
    expect(lines.length).toBeGreaterThan(1);
    for (const line of lines) {
      expect(measure(line.text, line.font)).toBeLessThanOrEqual(longBubble.w - 40);
    }
    expect(lines[1].y - lines[0].y).toBe(27);
    expect(lines[lines.length - 1].y).toBeLessThanOrEqual(longBubble.y + longBubble.h);

    // 文案更长 → 由「立绘那侧」封顶（气泡右缘恒留 18px）
    expect(longBubble.w).toBe(petX - 18 - PAD);

    // 短文案 → 150 下限；立绘很窄（petX 远）→ 340 上限
    const shortBubble = stepsOf(layoutExport(baseInput({ quote: "收工" }), measure), "rect").filter(
      (r) => r.color === "#fffdf9"
    )[0];
    expect(shortBubble.w).toBe(150);
    const widePetBubble = stepsOf(
      layoutExport(
        baseInput({
          quote: "很长很长的一段评语，".repeat(3),
          sheet: { rows: 11, geometry: { frameW: 150, frameH: 208, matchesConstants: false } },
        }),
        measure
      ),
      "rect"
    ).filter((r) => r.color === "#fffdf9")[0];
    expect(widePetBubble.w).toBe(340);
  });

  it("6. 让位：气泡高过立绘时 clusterBottom 取气泡底，页脚跟着下移且总高按同一条公式", () => {
    const longQuote = "这是一条很长的评语。".repeat(40); // 400 字 → 多行，气泡高 > 360
    const l = layoutExport(baseInput({ quote: longQuote }), measure);
    const [bubble] = stepsOf(l, "rect").filter((r) => r.color === "#fffdf9");
    const [sprite] = stepsOf(l, "sprite");
    const bubbleBottom = bubble.y + bubble.h;
    expect(bubble.h).toBeGreaterThan(EXPORT_PET_H); // 前提成立：气泡确实高过立绘
    expect(bubbleBottom).toBeGreaterThan(sprite.y + EXPORT_PET_H);
    const footerY = l.anchors.find((a) => a.id === "footer")?.y as number;
    expect(footerY).toBe(Math.max(sprite.y + EXPORT_PET_H, bubbleBottom) + 24);
    expect(l.height).toBe(Math.max(EXPORT_MIN_H, Math.ceil(l.contentBottom + 34)));
    expect(l.height).toBeGreaterThan(EXPORT_MIN_H); // 内容确实顶破了下限
  });

  it("7. 降级：sheet = null → 无 sprite 步骤，其余版式与总高照常（气泡仍按 petX = W-PAD 的公式）", () => {
    const l = layoutExport(baseInput({ sheet: null }), measure);
    expect(stepsOf(l, "sprite")).toHaveLength(0);
    const [bubble] = stepsOf(l, "rect").filter((r) => r.color === "#fffdf9");
    const petX = EXPORT_W - PAD; // 没有立绘 → 公式里的 petX 退化成内容右缘
    const quoteTextW = baseInput().quote.length * 10;
    expect(bubble.w).toBe(
      Math.min(340, Math.max(150, quoteTextW + 44), Math.max(150, petX - 18 - PAD))
    );
    expect(bubble.x).toBe(Math.max(PAD, petX - 18 - bubble.w));
    // 段锚点照旧、总高仍走同一条公式、步骤仍不出界
    expect(l.anchors.map((a) => a.id)).toEqual([
      "brand",
      "range",
      "hero",
      "metrics",
      "trend",
      "groups",
      "tools",
      "quote",
      "footer",
    ]);
    expect(l.height).toBe(Math.max(EXPORT_MIN_H, Math.ceil(l.contentBottom + 34)));
    expect(l.steps.filter((s) => !inBounds(s, l.height))).toEqual([]);
  });

  it("8. 趋势：点 < 2 → 空态占位（不画空图）；≥2 → 投影与 TrendChart 的峰值归一同式", () => {
    // 空态：没有 trend 步骤，只有占位文案
    const empty = layoutExport(baseInput({ points: [{ label: "09/27", value: 3 }] }), measure);
    expect(stepsOf(empty, "trend")).toHaveLength(0);
    expect(textsOf(empty)).toContain("暂无数据");

    // 有数据：x 首末贴 CHART_PAD、峰值贴顶、0 贴底
    const points = [
      { label: "09/27", value: 0 },
      { label: "09/28", value: 200 },
      { label: "10/03", value: 500 },
    ];
    const l = layoutExport(baseInput({ points }), measure);
    const [trend] = stepsOf(l, "trend");
    expect(trend.pts).toHaveLength(3);
    expect(trend.pts[0].x).toBeCloseTo(trend.x + CHART_PAD.l, 6);
    expect(trend.pts[2].x).toBeCloseTo(trend.x + trend.w - CHART_PAD.r, 6);
    expect(trend.pts[2].y).toBeCloseTo(trend.y + CHART_PAD.t, 6); // 峰值贴顶
    expect(trend.pts[0].y).toBeCloseTo(trend.y + trend.h - CHART_PAD.bLabel, 6); // 0 贴底（留标签位）
    expect(trend.pts[1].x).toBeCloseTo(trend.pts[0].x + (trend.pts[2].x - trend.pts[0].x) / 2, 6);
    // x 轴标签抽稀：<= 8 全出、末点必显
    const ticks = stepsOf(l, "text").filter((t) => t.font.startsWith("400 11px"));
    expect(ticks.map((t) => t.text)).toEqual(["09/27", "09/28", "10/03"]);
    const many = layoutExport(
      baseInput({
        points: Array.from({ length: 30 }, (_, i) => ({ label: `d${i}`, value: i + 1 })),
      }),
      measure
    );
    const manyTicks = stepsOf(many, "text").filter((t) => t.font.startsWith("400 11px"));
    // 抽稀目标 8 个，但「末点必显」可能比目标多出一个（与 TrendChart 的 labelIndexes 同规则）
    expect(manyTicks.length).toBeLessThanOrEqual(9);
    expect(manyTicks[manyTicks.length - 1].text).toBe("d29");
  });
});
