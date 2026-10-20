// Task 15/16 · 费用组织纯函数 + 金额格式 + 费用卡组件。
// 2026-10-20 计划 Task 7：行级断言移至 `v2-cost-table.test.tsx`（单表 + 展开小表）；
// 本文件保留组织纯层 / 金额纯层 / 卡壳（加载、错误、折叠、Top5、视图与币种切换、管理入口）。
import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeAll, describe, expect, it, vi } from "vitest";
import { useState, type ReactNode } from "react";
import { listen } from "@tauri-apps/api/event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import i18n from "@/i18n";
import { formatUsd } from "@/lib/usage/costFormat";
import { costChannelLabel, organizeCosts } from "@/lib/usage/costRows";
import { UsageCostCard } from "@/components/usage/UsageCostCard";
import { mockPricingStatus } from "@/lib/usage/mockFixtures";
import { tauriInvokeMock } from "../msw/tauriMocks";
import type { UsageRange } from "@/types/usage";
import type { BillingConfig, CostRow, PricingStatus, UsageCosts } from "@/types/usageV2";

const tf = i18n.t.bind(i18n);
const RANGE: UsageRange = { preset: "last7d" };

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

type Impl = (cmd: string, args?: unknown) => Promise<unknown>;

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

function callsOf(cmd: string): Array<[string, Record<string, unknown> | undefined]> {
  return tauriInvokeMock.mock.calls.filter(([c]) => c === cmd) as Array<
    [string, Record<string, unknown> | undefined]
  >;
}

function wrap(
  children: ReactNode,
  client = new QueryClient({ defaultOptions: { queries: { retry: false } } })
) {
  return <QueryClientProvider client={client}>{children}</QueryClientProvider>;
}

const PROFILES: BillingConfig["profiles"] = [
  { id: "p1", name: "Profile One", kind: "api", note: "", archived: false },
  { id: "p2", name: "Profile Two", kind: "api", note: "", archived: false },
];
const BILLING: BillingConfig = { revision: 0, profiles: PROFILES, rules: [], pricingRules: [] };
const PRICING_FRESH: PricingStatus = {
  enabled: true,
  snapshotId: "snap-1",
  fetchedAt: 1_700_000_000_000,
  stale: false,
  refreshing: false,
  lastError: null,
  fxRate: "7.2",
  fxFetchedAt: 1_700_000_000_000,
  fxSource: "er-api",
  fxFallback: false,
};
const PRICING_STALE: PricingStatus = { ...PRICING_FRESH, stale: true };
const PRICING_EMPTY: PricingStatus = {
  enabled: false,
  snapshotId: null,
  fetchedAt: null,
  stale: true,
  refreshing: false,
  lastError: null,
  fxRate: null,
  fxFetchedAt: null,
  fxSource: null,
  fxFallback: true,
};

function costRow(
  key: string,
  opts: Partial<CostRow> & {
    tokenUsage?: number;
    profileId?: string | null;
    sources?: CostRow["sourceIds"];
    kind?: CostRow["attributionKind"];
    status?: CostRow["status"];
    estimatedUsd?: string | null;
    modelId?: string;
    cacheHitRate?: number | null;
    coverageRate?: number | null;
    reasons?: string[];
  } = {}
): CostRow {
  const tokenUsage = opts.tokenUsage ?? 100;
  const profileId = opts.profileId === undefined ? "p1" : opts.profileId;
  const attributionKind = opts.kind ?? "userAssigned";
  const channelKind =
    opts.channelKind ??
    (profileId !== null
      ? "billingProfile"
      : attributionKind === "ambiguous"
        ? "ambiguous"
        : "unassigned");
  const displayProvider =
    opts.displayProvider === undefined
      ? profileId === null
        ? null
        : (PROFILES.find((profile) => profile.id === profileId)?.name ?? profileId)
      : opts.displayProvider;
  const channelKey =
    opts.channelKey ??
    (profileId !== null
      ? `bp:${profileId}`
      : channelKind === "ambiguous"
        ? "ambiguous"
        : "unassigned");
  return {
    key,
    identity: { sourceId: "codex", observedProvider: null, modelId: opts.modelId ?? "gpt-5" },
    sourceIds: opts.sources ?? ["codex"],
    billingProfileId: profileId,
    attributionKind,
    tokenUsage,
    cacheHitRate: opts.cacheHitRate === undefined ? 0.2 : opts.cacheHitRate,
    estimatedUsd: opts.estimatedUsd === undefined ? "0.1" : opts.estimatedUsd,
    estimatedCny: opts.estimatedCny === undefined ? "0.72" : opts.estimatedCny,
    usdConverted: false,
    cnyConverted: true,
    status: opts.status ?? "priced",
    pricedTokens: tokenUsage,
    unpricedTokens: 0,
    coverageRate: opts.coverageRate === undefined ? 1 : opts.coverageRate,
    statusCny: opts.status ?? "priced",
    pricedTokensCny: tokenUsage,
    unpricedTokensCny: 0,
    coverageRateCny: opts.coverageRate === undefined ? 1 : opts.coverageRate,
    pricingReference: { providerId: "p", modelId: "gpt-5" },
    pricingSnapshotId: "snap",
    pricingBasis: "baseCurrent",
    priceUpdatedAt: 1,
    reasons: opts.reasons ?? [],
    components: { input: null, cacheRead: null, cacheWrite: null, output: null },
    channelKey,
    channelKind,
    displayProvider,
    observedProviders: [],
    hasEmptyProvider: false,
    normalizedModelId: "gpt-5",
    pricingReleaseDate: "2025-08-07",
    priceOverride: false,
    ...opts,
  };
}

describe("costRows.organizeCosts", () => {
  it("1. 相同模型两个渠道在 channel 布局分开；flat 布局（按模型视图）单组平铺、行渠道名由 costChannelLabel 给", () => {
    const rows = [
      costRow("model:p1:gpt-5", { profileId: "p1", tokenUsage: 200 }),
      costRow("model:p2:gpt-5", { profileId: "p2", tokenUsage: 100 }),
    ];

    const channel = organizeCosts(rows, "channel");
    expect(channel).toHaveLength(2);
    expect(channel.map((g) => g.key).sort()).toEqual(["bp:p1", "bp:p2"]);
    expect(channel.map((g) => g.label).sort()).toEqual(["Profile One", "Profile Two"]);

    // 平铺：直接返回单组（Token 降序）；行名含供应商名由组件层经 costChannelLabel 拼
    const flat = organizeCosts(rows, "flat");
    expect(flat).toHaveLength(1);
    expect(flat[0].rows.map((r) => r.key)).toEqual(["model:p1:gpt-5", "model:p2:gpt-5"]);
    expect(rows.map((r) => costChannelLabel(r)).sort()).toEqual(["Profile One", "Profile Two"]);
  });

  it("2. 未指定/可能混合：各自一个渠道组，标签用 i18n 键（不按工具拆组）", () => {
    const rows = [
      costRow("u1", { profileId: null, sources: ["codex"], kind: "unknown", tokenUsage: 100 }),
      costRow("u2", { profileId: null, sources: ["claude"], kind: "unknown", tokenUsage: 50 }),
      costRow("amb", {
        profileId: null,
        sources: ["zcode"],
        kind: "ambiguous",
        tokenUsage: 25,
      }),
    ];
    const groups = organizeCosts(rows, "channel");
    expect(groups).toHaveLength(2);
    expect(groups.map((g) => g.key).sort()).toEqual(["ambiguous", "unassigned"]);
    // i18n 键交给组件翻译；同渠道不同工具不再拆成两组
    expect(groups.find((g) => g.key === "unassigned")?.label).toBe("usage.label.unassignedChannel");
    expect(groups.find((g) => g.key === "unassigned")?.rows).toHaveLength(2);
    expect(groups.find((g) => g.key === "ambiguous")?.label).toBe("usage.label.ambiguousChannel");
  });

  it("3. channel 模式没有金额小计字段，也不在纯层乘价", () => {
    const rows = [
      costRow("a", { profileId: "p1", tokenUsage: 200, estimatedUsd: "0.5" }),
      costRow("b", { profileId: "p1", tokenUsage: 100, estimatedUsd: "0.25" }),
    ];
    const groups = organizeCosts(rows, "channel");
    expect(groups).toHaveLength(1);
    expect(Object.keys(groups[0]).sort()).toEqual(["key", "label", "rows"]);
    expect(groups[0].rows).toHaveLength(2);
    // 行金额原样保留；组织项没有 total/subtotal
    expect(groups[0].rows.map((r) => r.estimatedUsd)).toEqual(["0.5", "0.25"]);
    expect("totalUsd" in groups[0]).toBe(false);
    expect("estimatedUsd" in groups[0]).toBe(false);
  });

  it("4. 按 tokenUsage 降序、同值 key 升序；Top5 只隐藏不丢行（纯层返回全部）", () => {
    const rows = [
      costRow("k1", { tokenUsage: 100 }),
      costRow("k2", { tokenUsage: 300 }),
      costRow("k3", { tokenUsage: 200 }),
      costRow("k4", { tokenUsage: 200 }),
      costRow("k5", { tokenUsage: 50 }),
      costRow("k6", { tokenUsage: 25 }),
      costRow("k7", { tokenUsage: 10 }),
    ];
    const flat = organizeCosts(rows, "flat");
    expect(flat[0].rows.map((r) => r.key)).toEqual(["k2", "k3", "k4", "k1", "k5", "k6", "k7"]);
    expect(flat[0].rows).toHaveLength(7); // 纯层不截断
  });

  it("5. 全未估价仍按 Token 降序", () => {
    const rows = [
      costRow("u-small", { status: "unpriced", estimatedUsd: null, tokenUsage: 10 }),
      costRow("u-large", { status: "unpriced", estimatedUsd: null, tokenUsage: 900 }),
      costRow("u-mid", { status: "unpriced", estimatedUsd: null, tokenUsage: 100 }),
    ];
    const flat = organizeCosts(rows, "flat");
    expect(flat[0].rows.map((r) => r.tokenUsage)).toEqual([900, 100, 10]);
  });

  it("6. 两种布局 row.key 集合一致（只是组织方式不同）", () => {
    const rows = [
      costRow("a", { profileId: "p1", tokenUsage: 300 }),
      costRow("b", { profileId: "p2", tokenUsage: 200 }),
      costRow("c", { profileId: null, sources: ["claude"], kind: "unknown", tokenUsage: 100 }),
    ];
    const keys = (layout: "channel" | "flat") =>
      layout === "channel"
        ? organizeCosts(rows, layout)
            .flatMap((g) => g.rows.map((r) => r.key))
            .sort()
        : organizeCosts(rows, layout)[0]
            .rows.map((r) => r.key)
            .sort();
    expect(keys("channel")).toEqual(keys("flat"));
    expect(keys("channel")).toEqual(["a", "b", "c"]);
  });

  it("6b. 同显示渠道跨工具合并；不同显示渠道即使模型相同也分行", () => {
    const rows = [
      costRow("acme-codex", {
        profileId: null,
        kind: "observed",
        sources: ["codex"],
        tokenUsage: 300,
        channelKey: "provider:Acme",
        channelKind: "alias",
        displayProvider: "Acme",
        modelId: "gpt-5",
      }),
      costRow("acme-claude", {
        profileId: null,
        kind: "observed",
        sources: ["claude"],
        tokenUsage: 200,
        channelKey: "provider:Acme",
        channelKind: "alias",
        displayProvider: "Acme",
        modelId: "gpt-5",
      }),
      costRow("other", {
        profileId: null,
        kind: "observed",
        sources: ["codex"],
        tokenUsage: 100,
        channelKey: "provider:Other",
        channelKind: "provider",
        displayProvider: "Other",
        modelId: "gpt-5",
      }),
    ];
    const channel = organizeCosts(rows, "channel");
    expect(channel.map((g) => g.key)).toEqual(["provider:Acme", "provider:Other"]);
    expect(channel[0].rows).toHaveLength(2);
    // 平铺：行渠道名 = costChannelLabel（同名渠道多行；不同渠道即使模型相同也分行）
    const flat = organizeCosts(rows, "flat");
    expect(flat[0].rows.map((r) => costChannelLabel(r))).toEqual(["Acme", "Acme", "Other"]);
  });
});

describe("costFormat.formatUsd", () => {
  it("7. null / 0 / 小额 / 大数 / 尾零 / 非法 / 负号", () => {
    expect(formatUsd(null)).toBeNull();
    expect(formatUsd("0")).toBe("$0.00");
    expect(formatUsd("0.000529")).toBe("$0.000529");
    expect(formatUsd("0.5")).toBe("$0.50");
    expect(formatUsd("1.2300")).toBe("$1.23");
    expect(formatUsd("1234567.89")).toBe("$1,234,567.89");
    // 大数不用科学计数法
    expect(formatUsd("1000000000000000000000")).toBe("$1,000,000,000,000,000,000,000.00");
    expect(formatUsd("1000000000000000000000")).not.toMatch(/e/i);

    // 非法字符串一律 null（不静默当 0）
    for (const bad of ["", "abc", "1.", ".5", "1e3", "1,000", " 1", "1 ", "-1", "+1", "NaN"]) {
      expect(formatUsd(bad), bad).toBeNull();
    }
  });
});

const COST_ROWS: CostRow[] = [
  costRow("row:p1:gpt-5", {
    profileId: "p1",
    tokenUsage: 700,
    estimatedUsd: "0.1",
    modelId: "gpt-5",
  }),
  costRow("row:p2:gpt-5", {
    profileId: "p2",
    tokenUsage: 600,
    estimatedUsd: "0.2",
    modelId: "gpt-5",
  }),
  costRow("row:p1:gpt-4", {
    profileId: "p1",
    tokenUsage: 500,
    status: "partial",
    estimatedUsd: "0.0002",
    coverageRate: 0.5,
    modelId: "gpt-4",
    reasons: ["unpricedComponent"],
  }),
  costRow("row:u:gpt-u", {
    profileId: null,
    kind: "unknown",
    sources: ["claude"],
    tokenUsage: 400,
    status: "unpriced",
    estimatedUsd: null,
    coverageRate: null,
    modelId: "gpt-u",
    reasons: ["noPrice"],
  }),
  costRow("row:p1:free", {
    profileId: "p1",
    tokenUsage: 300,
    estimatedUsd: "0",
    modelId: "free-model",
  }),
  costRow("row:p1:r6", { profileId: "p1", tokenUsage: 200, modelId: "model-6" }),
  costRow("row:p2:r7", { profileId: "p2", tokenUsage: 100, modelId: "model-7" }),
];

function costsOf(rows: CostRow[], pricing: PricingStatus): UsageCosts {
  return {
    range: RANGE,
    rows,
    pricing,
    fxRate: "7.2",
    fxFetchedAt: pricing.fxFetchedAt,
    fxSource: "er-api",
    fxFallback: false,
  };
}

function CardHarness({ initialOpen = false }: { initialOpen?: boolean }) {
  const [open, setOpen] = useState(initialOpen);
  return <UsageCostCard range={RANGE} open={open} onOpenChange={setOpen} t={tf} />;
}

function renderCard(
  rows: CostRow[] = COST_ROWS,
  pricing: PricingStatus = PRICING_FRESH,
  initialOpen = true
) {
  const restore = override({
    usage_costs: () => Promise.resolve(costsOf(rows, pricing)),
    usage_pricing_get: () => Promise.resolve(pricing),
    usage_billing_get: () => Promise.resolve(BILLING),
    usage_pricing_set_enabled: () => Promise.resolve({ ...PRICING_FRESH, enabled: true }),
  });
  render(wrap(<CardHarness initialOpen={initialOpen} />));
  return restore;
}

describe("UsageCostCard · 币种快捷切换与管理入口（2026-10-20 计划 Task 7）", () => {
  it("7a. 表头 USD/CNY 切换取后端双币原生值；换算金额带 data-converted 虚线；CNY 图例仅 CNY 显示", async () => {
    const rows = [
      costRow("cur", {
        estimatedUsd: "0.1",
        estimatedCny: "0.72",
        tokenUsage: 1500,
        coverageRate: 1,
        components: {
          input: {
            tokens: 1000,
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
      }),
    ];
    const restore = renderCard(rows, PRICING_FRESH, true);
    await screen.findByTestId("usage-cost-body");

    // 默认 USD：原生 USD 值、无换算标记；图例只在 CNY 出现
    expect(screen.getByTestId("usage-cost-amount-cur")).toHaveTextContent("$0.10");
    expect(
      screen.getByTestId("usage-cost-amount-cur").querySelector('[data-converted="false"]')
    ).toBeTruthy();
    expect(screen.queryByTestId("usage-cost-converted-legend")).toBeNull();
    expect(screen.getByTestId("usage-cost-table-currency-usd")).toHaveAttribute(
      "aria-pressed",
      "true"
    );
    fireEvent.click(screen.getByTestId("usage-cost-table-detail-toggle-cur"));
    expect(screen.getByTestId("usage-cost-details-cur")).toHaveTextContent("2.00 USD / 1M tokens");

    // 切 CNY：金额/分项金额/单价全部取后端原生 CNY 值；换算分量带虚线标记；图例出现
    fireEvent.click(screen.getByTestId("usage-cost-table-currency-cny"));
    await waitFor(() =>
      expect(screen.getByTestId("usage-cost-amount-cur")).toHaveTextContent("¥0.72")
    );
    expect(screen.getByTestId("usage-cost-table-currency-cny")).toHaveAttribute(
      "aria-pressed",
      "true"
    );
    const amountCell = screen.getByTestId("usage-cost-amount-cur");
    expect(amountCell.querySelector('[data-converted="true"]')).toBeTruthy();
    expect(amountCell.querySelector('[data-converted="true"]')?.className).toContain(
      "decoration-dashed"
    );
    // CNY 图例（卡头摘要处声明虚线含义）
    expect(screen.getByTestId("usage-cost-converted-legend")).toHaveTextContent(
      tf("usage.cost.convertedLegend")
    );
    const details = screen.getByTestId("usage-cost-details-cur");
    expect(details).toHaveTextContent("14.40 CNY / 1M tokens");
    expect(details).toHaveTextContent("72.00 CNY / 1M tokens");
    expect(details).not.toHaveTextContent("2.00 USD / 1M tokens");
    restore();
  });

  it("7a-2. 同步价格按钮：调用 refresh；同步中 disabled + spinner；成功出 status；失败出 alert", async () => {
    // 可控模式：deferred promise 手动放行，锁定「同步中」中间态
    let release!: (status: PricingStatus) => void;
    const restore = override({
      usage_costs: () => Promise.resolve(costsOf(COST_ROWS, PRICING_FRESH)),
      usage_pricing_get: () => Promise.resolve(PRICING_FRESH),
      usage_billing_get: () => Promise.resolve(BILLING),
      usage_pricing_refresh: () =>
        new Promise<PricingStatus>((resolve) => {
          release = resolve;
        }),
    });
    render(wrap(<CardHarness initialOpen />));
    await screen.findByTestId("usage-cost-body");

    const sync = screen.getByTestId("usage-cost-sync");
    expect(sync).not.toBeDisabled();
    fireEvent.click(sync);
    // 同步中：按钮 disabled，RefreshCw 进入旋转态
    await waitFor(() => expect(screen.getByTestId("usage-cost-sync")).toBeDisabled());
    expect(screen.getByTestId("usage-cost-sync").querySelector(".animate-spin")).toBeTruthy();
    expect(callsOf("usage_pricing_refresh")).toHaveLength(1);
    // 中间态无成功/失败文案
    expect(screen.queryByTestId("usage-cost-sync-status")).toBeNull();
    expect(screen.queryByTestId("usage-cost-sync-error")).toBeNull();

    // 成功：status（role=status）带快照时间
    release({ ...PRICING_FRESH, fetchedAt: 1_760_000_000_000, lastError: null });
    await waitFor(() =>
      expect(screen.getByTestId("usage-cost-sync-status")).toHaveAttribute("role", "status")
    );
    expect(screen.getByTestId("usage-cost-sync-status").textContent).toContain("价格已同步");
    expect(screen.queryByTestId("usage-cost-sync-error")).toBeNull();
    expect(screen.getByTestId("usage-cost-sync")).not.toBeDisabled();
    restore();

    // 失败（返回体带 lastError）：role=alert，成功文案不出现
    let reject!: (e: unknown) => void;
    const restore2 = override({
      usage_costs: () => Promise.resolve(costsOf(COST_ROWS, PRICING_FRESH)),
      usage_pricing_get: () => Promise.resolve(PRICING_FRESH),
      usage_billing_get: () => Promise.resolve(BILLING),
      usage_pricing_refresh: () =>
        new Promise<PricingStatus>((_resolve, rej) => {
          reject = rej;
        }),
    });
    fireEvent.click(screen.getByTestId("usage-cost-sync"));
    await waitFor(() => expect(screen.getByTestId("usage-cost-sync")).toBeDisabled());
    reject({ code: "usage-internal", detail: "network down" });
    const alert = await screen.findByTestId("usage-cost-sync-error");
    expect(alert).toHaveAttribute("role", "alert");
    expect(alert.textContent).toContain("network down");
    expect(screen.queryByTestId("usage-cost-sync-status")).toBeNull();
    expect(screen.getByTestId("usage-cost-sync")).not.toBeDisabled();
    restore2();
  });

  it("7a-3. refresh resolve 但 lastError 非空：走失败态（role=alert + 错误文案），按钮恢复可用", async () => {
    // 与 reject 同一条失败分支：后端把网络失败折叠进返回体的 lastError（P2 复审补例）
    const restore = override({
      usage_costs: () => Promise.resolve(costsOf(COST_ROWS, PRICING_FRESH)),
      usage_pricing_get: () => Promise.resolve(PRICING_FRESH),
      usage_billing_get: () => Promise.resolve(BILLING),
      usage_pricing_refresh: () => Promise.resolve({ ...mockPricingStatus(), lastError: "boom" }),
    });
    render(wrap(<CardHarness initialOpen />));
    await screen.findByTestId("usage-cost-body");

    fireEvent.click(screen.getByTestId("usage-cost-sync"));
    const alert = await screen.findByTestId("usage-cost-sync-error");
    expect(alert).toHaveAttribute("role", "alert");
    expect(alert.textContent).toContain("boom");
    // 成功文案不出现；按钮回到可用
    expect(screen.queryByTestId("usage-cost-sync-status")).toBeNull();
    expect(screen.getByTestId("usage-cost-sync")).not.toBeDisabled();
    restore();
  });

  it("7a-4. 定价未启用（enabled=false）：不渲染同步价格按钮（管理入口保留）", async () => {
    const disabled: PricingStatus = { ...PRICING_FRESH, enabled: false };
    const restore = renderCard(COST_ROWS, disabled, true);
    await screen.findByTestId("usage-cost-body");

    expect(screen.queryByTestId("usage-cost-sync")).toBeNull();
    // 卡头其余操作不受影响
    expect(screen.getByTestId("usage-cost-manage")).toBeTruthy();
    restore();
  });

  it("7b. 「管理配置」打开与设置页共用的配置弹窗；打开才拉观察条目，关闭卸载", async () => {
    const restore = renderCard(COST_ROWS, PRICING_FRESH, true);
    await screen.findByTestId("usage-cost-body");

    // 关闭态：D 卡下方不常驻配置卡，也没有弹窗壳 / 配置面板
    expect(screen.queryByTestId("usage-config-dialog-cost")).toBeNull();
    expect(screen.queryByTestId("usage-config-panel-cost")).toBeNull();
    expect(screen.queryByRole("dialog")).toBeNull();
    const entriesCallsBefore = callsOf("usage_observed_entries_get").length;

    // manage = aria-haspopup=dialog 的触发按钮（不再是内嵌折叠卡的 aria-expanded 开关）
    const manage = screen.getByTestId("usage-cost-manage");
    expect(manage).toHaveTextContent(i18n.t("usage.cost.manage"));
    expect(manage).toHaveAttribute("aria-haspopup", "dialog");
    expect(manage).not.toHaveAttribute("aria-expanded");

    // 点开：role=dialog 的弹窗挂载（与设置页同一壳，testid 带实例前缀）
    fireEvent.click(manage);
    const dialog = await screen.findByRole("dialog");
    expect(dialog).toHaveAttribute("data-testid", "usage-config-dialog-cost");
    expect(within(dialog).getByTestId("usage-config-panel-cost")).toBeTruthy();
    // 观察条目查询由弹窗打开触发（面板只在打开态挂载）
    await waitFor(() =>
      expect(callsOf("usage_observed_entries_get").length).toBeGreaterThan(entriesCallsBefore)
    );
    // 高级覆盖默认收起：Manager 仍未挂载
    expect(screen.queryByTestId("price-override-manager")).toBeNull();

    // Esc 关闭：弹窗连同面板一起卸载。不预先 blur/移动焦点——生产侧 onOpenAutoFocus
    // 已把初始焦点放到弹窗标题，这里直接 Esc 必须一次生效（回归修复本身）。
    fireEvent.keyDown(dialog, { key: "Escape" });
    await waitFor(() => expect(screen.queryByTestId("usage-config-dialog-cost")).toBeNull());
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(screen.queryByTestId("usage-config-panel-cost")).toBeNull();

    // 重开再从关闭按钮（X）关闭：同样卸载
    fireEvent.click(manage);
    const reopened = await screen.findByRole("dialog");
    fireEvent.click(within(reopened).getByRole("button", { name: "Close" }));
    await waitFor(() => expect(screen.queryByTestId("usage-config-dialog-cost")).toBeNull());
    restore();
  });
});

describe("UsageCostCard", () => {
  it("10. 默认折叠不挂载正文；展开后渠道默认、Top5 + 显示更多；切 model 同模型不同渠道两行", async () => {
    const restore = renderCard(COST_ROWS, PRICING_FRESH, false);

    // 默认折叠：卡头常在，正文不挂载
    expect(screen.getByTestId("usage-fold-toggle-cost")).toHaveAttribute("aria-expanded", "false");
    expect(screen.queryByTestId("usage-cost-body")).toBeNull();
    expect(screen.queryAllByTestId(/^usage-cost-row-/)).toHaveLength(0);
    expect(screen.getByText("按模型参考 API 价估算，非实际支出")).toBeTruthy();

    // 键盘展开（Enter / Space 均可操作原生按钮）
    const user = userEvent.setup();
    const toggle = screen.getByTestId("usage-fold-toggle-cost");
    toggle.focus();
    await user.keyboard("{Enter}");
    await waitFor(() => expect(screen.getByTestId("usage-cost-body")).toBeTruthy());
    expect(toggle).toHaveAttribute("aria-expanded", "true");

    // 渠道视图默认；Top5 + 更多
    expect(screen.getByTestId("usage-cost-view-channel")).toHaveAttribute("aria-pressed", "true");
    expect(screen.getAllByTestId(/^usage-cost-row-/)).toHaveLength(5);
    // 渠道视图行名不再重复组头：已知渠道只显示模型，完整渠道仍在 title / 组头。
    expect(screen.getByTestId("usage-cost-name-row:p1:gpt-5")).toHaveTextContent(/^gpt-5$/);
    expect(screen.getByTestId("usage-cost-name-row:p1:gpt-5")).toHaveAttribute(
      "title",
      "Profile One / gpt-5"
    );
    expect(screen.queryByText("Profile One / gpt-5")).toBeNull();
    // 全卡按 tokenUsage 全局降序：400 的 unknown 行必须在，200 的 p1 r6 必须被隐藏
    expect(screen.getByTestId("usage-cost-row-row:u:gpt-u")).toBeTruthy();
    expect(screen.queryByTestId("usage-cost-row-row:p1:r6")).toBeNull();
    const more = screen.getByTestId("usage-cost-more");
    expect(more).toHaveTextContent("2");
    expect(more).toHaveAttribute("aria-controls", "usage-cost-rows");
    fireEvent.click(more);
    expect(screen.getAllByTestId(/^usage-cost-row-/)).toHaveLength(7);

    // 切模型：同模型两个渠道各自一行，行名必须带渠道前缀。
    fireEvent.click(screen.getByTestId("usage-cost-view-model"));
    await waitFor(() =>
      expect(screen.getByTestId("usage-cost-view-model")).toHaveAttribute("aria-pressed", "true")
    );
    expect(screen.getByText("Profile One / gpt-5")).toBeTruthy();
    expect(screen.getByText("Profile Two / gpt-5")).toBeTruthy();

    // 无全局总计 / 渠道小计（说明性文案里提到「无…小计」不算，必须没有合计元素/金额）
    expect(screen.queryByTestId(/usage-cost-(total|subtotal)/)).toBeNull();
    expect(document.body.textContent).not.toContain("$总计");

    restore();
  });

  it("11. stale 快照显示日期与降级标记；无快照不自动开启，点击后才 setEnabled", async () => {
    let restore = renderCard(COST_ROWS, PRICING_STALE, true);
    expect(await screen.findByTestId("usage-cost-stale")).toHaveTextContent("已过期");
    expect(screen.getByTestId("usage-cost-stale")).toHaveTextContent(/\d{4}/);
    restore();

    // 无快照：只显示入口，点击前绝不调用 setEnabled
    restore = renderCard([], PRICING_EMPTY, true);
    const enable = await screen.findByTestId("usage-cost-enable-pricing");
    expect(callsOf("usage_pricing_set_enabled")).toHaveLength(0);
    fireEvent.click(enable);
    await waitFor(() =>
      expect(callsOf("usage_pricing_set_enabled")[0]?.[1]).toEqual({ enabled: true })
    );
    restore();
  });

  it("12. D 查询错误只替换卡体并可重试；不出现费用行", async () => {
    const restore = override({
      usage_costs: () => Promise.reject({ code: "usage-db-failed", detail: "cost boom" }),
      usage_pricing_get: () => Promise.resolve(PRICING_FRESH),
      usage_billing_get: () => Promise.resolve(BILLING),
    });
    render(wrap(<CardHarness initialOpen />));

    const error = await screen.findByTestId("usage-cost-error");
    expect(error).toHaveTextContent("cost boom");
    expect(screen.queryAllByTestId(/^usage-cost-row-/)).toHaveLength(0);
    const before = callsOf("usage_costs").length;
    fireEvent.click(within(error).getByRole("button", { name: "重试" }));
    await waitFor(() => expect(callsOf("usage_costs").length).toBeGreaterThan(before));

    restore();
  });

  it("13. 渠道视图行名去重：组头给渠道，已知行只显示模型；unassigned/ambiguous 保留上下文", async () => {
    const rows = [
      costRow("known", { profileId: "p1", tokenUsage: 500, modelId: "gpt-5" }),
      costRow("u-codex", {
        profileId: null,
        kind: "unknown",
        sources: ["codex"],
        tokenUsage: 400,
        modelId: "gpt-5",
        estimatedUsd: null,
        status: "unpriced",
      }),
      costRow("u-claude", {
        profileId: null,
        kind: "unknown",
        sources: ["claude"],
        tokenUsage: 300,
        modelId: "gpt-5",
        estimatedUsd: null,
        status: "unpriced",
      }),
      costRow("amb-zcode", {
        profileId: null,
        kind: "ambiguous",
        sources: ["zcode"],
        tokenUsage: 200,
        modelId: "gpt-5",
        estimatedUsd: null,
        status: "unpriced",
      }),
    ];
    const restore = renderCard(rows, PRICING_FRESH, true);

    // 渠道组只有一个「未指定渠道」，组头不拼接全部 sourceIds
    await waitFor(() => expect(screen.getAllByText("未指定渠道")).toHaveLength(1));
    expect(screen.queryByText("未指定渠道 · Claude, Codex")).toBeNull();
    // 渠道视图：已知渠道行只显示模型（渠道由组头给出），title 仍保留完整名称。
    expect(screen.getByTestId("usage-cost-name-known")).toHaveTextContent(/^gpt-5$/);
    expect(screen.getByTestId("usage-cost-name-known")).toHaveAttribute(
      "title",
      "Profile One / gpt-5"
    );
    // unassigned 行保留工具上下文（按后端拆行的行键区分），但不再重复渠道名。
    expect(screen.getByTestId("usage-cost-name-u-codex")).toHaveTextContent("Codex / gpt-5");
    expect(screen.getByTestId("usage-cost-name-u-codex")).not.toHaveTextContent("未指定渠道");
    expect(screen.getByTestId("usage-cost-name-u-claude")).toHaveTextContent("Claude / gpt-5");
    // ambiguous 行保留原始证据/工具上下文。
    expect(screen.getByTestId("usage-cost-name-amb-zcode")).toHaveTextContent("ZCode / gpt-5");
    expect(screen.getByTestId("usage-cost-name-amb-zcode")).not.toHaveTextContent("可能混合");

    fireEvent.click(screen.getByTestId("usage-cost-view-model"));
    await waitFor(() =>
      expect(screen.getByTestId("usage-cost-view-model")).toHaveAttribute("aria-pressed", "true")
    );
    // 模型视图是平铺行，渠道前缀必须回到行名。
    expect(screen.getByTestId("usage-cost-name-known")).toHaveTextContent("Profile One / gpt-5");
    expect(screen.getByText("未指定渠道 · Codex / gpt-5")).toBeTruthy();
    expect(screen.getByText("未指定渠道 · Claude / gpt-5")).toBeTruthy();

    restore();
  });

  it("14. 定价事件失效 pricing/costs：无快照 → 有快照并出现数据；refreshing 文案可见", async () => {
    let pricingState = PRICING_EMPTY;
    let rows: CostRow[] = [];
    const restore = override({
      usage_costs: () => Promise.resolve(costsOf(rows, pricingState)),
      usage_pricing_get: () => Promise.resolve(pricingState),
      usage_billing_get: () => Promise.resolve(BILLING),
    });

    const mockedListen = vi.mocked(listen);
    const original = mockedListen.getMockImplementation();
    let fire: (() => void) | null = null;
    mockedListen.mockImplementation(async (_event, handler) => {
      fire = () => (handler as () => void)();
      return () => {};
    });

    render(wrap(<CardHarness initialOpen />));
    expect(await screen.findByTestId("usage-cost-enable-pricing")).toBeTruthy();
    await waitFor(() => expect(fire).not.toBeNull());

    // 事件后：pricing 与 costs 同时失效重取，无快照 → 有数据
    pricingState = { ...PRICING_FRESH, refreshing: true };
    rows = COST_ROWS;
    fire!();
    await waitFor(() => expect(screen.queryByTestId("usage-cost-enable-pricing")).toBeNull());
    await waitFor(() =>
      expect(screen.getAllByTestId(/^usage-cost-row-/).length).toBeGreaterThan(0)
    );
    expect(screen.getByTestId("usage-cost-refreshing")).toHaveTextContent("正在刷新");

    if (original) mockedListen.mockImplementation(original);
    restore();
  });
});
