import { useEffect, useState } from "react";
import { fmtTokens } from "@/lib/usage/format";
import { getLang, type Lang } from "@/lib/usage/lang";
import type { SubagentView } from "./api";

/** 运行中子 agent chip 区（v1 自拉 → 观察台 T5 改纯展示：list 由 SessionDetail
 * 单一数据源下发——chip/面板卡区/详情三处一致）。
 * - 只渲染 status==="running"（「完成即消失」= 前端过滤；全量名单在 FilePanel 卡区）；
 * - 时长自走：tick 收在本组件（1s interval）；点击 chip 直达该子 agent 的
 *   实时预览对话框（观察台 §二.4，不经过清单）。 */
export default function SubagentChips({
  list,
  onOpenDetail,
}: {
  list: SubagentView[] | null;
  onOpenDetail: (id: string) => void;
}) {
  // 时长自走 tick（仅本组件重渲染）。now 收在 state 里由 effect/interval 更新——
  // render 期不得调 Date.now()（仓内 eslint react-hooks/purity 红线）；挂载即对齐，
  // 首帧前 now=null 不显时长（一帧，无感）
  const [now, setNow] = useState<number | null>(null);

  useEffect(() => {
    const update = () => setNow(Date.now());
    update();
    const t = setInterval(update, 1000);
    return () => clearInterval(t);
  }, []);

  const running = (list ?? []).filter((s) => s.status === "running");
  if (running.length === 0) return null;
  return (
    <div data-testid="subagent-chips" className="flex flex-wrap items-center gap-x-2 gap-y-1">
      {running.map((s) => {
        const ms = now !== null ? subagentElapsedMs(s, now) : null;
        return (
          <button
            type="button"
            key={s.id}
            data-testid={`subagent-chip-${s.id}`}
            title={s.description ?? undefined}
            onClick={() => onOpenDetail(s.id)}
            className="rounded-full bg-[var(--cb)]/60 px-2 py-0.5 text-[11px] text-[var(--tx)]"
          >
            ◉ {s.name}
            {ms !== null ? ` ${formatElapsed(ms)}` : ""} · {chipTokenText(s.tokens)}
          </button>
        );
      })}
    </div>
  );
}

/** 子 agent 运行时长毫秒（chip 与清单卡共享口径，观察台 §二.3）：
 *  running（endTs=null）→ now − spawnTs 持续走字；idle → endTs − spawnTs 冻结
 *  （续跑转回 running 后自然回到 now 锚——时长从首次派发连续累计不归零）。
 *  spawnTs 缺失/不可解析 → null（只显 token，v1 §8.2 口径） */
export function subagentElapsedMs(
  s: Pick<SubagentView, "spawnTs" | "endTs">,
  now: number
): number | null {
  if (s.spawnTs === null || Number.isNaN(Date.parse(s.spawnTs))) return null;
  const end = s.endTs !== null && !Number.isNaN(Date.parse(s.endTs)) ? Date.parse(s.endTs) : now;
  return Math.max(0, end - Date.parse(s.spawnTs));
}

/** 运行时长格式（纯函数）：M:SS；≥1h → H:MM:SS */
export function formatElapsed(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = s % 60;
  const two = (n: number) => String(n).padStart(2, "0");
  return h > 0 ? `${h}:${two(m)}:${two(sec)}` : `${m}:${two(sec)}`;
}

/** chip 的 token 文案：四桶求和走 fmtTokens 的当前语言口径（locale 缺省读 i18n 单例——
 *  lang.ts 内持单例，与 ModeBar.tsx:3 直引 i18n 同法；显式参数留给测试钉语言） */
export function chipTokenText(t: SubagentView["tokens"], locale: Lang = getLang()): string {
  return fmtTokens(t.input + t.cacheRead + t.cacheCreation + t.output, locale);
}
