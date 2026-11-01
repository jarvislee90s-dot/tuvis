// 双端 mock parity（Task 20 / 契约 §5「前端类型与双端 mock 的所有权」）：
// * `tests/msw/tauriMocks.ts`（vitest/jsdom 走的那个）与 `src/tauri-mock.ts`（浏览器/Playwright
//   渲染时自安装的那个）必须**逐字同形**——mock 与真实现的漂移、以及两端 mock 之间的漂移，
//   是本任务最阴的一类缺陷（浏览器里看着对、单测里也看着对，真机上形状不一致）。
// * 断言分两层：① **同一份结构断言跑两端**（下面 describe.each）；② 两端样例**深比较**。
// * Rust 真实现的 wire 形状由 `src-tauri/tests/usage_ipc_test.rs` 用同一组键名锁住，
//   本文件只负责 mock 侧（**mock 跑通 ≠ 真跑通**）。
import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";
import path from "node:path";
import { tauriInvokeMock } from "../msw/tauriMocks";
import { KNOWN_USAGE_CODES } from "@/components/usage/usageErrors";
import {
  mockPricingRefresh,
  mockPricingSetEnabled,
  resetUsageV2MockState,
} from "@/lib/usage/mockFixtures";
import type {
  UsageCard,
  UsageCollectResult,
  UsageDashboard,
  UsageMetrics,
  UsageRecords,
  UsageRow,
  UsageMetric,
  UsageSettings,
  UsageSourceStatus,
  UsageBuckets,
  WorkSummary,
} from "@/types/usage";
import type {
  ChannelOption,
  ObservedModelEntry,
  PriceOverrideView,
  PricingAutoMatch,
  PricingCatalogEntry,
  PricingCatalogSearch,
  PricingQuotePreview,
  ProviderAliasConfig,
  ProviderAliasRule,
} from "@/types/usageV2";
// 副作用导入：非 Tauri 环境（jsdom）下它会把 mock 装到 window.__TAURI_INTERNALS__ 上
import "@/tauri-mock";

type Invoke = (cmd: string, args?: Record<string, unknown>) => Promise<unknown>;

const mswInvoke: Invoke = tauriInvokeMock;

const browserInternals = (window as unknown as { __TAURI_INTERNALS__?: { invoke: Invoke } })
  .__TAURI_INTERNALS__;
const browserInvoke: Invoke = (cmd, args) => {
  if (!browserInternals) throw new Error("src/tauri-mock.ts 未在 jsdom 里安装 __TAURI_INTERNALS__");
  return browserInternals.invoke(cmd, args);
};

const SOURCE_IDS = ["claude", "codex", "kimi", "opencode", "workbuddy", "zcode", "dsh"];

/** 契约 §2 的 12 个可得性指标值（类型桥：漏一个值 → 编译错） */
const USAGE_METRICS: readonly UsageMetric[] = [
  "sessions",
  "turn",
  "errorModel",
  "errorTurn",
  "errorTool",
  "interrupted",
  "longestTurn",
  "toolCalls",
  "toolAvgMs",
  "topTool",
  "topToolMs",
  "userEst",
];
const USAGE_COMMANDS = [
  "usage_collect",
  "usage_collect_visible",
  "usage_dashboard",
  "usage_records",
  "usage_export_csv",
  "usage_get_settings",
  "usage_set_settings",
  "usage_billing_get",
  "usage_billing_preview",
  "usage_billing_commit",
  "usage_billing_undo",
  "usage_identities",
  "usage_trend",
  "usage_distribution",
  "usage_pricing_get",
  "usage_pricing_set_enabled",
  "usage_pricing_refresh",
  "usage_pricing_catalog",
  "usage_costs",
  "usage_cost_export_csv",
  // Task 6：别名 / 渠道选项 / 手动覆盖价
  "usage_provider_aliases_get",
  "usage_provider_alias_upsert",
  "usage_provider_alias_delete",
  "usage_channel_options",
  "usage_pricing_overrides_get",
  "usage_pricing_override_set",
  "usage_pricing_override_delete",
  // 2026-10-20 计划 Task 5：观察条目 / 模型条目配置 / 精选价目目录
  "usage_observed_entries_get",
  "usage_model_display_name_set",
  "usage_pricing_identity_set",
  "usage_pricing_catalog_search",
  "usage_pricing_auto_match",
  "usage_pricing_quote_get",
];

/** 键名列表的**类型桥**：`keyof T` 逼 `src/types/usage.ts` 真有这些字段（镜像改名 → 编译错） */
function keysOf<T>(keys: readonly (keyof T)[]): string[] {
  return [...(keys as readonly string[])].sort();
}

function actualKeys(o: unknown): string[] {
  return Object.keys(o as Record<string, unknown>).sort();
}

/** 深拷贝并抹掉时间戳（两端 `Date.now()` 必然不同，其余必须逐字相同） */
function stripTimestamps<T>(v: T): T {
  if (Array.isArray(v)) return v.map(stripTimestamps) as unknown as T;
  if (v && typeof v === "object") {
    return Object.fromEntries(
      Object.entries(v as Record<string, unknown>).map(([k, val]) => [
        k,
        k === "collectedAt" ? 0 : stripTimestamps(val),
      ])
    ) as T;
  }
  return v;
}

const today = () => ({ range: { preset: "today" }, groupBy: "tool", filters: {} });

async function assertUsageCollect(invoke: Invoke) {
  const c = (await invoke("usage_collect", { force: true })) as UsageCollectResult;
  expect(c, "usage_collect 未 mock（undefined = 前端会静默拿到空数据）").toBeTruthy();
  expect(actualKeys(c)).toEqual(
    keysOf<UsageCollectResult>(["collectedAt", "durationMs", "sources", "totalNewRecords"])
  );
  expect(c.sources).toHaveLength(7);
  expect(c.durationMs).toBeGreaterThan(0);
  expect(c.collectedAt).toBeGreaterThan(0);
  const ids: string[] = [];
  for (const s of c.sources) {
    const keys = keysOf<UsageSourceStatus>(["sourceId", "ok", "parsedFiles", "newRecords"]);
    // `errorCode?: string` → 只在失败源出现（skip_serializing_if 的 mock 侧等价物）
    expect(actualKeys(s)).toEqual(s.ok ? keys : [...keys, "errorCode"].sort());
    expect(typeof s.ok).toBe("boolean");
    expect(typeof s.parsedFiles).toBe("number");
    expect(typeof s.newRecords).toBe("number");
    if (!s.ok) {
      // W-23：失败源必须给码，且码要在前端白名单里（否则静默收敛成通用错误）
      expect(KNOWN_USAGE_CODES as readonly string[]).toContain(s.errorCode);
    }
    ids.push(s.sourceId);
  }
  expect([...ids].sort()).toEqual([...SOURCE_IDS].sort());
  expect(c.sources.filter((s) => !s.ok)).toHaveLength(1);
  expect(c.totalNewRecords).toBe(c.sources.reduce((n, s) => n + s.newRecords, 0));
}

function assertBuckets(b: UsageBuckets) {
  expect(actualKeys(b)).toEqual(
    keysOf<UsageBuckets>(["inputFresh", "cacheRead", "cacheWrite", "output"])
  );
  for (const v of Object.values(b)) expect(typeof v).toBe("number");
}

function assertMetrics(m: UsageMetrics) {
  expect(actualKeys(m)).toEqual(
    keysOf<UsageMetrics>(["requestTotal", "cacheHitRate", "userEst", "requests"])
  );
  // userEst 是 `| null`（不可得 ≠ 0）→ 必须**在场**
  expect("userEst" in m).toBe(true);
  expect(m.userEst === null || typeof m.userEst === "number").toBe(true);
}

function assertRow(r: UsageRow) {
  expect(actualKeys(r)).toEqual(
    keysOf<UsageRow>(["key", "label", "buckets", "metrics", "sourceKind", "isSubagent"])
  );
  expect(["measured", "inferred", "unknown"]).toContain(r.sourceKind);
  // isSubagent 是 `boolean | null`（契约 §2 2026-10-03 裁决：日档 / 记录页卡内行**算不出** → null）。
  // `null` 同样必须**在场**（`undefined`/缺键会让下面两式都判假）——与紧邻 `assertMetrics` 的
  // `userEst` 同款写法；**不得**把 `null` 当 `false`（「不知道」≠「非子代理」）。
  expect("isSubagent" in r).toBe(true);
  expect(r.isSubagent === null || typeof r.isSubagent === "boolean").toBe(true);
  assertBuckets(r.buckets);
  assertMetrics(r.metrics);
}

function assertWorkSummary(ws: WorkSummary) {
  expect(actualKeys(ws)).toEqual(
    keysOf<WorkSummary>([
      "sessions",
      "turnsPerTool",
      "errorModel",
      "errorTurn",
      "errorTool",
      "interrupted",
      "toolCalls",
      "toolAvgMs",
      "topTool",
      "topToolMs",
      "longestTurnPerTool",
    ])
  );
  // 契约 `| null` 的**值**位：mock 里这两个工具不可得 → 必须显式 null（不是 undefined/缺键）
  expect(ws.turnsPerTool.workbuddy).toBeNull();
  expect(ws.longestTurnPerTool.workbuddy).toBeNull();
}

async function assertUsageDashboard(invoke: Invoke) {
  const d = (await invoke("usage_dashboard", {
    range: { preset: "today" },
    groupBy: "tool",
  })) as UsageDashboard;
  expect(d, "usage_dashboard 未 mock").toBeTruthy();
  expect(actualKeys(d)).toEqual(
    keysOf<UsageDashboard>([
      "range",
      "groupBy",
      "rows",
      "totals",
      "totalsBuckets",
      "hero",
      "trend",
      "compare",
      "recentSession",
      "workSummary",
      "availability",
      "collectedAt",
    ])
  );
  expect(d.groupBy).toBe("tool");
  // 契约 `| null`：必须**显式**给值。**Minor 2**：`in` / `Object.keys` 对值为 `undefined`
  // 的键同样为真 → 这里直接钉值（mock 的 compare 固定 null；recentSession 为对象或 null）。
  expect(d.compare === null || typeof d.compare === "object").toBe(true);
  expect(d.recentSession === null || typeof d.recentSession === "object").toBe(true);
  // hero = totals.requestTotal + totalsBuckets.output（说明书 P1 第 2 条）
  expect(d.hero).toBe(d.totals.requestTotal + d.totalsBuckets.output);
  assertMetrics(d.totals);
  assertBuckets(d.totalsBuckets);
  expect(Array.isArray(d.rows) && d.rows.length > 0).toBe(true);
  d.rows.forEach(assertRow);
  expect(d.trend.length).toBeGreaterThan(0);
  for (const p of d.trend) {
    expect(actualKeys(p)).toEqual(keysOf<typeof p>(["key", "label", "buckets", "metrics"]));
    assertBuckets(p.buckets);
    assertMetrics(p.metrics);
  }
  assertWorkSummary(d.workSummary);
  if (d.recentSession) {
    expect(actualKeys(d.recentSession)).toEqual(
      keysOf<NonNullable<UsageDashboard["recentSession"]>>([
        "sourceId",
        "sessionId",
        "title",
        "buckets",
        "metrics",
      ])
    );
    // title 是 `string | null`（会话维度表可空）
    expect("title" in d.recentSession).toBe(true);
  }
  expect(d.availability.length).toBeGreaterThan(0);
  for (const a of d.availability) {
    expect(USAGE_METRICS).toContain(a.metric);
    expect(typeof a.available).toBe("boolean");
  }
}

async function assertUsageRecords(invoke: Invoke) {
  const r = (await invoke("usage_records", {
    range: { preset: "today" },
    groupBy: "project",
    filters: {},
  })) as UsageRecords;
  expect(r, "usage_records 未 mock").toBeTruthy();
  expect(actualKeys(r)).toEqual(
    keysOf<UsageRecords>(["range", "groupBy", "cards", "availability", "collectedAt"])
  );
  expect(r.groupBy).toBe("project");
  expect(Array.isArray(r.cards) && r.cards.length > 0).toBe(true);
  for (const card of r.cards) {
    expect(actualKeys(card)).toEqual(
      keysOf<UsageCard>(["toolId", "toolLabel", "buckets", "metrics", "rows"])
    );
    assertBuckets(card.buckets);
    assertMetrics(card.metrics);
    expect(card.rows.length).toBeGreaterThan(0);
    card.rows.forEach(assertRow);
  }
}

async function assertUsageExportCsv(invoke: Invoke) {
  const csv = (await invoke("usage_export_csv", today())) as string;
  expect(typeof csv, "usage_export_csv 未 mock").toBe("string");
  const header = csv.split("\n")[0];
  expect(header.split(",")).toEqual([
    "groupKey",
    "label",
    "inputFresh",
    "cacheRead",
    "cacheWrite",
    "output",
    "requestTotal",
    "cacheHitRate",
    "requests",
    "userEst",
    "sourceKind",
  ]);
}

async function assertUsageSettings(invoke: Invoke) {
  const s = (await invoke("usage_get_settings")) as UsageSettings;
  expect(s, "usage_get_settings 未 mock").toBeTruthy();
  expect(actualKeys(s)).toEqual(
    keysOf<UsageSettings>([
      "enabled",
      "miniBarRange",
      "miniBarToolRows",
      "detailRetentionDays",
      "collectIntervalMin",
      "providerMapRules",
      "exportQuote",
      "exportPose",
    ])
  );
  const merged = (await invoke("usage_set_settings", {
    patch: { detailRetentionDays: 30 },
  })) as UsageSettings;
  expect(merged.detailRetentionDays).toBe(30);
  expect(merged.miniBarToolRows).toBe(s.miniBarToolRows); // patch 未提的字段不得被清空
}

/** 2026-10-20 计划 Task 5：观察条目 / 模型条目配置 / 精选目录命令的形状与语义。 */
async function assertV2ModelConfigCommands(invoke: Invoke) {
  const entryKeys = keysOf<ObservedModelEntry>([
    "providerRaw",
    "sourceId",
    "modelId",
    // 2026-10-20 第三轮：后端机械归一化键（`GLM-5.3` → `glm-5-3`；空模型 = 空串）
    "normalizedModelId",
    "displayProvider",
    "displayModelName",
    "pricingProvider",
    "pricingModel",
    "totalTokens",
    "recentTokens7d",
  ]);

  resetUsageV2MockState();
  const entries = (await invoke("usage_observed_entries_get")) as ObservedModelEntry[];
  expect(entries, "usage_observed_entries_get 未 mock").toBeTruthy();
  expect(entries.length).toBe(4);
  for (const entry of entries) {
    expect(actualKeys(entry)).toEqual(entryKeys);
    // 空 provider 条目必有 sourceId；非空 provider 条目 sourceId 为 null。
    if (entry.providerRaw === null) {
      expect(entry.sourceId).not.toBeNull();
    } else {
      expect(entry.sourceId).toBeNull();
    }
    // 全历史与最近 7 天两个排序用量都必须是数字（Top10 排序键）
    expect(typeof entry.totalTokens).toBe("number");
    expect(typeof entry.recentTokens7d).toBe("number");
    // 归一化键：非空模型必为非空字符串；空模型 = 空串（快捷覆盖入口按它 disabled）
    if (entry.modelId === "") {
      expect(entry.normalizedModelId).toBe("");
    } else {
      expect(entry.normalizedModelId.length).toBeGreaterThan(0);
    }
  }
  // 归一化口径样例：`GLM-5.3` → `glm-5-3`（两端同一夹具，键集与值都要同形）
  expect(entries.find((entry) => entry.providerRaw === "edenai")?.normalizedModelId).toBe(
    "glm-5-3"
  );

  // 显示名写命令：写后返回刷新列表；幽灵身份 / 空白名固定 detail 拒绝。
  const afterDisplay = (await invoke("usage_model_display_name_set", {
    request: {
      providerRaw: "openai",
      sourceId: null,
      modelId: "gpt-5",
      displayName: "Parity GPT",
    },
  })) as ObservedModelEntry[];
  expect(afterDisplay.find((entry) => entry.providerRaw === "openai")?.displayModelName).toBe(
    "Parity GPT"
  );
  for (const entry of afterDisplay) expect(actualKeys(entry)).toEqual(entryKeys);
  await expect(
    invoke("usage_model_display_name_set", {
      request: {
        providerRaw: "ghost",
        sourceId: null,
        modelId: "m",
        displayName: "X",
      },
    })
  ).rejects.toMatchObject({
    code: "usage-settings-invalid",
    detail: "model config entry not observed",
  });
  await expect(
    invoke("usage_model_display_name_set", {
      request: {
        providerRaw: "openai",
        sourceId: null,
        modelId: "gpt-5",
        displayName: "   ",
      },
    })
  ).rejects.toMatchObject({
    code: "usage-settings-invalid",
    detail: "model config display name is empty after trim",
  });

  // 计价身份写命令：成对写 / 清除（null = 清除，不报错）。
  const afterIdentity = (await invoke("usage_pricing_identity_set", {
    request: {
      providerRaw: "openai",
      sourceId: null,
      modelId: "gpt-5",
      reference: { providerId: "openai", modelId: "gpt-5" },
    },
  })) as ObservedModelEntry[];
  expect(afterIdentity.find((entry) => entry.providerRaw === "openai")?.pricingProvider).toBe(
    "openai"
  );
  const cleared = (await invoke("usage_pricing_identity_set", {
    request: {
      providerRaw: "openai",
      sourceId: null,
      modelId: "gpt-5",
      reference: null,
    },
  })) as ObservedModelEntry[];
  expect(cleared.find((entry) => entry.providerRaw === "openai")?.pricingProvider).toBeNull();

  // 作用域目录搜索：缺省 curated 只含精选；all 含非精选；providerCount 基准 232；
  // 非法 scope 固定 detail 拒绝。
  const curated = (await invoke("usage_pricing_catalog_search", {
    query: "",
    limit: 50,
  })) as PricingCatalogSearch;
  expect(curated.providerCount).toBe(232);
  expect(curated.entries.every((entry) => entry.curatedKind !== null)).toBe(true);
  const all = (await invoke("usage_pricing_catalog_search", {
    query: "",
    limit: 50,
    scope: "all",
  })) as PricingCatalogSearch;
  expect(all.providerCount).toBe(232, "providerCount 恒为全量目录 provider 数");
  expect(all.entries.some((entry) => entry.curatedKind === null)).toBe(true);
  await expect(
    invoke("usage_pricing_catalog_search", { query: "", scope: "everything" })
  ).rejects.toMatchObject({
    code: "usage-settings-invalid",
    detail: "pricing catalog scope is invalid",
  });

  // 自动匹配（固定精选：官方优先、OpenRouter 兜底）与价格预览。
  // 与 `usage_pricing_quote_get` 同款快照门控：无 active 快照 = 目录为空（kind=none），不报错。
  const gated = (await invoke("usage_pricing_auto_match", {
    modelId: "gpt-5",
  })) as PricingAutoMatch;
  expect(gated.kind).toBe("none");
  expect(gated.reference).toBeNull();
  expect(gated.quote).toBeNull();

  mockPricingSetEnabled(true);
  mockPricingRefresh();
  const matched = (await invoke("usage_pricing_auto_match", {
    modelId: "gpt-5",
  })) as PricingAutoMatch;
  expect(actualKeys(matched)).toEqual(keysOf<PricingAutoMatch>(["kind", "quote", "reference"]));
  expect(matched.kind).toBe("exact");
  expect(matched.reference?.providerId).toBe("openai", "精选内官方优先");
  // OpenRouter 精选条目的 exact 同样可命中（官方优先、OpenRouter 兜底）
  const openrouterMatch = (await invoke("usage_pricing_auto_match", {
    modelId: "claude-sonnet-4-5",
  })) as PricingAutoMatch;
  expect(openrouterMatch.kind).toBe("exact");
  expect(openrouterMatch.reference?.providerId).toBe("openrouter");
  expect(openrouterMatch.quote?.cacheRead).toBe("0.5");
  const miss = (await invoke("usage_pricing_auto_match", {
    modelId: "never-matches",
  })) as PricingAutoMatch;
  expect(miss.kind).toBe("none");
  expect(miss.reference).toBeNull();

  resetUsageV2MockState();
  mockPricingSetEnabled(true);
  mockPricingRefresh();
  const quote = (await invoke("usage_pricing_quote_get", {
    providerId: "openai",
    modelId: "gpt-5",
  })) as PricingQuotePreview;
  expect(actualKeys(quote)).toEqual(
    keysOf<PricingQuotePreview>([
      "reference",
      "input",
      "cacheRead",
      "cacheWrite",
      "output",
      "inputCny",
      "cacheReadCny",
      "cacheWriteCny",
      "outputCny",
      "cnyConverted",
      "releaseDate",
      "conditional",
      "snapshotDate",
      "fxRate",
      "fxFetchedAt",
      "fxFallback",
    ])
  );
  expect(quote.input).toBe("1.25");
  expect(quote.snapshotDate).not.toBeNull();
  // 双币字段：CNY 四价与汇率三元组必须显式在场（null = 不可得，不省键）
  for (const key of ["inputCny", "cacheReadCny", "cacheWriteCny", "outputCny"] as const) {
    expect(quote[key] === null || typeof quote[key] === "string").toBe(true);
  }
  expect(typeof quote.cnyConverted).toBe("boolean");
  expect(quote.fxRate === null || typeof quote.fxRate === "string").toBe(true);
  expect(typeof quote.fxFallback).toBe("boolean");
  const missQuote = (await invoke("usage_pricing_quote_get", {
    providerId: "openai",
    modelId: "not-a-model",
  })) as PricingQuotePreview | null;
  expect(missQuote).toBeNull();
}

async function assertV2AliasAndOverrideCommands(invoke: Invoke) {
  const aliasKeys = keysOf<ProviderAliasRule>([
    "id",
    "kind",
    "provider",
    "sourceId",
    "modelId",
    "displayName",
  ]);

  resetUsageV2MockState();
  const cfg = (await invoke("usage_provider_aliases_get")) as ProviderAliasConfig;
  expect(cfg, "usage_provider_aliases_get 未 mock").toBeTruthy();
  expect(actualKeys(cfg)).toEqual(keysOf<ProviderAliasConfig>(["aliases"]));
  expect(cfg.aliases.length).toBeGreaterThan(0);
  for (const rule of cfg.aliases) {
    expect(actualKeys(rule)).toEqual(aliasKeys);
    expect(["provider", "toolModel"]).toContain(rule.kind);
    expect(typeof rule.id).toBe("string");
    expect(typeof rule.displayName).toBe("string");
  }

  const added = (await invoke("usage_provider_alias_upsert", {
    rule: {
      id: "alias-openai",
      kind: "provider",
      provider: "openai",
      sourceId: null,
      modelId: null,
      displayName: "OpenAI",
    },
  })) as ProviderAliasConfig;
  expect(added.aliases.find((rule) => rule.id === "alias-openai")).toMatchObject({
    kind: "provider",
    provider: "openai",
    sourceId: null,
    modelId: null,
    displayName: "OpenAI",
  });
  for (const rule of added.aliases) expect(actualKeys(rule)).toEqual(aliasKeys);

  await expect(
    invoke("usage_provider_alias_upsert", {
      rule: {
        id: "alias-openai-copy",
        kind: "provider",
        provider: "openai",
        sourceId: null,
        modelId: null,
        displayName: "Copy",
      },
    })
  ).rejects.toMatchObject({
    code: "usage-settings-invalid",
    detail: "provider alias key already exists: provider:openai",
  });

  const deleted = (await invoke("usage_provider_alias_delete", {
    id: "alias-openai",
  })) as ProviderAliasConfig;
  expect(deleted.aliases.map((rule) => rule.id)).not.toContain("alias-openai");

  const options = (await invoke("usage_channel_options", {
    range: { preset: "last7d" },
  })) as ChannelOption[];
  expect(Array.isArray(options)).toBe(true);
  expect(options.length).toBeGreaterThan(0);
  for (const option of options) {
    expect(actualKeys(option)).toEqual(
      keysOf<ChannelOption>(["key", "label", "kind", "billingProfileId", "tokenUsage"])
    );
    expect(["billingProfile", "alias", "provider"]).toContain(option.kind);
    expect(option.kind === "billingProfile" ? option.billingProfileId !== null : true).toBe(true);
    expect(typeof option.tokenUsage).toBe("number");
  }

  const overrides = (await invoke("usage_pricing_overrides_get")) as PriceOverrideView[];
  expect(Array.isArray(overrides)).toBe(true);
  expect(overrides.length).toBeGreaterThan(0);
  const overrideKeys = keysOf<PriceOverrideView>([
    "normalizedModelId",
    "currency",
    "input",
    "cacheRead",
    "cacheWrite",
    "output",
    "updatedAt",
    "sourceProvider",
    "sourceModel",
    "releaseDate",
    "snapshotDate",
    "timeRule",
  ]);
  for (const view of overrides) {
    expect(actualKeys(view)).toEqual(overrideKeys);
    expect(typeof view.updatedAt).toBe("number");
    expect(["USD", "CNY"]).toContain(view.currency);
  }

  const set = (await invoke("usage_pricing_override_set", {
    request: {
      normalizedModelId: "GPT-5",
      currency: "CNY",
      input: "2.5",
      cacheRead: null,
      cacheWrite: null,
      output: null,
    },
  })) as PriceOverrideView[];
  const updated = set.find((view) => view.normalizedModelId === "gpt-5");
  expect(updated).toBeTruthy();
  expect(updated).toMatchObject({
    normalizedModelId: "gpt-5",
    currency: "CNY",
    input: "2.5",
    cacheRead: null,
    cacheWrite: null,
    output: null,
  });
  for (const view of set) expect(actualKeys(view)).toEqual(overrideKeys);

  await expect(
    invoke("usage_pricing_override_set", {
      request: {
        normalizedModelId: "gpt-5",
        input: null,
        cacheRead: null,
        cacheWrite: null,
        output: null,
      },
    })
  ).rejects.toMatchObject({
    code: "usage-settings-invalid",
    detail: "price override must set at least one rate",
  });

  const afterDelete = (await invoke("usage_pricing_override_delete", {
    normalizedModelId: "gpt-5",
  })) as PriceOverrideView[];
  expect(afterDelete.map((view) => view.normalizedModelId)).not.toContain("gpt-5");
  for (const view of afterDelete) expect(actualKeys(view)).toEqual(overrideKeys);

  // 目录搜索需要 active 快照；两端共享同一份夹具状态。
  mockPricingSetEnabled(true);
  mockPricingRefresh();
  const catalog = (await invoke("usage_pricing_catalog", {
    query: "DEEP",
    limit: 20,
  })) as PricingCatalogEntry[];
  expect(catalog.length).toBeGreaterThan(0);
  const entryKeys = keysOf<PricingCatalogEntry>([
    "providerId",
    "modelId",
    "releaseDate",
    "hasRates",
    "conditional",
    "curatedKind",
  ]);
  for (const entry of catalog) {
    expect(actualKeys(entry)).toEqual(entryKeys);
    expect(typeof entry.hasRates).toBe("boolean");
    expect(typeof entry.conditional).toBe("boolean");
    expect(entry.releaseDate === null || typeof entry.releaseDate === "string").toBe(true);
  }
  expect(catalog.some((entry) => entry.providerId === "ollama-cloud")).toBe(true);
  resetUsageV2MockState();
}

describe("usage 命令双端 mock 形状（同一份断言跑两端）", () => {
  it("src/tauri-mock.ts 在 jsdom 里真的自安装了", () => {
    expect(browserInternals).toBeTruthy();
    expect(typeof browserInternals?.invoke).toBe("function");
  });

  it("未 mock 的命令在两端都返回 undefined/空（对照：证明断言不是恒真）", async () => {
    expect(await mswInvoke("usage_nonexistent_cmd")).toBeUndefined();
    expect(await browserInvoke("usage_nonexistent_cmd")).toBeNull();
  });

  for (const [name, invoke] of [
    ["tests/msw", mswInvoke],
    ["src/tauri-mock", browserInvoke],
  ] as const) {
    describe(`端点 ${name}`, () => {
      it("usage_collect：7 源含 1 失败源，ok 与 errorCode 配对", () => assertUsageCollect(invoke));
      it("usage_dashboard：契约 §2 的 12 字段 + 不可得为 null", () => assertUsageDashboard(invoke));
      it("usage_records：卡片维度 + 卡内行形状", () => assertUsageRecords(invoke));
      it("usage_export_csv：11 列表头", () => assertUsageExportCsv(invoke));
      it("usage_get_settings / usage_set_settings：8 字段 + patch 合并", () =>
        assertUsageSettings(invoke));
      it("Task 6：别名 / 渠道选项 / 覆盖价命令形状与 CRUD 语义", () =>
        assertV2AliasAndOverrideCommands(invoke));
      it("Task 5（2026-10-20）：观察条目 / 模型配置 / 精选目录命令形状与语义", () =>
        assertV2ModelConfigCommands(invoke));
    });
  }

  it("两端样例逐字同形（仅时间戳可不同）", async () => {
    for (const [cmd, args] of [
      ["usage_collect", { force: true }],
      ["usage_collect_visible", undefined],
      ["usage_dashboard", { range: { preset: "today" }, groupBy: "tool" }],
      ["usage_records", { range: { preset: "today" }, groupBy: "project", filters: {} }],
      ["usage_export_csv", today()],
      ["usage_get_settings", undefined],
      ["usage_set_settings", { patch: { detailRetentionDays: 30 } }],
      ["usage_provider_aliases_get", undefined],
      ["usage_channel_options", { range: { preset: "last7d" } }],
      ["usage_pricing_overrides_get", undefined],
      ["usage_observed_entries_get", undefined],
    ] as const) {
      // Task 6 的命令带可变状态：两端各从同一复位态取样，深比较才有意义。
      resetUsageV2MockState();
      const a = await mswInvoke(cmd as string, args as Record<string, unknown> | undefined);
      resetUsageV2MockState();
      const b = await browserInvoke(cmd as string, args as Record<string, unknown> | undefined);
      resetUsageV2MockState();
      // **Minor 2**：`toEqual` 忽略值为 `undefined` 的属性 → 两端同步改成 undefined/漏写
      // `title: null` 时也会绿，而真机是**显式 null**。`toStrictEqual` 区分 undefined 与缺键。
      expect(stripTimestamps(b), `${cmd} 两端 mock 形状/样例不一致`).toStrictEqual(
        stripTimestamps(a)
      );
    }
  });

  it("目录搜索样例两端逐字同形（含排序 / limit / 标记）", async () => {
    const sample = async (invoke: Invoke) => {
      resetUsageV2MockState();
      mockPricingSetEnabled(true);
      mockPricingRefresh();
      const result = (await invoke("usage_pricing_catalog", {
        query: "",
        limit: 2,
      })) as PricingCatalogEntry[];
      resetUsageV2MockState();
      return result;
    };
    const a = await sample(mswInvoke);
    const b = await sample(browserInvoke);
    expect(a).toStrictEqual(b);
    expect(a.map((entry) => `${entry.providerId}/${entry.modelId}`)).toEqual([
      "ollama-cloud/deepseek-v4.1-flash",
      "ollama-cloud/kimi-k2",
    ]);
  });

  it("两个 mock 文件里每条用量命令**各只有一个 case**（switch 重复 case 会静默遮蔽前者）", () => {
    for (const f of ["tests/msw/tauriMocks.ts", "src/tauri-mock.ts"]) {
      const src = readFileSync(path.join(process.cwd(), f), "utf8");
      for (const cmd of USAGE_COMMANDS) {
        const hits = src.split(`case "${cmd}"`).length - 1;
        expect(hits, `${f} 里 ${cmd} 的 case 数必须是 1（重复 case 静默遮蔽）`).toBe(1);
      }
    }
  });
});
