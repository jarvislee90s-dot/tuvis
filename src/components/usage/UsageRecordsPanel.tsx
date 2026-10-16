// 用量记录页（计划② Task 9 步骤 3/4；spec P3 · D6/D7/D21）——每「工具 / 项目」一卡、卡内行**恒为**
// 「供应商 / 模型」+ 三态来源徽标；筛选 = 工具（D1 的 7 源）+ 子代理模式。消费方：大看板窗口的
// 「记录」页签（`src/pages/usage-dashboard.tsx`）。
//
// 七条纪律：
//  ① **本面板自带三态并自持查询**（加载 / 错误可重试 / 空态）：页面传 `range` / `retentionDays` /
//     `t` 与**受控的筛选状态**。读命令只读账本、绝不触发扫描（§3 第 15 条）；设置也只能经
//     `useUsageSettingsQuery`（页面持有），本文件**不**直连设置命令。
//  ② 行维度**不随卡片分组变**（D6）：`cardDim`（`tool` | `project`，默认 `tool`）只喂
//     `usageRecords` 的 `groupBy`，卡内行一律直渲 `row.label` **原文** —— 真机这一行已是
//     `format!("{} / {}", provider, model)`，前端**不拼**（供应商不可得时它只有模型名）。
//  ③ 卡片标题按 `groupBy` 分派（§3 第 13 条）：`tool → usageAgentLabel(card.toolId)`（真机在这个
//     维度给的 `toolLabel` 是**小写工具 id**）、`project → card.toolLabel` **原文**（D21：key 小写、
//     label 原样，不做 realpath / 不做大小写规范化 / 不用 `projectPath`）。
//  ④ 值口径 = hero（`metrics.requestTotal + buckets.output`），唯一出处是 `distribution.ts` 的
//     `distValue`；缩写走 `fmtTokens`、hover 精确值走 `fmtInt`（spec P8）。
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
import { useState, type ReactNode } from "react";
import { Button } from "@/components/ui/button";
import { Card, CardAction, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Switch } from "@/components/ui/switch";
import { UsageEmpty, UsageError, UsageLoading } from "@/components/usage/UsageStates";
import { usageErrMsg } from "@/components/usage/usageErrors";
import { useUsageRecordsQuery } from "@/lib/query/queries/usage";
import { distValue } from "@/lib/usage/distribution";
import { EM_DASH, fmtInt, fmtTokens, usageAgentLabel } from "@/lib/usage/format";
import { getLang } from "@/lib/usage/lang";
import { cardTokenUnmeasured } from "@/lib/usage/range";
import { FILTER_TOOL_IDS, RECORD_DIMS } from "@/lib/usage/useRecordsFilters";
import type { RecordsFiltersState } from "@/lib/usage/useRecordsFilters";
import type { TFn } from "@/lib/usage/range";
import type { UsageCard, UsageRange, UsageRecordsGroupBy, UsageSourceKind } from "@/types/usage";

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

/** 单张卡片：标题（按卡片维度分派）+ 合计 + 若干「供应商 / 模型」行（超限折叠） */
function RecordCard({
  card,
  cardDim,
  expanded,
  onToggle,
  t,
}: {
  card: UsageCard;
  cardDim: UsageRecordsGroupBy;
  expanded: boolean;
  onToggle(key: string): void;
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
          <span
            className="text-muted-foreground text-xs tabular-nums"
            title={noTokens ? undefined : fmtInt(total)}
          >
            {t("usage.records.cardTotal", { v: noTokens ? EM_DASH : fmtTokens(total, lang) })}
          </span>
        </CardAction>
      </CardHeader>

      <CardContent className="space-y-1.5">
        {rows.map((row, i) => {
          const rowNoTokens = cardTokenUnmeasured(row);
          return (
            <div
              key={row.key}
              data-testid={`usage-record-row-${card.toolId}:${i}`}
              className="flex items-center gap-2 text-sm"
            >
              <SourceBadge kind={row.sourceKind} t={t} />
              {/* 行名 = `row.label` 原文（真机已是「供应商 / 模型」）；全名由 title 给，前端不再拼一层 */}
              <span className="min-w-0 flex-1 truncate" title={row.label}>
                {row.label}
              </span>
              <span
                className="font-medium tabular-nums"
                title={rowNoTokens ? undefined : fmtInt(distValue(row))}
              >
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

      {/* 页脚口径：`parentsOnly` 时**同步**切「不含子代理」（spec §4.3 明文）；保留期由设置项给 */}
      <footer data-testid="usage-records-caliber" className="text-muted-foreground text-xs">
        {parentsOnly ? t("usage.records.caliberParentsOnly") : t("usage.records.caliberInclude")}
        {retentionDays != null ? (
          <span data-testid="usage-records-retention">
            {" · "}
            {t("usage.records.retention", { days: retentionDays })}
          </span>
        ) : null}
      </footer>
    </div>
  );
}
