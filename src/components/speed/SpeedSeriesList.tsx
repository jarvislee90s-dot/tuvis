// V2 速度组合列表（2026-10-26 计划 Task 12；2026-10-27 实机反馈 #4a/#5 修订、三轮反馈 #3 再修订，Spec §8.1–8.2）：
// 工具筛选、点表头按 组合名/速度/会话数 升降序排序；行序由 SpeedDashboard 以
// 显示口径 `sortSeries(…, sortKey, mode, direction)` 客户端排好后传入（**列表恒客户端排序**，
// 服务端 `sort:"tool"` 只是查询载荷里的稳定基序——Task 12「服务端直出」纪律由实机反馈轮
// 裁决扩大为客户端排序，见 SpeedDashboard 的 listSeries 注释）。
//
// 纪律：
//  * 速度列 = `rateForMode(metrics, mode)`（null 如实 —，绝不伪装 0）；小样本按
//    `isSmallSample` 标短标记（沿既有 TPS 小输出阈值口径）；
//  * 会话数 = `metrics.sessionCount`，列头「有依据的会话数」（Spec §8.2——不称当前
//    打开窗口数）；Codex（method=log-boundary-approx）行带「日志近似」短标记；
//  * 选中态用选中 token（`bg-selected`）+ 非颜色指示（● 标记 + `aria-current`），
//    不靠单色相区分；
//  * 窄宽布局**共用同一份行表 DOM**（2026-10-27 裁决，替代原「窄窗收成下拉选择器」；
//    三轮反馈改：整页只滚一处——<lg 时列表置于详情下方限高滚动（max-h-56），
//    ≥lg 时左列 sticky 自滚（lg:max-h-[calc(100vh-190px)]），右列/图表随页滚不再内滚）；
//    名单元格悬浮即时显示全名（自绘 group-hover 提示）；工具筛选两布局都在，排序走表头；
//  * 空态只有「无任何组合」一种（搜索行已删除）。
import type { ReactElement } from "react";
import { useTranslation } from "react-i18next";
import { cn } from "@/lib/utils";
import { formatRateValue, unitForMode } from "@/lib/speed/chart";
import { isSmallSample, rateForMode, seriesRowLabel, seriesKeyId } from "@/lib/speed/view";
import type { SpeedListSortKey, SortDirection } from "@/lib/speed/view";
import { usageAgentLabel } from "@/lib/usage/format";
import type { SeriesKey, SpeedMetricMode, SpeedSeriesSummary } from "@/types/speed";

export interface SpeedSeriesListProps {
  /** 客户端排序后的 series（SpeedDashboard 以显示口径排好后传入，本组件不再加工） */
  series: SpeedSeriesSummary[];
  mode: SpeedMetricMode;
  selectedKey: SeriesKey | null;
  onSelect: (key: SeriesKey) => void;
  toolId: string | null;
  /** 工具筛选选项（来自快照 sourceStates——筛选态下列表只剩单工具，不能从列表反推全集） */
  toolOptions: string[];
  onToolFilterChange: (v: string | null) => void;
  /** 客户端排序键（组合名/速度/会话数）与方向；切键归位默认方向由父层负责 */
  sortKey: SpeedListSortKey;
  sortDirection: SortDirection;
  onSortKeyChange: (v: SpeedListSortKey) => void;
  onToggleSortDirection: () => void;
  /**
   * 追加到根元素的类（可选；二轮实机反馈 ③，2026-10-27）：父层栅格用它注入 `order` 类——
   * 窄窗（<lg）详情在上、列表在下（`order-2`），宽窗恢复左列表（`lg:order-1`）。
   * DOM 顺序不动（列表仍是第一子元素），视觉顺序交给 grid order。
   */
  className?: string;
}

const SELECT_CLASS =
  "border-border bg-background text-foreground rounded-md border px-2 py-1 text-xs";

/** 行右侧两列（速度/会话数）与表头共用的栅格模板。 */
const ROW_GRID = "grid grid-cols-[minmax(0,1fr)_4.5rem_4.5rem] items-center gap-2";

/**
 * 行内 testid 后缀（终审 Minor 5）：行级 testid 不能用 `tool`（同工具多组合会撞名），
 * 改由 `seriesKeyId` 派生；但它是含 `"`/`{`/`}`/`:` 的 JSON 串，直接拼进 testid 会让
 * querySelector 派生用法脆弱——取其 FNV-1a 32 位 hex 摘要（8 位 `[0-9a-f]`，不含任何
 * 选择器敏感字符；同一 series 跨快照/跨重启逐字稳定，不同 series 撞摘要在组合数量级
 * 下概率可忽略）。**安全化选择 = 固定 FNV-1a hash**（而非 encodeURIComponent：
 * 后者会把 testid 撑成长百分号串，可读性差且转义表依赖实现细节）。
 */
function seriesTestSuffix(key: SeriesKey): string {
  const id = seriesKeyId(key);
  let hash = 0x811c9dc5;
  for (let i = 0; i < id.length; i++) {
    hash ^= id.charCodeAt(i);
    hash = Math.imul(hash, 0x01000193);
  }
  return (hash >>> 0).toString(16).padStart(8, "0");
}

export function SpeedSeriesList({
  series,
  mode,
  selectedKey,
  onSelect,
  toolId,
  toolOptions,
  onToolFilterChange,
  sortKey,
  sortDirection,
  onSortKeyChange,
  onToggleSortDirection,
  className,
}: SpeedSeriesListProps): ReactElement {
  const { t } = useTranslation();
  const unit = unitForMode(mode, t);
  const selectedId = selectedKey ? seriesKeyId(selectedKey) : null;

  return (
    <aside
      className={cn("flex min-h-0 flex-col gap-2", className)}
      data-testid="speed-list-pane"
      aria-label={t("speed.list.listLabel")}
    >
      {/* 筛选控件（宽窄布局都在）；排序改由表头点击驱动（三轮实机反馈 ③） */}
      <div className="flex flex-wrap items-center gap-1.5">
        <select
          value={toolId ?? ""}
          data-testid="speed-tool-filter"
          aria-label={t("speed.list.toolFilter")}
          onChange={(e) => onToolFilterChange(e.target.value === "" ? null : e.target.value)}
          className={SELECT_CLASS}
        >
          <option value="">{t("speed.list.toolAll")}</option>
          {toolOptions.map((tool) => (
            <option key={tool} value={tool}>
              {usageAgentLabel(tool)}
            </option>
          ))}
        </select>
      </div>

      {/* 空态：无任何组合 */}
      {series.length === 0 ? (
        <p className="text-muted-foreground px-2 py-3 text-xs" data-testid="speed-list-empty-all">
          {t("speed.list.emptyAll")}
        </p>
      ) : null}

      {/* 行表（两布局同一份 DOM）：<lg 置于详情上方限高滚动；≥lg 左列铺满独立滚动 */}
      <div
        className="flex max-h-56 flex-col overflow-y-auto lg:sticky lg:top-0 lg:max-h-[calc(100vh-190px)]"
        data-testid="speed-series-list"
      >
        <div
          className={`text-muted-foreground ${ROW_GRID} px-2 pb-1 text-[11px]`}
          data-testid="speed-list-header"
        >
          {(
            [
              ["name", t("speed.list.colSeries"), "left"],
              ["rate", `${t("speed.list.colRate")}（${unit}）`, "right"],
              ["sessions", t("speed.list.colSessions"), "right"],
            ] as const
          ).map(([key, label, align]) => {
            const active = sortKey === key;
            return (
              <button
                key={key}
                type="button"
                data-testid={`speed-sort-header-${key}`}
                aria-sort={active ? (sortDirection === "asc" ? "ascending" : "descending") : "none"}
                title={t("speed.list.sortHeaderTitle")}
                onClick={() => (active ? onToggleSortDirection() : onSortKeyChange(key))}
                className={cn(
                  "hover:text-foreground flex items-center gap-0.5",
                  align === "right" && "flex-row-reverse justify-end",
                  active && "text-foreground font-medium"
                )}
              >
                {label}
                {active ? (
                  <span aria-hidden="true">{sortDirection === "asc" ? "↑" : "↓"}</span>
                ) : null}
              </button>
            );
          })}
        </div>
        {series.map((s) => {
          const id = seriesKeyId(s.seriesKey);
          const selected = id === selectedId;
          const rate = rateForMode(s.metrics, mode);
          return (
            <button
              key={id}
              type="button"
              data-testid="speed-series-row"
              data-tool={s.seriesKey.tool}
              data-selected={selected ? "true" : "false"}
              aria-current={selected ? "true" : undefined}
              onClick={() => onSelect(s.seriesKey)}
              className={`text-foreground ${ROW_GRID} w-full rounded-md px-2 py-1.5 text-left text-xs ${
                selected ? "bg-selected text-selected-foreground" : "hover:bg-accent"
              }`}
            >
              <span className="flex min-w-0 items-center gap-1">
                {/* 非颜色选中指示：● 只在选中行出现 */}
                <span aria-hidden="true" className={selected ? "" : "invisible"}>
                  ●
                </span>
                {/* 悬浮即时显示完整组合名（第四轮实机反馈）：原生 title 延迟 1~2s 不可配，
                    改纯 CSS group-hover 自绘提示（无延迟）；aria-label 保住可访问全名 */}
                <span className="group relative min-w-0" aria-label={seriesRowLabel(s.seriesKey)}>
                  {/* block 必须在：truncate（overflow/ellipsis）对 inline 盒不生效——
                      缺它整行全名会溢出包装盒、横穿「日志近似」徽标（四轮回归，实机截图坐实） */}
                  <span className="block min-w-0 truncate font-medium">
                    {seriesRowLabel(s.seriesKey)}
                  </span>
                  <span
                    role="tooltip"
                    className="bg-popover text-popover-foreground border-border pointer-events-none absolute bottom-full left-0 z-20 mb-1 hidden max-w-96 truncate rounded-md border px-2 py-1 text-xs whitespace-nowrap shadow-md group-hover:block"
                  >
                    {seriesRowLabel(s.seriesKey)}
                  </span>
                </span>
                {s.seriesKey.method === "log-boundary-approx" ? (
                  <span className="border-border/60 text-muted-foreground shrink-0 rounded border px-1 text-[10px]">
                    {t("speed.tile.approxMark")}
                  </span>
                ) : null}
              </span>
              <span
                className="text-right tabular-nums"
                data-testid={`speed-rate-${seriesTestSuffix(s.seriesKey)}`}
              >
                {formatRateValue(rate.value)}
                {isSmallSample(rate) ? (
                  <span className="text-muted-foreground ml-1 text-[10px]">
                    {t("speed.list.smallSample")}
                  </span>
                ) : null}
              </span>
              <span
                className="text-muted-foreground text-right tabular-nums"
                data-testid={`speed-sessions-${seriesTestSuffix(s.seriesKey)}`}
              >
                {s.metrics.sessionCount}
              </span>
            </button>
          );
        })}
      </div>
    </aside>
  );
}

export default SpeedSeriesList;
