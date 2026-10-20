// Task 5 · V2 查询 hooks（src/lib/query/queries/usageV2.ts）的行为锁：
//  * 四类查询键形状；
//  * 不同 range / selection / groupBy 产生不同键；
//  * 读请求绝不触发 usage_collect；
//  * commit 后 invalidate USAGE_KEY 能让 billing 查询重取。
import { act, renderHook, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ReactNode } from "react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { listen } from "@tauri-apps/api/event";
import { tauriInvokeMock } from "../msw/tauriMocks";
import {
  useUsageBillingQuery,
  useUsageChannelOptionsQuery,
  useUsageModelDisplayNameSet,
  useUsageObservedEntriesQuery,
  useUsageCostsQuery,
  useUsageDistributionQuery,
  useUsageIdentitiesQuery,
  useUsagePriceOverridesQuery,
  useUsagePricingEvents,
  useUsagePricingAutoMatchQuery,
  useUsagePricingCatalogQuery,
  useUsagePricingCatalogSearchQuery,
  useUsagePricingIdentitySet,
  useUsagePricingQuery,
  useUsagePricingQuoteQuery,
  useUsageProviderAliasesQuery,
  useUsageTrendQuery,
} from "@/lib/query/queries/usageV2";
import { USAGE_KEY } from "@/lib/query/queries/usage";
import {
  usageBillingCommit,
  usagePricingOverrideSet,
  usagePricingRefresh,
  usagePricingSetEnabled,
  usageProviderAliasDelete,
  usageProviderAliasUpsert,
} from "@/lib/api/usageV2";
import { mockPricingStatus, mockUsageCosts, resetUsageV2MockState } from "@/lib/usage/mockFixtures";
import type { UsageFilters, UsageRange } from "@/types/usage";
import type { ProviderAliasRule, TrendSelection } from "@/types/usageV2";

const TODAY: UsageRange = { preset: "today" };
const LAST7D: UsageRange = { preset: "last7d" };
const ALL: TrendSelection = { scope: "all" };
const TOOL: TrendSelection = { scope: "tool", selectedId: "codex" };

function createWrapper(
  client = new QueryClient({ defaultOptions: { queries: { retry: false } } })
) {
  return {
    client,
    wrapper: ({ children }: { children: ReactNode }) => (
      <QueryClientProvider client={client}>{children}</QueryClientProvider>
    ),
  };
}

function invokedCommands(): string[] {
  return tauriInvokeMock.mock.calls.map(([cmd]) => cmd);
}

beforeEach(() => {
  tauriInvokeMock.mockClear();
  resetUsageV2MockState();
});

afterEach(() => {
  vi.mocked(listen).mockReset();
  vi.mocked(listen).mockResolvedValue(() => {});
});

describe("V2 查询 hooks（src/lib/query/queries/usageV2.ts）", () => {
  it("1. 四类查询键形状固定，读请求不含 usage_collect", async () => {
    const { client, wrapper } = createWrapper();
    const filters: UsageFilters = { toolIds: ["codex"] };
    const { result } = renderHook(
      () => ({
        billing: useUsageBillingQuery(),
        identities: useUsageIdentitiesQuery(LAST7D, filters),
        trend: useUsageTrendQuery(LAST7D, TOOL),
        distribution: useUsageDistributionQuery(LAST7D, "model"),
      }),
      { wrapper }
    );

    await waitFor(() => expect(result.current.billing.isSuccess).toBe(true));
    await waitFor(() => expect(result.current.identities.isSuccess).toBe(true));
    await waitFor(() => expect(result.current.trend.isSuccess).toBe(true));
    await waitFor(() => expect(result.current.distribution.isSuccess).toBe(true));

    const keys = client
      .getQueryCache()
      .getAll()
      .map((query) => query.queryKey)
      .sort((a, b) => JSON.stringify(a).localeCompare(JSON.stringify(b)));
    expect(keys).toEqual(
      [
        ["usage", "v2", "billing"],
        ["usage", "v2", "distribution", LAST7D, "model"],
        ["usage", "v2", "identities", LAST7D, filters],
        ["usage", "v2", "trend", LAST7D, TOOL],
      ].sort((a, b) => JSON.stringify(a).localeCompare(JSON.stringify(b)))
    );
    expect(invokedCommands()).not.toContain("usage_collect");
  });

  it("2. 不同 range / selection / groupBy 产生不同 queryKey", async () => {
    const { client, wrapper } = createWrapper();
    const { result } = renderHook(
      () => ({
        trendToday: useUsageTrendQuery(TODAY, ALL),
        trendAll: useUsageTrendQuery(LAST7D, ALL),
        trendTool: useUsageTrendQuery(LAST7D, TOOL),
        distTool: useUsageDistributionQuery(LAST7D, "tool"),
        distModel: useUsageDistributionQuery(LAST7D, "model"),
      }),
      { wrapper }
    );
    await waitFor(() => expect(result.current.distModel.isSuccess).toBe(true));
    await waitFor(() => expect(result.current.trendTool.isSuccess).toBe(true));

    const keys = client
      .getQueryCache()
      .getAll()
      .map((query) => JSON.stringify(query.queryKey));
    for (const key of [
      ["usage", "v2", "trend", TODAY, ALL],
      ["usage", "v2", "trend", LAST7D, ALL],
      ["usage", "v2", "trend", LAST7D, TOOL],
      ["usage", "v2", "distribution", LAST7D, "tool"],
      ["usage", "v2", "distribution", LAST7D, "model"],
    ]) {
      expect(keys).toContain(JSON.stringify(key));
    }
    expect(new Set(keys).size).toBe(keys.length);
  });

  it("3. commit 后 invalidate USAGE_KEY，billing 查询重取并看到新 revision", async () => {
    const { client, wrapper } = createWrapper();
    const { result } = renderHook(() => useUsageBillingQuery(), { wrapper });
    await waitFor(() => expect(result.current.isSuccess).toBe(true));
    expect(result.current.data!.revision).toBe(0);

    await usageBillingCommit({
      expectedRevision: result.current.data!.revision,
      candidate: result.current.data!,
    });
    await client.invalidateQueries({ queryKey: USAGE_KEY });
    await waitFor(() => expect(result.current.data!.revision).toBe(1));

    const gets = invokedCommands().filter((cmd) => cmd === "usage_billing_get");
    expect(gets).toHaveLength(2);
    expect(invokedCommands()).not.toContain("usage_collect");
  });

  it("4. costs / pricing 查询键固定（fallbackRate 进键），读请求不含 usage_collect", async () => {
    const { client, wrapper } = createWrapper();
    const { result } = renderHook(
      () => ({
        costs: useUsageCostsQuery(LAST7D, "7.2"),
        pricing: useUsagePricingQuery(),
      }),
      { wrapper }
    );
    await waitFor(() => expect(result.current.costs.isSuccess).toBe(true));
    await waitFor(() => expect(result.current.pricing.isSuccess).toBe(true));

    const keys = client
      .getQueryCache()
      .getAll()
      .map((query) => query.queryKey);
    expect(keys).toContainEqual(["usage", "v2", "costs", LAST7D, "7.2"]);
    expect(keys).toContainEqual(["usage", "v2", "pricing"]);
    expect(invokedCommands()).not.toContain("usage_collect");
    // 备用汇率随请求下发（后端在无快照汇率时用它换算 CNY）
    const costsCall = invokedCommands().indexOf("usage_costs");
    expect(
      (tauriInvokeMock.mock.calls[costsCall]?.[1] as { fallbackRate?: string }).fallbackRate
    ).toBe("7.2");
    expect(result.current.costs.data!.rows.length).toBeGreaterThan(0);
    // 新字段在场：双币金额 + 汇率三元组（夹具回显，形状由 v2-api 锁）
    expect(result.current.costs.data!.fxRate).toBe("7.2");
    expect(typeof result.current.costs.data!.fxFallback).toBe("boolean");
  });

  it("5. pricing 事件触发后同时失效 pricing/costs，卸载幂等解除监听", async () => {
    let handler: (() => void) | undefined;
    const unlisten = vi.fn();
    vi.mocked(listen).mockImplementation(async (_event, callback) => {
      handler = callback as () => void;
      return unlisten;
    });

    const { client, wrapper } = createWrapper();
    client.setQueryData(["usage", "v2", "pricing"], mockPricingStatus());
    client.setQueryData(["usage", "v2", "costs", LAST7D], mockUsageCosts(LAST7D));

    const { unmount } = renderHook(() => useUsagePricingEvents(), { wrapper });
    await waitFor(() => expect(handler).toBeDefined());

    await act(async () => {
      handler!();
    });
    await waitFor(() => {
      expect(client.getQueryState(["usage", "v2", "pricing"])?.isInvalidated).toBe(true);
      expect(client.getQueryState(["usage", "v2", "costs", LAST7D])?.isInvalidated).toBe(true);
    });

    unmount();
    unmount();
    expect(unlisten).toHaveBeenCalledTimes(1);
  });

  it("6. 别名/渠道选项/覆盖价查询键固定，不同 range 不同键，读请求不含 usage_collect", async () => {
    const { client, wrapper } = createWrapper();
    const { result } = renderHook(
      () => ({
        aliases: useUsageProviderAliasesQuery(),
        channelsToday: useUsageChannelOptionsQuery(TODAY),
        channelsWeek: useUsageChannelOptionsQuery(LAST7D),
        overrides: useUsagePriceOverridesQuery(),
      }),
      { wrapper }
    );

    await waitFor(() => expect(result.current.aliases.isSuccess).toBe(true));
    await waitFor(() => expect(result.current.channelsToday.isSuccess).toBe(true));
    await waitFor(() => expect(result.current.channelsWeek.isSuccess).toBe(true));
    await waitFor(() => expect(result.current.overrides.isSuccess).toBe(true));

    const keys = client
      .getQueryCache()
      .getAll()
      .map((query) => query.queryKey);
    expect(keys).toContainEqual(["usage", "v2", "providerAliases"]);
    expect(keys).toContainEqual(["usage", "v2", "channelOptions", TODAY]);
    expect(keys).toContainEqual(["usage", "v2", "channelOptions", LAST7D]);
    expect(keys).toContainEqual(["usage", "v2", "priceOverrides"]);
    expect(JSON.stringify(keys.filter((key) => key[2] === "channelOptions")[0])).not.toBe(
      JSON.stringify(keys.filter((key) => key[2] === "channelOptions")[1])
    );

    expect(invokedCommands()).not.toContain("usage_collect");
    expect(
      client
        .getQueryCache()
        .getAll()
        .every((query) => query.options.refetchInterval === false)
    ).toBe(true);
  });

  it("7. 别名保存/删除与覆盖价保存后 invalidate USAGE_KEY 会重取，且不触发采集", async () => {
    const { client, wrapper } = createWrapper();
    const { result } = renderHook(
      () => ({
        aliases: useUsageProviderAliasesQuery(),
        overrides: useUsagePriceOverridesQuery(),
      }),
      { wrapper }
    );
    await waitFor(() => expect(result.current.aliases.isSuccess).toBe(true));
    await waitFor(() => expect(result.current.overrides.isSuccess).toBe(true));
    expect(result.current.aliases.data!.aliases).toHaveLength(2);
    // 先读 data：React Query 的默认 notifyOnChangeProps 只跟踪**实际读过**的属性；
    // 不在更新前读 data 的话，测试观察不到重取（真实组件在渲染时就会读 data）。
    expect(result.current.overrides.data![0].input).toBe("1.25");

    const rule: ProviderAliasRule = {
      id: "alias-openai",
      kind: "provider",
      provider: "openai",
      sourceId: null,
      modelId: null,
      displayName: "OpenAI",
    };
    await usageProviderAliasUpsert(rule);
    await client.invalidateQueries({ queryKey: USAGE_KEY });
    await waitFor(() => expect(result.current.aliases.data!.aliases).toHaveLength(3));

    await usageProviderAliasDelete(rule.id);
    await client.invalidateQueries({ queryKey: USAGE_KEY });
    await waitFor(() => expect(result.current.aliases.data!.aliases).toHaveLength(2));

    await act(async () => {
      await usagePricingOverrideSet({
        normalizedModelId: "gpt-5",
        input: "3.75",
        cacheRead: null,
        cacheWrite: null,
        output: null,
      });
      await client.invalidateQueries({ queryKey: USAGE_KEY });
    });
    await waitFor(() =>
      expect(
        result.current.overrides.data!.find((view) => view.normalizedModelId === "gpt-5")?.input
      ).toBe("3.75")
    );

    expect(invokedCommands().filter((cmd) => cmd === "usage_provider_aliases_get").length).toBe(4);
    expect(invokedCommands().filter((cmd) => cmd === "usage_pricing_overrides_get").length).toBe(4);
    expect(invokedCommands()).not.toContain("usage_collect");
  });

  it("8. 目录搜索：query 进键、trim 后 <2 字符不启用、不轮询", async () => {
    await usagePricingSetEnabled(true);
    await usagePricingRefresh();
    const { client, wrapper } = createWrapper();

    const { result, rerender } = renderHook(
      ({ query }: { query: string }) => useUsagePricingCatalogQuery(query),
      { wrapper, initialProps: { query: "de" } }
    );
    await waitFor(() => expect(result.current.isSuccess).toBe(true));
    expect(result.current.data).toHaveLength(1);
    expect(result.current.data![0]).toMatchObject({
      providerId: "ollama-cloud",
      modelId: "deepseek-v4.1-flash",
    });
    expect(
      client
        .getQueryCache()
        .getAll()
        .map((query) => query.queryKey)
    ).toContainEqual(["usage", "v2", "pricingCatalog", "de"]);
    expect(
      client
        .getQueryCache()
        .getAll()
        .every((query) => query.options.refetchInterval === false)
    ).toBe(true);
    // hook 固定最多 20 条
    const firstCall = tauriInvokeMock.mock.calls.filter(
      ([cmd]) => cmd === "usage_pricing_catalog"
    )[0];
    expect(firstCall[1]).toEqual({ query: "de", limit: 20 });

    // trim 后不足 2 字符：不发请求（key 也不出现）
    const before = tauriInvokeMock.mock.calls.filter(
      ([cmd]) => cmd === "usage_pricing_catalog"
    ).length;
    rerender({ query: " d " });
    await new Promise((resolve) => setTimeout(resolve, 20));
    const after = tauriInvokeMock.mock.calls.filter(
      ([cmd]) => cmd === "usage_pricing_catalog"
    ).length;
    expect(after).toBe(before);
    expect(invokedCommands()).not.toContain("usage_collect");
  });

  it("6. 观察条目/精选目录/自动匹配/价格预览查询键与 enabled 门控", async () => {
    const { client, wrapper } = createWrapper();
    const { result } = renderHook(
      () => ({
        observed: useUsageObservedEntriesQuery(),
        search: useUsagePricingCatalogSearchQuery("  GPT  ", "curated"),
        autoMatch: useUsagePricingAutoMatchQuery("gpt-5", "7.2"),
        quote: useUsagePricingQuoteQuery("openai", "gpt-5", "7.2"),
      }),
      { wrapper }
    );

    await waitFor(() => expect(result.current.observed.isSuccess).toBe(true));
    await waitFor(() => expect(result.current.search.isSuccess).toBe(true));
    await waitFor(() => expect(result.current.autoMatch.isSuccess).toBe(true));
    await waitFor(() => expect(result.current.quote.isSuccess).toBe(true));

    const keys = client
      .getQueryCache()
      .getAll()
      .map((query) => query.queryKey);
    expect(keys).toContainEqual(["usage", "v2", "observedEntries"]);
    // 键里的 query 必须是 trim 后的；scope 进键（切「显示全部」即换查询）。
    expect(keys).toContainEqual(["usage", "v2", "pricingCatalogSearch", "GPT", "curated"]);
    // 备用汇率进键（切汇率即换查询，避免跨币种串缓存）
    expect(keys).toContainEqual(["usage", "v2", "pricingAutoMatch", "gpt-5", "7.2"]);
    expect(keys).toContainEqual(["usage", "v2", "pricingQuote", "openai", "gpt-5", "7.2"]);
    expect(invokedCommands()).not.toContain("usage_collect");
  });

  it("7. 短 query / 空 modelId / 缺价格键不发请求（enabled 门控）", async () => {
    const { wrapper } = createWrapper();
    const { result } = renderHook(
      () => ({
        shortQuery: useUsagePricingCatalogSearchQuery("g", "curated"),
        emptyModel: useUsagePricingAutoMatchQuery("   "),
        missingQuote: useUsagePricingQuoteQuery(null, null),
      }),
      { wrapper }
    );
    expect(result.current.shortQuery.fetchStatus).toBe("idle");
    expect(result.current.emptyModel.fetchStatus).toBe("idle");
    expect(result.current.missingQuote.fetchStatus).toBe("idle");
    expect(invokedCommands()).toHaveLength(0);
  });

  it("8. 模型配置写命令成功后失效 USAGE_KEY（观察条目与费用联动重取）", async () => {
    const { wrapper } = createWrapper();
    const { result } = renderHook(
      () => ({
        observed: useUsageObservedEntriesQuery(),
        setDisplayName: useUsageModelDisplayNameSet(),
        setIdentity: useUsagePricingIdentitySet(),
      }),
      { wrapper }
    );
    await waitFor(() => expect(result.current.observed.isSuccess).toBe(true));
    const observedGets = () =>
      invokedCommands().filter((cmd) => cmd === "usage_observed_entries_get").length;
    const before = observedGets();

    await act(async () => {
      await result.current.setDisplayName.mutateAsync({
        providerRaw: "openai",
        sourceId: null,
        modelId: "gpt-5",
        displayName: "重取触发名",
      });
    });
    await waitFor(() => expect(observedGets()).toBeGreaterThan(before));
    expect(
      result.current.observed.data!.find((entry) => entry.providerRaw === "openai")
        ?.displayModelName
    ).toBe("重取触发名");

    await act(async () => {
      await result.current.setIdentity.mutateAsync({
        providerRaw: "openai",
        sourceId: null,
        modelId: "gpt-5",
        reference: { providerId: "openai", modelId: "gpt-5" },
      });
    });
    await waitFor(() => expect(observedGets()).toBeGreaterThan(before + 1));
    expect(
      result.current.observed.data!.find((entry) => entry.providerRaw === "openai")?.pricingProvider
    ).toBe("openai");
    expect(invokedCommands()).not.toContain("usage_collect");
  });
});
