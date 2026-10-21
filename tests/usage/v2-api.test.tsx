// Task 5 · V2 身份/Token IPC 包装（src/lib/api/usageV2.ts）的行为锁。
// 锁三件事：命令名逐字、参数键（camelCase）逐字、返回形状来自同源 mock 夹具。
import { beforeEach, describe, expect, it } from "vitest";
import { tauriInvokeMock } from "../msw/tauriMocks";
import {
  usageBillingCommit,
  usageBillingGet,
  usageBillingPreview,
  usageBillingUndo,
  usageChannelOptions,
  usageCostExportCsv,
  usageCosts,
  usageDistribution,
  usageIdentities,
  usagePricingGet,
  usagePricingCatalog,
  usageModelDisplayNameSet,
  usageObservedEntriesGet,
  usagePricingAutoMatch,
  usagePricingCatalogSearch,
  usagePricingIdentitySet,
  usagePricingOverrideDelete,
  usagePricingOverridesGet,
  usagePricingOverrideSet,
  usagePricingQuoteGet,
  usagePricingRefresh,
  usagePricingSetEnabled,
  usageProviderAliasDelete,
  usageProviderAliasesGet,
  usageProviderAliasUpsert,
  usageTrend,
} from "@/lib/api/usageV2";
import { mockBillingConfig, resetUsageV2MockState } from "@/lib/usage/mockFixtures";
import type { UsageRange } from "@/types/usage";
import type {
  BillingConfig,
  PriceOverrideRequest,
  ProviderAliasRule,
  TrendSelection,
} from "@/types/usageV2";

const TODAY: UsageRange = { preset: "today" };
const LAST7D: UsageRange = { preset: "last7d" };
const ALL: TrendSelection = { scope: "all" };
const TOOL: TrendSelection = { scope: "tool", selectedId: "codex" };

function invocations(): Array<[string, string[]]> {
  return tauriInvokeMock.mock.calls.map(([cmd, args]): [string, string[]] => [
    cmd,
    Object.keys((args ?? {}) as Record<string, unknown>).sort(),
  ]);
}

beforeEach(() => {
  tauriInvokeMock.mockClear();
  resetUsageV2MockState();
});

describe("V2 IPC 包装（src/lib/api/usageV2.ts）", () => {
  it("1. 七条命令名与参数键逐字对齐，返回契约形状", async () => {
    const initial = await usageBillingGet();
    expect(initial.revision).toBe(0);
    expect(initial.profiles).toEqual([]);
    expect(initial.rules).toEqual([]);
    expect(initial.pricingRules).toEqual([]);

    const candidate = mockBillingConfig();
    const preview = await usageBillingPreview(candidate, LAST7D);
    expect(preview.baseRevision).toBe(0);
    expect(preview.candidate.revision).toBe(1);
    expect(Array.isArray(preview.matchedIdentities)).toBe(true);
    expect(preview.matchedTokens).toBeGreaterThanOrEqual(0);

    const revision = await usageBillingCommit({ expectedRevision: 0, candidate });
    expect(revision.revision).toBe(1);

    const undone = await usageBillingUndo(1, 0);
    expect(undone.revision).toBe(2);

    const identities = await usageIdentities(LAST7D, { toolIds: ["codex"] });
    expect(Array.isArray(identities)).toBe(true);

    const trend = await usageTrend(LAST7D, TOOL);
    expect(Object.keys(trend).sort()).toEqual([
      "available",
      "compare",
      "hero",
      "reason",
      "selection",
      "trend",
    ]);
    expect(trend.selection).toEqual(TOOL);

    const distribution = await usageDistribution(LAST7D, "billingProfile");
    expect(Object.keys(distribution).sort()).toEqual(["groupBy", "rows"]);
    expect(distribution.groupBy).toBe("billingProfile");

    expect(invocations()).toEqual([
      ["usage_billing_get", []],
      ["usage_billing_preview", ["candidate", "range"]],
      ["usage_billing_commit", ["request"]],
      ["usage_billing_undo", ["expectedRevision", "targetRevision"]],
      ["usage_identities", ["filters", "range"]],
      ["usage_trend", ["range", "selection"]],
      ["usage_distribution", ["groupBy", "range"]],
    ]);
  });

  it("2. 趋势只向 usage_trend 传局部 selection，不碰旧 dashboard 命令", async () => {
    await usageTrend(TODAY, ALL);
    await usageTrend(TODAY, TOOL);
    const calls = tauriInvokeMock.mock.calls.filter(([cmd]) => cmd === "usage_trend");
    expect(calls).toHaveLength(2);
    for (const [, args] of calls) {
      expect(Object.keys(args as Record<string, unknown>).sort()).toEqual(["range", "selection"]);
    }
    expect(tauriInvokeMock.mock.calls.some(([cmd]) => cmd === "usage_dashboard")).toBe(false);
  });

  it("3. 旧 expectedRevision 被拒绝，detail 逐字为 billing revision conflict", async () => {
    const candidate = mockBillingConfig();
    await usageBillingCommit({ expectedRevision: 0, candidate });
    await expect(usageBillingCommit({ expectedRevision: 0, candidate })).rejects.toMatchObject({
      code: "usage-settings-invalid",
      detail: "billing revision conflict",
    });
  });

  it("4. 同一份夹具在两条 mock 路径上形状一致（billing get 单调递增）", async () => {
    const before = await usageBillingGet();
    const committed = await usageBillingCommit({
      expectedRevision: before.revision,
      candidate: mockBillingConfig(),
    });
    const after = await usageBillingGet();
    expect(after.revision).toBe(committed.revision);
    expect(after.revision).toBe(before.revision + 1);
  });

  it("5. 后 5 条定价/费用命令名、参数与 CostRow 形状", async () => {
    const pricing = await usagePricingGet();
    expect(Object.keys(pricing).sort()).toEqual([
      "enabled",
      "fetchedAt",
      "fxFallback",
      "fxFetchedAt",
      "fxRate",
      "fxSource",
      "lastError",
      "refreshing",
      "snapshotId",
      "stale",
    ]);
    const enabled = await usagePricingSetEnabled(true);
    expect(enabled.enabled).toBe(true);
    const refreshed = await usagePricingRefresh();
    expect(refreshed.enabled).toBe(true);

    const costs = await usageCosts(LAST7D, "7.2");
    expect(Object.keys(costs).sort()).toEqual([
      "fxFallback",
      "fxFetchedAt",
      "fxRate",
      "fxSource",
      "pricing",
      "range",
      "rows",
    ]);
    expect(costs.rows.length).toBeGreaterThan(0);
    for (const row of costs.rows) {
      expect(Object.keys(row).sort()).toEqual([
        "attributionKind",
        "billingProfileId",
        "cacheHitRate",
        "channelKey",
        "channelKind",
        "cnyConverted",
        "components",
        "coverageRate",
        "coverageRateCny",
        "displayModelName",
        "displayProvider",
        "estimatedCny",
        "estimatedUsd",
        "hasEmptyProvider",
        "identity",
        "key",
        "normalizedModelId",
        "observedProviders",
        "priceOverride",
        "priceRuleKind",
        "priceUpdatedAt",
        "pricedTokens",
        "pricedTokensCny",
        "pricingBasis",
        "pricingReference",
        "pricingReleaseDate",
        "pricingSnapshotId",
        "reasons",
        "sourceIds",
        "status",
        "statusCny",
        "timeBreakdown",
        "tokenUsage",
        "unpricedTokens",
        "unpricedTokensCny",
        "usdConverted",
      ]);
      expect("totalCost" in row).toBe(false);
      // 规则形态三值；分时行才带 Token 分布（固定价/未匹配为 null）
      expect(["none", "fixed", "time"]).toContain(row.priceRuleKind);
      expect(
        row.timeBreakdown === null ||
          (typeof row.timeBreakdown.peakTokens === "number" &&
            typeof row.timeBreakdown.offpeakTokens === "number" &&
            typeof row.timeBreakdown.dailyFallbackTokens === "number")
      ).toBe(true);
      expect(row.estimatedUsd === null || typeof row.estimatedUsd === "string").toBe(true);
      expect(row.estimatedCny === null || typeof row.estimatedCny === "string").toBe(true);
      expect(typeof row.usdConverted).toBe("boolean");
      expect(typeof row.cnyConverted).toBe("boolean");
      expect(["priced", "partial", "unpriced"]).toContain(row.statusCny);
      expect(row.pricingSnapshotId === null || typeof row.pricingSnapshotId === "string").toBe(
        true
      );
      expect(row.coverageRate === null || (row.coverageRate >= 0 && row.coverageRate <= 1)).toBe(
        true
      );
      expect(
        row.coverageRateCny === null || (row.coverageRateCny >= 0 && row.coverageRateCny <= 1)
      ).toBe(true);
      expect(Object.keys(row.components).sort()).toEqual([
        "cacheRead",
        "cacheWrite",
        "input",
        "output",
      ]);
      for (const component of Object.values(row.components)) {
        if (component === null) continue;
        expect(Object.keys(component).sort()).toEqual([
          "cnyConverted",
          "estimatedCny",
          "estimatedUsd",
          "tokens",
          "unitCnyPerMillion",
          "unitUsdPerMillion",
          "usdConverted",
        ]);
        expect(typeof component.usdConverted).toBe("boolean");
        expect(typeof component.cnyConverted).toBe("boolean");
      }
    }
    // 夹具必须同时覆盖 priced / partial / unpriced 与 null 金额。
    expect(new Set(costs.rows.map((row) => row.status))).toEqual(
      new Set(["priced", "partial", "unpriced"])
    );
    expect(costs.rows.some((row) => row.estimatedUsd === null)).toBe(true);
    expect(costs.rows.some((row) => row.coverageRate === null)).toBe(true);
    // 夹具覆盖固定价形态（gpt-5 有覆盖价）与无规则形态；分时行由 UI 单测本地构造
    expect(costs.rows.find((row) => row.normalizedModelId === "gpt-5")?.priceRuleKind).toBe(
      "fixed"
    );
    expect(costs.rows.some((row) => row.priceRuleKind === "none")).toBe(true);

    const csv = await usageCostExportCsv(LAST7D);
    expect(csv.split("\n")[0]).toBe(
      "billing_profile_id,billing_profile_name,source_ids,observed_provider,model_id,attribution_kind,token_usage,estimated_usd,status,priced_tokens,unpriced_tokens,coverage_rate,pricing_provider,pricing_model,pricing_snapshot_id,pricing_basis,price_updated_at,channel_key,channel_kind,display_provider,normalized_model_id,pricing_release_date,price_override,price_rule_kind"
    );
    expect(csv.toLowerCase()).not.toContain("total");

    expect(invocations()).toEqual([
      ["usage_pricing_get", []],
      ["usage_pricing_set_enabled", ["enabled"]],
      ["usage_pricing_refresh", []],
      ["usage_costs", ["fallbackRate", "range"]],
      ["usage_cost_export_csv", ["range"]],
    ]);
  });

  it("5b. 定价 stale 不含 enabled：enable→refresh→disable 后 stale=false", async () => {
    // 起始无 active 快照 ⇒ stale
    expect((await usagePricingGet()).stale).toBe(true);

    // 开启只切开关，不伪造快照（真实后端只触发后台刷新）
    const enabled = await usagePricingSetEnabled(true);
    expect(enabled.enabled).toBe(true);
    expect(enabled.stale).toBe(true);
    expect(enabled.snapshotId).toBeNull();

    const refreshed = await usagePricingRefresh();
    expect(refreshed.enabled).toBe(true);
    expect(refreshed.stale).toBe(false);
    expect(refreshed.snapshotId).not.toBeNull();

    // 停用不影响快照新鲜度：stale 仍为 false（真实 status 不以 enabled 参与 stale）
    const disabled = await usagePricingSetEnabled(false);
    expect(disabled.enabled).toBe(false);
    expect(disabled.stale).toBe(false);
    expect(disabled.snapshotId).toBe(refreshed.snapshotId);
  });

  it("5c. 目录搜索命令：参数 query/limit、无快照空数组、大小写不敏感与条目形状", async () => {
    // 无快照：空数组（前端用 usage_pricing_get 区分“无快照”）
    expect(await usagePricingCatalog("deepseek", 20)).toEqual([]);

    await usagePricingSetEnabled(true);
    await usagePricingRefresh();

    const entries = await usagePricingCatalog("DEEPSEEK", 20);
    expect(entries).toHaveLength(1);
    expect(Object.keys(entries[0]).sort()).toEqual([
      "conditional",
      "curatedKind",
      "hasRates",
      "modelId",
      "providerId",
      "releaseDate",
    ]);
    expect(entries[0]).toMatchObject({
      providerId: "ollama-cloud",
      modelId: "deepseek-v4.1-flash",
      hasRates: true,
      conditional: false,
    });

    const all = await usagePricingCatalog("", 2);
    expect(all).toHaveLength(2);
    expect(all.map((entry) => entry.providerId)).toEqual(["ollama-cloud", "ollama-cloud"]);

    const calls = tauriInvokeMock.mock.calls.filter(([cmd]) => cmd === "usage_pricing_catalog");
    expect(calls[calls.length - 1][1]).toEqual({ query: "", limit: 2 });
  });

  it("6. mock undo 恢复历史内容，缺失目标按契约拒绝", async () => {
    const profile = (id: string) => ({
      id,
      name: `Profile ${id}`,
      kind: "api",
      note: "",
      archived: false,
    });
    const candidate = (ids: string[]): BillingConfig => ({
      revision: 0,
      profiles: ids.map(profile),
      rules: [],
      pricingRules: [],
    });

    await usageBillingCommit({ expectedRevision: 0, candidate: candidate(["A"]) });
    await usageBillingCommit({
      expectedRevision: 1,
      candidate: candidate(["A", "B"]),
    });
    expect((await usageBillingGet()).profiles.map((item) => item.id)).toEqual(["A", "B"]);

    const undone = await usageBillingUndo(2, 1);
    expect(undone.revision).toBe(3);
    const restored = await usageBillingGet();
    expect(restored.profiles.map((item) => item.id)).toEqual(["A"]);

    await expect(usageBillingUndo(3, 99)).rejects.toMatchObject({
      code: "usage-settings-invalid",
      detail: "billing revision not found: 99",
    });
  });

  it("7. 七条别名/渠道/覆盖价命令名、参数键与返回形状逐字对齐", async () => {
    const initial = await usageProviderAliasesGet();
    expect(Object.keys(initial)).toEqual(["aliases"]);
    expect(initial.aliases).toHaveLength(2);
    for (const rule of initial.aliases) {
      expect(Object.keys(rule).sort()).toEqual([
        "displayName",
        "id",
        "kind",
        "modelId",
        "provider",
        "sourceId",
      ]);
      expect(["provider", "toolModel"]).toContain(rule.kind);
    }

    const added: ProviderAliasRule = {
      id: "alias-openai",
      kind: "provider",
      provider: "openai",
      sourceId: null,
      modelId: null,
      displayName: "OpenAI",
    };
    const afterAdd = await usageProviderAliasUpsert(added);
    expect(afterAdd.aliases).toHaveLength(3);
    expect(afterAdd.aliases.find((rule) => rule.id === "alias-openai")).toEqual(added);

    // 同 id 是覆盖式更新（不是重复键）
    const edited = await usageProviderAliasUpsert({ ...added, displayName: "OpenAI API" });
    expect(edited.aliases.find((rule) => rule.id === "alias-openai")?.displayName).toBe(
      "OpenAI API"
    );

    // 目标键被另一条规则占用 → 拒绝，且不静默合并
    await expect(
      usageProviderAliasUpsert({ ...added, id: "alias-openai-copy" })
    ).rejects.toMatchObject({
      code: "usage-settings-invalid",
      detail: "provider alias key already exists: provider:openai",
    });

    const afterDelete = await usageProviderAliasDelete("alias-openai");
    expect(afterDelete.aliases.map((rule) => rule.id)).not.toContain("alias-openai");
    // 不存在的 id 是幂等空操作
    const afterNoop = await usageProviderAliasDelete("alias-does-not-exist");
    expect(afterNoop).toEqual(afterDelete);

    const options = await usageChannelOptions(TODAY);
    expect(options.length).toBeGreaterThan(0);
    for (const option of options) {
      expect(Object.keys(option).sort()).toEqual([
        "billingProfileId",
        "key",
        "kind",
        "label",
        "tokenUsage",
      ]);
      expect(["billingProfile", "alias", "provider"]).toContain(option.kind);
      expect(option.key.includes(":")).toBe(true);
    }

    const overridesBefore = await usagePricingOverridesGet();
    expect(overridesBefore.length).toBeGreaterThan(0);
    for (const view of overridesBefore) {
      expect(Object.keys(view).sort()).toEqual([
        "cacheRead",
        "cacheWrite",
        "currency",
        "input",
        "normalizedModelId",
        "output",
        "releaseDate",
        "snapshotDate",
        "sourceModel",
        "sourceProvider",
        "timeRule",
        "updatedAt",
      ]);
      expect(typeof view.updatedAt).toBe("number");
      expect(["USD", "CNY"]).toContain(view.currency);
    }

    const request: PriceOverrideRequest = {
      normalizedModelId: "GPT-5",
      currency: "USD",
      input: "2.5",
      cacheRead: null,
      cacheWrite: null,
      output: null,
    };
    const afterSet = await usagePricingOverrideSet(request);
    const updated = afterSet.find((view) => view.normalizedModelId === "gpt-5");
    expect(updated?.input).toBe("2.5");
    // 币种随请求透传（覆盖价按所选币种存储，显示另一币种时按当日汇率换算）
    expect(updated?.currency).toBe("USD");
    // 未覆盖的分量必须显式为 null，不得残留旧值
    expect(updated?.output).toBeNull();
    expect(updated?.cacheRead).toBeNull();
    expect(updated?.cacheWrite).toBeNull();

    await expect(
      usagePricingOverrideSet({
        normalizedModelId: "gpt-5",
        currency: "USD",
        input: null,
        cacheRead: null,
        cacheWrite: null,
        output: null,
      })
    ).rejects.toMatchObject({
      code: "usage-settings-invalid",
      detail: "price override must set at least one rate",
    });
    await expect(
      usagePricingOverrideSet({
        normalizedModelId: "gpt-5",
        currency: "CNY",
        input: "abc",
        cacheRead: null,
        cacheWrite: null,
        output: null,
      })
    ).rejects.toMatchObject({
      code: "usage-settings-invalid",
      detail: "price override rate is invalid: input",
    });

    const afterOverrideDelete = await usagePricingOverrideDelete("gpt-5");
    expect(afterOverrideDelete.map((view) => view.normalizedModelId)).not.toContain("gpt-5");

    expect(invocations()).toEqual([
      ["usage_provider_aliases_get", []],
      ["usage_provider_alias_upsert", ["rule"]],
      ["usage_provider_alias_upsert", ["rule"]],
      ["usage_provider_alias_upsert", ["rule"]],
      ["usage_provider_alias_delete", ["id"]],
      ["usage_provider_alias_delete", ["id"]],
      ["usage_channel_options", ["range"]],
      ["usage_pricing_overrides_get", []],
      ["usage_pricing_override_set", ["request"]],
      ["usage_pricing_override_set", ["request"]],
      ["usage_pricing_override_set", ["request"]],
      ["usage_pricing_override_delete", ["normalizedModelId"]],
    ]);
  });

  it("5b. 分时规则随 request.timeRule 透传，固定价显式 null", async () => {
    // 固定价：timeRule = null（wire 上仍是显式键，与 Rust Option 序列化一致）
    const fixed = await usagePricingOverrideSet({
      normalizedModelId: "glm-5.3",
      currency: "USD",
      input: "2",
      cacheRead: null,
      cacheWrite: null,
      output: "8",
      timeRule: null,
    });
    expect(fixed.find((view) => view.normalizedModelId === "glm-5-3")?.timeRule).toBeNull();

    // 分时价：高峰四价 + 时区/窗口/低谷四价原样往返
    const timeRule = {
      timezone: "asiaShanghai" as const,
      windows: [
        { start: "09:00", end: "12:00" },
        { start: "22:00", end: "02:00" },
      ],
      offpeakInput: "1",
      offpeakCacheRead: "0.1",
      offpeakCacheWrite: null,
      offpeakOutput: "4",
    };
    const afterSet = await usagePricingOverrideSet({
      normalizedModelId: "glm-5.3",
      currency: "USD",
      input: "2",
      cacheRead: null,
      cacheWrite: null,
      output: "8",
      timeRule,
    });
    const view = afterSet.find((item) => item.normalizedModelId === "glm-5-3");
    expect(view?.timeRule).toEqual(timeRule);

    await usagePricingOverrideDelete("glm-5.3");
  });

  it("8. 六条观察条目/模型配置/精选目录命令名、参数键与返回形状逐字对齐", async () => {
    // 观察条目 get：夹具含非空 provider、历史 provider、多工具同显示名、空 provider 条目。
    const entries = await usageObservedEntriesGet();
    expect(entries.length).toBe(4);
    for (const entry of entries) {
      expect(Object.keys(entry).sort()).toEqual([
        "displayModelName",
        "displayProvider",
        "modelId",
        "normalizedModelId",
        "pricingModel",
        "pricingProvider",
        "providerRaw",
        "recentTokens7d",
        "sourceId",
        "totalTokens",
      ]);
      expect(typeof entry.recentTokens7d).toBe("number");
      // 归一化模型 ID（2026-10-20 第三轮）：与后端 normalize_model_id 同口径；空模型 = 空串
      expect(typeof entry.normalizedModelId).toBe("string");
    }
    const openai = entries.find((entry) => entry.providerRaw === "openai");
    expect(openai?.displayProvider).toBe("OpenAI");
    expect(openai?.normalizedModelId).toBe("gpt-5");
    // `GLM-5.3` → `glm-5-3`（小写 + 分隔符折叠）
    const edenai = entries.find((entry) => entry.providerRaw === "edenai");
    expect(edenai?.normalizedModelId).toBe("glm-5-3");
    const workbuddy = entries.find((entry) => entry.providerRaw === null);
    expect(workbuddy?.sourceId).toBe("workbuddy");
    // 空模型的归一化键也是空串（快捷覆盖入口按它 disabled）
    expect(workbuddy?.normalizedModelId).toBe("");

    // 显示名 set：参数键 request；返回刷新后的全量列表。
    const afterDisplay = await usageModelDisplayNameSet({
      providerRaw: "openai",
      sourceId: null,
      modelId: "gpt-5",
      displayName: "我的 GPT",
    });
    expect(afterDisplay.find((entry) => entry.providerRaw === "openai")?.displayModelName).toBe(
      "我的 GPT"
    );
    // 空白显示名 = 意图为设置却给了空白 → 固定 detail 拒绝。
    await expect(
      usageModelDisplayNameSet({
        providerRaw: "openai",
        sourceId: null,
        modelId: "gpt-5",
        displayName: "   ",
      })
    ).rejects.toMatchObject({
      code: "usage-settings-invalid",
      detail: "model config display name is empty after trim",
    });
    // 幽灵身份拒绝。
    await expect(
      usageModelDisplayNameSet({
        providerRaw: "ghost",
        sourceId: null,
        modelId: "m",
        displayName: "X",
      })
    ).rejects.toMatchObject({
      code: "usage-settings-invalid",
      detail: "model config entry not observed",
    });

    // 计价身份 set：null = 清除（回 null 而不是报错）。
    const afterIdentity = await usagePricingIdentitySet({
      providerRaw: "openai",
      sourceId: null,
      modelId: "gpt-5",
      reference: { providerId: "openai", modelId: "gpt-5" },
    });
    expect(afterIdentity.find((entry) => entry.providerRaw === "openai")?.pricingProvider).toBe(
      "openai"
    );
    const cleared = await usagePricingIdentitySet({
      providerRaw: "openai",
      sourceId: null,
      modelId: "gpt-5",
      reference: null,
    });
    expect(cleared.find((entry) => entry.providerRaw === "openai")?.pricingProvider).toBeNull();

    // 作用域目录搜索：缺省 curated 只含精选条目；all 含非精选；providerCount=232。
    const curated = await usagePricingCatalogSearch("", 50);
    expect(curated.providerCount).toBe(232);
    expect(
      curated.entries.every((entry) => entry.curatedKind !== null),
      "缺省 curated 作用域只含精选条目"
    ).toBe(true);
    const all = await usagePricingCatalogSearch("gpt", 50, "all");
    expect(all.entries.some((entry) => entry.curatedKind === null)).toBe(true);
    await expect(usagePricingCatalogSearch("", 50, "everything")).rejects.toMatchObject({
      code: "usage-settings-invalid",
      detail: "pricing catalog scope is invalid",
    });

    // 自动匹配（固定精选：官方优先、OpenRouter 兜底）；与 quote 同款快照门控：无快照 ⇒ none。
    const gated = await usagePricingAutoMatch("gpt-5", "7.2");
    expect(gated.kind).toBe("none");
    expect(gated.reference).toBeNull();
    await usagePricingSetEnabled(true);
    await usagePricingRefresh();
    const matched = await usagePricingAutoMatch("gpt-5", "7.2");
    expect(matched.kind).toBe("exact");
    expect(matched.reference?.providerId).toBe("openai", "精选内官方优先");
    // OpenRouter 精选条目的 exact 同样可命中（官方优先、OpenRouter 兜底）
    const openrouterMatch = await usagePricingAutoMatch("claude-sonnet-4-5", "7.2");
    expect(openrouterMatch.kind).toBe("exact");
    expect(openrouterMatch.reference?.providerId).toBe("openrouter");
    expect(openrouterMatch.quote?.cacheRead).toBe("0.5");
    const miss = await usagePricingAutoMatch("never-matches-anything", "7.2");
    expect(miss.kind).toBe("none");
    expect(miss.reference).toBeNull();

    // 价格预览：精确键 USD 四价 + CNY 四价 + 汇率三元组 + 快照日期；无该键 ⇒ null（不是错误）。
    const quote = await usagePricingQuoteGet("openai", "gpt-5", "7.2");
    expect(quote).not.toBeNull();
    expect(Object.keys(quote as object).sort()).toEqual([
      "cacheRead",
      "cacheReadCny",
      "cacheWrite",
      "cacheWriteCny",
      "cnyConverted",
      "conditional",
      "fxFallback",
      "fxFetchedAt",
      "fxRate",
      "input",
      "inputCny",
      "output",
      "outputCny",
      "reference",
      "releaseDate",
      "snapshotDate",
    ]);
    expect(quote?.input).toBe("1.25");
    expect(quote?.inputCny).toBe("9");
    // openai/gpt-5 无原生人民币价 → CNY 四价由 USD 按 7.2 换算（cnyConverted=true）
    expect(quote?.cnyConverted).toBe(true);
    expect(quote?.fxRate).toBe("7.2");
    expect(quote?.fxFallback).toBe(false);
    expect(quote?.snapshotDate).not.toBeNull();
    const missQuote = await usagePricingQuoteGet("openai", "not-a-model", "7.2");
    expect(missQuote).toBeNull();

    expect(invocations()).toEqual([
      ["usage_observed_entries_get", []],
      ["usage_model_display_name_set", ["request"]],
      ["usage_model_display_name_set", ["request"]],
      ["usage_model_display_name_set", ["request"]],
      ["usage_pricing_identity_set", ["request"]],
      ["usage_pricing_identity_set", ["request"]],
      ["usage_pricing_catalog_search", ["limit", "query", "scope"]],
      ["usage_pricing_catalog_search", ["limit", "query", "scope"]],
      ["usage_pricing_catalog_search", ["limit", "query", "scope"]],
      ["usage_pricing_auto_match", ["fallbackRate", "modelId"]],
      ["usage_pricing_set_enabled", ["enabled"]],
      ["usage_pricing_refresh", []],
      ["usage_pricing_auto_match", ["fallbackRate", "modelId"]],
      ["usage_pricing_auto_match", ["fallbackRate", "modelId"]],
      ["usage_pricing_auto_match", ["fallbackRate", "modelId"]],
      ["usage_pricing_quote_get", ["fallbackRate", "modelId", "providerId"]],
      ["usage_pricing_quote_get", ["fallbackRate", "modelId", "providerId"]],
    ]);
  });
});
