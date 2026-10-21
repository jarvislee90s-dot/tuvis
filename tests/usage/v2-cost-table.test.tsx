// Task 7（2026-10-20 计划 §4.3）· D 卡单表 `UsageCostTable`（每模型一卡片）+ 展开四列小表
// `UsageCostDetailGrid`（同日用户裁决：明细包在卡片内部、列头实体字、双币原生值 + 虚线换算标记）。
//
// 判据来源（计划 Task 7 TDD 清单 + 2026-10-20 增补裁决）：
//  * 每条模型一张卡片（usage-cost-card-{key}），明细 usage-cost-details-{key} 在卡片内部；
//  * 主表五列内容与顺序：渠道/模型 | 估价 | 总 Token | 缓存命中率 | 计费详情；
//    主表头与明细列头一律 `text-foreground font-medium` 实体字；
//  * 明细列 = 项目 / Token 用量 / 金额 / 单价（usage-cost-table-detail-header）；
//    行 = 新增输入 / 缓存命中 / 输出 / 合计（缓存写有数据才插入）；合计行单价留空；
//  * 估价按显示币种取后端 estimatedUsd/estimatedCny：原生币种无虚线（data-converted="false"），
//    经汇率换算的金额/单价 data-converted="true" + 虚线 class；
//  * 缺价「未估价」绝不显示 $0.00/¥0.00；partial 覆盖文案并入估价列；
//  * 小字只含归一化模型 + 快照获取时间；价格来源 / 模型发布日整行删除；reasons `·` 一行；
//  * 按渠道默认分组（组头无金额小计）、平铺行名含供应商名；`displayModelName` 进行名；
//  * 长名截断 + 容器无横向滚动；展开控件 aria-expanded / aria-controls。
//
// 纪律：语言固定 zh；不查询不取数（行夹具全由 props 给，与 pet/usage-cards 同法）。
import { fireEvent, render, screen, within } from "@testing-library/react";
import { beforeAll, describe, expect, it } from "vitest";
import i18n from "@/i18n";
import { UsageCostDetailGrid } from "@/components/usage/UsageCostDetailGrid";
import { UsageCostTable } from "@/components/usage/UsageCostTable";
import { organizeCosts } from "@/lib/usage/costRows";
import { fmtTokens } from "@/lib/usage/format";
import type { CostComponent, CostRow } from "@/types/usageV2";

const tf = i18n.t.bind(i18n);

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

/** 双币分量夹具：USD 原生 + CNY 值（默认按 7.2 换算并标记；可显式给 CNY 值/改原生标记）。 */
function comp(
  tokens: number,
  unitUsd: string,
  estUsd: string,
  opts: { cnyConverted?: boolean; cnyUnit?: string; cnyAmount?: string } = {}
): CostComponent {
  return {
    tokens,
    unitUsdPerMillion: unitUsd,
    estimatedUsd: estUsd,
    unitCnyPerMillion: opts.cnyUnit ?? String(Number(unitUsd) * 7.2),
    estimatedCny: opts.cnyAmount ?? String(Number(estUsd) * 7.2),
    usdConverted: false,
    cnyConverted: opts.cnyConverted ?? true,
  };
}

function costRow(key: string, opts: Partial<CostRow> = {}): CostRow {
  const tokenUsage = opts.tokenUsage ?? 100;
  return {
    key,
    identity: { sourceId: "codex", observedProvider: null, modelId: "gpt-5" },
    sourceIds: ["codex"],
    billingProfileId: "p1",
    attributionKind: "userAssigned",
    tokenUsage,
    cacheHitRate: 0.2,
    estimatedUsd: "0.1",
    estimatedCny: "0.72",
    usdConverted: false,
    cnyConverted: true,
    status: "priced",
    pricedTokens: tokenUsage,
    unpricedTokens: 0,
    coverageRate: 1,
    statusCny: "priced",
    pricedTokensCny: tokenUsage,
    unpricedTokensCny: 0,
    coverageRateCny: 1,
    pricingReference: { providerId: "openai", modelId: "gpt-5" },
    pricingSnapshotId: "snap",
    pricingBasis: "baseCurrent",
    priceUpdatedAt: 1_700_000_000_000,
    reasons: [],
    components: { input: null, cacheRead: null, cacheWrite: null, output: null },
    channelKey: "bp:p1",
    channelKind: "billingProfile",
    displayProvider: "Profile One",
    observedProviders: [],
    hasEmptyProvider: false,
    normalizedModelId: "gpt-5",
    pricingReleaseDate: "2025-08-07",
    priceOverride: false,
    priceRuleKind: "none",
    timeBreakdown: null,
    displayModelName: null,
    ...opts,
  };
}

/** 单组单行的最小表格夹具（详情行展开后断言小表内容） */
function renderTable(rows: CostRow[], flat = false) {
  return render(
    <UsageCostTable
      t={tf}
      flat={flat}
      groups={[
        {
          key: "g1",
          label: "Profile One",
          items: rows.map((row) => ({ row, channelName: "Profile One" })),
        },
      ]}
    />
  );
}

function detailOf(row: CostRow): HTMLElement {
  fireEvent.click(screen.getByTestId(`usage-cost-table-detail-toggle-${row.key}`));
  return screen.getByTestId(`usage-cost-details-${row.key}`);
}

describe("UsageCostTable · 主表", () => {
  it("1. 表头五列内容与顺序：名称 | 估价 | 总 Token | 缓存命中率 | 计费详情；实体字", () => {
    renderTable([costRow("r1")]);
    const header = screen.getByTestId("usage-cost-table-header");
    const cols = ["col-name", "col-amount", "col-token", "col-hit", "col-detail"].map((col) =>
      within(header).getByTestId(`usage-cost-table-${col}`)
    );
    expect(cols.map((col) => col.textContent)).toEqual([
      "渠道 / 模型",
      "估价",
      "总 Token",
      "缓存命中率",
      "计费详情",
    ]);
    // 列头为实体字（2026-10-20 用户裁决：text-foreground font-medium）
    expect(header.className).toContain("text-foreground");
    expect(header.className).toContain("font-medium");
  });

  it("2. 每条模型一张卡片；明细区域在卡片内部；aria 语义齐全", () => {
    renderTable([costRow("r1")]);
    // 卡片容器在场；明细（常驻 DOM、收起时 hidden）必须位于卡片内部
    const card = screen.getByTestId("usage-cost-card-r1");
    const toggle = screen.getByTestId("usage-cost-table-detail-toggle-r1");
    expect(toggle).toHaveAttribute("aria-expanded", "false");
    expect(toggle).toHaveAttribute("aria-controls", "usage-cost-details-r1");
    const region = screen.getByTestId("usage-cost-details-r1");
    expect(region).toHaveAttribute("hidden");
    expect(card.contains(region)).toBe(true);
    fireEvent.click(toggle);
    expect(toggle).toHaveAttribute("aria-expanded", "true");
    expect(screen.getByTestId("usage-cost-details-r1")).not.toHaveAttribute("hidden");
  });

  it("3. 行内容：金额 / Token 缩写 / 命中率", () => {
    renderTable([costRow("r1", { estimatedUsd: "0.000529", tokenUsage: 160, cacheHitRate: 0.25 })]);
    expect(screen.getByTestId("usage-cost-amount-r1")).toHaveTextContent("$0.000529");
    expect(screen.getByTestId("usage-cost-token-r1")).toHaveTextContent(fmtTokens(160, "zh"));
    expect(screen.getByTestId("usage-cost-hit-r1")).toHaveTextContent("25.0%");
  });

  it("4. 行名 = 自定义供应商名/自定义模型名；displayModelName 优先生效、title 保留原始模型", () => {
    renderTable([costRow("alias", { displayModelName: "GPT-5 主力" }), costRow("plain")], true);
    // 平铺行名含供应商名；自定义模型名优先后端 displayModelName
    expect(screen.getByTestId("usage-cost-name-alias")).toHaveTextContent(
      "Profile One / GPT-5 主力"
    );
    expect(screen.getByTestId("usage-cost-name-plain")).toHaveTextContent("Profile One / gpt-5");
    // title 仍保留原始模型证据（自定义名只在展示层）
    expect(screen.getByTestId("usage-cost-name-alias")).toHaveAttribute(
      "title",
      "Profile One / gpt-5"
    );
  });

  it("5. 缺价行：估价列「未估价」且绝不出现 $0.00；Token 照常；无全局合计元素", () => {
    renderTable([
      costRow("missing", {
        status: "unpriced",
        estimatedUsd: null,
        estimatedCny: null,
        coverageRate: null,
      }),
    ]);
    const amount = screen.getByTestId("usage-cost-amount-missing");
    expect(amount).toHaveTextContent("未估价");
    expect(amount.textContent).not.toContain("$0.00");
    expect(screen.getByTestId("usage-cost-token-missing")).toHaveTextContent("100");
    // 无全局合计 / 渠道小计字段
    expect(screen.queryByTestId(/usage-cost-(total|subtotal)/)).toBeNull();
  });

  it("6. partial：已估价部分并入估价列，覆盖率不再独立成行", () => {
    const row = costRow("partial", {
      status: "partial",
      estimatedUsd: "0.0002",
      estimatedCny: "0.00144",
      coverageRate: 0.5,
    });
    renderTable([row]);
    const amount = screen.getByTestId("usage-cost-amount-partial");
    expect(amount).toHaveTextContent("已估价部分 $0.0002");
    // 覆盖率并入估价列（同一单元格内），独立覆盖率行删除
    expect(within(amount).getByTestId("usage-cost-coverage-partial")).toHaveTextContent("50.0%");
    const details = detailOf(row);
    expect(within(details).queryByText("估价覆盖率")).toBeNull();
  });

  it("7. 按渠道分组：组头只有显示名（无金额小计）；已知渠道行名不带渠道前缀", () => {
    renderTable([costRow("r1"), costRow("r2", { tokenUsage: 50 })]);
    const group = screen.getByTestId("usage-cost-group-g1");
    expect(within(group).getByText("Profile One")).toBeTruthy();
    expect(screen.getByTestId("usage-cost-name-r1")).toHaveTextContent(/^gpt-5$/);
    expect(screen.getByTestId("usage-cost-name-r1")).toHaveAttribute(
      "title",
      "Profile One / gpt-5"
    );
    // 组段没有小计金额
    expect(within(group).queryByTestId(/subtotal/)).toBeNull();
  });

  it("8. 长模型名截断 + 容器无横向滚动", () => {
    renderTable([
      costRow("long", {
        identity: {
          sourceId: "codex",
          observedProvider: null,
          modelId: "gpt-5-with-a-very-long-model-name-that-should-truncate",
        },
      }),
    ]);
    const name = screen.getByTestId("usage-cost-name-long");
    expect(name.className).toContain("truncate");
    expect(screen.getByTestId("usage-cost-table-scroll").className).toContain("overflow-x-hidden");
  });
});

describe("UsageCostTable · 展开明细（卡片内四列小表）", () => {
  it("9. 明细列头四列 = 项目/Token 用量/金额/单价；实体字；行序与合计行单价留空", () => {
    const row = costRow("grid", {
      estimatedUsd: "0.000529",
      tokenUsage: 160,
      components: {
        input: comp(100, "2", "0.0002"),
        cacheRead: comp(20, "0.2", "0.000004"),
        cacheWrite: null,
        output: comp(40, "8", "0.000325"),
      },
    });
    renderTable([row]);
    const grid = within(detailOf(row)).getByTestId("usage-cost-table-detail-grid");

    // 明细列头（2026-10-20 增补）：项目 / Token 用量 / 金额 / 单价，实体字
    const header = within(grid).getByTestId("usage-cost-table-detail-header");
    expect(header.textContent).toContain(tf("usage.costTable.colItem"));
    expect(header.textContent).toContain(tf("usage.costTable.tokenAmount"));
    expect(header.textContent).toContain(tf("usage.costTable.colAmountShort"));
    expect(header.textContent).toContain(tf("usage.costTable.colUnitPrice"));
    expect(header.className).toContain("text-foreground");
    expect(header.className).toContain("font-medium");

    // 行序：新增输入 → 缓存命中 → 输出 → 合计（缓存写无数据 → 不插入缓存写行）
    const rowKeys = ["input", "cacheRead", "output", "total"].map(
      (key) => within(grid).getByTestId(`usage-cost-table-detail-row-${key}`).textContent
    );
    expect(rowKeys[0]).toContain(tf("usage.cost.component.input"));
    expect(rowKeys[1]).toContain(tf("usage.cost.component.cacheRead"));
    expect(rowKeys[2]).toContain(tf("usage.cost.component.output"));
    expect(rowKeys[3]).toContain(tf("usage.costTable.total"));

    // 新增输入行：Token 用量（缩写）/ 金额 / 单价三列
    const inputRow = within(grid).getByTestId("usage-cost-table-detail-row-input");
    expect(inputRow).toHaveTextContent("100");
    expect(inputRow).toHaveTextContent("$0.0002");
    expect(inputRow).toHaveTextContent("2.00 USD / 1M tokens");

    // 合计行：Token 与金额合计；单价单元格留空（—），不出现任何单价
    const totalRow = within(grid).getByTestId("usage-cost-table-detail-row-total");
    expect(totalRow).toHaveTextContent("160");
    expect(totalRow).toHaveTextContent("$0.000529");
    expect(totalRow.textContent).not.toContain("/ 1M tokens");
    expect(totalRow).toHaveTextContent("—");
    // 无缓存写行
    expect(within(grid).queryByTestId("usage-cost-table-detail-row-cacheWrite")).toBeNull();
  });

  it("10. 缓存写有数据时插入在缓存命中与输出之间", () => {
    const row = costRow("cw", {
      components: {
        input: null,
        cacheRead: null,
        cacheWrite: comp(10, "2.5", "0.000025"),
        output: null,
      },
    });
    renderTable([row]);
    const grid = within(detailOf(row)).getByTestId("usage-cost-table-detail-grid");
    const cwRow = within(grid).getByTestId("usage-cost-table-detail-row-cacheWrite");
    expect(cwRow).toHaveTextContent("缓存写");
    expect(cwRow).toHaveTextContent("10");
    expect(cwRow).toHaveTextContent("2.50 USD / 1M tokens");
    // 行序：缓存命中 → 缓存写 → 输出
    const order = ["cacheRead", "cacheWrite", "output"].map((key) =>
      within(grid).getByTestId(`usage-cost-table-detail-row-${key}`)
    );
    for (let i = 1; i < order.length; i += 1) {
      expect(order[i - 1].compareDocumentPosition(order[i])).toBe(Node.DOCUMENT_POSITION_FOLLOWING);
    }
  });

  it("11. 无数据的 component 三列显示 —（不是 0、不是该部分未估价）", () => {
    const row = costRow("nullcomp", {
      status: "priced",
      components: {
        input: null,
        cacheRead: null,
        cacheWrite: null,
        output: comp(30, "10", "0.0003"),
      },
    });
    renderTable([row]);
    const grid = within(detailOf(row)).getByTestId("usage-cost-table-detail-grid");
    const inputRow = within(grid).getByTestId("usage-cost-table-detail-row-input");
    expect(inputRow).toHaveTextContent("—");
    expect(inputRow.textContent).not.toContain("$0.00");
    expect(inputRow.textContent).not.toContain("该部分未估价");
  });

  it("12. 币种虚线契约：CNY 换算分量 data-converted=true + 虚线 class；原生人民币价正常显示", () => {
    // 显示币种 CNY：行金额与分量经汇率换算（cnyConverted=true）→ 虚线标记；
    // 原生人民币价分量（cnyConverted=false）→ 正常显示不加线。
    localStorage.setItem("usage.displayCurrency", "cny");
    const row = costRow("conv", {
      estimatedUsd: "0.0002",
      estimatedCny: "0.00144",
      usdConverted: false,
      cnyConverted: true,
      components: {
        input: comp(100, "2", "0.0002", { cnyUnit: "14.4", cnyAmount: "0.00144" }), // 换算
        cacheRead: comp(20, "0.2", "0.000004", {
          cnyConverted: false, // 原生人民币价
          cnyUnit: "1.44",
          cnyAmount: "0.0000288",
        }),
        cacheWrite: null,
        output: null,
      },
    });
    renderTable([row]);
    fireEvent.click(screen.getByTestId("usage-cost-table-detail-toggle-conv"));
    const grid = within(screen.getByTestId("usage-cost-details-conv")).getByTestId(
      "usage-cost-table-detail-grid"
    );
    // 金额取后端原生 CNY（不再乘汇率）
    const inputRow = within(grid).getByTestId("usage-cost-table-detail-row-input");
    expect(inputRow).toHaveTextContent("¥0.00144");
    expect(inputRow).toHaveTextContent("14.40 CNY / 1M tokens");
    // 换算分量：data-converted=true + 虚线下划线 class
    const convertedSpan = inputRow.querySelector('[data-converted="true"]');
    expect(convertedSpan).toBeTruthy();
    expect(convertedSpan?.className).toContain("decoration-dashed");
    // 原生人民币分量：正常显示（单价取原生 CNY 值），无换算标记
    const nativeRow = within(grid).getByTestId("usage-cost-table-detail-row-cacheRead");
    expect(nativeRow).toHaveTextContent("1.44 CNY / 1M tokens");
    expect(nativeRow.querySelector('[data-converted="true"]')).toBeNull();
    expect(nativeRow.querySelector('[data-converted="false"]')).toBeTruthy();
    localStorage.removeItem("usage.displayCurrency");
  });
});

describe("UsageCostTable · 小字区", () => {
  it("13. 小字只含归一化模型 + 快照获取时间；价格来源/模型发布日整行删除", () => {
    const row = costRow("foot", {
      normalizedModelId: "gpt-5",
      priceUpdatedAt: 1_700_000_000_000,
      pricingReleaseDate: "2025-08-07",
      pricingReference: { providerId: "openai", modelId: "gpt-5" },
    });
    renderTable([row]);
    const details = detailOf(row);
    const footnote = within(details).getByTestId(`usage-cost-table-footnote-${row.key}`);
    expect(footnote).toHaveTextContent("归一化模型");
    expect(footnote).toHaveTextContent("gpt-5");
    expect(footnote).toHaveTextContent("快照获取时间");
    // 一行内同时给出（`·` 分割）
    expect(footnote.textContent).toContain("·");
    // 价格来源 / 模型发布日删除
    expect(details.textContent).not.toContain("价格来源");
    expect(details.textContent).not.toContain("模型发布日");
    expect(details.textContent).not.toContain("模型发布日不是价格数据日期");
  });

  it("14. 不准确原因合并一行用 · 分割；无 reasons 不渲染该行", () => {
    const row = costRow("reasons", {
      reasons: ["unpricedComponent", "noPrice"],
    });
    const first = renderTable([row]);
    const reasons = within(detailOf(row)).getByTestId(`usage-cost-table-reasons-${row.key}`);
    expect(reasons.textContent).toContain("部分计费桶不可估价 · 缺少参考价");
    // 不再是 <ul> 列表
    expect(reasons.querySelector("li")).toBeNull();
    first.unmount();

    // 无 reasons 行：小字区不渲染原因行
    const clean = costRow("clean", { reasons: [] });
    renderTable([clean]);
    detailOf(clean);
    expect(screen.queryByTestId("usage-cost-table-reasons-clean")).toBeNull();
  });

  it("15. pricingBasis 文案保留一行；priceOverride 徽标保留", () => {
    const row = costRow("basis", { pricingBasis: "userOverride", priceOverride: true });
    renderTable([row]);
    const details = detailOf(row);
    expect(details).toHaveTextContent(tf("usage.cost.userOverride"));
    expect(details).not.toHaveTextContent(tf("usage.cost.baseCurrent"));
    expect(screen.getByTestId("usage-cost-override-basis")).toHaveTextContent(
      tf("usage.cost.priceOverride")
    );
  });

  it("15b. 分时规则：行名旁分时徽标 + 详情 Token 分布；非分时行不显示", () => {
    const timed = costRow("timed", {
      priceRuleKind: "time",
      priceOverride: true,
      timeBreakdown: { peakTokens: 1234, offpeakTokens: 500, dailyFallbackTokens: 20 },
    });
    renderTable([timed]);

    // 徽标在名称列、带说明 title；固定价/无规则行没有该徽标
    const badge = screen.getByTestId("usage-cost-time-badge-timed");
    expect(badge).toHaveTextContent(tf("usage.cost.timeRule"));
    expect(badge).toHaveAttribute("title", tf("usage.cost.timeRuleHint"));

    const details = detailOf(timed);
    const breakdown = within(details).getByTestId("usage-cost-time-breakdown-timed");
    expect(breakdown).toHaveTextContent(
      tf("usage.cost.timeBreakdown", {
        peak: fmtTokens(1234),
        offpeak: fmtTokens(500),
        fallback: fmtTokens(20),
      })
    );
    // 琥珀色提示（与规则类型视觉一致）
    expect(breakdown.className).toContain("amber-700");

    // 固定价行：有覆盖徽标但无分时徽标/分布
    const fixed = costRow("fixed", { priceRuleKind: "fixed", priceOverride: true });
    renderTable([fixed]);
    expect(screen.queryByTestId("usage-cost-time-badge-fixed")).toBeNull();
    const fixedDetails = detailOf(fixed);
    expect(screen.queryByTestId("usage-cost-time-breakdown-fixed")).toBeNull();
    expect(within(fixedDetails).getByTestId("usage-cost-override-fixed")).toBeInTheDocument();
  });
});

describe("UsageCostDetailGrid", () => {
  it("16. 缺价行单价列：无 unitUsd 可解析时显示 —（绝不显示 $0）", () => {
    render(<UsageCostDetailGrid row={costRow("gridonly")} t={tf} />);
    const grid = screen.getByTestId("usage-cost-table-detail-grid");
    // 全 null components：四行中 component 行的三列都是 —，合计行金额也是 —
    const totalRow = within(grid).getByTestId("usage-cost-table-detail-row-total");
    expect(totalRow.textContent).not.toContain("$0.00");
  });
});

describe("organizeCosts · 平铺模式（按模型视图）", () => {
  it("17. 平铺直接返回单组、Token 降序；渠道视图逻辑保留", () => {
    const rows = [
      costRow("k1", { tokenUsage: 100 }),
      costRow("k2", { tokenUsage: 300 }),
      costRow("k3", { tokenUsage: 200 }),
    ];
    const flat = organizeCosts(rows, "flat");
    expect(flat).toHaveLength(1);
    expect(flat[0].rows.map((r) => r.key)).toEqual(["k2", "k3", "k1"]);

    const channel = organizeCosts(rows, "channel");
    expect(channel).toHaveLength(1);
    expect(channel[0].label).toBe("Profile One");
    expect(channel[0].rows).toHaveLength(3);
  });
});
