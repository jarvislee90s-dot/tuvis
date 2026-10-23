// 「子 Agent」sheet 的清单看板（观察台 §二；T1 sheet 化起从 FilePanel 卡区迁出，
// 升格为与文件面板平级的独立看板）：本会话派生过的子 agent 单一列表，
// 服务端序即派发序（spawnTs 升序、新派发附末尾）——本组件不再排序。
// 绿点=活跃（时长/token 走字）、灰点=冻结原位不重排不折叠（§二.3）；
// 点击任一卡片 → 详情（活跃进实时预览 / 不活跃进定格快照——由 SubagentDetail
// 按 status 分诊，本组件只上报 id）。空（null/[]）整体不渲染（§二.7）。
// 分屏主从布局（审查 S5 后的左清单右详情）下用 selectedId 高亮选中卡。
import { useEffect, useState } from "react";
import i18n from "@/i18n";
import type { SubagentView } from "./api";
import { chipTokenText, formatElapsed, subagentElapsedMs } from "./SubagentChips";

export default function SubagentList({
  list,
  onOpen,
  selectedId = null,
}: {
  list: SubagentView[] | null;
  onOpen: (id: string) => void;
  /** 分屏主从布局的选中卡（详情在右栏时回显高亮；null/缺省 = 全不选） */
  selectedId?: string | null;
}) {
  // 相对时钟（1s：活跃卡走字；纯渲染期不调 Date.now——react-hooks/purity）
  const [now, setNow] = useState<number | null>(null);
  useEffect(() => {
    const update = () => setNow(Date.now());
    update();
    const t = setInterval(update, 1000);
    return () => clearInterval(t);
  }, []);

  if (!list || list.length === 0) return null;
  return (
    <section data-testid="subagent-list" aria-label={i18n.t("mobile.subagent.listAria")}>
      <p className="mb-1 text-xs font-medium tracking-wide text-[var(--mut)] uppercase">
        {i18n.t("mobile.subagent.listCount", { count: list.length })}
      </p>
      <ul className="space-y-1">
        {list.map((s) => {
          const running = s.status === "running";
          const selected = s.id === selectedId;
          const ms = now !== null ? subagentElapsedMs(s, now) : null;
          return (
            <li key={s.id}>
              <button
                type="button"
                data-testid={`subagent-card-${s.id}`}
                aria-pressed={selected}
                onClick={() => onOpen(s.id)}
                className={`flex w-full items-center gap-2 rounded-lg px-2 py-1.5 text-left hover:bg-[var(--cbg)] dark:hover:bg-[var(--btnp)] ${
                  selected
                    ? "bg-violet-500/10 ring-1 ring-violet-500 dark:bg-violet-400/20 dark:ring-violet-400"
                    : ""
                }`}
              >
                <span
                  data-testid={`subagent-dot-${s.id}`}
                  className={`h-2 w-2 shrink-0 rounded-full ${
                    running ? "bg-emerald-500" : "bg-gray-400"
                  }`}
                />
                <span className="min-w-0 flex-1 truncate text-sm text-[var(--tx)]">
                  {s.name}
                  {s.description ? (
                    <span className="ml-1 text-xs text-[var(--mut)]" title={s.description}>
                      {s.description}
                    </span>
                  ) : null}
                </span>
                <span className="shrink-0 text-xs text-[var(--mut)]">
                  {ms !== null ? `${formatElapsed(ms)} · ` : ""}
                  {chipTokenText(s.tokens)}
                </span>
              </button>
            </li>
          );
        })}
      </ul>
    </section>
  );
}
