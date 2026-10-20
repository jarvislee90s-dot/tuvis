// 工作活动（V2 Task 12）——无 Card 的 body + 可选 Card 包装，避免页面折叠卡里再套卡。
//
// 分层：
//  * 卡头摘要（页面用 `workHeaderCounts`）：会话数 / 工具调用次数，不可得 => `—`；
//  * 展开后主区：逐工具 turn、错误三层 + 打断、最常调用工具（不合计）；
//  * **第二个 `<details>` 高级统计**：p50/最长单 turn + 工具耗时（平均/耗时第一），
//    保留「含挂机、未剔 idle」提示。
//
// 纪律不变：逐指标空态 `—`（不填 0）；三层错误不合计；turn 不跨工具合计；工具名分派见注释。
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
  work: WorkSummary | null | undefined;
  availability: readonly UsageAvailability[] | null | undefined;
  t: TFn;
}

/** 卡头摘要：只出 sessions / toolCalls；不可得为 `null`（调用方渲染 `—`） */
export function workHeaderCounts(
  work: WorkSummary | null | undefined,
  availability: readonly UsageAvailability[] | null | undefined
): { sessions: number | null; toolCalls: number | null } {
  const sessionsAv = availabilityOf(availability, "sessions");
  const toolCallsAv = availabilityOf(availability, "toolCalls");
  return {
    sessions: work?.sessions != null && isMetricAvailable(sessionsAv) ? work.sessions : null,
    toolCalls: work?.toolCalls != null && isMetricAvailable(toolCallsAv) ? work.toolCalls : null,
  };
}

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

export function UsageWorkSummaryBody({ work, availability, t }: UsageWorkSummaryProps) {
  const lang = getLang();
  const sessions = work?.sessions ?? null;
  const turns = work?.turnsPerTool ?? {};
  const longest = work?.longestTurnPerTool ?? {};
  const topTool = work?.topTool ?? null;
  const topToolMs = work?.topToolMs ?? null;

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

  const countOf = (v: number | null | undefined, av: UsageAvailability | null): string | null =>
    v === null || v === undefined || !isMetricAvailable(av) ? null : fmtInt(v);

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
  const hasTurnRow = turnKeys.some((k) => turns[k] != null && isToolAvailable(turnAv, k));

  const longestKeys = Object.keys(longest);
  const hasLongestRow = longestKeys.some(
    (k) => longest[k] != null && isToolAvailable(longestAv, k)
  );

  const topCountText =
    topTool !== null && isMetricAvailable(topToolAv)
      ? `${topTool.name} · ${fmtInt(topTool.count)}`
      : null;
  const topMsText =
    topToolMs !== null && isMetricAvailable(topToolMsAv)
      ? `${topToolMs.name} · ${fmtDur(topToolMs.ms, lang)}`
      : null;

  const sectionTitle = (text: string, title?: string) => (
    <h3 title={title} className="text-muted-foreground text-xs font-medium">
      {text}
    </h3>
  );

  return (
    <div data-testid="usage-work-body" className="space-y-4 text-sm">
      <p data-testid="work-subagent-layer" className="text-muted-foreground text-xs">
        {t("usage.work.subagentLayer")}
      </p>

      <ValueRow
        testid="work-sessions"
        label={t("usage.work.sessions")}
        text={countOf(sessions, sessionsAv)}
        title={t("usage.work.sessionsHint")}
        unavailableTitle={metricReason(t, sessionsAv)}
      />

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

      {/* 最常调用工具（次数第一）留在主区；时长类进高级统计 */}
      <section className="space-y-1">
        {sectionTitle(t("usage.work.toolCalls"))}
        <ValueRow
          testid="work-tool-calls"
          label={t("usage.work.toolCallsTotal")}
          text={countOf(work?.toolCalls, toolCallsAv)}
          unavailableTitle={metricReason(t, toolCallsAv)}
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
        </ul>
      </section>

      {/* 高级统计：第二个 details（展开后可见） */}
      <details data-testid="work-advanced" className="text-muted-foreground text-xs">
        <summary className="cursor-pointer select-none">{t("usage.work.advanced")}</summary>

        <section className="mt-2 space-y-1">
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

        <section className="mt-3 space-y-1 text-sm">
          {sectionTitle(t("usage.work.toolDuration"))}
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
          <ul className="space-y-1">
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
      </details>
    </div>
  );
}

/** 保留 Card 外壳的独立形态（直接渲染 / 既有测试） */
export function UsageWorkSummary(props: UsageWorkSummaryProps) {
  return (
    <Card data-testid="usage-work-summary">
      <CardHeader>
        <CardTitle>{props.t("usage.work.title")}</CardTitle>
      </CardHeader>
      <CardContent>
        <UsageWorkSummaryBody {...props} />
      </CardContent>
    </Card>
  );
}
