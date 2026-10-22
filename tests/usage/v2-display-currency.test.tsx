// 显示币种（USD/CNY）纯层 + Hook 持久化 + 费用组件/设置页联动。
//
// 口径（用户 2026-10-19 手工测试裁决）：
//  * 默认 USD、默认汇率 7.2；localStorage 键 `usage.displayCurrency` / `usage.usdCnyRate`；
//  * 金额 ≥0.01 保留 2 位小数；<0.01 最多 6 位（不把小额印成 $0.00）；
//  * 只接受十进制字符串（科学计数 / 负号 / 千分位 / 空白一律 null）；
//  * 单价（USD / 1M tokens）同样换算，并显式标注 USD / CNY；
//  * 手动覆盖价与导出仍是 USD，不受显示币种影响（本文件只验显示层）。
import {
  act,
  fireEvent,
  render,
  renderHook,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import { beforeAll, beforeEach, describe, expect, it } from "vitest";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";
import i18n from "@/i18n";
import { UsageCostTable } from "@/components/usage/UsageCostTable";
import { UsageSection } from "@/components/settings/UsageSection";
import {
  DEFAULT_USD_CNY_RATE,
  DISPLAY_CURRENCY_STORAGE_KEY,
  USD_CNY_RATE_STORAGE_KEY,
  formatMoney,
  formatUnitPrice,
  readDisplayCurrency,
  useDisplayCurrency,
} from "@/lib/usage/displayCurrency";
import { tauriInvokeMock } from "../msw/tauriMocks";
import type { CostRow } from "@/types/usageV2";

const tf = i18n.t.bind(i18n);

function wrap(
  children: ReactNode,
  client = new QueryClient({ defaultOptions: { queries: { retry: false } } })
) {
  return <QueryClientProvider client={client}>{children}</QueryClientProvider>;
}

function costRow(overrides: Partial<CostRow> = {}): CostRow {
  return {
    key: "row",
    identity: { sourceId: "codex", observedProvider: null, modelId: "gpt-5" },
    sourceIds: ["codex"],
    billingProfileId: "p1",
    attributionKind: "userAssigned",
    tokenUsage: 1_500,
    cacheHitRate: 0.2,
    estimatedUsd: "0.1",
    status: "priced",
    pricedTokens: 1_500,
    unpricedTokens: 0,
    coverageRate: 1,
    pricingReference: { providerId: "openai", modelId: "gpt-5" },
    pricingSnapshotId: "snap",
    pricingBasis: "baseCurrent",
    priceUpdatedAt: 1_700_000_000_000,
    reasons: [],
    components: {
      input: { tokens: 1_000, unitUsdPerMillion: "2", estimatedUsd: "0.002" },
      cacheRead: null,
      cacheWrite: null,
      output: { tokens: 500, unitUsdPerMillion: "10", estimatedUsd: "0.005" },
    },
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
    ...overrides,
  };
}

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

beforeEach(() => {
  localStorage.clear();
  tauriInvokeMock.mockClear();
});

describe("displayCurrency · formatMoney", () => {
  it("1. null / 非法十进制字符串一律 null（不静默当 0）", () => {
    expect(formatMoney(null, "usd", "7.2")).toBeNull();
    for (const bad of ["", "abc", "1.", ".5", "1e3", "1,000", " 1", "1 ", "-1", "+1", "NaN"]) {
      expect(formatMoney(bad, "usd", "7.2"), bad).toBeNull();
    }
  });

  it("2. USD：≥0.01 两位小数、小额最多 6 位、大数不用科学计数法", () => {
    expect(formatMoney("0", "usd", "7.2")).toBe("$0.00");
    expect(formatMoney("0.5", "usd", "7.2")).toBe("$0.50");
    expect(formatMoney("1.2300", "usd", "7.2")).toBe("$1.23");
    expect(formatMoney("1234567.89", "usd", "7.2")).toBe("$1,234,567.89");
    expect(formatMoney("0.01", "usd", "7.2")).toBe("$0.01");
    expect(formatMoney("0.000529", "usd", "7.2")).toBe("$0.000529");
    expect(formatMoney("0.0010", "usd", "7.2")).toBe("$0.001");
    // 小于 0.0000005 的值在 6 位精度下归零：仍是 $0.00（不是 $0.0）
    expect(formatMoney("0.0000004", "usd", "7.2")).toBe("$0.00");
    expect(formatMoney("1000000000000000000000", "usd", "7.2")).toBe(
      "$1,000,000,000,000,000,000,000.00"
    );
    expect(formatMoney("1000000000000000000000", "usd", "7.2")).not.toMatch(/e/i);
  });

  it("3. CNY：按 1 USD = rate CNY 换算，精度规则与 USD 相同", () => {
    expect(formatMoney("0.5", "cny", "7.2")).toBe("¥3.60");
    expect(formatMoney("1.23", "cny", "7.2")).toBe("¥8.86"); // 8.856 → 8.86
    expect(formatMoney("10", "cny", "7.2")).toBe("¥72.00");
    expect(formatMoney("0.000529", "cny", "7.2")).toBe("¥0.003809"); // 0.0038088 → 6 位
    expect(formatMoney("1", "cny", "7.25")).toBe("¥7.25");
    expect(formatMoney("0.001", "cny", "7.25")).toBe("¥0.00725");
    expect(formatMoney("1000000000000000000000", "cny", "7.2")).not.toMatch(/e/i);
  });

  it("4. 非法汇率（空/非十进制/0）返回 null", () => {
    for (const bad of ["", "abc", "0", "-1", "1e3", "7,2"]) {
      expect(formatMoney("1", "cny", bad), bad).toBeNull();
    }
  });

  it("5. formatUnitPrice 同步换算并保留数值（单位标签由组件补）", () => {
    expect(formatUnitPrice("2", "usd", "7.2")).toBe("2.00");
    expect(formatUnitPrice("10", "usd", "7.2")).toBe("10.00");
    expect(formatUnitPrice("0.00125", "usd", "7.2")).toBe("0.00125");
    expect(formatUnitPrice("2", "cny", "7.2")).toBe("14.40");
    expect(formatUnitPrice("0.00125", "cny", "7.2")).toBe("0.009");
    expect(formatUnitPrice(null, "usd", "7.2")).toBeNull();
    expect(formatUnitPrice("2", "cny", "bad")).toBeNull();
  });
});

describe("displayCurrency · useDisplayCurrency", () => {
  it("6. 默认 USD + 7.2；读取与持久化 localStorage", () => {
    expect(readDisplayCurrency()).toEqual({ currency: "usd", rate: DEFAULT_USD_CNY_RATE });

    localStorage.setItem(DISPLAY_CURRENCY_STORAGE_KEY, "cny");
    localStorage.setItem(USD_CNY_RATE_STORAGE_KEY, "7.5");
    expect(readDisplayCurrency()).toEqual({ currency: "cny", rate: "7.5" });

    // 非法值回退默认，不把坏数据传进格式化层
    localStorage.setItem(DISPLAY_CURRENCY_STORAGE_KEY, "eur");
    localStorage.setItem(USD_CNY_RATE_STORAGE_KEY, "abc");
    expect(readDisplayCurrency()).toEqual({ currency: "usd", rate: DEFAULT_USD_CNY_RATE });
  });

  it("7. Hook setter 写 localStorage 并触发同窗口更新；非法汇率被忽略", () => {
    const { result } = renderHook(() => useDisplayCurrency());
    expect(result.current.currency).toBe("usd");
    expect(result.current.rate).toBe(DEFAULT_USD_CNY_RATE);

    act(() => result.current.setCurrency("cny"));
    expect(result.current.currency).toBe("cny");
    expect(localStorage.getItem(DISPLAY_CURRENCY_STORAGE_KEY)).toBe("cny");

    act(() => result.current.setRate("7.5"));
    expect(result.current.rate).toBe("7.5");
    expect(localStorage.getItem(USD_CNY_RATE_STORAGE_KEY)).toBe("7.5");

    act(() => result.current.setRate("abc"));
    expect(result.current.rate).toBe("7.5");
    expect(localStorage.getItem(USD_CNY_RATE_STORAGE_KEY)).toBe("7.5");
  });
});

describe("displayCurrency · 费用组件与设置页", () => {
  it("8. 费用单表按显示币种取后端原生值：USD 原生无虚线；CNY 换算值带 data-converted 虚线", () => {
    // 2026-10-20 契约：后端按显示币种给出 USD/CNY 两套金额/单价；
    // 前端只取值展示，经汇率换算的分量加虚线下划线（data-converted="true"）。
    const row = costRow({
      estimatedUsd: "0.1",
      estimatedCny: "0.72",
      usdConverted: false,
      cnyConverted: true,
      components: {
        input: {
          tokens: 1_000,
          unitUsdPerMillion: "2",
          estimatedUsd: "0.002",
          unitCnyPerMillion: "14.4",
          estimatedCny: "0.0144",
          usdConverted: false,
          cnyConverted: true,
        },
        cacheRead: null,
        cacheWrite: null,
        output: {
          tokens: 500,
          unitUsdPerMillion: "10",
          estimatedUsd: "0.005",
          unitCnyPerMillion: "72",
          estimatedCny: "0.036",
          usdConverted: false,
          cnyConverted: true,
        },
      },
    });
    const first = render(
      <UsageCostTable
        t={tf}
        groups={[{ key: "g", label: "Profile One", items: [{ row, channelName: "Profile One" }] }]}
      />
    );
    // 默认 USD：原生值原样显示，无虚线换算标记
    const usdAmountCell = screen.getByTestId("usage-cost-amount-row");
    expect(usdAmountCell).toHaveTextContent("$0.10");
    expect(usdAmountCell.querySelector('[data-converted="false"]')).toBeTruthy();
    expect(usdAmountCell.querySelector('[data-converted="true"]')).toBeNull();
    fireEvent.click(screen.getByTestId("usage-cost-table-detail-toggle-row"));
    const details = screen.getByTestId("usage-cost-details-row");
    expect(details).toHaveTextContent("2.00 USD / 1M tokens");
    first.unmount();

    localStorage.setItem(DISPLAY_CURRENCY_STORAGE_KEY, "cny");
    localStorage.setItem(USD_CNY_RATE_STORAGE_KEY, "7.2");
    const second = render(
      <UsageCostTable
        t={tf}
        groups={[{ key: "g", label: "Profile One", items: [{ row, channelName: "Profile One" }] }]}
      />
    );
    // CNY：直接显示后端原生 CNY 值；行金额 cnyConverted=true → 虚线下划线
    const amountCell = screen.getByTestId("usage-cost-amount-row");
    expect(amountCell).toHaveTextContent("¥0.72");
    expect(amountCell.querySelector('[data-converted="true"]')).toBeTruthy();
    fireEvent.click(screen.getByTestId("usage-cost-table-detail-toggle-row"));
    const secondDetails = screen.getByTestId("usage-cost-details-row");
    // 单价与分项金额全部取原生 CNY（单价列标注 CNY；≥0.01 两位小数）
    expect(secondDetails).toHaveTextContent("14.40 CNY / 1M tokens");
    expect(secondDetails).toHaveTextContent("72.00 CNY / 1M tokens");
    expect(secondDetails).toHaveTextContent("¥0.01");
    expect(secondDetails).toHaveTextContent("¥0.04");
    expect(secondDetails).not.toHaveTextContent("2.00 USD / 1M tokens");
    // 分项（后端标 cnyConverted）→ 虚线；合计行金额同样带换算标记
    expect(secondDetails.querySelectorAll('[data-converted="true"]').length).toBeGreaterThanOrEqual(
      4
    );
    const totalRow = within(secondDetails).getByTestId("usage-cost-table-detail-row-total");
    expect(totalRow).toHaveTextContent("¥0.72");
    second.unmount();
  });

  it("8b. 原生币种价格不带虚线（cnyConverted=false）；缺价 CNY 不印 ¥0.00", () => {
    const row = costRow({
      estimatedUsd: "0.1",
      estimatedCny: "0.72",
      usdConverted: false,
      cnyConverted: false, // 原生人民币价（如 zhipuai 目录）
      components: {
        input: {
          tokens: 1_000,
          unitUsdPerMillion: "2",
          estimatedUsd: "0.002",
          unitCnyPerMillion: "14.4",
          estimatedCny: "0.0144",
          usdConverted: false,
          cnyConverted: false,
        },
        cacheRead: null,
        cacheWrite: null,
        output: null, // 输出分量无 CNY 价 → 缺价 ≠ 0
      },
    });
    localStorage.setItem(DISPLAY_CURRENCY_STORAGE_KEY, "cny");
    localStorage.setItem(USD_CNY_RATE_STORAGE_KEY, "7.2");
    render(
      <UsageCostTable
        t={tf}
        groups={[{ key: "g", label: "Profile One", items: [{ row, channelName: "Profile One" }] }]}
      />
    );
    const amountCell = screen.getByTestId("usage-cost-amount-row");
    expect(amountCell).toHaveTextContent("¥0.72");
    // 原生币种：无任何虚线换算标记
    expect(amountCell.querySelector('[data-converted="true"]')).toBeNull();
    fireEvent.click(screen.getByTestId("usage-cost-table-detail-toggle-row"));
    const details = screen.getByTestId("usage-cost-details-row");
    expect(details.querySelector('[data-converted="true"]')).toBeNull();
    // 缺价分量：三列 —，绝不印 ¥0.00
    const outputRow = within(details).getByTestId("usage-cost-table-detail-row-output");
    expect(outputRow).toHaveTextContent("—");
    expect(outputRow.textContent).not.toContain("¥0.00");
  });

  it("9. 设置页可选择 CNY 并输入汇率；设置项持久化", async () => {
    render(wrap(<UsageSection />));

    const select = (await screen.findByTestId("usage-display-currency")) as HTMLSelectElement;
    expect(select.value).toBe("usd");
    // USD 时不显示汇率输入
    expect(screen.queryByTestId("usage-usd-cny-rate")).toBeNull();

    fireEvent.change(select, { target: { value: "cny" } });
    expect(localStorage.getItem(DISPLAY_CURRENCY_STORAGE_KEY)).toBe("cny");
    const rate = (await screen.findByTestId("usage-usd-cny-rate")) as HTMLInputElement;
    expect(rate.value).toBe(DEFAULT_USD_CNY_RATE);

    fireEvent.change(rate, { target: { value: "7.5" } });
    await waitFor(() => expect(localStorage.getItem(USD_CNY_RATE_STORAGE_KEY)).toBe("7.5"));
  });
});
