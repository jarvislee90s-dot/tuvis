// 用量配置卡模型条目（2026-10-20 计划 Task 6；2026-10-21 增加三键排序与可交互性标识）。
//
// 展示键 =（显示供应商, 原始模型）；空 provider 未指派条目按（sourceId, 原始模型）单列。
// 同一展示键下多个原始来源折叠为一个条目（别名合并面），编辑对**全部成员**批量写（收敛一致）。
//
// 每条目三行：
//  * row1（只读）：原始供应商 + 原始模型（全部去重证据；空模型显示「未知模型」）；
//  * row2：自定义供应商名（可编辑；对同一原始 provider 全局生效，空 = 回原名）
//    + 自定义模型名（可编辑；同一展示键的多个来源一起改名）+ 复制原始模型名 + 清除模型名；
//  * row3：计价身份（目录 provider/model，仅下拉/搜索选择，无自由输入）；
//    默认显示精选自动匹配徽标，可显式覆盖/清除；「显示全部 N 个来源」切换目录可见范围；
//    悬停/聚焦显示价格表（四价 + 发布日 + 条件价 + 快照日期，按当前显示币种）。
//  * 排序：按用量（默认降序）/ 原始供应商名 / 原始模型名，同一键再点切换升降序。
//
// 纪律：只改展示与估价口径，绝不回写账本；只消费既有 i18n 键。
import { useEffect, useState, type ReactNode } from "react";
import { useQueryClient } from "@tanstack/react-query";
import { ArrowDown, ArrowUp } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { ConvertedPrice } from "@/components/usage/ConvertedPrice";
import { UsageInfoTip } from "@/components/usage/UsageInfoTip";
import { usageErrMsg } from "@/components/usage/usageErrors";
import { usageProviderAliasDelete, usageProviderAliasUpsert } from "@/lib/api/usageV2";
import { EM_DASH, fmtDateTime } from "@/lib/usage/format";
import { formatNativeMoney, useDisplayCurrency } from "@/lib/usage/displayCurrency";
import {
  USAGE_KEY,
  useUsageModelDisplayNameSet,
  useUsagePricingAutoMatchQuery,
  useUsagePricingCatalogSearchQuery,
  useUsagePricingIdentitySet,
  useUsagePricingQuoteQuery,
  useUsageProviderAliasesQuery,
} from "@/lib/query/queries/usageV2";
import { cn } from "@/lib/utils";
import type { TFn } from "@/lib/usage/range";
import type { ProviderAliasRule } from "@/types/usageV2";
import type { UsageSourceId } from "@/types/usage";
import type {
  ObservedModelEntry,
  PriceOverrideView,
  PricingQuotePreview,
  PricingReference,
} from "@/types/usageV2";

/** 精选直营官方来源（与后端 `CURATED_OFFICIAL_PROVIDERS` 一致，仅用于徽标注源） */
const CURATED_OFFICIAL = new Set([
  "openai",
  "anthropic",
  "google",
  "deepseek",
  "zhipuai",
  "moonshotai",
  "moonshotai-cn",
  "alibaba",
  "alibaba-cn",
  "minimax-cn",
  "xai",
  "mistral",
  "meta",
]);
const CURATED_OPENROUTER = "openrouter";

/** 默认只展示最近 7 天用量前 10 条（2026-10-20 用户裁决）。 */
const TOP_ROWS = 10;

/** testid 键卫生化 */
function sanitize(part: string): string {
  return part.replace(/[^a-zA-Z0-9._-]/g, "-");
}

/** 无损键段（长度前缀）：`gpt-4.1` 与 `gpt-4-1` 等卫生化同形串不假合并 */
function safePart(part: string): string {
  return `${part.length}:${part}`;
}

/** B 段条目（展示键折叠后的成员组） */
interface ModelEntryGroup {
  /** 无损折叠键（长度前缀编码）：折叠与草稿键都走它，绝不合并不同展示键 */
  key: string;
  /** testid 键（卫生化，仅用于 DOM 定位）：有损，极端展示键可能撞名，不作折叠依据 */
  testId: string;
  /** null = 空 provider 未指派（成员共享同一 sourceId） */
  displayProvider: string | null;
  modelId: string;
  members: ObservedModelEntry[];
  /** 最近 7 天 requestTotal + output（成员和），用于默认 Top10 */
  recentTokens: number;
}

/** 观测条目 → B 段条目（展示键折叠；同键多成员保序） */
export function modelGroupsOf(entries: readonly ObservedModelEntry[]): ModelEntryGroup[] {
  const byKey = new Map<string, ModelEntryGroup>();
  for (const entry of entries) {
    const sourcePart = entry.displayProvider === null ? (entry.sourceId ?? "") : null;
    const key =
      sourcePart === null
        ? `d-${safePart(entry.displayProvider ?? "")}-${safePart(entry.modelId)}`
        : `u-${safePart(sourcePart)}-${safePart(entry.modelId)}`;
    const testId =
      sourcePart === null
        ? `d-${sanitize(entry.displayProvider ?? "")}-${entry.modelId === "" ? "unknown" : sanitize(entry.modelId)}`
        : `u-${sanitize(sourcePart)}-${entry.modelId === "" ? "unknown" : sanitize(entry.modelId)}`;
    const group = byKey.get(key) ?? {
      key,
      testId,
      displayProvider: entry.displayProvider,
      modelId: entry.modelId,
      members: [],
      recentTokens: 0,
    };
    group.members.push(entry);
    group.recentTokens += entry.recentTokens7d ?? 0;
    byKey.set(key, group);
  }
  return [...byKey.values()].sort((a, b) => {
    if (a.recentTokens !== b.recentTokens) return b.recentTokens - a.recentTokens;
    const providerA = a.displayProvider ?? "\uffff";
    const providerB = b.displayProvider ?? "\uffff";
    return providerA === providerB
      ? a.modelId.localeCompare(b.modelId)
      : providerA.localeCompare(providerB);
  });
}

/** row1 原始证据文本：全部去重原始 provider（空 provider 条目补工具）+ 原始模型 */
function rawEvidenceOf(group: ModelEntryGroup, unknownModelLabel: string): string {
  const providers = [
    ...new Set(group.members.map((m) => m.providerRaw).filter((p): p is string => p !== null)),
  ];
  const tools = [
    ...new Set(group.members.map((m) => m.sourceId).filter((s): s is UsageSourceId => s !== null)),
  ];
  const modelText = group.modelId === "" ? unknownModelLabel : group.modelId;
  const head = providers.length > 0 ? providers.join(", ") : tools.join(", ");
  return `${head} / ${modelText}`;
}

/** 悬停价格表浮层（CSS hover/focus 显隐；键盘经触发器 focus 可达） */
function PricePreview(props: {
  id: string;
  quote: PricingQuotePreview | null;
  t: TFn;
  currency: "usd" | "cny";
}) {
  const { id, quote, t, currency } = props;
  const hint = t("usage.cost.convertedPriceHint");
  const cell = (usd: string | null, cny: string | null) =>
    formatNativeMoney(currency === "cny" ? cny : usd, currency) ?? EM_DASH;
  const rowOf = (labelKey: string, usd: string | null, cny: string | null) => (
    <div key={labelKey} className="flex justify-between gap-3">
      <span>{t(labelKey)}</span>
      <span className="text-right tabular-nums">
        <ConvertedPrice
          converted={currency === "cny" && (quote?.cnyConverted ?? false)}
          hint={hint}
        >
          {cell(usd, cny)}
        </ConvertedPrice>
      </span>
    </div>
  );
  const textRowOf = (labelKey: string, text: string) => (
    <div key={labelKey} className="flex justify-between gap-3">
      <span>{t(labelKey)}</span>
      <span className="text-right tabular-nums">{text}</span>
    </div>
  );
  return (
    <div
      id={id}
      role="tooltip"
      data-testid={id}
      className="bg-background pointer-events-none absolute top-full left-0 z-50 mt-1 w-72 space-y-1 rounded-md border p-2 text-xs opacity-0 shadow-md group-focus-within:opacity-100 group-hover:opacity-100"
    >
      {rowOf(
        "usage.config.modelSection.pricePreview.input",
        quote?.input ?? null,
        quote?.inputCny ?? null
      )}
      {rowOf(
        "usage.config.modelSection.pricePreview.cacheRead",
        quote?.cacheRead ?? null,
        quote?.cacheReadCny ?? null
      )}
      {rowOf(
        "usage.config.modelSection.pricePreview.cacheWrite",
        quote?.cacheWrite ?? null,
        quote?.cacheWriteCny ?? null
      )}
      {rowOf(
        "usage.config.modelSection.pricePreview.output",
        quote?.output ?? null,
        quote?.outputCny ?? null
      )}
      {textRowOf(
        "usage.config.modelSection.pricePreview.releaseDate",
        quote?.releaseDate ?? EM_DASH
      )}
      {quote?.conditional ? (
        <p className="text-amber-600 dark:text-amber-400">
          {t("usage.config.modelSection.pricePreview.conditional")}
        </p>
      ) : null}
      {textRowOf(
        "usage.config.modelSection.pricePreview.snapshotDate",
        quote?.snapshotDate == null ? EM_DASH : fmtDateTime(quote.snapshotDate)
      )}
    </div>
  );
}

/** 计价身份单元格（row3）：徽标 + 选择器 + 价格表 */
function PricingIdentityCell(props: {
  t: TFn;
  /** DOM 定位键（卫生化）；折叠/草稿一律走调用方的无损键，与本键无关 */
  entryTestId: string;
  modelId: string;
  /** 已保存的计价身份覆盖（成对；null = 走自动匹配） */
  override: PricingReference | null;
  busy: boolean;
  onSet(reference: PricingReference | null): void;
}) {
  const { t, entryTestId, modelId, override, busy, onSet } = props;
  const { currency, rate } = useDisplayCurrency();
  // 空模型门控由 hook 自带（trim 后为空不发查询），调用方不再重复传 enabled
  const autoMatch = useUsagePricingAutoMatchQuery(modelId, rate);
  const overrideQuote = useUsagePricingQuoteQuery(
    override?.providerId ?? null,
    override?.modelId ?? null,
    rate
  );
  const [pickerOpen, setPickerOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [scope, setScope] = useState<"curated" | "all">("curated");
  /** 最近一次目录搜索的来源数（切 scope 重取时开关不闪烁；经状态持有，渲染期不读 ref） */
  const [lastCount, setLastCount] = useState<number | null>(null);
  const search = useUsagePricingCatalogSearchQuery(query, scope, { enabled: pickerOpen });
  const providerCount = search.data?.providerCount ?? lastCount;
  useEffect(() => {
    if (search.data) setLastCount(search.data.providerCount);
  }, [search.data]);

  const previewId = `usage-config-price-preview-${entryTestId}`;
  const quote = override ? (overrideQuote.data ?? null) : (autoMatch.data?.quote ?? null);

  let badge: ReactNode;
  if (override) {
    badge = (
      <>
        <span className="group relative inline-flex">
          <button
            type="button"
            data-testid={`usage-config-pricing-badge-${entryTestId}`}
            aria-describedby={previewId}
            className="rounded-full border border-amber-500/50 bg-amber-500/10 px-1.5 py-px text-[11px] text-amber-700 dark:text-amber-300"
          >
            {t("usage.config.modelSection.pricingOverridden")}
          </button>
          <PricePreview id={previewId} quote={quote} t={t} currency={currency} />
        </span>
        <span
          data-testid={`usage-config-pricing-ref-${entryTestId}`}
          className="font-mono text-xs text-amber-700 dark:text-amber-300"
        >
          {override.providerId} / {override.modelId}
        </span>
        <Button
          size="xs"
          variant="outline"
          data-testid={`usage-config-pricing-clear-${entryTestId}`}
          disabled={busy}
          // 与 copy/clear 同因：防止 blur 保存抢占 busy 后 click 被吞
          onMouseDown={(event) => event.preventDefault()}
          onClick={() => onSet(null)}
        >
          {t("usage.config.modelSection.clearOverride")}
        </Button>
      </>
    );
  } else if (autoMatch.data && autoMatch.data.kind !== "none" && autoMatch.data.reference) {
    const providerId = autoMatch.data.reference.providerId;
    let sourceLabel = t("usage.config.modelSection.sources.other");
    if (providerId === CURATED_OPENROUTER) {
      sourceLabel = t("usage.config.modelSection.sources.openrouter");
    } else if (CURATED_OFFICIAL.has(providerId)) {
      sourceLabel = t("usage.config.modelSection.sources.official");
    }
    const prefix =
      autoMatch.data.kind === "prefix" ? ` · ${t("usage.config.modelSection.prefixMatched")}` : "";
    badge = (
      <span className="group relative inline-flex">
        <button
          type="button"
          data-testid={`usage-config-pricing-badge-${entryTestId}`}
          aria-describedby={previewId}
          className="rounded-full border px-1.5 py-px text-[11px]"
        >
          {`${t("usage.config.modelSection.pricingAutoMatched")} · ${sourceLabel}${prefix}`}
        </button>
        <PricePreview id={previewId} quote={quote} t={t} currency={currency} />
      </span>
    );
  } else if (modelId.trim() === "") {
    badge = <span className="text-muted-foreground text-xs">{EM_DASH}</span>;
  } else if (autoMatch.data?.kind === "none") {
    badge = (
      <span
        className="text-muted-foreground text-xs"
        data-testid={`usage-config-pricing-none-${entryTestId}`}
      >
        {t("usage.config.modelSection.pricingNone")}
      </span>
    );
  } else {
    badge = <span className="text-muted-foreground text-xs">{EM_DASH}</span>;
  }

  const selectReference = (reference: PricingReference) => {
    onSet(reference);
    setPickerOpen(false);
    setQuery("");
  };

  return (
    <span className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1">
      {badge}
      <span className="flex min-w-0 items-center gap-2">
        <Button
          size="xs"
          variant="outline"
          data-testid={`usage-config-pricing-toggle-${entryTestId}`}
          aria-expanded={pickerOpen}
          disabled={busy}
          // 防止 mousedown 先触发模型名输入的 blur 保存，导致 busy 重渲染后 click 被吞
          onMouseDown={(event) => event.preventDefault()}
          onClick={() => setPickerOpen((v) => !v)}
        >
          {t("usage.config.modelSection.pricingIdentity")}
        </Button>
        {pickerOpen ? (
          <span className="flex min-w-0 flex-1 flex-col gap-1">
            <Input
              data-testid={`usage-config-pricing-search-${entryTestId}`}
              role="combobox"
              aria-expanded={pickerOpen}
              aria-autocomplete="list"
              aria-label={t("usage.billing.pricingCatalogSearch")}
              placeholder={t("usage.billing.pricingCatalogSearch")}
              className="h-8"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
            />
            {/* 目录可见范围开关：N = 全量目录 provider 数；只影响搜索可见范围，不改变自动匹配 */}
            {providerCount !== null ? (
              <button
                type="button"
                data-testid={`usage-config-pricing-scope-${entryTestId}`}
                aria-pressed={scope === "all"}
                className="text-muted-foreground w-fit rounded border px-1.5 py-px text-[11px]"
                onClick={() => setScope((s) => (s === "curated" ? "all" : "curated"))}
              >
                {t("usage.config.modelSection.showAllSources", { count: providerCount })}
              </button>
            ) : (
              <span
                data-testid={`usage-config-pricing-scope-${entryTestId}`}
                className="text-muted-foreground text-[11px]"
              >
                {t("usage.config.modelSection.curatedOnly")}
              </span>
            )}
            {search.isError ? (
              <p role="alert" className="text-destructive text-[11px]">
                {usageErrMsg(search.error, t)}
              </p>
            ) : !search.data && search.fetchStatus === "idle" ? (
              // 短输入（trim < 2）未启用查询：提示输入而不是永远停在「加载中…」
              <span
                data-testid={`usage-config-pricing-type-hint-${entryTestId}`}
                className="text-muted-foreground text-[11px]"
              >
                {t("usage.billing.pricingCatalogHint")}
              </span>
            ) : search.isPending ? (
              <span className="text-muted-foreground text-[11px]">{t("common.loading")}</span>
            ) : (search.data?.entries.length ?? 0) === 0 ? (
              <span className="text-muted-foreground text-[11px]">
                {t("usage.config.modelSection.noMatch")}
              </span>
            ) : (
              <ul
                role="listbox"
                aria-label={t("usage.config.modelSection.pricingIdentity")}
                className="max-h-40 space-y-0.5 overflow-y-auto rounded-md border p-1"
              >
                {(search.data?.entries ?? []).map((entry) => (
                  <li
                    key={`${entry.providerId}/${entry.modelId}`}
                    role="option"
                    aria-selected="false"
                  >
                    <button
                      type="button"
                      data-testid={`usage-config-pricing-option-${entry.providerId}/${entry.modelId}`}
                      className="hover:bg-accent w-full truncate rounded border px-1.5 py-1 text-left font-mono text-xs"
                      title={`${entry.providerId} / ${entry.modelId}`}
                      disabled={busy}
                      onClick={() =>
                        selectReference({ providerId: entry.providerId, modelId: entry.modelId })
                      }
                    >
                      {entry.providerId} / {entry.modelId}
                    </button>
                  </li>
                ))}
              </ul>
            )}
          </span>
        ) : null}
      </span>
    </span>
  );
}

export interface UsageConfigModelSectionProps {
  t: TFn;
  entries: readonly ObservedModelEntry[];
  /** 已有的四价覆盖（用于「已应用四价覆盖」徽标 / 快捷入口文案） */
  overrides?: readonly PriceOverrideView[];
  /** 条目快捷入口：展开高级区并预填覆盖表单 */
  onOverride?(modelId: string, normalizedModelId: string): void;
}

export function UsageConfigModelSection({
  t,
  entries,
  overrides = [],
  onOverride,
}: UsageConfigModelSectionProps) {
  const queryClient = useQueryClient();
  const displayNameSet = useUsageModelDisplayNameSet();
  const pricingIdentitySet = useUsagePricingIdentitySet();
  const aliasesQuery = useUsageProviderAliasesQuery();
  const [drafts, setDrafts] = useState<Record<string, string>>({});
  const [providerDrafts, setProviderDrafts] = useState<Record<string, string>>({});
  const [expanded, setExpanded] = useState(false);
  const [filter, setFilter] = useState("");
  const [sortKey, setSortKey] = useState<"usage" | "provider" | "model">("usage");
  const [sortDirection, setSortDirection] = useState<"asc" | "desc">("desc");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<unknown>(null);

  const groups = modelGroupsOf(entries);
  const unknownModelLabel = t("usage.label.unknownModel");
  const aliases = aliasesQuery.data?.aliases ?? [];

  const toggleSort = (key: "usage" | "provider" | "model") => {
    if (key === sortKey) {
      setSortDirection((current) => (current === "asc" ? "desc" : "asc"));
      return;
    }
    setSortKey(key);
    setSortDirection(key === "usage" ? "desc" : "asc");
  };

  const rawProviderOf = (group: ModelEntryGroup): string =>
    group.members.find((member) => member.providerRaw !== null)?.providerRaw ?? "";
  const sortedGroups = [...groups].sort((left, right) => {
    const direction = sortDirection === "asc" ? 1 : -1;
    let primary: number;
    if (sortKey === "usage") {
      primary = left.recentTokens - right.recentTokens;
    } else if (sortKey === "provider") {
      primary = rawProviderOf(left).localeCompare(rawProviderOf(right));
    } else {
      primary = left.modelId.localeCompare(right.modelId);
    }
    if (primary !== 0) return primary * direction;
    // 同值时用稳定键兜底，避免排序抖动；用量始终作为第一兜底。
    if (left.recentTokens !== right.recentTokens) return right.recentTokens - left.recentTokens;
    return left.key.localeCompare(right.key);
  });

  const findAlias = (
    predicate: (rule: ProviderAliasRule) => boolean
  ): ProviderAliasRule | undefined => aliases.find(predicate);

  const providerRuleOf = (member: ObservedModelEntry): ProviderAliasRule | undefined =>
    member.providerRaw !== null
      ? findAlias((rule) => rule.kind === "provider" && rule.provider === member.providerRaw)
      : findAlias(
          (rule) =>
            rule.kind === "toolModel" &&
            rule.sourceId === member.sourceId &&
            rule.modelId === member.modelId
        );

  /** 对展示键下全部成员批量写（收敛一致；计划 §3.5） */
  const writeAll = async (
    group: ModelEntryGroup,
    write: (member: ObservedModelEntry) => Promise<unknown>,
    options?: { clearModelDraft?: boolean }
  ) => {
    // 重入守卫：Enter 保存使输入框 disable 而**触发 blur**，blur 再走一次保存路径；
    // busy 中直接返回，避免同一草稿双提交（第二次会撞「条目未观测/键冲突」的假失败）。
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      await Promise.all(group.members.map((member) => write(member)));
      if (options?.clearModelDraft) {
        setDrafts((current) => {
          const next = { ...current };
          delete next[group.key];
          return next;
        });
      }
      await queryClient.invalidateQueries({ queryKey: USAGE_KEY });
    } catch (e) {
      setError(e);
    } finally {
      setBusy(false);
    }
  };

  const saveDisplayName = async (group: ModelEntryGroup, rawValue: string) => {
    const value = rawValue.trim();
    const current =
      group.members.find((m) => m.displayModelName !== null)?.displayModelName ?? null;
    if (value === (current ?? "")) return; // 未变化不提交
    await writeAll(
      group,
      (member) =>
        displayNameSet.mutateAsync({
          providerRaw: member.providerRaw,
          sourceId: member.sourceId,
          modelId: member.modelId,
          displayName: value === "" ? null : value,
        }),
      { clearModelDraft: true }
    );
  };

  /**
   * 供应商名保存：对同一原始 provider 的所有条目全局生效（2026-10-20 用户裁决）。
   * 空 provider 条目写 toolModel 别名；清空 = 删除对应别名（回原名 / 未指定渠道）。
   */
  const saveProviderName = async (group: ModelEntryGroup, rawValue: string) => {
    const value = rawValue.trim();
    const current = group.displayProvider ?? "";
    if (value === current) return;
    if (busy) return;
    setBusy(true);
    setError(null);
    try {
      // 同一原始 provider / 同一 toolModel 键在一个展示键下可能有多条成员；
      // 去重后逐条写，避免对同一别名键二次 upsert 撞唯一索引。
      const targets = new Map<string, ObservedModelEntry>();
      for (const member of group.members) {
        const key =
          member.providerRaw !== null
            ? `p:${member.providerRaw}`
            : `tm:${member.sourceId ?? ""}:${member.modelId}`;
        if (!targets.has(key)) targets.set(key, member);
      }
      for (const member of targets.values()) {
        const rule = providerRuleOf(member);
        if (value === "") {
          if (rule) await usageProviderAliasDelete(rule.id);
          continue;
        }
        await usageProviderAliasUpsert({
          id: rule?.id ?? crypto.randomUUID(),
          kind: member.providerRaw !== null ? "provider" : "toolModel",
          provider: member.providerRaw,
          sourceId: member.providerRaw === null ? member.sourceId : null,
          modelId: member.providerRaw === null ? member.modelId : null,
          displayName: value,
        });
      }
      setProviderDrafts((currentDrafts) => {
        const next = { ...currentDrafts };
        delete next[group.key];
        return next;
      });
      await queryClient.invalidateQueries({ queryKey: USAGE_KEY });
    } catch (e) {
      setError(e);
    } finally {
      setBusy(false);
    }
  };

  const needle = filter.trim().toLowerCase();
  const filtered = needle
    ? sortedGroups.filter((group) => {
        const evidence = rawEvidenceOf(group, unknownModelLabel).toLowerCase();
        return (
          (group.displayProvider ?? "").toLowerCase().includes(needle) ||
          group.modelId.toLowerCase().includes(needle) ||
          (group.members.find((member) => member.displayModelName !== null)?.displayModelName ?? "")
            .toLowerCase()
            .includes(needle) ||
          evidence.includes(needle)
        );
      })
    : sortedGroups;
  const visible = expanded || needle !== "" ? filtered : filtered.slice(0, TOP_ROWS);
  const hiddenCount = Math.max(0, groups.length - TOP_ROWS);

  return (
    <section className="space-y-2" data-testid="usage-config-model-section">
      <div className="flex flex-wrap items-baseline justify-between gap-2">
        <div className="flex items-center gap-1">
          <h3 className="text-sm font-medium">{t("usage.config.modelSection.title")}</h3>
          <UsageInfoTip
            label={t("usage.config.help.modelEntriesLabel")}
            testId="usage-config-model-section-info"
          >
            {t("usage.config.help.modelEntries")}
          </UsageInfoTip>
        </div>
        <span className="text-muted-foreground text-xs">{t("usage.config.modelSection.desc")}</span>
      </div>

      <div className="bg-muted/30 flex flex-wrap items-center justify-between gap-2 rounded-md border px-2 py-1.5">
        <div className="text-muted-foreground flex flex-wrap items-center gap-2 text-[11px]">
          <span>{t("usage.config.modelSection.legend")}</span>
          <span className="rounded border border-transparent px-1">
            {t("usage.config.modelSection.legendReadonly")}
          </span>
          <span className="bg-background rounded border px-1">
            {t("usage.config.modelSection.legendEditable")}
          </span>
          <span className="rounded border border-amber-500/40 bg-amber-500/10 px-1 text-amber-700 dark:text-amber-300">
            {t("usage.config.modelSection.legendModified")}
          </span>
        </div>
        <div className="flex flex-wrap items-center gap-1">
          <span className="text-muted-foreground text-[11px]">
            {t("usage.config.modelSection.sortBy")}
          </span>
          {(
            [
              ["usage", "sortUsage"],
              ["provider", "sortProvider"],
              ["model", "sortModel"],
            ] as const
          ).map(([key, labelKey]) => {
            const active = sortKey === key;
            const direction = active && sortDirection === "asc" ? "asc" : "desc";
            return (
              <Button
                key={key}
                type="button"
                size="xs"
                variant={active ? "default" : "outline"}
                aria-pressed={active}
                aria-label={`${t(`usage.config.modelSection.${labelKey}`)} ${
                  direction === "asc"
                    ? t("usage.config.modelSection.sortAsc")
                    : t("usage.config.modelSection.sortDesc")
                }`}
                data-testid={`usage-config-sort-${key}`}
                onClick={() => toggleSort(key)}
              >
                {t(`usage.config.modelSection.${labelKey}`)}
                {active ? (
                  sortDirection === "asc" ? (
                    <ArrowUp className="h-3 w-3" aria-hidden="true" />
                  ) : (
                    <ArrowDown className="h-3 w-3" aria-hidden="true" />
                  )
                ) : null}
              </Button>
            );
          })}
        </div>
      </div>

      {error ? (
        <p data-testid="usage-config-model-error" className="text-destructive text-sm" role="alert">
          {usageErrMsg(error, t)}
        </p>
      ) : null}

      {aliasesQuery.isPending ? (
        <p className="text-muted-foreground text-xs">{t("common.loading")}</p>
      ) : aliasesQuery.isError ? (
        <div className="space-y-1" role="alert" data-testid="usage-config-alias-error">
          <p className="text-destructive text-xs">{usageErrMsg(aliasesQuery.error, t)}</p>
          <Button size="sm" variant="outline" onClick={() => void aliasesQuery.refetch()}>
            {t("common.retry")}
          </Button>
        </div>
      ) : groups.length === 0 ? (
        <p className="text-muted-foreground text-xs">{t("usage.config.modelSection.empty")}</p>
      ) : (
        <>
          {expanded ? (
            <Input
              data-testid="usage-config-model-search"
              aria-label={t("usage.config.modelSection.searchPlaceholder")}
              placeholder={t("usage.config.modelSection.searchPlaceholder")}
              className="h-8"
              value={filter}
              onChange={(e) => setFilter(e.target.value)}
            />
          ) : null}
          {visible.length === 0 ? (
            <p
              data-testid="usage-config-model-search-empty"
              className="text-muted-foreground text-xs"
            >
              {t("usage.config.modelSection.noMatch")}
            </p>
          ) : (
            <ul className="space-y-2">
              {visible.map((group) => {
                const evidence = rawEvidenceOf(group, unknownModelLabel);
                const currentName =
                  group.members.find((m) => m.displayModelName !== null)?.displayModelName ?? null;
                const overridePair = group.members.find((m) => m.pricingProvider !== null);
                const override =
                  overridePair &&
                  overridePair.pricingProvider !== null &&
                  overridePair.pricingModel !== null
                    ? {
                        providerId: overridePair.pricingProvider,
                        modelId: overridePair.pricingModel,
                      }
                    : null;
                const value = drafts[group.key] ?? currentName ?? "";
                const normalizedModelId = group.members[0]?.normalizedModelId ?? "";
                const hasOverride =
                  normalizedModelId !== "" &&
                  overrides.some((view) => view.normalizedModelId === normalizedModelId);
                const providerValue = providerDrafts[group.key] ?? group.displayProvider ?? "";
                const providerPlaceholder =
                  group.members.find((m) => m.providerRaw !== null)?.providerRaw ??
                  t("usage.label.unassignedChannel");
                const rawProvider =
                  group.members.find((m) => m.providerRaw !== null)?.providerRaw ?? null;
                const providerModified =
                  group.displayProvider !== null &&
                  (rawProvider === null || group.displayProvider !== rawProvider);
                const modelModified = currentName !== null && currentName !== group.modelId;
                return (
                  <li
                    key={group.key}
                    data-testid={`usage-config-model-entry-${group.testId}`}
                    className="space-y-1.5 rounded-md border p-2"
                  >
                    {/* row1：原始身份（只读证据面） */}
                    <div className="flex min-w-0 items-center gap-2 text-xs">
                      <span className="text-muted-foreground shrink-0">
                        {t("usage.config.modelSection.rawIdentity")}:
                      </span>
                      <span
                        data-testid={`usage-config-model-raw-${group.testId}`}
                        className="min-w-0 truncate font-mono"
                        title={evidence}
                      >
                        {evidence}
                      </span>
                    </div>
                    {/* row2：自定义供应商名 + 自定义模型名（两者都可就地编辑） */}
                    <div className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 text-xs">
                      <span className="text-muted-foreground shrink-0">
                        {t("usage.config.modelSection.providerName")}:
                      </span>
                      <Input
                        data-testid={`usage-config-model-provider-input-${group.testId}`}
                        aria-label={t("usage.config.modelSection.providerName")}
                        placeholder={providerPlaceholder}
                        className={cn(
                          "h-8 w-40 min-w-0",
                          providerModified &&
                            "border-amber-500/60 text-amber-700 dark:text-amber-300"
                        )}
                        disabled={busy}
                        value={providerValue}
                        onChange={(e) =>
                          setProviderDrafts((current) => ({
                            ...current,
                            [group.key]: e.target.value,
                          }))
                        }
                        onBlur={(e) => void saveProviderName(group, e.target.value)}
                        onKeyDown={(e) => {
                          if (e.key === "Enter") {
                            e.preventDefault();
                            void saveProviderName(group, (e.target as HTMLInputElement).value);
                          }
                        }}
                      />
                      <span className="text-muted-foreground shrink-0">
                        {t("usage.config.modelSection.modelName")}:
                      </span>
                      <Input
                        data-testid={`usage-config-model-display-name-input-${group.testId}`}
                        aria-label={t("usage.config.modelSection.modelName")}
                        placeholder={group.modelId || unknownModelLabel}
                        className={cn(
                          "h-8 min-w-0 flex-1",
                          modelModified && "border-amber-500/60 text-amber-700 dark:text-amber-300"
                        )}
                        disabled={busy}
                        value={value}
                        onChange={(e) =>
                          setDrafts((current) => ({ ...current, [group.key]: e.target.value }))
                        }
                        onBlur={(e) => void saveDisplayName(group, e.target.value)}
                        onKeyDown={(e) => {
                          if (e.key === "Enter") {
                            e.preventDefault();
                            void saveDisplayName(group, (e.target as HTMLInputElement).value);
                          }
                        }}
                      />
                      <Button
                        size="xs"
                        variant="ghost"
                        data-testid={`usage-config-model-copy-${group.testId}`}
                        disabled={busy || group.modelId === ""}
                        title={t("usage.config.modelSection.copyRaw")}
                        // 防止 mousedown 先触发模型名输入的 blur 保存，导致 busy 重渲染后 click 被吞
                        onMouseDown={(event) => event.preventDefault()}
                        onClick={() => {
                          if (group.modelId === "") return;
                          setDrafts((current) => ({ ...current, [group.key]: group.modelId }));
                          void writeAll(
                            group,
                            (member) =>
                              displayNameSet.mutateAsync({
                                providerRaw: member.providerRaw,
                                sourceId: member.sourceId,
                                modelId: member.modelId,
                                displayName: group.modelId,
                              }),
                            { clearModelDraft: true }
                          );
                        }}
                      >
                        {t("usage.config.modelSection.copyRaw")}
                      </Button>
                      <Button
                        size="xs"
                        variant="ghost"
                        data-testid={`usage-config-model-clear-${group.testId}`}
                        disabled={busy}
                        // 防止 mousedown 先触发模型名输入的 blur 保存，导致 busy 重渲染后 click 被吞
                        onMouseDown={(event) => event.preventDefault()}
                        onClick={() => {
                          setDrafts((current) => ({ ...current, [group.key]: "" }));
                          void writeAll(
                            group,
                            (member) =>
                              displayNameSet.mutateAsync({
                                providerRaw: member.providerRaw,
                                sourceId: member.sourceId,
                                modelId: member.modelId,
                                displayName: null,
                              }),
                            { clearModelDraft: true }
                          );
                        }}
                      >
                        {t("usage.config.modelSection.clearOverride")}
                      </Button>
                    </div>
                    {/* row2 改名范围提示：供应商名全局生效；模型名按展示键批量 */}
                    <p
                      data-testid={`usage-config-model-display-name-hint-${group.testId}`}
                      className="text-muted-foreground text-[11px]"
                    >
                      {t("usage.config.modelSection.displayNameHint")}
                    </p>
                    {/* row3：计价身份（仅目录选择） + 四价覆盖快捷入口（另一层） */}
                    <div className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 text-xs">
                      <PricingIdentityCell
                        t={t}
                        entryTestId={group.testId}
                        modelId={group.modelId}
                        override={override}
                        busy={busy}
                        onSet={(reference) => {
                          void writeAll(group, (member) =>
                            pricingIdentitySet.mutateAsync({
                              providerRaw: member.providerRaw,
                              sourceId: member.sourceId,
                              modelId: member.modelId,
                              reference,
                            })
                          );
                        }}
                      />
                      <UsageInfoTip
                        label={t("usage.config.help.pricingIdentityLabel")}
                        testId={`usage-config-pricing-info-${group.testId}`}
                      >
                        {t("usage.config.help.pricingIdentity")}
                      </UsageInfoTip>
                      {onOverride ? (
                        <>
                          {hasOverride ? (
                            <span
                              data-testid={`usage-config-model-overridden-${group.testId}`}
                              className="rounded-full border px-1.5 py-px text-[11px]"
                              title={t("usage.config.help.priceBadge")}
                            >
                              {t("usage.config.modelSection.priceOverriddenBadge")}
                            </span>
                          ) : null}
                          <Button
                            size="xs"
                            variant="outline"
                            data-testid={`usage-config-model-override-${group.testId}`}
                            disabled={busy || normalizedModelId === ""}
                            onClick={() => onOverride(group.modelId, normalizedModelId)}
                          >
                            {hasOverride
                              ? t("usage.config.modelSection.editOverridePrice")
                              : t("usage.config.modelSection.overridePrice")}
                          </Button>
                        </>
                      ) : null}
                    </div>
                  </li>
                );
              })}
            </ul>
          )}
          {hiddenCount > 0 || expanded ? (
            <Button
              size="sm"
              variant="ghost"
              data-testid="usage-config-model-more"
              aria-expanded={expanded}
              onClick={() => {
                if (expanded) {
                  setFilter("");
                  setExpanded(false);
                } else {
                  setExpanded(true);
                }
              }}
            >
              {expanded
                ? t("usage.config.modelSection.collapse")
                : t("usage.config.modelSection.expandAll", { count: hiddenCount })}
            </Button>
          ) : null}
        </>
      )}
    </section>
  );
}
