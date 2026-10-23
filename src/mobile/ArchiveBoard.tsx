import { useCallback, useEffect, useState } from "react";
import i18n from "@/i18n";
import {
  deleteArchivedSession,
  fetchArchivedSessions,
  type ArchivedPayload,
  type ArchivedSession,
} from "./api";
import { chipLabel, filterArchivedByProject, filterArchivedByTool } from "./archive-logic";
// 复用既有导出（勿自造）：formatRelativeTime 相对时间
import { formatRelativeTime } from "./board-logic";

type Days = 1 | 3 | 7;

/** 空态文案：无筛选时按当前天数给扩窗引导（7 天已到顶 → 暂无）；有筛选残留
 * （常见：选定项目后切小窗，新窗口不含该项目）——扩窗文案会误导（扩了也没有），
 * 改显筛选语义（评审 Minor）。文案 M5 双语键现取 */
function emptyStateText(days: Days, tool: string, project: string): string {
  if (tool !== "all" || project !== "all") return i18n.t("mobile.archive.filterNoMatch");
  if (days === 7) return i18n.t("mobile.archive.empty");
  if (days === 3) return i18n.t("mobile.archive.empty3d");
  return i18n.t("mobile.archive.empty1d");
}

/** 历史会话页（spec §7.2）：懒加载（进页 days=1，切天数重拉，页内不轮询——
 *  死数据静态）；双维筛选（工具 chips × 项目下拉）独立于活板选择；卡片无按钮
 *  （裁决 6——打开动作只在详情页）。 */
export default function ArchiveBoard({
  onBack,
  onOpenCard,
  onUnpaired,
}: {
  onBack: () => void;
  onOpenCard: (s: ArchivedSession) => void;
  /** 403（配对态在历史页内过期）→ 透传 App 置 paired=false 回配对页（Board 同款） */
  onUnpaired: () => void;
}) {
  const [days, setDays] = useState<Days>(1);
  const [data, setData] = useState<ArchivedPayload | null>(null);
  const [error, setError] = useState(false);
  // 清空归档失败的行内反馈（终审 Finding 3：Promise 拒绝无 .catch → 按钮无反应 +
  // unhandled rejection）。不复用 error（那是加载失败语义，会带出 retry 按钮误导）；
  // 下次操作或刷新时清除
  const [manageError, setManageError] = useState(false);
  const [tool, setTool] = useState<string>("all");
  const [project, setProject] = useState<string>("all");
  // 相对时长基准时钟（react-hooks/purity 禁渲染期调 Date.now，同 Board 惯例）：
  // 挂载时取一次快照。历史页是死数据静态页（页内不轮询），时长冻结在进页时刻即可
  const [now] = useState(() => Date.now());

  const load = useCallback(
    async (d: Days) => {
      setError(false);
      try {
        const p = await fetchArchivedSessions(d);
        if (p === null) {
          // 403：配对态在本页过期 → 透传 App 回配对页（评审 Minor：原静默吞掉，
          // 用户停在历史页假活状态）
          onUnpaired();
          return;
        }
        setData(p);
      } catch {
        setError(true);
      }
    },
    [onUnpaired]
  );

  useEffect(() => {
    void load(days);
  }, [days, load]);

  const rows = data
    ? filterArchivedByProject(filterArchivedByTool(data.archived, tool), project)
    : [];
  // chips = 「全部」+ 当前窗口出现过的工具（动态——与活板 chips 的受管∩有卡不同，
  // 归档以结果集为准）
  const tools = ["all", ...new Set((data?.archived ?? []).map((s) => s.agentType))];

  return (
    <div className="mx-auto max-w-3xl px-4">
      <header className="mb-3 flex items-center justify-between pt-4">
        <div className="flex items-center gap-2">
          <button
            type="button"
            onClick={onBack}
            className="text-sm text-[var(--mut)]"
            aria-label={i18n.t("mobile.archive.backAria")}
          >
            {i18n.t("mobile.archive.back")}
          </button>
          <h1 className="text-lg font-semibold">{i18n.t("mobile.archive.title")}</h1>
        </div>
        <span className="flex items-center gap-2 text-xs text-[var(--mut)]">
          <button
            type="button"
            data-testid="archive-refresh"
            className="underline"
            onClick={() => {
              setManageError(false);
              void load(days);
            }}
          >
            {i18n.t("mobile.archive.refresh")}
          </button>
          <button
            type="button"
            data-testid="archive-clear"
            className="underline"
            onClick={() => {
              if (!window.confirm(i18n.t("mobile.archive.clearConfirm"))) return;
              setManageError(false);
              deleteArchivedSession()
                .then(() => void load(days))
                .catch(() => setManageError(true));
            }}
          >
            {i18n.t("mobile.archive.clear")}
          </button>
        </span>
      </header>

      {/* 清空归档失败反馈（终审 Finding 3）：小号红字贴近操作点，不触发 retry */}
      {manageError && (
        <p data-testid="archive-manage-error" className="mb-2 text-right text-xs text-red-600">
          {i18n.t("mobile.archive.manageError")}
        </p>
      )}

      {/* 第一行：工具 chips（文案复用 archive-logic.chipLabel → mobile.tool.* 键；选中态样式从简，品牌色后续按需） */}
      <div className="mb-2 flex gap-2 overflow-x-auto">
        {tools.map((t) => (
          <button
            key={t}
            type="button"
            onClick={() => setTool(t)}
            className={`rounded-full px-3 py-1 text-xs ${t === tool ? "bg-[var(--btnp)] text-[var(--btnpt)]" : "bg-[var(--cb)] text-[var(--tx)]"}`}
          >
            {chipLabel(t)}
          </button>
        ))}
      </div>

      {/* 第二行：项目下拉 + 天数分段 */}
      <div className="mb-3 flex gap-2">
        <select
          value={project}
          onChange={(e) => setProject(e.target.value)}
          className="min-w-0 flex-1 rounded-lg border border-[var(--cb)] px-2 py-1.5 text-sm"
          aria-label={i18n.t("mobile.archive.projectFilterAria")}
        >
          <option value="all">{i18n.t("mobile.archive.projectAll")}</option>
          {(data?.projects ?? []).map((p) => (
            <option key={p} value={p}>
              {p}
            </option>
          ))}
        </select>
        <span className="flex gap-1">
          {([1, 3, 7] as Days[]).map((d) => (
            <button
              key={d}
              type="button"
              data-testid={`archive-days-${d}`}
              onClick={() => setDays(d)}
              className={`rounded-lg px-2.5 py-1.5 text-xs ${d === days ? "bg-[var(--btnp)] text-[var(--btnpt)]" : "bg-[var(--cb)] text-[var(--tx)]"}`}
            >
              {i18n.t("mobile.archive.days", { d })}
            </button>
          ))}
        </span>
      </div>

      {error && (
        <div className="py-8 text-center text-sm text-[var(--mut)]">
          {i18n.t("mobile.archive.loadFailed")}
          <button
            type="button"
            data-testid="archive-retry"
            className="ml-2 underline"
            onClick={() => void load(days)}
          >
            {i18n.t("mobile.archive.retry")}
          </button>
        </div>
      )}
      {!error && data === null && (
        <p className="py-8 text-center text-sm text-[var(--mut)]">
          {i18n.t("mobile.detail.loading")}
        </p>
      )}
      {!error && data && rows.length === 0 && (
        <p className="py-8 text-center text-sm text-[var(--mut)]">
          {emptyStateText(days, tool, project)}
        </p>
      )}
      <div className="flex flex-col gap-2 pb-8">
        {rows.map((s) => (
          <button
            key={s.sessionId}
            type="button"
            onClick={() => onOpenCard(s)}
            className="rounded-xl border border-[var(--cb)] px-3 py-2.5 text-left enabled:hover:bg-[var(--cbg)]"
          >
            <div className="flex items-baseline justify-between">
              <span className="flex min-w-0 items-center gap-1.5">
                {/* 软归档活会话徽标（体验批二）：未结束、看板隐藏中，详情页可移回 */}
                {s.hiddenAlive && (
                  <span
                    data-testid="alive-badge"
                    className="shrink-0 rounded-full bg-green-100 px-1.5 py-0.5 text-[10px] text-green-700 dark:bg-green-900/40 dark:text-green-300"
                  >
                    {i18n.t("mobile.archive.aliveBadge")}
                  </span>
                )}
                <span className="truncate text-sm font-medium">{s.projectName}</span>
              </span>
              <span className="shrink-0 text-xs text-[var(--mut)]">
                {formatRelativeTime(s.lastSeenAt, now)}
                {i18n.t(s.hiddenAlive ? "mobile.archive.alive" : "mobile.archive.ended")}
              </span>
            </div>
            <div className="mt-0.5 text-xs text-[var(--mut)]">
              {chipLabel(s.agentType)} · {s.title ?? i18n.t("mobile.board.untitled")}
            </div>
          </button>
        ))}
      </div>
    </div>
  );
}
