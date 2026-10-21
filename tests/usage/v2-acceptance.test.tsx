// Task 19 / Task 11 · V2 端到端验收（合成夹具 + 真实 IPC 包装）+ 双语文案逐键核对。
//
// 前一组（Task 19）串起：未知身份 → 预览绑定 → 确认保存 → 趋势/分布失效更新 → D 同模型
// 不同渠道两行 → 撤销 → 原 Token 总量恢复同一值；并验证预览不改归属、确认失败不乐观篡改。
//
// 后一组（Task 11）是跨模块验收：别名设置（设置页 / 记录页浮窗）→ C/D 分组变化且 Token
// 总量不变；空 provider 逐条指派回溯；显式计费绑定优先于别名；自动定价与手动覆盖价；
// 空模型 / 同名模型 / 原始 provider 证据面；记录页行名别名优先但原始身份与 title 保留；费用 CSV 端到端。
// 纪律：只消费 mock 夹具与既有组件，不 mock 组件内部实现；产品代码零改动。
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ReactNode } from "react";
import { tauriInvokeMock } from "../msw/tauriMocks";
import enLocale from "@/i18n/locales/en.json";
import zhLocale from "@/i18n/locales/zh.json";
import i18n from "@/i18n";
import { UsageCostCard } from "@/components/usage/UsageCostCard";
import { PricingStatusPanel } from "@/components/usage/PricingStatusPanel";
import { UsageSection } from "@/components/settings/UsageSection";
import UsageDashboardPage from "@/pages/usage-dashboard";
import {
  usageBillingCommit,
  usageBillingGet,
  usageBillingPreview,
  usageBillingUndo,
  usageCosts,
  usageDistribution,
  usageIdentities,
  usageProviderAliasUpsert,
  usageTrend,
} from "@/lib/api/usageV2";
import { organizeCosts } from "@/lib/usage/costRows";
import { formatMoney } from "@/lib/usage/displayCurrency";
import {
  mockObservedEntriesGet,
  mockPricingOverridesGet,
  mockPricingRefresh,
  mockPricingSetEnabled,
  mockProviderAliasesGet,
  mockUsageDashboard,
  mockUsageRecords,
  resetUsageV2MockState,
} from "@/lib/usage/mockFixtures";
import { USAGE_KEY } from "@/lib/query/queries/usage";
import type {
  UsageCard,
  UsageDashboard,
  UsageFilters,
  UsageRange,
  UsageSourceId,
} from "@/types/usage";
import type {
  BillingConfig,
  BillingRule,
  ChannelKind,
  CostRow,
  DistributionGroup,
  DistributionRow,
  IdentityUsage,
  ObservedModelEntry,
  PricingRule,
  TrendSelection,
} from "@/types/usageV2";

// jsdom 未实现 matchMedia（设置页 / WindowFrame 需要；与其余 usage 测试同模式）
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

const RANGE: UsageRange = { preset: "last7d" };
const ALL: TrendSelection = { scope: "all" };
const FILTERS: UsageFilters = {};
const TOTAL = 1_000;

const BUCKETS = { inputFresh: 600, cacheRead: 100, cacheWrite: 0, output: 300 };
const METRICS = {
  requestTotal: 700,
  cacheHitRate: 100 / 700,
  userEst: null,
  requests: 2,
};

const PROFILE_A = {
  id: "p1",
  name: "Team API",
  kind: "api" as const,
  note: "",
  archived: false,
};
const PROFILE_B = {
  id: "p2",
  name: "Backup API",
  kind: "api" as const,
  note: "",
  archived: false,
};

const RULE: BillingRule = {
  id: "rule-1",
  identity: { sourceId: "codex", observedProvider: null, modelId: "gpt-5" },
  match: "exact",
  billingProfileId: "p1",
  effectiveFrom: 0,
  effectiveTo: null,
  mixed: false,
};

function identity(bound: boolean): IdentityUsage {
  return {
    identity: { sourceId: "codex", observedProvider: null, modelId: "gpt-5" },
    sourceKind: "measured",
    attribution: {
      billingProfileId: bound ? "p1" : null,
      attributionKind: bound ? "userAssigned" : "unknown",
      ruleIds: bound ? ["rule-1"] : [],
      reason: bound ? null : "unassigned",
    },
    buckets: { ...BUCKETS },
    metrics: { ...METRICS },
  };
}

function distributionRow(bound: boolean): DistributionRow {
  return {
    key: bound ? "bp:p1" : "bp:unknown",
    label: bound ? "Team API" : "usage.label.unassignedChannel",
    sourceIds: ["codex"],
    billingProfileId: bound ? "p1" : null,
    attributionKind: bound ? "userAssigned" : "unknown",
    buckets: { ...BUCKETS },
    metrics: { ...METRICS },
    channelKey: bound ? "bp:p1" : "unassigned",
    channelKind: bound ? "billingProfile" : "unassigned",
    displayProvider: bound ? "Team API" : null,
    observedProvider: null,
    observedProviders: [],
    hasEmptyProvider: false,
    modelId: null,
  };
}

function costRow(key: string, profileId: string, tokenUsage: number): CostRow {
  const profile = profileId === "p1" ? PROFILE_A : PROFILE_B;
  return {
    key,
    identity: { sourceId: "codex", observedProvider: null, modelId: "gpt-5" },
    sourceIds: ["codex"],
    billingProfileId: profileId,
    attributionKind: "userAssigned",
    tokenUsage,
    cacheHitRate: 0.2,
    estimatedUsd: "0.000529",
    status: "priced",
    pricedTokens: tokenUsage,
    unpricedTokens: 0,
    coverageRate: 1,
    pricingReference: { providerId: "openai", modelId: "gpt-5" },
    pricingSnapshotId: "snap-1",
    pricingBasis: "baseCurrent",
    priceUpdatedAt: 1_700_000_000_000,
    reasons: [],
    components: { input: null, cacheRead: null, cacheWrite: null, output: null },
    channelKey: `bp:${profileId}`,
    channelKind: "billingProfile",
    displayProvider: profile.name,
    observedProviders: [],
    hasEmptyProvider: false,
    normalizedModelId: "gpt-5",
    pricingReleaseDate: "2025-08-07",
    priceOverride: false,
    priceRuleKind: "none",
    timeBreakdown: null,
  };
}

// ── 有状态的双端 mock（只覆写本验收需要的命令）────────────────────────────
let revision = 0;
let bound = false;
let failCommit = false;
let config: BillingConfig = emptyConfig();
let trendCalls = 0;
let distributionCalls = 0;

function emptyConfig(): BillingConfig {
  return { revision: 0, profiles: [], rules: [], pricingRules: [] };
}

function candidate(): BillingConfig {
  return { revision: 0, profiles: [PROFILE_A], rules: [RULE], pricingRules: [] };
}

type Impl = (cmd: string, args?: unknown) => Promise<unknown>;

function installStatefulMock() {
  const original = tauriInvokeMock.getMockImplementation() as
    ((cmd: string, args?: unknown) => Promise<unknown>) | undefined;

  const handlers: Record<string, Impl> = {
    usage_billing_get: () => Promise.resolve({ ...config }),
    usage_billing_preview: (_cmd, args) => {
      const request = args as { candidate: BillingConfig; range: UsageRange };
      return Promise.resolve({
        baseRevision: revision,
        candidate: { ...request.candidate, revision: revision + 1 },
        matchedIdentities: [identity(true)],
        matchedTokens: TOTAL,
        ambiguousTokens: 0,
        boundaryTokens: 0,
      });
    },
    usage_billing_commit: (_cmd, args) => {
      const request = (args as { request: { expectedRevision: number; candidate: BillingConfig } })
        .request;
      if (failCommit || request.expectedRevision !== revision) {
        return Promise.reject({
          code: "usage-settings-invalid",
          detail: "billing revision conflict",
        });
      }
      revision += 1;
      config = { ...request.candidate, revision };
      bound = true;
      return Promise.resolve({ revision, committedAt: revision });
    },
    usage_billing_undo: (_cmd, args) => {
      const request = args as { expectedRevision: number; targetRevision: number };
      if (request.expectedRevision !== revision) {
        return Promise.reject({
          code: "usage-settings-invalid",
          detail: "billing revision conflict",
        });
      }
      const nextRevision = revision + 1;
      if (request.targetRevision === 0) {
        config = { ...emptyConfig(), revision: nextRevision };
        bound = false;
      } else {
        config = { ...config, revision: nextRevision };
      }
      revision = nextRevision;
      return Promise.resolve({ revision, committedAt: revision });
    },
    usage_identities: () => Promise.resolve([identity(bound)]),
    usage_trend: (_cmd, args) => {
      trendCalls += 1;
      return Promise.resolve({
        selection: (args as { selection: TrendSelection }).selection,
        trend: [
          { key: "2026-10-17", label: "10/17", buckets: { ...BUCKETS }, metrics: { ...METRICS } },
        ],
        hero: TOTAL,
        compare: null,
        available: true,
        reason: null,
      });
    },
    usage_distribution: (_cmd, args) => {
      distributionCalls += 1;
      return Promise.resolve({
        groupBy: (args as { groupBy: string }).groupBy,
        rows: [distributionRow(bound)],
      });
    },
    usage_costs: () =>
      Promise.resolve({
        range: RANGE,
        rows: bound
          ? [costRow("model:p1:gpt-5", "p1", 600), costRow("model:p2:gpt-5", "p2", 400)]
          : [],
        pricing: {
          enabled: true,
          snapshotId: "snap-1",
          fetchedAt: 1_700_000_000_000,
          stale: false,
          refreshing: false,
          lastError: null,
        },
      }),
  };

  tauriInvokeMock.mockImplementation((cmd: string, args?: unknown) =>
    handlers[cmd] ? handlers[cmd](cmd, args) : (original?.(cmd, args) ?? Promise.resolve())
  );
  return () => {
    if (original) tauriInvokeMock.mockImplementation(original);
  };
}

type InvokeCall = [string, Record<string, unknown> | undefined];

function callsOf(cmd: string): InvokeCall[] {
  return tauriInvokeMock.mock.calls.filter(([c]) => c === cmd) as InvokeCall[];
}

let defaultImpl: ((cmd: string, args?: unknown) => Promise<unknown>) | undefined;
let restoreStateful: (() => void) | undefined;

beforeAll(async () => {
  await i18n.changeLanguage("zh");
});

beforeEach(() => {
  localStorage.clear();
  resetUsageV2MockState();
  revision = 0;
  bound = false;
  failCommit = false;
  config = emptyConfig();
  trendCalls = 0;
  distributionCalls = 0;
  tauriInvokeMock.mockClear();
  defaultImpl = tauriInvokeMock.getMockImplementation() as
    ((cmd: string, args?: unknown) => Promise<unknown>) | undefined;
});

afterEach(() => {
  restoreStateful?.();
  restoreStateful = undefined;
  if (defaultImpl) tauriInvokeMock.mockImplementation(defaultImpl);
  cleanup();
});

function makeClient() {
  return new QueryClient({ defaultOptions: { queries: { retry: false } } });
}

function wrap(children: ReactNode, client = makeClient()) {
  return <QueryClientProvider client={client}>{children}</QueryClientProvider>;
}

/** 在既有 mock 路由上覆写指定命令；返回恢复函数（Task 11 用例共用）。 */
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

describe("V2 验收：未知身份 → 绑定 → 失效更新 → 撤销", () => {
  beforeEach(() => {
    restoreStateful = installStatefulMock();
  });

  it("1. 预览不改归属；确认后 trend/distribution 失效更新；D 两渠道；撤销后 Token 总量不变", async () => {
    const client = makeClient();
    const trendKey = ["usage", "v2", "trend", RANGE, ALL] as const;
    const distKey = ["usage", "v2", "distribution", RANGE, "billingProfile"] as const;

    // 初始：未知身份 + 原 Token 总量
    expect((await usageIdentities(RANGE, FILTERS))[0].attribution.attributionKind).toBe("unknown");
    const trendBefore = await client.fetchQuery({
      queryKey: trendKey,
      queryFn: () => usageTrend(RANGE, ALL),
    });
    const distBefore = await client.fetchQuery({
      queryKey: distKey,
      queryFn: () => usageDistribution(RANGE, "billingProfile"),
    });
    expect(trendBefore.hero).toBe(TOTAL);
    expect(distBefore.rows[0].billingProfileId).toBeNull();
    expect(distBefore.rows[0].label).toBe("usage.label.unassignedChannel");
    expect(trendCalls).toBe(1);
    expect(distributionCalls).toBe(1);

    // 预览：不写库、不改归属
    const preview = await usageBillingPreview(candidate(), RANGE);
    expect(preview.baseRevision).toBe(0);
    expect((await usageBillingGet()).rules).toHaveLength(0);
    expect((await usageIdentities(RANGE, FILTERS))[0].attribution.attributionKind).toBe("unknown");

    // 确认保存
    await usageBillingCommit({
      expectedRevision: preview.baseRevision,
      candidate: preview.candidate,
    });
    expect((await usageBillingGet()).revision).toBe(1);
    const assigned = (await usageIdentities(RANGE, FILTERS))[0];
    expect(assigned.attribution.attributionKind).toBe("userAssigned");
    expect(assigned.attribution.billingProfileId).toBe("p1");
    // Token 总量不变
    expect(assigned.metrics.requestTotal + assigned.buckets.output).toBe(TOTAL);

    // 失效后 trend/distribution 重取并反映新归属
    await client.invalidateQueries({ queryKey: USAGE_KEY });
    expect(client.getQueryState(trendKey)?.isInvalidated).toBe(true);
    expect(client.getQueryState(distKey)?.isInvalidated).toBe(true);
    const trendAfter = await client.fetchQuery({
      queryKey: trendKey,
      queryFn: () => usageTrend(RANGE, ALL),
    });
    const distAfter = await client.fetchQuery({
      queryKey: distKey,
      queryFn: () => usageDistribution(RANGE, "billingProfile"),
    });
    expect(trendCalls).toBeGreaterThan(1);
    expect(distributionCalls).toBeGreaterThan(1);
    expect(trendAfter.hero).toBe(TOTAL);
    expect(distAfter.rows[0].billingProfileId).toBe("p1");
    expect(distAfter.rows[0].label).toBe("Team API");
    expect(distAfter.rows[0].metrics.requestTotal + distAfter.rows[0].buckets.output).toBe(TOTAL);

    // D：同模型不同渠道 → channel 两个分组、model 两行
    const costs = await usageCosts(RANGE);
    expect(costs.rows).toHaveLength(2);
    expect(new Set(costs.rows.map((row) => row.identity.modelId))).toEqual(new Set(["gpt-5"]));
    const channelView = organizeCosts(costs.rows, "channel");
    expect(channelView.map((group) => group.key).sort()).toEqual(["bp:p1", "bp:p2"]);
    expect(channelView.map((group) => group.label).sort()).toEqual(["Backup API", "Team API"]);
    const modelView = organizeCosts(costs.rows, "model");
    expect(modelView).toHaveLength(2);

    // 撤销 → 归属回到 unknown；Token 总量恢复同一值
    await usageBillingUndo(1, 0);
    expect((await usageBillingGet()).revision).toBe(2);
    expect((await usageIdentities(RANGE, FILTERS))[0].attribution.attributionKind).toBe("unknown");
    await client.invalidateQueries({ queryKey: USAGE_KEY });
    const trendUndone = await client.fetchQuery({
      queryKey: trendKey,
      queryFn: () => usageTrend(RANGE, ALL),
    });
    const distUndone = await client.fetchQuery({
      queryKey: distKey,
      queryFn: () => usageDistribution(RANGE, "billingProfile"),
    });
    expect(trendUndone.hero).toBe(TOTAL);
    expect(distUndone.rows[0].billingProfileId).toBeNull();
    expect(distUndone.rows[0].metrics.requestTotal + distUndone.rows[0].buckets.output).toBe(TOTAL);
  }, 15_000); // 用例级 timeout：串行 8 次 IPC + 3 轮缓存失效/重取，整库并行跑时
  // 默认 5s 预算会被 CPU 饥饿吃满（2026-10-19 最终回归实测 Test timed out）；
  // 只放宽等待上限，断言语义不变。

  it("2. 确认失败不乐观篡改成功态", async () => {
    const client = makeClient();
    const identityKey = ["usage", "v2", "identities", RANGE, FILTERS] as const;
    await client.fetchQuery({
      queryKey: identityKey,
      queryFn: () => usageIdentities(RANGE, FILTERS),
    });
    const cachedBefore = client.getQueryData<IdentityUsage[]>(identityKey);

    const preview = await usageBillingPreview(candidate(), RANGE);
    failCommit = true;
    await expect(
      usageBillingCommit({
        expectedRevision: preview.baseRevision,
        candidate: preview.candidate,
      })
    ).rejects.toMatchObject({ code: "usage-settings-invalid" });

    // 服务端状态与查询缓存都保持原样（没有乐观写成功）
    expect((await usageBillingGet()).revision).toBe(0);
    expect((await usageBillingGet()).rules).toHaveLength(0);
    expect((await usageIdentities(RANGE, FILTERS))[0].attribution.attributionKind).toBe("unknown");
    expect(client.getQueryData<IdentityUsage[]>(identityKey)).toEqual(cachedBefore);
  });

  it("3. mock key 与真实 IPC 参数一一对应", async () => {
    const preview = await usageBillingPreview(candidate(), RANGE);
    await usageBillingCommit({
      expectedRevision: preview.baseRevision,
      candidate: preview.candidate,
    });
    await usageBillingUndo(revision, 0);
    await usageIdentities(RANGE, FILTERS);
    await usageTrend(RANGE, ALL);
    await usageDistribution(RANGE, "billingProfile");
    await usageCosts(RANGE);

    const shape = (cmd: string) => callsOf(cmd).map((call) => Object.keys(call[1] ?? {}).sort());
    expect(shape("usage_billing_preview")).toEqual([["candidate", "range"]]);
    expect(shape("usage_billing_commit")).toEqual([["request"]]);
    expect(shape("usage_billing_undo")).toEqual([["expectedRevision", "targetRevision"]]);
    expect(shape("usage_identities")).toEqual([["filters", "range"]]);
    expect(shape("usage_trend")).toEqual([["range", "selection"]]);
    expect(shape("usage_distribution")).toEqual([["groupBy", "range"]]);
    // 2026-10-20 契约：费用查询携带备用汇率（无快照汇率时后端用它换算 CNY）
    expect(shape("usage_costs")).toEqual([["fallbackRate", "range"]]);
  });
});

// ── Task 11 · 跨模块验收（页面级）──────────────────────────────────────────
//
// 串起「设置页 / 记录页浮窗 → 查询失效 → C/D/B 展示」。后端解析口径（别名优先级、归一化
// 定价、覆盖价）由本组的状态化 mock 按 Rust 契约模拟；前端只消费既有 IPC 形状，产品代码零改动。
// Token 总量一律用 hero / C 行值 `requestTotal + output` 断言，证明别名只改展示、不改口径。

const tf = i18n.t.bind(i18n);
const TM_MODEL = "gpt-6.1-sol";

function identityOf(
  sourceId: "codex" | "claude",
  observedProvider: string | null,
  modelId: string,
  requestTotal: number,
  output: number,
  attribution: IdentityUsage["attribution"] = {
    billingProfileId: null,
    attributionKind: observedProvider === null ? "unknown" : "observed",
    ruleIds: [],
    reason: observedProvider === null ? "unassigned" : null,
  }
): IdentityUsage {
  return {
    identity: { sourceId, observedProvider, modelId },
    sourceKind: "measured",
    attribution,
    buckets: { inputFresh: requestTotal, cacheRead: 0, cacheWrite: 0, output },
    metrics: { requestTotal, cacheHitRate: 0, userEst: null, requests: 1 },
  };
}

/** A 的 hero = requestTotal + output；其余字段沿用夹具（只做总量断言，不参与本组断言）。 */
function dashboardOf(requestTotal: number, output: number): UsageDashboard {
  const base = mockUsageDashboard(RANGE, "tool");
  return {
    ...base,
    hero: requestTotal + output,
    totals: { ...base.totals, requestTotal },
    totalsBuckets: { ...base.totalsBuckets, output },
  };
}

function distRow(opts: {
  key: string;
  label?: string;
  sourceIds: UsageSourceId[];
  value: number;
  channelKey: string;
  channelKind: ChannelKind;
  displayProvider: string | null;
  observedProvider?: string | null;
  observedProviders?: string[];
  hasEmptyProvider?: boolean;
  modelId?: string | null;
}): DistributionRow {
  const profileId = opts.channelKey.startsWith("bp:") ? opts.channelKey.slice(3) : null;
  return {
    key: opts.key,
    label: opts.label ?? opts.displayProvider ?? "",
    sourceIds: opts.sourceIds,
    billingProfileId: profileId,
    attributionKind:
      opts.channelKind === "billingProfile"
        ? "userAssigned"
        : opts.channelKind === "unassigned"
          ? "unknown"
          : "observed",
    buckets: { inputFresh: opts.value, cacheRead: 0, cacheWrite: 0, output: 0 },
    metrics: { requestTotal: opts.value, cacheHitRate: 0, userEst: null, requests: 1 },
    channelKey: opts.channelKey,
    channelKind: opts.channelKind,
    displayProvider: opts.displayProvider,
    observedProvider: opts.observedProvider ?? null,
    observedProviders:
      opts.observedProviders ?? (opts.observedProvider ? [opts.observedProvider] : []),
    hasEmptyProvider: opts.hasEmptyProvider ?? false,
    modelId: opts.modelId ?? null,
  };
}

function costOf(opts: {
  key: string;
  sourceId: "codex" | "claude";
  observedProvider: string | null;
  observedProviders?: string[];
  hasEmptyProvider?: boolean;
  modelId: string;
  tokenUsage: number;
  channelKey: string;
  channelKind: ChannelKind;
  displayProvider: string | null;
  normalizedModelId?: string | null;
  priceOverride?: boolean;
  pricingBasis?: CostRow["pricingBasis"];
  estimatedUsd?: string;
  inputUnit?: string;
  outputUnit?: string;
}): CostRow {
  const profileId = opts.channelKey.startsWith("bp:") ? opts.channelKey.slice(3) : null;
  return {
    key: opts.key,
    identity: {
      sourceId: opts.sourceId,
      observedProvider: opts.observedProvider,
      modelId: opts.modelId,
    },
    sourceIds: [opts.sourceId],
    billingProfileId: profileId,
    attributionKind: profileId ? "userAssigned" : opts.observedProvider ? "observed" : "unknown",
    tokenUsage: opts.tokenUsage,
    cacheHitRate: 0.2,
    estimatedUsd: opts.estimatedUsd ?? "0.000529",
    status: "priced",
    pricedTokens: opts.tokenUsage,
    unpricedTokens: 0,
    coverageRate: 1,
    pricingReference:
      opts.normalizedModelId === null
        ? null
        : { providerId: "openai", modelId: opts.normalizedModelId ?? "gpt-5" },
    pricingSnapshotId: "snap-acceptance",
    pricingBasis: opts.pricingBasis ?? "baseCurrent",
    priceUpdatedAt: 1_700_000_000_000,
    reasons: [],
    components: {
      input: {
        tokens: opts.tokenUsage,
        unitUsdPerMillion: opts.inputUnit ?? "1.25",
        estimatedUsd: "0.0002",
      },
      cacheRead: null,
      cacheWrite: null,
      output: { tokens: 0, unitUsdPerMillion: opts.outputUnit ?? "10", estimatedUsd: "0" },
    },
    channelKey: opts.channelKey,
    channelKind: opts.channelKind,
    displayProvider: opts.displayProvider,
    observedProviders:
      opts.observedProviders ?? (opts.observedProvider ? [opts.observedProvider] : []),
    hasEmptyProvider: opts.hasEmptyProvider ?? false,
    normalizedModelId: opts.normalizedModelId ?? "gpt-5",
    pricingReleaseDate: "2025-08-07",
    priceOverride: opts.priceOverride ?? false,
    priceRuleKind: opts.priceRuleKind ?? "none",
    timeBreakdown: opts.timeBreakdown ?? null,
  };
}

/** 当前 mock 别名存储里的 provider 显示名（未配置时 = 原始串）。 */
function providerDisplay(provider: string): string {
  const rule = mockProviderAliasesGet().aliases.find(
    (item) => item.kind === "provider" && item.provider === provider
  );
  return rule?.displayName ?? provider;
}

/** 当前 mock 别名存储里的「工具 + 模型」指派（未指派 = null）。 */
function toolModelDisplay(sourceId: string, modelId: string): string | null {
  const rule = mockProviderAliasesGet().aliases.find(
    (item) => item.kind === "toolModel" && item.sourceId === sourceId && item.modelId === modelId
  );
  return rule?.displayName ?? null;
}

/** 记录页卡片：每卡按给定行（label 原文 = 真机 `provider / model` 或纯模型）重建。 */
function recordsWithRows(
  rows: Array<{
    card: "codex" | "claude";
    label: string;
    requestTotal: number;
    output: number;
  }>
): ReturnType<typeof mockUsageRecords> {
  const base = mockUsageRecords(RANGE, "tool", {});
  const byTool = new Map<string, typeof rows>();
  for (const row of rows) byTool.set(row.card, [...(byTool.get(row.card) ?? []), row]);
  const cards: UsageCard[] = [...byTool.entries()].map(([toolId, cardRows]) => {
    const source = base.cards.find((card) => card.toolId === toolId)!;
    const totalRequests = cardRows.reduce((sum, row) => sum + row.requestTotal, 0);
    const totalOutput = cardRows.reduce((sum, row) => sum + row.output, 0);
    return {
      ...source,
      buckets: { inputFresh: totalRequests, cacheRead: 0, cacheWrite: 0, output: totalOutput },
      metrics: {
        requestTotal: totalRequests,
        cacheHitRate: 0,
        userEst: null,
        requests: cardRows.length,
      },
      rows: cardRows.map((row, i) => ({
        key: `${toolId}:${i}`,
        label: row.label,
        buckets: { inputFresh: row.requestTotal, cacheRead: 0, cacheWrite: 0, output: row.output },
        metrics: { requestTotal: row.requestTotal, cacheHitRate: 0, userEst: null, requests: 1 },
        sourceKind: "measured" as const,
        isSubagent: null,
      })),
    };
  });
  return { ...base, cards };
}

const PRICING_FRESH = {
  enabled: true,
  snapshotId: "snap-acceptance",
  fetchedAt: 1_700_000_000_000,
  stale: false,
  refreshing: false,
  lastError: null,
};

describe("Task 11 验收：供应商别名与自动定价（跨模块）", () => {
  it("1. 设置页保存 provider 别名后 C/D 显示新渠道名，总 Token（requestTotal+output）不变", async () => {
    const PROVIDER = "deepseek";
    const distributionCalls: unknown[] = [];
    const restore = override({
      usage_dashboard: () => Promise.resolve(dashboardOf(100, 40)),
      usage_identities: () => Promise.resolve([identityOf("codex", PROVIDER, "gpt-5", 100, 40)]),
      // 条目行来自观察条目（全历史）；deepseek 必须在列才有可编辑行。
      // displayProvider 随别名状态派生（真机后端语义：保存后重取回显新名）。
      usage_observed_entries_get: () =>
        Promise.resolve([
          {
            providerRaw: PROVIDER,
            sourceId: null,
            modelId: "gpt-5",
            normalizedModelId: "gpt-5",
            displayProvider: providerDisplay(PROVIDER),
            displayModelName: null,
            pricingProvider: null,
            pricingModel: null,
            totalTokens: 140,
            recentTokens7d: 140,
          },
        ]),
      usage_distribution: (_cmd, args) => {
        const groupBy = (args as { groupBy: DistributionGroup }).groupBy;
        distributionCalls.push(args);
        const name = providerDisplay(PROVIDER);
        const kind: ChannelKind = name === PROVIDER ? "provider" : "alias";
        return Promise.resolve({
          groupBy,
          rows: [
            distRow({
              key: groupBy === "model" ? `provider:${name}|gpt-5` : `provider:${name}`,
              sourceIds: ["codex"],
              value: 140,
              channelKey: `provider:${name}`,
              channelKind: kind,
              displayProvider: name,
              observedProvider: PROVIDER,
              modelId: groupBy === "model" ? "gpt-5" : null,
            }),
          ],
        });
      },
      usage_costs: () => {
        const name = providerDisplay(PROVIDER);
        return Promise.resolve({
          range: RANGE,
          rows: [
            costOf({
              key: `provider:${name}:gpt-5`,
              sourceId: "codex",
              observedProvider: PROVIDER,
              modelId: "gpt-5",
              tokenUsage: 140,
              channelKey: `provider:${name}`,
              channelKind: name === PROVIDER ? "provider" : "alias",
              displayProvider: name,
            }),
          ],
          pricing: PRICING_FRESH,
        });
      },
    });

    render(
      wrap(
        <>
          <UsageSection />
          <UsageDashboardPage />
        </>
      )
    );

    // 设置前：原始 provider 作为渠道名；总 Token = 100 + 40
    expect(await screen.findByTestId("usage-hero")).toHaveTextContent("140");
    await waitFor(() =>
      expect(screen.getByTestId("usage-dist-provider:deepseek")).toHaveTextContent("deepseek")
    );
    expect(screen.getByTestId("usage-val-provider:deepseek")).toHaveTextContent("140");

    // D 默认展开：设置前渠道组 = 原始串
    const groupsBefore = await screen.findByTestId("usage-cost-groups");
    expect(within(groupsBefore).getByText("deepseek")).toBeTruthy();
    // 渠道视图行名只留模型，渠道由组头给出。
    expect(screen.getByTestId("usage-cost-name-provider:deepseek:gpt-5")).toHaveTextContent(
      /^gpt-5$/
    );

    // 设置页配置弹窗条目内改供应商名（走真实 mock 命令与 USAGE_KEY 失效；
    // 2026-10-20 同日裁决：A 段删除，供应商改名收进条目 row2 输入框；第三轮：共用配置弹窗）
    fireEvent.click(await screen.findByTestId("usage-config-open-settings"));
    await screen.findByTestId("usage-config-dialog-settings");
    await screen.findByTestId("usage-config-panel-settings");
    const aliasInput = await screen.findByTestId(
      "usage-config-model-provider-input-d-deepseek-gpt-5"
    );
    fireEvent.change(aliasInput, { target: { value: "DeepSeek Pro" } });
    fireEvent.blur(aliasInput);
    await waitFor(() => expect(callsOf("usage_provider_alias_upsert")).toHaveLength(1));
    expect(callsOf("usage_provider_alias_upsert")[0][1].rule).toMatchObject({
      kind: "provider",
      provider: PROVIDER,
      sourceId: null,
      modelId: null,
      displayName: "DeepSeek Pro",
    });

    // C：新渠道名；title 保留原始 provider 证据；Token 总量不变
    const renamed = await screen.findByTestId("usage-dist-provider:DeepSeek Pro");
    expect(renamed).toHaveTextContent("DeepSeek Pro");
    expect(renamed).toHaveAttribute("title", expect.stringContaining("deepseek"));
    expect(screen.getByTestId("usage-val-provider:DeepSeek Pro")).toHaveTextContent("140");
    expect(screen.getByTestId("usage-hero")).toHaveTextContent("140");
    expect(screen.queryByTestId("usage-dist-provider:deepseek")).toBeNull();

    // D：组头与行名同步换新渠道名；金额/Token 不变
    await waitFor(() => {
      const groups = screen.getByTestId("usage-cost-groups");
      expect(within(groups).getByText("DeepSeek Pro")).toBeTruthy();
      // 渠道视图行名只留模型；完整渠道/证据在 title。
      expect(screen.getByTestId("usage-cost-name-provider:DeepSeek Pro:gpt-5")).toHaveTextContent(
        /^gpt-5$/
      );
      expect(screen.getByTestId("usage-cost-name-provider:DeepSeek Pro:gpt-5")).toHaveAttribute(
        "title",
        "DeepSeek Pro / deepseek / gpt-5"
      );
      expect(within(groups).queryByText("deepseek")).toBeNull();
    });
    expect(screen.getByTestId("usage-cost-token-provider:DeepSeek Pro:gpt-5")).toHaveTextContent(
      "140"
    );
    expect(distributionCalls.length).toBeGreaterThan(1);

    restore();
  });

  it("2. 空 provider 逐条指派回溯生效；未指派时渠道合并、模型按工具拆行", async () => {
    const restore = override({
      usage_dashboard: () => Promise.resolve(dashboardOf(150, 50)),
      usage_identities: (_cmd, args) => {
        const filters = (args as { filters?: UsageFilters }).filters ?? {};
        const all = [
          identityOf("codex", null, TM_MODEL, 100, 40),
          identityOf("claude", null, TM_MODEL, 50, 10),
        ];
        return Promise.resolve(
          filters.toolIds ? all.filter((i) => filters.toolIds!.includes(i.identity.sourceId)) : all
        );
      },
      usage_records: () =>
        Promise.resolve(
          recordsWithRows([
            { card: "codex", label: TM_MODEL, requestTotal: 100, output: 40 },
            { card: "claude", label: TM_MODEL, requestTotal: 50, output: 10 },
          ])
        ),
      usage_distribution: (_cmd, args) => {
        const groupBy = (args as { groupBy: DistributionGroup }).groupBy;
        const entries = [
          { sourceId: "codex" as const, value: 140 },
          { sourceId: "claude" as const, value: 60 },
        ];
        if (groupBy === "billingProfile") {
          const rows: DistributionRow[] = [];
          const unassignedSources: Array<"codex" | "claude"> = [];
          let unassignedValue = 0;
          for (const entry of entries) {
            const name = toolModelDisplay(entry.sourceId, TM_MODEL);
            if (name) {
              rows.push(
                distRow({
                  key: `provider:${name}`,
                  sourceIds: [entry.sourceId],
                  value: entry.value,
                  channelKey: `provider:${name}`,
                  channelKind: "alias",
                  displayProvider: name,
                })
              );
            } else {
              unassignedSources.push(entry.sourceId);
              unassignedValue += entry.value;
            }
          }
          if (unassignedValue > 0) {
            rows.push(
              distRow({
                key: "unassigned",
                label: "usage.label.unassignedChannel",
                sourceIds: unassignedSources,
                value: unassignedValue,
                channelKey: "unassigned",
                channelKind: "unassigned",
                displayProvider: null,
              })
            );
          }
          return Promise.resolve({ groupBy, rows });
        }
        // 模型维度：未指派按工具拆行；指派后该工具行换成显示名
        return Promise.resolve({
          groupBy,
          rows: entries.map((entry) => {
            const name = toolModelDisplay(entry.sourceId, TM_MODEL);
            return distRow({
              key: `${name ? `provider:${name}` : "unassigned"}|${entry.sourceId}|${TM_MODEL}`,
              sourceIds: [entry.sourceId],
              value: entry.value,
              channelKey: name ? `provider:${name}` : "unassigned",
              channelKind: name ? "alias" : "unassigned",
              displayProvider: name,
              modelId: TM_MODEL,
            });
          }),
        });
      },
    });

    render(wrap(<UsageDashboardPage />));

    // 未指派：渠道维度合并一行「未指定渠道」，Token = 200
    expect(await screen.findByTestId("usage-hero")).toHaveTextContent("200");
    await waitFor(() =>
      expect(screen.getByTestId("usage-dist-unassigned")).toHaveTextContent("未指定渠道")
    );
    expect(screen.getByTestId("usage-val-unassigned")).toHaveTextContent("200");

    // 模型维度按工具拆行（同名模型不合并、可区分）
    fireEvent.click(screen.getByTestId("usage-dist-group-model"));
    await waitFor(() =>
      expect(screen.getByTestId(`usage-dist-unassigned|codex|${TM_MODEL}`)).toHaveTextContent(
        `未指定渠道 · Codex / ${TM_MODEL}`
      )
    );
    expect(screen.getByTestId(`usage-dist-unassigned|claude|${TM_MODEL}`)).toHaveTextContent(
      `未指定渠道 · Claude / ${TM_MODEL}`
    );

    // 记录页浮窗逐条指派（空 provider → toolModel 规则）
    fireEvent.click(screen.getByTestId("usage-tab-records"));
    const codexCard = await screen.findByTestId("usage-record-card-codex");
    fireEvent.click(within(codexCard).getByTestId("usage-record-alias-codex:0"));
    const readonly = await screen.findByTestId("alias-readonly-identity");
    expect(readonly).toHaveTextContent("—"); // 原始 provider 为空
    expect(readonly).toHaveTextContent(TM_MODEL);
    fireEvent.change(screen.getByTestId("alias-display-name"), {
      target: { value: "Codex Team" },
    });
    fireEvent.click(screen.getByTestId("alias-save-button"));

    await waitFor(() => expect(callsOf("usage_provider_alias_upsert")).toHaveLength(1));
    expect(callsOf("usage_provider_alias_upsert")[0][1].rule).toMatchObject({
      kind: "toolModel",
      provider: null,
      sourceId: "codex",
      modelId: TM_MODEL,
      displayName: "Codex Team",
    });

    // 记录页行名改为别名优先（toolModel → `别名 / 模型`）；账本原始身份仍在 title；
    // 同一窗口内保存后立即更新（同一条 ["usage"] 失效链）。
    await waitFor(() => expect(screen.queryByTestId("alias-save-button")).toBeNull());
    const codexRow = screen.getByTestId("usage-record-row-codex:0");
    await waitFor(() => expect(codexRow).toHaveTextContent(`Codex Team / ${TM_MODEL}`));
    expect(within(codexRow).getByTitle(TM_MODEL)).toBeTruthy();
    expect(screen.getByTestId("usage-record-alias-codex:0")).toHaveTextContent("编辑");

    // 回溯生效：历史行在 C 的模型维度换成显示名，未指派的 Claude 行保持原样
    fireEvent.click(screen.getByTestId("usage-tab-board"));
    await waitFor(() =>
      expect(
        screen.getByTestId("usage-dist-provider:Codex Team|codex|" + TM_MODEL)
      ).toHaveTextContent(`Codex Team / ${TM_MODEL}`)
    );
    expect(screen.getByTestId(`usage-dist-unassigned|claude|${TM_MODEL}`)).toHaveTextContent(
      `未指定渠道 · Claude / ${TM_MODEL}`
    );

    // 渠道维度：Codex Team（140）+ 未指定渠道（60）
    fireEvent.click(screen.getByTestId("usage-dist-group-billingProfile"));
    await waitFor(() =>
      expect(screen.getByTestId("usage-dist-provider:Codex Team")).toHaveTextContent("Codex Team")
    );
    expect(screen.getByTestId("usage-val-provider:Codex Team")).toHaveTextContent("140");
    expect(screen.getByTestId("usage-dist-unassigned")).toHaveTextContent("未指定渠道");
    expect(screen.getByTestId("usage-val-unassigned")).toHaveTextContent("60");

    // 账本身份仍为原始值（observedProvider 不被别名改写）
    const identities = await usageIdentities(RANGE, {});
    expect(identities.map((item) => item.identity.observedProvider)).toEqual([null, null]);

    restore();
  });

  it("3. 显式计费绑定优先于别名：C/D 显示 profile 名，记录页标记用户指定", async () => {
    await usageProviderAliasUpsert({
      id: "alias-deepseek",
      kind: "provider",
      provider: "deepseek",
      sourceId: null,
      modelId: null,
      displayName: "DeepSeek Alias",
    });
    await usageBillingCommit({
      expectedRevision: 0,
      candidate: { revision: 0, profiles: [PROFILE_A], rules: [], pricingRules: [] },
    });

    const boundIdentity = identityOf("codex", "deepseek", "gpt-5", 100, 40, {
      billingProfileId: "p1",
      attributionKind: "userAssigned",
      ruleIds: ["rule-1"],
      reason: null,
    });

    const restore = override({
      usage_dashboard: () => Promise.resolve(dashboardOf(100, 40)),
      usage_identities: () => Promise.resolve([boundIdentity]),
      usage_records: () =>
        Promise.resolve(
          recordsWithRows([
            { card: "codex", label: "deepseek / gpt-5", requestTotal: 100, output: 40 },
          ])
        ),
      usage_distribution: (_cmd, args) => {
        const groupBy = (args as { groupBy: DistributionGroup }).groupBy;
        return Promise.resolve({
          groupBy,
          rows: [
            distRow({
              key: groupBy === "model" ? "bp:p1|gpt-5" : "bp:p1",
              sourceIds: ["codex"],
              value: 140,
              channelKey: "bp:p1",
              channelKind: "billingProfile",
              displayProvider: "Team API",
              observedProvider: "deepseek",
              modelId: groupBy === "model" ? "gpt-5" : null,
            }),
          ],
        });
      },
      usage_costs: () =>
        Promise.resolve({
          range: RANGE,
          rows: [
            costOf({
              key: "bp:p1:gpt-5",
              sourceId: "codex",
              observedProvider: "deepseek",
              modelId: "gpt-5",
              tokenUsage: 140,
              channelKey: "bp:p1",
              channelKind: "billingProfile",
              displayProvider: "Team API",
            }),
          ],
          pricing: PRICING_FRESH,
        }),
    });

    render(wrap(<UsageDashboardPage />));

    // C 默认渠道维度：显式绑定生效，别名不得覆盖
    expect(await screen.findByTestId("usage-hero")).toHaveTextContent("140");
    await waitFor(() =>
      expect(screen.getByTestId("usage-dist-bp:p1")).toHaveTextContent("Team API")
    );
    expect(screen.queryByText("DeepSeek Alias")).toBeNull();

    // 模型维度
    fireEvent.click(screen.getByTestId("usage-dist-group-model"));
    await waitFor(() =>
      expect(screen.getByTestId("usage-dist-bp:p1|gpt-5")).toHaveTextContent("Team API / gpt-5")
    );

    // D 渠道组头 = profile 名；行名只留模型（完整名称在 title 中）。D 默认展开。
    const costGroups = await screen.findByTestId("usage-cost-groups");
    expect(within(costGroups).getByText("Team API")).toBeTruthy();
    expect(screen.getByTestId("usage-cost-name-bp:p1:gpt-5")).toHaveTextContent(/^gpt-5$/);
    expect(screen.getByTestId("usage-cost-name-bp:p1:gpt-5")).toHaveAttribute(
      "title",
      "Team API / deepseek / gpt-5"
    );
    expect(within(costGroups).queryByText("DeepSeek Alias")).toBeNull();

    // 记录页：行名按供应商别名显示，原始「deepseek / gpt-5」保留在 title；用户指定标记不变
    fireEvent.click(screen.getByTestId("usage-tab-records"));
    const row = await screen.findByTestId("usage-record-row-codex:0");
    expect(row).toHaveTextContent("DeepSeek Alias / gpt-5");
    expect(within(row).getByTitle("deepseek / gpt-5")).toBeTruthy();
    expect(within(row).getByTestId("usage-record-attr-assigned")).toHaveTextContent("用户指定");

    restore();
  });

  it("4. 无 PricingRule 时按归一化模型估价；同模型不同渠道两行同单价；无总额/小计/费用趋势", async () => {
    const configBefore = await usageBillingGet();
    expect(configBefore.pricingRules).toEqual([]);

    const restore = override({
      usage_dashboard: () => Promise.resolve(dashboardOf(200, 50)),
      usage_costs: () =>
        Promise.resolve({
          range: RANGE,
          rows: [
            costOf({
              key: "model:provider:Acme:gpt-5",
              sourceId: "codex",
              observedProvider: null,
              observedProviders: ["acme-raw", "acme-alt"],
              modelId: "gpt-5",
              tokenUsage: 160,
              channelKey: "provider:Acme",
              channelKind: "alias",
              displayProvider: "Acme",
              estimatedUsd: "0.000529",
            }),
            costOf({
              key: "model:provider:Other:gpt-5",
              sourceId: "claude",
              observedProvider: null,
              modelId: "gpt-5",
              tokenUsage: 80,
              channelKey: "provider:Other",
              channelKind: "provider",
              displayProvider: "Other",
              estimatedUsd: "0.0002645",
            }),
          ],
          pricing: PRICING_FRESH,
        }),
    });

    render(wrap(<UsageDashboardPage />));
    await screen.findByTestId("usage-hero");
    // D 默认展开
    await screen.findByTestId("usage-cost-body");

    // 渠道视图：两个渠道组，组头只有显示名（无金额小计）
    expect(await screen.findByTestId("usage-cost-group-provider:Acme")).toBeTruthy();
    expect(screen.getByTestId("usage-cost-group-provider:Other")).toBeTruthy();
    expect(screen.queryByTestId(/usage-cost-(total|subtotal)/)).toBeNull();

    // 模型视图：同归一化模型、不同显示渠道 → 两行
    fireEvent.click(screen.getByTestId("usage-cost-view-model"));
    await waitFor(() => expect(screen.getByTestId("usage-cost-models")).toBeTruthy());
    expect(screen.getByText("Acme / gpt-5")).toBeTruthy();
    expect(screen.getByText("Other / gpt-5")).toBeTruthy();
    expect(screen.getAllByTestId(/^usage-cost-row-/)).toHaveLength(2);

    // 自动匹配证据：同归一化模型全渠道同单价（分项单价一致）、基准价口径一致；
    // 小字只留「归一化模型 + 快照获取时间」——价格来源/模型发布日已按 2026-10-20
    // 冻结口径 §0.6 从 D 卡删除，此处不再断言（字段仍在 wire/CSV，仅 UI 移除）。
    for (const key of ["model:provider:Acme:gpt-5", "model:provider:Other:gpt-5"]) {
      const details = screen.getByTestId(`usage-cost-details-${key}`);
      expect(details).toHaveTextContent("1.25 USD / 1M tokens");
      expect(details).toHaveTextContent("10.00 USD / 1M tokens");
      expect(details).toHaveTextContent("当前基准价回算");
      const footnote = within(details).getByTestId(`usage-cost-table-footnote-${key}`);
      expect(footnote).toHaveTextContent("归一化模型");
      expect(footnote).toHaveTextContent("gpt-5");
      expect(footnote).toHaveTextContent("快照获取时间");
      expect(details.textContent).not.toContain("价格来源");
      expect(details.textContent).not.toContain("模型发布日");
    }
    expect(screen.getByTestId("usage-cost-amount-model:provider:Acme:gpt-5")).toHaveTextContent(
      "$0.000529"
    );
    // title 用 observedProviders 完整证据（多个原始 provider 不丢）
    expect(screen.getByTestId("usage-cost-name-model:provider:Acme:gpt-5")).toHaveAttribute(
      "title",
      "Acme / acme-raw, acme-alt / gpt-5"
    );
    expect(screen.getByTestId("usage-cost-amount-model:provider:Other:gpt-5")).toHaveTextContent(
      "$0.000265"
    );

    // 无全局费用金额、无渠道小计、无费用趋势（折叠钮的 Chevron 图标不算趋势图）
    const costFold = screen.getByTestId("usage-fold-cost");
    expect(costFold.textContent).not.toContain("$总计");
    expect(costFold.querySelector('[data-testid="usage-trend-svg"]')).toBeNull();
    expect(costFold.querySelector('[data-testid="usage-cost-trend"]')).toBeNull();
    expect(screen.queryByTestId(/usage-cost-trend/)).toBeNull();

    restore();
  });

  it("5. 手动覆盖价生效并带 userOverride 标记；刷新定价后覆盖值不变", async () => {
    mockPricingSetEnabled(true);
    mockPricingRefresh();

    const MODEL = "claude-sonnet-4-5";
    const restore = override({
      usage_costs: () => {
        const override = mockPricingOverridesGet().find((item) => item.normalizedModelId === MODEL);
        return Promise.resolve({
          range: RANGE,
          rows: [
            costOf({
              key: `override:${MODEL}`,
              sourceId: "claude",
              observedProvider: "anthropic",
              modelId: MODEL,
              tokenUsage: 100,
              channelKey: "provider:Anthropic",
              channelKind: "provider",
              displayProvider: "Anthropic",
              normalizedModelId: MODEL,
              priceOverride: Boolean(override),
              pricingBasis: override ? "userOverride" : "baseCurrent",
              estimatedUsd: override ? "0.003" : "0.0015",
              inputUnit: override?.input ?? "1",
              outputUnit: override?.output ?? "5",
            }),
          ],
          pricing: PRICING_FRESH,
        });
      },
    });

    render(
      wrap(
        <>
          <PricingStatusPanel t={tf} />
          <UsageCostCard range={RANGE} open onOpenChange={() => {}} t={tf} />
        </>
      )
    );

    // 覆盖前：目录基准价
    await waitFor(() =>
      expect(screen.getByTestId(`usage-cost-amount-override:${MODEL}`)).toHaveTextContent("$0.0015")
    );
    expect(screen.queryByTestId(`usage-cost-override-override:${MODEL}`)).toBeNull();
    expect(screen.getByTestId(`usage-cost-details-override:${MODEL}`)).toHaveTextContent(
      "当前基准价回算"
    );

    // 设置手动覆盖价（真实 mock 命令写入覆盖表并失效 USAGE_KEY）。
    // 2026-10-20 第三轮：Manager 移入 D 卡「管理配置 → 自定义价格规则」；
    // 2026-10-21：状态面板不再渲染迁移提示（旧提示指向已改名的入口）。
    expect(screen.queryByTestId("price-override-manager")).toBeNull();
    expect(screen.queryByText(tf("usage.pricing.overridesMovedHint"))).toBeNull();
    fireEvent.click(screen.getByTestId("usage-cost-manage"));
    fireEvent.click(await screen.findByTestId("usage-config-advanced-cost-toggle"));
    fireEvent.click(await screen.findByTestId("price-override-add"));
    fireEvent.change(screen.getByTestId("price-override-model"), { target: { value: MODEL } });
    fireEvent.change(screen.getByTestId("price-override-input"), { target: { value: "3" } });
    fireEvent.change(screen.getByTestId("price-override-output"), { target: { value: "15" } });
    fireEvent.click(screen.getByTestId("price-override-save"));

    await waitFor(() => expect(callsOf("usage_pricing_override_set")).toHaveLength(1));
    await waitFor(() =>
      expect(screen.getByTestId(`usage-cost-amount-override:${MODEL}`)).toHaveTextContent("$0.003")
    );
    expect(screen.getByTestId(`usage-cost-override-override:${MODEL}`)).toHaveTextContent(
      tf("usage.cost.priceOverride")
    );
    const details = screen.getByTestId(`usage-cost-details-override:${MODEL}`);
    expect(details).toHaveTextContent(tf("usage.cost.userOverride"));
    expect(details).not.toHaveTextContent("当前基准价回算");

    // 刷新价目表：mock 同步只写快照/状态，不碰覆盖表
    const costCallsBefore = callsOf("usage_costs").length;
    fireEvent.click(screen.getByTestId("usage-pricing-refresh"));
    await waitFor(() => expect(callsOf("usage_pricing_refresh")).toHaveLength(1));
    await waitFor(() => expect(callsOf("usage_costs").length).toBeGreaterThan(costCallsBefore));

    expect(mockPricingOverridesGet()).toContainEqual(
      expect.objectContaining({ normalizedModelId: MODEL, input: "3", output: "15" })
    );
    expect(screen.getByTestId(`usage-cost-amount-override:${MODEL}`)).toHaveTextContent("$0.003");
    expect(screen.getByTestId(`usage-cost-override-override:${MODEL}`)).toHaveTextContent(
      tf("usage.cost.priceOverride")
    );

    restore();
  });

  it("6. 空模型显示未知模型；同名模型不重复；原始 provider 保留在 title", async () => {
    const restore = override({
      usage_dashboard: () => Promise.resolve(dashboardOf(500, 100)),
      usage_distribution: (_cmd, args) => {
        const groupBy = (args as { groupBy: DistributionGroup }).groupBy;
        if (groupBy === "billingProfile") {
          return Promise.resolve({
            groupBy,
            rows: [
              distRow({
                key: "provider:Acme",
                sourceIds: ["claude", "codex"],
                value: 500,
                channelKey: "provider:Acme",
                channelKind: "alias",
                displayProvider: "Acme",
                observedProvider: "acme-raw",
                observedProviders: ["acme-raw", "acme-alt"],
              }),
              distRow({
                key: "unassigned",
                label: "usage.label.unassignedChannel",
                sourceIds: ["codex"],
                value: 100,
                channelKey: "unassigned",
                channelKind: "unassigned",
                displayProvider: null,
              }),
            ],
          });
        }
        return Promise.resolve({
          groupBy,
          rows: [
            distRow({
              key: "provider:Acme|",
              sourceIds: ["codex"],
              value: 300,
              channelKey: "provider:Acme",
              channelKind: "alias",
              displayProvider: "Acme",
              observedProvider: "acme-raw",
              observedProviders: ["acme-raw", "acme-alt"],
              modelId: "",
            }),
            // 同一显示名跨工具合并：后端只出一行（sourceIds 保留两个工具）
            distRow({
              key: "provider:Acme|claude-4",
              sourceIds: ["claude", "codex"],
              value: 200,
              channelKey: "provider:Acme",
              channelKind: "alias",
              displayProvider: "Acme",
              observedProvider: "acme-raw",
              observedProviders: ["acme-raw", "acme-alt"],
              modelId: "claude-4",
            }),
            distRow({
              key: "unassigned|codex|",
              sourceIds: ["codex"],
              value: 100,
              channelKey: "unassigned",
              channelKind: "unassigned",
              displayProvider: null,
              modelId: "",
            }),
          ],
        });
      },
    });

    render(wrap(<UsageDashboardPage />));
    await screen.findByTestId("usage-hero");

    // 渠道维度：显示名 + 原始 provider 进 title
    const channel = await screen.findByTestId("usage-dist-provider:Acme");
    expect(channel).toHaveTextContent("Acme");
    expect(channel).toHaveAttribute("title", "Acme · acme-raw, acme-alt");

    // 模型维度：空模型 → 未知模型；同名模型只有一行；未指定渠道附工具
    fireEvent.click(screen.getByTestId("usage-dist-group-model"));
    await waitFor(() =>
      expect(screen.getByTestId("usage-dist-provider:Acme|")).toHaveTextContent("Acme / 未知模型")
    );
    expect(screen.getByTestId("usage-dist-provider:Acme|")).toHaveAttribute(
      "title",
      "acme-raw, acme-alt / 未知模型"
    );
    expect(screen.getAllByText("Acme / claude-4")).toHaveLength(1);
    expect(screen.getByTestId("usage-dist-unassigned|codex|")).toHaveTextContent(
      "未指定渠道 · Codex / 未知模型"
    );
    // 3 行：不出现无法区分的重复行
    expect(screen.getAllByTestId(/^usage-dist-(provider:.+\||unassigned\|)/)).toHaveLength(3);

    restore();
  });

  it("7. 记录页行名别名优先，但 title 与账本身份保留原始值（observedProvider 不被改写）", async () => {
    await usageProviderAliasUpsert({
      id: "alias-deepseek",
      kind: "provider",
      provider: "deepseek",
      sourceId: null,
      modelId: null,
      displayName: "DeepSeek Pro",
    });

    const restore = override({
      usage_dashboard: () => Promise.resolve(dashboardOf(100, 40)),
      usage_identities: () => Promise.resolve([identityOf("codex", "deepseek", "gpt-5", 100, 40)]),
      usage_records: () =>
        Promise.resolve(
          recordsWithRows([
            { card: "codex", label: "deepseek / gpt-5", requestTotal: 100, output: 40 },
          ])
        ),
    });

    render(wrap(<UsageDashboardPage />));
    fireEvent.click(await screen.findByTestId("usage-tab-records"));

    // 行名 = 别名优先；原始「deepseek / gpt-5」留在 title 与 IPC 结果里（别名不写账本）
    const row = await screen.findByTestId("usage-record-row-codex:0");
    expect(row).toHaveTextContent("DeepSeek Pro / gpt-5");
    expect(within(row).getByTitle("deepseek / gpt-5")).toBeTruthy();
    // 已有别名 → 按钮为「编辑」
    expect(screen.getByTestId("usage-record-alias-codex:0")).toHaveTextContent("编辑");

    fireEvent.click(screen.getByTestId("usage-record-alias-codex:0"));
    const readonly = await screen.findByTestId("alias-readonly-identity");
    expect(readonly).toHaveTextContent("deepseek");
    expect(readonly).toHaveTextContent("DeepSeek Pro");
    expect((screen.getByTestId("alias-display-name") as HTMLInputElement).value).toBe(
      "DeepSeek Pro"
    );

    // 只读 IPC 面：IdentityUsage 的 observedProvider 仍是原始值
    const identities = await usageIdentities(RANGE, {});
    expect(identities[0].identity.observedProvider).toBe("deepseek");

    restore();
  });

  it("8. 费用 CSV 端到端：追加渠道/来源/覆盖列，无总计行", async () => {
    const LEGACY = [
      "billing_profile_id",
      "billing_profile_name",
      "source_ids",
      "observed_provider",
      "model_id",
      "attribution_kind",
      "token_usage",
      "estimated_usd",
      "status",
      "priced_tokens",
      "unpriced_tokens",
      "coverage_rate",
      "pricing_provider",
      "pricing_model",
      "pricing_snapshot_id",
      "pricing_basis",
      "price_updated_at",
    ];
    const APPENDED = [
      "channel_key",
      "channel_kind",
      "display_provider",
      "normalized_model_id",
      "pricing_release_date",
      "price_override",
      "price_rule_kind",
    ];

    render(wrap(<UsageDashboardPage />));
    await screen.findByTestId("usage-hero");
    fireEvent.click(screen.getByTestId("usage-export-cost-csv"));

    await waitFor(() => expect(callsOf("usage_cost_export_csv")).toHaveLength(1));
    expect(callsOf("usage_cost_export_csv")[0][1]).toEqual({ range: RANGE });
    // 不混用旧 Token CSV
    expect(callsOf("usage_export_csv")).toHaveLength(0);

    const saved = await waitFor(() => {
      const calls = callsOf("export_save_text");
      expect(calls.length).toBeGreaterThan(0);
      return calls[0][1] as { name: string; content: string };
    });
    expect(saved.name).toMatch(/^tuvis-api-equivalent-last7d-\d{8}-\d{6}\.csv$/);

    const lines = saved.content.replace(/\n$/, "").split("\n");
    const header = lines[0].split(",");
    expect(header.slice(0, LEGACY.length)).toEqual(LEGACY);
    expect(header.slice(LEGACY.length)).toEqual(APPENDED);
    expect(saved.content).not.toMatch(/"(?:总计|合计|grand\s+total)"/i);

    // 每行都是一条完整费用行；覆盖列真实反映用户覆盖价
    const rows = lines.slice(1);
    expect(rows.length).toBeGreaterThan(0);
    for (const line of rows) {
      expect(line.split(",")).toHaveLength(header.length);
    }
    expect(saved.content).toMatch(/,"true","fixed"$/m);
  });
});

// ── Task 10 · 集成验收（2026-10-20 计划 §5 T10 / §9）────────────────────────
//
// 串起「配置弹窗两段式 ↔ D 卡单表 ↔ 明细计费回退 ↔ 币种/导出」。后端解析口径
// （别名解析、计价优先级链、明细/日聚合回退）由状态化 mock 按 Rust 契约模拟
// （与上方 Task 11 组同模式；链路真值由 src-tauri/tests/usage_cost_test.rs 锁定），
// 前端只消费既有 IPC 形状，产品代码零改动。
describe("Task 10 验收：配置两段式联动 · 明细计费 · D 卡单表", () => {
  /** 观察条目夹具（providerRaw 默认 deepseek；displayProvider 由别名状态派生）。 */
  function observedEntry(patch: Partial<ObservedModelEntry> = {}): ObservedModelEntry {
    return {
      providerRaw: "deepseek",
      sourceId: null,
      modelId: "gpt-5",
      normalizedModelId: "gpt-5",
      displayProvider: null,
      displayModelName: null,
      pricingProvider: null,
      pricingModel: null,
      totalTokens: 140,
      recentTokens7d: 140,
      ...patch,
    };
  }

  /** dsh 分量夹具：金额 = tokens × 单价，四桶之和恰为 estimatedUsd（合计=分项和可精确断言）。 */
  const dshComp = (tokens: number, unit: string, amount: string) => ({
    tokens,
    unitUsdPerMillion: unit,
    estimatedUsd: amount,
  });

  /** dsh 行（明细路径）：120×1.1 + 40×0.11 + 10×2.2 + 30×3.3 = 0.0002574 USD。 */
  const DSH_FULL: CostRow = {
    key: "d7-dsh",
    identity: { sourceId: "dsh", observedProvider: "zhipu", modelId: "dsh-glm" },
    sourceIds: ["dsh"],
    billingProfileId: null,
    attributionKind: "observed",
    tokenUsage: 200,
    cacheHitRate: 0.25,
    estimatedUsd: "0.0002574",
    status: "priced",
    pricedTokens: 200,
    unpricedTokens: 0,
    coverageRate: 1,
    pricingReference: { providerId: "zhipuai", modelId: "glm-5.3" },
    pricingSnapshotId: "snap-t10",
    pricingBasis: "baseCurrent",
    priceUpdatedAt: 1_700_000_000_000,
    reasons: [],
    components: {
      input: dshComp(120, "1.1", "0.000132"),
      cacheRead: dshComp(40, "0.11", "0.0000044"),
      cacheWrite: dshComp(10, "2.2", "0.000022"),
      output: dshComp(30, "3.3", "0.000099"),
    },
    channelKey: "provider:智谱直连",
    channelKind: "alias",
    displayProvider: "智谱直连",
    observedProviders: ["zhipu"],
    hasEmptyProvider: false,
    normalizedModelId: "dsh-glm",
    pricingReleaseDate: "2025-10-01",
    priceOverride: false,
    priceRuleKind: "none",
    timeBreakdown: null,
    displayModelName: null,
  };

  /** 分项金额字符串求和 → 格式化期望（合计 = 分项和的数值断言基準）。 */
  function sumUsdOf(parts: string[]): string {
    return formatMoney(
      parts.reduce((acc, cur) => acc + Number(cur), 0).toFixed(9),
      "usd",
      "7.2"
    ) as string;
  }

  it("T10-1. A 段改供应商显示名 → B 段展示键与 D 卡行名联动、Token 总量不变；清除回退原名", async () => {
    const restore = override({
      usage_dashboard: () => Promise.resolve(dashboardOf(100, 40)),
      usage_observed_entries_get: () =>
        Promise.resolve([observedEntry({ displayProvider: providerDisplay("deepseek") })]),
      usage_distribution: (_cmd, args) => {
        const groupBy = (args as { groupBy: DistributionGroup }).groupBy;
        const name = providerDisplay("deepseek");
        return Promise.resolve({
          groupBy,
          rows: [
            distRow({
              key: groupBy === "model" ? `provider:${name}|gpt-5` : `provider:${name}`,
              sourceIds: ["codex"],
              value: 140,
              channelKey: `provider:${name}`,
              channelKind: name === "deepseek" ? "provider" : "alias",
              displayProvider: name,
              observedProvider: "deepseek",
              modelId: groupBy === "model" ? "gpt-5" : null,
            }),
          ],
        });
      },
      usage_costs: () => {
        const name = providerDisplay("deepseek");
        return Promise.resolve({
          range: RANGE,
          rows: [
            costOf({
              key: `provider:${name}:gpt-5`,
              sourceId: "codex",
              observedProvider: "deepseek",
              modelId: "gpt-5",
              tokenUsage: 140,
              channelKey: `provider:${name}`,
              channelKind: name === "deepseek" ? "provider" : "alias",
              displayProvider: name,
            }),
          ],
          pricing: PRICING_FRESH,
        });
      },
    });

    render(
      wrap(
        <>
          <UsageSection />
          <UsageDashboardPage />
        </>
      )
    );

    // 初始：条目按（显示供应商, 原始模型）展示（未配置别名 → 显示名 = 原始串）；D 卡组头 = 原始串
    expect(await screen.findByTestId("usage-hero")).toHaveTextContent("140");
    await waitFor(() =>
      expect(screen.getByTestId("usage-dist-provider:deepseek")).toHaveTextContent("deepseek")
    );
    fireEvent.click(await screen.findByTestId("usage-config-open-settings"));
    await screen.findByTestId("usage-config-dialog-settings");
    await screen.findByTestId("usage-config-panel-settings");
    await screen.findByTestId("usage-config-model-entry-d-deepseek-gpt-5");
    expect(
      (screen.getByTestId("usage-config-model-provider-input-d-deepseek-gpt-5") as HTMLInputElement)
        .value
    ).toBe("deepseek");
    expect(screen.getByTestId("usage-config-model-raw-d-deepseek-gpt-5")).toHaveTextContent(
      "deepseek / gpt-5"
    );
    // D 默认展开
    await screen.findByTestId("usage-cost-body");
    expect(within(screen.getByTestId("usage-cost-groups")).getByText("deepseek")).toBeTruthy();

    // 条目内改供应商名：展示键换为（显示供应商, 原始模型），row1 原始证据不动
    fireEvent.change(screen.getByTestId("usage-config-model-provider-input-d-deepseek-gpt-5"), {
      target: { value: "DeepSeek Pro" },
    });
    fireEvent.blur(screen.getByTestId("usage-config-model-provider-input-d-deepseek-gpt-5"));
    await waitFor(() => expect(callsOf("usage_provider_alias_upsert")).toHaveLength(1));

    expect(await screen.findByTestId("usage-config-model-entry-d-DeepSeek-Pro-gpt-5")).toBeTruthy();
    expect(
      (
        screen.getByTestId(
          "usage-config-model-provider-input-d-DeepSeek-Pro-gpt-5"
        ) as HTMLInputElement
      ).value
    ).toBe("DeepSeek Pro");
    expect(screen.queryByTestId("usage-config-model-entry-d-deepseek-gpt-5")).toBeNull();
    expect(screen.getByTestId("usage-config-model-raw-d-DeepSeek-Pro-gpt-5")).toHaveTextContent(
      "deepseek / gpt-5"
    );

    // D 卡联动：组头换新名、行名仍只留模型（证据在 title）；Token 总量不变
    await waitFor(() => {
      const groups = screen.getByTestId("usage-cost-groups");
      expect(within(groups).getByText("DeepSeek Pro")).toBeTruthy();
      expect(within(groups).queryByText("deepseek")).toBeNull();
    });
    expect(screen.getByTestId("usage-cost-name-provider:DeepSeek Pro:gpt-5")).toHaveTextContent(
      /^gpt-5$/
    );
    expect(screen.getByTestId("usage-cost-token-provider:DeepSeek Pro:gpt-5")).toHaveTextContent(
      "140"
    );
    expect(screen.getByTestId("usage-hero")).toHaveTextContent("140");

    // 清除（空值 = 删规则）：条目与 D 卡回退原始名，Token 总量不变
    fireEvent.change(screen.getByTestId("usage-config-model-provider-input-d-DeepSeek-Pro-gpt-5"), {
      target: { value: "" },
    });
    fireEvent.blur(screen.getByTestId("usage-config-model-provider-input-d-DeepSeek-Pro-gpt-5"));
    await waitFor(() => expect(callsOf("usage_provider_alias_delete")).toHaveLength(1));
    await screen.findByTestId("usage-config-model-entry-d-deepseek-gpt-5");
    await waitFor(() =>
      expect(within(screen.getByTestId("usage-cost-groups")).getByText("deepseek")).toBeTruthy()
    );
    expect(screen.getByTestId("usage-hero")).toHaveTextContent("140");

    restore();
  });

  it("T10-2. 空 provider 指派后 B 段可配（工具+模型）；渠道/模型展示换名、原始证据保留", async () => {
    // 指派路径（记录页浮窗 → toolModel 规则）已由上方用例 2 覆盖；此处直接改默认夹具里
    // workbuddy/未知模型 的既有 toolModel 规则（同 id 原位替换），聚焦验收
    // 「指派后 B 段按（指派渠道, 原始模型）可配、row1 保留工具证据」。
    await usageProviderAliasUpsert({
      id: "alias-workbuddy-unknown",
      kind: "toolModel",
      provider: null,
      sourceId: "workbuddy",
      modelId: "",
      displayName: "WB-Direct",
    });
    const restore = override({
      usage_dashboard: () => Promise.resolve(dashboardOf(60, 29)),
      usage_observed_entries_get: () =>
        Promise.resolve([
          observedEntry({
            providerRaw: null,
            sourceId: "workbuddy",
            modelId: "",
            normalizedModelId: "",
            displayProvider: toolModelDisplay("workbuddy", ""),
            totalTokens: 89,
          }),
        ]),
      usage_distribution: (_cmd, args) => {
        const groupBy = (args as { groupBy: DistributionGroup }).groupBy;
        return Promise.resolve({
          groupBy,
          rows: [
            distRow({
              key: groupBy === "model" ? "provider:WB-Direct|workbuddy|" : "provider:WB-Direct",
              sourceIds: ["workbuddy"],
              value: 89,
              channelKey: "provider:WB-Direct",
              channelKind: "alias",
              displayProvider: "WB-Direct",
              modelId: groupBy === "model" ? "" : null,
            }),
          ],
        });
      },
      usage_costs: () =>
        Promise.resolve({
          range: RANGE,
          rows: [
            costOf({
              key: "provider:WB-Direct:unknown",
              sourceId: "workbuddy",
              observedProvider: null,
              modelId: "",
              tokenUsage: 89,
              channelKey: "provider:WB-Direct",
              channelKind: "alias",
              displayProvider: "WB-Direct",
            }),
          ],
          pricing: PRICING_FRESH,
        }),
    });

    render(
      wrap(
        <>
          <UsageSection />
          <UsageDashboardPage />
        </>
      )
    );

    // C：渠道 = 指派名；模型维度 = 显示名 / 未知模型
    expect(await screen.findByTestId("usage-hero")).toHaveTextContent("89");
    await waitFor(() =>
      expect(screen.getByTestId("usage-dist-provider:WB-Direct")).toHaveTextContent("WB-Direct")
    );
    fireEvent.click(screen.getByTestId("usage-dist-group-model"));
    await waitFor(() =>
      expect(screen.getByTestId("usage-dist-provider:WB-Direct|workbuddy|")).toHaveTextContent(
        "WB-Direct / 未知模型"
      )
    );

    // 条目：展示键换为（指派渠道, 未知模型），row1 保留「工具 / 未知模型」原始证据
    fireEvent.click(await screen.findByTestId("usage-config-open-settings"));
    await screen.findByTestId("usage-config-dialog-settings");
    await screen.findByTestId("usage-config-panel-settings");
    await screen.findByTestId("usage-config-model-entry-d-WB-Direct-unknown");
    expect(
      (
        screen.getByTestId(
          "usage-config-model-provider-input-d-WB-Direct-unknown"
        ) as HTMLInputElement
      ).value
    ).toBe("WB-Direct");
    expect(screen.getByTestId("usage-config-model-raw-d-WB-Direct-unknown")).toHaveTextContent(
      "workbuddy"
    );

    // row2 可编辑自定义模型名（空模型也按 sourceId 身份可配）
    fireEvent.change(
      screen.getByTestId("usage-config-model-display-name-input-d-WB-Direct-unknown"),
      { target: { value: "WB 模型" } }
    );
    fireEvent.keyDown(
      screen.getByTestId("usage-config-model-display-name-input-d-WB-Direct-unknown"),
      { key: "Enter" }
    );
    await waitFor(() => expect(callsOf("usage_model_display_name_set")).toHaveLength(1));
    expect(callsOf("usage_model_display_name_set")[0][1]?.request).toMatchObject({
      providerRaw: null,
      sourceId: "workbuddy",
      modelId: "",
      displayName: "WB 模型",
    });

    // row3 计价身份可配（空模型无自动匹配徽标，但选择器在场）
    expect(screen.queryByTestId("usage-config-pricing-badge-d-WB-Direct-unknown")).toBeNull();
    expect(screen.getByTestId("usage-config-pricing-toggle-d-WB-Direct-unknown")).toBeTruthy();

    restore();
  });

  it("T10-3. 计价身份覆盖：手选生效且高于自动匹配、低于高级 PricingRule；清除后回到自动匹配", async () => {
    // 自动匹配 mock 与 quote 同款快照门控：徽标要出「自动匹配」需要先有价目快照
    mockPricingSetEnabled(true);
    mockPricingRefresh();
    let revision = 0;
    let pricingRules: PricingRule[] = [];
    const ruleOf = (active: boolean): PricingRule[] =>
      active
        ? [
            {
              id: "pr-1",
              identity: { sourceId: "codex", observedProvider: "openai", modelId: "gpt-5" },
              match: "exact",
              reference: { providerId: "openai", modelId: "rule-model" },
            },
          ]
        : [];
    /** 观察条目上当前生效的计价身份覆盖（mock 的 identity_set 会真实改这份状态）。 */
    const entryIdentity = () => {
      const entry = mockObservedEntriesGet().find(
        (item) => item.providerRaw === "openai" && item.modelId === "gpt-5"
      );
      return entry && entry.pricingProvider && entry.pricingModel
        ? { providerId: entry.pricingProvider, modelId: entry.pricingModel }
        : null;
    };

    const restore = override({
      usage_dashboard: () => Promise.resolve(dashboardOf(100, 40)),
      // 单成员条目（默认夹具过滤到 openai/gpt-5）：读可变 mock 状态，
      // identity set / clear 的落库结果能通过重取回到徽标
      usage_observed_entries_get: () =>
        Promise.resolve(
          mockObservedEntriesGet().filter(
            (item) => item.providerRaw === "openai" && item.modelId === "gpt-5"
          )
        ),
      usage_billing_get: () => Promise.resolve({ revision, profiles: [], rules: [], pricingRules }),
      usage_billing_commit: (_cmd, args) => {
        const request = (
          args as {
            request: { expectedRevision: number; candidate: BillingConfig };
          }
        ).request;
        if (request.expectedRevision !== revision) {
          return Promise.reject({ code: "usage-settings-invalid", detail: "revision conflict" });
        }
        revision += 1;
        pricingRules = request.candidate.pricingRules;
        return Promise.resolve({ revision, committedAt: revision });
      },
      usage_billing_undo: (_cmd, args) => {
        const request = args as { expectedRevision: number; targetRevision: number };
        if (request.expectedRevision !== revision) {
          return Promise.reject({ code: "usage-settings-invalid", detail: "revision conflict" });
        }
        revision += 1;
        if (request.targetRevision === 0) pricingRules = [];
        return Promise.resolve({ revision, committedAt: revision });
      },
      usage_costs: () => {
        // 优先级链（Rust 契约模拟；真值见 usage_cost_test.rs
        // pricing_identity_override_sits_between_rule_and_auto_match）：
        // 四价覆盖 > PricingRule > 计价身份覆盖 > Curated 自动匹配。
        const ruleActive = pricingRules.length > 0;
        const identity = entryIdentity();
        const amount = ruleActive ? "0.002" : identity ? "0.001" : "0.000529";
        return Promise.resolve({
          range: RANGE,
          rows: [
            costOf({
              key: "chain",
              sourceId: "codex",
              observedProvider: "openai",
              modelId: "gpt-5",
              tokenUsage: 140,
              channelKey: "provider:OpenAI",
              channelKind: "provider",
              displayProvider: "OpenAI",
              estimatedUsd: amount,
            }),
          ],
          pricing: PRICING_FRESH,
        });
      },
    });

    const client = makeClient();
    render(
      wrap(
        <>
          <UsageSection />
          <UsageDashboardPage />
        </>,
        client
      )
    );

    expect(await screen.findByTestId("usage-hero")).toHaveTextContent("140");
    // D 默认展开，金额直接可见
    const amount = () => screen.getByTestId("usage-cost-amount-chain");
    await waitFor(() => expect(amount()).toHaveTextContent("$0.000529"));

    fireEvent.click(await screen.findByTestId("usage-config-open-settings"));
    await screen.findByTestId("usage-config-dialog-settings");
    await screen.findByTestId("usage-config-panel-settings");
    // 默认：精选自动匹配（官方 openai）徽标
    const badge = () => screen.getByTestId("usage-config-pricing-badge-d-OpenAI-gpt-5");
    await waitFor(() =>
      expect(badge()).toHaveTextContent(tf("usage.config.modelSection.pricingAutoMatched"))
    );
    expect(badge()).toHaveTextContent(tf("usage.config.modelSection.sources.official"));

    // 手选：精选内搜不到非精选来源；「显示全部 N 个来源」后可选（= 计价身份覆盖）
    fireEvent.click(screen.getByTestId("usage-config-pricing-toggle-d-OpenAI-gpt-5"));
    fireEvent.change(screen.getByTestId("usage-config-pricing-search-d-OpenAI-gpt-5"), {
      target: { value: "gp" },
    });
    await screen.findByTestId("usage-config-pricing-option-openai/gpt-5");
    expect(screen.queryByTestId("usage-config-pricing-option-aggregator-x/gpt-5")).toBeNull();
    fireEvent.click(screen.getByTestId("usage-config-pricing-scope-d-OpenAI-gpt-5"));
    fireEvent.click(await screen.findByTestId("usage-config-pricing-option-aggregator-x/gpt-5"));
    await waitFor(() => expect(callsOf("usage_pricing_identity_set")).toHaveLength(1));
    expect(callsOf("usage_pricing_identity_set")[0][1]?.request).toMatchObject({
      providerRaw: "openai",
      modelId: "gpt-5",
      reference: { providerId: "aggregator-x", modelId: "gpt-5" },
    });

    // 覆盖生效：徽标换「已覆盖」，D 卡金额按覆盖身份计（高于自动匹配的证据）
    await waitFor(() =>
      expect(badge()).toHaveTextContent(tf("usage.config.modelSection.pricingOverridden"))
    );
    expect(screen.getByTestId("usage-config-pricing-ref-d-OpenAI-gpt-5")).toHaveTextContent(
      "aggregator-x / gpt-5"
    );
    await waitFor(() => expect(amount()).toHaveTextContent("$0.001"));

    // 高级 PricingRule 压过计价身份（金额变；身份配置保留显示；规则入口在高级覆盖折叠区）
    await usageBillingCommit({
      expectedRevision: revision,
      candidate: { revision, profiles: [], rules: [], pricingRules: ruleOf(true) },
    });
    await client.invalidateQueries({ queryKey: USAGE_KEY });
    await waitFor(() => expect(amount()).toHaveTextContent("$0.002"));
    expect(badge()).toHaveTextContent(tf("usage.config.modelSection.pricingOverridden"));
    // 高级覆盖区（受控折叠）标题按钮在场：规则入口在配置卡的高级折叠区
    expect(screen.getByTestId("usage-config-advanced-settings-toggle")).toHaveTextContent(
      tf("usage.config.advanced")
    );

    // 清除计价身份：徽标回自动匹配；规则仍在 → 金额仍由规则决定
    fireEvent.click(screen.getByTestId("usage-config-pricing-clear-d-OpenAI-gpt-5"));
    await waitFor(() => expect(callsOf("usage_pricing_identity_set")).toHaveLength(2));
    expect(callsOf("usage_pricing_identity_set")[1][1]?.request).toMatchObject({ reference: null });
    await waitFor(() =>
      expect(badge()).toHaveTextContent(tf("usage.config.modelSection.pricingAutoMatched"))
    );
    expect(amount()).toHaveTextContent("$0.002");

    // 撤销规则：金额回到 Curated 自动匹配价
    await usageBillingUndo(revision, 0);
    await client.invalidateQueries({ queryKey: USAGE_KEY });
    await waitFor(() => expect(amount()).toHaveTextContent("$0.000529"));

    restore();
  });

  it("T10-4. 7d 费用=小时明细逐日累加（dsh 四桶全价）；删一天明细整段回退日聚合（dsh output-only+原因）；5h 不变", async () => {
    // 后端 usage_detail 状态由 mock 状态位模拟；前端契约 = 档位切换按范围重取 +
    // 回退行 UI 面（partial 金额、覆盖率并入估价列、output-only 分量、原因一行）。
    // 回退真值由 src-tauri/tests/usage_cost_test.rs
    // detail_full_coverage_prices_all_buckets_including_dsh /
    // missing_detail_day_falls_back_to_daily_for_the_whole_range 锁定。
    let detailDeleted = false;
    const DSH_FALLBACK: CostRow = {
      ...DSH_FULL,
      estimatedUsd: "0.000099",
      status: "partial",
      pricedTokens: 30,
      unpricedTokens: 170,
      coverageRate: 0.15,
      reasons: ["untrustedSource", "untrustedInputBuckets"],
      pricingBasis: "baseCurrent",
      components: {
        input: null,
        cacheRead: null,
        cacheWrite: null,
        output: dshComp(30, "3.3", "0.000099"),
      },
    };
    const restore = override({
      usage_costs: (_cmd, args) => {
        const range = (args as { range?: UsageRange }).range ?? RANGE;
        // 5h = 记录级小时明细（Hour 档全源可信，路径不变）；7d/30d = 小时明细按日聚合，
        // 任一天明细缺失 → 整段回退日聚合（dsh output-only + reasons，不逐日混合）。
        const dsh = range.preset === "last5h" || !detailDeleted ? DSH_FULL : DSH_FALLBACK;
        return Promise.resolve({
          range,
          rows: [
            costOf({
              key: "d7-codex",
              sourceId: "codex",
              observedProvider: "openai",
              modelId: "gpt-5",
              tokenUsage: 130,
              channelKey: "provider:OpenAI",
              channelKind: "provider",
              displayProvider: "OpenAI",
            }),
            dsh,
          ],
          pricing: PRICING_FRESH,
        });
      },
    });

    render(wrap(<UsageDashboardPage />));
    // D 默认展开
    const detailRegion = () => screen.getByTestId("usage-cost-details-d7-dsh");
    const gridOf = () => within(detailRegion()).getByTestId("usage-cost-table-detail-grid");
    /** 范围切换后明细区会随加载态重挂（展开态重置）：收起则重新展开。 */
    const expandDsh = async () => {
      await screen.findByTestId("usage-cost-name-d7-dsh");
      const toggle = screen.getByTestId("usage-cost-table-detail-toggle-d7-dsh");
      if (toggle.getAttribute("aria-expanded") !== "true") {
        fireEvent.click(toggle);
      }
      await waitFor(() => expect(detailRegion()).not.toHaveAttribute("hidden"));
      return gridOf();
    };

    // (a) 7d · 明细覆盖：四桶全价、合计 = 分项和、无原因行
    const grid = await expandDsh();
    const rowText = (key: string) =>
      within(grid).getByTestId(`usage-cost-table-detail-row-${key}`).textContent ?? "";
    expect(rowText("input")).toContain("120");
    expect(rowText("input")).toContain("1.10 USD / 1M tokens");
    expect(rowText("cacheRead")).toContain("40");
    expect(rowText("cacheWrite")).toContain("10");
    expect(rowText("output")).toContain("30");
    expect(rowText("total")).toContain("200");
    expect(rowText("total")).toContain(sumUsdOf(["0.000132", "0.0000044", "0.000022", "0.000099"]));
    expect(
      detailRegion().querySelector('[data-testid="usage-cost-table-reasons-d7-dsh"]')
    ).toBeNull();

    // (b) 5h：记录级明细路径不变（dsh 仍四桶全价、无原因）
    fireEvent.click(screen.getByTestId("usage-range-last5h"));
    await waitFor(() =>
      expect(
        callsOf("usage_costs").some((call) => (call[1]?.range as UsageRange)?.preset === "last5h")
      ).toBe(true)
    );
    const hourGrid = await expandDsh();
    expect(within(hourGrid).getByTestId("usage-cost-table-detail-row-input").textContent).toContain(
      "120"
    );
    expect(hourGrid.querySelector('[data-testid="usage-cost-table-reasons-d7-dsh"]')).toBeNull();

    // (c) 删一天明细 → 30d 整段回退日聚合：dsh output-only + 原因一行（`·` 分割、非列表）
    detailDeleted = true;
    fireEvent.click(screen.getByTestId("usage-range-last30d"));
    await waitFor(() =>
      expect(screen.getByTestId("usage-cost-amount-d7-dsh")).toHaveTextContent("已估价部分")
    );
    expect(screen.getByTestId("usage-cost-amount-d7-dsh")).toHaveTextContent(
      formatMoney("0.000099", "usd", "7.2") as string
    );
    expect(screen.getByTestId("usage-cost-coverage-d7-dsh")).toHaveTextContent("15.0%");
    const fallbackGrid = await expandDsh();
    // input/cacheRead 无数据 → 三列「—」（不是 0）；cacheWrite 无数据 → 整行不出现
    for (const key of ["input", "cacheRead"]) {
      const row = within(fallbackGrid).getByTestId(`usage-cost-table-detail-row-${key}`);
      expect(row.textContent).toContain("—");
      expect(row.textContent).not.toContain("$0.00");
    }
    expect(
      fallbackGrid.querySelector('[data-testid="usage-cost-table-detail-row-cacheWrite"]')
    ).toBeNull();
    expect(
      within(fallbackGrid).getByTestId("usage-cost-table-detail-row-output").textContent
    ).toContain(formatMoney("0.000099", "usd", "7.2") as string);
    const reasons = within(detailRegion()).getByTestId("usage-cost-table-reasons-d7-dsh");
    expect(reasons.textContent).toContain(
      `${tf("usage.cost.reason.untrustedSource")} · ${tf("usage.cost.reason.untrustedInputBuckets")}`
    );
    expect(reasons.querySelector("li")).toBeNull();

    restore();
  });

  it("T10-5. D 卡单表：五列表头；卡片内四列明细；合计行=分项和且单价空；缓存写无数据不出现；小字只留归一化模型+快照时间", async () => {
    render(wrap(<UsageDashboardPage />));
    await screen.findByTestId("usage-hero");
    // D 默认展开
    await screen.findByTestId("usage-cost-body");

    // 主表五列（顺序即口径）；默认按渠道分组、无全局合计/渠道小计
    const header = screen.getByTestId("usage-cost-table-header");
    expect(within(header).getByTestId("usage-cost-table-col-name")).toHaveTextContent(
      "渠道 / 模型"
    );
    expect(within(header).getByTestId("usage-cost-table-col-amount")).toHaveTextContent("估价");
    expect(within(header).getByTestId("usage-cost-table-col-token")).toHaveTextContent("总 Token");
    expect(within(header).getByTestId("usage-cost-table-col-hit")).toHaveTextContent("缓存命中率");
    expect(within(header).getByTestId("usage-cost-table-col-detail")).toHaveTextContent("计费详情");
    expect(screen.getByTestId("usage-cost-groups")).toBeTruthy();
    expect(screen.queryByTestId(/usage-cost-(total|subtotal)/)).toBeNull();

    // 已估价行：四行（含缓存写）三列；合计 = 分项 Token/金额之和、单价留空
    const pricedKey = "model:bp:profile-a:gpt-5";
    fireEvent.click(screen.getByTestId(`usage-cost-table-detail-toggle-${pricedKey}`));
    const pricedGrid = within(screen.getByTestId(`usage-cost-details-${pricedKey}`)).getByTestId(
      "usage-cost-table-detail-grid"
    );
    const rowText = (key: string) =>
      within(pricedGrid).getByTestId(`usage-cost-table-detail-row-${key}`).textContent ?? "";
    expect(rowText("input")).toContain("新增输入");
    expect(rowText("input")).toContain("100");
    expect(rowText("input")).toContain("$0.0002");
    expect(rowText("input")).toContain("2.00 USD / 1M tokens");
    expect(rowText("cacheRead")).toContain("20");
    expect(rowText("cacheWrite")).toContain("缓存写");
    expect(rowText("cacheWrite")).toContain("10");
    expect(rowText("output")).toContain("30");
    const totalText = rowText("total");
    expect(totalText).toContain("合计");
    expect(totalText).toContain("160");
    expect(totalText).toContain(sumUsdOf(["0.0002", "0.000004", "0.000025", "0.0003"]));
    // 合计行单价列留空（—），不出现任何单价
    expect(totalText).not.toContain("/ 1M tokens");

    // 部分估价行（无缓存写数据）：缓存写行不出现；原因一行（非列表）
    const partialKey = "model:provider:OpenRouter:claude-sonnet-4-5";
    fireEvent.click(screen.getByTestId(`usage-cost-table-detail-toggle-${partialKey}`));
    const partialDetails = within(screen.getByTestId(`usage-cost-details-${partialKey}`));
    expect(
      partialDetails
        .getByTestId("usage-cost-table-detail-grid")
        .querySelector('[data-testid="usage-cost-table-detail-row-cacheWrite"]')
    ).toBeNull();
    const reasons = screen.getByTestId(`usage-cost-table-reasons-${partialKey}`);
    expect(reasons.textContent).toContain("不准确原因");
    expect(reasons.textContent).toContain("部分计费桶不可估价");
    expect(reasons.querySelector("li")).toBeNull();

    // 小字：只含归一化模型 + 快照获取时间（价格来源/模型发布日已删）
    const detailsText = screen.getByTestId(`usage-cost-details-${pricedKey}`).textContent ?? "";
    expect(screen.getByTestId(`usage-cost-table-footnote-${pricedKey}`).textContent).toContain(
      "归一化模型"
    );
    expect(screen.getByTestId(`usage-cost-table-footnote-${pricedKey}`).textContent).toContain(
      "gpt-5"
    );
    expect(screen.getByTestId(`usage-cost-table-footnote-${pricedKey}`).textContent).toContain(
      "快照获取时间"
    );
    expect(detailsText).not.toContain("价格来源");
    expect(detailsText).not.toContain("模型发布日");

    // 缺价行：显示「—（未估价）」，绝不显示 $0.00
    const unpricedAmount = screen.getByTestId("usage-cost-amount-unknown:unassigned:gpt-unpriced");
    expect(unpricedAmount).toHaveTextContent("—（未估价）");
    expect(unpricedAmount.textContent).not.toContain("$0.00");
  });

  it("T10-6. 表头币种切换联动金额/单价/分项；导出 CSV 仍 USD、列集不变", async () => {
    render(wrap(<UsageDashboardPage />));
    await screen.findByTestId("usage-hero");
    // D 默认展开
    await screen.findByTestId("usage-cost-body");

    const pricedKey = "model:bp:profile-a:gpt-5";
    const amount = () => screen.getByTestId(`usage-cost-amount-${pricedKey}`);
    expect(amount()).toHaveTextContent("$0.000529");
    fireEvent.click(screen.getByTestId(`usage-cost-table-detail-toggle-${pricedKey}`));
    expect(screen.getByTestId(`usage-cost-details-${pricedKey}`)).toHaveTextContent(
      "2.00 USD / 1M tokens"
    );

    // USD → CNY：金额、分项金额、单价全部按显示币种换算并标注 CNY
    fireEvent.click(screen.getByTestId("usage-cost-table-currency-cny"));
    await waitFor(() => expect(amount()).toHaveTextContent("¥0.003809"));
    const details = screen.getByTestId(`usage-cost-details-${pricedKey}`);
    expect(details).toHaveTextContent("14.40 CNY / 1M tokens");
    expect(details).toHaveTextContent("72.00 CNY / 1M tokens");
    expect(details).toHaveTextContent("¥0.00144"); // 分项 0.0002 USD × 7.2
    expect(details).not.toHaveTextContent("2.00 USD / 1M tokens");
    expect(screen.getByTestId("usage-cost-currency")).toHaveTextContent("CNY");

    // 导出 CSV：仍 USD 原值、列集不变（显示币种只作用于展示层）
    expect(screen.getByTestId("usage-export-cost-csv")).toHaveAttribute(
      "title",
      tf("usage.cost.exportCsvUnit")
    );
    fireEvent.click(screen.getByTestId("usage-export-cost-csv"));
    await waitFor(() => expect(callsOf("usage_cost_export_csv")).toHaveLength(1));
    expect(callsOf("usage_cost_export_csv")[0][1]).toEqual({ range: RANGE });
    const saved = await waitFor(() => {
      const calls = callsOf("export_save_text");
      expect(calls.length).toBeGreaterThan(0);
      return calls[0][1] as { name: string; content: string };
    });
    expect(saved.content).toContain('"0.000529"');
    expect(saved.content).not.toContain("¥");
    expect(saved.content).not.toContain("3.809");
    // 23 列 + 2026-10-21 追加的 price_rule_kind
    expect(saved.content.split("\n")[0].split(",")).toHaveLength(24);

    // 切回 USD：展示层恢复
    fireEvent.click(screen.getByTestId("usage-cost-table-currency-usd"));
    await waitFor(() => expect(amount()).toHaveTextContent("$0.000529"));
  });

  it("T10-7. 无全局金额/渠道小计/费用趋势；A 卡无缓存写入块；计量维度文案统一", async () => {
    render(wrap(<UsageDashboardPage />));
    await screen.findByTestId("usage-hero");

    // A 卡：缓存写入（解释）整块不存在；七行计量网格 = 统一文案（新增输入/缓存命中/输出）
    expect(screen.queryByTestId("usage-cache-write")).toBeNull();
    expect(screen.getAllByTestId(/^usage-grid-/).map((el) => el.dataset.testid)).toEqual([
      "usage-grid-requestInput",
      "usage-grid-cacheRead",
      "usage-grid-cacheMiss",
      "usage-grid-output",
      "usage-grid-hitRate",
      "usage-grid-requests",
      "usage-grid-asOf",
    ]);
    expect(screen.getByTestId("usage-grid-requestInput")).toHaveTextContent("新增输入");
    expect(screen.getByTestId("usage-grid-cacheRead")).toHaveTextContent("缓存命中");
    expect(screen.getByTestId("usage-grid-output")).toHaveTextContent("输出");

    // D 卡：无全局金额、无渠道小计、无费用趋势；口径脚注明确声明（D 默认展开）
    await screen.findByTestId("usage-cost-body");
    expect(screen.queryByTestId(/usage-cost-(total|subtotal)/)).toBeNull();
    const fold = screen.getByTestId("usage-fold-cost");
    expect(fold.textContent).not.toContain("$总计");
    expect(fold.querySelector('[data-testid="usage-trend-svg"]')).toBeNull();
    expect(fold.querySelector('[data-testid="usage-cost-trend"]')).toBeNull();
    expect(screen.queryByTestId(/usage-cost-trend/)).toBeNull();
    expect(within(fold).getByText(tf("usage.cost.tokenBasis"))).toBeTruthy();

    // D 明细行与 A 卡共用计量术语：新增输入 / 输出（断言限定在目标行的详情区，
    // detail-row-* testid 在每行详情里都会出现）
    const pricedKey = "model:bp:profile-a:gpt-5";
    fireEvent.click(screen.getByTestId(`usage-cost-table-detail-toggle-${pricedKey}`));
    const pricedDetails = within(screen.getByTestId(`usage-cost-details-${pricedKey}`));
    expect(pricedDetails.getByTestId("usage-cost-table-detail-row-input")).toHaveTextContent(
      "新增输入"
    );
    expect(pricedDetails.getByTestId("usage-cost-table-detail-row-output")).toHaveTextContent(
      "输出"
    );
  });
});

// ── 双语逐键核对（不只比较键集）──────────────────────────────────────────
type Dict = Record<string, unknown>;

function flat(obj: unknown, prefix = ""): Record<string, string> {
  const out: Record<string, string> = {};
  for (const [key, value] of Object.entries(obj as Dict)) {
    const path = prefix ? `${prefix}.${key}` : key;
    if (typeof value === "object" && value !== null) {
      Object.assign(out, flat(value, path));
    } else {
      out[path] = String(value);
    }
  }
  return out;
}

const zh = flat(zhLocale);
const en = flat(enLocale);
const CJK = /[一-鿿]/;

describe("V2 双语逐键核对", () => {
  it("4. Task 19 指定的 cost 文案逐字对齐", () => {
    const expected: Record<string, [string, string]> = {
      "usage.cost.title": ["API 等效费用", "API equivalent cost"],
      "usage.cost.disclaimer": [
        "按模型参考 API 价估算，非实际支出",
        "Estimated from reference API prices, not actual spend",
      ],
      "usage.cost.unpriced": ["—（未估价）", "— (unpriced)"],
      "usage.cost.partialAmount": ["已估价部分 {{amount}}", "Priced portion {{amount}}"],
      "usage.cost.coverage": ["估价覆盖率", "Pricing coverage"],
      "usage.cost.baseCurrent": ["当前基准价回算", "Recalculated at current base prices"],
      "usage.cost.exportCsv": ["费用 CSV", "Cost CSV"],
    };
    for (const [key, [zhValue, enValue]] of Object.entries(expected)) {
      expect(zh[key], key).toBe(zhValue);
      expect(en[key], key).toBe(enValue);
    }
    // stale 允许带时间参数，但必须包含 Task 19 的语义
    expect(zh["usage.cost.stale"]).toContain("价格快照已过期，正在使用最近有效版本");
    expect(en["usage.cost.stale"]).toContain(
      "Pricing snapshot is stale; using the last valid version"
    );
  });

  it("5. 新增按钮/失败原因/折叠/筛选/记录页标记逐键有英文且无中文", () => {
    // 2026-10-20 Task 5：usage.grid.cacheWrite / usage.cacheWrite.* 已随「缓存写入（解释）」
    // 整块删除（zh/en 同步删），键清单同步移除。
    const keys = [
      "usage.grid.cacheMiss",
      "usage.label.unassignedChannel",
      "usage.label.ambiguousChannel",
      "usage.trend.mode",
      "usage.trend.all",
      "usage.trend.byTool",
      "usage.trend.byChannel",
      "usage.trend.target",
      "usage.trend.unknownChannel",
      "usage.trend.unavailable",
      "usage.trend.filterUnavailable",
      "usage.trend.reset",
      "usage.dist.more",
      "usage.dist.less",
      "usage.work.foldSummary",
      "usage.work.advanced",
      "usage.work.toolDuration",
      "usage.records.assignChannel",
      "usage.records.editAssignment",
      "usage.records.viewRules",
      "usage.records.billingRulesAdvanced",
      "usage.records.assigned",
      "usage.records.observed",
      "usage.records.ambiguous",
      "usage.billing.title",
      "usage.billing.candidates",
      "usage.billing.preview",
      "usage.billing.save",
      "usage.billing.nameRequired",
      "usage.billing.previewRequired",
      "usage.billing.manage",
      "usage.billing.pricingRules",
      "usage.billing.pricingAuthorize",
      "usage.billing.pricingSave",
      "usage.billing.pricingNeedEnable",
      "usage.pricing.enable",
      "usage.pricing.refresh",
      "usage.pricing.refreshing",
      "usage.pricing.privacy",
      "usage.pricing.lastError",
      "usage.pricing.lastValid",
      "usage.cost.title",
      "usage.cost.unpriced",
      "usage.cost.partialAmount",
      "usage.cost.details",
      "usage.cost.componentUnpriced",
      "usage.cost.reason.noPrice",
      "usage.cost.reason.unpricedComponent",
      "usage.cost.more",
      "usage.cost.less",
      "usage.cost.stale",
      "usage.cost.noSnapshot",
      "usage.cost.refreshing",
      "usage.cost.exportCsv",
      "usage.export.revealFailed",
      // 2026-10-19 修订新增键（别名浮窗预览 / 费用证据 / 覆盖价）
      "usage.label.unknownModel",
      "usage.records.aliasAction",
      "usage.cost.pricingSource",
      "usage.cost.pricingReleaseDate",
      "usage.cost.normalizedModel",
      "usage.cost.priceOverride",
      "usage.cost.userOverride",
      "usage.cost.reason.pricingPrefixAmbiguous",
      // 2026-10-19 最终复核补键：趋势空态/失效渠道与费用未标注来源
      "usage.trend.invalidChannel",
      "usage.trend.noChannels",
      "usage.cost.unlabeledSource",
      // 整组扫描：别名与覆盖价的所有键都必须双语存在且英文无 CJK
      ...Object.keys(zh).filter(
        (key) => key.startsWith("usage.alias.") || key.startsWith("usage.pricing.overrides.")
      ),
    ];
    for (const key of keys) {
      expect(en[key], `missing en: ${key}`).toBeTruthy();
      expect(zh[key], `missing zh: ${key}`).toBeTruthy();
      expect(en[key], `CJK in en: ${key}`).not.toMatch(CJK);
    }

    // 2026-10-20 计费入口收敛：行内按钮 / 页脚高级入口 / 管理器标题逐字锁定（zh/en 同步）
    const copy: Record<string, [string, string]> = {
      "usage.records.assignChannel": ["绑定计费归属", "Bind billing attribution"],
      "usage.records.aliasAction": ["供应商显示名", "Provider display name"],
      "usage.records.billingRulesAdvanced": [
        "高级：计费归属与参考价规则",
        "Advanced: billing attribution & reference rules",
      ],
      "usage.billing.manage": ["计费归属与参考价规则", "Billing attribution & reference rules"],
      "usage.billing.manageTitle": [
        "计费归属与参考价规则",
        "Billing attribution & reference rules",
      ],
    };
    for (const [key, [zhValue, enValue]] of Object.entries(copy)) {
      expect(zh[key], key).toBe(zhValue);
      expect(en[key], key).toBe(enValue);
    }

    // 新增分组键集在 zh/en 完全一致
    for (const group of [
      "usage.cost",
      "usage.pricing",
      "usage.billing",
      "usage.trend",
      "usage.dist",
      "usage.alias",
    ]) {
      const zhKeys = Object.keys(zh)
        .filter((key) => key.startsWith(`${group}.`))
        .sort();
      const enKeys = Object.keys(en)
        .filter((key) => key.startsWith(`${group}.`))
        .sort();
      expect(enKeys, group).toEqual(zhKeys);
    }

    // 渠道维度术语：中文「按渠道」/ 英文「By channel」（不得回退到「按计费渠道」）
    expect(zh["usage.group.billingProfile"]).toBe("按渠道");
    expect(en["usage.group.billingProfile"]).toBe("By channel");
  });

  it("6. 计量维度文案统一：新增输入/输出在 A 卡网格与 D 明细分量同值（zh/en 同步）；缓存写入解释键已删", () => {
    // 2026-10-20 计划 Task 5 值统一（冻结口径 §0.7）：A 卡网格与 D 明细共用计量术语
    expect(zh["usage.grid.requestInput"]).toBe("新增输入");
    expect(zh["usage.cost.component.input"]).toBe("新增输入");
    expect(zh["usage.grid.output"]).toBe("输出");
    expect(zh["usage.cost.component.output"]).toBe("输出");
    expect(en["usage.grid.requestInput"]).toBe("New input");
    expect(en["usage.cost.component.input"]).toBe("New input");
    expect(en["usage.grid.output"]).toBe("Output");
    expect(en["usage.cost.component.output"]).toBe("Output");
    // hero 口径说明跟随新措辞
    expect(zh["usage.hero.sub"]).toBe("新增输入 + 输出（不含缓存写入与用户输入估算）");
    expect(en["usage.hero.sub"]).toBe(
      "New input + output (excludes cache writes and user-input estimates)"
    );
    // A 卡缓存命中行的全局术语（D 明细行键名沿用 usage.cost.component.cacheRead）
    expect(zh["usage.grid.cacheRead"]).toBe("缓存命中");
    expect(en["usage.grid.cacheRead"]).toBe("Cache hit");
    // 「缓存写入（解释）」整块删除 → 键 zh/en 同步删，不得单侧残留
    expect(zh["usage.grid.cacheWrite"]).toBeUndefined();
    expect(en["usage.grid.cacheWrite"]).toBeUndefined();
    expect(zh["usage.cacheWrite"]).toBeUndefined();
    expect(en["usage.cacheWrite"]).toBeUndefined();
  });
});
