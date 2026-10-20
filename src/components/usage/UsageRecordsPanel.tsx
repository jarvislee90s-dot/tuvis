// 用量记录页（计划② Task 9 步骤 3/4；spec P3 · D6/D7/D21）——每「工具 / 项目」一卡、卡内行**恒为**
// 「供应商 / 模型」+ 三态来源徽标；筛选 = 工具（D1 的 7 源）+ 子代理模式。消费方：大看板窗口的
// 「记录」页签（`src/pages/usage-dashboard.tsx`）。
//
// 七条纪律：
//  ① **本面板自带三态并自持查询**（加载 / 错误可重试 / 空态）：页面传 `range` / `retentionDays` /
//     `t` 与**受控的筛选状态**。读命令只读账本、绝不触发扫描（§3 第 15 条）；设置也只能经
//     `useUsageSettingsQuery`（页面持有），本文件**不**直连设置命令。
//  ② 行维度**不随卡片分组变**（D6）：`cardDim`（`tool` | `project`，默认 `tool`）只喂
//     `usageRecords` 的 `groupBy`；2026-10-21 起行名经 `displayLabelOf` **供应商别名优先**
//     （`别名 / 原始模型`；无别名回退真机 `row.label` 原文，供应商不可得时只有模型名），
//     原始 label 始终保留在 `title` 与行数据中（spec §7.5）。
//  ③ 卡片标题按 `groupBy` 分派（§3 第 13 条）：`tool → usageAgentLabel(card.toolId)`（真机在这个
//     维度给的 `toolLabel` 是**小写工具 id**）、`project → card.toolLabel` **原文**（D21：key 小写、
//     label 原样，不做 realpath / 不做大小写规范化 / 不用 `projectPath`）。
//  ④ 值口径 = hero（`metrics.requestTotal + buckets.output`），唯一出处是 `distribution.ts` 的
//     `distValue`；只显示 `fmtTokens` 万/亿缩写，不挂完整千分位原始值 tooltip（2026-10-19 裁决）。
//  ⑤ **日档守卫三层**（契约 §3 要点 4 / 第 9 码，预防优于报错）：日档（非 `last5h` / `today`）下
//     开关 `disabled` + **常驻**原因（第①层，本文件）；**复位（②）与 `filters` 双保险（③）已上提到
//     `src/lib/usage/useRecordsFilters.ts`** —— 2026-10-06 用户裁决「CSV 要接收记录页筛选」之后，
//     筛选状态必须与导出行**同一个出处**，否则屏幕与导出必然漂移。三层都还在，只是不再全挤在本文件。
//  ⑥ 空态**出文案不出 0**（§3 第 5 条 / spec P8）：无卡片 → 「暂无数据」，不留一行 0。
//     **「没量到 token」的卡**同样不许印 0（2026-10-06 用户裁决「没产生数值就不显示数值」）：
//     判据是 `range.ts::cardTokenUnmeasured`（**逐卡 / 逐行**，不吃窗口级判据——某工具只落真空回合行
//     的窗口里逐卡判据才拦得住），为真 ⇒ 该卡合计与该行值出 `EM_DASH`；**计数类（`requests` /
//     `userEst`）不动**（它们是量出来的真值）。CSV 是**例外**：那是数据文件，保留原始 0
//     （理由与落点见 `UsageExportActions` 的 CSV 分支注释）。
//  ⑦ 取色只认主题 token、按钮 / 卡片走 `ui/{button,card}` 原语（§3 第 19/40 条）；`TFn` /
//     `usageAgentLabel` / `EM_DASH` 一律 import，**不重声明**（§3 第 22/13 条）。本面板**不轮询**：
//     新数据由采集成功后的 `["usage"]` 前缀失效驱动（故此处不出现任何轮询标记，见 ① 的源码锁纪律）。
import { useMemo, useState, type ReactNode } from "react";
import { Button } from "@/components/ui/button";
import { Card, CardAction, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Switch } from "@/components/ui/switch";
import {
  BillingAssignmentDialog,
  identityDisplayName,
} from "@/components/usage/BillingAssignmentDialog";
import { BillingProfilesManager } from "@/components/usage/BillingProfilesManager";
import { ProviderAliasDialog } from "@/components/usage/ProviderAliasDialog";
import { displayIdentityLabel, findAliasForIdentity } from "@/components/usage/ProviderAliasFields";
import { UsageEmpty, UsageError, UsageLoading } from "@/components/usage/UsageStates";
import { usageErrMsg } from "@/components/usage/usageErrors";
import { useUsageRecordsQuery } from "@/lib/query/queries/usage";
import { useUsageIdentitiesQuery } from "@/lib/query/queries/usageV2";
import { useUsageProviderAliasesQuery } from "@/lib/query/queries/usageV2";
import { distValue } from "@/lib/usage/distribution";
import { EM_DASH, fmtTokens, usageAgentLabel } from "@/lib/usage/format";
import { getLang } from "@/lib/usage/lang";
import { cardTokenUnmeasured } from "@/lib/usage/range";
import { FILTER_TOOL_IDS, RECORD_DIMS } from "@/lib/usage/useRecordsFilters";
import type { RecordsFiltersState } from "@/lib/usage/useRecordsFilters";
import type { TFn } from "@/lib/usage/range";
import type {
  UsageCard,
  UsageFilters,
  UsageRange,
  UsageRecordsGroupBy,
  UsageRow,
  UsageSourceKind,
} from "@/types/usage";
import type { IdentityUsage } from "@/types/usageV2";

/**
 * 卡内行上限：超出折「展开其余 N 行」（spec P3「按组合聚合后行数可控，前端分页即可」）。
 * claude 卡在夹具里刻意给 15 行，用于覆盖折叠分支。
 */
export const RECORD_ROW_CAP = 12;

/**
 * 来源三态徽标的取色（§3 第 19 条：主题 token / Tailwind 语义类，浅深各一套；不得硬编码参考实现色值）。
 * 三态在浅色与深色档下都两两可区分：实测（绿）/ 推断（琥珀）/ 未知（中性）。
 */
const SOURCE_BADGE_CLASS: Record<UsageSourceKind, string> = {
  measured:
    "border-emerald-600/40 text-emerald-700 dark:border-emerald-400/40 dark:text-emerald-300",
  inferred: "border-amber-600/40 text-amber-700 dark:border-amber-400/40 dark:text-amber-300",
  unknown:
    "border-muted-foreground/30 text-muted-foreground dark:border-muted-foreground/50 dark:text-muted-foreground/80",
};

/**
 * 供应商归因三态徽标（spec §4.3「供应商归因三态 … UI 必须可见地区分」）。
 * **本文件私有**（控制器裁决：全计划只有记录页消费它）——不导出、不新建文件、不放进 ① 的错误码模块。
 */
function SourceBadge({ kind, t }: { kind: UsageSourceKind; t: TFn }) {
  return (
    <span
      data-testid={`usage-record-source-${kind}`}
      className={`shrink-0 rounded-full border px-1.5 py-px text-[10px] leading-tight ${SOURCE_BADGE_CLASS[kind]}`}
    >
      {t(`usage.source.${kind}`)}
    </span>
  );
}

/** 行归属状态（由 `usageIdentities` 聚合得出） */
type RowAttribution =
  | { kind: "assigned"; profileId: string }
  | { kind: "observed"; profileId: string }
  | { kind: "ambiguous"; profileId: string | null }
  | { kind: "unassigned"; profileId: null };

const UNASSIGNED: RowAttribution = { kind: "unassigned", profileId: null };

/**
 * 保守聚合（project 卡与同 source 多身份共用）：
 *  * 任何 ambiguous / 跨 profile / 已归属与未归属混合 → ambiguous；
 *  * 全部 unknown → unassigned；
 *  * 全部 observed 且同 profile → observed；
 *  * 全部 userAssigned 且同 profile → assigned；
 *  * observed 与 userAssigned 混用 → ambiguous（不猜）。
 */
function aggregateAttribution(identities: readonly IdentityUsage[] | undefined): RowAttribution {
  if (!identities || identities.length === 0) return UNASSIGNED;
  const kinds = identities.map((identity) => identity.attribution.attributionKind);
  const profiles = identities
    .map((identity) => identity.attribution.billingProfileId)
    .filter((id): id is string => id !== null);
  const assignedish = identities.filter(
    (identity) =>
      identity.attribution.billingProfileId !== null &&
      (identity.attribution.attributionKind === "userAssigned" ||
        identity.attribution.attributionKind === "observed")
  );
  const distinctProfiles = new Set(profiles);

  if (kinds.includes("ambiguous")) {
    return { kind: "ambiguous", profileId: profiles[0] ?? null };
  }
  // 全部 unknown
  if (kinds.every((kind) => kind === "unknown")) return UNASSIGNED;
  // 未归属与已归属混合，或部分身份没有 profile ⇒ 含糊
  if (assignedish.length !== identities.length || distinctProfiles.size > 1) {
    return { kind: "ambiguous", profileId: profiles[0] ?? null };
  }
  const kindSet = new Set(kinds);
  if (kindSet.size === 1 && kindSet.has("observed")) {
    return { kind: "observed", profileId: profiles[0] };
  }
  if (kindSet.size === 1 && kindSet.has("userAssigned")) {
    return { kind: "assigned", profileId: profiles[0] };
  }
  // observed + userAssigned 混用
  return { kind: "ambiguous", profileId: profiles[0] ?? null };
}

/** 单张卡片：标题（按卡片维度分派）+ 合计 + 若干「供应商 / 模型」行（超限折叠） */
function RecordCard({
  card,
  cardDim,
  expanded,
  onToggle,
  onAssign,
  onAlias,
  displayLabelOf,
  attributionOf,
  hasAlias,
  t,
}: {
  card: UsageCard;
  cardDim: UsageRecordsGroupBy;
  expanded: boolean;
  onToggle(key: string): void;
  onAssign(card: UsageCard, row: UsageRow): void;
  onAlias(card: UsageCard, row: UsageRow, explicitlyBound: boolean): void;
  /** 行显示名：供应商别名优先，原始「供应商 / 模型」留在 tooltip */
  displayLabelOf(card: UsageCard, row: UsageRow): string;
  attributionOf(card: UsageCard, label: string): RowAttribution;
  hasAlias(card: UsageCard, label: string): boolean;
  t: TFn;
}) {
  // 格式化语言（issue #160）：数值单位（万/亿 vs K/M/B）随界面语言走，组件体取一次。
  const lang = getLang();
  // 标题分派（见文件头 ③）：真机 tool 维度的 `toolLabel` 是工具 id，展示名只由 `usageAgentLabel` 解析
  const title = cardDim === "tool" ? usageAgentLabel(card.toolId) : card.toolLabel;
  // 卡片 testid 用的是**契约 key**：按项目分组时它是小写规范化键（D21），标题才是 label 原文
  const total = distValue(card);
  // 「没量到 token」的卡（见文件头 ⑥）：合计与该卡每行的 token 位出 `EM_DASH`。
  // 逐行各自判（谁被显示就判谁）；与卡片级等价（后端恒等式：卡合计 = Σ 卡内行）。
  const noTokens = cardTokenUnmeasured(card);
  const hidden = card.rows.length - RECORD_ROW_CAP;
  const rows = expanded || hidden <= 0 ? card.rows : card.rows.slice(0, RECORD_ROW_CAP);

  return (
    <Card data-testid={`usage-record-card-${card.toolId}`}>
      <CardHeader>
        <CardTitle>{title}</CardTitle>
        <CardAction>
          <span className="text-muted-foreground text-xs tabular-nums">
            {t("usage.records.cardTotal", { v: noTokens ? EM_DASH : fmtTokens(total, lang) })}
          </span>
        </CardAction>
      </CardHeader>

      <CardContent className="space-y-1.5">
        {rows.map((row, i) => {
          const rowNoTokens = cardTokenUnmeasured(row);
          const attr = attributionOf(card, row.label);
          const shownLabel = displayLabelOf(card, row);
          return (
            <div
              key={row.key}
              data-testid={`usage-record-row-${card.toolId}:${i}`}
              className="flex items-center gap-2 text-sm"
            >
              <SourceBadge kind={row.sourceKind} t={t} />
              {/* 行名 = 供应商别名优先；原始「供应商 / 模型」留在 title 与行数据里，作为证据 */}
              <span className="min-w-0 flex-1 truncate" title={row.label}>
                {shownLabel}
              </span>
              {attr.kind === "assigned" ? (
                <span
                  data-testid="usage-record-attr-assigned"
                  className="border-primary/40 text-primary shrink-0 rounded-full border px-1.5 py-px text-[10px]"
                >
                  {t("usage.records.assigned")}
                </span>
              ) : attr.kind === "observed" ? (
                <span
                  data-testid="usage-record-attr-observed"
                  className="border-border text-foreground shrink-0 rounded-full border px-1.5 py-px text-[10px]"
                >
                  {t("usage.records.observed")}
                </span>
              ) : attr.kind === "ambiguous" ? (
                <span
                  data-testid="usage-record-attr-ambiguous"
                  className="border-muted-foreground/40 text-muted-foreground shrink-0 rounded-full border px-1.5 py-px text-[10px]"
                >
                  {t("usage.records.ambiguous")}
                </span>
              ) : null}

              {/* 只有含糊行需要显式计费归属；普通行的渠道由供应商别名/原始 provider 决定，
                  不再逐行要求「指定渠道」，避免与别名配置重复。 */}
              {attr.kind === "ambiguous" ? (
                <Button
                  size="xs"
                  variant="ghost"
                  data-testid={`usage-record-assign-${card.toolId}:${i}`}
                  onClick={() => onAssign(card, row)}
                >
                  {t("usage.records.assignChannel")}
                </Button>
              ) : null}
              {/* 供应商别名：所有行都可编辑；保存后同一窗口内记录页行名与看板同步 */}
              <Button
                size="xs"
                variant="ghost"
                data-testid={`usage-record-alias-${card.toolId}:${i}`}
                onClick={() => onAlias(card, row, attr.kind === "assigned")}
              >
                {hasAlias(card, row.label) ? t("usage.alias.edit") : t("usage.records.aliasAction")}
              </Button>

              <span className="font-medium tabular-nums">
                {rowNoTokens ? EM_DASH : fmtTokens(distValue(row), lang)}
              </span>
            </div>
          );
        })}

        {hidden > 0 ? (
          <Button
            variant="ghost"
            size="sm"
            data-testid={`usage-record-more-${card.toolId}`}
            onClick={() => onToggle(card.toolId)}
          >
            {expanded ? t("usage.records.collapse") : t("usage.records.moreRows", { n: hidden })}
          </Button>
        ) : null}
      </CardContent>
    </Card>
  );
}

/**
 * 点击行后才挂载：按卡片限制查询 identities（tool 卡限 toolIds；project 卡限 projects，
 * `card.toolId` 是项目键，绝不当 sourceId）。候选显示名与点击行 `row.label` 比较；
 * 同名碰撞（多条匹配）全部交给 Dialog 让用户显式选择。
 *
 * Task 7 起同一 loader 以 `intent` 分派两个独立浮窗（计费绑定 / 供应商别名）：
 * identities 查询与同名候选筛选只有这一套，禁止复制第二遍。
 */
function AssignmentLoader({
  card,
  row,
  cardDim,
  range,
  filters,
  intent,
  explicitlyBound,
  onClose,
  t,
}: {
  card: UsageCard;
  row: UsageRow;
  cardDim: UsageRecordsGroupBy;
  range: UsageRange;
  filters: UsageFilters;
  intent: "billing" | "alias";
  explicitlyBound: boolean;
  onClose(): void;
  t: TFn;
}) {
  const scoped: UsageFilters =
    cardDim === "tool"
      ? { ...filters, toolIds: [card.toolId] }
      : { ...filters, projects: [card.toolId] };
  const identities = useUsageIdentitiesQuery(range, scoped);
  // 别名浮窗需要当前规则（预填 / 复用 id）；与记录页行按钮共用同一查询缓存
  const aliases = useUsageProviderAliasesQuery();

  // useMemo 固化候选数组引用：后台 refetch（["usage"] 失效链）带来新 data 引用时，
  // 不得连带触发 Dialog 的重置 effect（否则用户已选候选与预览会被清空）。
  // 必须放在下方早退分支之前（hooks 不能条件调用）；pending/error 时结果是空数组，不参与渲染。
  const candidates = useMemo(() => {
    const all = identities.data ?? [];
    const matched = all.filter((id: IdentityUsage) => identityDisplayName(id) === row.label);
    return matched.length > 0 ? matched : all;
  }, [identities.data, row.label]);

  if (identities.isPending) {
    return <p className="text-muted-foreground text-sm">{t("common.loading")}</p>;
  }
  if (identities.isError) {
    return (
      <div className="flex items-center gap-2" role="alert">
        <p className="text-destructive text-sm">{usageErrMsg(identities.error, t)}</p>
        <Button size="sm" variant="outline" onClick={() => void identities.refetch()}>
          {t("common.retry")}
        </Button>
      </div>
    );
  }

  if (intent === "alias") {
    if (aliases.isPending) {
      return <p className="text-muted-foreground text-sm">{t("common.loading")}</p>;
    }
    if (aliases.isError) {
      return (
        <div className="flex items-center gap-2" role="alert">
          <p className="text-destructive text-sm">{usageErrMsg(aliases.error, t)}</p>
          <Button size="sm" variant="outline" onClick={() => void aliases.refetch()}>
            {t("common.retry")}
          </Button>
        </div>
      );
    }
    return (
      <ProviderAliasDialog
        open
        onOpenChange={(open) => {
          if (!open) onClose();
        }}
        identities={candidates}
        explicitlyBound={explicitlyBound}
        currentAliases={aliases.data?.aliases ?? []}
        t={t}
      />
    );
  }

  return (
    <BillingAssignmentDialog
      open
      onOpenChange={(open) => {
        if (!open) onClose();
      }}
      identities={candidates}
      range={range}
      t={t}
    />
  );
}

export interface UsageRecordsPanelProps {
  /** 与看板**共用**的时间范围（页面持有；两页签同一次打开窗口） */
  range: UsageRange;
  /** 明细保留期设置（天）。`null` = 设置还没回来 → 该段整段不出（不猜默认、不填 0） */
  retentionDays: number | null;
  /**
   * **受控筛选状态**（唯一出处 = `src/lib/usage/useRecordsFilters.ts`）。
   * 2026-10-06 用户裁决：CSV 要接收记录页的筛选条件 ⇒ 屏幕与导出共用同一份状态，故由页面注入。
   * `filters` 是**第三层守卫的产物**（日档不含 `subagentMode`），本面板**直接用它**、不自行现拼。
   */
  records: RecordsFiltersState;
  /** 译函数（`usage.records.*` / `usage.source.*` / `usage.error.*` / `common.*`） */
  t: TFn;
}

export function UsageRecordsPanel({
  range,
  retentionDays,
  records: rec,
  t,
}: UsageRecordsPanelProps) {
  /** 已展开的超限卡片（逐卡独立；`toolId` 即卡片 key）——**纯视图态**，不上提（导出用不到它） */
  const [expandedKeys, setExpandedKeys] = useState<readonly string[]>([]);
  /** 待处理的行（点击后才查询 identities；Dialog 关闭即清空）。`intent` 分派两个独立浮窗。 */
  const [assign, setAssign] = useState<{
    card: UsageCard;
    row: UsageRow;
    intent: "billing" | "alias";
    explicitlyBound: boolean;
  } | null>(null);

  // 筛选状态与三层日档守卫全部来自页面注入的**唯一出处**（`useRecordsFilters`）：
  // 「第一层」= `dayTier`（下面的 disabled + 常驻原因）；「第二层」复位与「第三层」filters 双保险
  // 都在那个 hook 里（导出要用同一份，见其文件头）。
  const {
    cardDim,
    setCardDim,
    toolIds,
    toggleTool,
    parentsOnly,
    setParentsOnly,
    dayTier,
    filters,
  } = rec;

  const records = useUsageRecordsQuery(range, cardDim, filters);
  const cards = records.data?.cards ?? [];

  /**
   * 归属映射：
   *  * tool 卡按 `${sourceId}|${label}` —— 不同工具的同一 label 不得互相覆盖；
   *  * project 卡按 label 聚合**所有**同 label identity（identity 无 project 字段），保守判定。
   */
  const identities = useUsageIdentitiesQuery(range, filters);
  const { byTool, byLabel } = useMemo(() => {
    const toolMap = new Map<string, IdentityUsage[]>();
    const labelMap = new Map<string, IdentityUsage[]>();
    for (const identity of identities.data ?? []) {
      const label = identityDisplayName(identity);
      const toolKey = `${identity.identity.sourceId}|${label}`;
      toolMap.set(toolKey, [...(toolMap.get(toolKey) ?? []), identity]);
      labelMap.set(label, [...(labelMap.get(label) ?? []), identity]);
    }
    return { byTool: toolMap, byLabel: labelMap };
  }, [identities.data]);

  /** 同一行在 tool / project 卡下的身份集合：tool 卡按工具隔离，project 卡按 label 聚合。 */
  const identitiesOf = (card: UsageCard, label: string) =>
    cardDim === "tool" ? byTool.get(`${card.toolId}|${label}`) : byLabel.get(label);

  const attributionOf = (card: UsageCard, label: string): RowAttribution =>
    aggregateAttribution(identitiesOf(card, label));

  const aliases = useUsageProviderAliasesQuery();
  const aliasRules = aliases.data?.aliases ?? [];
  /** 命中任一身份别名即视为可编辑（打开浮窗后由用户选择具体身份）。 */
  const hasAlias = (card: UsageCard, label: string): boolean =>
    (identitiesOf(card, label) ?? []).some((identity) =>
      Boolean(findAliasForIdentity(aliasRules, identity))
    );

  /**
   * 行显示名：供应商别名优先，原始模型名不变。
   *  * 同一行只映射到一个显示名时采用（例如 `zhipuai / glm-5` → `智谱 / glm-5`）；
   *  * project 卡把同名 label 跨工具聚合，若映射出多个显示名则保守回退原始 label；
   *  * identities 缺失/为空时回退原始 label。
   * 行数据与 tooltip 始终保留原始「供应商 / 模型」。
   */
  const displayLabelOf = (card: UsageCard, row: UsageRow): string => {
    const list = identitiesOf(card, row.label);
    if (!list || list.length === 0) return row.label;
    const labels = new Set(list.map((identity) => displayIdentityLabel(aliasRules, identity)));
    return labels.size === 1 ? [...labels][0] : row.label;
  };

  /** 旧版计费归属/参考价规则管理器：从记录页页脚的「高级」入口打开。 */
  const [rulesOpen, setRulesOpen] = useState(false);

  const toggleExpanded = (key: string) =>
    setExpandedKeys((prev) =>
      prev.includes(key) ? prev.filter((k) => k !== key) : [...prev, key]
    );

  let body: ReactNode;
  if (records.isPending) {
    body = <UsageLoading label={t("common.loading")} />;
  } else if (records.isError) {
    // 用量命令 reject 结构化 `{ code, detail }` → 一律 usageErrMsg（`String(e)` 会印 `[object Object]`）
    body = (
      <UsageError
        title={t("usage.error.title")}
        message={usageErrMsg(records.error, t)}
        retryLabel={t("common.retry")}
        onRetry={() => void records.refetch()}
      />
    );
  } else if (cards.length === 0) {
    // 空态：出文案而不是 0（`usage-records-empty` 是本页的稳定钩子；卡片区结构上不可能留一行 0）
    body = (
      <div data-testid="usage-records-empty">
        <UsageEmpty label={t("usage.records.empty")} />
      </div>
    );
  } else {
    body = (
      <div className="space-y-3">
        {cards.map((c) => (
          <RecordCard
            key={c.toolId}
            card={c}
            cardDim={cardDim}
            expanded={expandedKeys.includes(c.toolId)}
            onToggle={toggleExpanded}
            onAssign={(card, row) =>
              setAssign({ card, row, intent: "billing", explicitlyBound: false })
            }
            onAlias={(card, row, explicitlyBound) =>
              setAssign({ card, row, intent: "alias", explicitlyBound })
            }
            displayLabelOf={displayLabelOf}
            attributionOf={attributionOf}
            hasAlias={hasAlias}
            t={t}
          />
        ))}
      </div>
    );
  }

  const subagentLabel = parentsOnly
    ? t("usage.records.subagentParentsOnly")
    : t("usage.records.subagentInclude");
  const subagentReason = t("usage.records.subagentUnavailable");

  return (
    <div className="space-y-4">
      {/* 控制条：卡片维度 + 工具 chips（D1 七源）+ 子代理开关。三态分支之外 ⇒ 加载 / 空态下也在 */}
      <div className="flex flex-wrap items-center gap-x-4 gap-y-2">
        <div className="flex items-center gap-1.5">
          <span className="text-muted-foreground text-xs">{t("usage.records.cardDim")}</span>
          {RECORD_DIMS.map((dim) => (
            <Button
              key={dim}
              size="sm"
              variant={dim === cardDim ? "default" : "outline"}
              aria-pressed={dim === cardDim}
              data-testid={`usage-records-dim-${dim}`}
              onClick={() => setCardDim(dim)}
            >
              {t(`usage.records.dim.${dim}`)}
            </Button>
          ))}
        </div>

        <div className="flex flex-wrap items-center gap-1.5">
          <span className="text-muted-foreground text-xs">{t("usage.records.filterTools")}</span>
          {FILTER_TOOL_IDS.map((id) => {
            const on = toolIds.includes(id);
            return (
              <Button
                key={id}
                size="sm"
                variant={on ? "default" : "outline"}
                aria-pressed={on}
                data-testid={`usage-records-tool-${id}`}
                onClick={() => toggleTool(id)}
              >
                {usageAgentLabel(id)}
              </Button>
            );
          })}
        </div>

        <div className="flex items-center gap-2">
          <Switch
            aria-label={subagentLabel}
            data-testid="usage-records-subagent"
            checked={parentsOnly}
            // 第一层：日档禁用；`title` 与可见文案同源，都是常驻原因
            disabled={dayTier}
            title={dayTier ? subagentReason : undefined}
            onCheckedChange={setParentsOnly}
          />
          <span className="text-muted-foreground text-xs">{subagentLabel}</span>
          {dayTier ? (
            <span
              data-testid="usage-records-subagent-unavailable"
              title={subagentReason}
              className="text-xs text-amber-600 dark:text-amber-500"
            >
              {subagentReason}
            </span>
          ) : null}
        </div>
      </div>

      {body}

      {/* 页脚口径：`parentsOnly` 时**同步**切「不含子代理」（spec §4.3 明文）；保留期由设置项给。
          旧版计费归属/参考价规则不再逐行露出，收进这里的「高级」入口。 */}
      <footer data-testid="usage-records-caliber" className="text-muted-foreground text-xs">
        {parentsOnly ? t("usage.records.caliberParentsOnly") : t("usage.records.caliberInclude")}
        {retentionDays != null ? (
          <span data-testid="usage-records-retention">
            {" · "}
            {t("usage.records.retention", { days: retentionDays })}
          </span>
        ) : null}
        {" · "}
        <Button
          size="xs"
          variant="link"
          className="h-auto p-0 text-xs"
          data-testid="usage-records-billing-advanced"
          onClick={() => setRulesOpen(true)}
        >
          {t("usage.records.billingRulesAdvanced")}
        </Button>
      </footer>

      {assign ? (
        <AssignmentLoader
          card={assign.card}
          row={assign.row}
          cardDim={cardDim}
          range={range}
          filters={filters}
          intent={assign.intent}
          explicitlyBound={assign.explicitlyBound}
          onClose={() => setAssign(null)}
          t={t}
        />
      ) : null}

      <BillingProfilesManager t={t} open={rulesOpen} onOpenChange={setRulesOpen} hideTrigger />
    </div>
  );
}
