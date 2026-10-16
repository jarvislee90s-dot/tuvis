// 评语选取与填充（计划② Task 12 步骤 3）——纯函数，零 DOM、零 IPC、零 React。
// 消费方 = `useExportShare.ts`（`pickQuote(agg, getLang(), settings.exportQuote, vars)`）。
//
// 两条口径：
//  * **custom 非空完全覆盖预制池**：用户在设置里写的模板就是最终文案（不再叠加任何池条目）；
//    空白（`""` / `"   "`）等同没填 → 回落预制池。**非法模板不报错**：不认识的 `{…}` 原样保留，
//    用户能一眼看出写错了哪一段，这比静默吞掉或整条不出要好。
//  * `fillQuote` **只认 4 个占位符**（`{range}` / `{tokens}` / `{hitPct}` / `{models}`）：用
//    `split/join` 而不是 `replace` 正则——替换串里的 `$&` / `$1` 是另一套转义，日期与文本里带 `$`
//    时会炸出非预期结果（split/join 对替换内容零解释）。
import { QUOTE_POOL, type QuoteAgg } from "@/lib/usage/quotePool";
import type { Lang } from "@/lib/usage/lang";

/** 4 个占位符的取值（键名与占位符同名同义，不留第二套命名） */
export interface QuoteVars {
  range: string;
  tokens: string;
  hitPct: string;
  models: string;
}

/** vars 缺省时的空档：占位符按空串填（**不留** `{…}` 残骸进图片） */
const EMPTY_VARS: QuoteVars = { range: "", tokens: "", hitPct: "", models: "" };

/**
 * 占位符填充：4 个逐字替换，其余花括号**原样保留**（只认这 4 个，见文件头）。
 * 纯字符串操作，不做 HTML/正则解释。
 */
export function fillQuote(tpl: string, vars: QuoteVars): string {
  return tpl
    .split("{range}")
    .join(vars.range)
    .split("{tokens}")
    .join(vars.tokens)
    .split("{hitPct}")
    .join(vars.hitPct)
    .split("{models}")
    .join(vars.models);
}

/**
 * 取一条评语：custom（设置里的自定义模板）非空 → 填好后**直接返回**（完全覆盖预制池）；
 * 否则按序**首命中**预制池，一条都不命中时返回兜底条（池的末条恒真，兜底不可能落空）。
 * `vars` 缺省 → 4 个占位符按空串填（调用方永远该给 vars；给了就等于给了具体数字）。
 */
export function pickQuote(agg: QuoteAgg, lang: Lang, custom?: string, vars?: QuoteVars): string {
  const v = vars ?? EMPTY_VARS;
  const trimmed = custom?.trim() ?? "";
  if (trimmed) return fillQuote(trimmed, v);
  const pool = QUOTE_POOL[lang];
  for (const entry of pool) {
    if (entry.when(agg)) return fillQuote(entry.text, v);
  }
  return fillQuote(pool[pool.length - 1].text, v);
}
