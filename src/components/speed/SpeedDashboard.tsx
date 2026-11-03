// V2 速度页面（2026-10-26 计划 Task 12）——四页宿主「输出速度」页的唯一页体，把
// Task 9 数据层（useSpeedDashboardQuery + view.ts 纯显示状态）、Task 10 组件
// （SpeedChart / SpeedMetrics）、Task 11 管理面板（SpeedSourcesPanel）与自持的组合列表
// （SpeedSeriesList）装配成一页：左组合列表、右单图与四指标（Spec §8.1）。
//
// 驻留与门控（Task 7 宿主契约，替代过渡期 TpsCard 的角色）：
//  * 页体随宿主**常驻挂载**（四页同格堆叠 + visibility:hidden 隐藏），本地状态
//    （range/preferredMode/selection/toolFilter/search/sort/pinned/hovered/gapStyle）
//    跨页签切换保留——全部只在内存，重启不恢复悬停/固定（spec §2.6）；
//  * `active=false`（页签隐藏）：速度查询停发（无首查/无 5 秒轮询/无聚焦重查）+ TPS
//    视图租约立即释放（`useTpsViewLease(rootRef, active)`——停拍且先发一条
//    visible=false 心跳，与 TpsCard 既有语义逐字一致）；切回即恢复；
//  * 速度 range 默认 5h（2026-10-27 实机反馈 #4b），与用量 range 完全独立（键域
//    `["tps","v2"]` 不相交）。
//
// 口径（Spec §5.3）：查询携带**用户偏好** metricMode（后端按偏好排序）；实际显示模式 =
// `effectiveMetric(偏好, 选中来源能力)`——仅响应的 Codex 在 stream 偏好下临时显示
// response 并显著提示、流式按钮禁用带解释；偏好从不被 fallback 改写，切回支持该偏好的
// 组合自动恢复。asOf / rangeFrom / rangeTo 一律取服务端快照（视图不猜 now）。
// 列表排序（2026-10-27 实机反馈 #5，扩大自终审裁决 Important 1 的「临时回退例外」）：
// 列表**恒按显示口径客户端排序**（用户选的 组合名/速度/会话数 × 升降序）——fallback 时
// 显示口径 ≠ 查询口径的问题由此自然消解；查询载荷的 `sort` 恒发 `"tool"`，只作服务端
// 稳定基序（后端排序语义不改，wire 兼容）。
//
// 滚动窗口裁决（2026-10-26）：`rangeTo(=asOf)` 每 5 秒随轮询前移**不算切换**——图表的
// 固定/悬停自动解除只看 `clearSignalKey = mode:range:series`（SpeedChart 加成 prop），
// 固定读数快照本就不随 refetch 漂移。切换范围/模式/组合时父层显式清 pinned+hovered。
import { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { WorkspacePageHeader } from "@/components/common/WorkspacePageHeader";
import { Button } from "@/components/ui/button";
import { SpeedChart, type PinnedBucket } from "@/components/speed/SpeedChart";
import { SpeedMetrics, type SpeedMetricsContext } from "@/components/speed/SpeedMetrics";
import { SpeedSeriesList } from "@/components/speed/SpeedSeriesList";
import { SpeedSourcesPanel } from "@/components/speed/SpeedSourcesPanel";
import { useSpeedDashboardQuery } from "@/lib/query/queries/speed";
import { useTpsViewLease } from "@/lib/query/queries/tps";
import {
  defaultDirectionFor,
  effectiveMetric,
  seriesRowLabel,
  seriesKeyId,
  sortSeries,
  translateSpeedReason,
} from "@/lib/speed/view";
import type { SpeedListSortKey, SortDirection } from "@/lib/speed/view";
import type {
  SeriesKey,
  SpeedBucket,
  SpeedCapabilities,
  SpeedDashboardSnapshot,
  SpeedMetricMode,
  SpeedRange,
  SpeedSort,
} from "@/types/speed";

/** 档位按钮顺序（Spec §7：15m/1h/5h/24h，默认 5h——2026-10-27 实机反馈 #4b）。 */
const SPEED_RANGES: SpeedRange[] = ["15m", "1h", "5h", "24h"];

/** 查询载荷的服务端排序基序（恒 tool = tool→model→channel 稳定序；列表排序在客户端做）。 */
const SERVER_SORT: SpeedSort = "tool";

/** 能力位缺省（来源视图缺席时按双能力对待——后端是唯一真值源，这里只兜底不猜测）。 */
const BOTH_CAPABILITIES: SpeedCapabilities = {
  stream: true,
  response: true,
  streamReason: null,
  responseReason: null,
};

/** 选中来源的能力位（来源视图缺席 ⇒ 双能力兜底；不猜测、不按工具名硬编码）。 */
function capabilitiesOf(
  key: SeriesKey | null,
  snap: SpeedDashboardSnapshot | null
): SpeedCapabilities {
  if (key === null || snap === null) return BOTH_CAPABILITIES;
  return snap.sourceStates.find((st) => st.source === key.tool)?.capabilities ?? BOTH_CAPABILITIES;
}

function sameSeriesKey(a: SeriesKey | null, b: SeriesKey | null): boolean {
  if (a === null || b === null) return a === b;
  return seriesKeyId(a) === seriesKeyId(b);
}

export function SpeedDashboard({ active }: { active: boolean }) {
  const { t } = useTranslation();
  const rootRef = useRef<HTMLDivElement | null>(null);
  // TPS 视图租约（Task 7 门控口径，与 TpsCard 逐字同语义）：enabled=false = 停拍且先释放
  useTpsViewLease(rootRef, active);

  // —— 页面本地状态（全部内存态；跨页签保留，重启不恢复悬停/固定）——
  const [range, setRange] = useState<SpeedRange>("5h");
  const [preferredMode, setPreferredMode] = useState<SpeedMetricMode>("stream");
  const [selection, setSelection] = useState<SeriesKey | null>(null);
  const [toolFilter, setToolFilter] = useState<string | null>(null);
  // 列表客户端排序（实机反馈 #5）：组合名升序起步；切键归位该键默认方向
  const [listSort, setListSort] = useState<SpeedListSortKey>("name");
  const [sortDir, setSortDir] = useState<SortDirection>("asc");
  const [pinned, setPinned] = useState<PinnedBucket | null>(null);
  const [hovered, setHovered] = useState<SpeedBucket | null>(null);
  const [gapStyle, setGapStyle] = useState<"dash" | "band">("dash");
  const [sourcesOpen, setSourcesOpen] = useState(false);

  // —— 数据层（Task 9）：查询携带用户偏好；active 门控轮询与首查；sort 恒发服务端基序 ——
  const query = useMemo(
    () => ({
      range,
      selection,
      metricMode: preferredMode,
      toolId: toolFilter,
      search: "",
      sort: SERVER_SORT,
    }),
    [range, selection, preferredMode, toolFilter]
  );
  const speedQuery = useSpeedDashboardQuery(query, { active });
  const snapshot = speedQuery.data ?? null;
  // keepPreviousData 语义（视图层自持，不改查询层）：换键 pending 期间继续展示上一份
  // 快照（数据陈旧但结构完整），避免列表/图表在切范围/排序/键入搜索时闪空。
  // 用「渲染期调整 state」（React 文档认可的模式；identity 守卫保证收敛）——不用 ref：
  // ref 在渲染期读写被 react-hooks/refs 禁止，且 effect 方案会晚一帧、闪空列表。
  const [lastGood, setLastGood] = useState<SpeedDashboardSnapshot | null>(null);
  if (snapshot !== null && snapshot !== lastGood) setLastGood(snapshot);
  const shown = snapshot ?? lastGood;
  const serverSelected = snapshot?.selectedSeries ?? null;
  /** 显示用的服务端选中：`shown` 已含「有快照用快照、换键 pending 退上一份」语义。 */
  const shownServerSelected = shown?.selectedSeries ?? null;

  // 本地 selection 被筛掉 ⇒ 跟随服务端回退（后端 selected_series 同款回退是显示真值源）；
  // selection=null（未选择）不回写——保持「让服务端选默认」的语义，避免多余换键重查。
  useEffect(() => {
    if (snapshot === null || selection === null || serverSelected === null) return;
    if (!sameSeriesKey(selection, serverSelected)) setSelection(serverSelected);
  }, [snapshot, selection, serverSelected]);

  /**
   * 展示真值源：本地 selection 优先（点击行 → 立即生效，不等新快照；Codex fallback
   * 因此瞬时翻转）；未选择时跟随服务端选中（含回退）。sync effect 保证二者最终一致。
   */
  const displayKey = selection ?? shownServerSelected;

  const selectedCapabilities = capabilitiesOf(displayKey, shown);
  const mode = effectiveMetric(preferredMode, selectedCapabilities);
  const fallbackActive = mode !== preferredMode;

  // 列表恒客户端排序（2026-10-27 实机反馈 #5，用户裁决；扩大自终审裁决 Important 1
  // 的「临时回退例外」）：列表按**显示口径** `sortSeries(rawSeries, listSort, mode, sortDir)`
  // 排序——查询载荷的 `sort:"tool"` 只作服务端稳定基序（后端排序语义不改）。这样 fallback
  // （effective ≠ preferred，如 Codex）下列表速度列与排序口径天然一致，不再有「服务端按
  // 偏好口径排序、列表显示回退口径」的 visibly 错排问题。不用 useMemo：React Compiler 的
  // preserve-manual-memoization 会拒绝本组件的手动记忆化（把 const 原语 `mode` 误报为
  // 「dependency may be modified later」导致整组件跳过优化）；直排计算绕开该分析——
  // 组合数量级的小数组排序逐渲染重算可忽略。
  const rawSeries = shown?.series ?? [];
  const listSeries = sortSeries(rawSeries, listSort, mode, sortDir);

  const selectedSummary = useMemo(
    () =>
      displayKey && shown
        ? (shown.series.find((s) => sameSeriesKey(s.seriesKey, displayKey)) ?? null)
        : null,
    [displayKey, shown]
  );

  // 滚动窗口裁决：固定/悬停自动解除只看 mode:range:series（from/to 随 asOf 前移不算切换）
  const clearSignalKey = `${mode}:${range}:${displayKey ? seriesKeyId(displayKey) : "none"}`;

  // —— 交互（切换范围/模式/组合 = Spec §8.3「解除固定」，父层与图表哨兵双保险）——
  const switchRange = (r: SpeedRange) => {
    setRange(r);
    setPinned(null);
    setHovered(null);
  };
  const switchMode = (m: SpeedMetricMode) => {
    if (m === preferredMode) return;
    setPreferredMode(m);
    setPinned(null);
    setHovered(null);
  };
  const selectSeries = (key: SeriesKey) => {
    setSelection(key);
    setPinned(null);
    setHovered(null);
  };
  // 切排序键 = 归位该键默认方向（组合名升序 / 速度·会话数降序）；方向按钮只翻转
  const switchListSort = (key: SpeedListSortKey) => {
    if (key === listSort) return;
    setListSort(key);
    setSortDir(defaultDirectionFor(key));
  };
  const toggleSortDir = () => setSortDir((d) => (d === "asc" ? "desc" : "asc"));
  const clearPin = () => {
    setPinned(null);
    setHovered(null);
  };

  const toolOptions = useMemo(
    () => (shown ? shown.sourceStates.map((st) => st.source) : []),
    [shown]
  );

  // —— 详情区（右列）——
  let detail: React.ReactNode;
  if (speedQuery.isPending && shown === null) {
    detail = (
      <p className="text-muted-foreground text-sm" data-testid="speed-detail-loading">
        {t("common.loading")}
      </p>
    );
  } else if (speedQuery.isError && shown === null) {
    detail = (
      <div role="alert" className="space-y-2" data-testid="speed-detail-error">
        <p className="text-destructive text-sm">{t("speed.fetchError")}</p>
        <Button size="sm" variant="outline" onClick={() => void speedQuery.refetch()}>
          {t("common.retry")}
        </Button>
      </div>
    );
  } else if (shown?.disabled === true) {
    detail = (
      <p className="text-muted-foreground text-sm" data-testid="speed-disabled">
        {t("speed.disabled")}
      </p>
    );
  } else if (!displayKey || !selectedSummary || shown === null) {
    detail = (
      <p className="text-muted-foreground text-sm" data-testid="speed-detail-empty">
        {t("speed.detail.empty")}
      </p>
    );
  } else {
    // —— 四格读数：固定快照 > 悬停桶 > 区间汇总（Spec §8.3）——
    // 三路取数（固定深拷贝 / 悬停桶 / 区间汇总）类型层均不可空，读数恒非 null。
    // 只在本分支消费，故就地声明：selectedSummary 已被上方守卫收窄为非空。
    let metricsContext: SpeedMetricsContext = { kind: "range-summary" };
    let displayedMetrics = selectedSummary.metrics;
    if (pinned) {
      metricsContext = {
        kind: "bucket",
        startMs: pinned.startMs,
        endMs: pinned.endMs,
        bucketWidthMs: pinned.endMs - pinned.startMs,
        pinned: true,
      };
      displayedMetrics = pinned.metricsSnapshot;
    } else if (hovered) {
      metricsContext = {
        kind: "bucket",
        startMs: hovered.startMs,
        endMs: hovered.endMs,
        bucketWidthMs: hovered.endMs - hovered.startMs,
      };
      displayedMetrics = hovered.metrics;
    }
    detail = (
      <div className="flex flex-col gap-3" data-testid="speed-detail">
        {/* 组合标识可选框（第四轮实机反馈）：下拉即切换选中组合（同列表点击），选项
            文本 = 完整组合名（含渠道）；渠道/方法短标记随选项并入、近似徽标留框外 */}
        <div className="flex min-w-0 flex-wrap items-center gap-2">
          <select
            data-testid="speed-detail-title"
            aria-label={t("speed.detail.seriesPicker")}
            value={displayKey ? seriesKeyId(displayKey) : ""}
            onChange={(e) => {
              const picked = shown?.series.find((s) => seriesKeyId(s.seriesKey) === e.target.value);
              if (picked) selectSeries(picked.seriesKey);
            }}
            className="border-input bg-background max-w-full rounded-md border px-2 py-1 text-base font-semibold"
          >
            {(shown?.series ?? []).map((s) => {
              const id = seriesKeyId(s.seriesKey);
              return (
                <option key={id} value={id}>
                  {seriesRowLabel(s.seriesKey)}
                </option>
              );
            })}
            {/* 显示键不在当前快照（换键 pending 的 keepPreviousData 尾窗）→ 补一个守卫选项，
                避免 select 值悬空显示空白 */}
            {displayKey && !shown?.series.some((s) => sameSeriesKey(s.seriesKey, displayKey)) ? (
              <option value={seriesKeyId(displayKey)}>{seriesRowLabel(displayKey)}</option>
            ) : null}
          </select>
          {displayKey?.method === "log-boundary-approx" ? (
            <span className="border-border/60 text-muted-foreground rounded border px-1 text-[10px]">
              {t("speed.tile.approxMark")}
            </span>
          ) : null}
        </div>

        {/* 口径行：模式切换（speed-mode 根 = 全界面同步的唯一驱动）+ 显著 fallback 提示 */}
        <div className="flex flex-wrap items-center gap-2">
          <div data-testid="speed-mode" data-mode={mode} role="group" className="flex gap-1.5">
            <Button
              size="sm"
              variant={mode === "stream" ? "default" : "outline"}
              aria-pressed={mode === "stream"}
              data-testid="speed-mode-stream"
              disabled={!selectedCapabilities.stream}
              title={
                selectedCapabilities.stream
                  ? t("speed.modeHint.stream") // 口径定义 tooltip（实机反馈 #4c）
                  : selectedCapabilities.streamReason
                    ? translateSpeedReason(t, selectedCapabilities.streamReason)
                    : t("speed.codexFallback")
              }
              onClick={() => switchMode("stream")}
            >
              {t("speed.mode.stream")}
            </Button>
            <Button
              size="sm"
              variant={mode === "response" ? "default" : "outline"}
              aria-pressed={mode === "response"}
              data-testid="speed-mode-response"
              title={t("speed.modeHint.response")} // 口径定义 tooltip（实机反馈 #4c）
              onClick={() => switchMode("response")}
            >
              {t("speed.mode.response")}
            </Button>
          </div>
          {fallbackActive ? (
            <p
              role="status"
              data-testid="speed-fallback-notice"
              className="border-border/60 bg-muted/40 text-foreground rounded-md border px-2 py-1 text-xs"
            >
              {t("speed.codexFallback")} · {t("speed.tile.approxMark")}
            </p>
          ) : null}
        </div>
        {/* 口径说明已整体迁入图表脚注（二轮实机反馈 ④）：SpeedChart 的 footnote 三行，
            渲染在图例下方——详情区不再有独立 speed-mode-hint 段落；按钮 title 保留 */}

        {/* 范围 + 缺口样式（范围默认 5h；与用量范围完全独立） */}
        <div className="flex flex-wrap items-center gap-1.5">
          {SPEED_RANGES.map((r) => (
            <Button
              key={r}
              size="sm"
              variant={range === r ? "default" : "outline"}
              aria-pressed={range === r}
              data-testid={`speed-range-${r}`}
              onClick={() => switchRange(r)}
            >
              {t(`speed.range.${r}`)}
            </Button>
          ))}
          <label className="text-muted-foreground ml-auto flex items-center gap-1 text-xs">
            {t("speed.gapStyle.label")}
            <select
              value={gapStyle}
              data-testid="speed-gap-style"
              aria-label={t("speed.gapStyle.label")}
              onChange={(e) => setGapStyle(e.target.value as "dash" | "band")}
              className="border-border bg-background text-foreground rounded-md border px-2 py-1 text-xs"
            >
              <option value="dash">{t("speed.gapStyle.dash")}</option>
              <option value="band">{t("speed.gapStyle.band")}</option>
            </select>
          </label>
        </div>

        {/* 区间汇总四格 / 悬停与固定的桶读数（同一 SpeedMetrics，上下文行区分读数来源） */}
        <SpeedMetrics
          metrics={displayedMetrics}
          mode={mode}
          series={{
            tool: displayKey.tool,
            model: displayKey.model,
            channel: displayKey.channel,
            method: displayKey.method,
          }}
          context={metricsContext}
          onClearPin={clearPin}
        />

        {/* 整页唯一一张图（x 轴按服务端快照的完整 [rangeFrom, rangeTo] 铺满）；
            口径说明三行走 footnote（二轮实机反馈 ④：图例下方，替代原 speed-mode-hint 段落） */}
        <SpeedChart
          buckets={shown.buckets}
          mode={mode}
          from={shown.rangeFrom}
          to={shown.rangeTo}
          onHover={setHovered}
          onPin={setPinned}
          pinned={pinned}
          gapStyle={gapStyle}
          clearSignalKey={clearSignalKey}
          seriesKey={displayKey}
          footnote={
            <>
              <p>{t("speed.modeHint.line1")}</p>
              <p>{t("speed.modeHint.line2")}</p>
              <p>{t("speed.modeHint.line3")}</p>
            </>
          }
        />
      </div>
    );
  }

  return (
    <div
      ref={rootRef}
      data-testid="speed-dashboard"
      className="flex min-h-0 flex-1 flex-col gap-3 overflow-y-auto p-4"
    >
      <WorkspacePageHeader
        title={t("workspace.page.speed")}
        actions={
          <Button
            size="sm"
            variant="outline"
            data-testid="speed-sources-open"
            onClick={() => setSourcesOpen(true)}
          >
            {t("speed.sources.title")}
          </Button>
        }
      />

      {/* 已有数据时的增量取数失败：局部可重试提示，已加载数据不清空（brief Step3） */}
      {speedQuery.isError && shown !== null ? (
        <p role="status" className="text-destructive text-xs" data-testid="speed-fetch-error">
          {t("speed.fetchError")}{" "}
          <button
            type="button"
            className="border-border hover:bg-accent rounded-md border px-2 py-0.5"
            onClick={() => void speedQuery.refetch()}
          >
            {t("common.retry")}
          </button>
        </p>
      ) : null}

      {/* 左列轨加宽保底（实机反馈 #5：minmax(280px,340px)，组合名少截断）。
          三轮实机反馈 ③（2026-10-27）：整页只滚一处——根容器 overflow-y-auto，详情区
          不再内滚（图表上下完整可见）；宽窗左列 lg:sticky 自滚（见 SpeedSeriesList）。
          二轮实机反馈 ③（2026-10-27）：窄窗（<lg）详情在上、列表在下——**DOM 顺序不动**
          （列表仍是第一子元素），视觉顺序交给 grid order：列表 order-2 / 详情 order-1，
          宽窗 lg:order-1 / lg:order-2 恢复左列表、右详情 */}
      <div className="grid gap-3 lg:grid-cols-[minmax(280px,340px)_minmax(0,1fr)]">
        <SpeedSeriesList
          className="order-2 lg:order-1"
          series={listSeries}
          mode={mode}
          selectedKey={displayKey}
          onSelect={selectSeries}
          toolId={toolFilter}
          toolOptions={toolOptions}
          onToolFilterChange={setToolFilter}
          sortKey={listSort}
          sortDirection={sortDir}
          onSortKeyChange={switchListSort}
          onToggleSortDirection={toggleSortDir}
        />
        <section className="order-1 lg:order-2" aria-label={t("workspace.page.speed")}>
          {detail}
        </section>
      </div>

      {/* 采集管理（Task 11 面板，宿主注入快照来源；面板自身零轮询零租约） */}
      <SpeedSourcesPanel
        open={sourcesOpen}
        onOpenChange={setSourcesOpen}
        sources={shown?.sourceStates ?? []}
        fetchState={speedQuery.isError ? "error" : speedQuery.isPending ? "loading" : "ready"}
        onRetry={() => void speedQuery.refetch()}
      />
    </div>
  );
}

export default SpeedDashboard;
