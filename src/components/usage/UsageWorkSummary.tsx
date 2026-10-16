// 工作小结区（计划② Task 8；spec D15–D19）——会话数 / turn 逐工具 / 报错四行 /
// 最长单 turn 逐工具 / 工具调用 2×2 + Top 两个第一名。消费方：大看板页 board 分支
// （趋势卡之后、分布卡之前；本组件纯展示：不查询、不取数，数据全由 props 给）。
//
// 五条纪律：
//  ① **逐指标空态**（§3 第 5/9 条）：值本身为 `null`（或该源 `perSource` 不可得）→ `EM_DASH` +
//     `title` 说明原因，**绝不填 0**。`sessions` / `toolAvgMs` / `topTool` / `topToolMs` 四条
//     运行时**没有** `availability` 条目（`availability_table()` 只出 8 条）⇒ 不得按「缺条目
//     = 空态」处理（判据全在 `availability.ts`，本文件不重写判据）。
//  ② **不合计**（D16/D18/D19）：turn 与最长单 turn 只按工具分列、无合计行；报错三层 + 用户打断
//     **四行都在、绝不合并**（三层「错误」互不包含，相加不表示任何含义）。
//  ③ 显示口径（spec P8）：计数类 `fmtInt`（完整千分位）、平均耗时与 p50 走 `fmtDur`、
//     最长走 `fmtLongest`；`EM_DASH` 只从 `format.ts` 取（§3 第 22 条）。
//  ④ 工具名分派：`turnsPerTool` / `longestTurnPerTool` 的**键是工具 id**（`query.rs` 的
//     `group_of` 口径）⇒ 一律 `usageAgentLabel(toolId)`（§3 第 13 条）；而 `topTool` /
//     `topToolMs` 的 `name` 是**工具调用名**（后端 `tool_stats` 的键，如 `Bash`，
//     `query.rs::work_summary_with`）、不是采集源 id ⇒ **原样渲染**（走 `usageAgentLabel`
//     会印成 `?`）。两者不是同一个维度，**不得互相套用**。
//  ⑤ 段头「含挂机、未剔 idle」**常驻**（D19：七源里只有 opencode 有 `time_idle`，无法统一剔除）；
//     取色只认主题 token（§3 第 19 条），卡片走 `ui/card` 原语（§3 第 40 条）。
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import {
  availabilityOf,
  isMetricAvailable,
  isToolAvailable,
  metricReason,
  toolReason,
} from "@/lib/usage/availability";
import { EM_DASH, fmtDur, fmtInt, fmtLongest, usageAgentLabel } from "@/lib/usage/format";
import { getLang } from "@/lib/usage/lang";
import type { TFn } from "@/lib/usage/range";
import type { UsageAvailability, WorkSummary } from "@/types/usage";

export interface UsageWorkSummaryProps {
  /** 工作小结（页面传 `dash.workSummary`）；`null` / 某字段为 `null` = 不可得 → 逐指标空态 */
  work: WorkSummary | null | undefined;
  /** 可得性说明（页面传 `dash.availability`）；**缺条目 ≠ 不可得**（§3 第 9 条） */
  availability: readonly UsageAvailability[] | null | undefined;
  /** 译函数（`usage.work.*` / `usage.turnBasis.*`） */
  t: TFn;
}

/**
 * 「标签 + 值」一行（计数类与时长类共用；值由调用方格式化）。
 * `text === null` = 不可得 ⇒ `EM_DASH` + `unavailableTitle` 说明原因（绝不填 0）。
 */
function ValueRow({
  testid,
  label,
  text,
  title,
  unavailableTitle,
}: {
  testid: string;
  label: string;
  text: string | null;
  /** 值可用时的 `title`（如会话数的分层说明）；`undefined` = 不挂 */
  title?: string;
  unavailableTitle: string;
}) {
  return (
    <div
      data-testid={testid}
      title={text === null ? unavailableTitle : title}
      className="flex items-center justify-between gap-2"
    >
      <span className="text-muted-foreground">{label}</span>
      <span className="font-medium tabular-nums">{text ?? EM_DASH}</span>
    </div>
  );
}

export function UsageWorkSummary({ work, availability, t }: UsageWorkSummaryProps) {
  // 格式化语言（issue #160）：时长单位词（分钟/秒/毫秒 vs min/s/ms）随界面语言走，组件体取一次。
  const lang = getLang();
  const sessions = work?.sessions ?? null;
  const turns = work?.turnsPerTool ?? {};
  const longest = work?.longestTurnPerTool ?? {};
  const topTool = work?.topTool ?? null;
  const topToolMs = work?.topToolMs ?? null;

  // 可得性条目**逐指标各取一次**（缺条目 = 可用；指标级 vs 逐源是两层，见 availability.ts）
  const sessionsAv = availabilityOf(availability, "sessions");
  const turnAv = availabilityOf(availability, "turn");
  const errorModelAv = availabilityOf(availability, "errorModel");
  const errorTurnAv = availabilityOf(availability, "errorTurn");
  const errorToolAv = availabilityOf(availability, "errorTool");
  const interruptedAv = availabilityOf(availability, "interrupted");
  const longestAv = availabilityOf(availability, "longestTurn");
  const toolCallsAv = availabilityOf(availability, "toolCalls");
  const toolAvgAv = availabilityOf(availability, "toolAvgMs");
  const topToolAv = availabilityOf(availability, "topTool");
  const topToolMsAv = availabilityOf(availability, "topToolMs");

  /** 计数类：值本身为 `null` 或该指标级不可得 → `null`（渲染 `—`）（§3 第 5 条） */
  const countOf = (v: number | null | undefined, av: UsageAvailability | null): string | null =>
    v === null || v === undefined || !isMetricAvailable(av) ? null : fmtInt(v);

  /**
   * 聚合行的「**不含哪些源**」提示（Task 15 验收条目 B-5）：**数据驱动** —— 只要该指标的
   * `perSource` 里存在值为 `false` 的条目就给提示；无缺口（含没有 `perSource`）→ `undefined`。
   * 四层报错行**都**挂在同一个 helper 上：真机四行全有缺口（`caps.rs`：
   * `workbuddy.error_model` / `claude.error_turn` + `workbuddy.error_turn` /
   * `opencode.error_tool` / `kimi.interrupted` 均为 `false`，且 `availability_table()`
   * 对**每个**指标都挂 7 源全量映射）——**不得**按「哪几行看起来有缺口」写死，也不得一律加。
   * 源一律用**原始源 id** 呈现（① 的 `UsageStatusSection.tsx:239` 先例），**不得**走
   * `usageAgentLabel`（这里是能力缺口诊断，要的是可对账的源 id 而非显示名）。
   * 值本身不可得时这条让位给「不可得原因」（见 `ValueRow` 的 title 取舍）。
   */
  const partialSourcesOf = (av: UsageAvailability | null): string | undefined => {
    const excluded = Object.entries(av?.perSource ?? {})
      .filter(([, ok]) => !ok)
      .map(([sourceId]) => sourceId)
      .sort();
    return excluded.length === 0
      ? undefined
      : t("usage.work.partialSources", { sources: excluded.join(", ") });
  };

  const turnKeys = Object.keys(turns);
  // 「全不可得」（无一行可显示）→ 段内空态，而不是列一串 `—`（D16）
  const hasTurnRow = turnKeys.some((k) => turns[k] != null && isToolAvailable(turnAv, k));

  const longestKeys = Object.keys(longest);
  const hasLongestRow = longestKeys.some(
    (k) => longest[k] != null && isToolAvailable(longestAv, k)
  );

  // Top 两个「第一名」：名字是工具调用名（不是工具 id）⇒ 原样渲染（见文件头 ④）
  const topCountText =
    topTool !== null && isMetricAvailable(topToolAv)
      ? `${topTool.name} · ${fmtInt(topTool.count)}`
      : null;
  const topMsText =
    topToolMs !== null && isMetricAvailable(topToolMsAv)
      ? `${topToolMs.name} · ${fmtDur(topToolMs.ms, lang)}`
      : null;

  /** 段头（`title` 承载该段的全局口径提示） */
  const sectionTitle = (text: string, title?: string) => (
    <h3 title={title} className="text-muted-foreground text-xs font-medium">
      {text}
    </h3>
  );

  return (
    <Card data-testid="usage-work-summary">
      <CardHeader>
        <CardTitle>{t("usage.work.title")}</CardTitle>
      </CardHeader>

      <CardContent className="space-y-4 text-sm">
        {/* D17：计数类只显示**分层后的单值**，父子会话不重复计；展开子代理层需扩契约，本次不做 */}
        <p data-testid="work-subagent-layer" className="text-muted-foreground text-xs">
          {t("usage.work.subagentLayer")}
        </p>

        {/* 会话数：D17 分层后的单值 + hover 说明（不可得时才把 title 换成原因） */}
        <ValueRow
          testid="work-sessions"
          label={t("usage.work.sessions")}
          text={countOf(sessions, sessionsAv)}
          title={t("usage.work.sessionsHint")}
          unavailableTitle={metricReason(t, sessionsAv)}
        />

        {/* turn 逐工具：**无合计行**（D16：各源回合判据语义不可比），每工具恒给该源口径 */}
        <section className="space-y-1">
          {sectionTitle(t("usage.work.turns"), t("usage.work.turnsHint"))}
          {hasTurnRow ? (
            <ul className="space-y-1">
              {turnKeys.map((toolId) => {
                const value = turns[toolId];
                const text =
                  value == null || !isToolAvailable(turnAv, toolId) ? EM_DASH : fmtInt(value);
                return (
                  <li
                    key={toolId}
                    data-testid={`work-turns-${toolId}`}
                    // 逐源可查：可用时给该源回合口径；不可得时给不可得原因（perSource 也落到该源口径）
                    title={
                      text === EM_DASH
                        ? toolReason(t, toolId, turnAv)
                        : t(`usage.turnBasis.${toolId}`)
                    }
                    className="flex items-center justify-between gap-2"
                  >
                    <span>{usageAgentLabel(toolId)}</span>
                    <span className="font-medium tabular-nums">{text}</span>
                  </li>
                );
              })}
            </ul>
          ) : (
            <p data-testid="work-turns-empty" className="text-muted-foreground">
              {metricReason(t, turnAv)}
            </p>
          )}
        </section>

        {/* 报错三层 + 用户打断：四行分列（D18：三层互不包含，相加无意义；打断不计入错误）；
            四行**一律**按 availability.perSource 标注被排除的源（B-5：真机四行都有缺口，
            见 `partialSourcesOf`），数字与结构不受影响 */}
        <section className="space-y-1">
          {sectionTitle(t("usage.work.errors"))}
          <ValueRow
            testid="work-errorModel"
            label={t("usage.work.errorModel")}
            text={countOf(work?.errorModel, errorModelAv)}
            title={partialSourcesOf(errorModelAv)}
            unavailableTitle={metricReason(t, errorModelAv)}
          />
          <ValueRow
            testid="work-errorTurn"
            label={t("usage.work.errorTurn")}
            text={countOf(work?.errorTurn, errorTurnAv)}
            title={partialSourcesOf(errorTurnAv)}
            unavailableTitle={metricReason(t, errorTurnAv)}
          />
          <ValueRow
            testid="work-errorTool"
            label={t("usage.work.errorTool")}
            text={countOf(work?.errorTool, errorToolAv)}
            title={partialSourcesOf(errorToolAv)}
            unavailableTitle={metricReason(t, errorToolAv)}
          />
          <ValueRow
            testid="work-interrupted"
            label={t("usage.work.interrupted")}
            text={countOf(work?.interrupted, interruptedAv)}
            title={partialSourcesOf(interruptedAv)}
            unavailableTitle={metricReason(t, interruptedAv)}
          />
        </section>

        {/* 最长单 turn 逐工具：p50 + 最长两个数，**不跨工具合计**（D19） */}
        <section className="space-y-1">
          {sectionTitle(t("usage.work.longest"))}
          <p data-testid="work-longest-hint" className="text-muted-foreground text-xs">
            {t("usage.work.longestHint")}
          </p>
          {hasLongestRow ? (
            <ul className="space-y-1">
              {longestKeys.map((toolId) => {
                const value = longest[toolId];
                const usable = value != null && isToolAvailable(longestAv, toolId) ? value : null;
                return (
                  <li
                    key={toolId}
                    data-testid={`work-longest-${toolId}`}
                    title={usable === null ? toolReason(t, toolId, longestAv) : undefined}
                    className="flex items-center justify-between gap-2"
                  >
                    <span>{usageAgentLabel(toolId)}</span>
                    <span className="tabular-nums">
                      <span className="text-muted-foreground">{t("usage.work.p50")}</span>{" "}
                      {usable === null ? EM_DASH : fmtDur(usable.p50, lang)}
                      {" · "}
                      <span className="text-muted-foreground">{t("usage.work.max")}</span>{" "}
                      {usable === null ? EM_DASH : fmtLongest(usable.max, lang)}
                    </span>
                  </li>
                );
              })}
            </ul>
          ) : (
            <p data-testid="work-longest-empty" className="text-muted-foreground">
              {metricReason(t, longestAv)}
            </p>
          )}
        </section>

        {/* 工具调用 2×2：调用总数 / 平均耗时 / 次数第一名 / 耗时第一名。
            【默认裁定】本次只给两个「第一名」，不做 Top5 列表（要做需把契约改成数组）。 */}
        <section className="space-y-1">
          {sectionTitle(t("usage.work.toolCalls"))}
          <ValueRow
            testid="work-tool-calls"
            label={t("usage.work.toolCallsTotal")}
            text={countOf(work?.toolCalls, toolCallsAv)}
            unavailableTitle={metricReason(t, toolCallsAv)}
          />
          <ValueRow
            testid="work-tool-avg"
            label={t("usage.work.toolAvg")}
            text={
              work?.toolAvgMs == null || !isMetricAvailable(toolAvgAv)
                ? null
                : fmtDur(work.toolAvgMs, lang)
            }
            unavailableTitle={metricReason(t, toolAvgAv)}
          />
          <p className="text-muted-foreground text-xs">{t("usage.work.topList")}</p>
          <ul data-testid="work-top-list" className="space-y-1">
            <li>
              <ValueRow
                testid="work-top-count"
                label={t("usage.work.topToolCount")}
                text={topCountText}
                unavailableTitle={metricReason(t, topToolAv)}
              />
            </li>
            <li>
              <ValueRow
                testid="work-top-ms"
                label={t("usage.work.topToolMs")}
                text={topMsText}
                unavailableTitle={metricReason(t, topToolMsAv)}
              />
            </li>
          </ul>
        </section>
      </CardContent>
    </Card>
  );
}
