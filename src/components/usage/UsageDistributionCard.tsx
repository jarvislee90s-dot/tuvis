// 用量分布卡（V2 Task 12）——四维 Token 分布，默认 Top5，命中率与相对条，无美元列。
//
// 两种导出：
//  * `UsageDistributionBody`：**无 Card 外壳**，供页面放进 `UsageFold`（避免卡中卡）；
//  * `UsageDistributionCard`：保留 Card 壳的独立形态（直接渲染 / 既有测试）。
//
// 行名分派（逐维不同）：
//  * billingProfile（渠道）：BillingProfile/Alias/Provider 用 `displayProvider`；Unassigned
//    用 i18n 键（全局一行，**不附工具**）；Ambiguous 用 i18n 键；
//  * tool：`usageAgentLabel(label)`；
//  * model：`displayProvider` 或 i18n 键 + ` / ` + 原始 `modelId`（空串 → 未知模型）；
//    Unassigned 附工具上下文、Ambiguous 附原始 provider（缺失退工具）；同显示名跨工具合并；
//  * project：原文，完整名保留 title。
// 原始 provider 与完整模型保留在 title/tooltip；展示层不改写账本身份。
// Token 数值只显示万/亿缩写，不再挂完整千分位 hover 值（2026-10-19 用户裁决）。
// 占比纯层在 `distributionV2.ts`（Top5 与旧固定分享的 10 行上限互不影响）。
import { Fragment } from "react";
import { Button } from "@/components/ui/button";
import { Card, CardAction, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { DIST_MIN_BAR_PCT } from "@/lib/usage/distribution";
import {
  DIST_TOP_ROWS,
  distributionV2Rows,
  distributionV2View,
  type DistributionV2Row,
} from "@/lib/usage/distributionV2";
import { EM_DASH, fmtPct, fmtTokens, usageAgentLabel } from "@/lib/usage/format";
import { getLang } from "@/lib/usage/lang";
import type { TFn } from "@/lib/usage/range";
import type { DistributionGroup, DistributionRow } from "@/types/usageV2";

const GROUP_DIMS: readonly DistributionGroup[] = ["billingProfile", "tool", "project", "model"];

export interface UsageDistributionBodyProps {
  rows: readonly DistributionRow[] | null | undefined;
  groupBy: DistributionGroup;
  onGroupByChange(groupBy: DistributionGroup): void;
  /** Top5 展开状态由页面顶层持有（折叠卡不重置） */
  expanded: boolean;
  onExpandedChange(expanded: boolean): void;
  t: TFn;
}

function groupLabelKey(dim: DistributionGroup): string {
  // `usage.group.model` 被固定分享的文本摘要沿用（「按供应商/模型」）；看板 C 用新标签。
  return dim === "model" ? "usage.group.modelTokens" : `usage.group.${dim}`;
}

function GroupButtons({
  groupBy,
  onGroupByChange,
  t,
}: Pick<UsageDistributionBodyProps, "groupBy" | "onGroupByChange" | "t">) {
  return (
    <div className="flex flex-wrap items-center gap-1.5">
      {GROUP_DIMS.map((dim) => (
        <Button
          key={dim}
          size="sm"
          variant={dim === groupBy ? "default" : "outline"}
          aria-pressed={dim === groupBy}
          data-testid={`usage-dist-group-${dim}`}
          onClick={() => onGroupByChange(dim)}
        >
          {t(groupLabelKey(dim))}
        </Button>
      ))}
    </div>
  );
}

/** 行名分派（见文件头）；`full` 只在需要保留原始证据时用于 title */
function toolsOf(row: DistributionV2Row): string {
  return [...new Set(row.sourceIds)].map(usageAgentLabel).join(", ");
}

/** 原始 provider 证据面：多值时用完整列表，只有单个/旧形状时回退单值。 */
function observedEvidenceOf(row: DistributionV2Row): string | null {
  const list = row.observedProviders ?? [];
  if (list.length > 0) return list.join(", ");
  return row.observedProvider;
}

/**
 * 证据完整性提示：组内同时有非空原始 provider 与空 provider（未标注）行时，
 * 非空列表也不代表完整证据 ⇒ tooltip 必须显式说明（unassigned 行不重复标注）。
 */
function unlabeledNoteOf(row: DistributionV2Row, t: TFn): string | null {
  return (row.observedProviders?.length ?? 0) > 0 && row.hasEmptyProvider
    ? t("usage.cost.unlabeledSource")
    : null;
}

/** 渠道维度：显示名或 i18n 键；未指定/可能混合不追加工具后缀。 */
function channelNameOf(row: DistributionV2Row, t: TFn): { text: string; full: string | null } {
  let text: string;
  switch (row.channelKind) {
    case "unassigned":
      text = t("usage.label.unassignedChannel");
      break;
    case "ambiguous":
      text = t("usage.label.ambiguousChannel");
      break;
    case "billingProfile":
    case "alias":
    case "provider":
      text = row.displayProvider || row.label;
      break;
    default:
      // 兼容旧行形状（channelKind 缺失）：有绑定用 label；未绑定 label 是 i18n 键。
      text = row.billingProfileId !== null ? row.label : t(row.label);
      break;
  }
  // 原始 provider 完整证据进 tooltip/title；展示名与证据相同时不重复。
  const evidence = observedEvidenceOf(row);
  const withEvidence = evidence && evidence !== text ? `${text} · ${evidence}` : null;
  const note = unlabeledNoteOf(row, t);
  const full = note
    ? withEvidence
      ? `${withEvidence} · ${note}`
      : `${text} · ${note}`
    : withEvidence;
  return { text, full };
}

/** 模型维度：`显示名 / 原始模型`；未指定附工具，可能混合附原始 provider 或工具。 */
function modelNameOf(row: DistributionV2Row, t: TFn): { text: string; full: string | null } {
  const model =
    row.modelId !== null
      ? row.modelId === ""
        ? t("usage.label.unknownModel")
        : row.modelId
      : row.label;
  let text: string;
  switch (row.channelKind) {
    case "unassigned": {
      const tools = toolsOf(row);
      const base = t("usage.label.unassignedChannel");
      text = `${tools ? `${base} · ${tools}` : base} / ${model}`;
      break;
    }
    case "ambiguous": {
      const context = observedEvidenceOf(row) ?? toolsOf(row);
      const base = t("usage.label.ambiguousChannel");
      text = `${context ? `${base} · ${context}` : base} / ${model}`;
      break;
    }
    case "billingProfile":
    case "alias":
    case "provider":
      text = `${row.displayProvider ?? ""} / ${model}`;
      break;
    default:
      // 兼容旧行形状：有绑定时 label 已是「显示名 / 模型」；未绑定补未指定渠道前缀。
      text =
        row.billingProfileId !== null
          ? row.label
          : `${t("usage.label.unassignedChannel")} / ${row.label}`;
      break;
  }
  const evidence = observedEvidenceOf(row) ?? row.displayProvider;
  const base = row.channelKind !== null && evidence ? `${evidence} / ${model}` : text;
  const note = unlabeledNoteOf(row, t);
  return { text, full: note ? `${base} · ${note}` : base };
}

function nameOf(
  row: DistributionV2Row,
  groupBy: DistributionGroup,
  t: TFn
): { text: string; full: string | null } {
  if (groupBy === "tool") {
    return { text: usageAgentLabel(row.label), full: null };
  }
  if (groupBy === "model") return modelNameOf(row, t);
  if (groupBy === "billingProfile") return channelNameOf(row, t);
  return { text: row.label, full: row.label };
}

function barPctOf(share: number): string {
  return Math.max(DIST_MIN_BAR_PCT, share * 100).toFixed(1);
}

function DistributionRows({
  rows,
  groupBy,
  expanded,
  onExpandedChange,
  t,
}: Pick<UsageDistributionBodyProps, "rows" | "groupBy" | "expanded" | "onExpandedChange" | "t">) {
  const lang = getLang();
  const all = distributionV2Rows(rows);
  const view = distributionV2View(all, expanded);

  if (view.rows.length === 0) {
    return (
      <p data-testid="usage-empty" className="text-muted-foreground text-sm">
        {t("usage.empty")}
      </p>
    );
  }

  return (
    <div className="space-y-2">
      <div className="grid grid-cols-[minmax(0,1fr)_auto_auto_minmax(80px,1fr)] items-center gap-x-3 gap-y-2 text-sm">
        {view.rows.map((row) => {
          const name = nameOf(row, groupBy, t);
          return (
            <Fragment key={row.key}>
              <span
                data-testid={`usage-dist-${row.key}`}
                className="min-w-0 truncate"
                title={name.full ?? undefined}
              >
                {name.text}
              </span>
              <span
                data-testid={`usage-val-${row.key}`}
                className="text-right font-medium whitespace-nowrap tabular-nums"
              >
                {fmtTokens(row.value, lang)}
              </span>
              <span
                data-testid={`usage-hit-${row.key}`}
                className="text-muted-foreground text-right text-xs whitespace-nowrap tabular-nums"
              >
                {row.hitRate === null ? EM_DASH : fmtPct(row.hitRate)}
              </span>
              <span
                data-testid={`usage-dist-bar-${row.key}`}
                className="h-1.5 min-w-0 overflow-hidden rounded-full bg-[var(--usage-track)]"
              >
                <span
                  className="block h-full rounded-full bg-[var(--usage-accent-a)]"
                  style={{ width: `${barPctOf(row.share)}%` }}
                />
              </span>
            </Fragment>
          );
        })}
      </div>

      {all.length > DIST_TOP_ROWS ? (
        <Button
          size="sm"
          variant="ghost"
          data-testid="usage-dist-expand"
          aria-expanded={expanded}
          onClick={() => onExpandedChange(!expanded)}
        >
          {expanded
            ? t("usage.dist.less")
            : t("usage.dist.more", { n: all.length - DIST_TOP_ROWS })}
        </Button>
      ) : null}
    </div>
  );
}

/** 无 Card 外壳的 C 卡体（页面放进 `UsageFold`） */
export function UsageDistributionBody({
  rows,
  groupBy,
  onGroupByChange,
  expanded,
  onExpandedChange,
  t,
}: UsageDistributionBodyProps) {
  return (
    <div data-testid="usage-distribution-body" className="space-y-2">
      <GroupButtons groupBy={groupBy} onGroupByChange={onGroupByChange} t={t} />
      <DistributionRows
        rows={rows}
        groupBy={groupBy}
        expanded={expanded}
        onExpandedChange={onExpandedChange}
        t={t}
      />
    </div>
  );
}

/** 保留 Card 外壳的独立形态 */
export function UsageDistributionCard(props: UsageDistributionBodyProps) {
  const { t } = props;
  return (
    <Card data-testid="usage-distribution-card">
      <CardHeader>
        <CardTitle>{t("usage.card.distribution")}</CardTitle>
        <CardAction>
          <GroupButtons groupBy={props.groupBy} onGroupByChange={props.onGroupByChange} t={t} />
        </CardAction>
      </CardHeader>
      <CardContent>
        <DistributionRows
          rows={props.rows}
          groupBy={props.groupBy}
          expanded={props.expanded}
          onExpandedChange={props.onExpandedChange}
          t={t}
        />
      </CardContent>
    </Card>
  );
}
