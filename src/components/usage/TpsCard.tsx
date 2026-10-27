// 独立「近期TPS」区块（2026-10-24 TPS 短时抽样计划 Task 8；Spec §1 产品范围）。
//
// 与 Token/费用域的边界（Spec §7，硬约束）：
//  * 本区块自带 2m/10m/1h/today 区间状态（默认 1h），queryKey 前缀恒 `["tps"]`，
//    **不读不写**任何 Token/费用 range；Token 区间切换不会重发 TPS 查询，反之亦然；
//  * 本区块的加载/错误只替换本卡体，**绝不**遮盖费用卡；
//  * 不做瞬时速度：生成中的精确 TPS 不在本版承诺内，回合标「最近一轮」
//    （快照只有已采样的完成部分，没有进行中状态证据，不自称正在生成）。
//
// 展示纪律：四项详情只用样本值（未知 → —）；最新读数超 10 分钟 → 暂无近期样本
// （历史点仍可查看，n/方法/年龄照常可见）；gap 不画 0 不连线，出诚实说明；
// 来源状态只认 `tps-status=` 前缀；刷新按钮只读取已有事实（最小间隔 5 秒），
// 停用态判定**只认** snapshot.disabled，不认 refresh 的 reason。
import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { RefreshCw } from "lucide-react";
import { useQueryClient } from "@tanstack/react-query";
import { Card, CardAction, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { TpsChart } from "@/components/usage/TpsChart";
import { TpsMetricsPanel } from "@/components/usage/tps/TpsMetricsPanel";
import { TpsSourceStates } from "@/components/usage/tps/TpsSourceStates";
import { usageErrMsg } from "@/components/usage/usageErrors";
import { tpsRefresh } from "@/lib/api/tps";
import {
  TPS_KEY,
  TPS_POINTS_INTERVAL_MS,
  TPS_SNAPSHOT_INTERVAL_MS,
  tpsSnapshotKey,
  useTpsPointsQuery,
  useTpsSnapshotQuery,
  useTpsViewLease,
  useTpsVisible,
} from "@/lib/query/queries/tps";
import {
  formatAgeMs,
  formatTimeLabel,
  formatTpsValue,
  TPS_DEFAULT_VIEW_RANGE,
  TPS_METHOD_KEYS,
  TPS_VIEW_RANGES,
  tpsSeriesId,
} from "@/lib/usage/tpsSeries";
import { usageAgentLabel, fmtDateTime, EM_DASH } from "@/lib/usage/format";
import type { TFn } from "@/lib/usage/range";
import type { TpsSelection, TpsViewRange } from "@/types/tps";

/** 刷新最小间隔（Spec §2「最小间隔5秒」；限频时后端也会拒绝）。 */
const TPS_REFRESH_MIN_INTERVAL_MS = 5_000;

/** tps_refresh 的 reason → 提示文案键（只做提示，**不**驱动停用态）。 */
function refreshReasonKey(reason: string): string | null {
  const TABLE: Record<string, string> = {
    "rate-limited": "usage.tps.refreshNote.rateLimited",
    busy: "usage.tps.refreshNote.busy",
    disabled: "usage.tps.refreshNote.disabled",
  };
  return TABLE[reason] ?? null;
}

export function TpsCard({
  selection = {},
  active = true,
}: {
  selection?: TpsSelection;
  /**
   * active 门控（评审修复轮 2）：卡在四页宿主的「输出速度」页常驻挂载，页签切走时传
   * false。IntersectionObserver 是**纯几何判定**——`visibility:hidden`（Task 7 的隐藏
   * 机制，保滚动）的盒子仍在视口内，只看它会让隐藏页继续轮询/续租；必须与「页签是否
   * 当前」合取。默认 true，旧独立调用者行为不变。
   */
  active?: boolean;
}) {
  const { t } = useTranslation();
  const tFn = t as TFn;
  const queryClient = useQueryClient();
  const rootRef = useRef<HTMLDivElement | null>(null);
  const visible = useTpsVisible(rootRef) && active;
  // 租约 enabled 挂页签态（而非合成的 visible）：隐藏页立即释放已持租约并停拍
  useTpsViewLease(rootRef, active);

  // —— 独立区间状态：与 Token range 完全无关（Spec §7）——
  const [viewRange, setViewRange] = useState<TpsViewRange>(TPS_DEFAULT_VIEW_RANGE);
  const [seriesIdx, setSeriesIdx] = useState(0);

  // enabled 一并吃合成的 visible：隐藏页/滚出视口时 focus refetch（staleTime 未设 ⇒ 恒过期）
  // 也不再发——spec §2.7「离开速度页或窗口隐藏后交还视图租约并停止前端轮询」
  const snapshotQuery = useTpsSnapshotQuery(selection, visible, { enabled: visible });
  const pointsQuery = useTpsPointsQuery(selection, viewRange, visible, { enabled: visible });

  // —— 刷新（读事实 + manual 点；限频只出提示）——
  const [refreshing, setRefreshing] = useState(false);
  const [refreshNote, setRefreshNote] = useState<string | null>(null);
  const [cooldown, setCooldown] = useState(false);

  const disabled = snapshotQuery.data?.disabled === true;
  const snap = disabled ? null : (snapshotQuery.data?.snapshot ?? null);
  const seriesList = snap?.series ?? [];
  const sel =
    seriesList.length > 0 ? seriesList[Math.min(seriesIdx, seriesList.length - 1)]! : null;

  const allPoints = pointsQuery.data?.points ?? [];
  const selId = sel ? tpsSeriesId(sel.seriesKey) : null;
  const selPoints = selId ? allPoints.filter((p) => tpsSeriesId(p.seriesKey) === selId) : [];
  const valuePoints = selPoints.filter((p) => p.status === "value");
  const latestPoint =
    valuePoints.length > 0
      ? valuePoints.reduce((a, b) => (b.sampledAt > a.sampledAt ? b : a))
      : null;
  const gapCount = selPoints.filter((p) => p.status === "gap").length;

  // 「现在」只在 effect 里推进（渲染保持纯函数；age 显示随 30 秒心跳粒度刷新）
  const [nowMs, setNowMs] = useState(0);
  useEffect(() => {
    setNowMs(Date.now());
    const iv = setInterval(() => setNowMs(Date.now()), 30_000);
    return () => clearInterval(iv);
  }, []);
  const lastCall = sel?.lastCall ?? null;
  // 过期口径（Spec §2「超过10分钟不再当当前值」）：后端已把 lastCall 置 null；
  // n/方法/年龄回落到范围内最近点（历史点仍可查看）。
  const endedAt =
    sel?.latestEndedAt ?? latestPoint?.latestEndedAt ?? latestPoint?.sampledAt ?? null;
  const ageMs = sel?.ageMs ?? (endedAt !== null && nowMs > 0 ? Math.max(0, nowMs - endedAt) : null);
  const factsN = lastCall !== null ? lastCall.sampleCount : (latestPoint?.sampleCount ?? null);
  const factsMethod = lastCall !== null ? lastCall.method : (latestPoint?.method ?? null);
  const staleText =
    sel === null
      ? null
      : sel.nullReason === "stale"
        ? t("usage.tps.stale")
        : t("usage.tps.noSample");

  const turn = sel?.currentTurn ?? null;
  const turnStaleText =
    sel === null
      ? null
      : sel.nullReason === "unknown-turn"
        ? t("usage.tps.turnUnknown")
        : t("usage.tps.turnNone");

  const handleRefresh = () => {
    if (refreshing || cooldown) return;
    setRefreshing(true);
    setRefreshNote(null);
    void (async () => {
      try {
        const res = await tpsRefresh(selection);
        if (res.snapshot !== null && !res.snapshot.disabled) {
          queryClient.setQueryData(tpsSnapshotKey(selection), res.snapshot);
        }
        if (!res.refreshed && res.reason !== null) {
          const key = refreshReasonKey(res.reason);
          if (key) setRefreshNote(t(key));
        } else {
          // manual 点已落库 → 曲线立即补一次读（仍是只读查询）
          await queryClient.invalidateQueries({ queryKey: [...TPS_KEY, "points"] });
        }
      } catch (e) {
        setRefreshNote(usageErrMsg(e, t));
      } finally {
        setRefreshing(false);
        // 冷却结束后解锁按钮（最小间隔 5 秒；冷却态是纯 state，渲染不读时钟）
        setCooldown(true);
        setTimeout(() => setCooldown(false), TPS_REFRESH_MIN_INTERVAL_MS);
      }
    })();
  };

  const formatTooltip = (p: (typeof valuePoints)[number]) =>
    `${formatTimeLabel(p.sampledAt)} · ${formatTpsValue(p.tps)} token/s · n=${p.sampleCount} · ${t(
      TPS_METHOD_KEYS[p.method]
    )}`;

  let body: React.ReactNode;
  if (snapshotQuery.isPending) {
    body = <p className="text-muted-foreground text-sm">{t("common.loading")}</p>;
  } else if (snapshotQuery.isError) {
    body = (
      <div role="alert" data-testid="tps-error" className="space-y-2">
        <p className="text-destructive text-sm">{usageErrMsg(snapshotQuery.error, t)}</p>
        <Button size="sm" variant="outline" onClick={() => void snapshotQuery.refetch()}>
          {t("common.retry")}
        </Button>
      </div>
    );
  } else if (disabled) {
    // 停用态只认 snapshot.disabled（Spec §2「返回disabled但不删历史」）
    body = (
      <div data-testid="tps-disabled" className="text-muted-foreground text-sm">
        {t("usage.tps.disabled")}
      </div>
    );
  } else {
    body = (
      <div className="space-y-3">
        <div className="flex flex-wrap items-center gap-1.5">
          {TPS_VIEW_RANGES.map((r) => (
            <Button
              key={r}
              size="sm"
              variant={viewRange === r ? "default" : "outline"}
              aria-pressed={viewRange === r}
              data-testid={`tps-range-${r}`}
              onClick={() => setViewRange(r)}
            >
              {t(`usage.tps.range.${r}`)}
            </Button>
          ))}
        </div>

        {seriesList.length > 1 ? (
          <div className="flex flex-wrap items-center gap-1.5">
            {seriesList.map((s, i) => (
              <Button
                key={tpsSeriesId(s.seriesKey)}
                size="sm"
                variant={sel === s ? "default" : "outline"}
                aria-pressed={sel === s}
                data-testid={`tps-series-${i}`}
                onClick={() => setSeriesIdx(i)}
              >
                {`${usageAgentLabel(s.seriesKey.tool)} · ${s.seriesKey.model}`}
              </Button>
            ))}
          </div>
        ) : null}

        {sel === null ? (
          <p className="text-muted-foreground text-sm">{t("usage.tps.currentNone")}</p>
        ) : (
          <>
            <TpsMetricsPanel
              title={t("usage.tps.lastCallTitle")}
              metrics={lastCall}
              staleText={staleText}
              valueTestId="tps-current-tps"
              staleTestId="tps-stale"
              detailPrefix="tps-detail"
              t={tFn}
            />

            {/* 事实行：最新样本结束时间 / 距今年龄 / 有效 n / 计时来源（Spec §2） */}
            <div
              className="text-muted-foreground flex flex-wrap items-center gap-x-2 gap-y-0.5 text-xs"
              data-testid="tps-facts"
            >
              <span>
                {t("usage.tps.sampleN")}{" "}
                <span data-testid="tps-last-n">{factsN === null ? EM_DASH : factsN}</span>
              </span>
              <span>·</span>
              <span data-testid="tps-method">
                {factsMethod === null ? EM_DASH : t(TPS_METHOD_KEYS[factsMethod])}
              </span>
              <span>·</span>
              <span data-testid="tps-age">{formatAgeMs(ageMs, tFn)}</span>
              <span>·</span>
              <span>
                {t("usage.tps.latestEnded")} {endedAt === null ? EM_DASH : fmtDateTime(endedAt)}
              </span>
            </div>

            <TpsMetricsPanel
              title={t("usage.tps.turnLabel")}
              metrics={turn}
              staleText={turnStaleText}
              valueTestId="tps-turn-tps"
              staleTestId="tps-turn-stale"
              detailPrefix="tps-turn"
              t={tFn}
            />

            {/* 采样曲线：x = 实际采样时间；断线由 tpsSegments 决定（gap 不画 0） */}
            {pointsQuery.isError ? (
              <div role="alert" data-testid="tps-points-error" className="space-y-1">
                <p className="text-destructive text-xs">{usageErrMsg(pointsQuery.error, t)}</p>
                <Button size="sm" variant="outline" onClick={() => void pointsQuery.refetch()}>
                  {t("common.retry")}
                </Button>
              </div>
            ) : valuePoints.length === 0 ? (
              <p className="text-muted-foreground text-sm" data-testid="tps-chart-empty">
                {t("usage.tps.chartEmpty")}
              </p>
            ) : (
              <TpsChart
                points={selPoints}
                sinceMs={pointsQuery.data?.rangeSinceMs ?? nowMs - 3_600_000}
                untilMs={Math.max(nowMs, latestPoint?.sampledAt ?? nowMs)}
                // 与 UsageTrendCard 同一版式（880×112）：宽卡下 viewBox 不被过度放大
                width={880}
                height={112}
                formatTooltip={formatTooltip}
              />
            )}
            {gapCount > 0 ? (
              <p className="text-muted-foreground text-xs" data-testid="tps-gap-note">
                {t("usage.tps.gapNote", { n: gapCount })}
              </p>
            ) : null}
          </>
        )}

        <TpsSourceStates states={snap?.sourceStates ?? []} t={tFn} />
      </div>
    );
  }

  // Card 是 React 19 函数组件（props 直传 div）：ref 合法转发，供 IntersectionObserver 观察
  return (
    <Card data-testid="tps-card" className="gap-3 py-4" ref={rootRef}>
      <CardHeader className="gap-1 px-4">
        <CardTitle>{t("usage.tps.title")}</CardTitle>
        <div className="text-muted-foreground text-xs">
          {t("usage.tps.subtitle", {
            snapshotSec: Math.round(TPS_SNAPSHOT_INTERVAL_MS / 1000),
            pointsSec: Math.round(TPS_POINTS_INTERVAL_MS / 1000),
          })}
        </div>
        <CardAction>
          <div className="flex items-center gap-1.5">
            {refreshNote ? (
              <span
                className="text-muted-foreground text-xs"
                data-testid="tps-refresh-note"
                role="status"
              >
                {refreshNote}
              </span>
            ) : null}
            <Button
              size="sm"
              variant="outline"
              disabled={refreshing || cooldown}
              data-testid="tps-refresh"
              onClick={handleRefresh}
            >
              <RefreshCw
                className={refreshing ? "h-3.5 w-3.5 animate-spin" : "h-3.5 w-3.5"}
                aria-hidden="true"
              />
              {t("usage.tps.refresh")}
            </Button>
          </div>
        </CardAction>
      </CardHeader>
      <CardContent className="px-4">{body}</CardContent>
    </Card>
  );
}
