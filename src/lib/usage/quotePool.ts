// 评语池（计划② Task 12 步骤 3）——**文案资产**，刻意不进 `src/i18n/locales/*.json`：
// 同一批文案两处维护必然漂移（改了一处忘了另一处，分享图与界面就说两套话）。
// 双语**各 10 条**，按序**首命中** + 兜底；谓词只读 `QuoteAgg` 的结构化数字，**读不到任何会话正文**（§3 第 23 条）。
//
// 十条的构成（brief 逐一钉死，别加别减）：
//   ① 命中率 ≥ 0.9   ② 0 < 命中率 < 0.5
//   ③ 总量 ≥ 1000 万 ④ 总量 ≥ 100 万  ⑤ 0 < 总量 < 10 万  ⑥ 总量 = 0
//   ⑦ 趋势上行       ⑧ 趋势收敛
//   ⑨ 多线（multiModel）
//   ⑩ 兜底（恒真）
//
// 占位符**只认 4 个**：`{range}` `{tokens}` `{hitPct}` `{models}`（`fillQuote` 逐字替换；
// `{tool}` 是被禁的第 5 个——工具名不进评语，`agg.topTool` 只作形态信息给将来的文案用）。
import type { Lang } from "@/lib/usage/lang";

/**
 * 评语聚合输入（由 `useExportShare.ts` 从当前看板数据现场推导，**不为此另发查询**）。
 * `hitPct` 是 **0–1 分数**（契约口径，§3 第 7 条）。
 * `trendUp` 是 `boolean | null`：`null` = **算不出方向**（趋势点不足 2 个，或末两点相等）——
 * 这时两条趋势档都**不**命中，落兜底（§3 第 5 条：不可得一律 `null`，绝不把「说不上来」印成
 * 「在收敛」）。池里两条趋势档各自判 `=== true` / `=== false`，正是为了让 `null` 穿过去。
 * `topTool` 是当前第一名的**工具调用名**（可能是 `null`）；池里没有它的占位符，留着是为了让
 * 「多线/劳模」这类形态判据将来能加条目时不必改聚合形状。
 */
export interface QuoteAgg {
  total: number;
  hitPct: number;
  trendUp: boolean | null;
  multiModel: boolean;
  topTool: string | null;
}

/** 一条评语：`when` 为真即选中（按序首命中），`text` 里可带那 4 个占位符 */
export interface QuoteEntry {
  when(agg: QuoteAgg): boolean;
  text: string;
}

/** 双语池：顺序即优先级（**不是**按匹配度打分） */
export const QUOTE_POOL: Record<Lang, readonly QuoteEntry[]> = {
  zh: [
    { when: (a) => a.hitPct >= 0.9, text: "缓存吃得干干净净（命中率 {hitPct}），省钱小能手！" },
    {
      when: (a) => a.hitPct > 0 && a.hitPct < 0.5,
      text: "缓存有点冷（{hitPct}），上下文还在热身。",
    },
    {
      when: (a) => a.total >= 10_000_000,
      text: "{range} 冲进千万俱乐部：{tokens} token 说走就走。",
    },
    { when: (a) => a.total >= 1_000_000, text: "百万俱乐部又添一日：{tokens}。" },
    { when: (a) => a.total > 0 && a.total < 100_000, text: "很节制的一天，只花了 {tokens}。" },
    { when: (a) => a.total === 0, text: "{range} 还没开工，日志本空空的。" },
    { when: (a) => a.trendUp === true, text: "用量在爬坡，我在看着呢。" },
    { when: (a) => a.trendUp === false, text: "用量在收敛，干得漂亮。" },
    { when: (a) => a.multiModel, text: "多线作战（{models} 路并行），指挥若定。" },
    { when: () => true, text: "又是好好干活的一天。" },
  ],
  en: [
    { when: (a) => a.hitPct >= 0.9, text: "Cache hit like a pro — {hitPct}." },
    { when: (a) => a.hitPct > 0 && a.hitPct < 0.5, text: "Cache still warming up ({hitPct})." },
    {
      when: (a) => a.total >= 10_000_000,
      text: "{range} joined the ten-million club: {tokens} tokens.",
    },
    { when: (a) => a.total >= 1_000_000, text: "Another day in the million club: {tokens}." },
    { when: (a) => a.total > 0 && a.total < 100_000, text: "Lean day — only {tokens}." },
    { when: (a) => a.total === 0, text: "Nothing on the clock yet for {range}." },
    { when: (a) => a.trendUp === true, text: "Usage is climbing, I'm watching." },
    { when: (a) => a.trendUp === false, text: "Usage is tapering. Nice." },
    { when: (a) => a.multiModel, text: "Multi-line ops ({models} in parallel), fully in control." },
    { when: () => true, text: "Another good day's work." },
  ],
};
